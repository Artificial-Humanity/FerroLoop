//! Where the hub keeps artifacts: the hub data home, the system artifacts directory, per-project
//! artifacts directories, and the Owner-only endpoints that relocate them.
//!
//! - `[home]` (the hub data home) is the directory that holds the hub store, so in production
//!   `~/.local/share/fl`. It is not `$HOME` (see [`crate::api::hub_home_dir`]), where the settings
//!   file lives.
//! - The system artifacts directory is `[home]/artifacts` unless `Settings::artifacts_dir` says
//!   otherwise. The artifact registry (`registry.json`, `versions/`, `managed/`) lives there.
//! - A project's artifacts directory is `<system dir>/<slug>` unless
//!   `Settings::project_artifacts_dirs` has an entry for the slug.
//!
//! The registry is session-based and has no project association: nothing writes into a project's
//! directory yet, and archive writes are not routed by project.
//!
//! Relocating a directory (`POST /api/v1/settings/artifacts-dir`,
//! `POST /api/v1/projects/:id/artifacts-dir`) changes a setting and, on request, moves the old
//! directory's contents to the new one. Both are Owner only: a peer's agent name is
//! self-declared, so no finer tier can be trusted.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, TryLockError};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::HubState;
use crate::api::{
    HttpRequest, HttpResponse, hub_home_dir, is_forbidden_artifact_path, json_object_body,
    percent_decode,
};
use crate::artifacts;
use crate::auth::HubAuthority;
use crate::records;
use crate::settings::{self, Settings};

/// Longest `new_path` accepted, in bytes.
const MAX_PATH_BYTES: usize = 4096;

/// Directories no artifacts directory may be, or be inside.
const SENSITIVE_ROOTS: &[&str] = &[
    "/etc", "/proc", "/sys", "/dev", "/boot", "/run", "/bin", "/sbin", "/lib", "/lib64", "/usr",
    "/var/run", "/root", "/snap",
];

/// Path components no artifacts directory may contain (compared ignoring case).
const SENSITIVE_NAMES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".docker",
    ".azure",
    ".password-store",
    ".git",
    ".fl",
    "secrets",
];

/// Names in the system artifacts directory that belong to the registry and are never served.
const REGISTRY_FILE: &str = "registry.json";
const VERSIONS_DIR: &str = "versions";

/// At most this many entries of a system artifacts directory become serving roots.
const MAX_SERVING_ENTRIES: usize = 2000;

/// At most this many names a 409 conflict message lists.
const MAX_CONFLICT_NAMES: usize = 5;

/// Held for the whole of a relocation, so two never run at once.
pub(crate) static RELOCATE_LOCK: Mutex<()> = Mutex::new(());

// --- directory resolution ---------------------------------------------------------------------

/// `[home]`: the directory holding the hub store, as an absolute path.
///
/// A relative store path is resolved against the process's working directory, the same base the
/// store itself is opened against (`fl hub serve` passes an absolute path already).
pub(crate) fn hub_data_home(state: &HubState) -> PathBuf {
    let store = &state.config.store_path;
    let store = std::path::absolute(store).unwrap_or_else(|_| store.clone());
    store
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .to_path_buf()
}

/// `[home]/artifacts`.
pub(crate) fn default_system_dir(state: &HubState) -> PathBuf {
    hub_data_home(state).join("artifacts")
}

/// The saved settings (the defaults when there is no home or no file).
fn current_settings(state: &HubState) -> Settings {
    hub_home_dir(state)
        .map(|home| settings::load(&home))
        .unwrap_or_default()
}

/// A stored directory is usable only when it is not somewhere an artifacts directory must never
/// be. A hand-edited settings file naming `/etc` or `$HOME` is ignored, so it is never written
/// to or created.
fn usable_stored(state: &HubState, stored: &str) -> Option<PathBuf> {
    let path = PathBuf::from(stored);
    let resolved = resolve_path(&path).unwrap_or_else(|_| path.clone());
    sensitive_reason(state, &resolved).is_none().then_some(path)
}

fn system_dir_in(state: &HubState, s: &Settings) -> PathBuf {
    s.artifacts_dir
        .as_deref()
        .and_then(|stored| usable_stored(state, stored))
        .unwrap_or_else(|| default_system_dir(state))
}

