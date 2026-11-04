//! 5-Provider & Runtime Testing Matrix (Project Record 17).
//!
//! `GET /api/v1/runtimes/matrix` and `POST /api/v1/runtimes/probe` report three tiers:
//!
//! 1. subscription CLI runtimes (`claude`, `codex`, `grok`, `agy`, `muse`): binary discovery,
//!    a bounded `--version` probe, active route count and live session count;
//! 2. API-key providers held in the `FerroGate` vault: whether each provider secret is
//!    provisioned and unexpired. Secret values are never read, only metadata;
//! 3. local inference engines (Ollama, Lemonade) on loopback: reachability, latency and models.
//!
//! The matrix also carries `vendor_quotas`: the account allowance of each vendor (see
//! [`QuotaSource`]), and every CLI row says whether it can update itself (`update_supported`,
//! `update_command`; see [`crate::runtime_update`], which serves `POST /api/v1/runtimes/update`).
//!
//! Every probe is bounded. The three tiers and every item inside them run concurrently, so the
//! worst case is one probe timeout, not the sum of all of them. Probing is owner only and goes
//! through a [`ProbeGate`]: one probe at a time, and a fresh result is served again.
//!
//! The quota reads are the slow part (each cold read may run a CLI or a network call), so they have
//! a wall-clock budget of their own ([`QUOTA_BUDGET`]): a vendor that is not read in time is left
//! out of `vendor_quotas`. Non-Codex readings are kept for 30 s ([`QuotaCache`]); Codex
//! account tier reads bypass the cache so credential changes are reflected immediately.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use fl_core::telemetry::AdapterKind;
use fl_core::{
    AcpOperation, ConformanceState, OperationEvidence, SessionProtocolKind, VendorDescriptor,
    VendorRegistry, VendorTransport,
};
use fl_exec::child_env::{child_env, helper_env};
use fl_exec::stamp;
use fl_gate::{MASTER_KEY_LEN, Vault, VaultSecretMetadata, resolve_vault_path};
use serde::Serialize;
use serde_json::{Value, json};

use crate::HubState;
use crate::api::{HttpRequest, HttpResponse};
use crate::auth::HubAuthority;
use crate::direct_quota::AgyQuotaTarget;
use crate::discovery::{
    DiscoveredSessionSummary, SessionQuotaUsage, codex_account_quota,
    discover_route_sessions_without_model_info, quota_for_resolved_route,
    quota_for_resolved_route_at, snapshot_discovery_store,
};
use crate::runtime_update::update_command;

/// Subscription CLI runtimes, in display order.
pub const CLI_RUNTIMES: [&str; 5] = ["claude", "codex", "grok", "agy", "muse"];
/// The adapter dialect of each [`CLI_RUNTIMES`] name.
const CLI_KINDS: [(&str, AdapterKind); 5] = [
    ("claude", AdapterKind::Claude),
    ("codex", AdapterKind::Codex),
    ("grok", AdapterKind::Grok),
    ("agy", AdapterKind::Antigravity),
    ("muse", AdapterKind::Muse),
];
/// `(provider, vault secret name)` pairs.
pub const VAULT_PROVIDERS: [(&str, &str); 6] = [
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    ("xai", "XAI_API_KEY"),
    ("gemini", "GEMINI_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
    ("kimi", "MOONSHOT_API_KEY"),
];

pub(crate) const VERSION_TIMEOUT: Duration = Duration::from_secs(2);
const LOCAL_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_PROBE_OUTPUT: usize = 4096;
const MAX_LOCAL_BODY: usize = 256 * 1024;
const MAX_LOCAL_MODELS: usize = 100;
const MAX_TARGET_LEN: usize = 64;
/// A probe result younger than this is served again for the same target, so a burst of requests
/// spawns each CLI once.
const PROBE_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// The most wall-clock time one matrix build spends waiting for vendor quota reads. A vendor whose
/// read is not done by then is left out of `vendor_quotas` (its read finishes in the background).
pub(crate) const QUOTA_BUDGET: Duration = Duration::from_secs(4);
/// How long a vendor's quota reading is served again by [`QuotaCache`].
const QUOTA_CACHE_TTL_MS: u64 = 30_000;

/// Loopback inference engine to probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEngineSpec {
    pub name: String,
    pub port: u16,
    pub path: String,
    pub kind: LocalEngineKind,
}

/// Shape of the model-list response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalEngineKind {
    /// `{"models":[{"name":"..."}]}`
    Ollama,
    /// `{"data":[{"id":"..."}]}`
    OpenAiModels,
}

