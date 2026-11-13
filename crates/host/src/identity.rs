//! Process identity and signalling.
//!
//! Linux without a delegated cgroup is best effort. A descendant that installs its own
//! subreaper, and anything still running after both the supervisor and the daemon are
//! SIGKILLed, is a named residual. pidfd pins a process; a start-time check is not permission
//! to call `kill` on a numeric pid.
//!
//! macOS has no subreaper and no cgroup. `proc_pidinfo` validates identity and does not close
//! the check-to-kill race. Signals go only to pinned owned children or to a group anchored by
//! an unreaped leader. A `setsid` descendant reparented to launchd is a residual, including
//! after daemon SIGKILL. This module does not claim macOS adoption.

use std::cell::Cell;
#[cfg(unix)]
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

thread_local! {
    static PROBE_UNAVAILABLE: Cell<bool> = const { Cell::new(false) };
    static KILL_UNAVAILABLE: Cell<bool> = const { Cell::new(false) };
}

/// Test seam. The next [`check_pidfd_support`] call returns [`ContainmentRefusal::PidfdUnavailable`].
pub fn set_pidfd_probe_unavailable(unavailable: bool) {
    PROBE_UNAVAILABLE.with(|cell| cell.set(unavailable));
}

/// Test seam. Pidfd cleanup returns [`ContainmentRefusal::CleanupUnresolved`] and does not signal.
pub fn set_pidfd_kill_unavailable(unavailable: bool) {
    KILL_UNAVAILABLE.with(|cell| cell.set(unavailable));
}

use fl_host_proto::HostError;