fn project_dir_in(state: &HubState, s: &Settings, slug: &str) -> PathBuf {
    s.project_artifacts_dirs
        .get(slug)
        .and_then(|stored| usable_stored(state, stored))
        .unwrap_or_else(|| system_dir_in(state, s).join(slug))
}

/// The system artifacts directory in use: the setting, else [`default_system_dir`].
pub(crate) fn effective_system_dir(state: &HubState) -> PathBuf {
    system_dir_in(state, &current_settings(state))
}

/// `<effective system dir>/<slug>`. The handlers use `project_dir_in` and `system_dir_in` on one
/// settings snapshot instead, so a response never mixes two reads.
#[cfg(test)]
pub(crate) fn default_project_dir(state: &HubState, slug: &str) -> PathBuf {
    effective_system_dir(state).join(slug)
}

/// A project's artifacts directory in use: its override, else [`default_project_dir`].
#[cfg(test)]
pub(crate) fn effective_project_dir(state: &HubState, slug: &str) -> PathBuf {
    project_dir_in(state, &current_settings(state), slug)
}

// --- JSON views -------------------------------------------------------------------------------

fn display(path: &Path) -> String {
    path.display().to_string()
}

/// The stored settings plus `home_dir`, `default_artifacts_dir` and `effective_artifacts_dir`.
/// `GET` and `PUT /api/v1/settings` and the `settings.updated` broadcast all use it.
pub(crate) fn settings_view(state: &HubState, s: &Settings) -> Value {
    let mut view = json!(s);
    if let Some(map) = view.as_object_mut() {
        map.insert("home_dir".into(), json!(display(&hub_data_home(state))));
        map.insert(
            "default_artifacts_dir".into(),
            json!(display(&default_system_dir(state))),
        );
        map.insert(
            "effective_artifacts_dir".into(),
            json!(display(&system_dir_in(state, s))),
        );
    }
    view
}

fn project_view(state: &HubState, s: &Settings, id: &str) -> Value {
    json!({
        "id": id,
        "artifacts_dir": s.project_artifacts_dirs.get(id),
        "default_artifacts_dir": display(&system_dir_in(state, s).join(id)),
        "effective_artifacts_dir": display(&project_dir_in(state, s, id)),
    })
}

/// One project as `GET /api/v1/projects` lists it, read from the saved settings.
pub(crate) fn project_descriptor(state: &HubState, id: &str) -> Value {
    project_view(state, &current_settings(state), id)
}

fn error(status: u16, message: impl Into<String>) -> HttpResponse {
    HttpResponse::json(status, json!({ "error": message.into() }))
}

// --- path validation --------------------------------------------------------------------------

/// `path` with symlinks resolved: the nearest existing ancestor is canonicalized and the missing
/// tail appended. `path` must be absolute and free of `..`.
pub(crate) fn resolve_path(path: &Path) -> io::Result<PathBuf> {
    let mut tail: Vec<OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        match fs::canonicalize(&cur) {
            Ok(mut resolved) => {
                resolved.extend(tail.iter().rev());
                return Ok(resolved);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let Some(name) = cur.file_name().map(OsString::from) else {
                    return Err(e);
                };
                tail.push(name);
                cur.pop();
            }
            Err(e) => return Err(e),
        }
    }
}

/// Which directory a relocation targets.
#[derive(Clone, Copy)]
pub(crate) enum Scope<'a> {
    System,
    Project(&'a str),
}

/// A move of `old` to `new` that would put one inside the other. Equal is not that: it is a no-op.
pub(crate) fn check_relation(new: &Path, old: &Path) -> Result<(), String> {
    let old = resolve_path(old).unwrap_or_else(|_| old.to_path_buf());
    if new == old {
        return Ok(());
    }
    if new.starts_with(&old) {
        return Err(format!(
            "the new directory {} is inside the current one {}; a directory cannot move into \
             itself",
            new.display(),
            old.display()
        ));
    }
    if old.starts_with(new) {
        return Err(format!(
            "the new directory {} contains the current one {}; a directory cannot move into \
             itself",
            new.display(),
            old.display()
        ));
    }
    Ok(())
}

