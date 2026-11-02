//! Remodel tests (Local Record 59): a move of a session to another vendor keeps the persona and
//! is all or nothing. Each test runs in its own store and home with runtime spawning off.

use super::*;
use fl_core::MemStore;
use fl_core::at::At;
use serde_json::Value;
use tempfile::TempDir;

fn env(home: &TempDir) -> LifecycleEnv<'_> {
    LifecycleEnv {
        home_override: Some(home.path()),
        spawn_runtime: false,
        runtime: &TmuxRuntime,
        key_probe: &ProvisionedKeys,
    }
}

fn spin(store: &MemStore, env: &LifecycleEnv<'_>, persona: &str, role: &str, vendor: &str) {
    spin_with(store, env, persona, role, vendor, None, None);
}

fn spin_with(
    store: &MemStore,
    env: &LifecycleEnv<'_>,
    persona: &str,
    role: &str,
    vendor: &str,
    model: Option<&str>,
    cwd: Option<&Path>,
) {
    let req = SpinUpRequest {
        persona: persona.into(),
        project: "FerroLoop".into(),
        role: role.into(),
        vendor: vendor.into(),
        model: model.map(Into::into),
        cwd: cwd.map(|c| c.display().to_string()),
        ..SpinUpRequest::default()
    };
    spin_up(store, env, &req, "owner", Caller::Owner).unwrap();
}

/// A resident with an email and an extra area of its own, as `register` leaves one.
fn register(store: &MemStore, persona: &str, role: AgentRole) {
    store
        .register_agent(RegisterAgentArgs {
            display_name: capitalize(persona),
            email: Some(format!("{persona}@example.org")),
            project: "ferroloop".into(),
            role,
            extra_areas: vec!["notes".into()],
            create_area: true,
            replace: false,
            at: stamp::now(),
            by: "owner".into(),
        })
        .unwrap();
}

fn remodel_req(vendor: &str) -> RemodelRequest {
    RemodelRequest {
        vendor: vendor.into(),
        ..RemodelRequest::default()
    }
}

fn target(store: &MemStore, id: &str) -> SessionTarget {
    SessionTarget::resolve(store, id, None).unwrap()
}

fn remodel(
    store: &MemStore,
    env: &LifecycleEnv<'_>,
    id: &str,
    req: &RemodelRequest,
    caller: Caller,
) -> Result<RemodelResponse, LifecycleError> {
    let target = SessionTarget::resolve(store, id, None)?;
    remodel_session(store, env, &target, req, "owner", caller, |_| {})
}

fn record(store: &MemStore, id: &str) -> HostedSessionRecord {
    list_hosted_sessions(store)
        .unwrap()
        .into_iter()
        .find(|r| r.session_id == id)
        .unwrap_or_else(|| panic!("no hosted record `{id}`"))
}

fn agent(store: &MemStore, persona: &str) -> AgentRecord {
    store.agent_by_name(persona).unwrap().unwrap()
}

fn handoff(store: &MemStore, persona: &str) -> Option<(HandoffEntry, Value, usize)> {
    let entry = store
        .get_state_by_key(
            &StateScope::Workspace,
            StateConcern::Handoff,
            &remodel_handoff_slug(persona),
        )
        .unwrap()?;
    let revisions = store.state_revisions(&entry.id).unwrap().len();
    let StatePayload::Handoff(h) = entry.payload else {
        panic!("not a handoff entry");
    };
    let notes = serde_json::from_str(h.notes.as_deref().unwrap()).unwrap();
    Some((h, notes, revisions))
}

fn webhook_route(home: &Path, persona: &str) -> PathBuf {
    let mut routes = RouteTable::default();
    routes.upsert(
        persona,
        VendorRoute::Webhook {
            socket_path: home.join("none.sock"),
        },
        At::from_unix_millis(1),
    );
    let path = home.join(".cache").join("agent-bus").join("routes.json");
    routes.save(Some(&path)).unwrap();
    path
}