#[cfg(target_os = "linux")]
use crate::limits::TERM_GRACE;
use crate::refusal::ContainmentRefusal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub start: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct LeaderAnchor {
    pub pid: u32,
    pub start: u64,
    pub pgid: u32,
    pub reaped: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RecordedPid {
    pub pid: u32,
    pub start: u64,
}

pub type SignalHook = std::sync::Arc<dyn Fn(u32) + Send + Sync>;

pub fn process_identity(pid: u32) -> Result<ProcessIdentity, HostError> {
    #[cfg(target_os = "linux")]
    {
        linux_identity(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_identity(pid)
    }
    #[cfg(windows)]
    {
        windows_identity(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        Err(HostError::Unsupported {
            subject: "process identity",
        })
    }
}

/// Probe pidfd availability. Unavailable or disallowed is a typed refusal, not a numeric kill.
pub fn check_pidfd_support() -> Result<(), ContainmentRefusal> {
    #[cfg(target_os = "linux")]
    {
        if probe_forced_unavailable() {
            return Err(ContainmentRefusal::PidfdUnavailable);
        }
        let fd = pidfd_open(std::process::id()).map_err(classify_pidfd)?;
        pidfd_send(fd.as_raw_fd(), 0).map_err(classify_pidfd)?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(ContainmentRefusal::PidfdUnavailable)
    }
}

#[cfg(target_os = "linux")]
fn probe_forced_unavailable() -> bool {
    PROBE_UNAVAILABLE.with(|cell| cell.get())
        || std::env::var_os("FL_HOST_ASSUME_NO_PIDFD").is_some_and(|value| value == "1")
}

/// SIGKILL each proven target through a pidfd, vendor before supervisor.
/// A pin or send failure signals nothing further and returns [`ContainmentRefusal::CleanupUnresolved`].
#[cfg(target_os = "linux")]
pub(crate) fn signal_kill_pidfd(targets: &[(u32, u64)]) -> Result<(), ContainmentRefusal> {
    if KILL_UNAVAILABLE.with(|cell| cell.get()) {
        return Err(ContainmentRefusal::CleanupUnresolved);
    }
    let mut pins = Vec::new();
    for &(pid, start) in targets {
        if pid == 0 {
            continue;
        }
        match prove(pid, start) {
            Ok(Proof::Gone) => {}
            Ok(Proof::Pinned(pin)) => pins.push(pin),
            Ok(Proof::Unproven) | Err(_) => return Err(ContainmentRefusal::CleanupUnresolved),
        }
    }
    for pin in &pins {
        if let Err(err) = pidfd_send(pin.as_raw(), libc::SIGKILL)
            && err.raw_os_error() != Some(libc::ESRCH)
        {
            return Err(ContainmentRefusal::CleanupUnresolved);
        }
    }
    Ok(())
}

/// True while `pid` is still the process that started at `start` and is not a zombie.
#[cfg(target_os = "linux")]
pub(crate) fn still_executing(pid: u32, start: u64) -> bool {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(ident) = parse_stat(&text) else {
        return false;
    };
    if ident.pid != pid || ident.start != start {
        return false;
    }
    let Some(close) = text.rfind(')') else {
        return false;
    };
    let state = text[close + 1..].trim_start().chars().next();
    !matches!(state, None | Some('Z'))
}

/// Signal `anchor.pgid` only while that unreaped leader still anchors the group.
/// A reaped or missing leader returns [`ContainmentRefusal::UnanchoredGroup`] and sends nothing.
pub fn signal_anchored_group(
    anchor: Option<&LeaderAnchor>,
    signal: i32,
) -> Result<(), ContainmentRefusal> {
    let Some(anchor) = anchor else {
        return Err(ContainmentRefusal::UnanchoredGroup);
    };
    if anchor.reaped || anchor.pgid == 0 || anchor.pid == 0 || signal < 0 {
        return Err(ContainmentRefusal::UnanchoredGroup);
    }
    #[cfg(unix)]
    {
        let proof = prove(anchor.pid, anchor.start)?;
        let Proof::Pinned(_pin) = proof else {
            return Err(if matches!(proof, Proof::Gone) {
                ContainmentRefusal::UnanchoredGroup
            } else {
                ContainmentRefusal::IdentityUnproven
            });
        };
        let ident =
            process_identity(anchor.pid).map_err(|_| ContainmentRefusal::IdentityUnproven)?;
        if ident.start != anchor.start || ident.pgid != anchor.pgid || ident.pid != anchor.pid {
            #[cfg(target_os = "linux")]
            drop(_pin);
            return Err(ContainmentRefusal::IdentityUnproven);
        }
        // Linux keeps the pidfd open through the signal. This caller has not reaped the leader.
        // Group membership cannot be reused until that leader is reaped.
        let rc = unsafe { libc::kill(-(anchor.pgid as i32), signal) };
        #[cfg(target_os = "linux")]
        drop(_pin);
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Err(ContainmentRefusal::UnanchoredGroup);
            }
            return Err(ContainmentRefusal::SpawnRejected);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (anchor, signal);
        Err(ContainmentRefusal::UnanchoredGroup)
    }
}

/// Startup recovery. Linux signals only through a pidfd acquired before the start-time check.
/// Proven targets are signalled even when a later target is unproven. macOS does not
/// numeric-kill a process this daemon does not still own; it waits for the surviving
/// supervisor's own cleanup and refuses if the recorded process is still unpinned.
pub fn cleanup_recorded(
    targets: &[RecordedPid],
    hook: Option<&SignalHook>,
    deadline: Instant,
) -> Result<(), ContainmentRefusal> {
    let mut pinned = Vec::new();
    let mut unproven = false;
    for target in targets {
        if target.pid == 0 {
            continue;
        }
        match prove(target.pid, target.start)? {
            Proof::Gone => {}
            Proof::Unproven => unproven = true,
            Proof::Pinned(pin) => pinned.push((target.pid, target.start, pin)),
        }
    }
    let killed = finish_pinned(&pinned, hook, deadline);
    match killed {
        Err(err) => Err(err),
        Ok(()) if unproven => Err(ContainmentRefusal::IdentityUnproven),
        Ok(()) => Ok(()),
    }
}

fn finish_pinned(
    pinned: &[(u32, u64, Pin)],
    hook: Option<&SignalHook>,
    deadline: Instant,
) -> Result<(), ContainmentRefusal> {
    if pinned.is_empty() {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        for (pid, _, pin) in pinned {
            if let Some(hook) = hook {
                hook(*pid);
            }
            // The pidfd was opened before the start-time check. Signal that handle only.
            let _ = pidfd_send(pin.as_raw(), libc::SIGTERM);
        }
        wait_until(deadline.min(Instant::now() + TERM_GRACE), pinned);
        for (_, _, pin) in pinned {
            let _ = pidfd_send(pin.as_raw(), libc::SIGKILL);
        }
        wait_until(deadline, pinned);
        if pinned
            .iter()
            .any(|(pid, start, _)| still_same(*pid, *start))
        {
            Err(ContainmentRefusal::CleanupUnresolved)
        } else {
            Ok(())
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = hook;
        while Instant::now() < deadline {
            if pinned
                .iter()
                .all(|(pid, start, _)| !still_same(*pid, *start))
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if pinned
            .iter()
            .any(|(pid, start, _)| still_same(*pid, *start))
        {
            Err(ContainmentRefusal::CleanupUnresolved)
        } else {
            Ok(())
        }
    }
}

enum Proof {
    Gone,
    Unproven,
    Pinned(Pin),
}

#[cfg(target_os = "linux")]
struct Pin(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl Pin {
    fn as_raw(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
struct Pin;

#[cfg(not(unix))]
struct Pin;

fn prove(pid: u32, start: u64) -> Result<Proof, ContainmentRefusal> {
    #[cfg(target_os = "linux")]
    {
        let fd = match pidfd_open(pid) {
            Ok(fd) => fd,
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => return Ok(Proof::Gone),
            Err(err) => return Err(classify_pidfd(err)),
        };
        match linux_identity(pid) {
            Ok(ident) if ident.start == start && ident.pid == pid => Ok(Proof::Pinned(Pin(fd))),
            Ok(_) => {
                drop(fd);
                Ok(Proof::Unproven)
            }
            Err(err) if is_gone(&err) => {
                // The pidfd is held, but /proc no longer names this pid. Retry once.
                drop(fd);
                match pidfd_open(pid) {
                    Err(err) if err.raw_os_error() == Some(libc::ESRCH) => Ok(Proof::Gone),
                    Ok(_) => Ok(Proof::Unproven),
                    Err(err) => Err(classify_pidfd(err)),
                }
            }
            Err(_) => {
                drop(fd);
                Ok(Proof::Unproven)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        match process_identity(pid) {
            Ok(ident) if ident.start == start && ident.pid == pid => Ok(Proof::Pinned(Pin)),
            Ok(_) => Ok(Proof::Unproven),
            Err(err) if is_gone(&err) => Ok(Proof::Gone),
            Err(_) => Ok(Proof::Unproven),
        }
    }
}

fn still_same(pid: u32, start: u64) -> bool {
    matches!(process_identity(pid), Ok(ident) if ident.pid == pid && ident.start == start)
}

#[cfg(target_os = "linux")]
fn wait_until(deadline: Instant, pinned: &[(u32, u64, Pin)]) {
    while Instant::now() < deadline {
        if pinned
            .iter()
            .all(|(pid, start, _)| !still_same(*pid, *start))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
fn classify_pidfd(err: io::Error) -> ContainmentRefusal {
    match err.raw_os_error() {
        Some(code) if code == libc::ENOSYS || code == libc::EPERM || code == libc::EACCES => {
            ContainmentRefusal::PidfdUnavailable
        }
        _ => ContainmentRefusal::SpawnRejected,
    }
}

#[cfg(target_os = "linux")]
fn pidfd_open(pid: u32) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: pidfd_open returns a new fd or -1. Flags 0 requests a blocking pidfd.
    let rc = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(rc as i32) })
    }
}

#[cfg(target_os = "linux")]
fn pidfd_send(fd: std::os::fd::RawFd, signal: i32) -> io::Result<()> {
    // SAFETY: `fd` is a pidfd. A null siginfo asks the kernel to build the signal itself.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd,
            signal,
            std::ptr::null_mut::<libc::siginfo_t>(),
            0,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn linux_identity(pid: u32) -> Result<ProcessIdentity, HostError> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            HostError::Io("esrch".into())
        } else {
            HostError::Io(err.to_string())
        }
    })?;
    parse_stat(&text).ok_or_else(|| HostError::Io("stat".into()))
}

#[cfg(target_os = "linux")]
pub(crate) fn parse_stat(text: &str) -> Option<ProcessIdentity> {
    let pid: u32 = text.split_whitespace().next()?.parse().ok()?;
    let close = text.rfind(')')?;
    let rest = text[close + 1..].trim();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let pgid = fields.get(2)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some(ProcessIdentity {
        pid,
        ppid,
        pgid,
        start,
    })
}

#[cfg(target_os = "macos")]
fn macos_identity(pid: u32) -> Result<ProcessIdentity, HostError> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `info` is writable and sized. proc_pidinfo fails closed on a bad pid.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if wrote != size {
        return Err(HostError::Io("esrch".into()));
    }
    let start = info
        .pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbi_start_tvusec);
    Ok(ProcessIdentity {
        pid: info.pbi_pid,
        ppid: info.pbi_ppid,
        pgid: info.pbi_pgid,
        start,
    })
}

#[cfg(windows)]
fn windows_identity(pid: u32) -> Result<ProcessIdentity, HostError> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return Err(HostError::Io("esrch".into()));
    }
    // SAFETY: OpenProcess yields a handle this function owns, or null.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(HostError::Io("esrch".into()));
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetProcessTimes(
            std::os::windows::io::AsRawHandle::as_raw_handle(&handle) as _,
            &mut created,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return Err(HostError::Io("esrch".into()));
    }
    let start = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    let _ = CloseHandle;
    Ok(ProcessIdentity {
        pid,
        ppid: 0,
        pgid: pid,
        start,
    })
}

fn is_gone(err: &HostError) -> bool {
    matches!(err, HostError::Io(text) if text == "esrch")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::parse_stat;

    #[test]
    fn stat_comm_with_spaces_and_parens() {
        // starttime is the 20th field after ')': state, ppid, pgrp, then 16 fields, then starttime.
        let text = "12 (my proc) S 1 12 12 0 0 0 0 0 0 0 0 0 0 0 20 0 1 0 999 extra";
        let parsed = parse_stat(text).unwrap();
        assert_eq!(parsed.pid, 12);
        assert_eq!(parsed.ppid, 1);
        assert_eq!(parsed.pgid, 12);
        assert_eq!(parsed.start, 999);
    }
}
