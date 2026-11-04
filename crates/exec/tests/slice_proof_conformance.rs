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
        binary_hash: "sha256:fixture".into(),
        model: "fake-model".into(),
        auth_billing_mode: "api".into(),
        config_digest: "sha256:fixture-config".into(),
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
        binary_hash: "sha256:fixture",
        model: "fake-model",
        auth_billing_mode: "api",
        config_digest: "sha256:fixture-config",
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
            binary_hash: "drift",
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
            config_digest: "drift",
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
