//! Shared process harness, pre/post git snapshot delta, and wrapper liveness emitter.

use crate::runner::{AttemptOutcome, AttemptSpec};
use fl_core::at::At;
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::log::AttemptStatus;
use fl_core::telemetry::{AdapterKind, AttemptHeartbeat, LivenessStatus, RateCard};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

pub const EXCERPT_LIMIT: usize = 8192;
const SPAWN_RETRY_LIMIT: u32 = 20;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(2);
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// How often a running attempt's spend is compared with its ceiling.
const BUDGET_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Decides which ticks of the supervision loop also send a heartbeat. It counts ticks instead of
/// comparing wall-clock time: the loop ticks every `check_every`, and a clock comparison taken
/// just after the previous heartbeat always came out a hair under the interval, so every second
/// check was skipped and heartbeats arrived at twice the configured interval.
struct HeartbeatPacer {
    ticks_per_beat: u32,
    ticks: u32,
}

impl HeartbeatPacer {
    fn new(heartbeat_interval: Duration, check_every: Duration) -> Self {
        let ratio = heartbeat_interval
            .as_nanos()
            .div_ceil(check_every.as_nanos().max(1));
        Self {
            ticks_per_beat: u32::try_from(ratio).unwrap_or(u32::MAX).max(1),
            ticks: 0,
        }
    }

