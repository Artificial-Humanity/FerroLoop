//! Deleting a project: what `fl project delete` and `DELETE /api/v1/projects/:slug` run.
//!
//! The order is fixed: validate everything (the agents, the reassignment target, the safety of
//! the directory and the store files), then reassign or decommission the active agents, then
//! unregister (the `[[project]]` block and the area), then delete the store files, then delete
//! the directory. A refusal in the first step changes nothing. The last two steps never undo
//! the unregistration: a failure there is reported in the outcome (`error`, with the flag that
//! did not happen left `false`).
//!
//! By default nothing on disk is deleted: only the config block, the area and the workspace
//! store's own row for the project go. A project's store file is still in the Hub's data
//! directory then, and the Hub lists a project from a store file there, so the Hub keeps
//! listing the slug (with no path) until `delete_store` removes the file as well.
//!
//! The directory checks run twice: before anything changes, and again right before the
//! directory is removed. Between the second run and `remove_dir_all` there is a window no
//! check closes: if an ancestor of the directory is swapped for a link in that window, the
//! removal follows the link. `fl` does not hold a handle on the directory to prevent that, so a
//! directory that another process can rename while it is deleted is not safe to delete with
//! this flow. The config lock that serializes edits is held in this process only: a second
//! `fl` process or an editor can still change the config file while a flow runs.

use std::ops::Deref;
use std::path::{Component, Path, PathBuf};

use fl_core::agent::{AgentRecord, AgentRole, AreaRecord, RegisterAgentArgs};
use fl_core::store::{AgentRegistry, Catalog, check_project_removable};
use fl_store::AnyStore;
use serde::{Deserialize, Serialize};
use toml_edit::DocumentMut;

use crate::lifecycle::{self, LifecycleEnv, SessionTarget};
use crate::project_init::{
    CONFIG_LOCK, ConfigEntry, InitError, InitPaths, ProjectKind, checked_slug, config_entries,
    conflict, failed, invalid, read_config, remove_blocks, same_root, write_atomic,
};
use crate::project_list::{ProjectSummary, StoreHint, agent_in_project, held_slug, list_projects};

/// The extension of a FerroStore file.
const FLS: &str = "fls";

/// The fewest path components a directory that is deleted may have (`/a/b/c` has three).
pub const MIN_DELETE_DEPTH: usize = 3;

/// What to delete besides the registration. Every field defaults, so `{}` is a request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeleteRequest {
    /// Move the project's active agents to this project (it must exist and differ).
    pub reassign_to: Option<String>,
    /// Retire the project's active agents (tear their sessions down).
    pub decommission_agents: bool,
    /// Also delete the project's store file and its `.sock` and `.migrate.lock` siblings.
    pub delete_store: bool,
    /// Also delete the project's directory, after strict safety checks.
    pub delete_dir: bool,
}

/// What a deletion did. `error` is present only when the store files or the directory could not
/// be deleted after the project was unregistered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeleteOutcome {
    pub deleted: String,
    pub reassigned_agents: usize,
    pub decommissioned_agents: usize,
    pub store_deleted: bool,
    pub dir_deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Why a deletion was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    #[error(transparent)]
    Init(#[from] InitError),
    /// Active agents are assigned to the project and the request says nothing about them.
    #[error("project has active agents")]
    ActiveAgents { agents: Vec<String> },
}

impl DeleteError {
    /// The `409` body for active agents: `{"error", "active_agent_count", "active_agents"}`.
    pub fn active_agents_body(agents: &[String]) -> serde_json::Value {
        serde_json::json!({
            "error": "project has active agents",
            "active_agent_count": agents.len(),
            "active_agents": agents,
        })
    }
}

/// The places and process facts a deletion checks a directory against.
pub struct DeleteContext<'a> {
    pub paths: &'a InitPaths,
    /// Per-project store files the caller knows of beyond the config (the Hub's data directory).
    pub stores: &'a [StoreHint],
    /// The home directory of whoever runs this (the Hub's home, or `$HOME`). `$HOME` of the
    /// process is checked too.
    pub home: Option<&'a Path>,
    /// Every workspace root known (see [`locate_workspace_roots`]); none of them may be inside
    /// the directory that is deleted. With none known (and none above the directory), the
    /// directory is not deleted: the check cannot tell whether it holds the workspace.
    pub workspace_roots: &'a [PathBuf],
    pub lifecycle: &'a LifecycleEnv<'a>,
}

