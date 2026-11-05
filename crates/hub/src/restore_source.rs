//! The tmux restore source: the tmux-resurrect save file a host boot restores sessions from.
//!
//! A decommission stops the retired agent's tmux runtime. The runtime also sits in the newest
//! resurrect save, and a boot restore (`@continuum-restore on`) would start it again, running the
//! agent's vendor CLI in the dead persona's name. The prune removes what was stopped from that
//! file, so nothing is left to restore. What goes matches what was stopped:
//!
//! - [`prune_session_in`]: a whole session that is gone.
//! - [`prune_window_in`]: one window of a session that stays (a window of the shared `muse`
//!   session). Its `window` line goes, and so do the `pane` lines of that window; the session's
//!   other windows, its `state` lines and its `grouped_session` lines stay. A `window` line
//!   names the window (field 4, `:<name>`) and carries its `window_index` (field 3); a `pane`
//!   line carries only the index, so the name is mapped to the index first.
//!
//! The hub edits the file itself and does not run the plugin's `save.sh`. A save needs a live tmux
//! server, rewrites the state of every other session, races the periodic continuum save, and is
//! code outside this repository. The prune is one pure file edit: it reads the newest save, drops
//! the lines, writes a temp file beside it (with the permissions of the save, flushed to disk) and
//! renames it over the save (the `last` link keeps pointing at it), and keeps the content from
//! before the first prune as `<save>.bak` (an existing backup is kept, not overwritten). A backup
//! that cannot be written stops the prune. The directory keeps the newest five
//! `tmux_resurrect_*.txt.bak` files; older ones are removed, and files of other names are never
//! touched.
//!
//! In a session prune, `pane`, `window` and `grouped_session` lines of the session are removed; a
//! `state` line (the client's session and last session) that names it only has that field
//! emptied, so the client's other session survives. A prune is idempotent (a file without the
//! scope is not touched) and never fails the caller: the result says what happened.
//!
//! Residual race: a continuum save that started before the runtime was stopped can finish after
//! the rename and write it back. The hub does not lock the plugin out; the next prune of the
//! same scope (or the retired persona's orphan report at the next boot) deals with it.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What a prune did, for the decommission result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RestoreSourceOutcome {
    /// The newest save held the session; `lines` lines were removed or (a `state` line) rewritten.
    Pruned { file: String, lines: usize },
    /// The session still runs, so something can save it again: the save was left alone. This is
    /// the result for a running session the hub did not start; the hub neither stops nor prunes
    /// it.
    Running,
    /// The newest save does not hold the session (already pruned, or never saved).
    Clean { file: String },
    /// There is no save to prune (no plugin, no resurrect directory, or no save file yet).
    NoSave,
    /// The save could not be read or rewritten. The decommission goes on.
    Failed { error: String },
}

/// The resurrect directory tmux-resurrect uses on this host: `FL_TMUX_RESURRECT_DIR` when set
/// (and `home` is `None`), else the plugin's own default under `home` (`$HOME` when `None`):
/// `~/.tmux/resurrect` when that directory exists, else `$XDG_DATA_HOME/tmux/resurrect`
/// (`~/.local/share/tmux/resurrect`). An `@resurrect-dir` set in `tmux.conf` is not read; set
/// `FL_TMUX_RESURRECT_DIR` for it.
///
/// A home override is a hermetic home: the process's own `$HOME`, `$XDG_DATA_HOME` and
/// `FL_TMUX_RESURRECT_DIR` stay out.
pub fn resurrect_dir(home: Option<&Path>) -> Option<PathBuf> {
    resurrect_dir_for(std::env::var_os("FL_TMUX_RESURRECT_DIR"), home)
}

/// [`resurrect_dir`] with the value of `FL_TMUX_RESURRECT_DIR` given.
fn resurrect_dir_for(env_dir: Option<std::ffi::OsString>, home: Option<&Path>) -> Option<PathBuf> {
    if home.is_none()
        && let Some(dir) = env_dir.filter(|d| !d.is_empty())
    {
        return Some(PathBuf::from(dir));
    }
    let (home, xdg) = match home {
        Some(h) => (h.to_path_buf(), None),
        None => (
            PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?),
            std::env::var_os("XDG_DATA_HOME").filter(|x| !x.is_empty()),
        ),
    };
    let legacy = home.join(".tmux").join("resurrect");
    if legacy.is_dir() {
        return Some(legacy);
    }
    let data = xdg.map_or_else(|| home.join(".local").join("share"), PathBuf::from);
    Some(data.join("tmux").join("resurrect"))
}

