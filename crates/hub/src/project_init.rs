//! First-class project creation: `fl project create` (alias `init`) and `POST /api/v1/projects`
//! run these flows.
//!
//! There are two kinds of project.
//!
//! - A directory-backed project (`kind` "project") is a directory, whether or not it is a Git
//!   repository. Creating one registers it: a `[[project]]` block in the user's config, a
//!   per-project store holding the project and the default local routing map, its area in the
//!   workspace store, and a starter `.fl/mcp.toml`. Git is touched only on request (`git_init`):
//!   `git init` when the directory is not a repository, and an empty initial commit when the
//!   repository has no `HEAD`.
//! - A virtual group (`kind` "virtual") is an area with no path in the workspace store: a name
//!   for a group of agents, addressable as `@<slug>` on FerroWire. It has no directory, no
//!   `[[project]]` block and no store of its own; creating one writes the workspace area and
//!   nothing else.
//!
//! Running a flow again changes nothing that is already right. A slug names one kind: a
//! directory-backed project cannot take the slug of a virtual group, nor the reverse.
//!
//! The flows live here, below both the CLI and the Hub, because both need the whole of them: a
//! second copy would drift. The caller supplies the places that differ between a CLI process
//! and the Hub (the config file, the directory of per-project stores, the workspace store) and
//! functions that open the stores. [`crate::project_list`] lists and updates what these flows
//! make.
//!
//! Nothing here reads `$HOME`, `$XDG_*` or any other environment variable: every path is an
//! argument, so a test never reaches a real config or data directory.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
#[cfg(test)]
use std::sync::Mutex;

use crate::project_config_lock;
pub use crate::project_config_lock::ProjectConfigGuard;
use crate::project_hierarchy::{display_path, matches_display_path, slug_from_project_name};
use crate::project_list::{StoreHint, is_project_slug};
use fl_core::agent::AreaRecord;
use fl_core::routing::{Routes, RoutingMap, Tier};
use fl_core::state::{validate_slug, validate_summary};
use fl_core::store::{AgentRegistry, Catalog};
use fl_exec::git::Git;
use fl_store::AnyStore;
use serde::Serialize;
use toml_edit::{DocumentMut, Item, Table, value};

/// The message of the empty commit that gives a new repository a `HEAD`.
pub const INITIAL_COMMIT_MESSAGE: &str = "chore: initial commit";

/// The areas a new project routes to its local tier, in routing order.
pub const DEFAULT_ROUTING_AREAS: [&str; 7] = [
    "backlog", "code", "design", "product", "security", "tests", "ux",
];

/// The default areas whose items are treated as security items.
const SENSITIVE_AREAS: [&str; 1] = ["security"];

/// The longest path accepted, in bytes.
const MAX_PATH_BYTES: usize = 4096;

/// The longest display name accepted, in characters.
pub const MAX_NAME_CHARS: usize = 80;

/// Slugs a virtual group may not take: `@all` is everyone, and `fl` is the Hub's own store.
const RESERVED_GROUP_SLUGS: [&str; 2] = ["all", "fl"];

/// Take the project configuration guard before opening a store for a local area edit.
pub fn lock_project_configuration(path: &Path) -> Result<ProjectConfigGuard, InitError> {
    project_config_lock::acquire(path, true)
}

// Independent fixtures still contend on the process-wide production config lock. Keep their
// lifetimes separate so a deliberate nonblocking delete tests its own contention scenario.
#[cfg(test)]
pub(crate) static CONFIG_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

/// Why a request was refused or failed, by who is at fault.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// The request is wrong: a path, name, repository or credential that cannot be used.
    #[error("{0}")]
    Invalid(String),
    /// The request is well formed but clashes with what exists: another project, another
    /// tracker, another store. Nothing was changed for it.
    #[error("{0}")]
    Conflict(String),
    /// The request names a project that does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The machine could not do it: git, a store or a file failed.
    #[error("{0}")]
    Failed(String),
    /// The user config file cannot be read as a project registry (it is not TOML, or `project`
    /// is not a list of tables). No request is at fault: every flow that reads the config
    /// stops, and the message names the file and what is wrong with it.
    #[error("{0}")]
    ConfigInvalid(String),
}

pub(crate) fn invalid(msg: impl Into<String>) -> InitError {
    InitError::Invalid(msg.into())
}

pub(crate) fn conflict(msg: impl Into<String>) -> InitError {
    InitError::Conflict(msg.into())
}

pub(crate) fn failed(msg: impl Into<String>) -> InitError {
    InitError::Failed(msg.into())
}

/// Where a project's GitHub credential comes from (`credential = "app"` or `"env"` in a
/// `[[project]]` tracker binding, docs/github-tracker.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    App,
    Env,
}

impl Credential {
    pub fn as_str(self) -> &'static str {
        match self {
            Credential::App => "app",
            Credential::Env => "env",
        }
    }
}

impl FromStr for Credential {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "app" => Ok(Credential::App),
            "env" => Ok(Credential::Env),
            other => Err(format!(
                "`{other}` is not a credential source: use `app` or `env`"
            )),
        }
    }
}

/// A project's tracker binding when it is not the local store. It serializes as
/// `{"kind": "github", "repo": "owner/repo", "credential": "app"|"env"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracker {
    pub github: String,
    pub credential: Credential,
}

impl Serialize for Tracker {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut out = s.serialize_struct("Tracker", 3)?;
        out.serialize_field("kind", "github")?;
        out.serialize_field("repo", &self.github)?;
        out.serialize_field("credential", &self.credential)?;
        out.end()
    }
}

/// Which kind of project: see the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    /// A directory-backed project.
    Project,
    /// A virtual group: an area with no path.
    Virtual,
}

impl ProjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectKind::Project => "project",
            ProjectKind::Virtual => "virtual",
        }
    }
}

impl FromStr for ProjectKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "project" => Ok(ProjectKind::Project),
            "virtual" => Ok(ProjectKind::Virtual),
            other => Err(format!(
                "`{other}` is not a project kind: use `project` or `virtual`"
            )),
        }
    }
}

/// What to create as a directory-backed project.
#[derive(Debug, Clone)]
pub struct InitRequest {
    /// The project root: an absolute path, made if it does not exist.
    pub path: PathBuf,
    /// The project's slug. Without it: the slug the config already gives that root, else the
    /// one derived from `display_name`, else the root's lowercase directory name.
    pub slug: Option<String>,
    /// The name shown for the project (the area's name). Without it: the area's present name,
    /// else the slug.
    pub display_name: Option<String>,
    /// Create missing ancestor groups.
    pub create_parent: bool,
    /// Resume an interrupted flat registration from its existing project store.
    pub resume_flat: bool,
    /// `owner/repo`: bind the project's tracker to this GitHub repository.
    pub github: Option<String>,
    /// The tracker's credential source; `app` when `github` is given and this is not.
    pub credential: Option<Credential>,
    /// What the project is for, as the workspace area's purpose.
    pub purpose: Option<String>,
    /// Run `git init` (and make the empty initial commit when there is no `HEAD`). Git is not
    /// touched otherwise.
    pub git_init: bool,
    /// Who is creating, for the initial commit and the area.
    pub by: String,
}

