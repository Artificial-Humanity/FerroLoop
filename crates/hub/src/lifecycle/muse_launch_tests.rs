//! Built-in CLI launch tests: spin-up and restart pass the hydration prompt as the first input;
//! custom CLI vendors get a blank window and prompt delivery. Nothing here runs a real vendor
//! CLI or `tmux`: the runtime is a `FakeRuntime`, and discovery reads a tempdir home and, in the
//! end-to-end test, a fake tmux.

use super::*;
use fl_core::MemStore;
use fl_core::at::At;
use std::os::unix::net::UnixListener;
use tempfile::TempDir;

fn req(persona: &str, vendor: &str) -> SpinUpRequest {
    SpinUpRequest {
        persona: persona.into(),
        project: "FerroLoop".into(),
        role: "assistant".into(),
        vendor: vendor.into(),
        ..SpinUpRequest::default()
    }
}

fn spin(store: &MemStore, env: &LifecycleEnv<'_>, persona: &str, vendor: &str) -> SpinUpResponse {
    spin_up(store, env, &req(persona, vendor), "owner", Caller::Owner).unwrap()
}

/// A webhook route for `persona` and the socket behind it: a delivery connects to it, so
/// `accept` tells whether the prompt was delivered.
fn webhook_listener(home: &Path, persona: &str) -> UnixListener {
    let socket = home.join(format!("{persona}.sock"));
    let mut routes = RouteTable::default();
    routes.upsert(
        persona,
        VendorRoute::Webhook {
            socket_path: socket.clone(),
        },
        At::from_unix_millis(1),
    );
    let path = home.join(".cache").join("agent-bus").join("routes.json");
    routes.save(Some(&path)).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn delivered(listener: &UnixListener) -> bool {
    listener.accept().is_ok()
}

/// Waits until the script `bin`, just written by this test, can be executed. Writing a script and
/// running it races with every other test thread that forks: a child forked while the script's
/// write fd is still open holds that fd until its own `execve`, and `execve` of a file any
/// process holds open for writing fails with `ETXTBSY`. The runtime then sees a tmux that cannot
/// be spawned (an empty call log), which is the fixture's race and not the runtime's. Only that
/// exact error is retried, bounded; the script answers the probe argument without a side effect.
fn wait_until_executable(bin: &Path) {
    for _ in 0..1000 {
        match std::process::Command::new(bin)
            .arg(EXEC_PROBE)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Err(err) if err.raw_os_error() == Some(26) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => {
                other.unwrap_or_else(|err| panic!("{} cannot be executed: {err}", bin.display()));
                return;
            }
        }
    }
    panic!("{} stayed busy for writing", bin.display());
}

/// The argument [`wait_until_executable`] runs a fake with; a fake exits 0 on it, logging nothing.
const EXEC_PROBE: &str = "--fl-exec-probe";

/// A fake `<home>/.local/bin/muse`, so the binary resolves to a path of the tempdir.
fn install_home_muse(home: &Path) -> PathBuf {
    let bin = home.join(".local").join("bin").join("muse");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    bin
}

fn muse_launch(prompt: &str, home: &Path, persona: &str) -> LaunchSpec {
    LaunchSpec {
        env: launch_env(persona, Some(home)),
        argv: vec![
            home.join(".local/bin/muse").to_string_lossy().into_owned(),
            "--model".into(),
            MUSE_DEFAULT_MODEL.into(),
            prompt.to_string(),
        ],
    }
}

/// Serialises the tests that depend on `FL_WIRE_MUSE_BIN` (process-global) and restores it on
/// drop. A test that asserts which Muse binary is launched holds one, so it does not depend on
/// the developer's or CI's environment.
struct MuseBinEnv {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Option<std::ffi::OsString>,
}

static MUSE_BIN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl MuseBinEnv {
    /// Holds the lock with `FL_WIRE_MUSE_BIN` unset.
    fn cleared() -> Self {
        let lock = MUSE_BIN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let this = Self {
            _lock: lock,
            prev: std::env::var_os("FL_WIRE_MUSE_BIN"),
        };
        this.set(None);
        this
    }

    /// Sets (or, with `None`, unsets) the variable while the lock is held.
    fn set(&self, value: Option<&str>) {
        // SAFETY: the lock is held for the guard's life; drop restores the variable.
        unsafe {
            match value {
                Some(v) => std::env::set_var("FL_WIRE_MUSE_BIN", v),
                None => std::env::remove_var("FL_WIRE_MUSE_BIN"),
            }
        }
    }
}

impl Drop for MuseBinEnv {
    fn drop(&mut self) {
        // SAFETY: the lock is still held (fields drop after this).
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("FL_WIRE_MUSE_BIN", v),
                None => std::env::remove_var("FL_WIRE_MUSE_BIN"),
            }
        }
    }
}

// ------------------------------------------------------------------- the model id

#[test]
fn muse_model_names_normalise_to_catalog_ids() {
    let rows: [(Option<&str>, &str); 14] = [
        (Some("muse-wonder-full"), "muse-wonder-full-1.0"),
        (Some("muse-wonder"), "muse-wonder-1.0"),
        (Some("muse-spark"), "muse-spark-1.3"),
        (Some("  Muse-Wonder-Full  "), "muse-wonder-full-1.0"),
        (Some("MUSE-SPARK"), "muse-spark-1.3"),
        (Some("muse-wonder-full-1.0"), "muse-wonder-full-1.0"),
        (Some("muse-wonder-1.0"), "muse-wonder-1.0"),
        (Some("muse-spark-1.3"), "muse-spark-1.3"),
        (Some("MUSE-SPARK-1.3"), "muse-spark-1.3"),
        (None, "muse-wonder-full-1.0"),
        (Some(""), "muse-wonder-full-1.0"),
        (Some("   "), "muse-wonder-full-1.0"),
        // Any other plain id passes through, as typed.
        (Some("muse-future-2.0"), "muse-future-2.0"),
        (Some(" Local_Model.v2 "), "Local_Model.v2"),
    ];
    for (requested, want) in rows {
        assert_eq!(normalize_muse_model(requested), want, "{requested:?}");
    }
}

#[test]
fn a_muse_model_that_is_not_a_plain_id_falls_back_to_the_default() {
    let too_long = "a".repeat(65);
    let longest = "a".repeat(64);
    for bad in [
        "--help",
        "-m",
        "-muse-spark",
        "--model=x",
        ".hidden",
        "_x",
        "a b",
        "a;b",
        "a\nb",
        "mu$e",
        "modèle",
        too_long.as_str(),
    ] {
        assert_eq!(
            normalize_muse_model(Some(bad)),
            MUSE_DEFAULT_MODEL,
            "{bad:?}"
        );
    }
    assert_eq!(normalize_muse_model(Some(&longest)), longest);
}

