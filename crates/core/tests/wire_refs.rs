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
        also_known_as: vec![],
    };
    let mut finding = Finding::raise(p.clone(), r.clone(), "a", "c");
    finding.id = f.clone();
    finding.reproduction = Some(g.clone());
    let gate_run = GateRun {
        gate: g.clone(),
        record: Some(r.clone()),
        commit: "abc".into(),
        verdict: Verdict::from_predicate(true, 1),
        population: 1,
        output_excerpt: String::new(),
        duration_ms: 1,
        cost_usd_micros: 0,
    };
    let attempt = Attempt {
        project: p.clone(),
        record: r.clone(),
        adapter: "claude".into(),
        status: AttemptStatus::Completed,
        duration_ms: 1,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd_micros: 0,
        paths_touched: vec!["a.rs".into()],
        output_excerpt: String::new(),
    };
    assert_every_reference_refuses_a_handle("Project", &project, 1);
    assert_every_reference_refuses_a_handle("GateDef", &gate_def, 2);
    assert_every_reference_refuses_a_handle("Transition", &transition, 2);
    assert_every_reference_refuses_a_handle("Record", &record, 2);
    assert_every_reference_refuses_a_handle("Finding", &finding, 4);
    assert_every_reference_refuses_a_handle("GateRun", &gate_run, 2);
    assert_every_reference_refuses_a_handle("Attempt", &attempt, 2);
}
