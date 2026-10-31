//! Hub and worker node records plus one-time enrollment tokens in workspace runtime state.

use std::ffi::c_char;
use std::fs;
use std::path::Path;

use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StateEntry, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::StateStore;
use fl_exec::stamp;
use fl_store::RedbStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{constant_time_eq, sha256_hex, try_generate_hex_token};

const NODE_SLUG_PREFIX: &str = "hub-node-";
const ENROLL_SLUG_PREFIX: &str = "hub-node-enroll-";
const SLUG_TAIL_MAX: usize = 48;
const DEFAULT_ENROLL_TTL_SECS: u64 = 600;
const ONLINE_WINDOW_MS: u64 = 120_000;
const STALE_WINDOW_MS: u64 = 600_000;
const ACTOR: &str = "fl-hub";
/// Compared when an enroll slug is absent so the token check still runs.
const DUMMY_CODE_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRecord {
    pub node_id: String,
    pub display_name: String,
    pub role: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub listen_addr: Option<String>,
    pub capabilities: Vec<String>,
    pub labels: Vec<String>,
    pub token_hash: Option<String>,
    pub enrolled_at_ms: u64,
    pub last_seen_at_ms: u64,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeView {
    pub node_id: String,
    pub display_name: String,
    pub role: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub listen_addr: Option<String>,
    pub capabilities: Vec<String>,
    pub labels: Vec<String>,
    pub status: String,
    pub enrolled_at_ms: u64,
    pub last_seen_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureHubArgs {
    pub node_id: String,
    pub display_name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub listen_addr: Option<String>,
    pub capabilities: Vec<String>,
    pub now_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollTokenInfo {
    pub code: String,
    pub expires_at_ms: u64,
    pub ttl_secs: u64,
    pub join_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollFail {
    Invalid,
    Expired,
    Used,
    Rejected(String),
    Store(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollWorker {
    pub code: String,
    pub node_id: String,
    pub display_name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub listen_addr: Option<String>,
    pub capabilities: Vec<String>,
    pub labels: Vec<String>,
    pub now_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolledWorker {
    pub node_id: String,
    pub node_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatUpdate {
    pub node_id: String,
    pub capabilities: Option<Vec<String>>,
    pub labels: Option<Vec<String>>,
    pub now_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatFail {
    NotFound,
    Revoked,
    Store(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeFail {
    NotFound,
    IsHub,
    Store(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollRecord {
    code_hash: String,
    label: Option<String>,
    expires_at_ms: u64,
    used: bool,
    used_by_node_id: Option<String>,
}

pub fn node_status(role: &str, revoked: bool, last_seen_at_ms: u64, now_ms: u64) -> &'static str {
    if revoked {
        return "revoked";
    }
    let age = now_ms.saturating_sub(last_seen_at_ms);
    if role == "hub" || age <= ONLINE_WINDOW_MS {
        "online"
    } else if age <= STALE_WINDOW_MS {
        "stale"
    } else {
        "offline"
    }
}

pub fn ensure_hub_node(store: &RedbStore, args: EnsureHubArgs) -> Result<NodeRecord, String> {
    let node_id = canonical_node_id(&args.node_id)?;
    let slug = node_state_slug(&node_id)?;
    let existing = load_node(store, &slug)?;
    if let Some(rec) = &existing {
        if rec.role == "worker" {
            return Err(format!("node `{node_id}` is already enrolled as a worker"));
        }
        let role = &rec.role;
        if rec.role != "hub" {
            return Err(format!(
                "node `{node_id}` has role `{role}` and cannot become a hub"
            ));
        }
    }
    let (labels, token_hash, enrolled_at_ms) = match existing {
        Some(rec) => (rec.labels, rec.token_hash, rec.enrolled_at_ms),
        None => (Vec::new(), None, args.now_ms),
    };
    let record = NodeRecord {
        node_id,
        display_name: args.display_name,
        role: "hub".to_string(),
        hostname: args.hostname,
        os: args.os,
        arch: args.arch,
        listen_addr: args.listen_addr,
        capabilities: args.capabilities,
        labels,
        token_hash,
        enrolled_at_ms,
        last_seen_at_ms: args.now_ms,
        revoked: false,
    };
    persist_node(store, &slug, &record)?;
    Ok(record)
}

pub fn list_visible_nodes(store: &RedbStore, now_ms: u64) -> Result<Vec<NodeView>, String> {
    let mut views: Vec<NodeView> = iter_node_records(store)?
        .into_iter()
        .filter(|rec| !rec.revoked)
        .map(|rec| to_view(&rec, now_ms))
        .collect();
    views.sort_by(|a, b| {
        u8::from(a.role != "hub")
            .cmp(&u8::from(b.role != "hub"))
            .then(a.node_id.cmp(&b.node_id))
    });
    Ok(views)
}

pub fn mint_enroll_token(
    store: &RedbStore,
    label: Option<String>,
    ttl_secs: u64,
    hub_url: &str,
    now_ms: u64,
) -> Result<EnrollTokenInfo, String> {
    let ttl = if ttl_secs == 0 {
        DEFAULT_ENROLL_TTL_SECS
    } else {
        ttl_secs
    };
    let code = try_generate_hex_token(32)?;
    let code_hash = sha256_hex(&code);
    let expires_at_ms = now_ms.saturating_add(ttl.saturating_mul(1000));
    let label = normalize_label(label);
    let record = EnrollRecord {
        code_hash: code_hash.clone(),
        label: label.clone(),
        expires_at_ms,
        used: false,
        used_by_node_id: None,
    };
    persist_enroll(store, &enroll_state_slug(&code_hash), &record)?;
    let join_command = join_command(hub_url, &code, label.as_deref());
    Ok(EnrollTokenInfo {
        code,
        expires_at_ms,
        ttl_secs: ttl,
        join_command,
    })
}

pub fn enroll_worker(store: &RedbStore, req: EnrollWorker) -> Result<EnrolledWorker, EnrollFail> {
    if req.code.is_empty() || req.node_id.is_empty() || req.display_name.is_empty() {
        return Err(EnrollFail::Invalid);
    }
    let slug = match node_state_slug(&req.node_id) {
        Ok(slug) => slug,
        Err(_) => return Err(EnrollFail::Invalid),
    };
    let node_id = match canonical_node_id(&req.node_id) {
        Ok(node_id) => node_id,
        Err(_) => return Err(EnrollFail::Invalid),
    };
    let code_hash = sha256_hex(&req.code);
    let loaded = match load_enroll(store, &enroll_state_slug(&code_hash)) {
        Ok(loaded) => loaded,
        Err(err) => return Err(EnrollFail::Store(err)),
    };
    let stored_hash = loaded
        .as_ref()
        .map(|entry| entry.code_hash.as_str())
        .unwrap_or(DUMMY_CODE_HASH);
    if !constant_time_eq(&code_hash, stored_hash) {
        return Err(EnrollFail::Invalid);
    }
    let Some(mut entry) = loaded else {
        return Err(EnrollFail::Invalid);
    };
    if entry.used {
        return Err(EnrollFail::Used);
    }
    if req.now_ms >= entry.expires_at_ms {
        return Err(EnrollFail::Expired);
    }
    let existing = load_node(store, &slug).map_err(EnrollFail::Store)?;
    if existing.as_ref().is_some_and(|rec| rec.role == "hub") {
        return Err(EnrollFail::Rejected(format!(
            "refusing to enroll over hub node `{node_id}`"
        )));
    }
    let enrolled_at_ms = existing
        .as_ref()
        .filter(|rec| rec.role == "worker")
        .map(|rec| rec.enrolled_at_ms)
        .unwrap_or(req.now_ms);
    let node_token = try_generate_hex_token(32).map_err(EnrollFail::Store)?;
    let record = NodeRecord {
        node_id: node_id.clone(),
        display_name: req.display_name,
        role: "worker".to_string(),
        hostname: req.hostname,
        os: req.os,
        arch: req.arch,
        listen_addr: req.listen_addr,
        capabilities: req.capabilities,
        labels: req.labels,
        token_hash: Some(sha256_hex(&node_token)),
        enrolled_at_ms,
        last_seen_at_ms: req.now_ms,
        revoked: false,
    };
    persist_node(store, &slug, &record).map_err(EnrollFail::Store)?;
    entry.used = true;
    entry.used_by_node_id = Some(node_id.clone());
    persist_enroll(store, &enroll_state_slug(&code_hash), &entry).map_err(EnrollFail::Store)?;
    Ok(EnrolledWorker {
        node_id,
        node_token,
    })
}

pub fn node_token_matches(store: &RedbStore, node_id: &str, bearer: &str) -> Result<bool, String> {
    let Ok(slug) = node_state_slug(node_id) else {
        return Ok(false);
    };
    let Some(rec) = load_node(store, &slug)? else {
        return Ok(false);
    };
    if rec.revoked {
        return Ok(false);
    }
    let Some(token_hash) = rec.token_hash else {
        return Ok(false);
    };
    Ok(constant_time_eq(&sha256_hex(bearer), &token_hash))
}

pub fn touch_node(store: &RedbStore, update: HeartbeatUpdate) -> Result<NodeView, HeartbeatFail> {
    let slug = match node_state_slug(&update.node_id) {
        Ok(slug) => slug,
        Err(_) => return Err(HeartbeatFail::NotFound),
    };
    let Some(mut rec) = load_node(store, &slug).map_err(HeartbeatFail::Store)? else {
        return Err(HeartbeatFail::NotFound);
    };
    if rec.revoked {
        return Err(HeartbeatFail::Revoked);
    }
    rec.last_seen_at_ms = update.now_ms;
    if let Some(capabilities) = update.capabilities {
        rec.capabilities = capabilities;
    }
    if let Some(labels) = update.labels {
        rec.labels = labels;
    }
    persist_node(store, &slug, &rec).map_err(HeartbeatFail::Store)?;
    Ok(to_view(&rec, update.now_ms))
}

pub fn revoke_node(store: &RedbStore, node_id: &str) -> Result<NodeRecord, RevokeFail> {
    let slug = match node_state_slug(node_id) {
        Ok(slug) => slug,
        Err(_) => return Err(RevokeFail::NotFound),
    };
    let Some(mut rec) = load_node(store, &slug).map_err(RevokeFail::Store)? else {
        return Err(RevokeFail::NotFound);
    };
    if rec.role == "hub" {
        return Err(RevokeFail::IsHub);
    }
    rec.revoked = true;
    persist_node(store, &slug, &rec).map_err(RevokeFail::Store)?;
    Ok(rec)
}

pub fn detect_capabilities(path_env: &str) -> Vec<String> {
    let mut found = Vec::new();
    for name in ["claude", "agy", "codex", "muse", "grok", "tmux", "git"] {
        if path_has_file(path_env, name) {
            found.push(name.to_string());
        }
    }
    if path_has_file(path_env, "xcode") || path_has_file(path_env, "xcodebuild") {
        found.push("xcode".to_string());
    }
    found
}

// libc `gethostname`. No extra crate: the process already links libc.
unsafe extern "C" {
    fn gethostname(buf: *mut c_char, len: usize) -> i32;
}

fn hostname_from_buffer(buf: &[u8]) -> Option<String> {
    let end = buf.iter().position(|byte| *byte == 0).unwrap_or(buf.len());
    let trimmed = std::str::from_utf8(&buf[..end]).ok()?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn hostname_from_libc() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a 256-byte writable array that outlives the call.
    // `gethostname` writes at most `buf.len()` bytes and returns 0 or -1.
    let rc = unsafe { gethostname(buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
    if rc != 0 {
        return None;
    }
    hostname_from_buffer(&buf)
}

fn read_hostname_file(path: &str) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn pick_hostname(candidates: impl IntoIterator<Item = Option<String>>) -> String {
    candidates
        .into_iter()
        .flatten()
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

pub fn local_hostname() -> String {
    // `gethostname` first: macOS has neither procfs nor `/etc/hostname`.
    pick_hostname([
        hostname_from_libc(),
        read_hostname_file("/proc/sys/kernel/hostname"),
        read_hostname_file("/etc/hostname"),
    ])
}

pub fn find_hub_node(store: &RedbStore) -> Result<Option<NodeRecord>, String> {
    let mut hubs: Vec<NodeRecord> = iter_node_records(store)?
        .into_iter()
        .filter(|rec| rec.role == "hub" && !rec.revoked)
        .collect();
    hubs.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    Ok(hubs.into_iter().next())
}

fn to_view(rec: &NodeRecord, now_ms: u64) -> NodeView {
    NodeView {
        node_id: rec.node_id.clone(),
        display_name: rec.display_name.clone(),
        role: rec.role.clone(),
        hostname: rec.hostname.clone(),
        os: rec.os.clone(),
        arch: rec.arch.clone(),
        listen_addr: rec.listen_addr.clone(),
        capabilities: rec.capabilities.clone(),
        labels: rec.labels.clone(),
        status: node_status(&rec.role, rec.revoked, rec.last_seen_at_ms, now_ms).to_string(),
        enrolled_at_ms: rec.enrolled_at_ms,
        last_seen_at_ms: rec.last_seen_at_ms,
    }
}

/// Hostname shaped into an id that `canonical_node_id` and `node_state_slug` accept.
///
/// Trim, ASCII-lowercase, map `.` to `-`, drop every other character outside
/// `[a-z0-9_-]`, then truncate to 48. An empty result or an enroll-slug collision
/// is prefixed with `n-` and truncated again. A bare `n-`, or a result the slug
/// check still rejects, becomes `localhost`.
pub fn node_id_from_hostname(raw: &str) -> String {
    let mut id = String::new();
    for ch in raw.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        let mapped = if lower == '.' { '-' } else { lower };
        if mapped.is_ascii_lowercase() || mapped.is_ascii_digit() || mapped == '-' || mapped == '_'
        {
            id.push(mapped);
        }
        if id.len() >= SLUG_TAIL_MAX {
            break;
        }
    }
    if id.is_empty() || node_state_slug(&id).is_err() {
        id.insert_str(0, "n-");
        id.truncate(SLUG_TAIL_MAX);
    }
    // `n-` alone means the hostname contributed no characters (`""`, `@@@`).
    if id == "n-" || node_state_slug(&id).is_err() {
        "localhost".to_string()
    } else {
        id
    }
}

/// Trimmed, ASCII-lowercased node id. The whole result must match `[a-z0-9_-]{1,48}`.
pub fn canonical_node_id(raw: &str) -> Result<String, String> {
    let id = raw.trim().to_ascii_lowercase();
    let chars_ok = id.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
    });
    if id.is_empty() || id.len() > SLUG_TAIL_MAX || !chars_ok {
        return Err(
            "node_id must be 1-48 characters of [a-z0-9_-] after trim and lowercase".to_string(),
        );
    }
    Ok(id)
}

pub(crate) fn node_state_slug(node_id: &str) -> Result<String, String> {
    let tail = canonical_node_id(node_id)?;
    let slug = format!("{NODE_SLUG_PREFIX}{tail}");
    if slug.starts_with(ENROLL_SLUG_PREFIX) {
        return Err("node_id collides with the enroll-record slug prefix".to_string());
    }
    Ok(slug)
}

/// State slugs are at most 64 bytes. `hub-node-enroll-` is 16, so the slug keeps
/// the first 48 hex chars of the digest. `code_hash` stores the full SHA-256.
fn enroll_state_slug(code_hash: &str) -> String {
    let tail: String = code_hash.chars().take(SLUG_TAIL_MAX).collect();
    format!("{ENROLL_SLUG_PREFIX}{tail}")
}

fn is_node_slug(slug: &str) -> bool {
    slug.starts_with(NODE_SLUG_PREFIX) && !slug.starts_with(ENROLL_SLUG_PREFIX)
}

fn normalize_label(label: Option<String>) -> Option<String> {
    label.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn quote_shell_token(value: &str) -> String {
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn join_command(hub_url: &str, code: &str, label: Option<&str>) -> String {
    let url = hub_url.trim_end_matches('/');
    let mut command = format!("fl hub join {url} --token {code}");
    if let Some(label) = label {
        command.push_str(" --label ");
        command.push_str(&quote_shell_token(label));
    }
    command
}

fn path_has_file(path_env: &str, name: &str) -> bool {
    path_env
        .split(':')
        .filter(|dir| !dir.is_empty())
        .any(|dir| {
            let candidate = Path::new(dir).join(name);
            match fs::metadata(&candidate) {
                Ok(meta) => meta.is_file(),
                Err(_) => false,
            }
        })
}

fn persist_runtime(store: &RedbStore, slug: &str, value: Value) -> Result<(), String> {
    store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: slug.to_string(),
            sensitivity: SensitivityPatch::SetSensitive,
            payload: StatePayload::Runtime(RuntimeEntry {
                value,
                merge_mode: MergeMode::LastWriter,
            }),
            at: stamp::now(),
            by: ACTOR.to_string(),
        })
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn persist_node(store: &RedbStore, slug: &str, record: &NodeRecord) -> Result<(), String> {
    let value = serde_json::to_value(record).map_err(|err| err.to_string())?;
    persist_runtime(store, slug, value)
}

fn persist_enroll(store: &RedbStore, slug: &str, record: &EnrollRecord) -> Result<(), String> {
    let value = serde_json::to_value(record).map_err(|err| err.to_string())?;
    persist_runtime(store, slug, value)
}

fn load_runtime(store: &RedbStore, slug: &str) -> Result<Option<StateEntry>, String> {
    store
        .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, slug)
        .map_err(|err| err.to_string())
}

fn load_node(store: &RedbStore, slug: &str) -> Result<Option<NodeRecord>, String> {
    let Some(entry) = load_runtime(store, slug)? else {
        return Ok(None);
    };
    if !entry.sensitive {
        return Ok(None);
    }
    let StatePayload::Runtime(rt) = entry.payload else {
        return Err(format!("state `{slug}` is not a runtime node record"));
    };
    serde_json::from_value(rt.value)
        .map(Some)
        .map_err(|err| err.to_string())
}

fn load_enroll(store: &RedbStore, slug: &str) -> Result<Option<EnrollRecord>, String> {
    let Some(entry) = load_runtime(store, slug)? else {
        return Ok(None);
    };
    if !entry.sensitive {
        return Ok(None);
    }
    let StatePayload::Runtime(rt) = entry.payload else {
        return Err(format!("state `{slug}` is not a runtime enroll record"));
    };
    serde_json::from_value(rt.value)
        .map(Some)
        .map_err(|err| err.to_string())
}

fn node_from_entry(entry: &StateEntry) -> Option<NodeRecord> {
    if !entry.sensitive {
        return None;
    }
    let StatePayload::Runtime(rt) = &entry.payload else {
        return None;
    };
    serde_json::from_value(rt.value.clone()).ok()
}

fn iter_node_records(store: &RedbStore) -> Result<Vec<NodeRecord>, String> {
    let entries = store
        .list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))
        .map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    for entry in entries {
        if !is_node_slug(&entry.slug) || !entry.sensitive {
            continue;
        }
        if let Some(rec) = node_from_entry(&entry) {
            out.push(rec);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::state::{StateConcern, StatePayload, StateScope};
    use fl_core::store::StateStore;
    use tempfile::TempDir;

    fn open_store() -> (TempDir, RedbStore) {
        let tmp = TempDir::new().unwrap();
        let store = RedbStore::open(&tmp.path().join("store.redb")).unwrap();
        (tmp, store)
    }

    fn hub_args(node_id: &str, now_ms: u64, listen_addr: Option<&str>) -> EnsureHubArgs {
        EnsureHubArgs {
            node_id: node_id.to_string(),
            display_name: "Primary Hub".to_string(),
            hostname: "hubhost".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            listen_addr: listen_addr.map(str::to_string),
            capabilities: vec!["git".to_string()],
            now_ms,
        }
    }

    fn enroll_req(code: &str, node_id: &str, display_name: &str, now_ms: u64) -> EnrollWorker {
        EnrollWorker {
            code: code.to_string(),
            node_id: node_id.to_string(),
            display_name: display_name.to_string(),
            hostname: "workerhost".to_string(),
            os: "linux".to_string(),
            arch: "aarch64".to_string(),
            listen_addr: Some("127.0.0.1:9".to_string()),
            capabilities: vec!["git".to_string()],
            labels: vec!["edge".to_string()],
            now_ms,
        }
    }

    fn stored_enroll(store: &RedbStore, code: &str) -> (String, Value) {
        let code_hash = sha256_hex(code);
        let slug = enroll_state_slug(&code_hash);
        let entry = store
            .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)
            .unwrap()
            .unwrap();
        let StatePayload::Runtime(rt) = entry.payload else {
            panic!("enroll payload is not runtime");
        };
        (entry.slug, rt.value)
    }

    #[test]
    fn status_windows_and_revoked_hub() {
        assert_eq!(node_status("hub", false, 0, 1_000_000), "online");
        assert_eq!(node_status("worker", false, 0, 120_000), "online");
        assert_eq!(node_status("worker", false, 0, 120_001), "stale");
        assert_eq!(node_status("worker", false, 0, 600_000), "stale");
        assert_eq!(node_status("worker", false, 0, 600_001), "offline");
        assert_eq!(node_status("hub", true, 0, 0), "revoked");
    }

    #[test]
    fn detect_capabilities_finds_files_not_directories() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("git"), []).unwrap();
        fs::write(dir.path().join("xcodebuild"), []).unwrap();
        fs::create_dir(dir.path().join("claude")).unwrap();
        let caps = detect_capabilities(dir.path().to_str().unwrap());
        assert_eq!(caps, vec!["git".to_string(), "xcode".to_string()]);
    }

    #[test]
    fn local_hostname_is_non_empty() {
        assert!(!local_hostname().is_empty());
    }

    #[test]
    fn mint_enroll_list_and_token_match() {
        let (_tmp, store) = open_store();
        assert!(find_hub_node(&store).unwrap().is_none());
        let hub = ensure_hub_node(&store, hub_args("zzz-hub", 1_000, None)).unwrap();
        let info = mint_enroll_token(
            &store,
            Some("O'Brien".to_string()),
            0,
            "http://127.0.0.1:8420/",
            5_000,
        )
        .unwrap();
        assert_eq!(info.ttl_secs, 600);
        assert_eq!(info.expires_at_ms, 5_000 + 600_000);
        assert_eq!(info.code.len(), 64);
        assert!(info.code.chars().all(|ch| ch.is_ascii_hexdigit()));
        let code = &info.code;
        assert_eq!(
            info.join_command,
            format!("fl hub join http://127.0.0.1:8420 --token {code} --label 'O'\\''Brien'")
        );
        let (slug, stored) = stored_enroll(&store, &info.code);
        assert_eq!(slug.len(), 64);
        assert!(slug.starts_with(ENROLL_SLUG_PREFIX));
        assert_eq!(
            &slug[ENROLL_SLUG_PREFIX.len()..],
            &sha256_hex(&info.code)[..48]
        );
        assert_eq!(stored["code_hash"], sha256_hex(&info.code));
        assert_eq!(stored["label"], "O'Brien");
        assert!(stored.get("code").is_none());
        assert!(!stored.to_string().contains(&info.code));
        assert!(entry_sensitive(&store, &slug));

        let blank =
            mint_enroll_token(&store, Some("  \n".to_string()), 10, "http://h/", 100).unwrap();
        assert_eq!(blank.ttl_secs, 10);
        let blank_code = &blank.code;
        assert_eq!(
            blank.join_command,
            format!("fl hub join http://h --token {blank_code}")
        );
        assert!(stored_enroll(&store, &blank.code).1["label"].is_null());

        let enrolled = enroll_worker(
            &store,
            enroll_req(&info.code, "aaa-worker", "Worker A", 6_000),
        )
        .unwrap();
        assert_eq!(enrolled.node_id, "aaa-worker");
        assert_eq!(enrolled.node_token.len(), 64);
        assert!(node_token_matches(&store, "aaa-worker", &enrolled.node_token).unwrap());
        assert!(!node_token_matches(&store, "aaa-worker", "other-token").unwrap());
        assert!(!node_token_matches(&store, "missing", &enrolled.node_token).unwrap());
        assert!(!node_token_matches(&store, &hub.node_id, "x").unwrap());

        let listed = list_visible_nodes(&store, 1_000_000).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|view| view.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["zzz-hub", "aaa-worker"]
        );
        assert_eq!(listed[0].status, "online");
        assert_eq!(listed[1].status, "offline");
        assert_eq!(listed[1].labels, vec!["edge".to_string()]);
        assert_eq!(listed[1].listen_addr.as_deref(), Some("127.0.0.1:9"));
        let node_slug = node_state_slug("aaa-worker").unwrap();
        assert!(entry_sensitive(&store, &node_slug));
    }

    #[test]
    fn second_enroll_is_used_and_keeps_token_hash() {
        let (_tmp, store) = open_store();
        let info = mint_enroll_token(&store, None, 60, "http://hub", 1_000).unwrap();
        let enrolled = enroll_worker(&store, enroll_req(&info.code, "w1", "W", 1_100)).unwrap();
        let before = load_node(&store, &node_state_slug("w1").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            before.token_hash.as_deref(),
            Some(sha256_hex(&enrolled.node_token).as_str())
        );
        let err = enroll_worker(&store, enroll_req(&info.code, "w1", "W", 1_200)).unwrap_err();
        assert_eq!(err, EnrollFail::Used);
        let after = load_node(&store, &node_state_slug("w1").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(before, after);
        assert!(node_token_matches(&store, "w1", &enrolled.node_token).unwrap());

        let err = enroll_worker(
            &store,
            enroll_req(&info.code, "w1", "W", info.expires_at_ms + 1),
        )
        .unwrap_err();
        assert_eq!(err, EnrollFail::Used);
        let after_expiry = load_node(&store, &node_state_slug("w1").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(after_expiry.token_hash, before.token_hash);
    }

    #[test]
    fn expired_code_stays_unused() {
        let (_tmp, store) = open_store();
        let info = mint_enroll_token(&store, None, 5, "http://hub.example/", 1_000).unwrap();
        assert_eq!(info.expires_at_ms, 6_000);
        let err = enroll_worker(&store, enroll_req(&info.code, "w-exp", "W", 6_000)).unwrap_err();
        assert_eq!(err, EnrollFail::Expired);
        let (_slug, stored) = stored_enroll(&store, &info.code);
        assert_eq!(stored["used"], false);
        assert!(stored["used_by_node_id"].is_null());
        assert!(
            load_node(&store, &node_state_slug("w-exp").unwrap())
                .unwrap()
                .is_none()
        );
        let enrolled = enroll_worker(&store, enroll_req(&info.code, "w-exp", "W", 5_999)).unwrap();
        assert_eq!(enrolled.node_token.len(), 64);
        let (_slug, stored) = stored_enroll(&store, &info.code);
        assert_eq!(stored["used"], true);
        assert_eq!(stored["used_by_node_id"], "w-exp");
    }

    #[test]
    fn wrong_or_empty_code_is_invalid() {
        let (_tmp, store) = open_store();
        let info = mint_enroll_token(&store, None, 60, "http://hub", 1_000).unwrap();
        let wrong = "ab".repeat(32);
        let err = enroll_worker(&store, enroll_req(&wrong, "w-bad", "W", 1_100)).unwrap_err();
        assert_eq!(err, EnrollFail::Invalid);
        assert_eq!(
            enroll_worker(&store, enroll_req("", "w-bad", "W", 1_100)).unwrap_err(),
            EnrollFail::Invalid
        );
        assert_eq!(
            enroll_worker(&store, enroll_req(&info.code, "", "W", 1_100)).unwrap_err(),
            EnrollFail::Invalid
        );
        assert_eq!(
            enroll_worker(&store, enroll_req(&info.code, "w-bad", "", 1_100)).unwrap_err(),
            EnrollFail::Invalid
        );
        assert_eq!(
            enroll_worker(&store, enroll_req(&info.code, "!!!", "W", 1_100)).unwrap_err(),
            EnrollFail::Invalid
        );
        let (_slug, stored) = stored_enroll(&store, &info.code);
        assert_eq!(stored["used"], false);
        let enrolled = enroll_worker(&store, enroll_req(&info.code, "w-bad", "W", 1_100)).unwrap();
        assert!(!enrolled.node_token.is_empty());
    }

    #[test]
    fn heartbeat_updates_only_provided_fields_and_revoked_is_unchanged() {
        let (_tmp, store) = open_store();
        let info = mint_enroll_token(&store, None, 60, "http://hub", 1_000).unwrap();
        enroll_worker(&store, enroll_req(&info.code, "w-hb", "W", 10_000)).unwrap();
        let view = touch_node(
            &store,
            HeartbeatUpdate {
                node_id: "w-hb".to_string(),
                capabilities: Some(vec!["tmux".to_string()]),
                labels: None,
                now_ms: 20_000,
            },
        )
        .unwrap();
        assert_eq!(view.last_seen_at_ms, 20_000);
        assert_eq!(view.capabilities, vec!["tmux".to_string()]);
        assert_eq!(view.labels, vec!["edge".to_string()]);
        assert_eq!(view.status, "online");

        let missing = touch_node(
            &store,
            HeartbeatUpdate {
                node_id: "nope".to_string(),
                capabilities: None,
                labels: None,
                now_ms: 20_000,
            },
        )
        .unwrap_err();
        assert_eq!(missing, HeartbeatFail::NotFound);

        let revoked = revoke_node(&store, "w-hb").unwrap();
        assert!(revoked.revoked);
        assert_eq!(revoked.last_seen_at_ms, 20_000);
        let err = touch_node(
            &store,
            HeartbeatUpdate {
                node_id: "w-hb".to_string(),
                capabilities: Some(vec!["git".to_string()]),
                labels: Some(vec!["changed".to_string()]),
                now_ms: 90_000,
            },
        )
        .unwrap_err();
        assert_eq!(err, HeartbeatFail::Revoked);
        let after = load_node(&store, &node_state_slug("w-hb").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(after.last_seen_at_ms, 20_000);
        assert_eq!(after.capabilities, vec!["tmux".to_string()]);
        assert_eq!(after.labels, vec!["edge".to_string()]);
        assert!(list_visible_nodes(&store, 20_000).unwrap().is_empty());
    }

    #[test]
    fn revoke_worker_hides_it_and_hub_cannot_be_revoked() {
        let (_tmp, store) = open_store();
        let hub = ensure_hub_node(&store, hub_args("primary", 1_000, None)).unwrap();
        let info = mint_enroll_token(&store, None, 60, "http://hub", 1_000).unwrap();
        enroll_worker(&store, enroll_req(&info.code, "w-rev", "W", 2_000)).unwrap();
        let before_hub = load_node(&store, &node_state_slug("primary").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            revoke_node(&store, "primary").unwrap_err(),
            RevokeFail::IsHub
        );
        assert_eq!(
            revoke_node(&store, "missing").unwrap_err(),
            RevokeFail::NotFound
        );
        let after_hub = load_node(&store, &node_state_slug("primary").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(before_hub, after_hub);
        assert!(!after_hub.revoked);
        assert_eq!(find_hub_node(&store).unwrap().unwrap().node_id, hub.node_id);

        let revoked = revoke_node(&store, "w-rev").unwrap();
        assert!(revoked.revoked);
        assert_eq!(revoked.role, "worker");
        let listed = list_visible_nodes(&store, 2_000).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].node_id, "primary");
        assert_eq!(listed[0].status, "online");
    }

    #[test]
    fn revoked_worker_token_does_not_match() {
        let (_tmp, store) = open_store();
        let info = mint_enroll_token(&store, None, 60, "http://hub", 1_000).unwrap();
        let enrolled = enroll_worker(&store, enroll_req(&info.code, "w-tok", "W", 2_000)).unwrap();
        assert!(node_token_matches(&store, "w-tok", &enrolled.node_token).unwrap());
        revoke_node(&store, "w-tok").unwrap();
        assert!(!node_token_matches(&store, "w-tok", &enrolled.node_token).unwrap());
    }

    #[test]
    fn enroll_mac_node_id_is_rejected_and_writes_no_row() {
        let (_tmp, store) = open_store();
        let info = mint_enroll_token(&store, None, 60, "http://hub", 1_000).unwrap();
        let err =
            enroll_worker(&store, enroll_req(&info.code, "enroll-mac", "W", 1_100)).unwrap_err();
        assert_eq!(err, EnrollFail::Invalid);
        assert!(ensure_hub_node(&store, hub_args("enroll-mac", 1_200, None)).is_err());
        let slug = format!("{NODE_SLUG_PREFIX}enroll-mac");
        assert!(slug.starts_with(ENROLL_SLUG_PREFIX));
        assert!(
            store
                .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)
                .unwrap()
                .is_none()
        );
        let (_enroll_slug, stored) = stored_enroll(&store, &info.code);
        assert_eq!(stored["used"], false);
        assert!(list_visible_nodes(&store, 1_200).unwrap().is_empty());
        assert!(find_hub_node(&store).unwrap().is_none());
    }

    #[test]
    fn ensure_hub_refresh_keeps_enrolled_at_and_token() {
        let (_tmp, store) = open_store();
        assert!(ensure_hub_node(&store, hub_args("", 1, None)).is_err());
        assert!(ensure_hub_node(&store, hub_args("@@@", 1, None)).is_err());
        let created = ensure_hub_node(&store, hub_args("primary", 1_000, None)).unwrap();
        assert_eq!(created.enrolled_at_ms, 1_000);
        assert_eq!(created.last_seen_at_ms, 1_000);
        assert!(created.token_hash.is_none());
        assert!(!created.revoked);

        touch_node(
            &store,
            HeartbeatUpdate {
                node_id: "primary".to_string(),
                capabilities: None,
                labels: Some(vec!["keep".to_string()]),
                now_ms: 1_500,
            },
        )
        .unwrap();
        let slug = node_state_slug("primary").unwrap();
        let mut seeded = load_node(&store, &slug).unwrap().unwrap();
        seeded.token_hash = Some("abc123".to_string());
        persist_node(&store, &slug, &seeded).unwrap();

        let mut args = hub_args("primary", 5_000, Some("127.0.0.1:8420"));
        args.display_name = "Hub Two".to_string();
        args.hostname = "other".to_string();
        args.os = "macos".to_string();
        args.arch = "aarch64".to_string();
        args.capabilities = vec!["git".to_string(), "tmux".to_string()];
        let refreshed = ensure_hub_node(&store, args).unwrap();
        assert_eq!(refreshed.enrolled_at_ms, 1_000);
        assert_eq!(refreshed.last_seen_at_ms, 5_000);
        assert_eq!(refreshed.listen_addr.as_deref(), Some("127.0.0.1:8420"));
        assert_eq!(refreshed.display_name, "Hub Two");
        assert_eq!(refreshed.hostname, "other");
        assert_eq!(refreshed.os, "macos");
        assert_eq!(refreshed.arch, "aarch64");
        assert_eq!(
            refreshed.capabilities,
            vec!["git".to_string(), "tmux".to_string()]
        );
        assert_eq!(refreshed.labels, vec!["keep".to_string()]);
        assert_eq!(refreshed.token_hash.as_deref(), Some("abc123"));
        assert_eq!(refreshed.role, "hub");
        assert!(!refreshed.revoked);
        assert_eq!(to_view(&refreshed, 5_000 + 600_001).status, "online");

        let mixed = ensure_hub_node(&store, hub_args("Primary", 6_000, None)).unwrap();
        assert_eq!(mixed.node_id, "primary");
        assert_eq!(mixed.enrolled_at_ms, 1_000);
        assert_eq!(mixed.token_hash.as_deref(), Some("abc123"));

        ensure_hub_node(&store, hub_args("hub-b", 2_000, None)).unwrap();
        assert_eq!(find_hub_node(&store).unwrap().unwrap().node_id, "hub-b");
    }

    #[test]
    fn enroll_refuses_to_overwrite_hub_and_ensure_refuses_worker() {
        let (_tmp, store) = open_store();
        ensure_hub_node(&store, hub_args("primary", 1_000, None)).unwrap();
        let info = mint_enroll_token(&store, None, 60, "http://hub", 1_000).unwrap();
        let err =
            enroll_worker(&store, enroll_req(&info.code, "primary", "Hub", 1_500)).unwrap_err();
        match err {
            EnrollFail::Rejected(msg) => assert!(msg.contains("hub"), "{msg}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
        let hub = load_node(&store, &node_state_slug("primary").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(hub.role, "hub");
        assert!(hub.token_hash.is_none());
        assert!(!hub.revoked);
        let (_slug, stored) = stored_enroll(&store, &info.code);
        assert_eq!(stored["used"], false);

        let enrolled = enroll_worker(&store, enroll_req(&info.code, "edge-1", "W", 1_600)).unwrap();
        assert!(node_token_matches(&store, "edge-1", &enrolled.node_token).unwrap());
        let err = ensure_hub_node(&store, hub_args("edge-1", 1_700, None)).unwrap_err();
        assert!(err.contains("worker"), "{err}");
        let worker = load_node(&store, &node_state_slug("edge-1").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(worker.role, "worker");
        assert_eq!(worker.enrolled_at_ms, 1_600);

        let again = mint_enroll_token(&store, None, 60, "http://hub", 2_000).unwrap();
        let replaced =
            enroll_worker(&store, enroll_req(&again.code, "edge-1", "Renamed", 3_000)).unwrap();
        let worker = load_node(&store, &node_state_slug("edge-1").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(worker.enrolled_at_ms, 1_600);
        assert_eq!(worker.display_name, "Renamed");
        assert_eq!(worker.last_seen_at_ms, 3_000);
        assert!(!worker.revoked);
        assert_eq!(
            worker.token_hash.as_deref(),
            Some(sha256_hex(&replaced.node_token).as_str())
        );
        assert!(!node_token_matches(&store, "edge-1", &enrolled.node_token).unwrap());
        assert!(node_token_matches(&store, "edge-1", &replaced.node_token).unwrap());
    }

    #[test]
    fn enroll_rejects_slash_without_consuming_code_or_writing_stripped_row() {
        let (_tmp, store) = open_store();
        let info = mint_enroll_token(&store, None, 60, "http://hub", 1_000).unwrap();
        let err = enroll_worker(&store, enroll_req(&info.code, "a/b", "W", 1_100)).unwrap_err();
        assert_eq!(err, EnrollFail::Invalid);
        let (_slug, stored) = stored_enroll(&store, &info.code);
        assert_eq!(stored["used"], false);
        assert!(stored["used_by_node_id"].is_null());
        let stripped = format!("{NODE_SLUG_PREFIX}ab");
        assert!(
            store
                .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &stripped)
                .unwrap()
                .is_none()
        );
        assert!(list_visible_nodes(&store, 1_100).unwrap().is_empty());
    }

    #[test]
    fn enroll_stores_canonical_node_id_and_matches_either_case() {
        let (_tmp, store) = open_store();
        let info = mint_enroll_token(&store, None, 60, "http://hub", 1_000).unwrap();
        let enrolled =
            enroll_worker(&store, enroll_req(&info.code, "Worker-A", "W", 1_100)).unwrap();
        assert_eq!(enrolled.node_id, "worker-a");
        let record = load_node(&store, &node_state_slug("Worker-A").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(record.node_id, "worker-a");
        assert_eq!(
            node_state_slug("worker-a").unwrap(),
            node_state_slug("Worker-A").unwrap()
        );
        let (_slug, stored) = stored_enroll(&store, &info.code);
        assert_eq!(stored["used"], true);
        assert_eq!(stored["used_by_node_id"], "worker-a");
        assert!(node_token_matches(&store, "Worker-A", &enrolled.node_token).unwrap());
        assert!(node_token_matches(&store, "worker-a", &enrolled.node_token).unwrap());
    }

    #[test]
    fn enroll_rejects_49_char_id_without_consuming_code() {
        let (_tmp, store) = open_store();
        let info = mint_enroll_token(&store, None, 60, "http://hub", 1_000).unwrap();
        let long_id = "a".repeat(49);
        let err = enroll_worker(&store, enroll_req(&info.code, &long_id, "W", 1_100)).unwrap_err();
        assert_eq!(err, EnrollFail::Invalid);
        let (_slug, stored) = stored_enroll(&store, &info.code);
        assert_eq!(stored["used"], false);
        assert!(stored["used_by_node_id"].is_null());
        let truncated = format!("{NODE_SLUG_PREFIX}{}", "a".repeat(48));
        assert!(load_node(&store, &truncated).unwrap().is_none());
    }

    #[test]
    fn local_hostname_prefers_gethostname_then_files_then_localhost() {
        assert_eq!(hostname_from_buffer(b"lab\0unused").as_deref(), Some("lab"));
        assert_eq!(hostname_from_buffer(b"  lab \0").as_deref(), Some("lab"));
        assert_eq!(hostname_from_buffer(&[0u8; 4]), None);
        assert_eq!(hostname_from_buffer(b"   \0"), None);
        assert_eq!(hostname_from_buffer(&[0xff, 0xfe]), None);

        assert_eq!(
            pick_hostname([
                Some("from-libc".to_string()),
                Some("from-proc".to_string()),
                Some("from-etc".to_string()),
            ]),
            "from-libc"
        );
        assert_eq!(
            pick_hostname([
                None,
                Some("from-proc".to_string()),
                Some("from-etc".to_string())
            ]),
            "from-proc"
        );
        assert_eq!(
            pick_hostname([Some(String::new()), None, Some("from-etc".to_string())]),
            "from-etc"
        );
        assert_eq!(pick_hostname([None, None, None]), "localhost");

        let got = local_hostname();
        assert!(!got.is_empty());
        if let Some(from_libc) = hostname_from_libc() {
            assert_eq!(got, from_libc);
        }
    }

    #[test]
    fn node_id_from_hostname_is_canonical_and_stores() {
        assert_eq!(node_id_from_hostname("ai-lab-0"), "ai-lab-0");
        assert_eq!(node_id_from_hostname("Foo.Example.com"), "foo-example-com");
        let enroll_host = node_id_from_hostname("enroll-mac.local");
        assert_eq!(enroll_host, "n-enroll-mac-local");
        assert!(node_state_slug(&enroll_host).is_ok(), "{enroll_host}");
        assert_eq!(node_id_from_hostname(""), "localhost");
        assert_eq!(node_id_from_hostname("@@@"), "localhost");
        for id in [
            "ai-lab-0",
            "foo-example-com",
            enroll_host.as_str(),
            "localhost",
        ] {
            assert!(canonical_node_id(id).is_ok(), "{id}");
            assert!(node_state_slug(id).is_ok(), "{id}");
        }
        assert!(canonical_node_id("a/b").is_err());
        assert!(canonical_node_id("foo.bar").is_err());
        assert!(canonical_node_id(&"a".repeat(49)).is_err());

        let truncated = node_id_from_hostname(&"h".repeat(70));
        assert_eq!(truncated.len(), 48);
        assert_eq!(truncated, "h".repeat(48));
        let (_long_tmp, long_store) = open_store();
        let long_node = ensure_hub_node(&long_store, hub_args(&truncated, 1_000, None)).unwrap();
        assert_eq!(long_node.node_id, truncated);

        let (_lab_tmp, lab_store) = open_store();
        let lab_id = node_id_from_hostname("lab.local");
        assert_eq!(lab_id, "lab-local");
        let lab_node = ensure_hub_node(&lab_store, hub_args(&lab_id, 2_000, None)).unwrap();
        assert_eq!(lab_node.node_id, "lab-local");
    }

    fn entry_sensitive(store: &RedbStore, slug: &str) -> bool {
        store
            .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, slug)
            .unwrap()
            .unwrap()
            .sensitive
    }
}
