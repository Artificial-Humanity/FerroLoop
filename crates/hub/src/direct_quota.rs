//! Direct vendor account-quota probes.
//!
//! `muse usage --json` and `claude auth status --json` run at most once every 60 seconds.
//! A cold probe waits at most 3 seconds and never holds the hub store lock. The caller
//! drops that guard before discovery reads quota. A `home_override` (unit tests) does not
//! execute the host `muse` or `claude` binary unless `FL_HUB_MUSE_BIN` or
//! `FL_HUB_CLAUDE_BIN` is set.

use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use fl_core::telemetry::AdapterKind;
use fl_exec::child_env::{child_env, helper_env};
use serde_json::Value;

use crate::discovery::{QuotaWindowUsage, SessionQuotaUsage, json_f64, parse_iso8601_unix_millis};

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
enum ProbeKind {
    Muse,
    Claude,
    Agy,
}

#[derive(Clone)]
struct Slot {
    key: String,
    at: Instant,
    stale: bool,
    value: Option<SessionQuotaUsage>,
}

#[derive(Default)]
struct ProbeCache {
    muse: Option<Slot>,
    claude: Option<Slot>,
    agy: Option<Slot>,
}

impl ProbeCache {
    fn slot_mut(&mut self, kind: ProbeKind) -> &mut Option<Slot> {
        match kind {
            ProbeKind::Muse => &mut self.muse,
            ProbeKind::Claude => &mut self.claude,
            ProbeKind::Agy => &mut self.agy,
        }
    }
}

static PROBE_LOCK: Mutex<()> = Mutex::new(());
static CACHE: Mutex<ProbeCache> = Mutex::new(ProbeCache {
    muse: None,
    claude: None,
    agy: None,
});

thread_local! {
    static PROBE_LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
}

fn cache_lock() -> std::sync::MutexGuard<'static, ProbeCache> {
    CACHE.lock().unwrap_or_else(|err| err.into_inner())
}

struct ProbeLockDepth;