/// What to create as a virtual group.
#[derive(Debug, Clone)]
pub struct VirtualRequest {
    /// The group's slug; without it, derived from `display_name`.
    pub slug: Option<String>,
    /// The name shown for the group.
    pub display_name: String,
    /// Create missing ancestor groups.
    pub create_parent: bool,
    /// What the group is for; an existing group keeps its purpose when this is `None`.
    pub purpose: Option<String>,
    /// Who is creating.
    pub by: String,
}

/// The three places that differ between callers.
#[derive(Debug, Clone)]
pub struct InitPaths {
    /// The user config file, `config.toml`. It need not exist.
    pub config_path: PathBuf,
    /// The directory a new project's store goes in, as `<slug>.fls`.
    pub store_dir: PathBuf,
    /// The workspace store, which a project's slug must not name.
    pub workspace_store: PathBuf,
}

/// A request checked against the config, with nothing changed yet.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The project's slug, valid.
    pub slug: String,
    /// The canonical root.
    pub root: PathBuf,
    display_name: Option<String>,
    /// The project's store, as the config names it (an existing entry keeps its store).
    pub store: PathBuf,
    /// The tracker the project has, or will have, if any.
    pub tracker: Option<Tracker>,
    /// Whether the config has no block for this project yet.
    pub(crate) registers: bool,
    /// Whether a tracker is to be added to the block the config already has.
    adds_tracker: bool,
}

/// What happened to the repository, when `git_init` asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitOutcome {
    /// `git init` ran.
    pub initialized: bool,
    /// The initial empty commit was made.
    pub initial_commit: bool,
    /// `HEAD` afterwards.
    pub head: String,
}

/// What a creation did.
#[derive(Debug, Clone, Serialize)]
pub struct InitOutcome {
    pub slug: String,
    pub kind: ProjectKind,
    /// The project root; `None` for a virtual group.
    pub root: Option<String>,
    /// The project's store; `None` for a virtual group.
    pub store: Option<String>,
    /// The project's IRI in its store; `None` for a virtual group.
    pub project: Option<String>,
    pub purpose: String,
    pub tracker: Option<Tracker>,
    /// The areas the project's routing map declares (none for a virtual group).
    pub areas: Vec<String>,
    /// `None` unless `git_init` was asked for.
    pub git: Option<GitOutcome>,
    /// `.fl/mcp.toml` was made (it was absent).
    pub mcp_catalog_created: bool,
    /// The config file the project is registered in; `None` for a virtual group.
    pub config: Option<String>,
    /// New: a project's config block was appended, or a virtual group's area was made.
    pub registered: bool,
}

/// The default routing map of a new project: every default area routes to the local tier.
pub fn default_routing_map() -> RoutingMap {
    DEFAULT_ROUTING_AREAS
        .iter()
        .fold(RoutingMap::default(), |map, area| {
            map.with(area, Tier::Local, SENSITIVE_AREAS.contains(area))
        })
}

/// Whether `root` is a Git working tree: it holds a `.git` (a directory, or the file a linked
/// worktree has). Asks git nothing, so it never changes anything, and a repository with no
/// commit counts.
pub fn has_git(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".git")).is_ok()
}

// --- the config -------------------------------------------------------------------------------

/// One `[[project]]` block, as far as initialization reads it.
#[derive(Debug, Clone)]
pub(crate) struct ConfigEntry {
    pub(crate) name: Option<String>,
    pub(crate) root: String,
    pub(crate) store: Option<String>,
    pub(crate) tracker: Option<(String, String)>,
    pub(crate) cwd: Option<Item>,
}

impl ConfigEntry {
    /// Exact-root opt-in; malformed values must not silently select another directory.
    pub(crate) fn exact_cwd(&self) -> Result<bool, String> {
        match self.cwd.as_ref() {
            None => Ok(false),
            Some(item) => match item.as_str() {
                Some("workspace") => Ok(false),
                Some("exact") => Ok(true),
                _ => Err(format!(
                    "project binding `{}`: `cwd` must be `workspace` or `exact`",
                    self.slug().unwrap_or_else(|| self.root.clone())
                )),
            },
        }
    }
    /// The tracker binding the block has, if it names a GitHub repository.
    pub(crate) fn tracker_binding(&self) -> Option<Tracker> {
        self.tracker.as_ref().map(|(github, credential)| Tracker {
            github: github.clone(),
            credential: credential.parse().unwrap_or(Credential::App),
        })
    }

    /// The name the config gives the project: `name`, else the root's directory name, in
    /// lowercase (`Entry::matches_slug`).
    pub(crate) fn slug(&self) -> Option<String> {
        match &self.name {
            Some(n) => Some(n.to_ascii_lowercase()),
            None => Path::new(&self.root)
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase),
        }
    }
}

/// Resolve all bindings for a project as one canonical root and policy.
/// A present but invalid or conflicting binding must never fall back to a decoy.
pub(crate) fn resolve_bound_project(
    entries: &[ConfigEntry],
    name: &str,
) -> Result<Option<(PathBuf, bool)>, String> {
    let mut resolved = Vec::new();
    for entry in entries
        .iter()
        .filter(|e| e.slug().is_some_and(|slug| slug.eq_ignore_ascii_case(name)))
    {
        let exact = entry.exact_cwd()?;
        if entry.root.is_empty() {
            return Err(format!("registered project root unavailable for {name}"));
        }
        let path = Path::new(&entry.root);
        let root = path
            .is_absolute()
            .then(|| path.canonicalize().ok().filter(|p| p.is_dir()))
            .flatten()
            .ok_or_else(|| format!("registered project root unavailable for {name}"))?;
        resolved.push((root, exact));
    }
    resolved.sort();
    resolved.dedup();
    match resolved.len() {
        0 => Ok(None),
        1 => Ok(resolved.pop()),
        _ => Err(format!(
            "project binding `{name}` has conflicting roots or cwd policies"
        )),
    }
}

pub(crate) fn read_config(path: &Path) -> Result<DocumentMut, InitError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(failed(format!("could not read {}: {e}", path.display()))),
    };
    text.parse::<DocumentMut>().map_err(|e| {
        InitError::ConfigInvalid(format!(
            "the config {} is not valid TOML: {e}",
            path.display()
        ))
    })
}

pub(crate) fn config_entries(
    doc: &DocumentMut,
    path: &Path,
) -> Result<Vec<ConfigEntry>, InitError> {
    let Some(item) = doc.get("project") else {
        return Ok(Vec::new());
    };
    let Some(blocks) = item.as_array_of_tables() else {
        return Err(InitError::ConfigInvalid(format!(
            "`project` in the config {} is not a list of `[[project]]` tables",
            path.display()
        )));
    };
    Ok(blocks.iter().filter_map(entry_of).collect())
}

/// Preserve named malformed roots for binding resolution, so they cannot fall through
/// to a catalog or workspace decoy. Other config consumers retain their existing view.
pub(crate) fn binding_config_entries(
    doc: &DocumentMut,
    path: &Path,
) -> Result<Vec<ConfigEntry>, InitError> {
    let mut entries = config_entries(doc, path)?;
    if let Some(blocks) = doc.get("project").and_then(Item::as_array_of_tables) {
        for table in blocks {
            if table.get("root").and_then(Item::as_str).is_none()
                && let Some(name) = table.get("name").and_then(Item::as_str)
            {
                entries.push(ConfigEntry {
                    name: Some(name.to_owned()),
                    root: String::new(), // Invalid sentinel; never use as a path.
                    store: None,
                    tracker: None,
                    cwd: table.get("cwd").cloned(),
                });
            }
        }
    }
    Ok(entries)
}

