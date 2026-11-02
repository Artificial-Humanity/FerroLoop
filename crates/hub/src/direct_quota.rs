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

/// Mark every cached probe stale so the next read runs the command again.
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
        let raw = command_stdout(&bin, &["usage", "--json"], PROBE_TIMEOUT)?;
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
        let raw = command_stdout(path, &["auth", "status", "--json"], PROBE_TIMEOUT)?;
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

pub(crate) fn agy_account_quota(home_override: Option<&Path>) -> Option<SessionQuotaUsage> {
    let raw = std::env::var("FL_AGY_QUOTA_JSON").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let key = format!("agy:{}", home_key(home_override));
    let raw = raw.to_string();
    cached(ProbeKind::Agy, &key, move || {
        let value = load_json_override(&raw)?;
        agy_allowance(&value)
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

fn command_stdout(bin: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
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
}
