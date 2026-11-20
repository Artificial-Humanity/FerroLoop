//! Effectful fence regressions: execute only through the approved sandbox runner or CI.
#![cfg(unix)]
use super::*;
use fl_host_proto::HostEndpoint;
use tempfile::TempDir;

fn endpoint(root: &TempDir) -> HostEndpoint {
    HostEndpoint::unix(root.path().canonicalize().unwrap().join("host.sock")).unwrap()
}

#[test]
fn lifecycle_fence_contends_across_open_descriptions_and_normalizes_persona() {
    let root = TempDir::new().unwrap();
    let ep = endpoint(&root);
    let held =
        lock::LifecycleFence::acquire_endpoint(ep.clone(), None, "Fiona", "spin-up").unwrap();
    let error =
        lock::LifecycleFence::acquire_endpoint(ep.clone(), None, "fiona", "restart").unwrap_err();
    assert_eq!(error.status(), 409);
    let message = error.to_string();
    assert!(message.contains("spin-up"), "{message}");
    assert!(
        message.contains(&std::process::id().to_string()),
        "{message}"
    );
    // A different persona can progress without waiting for this holder.
    let other =
        lock::LifecycleFence::acquire_endpoint(ep.clone(), None, "felix", "restart").unwrap();
    drop(held);
    lock::LifecycleFence::acquire_endpoint(ep, None, "fiona", "teardown").unwrap();
    drop(other);
}

#[test]
fn lifecycle_fixture_homes_do_not_share_the_discovered_host_fence() {
    const CHILD: &str = "FL_TEST_LIFECYCLE_FOREIGN_HOST";
    if let Some(socket) = std::env::var_os(CHILD).map(PathBuf::from) {
        let _fixture = fl_host_proto::register_hub_test_host_fixture(&socket).unwrap();
        let discovered = HostEndpoint::discover().unwrap();
        assert_eq!(discovered, HostEndpoint::unix(&socket).unwrap());

        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let first_env = LifecycleEnv::new(Some(first.path()));
        let second_env = LifecycleEnv::new(Some(second.path()));
        let _first = lock::LifecycleFence::acquire(&first_env, "ada", "spin-up").unwrap();
        let _second = lock::LifecycleFence::acquire(&second_env, "ada", "teardown")
            .expect("distinct fixture homes must not share Ada's fence");
        assert_eq!(
            lock::LifecycleFence::acquire(&first_env, "ada", "restart")
                .unwrap_err()
                .status(),
            409
        );

        let _discovered =
            lock::LifecycleFence::acquire_endpoint(discovered, None, "ada", "host-fixture")
                .unwrap();
        assert_eq!(
            lock::LifecycleFence::acquire(&LifecycleEnv::new(None), "ada", "restart")
                .unwrap_err()
                .status(),
            409
        );
        return;
    }

    let root = TempDir::new().unwrap();
    let socket = root.path().join("host.sock");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lifecycle::fence_tests::lifecycle_fixture_homes_do_not_share_the_discovered_host_fence",
        ])
        .env(CHILD, &socket)
        .env("FL_HOST_SOCK", &socket)
        .env_remove("FL_WIRE_TMUX_BIN")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lifecycle fixture child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn lifecycle_fence_canonical_socket_aliases_share_the_same_lock() {
    let root = TempDir::new().unwrap();
    let real = root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let ep = HostEndpoint::unix(real.join("host.sock")).unwrap();
    let _held = lock::LifecycleFence::acquire_endpoint(ep, None, "fiona", "restart").unwrap();
    let ep = HostEndpoint::unix(alias.join("host.sock")).unwrap();
    assert_eq!(
        lock::LifecycleFence::acquire_endpoint(ep, None, "fiona", "spin-up")
            .unwrap_err()
            .status(),
        409
    );
}