/// Whether a resolved `path` is somewhere an artifacts directory must never be.
pub(crate) fn sensitive_reason(state: &HubState, path: &Path) -> Option<String> {
    for root in SENSITIVE_ROOTS {
        if path.starts_with(root) {
            return Some(format!(
                "{} is inside the protected directory {root}",
                path.display()
            ));
        }
    }
    for comp in path.components() {
        if let Component::Normal(name) = comp {
            let name = name.to_string_lossy().to_ascii_lowercase();
            if SENSITIVE_NAMES.contains(&name.as_str()) {
                return Some(format!(
                    "{} has a protected component `{name}`",
                    path.display()
                ));
            }
        }
    }
    if is_forbidden_artifact_path(path) {
        return Some(format!("{} is a protected path", path.display()));
    }
    let mut guarded = vec![hub_data_home(state)];
    if let Some(home) = hub_home_dir(state) {
        guarded.push(home);
    }
    for dir in guarded {
        let dir = resolve_path(&dir).unwrap_or(dir);
        if dir.starts_with(path) {
            return Some(format!(
                "{} is, or contains, the home directory {}",
                path.display(),
                dir.display()
            ));
        }
    }
    let attachments = crate::attachments::attachments_dir(state);
    let attachments = resolve_path(&attachments).unwrap_or(attachments);
    if path.starts_with(&attachments) {
        return Some(format!(
            "{} is inside the attachments directory",
            path.display()
        ));
    }
    None
}

/// Check a requested artifacts directory and return the path to store: symlinks resolved, and
/// every check run on the resolved path. Creates nothing. `old` is the directory in use now.
///
/// Refuses (with the reason) a path that is empty, over 4096 bytes, holds a NUL, is relative,
/// has a `.` or `..` component, is `/`, resolves into a system directory or one with a secret
/// name (`.ssh`, `.git`, `secrets`, ...), is or contains a home directory or the attachments
/// directory, or moves `old` into itself. A system directory also must not be, contain or sit
/// inside a project override. A project directory also must not contain the system directory or
/// sit in the registry's own files.
pub(crate) fn validate_target(
    state: &HubState,
    raw: &str,
    old: &Path,
    scope: Scope<'_>,
) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("`new_path` is empty".into());
    }
    if trimmed.len() > MAX_PATH_BYTES {
        return Err(format!("`new_path` is longer than {MAX_PATH_BYTES} bytes"));
    }
    if trimmed.contains('\0') {
        return Err("`new_path` contains a NUL byte".into());
    }
    let path = Path::new(trimmed);
    if !path.is_absolute() {
        return Err("`new_path` must be an absolute path".into());
    }
    if trimmed.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err("`new_path` must not contain `.` or `..` components".into());
    }
    if path.components().count() <= 1 {
        return Err("`new_path` must not be the filesystem root".into());
    }
    let resolved = resolve_path(path).map_err(|e| format!("could not resolve `{trimmed}`: {e}"))?;
    if resolved.components().count() <= 1 {
        return Err("`new_path` resolves to the filesystem root".into());
    }
    if let Some(reason) = sensitive_reason(state, &resolved) {
        return Err(reason);
    }
    check_relation(&resolved, old)?;
    if matches!(scope, Scope::System) {
        let s = current_settings(state);
        for (slug, dir) in &s.project_artifacts_dirs {
            let Ok(dir) = resolve_path(Path::new(dir)) else {
                continue;
            };
            if resolved.starts_with(&dir) || dir.starts_with(&resolved) {
                return Err(format!(
                    "{} is, contains or is inside the artifacts directory {} of project `{slug}`; \
                     change or reset that project's directory first",
                    resolved.display(),
                    dir.display()
                ));
            }
        }
    }
    if matches!(scope, Scope::Project(_)) {
        let system = effective_system_dir(state);
        let system = resolve_path(&system).unwrap_or(system);
        if system.starts_with(&resolved) {
            return Err(format!(
                "{} is, or contains, the system artifacts directory {}",
                resolved.display(),
                system.display()
            ));
        }
        let registry_files = resolved.starts_with(system.join(VERSIONS_DIR))
            || resolved.starts_with(system.join(REGISTRY_FILE));
        if registry_files {
            return Err(format!(
                "{} is inside the artifact registry's own files",
                resolved.display()
            ));
        }
    }
    Ok(resolved)
}

