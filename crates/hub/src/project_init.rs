//! First-class project initialization: `fl project init <path>` and
//! `POST /api/v1/projects` run this one flow.
//!
//! One call makes a directory a registered project: a git working tree with a `HEAD`, a
//! `[[project]]` block in the user's config, a per-project store holding the project and the
//! default local routing map, the project's area in the workspace store, and a starter
//! `.fl/mcp.toml`. Running it again on the same project changes nothing that is already right.
//!
//! The flow lives here, below both the CLI and the Hub, because both need the whole of it: a
//! second copy would drift. The caller supplies the three places that differ between a CLI
//! process and the Hub (the config file, the directory of per-project stores, the workspace
//! store) and a function that opens the two stores once the project's name is known.
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

use crate::HubState;
use crate::api::{HttpRequest, HttpResponse, hub_home_dir};
use crate::auth::HubAuthority;

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

/// One initialization at a time in this process: the config file is read, edited and replaced
/// whole, so two at once would lose one edit.
static INIT_LOCK: Mutex<()> = Mutex::new(());

/// Why an initialization was refused or failed, by who is at fault.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// The request is wrong: a path, name, repository or credential that cannot be used.
    #[error("{0}")]
    Invalid(String),
    /// The request is well formed but clashes with what exists: another project, another
    /// tracker, another store. Nothing was changed for it.
    #[error("{0}")]
    Conflict(String),
    /// The machine could not do it: git, a store or a file failed.
    #[error("{0}")]
    Failed(String),
}

fn invalid(msg: impl Into<String>) -> InitError {
    InitError::Invalid(msg.into())
}

fn conflict(msg: impl Into<String>) -> InitError {
    InitError::Conflict(msg.into())
}

fn failed(msg: impl Into<String>) -> InitError {
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

/// A project's tracker binding when it is not the local store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tracker {
    pub github: String,
    pub credential: Credential,
}

/// What to initialize.
#[derive(Debug, Clone)]
pub struct InitRequest {
    /// The project root: an absolute path, made if it does not exist.
    pub path: PathBuf,
    /// The project's name; without it, the root's lowercase directory name, or the name the
    /// config already gives that root.
    pub name: Option<String>,
    /// `owner/repo`: bind the project's tracker to this GitHub repository.
    pub github: Option<String>,
    /// The tracker's credential source; `app` when `github` is given and this is not.
    pub credential: Option<Credential>,
    /// What the project is for, as the workspace area's purpose.
    pub purpose: Option<String>,
    /// Who is initializing, for the initial commit and the area.
    pub by: String,
}

/// The three places that differ between callers.
#[derive(Debug, Clone)]
pub struct InitPaths {
    /// The user config file, `config.toml`. It need not exist.
    pub config_path: PathBuf,
    /// The directory a new project's store goes in, as `<name>.fls`.
    pub store_dir: PathBuf,
    /// The workspace store, which a project's name must not name.
    pub workspace_store: PathBuf,
}

/// A request checked against the config, with nothing changed yet.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The project's name, a valid slug.
    pub slug: String,
    /// The canonical root.
    pub root: PathBuf,
    /// The project's store, as the config names it (an existing entry keeps its store).
    pub store: PathBuf,
    /// The tracker the project has, or will have, if any.
    pub tracker: Option<Tracker>,
    /// Whether the config has no block for this project yet.
    registers: bool,
    /// Whether a tracker is to be added to the block the config already has.
    adds_tracker: bool,
}

/// What happened to the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitOutcome {
    /// `git init` ran.
    pub initialized: bool,
    /// The initial empty commit was made.
    pub initial_commit: bool,
    /// `HEAD` afterwards.
    pub head: String,
}

/// The created project, as `fl project init --json` prints it and `POST /api/v1/projects`
/// returns it.
#[derive(Debug, Clone, Serialize)]
pub struct InitOutcome {
    /// The project's name.
    pub name: String,
    pub root: String,
    pub store: String,
    /// The project's IRI in its store.
    pub project: String,
    pub purpose: String,
    pub tracker: Option<Tracker>,
    /// The areas the project's routing map declares.
    pub areas: Vec<String>,
    pub git: GitOutcome,
    /// `.fl/mcp.toml` was made (it was absent).
    pub mcp_catalog_created: bool,
    /// The config file the project is registered in.
    pub config: String,
    /// The config had no block for the project and now has one.
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

// --- the config -------------------------------------------------------------------------------

/// One `[[project]]` block, as far as initialization reads it.
#[derive(Debug, Clone)]
struct ConfigEntry {
    name: Option<String>,
    root: String,
    store: Option<String>,
    tracker: Option<(String, String)>,
}

impl ConfigEntry {
    /// The name the config gives the project: `name`, else the root's directory name, in
    /// lowercase (`Entry::matches_slug`).
    fn slug(&self) -> Option<String> {
        match &self.name {
            Some(n) => Some(n.to_ascii_lowercase()),
            None => Path::new(&self.root)
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase),
        }
    }
}

