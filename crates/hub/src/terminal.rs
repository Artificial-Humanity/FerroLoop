//! Owner-scoped interactive terminal process manager for `/ws/v1` (`channel = "terminal"`, spec §4.2).
//!
//! Resource limits (every one is per WebSocket connection):
//!
//! * Output is a bounded queue ([`OUTPUT_QUEUE_CAPACITY`] frames). The reader tasks `send().await`
//!   into it, so a slow client makes them stop reading the child's pipe; the pipe fills and the
//!   child blocks. Nothing is dropped and hub memory stays bounded.
//! * The connection loop is the queue's only consumer, so it must never wait on the queue or on a
//!   child. The manager owns the queue's receiving end ([`TerminalManager::next_frame`]) and every
//!   reply to a client frame is *returned* from [`TerminalManager::handle_frame`] as an ordered
//!   `Vec` that the caller writes to the socket, never queued.
//! * The final `exited` frame is the one frame that must not be lost or wait behind a full
//!   queue. A queue slot is reserved for it when the session opens (`try_reserve_owned`), so
//!   sending it never blocks and never fails while the client is connected. An `open` that finds
//!   no free slot is refused.
//! * Client input goes through a per-session stdin writer task fed by a bounded channel
//!   ([`STDIN_QUEUE_FRAMES`] frames and [`STDIN_QUEUE_BYTES`] bytes). `input` only enqueues and
//!   never waits on the child. A frame that does not fit is refused whole with a 503 `error`
//!   frame (the input is dropped, the session stays alive and the client may retry). A write
//!   that makes no progress for [`STDIN_WRITE_TIMEOUT`] closes the session from the writer task:
//!   a 504 `error` frame, then `exited`.
//! * At most [`MAX_SESSIONS_PER_CONNECTION`] live sessions; `session_id` is bounded.
//!
//! Frame order on one connection, per session id:
//!
//! * `opened` is the first frame of a session and `exited` its last. Replacing a session
//!   (`open` with a live id), `close`, and reopening an id whose child already exited all stop the
//!   old session's tasks first and then hand the caller every frame already queued, in order, in
//!   front of the new `opened`. The old session's `output`, `error` and `exited` frames therefore
//!   reach the socket before the new session's `opened`, and none can arrive after it, so frames
//!   carrying the shared id are never attributed to the wrong session.
//! * `exited` is sent after the child has exited and its output has been read, but the wait is
//!   bounded. A background job that inherited the pipes keeps them open, so after
//!   [`READER_DRAIN_GRACE`] without progress (or [`READER_DRAIN_CEILING`] in total, even while
//!   output keeps flowing) the readers are stopped and `exited` is sent. Output the child had
//!   written but the hub had not yet read is then discarded: `exited` overtakes it. Frames
//!   already queued still precede `exited`. The UI shows `exited (code)` and keeps the log it
//!   has; no output follows `exited`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};

use crate::HubWsFrame;
use crate::auth::HubAuthority;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Frames the output queue of one connection holds before the reader tasks wait for the
/// WebSocket writer. A frame is at most 4096 bytes of text, so this bounds the queue to about
/// 1 MiB (plus JSON overhead) however fast the child writes.
pub const OUTPUT_QUEUE_CAPACITY: usize = 256;

/// How long a write of client input to the child's stdin may make no progress. A child that
/// stopped reading fills the pipe (64 KiB on Linux) and the write would wait forever; the stdin
/// writer task then closes the session.
const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// `input` frames one session's stdin writer holds before further input is refused with 503.
const STDIN_QUEUE_FRAMES: usize = 64;

/// Bytes of input one session's stdin writer holds before further input is refused with 503.
/// A frame is accepted whole or refused whole; one frame larger than this is accepted when the
/// queue is empty (the WebSocket frame limit bounds it), so a large paste still works.
const STDIN_QUEUE_BYTES: usize = 256 * 1024;

/// Live terminal sessions one connection may hold. A session that has exited does not count.
const MAX_SESSIONS_PER_CONNECTION: usize = 8;

/// Longest `session_id` a client may choose.
const MAX_SESSION_ID_BYTES: usize = 64;

/// How long, after the child has exited, the reader tasks get to reach end of file before the
/// `exited` frame is sent anyway. A background job the shell left behind (`sleep 600 &`) keeps
/// the stdout/stderr pipes open, so without this bound the exit would go unreported (and a
/// close or disconnect would hang) until that job ended. The wait is for *no progress*: it
/// restarts while the readers keep delivering frames, so a slow client that drains a large
/// backlog still gets all the output before `exited`. [`READER_DRAIN_CEILING`] caps the total.
const READER_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Hard cap on the whole post-exit drain, however much progress the readers make. A background
/// job that keeps writing (`yes &`) would otherwise keep the grace restarting and `exited`
/// would never be sent while the client reads.
#[cfg(not(test))]
const READER_DRAIN_CEILING: Duration = Duration::from_secs(30);
/// Shortened under test so the ceiling can be exercised in a few seconds.
#[cfg(test)]
const READER_DRAIN_CEILING: Duration = Duration::from_secs(4);

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
/// Bounded hand-off from the connection loop to a session's stdin writer task.
struct StdinQueue {
    tx: mpsc::Sender<Vec<u8>>,
    /// Bytes enqueued and not yet written to the child.
    pending: Arc<AtomicUsize>,
}

/// Why [`StdinQueue::try_enqueue`] refused input.
enum InputRefused {
    /// The queue holds its limit; the input was dropped.
    Full,
    /// The writer task has ended (the child exited or the session is closing).
    Closed,
}

