//! Owner-only directories: a directory `fl` creates to hold secrets or private state is mode 0700
//! at most, whatever the umask says (the umask can only narrow it, as `0277` would), and a
//! directory `fl` did not create keeps its mode.
//!
//! `fs::create_dir_all` takes its mode from the umask, so under the common umask `002` a new
//! `~/.config/fl` came out `0775`, and any group member could replace `vault.key` or a hub token
//! inside it. [`create_private_dir_all`] is the one way `fl` makes such a directory. It lives here
//! so the vault (`fl-gate`), the store (`fl-store`, which re-exports it) and the CLI share it.
//!
//! An existing directory is never chmodded: its mode is the owner's choice, and `fl doctor`
//! (`G7.vault-perms`, the store warnings) reports one that others can write.

use std::io;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

/// The mode of a private directory `fl` creates.
#[cfg(unix)]
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// Create `path` and any missing parents; the directory `path` itself gets mode 0700 when this
/// call creates it (at most 0700: a umask can only narrow it). Intermediate directories this call creates use the default mode (the umask):
/// `fl` owns the leaf (`~/.config/fl`, `~/.local/share/fl`), not the `~/.config` or
/// `~/.local/share` above it, and a private mode on a shared parent could lock other users or
/// tools out of a path `fl` does not own. A directory that already exists is left exactly as it
/// is, mode included. A path that exists and is not a directory is an error, as with
/// [`std::fs::create_dir_all`]. Non-unix targets have no mode bits, so there it is
/// `create_dir_all`.
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() || path.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::DirBuilder::new()
            .mode(PRIVATE_DIR_MODE)
            .create(path)
        {
            Ok(()) => Ok(()),
            // Another process made it between the check and the create: its mode stands.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The assertions of the umask test, run in a child under `umask 0`: a plain `mkdir` there
    /// gives `0777`, so `0700` can only come from the helper.
    fn a_new_leaf_is_0700() {
        let tmp = TempDir::new().unwrap();
        let probe = tmp.path().join("probe");
        std::fs::create_dir(&probe).unwrap();
        assert_eq!(mode(&probe), 0o777, "the child must run under umask 0");
        let leaf = tmp.path().join("config").join("fl");
        create_private_dir_all(&leaf).unwrap();
        assert_eq!(mode(&leaf), 0o700, "the directory fl created");
        assert_eq!(
            mode(&tmp.path().join("config")),
            0o777,
            "an intermediate keeps the default mode"
        );
    }

    #[test]
    fn a_new_leaf_is_0700_whatever_the_umask() {
        const CHILD: &str = "FL_CORE_PRIVATE_DIR_UMASK_CHILD";
        if std::env::var_os(CHILD).is_some() {
            a_new_leaf_is_0700();
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("umask 0; exec \"$0\" --exact private_dir::tests::a_new_leaf_is_0700_whatever_the_umask --test-threads=1")
            .arg(exe)
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "child failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // A filter that matched no test also exits 0: the child must have run exactly this one.
        assert!(
            stdout.contains("1 passed"),
            "the child did not run the test:\n{stdout}"
        );
    }

    #[test]
    fn only_the_new_leaf_is_private_and_existing_directories_are_untouched() {
        let tmp = TempDir::new().unwrap();
        let existing = tmp.path().join("shared");
        std::fs::create_dir(&existing).unwrap();
        set_mode(&existing, 0o755);

        // An existing directory keeps its mode, whatever it is.
        create_private_dir_all(&existing).unwrap();
        assert_eq!(mode(&existing), 0o755);
        let open_dir = tmp.path().join("open");
        std::fs::create_dir(&open_dir).unwrap();
        set_mode(&open_dir, 0o775);
        create_private_dir_all(&open_dir).unwrap();
        assert_eq!(mode(&open_dir), 0o775);

        // Under an existing directory: only the new leaf is 0700, the parent is as it was.
        let leaf = existing.join("fl");
        create_private_dir_all(&leaf).unwrap();
        assert_eq!(mode(&leaf), 0o700);
        assert_eq!(mode(&existing), 0o755);

        // Several levels: intermediates take the default mode (what a plain `mkdir` beside them
        // gets under this umask), the leaf is 0700.
        let deep = existing.join("a").join("b").join("c");
        create_private_dir_all(&deep).unwrap();
        assert_eq!(mode(&deep), 0o700);
        let probe = existing.join("probe");
        std::fs::create_dir(&probe).unwrap();
        assert_eq!(
            mode(&existing.join("a")),
            mode(&probe),
            "intermediates take the default mode"
        );

        // A second call, and a path under a file, behave like `create_dir_all`.
        create_private_dir_all(&deep).unwrap();
        assert_eq!(mode(&deep), 0o700);
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(create_private_dir_all(&file.join("sub")).is_err());
        assert!(create_private_dir_all(&file).is_err());
    }
}