/// Create `dir` (and parents) and require it to be a directory. A symlink that resolves to a
/// directory is fine.
pub(crate) fn prepare_target(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    match fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(format!("{} is not a directory", dir.display())),
        Err(e) => Err(format!("could not read {}: {e}", dir.display())),
    }
}

// --- migration --------------------------------------------------------------------------------

/// What a completed migration moved.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Migrated {
    pub files: u64,
    pub bytes: u64,
    /// Top-level names moved, in the order they moved.
    pub moved: Vec<OsString>,
}

#[derive(Debug)]
pub(crate) enum MigrateError {
    /// These top-level names already exist in the target. Nothing was moved (or what moved was
    /// put back).
    Conflict(Vec<String>),
    /// The target could not be created. Nothing was moved.
    Prepare(String),
    /// A move failed. The entries moved before it were rolled back where possible;
    /// `rolled_back` is false when any of them, or the failed entry itself, is left in both
    /// directories or in the wrong one.
    Failed {
        message: String,
        files_moved: u64,
        rolled_back: bool,
    },
}

/// Regular files and their bytes under `path`, never following a symlink. A symlink and other
/// non-regular entries count for neither.
pub(crate) fn count_tree(path: &Path) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&cur) else {
            continue;
        };
        if meta.is_file() {
            files += 1;
            bytes += meta.len();
        } else if meta.is_dir()
            && let Ok(entries) = fs::read_dir(&cur)
        {
            stack.extend(entries.flatten().map(|e| e.path()));
        }
    }
    (files, bytes)
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Copy `src` to `dst` (which must not exist), never following a symlink: a symlink is
/// recreated as a symlink.
pub(crate) fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)
    } else if meta.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        fs::set_permissions(dst, meta.permissions())
    } else if meta.is_file() {
        fs::copy(src, dst).map(|_| ())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not a regular file, directory or symlink",
                src.display()
            ),
        ))
    }
}

/// The cross-device move: copy `src` to `dst`, then remove `src`. A failed copy removes the
/// partial `dst` and leaves `src` untouched. A failure removing `src` after a good copy keeps
/// both, and says so.
pub(crate) fn copy_then_remove(src: &Path, dst: &Path) -> io::Result<()> {
    if let Err(e) = copy_recursive(src, dst) {
        let _ = remove_path(dst);
        return Err(e);
    }
    remove_path(src).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "copied to {} but could not remove the original {}: {e}",
                dst.display(),
                src.display()
            ),
        )
    })
}

pub(crate) fn is_cross_device(e: &io::Error) -> bool {
    e.raw_os_error() == Some(18) || e.kind() == io::ErrorKind::CrossesDevices
}

/// Move one entry: `rename`, falling back to copy-then-remove across devices.
pub(crate) fn move_entry(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Err(e) if is_cross_device(&e) => copy_then_remove(src, dst),
        other => other,
    }
}

/// Put entries moved from `from` to `to` back. Best effort; whether every one went back.
pub(crate) fn rollback(from: &Path, to: &Path, moved: &[OsString]) -> bool {
    let mut all = true;
    for name in moved.iter().rev() {
        if move_entry(&to.join(name), &from.join(name)).is_err() {
            all = false;
        }
    }
    all
}

/// Sort top-level names for moving: alphabetical, `registry.json` last, so an interrupted move
/// never leaves a registry that points at files not yet moved.
pub(crate) fn order_entries(names: &mut [OsString]) {
    names.sort();
    names.sort_by_key(|n| n.as_os_str() == REGISTRY_FILE);
}

/// Move the contents of `old` into `new`, then create `new` if it did not exist.
///
/// Does nothing but create `new` when `old` is not a directory. Refuses with
/// [`MigrateError::Conflict`], moving nothing, when a top-level name of `old` already exists in
/// `new`. Each top-level entry moves with `rename`, else copy and remove across devices;
/// `registry.json` moves last. Symlinks are never followed. The caller must hold the registry
/// lock (see [`artifacts::with_registry_lock`]).
pub(crate) fn migrate_dir(old: &Path, new: &Path) -> Result<Migrated, MigrateError> {
    migrate_dir_with(old, new, &move_entry)
}

