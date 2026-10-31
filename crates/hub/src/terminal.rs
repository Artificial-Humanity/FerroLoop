//! Owner-scoped interactive terminal process manager for `/ws/v1` (`channel = "terminal"`, spec §4.2).
//!
//! Resource limits (every one is per WebSocket connection):
//!
//! * Output is a bounded queue ([`OUTPUT_QUEUE_CAPACITY`] frames). The reader tasks `send().await`
//!   into it, so a slow client makes them stop reading the child's pipe; the pipe fills and the
//!   child blocks. Nothing is dropped and hub memory stays bounded.
//! * The connection loop is also the queue's only consumer, so it must never wait on the queue.
//!   Every reply to a client frame (`opened`, `resized`, `error`) is therefore *returned* from
//!   [`TerminalManager::handle_frame`] and written by the caller, never queued.
//! * The final `exited` frame is the one frame that must not be lost or wait behind a full
//!   queue (a `close` waits for it). A queue slot is reserved for it when the session opens
//!   (`try_reserve_owned`), so sending it never blocks and never fails while the client is
//!   connected. An `open` that finds no free slot is refused.
//! * A write to the child's stdin is bounded by [`STDIN_WRITE_TIMEOUT`].
//! * At most [`MAX_SESSIONS_PER_CONNECTION`] live sessions; `session_id` is bounded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::HubWsFrame;
use crate::auth::HubAuthority;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Frames the output queue of one connection holds before the reader tasks wait for the
/// WebSocket writer. A frame is at most 4096 bytes of text, so this bounds the queue to about
/// 1 MiB (plus JSON overhead) however fast the child writes.
pub const OUTPUT_QUEUE_CAPACITY: usize = 256;

/// How long one write of client input to the child's stdin may take. A child that stopped
/// reading fills the pipe (64 KiB on Linux) and the write would wait forever.
const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Live terminal sessions one connection may hold. A session that has exited does not count.
const MAX_SESSIONS_PER_CONNECTION: usize = 8;

/// Longest `session_id` a client may choose.
const MAX_SESSION_ID_BYTES: usize = 64;

/// How long, after the child has exited, the reader tasks get to reach end of file before the
/// `exited` frame is sent anyway. A background job the shell left behind (`sleep 600 &`) keeps
/// the stdout/stderr pipes open, so without this bound the exit would go unreported (and a
/// close or disconnect would hang) until that job ended. The wait is for *no progress*: it
/// restarts while the readers keep delivering frames, so a slow client that drains a large
/// backlog still gets all the output before `exited`.
const READER_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Turns a byte stream read in arbitrary chunks into text without damaging characters: a
/// multi-byte character split across two reads is held back and completed by the next one,
/// where decoding each chunk alone would turn both halves into U+FFFD. Bytes that are invalid
/// in themselves still become U+FFFD.
#[derive(Default)]
struct Utf8Chunker {
    carry: Vec<u8>,
}

impl Utf8Chunker {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.carry.extend_from_slice(bytes);
        let mut out = String::new();
        let mut pos = 0;
        loop {
            match std::str::from_utf8(&self.carry[pos..]) {
                Ok(valid) => {
                    out.push_str(valid);
                    pos = self.carry.len();
                    break;
                }
                Err(err) => {
                    let valid_end = pos + err.valid_up_to();
                    out.push_str(&String::from_utf8_lossy(&self.carry[pos..valid_end]));
                    match err.error_len() {
                        Some(bad) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            pos = valid_end + bad;
                        }
                        // An incomplete character at the end: wait for the rest of it.
                        None => {
                            pos = valid_end;
                            break;
                        }
                    }
                }
            }
        }
        self.carry.drain(..pos);
        out
    }

    /// The held-back tail at end of stream, lossily decoded.
    fn finish(&mut self) -> String {
        let tail = String::from_utf8_lossy(&self.carry).into_owned();
        self.carry.clear();
        tail
    }
}
/// Active child process session managed for a single WebSocket connection.
pub struct TerminalSession {
    pub session_id: String,
    pub pid: Option<u32>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    stdin: Option<ChildStdin>,
    kill_tx: Option<oneshot::Sender<()>>,
    wait_task: Option<JoinHandle<()>>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    /// Set just before the `exited` frame is queued, so a client that has seen the frame can
    /// reopen at once without the session still counting against the cap.
    exited: Arc<AtomicBool>,
}

impl TerminalSession {
    /// Terminate the child process and wait briefly for background I/O tasks to complete.
    pub async fn shutdown(mut self) {
        self.stdin.take();
        if let Some(tx) = self.kill_tx.take() {
            let _ = tx.send(());
        }
        if let Some(wait_task) = self.wait_task.take() {
            let _ = wait_task.await;
        }
        self.stdout_task.abort();
        self.stderr_task.abort();
    }

