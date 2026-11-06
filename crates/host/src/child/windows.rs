//! Windows containment. The vendor is created suspended, inside a kill-on-close Job,
//! with an explicit stdio handle list and no breakaway. The Job stays open after the
//! leader exits. `CleanupComplete` waits for stable Job accounting, not for a saved PID.
//!
//! The optional daemon backstop Job is attempted once. A refusal is logged. It is not
//! a claimed guarantee.

use std::ffi::OsStr;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;
use std::time::Duration;

use fl_host_proto::{HostError, windows_command_line, windows_environment_block};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_WRITE, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, SetHandleInformation, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, CreatePipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::limits::CLEANUP_DEADLINE;
use crate::survivor_registry::CrashPoint;

pub struct JobChild {
    pub vendor_pid: u32,
    pub vendor_start: u64,
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
    pub job: OwnedHandle,
    pub process: OwnedHandle,
}

pub fn spawn_in_job(
    argv: &[String],
    cwd: &Path,
    env: &[(String, String)],
    crash: Option<CrashPoint>,
    publish: impl FnOnce(u32, u64) -> Result<(), HostError>,
) -> Result<JobChild, HostError> {
    let job = kill_on_close_job()?;
    let stdin = overlapped_stdin_pipe()?;
    let stdout = inheritable_pipe(true)?;
    let stderr = inheritable_pipe(true)?;
    clear_inherit(stdin.parent.as_raw_handle() as HANDLE)?;
    clear_inherit(stdout.parent.as_raw_handle() as HANDLE)?;
    clear_inherit(stderr.parent.as_raw_handle() as HANDLE)?;
    let mut handles = [
        stdin.child.as_raw_handle() as HANDLE,
        stdout.child.as_raw_handle() as HANDLE,
        stderr.child.as_raw_handle() as HANDLE,
    ];
    let job_handle = job.as_raw_handle() as HANDLE;
    let attrs = AttributeList::new(2)?;
    // SAFETY: `job_handle` is the non-inheritable session Job. The attribute keeps this
    // pointer only until CreateProcessW returns.
    unsafe {
        if UpdateProcThreadAttribute(
            attrs.raw(),
            0,
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            (&job_handle as *const HANDLE).cast(),
            std::mem::size_of::<HANDLE>(),
            ptr::null_mut(),
            ptr::null(),
        ) == 0
        {
            return Err(HostError::Protocol("job list".into()));
        }
        if UpdateProcThreadAttribute(
            attrs.raw(),
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_mut_ptr().cast(),
            handles.len() * std::mem::size_of::<HANDLE>(),
            ptr::null_mut(),
            ptr::null(),
        ) == 0
        {
            return Err(HostError::Protocol("handle list".into()));
        }
    }
    let mut command = wide(&windows_command_line(argv)?);
    let mut environment = windows_environment_block(env)?;
    if environment.last() != Some(&0) || environment.len() < 2 {
        environment.push(0);
    }
    let cwd_w = wide_os(cwd.as_os_str());
    let app_w = wide(&argv[0]);
    if inject("create") {
        return Err(HostError::Io("create process".into()));
    }
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = handles[0];
    si.StartupInfo.hStdOutput = handles[1];
    si.StartupInfo.hStdError = handles[2];
    si.lpAttributeList = attrs.raw();
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: command, environment, and cwd are NUL-terminated. The process is created
    // suspended, with the Job list and only the three stdio handles. No breakaway flag is set.
    let created = unsafe {
        CreateProcessW(
            app_w.as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            cwd_w.as_ptr(),
            (&si.StartupInfo) as *const _,
            &mut pi,
        )
    };
    drop(stdin.child);
    drop(stdout.child);
    drop(stderr.child);
    if created == 0 {
        return Err(HostError::Io("create process".into()));
    }
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread) };
    let contained = process_in_job(process.as_raw_handle() as HANDLE, job_handle)?;
    if !contained {
        terminate_and_wait(&process);
        return Err(HostError::Protocol("job membership".into()));
    }
    if let Ok(path) = std::env::var("FL_HOST_PIDFILE") {
        let _ = std::fs::write(path, pi.dwProcessId.to_string());
    }
    if inject("membership") {
        terminate_and_wait(&process);
        return Err(HostError::Protocol("job membership".into()));
    }
    if matches!(crash, Some(CrashPoint::WindowsCreated)) {
        die();
    }
    let start = process_start(&process)?;
    publish(pi.dwProcessId, start)?;
    if matches!(crash, Some(CrashPoint::WindowsContained)) {
        die();
    }
    if inject("resume") {
        terminate_and_wait(&process);
        return Err(HostError::Protocol("resume".into()));
    }
    // SAFETY: `thread` is the primary thread suspended by CREATE_SUSPENDED.
    let resumed = unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) };
    if resumed == u32::MAX {
        terminate_and_wait(&process);
        return Err(HostError::Protocol("resume".into()));
    }
    if matches!(crash, Some(CrashPoint::WindowsRunning)) {
        die();
    }
    Ok(JobChild {
        vendor_pid: pi.dwProcessId,
        vendor_start: start,
        stdin: File::from(stdin.parent),
        stdout: File::from(stdout.parent),
        stderr: File::from(stderr.parent),
        job,
        process,
    })
}