/// [`migrate_dir`] with the per-entry mover chosen by the caller (tests force the copy path).
pub(crate) fn migrate_dir_with(
    old: &Path,
    new: &Path,
    mover: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> Result<Migrated, MigrateError> {
    let mut names: Vec<OsString> = match fs::symlink_metadata(old) {
        Ok(_) if old.is_dir() => fs::read_dir(old)
            .map_err(|e| MigrateError::Prepare(format!("could not read {}: {e}", old.display())))?
            .flatten()
            .map(|e| e.file_name())
            .collect(),
        _ => Vec::new(),
    };
    order_entries(&mut names);

    let clashes: Vec<String> = names
        .iter()
        .filter(|n| fs::symlink_metadata(new.join(n)).is_ok())
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    if !clashes.is_empty() {
        return Err(MigrateError::Conflict(clashes));
    }
    prepare_target(new).map_err(MigrateError::Prepare)?;

    // Counted before anything moves.
    let sizes: Vec<(u64, u64)> = names.iter().map(|n| count_tree(&old.join(n))).collect();
    let mut done = Migrated::default();
    for (name, (files, bytes)) in names.iter().zip(sizes) {
        let (from, to) = (old.join(name), new.join(name));
        // Something may have appeared in the target since the preflight; `rename` would replace
        // a file or an empty directory silently. This narrows that window, it cannot close it.
        if fs::symlink_metadata(&to).is_ok() {
            let files_moved = done.files;
            return if rollback(old, new, &done.moved) {
                Err(MigrateError::Conflict(vec![
                    name.to_string_lossy().into_owned(),
                ]))
            } else {
                Err(MigrateError::Failed {
                    message: format!(
                        "`{}` appeared in {} during the move",
                        name.to_string_lossy(),
                        new.display()
                    ),
                    files_moved,
                    rolled_back: false,
                })
            };
        }
        if let Err(e) = mover(&from, &to) {
            let files_moved = done.files;
            // A copy that finished but whose source could not be fully removed leaves the entry
            // in both directories; it is not in `done.moved`, so the rollback cannot put it back.
            let in_both = fs::symlink_metadata(&from).is_ok() && fs::symlink_metadata(&to).is_ok();
            let mut message = format!("could not move `{}`: {e}", name.to_string_lossy());
            if in_both {
                message.push_str(&format!(
                    " (`{}` exists in both directories; keep one and delete the other)",
                    name.to_string_lossy()
                ));
            }
            let rolled_back = rollback(old, new, &done.moved) && !in_both;
            return Err(MigrateError::Failed {
                message,
                files_moved,
                rolled_back,
            });
        }
        done.files += files;
        done.bytes += bytes;
        done.moved.push(name.clone());
    }
    Ok(done)
}

// --- relocation endpoints ---------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RelocateBody {
    new_path: Option<String>,
    #[serde(default)]
    migrate: bool,
}

fn apply_setting(s: &mut Settings, scope: Scope<'_>, value: Option<&Path>) {
    let value = value.map(display);
    match scope {
        Scope::System => s.artifacts_dir = value,
        Scope::Project(id) => match value {
            Some(v) => {
                s.project_artifacts_dirs.insert(id.to_string(), v);
            }
            None => {
                s.project_artifacts_dirs.remove(id);
            }
        },
    }
}

