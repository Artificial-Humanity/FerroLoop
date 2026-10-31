//! Owner-scoped interactive terminal process manager for `/ws/v1` (`channel = "terminal"`, spec §4.2).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::HubWsFrame;
use crate::auth::HubAuthority;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// How long, after the child has exited, the reader tasks get to reach end of file before the
/// `exited` frame is sent anyway. A background job the shell left behind (`sleep 600 &`) keeps
/// the stdout/stderr pipes open, so without this bound the exit would go unreported (and a
/// close or disconnect would hang) until that job ended.
const READER_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

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

/// Per-WebSocket manager of active interactive terminal sessions (spec §4.2).
pub struct TerminalManager {
    sessions: BTreeMap<String, TerminalSession>,
    out_tx: mpsc::UnboundedSender<HubWsFrame>,
    next_id: u64,
}

impl TerminalManager {
    pub fn new(out_tx: mpsc::UnboundedSender<HubWsFrame>) -> Self {
        Self {
            sessions: BTreeMap::new(),
            out_tx,
            next_id: 1,
        }
    }

    pub fn active_count(&self) -> usize {
        self.sessions.len()
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

    /// Handle an incoming `channel == "terminal"` frame from a WebSocket client.
    pub async fn handle_frame(
        &mut self,
        authority: &HubAuthority,
        enable_terminal: bool,
        default_cwd: &Path,
        event: &str,
        data: &Value,
    ) {
        if !authority.is_owner() || !enable_terminal {
            let _ = self.out_tx.send(HubWsFrame {
                channel: "terminal".to_string(),
                event: "error".to_string(),
                data: json!({
                    "status": 403,
                    "message": "owner authority required for terminal sessions",
                }),
            });
            return;
        }

        match event {
            "open" => self.handle_open(default_cwd, data).await,
            "input" => self.handle_input(data).await,
            "resize" => self.handle_resize(data),
            "close" => self.handle_close(data).await,
            other => {
                let _ = self.out_tx.send(HubWsFrame {
                    channel: "terminal".to_string(),
                    event: "error".to_string(),
                    data: json!({
                        "status": 400,
                        "message": format!("unknown terminal event `{other}`"),
                    }),
                });
            }
        }
    }

    async fn handle_open(&mut self, default_cwd: &Path, data: &Value) {
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
                let _ = self.out_tx.send(HubWsFrame {
                    channel: "terminal".to_string(),
                    event: "error".to_string(),
                    data: json!({
                        "status": 400,
                        "message": format!("invalid terminal open payload: {e}"),
                    }),
                });
                return;
            }
        };

