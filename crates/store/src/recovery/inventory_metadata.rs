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
/// Read only an explicitly named small metadata artifact, never a store or alias.
/// Link rejection plus fstat guards avoid a hardlink or rename race before reading.
pub fn read_metadata_artifact(
    path: &Path,
    store_paths: &[PathBuf],
) -> Result<Vec<u8>, RecoveryError> {
    let refuse = || {
        RecoveryError::Refused(format!(
            "unsafe or oversized metadata artifact: {}",
            path.display()
        ))
    };
    let before = fs::symlink_metadata(path)
        .map_err(|e| RecoveryError::Metadata(format!("{}: {}", path.display(), e.kind())))?;
    if store_extension(path) || before.file_type().is_symlink() || !before.is_file() {
        return Err(refuse());
    }
    let canonical = fs::canonicalize(path).map_err(|_| refuse())?;
    if store_extension(&canonical) {
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
    if !meta.is_file() || meta.len() > MAX_ARTIFACT_BYTES || file_id(&before) != file_id(&meta) {
        return Err(refuse());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(refuse());
        }
    }
    for store in store_paths {
        if canonical == *store
            || fs::metadata(store)
                .ok()
                .is_some_and(|m| file_id(&m).is_some() && file_id(&m) == file_id(&meta))
        {
            return Err(refuse());
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_ARTIFACT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| refuse())?;
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(refuse());
    }
    Ok(bytes)
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
            if fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
                warnings.push(format!(
                    "symlinked directory not traversed: {}",
                    path.display()
                ));
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
            symlink_chain: chain,
            filesystem_identity: meta.as_ref().and_then(file_id),
            identity_confidence: if meta.is_some() {
                "filesystem_observation_not_persistent_uuid"
            } else {
                "unresolved_path"
            }
            .into(),
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
pub(super) fn unit_metadata(unit: &UnitInput, paths: &[PathBuf]) -> UnitMetadata {
    let mut out = UnitMetadata {
        declared: unit.clone(),
        unit_digest: None,
        installed_executable_digest: None,
        file_binding_verified: false,
        validation_warnings: Vec::new(),
    };
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
        match read_metadata_artifact(path, paths) {
            Ok(bytes) => out.installed_executable_digest = Some(metadata_digest(&bytes)),
            Err(_) => out.validation_warnings.push(format!(
                "executable digest unavailable or over bounded artifact limit: {}",
                unit.name
            )),
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
