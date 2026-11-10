//! Bounded metadata adapters; never read store contents or contact endpoints.
use super::{RecoveryError, inventory_types::*};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 64;
pub const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024;
/// Installed binaries may be large; hash at most 512 MiB using a 64 KiB buffer.
pub const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
const HASH_BUFFER_BYTES: usize = 64 * 1024;
pub(super) fn store_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("fls" | "redb")
    )
}
pub fn metadata_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(super) fn epoch(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis())
}
#[cfg(unix)]
fn file_id(meta: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("{}:{}", meta.dev(), meta.ino()))
}
#[cfg(not(unix))]
fn file_id(_: &fs::Metadata) -> Option<String> {
    None
}
fn file_metadata(meta: &fs::Metadata) -> FileMetadata {
    let mut out = FileMetadata {
        size_bytes: Some(meta.len()),
        modified_unix_ms: meta.modified().ok().and_then(epoch),
        ..Default::default()
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        out.mode = Some(meta.permissions().mode() & 0o7777);
        out.owner_uid = Some(meta.uid());
        out.hardlink_count = Some(meta.nlink());
        out.filesystem_device = Some(meta.dev());
    }
    out
}
/// Open a regular, single-link artifact only after excluding known store aliases.
/// Callers must first establish a complete candidate scan for their declared roots.
fn open_artifact(
    path: &Path,
    store_paths: &[PathBuf],
    limit: u64,
) -> Result<fs::File, RecoveryError> {
    let refuse = || {
        RecoveryError::Refused(format!(
            "unsafe or oversized metadata artifact: {}",
            path.display()
        ))
    };
    let before = fs::symlink_metadata(path)
        .map_err(|e| RecoveryError::Metadata(format!("{}: {}", path.display(), e.kind())))?;
    if store_extension(path)
        || before.file_type().is_symlink()
        || !before.is_file()
        || before.len() > limit
    {
        return Err(refuse());
    }
    let canonical = fs::canonicalize(path).map_err(|_| refuse())?;
    if store_extension(&canonical) {
        return Err(refuse());
    }
    // Check candidates before even opening the artifact: a store symlink may
    // point to this otherwise ordinary, singly-linked metadata filename.
    let aliases_store = |meta: &fs::Metadata| {
        store_paths.iter().any(|store| {
            canonical == *store
                || fs::canonicalize(store).is_ok_and(|p| p == canonical)
                || fs::metadata(store)
                    .ok()
                    .is_some_and(|m| file_id(&m).is_some() && file_id(&m) == file_id(meta))
        })
    };
    if aliases_store(&before) {
        return Err(refuse());
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        if before.nlink() != 1 {
            return Err(refuse());
        }
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| refuse())?;
    let meta = file.metadata().map_err(|_| refuse())?;
    if !meta.is_file() || meta.len() > limit || file_id(&before) != file_id(&meta) {
        return Err(refuse());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(refuse());
        }
    }
    if aliases_store(&meta) {
        return Err(refuse());
    }
    Ok(file)
}
/// Read an explicitly named small metadata artifact after a complete candidate scan.
/// Never read a store or alias; all metadata artifacts retain the 1 MiB bound.
pub fn read_metadata_artifact(
    path: &Path,
    store_paths: &[PathBuf],
) -> Result<Vec<u8>, RecoveryError> {
    let file = open_artifact(path, store_paths, MAX_ARTIFACT_BYTES)?;
    let refuse = || {
        RecoveryError::Refused(format!(
            "unsafe or oversized metadata artifact: {}",
            path.display()
        ))
    };
    let mut bytes = Vec::new();
    file.take(MAX_ARTIFACT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| refuse())?;
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(refuse());
    }
    Ok(bytes)
}
/// The extra byte detects growth beyond the cap without an unbounded EOF loop.
fn stream_digest(reader: impl Read, limit: u64) -> std::io::Result<String> {
    let mut reader = reader.take(limit + 1);
    let mut buffer = [0u8; HASH_BUFFER_BYTES];
    let mut total = 0u64;
    let mut hash = Sha256::new();
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        total += count as u64;
        if total > limit {
            return Err(std::io::Error::other("executable byte limit exceeded"));
        }
        hash.update(&buffer[..count]);
    }
}
fn executable_digest(path: &Path, paths: &[PathBuf]) -> Result<String, RecoveryError> {
    let file = open_artifact(path, paths, MAX_EXECUTABLE_BYTES)?;
    stream_digest(file, MAX_EXECUTABLE_BYTES).map_err(|_| {
        RecoveryError::Refused(format!(
            "unsafe or oversized executable artifact: {}",
            path.display()
        ))
    })
}
fn links(path: &Path) -> Result<Vec<Link>, ()> {
    let mut current = path.to_path_buf();
    let mut found = Vec::new();
    for _ in 0..40 {
        let components: Vec<_> = current.components().collect();
        let mut prefix = PathBuf::new();
        let mut changed = false;
        for (i, component) in components.iter().enumerate() {
            prefix.push(component.as_os_str());
            if fs::symlink_metadata(&prefix).is_ok_and(|m| m.file_type().is_symlink()) {
                let target = fs::read_link(&prefix).map_err(|_| ())?;
                found.push(Link {
                    path: prefix.clone(),
                    target: target.clone(),
                });
                let mut resolved = if target.is_absolute() {
                    target
                } else {
                    prefix.parent().ok_or(())?.join(target)
                };
                for rest in &components[i + 1..] {
                    resolved.push(rest.as_os_str());
                }
                current = resolved;
                changed = true;
                break;
            }
        }
        if !changed {
            return Ok(found);
        }
    }
    Err(())
}
pub(super) fn scan(root: &Path, paths: &mut BTreeSet<PathBuf>, warnings: &mut Vec<String>) {
    let mut pending = vec![(root.to_path_buf(), 0)];
    let mut visited = 0;
    while let Some((path, depth)) = pending.pop() {
        visited += 1;
        if visited > MAX_ENTRIES || depth > MAX_DEPTH {
            warnings.push(format!("inventory scan limit at {}", path.display()));
            break;
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                warnings.push(format!("unreadable metadata: {}", path.display()));
                continue;
            }
        };
        if store_extension(&path) {
            paths.insert(path.clone());
        }
        if meta.file_type().is_symlink() {
            match fs::metadata(&path) {
                Ok(target) if target.is_dir() => warnings.push(format!(
                    "symlinked directory not traversed: {}",
                    path.display()
                )),
                Err(_) => warnings.push(format!(
                    "unresolved symlink target; directory traversal unknown: {}",
                    path.display()
                )),
                Ok(_) => {}
            }
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o444 == 0 {
                warnings.push(format!("unreadable directory: {}", path.display()));
                continue;
            }
        }
        let entries = match fs::read_dir(&path) {
            Ok(v) => v,
            Err(_) => {
                warnings.push(format!("unreadable directory: {}", path.display()));
                continue;
            }
        };
        let mut children = Vec::new();
        for entry in entries {
            if children.len() + pending.len() + visited >= MAX_ENTRIES {
                warnings.push(format!("inventory scan limit at {}", path.display()));
                break;
            }
            match entry {
                Ok(e) => children.push(e.path()),
                Err(_) => warnings.push(format!("unreadable entry in {}", path.display())),
            }
        }
        children.sort();
        for child in children.into_iter().rev() {
            pending.push((child, depth + 1));
        }
    }
}
/// Discover all candidates in declared roots before reading any metadata artifact.
/// A partial scan cannot establish that an artifact is not a store alias.
pub fn preflight_candidates(roots: &[PathBuf]) -> Result<Vec<PathBuf>, RecoveryError> {
    let scan = scan_candidates(roots)?;
    scan.require_complete()?;
    Ok(scan.paths)
}
/// Metadata-only scan results can be reported even when artifact reads are unsafe.
pub struct CandidateScan {
    pub paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
}
impl CandidateScan {
    pub fn require_complete(&self) -> Result<(), RecoveryError> {
        if self.warnings.is_empty() {
            return Ok(());
        }
        Err(RecoveryError::Refused(format!(
            "candidate preflight incomplete; metadata artifacts cannot safely be read: {}",
            self.warnings.join("; ")
        )))
    }
}
pub fn scan_candidates(roots: &[PathBuf]) -> Result<CandidateScan, RecoveryError> {
    if roots.is_empty() || roots.iter().any(|root| !absolute(root)) {
        return Err(RecoveryError::Refused(
            "explicit absolute scan roots without parent traversal are required".into(),
        ));
    }
    let mut paths = BTreeSet::new();
    let mut warnings = Vec::new();
    for root in roots {
        scan(root, &mut paths, &mut warnings);
    }
    warnings.sort();
    warnings.dedup();
    Ok(CandidateScan {
        paths: paths.into_iter().collect(),
        warnings,
    })
}