pub fn default_local_engines() -> Vec<LocalEngineSpec> {
    vec![
        LocalEngineSpec {
            name: "ollama".into(),
            port: 11434,
            path: "/api/tags".into(),
            kind: LocalEngineKind::Ollama,
        },
        LocalEngineSpec {
            name: "lemonade-8000".into(),
            port: 8000,
            path: "/v1/models".into(),
            kind: LocalEngineKind::OpenAiModels,
        },
        LocalEngineSpec {
            name: "lemonade-8097".into(),
            port: 8097,
            path: "/v1/models".into(),
            kind: LocalEngineKind::OpenAiModels,
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CliRuntimeStatus {
    pub name: String,
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
    pub active_routes: usize,
    pub live_sessions: usize,
    pub probe_ms: u64,
    /// Whether the CLI has a self-update command `POST /api/v1/runtimes/update` can run. Fixed
    /// per runtime, whether or not the binary is installed.
    pub update_supported: bool,
    /// The update command as shown to a person (for example `claude update`); `None` when
    /// `update_supported` is false.
    pub update_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VaultProviderStatus {
    pub provider: String,
    pub secret_name: String,
    pub provisioned: bool,
    pub expired: bool,
    /// `ready`, `expired` or `missing`.
    pub status: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalEngineStatus {
    pub name: String,
    pub endpoint: String,
    pub reachable: bool,
    pub latency_ms: Option<u64>,
    pub models: Vec<String>,
    pub error: Option<String>,
}

/// A hosted-session protocol and whether it is the current default.
/// This is a profile selection, not proof that a vendor is installed or ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostedProtocolProfile {
    pub kind: SessionProtocolKind,
    pub active: bool,
}

/// Evidence for ACP production admission. No proof registry exists in this slice, so these
/// rows are unproven and never authorize production ACP work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AcpAdmissionEvidence {
    pub state: ConformanceState,
    pub production_admitted: bool,
    pub operations: Vec<OperationEvidence>,
}

/// One registered vendor, as `GET /api/v1/runtimes/matrix` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VendorCatalogEntry {
    pub id: String,
    pub display_name: String,
    /// `subscription_cli` or `http_api`.
    pub transport: String,
    pub default_model: String,
    pub default_context_window: u64,
    /// Whether an attempt through this vendor needs an API key (the key itself is never listed).
    pub needs_api_key: bool,
    /// `http_api` only.
    pub base_url: Option<String>,
    /// `http_api` only: `openai_chat_completions`, `anthropic_messages` or
    /// `gemini_generate_content`.
    pub dialect: Option<String>,
    pub mcp_config_rel_path: Option<String>,
    /// Whether the vendor ships with fl, as opposed to being registered at run time.
    pub builtin: bool,
    /// Native is the default; ACP is only a candidate until proven.
    pub hosted_protocol_profiles: Vec<HostedProtocolProfile>,
    pub acp_admission: AcpAdmissionEvidence,
}

impl VendorCatalogEntry {
    fn of(d: &VendorDescriptor) -> Self {
        let (base_url, dialect) = match &d.transport {
            VendorTransport::HttpApi {
                default_base_url,
                api_dialect,
                ..
            } => (
                Some(default_base_url.clone()),
                serde_json::to_value(api_dialect)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string)),
            ),
            VendorTransport::SubscriptionCli { .. } => (None, None),
        };
        Self {
            id: d.id.clone(),
            display_name: d.display_name.clone(),
            transport: match d.transport {
                VendorTransport::SubscriptionCli { .. } => "subscription_cli",
                VendorTransport::HttpApi { .. } => "http_api",
            }
            .to_string(),
            default_model: d.default_model.clone(),
            default_context_window: d.default_context_window,
            needs_api_key: d.transport.needs_api_key(),
            base_url,
            dialect,
            mcp_config_rel_path: d.mcp_config_rel_path.clone(),
            builtin: VendorRegistry::is_builtin(&d.id),
            hosted_protocol_profiles: vec![
                HostedProtocolProfile {
                    kind: SessionProtocolKind::Native,
                    active: true,
                },
                HostedProtocolProfile {
                    kind: SessionProtocolKind::AcpV1Stdio,
                    active: false,
                },
            ],
            acp_admission: AcpAdmissionEvidence {
                state: ConformanceState::Unproven,
                production_admitted: false,
                operations: AcpOperation::ALL
                    .into_iter()
                    .map(OperationEvidence::unproven)
                    .collect(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeMatrix {
    pub generated_at_ms: u64,
    pub cli_runtimes: Vec<CliRuntimeStatus>,
    pub vault_providers: Vec<VaultProviderStatus>,
    pub local_engines: Vec<LocalEngineStatus>,
    /// The registered vendor catalog, sorted by id. Descriptor data only: no key value, and no
    /// probe result.
    pub vendors: Vec<VendorCatalogEntry>,
    /// Account allowance per vendor adapter (`claude`, `codex`, `grok`, `antigravity`, `muse`),
    /// for the CLI runtimes the probe selected. A vendor whose allowance is unknown has no entry.
    pub vendor_quotas: BTreeMap<String, SessionQuotaUsage>,
    /// Vault failure (for example an unreadable vault file). The tier is then empty of
    /// `provisioned` secrets rather than the whole matrix failing.
    pub vault_error: Option<String>,
}

/// Everything a probe reads from outside itself. Tests build this by hand.
pub struct ProbeInputs {
    pub path_env: String,
    pub home: Option<PathBuf>,
    pub sessions: Vec<DiscoveredSessionSummary>,
    pub vault: Result<Vec<VaultSecretMetadata>, String>,
    pub local_engines: Vec<LocalEngineSpec>,
    pub now_ms: u64,
    /// The hub's `home_override` (a sandboxed hub, such as a test's), as the quota readers need it:
    /// `None` lets them read the real home.
    pub home_override: Option<PathBuf>,
    pub quota_source: QuotaSource,
    /// Wall-clock budget for the whole quota phase (see [`QUOTA_BUDGET`]).
    pub quota_budget: Duration,
    /// Recent vendor readings, so repeated reads do not repeat the quota sources.
    pub quota_cache: Arc<QuotaCache>,
}

/// Vendor quota readings by `(home_override, adapter)`, each served for
/// [`QUOTA_CACHE_TTL_MS`] after it was read (by `ProbeInputs::now_ms`, so a test sets the clock).
/// A reading that found nothing is cached too: the cold read is the expensive part. A reading that
/// misses the quota budget is stored when it finishes.
#[derive(Debug, Default)]
pub struct QuotaCache {
    entries: Mutex<QuotaEntries>,
}

/// `(home_override, adapter)` to `(read at ms, reading)`.
type QuotaEntries = BTreeMap<(Option<PathBuf>, String), (u64, Option<SessionQuotaUsage>)>;

impl QuotaCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, QuotaEntries> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `Some(reading)` while the entry is fresh at `now_ms`.
    fn get(
        &self,
        home: &Option<PathBuf>,
        adapter: &str,
        now_ms: u64,
    ) -> Option<Option<SessionQuotaUsage>> {
        let entries = self.lock();
        let (at, reading) = entries.get(&(home.clone(), adapter.to_string()))?;
        (now_ms.saturating_sub(*at) < QUOTA_CACHE_TTL_MS).then(|| reading.clone())
    }

    fn put(
        &self,
        home: &Option<PathBuf>,
        adapter: &str,
        now_ms: u64,
        reading: Option<SessionQuotaUsage>,
    ) {
        self.lock()
            .insert((home.clone(), adapter.to_string()), (now_ms, reading));
    }
}

/// What one vendor's quota read needs, owned so the read can outlive the matrix build that
/// started it.
struct QuotaInputs {
    sessions: Vec<DiscoveredSessionSummary>,
    home_override: Option<PathBuf>,
    quota_source: QuotaSource,
    now_ms: u64,
}

/// How the matrix reads a vendor's account allowance. The readers are plain functions so a test
/// can supply its own and never reach the host.
#[derive(Clone, Copy)]
pub struct QuotaSource {
    /// The allowance a discovered route session reports (its transcript, and the account).
    pub session: fn(&DiscoveredSessionSummary, Option<&Path>) -> Option<SessionQuotaUsage>,
    /// The account allowance of an adapter that no session reports.
    pub account: fn(&str, Option<&Path>) -> Option<SessionQuotaUsage>,
}

impl QuotaSource {
    /// The host's readers, which the direct-quota caches bound (60 s, 3 s per cold probe).
    pub const HOST: Self = Self {
        session: host_session_quota,
        account: host_account_quota,
    };
    /// Reads nothing.
    pub const NONE: Self = Self {
        session: |_, _| None,
        account: |_, _| None,
    };
}

fn host_session_quota(
    session: &DiscoveredSessionSummary,
    home_override: Option<&Path>,
) -> Option<SessionQuotaUsage> {
    quota_for_resolved_route(
        &session.adapter,
        session.transcript_path.as_deref(),
        home_override,
        &session.id,
    )
}

/// The account probe `GET /api/v1/watch/balance` uses: no session, so no route, model or
/// conversation id.
fn host_account_quota(adapter: &str, home_override: Option<&Path>) -> Option<SessionQuotaUsage> {
    if adapter == "codex" {
        codex_account_quota(home_override)
    } else {
        quota_for_resolved_route_at(adapter, None, home_override, &AgyQuotaTarget::default())
    }
}

/// Route sessions per adapter whose allowance is read from disk, newest first. Bounds the cost
/// of a probe on a host with many sessions.
const MAX_QUOTA_SESSIONS_PER_ADAPTER: usize = 4;

/// The adapter id discovery gives a session of the CLI runtime `cli`.
fn quota_adapter(cli: &str) -> &str {
    if cli == "agy" { "antigravity" } else { cli }
}

/// How many facts a quota reading holds; the reading with the most is the most useful. A window
/// that reset before `now_ms` describes an allowance that no longer applies, so it counts for
/// nothing.
fn quota_richness(q: &SessionQuotaUsage, now_ms: u64) -> usize {
    let text = |s: &Option<String>| s.as_deref().is_some_and(|s| !s.trim().is_empty());
    let live = |w: &Option<crate::discovery::QuotaWindowUsage>| {
        w.as_ref()
            .is_some_and(|w| w.resets_at_ms.is_none_or(|reset| reset >= now_ms))
    };
    usize::from(text(&q.tier))
        + usize::from(live(&q.primary_window))
        + usize::from(live(&q.secondary_window))
        + usize::from(text(&q.exhausted_reason))
}

/// The allowance of one vendor: the richest live reading among its sessions (the newest session
/// wins a tie), else the account probe.
fn vendor_quota(adapter: &str, inputs: &QuotaInputs) -> Option<SessionQuotaUsage> {
    let home = inputs.home_override.as_deref();
    let now_ms = inputs.now_ms;
    // Codex rollouts do not carry an account identity that can bind them to
    // the current auth file, even when both happen to use the same directory.
    if adapter == "codex" {
        return (inputs.quota_source.account)(adapter, home)
            .filter(|q| quota_richness(q, now_ms) > 0);
    }
    let mut sessions: Vec<&DiscoveredSessionSummary> = inputs
        .sessions
        .iter()
        .filter(|s| s.adapter == adapter)
        .collect();
    // Newest first, and the sort is stable: among equally rich readings the first (newest) stays.
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let mut reads_left = MAX_QUOTA_SESSIONS_PER_ADAPTER;
    let mut best: Option<(usize, SessionQuotaUsage)> = None;
    for session in sessions {
        let reading = match &session.quota_usage {
            Some(q) => Some(q.clone()),
            None if session.source == "route" && reads_left > 0 => {
                reads_left -= 1;
                (inputs.quota_source.session)(session, home)
            }
            None => None,
        };
        let Some(reading) = reading else { continue };
        let richness = quota_richness(&reading, now_ms);
        if richness > 0 && best.as_ref().is_none_or(|(r, _)| richness > *r) {
            best = Some((richness, reading));
        }
    }
    best.map(|(_, q)| q).or_else(|| {
        (inputs.quota_source.account)(adapter, home).filter(|q| quota_richness(q, now_ms) > 0)
    })
}

/// Vendor quota reads in flight: [`start_vendor_quotas`] starts them, [`PendingQuotas::collect`]
/// waits for them.
struct PendingQuotas {
    /// Readings already known (fresh in the cache).
    quotas: BTreeMap<String, SessionQuotaUsage>,
    rx: mpsc::Receiver<(String, Option<SessionQuotaUsage>)>,
    pending: usize,
    deadline: Instant,
}

impl PendingQuotas {
    /// The readings that arrive before the deadline, with the cached ones. A vendor that misses the
    /// deadline is omitted.
    fn collect(mut self) -> BTreeMap<String, SessionQuotaUsage> {
        for _ in 0..self.pending {
            let left = self.deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok((adapter, Some(q))) => {
                    self.quotas.insert(adapter, q);
                }
                Ok((_, None)) => {}
                Err(_) => break,
            }
        }
        self.quotas
    }
}

/// Start every selected vendor's quota read, to be collected within `inputs.quota_budget` of
/// wall-clock time from now. A fresh cached reading is used without a read. Each other vendor is
/// read on its own thread, not scoped to the matrix build: one that misses the deadline is omitted,
/// and its thread finishes later on its own (storing its reading in the cache) without holding the
/// response.
fn start_vendor_quotas(clis: &[&'static str], inputs: &ProbeInputs) -> PendingQuotas {
    let deadline = Instant::now() + inputs.quota_budget;
    let mut quotas: BTreeMap<String, SessionQuotaUsage> = BTreeMap::new();
    let (tx, rx) = mpsc::channel::<(String, Option<SessionQuotaUsage>)>();
    let mut pending = 0usize;
    for cli in clis {
        let adapter = quota_adapter(cli);
        if adapter != "codex"
            && let Some(cached) =
                inputs
                    .quota_cache
                    .get(&inputs.home_override, adapter, inputs.now_ms)
        {
            if let Some(q) = cached {
                quotas.insert(adapter.to_string(), q);
            }
            continue;
        }
        let read_inputs = QuotaInputs {
            sessions: inputs
                .sessions
                .iter()
                .filter(|s| s.adapter == adapter)
                .cloned()
                .collect(),
            home_override: inputs.home_override.clone(),
            quota_source: inputs.quota_source,
            now_ms: inputs.now_ms,
        };
        let cache = Arc::clone(&inputs.quota_cache);
        let tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("quota-{adapter}"))
            .spawn(move || {
                let reading = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    vendor_quota(adapter, &read_inputs)
                }))
                .ok();
                if adapter != "codex"
                    && let Some(reading) = &reading
                {
                    cache.put(
                        &read_inputs.home_override,
                        adapter,
                        read_inputs.now_ms,
                        reading.clone(),
                    );
                }
                // The receiver is gone once the budget ran out: a late reading has nowhere to go.
                let _ = tx.send((adapter.to_string(), reading.flatten()));
            });
        if spawned.is_ok() {
            pending += 1;
        }
    }
    PendingQuotas {
        quotas,
        rx,
        pending,
        deadline,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Cli,
    Vault,
    Local,
}

/// Which tiers and items a `target` filter selects.
struct Selection {
    cli: Vec<&'static str>,
    vault: Vec<(&'static str, &'static str)>,
    local: Vec<LocalEngineSpec>,
    /// Registered vendors, by id.
    vendors: Vec<VendorDescriptor>,
}

fn tier_for(target: &str) -> Option<Tier> {
    match target {
        "cli" | "cli-runtimes" => Some(Tier::Cli),
        "vault" | "vault-api-keys" => Some(Tier::Vault),
        "local" | "local-engines" => Some(Tier::Local),
        _ => None,
    }
}

/// The key names (environment variable, Vault secret) of a vendor; empty for a CLI or a keyless
/// engine.
fn key_names(d: &VendorDescriptor) -> Vec<&str> {
    match &d.transport {
        VendorTransport::HttpApi {
            vault_secret_name,
            env_key_name,
            ..
        } => [vault_secret_name, env_key_name]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect(),
        VendorTransport::SubscriptionCli { .. } => Vec::new(),
    }
}

fn select(target: Option<&str>, engines: &[LocalEngineSpec]) -> Result<Selection, String> {
    let all = Selection {
        cli: CLI_RUNTIMES.to_vec(),
        vault: VAULT_PROVIDERS.to_vec(),
        local: engines.to_vec(),
        vendors: VendorRegistry::global().list(),
    };
    let Some(raw) = target else {
        return Ok(all);
    };
    let target = raw.trim().to_ascii_lowercase();
    if target.is_empty() {
        return Ok(all);
    }
    if target.len() > MAX_TARGET_LEN {
        return Err("`target` is too long".to_string());
    }
    let mut sel = Selection {
        cli: Vec::new(),
        vault: Vec::new(),
        local: Vec::new(),
        vendors: Vec::new(),
    };
    if target == "vendors" {
        sel.vendors = all.vendors;
        return Ok(sel);
    }
    if let Some(tier) = tier_for(&target) {
        // A tier selects the vendors that reach it the way the tier does: a CLI vendor runs as
        // a process, a vault vendor is an API that needs a key, a local one needs none.
        let in_tier = |d: &VendorDescriptor| match (tier, &d.transport) {
            (Tier::Cli, VendorTransport::SubscriptionCli { .. }) => true,
            (Tier::Vault, t @ VendorTransport::HttpApi { .. }) => t.needs_api_key(),
            (Tier::Local, t @ VendorTransport::HttpApi { .. }) => !t.needs_api_key(),
            _ => false,
        };
        sel.vendors = all.vendors.iter().filter(|d| in_tier(d)).cloned().collect();
        match tier {
            Tier::Cli => sel.cli = all.cli,
            Tier::Vault => sel.vault = all.vault,
            Tier::Local => sel.local = all.local,
        }
        return Ok(sel);
    }
    sel.cli = all.cli.into_iter().filter(|n| *n == target).collect();
    // The vendor id `kimi-api` also selects the Vault entry that holds its key.
    let by_id = all.vendors.iter().find(|d| d.id == target);
    sel.vault = all
        .vault
        .into_iter()
        .filter(|(p, s)| {
            *p == target
                || s.eq_ignore_ascii_case(&target)
                || by_id.is_some_and(|d| key_names(d).iter().any(|n| n.eq_ignore_ascii_case(s)))
        })
        .collect();
    sel.local = all.local.into_iter().filter(|e| e.name == target).collect();
    // A vendor is selected by its id, by the CLI binary it runs (`agy` for `antigravity`), or by
    // the key it uses (`kimi` and `MOONSHOT_API_KEY` for `kimi-api`).
    sel.vendors = all
        .vendors
        .into_iter()
        .filter(|d| {
            d.id == target
                || matches!(&d.transport, VendorTransport::SubscriptionCli { default_binary }
                    if sel.cli.iter().any(|c| c == default_binary))
                || key_names(d).iter().any(|name| {
                    sel.vault
                        .iter()
                        .any(|(_, secret)| name.eq_ignore_ascii_case(secret))
                })
        })
        .collect();
    if sel.cli.is_empty() && sel.vault.is_empty() && sel.local.is_empty() && sel.vendors.is_empty()
    {
        return Err(format!("unknown probe target `{target}`"));
    }
    Ok(sel)
}

/// Run the selected probes. Unknown `target` is an error; no target selects everything.
pub fn build_matrix(inputs: &ProbeInputs, target: Option<&str>) -> Result<RuntimeMatrix, String> {
    let sel = select(target, &inputs.local_engines)?;
    // Started first and collected last, so the reads run beside the CLI and engine probes and the
    // whole build waits for them no longer than the quota budget.
    let pending_quotas = start_vendor_quotas(&sel.cli, inputs);
    let (cli_runtimes, local_engines) = std::thread::scope(|scope| {
        let cli_handles: Vec<_> = sel
            .cli
            .iter()
            .map(|name| scope.spawn(move || probe_cli(name, inputs, VERSION_TIMEOUT)))
            .collect();
        let local_handles: Vec<_> = sel
            .local
            .iter()
            .map(|spec| scope.spawn(move || probe_local_engine(spec, LOCAL_TIMEOUT)))
            .collect();
        let cli: Vec<CliRuntimeStatus> = cli_handles
            .into_iter()
            .zip(sel.cli.iter())
            .map(|(h, name)| h.join().unwrap_or_else(|_| panicked_cli(name)))
            .collect();
        let local: Vec<LocalEngineStatus> = local_handles
            .into_iter()
            .zip(sel.local.iter())
            .map(|(h, spec)| h.join().unwrap_or_else(|_| panicked_local(spec)))
            .collect();
        (cli, local)
    });
    let vendor_quotas = pending_quotas.collect();
    let (vault_providers, vault_error) = match &inputs.vault {
        Ok(meta) => (vault_statuses(&sel.vault, meta, inputs.now_ms), None),
        Err(err) => (
            vault_statuses(&sel.vault, &[], inputs.now_ms),
            Some(err.clone()),
        ),
    };
    Ok(RuntimeMatrix {
        generated_at_ms: inputs.now_ms,
        cli_runtimes,
        vault_providers,
        local_engines,
        vendors: sel.vendors.iter().map(VendorCatalogEntry::of).collect(),
        vendor_quotas,
        vault_error,
    })
}

fn panicked_cli(name: &str) -> CliRuntimeStatus {
    CliRuntimeStatus {
        name: name.to_string(),
        installed: false,
        path: None,
        version: None,
        error: Some("probe panicked".to_string()),
        active_routes: 0,
        live_sessions: 0,
        probe_ms: 0,
        update_command: update_command(name),
        update_supported: update_command(name).is_some(),
    }
}

fn panicked_local(spec: &LocalEngineSpec) -> LocalEngineStatus {
    LocalEngineStatus {
        name: spec.name.clone(),
        endpoint: endpoint_of(spec),
        reachable: false,
        latency_ms: None,
        models: Vec::new(),
        error: Some("probe panicked".to_string()),
    }
}

// ---------------------------------------------------------------- tier 1: CLI runtimes

/// First executable `name` in `path_env` directories, then `<home>/.local/bin`. A relative
/// directory is skipped (it would resolve against whatever the hub's working directory is), and
/// so is a relative home.
pub fn find_binary(name: &str, path_env: &str, home: Option<&Path>) -> Option<PathBuf> {
    let from_path = path_env
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name));
    let from_home = home.map(|h| h.join(".local").join("bin").join(name));
    from_path
        .chain(from_home)
        .find(|p| p.is_absolute() && is_executable_file(p))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn probe_cli(name: &str, inputs: &ProbeInputs, timeout: Duration) -> CliRuntimeStatus {
    let started = Instant::now();
    let active_routes = inputs
        .sessions
        .iter()
        .filter(|s| s.adapter == name && s.source == "route")
        .count();
    let live_sessions = inputs
        .sessions
        .iter()
        .filter(|s| s.adapter == name && matches!(s.status.as_str(), "active" | "running"))
        .count();
    let mut status = CliRuntimeStatus {
        name: name.to_string(),
        installed: false,
        path: None,
        version: None,
        error: None,
        active_routes,
        live_sessions,
        probe_ms: 0,
        update_command: update_command(name),
        update_supported: update_command(name).is_some(),
    };
    match find_binary(name, &inputs.path_env, inputs.home.as_deref()) {
        None => status.error = Some(format!("`{name}` not found in PATH or ~/.local/bin")),
        Some(path) => {
            status.installed = true;
            status.path = Some(path.display().to_string());
            match run_version(&path, timeout) {
                Ok(version) => status.version = Some(version),
                Err(err) => status.error = Some(err),
            }
        }
    }
    status.probe_ms = elapsed_ms(started);
    status
}

