//! Offline conformance around the real proof evaluator and process harness.
//! The in-memory ledger below is a test double, not a production admission path.
#![cfg(unix)]

use fl_core::governor::{
    AdmissionOutcome, AdmissionRequest, GovernorPolicy, GovernorStore, MemGovernorStore, Principal,
    SettleRequest, SettledOutcome,
};
use fl_core::ids::{RecordId, seq_iri};
use fl_core::log::AttemptStatus;
use fl_core::slice_proof::{
    AccountingUnit, AdmittedSliceBound, EpsilonStepBound, SliceAdmissionRefusal, SliceBoundKind,
    SliceBoundProof, SliceProofInput, Usd, evaluate_slice_bounds,
};
use fl_core::telemetry::{AdapterKind, RateCard};
use fl_exec::adapters::harness::{
    CliDialect, ParsedOutput, ProcessHarness, parse_json_or_jsonl_envelope,
};
use fl_exec::runner::AttemptSpec;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

fn usd(s: &str) -> Usd {
    Usd::parse(s).unwrap()
}
fn proof(kind: SliceBoundKind) -> SliceBoundProof {
    SliceBoundProof {
        proof_record_id: "fixture-proof".into(),
        adapter: "fake".into(),
        mode: "print".into(),
        cli_version_range: ">=1.0.0, <2.0.0".into(),
        binary_hash: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .into(),
        model: "fake-model".into(),
        auth_billing_mode: "api".into(),
        config_digest: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .into(),
        kind,
        accounting_unit: AccountingUnit::ActualUsd {
            billing_contract: "test-only-contract".into(),
        },
        transcript_ref: "synthetic-only".into(),
    }
}
fn arithmetic() -> SliceBoundProof {
    proof(SliceBoundKind::Arithmetic {
        steps_knob: "max-steps".into(),
        max_steps: 2,
        input_knob: "max-input".into(),
        max_input_tokens_per_step: 100,
        max_out_knob: "max-output".into(),
        max_output_tokens_per_step: 20,
        price_per_input_token: usd("0.000001"),
        price_per_output_token: usd("0.000002"),
    })
}
fn vendor_cap() -> SliceBoundProof {
    proof(SliceBoundKind::VendorCap {
        knob: "max-budget-usd".into(),
        cap_usd: usd("0.000100"),
        epsilon_tokens: 120,
        epsilon_usd: usd("0.000140"),
        token_bound_knob: "whole-slice-tokens".into(),
        max_total_tokens: Some(240),
        epsilon_step: Some(EpsilonStepBound {
            input_knob: "max-input".into(),
            max_input_tokens: 100,
            output_knob: "max-output".into(),
            max_output_tokens: 20,
            price_per_input_token: usd("0.000001"),
            price_per_output_token: usd("0.000002"),
        }),
    })
}
fn input() -> SliceProofInput<'static> {
    SliceProofInput {
        adapter: "fake",
        mode: "print",
        installed_cli_version: "1.0.0",
        binary_hash: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        model: "fake-model",
        auth_billing_mode: "api",
        config_digest: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    }
}

