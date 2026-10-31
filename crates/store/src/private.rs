//! Private permissions for a store: what `fl` itself creates is owner-only, and what it did not
//! create is never changed.
//!
//! A store holds hub pairing codes, session hashes and other records, so the file `fl` creates is
//! mode 0600 and a store directory `fl` creates is mode 0700, whatever the umask says. The rule
//! has two halves, and the second matters as much as the first:
//!
//! * [`create_private_dir_all`] and the file creation in [`RedbStore::open`](crate::RedbStore::open)
//!   give owner-only modes to what they create, and to nothing else. An existing directory or file
//!   (`~/.local/share`, a project directory, `/tmp`, a store made by an older `fl`) keeps its mode.
//! * An existing store that others can reach is only reported: [`warn_if_loose`] prints the path,
//!   its mode and the `chmod` that fixes it, once per path per process. `fl` never runs that
//!   `chmod` for you: the store may be shared on purpose, and a surprise mode change is worse than
//!   a reminder.
//!
//! Non-unix targets have no mode bits to set or read, so they behave as before.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

/// The mode of a store file `fl` creates.
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;
/// The mode of a store directory `fl` creates.
#[cfg(unix)]
const DIR_MODE: u32 = 0o700;
/// The most paths the once-per-path warning remembers. A process opens a handful of stores; the
/// bound only stops a pathological caller from growing the set without end. Past it, no new path
/// is warned about.
#[cfg_attr(not(unix), allow(dead_code))]
const MAX_WARNED: usize = 1024;

