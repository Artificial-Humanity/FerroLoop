//! Deleting a project: what `fl project delete` and `DELETE /api/v1/projects/:slug` run.
//!
//! The order is fixed: validate everything (the agents, the reassignment target, the safety of
//! the directory and the store files), then reassign or decommission the active agents, then
//! unregister (the `[[project]]` block and the area), then delete the store files, then delete
//! the directory. A refusal in the first step changes nothing. The last two steps never undo
//! the unregistration: a failure there is reported in the outcome (`error`, with the flag that
//! did not happen left `false`).
//!
//! By default nothing on disk is deleted: only the config block and the area go. A project's
//! store file is still in the Hub's data directory then, and the Hub lists a project from a
//! store file there, so the Hub keeps listing the slug (with no path) until `delete_store`
//! removes the file as well.

use std::ops::Deref;
use std::path::{Component, Path, PathBuf};

use fl_core::agent::{AgentRecord, AgentRole, AreaRecord, RegisterAgentArgs};
use fl_core::state::validate_slug;
use fl_core::store::{AgentRegistry, Catalog};
use fl_store::AnyStore;
use serde::{Deserialize, Serialize};
use toml_edit::DocumentMut;

use crate::lifecycle::{self, LifecycleEnv, SessionTarget};
use crate::project_init::{
    CONFIG_LOCK, ConfigEntry, InitError, InitPaths, ProjectKind, checked_slug, config_entries,
    conflict, failed, invalid, read_config, remove_blocks, same_root, slug_from_display,
    write_atomic,
};
use crate::project_list::{ProjectSummary, StoreHint, agent_in_project, list_projects};

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
    /// The workspace root (`fl_core::resolve_workspace_root`).
    pub workspace_root: &'a Path,
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

fn depth(path: &Path) -> usize {
    path.components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count()
}

/// The slug the workspace store's project row gets in the list (see `list_projects`).
fn held_slug(root: &str) -> Option<String> {
    let base = Path::new(root).file_name().and_then(|s| s.to_str())?;
    Some(base.to_ascii_lowercase())
        .filter(|s| validate_slug(s).is_ok())
        .or_else(|| slug_from_display(base).ok())
}

struct DirCheck<'a> {
    slug: &'a str,
    current: &'a ProjectSummary,
    area: Option<&'a AreaRecord>,
    entries: &'a [ConfigEntry],
    listed: &'a [ProjectSummary],
    held: &'a [fl_core::model::Project],
    ctx: &'a DeleteContext<'a>,
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

        // Not the home directory or one above it.
        let env_home = std::env::var_os("HOME").map(PathBuf::from);
        for home in [self.ctx.home.map(Path::to_path_buf), env_home]
            .into_iter()
            .flatten()
        {
            if !home.as_os_str().is_empty() && resolve_loose(&home).starts_with(&canon) {
                return Err(refuse(format!(
                    "it is the home directory `{}` or contains it",
                    home.display()
                )));
            }
        }
        // Not the workspace root or one above it.
        let root = resolve_loose(self.ctx.workspace_root);
        if root.starts_with(&canon) {
            return Err(refuse(format!(
                "it is the workspace root `{}` or contains it",
                root.display()
            )));
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

/// Refuses a store that is not this project's alone: the workspace store, another project's
/// store, a link or a directory.
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

    let dir = if req.delete_dir {
        Some(
            DirCheck {
                slug: &slug,
                current,
                area: area.as_ref(),
                entries: &entries,
                listed: &listed,
                held: &held,
                ctx,
            }
            .validate()?,
        )
    } else {
        None
    };
    let store_files = if req.delete_store {
        let store = current
            .store_path
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| paths.store_dir.join(format!("{slug}.fls")));
        check_store(&slug, &store, &entries, &listed, paths)?
    } else {
        Vec::new()
    };

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
    ws.delete_area(&slug, at.clone(), by).map_err(st)?;
    let mut doc: DocumentMut = doc;
    if remove_blocks(&mut doc, &slug) > 0 {
        let text = doc.to_string();
        let written = text
            .parse::<DocumentMut>()
            .map_err(|e| failed(format!("the edited config is not valid TOML: {e}")))
            .and_then(|_| write_atomic(&paths.config_path, &text));
        if let Err(e) = written {
            if let Some(before) = had_area {
                let _ = ws.upsert_area(before);
            }
            return Err(e.into());
        }
    }

    // --- files ---------------------------------------------------------------------------
    if req.delete_store {
        let mut error = None;
        for file in &store_files {
            #[cfg(test)]
            assert_deletable_in_test(file);
            if let Err(e) = std::fs::remove_file(file)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                error = Some(format!("could not delete {}: {e}", file.display()));
                break;
            }
        }
        outcome.store_deleted = error.is_none() && !store_files.is_empty();
        outcome.error = error;
    }
    if let Some(canon) = dir {
        // The directory is looked at once more: it must still be the one that was checked.
        let still = std::fs::symlink_metadata(&canon)
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
            && canon.canonicalize().is_ok_and(|c| c == canon);
        #[cfg(test)]
        assert_deletable_in_test(&canon);
        let result = if still {
            std::fs::remove_dir_all(&canon)
                .map_err(|e| format!("could not delete {}: {e}", canon.display()))
        } else {
            Err(format!(
                "{} changed after it was checked; it was not deleted",
                canon.display()
            ))
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