/// One `[[project]]` table as an entry; `None` for a table with no `root`.
fn entry_of(t: &Table) -> Option<ConfigEntry> {
    let text = |key: &str| t.get(key).and_then(Item::as_str).map(str::to_string);
    let root = text("root")?;
    let tracker = t
        .get("tracker")
        .and_then(Item::as_table_like)
        .and_then(|tr| {
            Some((
                tr.get("github")?.as_str()?.to_string(),
                tr.get("credential")?.as_str()?.to_string(),
            ))
        });
    Some(ConfigEntry {
        name: text("name"),
        root,
        store: text("store"),
        tracker,
        cwd: t.get("cwd").cloned(),
    })
}

/// Removes every `[[project]]` block that names `slug` (by [`ConfigEntry::slug`]) and returns
/// how many it removed. The `project` key goes with its last block. A table with no `root`
/// is no entry and stays.
pub(crate) fn remove_blocks(doc: &mut DocumentMut, slug: &str) -> usize {
    let Some(blocks) = doc
        .get_mut("project")
        .and_then(Item::as_array_of_tables_mut)
    else {
        return 0;
    };
    let mut removed = 0;
    let mut index = 0;
    while index < blocks.len() {
        let named = blocks
            .get(index)
            .and_then(entry_of)
            .and_then(|e| e.slug())
            .is_some_and(|s| s == slug);
        if named {
            blocks.remove(index);
            removed += 1;
        } else {
            index += 1;
        }
    }
    if blocks.is_empty() {
        doc.remove("project");
    }
    removed
}

/// Whether two roots name one directory: equal as written, or the same after resolving links.
pub(crate) fn same_root(a: &str, b: &Path) -> bool {
    let a = Path::new(a);
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
}

/// Replaces `path` whole by a rename of a sibling temporary file, keeping the old file's
/// permissions. A symbolic link is written through, so a config kept elsewhere stays a link.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<(), InitError> {
    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => path
            .canonicalize()
            .map_err(|e| failed(format!("could not resolve {}: {e}", path.display())))?,
        _ => path.to_path_buf(),
    };
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| failed(format!("{} has no directory", path.display())))?;
    fl_store::private::create_private_dir_all(dir)
        .map_err(|e| failed(format!("could not create {}: {e}", dir.display())))?;
    let io = |e: std::io::Error| failed(format!("could not write {}: {e}", path.display()));
    let mut temp = tempfile::Builder::new()
        .prefix(".config.toml-")
        .tempfile_in(dir)
        .map_err(io)?;
    temp.write_all(text.as_bytes()).map_err(io)?;
    if let Ok(meta) = std::fs::metadata(&target) {
        std::fs::set_permissions(temp.path(), meta.permissions()).map_err(io)?;
    }
    temp.as_file().sync_all().map_err(io)?;
    temp.persist(&target).map_err(|e| io(e.error))?;
    Ok(())
}

/// A TOML basic string for `s`.
fn quoted(s: &str) -> String {
    toml_edit::Value::from(s).decorated("", "").to_string()
}

/// The text of a new project's block: `[[project]]`, and `[project.tracker]` when it has one.
fn block_text(r: &Resolved) -> String {
    let mut text = format!(
        "[[project]]\nname = {}\nroot = {}\nstore = {}\n",
        quoted(&r.slug),
        quoted(&r.root.to_string_lossy()),
        quoted(&r.store.to_string_lossy()),
    );
    if let Some(t) = &r.tracker {
        text.push_str(&format!(
            "\n[project.tracker]\ngithub = {}\ncredential = {}\n",
            quoted(&t.github),
            quoted(t.credential.as_str())
        ));
    }
    text
}

/// Adds the project's block to the config, or the tracker to the block it has. A new block is
/// appended to the file's own text, so everything already there stays byte for byte; a tracker
/// is added with `toml_edit`, which keeps the block's comments.
fn upsert_config(paths: &InitPaths, r: &Resolved) -> Result<(), InitError> {
    if !r.registers && !r.adds_tracker {
        return Ok(());
    }
    let text = match std::fs::read_to_string(&paths.config_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(failed(format!(
                "could not read {}: {e}",
                paths.config_path.display()
            )));
        }
    };
    let out = if r.registers {
        let mut out = text;
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&block_text(r));
        out
    } else {
        let mut doc = read_config(&paths.config_path)?;
        let tracker = r
            .tracker
            .as_ref()
            .ok_or_else(|| failed("a tracker was to be added and there is none"))?;
        let block = doc
            .get_mut("project")
            .and_then(Item::as_array_of_tables_mut)
            .and_then(|blocks| {
                blocks.iter_mut().find(|b| {
                    b.get("root")
                        .and_then(Item::as_str)
                        .is_some_and(|root| same_root(root, &r.root))
                })
            })
            .ok_or_else(|| failed("the project's config block vanished while it was edited"))?;
        let mut table = Table::new();
        table.insert("github", value(tracker.github.as_str()));
        table.insert("credential", value(tracker.credential.as_str()));
        block.insert("tracker", Item::Table(table));
        doc.to_string()
    };
    // Nothing is written that this module could not read back.
    out.parse::<DocumentMut>()
        .map_err(|e| failed(format!("the edited config is not valid TOML: {e}")))?;
    write_atomic(&paths.config_path, &out)
}

// --- the request ------------------------------------------------------------------------------

/// `path` made absolute-canonical without requiring it to exist: the nearest existing ancestor
/// resolved, the rest appended. A relative path, a `..`, a NUL, an empty path and a path that
/// is a file are refused.
pub(crate) fn canonical_root(path: &Path) -> Result<PathBuf, InitError> {
    let Some(text) = path.to_str() else {
        return Err(invalid("the path is not valid UTF-8"));
    };
    if text.trim().is_empty() {
        return Err(invalid("the path is empty"));
    }
    if text.contains('\0') {
        return Err(invalid("the path contains a NUL character"));
    }
    if text.len() > MAX_PATH_BYTES {
        return Err(invalid(format!(
            "the path is longer than {MAX_PATH_BYTES} bytes"
        )));
    }
    if !path.is_absolute() {
        return Err(invalid(format!("`{text}` is not an absolute path")));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(invalid(format!("`{text}` contains a `..` component")));
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    let mut resolved = loop {
        match std::fs::canonicalize(&cur) {
            Ok(found) => break found,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = cur.file_name() else {
                    return Err(invalid(format!("`{text}` cannot be resolved")));
                };
                tail.push(name.to_os_string());
                cur.pop();
            }
            Err(e) => return Err(invalid(format!("`{text}` cannot be resolved: {e}"))),
        }
    };
    resolved.extend(tail.iter().rev());
    if resolved.exists() && !resolved.is_dir() {
        return Err(invalid(format!(
            "`{}` exists and is not a directory, so it cannot be a project root",
            resolved.display()
        )));
    }
    Ok(resolved)
}