pub fn backstop() {
    match kill_on_close_job() {
        Ok(job) => {
            // SAFETY: assigning this process to a nested Job. Failure is logged and the
            // handle is dropped, so a refused backstop does not become kill-on-close.
            let ok = unsafe {
                windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                    job.as_raw_handle() as HANDLE,
                    GetCurrentProcess(),
                )
            };
            if ok == 0 {
                eprintln!("fl-host: backstop job refused: assign");
            } else {
                std::mem::forget(job);
            }
        }
        Err(err) => eprintln!("fl-host: backstop job refused: {err}"),
    }
}

pub fn leader_exit_code(process: &OwnedHandle) -> Option<i32> {
    let mut code = 0u32;
    // SAFETY: `process` is the leader handle retained since CreateProcessW.
    let ok = unsafe { GetExitCodeProcess(process.as_raw_handle() as HANDLE, &mut code) };
    if ok == 0 || code == 259 {
        None
    } else {
        Some(code as i32)
    }
}

pub fn leader_signaled(process: &OwnedHandle) -> bool {
    // SAFETY: zero timeout polls the retained leader handle.
    unsafe { WaitForSingleObject(process.as_raw_handle() as HANDLE, 0) == WAIT_OBJECT_0 }
}

pub fn stable_job_quiescent(job: &OwnedHandle) -> bool {
    active_processes(job) == Some(0) && {
        std::thread::sleep(Duration::from_millis(20));
        active_processes(job) == Some(0)
    }
}

fn active_processes(job: &OwnedHandle) -> Option<u32> {
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is writable and sized for the basic accounting class.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        None
    } else {
        Some(info.ActiveProcesses)
    }
}

fn terminate_and_wait(process: &OwnedHandle) {
    // SAFETY: `process` is the suspended or partially started child. The wait is bounded.
    unsafe { TerminateProcess(process.as_raw_handle() as HANDLE, 1) };
    unsafe {
        WaitForSingleObject(
            process.as_raw_handle() as HANDLE,
            CLEANUP_DEADLINE.as_millis() as u32,
        )
    };
}

fn process_start(process: &OwnedHandle) -> Result<u64, HostError> {
    use windows_sys::Win32::Foundation::FILETIME;
    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = created;
    let mut kernel = created;
    let mut user = created;
    // SAFETY: the four FILETIME outputs are writable. The handle is the one CreateProcessW returned.
    let ok = unsafe {
        GetProcessTimes(
            process.as_raw_handle() as HANDLE,
            &mut created,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return Err(HostError::Io("process times".into()));
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn process_in_job(process: HANDLE, job: HANDLE) -> Result<bool, HostError> {
    let mut member = 0;
    // SAFETY: both handles are open. `member` is an output BOOL.
    let ok = unsafe { IsProcessInJob(process, job, &mut member) };
    if ok == 0 {
        Err(HostError::Io("job membership".into()))
    } else {
        Ok(member != 0)
    }
}

fn kill_on_close_job() -> Result<OwnedHandle, HostError> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 0,
    };
    // SAFETY: the Job is unnamed and not inheritable. A null name is anonymous.
    let raw = unsafe { CreateJobObjectW(&sa, ptr::null()) };
    if raw.is_null() {
        return Err(HostError::Io("job".into()));
    }
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `limits` describes kill-on-close and no breakaway. The Job handle is `job`.
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(HostError::Io("job limits".into()));
    }
    Ok(job)
}

struct PipePair {
    parent: OwnedHandle,
    child: OwnedHandle,
}