#[test]
fn execute_moves_vendor_and_keeps_persona_project_role_email_extra_areas_and_cwd() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    let worktree = TempDir::new().unwrap();
    register(&store, "felicity", AgentRole::Assistant);
    spin_with(
        &store,
        &env,
        "felicity",
        "assistant",
        "claude",
        Some("claude-opus-5-5"),
        Some(worktree.path()),
    );
    let created_at = record(&store, "claude:felicity").created_at_ms;

    let target = target(&store, "claude:felicity");
    let mut cleared = Vec::new();
    let out = remodel_session(
        &store,
        &env,
        &target,
        &remodel_req("codex"),
        "owner",
        Caller::Owner,
        |k| cleared.push(k.to_string()),
    )
    .unwrap();
    let cwd = worktree.path().display().to_string();
    assert_eq!(out.mode, RemodelMode::Execute);
    assert_eq!(out.session_id, "codex:felicity");
    assert_eq!(out.previous_session_id, "claude:felicity");
    assert_eq!(
        (
            out.persona.as_str(),
            out.project.as_str(),
            out.role.as_str()
        ),
        ("felicity", "FerroLoop", "assistant")
    );
    assert_eq!(
        (out.from_vendor.as_str(), out.vendor.as_str()),
        ("claude", "codex")
    );
    // The old vendor's model is never carried over to the new vendor.
    assert_eq!(out.model, None);
    assert_eq!(out.cwd.as_deref(), Some(cwd.as_str()));
    assert_eq!(out.status, STATUS_RUNNING);
    assert_eq!(out.tmux_target.as_deref(), Some("Felicity:codex"));
    // Runtime calls are off: no runtime was started, so none is reported stopped.
    assert!(!out.runtime_stopped);
    assert_eq!(out.handoff.as_deref(), Some("remodel-felicity"));
    assert!(out.warnings.is_empty());
    assert!(cleared.contains(&"claude:felicity".to_string()));

    let after = agent(&store, "felicity");
    assert_eq!(after.status, AgentStatus::Active);
    assert_eq!(after.project, "ferroloop");
    assert_eq!(after.role, AgentRole::Assistant);
    assert_eq!(after.email, "felicity@example.org");
    assert_eq!(after.extra_areas, ["notes"]);

    assert_eq!(
        record(&store, "claude:felicity").status,
        STATUS_DECOMMISSIONED
    );
    let new = record(&store, "codex:felicity");
    assert_eq!(new.status, STATUS_RUNNING);
    assert_eq!(new.cwd.as_deref(), Some(cwd.as_str()));
    assert_eq!(new.model, None);
    assert_eq!(new.node_id, DEFAULT_NODE_ID);
    assert!(new.hosted);
    assert!(new.created_at_ms >= created_at);
    // A bare persona now reaches the live record.
    assert_eq!(target_of(&store, "felicity"), "codex:felicity");

    let (h, notes, revisions) = handoff(&store, "felicity").unwrap();
    assert_eq!(
        h.summary,
        "Remodel of felicity: claude -> codex (remodeled)"
    );
    // One revision before the teardown, one with the outcome.
    assert_eq!(revisions, 2);
    for (key, want) in [
        ("outcome", "remodeled"),
        ("from_session_id", "claude:felicity"),
        ("to_session_id", "codex:felicity"),
        ("from_vendor", "claude"),
        ("to_vendor", "codex"),
        ("from_model", "claude-opus-5-5"),
        ("project", "FerroLoop"),
        ("role", "assistant"),
        ("cwd", cwd.as_str()),
        ("email", "felicity@example.org"),
    ] {
        assert_eq!(notes[key], want, "{key}");
    }
    assert!(notes["model"].is_null());
    assert_eq!(notes["extra_areas"], serde_json::json!(["notes"]));
}

fn target_of(store: &MemStore, id: &str) -> String {
    target(store, id).session_id
}

#[test]
fn same_vendor_remodel_ends_running_on_the_new_model_and_cwd() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    let (old_dir, new_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    spin_with(
        &store,
        &env,
        "felicity",
        "assistant",
        "claude",
        Some("claude-opus-5-5"),
        Some(old_dir.path()),
    );
    let before = record(&store, "claude:felicity");
    let req = RemodelRequest {
        vendor: "Claude".into(),
        model: Some(" claude-sonnet-5 ".into()),
        cwd: Some(new_dir.path().display().to_string()),
        ..RemodelRequest::default()
    };
    let out = remodel(&store, &env, "felicity", &req, Caller::Owner).unwrap();
    assert_eq!(out.session_id, "claude:felicity");
    assert_eq!(out.previous_session_id, "claude:felicity");
    assert_eq!(out.model.as_deref(), Some("claude-sonnet-5"));

    let records = list_hosted_sessions(&store).unwrap();
    assert_eq!(records.len(), 1);
    let rec = &records[0];
    assert_eq!(rec.status, STATUS_RUNNING);
    assert_eq!(rec.model.as_deref(), Some("claude-sonnet-5"));
    let new_cwd = new_dir.path().display().to_string();
    assert_eq!(rec.cwd.as_deref(), Some(new_cwd.as_str()));
    assert_eq!(rec.created_at_ms, before.created_at_ms);
    assert_eq!(rec.decommission_prepared_at_ms, None);
    assert_eq!(agent(&store, "felicity").status, AgentStatus::Active);
}

#[test]
fn prepare_sends_the_handoff_prompt_and_execute_after_it_completes_the_remodel() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    spin(&store, &env, "felicity", "assistant", "claude");
    let routes = webhook_route(home.path(), "felicity");

    let prepare = RemodelRequest {
        mode: Some("Prepare".into()),
        ..remodel_req("grok")
    };
    let out = remodel(&store, &env, "claude:felicity", &prepare, Caller::Owner).unwrap();
    assert_eq!(out.mode, RemodelMode::Prepare);
    assert_eq!(out.status, STATUS_DECOMMISSIONING);
    assert_eq!(out.session_id, "claude:felicity");
    assert_eq!(out.vendor, "grok");
    // The webhook socket does not exist, so nothing took the prompt.
    assert!(!out.handshake_dispatched);
    let prompt = out.prompt.unwrap();
    assert!(prompt.contains("fl state handoff") && prompt.contains("worktrees"));
    assert_eq!(out.handoff, None);

    // Nothing was stopped, retired or recorded yet.
    assert_eq!(
        record(&store, "claude:felicity").status,
        STATUS_DECOMMISSIONING
    );
    assert_eq!(agent(&store, "felicity").status, AgentStatus::Active);
    assert!(RouteTable::load(Some(&routes)).get("felicity").is_some());
    assert!(handoff(&store, "felicity").is_none());

    // `execute` on the `decommissioning` record finishes the move.
    let out = remodel(
        &store,
        &env,
        "claude:felicity",
        &remodel_req("grok"),
        Caller::Owner,
    )
    .unwrap();
    assert_eq!(out.session_id, "grok:felicity");
    assert_eq!(out.status, STATUS_RUNNING);
    assert_eq!(
        record(&store, "claude:felicity").status,
        STATUS_DECOMMISSIONED
    );
    assert_eq!(record(&store, "grok:felicity").status, STATUS_RUNNING);
    assert!(RouteTable::load(Some(&routes)).get("felicity").is_none());
}

