//! The migration verifier and the verified cutover (cutover spec §3.4).

use super::verify::{self, ParityReport};
use super::{FerroStore, migrate_redb_to_ferrostore};
use crate::RedbStore;
use crate::migrate::{migrate_verified, verify_redb_vs_ferrostore};
use fl_core::agent::{AgentRole, AreaRecord, RegisterAgentArgs};
use fl_core::at::At;
use fl_core::conformance::{entry_iri, sample_attempt, sample_record_run};
use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
use fl_core::finding::Finding;
use fl_core::ids::{GateId, ProjectId, RecordId, WorkflowId, WorkflowRunId, seq_iri};
use fl_core::model::{
    CommandSpec, GateKind, PopulationDelivery, Regret, Selector, State, Transition,
};
use fl_core::routing::RoutingMap;
use fl_core::split::{CachedSegment, LedgerCache, Outbox};

use fl_core::state::{HandoffEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs};
use fl_core::store::{
    AgentRegistry, Bindings, Catalog, Ledger, MessageStore, StateStore, Tracker, WorkflowStore,
};
use fl_core::workflow::{
    AssigneeRule, DEFAULT_MAX_TOTAL_STEPS, DEFAULT_STEP_MAX_ATTEMPTS, DEFAULT_STEP_MAX_VISITS,
    END_STEP, EdgeCondition, EdgeDef, JoinPolicy, StepCompletion, StepDef, WorkflowDef,
    WorkflowRun,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

struct Populated {
    project: ProjectId,
    gate: GateId,
    record: RecordId,
}

/// A redb store holding a row in nearly every table: catalog, tracker, ledger
/// with ids, cut-over and published marks, ledger cache, routing, bindings,
/// agents, state, FerroWire and workflows.
fn populate(redb: &RedbStore) -> Populated {
    let project = redb.add_project("/work/app").unwrap();
    let kind = GateKind::Command(CommandSpec {
        program: "true".into(),
        args: vec![],
        delivery: PopulationDelivery::Args,
        timeout_secs: 5,
        pass_codes: vec![0],
    });
    let selector = Selector::Glob {
        pattern: "**/*.rs".into(),
    };
    let gate = redb
        .add_gate(
            &project,
            "fmt",
            kind.clone(),
            selector.clone(),
            1,
            "c1",
            "o",
        )
        .unwrap();
    let lint = redb
        .add_gate(&project, "lint", kind, selector, 1, "c1", "o")
        .unwrap();
    redb.add_transition(Transition {
        project: project.clone(),
        name: "ship".into(),
        from: State::Review,
        to: State::Done,
        regret: Regret::High,
        gates: vec![gate.clone(), lint],
    })
    .unwrap();
    let record = redb.add_record(&project, "keep this").unwrap();
    redb.set_record_state(&record, State::Doing).unwrap();
    redb.add_finding(Finding::raise(
        project.clone(),
        record.clone(),
        "reviewer",
        "wrong on empty",
    ))
    .unwrap();
    redb.add_alias(record.iri(), entry_iri(90)).unwrap();
    redb.set_cutover("R_1", &entry_iri(0)).unwrap();
    for n in 1..=3 {
        redb.append_gate_run(sample_record_run(n, &gate, Some(&record)))
            .unwrap();
    }
    redb.append_attempt(sample_attempt(4, &project, &record))
        .unwrap();
    redb.append_attempt(sample_attempt(5, &project, &record))
        .unwrap();
    redb.mark_published("R_1", &[entry_iri(1)]).unwrap();
    redb.bind_node_id("Acme/Widgets", "R_1").unwrap();
    redb.set_ledger_root("R_1", "0123456789abcdef0123456789abcdef01234567")
        .unwrap();
    redb.remember(
        "R_1",
        "c1",
        &[(
            "runs/aa/1.jsonl".to_string(),
            CachedSegment {
                oid: "o1".into(),
                bytes: vec![0xff, 0xfe, 0xfd],
                closed: false,
            },
        )],
    )
    .unwrap();
    redb.set_routes(&project, &RoutingMap::starting()).unwrap();

    redb.upsert_area(
        AreaRecord::new(
            "ferroloop",
            Some("FerroLoop/".into()),
            "Agent orchestration",
            At::from_unix_millis(1),
            "owner",
        )
        .unwrap(),
    )
    .unwrap();
    redb.register_agent(RegisterAgentArgs {
        display_name: "ferris".into(),
        email: None,
        project: "ferroloop".into(),
        role: AgentRole::Lead,
        extra_areas: vec![],
        create_area: false,
        replace: false,
        at: At::from_unix_millis(2),
        by: "owner".into(),
    })
    .unwrap();
    for slug in ["rule", "other"] {
        redb.upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: slug.into(),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Handoff(HandoffEntry {
                summary: format!("{slug} v6"),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: None,
            }),
            at: At::from_unix_millis(10),
            by: "jarvis".into(),
        })
        .unwrap();
    }

    let workflow = redb
        .upsert_workflow_def(WorkflowDef {
            id: WorkflowId(seq_iri(801)),
            project: project.clone(),
            slug: "ci-flow".into(),
            title: "CI Flow".into(),
            start_step: "build".into(),
            steps: vec![StepDef {
                step_id: "build".into(),
                title: "Build".into(),
                prompt: "Run build.".into(),
                assignee: AssigneeRule::Agent {
                    name: "felicia".into(),
                },
                reviews_step: None,
                require_different_vendor: false,
                completion: StepCompletion::Ungated,
                join_policy: JoinPolicy::All,
                max_attempts: DEFAULT_STEP_MAX_ATTEMPTS,
                max_visits: DEFAULT_STEP_MAX_VISITS,
                budget_usd_micros: None,
                max_tokens: None,
                isolate_worktree: false,
            }],
            edges: vec![EdgeDef {
                from_step: "build".into(),
                to_step: END_STEP.into(),
                on: EdgeCondition::Pass,
            }],
            max_total_steps: DEFAULT_MAX_TOTAL_STEPS,
            budget_usd_micros: None,
            max_tokens: None,
            declared_keys: BTreeMap::new(),
            authored_at_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            authored_by: "ferris".into(),
        })
        .unwrap();
    redb.create_workflow_run(
        WorkflowRun::new(
            WorkflowRunId(seq_iri(802)),
            &workflow,
            "CI Run 1",
            At::from_unix_millis(100),
            "ferris",
        )
        .unwrap(),
    )
    .unwrap();
    let (first, _) = redb
        .send_wire(SendWireArgs {
            id: None,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: Some("ai-lab-0".into()),
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "first".into(),
            body: "hello".into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        })
        .unwrap();
    redb.send_wire(SendWireArgs {
        id: None,
        sender_id: None,
        sender_name: "ferris".into(),
        sender_host: None,
        to: WireAddress::AgentName("ferris".into()),
        kind: MessageKind::Reply,
        urgency: MessageUrgency::Normal,
        durable: true,
        subject: "re: first".into(),
        body: "reply".into(),
        reply_to: Some(first.id.clone()),
        thread_id: Some(first.thread_id.clone()),
        deadline_at: None,
        at: At::from_unix_millis(11),
    })
    .unwrap();
    Populated {
        project,
        gate,
        record,
    }
}

