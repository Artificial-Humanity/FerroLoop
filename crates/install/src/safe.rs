//! Where the installer may write. Every path the transaction touches is resolved through
//! [`Safe`]: it must lie inside the declared root, the root's existing ancestors are
//! canonicalised and may not lead into a system tree, and no component below the root may be a
//! symlink the installer did not create (it creates none but `current`, the launcher and the
//! aliases, which are never directory components).

use crate::error::{Ctx, InstallError, Result};
use fl_core::install::is_forbidden_install_root;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Safe {
    home: Option<PathBuf>,
    /// The declared root as the caller spelled it (normalised).
    lexical: PathBuf,
    /// Its canonical spelling: every symlink of its existing ancestors resolved.
    canonical: PathBuf,
}

fn normalised(p: &Path, what: &str) -> Result<PathBuf> {
    if !p.is_absolute() {
        return Err(InstallError::Usage(format!(
            "{what} `{}` is not absolute",
            p.display()
        )));
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::Normal(s) => out.push(s),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(InstallError::Usage(format!(
                    "{what} `{}` has a `.` or `..` segment",
                    p.display()
                )));
            }
        }
    }
    Ok(out)
}

/// The canonical spelling of `p`: the longest existing ancestor is canonicalised, the rest
/// (which does not exist yet) is appended as written.
fn canonicalise_existing_prefix(p: &Path) -> Result<PathBuf> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        match fs::symlink_metadata(&cur) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = cur.file_name().map(|n| n.to_os_string()) else {
                    break;
                };
                missing.push(name);
                if !cur.pop() {
                    break;
                }
            }
            Err(e) => {
                return Err(InstallError::io(
                    format!("cannot inspect {}", cur.display()),
                    e,
                ));
            }
        }
    }
    let mut canon = fs::canonicalize(&cur).ctx(|| format!("cannot resolve {}", cur.display()))?;
    for name in missing.iter().rev() {
        canon.push(name);
    }
    Ok(canon)
}