/// The slug for a directory: its lowercase name.
fn derive_slug(root: &Path) -> Result<String, InitError> {
    let base = root
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    validate_slug(&base).map_err(|_| {
        invalid(format!(
            "cannot derive a project name from `{base}`: a name is 1 to 64 of lowercase letters, \
             digits, `-` and `_`, starting with a letter. Pass one with --name or --slug"
        ))
    })
}

/// A slug as given: lowercase letters, digits, `-` and `_`, starting with a letter.
pub(crate) fn checked_slug(raw: &str) -> Result<String, InitError> {
    validate_slug(raw.trim()).map_err(|_| {
        invalid(format!(
            "`{raw}` is not a project name: a name is 1 to 64 of lowercase letters, digits, \
             `-` and `_`, starting with a letter"
        ))
    })
}

/// A slug derived from a display name: lowercased, every run of other characters one `-`, no
/// `-` at either end. `My Project` is `my-project`; `Chosen_Name` is `chosen_name`.
pub(crate) fn slug_from_display(name: &str) -> Result<String, InitError> {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else if c == '-' || c.is_whitespace() || c.is_ascii_punctuation() {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            // Not ASCII: nothing to carry over.
            if !out.ends_with('-') {
                out.push('-');
            }
        }
    }
    let derived = out.trim_matches('-').to_string();
    validate_slug(&derived).map_err(|_| {
        invalid(format!(
            "cannot derive a slug from the name `{name}`: a slug is 1 to 64 of lowercase \
             letters, digits, `-` and `_`, starting with a letter. Pass one as `slug`"
        ))
    })
}

/// A display name: one line, 1 to [`MAX_NAME_CHARS`] characters, no control characters.
pub(crate) fn checked_display_name(raw: &str) -> Result<String, InitError> {
    let name = raw.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(invalid(format!(
            "name: a name is one line of 1 to {MAX_NAME_CHARS} characters"
        )));
    }
    Ok(name.to_string())
}

/// A purpose: one line, as an area's purpose.
pub(crate) fn checked_purpose(raw: &str) -> Result<String, InitError> {
    validate_summary(raw).map_err(|e| invalid(format!("purpose: {e}")))
}

/// The purpose a project has when none is given, and the one an empty `purpose` resets it to.
pub(crate) fn default_purpose(kind: ProjectKind, slug: &str) -> String {
    match kind {
        ProjectKind::Project => format!("Project {slug}"),
        ProjectKind::Virtual => format!("Virtual group {slug}"),
    }
}

/// `owner/repo` or an error naming the form.
pub(crate) fn checked_github(raw: &str) -> Result<String, InitError> {
    let g = raw.trim();
    if fl_github::is_owner_repo(g) {
        Ok(g.to_string())
    } else {
        Err(invalid(format!(
            "`{g}` is not a GitHub repository: name it as `owner/repo`"
        )))
    }
}

/// Refuses a root and the stores' directory that contain one another.
pub(crate) fn check_store_dir_overlap(root: &Path, paths: &InitPaths) -> Result<(), InitError> {
    let store_dir = paths.store_dir.canonicalize().unwrap_or_else(|_| {
        paths
            .store_dir
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .map(|p| p.join(paths.store_dir.file_name().unwrap_or_default()))
            .unwrap_or_else(|| paths.store_dir.clone())
    });
    if root.starts_with(&store_dir) || store_dir.starts_with(root) {
        return Err(invalid(format!(
            "{} and the stores' directory {} contain one another, so it cannot be a project root",
            root.display(),
            store_dir.display()
        )));
    }
    Ok(())
}

/// Checks `req` against the config and the places in `paths`. Changes nothing.
pub fn resolve(req: &InitRequest, paths: &InitPaths) -> Result<Resolved, InitError> {
    resolve_project(req, paths, false, |_, _, _| Ok(false))
}

/// An old flat name can bypass hierarchy parsing or derivation only when it still names
/// the same stored area. Pass a configured or explicit slug to keep it authoritative.
fn display_identity(
    raw: &str,
    slug: Option<String>,
    matches_legacy: impl FnOnce(&str, &str) -> Result<bool, InitError>,
) -> Result<(String, String), InitError> {
    let raw = checked_display_name(raw)?;
    let current = display_path(&raw).and_then(|name| {
        let slug = slug
            .clone()
            .map_or_else(|| slug_from_project_name(&name), Ok)?;
        Ok((slug, name))
    });
    if let Ok(legacy_slug) = slug.map_or_else(|| slug_from_display(&raw), Ok)
        && !current.as_ref().is_ok_and(|(slug, _)| *slug == legacy_slug)
        && matches_legacy(&legacy_slug, &raw)?
    {
        return Ok((legacy_slug, raw));
    }
    current
}

fn resolve_project(
    req: &InitRequest,
    paths: &InitPaths,
    create_only: bool,
    matches_legacy: impl FnOnce(&str, &str, &Path) -> Result<bool, InitError>,
) -> Result<Resolved, InitError> {
    let root = canonical_root(&req.path)?;
    if req.by.trim().is_empty() {
        return Err(invalid("`by` is empty"));
    }
    if let Some(p) = &req.purpose {
        checked_purpose(p)?;
    }
    let github = req.github.as_deref().map(checked_github).transpose()?;
    if github.is_none() && req.credential.is_some() {
        return Err(invalid(
            "a credential source names how to reach a GitHub repository: pass `github` too",
        ));
    }

    let doc = read_config(&paths.config_path)?;
    let entries = config_entries(&doc, &paths.config_path)?;
    let here = entries.iter().find(|e| same_root(&e.root, &root));
    let known_slug = match (&req.slug, here) {
        (Some(s), _) => Some(checked_slug(s)?),
        (None, Some(e)) => Some(checked_slug(&e.slug().unwrap_or_default())?),
        (None, None) => None,
    };
    if req.resume_flat && (req.slug.is_none() || req.display_name.is_none() || req.create_parent) {
        return Err(invalid(
            "--resume-flat requires --slug, --name and --path, and cannot create ancestors",
        ));
    }
    let (slug, display_name) = match &req.display_name {
        Some(raw) if req.resume_flat => (
            known_slug.expect("flat recovery requires an explicit slug"),
            Some(checked_display_name(raw)?),
        ),
        Some(raw) => {
            let (slug, name) = display_identity(raw, known_slug, |slug, name| {
                matches_legacy(slug, name, &root)
            })?;
            (slug, Some(name))
        }
        None => (known_slug.map_or_else(|| derive_slug(&root), Ok)?, None),
    };
    if let Some(e) = here
        && e.slug().as_deref() != Some(slug.as_str())
    {
        return Err(conflict(format!(
            "{} is already registered as project `{}`, not `{slug}`. Use that name, or edit {}",
            root.display(),
            e.slug().unwrap_or_default(),
            paths.config_path.display()
        )));
    }
    if let Some(other) = entries
        .iter()
        .find(|e| !same_root(&e.root, &root) && e.slug().as_deref() == Some(slug.as_str()))
    {
        return Err(conflict(format!(
            "project `{slug}` is already registered at {}, not {}. Choose another name with \
             --name or --slug",
            other.root,
            root.display()
        )));
    }

    let store = match here.and_then(|e| e.store.as_deref()) {
        Some(s) => PathBuf::from(s),
        None if req.resume_flat || create_only => historical_store_path(paths, &slug),
        None => paths.store_dir.join(format!("{slug}.fls")),
    };
    if fl_store::store_identity(&store) == fl_store::store_identity(&paths.workspace_store) {
        return Err(conflict(format!(
            "the store for project `{slug}` would be the workspace store {}. Choose another name \
             with --name or --slug",
            paths.workspace_store.display()
        )));
    }
    check_store_dir_overlap(&root, paths)?;

    let existing = here.and_then(|e| e.tracker.as_ref());
    // A create-only retry can finish registration, but cannot edit a config block.
    let requested_tracker = github.as_ref().filter(|_| !create_only || here.is_none());
    let (tracker, adds_tracker) = match (requested_tracker, existing) {
        (Some(g), Some((eg, ec))) => {
            let wanted = req.credential.map(Credential::as_str);
            if !g.eq_ignore_ascii_case(eg) || wanted.is_some_and(|w| w != ec) {
                return Err(conflict(format!(
                    "project `{slug}` is already bound to GitHub `{eg}` with credential `{ec}`; \
                     creating will not rebind it. Change the binding with PATCH \
                     /api/v1/projects/{slug}, or edit {}",
                    paths.config_path.display()
                )));
            }
            (
                Some(Tracker {
                    github: eg.clone(),
                    credential: ec.parse().unwrap_or(Credential::App),
                }),
                false,
            )
        }
        (Some(g), None) => (
            Some(Tracker {
                github: g.clone(),
                credential: req.credential.unwrap_or(Credential::App),
            }),
            here.is_some(),
        ),
        (None, Some((eg, ec))) => (
            Some(Tracker {
                github: eg.clone(),
                credential: ec.parse().unwrap_or(Credential::App),
            }),
            false,
        ),
        (None, None) => (None, false),
    };

    Ok(Resolved {
        slug,
        root,
        display_name,
        store,
        tracker,
        registers: here.is_none(),
        adds_tracker,
    })
}