/// The environment of a `--version` probe of `path`, by the vendor its file name stands for.
fn version_env(path: &Path) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let name = path.file_name().and_then(|n| n.to_str());
    match CLI_KINDS.iter().find(|(n, _)| Some(*n) == name) {
        Some((_, kind)) => child_env(*kind, &[], &[]),
        None => helper_env(&[], &[]),
    }
}

/// `<path> --version` with a hard deadline. The child leads its own process group, and the whole
/// group is killed on timeout and after exit, so a wrapper script cannot leave a grandchild behind.
/// The group is only signalled while the leader is still unreaped (see [`exited_unreaped`]).
///
/// The child gets a scrubbed environment (see [`fl_exec::child_env`]): the baseline plus the
/// vendor variables of the CLI the file name stands for (`claude`, `codex`, `agy`, `muse`,
/// `grok`), or the baseline alone for any other name. It never sees `FL_VAULT_MASTER_KEY`,
/// `FL_HUB_*`, GitHub credentials, another vendor's keys or a proxy.
pub fn run_version(path: &Path, timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version")
        .env_clear()
        .envs(version_env(path))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let pid = child.id();
    let out_rx = drain_pipe(child.stdout.take());
    let err_rx = drain_pipe(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let exited = loop {
        match exited_unreaped(&mut child) {
            Ok(true) => break true,
            Ok(false) if Instant::now() >= deadline => break false,
            Ok(false) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                // The state of the leader is unknown, so no group signal: it may be reaped.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait failed: {e}"));
            }
        }
    };
    let exit = kill_group(pid, &mut child);
    if !exited {
        return Err(format!(
            "--version timed out after {}ms",
            timeout.as_millis()
        ));
    }
    let Ok(exit) = exit else {
        return Err("wait failed".to_string());
    };
    let stdout = out_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_default();
    let stderr = err_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_default();
    if !exit.success() {
        return Err(format!("--version exited with {exit}"));
    }
    let text = [stdout, stderr].iter().find_map(|s| {
        s.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
    });
    match text {
        Some(line) => Ok(line.chars().take(200).collect()),
        None => Err("--version printed nothing".to_string()),
    }
}

