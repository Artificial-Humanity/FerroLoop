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

use std::io::Write as _;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

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

/// One change to the config or the registry at a time in this process: the config file is
/// read, edited and replaced whole, so two at once would lose one edit.
pub(crate) static CONFIG_LOCK: Mutex<()> = Mutex::new(());

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
}

impl ConfigEntry {
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

pub(crate) fn read_config(path: &Path) -> Result<DocumentMut, InitError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(failed(format!("could not read {}: {e}", path.display()))),
    };
    text.parse::<DocumentMut>().map_err(|e| {
        invalid(format!(
            "{} is not valid TOML, so no project is added to it: {e}",
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
        return Err(invalid(format!(
            "`project` in {} is not a list of `[[project]]` tables",
            path.display()
        )));
    };
    let text = |t: &Table, key: &str| t.get(key).and_then(Item::as_str).map(str::to_string);
    Ok(blocks
        .iter()
        .filter_map(|t| {
            let root = text(t, "root")?;
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
                name: text(t, "name"),
                root,
                store: text(t, "store"),
                tracker,
            })
        })
        .collect())
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
    let root = canonical_root(&req.path)?;
    if req.by.trim().is_empty() {
        return Err(invalid("`by` is empty"));
    }
    if let Some(p) = &req.purpose {
        checked_purpose(p)?;
    }
    if let Some(n) = &req.display_name {
        checked_display_name(n)?;
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
    let slug = match (&req.slug, here, &req.display_name) {
        (Some(s), _, _) => checked_slug(s)?,
        (None, Some(e), _) => checked_slug(&e.slug().unwrap_or_default())?,
        (None, None, Some(n)) => slug_from_display(n)?,
        (None, None, None) => derive_slug(&root)?,
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
    let (tracker, adds_tracker) = match (&github, existing) {
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
        store,
        tracker,
        registers: here.is_none(),
        adds_tracker,
    })
}

// --- the work ---------------------------------------------------------------------------------

fn ensure_git(root: &Path, by: &str) -> Result<GitOutcome, InitError> {
    let git = |e: fl_exec::population::ExecError| failed(e.to_string());
    if let Ok(head) = Git::head(root) {
        return Ok(GitOutcome {
            initialized: false,
            initial_commit: false,
            head,
        });
    }
    let initialized = match std::fs::symlink_metadata(root.join(".git")) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Git::init(root).map_err(git)?;
            true
        }
        Err(e) => return Err(failed(format!("could not look for .git in the root: {e}"))),
    };
    let head = Git::commit_empty(root, INITIAL_COMMIT_MESSAGE, by).map_err(git)?;
    Ok(GitOutcome {
        initialized,
        initial_commit: true,
        head,
    })
}

