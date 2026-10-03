//! The user-level binding of projects to stores (spec §2.6).
//!
//! User-level, not in the repository: a store path belongs to a machine, and
//! a public repository would publish it.
//!
//! ⚠ A config file that exists but cannot be read is an ERROR, never a
//! silent fall-through to the default store. Falling through would put a
//! project's records in a store nobody chose.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub root: PathBuf,
    pub store: PathBuf,
    /// `None`: the tracker is the local store.
    #[serde(default)]
    pub tracker: Option<TrackerBinding>,
}

/// A project's tracker when it is not the local store (GitHub tracker spec
/// §1.4): `tracker = { github = "owner/repo", credential = "env" }`, and,
/// for mode B, `ledger = "github"` (GitHub ledger spec §1.5).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerBinding {
    pub github: String,
    pub credential: Credential,
    /// `Some(Github)`: decisions publish their evidence to the repository's
    /// `fl/ledger` branch. `None`: the ledger is the local store.
    ///
    /// ⚠ Part of the binding's identity: two entries for one store that
    /// differ here are two trackers, and refused.
    #[serde(default)]
    pub ledger: Option<LedgerChoice>,
}

impl TrackerBinding {
    /// Whether this binding names the GitHub ledger.
    pub fn github_ledger(&self) -> bool {
        self.ledger == Some(LedgerChoice::Github)
    }
}

/// The ledger a tracker binding names. One value: the GitHub ledger, which
/// always lives in the tracker's repository (spec §1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum LedgerChoice {
    Github,
}

impl TryFrom<String> for LedgerChoice {
    type Error = String;

    /// ⚠ Exactly `github`: any other value is refused by name, never read
    /// as the local ledger — a person who wrote it meant something.
    fn try_from(value: String) -> Result<Self, String> {
        match value.as_str() {
            "github" => Ok(LedgerChoice::Github),
            other => Err(format!(
                "`ledger = \"{other}\"` is not a ledger fl knows. Its only value is \"github\", \
                 the GitHub ledger in the tracker's repository; leave `ledger` out to keep every \
                 run and decision in the local store"
            )),
        }
    }
}

/// Where the GitHub credential comes from. One source, and no fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    App,
    Env,
}

/// `[github]`: the App fl writes as when a binding says `credential = "app"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubApp {
    pub app_id: u64,
    pub private_key: PathBuf,
}

#[derive(Debug, Default)]
pub struct Config {
    pub projects: Vec<Entry>,
    pub github: Option<GithubApp>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    project: Vec<Entry>,
    #[serde(default)]
    github: Option<GithubApp>,
}

/// The XDG config base directory: `xdg_config_home` if it is a non-empty,
/// ABSOLUTE path, else `home/.config`. Per the XDG base directory spec, a
/// relative `$XDG_CONFIG_HOME` (including empty, which is not absolute)
/// must be treated as unset rather than used as-is.
/// Pure and dependency-free so it can be unit-tested without touching this
/// process's own environment.
fn base_dir(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_base(xdg_config_home, home, ".config")
}

/// The XDG data base directory, by the same rule as [`base_dir`]:
/// `xdg_data_home` if it is an ABSOLUTE path, else `home/.local/share`. An
/// empty or relative `$XDG_DATA_HOME` is treated as unset — used as-is it
/// would put the store under the current directory.
pub fn data_dir(xdg_data_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_base(xdg_data_home, home, ".local/share")
}

/// The one rule both XDG bases follow: the variable if it is absolute, else
/// `home` joined with the spec's default for that base.
fn xdg_base(var: Option<PathBuf>, home: Option<PathBuf>, default: &str) -> Option<PathBuf> {
    var.filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(default)))
}

pub fn path() -> Option<PathBuf> {
    let base = base_dir(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )?;
    Some(base.join("fl").join("config.toml"))
}

pub fn load(path: Option<&Path>) -> Result<Config> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    let file: File = toml::from_str(&text)
        .with_context(|| format!("{} is not a valid fl config", path.display()))?;
    for e in &file.project {
        if !e.root.is_absolute() || !e.store.is_absolute() {
            bail!(
                "{}: `root` and `store` must be absolute paths (got root `{}`, store `{}`)",
                path.display(),
                e.root.display(),
                e.store.display()
            );
        }
        if let Some(t) = &e.tracker
            && !is_owner_repo(&t.github)
        {
            bail!(
                "{}: `github = \"{}\"` must name a repository as `owner/repo`",
                path.display(),
                t.github
            );
        }
    }
    if let Some(app) = &file.github
        && !app.private_key.is_absolute()
    {
        bail!(
            "{}: `private_key` must be an absolute path (got `{}`)",
            path.display(),
            app.private_key.display()
        );
    }
    Ok(Config {
        projects: file.project,
        github: file.github,
    })
}