#[test]
fn a_peer_cannot_remodel_an_unmanaged_session() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    register(&store, "jarvis", AgentRole::LabManager);
    let before = agent(&store, "jarvis");
    for mode in ["prepare", "execute"] {
        let req = RemodelRequest {
            mode: Some(mode.into()),
            ..remodel_req("codex")
        };
        let err = remodel(&store, &env, "claude:jarvis", &req, Caller::Peer).unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{mode}: {err}");
        assert_eq!(err.status(), 403);
    }
    assert_eq!(agent(&store, "jarvis"), before);
    assert!(list_hosted_sessions(&store).unwrap().is_empty());
    assert!(handoff(&store, "jarvis").is_none());

    // A Peer may remodel a hosted session, and the Owner an unmanaged one.
    spin(&store, &env, "ada", "assistant", "claude");
    remodel(
        &store,
        &env,
        "claude:ada",
        &remodel_req("codex"),
        Caller::Peer,
    )
    .unwrap();
    let out = remodel(
        &store,
        &env,
        "claude:jarvis",
        &remodel_req("codex"),
        Caller::Owner,
    )
    .unwrap();
    assert_eq!(out.session_id, "codex:jarvis");
    assert_eq!(out.role, "lab-manager");
    // With no record to keep it from, the new session takes spin-up's default: hosted.
    assert!(out.hosted);
    let jarvis = agent(&store, "jarvis");
    assert_eq!(
        (jarvis.status, jarvis.role, jarvis.email.as_str()),
        (
            AgentStatus::Active,
            AgentRole::LabManager,
            "jarvis@example.org"
        )
    );
    assert_eq!(jarvis.extra_areas, ["notes"]);
}

#[test]
fn unknown_targets_are_404_and_bad_fields_are_400_with_nothing_changed() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    let err = remodel(&store, &env, "ghost", &remodel_req("codex"), Caller::Owner).unwrap_err();
    assert_eq!(err.status(), 404, "{err}");

    spin_with(
        &store,
        &env,
        "felicity",
        "assistant",
        "claude",
        Some("claude-opus-5-5"),
        None,
    );
    let before = list_hosted_sessions(&store).unwrap();
    let agent_before = agent(&store, "felicity");
    let bad = |f: fn(&mut RemodelRequest)| {
        let mut req = remodel_req("codex");
        f(&mut req);
        req
    };
    for (what, req) in [
        ("vendor", remodel_req("gemini")),
        ("vendor", remodel_req("")),
        ("model", bad(|r| r.model = Some("a\nb".into()))),
        ("model", bad(|r| r.model = Some("m".repeat(129)))),
        ("node_id", bad(|r| r.node_id = Some("bad node".into()))),
        (
            "cwd",
            bad(|r| r.cwd = Some("/no/such/dir/for/remodel".into())),
        ),
        ("mode", bad(|r| r.mode = Some("teardown".into()))),
        ("mode", bad(|r| r.mode = Some(String::new()))),
    ] {
        let err = remodel(&store, &env, "claude:felicity", &req, Caller::Owner).unwrap_err();
        assert!(matches!(err, LifecycleError::Invalid(_)), "{what}: {err}");
        assert!(err.to_string().starts_with(what), "{what}: {err}");
    }
    assert_eq!(list_hosted_sessions(&store).unwrap(), before);
    assert_eq!(agent(&store, "felicity"), agent_before);
    assert!(handoff(&store, "felicity").is_none());
}

#[test]
fn unknown_request_fields_are_refused() {
    let err = serde_json::from_value::<RemodelRequest>(serde_json::json!({
        "vendor": "codex", "persona": "someone-else",
    }))
    .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
}

/// A spin-up step that counts its calls and fails for the vendors in `failing`.
fn failing_spin<'a>(
    failing: &'a [&'a str],
    calls: &'a std::cell::RefCell<Vec<String>>,
) -> impl FnMut(
    &MemStore,
    &LifecycleEnv<'_>,
    &SpinUpRequest,
    &str,
    Caller,
) -> Result<SpinUpResponse, LifecycleError>
+ 'a {
    move |store, env, req, by, caller| {
        calls.borrow_mut().push(req.vendor.clone());
        if failing.contains(&req.vendor.as_str()) {
            return Err(LifecycleError::Store(format!(
                "{} binary is missing",
                req.vendor
            )));
        }
        spin_up(store, env, req, by, caller)
    }
}

fn remodel_failing(
    store: &MemStore,
    env: &LifecycleEnv<'_>,
    id: &str,
    req: &RemodelRequest,
    failing: &[&str],
) -> (Result<RemodelResponse, LifecycleError>, Vec<String>) {
    remodel_failing_as(store, env, id, req, failing, Caller::Owner)
}