impl Safe {
    /// `declared_root` is where this install may write (`--prefix`, `$FL_INSTALL_ROOT`, or
    /// `$HOME`). `home` is what a leading `~/` expands to.
    pub fn new(declared_root: &Path, home: Option<&Path>) -> Result<Self> {
        let lexical = normalised(declared_root, "install root")?;
        let home = home.map(|h| normalised(h, "HOME")).transpose()?;
        let canonical = canonicalise_existing_prefix(&lexical)?;
        let canon_str = canonical.to_string_lossy();
        if is_forbidden_install_root(&canon_str) {
            return Err(InstallError::refused(format!(
                "`{}` resolves to `{canon_str}`, a system location: installs are per-user, \
                 never system-wide",
                lexical.display()
            )));
        }
        // The root itself may not be a symlink (only HOME may: many homes are links).
        if let Ok(m) = fs::symlink_metadata(&lexical)
            && m.file_type().is_symlink()
            && home.as_deref() != Some(lexical.as_path())
        {
            return Err(InstallError::refused(format!(
                "the install root `{}` is a symlink the installer did not create; pass the real \
                 directory",
                lexical.display()
            )));
        }
        Ok(Self {
            home,
            lexical,
            canonical,
        })
    }

    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical
    }

    /// A shown path (`~/x` or absolute) spelled as a lexical path, nothing resolved.
    pub fn lexical(&self, shown: &str) -> Result<PathBuf> {
        let p = match shown.strip_prefix("~/") {
            Some(rest) => self
                .home
                .as_ref()
                .ok_or_else(|| InstallError::refused("HOME is not set; pass --prefix"))?
                .join(rest),
            None => PathBuf::from(shown),
        };
        normalised(&p, "path")
    }

    /// The real path of a shown one, inside the canonical root. A path outside the declared
    /// root is refused.
    pub fn resolve(&self, shown: &str) -> Result<PathBuf> {
        let lex = self.lexical(shown)?;
        let rest = lex.strip_prefix(&self.lexical).map_err(|_| {
            InstallError::refused(format!(
                "`{shown}` is outside the declared install root `{}`",
                self.lexical.display()
            ))
        })?;
        Ok(self.canonical.join(rest))
    }

    /// Refuse a symlink or a non-directory among the components of `real` that lie below the
    /// root and exist. The last component is not looked at.
    pub fn check_chain(&self, real: &Path) -> Result<()> {
        let rest = real.strip_prefix(&self.canonical).map_err(|_| {
            InstallError::refused(format!("`{}` is outside the install root", real.display()))
        })?;
        let comps: Vec<_> = rest.components().collect();
        let mut cur = self.canonical.clone();
        for (i, c) in comps.iter().enumerate() {
            cur.push(c);
            if i + 1 == comps.len() {
                break;
            }
            match fs::symlink_metadata(&cur) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(InstallError::refused(format!(
                        "`{}` is a symlink the installer did not create; it does not follow it \
                         (pass the real directory as --prefix)",
                        cur.display()
                    )));
                }
                Ok(m) if !m.is_dir() => {
                    return Err(InstallError::refused(format!(
                        "`{}` exists and is not a directory",
                        cur.display()
                    )));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => {
                    return Err(InstallError::io(
                        format!("cannot inspect {}", cur.display()),
                        e,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Create `dir` and every missing ancestor below the root, refusing a symlink in the
    /// chain. Returns the directories it created, outermost first. A directory that already
    /// exists is left alone.
    pub fn ensure_dir(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        self.check_chain(&dir.join("x"))?; // checks every component of `dir` itself
        if !dir.starts_with(&self.canonical) {
            return Err(InstallError::refused(format!(
                "`{}` is outside the install root",
                dir.display()
            )));
        }
        let mut created = Vec::new();
        // From `/`: the part of the canonical root that exists has no symlink, and the part
        // that does not exist (a fresh `--prefix`) is created.
        let mut cur = PathBuf::new();
        for c in dir.components() {
            cur.push(c);
            if cur == Path::new("/") {
                continue;
            }
            match fs::symlink_metadata(&cur) {
                Ok(m) if m.is_dir() => continue,
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(InstallError::refused(format!(
                        "`{}` is a symlink the installer did not create",
                        cur.display()
                    )));
                }
                Ok(_) => {
                    return Err(InstallError::refused(format!(
                        "`{}` exists and is not a directory",
                        cur.display()
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    match fs::create_dir(&cur) {
                        Ok(()) => created.push(cur.clone()),
                        // Raced with another installer: it is a directory now or it is not.
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            if !fs::symlink_metadata(&cur).is_ok_and(|m| m.is_dir()) {
                                return Err(InstallError::refused(format!(
                                    "`{}` appeared and is not a directory",
                                    cur.display()
                                )));
                            }
                        }
                        Err(e) => {
                            return Err(InstallError::io(
                                format!("cannot create {}", cur.display()),
                                e,
                            ));
                        }
                    }
                }
                Err(e) => {
                    return Err(InstallError::io(
                        format!("cannot inspect {}", cur.display()),
                        e,
                    ));
                }
            }
        }
        Ok(created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_path_outside_the_declared_root_is_refused() {
        let t = tmp();
        let s = Safe::new(t.path(), None).unwrap();
        assert!(s.resolve(&format!("{}/a/b", t.path().display())).is_ok());
        for bad in ["/etc/passwd", "/tmp", "relative/x"] {
            assert!(s.resolve(bad).is_err(), "{bad}");
        }
        assert!(s.resolve("~/x").is_err(), "no HOME");
    }

    #[test]
    fn a_root_that_resolves_into_a_system_tree_is_refused() {
        let t = tmp();
        let link = t.path().join("sneaky");
        symlink("/etc", &link).unwrap();
        // The root itself is a symlink: refused as such...
        assert!(Safe::new(&link, None).is_err());
        // ...and a root UNDER a link into /etc resolves into the system tree.
        let err = Safe::new(&link.join("fl"), None).unwrap_err();
        assert!(err.to_string().contains("system location"), "{err}");
        assert!(Safe::new(Path::new("/"), None).is_err());
        assert!(Safe::new(Path::new("/usr/local"), None).is_err());
    }

    #[test]
    fn a_symlinked_component_below_the_root_is_refused_and_nothing_is_created() {
        let t = tmp();
        let elsewhere = tmp();
        let root = t.path().join("root");
        fs::create_dir(&root).unwrap();
        symlink(elsewhere.path(), root.join(".local")).unwrap();
        let s = Safe::new(&root, None).unwrap();
        let target = s
            .resolve(&format!("{}/.local/share/fl", root.display()))
            .unwrap();
        let err = s.ensure_dir(&target).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[test]
    fn ensure_dir_reports_exactly_what_it_created() {
        let t = tmp();
        let root = t.path().join("root");
        fs::create_dir(&root).unwrap();
        let s = Safe::new(&root, None).unwrap();
        let d = s.resolve(&format!("{}/a/b/c", root.display())).unwrap();
        fs::create_dir(root.join("a")).unwrap();
        let made = s.ensure_dir(&d).unwrap();
        assert_eq!(made.len(), 2);
        assert!(made[0].ends_with("a/b") && made[1].ends_with("a/b/c"));
        assert!(s.ensure_dir(&d).unwrap().is_empty());
    }

    #[test]
    fn a_symlinked_ancestor_above_the_root_is_followed_but_checked() {
        let real = tmp();
        let link_parent = tmp();
        let link = link_parent.path().join("l");
        symlink(real.path(), &link).unwrap();
        let s = Safe::new(&link.join("root"), None).unwrap();
        assert!(
            s.canonical_root()
                .starts_with(fs::canonicalize(real.path()).unwrap())
        );
    }

    #[test]
    fn dot_segments_are_a_usage_error() {
        assert!(matches!(
            Safe::new(Path::new("/tmp/x/../y"), None),
            Err(InstallError::Usage(_))
        ));
    }
}
