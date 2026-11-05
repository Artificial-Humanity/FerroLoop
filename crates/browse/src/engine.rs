//! Tier 2: an optional headless browser binary.
//!
//! [`HeadlessEngineProbe`] looks for one; [`dump_dom`] runs it once on a page and returns the
//! rendered DOM for the distiller (and, when asked, writes a screenshot). The binary must take
//! Chromium's command line: the flags are `--headless --dump-dom --window-size=W,H`
//! (`--screenshot=PATH`).
//!
//! The binary is found, in order, at `$FL_BROWSE_ENGINE_BIN` (an explicit path; one that is not
//! an executable file is an error, never a silent fall-through; the value `none` switches
//! detection off) and then under these names in
//! each `PATH` directory and in a few standard install directories: see [`ENGINE_CANDIDATES`].
//!
//! The process is started without a shell. Its arguments are separate argv entries, the target
//! follows a `--`, and the screenshot path sits inside one `--screenshot=` entry, so neither can
//! be read as a flag. It gets a cleared environment (only `PATH`, and a `HOME` that is a scratch
//! directory it also uses as its profile, removed afterwards), no stdin, a time limit after
//! which it is killed, and a cap on the output read.

use crate::{BrowseError, Viewport};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// The environment variable that names the engine binary.
pub const ENGINE_ENV: &str = "FL_BROWSE_ENGINE_BIN";
/// The value of [`ENGINE_ENV`] that switches engine detection off: no binary is looked for.
pub const ENGINE_DISABLED: &str = "none";

/// Binary names looked for, in order of preference.
pub const ENGINE_CANDIDATES: &[&str] = &[
    "lightpanda",
    "chromium-headless-shell",
    "headless_shell",
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
];

/// Directories searched after `PATH`.
pub const STANDARD_DIRS: &[&str] = &[
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/snap/bin",
    "/opt/google/chrome",
    "/opt/chromium",
];

