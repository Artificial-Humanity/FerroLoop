//! Helpers for constructing isolated CLI integration-test commands.
//!
//! This is a fixture convention, not a process sandbox. The caller must own
//! `root` and may explicitly override values for the fixture under test. The
//! helper does not isolate an in-process daemon or mediate direct syscalls.

use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Component, Path};
use std::process::Command;

/// A short, valid Windows host pipe suffix derived from the fixture root.
/// Roots should come from distinct per-test temporary directories.
pub fn windows_pipe_suffix(root: &Path) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hash);
    format!("fixture-{:016x}", hash.finish())
}

/// Configure an existing command. Calling `env_clear` here also removes any
/// variables set on it before isolation, not only its inherited environment.
pub fn isolate_command(command: &mut Command, root: impl AsRef<Path>) -> io::Result<()> {
    let root = root.as_ref();
    if !root.is_absolute() || root.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture root must be absolute without parent components",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        // Portable across the Linux and macOS Unix socket path limits. All
        // default socket names below have one-character basenames.
        if root.join("h").as_os_str().as_bytes().len() >= 104
            || root.as_os_str().as_bytes().contains(&0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixture socket path is too long or contains NUL",
            ));
        }
    }
    command.env_clear();
    for name in [
        "HOME",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "TMPDIR",
        "TEMP",
        "TMP",
    ] {
        command.env(name, root);
    }
    command.env("PATH", root.join("no-bin"));
    command.env("FL_WIRE_SOCKET", root.join("w"));
    command.env("FL_WIRE_ROUTES", root.join("routes.json"));
    command.env("FL_WIRE_CODEX_CONTROL_SOCK", root.join("c"));
    command.env("FL_TEST_RUN_USER_DIR", root.join("run-user"));
    command.env("FL_DB", root.join("fl.fls"));
    command.env("FL_HUB_URL", "http://127.0.0.1:0");
    command.env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1");
    #[cfg(unix)]
    command.env("FL_HOST_SOCK", root.join("h"));
    #[cfg(windows)]
    command.env("FL_HOST_SOCK", windows_pipe_suffix(root));
    for name in [
        "FL_WIRE_TMUX_BIN",
        "FL_WIRE_CLAUDE_BIN",
        "FL_WIRE_CODEX_BIN",
        "FL_WIRE_AGY_BIN",
        "FL_WIRE_MUSE_BIN",
        "FL_WIRE_GROK_BIN",
        "FL_WIRE_LEMONADE_BIN",
        "FL_WIRE_OLLAMA_BIN",
        "FL_HUB_CLAUDE_BIN",
        "FL_HUB_MUSE_BIN",
        "FL_HUB_AGENT_HYDRATE_BIN",
        "FL_BROWSE_ENGINE_BIN",
    ] {
        command.env(name, root.join("no-bin").join(name));
    }
    Ok(())
}

pub fn isolated_command(program: impl AsRef<OsStr>, root: impl AsRef<Path>) -> io::Result<Command> {
    let mut command = Command::new(program);
    isolate_command(&mut command, root)?;
    Ok(command)
}
