//! Windows named-pipe transport.
//!
//! Pipe instances are byte mode, reject remote clients, carry one current-SID
//! allow ACE, and are created non-inheritable. `FILE_FLAG_FIRST_PIPE_INSTANCE`
//! is the singleton check. The next instance is created before a connected
//! instance is handed off, so the pipe name is never released on handoff.
//!
//! Same-user CI does not prove a second-user or remote-machine denial.

#[cfg(windows)]
mod win32 {
    use std::ffi::OsStr;
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, Instant};

    use tokio::net::windows::named_pipe::{ClientOptions, PipeMode, ServerOptions};
    use tokio::sync::{mpsc, watch};

    use crate::HostConfig;
    use crate::auth::{self, Deadlines};
    use crate::client::{AsyncHostClient, ConnectOptions};
    use crate::endpoint::HostEndpoint;
    use crate::transport::{ConnIo, HostConn, HostListener, Shared, admit, reserve};
    use crate::types::{
        AuthToken, FRAME_READ_TIMEOUT, FirstInstance, HANDSHAKE_TIMEOUT, HostError, InstanceLedger,
        MutexFastPath, NamedPipeClientOptions, NamedPipeServerOptions, TokenFileCreate,
        WRITE_TIMEOUT, constant_time_eq, decide_first_instance,
    };

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND,
        ERROR_PIPE_BUSY, ERROR_SUCCESS, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SE_FILE_OBJECT, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CopySid, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetLengthSid, GetSecurityDescriptorControl, GetTokenInformation, IsValidSid,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PRESENT, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, DeleteFileW,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileAttributesW, GetFileInformationByHandle, INVALID_FILE_ATTRIBUTES,
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    };
    use windows_sys::Win32::System::Threading::{
        CreateMutexW, GetCurrentProcess, OpenProcess, OpenProcessToken,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    struct SecurityDescriptor(*mut core::ffi::c_void);

    // SAFETY: the descriptor is owned, immutable after creation, and freed only in Drop.
    #[allow(clippy::non_send_fields_in_send_ty)]
    unsafe impl Send for SecurityDescriptor {}
    #[allow(clippy::non_send_fields_in_send_ty)]
    unsafe impl Sync for SecurityDescriptor {}

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
                self.0 = std::ptr::null_mut();
            }
        }
    }

    impl SecurityDescriptor {
        fn as_ptr(&self) -> *mut core::ffi::c_void {
            self.0
        }
    }

    struct OwnedWinHandle(HANDLE);

    // SAFETY: the handle is owned by this value and closed on drop.
    #[allow(clippy::non_send_fields_in_send_ty)]
    unsafe impl Send for OwnedWinHandle {}

    impl Drop for OwnedWinHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
            self.0 = std::ptr::null_mut();
        }
    }

    impl OwnedWinHandle {
        fn new(handle: HANDLE) -> Result<Self, HostError> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                Err(HostError::PeerIdentityUnavailable)
            } else {
                Ok(Self(handle))
            }
        }

        fn as_raw(&self) -> HANDLE {
            self.0
        }

        fn into_file(self) -> std::fs::File {
            let handle = self.0;
            std::mem::forget(self);
            // SAFETY: `handle` is open and exclusively owned. The file closes it.
            unsafe { std::fs::File::from_raw_handle(handle) }
        }
    }

    #[repr(C, align(8))]
    struct OwnedSid {
        bytes: [u8; SECURITY_MAX_SID_SIZE as usize],
    }

    impl OwnedSid {
        fn as_psid(&self) -> PSID {
            self.bytes.as_ptr() as PSID
        }

        fn as_psid_mut(&mut self) -> PSID {
            self.bytes.as_mut_ptr() as PSID
        }
    }

    struct Listening {
        current: tokio::net::windows::named_pipe::NamedPipeServer,
        standby: tokio::net::windows::named_pipe::NamedPipeServer,
        descriptor: SecurityDescriptor,
        name: String,
    }

    pub async fn bind_listener(config: HostConfig) -> Result<HostListener, HostError> {
        let Some(name) = config.endpoint.pipe_name() else {
            return Err(HostError::Protocol("not a windows pipe endpoint".into()));
        };
        let name = name.to_owned();
        let sid = current_sid()?;
        let sid_text = sid_string(&sid)?;
        let descriptor = security_descriptor(&crate::pipe_dacl_sddl(&sid_text))?;
        let first_options = server_options(true)?;
        let current = match create_pipe(&first_options, &name, &descriptor) {
            Ok(pipe) => pipe,
            Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                let decision =
                    decide_first_instance(FirstInstance::AccessDenied, MutexFastPath::Free);
                return Err(decision.error.unwrap_or(HostError::AlreadyRunning));
            }
            Err(err) => return Err(HostError::Io(err.to_string())),
        };
        require_dacl(server_handle(&current), SE_KERNEL_OBJECT, &sid, false)?;
        let standby_options = server_options(false)?;
        let standby = create_pipe(&standby_options, &name, &descriptor)
            .map_err(|err| HostError::Io(err.to_string()))?;
        require_dacl(server_handle(&standby), SE_KERNEL_OBJECT, &sid, false)?;
        let (mutex, already_existed) = create_mutex(&config.endpoint, &descriptor)?;
        let fast = if already_existed {
            MutexFastPath::HeldElsewhere
        } else {
            MutexFastPath::Free
        };
        let decision = decide_first_instance(FirstInstance::Created, fast);
        if !decision.proceed || !decision.touch_token {
            return Err(decision.error.unwrap_or(HostError::AlreadyRunning));
        }
        let token_path = token_disk_path(&config.endpoint, true)?;
        let generated = AuthToken::generate()?;
        publish_token_file(&token_path, generated.as_bytes())?;
        let token = Arc::new(RwLock::new(generated));
        let publish_path = token_path.clone();
        let publish_token = Arc::clone(&token);
        let publish: crate::transport::TokenPublisher = Arc::new(move |bytes| {
            publish_token_file(&publish_path, bytes)?;
            *publish_token.write().unwrap_or_else(|err| err.into_inner()) = AuthToken::new(*bytes);
            Ok(())
        });
        let shared = Arc::new(Shared {
            token: Arc::clone(&token),
            attempts: std::sync::atomic::AtomicUsize::new(0),
            requests: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            garbage: std::sync::atomic::AtomicBool::new(false),
            slots: std::sync::atomic::AtomicUsize::new(0),
            frame_timeout: config.frame_timeout,
            write_timeout: config.write_timeout,
            handshake_timeout: config.handshake_timeout,
            max_unauth: config.max_unauthenticated,
            daemon_version: config.daemon_version,
            major: config.major,
            minor: config.minor,
            identity: config.endpoint.identity_bytes(),
            failures: std::sync::Mutex::new(None),
        });
        let (tx, rx) = mpsc::channel(64);
        let (stop_tx, stop_rx) = watch::channel(false);
        let task_shared = Arc::clone(&shared);
        let endpoint = config.endpoint.clone();
        let task = tokio::spawn(async move {
            accept_loop(
                Listening {
                    current,
                    standby,
                    descriptor,
                    name,
                },
                task_shared,
                tx,
                stop_rx,
            )
            .await;
        });
        let cleanup_token = Arc::clone(&token);
        let cleanup = Box::new(move || {
            remove_token_if_ours(&token_path, &cleanup_token);
            drop(mutex);
        }) as Box<dyn FnOnce() + Send>;
        Ok(HostListener {
            endpoint,
            incoming: rx,
            stop: stop_tx,
            task: Some(task),
            shared,
            publish,
            cleanup: Some(cleanup),
        })
    }

    pub async fn connect_client(
        endpoint: &HostEndpoint,
        options: &ConnectOptions,
    ) -> Result<AsyncHostClient, HostError> {
        let Some(name) = endpoint.pipe_name() else {
            return Err(HostError::Protocol("not a windows pipe endpoint".into()));
        };
        let name = name.to_owned();
        let first = read_token_file(endpoint)?;
        match connect_once(endpoint, &name, &first, options).await {
            Err(HostError::Auth) => {
                let fresh = read_token_file(endpoint)?;
                if constant_time_eq(fresh.as_bytes(), first.as_bytes()) {
                    Err(HostError::Auth)
                } else {
                    connect_once(endpoint, &name, &fresh, options).await
                }
            }
            other => other,
        }
    }

    async fn connect_once(
        endpoint: &HostEndpoint,
        name: &str,
        token: &AuthToken,
        options: &ConnectOptions,
    ) -> Result<AsyncHostClient, HostError> {
        if let Some(hook) = &options.pre_connect {
            hook();
        }
        let policy = NamedPipeClientOptions::identification();
        if !policy.sqos_identification || policy.inherit_handle {
            return Err(HostError::Protocol("pipe client policy".into()));
        }
        let pipe = retry_busy_within(HANDSHAKE_TIMEOUT, || open_client(name)).await?;
        peer_same_user(pipe.as_raw_handle() as HANDLE, true)?;
        let deadlines = Deadlines {
            handshake: HANDSHAKE_TIMEOUT,
            frame: FRAME_READ_TIMEOUT,
            write: WRITE_TIMEOUT,
        };
        let hook = options.pre_proof.as_ref().map(|hook| hook.as_ref());
        let (pipe, major, minor) = auth::drive_client(
            pipe,
            &endpoint.identity_bytes(),
            token,
            options.major,
            options.minor,
            deadlines,
            None,
            hook,
        )
        .await?;
        Ok(AsyncHostClient::from_pipe(pipe, major, minor))
    }

    fn open_client(
        name: &str,
    ) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, std::io::Error> {
        // `security_qos_flags` also sets SECURITY_SQOS_PRESENT. Identification
        // stops the server from impersonating this client.
        let flags = SECURITY_IDENTIFICATION | SECURITY_SQOS_PRESENT;
        ClientOptions::new().security_qos_flags(flags).open(name)
    }

    enum PipeSlot {
        Primary,
        Secondary,
    }

    async fn accept_loop(
        pipes: Listening,
        shared: Arc<Shared>,
        tx: mpsc::Sender<HostConn>,
        mut stop: watch::Receiver<bool>,
    ) {
        let Listening {
            current,
            standby,
            descriptor,
            name,
        } = pipes;
        // Both stay open for the whole loop. A client may land on either
        // instance. The replacement is created before the connected instance
        // leaves its slot, so the name is never unheld.
        let mut primary = current;
        let mut secondary = standby;
        let mut ledger = InstanceLedger::first();
        ledger.precreate_standby();
        loop {
            let slot = tokio::select! {
                biased;
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        ledger.shutdown();
                        break;
                    }
                    continue;
                }
                result = primary.connect() => {
                    if result.is_err() || *stop.borrow() {
                        continue;
                    }
                    PipeSlot::Primary
                }
                result = secondary.connect() => {
                    if result.is_err() || *stop.borrow() {
                        continue;
                    }
                    PipeSlot::Secondary
                }
            };
            // Hold the connected instance until a new listener exists.
            let Some(next) = create_standby_until(&name, &descriptor, &mut stop).await else {
                ledger.shutdown();
                break;
            };
            let connected = match slot {
                PipeSlot::Primary => std::mem::replace(&mut primary, next),
                PipeSlot::Secondary => std::mem::replace(&mut secondary, next),
            };
            ledger.precreate_standby();
            if ledger.handoff().is_err() {
                continue;
            }
            dispatch(connected, &shared, &tx);
        }
    }

    async fn create_standby_until(
        name: &str,
        descriptor: &SecurityDescriptor,
        stop: &mut watch::Receiver<bool>,
    ) -> Option<tokio::net::windows::named_pipe::NamedPipeServer> {
        loop {
            if *stop.borrow() {
                return None;
            }
            match server_options(false).and_then(|options| {
                create_pipe(&options, name, descriptor)
                    .map_err(|err| HostError::Io(err.to_string()))
            }) {
                Ok(pipe) => return Some(pipe),
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    }

    fn dispatch(
        pipe: tokio::net::windows::named_pipe::NamedPipeServer,
        shared: &Arc<Shared>,
        tx: &mpsc::Sender<HostConn>,
    ) {
        if peer_same_user(server_handle(&pipe), false).is_err() {
            return;
        }
        let Some(slot) = reserve(shared) else {
            return;
        };
        let shared = Arc::clone(shared);
        let tx = tx.clone();
        tokio::spawn(async move {
            let _slot = slot;
            shared
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match admit(Arc::clone(&shared), ConnIo::Server(pipe)).await {
                Ok(conn) => {
                    let _ = tx.send(conn).await;
                }
                Err(err) => shared.note_handshake_failure(err),
            }
        });
    }

    fn server_options(first: bool) -> Result<ServerOptions, HostError> {
        let policy = NamedPipeServerOptions::authenticated_local(first);
        if !policy.byte_mode || !policy.reject_remote_clients || policy.inherit_handle {
            return Err(HostError::Protocol("pipe server policy".into()));
        }
        let mut options = ServerOptions::new();
        options
            .pipe_mode(PipeMode::Byte)
            .first_pipe_instance(policy.first_pipe_instance)
            .reject_remote_clients(true)
            .access_inbound(true)
            .access_outbound(true);
        Ok(options)
    }

    fn create_pipe(
        options: &ServerOptions,
        name: &str,
        descriptor: &SecurityDescriptor,
    ) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, std::io::Error> {
        let attrs = security_attributes(descriptor.as_ptr());
        // SAFETY: `attrs` is a live SECURITY_ATTRIBUTES. The descriptor stays
        // alive for the call. CreateNamedPipe copies it before returning.
        unsafe {
            options.create_with_security_attributes_raw(
                name,
                (&raw const attrs).cast_mut().cast::<core::ffi::c_void>(),
            )
        }
    }

    fn server_handle(pipe: &tokio::net::windows::named_pipe::NamedPipeServer) -> HANDLE {
        pipe.as_raw_handle() as HANDLE
    }

    fn create_mutex(
        endpoint: &HostEndpoint,
        descriptor: &SecurityDescriptor,
    ) -> Result<(OwnedWinHandle, bool), HostError> {
        let Some(name) = endpoint.mutex_name() else {
            return Err(HostError::Protocol("missing mutex name".into()));
        };
        let wide = wide(&name);
        let attrs = security_attributes(descriptor.as_ptr());
        // SAFETY: `wide` is NUL-terminated. `attrs` is live and non-inheritable.
        let handle = unsafe { CreateMutexW(&attrs, 0, wide.as_ptr()) };
        let owned = OwnedWinHandle::new(handle).map_err(|_| HostError::Io("mutex".into()))?;
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        Ok((owned, existed))
    }

    pub fn current_sid_string() -> Result<String, HostError> {
        sid_string(&current_sid()?)
    }

    fn current_sid() -> Result<OwnedSid, HostError> {
        let token = open_current_token()?;
        token_sid(token.as_raw())
    }

    fn open_current_token() -> Result<OwnedWinHandle, HostError> {
        let mut token = std::ptr::null_mut();
        // SAFETY: the process handle is the current process. The token is owned.
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        OwnedWinHandle::new(token)
    }

    /// Copy the SID out of the token buffer. The buffer is dropped before return.
    fn token_sid(token: HANDLE) -> Result<OwnedSid, HostError> {
        let mut needed = 0u32;
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        }
        if needed == 0 || needed > 65_536 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let words = usize::try_from(needed)
            .map_err(|_| HostError::PeerIdentityUnavailable)?
            .div_ceil(std::mem::size_of::<u64>())
            .max(1);
        let mut buf = vec![0u64; words];
        let capacity = u32::try_from(buf.len() * std::mem::size_of::<u64>())
            .map_err(|_| HostError::PeerIdentityUnavailable)?;
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                capacity,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        if !(buf.as_ptr() as usize).is_multiple_of(std::mem::align_of::<TOKEN_USER>()) {
            return Err(HostError::PeerIdentityUnavailable);
        }
        // SAFETY: `buf` is aligned for TOKEN_USER and holds `needed` initialized bytes.
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        let copied = copy_sid(user.User.Sid)?;
        drop(buf);
        Ok(copied)
    }

    fn copy_sid(src: PSID) -> Result<OwnedSid, HostError> {
        if src.is_null() || unsafe { IsValidSid(src) } == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let len = unsafe { GetLengthSid(src) };
        if len == 0 || len > SECURITY_MAX_SID_SIZE {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let mut owned = OwnedSid {
            bytes: [0u8; SECURITY_MAX_SID_SIZE as usize],
        };
        // SAFETY: the destination is 8-aligned and SECURITY_MAX_SID_SIZE bytes.
        let ok = unsafe { CopySid(SECURITY_MAX_SID_SIZE, owned.as_psid_mut(), src) };
        if ok == 0 || unsafe { IsValidSid(owned.as_psid()) } == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        Ok(owned)
    }

    fn sid_string(sid: &OwnedSid) -> Result<String, HostError> {
        let mut raw = std::ptr::null_mut();
        let ok = unsafe { ConvertSidToStringSidW(sid.as_psid(), &mut raw) };
        if ok == 0 || raw.is_null() {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let mut len = 0usize;
        unsafe {
            while len <= 256 && *raw.add(len) != 0 {
                len += 1;
            }
        }
        if len == 0 || len > 256 {
            unsafe { LocalFree(raw.cast()) };
            return Err(HostError::PeerIdentityUnavailable);
        }
        let slice = unsafe { std::slice::from_raw_parts(raw, len) };
        let text = String::from_utf16_lossy(slice);
        unsafe { LocalFree(raw.cast()) };
        if text.is_empty() {
            Err(HostError::PeerIdentityUnavailable)
        } else {
            Ok(text)
        }
    }

    pub fn peer_same_user(handle: HANDLE, server: bool) -> Result<(), HostError> {
        let mut pid = 0u32;
        let ok = if server {
            unsafe { GetNamedPipeServerProcessId(handle, &mut pid) }
        } else {
            unsafe { GetNamedPipeClientProcessId(handle, &mut pid) }
        };
        if ok == 0 || pid == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        // SAFETY: `pid` came from the pipe. A null process is a refusal.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        let process =
            OwnedWinHandle::new(process).map_err(|_| HostError::PeerIdentityUnavailable)?;
        let mut raw_token = std::ptr::null_mut();
        let opened = unsafe { OpenProcessToken(process.as_raw(), TOKEN_QUERY, &mut raw_token) };
        if opened == 0 {
            return Err(HostError::PeerIdentityUnavailable);
        }
        let token = OwnedWinHandle::new(raw_token)?;
        let peer = token_sid(token.as_raw())?;
        drop(token);
        drop(process);
        let own_token = open_current_token()?;
        let own = token_sid(own_token.as_raw())?;
        drop(own_token);
        if unsafe { EqualSid(peer.as_psid(), own.as_psid()) } != 0 {
            Ok(())
        } else {
            Err(HostError::ForeignIdentity)
        }
    }

    fn host_dir_path() -> Result<PathBuf, HostError> {
        let local = std::env::var_os("LOCALAPPDATA").ok_or(HostError::ParentDenied)?;
        if local.is_empty() {
            return Err(HostError::ParentDenied);
        }
        Ok(PathBuf::from(local).join("FerroLoop").join("host"))
    }

    fn token_name(endpoint: &HostEndpoint) -> Result<String, HostError> {
        let name = endpoint.token_file_name();
        if name.is_empty() || name.contains(['/', '\\', '\0']) || name.contains("..") {
            return Err(HostError::EndpointStateDenied);
        }
        Ok(name)
    }

    fn token_disk_path(endpoint: &HostEndpoint, create_dir: bool) -> Result<PathBuf, HostError> {
        let host = host_dir_path()?;
        let ferro = host.parent().ok_or(HostError::ParentDenied)?;
        if create_dir {
            ensure_private_dir(ferro)?;
            ensure_private_dir(&host)?;
        } else {
            require_existing_dir(ferro)?;
            require_existing_dir(&host)?;
        }
        Ok(host.join(token_name(endpoint)?))
    }

    fn ensure_private_dir(path: &Path) -> Result<(), HostError> {
        require_creation_policy()?;
        let sid = current_sid()?;
        let text = sid_string(&sid)?;
        match attributes(path)? {
            None => {
                let descriptor = security_descriptor(&crate::pipe_dacl_sddl(&text))?;
                let wide = wide_os(path.as_os_str());
                let attrs = security_attributes(descriptor.as_ptr());
                // SAFETY: `wide` is NUL-terminated. The DACL is applied at creation.
                let ok = unsafe { CreateDirectoryW(wide.as_ptr(), &attrs) };
                if ok == 0 && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
                    return Err(HostError::ParentDenied);
                }
            }
            Some(flags)
                if flags & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || flags & FILE_ATTRIBUTE_DIRECTORY == 0 =>
            {
                return Err(HostError::ParentDenied);
            }
            Some(_) => {}
        }
        require_existing_dir(path)
    }

    fn require_existing_dir(path: &Path) -> Result<(), HostError> {
        let sid = current_sid()?;
        let handle = open_directory(path)?;
        let info = file_info(handle.as_raw())?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        {
            return Err(HostError::ParentDenied);
        }
        require_dacl(handle.as_raw(), SE_FILE_OBJECT, &sid, false)
    }

    fn publish_token_file(path: &Path, bytes: &[u8; 32]) -> Result<(), HostError> {
        require_creation_policy()?;
        let sid = current_sid()?;
        if let Some(existing) = open_existing(path)? {
            require_private_file(&existing, &sid)?;
        }
        let dir = path.parent().ok_or(HostError::ParentDenied)?;
        let temp = temp_path(dir)?;
        let text = sid_string(&sid)?;
        let descriptor = security_descriptor(&crate::pipe_dacl_sddl(&text))?;
        if let Err(err) = write_new_file(&temp, &descriptor, bytes) {
            let _ = delete_path(&temp);
            return Err(err);
        }
        let from = wide_os(temp.as_os_str());
        let to = wide_os(path.as_os_str());
        // SAFETY: both paths are NUL-terminated. REPLACE_EXISTING publishes the new file.
        let ok = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            let _ = delete_path(&temp);
            return Err(HostError::Io("token replace".into()));
        }
        Ok(())
    }

    fn read_token_file(endpoint: &HostEndpoint) -> Result<AuthToken, HostError> {
        let path = token_disk_path(endpoint, false)?;
        let sid = current_sid()?;
        let handle = open_existing(&path)?.ok_or(HostError::EndpointStateDenied)?;
        require_private_file(&handle, &sid)?;
        let mut file = handle.into_file();
        let mut buf = [0u8; 32];
        file.read_exact(&mut buf)
            .map_err(|_| HostError::EndpointStateDenied)?;
        let mut extra = [0u8; 1];
        if file
            .read(&mut extra)
            .map_err(|_| HostError::EndpointStateDenied)?
            != 0
        {
            return Err(HostError::EndpointStateDenied);
        }
        Ok(AuthToken::new(buf))
    }

    fn remove_token_if_ours(path: &Path, token: &RwLock<AuthToken>) {
        let current = *token
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .as_bytes();
        let Ok(found) = (|| {
            let sid = current_sid()?;
            let handle = open_existing(path)?.ok_or(HostError::EndpointStateDenied)?;
            require_private_file(&handle, &sid)?;
            let mut file = handle.into_file();
            let mut buf = [0u8; 32];
            file.read_exact(&mut buf)?;
            Ok::<AuthToken, HostError>(AuthToken::new(buf))
        })() else {
            return;
        };
        if constant_time_eq(found.as_bytes(), &current) {
            let _ = delete_path(path);
        }
    }

    fn require_creation_policy() -> Result<(), HostError> {
        let create = TokenFileCreate::protected();
        if !create.disposition_create_new
            || !create.dacl_in_security_attributes
            || create.inherit_handle
            || create.post_write_acl_fixup
        {
            return Err(HostError::EndpointStateDenied);
        }
        Ok(())
    }

    fn write_new_file(
        path: &Path,
        descriptor: &SecurityDescriptor,
        bytes: &[u8; 32],
    ) -> Result<(), HostError> {
        let wide = wide_os(path.as_os_str());
        let attrs = security_attributes(descriptor.as_ptr());
        // SAFETY: CREATE_NEW refuses an existing name, including a planted reparse.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                &attrs,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        let owned = OwnedWinHandle::new(handle).map_err(|_| HostError::EndpointStateDenied)?;
        let mut file = owned.into_file();
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }

    fn require_private_file(handle: &OwnedWinHandle, sid: &OwnedSid) -> Result<(), HostError> {
        let info = file_info(handle.as_raw())?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
            || info.nNumberOfLinks != 1
        {
            return Err(HostError::EndpointStateDenied);
        }
        require_dacl(handle.as_raw(), SE_FILE_OBJECT, sid, true)
    }

    fn require_dacl(
        handle: HANDLE,
        object: i32,
        sid: &OwnedSid,
        check_owner: bool,
    ) -> Result<(), HostError> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the pointers are ours. On success `sd` owns the returned buffers.
        let status = unsafe {
            GetSecurityInfo(
                handle,
                object,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if status != ERROR_SUCCESS || sd.is_null() {
            return Err(HostError::EndpointStateDenied);
        }
        let result = dacl_matches(sd, owner, dacl, sid, check_owner);
        unsafe { LocalFree(sd) };
        result
    }

    fn dacl_matches(
        sd: PSECURITY_DESCRIPTOR,
        owner: PSID,
        dacl: *mut ACL,
        sid: &OwnedSid,
        check_owner: bool,
    ) -> Result<(), HostError> {
        let mut control = 0u16;
        let mut revision = 0u32;
        let ok = unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) };
        if ok == 0
            || control & SE_DACL_PRESENT == 0
            || control & SE_DACL_PROTECTED == 0
            || dacl.is_null()
        {
            return Err(HostError::EndpointStateDenied);
        }
        if check_owner && (owner.is_null() || unsafe { EqualSid(owner, sid.as_psid()) } == 0) {
            return Err(HostError::EndpointStateDenied);
        }
        // SAFETY: GetSecurityInfo returned this DACL inside `sd`.
        let acl = unsafe { &*dacl };
        if acl.AceCount != 1 {
            return Err(HostError::EndpointStateDenied);
        }
        let mut ace = std::ptr::null_mut();
        let ok = unsafe { GetAce(dacl, 0, &mut ace) };
        if ok == 0 || ace.is_null() {
            return Err(HostError::EndpointStateDenied);
        }
        // SAFETY: GetAce returned one ACE. The header is the first field.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags != 0 {
            return Err(HostError::EndpointStateDenied);
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let ace_sid = (&raw const allowed.SidStart)
            .cast_mut()
            .cast::<core::ffi::c_void>();
        if unsafe { EqualSid(ace_sid, sid.as_psid()) } == 0 {
            Err(HostError::EndpointStateDenied)
        } else {
            Ok(())
        }
    }

    fn open_existing(path: &Path) -> Result<Option<OwnedWinHandle>, HostError> {
        let wide = wide_os(path.as_os_str());
        // SAFETY: OPEN_REPARSE_POINT does not follow a reparse on the last component.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return if unsafe { GetLastError() } == ERROR_FILE_NOT_FOUND {
                Ok(None)
            } else {
                Err(HostError::EndpointStateDenied)
            };
        }
        OwnedWinHandle::new(handle).map(Some)
    }

    fn open_directory(path: &Path) -> Result<OwnedWinHandle, HostError> {
        let wide = wide_os(path.as_os_str());
        // SAFETY: BACKUP_SEMANTICS opens a directory. OPEN_REPARSE_POINT does not follow it.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        OwnedWinHandle::new(handle).map_err(|_| HostError::ParentDenied)
    }

    fn file_info(handle: HANDLE) -> Result<BY_HANDLE_FILE_INFORMATION, HostError> {
        // SAFETY: the structure is plain data. GetFileInformationByHandle writes it.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
        if ok == 0 {
            Err(HostError::EndpointStateDenied)
        } else {
            Ok(info)
        }
    }

    fn attributes(path: &Path) -> Result<Option<u32>, HostError> {
        let wide = wide_os(path.as_os_str());
        let flags = unsafe { GetFileAttributesW(wide.as_ptr()) };
        if flags == INVALID_FILE_ATTRIBUTES {
            let err = unsafe { GetLastError() };
            if err == ERROR_FILE_NOT_FOUND {
                Ok(None)
            } else {
                Err(HostError::ParentDenied)
            }
        } else {
            Ok(Some(flags))
        }
    }

    fn delete_path(path: &Path) -> Result<(), HostError> {
        let wide = wide_os(path.as_os_str());
        if unsafe { DeleteFileW(wide.as_ptr()) } == 0 {
            Err(HostError::Io("delete".into()))
        } else {
            Ok(())
        }
    }

    fn temp_path(dir: &Path) -> Result<PathBuf, HostError> {
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).map_err(|err| HostError::Io(err.to_string()))?;
        let mut name = String::from(".flh-");
        for byte in bytes {
            name.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
            name.push(char::from(b"0123456789abcdef"[(byte & 0xf) as usize]));
        }
        Ok(dir.join(name))
    }

    async fn retry_busy_within<T, F>(budget: Duration, mut open: F) -> Result<T, HostError>
    where
        F: FnMut() -> Result<T, std::io::Error>,
    {
        let start = Instant::now();
        loop {
            match open() {
                Ok(value) => return Ok(value),
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    if start.elapsed() >= budget {
                        return Err(HostError::Io(err.to_string()));
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(err) => return Err(HostError::Io(err.to_string())),
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
        if ok == 0 || raw.is_null() {
            return Err(HostError::Io("sddl".into()));
        }
        Ok(SecurityDescriptor(raw))
    }

    fn security_attributes(descriptor: *mut core::ffi::c_void) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn copied_sid_outlives_the_token_buffer() {
            let first = current_sid_string().unwrap();
            assert!(first.starts_with("S-1-"), "{first}");
            assert!(!first.contains('\0'));
            let mut hold = Vec::new();
            for _ in 0..64 {
                hold.push(vec![0x5au8; 8192]);
                assert_eq!(current_sid_string().unwrap(), first);
            }
        }

        #[tokio::test]
        async fn busy_retry_stops_at_the_deadline() {
            let budget = Duration::from_millis(200);
            let started = Instant::now();
            let err = retry_busy_within(budget, || {
                Err::<(), _>(std::io::Error::from_raw_os_error(ERROR_PIPE_BUSY as i32))
            })
            .await
            .unwrap_err();
            assert!(matches!(err, HostError::Io(_)), "{err:?}");
            let elapsed = started.elapsed();
            assert!(elapsed >= Duration::from_millis(180), "{elapsed:?}");
            assert!(elapsed < Duration::from_millis(800), "{elapsed:?}");
        }

        #[tokio::test]
        async fn busy_retry_succeeds_after_pipe_busy() {
            let mut calls = 0u32;
            let value = retry_busy_within(Duration::from_secs(2), || {
                calls += 1;
                if calls < 3 {
                    Err(std::io::Error::from_raw_os_error(ERROR_PIPE_BUSY as i32))
                } else {
                    Ok(9u32)
                }
            })
            .await
            .unwrap();
            assert_eq!(value, 9);
            assert!(calls >= 3);
        }

        #[tokio::test]
        async fn listener_publishes_token_and_accepts_same_user() {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let sid = current_sid_string().unwrap();
            let suffix = format!(
                "rev-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let endpoint = HostEndpoint::windows(&suffix, &sid).unwrap();
            let mut config = HostConfig::new(endpoint.clone());
            config.daemon_version = "fl-host-version-unique-zz".into();
            config.handshake_timeout = Duration::from_secs(2);
            let listener = HostListener::bind(config).await.unwrap();
            let bound = listener.endpoint().clone();
            let token_path = token_disk_path(&bound, false).unwrap();
            let expected = host_dir_path().unwrap().join(bound.token_file_name());
            assert_eq!(token_path, expected);
            assert!(
                token_path.ends_with(
                    Path::new("FerroLoop")
                        .join("host")
                        .join(bound.token_file_name())
                ),
                "{}",
                token_path.display()
            );
            let published = std::fs::read(&token_path).unwrap();
            assert_eq!(published.len(), 32);

            let err = HostListener::bind(HostConfig::new(bound.clone()))
                .await
                .unwrap_err();
            assert!(matches!(err, HostError::AlreadyRunning), "{err:?}");
            assert_eq!(std::fs::read(&token_path).unwrap(), published);

            let server = tokio::spawn(async move {
                let mut listener = listener;
                let mut conn = listener.accept().await.unwrap();
                let request = conn.recv().await.unwrap();
                let id = request.request_id();
                conn.respond(&crate::HostResponse::Listed {
                    id,
                    children: vec![],
                })
                .await
                .unwrap();
                listener
            });
            let mut client = AsyncHostClient::connect(&bound).await.unwrap();
            let response = client
                .request(crate::HostRequest::List {
                    id: crate::RequestId(1),
                })
                .await
                .unwrap();
            assert_eq!(response.request_id(), crate::RequestId(1));
            let listener = server.await.unwrap();
            listener.shutdown().await.unwrap();
            assert!(
                !token_path.exists(),
                "shutdown must remove the token it still owns"
            );

            let started = Instant::now();
            let listener = loop {
                match HostListener::bind(HostConfig::new(bound.clone())).await {
                    Ok(listener) => break listener,
                    Err(HostError::AlreadyRunning)
                        if started.elapsed() < Duration::from_secs(2) =>
                    {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(err) => panic!("rebind after shutdown: {err:?}"),
                }
            };
            let replacement = [0x3cu8; 32];
            std::fs::write(&token_path, replacement).unwrap();
            listener.shutdown().await.unwrap();
            assert_eq!(std::fs::read(&token_path).unwrap(), replacement);
            let _ = std::fs::remove_file(&token_path);
        }
    }
}

#[cfg(windows)]
pub use win32::current_sid_string;

#[cfg(windows)]
pub(crate) use win32::{bind_listener, connect_client};
