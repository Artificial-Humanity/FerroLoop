//! Bounded reading, copying, verifying, sealing and removing of file trees. Nothing here
//! follows a symlink; a symlink, a device or a name outside `[A-Za-z0-9._+@-]` inside an
//! artifact is refused, because the manifest declares none.

use crate::error::{Ctx, InstallError, Result};
use crate::journal::sync_dir;
use fl_core::install::{hex, rel_path_problem, tree_digest};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The most files one artifact tree may hold. The receipt bounds its entries at 4096; the
/// directories of a tree and the files of earlier releases share that bound.
pub const MAX_TREE_FILES: usize = 2500;

/// One regular file found in an artifact tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    pub rel: String,
    pub sha256: String,
    pub size: u64,
}

/// A scanned artifact tree.
#[derive(Debug, Clone)]
pub struct Scanned {
    pub files: BTreeMap<String, ScannedFile>,
    pub total_bytes: u64,
    pub digest: String,
}

fn open_nofollow(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// Hash a regular file (opened without following a symlink, checked with `fstat`). `limit`
/// is the most bytes it may hold.
pub fn hash_file(path: &Path, limit: u64) -> Result<(String, u64)> {
    hash_file_as(path, limit, &path.display().to_string())
}

fn hash_file_as(path: &Path, limit: u64, shown: &str) -> Result<(String, u64)> {
    let f = open_nofollow(path).ctx(|| format!("cannot open {}", path.display()))?;
    hash_open(f, path, limit, shown)
}

/// Hash an already opened file (checked with `fstat` to be a regular file).
fn hash_open(f: File, path: &Path, limit: u64, shown: &str) -> Result<(String, u64)> {
    let meta = f
        .metadata()
        .ctx(|| format!("cannot stat {}", path.display()))?;
    if !meta.is_file() {
        return Err(InstallError::refused(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > limit {
        return Err(InstallError::refused(format!(
            "{shown} is larger than the {limit} bytes that remain of the declared size"
        )));
    }
    let mut h = Sha256::new();
    let mut n = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    let mut r = f.take(limit + 1);
    loop {
        let k = r
            .read(&mut buf)
            .ctx(|| format!("cannot read {}", path.display()))?;
        if k == 0 {
            break;
        }
        n += k as u64;
        h.update(&buf[..k]);
    }
    if n > limit || n != meta.len() {
        return Err(InstallError::refused(format!(
            "{} changed while it was read",
            path.display()
        )));
    }
    Ok((hex(&h.finalize()), n))
}

/// Walk `root` (an extracted artifact), hash every regular file and compute the tree digest.
/// Refuses a symlink, a special file, a bad name, too many files, and more than `max_bytes`.
pub fn scan_tree(root: &Path, max_bytes: u64) -> Result<Scanned> {
    let meta = fs::symlink_metadata(root)
        .ctx(|| format!("artifact {} cannot be inspected", root.display()))?;
    if !meta.is_dir() {
        return Err(InstallError::refused(format!(
            "artifact {} is not a directory (archives are not unpacked in this release; \
             pass an extracted tree)",
            root.display()
        )));
    }
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .ctx(|| format!("cannot read {}", dir.display()))?
            .collect::<std::io::Result<_>>()
            .ctx(|| format!("cannot read {}", dir.display()))?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            let Some(name) = name.to_str() else {
                return Err(InstallError::refused(format!(
                    "artifact path under {} is not UTF-8",
                    dir.display()
                )));
            };
            let child_rel = if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            };
            if let Some(why) = rel_path_problem(&child_rel) {
                return Err(InstallError::refused(format!(
                    "artifact path `{child_rel}` {why}"
                )));
            }
            let path = e.path();
            let m =
                fs::symlink_metadata(&path).ctx(|| format!("cannot inspect {}", path.display()))?;
            let ft = m.file_type();
            if ft.is_symlink() {
                return Err(InstallError::refused(format!(
                    "artifact path `{child_rel}` is a symlink; the manifest declares none"
                )));
            } else if ft.is_dir() {
                stack.push((path, child_rel));
            } else if ft.is_file() {
                if files.len() >= MAX_TREE_FILES {
                    return Err(InstallError::refused(format!(
                        "artifact holds more than {MAX_TREE_FILES} files"
                    )));
                }
                let (sha256, size) = hash_file_as(
                    &path,
                    max_bytes.saturating_sub(total),
                    &format!("artifact file `{child_rel}`"),
                )?;
                total += size;
                files.insert(
                    child_rel.clone(),
                    ScannedFile {
                        rel: child_rel,
                        sha256,
                        size,
                    },
                );
            } else {
                return Err(InstallError::refused(format!(
                    "artifact path `{child_rel}` is not a file or directory"
                )));
            }
        }
    }
    let digest = tree_digest(
        &files
            .values()
            .map(|f| (f.rel.clone(), f.sha256.clone(), f.size))
            .collect::<Vec<_>>(),
    );
    Ok(Scanned {
        files,
        total_bytes: total,
        digest,
    })
}

/// Copy `src` to `dest` (created exclusively, mode 0600), hashing the bytes as they are
/// copied; the hash must be `expect`. The copy is flushed to disk.
pub fn copy_hashed(src: &Path, dest: &Path, expect: &str, size: u64) -> Result<()> {
    let input = open_nofollow(src).ctx(|| format!("cannot open {}", src.display()))?;
    let meta = input
        .metadata()
        .ctx(|| format!("cannot stat {}", src.display()))?;
    if !meta.is_file() {
        return Err(InstallError::refused(format!(
            "{} is no longer a regular file",
            src.display()
        )));
    }
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dest)
        .ctx(|| format!("cannot create {}", dest.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    let mut r = input.take(size + 1);
    loop {
        let k = r
            .read(&mut buf)
            .ctx(|| format!("cannot read {}", src.display()))?;
        if k == 0 {
            break;
        }
        n += k as u64;
        h.update(&buf[..k]);
        out.write_all(&buf[..k])
            .ctx(|| format!("cannot write {}", dest.display()))?;
    }
    if n != size || hex(&h.finalize()) != expect {
        return Err(InstallError::refused(format!(
            "{} changed after it was verified; nothing was activated",
            src.display()
        )));
    }
    out.sync_all()
        .ctx(|| format!("cannot sync {}", dest.display()))
}

/// Write `bytes` to a new file at `dest` (exclusive, mode 0600) and flush it.
pub fn write_new(dest: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dest)
        .ctx(|| format!("cannot create {}", dest.display()))?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .ctx(|| format!("cannot write {}", dest.display()))
}

/// The files of a staged tree, hashed from disk: relative path to (hash, size). Refuses a
/// symlink or anything that is not a file or directory.
pub fn read_staged(root: &Path) -> Result<BTreeMap<String, (String, u64)>> {
    let mut out = BTreeMap::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for e in fs::read_dir(&dir).ctx(|| format!("cannot read {}", dir.display()))? {
            let e = e.ctx(|| format!("cannot read {}", dir.display()))?;
            let name = e.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let path = e.path();
            let m =
                fs::symlink_metadata(&path).ctx(|| format!("cannot inspect {}", path.display()))?;
            if m.is_dir() {
                stack.push((path, child));
            } else if m.is_file() {
                let (sha, size) = hash_file(&path, u64::MAX / 2)?;
                out.insert(child, (sha, size));
            } else {
                return Err(InstallError::refused(format!(
                    "staged path `{child}` is a symlink or special file"
                )));
            }
        }
    }
    Ok(out)
}