impl StdinQueue {
    /// Queue `bytes` without waiting. The frame is taken whole or refused whole.
    fn try_enqueue(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        let len = bytes.len();
        loop {
            let current = self.pending.load(Ordering::Acquire);
            if current != 0 && current.saturating_add(len) > STDIN_QUEUE_BYTES {
                return Err(InputRefused::Full);
            }
            if self
                .pending
                .compare_exchange(current, current + len, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        self.tx.try_send(bytes).map_err(|err| {
            self.pending.fetch_sub(len, Ordering::AcqRel);
            match err {
                mpsc::error::TrySendError::Full(_) => InputRefused::Full,
                mpsc::error::TrySendError::Closed(_) => InputRefused::Closed,
            }
        })
    }
}

/// Active child process session managed for a single WebSocket connection.
pub struct TerminalSession {
    pub session_id: String,
    pub pid: Option<u32>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    input: StdinQueue,
    stdin_task: JoinHandle<()>,
    kill_tx: Option<oneshot::Sender<()>>,
    wait_task: Option<JoinHandle<()>>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    /// Set just after the `exited` frame is queued, so a client that has seen the frame can
    /// reopen at once without the session still counting against the cap. A session whose flag
    /// is set has its `exited` frame in the queue (or already written), so reaping it never
    /// leaves that frame still to come.
    exited: Arc<AtomicBool>,
}

impl TerminalSession {
    /// Terminate the child process and wait for the session's tasks to end.
    ///
    /// The readers and the stdin writer are stopped first and awaited, so nothing of this session
    /// is produced after this returns except the `exited` frame, which is already in the output
    /// queue by then. Stopping the readers first also lets the exit task skip its drain wait: a
    /// flooding child with a full queue would otherwise hold every close for the whole grace.
    pub async fn shutdown(mut self) {
        self.stdin_task.abort();
        self.stdout_task.abort();
        self.stderr_task.abort();
        let _ = (&mut self.stdin_task).await;
        let _ = (&mut self.stdout_task).await;
        let _ = (&mut self.stderr_task).await;
        if let Some(tx) = self.kill_tx.take() {
            let _ = tx.send(());
        }
        if let Some(wait_task) = self.wait_task.take() {
            let _ = wait_task.await;
        }
    }

    /// The child has exited and its `exited` frame is queued.
    fn is_finished(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.stdin_task.abort();
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
    out_rx: mpsc::Receiver<HubWsFrame>,
    next_id: u64,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalManager {
    /// A manager with its own output queue of [`OUTPUT_QUEUE_CAPACITY`] frames.
    pub fn new() -> Self {
        let (out_tx, out_rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        Self {
            sessions: BTreeMap::new(),
            out_tx,
            out_rx,
            next_id: 1,
        }
    }

    /// The next queued `output`/`error`/`exited` frame, waiting for one. The connection loop
    /// writes it to the socket. Cancel safe. `None` only after [`Self::close_output`] once the
    /// queue is empty.
    pub async fn next_frame(&mut self) -> Option<HubWsFrame> {
        self.out_rx.recv().await
    }

    /// Stop accepting output: a reader blocked on the full queue fails at once and its child is
    /// stopped. Frames already queued can still be received.
    pub fn close_output(&mut self) {
        self.out_rx.close();
    }

    /// Sessions whose child is still running.
    pub fn active_count(&self) -> usize {
        self.sessions.values().filter(|s| !s.is_finished()).count()
    }

    /// Terminate all spawned child processes when the owning WebSocket disconnects. The sessions
    /// are shut down concurrently, so the time taken does not grow with their number.
    pub async fn shutdown_all(&mut self) {
        // Closed first: a reader blocked on a full queue then fails at once and the children stop,
        // instead of the teardown waiting for them.
        self.close_output();
        let mut set = JoinSet::new();
        for (_, session) in std::mem::take(&mut self.sessions) {
            set.spawn(session.shutdown());
        }
        while set.join_next().await.is_some() {}
    }

    /// Every frame queued right now, in order. Called only after the tasks that feed the queue
    /// for the session being replaced or closed have ended, so all of that session's frames
    /// (its `exited` included) are among them. Bounded by the queue's length at the call.
    fn drain_queued(&mut self) -> Vec<HubWsFrame> {
        let queued = self.out_rx.len();
        let mut frames = Vec::with_capacity(queued);
        for _ in 0..queued {
            match self.out_rx.try_recv() {
                Ok(frame) => frames.push(frame),
                Err(_) => break,
            }
        }
        frames
    }

    /// Handle an incoming `channel == "terminal"` frame from a WebSocket client. The returned
    /// frames are the direct reply, in the order the caller must write them (see the module
    /// comment for why they are not queued). Output and `exited` frames of running sessions
    /// arrive through [`Self::next_frame`].
    pub async fn handle_frame(
        &mut self,
        authority: &HubAuthority,
        enable_terminal: bool,
        default_cwd: &Path,
        event: &str,
        data: &Value,
    ) -> Vec<HubWsFrame> {
        if !authority.is_owner() || !enable_terminal {
            return vec![error_frame(
                403,
                None,
                "owner authority required for terminal sessions",
            )];
        }

        match event {
            "open" => self.handle_open(default_cwd, data).await,
            "input" => self.handle_input(data),
            "resize" => vec![self.handle_resize(data)],
            "close" => self.handle_close(data).await,
            other => vec![error_frame(
                400,
                None,
                format!("unknown terminal event `{other}`"),
            )],
        }
    }

    /// Open a session. The reply is `[queued frames of the replaced session..., opened]` or
    /// `[..., error]`; the queued frames come first so the old session's `exited` precedes the
    /// new `opened`.
    async fn handle_open(&mut self, default_cwd: &Path, data: &Value) -> Vec<HubWsFrame> {
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
                return vec![error_frame(
                    400,
                    None,
                    format!("invalid terminal open payload: {e}"),
                )];
            }
        };

        let session_id = match clean_session_id(payload.session_id.as_deref()) {
            Ok(Some(id)) => id,
            Ok(None) => {
                let id = format!("term-{}", self.next_id);
                self.next_id += 1;
                id
            }
            Err(message) => return vec![error_frame(400, None, message)],
        };

        // Reopening an id replaces its session, so that one does not count against the cap. A
        // finished session of this id stays in the map until the replacement below, so its
        // `exited` frame is still ordered in front of the new `opened`.
        self.sessions
            .retain(|id, s| *id == session_id || !s.is_finished());
        let others = self.sessions.len() - usize::from(self.sessions.contains_key(&session_id));
        if others >= MAX_SESSIONS_PER_CONNECTION {
            return vec![error_frame(
                429,
                Some(&session_id),
                format!(
                    "too many terminal sessions on this connection (limit {MAX_SESSIONS_PER_CONNECTION})"
                ),
            )];
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
            return vec![error_frame(
                400,
                Some(&session_id),
                format!("working directory `{}` does not exist", cwd.display()),
            )];
        }

        let (program, args) = resolve_command(payload.command.as_ref(), &payload.args);
        let cols = payload.cols.unwrap_or(DEFAULT_COLS).max(1);
        let rows = payload.rows.unwrap_or(DEFAULT_ROWS).max(1);

        // If an older session with the same ID exists, shut it down cleanly before replacing it.
        // Then take everything queued: the old session's last frames (its `exited`, queued when
        // the child died, whether or not this connection still tracked it) belong before the
        // `opened` below. This always runs, so an id that was reaped earlier is covered too.
        if let Some(prev) = self.sessions.remove(&session_id) {
            prev.shutdown().await;
        }
        let mut frames = self.drain_queued();

        // The queue slot for this session's `exited` frame (see the module comment). Taken
        // without waiting: the connection loop is the queue's consumer, so it cannot wait here.
        let exit_permit = match self.out_tx.clone().try_reserve_owned() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(_)) => {
                frames.push(error_frame(
                    503,
                    Some(&session_id),
                    "terminal output backlog is full; retry once the client has caught up",
                ));
                return frames;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                frames.push(error_frame(
                    500,
                    Some(&session_id),
                    "terminal output channel is closed",
                ));
                return frames;
            }
        };

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
                frames.push(error_frame(
                    500,
                    Some(&session_id),
                    format!("failed to spawn `{program}`: {e}"),
                ));
                return frames;
            }
        };

        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Frames the readers have delivered (the drain grace restarts while this moves), the
        // signal a reader raises when the output queue is closed because the client is gone, and
        // the one the stdin writer raises when the child stopped reading its input.
        let progress = Arc::new(AtomicU64::new(0));
        let client_gone = Arc::new(Notify::new());
        let stdin_stalled = Arc::new(Notify::new());

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

        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>(STDIN_QUEUE_FRAMES);
        let pending = Arc::new(AtomicUsize::new(0));
        let stdin_task = tokio::spawn(pump_input(
            stdin,
            input_rx,
            pending.clone(),
            session_id.clone(),
            self.out_tx.clone(),
            stdin_stalled.clone(),
        ));