/// The file the next restore reads: what the `last` link names when that file is inside `dir`,
/// else the newest `tmux_resurrect_*.txt` by name (the names carry their timestamp). A link that
/// points outside `dir` is not followed: the hub edits only files of the resurrect directory.
fn newest_save(dir: &Path) -> Option<PathBuf> {
    if let Ok(target) = fs::read_link(dir.join("last"))
        && let (Ok(root), Ok(file)) = (fs::canonicalize(dir), fs::canonicalize(dir.join(target)))
        && file.is_file()
        && file.parent() == Some(root.as_path())
    {
        return Some(dir.join(file.file_name()?));
    }
    let mut saves: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("tmux_resurrect_") && n.ends_with(".txt"))
        })
        .collect();
    saves.sort();
    saves.pop()
}

/// What a prune does to one resurrect line.
#[derive(Debug, PartialEq, Eq)]
enum LineEdit {
    Keep,
    Drop,
    /// The line body with its `state` fields that name the session emptied.
    Rewrite(Vec<u8>),
}

/// What a prune removes: a whole session, or one window of a session that stays.
#[derive(Debug, Clone, Copy)]
enum Scope<'a> {
    Session(&'a str),
    Window { session: &'a str, window: &'a str },
}

/// What to do with the resurrect line `body` (tab separated, no line ending; the first field is
/// its kind) for `session`. `pane` and `window` lines carry the session in field 2 and are
/// dropped; `grouped_session` carries the session and the one it groups with in fields 2 and 3
/// and is dropped when either is the session; `state` carries the client's session and last
/// session in fields 2 and 3, and only the field that names the session is emptied. Bytes, so a
/// name in another encoding survives untouched.
fn edit_line(body: &[u8], session: &str) -> LineEdit {
    let session = session.as_bytes();
    let mut fields = body.split(|b| *b == b'\t');
    match fields.next() {
        Some(b"pane" | b"window") if fields.next() == Some(session) => LineEdit::Drop,
        Some(b"grouped_session") if fields.take(2).any(|f| f == session) => LineEdit::Drop,
        Some(b"state") => {
            let mut changed = false;
            let parts: Vec<&[u8]> = body
                .split(|b| *b == b'\t')
                .enumerate()
                .map(|(i, f)| {
                    if (1..=2).contains(&i) && f == session {
                        changed = true;
                        &[][..]
                    } else {
                        f
                    }
                })
                .collect();
            if changed {
                LineEdit::Rewrite(parts.join(&b'\t'))
            } else {
                LineEdit::Keep
            }
        }
        _ => LineEdit::Keep,
    }
}

/// The `window_index` of every window line of `session` whose name is `window`. A `window` line
/// is `window <session> <index> :<name> ...`; the `pane` lines of the window carry only the
/// index, so the name has to be mapped to it first.
fn window_indices<'t>(text: &'t [u8], session: &str, window: &str) -> Vec<&'t [u8]> {
    let name = format!(":{window}");
    text.split(|b| *b == b'\n')
        .filter_map(|line| {
            let mut f = line.split(|b| *b == b'\t');
            let (kind, sess, index, wname) = (f.next()?, f.next()?, f.next()?, f.next()?);
            let wname = wname.strip_suffix(b"\r").unwrap_or(wname);
            (kind == b"window" && sess == session.as_bytes() && wname == name.as_bytes())
                .then_some(index)
        })
        .collect()
}

/// What to do with the resurrect line `body` when only the window `window` (named by `name`, whose
/// `window_index` is one of `indices`) of `session` goes: its `window` line and its `pane` lines
/// are dropped; every other line (other windows, `state`, `grouped_session`) is kept.
fn edit_window_line(body: &[u8], session: &str, name: &[u8], indices: &[&[u8]]) -> LineEdit {
    let session = session.as_bytes();
    let mut fields = body.split(|b| *b == b'\t');
    match fields.next() {
        Some(b"window") if fields.next() == Some(session) => {
            let _index = fields.next();
            let wname = fields.next().map(|w| w.strip_suffix(b"\r").unwrap_or(w));
            if wname == Some(name) {
                LineEdit::Drop
            } else {
                LineEdit::Keep
            }
        }
        Some(b"pane") if fields.next() == Some(session) => {
            if fields.next().is_some_and(|i| indices.contains(&i)) {
                LineEdit::Drop
            } else {
                LineEdit::Keep
            }
        }
        _ => LineEdit::Keep,
    }
}

/// Most `.bak` files a prune keeps in the directory (the newest by name, which carries the save's
/// timestamp). Every continuum save has a new name and so would get its own backup.
const KEEP_BACKUPS: usize = 5;

/// Write `bytes` to `path` (replacing a stale file), with the permissions `mode` when given
/// (the file never exists with wider ones), and flush it to disk.
fn write_synced(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    use std::io::Write;

    let _ = fs::remove_file(path);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        // The umask may have narrowed the mode the file was created with.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    file.write_all(bytes)?;
    file.sync_all()
}