/// How long a dump may take by default.
pub const DEFAULT_ENGINE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `--version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// The most DOM text read from the engine.
const MAX_DOM_BYTES: usize = crate::html::MAX_HTML_BYTES;
const STDERR_KEEP: usize = 2048;

/// A detected engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInfo {
    pub path: PathBuf,
    /// The file name of the binary.
    pub name: String,
    /// Whether `$FL_BROWSE_ENGINE_BIN` named it.
    pub from_env: bool,
}

/// Where to look for an engine. [`HeadlessEngineProbe::from_env`] reads the process
/// environment; tests build one by hand.
#[derive(Debug, Clone, Default)]
pub struct HeadlessEngineProbe {
    /// The value of `$FL_BROWSE_ENGINE_BIN`.
    pub engine_bin: Option<OsString>,
    /// The value of `$PATH`.
    pub path: Option<OsString>,
    /// Directories searched after `PATH`.
    pub extra_dirs: Vec<PathBuf>,
}

fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl HeadlessEngineProbe {
    pub fn from_env() -> Self {
        Self {
            engine_bin: std::env::var_os(ENGINE_ENV),
            path: std::env::var_os("PATH"),
            extra_dirs: STANDARD_DIRS.iter().map(PathBuf::from).collect(),
        }
    }

    /// Find an engine. `Ok(None)` when none is installed; an error when
    /// `$FL_BROWSE_ENGINE_BIN` is set to something that cannot run.
    pub fn detect(&self) -> Result<Option<EngineInfo>, BrowseError> {
        if let Some(bin) = self.engine_bin.as_deref().filter(|b| !b.is_empty()) {
            if bin == ENGINE_DISABLED {
                return Ok(None);
            }
            let p = PathBuf::from(bin);
            if !is_executable_file(&p) {
                return Err(BrowseError::EngineBinInvalid(p.display().to_string()));
            }
            return Ok(Some(info(p, true)));
        }
        let mut dirs: Vec<PathBuf> = self
            .path
            .as_deref()
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default();
        dirs.extend(self.extra_dirs.iter().cloned());
        let mut seen = std::collections::BTreeSet::new();
        dirs.retain(|d| !d.as_os_str().is_empty() && d.is_absolute() && seen.insert(d.clone()));
        for name in ENGINE_CANDIDATES {
            for d in &dirs {
                let p = d.join(name);
                if is_executable_file(&p) {
                    return Ok(Some(info(p, false)));
                }
            }
        }
        Ok(None)
    }
}

fn info(path: PathBuf, from_env: bool) -> EngineInfo {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("engine")
        .to_string();
    EngineInfo {
        path,
        name,
        from_env,
    }
}

/// What a bounded child process produced.
struct Captured {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

enum RunError {
    Spawn(std::io::Error),
    Timeout,
    Overflow,
}

/// Spawn, retrying a few times on `ETXTBSY` (the binary was written a moment ago and another
/// thread's `fork` still holds a copy of its descriptor).
fn spawn_retrying(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    const ETXTBSY: i32 = 26;
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && tries < 10 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

/// Run `cmd` with piped output until it exits, `timeout` passes (the process is killed) or its
/// output passes `stdout_cap`.
fn run_bounded(
    mut cmd: Command,
    timeout: Duration,
    stdout_cap: usize,
) -> Result<Captured, RunError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_retrying(&mut cmd).map_err(RunError::Spawn)?;
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let overflow = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    {
        let (tx, overflow) = (tx.clone(), overflow.clone());
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut out).take(stdout_cap as u64 + 1).read_to_end(&mut buf);
            if buf.len() > stdout_cap {
                overflow.store(true, Ordering::SeqCst);
            }
            let _ = tx.send((true, buf));
        });
    }
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut err).take(STDERR_KEEP as u64).read_to_end(&mut buf);
        // Keep draining so the child never blocks on a full pipe.
        let _ = std::io::copy(&mut err, &mut std::io::sink());
        let _ = tx.send((false, buf));
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Spawn(e));
            }
        }
        if overflow.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunError::Overflow);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // The child is gone; its output is complete unless a grandchild holds the pipe.
    let (mut stdout, mut stderr) = (None, None);
    let grace = Instant::now() + Duration::from_secs(2);
    while stdout.is_none() || stderr.is_none() {
        let left = grace.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((true, b)) => stdout = Some(b),
            Ok((false, b)) => stderr = Some(b),
            Err(_) => break,
        }
    }
    let stdout = stdout.unwrap_or_default();
    if stdout.len() > stdout_cap {
        return Err(RunError::Overflow);
    }
    Ok(Captured {
        success: status.success(),
        stdout,
        stderr: stderr.unwrap_or_default(),
    })
}

/// The first line of `<engine> --version`, or `None` when it did not answer in time.
pub fn engine_version(engine: &EngineInfo) -> Option<String> {
    let mut cmd = Command::new(&engine.path);
    cmd.arg("--version").env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    let out = run_bounded(cmd, VERSION_TIMEOUT, 4096).ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(crate::distill::normalize(line, 200))
}

/// The arguments for one dump. `target` is a URL; it comes after `--`.
pub fn dump_args(
    profile: &Path,
    viewport: Viewport,
    screenshot: Option<&Path>,
    target: &str,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--headless".into(),
        "--dump-dom".into(),
        format!("--window-size={},{}", viewport.width, viewport.height).into(),
    ];
    let mut profile_arg = OsString::from("--user-data-dir=");
    profile_arg.push(profile);
    args.push(profile_arg);
    if let Some(p) = screenshot {
        let mut a = OsString::from("--screenshot=");
        a.push(p);
        args.push(a);
    }
    args.push("--".into());
    args.push(target.into());
    args
}

/// Render `target_url` in the engine and return the serialized DOM. `screenshot`, when given,
/// must be an absolute path whose directory exists; the file must exist afterwards.
pub fn dump_dom(
    engine: &EngineInfo,
    target_url: &str,
    viewport: Viewport,
    screenshot: Option<&Path>,
    timeout: Duration,
) -> Result<String, BrowseError> {
    let scratch = tempfile::Builder::new()
        .prefix("fl-browse-")
        .tempdir()
        .map_err(|e| {
            BrowseError::EngineFailed(format!("could not make a scratch directory: {e}"))
        })?;
    let mut cmd = Command::new(&engine.path);
    cmd.args(dump_args(scratch.path(), viewport, screenshot, target_url))
        .env_clear()
        .env("HOME", scratch.path())
        .current_dir(scratch.path());
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    let out = run_bounded(cmd, timeout, MAX_DOM_BYTES).map_err(|e| match e {
        RunError::Spawn(e) => {
            BrowseError::EngineFailed(format!("could not start {}: {e}", engine.path.display()))
        }
        RunError::Timeout => BrowseError::EngineFailed(format!(
            "{} did not finish within {} s and was killed",
            engine.path.display(),
            timeout.as_secs_f64()
        )),
        RunError::Overflow => BrowseError::EngineFailed(format!(
            "{} produced more than {MAX_DOM_BYTES} bytes of DOM and was killed",
            engine.path.display()
        )),
    })?;
    if !out.success {
        let tail = String::from_utf8_lossy(&out.stderr);
        return Err(BrowseError::EngineFailed(format!(
            "{} exited with an error: {}",
            engine.path.display(),
            crate::distill::normalize(&tail, 300)
        )));
    }
    if out.stdout.iter().all(u8::is_ascii_whitespace) {
        return Err(BrowseError::EngineFailed(format!(
            "{} printed no DOM",
            engine.path.display()
        )));
    }
    if let Some(p) = screenshot
        && !p.is_file()
    {
        return Err(BrowseError::EngineFailed(format!(
            "{} did not write the screenshot to {}",
            engine.path.display(),
            p.display()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn probe(dir: &Path) -> HeadlessEngineProbe {
        HeadlessEngineProbe {
            engine_bin: None,
            path: Some(dir.into()),
            extra_dirs: vec![],
        }
    }

    const VP: Viewport = Viewport {
        width: 320,
        height: 568,
    };

    #[test]
    fn nothing_installed_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(probe(dir.path()).detect().unwrap(), None);
        assert_eq!(HeadlessEngineProbe::default().detect().unwrap(), None);
    }

    #[test]
    fn finds_each_candidate_name_on_path() {
        for name in ENGINE_CANDIDATES {
            let dir = tempfile::tempdir().unwrap();
            let p = script(dir.path(), name, "exit 0");
            let found = probe(dir.path()).detect().unwrap().unwrap();
            assert_eq!(found.path, p);
            assert_eq!(found.name, *name);
            assert!(!found.from_env);
        }
    }

    #[test]
    fn candidate_order_beats_directory_order() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        script(a.path(), "google-chrome", "exit 0");
        let panda = script(b.path(), "lightpanda", "exit 0");
        let pr = HeadlessEngineProbe {
            engine_bin: None,
            path: Some(std::env::join_paths([a.path(), b.path()]).unwrap()),
            extra_dirs: vec![],
        };
        assert_eq!(pr.detect().unwrap().unwrap().path, panda);
    }

    #[test]
    fn a_non_executable_or_relative_path_entry_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chromium");
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(probe(dir.path()).detect().unwrap(), None);
        let rel = HeadlessEngineProbe {
            engine_bin: None,
            path: Some(".".into()),
            extra_dirs: vec![],
        };
        assert_eq!(
            rel.detect().unwrap(),
            None,
            "a relative PATH entry is never searched"
        );
    }

    #[test]
    fn standard_dirs_are_searched_after_path() {
        let a = tempfile::tempdir().unwrap();
        let std_dir = tempfile::tempdir().unwrap();
        let p = script(std_dir.path(), "chromium", "exit 0");
        let pr = HeadlessEngineProbe {
            engine_bin: None,
            path: Some(a.path().into()),
            extra_dirs: vec![std_dir.path().into()],
        };
        assert_eq!(pr.detect().unwrap().unwrap().path, p);
    }

    #[test]
    fn the_env_override_wins_and_a_bad_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        script(dir.path(), "chromium", "exit 0");
        let fake = script(dir.path(), "my-engine", "exit 0");
        let mut pr = probe(dir.path());
        pr.engine_bin = Some(fake.clone().into());
        let found = pr.detect().unwrap().unwrap();
        assert_eq!((found.path, found.from_env), (fake, true));

        pr.engine_bin = Some(dir.path().join("nope").into());
        assert!(matches!(pr.detect(), Err(BrowseError::EngineBinInvalid(_))));
        pr.engine_bin = Some(dir.path().into());
        assert!(matches!(pr.detect(), Err(BrowseError::EngineBinInvalid(_))));
        // `none` switches detection off, even with a candidate on PATH.
        pr.engine_bin = Some("none".into());
        assert_eq!(pr.detect().unwrap(), None);
        // An empty value is the same as unset.
        pr.engine_bin = Some("".into());
        assert_eq!(pr.detect().unwrap().unwrap().name, "chromium");
    }

    #[test]
    fn version_is_the_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "chromium",
            "echo; echo 'Chromium 126.0 fake'; echo more",
        );
        assert_eq!(
            engine_version(&info(p, false)).as_deref(),
            Some("Chromium 126.0 fake")
        );
        let bad = script(dir.path(), "chrome2", "exit 3");
        let _ = engine_version(&info(bad, false));
    }

    #[test]
    fn args_are_separate_entries_and_the_target_follows_a_double_dash() {
        let args = dump_args(
            Path::new("/p"),
            VP,
            Some(Path::new("/tmp/-evil flag.png")),
            "file:///tmp/x.html",
        );
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            vec![
                "--headless",
                "--dump-dom",
                "--window-size=320,568",
                "--user-data-dir=/p",
                "--screenshot=/tmp/-evil flag.png",
                "--",
                "file:///tmp/x.html"
            ]
        );
    }

    #[test]
    fn dumps_the_dom_the_engine_prints_and_passes_the_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "chromium",
            r#"printf '<html><body><button>%s</button></body></html>' "$*""#,
        );
        let html = dump_dom(
            &info(p, false),
            "http://127.0.0.1:1/x",
            VP,
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(
            html.contains("--headless --dump-dom --window-size=320,568"),
            "{html}"
        );
        assert!(html.contains("-- http://127.0.0.1:1/x"), "{html}");
        assert!(!html.contains("--screenshot"));
    }

    #[test]
    fn the_engine_gets_a_cleared_environment_and_a_scratch_home() {
        let dir = tempfile::tempdir().unwrap();
        // `cargo test` sets CARGO_MANIFEST_DIR in this process; the engine must not see it.
        let p = script(
            dir.path(),
            "chromium",
            r#"printf '<p>%s|%s</p>' "${CARGO_MANIFEST_DIR:-unset}" "${HOME}""#,
        );
        let html = dump_dom(
            &info(p, false),
            "http://127.0.0.1:1/",
            VP,
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(html.starts_with("<p>unset|/"), "{html}");
        let home = html
            .trim_start_matches("<p>unset|")
            .trim_end_matches("</p>");
        assert!(home.contains("fl-browse-"), "{home}");
        assert!(
            !Path::new(home).exists(),
            "the scratch directory is removed"
        );
    }

    #[test]
    fn writes_a_screenshot_through_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "chromium",
            r#"for a in "$@"; do case "$a" in --screenshot=*) printf png > "${a#--screenshot=}";; esac; done
echo '<p>ok</p>'"#,
        );
        let shot = dir.path().join("out.png");
        let html = dump_dom(
            &info(p.clone(), false),
            "http://127.0.0.1:1/",
            VP,
            Some(&shot),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(html.contains("ok"));
        assert_eq!(std::fs::read(&shot).unwrap(), b"png");

        // An engine that does not write it is an error, not a silent skip.
        let lazy = script(dir.path(), "lazy", "echo '<p>ok</p>'");
        let missing = dir.path().join("missing.png");
        let e = dump_dom(
            &info(lazy, false),
            "http://127.0.0.1:1/",
            VP,
            Some(&missing),
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(
            e.to_string().contains("did not write the screenshot"),
            "{e}"
        );
    }

    #[test]
    fn a_failing_engine_reports_its_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "chromium", "echo 'no display' >&2; exit 7");
        let e = dump_dom(
            &info(p, false),
            "http://x/",
            VP,
            None,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(
            matches!(&e, BrowseError::EngineFailed(m) if m.contains("no display")),
            "{e}"
        );
    }

    #[test]
    fn an_engine_that_prints_nothing_fails() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "chromium", "exit 0");
        let e = dump_dom(
            &info(p, false),
            "http://x/",
            VP,
            None,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(e.to_string().contains("printed no DOM"), "{e}");
    }

    #[test]
    fn a_hung_engine_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "chromium", "exec sleep 30");
        let start = Instant::now();
        let e = dump_dom(
            &info(p, false),
            "http://x/",
            VP,
            None,
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(e.to_string().contains("killed"), "{e}");
    }

    #[test]
    fn runaway_output_is_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "chromium", "exec yes '<p>spam</p>'");
        let start = Instant::now();
        let e = dump_dom(
            &info(p, false),
            "http://x/",
            VP,
            None,
            Duration::from_secs(20),
        )
        .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(15));
        assert!(e.to_string().contains("more than"), "{e}");
    }

    #[test]
    fn a_missing_binary_is_a_clear_error() {
        let e = dump_dom(
            &info("/nonexistent/engine".into(), false),
            "http://x/",
            VP,
            None,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(e.to_string().contains("could not start"), "{e}");
    }
}