fn remodel_failing_as(
    store: &MemStore,
    env: &LifecycleEnv<'_>,
    id: &str,
    req: &RemodelRequest,
    failing: &[&str],
    caller: Caller,
) -> (Result<RemodelResponse, LifecycleError>, Vec<String>) {
    let calls = std::cell::RefCell::new(Vec::new());
    let target = target(store, id);
    let out = {
        let mut step = failing_spin(failing, &calls);
        remodel_with(store, env, &target, req, "owner", caller, |_| {}, &mut step)
    };
    (out, calls.into_inner())
}

#[test]
fn a_failed_pre_check_leaves_the_old_session_untouched() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    let worktree = TempDir::new().unwrap();
    spin_with(
        &store,
        &env,
        "ferris",
        "lead",
        "claude",
        None,
        Some(worktree.path()),
    );
    let routes = webhook_route(home.path(), "ferris");

    // The worktree went away: the new runtime would start somewhere else.
    drop(worktree);
    let (err, calls) = remodel_failing(&store, &env, "claude:ferris", &remodel_req("codex"), &[]);
    let err = err.unwrap_err();
    assert!(
        matches!(err, LifecycleError::Invalid(ref m) if m.starts_with("cwd")),
        "{err}"
    );
    assert!(calls.is_empty());
    // A runtime that is not hosted never uses the directory.
    let unhosted = RemodelRequest {
        hosted: Some(false),
        mode: Some("prepare".into()),
        ..remodel_req("codex")
    };
    remodel(&store, &env, "claude:ferris", &unhosted, Caller::Owner).unwrap();
    // Undo the prepare: a restart marks the record running again.
    restart_hosted_session(&store, &env, "claude:ferris", "owner").unwrap();

    // Another agent took the Lead seat outside the lifecycle (registering as Lead with
    // `replace` retires the sitting Lead).
    store
        .register_agent(RegisterAgentArgs {
            display_name: "Felix".into(),
            email: None,
            project: "ferroloop".into(),
            role: AgentRole::Lead,
            extra_areas: Vec::new(),
            create_area: false,
            replace: true,
            at: stamp::now(),
            by: "owner".into(),
        })
        .unwrap();
    let felix = agent(&store, "felix");
    let mut req = remodel_req("codex");
    req.hosted = Some(false);
    let (err, calls) = remodel_failing(&store, &env, "claude:ferris", &req, &[]);
    let err = err.unwrap_err();
    assert!(
        matches!(err, LifecycleError::Conflict(ref m) if m.contains("active Lead")),
        "{err}"
    );
    assert!(calls.is_empty());

    // No teardown happened: the record, the routes and the registry are as they were.
    let rec = record(&store, "claude:ferris");
    assert_eq!(rec.status, STATUS_RUNNING);
    assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
    assert!(RouteTable::load(Some(&routes)).get("ferris").is_some());
    assert_eq!(agent(&store, "felix"), felix);
    assert!(handoff(&store, "ferris").is_none());
}

#[test]
fn a_failed_spin_up_after_teardown_restores_the_old_vendor() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    let worktree = TempDir::new().unwrap();
    register(&store, "felicity", AgentRole::Assistant);
    spin_with(
        &store,
        &env,
        "felicity",
        "assistant",
        "claude",
        Some("claude-opus-5-5"),
        Some(worktree.path()),
    );
    let req = RemodelRequest {
        model: Some("gpt-6".into()),
        ..remodel_req("codex")
    };
    let (out, calls) = remodel_failing(&store, &env, "claude:felicity", &req, &["codex"]);
    let err = out.unwrap_err();
    assert_eq!(calls, ["codex", "claude"]);
    assert!(matches!(err, LifecycleError::Partial { .. }), "{err}");
    assert_eq!(err.status(), 502);
    let msg = err.to_string();
    assert!(
        msg.starts_with(
            "remodel of `felicity` failed: spin-up on codex failed: codex binary is missing"
        ),
        "{msg}"
    );
    assert!(
        msg.ends_with("the persona was restored on claude as `claude:felicity`"),
        "{msg}"
    );

    // The old session runs again on its old model and directory, with its agent intact.
    let rec = record(&store, "claude:felicity");
    assert_eq!(rec.status, STATUS_RUNNING);
    assert_eq!(rec.model.as_deref(), Some("claude-opus-5-5"));
    let cwd = worktree.path().display().to_string();
    assert_eq!(rec.cwd.as_deref(), Some(cwd.as_str()));
    assert!(
        list_hosted_sessions(&store)
            .unwrap()
            .iter()
            .all(|r| r.session_id != "codex:felicity")
    );
    let after = agent(&store, "felicity");
    assert_eq!(after.status, AgentStatus::Active);
    assert_eq!(after.email, "felicity@example.org");
    assert_eq!(after.extra_areas, ["notes"]);

    let (h, notes, revisions) = handoff(&store, "felicity").unwrap();
    assert_eq!(revisions, 2);
    assert_eq!(notes["outcome"], "restored");
    assert_eq!(notes["restored_session_id"], "claude:felicity");
    assert!(notes["to_session_id"].is_null());
    assert_eq!(notes["model"], "gpt-6");
    assert!(
        notes["error"]
            .as_str()
            .unwrap()
            .contains("codex binary is missing")
    );
    assert!(notes["restore_error"].is_null());
    assert_eq!(h.blockers.len(), 1);
}