pub(super) fn absolute(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir))
}
pub(super) fn record(path: &Path, warnings: &mut Vec<String>) -> StoreRecord {
    let meta = fs::metadata(path).ok();
    let canonical = fs::canonicalize(path).ok();
    let chain = links(path).unwrap_or_else(|_| {
        warnings.push(format!("unresolved symlink chain: {}", path.display()));
        Vec::new()
    });
    let kind = match &meta {
        Some(m) if m.is_file() => "file",
        Some(m) if m.is_dir() => "directory",
        Some(_) => "other",
        None => "missing_or_unreadable",
    };
    if kind != "file" {
        warnings.push(format!("store target {kind}: {}", path.display()));
    }
    let metadata = meta.as_ref().map(file_metadata).unwrap_or_default();
    if metadata.hardlink_count.is_some_and(|n| n > 1) {
        warnings.push(format!(
            "hardlinked store requires ownership review: {}",
            path.display()
        ));
    }
    let key_path = canonical.as_deref().unwrap_or(path);
    StoreRecord {
        identity: StoreIdentity {
            catalog_key: format!(
                "path-sha256:{}",
                metadata_digest(key_path.as_os_str().as_encoded_bytes())
            ),
            given_paths: vec![path.to_path_buf()],
            canonical_path: canonical,
            exists: meta.is_some(),
            file_type: kind.into(),
            symlink_chain: chain.clone(),
            filesystem_identity: meta.as_ref().and_then(file_id),
            identity_confidence: if meta.is_some() {
                "filesystem_observation_not_persistent_uuid"
            } else {
                "unresolved_path"
            }
            .into(),
            alias_status: if !chain.is_empty() || metadata.hardlink_count.is_some_and(|n| n > 1) {
                AliasStatus::ReviewRequired
            } else if meta.as_ref().and_then(file_id).is_some() {
                AliasStatus::NoAliasObserved
            } else {
                AliasStatus::Unknown
            },
            intrinsic_lineage: None,
        },
        classification: "unclassified".into(),
        classification_evidence: Vec::new(),
        owner_decision: None,
        metadata,
        ownership: Ownership::default(),
        consumers: Vec::new(),
        recovery: Vec::new(),
    }
}
pub(super) fn unit_metadata(
    unit: &UnitInput,
    paths: &[PathBuf],
    scan_complete: bool,
) -> UnitMetadata {
    let mut out = UnitMetadata {
        declared: unit.clone(),
        unit_digest: None,
        installed_executable_digest: None,
        file_binding_verified: false,
        validation_warnings: Vec::new(),
    };
    if !scan_complete {
        out.validation_warnings.push(format!(
            "candidate scan incomplete; unit and executable artifact reads deferred: {}",
            unit.name
        ));
    } else {
        match read_metadata_artifact(&unit.file, paths) {
            Ok(bytes) => {
                out.unit_digest = Some(metadata_digest(&bytes));
                // Only an unambiguous literal argv token is accepted. Complex systemd
                // expansion/quoting remains supplied evidence, never a parsed binding.
                let text = String::from_utf8_lossy(&bytes);
                out.file_binding_verified = text
                    .lines()
                    .filter_map(|l| l.strip_prefix("ExecStart="))
                    .any(|line| {
                        if line.contains(['\"', '\\', '\'', '$', '%', ';']) {
                            return false;
                        }
                        let args: Vec<_> = line.split_whitespace().collect();
                        let executable = args
                            .first()
                            .and_then(|a| Path::new(a).file_name())
                            .and_then(|n| n.to_str());
                        let serving = match executable {
                            Some("ferrostore") => args.get(1) == Some(&"serve"),
                            Some("fl") => args.windows(2).any(|a| a == ["store", "serve"]),
                            _ => false,
                        };
                        serving
                            && args.windows(2).any(|a| {
                                matches!(a[0], "--store" | "-s" | "--db")
                                    && Path::new(a[1]) == unit.store_path
                            })
                    });
                if !out.file_binding_verified {
                    out.validation_warnings.push(format!(
                        "unit binding not verified from literal ExecStart: {}",
                        unit.name
                    ));
                }
            }
            Err(_) => out
                .validation_warnings
                .push(format!("unit file unavailable or refused: {}", unit.name)),
        }
        if let Some(path) = &unit.installed_executable {
            match executable_digest(path, paths) {
                Ok(digest) => out.installed_executable_digest = Some(digest),
                Err(_) => out.validation_warnings.push(format!(
                    "executable digest unavailable or over 512 MiB executable limit: {}",
                    unit.name
                )),
            }
        }
    }
    for observation in [
        &unit.observation,
        &unit.observed_access_mode,
        &unit.installed_build,
        &unit.running_build,
    ]
    .into_iter()
    .flatten()
    {
        if !observation.attributed() {
            out.validation_warnings.push(format!(
                "unattributed supplied unit observation: {}",
                unit.name
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_executable_above_metadata_limit_is_hashed() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("fl");
        let bytes = vec![42; 2 * 1024 * 1024];
        fs::write(&executable, &bytes).unwrap();
        let unit = UnitInput {
            installed_executable: Some(executable.clone()),
            ..Default::default()
        };
        assert_eq!(
            unit_metadata(&unit, &[], true).installed_executable_digest,
            Some(metadata_digest(&bytes))
        );
        assert!(read_metadata_artifact(&executable, &[]).is_err());
    }
    #[test]
    fn executable_cap_refuses_sparse_oversized_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fl");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_EXECUTABLE_BYTES + 1)
            .unwrap();
        assert!(executable_digest(&path, &[]).is_err());
        let unit = UnitInput {
            installed_executable: Some(path),
            ..Default::default()
        };
        let observed = unit_metadata(&unit, &[], true);
        assert_eq!(observed.installed_executable_digest, None);
        assert!(
            observed
                .validation_warnings
                .iter()
                .any(|w| w.contains("512 MiB"))
        );
    }

    #[test]
    fn executable_stream_bounds_growth_and_handles_exact_boundary() {
        struct Growing {
            bytes: u64,
            largest_buffer: usize,
        }
        impl Read for Growing {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.largest_buffer = self.largest_buffer.max(buffer.len());
                buffer.fill(42);
                self.bytes += buffer.len() as u64;
                Ok(buffer.len())
            }
        }
        let limit = 3 * 64 * 1024;
        let mut growing = Growing {
            bytes: 0,
            largest_buffer: 0,
        };
        assert!(stream_digest(&mut growing, limit).is_err());
        assert_eq!(growing.bytes, limit + 1);
        assert_eq!(growing.largest_buffer, 64 * 1024);
        assert_eq!(
            stream_digest(&b"hello"[..], 5).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(stream_digest(&b"hello!"[..], 5).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn executable_digest_refuses_store_extensions_and_all_alias_directions() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store.fls");
        fs::write(&store, b"fixture").unwrap();
        assert!(executable_digest(&store, &[]).is_err());
        let alias = root.path().join("fl");
        symlink(&store, &alias).unwrap();
        assert!(executable_digest(&alias, &[]).is_err());
        fs::remove_file(&alias).unwrap();
        fs::hard_link(&store, &alias).unwrap();
        assert!(executable_digest(&alias, &[]).is_err());
        fs::remove_file(&alias).unwrap();
        fs::write(&alias, b"fixture").unwrap();
        let reverse = root.path().join("reverse.fls");
        symlink(&alias, &reverse).unwrap();
        assert!(executable_digest(&alias, &[reverse]).is_err());
        assert!(executable_digest(root.path(), &[]).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn executable_reverse_alias_is_refused_before_open() {
        use std::os::fd::{AsRawFd, FromRawFd};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fl");
        fs::write(&path, b"fixture").unwrap();
        let alias = root.path().join("reverse.fls");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0);
        let fd = unsafe { fs::File::from_raw_fd(fd) };
        let watched = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert!(
            unsafe {
                libc::inotify_add_watch(
                    fd.as_raw_fd(),
                    watched.as_ptr(),
                    libc::IN_OPEN | libc::IN_ACCESS,
                )
            } >= 0
        );
        assert!(executable_digest(&path, &[alias]).is_err());
        let mut bytes = [0u8; 4096];
        assert_eq!(
            unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[cfg(unix)]
    #[test]
    fn unresolved_symlink_target_is_an_incomplete_candidate_scan() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("missing-target", root.path().join("unknown")).unwrap();
        let roots = [root.path().to_path_buf()];
        let scan = scan_candidates(&roots).unwrap();
        assert!(
            scan.warnings
                .iter()
                .any(|w| w.contains("unresolved symlink target"))
        );
        assert!(preflight_candidates(&roots).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unresolved_symlink_target_defers_all_unit_artifact_opens() {
        use std::os::fd::{AsRawFd, FromRawFd};
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("missing-target", root.path().join("unknown")).unwrap();
        let unit = outside.path().join("unit.service");
        let executable = outside.path().join("fl");
        fs::write(&unit, b"# fixture").unwrap();
        fs::write(&executable, b"fixture").unwrap();
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0);
        let fd = unsafe { fs::File::from_raw_fd(fd) };
        for path in [&unit, &executable] {
            let watched = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            assert!(
                unsafe {
                    libc::inotify_add_watch(
                        fd.as_raw_fd(),
                        watched.as_ptr(),
                        libc::IN_OPEN | libc::IN_ACCESS,
                    )
                } >= 0
            );
        }
        let map = super::super::inventory::collect_inventory(&InventoryInputs {
            roots: vec![root.path().to_path_buf()],
            deployment: DeploymentMetadata {
                units: vec![UnitInput {
                    file: unit,
                    installed_executable: Some(executable),
                    store_path: root.path().join("store.fls"),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        let mut bytes = [0u8; 4096];
        assert_eq!(
            unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) },
            -1,
            "unknown directory scope must not authorize artifact opens"
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!map.complete);
        assert_eq!(map.stores[0].ownership.units[0].unit_digest, None);
        assert_eq!(
            map.stores[0].ownership.units[0].installed_executable_digest,
            None
        );
    }
}
