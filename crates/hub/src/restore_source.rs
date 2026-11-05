//! The tmux restore source: the tmux-resurrect save file a host boot restores sessions from.
//!
//! A decommission kills the retired agent's tmux session. The session also sits in the newest
//! resurrect save, and a boot restore (`@continuum-restore on`) would start it again, running the
//! agent's vendor CLI in the dead persona's name. [`prune_session`] removes the session from that
//! file, so nothing is left to restore.
//!
//! The hub edits the file itself and does not run the plugin's `save.sh`. A save needs a live tmux
//! server, rewrites the state of every other session, races the periodic continuum save, and is
//! code outside this repository. The prune is one pure file edit: it reads the newest save, drops
//! the lines that name the session, writes a temp file beside it and renames it over the save
//! (the `last` link keeps pointing at it), and keeps the old content as `<save>.bak`. It is
//! idempotent (a file without the session is not touched) and never fails the caller: the result
//! says what happened.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What a prune did, for the decommission result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RestoreSourceOutcome {
    /// The newest save held the session; `lines` lines were removed.
    Pruned { file: String, lines: usize },
    /// The newest save does not hold the session (already pruned, or never saved).
    Clean { file: String },
    /// There is no save to prune (no plugin, no resurrect directory, or no save file yet).
    NoSave,
    /// The save could not be read or rewritten. The decommission goes on.
    Failed { error: String },
}

/// The resurrect directory tmux-resurrect uses on this host: `FL_TMUX_RESURRECT_DIR` when set,
/// else the plugin's own default under `home` (`$HOME` when `None`): `~/.tmux/resurrect` when
/// that directory exists, else `$XDG_DATA_HOME/tmux/resurrect` (`~/.local/share/tmux/resurrect`).
/// An `@resurrect-dir` set in `tmux.conf` is not read; set `FL_TMUX_RESURRECT_DIR` for it.
pub fn resurrect_dir(home: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FL_TMUX_RESURRECT_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    // A home override is a hermetic home: the process's own `$HOME` and `$XDG_DATA_HOME` stay out.
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

/// The file the next restore reads: what the `last` link names, else the newest
/// `tmux_resurrect_*.txt` by name (the names carry their timestamp).
fn newest_save(dir: &Path) -> Option<PathBuf> {
    if let Ok(target) = fs::read_link(dir.join("last")) {
        let file = dir.join(target.file_name()?);
        if file.is_file() {
            return Some(file);
        }
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

/// Whether the resurrect line `line` (tab separated; the first field is its kind) names `session`.
/// `pane` and `window` lines carry the session in field 2; `grouped_session` carries the session
/// and the one it groups with in fields 2 and 3; `state` carries the client's session and the
/// last session in fields 2 and 3.
fn line_names_session(line: &str, session: &str) -> bool {
    let mut fields = line.split('\t');
    match fields.next() {
        Some("pane" | "window") => fields.next() == Some(session),
        Some("grouped_session" | "state") => fields.take(2).any(|f| f == session),
        _ => false,
    }
}

/// Remove `session` from the newest save in `dir`. See the module doc.
pub fn prune_session_in(dir: &Path, session: &str) -> RestoreSourceOutcome {
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
    // Lossless: a line is dropped or kept as bytes, so a name in another encoding survives.
    let mut kept: Vec<u8> = Vec::with_capacity(text.len());
    let mut removed = 0usize;
    for line in text.split_inclusive(|b| *b == b'\n') {
        let as_text = String::from_utf8_lossy(line);
        if line_names_session(as_text.trim_end_matches(['\n', '\r']), session) {
            removed += 1;
        } else {
            kept.extend_from_slice(line);
        }
    }
    if removed == 0 {
        return RestoreSourceOutcome::Clean { file: name };
    }
    let backup = file.with_file_name(format!("{name}.bak"));
    if let Err(e) = fs::write(&backup, &text) {
        return fail("back up to", e);
    }
    let temp = file.with_file_name(format!(".{name}.prune.tmp"));
    let written = fs::write(&temp, &kept).and_then(|()| {
        if let Ok(meta) = fs::metadata(&file) {
            let _ = fs::set_permissions(&temp, meta.permissions());
        }
        fs::rename(&temp, &file)
    });
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return fail("rewrite", e);
    }
    RestoreSourceOutcome::Pruned {
        file: name,
        lines: removed,
    }
}

/// [`prune_session_in`] on the host's resurrect directory ([`resurrect_dir`]).
pub fn prune_session(home: Option<&Path>, session: &str) -> RestoreSourceOutcome {
    match resurrect_dir(home) {
        Some(dir) => prune_session_in(&dir, session),
        None => RestoreSourceOutcome::NoSave,
    }
}
