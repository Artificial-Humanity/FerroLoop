//! Endpoint identity. Unix paths grow `.lock`, `.token`, and `.state.json`.
//! Windows pipe suffixes hash to mutex and file names. The same endpoint,
//! including ASCII case aliases, shares one identity.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::types::HostError;

#[derive(Clone, PartialEq, Eq)]
pub enum HostEndpoint {
    Unix(PathBuf),
    Windows {
        full_name: String,
        token_file: String,
        state_file: String,
        mutex: String,
    },
}

impl std::fmt::Debug for HostEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostEndpoint(redacted)")
    }
}

impl HostEndpoint {
    pub fn unix(path: impl AsRef<Path>) -> Result<Self, HostError> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(HostError::Protocol("unix endpoint must be absolute".into()));
        }
        if path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(HostError::Protocol("unix endpoint contains ..".into()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bytes = path.as_os_str().as_bytes();
            if bytes.is_empty() || bytes.contains(&0) || !socket_path_fits(path) {
                return Err(HostError::Protocol(
                    "unix endpoint is empty, contains NUL, or exceeds sun_path".into(),
                ));
            }
        }
        #[cfg(not(unix))]
        {
            let text = path.to_string_lossy();
            if text.is_empty() || text.contains('\0') || text.len() >= 108 {
                return Err(HostError::Protocol("unix endpoint is not usable".into()));
            }
        }
        Ok(Self::Unix(path.to_path_buf()))
    }

    pub fn windows(suffix: &str, sid: &str) -> Result<Self, HostError> {
        let suffix = validate_pipe_suffix(suffix)?;
        if sid.is_empty() || sid.as_bytes().contains(&0) || !sid.is_ascii() {
            return Err(HostError::Protocol("windows sid".into()));
        }
        let full_name = format!(r"\\.\pipe\fl-host-{sid}-{suffix}");
        if full_name.chars().count() > 256 {
            return Err(HostError::Protocol(
                "pipe name exceeds 256 characters".into(),
            ));
        }
        let hash = sha256_hex(full_name.as_bytes());
        Ok(Self::Windows {
            full_name,
            token_file: format!("{hash}.token"),
            state_file: format!("{hash}.state.json"),
            mutex: format!(r"Local\fl-host-{hash}"),
        })
    }

    #[cfg(unix)]
    pub fn discover_unix_path_with(
        host_sock: Option<&std::ffi::OsStr>,
        xdg_runtime_dir: Option<&std::ffi::OsStr>,
        run_user_dir: Option<&Path>,
        home: Option<&std::ffi::OsStr>,
        euid: u32,
    ) -> PathBuf {
        if let Some(configured) = host_sock.filter(|s| !s.is_empty()) {
            return PathBuf::from(configured);
        }
        if let Some(runtime) = xdg_runtime_dir.filter(|s| !s.is_empty()) {
            let p = PathBuf::from(runtime);
            if p.is_absolute() {
                return p.join("ferroloop/fl-host.sock");
            }
        }
        if let Some(dir) = run_user_dir.and_then(|d| probe_explicit_run_user_dir(d, euid)) {
            return dir.join("ferroloop/fl-host.sock");
        }
        if let Some(home) = home.filter(|s| !s.is_empty()) {
            let p = PathBuf::from(home);
            if p.is_absolute() {
                return p.join(".cache/fl/host.sock");
            }
        }
        PathBuf::from(format!("/tmp/ferroloop-{euid}/fl-host.sock"))
    }

    #[cfg(unix)]
    pub fn discover_unix_path_for_root(
        host_sock: Option<&std::ffi::OsStr>,
        xdg_runtime_dir: Option<&std::ffi::OsStr>,
        run_user_root: &Path,
        home: Option<&std::ffi::OsStr>,
        euid: u32,
    ) -> PathBuf {
        let probed = probe_run_user_dir(run_user_root, euid);
        Self::discover_unix_path_with(host_sock, xdg_runtime_dir, probed.as_deref(), home, euid)
    }

    pub fn discover() -> Result<Self, HostError> {
        if let Ok(configured) = std::env::var("FL_HOST_SOCK")
            && !configured.is_empty()
        {
            #[cfg(unix)]
            {
                return Self::unix(configured);
            }
            #[cfg(windows)]
            {
                return Self::windows(
                    &configured,
                    &crate::transport::windows::current_sid_string()?,
                );
            }
        }
        #[cfg(target_os = "linux")]
        {
            let uid = unix_euid();
            let path = if let Some(raw) = std::env::var_os("FL_TEST_RUN_USER_DIR") {
                let candidate = PathBuf::from(raw);
                Self::discover_unix_path_with(
                    std::env::var_os("FL_HOST_SOCK").as_deref(),
                    std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
                    Some(candidate.as_path()),
                    std::env::var_os("HOME").as_deref(),
                    uid,
                )
            } else {
                Self::discover_unix_path_for_root(
                    std::env::var_os("FL_HOST_SOCK").as_deref(),
                    std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
                    Path::new("/run/user"),
                    std::env::var_os("HOME").as_deref(),
                    uid,
                )
            };
            Self::unix(path)
        }
        #[cfg(target_os = "macos")]
        {
            Self::unix(std::env::temp_dir().join("ferroloop/fl-host.sock"))
        }
        #[cfg(windows)]
        {
            Self::windows("fl-host", &crate::transport::windows::current_sid_string()?)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            Err(HostError::Protocol("no default host endpoint".into()))
        }
    }

    pub fn socket_path(&self) -> &Path {
        match self {
            Self::Unix(path) => path,
            Self::Windows { full_name, .. } => Path::new(full_name.as_str()),
        }
    }

    pub fn lock_path(&self) -> PathBuf {
        self.with_suffix(".lock")
    }

    pub fn token_path(&self) -> PathBuf {
        self.with_suffix(".token")
    }

    pub fn state_path(&self) -> PathBuf {
        self.with_suffix(".state.json")
    }

    pub fn token_file_name(&self) -> String {
        match self {
            Self::Unix(path) => file_name_string(&self.with_suffix(".token"), path),
            Self::Windows { token_file, .. } => token_file.clone(),
        }
    }

    pub fn state_file_name(&self) -> String {
        match self {
            Self::Unix(_) => file_name_only(&self.state_path()),
            Self::Windows { state_file, .. } => state_file.clone(),
        }
    }

    pub fn mutex_name(&self) -> Option<String> {
        match self {
            Self::Unix(_) => None,
            Self::Windows { mutex, .. } => Some(mutex.clone()),
        }
    }

    pub fn identity_bytes(&self) -> Vec<u8> {
        match self {
            Self::Unix(path) => os_bytes(path),
            Self::Windows { full_name, .. } => full_name.as_bytes().to_vec(),
        }
    }

    pub fn pipe_name(&self) -> Option<&str> {
        match self {
            Self::Unix(_) => None,
            Self::Windows { full_name, .. } => Some(full_name.as_str()),
        }
    }

    fn with_suffix(&self, suffix: &str) -> PathBuf {
        match self {
            Self::Unix(path) => {
                let mut owned = path.as_os_str().to_os_string();
                owned.push(suffix);
                PathBuf::from(owned)
            }
            Self::Windows { token_file, .. } => PathBuf::from(format!("{token_file}{suffix}")),
        }
    }
}