#[test]
fn the_launch_argv_puts_the_model_first_and_the_prompt_last() {
    let _bin = MuseBinEnv::cleared();
    let home = TempDir::new().unwrap();
    install_home_muse(home.path());
    let bin = home
        .path()
        .join(".local/bin/muse")
        .to_string_lossy()
        .into_owned();
    for (requested, want) in [
        (None, "muse-wonder-full-1.0"),
        (Some("muse-spark"), "muse-spark-1.3"),
        (Some("--evil"), "muse-wonder-full-1.0"),
    ] {
        let store = MemStore::default();
        let fake = FakeRuntime::default();
        let mut request = req("ada", "muse");
        request.model = requested.map(Into::into);
        let out = spin_up(
            &store,
            &fake.env(home.path()),
            &request,
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let launches = fake.launches.borrow();
        let argv = &launches[0].1.as_ref().unwrap().argv;
        assert_eq!(
            argv,
            &[
                bin.clone(),
                "--model".to_string(),
                want.to_string(),
                out.prompt.unwrap(),
            ],
            "{requested:?}"
        );
    }
}

#[test]
fn a_restart_launches_with_the_recorded_model_normalised() {
    let _bin = MuseBinEnv::cleared();
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    install_home_muse(home.path());
    let first = FakeRuntime::default();
    let mut request = req("ada", "muse");
    request.model = Some("muse-wonder".into());
    let out = spin_up(
        &store,
        &first.env(home.path()),
        &request,
        "owner",
        Caller::Owner,
    )
    .unwrap();
    // The tmux session is gone, so the restart reopens it.
    let second = FakeRuntime::default();
    restart_hosted_session(&store, &second.env(home.path()), &out.session_id, "owner").unwrap();
    let launches = second.launches.borrow();
    let argv = &launches[0].1.as_ref().expect("restart launches").argv;
    assert_eq!(&argv[1..3], ["--model", "muse-wonder-1.0"]);
}

#[test]
fn plain_models_reject_flags_and_invalid_ids() {
    let long = "a".repeat(65);
    for invalid in [
        None,
        Some(""),
        Some("  "),
        Some("--help"),
        Some(".x"),
        Some("a b"),
        Some("é"),
        Some(long.as_str()),
    ] {
        assert_eq!(normalize_plain_model(invalid), None, "{invalid:?}");
    }
    assert_eq!(
        normalize_plain_model(Some(" Model.v2_3-x ")),
        Some("Model.v2_3-x".into())
    );
    assert_eq!(
        normalize_plain_model(Some(&"a".repeat(64))),
        Some("a".repeat(64))
    );
}

#[test]
fn vendor_binary_resolution_prefers_config_then_home_then_path() {
    let home = TempDir::new().unwrap();
    assert_eq!(resolve_vendor_bin(None, Some(home.path()), "agy"), "agy");
    let path = home.path().join(".local/bin/agy");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "").unwrap();
    assert_eq!(
        resolve_vendor_bin(Some("".into()), Some(home.path()), "agy"),
        path.to_string_lossy()
    );
    assert_eq!(
        resolve_vendor_bin(Some("/custom/agy".into()), Some(home.path()), "agy"),
        "/custom/agy"
    );
}

#[test]
fn launched_built_in_vendors_do_not_receive_a_duplicate_prompt() {
    for vendor in ["claude", "codex", "grok", "antigravity"] {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let listener = webhook_listener(home.path(), "ada");
        let fake = FakeRuntime::default();
        let out = spin(&store, &fake.env(home.path()), "ada", vendor);
        assert!(out.runtime_spawned && out.hydration_dispatched, "{vendor}");
        assert!(!delivered(&listener), "{vendor}");
    }
}

// ------------------------------------------------------------------- what is launched

#[test]
fn spin_up_launches_muse_with_the_prompt_as_its_first_argument() {
    let _bin = MuseBinEnv::cleared();
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    install_home_muse(home.path());
    let fake = FakeRuntime::default();
    let out = spin(&store, &fake.env(home.path()), "ada", "muse");

    let launches = fake.launches.borrow();
    assert_eq!(launches.len(), 1, "{launches:?}");
    assert_eq!(launches[0].0, "muse:ada");
    assert_eq!(
        launches[0].1,
        Some(muse_launch(&out.prompt.unwrap(), home.path(), "ada"))
    );
}

#[test]
fn the_launched_muse_binary_follows_the_environment_setting_and_the_guard_clears_it() {
    let bin = MuseBinEnv::cleared();
    let home = TempDir::new().unwrap();
    install_home_muse(home.path());
    let argv0 = |persona: &str| {
        let store = MemStore::default();
        let fake = FakeRuntime::default();
        spin(&store, &fake.env(home.path()), persona, "muse");
        let launches = fake.launches.borrow();
        launches[0].1.as_ref().unwrap().argv[0].clone()
    };
    bin.set(Some("/ambient/muse"));
    assert_eq!(argv0("ada"), "/ambient/muse");
    bin.set(None);
    assert_eq!(
        argv0("ada"),
        home.path().join(".local/bin/muse").to_string_lossy()
    );
}

#[test]
fn built_in_vendors_launch_with_their_prompt() {
    for (vendor, bin, variable, flags) in [
        (
            "claude",
            "claude",
            "FL_WIRE_CLAUDE_BIN",
            vec!["--enable-auto-mode"],
        ),
        ("codex", "codex", "FL_WIRE_CODEX_BIN", vec!["--yolo"]),
        (
            "grok",
            "grok",
            "FL_WIRE_GROK_BIN",
            vec!["--yolo", "--always-approve"],
        ),
        (
            "antigravity",
            "agy",
            "FL_WIRE_AGY_BIN",
            vec!["--mode", "auto", "--prompt-interactive"],
        ),
    ] {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let out = spin(&store, &fake.env(home.path()), "ada", vendor);
        let launches = fake.launches.borrow();
        let spec = launches[0].1.as_ref().expect(vendor);
        let expected_bin = std::env::var_os(variable)
            .filter(|value| !value.is_empty())
            .map_or_else(
                || bin.to_string(),
                |value| value.to_string_lossy().into_owned(),
            );
        let mut expected = vec![expected_bin];
        expected.extend(flags.into_iter().map(String::from));
        expected.push(out.prompt.unwrap());
        assert_eq!(spec.env, launch_env("ada", Some(home.path())));
        assert_eq!(spec.argv, expected, "{vendor}");
    }
}

#[test]
fn restart_launches_muse_in_the_window_it_reopens() {
    let _bin = MuseBinEnv::cleared();
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    install_home_muse(home.path());
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    // The window is gone (the CLI exited): a restart opens it again.
    fake.clear();
    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();

    let launches = fake.launches.borrow();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert_eq!(launches[1].0, "muse:ada");
    assert_eq!(
        launches[1].1,
        Some(muse_launch(&out.prompt, home.path(), "ada"))
    );
    assert!(out.record.runtime_spawned);
}

#[test]
fn restart_launches_other_built_in_vendors_with_the_recorded_model() {
    for (vendor, bin, variable, flags) in [
        (
            "claude",
            "claude",
            "FL_WIRE_CLAUDE_BIN",
            vec!["--enable-auto-mode"],
        ),
        ("codex", "codex", "FL_WIRE_CODEX_BIN", vec!["--yolo"]),
        (
            "grok",
            "grok",
            "FL_WIRE_GROK_BIN",
            vec!["--yolo", "--always-approve"],
        ),
        (
            "antigravity",
            "agy",
            "FL_WIRE_AGY_BIN",
            vec!["--mode", "auto"],
        ),
    ] {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = fake.env(home.path());
        let mut request = req("ada", vendor);
        request.model = Some(" Model.v2 ".into());
        let initial = spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap();
        fake.clear();
        let out = restart_hosted_session(&store, &env, &initial.session_id, "owner").unwrap();
        let launches = fake.launches.borrow();
        let spec = launches[1].1.as_ref().expect(vendor);
        let expected_bin = std::env::var_os(variable)
            .filter(|value| !value.is_empty())
            .map_or_else(
                || bin.to_string(),
                |value| value.to_string_lossy().into_owned(),
            );
        let mut expected = vec![expected_bin];
        expected.extend(flags.into_iter().map(String::from));
        // `agy` only takes effort-suffixed, lowercase ids, so its recorded model gets `-medium`.
        let model = if vendor == "antigravity" {
            "model.v2-medium"
        } else {
            "Model.v2"
        };
        expected.extend(["--model".into(), model.into()]);
        if vendor == "antigravity" {
            expected.push("--prompt-interactive".into());
        }
        expected.push(out.prompt);
        assert_eq!(spec.env, launch_env("ada", Some(home.path())));
        assert_eq!(spec.argv, expected, "{vendor}");
    }
}

// ------------------------------------------------------------------- hydration