/// `path` with its longest existing prefix resolved through links and the rest appended as
/// written, so a location that does not exist yet still compares with one that does.
fn resolve_loose(path: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(real) = cur.canonicalize() {
            let mut out = real;
            out.extend(tail.iter().rev());
            return out;
        }
        match cur.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                cur.pop();
            }
            None => return path.to_path_buf(),
        }
    }
}

/// Under test, nothing outside a directory made inside the system temporary directory is ever
/// deleted, whatever the checks above say: a test of a refusal that fails must not take a real
/// directory with it.
#[cfg(test)]
fn assert_deletable_in_test(path: &Path) {
    let tmp = std::env::temp_dir().canonicalize().unwrap();
    let real = resolve_loose(path);
    assert!(
        real != tmp && real.starts_with(&tmp),
        "a test would delete {}, outside the temporary directory",
        real.display()
    );
}

/// What a test can make go wrong or change at a given step of a deletion. The flow's own code
/// reaches none of these outside tests.
#[cfg(test)]
pub(crate) mod hooks {
    use std::cell::RefCell;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Fault {
        /// `delete_area` fails.
        DeleteArea,
        /// Writing the edited config fails.
        WriteConfig,
        /// `remove_dir_all` fails.
        RemoveDir,
    }

    #[derive(Default)]
    pub(crate) struct Hooks {
        pub(crate) fault: Option<Fault>,
        pub(crate) after_unregister: Option<Box<dyn FnOnce()>>,
    }

    thread_local! {
        pub(crate) static HOOKS: RefCell<Hooks> = RefCell::new(Hooks::default());
    }

    pub(crate) fn fails(which: Fault) -> bool {
        HOOKS.with(|h| h.borrow().fault == Some(which))
    }

    pub(crate) fn run_after_unregister() {
        let hook = HOOKS.with(|h| h.borrow_mut().after_unregister.take());
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn inject(fault: Fault) {
        HOOKS.with(|h| h.borrow_mut().fault = Some(fault));
    }

    pub(crate) fn after_unregister(f: impl FnOnce() + 'static) {
        HOOKS.with(|h| h.borrow_mut().after_unregister = Some(Box::new(f)));
    }

    pub(crate) fn reset() {
        HOOKS.with(|h| *h.borrow_mut() = Hooks::default());
    }
}

/// How far below the home directory [`locate_workspace_roots`] looks for a workspace.
const HOME_SCAN_DEPTH: usize = 2;
/// The most entries of one directory [`locate_workspace_roots`] reads.
const HOME_SCAN_ENTRIES: usize = 400;

/// The workspace roots near the places a deletion runs from, for [`DeleteContext`]:
///
/// - the first root at or above each of `starts` (the current directory, the Hub's repository),
/// - for each of `homes`: the first root at or above it, and every root up to two levels below
///   it (a workspace kept in `~/Projects/<name>`; hidden directories and links are not
///   entered),
/// - the first root at or above the directory of `exe` (the running `fl`).
///
/// Canonical, without duplicates. Empty when there is none anywhere: then no directory is
/// deleted, because nothing says where the workspace is.
pub fn locate_workspace_roots(
    starts: &[&Path],
    homes: &[&Path],
    exe: Option<&Path>,
) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut add = |root: PathBuf| {
        let root = resolve_loose(&root);
        if !found.contains(&root) {
            found.push(root);
        }
    };
    for start in starts {
        if let Some(root) = fl_core::find_workspace_root(start) {
            add(root);
        }
    }
    if let Some(exe) = exe.and_then(Path::parent)
        && let Some(root) = fl_core::find_workspace_root(exe)
    {
        add(root);
    }
    for home in homes.iter().filter(|h| !h.as_os_str().is_empty()) {
        if let Some(root) = fl_core::find_workspace_root(home) {
            add(root);
        }
        let mut level = vec![home.to_path_buf()];
        for _ in 0..HOME_SCAN_DEPTH {
            let mut next = Vec::new();
            for dir in &level {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.take(HOME_SCAN_ENTRIES).flatten() {
                    let hidden = entry.file_name().to_string_lossy().starts_with('.');
                    if hidden || !entry.file_type().is_ok_and(|t| t.is_dir()) {
                        continue;
                    }
                    let path = entry.path();
                    if fl_core::is_workspace_root(&path) {
                        add(path.clone());
                    }
                    next.push(path);
                }
            }
            level = next;
        }
    }
    found
}

