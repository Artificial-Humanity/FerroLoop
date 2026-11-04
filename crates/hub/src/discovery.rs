//! Host session & transcript discovery, git worktree & diff inspection, and Cargo
//! workspace architecture conformance scanning (spec §1.3, §3.2, §3.3).
//!
//! # How stale discovery can be
//!
//! The hub caches the costly discovery reads. These are all the windows in which an answer can
//! lag the host or the store. A `Some(home)` override (tests, hermetic runs) bypasses the route
//! cache and keeps its own saved-route clock, so none of them crosses between two homes.
//!
//! - **Route cache, 2.5 s** (`ROUTE_DISCOVERY_TTL`). `discovered_route_table` serves one host
//!   scan (route files, vendor session files, `/proc`) for 2.5 s after the scan ends. The hub
//!   drops it when it spawns, restarts, resets or tears down a session and whenever it saves the
//!   route table file (`invalidate_route_discovery_cache`), so its own changes show at once.
//! - **External writers, up to 2.5 s.** A change the hub did not make (the `fl wire daemon` or
//!   the `fl wire` CLI saving the route table, a vendor CLI starting or replacing a session) is
//!   not an invalidation: it shows when the cached scan expires.
//! - **Store snapshot, 2 s** (`DISCOVERY_SNAPSHOT_TTL`). The read-only endpoints share
//!   one `DiscoveryStoreSnapshot` (agent names and attempt rows) through
//!   `HubState::cached_discovery_snapshot`. Any request other than a `GET` or `HEAD` drops it
//!   (`api::handle_http_request`); a store write from outside the hub (the CLI) shows within 2 s.
//!   Requests that act on a session (prompt, queue drain) take a fresh snapshot.
//! - **Saved routes, 60 s** (fl-wire's `ROUTE_REDISCOVERY_INTERVAL`). Dispatches read routes
//!   from the saved route table file, not from a scan (`lifecycle::lookup_route`, `/wire/send`).
//!   A saved route is used only when the hub saved a host scan to that file within the
//!   interval (`saved_routes_fresh`) and, when the request resolved a vendor session, the
//!   route names that session. A lookup with no session id (lifecycle hydration, a wire push
//!   by name) can therefore reach a replaced Antigravity, Codex, Muse or Grok conversation that
//!   still accepts input for up to 60 s. The daemon can also overwrite the file with its own
//!   older view (see `with_route_file_lock`).
//! - **`/agents` mixes ages.** It reads agents and areas from the store live, but its `routes`
//!   come from the cached store snapshot (2 s) and the cached route scan (2.5 s), so an agent
//!   registered or retired within those windows can be listed with a route list that does not
//!   reflect it yet.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fl_core::arch::{
    ArchConformanceReport, ArchitectureModel, PackageDep, SourceReference,
    check_architecture_conformance,
};
use fl_core::store::{AgentRegistry, Catalog, Ledger};
use fl_exec::stamp;
use fl_store::AnyStore;
use fl_wire::identity::{RouteTable, VendorRoute};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

mod execution_mode;

/// Summary of an active or historical agent session on the host or in the store (spec §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredSessionSummary {
    pub id: String,
    pub source: String,
    pub adapter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    /// Account quota or rate-limit window for this vendor session, when the runtime
    /// persists one. Omitted for attempt rows and for vendors with no on-disk allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_usage: Option<SessionQuotaUsage>,
    /// Model the vendor session runs, without the thinking-level suffix. Omitted for
    /// attempt rows, for other adapters, and when no source names a model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort (`low`, `medium`, `high`, ...) when the vendor reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    /// Model ids a user can switch this session to. Empty for attempt rows and adapters
    /// with no model choice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    /// Server-measured context window usage of the live vendor session. Set only by the
    /// model-info pass (`GET /api/v1/sessions`); omitted for attempt rows, lookup-only
    /// discovery, and sessions whose transcript reports no usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<SessionContextUsage>,
    /// Effective execution (permission) mode the vendor session runs in, read-only from the
    /// vendor's own transcript or config: `auto`, `yolo`, `plan`, `default`, `accept-edits`,
    /// or a lower-cased vendor value this crate has no name for. Omitted when no source
    /// states one. Set only by the model-info pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_mode: Option<String>,
}

/// Normalized tool invocation inside a [`TranscriptTurn`] (spec §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Normalized conversation turn parsed from an Antigravity, Claude, Codex, Muse, Grok,
/// or store `Attempt` transcript (spec §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptTurn {
    pub index: usize,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<TranscriptToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Server-measured context window token usage for a live vendor session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionContextUsage {
    pub used_tokens: u64,
    pub max_tokens: u64,
}

/// One vendor allowance window (5-hour primary, weekly secondary, or a single reset).
///
/// `used_percent` is the vendor's number, copied verbatim. Equality compares the IEEE bits
/// so [`DiscoveredSessionSummary`] can keep `Eq`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWindowUsage {
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
}

impl PartialEq for QuotaWindowUsage {
    fn eq(&self, other: &Self) -> bool {
        self.used_percent.to_bits() == other.used_percent.to_bits()
            && self.window_minutes == other.window_minutes
            && self.resets_at_ms == other.resets_at_ms
    }
}

impl Eq for QuotaWindowUsage {}

/// Account quota or rate-limit snapshot attached to a discovered vendor session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionQuotaUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_window: Option<QuotaWindowUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_window: Option<QuotaWindowUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhausted_reason: Option<String>,
}

/// B-tree rows discovery needs, captured while a [`AnyStore`] guard is held.
/// Filesystem walks use this snapshot after the guard is dropped.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryStoreSnapshot {
    registered_names: BTreeSet<String>,
    attempts: Vec<DiscoveryAttemptRow>,
}

#[derive(Debug, Clone)]
struct DiscoveryAttemptRow {
    id: String,
    adapter: String,
    status: String,
    updated_at: Option<String>,
    cwd: String,
    record_id: String,
    record_str: String,
    output_excerpt: Option<String>,
    created_at: Option<String>,
}

/// Per-thread counts of the costly steps a request takes, so tests can prove a step was skipped.
/// A handler runs on the calling thread, so a test reads the counts of its own requests only.
#[cfg(test)]
pub(crate) mod cost_counters {
    use std::cell::Cell;
    use std::thread::LocalKey;

    thread_local! {
        /// Host route scans ([`super::discover_routes_into`]): one `RouteTable::discover_routes` each.
        pub(crate) static ROUTE_SCANS: Cell<u64> = const { Cell::new(0) };
        /// Transcript files read in full ([`super::read_transcript`]).
        pub(crate) static TRANSCRIPT_READS: Cell<u64> = const { Cell::new(0) };
        /// Transcripts parsed into turns ([`super::parse_jsonl_transcript`]).
        pub(crate) static TRANSCRIPT_PARSES: Cell<u64> = const { Cell::new(0) };
        pub(crate) static TRANSCRIPT_FINDS: Cell<u64> = const { Cell::new(0) };
        /// Store snapshots ([`super::snapshot_discovery_store`]): `2 + projects` store reads each.
        pub(crate) static STORE_SNAPSHOTS: Cell<u64> = const { Cell::new(0) };
        /// Workspace store opens (`HubState::open_store_at`).
        pub(crate) static STORE_OPENS: Cell<u64> = const { Cell::new(0) };
        /// Calls to [`super::invalidate_route_discovery_cache`].
        pub(crate) static ROUTE_CACHE_INVALIDATIONS: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn bump(counter: &'static LocalKey<Cell<u64>>) {
        counter.with(|n| n.set(n.get() + 1));
    }

    /// Counts taken on this thread while one closure ran.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(crate) struct Costs {
        pub route_scans: u64,
        pub transcript_reads: u64,
        pub transcript_parses: u64,
        pub transcript_finds: u64,
        pub store_snapshots: u64,
        pub store_opens: u64,
    }

    fn now() -> Costs {
        Costs {
            route_scans: ROUTE_SCANS.with(Cell::get),
            transcript_reads: TRANSCRIPT_READS.with(Cell::get),
            transcript_parses: TRANSCRIPT_PARSES.with(Cell::get),
            transcript_finds: TRANSCRIPT_FINDS.with(Cell::get),
            store_snapshots: STORE_SNAPSHOTS.with(Cell::get),
            store_opens: STORE_OPENS.with(Cell::get),
        }
    }

    /// Run `body` and return what it cost on this thread.
    pub(crate) fn measure<T>(body: impl FnOnce() -> T) -> (T, Costs) {
        let before = now();
        let out = body();
        let after = now();
        let costs = Costs {
            route_scans: after.route_scans - before.route_scans,
            transcript_reads: after.transcript_reads - before.transcript_reads,
            transcript_parses: after.transcript_parses - before.transcript_parses,
            transcript_finds: after.transcript_finds - before.transcript_finds,
            store_snapshots: after.store_snapshots - before.store_snapshots,
            store_opens: after.store_opens - before.store_opens,
        };
        (out, costs)
    }
}

/// The route table file the hub reads: the override home's `routes.json`, else `None` (the
/// default path merged with the legacy agent-bus file, see [`RouteTable::load`]).
pub(crate) fn route_table_path(home_override: Option<&Path>) -> Option<PathBuf> {
    home_override.map(|h| h.join(".cache").join("agent-bus").join("routes.json"))
}

/// Load the route table and scan the host's vendor session files for live routes. This is the
/// costly step (every vendor directory, transcript heads and `/proc`), so callers that only read
/// go through [`discovered_route_table`].
pub(crate) fn scan_route_table(home_override: Option<&Path>) -> RouteTable {
    let mut routes = RouteTable::load(route_table_path(home_override).as_deref());
    discover_routes_into(&mut routes, home_override);
    routes
}

/// A thread-safe cache of one value that expires `ttl` after it was loaded, or at the next
/// [`TtlCache::invalidate`]. Every read returns a clone, so a caller that changes its copy never
/// changes the cache.
///
/// One load at a time: callers that miss together wait for the first one's load and then read
/// its value. A load that an invalidation overtook is not served, because each value carries the
/// generation it was loaded in and an invalidation starts a new one.
pub(crate) struct TtlCache<K, V> {
    ttl: Duration,
    state: Mutex<TtlCacheState<K, V>>,
    load_gate: Mutex<()>,
}

struct TtlCacheState<K, V> {
    generation: u64,
    entry: Option<TtlCacheEntry<K, V>>,
}

struct TtlCacheEntry<K, V> {
    key: K,
    loaded_at: Instant,
    generation: u64,
    value: V,
}

impl<K, V> std::fmt::Debug for TtlCache<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TtlCache")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl<K: PartialEq, V: Clone> TtlCache<K, V> {
    pub(crate) const fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            state: Mutex::new(TtlCacheState {
                generation: 0,
                entry: None,
            }),
            load_gate: Mutex::new(()),
        }
    }

    /// The cached value for `key` when it is younger than the TTL; otherwise `load` runs and its
    /// `Ok` value is cached under `key`. An `Err` is returned and not cached. `clock` is the time
    /// source (`Instant::now` outside tests); it is read for each freshness check and again when
    /// `load` returns, so a value's age counts from the end of its load: a scan that took 1.4 s
    /// still serves for the whole TTL.
    pub(crate) fn get_or_load<E>(
        &self,
        key: K,
        clock: impl Fn() -> Instant,
        load: impl FnOnce() -> Result<V, E>,
    ) -> Result<V, E> {
        if let Some(hit) = self.fresh(&key, clock()) {
            return Ok(hit);
        }
        let _one_load = self.load_gate.lock().unwrap_or_else(|e| e.into_inner());
        // A caller that waited on the gate reads what the load before it stored.
        if let Some(hit) = self.fresh(&key, clock()) {
            return Ok(hit);
        }
        // The generation the load starts in. An invalidation during the load starts a new one,
        // so `fresh` never serves this value.
        let generation = self.lock_state().generation;
        let value = load()?;
        let loaded_at = clock();
        let mut state = self.lock_state();
        state.entry = Some(TtlCacheEntry {
            key,
            loaded_at,
            generation,
            value: value.clone(),
        });
        Ok(value)
    }

    /// Drop the cached value. A load already running when this is called is not served either.
    pub(crate) fn invalidate(&self) {
        let mut state = self.lock_state();
        state.generation = state.generation.wrapping_add(1);
        state.entry = None;
    }

    fn fresh(&self, key: &K, now: Instant) -> Option<V> {
        let state = self.lock_state();
        let entry = state.entry.as_ref()?;
        (entry.generation == state.generation
            && entry.key == *key
            && now.saturating_duration_since(entry.loaded_at) < self.ttl)
            .then(|| entry.value.clone())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, TtlCacheState<K, V>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// How long the hub reuses one host route scan. FerroDeck polls several discovery endpoints at
/// once and each needs the route table, so they share one scan instead of each taking ~1.4 s.
const ROUTE_DISCOVERY_TTL: Duration = Duration::from_millis(2500);

/// What a host route scan read: the route files it loaded and whether host discovery was on.
/// A scan cached under other values (a test that moved `HOME` or `XDG_RUNTIME_DIR`) is not reused.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteScanKey {
    primary: PathBuf,
    legacy: Option<PathBuf>,
    host_discovery_disabled: bool,
}

impl RouteScanKey {
    fn current() -> Self {
        Self {
            primary: fl_wire::identity::default_routes_path(),
            legacy: fl_wire::identity::legacy_routes_path(),
            host_discovery_disabled: std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref()
                == Ok("1"),
        }
    }
}

static ROUTE_DISCOVERY_CACHE: TtlCache<RouteScanKey, RouteTable> =
    TtlCache::new(ROUTE_DISCOVERY_TTL);

/// The host's route table after discovery. With no home override the scan is shared for
/// [`ROUTE_DISCOVERY_TTL`]; a `Some(home)` call (tests, hermetic runs) always scans fresh and
/// never reads or fills the shared cache.
pub(crate) fn discovered_route_table(home_override: Option<&Path>) -> RouteTable {
    if home_override.is_some() {
        return scan_route_table(home_override);
    }
    let Ok(table) =
        ROUTE_DISCOVERY_CACHE.get_or_load(RouteScanKey::current(), Instant::now, || {
            Ok::<_, std::convert::Infallible>(scan_route_table(None))
        });
    table
}

/// Forget the shared route scan, so the next discovery reads the host again. Called when the hub
/// spawns, restarts, clears or stops a session, or writes the route table, so a change the hub
/// made itself shows at once.
pub(crate) fn invalidate_route_discovery_cache() {
    #[cfg(test)]
    cost_counters::bump(&cost_counters::ROUTE_CACHE_INVALIDATIONS);
    ROUTE_DISCOVERY_CACHE.invalidate();
}

/// Scan the host like [`scan_route_table`] and save what the scan learned to the route table
/// file, so the next lookup finds it without a scan. Returns the scanned table.
///
/// The file is shared with the live `fl wire daemon`, which saves its whole table by writing a
/// private temp file and renaming it over the target ([`RouteTable::save`]); the CLI fallback
/// does load, change, save. The hub writes the same way and merges as the daemon merges a
/// dispatch's scan into its live table ([`RouteTable::merge_dispatch_result`]): it reads the file
/// again just before saving, so a route the daemon registered during the scan is kept, a route
/// is dropped only when the scan pruned it and nobody changed it meanwhile, and a route removed
/// during the scan stays removed ([`merge_route_scan`]). Like the daemon, it writes only when
/// the scan changed a route. Hub writers are serialized ([`with_route_file_lock`]).
pub(crate) fn scan_and_save_route_table(home_override: Option<&Path>) -> RouteTable {
    let started = Instant::now();
    let saved = RouteTable::load(route_table_path(home_override).as_deref());
    let mut scanned = saved.clone();
    discover_routes_into(&mut scanned, home_override);
    save_route_scan(home_override, &saved, &scanned, started);
    scanned
}

/// Scan the host's vendor session files into `routes` (one [`RouteTable::discover_routes`]).
pub(crate) fn discover_routes_into(routes: &mut RouteTable, home_override: Option<&Path>) {
    #[cfg(test)]
    cost_counters::bump(&cost_counters::ROUTE_SCANS);
    routes.discover_routes(home_override, stamp::now());
}

/// Save the routes `scanned` learned over `base` (the table the scan started from) to the route
/// table file. See [`scan_and_save_route_table`] for how this shares the file with the daemon.
/// `started` is when the scan began: once the file holds the scan, saved routes count as fresh
/// from then ([`saved_routes_fresh`]).
pub(crate) fn save_route_scan(
    home_override: Option<&Path>,
    base: &RouteTable,
    scanned: &RouteTable,
    started: Instant,
) {
    if scanned.same_routes(base) {
        // The file already held what the scan found.
        note_route_file_scan(home_override, started);
        return;
    }
    let path = route_table_path(home_override);
    let saved = with_route_file_lock(|| {
        let mut live = RouteTable::load(path.as_deref());
        merge_route_scan(&mut live, base, scanned);
        live.save(path.as_deref()).is_ok()
    });
    if saved {
        note_route_file_scan(home_override, started);
    }
    // The hub wrote the route table: the shared scan must not hide it.
    invalidate_route_discovery_cache();
}

/// When the hub last scanned the host and the route table file then held the scan, per file
/// ([`route_table_path`]; `None` is the default file). Kept per file so a `Some(home)` test never
/// sees another test's scan. Only scans saved to the file count: the shared discovery scan
/// ([`discovered_route_table`]) is not saved, so it does not make saved routes fresh.
static ROUTE_FILE_SCANS: Mutex<BTreeMap<Option<PathBuf>, Instant>> = Mutex::new(BTreeMap::new());

/// Record that the route table file for `home_override` holds a host scan that began at `started`.
pub(crate) fn note_route_file_scan(home_override: Option<&Path>, started: Instant) {
    let mut scans = ROUTE_FILE_SCANS.lock().unwrap_or_else(|e| e.into_inner());
    let last = scans
        .entry(route_table_path(home_override))
        .or_insert(started);
    *last = (*last).max(started);
}

/// `true` when the route table file for `home_override` holds a host scan that began no more
/// than fl-wire's [`ROUTE_REDISCOVERY_INTERVAL`] before `now`. A route read from a file that is
/// not fresh may name a session the persona has since replaced, so the caller scans again.
/// `false` until this process has saved a scan.
///
/// [`ROUTE_REDISCOVERY_INTERVAL`]: fl_wire::dispatcher::ROUTE_REDISCOVERY_INTERVAL
pub(crate) fn saved_routes_fresh(home_override: Option<&Path>, now: Instant) -> bool {
    let scans = ROUTE_FILE_SCANS.lock().unwrap_or_else(|e| e.into_inner());
    scans
        .get(&route_table_path(home_override))
        .is_some_and(|last| {
            now.saturating_duration_since(*last) <= fl_wire::dispatcher::ROUTE_REDISCOVERY_INTERVAL
        })
}

/// Fold what a scan learned into `live`, the route table file as it is now. `scanned` started
/// as a copy of `base` and went through discovery:
///
/// - a route the scan pruned (in `base`, gone from `scanned`) is dropped from `live`, unless
///   another writer changed it meanwhile;
/// - a route the scan found or changed merges as [`RouteTable::merge_newer`] does;
/// - a route the scan left as `base` had it is only restamped in `live`, never written back:
///   when `live` no longer holds it, another writer removed it during the scan (a teardown's
///   `remove_routes`, the daemon), and it stays removed.
pub(crate) fn merge_route_scan(live: &mut RouteTable, base: &RouteTable, scanned: &RouteTable) {
    let mut learned = scanned.clone();
    learned.entries.retain(|key, entry| {
        let unchanged = base
            .entries
            .get(key)
            .is_some_and(|before| before.route == entry.route);
        !unchanged || live.entries.contains_key(key)
    });
    live.merge_dispatch_result(base, learned);
}

/// Serializes every hub writer of the route table file: [`save_route_scan`] and the teardown's
/// route removal (`lifecycle::remove_routes`). Each does load, change, save; without the lock a
/// removal saved between another writer's load and its save is overwritten and the route comes
/// back.
///
/// The lock is held in this process only. The `fl wire daemon` and the `fl wire` CLI fallback
/// write the same file from other processes, and nothing serializes them with the hub. Every
/// write is a whole-file rename ([`RouteTable::save`]), so no reader sees a torn file, but a
/// change one process saves between another's load and its rename is lost. The daemon is the
/// wider window: it keeps its table in memory, never re-reads the file, and saves that whole
/// table only on a `RegisterRoute` request or a dispatch whose scan changed a route. Such a save
/// drops any route the hub saved that the daemon does not hold, and brings back a route a
/// teardown removed while the daemon still holds it, until the next scan learns or prunes it again.
static ROUTE_FILE_WRITE: Mutex<()> = Mutex::new(());

/// Run `write` (a load, change and save of the route table file) holding [`ROUTE_FILE_WRITE`].
pub(crate) fn with_route_file_lock<T>(write: impl FnOnce() -> T) -> T {
    let _one_writer = ROUTE_FILE_WRITE.lock().unwrap_or_else(|e| e.into_inner());
    write()
}

/// Read agent names and attempt rows. Does not walk the filesystem.
pub fn snapshot_discovery_store(store: &AnyStore) -> DiscoveryStoreSnapshot {
    #[cfg(test)]
    cost_counters::bump(&cost_counters::STORE_SNAPSHOTS);
    let registered_names = store
        .list_agents(None, true)
        .unwrap_or_default()
        .into_iter()
        .map(|agent| agent.name_key.to_ascii_lowercase())
        .collect();
    let mut attempts = Vec::new();
    if let Ok(projects) = store.list_projects() {
        for project in projects {
            if let Ok(rows) = store.attempts(&project.id) {
                for (idx, attempt) in rows.into_iter().enumerate() {
                    let id = attempt
                        .id
                        .as_ref()
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| format!("attempt-{}-{idx}", project.id));
                    let created_at = attempt.at.map(|at| at.to_string());
                    attempts.push(DiscoveryAttemptRow {
                        id,
                        adapter: attempt.adapter,
                        status: attempt.status.as_wire().to_string(),
                        updated_at: created_at.clone(),
                        cwd: project.root.clone(),
                        record_id: attempt.record.to_string(),
                        record_str: attempt.record.as_str().to_string(),
                        output_excerpt: attempt.output_excerpt,
                        created_at,
                    });
                }
            }
        }
    }
    DiscoveryStoreSnapshot {
        registered_names,
        attempts,
    }
}

/// The route ordering needs agent names, but not attempt history.
pub(crate) fn snapshot_discovery_names(store: &AnyStore) -> DiscoveryStoreSnapshot {
    DiscoveryStoreSnapshot {
        registered_names: store
            .list_agents(None, true)
            .unwrap_or_default()
            .into_iter()
            .map(|agent| agent.name_key.to_ascii_lowercase())
            .collect(),
        attempts: Vec::new(),
    }
}

/// Summary of a Git worktree discovered via `git worktree list --porcelain` (spec §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSummary {
    pub id: String,
    pub path: String,
    pub head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub dirty_files: Vec<String>,
    pub is_clean: bool,
}

/// File status line inside a worktree (`git status --porcelain`, spec §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeFileStatus {
    pub status: String,
    pub path: String,
}

/// Detailed status and `git diff HEAD` output for a specific worktree (spec §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeDiff {
    pub id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub head: String,
    pub files: Vec<WorktreeFileStatus>,
    pub diff: String,
}

/// Extracted Cargo package dependencies, source references, and repository files for
/// [`check_architecture_conformance`] (spec §1.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceArchScan {
    pub package_deps: Vec<PackageDep>,
    pub source_refs: Vec<SourceReference>,
    pub repo_files: Vec<String>,
}

/// Discover active sessions from `RouteTable` and recorded `Attempt` runs in `store` (spec §3.2).
pub fn discover_sessions(store: &AnyStore) -> Vec<DiscoveredSessionSummary> {
    discover_sessions_with_home(store, None)
}

/// Discover active sessions with an optional `HOME` directory override for hermetic tests.
pub fn discover_sessions_with_home(
    store: &AnyStore,
    home_override: Option<&Path>,
) -> Vec<DiscoveredSessionSummary> {
    let snap = snapshot_discovery_store(store);
    discover_sessions_from_snapshot(&snap, home_override)
}

/// Discover sessions from a store snapshot. Filesystem route discovery runs here, with no
/// live [`AnyStore`] borrow.
pub fn discover_sessions_from_snapshot(
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
) -> Vec<DiscoveredSessionSummary> {
    discover_sessions_impl(snap, home_override, true)
}

/// Discover sessions like [`discover_sessions_from_snapshot`], but leave `quota_usage`,
/// `model`, `thinking_level` and `available_models` empty. For callers that only look a
/// session up by `id`, `persona` or `source`: it never reads a model file, a transcript or a
/// transcript tail, and never runs a quota probe or calls a language server.
pub fn discover_route_sessions_without_model_info(
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
) -> Vec<DiscoveredSessionSummary> {
    discover_sessions_impl(snap, home_override, false)
}

/// Shared body of [`discover_sessions_from_snapshot`]. `with_model_info` is false for
/// callers that only look a session up by id, so they never read model files or transcripts
/// for quota, run a quota probe, or wait on a language server.
fn discover_sessions_impl(
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
    with_model_info: bool,
) -> Vec<DiscoveredSessionSummary> {
    discover_sessions_from_routes(
        snap,
        home_override,
        with_model_info,
        discovered_route_table(home_override),
        None,
    )
}

/// The canonical ID used both for fast resolution and complete discovery.
fn route_session_id(persona: &str, route: &VendorRoute) -> String {
    match route {
        VendorRoute::Claude {
            pid, session_id, ..
        } if session_id.trim().is_empty() => {
            format!("claude-{persona}-{pid}")
        }
        VendorRoute::Claude { session_id, .. } | VendorRoute::Grok { session_id, .. } => {
            session_id.clone()
        }
        VendorRoute::Agy {
            conversation_id, ..
        } => conversation_id.clone(),
        VendorRoute::Codex { thread_id, .. } => thread_id.clone(),
        VendorRoute::Muse { session_id, .. } if session_id.is_empty() => format!("muse-{persona}"),
        VendorRoute::Muse { session_id, .. } => session_id.clone(),
        VendorRoute::Webhook { .. } => format!("webhook-{persona}"),
    }
}

/// Resolve one route in the same order and with the same deduplication as full discovery.
/// No transcript path is searched for an unrelated route.
pub(crate) fn lookup_one_route_session(
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
    requested: &str,
) -> Option<DiscoveredSessionSummary> {
    let routes = discovered_route_table(home_override);
    let mut ordered: Vec<_> = routes.entries.iter().collect();
    ordered.sort_by_key(|(persona, entry)| {
        (
            !snap
                .registered_names
                .contains(&persona.to_ascii_lowercase()),
            std::cmp::Reverse(entry.updated_at.unix_millis()),
            *persona,
        )
    });
    let mut seen = BTreeSet::new();
    let requested = requested.trim();
    let persona = ordered.into_iter().find_map(|(persona, entry)| {
        let id = route_session_id(persona, &entry.route);
        (seen.insert(id.clone()) && (id == requested || persona == requested))
            .then(|| persona.clone())
    })?;
    discover_sessions_from_routes(snap, home_override, false, routes, Some(&persona))
        .into_iter()
        .next()
}

fn discover_sessions_from_routes(
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
    with_model_info: bool,
    routes: RouteTable,
    only_persona: Option<&str>,
) -> Vec<DiscoveredSessionSummary> {
    let mut out = Vec::new();
    let mut agy_endpoints: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    let mut seen_ids = BTreeSet::new();

    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));

    let registered_names = &snap.registered_names;

    let mut ordered_routes: Vec<_> = routes.entries.iter().collect();
    ordered_routes.sort_by_key(|(persona, entry)| {
        let is_registered = registered_names.contains(&persona.to_ascii_lowercase());
        (
            !is_registered,
            std::cmp::Reverse(entry.updated_at.unix_millis()),
            *persona,
        )
    });

    for (persona, entry) in ordered_routes {
        if only_persona.is_some_and(|selected| selected != persona) {
            continue;
        }
        let raw_id = route_session_id(persona, &entry.route);
        let (adapter, cwd, transcript_path) = match &entry.route {
            VendorRoute::Claude { .. } => {
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, &raw_id));
                ("claude", None, tpath)
            }
            VendorRoute::Agy {
                conversation_id,
                ls_address,
                csrf_token,
                ..
            } => {
                // Remember the endpoint whatever the caller reads now: the watch and
                // transcript quota reads never run a model-info pass, yet need it.
                crate::direct_quota::remember_agy_endpoint(
                    home_override,
                    conversation_id,
                    ls_address.as_deref(),
                    csrf_token.as_deref(),
                );
                agy_endpoints.insert(
                    conversation_id.clone(),
                    (ls_address.clone(), csrf_token.clone()),
                );
                let tpath = home
                    .as_deref()
                    .filter(|_| is_safe_path_component(conversation_id))
                    .and_then(|h| {
                        let p = h
                            .join(".gemini")
                            .join("antigravity-cli")
                            .join("brain")
                            .join(conversation_id)
                            .join(".system_generated")
                            .join("logs")
                            .join("transcript.jsonl");
                        p.is_file().then(|| p.display().to_string())
                    });
                ("antigravity", None, tpath)
            }
            VendorRoute::Codex { thread_id, cwd } => {
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, thread_id));
                (
                    "codex",
                    cwd.as_ref().map(|p| p.display().to_string()),
                    tpath,
                )
            }
            VendorRoute::Muse {
                session_id, cwd, ..
            } => {
                let tpath = home
                    .as_deref()
                    .filter(|_| !session_id.is_empty())
                    .and_then(|h| find_transcript_file_in_home(h, &raw_id));
                ("muse", cwd.as_ref().map(|p| p.display().to_string()), tpath)
            }
            VendorRoute::Grok {
                session_id, cwd, ..
            } => {
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, session_id));
                ("grok", cwd.as_ref().map(|p| p.display().to_string()), tpath)
            }
            VendorRoute::Webhook { socket_path } => {
                ("webhook", Some(socket_path.display().to_string()), None)
            }
        };

        if seen_ids.insert(raw_id.clone()) {
            out.push(DiscoveredSessionSummary {
                id: raw_id,
                source: "route".to_string(),
                adapter: adapter.to_string(),
                persona: Some(persona.clone()),
                status: "active".to_string(),
                updated_at: Some(entry.updated_at.to_string()),
                cwd,
                record_id: None,
                transcript_path,
                quota_usage: None,
                model: None,
                thinking_level: None,
                available_models: Vec::new(),
                context_usage: None,
                execution_mode: None,
            });
        }
    }

    for attempt in &snap.attempts {
        if seen_ids.insert(attempt.id.clone()) {
            out.push(DiscoveredSessionSummary {
                id: attempt.id.clone(),
                source: "attempt".to_string(),
                adapter: attempt.adapter.clone(),
                persona: None,
                status: attempt.status.clone(),
                updated_at: attempt.updated_at.clone(),
                cwd: Some(attempt.cwd.clone()),
                record_id: Some(attempt.record_id.clone()),
                transcript_path: None,
                quota_usage: None,
                model: None,
                thinking_level: None,
                available_models: Vec::new(),
                context_usage: None,
                execution_mode: None,
            });
        }
    }

    for session in &mut out {
        if session.source == "route" && with_model_info {
            // `home` is for transcript lookup only. Production passes `None`
            // so probes may use `PATH`; `Some($HOME)` would skip them.
            let endpoint = agy_endpoints.get(&session.id);
            let agy_address = endpoint.and_then(|(addr, _)| addr.as_deref());
            let agy_csrf = endpoint.and_then(|(_, csrf)| csrf.as_deref());
            // One read per transcript file serves the model, quota, context and mode reads.
            let mut files = TranscriptReads::default();
            // Muse's model comes from its transcript text, read once through `files`.
            let info = if session.adapter == "muse" {
                muse_model_info(session.transcript_path.as_deref(), &mut files)
            } else {
                session_model_info(
                    &session.adapter,
                    &session.id,
                    session.transcript_path.as_deref(),
                    home.as_deref(),
                    agy_address,
                    agy_csrf,
                    agy_env_fallback_allowed(home_override),
                )
            };
            // The quota read needs the model: Antigravity keeps separate buckets per model family.
            session.quota_usage = quota_for_resolved_route_with_reads(
                &session.adapter,
                session.transcript_path.as_deref(),
                home_override,
                &mut files,
                &crate::direct_quota::AgyQuotaTarget {
                    address: agy_address,
                    csrf: agy_csrf,
                    model: info.model.as_deref(),
                    session: Some(&session.id),
                },
            );
            session.context_usage = session
                .transcript_path
                .as_deref()
                .and_then(|path| context_usage_for_transcript(path, &mut files));
            session.execution_mode = execution_mode::session_execution_mode(
                &session.adapter,
                session.transcript_path.as_deref(),
                home.as_deref(),
                &mut files,
            );
            session.model = info.model;
            session.thinking_level = info.thinking_level;
            session.available_models = info.available_models;
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Live model discovery (per-adapter model, thinking level, and selectable models)
// ---------------------------------------------------------------------------

const AGY_DEFAULT_MODELS: [&str; 5] = [
    "gemini-4-argon",
    "gemini-3.8-flash",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "gpt-oss-120b",
];
const CLAUDE_MODELS: [&str; 3] = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-4-5"];
const CODEX_DEFAULT_MODELS: [&str; 5] = [
    "gpt-6.1-terra",
    "gpt-6.1-sol",
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
];
const GROK_DEFAULT_MODELS: [&str; 4] = ["grok-4.7", "grok-4.7-build-fast", "grok-4.6", "grok-4.5"];

const AGY_LS_SERVICE: &str = "/exa.language_server_pb.LanguageServerService";
/// Connect and per-read timeout for each language-server call.
const AGY_RPC_TIMEOUT: Duration = Duration::from_secs(1);
/// A trajectory grows with the conversation, so it gets a larger body cap than a reset call.
const AGY_TRAJECTORY_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Steps before the transcript's newest `step_index` that the tail-steps call still asks for.
const AGY_STEPS_TAIL_MARGIN: u64 = 10;
/// Newest bytes of an Antigravity transcript scanned for the last `step_index`.
const AGY_TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;
const AGY_CACHE_TTL: Duration = Duration::from_secs(5);
/// A failed or empty lookup is cached briefly so a dead server costs one attempt, not one per poll.
const AGY_NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(5);
const AGY_CACHE_MAX_ENTRIES: usize = 256;
/// Newest bytes of a Claude transcript scanned for the latest `model`.
const CLAUDE_TRANSCRIPT_TAIL_BYTES: u64 = 512 * 1024;

/// Model fields for one discovered session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SessionModelInfo {
    model: Option<String>,
    thinking_level: Option<String>,
    available_models: Vec<String>,
}

fn owned_strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

/// Whether `ANTIGRAVITY_LS_ADDRESS` and `ANTIGRAVITY_CSRF_TOKEN` may supply a missing route
/// value. Never with a home override (tests, hermetic runs) or with host discovery disabled,
/// so those runs cannot reach the host's language server.
pub(crate) fn agy_env_fallback_allowed(home_override: Option<&Path>) -> bool {
    home_override.is_none() && std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() != Ok("1")
}

/// Model, thinking level and selectable models for a route session.
/// Missing or malformed sources fall back silently; this never fails discovery.
fn session_model_info(
    adapter: &str,
    session_id: &str,
    transcript_path: Option<&str>,
    home: Option<&Path>,
    agy_ls_address: Option<&str>,
    agy_csrf_token: Option<&str>,
    agy_env_fallback: bool,
) -> SessionModelInfo {
    match adapter {
        "antigravity" => agy_live_model_info_for_transcript(
            agy_ls_address,
            agy_csrf_token,
            session_id,
            transcript_path,
            agy_env_fallback,
        ),
        "claude" => claude_model_info(transcript_path, home),
        "codex" => codex_model_info(transcript_path, home),
        "grok" => grok_model_info(transcript_path, home),
        _ => SessionModelInfo::default(),
    }
}

/// Muse's model: the `payload.record.model_id` of the transcript's last `runtime.session.metadata`
/// record, else the default. The text comes through `files`, so it has the transcript read cap.
// Record layout per the Lead's inspection of Muse 1.4.14; unverified against a live file.
fn muse_model_info(transcript_path: Option<&str>, files: &mut TranscriptReads) -> SessionModelInfo {
    let model = transcript_path
        .and_then(|path| files.raw(path))
        .and_then(muse_transcript_model_id)
        .unwrap_or_else(|| crate::lifecycle::MUSE_DEFAULT_MODEL.to_string());
    SessionModelInfo {
        model: Some(model),
        thinking_level: None,
        available_models: owned_strings(&crate::lifecycle::MUSE_CATALOG_MODELS),
    }
}

/// `payload.record.model_id` of the last `runtime.session.metadata` record in `raw`.
fn muse_transcript_model_id(raw: &str) -> Option<String> {
    raw.lines()
        .rev()
        .filter(|line| line.contains("runtime.session.metadata"))
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|val| {
            val.get("payload_type").and_then(Value::as_str) == Some("runtime.session.metadata")
        })
        .find_map(|val| non_empty_str(val.pointer("/payload/record/model_id")))
}

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Largest config or cache file (`settings.json`, `config.toml`, `models_cache.json`,
/// `summary.json`) read on a poll. A bigger file counts as unreadable.
const MODEL_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// A whole file as UTF-8, or `None` when it is unreadable, not UTF-8, or longer than
/// `max_bytes`. Never reads more than `max_bytes + 1` bytes, even from a special file.
fn read_capped_string(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read;
    let file = fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= max_bytes).then_some(text)
}

fn read_json_file(path: &Path) -> Option<Value> {
    serde_json::from_str(&read_capped_string(path, MODEL_FILE_MAX_BYTES)?).ok()
}

fn read_toml_file(path: &Path) -> Option<toml::Table> {
    read_capped_string(path, MODEL_FILE_MAX_BYTES)?
        .parse::<toml::Table>()
        .ok()
}

fn toml_non_empty_str(table: &toml::Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

// --- Antigravity ------------------------------------------------------------

/// One `clientModelConfigs[]` entry: the full model id (with thinking suffix) and the
/// `MODEL_PLACEHOLDER_*` name that trajectory steps use for it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgyConfiguredModel {
    model_id: String,
    placeholder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgyModelConfig {
    models: Vec<AgyConfiguredModel>,
    default_placeholder: Option<String>,
}

/// Parse a `GetCascadeModelConfigData` response. `None` when it is not JSON or lists no model.
fn parse_agy_model_config(body: &str) -> Option<AgyModelConfig> {
    let val: Value = serde_json::from_str(body).ok()?;
    let models: Vec<AgyConfiguredModel> = val
        .get("clientModelConfigs")?
        .as_array()?
        .iter()
        .filter_map(|entry| {
            Some(AgyConfiguredModel {
                model_id: non_empty_str(entry.get("modelId"))?,
                placeholder: non_empty_str(entry.pointer("/modelOrAlias/model")),
            })
        })
        .collect();
    if models.is_empty() {
        return None;
    }
    Some(AgyModelConfig {
        models,
        default_placeholder: non_empty_str(
            val.pointer("/defaultOverrideModelConfig/modelOrAlias/model"),
        ),
    })
}

/// Latest `metadata.generatorModel` in the steps of a `GetCascadeTrajectory` response
/// (`trajectory.steps`) or a `GetCascadeTrajectorySteps` response (`steps`, or
/// `trajectory.steps`). A body cut off by the size cap is not valid JSON; the last
/// `"generatorModel"` string in the text that did arrive is used then.
fn parse_agy_generator_model(body: &str) -> Option<String> {
    if let Ok(val) = serde_json::from_str::<Value>(body) {
        return generator_model_from_json(&val);
    }
    const KEY: &str = "\"generatorModel\"";
    let mut latest = None;
    let mut rest = body;
    while let Some(at) = rest.find(KEY) {
        rest = &rest[at + KEY.len()..];
        let after = rest.trim_start().strip_prefix(':').map(str::trim_start);
        if let Some(value) = after.and_then(|a| a.strip_prefix('"'))
            && let Some(end) = value.find('"')
            && !value[..end].trim().is_empty()
        {
            latest = Some(value[..end].trim().to_string());
        }
    }
    latest
}

fn generator_model_from_json(val: &Value) -> Option<String> {
    val.pointer("/steps")
        .or_else(|| val.pointer("/trajectory/steps"))?
        .as_array()?
        .iter()
        .rev()
        .find_map(|step| non_empty_str(step.pointer("/metadata/generatorModel")))
}

/// [`parse_agy_generator_model`] for a body that must be complete. A `GetCascadeTrajectory`
/// body cut off by the size cap holds the OLDEST steps, so its last `generatorModel` is a
/// stale early model: a body that is not valid JSON names no model.
fn parse_agy_complete_generator_model(body: &str) -> Option<String> {
    generator_model_from_json(&serde_json::from_str::<Value>(body).ok()?)
}

/// Split a trailing `-low`, `-medium` or `-high` off a model id. An id with no such suffix
/// is returned whole with no thinking level.
fn split_agy_thinking_suffix(model_id: &str) -> (String, Option<String>) {
    for level in ["low", "medium", "high"] {
        if let Some(base) = model_id.strip_suffix(&format!("-{level}"))
            && !base.is_empty()
        {
            return (base.to_string(), Some(level.to_string()));
        }
    }
    (model_id.to_string(), None)
}

/// Match a trajectory placeholder (then the config's default placeholder) against the
/// configured models. `available_models` lists each base id once, in config order.
fn resolve_agy_model_info(config: &AgyModelConfig, placeholder: Option<&str>) -> SessionModelInfo {
    let mut available_models: Vec<String> = Vec::new();
    for entry in &config.models {
        let (base, _) = split_agy_thinking_suffix(&entry.model_id);
        if !available_models.contains(&base) {
            available_models.push(base);
        }
    }
    let matched = [placeholder, config.default_placeholder.as_deref()]
        .into_iter()
        .flatten()
        .find_map(|wanted| {
            config
                .models
                .iter()
                .find(|entry| entry.placeholder.as_deref() == Some(wanted))
        });
    let (model, thinking_level) = match matched {
        Some(entry) => {
            let (base, level) = split_agy_thinking_suffix(&entry.model_id);
            (Some(base), level)
        }
        None => (None, None),
    };
    SessionModelInfo {
        model,
        thinking_level,
        available_models,
    }
}

fn offline_agy_model_info() -> SessionModelInfo {
    SessionModelInfo {
        model: None,
        thinking_level: None,
        available_models: owned_strings(&AGY_DEFAULT_MODELS),
    }
}

type AgyCache<K, V> = Mutex<BTreeMap<K, (Instant, Option<V>)>>;

static AGY_CONFIG_CACHE: AgyCache<u16, AgyModelConfig> = Mutex::new(BTreeMap::new());
static AGY_PLACEHOLDER_CACHE: AgyCache<(u16, String), String> = Mutex::new(BTreeMap::new());

/// `Some(entry)` while `key` has a live cache entry. A cached miss lives for the shorter
/// negative TTL.
fn agy_cache_get<K: Ord + Clone, V: Clone>(cache: &AgyCache<K, V>, key: &K) -> Option<Option<V>> {
    agy_cache_get_with_ttls(cache, key, AGY_CACHE_TTL, AGY_NEGATIVE_CACHE_TTL)
}

/// [`agy_cache_get`] with explicit TTLs, so a test can expire an entry without waiting.
fn agy_cache_get_with_ttls<K: Ord + Clone, V: Clone>(
    cache: &AgyCache<K, V>,
    key: &K,
    ttl: Duration,
    negative_ttl: Duration,
) -> Option<Option<V>> {
    let guard = cache.lock().unwrap_or_else(|err| err.into_inner());
    let (at, value) = guard.get(key)?;
    let ttl = if value.is_some() { ttl } else { negative_ttl };
    (at.elapsed() < ttl).then(|| value.clone())
}

/// Drop every cached answer for `port`. The kernel can hand a stub's port to a later stub
/// inside the TTL, and the stale entry would answer from the cache.
#[cfg(test)]
fn forget_agy_port_caches(port: u16) {
    AGY_CONFIG_CACHE
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .remove(&port);
    AGY_PLACEHOLDER_CACHE
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .retain(|(cached_port, _), _| *cached_port != port);
}

fn agy_cache_put<K: Ord, V>(cache: &AgyCache<K, V>, key: K, value: Option<V>) {
    let mut guard = cache.lock().unwrap_or_else(|err| err.into_inner());
    if guard.len() >= AGY_CACHE_MAX_ENTRIES {
        guard.clear();
    }
    guard.insert(key, (Instant::now(), value));
}

pub(crate) fn agy_rpc(
    port: u16,
    csrf: &str,
    method: &str,
    body: &str,
    max_bytes: usize,
) -> Option<String> {
    let procedure = format!("{AGY_LS_SERVICE}/{method}");
    let (code, _, resp) = fl_wire::dispatcher::post_agy_connect_rpc_limited(
        port,
        csrf,
        &procedure,
        body,
        AGY_RPC_TIMEOUT,
        max_bytes,
    )
    .ok()?;
    (code == 200).then_some(resp)
}

fn agy_cached_model_config(port: u16, csrf: &str) -> Option<AgyModelConfig> {
    if let Some(hit) = agy_cache_get(&AGY_CONFIG_CACHE, &port) {
        return hit;
    }
    let config = agy_rpc(port, csrf, "GetCascadeModelConfigData", "{}", 1024 * 1024)
        .and_then(|body| parse_agy_model_config(&body));
    agy_cache_put(&AGY_CONFIG_CACHE, port, config.clone());
    config
}

/// `step_index` of the newest complete JSON line in an Antigravity transcript tail.
/// A cut-off first line or a half-written last line is not valid JSON and is skipped.
fn agy_last_step_index(tail: &str) -> Option<u64> {
    tail.lines().rev().find_map(|line| {
        let val: Value = serde_json::from_str(line.trim()).ok()?;
        val.get("step_index").and_then(Value::as_u64)
    })
}

/// First step the tail-steps call asks for: [`AGY_STEPS_TAIL_MARGIN`] before the transcript's
/// last `step_index`. 0 when the transcript is missing, unreadable or names no step, and
/// for a conversation too short to need the tail call.
fn agy_tail_step_offset(transcript_path: Option<&str>) -> u64 {
    transcript_path
        .and_then(|p| read_file_tail(Path::new(p), AGY_TRANSCRIPT_TAIL_BYTES))
        .and_then(|tail| agy_last_step_index(&tail))
        .map_or(0, |index| index.saturating_sub(AGY_STEPS_TAIL_MARGIN))
}

/// Latest generator model from `GetCascadeTrajectorySteps`, which returns only the newest
/// steps. `None` when no tail offset is known, the call fails, or the steps name no model.
fn agy_tail_generator_model(
    port: u16,
    csrf: &str,
    session_id: &str,
    transcript_path: Option<&str>,
) -> Option<String> {
    let step_offset = agy_tail_step_offset(transcript_path);
    if step_offset == 0 {
        return None;
    }
    let body =
        serde_json::json!({ "cascadeId": session_id, "stepOffset": step_offset }).to_string();
    agy_rpc(
        port,
        csrf,
        "GetCascadeTrajectorySteps",
        &body,
        AGY_TRAJECTORY_MAX_BYTES,
    )
    .and_then(|resp| parse_agy_generator_model(&resp))
}

fn agy_cached_generator_model(
    port: u16,
    csrf: &str,
    session_id: &str,
    transcript_path: Option<&str>,
) -> Option<String> {
    let key = (port, session_id.to_string());
    if let Some(hit) = agy_cache_get(&AGY_PLACEHOLDER_CACHE, &key) {
        return hit;
    }
    // A long conversation's full trajectory can be tens of megabytes, so ask for the newest
    // steps first. The full call runs only when that fast path finds no model.
    let placeholder =
        agy_tail_generator_model(port, csrf, session_id, transcript_path).or_else(|| {
            let body = serde_json::json!({ "cascadeId": session_id }).to_string();
            agy_rpc(
                port,
                csrf,
                "GetCascadeTrajectory",
                &body,
                AGY_TRAJECTORY_MAX_BYTES,
            )
            .and_then(|resp| parse_agy_complete_generator_model(&resp))
        });
    agy_cache_put(&AGY_PLACEHOLDER_CACHE, key, placeholder.clone());
    placeholder
}

/// Ask the Antigravity language server which model a conversation runs and which models it
/// offers. The route's `ls_address` and `csrf_token` win; the environment fills a gap only
/// when `env_fallback` is true. Any failure yields the offline defaults.
///
/// `transcript_path` is the session's Antigravity transcript; its last `step_index` lets the
/// model lookup fetch only the newest steps.
fn agy_live_model_info_for_transcript(
    route_address: Option<&str>,
    route_csrf: Option<&str>,
    session_id: &str,
    transcript_path: Option<&str>,
    env_fallback: bool,
) -> SessionModelInfo {
    let from_env = |name: &str| {
        env_fallback
            .then(|| std::env::var(name).ok())
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let clean = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned)
    };
    let addresses: Vec<String> = [clean(route_address), from_env("ANTIGRAVITY_LS_ADDRESS")]
        .into_iter()
        .flatten()
        .collect();
    let Some(csrf) = clean(route_csrf).or_else(|| from_env("ANTIGRAVITY_CSRF_TOKEN")) else {
        return offline_agy_model_info();
    };
    // The token goes into an HTTP header: refuse anything that is not one plain value.
    if session_id.is_empty() || csrf.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return offline_agy_model_info();
    }
    let port = addresses.iter().find_map(|addr| {
        let port = fl_wire::identity::agy_ls_port(addr)?;
        let cached = agy_cache_get(&AGY_CONFIG_CACHE, &port).is_some();
        (cached || fl_wire::identity::agy_ls_address_listening(addr)).then_some(port)
    });
    let Some(port) = port else {
        return offline_agy_model_info();
    };
    let Some(config) = agy_cached_model_config(port, &csrf) else {
        return offline_agy_model_info();
    };
    let placeholder = agy_cached_generator_model(port, &csrf, session_id, transcript_path);
    resolve_agy_model_info(&config, placeholder.as_deref())
}

/// [`agy_live_model_info_for_transcript`] for a session with no known transcript.
#[cfg(test)]
fn agy_live_model_info(
    route_address: Option<&str>,
    route_csrf: Option<&str>,
    session_id: &str,
    env_fallback: bool,
) -> SessionModelInfo {
    agy_live_model_info_for_transcript(route_address, route_csrf, session_id, None, env_fallback)
}

// --- Claude -----------------------------------------------------------------

/// Largest transcript the hub reads whole. A bigger file is read from its tail, so the newest
/// turns survive and one runaway log cannot exhaust memory.
const TRANSCRIPT_READ_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// A JSONL line longer than this is skipped, not parsed.
const TRANSCRIPT_LINE_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Newest bytes of a vendor log (such as Grok's `unified.jsonl`) read for a quota poll.
const QUOTA_LOG_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// Whether `id` is one plain path component. A session id comes from a route table, a request
/// or a transcript, so it must not climb out of the directory it is joined onto.
fn is_safe_path_component(id: &str) -> bool {
    !id.is_empty() && id.len() <= 255 && id != "." && id != ".." && !id.contains(['/', '\\', '\0'])
}

/// A transcript as lossy UTF-8. Invalid bytes become U+FFFD, so one bad line cannot hide the
/// whole file. Only a regular file is opened (a FIFO would block the caller), and a file over
/// `max_bytes` is read from its tail with the cut-off first line dropped.
fn read_transcript_lossy(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    if !fs::metadata(path)?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let truncated = len > max_bytes;
    if truncated {
        file.seek(SeekFrom::Start(len - max_bytes))?;
    }
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    let mut body = bytes.as_slice();
    if truncated {
        body = body
            .iter()
            .position(|&b| b == b'\n')
            .map_or(&[][..], |nl| &body[nl + 1..]);
    }
    Ok(String::from_utf8_lossy(body).into_owned())
}

fn read_transcript(path: impl AsRef<Path>) -> std::io::Result<String> {
    #[cfg(test)]
    cost_counters::bump(&cost_counters::TRANSCRIPT_READS);
    read_transcript_lossy(path.as_ref(), TRANSCRIPT_READ_MAX_BYTES)
}

/// Read at most the last `max_bytes` of a file as lossy UTF-8.
fn read_file_tail(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max_bytes)))
        .ok()?;
    let mut bytes = Vec::new();
    // `take` bounds the read even when the file grew after `metadata()` or is a special file.
    file.take(max_bytes).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Canonical Claude model id for a `/model` display name (`Opus 5.5`), a `settings.json`
/// shorthand (`opus`) or a dated id (`claude-sonnet-5-5-20261001`), compared case-insensitively.
/// A bare family name means that family's current model. Any other id passes through unchanged.
fn normalize_claude_model(raw: &str) -> String {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    for id in [
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-haiku-4-5",
    ] {
        if lower == id
            || lower
                .strip_prefix(id)
                .is_some_and(|rest| rest.starts_with('-'))
        {
            return id.to_string();
        }
    }
    let mut words = lower.split_whitespace();
    let (Some(family), version) = (words.next(), words.next()) else {
        return trimmed.to_string();
    };
    let current = match family {
        "opus" | "sonnet" => "5-5",
        "haiku" => "4-5",
        _ => return trimmed.to_string(),
    };
    let version = match version {
        None => current.to_string(),
        Some(v) => match v.split_once('.') {
            Some((major, minor))
                if !major.is_empty()
                    && !minor.is_empty()
                    && major
                        .bytes()
                        .chain(minor.bytes())
                        .all(|b| b.is_ascii_digit()) =>
            {
                format!("{major}-{minor}")
            }
            _ => return trimmed.to_string(),
        },
    };
    format!("claude-{family}-{version}")
}

/// The model and effort a `/model` command left in a transcript line:
/// ``<local-command-stdout>Set model to `Opus 5.5` ... with `high` effort</local-command-stdout>``.
/// Only a genuine user turn whose whole text is that local-command output counts, so a prompt
/// that merely quotes the sentence, or an API-request blob that embeds one, is not a model change.
fn claude_model_command(val: &Value) -> Option<(String, Option<String>)> {
    if val.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let content = val.pointer("/message/content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                (b.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| b.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => return None,
    };
    let inner = text
        .trim()
        .strip_prefix("<local-command-stdout>")?
        .strip_suffix("</local-command-stdout>")?;
    let rest = inner.trim().strip_prefix("Set model to `")?;
    let (name, after) = rest.split_once('`')?;
    let name = plain_bounded_str(Some(&Value::String(name.to_string())), MODEL_ID_MAX_CHARS)?;
    let effort = after
        .split_once("with `")
        .and_then(|(_, tail)| tail.split_once("` effort"))
        .and_then(|(level, _)| {
            plain_bounded_str(
                Some(&Value::String(level.to_string())),
                CODEX_EFFORT_MAX_CHARS,
            )
        });
    Some((normalize_claude_model(&name), effort))
}

/// The newest model signal in Claude transcript JSONL: a `/model` command's `Set model to`
/// output or an assistant turn's `message.model` (or top-level `model`), whichever is last in the
/// file. The effort is the newest `/model` command's, when that command names the same model.
/// Claude Code's `<synthetic>` placeholder is not a model name and is skipped.
fn latest_claude_transcript_signal(jsonl: &str) -> Option<(String, Option<String>)> {
    let mut winner: Option<String> = None;
    for line in jsonl.lines().rev() {
        let Ok(val) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some((model, effort)) = claude_model_command(&val) {
            return match &winner {
                // An assistant turn after the command names the model in use now.
                Some(w) if *w != model => Some((w.clone(), None)),
                _ => Some((model, effort)),
            };
        }
        if winner.is_none()
            && let Some(model) =
                plain_bounded_str(val.pointer("/message/model"), MODEL_ID_MAX_CHARS)
                    .or_else(|| plain_bounded_str(val.get("model"), MODEL_ID_MAX_CHARS))
                    .filter(|model| !model.starts_with('<'))
        {
            winner = Some(normalize_claude_model(&model));
        }
    }
    winner.map(|model| (model, None))
}

/// Latest model named in Claude transcript JSONL (see [`latest_claude_transcript_signal`]).
#[cfg(test)]
fn latest_claude_transcript_model(jsonl: &str) -> Option<String> {
    latest_claude_transcript_signal(jsonl).map(|(model, _)| model)
}

/// `~/.claude/settings.json` `model`: `(normalized, as written)`.
fn claude_settings_model(settings: &Value) -> (Option<String>, Option<String>) {
    let raw = plain_bounded_str(settings.get("model"), MODEL_ID_MAX_CHARS);
    let model = raw.as_deref().map(normalize_claude_model);
    (model, raw)
}

/// The `settings.json` effort for `model`: the global `effortLevel`, else
/// `modelSettings.<model>.effortLevel`. The per-model entry is looked up by the canonical id,
/// and by `settings_raw` (the spelling `settings.json` uses for its own model) only when
/// `model` is that settings model.
fn claude_settings_effort(
    settings: &Value,
    model: &str,
    settings_model: Option<&str>,
    settings_raw: Option<&str>,
) -> Option<String> {
    let raw = settings_raw.filter(|_| settings_model == Some(model));
    plain_bounded_str(settings.get("effortLevel"), CODEX_EFFORT_MAX_CHARS).or_else(|| {
        [Some(model), raw].into_iter().flatten().find_map(|key| {
            plain_bounded_str(
                settings
                    .get("modelSettings")
                    .and_then(|m| m.get(key))
                    .and_then(|m| m.get("effortLevel")),
                CODEX_EFFORT_MAX_CHARS,
            )
        })
    })
}

fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

/// A Claude session's model and effort. The transcript names the session's own model (a turn's
/// `message.model`, or a `/model` command's output, whichever is newer). `settings.json` wins
/// when it was written after the transcript: a model changed in the Claude app or in that file
/// shows at once, before any new transcript line exists. Without a transcript signal,
/// `settings.json` supplies the model.
fn claude_model_info(transcript_path: Option<&str>, home: Option<&Path>) -> SessionModelInfo {
    let transcript = transcript_path.map(Path::new);
    let from_transcript = transcript
        .and_then(|p| read_file_tail(p, CLAUDE_TRANSCRIPT_TAIL_BYTES))
        .and_then(|tail| latest_claude_transcript_signal(&tail));
    let settings_path = home.map(|h| h.join(".claude").join("settings.json"));
    let settings = settings_path.as_deref().and_then(read_json_file);
    let (settings_model, settings_raw) = settings
        .as_ref()
        .map_or((None, None), claude_settings_model);
    let settings_effort = |model: &str| {
        settings.as_ref().and_then(|v| {
            claude_settings_effort(v, model, settings_model.as_deref(), settings_raw.as_deref())
        })
    };
    let settings_newer = match (
        settings_path.as_deref().and_then(file_mtime),
        transcript.and_then(file_mtime),
    ) {
        (Some(settings), Some(transcript)) => settings > transcript,
        _ => false,
    };
    let (model, thinking_level) = match from_transcript {
        Some((signal, effort)) if settings_newer && settings_model.is_some() => {
            let model = settings_model.clone().unwrap_or_default();
            // The transcript's command effort still holds when settings.json names that model.
            let effort = settings_effort(&model).or(effort.filter(|_| signal == model));
            (Some(model), effort)
        }
        Some((model, effort)) => {
            let effort = effort.or_else(|| settings_effort(&model));
            (Some(model), effort)
        }
        None => {
            let effort = settings_model.as_deref().and_then(settings_effort);
            (settings_model, effort)
        }
    };
    SessionModelInfo {
        model,
        thinking_level,
        available_models: owned_strings(&CLAUDE_MODELS),
    }
}

// --- Codex ------------------------------------------------------------------

/// `models[].slug` entries whose `visibility` is `list`.
fn codex_listed_models(cache: &Value) -> Vec<String> {
    cache
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
                .filter_map(|m| non_empty_str(m.get("slug")))
                .collect()
        })
        .unwrap_or_default()
}

/// Longest accepted per-session model id (Claude and Codex).
const MODEL_ID_MAX_CHARS: usize = 128;
/// Longest accepted per-session Codex reasoning effort.
const CODEX_EFFORT_MAX_CHARS: usize = 32;

/// A trimmed, non-empty, single-line string of at most `max_chars` characters with no
/// control characters. Anything else is `None`.
fn plain_bounded_str(value: Option<&Value>, max_chars: usize) -> Option<String> {
    non_empty_str(value)
        .filter(|s| s.chars().count() <= max_chars && !s.chars().any(char::is_control))
}

/// Per-session `(model, thinking_level)` from the newest Codex rollout lines. Each field is
/// the most recent valid value, found independently: a later line may carry only one of them.
/// The effort comes from `payload.effort`, else `payload.reasoning_effort`, else
/// `payload.collaboration_mode.settings.reasoning_effort`. A cut-off first line of a tail
/// is not valid JSON and is skipped like any other malformed line.
fn codex_rollout_settings(jsonl: &str) -> (Option<String>, Option<String>) {
    const EFFORT_POINTERS: [&str; 3] = [
        "/payload/effort",
        "/payload/reasoning_effort",
        "/payload/collaboration_mode/settings/reasoning_effort",
    ];
    let (mut model, mut effort) = (None, None);
    for line in jsonl.lines().rev() {
        if model.is_some() && effort.is_some() {
            break;
        }
        // Cheap prefilter: a line that names neither key cannot carry a value.
        if !line.contains("\"model\"") && !line.contains("effort") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if model.is_none() {
            model = plain_bounded_str(val.pointer("/payload/model"), MODEL_ID_MAX_CHARS);
        }
        if effort.is_none() {
            effort = EFFORT_POINTERS.iter().find_map(|pointer| {
                plain_bounded_str(val.pointer(pointer), CODEX_EFFORT_MAX_CHARS)
            });
        }
    }
    (model, effort)
}

/// Model and thinking level of a Codex session. The session's rollout transcript names what
/// the session runs; `~/.codex/config.toml` is only the default for a field the rollout
/// does not carry.
fn codex_model_info(transcript_path: Option<&str>, home: Option<&Path>) -> SessionModelInfo {
    let (session_model, session_effort) = transcript_path
        .and_then(|p| read_file_tail(Path::new(p), CLAUDE_TRANSCRIPT_TAIL_BYTES))
        .map(|tail| codex_rollout_settings(&tail))
        .unwrap_or_default();
    let codex_dir = home.map(|h| h.join(".codex"));
    let config = codex_dir
        .as_ref()
        .and_then(|d| read_toml_file(&d.join("config.toml")));
    let mut available_models = codex_dir
        .as_ref()
        .and_then(|d| read_json_file(&d.join("models_cache.json")))
        .map(|cache| codex_listed_models(&cache))
        .unwrap_or_default();
    if available_models.is_empty() {
        available_models = owned_strings(&CODEX_DEFAULT_MODELS);
    }
    SessionModelInfo {
        model: session_model
            .or_else(|| config.as_ref().and_then(|c| toml_non_empty_str(c, "model"))),
        thinking_level: session_effort.or_else(|| {
            config
                .as_ref()
                .and_then(|c| toml_non_empty_str(c, "model_reasoning_effort"))
        }),
        available_models,
    }
}

// --- Grok -------------------------------------------------------------------

/// Keys of the cache's `models` object whose `info.hidden` is not true.
fn grok_visible_models(cache: &Value) -> Vec<String> {
    cache
        .get("models")
        .and_then(Value::as_object)
        .map(|models| {
            models
                .iter()
                .filter(|(_, m)| m.pointer("/info/hidden").and_then(Value::as_bool) != Some(true))
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn grok_model_info(transcript_path: Option<&str>, home: Option<&Path>) -> SessionModelInfo {
    let grok_dir = home.map(|h| h.join(".grok"));
    let session_model = transcript_path
        .map(Path::new)
        .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some("chat_history.jsonl"))
        .and_then(Path::parent)
        .and_then(|dir| read_json_file(&dir.join("summary.json")))
        .and_then(|summary| non_empty_str(summary.get("current_model_id")));
    let config = grok_dir
        .as_ref()
        .and_then(|d| read_toml_file(&d.join("config.toml")));
    // The CLI writes these under `[models]`; accept them at the top level too.
    let config_str = |key: &str| {
        let models = config
            .as_ref()?
            .get("models")
            .and_then(toml::Value::as_table);
        models
            .and_then(|t| toml_non_empty_str(t, key))
            .or_else(|| toml_non_empty_str(config.as_ref()?, key))
    };
    let mut available_models = grok_dir
        .as_ref()
        .and_then(|d| read_json_file(&d.join("models_cache.json")))
        .map(|cache| grok_visible_models(&cache))
        .unwrap_or_default();
    if available_models.is_empty() {
        available_models = owned_strings(&GROK_DEFAULT_MODELS);
    }
    SessionModelInfo {
        model: session_model.or_else(|| config_str("default")),
        thinking_level: config_str("default_reasoning_effort"),
        available_models,
    }
}

fn find_transcript_file_in_home(home: &Path, session_id: &str) -> Option<String> {
    #[cfg(test)]
    cost_counters::bump(&cost_counters::TRANSCRIPT_FINDS);
    if !is_safe_path_component(session_id) {
        return None;
    }
    let agy = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("brain")
        .join(session_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl");
    if agy.is_file() {
        return Some(agy.display().to_string());
    }

    let muse = home
        .join(".local")
        .join("share")
        .join("muse")
        .join("sessions")
        .join(session_id)
        .join("session.jsonl");
    if muse.is_file() {
        return Some(muse.display().to_string());
    }

    let grok_sessions = home.join(".grok").join("sessions");
    if grok_sessions.is_dir() {
        let direct_chat = grok_sessions.join(session_id).join("chat_history.jsonl");
        if direct_chat.is_file() {
            return Some(direct_chat.display().to_string());
        }
        let direct_jsonl = grok_sessions.join(format!("{session_id}.jsonl"));
        if direct_jsonl.is_file() {
            return Some(direct_jsonl.display().to_string());
        }
        if let Ok(entries) = fs::read_dir(&grok_sessions) {
            for entry in entries.flatten() {
                let candidate = entry.path().join(session_id).join("chat_history.jsonl");
                if candidate.is_file() {
                    return Some(candidate.display().to_string());
                }
            }
        }
    }

    for sub in [
        home.join(".claude"),
        home.join(".codex"),
        home.join(".local").join("share").join("muse"),
        home.join(".grok"),
    ] {
        if !sub.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&sub)
            .max_depth(8)
            .into_iter()
            .filter_entry(|e| {
                if !e.file_type().is_dir() {
                    return true;
                }
                let Some(name) = e.file_name().to_str() else {
                    return true;
                };
                !matches!(
                    name,
                    ".tmp" | "tmp" | ".git" | "plugins" | "cache" | "node_modules" | "subagents"
                )
            })
            .flatten()
        {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let Some(ext) = p.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            if ext != "jsonl" {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && (stem == session_id || stem.ends_with(&format!("-{session_id}")))
            {
                return Some(p.display().to_string());
            }
            if matches!(
                p.file_name().and_then(|s| s.to_str()),
                Some("session.jsonl" | "chat_history.jsonl")
            ) && p
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|s| s.to_str())
                == Some(session_id)
            {
                return Some(p.display().to_string());
            }
        }
    }
    None
}

fn read_codex_configured_context_window(tpath: &Path) -> Option<u64> {
    let codex_dir = tpath
        .ancestors()
        .find(|a| a.file_name().and_then(|s| s.to_str()) == Some(".codex"))?;
    let raw = fs::read_to_string(codex_dir.join("config.toml")).ok()?;
    for line in raw.lines() {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if let Some(rest) = trimmed.strip_prefix("model_context_window") {
            let Some(after_eq) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let cleaned: String = after_eq
                .trim()
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect();
            if let Ok(val) = cleaned.parse::<u64>()
                && val > 0
            {
                return Some(val);
            }
        }
    }
    None
}

pub(crate) fn json_f64(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|n| n as f64))
        .or_else(|| value.as_u64().map(|n| n as f64))
}

fn json_u64_token(value: Option<&Value>) -> u64 {
    json_f64(value).map(|n| n.max(0.0) as u64).unwrap_or(0)
}

/// Normalize an ISO-8601 timestamp to Unix milliseconds.
///
/// Accepts `fl_core::At`'s 24-character millisecond `Z` spelling, second-precision `Z`,
/// and numeric offsets (`+00:00`, `-05:30`).
pub(crate) fn parse_iso8601_unix_millis(raw: &str) -> Option<u64> {
    let s = raw.trim().trim_end_matches(['.', ',', ';']);
    if let Ok(at) = fl_core::At::parse(s) {
        return Some(at.unix_millis());
    }
    let (datetime, offset_minutes) = if let Some(rest) = s.strip_suffix('Z') {
        (rest, 0i32)
    } else {
        let idx = s.rfind(['+', '-'])?;
        if idx < 11 {
            return None;
        }
        let (dt, off) = s.split_at(idx);
        (dt, parse_utc_offset_minutes(off)?)
    };
    let (base, frac_ms) = if let Some((clock, frac)) = datetime.split_once('.') {
        let mut digits = String::new();
        for ch in frac.chars() {
            if ch.is_ascii_digit() {
                digits.push(ch);
            } else {
                break;
            }
            if digits.len() == 3 {
                break;
            }
        }
        if digits.is_empty() {
            return None;
        }
        let padded = format!("{digits:0<3}");
        let ms: i64 = padded.parse().ok()?;
        (clock, ms)
    } else {
        (datetime, 0)
    };
    if base.len() != 19 {
        return None;
    }
    let spelled = format!("{base}.{frac_ms:03}Z");
    let at = fl_core::At::parse(&spelled).ok()?;
    let adjusted = at.unix_millis() as i64 - i64::from(offset_minutes) * 60_000;
    if adjusted < 0 {
        return None;
    }
    Some(adjusted as u64)
}

fn parse_utc_offset_minutes(raw: &str) -> Option<i32> {
    let (sign, rest) = if let Some(rest) = raw.strip_prefix('+') {
        (1i32, rest)
    } else {
        let rest = raw.strip_prefix('-')?;
        (-1, rest)
    };
    if !rest.is_ascii() {
        return None;
    }
    let (hours, minutes) = if let Some((h, m)) = rest.split_once(':') {
        (h, m)
    } else if rest.len() == 4 {
        rest.split_at(2)
    } else if rest.len() == 2 {
        (rest, "00")
    } else {
        return None;
    };
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

fn parse_reset_instant_from_reason(reason: &str) -> Option<u64> {
    const MARKER: &str = "resets at ";
    let idx = reason.find(MARKER)?;
    let token = reason[idx + MARKER.len()..].split_whitespace().next()?;
    parse_iso8601_unix_millis(token)
}

fn codex_window(value: Option<&Value>, window_minutes: u64) -> Option<QuotaWindowUsage> {
    let value = value?;
    let used_percent = json_f64(value.get("used_percent"))?;
    let resets_at_ms = value.get("resets_at").and_then(|raw| {
        raw.as_i64()
            .or_else(|| raw.as_u64().map(|n| n as i64))
            .or_else(|| raw.as_f64().map(|n| n as i64))
            .filter(|secs| *secs > 0)
            .map(|secs| (secs as u64).saturating_mul(1000))
    });
    Some(QuotaWindowUsage {
        used_percent,
        window_minutes: Some(window_minutes),
        resets_at_ms,
    })
}

/// Normalize Codex's legacy plan spelling for display.
fn normalize_codex_plan(plan: &str) -> String {
    let key = plan.trim().to_ascii_lowercase();
    if matches!(key.as_str(), "prolite" | "pro_lite" | "pro-lite") {
        "pro".to_owned()
    } else {
        plan.to_owned()
    }
}

fn codex_auth_path(
    home_override: Option<&Path>,
    codex_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(home) = home_override {
        let base = fs::canonicalize(home).ok()?;
        let dir = fs::canonicalize(base.join(".codex")).ok()?;
        if !dir.starts_with(&base) {
            return None;
        }
        Some(dir.join("auth.json"))
    } else if let Some(configured) = codex_home.filter(|s| !s.is_empty()) {
        let dir = PathBuf::from(configured);
        if !dir.is_absolute() {
            return None;
        }
        Some(dir.join("auth.json"))
    } else {
        let home = PathBuf::from(home?);
        home.is_absolute().then(|| home.join(".codex/auth.json"))
    }
}

#[cfg(all(test, unix))]
thread_local! {
    static BEFORE_CODEX_AUTH_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
}

/// Open the sandbox's Codex credentials without following a replaced path component.
#[cfg(unix)]
fn open_sandboxed_codex_auth(home: &Path) -> Option<fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let canonical_home = fs::canonicalize(home).ok()?;
    let root_name = CString::new("/").ok()?;
    // SAFETY: root_name is a valid C string; ownership of a successful fd is transferred to File.
    let root_fd = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return None;
    }
    // SAFETY: root_fd is a fresh successful descriptor.
    let mut dir = unsafe { fs::File::from_raw_fd(root_fd) };
    for component in canonical_home.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => CString::new(name.as_bytes()).ok()?,
            _ => return None,
        };
        // SAFETY: dir owns a live directory fd and name is NUL terminated.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        // SAFETY: fd is fresh and ownership is transferred once.
        dir = unsafe { fs::File::from_raw_fd(fd) };
    }
    let codex = CString::new(".codex").ok()?;
    // SAFETY: dir owns a live directory fd and codex is a valid C string.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            codex.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is fresh and ownership is transferred once.
    let codex_dir = unsafe { fs::File::from_raw_fd(fd) };
    #[cfg(test)]
    BEFORE_CODEX_AUTH_OPEN.with(|hook| {
        if let Some(f) = hook.borrow_mut().take() {
            f();
        }
    });
    let auth = CString::new("auth.json").ok()?;
    // SAFETY: codex_dir is a live directory fd and auth is a valid C string.
    let fd = unsafe {
        libc::openat(
            codex_dir.as_raw_fd(),
            auth.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is fresh and ownership is transferred once.
    Some(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(not(unix))]
fn open_sandboxed_codex_auth(_home: &Path) -> Option<fs::File> {
    // Without descriptor-anchored traversal, an override cannot be safely confined.
    None
}

/// Read only the plan claim from Codex's current account file. The token is never returned
/// or included in an error. A home override is hermetic and ignores ambient CODEX_HOME.
fn codex_auth_plan(home_override: Option<&Path>) -> Option<String> {
    use base64::Engine as _;
    use std::io::Read as _;

    const MAX_AUTH_BYTES: u64 = 256 * 1024;
    let file = if let Some(home) = home_override {
        open_sandboxed_codex_auth(home)?
    } else {
        let path = codex_auth_path(
            None,
            std::env::var_os("CODEX_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        #[cfg(not(any(unix, windows)))]
        if fs::symlink_metadata(&path).ok()?.file_type().is_symlink() {
            return None;
        }
        options.open(path).ok()?
    };
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_AUTH_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_AUTH_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_AUTH_BYTES {
        return None;
    }
    let auth: Value = serde_json::from_slice(&bytes).ok()?;
    let token = auth.pointer("/tokens/id_token")?.as_str()?;
    let mut segments = token.split('.');
    let _header = segments.next()?;
    let payload = segments.next()?;
    let _signature = segments.next()?;
    if segments.next().is_some() || payload.is_empty() {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    let plan = claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_plan_type")?
        .as_str()?
        .trim();
    if plan.is_empty()
        || plan.len() > 64
        || !plan
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return None;
    }
    Some(normalize_codex_plan(plan))
}

/// Session discovery searches `<home>/.codex/sessions`; a route does not carry a
/// credential-home identity. Only a rollout inside that sessions tree binds a
/// session to the corresponding auth file. Never use the hub's CODEX_HOME for it.
fn codex_bound_session_home(home: &Path, transcript: Option<&str>) -> Option<PathBuf> {
    let transcript = Path::new(transcript?);
    if !home.is_absolute() {
        return None;
    }
    let home = fs::canonicalize(home).ok()?;
    let codex = home.join(".codex");
    let sessions = codex.join("sessions");
    if fs::symlink_metadata(&codex).ok()?.file_type().is_symlink()
        || fs::symlink_metadata(&sessions)
            .ok()?
            .file_type()
            .is_symlink()
    {
        return None;
    }
    let session_root = fs::canonicalize(sessions).ok()?;
    let path = fs::canonicalize(transcript).ok()?;
    if !session_root.starts_with(&home) || !path.starts_with(&session_root) {
        return None;
    }
    Some(home)
}

fn codex_session_auth_plan(home: &Path, transcript: Option<&str>) -> Option<String> {
    codex_auth_plan(Some(&codex_bound_session_home(home, transcript)?))
}

/// A matrix account probe and a session's usage describe the same credential home only when
/// the session has a rollout in discovery's sessions tree and the configured account home
/// resolves to that tree's `.codex` directory. Missing paths and unbound sessions fail closed.
pub(crate) fn codex_session_matches_account(
    session: &DiscoveredSessionSummary,
    home_override: Option<&Path>,
) -> bool {
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let Some(bound_home) = home
        .as_deref()
        .and_then(|h| codex_bound_session_home(h, session.transcript_path.as_deref()))
    else {
        return false;
    };
    let account_path = codex_auth_path(
        home_override,
        std::env::var_os("CODEX_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let Some(account_dir) = account_path.as_deref().and_then(Path::parent) else {
        return false;
    };
    match (
        fs::canonicalize(bound_home.join(".codex")),
        fs::canonicalize(account_dir),
    ) {
        (Ok(session_dir), Ok(account_dir)) => session_dir == account_dir,
        _ => false,
    }
}

/// An account-level probe is not attached to a session and uses the configured
/// account home. It must not be merged into a session whose home is unknown.
pub(crate) fn codex_account_quota(home_override: Option<&Path>) -> Option<SessionQuotaUsage> {
    Some(SessionQuotaUsage {
        tier: Some(codex_auth_plan(home_override)?),
        primary_window: None,
        secondary_window: None,
        exhausted_reason: None,
    })
}

fn codex_quota_from_rollout(raw: &str) -> Option<SessionQuotaUsage> {
    let mut latest = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || !trimmed.contains("\"token_count\"")
            || !trimmed.contains("rate_limits")
        {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if val.get("type").and_then(Value::as_str) != Some("event_msg")
            || val.pointer("/payload/type").and_then(Value::as_str) != Some("token_count")
        {
            continue;
        }
        let Some(limits) = val
            .pointer("/payload/rate_limits")
            .filter(|v| v.is_object())
        else {
            continue;
        };
        let tier = limits
            .get("plan_type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(normalize_codex_plan);
        let exhausted_reason = limits
            .get("rate_limit_reached_type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let primary_window = codex_window(limits.get("primary"), 300);
        let secondary_window = codex_window(limits.get("secondary"), 10_080);
        if tier.is_none()
            && exhausted_reason.is_none()
            && primary_window.is_none()
            && secondary_window.is_none()
        {
            continue;
        }
        latest = Some(SessionQuotaUsage {
            tier,
            primary_window,
            secondary_window,
            exhausted_reason,
        });
    }
    latest
}

fn grok_quota_from_home(home: &Path) -> Option<SessionQuotaUsage> {
    let raw = read_transcript_lossy(
        &home.join(".grok").join("logs").join("unified.jsonl"),
        QUOTA_LOG_TAIL_BYTES,
    )
    .ok()?;
    let mut latest = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if !trimmed.contains("billing: fetched credits config") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if val.get("msg").and_then(Value::as_str) != Some("billing: fetched credits config") {
            continue;
        }
        let Some(ctx) = val.get("ctx") else {
            continue;
        };
        let tier = ctx
            .get("subscriptionTier")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let used_percent = json_f64(ctx.pointer("/config/creditUsagePercent"));
        let resets_at_ms = ctx
            .pointer("/config/currentPeriod/end")
            .and_then(Value::as_str)
            .or_else(|| {
                ctx.pointer("/config/billingPeriodEnd")
                    .and_then(Value::as_str)
            })
            .and_then(parse_iso8601_unix_millis);
        let secondary_window = used_percent.map(|used_percent| QuotaWindowUsage {
            used_percent,
            window_minutes: Some(10_080),
            resets_at_ms,
        });
        if tier.is_none() && secondary_window.is_none() {
            continue;
        }
        latest = Some(SessionQuotaUsage {
            tier,
            primary_window: None,
            secondary_window,
            exhausted_reason: None,
        });
    }
    latest
}

fn muse_quota_from_session(raw: &str) -> Option<SessionQuotaUsage> {
    let mut exhausted_reason = None;
    let mut resets_at_ms = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains("runtime.session") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if val.get("payload_type").and_then(Value::as_str) != Some("runtime.session") {
            continue;
        }
        let kind = val.pointer("/payload/event/kind").and_then(Value::as_str);
        if kind == Some("model_completed") {
            exhausted_reason = None;
            resets_at_ms = None;
            continue;
        }
        if !matches!(kind, Some("model_failure_terminal_intent" | "failed")) {
            continue;
        }
        let Some(reason) = val.pointer("/payload/event/reason").and_then(Value::as_str) else {
            continue;
        };
        if !reason.contains("Subscription quota exhausted") {
            continue;
        }
        resets_at_ms = parse_reset_instant_from_reason(reason);
        exhausted_reason = Some(reason.to_string());
    }
    let exhausted_reason = exhausted_reason?;
    Some(SessionQuotaUsage {
        tier: None,
        primary_window: Some(QuotaWindowUsage {
            used_percent: 100.0,
            window_minutes: None,
            resets_at_ms,
        }),
        secondary_window: None,
        exhausted_reason: Some(exhausted_reason),
    })
}

fn is_antigravity_transcript(path: &Path) -> bool {
    let lossy = path.to_string_lossy();
    lossy.contains(".system_generated/logs/transcript.jsonl")
        || lossy.contains(".system_generated\\logs\\transcript.jsonl")
}

/// Hydrate truncated `content` and `thinking` fields on Antigravity `transcript.jsonl` turns
/// from the sibling `transcript_full.jsonl` when present, while keeping tool outputs (`GENERIC`,
/// `TOOL_RESULT`, `CHECKPOINT`) and `tool_calls` compact.
fn hydrate_agy_truncated_turns(path: &Path, compact_raw: &str) -> std::io::Result<Option<String>> {
    use std::io::{BufRead, BufReader, Read};

    if !is_antigravity_transcript(path) || !compact_raw.contains("\"truncated_fields\"") {
        return Ok(None);
    }
    let full_path = path.with_file_name("transcript_full.jsonl");
    if !fs::metadata(&full_path).is_ok_and(|m| m.is_file()) {
        return Ok(None);
    }

    struct PendingHydration {
        compact_idx: usize,
        step_index: Option<u64>,
        created_at: Option<String>,
        line_type: Option<String>,
        needs_content: bool,
        needs_thinking: bool,
        val: Value,
        hydrated: bool,
    }

    let compact_lines: Vec<&str> = compact_raw.lines().collect();
    let mut pending = Vec::new();
    for (compact_idx, line) in compact_lines.iter().enumerate() {
        if !line.contains("\"truncated_fields\"") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let line_type = val.get("type").and_then(Value::as_str).map(str::to_string);
        if matches!(
            line_type.as_deref(),
            Some("GENERIC" | "TOOL_RESULT" | "tool_result" | "tool" | "CHECKPOINT")
        ) {
            continue;
        }
        let Some(trunc_arr) = val.get("truncated_fields").and_then(Value::as_array) else {
            continue;
        };
        let needs_content = trunc_arr.iter().any(|v| v.as_str() == Some("content"));
        let needs_thinking = trunc_arr.iter().any(|v| v.as_str() == Some("thinking"));
        if !needs_content && !needs_thinking {
            continue;
        }
        let step_index = val.get("step_index").and_then(Value::as_u64);
        let created_at = val
            .get("created_at")
            .and_then(Value::as_str)
            .map(str::to_string);
        pending.push(PendingHydration {
            compact_idx,
            step_index,
            created_at,
            line_type,
            needs_content,
            needs_thinking,
            val,
            hydrated: false,
        });
    }

    if pending.is_empty() {
        return Ok(None);
    }

    let file = fs::File::open(&full_path)?;
    let mut reader = BufReader::new(file.take(TRANSCRIPT_READ_MAX_BYTES * 2));
    let mut buf = Vec::new();
    let mut full_idx = 0usize;
    loop {
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let full_line = String::from_utf8_lossy(&buf);
        let trimmed = full_line.trim();
        if !trimmed.is_empty() {
            let candidate = pending.iter().any(|p| {
                !p.hydrated
                    && (p.compact_idx == full_idx
                        || p.created_at
                            .as_deref()
                            .is_some_and(|ca| trimmed.contains(ca))
                        || (p.created_at.is_none() && p.step_index.is_some()))
            });
            if candidate && let Ok(full_val) = serde_json::from_str::<Value>(trimmed) {
                let full_step = full_val.get("step_index").and_then(Value::as_u64);
                let full_ca = full_val.get("created_at").and_then(Value::as_str);
                let full_type = full_val.get("type").and_then(Value::as_str);
                if let Some(p) = pending.iter_mut().find(|p| {
                    !p.hydrated
                        && p.step_index == full_step
                        && p.created_at.as_deref() == full_ca
                        && p.line_type.as_deref() == full_type
                }) {
                    if let Some(obj) = p.val.as_object_mut() {
                        if p.needs_content
                            && let Some(full_content) = full_val.get("content")
                        {
                            obj.insert("content".to_string(), full_content.clone());
                        }
                        if p.needs_thinking
                            && let Some(full_thinking) = full_val.get("thinking")
                        {
                            obj.insert("thinking".to_string(), full_thinking.clone());
                        }
                        if let Some(trunc) = obj
                            .get_mut("truncated_fields")
                            .and_then(Value::as_array_mut)
                        {
                            trunc.retain(|v| !matches!(v.as_str(), Some("content" | "thinking")));
                        }
                    }
                    p.hydrated = true;
                    if pending.iter().all(|p| p.hydrated) {
                        break;
                    }
                }
            }
        }
        buf.clear();
        full_idx += 1;
    }

    if !pending.iter().any(|p| p.hydrated) {
        return Ok(None);
    }

    let mut out_lines: Vec<String> = compact_lines.iter().map(|&s| s.to_string()).collect();
    for p in pending {
        if p.hydrated
            && let Ok(serialized) = serde_json::to_string(&p.val)
        {
            out_lines[p.compact_idx] = serialized;
        }
    }
    Ok(Some(out_lines.join("\n")))
}

fn antigravity_context_from_transcript(raw: &str) -> Option<SessionContextUsage> {
    let lines: Vec<&str> = raw.lines().collect();
    let mut last_step_index: Option<u64> = None;
    let mut segment_start = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(step) = val.get("step_index").and_then(Value::as_u64)
            && last_step_index.is_some_and(|prev| agy_step_is_rewind(step, prev))
        {
            segment_start = index;
        }
        if let Some(step) = val.get("step_index").and_then(Value::as_u64) {
            last_step_index = Some(step);
        }
    }

    let mut latest: Option<SessionContextUsage> = None;
    for line in lines.iter().skip(segment_start) {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains("PLANNER_RESPONSE") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if val.get("type").and_then(Value::as_str) != Some("PLANNER_RESPONSE") {
            continue;
        }
        let used_tokens = json_u64_token(val.get("input_tokens"))
            .saturating_add(json_u64_token(val.get("cache_read_tokens")))
            .saturating_add(json_u64_token(val.get("output_tokens")));
        if used_tokens > 0 {
            latest = Some(SessionContextUsage {
                used_tokens,
                max_tokens: 1_000_000,
            });
        }
    }
    latest
}

#[cfg(test)]
thread_local! {
    static AFTER_TRANSCRIPT_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

/// Transcript files one request has read. Reuse a successful read and its parsed turns only
/// when file identity and metadata remain stable. Failed reads are retried on the next access.
#[cfg(test)]
pub(crate) fn after_next_transcript_read(f: impl FnOnce() + 'static) {
    AFTER_TRANSCRIPT_READ.with(|hook| *hook.borrow_mut() = Some(Box::new(f)));
}

#[derive(Debug, Default)]
pub(crate) struct TranscriptReads {
    raw: BTreeMap<String, String>,
    turns: BTreeMap<String, Vec<TranscriptTurn>>,
    stamps: BTreeMap<String, TranscriptStamp>,
}

#[derive(Debug, PartialEq, Eq)]
struct TranscriptStamp {
    main: FileStamp,
    agy_full: Option<FileStamp>,
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    modified: std::time::SystemTime,
    len: u64,
    device: u64,
    inode: u64,
    change_seconds: i64,
    change_nanos: i64,
}

impl TranscriptReads {
    #[cfg(unix)]
    fn stamp(path: &str) -> Option<TranscriptStamp> {
        use std::os::unix::fs::MetadataExt;
        fn one(path: &Path) -> std::io::Result<FileStamp> {
            let meta = fs::metadata(path)?;
            Ok(FileStamp {
                modified: meta.modified()?,
                len: meta.len(),
                device: meta.dev(),
                inode: meta.ino(),
                change_seconds: meta.ctime(),
                change_nanos: meta.ctime_nsec(),
            })
        }
        let path = Path::new(path);
        let main = one(path).ok()?;
        let agy_full = if is_antigravity_transcript(path) {
            match one(&path.with_file_name("transcript_full.jsonl")) {
                Ok(stamp) => Some(stamp),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return None,
            }
        } else {
            None
        };
        Some(TranscriptStamp { main, agy_full })
    }

    // Without a reliable file identity, read again on every access.
    #[cfg(not(unix))]
    fn stamp(_path: &str) -> Option<TranscriptStamp> {
        None
    }

    fn invalidate(&mut self, path: &str) {
        self.raw.remove(path);
        self.turns.remove(path);
        self.stamps.remove(path);
    }

    fn refresh_if_changed(&mut self, path: &str) {
        let current = Self::stamp(path);
        if current.is_none() || current.as_ref() != self.stamps.get(path) {
            self.invalidate(path);
        }
    }

    /// Reuse a read only while its file identity and metadata are unchanged.
    fn raw(&mut self, path: &str) -> Option<&str> {
        self.refresh_if_changed(path);
        if !self.raw.contains_key(path) {
            let before = Self::stamp(path);
            let text = read_transcript(path).ok()?;
            #[cfg(test)]
            AFTER_TRANSCRIPT_READ.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook();
                }
            });
            let after = Self::stamp(path);
            // A change or lost identity during the read cannot establish a safe baseline.
            if before != after {
                self.invalidate(path);
                return None;
            }
            if let Some(stamp) = before {
                self.stamps.insert(path.to_string(), stamp);
            }
            self.raw.insert(path.to_string(), text);
        }
        self.raw.get(path).map(String::as_str)
    }

    /// Reuse parsed turns only if the compact and hydration files stayed stable.
    fn turns(&mut self, path: &str) -> Option<&[TranscriptTurn]> {
        self.refresh_if_changed(path);
        if !self.turns.contains_key(path) {
            let raw = self.raw(path)?;
            let hydrated = match hydrate_agy_truncated_turns(Path::new(path), raw) {
                Ok(value) => value,
                Err(_) => {
                    self.invalidate(path);
                    return None;
                }
            };
            let parsed = parse_jsonl_transcript(hydrated.as_deref().unwrap_or(raw));
            let after = Self::stamp(path);
            if after.as_ref() != self.stamps.get(path) {
                self.invalidate(path);
                return None;
            }
            self.turns.insert(path.to_string(), parsed);
        }
        self.turns.get(path).map(Vec::as_slice)
    }
}

/// Read account quota for `session_id`.
///
/// Codex rollouts supply both windows. Grok's unified log supplies the weekly window.
/// Muse, Claude, and Antigravity merge on-disk exhaustion with the direct account probe.
/// The probe result is cached for 60 seconds. A home override does not run host binaries.
pub fn read_session_quota_usage_with_home(
    session_id: &str,
    home_override: Option<&Path>,
) -> Option<SessionQuotaUsage> {
    crate::direct_quota::with_probe_lock(|| {
        read_session_quota_usage_locked(session_id, home_override)
    })
}

/// Same read as [`read_session_quota_usage_with_home`], for a caller that already holds
/// the probe lock.
pub(crate) fn read_session_quota_usage_locked(
    session_id: &str,
    home_override: Option<&Path>,
) -> Option<SessionQuotaUsage> {
    quota_usage_with_reads_locked(session_id, home_override, &mut TranscriptReads::default())
}

/// [`read_session_quota_usage_with_home`] that reads the transcript through `files`.
pub(crate) fn read_session_quota_usage_with_reads(
    session_id: &str,
    home_override: Option<&Path>,
    files: &mut TranscriptReads,
) -> Option<SessionQuotaUsage> {
    crate::direct_quota::with_probe_lock(|| {
        quota_usage_with_reads_locked(session_id, home_override, files)
    })
}

fn quota_usage_with_reads_locked(
    session_id: &str,
    home_override: Option<&Path>,
    files: &mut TranscriptReads,
) -> Option<SessionQuotaUsage> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    let tpath = find_transcript_file_in_home(&home, trimmed)?;
    let adapter = adapter_for_transcript(Path::new(&tpath))?;
    // An Antigravity session's language server comes from its route; a restarted one has a new
    // address and token, so the route wins over what an earlier read stored. The model is not
    // known here, so the stored hint (or the `gemini-*` default) supplies it.
    let (address, csrf) = if adapter == "antigravity" {
        agy_route_endpoint(home_override, trimmed)
    } else {
        (None, None)
    };
    quota_for_resolved_route_locked(
        &adapter,
        Some(&tpath),
        home_override,
        files,
        &crate::direct_quota::AgyQuotaTarget {
            address: address.as_deref(),
            csrf: csrf.as_deref(),
            model: None,
            session: Some(trimmed),
        },
    )
}

/// The language-server address and token the saved route table names for Antigravity conversation
/// `conversation_id`, or `(None, None)` when no route matches.
fn agy_route_endpoint(
    home_override: Option<&Path>,
    conversation_id: &str,
) -> (Option<String>, Option<String>) {
    // The host's shared scan is cached; a home override has no cache, so read its saved table
    // (one file) rather than scan it again. The by-id endpoints budget one scan per request.
    let routes = match route_table_path(home_override) {
        Some(path) => RouteTable::load(Some(&path)),
        None => discovered_route_table(None),
    };
    routes
        .entries
        .values()
        .find_map(|entry| match &entry.route {
            VendorRoute::Agy {
                conversation_id: id,
                ls_address,
                csrf_token,
                ..
            } if id == conversation_id => Some((ls_address.clone(), csrf_token.clone())),
            _ => None,
        })
        .unwrap_or((None, None))
}

/// Quota for a route row whose transcript path is already known.
///
/// This does not search the home directory again. `session_id` lets the Antigravity read reuse
/// the language-server route and model an earlier discovery pass remembered for that session.
pub(crate) fn quota_for_resolved_route(
    adapter: &str,
    transcript_path: Option<&str>,
    home_override: Option<&Path>,
    session_id: &str,
) -> Option<SessionQuotaUsage> {
    quota_for_resolved_route_at(
        adapter,
        transcript_path,
        home_override,
        &crate::direct_quota::AgyQuotaTarget {
            session: Some(session_id),
            ..Default::default()
        },
    )
}

/// [`quota_for_resolved_route`] for a session whose Antigravity endpoint and model are known.
pub(crate) fn quota_for_resolved_route_at(
    adapter: &str,
    transcript_path: Option<&str>,
    home_override: Option<&Path>,
    agy: &crate::direct_quota::AgyQuotaTarget<'_>,
) -> Option<SessionQuotaUsage> {
    quota_for_resolved_route_with_reads(
        adapter,
        transcript_path,
        home_override,
        &mut TranscriptReads::default(),
        agy,
    )
}

/// [`quota_for_resolved_route_at`] that reads the transcript through `files`, so a caller that
/// also reads the context window or execution mode reads the file once.
fn quota_for_resolved_route_with_reads(
    adapter: &str,
    transcript_path: Option<&str>,
    home_override: Option<&Path>,
    files: &mut TranscriptReads,
    agy: &crate::direct_quota::AgyQuotaTarget<'_>,
) -> Option<SessionQuotaUsage> {
    crate::direct_quota::with_probe_lock(|| {
        quota_for_resolved_route_locked(adapter, transcript_path, home_override, files, agy)
    })
}

fn quota_for_resolved_route_locked(
    adapter: &str,
    transcript_path: Option<&str>,
    home_override: Option<&Path>,
    files: &mut TranscriptReads,
    agy: &crate::direct_quota::AgyQuotaTarget<'_>,
) -> Option<SessionQuotaUsage> {
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    match adapter {
        "codex" => {
            let mut quota = transcript_path
                .and_then(|path| files.raw(path))
                .and_then(codex_quota_from_rollout);
            if let Some(plan) = home
                .as_deref()
                .and_then(|h| codex_session_auth_plan(h, transcript_path))
            {
                quota
                    .get_or_insert(SessionQuotaUsage {
                        tier: None,
                        primary_window: None,
                        secondary_window: None,
                        exhausted_reason: None,
                    })
                    .tier = Some(plan);
            }
            quota
        }
        "grok" => {
            transcript_path?;
            let home = home?;
            grok_quota_from_home(&home)
        }
        "muse" => {
            let disk = transcript_path.and_then(|path| muse_quota_from_session(files.raw(path)?));
            let account = crate::direct_quota::muse_account_quota(home_override);
            crate::direct_quota::merge_account_quota(account, disk)
        }
        "claude" => {
            let disk = transcript_path.and_then(|path| {
                let reason = crate::direct_quota::claude_rate_limit_reason(files.raw(path)?)?;
                Some(SessionQuotaUsage {
                    tier: None,
                    primary_window: None,
                    secondary_window: None,
                    exhausted_reason: Some(reason),
                })
            });
            let account = crate::direct_quota::claude_account_quota(home_override);
            crate::direct_quota::merge_account_quota(account, disk)
        }
        "antigravity" => {
            let disk = transcript_path.and_then(|path| {
                let reason = crate::direct_quota::antigravity_exhausted_reason(files.raw(path)?)?;
                Some(SessionQuotaUsage {
                    tier: None,
                    primary_window: None,
                    secondary_window: None,
                    exhausted_reason: Some(reason),
                })
            });
            let account = crate::direct_quota::agy_account_quota(home_override, agy);
            crate::direct_quota::merge_account_quota(account, disk)
        }
        _ => None,
    }
}

fn adapter_for_transcript(path: &Path) -> Option<String> {
    let lossy = path.to_string_lossy();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if lossy.contains(".codex") && file_name.starts_with("rollout-") {
        return Some("codex".to_string());
    }
    if lossy.contains(".grok") {
        return Some("grok".to_string());
    }
    if file_name == "session.jsonl" && lossy.contains("muse") {
        return Some("muse".to_string());
    }
    if lossy.contains(".claude") {
        return Some("claude".to_string());
    }
    if is_antigravity_transcript(path) {
        return Some("antigravity".to_string());
    }
    None
}

/// The context window, in tokens, that a transcript with none of its own is assumed to have.
const GENERIC_CONTEXT_WINDOW: u64 = 500_000;

/// The context window the vendor registry declares for `adapter`'s vendor (the adapter name is
/// the vendor id), when it declares one: a built-in API vendor, a local engine or a registered
/// custom vendor. `None` for an unknown adapter and for a vendor whose window is `0` (unknown,
/// or the CLI decides).
pub(crate) fn registered_context_window(adapter: &str) -> Option<u64> {
    fl_core::lookup_vendor(adapter)
        .map(|d| d.default_context_window)
        .filter(|&n| n > 0)
}

/// Where a transcript names no window: the vendor's registered default, else the generic one.
/// Every answer the transcript itself gives wins over both.
pub(crate) fn fallback_context_window(adapter: &str) -> u64 {
    registered_context_window(adapter).unwrap_or(GENERIC_CONTEXT_WINDOW)
}

/// Read server-measured context window token usage for `session_id` when vendor session
/// metadata (such as Grok `signals.json`/`summary.json`, Claude `message.usage`, Codex `token_count`,
/// Antigravity `PLANNER_RESPONSE`, or Muse `model_completed`) is available.
pub fn read_session_context_usage_with_home(
    session_id: &str,
    store: &AnyStore,
    home_override: Option<&Path>,
) -> Option<SessionContextUsage> {
    let snap = snapshot_discovery_store(store);
    read_session_context_usage_from_snapshot(session_id, &snap, home_override)
}

/// Context-window read that does not borrow a live store. `snap` only supplies attempt
/// identity; the transcript scan is filesystem IO.
pub fn read_session_context_usage_from_snapshot(
    session_id: &str,
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
) -> Option<SessionContextUsage> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sessions = discover_sessions_impl(snap, home_override, false);
    context_usage_from_sessions(
        trimmed,
        &sessions,
        home_override,
        &mut TranscriptReads::default(),
    )
}

/// The context-window read of [`read_session_context_usage_from_snapshot`] over sessions the
/// caller already discovered, reading the transcript through `files`.
pub(crate) fn context_usage_from_sessions(
    session_id: &str,
    sessions: &[DiscoveredSessionSummary],
    home_override: Option<&Path>,
    files: &mut TranscriptReads,
) -> Option<SessionContextUsage> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut tpath_opt = sessions
        .iter()
        .find(|s| s.id == trimmed || s.persona.as_deref() == Some(trimmed))
        .and_then(|s| s.transcript_path.clone());

    if tpath_opt.is_none() {
        let home = home_override
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        if let Some(ref h) = home {
            tpath_opt = find_transcript_file_in_home(h, trimmed);
        }
    }

    context_usage_for_transcript(&tpath_opt?, files)
}

/// The context-window read for a session whose transcript path is known, choosing the vendor
/// parser from the path and reading the file through `files`.
fn context_usage_for_transcript(
    tpath_str: &str,
    files: &mut TranscriptReads,
) -> Option<SessionContextUsage> {
    let tpath = PathBuf::from(tpath_str);
    if tpath.file_name().and_then(|s| s.to_str()) == Some("chat_history.jsonl")
        && let Some(dir) = tpath.parent()
    {
        let signals_val = fs::read_to_string(dir.join("signals.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let summary_val = fs::read_to_string(dir.join("summary.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let usage_val = fs::read_to_string(dir.join("usage.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());

        let used_tokens = signals_val
            .as_ref()
            .and_then(|v| v.get("contextTokensUsed").and_then(Value::as_u64))
            .filter(|&n| n > 0)
            .or_else(|| {
                usage_val.as_ref().and_then(|v| {
                    v.pointer("/session/totalTokens")
                        .and_then(Value::as_u64)
                        .filter(|&n| n > 0)
                })
            });
        let max_tokens = signals_val
            .as_ref()
            .and_then(|v| v.get("contextWindowTokens").and_then(Value::as_u64))
            .filter(|&n| n > 0)
            .or_else(|| {
                summary_val
                    .as_ref()
                    .and_then(|v| v.get("context_window").and_then(Value::as_u64))
                    .filter(|&n| n > 0)
            })
            .unwrap_or_else(|| fallback_context_window("grok"));

        if let Some(used_tokens) = used_tokens {
            return Some(SessionContextUsage {
                used_tokens,
                max_tokens,
            });
        }
    }

    if tpath.to_string_lossy().contains(".codex")
        && let Some(raw) = files.raw(tpath_str)
    {
        let configured_max = read_codex_configured_context_window(&tpath);
        for line in raw.lines().rev() {
            let trimmed_line = line.trim();
            if trimmed_line.is_empty() || !trimmed_line.contains("\"token_count\"") {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed_line) else {
                continue;
            };
            if val.get("type").and_then(Value::as_str) != Some("event_msg")
                || val.pointer("/payload/type").and_then(Value::as_str) != Some("token_count")
            {
                continue;
            }
            let used_tokens = val
                .pointer("/payload/info/last_token_usage/total_tokens")
                .and_then(Value::as_u64)
                .filter(|&n| n > 0)
                .or_else(|| {
                    val.pointer("/payload/info/total_token_usage/total_tokens")
                        .and_then(Value::as_u64)
                        .filter(|&n| n > 0)
                });
            let max_tokens = configured_max
                .or_else(|| {
                    val.pointer("/payload/info/model_context_window")
                        .and_then(Value::as_u64)
                        .filter(|&n| n > 0)
                })
                .unwrap_or_else(|| fallback_context_window("codex"));
            if let Some(used_tokens) = used_tokens {
                return Some(SessionContextUsage {
                    used_tokens,
                    max_tokens,
                });
            }
        }
        if let Some(max_tokens) = configured_max {
            return Some(SessionContextUsage {
                used_tokens: 0,
                max_tokens,
            });
        }
    }

    if tpath.file_name().and_then(|s| s.to_str()) == Some("session.jsonl")
        && let Some(raw) = files.raw(tpath_str)
    {
        for line in raw.lines().rev() {
            let trimmed_line = line.trim();
            if trimmed_line.is_empty() || !trimmed_line.contains("\"model_completed\"") {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed_line) else {
                continue;
            };
            if val.pointer("/payload/event/kind").and_then(Value::as_str) == Some("model_completed")
            {
                let input = val
                    .pointer("/payload/event/usage/input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let output = val
                    .pointer("/payload/event/usage/output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let estimated = val
                    .pointer("/payload/event/estimated_prompt_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let used_tokens = if input.saturating_add(output) > 0 {
                    input.saturating_add(output)
                } else {
                    estimated
                };
                if used_tokens > 0 {
                    return Some(SessionContextUsage {
                        used_tokens,
                        max_tokens: 1_000_000,
                    });
                }
            }
        }
    }

    if tpath.to_string_lossy().contains(".claude")
        && let Some(raw) = files.raw(tpath_str)
    {
        // The latest row wins: an assistant turn's `message.usage`, or a `compact_boundary`
        // row's `compactMetadata.postTokens` (what the context holds after `/compact`).
        for line in raw.lines().rev() {
            let trimmed_line = line.trim();
            if trimmed_line.is_empty()
                || !(trimmed_line.contains("\"usage\"")
                    || trimmed_line.contains("compact_boundary")
                    || trimmed_line.contains("compactMetadata"))
            {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed_line) else {
                continue;
            };
            if val.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if val.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
                let post_tokens = val
                    .pointer("/compactMetadata/postTokens")
                    .and_then(Value::as_u64)
                    .filter(|&n| n > 0);
                if let Some(used_tokens) = post_tokens {
                    return Some(SessionContextUsage {
                        used_tokens,
                        max_tokens: 1_000_000,
                    });
                }
                // A boundary without a post-compaction size says nothing: keep looking.
                continue;
            }
            if let Some(usage) = val.pointer("/message/usage") {
                let input = usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let cache_create = usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let cache_read = usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let output = usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let used_tokens = input
                    .saturating_add(cache_create)
                    .saturating_add(cache_read)
                    .saturating_add(output);
                if used_tokens > 0 {
                    return Some(SessionContextUsage {
                        used_tokens,
                        max_tokens: 1_000_000,
                    });
                }
            }
        }
    }

    if is_antigravity_transcript(&tpath)
        && let Some(raw) = files.raw(tpath_str)
        && let Some(usage) = antigravity_context_from_transcript(raw)
    {
        return Some(usage);
    }

    None
}

/// Read and normalize a session transcript by `session_id` (or persona name / attempt IRI) (spec §3.2).
pub fn read_session_transcript(session_id: &str, store: &AnyStore) -> Vec<TranscriptTurn> {
    read_session_transcript_with_home(session_id, store, None)
}

/// Read and normalize a session transcript with an optional `HOME` directory override.
pub fn read_session_transcript_with_home(
    session_id: &str,
    store: &AnyStore,
    home_override: Option<&Path>,
) -> Vec<TranscriptTurn> {
    let snap = snapshot_discovery_store(store);
    read_session_transcript_from_snapshot(session_id, &snap, home_override)
}

/// Transcript read that uses a store snapshot for attempt fallback and does the file
/// parse after the caller has dropped the store guard.
pub fn read_session_transcript_from_snapshot(
    session_id: &str,
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
) -> Vec<TranscriptTurn> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let sessions = discover_sessions_impl(snap, home_override, false);
    transcript_from_sessions(
        trimmed,
        &sessions,
        snap,
        home_override,
        &mut TranscriptReads::default(),
    )
}

/// Lookup-only discovery (no model or quota information), for a request that resolves a
/// session and reads its transcript, context window and quota from one route scan.
pub(crate) fn lookup_sessions(
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
) -> Vec<DiscoveredSessionSummary> {
    discover_sessions_impl(snap, home_override, false)
}

/// The transcript read of [`read_session_transcript_from_snapshot`] over sessions the caller
/// already discovered, reading and parsing each file through `files`.
pub(crate) fn transcript_from_sessions(
    session_id: &str,
    sessions: &[DiscoveredSessionSummary],
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
    files: &mut TranscriptReads,
) -> Vec<TranscriptTurn> {
    transcript_observation_from_sessions(session_id, sessions, snap, home_override, files)
        .unwrap_or_default()
}

/// `None` means a discovered transcript could not be read consistently, not an empty transcript.
pub(crate) fn transcript_observation_from_sessions(
    session_id: &str,
    sessions: &[DiscoveredSessionSummary],
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
    files: &mut TranscriptReads,
) -> Option<Vec<TranscriptTurn>> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    for s in sessions {
        if (s.id == trimmed || s.persona.as_deref() == Some(trimmed))
            && let Some(ref tpath) = s.transcript_path
        {
            // Discovery supplies a path only after finding a file. If it disappears,
            // its previous contents are unknown; it is not a new empty transcript.
            if fs::metadata(tpath).is_err() {
                return None;
            }
            let turns = files.turns(tpath)?;
            if !turns.is_empty() {
                return Some(turns.to_vec());
            }
        }
    }
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    if let Some(ref h) = home
        && let Some(tpath) = find_transcript_file_in_home(h, trimmed)
    {
        let turns = files.turns(&tpath)?;
        if !turns.is_empty() {
            return Some(turns.to_vec());
        }
    }
    for attempt in &snap.attempts {
        if attempt.id == trimmed || attempt.record_str == trimmed {
            return Some(vec![TranscriptTurn {
                index: 0,
                role: "assistant".to_string(),
                content: attempt.output_excerpt.clone().unwrap_or_default(),
                thinking: None,
                tool_calls: Vec::new(),
                created_at: attempt.created_at.clone(),
            }]);
        }
    }
    Some(Vec::new())
}

/// Read a queued session without confusing a failed transcript read with no transcript.
pub(crate) fn read_session_transcript_observation_from_snapshot(
    session_id: &str,
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
) -> Option<Vec<TranscriptTurn>> {
    let sessions = discover_sessions_impl(snap, home_override, false);
    transcript_observation_from_sessions(
        session_id,
        &sessions,
        snap,
        home_override,
        &mut TranscriptReads::default(),
    )
}

/// A lower step rewinds. Step 0 or 1 also rewinds when it repeats.
/// A repeat above 1 is the same step, such as a command moving to the background.
/// `step < prev || step <= 1` is wrong: it rewinds the normal `0 -> 1` transition.
fn agy_step_is_rewind(step: u64, prev: u64) -> bool {
    step < prev || (step <= 1 && step <= prev)
}

/// Apply [`agy_step_is_rewind`]. `step_index` 0 drops any saved step 0 turn before
/// the clear. A displayable step 0 line is saved again after it is pushed. A later
/// rewind keeps that turn.
fn apply_agy_step_rewind(
    step_index: Option<u64>,
    last_step_index: &mut Option<u64>,
    step_zero_turn: &mut Option<TranscriptTurn>,
    turns: &mut Vec<TranscriptTurn>,
) {
    let Some(step) = step_index else {
        return;
    };
    if step == 0 {
        *step_zero_turn = None;
    }
    if last_step_index.is_some_and(|prev| agy_step_is_rewind(step, prev)) {
        turns.clear();
        if step > 0
            && let Some(mut zero) = step_zero_turn.clone()
        {
            zero.index = 0;
            turns.push(zero);
        }
    }
    *last_step_index = Some(step);
}

fn remember_agy_step_zero(
    step_index: Option<u64>,
    step_zero_turn: &mut Option<TranscriptTurn>,
    turns: &[TranscriptTurn],
) {
    if step_index == Some(0)
        && let Some(turn) = turns.last()
    {
        *step_zero_turn = Some(turn.clone());
    }
}

/// Normalize JSONL lines from Antigravity (`transcript.jsonl`), Claude Code, Codex, Muse, or Grok
/// into a unified `Vec<TranscriptTurn>` sequence (spec §3.2).
pub fn parse_jsonl_transcript(text: &str) -> Vec<TranscriptTurn> {
    #[cfg(test)]
    cost_counters::bump(&cost_counters::TRANSCRIPT_PARSES);
    let mut turns = Vec::new();
    let mut last_step_index: Option<u64> = None;
    let mut step_zero_turn: Option<TranscriptTurn> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.len() > TRANSCRIPT_LINE_MAX_BYTES {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(obj) = val.as_object() else {
            continue;
        };
        let step_index = obj.get("step_index").and_then(Value::as_u64);
        apply_agy_step_rewind(
            step_index,
            &mut last_step_index,
            &mut step_zero_turn,
            &mut turns,
        );

        let created_at = obj
            .get("created_at")
            .or_else(|| obj.get("timestamp"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                obj.get("recorded_at")
                    .and_then(Value::as_i64)
                    .and_then(format_unix_micros_iso8601)
            });

        if let Some(muse_turn) = parse_muse_event_turn(&val, created_at.clone(), turns.len()) {
            turns.push(muse_turn);
            continue;
        }
        if is_muse_run_completed_terminal(&val) && is_transcript_busy(&turns) {
            let index = turns.len();
            turns.push(TranscriptTurn {
                index,
                role: "assistant".to_string(),
                content: "(Run completed.)".to_string(),
                thinking: None,
                tool_calls: Vec::new(),
                created_at,
            });
            continue;
        }
        if handle_codex_rollout_line(&val, created_at.clone(), &mut turns) {
            continue;
        }

        if obj.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(t) = obj.get("type").and_then(Value::as_str)
            && (t.starts_with("api-request")
                || matches!(
                    t,
                    "attachment"
                        | "queue-operation"
                        | "last-prompt"
                        | "file-history-snapshot"
                        | "mode"
                        | "permission-mode"
                        | "atis-latch"
                        | "bridge-session"
                        | "ai-title"
                        | "tools-used"
                ))
        {
            continue;
        }

        // Determine role from Antigravity (`type` / `source`), Claude (`type` / `message.role`), or generic (`role`).
        let role = if let Some(r) = obj.get("role").and_then(Value::as_str) {
            normalize_role(r)
        } else if let Some(r) = val.pointer("/message/role").and_then(Value::as_str) {
            normalize_role(r)
        } else if let Some(t) = obj.get("type").and_then(Value::as_str) {
            match t {
                "USER_INPUT" | "user" | "human" => "user".to_string(),
                "PLANNER_RESPONSE" | "assistant" | "model" | "result" | "reasoning" => {
                    "assistant".to_string()
                }
                "GENERIC" | "TOOL_RESULT" | "tool_result" | "tool" => "tool".to_string(),
                "SYSTEM" | "SYSTEM_MESSAGE" | "system" => "system".to_string(),
                _ => {
                    if let Some(src) = obj.get("source").and_then(Value::as_str) {
                        normalize_role(src)
                    } else {
                        continue;
                    }
                }
            }
        } else if let Some(src) = obj.get("source").and_then(Value::as_str) {
            normalize_role(src)
        } else {
            continue;
        };

        let mut content_parts = Vec::new();
        let mut thinking_parts = Vec::new();
        let mut tool_calls = Vec::new();

        if let Some(c) = obj.get("content") {
            extract_content_and_tools(c, &mut content_parts, &mut thinking_parts, &mut tool_calls);
        } else if let Some(msg_c) = val.pointer("/message/content") {
            extract_content_and_tools(
                msg_c,
                &mut content_parts,
                &mut thinking_parts,
                &mut tool_calls,
            );
        } else if let Some(res) = obj
            .get("result")
            .or_else(|| obj.get("text"))
            .and_then(Value::as_str)
            && !res.trim().is_empty()
        {
            content_parts.push(res.to_string());
        }

        if let Some(th) = obj
            .get("thinking")
            .or_else(|| obj.get("reasoning"))
            .and_then(Value::as_str)
            && !th.trim().is_empty()
        {
            thinking_parts.push(th.to_string());
        }

        if obj.get("type").and_then(Value::as_str) == Some("reasoning")
            && let Some(summary_arr) = obj.get("summary").and_then(Value::as_array)
        {
            for item in summary_arr {
                if let Some(s) = item.as_str() {
                    if !s.trim().is_empty() {
                        thinking_parts.push(s.to_string());
                    }
                } else if let Some(t) = item.get("text").and_then(Value::as_str)
                    && !t.trim().is_empty()
                {
                    thinking_parts.push(t.to_string());
                }
            }
        }

        if let Some(arr) = obj.get("tool_calls").and_then(Value::as_array) {
            for tc in arr {
                if let Some(parsed) = parse_tool_call_item(tc) {
                    tool_calls.push(parsed);
                }
            }
        }

        let mut raw_joined = content_parts.join("\n");
        let mut effective_role = role;
        // Claude Code marks the summary it writes after a compaction boundary. Its text can
        // differ from the known opener, so the flag alone makes it a system notice.
        let is_compact_summary = effective_role == "user"
            && obj.get("isCompactSummary").and_then(Value::as_bool) == Some(true);
        if effective_role == "user" {
            let trimmed_raw = raw_joined.trim();
            if is_compact_summary
                || obj.get("synthetic_reason").is_some()
                || is_grok_scaffolding_turn(trimmed_raw)
            {
                effective_role = "system".to_string();
            } else {
                raw_joined = strip_grok_user_query_wrapper(&raw_joined);
            }
        }
        let (mut role, mut content) = if is_compact_summary {
            // Harness text, whatever it quotes: never re-sorted by the wrapper rules.
            ("system".to_string(), raw_joined.trim().to_string())
        } else {
            normalize_system_turn(&effective_role, &raw_joined)
        };
        if role == "user"
            && let Some(command) = slash_command_text(&content)
        {
            content = command;
        }
        let thinking = if thinking_parts.is_empty() {
            None
        } else {
            Some(thinking_parts.join("\n"))
        };

        if role == "user" {
            let raw_content = obj
                .get("content")
                .or_else(|| val.pointer("/message/content"));
            if let Some(arr) = raw_content.and_then(Value::as_array)
                && !arr.is_empty()
                && arr
                    .iter()
                    .all(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
            {
                role = "tool".to_string();
            }
        }

        if role == "assistant"
            && content.trim_start().starts_with("Created At:")
            && thinking.is_none()
            && tool_calls.is_empty()
        {
            role = "tool".to_string();
        }

        if content.is_empty() && thinking.is_none() && tool_calls.is_empty() {
            continue;
        }

        // Merge standalone reasoning turn (e.g. Grok `{"type":"reasoning","summary":[...]}`)
        // into the immediately following assistant turn when that assistant turn has no thinking yet.
        if role == "assistant"
            && thinking.is_none()
            && (!content.is_empty() || !tool_calls.is_empty())
            && let Some(prev) = turns.last_mut()
            && prev.role == "assistant"
            && prev.content.is_empty()
            && prev.tool_calls.is_empty()
            && prev.thinking.is_some()
        {
            prev.content = content;
            prev.tool_calls = tool_calls;
            if prev.created_at.is_none() {
                prev.created_at = created_at;
            }
            remember_agy_step_zero(step_index, &mut step_zero_turn, &turns);
            continue;
        }

        let index = turns.len();
        turns.push(TranscriptTurn {
            index,
            role,
            content,
            thinking,
            tool_calls,
            created_at,
        });
        remember_agy_step_zero(step_index, &mut step_zero_turn, &turns);
    }
    turns
}

fn handle_codex_rollout_line(
    val: &Value,
    created_at: Option<String>,
    turns: &mut Vec<TranscriptTurn>,
) -> bool {
    let Some(top_type) = val.get("type").and_then(Value::as_str) else {
        return false;
    };
    if matches!(top_type, "session_meta" | "turn_context" | "event_msg") {
        return true;
    }
    if top_type != "response_item" {
        return false;
    }
    let Some(payload) = val.get("payload") else {
        return true;
    };
    let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
    match ptype {
        "message" => {
            let raw_role = payload
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("assistant");
            let mut content_parts = Vec::new();
            let mut thinking_parts = Vec::new();
            let mut tool_calls = Vec::new();
            if let Some(c) = payload.get("content") {
                extract_content_and_tools(
                    c,
                    &mut content_parts,
                    &mut thinking_parts,
                    &mut tool_calls,
                );
            }
            let raw_joined = content_parts.join("\n");
            let initial_role = if raw_role == "developer" {
                "system".to_string()
            } else if raw_role == "user" {
                let trimmed_raw = raw_joined.trim_start();
                if trimmed_raw.starts_with("# AGENTS.md instructions for ")
                    || trimmed_raw.starts_with("<environment_context>")
                    || trimmed_raw.starts_with("<INSTRUCTIONS>")
                    || trimmed_raw.starts_with("<permissions instructions>")
                {
                    "system".to_string()
                } else if trimmed_raw.starts_with("<user_shell_command>") {
                    "tool".to_string()
                } else {
                    "user".to_string()
                }
            } else {
                normalize_role(raw_role)
            };
            let (role, content) = normalize_system_turn(&initial_role, &raw_joined);
            let thinking = if thinking_parts.is_empty() {
                None
            } else {
                Some(thinking_parts.join("\n"))
            };
            if content.is_empty() && thinking.is_none() && tool_calls.is_empty() {
                return true;
            }
            if role == "assistant"
                && thinking.is_none()
                && let Some(prev) = turns.last_mut()
                && prev.role == "assistant"
                && prev.content.is_empty()
                && prev.tool_calls.is_empty()
                && prev.thinking.is_some()
            {
                prev.content = content;
                prev.tool_calls = tool_calls;
                if prev.created_at.is_none() {
                    prev.created_at = created_at;
                }
                return true;
            }
            let index = turns.len();
            turns.push(TranscriptTurn {
                index,
                role,
                content,
                thinking,
                tool_calls,
                created_at,
            });
            true
        }
        "reasoning" => {
            let mut thinking_parts = Vec::new();
            if let Some(summary_arr) = payload.get("summary").and_then(Value::as_array) {
                for item in summary_arr {
                    if let Some(s) = item.as_str() {
                        if !s.trim().is_empty() {
                            thinking_parts.push(s.to_string());
                        }
                    } else if let Some(t) = item.get("text").and_then(Value::as_str)
                        && !t.trim().is_empty()
                    {
                        thinking_parts.push(t.to_string());
                    }
                }
            }
            if !thinking_parts.is_empty() {
                let index = turns.len();
                turns.push(TranscriptTurn {
                    index,
                    role: "assistant".to_string(),
                    content: String::new(),
                    thinking: Some(thinking_parts.join("\n")),
                    tool_calls: Vec::new(),
                    created_at,
                });
            }
            true
        }
        "custom_tool_call" | "function_call" => {
            if let Some(tc) = parse_tool_call_item(payload) {
                if let Some(prev) = turns.last_mut()
                    && prev.role == "assistant"
                    && prev.tool_calls.is_empty()
                {
                    prev.tool_calls.push(tc);
                } else {
                    let index = turns.len();
                    turns.push(TranscriptTurn {
                        index,
                        role: "assistant".to_string(),
                        content: String::new(),
                        thinking: None,
                        tool_calls: vec![tc],
                        created_at,
                    });
                }
            }
            true
        }
        "custom_tool_call_output" | "function_call_output" => {
            let mut content_parts = Vec::new();
            let mut thinking_parts = Vec::new();
            let mut tool_calls = Vec::new();
            if let Some(out) = payload.get("output") {
                extract_content_and_tools(
                    out,
                    &mut content_parts,
                    &mut thinking_parts,
                    &mut tool_calls,
                );
            }
            let content = content_parts.join("\n");
            if !content.is_empty() {
                let index = turns.len();
                turns.push(TranscriptTurn {
                    index,
                    role: "tool".to_string(),
                    content,
                    thinking: None,
                    tool_calls: Vec::new(),
                    created_at,
                });
            }
            true
        }
        _ => true,
    }
}

/// Leading tags of the prompt scaffolding Grok writes as separate `user` rows: the
/// `<user_info>` + `<rules>` preamble and `<system-reminder>` notices.
const GROK_SCAFFOLDING_OPENERS: [&str; 4] =
    ["<user_info>", "<rules>", "<user_rules", "<system-reminder>"];

fn starts_with_grok_scaffolding(trimmed: &str) -> bool {
    GROK_SCAFFOLDING_OPENERS
        .iter()
        .any(|tag| trimmed.starts_with(tag))
}

/// True for a Grok `user` row that is harness scaffolding, not an operator prompt: it opens
/// with a scaffolding tag and carries no `<user_query>`. Only a leading tag counts, so a prompt
/// that mentions `<rules>` mid-sentence is not scaffolding.
fn is_grok_scaffolding_turn(trimmed: &str) -> bool {
    !trimmed.contains("<user_query>") && starts_with_grok_scaffolding(trimmed)
}

fn strip_grok_user_query_wrapper(content: &str) -> String {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    let trimmed = content.trim();
    let after_notice = trimmed
        .strip_prefix("The user sent a message while you were working:\n")
        .or_else(|| trimmed.strip_prefix("The user sent a message while you were working:\r\n"))
        .unwrap_or(trimmed)
        .trim_start();
    // Scaffolding and the prompt can share one message. The prompt is then the last
    // `<user_query>` block and everything around it is scaffolding.
    let inner = if let Some(inner) = after_notice.strip_prefix(OPEN) {
        inner
    } else if starts_with_grok_scaffolding(after_notice)
        && let Some(at) = after_notice.rfind(OPEN)
    {
        &after_notice[at + OPEN.len()..]
    } else {
        return content.to_string();
    };
    // A truncated block has no close tag and keeps the rest of the text.
    let body = match inner.rfind(CLOSE) {
        Some(at) => &inner[..at],
        None => inner,
    };
    body.trim().to_string()
}

/// Wrapper tags Antigravity puts around operator and notice turns.
const AGY_WRAPPER_TAGS: [&str; 5] = [
    "user_request",
    "additional_metadata",
    "ephemeral_message",
    "user_settings_change",
    "context_summary",
];

/// Strip Antigravity `<USER_REQUEST>`, `<ADDITIONAL_METADATA>`, `<EPHEMERAL_MESSAGE>`,
/// `<USER_SETTINGS_CHANGE>` and `<CONTEXT_SUMMARY>` wrappers from a user turn. Mirrors the UI's `stripAgyUserRequestWrapper`.
///
/// Content that does not start with one of these tags is returned unchanged, so a message
/// that merely quotes a tag stays intact. Otherwise every `<ADDITIONAL_METADATA>`,
/// `<USER_SETTINGS_CHANGE>` and `<CONTEXT_SUMMARY>` block is removed, every `<USER_REQUEST>` and `<EPHEMERAL_MESSAGE>` block is replaced by its trimmed
/// inner text, runs of three or more newlines become two, and the result is trimmed.
pub(crate) fn strip_agy_user_request_wrapper(content: &str) -> String {
    if !starts_with_agy_wrapper_tag(content) {
        return content.to_string();
    }
    let (without_metadata, _) =
        replace_agy_blocks(content, "additional_metadata", |_| String::new());
    // Settings notices and compaction summaries go before `<USER_REQUEST>` so a summary that
    // quotes a request tag cannot be mistaken for one.
    let (without_settings, _) =
        replace_agy_blocks(&without_metadata, "user_settings_change", |_| String::new());
    let (without_summary, _) =
        replace_agy_blocks(&without_settings, "context_summary", |_| String::new());
    let (without_request, _) = replace_agy_blocks(&without_summary, "user_request", |inner| {
        inner.trim().to_string()
    });
    let (unwrapped, _) = replace_agy_blocks(&without_request, "ephemeral_message", |inner| {
        inner.trim().to_string()
    });
    collapse_blank_runs(&unwrapped).trim().to_string()
}

/// True when `text` starts with `prefix`, ASCII case-insensitively. Only the leading bytes are
/// compared: no copy of the text is made.
fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    text.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// True when `text` holds `needle` anywhere, ASCII case-insensitively, without a lowercase copy.
fn contains_ignore_ascii_case(text: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    !needle.is_empty()
        && text
            .as_bytes()
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle))
}

fn starts_with_agy_wrapper_tag(content: &str) -> bool {
    let head = content.trim_start();
    AGY_WRAPPER_TAGS.iter().any(|tag| {
        let bytes = head.as_bytes();
        // `<tag>`: the tag name sits between a `<` and a `>`.
        bytes.len() >= tag.len() + 2
            && bytes[0] == b'<'
            && bytes[1..=tag.len()].eq_ignore_ascii_case(tag.as_bytes())
            && bytes[tag.len() + 1] == b'>'
    })
}

/// True when the turn starts with an Antigravity wrapper tag and holds a complete
/// `<EPHEMERAL_MESSAGE>` block: a system notice, not an operator message.
fn is_agy_ephemeral_message(content: &str) -> bool {
    starts_with_agy_wrapper_tag(content)
        && replace_agy_blocks(content, "ephemeral_message", |_| String::new()).1
}

/// Replace each complete `<tag>...</tag>` block (ASCII case-insensitive, non-greedy) with
/// `replace(inner)`. An opening tag with no closing tag is left as it is.
fn replace_agy_blocks(text: &str, tag: &str, replace: impl Fn(&str) -> String) -> (String, bool) {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    // ASCII lowercasing keeps byte offsets, so indexes into `lowered` are valid in `text`.
    let lowered = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut found = false;
    while let Some(rel_open) = lowered[cursor..].find(&open) {
        let open_at = cursor + rel_open;
        let inner_at = open_at + open.len();
        let Some(rel_close) = lowered[inner_at..].find(&close) else {
            break;
        };
        let close_at = inner_at + rel_close;
        out.push_str(&text[cursor..open_at]);
        out.push_str(&replace(&text[inner_at..close_at]));
        cursor = close_at + close.len();
        found = true;
    }
    out.push_str(&text[cursor..]);
    (out, found)
}

/// Collapse every run of three or more newlines into exactly two.
fn collapse_blank_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = 0;
    for ch in text.chars() {
        if ch == '\n' {
            run += 1;
            if run <= 2 {
                out.push(ch);
            }
        } else {
            run = 0;
            out.push(ch);
        }
    }
    out
}

/// Return `true` when the latest turn in `turns` indicates the agent session is actively working
/// (e.g. a user/wire/tool/system turn is awaiting an assistant reply, or the latest assistant turn
/// issued tool calls that have not yet produced a final text-only assistant turn).
pub fn is_transcript_busy(turns: &[TranscriptTurn]) -> bool {
    let Some(last) = turns.last() else {
        return false;
    };
    !(last.role == "assistant"
        && last.tool_calls.is_empty()
        && (!last.content.trim().is_empty() || last.thinking.is_none()))
}

fn normalize_role(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "user" | "user_explicit" | "human" => "user".to_string(),
        "assistant" | "model" | "planner_response" => "assistant".to_string(),
        "system" | "system_message" => "system".to_string(),
        other => other.to_string(),
    }
}

fn clean_sender_token(raw: &str) -> String {
    let mut s = raw.trim();
    loop {
        let before = s;
        s = s.trim_matches(|c| c == '"' || c == '\'');
        s = s.trim_end_matches([',', ';', ']', ')', '}']);
        s = s.trim_matches(|c| c == '"' || c == '\'');
        if s == before {
            break;
        }
    }
    s.to_string()
}

/// Openers of Claude Code transcript user turns that the harness, not the operator, wrote.
/// A turn whose text starts with one is a system notice.
const SYSTEM_NOTICE_OPENERS: [&str; 5] = [
    "<task-notification>",
    "<local-command-caveat>",
    "<local-command-stdout>",
    "[SYSTEM NOTIFICATION - NOT USER INPUT]",
    // The summary Claude Code writes after a `/compact` boundary.
    "This session is being continued from a previous conversation",
];
/// Spellings of the `<agent-message ...>` open tag: plain, and backslash-escaped.
const AGENT_MESSAGE_OPEN_TAGS: [&str; 2] = ["<agent-message", "<\\agent-message"];
/// Spellings of the `</agent-message>` close tag: plain, and the two backslash-escaped forms.
const AGENT_MESSAGE_CLOSE_TAGS: [&str; 3] = [
    "</agent-message>",
    "<\\/agent-message>",
    "</\\agent-message>",
];
/// Last line of the boilerplate header in front of a subagent hand-back report.
const SUBAGENT_REPORT_HEADER_END: &str = "The report follows:";
/// The boilerplate header is short; a later occurrence of its last line is report text.
const SUBAGENT_REPORT_HEADER_MAX_BYTES: usize = 1024;

fn starts_with_system_notice_opener(text: &str) -> bool {
    let text = text.trim_start();
    SYSTEM_NOTICE_OPENERS
        .iter()
        .any(|opener| text.starts_with(opener))
}

/// The text after an `<agent-message ...>` open tag (either spelling), when `text` starts
/// with one. The tag name must end at whitespace or `>`, so `<agent-messages>` is no tag.
fn strip_agent_message_open_tag(text: &str) -> Option<&str> {
    let text = text.trim_start();
    AGENT_MESSAGE_OPEN_TAGS.iter().find_map(|tag| {
        let rest = text.strip_prefix(tag)?;
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '>') {
            return None;
        }
        Some(&rest[rest.find('>')? + 1..])
    })
}

/// Whether `text` opens a subagent hand-back. The marker must be at the START of the
/// text (after the optional `Another Claude session sent a message:` wrapper, which the
/// caller strips first): an `<agent-message` open tag or a `[Subagent ` bracket. A
/// marker anywhere later is ordinary prose, such as an operator quoting a hand-back, and
/// never reclassifies the turn.
fn starts_subagent_handback(text: &str) -> bool {
    let text = text.trim_start();
    strip_agent_message_open_tag(text).is_some() || text.starts_with("[Subagent ")
}

/// The report body of a subagent hand-back: the `<agent-message ...>` open tag, the
/// `</agent-message>` close tag and the `[Subagent hand-back] ... The report follows:`
/// header are removed. A header without its `The report follows:` line is kept.
fn clean_subagent_handback(text: &str) -> String {
    let mut body = text.trim();
    if let Some(after_tag) = strip_agent_message_open_tag(body) {
        body = after_tag.trim();
    }
    for tag in AGENT_MESSAGE_CLOSE_TAGS {
        if let Some(before_tag) = body.strip_suffix(tag) {
            body = before_tag.trim_end();
            break;
        }
    }
    if body.starts_with("[Subagent ")
        && let Some(at) = body.find(SUBAGENT_REPORT_HEADER_END)
        && at < SUBAGENT_REPORT_HEADER_MAX_BYTES
    {
        body = body[at + SUBAGENT_REPORT_HEADER_END.len()..].trim();
    }
    body.to_string()
}

/// First words of the harness preamble in front of a `<SYSTEM_MESSAGE>` block.
const SYSTEM_MESSAGE_PREAMBLE: &str = "The following is a <SYSTEM_MESSAGE>";

/// How a tag's text is found in a string.
enum TagText<'a> {
    Absent,
    /// An open tag with no close tag after it.
    Unterminated,
    Closed(&'a str),
}

/// The text between the first `<tag>` and the next `</tag>`. One forward search per tag.
fn tag_text<'a>(text: &'a str, tag: &str) -> TagText<'a> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = text.find(&open) else {
        return TagText::Absent;
    };
    let inner = &text[start + open.len()..];
    match inner.find(&close) {
        Some(end) => TagText::Closed(&inner[..end]),
        None => TagText::Unterminated,
    }
}

/// The command a Claude Code slash-command wrapper stands for, such as `/compact` or
/// `/compact focus on auth`. Claude Code records `<command-name>/compact</command-name>
/// <command-message>compact</command-message><command-args></command-args>` as a user
/// turn. `None` when the text does not START with a `<command-name>` or `<command-message>`
/// tag (so prose that only quotes the tag stays as written), or when a tag it needs is
/// unterminated or empty.
fn slash_command_text(text: &str) -> Option<String> {
    let text = text.trim_start();
    if !(text.starts_with("<command-name>") || text.starts_with("<command-message>")) {
        return None;
    }
    let name = match tag_text(text, "command-name") {
        TagText::Closed(name) if !name.trim().is_empty() => name.trim().to_string(),
        TagText::Unterminated => return None,
        _ => match tag_text(text, "command-message") {
            TagText::Closed(message) if !message.trim().is_empty() => {
                let message = message.trim();
                if message.starts_with('/') {
                    message.to_string()
                } else {
                    format!("/{message}")
                }
            }
            _ => return None,
        },
    };
    match tag_text(text, "command-args") {
        TagText::Unterminated => None,
        TagText::Closed(args) if !args.trim().is_empty() => Some(format!("{name} {}", args.trim())),
        TagText::Closed(_) | TagText::Absent => Some(name),
    }
}

fn normalize_system_turn(role: &str, content: &str) -> (String, String) {
    if role != "system" && role != "user" {
        return (role.to_string(), content.to_string());
    }
    let from_ephemeral = is_agy_ephemeral_message(content);
    let unwrapped = strip_agy_user_request_wrapper(content);
    // An Antigravity turn that is only metadata, a settings notice or a compaction summary
    // holds no operator text. A summary may quote `<USER_REQUEST>`, so a leading
    // `<CONTEXT_SUMMARY>` decides on its own unless the turn also has a closed request.
    if starts_with_agy_wrapper_tag(content) {
        let is_summary_only =
            starts_with_ignore_ascii_case(content.trim_start(), "<context_summary>")
                && !contains_ignore_ascii_case(content, "<user_request>");
        if unwrapped.trim().is_empty() || is_summary_only {
            return ("system".to_string(), unwrapped);
        }
    }
    let content = unwrapped.as_str();
    let trimmed_content = content.trim();
    let is_notice = trimmed_content.starts_with("[Notice] ");
    // A block in the middle of the text counts only after the harness's own preamble (or in a
    // `system` turn). An operator message that merely quotes the tag keeps its own text.
    let has_system_msg_block = trimmed_content.starts_with("<SYSTEM_MESSAGE>")
        || ((role == "system" || trimmed_content.starts_with(SYSTEM_MESSAGE_PREAMBLE))
            && content.contains("\n<SYSTEM_MESSAGE>"));
    let has_claude_peer_wrapper =
        trimmed_content.starts_with("Another Claude session sent a message:");
    let has_wire_or_operator_prefix = trimmed_content.starts_with("[Operator Prompt via FerroDeck")
        || trimmed_content.starts_with("[Incoming NATS Message")
        || trimmed_content.starts_with("[fl-wire ");
    // Harness-written turns that Claude Code records with the `user` role.
    let has_harness_opener = starts_with_system_notice_opener(trimmed_content)
        || starts_subagent_handback(trimmed_content);
    if role != "system"
        && !has_system_msg_block
        && !from_ephemeral
        && !is_notice
        && !(role == "user"
            && (has_claude_peer_wrapper || has_wire_or_operator_prefix || has_harness_opener))
    {
        return (role.to_string(), content.to_string());
    }

    let mut inner = trimmed_content;
    if let Some(rest) = inner.strip_prefix("Another Claude session sent a message:") {
        inner = rest.trim();
        if let Some(footer_idx) = inner.rfind("\n\nThis came from another Claude session") {
            inner = inner[..footer_idx].trim();
        }
    }
    if has_system_msg_block {
        let before_close = match content.rfind("</SYSTEM_MESSAGE>") {
            Some(close_idx) => &content[..close_idx],
            None => content,
        };
        if let Some(rest) = before_close.trim_start().strip_prefix("<SYSTEM_MESSAGE>") {
            inner = rest.trim();
        } else if let Some(idx) = before_close.find("\n<SYSTEM_MESSAGE>") {
            inner = before_close[idx + "\n<SYSTEM_MESSAGE>".len()..].trim();
        }
    }

    let mut msg_sender = String::new();
    if inner.starts_with("[Message]") {
        let first_line_end = inner.find('\n').unwrap_or(inner.len());
        let first_line = &inner[..first_line_end];
        let header_end = first_line.find(" content=").unwrap_or(first_line.len());
        let header = &first_line[..header_end];
        if let Some(pos) = header.match_indices("sender=").map(|(p, _)| p).find(|&p| {
            p == 0
                || header[..p]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_whitespace())
        }) {
            let after = &header[pos + "sender=".len()..];
            let raw_token = after
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or("");
            msg_sender = clean_sender_token(raw_token);
        }
        if let Some(pos) = inner[..first_line_end].find(" content=") {
            inner = inner[pos + " content=".len()..].trim();
        }
    }

    if inner.starts_with("[Operator Prompt via FerroDeck") {
        let cleaned = match inner.find('\n') {
            Some(nl) => inner[nl + 1..].trim(),
            None => inner,
        };
        return ("user".to_string(), cleaned.to_string());
    }

    if inner.starts_with("[Incoming NATS Message") {
        let mut sender = String::new();
        let mut subject = String::new();
        let mut body_lines = Vec::new();
        let mut in_headers = true;
        for (i, line) in inner.lines().enumerate() {
            if i == 0 && line.starts_with("[Incoming NATS Message") {
                continue;
            }
            if in_headers {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    in_headers = false;
                    continue;
                }
                if let Some((k, v)) = trimmed.split_once(':') {
                    let key_lower = k.trim().to_ascii_lowercase();
                    if matches!(
                        key_lower.as_str(),
                        "from" | "to" | "subject" | "timestamp" | "message id"
                    ) {
                        if key_lower == "from" {
                            sender = v.trim().to_string();
                        } else if key_lower == "subject" {
                            subject = v.trim().to_string();
                        }
                        continue;
                    }
                }
                in_headers = false;
            }
            body_lines.push(line);
        }
        let mut cleaned = body_lines.join("\n").trim().to_string();
        if cleaned.is_empty() && !subject.is_empty() {
            cleaned = subject;
        }
        if sender.eq_ignore_ascii_case("owner") || sender.eq_ignore_ascii_case("lmcfarlin") {
            return ("user".to_string(), cleaned);
        }
        if !sender.is_empty() {
            return (format!("wire:{}", sender.to_ascii_lowercase()), cleaned);
        }
        return ("system".to_string(), cleaned);
    }

    if inner.starts_with("[fl-wire ") {
        let first_line_end = inner.find('\n').unwrap_or(inner.len());
        let first_line = &inner[..first_line_end];
        let mut sender = String::new();
        if let Some(from_idx) = first_line.find("] from ") {
            let after_from = &first_line[from_idx + "] from ".len()..];
            let end_idx = after_from
                .find(" (")
                .or_else(|| after_from.find(':'))
                .unwrap_or(after_from.len());
            sender = after_from[..end_idx].trim().to_string();
        }
        let body = if let Some(blank_idx) = inner.find("\n\n") {
            inner[blank_idx + 2..].trim().to_string()
        } else if first_line_end < inner.len() {
            inner[first_line_end + 1..].trim().to_string()
        } else {
            first_line
                .split_once("): ")
                .map(|(_, subj)| subj.trim().to_string())
                .unwrap_or_else(|| inner.to_string())
        };
        if sender.eq_ignore_ascii_case("owner") || sender.eq_ignore_ascii_case("lmcfarlin") {
            return ("user".to_string(), body);
        }
        if !sender.is_empty() {
            return (format!("wire:{}", sender.to_ascii_lowercase()), body);
        }
        return ("system".to_string(), body);
    }

    // A Claude Code notice the harness wrote is a system turn, not an operator turn.
    if starts_with_system_notice_opener(inner) {
        return ("system".to_string(), inner.to_string());
    }

    // An async subagent hand-back is a wire turn from the subagent. Only `inner`, the
    // text after the peer wrapper, is tested, and only at its start.
    if starts_subagent_handback(inner) {
        return ("wire:subagent".to_string(), clean_subagent_handback(inner));
    }

    // An Antigravity `[Notice]` or `<EPHEMERAL_MESSAGE>` is a system notice, never an
    // operator message, even though the transcript labels it as a user turn.
    if from_ephemeral || inner.starts_with("[Notice] ") {
        return ("system".to_string(), inner.to_string());
    }

    if !inner.starts_with("[Operator Prompt via FerroDeck")
        && !inner.starts_with("[Incoming NATS Message")
        && !inner.starts_with("[fl-wire ")
    {
        if inner.starts_with("Task id \"") || msg_sender.contains("/task-") {
            return ("tool".to_string(), inner.to_string());
        }
        if !msg_sender.is_empty() && !msg_sender.eq_ignore_ascii_case("system") {
            return ("wire:subagent".to_string(), inner.to_string());
        }
    }

    if (has_system_msg_block || has_claude_peer_wrapper)
        && !inner.starts_with("[Incoming NATS Message")
        && !inner.starts_with("[fl-wire ")
        && !inner.starts_with("[Task ")
    {
        return ("user".to_string(), inner.to_string());
    }

    ("system".to_string(), inner.to_string())
}

fn extract_content_and_tools(
    val: &Value,
    content_parts: &mut Vec<String>,
    thinking_parts: &mut Vec<String>,
    tool_calls: &mut Vec<TranscriptToolCall>,
) {
    match val {
        Value::String(s) => {
            if !s.is_empty() {
                content_parts.push(s.clone());
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(s) if !s.is_empty() => content_parts.push(s.clone()),
                    Value::Object(map) => {
                        let item_type = map.get("type").and_then(Value::as_str).unwrap_or("");
                        match item_type {
                            "text" => {
                                if let Some(t) = map.get("text").and_then(Value::as_str)
                                    && !t.is_empty()
                                {
                                    content_parts.push(t.to_string());
                                }
                            }
                            "thinking" => {
                                if let Some(t) = map
                                    .get("thinking")
                                    .or_else(|| map.get("text"))
                                    .and_then(Value::as_str)
                                    && !t.is_empty()
                                {
                                    thinking_parts.push(t.to_string());
                                }
                            }
                            "tool_use" | "tool_call" => {
                                if let Some(tc) = parse_tool_call_item(item) {
                                    tool_calls.push(tc);
                                }
                            }
                            "tool_result" => {
                                if let Some(nested) = map.get("content") {
                                    extract_content_and_tools(
                                        nested,
                                        content_parts,
                                        thinking_parts,
                                        tool_calls,
                                    );
                                }
                            }
                            _ => {
                                if let Some(t) = map
                                    .get("text")
                                    .or_else(|| map.get("content"))
                                    .and_then(Value::as_str)
                                    && !t.is_empty()
                                {
                                    content_parts.push(t.to_string());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn parse_tool_call_item(v: &Value) -> Option<TranscriptToolCall> {
    let obj = v.as_object()?;
    let name = obj
        .get("name")
        .or_else(|| obj.get("tool_name"))
        .or_else(|| v.pointer("/function/name"))
        .and_then(Value::as_str)?
        .to_string();
    let raw_args = obj
        .get("arguments")
        .or_else(|| obj.get("input"))
        .or_else(|| obj.get("args"))
        .or_else(|| v.pointer("/function/arguments"))
        .cloned()
        .unwrap_or(Value::Null);
    let arguments = match raw_args {
        Value::String(ref s) => serde_json::from_str::<Value>(s).unwrap_or(raw_args),
        other => other,
    };
    Some(TranscriptToolCall { name, arguments })
}

fn format_unix_micros_iso8601(micros: i64) -> Option<String> {
    if micros <= 0 {
        return None;
    }
    let millis = u64::try_from(micros / 1_000).ok()?;
    Some(fl_core::At::from_unix_millis(millis).to_string())
}

fn parse_muse_event_turn(
    val: &Value,
    created_at: Option<String>,
    index: usize,
) -> Option<TranscriptTurn> {
    let pt = val.get("payload_type").and_then(Value::as_str)?;
    match pt {
        "runtime.user_intent.accepted" => {
            let msgs = val.pointer("/payload/model_messages")?.as_array()?;
            let mut parts = Vec::new();
            for m in msgs {
                if let Some(c) = m.get("content") {
                    let mut th = Vec::new();
                    let mut tc = Vec::new();
                    extract_content_and_tools(c, &mut parts, &mut th, &mut tc);
                }
            }
            let raw = parts.join("\n");
            if raw.trim().is_empty() {
                return None;
            }
            let (role, content) = normalize_system_turn("system", &raw);
            let final_role = if role == "system" {
                "user".to_string()
            } else {
                role
            };
            Some(TranscriptTurn {
                index,
                role: final_role,
                content,
                thinking: None,
                tool_calls: Vec::new(),
                created_at,
            })
        }
        "peer_message.admission.requested" => {
            let body = val.pointer("/payload/body").and_then(Value::as_str)?.trim();
            if body.is_empty() {
                return None;
            }
            let (role, content) = normalize_system_turn("system", body);
            let final_role = if role == "system" {
                "user".to_string()
            } else {
                role
            };
            Some(TranscriptTurn {
                index,
                role: final_role,
                content,
                thinking: None,
                tool_calls: Vec::new(),
                created_at,
            })
        }
        "runtime.session" => {
            if val.pointer("/payload/kind").and_then(Value::as_str) != Some("run") {
                return None;
            }
            let ev = val.pointer("/payload/event")?;
            let ev_kind = ev.get("kind").and_then(Value::as_str)?;
            match ev_kind {
                "assistant_message_committed" => {
                    let text = ev.get("text").and_then(Value::as_str)?.trim();
                    if text.is_empty()
                        || text == "[Previous turn ended without an assistant reply.]"
                    {
                        return None;
                    }
                    Some(TranscriptTurn {
                        index,
                        role: "assistant".to_string(),
                        content: text.to_string(),
                        thinking: None,
                        tool_calls: Vec::new(),
                        created_at,
                    })
                }
                "reasoning_summary_committed" => {
                    let text = ev.get("text").and_then(Value::as_str)?.trim();
                    if text.is_empty() {
                        return None;
                    }
                    Some(TranscriptTurn {
                        index,
                        role: "assistant".to_string(),
                        content: String::new(),
                        thinking: Some(text.to_string()),
                        tool_calls: Vec::new(),
                        created_at,
                    })
                }
                "assistant_tool_calls_committed" => {
                    let arr = ev.get("tool_calls").and_then(Value::as_array)?;
                    let tool_calls: Vec<TranscriptToolCall> =
                        arr.iter().filter_map(parse_tool_call_item).collect();
                    if tool_calls.is_empty() {
                        return None;
                    }
                    Some(TranscriptTurn {
                        index,
                        role: "assistant".to_string(),
                        content: String::new(),
                        thinking: None,
                        tool_calls,
                        created_at,
                    })
                }
                "tool_result_batch_committed" => {
                    let arr = ev.get("results").and_then(Value::as_array)?;
                    let mut texts = Vec::new();
                    for item in arr {
                        if let Some(t) = item.get("text").and_then(Value::as_str)
                            && !t.trim().is_empty()
                        {
                            texts.push(t.to_string());
                        }
                    }
                    if texts.is_empty() {
                        return None;
                    }
                    Some(TranscriptTurn {
                        index,
                        role: "tool".to_string(),
                        content: texts.join("\n"),
                        thinking: None,
                        tool_calls: Vec::new(),
                        created_at,
                    })
                }
                "terminal" => {
                    let terminal = ev
                        .get("terminal")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if matches!(terminal, "failed" | "cancelled" | "aborted") {
                        let reason = ev
                            .get("reason")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("Run ended without an assistant reply.");
                        Some(TranscriptTurn {
                            index,
                            role: "assistant".to_string(),
                            content: format!("⚠️ **Run {terminal}:** {reason}"),
                            thinking: None,
                            tool_calls: Vec::new(),
                            created_at,
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn is_muse_run_completed_terminal(val: &Value) -> bool {
    val.get("payload_type").and_then(Value::as_str) == Some("runtime.session")
        && val.pointer("/payload/kind").and_then(Value::as_str) == Some("run")
        && val.pointer("/payload/event/kind").and_then(Value::as_str) == Some("terminal")
        && val
            .pointer("/payload/event/terminal")
            .and_then(Value::as_str)
            == Some("completed")
}

fn slugify_worktree_path(path: &Path) -> String {
    let raw = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("worktree");
    let mut slug = String::new();
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '-' || lower == '_' {
            slug.push(lower);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "worktree".to_string()
    } else {
        slug
    }
}

/// List Git worktrees for `repo_root` via `git worktree list --porcelain` and `git status --porcelain` (spec §3.3).
pub fn list_git_worktrees(repo_root: &Path) -> Result<Vec<WorktreeSummary>, String> {
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("failed to run git worktree list: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree list failed in {}: {}",
            repo_root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut raw_entries: Vec<(PathBuf, String, Option<String>)> = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_head = String::new();
    let mut cur_branch: Option<String> = None;

    for line in stdout.lines().chain(std::iter::once("")) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(p) = cur_path.take() {
                raw_entries.push((p, std::mem::take(&mut cur_head), cur_branch.take()));
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("worktree ") {
            if let Some(p) = cur_path.take() {
                raw_entries.push((p, std::mem::take(&mut cur_head), cur_branch.take()));
            }
            cur_path = Some(PathBuf::from(rest.trim()));
        } else if let Some(rest) = trimmed.strip_prefix("HEAD ") {
            cur_head = rest.trim().to_string();
        } else if let Some(rest) = trimmed.strip_prefix("branch ") {
            let b = rest.trim();
            let short = b.strip_prefix("refs/heads/").unwrap_or(b);
            cur_branch = Some(short.to_string());
        }
    }

    let mut used_ids = BTreeSet::new();
    let mut summaries = Vec::with_capacity(raw_entries.len());

    for (wt_path, head, branch) in raw_entries {
        let base_id = slugify_worktree_path(&wt_path);
        let mut id = base_id.clone();
        let mut n = 2usize;
        while !used_ids.insert(id.clone()) {
            id = format!("{base_id}-{n}");
            n += 1;
        }

        let files = read_worktree_status_files(&wt_path);
        let dirty_files: Vec<String> = files.into_iter().map(|f| f.path).collect();
        let is_clean = dirty_files.is_empty();

        summaries.push(WorktreeSummary {
            id,
            path: wt_path.display().to_string(),
            head,
            branch,
            dirty_files,
            is_clean,
        });
    }

    Ok(summaries)
}

fn read_worktree_status_files(wt_path: &Path) -> Vec<WorktreeFileStatus> {
    if !wt_path.is_dir() {
        return Vec::new();
    }
    let Ok(out) = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(wt_path)
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut files = Vec::new();
    for line in stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        let status = line[..2].trim().to_string();
        let path = line[3..].trim().to_string();
        if !path.is_empty() {
            files.push(WorktreeFileStatus {
                status: if status.is_empty() {
                    "M".to_string()
                } else {
                    status
                },
                path,
            });
        }
    }
    files
}

/// Inspect file status (`git status --porcelain`) and unified diff (`git diff HEAD`) for a single worktree (spec §3.3).
pub fn read_worktree_diff(
    repo_root: &Path,
    worktree_id: &str,
) -> Result<Option<WorktreeDiff>, String> {
    let worktrees = list_git_worktrees(repo_root)?;
    let target = worktree_id.trim();
    let Some(wt) = worktrees.into_iter().find(|w| {
        w.id == target || Path::new(&w.path).file_name().and_then(|s| s.to_str()) == Some(target)
    }) else {
        return Ok(None);
    };

    let wt_path = PathBuf::from(&wt.path);
    let files = read_worktree_status_files(&wt_path);
    let diff = if wt_path.is_dir() {
        let out = Command::new("git")
            .args(["diff", "HEAD"])
            .current_dir(&wt_path)
            .output()
            .map_err(|e| format!("failed to run git diff HEAD: {e}"))?;
        String::from_utf8_lossy(&out.stdout).to_string()
    } else {
        String::new()
    };

    Ok(Some(WorktreeDiff {
        id: wt.id,
        path: wt.path,
        branch: wt.branch,
        head: wt.head,
        files,
        diff,
    }))
}

fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | "dist" | ".fl" | ".superpowers" | ".cargo"
    )
}

/// The paths listed under `[workspace] exclude` in `repo_root/Cargo.toml`, joined onto
/// `repo_root`. Cargo reads each entry as a plain path relative to the workspace root and
/// excludes everything beneath it; it expands no globs there, so neither does this. A missing
/// or unparsable root manifest excludes nothing.
fn workspace_exclude_paths(repo_root: &Path) -> Vec<PathBuf> {
    let Ok(raw) = fs::read_to_string(repo_root.join("Cargo.toml")) else {
        return Vec::new();
    };
    let Ok(parsed) = toml::from_str::<toml::Value>(&raw) else {
        return Vec::new();
    };
    parsed
        .get("workspace")
        .and_then(|w| w.get("exclude"))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .filter(|ex| !ex.trim().is_empty())
        .map(|ex| repo_root.join(ex))
        .collect()
}

/// Walk `repo_root` for the architecture scan (`fl arch check`, `fl arch init`, and the hub's
/// architecture API). Skips build and tool directories, and every path the root manifest's
/// `[workspace] exclude` lists, so a directory cargo does not treat as part of the workspace
/// (for example a mounted git submodule) contributes no packages, sources or files.
pub fn arch_scan_walk(repo_root: &Path) -> impl Iterator<Item = walkdir::DirEntry> {
    let excluded = workspace_exclude_paths(repo_root);
    WalkDir::new(repo_root)
        .into_iter()
        .filter_entry(move |e| {
            e.depth() == 0
                || !e.file_type().is_dir()
                || (e
                    .file_name()
                    .to_str()
                    .is_none_or(|name| !should_skip_dir(name))
                    && !excluded.iter().any(|ex| e.path() == ex))
        })
        .flatten()
}

/// Scan `repo_root` for Cargo package dependencies, Rust source crate references, and
/// repository files for architecture conformance checking (spec §1.3).
pub fn scan_workspace_for_arch(repo_root: &Path) -> Result<WorkspaceArchScan, String> {
    let mut manifest_paths = BTreeSet::new();
    let root_cargo = repo_root.join("Cargo.toml");
    if root_cargo.is_file() {
        manifest_paths.insert(root_cargo);
    }

    for entry in arch_scan_walk(repo_root) {
        let path = entry.path();
        if path.is_file() && path.file_name().and_then(|s| s.to_str()) == Some("Cargo.toml") {
            manifest_paths.insert(path.to_path_buf());
        }
    }

    let mut package_deps = Vec::new();
    let mut workspace_packages = BTreeMap::new();

    for manifest in &manifest_paths {
        let Ok(raw) = fs::read_to_string(manifest) else {
            continue;
        };
        let Ok(parsed) = toml::from_str::<toml::Value>(&raw) else {
            continue;
        };
        let Some(pkg_name) = parsed
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        let rel_manifest = manifest
            .strip_prefix(repo_root)
            .unwrap_or(manifest)
            .to_string_lossy()
            .replace('\\', "/");

        let ident = pkg_name.replace('-', "_");
        workspace_packages.insert(ident, pkg_name.to_string());

        let mut recorded_any = false;
        for section in ["dependencies", "build-dependencies"] {
            if let Some(deps_table) = parsed.get(section).and_then(toml::Value::as_table) {
                for (dep_key, dep_val) in deps_table {
                    let target_pkg = dep_val
                        .as_table()
                        .and_then(|t| t.get("package"))
                        .and_then(toml::Value::as_str)
                        .unwrap_or(dep_key.as_str());
                    package_deps.push(PackageDep {
                        from_package: pkg_name.to_string(),
                        to_package: target_pkg.to_string(),
                        manifest_path: rel_manifest.clone(),
                    });
                    recorded_any = true;
                }
            }
        }
        if !recorded_any {
            package_deps.push(PackageDep {
                from_package: pkg_name.to_string(),
                to_package: pkg_name.to_string(),
                manifest_path: rel_manifest,
            });
        }
    }

    let mut source_refs = Vec::new();
    let mut repo_files = Vec::new();
    let has_crates_or_ui = repo_root.join("crates").is_dir() || repo_root.join("ui").is_dir();

    for entry in arch_scan_walk(repo_root) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel.is_empty() || rel == "Cargo.toml" || rel == "Cargo.lock" || rel.starts_with('.') {
            continue;
        }

        if !has_crates_or_ui || rel.starts_with("crates/") || rel.starts_with("ui/") {
            repo_files.push(rel.clone());
        }

        if path.extension().and_then(|s| s.to_str()) == Some("rs")
            && let Ok(src_text) = fs::read_to_string(path)
        {
            for target_pkg in extract_rust_crate_imports(&src_text, &workspace_packages) {
                source_refs.push(SourceReference {
                    from_path: rel.clone(),
                    target_component_or_package: target_pkg,
                });
            }
        }
    }

    repo_files.sort();
    Ok(WorkspaceArchScan {
        package_deps,
        source_refs,
        repo_files,
    })
}

fn extract_rust_crate_imports(
    src_text: &str,
    workspace_packages: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in src_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.starts_with("#[cfg(test)]") {
            break;
        }
        let rest = trimmed
            .strip_prefix("pub(crate) use ")
            .or_else(|| trimmed.strip_prefix("pub use "))
            .or_else(|| trimmed.strip_prefix("use "))
            .or_else(|| trimmed.strip_prefix("extern crate "));
        let Some(after_use) = rest else {
            continue;
        };
        let root_ident: String = after_use
            .trim_start_matches("::")
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if let Some(pkg) = workspace_packages.get(&root_ident) {
            found.insert(pkg.clone());
        }
    }
    found
}

/// Scan `repo_root` and evaluate [`check_architecture_conformance`] against `model` (spec §1.3).
pub fn scan_cargo_architecture_conformance(
    repo_root: &Path,
    model: &ArchitectureModel,
) -> Result<ArchConformanceReport, String> {
    let scan = scan_workspace_for_arch(repo_root)?;
    check_architecture_conformance(
        model,
        &scan.package_deps,
        &scan.source_refs,
        &scan.repo_files,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn unstable_read_is_unknown_but_missing_transcript_is_known_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.jsonl");
        let old = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"old\"}}\n";
        fs::write(&path, old).unwrap();
        let session: DiscoveredSessionSummary = serde_json::from_value(serde_json::json!({
            "id": "example", "source": "route", "adapter": "claude", "status": "active",
            "transcript_path": path.to_str().unwrap()
        }))
        .unwrap();
        let snapshot = DiscoveryStoreSnapshot::default();
        let mut reads = TranscriptReads::default();
        let change = path.clone();
        AFTER_TRANSCRIPT_READ.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
            fs::write(change, format!("{old}{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":\"reply\"}}}}\n")).unwrap();
        })));
        assert!(
            transcript_observation_from_sessions(
                "example",
                std::slice::from_ref(&session),
                &snapshot,
                Some(tmp.path()),
                &mut reads,
            )
            .is_none()
        );
        let observed = transcript_observation_from_sessions(
            "example",
            std::slice::from_ref(&session),
            &snapshot,
            Some(tmp.path()),
            &mut reads,
        )
        .unwrap();
        assert_eq!(observed.len(), 2);
        fs::remove_file(&path).unwrap();
        assert_eq!(
            transcript_observation_from_sessions(
                "example",
                &[session],
                &snapshot,
                Some(tmp.path()),
                &mut reads,
            ),
            None,
            "a discovered transcript that disappears has an unknown observation",
        );
        assert_eq!(
            transcript_observation_from_sessions(
                "not-found",
                &[],
                &snapshot,
                Some(tmp.path()),
                &mut reads,
            ),
            Some(Vec::new())
        );
    }

    #[test]
    fn transcript_reads_refreshes_after_file_changes_but_reuses_unchanged_data() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.jsonl");
        fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"one\"}}\n",
        )
        .unwrap();
        let name = path.to_str().unwrap();
        let mut reads = TranscriptReads::default();
        let (_, cost) = cost_counters::measure(|| {
            reads.turns(name).unwrap();
            reads.turns(name).unwrap();
        });
        assert_eq!((cost.transcript_reads, cost.transcript_parses), (1, 1));
        fs::write(&path, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"different message\"}}\n").unwrap();
        let (turns, cost) = cost_counters::measure(|| reads.turns(name).unwrap().to_vec());
        assert_eq!((cost.transcript_reads, cost.transcript_parses), (1, 1));
        assert!(
            turns
                .iter()
                .any(|t| t.content.contains("different message"))
        );
    }

    #[test]
    fn transcript_cache_refreshes_when_hydration_companion_changes() {
        let tmp = TempDir::new().unwrap();
        let logs = tmp.path().join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let compact = logs.join("transcript.jsonl");
        let full = logs.join("transcript_full.jsonl");
        let row = |content: &str, truncated: bool| {
            let mut row =
                serde_json::json!({"step_index": 0, "type": "USER_INPUT", "content": content});
            if truncated {
                row["truncated_fields"] = serde_json::json!(["content"]);
            }
            format!("{row}\n")
        };
        fs::write(&compact, row("short", true)).unwrap();
        fs::write(&full, row("first full text", false)).unwrap();
        let mut cache = TranscriptReads::default();
        let path = compact.to_str().unwrap();
        assert!(
            cache.turns(path).unwrap()[0]
                .content
                .contains("first full text")
        );
        fs::write(&full, row("a longer changed full text", false)).unwrap();
        let turns = cache.turns(path).unwrap();
        assert!(turns[0].content.contains("a longer changed full text"));
    }

    #[cfg(unix)]
    fn replace_preserving_mtime(path: &Path, contents: &str) {
        use std::fs::FileTimes;
        let old = fs::metadata(path).unwrap();
        assert_eq!(old.len(), contents.len() as u64);
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, contents).unwrap();
        fs::File::open(&replacement)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old.modified().unwrap()))
            .unwrap();
        fs::rename(&replacement, path).unwrap();
        let new = fs::metadata(path).unwrap();
        assert_eq!(new.modified().unwrap(), old.modified().unwrap());
        assert_eq!(new.len(), old.len());
    }

    #[cfg(unix)]
    #[test]
    fn transcript_cache_detects_same_size_replacement_with_preserved_mtime() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.jsonl");
        let row = |s: &str| {
            format!("{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{s}\"}}}}\n")
        };
        fs::write(&path, row("first")).unwrap();
        let name = path.to_str().unwrap();
        let mut reads = TranscriptReads::default();
        assert!(reads.turns(name).unwrap()[0].content.contains("first"));
        replace_preserving_mtime(&path, &row("other"));
        let (turns, cost) = cost_counters::measure(|| reads.turns(name).unwrap().to_vec());
        assert_eq!((cost.transcript_reads, cost.transcript_parses), (1, 1));
        assert!(turns[0].content.contains("other"));
    }

    #[cfg(unix)]
    #[test]
    fn transcript_cache_detects_same_size_companion_replacement_with_preserved_mtime() {
        let tmp = TempDir::new().unwrap();
        let logs = tmp.path().join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let compact = logs.join("transcript.jsonl");
        let full = logs.join("transcript_full.jsonl");
        let row = |content: &str, truncated: bool| {
            let mut row =
                serde_json::json!({"step_index": 0, "type": "USER_INPUT", "content": content});
            if truncated {
                row["truncated_fields"] = serde_json::json!(["content"]);
            }
            format!("{row}\n")
        };
        fs::write(&compact, row("short", true)).unwrap();
        fs::write(&full, row("first", false)).unwrap();
        let mut reads = TranscriptReads::default();
        let name = compact.to_str().unwrap();
        assert!(reads.turns(name).unwrap()[0].content.contains("first"));
        replace_preserving_mtime(&full, &row("other", false));
        assert!(reads.turns(name).unwrap()[0].content.contains("other"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_transcript_read_is_not_cached_and_retries_after_permissions_recover() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.jsonl");
        fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
        let name = path.to_str().unwrap();
        let mut reads = TranscriptReads::default();
        assert!(reads.turns(name).is_none());
        assert!(
            !reads.raw.contains_key(name),
            "failed read must not become a cached result"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(reads.turns(name).unwrap()[0].content.contains("hello"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_companion_read_is_retried_after_access_recovers() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let logs = tmp.path().join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let compact = logs.join("transcript.jsonl");
        let full = logs.join("transcript_full.jsonl");
        fs::write(&compact, "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"short\",\"truncated_fields\":[\"content\"]}\n").unwrap();
        fs::write(
            &full,
            "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"complete\"}\n",
        )
        .unwrap();
        fs::set_permissions(&full, fs::Permissions::from_mode(0o0)).unwrap();
        let mut reads = TranscriptReads::default();
        let name = compact.to_str().unwrap();
        assert!(reads.turns(name).is_none());
        assert!(!reads.raw.contains_key(name));
        fs::set_permissions(&full, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(reads.turns(name).unwrap()[0].content.contains("complete"));
    }

    /// A workspace with `crates/a`, a nested `crates/a/submodules` package, a top-level
    /// `submodules/Standalone` package whose source imports `pkg_a`, and a `vendor/lib`
    /// package, under a root manifest whose `[workspace]` table ends with `extra`.
    fn arch_scan_fixture(workspace_extra: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let write = |rel: &str, body: &str| {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        };
        write(
            "Cargo.toml",
            &format!("[workspace]\nmembers = [\"crates/a\"]\n{workspace_extra}"),
        );
        write(
            "crates/a/Cargo.toml",
            "[package]\nname = \"pkg-a\"\nversion = \"0.1.0\"\n",
        );
        write(
            "crates/a/submodules/Cargo.toml",
            "[package]\nname = \"pkg-nested\"\nversion = \"0.1.0\"\n",
        );
        write(
            "submodules/Standalone/Cargo.toml",
            "[package]\nname = \"pkg-standalone\"\nversion = \"0.1.0\"\n",
        );
        write("submodules/Standalone/src/lib.rs", "use pkg_a::x;\n");
        write(
            "vendor/lib/Cargo.toml",
            "[package]\nname = \"pkg-vendor\"\nversion = \"0.1.0\"\n",
        );
        tmp
    }

    fn scanned_packages(scan: &WorkspaceArchScan) -> BTreeSet<&str> {
        scan.package_deps
            .iter()
            .map(|d| d.from_package.as_str())
            .collect()
    }

    #[test]
    fn arch_scan_skips_the_paths_the_root_manifest_excludes() {
        let tmp = arch_scan_fixture("exclude = [\"./submodules/\", \"vendor/lib\"]\n");
        let scan = scan_workspace_for_arch(tmp.path()).unwrap();
        assert_eq!(
            scanned_packages(&scan),
            BTreeSet::from(["pkg-a", "pkg-nested"])
        );
        assert!(
            scan.source_refs
                .iter()
                .all(|r| !r.from_path.starts_with("submodules/")),
            "{:?}",
            scan.source_refs
        );
    }

    #[test]
    fn arch_scan_reads_a_top_level_submodules_directory_the_manifest_does_not_exclude() {
        let tmp = arch_scan_fixture("");
        let scan = scan_workspace_for_arch(tmp.path()).unwrap();
        assert_eq!(
            scanned_packages(&scan),
            BTreeSet::from(["pkg-a", "pkg-nested", "pkg-standalone", "pkg-vendor"])
        );
        assert!(
            scan.source_refs
                .iter()
                .any(|r| r.from_path == "submodules/Standalone/src/lib.rs"),
            "{:?}",
            scan.source_refs
        );
    }

    #[test]
    fn parse_jsonl_transcript_normalizes_antigravity_claude_and_codex_turns() {
        let jsonl = r#"
{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"Check hub status","created_at":"2026-10-27T01:00:00Z"}
{"step_index":1,"type":"PLANNER_RESPONSE","source":"MODEL","content":"Running status check.","thinking":"Let me inspect the hub.","tool_calls":[{"name":"run_command","arguments":{"cmd":"fl hub status"}}],"created_at":"2026-10-27T01:00:05Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Checking files"},{"type":"text","text":"All checks passed."},{"type":"tool_use","name":"read_file","input":{"path":"Cargo.toml"}}]},"timestamp":"2026-10-27T01:00:10Z"}
"#;
        let turns = parse_jsonl_transcript(jsonl);
        assert_eq!(turns.len(), 3);

        assert_eq!(turns[0].index, 0);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].content, "Check hub status");

        assert_eq!(turns[1].index, 1);
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].content, "Running status check.");
        assert_eq!(
            turns[1].thinking.as_deref(),
            Some("Let me inspect the hub.")
        );
        assert_eq!(turns[1].tool_calls.len(), 1);
        assert_eq!(turns[1].tool_calls[0].name, "run_command");

        assert_eq!(turns[2].index, 2);
        assert_eq!(turns[2].role, "assistant");
        assert_eq!(turns[2].content, "All checks passed.");
        assert_eq!(turns[2].thinking.as_deref(), Some("Checking files"));
        assert_eq!(turns[2].tool_calls.len(), 1);
        assert_eq!(turns[2].tool_calls[0].name, "read_file");
    }

    #[test]
    fn parse_jsonl_transcript_normalizes_antigravity_system_message_prompts_and_wire() {
        let jsonl = [
            serde_json::json!({
                "step_index": 0,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "content": "The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z sender=system priority=MESSAGE_PRIORITY_HIGH content=[Operator Prompt via FerroDeck — from owner]\n\nPlease inspect the iPad layout.\n</SYSTEM_MESSAGE>",
                "created_at": "2026-10-27T15:35:06Z"
            })
            .to_string(),
            serde_json::json!({
                "step_index": 1,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "content": "The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:10Z sender=system priority=MESSAGE_PRIORITY_HIGH content=Direct prompt without prefix\n</SYSTEM_MESSAGE>",
                "created_at": "2026-10-27T15:35:10Z"
            })
            .to_string(),
            serde_json::json!({
                "step_index": 2,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "content": "The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:20Z sender=system priority=MESSAGE_PRIORITY_HIGH content=[Incoming NATS Message — pushed automatically]\nFrom: jarvis\nSubject: PR #16 review\nTimestamp: 2026-10-27T15:35:20Z\nMessage ID: msg-1\n\nApproved and ready to merge.\n</SYSTEM_MESSAGE>",
                "created_at": "2026-10-27T15:35:20Z"
            })
            .to_string(),
            serde_json::json!({
                "step_index": 3,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "content": "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:30Z sender=system priority=MESSAGE_PRIORITY_HIGH content=[fl-wire mail/normal] from jarvis (id: urn:uuid:11111111-1111-1111-1111-111111111111, thread: urn:uuid:11111111-1111-1111-1111-111111111111): Status check\n\nAll systems nominal.\n</SYSTEM_MESSAGE>",
                "created_at": "2026-10-27T15:35:30Z"
            })
            .to_string(),
        ]
        .join("\n");

        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 4);

        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].content, "Please inspect the iPad layout.");
        assert!(!turns[0].content.contains("<SYSTEM_MESSAGE>"));

        assert_eq!(turns[1].role, "user");
        assert_eq!(turns[1].content, "Direct prompt without prefix");
        assert!(!turns[1].content.contains("<SYSTEM_MESSAGE>"));

        assert_eq!(turns[2].role, "wire:jarvis");
        assert_eq!(turns[2].content, "Approved and ready to merge.");
        assert!(!turns[2].content.contains("<SYSTEM_MESSAGE>"));

        assert_eq!(turns[3].role, "wire:jarvis");
        assert_eq!(turns[3].content, "All systems nominal.");
        assert!(!turns[3].content.contains("<SYSTEM_MESSAGE>"));
    }

    #[test]
    fn normalize_system_turn_classifies_task_and_subagent_senders() {
        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z sender=abc/task-3597 priority=MESSAGE_PRIORITY_HIGH content=Task id \"x\" finished with result: done\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "tool");
        assert!(content.starts_with("Task id"));

        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z sender=11111111-1111-1111-1111-111111111111 priority=MESSAGE_PRIORITY_HIGH content=Subagent reply here\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "wire:subagent");
        assert_eq!(content, "Subagent reply here");

        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z sender=system priority=MESSAGE_PRIORITY_HIGH content=Task id \"y\" finished with result: done\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "tool");
        assert!(content.starts_with("Task id"));

        // (a) /task- sender is the only trigger; content does not start with Task id.
        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z sender=abc/task-3597 priority=MESSAGE_PRIORITY_HIGH content=Finished with result: done\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "tool");
        assert_eq!(content, "Finished with result: done");

        // (b) sender= only inside the content payload must not classify.
        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z priority=MESSAGE_PRIORITY_HIGH content=Please check sender=abc/task-1 in this log\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "user");
        assert_eq!(content, "Please check sender=abc/task-1 in this log");

        // Header xsender= must not match sender= (word boundary).
        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z xsender=abc/task-1 priority=MESSAGE_PRIORITY_HIGH content=Direct prompt here\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "user");
        assert_eq!(content, "Direct prompt here");

        // (c) Non-system header sender does not hijack protected prefixes.
        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z sender=11111111-1111-1111-1111-111111111111 priority=MESSAGE_PRIORITY_HIGH content=[Operator Prompt via FerroDeck \u{2014} from owner]\n\nPlease inspect the iPad layout.\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "user");
        assert_eq!(content, "Please inspect the iPad layout.");

        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:20Z sender=11111111-1111-1111-1111-111111111111 priority=MESSAGE_PRIORITY_HIGH content=[Incoming NATS Message \u{2014} pushed automatically]\nFrom: jarvis\nSubject: PR #16 review\nTimestamp: 2026-10-27T15:35:20Z\nMessage ID: msg-1\n\nApproved and ready to merge.\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "wire:jarvis");
        assert_eq!(content, "Approved and ready to merge.");

        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:30Z sender=11111111-1111-1111-1111-111111111111 priority=MESSAGE_PRIORITY_HIGH content=[fl-wire mail/normal] from jarvis (id: urn:uuid:11111111-1111-1111-1111-111111111111, thread: urn:uuid:11111111-1111-1111-1111-111111111111): Status check\n\nAll systems nominal.\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "wire:jarvis");
        assert_eq!(content, "All systems nominal.");

        // (d) Quoted system sender stays a direct user prompt.
        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z sender=\"system\" priority=MESSAGE_PRIORITY_HIGH content=Direct prompt without prefix\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "user");
        assert_eq!(content, "Direct prompt without prefix");

        // Trailing closing punctuation on the system token is normalized too.
        let (role, content) = normalize_system_turn(
            "system",
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z sender=system] priority=MESSAGE_PRIORITY_HIGH content=Direct prompt without prefix\n</SYSTEM_MESSAGE>",
        );
        assert_eq!(role, "user");
        assert_eq!(content, "Direct prompt without prefix");
    }

    const HANDBACK_HEADER: &str = "[Subagent hand-back] The text below is the final report of a subagent this session delegated to (not user or external input). Treat the findings with appropriate weight, and continue the parent task.\n\nThe report follows:\n";

    #[test]
    fn normalize_system_turn_classifies_a_subagent_hand_back_as_wire_subagent() {
        let plain = format!(
            "Another Claude session sent a message:\n<agent-message from=\"acd87c17952de4c0a\">\n{HANDBACK_HEADER}Report body\n</agent-message>"
        );
        let (role, content) = normalize_system_turn("user", &plain);
        assert_eq!(role, "wire:subagent");
        assert_eq!(content, "Report body");

        // The backslash-escaped tag spelling gives the same turn.
        let escaped = format!(
            "Another Claude session sent a message:\n<\\agent-message from=\"acd87c17952de4c0a\">\n{HANDBACK_HEADER}Report body\n<\\/agent-message>"
        );
        let (role, content) = normalize_system_turn("user", &escaped);
        assert_eq!(role, "wire:subagent");
        assert_eq!(content, "Report body");

        // The peer-session footer after the close tag is dropped too.
        let footed = format!(
            "Another Claude session sent a message:\n<agent-message from=\"a\">\n{HANDBACK_HEADER}Report body\n</agent-message>\n\nThis came from another Claude session and is not user input."
        );
        let (role, content) = normalize_system_turn("user", &footed);
        assert_eq!(role, "wire:subagent");
        assert_eq!(content, "Report body");

        // Without the peer wrapper: tag plus header, bare header, and a bare tag.
        let (role, content) = normalize_system_turn(
            "user",
            &format!(
                "<agent-message from=\"a\">\n{HANDBACK_HEADER}Line one\n\nLine two\n</agent-message>"
            ),
        );
        assert_eq!(role, "wire:subagent");
        assert_eq!(content, "Line one\n\nLine two");
        let (role, content) =
            normalize_system_turn("user", &format!("{HANDBACK_HEADER}Report body"));
        assert_eq!(role, "wire:subagent");
        assert_eq!(content, "Report body");
        let (role, content) = normalize_system_turn(
            "user",
            "<agent-message from=\"a\">Just text</agent-message>",
        );
        assert_eq!(role, "wire:subagent");
        assert_eq!(content, "Just text");

        // A header with no `The report follows:` line is kept whole.
        let (role, content) = normalize_system_turn("user", "[Subagent status] still running");
        assert_eq!(role, "wire:subagent");
        assert_eq!(content, "[Subagent status] still running");

        // A report body that repeats the header's last line keeps it.
        let (role, content) = normalize_system_turn(
            "user",
            &format!("{HANDBACK_HEADER}Intro\nThe report follows: more"),
        );
        assert_eq!(role, "wire:subagent");
        assert_eq!(content, "Intro\nThe report follows: more");
    }

    #[test]
    fn normalize_system_turn_keeps_user_text_that_only_mentions_a_hand_back() {
        for text in [
            "Can you explain what the [Subagent hand-back] marker in the transcript means when the parent task continues?",
            "Another Claude session sent a message:\nPlease note the [Subagent hand-back] text in the log above.",
            "Quoting the tool: <agent-message from=\"a\"> was in the output.",
            "<agent-messages> is not the tag",
        ] {
            let (role, content) = normalize_system_turn("user", text);
            assert_eq!(role, "user", "{text}");
            assert!(!content.is_empty());
        }
        // The peer-wrapped ordinary message loses only its wrapper.
        let (role, content) = normalize_system_turn(
            "user",
            "Another Claude session sent a message:\nPlease note the [Subagent hand-back] text in the log above.",
        );
        assert_eq!(role, "user");
        assert_eq!(
            content,
            "Please note the [Subagent hand-back] text in the log above."
        );
    }

    #[test]
    fn normalize_system_turn_classifies_harness_notices_on_user_turns_as_system() {
        for opener in [
            "<task-notification>",
            "<local-command-caveat>",
            "<local-command-stdout>",
            "[SYSTEM NOTIFICATION - NOT USER INPUT]",
        ] {
            let text = format!("{opener} something happened");
            let (role, content) = normalize_system_turn("user", &text);
            assert_eq!(role, "system", "{opener}");
            assert_eq!(content, text);
            // Leading whitespace is ignored, and a system-role turn stays system.
            let (role, _) = normalize_system_turn("user", &format!("\n  {text}"));
            assert_eq!(role, "system", "{opener}");
            let (role, _) = normalize_system_turn("system", &text);
            assert_eq!(role, "system", "{opener}");
        }
        // The opener must start the turn.
        let (role, _) = normalize_system_turn("user", "see <task-notification> in the log");
        assert_eq!(role, "user");
    }

    #[test]
    fn strip_agy_user_request_wrapper_mirrors_the_ui_twin() {
        let raw = "<USER_REQUEST>\nPlease continue\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-10-29T18:44:28Z.\n</ADDITIONAL_METADATA>";
        assert_eq!(strip_agy_user_request_wrapper(raw), "Please continue");

        // Metadata first, mixed case tags, and blank-line runs collapse to one blank line.
        let raw = "<additional_metadata>\nx\n</additional_metadata>\n\n\n<User_Request>\n a\n\n\n\nb \n</User_Request>";
        assert_eq!(strip_agy_user_request_wrapper(raw), "a\n\nb");

        let raw = "<EPHEMERAL_MESSAGE>\n[Notice] stopped\n</EPHEMERAL_MESSAGE>";
        assert_eq!(strip_agy_user_request_wrapper(raw), "[Notice] stopped");

        // A tag that is not at the start is quoted text and stays intact.
        let quoted = "Here is a snippet:\n<USER_REQUEST>\nhello\n</USER_REQUEST>";
        assert_eq!(strip_agy_user_request_wrapper(quoted), quoted);
        assert_eq!(strip_agy_user_request_wrapper(""), "");

        // An unclosed tag is left alone rather than swallowing the rest.
        assert_eq!(
            strip_agy_user_request_wrapper("<USER_REQUEST>\nno close"),
            "<USER_REQUEST>\nno close"
        );
    }

    fn grok_user_line(text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "content": [{"type": "text", "text": text}]
        })
        .to_string()
    }

    const GROK_USER_INFO: &str = "<user_info>\nOS Version: linux\nWorkspace: /tmp/x\n</user_info>";
    const GROK_RULES: &str =
        "<rules>\n<user_rules description=\"r\">\nBe kind.\n</user_rules>\n</rules>";
    const GROK_SKILLS: &str = "<system-reminder>\nThe following skills are available for use with the Skill tool:\n- a\n</system-reminder>";
    const GROK_MCP: &str = "<system-reminder>\nMCP servers connected:\n- fl\n</system-reminder>";

    fn grok_roles_and_content(texts: &[&str]) -> Vec<(String, String)> {
        let jsonl = texts
            .iter()
            .map(|t| grok_user_line(t))
            .collect::<Vec<_>>()
            .join("\n");
        parse_jsonl_transcript(&jsonl)
            .into_iter()
            .map(|t| (t.role, t.content))
            .collect()
    }

    #[test]
    fn grok_scaffolding_rows_are_system_never_operator() {
        let preamble = format!("{GROK_USER_INFO}\n\n{GROK_RULES}");
        // The preamble ends with `</rules>`, not `</user_info>`.
        for text in [
            preamble.as_str(),
            GROK_SKILLS,
            GROK_MCP,
            GROK_RULES,
            GROK_USER_INFO,
        ] {
            let turns = grok_roles_and_content(&[text]);
            assert!(
                turns.iter().all(|(role, _)| role != "user"),
                "scaffolding became an operator turn: {turns:?}"
            );
        }
        let turns = grok_roles_and_content(&[&preamble]);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].0, "system");
        // A turn opening with `<user_rules` alone is scaffolding too.
        let turns = grok_roles_and_content(&["<user_rules>\nBe kind.\n</user_rules>"]);
        assert_eq!(turns[0].0, "system");
        // The leading-whitespace case.
        let turns = grok_roles_and_content(&[&format!("\n  {GROK_MCP}\n")]);
        assert_eq!(turns[0].0, "system");
    }

    #[test]
    fn grok_user_query_is_unwrapped_and_surrounding_scaffolding_removed() {
        let query = "<user_query>\nFix the bug in foo.rs\n</user_query>";
        let turns = grok_roles_and_content(&[query]);
        assert_eq!(
            turns,
            vec![("user".to_string(), "Fix the bug in foo.rs".to_string())]
        );

        // Scaffolding and the prompt in one message.
        let mixed =
            format!("{GROK_USER_INFO}\n\n{GROK_RULES}\n\n{GROK_SKILLS}\n\n{query}\n\n{GROK_MCP}");
        let turns = grok_roles_and_content(&[&mixed]);
        assert_eq!(
            turns,
            vec![("user".to_string(), "Fix the bug in foo.rs".to_string())]
        );

        // The last block wins when several are present.
        let two = format!(
            "{GROK_SKILLS}\n<user_query>\nold\n</user_query>\n<user_query>\nnew\n</user_query>"
        );
        assert_eq!(strip_grok_user_query_wrapper(&two), "new");

        // A truncated block keeps the rest of the text.
        let cut = format!("{GROK_SKILLS}\n<user_query>\nhalf a prompt");
        assert_eq!(strip_grok_user_query_wrapper(&cut), "half a prompt");
        assert_eq!(
            strip_grok_user_query_wrapper("<user_query>\nhalf a prompt"),
            "half a prompt"
        );
        let turns = grok_roles_and_content(&[&cut]);
        assert_eq!(
            turns,
            vec![("user".to_string(), "half a prompt".to_string())]
        );

        // A mid-run notice before the query is dropped too.
        let notice =
            "The user sent a message while you were working:\n<user_query>\nAlso x\n</user_query>";
        assert_eq!(strip_grok_user_query_wrapper(notice), "Also x");
    }

    #[test]
    fn grok_plain_prompts_and_mentions_of_tags_are_untouched() {
        for text in [
            "Please fix the build",
            "Please look at the <rules> section of the doc",
            "Why does <system-reminder> show up in my logs?",
            "see <user_info> in the file",
        ] {
            let turns = grok_roles_and_content(&[text]);
            assert_eq!(
                turns,
                vec![("user".to_string(), text.to_string())],
                "{text}"
            );
            assert_eq!(strip_grok_user_query_wrapper(text), text);
        }
    }

    #[test]
    fn grok_full_four_row_turn_yields_one_clean_operator_turn() {
        let preamble = format!("{GROK_USER_INFO}\n\n{GROK_RULES}");
        let query = "<user_query>\nWhat changed today?\n</user_query>";
        let turns = grok_roles_and_content(&[&preamble, GROK_SKILLS, query, GROK_MCP]);
        let users: Vec<_> = turns.iter().filter(|(r, _)| r == "user").collect();
        assert_eq!(users.len(), 1, "{turns:?}");
        assert_eq!(users[0].1, "What changed today?");
        assert!(
            turns
                .iter()
                .filter(|(r, _)| r != "user")
                .all(|(r, _)| r == "system"),
            "{turns:?}"
        );
    }

    #[test]
    fn agy_settings_change_and_context_summary_are_stripped_and_system() {
        let settings = "<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from A to B\n</USER_SETTINGS_CHANGE>";
        let meta = "<ADDITIONAL_METADATA>\ntime\n</ADDITIONAL_METADATA>";

        // A real request with trailing metadata + settings change renders just the request.
        let raw = format!("<USER_REQUEST>\nPlease continue\n</USER_REQUEST>\n{meta}\n{settings}");
        assert_eq!(strip_agy_user_request_wrapper(&raw), "Please continue");
        let (role, content) = normalize_system_turn("user", &raw);
        assert_eq!(
            (role.as_str(), content.as_str()),
            ("user", "Please continue")
        );

        // Case-insensitive, as the other wrappers are.
        let lower = "<user_request>\nhi\n</user_request>\n<user_settings_change>\nx\n</user_settings_change>";
        assert_eq!(strip_agy_user_request_wrapper(lower), "hi");

        // Metadata plus settings change and nothing else: system, empty.
        let raw = format!("{meta}\n{settings}");
        let (role, content) = normalize_system_turn("user", &raw);
        assert_eq!((role.as_str(), content.as_str()), ("system", ""));
        let (role, _) = normalize_system_turn("user", settings);
        assert_eq!(role, "system");

        // A compaction summary is a system turn, even when it quotes a request tag after
        // the summary closed.
        let summary = "<CONTEXT_SUMMARY>\nWe did things.\n</CONTEXT_SUMMARY>";
        let (role, content) = normalize_system_turn("user", summary);
        assert_eq!((role.as_str(), content.as_str()), ("system", ""));
        let (role, _) =
            normalize_system_turn("user", "<context_summary>\nlower\n</context_summary>");
        assert_eq!(role, "system");
        // A very large turn opening with a mixed-case summary tag is still system.
        let big = format!("<Context_Summary>\n{}", "word ".repeat(400_000));
        let (role, _) = normalize_system_turn("user", &big);
        assert_eq!(role, "system");
        // An unclosed summary is still a system turn.
        let (role, _) = normalize_system_turn("user", "<CONTEXT_SUMMARY>\ncut off, no close");
        assert_eq!(role, "system");
        // A summary followed by a real request keeps the request.
        let both = format!("{summary}\n<USER_REQUEST>\nNext step\n</USER_REQUEST>");
        let (role, content) = normalize_system_turn("user", &both);
        assert_eq!((role.as_str(), content.as_str()), ("user", "Next step"));
        // A summary quoting a request tag does not leak it.
        let quoting =
            "<CONTEXT_SUMMARY>\nUser asked <USER_REQUEST>x</USER_REQUEST>\n</CONTEXT_SUMMARY>";
        let (role, content) = normalize_system_turn("user", quoting);
        assert_eq!((role.as_str(), content.as_str()), ("system", ""));

        // Quoted tags mid-text stay operator text.
        let quoted = "Please explain <CONTEXT_SUMMARY> to me";
        let (role, content) = normalize_system_turn("user", quoted);
        assert_eq!((role.as_str(), content.as_str()), ("user", quoted));

        // End to end: the settings-change turn never renders as an Operator turn.
        let jsonl = [
            serde_json::json!({"step_index": 0, "type": "USER_INPUT", "source": "USER_EXPLICIT",
                "content": format!("<USER_REQUEST>\nGo\n</USER_REQUEST>\n{meta}\n{settings}")}),
            serde_json::json!({"step_index": 1, "type": "USER_INPUT", "source": "USER_EXPLICIT",
                "content": format!("{meta}\n{settings}")}),
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        let turns = parse_jsonl_transcript(&jsonl);
        let users: Vec<_> = turns.iter().filter(|t| t.role == "user").collect();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].content, "Go");
    }

    #[test]
    fn parse_jsonl_transcript_strips_agy_wrapper_and_classifies_notices_as_system() {
        let jsonl = [
            serde_json::json!({
                "step_index": 0, "type": "USER_INPUT", "source": "USER_EXPLICIT",
                "content": "<USER_REQUEST>\nPlease continue\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-10-29T18:44:28Z.\n</ADDITIONAL_METADATA>"
            }),
            serde_json::json!({
                "step_index": 1, "type": "USER_INPUT", "source": "USER_EXPLICIT",
                "content": "<USER_REQUEST>\n[Notice] All your subagents and background tasks have been stopped.\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nlocal time\n</ADDITIONAL_METADATA>"
            }),
            serde_json::json!({
                "step_index": 2, "type": "USER_INPUT", "source": "USER_EXPLICIT",
                "content": "<EPHEMERAL_MESSAGE>\nThe user rewound the conversation.\n</EPHEMERAL_MESSAGE>"
            }),
            serde_json::json!({
                "step_index": 3, "type": "SYSTEM_MESSAGE", "source": "SYSTEM",
                "content": "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-27T15:35:06Z sender=system priority=MESSAGE_PRIORITY_HIGH content=[Notice] Background task finished\n</SYSTEM_MESSAGE>"
            }),
            serde_json::json!({
                "step_index": 4, "type": "USER_INPUT", "source": "USER_EXPLICIT",
                "content": "Another Claude session sent a message:\nhello from a peer\n\nThis came from another Claude session"
            }),
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 5);
        assert_eq!(
            (turns[0].role.as_str(), turns[0].content.as_str()),
            ("user", "Please continue")
        );
        assert_eq!(turns[1].role, "system");
        assert_eq!(
            turns[1].content,
            "[Notice] All your subagents and background tasks have been stopped."
        );
        assert_eq!(turns[2].role, "system");
        assert_eq!(turns[2].content, "The user rewound the conversation.");
        assert_eq!(turns[3].role, "system");
        assert_eq!(turns[3].content, "[Notice] Background task finished");
        assert_eq!(turns[4].role, "user");
        assert_eq!(turns[4].content, "hello from a peer");
    }

    // ----- live model discovery -------------------------------------------------

    const AGY_CONFIG_FIXTURE: &str = r#"{
        "clientModelConfigs": [
            {"label":"Gemini 4 Argon (High)","modelId":"gemini-4-argon-high","modelOrAlias":{"model":"MODEL_PLACEHOLDER_M417"}},
            {"label":"Gemini 4 Argon (Low)","modelId":"gemini-4-argon-low","modelOrAlias":{"model":"MODEL_PLACEHOLDER_M416"}},
            {"label":"Gemini 3.8 Flash","modelId":"gemini-3.8-flash-medium","modelOrAlias":{"model":"MODEL_PLACEHOLDER_M420"}},
            {"label":"Claude Opus 5.5","modelId":"claude-opus-5-5-low","modelOrAlias":{"model":"MODEL_PLACEHOLDER_M430"}},
            {"label":"GPT-OSS","modelId":"gpt-oss-120b-medium","modelOrAlias":{"model":"MODEL_PLACEHOLDER_M440"}},
            {"label":"Bare","modelId":"bare-model","modelOrAlias":{"model":"MODEL_PLACEHOLDER_M450"}},
            {"label":"No id","modelOrAlias":{"model":"MODEL_PLACEHOLDER_M999"}}
        ],
        "defaultOverrideModelConfig": {"modelOrAlias":{"model":"MODEL_PLACEHOLDER_M420"}}
    }"#;

    const AGY_TRAJECTORY_FIXTURE: &str = r#"{"trajectory":{"steps":[
        {"metadata":{"generatorModel":"MODEL_PLACEHOLDER_M416"}},
        {"metadata":{}},
        {"metadata":{"generatorModel":"MODEL_PLACEHOLDER_M417"}},
        {"type":"USER_INPUT"}
    ]}}"#;

    fn write_routes(home: &Path, entries: &[(&str, &str)]) {
        let body: Vec<String> = entries
            .iter()
            .map(|(persona, route)| {
                format!(
                    r#""{persona}":{{"persona":"{persona}","updated_at":"2026-10-29T00:00:00.000Z","route":{route}}}"#
                )
            })
            .collect();
        let path = home.join(".cache").join("agent-bus").join("routes.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!(r#"{{"entries":{{{}}}}}"#, body.join(","))).unwrap();
    }

    fn discover_in(home: &Path) -> Vec<DiscoveredSessionSummary> {
        let store = fl_store::open_redb(&home.join("store.redb")).unwrap();
        discover_sessions_with_home(&store, Some(home))
    }

    fn session_for<'a>(
        sessions: &'a [DiscoveredSessionSummary],
        persona: &str,
    ) -> &'a DiscoveredSessionSummary {
        sessions
            .iter()
            .find(|s| s.persona.as_deref() == Some(persona))
            .unwrap_or_else(|| panic!("no session for {persona}"))
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn agy_thinking_suffix_splits_only_known_levels() {
        assert_eq!(
            split_agy_thinking_suffix("gemini-4-argon-high"),
            ("gemini-4-argon".to_string(), Some("high".to_string()))
        );
        assert_eq!(
            split_agy_thinking_suffix("gemini-3.8-flash-medium"),
            ("gemini-3.8-flash".to_string(), Some("medium".to_string()))
        );
        assert_eq!(
            split_agy_thinking_suffix("claude-opus-5-5-low"),
            ("claude-opus-5-5".to_string(), Some("low".to_string()))
        );
        assert_eq!(
            split_agy_thinking_suffix("bare-model"),
            ("bare-model".to_string(), None)
        );
        assert_eq!(
            split_agy_thinking_suffix("-high"),
            ("-high".to_string(), None)
        );
    }

    #[test]
    fn agy_config_and_trajectory_fixtures_resolve_the_live_model() {
        let config = parse_agy_model_config(AGY_CONFIG_FIXTURE).expect("config parses");
        assert_eq!(
            config.models.len(),
            6,
            "an entry with no modelId is skipped"
        );
        let placeholder = parse_agy_generator_model(AGY_TRAJECTORY_FIXTURE);
        assert_eq!(placeholder.as_deref(), Some("MODEL_PLACEHOLDER_M417"));

        let info = resolve_agy_model_info(&config, placeholder.as_deref());
        assert_eq!(info.model.as_deref(), Some("gemini-4-argon"));
        assert_eq!(info.thinking_level.as_deref(), Some("high"));
        assert_eq!(
            info.available_models,
            strings(&[
                "gemini-4-argon",
                "gemini-3.8-flash",
                "claude-opus-5-5",
                "gpt-oss-120b",
                "bare-model"
            ]),
            "base ids are deduplicated in config order"
        );

        // No trajectory model falls back to the default override.
        let info = resolve_agy_model_info(&config, None);
        assert_eq!(info.model.as_deref(), Some("gemini-3.8-flash"));
        assert_eq!(info.thinking_level.as_deref(), Some("medium"));
        // An unknown placeholder falls back too.
        let info = resolve_agy_model_info(&config, Some("MODEL_PLACEHOLDER_M000"));
        assert_eq!(info.model.as_deref(), Some("gemini-3.8-flash"));
        // A model with no thinking suffix keeps the whole id.
        let info = resolve_agy_model_info(&config, Some("MODEL_PLACEHOLDER_M450"));
        assert_eq!(info.model.as_deref(), Some("bare-model"));
        assert_eq!(info.thinking_level, None);
    }

    #[test]
    fn agy_parsers_reject_malformed_bodies_and_read_truncated_trajectories() {
        assert!(parse_agy_model_config("not json").is_none());
        assert!(parse_agy_model_config(r#"{"clientModelConfigs":[]}"#).is_none());
        assert!(parse_agy_model_config(r#"{"other":1}"#).is_none());
        assert!(
            parse_agy_generator_model(r#"{"trajectory":{"steps":[{"metadata":{}}]}}"#).is_none()
        );
        assert!(parse_agy_generator_model(r#"{"trajectory":{}}"#).is_none());
        // A body cut off by the size cap is invalid JSON; the last generatorModel that arrived wins.
        let truncated = r#"{"trajectory":{"steps":[{"metadata":{"generatorModel":"MODEL_PLACEHOLDER_M416"}},{"metadata":{"generatorModel": "MODEL_PLACEHOLDER_M417"}},{"metadata":{"generatorMo"#;
        assert_eq!(
            parse_agy_generator_model(truncated).as_deref(),
            Some("MODEL_PLACEHOLDER_M417")
        );
        assert!(parse_agy_generator_model("garbage").is_none());
    }

    #[test]
    fn agy_without_a_reachable_language_server_lists_the_default_models() {
        let offline = offline_agy_model_info();
        assert_eq!(offline.model, None);
        assert_eq!(offline.thinking_level, None);
        assert_eq!(offline.available_models, strings(&AGY_DEFAULT_MODELS));
        // No address, no csrf token, a dead port, and an unusable token all stay offline.
        assert_eq!(agy_live_model_info(None, None, "c1", false), offline);
        assert_eq!(
            agy_live_model_info(Some("127.0.0.1:1"), Some("tok"), "c1", false),
            offline
        );
        assert_eq!(
            agy_live_model_info(Some("127.0.0.1:1"), Some("bad token"), "c1", false),
            offline
        );
        assert_eq!(agy_live_model_info(None, Some("tok"), "c1", false), offline);

        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        write_routes(
            home,
            &[(
                "ferris",
                r#"{"kind":"agy","conversation_id":"agy-offline-1"}"#,
            )],
        );
        let sessions = discover_in(home);
        let agy = session_for(&sessions, "ferris");
        assert_eq!(agy.adapter, "antigravity");
        assert_eq!(agy.model, None);
        assert_eq!(agy.thinking_level, None);
        assert_eq!(agy.available_models, strings(&AGY_DEFAULT_MODELS));
    }

    /// Serve canned Antigravity language-server replies on a loopback port. Records each
    /// `(path, csrf header)` it sees. Stops when `stop` is set.
    struct AgyStub {
        port: u16,
        seen: std::sync::Arc<Mutex<Vec<(String, String)>>>,
        /// `(path, request body)` for each request, in arrival order.
        bodies: std::sync::Arc<Mutex<Vec<(String, String)>>>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    }

    impl AgyStub {
        fn shutdown(self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            self.handle.join().unwrap();
        }
    }

    fn spawn_agy_stub(config_body: &'static str, trajectory_body: &'static str) -> AgyStub {
        spawn_agy_stub_with_steps(config_body, trajectory_body, "agy-live-1", None)
    }

    /// Like [`spawn_agy_stub`], and `steps` is the `(status, body)` it answers
    /// `GetCascadeTrajectorySteps` with. `None` answers an empty object. Every cascade request
    /// must name `cascade_id`.
    fn spawn_agy_stub_with_steps(
        config_body: &'static str,
        trajectory_body: &'static str,
        cascade_id: &'static str,
        steps: Option<(u16, &'static str)>,
    ) -> AgyStub {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        // The kernel may reuse a port within the cache TTL; drop answers cached for an
        // earlier stub on it so this test sees every request it expects.
        forget_agy_port_caches(port);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen_t, bodies_t, stop_t) =
            (Arc::clone(&seen), Arc::clone(&bodies), Arc::clone(&stop));
        let handle = std::thread::spawn(move || {
            while !stop_t.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                if request_line.is_empty() {
                    // The listening probe connects and hangs up without a request.
                    continue;
                }
                let (mut csrf, mut len) = (String::new(), 0usize);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("x-codeium-csrf-token") {
                            csrf = value.trim().to_string();
                        } else if name.eq_ignore_ascii_case("content-length") {
                            len = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut req_body = vec![0u8; len];
                reader.read_exact(&mut req_body).unwrap();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let mut status = 200u16;
                let reply = if path.ends_with("/GetCascadeModelConfigData") {
                    config_body
                } else if path.ends_with("/GetCascadeTrajectory") {
                    assert!(
                        String::from_utf8_lossy(&req_body).contains(cascade_id),
                        "trajectory request names the cascade id"
                    );
                    trajectory_body
                } else if path.ends_with("/GetCascadeTrajectorySteps") {
                    assert!(
                        String::from_utf8_lossy(&req_body).contains(cascade_id),
                        "steps request names the cascade id"
                    );
                    match steps {
                        Some((code, body)) => {
                            status = code;
                            body
                        }
                        None => "{}",
                    }
                } else {
                    "{}"
                };
                bodies_t.lock().unwrap().push((
                    path.clone(),
                    String::from_utf8_lossy(&req_body).into_owned(),
                ));
                seen_t.lock().unwrap().push((path, csrf));
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        AgyStub {
            port,
            seen,
            bodies,
            stop,
            handle,
        }
    }

    #[test]
    fn agy_live_model_info_queries_a_local_stub_server_and_caches_the_answer() {
        let stub = spawn_agy_stub(AGY_CONFIG_FIXTURE, AGY_TRAJECTORY_FIXTURE);
        let seen = std::sync::Arc::clone(&stub.seen);
        let addr = format!("127.0.0.1:{}", stub.port);

        let info = agy_live_model_info(Some(&addr), Some("stub-csrf"), "agy-live-1", false);
        assert_eq!(info.model.as_deref(), Some("gemini-4-argon"));
        assert_eq!(info.thinking_level.as_deref(), Some("high"));
        assert_eq!(
            info.available_models.first().map(String::as_str),
            Some("gemini-4-argon")
        );
        let calls = seen.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            2,
            "one config call and one trajectory call: {calls:?}"
        );
        assert!(calls.iter().all(|(_, csrf)| csrf == "stub-csrf"));
        assert!(
            calls
                .iter()
                .any(|(p, _)| p.ends_with("/GetCascadeModelConfigData"))
        );
        assert!(
            calls
                .iter()
                .any(|(p, _)| p.ends_with("/GetCascadeTrajectory"))
        );

        // The second poll is served from the cache: no new request reaches the server.
        let again = agy_live_model_info(Some(&addr), Some("stub-csrf"), "agy-live-1", false);
        assert_eq!(again, info);
        assert_eq!(seen.lock().unwrap().len(), 2);

        // A full discovery pass through a route that names the stub resolves the same model.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        write_routes(
            home,
            &[(
                "ferris",
                &format!(
                    r#"{{"kind":"agy","conversation_id":"agy-live-1","ls_address":"{addr}","csrf_token":"stub-csrf"}}"#
                ),
            )],
        );
        let sessions = discover_in(home);
        let agy = session_for(&sessions, "ferris");
        assert_eq!(agy.model.as_deref(), Some("gemini-4-argon"));
        assert_eq!(agy.thinking_level.as_deref(), Some("high"));
        // Discovery reused the model cache. It also asked for the account quota, once per call.
        let calls = seen.lock().unwrap().clone();
        let quota_calls = |method: &str| calls.iter().filter(|(p, _)| p.ends_with(method)).count();
        assert_eq!(calls.len(), 4, "discovery reused the cache: {calls:?}");
        assert_eq!(quota_calls("/RetrieveUserQuotaSummary"), 1);
        assert_eq!(quota_calls("/GetUserStatus"), 1);

        stub.shutdown();
    }

    const AGY_STEPS_FIXTURE: &str = r#"{"steps":[
        {"metadata":{"generatorModel":"MODEL_PLACEHOLDER_M416"}},
        {"metadata":{}},
        {"metadata":{"generatorModel":"MODEL_PLACEHOLDER_M417"}},
        {"type":"USER_INPUT"}
    ]}"#;

    #[test]
    fn agy_steps_responses_parse_under_steps_and_trajectory_steps() {
        assert_eq!(
            parse_agy_generator_model(AGY_STEPS_FIXTURE).as_deref(),
            Some("MODEL_PLACEHOLDER_M417")
        );
        assert_eq!(
            parse_agy_generator_model(
                r#"{"trajectory":{"steps":[{"metadata":{"generatorModel":"MODEL_PLACEHOLDER_M420"}}]}}"#
            )
            .as_deref(),
            Some("MODEL_PLACEHOLDER_M420")
        );
        assert!(parse_agy_generator_model(r#"{"steps":[{"metadata":{}}]}"#).is_none());
        assert!(parse_agy_generator_model(r#"{"steps":[]}"#).is_none());
        assert!(parse_agy_generator_model("{}").is_none());
    }

    fn write_agy_transcript(dir: &Path, body: &str) -> String {
        let path = dir.join("transcript.jsonl");
        fs::write(&path, body).unwrap();
        path.display().to_string()
    }

    #[test]
    fn agy_step_offset_comes_from_the_last_transcript_line() {
        let tmp = TempDir::new().unwrap();
        let line = |n: u64| format!("{{\"step_index\":{n},\"type\":\"PLANNER_RESPONSE\"}}\n");

        // The last line decides, minus the margin.
        let path = write_agy_transcript(
            tmp.path(),
            &format!("{}{}{}", line(1), line(40), line(6768)),
        );
        assert_eq!(agy_tail_step_offset(Some(&path)), 6758);
        // A young conversation saturates to 0, and so does one at exactly the margin.
        let path = write_agy_transcript(tmp.path(), &format!("{}{}", line(0), line(4)));
        assert_eq!(agy_tail_step_offset(Some(&path)), 0);
        let path = write_agy_transcript(tmp.path(), &line(10));
        assert_eq!(agy_tail_step_offset(Some(&path)), 0);
        let path = write_agy_transcript(tmp.path(), &line(11));
        assert_eq!(agy_tail_step_offset(Some(&path)), 1);
        // No trailing newline, and a half-written last line is skipped for the one before it.
        let path = write_agy_transcript(tmp.path(), line(200).trim_end());
        assert_eq!(agy_tail_step_offset(Some(&path)), 190);
        let path = write_agy_transcript(
            tmp.path(),
            &format!("{}{{\"step_index\":9999,\"ty", line(300)),
        );
        assert_eq!(agy_tail_step_offset(Some(&path)), 290);
        // Only the newest bytes are read: a first line cut by the tail window is not used.
        let big = "x".repeat(AGY_TRANSCRIPT_TAIL_BYTES as usize);
        let path = write_agy_transcript(
            tmp.path(),
            &format!("{{\"step_index\":5000,\"pad\":\"{big}\"}}\n{}", line(70)),
        );
        assert_eq!(agy_tail_step_offset(Some(&path)), 60);

        // Missing, unreadable, garbage, or index-less transcripts all give 0.
        assert_eq!(agy_tail_step_offset(None), 0);
        assert_eq!(agy_tail_step_offset(Some("/definitely/missing.jsonl")), 0);
        assert_eq!(agy_tail_step_offset(tmp.path().to_str()), 0);
        let path = write_agy_transcript(tmp.path(), "not json\n{broken\n");
        assert_eq!(agy_tail_step_offset(Some(&path)), 0);
        let path = write_agy_transcript(tmp.path(), "{\"type\":\"USER_INPUT\"}\n");
        assert_eq!(agy_tail_step_offset(Some(&path)), 0);
        let path =
            write_agy_transcript(tmp.path(), "{\"step_index\":\"7\"}\n{\"step_index\":-5}\n");
        assert_eq!(agy_tail_step_offset(Some(&path)), 0);
        let path = write_agy_transcript(tmp.path(), "");
        assert_eq!(agy_tail_step_offset(Some(&path)), 0);
    }

    fn stub_paths(stub: &AgyStub) -> Vec<String> {
        stub.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.rsplit('/').next().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn agy_live_model_uses_the_tail_steps_call_and_skips_the_full_trajectory() {
        let tmp = TempDir::new().unwrap();
        let transcript =
            write_agy_transcript(tmp.path(), "{\"step_index\":6767}\n{\"step_index\":6768}\n");
        let stub = spawn_agy_stub_with_steps(
            AGY_CONFIG_FIXTURE,
            // The full trajectory names a different model; it must never be asked.
            r#"{"trajectory":{"steps":[{"metadata":{"generatorModel":"MODEL_PLACEHOLDER_M420"}}]}}"#,
            "agy-steps-1",
            Some((200, AGY_STEPS_FIXTURE)),
        );
        let addr = format!("127.0.0.1:{}", stub.port);
        let info = agy_live_model_info_for_transcript(
            Some(&addr),
            Some("stub-csrf"),
            "agy-steps-1",
            Some(&transcript),
            false,
        );
        assert_eq!(info.model.as_deref(), Some("gemini-4-argon"));
        assert_eq!(info.thinking_level.as_deref(), Some("high"));
        assert_eq!(
            stub_paths(&stub),
            ["GetCascadeModelConfigData", "GetCascadeTrajectorySteps"],
            "the full trajectory endpoint is not called"
        );
        let bodies = stub.bodies.lock().unwrap().clone();
        let steps_body: Value = serde_json::from_str(&bodies[1].1).unwrap();
        assert_eq!(
            steps_body,
            serde_json::json!({"cascadeId":"agy-steps-1","stepOffset":6758})
        );

        // The answer is cached: a second poll sends nothing.
        let again = agy_live_model_info_for_transcript(
            Some(&addr),
            Some("stub-csrf"),
            "agy-steps-1",
            Some(&transcript),
            false,
        );
        assert_eq!(again, info);
        assert_eq!(stub.seen.lock().unwrap().len(), 2);
        stub.shutdown();
    }

    #[test]
    fn agy_live_model_falls_back_to_the_full_trajectory_when_the_tail_call_finds_no_model() {
        let tmp = TempDir::new().unwrap();
        let transcript = write_agy_transcript(tmp.path(), "{\"step_index\":500}\n");
        let cases: [(&'static str, Option<(u16, &'static str)>); 4] = [
            (
                "agy-fb-empty",
                Some((200, r#"{"steps":[{"metadata":{}}]}"#)),
            ),
            ("agy-fb-error", Some((500, r#"{"code":"internal"}"#))),
            ("agy-fb-garbage", Some((200, "not json"))),
            ("agy-fb-unknown", None),
        ];
        for (cascade, steps) in cases {
            let stub = spawn_agy_stub_with_steps(
                AGY_CONFIG_FIXTURE,
                AGY_TRAJECTORY_FIXTURE,
                cascade,
                steps,
            );
            let addr = format!("127.0.0.1:{}", stub.port);
            let info = agy_live_model_info_for_transcript(
                Some(&addr),
                Some("stub-csrf"),
                cascade,
                Some(&transcript),
                false,
            );
            assert_eq!(info.model.as_deref(), Some("gemini-4-argon"), "{cascade}");
            assert_eq!(info.thinking_level.as_deref(), Some("high"), "{cascade}");
            assert_eq!(
                stub_paths(&stub),
                [
                    "GetCascadeModelConfigData",
                    "GetCascadeTrajectorySteps",
                    "GetCascadeTrajectory"
                ],
                "{cascade}: the tail call first, then the full trajectory"
            );
            stub.shutdown();
        }
    }

    #[test]
    fn agy_live_model_goes_straight_to_the_full_trajectory_without_a_step_offset() {
        let tmp = TempDir::new().unwrap();
        let young = write_agy_transcript(tmp.path(), "{\"step_index\":3}\n");
        let garbage = tmp.path().join("garbage.jsonl");
        fs::write(&garbage, "nope\n").unwrap();
        let garbage = garbage.display().to_string();
        let cases: [(&'static str, Option<&str>); 4] = [
            ("agy-direct-young", Some(&young)),
            ("agy-direct-garbage", Some(&garbage)),
            ("agy-direct-missing", Some("/definitely/missing.jsonl")),
            ("agy-direct-none", None),
        ];
        for (cascade, transcript) in cases {
            let stub = spawn_agy_stub_with_steps(
                AGY_CONFIG_FIXTURE,
                AGY_TRAJECTORY_FIXTURE,
                cascade,
                // A steps answer exists but must never be asked for.
                Some((
                    200,
                    r#"{"steps":[{"metadata":{"generatorModel":"MODEL_PLACEHOLDER_M420"}}]}"#,
                )),
            );
            let addr = format!("127.0.0.1:{}", stub.port);
            let info = agy_live_model_info_for_transcript(
                Some(&addr),
                Some("stub-csrf"),
                cascade,
                transcript,
                false,
            );
            assert_eq!(info.model.as_deref(), Some("gemini-4-argon"), "{cascade}");
            assert_eq!(
                stub_paths(&stub),
                ["GetCascadeModelConfigData", "GetCascadeTrajectory"],
                "{cascade}"
            );
            stub.shutdown();
        }
    }

    #[test]
    fn agy_discovery_passes_the_session_transcript_to_the_tail_steps_call() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let logs = home
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join("agy-disc-1")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("transcript.jsonl"), "{\"step_index\":900}\n").unwrap();
        let stub = spawn_agy_stub_with_steps(
            AGY_CONFIG_FIXTURE,
            AGY_TRAJECTORY_FIXTURE,
            "agy-disc-1",
            Some((200, AGY_STEPS_FIXTURE)),
        );
        let addr = format!("127.0.0.1:{}", stub.port);
        write_routes(
            home,
            &[(
                "ferris",
                &format!(
                    r#"{{"kind":"agy","conversation_id":"agy-disc-1","ls_address":"{addr}","csrf_token":"stub-csrf"}}"#
                ),
            )],
        );
        let sessions = discover_in(home);
        let agy = session_for(&sessions, "ferris");
        assert_eq!(agy.model.as_deref(), Some("gemini-4-argon"));
        assert!(
            !stub_paths(&stub).contains(&"GetCascadeTrajectory".to_string()),
            "{:?}",
            stub_paths(&stub)
        );
        stub.shutdown();
    }

    #[test]
    fn route_discovery_without_model_info_never_reads_model_sources() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let project = home.join(".claude").join("projects").join("p");
        fs::create_dir_all(&project).unwrap();
        let session_id = "claude-lookup-sess-1";
        fs::write(
            project.join(format!("{session_id}.jsonl")),
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-5-5","content":"a"}}"#,
        )
        .unwrap();
        fs::write(
            home.join(".claude").join("settings.json"),
            r#"{"model":"sonnet"}"#,
        )
        .unwrap();
        // settings.json wins only when strictly newer than the transcript. Pin both times so
        // the result does not depend on which filesystem tick each write landed in.
        set_mtime_secs(&project.join(format!("{session_id}.jsonl")), 2_000_000_000);
        set_mtime_secs(&home.join(".claude").join("settings.json"), 1_900_000_000);
        let stub = spawn_agy_stub_with_steps(
            AGY_CONFIG_FIXTURE,
            AGY_TRAJECTORY_FIXTURE,
            "agy-lookup-1",
            None,
        );
        let addr = format!("127.0.0.1:{}", stub.port);
        write_routes(
            home,
            &[
                (
                    "kayla",
                    &format!(
                        r#"{{"kind":"claude","pid":1,"session_id":"{session_id}","socket_path":"{}"}}"#,
                        home.join("c.sock").display()
                    ),
                ),
                (
                    "ferris",
                    &format!(
                        r#"{{"kind":"agy","conversation_id":"agy-lookup-1","ls_address":"{addr}","csrf_token":"stub-csrf"}}"#
                    ),
                ),
            ],
        );
        let snap = DiscoveryStoreSnapshot::default();

        // The lookup-only pass must not call the language server (the stub log stays
        // empty) and must leave every model field empty even though the transcript,
        // settings and stub all name a model.
        let lookup = discover_route_sessions_without_model_info(&snap, Some(home));
        assert!(
            stub.seen.lock().unwrap().is_empty(),
            "no language-server call: {:?}",
            stub.seen.lock().unwrap()
        );
        assert_eq!(lookup.len(), 2);
        for session in &lookup {
            assert_eq!(session.model, None, "{}", session.id);
            assert_eq!(session.thinking_level, None, "{}", session.id);
            assert!(session.available_models.is_empty(), "{}", session.id);
        }

        let full = discover_sessions_from_snapshot(&snap, Some(home));
        assert_eq!(
            full.iter()
                .map(|s| (&s.id, &s.persona, &s.source, &s.adapter))
                .collect::<Vec<_>>(),
            lookup
                .iter()
                .map(|s| (&s.id, &s.persona, &s.source, &s.adapter))
                .collect::<Vec<_>>(),
            "ids, personas, sources and adapters match the full discovery"
        );
        assert_eq!(
            session_for(&full, "kayla").model.as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(
            session_for(&full, "ferris").model.as_deref(),
            Some("gemini-4-argon")
        );
        assert!(
            !stub.seen.lock().unwrap().is_empty(),
            "the full discovery does call the language server"
        );
        stub.shutdown();
    }

    #[test]
    fn agy_trajectory_cut_by_the_size_cap_names_no_model() {
        // The first step names a model; padding pushes the body past the cap, so the
        // client receives only the OLDEST steps. Their model is stale and must not be shown.
        let pad = "a".repeat(AGY_TRAJECTORY_MAX_BYTES + 1024);
        let body: &'static str = Box::leak(
            format!(
                r#"{{"trajectory":{{"steps":[{{"metadata":{{"generatorModel":"MODEL_PLACEHOLDER_M417"}}}},{{"pad":"{pad}"}}]}}}}"#
            )
            .into_boxed_str(),
        );
        assert!(body.len() > AGY_TRAJECTORY_MAX_BYTES);
        let stub = spawn_agy_stub(AGY_CONFIG_FIXTURE, body);
        let addr = format!("127.0.0.1:{}", stub.port);
        let info = agy_live_model_info(Some(&addr), Some("stub-csrf"), "agy-live-1", false);
        // M417 is gemini-4-argon; the config default (M420) is shown instead of it.
        assert_eq!(info.model.as_deref(), Some("gemini-3.8-flash"));
        assert_eq!(info.thinking_level.as_deref(), Some("medium"));
        assert_eq!(
            info.available_models.first().map(String::as_str),
            Some("gemini-4-argon")
        );
        assert_eq!(
            stub_paths(&stub),
            ["GetCascadeModelConfigData", "GetCascadeTrajectory"]
        );
        stub.shutdown();

        // The strict parser accepts a complete body and refuses a cut one.
        assert_eq!(
            parse_agy_complete_generator_model(AGY_TRAJECTORY_FIXTURE).as_deref(),
            Some("MODEL_PLACEHOLDER_M417")
        );
        let cut = r#"{"trajectory":{"steps":[{"metadata":{"generatorModel":"MODEL_PLACEHOLDER_M416"}},{"metadata":{"generatorMo"#;
        assert_eq!(parse_agy_complete_generator_model(cut), None);
        assert_eq!(
            parse_agy_generator_model(cut).as_deref(),
            Some("MODEL_PLACEHOLDER_M416"),
            "the lenient parser still reads a cut body for the tail-steps call"
        );
    }

    #[test]
    fn agy_stub_ports_start_with_no_cached_answers() {
        // A cached answer for a port is dropped when a stub takes the port.
        agy_cache_put(&AGY_CONFIG_CACHE, 4242, None);
        agy_cache_put(&AGY_PLACEHOLDER_CACHE, (4242, "c".to_string()), None);
        agy_cache_put(&AGY_PLACEHOLDER_CACHE, (4243, "c".to_string()), None);
        forget_agy_port_caches(4242);
        assert!(agy_cache_get(&AGY_CONFIG_CACHE, &4242).is_none());
        assert!(agy_cache_get(&AGY_PLACEHOLDER_CACHE, &(4242, "c".to_string())).is_none());
        assert!(agy_cache_get(&AGY_PLACEHOLDER_CACHE, &(4243, "c".to_string())).is_some());
        forget_agy_port_caches(4243);
    }

    #[test]
    fn agy_failed_lookup_is_cached_briefly_then_retried() {
        let cache: AgyCache<u16, String> = Mutex::new(BTreeMap::new());
        let (ttl, negative_ttl) = (Duration::from_secs(30), Duration::from_millis(60));
        agy_cache_put(&cache, 1, None);
        agy_cache_put(&cache, 2, Some("hit".to_string()));
        assert_eq!(
            agy_cache_get_with_ttls(&cache, &1, ttl, negative_ttl),
            Some(None)
        );
        assert_eq!(
            agy_cache_get_with_ttls(&cache, &2, ttl, negative_ttl),
            Some(Some("hit".to_string()))
        );
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            agy_cache_get_with_ttls(&cache, &1, ttl, negative_ttl),
            None,
            "the cached miss expired, so the lookup is retried"
        );
        assert_eq!(
            agy_cache_get_with_ttls(&cache, &2, ttl, negative_ttl),
            Some(Some("hit".to_string())),
            "a hit lives for the longer TTL"
        );

        // Through a stub: a config that fails to parse is asked for once, then served
        // from the negative cache.
        let stub = spawn_agy_stub("not json", AGY_TRAJECTORY_FIXTURE);
        let addr = format!("127.0.0.1:{}", stub.port);
        let offline = offline_agy_model_info();
        for _ in 0..3 {
            assert_eq!(
                agy_live_model_info(Some(&addr), Some("stub-csrf"), "agy-live-1", false),
                offline
            );
        }
        assert_eq!(stub_paths(&stub), ["GetCascadeModelConfigData"]);
        stub.shutdown();
    }

    #[test]
    fn model_file_reads_are_capped() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f.json");
        let wrap = |pad: usize| format!(r#"{{"model":"x","pad":"{}"}}"#, "a".repeat(pad));
        let overhead = wrap(0).len();
        let cap = MODEL_FILE_MAX_BYTES as usize;

        fs::write(&path, wrap(cap - overhead)).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), MODEL_FILE_MAX_BYTES);
        assert!(read_json_file(&path).is_some(), "a file at the cap is read");
        fs::write(&path, wrap(cap - overhead + 1)).unwrap();
        assert!(
            read_json_file(&path).is_none(),
            "one byte over the cap is unreadable"
        );

        let toml_path = tmp.path().join("c.toml");
        fs::write(
            &toml_path,
            format!("model = \"x\"\n# {}\n", "a".repeat(cap)),
        )
        .unwrap();
        assert!(read_toml_file(&toml_path).is_none());
        fs::write(&toml_path, "model = \"x\"\n").unwrap();
        assert!(read_toml_file(&toml_path).is_some());

        // Through discovery: an over-cap Codex cache and config fall back silently.
        let codex = tmp.path().join(".codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(
            codex.join("models_cache.json"),
            format!(
                r#"{{"models":[{{"slug":"big-model","visibility":"list"}}],"pad":"{}"}}"#,
                "a".repeat(cap)
            ),
        )
        .unwrap();
        fs::write(
            codex.join("config.toml"),
            format!("model = \"cfg-model\"\n# {}\n", "a".repeat(cap)),
        )
        .unwrap();
        let info = codex_model_info(None, Some(tmp.path()));
        assert_eq!(info.model, None);
        assert_eq!(info.available_models, strings(&CODEX_DEFAULT_MODELS));

        // The tail reader returns at most the last `max_bytes`.
        let log = tmp.path().join("log.jsonl");
        fs::write(&log, "0123456789").unwrap();
        assert_eq!(read_file_tail(&log, 4).as_deref(), Some("6789"));
        assert_eq!(read_file_tail(&log, 100).as_deref(), Some("0123456789"));
    }

    #[test]
    fn claude_model_names_are_length_and_control_character_bounded() {
        let line = |model: &str| {
            serde_json::json!({"type":"assistant","message":{"role":"assistant","model":model}})
                .to_string()
        };
        let oversized = "m".repeat(MODEL_ID_MAX_CHARS + 1);
        let at_limit = "m".repeat(MODEL_ID_MAX_CHARS);
        // The newest valid name wins; an oversized or control-character one is skipped.
        let transcript = [
            line("claude-opus-5-5"),
            line(&oversized),
            line("bad\u{7}model"),
            line("two\nlines"),
        ]
        .join("\n");
        assert_eq!(
            latest_claude_transcript_model(&transcript).as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(
            latest_claude_transcript_model(&line(&at_limit)).as_deref(),
            Some(at_limit.as_str())
        );
        assert_eq!(latest_claude_transcript_model(&line(&oversized)), None);
        // The top-level `model` key is bounded the same way.
        let top = serde_json::json!({"model": "x\u{1b}[31m"}).to_string();
        assert_eq!(latest_claude_transcript_model(&top), None);

        // settings.json is too.
        let tmp = TempDir::new().unwrap();
        let claude = tmp.path().join(".claude");
        fs::create_dir_all(&claude).unwrap();
        for bad in [oversized.as_str(), "bad\u{7}model"] {
            fs::write(
                claude.join("settings.json"),
                serde_json::json!({ "model": bad }).to_string(),
            )
            .unwrap();
            assert_eq!(claude_model_info(None, Some(tmp.path())).model, None);
        }
        fs::write(claude.join("settings.json"), r#"{"model":"sonnet"}"#).unwrap();
        assert_eq!(
            claude_model_info(None, Some(tmp.path())).model.as_deref(),
            Some("claude-sonnet-5-5")
        );
    }

    #[test]
    fn claude_model_comes_from_the_latest_transcript_entry_then_settings() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let project = home.join(".claude").join("projects").join("p");
        fs::create_dir_all(&project).unwrap();
        let session_id = "claude-model-sess-1";
        let transcript = [
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-5-5","content":"a"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-5-5","content":"b"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","model":"<synthetic>","content":"c"}}"#,
            "not json at all",
        ]
        .join("\n");
        let transcript_file = project.join(format!("{session_id}.jsonl"));
        fs::write(&transcript_file, transcript).unwrap();
        fs::write(
            home.join(".claude").join("settings.json"),
            r#"{"model":"sonnet"}"#,
        )
        .unwrap();
        // The transcript is the newer file, so its own model wins over settings.json.
        set_mtime_secs(&home.join(".claude").join("settings.json"), 1_000);
        set_mtime_secs(&transcript_file, 2_000);
        write_routes(
            home,
            &[(
                "kayla",
                &format!(
                    r#"{{"kind":"claude","pid":1,"session_id":"{session_id}","socket_path":"{}"}}"#,
                    home.join("c.sock").display()
                ),
            )],
        );
        let sessions = discover_in(home);
        let claude = session_for(&sessions, "kayla");
        assert_eq!(claude.adapter, "claude");
        assert_eq!(claude.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(claude.thinking_level, None);
        assert_eq!(claude.available_models, strings(&CLAUDE_MODELS));

        // No transcript entry names a model: settings.json supplies it.
        let info = claude_model_info(None, Some(home));
        assert_eq!(info.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(info.available_models, strings(&CLAUDE_MODELS));

        // Malformed or missing settings never fail the lookup.
        fs::write(home.join(".claude").join("settings.json"), "{ broken").unwrap();
        assert_eq!(claude_model_info(None, Some(home)).model, None);
        fs::remove_file(home.join(".claude").join("settings.json")).unwrap();
        assert_eq!(claude_model_info(None, Some(home)).model, None);
        assert_eq!(
            claude_model_info(Some("/definitely/missing.jsonl"), None).available_models,
            strings(&CLAUDE_MODELS)
        );
    }

    /// Set a file's modification time to `secs` after the Unix epoch.
    fn set_mtime_secs(path: &Path, secs: u64) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn assistant_line(model: &str) -> String {
        serde_json::json!({"type":"assistant","message":{"role":"assistant","model":model}})
            .to_string()
    }

    fn model_command_line(content: Value) -> String {
        serde_json::json!({"type":"user","message":{"role":"user","content":content}}).to_string()
    }

    const OPUS_HIGH: &str = "<local-command-stdout>Set model to `Opus 5.5` and saved as your \
        default for new sessions with `high` effort</local-command-stdout>";

    #[test]
    fn claude_model_command_output_overrides_an_earlier_assistant_turn() {
        // String content and an array of text blocks both carry the command output.
        for content in [
            Value::String(OPUS_HIGH.to_string()),
            serde_json::json!([{"type":"text","text": OPUS_HIGH}]),
        ] {
            let transcript = [
                assistant_line("claude-sonnet-5-5"),
                model_command_line(content),
            ]
            .join("\n");
            assert_eq!(
                latest_claude_transcript_signal(&transcript),
                Some(("claude-opus-5-5".to_string(), Some("high".to_string())))
            );
        }
        // Without the effort suffix there is no effort.
        let plain = model_command_line(Value::String(
            "<local-command-stdout>Set model to `Sonnet 5.5` and saved as your default for new \
             sessions</local-command-stdout>"
                .to_string(),
        ));
        assert_eq!(
            latest_claude_transcript_signal(&[assistant_line("claude-opus-5-5"), plain].join("\n")),
            Some(("claude-sonnet-5-5".to_string(), None))
        );
    }

    #[test]
    fn claude_assistant_turn_after_a_model_command_wins_and_keeps_only_a_matching_effort() {
        let command = model_command_line(Value::String(OPUS_HIGH.to_string()));
        // A later turn on another model (the command was session-only and did not apply yet).
        let later_other = [command.clone(), assistant_line("claude-sonnet-5-5")].join("\n");
        assert_eq!(
            latest_claude_transcript_signal(&later_other),
            Some(("claude-sonnet-5-5".to_string(), None))
        );
        // A later turn on the commanded model, named by its dated id, keeps the effort.
        let later_same = [command, assistant_line("claude-opus-5-5-20261001")].join("\n");
        assert_eq!(
            latest_claude_transcript_signal(&later_same),
            Some(("claude-opus-5-5".to_string(), Some("high".to_string())))
        );
    }

    #[test]
    fn claude_model_command_text_that_is_not_a_command_turn_is_ignored() {
        let quoted = model_command_line(Value::String(format!("please explain: {OPUS_HIGH}")));
        let blob = serde_json::json!({
            "type":"api-request-blob",
            "message":{"role":"user","content":OPUS_HIGH},
        })
        .to_string();
        let assistant_quote = serde_json::json!({
            "type":"assistant",
            "message":{"role":"assistant","content":OPUS_HIGH},
        })
        .to_string();
        let transcript = [
            assistant_line("claude-sonnet-5-5"),
            quoted,
            blob,
            assistant_quote,
        ]
        .join("\n");
        assert_eq!(
            latest_claude_transcript_signal(&transcript),
            Some(("claude-sonnet-5-5".to_string(), None))
        );
        // An unquoted or oversized name is not a model either.
        let bad = model_command_line(Value::String(format!(
            "<local-command-stdout>Set model to `{}`</local-command-stdout>",
            "m".repeat(MODEL_ID_MAX_CHARS + 1)
        )));
        assert_eq!(latest_claude_transcript_signal(&bad), None);
    }

    #[test]
    fn claude_model_names_and_shorthands_normalize_to_canonical_ids() {
        for (raw, want) in [
            ("opus", "claude-opus-5-5"),
            ("Opus 5.5", "claude-opus-5-5"),
            ("OPUS", "claude-opus-5-5"),
            ("claude-opus-5-5", "claude-opus-5-5"),
            ("claude-opus-5-5-20261001", "claude-opus-5-5"),
            ("sonnet", "claude-sonnet-5-5"),
            ("Sonnet 5.5", "claude-sonnet-5-5"),
            ("claude-sonnet-5-5-20261001", "claude-sonnet-5-5"),
            ("haiku", "claude-haiku-4-5"),
            ("Haiku 4.5", "claude-haiku-4-5"),
            ("claude-haiku-4-5-20251001", "claude-haiku-4-5"),
            // A different real model is not folded into the current one.
            ("Haiku 5.5", "claude-haiku-5-5"),
            ("claude-haiku-5-5", "claude-haiku-5-5"),
            ("Opus 4.1", "claude-opus-4-1"),
            ("claude-opus-4-1", "claude-opus-4-1"),
            // Anything else passes through unchanged.
            ("some-other-model", "some-other-model"),
            ("opus-custom", "opus-custom"),
            ("Opus banana", "Opus banana"),
        ] {
            assert_eq!(normalize_claude_model(raw), want, "{raw}");
        }
    }

    #[test]
    fn claude_settings_json_wins_only_when_newer_than_the_transcript() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude = home.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        let transcript = home.join("t.jsonl");
        fs::write(&transcript, assistant_line("claude-sonnet-5-5")).unwrap();
        let tp = transcript.to_str().unwrap();

        // settings.json older than the transcript: the session's own model.
        fs::write(&settings, r#"{"model":"opus","effortLevel":"low"}"#).unwrap();
        set_mtime_secs(&settings, 1_000);
        set_mtime_secs(&transcript, 2_000);
        let info = claude_model_info(Some(tp), Some(home));
        assert_eq!(info.model.as_deref(), Some("claude-sonnet-5-5"));
        // The effort has no transcript source here, so settings supplies it.
        assert_eq!(info.thinking_level.as_deref(), Some("low"));

        // settings.json written afterwards (the Claude app or an edit): it wins at once.
        set_mtime_secs(&settings, 3_000);
        let info = claude_model_info(Some(tp), Some(home));
        assert_eq!(info.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(info.thinking_level.as_deref(), Some("low"));

        // A newer settings.json with no usable model does not erase the transcript's.
        fs::write(&settings, r#"{"effortLevel":"low"}"#).unwrap();
        set_mtime_secs(&settings, 4_000);
        assert_eq!(
            claude_model_info(Some(tp), Some(home)).model.as_deref(),
            Some("claude-sonnet-5-5")
        );
    }

    #[test]
    fn claude_effort_comes_from_the_model_command_then_settings() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude = home.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        let transcript = home.join("t.jsonl");
        fs::write(
            &transcript,
            [
                assistant_line("claude-sonnet-5-5"),
                model_command_line(Value::String(OPUS_HIGH.to_string())),
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(&settings, r#"{"model":"sonnet","effortLevel":"low"}"#).unwrap();
        set_mtime_secs(&settings, 1_000);
        set_mtime_secs(&transcript, 2_000);
        let tp = transcript.to_str().unwrap();
        // The newer `/model` output beats both the older assistant turn and settings.json.
        let info = claude_model_info(Some(tp), Some(home));
        assert_eq!(info.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(info.thinking_level.as_deref(), Some("high"));

        // Per-model effort in settings.json, by shorthand or canonical id.
        for key in ["opus", "claude-opus-5-5"] {
            fs::write(
                &settings,
                format!(
                    r#"{{"model":"opus","modelSettings":{{"{key}":{{"effortLevel":"medium"}}}}}}"#
                ),
            )
            .unwrap();
            let info = claude_model_info(None, Some(home));
            assert_eq!(info.model.as_deref(), Some("claude-opus-5-5"), "{key}");
            assert_eq!(info.thinking_level.as_deref(), Some("medium"), "{key}");
        }
        // An over-long or control-character effort is dropped.
        fs::write(&settings, r#"{"model":"opus","effortLevel":"bad\u0007"}"#).unwrap();
        assert_eq!(claude_model_info(None, Some(home)).thinking_level, None);
    }

    #[test]
    fn claude_per_model_settings_effort_belongs_to_the_winning_model_only() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude = home.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        let transcript = home.join("t.jsonl");
        fs::write(&transcript, assistant_line("claude-sonnet-5-5")).unwrap();
        let tp = transcript.to_str().unwrap();

        // Only the settings model (opus) has a per-model effort; the transcript wins with
        // sonnet, which must not inherit it.
        fs::write(
            &settings,
            r#"{"model":"opus","modelSettings":{"opus":{"effortLevel":"medium"}}}"#,
        )
        .unwrap();
        set_mtime_secs(&settings, 1_000);
        set_mtime_secs(&transcript, 2_000);
        let info = claude_model_info(Some(tp), Some(home));
        assert_eq!(info.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(info.thinking_level, None);

        // The converse: the winning model has its own per-model entry (canonical id).
        fs::write(
            &settings,
            r#"{"model":"opus","modelSettings":{"opus":{"effortLevel":"medium"},
                "claude-sonnet-5-5":{"effortLevel":"low"}}}"#,
        )
        .unwrap();
        set_mtime_secs(&settings, 1_000);
        let info = claude_model_info(Some(tp), Some(home));
        assert_eq!(info.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(info.thinking_level.as_deref(), Some("low"));

        // A global effortLevel applies to whichever model wins.
        fs::write(
            &settings,
            r#"{"model":"opus","effortLevel":"high","modelSettings":{"opus":{"effortLevel":"medium"}}}"#,
        )
        .unwrap();
        set_mtime_secs(&settings, 1_000);
        assert_eq!(
            claude_model_info(Some(tp), Some(home))
                .thinking_level
                .as_deref(),
            Some("high")
        );

        // A key with `/` or `~` is a plain key, not a JSON pointer.
        fs::write(
            &settings,
            r#"{"model":"a/b~c","modelSettings":{"a":{"b~c":{"effortLevel":"wrong"}},
                "a/b~c":{"effortLevel":"max"}}}"#,
        )
        .unwrap();
        let info = claude_model_info(None, Some(home));
        assert_eq!(info.model.as_deref(), Some("a/b~c"));
        assert_eq!(info.thinking_level.as_deref(), Some("max"));
    }

    #[test]
    fn claude_newer_settings_naming_the_transcript_model_keep_the_command_effort() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude = home.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        let transcript = home.join("t.jsonl");
        fs::write(
            &transcript,
            model_command_line(Value::String(OPUS_HIGH.to_string())),
        )
        .unwrap();
        let tp = transcript.to_str().unwrap();
        set_mtime_secs(&transcript, 1_000);

        // Same model, no settings effort: the command's effort survives.
        fs::write(&settings, r#"{"model":"opus"}"#).unwrap();
        set_mtime_secs(&settings, 2_000);
        let info = claude_model_info(Some(tp), Some(home));
        assert_eq!(info.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(info.thinking_level.as_deref(), Some("high"));

        // Settings effort, when present, still wins.
        fs::write(&settings, r#"{"model":"opus","effortLevel":"low"}"#).unwrap();
        set_mtime_secs(&settings, 2_000);
        assert_eq!(
            claude_model_info(Some(tp), Some(home))
                .thinking_level
                .as_deref(),
            Some("low")
        );

        // A different settings model does not take the command's effort.
        fs::write(&settings, r#"{"model":"sonnet"}"#).unwrap();
        set_mtime_secs(&settings, 2_000);
        let info = claude_model_info(Some(tp), Some(home));
        assert_eq!(info.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(info.thinking_level, None);
    }

    #[test]
    fn claude_settings_and_transcript_edge_cases() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude = home.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        let transcript = home.join("t.jsonl");
        fs::write(&transcript, assistant_line("claude-sonnet-5-5")).unwrap();
        let tp = transcript.to_str().unwrap();

        // Equal mtimes: the comparison is strict, so the transcript wins.
        fs::write(&settings, r#"{"model":"opus"}"#).unwrap();
        set_mtime_secs(&settings, 2_000);
        set_mtime_secs(&transcript, 2_000);
        assert_eq!(
            claude_model_info(Some(tp), Some(home)).model.as_deref(),
            Some("claude-sonnet-5-5")
        );

        // A transcript path to a missing file: settings supplies the model.
        let missing = home.join("missing.jsonl");
        assert_eq!(
            claude_model_info(missing.to_str(), Some(home))
                .model
                .as_deref(),
            Some("claude-opus-5-5")
        );

        // A newer settings.json whose model is unusable never erases the transcript's.
        let oversized = "m".repeat(MODEL_ID_MAX_CHARS + 1);
        for bad in [oversized.as_str(), "bad\u{7}model"] {
            fs::write(&settings, serde_json::json!({ "model": bad }).to_string()).unwrap();
            set_mtime_secs(&settings, 3_000);
            assert_eq!(
                claude_model_info(Some(tp), Some(home)).model.as_deref(),
                Some("claude-sonnet-5-5")
            );
        }
    }

    #[test]
    fn codex_model_reads_config_toml_and_the_listed_models_cache() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let codex = home.join(".codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(
            codex.join("config.toml"),
            "model = \"gpt-6.1-terra\"\nmodel_reasoning_effort = \"high\"\n",
        )
        .unwrap();
        fs::write(
            codex.join("models_cache.json"),
            r#"{"models":[
                {"slug":"gpt-6.1-sol","visibility":"list"},
                {"slug":"gpt-internal","visibility":"hide"},
                {"slug":"gpt-6.1-terra","visibility":"list"},
                {"visibility":"list"}
            ]}"#,
        )
        .unwrap();
        write_routes(
            home,
            &[("katie", r#"{"kind":"codex","thread_id":"codex-model-1"}"#)],
        );
        let sessions = discover_in(home);
        let codex_sess = session_for(&sessions, "katie");
        assert_eq!(codex_sess.adapter, "codex");
        assert_eq!(codex_sess.model.as_deref(), Some("gpt-6.1-terra"));
        assert_eq!(codex_sess.thinking_level.as_deref(), Some("high"));
        assert_eq!(
            codex_sess.available_models,
            strings(&["gpt-6.1-sol", "gpt-6.1-terra"])
        );
    }

    #[test]
    fn codex_model_falls_back_when_files_are_missing_or_malformed() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let missing = codex_model_info(None, Some(home));
        assert_eq!(missing.model, None);
        assert_eq!(missing.thinking_level, None);
        assert_eq!(missing.available_models, strings(&CODEX_DEFAULT_MODELS));

        let codex = home.join(".codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(codex.join("config.toml"), "model = [unterminated").unwrap();
        fs::write(codex.join("models_cache.json"), "{ nope").unwrap();
        assert_eq!(codex_model_info(None, Some(home)), missing);

        // A cache that lists nothing visible also falls back.
        fs::write(
            codex.join("models_cache.json"),
            r#"{"models":[{"slug":"x","visibility":"hide"}]}"#,
        )
        .unwrap();
        assert_eq!(
            codex_model_info(None, Some(home)).available_models,
            strings(&CODEX_DEFAULT_MODELS)
        );
        assert_eq!(
            codex_model_info(None, None).available_models,
            strings(&CODEX_DEFAULT_MODELS)
        );
    }

    /// A temp home whose `config.toml` defaults to `gpt-6.1-terra` at `high`.
    fn codex_home_with_config() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let codex = tmp.path().join(".codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(
            codex.join("config.toml"),
            "model = \"gpt-6.1-terra\"\nmodel_reasoning_effort = \"high\"\n",
        )
        .unwrap();
        tmp
    }

    /// Write a rollout under the temp home and return its path.
    fn write_codex_rollout(home: &Path, thread_id: &str, body: &str) -> String {
        let dir = home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("10")
            .join("30");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-10-30T01-02-03-{thread_id}.jsonl"));
        fs::write(&path, body).unwrap();
        path.display().to_string()
    }

    fn codex_session_info(home: &Path, rollout_lines: &[&str]) -> SessionModelInfo {
        let path = write_codex_rollout(home, "t-fixture", &rollout_lines.join("\n"));
        codex_model_info(Some(&path), Some(home))
    }

    #[test]
    fn codex_model_comes_from_the_session_rollout_before_config() {
        let tmp = codex_home_with_config();
        let info = codex_session_info(
            tmp.path(),
            &[
                r#"{"type":"session_meta","payload":{"id":"t-fixture"}}"#,
                r#"{"type":"turn_context","payload":{"model":"gpt-6.1-sol","effort":"low"}}"#,
            ],
        );
        assert_eq!(info.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(info.thinking_level.as_deref(), Some("low"));
        assert_eq!(
            info.available_models,
            strings(&CODEX_DEFAULT_MODELS),
            "the selectable models do not come from the rollout"
        );
    }

    #[test]
    fn codex_effort_falls_back_to_config_when_the_rollout_has_a_model_only() {
        let tmp = codex_home_with_config();
        let info = codex_session_info(
            tmp.path(),
            &[r#"{"type":"turn_context","payload":{"model":"gpt-6.1-sol"}}"#],
        );
        assert_eq!(info.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(info.thinking_level.as_deref(), Some("high"));
        // An effort with no model keeps the config model.
        let info = codex_session_info(
            tmp.path(),
            &[r#"{"type":"turn_context","payload":{"effort":"minimal"}}"#],
        );
        assert_eq!(info.model.as_deref(), Some("gpt-6.1-terra"));
        assert_eq!(info.thinking_level.as_deref(), Some("minimal"));
    }

    #[test]
    fn codex_effort_pointers_are_honored_in_priority_order() {
        let tmp = codex_home_with_config();
        let effort = |line: &str| {
            codex_session_info(tmp.path(), &[line])
                .thinking_level
                .unwrap()
        };
        assert_eq!(
            effort(
                r#"{"payload":{"effort":"low","reasoning_effort":"medium","collaboration_mode":{"settings":{"reasoning_effort":"xhigh"}}}}"#
            ),
            "low"
        );
        assert_eq!(
            effort(
                r#"{"payload":{"reasoning_effort":"medium","collaboration_mode":{"settings":{"reasoning_effort":"xhigh"}}}}"#
            ),
            "medium"
        );
        assert_eq!(
            effort(
                r#"{"payload":{"collaboration_mode":{"settings":{"reasoning_effort":"xhigh"}}}}"#
            ),
            "xhigh"
        );
        // An unusable higher pointer yields to the next one on the same line.
        assert_eq!(
            effort(r#"{"payload":{"effort":"","reasoning_effort":"medium"}}"#),
            "medium"
        );
        // Only the payload is read, not other top-level fields.
        assert_eq!(effort(r#"{"effort":"low","model":"x"}"#), "high");
    }

    #[test]
    fn codex_later_rollout_lines_win_field_by_field() {
        let tmp = codex_home_with_config();
        let info = codex_session_info(
            tmp.path(),
            &[
                r#"{"payload":{"model":"gpt-old","effort":"low"}}"#,
                r#"{"payload":{"model":"gpt-6.1-sol"}}"#,
                r#"{"type":"event_msg","payload":{"type":"token_count"}}"#,
                r#"{"payload":{"effort":"medium"}}"#,
            ],
        );
        assert_eq!(info.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(info.thinking_level.as_deref(), Some("medium"));
        // The newer of two full lines wins both fields.
        let info = codex_session_info(
            tmp.path(),
            &[
                r#"{"payload":{"model":"gpt-a","effort":"low"}}"#,
                r#"{"payload":{"model":"gpt-b","effort":"minimal"}}"#,
            ],
        );
        assert_eq!(info.model.as_deref(), Some("gpt-b"));
        assert_eq!(info.thinking_level.as_deref(), Some("minimal"));
    }

    #[test]
    fn codex_rollout_tail_ignores_a_cut_first_line_and_malformed_lines() {
        let tmp = codex_home_with_config();
        let pad = |n: usize| "x".repeat(n);
        let cut_line = |n: usize| {
            format!(
                r#"{{"pad":"{}","payload":{{"model":"gpt-cut","effort":"cut"}}}}"#,
                pad(n)
            )
        };
        let newest = r#"{"payload":{"reasoning_effort":"medium"}}"#;

        // Control: a short file keeps its first line, so its model is used.
        let path = write_codex_rollout(
            tmp.path(),
            "t-short",
            &format!("{}\n{newest}\n", cut_line(10)),
        );
        let info = codex_model_info(Some(&path), Some(tmp.path()));
        assert_eq!(info.model.as_deref(), Some("gpt-cut"));
        assert_eq!(info.thinking_level.as_deref(), Some("medium"));

        // A first line longer than the tail window is cut in the middle: its model is not
        // read, so the config model stays.
        let long = CLAUDE_TRANSCRIPT_TAIL_BYTES as usize;
        let path = write_codex_rollout(
            tmp.path(),
            "t-long",
            &format!("{}\n{newest}\n", cut_line(long)),
        );
        let info = codex_model_info(Some(&path), Some(tmp.path()));
        assert_eq!(info.model.as_deref(), Some("gpt-6.1-terra"));
        assert_eq!(info.thinking_level.as_deref(), Some("medium"));

        // Malformed lines, blank lines and wrongly typed values are skipped.
        let info = codex_session_info(
            tmp.path(),
            &[
                r#"{"payload":{"model":"gpt-6.1-sol","effort":"low"}}"#,
                "this is not json but mentions \"model\" and effort",
                r#"{"payload":{"model":"#,
                "",
                r#"{"payload":{"model":42,"effort":["high"]}}"#,
                r#"[1,2,3]"#,
            ],
        );
        assert_eq!(info.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(info.thinking_level.as_deref(), Some("low"));
    }

    #[test]
    fn codex_without_a_readable_rollout_uses_config() {
        let tmp = codex_home_with_config();
        let home = tmp.path();
        let config = codex_model_info(None, Some(home));
        assert_eq!(config.model.as_deref(), Some("gpt-6.1-terra"));
        assert_eq!(config.thinking_level.as_deref(), Some("high"));
        assert_eq!(
            codex_model_info(Some("/definitely/missing.jsonl"), Some(home)),
            config
        );
        // A directory is unreadable as a file, and an empty or binary file names nothing.
        assert_eq!(codex_model_info(home.to_str(), Some(home)), config);
        let empty = write_codex_rollout(home, "t-empty", "");
        assert_eq!(codex_model_info(Some(&empty), Some(home)), config);
        let binary = home.join("binary.jsonl");
        fs::write(&binary, [0xff, 0xfe, 0x00, b'{', 0x80]).unwrap();
        assert_eq!(codex_model_info(binary.to_str(), Some(home)), config);
        // No config either: nothing is invented.
        let bare = TempDir::new().unwrap();
        let rollout = write_codex_rollout(bare.path(), "t-bare", "not json");
        let info = codex_model_info(Some(&rollout), Some(bare.path()));
        assert_eq!((info.model, info.thinking_level), (None, None));
    }

    #[test]
    fn codex_rollout_values_are_validated_before_use() {
        let tmp = codex_home_with_config();
        let model_of = |model: &str, effort: &str| {
            let line = serde_json::json!({"payload": {"model": model, "effort": effort}});
            let info = codex_session_info(tmp.path(), &[&line.to_string()]);
            (info.model.unwrap(), info.thinking_level.unwrap())
        };
        let config = ("gpt-6.1-terra".to_string(), "high".to_string());

        // Oversized and control-character values are dropped, so config answers.
        assert_eq!(model_of(&"m".repeat(129), &"e".repeat(33)), config);
        assert_eq!(model_of("gpt\u{7}x", "lo\nw"), config);
        assert_eq!(model_of("gpt\nsol", "lo\rw"), config);
        assert_eq!(model_of("   ", "\t"), config);
        // The bounds themselves are accepted, and surrounding whitespace is trimmed.
        assert_eq!(
            model_of(&"m".repeat(128), &"e".repeat(32)),
            ("m".repeat(128), "e".repeat(32))
        );
        assert_eq!(
            model_of("  gpt-6.1-sol ", " low "),
            ("gpt-6.1-sol".to_string(), "low".to_string())
        );
    }

    #[test]
    fn codex_discovery_reports_each_session_own_rollout_model() {
        let tmp = codex_home_with_config();
        let home = tmp.path();
        write_codex_rollout(
            home,
            "codex-sol-thread",
            r#"{"type":"turn_context","payload":{"model":"gpt-6.1-sol","effort":"xhigh"}}"#,
        );
        write_codex_rollout(
            home,
            "codex-default-thread",
            r#"{"type":"session_meta","payload":{"id":"codex-default-thread"}}"#,
        );
        write_routes(
            home,
            &[
                ("sol", r#"{"kind":"codex","thread_id":"codex-sol-thread"}"#),
                (
                    "plain",
                    r#"{"kind":"codex","thread_id":"codex-default-thread"}"#,
                ),
            ],
        );
        let sessions = discover_in(home);
        let sol = session_for(&sessions, "sol");
        assert_eq!(sol.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(sol.thinking_level.as_deref(), Some("xhigh"));
        let plain = session_for(&sessions, "plain");
        assert_eq!(plain.model.as_deref(), Some("gpt-6.1-terra"));
        assert_eq!(plain.thinking_level.as_deref(), Some("high"));
    }

    #[test]
    fn grok_model_prefers_the_session_summary_over_config_defaults() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let session_id = "grok-model-sess-1";
        let dir = home
            .join(".grok")
            .join("sessions")
            .join("%2Fhome%2Fx")
            .join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("chat_history.jsonl"), "").unwrap();
        fs::write(
            dir.join("summary.json"),
            r#"{"current_model_id":"grok-4.6","num_messages":3}"#,
        )
        .unwrap();
        fs::write(
            home.join(".grok").join("config.toml"),
            "[models]\ndefault = \"grok-4.7\"\ndefault_reasoning_effort = \"high\"\n",
        )
        .unwrap();
        fs::write(
            home.join(".grok").join("models_cache.json"),
            r#"{"models":{
                "grok-4.7":{"info":{"hidden":false}},
                "grok-secret":{"info":{"hidden":true}},
                "grok-4.7-build-fast":{"info":{}}
            }}"#,
        )
        .unwrap();
        write_routes(
            home,
            &[(
                "wendy",
                &format!(r#"{{"kind":"grok","session_id":"{session_id}"}}"#),
            )],
        );
        let sessions = discover_in(home);
        let grok = session_for(&sessions, "wendy");
        assert_eq!(grok.adapter, "grok");
        assert_eq!(grok.model.as_deref(), Some("grok-4.6"));
        assert_eq!(grok.thinking_level.as_deref(), Some("high"));
        assert_eq!(
            grok.available_models,
            strings(&["grok-4.7", "grok-4.7-build-fast"]),
            "hidden models are left out"
        );

        // Without a session summary the config default names the model.
        fs::remove_file(dir.join("summary.json")).unwrap();
        let sessions = discover_in(home);
        assert_eq!(
            session_for(&sessions, "wendy").model.as_deref(),
            Some("grok-4.7")
        );
    }

    #[test]
    fn grok_model_falls_back_when_files_are_missing_or_malformed() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let missing = grok_model_info(None, Some(home));
        assert_eq!(missing.model, None);
        assert_eq!(missing.thinking_level, None);
        assert_eq!(missing.available_models, strings(&GROK_DEFAULT_MODELS));

        let grok = home.join(".grok");
        let session = grok.join("sessions").join("p").join("s1");
        fs::create_dir_all(&session).unwrap();
        fs::write(grok.join("config.toml"), "[models\nbroken").unwrap();
        fs::write(grok.join("models_cache.json"), "[]").unwrap();
        fs::write(session.join("summary.json"), "{ not json").unwrap();
        let chat = session.join("chat_history.jsonl");
        assert_eq!(
            grok_model_info(chat.to_str(), Some(home)),
            missing,
            "malformed files fall back silently"
        );

        // Top-level config keys are accepted as well as `[models]`.
        fs::write(
            grok.join("config.toml"),
            "default = \"grok-4.5\"\ndefault_reasoning_effort = \"low\"\n",
        )
        .unwrap();
        let info = grok_model_info(chat.to_str(), Some(home));
        assert_eq!(info.model.as_deref(), Some("grok-4.5"));
        assert_eq!(info.thinking_level.as_deref(), Some("low"));
    }

    #[test]
    fn muse_and_other_adapters_report_fixed_or_empty_model_fields() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        write_routes(
            home,
            &[
                (
                    "muse-persona",
                    r#"{"kind":"muse","session_id":"muse-model-1"}"#,
                ),
                (
                    "hook",
                    &format!(
                        r#"{{"kind":"webhook","socket_path":"{}"}}"#,
                        home.join("hook.sock").display()
                    ),
                ),
            ],
        );
        let sessions = discover_in(home);
        let muse = session_for(&sessions, "muse-persona");
        assert_eq!(muse.model.as_deref(), Some("muse-wonder-full-1.0"));
        assert_eq!(muse.thinking_level, None);
        assert_eq!(
            muse.available_models,
            strings(&["muse-wonder-full-1.0", "muse-wonder-1.0", "muse-spark-1.3"])
        );
        let hook = session_for(&sessions, "hook");
        assert_eq!(hook.model, None);
        assert_eq!(hook.thinking_level, None);
        assert!(hook.available_models.is_empty());
        let json = serde_json::to_value(hook).unwrap();
        assert!(json.get("model").is_none());
        assert!(json.get("thinking_level").is_none());
        assert!(json.get("available_models").is_none());
        let muse_json = serde_json::to_value(muse).unwrap();
        assert_eq!(muse_json["model"], "muse-wonder-full-1.0");
        assert_eq!(
            muse_json["available_models"],
            serde_json::json!(["muse-wonder-full-1.0", "muse-wonder-1.0", "muse-spark-1.3"])
        );
    }

    /// A Muse `session.jsonl` line of `payload_type` with `payload`.
    /// Layout per Lead's inspection of Muse 1.4.14; unverified against a live file.
    fn muse_line(payload_type: &str, payload: serde_json::Value) -> String {
        serde_json::json!({
            "schema_version": 1,
            "recorded_at": 1_793_104_670_899_152_i64,
            "payload_type": payload_type,
            "payload": payload,
        })
        .to_string()
    }

    fn muse_metadata_line(model_id: &str) -> String {
        muse_line(
            "runtime.session.metadata",
            serde_json::json!({"record": {"model_id": model_id}}),
        )
    }

    /// Discovery of one Muse route whose synthetic transcript holds `lines` (none: no file).
    fn muse_session_with_transcript(lines: Option<&[String]>) -> DiscoveredSessionSummary {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let id = "01a17d5b-482d-7ec0-a974-eaa56ef667a7";
        write_routes(
            home,
            &[(
                "muse-persona",
                &format!(r#"{{"kind":"muse","session_id":"{id}"}}"#),
            )],
        );
        if let Some(lines) = lines {
            let dir = home.join(".local/share/muse/sessions/2026/10/27").join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("session.jsonl"), lines.join("\n")).unwrap();
        }
        let sessions = discover_in(home);
        session_for(&sessions, "muse-persona").clone()
    }

    #[test]
    fn muse_model_is_the_last_session_metadata_model_id() {
        let turn = muse_line(
            "runtime.session",
            serde_json::json!({"kind":"run","event":{"kind":"assistant_message_committed","text":"hi"}}),
        );
        let session = muse_session_with_transcript(Some(&[
            muse_metadata_line("muse-wonder-1.0"),
            turn.clone(),
            muse_metadata_line("muse-spark-1.3"),
            turn,
            // Another record type that merely mentions the metadata name is not a metadata record.
            muse_line(
                "runtime.session",
                serde_json::json!({"note": "runtime.session.metadata", "record": {"model_id": "muse-wonder-1.0"}}),
            ),
        ]));
        assert!(session.transcript_path.is_some());
        assert_eq!(session.model.as_deref(), Some("muse-spark-1.3"));
        assert_eq!(
            session.available_models,
            strings(&["muse-wonder-full-1.0", "muse-wonder-1.0", "muse-spark-1.3"])
        );
    }

    #[test]
    fn muse_model_defaults_without_a_metadata_record() {
        // A transcript with no metadata record, one whose record has no model_id, and no file.
        let other = muse_line(
            "runtime.session",
            serde_json::json!({"model_id":"muse-spark-1.3"}),
        );
        let no_id = muse_line("runtime.session.metadata", serde_json::json!({"record":{}}));
        for lines in [
            Some(vec![other]),
            Some(vec![no_id, "not json".to_string()]),
            None,
        ] {
            let session = muse_session_with_transcript(lines.as_deref());
            assert_eq!(session.model.as_deref(), Some("muse-wonder-full-1.0"));
        }
    }

    #[test]
    fn attempt_rows_carry_no_model_fields() {
        let tmp = TempDir::new().unwrap();
        let snap = DiscoveryStoreSnapshot {
            registered_names: BTreeSet::new(),
            attempts: vec![DiscoveryAttemptRow {
                id: "attempt-1".into(),
                adapter: "claude".into(),
                status: "succeeded".into(),
                updated_at: None,
                cwd: "/tmp/x".into(),
                record_id: "rec-1".into(),
                record_str: "rec-1".into(),
                output_excerpt: None,
                created_at: None,
            }],
        };
        let sessions = discover_sessions_from_snapshot(&snap, Some(tmp.path()));
        let attempt = sessions.iter().find(|s| s.id == "attempt-1").unwrap();
        assert_eq!(attempt.source, "attempt");
        assert_eq!(attempt.model, None);
        assert_eq!(attempt.thinking_level, None);
        assert!(attempt.available_models.is_empty());
    }

    #[test]
    fn parse_jsonl_transcript_normalizes_antigravity_generic_tool_output_as_tool_role() {
        let jsonl = [
            serde_json::json!({
                "step_index": 0,
                "source": "MODEL",
                "type": "GENERIC",
                "content": "Created At: 2026-10-27T15:38:05Z\nCompleted At: 2026-10-27T15:38:06Z\nExit Code: 0\n\ntest result: ok.",
                "created_at": "2026-10-27T15:38:06Z"
            })
            .to_string(),
            serde_json::json!({
                "step_index": 1,
                "source": "MODEL",
                "content": "Created At: 2026-10-27T15:38:07Z\nCompleted At: 2026-10-27T15:38:08Z\nShowing lines 1-10",
                "created_at": "2026-10-27T15:38:08Z"
            })
            .to_string(),
        ]
        .join("\n");

        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "tool");
        assert!(turns[0].content.contains("test result: ok."));
        assert_eq!(turns[1].role, "tool");
    }

    #[test]
    fn git_worktree_inspector_lists_worktree_and_diff() {
        let tmp = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(tmp.path())
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        fs::write(tmp.path().join("README.md"), "hello\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-qm", "init"]);

        // Modify README.md so worktree is dirty and has a diff
        fs::write(tmp.path().join("README.md"), "hello world\n").unwrap();

        let wts = list_git_worktrees(tmp.path()).unwrap();
        assert_eq!(wts.len(), 1);
        assert!(!wts[0].is_clean);
        assert_eq!(wts[0].dirty_files, vec!["README.md".to_string()]);

        let diff = read_worktree_diff(tmp.path(), &wts[0].id).unwrap().unwrap();
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "README.md");
        assert!(diff.diff.contains("+hello world"));
    }

    #[test]
    fn is_transcript_busy_detects_active_vs_idle_session_states() {
        assert!(!is_transcript_busy(&[]));

        let user_turn = TranscriptTurn {
            index: 0,
            role: "user".into(),
            content: "Please run tests".into(),
            thinking: None,
            tool_calls: Vec::new(),
            created_at: None,
        };
        assert!(is_transcript_busy(std::slice::from_ref(&user_turn)));

        let assistant_with_tools = TranscriptTurn {
            index: 1,
            role: "assistant".into(),
            content: "Running cargo test".into(),
            thinking: None,
            tool_calls: vec![TranscriptToolCall {
                name: "run_command".into(),
                arguments: serde_json::json!({"cmd": "cargo test"}),
            }],
            created_at: None,
        };
        assert!(is_transcript_busy(&[
            user_turn.clone(),
            assistant_with_tools.clone()
        ]));

        let tool_turn = TranscriptTurn {
            index: 2,
            role: "tool".into(),
            content: "test result: ok.".into(),
            thinking: None,
            tool_calls: Vec::new(),
            created_at: None,
        };
        assert!(is_transcript_busy(&[
            user_turn.clone(),
            assistant_with_tools.clone(),
            tool_turn.clone()
        ]));

        let final_assistant = TranscriptTurn {
            index: 3,
            role: "assistant".into(),
            content: "All tests passed.".into(),
            thinking: None,
            tool_calls: Vec::new(),
            created_at: None,
        };
        let thinking_only_assistant = TranscriptTurn {
            index: 3,
            role: "assistant".into(),
            content: String::new(),
            thinking: Some("Reasoning in progress...".into()),
            tool_calls: Vec::new(),
            created_at: None,
        };
        assert!(is_transcript_busy(&[
            user_turn.clone(),
            thinking_only_assistant
        ]));
        assert!(!is_transcript_busy(&[
            user_turn,
            assistant_with_tools,
            tool_turn,
            final_assistant
        ]));
    }

    #[test]
    fn find_transcript_file_and_parse_muse_session_jsonl() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let session_id = "01a17d5b-482d-7ec0-a974-eaa56ef667a7";

        // Create a non-transcript `.json` runtime file that shares the session stem
        let runtime_dir = home.join(".local/share/muse/runtime/muse/sessions");
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(
            runtime_dir.join(format!("{session_id}.json")),
            r#"{"status":"running"}"#,
        )
        .unwrap();

        // Create the actual Muse `session.jsonl` inside the session directory
        let session_dir = home
            .join(".local/share/muse/sessions/2026/10/27")
            .join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        let jsonl_path = session_dir.join("session.jsonl");
        let jsonl = [
            serde_json::json!({
                "schema_version": 1,
                "recorded_at": 1_793_104_670_899_152_i64,
                "payload_type": "peer_message.admission.requested",
                "payload": {
                    "body": "Hello felicia"
                }
            })
            .to_string(),
            serde_json::json!({
                "schema_version": 1,
                "recorded_at": 1_793_104_845_773_692_i64,
                "payload_type": "runtime.session",
                "payload": {
                    "kind": "run",
                    "event": {
                        "kind": "assistant_tool_calls_committed",
                        "tool_calls": [
                            {
                                "name": "list_peer_sessions",
                                "args": "{\"scope\":\"workspace\"}"
                            }
                        ]
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "schema_version": 1,
                "recorded_at": 1_793_104_889_531_568_i64,
                "payload_type": "runtime.session",
                "payload": {
                    "kind": "run",
                    "event": {
                        "kind": "assistant_message_committed",
                        "text": "Peer session greeted Felicia; I replied with a hello back."
                    }
                }
            })
            .to_string(),
        ]
        .join("\n");
        fs::write(&jsonl_path, &jsonl).unwrap();

        let found =
            find_transcript_file_in_home(home, session_id).expect("must find session.jsonl");
        assert_eq!(found, jsonl_path.display().to_string());

        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].content, "Hello felicia");
        assert!(turns[0].created_at.is_some());
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].tool_calls.len(), 1);
        assert_eq!(turns[1].tool_calls[0].name, "list_peer_sessions");
        assert_eq!(
            turns[1].tool_calls[0].arguments,
            serde_json::json!({"scope": "workspace"})
        );
        assert_eq!(turns[2].role, "assistant");
        assert_eq!(
            turns[2].content,
            "Peer session greeted Felicia; I replied with a hello back."
        );
        assert!(!is_transcript_busy(&turns));

        // Verify that a subsequent user turn followed by a Muse terminal failure event
        // surfaces the error turn and leaves the session non-busy so prompts do not stall in queue.
        let quota_failed_jsonl = [
            jsonl.as_str(),
            &serde_json::json!({
                "schema_version": 1,
                "recorded_at": 1_793_198_990_000_000_i64,
                "payload_type": "runtime.user_intent.accepted",
                "payload": {
                    "model_messages": [
                        { "content": "Check status please" }
                    ]
                }
            })
            .to_string(),
            &serde_json::json!({
                "schema_version": 1,
                "recorded_at": 1_793_198_995_473_433_i64,
                "payload_type": "runtime.session",
                "payload": {
                    "kind": "run",
                    "event": {
                        "kind": "terminal",
                        "terminal": "failed",
                        "reason": "API error 429: Subscription quota exhausted. Your usage window resets at 2026-11-02T00:00:00Z."
                    }
                }
            })
            .to_string(),
        ]
        .join("\n");
        let failed_turns = parse_jsonl_transcript(&quota_failed_jsonl);
        assert_eq!(failed_turns.len(), 5);
        assert_eq!(failed_turns[3].role, "user");
        assert_eq!(failed_turns[3].content, "Check status please");
        assert_eq!(failed_turns[4].role, "assistant");
        assert!(
            failed_turns[4]
                .content
                .contains("Subscription quota exhausted")
        );
        assert!(!is_transcript_busy(&failed_turns));
    }

    #[test]
    fn parse_jsonl_transcript_normalizes_claude_tool_results_and_uds_peer_messages() {
        let jsonl = [
            serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_123",
                            "content": [{"type": "text", "text": "MCP tool output here"}]
                        }
                    ]
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": "Another Claude session sent a message:\n[Incoming NATS Message — already acknowledged]\nTimestamp: 2026-10-29T00:55:00Z\nFrom: sonya\nTo: sammy\nMessage ID: urn:uuid:11111111-1111-1111-1111-111111111111\nSubject: Welcome\n\nHi Sammy, welcome to Claude Code!\n\nThis came from another Claude session — not typed by your user, but very likely working on their behalf."
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": "Another Claude session sent a message:\n[Operator Prompt via FerroDeck — from lmcfarlin]\n\nPlease check the Sonora build.\n\nThis came from another Claude session — not typed by your user, but very likely working on their behalf."
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": "Another Claude session sent a message:\nHello Sammy. Welcome to the team.\n\nThis came from another Claude session — not typed by your user, but very likely working on their behalf."
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "api-request-blob",
                "message": {
                    "role": "system",
                    "content": [{"type": "text", "text": "Internal cached system prompt blob"}]
                }
            })
            .to_string(),
            serde_json::json!({
                "step_index": 4611,
                "source": "MODEL",
                "type": "GENERIC",
                "content": "Created At: 2026-10-29T01:03:12Z\nShowing lines 725-735\n728:     if content.contains(\"<SYSTEM_MESSAGE>\")\n734:         return (\"user\".to_string(), inner.to_string());"
            })
            .to_string(),
        ]
        .join("\n");

        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 5);
        assert_eq!(turns[0].role, "tool");
        assert_eq!(turns[0].content, "MCP tool output here");
        assert_eq!(turns[1].role, "wire:sonya");
        assert_eq!(turns[1].content, "Hi Sammy, welcome to Claude Code!");
        assert!(!turns[1].content.contains("Another Claude session"));
        assert_eq!(turns[2].role, "user");
        assert_eq!(turns[2].content, "Please check the Sonora build.");
        assert!(!turns[2].content.contains("Another Claude session"));
        assert_eq!(turns[3].role, "user");
        assert_eq!(turns[3].content, "Hello Sammy. Welcome to the team.");
        assert_eq!(turns[4].role, "tool");
        assert!(turns[4].content.contains("Showing lines 725-735"));
    }

    #[test]
    fn grok_nested_transcript_discovery_normalization_and_context_usage() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let store = fl_store::open_redb(&home.join("store.redb")).unwrap();

        let grok_session_dir = home
            .join(".grok")
            .join("sessions")
            .join("%2Fhome%2Flmcfarlin%2FProjects%2FArtificial-Humanity")
            .join("01a18538-6f1a-79c2-99d2-8e8540366c72");
        fs::create_dir_all(&grok_session_dir).unwrap();

        let jsonl = [
            serde_json::json!({
                "type": "system",
                "content": "You are Grok 4.7 released by xAI."
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "content": [{"type": "text", "text": "<user_info>\nOS Version: linux\n</user_info>"}]
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "synthetic_reason": "system_reminder",
                "content": [{"type": "text", "text": "<system-reminder>\nSkills available\n</system-reminder>"}]
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "prompt_index": 0,
                "content": [{"type": "text", "text": "<user_query>\nYou are Wendy. Please read agent instruction files.\n</user_query>"}]
            })
            .to_string(),
            serde_json::json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "I need to read the agent instructions and register as Wendy."}],
                "status": "completed"
            })
            .to_string(),
            serde_json::json!({
                "type": "assistant",
                "content": "I'll read the workspace orientation and register as Wendy.",
                "tool_calls": [
                    {
                        "id": "call-1",
                        "name": "read_file",
                        "arguments": "{\"target_file\":\"AGENTS.md\"}"
                    }
                ]
            })
            .to_string(),
            serde_json::json!({
                "type": "tool_result",
                "tool_call_id": "call-1",
                "content": "# Artificial Humanity workspace"
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "prompt_index": 1,
                "content": [{"type": "text", "text": "The user sent a message while you were working:\n<user_query>\nAlso note that you are replacing a previous Wendy\n</user_query>"}]
            })
            .to_string(),
            serde_json::json!({
                "type": "assistant",
                "content": "Wendy is registered and has reported to Wesley."
            })
            .to_string(),
        ]
        .join("\n");
        fs::write(grok_session_dir.join("chat_history.jsonl"), &jsonl).unwrap();
        fs::write(
            grok_session_dir.join("signals.json"),
            serde_json::json!({
                "contextTokensUsed": 47874,
                "contextWindowTokens": 500000
            })
            .to_string(),
        )
        .unwrap();

        let sessions = discover_sessions_with_home(&store, Some(home));
        let wendy_sess = sessions
            .iter()
            .find(|s| s.persona.as_deref() == Some("wendy"))
            .expect("wendy grok session should be discovered");
        assert_eq!(wendy_sess.adapter, "grok");
        assert_eq!(wendy_sess.id, "01a18538-6f1a-79c2-99d2-8e8540366c72");

        let turns = read_session_transcript_with_home("wendy", &store, Some(home));
        assert_eq!(turns.len(), 8);
        assert_eq!(turns[0].role, "system");
        assert_eq!(turns[1].role, "system");
        assert_eq!(turns[2].role, "system");
        assert_eq!(turns[3].role, "user");
        assert_eq!(
            turns[3].content,
            "You are Wendy. Please read agent instruction files."
        );
        assert_eq!(turns[4].role, "assistant");
        assert_eq!(
            turns[4].thinking.as_deref(),
            Some("I need to read the agent instructions and register as Wendy.")
        );
        assert_eq!(
            turns[4].content,
            "I'll read the workspace orientation and register as Wendy."
        );
        assert_eq!(turns[4].tool_calls.len(), 1);
        assert_eq!(turns[5].role, "tool");
        assert_eq!(turns[6].role, "user");
        assert_eq!(
            turns[6].content,
            "Also note that you are replacing a previous Wendy"
        );
        assert_eq!(turns[7].role, "assistant");
        assert!(!is_transcript_busy(&turns));

        let usage = read_session_context_usage_with_home("wendy", &store, Some(home))
            .expect("grok context usage should be read from signals.json");
        assert_eq!(
            usage,
            SessionContextUsage {
                used_tokens: 47874,
                max_tokens: 500000,
            }
        );
    }

    #[test]
    fn codex_rollout_transcript_normalization_and_context_usage() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let store = fl_store::open_redb(&home.join("store.redb")).unwrap();

        let codex_dir = home.join(".codex");
        let day_dir = codex_dir
            .join("sessions")
            .join("2026")
            .join("10")
            .join("29");
        fs::create_dir_all(&day_dir).unwrap();

        fs::write(
            codex_dir.join("config.toml"),
            "model = \"gpt-6.1-terra\"\nmodel_context_window = 872000\n",
        )
        .unwrap();

        let rollout_path =
            day_dir.join("rollout-2026-10-29T01-39-41-01a18550-1b11-7882-82b6-ea40058ed587.jsonl");
        let jsonl = [
            serde_json::json!({
                "timestamp": "2026-10-29T01:39:41.000Z",
                "type": "session_meta",
                "payload": {
                    "id": "01a18550-1b11-7882-82b6-ea40058ed587",
                    "cwd": "/home/lmcfarlin/Projects/Artificial-Humanity"
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-10-29T01:39:41.050Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "You are Codex, an agent based on GPT-6."}]
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-10-29T01:39:41.100Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "# AGENTS.md instructions for /home/lmcfarlin/Projects/Artificial-Humanity"}]
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-10-29T01:40:14.794Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "You are Katie. Please review the agent instructions and register as assistant to Kayla."}]
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-10-29T01:40:20.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "I'll read AGENTS.md and register as Katie."}]
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-10-29T01:40:20.500Z",
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "input": "agent-msg register Katie --project Kaggle-Gemma4 --role assistant"
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-10-29T01:40:21.000Z",
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call_output",
                    "output": [{"type": "input_text", "text": "Registered Katie <katie@artificialhumanity.io>"}]
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-10-29T01:40:25.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Registered as Katie and checked in with Kayla."}]
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-10-29T01:40:26.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "total_tokens": 65560
                        },
                        "model_context_window": 828400
                    }
                }
            })
            .to_string(),
        ]
        .join("\n");
        fs::write(&rollout_path, &jsonl).unwrap();

        let sessions = discover_sessions_with_home(&store, Some(home));
        let katie_sess = sessions
            .iter()
            .find(|s| s.persona.as_deref() == Some("katie"))
            .expect("katie codex session should be discovered");
        assert_eq!(katie_sess.adapter, "codex");
        assert_eq!(katie_sess.id, "01a18550-1b11-7882-82b6-ea40058ed587");

        let turns = read_session_transcript_with_home("katie", &store, Some(home));
        assert_eq!(turns.len(), 6);
        assert_eq!(turns[0].role, "system");
        assert_eq!(turns[1].role, "system");
        assert_eq!(turns[2].role, "user");
        assert_eq!(turns[3].role, "assistant");
        assert_eq!(turns[3].tool_calls.len(), 1);
        assert_eq!(turns[4].role, "tool");
        assert_eq!(turns[5].role, "assistant");
        assert!(!is_transcript_busy(&turns));

        let usage = read_session_context_usage_with_home("katie", &store, Some(home))
            .expect("codex context usage should be read from rollout + config.toml");
        assert_eq!(
            usage,
            SessionContextUsage {
                used_tokens: 65560,
                max_tokens: 872000,
            }
        );
    }

    #[test]
    fn antigravity_planner_response_context_usage_uses_latest_nonzero_sum() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let store = fl_store::open_redb(&home.join("store.redb")).unwrap();
        let conv = "conv-agy-ctx";
        let logs = home
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join(conv)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        let path = logs.join("transcript.jsonl");
        let jsonl = [
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "input_tokens": 10,
                "cache_read_tokens": 1,
                "output_tokens": 2
            })
            .to_string(),
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "input_tokens": 0,
                "cache_read_tokens": 0,
                "output_tokens": 0
            })
            .to_string(),
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "input_tokens": 100,
                "cache_read_tokens": 20,
                "output_tokens": 5
            })
            .to_string(),
            serde_json::json!({
                "type": "USER_INPUT",
                "content": "next",
                "input_tokens": 9999
            })
            .to_string(),
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "input_tokens": 0,
                "cache_read_tokens": 0,
                "output_tokens": 0
            })
            .to_string(),
        ]
        .join("\n");
        fs::write(&path, jsonl).unwrap();

        let usage = read_session_context_usage_with_home(conv, &store, Some(home))
            .expect("latest non-zero planner response");
        assert_eq!(
            usage,
            SessionContextUsage {
                used_tokens: 125,
                max_tokens: 1_000_000,
            }
        );

        fs::write(
            &path,
            serde_json::json!({
                "type": "PLANNER_RESPONSE",
                "content": "done"
            })
            .to_string(),
        )
        .unwrap();
        assert!(read_session_context_usage_with_home(conv, &store, Some(home)).is_none());
    }

    #[test]
    fn parse_jsonl_transcript_drops_turns_before_antigravity_step_rewind() {
        let jsonl = [
            serde_json::json!({
                "step_index": 0,
                "type": "USER_INPUT",
                "content": "keep"
            }),
            serde_json::json!({
                "step_index": 1,
                "type": "PLANNER_RESPONSE",
                "content": "old"
            }),
            serde_json::json!({
                "step_index": 27,
                "type": "PLANNER_RESPONSE",
                "content": "older"
            }),
            serde_json::json!({
                "step_index": 1,
                "type": "USER_INPUT",
                "content": "fresh"
            }),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].index, 0);
        assert_eq!(turns[0].content, "keep");
        assert_eq!(turns[1].index, 1);
        assert_eq!(turns[1].role, "user");
        assert_eq!(turns[1].content, "fresh");

        let reset = [
            serde_json::json!({"step_index": 0, "type": "USER_INPUT", "content": "first"}),
            serde_json::json!({"step_index": 3, "type": "PLANNER_RESPONSE", "content": "mid"}),
            serde_json::json!({"step_index": 0, "type": "USER_INPUT", "content": "reset"}),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = parse_jsonl_transcript(&reset);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "reset");
        assert_eq!(turns[0].index, 0);
    }

    #[test]
    fn antigravity_context_ignores_tokens_before_step_rewind() {
        let stale = [
            serde_json::json!({
                "step_index": 0,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 10
            }),
            serde_json::json!({
                "step_index": 27,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 50000
            }),
            serde_json::json!({
                "step_index": 1,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 0,
                "output_tokens": 0
            }),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert!(antigravity_context_from_transcript(&stale).is_none());

        let fresh = [
            serde_json::json!({
                "step_index": 27,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 50000
            }),
            serde_json::json!({
                "step_index": 1,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 4
            }),
            serde_json::json!({
                "step_index": 2,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 7,
                "cache_read_tokens": 1,
                "output_tokens": 1
            }),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert_eq!(
            antigravity_context_from_transcript(&fresh),
            Some(SessionContextUsage {
                used_tokens: 9,
                max_tokens: 1_000_000,
            })
        );
    }

    #[test]
    fn parse_jsonl_transcript_rewinds_on_token_only_antigravity_planner_step() {
        let jsonl = [
            serde_json::json!({"step_index": 0, "type": "USER_INPUT", "content": "keep"}),
            serde_json::json!({"step_index": 27, "type": "PLANNER_RESPONSE", "content": "older"}),
            serde_json::json!({"step_index": 1, "type": "PLANNER_RESPONSE", "input_tokens": 0}),
            serde_json::json!({"step_index": 2, "type": "PLANNER_RESPONSE", "content": "next"}),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].index, 0);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].content, "keep");
        assert_eq!(turns[1].index, 1);
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].content, "next");

        let empty_step_zero = [
            serde_json::json!({"step_index": 0, "type": "USER_INPUT", "content": "keep"}),
            serde_json::json!({"step_index": 4, "type": "PLANNER_RESPONSE", "content": "mid"}),
            serde_json::json!({"step_index": 0, "type": "PLANNER_RESPONSE", "input_tokens": 0}),
            serde_json::json!({"step_index": 5, "type": "USER_INPUT", "content": "later"}),
            serde_json::json!({"step_index": 1, "type": "USER_INPUT", "content": "fresh"}),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = parse_jsonl_transcript(&empty_step_zero);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "fresh");
        assert_eq!(turns[0].index, 0);
    }

    #[test]
    fn parse_jsonl_transcript_keeps_repeated_antigravity_steps_above_one() {
        let jsonl = [
            serde_json::json!({"step_index": 0, "type": "USER_INPUT", "content": "keep"}),
            serde_json::json!({"step_index": 1, "type": "PLANNER_RESPONSE", "content": "first"}),
            serde_json::json!({"step_index": 37, "type": "GENERIC", "content": "bg-a"}),
            serde_json::json!({"step_index": 37, "type": "GENERIC", "content": "bg-b"}),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = parse_jsonl_transcript(&jsonl);
        assert_eq!(turns.len(), 4);
        assert_eq!(turns[0].content, "keep");
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].content, "first");
        assert_eq!(turns[2].content, "bg-a");
        assert_eq!(turns[2].role, "tool");
        assert_eq!(turns[3].content, "bg-b");
        assert_eq!(turns[3].role, "tool");

        let repeat_one = [
            serde_json::json!({"step_index": 0, "type": "USER_INPUT", "content": "keep"}),
            serde_json::json!({"step_index": 1, "type": "PLANNER_RESPONSE", "content": "old"}),
            serde_json::json!({"step_index": 1, "type": "USER_INPUT", "content": "fresh"}),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = parse_jsonl_transcript(&repeat_one);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].content, "keep");
        assert_eq!(turns[1].content, "fresh");
    }

    #[test]
    fn antigravity_context_repeated_step_does_not_split_and_rewind_still_drops_prior_tokens() {
        let repeated = [
            serde_json::json!({
                "step_index": 36,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 15
            }),
            serde_json::json!({
                "step_index": 37,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 0
            }),
            serde_json::json!({
                "step_index": 37,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 0
            }),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert_eq!(
            antigravity_context_from_transcript(&repeated),
            Some(SessionContextUsage {
                used_tokens: 15,
                max_tokens: 1_000_000,
            })
        );

        let forward = [
            serde_json::json!({
                "step_index": 0,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 4
            }),
            serde_json::json!({
                "step_index": 1,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 0
            }),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert_eq!(
            antigravity_context_from_transcript(&forward),
            Some(SessionContextUsage {
                used_tokens: 4,
                max_tokens: 1_000_000,
            })
        );

        let rewind = [
            serde_json::json!({
                "step_index": 27,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 50000
            }),
            serde_json::json!({
                "step_index": 1,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 0
            }),
            serde_json::json!({
                "step_index": 37,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 0
            }),
            serde_json::json!({
                "step_index": 37,
                "type": "PLANNER_RESPONSE",
                "input_tokens": 0
            }),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert!(antigravity_context_from_transcript(&rewind).is_none());
    }

    #[test]
    fn codex_rate_limits_map_to_session_quota_usage_latest_event_wins() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let thread = "01a18550-1b11-7882-82b6-ea40058ed587";
        let dir = home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("10")
            .join("29");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-10-29T01-39-41-{thread}.jsonl"));
        let line = |primary: f64, secondary: f64, reached: Option<&str>, plan: &str| {
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "rate_limits": {
                        "plan_type": plan,
                        "rate_limit_reached_type": reached,
                        "primary": {
                            "used_percent": primary,
                            "window_minutes": 300,
                            "resets_at": 1_700_000_000_i64
                        },
                        "secondary": {
                            "used_percent": secondary,
                            "window_minutes": 10080,
                            "resets_at": 1_700_086_400_i64
                        }
                    }
                }
            })
            .to_string()
        };
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                line(1.0, 2.0, Some("stale"), "free"),
                line(12.5, 40.0, None, "plus")
            ),
        )
        .unwrap();

        let quota = read_session_quota_usage_with_home(thread, Some(home)).expect("codex quota");
        assert_eq!(quota.tier.as_deref(), Some("plus"));
        assert!(quota.exhausted_reason.is_none());
        assert_eq!(
            quota.primary_window,
            Some(QuotaWindowUsage {
                used_percent: 12.5,
                window_minutes: Some(300),
                resets_at_ms: Some(1_700_000_000_000),
            })
        );
        assert_eq!(
            quota.secondary_window,
            Some(QuotaWindowUsage {
                used_percent: 40.0,
                window_minutes: Some(10_080),
                resets_at_ms: Some(1_700_086_400_000),
            })
        );

        fs::write(
            &path,
            format!("{}\n", line(9.0, 9.0, Some("primary"), "plus")),
        )
        .unwrap();
        let blocked = read_session_quota_usage_with_home(thread, Some(home)).expect("exhausted");
        assert_eq!(blocked.exhausted_reason.as_deref(), Some("primary"));
    }

    #[test]
    fn codex_prolite_plan_is_shown_as_pro() {
        for raw in ["prolite", "ProLite", "pro_lite", "PRO-LITE"] {
            assert_eq!(normalize_codex_plan(raw), "pro", "{raw}");
        }
        for raw in ["pro", "plus", "free", "team"] {
            assert_eq!(normalize_codex_plan(raw), raw);
        }
        let rollout = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "plan_type": "prolite",
                    "primary": {
                        "used_percent": 5.0,
                        "window_minutes": 300,
                        "resets_at": 1_700_000_000_i64
                    }
                }
            }
        })
        .to_string();
        let quota = codex_quota_from_rollout(&format!("{rollout}\n")).expect("rollout quota");
        assert_eq!(quota.tier.as_deref(), Some("pro"));
    }

    #[cfg(unix)]
    #[test]
    fn codex_auth_override_directory_swap_never_reads_outside_home() {
        use base64::Engine as _;
        use std::os::unix::fs::symlink;
        let home = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let dir = home.path().join(".codex");
        fs::create_dir(&dir).unwrap();
        let make_auth = |plan: &str| {
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                serde_json::json!({"https://api.openai.com/auth":{"chatgpt_plan_type":plan}})
                    .to_string(),
            );
            serde_json::json!({"tokens":{"id_token":format!("e30.{payload}.sig")}}).to_string()
        };
        fs::write(dir.join("auth.json"), make_auth("plus")).unwrap();
        fs::write(outside.path().join("auth.json"), make_auth("enterprise")).unwrap();
        let original = dir.clone();
        let parked = home.path().join("old-codex");
        let target = outside.path().to_owned();
        BEFORE_CODEX_AUTH_OPEN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::rename(&original, &parked).unwrap();
                symlink(&target, &original).unwrap();
            }))
        });
        assert_ne!(
            codex_auth_plan(Some(home.path())).as_deref(),
            Some("enterprise")
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_session_and_matrix_account_use_their_own_homes() {
        use base64::Engine as _;
        const CHILD: &str = "FL_TEST_CODEX_HOME_BINDING_CHILD";
        if let Ok(rollout) = std::env::var(CHILD) {
            let session = DiscoveredSessionSummary {
                id: "bound".into(),
                source: "route".into(),
                adapter: "codex".into(),
                persona: None,
                status: "active".into(),
                updated_at: None,
                cwd: None,
                record_id: None,
                transcript_path: Some(rollout),
                quota_usage: None,
                model: None,
                thinking_level: None,
                available_models: vec![],
                context_usage: None,
                execution_mode: None,
            };
            let source = crate::runtime_matrix::QuotaSource::HOST;
            let quota = (source.session)(&session, None).unwrap();
            assert_eq!(quota.tier.as_deref(), Some("plus"));
            assert_eq!(quota.primary_window.unwrap().used_percent, 7.0);
            assert_eq!(
                (source.account)("codex", None).unwrap().tier.as_deref(),
                Some("enterprise")
            );
            return;
        }
        let account_a = TempDir::new().unwrap();
        let account_b = TempDir::new().unwrap();
        for (home, plan) in [(account_a.path(), "plus"), (account_b.path(), "enterprise")] {
            fs::create_dir_all(home.join(".codex")).unwrap();
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                serde_json::json!({"https://api.openai.com/auth":{"chatgpt_plan_type":plan}})
                    .to_string(),
            );
            fs::write(
                home.join(".codex/auth.json"),
                serde_json::json!({"tokens":{"id_token":format!("e30.{payload}.sig")}}).to_string(),
            )
            .unwrap();
        }
        let rollout = write_codex_rollout(account_a.path(), "bound", &serde_json::json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{"plan_type":"free","primary":{"used_percent":7.0}}}}).to_string());
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "discovery::tests::codex_session_and_matrix_account_use_their_own_homes",
            ])
            .env(CHILD, rollout)
            .env("HOME", account_a.path())
            .env("CODEX_HOME", account_b.path().join(".codex"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_session_never_uses_auth_from_an_unrelated_home() {
        use base64::Engine as _;
        let account_a = TempDir::new().unwrap();
        let account_b = TempDir::new().unwrap();
        for (home, plan) in [(account_a.path(), "plus"), (account_b.path(), "pro")] {
            fs::create_dir_all(home.join(".codex/sessions")).unwrap();
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                serde_json::json!({"https://api.openai.com/auth":{"chatgpt_plan_type":plan}})
                    .to_string(),
            );
            fs::write(
                home.join(".codex/auth.json"),
                serde_json::json!({"tokens":{"id_token":format!("e30.{payload}.sig")}}).to_string(),
            )
            .unwrap();
        }
        let id = "foreign-session";
        let rollout = account_b
            .path()
            .join(".codex/sessions/foreign-session.jsonl");
        fs::write(&rollout, serde_json::json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{"plan_type":"pro","primary":{"used_percent":7.0}}}}).to_string()).unwrap();
        let quota = quota_for_resolved_route("codex", rollout.to_str(), Some(account_a.path()), id)
            .unwrap();
        assert_eq!(
            quota.tier.as_deref(),
            Some("pro"),
            "unrelated A auth must not override B rollout"
        );
        assert_eq!(quota.primary_window.unwrap().used_percent, 7.0);
        assert!(
            quota_for_resolved_route("codex", None, Some(account_a.path()), id).is_none(),
            "an unbound route must not inherit a plan"
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_auth_plan_overrides_stale_rollout_without_changing_windows() {
        use base64::Engine;
        let home = TempDir::new().unwrap();
        let codex = home.path().join(".codex");
        fs::create_dir_all(&codex).unwrap();
        let claims =
            serde_json::json!({"https://api.openai.com/auth": {"chatgpt_plan_type": "pro"}});
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        fs::write(codex.join("auth.json"), serde_json::json!({"tokens": {"id_token": format!("e30.{payload}.signature"), "access_token": "secret-token"}}).to_string()).unwrap();
        let rollout = PathBuf::from(write_codex_rollout(home.path(), "bound-session", &serde_json::json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{"plan_type":"plus","primary":{"used_percent":7.0,"resets_at":100}}}}).to_string()));
        let quota =
            quota_for_resolved_route("codex", rollout.to_str(), Some(home.path()), "s").unwrap();
        assert_eq!(quota.tier.as_deref(), Some("pro"));
        assert!(
            !serde_json::to_string(&quota)
                .unwrap()
                .contains("secret-token")
        );
        assert_eq!(quota.primary_window.unwrap().used_percent, 7.0);
    }

    #[cfg(unix)]
    #[test]
    fn codex_auth_invalid_or_missing_falls_back_and_rotation_is_live() {
        use base64::Engine;
        let home = TempDir::new().unwrap();
        let dir = home.path().join(".codex");
        fs::create_dir_all(&dir).unwrap();
        let auth = dir.join("auth.json");
        let rollout = PathBuf::from(write_codex_rollout(home.path(), "bound-session", &serde_json::json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{"plan_type":"plus"}}}).to_string()));
        let tier = || {
            quota_for_resolved_route("codex", rollout.to_str(), Some(home.path()), "s")
                .unwrap()
                .tier
                .unwrap()
        };
        assert_eq!(tier(), "plus");
        for invalid in [
            "not json",
            "{}",
            "{\"tokens\":{\"id_token\":\"e30.!!!.sig\"}}",
        ] {
            fs::write(&auth, invalid).unwrap();
            assert_eq!(tier(), "plus");
        }
        fs::write(&auth, vec![b'x'; 256 * 1024 + 1]).unwrap();
        assert_eq!(tier(), "plus");
        for (plan, expected) in [("prolite", "pro"), ("team", "team")] {
            let claims =
                serde_json::json!({"https://api.openai.com/auth":{"chatgpt_plan_type":plan}});
            let payload =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
            fs::write(
                &auth,
                serde_json::json!({"tokens":{"id_token":format!("e30.{payload}.sig")}}).to_string(),
            )
            .unwrap();
            assert_eq!(tier(), expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn codex_auth_plan_reaches_by_id_and_account_quota() {
        use base64::Engine;
        let home = TempDir::new().unwrap();
        let id = "abc123";
        let event = serde_json::json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{"plan_type":"plus","primary":{"used_percent":3.0}}}});
        write_codex_rollout(home.path(), id, &event.to_string());
        let claims = serde_json::json!({"https://api.openai.com/auth":{"chatgpt_plan_type":"pro"}});
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        fs::write(
            home.path().join(".codex/auth.json"),
            serde_json::json!({"tokens":{"id_token":format!("e30.{payload}.sig")}}).to_string(),
        )
        .unwrap();
        let quota = read_session_quota_usage_with_home(id, Some(home.path())).unwrap();
        assert_eq!(quota.tier.as_deref(), Some("pro"));
        assert_eq!(quota.primary_window.unwrap().used_percent, 3.0);
        let account = codex_account_quota(Some(home.path())).unwrap();
        assert_eq!(account.tier.as_deref(), Some("pro"));
        assert!(account.primary_window.is_none());
    }

    #[cfg(not(unix))]
    #[test]
    fn sandboxed_auth_without_anchored_open_fails_closed() {
        let home = TempDir::new().unwrap();
        fs::create_dir(home.path().join(".codex")).unwrap();
        fs::write(home.path().join(".codex/auth.json"), "{}").unwrap();
        assert!(codex_auth_plan(Some(home.path())).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn codex_auth_fifo_is_refused_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let home = TempDir::new().unwrap();
        fs::create_dir(home.path().join(".codex")).unwrap();
        let path = home.path().join(".codex/auth.json");
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a valid NUL-terminated path, owned for this call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (send, recv) = std::sync::mpsc::channel();
        let root = home.path().to_path_buf();
        std::thread::spawn(move || send.send(codex_auth_plan(Some(&root))).unwrap());
        assert_eq!(
            recv.recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            None
        );
    }

    #[test]
    fn codex_auth_path_honors_explicit_home_and_isolates_overrides() {
        use std::ffi::OsStr;
        let home = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        fs::create_dir(home.path().join(".codex")).unwrap();
        let configured = other.path().as_os_str();
        assert_eq!(
            codex_auth_path(None, Some(configured), Some(home.path().as_os_str())),
            Some(other.path().join("auth.json"))
        );
        assert_eq!(
            codex_auth_path(None, None, Some(home.path().as_os_str())),
            Some(home.path().join(".codex/auth.json"))
        );
        assert!(
            codex_auth_path(
                None,
                Some(OsStr::new("relative")),
                Some(home.path().as_os_str())
            )
            .is_none()
        );
        assert!(codex_auth_path(None, None, Some(OsStr::new("relative"))).is_none());
        assert_eq!(
            codex_auth_path(Some(home.path()), Some(configured), None),
            Some(home.path().join(".codex/auth.json"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_auth_does_not_follow_symlink_or_escape_sandbox() {
        use std::os::unix::fs::symlink;
        let home = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        symlink(outside.path(), home.path().join(".codex")).unwrap();
        assert!(codex_auth_plan(Some(home.path())).is_none());
        fs::remove_file(home.path().join(".codex")).unwrap();
        fs::create_dir(home.path().join(".codex")).unwrap();
        fs::write(outside.path().join("auth.json"), "{}").unwrap();
        symlink(
            outside.path().join("auth.json"),
            home.path().join(".codex/auth.json"),
        )
        .unwrap();
        assert!(codex_auth_plan(Some(home.path())).is_none());
    }

    #[test]
    fn grok_billing_log_maps_weekly_window_and_ignores_other_vendors() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let session = "01a18538-6f1a-79c2-99d2-8e8540366c72";
        let session_dir = home.join(".grok").join("sessions").join(session);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("chat_history.jsonl"), "{}\n").unwrap();
        let logs = home.join(".grok").join("logs");
        fs::create_dir_all(&logs).unwrap();
        let end = "2026-11-05T01:11:25.780+00:00";
        let expected_ms = fl_core::At::parse("2026-11-05T01:11:25.780Z")
            .unwrap()
            .unix_millis();
        let line = serde_json::json!({
            "msg": "billing: fetched credits config",
            "ctx": {
                "subscriptionTier": "SuperGrok",
                "config": {
                    "creditUsagePercent": 1.0,
                    "currentPeriod": {
                        "type": "USAGE_PERIOD_TYPE_WEEKLY",
                        "end": end
                    },
                    "billingPeriodEnd": "2026-12-01T00:00:00Z"
                }
            }
        });
        fs::write(logs.join("unified.jsonl"), format!("{line}\n")).unwrap();

        let quota = read_session_quota_usage_with_home(session, Some(home)).expect("grok quota");
        assert_eq!(quota.tier.as_deref(), Some("SuperGrok"));
        assert!(quota.primary_window.is_none());
        assert!(quota.exhausted_reason.is_none());
        assert_eq!(
            quota.secondary_window,
            Some(QuotaWindowUsage {
                used_percent: 1.0,
                window_minutes: Some(10_080),
                resets_at_ms: Some(expected_ms),
            })
        );

        let claude = home.join(".claude").join("projects").join("demo");
        fs::create_dir_all(&claude).unwrap();
        fs::write(claude.join("claude-session.jsonl"), "{}\n").unwrap();
        assert!(read_session_quota_usage_with_home("claude-session", Some(home)).is_none());
    }

    #[test]
    fn muse_subscription_exhaustion_clears_after_model_completed() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let session = "muse-session-1";
        let dir = home
            .join(".local")
            .join("share")
            .join("muse")
            .join("sessions")
            .join(session);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let reason = "API error 429: Subscription quota exhausted. Your usage window resets at 2026-11-02T00:00:00Z.";
        let failure = serde_json::json!({
            "payload_type": "runtime.session",
            "payload": {
                "event": {
                    "kind": "model_failure_terminal_intent",
                    "reason": reason
                }
            }
        });
        let failed = serde_json::json!({
            "payload_type": "runtime.session",
            "payload": {
                "event": {
                    "kind": "failed",
                    "reason": reason
                }
            }
        });
        fs::write(&path, format!("{failure}\n{failed}\n")).unwrap();
        let expected_ms = fl_core::At::parse("2026-11-02T00:00:00.000Z")
            .unwrap()
            .unix_millis();
        let quota = read_session_quota_usage_with_home(session, Some(home)).expect("muse quota");
        assert_eq!(quota.exhausted_reason.as_deref(), Some(reason));
        assert!(quota.tier.is_none());
        assert!(quota.secondary_window.is_none());
        assert_eq!(
            quota.primary_window,
            Some(QuotaWindowUsage {
                used_percent: 100.0,
                window_minutes: None,
                resets_at_ms: Some(expected_ms),
            })
        );

        let completed = serde_json::json!({
            "payload_type": "runtime.session",
            "payload": { "event": { "kind": "model_completed" } }
        });
        fs::write(&path, format!("{failure}\n{completed}\n")).unwrap();
        assert!(read_session_quota_usage_with_home(session, Some(home)).is_none());
    }

    #[test]
    fn resolved_transcript_path_supplies_quota_without_searching_home() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let logs = home.join(".grok").join("logs");
        fs::create_dir_all(&logs).unwrap();
        let line = serde_json::json!({
            "msg": "billing: fetched credits config",
            "ctx": {
                "subscriptionTier": "SuperGrok",
                "config": { "creditUsagePercent": 1.0 }
            }
        });
        fs::write(logs.join("unified.jsonl"), format!("{line}\n")).unwrap();
        assert!(quota_for_resolved_route("grok", None, Some(home), "s").is_none());

        let quota = quota_for_resolved_route("grok", Some("already-resolved"), Some(home), "s")
            .expect("resolved grok path");
        assert_eq!(quota.tier.as_deref(), Some("SuperGrok"));

        let rollout = tmp.path().join("not-under-codex.jsonl");
        let event = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "plan_type": "plus",
                    "primary": { "used_percent": 3.0, "resets_at": 10 },
                    "secondary": { "used_percent": 4.0, "resets_at": 20 }
                }
            }
        });
        fs::write(&rollout, format!("{event}\n")).unwrap();
        let codex =
            quota_for_resolved_route("codex", Some(rollout.to_str().unwrap()), Some(home), "s")
                .expect("resolved codex path");
        assert_eq!(codex.tier.as_deref(), Some("plus"));
        assert_eq!(codex.primary_window.unwrap().window_minutes, Some(300));
        assert!(quota_for_resolved_route("codex", None, Some(home), "s").is_none());
    }

    #[test]
    fn production_snapshot_probes_muse_on_path_when_home_override_is_none() {
        crate::direct_quota::with_probe_lock(|| {
            struct RestoreEnv {
                home: Option<std::ffi::OsString>,
                path: Option<std::ffi::OsString>,
                xdg: Option<std::ffi::OsString>,
                disable_host: Option<std::ffi::OsString>,
                muse_bin: Option<std::ffi::OsString>,
                claude_bin: Option<std::ffi::OsString>,
                claude_usage: Option<std::ffi::OsString>,
                agy: Option<std::ffi::OsString>,
            }

            impl RestoreEnv {
                fn capture() -> Self {
                    Self {
                        home: std::env::var_os("HOME"),
                        path: std::env::var_os("PATH"),
                        xdg: std::env::var_os("XDG_RUNTIME_DIR"),
                        disable_host: std::env::var_os("FL_WIRE_DISABLE_HOST_DISCOVERY"),
                        muse_bin: std::env::var_os("FL_HUB_MUSE_BIN"),
                        claude_bin: std::env::var_os("FL_HUB_CLAUDE_BIN"),
                        claude_usage: std::env::var_os("FL_CLAUDE_USAGE_JSON"),
                        agy: std::env::var_os("FL_AGY_QUOTA_JSON"),
                    }
                }

                fn restore_one(key: &str, value: &Option<std::ffi::OsString>) {
                    // SAFETY: PROBE_LOCK is held, and this guard restores the
                    // previous value before that lock is released.
                    unsafe {
                        match value {
                            Some(value) => std::env::set_var(key, value),
                            None => std::env::remove_var(key),
                        }
                    }
                }
            }

            impl Drop for RestoreEnv {
                fn drop(&mut self) {
                    Self::restore_one("HOME", &self.home);
                    Self::restore_one("PATH", &self.path);
                    Self::restore_one("XDG_RUNTIME_DIR", &self.xdg);
                    Self::restore_one("FL_WIRE_DISABLE_HOST_DISCOVERY", &self.disable_host);
                    Self::restore_one("FL_HUB_MUSE_BIN", &self.muse_bin);
                    Self::restore_one("FL_HUB_CLAUDE_BIN", &self.claude_bin);
                    Self::restore_one("FL_CLAUDE_USAGE_JSON", &self.claude_usage);
                    Self::restore_one("FL_AGY_QUOTA_JSON", &self.agy);
                }
            }

            use std::os::unix::fs::PermissionsExt;

            let _restore = RestoreEnv::capture();
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();
            let session_id = "muse-direct-probe-6f0a9c2e";
            let routes = format!(
                r#"{{"entries":{{"muse-persona":{{"persona":"muse-persona","updated_at":"2026-10-29T00:00:00.000Z","route":{{"kind":"muse","session_id":"{session_id}"}}}}}}}}"#
            );
            fs::write(root.join("fl-wire-routes.json"), routes).unwrap();

            let bin_dir = root.join("bin");
            fs::create_dir_all(&bin_dir).unwrap();
            let counter_path = root.join("muse-count");
            let script = bin_dir.join("muse");
            fs::write(
                &script,
                format!(
                    "#!/bin/sh\nf=\"{}\"\nn=0\nif [ -f \"$f\" ]; then n=$(cat \"$f\"); fi\nn=$((n+1))\nprintf '%s\\n' \"$n\" > \"$f\"\nprintf '%s\\n' '{{\"allowance\":null}}'\n",
                    counter_path.display()
                ),
            )
            .unwrap();
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();

            let mut path_dirs = vec![bin_dir];
            if let Some(path) = std::env::var_os("PATH") {
                path_dirs.extend(std::env::split_paths(&path));
            }
            let path = std::env::join_paths(&path_dirs).unwrap();
            // SAFETY: PROBE_LOCK is held, and RestoreEnv restores HOME, PATH,
            // and the probe variables before that lock is released.
            unsafe {
                std::env::set_var("HOME", root);
                std::env::set_var("XDG_RUNTIME_DIR", root);
                std::env::set_var("PATH", &path);
                std::env::set_var("FL_WIRE_DISABLE_HOST_DISCOVERY", "1");
                std::env::remove_var("FL_HUB_MUSE_BIN");
                std::env::remove_var("FL_HUB_CLAUDE_BIN");
                std::env::remove_var("FL_CLAUDE_USAGE_JSON");
                std::env::remove_var("FL_AGY_QUOTA_JSON");
            }
            crate::direct_quota::expire_direct_quota_cache();

            let sessions =
                discover_sessions_from_snapshot(&DiscoveryStoreSnapshot::default(), None);
            assert_eq!(
                sessions
                    .iter()
                    .filter(|session| session.id == session_id)
                    .count(),
                1
            );
            let hits = fs::read_to_string(&counter_path)
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok())
                .unwrap_or(0);
            assert_eq!(hits, 1);
        });
    }

    // ---- audit 2026-10-30 regressions ----

    fn write_transcript(home: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = home.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn user_line(text: &str) -> String {
        serde_json::json!({ "role": "user", "content": text }).to_string()
    }

    #[test]
    fn utc_offset_with_multibyte_text_is_rejected_not_a_panic() {
        // `+1é1` is four bytes after the sign, so a byte split at 2 lands inside `é`.
        for raw in [
            "2026-01-01T00:00:00+1\u{e9}1",
            "2026-01-01T00:00:00-\u{e9}\u{e9}",
            "2026-01-01T00:00:00+\u{1f600}",
            "2026-01-01T00:00:00+\u{ff11}\u{ff12}",
        ] {
            assert_eq!(parse_iso8601_unix_millis(raw), None, "{raw:?}");
        }
        assert_eq!(parse_utc_offset_minutes("+0530"), Some(330));
        assert_eq!(parse_utc_offset_minutes("-05:30"), Some(-330));
        // The same input reaches the parser through a Muse reset hint.
        assert_eq!(
            parse_reset_instant_from_reason("quota resets at 2026-01-01T00:00:00+1\u{e9}1 soon"),
            None
        );
    }

    #[test]
    fn token_sums_saturate_instead_of_overflowing() {
        let max = u64::MAX;
        // Antigravity: a float token count saturates to `u64::MAX`, and three of them add up.
        let agy = format!(
            "{}\n",
            serde_json::json!({
                "step_index": 1, "type": "PLANNER_RESPONSE",
                "input_tokens": 1e30, "cache_read_tokens": max, "output_tokens": max
            })
        );
        assert_eq!(
            antigravity_context_from_transcript(&agy).map(|u| u.used_tokens),
            Some(max)
        );

        let home = TempDir::new().unwrap();
        let claude = serde_json::json!({
            "type": "assistant",
            "message": { "usage": {
                "input_tokens": max, "cache_creation_input_tokens": max,
                "cache_read_input_tokens": max, "output_tokens": 7
            } }
        });
        write_transcript(
            home.path(),
            ".claude/projects/p/claude-sat.jsonl",
            format!("{claude}\n").as_bytes(),
        );
        let snap = DiscoveryStoreSnapshot::default();
        let usage =
            read_session_context_usage_from_snapshot("claude-sat", &snap, Some(home.path()))
                .unwrap();
        assert_eq!(usage.used_tokens, max);

        let muse = serde_json::json!({
            "payload": { "event": { "kind": "model_completed",
                "usage": { "input_tokens": max, "output_tokens": max } } }
        });
        write_transcript(
            home.path(),
            ".local/share/muse/sessions/muse-sat/session.jsonl",
            format!("{muse}\n").as_bytes(),
        );
        let usage =
            read_session_context_usage_from_snapshot("muse-sat", &snap, Some(home.path())).unwrap();
        assert_eq!(usage.used_tokens, max);
    }

    #[test]
    fn session_ids_that_climb_out_of_the_vendor_dirs_find_no_transcript() {
        let home = TempDir::new().unwrap();
        // A readable `.jsonl` outside every vendor directory.
        write_transcript(
            home.path(),
            "outside/secret.jsonl",
            user_line("secret").as_bytes(),
        );
        fs::create_dir_all(home.path().join(".grok/sessions")).unwrap();
        let snap = DiscoveryStoreSnapshot::default();
        for id in [
            "../../outside/secret",
            "../outside/secret",
            "..",
            ".",
            "a/b",
            "a\\b",
            "x\0y",
        ] {
            assert_eq!(
                find_transcript_file_in_home(home.path(), id),
                None,
                "{id:?}"
            );
            assert!(
                read_session_transcript_from_snapshot(id, &snap, Some(home.path())).is_empty(),
                "{id:?}"
            );
            assert_eq!(
                read_session_context_usage_from_snapshot(id, &snap, Some(home.path())),
                None
            );
        }
        assert_eq!(
            find_transcript_file_in_home(home.path(), &"a".repeat(300)),
            None
        );
        assert!(is_safe_path_component("claude:ada"));
        assert!(is_safe_path_component(
            "0f1e2d3c-4b5a-6978-8a9b-0c1d2e3f4a5b"
        ));
        assert!(!is_safe_path_component(""));
    }

    #[test]
    fn an_antigravity_route_cannot_name_a_transcript_outside_the_brain_dir() {
        let home = TempDir::new().unwrap();
        write_transcript(
            home.path(),
            ".system_generated/logs/transcript.jsonl",
            user_line("secret").as_bytes(),
        );
        fs::create_dir_all(home.path().join(".gemini/antigravity-cli/brain")).unwrap();
        let mut routes = RouteTable::default();
        routes.upsert(
            "ada",
            VendorRoute::Agy {
                conversation_id: "../../..".to_string(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            fl_core::at::At::from_unix_millis(1),
        );
        let path = home.path().join(".cache/agent-bus/routes.json");
        routes.save(Some(&path)).unwrap();
        let found =
            discover_sessions_from_snapshot(&DiscoveryStoreSnapshot::default(), Some(home.path()));
        let agy = found.iter().find(|s| s.adapter == "antigravity").unwrap();
        assert_eq!(agy.transcript_path, None);
    }

    #[test]
    fn invalid_utf8_in_one_line_does_not_hide_the_transcript() {
        let home = TempDir::new().unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(user_line("before").as_bytes());
        bytes.extend_from_slice(b"\n{\"role\":\"user\",\"content\":\"bad \xff\xfe bytes\"}\n");
        bytes.extend_from_slice(user_line("after").as_bytes());
        bytes.push(b'\n');
        write_transcript(home.path(), ".claude/projects/p/utf8-sess.jsonl", &bytes);
        let turns = read_session_transcript_from_snapshot(
            "utf8-sess",
            &DiscoveryStoreSnapshot::default(),
            Some(home.path()),
        );
        let contents: Vec<&str> = turns.iter().map(|t| t.content.as_str()).collect();
        assert_eq!(contents.len(), 3, "{contents:?}");
        assert_eq!(contents[0], "before");
        assert!(contents[1].contains('\u{fffd}'));
        assert_eq!(contents[2], "after");
    }

    #[test]
    fn a_transcript_over_the_cap_is_read_from_its_tail_without_the_cut_line() {
        let dir = TempDir::new().unwrap();
        let path = write_transcript(dir.path(), "t.jsonl", b"aaaa\nbbbb\ncccc\n");
        assert_eq!(
            read_transcript_lossy(&path, 1024).unwrap(),
            "aaaa\nbbbb\ncccc\n"
        );
        // Starts inside `bbbb`: the cut-off first line goes, the whole lines stay.
        assert_eq!(read_transcript_lossy(&path, 8).unwrap(), "cccc\n");
        // One line longer than the cap leaves nothing, never half a line.
        let long = write_transcript(dir.path(), "long.jsonl", &[b'x'; 64]);
        assert_eq!(read_transcript_lossy(&long, 8).unwrap(), "");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_transcript_is_refused_instead_of_blocking_the_reader() {
        let dir = TempDir::new().unwrap();
        let fifo = dir.path().join("pipe.jsonl");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|s| s.success()) {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read_transcript_lossy(&fifo, 1024).is_err());
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)),
            Ok(true),
            "reading a FIFO must fail fast, not wait for a writer"
        );
        assert!(read_transcript_lossy(dir.path(), 1024).is_err());
    }

    #[test]
    fn an_oversized_line_is_skipped_and_its_neighbours_survive() {
        let huge = user_line(&"x".repeat(TRANSCRIPT_LINE_MAX_BYTES + 1));
        let text = format!("{}\n{huge}\n{}\n", user_line("one"), user_line("two"));
        let turns = parse_jsonl_transcript(&text);
        let contents: Vec<&str> = turns.iter().map(|t| t.content.as_str()).collect();
        assert_eq!(contents, ["one", "two"]);
        assert_eq!(turns[1].index, 1);
    }

    #[test]
    fn malformed_truncated_and_deeply_nested_lines_are_skipped() {
        let deep_array = "[".repeat(200_000);
        let deep_object = format!("{}1{}", "{\"a\":".repeat(500), "}".repeat(500));
        // `tool_result` content nested far past the JSON parser's depth limit.
        let deep_result = format!(
            "{{\"role\":\"user\",\"content\":{}\"x\"{}}}",
            "[{\"type\":\"tool_result\",\"content\":".repeat(300),
            "}]".repeat(300)
        );
        let deep_args = format!(
            "{{\"role\":\"assistant\",\"tool_calls\":[{{\"name\":\"t\",\"arguments\":\"{}\"}}]}}",
            "[".repeat(10_000)
        );
        let lines = [
            user_line("first"),
            deep_array,
            deep_object,
            deep_result,
            "{\"role\":\"user\",\"content\":\"trunc".to_string(),
            "null".to_string(),
            "\"just a string\"".to_string(),
            "[1,2,3]".to_string(),
            "{\"role\":\"user\"}".to_string(),
            "{\"role\":\"user\",\"content\":[null,1,[],{}]}".to_string(),
            deep_args,
            user_line("last"),
        ];
        let turns = parse_jsonl_transcript(&lines.join("\n"));
        let contents: Vec<&str> = turns.iter().map(|t| t.content.as_str()).collect();
        assert_eq!(contents.first(), Some(&"first"));
        assert_eq!(contents.last(), Some(&"last"));
        // Too-deep arguments stay a plain string instead of failing the turn.
        let call = turns.iter().flat_map(|t| &t.tool_calls).next().unwrap();
        assert!(call.arguments.is_string());
        for (i, turn) in turns.iter().enumerate() {
            assert_eq!(turn.index, i);
        }
    }

    #[test]
    fn nested_tool_result_content_below_the_depth_limit_still_flattens() {
        let depth = 40;
        let line = format!(
            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":{}\"deep\"{}}}}}",
            "[{\"type\":\"tool_result\",\"content\":".repeat(depth),
            "}]".repeat(depth)
        );
        let turns = parse_jsonl_transcript(&line);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "tool");
        assert_eq!(turns[0].content, "deep");
    }

    #[test]
    fn an_operator_message_that_quotes_a_system_message_tag_keeps_its_text() {
        let quoted = "Please explain this:\n<SYSTEM_MESSAGE>\nfoo\n</SYSTEM_MESSAGE>";
        let (role, content) = normalize_system_turn("user", quoted);
        assert_eq!(role, "user");
        assert_eq!(content, quoted);
        let turns = parse_jsonl_transcript(&user_line(quoted));
        assert_eq!(turns[0].role, "user");
        assert!(turns[0].content.starts_with("Please explain this:"));

        // The harness preamble in front of a real block is still dropped.
        let real = "The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n\
                    <SYSTEM_MESSAGE>\n[Notice] disk is full\n</SYSTEM_MESSAGE>";
        let (role, content) = normalize_system_turn("system", real);
        assert_eq!(role, "system");
        assert_eq!(content, "[Notice] disk is full");
        // An unterminated block is not a panic either.
        let (_, content) = normalize_system_turn("system", "x\n<SYSTEM_MESSAGE>\nno close");
        assert_eq!(content, "no close");
    }

    #[test]
    fn agent_message_tag_spellings_and_malformed_tags() {
        for open in ["<agent-message from=\"w\">", "<\\agent-message from=\"w\">"] {
            for close in [
                "</agent-message>",
                "<\\/agent-message>",
                "</\\agent-message>",
            ] {
                let (role, content) =
                    normalize_system_turn("user", &format!("{open}\nreport body\n{close}"));
                assert_eq!(role, "wire:subagent", "{open} {close}");
                assert_eq!(content, "report body");
            }
        }
        // Not a hand-back: tag name continues, tag never closes, or the tag is quoted mid-text.
        for text in [
            "<agent-messages>hi</agent-messages>",
            "<agent-message from=\"w\" never closed",
            "<agent-message",
            "see <agent-message from=\"w\">x</agent-message> above",
        ] {
            let (role, content) = normalize_system_turn("user", text);
            assert_eq!(role, "user", "{text:?}");
            assert_eq!(content, text);
        }
    }

    #[test]
    fn harness_notice_openers_only_count_at_the_start() {
        for opener in [
            "<task-notification>",
            "<local-command-caveat>",
            "<local-command-stdout>",
        ] {
            let (role, _) = normalize_system_turn("user", &format!("  {opener}\nx"));
            assert_eq!(role, "system", "{opener}");
            let quoted = format!("I saw {opener} in the log");
            assert_eq!(normalize_system_turn("user", &quoted).0, "user");
        }
    }

    #[test]
    fn agy_wrappers_handle_unclosed_nested_mixed_case_and_multibyte_text() {
        // Unclosed: left as it is.
        assert_eq!(
            strip_agy_user_request_wrapper("<USER_REQUEST>no close"),
            "<USER_REQUEST>no close"
        );
        // Mixed case and multibyte text around the tags keep the byte offsets right.
        assert_eq!(
            strip_agy_user_request_wrapper(
                "<User_Request>\u{130}\u{e9}\u{1f600} hi</USER_REQUEST>"
            ),
            "\u{130}\u{e9}\u{1f600} hi"
        );
        assert_eq!(
            strip_agy_user_request_wrapper(
                "<USER_REQUEST>a</USER_REQUEST>\u{e9}<ADDITIONAL_METADATA>m</ADDITIONAL_METADATA>"
            ),
            "a\u{e9}"
        );
        // Nested same-name tags: non-greedy, so the first close ends the block.
        assert_eq!(
            strip_agy_user_request_wrapper(
                "<USER_REQUEST><USER_REQUEST>a</USER_REQUEST>b</USER_REQUEST>"
            ),
            "<USER_REQUEST>ab</USER_REQUEST>"
        );
        // Text that only quotes a tag is untouched.
        assert_eq!(
            strip_agy_user_request_wrapper("say <USER_REQUEST>x</USER_REQUEST>"),
            "say <USER_REQUEST>x</USER_REQUEST>"
        );
        // A long run of opens with no close must stay linear.
        let many = "<user_request>".repeat(100_000);
        let started = std::time::Instant::now();
        assert_eq!(strip_agy_user_request_wrapper(&many), many);
        assert!(started.elapsed() < Duration::from_secs(5));
        // A closed ephemeral block is a system notice.
        let (role, content) = normalize_system_turn(
            "user",
            "<EPHEMERAL_MESSAGE>\nheads up\n</EPHEMERAL_MESSAGE>",
        );
        assert_eq!((role.as_str(), content.as_str()), ("system", "heads up"));
    }

    #[test]
    fn nats_and_wire_headers_with_odd_bytes_never_panic() {
        for text in [
            "[Incoming NATS Message",
            "[Incoming NATS Message\nFrom: \u{e9}\u{e9}\nSubject:\n\n",
            "[fl-wire ] from \u{1f600} (",
            "[fl-wire ] from ",
            "[fl-wire x",
            "[Message] sender=\u{e9} content=",
            "[Message] content=",
            "[Message]",
            "<SYSTEM_MESSAGE>\n[Message] sender=\"'\u{e9}\"' content=[fl-wire a] from \u{e9}: x\n</SYSTEM_MESSAGE>",
        ] {
            for role in ["user", "system"] {
                let _ = normalize_system_turn(role, text);
            }
        }
    }

    #[test]
    fn grok_quota_reads_only_the_tail_of_a_huge_log() {
        let home = TempDir::new().unwrap();
        let log = home.path().join(".grok/logs/unified.jsonl");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        let billing = |tier: &str| {
            serde_json::json!({
                "msg": "billing: fetched credits config",
                "ctx": { "subscriptionTier": tier, "config": { "creditUsagePercent": 12.5 } }
            })
            .to_string()
        };
        let mut body = format!("{}\n", billing("stale-tier"));
        let filler = format!(
            "{}\n",
            serde_json::json!({ "msg": "tick", "pad": "p".repeat(900) })
        );
        while (body.len() as u64) < QUOTA_LOG_TAIL_BYTES + 64 * 1024 {
            body.push_str(&filler);
        }
        fs::write(&log, &body).unwrap();
        // The only billing line is older than the tail window, so it is not read.
        assert_eq!(grok_quota_from_home(home.path()), None);
        body.push_str(&format!("{}\n", billing("fresh-tier")));
        fs::write(&log, body).unwrap();
        let quota = grok_quota_from_home(home.path()).unwrap();
        assert_eq!(quota.tier.as_deref(), Some("fresh-tier"));
    }

    fn register_context_vendor(id: &str, context_window: u64) {
        fl_core::ensure_vendor_registered(fl_core::VendorDescriptor {
            id: id.to_string(),
            display_name: format!("Custom {id}"),
            transport: fl_core::VendorTransport::HttpApi {
                default_base_url: "https://example.org/v1".to_string(),
                api_dialect: fl_core::ApiDialect::OpenAiChatCompletions,
                vault_secret_name: None,
                env_key_name: None,
            },
            default_model: "m".to_string(),
            default_context_window: context_window,
            default_rate_card: fl_core::telemetry::RateCard::default(),
            mcp_config_rel_path: None,
            slice_bounds: Vec::new(),
        })
        .unwrap();
    }

    #[test]
    fn registered_vendor_context_windows_are_the_fallback_before_the_generic_one() {
        register_context_vendor("disc-ctx-custom", 77_000);
        register_context_vendor("disc-ctx-zero", 0);
        // Built-in API vendors and local engines answer with their registered default.
        for (adapter, window) in [
            ("kimi-api", 256_000),
            ("openrouter-api", 128_000),
            ("openai-api", 400_000),
            ("ollama", 128_000),
            ("lemonade", 128_000),
            ("disc-ctx-custom", 77_000),
        ] {
            assert_eq!(
                registered_context_window(adapter),
                Some(window),
                "{adapter}"
            );
            assert_eq!(fallback_context_window(adapter), window, "{adapter}");
        }
        // No window registered (a CLI the CLI decides for, a window of 0) or no such vendor:
        // the generic default stays.
        for adapter in [
            "claude",
            "grok",
            "codex",
            "disc-ctx-zero",
            "no-such-adapter",
            "",
        ] {
            assert_eq!(registered_context_window(adapter), None, "{adapter}");
            assert_eq!(fallback_context_window(adapter), 500_000, "{adapter}");
        }
    }

    #[test]
    fn a_transcript_that_names_its_window_keeps_it_and_one_that_does_not_gets_the_fallback() {
        let home = TempDir::new().unwrap();
        let session_dir = home.path().join("grok-session");
        fs::create_dir_all(&session_dir).unwrap();
        let chat = session_dir.join("chat_history.jsonl");
        fs::write(&chat, "{}\n").unwrap();
        let summary = DiscoveredSessionSummary {
            id: "grok-session".into(),
            source: "route".into(),
            adapter: "grok".into(),
            persona: None,
            status: "active".into(),
            updated_at: None,
            cwd: None,
            record_id: None,
            transcript_path: Some(chat.display().to_string()),
            quota_usage: None,
            model: None,
            thinking_level: None,
            available_models: Vec::new(),
            context_usage: None,
            execution_mode: None,
        };
        let usage = |signals: serde_json::Value| {
            fs::write(session_dir.join("signals.json"), signals.to_string()).unwrap();
            context_usage_from_sessions(
                "grok-session",
                std::slice::from_ref(&summary),
                Some(home.path()),
                &mut TranscriptReads::default(),
            )
            .unwrap()
        };
        // The window the session reports always wins.
        let reported = usage(serde_json::json!({
            "contextTokensUsed": 10,
            "contextWindowTokens": 123_456
        }));
        assert_eq!((reported.used_tokens, reported.max_tokens), (10, 123_456));
        // Without one, the answer is unchanged for a vendor that registers no window.
        let unreported = usage(serde_json::json!({ "contextTokensUsed": 10 }));
        assert_eq!(
            (unreported.used_tokens, unreported.max_tokens),
            (10, 500_000)
        );
    }

    fn claude_user_row(content: Value) -> String {
        serde_json::json!({"type":"user","message":{"role":"user","content":content}}).to_string()
    }

    fn claude_user_turn(content: Value) -> (String, String) {
        let turns = parse_jsonl_transcript(&claude_user_row(content));
        assert_eq!(turns.len(), 1, "{turns:?}");
        (turns[0].role.clone(), turns[0].content.clone())
    }

    #[test]
    fn slash_command_wrapper_becomes_the_command_the_operator_typed() {
        let wrap = |inner: &str| format!("\n  {inner}\n");
        let cases: [(&str, &str); 9] = [
            (
                "<command-name>/compact</command-name>\n            <command-message>compact</command-message>\n            <command-args></command-args>",
                "/compact",
            ),
            (
                "<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args>focus on auth</command-args>",
                "/compact focus on auth",
            ),
            // Only the message tag: the name is the message with a slash.
            ("<command-message>compact</command-message>", "/compact"),
            // The slash is never doubled.
            ("<command-message>/compact</command-message>", "/compact"),
            // Reversed tag order.
            (
                "<command-message>compact</command-message><command-args>  now  </command-args><command-name>/compact</command-name>",
                "/compact now",
            ),
            // Whitespace-only args, and no args tag at all.
            (
                "<command-name>/clear</command-name><command-args>  \n </command-args>",
                "/clear",
            ),
            ("<command-name>/clear</command-name>", "/clear"),
            // A name without a slash is left as written.
            ("<command-name>compact</command-name>", "compact"),
            // An empty name falls back to the message.
            (
                "<command-name> </command-name><command-message>init</command-message>",
                "/init",
            ),
        ];
        for (wrapper, want) in cases {
            for text in [wrapper.to_string(), wrap(wrapper)] {
                let (role, content) = claude_user_turn(serde_json::json!(text));
                assert_eq!(
                    (role.as_str(), content.as_str()),
                    ("user", want),
                    "{text:?}"
                );
            }
        }
    }

    #[test]
    fn slash_command_wrapper_in_an_array_of_text_blocks() {
        let (role, content) = claude_user_turn(serde_json::json!([
            {"type": "text", "text": "<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args>keep the plan</command-args>"}
        ]));
        assert_eq!(
            (role.as_str(), content.as_str()),
            ("user", "/compact keep the plan")
        );
        // The tags may arrive in separate blocks.
        let (role, content) = claude_user_turn(serde_json::json!([
            {"type": "text", "text": "<command-name>/model</command-name>"},
            {"type": "text", "text": "<command-args>opus</command-args>"}
        ]));
        assert_eq!((role.as_str(), content.as_str()), ("user", "/model opus"));
    }

    #[test]
    fn text_that_is_not_a_whole_slash_command_wrapper_is_unchanged() {
        for text in [
            // An unterminated tag, in each position.
            "<command-name>/compact",
            "<command-name>/compact</command-name><command-args>never closed",
            "<command-message>compact",
            // A close tag only.
            "</command-name>/compact",
            // Nothing in either tag.
            "<command-name></command-name>",
            "<command-message> </command-message>",
            // Plain operator text, and text that merely contains the tag.
            "please run /compact now",
            "use <command-name>/compact</command-name> to shrink the context",
            "<command-nameX>/compact</command-nameX>",
        ] {
            let (role, content) = claude_user_turn(serde_json::json!(text));
            assert_eq!(
                (role.as_str(), content.as_str()),
                ("user", text),
                "{text:?}"
            );
        }
    }

    #[test]
    fn slash_command_wrapper_with_multibyte_and_huge_text_stays_cheap() {
        let (_, content) = claude_user_turn(serde_json::json!(
            "<command-name>/compact</command-name><command-args>\u{e9}\u{1f600} \u{130}</command-args>"
        ));
        assert_eq!(content, "/compact \u{e9}\u{1f600} \u{130}");
        // A very long run of opens with no close is a single pass, and unchanged.
        let many = "<command-name>".repeat(200_000);
        let started = std::time::Instant::now();
        assert_eq!(slash_command_text(&many), None);
        assert!(started.elapsed() < Duration::from_secs(5));
        let big_args = "x".repeat(1_000_000);
        let text =
            format!("<command-name>/compact</command-name><command-args>{big_args}</command-args>");
        assert_eq!(
            slash_command_text(&text),
            Some(format!("/compact {big_args}"))
        );
    }

    #[test]
    fn local_command_notices_stay_system_turns() {
        let caveat = "<local-command-caveat>Caveat: the messages below were generated by the user</local-command-caveat>";
        let stdout = "<local-command-stdout>Compacted</local-command-stdout>";
        for text in [caveat, stdout] {
            let (role, content) = claude_user_turn(serde_json::json!(text));
            assert_eq!((role.as_str(), content.as_str()), ("system", text));
        }
    }

    #[test]
    fn compaction_continuation_summary_is_a_system_turn() {
        let opener = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.";
        // By opener text alone, string and block content, with leading whitespace.
        for content in [
            serde_json::json!(format!("{opener}\n\nSummary: built the thing")),
            serde_json::json!([{"type": "text", "text": format!("\n{opener}\n\nSummary: x")}]),
        ] {
            let (role, text) = claude_user_turn(content);
            assert_eq!(role, "system");
            assert!(text.starts_with(opener), "{text:?}");
        }
        // By the flag alone, with text that is not the known opener.
        let flagged = serde_json::json!({
            "type": "user", "isCompactSummary": true, "parentUuid": "boundary-1",
            "message": {"role": "user", "content": "Totally different wording of the recap."}
        })
        .to_string();
        let turns = parse_jsonl_transcript(&flagged);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "system");
        assert_eq!(turns[0].content, "Totally different wording of the recap.");
        // The flag holds even when the recap quotes text the wrapper rules would reclassify.
        let quoting = serde_json::json!({
            "type": "user", "isCompactSummary": true,
            "message": {"role": "user", "content": "Recap\n<SYSTEM_MESSAGE>quoted</SYSTEM_MESSAGE>"}
        })
        .to_string();
        assert_eq!(parse_jsonl_transcript(&quoting)[0].role, "system");
        // A false flag changes nothing.
        let unflagged = serde_json::json!({
            "type": "user", "isCompactSummary": false,
            "message": {"role": "user", "content": "do the thing"}
        })
        .to_string();
        assert_eq!(parse_jsonl_transcript(&unflagged)[0].role, "user");
        // An operator turn that only mentions the phrase keeps its role.
        let (role, _) = claude_user_turn(serde_json::json!(format!(
            "why did you say \"{opener}\" earlier?"
        )));
        assert_eq!(role, "user");
    }

    #[test]
    fn a_compaction_session_reads_as_command_boundary_summary_in_order() {
        let boundary = serde_json::json!({
            "type": "system", "subtype": "compact_boundary", "content": "Conversation compacted",
            "compactMetadata": {"trigger": "manual", "preTokens": 746_687, "postTokens": 21_546}
        })
        .to_string();
        let text = [
            claude_user_row(serde_json::json!("hello")),
            claude_user_row(serde_json::json!(
                "<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args></command-args>"
            )),
            boundary,
            serde_json::json!({
                "type": "user", "isCompactSummary": true,
                "message": {"role": "user", "content": "This session is being continued from a previous conversation that ran out of context."}
            })
            .to_string(),
            claude_user_row(serde_json::json!("carry on")),
        ]
        .join("\n");
        let turns = parse_jsonl_transcript(&text);
        let seen: Vec<(&str, &str)> = turns
            .iter()
            .map(|t| (t.role.as_str(), t.content.as_str()))
            .collect();
        assert_eq!(seen[0], ("user", "hello"));
        assert_eq!(seen[1], ("user", "/compact"));
        assert_eq!(seen[2], ("system", "Conversation compacted"));
        assert_eq!(seen[3].0, "system");
        assert_eq!(seen[4], ("user", "carry on"));
        assert_eq!(seen.len(), 5);
    }

    fn assistant_usage_row(input: u64, output: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "usage": {
                "input_tokens": input, "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0, "output_tokens": output
            }}
        })
        .to_string()
    }

    fn boundary_row(post_tokens: Option<u64>) -> String {
        let mut meta = serde_json::json!({
            "trigger": "manual", "preTokens": 746_687,
            "cumulativeDroppedTokens": 725_141, "durationMs": 47_572
        });
        if let Some(n) = post_tokens {
            meta["postTokens"] = serde_json::json!(n);
        }
        serde_json::json!({
            "type": "system", "subtype": "compact_boundary",
            "content": "Conversation compacted", "compactMetadata": meta
        })
        .to_string()
    }

    fn claude_context(rows: &[String]) -> Option<SessionContextUsage> {
        let home = TempDir::new().unwrap();
        write_transcript(
            home.path(),
            ".claude/projects/p/claude-ctx.jsonl",
            format!("{}\n", rows.join("\n")).as_bytes(),
        );
        let snap = DiscoveryStoreSnapshot::default();
        read_session_context_usage_from_snapshot("claude-ctx", &snap, Some(home.path()))
    }

    fn used(rows: &[String]) -> Option<u64> {
        claude_context(rows).map(|u| u.used_tokens)
    }

    #[test]
    fn claude_context_after_compaction_is_the_boundary_post_tokens() {
        // The reported case: a 746,687-token turn, then a compaction down to 21,546.
        let usage = claude_context(&[
            assistant_usage_row(700_000, 46_687),
            boundary_row(Some(21_546)),
        ])
        .unwrap();
        assert_eq!(
            usage,
            SessionContextUsage {
                used_tokens: 21_546,
                max_tokens: 1_000_000,
            }
        );
        // Rows after the boundary that carry no usage do not bring the old figure back.
        assert_eq!(
            used(&[
                assistant_usage_row(700_000, 46_687),
                boundary_row(Some(21_546)),
                claude_user_row(serde_json::json!("go on")),
            ]),
            Some(21_546)
        );
    }

    #[test]
    fn claude_context_latest_of_assistant_usage_and_boundary_wins() {
        assert_eq!(
            used(&[
                assistant_usage_row(700_000, 46_687),
                boundary_row(Some(21_546)),
                assistant_usage_row(30_000, 500),
            ]),
            Some(30_500)
        );
        // A second compaction wins over the turn before it.
        assert_eq!(
            used(&[
                boundary_row(Some(21_546)),
                assistant_usage_row(300_000, 1_000),
                boundary_row(Some(9_000)),
            ]),
            Some(9_000)
        );
        // A boundary before any usage is the only answer there is.
        assert_eq!(used(&[boundary_row(Some(4_321))]), Some(4_321));
    }

    #[test]
    fn claude_context_ignores_a_boundary_without_post_tokens() {
        for boundary in [boundary_row(Some(0)), boundary_row(None)] {
            assert_eq!(
                used(&[assistant_usage_row(700_000, 46_687), boundary]),
                Some(746_687)
            );
        }
        // Nothing older to fall back on: no figure.
        assert_eq!(used(&[boundary_row(None)]), None);
        assert_eq!(used(&[boundary_row(Some(0))]), None);
        // A wrong type does not count either.
        let stringy = serde_json::json!({
            "type": "system", "subtype": "compact_boundary",
            "compactMetadata": {"postTokens": "21546"}
        })
        .to_string();
        assert_eq!(used(&[assistant_usage_row(1_000, 5), stringy]), Some(1_005));
    }

    #[test]
    fn claude_context_skips_sidechain_rows_and_quoted_boundaries() {
        let mut side_boundary: Value = serde_json::from_str(&boundary_row(Some(77))).unwrap();
        side_boundary["isSidechain"] = serde_json::json!(true);
        let mut side_usage: Value = serde_json::from_str(&assistant_usage_row(5, 5)).unwrap();
        side_usage["isSidechain"] = serde_json::json!(true);
        // An assistant turn that merely talks about a boundary is not one.
        let quoting = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": "the compact_boundary row has compactMetadata",
                        "usage": {"input_tokens": 40, "output_tokens": 2}}
        })
        .to_string();
        assert_eq!(
            used(&[
                assistant_usage_row(700_000, 46_687),
                side_boundary.to_string(),
                side_usage.to_string(),
            ]),
            Some(746_687)
        );
        assert_eq!(used(&[boundary_row(Some(21_546)), quoting]), Some(42));
        // Unparseable candidate lines are skipped.
        assert_eq!(
            used(&[
                assistant_usage_row(10, 1),
                "{\"subtype\":\"compact_boundary\" broken".to_string(),
            ]),
            Some(11)
        );
    }

    #[test]
    fn session_context_usage_in_discovery_reflects_a_compaction() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let project = home.join(".claude").join("projects").join("p");
        fs::create_dir_all(&project).unwrap();
        let session_id = "claude-compacted-sess";
        let transcript = [
            assistant_usage_row(700_000, 46_687),
            boundary_row(Some(21_546)),
        ]
        .join("\n");
        fs::write(project.join(format!("{session_id}.jsonl")), transcript).unwrap();
        write_routes(
            home,
            &[(
                "kayla",
                &format!(
                    r#"{{"kind":"claude","pid":1,"session_id":"{session_id}","socket_path":"{}"}}"#,
                    home.join("c.sock").display()
                ),
            )],
        );
        let sessions = discover_in(home);
        assert_eq!(
            session_for(&sessions, "kayla").context_usage,
            Some(SessionContextUsage {
                used_tokens: 21_546,
                max_tokens: 1_000_000,
            })
        );
    }

    #[test]
    fn discover_sessions_prefers_registered_and_newer_route_over_alphabetical_stale_alias() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let store = fl_store::open_redb(&home.join("store.redb")).unwrap();
        store
            .register_agent(fl_core::agent::RegisterAgentArgs {
                display_name: "Sonya".into(),
                email: None,
                project: "sonora".into(),
                role: fl_core::agent::AgentRole::Lead,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: fl_core::at::At::from_unix_millis(10),
                by: "owner".into(),
            })
            .unwrap();

        let path = home.join(".cache").join("agent-bus").join("routes.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // 1. Registered `sonya` (older timestamp) vs unregistered `github-44` (newer timestamp, alphabetically earlier) sharing `sess-sonya`.
        // 2. Unregistered `kayla` (newer timestamp) vs unregistered `highly` (older timestamp, alphabetically earlier) sharing `conv-kayla`.
        fs::write(
            &path,
            serde_json::json!({
                "entries": {
                    "github-44": {
                        "persona": "github-44",
                        "updated_at": "2026-11-03T12:00:00.000Z",
                        "route": {
                            "kind": "claude",
                            "pid": 1,
                            "session_id": "sess-sonya",
                            "socket_path": home.join("s.sock")
                        }
                    },
                    "sonya": {
                        "persona": "sonya",
                        "updated_at": "2026-11-03T10:00:00.000Z",
                        "route": {
                            "kind": "claude",
                            "pid": 1,
                            "session_id": "sess-sonya",
                            "socket_path": home.join("s.sock")
                        }
                    },
                    "highly": {
                        "persona": "highly",
                        "updated_at": "2026-11-03T10:00:00.000Z",
                        "route": {
                            "kind": "agy",
                            "conversation_id": "conv-kayla"
                        }
                    },
                    "kayla": {
                        "persona": "kayla",
                        "updated_at": "2026-11-03T12:00:00.000Z",
                        "route": {
                            "kind": "agy",
                            "conversation_id": "conv-kayla"
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let sessions = discover_sessions_with_home(&store, Some(home));
        let sonya_sess = sessions.iter().find(|s| s.id == "sess-sonya").unwrap();
        assert_eq!(sonya_sess.persona.as_deref(), Some("sonya"));
        let kayla_sess = sessions.iter().find(|s| s.id == "conv-kayla").unwrap();
        assert_eq!(kayla_sess.persona.as_deref(), Some("kayla"));
    }

    #[test]
    fn agy_transcript_hydrates_truncated_content_and_thinking_from_transcript_full() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let store = fl_store::open_redb(&home.join("store.redb")).unwrap();
        let logs = home
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join("agy-hydrate-1")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();

        let compact = [
            serde_json::json!({
                "step_index": 0,
                "type": "USER_INPUT",
                "created_at": "2026-11-03T17:00:00Z",
                "content": "short prefix... <truncated 457 bytes>",
                "truncated_fields": ["content"]
            }),
            serde_json::json!({
                "step_index": 1,
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-03T17:00:05Z",
                "content": "reply prefix... <truncated 512 bytes>",
                "thinking": "thought prefix... <truncated 300 bytes>",
                "tool_calls": [{
                    "name": "run_command",
                    "arguments": {"CommandLine": "cargo test <truncated 200 bytes>"}
                }],
                "truncated_fields": ["content", "thinking", "tool_calls"]
            }),
            serde_json::json!({
                "step_index": 2,
                "type": "GENERIC",
                "created_at": "2026-11-03T17:00:06Z",
                "content": "Created At: 2026-11-03\ncompact tool output <truncated 9000 bytes>",
                "truncated_fields": ["content"]
            }),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");

        let full = [
            serde_json::json!({
                "step_index": 0,
                "type": "USER_INPUT",
                "created_at": "2026-11-03T17:00:00Z",
                "content": "short prefix... and the complete user prompt without truncation"
            }),
            serde_json::json!({
                "step_index": 1,
                "type": "PLANNER_RESPONSE",
                "created_at": "2026-11-03T17:00:05Z",
                "content": "reply prefix... and the full planner response body",
                "thinking": "thought prefix... and the full chain of thought",
                "tool_calls": [{
                    "name": "run_command",
                    "arguments": {"CommandLine": "cargo test --all-targets --workspace --verbose"}
                }]
            }),
            serde_json::json!({
                "step_index": 2,
                "type": "GENERIC",
                "created_at": "2026-11-03T17:00:06Z",
                "content": "Created At: 2026-11-03\nfull giant 9000-byte tool output that should stay compact"
            }),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");

        fs::write(logs.join("transcript.jsonl"), format!("{compact}\n")).unwrap();
        fs::write(logs.join("transcript_full.jsonl"), format!("{full}\n")).unwrap();

        let (turns, costs) = cost_counters::measure(|| {
            read_session_transcript_with_home("agy-hydrate-1", &store, Some(home))
        });
        assert_eq!(costs.transcript_reads, 1);
        assert_eq!(costs.transcript_parses, 1);
        assert_eq!(turns.len(), 3);

        assert_eq!(turns[0].role, "user");
        assert_eq!(
            turns[0].content,
            "short prefix... and the complete user prompt without truncation"
        );

        assert_eq!(turns[1].role, "assistant");
        assert_eq!(
            turns[1].content,
            "reply prefix... and the full planner response body"
        );
        assert_eq!(
            turns[1].thinking.as_deref(),
            Some("thought prefix... and the full chain of thought")
        );
        assert_eq!(turns[1].tool_calls.len(), 1);
        assert_eq!(
            turns[1].tool_calls[0].arguments["CommandLine"],
            "cargo test <truncated 200 bytes>"
        );

        assert_eq!(turns[2].role, "tool");
        assert_eq!(
            turns[2].content,
            "Created At: 2026-11-03\ncompact tool output <truncated 9000 bytes>"
        );
    }

    #[test]
    fn agy_transcript_falls_back_cleanly_when_transcript_full_is_missing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let store = fl_store::open_redb(&home.join("store.redb")).unwrap();
        let logs = home
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join("agy-no-full-1")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();

        let compact = serde_json::json!({
            "step_index": 1,
            "type": "PLANNER_RESPONSE",
            "created_at": "2026-11-03T17:00:05Z",
            "content": "compact only <truncated 100 bytes>",
            "truncated_fields": ["content"]
        })
        .to_string();
        fs::write(logs.join("transcript.jsonl"), format!("{compact}\n")).unwrap();

        let turns = read_session_transcript_with_home("agy-no-full-1", &store, Some(home));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "assistant");
        assert_eq!(turns[0].content, "compact only <truncated 100 bytes>");
    }
}