#[test]
fn a_launched_muse_has_its_prompt_and_is_sent_nothing_more() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let listener = webhook_listener(home.path(), "ada");
    let fake = FakeRuntime::default();
    let out = spin(&store, &fake.env(home.path()), "ada", "muse");

    assert!(out.runtime_spawned);
    assert!(
        out.hydration_dispatched,
        "the prompt went in as an argument"
    );
    assert!(
        !delivered(&listener),
        "the prompt must not also be delivered"
    );
    let record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    assert!(record.hydration_dispatched);
}

#[test]
fn a_launched_muse_without_any_route_still_counts_as_hydrated() {
    // No route exists, so a `deliver_prompt` call could only report `false`.
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let out = spin(&store, &fake.env(home.path()), "ada", "muse");
    assert!(out.hydration_dispatched);
}

#[test]
fn a_muse_session_that_already_runs_is_hydrated_by_delivery() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let listener = webhook_listener(home.path(), "ada");
    // The window `muse:ada` exists already: nothing is launched.
    let fake = FakeRuntime::with_muse_windows(&["ada"]);
    let out = spin(&store, &fake.env(home.path()), "ada", "muse");

    assert!(!out.runtime_spawned);
    assert!(fake.launches.borrow().is_empty());
    assert!(out.hydration_dispatched);
    assert!(delivered(&listener), "the prompt is delivered to the route");
}

#[test]
fn a_muse_session_that_is_not_hosted_here_is_hydrated_by_delivery() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let listener = webhook_listener(home.path(), "ada");
    let fake = FakeRuntime::default();
    let mut r = req("ada", "muse");
    r.hosted = Some(false);
    let out = spin_up(&store, &fake.env(home.path()), &r, "owner", Caller::Owner).unwrap();
    assert!(!out.runtime_spawned);
    assert!(fake.launches.borrow().is_empty());
    assert!(delivered(&listener));
}

#[test]
fn a_custom_cli_opens_blank_and_is_hydrated_by_delivery() {
    let vendor = super::vendor_registry_tests::custom_cli("freya-custom-cli");
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let listener = webhook_listener(home.path(), "ada");
    let fake = FakeRuntime::default();
    let out = spin(&store, &fake.env(home.path()), "ada", &vendor);
    assert!(out.runtime_spawned && out.hydration_dispatched);
    assert_eq!(fake.launches.borrow()[0].1, None);
    assert!(delivered(&listener));

    fake.clear();
    let restarted =
        restart_hosted_session(&store, &fake.env(home.path()), &out.session_id, "owner").unwrap();
    assert!(restarted.record.runtime_spawned && restarted.record.hydration_dispatched);
    assert_eq!(fake.launches.borrow()[1].1, None);
    assert!(delivered(&listener));
}

#[test]
fn restart_of_a_launched_muse_does_not_deliver_the_prompt_again() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    fake.clear();
    let listener = webhook_listener(home.path(), "ada");
    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();
    assert!(out.record.hydration_dispatched);
    assert!(!delivered(&listener));
}

#[test]
fn restart_of_a_muse_whose_window_exists_delivers_the_prompt() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    // The window still exists: no launch, so the reforge prompt must be delivered.
    let listener = webhook_listener(home.path(), "ada");
    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();
    assert_eq!(fake.launches.borrow().len(), 1);
    assert!(out.record.hydration_dispatched);
    assert!(delivered(&listener));
}

#[test]
fn a_launched_muse_that_exits_at_once_is_neither_spawned_nor_hydrated() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let listener = webhook_listener(home.path(), "ada");
    let fake = FakeRuntime::default();
    fake.vanish_windows.borrow_mut().push("ada".into());
    let out = spin(&store, &fake.env(home.path()), "ada", "muse");

    assert!(!out.runtime_spawned);
    assert!(!out.hydration_dispatched);
    assert_eq!(fake.owns_calls.get(), 5, "bounded retry");
    assert!(!delivered(&listener), "nothing to deliver to");
    let record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    assert!(!record.runtime_spawned && !record.hydration_dispatched);
}

#[test]
fn restart_of_a_launched_muse_that_exits_at_once_is_neither_spawned_nor_hydrated() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    fake.clear();
    fake.vanish_windows.borrow_mut().push("ada".into());
    let listener = webhook_listener(home.path(), "ada");
    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();

    assert!(!out.record.runtime_spawned);
    assert!(!out.record.hydration_dispatched);
    assert!(!delivered(&listener));
    assert_eq!(out.record.restart_count, 1);
}

#[test]
fn a_launched_muse_that_stays_up_is_checked_once() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let out = spin(&store, &fake.env(home.path()), "ada", "muse");
    assert!(out.runtime_spawned && out.hydration_dispatched);
    assert_eq!(fake.owns_calls.get(), 1);
}

// ------------------------------------------------------------------- the launch itself

#[test]
fn the_muse_binary_is_the_configured_one_then_the_home_one_then_path() {
    let home = TempDir::new().unwrap();
    let configured = Some(std::ffi::OsString::from("/opt/muse/bin/muse"));
    assert_eq!(
        resolve_muse_bin(configured.clone(), Some(home.path())),
        "/opt/muse/bin/muse"
    );
    // Not installed in the home: found on PATH.
    assert_eq!(resolve_muse_bin(None, Some(home.path())), "muse");
    assert_eq!(resolve_muse_bin(None, None), "muse");
    // An empty setting is no setting.
    assert_eq!(resolve_muse_bin(Some("".into()), Some(home.path())), "muse");
    let bin = install_home_muse(home.path());
    assert_eq!(
        resolve_muse_bin(None, Some(home.path())),
        bin.to_string_lossy()
    );
    // The setting still wins over the home install.
    assert_eq!(
        resolve_muse_bin(configured, Some(home.path())),
        "/opt/muse/bin/muse"
    );
}

#[test]
fn tmux_new_session_runs_the_launch_as_an_argument_vector() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    let launch = LaunchSpec {
        env: vec![("AGENT_PERSONA".into(), "ada".into())],
        argv: vec![
            "/bin/muse".into(),
            "Take the name of Ada; $(rm -rf /) 'x'".into(),
        ],
    };
    assert_eq!(
        tmux_new_session_args("Ada", "muse", Some(&cwd), Some(&launch)),
        [
            "new-session",
            "-d",
            "-s",
            "Ada",
            "-n",
            "muse",
            "-c",
            cwd.as_str(),
            "-e",
            "AGENT_PERSONA=ada",
            "--",
            "/bin/muse",
            "Take the name of Ada; $(rm -rf /) 'x'",
        ]
    );
}

#[test]
fn the_muse_argv_after_the_tmux_separator_has_no_further_separator() {
    // Muse's CLI rejects a bare `--`; only tmux's own `--` (before the command) is right.
    let _bin = MuseBinEnv::cleared();
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    install_home_muse(home.path());
    let fake = FakeRuntime::default();
    spin(&store, &fake.env(home.path()), "ada", "muse");
    let launches = fake.launches.borrow();
    let launch = launches[0].1.as_ref().unwrap();
    let args = tmux_muse_spawn_args(true, "ada", None, Some(launch));
    let tmux_sep = args.iter().position(|a| a == "--").expect("tmux separator");
    assert_eq!(args.iter().filter(|a| *a == "--").count(), 1, "{args:?}");
    assert_eq!(&args[tmux_sep + 1..], launch.argv.as_slice());
    assert!(!launch.argv.iter().any(|a| a == "--"), "{:?}", launch.argv);
}