fn drain_pipe<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = pipe {
            let _ = pipe.take(MAX_PROBE_OUTPUT as u64).read_to_end(&mut buf);
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

/// Whether the child has exited, without reaping it. An exited but unreaped leader is a zombie
/// that still owns its pid, so `kill_group` can signal `-pid` knowing no other process holds that
/// id; once a leader is reaped (`try_wait`, `wait`) and its group is empty, the id can be reused.
#[cfg(unix)]
pub(crate) fn exited_unreaped(child: &mut std::process::Child) -> std::io::Result<bool> {
    // SAFETY: `waitid` with `WNOHANG | WNOWAIT` only fills `info` and leaves the child waitable;
    // an all-zero `siginfo_t` is a valid value to pass in. `si_pid` stays 0 while the child runs.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let rc = libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.si_pid() != 0)
    }
}

#[cfg(not(unix))]
pub(crate) fn exited_unreaped(child: &mut std::process::Child) -> std::io::Result<bool> {
    child.try_wait().map(|status| status.is_some())
}

/// SIGKILL the child's process group, then reap the child. Call it only while the leader is
/// unreaped, so `-pid` still names the child's own group.
pub(crate) fn kill_group(
    pid: u32,
    child: &mut std::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: plain `kill(2)` on a negative pid targets the process group the child created
        // with `process_group(0)`. No memory is touched; ESRCH is ignored.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    child.wait()
}

// ---------------------------------------------------------------- tier 2: vault

/// Vault status per provider, from metadata only. Secret values are never decrypted.
pub fn vault_statuses(
    providers: &[(&str, &str)],
    metadata: &[VaultSecretMetadata],
    now_ms: u64,
) -> Vec<VaultProviderStatus> {
    let now = fl_core::At::from_unix_millis(now_ms).to_string();
    providers
        .iter()
        .map(|(provider, secret)| {
            let meta = metadata
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case(secret));
            // Same lexicographic comparison the vault itself applies on read.
            let expired = meta
                .and_then(|m| m.expires_at.as_deref())
                .is_some_and(|exp| now.as_str() > exp);
            let status = match meta {
                None => "missing",
                Some(_) if expired => "expired",
                Some(_) => "ready",
            };
            VaultProviderStatus {
                provider: (*provider).to_string(),
                secret_name: (*secret).to_string(),
                provisioned: meta.is_some(),
                expired,
                status: status.to_string(),
                expires_at: meta.and_then(|m| m.expires_at.clone()),
            }
        })
        .collect()
}

// ---------------------------------------------------------------- tier 3: local engines

fn endpoint_of(spec: &LocalEngineSpec) -> String {
    format!("127.0.0.1:{}{}", spec.port, spec.path)
}

pub fn probe_local_engine(spec: &LocalEngineSpec, timeout: Duration) -> LocalEngineStatus {
    let started = Instant::now();
    let mut status = LocalEngineStatus {
        name: spec.name.clone(),
        endpoint: endpoint_of(spec),
        reachable: false,
        latency_ms: None,
        models: Vec::new(),
        error: None,
    };
    match loopback_get(spec.port, &spec.path, timeout) {
        Ok((code, body)) if (200..300).contains(&code) => {
            status.reachable = true;
            status.latency_ms = Some(elapsed_ms(started));
            status.models = parse_model_ids(spec.kind, &body);
        }
        Ok((code, _)) => {
            status.latency_ms = Some(elapsed_ms(started));
            status.error = Some(format!("HTTP {code}"));
        }
        Err(err) => status.error = Some(err),
    }
    status
}

/// HTTP/1.0 GET to `127.0.0.1:port`. HTTP/1.0 keeps servers from chunking the body.
/// The connect and the whole read share one `timeout` budget.
fn loopback_get(port: u16, path: &str, timeout: Duration) -> Result<(u16, String), String> {
    let deadline = Instant::now() + timeout;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
        std::io::ErrorKind::TimedOut => "connect timed out".to_string(),
        _ => format!("connect failed: {e}"),
    })?;
    stream.set_write_timeout(Some(timeout)).ok();
    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("read timed out".to_string());
        }
        stream.set_read_timeout(Some(left)).ok();
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > MAX_LOCAL_BODY {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("read timed out".to_string());
            }
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
    parse_http_response(&raw)
}

fn parse_http_response(raw: &[u8]) -> Result<(u16, String), String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let code = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| "malformed HTTP response".to_string())?;
    Ok((code, body.to_string()))
}

fn parse_model_ids(kind: LocalEngineKind, body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let (list, keys): (&str, &[&str]) = match kind {
        LocalEngineKind::Ollama => ("models", &["name", "model"]),
        LocalEngineKind::OpenAiModels => ("data", &["id"]),
    };
    value
        .get(list)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    keys.iter()
                        .find_map(|k| item.get(*k).and_then(Value::as_str))
                })
                .take(MAX_LOCAL_MODELS)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------- HTTP handlers

/// Serializes probes and remembers the last result. A probe spawns `<binary> --version` for each
/// CLI, so a second request while one runs gets `429` instead of a second set of children, and a
/// request within [`PROBE_MIN_INTERVAL`] of a finished probe of the same target gets that result.
#[derive(Debug, Default)]
pub struct ProbeGate {
    state: Mutex<GateState>,
}

#[derive(Debug, Default)]
struct GateState {
    running: bool,
    last: Option<CachedProbe>,
}

#[derive(Debug)]
struct CachedProbe {
    at: Instant,
    target: Option<String>,
    body: Value,
}

/// Clears `running` when the probe ends, including by panic.
struct Running<'a>(&'a ProbeGate);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.lock().running = false;
    }
}

impl ProbeGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forget the last result, so the next probe runs again. A CLI update calls this: the
    /// versions the last result holds are out of date.
    pub(crate) fn invalidate(&self) {
        self.lock().last = None;
    }

    /// Serve `target` from the last result, refuse while another probe runs, or run `probe` and
    /// remember its result. An error response is returned as it is and not remembered.
    fn run(
        &self,
        target: Option<&str>,
        probe: impl FnOnce() -> Result<Value, HttpResponse>,
    ) -> HttpResponse {
        // The same normalisation `select` applies, so `Vault` and `vault ` share a result.
        let key = target
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty());
        {
            let mut state = self.lock();
            if let Some(last) = &state.last
                && last.target == key
                && last.at.elapsed() < PROBE_MIN_INTERVAL
            {
                return HttpResponse::json(200, last.body.clone());
            }
            if state.running {
                return HttpResponse::json(
                    429,
                    json!({ "error": "a probe is already running; retry shortly" }),
                )
                .with_header("Retry-After", "1");
            }
            state.running = true;
        }
        let _running = Running(self);
        match probe() {
            Ok(body) => {
                self.lock().last = Some(CachedProbe {
                    at: Instant::now(),
                    target: key,
                    body: body.clone(),
                });
                HttpResponse::json(200, body)
            }
            Err(resp) => resp,
        }
    }
}

/// Metadata of the vault's secrets, read without the master key: nothing is decrypted, so the
/// key file is neither read nor created, and a missing vault file reads as empty. (`Vault::open`
/// only needs a key to seal and open secret values.)
pub(crate) fn vault_metadata(
    home_override: Option<&Path>,
) -> Result<Vec<VaultSecretMetadata>, String> {
    Vault::open(resolve_vault_path(home_override), [0u8; MASTER_KEY_LEN])
        .map(|v| v.list())
        .map_err(|e| e.to_string())
}

/// Where this hub looks for the vendor CLIs: the `PATH` directories, and the home whose
/// `.local/bin` is searched after them.
///
/// A hub with a `home_override` is sandboxed (its tests): it looks only under that home's
/// `.local/bin`, never on the host's PATH, so a probe cannot run (and write the state of) the
/// real `claude`, `codex`, `grok`, `agy` or `muse` of whoever runs it.
pub(crate) fn cli_search_paths(state: &HubState) -> (String, Option<PathBuf>) {
    let home = state
        .config
        .home_override
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let path_env = if state.config.home_override.is_some() {
        String::new()
    } else {
        std::env::var("PATH").unwrap_or_default()
    };
    (path_env, home)
}

pub(crate) fn gather_inputs(state: &HubState) -> Result<ProbeInputs, HttpResponse> {
    // Snapshot under the store guard, then drop it before any filesystem or network probing.
    let snapshot = match state.open_store() {
        Ok(store) => snapshot_discovery_store(&store),
        Err(_) => {
            return Err(HttpResponse::json(
                500,
                json!({ "error": "failed to open store: store unavailable" }),
            ));
        }
    };
    let (path_env, home) = cli_search_paths(state);
    let sessions = discover_route_sessions_without_model_info(
        &snapshot,
        state.config.home_override.as_deref(),
    );
    let vault = vault_metadata(state.config.home_override.as_deref());
    Ok(ProbeInputs {
        path_env,
        home,
        sessions,
        vault,
        local_engines: default_local_engines(),
        now_ms: stamp::now().unix_millis(),
        home_override: state.config.home_override.clone(),
        quota_source: QuotaSource::HOST,
        quota_budget: QUOTA_BUDGET,
        quota_cache: Arc::clone(&state.quota_cache),
    })
}

fn run_probe(state: &HubState, target: Option<&str>) -> HttpResponse {
    state.probe_gate.run(target, || {
        let inputs = gather_inputs(state)?;
        match build_matrix(&inputs, target) {
            Ok(matrix) => Ok(serde_json::to_value(&matrix).unwrap_or_else(|_| json!({}))),
            Err(error) => Err(HttpResponse::json(400, json!({ "error": error }))),
        }
    })
}

/// `GET /api/v1/runtimes/matrix`: probe every runtime. Owner only, like the POST: a probe spawns
/// `<binary> --version` for binaries found on the hub's PATH.
pub fn handle_get_matrix(state: &HubState, authority: &HubAuthority) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(403, json!({ "error": "owner authority required" }));
    }
    run_probe(state, None)
}