#[cfg(unix)]
#[test]
fn lifecycle_fence_rejects_unsafe_lock_file_and_directory_without_repairing_them() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let ep = endpoint(&root);
    let held =
        lock::LifecycleFence::acquire_endpoint(ep.clone(), None, "fiona", "restart").unwrap();
    let path = held.paths()[0].clone();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    drop(held);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(lock::LifecycleFence::acquire_endpoint(ep.clone(), None, "fiona", "restart").is_err());
    std::fs::remove_file(&path).unwrap();
    let target = root.path().join("target");
    std::fs::write(&target, "do not change").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(lock::LifecycleFence::acquire_endpoint(ep, None, "fiona", "restart").is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "do not change");
    std::fs::remove_file(&path).unwrap();
    let directory = path.parent().unwrap();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        lock::LifecycleFence::acquire_endpoint(endpoint(&root), None, "fiona", "restart").is_err()
    );
    assert_eq!(
        std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn lifecycle_fence_tmux_contention_releases_partial_native_lock() {
    let root = TempDir::new().unwrap();
    let ep_a = endpoint(&root);
    let ep_b = HostEndpoint::unix(root.path().join("other.sock")).unwrap();
    let tmux = root.path().join("tmux.sock");
    let held = lock::LifecycleFence::acquire_endpoint(ep_a, Some(tmux.clone()), "fiona", "remodel")
        .unwrap();
    assert_eq!(
        lock::LifecycleFence::acquire_endpoint(ep_b.clone(), Some(tmux), "fiona", "restart")
            .unwrap_err()
            .status(),
        409
    );
    // Failed second-fence acquisition must release its first fence.
    lock::LifecycleFence::acquire_endpoint(ep_b, None, "fiona", "restart").unwrap();
    drop(held);
}

#[test]
fn lifecycle_fence_runtime_pin_restores_on_nested_scope_and_error() {
    let root = TempDir::new().unwrap();
    let a = endpoint(&root);
    let b = HostEndpoint::unix(root.path().join("other.sock")).unwrap();
    assert!(lock::pinned_endpoint().is_none());
    {
        let held =
            lock::LifecycleFence::acquire_endpoint(a.clone(), None, "fiona", "restart").unwrap();
        let _pin = held.pin();
        assert_eq!(lock::pinned_endpoint(), Some(a.clone()));
        {
            let nested =
                lock::LifecycleFence::acquire_endpoint(b.clone(), None, "felix", "restart")
                    .unwrap();
            let _pin = nested.pin();
            assert_eq!(lock::pinned_endpoint(), Some(b));
        }
        assert_eq!(lock::pinned_endpoint(), Some(a));
    }
    assert!(lock::pinned_endpoint().is_none());
}

#[test]
fn lifecycle_fence_metadata_cannot_steal_or_block_an_unheld_lock() {
    let root = TempDir::new().unwrap();
    let ep = endpoint(&root);
    let held =
        lock::LifecycleFence::acquire_endpoint(ep.clone(), None, "fiona", "restart").unwrap();
    let path = held.paths()[0].clone();
    std::fs::write(&path, r#"{"pid":0,"operation":"fake-dead-holder"}"#).unwrap();
    assert_eq!(
        lock::LifecycleFence::acquire_endpoint(ep.clone(), None, "fiona", "restart")
            .unwrap_err()
            .status(),
        409
    );
    drop(held);
    std::fs::write(
        &path,
        format!(
            r#"{{"pid":{},"operation":"fake-live-holder"}}"#,
            std::process::id()
        ),
    )
    .unwrap();
    lock::LifecycleFence::acquire_endpoint(ep, None, "fiona", "restart").unwrap();
}

// A private child of the regression below; never contacts an agent or service.
#[test]
fn lifecycle_fence_child_holder() {
    let Some(root) = std::env::var_os("FL_TEST_LIFECYCLE_FENCE_ROOT").map(PathBuf::from) else {
        return;
    };
    let ep = HostEndpoint::unix(root.join("host.sock")).unwrap();
    let _held = lock::LifecycleFence::acquire_endpoint(ep, None, "fiona", "child-holder").unwrap();
    std::fs::write(root.join("ready"), "ready").unwrap();
    std::thread::sleep(Duration::from_secs(30));
}

#[test]
fn lifecycle_fence_process_death_releases_ownership() {
    let root = TempDir::new().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lifecycle::fence_tests::lifecycle_fence_child_holder",
            "--nocapture",
        ])
        .env("FL_TEST_LIFECYCLE_FENCE_ROOT", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.path().join("ready").exists() {
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("fence child did not become ready");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let refused = lock::LifecycleFence::acquire_endpoint(endpoint(&root), None, "fiona", "parent");
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(refused.unwrap_err().status(), 409);
    lock::LifecycleFence::acquire_endpoint(endpoint(&root), None, "fiona", "parent").unwrap();
}

#[cfg(unix)]
#[test]
fn lifecycle_fence_descriptor_is_not_inherited_by_children() {
    let root = TempDir::new().unwrap();
    let held =
        lock::LifecycleFence::acquire_endpoint(endpoint(&root), None, "fiona", "parent").unwrap();
    assert!(
        held.descriptors_are_cloexec(),
        "every fence FD must be close-on-exec"
    );
    let mut child = Command::new("/bin/sleep").arg("10").spawn().unwrap();
    #[cfg(target_os = "linux")]
    let inherited = std::fs::read_dir(format!("/proc/{}/fd", child.id()))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .any(|path| held.paths().contains(&path));

    drop(held);
    let acquired = lock::LifecycleFence::acquire_endpoint(endpoint(&root), None, "fiona", "parent");
    let running = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(running, "inheritance fixture must still be alive");
    #[cfg(target_os = "linux")]
    assert!(!inherited, "child inherited a lifecycle fence descriptor");

    acquired.unwrap();
}

#[test]
fn explicit_lifecycle_losers_leave_store_and_runtime_unchanged() {
    let root = TempDir::new().unwrap();
    let store = fl_core::MemStore::default();
    let runtime = FakeRuntime::default();
    let env = runtime.env(root.path());
    let request = SpinUpRequest {
        persona: "fiona".into(),
        project: "ferroloop".into(),
        role: "assistant".into(),
        vendor: "claude".into(),
        ..Default::default()
    };
    spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap();
    let target = SessionTarget::resolve(&store, "claude:fiona", None).unwrap();
    let before = list_hosted_sessions(&store).unwrap();
    let agent_before = store.agent_by_name("fiona").unwrap();
    let recovery_before = load_hosted_respawn_state(&store, "claude:fiona").unwrap();
    let calls_before = runtime.calls.borrow().clone();
    let launches_before = runtime.launches.borrow().clone();
    let held = lock::LifecycleFence::acquire(&env, "fiona", "other-process").unwrap();
    assert_eq!(
        spin_up(&store, &env, &request, "owner", Caller::Owner)
            .unwrap_err()
            .status(),
        409
    );
    assert_eq!(
        restart_hosted_session(&store, &env, "claude:fiona", "owner")
            .unwrap_err()
            .status(),
        409
    );
    assert_eq!(
        prepare_decommission(&store, &env, &target, "owner", Caller::Owner)
            .unwrap_err()
            .status(),
        409
    );
    assert_eq!(
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| panic!(
            "loser cleared a queue"
        ))
        .unwrap_err()
        .status(),
        409
    );
    assert_eq!(
        remodel_session(
            &store,
            &env,
            &target,
            &RemodelRequest {
                vendor: "claude".into(),
                ..Default::default()
            },
            "owner",
            Caller::Owner,
            |_| panic!("loser cleared a queue")
        )
        .unwrap_err()
        .status(),
        409
    );
    assert_eq!(
        *runtime.calls.borrow(),
        calls_before,
        "loser made runtime calls"
    );
    assert_eq!(
        *runtime.launches.borrow(),
        launches_before,
        "loser launched a vendor"
    );
    assert_eq!(list_hosted_sessions(&store).unwrap(), before);
    assert_eq!(store.agent_by_name("fiona").unwrap(), agent_before);
    assert_eq!(
        load_hosted_respawn_state(&store, "claude:fiona").unwrap(),
        recovery_before
    );
    drop(held);
}

#[test]
fn borrowed_lifecycle_fence_allows_only_its_persona_and_restores_pin_on_error() {
    let root = TempDir::new().unwrap();
    let runtime = FakeRuntime::default();
    let env = runtime.env(root.path());
    let result: Result<(), LifecycleError> =
        with_lifecycle_fence(&env, "fiona", "remodel", |env| {
            with_lifecycle_fence(env, "fiona", "rollback", |_| Ok(()))?;
            assert_eq!(
                with_lifecycle_fence(env, "felix", "spin-up", |_| Ok(()))
                    .unwrap_err()
                    .status(),
                409
            );
            Err(LifecycleError::Invalid("deliberate rollback error".into()))
        });
    assert_eq!(result.unwrap_err().status(), 400);
    assert!(lock::pinned_endpoint().is_none());
    with_lifecycle_fence(&env, "fiona", "restart", |_| Ok(())).unwrap();
}

#[test]
fn lifecycle_dispatch_pin_survives_endpoint_alias_retarget() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("first.sock");
    let second = root.path().join("second.sock");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    for socket in [&first, &second] {
        let endpoint = HostEndpoint::unix(socket).unwrap();
        std::fs::write(endpoint.token_path(), [0u8; 32]).unwrap();
        std::fs::set_permissions(
            endpoint.token_path(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let _a = std::os::unix::net::UnixListener::bind(&first).unwrap();
    let _b = std::os::unix::net::UnixListener::bind(&second).unwrap();
    let alias = root.path().join("alias.sock");
    std::os::unix::fs::symlink(&first, &alias).unwrap();
    let held = lock::LifecycleFence::acquire_endpoint(
        HostEndpoint::unix(&alias).unwrap(),
        None,
        "fiona",
        "prompt",
    )
    .unwrap();
    let _pin = held.pin();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&second, &alias).unwrap();
    assert_eq!(
        fl_wire::runtime_context::host_endpoint()
            .unwrap()
            .socket_path(),
        first
    );
    // Exercise a real runtime-port connection after retargeting, not just the selector.
    // Both private listeners close accepted streams, so a regressed selection cannot hang.
    use fl_core::vendor_adapter::{AdapterIdentityBinding, RuntimePort};
    for listener in [&_a, &_b] {
        listener.set_nonblocking(true).unwrap();
    }
    std::thread::scope(|scope| {
        let accept = |listener: &std::os::unix::net::UnixListener| {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        drop(stream);
                        return true;
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => return false,
                }
            }
        };
        let first_seen = scope.spawn(move || accept(&_a));
        let second_seen = scope.spawn(move || accept(&_b));
        let binding = AdapterIdentityBinding::new("fiona", 1, "Fiona.claude", None);
        assert!(
            fl_wire::host_runtime::HostRuntimePort::with_endpoint(
                HostEndpoint::unix(&alias).unwrap()
            )
            .capture_output(&binding, 32)
            .is_err()
        );
        assert!(first_seen.join().unwrap(), "pinned endpoint was not used");
        assert!(
            !second_seen.join().unwrap(),
            "retargeted endpoint received control traffic"
        );
    });
    assert!(
        fl_wire::runtime_context::tmux_command(std::path::Path::new("tmux")).is_err(),
        "unfenced tmux fallback must refuse"
    );
}