/// Test-only model of one atomic reservation. It never claims durable store semantics.
#[derive(Debug)]
struct TestLedger {
    tokens: u64,
    micros: u64,
    reservations: Vec<(u64, u64)>,
}
impl TestLedger {
    fn reserve(&mut self, bound: &AdmittedSliceBound) -> bool {
        if bound.max_tokens > self.tokens || bound.max_cost_micros > self.micros {
            return false;
        }
        self.tokens -= bound.max_tokens;
        self.micros -= bound.max_cost_micros;
        self.reservations
            .push((bound.max_tokens, bound.max_cost_micros));
        true
    }
}
#[derive(Debug, Clone, Copy)]
struct FakeDialect;
impl CliDialect for FakeDialect {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Claude
    }
    fn default_binary(&self) -> &'static str {
        "fake"
    }
    fn argv(&self, _: &AttemptSpec) -> Vec<String> {
        vec![]
    }
    fn parse_output(&self, stdout: &[u8], stderr: &[u8]) -> ParsedOutput {
        parse_json_or_jsonl_envelope(stdout, stderr, true)
    }
}
fn fake(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-vendor");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut mode = std::fs::metadata(&path).unwrap().permissions();
    mode.set_mode(0o755);
    std::fs::set_permissions(&path, mode).unwrap();
    path
}
fn harness(bin: &Path, marker: &Path, receipt: &Path) -> ProcessHarness<FakeDialect> {
    ProcessHarness::new(bin.to_string_lossy().into_owned(), FakeDialect)
        .with_env(vec![
            ("FAKE_MARKER".into(), marker.as_os_str().into()),
            ("FAKE_RECEIPT".into(), receipt.as_os_str().into()),
        ])
        .with_rate_card(Some(RateCard {
            input_usd_micros_per_mtok: 1_000_000,
            output_usd_micros_per_mtok: 2_000_000,
        }))
}
fn attempt(root: &Path, ceiling: u64) -> AttemptSpec {
    AttemptSpec {
        project_root: root.into(),
        record: RecordId(seq_iri(1)),
        instruction: "fixture".into(),
        timeout_secs: 5,
        budget_usd_micros: ceiling,
    }
}
async fn admit_then_run(
    proofs: &[SliceBoundProof],
    identity: &SliceProofInput<'_>,
    ledger: &Arc<Mutex<TestLedger>>,
    dir: &TempDir,
    bin: &Path,
) -> Result<fl_exec::AttemptOutcome, SliceAdmissionRefusal> {
    let bound = evaluate_slice_bounds(proofs, identity)?;
    let receipt = dir.path().join("reserved");
    {
        let mut guard = ledger.lock().unwrap();
        if !guard.reserve(&bound) {
            return Err(SliceAdmissionRefusal::Unproven);
        }
        std::fs::write(
            &receipt,
            format!("{}:{}", bound.max_tokens, bound.max_cost_micros),
        )
        .unwrap();
    }
    let marker = dir.path().join("spawned");
    Ok(harness(bin, &marker, &receipt)
        .run(attempt(dir.path(), bound.max_cost_micros))
        .await)
}

#[tokio::test]
async fn both_proof_paths_reserve_before_real_fake_vendor_spawn() {
    for (proof, expected) in [(arithmetic(), (240, 280)), (vendor_cap(), (360, 240))] {
        let dir = TempDir::new().unwrap();
        let bin = fake(
            dir.path(),
            r#"test -s "$FAKE_RECEIPT" || exit 9
cp "$FAKE_RECEIPT" "$FAKE_MARKER"
printf '%s\n' '{"text":"step 1","usage":{"input_tokens":40,"output_tokens":10}}' '{"text":"step 2","usage":{"input_tokens":30,"output_tokens":10}}'"#,
        );
        let ledger = Arc::new(Mutex::new(TestLedger {
            tokens: expected.0,
            micros: expected.1,
            reservations: vec![],
        }));
        let out = admit_then_run(&[proof], &input(), &ledger, &dir, &bin)
            .await
            .unwrap();
        assert_eq!(out.status, AttemptStatus::Completed, "{out:?}");
        assert_eq!(
            (out.tokens_in, out.tokens_out, out.cost_usd_micros),
            (70, 20, 110)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("spawned")).unwrap(),
            format!("{}:{}", expected.0, expected.1)
        );
        assert_eq!(ledger.lock().unwrap().reservations, vec![expected]);
    }
}

#[tokio::test]
async fn missing_proof_and_every_pinned_drift_refuse_without_spawn_or_reservation() {
    let dir = TempDir::new().unwrap();
    let bin = fake(dir.path(), "touch \"$FAKE_MARKER\"");
    let ledger = Arc::new(Mutex::new(TestLedger {
        tokens: 1000,
        micros: 1000,
        reservations: vec![],
    }));
    assert_eq!(
        admit_then_run(&[], &input(), &ledger, &dir, &bin)
            .await
            .unwrap_err(),
        SliceAdmissionRefusal::ManualOnly
    );
    let good = arithmetic();
    let variants = [
        SliceProofInput {
            adapter: "other",
            ..input()
        },
        SliceProofInput {
            mode: "interactive",
            ..input()
        },
        SliceProofInput {
            installed_cli_version: "2.0.0",
            ..input()
        },
        SliceProofInput {
            binary_hash: "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            ..input()
        },
        SliceProofInput {
            model: "drift",
            ..input()
        },
        SliceProofInput {
            auth_billing_mode: "subscription",
            ..input()
        },
        SliceProofInput {
            config_digest: "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            ..input()
        },
    ];
    for i in variants {
        assert_eq!(
            admit_then_run(std::slice::from_ref(&good), &i, &ledger, &dir, &bin)
                .await
                .unwrap_err(),
            SliceAdmissionRefusal::Unproven
        );
    }
    assert!(!dir.path().join("spawned").exists());
    assert!(!dir.path().join("reserved").exists());
    assert!(ledger.lock().unwrap().reservations.is_empty());
}

