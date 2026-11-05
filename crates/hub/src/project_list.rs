//! Listing and updating projects: what `fl project list`, `GET /api/v1/projects` and
//! `PATCH /api/v1/projects/:slug` read and change. The creation flows are in
//! [`crate::project_init`], which this module builds on: both kinds of project (directory-backed
//! and virtual) are listed here as one list.
//!
//! A project is known from four places, merged by slug:
//!
//! - a `[[project]]` block in the user config (a directory-backed project, with its root,
//!   store and tracker),
//! - a per-project store file the caller knows of (the Hub scans its data directory),
//! - a project the workspace store itself holds (`fl project add` with no binding, the Hub's
//!   own records), named by its root's directory, and
//! - an area with no path in the workspace store (a virtual group).
//!
//! The first three make a project and win over the last, so a slug is one kind.

use std::collections::BTreeMap;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use fl_core::agent::{AgentRecord, AgentStatus, AreaRecord};
use fl_core::ids::ProjectId;
use fl_core::model::Project;
use fl_core::state::validate_slug;
use fl_core::store::{AgentRegistry, Catalog};
use fl_store::AnyStore;
use serde::Serialize;
use toml_edit::{DocumentMut, Item, Table, value};

use crate::project_init::{
    CONFIG_LOCK, ConfigEntry, Credential, InitError, InitPaths, ProjectKind, Tracker,
    canonical_root, check_store_dir_overlap, checked_display_name, checked_github, checked_purpose,
    config_entries, conflict, default_purpose, failed, has_git, invalid, read_config, same_root,
    slug_from_display, write_atomic,
};

/// One project as the list shows it. It serializes with every field present; an absent value
/// is `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectSummary {
    /// The project's slug: its name in routes, areas and addresses. Never changes.
    pub slug: String,
    /// The name shown for the project (its area's name).
    pub name: String,
    pub kind: ProjectKind,
    /// The project root; `null` for a virtual group.
    pub path: Option<String>,
    /// The root is a Git working tree (it holds a `.git`; a repository with no commit counts).
    pub has_git: bool,
    pub purpose: String,
    pub tracker: Option<Tracker>,
    /// The project's store file; `null` for a virtual group, and for a project with none yet.
    pub store_path: Option<String>,
    /// Active agents whose project or extra areas include this slug.
    pub active_agent_count: usize,
}

/// A per-project store file the caller knows of: its slug, and where the file is (`None` when
/// the project is listed without one, as the Hub's default project).
pub type StoreHint = (String, Option<PathBuf>);

/// The slug the workspace store's project row gets in the list: its root's directory name in
/// lowercase, or the slug made from it when that is not a slug.
pub(crate) fn held_slug(root: &str) -> Option<String> {
    let base = Path::new(root).file_name().and_then(|s| s.to_str())?;
    Some(base.to_ascii_lowercase())
        .filter(|s| validate_slug(s).is_ok())
        .or_else(|| slug_from_display(base).ok())
}

/// Whether the list shows `slug` as a project and not as a virtual group: a `[[project]]` block
/// names it, a per-project store file is known for it (`stores`, the default project among
/// them), or the workspace store holds a project row of that name (`held`). This is the one
/// predicate [`list_projects`] and the records routes ask; an area that none of these names is a
/// virtual group (when it has no path).
pub(crate) fn is_project_slug(
    slug: &str,
    entries: &[ConfigEntry],
    stores: &[StoreHint],
    held: &[Project],
) -> bool {
    entries.iter().any(|e| e.slug().as_deref() == Some(slug))
        || stores.iter().any(|(s, _)| s == slug)
        || held
            .iter()
            .any(|p| held_slug(&p.root).as_deref() == Some(slug))
}

/// The per-project stores in `dir`: each `<slug>.<ext>` file there (not a link, at most 200
/// entries) as a [`StoreHint`], by slug. Reads the directory and nothing else; absent is none.
pub fn scan_store_dir(dir: &Path, ext: &str) -> Vec<StoreHint> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let suffix = format!(".{ext}");
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    for entry in entries.take(200).flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(suffix.as_str())) else {
            continue;
        };
        if validate_slug(stem).is_ok() {
            found.insert(stem.to_string(), entry.path());
        }
    }
    found.into_iter().map(|(s, p)| (s, Some(p))).collect()
}