    /// The child has exited and its `exited` frame is queued (or about to be).
    fn is_finished(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(tx) = self.kill_tx.take() {
            let _ = tx.send(());
        }
        self.stdout_task.abort();
        self.stderr_task.abort();
    }
}

fn terminal_frame(event: &str, data: Value) -> HubWsFrame {
    HubWsFrame {
        channel: "terminal".to_string(),
        event: event.to_string(),
        data,
    }
}

fn error_frame(status: u16, session_id: Option<&str>, message: impl Into<String>) -> HubWsFrame {
    let mut data = json!({ "status": status, "message": message.into() });
    if let Some(id) = session_id {
        data["session_id"] = json!(id);
    }
    terminal_frame("error", data)
}

/// The client's `session_id`, trimmed. `None` when absent or blank (the manager then picks one).
fn clean_session_id(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(id) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if id.len() > MAX_SESSION_ID_BYTES {
        return Err(format!(
            "session_id is longer than {MAX_SESSION_ID_BYTES} bytes"
        ));
    }
    if id.chars().any(char::is_control) {
        return Err("session_id contains a control character".to_string());
    }
    Ok(Some(id.to_string()))
}

/// Per-WebSocket manager of active interactive terminal sessions (spec §4.2).
pub struct TerminalManager {
    sessions: BTreeMap<String, TerminalSession>,
    out_tx: mpsc::Sender<HubWsFrame>,
    next_id: u64,
}

impl TerminalManager {
    /// `out_tx` is the connection's output queue; create it with
    /// `mpsc::channel(OUTPUT_QUEUE_CAPACITY)`.
    pub fn new(out_tx: mpsc::Sender<HubWsFrame>) -> Self {
        Self {
            sessions: BTreeMap::new(),
            out_tx,
            next_id: 1,
        }
    }

    /// Sessions whose child is still running.
    pub fn active_count(&self) -> usize {
        self.sessions.values().filter(|s| !s.is_finished()).count()
    }

    /// Terminate all spawned child processes when the owning WebSocket disconnects.
    pub async fn shutdown_all(&mut self) {
        let keys: Vec<String> = self.sessions.keys().cloned().collect();
        for key in keys {
            if let Some(sess) = self.sessions.remove(&key) {
                sess.shutdown().await;
            }
        }
    }

    /// Forget sessions whose child has exited, so they no longer count against the cap.
    fn reap_finished(&mut self) {
        self.sessions.retain(|_, s| !s.is_finished());
    }

    /// Handle an incoming `channel == "terminal"` frame from a WebSocket client. The returned
    /// frame, if any, is the direct reply; the caller writes it (see the module comment for why
    /// it is not queued). Output and `exited` frames arrive on the output queue.
    pub async fn handle_frame(
        &mut self,
        authority: &HubAuthority,
        enable_terminal: bool,
        default_cwd: &Path,
        event: &str,
        data: &Value,
    ) -> Option<HubWsFrame> {
        if !authority.is_owner() || !enable_terminal {
            return Some(error_frame(
                403,
                None,
                "owner authority required for terminal sessions",
            ));
        }

        match event {
            "open" => Some(self.handle_open(default_cwd, data).await),
            "input" => self.handle_input(data).await,
            "resize" => Some(self.handle_resize(data)),
            "close" => self.handle_close(data).await,
            other => Some(error_frame(
                400,
                None,
                format!("unknown terminal event `{other}`"),
            )),
        }
    }

    async fn handle_open(&mut self, default_cwd: &Path, data: &Value) -> HubWsFrame {
        #[derive(Debug, Deserialize, Default)]
        struct OpenPayload {
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            cwd: Option<String>,
            #[serde(default)]
            cols: Option<u16>,
            #[serde(default)]
            rows: Option<u16>,
            #[serde(default)]
            command: Option<Value>,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            env: BTreeMap<String, String>,
        }

        let payload: OpenPayload = match serde_json::from_value(data.clone()) {
            Ok(p) => p,
            Err(e) => {
                return error_frame(400, None, format!("invalid terminal open payload: {e}"));
            }
        };

        let session_id = match clean_session_id(payload.session_id.as_deref()) {
            Ok(Some(id)) => id,
            Ok(None) => {
                let id = format!("term-{}", self.next_id);
                self.next_id += 1;
                id
            }
            Err(message) => return error_frame(400, None, message),
        };

        // Reopening an id replaces its session, so that one does not count against the cap.
        self.reap_finished();
        let others = self.sessions.len() - usize::from(self.sessions.contains_key(&session_id));
        if others >= MAX_SESSIONS_PER_CONNECTION {
            return error_frame(
                429,
                Some(&session_id),
                format!(
                    "too many terminal sessions on this connection (limit {MAX_SESSIONS_PER_CONNECTION})"
                ),
            );
        }

        let cwd = match payload
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => {
                let candidate = Path::new(raw);
                if candidate.is_absolute() {
                    candidate.to_path_buf()
                } else {
                    default_cwd.join(candidate)
                }
            }
            None => default_cwd.to_path_buf(),
        };