// --- the work ---------------------------------------------------------------------------------

/// The repository of `root` itself: its own `.git`, never one of a parent directory (a project
/// nested in another repository gets its own). Every git call runs with the variables that
/// would point it elsewhere removed.
fn ensure_git(root: &Path, by: &str) -> Result<GitOutcome, InitError> {
    let git = |e: fl_exec::population::ExecError| failed(e.to_string());
    match std::fs::symlink_metadata(root.join(".git")) {
        Ok(_) => {
            if let Ok(head) = Git::head_scrubbed(root) {
                return Ok(GitOutcome {
                    initialized: false,
                    initial_commit: false,
                    head,
                });
            }
            // A repository with no commit yet.
            let head = Git::commit_empty(root, INITIAL_COMMIT_MESSAGE, by).map_err(git)?;
            Ok(GitOutcome {
                initialized: false,
                initial_commit: true,
                head,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Git::init(root).map_err(git)?;
            let head = Git::commit_empty(root, INITIAL_COMMIT_MESSAGE, by).map_err(git)?;
            Ok(GitOutcome {
                initialized: true,
                initial_commit: true,
                head,
            })
        }
        Err(e) => Err(failed(format!("could not look for .git in the root: {e}"))),
    }
}

/// Puts the project in its store (once) and gives it the default routing map (if it has none).
/// Returns the project's IRI and the areas its map declares.
fn seed_project_store(
    store: &AnyStore,
    r: &Resolved,
    require_owner: bool,
) -> Result<(String, Vec<String>), InitError> {
    let st = |e: fl_core::store::StoreError| failed(format!("store: {e}"));
    let root = r.root.to_string_lossy();
    let held = store.list_projects().map_err(st)?;
    let id = match held.as_slice() {
        [p] if same_root(&p.root, &r.root) => p.id.clone(),
        _ if require_owner => return Err(resume_store_refusal(r)),
        [] => store.add_project(&root).map_err(st)?,
        _ => return Err(store_owner_conflict(r, &held)),
    };
    let map = match store.routes(&id).map_err(st)? {
        Some(map) => map,
        None => {
            let map = default_routing_map();
            store.set_routes(&id, &map).map_err(st)?;
            map
        }
    };
    Ok((
        id.iri().as_str().to_string(),
        map.areas.iter().map(|a| a.area.clone()).collect(),
    ))
}

fn store_owner_conflict(resolved: &Resolved, held: &[fl_core::model::Project]) -> InitError {
    conflict(format!(
        "the store {} already holds {}, not {}. A project's store holds only that \
         project: choose another name with --name or --slug",
        resolved.store.display(),
        held.iter()
            .map(|project| project.root.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        resolved.root.display(),
    ))
}

fn historical_store_path(paths: &InitPaths, slug: &str) -> PathBuf {
    let ferro = paths.store_dir.join(format!("{slug}.fls"));
    let redb = ferro.with_extension("redb");
    if std::fs::symlink_metadata(&ferro).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        && redb.is_file()
    {
        redb
    } else {
        ferro
    }
}

fn check_create_store_owner(
    resolved: &Resolved,
    projects: &[fl_core::model::Project],
    require_owner: bool,
) -> Result<(), InitError> {
    match projects {
        [] if !require_owner => Ok(()),
        [project] if same_root(&project.root, &resolved.root) => Ok(()),
        [] => Err(resume_store_refusal(resolved)),
        _ => Err(store_owner_conflict(resolved, projects)),
    }
}

fn check_lone_redb_owner(resolved: &Resolved) -> Result<(), InitError> {
    let error =
        |e: fl_core::store::StoreError| failed(format!("store {}: {e}", resolved.store.display()));
    // The usual scan can repair an unreadable redb. Prove this unbound store's owner
    // with a strictly read-only open before permitting its ordinary open to migrate it.
    let redb = fl_store::RedbStore::open_read_only(
        &resolved.store,
        fl_store::StoreOptions::default().retries,
    )
    .map_err(error)?;
    let projects = redb.list_projects().map_err(error)?;
    check_create_store_owner(resolved, &projects, true)
}

fn resume_store_refusal(resolved: &Resolved) -> InitError {
    conflict(format!(
        "the store {} does not hold exactly the project at {}",
        resolved.store.display(),
        resolved.root.display()
    ))
}

fn check_resume_store(resolved: &Resolved) -> Result<(), InitError> {
    if !fl_store::store_exists(&resolved.store) {
        return Err(conflict(format!(
            "--resume-flat requires an existing project store at {}",
            resolved.store.display()
        )));
    }
    // Scan opens may initialize a zero-byte file. A flat recovery must find existing data.
    let physical = fl_store::store_identity(&resolved.store);
    let metadata = std::fs::metadata(physical).or_else(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            std::fs::metadata(&resolved.store)
        } else {
            Err(e)
        }
    });
    if metadata.is_ok_and(|meta| meta.is_file() && meta.len() == 0) {
        return Err(resume_store_refusal(resolved));
    }
    // A wrong root must not cause a lone redb to be migrated.
    let store = fl_store::open_store_for_scan(&resolved.store)
        .map_err(|e| failed(format!("store {}: {e}", resolved.store.display())))?;
    let projects = store
        .list_projects()
        .map_err(|e| failed(format!("store {}: {e}", resolved.store.display())))?;
    match projects.as_slice() {
        [project] if same_root(&project.root, &resolved.root) => Ok(()),
        _ => Err(resume_store_refusal(resolved)),
    }
}

/// The registry's refusal when a project's slug is already a virtual group. The way out is
/// named: an area made earlier for a real repository (`--create-area`) has no path either, and
/// `fl agent area set` gives it one.
fn virtual_slug_taken_by_group(slug: &str) -> InitError {
    conflict(format!(
        "`{slug}` is already a virtual group (an area with no path), so it cannot also be a \
         directory-backed project. Choose another name with --name or --slug; or, if that area \
         was made for this directory, give it the directory as its path with `fl agent area set \
         {slug} --path <dir> --purpose <text> --by <you>` and run this again"
    ))
}

/// A new config binding cannot take a virtual group's slug or repoint a different project.
/// A project that the config already registers keeps its area.
fn check_area_for_project(area: Option<&AreaRecord>, r: &Resolved) -> Result<(), InitError> {
    if !r.registers {
        return Ok(());
    }
    if area.is_some_and(AreaRecord::is_virtual) {
        return Err(virtual_slug_taken_by_group(&r.slug));
    }
    match area
        .and_then(|area| area.path.as_deref())
        .filter(|path| !path.trim().is_empty())
    {
        Some(path) if !same_root(path, &r.root) => Err(conflict(format!(
            "`{}` is already an area for the directory {path}, not {}. Choose another slug \
             with --slug or --name, or register the project at {path}",
            r.slug,
            r.root.display()
        ))),
        _ => Ok(()),
    }
}

fn registry_areas(ws: &AnyStore) -> Result<BTreeMap<String, AreaRecord>, InitError> {
    Ok(ws
        .list_areas()
        .map_err(workspace_error)?
        .into_iter()
        .map(|area| (area.area_key.clone(), area))
        .collect())
}

fn workspace_error(error: fl_core::store::StoreError) -> InitError {
    failed(format!("workspace store: {error}"))
}

/// Keep the same workspace guard and registry snapshot from the legacy lookup through
/// placement. A legacy lookup must never create a workspace.
struct InitWorkspace<W, F> {
    opened: Option<(W, BTreeMap<String, AreaRecord>)>,
    opener: Option<F>,
}

impl<W: Deref<Target = AnyStore>, F: FnOnce() -> Result<W, InitError>> InitWorkspace<W, F> {
    fn new(opener: F) -> Self {
        Self {
            opened: None,
            opener: Some(opener),
        }
    }

    fn legacy_area(
        &mut self,
        path: &Path,
        slug: &str,
        name: &str,
    ) -> Result<Option<&AreaRecord>, InitError> {
        if !fl_store::store_exists(path) {
            return Ok(None);
        }
        let opened = self.read_registry()?;
        let (_, areas) = self.opened.insert(opened);
        Ok(areas
            .get(slug)
            .filter(|area| area.parent.is_none() && area.name == name))
    }

    fn cached_area(&self, slug: &str) -> Option<&AreaRecord> {
        self.opened.as_ref()?.1.get(slug)
    }

    fn read_registry(&mut self) -> Result<(W, BTreeMap<String, AreaRecord>), InitError> {
        let store = self.opener.take().expect("workspace opened once")()?;
        let areas = registry_areas(&store)?;
        Ok((store, areas))
    }

    fn open(mut self) -> Result<(W, BTreeMap<String, AreaRecord>), InitError> {
        match self.opened.take() {
            Some(workspace) => Ok(workspace),
            None => self.read_registry(),
        }
    }
}

/// A linked child or a parent with children can repeat its displayed name. A raw child
/// edge still counts while registry sync has left its projected chain incomplete.
pub(crate) fn project_name_changes(
    area: &AreaRecord,
    name: &str,
    areas: impl IntoIterator<Item = impl std::borrow::Borrow<AreaRecord>>,
) -> Result<bool, InitError> {
    if name == area.name {
        return Ok(false);
    }
    if area.parent.is_some()
        || areas
            .into_iter()
            .any(|child| child.borrow().parent.as_deref() == Some(area.area_key.as_str()))
    {
        if display_path(name).is_ok_and(|name| matches_display_path(&area.name, &name)) {
            return Ok(false);
        }
        return Err(conflict(format!(
            "cannot rename linked project `{}`",
            area.area_key
        )));
    }
    Ok(true)
}

struct Placement {
    name: String,
    parent: Option<String>,
    missing: Vec<AreaRecord>,
}

fn plan_placement(
    slug: &str,
    name: String,
    create_parent: bool,
    by: &str,
    areas: &BTreeMap<String, AreaRecord>,
    check_available_parent: impl Fn(&str) -> Result<(), InitError>,
) -> Result<Placement, InitError> {
    let mut parent: Option<String> = None;
    let mut missing = Vec::new();
    if let Some(area) = areas.get(slug) {
        project_name_changes(area, &name, areas.values())?;
        let name = if matches_display_path(&area.name, &name) {
            area.name.clone()
        } else if name.contains('/') {
            return Err(conflict(format!(
                "`{slug}` already exists as a top-level project. Choose another slug for `{name}`"
            )));
        } else {
            name
        };
        // The suffix of an old top-level area's display name has no topology.
        return Ok(Placement {
            name,
            parent: area.parent.clone(),
            missing,
        });
    }

    for (split, _) in name.match_indices('/') {
        let prefix = &name[..split];
        let mut matches = areas
            .values()
            .filter(|area| matches_display_path(&area.name, prefix));
        let key = if let Some(area) = matches.next() {
            if matches.next().is_some() {
                return Err(conflict(format!("project parent `{prefix}` is ambiguous")));
            }
            if area.parent != parent {
                return Err(conflict(format!(
                    "project parent `{prefix}` has an incompatible parent chain"
                )));
            }
            area.area_key.clone()
        } else {
            if !create_parent {
                return Err(invalid(format!(
                    "project parent `{prefix}` does not exist. Create it first or pass --create-parent"
                )));
            }
            let key = slug_from_project_name(prefix).map_err(|_| {
                invalid(format!(
                    "cannot derive a slug for parent `{prefix}`. Create it first with --slug"
                ))
            })?;
            if key == slug
                || areas.contains_key(&key)
                || missing.iter().any(|area: &AreaRecord| area.area_key == key)
            {
                return Err(conflict(format!(
                    "the slug `{key}` for project parent `{prefix}` is already in use"
                )));
            }
            check_available_parent(&key)?;
            missing.push(AreaRecord {
                area_key: key.clone(),
                name: prefix.to_string(),
                path: None,
                parent,
                purpose: default_purpose(ProjectKind::Virtual, &key),
                updated_at: fl_exec::stamp::now(),
                by: by.trim().to_string(),
            });
            key
        };
        parent = Some(key);
    }
    Ok(Placement {
        name,
        parent,
        missing,
    })
}

fn check_held_project(slug: &str, held: &[fl_core::model::Project]) -> Result<(), InitError> {
    let hints: [StoreHint; 1] = [(crate::records::DEFAULT_PROJECT.to_string(), None)];
    if is_project_slug(slug, &[], &hints, held) {
        return Err(conflict(format!(
            "`{slug}` is already a project, so it cannot also be a virtual group. Choose another slug"
        )));
    }
    Ok(())
}

fn publish_area(
    ws: &AnyStore,
    area: AreaRecord,
    missing: Vec<AreaRecord>,
    existing: Option<&AreaRecord>,
) -> Result<AreaRecord, InitError> {
    if let Some(before) = existing
        && before.name == area.name
        && before.purpose == area.purpose
        && before.path == area.path
        && before.parent == area.parent
    {
        return Ok(before.clone());
    }
    for ancestor in missing {
        ws.upsert_area(ancestor).map_err(workspace_error)?;
    }
    ws.upsert_area(area).map_err(workspace_error)
}

/// Writes the starter `.fl/mcp.toml` when the project has none. Whatever is there is left alone.
fn ensure_mcp_catalog(root: &Path) -> Result<bool, InitError> {
    match std::fs::symlink_metadata(fl_mcp::catalog::Catalog::path(root)) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fl_mcp::catalog::Editor::open(root)
                .and_then(|editor| editor.save())
                .map_err(|e| failed(e.to_string()))?;
            Ok(true)
        }
        Err(e) => Err(failed(format!("could not look for .fl/mcp.toml: {e}"))),
    }
}