#[test]
fn competing_test_reservations_cannot_spend_the_same_remainder() {
    let bound = evaluate_slice_bounds(&[arithmetic()], &input()).unwrap();
    let ledger = Arc::new(Mutex::new(TestLedger {
        tokens: 240,
        micros: 280,
        reservations: vec![],
    }));
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut threads = vec![];
    for _ in 0..2 {
        let (ledger, bound, barrier) = (ledger.clone(), bound.clone(), barrier.clone());
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            ledger.lock().unwrap().reserve(&bound)
        }));
    }
    barrier.wait();
    let wins = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(wins, 1);
    assert_eq!(ledger.lock().unwrap().reservations.len(), 1);
}

/// Exercise the real single-unit governor with micro-USD units. This demonstrates its
/// unknown-outcome accounting, not a production Track C dual token/cost reservation.
#[tokio::test]
async fn unknown_interrupted_slice_keeps_full_cost_charge_and_live_permit() {
    let dir = TempDir::new().unwrap();
    let bin = fake(
        dir.path(),
        r#"test -s "$FAKE_RECEIPT" || exit 9
 touch "$FAKE_MARKER"
 printf '%s\n' '{"text":"partial","usage":{"input_tokens":40,"output_tokens":10}}'
 kill -INT $$"#,
    );
    let bound = evaluate_slice_bounds(&[arithmetic()], &input()).unwrap();
    assert_eq!((bound.max_tokens, bound.max_cost_micros), (240, 280));
    let store = MemGovernorStore::new(GovernorPolicy {
        budget_units_per_scope: bound.max_cost_micros,
        concurrency_per_scope: 1,
        ..GovernorPolicy::default()
    });
    let request = AdmissionRequest::new(Principal::Owner, "project", "fake", bound.max_cost_micros);
    let permit = match store.reserve(&request, 1).unwrap() {
        AdmissionOutcome::Admitted(permit) => permit,
        other => panic!("expected a cost reservation: {other:?}"),
    };
    let before = store.snapshot();
    let principal = &before.scopes["principal:owner"];
    assert_eq!(
        (
            principal.budget_used,
            principal.budget_reserved,
            principal.concurrency_live
        ),
        (0, 280, 1)
    );
    let receipt = dir.path().join("reserved");
    std::fs::write(&receipt, permit.reserved_units.to_string()).unwrap();
    let marker = dir.path().join("spawned");
    let out = harness(&bin, &marker, &receipt)
        .run(attempt(dir.path(), bound.max_cost_micros))
        .await;
    assert_eq!(out.status, AttemptStatus::Crashed);
    assert_eq!(
        (out.tokens_in, out.tokens_out, out.cost_usd_micros),
        (40, 10, 60)
    );
    assert!(marker.exists());

    // The interrupted process may have spent more than its last observed usage. An unknown
    // outcome charges the full cost reservation and keeps the permit live for reconciliation.
    let settlement = SettleRequest {
        permit_id: permit.id.clone(),
        actual_units: out.cost_usd_micros,
        outcome: SettledOutcome::UnknownAfterSend,
        complete: true,
    };
    store.settle(&settlement).unwrap();
    store.settle(&settlement).unwrap(); // settlement is idempotent
    let after = store.snapshot();
    let principal = &after.scopes["principal:owner"];
    assert_eq!(
        (
            principal.budget_used,
            principal.budget_reserved,
            principal.concurrency_live
        ),
        (280, 0, 1)
    );
    assert!(
        store
            .unconfirmed_permits()
            .iter()
            .any(|p| p.id == permit.id)
    );
    assert!(matches!(
        store.reserve(&request, 2).unwrap(),
        AdmissionOutcome::Deferred { .. }
    ));
}

