//! Nonblocking, process-shared lifecycle ownership. Lock contents are diagnostics only.
//! Order: native file, optional tmux file, local mutex, store guard. Never unlink a lock.
use super::{LifecycleEnv, LifecycleError};
use fl_host_proto::HostEndpoint;
#[cfg(unix)]
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
struct RuntimePin {
    endpoint: HostEndpoint,
    tmux: Option<PathBuf>,
}
thread_local! {
    static RUNTIME_PIN: RefCell<Option<RuntimePin>> = const { RefCell::new(None) };
}

/// A thread-bound proof of ownership, borrowed by nested remodel operations.
#[derive(Debug)]
pub(crate) struct LifecycleFence {
    personas: Vec<String>,
    runtime: RuntimePin,
    files: Vec<File>,
    #[cfg(test)]
    paths: Vec<PathBuf>,
    // A scoped runtime pin must be dropped on its originating thread.
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

pub(super) struct PinScope(Option<RuntimePin>, fl_wire::runtime_context::RuntimeScope);
impl Drop for PinScope {
    fn drop(&mut self) {
        let _ = &self.1;
        RUNTIME_PIN.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

pub(super) fn pinned_endpoint() -> Option<HostEndpoint> {
    RUNTIME_PIN.with_borrow(|pin| pin.as_ref().map(|p| p.endpoint.clone()))
}

impl LifecycleFence {
    #[cfg(test)]
    pub(super) fn acquire(
        env: &LifecycleEnv<'_>,
        persona: &str,
        operation: &str,
    ) -> Result<Self, LifecycleError> {
        Self::acquire_many(env, &[persona.to_string()], operation)
    }

    pub(super) fn acquire_many(
        env: &LifecycleEnv<'_>,
        personas: &[String],
        operation: &str,
    ) -> Result<Self, LifecycleError> {
        let endpoint = resolve_endpoint(env)?;
        let tmux = if std::env::var_os("FL_WIRE_TMUX_BIN").is_some()
            || env.runtime.requires_tmux_fence()
        {
            Some(resolve_tmux_socket(env)?)
        } else {
            None
        };
        Self::acquire_endpoints(endpoint, tmux, personas, operation)
    }

    #[cfg(test)]
    pub(super) fn acquire_endpoint(
        endpoint: HostEndpoint,
        tmux: Option<PathBuf>,
        persona: &str,
        operation: &str,
    ) -> Result<Self, LifecycleError> {
        Self::acquire_endpoints(endpoint, tmux, &[persona.to_string()], operation)
    }

    fn acquire_endpoints(
        endpoint: HostEndpoint,
        tmux: Option<PathBuf>,
        personas: &[String],
        operation: &str,
    ) -> Result<Self, LifecycleError> {
        let mut personas: Vec<String> = personas
            .iter()
            .map(|p| p.trim().to_ascii_lowercase())
            .collect();
        personas.sort();
        personas.dedup();
        for persona in &personas {
            fl_core::state::validate_slug(persona)
                .map_err(|e| LifecycleError::Invalid(format!("lifecycle persona: {e}")))?;
        }
        let endpoint = match endpoint {
            HostEndpoint::Unix(path) => {
                HostEndpoint::unix(canonical_socket(&path)?).map_err(|e| failure("endpoint", e))?
            }
            HostEndpoint::Windows { .. } => {
                return Err(failure(
                    "endpoint",
                    format!(
                        "secure lifecycle fencing is unsupported on platform {}",
                        std::env::consts::OS
                    ),
                ));
            }
        };
        let tmux = tmux.map(|p| canonical_socket(&p)).transpose()?;
        // Same-rank personas sort identically in every process: all native, then all tmux.
        let mut paths = Vec::new();
        if let HostEndpoint::Unix(socket) = &endpoint {
            for persona in &personas {
                paths.push(lock_path(socket, persona, "host")?);
            }
        }
        if let Some(socket) = &tmux {
            for persona in &personas {
                paths.push(lock_path(socket, persona, "tmux")?);
            }
        }
        let mut files = Vec::with_capacity(paths.len());
        for (index, path) in paths.iter().enumerate() {
            let mut file = safe_open(path)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    let mut detail = String::new();
                    let _ = (&mut file).take(512).read_to_string(&mut detail);
                    let detail = holder_diagnostic(&detail);
                    return Err(LifecycleError::Conflict(format!(
                        "lifecycle operation for `{}` is in progress; holder {}",
                        personas[index % personas.len()],
                        detail
                    )));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(failure("lock", e)),
            }
            // Contents are bounded and never inspected for authority, liveness or stealing.
            let metadata = serde_json::json!({
                "pid": std::process::id(),
                "operation": operation.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(48).collect::<String>(),
                "started_at_ms": super::now_ms(),
            });
            // Diagnostics cannot turn acquired ownership into a refusal.
            let _ = file
                .set_len(0)
                .and_then(|()| file.seek(SeekFrom::Start(0)))
                .and_then(|_| file.write_all(metadata.to_string().as_bytes()));
            files.push(file);
        }
        Ok(Self {
            personas,
            runtime: RuntimePin { endpoint, tmux },
            files,
            #[cfg(test)]
            paths,
            _thread_bound: std::marker::PhantomData,
        })
    }

    pub(crate) fn covers(&self, persona: &str) -> bool {
        self.personas
            .iter()
            .any(|p| p.eq_ignore_ascii_case(persona.trim()))
    }
    pub(super) fn pin(&self) -> PinScope {
        PinScope(
            RUNTIME_PIN.with(|slot| slot.replace(Some(self.runtime.clone()))),
            fl_wire::runtime_context::pin(self.runtime.endpoint.clone(), self.runtime.tmux.clone()),
        )
    }
    #[cfg(all(test, unix))]
    pub(super) fn descriptors_are_cloexec(&self) -> bool {
        use std::os::fd::AsRawFd;
        self.files.iter().all(|file| {
            // SAFETY: this fence owns the descriptor throughout the call.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
            flags >= 0 && flags & libc::FD_CLOEXEC != 0
        })
    }
    #[cfg(test)]
    pub(super) fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}
impl Drop for LifecycleFence {
    fn drop(&mut self) {
        // Keep files and diagnostic contents in place: replacing an inode splits ownership.
        for file in self.files.iter().rev() {
            let _ = file.unlock();
        }
    }
}

// Only bounded, typed diagnostic fields may enter a response. Never expose raw file text.
fn holder_diagnostic(raw: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Holder {
        pid: u32,
        operation: String,
        started_at_ms: u64,
    }
    match serde_json::from_str::<Holder>(raw) {
        Ok(holder)
            if holder.operation.len() <= 48
                && holder
                    .operation
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-') =>
        {
            format!(
                "pid {}, operation {}, started_at_ms {}",
                holder.pid, holder.operation, holder.started_at_ms
            )
        }
        _ => "metadata unavailable".into(),
    }
}

fn failure(action: &str, error: impl std::fmt::Display) -> LifecycleError {
    LifecycleError::Store(format!("lifecycle fence {action}: {error}"))
}
fn resolve_endpoint(_env: &LifecycleEnv<'_>) -> Result<HostEndpoint, LifecycleError> {
    // Unit fixtures with an isolated home keep their own fence even when another
    // parallel test has registered its mock host for process-wide discovery.
    #[cfg(test)]
    if let Some(home) = _env.home_override {
        return HostEndpoint::unix(home.join("test-lifecycle-host.sock"))
            .map_err(|e| failure("discovery", e));
    }
    HostEndpoint::discover().map_err(|e| failure("discovery", e))
}

fn canonical_socket(path: &Path) -> Result<PathBuf, LifecycleError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(failure("endpoint", "requires an absolute path without .."));
    }
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(failure("endpoint", "cannot resolve socket symlink"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| failure("endpoint", "missing parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| failure("endpoint", "missing socket name"))?;
    let parent = parent
        .canonicalize()
        .map_err(|e| failure("endpoint parent", e))?;
    Ok(parent.join(name))
}

fn lock_path(socket: &Path, persona: &str, kind: &str) -> Result<PathBuf, LifecycleError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let uid = unsafe { libc::geteuid() };
        let directory = socket
            .parent()
            .ok_or_else(|| failure("directory", "missing parent"))?
            .join(format!(".fl-lifecycle-{uid}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(failure("directory", e)),
        }
        let metadata =
            std::fs::symlink_metadata(&directory).map_err(|e| failure("directory", e))?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
            return Err(failure(
                "directory",
                "requires a user-owned 0700 directory without symlinks",
            ));
        }
        let mut hash = Sha256::new();
        hash.update(kind.as_bytes());
        hash.update([0]);
        hash.update(socket.as_os_str().as_bytes());
        hash.update([0]);
        hash.update(persona.as_bytes());
        Ok(directory.join(format!("{:x}.lock", hash.finalize())))
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, persona, kind);
        Err(failure(
            "directory",
            format!(
                "secure lifecycle fencing is unsupported on platform {}",
                std::env::consts::OS
            ),
        ))
    }
}