/// Every project and virtual group, by slug. `stores` are the per-project store files the
/// caller knows of beyond the config (the Hub passes the ones in its data directory), and `held`
/// are the project rows of the stores the caller has open (the workspace store; the CLI adds
/// the store the current directory is bound to).
pub fn list_projects<R: AgentRegistry + ?Sized>(
    config_path: &Path,
    stores: &[StoreHint],
    held: &[Project],
    registry: &R,
) -> Result<Vec<ProjectSummary>, InitError> {
    let st = |e: fl_core::store::StoreError| failed(format!("workspace store: {e}"));
    let doc = read_config(config_path)?;
    let entries = config_entries(&doc, config_path)?;
    let areas: BTreeMap<String, AreaRecord> = registry
        .list_areas()
        .map_err(st)?
        .into_iter()
        .map(|a| (a.area_key.clone(), a))
        .collect();
    let agents = registry.list_agents(None, false).map_err(st)?;
    let count = |slug: &str| {
        let name = areas.get(slug).map(|a| a.name.as_str());
        agents
            .iter()
            .filter(|a| agent_in_project(a, slug, name))
            .count()
    };
    let make = |slug: &str, kind, path: Option<&str>, tracker, store: Option<String>| {
        let area = areas.get(slug);
        ProjectSummary {
            slug: slug.to_string(),
            name: area.map_or_else(|| slug.to_string(), |a| a.name.clone()),
            kind,
            path: path.map(str::to_string),
            has_git: path.is_some_and(|p| has_git(Path::new(p))),
            purpose: area.map(|a| a.purpose.clone()).unwrap_or_default(),
            tracker,
            store_path: store,
            active_agent_count: count(slug),
        }
    };

    let mut out: BTreeMap<String, ProjectSummary> = BTreeMap::new();
    for e in &entries {
        let Some(slug) = e.slug().filter(|s| validate_slug(s).is_ok()) else {
            continue;
        };
        let hint = stores
            .iter()
            .find(|(s, _)| *s == slug)
            .and_then(|(_, p)| p.as_ref());
        let store = e
            .store
            .clone()
            .or_else(|| hint.map(|p| p.display().to_string()));
        out.entry(slug.clone()).or_insert_with(|| {
            make(
                &slug,
                ProjectKind::Project,
                Some(&e.root),
                e.tracker_binding(),
                store,
            )
        });
    }
    for (slug, path) in stores {
        out.entry(slug.clone()).or_insert_with(|| {
            make(
                slug,
                ProjectKind::Project,
                None,
                None,
                path.as_ref().map(|p| p.display().to_string()),
            )
        });
    }
    // Projects the workspace store itself holds (`fl project add` into it, or the Hub's own
    // records): named by their root's directory, like a config block with no `name`.
    for p in held {
        // A directory name that is no slug (`.tmpAb1`) is made into one, as a display name is.
        let Some(slug) = held_slug(&p.root) else {
            continue;
        };
        match out.get_mut(&slug) {
            Some(held) if held.path.is_none() => {
                held.has_git = has_git(Path::new(&p.root));
                held.path = Some(p.root.clone());
            }
            Some(_) => {}
            None => {
                out.insert(
                    slug.clone(),
                    make(&slug, ProjectKind::Project, Some(&p.root), None, None),
                );
            }
        }
    }
    for (slug, area) in &areas {
        if area.is_virtual() && !is_project_slug(slug, &entries, stores, held) {
            out.insert(
                slug.clone(),
                make(slug, ProjectKind::Virtual, None, None, None),
            );
        }
    }
    Ok(out.into_values().collect())
}

/// Whether `agent` is an active agent of the project `slug`, whose area is named `area_name`:
/// its `project` or one of its extra areas equals the slug or the area's name, ignoring ASCII
/// case (`FerroLoop`, `FERROLOOP` and `ferroloop` are one project). This is the one matcher
/// the active-agent count and the removal of a project use.
pub fn agent_in_project(agent: &AgentRecord, slug: &str, area_name: Option<&str>) -> bool {
    let names = |candidate: &str| {
        candidate.trim().eq_ignore_ascii_case(slug)
            || area_name.is_some_and(|n| candidate.trim().eq_ignore_ascii_case(n.trim()))
    };
    agent.status == AgentStatus::Active
        && (names(&agent.project) || agent.extra_areas.iter().any(|a| names(a)))
}