    /// Call once per tick; true when this tick should send a heartbeat.
    fn tick(&mut self) -> bool {
        self.ticks += 1;
        if self.ticks >= self.ticks_per_beat {
            self.ticks = 0;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedOutput {
    pub excerpt_text: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub vendor_cost_usd_micros: Option<u64>,
}

pub trait CliDialect: Send + Sync {
    fn kind(&self) -> AdapterKind;
    fn default_binary(&self) -> &'static str;
    fn argv(&self, spec: &AttemptSpec) -> Vec<String>;
    fn parse_output(&self, stdout: &[u8], stderr: &[u8]) -> ParsedOutput;
}

pub type LivenessSink = Arc<dyn Fn(&AttemptHeartbeat) + Send + Sync>;

pub struct ProcessHarness<D: CliDialect> {
    pub binary: String,
    pub dialect: D,
    pub rate_card: Option<RateCard>,
    pub liveness_sink: Option<LivenessSink>,
    pub attempt_id: Option<Iri>,
    pub project: Option<ProjectId>,
    pub agent: Option<String>,
    pub host: String,
    pub heartbeat_interval: Duration,
}

impl<D: CliDialect> ProcessHarness<D> {
    pub fn new(binary: String, dialect: D) -> Self {
        Self {
            binary,
            dialect,
            rate_card: None,
            liveness_sink: None,
            attempt_id: None,
            project: None,
            agent: None,
            host: detect_host(),
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
        }
    }

    pub fn with_rate_card(mut self, rate_card: Option<RateCard>) -> Self {
        self.rate_card = rate_card;
        self
    }

    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    pub fn with_liveness(
        mut self,
        attempt_id: Iri,
        project: ProjectId,
        agent: Option<String>,
        sink: LivenessSink,
    ) -> Self {
        self.attempt_id = Some(attempt_id);
        self.project = Some(project);
        self.agent = agent;
        self.liveness_sink = Some(sink);
        self
    }

    pub async fn run(&self, spec: AttemptSpec) -> AttemptOutcome {
        let attempt_id = self.attempt_id.clone().unwrap_or_else(|| {
            Iri::parse(&format!("urn:uuid:{}", Uuid::now_v7()))
                .expect("uuid v7 forms a valid urn:uuid IRI")
        });
        let fallback_project = ProjectId(
            Iri::parse("urn:uuid:00000000-0000-4000-8000-000000000000")
                .expect("static urn is valid"),
        );
        let project = self.project.clone().unwrap_or(fallback_project);

        let emit = |hb: &AttemptHeartbeat| {
            if let Some(sink) = &self.liveness_sink {
                sink(hb);
            }
        };

        let started_wall = now_at();
        let mut hb = AttemptHeartbeat {
            attempt_id,
            project,
            record: spec.record.clone(),
            adapter: self.dialect.kind().as_wire().to_owned(),
            agent: self.agent.clone(),
            host: self.host.clone(),
            pid: 0,
            started_at: started_wall.clone(),
            heartbeat_at: started_wall,
            status: LivenessStatus::Live,
            outcome: None,
            exit_at: None,
            duration_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd_micros: 0,
        };

        if spec.budget_usd_micros == 0 {
            let now = now_at();
            hb.heartbeat_at = now.clone();
            hb.exit_at = Some(now);
            hb.status = LivenessStatus::Exited;
            hb.outcome = Some(AttemptStatus::Refused);
            emit(&hb);
            return AttemptOutcome::refused(
                "the attempt's spend ceiling is zero, so nothing was started",
            );
        }

        let snap_before = GitSnapshot::capture(&spec.project_root).await;
        let started = Instant::now();

        let mut cmd = Command::new(&self.binary);
        cmd.args(self.dialect.argv(&spec))
            .current_dir(&spec.project_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so a timeout or a spend-ceiling kill reaches the tools the
        // agent started (a build, a test run) and not just the agent's own process.
        cmd.process_group(0);

        let mut retries = 0u32;
        let mut child = loop {
            match cmd.spawn() {
                Ok(c) => break c,
                Err(e)
                    if e.kind() == ErrorKind::ExecutableFileBusy && retries < SPAWN_RETRY_LIMIT =>
                {
                    retries += 1;
                    tokio::time::sleep(SPAWN_RETRY_DELAY).await;
                }
                Err(e) => {
                    let now = now_at();
                    hb.heartbeat_at = now.clone();
                    hb.exit_at = Some(now);
                    hb.status = LivenessStatus::Exited;
                    hb.outcome = Some(AttemptStatus::Refused);
                    emit(&hb);
                    return AttemptOutcome::refused(format!(
                        "could not start `{}`: {e}",
                        self.binary
                    ));
                }
            }
        };

        hb.pid = child.id().unwrap_or(0);
        hb.heartbeat_at = now_at();
        emit(&hb);

        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");

        let stdout_buf = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let stderr_buf = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));

        let stdout_buf_writer = Arc::clone(&stdout_buf);
        let stdout_task = tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            loop {
                match read_pipe(&mut stdout_pipe, &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut guard) = stdout_buf_writer.lock() {
                            guard.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
        });

        let stderr_buf_writer = Arc::clone(&stderr_buf);
        let stderr_task = tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            loop {
                match read_pipe(&mut stderr_pipe, &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut guard) = stderr_buf_writer.lock() {
                            guard.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
        });

        let limit = Duration::from_secs(spec.timeout_secs.max(1));
        let deadline = tokio::time::Instant::now() + limit;
        // Spend is checked at least once a second even when heartbeats are sent less often: a
        // ceiling noticed only every few seconds is a ceiling that can be overshot by minutes.
        let check_every = self.heartbeat_interval.min(BUDGET_CHECK_INTERVAL);
        let mut interval = tokio::time::interval(check_every);
        interval.tick().await; // consume immediate first tick
        let mut pacer = HeartbeatPacer::new(self.heartbeat_interval, check_every);
        let mut over_budget = false;

        let wait_result = loop {
            tokio::select! {
                res = child.wait() => {
                    break Some(res);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    break None;
                }
                _ = interval.tick() => {
                    let (so, se) = (
                        stdout_buf.lock().map(|g| g.clone()).unwrap_or_default(),
                        stderr_buf.lock().map(|g| g.clone()).unwrap_or_default(),
                    );
                    let partial = self.dialect.parse_output(&so, &se);
                    let partial_cost = self.rate_card.unwrap_or_default().resolve_cost(
                        partial.tokens_in,
                        partial.tokens_out,
                        partial.vendor_cost_usd_micros,
                    );
                    if pacer.tick() {
                        hb.heartbeat_at = now_at();
                        hb.duration_ms = started.elapsed().as_millis() as u64;
                        hb.tokens_in = partial.tokens_in;
                        hb.tokens_out = partial.tokens_out;
                        hb.cost_usd_micros = partial_cost;
                        emit(&hb);
                    }
                    if partial_cost > spec.budget_usd_micros {
                        over_budget = true;
                        break None;
                    }
                }
            }
        };

        if wait_result.is_none() {
            if let Some(pid) = child.id() {
                kill_process_group(pid).await;
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        let _ = timeout(Duration::from_millis(200), stdout_task).await;
        let _ = timeout(Duration::from_millis(200), stderr_task).await;

        let stdout_bytes = stdout_buf.lock().map(|g| g.clone()).unwrap_or_default();
        let stderr_bytes = stderr_buf.lock().map(|g| g.clone()).unwrap_or_default();

        let parsed = self.dialect.parse_output(&stdout_bytes, &stderr_bytes);
        let cost_usd_micros = self.rate_card.unwrap_or_default().resolve_cost(
            parsed.tokens_in,
            parsed.tokens_out,
            parsed.vendor_cost_usd_micros,
        );
        let paths_touched = match snap_before {
            Some(before) => before.diff_after(&spec.project_root).await,
            None => Vec::new(),
        };

        let (status, raw_excerpt) = match wait_result {
            None => {
                let (status, prefix) = if over_budget {
                    (
                        AttemptStatus::Crashed,
                        format!(
                            "`{}` was stopped: it passed its spend ceiling of {} micro-USD",
                            self.binary, spec.budget_usd_micros
                        ),
                    )
                } else {
                    (
                        AttemptStatus::Timeout,
                        format!(
                            "`{}` exceeded its {}s timeout",
                            self.binary, spec.timeout_secs
                        ),
                    )
                };
                let excerpt = if parsed.excerpt_text.trim().is_empty() {
                    prefix
                } else {
                    format!("{prefix}\n{}", parsed.excerpt_text)
                };
                (status, excerpt)
            }
            Some(Err(e)) => (AttemptStatus::Crashed, e.to_string()),
            Some(Ok(exit_status)) => {
                let st = if exit_status.success() {
                    AttemptStatus::Completed
                } else {
                    AttemptStatus::Crashed
                };
                (st, parsed.excerpt_text)
            }
        };

        let output_excerpt = truncate_utf8(raw_excerpt, EXCERPT_LIMIT);

        let now = now_at();
        hb.heartbeat_at = now.clone();
        hb.exit_at = Some(now);
        hb.status = LivenessStatus::Exited;
        hb.outcome = Some(status);
        hb.duration_ms = duration_ms;
        hb.tokens_in = parsed.tokens_in;
        hb.tokens_out = parsed.tokens_out;
        hb.cost_usd_micros = cost_usd_micros;
        emit(&hb);

        AttemptOutcome {
            status,
            output_excerpt,
            duration_ms,
            tokens_in: parsed.tokens_in,
            tokens_out: parsed.tokens_out,
            cost_usd_micros,
            paths_touched,
        }
    }
}

/// SIGKILL the process group `pgid`. `kill` is the shell's built-in, so this needs no `kill`
/// binary and no libc binding. Failure is ignored: the direct child is killed separately.
async fn kill_process_group(pgid: u32) {
    let _ = Command::new("sh")
        .args(["-c", &format!("kill -KILL -{pgid} 2>/dev/null")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

fn read_pipe<R: AsyncRead + Unpin>(
    pipe: &mut R,
    buf: &mut [u8],
) -> impl std::future::Future<Output = std::io::Result<usize>> {
    std::future::poll_fn(|cx| {
        let mut rb = ReadBuf::new(buf);
        match std::pin::Pin::new(&mut *pipe).poll_read(cx, &mut rb) {
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(rb.filled().len())),
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
}

pub fn truncate_utf8(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

pub fn now_at() -> At {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    At::from_unix_millis(millis)
}

pub fn detect_host() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.trim().is_empty()
    {
        return h.trim().to_owned();
    }
    if let Ok(h) = std::fs::read_to_string("/etc/hostname")
        && !h.trim().is_empty()
    {
        return h.trim().to_owned();
    }
    "localhost".to_owned()
}

/// Converts a dollar `f64` value into integer microdollars (`1 USD = 1_000_000`).
pub fn usd_f64_to_micros(usd: f64) -> Option<u64> {
    if !usd.is_finite() || usd < 0.0 {
        return None;
    }
    Some((usd * 1_000_000.0).round() as u64)
}

/// Extracts usage and cost fields from a JSON object using standard vendor shapes.
pub fn extract_usage_from_value(
    v: &serde_json::Value,
    tokens_in: &mut u64,
    tokens_out: &mut u64,
    vendor_cost_micros: &mut Option<u64>,
    accumulate_tokens: bool,
) {
    if let Some(usage) = v.get("usage").or_else(|| v.get("stats")) {
        let base_in = usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .or_else(|| usage.get("tokens_in"))
            .and_then(|n| n.as_u64());
        let cache_create = usage
            .get("cache_creation_input_tokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let in_tok = base_in
            .map(|b| b.saturating_add(cache_create).saturating_add(cache_read))
            .or_else(|| {
                (cache_create > 0 || cache_read > 0)
                    .then(|| cache_create.saturating_add(cache_read))
            });
        let out_tok = usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .or_else(|| usage.get("tokens_out"))
            .and_then(|n| n.as_u64());
        if let Some(i) = in_tok {
            if accumulate_tokens {
                *tokens_in = tokens_in.saturating_add(i);
            } else {
                *tokens_in = i;
            }
        }
        if let Some(o) = out_tok {
            if accumulate_tokens {
                *tokens_out = tokens_out.saturating_add(o);
            } else {
                *tokens_out = o;
            }
        }
        if let Some(c) = usage.get("cost_usd_micros").and_then(|n| n.as_u64()) {
            *vendor_cost_micros = Some(c);
        } else if let Some(c) = usage
            .get("cost_usd")
            .or_else(|| usage.get("total_cost_usd"))
            .and_then(|n| n.as_f64())
            .and_then(usd_f64_to_micros)
        {
            *vendor_cost_micros = Some(c);
        }
    }

    if let Some(c) = v.get("cost_usd_micros").and_then(|n| n.as_u64()) {
        *vendor_cost_micros = Some(c);
    } else if let Some(c) = v
        .get("total_cost_usd")
        .or_else(|| v.get("cost_usd"))
        .and_then(|n| n.as_f64())
        .and_then(usd_f64_to_micros)
    {
        *vendor_cost_micros = Some(c);
    }
}

/// Parses a single JSON object or JSONL stream for standard CLI envelopes (`claude`, `agy`, `muse`, `grok`),
/// falling back to raw stdout + stderr if no JSON object can be parsed.
pub fn parse_json_or_jsonl_envelope(
    stdout: &[u8],
    stderr: &[u8],
    accumulate_jsonl_tokens: bool,
) -> ParsedOutput {
    let stdout_str = String::from_utf8_lossy(stdout);
    let stderr_str = String::from_utf8_lossy(stderr);
    let trimmed = stdout_str.trim();

    if trimmed.is_empty() {
        return ParsedOutput {
            excerpt_text: stderr_str.into_owned(),
            tokens_in: 0,
            tokens_out: 0,
            vendor_cost_usd_micros: None,
        };
    }

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
        && val.is_object()
    {
        let mut tokens_in = 0u64;
        let mut tokens_out = 0u64;
        let mut vendor_cost = None;
        extract_usage_from_value(
            &val,
            &mut tokens_in,
            &mut tokens_out,
            &mut vendor_cost,
            false,
        );
        let text =
            extract_text_from_object(&val).unwrap_or_else(|| stdout_str.clone().into_owned());
        let excerpt_text = append_stderr(text, &stderr_str);
        return ParsedOutput {
            excerpt_text,
            tokens_in,
            tokens_out,
            vendor_cost_usd_micros: vendor_cost,
        };
    }

    let mut any_json_obj = false;
    let mut tokens_in = 0u64;
    let mut tokens_out = 0u64;
    let mut vendor_cost = None;
    let mut text_parts: Vec<String> = Vec::new();

    for line in trimmed.lines() {
        let line_trim = line.trim();
        if line_trim.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line_trim)
            && val.is_object()
        {
            any_json_obj = true;
            extract_usage_from_value(
                &val,
                &mut tokens_in,
                &mut tokens_out,
                &mut vendor_cost,
                accumulate_jsonl_tokens,
            );
            if let Some(t) = extract_text_from_object(&val)
                && !t.is_empty()
            {
                text_parts.push(t);
            }
        }
    }

    if any_json_obj {
        let base_text = if text_parts.is_empty() {
            stdout_str.into_owned()
        } else {
            text_parts.join("\n")
        };
        return ParsedOutput {
            excerpt_text: append_stderr(base_text, &stderr_str),
            tokens_in,
            tokens_out,
            vendor_cost_usd_micros: vendor_cost,
        };
    }

    let mut raw = stdout_str.into_owned();
    raw.push_str(&stderr_str);
    ParsedOutput {
        excerpt_text: raw,
        tokens_in: 0,
        tokens_out: 0,
        vendor_cost_usd_micros: None,
    }
}

pub fn extract_text_from_object(val: &serde_json::Value) -> Option<String> {
    for key in ["result", "response", "output", "text", "content", "message"] {
        if let Some(s) = val.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_owned());
        }
    }
    if let Some(msg) = val.get("message") {
        if let Some(s) = msg.get("content").and_then(|v| v.as_str()) {
            return Some(s.to_owned());
        }
        if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
            let parts: Vec<&str> = arr
                .iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect();
            if !parts.is_empty() {
                return Some(parts.join("\n"));
            }
        }
    }
    if let Some(choices) = val.get("choices").and_then(|v| v.as_array())
        && let Some(first) = choices.first()
        && let Some(msg) = first.get("message")
        && let Some(s) = msg.get("content").and_then(|v| v.as_str())
    {
        return Some(s.to_owned());
    }
    if let Some(item) = val.get("item")
        && let Some(s) = item.get("text").and_then(|v| v.as_str())
    {
        return Some(s.to_owned());
    }
    None
}

fn append_stderr(mut base: String, stderr: &str) -> String {
    if !stderr.is_empty() {
        if !base.is_empty() && !base.ends_with('\n') {
            base.push('\n');
        }
        base.push_str(stderr);
    }
    base
}

fn is_ignored_fl_path(path: &str) -> bool {
    path == ".fl" || path.starts_with(".fl/")
}

#[derive(Debug, Clone)]
pub struct GitSnapshot {
    head: Option<String>,
    dirty: BTreeMap<String, Option<[u8; 32]>>,
}

impl GitSnapshot {
    pub async fn capture(root: &Path) -> Option<Self> {
        let status_out = Command::new("git")
            .args(["status", "--porcelain=v1", "-z", "-uall"])
            .current_dir(root)
            .output()
            .await
            .ok()?;
        if !status_out.status.success() {
            return None;
        }

        let head = Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(root)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty());

        let dirty = parse_porcelain_z_and_hash(root, &status_out.stdout);
        Some(Self { head, dirty })
    }

    pub async fn diff_after(&self, root: &Path) -> Vec<String> {
        let Some(after) = Self::capture(root).await else {
            return Vec::new();
        };
        let mut touched = BTreeSet::new();

        for (path, after_digest) in &after.dirty {
            if is_ignored_fl_path(path) {
                continue;
            }
            match self.dirty.get(path) {
                None => {
                    touched.insert(path.clone());
                }
                Some(before_digest) if before_digest != after_digest => {
                    touched.insert(path.clone());
                }
                _ => {}
            }
        }

        if let (Some(h_before), Some(h_after)) = (&self.head, &after.head)
            && h_before != h_after
            && let Ok(diff_out) = Command::new("git")
                .args(["diff", "--name-only", "-z", h_before, h_after])
                .current_dir(root)
                .output()
                .await
            && diff_out.status.success()
        {
            for entry in diff_out.stdout.split(|b| *b == 0) {
                if !entry.is_empty() {
                    let p = String::from_utf8_lossy(entry).into_owned();
                    if !is_ignored_fl_path(&p) {
                        touched.insert(p);
                    }
                }
            }
        } else if self.head.is_none()
            && let Some(h_after) = &after.head
            && let Ok(tree_out) = Command::new("git")
                .args(["ls-tree", "-r", "--name-only", "-z", h_after])
                .current_dir(root)
                .output()
                .await
            && tree_out.status.success()
        {
            for entry in tree_out.stdout.split(|b| *b == 0) {
                if !entry.is_empty() {
                    let p = String::from_utf8_lossy(entry).into_owned();
                    if !is_ignored_fl_path(&p) {
                        touched.insert(p);
                    }
                }
            }
        }

        for (path, before_digest) in &self.dirty {
            if is_ignored_fl_path(path) {
                continue;
            }
            if !after.dirty.contains_key(path) {
                let current_digest = hash_worktree_path(&root.join(path));
                if &current_digest != before_digest {
                    touched.insert(path.clone());
                }
            }
        }

        touched.into_iter().collect()
    }
}

fn parse_porcelain_z_and_hash(root: &Path, bytes: &[u8]) -> BTreeMap<String, Option<[u8; 32]>> {
    let mut map = BTreeMap::new();
    let mut fields = bytes.split(|b| *b == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let xy = &field[0..2];
        let path_bytes = &field[3..];
        let path = String::from_utf8_lossy(path_bytes).into_owned();
        if !is_ignored_fl_path(&path) {
            let digest = hash_worktree_path(&root.join(&path));
            map.insert(path, digest);
        }
        if (xy[0] == b'R' || xy[0] == b'C' || xy[1] == b'R' || xy[1] == b'C')
            && let Some(orig_bytes) = fields.next()
            && !orig_bytes.is_empty()
        {
            let orig_path = String::from_utf8_lossy(orig_bytes).into_owned();
            if !is_ignored_fl_path(&orig_path) {
                let orig_digest = hash_worktree_path(&root.join(&orig_path));
                map.insert(orig_path, orig_digest);
            }
        }
    }
    map
}

fn hash_worktree_path(full_path: &Path) -> Option<[u8; 32]> {
    let meta = std::fs::symlink_metadata(full_path).ok()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(full_path).ok()?;
        let mut hasher = Sha256::new();
        hasher.update(b"symlink:");
        hasher.update(target.to_string_lossy().as_bytes());
        return Some(hasher.finalize().into());
    }
    if meta.is_file() {
        // Streamed: an untracked build artifact can be gigabytes, and `fs::read` would hold the
        // whole file in memory (twice, before and after the attempt).
        use std::io::Read;
        let mut file = std::fs::File::open(full_path).ok()?;
        let mut hasher = Sha256::new();
        hasher.update(b"file:");
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            hasher.update(&chunk[..n]);
        }
        return Some(hasher.finalize().into());
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn heartbeats_fire_every_interval_not_every_second_one() {
        // Default shape: heartbeat every 5 s, supervision tick every 1 s.
        let mut pacer = HeartbeatPacer::new(Duration::from_secs(5), Duration::from_secs(1));
        let beats: Vec<u32> = (1..=20).filter(|_| pacer.tick()).collect();
        assert_eq!(beats.len(), 4, "one heartbeat per 5 ticks over 20 ticks");
        let mut pacer = HeartbeatPacer::new(Duration::from_secs(5), Duration::from_secs(1));
        let at: Vec<u32> = (1..=20).filter(|_| pacer.tick()).collect();
        assert_eq!(at, vec![5, 10, 15, 20]);
        // Interval at or below the check period: every tick beats.
        let mut fast = HeartbeatPacer::new(Duration::from_millis(50), Duration::from_millis(50));
        assert!((0..10).all(|_| fast.tick()));
        // Not a whole multiple: rounds up, never beats early.
        let mut odd = HeartbeatPacer::new(Duration::from_millis(1500), Duration::from_secs(1));
        assert_eq!(
            (1..=4).map(|_| odd.tick()).collect::<Vec<_>>(),
            [false, true, false, true]
        );
    }

    use super::*;
    use fl_core::ids::{RecordId, seq_iri};
    use std::sync::Mutex;

    fn run_git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("git command should run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn count_loose_objects(git_objects_dir: &Path) -> usize {
        walkdir::WalkDir::new(git_objects_dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .count()
    }

    #[tokio::test]
    async fn git_snapshot_tracks_dirty_untracked_and_committed_changes_without_writing_git_objects()
    {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        run_git(root, &["init"]);

        std::fs::write(root.join("tracked_untouched.txt"), "initial-1\n").unwrap();
        std::fs::write(root.join("tracked_modified.txt"), "initial-2\n").unwrap();
        std::fs::write(root.join("to_commit.txt"), "initial-3\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "initial"]);

        // Make both tracked files dirty BEFORE capturing the pre-snapshot, plus an untracked file.
        std::fs::write(root.join("tracked_untouched.txt"), "dirty-before\n").unwrap();
        std::fs::write(root.join("tracked_modified.txt"), "dirty-before\n").unwrap();
        std::fs::write(root.join("pre_untracked.txt"), "untracked-before\n").unwrap();

        let objects_dir = root.join(".git/objects");
        let objects_before = count_loose_objects(&objects_dir);

        let snap = GitSnapshot::capture(root).await.expect("git repo snapshot");
        let objects_after_capture = count_loose_objects(&objects_dir);
        assert_eq!(
            objects_before, objects_after_capture,
            "capturing untracked files must not write loose objects into .git/objects"
        );

        // During the attempt:
        // 1. tracked_untouched.txt and pre_untracked.txt remain untouched.
        // 2. tracked_modified.txt is modified further.
        std::fs::write(root.join("tracked_modified.txt"), "dirty-after\n").unwrap();
        // 3. A new untracked file is created.
        std::fs::write(root.join("new_untracked.txt"), "hello\n").unwrap();
        // 4. An ignored .fl/ file is created.
        std::fs::create_dir_all(root.join(".fl")).unwrap();
        std::fs::write(root.join(".fl/state.json"), "{}\n").unwrap();
        // 5. A change is committed (advancing HEAD).
        std::fs::write(root.join("to_commit.txt"), "committed-after\n").unwrap();
        run_git(root, &["add", "to_commit.txt"]);
        run_git(root, &["commit", "-m", "advance head"]);

        let touched = snap.diff_after(root).await;
        assert_eq!(
            touched,
            vec![
                "new_untracked.txt".to_owned(),
                "to_commit.txt".to_owned(),
                "tracked_modified.txt".to_owned(),
            ]
        );
    }

    struct StubDialect;

    impl CliDialect for StubDialect {
        fn kind(&self) -> AdapterKind {
            AdapterKind::Claude
        }

        fn default_binary(&self) -> &'static str {
            "claude"
        }

        fn argv(&self, _spec: &AttemptSpec) -> Vec<String> {
            Vec::new()
        }

        fn parse_output(&self, stdout: &[u8], stderr: &[u8]) -> ParsedOutput {
            parse_json_or_jsonl_envelope(stdout, stderr, true)
        }
    }

    #[tokio::test]
    async fn process_harness_emits_live_and_exited_heartbeats_and_preserves_partial_tokens_on_timeout()
     {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("emit_then_hang.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' '{\"text\":\"working\",\"usage\":{\"input_tokens\":75,\"output_tokens\":25}}'\nsleep 30\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let events: Arc<Mutex<Vec<AttemptHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
        let events_sink = Arc::clone(&events);
        let sink: LivenessSink = Arc::new(move |hb: &AttemptHeartbeat| {
            events_sink.lock().unwrap().push(hb.clone());
        });

        let harness = ProcessHarness::new(script.to_string_lossy().into_owned(), StubDialect)
            .with_rate_card(Some(RateCard {
                input_usd_micros_per_mtok: 2_000_000,
                output_usd_micros_per_mtok: 4_000_000,
            }))
            .with_heartbeat_interval(Duration::from_millis(50))
            .with_liveness(
                seq_iri(10),
                ProjectId(seq_iri(20)),
                Some("ferris".into()),
                sink,
            );

        let outcome = harness
            .run(AttemptSpec {
                project_root: tmp.path().to_path_buf(),
                record: RecordId(seq_iri(30)),
                instruction: "test".into(),
                timeout_secs: 1,
                budget_usd_micros: 1_000_000,
            })
            .await;

        assert_eq!(outcome.status, AttemptStatus::Timeout);
        assert_eq!(outcome.tokens_in, 75);
        assert_eq!(outcome.tokens_out, 25);
        // 75 * 2 + 25 * 4 = 250 micro-USD
        assert_eq!(outcome.cost_usd_micros, 250);
        assert!(outcome.output_excerpt.contains("exceeded its 1s timeout"));
        assert!(outcome.output_excerpt.contains("working"));

        let recorded = events.lock().unwrap();
        assert!(
            recorded.len() >= 2,
            "expected at least Live and Exited heartbeats"
        );
        assert_eq!(recorded.first().unwrap().status, LivenessStatus::Live);
        assert!(recorded.first().unwrap().pid > 0);
        let last = recorded.last().unwrap();
        assert_eq!(last.status, LivenessStatus::Exited);
        assert_eq!(last.outcome, Some(AttemptStatus::Timeout));
        assert_eq!(last.tokens_in, 75);
        assert_eq!(last.tokens_out, 25);
        assert_eq!(last.cost_usd_micros, 250);
        assert!(last.exit_at.is_some());
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn priced_harness(bin: String) -> ProcessHarness<StubDialect> {
        ProcessHarness::new(bin, StubDialect)
            .with_rate_card(Some(RateCard {
                input_usd_micros_per_mtok: 2_000_000,
                output_usd_micros_per_mtok: 4_000_000,
            }))
            .with_heartbeat_interval(Duration::from_secs(60))
    }

    fn spec_in(dir: &Path, timeout_secs: u64, budget_usd_micros: u64) -> AttemptSpec {
        AttemptSpec {
            project_root: dir.to_path_buf(),
            record: RecordId(seq_iri(30)),
            instruction: "test".into(),
            timeout_secs,
            budget_usd_micros,
        }
    }

    /// Audit H-1: `budget_usd_micros` was only compared with zero. An attempt that had already
    /// spent past its ceiling ran on until it finished or timed out.
    #[tokio::test]
    async fn an_attempt_that_passes_its_spend_ceiling_is_stopped_not_left_to_finish() {
        let tmp = tempfile::tempdir().unwrap();
        // 75 * 2 + 25 * 4 = 250 micro-USD, then a 30 s hang.
        let bin = write_script(
            tmp.path(),
            "spend_then_hang.sh",
            "#!/bin/sh\nprintf '%s\\n' '{\"text\":\"spent\",\"usage\":{\"input_tokens\":75,\"output_tokens\":25}}'\nsleep 30\n",
        );
        let started = Instant::now();
        // The harness is configured with a 60 s heartbeat, so only the budget check can stop it.
        let outcome = priced_harness(bin).run(spec_in(tmp.path(), 25, 100)).await;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "ran {:?} past a spend ceiling",
            started.elapsed()
        );
        assert_eq!(outcome.status, AttemptStatus::Crashed);
        assert_eq!(outcome.cost_usd_micros, 250);
        assert!(
            outcome.output_excerpt.contains("spend ceiling of 100"),
            "{}",
            outcome.output_excerpt
        );
        assert!(outcome.output_excerpt.contains("spent"));
    }

    #[tokio::test]
    async fn spending_exactly_the_ceiling_is_allowed_and_a_zero_ceiling_starts_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = write_script(
            tmp.path(),
            "spend_exactly.sh",
            "#!/bin/sh\nprintf '%s\\n' '{\"text\":\"ok\",\"usage\":{\"input_tokens\":75,\"output_tokens\":25}}'\n",
        );
        let ok = priced_harness(bin.clone())
            .run(spec_in(tmp.path(), 10, 250))
            .await;
        assert_eq!(ok.status, AttemptStatus::Completed);
        assert_eq!(ok.cost_usd_micros, 250);
        let refused = priced_harness(bin).run(spec_in(tmp.path(), 10, 0)).await;
        assert_eq!(refused.status, AttemptStatus::Refused);
        assert_eq!(refused.cost_usd_micros, 0);
    }

    #[tokio::test]
    async fn a_missing_binary_is_refused_and_a_nonzero_exit_is_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = priced_harness("/nonexistent/fl-audit-agent".into())
            .run(spec_in(tmp.path(), 5, 1_000))
            .await;
        assert_eq!(missing.status, AttemptStatus::Refused);
        assert!(missing.output_excerpt.contains("could not start"));
        let bin = write_script(tmp.path(), "fail.sh", "#!/bin/sh\necho boom >&2\nexit 3\n");
        let crashed = priced_harness(bin).run(spec_in(tmp.path(), 5, 1_000)).await;
        assert_eq!(crashed.status, AttemptStatus::Crashed);
    }

    /// Audit H-2: only the agent's own process was killed at the timeout, so a build or test run
    /// it had started kept going (and kept spending) after the attempt was reported over.
    #[tokio::test]
    async fn a_timeout_also_kills_what_the_agent_started() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("orphan-survived");
        let bin = write_script(
            tmp.path(),
            "spawn_orphan.sh",
            &format!(
                "#!/bin/sh\n(sleep 2; touch '{}') &\nsleep 30\n",
                marker.display()
            ),
        );
        let outcome = priced_harness(bin).run(spec_in(tmp.path(), 1, 1_000)).await;
        assert_eq!(outcome.status, AttemptStatus::Timeout);
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            !marker.exists(),
            "a process the agent started outlived the attempt"
        );
    }

    /// Audit H-3: a dirty file was read into memory whole to be hashed; the digest must stay what
    /// it was (`sha256("file:" + bytes)`) while being computed in constant memory.
    #[test]
    fn worktree_hash_streams_large_files_and_keeps_its_digest_format() {
        let tmp = tempfile::tempdir().unwrap();
        let big = tmp.path().join("big.bin");
        let bytes: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&big, &bytes).unwrap();
        let mut want = Sha256::new();
        want.update(b"file:");
        want.update(&bytes);
        assert_eq!(hash_worktree_path(&big), Some(want.finalize().into()));

        let empty = tmp.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        let mut want = Sha256::new();
        want.update(b"file:");
        assert_eq!(hash_worktree_path(&empty), Some(want.finalize().into()));

        // Missing paths and directories have no digest; a symlink hashes its target text.
        assert_eq!(hash_worktree_path(&tmp.path().join("nope")), None);
        assert_eq!(hash_worktree_path(tmp.path()), None);
        let link_a = tmp.path().join("a");
        let link_b = tmp.path().join("b");
        std::os::unix::fs::symlink("target-one", &link_a).unwrap();
        std::os::unix::fs::symlink("target-two", &link_b).unwrap();
        assert!(hash_worktree_path(&link_a).is_some());
        assert_ne!(hash_worktree_path(&link_a), hash_worktree_path(&link_b));
    }

    #[tokio::test]
    async fn snapshot_delta_sees_a_revert_to_head_a_deletion_a_rename_and_ignores_fl_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        run_git(root, &["init", "-q"]);
        std::fs::write(root.join("kept.txt"), "head\n").unwrap();
        std::fs::write(root.join("gone.txt"), "bye\n").unwrap();
        std::fs::write(root.join("old name.txt"), "rename me\n").unwrap();
        std::fs::write(root.join("untouched.txt"), "same\n").unwrap();
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-qm", "init"]);
        // Already dirty before the attempt.
        std::fs::write(root.join("kept.txt"), "dirty before\n").unwrap();

        let before = GitSnapshot::capture(root).await.unwrap();
        // The agent reverts the pre-existing edit, deletes one file and renames another.
        std::fs::write(root.join("kept.txt"), "head\n").unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::rename(root.join("old name.txt"), root.join("new name.txt")).unwrap();
        run_git(root, &["add", "-A"]);
        std::fs::create_dir_all(root.join(".fl")).unwrap();
        std::fs::write(root.join(".fl/state.json"), "{}").unwrap();

        let touched = before.diff_after(root).await;
        assert_eq!(
            touched,
            vec![
                "gone.txt".to_owned(),
                "kept.txt".to_owned(),
                "new name.txt".to_owned(),
                "old name.txt".to_owned(),
            ]
        );
    }
}
