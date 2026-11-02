//! `POST /api/v1/runtimes/update`: run a subscription CLI's own self-update command.
//!
//! Owner only. The body is `{ "runtime": "claude" | "codex" | "grok" | "agy" | "muse" | "all" }`
//! and nothing else: the command is a fixed table in this file ([`UPDATE_COMMANDS`]), the binary is
//! the one [`find_binary`] resolves, there is no shell, and the caller supplies no argument.
//! A runtime with no self-update subcommand (`muse` updates itself in the background) is
//! `unsupported`; this file never guesses a package-manager install line.
//!
//! Each command has a hard deadline ([`UPDATE_TIMEOUT`]); on expiry its whole process group is
//! killed. Output is captured without limit on the child but kept only as the last
//! [`MAX_UPDATE_OUTPUT`] bytes, stripped of terminal escapes, and is returned to the caller and
//! never logged. The output is kept as it is read, so an updater that exits while a detached
//! grandchild still holds its pipe open returns what it printed after [`OUTPUT_GRACE`]. One update
//! runs at a time per hub ([`UpdateGate`]): a second request is `409`. After a run the probe cache
//! is dropped (again when the whole request ends), so the next matrix read reports the new version.
//!
//! # The child's environment and directory
//!
//! An updater runs npm or curl installers and its output goes back to the UI, so it never gets the
//! hub's environment. [`child_env`] clears it and copies only these variables from the hub:
//! `LANG`, `LANGUAGE`, `LC_*`, `TERM`, `USER`, `LOGNAME`, `TMPDIR`, `SHELL`, `XDG_CONFIG_HOME`,
//! `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR`, `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`
//! (and the lower-case forms), `SSL_CERT_FILE`, `SSL_CERT_DIR`, `NODE_EXTRA_CA_CERTS`,
//! `NPM_CONFIG_PREFIX`, `NPM_CONFIG_CACHE`, `CARGO_HOME` and `RUSTUP_HOME`. `PATH` is the directories
//! the binary was resolved with (absolute ones only) and `HOME` is the hub's effective home. A
//! variable whose name starts with `FL_` or ends with `_TOKEN`, `_KEY`, `_SECRET` or `_PASSWORD`
//! never passes, listed or not. The working directory is the effective home (`/` when there is
//! none), never the hub's own.
//!
//! One runtime answers with one result object; `all` runs every runtime in order and answers with
//! an array of them.

use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::HubState;
use crate::api::{HttpRequest, HttpResponse};
use crate::auth::HubAuthority;
use crate::runtime_matrix::{
    CLI_RUNTIMES, VERSION_TIMEOUT, cli_search_paths, exited_unreaped, find_binary, kill_group,
    run_version,
};

/// Longest an update command may run.
pub const UPDATE_TIMEOUT: Duration = Duration::from_secs(300);
/// Most output bytes a result carries (the end of the output).
pub const MAX_UPDATE_OUTPUT: usize = 4096;
/// How long to wait for the output after the command ended, in case a grandchild still holds a
/// pipe open.
const OUTPUT_GRACE: Duration = Duration::from_millis(500);

/// One runtime's self-update command. The program is the runtime's own binary name (the names in
/// [`CLI_RUNTIMES`] are the binaries).
#[derive(Debug, Clone, Copy)]
pub struct UpdateSpec {
    pub runtime: &'static str,
    pub argv: &'static [&'static str],
}

/// The self-update subcommand of each CLI that has one, from the `--help` of the installed CLI:
/// `claude update` ("Check for updates and install if available"), `codex update` ("Update Codex
/// to the latest version"), `grok update` ("Check for updates or install a specific version";
/// with no flag it installs the latest) and `agy update` ("Update CLI"). `muse` has no update
/// subcommand: its launcher script updates itself in the background.
pub const UPDATE_COMMANDS: [UpdateSpec; 4] = [
    UpdateSpec {
        runtime: "claude",
        argv: &["update"],
    },
    UpdateSpec {
        runtime: "codex",
        argv: &["update"],
    },
    UpdateSpec {
        runtime: "grok",
        argv: &["update"],
    },
    UpdateSpec {
        runtime: "agy",
        argv: &["update"],
    },
];

fn spec_for(runtime: &str) -> Option<&'static UpdateSpec> {
    UPDATE_COMMANDS.iter().find(|s| s.runtime == runtime)
}