        let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
        let sid_wait = session_id.clone();
        let exited = Arc::new(AtomicBool::new(false));
        let exited_flag = exited.clone();
        let readers = [stdout_task.abort_handle(), stderr_task.abort_handle()];
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
                // The child stopped reading its input; the writer has told the client why.
                _ = stdin_stalled.notified() => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let mut drained = Box::pin(async {
                let _ = stdout_done_rx.await;
                let _ = stderr_done_rx.await;
            });
            drain_readers(&mut drained, &progress).await;
            // `exited` is the session's last frame: stop any reader a lingering job kept alive.
            stop_readers(&readers);
            let exit_code = status_res.ok().and_then(|s| s.code()).unwrap_or(-1);
            // Into the slot reserved at open: never waits, and is lost only if the client is
            // gone. Queued before the flag is raised (see `TerminalSession::exited`).
            exit_permit.send(terminal_frame(
                "exited",
                json!({
                    "session_id": sid_wait,
                    "exit_code": exit_code,
                }),
            ));
            exited_flag.store(true, Ordering::Release);
        });

        self.sessions.insert(
            session_id.clone(),
            TerminalSession {
                session_id: session_id.clone(),
                pid,
                cwd: cwd.clone(),
                cols,
                rows,
                input: StdinQueue {
                    tx: input_tx,
                    pending,
                },
                stdin_task,
                kill_tx: Some(kill_tx),
                wait_task: Some(wait_task),
                stdout_task,
                stderr_task,
                exited,
            },
        );

        // Written after everything queued so far and before the session's own output.
        frames.push(terminal_frame(
            "opened",
            json!({
                "session_id": session_id,
                "pid": pid,
                "cwd": cwd.display().to_string(),
                "cols": cols,
                "rows": rows,
            }),
        ));
        frames
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

    /// Queue client input for the child's stdin; never waits on the child.
    ///
    /// The session's stdin writer task does the write (see the module comment). Input that does
    /// not fit its bounded queue is dropped whole with a 503 `error` frame; the session stays
    /// alive and the client may retry. A child that stops reading is detected by the writer
    /// task, which closes the session with a 504 `error` frame followed by `exited`.
    fn handle_input(&mut self, data: &Value) -> Vec<HubWsFrame> {
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
                return vec![error_frame(
                    400,
                    None,
                    format!("invalid terminal input payload: {e}"),
                )];
            }
        };

        let Some(key) = self.resolve_session_key(payload.session_id.as_deref()) else {
            return vec![error_frame(404, None, "terminal session not found")];
        };
        if payload.data.is_empty() {
            return Vec::new();
        }

        let Some(session) = self.sessions.get(&key) else {
            return Vec::new();
        };
        match session.input.try_enqueue(payload.data.into_bytes()) {
            Ok(()) => Vec::new(),
            Err(InputRefused::Full) => vec![error_frame(
                503,
                Some(&key),
                "terminal input backlog is full; the input was dropped, retry once the process has read its input",
            )],
            Err(InputRefused::Closed) => vec![error_frame(
                500,
                Some(&key),
                "terminal stdin is closed (the process exited or the session is closing)",
            )],
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

    /// Close a session. The reply is every frame queued once it has stopped, ending with its
    /// `exited` frame (unless the client already received that one).
    async fn handle_close(&mut self, data: &Value) -> Vec<HubWsFrame> {
        #[derive(Debug, Deserialize, Default)]
        struct ClosePayload {
            #[serde(default)]
            session_id: Option<String>,
        }

        let payload: ClosePayload = serde_json::from_value(data.clone()).unwrap_or_default();
        let Some(key) = self.resolve_session_key(payload.session_id.as_deref()) else {
            return vec![error_frame(404, None, "terminal session not found")];
        };

        if let Some(sess) = self.sessions.remove(&key) {
            sess.shutdown().await;
        }
        self.drain_queued()
    }
}