fn relocate(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
    scope: Scope<'_>,
) -> HttpResponse {
    if !authority.is_owner() {
        return error(403, "only Owner authority may change artifact directories");
    }
    let body = match json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    if body.get("new_path").is_none() {
        return error(400, "`new_path` is required (null resets to the default)");
    }
    let body: RelocateBody = match serde_json::from_value(body) {
        Ok(body) => body,
        Err(e) => return error(400, format!("invalid body: {e}")),
    };
    let Some(home) = hub_home_dir(state) else {
        return error(500, "home directory unavailable");
    };
    let _relocating = match RELOCATE_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => {
            return error(409, "another artifact directory relocation is running");
        }
    };
    if let Err(why) = settings::load_attachment_retention_strict(&home) {
        return error(
            409,
            format!("the stored settings file is damaged ({why}); repair it first"),
        );
    }

    let before = settings::load(&home);
    let old = match scope {
        Scope::System => system_dir_in(state, &before),
        Scope::Project(id) => project_dir_in(state, &before, id),
    };
    let default = match scope {
        Scope::System => default_system_dir(state),
        Scope::Project(id) => system_dir_in(state, &before).join(id),
    };
    let reset = body.new_path.as_deref().is_none_or(|p| p.trim().is_empty());
    let new = if reset {
        let resolved = resolve_path(&default).unwrap_or_else(|_| default.clone());
        match check_relation(&resolved, &old) {
            Ok(()) => resolved,
            Err(why) => return error(400, why),
        }
    } else {
        let raw = body.new_path.as_deref().unwrap_or("");
        match validate_target(state, raw, &old, scope) {
            Ok(path) => path,
            Err(why) => return error(400, why),
        }
    };
    let old_resolved = resolve_path(&old).unwrap_or_else(|_| old.clone());

    // The registry lock is held across the move and the setting change, so a registry write
    // cannot land in the old directory in between.
    let outcome = artifacts::with_registry_lock(|| {
        let migrated = if body.migrate && old_resolved != new {
            migrate_dir(&old_resolved, &new)?
        } else {
            prepare_target(&new).map_err(MigrateError::Prepare)?;
            Migrated::default()
        };
        let saved = settings::update_artifact_dirs(&home, |s| {
            apply_setting(s, scope, (!reset).then_some(new.as_path()));
        });
        match saved {
            Ok(updated) => Ok((migrated, updated)),
            Err(e) => {
                let rolled_back = rollback(&old_resolved, &new, &migrated.moved);
                let message = match e {
                    settings::UpdateError::Damaged(why) => {
                        format!("the stored settings file is damaged ({why})")
                    }
                    settings::UpdateError::Io(e) => format!("could not save settings: {e}"),
                    settings::UpdateError::Invalid(why) => why,
                };
                Err(MigrateError::Failed {
                    message,
                    files_moved: migrated.files,
                    rolled_back,
                })
            }
        }
    });

    match outcome {
        Ok((migrated, updated)) => {
            let view = settings_view(state, &updated);
            state.broadcast("settings", "settings.updated", view);
            let mut out = json!({
                "old_path": display(&old),
                "new_path": display(&new),
                "migrated_files": migrated.files,
                "migrated_bytes": migrated.bytes,
            });
            let shown = match scope {
                Scope::System => json!({
                    "artifacts_dir": updated.artifacts_dir,
                    "default_artifacts_dir": display(&default_system_dir(state)),
                    "effective_artifacts_dir": display(&system_dir_in(state, &updated)),
                }),
                Scope::Project(id) => {
                    let mut v = project_view(state, &updated, id);
                    if let Some(map) = v.as_object_mut() {
                        map.remove("id");
                    }
                    v
                }
            };
            if let (Some(out), Some(shown)) = (out.as_object_mut(), shown.as_object()) {
                out.extend(shown.clone());
            }
            if let (Scope::Project(id), Some(out)) = (scope, out.as_object_mut()) {
                out.insert("project".into(), json!(id));
            }
            HttpResponse::json(200, out)
        }
        Err(MigrateError::Conflict(names)) => {
            let listed = names
                .iter()
                .take(MAX_CONFLICT_NAMES)
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let more = names.len().saturating_sub(MAX_CONFLICT_NAMES);
            let more = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            error(
                409,
                format!(
                    "{} already holds {listed}{more}; nothing was moved",
                    new.display()
                ),
            )
        }
        Err(MigrateError::Prepare(why)) => error(400, why),
        Err(MigrateError::Failed {
            message,
            files_moved,
            rolled_back,
        }) => {
            let tail = if rolled_back {
                "the moved entries were put back"
            } else {
                "not every moved entry could be put back; look in both directories"
            };
            error(
                500,
                format!(
                    "{message}; {files_moved} file(s) had already been moved ({tail}); the \
                     setting was not changed"
                ),
            )
        }
    }
}

/// `POST /api/v1/settings/artifacts-dir`. Owner only.
pub(crate) fn handle_post_system_dir(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    relocate(state, authority, req, Scope::System)
}

