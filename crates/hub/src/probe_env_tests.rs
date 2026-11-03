//! The hub's non-interactive probe spawns (`muse usage`, `claude auth status`,
//! `<cli> --version`, `agent-hydrate`) run with a scrubbed environment. Each test puts fake
//! secrets into this process's environment, runs the real spawn path against a fake binary that
//! dumps its own environment into a file, and reads the names back.
//!
//! Lock order, outermost first: [`SECRET_ENV_LOCK`], then the probe lock (`with_probe_lock`), then
//! the hydrate lock. Every test here takes them in that order, so none can deadlock.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::SECRET_ENV_LOCK;
use crate::direct_quota::{
    claude_account_quota, command_stdout, expire_direct_quota_cache, muse_account_quota,
    with_probe_lock,
};

/// Secrets and foreign settings the hub process may hold. None may reach a probe.
const LEAKS: [&str; 6] = [
    "FL_VAULT_MASTER_KEY",
    "FL_HUB_OWNER_TOKEN",
    "GITHUB_TOKEN",
    "OPENAI_API_KEY",
    "HTTPS_PROXY",
    "SSH_AUTH_SOCK",
];
/// Variables that belong to the claude and muse dialects and to no other.
const CLAUDE_OWN: &str = "CLAUDE_PROBE_SENTINEL";
const MUSE_OWN: &str = "MUSE_PROBE_SENTINEL";

/// Sets every [`LEAKS`] name and both dialect variables to fake values, holding
/// [`SECRET_ENV_LOCK`], and puts the previous values back on drop.
struct Sentinels {
    saved: Vec<(&'static str, Option<OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn sentinels() -> Sentinels {
    let lock = SECRET_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let names: Vec<&'static str> = LEAKS
        .iter()
        .copied()
        .chain([CLAUDE_OWN, MUSE_OWN])
        .collect();
    let saved = names.iter().map(|n| (*n, std::env::var_os(n))).collect();
    for name in names {
        // SAFETY: every test that sets these names holds SECRET_ENV_LOCK until this drops.
        unsafe { std::env::set_var(name, format!("fake-sentinel-{name}")) };
    }
    Sentinels { saved, _lock: lock }
}

impl Drop for Sentinels {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            // SAFETY: still holding SECRET_ENV_LOCK.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

/// Removes the named variables on drop. Hold [`SECRET_ENV_LOCK`] and the probe lock.
struct ClearOnDrop(&'static [&'static str]);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        for name in self.0 {
            // SAFETY: the test that set these holds the locks until this drops.
            unsafe { std::env::remove_var(name) };
        }
    }
}

/// An executable `name` in `dir` that writes its environment to `dump`, then runs `tail`.
fn dumping_script(dir: &Path, name: &str, dump: &Path, tail: &str) -> PathBuf {
    let path = dir.join(name);
    let body = format!("#!/bin/sh\nenv > \"{}\"\n{tail}\n", dump.display());
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The variable names in a dump written by `env`.
fn dumped_names(dump: &Path) -> BTreeSet<String> {
    let text = fs::read_to_string(dump).expect("the probe binary did not run");
    text.lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.to_string()))
        .collect()
}

/// A probe's baseline is present and no leak is.
fn assert_scrubbed(names: &BTreeSet<String>) {
    for leak in LEAKS {
        assert!(!names.contains(leak), "{leak} reached the probe");
    }
    assert!(
        !names.iter().any(|n| n.starts_with("FL_")),
        "an FL_ variable reached the probe: {names:?}"
    );
    for kept in ["PATH", "HOME"] {
        assert!(names.contains(kept), "{kept} missing: {names:?}");
    }
}

#[test]
fn the_muse_quota_probe_gets_the_baseline_and_muse_variables_only() {
    let _s = sentinels();
    with_probe_lock(|| {
        let _clear = ClearOnDrop(&["FL_HUB_MUSE_BIN"]);
        let tmp = tempfile::tempdir().unwrap();
        let dump = tmp.path().join("env.txt");
        let script = dumping_script(tmp.path(), "muse", &dump, "printf '%s\\n' '{}'");
        // SAFETY: SECRET_ENV_LOCK and the probe lock are held until `_clear` drops.
        unsafe { std::env::set_var("FL_HUB_MUSE_BIN", &script) };
        expire_direct_quota_cache();
        let _ = muse_account_quota(Some(tmp.path()));
        let names = dumped_names(&dump);
        assert_scrubbed(&names);
        assert!(names.contains(MUSE_OWN), "{names:?}");
        assert!(!names.contains(CLAUDE_OWN), "{names:?}");
    });
}