fn file_name_string(path: &Path, _unix: &Path) -> String {
    file_name_only(path)
}

fn file_name_only(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn os_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned().into_bytes()
    }
}

fn validate_pipe_suffix(suffix: &str) -> Result<String, HostError> {
    if suffix.is_empty() || suffix.len() > 200 || suffix == ".." {
        return Err(HostError::Protocol("pipe suffix".into()));
    }
    if !suffix
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(HostError::Protocol("pipe suffix".into()));
    }
    Ok(suffix.to_ascii_lowercase())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[cfg(unix)]
fn socket_path_fits(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let sun = std::mem::size_of::<libc::sockaddr_un>()
        - std::mem::offset_of!(libc::sockaddr_un, sun_path);
    path.as_os_str().as_bytes().len() < sun
}

#[cfg(unix)]
pub fn probe_explicit_run_user_dir(candidate: &Path, expected_uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    if !candidate.is_absolute() {
        return None;
    }
    let meta = std::fs::symlink_metadata(candidate).ok()?;
    if !meta.file_type().is_dir() || meta.uid() != expected_uid {
        return None;
    }
    Some(candidate.to_path_buf())
}

#[cfg(unix)]
pub fn probe_run_user_dir(run_user_root: &Path, expected_uid: u32) -> Option<PathBuf> {
    if !run_user_root.is_absolute() {
        return None;
    }
    let candidate = run_user_root.join(expected_uid.to_string());
    probe_explicit_run_user_dir(&candidate, expected_uid)
}