/// The permission bits of `path`.
fn mode_of(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Keep the newest [`KEEP_BACKUPS`] `tmux_resurrect_*.txt.bak` files of `dir`, and always
/// `current` (the backup of the save just pruned). Files of other names are never touched.
/// Best effort: a file that cannot be removed stays.
fn trim_backups(dir: &Path, current: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p != current
                && p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("tmux_resurrect_") && n.ends_with(".txt.bak"))
        })
        .collect();
    backups.sort();
    let others = KEEP_BACKUPS.saturating_sub(1);
    let surplus = backups.len().saturating_sub(others);
    for old in backups.into_iter().take(surplus) {
        let _ = fs::remove_file(old);
    }
}

/// Remove `scope` from the newest save in `dir`. See the module doc.
fn prune_in(dir: &Path, scope: Scope<'_>) -> RestoreSourceOutcome {
    let Some(file) = newest_save(dir) else {
        return RestoreSourceOutcome::NoSave;
    };
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fail = |what: &str, e: std::io::Error| RestoreSourceOutcome::Failed {
        error: format!("{what} {}: {e}", file.display()),
    };
    let text = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) => return fail("read", e),
    };
    let window_name = match scope {
        Scope::Window { window, .. } => format!(":{window}"),
        Scope::Session(_) => String::new(),
    };
    let indices = match scope {
        Scope::Window { session, window } => window_indices(&text, session, window),
        Scope::Session(_) => Vec::new(),
    };
    // Lossless: a line is dropped, rewritten or kept as bytes.
    let mut kept: Vec<u8> = Vec::with_capacity(text.len());
    let mut removed = 0usize;
    for line in text.split_inclusive(|b| *b == b'\n') {
        let eol = line
            .iter()
            .rev()
            .take_while(|b| matches!(**b, b'\n' | b'\r'))
            .count();
        let (body, ending) = line.split_at(line.len() - eol);
        let edit = match scope {
            Scope::Session(session) => edit_line(body, session),
            Scope::Window { session, .. } => {
                edit_window_line(body, session, window_name.as_bytes(), &indices)
            }
        };
        match edit {
            LineEdit::Keep => kept.extend_from_slice(line),
            LineEdit::Drop => removed += 1,
            LineEdit::Rewrite(new) => {
                removed += 1;
                kept.extend_from_slice(&new);
                kept.extend_from_slice(ending);
            }
        }
    }
    if removed == 0 {
        return RestoreSourceOutcome::Clean { file: name };
    }
    let mode = mode_of(&file);
    // The first backup is the content from before any prune; a later prune keeps it. A backup
    // that cannot be written stops the prune: nothing is rewritten without one.
    let backup = file.with_file_name(format!("{name}.bak"));
    if !backup.exists()
        && let Err(e) = write_synced(&backup, &text, mode)
    {
        return fail("back up to", e);
    }
    let temp = file.with_file_name(format!(".{name}.prune.tmp"));
    let written = write_synced(&temp, &kept, mode).and_then(|()| fs::rename(&temp, &file));
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return fail("rewrite", e);
    }
    trim_backups(dir, &backup);
    RestoreSourceOutcome::Pruned {
        file: name,
        lines: removed,
    }
}

/// Remove `session` from the newest save in `dir`. See the module doc.
pub fn prune_session_in(dir: &Path, session: &str) -> RestoreSourceOutcome {
    prune_in(dir, Scope::Session(session))
}

/// Remove only the window `window` of `session` from the newest save in `dir`: its `window` line
/// and its `pane` lines. The session's other windows, its `state` lines and its
/// `grouped_session` lines stay. See the module doc.
pub fn prune_window_in(dir: &Path, session: &str, window: &str) -> RestoreSourceOutcome {
    prune_in(dir, Scope::Window { session, window })
}

/// [`prune_session_in`] on the host's resurrect directory ([`resurrect_dir`]).
pub fn prune_session(home: Option<&Path>, session: &str) -> RestoreSourceOutcome {
    match resurrect_dir(home) {
        Some(dir) => prune_session_in(&dir, session),
        None => RestoreSourceOutcome::NoSave,
    }
}

/// [`prune_window_in`] on the host's resurrect directory ([`resurrect_dir`]).
pub fn prune_window(home: Option<&Path>, session: &str, window: &str) -> RestoreSourceOutcome {
    match resurrect_dir(home) {
        Some(dir) => prune_window_in(&dir, session, window),
        None => RestoreSourceOutcome::NoSave,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_home_override_ignores_the_resurrect_dir_variable() {
        let env = Some(std::ffi::OsString::from("/real/resurrect"));
        let home = Path::new("/hermetic/home");
        assert_eq!(
            resurrect_dir_for(env.clone(), Some(home)),
            Some(home.join(".local/share/tmux/resurrect")),
            "the variable stays out of a hermetic home"
        );
        assert_eq!(
            resurrect_dir_for(env, None),
            Some(PathBuf::from("/real/resurrect")),
            "without an override the variable wins"
        );
        assert_eq!(
            resurrect_dir_for(Some(std::ffi::OsString::new()), Some(home)),
            Some(home.join(".local/share/tmux/resurrect"))
        );
    }
}