impl ProbeLockDepth {
    fn enter() -> Self {
        PROBE_LOCK_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for ProbeLockDepth {
    fn drop(&mut self) {
        PROBE_LOCK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Serialize quota reads with tests that set probe environment variables.
///
/// Reentrant on this thread: an outer call holds `PROBE_LOCK`, and a nested
/// call runs without locking again. The depth drops on unwind.
pub(crate) fn with_probe_lock<T>(body: impl FnOnce() -> T) -> T {
    let nested = PROBE_LOCK_DEPTH.with(Cell::get) > 0;
    let _guard = (!nested).then(|| PROBE_LOCK.lock().unwrap_or_else(|err| err.into_inner()));
    let _depth = ProbeLockDepth::enter();
    body()
}

/// Mark every cached probe stale (and drop the live Antigravity answers) so the next read
/// runs the command again.
#[cfg(test)]
pub(crate) fn expire_direct_quota_cache() {
    let mut cache = cache_lock();
    if let Some(slot) = cache.muse.as_mut() {
        slot.stale = true;
    }
    if let Some(slot) = cache.claude.as_mut() {
        slot.stale = true;
    }
    if let Some(slot) = cache.agy.as_mut() {
        slot.stale = true;
    }
    drop(cache);
    AGY_LIVE_CACHE
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clear();
}

pub(crate) fn merge_account_quota(
    account: Option<SessionQuotaUsage>,
    disk: Option<SessionQuotaUsage>,
) -> Option<SessionQuotaUsage> {
    match (account, disk) {
        (None, None) => None,
        (Some(account), None) => Some(account),
        (None, Some(disk)) => Some(disk),
        (Some(mut account), Some(disk)) => {
            if account.tier.is_none() {
                account.tier = disk.tier;
            }
            if account.primary_window.is_none() {
                account.primary_window = disk.primary_window;
            }
            if account.secondary_window.is_none() {
                account.secondary_window = disk.secondary_window;
            }
            if disk.exhausted_reason.is_some() {
                account.exhausted_reason = disk.exhausted_reason;
            }
            Some(account)
        }
    }
}

pub(crate) fn muse_account_quota(home_override: Option<&Path>) -> Option<SessionQuotaUsage> {
    let bin = probe_bin(home_override, "FL_HUB_MUSE_BIN", "muse")?;
    let key = format!("muse:{}:{}", home_key(home_override), bin.display());
    cached(ProbeKind::Muse, &key, || {
        let raw = command_stdout(
            &bin,
            &["usage", "--json"],
            Some(AdapterKind::Muse),
            PROBE_TIMEOUT,
        )?;
        let value = parse_json_document(&raw)?;
        muse_allowance(&value)
    })
}

pub(crate) fn claude_account_quota(home_override: Option<&Path>) -> Option<SessionQuotaUsage> {
    let bin = probe_bin(home_override, "FL_HUB_CLAUDE_BIN", "claude");
    let override_json = usage_override_json("FL_CLAUDE_USAGE_JSON", home_override, bin.is_some());
    // A home override that skipped the binary is not a probe. Caching it would
    // evict the one Claude slot. A missing host binary in production is a miss
    // and stays cached for 60 seconds. Curl runs only when `home_override` is None.
    if bin.is_none() && home_override.is_some() {
        return claude_quota_uncached(home_override, None, override_json);
    }
    let key = format!(
        "claude:{}:{}:{}",
        home_key(home_override),
        bin.as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        override_json
            .as_ref()
            .map(|value| value.to_string())
            .unwrap_or_default()
    );
    cached(ProbeKind::Claude, &key, move || {
        claude_quota_uncached(home_override, bin.as_deref(), override_json)
    })
}

fn claude_quota_uncached(
    home_override: Option<&Path>,
    bin: Option<&Path>,
    override_json: Option<Value>,
) -> Option<SessionQuotaUsage> {
    let auth = bin.and_then(|path| {
        let raw = command_stdout(
            path,
            &["auth", "status", "--json"],
            Some(AdapterKind::Claude),
            PROBE_TIMEOUT,
        )?;
        parse_json_document(&raw)
    });
    let auth_tier = auth.as_ref().and_then(subscription_type);
    let creds = credentials_path(home_override).and_then(|path| read_claude_credentials(&path));
    let mut quota = if let Some(doc) = override_json {
        usage_windows(&doc)
    } else if let Some(creds) = creds.as_ref() {
        if let Some(embedded) = creds.embedded.clone() {
            Some(embedded)
        } else if home_override.is_none() && creds.rate_limit_tier.is_some() {
            creds
                .token
                .as_deref()
                .and_then(fetch_claude_oauth_usage)
                .and_then(|value| usage_windows(&value))
        } else {
            None
        }
    } else {
        None
    }
    .unwrap_or(SessionQuotaUsage {
        tier: None,
        primary_window: None,
        secondary_window: None,
        exhausted_reason: None,
    });
    // `subscriptionType` is written once at `claude auth login`, so the
    // credentials file and `claude auth status` stay on the old plan after an
    // upgrade. `~/.claude.json` and the live profile track the account.
    let fresh_tier = resolve_claude_tier(
        claude_json_plan(home_override),
        home_override.is_none(),
        || {
            creds
                .as_ref()
                .and_then(|creds| creds.token.as_deref())
                .and_then(fetch_claude_oauth_profile)
                .and_then(|profile| plan_from_profile(&profile))
        },
        auth_tier,
    );
    if let Some(tier) = fresh_tier {
        quota.tier = Some(tier);
    } else if quota.tier.is_none()
        && let Some(creds) = creds.as_ref()
    {
        quota.tier = creds.tier.clone();
    }
    if quota.tier.is_none() && quota.primary_window.is_none() && quota.secondary_window.is_none() {
        None
    } else {
        Some(quota)
    }
}

/// Picks the Claude plan tier. `~/.claude.json` wins when it names a plan
/// above `pro`; otherwise the live profile (fetched lazily, only when
/// `query_profile` is set and the json plan is missing or `pro`) may upgrade
/// it; `auth_tier` is the last resort.
fn resolve_claude_tier(
    json_plan: Option<String>,
    query_profile: bool,
    live_profile: impl FnOnce() -> Option<String>,
    auth_tier: Option<String>,
) -> Option<String> {
    let mut fresh_tier = json_plan;
    if query_profile
        && matches!(fresh_tier.as_deref(), None | Some("pro"))
        && let Some(live) = live_profile()
    {
        fresh_tier = Some(live);
    }
    fresh_tier.or(auth_tier)
}

/// Where to find the Antigravity language server for a quota read, and which model the
/// session runs. The route's address and token win; the environment fills a gap only when
/// the host may be probed (no `home_override`, host discovery on). `session` (the
/// conversation id) lets a later read that lacks the route reuse what an earlier one saw.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AgyQuotaTarget<'a> {
    pub address: Option<&'a str>,
    pub csrf: Option<&'a str>,
    pub model: Option<&'a str>,
    pub session: Option<&'a str>,
}

/// What an earlier read learned about one session's language server and model.
#[derive(Debug, Clone, Default)]
struct AgyHint {
    address: Option<String>,
    csrf: Option<String>,
    model: Option<String>,
}

const AGY_HINT_MAX: usize = 256;

type AgyHintMap = std::collections::BTreeMap<(String, String), AgyHint>;
static AGY_HINTS: Mutex<AgyHintMap> = Mutex::new(std::collections::BTreeMap::new());

/// Store `hint` for `key`, bounding the map. The caller holds the `AGY_HINTS` guard.
fn agy_store_hint(hints: &mut AgyHintMap, key: (String, String), hint: AgyHint) {
    if hints.len() >= AGY_HINT_MAX && !hints.contains_key(&key) {
        hints.clear();
    }
    hints.insert(key, hint);
}

fn agy_clean(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

/// Fill the fields `target` lacks from the hint stored for its session and store the merge
/// back. A field `target` has replaces the stored one; a missing field never erases it.
/// Hints are scoped to `home_override`, so another home never inherits them.
///
/// Where neither the caller nor an earlier read knows the model (a watch or transcript read
/// on a cold hub), the model stays `None` and the quota group defaults to `gemini-*`. That is
/// accepted: the model is only learned by a full discovery pass, and is not guessed here.
fn agy_resolve_hint(home_override: Option<&Path>, target: &AgyQuotaTarget<'_>) -> AgyHint {
    let mut merged = AgyHint {
        address: agy_clean(target.address),
        csrf: agy_clean(target.csrf),
        model: agy_clean(target.model),
    };
    let Some(session) = target.session.map(str::trim).filter(|s| !s.is_empty()) else {
        return merged;
    };
    let key = (home_key(home_override), session.to_string());
    let mut hints = AGY_HINTS.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(stored) = hints.get(&key) {
        merged.address = merged.address.or_else(|| stored.address.clone());
        merged.csrf = merged.csrf.or_else(|| stored.csrf.clone());
        merged.model = merged.model.or_else(|| stored.model.clone());
    }
    if merged.address.is_some() || merged.csrf.is_some() || merged.model.is_some() {
        agy_store_hint(&mut hints, key, merged.clone());
    }
    merged
}

/// Record the language-server address and token a route names for `session`, whether or not
/// the caller reads a quota now, so a later watch or transcript read finds them. A field the
/// route has replaces the stored one (a restarted language server gets a new endpoint); a
/// field it lacks never erases a stored one; the remembered model is per conversation and is
/// left alone.
pub(crate) fn remember_agy_endpoint(
    home_override: Option<&Path>,
    session: &str,
    address: Option<&str>,
    csrf: Option<&str>,
) {
    let (address, csrf) = (agy_clean(address), agy_clean(csrf));
    let session = session.trim();
    if session.is_empty() || (address.is_none() && csrf.is_none()) {
        return;
    }
    let key = (home_key(home_override), session.to_string());
    let mut hints = AGY_HINTS.lock().unwrap_or_else(|err| err.into_inner());
    let mut hint = hints.get(&key).cloned().unwrap_or_default();
    hint.address = address.or(hint.address);
    hint.csrf = csrf.or(hint.csrf);
    agy_store_hint(&mut hints, key, hint);
}

pub(crate) fn agy_account_quota(
    home_override: Option<&Path>,
    target: &AgyQuotaTarget<'_>,
) -> Option<SessionQuotaUsage> {
    // `FL_AGY_QUOTA_JSON` is an explicit override and stays authoritative.
    if let Ok(raw) = std::env::var("FL_AGY_QUOTA_JSON") {
        let raw = raw.trim();
        if !raw.is_empty() {
            let key = format!("agy:{}", home_key(home_override));
            let raw = raw.to_string();
            return cached(ProbeKind::Agy, &key, move || {
                let value = load_json_override(&raw)?;
                agy_allowance(&value)
            });
        }
    }
    let hint = agy_resolve_hint(home_override, target);
    let target = AgyQuotaTarget {
        address: hint.address.as_deref(),
        csrf: hint.csrf.as_deref(),
        model: hint.model.as_deref(),
        session: target.session,
    };
    let (port, csrf) = agy_live_endpoint(home_override, &target)?;
    let live = agy_live_snapshot(port, &csrf)?;
    agy_live_usage(&live, target.model)
}

/// Port and csrf token of the language server to ask, or `None` when there is none or
/// the token is not one plain header value.
fn agy_live_endpoint(
    home_override: Option<&Path>,
    target: &AgyQuotaTarget<'_>,
) -> Option<(u16, String)> {
    let env_ok = crate::discovery::agy_env_fallback_allowed(home_override);
    let from_env = |name: &str| {
        env_ok
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
    let csrf = clean(target.csrf).or_else(|| from_env("ANTIGRAVITY_CSRF_TOKEN"))?;
    if csrf.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let addresses = [clean(target.address), from_env("ANTIGRAVITY_LS_ADDRESS")];
    let port = addresses.into_iter().flatten().find_map(|addr| {
        let port = fl_wire::identity::agy_ls_port(&addr)?;
        // A cached answer (hit or miss) means the port was just tried; skip the connect probe.
        (agy_live_cache_has(port, &csrf) || fl_wire::identity::agy_ls_address_listening(&addr))
            .then_some(port)
    })?;
    Some((port, csrf))
}

/// One `RetrieveUserQuotaSummary` bucket.
#[derive(Debug, Clone, PartialEq)]
struct AgyBucket {
    id: String,
    window: String,
    used_percent: f64,
    resets_at_ms: Option<u64>,
}

/// One model's `quotaInfo` from `GetUserStatus`.
#[derive(Debug, Clone, PartialEq)]
struct AgyModelQuota {
    model_id: String,
    used_percent: f64,
    resets_at_ms: Option<u64>,
}

/// The parsed answers of the two language-server calls.
#[derive(Debug, Clone, PartialEq, Default)]
struct AgyLive {
    buckets: Vec<AgyBucket>,
    models: Vec<AgyModelQuota>,
    tier: Option<String>,
}

const AGY_LIVE_NEGATIVE_TTL: Duration = Duration::from_secs(10);
const AGY_LIVE_MAX_BYTES: usize = 1024 * 1024;

type AgyLiveCache = std::collections::BTreeMap<(u16, String), (Instant, Option<AgyLive>)>;
static AGY_LIVE_CACHE: Mutex<AgyLiveCache> = Mutex::new(std::collections::BTreeMap::new());

fn agy_live_cache_has(port: u16, csrf: &str) -> bool {
    AGY_LIVE_CACHE
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get(&(port, csrf.to_string()))
        .is_some_and(|(at, value)| at.elapsed() < agy_live_ttl(value.is_some()))
}

fn agy_live_ttl(hit: bool) -> Duration {
    if hit {
        CACHE_TTL
    } else {
        AGY_LIVE_NEGATIVE_TTL
    }
}

/// Both language-server answers for `port` and `csrf`, cached for [`CACHE_TTL`] (a miss for a shorter
/// time), so discovery and `/api/v1/watch` polls cost one pair of calls a minute.
fn agy_live_snapshot(port: u16, csrf: &str) -> Option<AgyLive> {
    let key = (port, csrf.to_string());
    {
        let cache = AGY_LIVE_CACHE.lock().unwrap_or_else(|err| err.into_inner());
        if let Some((at, value)) = cache.get(&key)
            && at.elapsed() < agy_live_ttl(value.is_some())
        {
            return value.clone();
        }
    }
    let live = fetch_agy_live(port, csrf);
    let mut cache = AGY_LIVE_CACHE.lock().unwrap_or_else(|err| err.into_inner());
    if cache.len() >= 64 {
        cache.clear();
    }
    cache.insert(key, (Instant::now(), live.clone()));
    live
}

fn fetch_agy_live(port: u16, csrf: &str) -> Option<AgyLive> {
    let summary = crate::discovery::agy_rpc(
        port,
        csrf,
        "RetrieveUserQuotaSummary",
        "{}",
        AGY_LIVE_MAX_BYTES,
    );
    // A dead server costs one timeout, not two.
    let status = summary.as_ref().and_then(|_| {
        crate::discovery::agy_rpc(port, csrf, "GetUserStatus", "{}", AGY_LIVE_MAX_BYTES)
    });
    let mut live = AgyLive::default();
    if let Some(doc) = summary.and_then(|body| serde_json::from_str::<Value>(&body).ok()) {
        live.buckets = parse_agy_buckets(&doc);
    }
    if let Some(doc) = status.and_then(|body| serde_json::from_str::<Value>(&body).ok()) {
        live.tier = parse_agy_tier(&doc);
        live.models = parse_agy_model_quotas(&doc);
    }
    (live.tier.is_some() || !live.buckets.is_empty() || !live.models.is_empty()).then_some(live)
}

/// `used_percent` for a protobuf-JSON `remainingFraction`. The wire format omits a zero,
/// so a missing fraction beside a reset time means nothing is left.
fn used_percent_of(entry: &Value) -> Option<f64> {
    let remaining = match json_f64(entry.get("remainingFraction")) {
        Some(fraction) => fraction,
        None if entry.get("resetTime").is_some() => 0.0,
        None => return None,
    };
    Some((1.0 - remaining).clamp(0.0, 1.0) * 100.0)
}

fn reset_ms(entry: &Value) -> Option<u64> {
    entry
        .get("resetTime")
        .and_then(Value::as_str)
        .and_then(parse_iso8601_unix_millis)
}

fn parse_agy_buckets(doc: &Value) -> Vec<AgyBucket> {
    let root = doc.get("response").unwrap_or(doc);
    let Some(groups) = root.get("groups").and_then(Value::as_array) else {
        return Vec::new();
    };
    groups
        .iter()
        .filter_map(|group| group.get("buckets").and_then(Value::as_array))
        .flatten()
        .filter_map(|bucket| {
            let id = bucket.get("bucketId").and_then(Value::as_str)?.to_string();
            let window = bucket
                .get("window")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| id.rsplit_once('-').map(|(_, tail)| tail.to_string()))?;
            Some(AgyBucket {
                used_percent: used_percent_of(bucket)?,
                resets_at_ms: reset_ms(bucket),
                id,
                window,
            })
        })
        .collect()
}

fn agy_user_status(doc: &Value) -> &Value {
    let root = doc.get("response").unwrap_or(doc);
    root.get("userStatus").unwrap_or(root)
}

fn parse_agy_tier(doc: &Value) -> Option<String> {
    let info = agy_user_status(doc).pointer("/planStatus/planInfo")?;
    let text = |key: &str| {
        info.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    text("planName").map(str::to_ascii_lowercase).or_else(|| {
        text("teamsTier").map(|tier| {
            tier.strip_prefix("TEAMS_TIER_")
                .unwrap_or(tier)
                .to_ascii_lowercase()
        })
    })
}

fn parse_agy_model_quotas(doc: &Value) -> Vec<AgyModelQuota> {
    let Some(configs) = agy_user_status(doc)
        .pointer("/cascadeModelConfigData/clientModelConfigs")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    configs
        .iter()
        .filter_map(|entry| {
            let info = entry.get("quotaInfo")?;
            Some(AgyModelQuota {
                model_id: entry.get("modelId").and_then(Value::as_str)?.to_string(),
                used_percent: used_percent_of(info)?,
                resets_at_ms: reset_ms(info),
            })
        })
        .collect()
}

/// `claude-*` and `gpt-*` models draw on the shared third-party buckets (`3p-*`); every
/// other model draws on the Gemini buckets.
fn agy_bucket_prefix(model: Option<&str>) -> &'static str {
    match model {
        Some(m) if m.starts_with("claude-") || m.starts_with("gpt-") => "3p-",
        _ => "gemini-",
    }
}

/// The configured entry for `model`: an exact id, or the id with a thinking suffix.
fn agy_model_quota<'a>(live: &'a AgyLive, model: &str) -> Option<&'a AgyModelQuota> {
    live.models.iter().find(|entry| {
        entry.model_id == model
            || entry
                .model_id
                .strip_prefix(model)
                .is_some_and(|rest| matches!(rest, "-low" | "-medium" | "-high"))
    })
}

fn agy_live_usage(live: &AgyLive, model: Option<&str>) -> Option<SessionQuotaUsage> {
    let prefix = agy_bucket_prefix(model);
    let window = |name: &str, minutes: u64| {
        live.buckets
            .iter()
            .find(|b| b.id.starts_with(prefix) && b.window == name)
            .map(|b| QuotaWindowUsage {
                used_percent: b.used_percent,
                window_minutes: Some(minutes),
                resets_at_ms: b.resets_at_ms,
            })
    };
    let mut primary_window = window("5h", 300);
    let secondary_window = window("weekly", 10_080);
    // The active model's own quota replaces the shared 5h bucket once it resets later.
    if let Some(entry) = model.and_then(|m| agy_model_quota(live, m))
        && entry.resets_at_ms > primary_window.as_ref().and_then(|w| w.resets_at_ms)
    {
        primary_window = Some(QuotaWindowUsage {
            used_percent: entry.used_percent,
            window_minutes: Some(300),
            resets_at_ms: entry.resets_at_ms,
        });
    }
    if live.tier.is_none() && primary_window.is_none() && secondary_window.is_none() {
        return None;
    }
    Some(SessionQuotaUsage {
        tier: live.tier.clone(),
        primary_window,
        secondary_window,
        exhausted_reason: None,
    })
}

pub(crate) fn claude_rate_limit_reason(raw: &str) -> Option<String> {
    let mut latest = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            // A later assistant turn with no error is a completed turn. It clears
            // an earlier rate-limit reason, the same way Muse clears on model_completed.
            "assistant" => match value.get("error") {
                None | Some(Value::Null) => latest = None,
                Some(Value::String(text)) if is_rate_text(text) => {
                    latest = Some(text.clone());
                }
                Some(_) => {}
            },
            "system" | "api-request" => {
                if let Some(found) = first_rate_string(&value, 0) {
                    latest = Some(found);
                }
            }
            _ => {}
        }
    }
    latest
}