/// The home directories a directory is checked against: the caller's, and `$HOME`. An error
/// when neither is known: the check that a directory is not the home directory or above it
/// cannot run then, so the directory is not deleted.
pub(crate) fn home_dirs(
    ctx_home: Option<&Path>,
    env_home: Option<PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    let homes: Vec<PathBuf> = [ctx_home.map(Path::to_path_buf), env_home]
        .into_iter()
        .flatten()
        .filter(|h| !h.as_os_str().is_empty())
        .collect();
    if homes.is_empty() {
        return Err(
            "the home directory is unknown ($HOME is not set and none was given), so it cannot              be checked that the directory is not the home directory or above it"
                .into(),
        );
    }
    Ok(homes)
}

fn depth(path: &Path) -> usize {
    path.components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count()
}

struct DirCheck<'a> {
    slug: &'a str,
    current: &'a ProjectSummary,
    area: Option<&'a AreaRecord>,
    entries: &'a [ConfigEntry],
    listed: &'a [ProjectSummary],
    held: &'a [fl_core::model::Project],
    ctx: &'a DeleteContext<'a>,
    /// `$HOME` of this process.
    env_home: Option<PathBuf>,
}

impl DirCheck<'_> {
    /// The canonical directory to delete, or why it is not safe to. Reads only.
    fn validate(&self) -> Result<PathBuf, InitError> {
        let slug = self.slug;
        let refuse = |why: String| {
            invalid(format!(
                "refusing to delete the directory of `{slug}`: {why}. Nothing was changed"
            ))
        };
        let registered = self.current.path.as_deref().unwrap_or("");
        if registered.trim().is_empty() {
            return Err(refuse("the project has no registered directory".into()));
        }
        let raw = Path::new(registered);
        if !raw.is_absolute() {
            return Err(refuse(format!(
                "the registered path `{registered}` is not absolute"
            )));
        }
        if raw.components().any(|c| c == Component::ParentDir) {
            return Err(refuse(format!(
                "the registered path `{registered}` has a `..` component"
            )));
        }
        // A symbolic link is not followed: it could point anywhere.
        let meta = std::fs::symlink_metadata(raw)
            .map_err(|e| refuse(format!("`{registered}` cannot be read: {e}")))?;
        if meta.file_type().is_symlink() {
            return Err(refuse(format!("`{registered}` is a symbolic link")));
        }
        if !meta.is_dir() {
            return Err(refuse(format!("`{registered}` is not a directory")));
        }
        let canon = raw
            .canonicalize()
            .map_err(|e| refuse(format!("`{registered}` cannot be resolved: {e}")))?;
        if canon.as_os_str().is_empty() || canon == Path::new("/") {
            return Err(refuse("it is the filesystem root".into()));
        }
        // What was registered is what is deleted: a path with a link in it (or a `.`/`..`
        // that moves it) would name one directory in the registration and delete another.
        if canon != raw {
            return Err(refuse(format!(
                "the registered path `{registered}` is not its own canonical path `{}` (a                  symbolic link in it, or a `.` or `..` component)",
                canon.display()
            )));
        }
        if depth(&canon) < MIN_DELETE_DEPTH {
            return Err(refuse(format!(
                "`{}` is shallower than {MIN_DELETE_DEPTH} path components",
                canon.display()
            )));
        }

        // Every record of where this project lives must name this directory.
        let agrees = |other: &str| same_root(other, &canon);
        if let Some(path) = self.area.and_then(|a| a.path.as_deref())
            && !agrees(path)
        {
            return Err(refuse(format!(
                "the area records `{path}` but the project is registered at `{registered}`; the \
                 path no longer matches its registration"
            )));
        }
        for e in self
            .entries
            .iter()
            .filter(|e| e.slug().as_deref() == Some(slug))
        {
            if !agrees(&e.root) {
                return Err(refuse(format!(
                    "the config registers `{}` but the project resolves to `{registered}`",
                    e.root
                )));
            }
        }
        for p in self
            .held
            .iter()
            .filter(|p| held_slug(&p.root).as_deref() == Some(slug))
        {
            if !agrees(&p.root) {
                return Err(refuse(format!(
                    "the workspace store holds `{}` for this project, not `{registered}`",
                    p.root
                )));
            }
        }

        // Not the home directory or one above it. With no home known the check cannot run.
        for home in home_dirs(self.ctx.home, self.env_home.clone()).map_err(refuse)? {
            if resolve_loose(&home).starts_with(&canon) {
                return Err(refuse(format!(
                    "it is the home directory `{}` or contains it",
                    home.display()
                )));
            }
        }
        // Not a workspace root, not one above a workspace root. The directory itself is looked
        // at first: its own markers do not depend on where the caller stands.
        if fl_core::is_workspace_root(&canon) {
            return Err(refuse(
                "it is a workspace root (it holds `.agents/memory` or `Notes`)".into(),
            ));
        }
        for root in self.ctx.workspace_roots {
            let root = resolve_loose(root);
            if root.starts_with(&canon) {
                return Err(refuse(format!(
                    "it is the workspace root `{}` or contains it",
                    root.display()
                )));
            }
        }
        // With no workspace root known anywhere, nothing says whether the directory holds the
        // workspace: fail closed.
        if self.ctx.workspace_roots.is_empty() && fl_core::find_workspace_root(&canon).is_none() {
            return Err(refuse(
                "no workspace root could be found (no `.agents/memory` or `Notes` directory at                  or above the current directory, at or above or just below the home                  directory, or at or above the `fl` executable), so it cannot be told whether                  the directory holds the workspace"
                    .into(),
            ));
        }
        // Not holding another registered project.
        let others = self
            .entries
            .iter()
            .filter(|e| e.slug().as_deref() != Some(slug))
            .map(|e| e.root.clone())
            .chain(
                self.listed
                    .iter()
                    .filter(|p| p.slug != slug)
                    .filter_map(|p| p.path.clone()),
            )
            .chain(
                self.held
                    .iter()
                    .filter(|p| held_slug(&p.root).as_deref() != Some(slug))
                    .map(|p| p.root.clone()),
            );
        for other in others {
            let other = Path::new(&other);
            if other.starts_with(&canon) || resolve_loose(other).starts_with(&canon) {
                return Err(refuse(format!(
                    "it contains another registered project, `{}`",
                    other.display()
                )));
            }
        }
        // Not holding, or held by, a store or config location.
        let paths = self.ctx.paths;
        let dirs = [
            Some(paths.store_dir.clone()),
            paths.config_path.parent().map(Path::to_path_buf),
        ];
        for dir in dirs.into_iter().flatten() {
            let dir = resolve_loose(&dir);
            if dir.starts_with(&canon) || canon.starts_with(&dir) {
                return Err(refuse(format!(
                    "it contains or lies inside the store or config directory `{}`",
                    dir.display()
                )));
            }
        }
        let files = std::iter::once(paths.workspace_store.clone())
            .chain(std::iter::once(paths.config_path.clone()))
            .chain(
                self.entries
                    .iter()
                    .filter_map(|e| e.store.as_deref().map(PathBuf::from)),
            )
            .chain(
                self.listed
                    .iter()
                    .filter_map(|p| p.store_path.as_deref().map(PathBuf::from)),
            )
            .chain(self.ctx.stores.iter().filter_map(|(_, f)| f.clone()));
        for file in files {
            if resolve_loose(&file).starts_with(&canon) {
                return Err(refuse(format!(
                    "it contains the store or config file `{}`",
                    file.display()
                )));
            }
        }
        Ok(canon)
    }
}

