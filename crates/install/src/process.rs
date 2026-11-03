//! Running a child process the way the source-build installer needs (#205 A4b): argument
//! vectors only (no shell), an environment the caller spells out in full, its own process
//! group (so a timeout kills everything it started), a deadline, and output that is bounded
//! both on disk (the build log) and in memory (what the caller parses).

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The most bytes one run appends to its log; the rest is read and dropped.
pub const MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;
/// How long to wait for the output pipes after the child (and its group) is gone.
const DRAIN_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(15);

/// What to run.
pub struct Spec<'a> {
    pub program: &'a Path,
    pub args: Vec<OsString>,
    pub cwd: &'a Path,
    /// The child's whole environment. Nothing is inherited.
    pub env: &'a [(OsString, OsString)],
    pub timeout: Duration,
    /// Append everything the child prints (stdout and stderr, interleaved) here.
    pub log: Option<&'a Path>,
    /// Keep this many bytes of the output in memory.
    pub capture_cap: usize,
}

/// How a run ended.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub output: Vec<u8>,
    /// Output beyond [`MAX_LOG_BYTES`] was dropped from the log.
    pub log_truncated: bool,
}

impl Outcome {
    pub fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }

    /// `exit status 101`, `killed by signal 9`, `timed out`.
    pub fn describe(&self) -> String {
        if self.timed_out {
            "timed out and was killed".to_string()
        } else if let Some(c) = self.code {
            format!("exit status {c}")
        } else if let Some(s) = self.signal {
            format!("killed by signal {s}")
        } else {
            "ended abnormally".to_string()
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }
}

struct Sink {
    file: Option<File>,
    written: u64,
    truncated: bool,
    mem: Vec<u8>,
    mem_cap: usize,
}

impl Sink {
    fn push(&mut self, data: &[u8]) {
        if let Some(f) = &mut self.file {
            let room = MAX_LOG_BYTES.saturating_sub(self.written) as usize;
            let n = room.min(data.len());
            if n > 0 && f.write_all(&data[..n]).is_ok() {
                self.written += n as u64;
            }
            if n < data.len() {
                self.truncated = true;
            }
        }
        let room = self.mem_cap.saturating_sub(self.mem.len());
        self.mem.extend_from_slice(&data[..room.min(data.len())]);
    }
}

fn pump(mut r: impl Read + Send + 'static, sink: Arc<Mutex<Sink>>, done: mpsc::Sender<()>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut s) = sink.lock() {
                        s.push(&buf[..n]);
                    }
                }
            }
        }
        let _ = done.send(());
    });
}

/// Whether `pid` has exited, WITHOUT reaping it: the zombie keeps the pid (and so the process
/// group number) reserved until the caller has killed the group.
fn exited_unreaped(pid: i32) -> bool {
    // SAFETY: a zeroed siginfo_t is a valid out-parameter; WNOWAIT leaves the child waitable.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        r == 0 && info.si_pid() != 0
    }
}

fn kill_group(pid: i32) {
    // SAFETY: signalling a process group; the leader is still unreaped, so the number is ours.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
}