/// Whether `s` names a repository as `owner/repo`: exactly two non-empty
/// parts and no whitespace. The config's `github = …` and a typed
/// `owner/repo#41` follow the same rule.
pub fn is_owner_repo(s: &str) -> bool {
    s.split('/').count() == 2
        && s.split('/').all(|p| !p.is_empty())
        && !s.contains(char::is_whitespace)
}

/// Canonicalize `path`. A path that does not exist is not an error here —
/// `Ok(None)` — that is simply a project root (or a `cwd`) not yet created.
/// Any OTHER failure, such as a permission error partway down the tree, IS
/// an error naming the path: silently treating it the same as "does not
/// exist" would fall through to the default store, putting a project's
/// records in a store nobody chose.
fn canonicalize(path: &Path) -> Result<Option<PathBuf>> {
    match path.canonicalize() {
        Ok(p) => Ok(Some(p)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("could not resolve {}", path.display())),
    }
}

/// The config entry for the project containing `cwd`: the entry whose root
/// is the longest ancestor of `cwd`. Both sides are canonicalized, so a
/// symlinked path binds like the real one.
///
/// §2.6 binds a project to exactly one store, and a project has one
/// tracker. Two (or more) entries whose canonical `root` is identical — the
/// longest match is therefore tied — but whose `(store, tracker)` differs
/// are refused rather than silently picking one: nothing chose between them.
pub fn bound_entry(entries: &[Entry], cwd: &Path) -> Result<Option<Entry>> {
    let Some(cwd) = canonicalize(cwd)? else {
        return Ok(None);
    };
    let mut matches: Vec<(PathBuf, &Entry)> = Vec::new();
    for e in entries {
        let Some(root) = canonicalize(&e.root)? else {
            continue;
        };
        if cwd.starts_with(&root) {
            matches.push((root, e));
        }
    }
    let Some(longest) = matches.iter().map(|(r, _)| r.components().count()).max() else {
        return Ok(None);
    };
    let winners: Vec<&(PathBuf, &Entry)> = matches
        .iter()
        .filter(|(r, _)| r.components().count() == longest)
        .collect();
    let mut distinct: Vec<(&PathBuf, &Option<TrackerBinding>)> = Vec::new();
    for (_, e) in &winners {
        if !distinct.contains(&(&e.store, &e.tracker)) {
            distinct.push((&e.store, &e.tracker));
        }
    }
    if distinct.len() > 1 {
        let names = winners
            .iter()
            .map(|(_, e)| {
                let tracker = match &e.tracker {
                    Some(t) if t.github_ledger() => format!("github:{} (ledger github)", t.github),
                    Some(t) => format!("github:{}", t.github),
                    None => "the store's own tracker".to_string(),
                };
                format!("{} -> {} -> {tracker}", e.root.display(), e.store.display())
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "the project at {} is bound to more than one store or tracker in the config: \
             {names}. Remove all but one of these entries",
            cwd.display()
        );
    }
    Ok(Some(winners[0].1.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `load` over a config file holding `text`.
    fn load_text(text: &str) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        load(Some(&path))
    }

    fn entry_with(github: &str) -> String {
        format!(
            "[[project]]\nroot = \"/r\"\nstore = \"/s.redb\"\n\
             tracker = {{ github = \"{github}\", credential = \"env\" }}\n"
        )
    }

    #[test]
    fn a_tracker_must_name_a_repository_as_owner_slash_repo() {
        for bad in [
            "acme",
            "acme/",
            "/widgets",
            "acme/widgets/x",
            "ac me/widgets",
            "",
        ] {
            let err = load_text(&entry_with(bad)).expect_err(bad);
            assert!(
                format!("{err:#}").contains("must name a repository as `owner/repo`"),
                "{bad}: {err:#}"
            );
        }
        let cfg = load_text(&entry_with("acme/widgets")).unwrap();
        assert_eq!(
            cfg.projects[0].tracker,
            Some(TrackerBinding {
                github: "acme/widgets".into(),
                credential: Credential::Env,
                ledger: None,
            })
        );
    }

    #[test]
    fn a_relative_private_key_is_refused() {
        let err = load_text("[github]\napp_id = 1\nprivate_key = \"key.pem\"\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("`private_key` must be an absolute path"),
            "{err:#}"
        );
        let cfg = load_text("[github]\napp_id = 1\nprivate_key = \"/k/key.pem\"\n").unwrap();
        assert_eq!(cfg.github.unwrap().app_id, 1);
    }

    #[test]
    fn two_entries_differing_only_by_tracker_are_refused_naming_each_tracker() {
        let root = tempfile::tempdir().unwrap();
        let store = PathBuf::from("/tmp/fl-config-test-same.redb");
        let entries = vec![
            Entry {
                root: root.path().to_path_buf(),
                store: store.clone(),
                tracker: None,
            },
            Entry {
                root: root.path().to_path_buf(),
                store,
                tracker: Some(TrackerBinding {
                    github: "acme/widgets".into(),
                    credential: Credential::Env,
                    ledger: None,
                }),
            },
        ];
        let msg = format!("{:#}", bound_entry(&entries, root.path()).unwrap_err());
        assert!(msg.contains("-> github:acme/widgets"), "{msg}");
        assert!(msg.contains("-> the store's own tracker"), "{msg}");
        assert!(msg.contains("Remove all but one of these entries"), "{msg}");
    }

    #[test]
    fn identical_entries_are_not_a_conflict() {
        let root = tempfile::tempdir().unwrap();
        let e = Entry {
            root: root.path().to_path_buf(),
            store: PathBuf::from("/tmp/fl-config-test-same.redb"),
            tracker: Some(TrackerBinding {
                github: "acme/widgets".into(),
                credential: Credential::Env,
                ledger: None,
            }),
        };
        let got = bound_entry(&[e.clone(), e], root.path()).unwrap().unwrap();
        assert_eq!(got.tracker.unwrap().github, "acme/widgets");
    }

    #[test]
    fn an_empty_or_relative_xdg_config_home_falls_back_to_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            base_dir(Some(PathBuf::from("")), home.clone()),
            Some(PathBuf::from("/home/u/.config")),
            "an empty XDG_CONFIG_HOME must be treated as unset"
        );
        assert_eq!(
            base_dir(Some(PathBuf::from("relative/config")), home.clone()),
            Some(PathBuf::from("/home/u/.config")),
            "a relative XDG_CONFIG_HOME must be treated as unset"
        );
        assert_eq!(
            base_dir(Some(PathBuf::from("/abs/config")), home),
            Some(PathBuf::from("/abs/config")),
            "an absolute XDG_CONFIG_HOME must still win"
        );
    }

    #[test]
    fn an_empty_or_relative_xdg_data_home_falls_back_to_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            data_dir(Some(PathBuf::from("")), home.clone()),
            Some(PathBuf::from("/home/u/.local/share")),
            "an empty XDG_DATA_HOME must be treated as unset"
        );
        assert_eq!(
            data_dir(Some(PathBuf::from("relative/data")), home.clone()),
            Some(PathBuf::from("/home/u/.local/share")),
            "a relative XDG_DATA_HOME must be treated as unset"
        );
        assert_eq!(
            data_dir(Some(PathBuf::from("/abs/data")), home.clone()),
            Some(PathBuf::from("/abs/data")),
            "an absolute XDG_DATA_HOME must still win"
        );
        assert_eq!(
            data_dir(None, home),
            Some(PathBuf::from("/home/u/.local/share"))
        );
        assert_eq!(data_dir(Some(PathBuf::from("rel")), None), None);
    }

    #[cfg(unix)]
    #[test]
    fn bound_resolves_a_symlinked_cwd_passed_directly() {
        // Task 6's own "symlinked working directory" CLI test drives `fl`
        // as a subprocess; `std::env::current_dir()` (`getcwd(3)`) already
        // resolves a symlinked process cwd before `bound` ever sees it on
        // Linux, so that test cannot tell `bound`'s own `cwd.canonicalize()`
        // apart from a naive string comparison. Calling `bound` directly
        // here, with a raw symlink path nothing else has resolved first,
        // closes that gap.
        let real = tempfile::tempdir().unwrap();
        let stores = tempfile::tempdir().unwrap();
        let store = stores.path().join("a.redb");
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("via-link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        let entries = vec![Entry {
            root: real.path().to_path_buf(),
            store: store.clone(),
            tracker: None,
        }];
        let got = bound_entry(&entries, &link).unwrap().map(|e| e.store);
        assert_eq!(got, Some(store));
    }

    #[test]
    fn two_entries_with_the_same_root_but_different_stores_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let entries = vec![
            Entry {
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-one.redb"),
                tracker: None,
            },
            Entry {
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-two.redb"),
                tracker: None,
            },
        ];
        let err = bound_entry(&entries, root.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fl-config-test-one.redb") && msg.contains("fl-config-test-two.redb"),
            "the refusal must name both entries: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_permission_error_while_canonicalizing_is_an_error_not_a_silent_skip() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let blocked = parent.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        let inner = blocked.join("root");
        std::fs::create_dir(&inner).unwrap();
        // No execute permission on `blocked`: the kernel refuses to resolve
        // anything under it — for anyone but root.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Ask the standard library directly, first, to find out whether
        // this process (root, or a filesystem that does not enforce the
        // bit) can even be made to see the failure this test exists to
        // pin — a soft `Err(_) => {}` / `Ok(_) => eprintln!(...)` on OUR
        // wrapper's result would never fail no matter what `canonicalize`
        // does with the error, so it could not catch a regression either.
        let raw = inner.canonicalize();
        let result = canonicalize(&inner);

        // Restore before the TempDir's own cleanup has to walk it again.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

        match raw {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                assert!(
                    result.is_err(),
                    "a permission failure must be reported as an error, not treated the \
                     same as \"does not exist\""
                );
            }
            _ => eprintln!(
                "skipping the assertion: this process could still resolve a 0o000 \
                 directory, so it is likely running as root"
            ),
        }
    }

    // Spec §1.5: `ledger` is optional, and its only value is "github"; any
    // other is a config error naming it.
    #[test]
    fn the_only_ledger_a_binding_names_is_github() {
        let with = |ledger: &str| {
            format!(
                "[[project]]\nroot = \"/r\"\nstore = \"/s.redb\"\n\
                 tracker = {{ github = \"acme/widgets\", credential = \"env\"{ledger} }}\n"
            )
        };
        let binding = |text: &str| {
            load_text(text).unwrap().projects[0]
                .tracker
                .clone()
                .unwrap()
        };
        assert!(!binding(&with("")).github_ledger());
        assert!(binding(&with(", ledger = \"github\"")).github_ledger());
        for bad in ["local", "GitHub", ""] {
            let err = load_text(&with(&format!(", ledger = \"{bad}\""))).expect_err(bad);
            assert!(
                format!("{err:#}")
                    .contains(&format!("`ledger = \"{bad}\"` is not a ledger fl knows")),
                "{bad}: {err:#}"
            );
        }
    }

    // `ledger = true` is not the string "github": refused when the config
    // is read, never taken as the local ledger.
    #[test]
    fn a_ledger_key_that_is_not_a_string_is_refused() {
        let text = "[[project]]\nroot = \"/r\"\nstore = \"/s.redb\"\n\
                    tracker = { github = \"acme/widgets\", credential = \"env\", ledger = true }\n";
        let msg = format!(
            "{:#}",
            load_text(text).expect_err("`ledger = true` is refused")
        );
        assert!(msg.contains("invalid type: boolean `true`"), "{msg}");
        assert!(msg.contains("expected a string"), "{msg}");
    }

    // Two entries on one root and one store that differ only in `ledger`
    // are two trackers: refused, naming which has the GitHub ledger.
    #[test]
    fn two_entries_differing_only_by_ledger_are_refused_naming_which_has_it() {
        let root = tempfile::tempdir().unwrap();
        let binding = |ledger| TrackerBinding {
            github: "acme/widgets".into(),
            credential: Credential::Env,
            ledger,
        };
        let entries = vec![
            Entry {
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-same.redb"),
                tracker: Some(binding(None)),
            },
            Entry {
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-same.redb"),
                tracker: Some(binding(Some(LedgerChoice::Github))),
            },
        ];
        let msg = format!("{:#}", bound_entry(&entries, root.path()).unwrap_err());
        assert!(
            msg.contains("-> github:acme/widgets (ledger github)"),
            "{msg}"
        );
        assert!(msg.contains("-> github:acme/widgets,"), "{msg}");
    }
}
