//! Unix spawn. The daemon forks the supervisor and then only reaps that pid.
//! Vendor execution stays blocked until `publish` returns.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fl_host_proto::HostError;

use crate::identity::process_identity;
use crate::limits::{CLEANUP_DEADLINE, HANDOFF_TIMEOUT};
use crate::protocol::{DaemonMsg, SupervisorMsg, from_line, to_line};
use crate::refusal::ContainmentRefusal;

pub struct Supervised {
    pub vendor_pid: u32,
    pub vendor_start: u64,
    pub supervisor_pid: u32,
    pub supervisor_start: u64,
    pub pgid: u32,
    pub control_inode: u64,
    pub control: File,
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
    pub status: File,
    pub child: Child,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_supervised(
    bin: &Path,
    argv: Vec<String>,
    cwd: &Path,
    env: Vec<(String, String)>,
    session: &str,
    epoch: u64,
    publish: impl FnOnce(u32, u64, u32, u64, u32) -> Result<(), HostError>,
    fail_after_publish: bool,
) -> Result<Supervised, HostError> {
    block_sigpipe();
    let (control_r, control_w) = pipe_cloexec()?;
    let (status_r, status_w) = pipe_cloexec()?;
    let use_pty = env.iter().any(|(k, v)| k == "FL_HOST_PTY" && v == "1");
    let (stdin_r, stdin_w, stdout_r, stdout_w, stderr_r, stderr_w) = if use_pty {
        let mut master_raw = -1;
        let mut slave_raw = -1;
        #[cfg(target_os = "macos")]
        let openpty_rc = {
            let mut winsize = libc::winsize {
                ws_row: 40,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            unsafe {
                libc::openpty(
                    &mut master_raw,
                    &mut slave_raw,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut winsize,
                )
            }
        };
        #[cfg(not(target_os = "macos"))]
        let openpty_rc = {
            let winsize = libc::winsize {
                ws_row: 40,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            unsafe {
                libc::openpty(
                    &mut master_raw,
                    &mut slave_raw,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &winsize,
                )
            }
        };
        if openpty_rc != 0 {
            return Err(HostError::Io(io::Error::last_os_error().to_string()));
        }
        let master_fd = unsafe { OwnedFd::from_raw_fd(master_raw) };
        let slave_fd = unsafe { OwnedFd::from_raw_fd(slave_raw) };
        for fd in [master_fd.as_raw_fd(), slave_fd.as_raw_fd()] {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(HostError::Io(io::Error::last_os_error().to_string()));
            }
        }
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(slave_fd.as_raw_fd(), &mut termios) } == 0 {
            termios.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
            let _ = unsafe { libc::tcsetattr(slave_fd.as_raw_fd(), libc::TCSANOW, &termios) };
        }
        let stdin_r = elevate(slave_fd)?;
        let stdout_w = elevate(stdin_r.try_clone()?)?;
        let stderr_w = elevate(stdin_r.try_clone()?)?;
        let stdin_w = master_fd.try_clone()?;
        let stdout_r = master_fd;
        let (stderr_r, stderr_w_dummy) = pipe_cloexec()?;
        drop(stderr_w_dummy);
        (stdin_r, stdin_w, stdout_r, stdout_w, stderr_r, stderr_w)
    } else {
        let (stdin_r, stdin_w) = pipe_cloexec()?;
        let (stdout_r, stdout_w) = pipe_cloexec()?;
        let (stderr_r, stderr_w) = pipe_cloexec()?;
        let stdin_r = elevate(stdin_r)?;
        let stdout_w = elevate(stdout_w)?;
        let stderr_w = elevate(stderr_w)?;
        (stdin_r, stdin_w, stdout_r, stdout_w, stderr_r, stderr_w)
    };
    let control_r = elevate(control_r)?;
    let status_w = elevate(status_w)?;
    let fds = [
        control_r.as_raw_fd(),
        status_w.as_raw_fd(),
        stdin_r.as_raw_fd(),
        stdout_w.as_raw_fd(),
        stderr_w.as_raw_fd(),
    ];
    let mut cmd = Command::new(bin);
    cmd.arg("--session-supervisor")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the closure runs between fork and exec in the child. dup2 and close_range
    // are async-signal-safe. Every fd above the handoff set is closed, including the
    // daemon lock, token, listener, and client sockets.
    unsafe {
        cmd.pre_exec(move || {
            let mut signals: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut signals);
            libc::sigaddset(&mut signals, libc::SIGINT);
            libc::sigaddset(&mut signals, libc::SIGTERM);
            let rc = libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut());
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            dup_fd(fds[0], 3)?;
            dup_fd(fds[1], 4)?;
            dup_fd(fds[2], 5)?;
            dup_fd(fds[3], 6)?;
            dup_fd(fds[4], 7)?;
            close_from(8);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|err| HostError::Io(err.to_string()))?;
    drop(control_r);
    drop(status_w);
    drop(stdin_r);
    drop(stdout_w);
    drop(stderr_w);
    let control_inode = File::from(control_w.try_clone()?)
        .metadata()
        .map(|meta| meta.ino())
        .unwrap_or(0);
    let mut status = File::from(status_r);
    let mut control = File::from(control_w);
    let mut proven = None;
    let outcome = handshake(
        &mut child,
        &mut control,
        &mut status,
        VendorLaunch {
            argv,
            cwd,
            env,
            session,
            epoch,
        },
        publish,
        fail_after_publish,
        &mut proven,
    );
    match outcome {
        Ok((vendor_pid, vendor_start, supervisor_start, pgid)) => Ok(Supervised {
            vendor_pid,
            vendor_start,
            supervisor_pid: child.id(),
            supervisor_start,
            pgid,
            control_inode,
            control,
            stdin: File::from(stdin_w),
            stdout: File::from(stdout_r),
            stderr: File::from(stderr_r),
            status,
            child,
        }),
        Err(err) => {
            drop(control);
            // Dropping the control pipe asks a live supervisor to abort its own vendor.
            // A numeric kill here would also drop the subreaper and reparent a gated vendor.
            match abort_supervised(&mut child, proven) {
                Ok(()) => Err(err),
                Err(refusal) => Err(HostError::from(refusal)),
            }
        }
    }
}

struct VendorLaunch<'a> {
    argv: Vec<String>,
    cwd: &'a Path,
    env: Vec<(String, String)>,
    session: &'a str,
    epoch: u64,
}