/// The store files to delete: the file as the project names it (and its `.fls` form), with the
/// `.sock` and `.migrate.lock` siblings of an `.fls`. Only what exists.
fn store_files(store: &Path) -> Vec<PathBuf> {
    let mut main = vec![store.to_path_buf()];
    let fls = store.with_extension(FLS);
    if fls != store {
        main.push(fls);
    }
    let mut out = Vec::new();
    for file in main {
        let is_fls = file.extension().and_then(|e| e.to_str()) == Some(FLS);
        let exists = std::fs::symlink_metadata(&file).is_ok();
        if exists {
            out.push(file.clone());
        }
        if is_fls {
            for sibling in [
                fl_store::default_socket_path_for_store(&file),
                fl_store::MigrationLock::path_for(&file),
            ] {
                if std::fs::symlink_metadata(&sibling).is_ok() {
                    out.push(sibling);
                }
            }
        }
    }
    out
}

/// Refuses a store that is not this project's alone: one that is not a store file by its name
/// (`.fls`, or the older `.redb`), the workspace store, another project's store, a link or a
/// directory; and one that is neither in the stores' directory nor the store this project
/// itself registers.
fn check_store(
    slug: &str,
    store: &Path,
    entries: &[ConfigEntry],
    listed: &[ProjectSummary],
    paths: &InitPaths,
) -> Result<Vec<PathBuf>, InitError> {
    let refuse = |why: String| {
        invalid(format!(
            "refusing to delete the store of `{slug}`: {why}. Nothing was changed"
        ))
    };
    let extension = store.extension().and_then(|e| e.to_str());
    if !matches!(extension, Some(FLS | "redb")) {
        return Err(refuse(format!(
            "`{}` is not named like a store file (`.fls`)",
            store.display()
        )));
    }
    let registered_here: Vec<PathBuf> = entries
        .iter()
        .filter(|e| e.slug().as_deref() == Some(slug))
        .filter_map(|e| e.store.as_deref().map(PathBuf::from))
        .chain(
            listed
                .iter()
                .filter(|p| p.slug == slug)
                .filter_map(|p| p.store_path.as_deref().map(PathBuf::from)),
        )
        .map(|p| resolve_loose(&p))
        .collect();
    let store_dir = resolve_loose(&paths.store_dir);
    let files = store_files(store);
    let ws = resolve_loose(&paths.workspace_store);
    let ws_main = [ws.clone(), ws.with_extension(FLS)];
    let others: Vec<PathBuf> = entries
        .iter()
        .filter(|e| e.slug().as_deref() != Some(slug))
        .filter_map(|e| e.store.as_deref().map(PathBuf::from))
        .chain(
            listed
                .iter()
                .filter(|p| p.slug != slug)
                .filter_map(|p| p.store_path.as_deref().map(PathBuf::from)),
        )
        .map(|p| resolve_loose(&p))
        .collect();
    for file in &files {
        let meta = std::fs::symlink_metadata(file)
            .map_err(|e| refuse(format!("`{}` cannot be read: {e}", file.display())))?;
        // The sockets beside a store are not plain files; a link or a directory is never one
        // of its files.
        if meta.file_type().is_symlink() || meta.is_dir() {
            return Err(refuse(format!(
                "`{}` is a link or a directory, not a store file",
                file.display()
            )));
        }
        let real = resolve_loose(file);
        // The sockets and locks beside a store come from its name; only the store files
        // themselves are placed.
        let is_store_file = matches!(
            file.extension().and_then(|e| e.to_str()),
            Some(FLS | "redb")
        );
        let own = real.starts_with(&store_dir)
            || registered_here
                .iter()
                .any(|r| *r == real || r.with_extension(FLS) == real);
        if is_store_file && !own {
            return Err(refuse(format!(
                "`{}` is neither in the stores' directory `{}` nor the store this project                  registers",
                file.display(),
                store_dir.display()
            )));
        }
        if ws_main.contains(&real) {
            return Err(refuse(format!(
                "`{}` is the workspace store",
                file.display()
            )));
        }
        if others.contains(&real) {
            return Err(refuse(format!(
                "`{}` is another project's store",
                file.display()
            )));
        }
    }
    Ok(files)
}