fn read_config(path: &Path) -> Result<DocumentMut, InitError> {
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

fn config_entries(doc: &DocumentMut, path: &Path) -> Result<Vec<ConfigEntry>, InitError> {
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
fn same_root(a: &str, b: &Path) -> bool {
    let a = Path::new(a);
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
}

/// Replaces `path` whole by a rename of a sibling temporary file, keeping the old file's
/// permissions. A symbolic link is written through, so a config kept elsewhere stays a link.
fn write_atomic(path: &Path, text: &str) -> Result<(), InitError> {
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

/// The text of the project's block: `[[project]]`, and `[project.tracker]` when it has one.
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

fn derive_slug(root: &Path) -> Result<String, InitError> {
    let base = root
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    validate_slug(&base).map_err(|_| {
        invalid(format!(
            "cannot derive a project name from `{base}`: a name is 1 to 64 of lowercase letters, \
             digits, `-` and `_`, starting with a letter. Pass one with --name"
        ))
    })
}

fn checked_name(raw: &str) -> Result<String, InitError> {
    validate_slug(raw.trim()).map_err(|_| {
        invalid(format!(
            "`{raw}` is not a project name: a name is 1 to 64 of lowercase letters, digits, \
             `-` and `_`, starting with a letter"
        ))
    })
}

/// Checks `req` against the config and the places in `paths`. Changes nothing.
pub fn resolve(req: &InitRequest, paths: &InitPaths) -> Result<Resolved, InitError> {
    let root = canonical_root(&req.path)?;
    if req.by.trim().is_empty() {
        return Err(invalid("`by` is empty"));
    }
    if let Some(p) = &req.purpose {
        validate_summary(p).map_err(|e| invalid(format!("purpose: {e}")))?;
    }
    let github = match req.github.as_deref().map(str::trim) {
        Some(g) if fl_github::is_owner_repo(g) => Some(g.to_string()),
        Some(g) => {
            return Err(invalid(format!(
                "`{g}` is not a GitHub repository: name it as `owner/repo`"
            )));
        }
        None => None,
    };
    if github.is_none() && req.credential.is_some() {
        return Err(invalid(
            "a credential source names how to reach a GitHub repository: pass `github` too",
        ));
    }

    let doc = read_config(&paths.config_path)?;
    let entries = config_entries(&doc, &paths.config_path)?;
    let here = entries.iter().find(|e| same_root(&e.root, &root));
    let slug = match (&req.name, here) {
        (Some(n), _) => checked_name(n)?,
        (None, Some(e)) => checked_name(&e.slug().unwrap_or_default())?,
        (None, None) => derive_slug(&root)?,
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
             --name",
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
             with --name",
            paths.workspace_store.display()
        )));
    }
    let store_dir = paths.store_dir.canonicalize().unwrap_or_else(|_| {
        paths
            .store_dir
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .map(|p| p.join(paths.store_dir.file_name().unwrap_or_default()))
            .unwrap_or_else(|| paths.store_dir.clone())
    });
    if root.starts_with(&store_dir) || store_dir.starts_with(&root) {
        return Err(invalid(format!(
            "{} and the stores' directory {} contain one another, so it cannot be a project root",
            root.display(),
            store_dir.display()
        )));
    }

    let existing = here.and_then(|e| e.tracker.as_ref());
    let (tracker, adds_tracker) = match (&github, existing) {
        (Some(g), Some((eg, ec))) => {
            let wanted = req.credential.map(Credential::as_str);
            if !g.eq_ignore_ascii_case(eg) || wanted.is_some_and(|w| w != ec) {
                return Err(conflict(format!(
                    "project `{slug}` is already bound to GitHub `{eg}` with credential `{ec}`; \
                     fl will not rebind it. Edit {} to change the binding",
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
                 project: choose another name with --name",
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

/// Upserts the project's area in the workspace store, keeping the purpose an existing area has
/// when the request names none.
fn upsert_workspace_area(
    ws: &AnyStore,
    r: &Resolved,
    req: &InitRequest,
) -> Result<String, InitError> {
    let st = |e: fl_core::store::StoreError| failed(format!("workspace store: {e}"));
    let purpose = match (&req.purpose, ws.get_area(&r.slug).map_err(st)?) {
        (Some(p), _) => p.clone(),
        (None, Some(existing)) => existing.purpose,
        (None, None) => format!("Project {}", r.slug),
    };
    let area = AreaRecord::new(
        &r.slug,
        Some(r.root.to_string_lossy().into_owned()),
        &purpose,
        fl_exec::stamp::now(),
        &req.by,
    )
    .map_err(|e| invalid(e.to_string()))?;
    Ok(ws.upsert_area(area).map_err(st)?.purpose)
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

/// Initializes the project `req` names.
///
/// `open` opens the project's store and the workspace store once the name and the store path
/// are known; the caller picks how (a CLI process opens them directly, the Hub through its
/// locks). Order: nothing is changed until the request is checked; the config block is written
/// last, so a failure before it leaves the project unregistered and a re-run finishes the job.
pub fn init_project<P, W>(
    req: &InitRequest,
    paths: &InitPaths,
    open: impl FnOnce(&Resolved) -> Result<(P, W), InitError>,
) -> Result<InitOutcome, InitError>
where
    P: Deref<Target = AnyStore>,
    W: Deref<Target = AnyStore>,
{
    let _one_at_a_time = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let resolved = resolve(req, paths)?;

    std::fs::create_dir_all(&resolved.root)
        .map_err(|e| failed(format!("could not create {}: {e}", resolved.root.display())))?;
    let git = ensure_git(&resolved.root, &req.by)?;

    if let Some(dir) = resolved
        .store
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        fl_store::private::create_private_dir_all(dir)
            .map_err(|e| failed(format!("could not create {}: {e}", dir.display())))?;
    }
    let (project_store, workspace_store) = open(&resolved)?;
    let (project, areas) = seed_project_store(&project_store, &resolved)?;
    let purpose = upsert_workspace_area(&workspace_store, &resolved, req)?;
    drop(project_store);
    drop(workspace_store);

    let mcp_catalog_created = ensure_mcp_catalog(&resolved.root)?;
    upsert_config(paths, &resolved)?;

    Ok(InitOutcome {
        name: resolved.slug.clone(),
        root: resolved.root.display().to_string(),
        store: resolved.store.display().to_string(),
        project,
        purpose,
        tracker: resolved.tracker.clone(),
        areas,
        git,
        mcp_catalog_created,
        config: paths.config_path.display().to_string(),
        registered: resolved.registers,
    })
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

// --- POST /api/v1/projects --------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProjectBody {
    path: String,
    name: Option<String>,
    github: Option<String>,
    credential: Option<String>,
    purpose: Option<String>,
}

fn error_response(status: u16, message: &str) -> HttpResponse {
    HttpResponse::json(status, serde_json::json!({ "error": message }))
}

fn init_error_response(e: &InitError) -> HttpResponse {
    let status = match e {
        InitError::Invalid(_) => 400,
        InitError::Conflict(_) => 409,
        InitError::Failed(_) => 500,
    };
    error_response(status, &e.to_string())
}

/// `POST /api/v1/projects`. Owner only: it writes the Owner's config and may `git init` the
/// path. Runs the same initialization as `fl project init` (see the module), in the Hub's
/// home: the config is `<home>/.config/fl/config.toml` and the project's store is
/// `<home>/.local/share/fl/<name>.fls`, where `GET /api/v1/projects` finds it. `201` for a new
/// project, `200` when it was already initialized.
pub(crate) fn handle_post_projects(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return error_response(403, "only Owner authority may create projects");
    }
    let body: CreateProjectBody = match req.json() {
        Ok(body) => body,
        Err(e) => return error_response(400, &e),
    };
    let credential = match body.credential.as_deref().map(str::parse::<Credential>) {
        Some(Ok(c)) => Some(c),
        Some(Err(e)) => return error_response(400, &e),
        None => None,
    };
    let Some(home) = hub_home_dir(state) else {
        return error_response(500, "the Hub has no home directory");
    };
    let Some(config_path) = user_config_path(state.config.home_override.as_deref()) else {
        return error_response(500, "the Hub has no config directory");
    };
    let path = PathBuf::from(&body.path);
    // Before anything is made: the Hub refuses a root in a system directory, a secret's
    // directory, or the home directory itself, as it does for an artifacts directory.
    match canonical_root(&path) {
        Ok(root) => {
            if let Some(reason) = crate::artifact_dirs::sensitive_reason(state, &root) {
                return error_response(400, &reason);
            }
        }
        Err(e) => return init_error_response(&e),
    }
    let request = InitRequest {
        path,
        name: body.name,
        github: body.github,
        credential,
        purpose: body.purpose,
        by: authority.actor_name().to_string(),
    };
    let paths = InitPaths {
        config_path,
        store_dir: crate::records::projects_dir(&home),
        workspace_store: std::path::absolute(&state.config.store_path)
            .unwrap_or_else(|_| state.config.store_path.clone()),
    };
    let outcome = init_project(&request, &paths, |r| {
        let project = state.open_project_store_at(&r.store).map_err(|e| match e {
            crate::ProjectOpenError::Busy => failed("the project's store is busy; retry"),
            crate::ProjectOpenError::Store(e) => failed(format!("project store: {e}")),
        })?;
        let workspace = state
            .open_store()
            .map_err(|e| failed(format!("workspace store: {e}")))?;
        Ok((project, workspace))
    });
    match outcome {
        Ok(outcome) => {
            let mut descriptor = crate::artifact_dirs::project_descriptor(state, &outcome.name);
            if let (Some(map), Ok(serde_json::Value::Object(created))) =
                (descriptor.as_object_mut(), serde_json::to_value(&outcome))
            {
                map.extend(created);
            }
            state.broadcast("projects", "project.created", descriptor.clone());
            HttpResponse::json(if outcome.registered { 201 } else { 200 }, descriptor)
        }
        Err(e) => init_error_response(&e),
    }
}