fn handshake(
    child: &mut Child,
    control: &mut File,
    status: &mut File,
    launch: VendorLaunch<'_>,
    publish: impl FnOnce(u32, u64, u32, u64, u32) -> Result<(), HostError>,
    fail_after_publish: bool,
    proven: &mut Option<(u32, u64)>,
) -> Result<(u32, u64, u64, u32), HostError> {
    let VendorLaunch {
        argv,
        cwd,
        env,
        session,
        epoch,
    } = launch;
    let message = DaemonMsg::Spawn {
        argv,
        cwd: cwd.as_os_str().as_bytes().to_vec(),
        env,
        session: session.to_owned(),
        epoch,
    };
    let bytes = to_line(&message).map_err(HostError::Protocol)?;
    control.write_all(&bytes)?;
    let identity = match read_supervisor(status, HANDOFF_TIMEOUT)? {
        Some(SupervisorMsg::Identity {
            vendor_pid,
            vendor_start,
            pgid,
            supervisor_pid,
            supervisor_start,
        }) => (
            vendor_pid,
            vendor_start,
            pgid,
            supervisor_pid,
            supervisor_start,
        ),
        Some(SupervisorMsg::Fail { error }) => return Err(HostError::Protocol(error)),
        _ => return Err(HostError::Protocol("identity".into())),
    };
    let (vendor_pid, vendor_start, pgid, supervisor_pid, supervisor_start) = identity;
    if supervisor_pid != child.id() || pgid != vendor_pid {
        return Err(HostError::Protocol("identity mismatch".into()));
    }
    let vendor = process_identity(vendor_pid)?;
    let supervisor = process_identity(supervisor_pid)?;
    if vendor.pid != vendor_pid
        || vendor.start != vendor_start
        || vendor.pgid != pgid
        || supervisor.pid != supervisor_pid
        || supervisor.start != supervisor_start
    {
        return Err(HostError::Protocol("unproven ownership".into()));
    }
    *proven = Some((vendor_pid, vendor_start));
    publish(
        vendor_pid,
        vendor_start,
        supervisor_pid,
        supervisor_start,
        pgid,
    )?;
    if fail_after_publish {
        return Err(HostError::Protocol("handshake".into()));
    }
    let go = to_line(&DaemonMsg::Go).map_err(HostError::Protocol)?;
    control.write_all(&go)?;
    match read_supervisor(status, HANDOFF_TIMEOUT)? {
        Some(SupervisorMsg::Exec { ok: true, .. }) => {
            Ok((vendor_pid, vendor_start, supervisor_start, pgid))
        }
        Some(SupervisorMsg::Exec { error, .. }) => Err(HostError::Protocol(error)),
        _ => Err(HostError::Protocol("exec ack".into())),
    }
}