#[cfg(unix)]
pub(crate) fn unix_euid() -> u32 {
    // SAFETY: geteuid has no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn discover_unix_path_with_falls_back_through_xdg_run_user_home_and_tmp() {
        let uid = unix_euid();
        let tmp = tempfile::tempdir().unwrap();
        let run_user = tmp.path().join("run-user");
        std::fs::create_dir_all(&run_user).unwrap();
        let home = tmp.path().join("home");

        assert_eq!(
            HostEndpoint::discover_unix_path_with(
                Some(OsStr::new("/custom/host.sock")),
                Some(OsStr::new("/xdg/runtime")),
                Some(&run_user),
                Some(home.as_os_str()),
                uid,
            ),
            PathBuf::from("/custom/host.sock")
        );

        assert_eq!(
            HostEndpoint::discover_unix_path_with(
                None,
                Some(OsStr::new("/xdg/runtime")),
                Some(&run_user),
                Some(home.as_os_str()),
                uid,
            ),
            PathBuf::from("/xdg/runtime/ferroloop/fl-host.sock")
        );

        assert_eq!(
            HostEndpoint::discover_unix_path_with(
                None,
                None,
                Some(&run_user),
                Some(home.as_os_str()),
                uid,
            ),
            run_user.join("ferroloop/fl-host.sock")
        );

        let missing_run_user = tmp.path().join("missing-run-user");
        assert_eq!(
            HostEndpoint::discover_unix_path_with(
                None,
                None,
                Some(&missing_run_user),
                Some(home.as_os_str()),
                uid,
            ),
            home.join(".cache/fl/host.sock")
        );

        assert_eq!(
            HostEndpoint::discover_unix_path_with(None, None, None, None, uid),
            PathBuf::from(format!("/tmp/ferroloop-{uid}/fl-host.sock"))
        );
    }

    #[test]
    fn discover_unix_path_for_root_checks_ownership_symlink_and_missing_dir() {
        let uid = unix_euid();
        let other_uid = uid.wrapping_add(1);
        let tmp = tempfile::tempdir().unwrap();
        let run_root = tmp.path().join("run/user");
        let home = tmp.path().join("home");

        // 1. Missing `/run/user/<uid>` directory -> falls back to HOME/.cache
        std::fs::create_dir_all(&run_root).unwrap();
        assert_eq!(
            HostEndpoint::discover_unix_path_for_root(
                None,
                None,
                &run_root,
                Some(home.as_os_str()),
                uid,
            ),
            home.join(".cache/fl/host.sock")
        );

        // 2. Real `<root>/<uid>` directory owned by `uid` -> succeeds
        let my_run_dir = run_root.join(uid.to_string());
        std::fs::create_dir_all(&my_run_dir).unwrap();
        assert_eq!(
            HostEndpoint::discover_unix_path_for_root(
                None,
                None,
                &run_root,
                Some(home.as_os_str()),
                uid,
            ),
            my_run_dir.join("ferroloop/fl-host.sock")
        );

        // 3. Wrong owner (`<root>/<other_uid>` owned by `uid` != `other_uid`) -> rejected
        let wrong_owner_dir = run_root.join(other_uid.to_string());
        std::fs::create_dir_all(&wrong_owner_dir).unwrap();
        assert_eq!(
            HostEndpoint::discover_unix_path_for_root(
                None,
                None,
                &run_root,
                Some(home.as_os_str()),
                other_uid,
            ),
            home.join(".cache/fl/host.sock")
        );
        assert_eq!(
            HostEndpoint::discover_unix_path_with(
                None,
                None,
                Some(&wrong_owner_dir),
                Some(home.as_os_str()),
                other_uid,
            ),
            home.join(".cache/fl/host.sock")
        );

        // 4. Symlinked `<root>/<uid>` directory -> rejected by `symlink_metadata().file_type().is_dir()`
        std::fs::remove_dir(&my_run_dir).unwrap();
        let real_target = tmp.path().join("real-target");
        std::fs::create_dir_all(&real_target).unwrap();
        std::os::unix::fs::symlink(&real_target, &my_run_dir).unwrap();
        assert_eq!(
            HostEndpoint::discover_unix_path_for_root(
                None,
                None,
                &run_root,
                Some(home.as_os_str()),
                uid,
            ),
            home.join(".cache/fl/host.sock")
        );
        assert_eq!(
            HostEndpoint::discover_unix_path_with(
                None,
                None,
                Some(&my_run_dir),
                Some(home.as_os_str()),
                uid,
            ),
            home.join(".cache/fl/host.sock")
        );
    }
}