        let session_id = match payload
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(id) => id.to_string(),
            None => {
                let id = format!("term-{}", self.next_id);
                self.next_id += 1;
                id
            }
        };

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
            let _ = self.out_tx.send(HubWsFrame {
                channel: "terminal".to_string(),
                event: "error".to_string(),
                data: json!({
                    "status": 400,
                    "session_id": session_id,
                    "message": format!("working directory `{}` does not exist", cwd.display()),
                }),
            });
            return;
        }

        let (program, args) = resolve_command(payload.command.as_ref(), &payload.args);
        let cols = payload.cols.unwrap_or(DEFAULT_COLS).max(1);
        let rows = payload.rows.unwrap_or(DEFAULT_ROWS).max(1);

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
                let _ = self.out_tx.send(HubWsFrame {
                    channel: "terminal".to_string(),
                    event: "error".to_string(),
                    data: json!({
                        "status": 500,
                        "session_id": session_id,
                        "message": format!("failed to spawn `{program}`: {e}"),
                    }),
                });
                return;
            }
        };

        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Emit `terminal:opened` before any stdout/stderr output or exit frames.
        let _ = self.out_tx.send(HubWsFrame {
            channel: "terminal".to_string(),
            event: "opened".to_string(),
            data: json!({
                "session_id": session_id,
                "pid": pid,
                "cwd": cwd.display().to_string(),
                "cols": cols,
                "rows": rows,
            }),
        });

        let (stdout_done_tx, stdout_done_rx) = oneshot::channel::<()>();
        let sid_stdout = session_id.clone();
        let out_tx_stdout = self.out_tx.clone();
        let stdout_task = tokio::spawn(async move {
            if let Some(mut reader) = stdout {
                let mut buf = [0u8; 4096];
                let mut chunker = Utf8Chunker::default();
                loop {
                    let (text, eof) = match reader.read(&mut buf).await {
                        Ok(n) if n > 0 => (chunker.push(&buf[..n]), false),
                        _ => (chunker.finish(), true),
                    };
                    if !text.is_empty()
                        && out_tx_stdout
                            .send(HubWsFrame {
                                channel: "terminal".to_string(),
                                event: "output".to_string(),
                                data: json!({
                                    "session_id": sid_stdout,
                                    "stream": "stdout",
                                    "data": text,
                                    "output": text,
                                }),
                            })
                            .is_err()
                    {
                        break;
                    }
                    if eof {
                        break;
                    }
                }
            }
            let _ = stdout_done_tx.send(());
        });

        let (stderr_done_tx, stderr_done_rx) = oneshot::channel::<()>();
        let sid_stderr = session_id.clone();
        let out_tx_stderr = self.out_tx.clone();
        let stderr_task = tokio::spawn(async move {
            if let Some(mut reader) = stderr {
                let mut buf = [0u8; 4096];
                let mut chunker = Utf8Chunker::default();
                loop {
                    let (text, eof) = match reader.read(&mut buf).await {
                        Ok(n) if n > 0 => (chunker.push(&buf[..n]), false),
                        _ => (chunker.finish(), true),
                    };
                    let text = filter_bash_startup_noise(&text);
                    if !text.is_empty()
                        && out_tx_stderr
                            .send(HubWsFrame {
                                channel: "terminal".to_string(),
                                event: "output".to_string(),
                                data: json!({
                                    "session_id": sid_stderr,
                                    "stream": "stderr",
                                    "data": text,
                                    "output": text,
                                }),
                            })
                            .is_err()
                    {
                        break;
                    }
                    if eof {
                        break;
                    }
                }
            }
            let _ = stderr_done_tx.send(());
        });

        let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
        let sid_wait = session_id.clone();
        let out_tx_wait = self.out_tx.clone();
        let wait_task = tokio::spawn(async move {
            let status_res = tokio::select! {
                res = child.wait() => res,
                _ = &mut kill_rx => {
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
            let _ = tokio::time::timeout(READER_DRAIN_GRACE, drained).await;
            let exit_code = status_res.ok().and_then(|s| s.code()).unwrap_or(-1);
            let _ = out_tx_wait.send(HubWsFrame {
                channel: "terminal".to_string(),
                event: "exited".to_string(),
                data: json!({
                    "session_id": sid_wait,
                    "exit_code": exit_code,
                }),
            });
        });

        self.sessions.insert(
            session_id.clone(),
            TerminalSession {
                session_id,
                pid,
                cwd,
                cols,
                rows,
                stdin,
                kill_tx: Some(kill_tx),
                wait_task: Some(wait_task),
                stdout_task,
                stderr_task,
            },
        );
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

    async fn handle_input(&mut self, data: &Value) {
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
                let _ = self.out_tx.send(HubWsFrame {
                    channel: "terminal".to_string(),
                    event: "error".to_string(),
                    data: json!({
                        "status": 400,
                        "message": format!("invalid terminal input payload: {e}"),
                    }),
                });
                return;
            }
        };

        let Some(key) = self.resolve_session_key(payload.session_id.as_deref()) else {
            let _ = self.out_tx.send(HubWsFrame {
                channel: "terminal".to_string(),
                event: "error".to_string(),
                data: json!({
                    "status": 404,
                    "message": "terminal session not found",
                }),
            });
            return;
        };

        let Some(sess) = self.sessions.get_mut(&key) else {
            return;
        };
        if let Some(ref mut stdin) = sess.stdin {
            if let Err(e) = stdin.write_all(payload.data.as_bytes()).await {
                let _ = self.out_tx.send(HubWsFrame {
                    channel: "terminal".to_string(),
                    event: "error".to_string(),
                    data: json!({
                        "status": 500,
                        "session_id": key,
                        "message": format!("failed writing to terminal stdin: {e}"),
                    }),
                });
                return;
            }
            let _ = stdin.flush().await;
        }
    }

    fn handle_resize(&mut self, data: &Value) {
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
                let _ = self.out_tx.send(HubWsFrame {
                    channel: "terminal".to_string(),
                    event: "error".to_string(),
                    data: json!({
                        "status": 400,
                        "message": format!("invalid terminal resize payload: {e}"),
                    }),
                });
                return;
            }
        };

        let Some(key) = self.resolve_session_key(payload.session_id.as_deref()) else {
            let _ = self.out_tx.send(HubWsFrame {
                channel: "terminal".to_string(),
                event: "error".to_string(),
                data: json!({
                    "status": 404,
                    "message": "terminal session not found",
                }),
            });
            return;
        };

        if let Some(sess) = self.sessions.get_mut(&key) {
            if let Some(c) = payload.cols.filter(|&c| c > 0) {
                sess.cols = c;
            }
            if let Some(r) = payload.rows.filter(|&r| r > 0) {
                sess.rows = r;
            }
            let _ = self.out_tx.send(HubWsFrame {
                channel: "terminal".to_string(),
                event: "resized".to_string(),
                data: json!({
                    "session_id": sess.session_id,
                    "cols": sess.cols,
                    "rows": sess.rows,
                }),
            });
        }
    }

    async fn handle_close(&mut self, data: &Value) {
        #[derive(Debug, Deserialize, Default)]
        struct ClosePayload {
            #[serde(default)]
            session_id: Option<String>,
        }

        let payload: ClosePayload = serde_json::from_value(data.clone()).unwrap_or_default();
        let Some(key) = self.resolve_session_key(payload.session_id.as_deref()) else {
            let _ = self.out_tx.send(HubWsFrame {
                channel: "terminal".to_string(),
                event: "error".to_string(),
                data: json!({
                    "status": 404,
                    "message": "terminal session not found",
                }),
            });
            return;
        };

        if let Some(sess) = self.sessions.remove(&key) {
            sess.shutdown().await;
        }
    }
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

    async fn open_shell(mgr: &mut TerminalManager, id: &str, script: &str) {
        let cwd = std::env::temp_dir();
        mgr.handle_frame(
            &owner(),
            true,
            &cwd,
            "open",
            &json!({ "session_id": id, "command": ["/bin/sh", "-c", script] }),
        )
        .await;
    }

    /// Frames of `event` for `id`, waiting up to `limit` for the first one.
    async fn wait_for(
        rx: &mut mpsc::UnboundedReceiver<HubWsFrame>,
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
        let (tx, mut rx) = mpsc::unbounded_channel();
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
        let (tx, rx) = mpsc::unbounded_channel();
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
        let (tx, mut rx) = mpsc::unbounded_channel();
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
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut mgr = TerminalManager::new(tx);
        let cwd = std::env::temp_dir();
        let peer = HubAuthority::Peer { agent: None };
        mgr.handle_frame(&peer, true, &cwd, "open", &json!({}))
            .await;
        mgr.handle_frame(&owner(), false, &cwd, "open", &json!({}))
            .await;
        for _ in 0..2 {
            let frame = rx.recv().await.unwrap();
            assert_eq!(frame.event, "error");
            assert_eq!(frame.data["status"], 403);
        }
        assert_eq!(mgr.active_count(), 0);
    }
}