fn abort_supervised(
    child: &mut Child,
    vendor: Option<(u32, u64)>,
) -> Result<(), ContainmentRefusal> {
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        // Both pidfds are pinned before either signal. A missing supervisor is unresolved:
        // signalling only the vendor would leave the subreaper, and the reverse reparents it.
        let supervisor =
            process_identity(child.id()).map_err(|_| ContainmentRefusal::CleanupUnresolved)?;
        let mut targets = Vec::new();
        if let Some(vendor) = vendor {
            targets.push(vendor);
        }
        targets.push((supervisor.pid, supervisor.start));
        crate::identity::signal_kill_pidfd(&targets)?;
        wait_stopped(child, &targets, deadline)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = vendor;
        let deadline = Instant::now() + CLEANUP_DEADLINE;
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if child.try_wait().ok().flatten().is_some() {
            Ok(())
        } else {
            Err(ContainmentRefusal::CleanupUnresolved)
        }
    }
}

#[cfg(target_os = "linux")]
fn wait_stopped(
    child: &mut Child,
    targets: &[(u32, u64)],
    deadline: Instant,
) -> Result<(), ContainmentRefusal> {
    loop {
        let _ = child.try_wait();
        let live = targets
            .iter()
            .any(|(pid, start)| crate::identity::still_executing(*pid, *start));
        if !live {
            let _ = child.try_wait();
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ContainmentRefusal::CleanupUnresolved);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_supervisor(file: &mut File, timeout: Duration) -> Result<Option<SupervisorMsg>, HostError> {
    let Some(line) = read_line(file, timeout)? else {
        return Ok(None);
    };
    from_line(&line).map(Some).map_err(HostError::Protocol)
}

pub fn read_line_blocking(file: &mut File) -> Result<Option<Vec<u8>>, HostError> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = file.read(&mut byte)?;
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

pub fn read_line(file: &mut File, timeout: Duration) -> Result<Option<Vec<u8>>, HostError> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    let fd = file.as_raw_fd();
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
        // SAFETY: one pollfd for a status or pipe fd owned by the caller.
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
        let n = file.read(&mut byte)?;
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
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(not(target_os = "linux"))]
    {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|err| err.into_inner());
        // SAFETY: pipe initializes both ends. cloexec is set before the lock is released.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok((unsafe { OwnedFd::from_raw_fd(fds[0]) }, unsafe {
        OwnedFd::from_raw_fd(fds[1])
    }))
}

fn elevate(fd: OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC allocates a new fd at or above 8.
    let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 8) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

fn dup_fd(src: i32, dst: i32) -> io::Result<()> {
    if src == dst {
        return Ok(());
    }
    // SAFETY: both numbers are open fds in the forked child, or dup2 fails.
    if unsafe { libc::dup2(src, dst) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn close_from(start: i32) {
    #[cfg(target_os = "linux")]
    unsafe {
        // SAFETY: close_range closes every fd from `start` through the last one. It does
        // not allocate. The handoff fds are below `start`.
        libc::syscall(libc::SYS_close_range, start as libc::c_uint, !0u32, 0u32);
    }
    #[cfg(not(target_os = "linux"))]
    unsafe {
        let mut fd = start;
        while fd < 65_536 {
            libc::close(fd);
            fd += 1;
        }
    }
}

fn block_sigpipe() {
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}