/// The update command of `runtime` as shown to a person (`claude update`), or `None` when the
/// runtime has none.
pub fn update_command(runtime: &str) -> Option<String> {
    spec_for(runtime).map(|s| format!("{} {}", s.runtime, s.argv.join(" ")))
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRun {
    Exited {
        success: bool,
        code: Option<i32>,
        output: String,
    },
    TimedOut {
        output: String,
    },
    SpawnFailed(String),
}

/// Where the hub looks for CLIs, as the child of an update sees it: the `PATH` directories the
/// binary was resolved with, and the hub's effective home.
#[derive(Debug, Clone, Copy)]
pub struct UpdateEnv<'a> {
    pub path_env: &'a str,
    pub home: Option<&'a Path>,
}

/// What an update needs from the host: run a command, read a version. A test supplies its own.
pub trait UpdateExec: Sync {
    fn run(&self, program: &Path, argv: &[&str], env: &UpdateEnv<'_>) -> CommandRun;
    fn version(&self, program: &Path) -> Result<String, String>;
}

/// The real thing: [`run_command`] with [`UPDATE_TIMEOUT`] and a `--version` probe.
pub struct HostExec;

impl UpdateExec for HostExec {
    fn run(&self, program: &Path, argv: &[&str], env: &UpdateEnv<'_>) -> CommandRun {
        run_command(program, argv, UPDATE_TIMEOUT, env)
    }

    fn version(&self, program: &Path) -> Result<String, String> {
        run_version(program, VERSION_TIMEOUT)
    }
}

/// Result of updating one runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeUpdateResult {
    pub runtime: String,
    pub ok: bool,
    pub version_before: Option<String>,
    pub version_after: Option<String>,
    /// `updated`, `unchanged`, `unknown_version` (the command succeeded but the version after it
    /// cannot be read), `failed`, `unsupported`, `not_installed` or `timeout`.
    pub status: String,
    pub output_tail: String,
}

impl RuntimeUpdateResult {
    fn bare(runtime: &str, status: &str, output_tail: String) -> Self {
        Self {
            runtime: runtime.to_string(),
            ok: false,
            version_before: None,
            version_after: None,
            status: status.to_string(),
            output_tail,
        }
    }
}

/// Update one runtime of [`CLI_RUNTIMES`]. Returns the result and whether a command ran (so the
/// caller knows the probe cache is stale).
pub fn update_runtime(
    runtime: &str,
    path_env: &str,
    home: Option<&Path>,
    exec: &dyn UpdateExec,
) -> (RuntimeUpdateResult, bool) {
    let Some(spec) = spec_for(runtime) else {
        let note = format!("`{runtime}` has no self-update command");
        return (
            RuntimeUpdateResult::bare(runtime, "unsupported", note),
            false,
        );
    };
    let Some(program) = find_binary(spec.runtime, path_env, home) else {
        let note = format!("`{}` not found in PATH or ~/.local/bin", spec.runtime);
        return (
            RuntimeUpdateResult::bare(runtime, "not_installed", note),
            false,
        );
    };
    let version_before = exec.version(&program).ok();
    let run = exec.run(&program, spec.argv, &UpdateEnv { path_env, home });
    let version_after = exec.version(&program).ok();
    let mut result = RuntimeUpdateResult {
        runtime: runtime.to_string(),
        ok: false,
        version_before,
        version_after,
        status: "failed".to_string(),
        output_tail: String::new(),
    };
    match run {
        CommandRun::Exited {
            success: true,
            output,
            ..
        } => {
            result.ok = true;
            result.status = match (&result.version_before, &result.version_after) {
                // Nothing to compare with: the command succeeded, and that is all that is known.
                (_, None) => "unknown_version",
                (Some(before), Some(after)) if before == after => "unchanged",
                _ => "updated",
            }
            .to_string();
            result.output_tail = output;
        }
        CommandRun::Exited { code, output, .. } => {
            result.output_tail = if output.is_empty() {
                match code {
                    Some(code) => format!("update exited with status {code}"),
                    None => "update was ended by a signal".to_string(),
                }
            } else {
                output
            };
        }
        CommandRun::TimedOut { output } => {
            result.status = "timeout".to_string();
            result.output_tail = if output.is_empty() {
                format!("update timed out after {}s", UPDATE_TIMEOUT.as_secs())
            } else {
                output
            };
        }
        CommandRun::SpawnFailed(err) => result.output_tail = err,
    }
    (result, true)
}