/// The agents moved to `target`: `project` becomes the target when it named this project, and an
/// extra area of this project becomes the target.
fn moved(agent: &AgentRecord, target: &str, hit: &dyn Fn(&str) -> bool) -> (String, Vec<String>) {
    let project = if hit(&agent.project) {
        target.to_string()
    } else {
        agent.project.clone()
    };
    let mut extra: Vec<String> = agent
        .extra_areas
        .iter()
        .filter(|a| !hit(a))
        .cloned()
        .collect();
    if agent.extra_areas.iter().any(|a| hit(a)) && project != target {
        extra.push(target.to_string());
    }
    extra.retain(|a| *a != project);
    extra.sort();
    extra.dedup();
    (project, extra)
}

/// The store file a project keeps: the one it is listed with, else `<slug>.fls` in the stores'
/// directory.
fn store_of(slug: &str, current: &ProjectSummary, paths: &InitPaths) -> PathBuf {
    current
        .store_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.store_dir.join(format!("{slug}.fls")))
}

fn remove_dir(canon: &Path) -> Result<(), String> {
    #[cfg(test)]
    if hooks::fails(hooks::Fault::RemoveDir) {
        return Err(format!(
            "could not delete {}: injected failure",
            canon.display()
        ));
    }
    std::fs::remove_dir_all(canon).map_err(|e| format!("could not delete {}: {e}", canon.display()))
}

