//! Muse launch tests: a spin-up or restart starts the Muse CLI in its tmux window with the
//! hydration prompt as its first input, so the prompt is not delivered a second time; every other
//! vendor still gets a blank window. Nothing here runs the real `tmux` or `muse`: the runtime is
//! a `FakeRuntime`, and discovery reads a tempdir home and, in the end-to-end test, a fake tmux.

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

/// A fake `<home>/.local/bin/muse`, so the binary resolves to a path of the tempdir.
fn install_home_muse(home: &Path) -> PathBuf {
    let bin = home.join(".local").join("bin").join("muse");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    bin
}

fn muse_launch(prompt: &str, home: &Path, persona: &str) -> LaunchSpec {
    LaunchSpec {
        env: vec![("AGENT_PERSONA".into(), persona.into())],
        argv: vec![
            home.join(".local/bin/muse").to_string_lossy().into_owned(),
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
    assert_eq!(launches[0].0, "Ada:muse");
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
fn only_muse_is_launched_other_vendors_open_a_blank_window() {
    for vendor in ["claude", "codex", "grok", "antigravity"] {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let out = spin(&store, &fake.env(home.path()), "ada", vendor);
        assert!(out.runtime_spawned, "{vendor}");
        assert_eq!(
            *fake.launches.borrow(),
            [(format!("Ada:{vendor}"), None)],
            "{vendor}"
        );
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
    fake.live.borrow_mut().clear();
    let out = restart_hosted_session(&store, &env, "muse:ada", "owner").unwrap();

    let launches = fake.launches.borrow();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert_eq!(launches[1].0, "Ada:muse");
    assert_eq!(
        launches[1].1,
        Some(muse_launch(&out.prompt, home.path(), "ada"))
    );
    assert!(out.record.runtime_spawned);
}

#[test]
fn restart_opens_other_vendors_blank() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "codex");
    fake.live.borrow_mut().clear();
    restart_hosted_session(&store, &env, "codex:ada", "owner").unwrap();
    assert_eq!(
        *fake.launches.borrow(),
        [
            ("Ada:codex".to_string(), None),
            ("Ada:codex".to_string(), None)
        ]
    );
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
    // The tmux session `Ada` exists already: nothing is launched.
    let fake = FakeRuntime::running(&["Ada"]);
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
fn a_launched_other_vendor_is_still_hydrated_by_delivery() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let listener = webhook_listener(home.path(), "ada");
    let fake = FakeRuntime::default();
    let out = spin(&store, &fake.env(home.path()), "ada", "claude");
    assert!(out.runtime_spawned && out.hydration_dispatched);
    assert!(delivered(&listener));
}

#[test]
fn restart_of_a_launched_muse_does_not_deliver_the_prompt_again() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "muse");
    fake.live.borrow_mut().clear();
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
    fake.vanish_windows.borrow_mut().push("muse".into());
    let out = spin(&store, &fake.env(home.path()), "ada", "muse");

    assert!(!out.runtime_spawned);
    assert!(!out.hydration_dispatched);
    assert_eq!(fake.exists_calls.get(), 5, "bounded retry");
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
    fake.live.borrow_mut().clear();
    fake.vanish_windows.borrow_mut().push("muse".into());
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
    assert_eq!(fake.exists_calls.get(), 1);
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
                "#!/bin/sh\nif [ \"$1\" = \"list-windows\" ]; then\n{list}elif [ \"$1\" = \"display-message\" ]; then\n  echo '%5'\nfi\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
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
        assert_eq!(out.tmux_target.as_deref(), Some("Muselaunch:muse"));
        assert!(fake.is_live("Muselaunch"));

        // The window the launch opened, as tmux lists it. No transcript exists yet.
        let query = RouteQuery::persona("muselaunch", &out.session_id);
        let _tmux = FakeTmuxWindows::install(home.path(), &["Muselaunch:muse", "Other:zsh"]);
        let route = lookup_route(Some(home.path()), query)
            .expect("the window makes a route")
            .into_route();

        assert_eq!(
            route,
            VendorRoute::Muse {
                session_id: String::new(),
                cwd: None,
                tmux_target: Some("Muselaunch:muse".into()),
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