#[test]
fn a_muse_prompt_that_starts_with_a_dash_is_guarded_against_flag_parsing() {
    for prompt in ["-h", "--version", "- a list item", "---"] {
        let arg = muse_prompt_argument(prompt);
        assert!(!arg.starts_with('-'), "{prompt:?} -> {arg:?}");
        assert!(arg.ends_with(prompt), "{prompt:?} -> {arg:?}");
    }
    // Any other prompt is passed unchanged.
    assert_eq!(
        muse_prompt_argument("Take up residence"),
        "Take up residence"
    );
    assert_eq!(muse_prompt_argument(""), "");
}

#[test]
fn tmux_new_session_without_a_launch_is_a_blank_window() {
    assert_eq!(
        tmux_new_session_args("Ada", "claude", Some("/no/such/dir"), None),
        ["new-session", "-d", "-s", "Ada", "-n", "claude"]
    );
}

// ------------------------------------------------------------------- an empty id is no id

#[test]
fn a_muse_route_without_a_session_id_matches_no_session_id() {
    let route = VendorRoute::Muse {
        session_id: String::new(),
        cwd: None,
        tmux_target: Some("Ada:muse".into()),
        tmux_pane: None,
    };
    for raw in ["", " ", "muse-", "muse-ada", "muse:ada", "ada"] {
        assert!(
            !crate::api::route_matches_session_id(&route, raw),
            "matched `{raw}`"
        );
        assert!(!route_serves_session(&route, raw), "serves `{raw}`");
    }
    let with_id = VendorRoute::Muse {
        session_id: "abc".into(),
        cwd: None,
        tmux_target: None,
        tmux_pane: None,
    };
    assert!(crate::api::route_matches_session_id(&with_id, "abc"));
    assert!(crate::api::route_matches_session_id(&with_id, "muse-abc"));
}

#[test]
fn a_grok_route_without_a_session_id_matches_no_session_id() {
    let route = VendorRoute::Grok {
        session_id: String::new(),
        cwd: None,
        tmux_target: Some("Ada:grok".into()),
        tmux_pane: None,
    };
    for raw in ["", " ", "grok-", "grok-ada", "ada"] {
        assert!(
            !crate::api::route_matches_session_id(&route, raw),
            "matched `{raw}`"
        );
        assert!(!route_serves_session(&route, raw), "serves `{raw}`");
    }
    let with_id = VendorRoute::Grok {
        session_id: "abc".into(),
        cwd: None,
        tmux_target: None,
        tmux_pane: None,
    };
    assert!(crate::api::route_matches_session_id(&with_id, "abc"));
    assert!(crate::api::route_matches_session_id(&with_id, "grok-abc"));
}

// ------------------------------------------------------------------- end to end

/// A fake `tmux` that lists `windows` and reports every target live, with tmux discovery opted
/// in for scans of the home `dir`; restores the variables on drop. Holds [`crate::TMUX_ENV_LOCK`].
/// Call it inside `with_probe_lock`, whose tests also set `FL_WIRE_DISABLE_HOST_DISCOVERY`.
struct FakeTmuxWindows {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev_bin: Option<std::ffi::OsString>,
    prev_opt_in: Option<std::ffi::OsString>,
}

impl FakeTmuxWindows {
    fn install(dir: &Path, windows: &[&str]) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let lock = crate::TMUX_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let list: String = windows.iter().map(|w| format!("  echo '{w}'\n")).collect();
        let bin = dir.join("fake-tmux.sh");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\n[ \"$1\" = \"{EXEC_PROBE}\" ] && exit 0\nif [ \"$1\" = \"list-windows\" ]; then\n{list}elif [ \"$1\" = \"display-message\" ]; then\n  echo '%5'\nfi\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        wait_until_executable(&bin);
        let this = Self {
            _lock: lock,
            prev_bin: std::env::var_os("FL_WIRE_TMUX_BIN"),
            prev_opt_in: std::env::var_os("FL_WIRE_TMUX_DISCOVERY"),
        };
        // SAFETY: the lock is held while the variables are set; drop restores them.
        unsafe {
            std::env::set_var("FL_WIRE_TMUX_BIN", &bin);
            std::env::set_var("FL_WIRE_TMUX_DISCOVERY", dir);
        }
        this
    }
}