#[test]
fn a_failed_restore_is_reported_and_recorded_as_down() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    register(&store, "felicity", AgentRole::Assistant);
    spin(&store, &env, "felicity", "assistant", "claude");
    let (out, calls) = remodel_failing(
        &store,
        &env,
        "claude:felicity",
        &remodel_req("codex"),
        &["codex", "claude"],
    );
    let msg = out.unwrap_err().to_string();
    assert_eq!(calls, ["codex", "claude"]);
    assert!(
        msg.contains("spin-up on codex failed: codex binary is missing"),
        "{msg}"
    );
    assert!(
        msg.contains("restore on claude ALSO failed: claude binary is missing"),
        "{msg}"
    );
    assert!(msg.ends_with("the persona is DOWN"), "{msg}");

    assert_eq!(
        record(&store, "claude:felicity").status,
        STATUS_DECOMMISSIONED
    );
    assert_eq!(agent(&store, "felicity").status, AgentStatus::Retired);
    let (h, notes, _) = handoff(&store, "felicity").unwrap();
    assert_eq!(notes["outcome"], "down");
    assert!(
        notes["restore_error"]
            .as_str()
            .unwrap()
            .contains("claude binary")
    );
    assert_eq!(h.blockers.len(), 2);
    assert!(h.next_steps[0].contains("spin `felicity` up again"));
    // It can be spun up again by hand, keeping its email and extra areas.
    spin(&store, &env, "felicity", "assistant", "claude");
    let back = agent(&store, "felicity");
    assert_eq!(
        (back.status, back.email.as_str()),
        (AgentStatus::Active, "felicity@example.org")
    );
    assert_eq!(back.extra_areas, ["notes"]);
}

#[test]
fn a_lead_remodel_keeps_the_seat_and_never_conflicts_with_itself() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    spin(&store, &env, "ferris", "lead", "claude");
    spin(&store, &env, "ada", "assistant", "claude");
    for vendor in ["codex", "codex", "muse"] {
        let out = remodel(&store, &env, "ferris", &remodel_req(vendor), Caller::Owner).unwrap();
        assert_eq!(out.session_id, format!("{vendor}:ferris"));
        assert_eq!(out.role, "lead");
        let leads: Vec<String> = store
            .list_agents(Some("ferroloop"), false)
            .unwrap()
            .into_iter()
            .filter(|a| a.role == AgentRole::Lead)
            .map(|a| a.name_key)
            .collect();
        assert_eq!(leads, ["ferris"]);
    }
    // The project's other agent is untouched, and a second Lead is still refused.
    assert_eq!(agent(&store, "ada").status, AgentStatus::Active);
    let mut felix = SpinUpRequest {
        persona: "felix".into(),
        project: "FerroLoop".into(),
        role: "lead".into(),
        vendor: "claude".into(),
        ..SpinUpRequest::default()
    };
    assert_eq!(
        spin_up(&store, &env, &felix, "owner", Caller::Owner)
            .unwrap_err()
            .status(),
        409
    );
    felix.role = "assistant".into();
    spin_up(&store, &env, &felix, "owner", Caller::Owner).unwrap();
}

#[test]
fn remodel_of_a_decommissioned_session_conflicts() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    spin(&store, &env, "felicity", "assistant", "claude");
    let t = target(&store, "claude:felicity");
    teardown_session(&store, &env, &t, "owner", Caller::Owner, |_| {}).unwrap();
    let err = remodel(
        &store,
        &env,
        "claude:felicity",
        &remodel_req("codex"),
        Caller::Owner,
    )
    .unwrap_err();
    assert_eq!(err.status(), 409, "{err}");
    assert!(handoff(&store, "felicity").is_none());
}

/// The restore message's session, `None` when the persona is down.
fn restored_of(err: &LifecycleError) -> Option<&str> {
    match err {
        LifecycleError::Partial {
            restored_session_id,
            ..
        } => restored_session_id.as_deref(),
        other => panic!("not a partial failure: {other}"),
    }
}

#[test]
fn a_new_runtime_that_does_not_start_is_a_failed_spin_up_and_restores_the_old_vendor() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    register(&store, "felicity", AgentRole::Assistant);
    spin(&store, &env, "felicity", "assistant", "claude");
    assert!(fake.is_live("Felicity"));
    assert!(record(&store, "claude:felicity").runtime_spawned);

    // tmux cannot start the new vendor's runtime: spin-up registers and records it, and starts
    // nothing.
    fake.fail_windows.borrow_mut().push("codex".into());
    let err = remodel(
        &store,
        &env,
        "claude:felicity",
        &remodel_req("codex"),
        Caller::Owner,
    )
    .unwrap_err();
    assert_eq!(err.status(), 502);
    assert_eq!(restored_of(&err), Some("claude:felicity"));
    let msg = err.to_string();
    assert!(
        msg.contains("spin-up on codex failed: its tmux runtime did not start"),
        "{msg}"
    );
    assert_eq!(
        *fake.calls.borrow(),
        [
            "spawn Felicity:claude",
            "kill Felicity",
            "spawn Felicity:codex",
            "spawn Felicity:claude",
        ]
    );
    // One runtime runs, the restored one, and only its record claims the persona.
    assert!(fake.is_live("Felicity"));
    let old = record(&store, "claude:felicity");
    assert_eq!(old.status, STATUS_RUNNING);
    assert!(old.runtime_spawned);
    assert_eq!(
        record(&store, "codex:felicity").status,
        STATUS_DECOMMISSIONED
    );
    assert_eq!(target_of(&store, "felicity"), "claude:felicity");
    assert_eq!(agent(&store, "felicity").status, AgentStatus::Active);
    assert_eq!(
        handoff(&store, "felicity").unwrap().1["outcome"],
        "restored"
    );
}