/// A separate process-shared lock for the version observer. Launches never acquire it.
pub(super) fn try_version_observer(store_file: &Path) -> Result<Option<File>, LifecycleError> {
    let path = lock_path(store_file, "", "vendor-version-observer")?;
    let file = safe_open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(err)) => Err(failure("lock", err)),
    }
}

fn safe_open(path: &Path) -> Result<File, LifecycleError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| failure("open", e))?;
        let metadata = file.metadata().map_err(|e| failure("file", e))?;
        let named = std::fs::symlink_metadata(path).map_err(|e| failure("file identity", e))?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
            || metadata.dev() != named.dev()
            || metadata.ino() != named.ino()
        {
            return Err(failure(
                "file",
                "requires one user-owned regular 0600 inode without symlinks",
            ));
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(failure(
            "open",
            format!(
                "secure lifecycle fencing is unsupported on platform {}",
                std::env::consts::OS
            ),
        ))
    }
}

fn resolve_tmux_socket(_env: &LifecycleEnv<'_>) -> Result<PathBuf, LifecycleError> {
    #[cfg(test)]
    if let Some(home) = _env.home_override {
        return Ok(home.join("test-lifecycle-tmux.sock"));
    }
    tmux_socket()
}

fn tmux_socket() -> Result<PathBuf, LifecycleError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        if let Some(tmux) = std::env::var_os("TMUX").filter(|v| !v.is_empty()) {
            let mut fields = tmux.as_bytes().split(|b| *b == b',');
            let raw = fields.next().unwrap_or_default();
            if raw.is_empty() || fields.next().is_none() || fields.next().is_none() {
                return Err(failure(
                    "tmux",
                    "cannot resolve malformed TMUX socket identity",
                ));
            }
            return Ok(PathBuf::from(std::ffi::OsString::from_vec(raw.to_vec())));
        }
        let root = std::env::var_os("TMUX_TMPDIR")
            .filter(|v| !v.is_empty())
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        Ok(root.join(format!("tmux-{}/default", unsafe { libc::getuid() })))
    }
    #[cfg(not(unix))]
    {
        Err(failure(
            "tmux",
            format!(
                "tmux fencing is unsupported on platform {}",
                std::env::consts::OS
            ),
        ))
    }
}

#[cfg(all(test, not(unix)))]
#[test]
fn unsupported_platform_fails_closed_before_runtime_effects() {
    let endpoint = HostEndpoint::windows("fixture", "S-1-5-21-1").unwrap();
    let error = LifecycleFence::acquire_endpoint(endpoint, None, "fiona", "restart").unwrap_err();
    assert!(error.to_string().contains(std::env::consts::OS));
}
