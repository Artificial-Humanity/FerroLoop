//! Cross-process lifecycle contention; all homes/endpoints/stores are private fixtures.
#![cfg(unix)]
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn command(root: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fl"));
    cmd.env_clear()
        .current_dir(root)
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home)
        .env("FL_HOST_SOCK", root.join("host.sock"))
        .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
        .env("FL_TEST_SPINUP_NO_SPAWN", "1")
        .args(["--db"])
        .arg(home.join("fl.fls"));
    cmd
}
fn wait_file(path: &Path, child: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lifecycle fixture did not become ready: {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// This holder uses the exact public lifecycle entry used by the HTTP spin-up endpoint.
// The store reopen barrier is after ownership and before effects, with no live runtime.
#[test]
fn lifecycle_fence_holder_child() {
    use fl_hub::lifecycle::{self, Caller, LifecycleEnv, LifecycleError, SpinUpRequest};
    let Some(root) = std::env::var_os("FL_TEST_FENCE_HOLDER").map(PathBuf::from) else {
        return;
    };
    let home = root.join("holder");
    let store = fl_core::MemStore::default();
    let mut env = LifecycleEnv::new(Some(&home));
    env.spawn_runtime = false;
    let request = SpinUpRequest {
        persona: "ada".into(),
        project: "ferroloop".into(),
        role: "assistant".into(),
        vendor: "claude".into(),
        ..Default::default()
    };
    let out = lifecycle::spin_up_with_store(
        &store,
        || {
            std::fs::write(root.join("ready"), "ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while !root.join("release").exists() {
                assert!(Instant::now() < deadline, "holder release deadline");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err::<&fl_core::MemStore, _>(LifecycleError::Store("fixture release".into()))
        },
        &env,
        &request,
        "owner",
        Caller::Owner,
    );
    assert!(matches!(out, Err(LifecycleError::Store(detail)) if detail == "fixture release"));
}

#[test]
fn lifecycle_fence_http_contender_child() {
    let Some(root) = std::env::var_os("FL_TEST_FENCE_HTTP").map(PathBuf::from) else {
        return;
    };
    let home = root.join("contender");
    let state = fl_hub::HubState::new(fl_hub::HubServerConfig {
        store_path: home.join("fl.fls"),
        repo_root: home.clone(),
        home_override: Some(home),
        owner_token: Some("fixture-owner".into()),
        ..Default::default()
    });
    for (path, payload) in [
        (
            "/api/v1/sessions/spin-up",
            serde_json::json!({"persona":"ada","project":"ferroloop","role":"assistant","vendor":"claude"}),
        ),
        (
            "/api/v1/hosted-sessions/claude:ada/restart",
            serde_json::json!({}),
        ),
        (
            "/api/v1/sessions/claude:ada/decommission",
            serde_json::json!({"mode":"prepare"}),
        ),
        (
            "/api/v1/sessions/claude:ada/decommission",
            serde_json::json!({"mode":"teardown"}),
        ),
        (
            "/api/v1/sessions/claude:ada/remodel",
            serde_json::json!({"vendor":"claude"}),
        ),
    ] {
        let request = fl_hub::HttpRequest::new("POST", path)
            .with_header("authorization", "Bearer fixture-owner")
            .with_json_body(&payload);
        let response = fl_hub::handle_http_request(&state, &request);
        assert_eq!(
            response.status,
            409,
            "{path}: {}",
            String::from_utf8_lossy(&response.body)
        );
        assert!(String::from_utf8_lossy(&response.body).contains("in progress"));
    }
}

#[test]
fn held_lifecycle_rejects_cli_and_http_without_record_or_budget_changes() {
    let root = tempfile::tempdir().unwrap();
    let holder_home = root.path().join("holder");
    let contender_home = root.path().join("contender");
    std::fs::create_dir(&holder_home).unwrap();
    std::fs::create_dir(&contender_home).unwrap();
    let seeded = command(root.path(), &contender_home)
        .args([
            "agent",
            "spin-up",
            "ada",
            "--project",
            "ferroloop",
            "--role",
            "assistant",
            "--vendor",
            "claude",
            "--by",
            "owner",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        seeded.status.success(),
        "{}",
        String::from_utf8_lossy(&seeded.stderr)
    );
    let snapshot = || {
        let out = command(root.path(), &contender_home)
            .args(["state", "list", "--scope", "workspace", "--json"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let before = snapshot();
    let mut holder = Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &holder_home)
        .env("FL_HOST_SOCK", root.path().join("host.sock"))
        .env("FL_TEST_FENCE_HOLDER", root.path())
        .args(["--exact", "lifecycle_fence_holder_child", "--nocapture"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_file(&root.path().join("ready"), &mut holder);
    let commands = [
        vec![
            "agent",
            "spin-up",
            "ada",
            "--project",
            "ferroloop",
            "--role",
            "assistant",
            "--vendor",
            "claude",
            "--by",
            "owner",
            "--json",
        ],
        vec!["agent", "restart", "ada", "--by", "owner", "--json"],
        vec![
            "agent",
            "decommission",
            "claude:ada",
            "--mode",
            "prepare",
            "--by",
            "owner",
            "--json",
        ],
        vec![
            "agent",
            "decommission",
            "claude:ada",
            "--mode",
            "teardown",
            "--by",
            "owner",
            "--json",
        ],
        vec![
            "agent",
            "remodel",
            "claude:ada",
            "--vendor",
            "claude",
            "--by",
            "owner",
            "--json",
        ],
    ];
    for args in commands {
        let out = command(root.path(), &contender_home)
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("in progress"));
    }
    let http = Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &contender_home)
        .env("FL_HOST_SOCK", root.path().join("host.sock"))
        .env("FL_TEST_SPINUP_NO_SPAWN", "1")
        .env("FL_TEST_FENCE_HTTP", root.path())
        .args([
            "--exact",
            "lifecycle_fence_http_contender_child",
            "--nocapture",
        ])
        .output()
        .unwrap();
    let after = snapshot();
    std::fs::write(root.path().join("release"), "release").unwrap();
    assert!(holder.wait().unwrap().success());
    assert!(
        http.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&http.stdout),
        String::from_utf8_lossy(&http.stderr)
    );
    assert_eq!(
        after, before,
        "losing lifecycle changed runtime/recovery entries"
    );
}

#[test]
fn actual_cli_holder_rejects_http_before_any_losing_runtime_or_store_effect() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let holder_home = root.path().join("holder");
    let contender_home = root.path().join("contender");
    std::fs::create_dir(&holder_home).unwrap();
    std::fs::create_dir(&contender_home).unwrap();
    let seeded = command(root.path(), &contender_home)
        .args([
            "agent",
            "spin-up",
            "ada",
            "--project",
            "ferroloop",
            "--role",
            "assistant",
            "--vendor",
            "claude",
            "--by",
            "owner",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        seeded.status.success(),
        "{}",
        String::from_utf8_lossy(&seeded.stderr)
    );
    let snapshot = || {
        let out = command(root.path(), &contender_home)
            .args(["state", "list", "--scope", "workspace", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success());
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let before = snapshot();
    // The actual CLI owns the fence during its runtime spawn, not a synthetic
    // callback in a helper. This stub is private and never invokes a real tmux.
    let tmux = root.path().join("tmux-stub");
    std::fs::write(
        &tmux,
        r#"#!/bin/sh
[ "$1" = -S ] && shift 2
case "$1" in
new-session)
  printf 'spawn\n' >> "$FIXTURE_ROOT/effects"
  : > "$FIXTURE_ROOT/ready"
  while [ ! -e "$FIXTURE_ROOT/release" ]; do /bin/sleep 0.01; done
  exit 1;;
kill*|send*|paste*) printf '%s\n' "$1" >> "$FIXTURE_ROOT/effects"; exit 0;;
*) exit 1;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut holder = command(root.path(), &holder_home)
        .env_remove("FL_TEST_SPINUP_NO_SPAWN")
        .env("FIXTURE_ROOT", root.path())
        .env("FL_WIRE_TMUX_BIN", &tmux)
        .env(
            "TMUX",
            format!("{},0,0", root.path().join("tmux.sock").display()),
        )
        .args([
            "agent",
            "spin-up",
            "ada",
            "--project",
            "ferroloop",
            "--role",
            "assistant",
            "--vendor",
            "claude",
            "--by",
            "owner",
            "--json",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_file(&root.path().join("ready"), &mut holder);
    let effects_before = std::fs::read(root.path().join("effects")).unwrap();
    let http = Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &contender_home)
        .env("FL_HOST_SOCK", root.path().join("host.sock"))
        .env("FL_WIRE_TMUX_BIN", &tmux)
        .env("FIXTURE_ROOT", root.path())
        .env(
            "TMUX",
            format!("{},0,0", root.path().join("tmux.sock").display()),
        )
        .env("FL_TEST_SPINUP_NO_SPAWN", "1")
        .env("FL_TEST_FENCE_HTTP", root.path())
        .args([
            "--exact",
            "lifecycle_fence_http_contender_child",
            "--nocapture",
        ])
        .output()
        .unwrap();
    let background = Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &contender_home)
        .env("FL_HOST_SOCK", root.path().join("host.sock"))
        .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
        .env("FL_WIRE_TMUX_BIN", &tmux)
        .env("FIXTURE_ROOT", root.path())
        .env(
            "TMUX",
            format!("{},0,0", root.path().join("tmux.sock").display()),
        )
        .env("FL_TEST_FENCE_BACKGROUND", root.path())
        .args([
            "--exact",
            "lifecycle_background_contender_child",
            "--nocapture",
        ])
        .output()
        .unwrap();
    let effects_after = std::fs::read(root.path().join("effects")).unwrap();
    let after = snapshot();
    std::fs::write(root.path().join("release"), "release").unwrap();
    holder.wait().unwrap();
    assert!(
        background.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&background.stdout),
        String::from_utf8_lossy(&background.stderr)
    );
    assert!(
        http.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&http.stdout),
        String::from_utf8_lossy(&http.stderr)
    );
    assert_eq!(
        effects_after, effects_before,
        "loser spawned, prompted or killed a runtime"
    );
    assert_eq!(
        after, before,
        "loser changed a record, tombstone or recovery budget"
    );
}

// Each operation gets its own real fixture store. The CLI owner uses another home
// but the same actual endpoint; the loser's runtime would reach the tmux effect log.
#[test]
fn lifecycle_background_contender_child() {
    use fl_core::StateStore;
    use fl_hub::lifecycle::{self, HostedRespawnState, LifecycleEnv};
    let Some(root) = std::env::var_os("FL_TEST_FENCE_BACKGROUND").map(PathBuf::from) else {
        return;
    };
    let original = {
        let store = fl_store::open_store(&root.join("contender/fl.fls")).unwrap();
        lifecycle::get_hosted_session_by_id(&store, "claude:ada")
            .unwrap()
            .unwrap()
    };
    for operation in ["dead", "health", "pid", "auto", "get"] {
        let home = root.join("background").join(operation);
        std::fs::create_dir_all(&home).unwrap();
        let hub = fl_hub::HubState::new(fl_hub::HubServerConfig {
            store_path: home.join("fixture.fls"),
            repo_root: home.clone(),
            home_override: Some(home.clone()),
            owner_token: Some("fixture-owner".into()),
            ..Default::default()
        });
        let mut record = original.clone();
        record.runtime_spawned = true;
        if operation == "auto" {
            record.status = "stopped".into();
            record.stop_reason = Some("crash_loop".into());
        }
        let before = {
            let store = hub.open_store().unwrap();
            lifecycle::persist_hosted_session(&*store, &record, "fixture").unwrap();
            lifecycle::save_hosted_respawn_state(
                &*store,
                &record.session_id,
                &HostedRespawnState {
                    recovery_started_at_ms: Some(1),
                    recovery_attempts: if operation == "auto" { 6 } else { 1 },
                    next_allowed_at_ms: Some(1),
                    ..Default::default()
                },
                "fixture",
            )
            .unwrap();
            store.state_revisions_after(0).unwrap().len()
        };
        let effects = std::fs::read(root.join("effects")).unwrap();
        let env = LifecycleEnv::new(Some(&home));
        let mut events = Vec::new();
        match operation {
            "dead" | "health" => {
                lifecycle::reconcile_dead_hosted_sessions_with_store(
                    hub.open_store().unwrap(),
                    || hub.open_store().map_err(lifecycle::LifecycleError::from),
                    &env,
                    "fixture",
                )
                .unwrap();
            }
            "pid" => {
                lifecycle::reconcile_running_session_pids_in_hub(&hub).unwrap();
            }
            "auto" => {
                assert!(
                    lifecycle::reconcile_and_respawn_hosted_sessions_with_store(
                        hub.open_store().unwrap(),
                        || hub.open_store().map_err(lifecycle::LifecycleError::from),
                        &env,
                        |name, _| events.push(name.to_string())
                    )
                    .unwrap()
                    .is_empty()
                );
            }
            _ => {
                let response = fl_hub::handle_http_request(
                    &hub,
                    &fl_hub::HttpRequest::new("GET", "/api/v1/sessions")
                        .with_header("authorization", "Bearer fixture-owner"),
                );
                assert_eq!(response.status, 200);
            }
        }
        let store = hub.open_store().unwrap();
        assert_eq!(
            store.state_revisions_after(0).unwrap().len(),
            before,
            "{operation}: no losing write"
        );
        assert_eq!(
            lifecycle::get_hosted_session_by_id(&*store, &record.session_id).unwrap(),
            Some(record),
            "{operation}"
        );
        assert!(events.is_empty(), "{operation}: no losing event");
        assert_eq!(
            std::fs::read(root.join("effects")).unwrap(),
            effects,
            "{operation}: no losing runtime probe/effect"
        );
    }
}

#[test]
fn lifecycle_background_owner_child() {
    use fl_hub::lifecycle;
    let Some(root) = std::env::var_os("FL_TEST_FENCE_BACKGROUND_OWNER").map(PathBuf::from) else {
        return;
    };
    let operation = std::env::var("FL_TEST_BACKGROUND_OPERATION").unwrap();
    let home = root.join("contender");
    let hub = fl_hub::HubState::new(fl_hub::HubServerConfig {
        store_path: home.join("fl.fls"),
        repo_root: home.clone(),
        home_override: Some(home),
        owner_token: Some("fixture-owner".into()),
        ..Default::default()
    });
    if operation == "auto" {
        let store = hub.open_store().unwrap();
        let mut record = lifecycle::get_hosted_session_by_id(&*store, "claude:ada")
            .unwrap()
            .unwrap();
        record.status = "stopped".into();
        record.stop_reason = Some("runtime_gone".into());
        lifecycle::persist_hosted_session(&*store, &record, "fixture").unwrap();
    }
    match operation.as_str() {
        "dead" | "health" => {
            lifecycle::reconcile_dead_hosted_sessions_in_hub(&hub, "fixture").unwrap();
        }
        "pid" => {
            lifecycle::reconcile_running_session_pids_in_hub(&hub).unwrap();
        }
        "auto" => {
            lifecycle::reconcile_and_respawn_hosted_sessions_in_hub(&hub).unwrap();
        }
        _ => {
            let response = fl_hub::handle_http_request(
                &hub,
                &fl_hub::HttpRequest::new("GET", "/api/v1/sessions")
                    .with_header("authorization", "Bearer fixture-owner"),
            );
            assert_eq!(response.status, 200);
        }
    }
}

#[test]
fn background_dead_health_auto_pid_and_get_owners_reject_actual_cli() {
    use std::os::unix::fs::PermissionsExt;
    for operation in ["dead", "health", "auto", "pid", "get"] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("contender");
        std::fs::create_dir(&home).unwrap();
        let seeded = command(root.path(), &home)
            .args([
                "agent",
                "spin-up",
                "ada",
                "--project",
                "ferroloop",
                "--role",
                "assistant",
                "--vendor",
                "claude",
                "--by",
                "owner",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(
            seeded.status.success(),
            "{}",
            String::from_utf8_lossy(&seeded.stderr)
        );
        let tmux = root.path().join("tmux-stub");
        std::fs::write(
            &tmux,
            r#"#!/bin/sh
printf 'observed\n' >> "$FIXTURE_ROOT/effects"
: > "$FIXTURE_ROOT/ready"
while [ ! -e "$FIXTURE_ROOT/release" ]; do /bin/sleep 0.01; done
exit 1
"#,
        )
        .unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut owner = Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("FL_HOST_SOCK", root.path().join("host.sock"))
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env("FL_WIRE_TMUX_BIN", &tmux)
            .env("FIXTURE_ROOT", root.path())
            .env(
                "TMUX",
                format!("{},0,0", root.path().join("tmux.sock").display()),
            )
            .env("FL_TEST_FENCE_BACKGROUND_OWNER", root.path())
            .env("FL_TEST_BACKGROUND_OPERATION", operation)
            .args(["--exact", "lifecycle_background_owner_child", "--nocapture"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        wait_file(&root.path().join("ready"), &mut owner);
        let before = std::fs::read(root.path().join("effects")).unwrap();
        let contender_home = root.path().join("other-home");
        std::fs::create_dir(&contender_home).unwrap();
        let losing = command(root.path(), &contender_home)
            .args([
                "agent",
                "spin-up",
                "ada",
                "--project",
                "ferroloop",
                "--role",
                "assistant",
                "--vendor",
                "claude",
                "--by",
                "owner",
                "--json",
            ])
            .output()
            .unwrap();
        let after = std::fs::read(root.path().join("effects")).unwrap();
        std::fs::write(root.path().join("release"), "release").unwrap();
        let finished = owner.wait().unwrap();
        assert!(finished.success(), "{operation}");
        assert_eq!(
            losing.status.code(),
            Some(2),
            "{operation}: {}",
            String::from_utf8_lossy(&losing.stderr)
        );
        assert!(
            String::from_utf8_lossy(&losing.stderr).contains("in progress"),
            "{operation}"
        );
        assert_eq!(after, before, "{operation}: no losing runtime effect");
    }
}