/// `POST /api/v1/runtimes/probe` with optional `{ "target": "<tier|runtime|provider|engine>" }`.
/// Owner only: a probe spawns `<binary> --version` for binaries found on the hub's PATH.
pub fn handle_post_probe(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(403, json!({ "error": "owner authority required" }));
    }
    let body: Value = if req.body.iter().all(|b| b.is_ascii_whitespace()) {
        json!({})
    } else {
        match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                return HttpResponse::json(
                    400,
                    json!({ "error": format!("invalid JSON body: {e}") }),
                );
            }
        }
    };
    let target = match body.get("target") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return HttpResponse::json(400, json!({ "error": "`target` must be a string" }));
        }
    };
    run_probe(state, target.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use tempfile::TempDir;

    fn meta(name: &str, expires_at: Option<&str>) -> VaultSecretMetadata {
        VaultSecretMetadata {
            name: name.to_string(),
            description: None,
            allowed_hosts: vec![],
            allowed_methods: vec![],
            allowed_agents: vec![],
            allow_private_network: false,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
            lease_ttl_secs: None,
            expires_at: expires_at.map(str::to_string),
        }
    }

    fn session(adapter: &str, source: &str, status: &str) -> DiscoveredSessionSummary {
        DiscoveredSessionSummary {
            id: format!("{adapter}-{source}-{status}"),
            source: source.into(),
            adapter: adapter.into(),
            persona: None,
            status: status.into(),
            updated_at: None,
            cwd: None,
            record_id: None,
            transcript_path: None,
            quota_usage: None,
            model: None,
            thinking_level: None,
            available_models: Vec::new(),
            context_usage: None,
            execution_mode: None,
        }
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `run_version` on a script this test just wrote. Writing a script and executing it races
    /// with every other test thread that forks: a child forked while the script's write fd is
    /// still open holds that fd until its own exec, and `execve` of a file that any process has
    /// open for writing fails with ETXTBSY. The race lives in the fixture, so the retry lives
    /// here: only that exact spawn error is retried, bounded, and every other result is returned.
    #[cfg(unix)]
    fn run_fixture_version(path: &Path, timeout: Duration) -> Result<String, String> {
        let mut attempts = 0;
        loop {
            let result = run_version(path, timeout);
            let busy = matches!(&result, Err(e) if e.starts_with("spawn failed:") && e.contains("Text file busy"));
            attempts += 1;
            if !busy || attempts >= 200 {
                return result;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn inputs(path_env: &str, home: Option<&Path>) -> ProbeInputs {
        ProbeInputs {
            path_env: path_env.to_string(),
            home: home.map(Path::to_path_buf),
            sessions: Vec::new(),
            vault: Ok(Vec::new()),
            local_engines: Vec::new(),
            now_ms: 1_800_000_000_000,
            home_override: None,
            quota_source: QuotaSource::NONE,
            quota_budget: QUOTA_BUDGET,
            quota_cache: Arc::default(),
        }
    }

    /// One-shot loopback server answering the first connection with `response`.
    fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(response.as_bytes());
            }
        });
        port
    }

    fn spec(name: &str, port: u16, kind: LocalEngineKind) -> LocalEngineSpec {
        LocalEngineSpec {
            name: name.into(),
            port,
            path: "/v1/models".into(),
            kind,
        }
    }

    #[test]
    fn matrix_tables_match_the_spec() {
        assert_eq!(CLI_RUNTIMES, ["claude", "codex", "grok", "agy", "muse"]);
        let kinds: Vec<_> = CLI_KINDS.iter().map(|(n, _)| *n).collect();
        assert_eq!(kinds, CLI_RUNTIMES);
        for (name, kind) in CLI_KINDS {
            assert_eq!(kind.default_binary(), name);
        }
        let secrets: Vec<_> = VAULT_PROVIDERS.iter().map(|(_, s)| *s).collect();
        assert_eq!(
            secrets,
            [
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "XAI_API_KEY",
                "GEMINI_API_KEY",
                "OPENROUTER_API_KEY",
                "MOONSHOT_API_KEY"
            ]
        );
        let engines = default_local_engines();
        assert_eq!(engines.len(), 3);
        assert_eq!(endpoint_of(&engines[0]), "127.0.0.1:11434/api/tags");
        assert_eq!(endpoint_of(&engines[1]), "127.0.0.1:8000/v1/models");
        assert_eq!(endpoint_of(&engines[2]), "127.0.0.1:8097/v1/models");
    }

    #[test]
    fn vault_status_ready_missing_expired() {
        let metas = vec![
            meta("ANTHROPIC_API_KEY", None),
            meta("OPENAI_API_KEY", Some("2020-01-01T00:00:00.000Z")),
            meta("XAI_API_KEY", Some("2999-01-01T00:00:00.000Z")),
        ];
        let out = vault_statuses(&VAULT_PROVIDERS, &metas, 1_800_000_000_000);
        let by = |p: &str| out.iter().find(|s| s.provider == p).unwrap();
        assert_eq!(by("anthropic").status, "ready");
        assert!(by("anthropic").provisioned && !by("anthropic").expired);
        assert_eq!(by("openai").status, "expired");
        assert!(by("openai").provisioned && by("openai").expired);
        assert_eq!(by("xai").status, "ready");
        assert_eq!(by("gemini").status, "missing");
        assert!(!by("openrouter").provisioned);
    }

    #[test]
    fn vault_status_never_serializes_secret_material() {
        let out = vault_statuses(&VAULT_PROVIDERS, &[meta("OPENAI_API_KEY", None)], 0);
        let json = serde_json::to_string(&out).unwrap();
        assert!(!json.contains("value"));
        assert!(!json.contains("nonce"));
        assert!(!json.contains("ciphertext"));
    }

    #[test]
    fn find_binary_checks_path_then_home_local_bin() {
        let path_dir = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        #[cfg(unix)]
        {
            write_script(path_dir.path(), "claude", "exit 0");
            write_script(&bin, "codex", "exit 0");
            write_script(&bin, "claude", "exit 0");
            let p = path_dir.path().to_str().unwrap();
            assert_eq!(
                find_binary("claude", p, Some(home.path())).unwrap(),
                path_dir.path().join("claude")
            );
            assert_eq!(
                find_binary("codex", p, Some(home.path())).unwrap(),
                bin.join("codex")
            );
            assert!(find_binary("grok", p, Some(home.path())).is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn find_binary_skips_non_executable_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agy"), "x").unwrap();
        assert!(find_binary("agy", dir.path().to_str().unwrap(), None).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_reads_first_line() {
        let dir = TempDir::new().unwrap();
        let p = write_script(
            dir.path(),
            "claude",
            "echo '1.2.3 (Claude Code)'; echo second",
        );
        assert_eq!(
            run_fixture_version(&p, Duration::from_secs(2)).unwrap(),
            "1.2.3 (Claude Code)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_falls_back_to_stderr_and_reports_failure() {
        let dir = TempDir::new().unwrap();
        let p = write_script(dir.path(), "a", "echo v9 >&2");
        assert_eq!(
            run_fixture_version(&p, Duration::from_secs(2)).unwrap(),
            "v9"
        );
        let bad = write_script(dir.path(), "b", "exit 3");
        assert!(
            run_fixture_version(&bad, Duration::from_secs(2))
                .unwrap_err()
                .contains("exited")
        );
        let empty = write_script(dir.path(), "c", "exit 0");
        assert!(
            run_fixture_version(&empty, Duration::from_secs(2))
                .unwrap_err()
                .contains("nothing")
        );
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_times_out_and_kills_the_process_group() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("grandchild-alive");
        // The grandchild would create the marker after 1s if it survived the group kill.
        let body = format!("(sleep 1; touch '{}') &\nsleep 30", marker.display());
        let p = write_script(dir.path(), "slow", &body);
        let started = Instant::now();
        let err = run_fixture_version(&p, Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(1400));
        assert!(!marker.exists(), "grandchild survived the group kill");
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_kills_a_grandchild_left_behind_by_a_normal_exit() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("grandchild-alive");
        // The script exits at once, but its background child would touch the marker after 1s.
        let body = format!("(sleep 1; touch '{}') &\necho v1", marker.display());
        let p = write_script(dir.path(), "wrapper", &body);
        assert_eq!(
            run_fixture_version(&p, Duration::from_secs(2)).unwrap(),
            "v1"
        );
        std::thread::sleep(Duration::from_millis(1400));
        assert!(!marker.exists(), "grandchild survived the group kill");
    }

    #[cfg(unix)]
    #[test]
    fn noticing_an_exit_does_not_reap_the_leader() {
        let mut child = Command::new("true").spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !exited_unreaped(&mut child).unwrap() {
            assert!(Instant::now() < deadline, "child never exited");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Still waitable: its pid is not free for reuse until `wait` below.
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn the_probe_gate_refuses_a_concurrent_probe_and_reuses_a_fresh_result() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let gate = ProbeGate::default();
        let calls = AtomicUsize::new(0);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let (gate, calls) = (&gate, &calls);
            let first = scope.spawn(move || {
                gate.run(Some("cli"), move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    started_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(5));
                    Ok(json!({ "n": 1 }))
                })
            });
            started_rx.recv().unwrap();
            // A second probe while the first runs spawns nothing.
            let second = gate.run(Some("cli"), || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "n": 2 }))
            });
            assert_eq!(second.status, 429);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            release_tx.send(()).unwrap();
            assert_eq!(first.join().unwrap().status, 200);
        });
        // A fresh result for the same target (however spelled) is served again.
        let again = gate.run(Some(" CLI "), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "n": 3 }))
        });
        assert_eq!(again.status, 200);
        assert_eq!(again.json_value().unwrap(), json!({ "n": 1 }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Another target probes. An error is returned as it is and is not remembered.
        let other = gate.run(Some("vault"), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(HttpResponse::json(400, json!({ "error": "no" })))
        });
        assert_eq!(other.status, 400);
        let retry = gate.run(Some("vault"), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "n": 4 }))
        });
        assert_eq!(retry.json_value().unwrap(), json!({ "n": 4 }));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn reading_vault_metadata_creates_neither_a_key_nor_a_vault_file() {
        let home = TempDir::new().unwrap();
        assert!(vault_metadata(Some(home.path())).unwrap().is_empty());
        assert!(!fl_gate::resolve_master_key_path(Some(home.path())).exists());
        assert!(!resolve_vault_path(Some(home.path())).exists());
    }

    #[cfg(unix)]
    #[test]
    fn cli_probe_counts_routes_and_sessions_per_adapter() {
        let dir = TempDir::new().unwrap();
        write_script(dir.path(), "claude", "echo 2.0.0");
        let mut inp = inputs(dir.path().to_str().unwrap(), None);
        inp.sessions = vec![
            session("claude", "route", "active"),
            session("claude", "attempt", "running"),
            session("claude", "attempt", "succeeded"),
            session("codex", "route", "active"),
        ];
        let claude = probe_cli("claude", &inp, Duration::from_secs(2));
        assert!(claude.installed);
        assert_eq!(claude.version.as_deref(), Some("2.0.0"));
        assert_eq!(claude.active_routes, 1);
        assert_eq!(claude.live_sessions, 2);
        let codex = probe_cli("codex", &inp, Duration::from_secs(2));
        assert!(!codex.installed);
        assert!(codex.error.as_deref().unwrap().contains("not found"));
        assert_eq!((codex.active_routes, codex.live_sessions), (1, 1));
    }

    #[test]
    fn local_probe_reads_ollama_and_openai_model_lists() {
        let port = serve_once(
            "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"models\":[{\"name\":\"llama3:8b\"},{\"name\":\"qwen3\"}]}",
        );
        let st = probe_local_engine(
            &spec("ollama", port, LocalEngineKind::Ollama),
            LOCAL_TIMEOUT,
        );
        assert!(st.reachable, "{st:?}");
        assert!(st.latency_ms.is_some());
        assert_eq!(st.models, ["llama3:8b", "qwen3"]);

        let port = serve_once("HTTP/1.1 200 OK\r\n\r\n{\"data\":[{\"id\":\"Qwen3-8B\"}]}");
        let st = probe_local_engine(
            &spec("lemonade-8000", port, LocalEngineKind::OpenAiModels),
            LOCAL_TIMEOUT,
        );
        assert_eq!(st.models, ["Qwen3-8B"]);
    }

    #[test]
    fn local_probe_reports_http_errors_and_garbage_bodies() {
        let port = serve_once("HTTP/1.0 503 Service Unavailable\r\n\r\nno");
        let st = probe_local_engine(
            &spec("x", port, LocalEngineKind::OpenAiModels),
            LOCAL_TIMEOUT,
        );
        assert!(!st.reachable);
        assert_eq!(st.error.as_deref(), Some("HTTP 503"));

        let port = serve_once("HTTP/1.0 200 OK\r\n\r\nnot json");
        let st = probe_local_engine(
            &spec("x", port, LocalEngineKind::OpenAiModels),
            LOCAL_TIMEOUT,
        );
        assert!(st.reachable);
        assert!(st.models.is_empty());
    }

    #[test]
    fn local_probe_refused_port_is_unreachable() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let st = probe_local_engine(&spec("gone", port, LocalEngineKind::Ollama), LOCAL_TIMEOUT);
        assert!(!st.reachable);
        assert_eq!(st.latency_ms, None);
        assert!(st.error.is_some());
    }

    #[test]
    fn local_probe_times_out_on_a_silent_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hold = std::thread::spawn(move || {
            let conn = listener.accept();
            std::thread::sleep(Duration::from_millis(900));
            drop(conn);
        });
        let started = Instant::now();
        let st = probe_local_engine(&spec("slow", port, LocalEngineKind::Ollama), LOCAL_TIMEOUT);
        assert!(!st.reachable);
        assert!(st.error.as_deref().unwrap().contains("timed out"), "{st:?}");
        assert!(started.elapsed() < Duration::from_millis(800));
        hold.join().unwrap();
    }

    #[test]
    fn model_ids_are_capped() {
        let items: Vec<String> = (0..300).map(|i| format!("{{\"id\":\"m{i}\"}}")).collect();
        let body = format!("{{\"data\":[{}]}}", items.join(","));
        assert_eq!(
            parse_model_ids(LocalEngineKind::OpenAiModels, &body).len(),
            MAX_LOCAL_MODELS
        );
    }

    #[test]
    fn target_filter_selects_tiers_and_items() {
        let engines = default_local_engines();
        let sel = select(Some("cli"), &engines).unwrap();
        assert_eq!((sel.cli.len(), sel.vault.len(), sel.local.len()), (5, 0, 0));
        let sel = select(Some(" Codex "), &engines).unwrap();
        assert_eq!(sel.cli, ["codex"]);
        assert!(sel.vault.is_empty() && sel.local.is_empty());
        let sel = select(Some("openai"), &engines).unwrap();
        assert_eq!(sel.vault, [("openai", "OPENAI_API_KEY")]);
        let sel = select(Some("lemonade-8097"), &engines).unwrap();
        assert_eq!(sel.local.len(), 1);
        let sel = select(Some(""), &engines).unwrap();
        assert_eq!((sel.cli.len(), sel.vault.len(), sel.local.len()), (5, 6, 3));
        assert!(select(Some("nope"), &engines).is_err());
        assert!(select(Some(&"x".repeat(200)), &engines).is_err());
    }

    #[test]
    fn build_matrix_filters_and_survives_vault_failure() {
        let mut inp = inputs("", None);
        inp.vault = Err("vault unreadable".into());
        let m = build_matrix(&inp, Some("vault")).unwrap();
        assert!(m.cli_runtimes.is_empty() && m.local_engines.is_empty());
        assert_eq!(m.vault_providers.len(), 6);
        assert!(m.vault_providers.iter().all(|p| !p.provisioned));
        assert_eq!(m.vault_error.as_deref(), Some("vault unreadable"));

        let port = serve_once("HTTP/1.0 200 OK\r\n\r\n{\"data\":[{\"id\":\"m\"}]}");
        inp.local_engines = vec![spec("lemonade-8000", port, LocalEngineKind::OpenAiModels)];
        let m = build_matrix(&inp, Some("lemonade-8000")).unwrap();
        assert_eq!(m.local_engines.len(), 1);
        assert!(m.local_engines[0].reachable);
        assert!(build_matrix(&inp, Some("bogus")).is_err());

        let full = build_matrix(&inputs("", None), None).unwrap();
        assert_eq!(full.cli_runtimes.len(), 5);
        assert_eq!(full.vault_providers.len(), 6);
    }

    fn custom_vendor(id: &str, key: Option<&str>) {
        fl_core::ensure_vendor_registered(VendorDescriptor {
            id: id.to_string(),
            display_name: format!("Custom {id}"),
            transport: VendorTransport::HttpApi {
                default_base_url: "https://example.org/v1".to_string(),
                api_dialect: fl_core::ApiDialect::AnthropicMessages,
                vault_secret_name: key.map(str::to_string),
                env_key_name: key.map(str::to_string),
            },
            default_model: "custom-model".to_string(),
            default_context_window: 54_321,
            default_rate_card: fl_core::telemetry::RateCard::default(),
            mcp_config_rel_path: Some(".custom/mcp.json".to_string()),
            slice_bounds: Vec::new(),
        })
        .unwrap();
    }

    fn vendor_ids(m: &RuntimeMatrix) -> Vec<&str> {
        m.vendors.iter().map(|v| v.id.as_str()).collect()
    }

    #[test]
    fn kimi_is_a_vault_provider_and_target_filters_select_it() {
        let engines = default_local_engines();
        for target in ["kimi", "MOONSHOT_API_KEY", "moonshot_api_key", "kimi-api"] {
            let sel = select(Some(target), &engines).unwrap();
            assert_eq!(sel.vault, [("kimi", "MOONSHOT_API_KEY")], "{target}");
            assert!(sel.cli.is_empty() && sel.local.is_empty(), "{target}");
            let ids: Vec<_> = sel.vendors.iter().map(|d| d.id.as_str()).collect();
            assert_eq!(ids, ["kimi-api"], "{target}");
        }
        let metas = vec![meta("MOONSHOT_API_KEY", None)];
        let out = vault_statuses(&VAULT_PROVIDERS, &metas, 1_800_000_000_000);
        let kimi = out.iter().find(|s| s.provider == "kimi").unwrap();
        assert_eq!((kimi.status.as_str(), kimi.provisioned), ("ready", true));
        let m = build_matrix(&inputs("", None), Some("kimi")).unwrap();
        assert_eq!(m.vault_providers.len(), 1);
        assert_eq!(m.vault_providers[0].secret_name, "MOONSHOT_API_KEY");
    }

    #[test]
    fn the_matrix_lists_the_registered_vendor_catalog_sorted_by_id() {
        let m = build_matrix(&inputs("", None), None).unwrap();
        let ids = vendor_ids(&m);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        for builtin in [
            "anthropic-api",
            "antigravity",
            "claude",
            "codex",
            "gemini-api",
            "grok",
            "kimi-api",
            "lemonade",
            "muse",
            "ollama",
            "openai-api",
            "openrouter-api",
            "xai-api",
        ] {
            assert!(ids.contains(&builtin), "{builtin} in {ids:?}");
        }
        let by = |id: &str| m.vendors.iter().find(|v| v.id == id).unwrap();

        let kimi = by("kimi-api");
        assert_eq!(kimi.display_name, "Kimi (Moonshot) API");
        assert_eq!(kimi.transport, "http_api");
        assert_eq!(kimi.default_model, "kimi-k2.5");
        assert_eq!(kimi.default_context_window, 256_000);
        assert!(kimi.needs_api_key && kimi.builtin);
        assert_eq!(kimi.base_url.as_deref(), Some("https://api.moonshot.ai/v1"));
        assert_eq!(kimi.dialect.as_deref(), Some("openai_chat_completions"));
        assert_eq!(kimi.mcp_config_rel_path, None);

        let claude = by("claude");
        assert_eq!(claude.transport, "subscription_cli");
        assert!(!claude.needs_api_key && claude.builtin);
        assert_eq!(
            (claude.base_url.as_deref(), claude.dialect.as_deref()),
            (None, None)
        );
        assert_eq!(claude.mcp_config_rel_path.as_deref(), Some(".mcp.json"));

        let ollama = by("ollama");
        assert!(
            !ollama.needs_api_key && ollama.transport == "http_api" && ollama.base_url.is_some()
        );
        assert_eq!(
            by("anthropic-api").dialect.as_deref(),
            Some("anthropic_messages")
        );
        assert_eq!(
            by("gemini-api").dialect.as_deref(),
            Some("gemini_generate_content")
        );
    }

    #[test]
    fn a_custom_registered_vendor_appears_in_the_catalog() {
        custom_vendor("rtm-custom-keyed", Some("RTM_CUSTOM_KEY"));
        let m = build_matrix(&inputs("", None), None).unwrap();
        let entry = m
            .vendors
            .iter()
            .find(|v| v.id == "rtm-custom-keyed")
            .unwrap();
        assert_eq!(entry.display_name, "Custom rtm-custom-keyed");
        assert_eq!(entry.transport, "http_api");
        assert_eq!(entry.default_model, "custom-model");
        assert_eq!(entry.default_context_window, 54_321);
        assert!(entry.needs_api_key && !entry.builtin);
        assert_eq!(entry.base_url.as_deref(), Some("https://example.org/v1"));
        assert_eq!(entry.dialect.as_deref(), Some("anthropic_messages"));
        assert_eq!(
            entry.mcp_config_rel_path.as_deref(),
            Some(".custom/mcp.json")
        );
        // It is selected by its id, and its key name selects it with the Vault entry it shares.
        let m = build_matrix(&inputs("", None), Some("rtm-custom-keyed")).unwrap();
        assert_eq!(vendor_ids(&m), ["rtm-custom-keyed"]);
        assert!(
            m.cli_runtimes.is_empty() && m.vault_providers.is_empty() && m.local_engines.is_empty()
        );
    }

    #[test]
    fn every_registered_vendor_reports_native_and_unproven_acp_profiles() {
        custom_vendor("rtm-acp-custom", None);
        fl_core::ensure_vendor_registered(VendorDescriptor {
            id: "rtm-acp-custom-cli".into(),
            display_name: "Custom CLI".into(),
            transport: VendorTransport::SubscriptionCli {
                default_binary: "custom-cli".into(),
            },
            default_model: String::new(),
            default_context_window: 0,
            default_rate_card: fl_core::telemetry::RateCard::default(),
            mcp_config_rel_path: None,
            slice_bounds: Vec::new(),
        })
        .unwrap();
        let matrix = build_matrix(&inputs("", None), Some("vendors")).unwrap();
        let output = serde_json::to_value(&matrix).unwrap();
        let entries = output["vendors"].as_array().unwrap();
        for id in [
            "claude",
            "codex",
            "grok",
            "antigravity",
            "muse",
            "rtm-acp-custom",
            "rtm-acp-custom-cli",
        ] {
            assert!(entries.iter().any(|entry| entry["id"] == id), "{id}");
        }
        for entry in entries {
            assert_eq!(
                entry["hosted_protocol_profiles"],
                json!([
                    {"kind": "native", "active": true},
                    {"kind": "acp_v1_stdio", "active": false},
                ]),
                "{}",
                entry["id"]
            );
            assert_eq!(entry["acp_admission"]["state"], "unproven");
            assert_eq!(entry["acp_admission"]["production_admitted"], false);
            let operations = entry["acp_admission"]["operations"].as_array().unwrap();
            assert_eq!(operations.len(), 6);
            for (row, name) in operations.iter().zip([
                "approvals",
                "interrupt",
                "reset",
                "compact",
                "thread_identity",
                "usage",
            ]) {
                assert_eq!(
                    row,
                    &json!({
                        "operation": name,
                        "state": "unproven",
                        "required_capabilities": [],
                        "mechanism": "",
                        "semantic_constraints": [],
                        "test_evidence_refs": [],
                    }),
                    "{}",
                    entry["id"]
                );
            }
        }
    }

    #[test]
    fn the_target_filter_applies_to_the_vendor_catalog() {
        custom_vendor("rtm-custom-keyless", None);
        let by_target = |t: &str| {
            let m = build_matrix(&inputs("", None), Some(t)).unwrap();
            m.vendors.into_iter().map(|v| v.id).collect::<Vec<_>>()
        };
        let has = |ids: &[String], id: &str| ids.iter().any(|i| i == id);
        let cli = by_target("cli");
        assert!(has(&cli, "claude") && has(&cli, "antigravity") && !has(&cli, "kimi-api"));
        let vault = by_target("vault");
        assert!(has(&vault, "kimi-api") && has(&vault, "openai-api"));
        assert!(
            !has(&vault, "ollama") && !has(&vault, "claude") && !has(&vault, "rtm-custom-keyless")
        );
        let local = by_target("local");
        assert!(
            has(&local, "ollama") && has(&local, "lemonade") && has(&local, "rtm-custom-keyless")
        );
        assert!(!has(&local, "kimi-api"));
        assert_eq!(by_target("agy"), ["antigravity"]);
        assert_eq!(by_target("codex"), ["codex"]);
        assert_eq!(by_target("openai"), ["openai-api"]);
        assert_eq!(by_target("ollama"), ["ollama"]);
        assert_eq!(by_target("OPENROUTER-API"), ["openrouter-api"]);
        assert!(by_target("vendors").len() >= 13);
        // `vendors` is the catalog alone: no probe ran.
        let m = build_matrix(&inputs("", None), Some("vendors")).unwrap();
        assert!(
            m.cli_runtimes.is_empty() && m.vault_providers.is_empty() && m.local_engines.is_empty()
        );
        assert!(build_matrix(&inputs("", None), Some("no-such-vendor")).is_err());
    }

    #[test]
    fn the_matrix_json_keeps_its_fields_and_carries_no_secret_material() {
        custom_vendor("rtm-custom-json", Some("RTM_JSON_KEY"));
        let mut inp = inputs("", None);
        // A vault holding a key: only its metadata may surface.
        inp.vault = Ok(vec![meta("MOONSHOT_API_KEY", None)]);
        let m = build_matrix(&inp, None).unwrap();
        let json = serde_json::to_value(&m).unwrap();
        for field in [
            "generated_at_ms",
            "cli_runtimes",
            "vault_providers",
            "local_engines",
            "vault_error",
            "vendors",
        ] {
            assert!(json.get(field).is_some(), "{field}");
        }
        let vendors = json["vendors"].as_array().unwrap();
        let kimi = vendors.iter().find(|v| v["id"] == "kimi-api").unwrap();
        let mut keys: Vec<&str> = kimi
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "acp_admission",
                "base_url",
                "builtin",
                "default_context_window",
                "default_model",
                "dialect",
                "display_name",
                "hosted_protocol_profiles",
                "id",
                "mcp_config_rel_path",
                "needs_api_key",
                "transport",
            ]
        );
        let text = json["vendors"].to_string();
        for needle in [
            "MOONSHOT_API_KEY",
            "RTM_JSON_KEY",
            "secret",
            "value",
            "nonce",
            "ciphertext",
            "key_name",
        ] {
            assert!(!text.contains(needle), "{needle} in vendors JSON: {text}");
        }
        assert!(
            json["vault_providers"]
                .to_string()
                .contains("MOONSHOT_API_KEY")
        );
    }

    // ------------------------------------------------------------ vendor quotas

    use crate::discovery::QuotaWindowUsage;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn window(used: f64, minutes: u64) -> QuotaWindowUsage {
        QuotaWindowUsage {
            used_percent: used,
            window_minutes: Some(minutes),
            resets_at_ms: Some(1_800_000_600_000),
        }
    }

    fn quota(
        tier: Option<&str>,
        primary: Option<f64>,
        secondary: Option<f64>,
    ) -> SessionQuotaUsage {
        SessionQuotaUsage {
            tier: tier.map(str::to_string),
            primary_window: primary.map(|u| window(u, 300)),
            secondary_window: secondary.map(|u| window(u, 10_080)),
            exhausted_reason: None,
        }
    }

    fn quota_session(
        adapter: &str,
        id: &str,
        updated_at: &str,
        q: Option<SessionQuotaUsage>,
    ) -> DiscoveredSessionSummary {
        let mut s = session(adapter, "route", "active");
        s.id = id.to_string();
        s.updated_at = Some(updated_at.to_string());
        s.quota_usage = q;
        s
    }

    fn only_cli(inp: &ProbeInputs, target: &str) -> RuntimeMatrix {
        build_matrix(inp, Some(target)).unwrap()
    }

    #[test]
    fn a_vendor_quota_is_the_richest_session_reading_and_the_account_is_not_probed() {
        static ACCOUNT_CALLS: AtomicUsize = AtomicUsize::new(0);
        let mut inp = inputs("", None);
        inp.quota_source = QuotaSource {
            session: |_, _| None,
            account: |_, _| {
                ACCOUNT_CALLS.fetch_add(1, Ordering::SeqCst);
                None
            },
        };
        inp.sessions = vec![
            // Newest, but it knows only the tier.
            quota_session(
                "claude",
                "c-new",
                "2026-11-02T10:00:00Z",
                Some(quota(Some("max"), None, None)),
            ),
            // Older, with both windows: the most data wins.
            quota_session(
                "claude",
                "c-old",
                "2026-11-01T10:00:00Z",
                Some(quota(Some("max"), Some(40.0), Some(10.0))),
            ),
            quota_session(
                "codex",
                "x1",
                "2026-11-01T10:00:00Z",
                Some(quota(None, Some(5.0), None)),
            ),
        ];
        let m = only_cli(&inp, "claude");
        let claude = &m.vendor_quotas["claude"];
        assert_eq!(claude.primary_window.as_ref().unwrap().used_percent, 40.0);
        assert_eq!(claude.secondary_window.as_ref().unwrap().used_percent, 10.0);
        // A target selects its own vendor only.
        assert_eq!(m.vendor_quotas.keys().collect::<Vec<_>>(), ["claude"]);
        assert_eq!(ACCOUNT_CALLS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn equally_rich_readings_resolve_to_the_newest_session() {
        let mut inp = inputs("", None);
        inp.sessions = vec![
            quota_session(
                "claude",
                "older",
                "2026-11-01T10:00:00Z",
                Some(quota(None, Some(90.0), None)),
            ),
            quota_session(
                "claude",
                "newer",
                "2026-11-02T10:00:00Z",
                Some(quota(None, Some(12.0), None)),
            ),
        ];
        let m = only_cli(&inp, "claude");
        assert_eq!(
            m.vendor_quotas["claude"]
                .primary_window
                .as_ref()
                .unwrap()
                .used_percent,
            12.0
        );
    }

    #[test]
    fn a_route_session_with_no_reading_is_read_from_disk_but_only_a_route_and_only_a_few() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        let mut inp = inputs("", None);
        inp.quota_source = QuotaSource {
            session: |s, _| {
                READS.fetch_add(1, Ordering::SeqCst);
                (s.id == "route-3").then(|| quota(Some("pro"), Some(7.0), Some(1.0)))
            },
            account: |_, _| None,
        };
        inp.sessions = (0..8)
            .map(|i| {
                quota_session(
                    "grok",
                    &format!("route-{i}"),
                    &format!("2026-11-0{}T00:00:00Z", i + 1),
                    None,
                )
            })
            .collect();
        let mut attempt = session("grok", "attempt", "running");
        attempt.updated_at = Some("2026-12-01T00:00:00Z".into());
        inp.sessions.push(attempt);
        let m = only_cli(&inp, "grok");
        // Newest first: route-7, 6, 5, 4 are read and hold nothing, so route-3 is out of reach.
        assert_eq!(READS.load(Ordering::SeqCst), MAX_QUOTA_SESSIONS_PER_ADAPTER);
        assert!(m.vendor_quotas.is_empty(), "{:?}", m.vendor_quotas);

        READS.store(0, Ordering::SeqCst);
        // The first result is cached: this is a different set of sessions.
        inp.quota_cache = Arc::default();
        inp.sessions
            .retain(|s| s.id != "route-7" && s.id != "route-6");
        let m = only_cli(&inp, "grok");
        assert_eq!(m.vendor_quotas["grok"].tier.as_deref(), Some("pro"));
    }

    #[test]
    fn the_account_probe_fills_in_a_vendor_no_session_reports() {
        static HOME_SEEN: Mutex<Option<Option<PathBuf>>> = Mutex::new(None);
        static ADAPTERS: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let mut inp = inputs("", None);
        inp.home_override = Some(PathBuf::from("/sandbox/home"));
        inp.quota_source = QuotaSource {
            session: |_, _| None,
            account: |adapter, home| {
                ADAPTERS.lock().unwrap().push(adapter.to_string());
                *HOME_SEEN.lock().unwrap() = Some(home.map(Path::to_path_buf));
                (adapter == "antigravity").then(|| quota(Some("ultra"), Some(20.0), None))
            },
        };
        // A claude session that reports nothing does not stop the account probe either.
        inp.sessions = vec![quota_session("claude", "c", "2026-11-02T00:00:00Z", None)];
        let m = build_matrix(&inp, Some("cli")).unwrap();
        assert_eq!(m.vendor_quotas.keys().collect::<Vec<_>>(), ["antigravity"]);
        assert_eq!(
            m.vendor_quotas["antigravity"].tier.as_deref(),
            Some("ultra")
        );
        let mut adapters = ADAPTERS.lock().unwrap().clone();
        adapters.sort();
        // `agy` is probed under its adapter id, once per selected vendor.
        assert_eq!(adapters, ["antigravity", "claude", "codex", "grok", "muse"]);
        // The sandboxed home reaches the reader: the real HOME is never consulted.
        assert_eq!(
            HOME_SEEN.lock().unwrap().clone(),
            Some(Some(PathBuf::from("/sandbox/home")))
        );
    }

    #[test]
    fn a_reading_with_no_data_is_not_a_quota() {
        static ACCOUNT: AtomicUsize = AtomicUsize::new(0);
        let mut inp = inputs("", None);
        inp.quota_source = QuotaSource {
            session: |_, _| None,
            account: |_, _| {
                ACCOUNT.fetch_add(1, Ordering::SeqCst);
                Some(quota(Some("  "), None, None))
            },
        };
        inp.sessions = vec![quota_session(
            "muse",
            "m",
            "2026-11-02T00:00:00Z",
            Some(quota(None, None, None)),
        )];
        let m = only_cli(&inp, "muse");
        assert!(m.vendor_quotas.is_empty());
        assert_eq!(
            ACCOUNT.load(Ordering::SeqCst),
            1,
            "the empty session reading fell through"
        );
    }

    #[test]
    fn no_sessions_and_no_account_data_is_an_empty_map_that_still_serializes() {
        let m = build_matrix(&inputs("", None), None).unwrap();
        assert!(m.vendor_quotas.is_empty());
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["vendor_quotas"], serde_json::json!({}));
    }

    #[test]
    fn a_tier_that_selects_no_cli_runtime_reads_no_quota() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let mut inp = inputs("", None);
        inp.quota_source = QuotaSource {
            session: |_, _| {
                CALLS.fetch_add(1, Ordering::SeqCst);
                None
            },
            account: |_, _| {
                CALLS.fetch_add(1, Ordering::SeqCst);
                Some(quota(Some("x"), Some(1.0), None))
            },
        };
        inp.sessions = vec![quota_session("claude", "c", "2026-11-02T00:00:00Z", None)];
        for target in ["vault", "local", "vendors", "openai"] {
            let m = build_matrix(&inp, Some(target)).unwrap();
            assert!(m.vendor_quotas.is_empty(), "{target}");
        }
        assert_eq!(CALLS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_matrix_json_carries_vendor_quotas_and_update_capability() {
        let mut inp = inputs("", None);
        inp.sessions = vec![quota_session(
            "claude",
            "c",
            "2026-11-02T00:00:00Z",
            Some(quota(Some("max"), Some(40.0), Some(10.0))),
        )];
        let json = serde_json::to_value(build_matrix(&inp, None).unwrap()).unwrap();
        let claude_quota = &json["vendor_quotas"]["claude"];
        assert_eq!(claude_quota["tier"], "max");
        assert_eq!(claude_quota["primary_window"]["used_percent"], 40.0);
        assert_eq!(claude_quota["secondary_window"]["window_minutes"], 10_080);
        assert!(claude_quota.get("exhausted_reason").is_none());

        let cli = |name: &str| {
            json["cli_runtimes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == name)
                .unwrap()
                .clone()
        };
        for name in ["claude", "codex", "grok", "agy"] {
            let row = cli(name);
            assert_eq!(row["update_supported"], true, "{name}");
            assert_eq!(row["update_command"], format!("{name} update"), "{name}");
        }
        // Whether the binary is installed does not change what the CLI can do.
        assert_eq!(cli("claude")["installed"], false);
        let muse = cli("muse");
        assert_eq!(muse["update_supported"], false);
        assert_eq!(muse["update_command"], serde_json::Value::Null);
    }

    fn window_resetting(used: f64, resets_at_ms: u64) -> QuotaWindowUsage {
        QuotaWindowUsage {
            used_percent: used,
            window_minutes: Some(300),
            resets_at_ms: Some(resets_at_ms),
        }
    }

    #[test]
    fn a_window_that_already_reset_does_not_make_an_old_reading_richer() {
        let mut inp = inputs("", None);
        let past = inp.now_ms - 1;
        let stale_but_rich = SessionQuotaUsage {
            tier: None,
            primary_window: Some(window_resetting(95.0, past)),
            secondary_window: Some(window_resetting(80.0, past)),
            exhausted_reason: None,
        };
        inp.sessions = vec![
            quota_session(
                "claude",
                "old",
                "2026-10-01T10:00:00Z",
                Some(stale_but_rich),
            ),
            quota_session(
                "claude",
                "new",
                "2026-11-02T10:00:00Z",
                Some(quota(None, Some(12.0), None)),
            ),
        ];
        let m = only_cli(&inp, "claude");
        let claude = &m.vendor_quotas["claude"];
        assert_eq!(claude.primary_window.as_ref().unwrap().used_percent, 12.0);

        // A reading that is stale all through is not a quota: the account probe is asked instead.
        inp.sessions.truncate(1);
        inp.quota_cache = Arc::default();
        let m = only_cli(&inp, "claude");
        assert!(m.vendor_quotas.is_empty(), "{:?}", m.vendor_quotas);
    }

    #[test]
    fn a_slow_quota_source_is_left_out_and_does_not_hold_the_matrix() {
        let mut inp = inputs("", None);
        inp.quota_budget = Duration::from_millis(300);
        inp.quota_source = QuotaSource {
            session: |_, _| None,
            account: |adapter, _| {
                if adapter == "grok" {
                    std::thread::sleep(Duration::from_millis(1500));
                }
                Some(quota(Some("pro"), Some(1.0), None))
            },
        };
        let began = Instant::now();
        let m = build_matrix(&inp, Some("cli")).unwrap();
        let took = began.elapsed();
        assert!(
            took < Duration::from_millis(1200),
            "the matrix waited {took:?} for a slow quota read"
        );
        assert_eq!(
            m.vendor_quotas.keys().collect::<Vec<_>>(),
            ["antigravity", "claude", "codex", "muse"]
        );
        assert_eq!(m.cli_runtimes.len(), CLI_RUNTIMES.len());
        // The slow read finishes on its own and is kept for the next request.
        let late = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            inp.quota_cache
                .get(&inp.home_override, "grok", inp.now_ms)
                .is_some()
        });
        assert!(late, "the late reading never reached the cache");
        let began = Instant::now();
        let again = build_matrix(&inp, Some("cli")).unwrap();
        assert!(began.elapsed() < Duration::from_millis(1000));
        assert!(again.vendor_quotas.contains_key("grok"));
    }

    #[test]
    fn a_reading_is_served_from_the_cache_for_thirty_seconds_per_home_and_adapter() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        let mut inp = inputs("", None);
        inp.quota_source = QuotaSource {
            session: |_, _| None,
            account: |_, _| {
                READS.fetch_add(1, Ordering::SeqCst);
                Some(quota(Some("max"), Some(3.0), None))
            },
        };
        let first = only_cli(&inp, "claude");
        assert_eq!(READS.load(Ordering::SeqCst), 1);
        inp.now_ms += QUOTA_CACHE_TTL_MS - 1;
        let second = only_cli(&inp, "claude");
        assert_eq!(READS.load(Ordering::SeqCst), 1, "served from the cache");
        assert_eq!(first.vendor_quotas, second.vendor_quotas);

        // Another home is another entry.
        inp.home_override = Some(PathBuf::from("/sandbox/other"));
        only_cli(&inp, "claude");
        assert_eq!(READS.load(Ordering::SeqCst), 2);

        // Past the TTL the source is read again.
        inp.home_override = None;
        inp.now_ms += 2;
        only_cli(&inp, "claude");
        assert_eq!(READS.load(Ordering::SeqCst), 3);
    }
}