/// What to change in a project. A field that is `None` is left as it is.
#[derive(Debug, Clone, Default)]
pub struct UpdateRequest {
    /// The display name. The slug never changes.
    pub name: Option<String>,
    pub purpose: Option<String>,
    /// The new root of a directory-backed project: it must be an existing directory.
    pub path: Option<PathBuf>,
    /// `owner/repo` to set or replace the tracker; `""` to clear it.
    pub github: Option<String>,
    /// The tracker's credential source. It applies to the tracker the project has, or the one
    /// `github` sets (`app` when neither says).
    pub credential: Option<Credential>,
    /// Who is changing it.
    pub by: String,
}

impl UpdateRequest {
    fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.purpose.is_none()
            && self.path.is_none()
            && self.github.is_none()
            && self.credential.is_none()
    }
}

/// The tracker a project has after `req`.
fn planned_tracker(
    req: &UpdateRequest,
    current: Option<&Tracker>,
) -> Result<Option<Tracker>, InitError> {
    match (req.github.as_deref().map(str::trim), req.credential) {
        (Some(""), Some(_)) => Err(invalid(
            "a credential source names how to reach a GitHub repository: it cannot come with \
             `github: \"\"`, which clears the tracker",
        )),
        (Some(""), None) => Ok(None),
        (Some(g), c) => Ok(Some(Tracker {
            github: checked_github(g)?,
            credential: c
                .or(current.map(|t| t.credential))
                .unwrap_or(Credential::App),
        })),
        (None, Some(c)) => match current {
            Some(t) => Ok(Some(Tracker {
                github: t.github.clone(),
                credential: c,
            })),
            None => Err(invalid(
                "the project has no tracker to set a credential on: pass `github` too",
            )),
        },
        (None, None) => Ok(current.cloned()),
    }
}

/// Edits the project's block in `doc`: its root and tracker.
fn edit_block(
    doc: &mut DocumentMut,
    old_root: &str,
    new_root: &Path,
    old: Option<&Tracker>,
    new: Option<&Tracker>,
) -> Result<(), InitError> {
    let block = doc
        .get_mut("project")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|blocks| {
            blocks.iter_mut().find(|b| {
                b.get("root")
                    .and_then(Item::as_str)
                    .is_some_and(|root| root == old_root)
            })
        })
        .ok_or_else(|| failed("the project's config block vanished while it was edited"))?;
    if !same_root(old_root, new_root) {
        block.insert("root", value(new_root.to_string_lossy().as_ref()));
    }
    if old != new {
        match (old, new) {
            // The same repository with another credential: edit in place, so the block's
            // other tracker keys (`ledger`) stay.
            (Some(o), Some(n))
                if o.github == n.github
                    && let Some(t) = block.get_mut("tracker").and_then(Item::as_table_like_mut) =>
            {
                t.insert("credential", value(n.credential.as_str()));
            }
            (_, Some(n)) => {
                let mut table = Table::new();
                table.insert("github", value(n.github.as_str()));
                table.insert("credential", value(n.credential.as_str()));
                block.insert("tracker", Item::Table(table));
            }
            (_, None) => {
                block.remove("tracker");
            }
        }
    }
    Ok(())
}