#[test]
fn a_restore_that_starts_no_runtime_reports_the_persona_down() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "felicity", "assistant", "claude");
    fake.fail_windows
        .borrow_mut()
        .extend(["codex".to_string(), "claude".to_string()]);
    let err = remodel(
        &store,
        &env,
        "claude:felicity",
        &remodel_req("codex"),
        Caller::Owner,
    )
    .unwrap_err();
    assert_eq!(restored_of(&err), None);
    let msg = err.to_string();
    assert!(msg.contains("restore on claude ALSO failed"), "{msg}");
    assert!(!fake.is_live("Felicity"));
    assert_eq!(handoff(&store, "felicity").unwrap().1["outcome"], "down");
}

#[test]
fn an_old_runtime_the_teardown_cannot_stop_is_refused_before_anything_changes() {
    let home = TempDir::new().unwrap();

    // Hosted, but its runtime was not started by this lifecycle (the name was taken).
    let store = MemStore::default();
    spin(&store, &env(&home), "felicity", "assistant", "claude");
    let fake = FakeRuntime::running(&["Felicity"]);
    let env = fake.env(home.path());
    let before = list_hosted_sessions(&store).unwrap();
    let agent_before = agent(&store, "felicity");
    let err = remodel(
        &store,
        &env,
        "claude:felicity",
        &remodel_req("codex"),
        Caller::Owner,
    )
    .unwrap_err();
    assert_eq!(err.status(), 409, "{err}");
    assert!(err.to_string().contains("did not start it"), "{err}");
    assert_eq!(list_hosted_sessions(&store).unwrap(), before);
    assert_eq!(agent(&store, "felicity"), agent_before);
    assert!(handoff(&store, "felicity").is_none());
    assert!(fake.calls.borrow().is_empty() && fake.is_live("Felicity"));
    // A prepare stops nothing, so it is not refused.
    let prepare = RemodelRequest {
        mode: Some("prepare".into()),
        ..remodel_req("codex")
    };
    remodel(&store, &env, "claude:felicity", &prepare, Caller::Owner).unwrap();

    // No hosted record at all: the session runs outside the lifecycle.
    let store = MemStore::default();
    register(&store, "jarvis", AgentRole::LabManager);
    let fake = FakeRuntime::running(&["Jarvis"]);
    let err = remodel(
        &store,
        &fake.env(home.path()),
        "claude:jarvis",
        &remodel_req("codex"),
        Caller::Owner,
    )
    .unwrap_err();
    assert_eq!(err.status(), 409, "{err}");
    assert!(err.to_string().contains("no hosted record"), "{err}");
    assert!(list_hosted_sessions(&store).unwrap().is_empty());
    assert_eq!(agent(&store, "jarvis").status, AgentStatus::Active);
    assert!(fake.calls.borrow().is_empty() && fake.is_live("Jarvis"));

    // Started by this lifecycle, but another live session shares it.
    let store = MemStore::default();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store, &env, "ada", "assistant", "claude");
    spin(&store, &env, "ada", "assistant", "codex");
    fake.calls.borrow_mut().clear();
    let err = remodel(
        &store,
        &env,
        "claude:ada",
        &remodel_req("grok"),
        Caller::Owner,
    )
    .unwrap_err();
    assert_eq!(err.status(), 409, "{err}");
    assert!(err.to_string().contains("it also runs codex:ada"), "{err}");
    assert!(fake.calls.borrow().is_empty() && fake.is_live("Ada"));
    assert_eq!(record(&store, "claude:ada").status, STATUS_RUNNING);
}

#[test]
fn the_restore_of_a_session_with_no_record_keeps_it_unhosted() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    register(&store, "jarvis", AgentRole::LabManager);
    let (out, calls) = remodel_failing(
        &store,
        &env,
        "claude:jarvis",
        &remodel_req("codex"),
        &["codex"],
    );
    assert_eq!(restored_of(&out.unwrap_err()), Some("claude:jarvis"));
    assert_eq!(calls, ["codex", "claude"]);
    // It was not hosted before the remodel, so the restore does not make it managed.
    let rec = record(&store, "claude:jarvis");
    assert_eq!(rec.status, STATUS_RUNNING);
    assert!(!rec.hosted);
}

#[test]
fn a_remodel_that_could_not_be_restored_needs_the_owner_and_warns() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    register(&store, "jarvis", AgentRole::LabManager);
    // The Owner's prepare adopts the session under an id that names no vendor.
    let prepare = RemodelRequest {
        mode: Some("prepare".into()),
        ..remodel_req("codex")
    };
    remodel(&store, &env, "jarvis", &prepare, Caller::Owner).unwrap();
    let adopted = record(&store, "jarvis");
    assert_eq!(adopted.vendor, "unknown");

    for req in [prepare.clone(), remodel_req("codex")] {
        let err = remodel(&store, &env, "jarvis", &req, Caller::Peer).unwrap_err();
        assert_eq!(err.status(), 409, "{err}");
        assert!(err.to_string().contains("could not be restored"), "{err}");
    }
    assert_eq!(record(&store, "jarvis"), adopted);
    assert_eq!(agent(&store, "jarvis").status, AgentStatus::Active);
    assert!(handoff(&store, "jarvis").is_none());

    let out = remodel(&store, &env, "jarvis", &remodel_req("codex"), Caller::Owner).unwrap();
    assert_eq!(out.session_id, "codex:jarvis");
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(out.warnings[0].starts_with("no restore was possible"));
}