        if !cwd.is_dir() {
            return error_frame(
                400,
                Some(&session_id),
                format!("working directory `{}` does not exist", cwd.display()),
            );
        }

        let (program, args) = resolve_command(payload.command.as_ref(), &payload.args);
        let cols = payload.cols.unwrap_or(DEFAULT_COLS).max(1);
        let rows = payload.rows.unwrap_or(DEFAULT_ROWS).max(1);

        // The queue slot for this session's `exited` frame (see the module comment). Taken
        // without waiting: the connection loop is the queue's consumer, so it cannot wait here.
        let exit_permit = match self.out_tx.clone().try_reserve_owned() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(_)) => {
                return error_frame(
                    503,
                    Some(&session_id),
                    "terminal output backlog is full; retry once the client has caught up",
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return error_frame(500, Some(&session_id), "terminal output channel is closed");
            }
        };

        // If an older session with the same ID exists, shut it down cleanly before replacing it.
        if let Some(prev) = self.sessions.remove(&session_id) {
            prev.shutdown().await;
        }

        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .current_dir(&cwd)
            .env("TERM", "xterm-256color")
            .env("COLUMNS", cols.to_string())
            .env("LINES", rows.to_string())
            .envs(&payload.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return error_frame(
                    500,
                    Some(&session_id),
                    format!("failed to spawn `{program}`: {e}"),
                );
            }
        };

        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Frames the readers have delivered (the drain grace restarts while this moves), and
        // the signal a reader raises when the output queue is closed because the client is gone.
        let progress = Arc::new(AtomicU64::new(0));
        let client_gone = Arc::new(Notify::new());

        let (stdout_done_tx, stdout_done_rx) = oneshot::channel::<()>();
        let stdout_task = tokio::spawn(pump_output(
            stdout,
            "stdout",
            session_id.clone(),
            self.out_tx.clone(),
            progress.clone(),
            client_gone.clone(),
            stdout_done_tx,
        ));
        let (stderr_done_tx, stderr_done_rx) = oneshot::channel::<()>();
        let stderr_task = tokio::spawn(pump_output(
            stderr,
            "stderr",
            session_id.clone(),
            self.out_tx.clone(),
            progress.clone(),
            client_gone.clone(),
            stderr_done_tx,
        ));

        let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
        let sid_wait = session_id.clone();
        let exited = Arc::new(AtomicBool::new(false));
        let exited_flag = exited.clone();
        let wait_task = tokio::spawn(async move {
            let status_res = tokio::select! {
                res = child.wait() => res,
                _ = &mut kill_rx => {
                    let _ = child.start_kill();
                    child.wait().await
                }
                // The client disconnected: nobody can read the output, so stop the child
                // (a child that ignores SIGPIPE would otherwise run on, unread).
                _ = client_gone.notified() => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            // Normally both readers hit end of file at once. A background job that inherited
            // the pipes can hold them open, so the wait is bounded (see `READER_DRAIN_GRACE`).
            let drained = async {
                let _ = stdout_done_rx.await;
                let _ = stderr_done_rx.await;
            };
            tokio::pin!(drained);
            let mut seen = progress.load(Ordering::Relaxed);
            loop {
                if tokio::time::timeout(READER_DRAIN_GRACE, &mut drained)
                    .await
                    .is_ok()
                {
                    break;
                }
                let now = progress.load(Ordering::Relaxed);
                if now == seen {
                    break;
                }
                seen = now;
            }
            let exit_code = status_res.ok().and_then(|s| s.code()).unwrap_or(-1);
            exited_flag.store(true, Ordering::Release);
            // Into the slot reserved at open: never waits, and is lost only if the client is gone.
            exit_permit.send(terminal_frame(
                "exited",
                json!({
                    "session_id": sid_wait,
                    "exit_code": exit_code,
                }),
            ));
        });

        self.sessions.insert(
            session_id.clone(),
            TerminalSession {
                session_id: session_id.clone(),
                pid,
                cwd: cwd.clone(),
                cols,
                rows,
                stdin,
                kill_tx: Some(kill_tx),
                wait_task: Some(wait_task),
                stdout_task,
                stderr_task,
                exited,
            },
        );

        // The caller writes this before it next drains the output queue, so `opened` always
        // precedes the session's output and exit frames.
        terminal_frame(
            "opened",
            json!({
                "session_id": session_id,
                "pid": pid,
                "cwd": cwd.display().to_string(),
                "cols": cols,
                "rows": rows,
            }),
        )
    }

    fn resolve_session_key(&self, requested: Option<&str>) -> Option<String> {
        if let Some(id) = requested.map(str::trim).filter(|s| !s.is_empty()) {
            if self.sessions.contains_key(id) {
                return Some(id.to_string());
            }
            return None;
        }
        if self.sessions.len() == 1 {
            return self.sessions.keys().next().cloned();
        }
        None
    }

    /// Write client input to the child's stdin.
    ///
    /// The write is bounded by [`STDIN_WRITE_TIMEOUT`] so a child that stopped reading cannot
    /// stall the connection loop (and with it every other session's `close`). On a timeout the
    /// session is closed: part of the input may already be in the pipe, so keeping the session
    /// would feed the child a corrupted stream. The client gets a 504 error frame, then the
    /// session's `exited` frame.
    async fn handle_input(&mut self, data: &Value) -> Option<HubWsFrame> {
        #[derive(Debug, Deserialize, Default)]
        struct InputPayload {
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default, alias = "input", alias = "text")]
            data: String,
        }

        let payload: InputPayload = match serde_json::from_value(data.clone()) {
            Ok(p) => p,
            Err(e) => {
                return Some(error_frame(
                    400,
                    None,
                    format!("invalid terminal input payload: {e}"),
                ));
            }
        };

        let Some(key) = self.resolve_session_key(payload.session_id.as_deref()) else {
            return Some(error_frame(404, None, "terminal session not found"));
        };

        let stdin = self.sessions.get_mut(&key)?.stdin.as_mut()?;
        let written = tokio::time::timeout(STDIN_WRITE_TIMEOUT, async {
            stdin.write_all(payload.data.as_bytes()).await?;
            stdin.flush().await
        })
        .await;
        match written {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(error_frame(
                500,
                Some(&key),
                format!("failed writing to terminal stdin: {e}"),
            )),
            Err(_elapsed) => {
                if let Some(sess) = self.sessions.remove(&key) {
                    sess.shutdown().await;
                }
                Some(error_frame(
                    504,
                    Some(&key),
                    "writing to terminal stdin timed out (the process is not reading); session closed",
                ))
            }
        }
    }

    fn handle_resize(&mut self, data: &Value) -> HubWsFrame {
        #[derive(Debug, Deserialize, Default)]
        struct ResizePayload {
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            cols: Option<u16>,
            #[serde(default)]
            rows: Option<u16>,
        }

        let payload: ResizePayload = match serde_json::from_value(data.clone()) {
            Ok(p) => p,
            Err(e) => {
                return error_frame(400, None, format!("invalid terminal resize payload: {e}"));
            }
        };

        let Some(sess) = self
            .resolve_session_key(payload.session_id.as_deref())
            .and_then(|key| self.sessions.get_mut(&key))
        else {
            return error_frame(404, None, "terminal session not found");
        };

        if let Some(c) = payload.cols.filter(|&c| c > 0) {
            sess.cols = c;
        }
        if let Some(r) = payload.rows.filter(|&r| r > 0) {
            sess.rows = r;
        }
        terminal_frame(
            "resized",
            json!({
                "session_id": sess.session_id,
                "cols": sess.cols,
                "rows": sess.rows,
            }),
        )
    }

    async fn handle_close(&mut self, data: &Value) -> Option<HubWsFrame> {
        #[derive(Debug, Deserialize, Default)]
        struct ClosePayload {
            #[serde(default)]
            session_id: Option<String>,
        }

        let payload: ClosePayload = serde_json::from_value(data.clone()).unwrap_or_default();
        let Some(key) = self.resolve_session_key(payload.session_id.as_deref()) else {
            return Some(error_frame(404, None, "terminal session not found"));
        };

        if let Some(sess) = self.sessions.remove(&key) {
            sess.shutdown().await;
        }
        None
    }
}