impl Drop for FakeTmuxWindows {
    fn drop(&mut self) {
        // SAFETY: the lock is still held (fields drop after this).
        unsafe {
            for (key, prev) in [
                ("FL_WIRE_TMUX_BIN", self.prev_bin.take()),
                ("FL_WIRE_TMUX_DISCOVERY", self.prev_opt_in.take()),
            ] {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[test]
fn a_spun_up_muse_resolves_to_its_tmux_window_before_it_has_a_transcript() {
    crate::direct_quota::with_probe_lock(|| {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = fake.env(home.path());
        let out = spin(&store, &env, "muselaunch", "muse");
        assert_eq!(out.tmux_target.as_deref(), Some("muse:muselaunch"));
        assert!(fake.is_live("muse:muselaunch"));

        // The window the launch opened, as tmux lists it. No transcript exists yet.
        let query = RouteQuery::persona("muselaunch", &out.session_id);
        let _tmux = FakeTmuxWindows::install(home.path(), &["muse:muselaunch", "Other:zsh"]);
        let route = lookup_route(Some(home.path()), query)
            .expect("the window makes a route")
            .into_route();

        assert_eq!(
            route,
            VendorRoute::Muse {
                session_id: String::new(),
                cwd: None,
                tmux_target: Some("muse:muselaunch".into()),
                tmux_pane: None,
            }
        );
        // Nothing to refuse the delivery before it is tried, and no session id to match.
        assert_eq!(undeliverable(&route), None);
        assert!(!crate::api::route_matches_session_id(&route, ""));
    });
}

#[test]
fn without_the_window_a_muse_with_no_transcript_has_no_route() {
    crate::direct_quota::with_probe_lock(|| {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let out = spin(&store, &fake.env(home.path()), "museabsent", "muse");
        let query = RouteQuery::persona("museabsent", &out.session_id);
        let _tmux = FakeTmuxWindows::install(home.path(), &["Other:muse"]);
        assert_eq!(lookup_route(Some(home.path()), query), None);
    });
}

#[test]
fn the_session_list_names_a_muse_with_no_transcript_instead_of_an_empty_id() {
    let home = TempDir::new().unwrap();
    let mut routes = RouteTable::default();
    routes.upsert(
        "ada",
        VendorRoute::Muse {
            session_id: String::new(),
            cwd: None,
            tmux_target: Some("Ada:muse".into()),
            tmux_pane: None,
        },
        At::from_unix_millis(1),
    );
    let path = home
        .path()
        .join(".cache")
        .join("agent-bus")
        .join("routes.json");
    routes.save(Some(&path)).unwrap();

    let sessions = crate::discovery::discover_route_sessions_without_model_info(
        &crate::discovery::DiscoveryStoreSnapshot::default(),
        Some(home.path()),
    );
    let ada: Vec<_> = sessions
        .iter()
        .filter(|s| s.persona.as_deref() == Some("ada"))
        .collect();
    assert_eq!(ada.len(), 1, "{sessions:?}");
    assert_eq!(ada[0].id, "muse-ada");
    assert_eq!(ada[0].adapter, "muse");
    assert_eq!(ada[0].transcript_path, None);
}

// ------------------------------------------------------------------- the shared `muse` session

fn muse_spec(prompt: &str) -> LaunchSpec {
    LaunchSpec {
        env: vec![("AGENT_PERSONA".into(), "ada".into())],
        argv: vec![
            "/bin/muse".into(),
            "--model".into(),
            "muse-wonder-full-1.0".into(),
            prompt.into(),
        ],
    }
}

fn teardown(store: &MemStore, env: &LifecycleEnv<'_>, id: &str) -> DecommissionResponse {
    let target = SessionTarget::resolve(store, id, None).unwrap();
    teardown_session(store, env, &target, "owner", Caller::Owner, |_| {}).unwrap()
}

fn killed(fake: &FakeRuntime) -> Vec<String> {
    fake.calls
        .borrow()
        .iter()
        .filter(|c| c.starts_with("kill"))
        .cloned()
        .collect()
}

/// A fake `tmux` that logs every call (one line of arguments) in `<dir>/tmux-calls`, answers
/// `has-session` by `session_runs`, and lists `windows` (see [`TmuxLog::install`]). A new window
/// is reported as `@99`. Holds [`crate::TMUX_ENV_LOCK`] and restores `FL_WIRE_TMUX_BIN` on drop,
/// so the real `tmux` is never reached.
struct TmuxLog {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev_bin: Option<std::ffi::OsString>,
    log: PathBuf,
}

impl TmuxLog {
    /// `windows` are the names in the session, window `i` having the id `@{i+1}`; `name=mark`
    /// gives the window the [`HOSTED_MARK`] `mark`. `list-windows` answers the id, name and mark
    /// (tab separated) when asked for `window_id`, else the names alone.
    fn install(dir: &Path, session_runs: bool, windows: &[&str]) -> Self {
        Self::install_failing(dir, session_runs, windows, "@99", &[])
    }

    /// As [`Self::install`], with `new_window_out` printed by `new-window` and `new-session` and
    /// `failing` the tmux commands (`set-option`, `kill-window`, ...) that exit 1.
    fn install_failing(
        dir: &Path,
        session_runs: bool,
        windows: &[&str],
        new_window_out: &str,
        failing: &[&str],
    ) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let lock = crate::TMUX_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let log = dir.join("tmux-calls");
        let mut names = String::new();
        let mut full = String::new();
        for (i, w) in windows.iter().enumerate() {
            let (name, mark) = w.split_once('=').unwrap_or((w, ""));
            names.push_str(&format!("      echo '{name}'\n"));
            full.push_str(&format!(
                "      printf '@%s\\t%s\\t%s\\n' '{}' '{name}' '{mark}'\n",
                i + 1
            ));
        }
        let fail_cases: String = failing
            .iter()
            .map(|cmd| format!("  {cmd}) exit 1;;\n"))
            .collect();
        let has_session = i32::from(!session_runs);
        let bin = dir.join("fake-tmux-log.sh");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\n[ \"$1\" = \"{EXEC_PROBE}\" ] && exit 0\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in\n{fail_cases}  has-session) exit {has_session};;\n  new-window|new-session) echo '{new_window_out}'; exit 0;;\n  list-windows)\n    case \"$*\" in\n    *'#{{window_id}}'*)\n{full}      exit 0;;\n    *)\n{names}      exit 0;;\n    esac;;\n  *) exit 0;;\nesac\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        wait_until_executable(&bin);
        let this = Self {
            _lock: lock,
            prev_bin: std::env::var_os("FL_WIRE_TMUX_BIN"),
            log,
        };
        // SAFETY: the lock is held while the variable is set; drop restores it.
        unsafe { std::env::set_var("FL_WIRE_TMUX_BIN", &bin) };
        this
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The calls that change something (`new-*`, `set-option`, `kill-*`).
    fn mutations(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| {
                ["new-", "set-option", "kill-"]
                    .iter()
                    .any(|p| c.starts_with(p))
            })
            .collect()
    }
}

impl Drop for TmuxLog {
    fn drop(&mut self) {
        // SAFETY: the lock is still held (fields drop after this).
        unsafe {
            match self.prev_bin.take() {
                Some(v) => std::env::set_var("FL_WIRE_TMUX_BIN", v),
                None => std::env::remove_var("FL_WIRE_TMUX_BIN"),
            }
        }
    }
}

#[test]
fn the_muse_target_is_the_shared_session_window_in_the_record_and_the_response() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    let out = spin(&store, &env, "ada", "muse");
    assert_eq!(out.tmux_target.as_deref(), Some("muse:ada"));
    let record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    assert_eq!(record.tmux_target.as_deref(), Some("muse:ada"));
    // The runtime was asked for window `ada` of session `muse`, nothing named for the persona.
    assert_eq!(*fake.calls.borrow(), ["spawn muse:ada"]);
    assert_eq!(*fake.windows.borrow(), ["muse:ada".to_string()].into());

    // A restart answers with the same target.
    fake.clear();
    let restarted = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();
    assert_eq!(restarted.record.tmux_target.as_deref(), Some("muse:ada"));
    assert_eq!(*fake.windows.borrow(), ["muse:ada".to_string()].into());

    // Other vendors keep `<Display>:<vendor>`.
    let claude = spin(&store, &env, "bob", "claude");
    assert_eq!(claude.tmux_target.as_deref(), Some("Bob:claude"));
}

#[test]
fn a_muse_window_opens_beside_the_ones_already_in_the_shared_session() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::with_muse_windows(&["mira"]);
    let out = spin(&store, &fake.env(home.path()), "ada", "muse");
    assert!(out.runtime_spawned);
    assert_eq!(
        *fake.windows.borrow(),
        ["muse:ada".to_string(), "muse:mira".to_string()].into()
    );
    assert!(fake.is_live("muse"));
}

#[test]
fn a_muse_window_that_exists_is_not_touched_and_the_prompt_goes_through_delivery() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let listener = webhook_listener(home.path(), "ada");
    // `muse-sessions` started `ada` already.
    let fake = FakeRuntime::with_muse_windows(&["ada"]);
    let out = spin(&store, &fake.env(home.path()), "ada", "muse");

    assert!(!out.runtime_spawned);
    assert_eq!(out.tmux_target.as_deref(), Some("muse:ada"));
    assert!(fake.launches.borrow().is_empty(), "nothing was launched");
    assert_eq!(*fake.windows.borrow(), ["muse:ada".to_string()].into());
    assert!(killed(&fake).is_empty());
    assert!(out.hydration_dispatched);
    assert!(delivered(&listener), "deliver_prompt ran on the window");
    let record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    assert!(!record.runtime_spawned);
}

#[test]
fn tmux_opens_a_muse_window_with_new_window_when_the_session_runs_and_new_session_when_not() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    let spec = muse_spec("Take up residence; $(x) 'y'");
    let tail = [
        "-c",
        cwd.as_str(),
        "-e",
        "AGENT_PERSONA=ada",
        "--",
        "/bin/muse",
        "--model",
        "muse-wonder-full-1.0",
        "Take up residence; $(x) 'y'",
    ];
    let expected = |head: &[&str]| -> Vec<String> {
        head.iter()
            .chain(tail.iter())
            .map(|s| s.to_string())
            .collect()
    };
    assert_eq!(
        tmux_muse_spawn_args(true, "ada", Some(&cwd), Some(&spec)),
        expected(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{window_id}",
            "-t",
            "=muse:",
            "-n",
            "ada"
        ])
    );
    assert_eq!(
        tmux_muse_spawn_args(false, "ada", Some(&cwd), Some(&spec)),
        expected(&[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{window_id}",
            "-s",
            "muse",
            "-n",
            "ada"
        ])
    );
}

#[test]
fn the_tmux_runtime_adds_a_window_to_a_running_muse_session() {
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install(dir.path(), true, &["mira"]);
    let spec = muse_spec("hi");
    assert!(TmuxRuntime.spawn("muse", "ada", None, Some(&spec), "muse:ada"));
    assert_eq!(
        tmux.calls(),
        [
            "has-session -t =muse",
            "list-windows -t =muse -F #{window_name}",
            "new-window -d -P -F #{window_id} -t =muse: -n ada -e AGENT_PERSONA=ada -- \
             /bin/muse --model muse-wonder-full-1.0 hi",
            "set-option -w -t @99 @fl_hosted muse:ada",
        ]
    );
}