/// The normalized slug for a path segment, or the error response. `404` for a slug that is not
/// an available project.
fn project_slug(state: &HubState, raw: &str) -> Result<String, HttpResponse> {
    let decoded = percent_decode(raw);
    if decoded.trim().is_empty() {
        return Err(error(400, "project id is empty"));
    }
    let slug = records::normalize_project(Some(&decoded)).map_err(|e| error(400, e))?;
    let home = hub_home_dir(state);
    if records::available_projects(state, home.as_deref()).contains(&slug) {
        Ok(slug)
    } else {
        Err(error(404, format!("no project `{slug}`")))
    }
}

/// `GET /api/v1/projects`.
pub(crate) fn handle_get_projects(state: &HubState) -> HttpResponse {
    let s = current_settings(state);
    let home = hub_home_dir(state);
    let projects: Vec<Value> = records::available_projects(state, home.as_deref())
        .iter()
        .map(|id| project_view(state, &s, id))
        .collect();
    HttpResponse::json(200, json!({ "projects": projects }))
}

/// `GET /api/v1/projects/:id/artifacts-dir`.
pub(crate) fn handle_get_project_dir(state: &HubState, raw_id: &str) -> HttpResponse {
    match project_slug(state, raw_id) {
        Ok(id) => HttpResponse::json(200, project_view(state, &current_settings(state), &id)),
        Err(resp) => resp,
    }
}

/// `POST /api/v1/projects/:id/artifacts-dir`. Owner only.
pub(crate) fn handle_post_project_dir(
    state: &HubState,
    authority: &HubAuthority,
    raw_id: &str,
    req: &HttpRequest,
) -> HttpResponse {
    if !authority.is_owner() {
        return error(403, "only Owner authority may change artifact directories");
    }
    match project_slug(state, raw_id) {
        Ok(id) => relocate(state, authority, req, Scope::Project(&id)),
        Err(resp) => resp,
    }
}

// --- serving and startup ----------------------------------------------------------------------

/// The directories `GET /api/v1/artifacts/preview` and friends may serve from, beyond the
/// repo: the default and effective system directories (everything in them except the
/// registry's own `registry.json*` and `versions/`) and each project override. A root that
/// resolves into a protected place is dropped, so an override never makes one servable.
pub(crate) fn serving_roots(state: &HubState) -> Vec<PathBuf> {
    let s = current_settings(state);
    let mut systems = vec![default_system_dir(state), system_dir_in(state, &s)];
    systems.dedup();
    let mut roots = Vec::new();
    for dir in systems {
        let Ok(canon) = fs::canonicalize(&dir) else {
            continue;
        };
        if sensitive_reason(state, &canon).is_some() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&canon) else {
            continue;
        };
        for entry in entries.take(MAX_SERVING_ENTRIES).flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let internal = name.starts_with(".tmp-")
                || name == REGISTRY_FILE
                || name.starts_with(&format!("{REGISTRY_FILE}."))
                || name == VERSIONS_DIR;
            // A symlink inside the directory could point anywhere.
            if !internal && entry.file_type().is_ok_and(|t| !t.is_symlink()) {
                roots.push(entry.path());
            }
        }
    }
    // The registry's own files in any system directory: no project root may serve them.
    let registry_paths: Vec<PathBuf> = [default_system_dir(state), system_dir_in(state, &s)]
        .iter()
        .flat_map(|dir| {
            let dir = resolve_path(dir).unwrap_or_else(|_| dir.clone());
            [dir.join(REGISTRY_FILE), dir.join(VERSIONS_DIR)]
        })
        .collect();
    for dir in s.project_artifacts_dirs.values() {
        let Some(dir) = usable_stored(state, dir) else {
            continue;
        };
        let Ok(canon) = fs::canonicalize(&dir) else {
            continue;
        };
        let reaches_registry = registry_paths
            .iter()
            .any(|p| p.starts_with(&canon) || canon.starts_with(p));
        if !reaches_registry {
            roots.push(canon);
        }
    }
    roots
}

/// Best-effort creation of the hub's directories at startup. A failure is logged and ignored.
pub(crate) fn ensure_startup_dirs(state: &HubState) {
    let data = hub_data_home(state);
    for dir in [
        data.join("artifacts"),
        data.join("attachments"),
        effective_system_dir(state),
    ] {
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("fl-hub: could not create {}: {e}", dir.display());
        }
    }
}