/// A store whose writes can be made to fail: the `nth` write (from 1) of one state slug, and
/// every `retire_agent`.
#[derive(Default)]
struct FaultyStore {
    inner: MemStore,
    fail_state_write: std::cell::RefCell<Option<(String, usize)>>,
    fail_retire: std::cell::Cell<bool>,
}

fn injected() -> StoreError {
    StoreError::Backend("injected store failure".into())
}

impl AgentRegistry for FaultyStore {
    fn upsert_area(
        &self,
        area: fl_core::agent::AreaRecord,
    ) -> Result<fl_core::agent::AreaRecord, StoreError> {
        self.inner.upsert_area(area)
    }
    fn get_area(&self, key: &str) -> Result<Option<fl_core::agent::AreaRecord>, StoreError> {
        self.inner.get_area(key)
    }
    fn list_areas(&self) -> Result<Vec<fl_core::agent::AreaRecord>, StoreError> {
        self.inner.list_areas()
    }
    fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError> {
        self.inner.register_agent(args)
    }
    fn retire_agent(
        &self,
        id: &fl_core::ids::AgentId,
        at: At,
        by: &str,
    ) -> Result<AgentRecord, StoreError> {
        if self.fail_retire.get() {
            return Err(injected());
        }
        self.inner.retire_agent(id, at, by)
    }
    fn get_agent(&self, id: &fl_core::ids::AgentId) -> Result<Option<AgentRecord>, StoreError> {
        self.inner.get_agent(id)
    }
    fn agent_by_name(&self, name_key: &str) -> Result<Option<AgentRecord>, StoreError> {
        self.inner.agent_by_name(name_key)
    }
    fn list_agents(
        &self,
        project: Option<&str>,
        include_retired: bool,
    ) -> Result<Vec<AgentRecord>, StoreError> {
        self.inner.list_agents(project, include_retired)
    }
    fn registry_events_after(
        &self,
        after_seq: u64,
    ) -> Result<Vec<(u64, fl_core::agent::RegistryEvent)>, StoreError> {
        self.inner.registry_events_after(after_seq)
    }
    fn import_registry_events(
        &self,
        events: &[fl_core::agent::RegistryEvent],
    ) -> Result<(), StoreError> {
        self.inner.import_registry_events(events)
    }
}

impl StateStore for FaultyStore {
    fn upsert_state(
        &self,
        args: UpsertStateArgs,
    ) -> Result<fl_core::state::StateEntry, StoreError> {
        let mut fault = self.fail_state_write.borrow_mut();
        if let Some((slug, nth)) = fault.as_mut()
            && *slug == args.slug
        {
            if *nth == 1 {
                // Only this one write fails; later writes of the slug land.
                *fault = None;
                return Err(injected());
            }
            *nth -= 1;
        }
        drop(fault);
        self.inner.upsert_state(args)
    }
    fn retire_memory(
        &self,
        scope: &StateScope,
        slug: &str,
        lifecycle: fl_core::state::MemoryLifecycle,
        superseded_by: Option<fl_core::ids::StateId>,
        at: At,
        by: &str,
    ) -> Result<fl_core::state::StateEntry, StoreError> {
        self.inner
            .retire_memory(scope, slug, lifecycle, superseded_by, at, by)
    }
    fn get_state(
        &self,
        id: &fl_core::ids::StateId,
    ) -> Result<Option<fl_core::state::StateEntry>, StoreError> {
        self.inner.get_state(id)
    }
    fn get_state_by_key(
        &self,
        scope: &StateScope,
        concern: StateConcern,
        slug: &str,
    ) -> Result<Option<fl_core::state::StateEntry>, StoreError> {
        self.inner.get_state_by_key(scope, concern, slug)
    }
    fn list_state(
        &self,
        scope: Option<&StateScope>,
        concern: Option<StateConcern>,
    ) -> Result<Vec<fl_core::state::StateEntry>, StoreError> {
        self.inner.list_state(scope, concern)
    }
    fn state_revisions(
        &self,
        id: &fl_core::ids::StateId,
    ) -> Result<Vec<fl_core::state::StateRevision>, StoreError> {
        self.inner.state_revisions(id)
    }
    fn list_conflicts(&self) -> Result<Vec<fl_core::state::StateConflict>, StoreError> {
        self.inner.list_conflicts()
    }
    fn resolve_conflict(
        &self,
        id: &fl_core::ids::StateId,
        keep: &fl_core::ids::RevisionId,
        at: At,
        by: &str,
    ) -> Result<fl_core::state::StateEntry, StoreError> {
        self.inner.resolve_conflict(id, keep, at, by)
    }
    fn state_revisions_after(
        &self,
        after_seq: u64,
    ) -> Result<Vec<(u64, fl_core::state::StateRevision)>, StoreError> {
        self.inner.state_revisions_after(after_seq)
    }
    fn import_state_revisions(
        &self,
        revisions: &[fl_core::state::StateRevision],
        merge_at: At,
    ) -> Result<(), StoreError> {
        self.inner.import_state_revisions(revisions, merge_at)
    }
    fn sync_cursor(
        &self,
        remote: &str,
    ) -> Result<Option<fl_core::state::StateSyncCursor>, StoreError> {
        self.inner.sync_cursor(remote)
    }
    fn set_sync_cursor(&self, cursor: fl_core::state::StateSyncCursor) -> Result<(), StoreError> {
        self.inner.set_sync_cursor(cursor)
    }
}