/// Creates the directory-backed project `req` names.
///
/// `open_workspace` opens the workspace store and `open_project` the project's store, once the
/// slug and the store path are known; the caller picks how (a CLI process opens them directly,
/// the Hub through its locks). Order: nothing is changed until the request is checked against
/// the config and the workspace registry; git runs only when `git_init` asks; the config block
/// is written last, so a failure before it leaves the project unregistered and a re-run
/// finishes the job.
pub fn init_project<P, W>(
    req: &InitRequest,
    paths: &InitPaths,
    open_workspace: impl FnOnce() -> Result<W, InitError>,
    open_project: impl FnOnce(&Resolved) -> Result<P, InitError>,
) -> Result<InitOutcome, InitError>
where
    P: Deref<Target = AnyStore>,
    W: Deref<Target = AnyStore>,
{
    init_project_with_policy(req, paths, false, open_workspace, open_project)
}

/// Create-only requests can finish a partial registration without replacing an existing area.
pub(crate) fn init_project_with_policy<P, W>(
    req: &InitRequest,
    paths: &InitPaths,
    create_only: bool,
    open_workspace: impl FnOnce() -> Result<W, InitError>,
    open_project: impl FnOnce(&Resolved) -> Result<P, InitError>,
) -> Result<InitOutcome, InitError>
where
    P: Deref<Target = AnyStore>,
    W: Deref<Target = AnyStore>,
{
    let _one_at_a_time = lock_project_configuration(&paths.config_path)?;
    let mut workspace = InitWorkspace::new(open_workspace);
    let resolved = resolve_project(req, paths, create_only, |slug, name, root| {
        let area = workspace.legacy_area(&paths.workspace_store, slug, name)?;
        Ok(area
            .and_then(|area| area.path.as_deref())
            .is_some_and(|path| !path.trim().is_empty() && same_root(path, root)))
    })?;
    // An old initializer can leave its store before publishing the area or binding. With
    // no directory record anchoring that store to another root, require an explicit choice.
    if resolved.registers
        && req.slug.is_none()
        && let Some(name) = req.display_name.as_deref()
        && let Ok(legacy_slug) = slug_from_display(name)
        && legacy_slug != resolved.slug
    {
        let legacy_root = workspace
            .cached_area(&legacy_slug)
            .and_then(|area| area.path.as_deref());
        let legacy_store = historical_store_path(paths, &legacy_slug);
        if legacy_root.is_none_or(|path| path.trim().is_empty() || same_root(path, &resolved.root))
            && fl_store::store_exists(&legacy_store)
        {
            return Err(conflict(format!(
                "the historical store {} may be from an interrupted project setup. Use \
                 --resume-flat --slug {legacy_slug} with --name and --path to resume it flat, \
                 or --slug {} to create a separate project",
                legacy_store.display(),
                resolved.slug
            )));
        }
    }
    let lone_redb = create_only
        && resolved.registers
        && resolved.store.extension().is_some_and(|ext| ext == "redb");
    if req.resume_flat {
        check_resume_store(&resolved)?;
    } else if lone_redb {
        check_lone_redb_owner(&resolved)?;
    }
    let (workspace_store, registry) = workspace.open()?;
    let before = registry.get(&resolved.slug);
    check_area_for_project(before, &resolved)?;
    let name = resolved
        .display_name
        .clone()
        .unwrap_or_else(|| before.map_or_else(|| resolved.slug.clone(), |area| area.name.clone()));
    let preserve_area = create_only && before.is_some();
    let placement = if let Some(area) = before.filter(|_| create_only) {
        if !matches_display_path(&area.name, &name)
            || !area
                .path
                .as_deref()
                .is_some_and(|path| same_root(path, &resolved.root))
        {
            return Err(conflict(format!(
                "a different project or group with the slug `{}` already exists. Choose another slug",
                resolved.slug
            )));
        }
        Placement {
            name: area.name.clone(),
            parent: area.parent.clone(),
            missing: Vec::new(),
        }
    } else if req.resume_flat {
        if let Some(area) = before {
            if area.parent.is_some() {
                return Err(conflict(format!(
                    "cannot resume linked project `{}` as flat",
                    resolved.slug
                )));
            }
            if area.name != name
                || !area
                    .path
                    .as_deref()
                    .is_some_and(|path| same_root(path, &resolved.root))
            {
                return Err(conflict(format!(
                    "existing flat area `{}` must match --name and --path to resume it",
                    resolved.slug
                )));
            }
        }
        Placement {
            name,
            parent: None,
            missing: Vec::new(),
        }
    } else {
        plan_placement(
            &resolved.slug,
            name,
            req.create_parent,
            &req.by,
            &registry,
            |parent| {
                let doc = read_config(&paths.config_path)?;
                let entries = config_entries(&doc, &paths.config_path)?;
                check_virtual_slug(parent, paths, &entries)?;
                check_held_project(
                    parent,
                    &workspace_store.list_projects().map_err(workspace_error)?,
                )
            },
        )?
    };
    let purpose = match (req.purpose.as_ref().filter(|_| !preserve_area), before) {
        (Some(purpose), _) => checked_purpose(purpose)?,
        (None, Some(area)) => area.purpose.clone(),
        (None, None) => default_purpose(ProjectKind::Project, &resolved.slug),
    };

    let prepare_root = || {
        std::fs::create_dir_all(&resolved.root)
            .map_err(|e| failed(format!("could not create {}: {e}", resolved.root.display())))?;
        if req.git_init && (!create_only || resolved.registers) {
            ensure_git(&resolved.root, &req.by).map(Some)
        } else {
            Ok(None)
        }
    };
    // Keep the usual flow's ordering. A create-only request checks its store
    // first so a store-only collision cannot create a checkout or repository.
    let mut git = if create_only { None } else { prepare_root()? };

    if let Some(dir) = resolved
        .store
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        fl_store::private::create_private_dir_all(dir)
            .map_err(|e| failed(format!("could not create {}: {e}", dir.display())))?;
    }
    let project_store = open_project(&resolved)?;
    if create_only {
        let held = project_store
            .list_projects()
            .map_err(|e| failed(format!("store: {e}")))?;
        check_create_store_owner(&resolved, &held, lone_redb)?;
        git = prepare_root()?;
    }
    let (project, areas) =
        seed_project_store(&project_store, &resolved, req.resume_flat || lone_redb)?;
    let area = AreaRecord {
        area_key: resolved.slug.clone(),
        name: placement.name,
        path: before
            .filter(|_| preserve_area)
            .and_then(|area| area.path.clone())
            .or_else(|| Some(resolved.root.to_string_lossy().into_owned())),
        parent: placement.parent,
        purpose,
        updated_at: fl_exec::stamp::now(),
        by: req.by.trim().to_string(),
    };
    let area = publish_area(&workspace_store, area, placement.missing, before)?;
    drop(project_store);
    drop(workspace_store);

    let mcp_catalog_created = ensure_mcp_catalog(&resolved.root)?;
    upsert_config(paths, &resolved)?;

    Ok(InitOutcome {
        slug: resolved.slug.clone(),
        kind: ProjectKind::Project,
        root: Some(resolved.root.display().to_string()),
        store: Some(resolved.store.display().to_string()),
        project: Some(project),
        purpose: area.purpose,
        tracker: resolved.tracker.clone(),
        areas,
        git,
        mcp_catalog_created,
        config: Some(paths.config_path.display().to_string()),
        registered: resolved.registers,
    })
}