/// Environment variables copied from the hub to an updater (see the module doc). `LC_*` is also
/// copied.
const ENV_ALLOW: &[&str] = &[
    "LANG",
    "LANGUAGE",
    "TERM",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "SHELL",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "NPM_CONFIG_PREFIX",
    "NPM_CONFIG_CACHE",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

/// A name that never reaches an updater, whatever else says.
fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("FL_")
        || ["_TOKEN", "_KEY", "_SECRET", "_PASSWORD"]
            .iter()
            .any(|suffix| upper.ends_with(suffix))
}

/// The whole environment of an updater: the allowed variables of `host`, `PATH` (the absolute
/// directories of `path_env`; unset when there are none, so the shell's own default applies) and
/// `HOME`.
pub fn child_env(
    path_env: &str,
    home: Option<&Path>,
    host: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = host
        .into_iter()
        .filter(|(name, _)| {
            name.to_str().is_some_and(|name| {
                !is_secret_name(name) && (ENV_ALLOW.contains(&name) || name.starts_with("LC_"))
            })
        })
        .collect();
    let path = path_env
        .split(':')
        .filter(|dir| Path::new(dir).is_absolute())
        .collect::<Vec<_>>()
        .join(":");
    if !path.is_empty() {
        env.push(("PATH".into(), path.into()));
    }
    if let Some(home) = home {
        env.push(("HOME".into(), home.as_os_str().to_owned()));
    }
    env
}

/// Run `program argv` with no shell and no stdin, killing its process group at `timeout`. The
/// child leads its own group, runs in the effective home (`/` when there is none) and sees only
/// [`child_env`]. Both pipes are read to the end whatever their size, so a chatty updater never
/// blocks on a full pipe; only the end of each is kept.
pub fn run_command(
    program: &Path,
    argv: &[&str],
    timeout: Duration,
    env: &UpdateEnv<'_>,
) -> CommandRun {
    let mut cmd = Command::new(program);
    cmd.args(argv)
        .env_clear()
        .envs(child_env(env.path_env, env.home, std::env::vars_os()))
        .current_dir(
            env.home
                .filter(|h| h.is_absolute() && h.is_dir())
                .unwrap_or(Path::new("/")),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return CommandRun::SpawnFailed(format!("spawn failed: {e}")),
    };
    let pid = child.id();
    let out = drain_tail(child.stdout.take(), MAX_UPDATE_OUTPUT);
    let err = drain_tail(child.stderr.take(), MAX_UPDATE_OUTPUT);

    let deadline = Instant::now() + timeout;
    let exited = loop {
        match exited_unreaped(&mut child) {
            Ok(true) => break true,
            Ok(false) if Instant::now() >= deadline => break false,
            Ok(false) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return CommandRun::SpawnFailed(format!("wait failed: {e}"));
            }
        }
    };
    let status = if exited {
        // A normal exit leaves the group alone: the updater may finish detached work.
        child.wait()
    } else {
        kill_group(pid, &mut child)
    };
    // One grace period for both pipes. A grandchild that keeps one open does not hold the reply:
    // what was read by then is the output.
    let grace = Instant::now() + OUTPUT_GRACE;
    out.wait_until(grace);
    err.wait_until(grace);
    let output = combine_output(out.snapshot(), err.snapshot());
    if !exited {
        return CommandRun::TimedOut { output };
    }
    match status {
        Ok(status) => CommandRun::Exited {
            success: status.success(),
            code: status.code(),
            output,
        },
        Err(e) => CommandRun::SpawnFailed(format!("wait failed: {e}")),
    }
}

/// The last bytes a reader thread has read from a pipe, readable while it still reads.
struct Tail {
    bytes: Arc<Mutex<Vec<u8>>>,
    cap: usize,
    /// Disconnects when the reader reaches the end of the pipe.
    ended: mpsc::Receiver<()>,
}

impl Tail {
    /// Wait for the end of the pipe, no later than `deadline`.
    fn wait_until(&self, deadline: Instant) {
        let _ = self
            .ended
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
    }

    /// The last `cap` bytes read so far.
    fn snapshot(&self) -> Vec<u8> {
        let bytes = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
        bytes[bytes.len().saturating_sub(self.cap)..].to_vec()
    }
}

/// Read `pipe` on a thread that is never joined, keeping the last `cap` bytes where [`Tail`] can
/// see them as they arrive.
fn drain_tail<R: Read + Send + 'static>(pipe: Option<R>, cap: usize) -> Tail {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let (done, ended) = mpsc::channel::<()>();
    let shared = Arc::clone(&bytes);
    std::thread::spawn(move || {
        // Dropped on return, which ends the wait.
        let _done = done;
        let Some(mut pipe) = pipe else { return };
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut tail = shared.lock().unwrap_or_else(PoisonError::into_inner);
                    tail.extend_from_slice(&chunk[..n]);
                    if tail.len() > cap * 2 {
                        let excess = tail.len() - cap;
                        tail.drain(..excess);
                    }
                }
            }
        }
    });
    Tail { bytes, cap, ended }
}