/// Create `path` and any missing parents; the directory `path` itself gets mode 0700 when this
/// call creates it. Intermediate directories this call creates use the default mode (the umask):
/// `fl` owns the leaf that holds the store, not the `~/.local/share` or `~/project` above it, and
/// a private mode on a shared parent could lock other users or tools out of a path `fl` does not
/// own. A directory that already exists is left exactly as it is, mode included. A path that
/// exists and is not a directory is an error, as with [`std::fs::create_dir_all`].
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() || path.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::DirBuilder::new().mode(DIR_MODE).create(path) {
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

/// Create the empty store file at `path` with mode 0600, unless something is already there.
/// `Ok(true)` when this call made it, `Ok(false)` when the path existed (a file, a symlink, a
/// dangling symlink: the exclusive create refuses all of them and follows none) or another
/// process created it first. The empty file it leaves is what `redb::Database::create` accepts
/// as a new database, so the caller opens the path as before. Any other error is returned for
/// the caller to ignore: the open that follows reports a real fault in its own words.
pub(crate) fn create_private_file(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(FILE_MODE);
        match opts.open(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

/// Paths already warned about in this process.
static WARNED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Warnings printed so far, kept only under test so a test can count them without parsing stderr.
#[cfg(test)]
pub(crate) static EMITTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Claim `path` in `seen`: true the first time, false after, and false once the set is full.
/// Poison-tolerant: a panic elsewhere must not turn into a missing warning or a second panic.
#[cfg_attr(not(unix), allow(dead_code))]
fn claim(seen: &Mutex<BTreeSet<PathBuf>>, path: &Path) -> bool {
    let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() >= MAX_WARNED || seen.contains(path) {
        return false;
    }
    seen.insert(path.to_path_buf());
    true
}

/// The warnings an existing store at `store` earns, each one only for a path `seen` has not
/// claimed yet. A path that cannot be read earns none: a stat error is not a finding. The text
/// names the path, its mode and the fix; it never holds store contents.
pub(crate) fn loose_warnings(store: &Path, seen: &Mutex<BTreeSet<PathBuf>>) -> Vec<String> {
    #[cfg(unix)]
    {
        let mut out = Vec::new();
        // `metadata` follows a symlink: the mode that matters is the file that holds the data.
        if let Ok(meta) = std::fs::metadata(store)
            && meta.is_file()
            && meta.mode() & 0o077 != 0
            && claim(seen, store)
        {
            out.push(format!(
                "warning: the store {} is mode {:03o}, so other users can read it; \
                 fix it with: chmod 600 {}",
                store.display(),
                meta.mode() & 0o777,
                store.display()
            ));
        }
        let dir = match store.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        // A sticky directory (`/tmp`) is shared by design: its owner is not this store's.
        if let Ok(meta) = std::fs::metadata(dir)
            && meta.is_dir()
            && meta.mode() & 0o077 != 0
            && meta.mode() & 0o1000 == 0
            && claim(seen, dir)
        {
            out.push(format!(
                "warning: the directory {} holds a store and is mode {:03o}, so other users can \
                 reach it; if only your own data lives there, fix it with: chmod 700 {}",
                dir.display(),
                meta.mode() & 0o777,
                dir.display()
            ));
        }
        out
    }
    #[cfg(not(unix))]
    {
        let _ = (store, seen);
        Vec::new()
    }
}

/// Print, to stderr, one warning per path per process for a store at `store` that other users
/// can reach (file mode, or its directory's mode, has a group or other bit). Never changes a
/// mode, never fails and never panics.
pub fn warn_if_loose(store: &Path) {
    for text in loose_warnings(store, &WARNED) {
        eprintln!("{text}");
        #[cfg(test)]
        EMITTED.lock().unwrap_or_else(|e| e.into_inner()).push(text);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::RedbStore;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::Barrier;
    use tempfile::TempDir;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The warnings the open of `store` printed, found by the unique temp path they name.
    fn emitted_for(dir: &Path) -> Vec<String> {
        let needle = dir.display().to_string();
        EMITTED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|w| w.contains(&needle))
            .cloned()
            .collect()
    }

    /// The work of the umask test. It runs in a child process that `sh` started under `umask 0`,
    /// because the umask is per process and a test thread must not change it for the others.
    fn new_store_in_a_fresh_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("fl");
        let store = dir.join("fl.redb");
        create_private_dir_all(&dir).unwrap();
        drop(RedbStore::open(&store).unwrap());
        assert_eq!(mode(&dir), 0o700, "the directory fl created");
        assert_eq!(mode(&store), 0o600, "the store file fl created");
    }

    #[test]
    fn a_new_store_and_its_directory_are_private() {
        new_store_in_a_fresh_dir();
    }

    #[test]
    fn the_modes_do_not_depend_on_the_umask() {
        const CHILD: &str = "FL_STORE_PRIVATE_UMASK_CHILD";
        if std::env::var_os(CHILD).is_some() {
            // Proof the child really runs permissive: a plain create would be 0666 here.
            let probe = TempDir::new().unwrap().keep().join("probe");
            drop(std::fs::File::create(&probe).unwrap());
            assert_eq!(mode(&probe), 0o666, "the child must run under umask 0");
            new_store_in_a_fresh_dir();
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("umask 0; exec \"$0\" --exact private::tests::the_modes_do_not_depend_on_the_umask --test-threads=1")
            .arg(exe)
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn only_the_leaf_directory_fl_creates_is_private_and_existing_ones_are_untouched() {
        let tmp = TempDir::new().unwrap();
        let existing = tmp.path().join("shared");
        std::fs::create_dir(&existing).unwrap();
        set_mode(&existing, 0o755);

        // An existing directory keeps its mode, whatever it is.
        create_private_dir_all(&existing).unwrap();
        assert_eq!(mode(&existing), 0o755);
        let open_dir = tmp.path().join("open");
        std::fs::create_dir(&open_dir).unwrap();
        set_mode(&open_dir, 0o777);
        create_private_dir_all(&open_dir).unwrap();
        assert_eq!(mode(&open_dir), 0o777);

        // Under an existing directory: only the new leaf is 0700, the parent is as it was.
        let leaf = existing.join("fl");
        create_private_dir_all(&leaf).unwrap();
        assert_eq!(mode(&leaf), 0o700);
        assert_eq!(mode(&existing), 0o755);

        // Several levels: intermediates take the default mode, the leaf is 0700.
        let deep = existing.join("a").join("b").join("c");
        create_private_dir_all(&deep).unwrap();
        assert_eq!(mode(&deep), 0o700);
        assert_ne!(
            mode(&existing.join("a")),
            0o700,
            "intermediates are not private"
        );
        assert_ne!(
            mode(&existing.join("a/b")),
            0o700,
            "intermediates are not private"
        );

        // A second call, and a path that is a file, behave like `create_dir_all`.
        create_private_dir_all(&deep).unwrap();
        assert_eq!(mode(&deep), 0o700);
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(create_private_dir_all(&file.join("sub")).is_err());
    }

    #[test]
    fn an_existing_loose_store_is_warned_about_once_and_never_chmodded() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("old");
        std::fs::create_dir(&dir).unwrap();
        set_mode(&dir, 0o755);
        let path = dir.join("fl.redb");
        // Made the way an older fl made it: by redb, with the default mode.
        drop(redb::Database::create(&path).unwrap());
        set_mode(&path, 0o644);

        drop(RedbStore::open(&path).unwrap());
        assert_eq!(mode(&path), 0o644, "the file is not chmodded");
        assert_eq!(mode(&dir), 0o755, "the directory is not chmodded");
        let first = emitted_for(&dir);
        assert_eq!(
            first.len(),
            2,
            "one for the file, one for the dir: {first:?}"
        );
        let file_warning = first.iter().find(|w| w.contains("chmod 600")).unwrap();
        assert!(file_warning.contains(&path.display().to_string()));
        assert!(file_warning.contains("644"));
        let dir_warning = first.iter().find(|w| w.contains("chmod 700")).unwrap();
        assert!(dir_warning.contains("755"));

        // A second open of the same path is quiet.
        drop(RedbStore::open(&path).unwrap());
        assert_eq!(
            emitted_for(&dir),
            first,
            "no second warning for the same path"
        );
    }

    #[test]
    fn a_private_store_earns_no_warning() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("fl");
        create_private_dir_all(&dir).unwrap();
        let path = dir.join("fl.redb");
        drop(RedbStore::open(&path).unwrap());
        drop(RedbStore::open(&path).unwrap());
        assert_eq!(emitted_for(&dir), Vec::<String>::new());
    }

    #[test]
    fn the_warning_dedupes_per_path_stays_bounded_and_survives_poison() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        set_mode(&dir, 0o755);
        let path = dir.join("s.redb");
        std::fs::write(&path, b"").unwrap();
        set_mode(&path, 0o664);
        let seen = Mutex::new(BTreeSet::new());

        assert_eq!(loose_warnings(&path, &seen).len(), 2);
        assert_eq!(
            loose_warnings(&path, &seen).len(),
            0,
            "one warning per path"
        );
        // A second store in the same directory: the directory is not repeated.
        let other = dir.join("t.redb");
        std::fs::write(&other, b"").unwrap();
        set_mode(&other, 0o600);
        assert_eq!(loose_warnings(&other, &seen).len(), 0);

        // A stat error is no warning, and no panic.
        assert!(loose_warnings(&tmp.path().join("missing/x.redb"), &seen).is_empty());

        // The set is bounded: a full one warns about nothing new.
        let full = Mutex::new(
            (0..MAX_WARNED)
                .map(|i| PathBuf::from(format!("/p/{i}")))
                .collect(),
        );
        assert!(loose_warnings(&path, &full).is_empty());

        // A poisoned set still works.
        let poisoned = Arc::new(Mutex::new(BTreeSet::new()));
        let p2 = Arc::clone(&poisoned);
        let _ = std::thread::spawn(move || {
            let _held = p2.lock().unwrap();
            panic!("poison the set");
        })
        .join();
        assert!(poisoned.is_poisoned());
        assert_eq!(loose_warnings(&path, &poisoned).len(), 2);
    }

    #[test]
    fn a_sticky_shared_directory_is_not_called_loose() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("tmp-like");
        std::fs::create_dir(&dir).unwrap();
        set_mode(&dir, 0o1777);
        let path = dir.join("s.redb");
        std::fs::write(&path, b"").unwrap();
        set_mode(&path, 0o600);
        assert!(loose_warnings(&path, &Mutex::new(BTreeSet::new())).is_empty());
    }

    #[test]
    fn an_existing_empty_file_is_accepted_as_a_new_store() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.redb");
        std::fs::write(&path, b"").unwrap();
        set_mode(&path, 0o640);
        let store = RedbStore::open(&path).unwrap();
        drop(store);
        assert!(
            std::fs::metadata(&path).unwrap().len() > 0,
            "redb initialised it"
        );
        assert_eq!(mode(&path), 0o640, "an existing file keeps its mode");
        drop(RedbStore::open(&path).unwrap());
    }

    #[test]
    fn two_first_opens_at_once_both_succeed() {
        for round in 0..5 {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join(format!("race{round}.redb"));
            let barrier = Arc::new(Barrier::new(2));
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    let path = path.clone();
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        drop(RedbStore::open(&path).expect("a first open must not fail"));
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(mode(&path), 0o600);
        }
        // The helper itself: a lost create race is not an error.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("x.redb");
        assert!(create_private_file(&path).unwrap());
        assert!(!create_private_file(&path).unwrap());
    }

    #[test]
    fn a_symlink_at_the_store_path_is_followed_by_redb_and_chmodded_by_nobody() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real.redb");
        drop(redb::Database::create(&real).unwrap());
        set_mode(&real, 0o644);
        let link = tmp.path().join("link.redb");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        drop(RedbStore::open(&link).unwrap());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link is not replaced"
        );
        assert_eq!(mode(&real), 0o644, "the target is not chmodded");
        // The mode that is warned about is the target's: the file that holds the data.
        assert!(
            emitted_for(tmp.path())
                .iter()
                .any(|w| w.contains(&link.display().to_string()) && w.contains("644"))
        );

        // A dangling link: the exclusive create refuses it, so nothing is created through it
        // by fl's own code, and the link stays a link.
        let dangling = tmp.path().join("dangling.redb");
        std::os::unix::fs::symlink(tmp.path().join("nowhere.redb"), &dangling).unwrap();
        assert!(!create_private_file(&dangling).unwrap());
        assert!(!tmp.path().join("nowhere.redb").exists());
    }
}