#[tokio::test]
async fn reactive_budget_safeguard_stops_fake_vendor_after_reported_overspend() {
    let dir = TempDir::new().unwrap();
    let bin = fake(
        dir.path(),
        r#"touch "$FAKE_MARKER"
printf '%s\n' '{"text":"overspend","usage":{"input_tokens":200,"output_tokens":100}}'
sleep 30"#,
    );
    let marker = dir.path().join("spawned");
    let start = std::time::Instant::now();
    let out = harness(&bin, &marker, &dir.path().join("unused"))
        .run(attempt(dir.path(), 280))
        .await;
    assert_eq!(out.status, AttemptStatus::Crashed);
    assert!(marker.exists());
    assert_eq!(out.cost_usd_micros, 400);
    assert!(out.output_excerpt.contains("spend ceiling"));
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

fn arithmetic_with(max_steps: u64, input_price: &str, output_price: &str) -> SliceBoundProof {
    proof(SliceBoundKind::Arithmetic {
        steps_knob: "max-steps".into(),
        max_steps,
        input_knob: "max-input".into(),
        max_input_tokens_per_step: 100,
        max_out_knob: "max-output".into(),
        max_output_tokens_per_step: 20,
        price_per_input_token: usd(input_price),
        price_per_output_token: usd(output_price),
    })
}

/// A scripted multi-step stream that stays inside the per-step caps and the step cap. The
/// expected bound is computed here from the descriptor values, not read back from the code
/// under test: steps x (max_in + max_out) tokens and steps x (max_in x p_in + max_out x p_out).
#[tokio::test]
async fn multi_step_stream_stays_inside_the_evaluated_arithmetic_bound() {
    const STEPS: u64 = 4;
    let steps = [(40u64, 10u64), (35, 12), (60, 8), (20, 20)];
    assert_eq!(steps.len() as u64, STEPS);
    let proof = arithmetic_with(STEPS, "0.000001", "0.000002");
    let bound = evaluate_slice_bounds(std::slice::from_ref(&proof), &input()).unwrap();
    assert_eq!(bound.max_tokens, STEPS * (100 + 20));
    assert_eq!(bound.max_cost_micros, STEPS * (100 + 20 * 2));
    assert_eq!((bound.max_tokens, bound.max_cost_micros), (480, 560));
    // Every scripted step is itself inside the per-step factors the proof was built from.
    assert!(steps.iter().all(|(i, o)| *i <= 100 && *o <= 20));

    let dir = TempDir::new().unwrap();
    let lines: String = steps
        .iter()
        .map(|(i, o)| {
            format!(r#"{{"text":"step","usage":{{"input_tokens":{i},"output_tokens":{o}}}}}"#)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let bin = fake(
        dir.path(),
        &format!(
            "test -s \"$FAKE_RECEIPT\" || exit 9\ncp \"$FAKE_RECEIPT\" \"$FAKE_MARKER\"\ncat <<'EOF'\n{lines}\nEOF"
        ),
    );
    let ledger = Arc::new(Mutex::new(TestLedger {
        tokens: bound.max_tokens,
        micros: bound.max_cost_micros,
        reservations: vec![],
    }));
    let out = admit_then_run(&[proof], &input(), &ledger, &dir, &bin)
        .await
        .unwrap();
    assert_eq!(out.status, AttemptStatus::Completed, "{out:?}");
    let (tin, tout) = steps.iter().fold((0, 0), |a, s| (a.0 + s.0, a.1 + s.1));
    assert_eq!((out.tokens_in, out.tokens_out), (tin, tout));
    assert_eq!(out.cost_usd_micros, tin + 2 * tout); // 155 + 100
    assert!(out.tokens_in + out.tokens_out <= bound.max_tokens);
    assert!(out.cost_usd_micros <= bound.max_cost_micros);
    // The whole bound was reserved before the spawn, then nothing else was taken.
    assert_eq!(
        ledger.lock().unwrap().reservations,
        vec![(bound.max_tokens, bound.max_cost_micros)]
    );
}

/// Fractional-micro prices round the whole-slice cost UP: 3 x (100 x 0.000000333 +
/// 20 x 0.0000005) USD = 129.9 micro-USD, admitted as 130.
#[test]
fn bound_cost_is_rounded_up_to_whole_micros() {
    let proof = arithmetic_with(3, "0.000000333", "0.0000005");
    let bound = evaluate_slice_bounds(&[proof], &input()).unwrap();
    assert_eq!(bound.max_tokens, 360);
    assert_eq!(bound.max_cost_micros, 130);
}

/// Every class of model call that the step factor of a P-B proof must include.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepClass {
    Model,
    Subagent,
    Compaction,
    Retry,
    Planner,
}
impl StepClass {
    const ALL: [StepClass; 5] = [
        StepClass::Model,
        StepClass::Subagent,
        StepClass::Compaction,
        StepClass::Retry,
        StepClass::Planner,
    ];
    fn parse(tag: &str) -> Option<Self> {
        Some(match tag {
            "model" => Self::Model,
            "subagent" => Self::Subagent,
            "compaction" => Self::Compaction,
            "retry" => Self::Retry,
            "planner" => Self::Planner,
            _ => return None,
        })
    }
}
#[derive(Debug, Default, PartialEq, Eq)]
struct StepCount {
    by_class: [u64; 5],
    tokens: u64,
}
impl StepCount {
    fn total(&self) -> u64 {
        self.by_class.iter().sum()
    }
}
/// Test-local classifier (the production code has no step-counting notion yet). Every
/// usage-bearing line is one billed step; each must carry a known class tag, so a class the
/// counter does not know is an error rather than an uncounted step.
fn count_steps(stream: &str) -> Result<StepCount, String> {
    let mut count = StepCount::default();
    for line in stream.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let Some(usage) = v.get("usage") else {
            continue;
        };
        let tag = v.get("step_class").and_then(|t| t.as_str()).unwrap_or("");
        let class = StepClass::parse(tag).ok_or_else(|| format!("unclassified step: {line}"))?;
        let idx = StepClass::ALL.iter().position(|c| *c == class).unwrap();
        count.by_class[idx] += 1;
        for key in ["input_tokens", "output_tokens"] {
            count.tokens += usage[key].as_u64().ok_or("missing usage field")?;
        }
    }
    Ok(count)
}

/// A forced-compaction, subagent, retry and planner stream. The step factor of the proof must
/// count all of them: the harness bills every usage line, so a counter that skips a class
/// under-reports both the step total and the token sum the harness observed.
#[tokio::test]
async fn subagent_compaction_retry_and_planner_steps_are_counted_in_the_step_factor() {
    let stream = [
        r#"{"step_class":"planner","text":"plan","usage":{"input_tokens":20,"output_tokens":5}}"#,
        r#"{"step_class":"model","text":"a","usage":{"input_tokens":40,"output_tokens":10}}"#,
        r#"{"step_class":"subagent","text":"sub","usage":{"input_tokens":30,"output_tokens":8}}"#,
        r#"{"step_class":"retry","text":"again","usage":{"input_tokens":40,"output_tokens":10}}"#,
        r#"{"step_class":"compaction","text":"summary","usage":{"input_tokens":90,"output_tokens":20}}"#,
        r#"{"step_class":"model","text":"b","usage":{"input_tokens":50,"output_tokens":6}}"#,
    ]
    .join("\n");
    let counted = count_steps(&stream).unwrap();
    for class in StepClass::ALL {
        let idx = StepClass::ALL.iter().position(|c| *c == class).unwrap();
        assert!(counted.by_class[idx] > 0, "{class:?} is not exercised");
    }
    assert_eq!(counted.total(), 6);

    let proof = arithmetic_with(counted.total(), "0.000001", "0.000002");
    let bound = evaluate_slice_bounds(std::slice::from_ref(&proof), &input()).unwrap();
    let dir = TempDir::new().unwrap();
    let bin = fake(
        dir.path(),
        &format!(
            "test -s \"$FAKE_RECEIPT\" || exit 9\ncp \"$FAKE_RECEIPT\" \"$FAKE_MARKER\"\ncat <<'EOF'\n{stream}\nEOF"
        ),
    );
    let ledger = Arc::new(Mutex::new(TestLedger {
        tokens: bound.max_tokens,
        micros: bound.max_cost_micros,
        reservations: vec![],
    }));
    let out = admit_then_run(&[proof], &input(), &ledger, &dir, &bin)
        .await
        .unwrap();
    assert_eq!(out.status, AttemptStatus::Completed, "{out:?}");
    // The counter agrees with what the harness actually saw billed: nothing hidden, nothing extra.
    assert_eq!(counted.tokens, out.tokens_in + out.tokens_out);
    assert!(out.tokens_in + out.tokens_out <= bound.max_tokens);
    assert!(out.cost_usd_micros <= bound.max_cost_micros);

    // A step factor sized from the plain model steps alone is voided by this stream: the same
    // billed usage exceeds that smaller proof's token and cost bound.
    let plain = counted.by_class[0];
    assert_eq!(plain, 2);
    let small =
        evaluate_slice_bounds(&[arithmetic_with(plain, "0.000001", "0.000002")], &input()).unwrap();
    assert!(out.tokens_in + out.tokens_out > small.max_tokens);
}

#[test]
fn an_unclassified_step_is_an_error_not_an_uncounted_step() {
    let hidden =
        r#"{"step_class":"hidden-reasoning","usage":{"input_tokens":1,"output_tokens":1}}"#;
    assert!(count_steps(hidden).unwrap_err().contains("unclassified"));
    let untagged = r#"{"usage":{"input_tokens":1,"output_tokens":1}}"#;
    assert!(count_steps(untagged).is_err());
}

/// N tasks race for the one slice a ledger holds, each through evaluate + reserve + a real
/// fake-vendor spawn on the multi-thread runtime. Exactly one may reserve and spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_admissions_for_one_slice_spawn_exactly_one_vendor() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    const RACERS: usize = 6;
    let bound = evaluate_slice_bounds(&[arithmetic()], &input()).unwrap();
    let ledger = Arc::new(Mutex::new(TestLedger {
        tokens: bound.max_tokens,
        micros: bound.max_cost_micros,
        reservations: vec![],
    }));
    let ready = Arc::new(AtomicUsize::new(0));
    let mut tasks = vec![];
    for _ in 0..RACERS {
        let (ledger, ready) = (ledger.clone(), ready.clone());
        tasks.push(tokio::spawn(async move {
            let dir = TempDir::new().unwrap();
            let bin = fake(
                dir.path(),
                r#"test -s "$FAKE_RECEIPT" || exit 9
cp "$FAKE_RECEIPT" "$FAKE_MARKER"
printf '%s\n' '{"text":"ok","usage":{"input_tokens":40,"output_tokens":10}}'"#,
            );
            ready.fetch_add(1, Ordering::SeqCst);
            while ready.load(Ordering::SeqCst) < RACERS {
                tokio::task::yield_now().await;
            }
            let result = admit_then_run(&[arithmetic()], &input(), &ledger, &dir, &bin).await;
            (dir, result)
        }));
    }
    let mut winners = 0;
    for task in tasks {
        let (dir, result) = task.await.unwrap();
        let spawned = dir.path().join("spawned").exists();
        match result {
            Ok(out) => {
                winners += 1;
                assert_eq!(out.status, AttemptStatus::Completed, "{out:?}");
                assert!(spawned);
            }
            Err(refusal) => {
                assert_eq!(refusal, SliceAdmissionRefusal::Unproven);
                assert!(!spawned, "a refused admission spawned a vendor");
                assert!(!dir.path().join("reserved").exists());
            }
        }
    }
    assert_eq!(winners, 1);
    let ledger = ledger.lock().unwrap();
    assert_eq!(
        ledger.reservations,
        vec![(bound.max_tokens, bound.max_cost_micros)]
    );
    assert_eq!((ledger.tokens, ledger.micros), (0, 0));
}
