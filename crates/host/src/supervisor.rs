//! Per-session supervisor. This process is the only `waitpid(-1)` waiter.
//!
//! Linux sets `PR_SET_CHILD_SUBREAPER` before the vendor is created, so orphans of
//! this session reparent here. macOS has no subreaper: a descendant that calls
//! `setsid` is reparented to launchd and remains a best-effort residual.
//! Linux without a cgroup also leaves anything still running after both this
//! supervisor and the daemon are SIGKILLed.
//!
//! Control EOF requests TERM-to-KILL cleanup. It does not prove the subtree exited.
//! Leader exit is reported on its own and does not drop this owner.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fl_host_proto::HostError;

use crate::identity::{LeaderAnchor, process_identity, signal_anchored_group};
use crate::limits::{CLEANUP_DEADLINE, HANDOFF_TIMEOUT, TERM_GRACE};
use crate::protocol::{DaemonMsg, SupervisorMsg, from_line, to_line};

const CONTROL_R: i32 = 3;
const STATUS_W: i32 = 4;
const VENDOR_STDIN: i32 = 5;
const VENDOR_STDOUT: i32 = 6;
const VENDOR_STDERR: i32 = 7;

static CLEANUP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: i32) {
    CLEANUP.store(true, Ordering::Relaxed);
}

struct ExecPrep {
    argv: Vec<CString>,
    envp: Vec<CString>,
    cwd: CString,
}

pub fn run() -> Result<(), HostError> {
    install_signals()?;
    if unsafe { libc::setsid() } < 0 {
        let _ = write_msg(&SupervisorMsg::Fail {
            error: "setsid".into(),
        });
        return Err(HostError::Protocol("setsid".into()));
    }
    #[cfg(target_os = "linux")]
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        let _ = write_msg(&SupervisorMsg::Fail {
            error: "subreaper".into(),
        });
        return Err(HostError::Protocol("subreaper".into()));
    }
    let spawn = match read_msg(CONTROL_R, HANDOFF_TIMEOUT)? {
        Some(DaemonMsg::Spawn {
            argv,
            cwd,
            env,
            session: _,
            epoch: _,
        }) => (argv, cwd, env),
        _ => {
            let _ = write_msg(&SupervisorMsg::Fail {
                error: "spawn envelope".into(),
            });
            return Err(HostError::Protocol("spawn envelope".into()));
        }
    };
    let prep = match prep_exec(spawn.0, spawn.1, spawn.2) {
        Ok(prep) => prep,
        Err(err) => {
            let _ = write_msg(&SupervisorMsg::Fail { error: err });
            return Err(HostError::Protocol("exec prep".into()));
        }
    };
    let (release_r, release_w) = pipe_cloexec()?;
    let (err_r, err_w) = pipe_cloexec()?;
    set_cloexec(release_r.as_raw_fd())?;
    set_cloexec(err_w.as_raw_fd())?;
    let release_r = elevate(release_r)?;
    let err_w = elevate(err_w)?;
    let vendor = unsafe { libc::fork() };
    if vendor < 0 {
        let _ = write_msg(&SupervisorMsg::Fail {
            error: "fork".into(),
        });
        return Err(HostError::Io(io::Error::last_os_error().to_string()));
    }
    if vendor == 0 {
        become_vendor(&prep, release_r.as_raw_fd(), err_w.as_raw_fd());
    }
    drop(release_r);
    drop(err_w);
    // The vendor child owns the stdio ends after the fork. Dropping these copies
    // is what lets the daemon observe stdout EOF when the vendor exits.
    unsafe {
        libc::close(VENDOR_STDIN);
        libc::close(VENDOR_STDOUT);
        libc::close(VENDOR_STDERR);
    }
    // The child calls setsid before setpgid. A parent setpgid here would make it a
    // process-group leader first, and setsid would then fail.
    let vendor_pid = vendor as u32;
    let ident = match wait_vendor_group(vendor_pid) {
        Ok(ident) => ident,
        Err(err) => {
            abort_vendor(vendor);
            let _ = write_msg(&SupervisorMsg::Fail {
                error: err.to_string(),
            });
            reap_pid(vendor);
            return Err(err);
        }
    };
    let self_ident = process_identity(std::process::id())?;
    write_msg(&SupervisorMsg::Identity {
        vendor_pid,
        vendor_start: ident.start,
        pgid: ident.pgid,
        supervisor_pid: self_ident.pid,
        supervisor_start: self_ident.start,
    })?;
    let go = read_msg(CONTROL_R, HANDOFF_TIMEOUT)?;
    if !matches!(go, Some(DaemonMsg::Go)) {
        abort_vendor(vendor);
        let _ = write_msg(&SupervisorMsg::Exec {
            ok: false,
            error: "handshake".into(),
        });
        reap_pid(vendor);
        return Err(HostError::Protocol("handshake".into()));
    }
    let mut release = unsafe { std::fs::File::from_raw_fd(release_w.into_raw_fd()) };
    let wrote = release.write_all(b"G");
    drop(release);
    if wrote.is_err() {
        abort_vendor(vendor);
        let _ = write_msg(&SupervisorMsg::Exec {
            ok: false,
            error: "release".into(),
        });
        reap_pid(vendor);
        return Err(HostError::Protocol("release".into()));
    }
    let exec_ok = read_exec_result(err_r.as_raw_fd())?;
    drop(err_r);
    if !exec_ok {
        abort_vendor(vendor);
        write_msg(&SupervisorMsg::Exec {
            ok: false,
            error: "exec".into(),
        })?;
        reap_pid(vendor);
        return Ok(());
    }
    write_msg(&SupervisorMsg::Exec {
        ok: true,
        error: String::new(),
    })?;
    supervise(vendor_pid, ident.start, ident.pgid)?;
    Ok(())
}