#[test]
fn the_tmux_runtime_creates_the_muse_session_when_it_does_not_run() {
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install(dir.path(), false, &[]);
    let spec = muse_spec("hi");
    assert!(TmuxRuntime.spawn("muse", "ada", None, Some(&spec), "muse:ada"));
    assert_eq!(
        tmux.calls(),
        [
            "has-session -t =muse",
            "new-session -d -P -F #{window_id} -s muse -n ada -e AGENT_PERSONA=ada -- \
             /bin/muse --model muse-wonder-full-1.0 hi",
            "set-option -w -t @99 @fl_hosted muse:ada",
        ]
    );
}

#[test]
fn the_tmux_runtime_leaves_an_existing_muse_window_alone() {
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install(dir.path(), true, &["mira", "ada"]);
    let spec = muse_spec("hi");
    assert!(!TmuxRuntime.spawn("muse", "ada", None, Some(&spec), "muse:ada"));
    let calls = tmux.calls();
    assert!(
        calls
            .iter()
            .all(|c| c.starts_with("has-session") || c.starts_with("list-windows")),
        "{calls:?}"
    );
}

const LIST_MARKED: &str = "list-windows -t =muse -F #{window_id}\t#{window_name}\t#{@fl_hosted}";

#[test]
fn the_tmux_runtime_kills_a_marked_muse_window_by_its_id_and_never_the_session() {
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install(dir.path(), true, &["mira=muse:mira", "ada=muse:ada"]);
    assert!(TmuxRuntime.kill("muse:ada", "muse:ada"));
    assert_eq!(
        tmux.calls(),
        ["has-session -t =muse", LIST_MARKED, "kill-window -t @2"]
    );
}

#[test]
fn the_tmux_runtime_never_kills_a_muse_window_without_its_mark() {
    // Not marked at all, or marked for another session: someone else's window of the name.
    for window in ["ada", "ada=muse:bob", "ada=muse:ada-2"] {
        let dir = TempDir::new().unwrap();
        let tmux = TmuxLog::install(dir.path(), true, &["mira=muse:mira", window]);
        assert!(
            TmuxRuntime.kill("muse:ada", "muse:ada"),
            "{window}: nothing of ours to kill"
        );
        assert_eq!(tmux.mutations(), Vec::<String>::new(), "{window}");
    }
    // An empty id is nobody's mark: it never matches an unmarked window.
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install(dir.path(), true, &["ada"]);
    assert!(!TmuxRuntime.kill("muse:ada", ""));
    assert!(!TmuxRuntime.owns("muse:ada", ""));
    assert!(tmux.mutations().is_empty());
}

#[test]
fn the_tmux_runtime_kills_nothing_when_the_muse_window_is_gone_or_ambiguous() {
    // Gone (or the session is): nothing to kill, and it counts as stopped.
    let dir = TempDir::new().unwrap();
    {
        let tmux = TmuxLog::install(dir.path(), true, &["mira=muse:mira"]);
        assert!(TmuxRuntime.kill("muse:ada", "muse:ada"));
        assert!(tmux.mutations().is_empty());
    }
    let dir = TempDir::new().unwrap();
    {
        let tmux = TmuxLog::install(dir.path(), false, &[]);
        assert!(TmuxRuntime.kill("muse:ada", "muse:ada"));
        assert!(tmux.mutations().is_empty());
    }
    // Two windows of the name, ours among them: a kill is not made on a guess.
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install(dir.path(), true, &["ada=muse:ada", "ada"]);
    assert!(!TmuxRuntime.kill("muse:ada", "muse:ada"));
    assert!(tmux.mutations().is_empty());
}

#[test]
fn the_tmux_runtime_reports_a_failed_window_kill() {
    let dir = TempDir::new().unwrap();
    let tmux =
        TmuxLog::install_failing(dir.path(), true, &["ada=muse:ada"], "@99", &["kill-window"]);
    // The window is still listed after the failed kill, so it is not gone.
    assert!(!TmuxRuntime.kill("muse:ada", "muse:ada"));
    assert_eq!(tmux.mutations(), ["kill-window -t @1"]);
}

#[test]
fn the_tmux_runtime_owns_only_a_window_carrying_the_mark() {
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install(
        dir.path(),
        true,
        &["ada", "bob=muse:bob", "cy=muse:cy", "dee=muse:dee"],
    );
    assert!(!TmuxRuntime.owns("muse:ada", "muse:ada"), "unmarked");
    assert!(!TmuxRuntime.owns("muse:bob", "muse:ada"), "another mark");
    assert!(TmuxRuntime.owns("muse:cy", "muse:cy"));
    assert!(!TmuxRuntime.owns("muse:eve", "muse:eve"), "no such window");
    // A name is exact: `cy` is not `cyd`.
    assert!(!TmuxRuntime.owns("muse:cyd", "muse:cy"));
    assert!(tmux.mutations().is_empty());
    // Existence alone ignores marks (any window of the name counts as live).
    assert!(TmuxRuntime.exists("muse:ada"));
}

#[test]
fn a_failed_mark_removes_the_window_the_spawn_just_made_by_its_id_and_fails() {
    let dir = TempDir::new().unwrap();
    let spec = muse_spec("hi");
    {
        let tmux =
            TmuxLog::install_failing(dir.path(), true, &["mira=muse:mira"], "@7", &["set-option"]);
        assert!(!TmuxRuntime.spawn("muse", "ada", None, Some(&spec), "muse:ada"));
        let mutations = tmux.mutations();
        assert_eq!(mutations.len(), 3, "{mutations:?}");
        assert!(mutations[0].starts_with("new-window "), "{mutations:?}");
        assert_eq!(
            mutations[1..],
            [
                "set-option -w -t @7 @fl_hosted muse:ada",
                "kill-window -t @7"
            ]
        );
    }

    // The same through the new-session branch.
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install_failing(dir.path(), false, &[], "@3", &["set-option"]);
    assert!(!TmuxRuntime.spawn("muse", "ada", None, Some(&spec), "muse:ada"));
    assert_eq!(tmux.mutations().last().unwrap(), "kill-window -t @3");
}

#[test]
fn a_spawn_that_cannot_read_the_window_id_marks_and_kills_nothing() {
    let spec = muse_spec("hi");
    for out in ["", "ada", "@", "@x1", "%5"] {
        let dir = TempDir::new().unwrap();
        let tmux = TmuxLog::install_failing(dir.path(), true, &[], out, &[]);
        assert!(
            !TmuxRuntime.spawn("muse", "ada", None, Some(&spec), "muse:ada"),
            "{out:?}"
        );
        let mutations = tmux.mutations();
        assert_eq!(mutations.len(), 1, "{out:?}: {mutations:?}");
        assert!(mutations[0].starts_with("new-window "));
    }
}

#[test]
fn the_tmux_runtime_still_kills_a_persona_session_whole() {
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install(dir.path(), true, &[]);
    assert!(TmuxRuntime.kill("Ada", "claude:ada"));
    assert_eq!(tmux.calls()[0], "kill-session -t =Ada");
}

#[test]
fn a_blank_persona_session_is_spawned_as_before_without_a_window_id_or_mark() {
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install(dir.path(), false, &[]);
    assert!(TmuxRuntime.spawn("Ada", "claude", None, None, "claude:ada"));
    assert_eq!(tmux.calls(), ["new-session -d -s Ada -n claude"]);
}

#[test]
fn the_tmux_runtime_asks_for_a_muse_window_by_its_exact_name() {
    let dir = TempDir::new().unwrap();
    let _tmux = TmuxLog::install(dir.path(), true, &["ada-2", "mira"]);
    assert!(!TmuxRuntime.exists("muse:ada"), "a longer name is not hers");
    assert!(TmuxRuntime.exists("muse:mira"));
}