fn view<'a>(report: &'a ParityReport, prefix: &str) -> &'a verify::TableParity {
    report
        .trait_views
        .iter()
        .find(|v| v.table.starts_with(prefix))
        .unwrap_or_else(|| panic!("no view {prefix}"))
}

fn table<'a>(report: &'a ParityReport, name: &str) -> &'a verify::TableParity {
    report
        .tables
        .iter()
        .find(|t| t.table == name)
        .unwrap_or_else(|| panic!("no table {name}"))
}

fn mismatch_names(report: &ParityReport) -> Vec<String> {
    report
        .mismatches()
        .into_iter()
        .map(|m| m.table.clone())
        .collect()
}

/// A populated redb, migrated into a new on-disk FerroStore.
fn migrated() -> (tempfile::TempDir, RedbStore, FerroStore, Populated) {
    let dir = tempfile::tempdir().unwrap();
    let redb = RedbStore::open(&dir.path().join("src.redb")).unwrap();
    let populated = populate(&redb);
    let ferro = FerroStore::open(dir.path().join("dst.fls")).unwrap();
    migrate_redb_to_ferrostore(&redb, &ferro).unwrap();
    (dir, redb, ferro, populated)
}

#[test]
fn parity_is_clean_on_a_migrated_populated_store() {
    let (_dir, redb, ferro, populated) = migrated();
    let report = verify_redb_vs_ferrostore(&redb, &ferro).unwrap();
    assert!(report.is_clean(), "{:#?}", report.mismatches());

    // Not a vacuous pass: the entries that matter hold rows on both sides.
    for name in [
        "meta",
        "ids",
        "aliases",
        "handles",
        "handle_of",
        "projects",
        "gates",
        "transitions",
        "records",
        "findings",
        "gate_runs",
        "attempts",
        "github_bindings",
        "ledger_published",
        "ledger_cutovers",
        "ledger_candidate_runs",
        "ledger_candidate_attempts",
        "ledger_roots",
        "ledger_heads",
        "ledger_segments",
        "routing",
        "state_entries",
        "state_keys",
        "state_revisions",
        "state_revision_ids",
        "agents",
        "agent_by_name",
        "areas",
        "agent_events",
        "agent_event_ids",
        "wire_messages",
        "wire_msg_log",
        "wire_msg_seq",
        "wire_inbox",
        "wire_rcpt_log",
        "wire_threads",
        "wire_replies",
        "workflow_defs",
        "workflow_slugs",
        "workflow_runs",
        "__meta",
    ] {
        let t = table(&report, name);
        assert!(t.redb_rows > 0, "{name} is empty: the fixture misses it");
        assert_eq!(t.redb_rows, t.ferro_rows, "{name}");
    }
    assert_eq!(table(&report, "ferrostore:unlisted").ferro_rows, 0);
    assert!(
        !report.tables.iter().any(|t| t.table.starts_with("redb:")),
        "every redb table is on the copy list"
    );
    for (prefix, at_least) in [
        ("Catalog::list_projects", 1),
        ("Catalog::list_gates(", 2),
        ("Catalog::list_transitions(", 1),
        ("Tracker::list_records(", 1),
        ("Tracker::list_findings(", 1),
        ("Ledger::attempts(", 2),
        ("Ledger::gate_runs(", 3),
        ("Outbox::unpublished(R_1", 2),
        ("LedgerCache::cached_under(R_1, runs)", 1),
        ("AgentRegistry::list_areas", 1),
        ("AgentRegistry::list_agents", 1),
        ("AgentRegistry::registry_events_after", 1),
        ("StateStore::list_state", 2),
        ("StateStore::state_revisions_after", 2),
        ("StateStore::state_revisions(", 1),
        ("MessageStore::list_wire_inbox", 2),
        ("MessageStore::wire_messages_after", 2),
        ("MessageStore::wire_receipts_after", 2),
        ("MessageStore::list_wire_thread(", 2),
        ("MessageStore::list_wire_receipts(", 1),
        ("WorkflowStore::list_workflow_defs(all)", 1),
        ("WorkflowStore::list_workflow_runs(all)", 1),
    ] {
        let v = view(&report, prefix);
        assert!(v.redb_rows >= at_least, "{prefix}: {v:?}");
    }
    assert!(
        report
            .trait_views
            .iter()
            .any(|v| v.table == format!("Ledger::gate_runs({})", populated.gate))
    );
    assert!(
        report
            .trait_views
            .iter()
            .any(|v| v.table == format!("Tracker::list_records({})", populated.project))
    );
}