/// Creates the virtual group `req` names as an area with no path in the workspace store.
/// Virtual groups have no project directory, store file, config block or Git repository.
/// Repeats can update the group's purpose and, for a flat group, its display name.
pub fn create_virtual<W>(
    req: &VirtualRequest,
    paths: &InitPaths,
    open_workspace: impl FnOnce() -> Result<W, InitError>,
) -> Result<InitOutcome, InitError>
where
    W: Deref<Target = AnyStore>,
{
    create_virtual_with_policy(req, paths, false, open_workspace)
}

/// A creation-only caller can retry the same resolved group without changing it.
pub(crate) fn create_virtual_with_policy<W>(
    req: &VirtualRequest,
    paths: &InitPaths,
    create_only: bool,
    open_workspace: impl FnOnce() -> Result<W, InitError>,
) -> Result<InitOutcome, InitError>
where
    W: Deref<Target = AnyStore>,
{
    let _one_at_a_time = lock_project_configuration(&paths.config_path)?;
    if req.by.trim().is_empty() {
        return Err(invalid("`by` is empty"));
    }
    let purpose = req.purpose.as_deref().map(checked_purpose).transpose()?;
    let doc = read_config(&paths.config_path)?;
    let entries = config_entries(&doc, &paths.config_path)?;
    let known_slug = req.slug.as_deref().map(checked_slug).transpose()?;
    let mut workspace = InitWorkspace::new(open_workspace);
    let (slug, display) = display_identity(&req.display_name, known_slug, |slug, name| {
        let area = workspace.legacy_area(&paths.workspace_store, slug, name)?;
        Ok(area.is_some_and(AreaRecord::is_virtual))
    })?;
    check_virtual_slug(&slug, paths, &entries)?;
    let (ws, registry) = workspace.open()?;
    let held = ws.list_projects().map_err(workspace_error)?;
    check_held_project(&slug, &held)?;
    let before = registry.get(&slug);
    if let Some(path) = before.and_then(|area| area.path.as_deref()) {
        return Err(conflict(format!(
            "`{slug}` is already an area for the project at {path}, so it cannot also be a virtual group. Choose another slug"
        )));
    }
    if create_only && let Some(area) = before {
        return if matches_display_path(&area.name, &display) {
            Ok(virtual_outcome(area.clone(), false))
        } else {
            Err(conflict(format!(
                "a project or group with the slug `{slug}` already exists. Choose another slug"
            )))
        };
    }
    let placement = plan_placement(
        &slug,
        display,
        req.create_parent,
        &req.by,
        &registry,
        |parent| {
            check_virtual_slug(parent, paths, &entries)?;
            check_held_project(parent, &held)
        },
    )?;
    let area = AreaRecord {
        purpose: purpose.unwrap_or_else(|| {
            before.map_or_else(
                || default_purpose(ProjectKind::Virtual, &slug),
                |area| area.purpose.clone(),
            )
        }),
        area_key: slug,
        name: placement.name,
        path: None,
        parent: placement.parent,
        updated_at: fl_exec::stamp::now(),
        by: req.by.trim().to_string(),
    };
    let saved = publish_area(&ws, area, placement.missing, before)?;
    Ok(virtual_outcome(saved, before.is_none()))
}