#[test]
fn lead_review_owned_http_waits_for_poll_store_then_succeeds() {
    let root = TempDir::new().unwrap();
    let state = crate::HubState::new(crate::HubServerConfig {
        store_path: root.path().join("fixture.fls"),
        repo_root: root.path().to_path_buf(),
        home_override: Some(root.path().to_path_buf()),
        owner_token: Some("fixture-owner".into()),
        ..Default::default()
    });
    let mut events = state.subscribe();
    let guard = state.open_store().unwrap();
    std::thread::scope(|scope| {
        let (started, start) = std::sync::mpsc::channel();
        let (done, result) = std::sync::mpsc::channel();
        let state = &state;
        scope.spawn(move || {
            started.send(()).unwrap();
            let request = crate::HttpRequest::new("POST", "/api/v1/sessions/spin-up")
                .with_header("authorization", "Bearer fixture-owner")
                .with_json_body(&serde_json::json!({"persona":"ada","project":"ferroloop","role":"assistant","vendor":"claude"}));
            done.send(crate::handle_http_request(state, &request)).unwrap();
        });
        start.recv().unwrap();
        assert!(
            result.recv_timeout(Duration::from_millis(100)).is_err(),
            "owned POST must wait for poll store"
        );
        drop(guard);
        let response = result.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
    });
    assert_eq!(events.try_recv().unwrap().event, "session.spun_up");
    assert_eq!(
        list_hosted_sessions(&*state.open_store().unwrap())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn lead_review_missing_endpoint_parent_has_no_effects() {
    let root = TempDir::new().unwrap();
    let missing = root.path().join("absent");
    let ep = HostEndpoint::unix(missing.join("host.sock")).unwrap();
    assert!(lock::LifecycleFence::acquire_endpoint(ep, None, "ada", "restart").is_err());
    assert!(!missing.exists());
}

#[test]
fn lead_review_group_conflict_names_only_busy_persona_and_safe_metadata() {
    let root = TempDir::new().unwrap();
    let held =
        lock::LifecycleFence::acquire(&LifecycleEnv::new(Some(root.path())), "zelda", "restart")
            .unwrap();
    std::fs::write(&held.paths()[0], r#"{"pid":42,"operation":"private/path\nsecret","started_at_ms":123,"secret":"do not disclose"}"#).unwrap();
    let error = lock::LifecycleFence::acquire_many(
        &LifecycleEnv::new(Some(root.path())),
        &["ada".into(), "zelda".into()],
        "group",
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("`zelda`"), "{message}");
    assert!(
        !message.contains("ada, ")
            && !message.contains("secret")
            && !message.contains(root.path().to_str().unwrap()),
        "{message}"
    );
}

#[test]
fn lifecycle_native_only_pin_refuses_unfenced_tmux_preflight() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let marker = root.path().join("unfenced-effect");
    let binary = root.path().join("tmux-stub");
    std::fs::write(
        &binary,
        format!("#!/bin/sh\n: > '{}'\nprintf 'unfenced'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let held =
        lock::LifecycleFence::acquire_endpoint(endpoint(&root), None, "fiona", "prompt").unwrap();
    let _pin = held.pin();
    assert!(tmux_stdout_of(&binary, &["list-panes"]).is_none());
    assert!(!marker.exists(), "unfenced tmux preflight ran");
}

#[test]
fn lead_review_unknown_restart_does_not_acquire_guessed_persona_fence() {
    let root = TempDir::new().unwrap();
    let state = crate::HubState::new(crate::HubServerConfig {
        store_path: root.path().join("fixture.fls"),
        repo_root: root.path().into(),
        home_override: Some(root.path().into()),
        owner_token: Some("fixture-owner".into()),
        ..Default::default()
    });
    drop(state.open_store().unwrap());
    let env = crate::api::lifecycle_env(&state);
    let _held = lock::LifecycleFence::acquire(&env, "ada", "other-operation").unwrap();
    let request = crate::HttpRequest::new("POST", "/api/v1/hosted-sessions/claude:ada/restart")
        .with_header("authorization", "Bearer fixture-owner");
    assert_eq!(crate::handle_http_request(&state, &request).status, 404);
    assert!(
        list_hosted_sessions(&*state.open_store().unwrap())
            .unwrap()
            .is_empty()
    );
}