/// Read one of the child's output pipes and queue its text as `output` frames until end of file.
///
/// `send().await` is the backpressure: while the queue is full this task stops reading, the
/// pipe fills and the child blocks. A failed send means the queue's receiver is gone (the
/// client disconnected): raise `client_gone` so the child is stopped, and stop reading.
/// `done` is signalled on every way out.
async fn pump_output(
    reader: Option<impl AsyncRead + Unpin>,
    stream: &'static str,
    session_id: String,
    out_tx: mpsc::Sender<HubWsFrame>,
    progress: Arc<AtomicU64>,
    client_gone: Arc<Notify>,
    done: oneshot::Sender<()>,
) {
    if let Some(mut reader) = reader {
        let mut buf = [0u8; 4096];
        let mut chunker = Utf8Chunker::default();
        loop {
            let (text, eof) = match reader.read(&mut buf).await {
                Ok(n) if n > 0 => (chunker.push(&buf[..n]), false),
                _ => (chunker.finish(), true),
            };
            let text = if stream == "stderr" {
                filter_bash_startup_noise(&text)
            } else {
                text
            };
            if !text.is_empty() {
                let frame = terminal_frame(
                    "output",
                    json!({
                        "session_id": session_id,
                        "stream": stream,
                        "data": text,
                        "output": text,
                    }),
                );
                if out_tx.send(frame).await.is_err() {
                    client_gone.notify_one();
                    break;
                }
                progress.fetch_add(1, Ordering::Relaxed);
            }
            if eof {
                break;
            }
        }
    }
    let _ = done.send(());
}