#[test]
fn a_row_changed_after_migration_is_a_mismatch_in_its_table_and_its_view() {
    let (_dir, redb, ferro, populated) = migrated();
    let mut gate = ferro
        .get_doc("gates", populated.gate.iri().as_str())
        .unwrap()
        .unwrap();
    gate["name"] = json!("renamed");
    ferro
        .put_doc("gates", populated.gate.iri().as_str(), &gate)
        .unwrap();
    let report = verify_redb_vs_ferrostore(&redb, &ferro).unwrap();
    assert!(!report.is_clean());
    let gates = table(&report, "gates");
    assert_eq!(
        gates.redb_rows, gates.ferro_rows,
        "same count, other content"
    );
    assert_ne!(gates.redb_hash, gates.ferro_hash);
    let names = mismatch_names(&report);
    assert!(names.contains(&"gates".to_string()), "{names:?}");
    assert!(
        names.contains(&format!("Catalog::list_gates({})", populated.project)),
        "{names:?}"
    );
    assert!(!names.contains(&"records".to_string()), "{names:?}");
}

#[test]
fn a_missing_row_is_a_mismatch() {
    let (_dir, redb, ferro, populated) = migrated();
    ferro
        .delete_doc("records", populated.record.iri().as_str())
        .unwrap();
    let report = verify_redb_vs_ferrostore(&redb, &ferro).unwrap();
    let records = table(&report, "records");
    assert_eq!(records.ferro_rows + 1, records.redb_rows);
    let names = mismatch_names(&report);
    assert!(names.contains(&"records".to_string()), "{names:?}");
    assert!(
        names.contains(&format!("Tracker::list_records({})", populated.project)),
        "{names:?}"
    );
}

