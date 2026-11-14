//! Path checks for private test fixture endpoints.
//!
//! This is an accident-prevention tripwire, not a sandbox. The filesystem can
//! change after this check, so it does not protect against hostile same-user
//! replacement or other time-of-check/time-of-use races. Callers must invoke
//! it immediately before the operation they intend to guard.

use std::io;
use std::path::{Component, Path, PathBuf};

/// An endpoint was refused by the fixture-root check.
///
/// Callers can distinguish this from an operating-system permission failure
/// by inspecting the payload of the returned [`io::Error`].
#[derive(Debug)]
pub struct FixtureEndpointRefusal {
    reason: String,
}

impl std::fmt::Display for FixtureEndpointRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "fixture endpoint refused: {}", self.reason)
    }
}

impl std::error::Error for FixtureEndpointRefusal {}

/// True when an I/O error carries a fixture-endpoint refusal payload.
pub fn is_fixture_endpoint_refusal(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|cause| cause.is::<FixtureEndpointRefusal>())
}

fn refused(reason: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        FixtureEndpointRefusal {
            reason: reason.to_string(),
        },
    )
}

fn validate_absolute(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return Err(refused("path must be absolute without parent traversal"));
    }
    Ok(())
}

/// Check an endpoint against a private fixture root, if one is configured.
///
/// Both arguments must be absolute and contain no `..` components. The root
/// must already exist as a directory. Existing endpoint components are
/// canonicalized; for a missing suffix, its nearest existing ancestor is
/// canonicalized. Existing dangling symlinks are refused. Filesystem errors
/// fail closed. When `root` is `None`, the endpoint is left unchecked.
///
/// This does not validate Windows named-pipe identities, which are not normal
/// filesystem paths. It also does not perform or authorize the operation.
pub fn check_endpoint(root: Option<&Path>, endpoint: &Path) -> io::Result<()> {
    let Some(root) = root else { return Ok(()) };
    validate_absolute(root)?;
    validate_absolute(endpoint)?;
    let canonical_root = std::fs::canonicalize(root).map_err(refused)?;
    if !std::fs::metadata(&canonical_root)
        .map_err(refused)?
        .is_dir()
    {
        return Err(refused("fixture root is not a directory"));
    }

    let mut existing = PathBuf::new();
    for component in endpoint.components() {
        existing.push(component.as_os_str());
        match std::fs::symlink_metadata(&existing) {
            Ok(_) => {
                // This also refuses dangling symlinks instead of treating
                // them as a missing path component.
                std::fs::canonicalize(&existing).map_err(refused)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                existing.pop();
                let ancestor = std::fs::canonicalize(&existing).map_err(refused)?;
                return if ancestor.starts_with(&canonical_root) {
                    Ok(())
                } else {
                    Err(refused("endpoint is outside the fixture root"))
                };
            }
            Err(error) => return Err(refused(error)),
        }
    }
    let resolved = std::fs::canonicalize(endpoint).map_err(refused)?;
    if resolved.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(refused("endpoint is outside the fixture root"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_root_does_not_change_resolution() {
        assert!(check_endpoint(None, Path::new("relative/../socket")).is_ok());
    }

    #[test]
    fn existing_and_missing_paths_inside_root_are_allowed() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let file = nested.join("existing");
        std::fs::write(&file, b"").unwrap();
        assert!(check_endpoint(Some(root.path()), &file).is_ok());
        assert!(check_endpoint(Some(root.path()), &nested.join("missing/socket")).is_ok());
    }

    #[test]
    fn fixture_refusal_has_distinct_error_payload() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let refusal = check_endpoint(Some(root.path()), &outside.path().join("wire.sock"))
            .expect_err("outside endpoint must be refused");
        assert_eq!(refusal.kind(), io::ErrorKind::PermissionDenied);
        assert!(is_fixture_endpoint_refusal(&refusal));
        assert!(refusal.get_ref().unwrap().is::<FixtureEndpointRefusal>());
        assert!(
            refusal
                .to_string()
                .contains("endpoint is outside the fixture root")
        );
        let ordinary = io::Error::new(io::ErrorKind::PermissionDenied, "ordinary denial");
        assert!(!is_fixture_endpoint_refusal(&ordinary));
        assert!(!is_fixture_endpoint_refusal(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn outside_endpoint_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let error = check_endpoint(Some(root.path()), &other.path().join("wire.sock"))
            .expect_err("outside endpoint must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn filesystem_failure_is_a_typed_refusal_with_its_reason() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing-root");
        let cause = std::fs::canonicalize(&missing).unwrap_err();
        let refusal = check_endpoint(Some(&missing), &missing.join("socket")).unwrap_err();
        assert!(is_fixture_endpoint_refusal(&refusal));
        assert_eq!(refusal.kind(), io::ErrorKind::PermissionDenied);
        assert!(refusal.to_string().contains(&cause.to_string()));
    }

    #[test]
    fn relative_parent_and_invalid_roots_are_refused() {
        let root = tempfile::tempdir().unwrap();
        for endpoint in [PathBuf::from("relative"), root.path().join("a/../socket")] {
            assert_eq!(
                check_endpoint(Some(root.path()), &endpoint)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        assert!(check_endpoint(Some(Path::new("relative")), root.path()).is_err());
        assert!(check_endpoint(Some(&root.path().join("missing")), root.path()).is_err());
        let file = root.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(check_endpoint(Some(&file), &file).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escapes_and_dangling_symlinks_are_refused() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        symlink(root.path().join("missing"), root.path().join("dangling")).unwrap();
        for path in [
            root.path().join("escape/socket"),
            root.path().join("dangling/socket"),
        ] {
            assert_eq!(
                check_endpoint(Some(root.path()), &path).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn aliases_to_the_same_root_are_allowed() {
        use std::os::unix::fs::symlink;
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        let alias = parent.path().join("alias");
        std::fs::create_dir(&root).unwrap();
        symlink(&root, &alias).unwrap();
        assert!(check_endpoint(Some(&alias), &root.join("missing/socket")).is_ok());
        assert!(check_endpoint(Some(&root), &alias.join("missing/socket")).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_fd_alias_of_fixture_root_is_allowed() {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap();
        let held = std::fs::File::open(root.path()).unwrap();
        let alias = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            held.as_raw_fd()
        ));
        assert!(check_endpoint(Some(root.path()), &alias.join("wire.sock")).is_ok());
        assert!(check_endpoint(Some(&alias), &root.path().join("wire.sock")).is_ok());
    }
}