/// Run `spec` to completion or to its deadline. On the deadline, and also when the child has
/// exited, the child's whole process group is killed, so nothing it started outlives the run.
pub fn run(spec: &Spec<'_>) -> io::Result<Outcome> {
    let file = match spec.log {
        Some(p) => Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(p)?,
        ),
        None => None,
    };
    let mut cmd = Command::new(spec.program);
    cmd.args(&spec.args)
        .current_dir(spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;
    let sink = Arc::new(Mutex::new(Sink {
        file,
        written: 0,
        truncated: false,
        mem: Vec::new(),
        mem_cap: spec.capture_cap,
    }));
    let (tx, rx) = mpsc::channel();
    let mut readers = 0;
    if let Some(o) = child.stdout.take() {
        pump(o, sink.clone(), tx.clone());
        readers += 1;
    }
    if let Some(e) = child.stderr.take() {
        pump(e, sink.clone(), tx.clone());
        readers += 1;
    }
    drop(tx);

    let deadline = Instant::now() + spec.timeout;
    let mut timed_out = false;
    loop {
        if exited_unreaped(pid) {
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        std::thread::sleep(POLL);
    }
    kill_group(pid);
    let status = child.wait()?;
    for _ in 0..readers {
        if rx.recv_timeout(DRAIN_WAIT).is_err() {
            break;
        }
    }
    let (output, truncated) = match sink.lock() {
        Ok(mut s) => (std::mem::take(&mut s.mem), s.truncated),
        Err(_) => (Vec::new(), false),
    };
    Ok(Outcome {
        code: status.code(),
        signal: status.signal(),
        timed_out,
        output,
        log_truncated: truncated,
    })
}

/// The first absolute-directory entry of `path_var` holding an executable file `name`.
/// A relative entry (including an empty one, which means the current directory) is skipped:
/// a program found relative to a build directory is not a program the user chose.
pub fn which(name: &str, path_var: &OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path_var)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// `path_var` without its relative entries.
pub fn absolute_path_entries(path_var: &OsStr) -> OsString {
    let kept: Vec<PathBuf> = std::env::split_paths(path_var)
        .filter(|d| d.is_absolute())
        .collect();
    std::env::join_paths(kept).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration, dir: &Path) -> io::Result<Outcome> {
        run(&Spec {
            program: Path::new("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            cwd: dir,
            env: &[],
            timeout,
            log: None,
            capture_cap: 1024,
        })
    }

    #[test]
    fn the_child_gets_exactly_the_environment_given() {
        let d = tempfile::tempdir().unwrap();
        let env = [(OsString::from("ONLY"), OsString::from("this"))];
        let o = run(&Spec {
            program: Path::new("/usr/bin/env"),
            args: vec![],
            cwd: d.path(),
            env: &env,
            timeout: Duration::from_secs(10),
            log: None,
            capture_cap: 1024,
        })
        .unwrap();
        assert!(o.success());
        assert_eq!(o.text().trim(), "ONLY=this");
    }

    #[test]
    fn output_is_bounded_in_memory_and_in_the_log() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("l.log");
        let o = run(&Spec {
            program: Path::new("/bin/sh"),
            args: vec!["-c".into(), "yes x | head -c 100000".into()],
            cwd: d.path(),
            env: &[],
            timeout: Duration::from_secs(10),
            log: Some(&log),
            capture_cap: 100,
        })
        .unwrap();
        assert!(o.success());
        assert_eq!(o.output.len(), 100);
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 100_000);
        assert!(!o.log_truncated);
    }

    #[test]
    fn a_timeout_kills_the_whole_process_group() {
        let d = tempfile::tempdir().unwrap();
        let pidfile = d.path().join("child.pid");
        let script = format!("sleep 300 & echo $! > '{}'; wait", pidfile.display());
        let t0 = Instant::now();
        let o = sh(&script, Duration::from_millis(400), d.path()).unwrap();
        assert!(o.timed_out, "{o:?}");
        assert!(t0.elapsed() < Duration::from_secs(20));
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The grandchild is gone (reaped by init or already dead), not an orphan.
        let mut gone = false;
        for _ in 0..100 {
            if !crate::lock::pid_alive(pid) || is_zombie(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(gone, "pid {pid} outlived the timeout");
    }

    fn is_zombie(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| s.rsplit(") ").next().is_some_and(|r| r.starts_with('Z')))
            .unwrap_or(true)
    }

    #[test]
    fn a_straggler_left_behind_by_a_finished_child_is_killed_too() {
        let d = tempfile::tempdir().unwrap();
        let pidfile = d.path().join("child.pid");
        let script = format!(
            "sleep 300 >/dev/null 2>&1 & echo $! > '{}'",
            pidfile.display()
        );
        let o = sh(&script, Duration::from_secs(10), d.path()).unwrap();
        assert!(o.success(), "{o:?}");
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut gone = false;
        for _ in 0..100 {
            if !crate::lock::pid_alive(pid) || is_zombie(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(gone, "pid {pid} outlived its parent");
    }

    #[test]
    fn which_skips_relative_path_entries() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("tool");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let abs = d.path().as_os_str().to_os_string();
        assert_eq!(which("tool", &abs), Some(bin));
        assert_eq!(which("tool", OsStr::new(".:bin:")), None);
        assert_eq!(
            absolute_path_entries(OsStr::new(".:/usr/bin::bin")),
            OsString::from("/usr/bin")
        );
    }
}
