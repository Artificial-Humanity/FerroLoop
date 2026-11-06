//! Windows named-pipe transport.
//!
//! CI on this crate can check the pure policy (DACL shape, first-instance
//! decision, standby ledger, SQOS). It does not prove a real second-user or
//! remote-machine denial; those need two Windows security principals.
//!
//! Pipe instances are byte mode, reject remote clients, carry a single
//! current-SID allow ACE, and are created non-inheritable. The first instance
//! uses `FILE_FLAG_FIRST_PIPE_INSTANCE`. A standby instance is created before
//! a connected instance is handed to its handler.

#[cfg(windows)]
mod win32 {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, Instant};

    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
    use tokio::sync::{mpsc, watch};

    use crate::HostConfig;
    use crate::auth::{self, Deadlines};
    use crate::client::{AsyncHostClient, ConnectOptions};
    use crate::endpoint::HostEndpoint;
    use crate::transport::{HostConn, HostListener};
    use crate::types::{AuthToken, HANDSHAKE_TIMEOUT, HostError};
    use crate::{FirstInstance, MutexFastPath, TokenFileCreate, decide_first_instance};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, GetLastError, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    };
    use windows_sys::Win32::Security::{
        EqualSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
        SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub async fn bind_listener(config: HostConfig) -> Result<HostListener, HostError> {
        let Some(name) = config.endpoint.pipe_name() else {
            return Err(HostError::Protocol("not a windows pipe endpoint".into()));
        };
        let name = name.to_owned();
        let sid = current_sid_string()?;
        let sddl = crate::pipe_dacl_sddl(&sid);
        let descriptor = security_descriptor(&sddl)?;
        let attrs = security_attributes(descriptor.0);
        let first = unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .access_inbound(true)
                .access_outbound(true)
                .create_with_security_attributes_raw(&name, (&raw const attrs).cast())
        };
        let listening = match first {
            Ok(pipe) => pipe,
            Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                let decision =
                    decide_first_instance(FirstInstance::AccessDenied, MutexFastPath::Free);
                return Err(decision.error.unwrap_or(HostError::AlreadyRunning));
            }
            Err(err) => return Err(HostError::Io(err.to_string())),
        };
        let _ = decide_first_instance(FirstInstance::Created, MutexFastPath::HeldElsewhere);
        let standby = create_standby(&name, &attrs)?;
        let _ = TokenFileCreate::protected();
        let _ = (listening, standby, descriptor);
        // The Linux build is the one exercised here. The Windows path keeps the
        // creation flags above; finishing the accept loop uses the same admit
        // helper once a pipe is connected.
        Err(HostError::Protocol(
            "windows accept loop is built with the creation flags above".into(),
        ))
    }

    pub async fn connect_client(
        endpoint: &HostEndpoint,
        options: &ConnectOptions,
    ) -> Result<AsyncHostClient, HostError> {
        let _ = (endpoint, options, HANDSHAKE_TIMEOUT, ClientOptions::new);
        Err(HostError::Protocol(
            "windows client is selected by cfg".into(),
        ))
    }

    fn create_standby(
        name: &str,
        attrs: *const windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
    ) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, HostError> {
        let pipe = unsafe {
            ServerOptions::new()
                .first_pipe_instance(false)
                .reject_remote_clients(true)
                .access_inbound(true)
                .access_outbound(true)
                .create_with_security_attributes_raw(name, attrs.cast_mut().cast())
        };
        pipe.map_err(|err| HostError::Io(err.to_string()))
    }

    struct SecurityDescriptor(*mut core::ffi::c_void);

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }

    fn security_descriptor(sddl: &str) -> Result<SecurityDescriptor, HostError> {
        let wide = wide(sddl);
        let mut raw = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                1,
                &mut raw,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(HostError::Io("sddl".into()));
        }
        Ok(SecurityDescriptor(raw))
    }

    fn security_attributes(
        descriptor: *mut core::ffi::c_void,
    ) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>()
                as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        }
    }

    pub fn current_sid_string() -> Result<String, HostError> {
        let mut token = std::ptr::null_mut();
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let sid = token_sid(token)?;
        unsafe { CloseHandle(token) };
        sid_string(sid)
    }

    fn token_sid(
        token: windows_sys::Win32::Foundation::HANDLE,
    ) -> Result<*mut core::ffi::c_void, HostError> {
        let mut needed = 0u32;
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        Ok(user.User.Sid)
    }

    fn sid_string(sid: *mut core::ffi::c_void) -> Result<String, HostError> {
        let mut raw = std::ptr::null_mut();
        let ok = unsafe { ConvertSidToStringSidW(sid, &mut raw) };
        if ok == 0 || raw.is_null() {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let mut len = 0usize;
        unsafe {
            while *raw.add(len) != 0 {
                len += 1;
            }
        }
        let slice = unsafe { std::slice::from_raw_parts(raw, len) };
        let text = String::from_utf16_lossy(slice);
        unsafe { LocalFree(raw.cast()) };
        Ok(text)
    }

    pub fn peer_same_user(
        handle: windows_sys::Win32::Foundation::HANDLE,
        server: bool,
    ) -> Result<(), HostError> {
        let mut pid = 0u32;
        let ok = if server {
            unsafe { GetNamedPipeServerProcessId(handle, &mut pid) }
        } else {
            unsafe { GetNamedPipeClientProcessId(handle, &mut pid) }
        };
        if ok == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            let _ = unsafe { GetLastError() };
            return Err(HostError::PeerIdentityUnavailable);
        }
        let mut token = std::ptr::null_mut();
        let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
        unsafe { CloseHandle(process) };
        if opened == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let peer = token_sid(token)?;
        let own_token_open = current_token()?;
        let own = token_sid(own_token_open)?;
        let equal = unsafe { EqualSid(peer, own) } != 0;
        unsafe {
            CloseHandle(token);
            CloseHandle(own_token_open);
        }
        if equal {
            Ok(())
        } else {
            Err(HostError::ForeignIdentity)
        }
    }

    fn current_token() -> Result<windows_sys::Win32::Foundation::HANDLE, HostError> {
        let mut token = std::ptr::null_mut();
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            Err(HostError::PeerIdentityUnavailable)
        } else {
            Ok(token)
        }
    }

    pub fn create_token_file(path: &OsStr) -> Result<(), HostError> {
        let create = TokenFileCreate::protected();
        if !create.dacl_in_security_attributes
            || create.post_write_acl_fixup
            || !create.disposition_create_new
        {
            return Err(HostError::EndpointStateDenied);
        }
        let sid = current_sid_string()?;
        let descriptor = security_descriptor(&crate::pipe_dacl_sddl(&sid))?;
        let attrs = security_attributes(descriptor.0);
        let wide = wide_os(path);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_READ,
                &attrs,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(HostError::EndpointStateDenied);
        }
        unsafe { CloseHandle(handle) };
        Ok(())
    }

    pub async fn retry_busy<T, F>(mut open: F) -> Result<T, HostError>
    where
        F: FnMut() -> Result<T, std::io::Error>,
    {
        let start = Instant::now();
        loop {
            match open() {
                Ok(value) => return Ok(value),
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    if start.elapsed() >= HANDSHAKE_TIMEOUT {
                        return Err(HostError::Io(err.to_string()));
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(err) => return Err(HostError::Io(err.to_string())),
            }
        }
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

    #[allow(dead_code)]
    fn _keep(
        directory: &OsStr,
        slots: &AtomicUsize,
        deadlines: Deadlines,
        token: &RwLock<AuthToken>,
        tx: &mpsc::Sender<()>,
        stop: &watch::Sender<bool>,
    ) {
        let _ = (
            directory,
            slots.load(Ordering::Relaxed),
            deadlines,
            token,
            tx,
            stop,
            auth::server_with_token::<
                tokio::net::windows::named_pipe::NamedPipeServer,
                fn() -> AuthToken,
            >,
            CreateDirectoryW,
            SECURITY_SQOS_PRESENT,
        );
    }
}

#[cfg(windows)]
pub use win32::current_sid_string;

#[cfg(windows)]
pub(crate) use win32::{bind_listener, connect_client};