#[test]
fn the_claude_quota_probe_gets_the_baseline_and_claude_variables_only() {
    let _s = sentinels();
    with_probe_lock(|| {
        let _clear = ClearOnDrop(&["FL_HUB_CLAUDE_BIN", "FL_CLAUDE_USAGE_JSON"]);
        let tmp = tempfile::tempdir().unwrap();
        let dump = tmp.path().join("env.txt");
        let script = dumping_script(tmp.path(), "claude", &dump, "printf '%s\\n' '{}'");
        // SAFETY: SECRET_ENV_LOCK and the probe lock are held until `_clear` drops.
        unsafe { std::env::set_var("FL_HUB_CLAUDE_BIN", &script) };
        expire_direct_quota_cache();
        let _ = claude_account_quota(Some(tmp.path()));
        let names = dumped_names(&dump);
        assert_scrubbed(&names);
        assert!(names.contains(CLAUDE_OWN), "{names:?}");
        assert!(!names.contains(MUSE_OWN), "{names:?}");
    });
}

#[test]
fn a_version_probe_gets_the_baseline_and_its_own_vendor_variables_only() {
    let _s = sentinels();
    with_probe_lock(|| {
        let tmp = tempfile::tempdir().unwrap();
        for (name, own, other) in [
            ("claude", CLAUDE_OWN, MUSE_OWN),
            ("muse", MUSE_OWN, CLAUDE_OWN),
            // Not a vendor name: the baseline alone.
            ("some-other-tool", "", CLAUDE_OWN),
        ] {
            let dump = tmp.path().join(format!("{name}.env"));
            let script = dumping_script(tmp.path(), name, &dump, "printf '%s\\n' 'tool 1.2.3'");
            let version = run_fixture_version(&script).expect("version");
            assert_eq!(version, "tool 1.2.3");
            let names = dumped_names(&dump);
            assert_scrubbed(&names);
            assert!(own.is_empty() || names.contains(own), "{name}: {names:?}");
            assert!(!names.contains(other), "{name}: {names:?}");
            if own.is_empty() {
                assert!(!names.contains(MUSE_OWN), "{name}: {names:?}");
            }
        }
    });
}

/// `run_version` on a script this test just wrote, retried while the fresh file is still held
/// open for writing by a concurrently forked child (ETXTBSY).
fn run_fixture_version(path: &Path) -> Result<String, String> {
    for _ in 0..200 {
        let result = crate::runtime_matrix::run_version(path, Duration::from_secs(5));
        match &result {
            Err(e) if e.starts_with("spawn failed:") && e.contains("Text file busy") => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => return result,
        }
    }
    Err("Text file busy".into())
}

#[test]
fn the_hydrate_probe_gets_the_baseline_and_uv_paths_and_no_vendor_variables() {
    let _s = sentinels();
    with_probe_lock(|| {
        let _clear = ClearOnDrop(&["UV_CACHE_DIR"]);
        // SAFETY: SECRET_ENV_LOCK is held until `_clear` and `_s` drop.
        unsafe { std::env::set_var("UV_CACHE_DIR", "/fake/uv-cache") };
        let tmp = tempfile::tempdir().unwrap();
        let dump = tmp.path().join("env.txt");
        let script = dumping_script(
            tmp.path(),
            "agent-hydrate",
            &dump,
            "printf '%s\\n' '{\"Felicia\":\"PROMPT\"}'",
        );
        let mut out = None;
        for _ in 0..200 {
            out = crate::api::with_hydrate_lock(|| {
                crate::api::read_command_stdout(
                    &script,
                    &["felicia", "--print", "--json"],
                    Duration::from_secs(5),
                )
            });
            if out.is_some() || dump.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(out.expect("hydrate output").contains("PROMPT"));
        let names = dumped_names(&dump);
        assert_scrubbed(&names);
        assert!(names.contains("UV_CACHE_DIR"), "{names:?}");
        for vendor in [CLAUDE_OWN, MUSE_OWN] {
            assert!(!names.contains(vendor), "{vendor} reached hydrate");
        }
    });
}

#[test]
fn the_curl_probe_gets_the_baseline_and_no_vendor_variables() {
    let _s = sentinels();
    with_probe_lock(|| {
        let tmp = tempfile::tempdir().unwrap();
        let dump = tmp.path().join("env.txt");
        let script = dumping_script(tmp.path(), "curl", &dump, "printf '%s\\n' '{}'");
        let mut out = None;
        for _ in 0..200 {
            out = command_stdout(&script, &["--fail"], None, Duration::from_secs(5));
            if out.is_some() || dump.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(out.is_some());
        let names = dumped_names(&dump);
        assert_scrubbed(&names);
        for vendor in [CLAUDE_OWN, MUSE_OWN] {
            assert!(!names.contains(vendor), "{vendor} reached curl");
        }
    });
}