#[test]
fn a_raw_string_stored_as_parsed_json_is_a_mismatch() {
    // `put_raw` keeps a raw `&str` a JSON string; a value parsed from it is not.
    let (_dir, redb, ferro, populated) = migrated();
    let kind = ferro
        .get_doc("ids", populated.record.iri().as_str())
        .unwrap()
        .unwrap();
    assert!(kind.is_string(), "{kind}");
    ferro
        .put_doc(
            "ids",
            populated.record.iri().as_str(),
            &json!({"kind": kind}),
        )
        .unwrap();
    let report = verify_redb_vs_ferrostore(&redb, &ferro).unwrap();
    assert!(mismatch_names(&report).contains(&"ids".to_string()));
}

#[test]
fn data_outside_the_copy_list_is_a_mismatch_on_either_side() {
    let (_dir, redb, ferro, _) = migrated();
    ferro.put_doc("stray", "x", &json!(1)).unwrap();
    let report = verify_redb_vs_ferrostore(&redb, &ferro).unwrap();
    assert_eq!(
        mismatch_names(&report),
        vec!["ferrostore:unlisted".to_string()]
    );

    let (_dir, redb, ferro, _) = migrated();
    {
        let tx = redb.db.begin_write().unwrap();
        tx.open_table(redb::TableDefinition::<&str, &str>::new("mystery"))
            .unwrap()
            .insert("k", "v")
            .unwrap();
        tx.commit().unwrap();
    }
    let report = verify_redb_vs_ferrostore(&redb, &ferro).unwrap();
    assert_eq!(mismatch_names(&report), vec!["redb:mystery".to_string()]);
    assert_eq!(table(&report, "redb:mystery").redb_rows, 1);

    let (_dir, redb, ferro, _) = migrated();
    ferro
        .put_doc("__meta", "schema_version", &json!(crate::FORMAT_VERSION))
        .unwrap();
    let report = verify_redb_vs_ferrostore(&redb, &ferro).unwrap();
    assert_eq!(mismatch_names(&report), vec!["__meta".to_string()]);
}