#[test]
fn teardown_kills_only_the_window_it_opened_and_never_the_shared_session() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::with_muse_windows(&["mira"]);
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    let out = teardown(&store, &env, "muse:ada");

    assert!(out.runtime_stopped);
    assert_eq!(killed(&fake), ["kill muse:ada"]);
    assert!(!fake.calls.borrow().iter().any(|c| c == "kill muse"));
    assert!(fake.is_live("muse:mira"), "another persona's window stays");
    assert!(fake.is_live("muse"), "the shared session stays");
    let record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    assert!(!record.runtime_spawned);
}

#[test]
fn teardown_leaves_a_muse_window_it_did_not_create() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::with_muse_windows(&["ada"]);
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    let out = teardown(&store, &env, "muse:ada");

    assert!(!out.runtime_stopped);
    assert!(killed(&fake).is_empty(), "{:?}", fake.calls.borrow());
    assert!(fake.is_live("muse:ada") && fake.is_live("muse"));
}

#[test]
fn two_personas_in_the_shared_session_do_not_block_each_others_teardown() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    spin(&store, &env, "bob", "muse");
    assert_eq!(fake.windows.borrow().len(), 2);

    let first = teardown(&store, &env, "muse:ada");
    assert!(first.runtime_stopped, "bob is not a sharer of ada's window");
    assert_eq!(killed(&fake), ["kill muse:ada"]);
    let bob = find_hosted_session(&store, &["muse:bob"]).unwrap().unwrap();
    assert!(bob.runtime_spawned, "bob still owns his window");
    assert!(fake.is_live("muse:bob") && fake.is_live("muse"));

    let second = teardown(&store, &env, "muse:bob");
    assert!(second.runtime_stopped);
    assert_eq!(killed(&fake), ["kill muse:ada", "kill muse:bob"]);
    // The last window went, so the shared session ended on its own.
    assert!(!fake.is_live("muse"));
}

#[test]
fn a_muse_window_and_a_blank_session_of_one_persona_are_separate_runtimes() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    spin(&store, &env, "ada", "claude");
    let out = teardown(&store, &env, "claude:ada");
    assert!(out.runtime_stopped);
    assert_eq!(killed(&fake), ["kill Ada"]);
    assert!(fake.is_live("muse:ada"));
}

#[test]
fn restart_leaves_a_live_or_foreign_muse_window_alone_and_never_kills() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    // The window lives: the restart spawns nothing and keeps ownership.
    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();
    assert!(out.record.runtime_spawned);
    assert_eq!(fake.launches.borrow().len(), 1);
    assert!(killed(&fake).is_empty());

    // A window `muse-sessions` made: not ours, and a restart does not claim it.
    let store = MemStore::default();
    let fake = FakeRuntime::with_muse_windows(&["bob"]);
    let env = fake.env(home.path());
    spin(&store, &env, "bob", "muse");
    let out = restart_hosted_session(&store, &env, "muse:bob", "owner").unwrap();
    assert!(!out.record.runtime_spawned);
    assert!(fake.launches.borrow().is_empty());
    assert!(!teardown(&store, &env, "muse:bob").runtime_stopped);
    assert!(killed(&fake).is_empty());
    assert!(fake.is_live("muse:bob"));
}

#[test]
fn a_restart_does_not_hand_a_foreign_window_to_an_older_record_of_another_target() {
    // An older record addressed Muse as `Ada:muse` and claimed that session's runtime.
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::with_muse_windows(&["ada"]);
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    let mut record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    record.tmux_target = Some("Ada:muse".into());
    record.runtime_spawned = true;
    persist_hosted_session(&store, &record, "owner").unwrap();

    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();
    assert_eq!(out.record.tmux_target.as_deref(), Some("muse:ada"));
    assert!(
        !out.record.runtime_spawned,
        "the window under it is not ours"
    );
    assert!(!teardown(&store, &env, "muse:ada").runtime_stopped);
    assert!(killed(&fake).is_empty());
    assert!(fake.is_live("muse:ada"));
}

#[test]
fn a_spin_up_does_not_carry_ownership_over_from_a_record_of_another_target() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::with_muse_windows(&["ada"]);
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    let mut record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    record.tmux_target = Some("Ada:muse".into());
    record.runtime_spawned = true;
    persist_hosted_session(&store, &record, "owner").unwrap();

    spin(&store, &env, "ada", "muse");
    let record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    assert_eq!(record.tmux_target.as_deref(), Some("muse:ada"));
    assert!(!record.runtime_spawned);
}

#[test]
fn the_shared_session_name_is_not_a_muse_persona() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let err = spin_up(
        &store,
        &fake.env(home.path()),
        &req("muse", "muse"),
        "owner",
        Caller::Owner,
    )
    .unwrap_err();
    assert_eq!(err.status(), 400, "{err}");
    assert!(fake.calls.borrow().is_empty());
    // On another vendor the name is fine: its session is `Muse`.
    let out = spin(&store, &fake.env(home.path()), "muse", "claude");
    assert_eq!(out.tmux_target.as_deref(), Some("Muse:claude"));
}

// ------------------------------------------------- a stale ownership flag, a stale old session

/// Our `muse:ada` window died; the next window of that name is someone else's.
fn our_window_dies_then_a_foreign_one_opens(fake: &FakeRuntime) {
    fake.close_window("muse:ada");
}

fn a_foreign_window_opens(fake: &FakeRuntime) {
    fake.open_foreign_window("muse:ada");
}

#[test]
fn a_restart_drops_the_ownership_flag_of_a_window_that_died() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    our_window_dies_then_a_foreign_one_opens(&fake);
    // The reopening fails (tmux does not answer), so nothing of ours is under the name.
    fake.fail_windows.borrow_mut().push("ada".to_string());
    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();
    assert!(!out.record.runtime_spawned, "our window is gone");

    fake.fail_windows.borrow_mut().clear();
    a_foreign_window_opens(&fake);
    let stopped = teardown(&store, &env, "muse:ada").runtime_stopped;
    assert!(!stopped);
    assert!(killed(&fake).is_empty(), "{:?}", fake.calls.borrow());
    assert!(fake.is_live("muse:ada"), "the foreign window was killed");
}

#[test]
fn a_restart_that_reopens_the_window_owns_it_again() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    our_window_dies_then_a_foreign_one_opens(&fake);
    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();
    assert!(out.record.runtime_spawned);
    assert_eq!(fake.launches.borrow().len(), 2);
    assert!(teardown(&store, &env, "muse:ada").runtime_stopped);
    assert_eq!(killed(&fake), ["kill muse:ada"]);
}

#[test]
fn a_spin_up_drops_the_ownership_flag_of_a_window_that_died() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    our_window_dies_then_a_foreign_one_opens(&fake);
    fake.fail_windows.borrow_mut().push("ada".to_string());
    spin(&store, &env, "ada", "muse");
    let record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    assert!(!record.runtime_spawned, "our window is gone");

    fake.fail_windows.borrow_mut().clear();
    a_foreign_window_opens(&fake);
    assert!(!teardown(&store, &env, "muse:ada").runtime_stopped);
    assert!(killed(&fake).is_empty(), "{:?}", fake.calls.borrow());
    assert!(fake.is_live("muse:ada"), "the foreign window was killed");
}

#[test]
fn a_spin_up_keeps_the_ownership_flag_of_a_window_that_still_runs() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    spin(&store, &env, "ada", "muse");
    let record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    assert!(record.runtime_spawned);
    assert!(teardown(&store, &env, "muse:ada").runtime_stopped);
}