fn faulty_remodel(
    store: &FaultyStore,
    env: &LifecycleEnv<'_>,
    id: &str,
    vendor: &str,
) -> Result<RemodelResponse, LifecycleError> {
    let target = SessionTarget::resolve(store, id, None).unwrap();
    remodel_session(
        store,
        env,
        &target,
        &remodel_req(vendor),
        "owner",
        Caller::Owner,
        |_| {},
    )
}

#[test]
fn a_spin_up_that_fails_after_registering_restores_and_decommissions_its_record() {
    let store = FaultyStore::default();
    let home = TempDir::new().unwrap();
    let env = env(&home);
    register(&store.inner, "felicity", AgentRole::Assistant);
    spin(&store.inner, &env, "felicity", "assistant", "claude");
    // The new session's first record write lands (after its agent registered); the second, made
    // once its runtime and reforge prompt ran, fails inside the real spin-up.
    *store.fail_state_write.borrow_mut() = Some((hosted_session_slug("codex:felicity"), 2));

    let err = faulty_remodel(&store, &env, "claude:felicity", "codex").unwrap_err();
    assert_eq!(restored_of(&err), Some("claude:felicity"), "{err}");
    let msg = err.to_string();
    assert!(
        msg.contains("spin-up on codex failed") && msg.contains("injected store failure"),
        "{msg}"
    );
    // The half-made record is no longer `running`: only the restored session claims the persona.
    assert_eq!(
        record(&store.inner, "codex:felicity").status,
        STATUS_DECOMMISSIONED
    );
    assert_eq!(
        record(&store.inner, "claude:felicity").status,
        STATUS_RUNNING
    );
    assert_eq!(target_of(&store.inner, "felicity"), "claude:felicity");
    let after = agent(&store.inner, "felicity");
    assert_eq!(
        (after.status, after.email.as_str()),
        (AgentStatus::Active, "felicity@example.org")
    );
    assert_eq!(after.extra_areas, ["notes"]);
}

#[test]
fn a_teardown_that_fails_after_stopping_the_runtime_restores_it() {
    let store = FaultyStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    spin(&store.inner, &env, "felicity", "assistant", "claude");
    store.fail_retire.set(true);

    let err = faulty_remodel(&store, &env, "claude:felicity", "codex").unwrap_err();
    assert_eq!(restored_of(&err), Some("claude:felicity"), "{err}");
    let msg = err.to_string();
    assert!(
        msg.contains("teardown of `claude:felicity` failed") && msg.contains("injected"),
        "{msg}"
    );
    // The runtime was stopped before the retire failed; the restore started it again and the
    // new vendor was never tried.
    assert_eq!(
        *fake.calls.borrow(),
        [
            "spawn Felicity:claude",
            "kill Felicity",
            "spawn Felicity:claude"
        ]
    );
    assert!(fake.is_live("Felicity"));
    let rec = record(&store.inner, "claude:felicity");
    assert_eq!(rec.status, STATUS_RUNNING);
    assert!(rec.runtime_spawned);
    assert_eq!(list_hosted_sessions(&store.inner).unwrap().len(), 1);
    assert_eq!(agent(&store.inner, "felicity").status, AgentStatus::Active);
}

#[test]
fn a_peers_failed_remodel_restores_exactly_the_session_it_tore_down() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = fake.env(home.path());
    let worktree = TempDir::new().unwrap();
    register(&store, "felicity", AgentRole::Assistant);
    spin_with(
        &store,
        &env,
        "felicity",
        "assistant",
        "claude",
        Some("claude-opus-5-5"),
        Some(worktree.path()),
    );
    let before = record(&store, "claude:felicity");
    let agent_before = agent(&store, "felicity");

    let req = RemodelRequest {
        model: Some("gpt-6".into()),
        node_id: Some("ai-lab-1".into()),
        ..remodel_req("codex")
    };
    let (out, calls) = remodel_failing_as(
        &store,
        &env,
        "claude:felicity",
        &req,
        &["codex"],
        Caller::Peer,
    );
    assert_eq!(restored_of(&out.unwrap_err()), Some("claude:felicity"));
    assert_eq!(calls, ["codex", "claude"]);

    // Everything but the write time is as it was before the teardown.
    let after = record(&store, "claude:felicity");
    assert_eq!(
        HostedSessionRecord {
            updated_at_ms: before.updated_at_ms,
            ..after
        },
        before
    );
    let agent_after = agent(&store, "felicity");
    assert_eq!(
        AgentRecord {
            updated_at: agent_before.updated_at.clone(),
            by: agent_before.by.clone(),
            ..agent_after
        },
        agent_before
    );
    assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
    assert!(fake.is_live("Felicity"));
}