pub(crate) fn antigravity_exhausted_reason(raw: &str) -> Option<String> {
    let mut latest = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || (!trimmed.contains("ERROR_MESSAGE") && !trimmed.contains("PLANNER_RESPONSE"))
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            // A later planner turn means the earlier quota error is no longer current.
            Some("PLANNER_RESPONSE") => latest = None,
            Some("ERROR_MESSAGE") => {
                let Some(error) = value.get("error").and_then(Value::as_str) else {
                    continue;
                };
                if is_rate_text(error) {
                    latest = Some(error.to_string());
                }
            }
            _ => {}
        }
    }
    latest
}

fn cached(
    kind: ProbeKind,
    key: &str,
    fetch: impl FnOnce() -> Option<SessionQuotaUsage>,
) -> Option<SessionQuotaUsage> {
    {
        let cache = cache_lock();
        if let Some(slot) = cache.slot_mut_ref(kind)
            && !slot.stale
            && slot.key == key
            && slot.at.elapsed() < CACHE_TTL
        {
            return slot.value.clone();
        }
    }
    let value = fetch();
    let mut cache = cache_lock();
    *cache.slot_mut(kind) = Some(Slot {
        key: key.to_string(),
        at: Instant::now(),
        stale: false,
        value: value.clone(),
    });
    value
}

impl ProbeCache {
    fn slot_mut_ref(&self, kind: ProbeKind) -> Option<&Slot> {
        match kind {
            ProbeKind::Muse => self.muse.as_ref(),
            ProbeKind::Claude => self.claude.as_ref(),
            ProbeKind::Agy => self.agy.as_ref(),
        }
    }
}

fn home_key(home_override: Option<&Path>) -> String {
    home_override
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "host".to_string())
}

fn probe_bin(home_override: Option<&Path>, var: &str, command: &str) -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os(var) {
        let path = PathBuf::from(raw);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    if home_override.is_some() {
        return None;
    }
    resolve_host_bin(command)
}

fn resolve_host_bin(command: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(command);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME")?;
    let local = PathBuf::from(home).join(".local").join("bin").join(command);
    local.is_file().then_some(local)
}

fn usage_override_json(var: &str, home_override: Option<&Path>, bin_set: bool) -> Option<Value> {
    let raw = std::env::var(var).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if var == "FL_CLAUDE_USAGE_JSON" && home_override.is_some() && !bin_set {
        return None;
    }
    load_json_override(raw)
}

fn load_json_override(raw: &str) -> Option<Value> {
    if raw.starts_with('{') {
        return serde_json::from_str(raw).ok();
    }
    let text = fs::read_to_string(raw).ok()?;
    serde_json::from_str(&text).ok()
}

/// Runs `bin` and returns its stdout. The child gets a scrubbed environment (see
/// [`fl_exec::child_env`]): the baseline plus `kind`'s own vendor variables, or the baseline alone
/// when `kind` is `None` (`curl`). It never sees `FL_VAULT_MASTER_KEY`, `FL_HUB_*`, GitHub
/// credentials, another vendor's keys or a proxy.
pub(crate) fn command_stdout(
    bin: &Path,
    args: &[&str],
    kind: Option<AdapterKind>,
    timeout: Duration,
) -> Option<String> {
    let env = match kind {
        Some(kind) => child_env(kind, &[], &[]),
        None => helper_env(&[], &[]),
    };
    let mut child = Command::new(bin)
        .args(args)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                // Detach the reader. Joining can block while a grandchild still
                // owns stdout and would hold the process-wide probe lock. Do not
                // kill the process group: the child shares the hub's group.
                let _ = child.kill();
                let _ = child.wait();
                drop(reader);
                return None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(reader);
                return None;
            }
        }
    };
    let output = reader.join().ok()?;
    status.success().then_some(output)
}

fn parse_json_document(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    let start = trimmed.find('{')?;
    serde_json::from_str(&trimmed[start..]).ok()
}

fn muse_allowance(value: &Value) -> Option<SessionQuotaUsage> {
    let allowance = value.get("allowance")?;
    if allowance.is_null() {
        return None;
    }
    let tier = allowance
        .get("tier")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned);
    let primary_window = allowance
        .get("window")
        .and_then(|window| window_from_usage(window, None));
    let secondary_window = allowance
        .get("weekly")
        .and_then(|window| window_from_usage(window, Some(10_080)));
    if tier.is_none() && primary_window.is_none() && secondary_window.is_none() {
        return None;
    }
    Some(SessionQuotaUsage {
        tier,
        primary_window,
        secondary_window,
        exhausted_reason: None,
    })
}

fn agy_allowance(value: &Value) -> Option<SessionQuotaUsage> {
    if value.get("allowance").is_some() {
        return muse_allowance(value);
    }
    if value.get("window").is_some() || value.get("weekly").is_some() {
        let wrapped = serde_json::json!({ "allowance": value });
        return muse_allowance(&wrapped);
    }
    let tier = value
        .get("tier")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned);
    let primary_window = value
        .get("primary_window")
        .and_then(|window| window_from_usage(window, Some(300)));
    let secondary_window = value
        .get("secondary_window")
        .and_then(|window| window_from_usage(window, Some(10_080)));
    if tier.is_none() && primary_window.is_none() && secondary_window.is_none() {
        return None;
    }
    Some(SessionQuotaUsage {
        tier,
        primary_window,
        secondary_window,
        exhausted_reason: None,
    })
}

fn usage_windows(value: &Value) -> Option<SessionQuotaUsage> {
    let root = value.get("rate_limits").unwrap_or(value);
    let five = root.get("five_hour");
    let seven = root.get("seven_day");
    let primary_window = five.and_then(|window| window_from_usage(window, Some(300)));
    let secondary_window = seven.and_then(|window| window_from_usage(window, Some(10_080)));
    let tier = root
        .get("tier")
        .or_else(|| value.get("tier"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned);
    if tier.is_none() && primary_window.is_none() && secondary_window.is_none() {
        return None;
    }
    Some(SessionQuotaUsage {
        tier,
        primary_window,
        secondary_window,
        exhausted_reason: None,
    })
}

fn window_from_usage(value: &Value, default_minutes: Option<u64>) -> Option<QuotaWindowUsage> {
    let used_percent = json_f64(value.get("usedPercent"))
        .or_else(|| json_f64(value.get("used_percent")))
        .or_else(|| json_f64(value.get("utilization")))
        .or_else(|| json_f64(value.get("used_percentage")))?;
    let window_minutes = value
        .get("windowDurationMins")
        .and_then(Value::as_u64)
        .or_else(|| value.get("window_minutes").and_then(Value::as_u64))
        .or(default_minutes);
    Some(QuotaWindowUsage {
        used_percent,
        window_minutes,
        resets_at_ms: resets_ms(value),
    })
}

fn resets_ms(value: &Value) -> Option<u64> {
    if let Some(ms) =
        json_u64(value.get("resetsAtMs")).or_else(|| json_u64(value.get("resets_at_ms")))
    {
        return Some(ms);
    }
    let raw = value.get("resets_at").or_else(|| value.get("resetsAt"))?;
    if let Some(text) = raw.as_str() {
        return parse_iso8601_unix_millis(text.trim().trim_end_matches('.'));
    }
    let n = json_u64(Some(raw))?;
    if n == 0 {
        return None;
    }
    if n < 1_000_000_000_000 {
        Some(n.saturating_mul(1000))
    } else {
        Some(n)
    }
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_i64().filter(|n| *n >= 0).map(|n| n as u64))
        .or_else(|| {
            value
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| n as u64)
        })
}