#[test]
fn a_restart_of_a_stale_display_record_with_a_live_old_session_opens_no_second_muse() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    // An older record addressed Muse as `Ada:muse`, and that session still runs.
    let mut record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
    record.tmux_target = Some("Ada:muse".into());
    record.runtime_spawned = true;
    persist_hosted_session(&store, &record, "owner").unwrap();
    fake.live.borrow_mut().insert("Ada".to_string());
    fake.close_window("muse:ada");
    fake.calls.borrow_mut().clear();
    fake.launches.borrow_mut().clear();
    let listener = webhook_listener(home.path(), "ada");

    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();
    assert_eq!(out.record.tmux_target.as_deref(), Some("Ada:muse"));
    assert!(out.record.runtime_spawned, "ownership is unchanged");
    assert!(
        fake.calls.borrow().iter().all(|c| !c.starts_with("spawn")),
        "{:?}",
        fake.calls.borrow()
    );
    assert!(fake.launches.borrow().is_empty());
    assert!(!fake.is_live("muse:ada"));
    assert!(out.record.hydration_dispatched);
    assert!(delivered(&listener), "the prompt goes to the existing Muse");
}

#[test]
fn the_shared_session_and_an_empty_window_are_never_killed() {
    let dir = TempDir::new().unwrap();
    let tmux = TmuxLog::install(dir.path(), true, &["ada=muse:ada"]);
    for unit in ["muse", "muse:", ":ada", ":", ""] {
        assert!(!TmuxRuntime.kill(unit, "muse:ada"), "{unit:?}");
    }
    assert!(tmux.calls().is_empty(), "{:?}", tmux.calls());

    let fake = FakeRuntime::with_muse_windows(&["ada", "bob"]);
    for unit in ["muse", "muse:", ":ada", ""] {
        assert!(!fake.kill(unit, "muse:ada"), "{unit:?}");
    }
    assert!(fake.is_live("muse:ada") && fake.is_live("muse:bob") && fake.is_live("muse"));
}

// -------------------------------------------- a window is ours only when it carries our mark

/// `muse:ada` is hosted and its window was opened by the lifecycle; then it dies and a foreign
/// window takes the name.
fn ours_dies_then_a_foreign_window_takes_the_name(
    store: &MemStore,
    env: &LifecycleEnv<'_>,
    fake: &FakeRuntime,
) {
    spin(store, env, "ada", "muse");
    assert!(record_of(store).runtime_spawned);
    fake.close_window("muse:ada");
    fake.open_foreign_window("muse:ada");
}

fn record_of(store: &MemStore) -> HostedSessionRecord {
    find_hosted_session(store, &["muse:ada"]).unwrap().unwrap()
}

#[test]
fn a_spawn_marks_its_muse_window_with_the_hosted_session_id_and_a_blank_session_gets_no_mark() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    spin(&store, &env, "ada", "claude");
    assert_eq!(
        *fake.marks.borrow(),
        [("muse:ada".to_string(), "muse:ada".to_string())].into(),
        "only the Muse window is marked, with the hosted session id"
    );
    assert!(fake.owns("muse:ada", "muse:ada"));
    assert!(!fake.owns("muse:ada", "muse:bob"));
    assert!(fake.owns("Ada", "claude:ada"), "a session needs no mark");
}

#[test]
fn a_restart_does_not_claim_or_kill_a_foreign_window_that_took_the_name_of_a_dead_one() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    ours_dies_then_a_foreign_window_takes_the_name(&store, &env, &fake);
    let listener = webhook_listener(home.path(), "ada");

    // The name exists, so a name check keeps the flag; the mark does not.
    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();
    assert!(!out.record.runtime_spawned, "the window is not ours");
    assert_eq!(fake.launches.borrow().len(), 1, "nothing new was opened");
    assert!(out.record.hydration_dispatched);
    assert!(delivered(&listener), "the prompt goes through delivery");

    let stopped = teardown(&store, &env, "muse:ada");
    assert!(!stopped.runtime_stopped);
    assert!(killed(&fake).is_empty(), "{:?}", fake.calls.borrow());
    assert!(fake.is_live("muse:ada") && fake.is_live("muse"));
    assert!(!record_of(&store).runtime_spawned);
}

#[test]
fn a_spin_up_does_not_claim_or_kill_a_foreign_window_that_took_the_name_of_a_dead_one() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    ours_dies_then_a_foreign_window_takes_the_name(&store, &env, &fake);
    let listener = webhook_listener(home.path(), "ada");

    let out = spin(&store, &env, "ada", "muse");
    assert!(!out.runtime_spawned);
    assert!(!record_of(&store).runtime_spawned);
    assert_eq!(fake.launches.borrow().len(), 1, "nothing new was opened");
    assert!(out.hydration_dispatched);
    assert!(delivered(&listener));

    assert!(!teardown(&store, &env, "muse:ada").runtime_stopped);
    assert!(killed(&fake).is_empty(), "{:?}", fake.calls.borrow());
    assert!(fake.is_live("muse:ada"));
}

#[test]
fn a_teardown_with_a_stale_flag_never_kills_a_foreign_window_nor_reports_it_stopped() {
    // No restart or spin-up repaired the flag: the foreign window is under the name, the flag
    // still says ours. The runtime is asked, and it refuses a window without our mark.
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    ours_dies_then_a_foreign_window_takes_the_name(&store, &env, &fake);
    assert!(record_of(&store).runtime_spawned, "the flag is stale");

    let out = teardown(&store, &env, "muse:ada");
    assert!(
        !out.runtime_stopped,
        "a foreign window is not a stopped runtime"
    );
    assert!(fake.is_live("muse:ada"), "the foreign window was killed");
    assert!(!record_of(&store).runtime_spawned, "the claim is cleared");

    // A window marked for another session is no different.
    let store = MemStore::default();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    ours_dies_then_a_foreign_window_takes_the_name(&store, &env, &fake);
    fake.marks
        .borrow_mut()
        .insert("muse:ada".to_string(), "muse:bob".to_string());
    assert!(!teardown(&store, &env, "muse:ada").runtime_stopped);
    assert!(
        fake.is_live("muse:ada"),
        "a window with another mark was killed"
    );
}

#[test]
fn a_teardown_kills_the_marked_window_it_opened() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    let out = teardown(&store, &env, "muse:ada");
    assert!(out.runtime_stopped);
    assert_eq!(killed(&fake), ["kill muse:ada"]);
    assert!(!fake.is_live("muse:ada") && fake.marks.borrow().is_empty());
}

#[test]
fn a_launched_command_survives_only_as_the_marked_window_not_as_any_window_of_the_name() {
    let home = TempDir::new().unwrap();
    // Any window of the name, but not the one this lifecycle opened.
    let fake = FakeRuntime::with_muse_windows(&["ada"]);
    let env = fake.env(home.path());
    assert!(!launch_survived(&env, "muse:ada", "muse:ada"));
    assert_eq!(
        fake.exists_calls.get(),
        0,
        "survival does not go by the name"
    );

    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    assert!(fake.spawn("muse", "ada", None, Some(&muse_spec("hi")), "muse:ada"));
    assert!(launch_survived(&env, "muse:ada", "muse:ada"));
    assert!(!launch_survived(&env, "muse:ada", "muse:bob"));
}

#[test]
fn a_hand_edited_flag_does_not_make_a_window_without_the_mark_ours_on_restart() {
    // The record says it spawned the window; the window under the name has no mark.
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::with_muse_windows(&["ada"]);
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    let mut record = record_of(&store);
    record.runtime_spawned = true;
    persist_hosted_session(&store, &record, "owner").unwrap();

    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();
    assert!(!out.record.runtime_spawned);
    assert!(!teardown(&store, &env, "muse:ada").runtime_stopped);
    assert!(killed(&fake).is_empty());
}

#[test]
fn non_muse_vendors_keep_a_blank_session_that_teardown_kills_by_name() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    for vendor in ["claude", "codex", "grok"] {
        spin(&store, &env, "ada", vendor);
        let out = teardown(&store, &env, &format!("{vendor}:ada"));
        assert!(out.runtime_stopped, "{vendor}");
    }
    assert_eq!(killed(&fake), ["kill Ada", "kill Ada", "kill Ada"]);
    assert!(fake.marks.borrow().is_empty());
}