/// Updates one project, in the config and the workspace registry together.
///
/// - `name` is the display name (the area's name); the slug never changes.
/// - `purpose` is the area's purpose; an empty or blank one resets it to the default for the
///   project's kind.
/// - `path` moves a directory-backed project to another existing directory, with the safety
///   rules creation has; a virtual group has no path and refuses it. The project's store keeps
///   its gates and records and is pointed at the new root.
/// - `github` and `credential` set, replace or clear (`github: ""`) the tracker; a virtual
///   group has none.
///
/// Everything is checked before anything is written. The stores are written first and the
/// config last, in one atomic replacement; if that fails the stores are put back. Asking for
/// what is already so changes nothing, so a repeat is a no-op.
pub fn update_project<P, W>(
    slug: &str,
    req: &UpdateRequest,
    paths: &InitPaths,
    stores: &[StoreHint],
    open_workspace: impl FnOnce() -> Result<W, InitError>,
    open_project: impl Fn(&Path) -> Result<P, InitError>,
) -> Result<ProjectSummary, InitError>
where
    P: Deref<Target = AnyStore>,
    W: Deref<Target = AnyStore>,
{
    let _one_at_a_time = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if req.is_empty() {
        return Err(invalid(
            "nothing to change: pass `name`, `purpose`, `path`, `github` or `credential`",
        ));
    }
    if req.by.trim().is_empty() {
        return Err(invalid("`by` is empty"));
    }
    let name = req.name.as_deref().map(checked_display_name).transpose()?;
    // An empty purpose is not a purpose: it resets to the default for the project's kind.
    let reset_purpose = req.purpose.as_deref().is_some_and(|p| p.trim().is_empty());
    let mut purpose = match req.purpose.as_deref() {
        Some(p) if !reset_purpose => Some(checked_purpose(p)?),
        _ => None,
    };
    let st = |e: fl_core::store::StoreError| failed(format!("workspace store: {e}"));

    let ws = open_workspace()?;
    let held = ws.list_projects().map_err(st)?;
    let listed = list_projects(&paths.config_path, stores, &held, &*ws)?;
    let Some(current) = listed.into_iter().find(|p| p.slug == slug) else {
        return Err(InitError::NotFound(format!("no project `{slug}`")));
    };

    if reset_purpose {
        purpose = Some(default_purpose(current.kind, slug));
    }

    // The area as it will be: its path is the project's root, and a virtual group has none.
    let existing = ws.get_area(slug).map_err(st)?;
    let mut area = existing.clone().unwrap_or_else(|| AreaRecord {
        area_key: slug.to_string(),
        name: current.name.clone(),
        path: current.path.clone(),
        purpose: if current.purpose.is_empty() {
            default_purpose(current.kind, slug)
        } else {
            current.purpose.clone()
        },
        updated_at: fl_exec::stamp::now(),
        by: req.by.trim().to_string(),
    });
    if let Some(n) = &name {
        area.name = n.clone();
    }
    if let Some(p) = &purpose {
        area.purpose = p.clone();
    }

    if current.kind == ProjectKind::Virtual {
        if req.path.is_some() {
            return Err(invalid(
                "a virtual group has no path: only a directory-backed project can be moved",
            ));
        }
        if req.github.is_some() || req.credential.is_some() {
            return Err(invalid("a virtual group has no tracker"));
        }
        return finish_area_only(slug, existing, area, req, &*ws, paths, stores);
    }

    // A directory-backed project. Path and tracker live in its config block.
    let doc = read_config(&paths.config_path)?;
    let entries = config_entries(&doc, &paths.config_path)?;
    let entry = entries.iter().find(|e| e.slug().as_deref() == Some(slug));
    if entry.is_none() && (req.path.is_some() || req.github.is_some() || req.credential.is_some()) {
        return Err(conflict(format!(
            "project `{slug}` has a store but no [[project]] block in {}, so its path and \
             tracker cannot be changed here. Register it with `fl project create`",
            paths.config_path.display()
        )));
    }
    let new_root = match (&req.path, entry) {
        (Some(path), Some(e)) => {
            let root = canonical_root(path)?;
            if !root.is_dir() {
                return Err(invalid(format!(
                    "{} is not an existing directory: create it first, then move the project \
                     there",
                    root.display()
                )));
            }
            check_store_dir_overlap(&root, paths)?;
            if let Some(other) = entries
                .iter()
                .find(|o| o.slug().as_deref() != Some(slug) && same_root(&o.root, &root))
            {
                return Err(conflict(format!(
                    "{} is already project `{}`; one directory is one project",
                    root.display(),
                    other.slug().unwrap_or_default()
                )));
            }
            Some((root, e.root.clone()))
        }
        _ => None,
    };
    let old_tracker = entry.and_then(|e| e.tracker_binding());
    let new_tracker = match entry {
        Some(_) => planned_tracker(req, old_tracker.as_ref())?,
        None => None,
    };
    if let Some((root, _)) = &new_root {
        area.path = Some(root.to_string_lossy().into_owned());
    }

    let moves = new_root
        .as_ref()
        .filter(|(root, old)| !same_root(old, root));
    let config_changes = moves.is_some() || old_tracker != new_tracker;
    let area_changes = existing
        .as_ref()
        .is_none_or(|a| a.name != area.name || a.purpose != area.purpose || a.path != area.path);

    // The project's store is pointed at the new root first: it is the write that can refuse.
    let mut moved: Option<(PathBuf, String, ProjectId)> = None;
    if let (Some((root, old)), Some(e)) = (moves, entry) {
        let store_path = PathBuf::from(e.store.clone().unwrap_or_else(|| {
            paths
                .store_dir
                .join(format!("{slug}.fls"))
                .to_string_lossy()
                .into_owned()
        }));
        // Opening a store that is not there would make an empty one.
        if !fl_store::store_exists(&store_path) {
            return Err(conflict(format!(
                "the store {} of project `{slug}` does not exist, so the project cannot be moved \
                 (opening it would make an empty store). Restore the store, or register the \
                 project again with `fl project create`",
                store_path.display()
            )));
        }
        let store = open_project(&store_path)?;
        let held = store
            .list_projects()
            .map_err(|e| failed(format!("store: {e}")))?;
        match held.as_slice() {
            [] => {}
            [p] if same_root(&p.root, Path::new(old)) || same_root(&p.root, root) => {
                store
                    .set_project_root(&p.id, &root.to_string_lossy())
                    .map_err(|e| failed(format!("store: {e}")))?;
                moved = Some((store_path.clone(), p.root.clone(), p.id.clone()));
            }
            other => {
                return Err(conflict(format!(
                    "the store {} holds {}, not {old}: it is not this project's",
                    store_path.display(),
                    other
                        .iter()
                        .map(|p| p.root.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
    }

    let put_back = |why: InitError| {
        if area_changes {
            match &existing {
                Some(before) => {
                    let _ = ws.upsert_area(before.clone());
                }
                // The area was made by this update: it goes again.
                None => {
                    let _ = ws.delete_area(slug, fl_exec::stamp::now(), req.by.trim());
                }
            }
        }
        if let Some((path, root, id)) = &moved
            && let Ok(store) = open_project(path)
        {
            let _ = store.set_project_root(id, root);
        }
        why
    };
    if area_changes {
        area.updated_at = fl_exec::stamp::now();
        area.by = req.by.trim().to_string();
        ws.upsert_area(area.clone())
            .map_err(|e| put_back(failed(format!("workspace store: {e}"))))?;
    }
    if config_changes && let Some(e) = entry {
        let mut doc = doc;
        let root = new_root
            .as_ref()
            .map_or_else(|| PathBuf::from(&e.root), |(r, _)| r.clone());
        let written = edit_block(
            &mut doc,
            &e.root,
            &root,
            old_tracker.as_ref(),
            new_tracker.as_ref(),
        )
        .and_then(|()| {
            let text = doc.to_string();
            text.parse::<DocumentMut>()
                .map_err(|e| failed(format!("the edited config is not valid TOML: {e}")))?;
            crate::project_delete::fault_write_config()?;
            write_atomic(&paths.config_path, &text)
        });
        written.map_err(put_back)?;
    }
    refreshed(slug, paths, stores, &*ws)
}

/// A virtual group: only its area can change.
fn finish_area_only<R: AgentRegistry + Catalog + ?Sized>(
    slug: &str,
    existing: Option<AreaRecord>,
    mut area: AreaRecord,
    req: &UpdateRequest,
    ws: &R,
    paths: &InitPaths,
    stores: &[StoreHint],
) -> Result<ProjectSummary, InitError> {
    let changed = existing
        .as_ref()
        .is_none_or(|a| a.name != area.name || a.purpose != area.purpose);
    if changed {
        area.updated_at = fl_exec::stamp::now();
        area.by = req.by.trim().to_string();
        ws.upsert_area(area)
            .map_err(|e| failed(format!("workspace store: {e}")))?;
    }
    refreshed(slug, paths, stores, ws)
}

fn refreshed<R: AgentRegistry + Catalog + ?Sized>(
    slug: &str,
    paths: &InitPaths,
    stores: &[StoreHint],
    ws: &R,
) -> Result<ProjectSummary, InitError> {
    let held = ws
        .list_projects()
        .map_err(|e| failed(format!("workspace store: {e}")))?;
    list_projects(&paths.config_path, stores, &held, ws)?
        .into_iter()
        .find(|p| p.slug == slug)
        .ok_or_else(|| failed("the project vanished while it was updated"))
}