fn supervise(vendor_pid: u32, vendor_start: u64, pgid: u32) -> Result<(), HostError> {
    let mut anchor = LeaderAnchor {
        pid: vendor_pid,
        start: vendor_start,
        pgid,
        reaped: false,
    };
    let mut reported_leader = false;
    let mut reported: BTreeSet<u32> = BTreeSet::new();
    reported.insert(vendor_pid);
    let mut cleanup_started: Option<Instant> = None;
    let mut saw_child = true;
    // One empty wait is not adoption. A child can be between the exiting leader and
    // this subreaper. Two empty passes, with the poll below between them, are the
    // stable observation that no owned child remains.
    let mut empty_streak = 0u8;
    loop {
        if (CLEANUP.load(Ordering::Relaxed) || control_closed()) && cleanup_started.is_none() {
            cleanup_started = Some(Instant::now());
        }
        if let Some(started) = cleanup_started {
            let signal = if started.elapsed() >= TERM_GRACE {
                libc::SIGKILL
            } else {
                libc::SIGTERM
            };
            // Repeat while the leader is unreaped so a group signal stays anchored, and
            // again after adoption so a descendant that reparents during cleanup is pinned.
            if !anchor.reaped {
                let _ = signal_anchored_group(Some(&anchor), signal);
            }
            signal_children(vendor_pid, signal);
        }
        match wait_one() {
            Wait::Status { pid, code, signal } => {
                saw_child = true;
                empty_streak = 0;
                if pid == vendor_pid && !reported_leader {
                    reported_leader = true;
                    anchor.reaped = true;
                    let _ = write_msg(&SupervisorMsg::Leader { code, signal });
                } else if reported.insert(pid) {
                    let _ = write_msg(&SupervisorMsg::Descendant {
                        pid,
                        code,
                        signal,
                        adopted: true,
                    });
                }
            }
            Wait::Empty => {
                if cleanup_started.is_some() && direct_children().is_empty() {
                    empty_streak = empty_streak.saturating_add(1);
                    if empty_streak >= 2 {
                        let _ = write_msg(&SupervisorMsg::Cleanup { complete: true });
                        return Ok(());
                    }
                } else {
                    empty_streak = 0;
                }
                saw_child = false;
            }
            Wait::Again => {
                empty_streak = 0;
            }
        }
        if let Some(started) = cleanup_started
            && started.elapsed() >= CLEANUP_DEADLINE
        {
            let children = direct_children();
            let complete = children.is_empty() && !saw_child;
            let _ = write_msg(&SupervisorMsg::Cleanup { complete });
            return Ok(());
        }
        let mut pfd = libc::pollfd {
            fd: CONTROL_R,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd. A short timeout also notices the SIGTERM flag.
        unsafe { libc::poll(&mut pfd, 1, 30) };
    }
}

enum Wait {
    Status {
        pid: u32,
        code: Option<i32>,
        signal: Option<i32>,
    },
    Empty,
    Again,
}

fn wait_vendor_group(vendor_pid: u32) -> Result<crate::identity::ProcessIdentity, HostError> {
    let deadline = Instant::now() + HANDOFF_TIMEOUT;
    loop {
        match process_identity(vendor_pid) {
            Ok(ident) if ident.pid == vendor_pid && ident.pgid == vendor_pid => return Ok(ident),
            Ok(_) if Instant::now() >= deadline => {
                return Err(HostError::Protocol("vendor group".into()));
            }
            Err(err) if Instant::now() >= deadline => return Err(err),
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_one() -> Wait {
    let mut status = 0;
    // SAFETY: this supervisor is the sole waiter for its subtree. waitpid(-1) cannot
    // observe another session. WNOHANG returns immediately when nobody has exited.
    let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
    if pid == 0 {
        return Wait::Again;
    }
    if pid < 0 {
        let err = io::Error::last_os_error();
        return if err.raw_os_error() == Some(libc::ECHILD) {
            Wait::Empty
        } else {
            Wait::Again
        };
    }
    let code = if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else {
        None
    };
    let signal = if libc::WIFSIGNALED(status) {
        Some(libc::WTERMSIG(status))
    } else {
        None
    };
    Wait::Status {
        pid: pid as u32,
        code,
        signal,
    }
}

fn control_closed() -> bool {
    let mut pfd = libc::pollfd {
        fd: CONTROL_R,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll on the control reader. POLLHUP without data is EOF.
    let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
    if rc <= 0 {
        return false;
    }
    if pfd.revents & libc::POLLIN != 0 {
        let mut buf = [0u8; 64];
        // SAFETY: `buf` is writable. A zero read is EOF. Extra bytes are discarded;
        // cleanup is requested by EOF, not by a payload.
        let n = unsafe { libc::read(CONTROL_R, buf.as_mut_ptr().cast(), buf.len()) };
        return n == 0;
    }
    pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

fn signal_children(vendor_pid: u32, signal: i32) {
    for pid in direct_children() {
        if pid == vendor_pid {
            continue;
        }
        signal_owned(pid, signal);
    }
}

fn signal_owned(pid: u32, signal: i32) {
    #[cfg(target_os = "linux")]
    {
        let fd = match pidfd_open(pid) {
            Some(fd) => fd,
            None => return,
        };
        // SAFETY: `fd` is a pidfd for `pid` opened while this supervisor still owns the
        // unreaped child. pidfd_send_signal does not retarget a reused numeric pid.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                signal,
                std::ptr::null_mut::<libc::siginfo_t>(),
                0,
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // macOS has no pidfd. This child is still unreaped and listed as our direct
        // child, which is the only pin available. Detached descendants are not in the list.
        unsafe { libc::kill(pid as i32, signal) };
    }
}

#[cfg(target_os = "linux")]
fn pidfd_open(pid: u32) -> Option<OwnedFd> {
    // SAFETY: pidfd_open returns a new fd or -1.
    let rc = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0) };
    if rc < 0 {
        None
    } else {
        Some(unsafe { OwnedFd::from_raw_fd(rc as i32) })
    }
}

fn direct_children() -> Vec<u32> {
    let self_pid = std::process::id();
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return out;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if pid == self_pid {
                continue;
            }
            if let Ok(ident) = process_identity(pid)
                && ident.ppid == self_pid
            {
                out.push(pid);
            }
        }
        out
    }
    #[cfg(target_os = "macos")]
    {
        // proc_pidinfo validates identity and does not pin a signal. Only pids whose
        // ppid is still this supervisor are owned children. setsid descendants are absent.
        let mut buf = vec![0i32; 4096];
        let bytes = unsafe {
            libc::proc_listallpids(
                buf.as_mut_ptr().cast(),
                (buf.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        if bytes <= 0 {
            return Vec::new();
        }
        let count = (bytes as usize) / std::mem::size_of::<i32>();
        let mut out = Vec::new();
        for pid in buf.into_iter().take(count) {
            if pid <= 0 || pid as u32 == self_pid {
                continue;
            }
            if let Ok(ident) = process_identity(pid as u32)
                && ident.ppid == self_pid
            {
                out.push(pid as u32);
            }
        }
        out
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = self_pid;
        Vec::new()
    }
}

fn abort_vendor(pid: libc::pid_t) {
    signal_owned(pid as u32, libc::SIGKILL);
}

fn reap_pid(pid: libc::pid_t) {
    let mut status = 0;
    // SAFETY: `pid` is the vendor this supervisor forked and has not reaped.
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

fn become_vendor(prep: &ExecPrep, release_r: i32, err_w: i32) -> ! {
    // setsid creates the session and makes this process the leader of a new group
    // whose id is its pid. setpgid after that fails with EPERM.
    if unsafe { libc::setsid() } < 0 {
        let _ = write_byte(err_w, b'E');
        unsafe { libc::_exit(127) };
    }
    unsafe {
        libc::dup2(VENDOR_STDIN, 0);
        libc::dup2(VENDOR_STDOUT, 1);
        libc::dup2(VENDOR_STDERR, 2);
    }
    close_except(&[0, 1, 2, release_r, err_w]);
    let mut byte = [0u8; 1];
    let n = loop {
        // SAFETY: one-byte read on the release pipe. EINTR retries. Any other result decides exec.
        let n = unsafe { libc::read(release_r, byte.as_mut_ptr().cast(), 1) };
        if n < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break n;
    };
    if n != 1 || byte[0] != b'G' {
        let _ = write_byte(err_w, b'X');
        unsafe { libc::_exit(1) };
    }
    unsafe { libc::close(release_r) };
    if unsafe { libc::chdir(prep.cwd.as_ptr()) } != 0 {
        let _ = write_byte(err_w, b'C');
        unsafe { libc::_exit(1) };
    }
    let mut argv: Vec<*const libc::c_char> = prep.argv.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = prep.envp.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());
    // SAFETY: argv and envp are NUL-terminated pointer arrays into `prep`, which the
    // child address space still owns. execve does not return on success.
    unsafe { libc::execve(prep.argv[0].as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    let _ = write_byte(err_w, b'E');
    unsafe { libc::_exit(127) };
}

fn prep_exec(
    argv: Vec<String>,
    cwd: Vec<u8>,
    env: Vec<(String, String)>,
) -> Result<ExecPrep, String> {
    if argv.is_empty() {
        return Err("argv".into());
    }
    let argv = argv
        .into_iter()
        .map(|arg| CString::new(arg).map_err(|_| "argv nul".to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    let cwd = CString::new(cwd).map_err(|_| "cwd nul".to_owned())?;
    let mut envp = Vec::new();
    for (key, value) in env {
        if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
            return Err("env".into());
        }
        envp.push(CString::new(format!("{key}={value}")).map_err(|_| "env nul".to_owned())?);
    }
    Ok(ExecPrep { argv, envp, cwd })
}

fn read_exec_result(fd: i32) -> Result<bool, HostError> {
    let deadline = Instant::now() + HANDOFF_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: one pollfd on the CLOEXEC exec-error pipe.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(HostError::Io(io::Error::last_os_error().to_string()));
        }
        if rc == 0 {
            return Ok(false);
        }
        let mut byte = [0u8; 1];
        // SAFETY: one-byte read. EOF with no byte means execve closed the CLOEXEC pipe.
        let n = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
        if n < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(HostError::Io(io::Error::last_os_error().to_string()));
        }
        return Ok(n == 0);
    }
}

fn install_signals() -> Result<(), HostError> {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_term as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0 {
            return Err(HostError::Io(io::Error::last_os_error().to_string()));
        }
    }
    Ok(())
}

fn write_msg(msg: &SupervisorMsg) -> Result<(), HostError> {
    let bytes = to_line(msg).map_err(HostError::Protocol)?;
    write_all(STATUS_W, &bytes)
}

fn write_all(fd: i32, mut bytes: &[u8]) -> Result<(), HostError> {
    while !bytes.is_empty() {
        // SAFETY: `bytes` is readable. EINTR retries. A closed status pipe returns EPIPE
        // because SIGPIPE is ignored in this process.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(HostError::Io(io::Error::last_os_error().to_string()));
        }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

fn write_byte(fd: i32, byte: u8) -> io::Result<()> {
    let buf = [byte];
    loop {
        // SAFETY: one-byte write of a stack buffer.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), 1) };
        if n < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
}

fn read_msg(fd: i32, timeout: Duration) -> Result<Option<DaemonMsg>, HostError> {
    let Some(line) = read_line(fd, timeout)? else {
        return Ok(None);
    };
    from_line(&line).map(Some).map_err(HostError::Protocol)
}

fn read_line(fd: i32, timeout: Duration) -> Result<Option<Vec<u8>>, HostError> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(HostError::FrameTimeout);
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: one pollfd. The timeout is the remaining handshake budget.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(HostError::Io(io::Error::last_os_error().to_string()));
        }
        if rc == 0 {
            return Err(HostError::FrameTimeout);
        }
        // SAFETY: one-byte read into `byte`.
        let n = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
        if n < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(HostError::Io(io::Error::last_os_error().to_string()));
        }
        if n == 0 {
            return Ok(None);
        }
        if byte[0] == b'\n' {
            return Ok(Some(buf));
        }
        if buf.len() >= 1024 * 1024 {
            return Err(HostError::Protocol("supervisor line".into()));
        }
        buf.push(byte[0]);
    }
}

fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    #[cfg(target_os = "linux")]
    {
        // SAFETY: pipe2 initializes both fds or fails without leaking.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: pipe initializes both fds. The following fcntl sets cloexec before return.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        set_cloexec(fds[0]).map_err(|err| io::Error::other(err.to_string()))?;
        set_cloexec(fds[1]).map_err(|err| io::Error::other(err.to_string()))?;
    }
    Ok((unsafe { OwnedFd::from_raw_fd(fds[0]) }, unsafe {
        OwnedFd::from_raw_fd(fds[1])
    }))
}

fn elevate(fd: OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC returns a new fd at or above 8, or -1.
    let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 8) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

fn set_cloexec(fd: i32) -> Result<(), HostError> {
    // SAFETY: F_SETFD replaces the descriptor flags of an open fd.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(HostError::Io(io::Error::last_os_error().to_string()));
    }
    Ok(())
}

fn close_except(keep: &[i32]) {
    let max = 65_536;
    for fd in 0..max {
        if keep.contains(&fd) {
            continue;
        }
        unsafe { libc::close(fd) };
    }
}