fn subscription_type(value: &Value) -> Option<String> {
    value
        .get("subscriptionType")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

/// Plan name for an Anthropic `organization_type` such as `claude_max`.
fn plan_from_org_type(text: &str) -> Option<String> {
    let plan = text.trim().to_ascii_lowercase();
    let plan = plan.strip_prefix("claude_").unwrap_or(&plan);
    matches!(plan, "max" | "pro" | "team" | "enterprise").then(|| plan.to_owned())
}

/// Plan name for a rate-limit tier such as `default_claude_max_5x`. Only the
/// paid consumer plans are named. Other tiers are not plan names.
fn plan_from_rate_tier(text: &str) -> Option<String> {
    let text = text.to_ascii_lowercase();
    if text.contains("claude_max") {
        Some("max".to_owned())
    } else if text.contains("claude_pro") {
        Some("pro".to_owned())
    } else {
        None
    }
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// The plan from `<home>/.claude.json` `oauthAccount`. Claude Code refreshes it
/// after an account change.
fn claude_json_plan(home_override: Option<&Path>) -> Option<String> {
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    let raw = fs::read_to_string(home.join(".claude.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let account = value.get("oauthAccount")?;
    string_field(account, "organizationType")
        .and_then(plan_from_org_type)
        .or_else(|| {
            ["organizationRateLimitTier", "userRateLimitTier"]
                .into_iter()
                .filter_map(|key| string_field(account, key))
                .find_map(plan_from_rate_tier)
        })
}

/// The plan from the `/api/oauth/profile` response.
fn plan_from_profile(profile: &Value) -> Option<String> {
    let account = profile.get("account");
    let organization = profile.get("organization");
    if account
        .and_then(|a| a.get("has_claude_max"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Some("max".to_owned());
    }
    organization
        .and_then(|o| string_field(o, "organization_type"))
        .and_then(plan_from_org_type)
        .or_else(|| {
            organization
                .and_then(|o| string_field(o, "rate_limit_tier"))
                .and_then(plan_from_rate_tier)
        })
        .or_else(|| {
            (account
                .and_then(|a| a.get("has_claude_pro"))
                .and_then(Value::as_bool)
                == Some(true))
            .then(|| "pro".to_owned())
        })
}

struct ClaudeCredentials {
    tier: Option<String>,
    rate_limit_tier: Option<String>,
    token: Option<String>,
    embedded: Option<SessionQuotaUsage>,
}

fn credentials_path(home_override: Option<&Path>) -> Option<PathBuf> {
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    Some(home.join(".claude").join(".credentials.json"))
}

fn read_claude_credentials(path: &Path) -> Option<ClaudeCredentials> {
    let raw = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let oauth = value.get("claudeAiOauth")?;
    let tier = subscription_type(oauth);
    let rate_limit_tier = oauth
        .get("rateLimitTier")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned);
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned);
    let embedded = usage_windows(oauth).or_else(|| usage_windows(&value));
    Some(ClaudeCredentials {
        tier,
        rate_limit_tier,
        token,
        embedded,
    })
}

fn fetch_claude_oauth_usage(token: &str) -> Option<Value> {
    fetch_claude_oauth_json(token, "usage")
}

fn fetch_claude_oauth_profile(token: &str) -> Option<Value> {
    fetch_claude_oauth_json(token, "profile")
}

fn fetch_claude_oauth_json(token: &str, endpoint: &str) -> Option<Value> {
    if token.is_empty()
        || token
            .chars()
            .any(|ch| ch == '"' || ch == '\n' || ch == '\r' || ch == '\\')
    {
        return None;
    }
    #[cfg(unix)]
    {
        fetch_claude_oauth_unix(token, endpoint)
    }
    #[cfg(not(unix))]
    {
        let _ = endpoint;
        None
    }
}

#[cfg(unix)]
fn fetch_claude_oauth_unix(token: &str, endpoint: &str) -> Option<Value> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(1);
    let name = format!(
        "fl-hub-claude-usage-{}-{}.cfg",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join(name);
    let _cleanup = TempSecret(&path);
    let body = format!(
        "silent\nmax-time = 3\nurl = \"https://api.anthropic.com/api/oauth/{endpoint}\"\nheader = \"Accept: application/json\"\nheader = \"anthropic-beta: oauth-2025-04-20\"\nheader = \"User-Agent: claude-code/2.1.312\"\nheader = \"Authorization: Bearer {token}\"\n"
    );
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    file.write_all(body.as_bytes()).ok()?;
    drop(file);
    let raw = command_stdout(
        Path::new("curl"),
        &[
            "--fail",
            "--silent",
            "--max-time",
            "3",
            "--config",
            &path.to_string_lossy(),
        ],
        None,
        PROBE_TIMEOUT,
    )?;
    parse_json_document(&raw)
}

struct TempSecret<'a>(&'a Path);

impl Drop for TempSecret<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}

fn is_rate_text(text: &str) -> bool {
    // A SHA-256 digest is alphanumeric. Splitting only on non-digits used to
    // treat the `429` inside `...b429d...` as an HTTP status.
    if text.len() >= 32 && text.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("session limit")
    {
        return true;
    }
    let has_429 = lower
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part == "429");
    if has_429 {
        return true;
    }
    if lower.contains("quota") {
        return lower.contains("exhausted")
            || lower.contains("exceeded")
            || lower.contains("reached")
            || lower.contains("limit");
    }
    false
}