/// Stdout then stderr, as text for a person: terminal escapes and control characters removed
/// (`\r` is a line break), at most [`MAX_UPDATE_OUTPUT`] bytes from the end, on a character
/// boundary, trimmed.
fn combine_output(stdout: Vec<u8>, stderr: Vec<u8>) -> String {
    let mut raw = stdout;
    if !raw.is_empty() && !raw.ends_with(b"\n") && !stderr.is_empty() {
        raw.push(b'\n');
    }
    raw.extend_from_slice(&stderr);
    let text = String::from_utf8_lossy(&raw);
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // CSI: ESC [ params final-byte. Anything else after ESC is dropped with it.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\r' => clean.push('\n'),
            '\n' | '\t' => clean.push(c),
            c if c.is_control() => {}
            c => clean.push(c),
        }
    }
    let mut start = clean.len().saturating_sub(MAX_UPDATE_OUTPUT);
    while !clean.is_char_boundary(start) {
        start += 1;
    }
    clean[start..].trim().to_string()
}

/// Admits one update at a time.
#[derive(Debug, Default)]
pub struct UpdateGate {
    running: AtomicBool,
}

/// Holds the [`UpdateGate`] until dropped, including by panic.
pub struct UpdateGuard<'a>(&'a UpdateGate);

impl UpdateGate {
    /// The guard, or `None` while an update runs.
    pub fn try_begin(&self) -> Option<UpdateGuard<'_>> {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| UpdateGuard(self))
    }
}

impl Drop for UpdateGuard<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
    }
}

fn bad_request(message: impl Into<String>) -> HttpResponse {
    HttpResponse::json(400, json!({ "error": message.into() }))
}

/// The runtime the body names: one of [`CLI_RUNTIMES`], or `all`.
fn parse_runtime(req: &HttpRequest) -> Result<String, HttpResponse> {
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| bad_request(format!("invalid JSON body: {e}")))?;
    let Some(object) = body.as_object() else {
        return Err(bad_request("body must be a JSON object"));
    };
    if let Some(key) = object.keys().find(|k| k.as_str() != "runtime") {
        return Err(bad_request(format!("unknown field `{key}`")));
    }
    let Some(Value::String(raw)) = object.get("runtime") else {
        return Err(bad_request("`runtime` must be a string"));
    };
    let runtime = raw.trim().to_ascii_lowercase();
    if runtime == "all" || CLI_RUNTIMES.contains(&runtime.as_str()) {
        Ok(runtime)
    } else {
        Err(bad_request(format!(
            "unknown runtime; expected one of {} or `all`",
            CLI_RUNTIMES.join(", ")
        )))
    }
}

/// `POST /api/v1/runtimes/update`. Owner only. `all` runs every CLI's update in turn, so the
/// request can be held for several minutes (each command may take [`UPDATE_TIMEOUT`]).
pub fn handle_post_update(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    post_update_with(state, authority, req, &HostExec)
}

pub(crate) fn post_update_with(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
    exec: &dyn UpdateExec,
) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(403, json!({ "error": "owner authority required" }));
    }
    let runtime = match parse_runtime(req) {
        Ok(runtime) => runtime,
        Err(resp) => return resp,
    };
    let Some(_running) = state.update_gate.try_begin() else {
        return HttpResponse::json(
            409,
            json!({ "error": "a CLI update is already running; retry when it ends" }),
        );
    };
    let (path_env, home) = cli_search_paths(state);
    let names: Vec<&str> = if runtime == "all" {
        CLI_RUNTIMES.to_vec()
    } else {
        vec![runtime.as_str()]
    };
    let mut results = Vec::new();
    for name in names {
        let (result, ran) = update_runtime(name, &path_env, home.as_deref(), exec);
        if ran {
            // The versions the last probe holds are out of date.
            state.probe_gate.invalidate();
        }
        results.push(result);
    }
    // A probe that began before the first update ended may have cached pre-update versions.
    state.probe_gate.invalidate();
    let body = if runtime == "all" {
        serde_json::to_value(&results)
    } else {
        serde_json::to_value(&results[0])
    };
    HttpResponse::json(200, body.unwrap_or_else(|_| json!({})))
}