fn fault_delete_area() -> Result<(), fl_core::store::StoreError> {
    #[cfg(test)]
    if hooks::fails(hooks::Fault::DeleteArea) {
        return Err(fl_core::store::StoreError::InvalidState(
            "injected failure".into(),
        ));
    }
    Ok(())
}

fn fault_write_config() -> Result<(), InitError> {
    #[cfg(test)]
    if hooks::fails(hooks::Fault::WriteConfig) {
        return Err(failed("could not write the config: injected failure"));
    }
    Ok(())
}

/// Deletes the project `slug`, in the order the module describes. `clear_queue` is called with
/// each id a decommissioned session may be queued under.
///
/// Held for the whole call: the config lock, so no other config edit interleaves.
pub fn delete_project<W>(
    slug: &str,
    req: &DeleteRequest,
    by: &str,
    ctx: &DeleteContext<'_>,
    open_workspace: impl FnOnce() -> Result<W, InitError>,
    clear_queue: impl FnMut(&str),
) -> Result<DeleteOutcome, DeleteError>
where
    W: Deref<Target = AnyStore>,
{
    let env_home = std::env::var_os("HOME").map(PathBuf::from);
    delete_project_in(slug, req, by, ctx, env_home, open_workspace, clear_queue)
}

/// [`delete_project`] with `$HOME` passed in, so a test can name it without changing the
/// process's.
pub(crate) fn delete_project_in<W>(
    slug: &str,
    req: &DeleteRequest,
    by: &str,
    ctx: &DeleteContext<'_>,
    env_home: Option<PathBuf>,
    open_workspace: impl FnOnce() -> Result<W, InitError>,
    mut clear_queue: impl FnMut(&str),
) -> Result<DeleteOutcome, DeleteError>
where
    W: Deref<Target = AnyStore>,
{
    let _one_at_a_time = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let paths = ctx.paths;
    let slug = checked_slug(slug)?;
    if by.trim().is_empty() {
        return Err(invalid("`by` is empty").into());
    }
    if req.reassign_to.is_some() && req.decommission_agents {
        return Err(invalid("`reassign_to` and `decommission_agents` exclude each other").into());
    }
    let st = |e: fl_core::store::StoreError| failed(format!("workspace store: {e}"));

    // --- validate ------------------------------------------------------------------------
    let ws = open_workspace()?;
    let held = ws.list_projects().map_err(st)?;
    let listed = list_projects(&paths.config_path, ctx.stores, &held, &*ws)?;
    let Some(current) = listed.iter().find(|p| p.slug == slug) else {
        return Err(InitError::NotFound(format!("no project `{slug}`")).into());
    };
    if current.kind == ProjectKind::Virtual && (req.delete_store || req.delete_dir) {
        return Err(invalid(
            "a virtual group has no store and no directory: `delete_store` and `delete_dir` \
             do not apply",
        )
        .into());
    }
    let doc = read_config(&paths.config_path)?;
    let entries = config_entries(&doc, &paths.config_path)?;
    let area = ws.get_area(&slug).map_err(st)?;
    let area_name = area.as_ref().map(|a| a.name.clone());
    let hit = |name: &str| {
        name.trim().eq_ignore_ascii_case(&slug)
            || area_name
                .as_deref()
                .is_some_and(|n| name.trim().eq_ignore_ascii_case(n.trim()))
    };
    let everyone = ws.list_agents(None, false).map_err(st)?;
    let agents: Vec<AgentRecord> = everyone
        .iter()
        .filter(|a| agent_in_project(a, &slug, area_name.as_deref()))
        .cloned()
        .collect();
    if !agents.is_empty() && req.reassign_to.is_none() && !req.decommission_agents {
        return Err(DeleteError::ActiveAgents {
            agents: agents.iter().map(|a| a.display_name.clone()).collect(),
        });
    }

    let target: Option<&ProjectSummary> = match &req.reassign_to {
        Some(raw) => {
            let t = checked_slug(raw)?;
            if t == slug {
                return Err(invalid("`reassign_to` is the project being deleted").into());
            }
            let found = listed.iter().find(|p| p.slug == t);
            Some(found.ok_or_else(|| {
                InitError::NotFound(format!("no project `{t}` to reassign the agents to"))
            })?)
        }
        None => None,
    };
    if let Some(t) = target
        && let Some(lead) = agents.iter().find(|a| a.role == AgentRole::Lead)
        && let Some(sitting) = everyone.iter().find(|o| {
            o.project == t.slug
                && o.role == AgentRole::Lead
                && !agents.iter().any(|a| a.name_key == o.name_key)
        })
    {
        return Err(conflict(format!(
            "`{}` is the Lead of `{slug}` and `{}` already leads `{}`; retire one first",
            lead.display_name, sitting.display_name, t.slug
        ))
        .into());
    }

    // The workspace store's own rows for this project (`fl project add`, the Hub's records):
    // they go with it, or the list would keep it. A row that holds data stops the deletion.
    let held_ids: Vec<fl_core::ids::ProjectId> = held
        .iter()
        .filter(|p| held_slug(&p.root).as_deref() == Some(slug.as_str()))
        .map(|p| p.id.clone())
        .collect();
    for id in &held_ids {
        check_project_removable(&*ws, id).map_err(|e| {
            invalid(format!(
                "refusing to delete `{slug}`: the workspace store holds data for it ({e}). \
                 Nothing was changed"
            ))
        })?;
    }

    let dir_check = DirCheck {
        slug: &slug,
        current,
        area: area.as_ref(),
        entries: &entries,
        listed: &listed,
        held: &held,
        ctx,
        env_home,
    };
    let dir = if req.delete_dir {
        Some(dir_check.validate()?)
    } else {
        None
    };
    if req.delete_store {
        check_store(
            &slug,
            &store_of(&slug, current, paths),
            &entries,
            &listed,
            paths,
        )?;
    }

    // --- agents --------------------------------------------------------------------------
    let at = fl_exec::stamp::now();
    let mut outcome = DeleteOutcome {
        deleted: slug.clone(),
        reassigned_agents: 0,
        decommissioned_agents: 0,
        store_deleted: false,
        dir_deleted: false,
        error: None,
    };
    if let Some(t) = target {
        if ws.get_area(&t.slug).map_err(st)?.is_none() {
            ws.upsert_area(AreaRecord {
                area_key: t.slug.clone(),
                name: t.name.clone(),
                path: t.path.clone(),
                purpose: if t.purpose.is_empty() {
                    format!("Project {}", t.slug)
                } else {
                    t.purpose.clone()
                },
                updated_at: at.clone(),
                by: by.to_string(),
            })
            .map_err(st)?;
        }
        for agent in &agents {
            let (project, extra_areas) = moved(agent, &t.slug, &hit);
            ws.register_agent(RegisterAgentArgs {
                display_name: agent.display_name.clone(),
                email: Some(agent.email.clone()),
                project,
                role: agent.role.clone(),
                extra_areas,
                create_area: false,
                replace: true,
                at: at.clone(),
                by: by.to_string(),
            })
            .map_err(|e| {
                failed(format!(
                    "could not reassign {}: {e} ({} of {} agents were reassigned; the project is \
                     still registered)",
                    agent.display_name,
                    outcome.reassigned_agents,
                    agents.len()
                ))
            })?;
            outcome.reassigned_agents += 1;
        }
    } else if req.decommission_agents {
        for agent in &agents {
            let done = SessionTarget::resolve(&*ws, &agent.name_key, None).and_then(|target| {
                lifecycle::teardown_session(
                    &*ws,
                    ctx.lifecycle,
                    &target,
                    by,
                    lifecycle::Caller::Owner,
                    &mut clear_queue,
                )
            });
            done.map_err(|e| {
                failed(format!(
                    "could not decommission {}: {e} ({} of {} agents were decommissioned; the \
                     project is still registered)",
                    agent.display_name,
                    outcome.decommissioned_agents,
                    agents.len()
                ))
            })?;
            outcome.decommissioned_agents += 1;
        }
    }

    // --- unregister ----------------------------------------------------------------------
    let had_area = area.clone();
    // The config is written next; if that fails the area is put back.
    let area_gone = fault_delete_area()
        .and_then(|()| ws.delete_area(&slug, at.clone(), by))
        .map_err(|e| {
            failed(format!(
                "could not delete the area of `{slug}`: {e} ({} agents were reassigned and {} \
                 decommissioned; the project is still registered)",
                outcome.reassigned_agents, outcome.decommissioned_agents
            ))
        });
    area_gone?;
    let mut doc: DocumentMut = doc;
    if remove_blocks(&mut doc, &slug) > 0 {
        let text = doc.to_string();
        let written = text
            .parse::<DocumentMut>()
            .map_err(|e| failed(format!("the edited config is not valid TOML: {e}")))
            .and_then(|_| {
                fault_write_config().and_then(|()| write_atomic(&paths.config_path, &text))
            });
        if let Err(e) = written {
            if let Some(before) = had_area {
                let _ = ws.upsert_area(before);
            }
            return Err(e.into());
        }
    }
    for id in &held_ids {
        ws.remove_project(id).map_err(|e| {
            failed(format!(
                "`{slug}` is unregistered, but the workspace store's own row for it could not be \
                 removed: {e}. Nothing on disk was deleted"
            ))
        })?;
    }
    #[cfg(test)]
    hooks::run_after_unregister();

    // --- files ---------------------------------------------------------------------------
    if req.delete_store {
        // The store is looked at once more: the checks above ran before the agents moved and
        // the project was unregistered.
        let again = check_store(
            &slug,
            &store_of(&slug, current, paths),
            &entries,
            &listed,
            paths,
        );
        let mut error = None;
        match again {
            Ok(files) => {
                for file in &files {
                    #[cfg(test)]
                    assert_deletable_in_test(file);
                    if let Err(e) = std::fs::remove_file(file)
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        error = Some(format!("could not delete {}: {e}", file.display()));
                        break;
                    }
                }
                outcome.store_deleted = error.is_none() && !files.is_empty();
            }
            Err(e) => error = Some(e.to_string()),
        }
        outcome.error = error;
    }
    if let Some(canon) = dir {
        // Every directory check runs again, on the directory as it is now: it must pass, and
        // it must be the directory that was checked. (A swapped ancestor between this look and
        // the removal is the one change left; see the module docs.)
        let result = match dir_check.validate() {
            Ok(now) if now == canon => {
                #[cfg(test)]
                assert_deletable_in_test(&canon);
                remove_dir(&canon)
            }
            Ok(now) => Err(format!(
                "{} changed after it was checked (it is now {}); it was not deleted",
                canon.display(),
                now.display()
            )),
            Err(e) => Err(format!(
                "{} changed after it was checked, and it was not deleted: {e}",
                canon.display()
            )),
        };
        match result {
            Ok(()) => outcome.dir_deleted = true,
            Err(e) => {
                outcome.error = Some(match outcome.error.take() {
                    Some(prior) => format!("{prior}; {e}"),
                    None => e,
                });
            }
        }
    }
    Ok(outcome)
}