#[test]
fn the_hash_is_sha256_over_sorted_length_prefixed_canonical_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let redb = RedbStore::open(&tmp.path().join("s.redb")).unwrap();
    let ferro = FerroStore::in_memory();
    migrate_redb_to_ferrostore(&redb, &ferro).unwrap();
    ferro
        .put_doc("projects", "b", &json!({"z": 1, "a": [{"d": 1, "c": 2}]}))
        .unwrap();
    ferro.put_doc("projects", "a", &json!("x")).unwrap();
    let report = verify_redb_vs_ferrostore(&redb, &ferro).unwrap();
    let projects = table(&report, "projects");
    let mut want = Sha256::new();
    for (key, value) in [("a", r#""x""#), ("b", r#"{"a":[{"c":2,"d":1}],"z":1}"#)] {
        want.update((key.len() as u64).to_le_bytes());
        want.update(key.as_bytes());
        want.update((value.len() as u64).to_le_bytes());
        want.update(value.as_bytes());
    }
    let want: [u8; 32] = want.finalize().into();
    assert_eq!(projects.ferro_rows, 2);
    assert_eq!(projects.ferro_hash, want);
    assert_eq!(projects.redb_rows, 0);
    assert_eq!(projects.redb_hash, <[u8; 32]>::from(Sha256::digest(b"")));
}

/// A populated redb store file at `dir/fl.redb`, closed.
fn redb_file(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("fl.redb");
    let redb = RedbStore::open(&path).unwrap();
    populate(&redb);
    path
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_verified_migration_publishes_the_store_and_freezes_the_redb() {
    let tmp = tempfile::tempdir().unwrap();
    let redb_path = redb_file(tmp.path());
    let fls = tmp.path().join("fl.fls");
    let report = migrate_verified(&redb_path, &fls).unwrap();
    assert!(report.is_clean(), "{:#?}", report.mismatches());
    assert_eq!(
        entries(tmp.path()),
        vec!["fl.fls".to_string(), "fl.redb".to_string()],
        "no temp file is left"
    );
    assert_eq!(mode(&fls), 0o600);
    assert_eq!(mode(&redb_path), 0o400, "the redb is kept, read-only");
    let ferro = FerroStore::open(&fls).unwrap();
    assert_eq!(ferro.list_projects().unwrap().len(), 1);
    drop(ferro);

    // An existing store is never replaced.
    let held = std::fs::read(&fls).unwrap();
    std::fs::set_permissions(&redb_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let err = migrate_verified(&redb_path, &fls).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(std::fs::read(&fls).unwrap(), held);
    assert_eq!(mode(&redb_path), 0o600, "a refusal changes nothing");

    // A missing redb is not created by the attempt.
    let missing = tmp.path().join("none.redb");
    assert!(migrate_verified(&missing, &tmp.path().join("none.fls")).is_err());
    assert!(!missing.exists());
    assert!(!tmp.path().join("none.fls").exists());
}

#[test]
fn an_unclean_verify_leaves_no_store_and_no_temp_file() {
    let tmp = tempfile::tempdir().unwrap();
    let redb_path = redb_file(tmp.path());
    let fls = tmp.path().join("fl.fls");
    let report = verify::migrate_verified_with(&redb_path, &fls, |dst| {
        let ids = dst.query_by_field("records", "title", "keep this")?;
        assert_eq!(ids.len(), 1);
        dst.delete_doc("records", &ids[0])
    })
    .unwrap();
    assert!(!report.is_clean());
    assert!(mismatch_names(&report).contains(&"records".to_string()));
    assert_eq!(
        entries(tmp.path()),
        vec!["fl.redb".to_string()],
        "neither the store nor the temp file is left"
    );
    assert_eq!(mode(&redb_path), 0o600, "the redb stays writable");
    // The caller keeps running on redb.
    let redb = RedbStore::open(&redb_path).unwrap();
    assert_eq!(redb.list_projects().unwrap().len(), 1);
}

#[test]
fn a_failed_step_after_the_copy_leaves_no_temp_file() {
    let tmp = tempfile::tempdir().unwrap();
    let redb_path = redb_file(tmp.path());
    let fls = tmp.path().join("fl.fls");
    let err = verify::migrate_verified_with(&redb_path, &fls, |_| {
        Err(crate::backend("injected failure"))
    })
    .unwrap_err();
    assert!(err.to_string().contains("injected failure"), "{err}");
    assert_eq!(entries(tmp.path()), vec!["fl.redb".to_string()]);
}