/// Parent write end is overlapped so stdin `WriteFile` can be cancelled at the deadline.
/// The child read end stays synchronous.
fn overlapped_stdin_pipe() -> Result<PipePair, HostError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static PIPE_SEQ: AtomicU64 = AtomicU64::new(1);
    let mut last = HostError::Io("stdin pipe".into());
    for _ in 0..8 {
        let n = PIPE_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!(r"\\.\pipe\fl-host-in-{}-{n}", std::process::id());
        let wide = wide(&name);
        let inherit = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: `wide` is NUL-terminated. The server end is the child's synchronous stdin.
        let read = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                65_536,
                65_536,
                0,
                &inherit,
            )
        };
        if read.is_null() || read == INVALID_HANDLE_VALUE {
            last = HostError::Io("stdin pipe".into());
            continue;
        }
        let read = unsafe { OwnedHandle::from_raw_handle(read) };
        let private = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 0,
        };
        // SAFETY: this opens the write end of `read`. Overlapped lets the host cancel it.
        let write = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_WRITE,
                0,
                &private,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if write.is_null() || write == INVALID_HANDLE_VALUE {
            last = HostError::Io("stdin pipe".into());
            continue;
        }
        let write = unsafe { OwnedHandle::from_raw_handle(write) };
        // SAFETY: the client is already connected. ERROR_PIPE_CONNECTED is that success.
        let connected =
            unsafe { ConnectNamedPipe(read.as_raw_handle() as HANDLE, ptr::null_mut()) };
        if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            last = HostError::Io("stdin pipe".into());
            continue;
        }
        return Ok(PipePair {
            parent: write,
            child: read,
        });
    }
    Err(last)
}

fn inheritable_pipe(parent_reads: bool) -> Result<PipePair, HostError> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    // SAFETY: both ends are created inheritable. The caller clears the parent end.
    let ok = unsafe { CreatePipe(&mut read, &mut write, &sa, 0) };
    if ok == 0 {
        return Err(HostError::Io("pipe".into()));
    }
    let (parent, child) = if parent_reads {
        (read, write)
    } else {
        (write, read)
    };
    Ok(PipePair {
        parent: unsafe { OwnedHandle::from_raw_handle(parent) },
        child: unsafe { OwnedHandle::from_raw_handle(child) },
    })
}

fn clear_inherit(handle: HANDLE) -> Result<(), HostError> {
    // SAFETY: `handle` is an open pipe end. Clearing HANDLE_FLAG_INHERIT keeps it out of children.
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
        Err(HostError::Io("inherit".into()))
    } else {
        Ok(())
    }
}

struct AttributeList {
    mem: *mut u8,
    layout: std::alloc::Layout,
}

impl AttributeList {
    fn new(count: u32) -> Result<Self, HostError> {
        let mut size = 0usize;
        // SAFETY: a null list with a size out-parameter reports the required bytes.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut size) };
        if size == 0 {
            return Err(HostError::Protocol("attribute list".into()));
        }
        let layout = std::alloc::Layout::from_size_align(size, 16)
            .map_err(|_| HostError::Protocol("attribute list".into()))?;
        // SAFETY: `layout` is non-zero. The block is initialized by the next call.
        let mem = unsafe { std::alloc::alloc(layout) };
        if mem.is_null() {
            return Err(HostError::Io("attribute list".into()));
        }
        let ok = unsafe { InitializeProcThreadAttributeList(mem.cast(), count, 0, &mut size) };
        if ok == 0 {
            unsafe { std::alloc::dealloc(mem, layout) };
            return Err(HostError::Protocol("attribute list".into()));
        }
        Ok(Self { mem, layout })
    }

    fn raw(&self) -> windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST {
        self.mem.cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if !self.mem.is_null() {
            unsafe { DeleteProcThreadAttributeList(self.mem.cast()) };
            unsafe { std::alloc::dealloc(self.mem, self.layout) };
        }
    }
}

fn inject(name: &str) -> bool {
    std::env::var("FL_HOST_INJECT").ok().as_deref() == Some(name)
}

fn die() -> ! {
    unsafe { TerminateProcess(GetCurrentProcess(), 9) };
    std::process::abort();
}

fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn wide_os(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

// Silence an unused import when the Job is only terminated from the session owner.
#[allow(dead_code)]
fn _job_terminate_linked() {
    let _ = TerminateJobObject;
    let _ = CloseHandle;
}