/// What a sealed release holds: each file's (hash, size, mode bits) and every subdirectory
/// with its mode bits. Refuses a symlink or special file.
pub type Shape = (BTreeMap<String, (String, u64, u32)>, BTreeMap<String, u32>);

pub fn read_shape(root: &Path) -> Result<Shape> {
    let mut files = BTreeMap::new();
    let mut dirs = BTreeMap::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for e in fs::read_dir(&dir).ctx(|| format!("cannot read {}", dir.display()))? {
            let e = e.ctx(|| format!("cannot read {}", dir.display()))?;
            let name = e.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let path = e.path();
            let m =
                fs::symlink_metadata(&path).ctx(|| format!("cannot inspect {}", path.display()))?;
            if m.is_dir() {
                dirs.insert(child.clone(), m.mode() & 0o7777);
                stack.push((path, child));
            } else if m.is_file() {
                let (sha, size) = hash_file(&path, u64::MAX / 2)?;
                files.insert(child, (sha, size, m.mode() & 0o7777));
            } else {
                return Err(InstallError::refused(format!(
                    "path `{child}` is a symlink or special file"
                )));
            }
        }
    }
    Ok((files, dirs))
}

/// Make the staged tree read-only (files `0555` for binaries, else `0444`; directories
/// `0555`; children before parents) and return what the SEALED files hold: relative path to
/// (hash, size). Every file and directory is opened without following a symlink and changed
/// with `fchmod` on that descriptor, and a file is hashed through the same descriptor, so
/// nothing swapped in at a path can be chmodded or hashed in place of a staged file.
pub fn seal_and_read(
    root: &Path,
    exec: &dyn Fn(&str) -> bool,
) -> Result<BTreeMap<String, (String, u64)>> {
    fn walk(
        dir: &Path,
        rel: &str,
        exec: &dyn Fn(&str) -> bool,
        out: &mut BTreeMap<String, (String, u64)>,
    ) -> Result<()> {
        for e in fs::read_dir(dir).ctx(|| format!("cannot read {}", dir.display()))? {
            let e = e.ctx(|| format!("cannot read {}", dir.display()))?;
            let name = e.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let path = e.path();
            let m =
                fs::symlink_metadata(&path).ctx(|| format!("cannot inspect {}", path.display()))?;
            if m.is_dir() {
                walk(&path, &child, exec, out)?;
            } else if m.is_file() {
                let f = open_nofollow(&path).ctx(|| format!("cannot open {}", path.display()))?;
                let mode = if exec(&child) { 0o555 } else { 0o444 };
                f.set_permissions(fs::Permissions::from_mode(mode))
                    .ctx(|| format!("cannot seal {}", path.display()))?;
                let (sha, size) = hash_open(f, &path, u64::MAX / 2, &child)?;
                out.insert(child, (sha, size));
            } else {
                return Err(InstallError::refused(format!(
                    "staged path `{child}` is a symlink or special file"
                )));
            }
        }
        let d = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(dir)
            .ctx(|| format!("cannot open {}", dir.display()))?;
        d.set_permissions(fs::Permissions::from_mode(0o555))
            .ctx(|| format!("cannot seal {}", dir.display()))
    }
    let mut out = BTreeMap::new();
    walk(root, "", exec, &mut out)?;
    Ok(out)
}