fn first_rate_string(value: &Value, depth: usize) -> Option<String> {
    if depth > 4 {
        return None;
    }
    match value {
        Value::String(text) if text.len() <= 500 && is_rate_text(text) => Some(text.clone()),
        Value::Object(map) => {
            for (key, child) in map {
                if matches!(
                    key.as_str(),
                    "uuid"
                        | "sessionId"
                        | "parentUuid"
                        | "cwd"
                        | "timestamp"
                        | "requestDigest"
                        | "shapeHash"
                        | "id"
                ) {
                    continue;
                }
                if let Some(found) = first_rate_string(child, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => {
            for item in items.iter().take(8) {
                if let Some(found) = first_rate_string(item, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::read_session_quota_usage_locked;
    use fl_core::At;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn resolve_claude_tier_prefers_json_and_fetches_profile_lazily() {
        let s = |v: &str| Some(v.to_string());
        let calls = Cell::new(0);
        let live = |value: Option<String>| {
            let calls = &calls;
            move || {
                calls.set(calls.get() + 1);
                value
            }
        };
        assert_eq!(
            resolve_claude_tier(s("pro"), true, live(s("max")), None),
            s("max")
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            resolve_claude_tier(s("pro"), true, live(None), None),
            s("pro")
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(
            resolve_claude_tier(s("max"), true, live(s("pro")), s("pro")),
            s("max")
        );
        assert_eq!(calls.get(), 2, "json max must not fetch the profile");
        assert_eq!(
            resolve_claude_tier(None, false, live(s("max")), s("pro")),
            s("pro")
        );
        assert_eq!(calls.get(), 2, "query_profile=false must not fetch");
        assert_eq!(
            resolve_claude_tier(None, true, live(None), s("pro")),
            s("pro")
        );
        assert_eq!(
            resolve_claude_tier(None, true, live(s("max")), s("pro")),
            s("max")
        );
        assert_eq!(calls.get(), 4);
    }

    fn write_script(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn counter(path: &Path) -> u64 {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    struct ClearProbeEnv;

    impl Drop for ClearProbeEnv {
        fn drop(&mut self) {
            // SAFETY: tests that set these vars hold PROBE_LOCK until this guard drops.
            unsafe {
                std::env::remove_var("FL_HUB_MUSE_BIN");
                std::env::remove_var("FL_HUB_CLAUDE_BIN");
                std::env::remove_var("FL_CLAUDE_USAGE_JSON");
                std::env::remove_var("FL_AGY_QUOTA_JSON");
                std::env::remove_var("ANTIGRAVITY_LS_ADDRESS");
                std::env::remove_var("ANTIGRAVITY_CSRF_TOKEN");
                std::env::remove_var("FL_WIRE_DISABLE_HOST_DISCOVERY");
            }
        }
    }

    #[test]
    fn muse_poller_caches_for_60s_and_merges_disk_exhaustion() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let session = "muse-poll-1";
            let dir = home
                .join(".local")
                .join("share")
                .join("muse")
                .join("sessions")
                .join(session);
            fs::create_dir_all(&dir).unwrap();
            let reason = "API error 429: Subscription quota exhausted. Your usage window resets at 2026-11-02T00:00:00Z.";
            let failure = serde_json::json!({
                "payload_type": "runtime.session",
                "payload": { "event": { "kind": "failed", "reason": reason } }
            });
            fs::write(dir.join("session.jsonl"), format!("{failure}\n")).unwrap();

            let counter_path = tmp.path().join("muse-count");
            let script = tmp.path().join("muse");
            write_script(
                &script,
                &format!(
                    "#!/bin/sh\nf=\"{}\"\nn=0\nif [ -f \"$f\" ]; then n=$(cat \"$f\"); fi\nn=$((n+1))\nprintf '%s\\n' \"$n\" > \"$f\"\nprintf '%s\\n' '{{\"allowance\":{{\"tier\":\"muse-pro\",\"window\":{{\"usedPercent\":12.5,\"windowDurationMins\":300,\"resetsAtMs\":1700000000000}},\"weekly\":{{\"usedPercent\":40,\"resetsAtMs\":1700086400000}}}}}}'\n",
                    counter_path.display()
                ),
            );
            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::set_var("FL_HUB_MUSE_BIN", &script);
                std::env::remove_var("FL_HUB_CLAUDE_BIN");
                std::env::remove_var("FL_CLAUDE_USAGE_JSON");
                std::env::remove_var("FL_AGY_QUOTA_JSON");
            }
            expire_direct_quota_cache();

            let first = read_session_quota_usage_locked(session, Some(home)).expect("muse quota");
            assert_eq!(counter(&counter_path), 1);
            assert_eq!(first.tier.as_deref(), Some("muse-pro"));
            assert_eq!(first.exhausted_reason.as_deref(), Some(reason));
            assert_eq!(
                first.primary_window,
                Some(QuotaWindowUsage {
                    used_percent: 12.5,
                    window_minutes: Some(300),
                    resets_at_ms: Some(1_700_000_000_000),
                })
            );
            assert_eq!(
                first.secondary_window,
                Some(QuotaWindowUsage {
                    used_percent: 40.0,
                    window_minutes: Some(10_080),
                    resets_at_ms: Some(1_700_086_400_000),
                })
            );

            let second = read_session_quota_usage_locked(session, Some(home)).expect("cached");
            assert_eq!(counter(&counter_path), 1);
            assert_eq!(second, first);

            expire_direct_quota_cache();
            let _third = read_session_quota_usage_locked(session, Some(home)).expect("refreshed");
            assert_eq!(counter(&counter_path), 2);

            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::remove_var("FL_HUB_MUSE_BIN");
            }
            let disk_only = read_session_quota_usage_locked(session, Some(home)).expect("disk");
            assert_eq!(counter(&counter_path), 2);
            assert!(disk_only.tier.is_none());
            assert!(disk_only.secondary_window.is_none());
            assert_eq!(disk_only.exhausted_reason.as_deref(), Some(reason));
            assert_eq!(disk_only.primary_window.unwrap().used_percent, 100.0);
        });
    }

    #[test]
    fn claude_poller_caches_tier_and_merges_usage_windows() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let dir = home.join(".claude").join("projects").join("demo");
            fs::create_dir_all(&dir).unwrap();
            let transcript = dir.join("claude-poll.jsonl");
            let early = serde_json::json!({
                "type": "assistant",
                "error": "rate_limit",
                "uuid": "aaaaaaaa-bbbb-4291-cccc-dddddddddddd"
            });
            let latest = serde_json::json!({
                "type": "system",
                "subtype": "api_error",
                "uuid": "aaaaaaaa-bbbb-4291-cccc-dddddddddddd",
                "message": "HTTP 429 rate limit reached"
            });
            fs::write(&transcript, format!("{early}\n{latest}\n")).unwrap();

            let counter_path = tmp.path().join("claude-count");
            let script = tmp.path().join("claude");
            write_script(
                &script,
                &format!(
                    "#!/bin/sh\nf=\"{}\"\nn=0\nif [ -f \"$f\" ]; then n=$(cat \"$f\"); fi\nn=$((n+1))\nprintf '%s\\n' \"$n\" > \"$f\"\nprintf '%s\\n' '{{\"subscriptionType\":\"max\",\"email\":\"hidden@example.com\"}}'\n",
                    counter_path.display()
                ),
            );
            let usage = r#"{"five_hour":{"utilization":10,"resets_at":"2026-11-05T01:11:25.780Z"},"seven_day":{"utilization":20,"resets_at":"2026-11-05T01:11:25.780Z"}}"#;
            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::set_var("FL_HUB_CLAUDE_BIN", &script);
                std::env::set_var("FL_CLAUDE_USAGE_JSON", usage);
                std::env::remove_var("FL_HUB_MUSE_BIN");
                std::env::remove_var("FL_AGY_QUOTA_JSON");
            }
            expire_direct_quota_cache();
            let expected_ms = At::parse("2026-11-05T01:11:25.780Z").unwrap().unix_millis();

            let first =
                read_session_quota_usage_locked("claude-poll", Some(home)).expect("claude quota");
            assert_eq!(counter(&counter_path), 1);
            assert_eq!(first.tier.as_deref(), Some("max"));
            assert_eq!(
                first.exhausted_reason.as_deref(),
                Some("HTTP 429 rate limit reached")
            );
            assert_eq!(
                first.primary_window,
                Some(QuotaWindowUsage {
                    used_percent: 10.0,
                    window_minutes: Some(300),
                    resets_at_ms: Some(expected_ms),
                })
            );
            assert_eq!(
                first.secondary_window,
                Some(QuotaWindowUsage {
                    used_percent: 20.0,
                    window_minutes: Some(10_080),
                    resets_at_ms: Some(expected_ms),
                })
            );

            let _second = read_session_quota_usage_locked("claude-poll", Some(home)).unwrap();
            assert_eq!(counter(&counter_path), 1);

            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::remove_var("FL_HUB_CLAUDE_BIN");
                std::env::remove_var("FL_CLAUDE_USAGE_JSON");
            }
            let guarded = read_session_quota_usage_locked("claude-poll", Some(home)).unwrap();
            assert_eq!(counter(&counter_path), 1);
            assert!(guarded.tier.is_none());
            assert!(guarded.primary_window.is_none());
            assert_eq!(
                guarded.exhausted_reason.as_deref(),
                Some("HTTP 429 rate limit reached")
            );
        });
    }

    #[test]
    fn claude_credentials_windows_merge_without_a_subprocess() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let claude_dir = home.join(".claude");
            fs::create_dir_all(&claude_dir).unwrap();
            fs::write(
                claude_dir.join(".credentials.json"),
                r#"{"claudeAiOauth":{"subscriptionType":"pro","rateLimitTier":"default_claude_max","accessToken":"not-a-real-token","five_hour":{"utilization":5,"resets_at":"2026-11-05T01:11:25.780Z"},"seven_day":{"utilization":9,"resets_at":"2026-11-05T01:11:25.780Z"}}}"#,
            )
            .unwrap();
            let projects = claude_dir.join("projects").join("demo");
            fs::create_dir_all(&projects).unwrap();
            fs::write(
                projects.join("cred-session.jsonl"),
                "{\"type\":\"api-request\",\"message\":\"quota exceeded for this session\"}\n",
            )
            .unwrap();
            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::remove_var("FL_HUB_CLAUDE_BIN");
                std::env::remove_var("FL_CLAUDE_USAGE_JSON");
            }
            expire_direct_quota_cache();
            let quota =
                read_session_quota_usage_locked("cred-session", Some(home)).expect("credentials");
            assert_eq!(quota.tier.as_deref(), Some("pro"));
            let primary = quota.primary_window.as_ref().unwrap();
            assert_eq!(primary.used_percent, 5.0);
            assert_eq!(primary.window_minutes, Some(300));
            assert_eq!(quota.secondary_window.as_ref().unwrap().used_percent, 9.0);
            assert_eq!(
                quota.exhausted_reason.as_deref(),
                Some("quota exceeded for this session")
            );
            let raw = fs::read_to_string(claude_dir.join(".credentials.json")).unwrap();
            assert!(!raw.contains("sk-ant"));
        });
    }

    #[test]
    fn claude_json_oauth_account_overrides_a_stale_pro_subscription() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let claude_dir = home.join(".claude");
            fs::create_dir_all(&claude_dir).unwrap();
            fs::write(
                claude_dir.join(".credentials.json"),
                r#"{"claudeAiOauth":{"subscriptionType":"pro","accessToken":"not-a-real-token","five_hour":{"utilization":5,"resets_at":"2026-11-05T01:11:25.780Z"}}}"#,
            )
            .unwrap();
            fs::write(
                home.join(".claude.json"),
                r#"{"oauthAccount":{"organizationType":"claude_max","organizationRateLimitTier":"default_claude_max_5x"}}"#,
            )
            .unwrap();
            let bin = home.join("claude-stub");
            write_script(
                &bin,
                "#!/bin/sh\nprintf '%s\\n' '{\"subscriptionType\":\"pro\"}'\n",
            );
            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::set_var("FL_HUB_CLAUDE_BIN", &bin);
                std::env::remove_var("FL_CLAUDE_USAGE_JSON");
            }
            expire_direct_quota_cache();
            let quota = claude_account_quota(Some(home)).expect("quota");
            assert_eq!(quota.tier.as_deref(), Some("max"));
            assert_eq!(quota.primary_window.as_ref().unwrap().used_percent, 5.0);
        });
    }

    #[test]
    fn claude_json_rate_tier_names_the_plan_when_the_org_type_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |json: &str| fs::write(tmp.path().join(".claude.json"), json).unwrap();
        write(r#"{"oauthAccount":{"organizationRateLimitTier":"default_claude_max_20x"}}"#);
        assert_eq!(claude_json_plan(Some(tmp.path())).as_deref(), Some("max"));
        write(r#"{"oauthAccount":{"userRateLimitTier":"default_claude_max_5x"}}"#);
        assert_eq!(claude_json_plan(Some(tmp.path())).as_deref(), Some("max"));
        write(r#"{"oauthAccount":{"organizationType":"claude_pro"}}"#);
        assert_eq!(claude_json_plan(Some(tmp.path())).as_deref(), Some("pro"));
        write(r#"{"oauthAccount":{"organizationType":"claude_team"}}"#);
        assert_eq!(claude_json_plan(Some(tmp.path())).as_deref(), Some("team"));
        write(
            r#"{"oauthAccount":{"organizationType":"mystery","organizationRateLimitTier":"default"}}"#,
        );
        assert_eq!(claude_json_plan(Some(tmp.path())), None);
        write(r#"{"no":"account"}"#);
        assert_eq!(claude_json_plan(Some(tmp.path())), None);
        write("not json");
        assert_eq!(claude_json_plan(Some(tmp.path())), None);
    }

    #[test]
    fn oauth_profile_names_the_live_plan() {
        let max_flag: Value =
            serde_json::from_str(r#"{"account":{"has_claude_max":true,"has_claude_pro":false}}"#)
                .unwrap();
        assert_eq!(plan_from_profile(&max_flag).as_deref(), Some("max"));
        let org: Value = serde_json::from_str(
            r#"{"account":{"has_claude_max":false},"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_5x"}}"#,
        )
        .unwrap();
        assert_eq!(plan_from_profile(&org).as_deref(), Some("max"));
        let tier_only: Value = serde_json::from_str(
            r#"{"organization":{"rate_limit_tier":"default_claude_max_20x"}}"#,
        )
        .unwrap();
        assert_eq!(plan_from_profile(&tier_only).as_deref(), Some("max"));
        let pro: Value = serde_json::from_str(
            r#"{"account":{"has_claude_max":false,"has_claude_pro":true},"organization":{"organization_type":"claude_pro"}}"#,
        )
        .unwrap();
        assert_eq!(plan_from_profile(&pro).as_deref(), Some("pro"));
        assert_eq!(plan_from_profile(&serde_json::json!({})), None);
    }

    #[test]
    fn antigravity_error_merges_with_quota_json_cache() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let conv = "agy-quota-1";
            let logs = home
                .join(".gemini")
                .join("antigravity-cli")
                .join("brain")
                .join(conv)
                .join(".system_generated")
                .join("logs");
            fs::create_dir_all(&logs).unwrap();
            let lines = [
                serde_json::json!({"type":"ERROR_MESSAGE","error":"disk full"}).to_string(),
                serde_json::json!({"type":"ERROR_MESSAGE","error":"429 RESOURCE_EXHAUSTED: quota exceeded"}).to_string(),
            ];
            fs::write(
                logs.join("transcript.jsonl"),
                format!("{}\n", lines.join("\n")),
            )
            .unwrap();
            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::set_var(
                    "FL_AGY_QUOTA_JSON",
                    r#"{"tier":"Google AI Pro","window":{"usedPercent":15,"windowDurationMins":300,"resetsAtMs":1700000000000},"weekly":{"usedPercent":55,"resetsAtMs":1700086400000}}"#,
                );
            }
            expire_direct_quota_cache();
            let first = read_session_quota_usage_locked(conv, Some(home)).expect("agy quota");
            assert_eq!(first.tier.as_deref(), Some("Google AI Pro"));
            let primary = first.primary_window.as_ref().unwrap();
            let secondary = first.secondary_window.as_ref().unwrap();
            assert_eq!(primary.used_percent, 15.0);
            assert_eq!(primary.window_minutes, Some(300));
            assert_eq!(secondary.used_percent, 55.0);
            assert_eq!(secondary.window_minutes, Some(10_080));
            assert_eq!(
                first.exhausted_reason.as_deref(),
                Some("429 RESOURCE_EXHAUSTED: quota exceeded")
            );

            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::set_var(
                    "FL_AGY_QUOTA_JSON",
                    r#"{"tier":"changed","window":{"usedPercent":99,"windowDurationMins":300,"resetsAtMs":1}}"#,
                );
            }
            let cached = read_session_quota_usage_locked(conv, Some(home)).expect("cached agy");
            assert_eq!(cached.tier.as_deref(), Some("Google AI Pro"));
            assert_eq!(cached.primary_window.unwrap().used_percent, 15.0);

            expire_direct_quota_cache();
            let refreshed = read_session_quota_usage_locked(conv, Some(home)).expect("refreshed");
            assert_eq!(refreshed.tier.as_deref(), Some("changed"));
            assert_eq!(refreshed.primary_window.unwrap().used_percent, 99.0);
            assert_eq!(
                refreshed.exhausted_reason.as_deref(),
                Some("429 RESOURCE_EXHAUSTED: quota exceeded")
            );
            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::remove_var("FL_AGY_QUOTA_JSON");
            }
        });
    }

    #[test]
    fn home_override_without_claude_bin_does_not_evict_probe_cache() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let home_a = tempfile::tempdir().unwrap();
            let dir_a = home_a.path().join(".claude").join("projects").join("demo");
            fs::create_dir_all(&dir_a).unwrap();
            fs::write(dir_a.join("claude-home-a.jsonl"), "{\"type\":\"user\"}\n").unwrap();

            let counter_path = home_a.path().join("claude-count");
            let script = home_a.path().join("claude");
            write_script(
                &script,
                &format!(
                    "#!/bin/sh\nf=\"{}\"\nn=0\nif [ -f \"$f\" ]; then n=$(cat \"$f\"); fi\nn=$((n+1))\nprintf '%s\\n' \"$n\" > \"$f\"\nprintf '%s\\n' '{{\"subscriptionType\":\"max\"}}'\n",
                    counter_path.display()
                ),
            );
            let usage = r#"{"five_hour":{"utilization":10,"resets_at":"2026-11-05T01:11:25.780Z"},"seven_day":{"utilization":20,"resets_at":"2026-11-05T01:11:25.780Z"}}"#;
            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::set_var("FL_HUB_CLAUDE_BIN", &script);
                std::env::set_var("FL_CLAUDE_USAGE_JSON", usage);
                std::env::remove_var("FL_HUB_MUSE_BIN");
                std::env::remove_var("FL_AGY_QUOTA_JSON");
            }
            expire_direct_quota_cache();

            let first = read_session_quota_usage_locked("claude-home-a", Some(home_a.path()))
                .expect("home A");
            assert_eq!(counter(&counter_path), 1);
            assert_eq!(first.tier.as_deref(), Some("max"));
            assert_eq!(first.primary_window.as_ref().unwrap().used_percent, 10.0);

            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::remove_var("FL_HUB_CLAUDE_BIN");
            }
            let home_b = tempfile::tempdir().unwrap();
            let dir_b = home_b.path().join(".claude");
            fs::create_dir_all(dir_b.join("projects").join("demo")).unwrap();
            fs::write(
                dir_b.join(".credentials.json"),
                r#"{"claudeAiOauth":{"subscriptionType":"pro","accessToken":"not-a-real-token","five_hour":{"utilization":5,"resets_at":"2026-11-05T01:11:25.780Z"},"seven_day":{"utilization":9,"resets_at":"2026-11-05T01:11:25.780Z"}}}"#,
            )
            .unwrap();
            fs::write(
                dir_b
                    .join("projects")
                    .join("demo")
                    .join("claude-home-b.jsonl"),
                "{\"type\":\"user\"}\n",
            )
            .unwrap();
            let from_b = read_session_quota_usage_locked("claude-home-b", Some(home_b.path()))
                .expect("home B credentials");
            assert_eq!(from_b.tier.as_deref(), Some("pro"));
            assert_eq!(from_b.primary_window.as_ref().unwrap().used_percent, 5.0);
            assert_eq!(counter(&counter_path), 1);

            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::set_var("FL_HUB_CLAUDE_BIN", &script);
                std::env::set_var("FL_CLAUDE_USAGE_JSON", usage);
            }
            let again = read_session_quota_usage_locked("claude-home-a", Some(home_a.path()))
                .expect("cached home A");
            assert_eq!(counter(&counter_path), 1);
            assert_eq!(again, first);
        });
    }

    #[test]
    fn claude_rate_limit_tier_is_not_a_plan_name() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let home = tempfile::tempdir().unwrap();
            let dir = home.path().join(".claude");
            fs::create_dir_all(dir.join("projects").join("demo")).unwrap();
            fs::write(
                dir.join(".credentials.json"),
                r#"{"claudeAiOauth":{"rateLimitTier":"default_claude_max","accessToken":"not-a-real-token","five_hour":{"utilization":5,"resets_at":"2026-11-05T01:11:25.780Z"},"seven_day":{"utilization":9,"resets_at":"2026-11-05T01:11:25.780Z"}}}"#,
            )
            .unwrap();
            fs::write(
                dir.join("projects").join("demo").join("tier-session.jsonl"),
                "{\"type\":\"user\"}\n",
            )
            .unwrap();
            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::remove_var("FL_HUB_CLAUDE_BIN");
                std::env::remove_var("FL_CLAUDE_USAGE_JSON");
                std::env::remove_var("FL_HUB_MUSE_BIN");
                std::env::remove_var("FL_AGY_QUOTA_JSON");
            }
            expire_direct_quota_cache();
            let quota = read_session_quota_usage_locked("tier-session", Some(home.path()))
                .expect("embedded windows");
            assert!(quota.tier.is_none());
            assert_eq!(quota.primary_window.as_ref().unwrap().used_percent, 5.0);
            assert_eq!(
                quota.primary_window.as_ref().unwrap().window_minutes,
                Some(300)
            );
            assert_eq!(quota.secondary_window.as_ref().unwrap().used_percent, 9.0);
            let raw = fs::read_to_string(dir.join(".credentials.json")).unwrap();
            assert!(raw.contains("not-a-real-token"));
            assert!(!raw.contains("sk-ant"));
        });
    }

    #[test]
    fn sha256_digest_and_bare_quota_are_not_exhaustion() {
        let digest = "46b062aeae5a97c4e6c16eb9c078d9c5813439b429d59dc9224b1d979b381d72";
        assert!(!is_rate_text(digest));
        assert!(!is_rate_text("Please check the quota documentation"));
        assert!(!is_rate_text("aaaaaaaa-bbbb-4291-cccc-dddddddddddd"));
        assert!(is_rate_text("Subscription quota exhausted"));
        assert!(is_rate_text("HTTP 429"));
        let raw = format!(
            "{}\n{}\n",
            serde_json::json!({"type":"system","message": digest}),
            serde_json::json!({"type":"assistant","error": digest}),
        );
        assert!(claude_rate_limit_reason(&raw).is_none());
        let agy = format!(
            "{}\n",
            serde_json::json!({"type":"ERROR_MESSAGE","error": digest})
        );
        assert!(antigravity_exhausted_reason(&agy).is_none());
    }

    #[test]
    fn completed_turn_clears_an_earlier_rate_limit_reason() {
        let claude = concat!(
            r#"{"type":"assistant","error":"HTTP 429 rate limit reached"}"#,
            "\n",
            r#"{"type":"assistant","content":"the follow-up completed"}"#,
            "\n",
        );
        assert!(claude_rate_limit_reason(claude).is_none());
        let still = r#"{"type":"system","message":"HTTP 429 rate limit reached"}"#;
        assert_eq!(
            claude_rate_limit_reason(still).as_deref(),
            Some("HTTP 429 rate limit reached")
        );
        let agy = concat!(
            r#"{"type":"ERROR_MESSAGE","error":"429 RESOURCE_EXHAUSTED: quota exceeded"}"#,
            "\n",
            r#"{"type":"PLANNER_RESPONSE","content":"next step"}"#,
            "\n",
        );
        assert!(antigravity_exhausted_reason(agy).is_none());
        let agy_open =
            r#"{"type":"ERROR_MESSAGE","error":"429 RESOURCE_EXHAUSTED: quota exceeded"}"#;
        assert_eq!(
            antigravity_exhausted_reason(agy_open).as_deref(),
            Some("429 RESOURCE_EXHAUSTED: quota exceeded")
        );
    }

    const AGY_SUMMARY: &str = r#"{"response":{"groups":[
        {"displayName":"Gemini Models","buckets":[
            {"bucketId":"gemini-weekly","window":"weekly","remainingFraction":0.9739852,"resetTime":"2026-11-07T20:27:46Z"},
            {"bucketId":"gemini-5h","window":"5h","remainingFraction":0.99091005,"resetTime":"2026-11-01T06:46:15Z"}]},
        {"displayName":"Claude and GPT models","buckets":[
            {"bucketId":"3p-weekly","window":"weekly","remainingFraction":0.9975795,"resetTime":"2026-11-02T19:29:36Z"},
            {"bucketId":"3p-5h","window":"5h","remainingFraction":1,"resetTime":"2026-11-01T07:10:39Z"}]}]}}"#;
    const AGY_STATUS: &str = r#"{"userStatus":{"planStatus":{"planInfo":{"teamsTier":"TEAMS_TIER_PRO","planName":"Pro"}},
        "cascadeModelConfigData":{"defaultOverrideModelConfig":{"modelOrAlias":{"model":"MODEL_PLACEHOLDER_M418"}},
        "clientModelConfigs":[
            {"modelId":"gemini-4-argon-medium","modelOrAlias":{"model":"MODEL_PLACEHOLDER_M418"},
             "quotaInfo":{"remainingFraction":0.9811882,"resetTime":"2026-11-02T17:46:15Z"}},
            {"modelId":"claude-opus-5-5","modelOrAlias":{"model":"MODEL_PLACEHOLDER_M12"}}]}}}"#;

    fn ms(iso: &str) -> u64 {
        parse_iso8601_unix_millis(iso).unwrap()
    }

    fn agy_fixture_live() -> AgyLive {
        AgyLive {
            buckets: parse_agy_buckets(&serde_json::from_str(AGY_SUMMARY).unwrap()),
            models: parse_agy_model_quotas(&serde_json::from_str(AGY_STATUS).unwrap()),
            tier: parse_agy_tier(&serde_json::from_str(AGY_STATUS).unwrap()),
        }
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn agy_live_picks_the_bucket_group_for_the_model() {
        let live = agy_fixture_live();
        assert_eq!(live.tier.as_deref(), Some("pro"));
        assert_eq!(live.buckets.len(), 4);

        // A Claude or GPT model reads the `3p-*` buckets.
        for model in ["claude-opus-5-5", "gpt-oss-120b"] {
            let q = agy_live_usage(&live, Some(model)).unwrap();
            let (five, week) = (q.primary_window.unwrap(), q.secondary_window.unwrap());
            assert!(near(five.used_percent, 0.0), "{model}: {five:?}");
            assert_eq!(five.window_minutes, Some(300));
            assert_eq!(five.resets_at_ms, Some(ms("2026-11-01T07:10:39Z")));
            assert!(near(week.used_percent, (1.0 - 0.9975795) * 100.0));
            assert_eq!(week.window_minutes, Some(10_080));
            assert_eq!(week.resets_at_ms, Some(ms("2026-11-02T19:29:36Z")));
            assert_eq!(q.tier.as_deref(), Some("pro"));
        }
        // Any other model, and an unknown one, reads `gemini-*`.
        let week_pct = (1.0 - 0.9739852) * 100.0;
        for model in [Some("gemini-3.8-flash"), None] {
            let q = agy_live_usage(&live, model).unwrap();
            assert!(near(q.secondary_window.unwrap().used_percent, week_pct));
        }
    }

    #[test]
    fn agy_live_model_quota_replaces_the_5h_bucket_only_when_it_resets_later() {
        let live = agy_fixture_live();
        // The model resets 2026-11-02T17:46:15Z, after the 5h bucket's 2026-11-01T06:46:15Z.
        let q = agy_live_usage(&live, Some("gemini-4-argon")).unwrap();
        let five = q.primary_window.unwrap();
        assert!(
            near(five.used_percent, (1.0 - 0.9811882) * 100.0),
            "{five:?}"
        );
        assert_eq!(five.window_minutes, Some(300));
        assert_eq!(five.resets_at_ms, Some(ms("2026-11-02T17:46:15Z")));
        // The weekly window is untouched.
        assert!(near(
            q.secondary_window.unwrap().used_percent,
            (1.0 - 0.9739852) * 100.0
        ));

        // An older model reset keeps the bucket.
        let mut older = live.clone();
        older.models[0].resets_at_ms = Some(ms("2026-11-01T06:00:00Z"));
        let five = agy_live_usage(&older, Some("gemini-4-argon"))
            .unwrap()
            .primary_window
            .unwrap();
        assert_eq!(five.resets_at_ms, Some(ms("2026-11-01T06:46:15Z")));
        // A model with no quotaInfo keeps the bucket.
        let five = agy_live_usage(&live, Some("claude-opus-5-5"))
            .unwrap()
            .primary_window
            .unwrap();
        assert_eq!(five.resets_at_ms, Some(ms("2026-11-01T07:10:39Z")));
    }

    #[test]
    fn agy_live_parsers_handle_odd_shapes() {
        // protobuf JSON omits a zero fraction: a bucket with a reset time and none is empty.
        let doc: Value = serde_json::from_str(
            r#"{"groups":[{"buckets":[{"bucketId":"gemini-5h","resetTime":"2026-11-01T06:46:15Z"},
                {"bucketId":"gemini-weekly"}, {"window":"5h","remainingFraction":0.5}]}]}"#,
        )
        .unwrap();
        let buckets = parse_agy_buckets(&doc);
        assert_eq!(
            buckets.len(),
            1,
            "no id and no data are skipped: {buckets:?}"
        );
        assert_eq!(
            buckets[0].window, "5h",
            "window falls back to the id suffix"
        );
        assert!(near(buckets[0].used_percent, 100.0));
        // The fraction is clamped.
        let over: Value = serde_json::from_str(
            r#"{"groups":[{"buckets":[{"bucketId":"gemini-5h","remainingFraction":1.5},
                {"bucketId":"gemini-weekly","remainingFraction":-1}]}]}"#,
        )
        .unwrap();
        let used: Vec<f64> = parse_agy_buckets(&over)
            .iter()
            .map(|b| b.used_percent)
            .collect();
        assert_eq!(used, vec![0.0, 100.0]);
        // Tier: the plan name wins, the tier enum is the fallback, and neither is an error.
        let tier = |s: &str| parse_agy_tier(&serde_json::from_str(s).unwrap());
        assert_eq!(
            tier(r#"{"userStatus":{"planStatus":{"planInfo":{"planName":" Ultra "}}}}"#).as_deref(),
            Some("ultra")
        );
        assert_eq!(
            tier(r#"{"userStatus":{"planStatus":{"planInfo":{"teamsTier":"TEAMS_TIER_PRO"}}}}"#)
                .as_deref(),
            Some("pro")
        );
        assert_eq!(tier(r#"{"userStatus":{}}"#), None);
        assert_eq!(tier("{}"), None);
        // No data at all is no quota.
        assert_eq!(agy_live_usage(&AgyLive::default(), None), None);
    }

    struct QuotaStub {
        port: u16,
        seen: std::sync::Arc<Mutex<Vec<(String, String)>>>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: thread::JoinHandle<()>,
    }

    impl QuotaStub {
        fn calls(&self) -> Vec<(String, String)> {
            self.seen.lock().unwrap().clone()
        }
        fn shutdown(self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            self.handle.join().unwrap();
        }
    }

    /// A loopback language server answering the quota calls (and the model config call that
    /// discovery makes) with the fixtures above.
    fn spawn_quota_stub(summary: &'static str, status: &'static str) -> QuotaStub {
        use std::io::{BufRead, BufReader};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        AGY_LIVE_CACHE
            .lock()
            .unwrap()
            .retain(|(p, _), _| *p != port);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen_t, stop_t) = (Arc::clone(&seen), Arc::clone(&stop));
        let handle = thread::spawn(move || {
            while !stop_t.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                if request_line.is_empty() {
                    continue; // the listening probe connects and hangs up
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
                let mut body = vec![0u8; len];
                std::io::Read::read_exact(&mut reader, &mut body).unwrap();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let reply = if path.ends_with("/RetrieveUserQuotaSummary") {
                    summary
                } else if path.ends_with("/GetUserStatus") {
                    status
                } else if path.ends_with("/GetCascadeModelConfigData") {
                    // The status document's model block, without the `userStatus` wrapper.
                    r#"{"defaultOverrideModelConfig":{"modelOrAlias":{"model":"MODEL_PLACEHOLDER_M12"}},
                        "clientModelConfigs":[{"modelId":"claude-opus-5-5","modelOrAlias":{"model":"MODEL_PLACEHOLDER_M12"}}]}"#
                } else {
                    "{}"
                };
                seen_t.lock().unwrap().push((path, csrf));
                let mut out = stream;
                let _ = std::io::Write::write_all(
                    &mut out,
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                );
            }
        });
        QuotaStub {
            port,
            seen,
            stop,
            handle,
        }
    }

    #[test]
    fn agy_account_quota_probes_the_language_server_and_caches_for_a_minute() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let stub = spawn_quota_stub(AGY_SUMMARY, AGY_STATUS);
            let addr = format!("127.0.0.1:{}", stub.port);
            let tmp = tempfile::tempdir().unwrap();
            let home = Some(tmp.path());
            let target = AgyQuotaTarget {
                address: Some(&addr),
                csrf: Some("stub-csrf"),
                model: Some("claude-opus-5-5"),
                session: None,
            };
            expire_direct_quota_cache();
            let first = agy_account_quota(home, &target).expect("live quota");
            assert_eq!(first.tier.as_deref(), Some("pro"));
            assert!(near(
                first.secondary_window.as_ref().unwrap().used_percent,
                (1.0 - 0.9975795) * 100.0
            ));
            let calls = stub.calls();
            assert_eq!(calls.len(), 2, "{calls:?}");
            assert!(
                calls
                    .iter()
                    .any(|(p, _)| p.ends_with("/RetrieveUserQuotaSummary"))
            );
            assert!(calls.iter().any(|(p, _)| p.ends_with("/GetUserStatus")));
            assert!(calls.iter().all(|(_, csrf)| csrf == "stub-csrf"));

            // A second read, even for another model family, is served from the cache.
            let gemini = AgyQuotaTarget {
                model: Some("gemini-4-argon"),
                ..target
            };
            let second = agy_account_quota(home, &gemini).expect("cached quota");
            assert!(near(
                second.secondary_window.as_ref().unwrap().used_percent,
                (1.0 - 0.9739852) * 100.0
            ));
            assert_eq!(stub.calls().len(), 2, "no new request inside the TTL");

            // Expiry asks again.
            expire_direct_quota_cache();
            agy_account_quota(home, &target).expect("refreshed");
            assert_eq!(stub.calls().len(), 4);
            stub.shutdown();
        });
    }

    #[test]
    fn agy_account_quota_override_wins_and_a_home_override_never_reaches_the_host() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let stub = spawn_quota_stub(AGY_SUMMARY, AGY_STATUS);
            let addr = format!("127.0.0.1:{}", stub.port);
            let tmp = tempfile::tempdir().unwrap();
            let target = AgyQuotaTarget {
                address: Some(&addr),
                csrf: Some("c"),
                model: None,
                session: None,
            };

            // SAFETY: this test holds PROBE_LOCK until `_clear` drops and removes the vars.
            unsafe {
                std::env::set_var(
                    "FL_AGY_QUOTA_JSON",
                    r#"{"tier":"forced","window":{"usedPercent":7}}"#,
                );
            }
            expire_direct_quota_cache();
            let forced = agy_account_quota(Some(tmp.path()), &target).unwrap();
            assert_eq!(forced.tier.as_deref(), Some("forced"));
            assert!(
                stub.calls().is_empty(),
                "the override short-circuits the probe"
            );
            // SAFETY: as above.
            unsafe {
                std::env::remove_var("FL_AGY_QUOTA_JSON");
                std::env::set_var("ANTIGRAVITY_LS_ADDRESS", &addr);
                std::env::set_var("ANTIGRAVITY_CSRF_TOKEN", "env-csrf");
            }
            expire_direct_quota_cache();
            // A home override ignores the environment: nothing names a server.
            let none = AgyQuotaTarget::default();
            assert_eq!(agy_account_quota(Some(tmp.path()), &none), None);
            assert!(
                stub.calls().is_empty(),
                "home override must not read the env endpoint"
            );
            // Host discovery off ignores it too.
            // SAFETY: as above.
            unsafe { std::env::set_var("FL_WIRE_DISABLE_HOST_DISCOVERY", "1") };
            assert_eq!(agy_account_quota(None, &none), None);
            assert!(
                stub.calls().is_empty(),
                "disabled host discovery must not read the env endpoint"
            );
            // SAFETY: as above.
            unsafe { std::env::remove_var("FL_WIRE_DISABLE_HOST_DISCOVERY") };
            // With the host allowed, the environment fills the gap.
            let live = agy_account_quota(None, &none).expect("env endpoint");
            assert_eq!(live.tier.as_deref(), Some("pro"));
            assert!(stub.calls().iter().all(|(_, csrf)| csrf == "env-csrf"));
            stub.shutdown();
        });
    }

    #[test]
    fn agy_account_quota_dead_server_and_bad_token_are_misses() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let tmp = tempfile::tempdir().unwrap();
            let dead = AgyQuotaTarget {
                address: Some("127.0.0.1:1"),
                csrf: Some("c"),
                model: None,
                session: None,
            };
            assert_eq!(agy_account_quota(Some(tmp.path()), &dead), None);
            let stub = spawn_quota_stub(AGY_SUMMARY, AGY_STATUS);
            let addr = format!("127.0.0.1:{}", stub.port);
            for csrf in ["bad token", "a\nb", ""] {
                let bad = AgyQuotaTarget {
                    address: Some(&addr),
                    csrf: Some(csrf),
                    model: None,
                    session: None,
                };
                assert_eq!(agy_account_quota(Some(tmp.path()), &bad), None, "{csrf:?}");
            }
            assert!(stub.calls().is_empty());
            stub.shutdown();
        });
    }

    #[test]
    fn discovery_reads_live_quota_for_the_sessions_model_family() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let stub = spawn_quota_stub(AGY_SUMMARY, AGY_STATUS);
            let addr = format!("127.0.0.1:{}", stub.port);
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            crate::latency_tests::write_agy_identity(home, "agy-q-1", "ferris");
            let routes = home.join(".cache").join("agent-bus").join("routes.json");
            fs::create_dir_all(routes.parent().unwrap()).unwrap();
            fs::write(
                &routes,
                format!(
                    r#"{{"entries":{{"ferris":{{"persona":"ferris","updated_at":"2026-10-29T00:00:00.000Z",
                    "route":{{"kind":"agy","conversation_id":"agy-q-1","ls_address":"{addr}","csrf_token":"stub-csrf"}}}}}}}}"#
                ),
            )
            .unwrap();
            expire_direct_quota_cache();
            let store = fl_store::open_redb(&home.join("store.redb")).unwrap();
            let sessions = crate::discovery::discover_sessions_with_home(&store, Some(home));
            let agy = sessions
                .iter()
                .find(|s| s.persona.as_deref() == Some("ferris"))
                .expect("agy session");
            // The stub's config names claude-opus-5-5 as the default model, so `3p-*` is read.
            assert_eq!(agy.model.as_deref(), Some("claude-opus-5-5"));
            let q = agy.quota_usage.as_ref().expect("live quota on the session");
            assert_eq!(q.tier.as_deref(), Some("pro"));
            assert_eq!(
                q.primary_window.as_ref().unwrap().resets_at_ms,
                Some(ms("2026-11-01T07:10:39Z"))
            );

            // The watch entry knows only the session id: the route and the model come from
            // what discovery remembered, and the cached answer costs no request.
            let before = stub.calls().len();
            let watched = crate::discovery::quota_for_resolved_route(
                "antigravity",
                None,
                Some(home),
                "agy-q-1",
            )
            .expect("remembered route serves the watch path");
            assert_eq!(
                &watched, q,
                "claude-opus-5-5 comes from the remembered hint"
            );
            assert_eq!(stub.calls().len(), before, "no extra request while cached");

            // Another home never inherits the hint.
            let other = tempfile::tempdir().unwrap();
            assert_eq!(
                crate::discovery::quota_for_resolved_route(
                    "antigravity",
                    None,
                    Some(other.path()),
                    "agy-q-1",
                ),
                None
            );
            stub.shutdown();
        });
    }

    #[test]
    fn agy_hint_fills_gaps_and_a_missing_model_never_erases_the_remembered_one() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let stub = spawn_quota_stub(AGY_SUMMARY, AGY_STATUS);
            let addr = format!("127.0.0.1:{}", stub.port);
            let tmp = tempfile::tempdir().unwrap();
            let home = Some(tmp.path());
            let session_only = AgyQuotaTarget {
                session: Some("hint-s1"),
                ..Default::default()
            };
            // Nothing is remembered yet.
            assert_eq!(agy_account_quota(home, &session_only), None);
            let full = AgyQuotaTarget {
                address: Some(&addr),
                csrf: Some("stub-csrf"),
                model: Some("claude-opus-5-5"),
                session: Some("hint-s1"),
            };
            let claude = agy_account_quota(home, &full).expect("live quota");
            // A later read with no model keeps the remembered one (and the route).
            let no_model = AgyQuotaTarget {
                model: None,
                ..full
            };
            assert_eq!(agy_account_quota(home, &no_model), Some(claude.clone()));
            assert_eq!(agy_account_quota(home, &session_only), Some(claude.clone()));
            // A model the target names replaces the remembered one.
            let gemini = AgyQuotaTarget {
                model: Some("gemini-4-argon"),
                ..session_only
            };
            let gemini_usage = agy_account_quota(home, &gemini).expect("gemini quota");
            assert_ne!(gemini_usage, claude);
            assert_eq!(agy_account_quota(home, &session_only), Some(gemini_usage));
            // The same session id under another home has no hint.
            let other = tempfile::tempdir().unwrap();
            assert_eq!(agy_account_quota(Some(other.path()), &session_only), None);
            stub.shutdown();
        });
    }

    fn write_agy_route(home: &Path, conv: &str, endpoint: Option<(&str, &str)>) {
        crate::latency_tests::write_agy_identity(home, conv, "ferris");
        let routes = home.join(".cache").join("agent-bus").join("routes.json");
        fs::create_dir_all(routes.parent().unwrap()).unwrap();
        let ls = endpoint
            .map(|(addr, csrf)| format!(r#","ls_address":"{addr}","csrf_token":"{csrf}""#))
            .unwrap_or_default();
        fs::write(
            &routes,
            format!(
                r#"{{"entries":{{"ferris":{{"persona":"ferris","updated_at":"2026-10-29T00:00:00.000Z",
                "route":{{"kind":"agy","conversation_id":"{conv}"{ls}}}}}}}}}"#
            ),
        )
        .unwrap();
    }

    fn cold_discovery_pass(home: &Path) {
        let snap = crate::discovery::DiscoveryStoreSnapshot::default();
        let found = crate::discovery::discover_route_sessions_without_model_info(&snap, Some(home));
        assert!(found.iter().any(|s| s.persona.as_deref() == Some("ferris")));
    }

    #[test]
    fn cold_watch_discovery_remembers_the_route_endpoint_without_model_info() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let stub = spawn_quota_stub(AGY_SUMMARY, AGY_STATUS);
            let addr = format!("127.0.0.1:{}", stub.port);
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            write_agy_route(home, "agy-cold-1", Some((&addr, "stub-csrf")));
            expire_direct_quota_cache();
            // No `discover_sessions_with_home` first: only the id-lookup discovery the watch
            // summary runs, which makes no quota read of its own.
            cold_discovery_pass(home);
            assert!(stub.calls().is_empty(), "the lookup pass reads no quota");
            let q = crate::discovery::quota_for_resolved_route(
                "antigravity",
                None,
                Some(home),
                "agy-cold-1",
            )
            .expect("the remembered route serves the cold watch read");
            // The model is unknown on this path, so the group defaults to `gemini-*`.
            assert_eq!(q.tier.as_deref(), Some("pro"));
            assert_eq!(
                q.primary_window.as_ref().unwrap().resets_at_ms,
                Some(ms("2026-11-01T06:46:15Z"))
            );
            assert!(
                stub.calls().iter().all(|(_, csrf)| csrf == "stub-csrf"),
                "{:?}",
                stub.calls()
            );
            assert!(!stub.calls().is_empty());
            stub.shutdown();
        });
    }

    #[test]
    fn cold_by_id_transcript_read_uses_the_saved_route() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let stub = spawn_quota_stub(AGY_SUMMARY, AGY_STATUS);
            let addr = format!("127.0.0.1:{}", stub.port);
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let conv = "agy-cold-2";
            let logs = home
                .join(".gemini")
                .join("antigravity-cli")
                .join("brain")
                .join(conv)
                .join(".system_generated")
                .join("logs");
            fs::create_dir_all(&logs).unwrap();
            fs::write(
                logs.join("transcript.jsonl"),
                format!(
                    "{}\n",
                    serde_json::json!({"type":"ERROR_MESSAGE","error":"disk full"})
                ),
            )
            .unwrap();
            write_agy_route(home, conv, Some((&addr, "stub-csrf")));
            expire_direct_quota_cache();
            // Nothing ran a discovery pass: the by-id read finds the route itself.
            let q = crate::discovery::read_session_quota_usage_locked(conv, Some(home))
                .expect("live quota from the route");
            assert_eq!(q.tier.as_deref(), Some("pro"));
            assert_eq!(
                q.primary_window.as_ref().unwrap().resets_at_ms,
                Some(ms("2026-11-01T06:46:15Z"))
            );
            assert_eq!(q.exhausted_reason, None);
            assert!(!stub.calls().is_empty());
            assert!(stub.calls().iter().all(|(_, csrf)| csrf == "stub-csrf"));

            // Without a route nothing is known about the endpoint, as before.
            let bare = tempfile::tempdir().unwrap();
            let logs = bare
                .path()
                .join(".gemini")
                .join("antigravity-cli")
                .join("brain")
                .join(conv)
                .join(".system_generated")
                .join("logs");
            fs::create_dir_all(&logs).unwrap();
            fs::write(logs.join("transcript.jsonl"), "{}\n").unwrap();
            assert_eq!(
                crate::discovery::read_session_quota_usage_locked(conv, Some(bare.path())),
                None
            );
            stub.shutdown();
        });
    }

    #[test]
    fn restarted_language_server_replaces_the_endpoint_and_keeps_the_model() {
        with_probe_lock(|| {
            let _clear = ClearProbeEnv;
            let old = spawn_quota_stub(AGY_SUMMARY, AGY_STATUS);
            let new = spawn_quota_stub(AGY_SUMMARY, AGY_STATUS);
            let old_addr = format!("127.0.0.1:{}", old.port);
            let new_addr = format!("127.0.0.1:{}", new.port);
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let conv = "agy-restart-1";
            write_agy_route(home, conv, Some((&old_addr, "csrf-old")));
            expire_direct_quota_cache();
            cold_discovery_pass(home);
            // A full pass elsewhere learned the model of this conversation.
            agy_resolve_hint(
                Some(home),
                &AgyQuotaTarget {
                    model: Some("claude-opus-5-5"),
                    session: Some(conv),
                    ..Default::default()
                },
            );
            let watch = || {
                expire_direct_quota_cache();
                crate::discovery::quota_for_resolved_route("antigravity", None, Some(home), conv)
            };
            let claude_5h = Some(ms("2026-11-01T07:10:39Z"));
            let first = watch().expect("old endpoint");
            assert_eq!(
                first.primary_window.as_ref().unwrap().resets_at_ms,
                claude_5h
            );
            assert!(!old.calls().is_empty() && new.calls().is_empty());

            // The language server restarted: new address and token in the route table.
            write_agy_route(home, conv, Some((&new_addr, "csrf-new")));
            cold_discovery_pass(home);
            let old_before = old.calls().len();
            let second = watch().expect("new endpoint");
            assert_eq!(second, first, "the remembered model is kept");
            assert_eq!(old.calls().len(), old_before, "the old stub is not asked");
            assert!(!new.calls().is_empty());
            assert!(new.calls().iter().all(|(_, csrf)| csrf == "csrf-new"));

            // A route that names no endpoint never wipes the stored one.
            write_agy_route(home, conv, None);
            cold_discovery_pass(home);
            let new_before = new.calls().len();
            let third = watch().expect("endpoint survives an empty route");
            assert_eq!(third, first);
            assert!(new.calls().len() > new_before);
            assert_eq!(old.calls().len(), old_before);
            old.shutdown();
            new.shutdown();
        });
    }

    #[test]
    fn agy_hint_map_is_bounded() {
        with_probe_lock(|| {
            let tmp = tempfile::tempdir().unwrap();
            for i in 0..(AGY_HINT_MAX + 5) {
                let id = format!("bound-{i}");
                agy_resolve_hint(
                    Some(tmp.path()),
                    &AgyQuotaTarget {
                        model: Some("m"),
                        session: Some(&id),
                        ..Default::default()
                    },
                );
            }
            let len = AGY_HINTS.lock().unwrap().len();
            assert!(len <= AGY_HINT_MAX, "{len}");
        });
    }

    #[test]
    fn agy_live_miss_expires_sooner_than_a_hit() {
        with_probe_lock(|| {
            let port = 1u16; // never connected: the cache is read directly
            let aged = |secs: u64, value: Option<AgyLive>| {
                let at = Instant::now() - Duration::from_secs(secs);
                AGY_LIVE_CACHE
                    .lock()
                    .unwrap()
                    .insert((port, "c".to_string()), (at, value));
            };
            aged(11, None);
            assert!(
                !agy_live_cache_has(port, "c"),
                "a miss older than 10s is retried"
            );
            aged(5, None);
            assert!(agy_live_cache_has(port, "c"), "a young miss is served");
            aged(11, Some(AgyLive::default()));
            assert!(agy_live_cache_has(port, "c"), "a hit lives 60s");
            aged(61, Some(AgyLive::default()));
            assert!(!agy_live_cache_has(port, "c"));
            // The token is part of the key: a rejected token does not poison the port.
            aged(5, None);
            assert!(!agy_live_cache_has(port, "other-csrf"));
            AGY_LIVE_CACHE.lock().unwrap().clear();
        });
    }
}
