//! Host session & transcript discovery, git worktree & diff inspection, and Cargo
//! workspace architecture conformance scanning (spec §1.3, §3.2, §3.3).

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

/// Read agent names and attempt rows. Does not walk the filesystem.
pub fn snapshot_discovery_store(store: &AnyStore) -> DiscoveryStoreSnapshot {
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

/// Discover sessions like [`discover_sessions_from_snapshot`], but leave `model`,
/// `thinking_level` and `available_models` empty. For callers that only look a session up
/// by `id`, `persona` or `source`: it never reads a model file or transcript tail and never
/// calls a language server.
pub fn discover_route_sessions_without_model_info(
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
) -> Vec<DiscoveredSessionSummary> {
    discover_sessions_impl(snap, home_override, false)
}

/// Shared body of [`discover_sessions_from_snapshot`]. `with_model_info` is false for
/// callers that only look a session up by id, so they never read model files or wait on
/// a language server.
fn discover_sessions_impl(
    snap: &DiscoveryStoreSnapshot,
    home_override: Option<&Path>,
    with_model_info: bool,
) -> Vec<DiscoveredSessionSummary> {
    let mut out = Vec::new();
    let mut agy_endpoints: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    let mut seen_ids = BTreeSet::new();

    let route_override =
        home_override.map(|h| h.join(".cache").join("agent-bus").join("routes.json"));
    let mut routes = RouteTable::load(route_override.as_deref());
    routes.discover_routes(home_override, stamp::now());

    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));

    let registered_names = &snap.registered_names;

    let mut ordered_routes: Vec<_> = routes.entries.iter().collect();
    ordered_routes.sort_by_key(|(persona, _)| {
        let is_registered = registered_names.contains(&persona.to_ascii_lowercase());
        (!is_registered, *persona)
    });

    for (persona, entry) in ordered_routes {
        let (adapter, raw_id, cwd, transcript_path) = match &entry.route {
            VendorRoute::Claude {
                pid, session_id, ..
            } => {
                let id = if session_id.trim().is_empty() {
                    format!("claude-{persona}-{pid}")
                } else {
                    session_id.clone()
                };
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, &id));
                ("claude", id, None, tpath)
            }
            VendorRoute::Agy {
                conversation_id,
                ls_address,
                csrf_token,
                ..
            } => {
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
                ("antigravity", conversation_id.clone(), None, tpath)
            }
            VendorRoute::Codex { thread_id, cwd } => {
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, thread_id));
                (
                    "codex",
                    thread_id.clone(),
                    cwd.as_ref().map(|p| p.display().to_string()),
                    tpath,
                )
            }
            VendorRoute::Muse {
                session_id, cwd, ..
            } => {
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, session_id));
                (
                    "muse",
                    session_id.clone(),
                    cwd.as_ref().map(|p| p.display().to_string()),
                    tpath,
                )
            }
            VendorRoute::Grok {
                session_id, cwd, ..
            } => {
                let tpath = home
                    .as_deref()
                    .and_then(|h| find_transcript_file_in_home(h, session_id));
                (
                    "grok",
                    session_id.clone(),
                    cwd.as_ref().map(|p| p.display().to_string()),
                    tpath,
                )
            }
            VendorRoute::Webhook { socket_path } => (
                "webhook",
                format!("webhook-{persona}"),
                Some(socket_path.display().to_string()),
                None,
            ),
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
            });
        }
    }

    for session in &mut out {
        if session.source == "route" {
            // `home` is for transcript lookup only. Production passes `None`
            // so probes may use `PATH`; `Some($HOME)` would skip them.
            session.quota_usage = quota_for_resolved_route(
                &session.adapter,
                session.transcript_path.as_deref(),
                home_override,
            );
            if with_model_info {
                let endpoint = agy_endpoints.get(&session.id);
                let info = session_model_info(
                    &session.adapter,
                    &session.id,
                    session.transcript_path.as_deref(),
                    home.as_deref(),
                    endpoint.and_then(|(addr, _)| addr.as_deref()),
                    endpoint.and_then(|(_, csrf)| csrf.as_deref()),
                    agy_env_fallback_allowed(home_override),
                );
                session.model = info.model;
                session.thinking_level = info.thinking_level;
                session.available_models = info.available_models;
            }
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
const MUSE_MODEL: &str = "muse-spark";

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
fn agy_env_fallback_allowed(home_override: Option<&Path>) -> bool {
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
        "muse" => SessionModelInfo {
            model: Some(MUSE_MODEL.to_string()),
            thinking_level: None,
            available_models: vec![MUSE_MODEL.to_string()],
        },
        _ => SessionModelInfo::default(),
    }
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

fn agy_rpc(port: u16, csrf: &str, method: &str, body: &str, max_bytes: usize) -> Option<String> {
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

/// `~/.claude/settings.json` `model` (normalized) and its effort: `effortLevel`, else
/// `modelSettings.<model>.effortLevel`.
fn claude_settings_model(settings: &Value) -> (Option<String>, Option<String>) {
    let raw = plain_bounded_str(settings.get("model"), MODEL_ID_MAX_CHARS);
    let model = raw.as_deref().map(normalize_claude_model);
    let effort =
        plain_bounded_str(settings.get("effortLevel"), CODEX_EFFORT_MAX_CHARS).or_else(|| {
            [raw.as_deref(), model.as_deref()]
                .into_iter()
                .flatten()
                .find_map(|key| {
                    plain_bounded_str(
                        settings.pointer(&format!("/modelSettings/{key}/effortLevel")),
                        CODEX_EFFORT_MAX_CHARS,
                    )
                })
        });
    (model, effort)
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
    let (settings_model, settings_effort) = settings_path
        .as_deref()
        .and_then(read_json_file)
        .map_or((None, None), |v| claude_settings_model(&v));
    let settings_newer = match (
        settings_path.as_deref().and_then(file_mtime),
        transcript.and_then(file_mtime),
    ) {
        (Some(settings), Some(transcript)) => settings > transcript,
        _ => false,
    };
    let (model, thinking_level) = match from_transcript {
        Some(_) if settings_newer && settings_model.is_some() => (settings_model, settings_effort),
        Some((model, effort)) => (Some(model), effort.or(settings_effort)),
        None => (settings_model, settings_effort),
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
            .map(str::to_string);
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
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    let tpath = find_transcript_file_in_home(&home, trimmed)?;
    let adapter = adapter_for_transcript(Path::new(&tpath))?;
    quota_for_resolved_route_locked(&adapter, Some(&tpath), home_override)
}

/// Quota for a route row whose transcript path is already known.
///
/// This does not search the home directory again.
pub(crate) fn quota_for_resolved_route(
    adapter: &str,
    transcript_path: Option<&str>,
    home_override: Option<&Path>,
) -> Option<SessionQuotaUsage> {
    crate::direct_quota::with_probe_lock(|| {
        quota_for_resolved_route_locked(adapter, transcript_path, home_override)
    })
}

fn quota_for_resolved_route_locked(
    adapter: &str,
    transcript_path: Option<&str>,
    home_override: Option<&Path>,
) -> Option<SessionQuotaUsage> {
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    match adapter {
        "codex" => {
            let path = transcript_path?;
            let raw = read_transcript(path).ok()?;
            codex_quota_from_rollout(&raw)
        }
        "grok" => {
            transcript_path?;
            let home = home?;
            grok_quota_from_home(&home)
        }
        "muse" => {
            let disk = transcript_path.and_then(|path| {
                let raw = read_transcript(path).ok()?;
                muse_quota_from_session(&raw)
            });
            let account = crate::direct_quota::muse_account_quota(home_override);
            crate::direct_quota::merge_account_quota(account, disk)
        }
        "claude" => {
            let disk = transcript_path.and_then(|path| {
                let raw = read_transcript(path).ok()?;
                let reason = crate::direct_quota::claude_rate_limit_reason(&raw)?;
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
                let raw = read_transcript(path).ok()?;
                let reason = crate::direct_quota::antigravity_exhausted_reason(&raw)?;
                Some(SessionQuotaUsage {
                    tier: None,
                    primary_window: None,
                    secondary_window: None,
                    exhausted_reason: Some(reason),
                })
            });
            let account = crate::direct_quota::agy_account_quota(home_override);
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

    let tpath = PathBuf::from(tpath_opt?);
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
            .unwrap_or(500_000);

        if let Some(used_tokens) = used_tokens {
            return Some(SessionContextUsage {
                used_tokens,
                max_tokens,
            });
        }
    }

    if tpath.to_string_lossy().contains(".codex")
        && let Ok(raw) = read_transcript(&tpath)
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
                .unwrap_or(500_000);
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
        && let Ok(raw) = read_transcript(&tpath)
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
        && let Ok(raw) = read_transcript(&tpath)
    {
        for line in raw.lines().rev() {
            let trimmed_line = line.trim();
            if trimmed_line.is_empty() || !trimmed_line.contains("\"usage\"") {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed_line) else {
                continue;
            };
            if val.get("isSidechain").and_then(Value::as_bool) == Some(true) {
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
        && let Ok(raw) = read_transcript(&tpath)
        && let Some(usage) = antigravity_context_from_transcript(&raw)
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
    for s in &sessions {
        if (s.id == trimmed || s.persona.as_deref() == Some(trimmed))
            && let Some(ref tpath) = s.transcript_path
            && let Ok(raw) = read_transcript(tpath)
        {
            let turns = parse_jsonl_transcript(&raw);
            if !turns.is_empty() {
                return turns;
            }
        }
    }

    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    if let Some(ref h) = home
        && let Some(tpath) = find_transcript_file_in_home(h, trimmed)
        && let Ok(raw) = read_transcript(tpath)
    {
        let turns = parse_jsonl_transcript(&raw);
        if !turns.is_empty() {
            return turns;
        }
    }

    for attempt in &snap.attempts {
        if attempt.id == trimmed || attempt.record_str == trimmed {
            return vec![TranscriptTurn {
                index: 0,
                role: "assistant".to_string(),
                content: attempt.output_excerpt.clone().unwrap_or_default(),
                thinking: None,
                tool_calls: Vec::new(),
                created_at: attempt.created_at.clone(),
            }];
        }
    }

    Vec::new()
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
        if effective_role == "user" {
            let trimmed_raw = raw_joined.trim();
            if obj.get("synthetic_reason").is_some()
                || (trimmed_raw.starts_with("<user_info>") && trimmed_raw.ends_with("</user_info>"))
                || (trimmed_raw.starts_with("<system-reminder>")
                    && trimmed_raw.ends_with("</system-reminder>"))
            {
                effective_role = "system".to_string();
            } else {
                raw_joined = strip_grok_user_query_wrapper(&raw_joined);
            }
        }
        let (mut role, content) = normalize_system_turn(&effective_role, &raw_joined);
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

fn strip_grok_user_query_wrapper(content: &str) -> String {
    let trimmed = content.trim();
    let after_notice = trimmed
        .strip_prefix("The user sent a message while you were working:\n")
        .or_else(|| trimmed.strip_prefix("The user sent a message while you were working:\r\n"))
        .unwrap_or(trimmed)
        .trim_start();
    if let Some(inner) = after_notice.strip_prefix("<user_query>") {
        let without_close = inner
            .trim_end()
            .strip_suffix("</user_query>")
            .unwrap_or(inner);
        return without_close.trim().to_string();
    }
    content.to_string()
}

/// Wrapper tags Antigravity puts around operator and notice turns.
const AGY_WRAPPER_TAGS: [&str; 3] = ["user_request", "additional_metadata", "ephemeral_message"];

/// Strip Antigravity `<USER_REQUEST>`, `<ADDITIONAL_METADATA>` and `<EPHEMERAL_MESSAGE>` wrappers
/// from a user turn. Mirrors the UI's `stripAgyUserRequestWrapper`.
///
/// Content that does not start with one of the three tags is returned unchanged, so a message
/// that merely quotes a tag stays intact. Otherwise every `<ADDITIONAL_METADATA>` block is
/// removed, every `<USER_REQUEST>` and `<EPHEMERAL_MESSAGE>` block is replaced by its trimmed
/// inner text, runs of three or more newlines become two, and the result is trimmed.
pub(crate) fn strip_agy_user_request_wrapper(content: &str) -> String {
    if !starts_with_agy_wrapper_tag(content) {
        return content.to_string();
    }
    let (without_metadata, _) =
        replace_agy_blocks(content, "additional_metadata", |_| String::new());
    let (without_request, _) = replace_agy_blocks(&without_metadata, "user_request", |inner| {
        inner.trim().to_string()
    });
    let (unwrapped, _) = replace_agy_blocks(&without_request, "ephemeral_message", |inner| {
        inner.trim().to_string()
    });
    collapse_blank_runs(&unwrapped).trim().to_string()
}

fn starts_with_agy_wrapper_tag(content: &str) -> bool {
    let lowered = content.trim_start().to_ascii_lowercase();
    AGY_WRAPPER_TAGS
        .iter()
        .any(|tag| lowered.starts_with(&format!("<{tag}>")))
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
const SYSTEM_NOTICE_OPENERS: [&str; 4] = [
    "<task-notification>",
    "<local-command-caveat>",
    "<local-command-stdout>",
    "[SYSTEM NOTIFICATION - NOT USER INPUT]",
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

fn normalize_system_turn(role: &str, content: &str) -> (String, String) {
    if role != "system" && role != "user" {
        return (role.to_string(), content.to_string());
    }
    let from_ephemeral = is_agy_ephemeral_message(content);
    let unwrapped = strip_agy_user_request_wrapper(content);
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
        assert_eq!(seen.lock().unwrap().len(), 2, "discovery reused the cache");

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
    fn antigravity_model_cache_refreshes_within_five_seconds() {
        assert_eq!(AGY_CACHE_TTL, Duration::from_secs(5));
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
        assert_eq!(muse.model.as_deref(), Some("muse-spark"));
        assert_eq!(muse.thinking_level, None);
        assert_eq!(muse.available_models, strings(&["muse-spark"]));
        let hook = session_for(&sessions, "hook");
        assert_eq!(hook.model, None);
        assert_eq!(hook.thinking_level, None);
        assert!(hook.available_models.is_empty());
        let json = serde_json::to_value(hook).unwrap();
        assert!(json.get("model").is_none());
        assert!(json.get("thinking_level").is_none());
        assert!(json.get("available_models").is_none());
        let muse_json = serde_json::to_value(muse).unwrap();
        assert_eq!(muse_json["model"], "muse-spark");
        assert_eq!(
            muse_json["available_models"],
            serde_json::json!(["muse-spark"])
        );
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
        assert!(quota_for_resolved_route("grok", None, Some(home)).is_none());

        let quota = quota_for_resolved_route("grok", Some("already-resolved"), Some(home))
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
        let codex = quota_for_resolved_route("codex", Some(rollout.to_str().unwrap()), None)
            .expect("resolved codex path");
        assert_eq!(codex.tier.as_deref(), Some("plus"));
        assert_eq!(codex.primary_window.unwrap().window_minutes, Some(300));
        assert!(quota_for_resolved_route("codex", None, Some(home)).is_none());
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
}
