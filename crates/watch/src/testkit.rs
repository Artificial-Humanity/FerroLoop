//! Test fixtures shared by the module tests.

use fl_core::at::At;
use fl_core::ids::{ProjectId, RecordId, seq_iri};
use fl_core::iri::Iri;
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs,
};
use fl_core::store::StateStore;
use fl_core::telemetry::{AttemptHeartbeat, LivenessStatus};
use serde_json::Value;

/// A heartbeat for attempt `urn:fl:attempt:<attempt>` by `agent` (empty: none) on `host`, last
/// beating at `heartbeat_ms`, for project `seq_iri(1)` and record `seq_iri(2)`, pid 4242.
pub(crate) fn heartbeat(
    attempt: &str,
    agent: &str,
    host: &str,
    heartbeat_ms: u64,
    status: LivenessStatus,
) -> AttemptHeartbeat {
    AttemptHeartbeat {
        attempt_id: Iri::parse(&format!("urn:fl:attempt:{attempt}")).unwrap(),
        project: ProjectId(seq_iri(1)),
        record: RecordId(seq_iri(2)),
        adapter: "claude".into(),
        agent: (!agent.is_empty()).then(|| agent.to_string()),
        host: host.into(),
        pid: 4242,
        started_at: At::from_unix_millis(heartbeat_ms.saturating_sub(10)),
        heartbeat_at: At::from_unix_millis(heartbeat_ms),
        status,
        outcome: None,
        exit_at: None,
        duration_ms: 10,
        tokens_in: 1,
        tokens_out: 2,
        cost_usd_micros: 3,
    }
}

/// Upsert a workspace runtime entry.
pub(crate) fn put_runtime<S: StateStore + ?Sized>(store: &S, slug: &str, value: Value) {
    store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: slug.to_string(),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Runtime(RuntimeEntry {
                value,
                merge_mode: MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(1),
            by: "test".into(),
        })
        .unwrap();
}