/// Puts the project in its store (once) and gives it the default routing map (if it has none).
/// Returns the project's IRI and the areas its map declares.
fn seed_project_store(store: &AnyStore, r: &Resolved) -> Result<(String, Vec<String>), InitError> {
    let st = |e: fl_core::store::StoreError| failed(format!("store: {e}"));
    let root = r.root.to_string_lossy();
    let held = store.list_projects().map_err(st)?;
    let id = match held.as_slice() {
        [] => store.add_project(&root).map_err(st)?,
        [p] if same_root(&p.root, &r.root) => p.id.clone(),
        _ => {
            return Err(conflict(format!(
                "the store {} already holds {}, not {}. A project's store holds only that \
                 project: choose another name with --name or --slug",
                r.store.display(),
                held.iter()
                    .map(|p| p.root.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                r.root.display(),
            )));
        }
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

/// A directory-backed project onto a virtual group's slug is refused.
fn check_not_a_virtual_group(ws: &AnyStore, r: &Resolved) -> Result<(), InitError> {
    if !r.registers {
        return Ok(());
    }
    let area = ws
        .get_area(&r.slug)
        .map_err(|e| failed(format!("workspace store: {e}")))?;
    match area {
        Some(a) if a.is_virtual() => Err(virtual_slug_taken_by_group(&r.slug)),
        _ => Ok(()),
    }
}

/// Upserts the project's area in the workspace store (name, root as path, purpose). What the
/// request does not name stays as the area has it. An area that is already right is left alone.
/// Returns the area as it is.
fn upsert_project_area(
    ws: &AnyStore,
    r: &Resolved,
    req: &InitRequest,
) -> Result<AreaRecord, InitError> {
    let st = |e: fl_core::store::StoreError| failed(format!("workspace store: {e}"));
    let existing = ws.get_area(&r.slug).map_err(st)?;
    let name = match (&req.display_name, &existing) {
        (Some(n), _) => checked_display_name(n)?,
        (None, Some(a)) => a.name.clone(),
        (None, None) => r.slug.clone(),
    };
    let purpose = match (&req.purpose, &existing) {
        (Some(p), _) => checked_purpose(p)?,
        (None, Some(a)) => a.purpose.clone(),
        (None, None) => format!("Project {}", r.slug),
    };
    let path = Some(r.root.to_string_lossy().into_owned());
    if let Some(a) = &existing
        && a.name == name
        && a.purpose == purpose
        && a.path == path
    {
        return Ok(a.clone());
    }
    ws.upsert_area(AreaRecord {
        area_key: r.slug.clone(),
        name,
        path,
        purpose,
        updated_at: fl_exec::stamp::now(),
        by: req.by.trim().to_string(),
    })
    .map_err(st)
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
    let _one_at_a_time = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let resolved = resolve(req, paths)?;
    let workspace_store = open_workspace()?;
    check_not_a_virtual_group(&workspace_store, &resolved)?;

    std::fs::create_dir_all(&resolved.root)
        .map_err(|e| failed(format!("could not create {}: {e}", resolved.root.display())))?;
    let git = if req.git_init {
        Some(ensure_git(&resolved.root, &req.by)?)
    } else {
        None
    };

    if let Some(dir) = resolved
        .store
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        fl_store::private::create_private_dir_all(dir)
            .map_err(|e| failed(format!("could not create {}: {e}", dir.display())))?;
    }
    let project_store = open_project(&resolved)?;
    let (project, areas) = seed_project_store(&project_store, &resolved)?;
    let area = upsert_project_area(&workspace_store, &resolved, req)?;
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

/// Creates the virtual group `req` names: an area with no path in the workspace store, and
/// nothing else. No directory, no store file, no config block and no git. A group that exists
/// is kept; the request's name and purpose, when given, replace the group's.
pub fn create_virtual<W>(
    req: &VirtualRequest,
    paths: &InitPaths,
    open_workspace: impl FnOnce() -> Result<W, InitError>,
) -> Result<InitOutcome, InitError>
where
    W: Deref<Target = AnyStore>,
{
    let _one_at_a_time = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if req.by.trim().is_empty() {
        return Err(invalid("`by` is empty"));
    }
    let display = checked_display_name(&req.display_name)?;
    let slug = match &req.slug {
        Some(s) => checked_slug(s)?,
        None => slug_from_display(&display)?,
    };
    if RESERVED_GROUP_SLUGS.contains(&slug.as_str()) {
        return Err(invalid(format!(
            "`{slug}` cannot be a virtual group: `@all` is everyone and `fl` is the Hub's own \
             store. Choose another slug"
        )));
    }
    let purpose = req.purpose.as_deref().map(checked_purpose).transpose()?;

    // A slug names one kind. A directory-backed project holds it in the config or as a store.
    let doc = read_config(&paths.config_path)?;
    if let Some(e) = config_entries(&doc, &paths.config_path)?
        .iter()
        .find(|e| e.slug().as_deref() == Some(slug.as_str()))
    {
        return Err(conflict(format!(
            "`{slug}` is already a directory-backed project at {}, so it cannot also be a \
             virtual group. Choose another slug",
            e.root
        )));
    }
    for ext in ["fls", "redb"] {
        let store = paths.store_dir.join(format!("{slug}.{ext}"));
        if std::fs::symlink_metadata(&store).is_ok() {
            return Err(conflict(format!(
                "`{slug}` is already a project: its store {} exists. Choose another slug",
                store.display()
            )));
        }
    }

    let st = |e: fl_core::store::StoreError| failed(format!("workspace store: {e}"));
    let ws = open_workspace()?;
    let existing = ws.get_area(&slug).map_err(st)?;
    if let Some(a) = &existing
        && let Some(path) = &a.path
    {
        return Err(conflict(format!(
            "`{slug}` is already an area for the project at {path}, so it cannot also be a \
             virtual group. Choose another slug"
        )));
    }
    let name = display;
    let purpose = match (&purpose, &existing) {
        (Some(p), _) => p.clone(),
        (None, Some(a)) => a.purpose.clone(),
        (None, None) => format!("Virtual group {slug}"),
    };
    if let Some(a) = &existing
        && a.name == name
        && a.purpose == purpose
    {
        return Ok(virtual_outcome(a.clone(), false));
    }
    let registered = existing.is_none();
    let saved = ws
        .upsert_area(AreaRecord {
            area_key: slug,
            name,
            path: None,
            purpose,
            updated_at: fl_exec::stamp::now(),
            by: req.by.trim().to_string(),
        })
        .map_err(st)?;
    Ok(virtual_outcome(saved, registered))
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
    let base = match home {
        Some(h) => h.join(".config"),
        None => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?,
    };
    Some(base.join("fl").join("config.toml"))
}