/// Wait for both readers to reach end of file, bounded (see [`READER_DRAIN_GRACE`] and
/// [`READER_DRAIN_CEILING`]): give up after the grace passes with no new frame, or when the
/// ceiling is reached.
async fn drain_readers(
    drained: &mut (impl std::future::Future<Output = ()> + Unpin),
    progress: &AtomicU64,
) {
    let deadline = tokio::time::Instant::now() + READER_DRAIN_CEILING;
    let mut seen = progress.load(Ordering::Relaxed);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return;
        }
        let slice = READER_DRAIN_GRACE.min(left);
        if tokio::time::timeout(slice, &mut *drained).await.is_ok() {
            return;
        }
        let now = progress.load(Ordering::Relaxed);
        if now == seen {
            return;
        }
        seen = now;
    }
}

fn stop_readers(readers: &[AbortHandle]) {
    for reader in readers {
        reader.abort();
    }
}

/// Write the client's input to the child's stdin, one queued frame at a time.
///
/// Each `write` is bounded by [`STDIN_WRITE_TIMEOUT`]: that is a bound on *no progress*, so a
/// child that reads slowly is fed and one that stopped reading is not waited for. On a timeout
/// the client gets a 504 `error` frame and the child is stopped (through `stalled`), which
/// makes the exit task send `exited` after it: part of a frame may already be in the pipe, so
/// keeping the session would feed the child a corrupted stream. Any other write error (the
/// child is gone) is reported with a 500 `error` frame. Either way the task ends and later
/// `input` is refused as closed.
async fn pump_input(
    stdin: Option<ChildStdin>,
    mut queue: mpsc::Receiver<Vec<u8>>,
    pending: Arc<AtomicUsize>,
    session_id: String,
    out_tx: mpsc::Sender<HubWsFrame>,
    stalled: Arc<Notify>,
) {
    let Some(mut stdin) = stdin else {
        return;
    };
    while let Some(bytes) = queue.recv().await {
        let len = bytes.len();
        let mut written = 0;
        let mut fault = None;
        while written < len {
            match tokio::time::timeout(STDIN_WRITE_TIMEOUT, stdin.write(&bytes[written..])).await {
                Ok(Ok(n)) if n > 0 => written += n,
                Ok(Ok(_)) => {
                    fault = Some(error_frame(
                        500,
                        Some(&session_id),
                        "failed writing to terminal stdin: wrote zero bytes",
                    ));
                    break;
                }
                Ok(Err(e)) => {
                    fault = Some(error_frame(
                        500,
                        Some(&session_id),
                        format!("failed writing to terminal stdin: {e}"),
                    ));
                    break;
                }
                Err(_elapsed) => {
                    // The error frame goes first; `exited` follows once the child is stopped.
                    let _ = out_tx
                        .send(error_frame(
                            504,
                            Some(&session_id),
                            "writing to terminal stdin timed out (the process is not reading); session closed",
                        ))
                        .await;
                    stalled.notify_one();
                    return;
                }
            }
        }
        pending.fetch_sub(len, Ordering::AcqRel);
        if let Some(frame) = fault {
            let _ = out_tx.send(frame).await;
            return;
        }
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

    /// A manager and the socket it writes to. `wire` is every frame in the order the connection
    /// loop writes it: a direct reply is written the moment `call` returns it, a queued frame
    /// when `recv` takes it off the output queue. `read` is how far a test has looked.
    struct Wire {
        mgr: TerminalManager,
        wire: Vec<HubWsFrame>,
        read: usize,
    }

    impl Wire {
        fn new() -> Self {
            Self {
                mgr: TerminalManager::new(),
                wire: Vec::new(),
                read: 0,
            }
        }

        async fn call(&mut self, event: &str, data: Value) -> Vec<HubWsFrame> {
            let cwd = std::env::temp_dir();
            let frames = self
                .mgr
                .handle_frame(&owner(), true, &cwd, event, &data)
                .await;
            self.wire.extend(frames.iter().cloned());
            frames
        }

        /// Open `script` under `/bin/sh -c`; returns the reply (`opened` or an error), the last
        /// frame of the direct reply.
        async fn open_shell(&mut self, id: &str, script: &str) -> HubWsFrame {
            self.call(
                "open",
                json!({ "session_id": id, "command": ["/bin/sh", "-c", script] }),
            )
            .await
            .pop()
            .expect("open always replies")
        }

        /// The next frame on the socket, waiting up to `limit` for one.
        async fn recv(&mut self, limit: Duration) -> Option<HubWsFrame> {
            if self.read >= self.wire.len() {
                let frame = tokio::time::timeout(limit, self.mgr.next_frame())
                    .await
                    .ok()
                    .flatten()?;
                self.wire.push(frame);
            }
            self.read += 1;
            Some(self.wire[self.read - 1].clone())
        }

        /// Take the next frame off the output queue onto the socket, waiting up to `limit`.
        /// Leaves the read cursor alone (the frame is for `wire`, not for a `recv`).
        async fn pump(&mut self, limit: Duration) -> Option<HubWsFrame> {
            let frame = tokio::time::timeout(limit, self.mgr.next_frame())
                .await
                .ok()
                .flatten()?;
            self.wire.push(frame.clone());
            Some(frame)
        }

        /// The next frame of `event`, waiting up to `limit` for it.
        async fn wait_for(&mut self, event: &str, limit: Duration) -> Option<HubWsFrame> {
            let deadline = Instant::now() + limit;
            loop {
                let left = deadline.checked_duration_since(Instant::now())?;
                let frame = self.recv(left).await?;
                if frame.event == event {
                    return Some(frame);
                }
            }
        }

        /// Frames in the output queue right now.
        fn queued(&self) -> usize {
            self.mgr.out_rx.len()
        }

        /// The `opened`/`exited` frames of session `id`, in the order they were written.
        fn lifecycle(&self, id: &str) -> Vec<&str> {
            self.wire
                .iter()
                .filter(|f| matches!(f.event.as_str(), "opened" | "exited"))
                .filter(|f| f.data["session_id"] == id)
                .map(|f| f.event.as_str())
                .collect()
        }
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

    /// Wait until the output queue is (nearly) full. Each open session holds one slot for its
    /// `exited` frame (`sessions` of them), so the queue never holds the full capacity.
    async fn wait_until_full(w: &Wire, sessions: usize) {
        assert!(
            eventually(Duration::from_secs(15), || {
                w.queued() + sessions + 1 >= OUTPUT_QUEUE_CAPACITY
            })
            .await,
            "the queue never filled (len {})",
            w.queued()
        );
    }

    #[tokio::test]
    async fn exit_is_reported_even_when_a_background_job_holds_the_pipes() {
        let mut w = Wire::new();
        // The shell exits at once; `sleep` inherits stdout/stderr and lives on for 8 s.
        w.open_shell("t1", "sleep 8 & exit 3").await;
        let exited = w
            .wait_for("exited", Duration::from_secs(5))
            .await
            .expect("`exited` must not wait for the background job");
        assert_eq!(exited.data["exit_code"], 3);
        assert_eq!(exited.data["session_id"], "t1");
        w.mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn close_does_not_hang_on_a_pipe_held_by_a_background_job() {
        let mut w = Wire::new();
        w.open_shell("t2", "sleep 8 & echo ready; wait").await;
        // Kill the shell only once it has started the background job that holds the pipes.
        let ready = w.wait_for("output", Duration::from_secs(5)).await;
        assert!(ready.is_some_and(|f| f.data["data"].as_str().unwrap().contains("ready")));
        let started = Instant::now();
        let done = tokio::time::timeout(Duration::from_secs(5), w.mgr.shutdown_all()).await;
        assert!(
            done.is_ok(),
            "shutdown_all hung for {:?}",
            started.elapsed()
        );
        assert_eq!(w.mgr.active_count(), 0);
    }

    #[tokio::test]
    async fn terminal_output_keeps_multibyte_text_whole() {
        let mut w = Wire::new();
        // 4095 ASCII bytes then "é" (2 bytes) puts the character across the 4096-byte read.
        w.open_shell(
            "t3",
            "head -c 4095 /dev/zero | tr '\\0' a; printf '\\303\\251 end'",
        )
        .await;
        let mut text = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let Some(frame) = w.wait_for("output", Duration::from_secs(1)).await else {
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
        w.mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn terminal_requires_owner_authority_and_the_feature_flag() {
        let mut mgr = TerminalManager::new();
        let cwd = std::env::temp_dir();
        let peer = HubAuthority::Peer { agent: None };
        let replies = [
            mgr.handle_frame(&peer, true, &cwd, "open", &json!({}))
                .await,
            mgr.handle_frame(&owner(), false, &cwd, "open", &json!({}))
                .await,
        ];
        for reply in replies {
            assert_eq!(reply.len(), 1, "a refusal is one direct reply");
            assert_eq!(reply[0].event, "error");
            assert_eq!(reply[0].data["status"], 403);
        }
        assert_eq!(mgr.out_rx.len(), 0, "replies are not queued");
        assert_eq!(mgr.active_count(), 0);
    }

    #[tokio::test]
    async fn a_flooding_child_is_held_back_by_the_bounded_queue_and_resumes_on_drain() {
        const { assert!(OUTPUT_QUEUE_CAPACITY <= 1024, "the bound must stay small") };
        let mut w = Wire::new();
        pid_of(&w.open_shell("flood", "yes").await);

        // Nobody reads: the queue fills and then stops growing, however fast `yes` writes.
        wait_until_full(&w, 1).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let held = w.queued();
        assert!(held <= OUTPUT_QUEUE_CAPACITY, "queue holds {held} frames");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            w.queued(),
            held,
            "the reader must be blocked, not producing"
        );

        // Draining unblocks the reader and the child: the queue refills.
        for _ in 0..100 {
            w.mgr.next_frame().await.expect("queued frame");
        }
        wait_until_full(&w, 1).await;
        assert!(w.queued() <= OUTPUT_QUEUE_CAPACITY);

        w.mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn backpressure_loses_no_output_and_keeps_its_order() {
        let mut w = Wire::new();
        // About 2 MiB, well over what the queue and the pipe hold together.
        pid_of(&w.open_shell("count", "exec seq 1 300000").await);
        wait_until_full(&w, 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let expected: String = (1..=300_000).map(|n| format!("{n}\n")).collect();
        let mut got = String::new();
        let mut exited = None;
        while exited.is_none() {
            let frame = w
                .recv(Duration::from_secs(20))
                .await
                .expect("the stream stalled");
            match frame.event.as_str() {
                "opened" => {}
                "output" => got.push_str(frame.data["data"].as_str().unwrap()),
                "exited" => exited = Some(frame),
                other => panic!("unexpected `{other}` frame"),
            }
        }
        assert_eq!(exited.unwrap().data["exit_code"], 0);
        assert!(got == expected, "output differs ({} bytes)", got.len());
        w.mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn exited_is_queued_without_waiting_when_the_queue_is_full() {
        let mut w = Wire::new();
        let pid = pid_of(&w.open_shell("full", "exec yes").await);
        wait_until_full(&w, 1).await;

        // The child dies on its own (not through `close`) while nobody has drained anything.
        // A blocked child cannot exit by itself, so end it from outside.
        let status = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .expect("run kill");
        assert!(status.success());
        let finished = eventually(Duration::from_secs(15), || {
            w.mgr.sessions["full"]
                .wait_task
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
        })
        .await;
        assert!(finished, "`exited` waited for the full queue to drain");

        let exited = w
            .wait_for("exited", Duration::from_secs(15))
            .await
            .expect("`exited` must be delivered");
        assert_eq!(exited.data["session_id"], "full");
        assert_eq!(exited.data["exit_code"], -1);
        w.mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn close_with_a_full_queue_returns_and_delivers_exited_last() {
        let mut w = Wire::new();
        pid_of(&w.open_shell("busy", "yes").await);
        wait_until_full(&w, 1).await;

        // The loop that handles `close` is the queue's only consumer: it must not wait on it.
        let started = Instant::now();
        let reply = tokio::time::timeout(
            Duration::from_secs(20),
            w.call("close", json!({ "session_id": "busy" })),
        )
        .await
        .expect("close deadlocked behind the full output queue");
        assert!(started.elapsed() < Duration::from_secs(15));
        assert_eq!(w.mgr.active_count(), 0);

        // The final `exited` frame is in the reply, after everything the session had queued.
        let last = reply.last().expect("close returns the queued frames");
        assert_eq!(last.event, "exited", "{last:?}");
        assert_eq!(last.data["session_id"], "busy");
        assert_eq!(
            reply.iter().filter(|f| f.event == "exited").count(),
            1,
            "exactly one `exited`"
        );
        assert_eq!(w.queued(), 0, "nothing of the session is left queued");
    }

    #[tokio::test]
    async fn close_and_shutdown_are_prompt_with_flooding_children_and_a_full_queue() {
        // A flooding child with a full queue has both readers blocked in `send`; the close must
        // not wait for the drain grace (2 s) on them.
        let mut w = Wire::new();
        pid_of(&w.open_shell("flood", "exec yes").await);
        wait_until_full(&w, 1).await;
        let started = Instant::now();
        let reply = w.call("close", json!({ "session_id": "flood" })).await;
        let took = started.elapsed();
        assert!(
            took < Duration::from_millis(1500),
            "close of a flooding session took {took:?}"
        );
        assert_eq!(reply.last().expect("reply").event, "exited");

        // Several flooding sessions torn down together: bounded, and not one grace per session.
        const FLOODERS: usize = 4;
        for i in 0..FLOODERS {
            pid_of(&w.open_shell(&format!("f{i}"), "exec yes").await);
        }
        wait_until_full(&w, FLOODERS).await;
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(10), w.mgr.shutdown_all())
            .await
            .expect("shutdown_all hung");
        let took = started.elapsed();
        assert!(
            took < Duration::from_millis(3000),
            "shutdown_all of {FLOODERS} flooding sessions took {took:?}"
        );
        assert_eq!(w.mgr.active_count(), 0);
    }

    #[tokio::test]
    async fn exited_is_sent_within_the_ceiling_while_a_background_job_keeps_writing() {
        let mut w = Wire::new();
        // The shell exits at once; `yes` keeps both pipes open and keeps producing output, so
        // the no-progress grace never expires while the client reads.
        pid_of(&w.open_shell("keepalive", "yes & exit 0").await);
        let started = Instant::now();
        let mut outputs_before_exit = 0usize;
        let exited = loop {
            let frame = w
                .recv(Duration::from_secs(20))
                .await
                .expect("`exited` was never sent while output kept flowing");
            // A ready stream never yields, so check the clock here instead of timing out.
            assert!(
                started.elapsed() < READER_DRAIN_CEILING + Duration::from_secs(8),
                "`exited` was not sent within the ceiling while output kept flowing"
            );
            match frame.event.as_str() {
                "output" => outputs_before_exit += 1,
                "exited" => break frame,
                _ => {}
            }
        };
        let took = started.elapsed();
        assert_eq!(exited.data["exit_code"], 0);
        assert!(outputs_before_exit > 0);
        assert!(
            took >= READER_DRAIN_GRACE,
            "ended by the grace, not the ceiling: {took:?}"
        );
        assert!(
            took < READER_DRAIN_CEILING + Duration::from_secs(8),
            "exited took {took:?} (ceiling {READER_DRAIN_CEILING:?})"
        );

        // `exited` is final: the readers were stopped, so no output follows it.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Some(frame) = w.recv(Duration::from_millis(50)).await {
            assert_ne!(frame.event, "output", "output after `exited`");
        }
        w.mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn a_child_that_never_reads_stdin_is_refused_then_closed_without_blocking_the_loop() {
        let mut w = Wire::new();
        let pid = pid_of(&w.open_shell("deaf", "exec sleep 30").await);

        // Input is queued, never awaited: every call returns at once, and once the bounded
        // queue holds its limit the frame is refused with 503 while the session stays alive.
        let started = Instant::now();
        let mut refused = None;
        for _ in 0..64 {
            let reply = w
                .call(
                    "input",
                    json!({ "session_id": "deaf", "data": "x".repeat(64 * 1024) }),
                )
                .await;
            if let Some(frame) = reply.into_iter().next() {
                refused = Some(frame);
                break;
            }
        }
        let refused = refused.expect("a bounded queue must refuse input once full");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(refused.event, "error");
        assert_eq!(refused.data["status"], 503, "{:?}", refused.data);
        assert_eq!(refused.data["session_id"], "deaf");
        assert_eq!(w.mgr.active_count(), 1, "a refusal must keep the session");
        assert!(Path::new(&format!("/proc/{pid}")).exists());

        // No progress for STDIN_WRITE_TIMEOUT: the writer reports 504 and the session ends.
        let timed_out = loop {
            let frame = w
                .wait_for("error", Duration::from_secs(25))
                .await
                .expect("the stalled write was never reported");
            if frame.data["status"] == 504 {
                break frame;
            }
        };
        assert_eq!(timed_out.data["session_id"], "deaf");
        assert!(process_gone(pid).await, "child {pid} still running");
        w.wait_for("exited", Duration::from_secs(10))
            .await
            .expect("`exited` follows the timeout");
        assert_eq!(w.mgr.active_count(), 0);
        let order: Vec<&str> = w
            .wire
            .iter()
            .filter(|f| f.data["session_id"] == "deaf")
            .filter(|f| f.data["status"] == 504 || f.event == "exited")
            .map(|f| f.event.as_str())
            .collect();
        assert_eq!(order, ["error", "exited"], "504 comes before `exited`");

        // Later input is refused as closed, and the loop is free: a close and a fresh session
        // are handled promptly.
        let closed = w
            .call("input", json!({ "session_id": "deaf", "data": "x" }))
            .await;
        assert_eq!(closed[0].data["status"], 500, "{:?}", closed[0].data);
        let started = Instant::now();
        let reply = w.call("close", json!({ "session_id": "deaf" })).await;
        assert_eq!(
            reply.iter().filter(|f| f.event == "exited").count(),
            0,
            "the client already has this session's `exited`"
        );
        let next = w.open_shell("next", "exec sleep 30").await;
        pid_of(&next);
        let reply = w.call("close", json!({ "session_id": "next" })).await;
        assert_eq!(reply.last().expect("reply").event, "exited");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn an_echoing_child_survives_input_larger_than_every_buffer() {
        // 4 MiB of numbered lines in 16 KiB frames: far more than the stdin pipe (64 KiB), the
        // output pipe, the output queue (about 1 MiB) and the stdin queue hold together. `cat`
        // stops reading while its echo cannot be delivered, so a connection loop that waited on
        // the child would deadlock here and later kill a healthy session.
        const LINES_PER_FRAME: usize = 2048;
        const FRAMES: usize = 256;
        let line = |n: usize| format!("{n:07}\n");
        let frames: Vec<String> = (0..FRAMES)
            .map(|f| {
                (f * LINES_PER_FRAME..(f + 1) * LINES_PER_FRAME)
                    .map(line)
                    .collect()
            })
            .collect();
        let expected: String = frames.concat();
        assert!(expected.len() >= 4 << 20);

        let mut w = Wire::new();
        let pid = pid_of(&w.open_shell("echo", "exec cat").await);

        let mut echoed = 0usize;
        let mut refusals = 0usize;
        let run = async {
            for chunk in &frames {
                loop {
                    let reply = w
                        .call("input", json!({ "session_id": "echo", "data": chunk }))
                        .await;
                    match reply.first() {
                        None => break,
                        Some(f) if f.data["status"] == 503 => refusals += 1,
                        Some(f) => panic!("unexpected reply {f:?}"),
                    }
                    // The client reads only when refused, so the burst outruns every buffer
                    // first; reading relieves the back-pressure and the frame is retried.
                    if let Some(f) = w.pump(Duration::from_millis(20)).await
                        && f.event == "output"
                    {
                        echoed += f.data["data"].as_str().unwrap().len();
                    }
                }
            }
            while echoed < expected.len() {
                let f = w.pump(Duration::from_secs(15)).await.expect("echo stalled");
                if f.event == "output" {
                    echoed += f.data["data"].as_str().unwrap().len();
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("the connection loop waited on the child");

        let got: String = w
            .wire
            .iter()
            .filter(|f| f.event == "output")
            .map(|f| f.data["data"].as_str().unwrap())
            .collect();
        assert!(got == expected, "echo differs ({} bytes)", got.len());
        assert!(
            w.wire
                .iter()
                .all(|f| f.event != "exited" && f.data["status"] != 504),
            "a healthy session was closed (refusals: {refusals})"
        );
        assert!(refusals > 0, "the burst never outran the buffers");
        assert_eq!(w.mgr.active_count(), 1);

        // The loop stayed responsive: a close after the burst completes promptly.
        let started = Instant::now();
        let reply = tokio::time::timeout(
            Duration::from_secs(10),
            w.call("close", json!({ "session_id": "echo" })),
        )
        .await
        .expect("close hung after the burst");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(reply.last().expect("reply").event, "exited");
        assert!(process_gone(pid).await);
    }

    /// How the old session ends before the id is opened again.
    #[derive(Clone, Copy, Debug)]
    enum Replace {
        /// `close`, then `open` with the same id.
        CloseThenOpen,
        /// `open` with the id of a live session.
        OpenOverLive,
    }

    /// Open `x`, end it as `how` says, open `x` again, and check what a socket sees: `exited`
    /// (old) before `opened` (new), none after it, and none of the old session's output after it.
    async fn check_replace_order(old_script: &str, how: Replace, separate_reads: bool) {
        let mut w = Wire::new();
        let flood = old_script.contains("yes");
        pid_of(&w.open_shell("x", old_script).await);
        if flood {
            wait_until_full(&w, 1).await;
        } else {
            assert!(
                eventually(Duration::from_secs(5), || w.queued() >= 1).await,
                "the old session produced nothing"
            );
        }
        if separate_reads {
            // The client reads some of the old session's frames, then sends the next request.
            for _ in 0..5 {
                if w.recv(Duration::from_millis(200)).await.is_none() {
                    break;
                }
            }
        }
        match how {
            Replace::CloseThenOpen => {
                let closed = w.call("close", json!({ "session_id": "x" })).await;
                assert_eq!(closed.last().expect("close reply").event, "exited");
                if separate_reads {
                    while w.recv(Duration::from_millis(50)).await.is_some() {}
                }
            }
            Replace::OpenOverLive => {}
        }
        let reopened = w.open_shell("x", "echo new; exec sleep 30").await;
        pid_of(&reopened);
        // Let everything the sessions will produce arrive.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while w.recv(Duration::from_millis(100)).await.is_some() {}

        let case = format!("{old_script:?} {how:?} separate_reads={separate_reads}");
        assert_eq!(
            w.lifecycle("x"),
            ["opened", "exited", "opened"],
            "{case}: exited(old) must precede opened(new), with no exited after it"
        );
        let last_opened = w
            .wire
            .iter()
            .rposition(|f| f.event == "opened")
            .expect("opened");
        for f in &w.wire[last_opened + 1..] {
            if f.event == "output" {
                let data = f.data["data"].as_str().unwrap();
                assert!(
                    data.contains("new") && !data.contains("old"),
                    "{case}: stale output after the new `opened`: {data:?}"
                );
            } else {
                panic!("{case}: unexpected {f:?} after the new `opened`");
            }
        }
        w.mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn exited_of_the_old_session_precedes_opened_of_the_new_one() {
        for old_script in ["echo old; exec sleep 30", "echo old; exec yes"] {
            for how in [Replace::CloseThenOpen, Replace::OpenOverLive] {
                for separate_reads in [false, true] {
                    check_replace_order(old_script, how, separate_reads).await;
                }
            }
        }
    }

    #[tokio::test]
    async fn exited_of_a_session_that_ended_on_its_own_precedes_a_reopen() {
        // The child exits by itself and nobody has read its `exited` yet when the id is opened
        // again, with the old session still tracked or already reaped by another open.
        for reaped_first in [false, true] {
            let mut w = Wire::new();
            pid_of(&w.open_shell("x", "exit 0").await);
            assert!(
                eventually(Duration::from_secs(10), || w.mgr.sessions["x"]
                    .is_finished())
                .await
            );
            if reaped_first {
                pid_of(&w.open_shell("y", "exec sleep 30").await);
            }
            pid_of(&w.open_shell("x", "exec sleep 30").await);
            tokio::time::sleep(Duration::from_millis(200)).await;
            while w.recv(Duration::from_millis(100)).await.is_some() {}
            assert_eq!(
                w.lifecycle("x"),
                ["opened", "exited", "opened"],
                "reaped_first={reaped_first}"
            );
            w.mgr.shutdown_all().await;
        }
    }

    #[tokio::test]
    async fn a_connection_holds_a_bounded_number_of_live_sessions() {
        let mut w = Wire::new();
        for i in 0..MAX_SESSIONS_PER_CONNECTION {
            let opened = w.open_shell(&format!("s{i}"), "exec sleep 30").await;
            assert_eq!(opened.event, "opened", "{:?}", opened.data);
        }
        assert_eq!(w.mgr.active_count(), MAX_SESSIONS_PER_CONNECTION);

        // One more is refused and spawns nothing.
        let refused = w.open_shell("extra", "exec sleep 30").await;
        assert_eq!(refused.event, "error");
        assert_eq!(refused.data["status"], 429, "{:?}", refused.data);
        assert_eq!(w.mgr.active_count(), MAX_SESSIONS_PER_CONNECTION);
        assert!(!w.mgr.sessions.contains_key("extra"));
        // Reopening an id that is already open replaces it: that is not a new session.
        let replaced = w.open_shell("s0", "exec sleep 30").await;
        assert_eq!(replaced.event, "opened", "{:?}", replaced.data);

        // Closing one frees its slot.
        let closed = w.call("close", json!({ "session_id": "s1" })).await;
        assert_eq!(closed.last().expect("reply").event, "exited");
        let again = w.open_shell("extra", "exec sleep 30").await;
        assert_eq!(again.event, "opened", "{:?}", again.data);
        assert_eq!(w.mgr.active_count(), MAX_SESSIONS_PER_CONNECTION);

        w.mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn sessions_that_exit_on_their_own_free_their_slots() {
        let mut w = Wire::new();
        for i in 0..MAX_SESSIONS_PER_CONNECTION {
            let opened = w.open_shell(&format!("q{i}"), "exit 0").await;
            assert_eq!(opened.event, "opened", "{:?}", opened.data);
        }
        for _ in 0..MAX_SESSIONS_PER_CONNECTION {
            w.wait_for("exited", Duration::from_secs(10))
                .await
                .expect("every child exits");
        }
        // The cap counts live sessions only, so the exited ones never exhaust it.
        for round in 0..3 {
            let opened = w.open_shell(&format!("again{round}"), "exit 0").await;
            assert_eq!(opened.event, "opened", "round {round}: {:?}", opened.data);
            w.wait_for("exited", Duration::from_secs(10))
                .await
                .expect("exits");
        }
        assert_eq!(w.mgr.active_count(), 0);
        assert!(w.mgr.sessions.len() <= MAX_SESSIONS_PER_CONNECTION);
    }

    #[tokio::test]
    async fn a_session_id_is_bounded_and_free_of_control_characters() {
        let mut w = Wire::new();
        for bad in [
            "x".repeat(MAX_SESSION_ID_BYTES + 1),
            "a\u{0}b".to_string(),
            "a\nb".to_string(),
            "esc\u{1b}[0m".to_string(),
        ] {
            let reply = w.open_shell(&bad, "exec sleep 30").await;
            assert_eq!(reply.event, "error", "{bad:?}");
            assert_eq!(reply.data["status"], 400, "{bad:?}");
        }
        assert!(
            w.mgr.sessions.is_empty(),
            "a refused open spawned something"
        );

        // The longest allowed id works, and a blank one gets a generated id.
        let longest = "y".repeat(MAX_SESSION_ID_BYTES);
        let ok = w.open_shell(&longest, "exec sleep 30").await;
        assert_eq!(ok.event, "opened", "{:?}", ok.data);
        let blank = w.open_shell("   ", "exec sleep 30").await;
        assert_eq!(blank.data["session_id"], "term-1");
        w.mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn a_disconnecting_client_stops_a_flooding_child() {
        let mut w = Wire::new();
        // Ignores SIGPIPE, so it would spin on a closed pipe forever unless the hub stops it.
        let pid = pid_of(
            &w.open_shell("orphan", "trap '' PIPE; while true; do echo flood; done")
                .await,
        );
        wait_until_full(&w, 1).await;

        // The client goes away; the manager is not told (no `close`, no `shutdown_all`).
        w.mgr.close_output();
        assert!(
            process_gone(pid).await,
            "child {pid} kept running after the client disconnected"
        );
    }
}