/// Make every directory under `root` writable again, without following a symlink, so the
/// tree can be removed (a sealed release is read-only). Files need no change to be unlinked.
pub fn make_tree_writable(root: &Path) {
    fn walk(dir: &Path) {
        let Ok(m) = fs::symlink_metadata(dir) else {
            return;
        };
        if !m.is_dir() {
            return;
        }
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                walk(&e.path());
            }
        }
    }
    walk(root);
}

/// Remove a tree the installer created (and no other), sealed or not.
pub fn remove_tree(root: &Path) -> Result<()> {
    match fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() => {
            make_tree_writable(root);
            fs::remove_dir_all(root).ctx(|| format!("cannot remove {}", root.display()))?;
        }
        Ok(_) => fs::remove_file(root).ctx(|| format!("cannot remove {}", root.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(InstallError::io(
                format!("cannot inspect {}", root.display()),
                e,
            ));
        }
    }
    if let Some(p) = root.parent() {
        sync_dir(p)?;
    }
    Ok(())
}

/// The mode bits of `path` (without following a symlink).
pub fn mode_of(path: &Path) -> Option<u32> {
    fs::symlink_metadata(path).ok().map(|m| m.mode() & 0o7777)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("bin")).unwrap();
        fs::create_dir_all(d.path().join("ui/dist/assets")).unwrap();
        fs::write(d.path().join("bin/fl"), "binary").unwrap();
        fs::write(d.path().join("ui/dist/index.html"), "<html>").unwrap();
        fs::write(d.path().join("ui/dist/assets/a.js"), "js").unwrap();
        d
    }

    #[test]
    fn a_scan_hashes_every_file_and_the_digest_is_stable() {
        let d = tree();
        let a = scan_tree(d.path(), 1 << 20).unwrap();
        assert_eq!(a.files.len(), 3);
        assert_eq!(a.total_bytes, 6 + 6 + 2);
        let b = scan_tree(d.path(), 1 << 20).unwrap();
        assert_eq!(a.digest, b.digest);
        fs::write(d.path().join("ui/dist/assets/a.js"), "jS").unwrap();
        assert_ne!(scan_tree(d.path(), 1 << 20).unwrap().digest, a.digest);
    }

    #[test]
    fn a_symlink_a_bad_name_a_fifo_and_too_many_bytes_are_refused() {
        let d = tree();
        symlink("/etc/passwd", d.path().join("bin/evil")).unwrap();
        let e = scan_tree(d.path(), 1 << 20).unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
        fs::remove_file(d.path().join("bin/evil")).unwrap();

        fs::write(d.path().join("bin/has space"), "x").unwrap();
        assert!(scan_tree(d.path(), 1 << 20).is_err());
        fs::remove_file(d.path().join("bin/has space")).unwrap();

        assert!(
            std::process::Command::new("mkfifo")
                .arg(d.path().join("bin/pipe"))
                .status()
                .unwrap()
                .success()
        );
        assert!(scan_tree(d.path(), 1 << 20).is_err());
        fs::remove_file(d.path().join("bin/pipe")).unwrap();

        let e = scan_tree(d.path(), 5).unwrap_err();
        assert!(e.to_string().contains("larger"), "{e}");
        // a root that is a file or a symlink to a directory
        let f = d.path().join("bin/fl");
        assert!(scan_tree(&f, 100).is_err());
        let l = d.path().join("lnk");
        symlink(d.path().join("bin"), &l).unwrap();
        assert!(scan_tree(&l, 100).is_err());
    }

    #[test]
    fn a_file_that_changes_between_hash_and_copy_is_caught() {
        let d = tree();
        let a = scan_tree(d.path(), 1 << 20).unwrap();
        let f = &a.files["bin/fl"];
        let out = tempfile::tempdir().unwrap();
        copy_hashed(
            &d.path().join("bin/fl"),
            &out.path().join("fl"),
            &f.sha256,
            f.size,
        )
        .unwrap();
        fs::write(d.path().join("bin/fl"), "tampered").unwrap();
        let err = copy_hashed(
            &d.path().join("bin/fl"),
            &out.path().join("fl2"),
            &f.sha256,
            f.size,
        )
        .unwrap_err();
        assert!(err.to_string().contains("changed after"), "{err}");
    }

    #[test]
    fn sealing_makes_a_tree_read_only_and_removal_still_works() {
        let d = tree();
        let root = d.path().join("ui");
        let sealed = seal_and_read(&root, &|rel| rel == "dist/index.html").unwrap();
        assert_eq!(sealed.len(), 2);
        assert_eq!(mode_of(&root.join("dist/index.html")).unwrap(), 0o555);
        assert_eq!(mode_of(&root).unwrap(), 0o555);
        assert_eq!(mode_of(&root.join("dist/assets/a.js")).unwrap(), 0o444);
        assert!(fs::write(root.join("dist/new"), "x").is_err());
        remove_tree(&root).unwrap();
        assert!(!root.exists());
        // remove_tree of a missing path is fine, of a symlink removes the link only
        remove_tree(&root).unwrap();
        let t = tempfile::tempdir().unwrap();
        let keep = t.path().join("keep");
        fs::create_dir(&keep).unwrap();
        symlink(&keep, t.path().join("l")).unwrap();
        remove_tree(&t.path().join("l")).unwrap();
        assert!(keep.exists());
    }
}
