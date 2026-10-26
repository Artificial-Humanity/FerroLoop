//! ⚠⚠ Every stored reference is a full IRI (spec §4, Invariant).
//!
//! The scan is driven by what each type SERIALIZES as an IRI, not by a list
//! of field names: every IRI string in the JSON is replaced, one at a time,
//! with a handle-shaped number, and the type must refuse to read it back. A
//! reference field that would accept a handle is the defect. Each type also
//! carries a floor on how many IRIs it must contain, so a scan that silently
//! found fewer fields fails instead of passing over nothing.

use fl_core::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

fn iri(n: u64) -> Iri {
    fl_core::ids::seq_iri(n)
}

fn iri_paths(v: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match v {
        Value::String(s) if Iri::parse(s).is_ok() && s.starts_with("urn:uuid:") => {
            out.push(path.clone())
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                path.push(i.to_string());
                iri_paths(item, path, out);
                path.pop();
            }
        }
        Value::Object(map) => {
            for (k, item) in map {
                path.push(k.clone());
                iri_paths(item, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

fn set(v: &mut Value, path: &[String], to: Value) {
    let mut cur = v;
    for key in path {
        cur = match cur {
            Value::Array(a) => &mut a[key.parse::<usize>().unwrap()],
            Value::Object(m) => m.get_mut(key).unwrap(),
            _ => unreachable!(),
        };
    }
    *cur = to;
}

fn assert_every_reference_refuses_a_handle<T: Serialize + DeserializeOwned>(
    name: &str,
    sample: &T,
    floor: usize,
) {
    let json = serde_json::to_value(sample).unwrap();
    let mut paths = Vec::new();
    iri_paths(&json, &mut Vec::new(), &mut paths);
    assert!(
        paths.len() >= floor,
        "{name}: found {} IRI fields, expected at least {floor}",
        paths.len()
    );
    for p in &paths {
        let mut broken = json.clone();
        set(&mut broken, p, Value::from(3u64));
        assert!(
            serde_json::from_value::<T>(broken).is_err(),
            "{name}.{} accepted a handle where a full IRI belongs",
            p.join(".")
        );
    }
}

#[test]
fn every_reference_field_on_the_wire_is_a_full_iri() {
    let (p, g, r, f) = (
        ProjectId(iri(1)),
        GateId(iri(2)),
        RecordId(iri(3)),
        FindingId(iri(4)),
    );
    // One sample per type that crosses the process boundary, with every
    // Option set to Some and every Vec non-empty, so no reference field is
    // hidden by a None or an empty list.
    let project = Project {
        id: p.clone(),
        root: "/p".into(),
    };
    let gate_def = GateDef {
        id: g.clone(),
        project: p.clone(),
        name: "g".into(),
        kind: GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec!["a".into()],
            delivery: PopulationDelivery::Args,
            timeout_secs: 1,
            pass_codes: vec![0],
        }),
        selector: Selector::Glob {
            pattern: "*.rs".into(),
        },
        min_population: 1,
        authored_at_commit: "abc".into(),
        authored_by: "o".into(),
        last_pass_commit: Some("abc".into()),
    };
    let transition = Transition {
        project: p.clone(),
        name: "launch".into(),
        from: State::Review,
        to: State::Done,
        regret: Regret::High,
        gates: vec![g.clone()],
    };
    let record = Record {
        id: r.clone(),
        project: p.clone(),
        title: "t".into(),
        state: State::Todo,
        also_known_as: vec![iri(5)],
        area: None,
    };
    let mut finding = Finding::raise(p.clone(), r.clone(), "a", "c");
    finding.id = f.clone();
    finding.reproduction = Some(g.clone());
    finding.also_known_as = vec![iri(6)];
    let gate_run = GateRun {
        id: Some(iri(7)),
        at: Some(At::from_unix_millis(1)),
        gate: g.clone(),
        record: Some(r.clone()),
        commit: "abc".into(),
        verdict: Verdict::from_predicate(true, 1),
        population: 1,
        output_excerpt: Some(String::new()),
        duration_ms: 1,
        cost_usd_micros: 0,
    };
    let attempt = Attempt {
        id: Some(iri(8)),
        at: Some(At::from_unix_millis(2)),
        project: p.clone(),
        record: r.clone(),
        adapter: "claude".into(),
        status: AttemptStatus::Completed,
        duration_ms: 1,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd_micros: 0,
        paths_touched: PathsTouched::Listed(vec!["a.rs".into()]),
        output_excerpt: Some(String::new()),
    };
    let decision = Decision {
        id: iri(9),
        at: At::from_unix_millis(3),
        record: r.clone(),
        finding: Some(f.clone()),
        outcome: Outcome::Verify {
            reproduction: g.clone(),
            reproduction_passed: true,
            regressions: vec![g.clone()],
            closed: false,
        },
        rests_on: vec![iri(10)],
    };
    assert_every_reference_refuses_a_handle("Project", &project, 1);
    assert_every_reference_refuses_a_handle("GateDef", &gate_def, 2);
    assert_every_reference_refuses_a_handle("Transition", &transition, 2);
    assert_every_reference_refuses_a_handle("Record", &record, 3);
    assert_every_reference_refuses_a_handle("Finding", &finding, 5);
    assert_every_reference_refuses_a_handle("GateRun", &gate_run, 3);
    assert_every_reference_refuses_a_handle("Attempt", &attempt, 3);
    assert_every_reference_refuses_a_handle("Decision", &decision, 6);

    let state_entry = StateEntry {
        id: StateId(iri(11)),
        scope: StateScope::Project("ferroloop".into()),
        concern: StateConcern::Memory,
        slug: "arch".into(),
        sensitive: false,
        ever_sensitive: false,
        payload: StatePayload::Memory(MemoryEntry {
            category: MemoryCategory::Decision,
            basis: MemoryBasis::Ruling,
            lifecycle: MemoryLifecycle::Superseded,
            superseded_by: Some(StateId(iri(13))),
            pinned: true,
            summary: "summary".into(),
            body: "body".into(),
        }),
        head_revision: RevisionId(iri(12)),
        updated_at: At::from_unix_millis(5),
        by: "ferris".into(),
    };
    let state_revision = StateRevision {
        revision_id: RevisionId(iri(12)),
        entry_id: StateId(iri(11)),
        parent_revisions: vec![RevisionId(iri(14))],
        scope: StateScope::Project("ferroloop".into()),
        concern: StateConcern::Memory,
        slug: "arch".into(),
        sensitive: false,
        ever_sensitive: false,
        payload: state_entry.payload.clone(),
        at: At::from_unix_millis(5),
        by: "ferris".into(),
    };
    let agent_record = AgentRecord {
        id: AgentId(iri(15)),
        name_key: "ferris".into(),
        display_name: "Ferris".into(),
        email: "ferris@artificialhumanity.io".into(),
        project: "ferroloop".into(),
        role: AgentRole::Lead,
        extra_areas: vec![],
        status: AgentStatus::Active,
        updated_at: At::from_unix_millis(6),
        by: "owner".into(),
    };
    let registry_event = RegistryEvent {
        event_id: iri(16),
        at: At::from_unix_millis(6),
        by: "owner".into(),
        kind: RegistryEventKind::AgentUpsert(agent_record.clone()),
    };
    assert_every_reference_refuses_a_handle("StateEntry", &state_entry, 3);
    assert_every_reference_refuses_a_handle("StateRevision", &state_revision, 4);
    assert_every_reference_refuses_a_handle("AgentRecord", &agent_record, 1);
    assert_every_reference_refuses_a_handle("RegistryEvent", &registry_event, 2);

    let wire_message = WireMessage {
        id: MessageId(iri(17)),
        thread_id: MessageId(iri(18)),
        reply_to: Some(MessageId(iri(19))),
        correlation_id: Some(MessageId(iri(20))),
        sender_id: Some(AgentId(iri(21))),
        sender_name: "ferris".into(),
        sender_host: Some("ai-lab-0".into()),
        to: WireAddress::AgentId(AgentId(iri(22))),
        kind: MessageKind::Reply,
        urgency: MessageUrgency::Urgent,
        durable: true,
        subject: "Re: check".into(),
        body: "All green".into(),
        created_at: At::from_unix_millis(7),
        deadline_at: Some(At::from_unix_millis(8)),
    };
    let delivery_receipt = DeliveryReceipt {
        message_id: MessageId(iri(17)),
        recipient_id: iri(22),
        recipient_name: "jarvis".into(),
        status: DeliveryStatus::Acked,
        attempts: 1,
        updated_at: At::from_unix_millis(8),
        acked_at: Some(At::from_unix_millis(8)),
    };
    assert_every_reference_refuses_a_handle("WireMessage", &wire_message, 6);
    assert_every_reference_refuses_a_handle("DeliveryReceipt", &delivery_receipt, 2);

    assert_every_reference_refuses_a_handle(
        "AttemptHeartbeat",
        &fl_core::telemetry::AttemptHeartbeat {
            attempt_id: fl_core::ids::seq_iri(1),
            project: ProjectId(fl_core::ids::seq_iri(2)),
            record: RecordId(fl_core::ids::seq_iri(3)),
            adapter: "claude".into(),
            agent: Some("ferris".into()),
            host: "ai-lab-0".into(),
            pid: 1234,
            started_at: At::from_unix_millis(1_000),
            heartbeat_at: At::from_unix_millis(2_000),
            status: fl_core::telemetry::LivenessStatus::Live,
            outcome: None,
            exit_at: None,
            duration_ms: 1_000,
            tokens_in: 10,
            tokens_out: 20,
            cost_usd_micros: 50,
        },
        3,
    );
}

#[test]
fn pr_policy_wire_types_round_trip_cleanly() {
    for tier in [
        RequiredTier::Lead {
            project: "ferroloop".into(),
        },
        RequiredTier::LabManager,
        RequiredTier::Owner,
    ] {
        let json = serde_json::to_string(&tier).unwrap();
        let back: RequiredTier = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tier);
    }

    for verdict in ReviewVerdict::ALL {
        let json = serde_json::to_string(verdict).unwrap();
        let back: ReviewVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(back, *verdict);
    }

    let marker = FlReviewMarker {
        reviewer: "ferris".into(),
        role: AgentRole::Lead,
        project: "ferroloop".into(),
        head_sha: "abc1234".into(),
        verdict: ReviewVerdict::Approved,
        at: At::from_unix_millis(1_000),
    };
    let marker_json = serde_json::to_string(&marker).unwrap();
    let marker_back: FlReviewMarker = serde_json::from_str(&marker_json).unwrap();
    assert_eq!(marker_back, marker);

    let eval = PrPolicyEvaluation {
        project: "ferroloop".into(),
        co_authors: vec!["felicia".into()],
        unattributed_commits: vec![],
        unknown_co_authors: vec![],
        protected_files: vec![],
        required_tier: RequiredTier::Lead {
            project: "ferroloop".into(),
        },
        target_reviewer: "ferris".into(),
        escalation_reasons: vec![],
    };
    let eval_json = serde_json::to_string(&eval).unwrap();
    let eval_back: PrPolicyEvaluation = serde_json::from_str(&eval_json).unwrap();
    assert_eq!(eval_back, eval);

    for refusal in [
        PolicyRefusal::UnknownOrInactiveAgent {
            agent: "ghost".into(),
        },
        PolicyRefusal::SelfReviewForbidden {
            agent: "felicia".into(),
        },
        PolicyRefusal::ProtectedPathRequiresOwner {
            agent: "ferris".into(),
            files: vec![".github/workflows/ci.yml".into()],
        },
        PolicyRefusal::UnattributedCommitsRequireOwner {
            commits: vec!["deadbeef".into()],
        },
        PolicyRefusal::UnknownCoAuthorsRequireOwner {
            co_authors: vec!["stranger".into()],
        },
        PolicyRefusal::InsufficientRole {
            agent: "felicia".into(),
            role: AgentRole::Assistant,
            required: RequiredTier::Lead {
                project: "ferroloop".into(),
            },
        },
        PolicyRefusal::MissingValidApproval {
            head_sha: "abc1234".into(),
        },
    ] {
        let json = serde_json::to_string(&refusal).unwrap();
        let back: PolicyRefusal = serde_json::from_str(&json).unwrap();
        assert_eq!(back, refusal);
        assert!(!refusal.to_string().is_empty());
    }
}