fn resolve_command(command: Option<&Value>, extra_args: &[String]) -> (String, Vec<String>) {
    match command {
        Some(Value::Array(items)) => {
            let mut parsed: Vec<String> = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            if parsed.is_empty() {
                ("/bin/sh".to_string(), extra_args.to_vec())
            } else {
                let prog = parsed.remove(0);
                parsed.extend_from_slice(extra_args);
                (prog, parsed)
            }
        }
        Some(Value::String(raw)) if !raw.trim().is_empty() => {
            let trimmed = raw.trim();
            if extra_args.is_empty() && trimmed.contains(char::is_whitespace) {
                (
                    "/bin/sh".to_string(),
                    vec!["-c".to_string(), trimmed.to_string()],
                )
            } else {
                (trimmed.to_string(), extra_args.to_vec())
            }
        }
        _ => ("/bin/sh".to_string(), extra_args.to_vec()),
    }
}

/// Drop the job-control warnings an interactive `bash -i` prints on stderr at startup.
///
/// The terminal spawns the shell with piped (non-PTY) stdio, so bash cannot take a terminal
/// process group and prints two warnings that mean nothing to the operator. Every other line,
/// including one that merely mentions a job, passes through unchanged.
fn filter_bash_startup_noise(text: &str) -> String {
    const NOISE: [&str; 2] = [
        "cannot set terminal process group",
        "no job control in this shell",
    ];
    text.split_inclusive('\n')
        .filter(|line| !NOISE.iter().any(|needle| line.contains(needle)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn bash_startup_noise_lines_are_dropped() {
        let noise = "bash: cannot set terminal process group (-1): Inappropriate ioctl for device\nbash: no job control in this shell\n";
        assert_eq!(filter_bash_startup_noise(noise), "");
        assert_eq!(
            filter_bash_startup_noise("bash: no job control in this shell\n"),
            ""
        );
    }

    #[test]
    fn ordinary_stderr_and_job_mentions_are_kept() {
        let text = "ls: cannot access 'x': No such file or directory\n";
        assert_eq!(filter_bash_startup_noise(text), text);
        let job = "[1]+  Done   sleep 1 (job finished)\n";
        assert_eq!(filter_bash_startup_noise(job), job);
        let mixed = "bash: no job control in this shell\nreal error\n";
        assert_eq!(filter_bash_startup_noise(mixed), "real error\n");
        assert_eq!(filter_bash_startup_noise("no newline"), "no newline");
    }

    #[test]
    fn chunker_joins_a_character_split_across_reads() {
        // "é" is C3 A9, "€" is E2 82 AC, "🦀" is F0 9F A6 80: split each at every byte.
        for text in ["é", "€", "🦀", "a🦀b€é"] {
            let bytes = text.as_bytes();
            for cut in 0..=bytes.len() {
                let mut chunker = Utf8Chunker::default();
                let mut got = chunker.push(&bytes[..cut]);
                got.push_str(&chunker.push(&bytes[cut..]));
                got.push_str(&chunker.finish());
                assert_eq!(got, text, "cut at {cut}");
            }
        }
    }

    #[test]
    fn chunker_holds_back_only_an_incomplete_tail() {
        let mut chunker = Utf8Chunker::default();
        assert_eq!(chunker.push(b"ab\xE2\x82"), "ab");
        assert_eq!(chunker.push(b"\xAC!"), "€!");
        assert_eq!(chunker.finish(), "");
    }

    #[test]
    fn chunker_replaces_invalid_bytes_and_a_truncated_tail_at_eof() {
        let mut chunker = Utf8Chunker::default();
        // 0xFF is never valid; a lone lead byte followed by ASCII is invalid too.
        assert_eq!(chunker.push(b"a\xFFb\xC3c"), "a\u{FFFD}b\u{FFFD}c");
        assert_eq!(chunker.push(b"x\xF0\x9F"), "x");
        assert_eq!(chunker.finish(), "\u{FFFD}");
        assert_eq!(chunker.finish(), "");
    }

    #[test]
    fn chunker_keeps_a_four_kib_stream_of_multibyte_text_intact() {
        // 4096 is the reader's buffer size; 4095 bytes of ASCII then "é" straddles it.
        let text = format!("{}é{}", "a".repeat(4095), "z");
        let bytes = text.as_bytes();
        let mut chunker = Utf8Chunker::default();
        let mut got = chunker.push(&bytes[..4096]);
        got.push_str(&chunker.push(&bytes[4096..]));
        assert_eq!(got, text);
        assert!(!got.contains('\u{FFFD}'));
    }

    fn owner() -> HubAuthority {
        HubAuthority::Owner {
            login: "owner".into(),
        }
    }

    /// Open `script` under `/bin/sh -c` and return the direct reply (`opened` or an error).
    async fn open_shell(mgr: &mut TerminalManager, id: &str, script: &str) -> HubWsFrame {
        send(
            mgr,
            "open",
            json!({ "session_id": id, "command": ["/bin/sh", "-c", script] }),
        )
        .await
        .expect("open always replies")
    }

    async fn send(mgr: &mut TerminalManager, event: &str, data: Value) -> Option<HubWsFrame> {
        let cwd = std::env::temp_dir();
        mgr.handle_frame(&owner(), true, &cwd, event, &data).await
    }

    /// Frames of `event` for `id`, waiting up to `limit` for the first one.
    async fn wait_for(
        rx: &mut mpsc::Receiver<HubWsFrame>,
        event: &str,
        limit: Duration,
    ) -> Option<HubWsFrame> {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Some(frame)) if frame.event == event => return Some(frame),
                Ok(Some(_)) => {}
                _ => return None,
            }
        }
    }

    #[tokio::test]
    async fn exit_is_reported_even_when_a_background_job_holds_the_pipes() {
        let (tx, mut rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        // The shell exits at once; `sleep` inherits stdout/stderr and lives on for 8 s.
        open_shell(&mut mgr, "t1", "sleep 8 & exit 3").await;
        let exited = wait_for(&mut rx, "exited", Duration::from_secs(5))
            .await
            .expect("`exited` must not wait for the background job");
        assert_eq!(exited.data["exit_code"], 3);
        assert_eq!(exited.data["session_id"], "t1");
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn close_does_not_hang_on_a_pipe_held_by_a_background_job() {
        let (tx, rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        open_shell(&mut mgr, "t2", "sleep 8 & echo ready; wait").await;
        // Kill the shell only once it has started the background job that holds the pipes.
        let mut rx = rx;
        let ready = wait_for(&mut rx, "output", Duration::from_secs(5)).await;
        assert!(ready.is_some_and(|f| f.data["data"].as_str().unwrap().contains("ready")));
        let started = Instant::now();
        let done = tokio::time::timeout(Duration::from_secs(5), mgr.shutdown_all()).await;
        assert!(
            done.is_ok(),
            "shutdown_all hung for {:?}",
            started.elapsed()
        );
        assert_eq!(mgr.active_count(), 0);
    }

    #[tokio::test]
    async fn terminal_output_keeps_multibyte_text_whole() {
        let (tx, mut rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        // 4095 ASCII bytes then "é" (2 bytes) puts the character across the 4096-byte read.
        open_shell(
            &mut mgr,
            "t3",
            "head -c 4095 /dev/zero | tr '\\0' a; printf '\\303\\251 end'",
        )
        .await;
        let mut text = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let Some(frame) = wait_for(&mut rx, "output", Duration::from_secs(1)).await else {
                break;
            };
            text.push_str(frame.data["data"].as_str().unwrap());
            if text.ends_with("end") {
                break;
            }
        }
        assert!(
            text.ends_with("\u{e9} end"),
            "got tail {:?}",
            &text[text.len().saturating_sub(12)..]
        );
        assert!(!text.contains('\u{FFFD}'));
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn terminal_requires_owner_authority_and_the_feature_flag() {
        let (tx, mut rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        let cwd = std::env::temp_dir();
        let peer = HubAuthority::Peer { agent: None };
        let replies = [
            mgr.handle_frame(&peer, true, &cwd, "open", &json!({}))
                .await,
            mgr.handle_frame(&owner(), false, &cwd, "open", &json!({}))
                .await,
        ];
        for reply in replies {
            let frame = reply.expect("a refusal is a direct reply");
            assert_eq!(frame.event, "error");
            assert_eq!(frame.data["status"], 403);
        }
        assert!(rx.try_recv().is_err(), "replies are not queued");
        assert_eq!(mgr.active_count(), 0);
    }

    /// Poll `cond` every 10 ms for up to `limit`.
    async fn eventually(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cond()
    }

    /// The pid is no longer in `/proc` (the manager reaps its child, so no zombie lingers).
    async fn process_gone(pid: u32) -> bool {
        let path = format!("/proc/{pid}");
        eventually(Duration::from_secs(10), || !Path::new(&path).exists()).await
    }

    fn pid_of(opened: &HubWsFrame) -> u32 {
        assert_eq!(opened.event, "opened", "{:?}", opened.data);
        opened.data["pid"].as_u64().expect("pid") as u32
    }

    /// Wait until the output queue is (nearly) full: one slot is held for `exited`.
    async fn wait_until_full(rx: &Receiver) {
        assert!(
            eventually(Duration::from_secs(15), || {
                rx.len() + 2 >= OUTPUT_QUEUE_CAPACITY
            })
            .await,
            "the queue never filled (len {})",
            rx.len()
        );
    }

    type Receiver = mpsc::Receiver<HubWsFrame>;

    #[tokio::test]
    async fn a_flooding_child_is_held_back_by_the_bounded_queue_and_resumes_on_drain() {
        const { assert!(OUTPUT_QUEUE_CAPACITY <= 1024, "the bound must stay small") };
        let (tx, mut rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        pid_of(&open_shell(&mut mgr, "flood", "yes").await);

        // Nobody reads: the queue fills and then stops growing, however fast `yes` writes.
        wait_until_full(&rx).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let held = rx.len();
        assert!(held <= OUTPUT_QUEUE_CAPACITY, "queue holds {held} frames");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(rx.len(), held, "the reader must be blocked, not producing");

        // Draining unblocks the reader and the child: the queue refills.
        for _ in 0..100 {
            rx.recv().await.expect("queued frame");
        }
        wait_until_full(&rx).await;
        assert!(rx.len() <= OUTPUT_QUEUE_CAPACITY);

        drop(rx);
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn backpressure_loses_no_output_and_keeps_its_order() {
        let (tx, mut rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        // About 2 MiB, well over what the queue and the pipe hold together.
        pid_of(&open_shell(&mut mgr, "count", "exec seq 1 300000").await);
        wait_until_full(&rx).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let expected: String = (1..=300_000).map(|n| format!("{n}\n")).collect();
        let mut got = String::new();
        let mut exited = None;
        while exited.is_none() {
            let frame = tokio::time::timeout(Duration::from_secs(20), rx.recv())
                .await
                .expect("the stream stalled")
                .expect("queue open");
            match frame.event.as_str() {
                "output" => got.push_str(frame.data["data"].as_str().unwrap()),
                "exited" => exited = Some(frame),
                other => panic!("unexpected `{other}` frame"),
            }
        }
        assert_eq!(exited.unwrap().data["exit_code"], 0);
        assert!(got == expected, "output differs ({} bytes)", got.len());
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn exited_is_queued_without_waiting_when_the_queue_is_full() {
        let (tx, mut rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        let pid = pid_of(&open_shell(&mut mgr, "full", "exec yes").await);
        wait_until_full(&rx).await;

        // The child dies on its own (not through `close`) while nobody has drained anything.
        // A blocked child cannot exit by itself, so end it from outside.
        let status = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .expect("run kill");
        assert!(status.success());
        let finished = eventually(Duration::from_secs(15), || {
            mgr.sessions["full"]
                .wait_task
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
        })
        .await;
        assert!(finished, "`exited` waited for the full queue to drain");

        let exited = wait_for(&mut rx, "exited", Duration::from_secs(15))
            .await
            .expect("`exited` must be delivered");
        assert_eq!(exited.data["session_id"], "full");
        assert_eq!(exited.data["exit_code"], -1);
        drop(rx);
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn close_with_a_full_queue_returns_and_still_delivers_exited() {
        let (tx, mut rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        pid_of(&open_shell(&mut mgr, "busy", "yes").await);
        wait_until_full(&rx).await;

        // The loop that handles `close` is the queue's only consumer: it must not wait on it.
        let started = Instant::now();
        let closed = tokio::time::timeout(
            Duration::from_secs(20),
            send(&mut mgr, "close", json!({ "session_id": "busy" })),
        )
        .await
        .expect("close deadlocked behind the full output queue");
        assert!(closed.is_none());
        assert!(started.elapsed() < Duration::from_secs(15));
        assert_eq!(mgr.active_count(), 0);

        let exited = wait_for(&mut rx, "exited", Duration::from_secs(5))
            .await
            .expect("the final `exited` frame must not be lost");
        assert_eq!(exited.data["session_id"], "busy");
    }

    #[tokio::test]
    async fn a_child_that_never_reads_stdin_times_out_without_blocking_the_loop() {
        let (tx, mut rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        let pid = pid_of(&open_shell(&mut mgr, "deaf", "exec sleep 30").await);

        // 1 MiB cannot fit in the pipe, so the write waits for a reader that never comes.
        let started = Instant::now();
        let reply = tokio::time::timeout(
            Duration::from_secs(25),
            send(
                &mut mgr,
                "input",
                json!({ "session_id": "deaf", "data": "x".repeat(1 << 20) }),
            ),
        )
        .await
        .expect("a stalled stdin write hung the connection loop")
        .expect("a timeout is reported to the client");
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(reply.event, "error");
        assert_eq!(reply.data["status"], 504, "{:?}", reply.data);
        assert_eq!(reply.data["session_id"], "deaf");

        // The session was closed: its child is gone and the client is told it exited.
        assert!(process_gone(pid).await, "child {pid} still running");
        wait_for(&mut rx, "exited", Duration::from_secs(10))
            .await
            .expect("`exited` follows the timeout");
        assert_eq!(mgr.active_count(), 0);

        // The loop is free: a later close and a fresh session are handled promptly.
        let started = Instant::now();
        let gone = send(&mut mgr, "close", json!({ "session_id": "deaf" }))
            .await
            .expect("the session no longer exists");
        assert_eq!(gone.data["status"], 404);
        let next = open_shell(&mut mgr, "next", "exec sleep 30").await;
        pid_of(&next);
        assert!(
            send(&mut mgr, "close", json!({ "session_id": "next" }))
                .await
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_connection_holds_a_bounded_number_of_live_sessions() {
        let (tx, mut rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        for i in 0..MAX_SESSIONS_PER_CONNECTION {
            let opened = open_shell(&mut mgr, &format!("s{i}"), "exec sleep 30").await;
            assert_eq!(opened.event, "opened", "{:?}", opened.data);
        }
        assert_eq!(mgr.active_count(), MAX_SESSIONS_PER_CONNECTION);

        // One more is refused and spawns nothing.
        let refused = open_shell(&mut mgr, "extra", "exec sleep 30").await;
        assert_eq!(refused.event, "error");
        assert_eq!(refused.data["status"], 429, "{:?}", refused.data);
        assert_eq!(mgr.active_count(), MAX_SESSIONS_PER_CONNECTION);
        assert!(!mgr.sessions.contains_key("extra"));
        // Reopening an id that is already open replaces it: that is not a new session.
        let replaced = open_shell(&mut mgr, "s0", "exec sleep 30").await;
        assert_eq!(replaced.event, "opened", "{:?}", replaced.data);

        // Closing one frees its slot.
        assert!(
            send(&mut mgr, "close", json!({ "session_id": "s1" }))
                .await
                .is_none()
        );
        let again = open_shell(&mut mgr, "extra", "exec sleep 30").await;
        assert_eq!(again.event, "opened", "{:?}", again.data);
        assert_eq!(mgr.active_count(), MAX_SESSIONS_PER_CONNECTION);

        drop(rx.try_recv());
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn sessions_that_exit_on_their_own_free_their_slots() {
        let (tx, mut rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        for i in 0..MAX_SESSIONS_PER_CONNECTION {
            let opened = open_shell(&mut mgr, &format!("q{i}"), "exit 0").await;
            assert_eq!(opened.event, "opened", "{:?}", opened.data);
        }
        for _ in 0..MAX_SESSIONS_PER_CONNECTION {
            wait_for(&mut rx, "exited", Duration::from_secs(10))
                .await
                .expect("every child exits");
        }
        // The cap counts live sessions only, so the exited ones never exhaust it.
        for round in 0..3 {
            let opened = open_shell(&mut mgr, &format!("again{round}"), "exit 0").await;
            assert_eq!(opened.event, "opened", "round {round}: {:?}", opened.data);
            wait_for(&mut rx, "exited", Duration::from_secs(10))
                .await
                .expect("exits");
        }
        assert_eq!(mgr.active_count(), 0);
        assert!(mgr.sessions.len() <= MAX_SESSIONS_PER_CONNECTION);
    }

    #[tokio::test]
    async fn a_session_id_is_bounded_and_free_of_control_characters() {
        let (tx, _rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        for bad in [
            "x".repeat(MAX_SESSION_ID_BYTES + 1),
            "a\u{0}b".to_string(),
            "a\nb".to_string(),
            "esc\u{1b}[0m".to_string(),
        ] {
            let reply = open_shell(&mut mgr, &bad, "exec sleep 30").await;
            assert_eq!(reply.event, "error", "{bad:?}");
            assert_eq!(reply.data["status"], 400, "{bad:?}");
        }
        assert!(mgr.sessions.is_empty(), "a refused open spawned something");

        // The longest allowed id works, and a blank one gets a generated id.
        let longest = "y".repeat(MAX_SESSION_ID_BYTES);
        let ok = open_shell(&mut mgr, &longest, "exec sleep 30").await;
        assert_eq!(ok.event, "opened", "{:?}", ok.data);
        let blank = open_shell(&mut mgr, "   ", "exec sleep 30").await;
        assert_eq!(blank.data["session_id"], "term-1");
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn a_disconnecting_client_stops_a_flooding_child() {
        let (tx, rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut mgr = TerminalManager::new(tx);
        // Ignores SIGPIPE, so it would spin on a closed pipe forever unless the hub stops it.
        let pid = pid_of(
            &open_shell(
                &mut mgr,
                "orphan",
                "trap '' PIPE; while true; do echo flood; done",
            )
            .await,
        );
        wait_until_full(&rx).await;

        // The client goes away; the manager is not told (no `close`, no `shutdown_all`).
        drop(rx);
        assert!(
            process_gone(pid).await,
            "child {pid} kept running after the client disconnected"
        );
        drop(mgr);
    }
}