fn check_virtual_slug(
    slug: &str,
    paths: &InitPaths,
    entries: &[ConfigEntry],
) -> Result<(), InitError> {
    if RESERVED_GROUP_SLUGS.contains(&slug) {
        return Err(invalid(format!(
            "`{slug}` cannot be a virtual group: `@all` is everyone and `fl` is the Hub's own store. Choose another slug"
        )));
    }
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.slug().as_deref() == Some(slug))
    {
        return Err(conflict(format!(
            "`{slug}` is already a directory-backed project at {}, so it cannot also be a virtual group. Choose another slug",
            entry.root
        )));
    }
    for ext in ["fls", "redb"] {
        let store = paths.store_dir.join(format!("{slug}.{ext}"));
        match std::fs::symlink_metadata(&store) {
            Ok(_) => {
                return Err(conflict(format!(
                    "`{slug}` is already a project: its store {} exists. Choose another slug",
                    store.display()
                )));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(failed(format!(
                    "could not check {}: {err}",
                    store.display()
                )));
            }
        }
    }
    Ok(())
}

fn virtual_outcome(area: AreaRecord, registered: bool) -> InitOutcome {
    InitOutcome {
        slug: area.area_key,
        kind: ProjectKind::Virtual,
        root: None,
        store: None,
        project: None,
        purpose: area.purpose,
        tracker: None,
        areas: Vec::new(),
        git: None,
        mcp_catalog_created: false,
        config: None,
        registered,
    }
}

// --- the user config's place ------------------------------------------------------------------

/// The user config file for a Hub whose home is `home`: `<home>/.config/fl/config.toml`.
/// With no home given, `$XDG_CONFIG_HOME/fl/config.toml` (absolute values only) or
/// `$HOME/.config/fl/config.toml`, the CLI's own rule. `None` when none applies.
pub fn user_config_path(home: Option<&Path>) -> Option<PathBuf> {
    // Unit tests of this crate name a temporary home; one that does not would read (and could
    // write) the developer's own config.
    #[cfg(test)]
    assert!(
        home.is_some(),
        "a test must name a temporary home, never the real config of the machine"
    );
    let base = match home {
        Some(h) => h.join(".config"),
        None => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?,
    };
    Some(base.join("fl").join("config.toml"))
}
