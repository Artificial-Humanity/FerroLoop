//! Bounded, volatile observations of dispatch fences. These snapshots confer no
//! delivery, recovery, liveness or authorization authority.

use fl_core::{at::At, ids::MessageId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::identity::{RouteEntry, VendorRoute, normalize_persona};

const MAX_FAILURE_STAGES: usize = 4;
const MAX_ROUTE_OBSERVATIONS: usize = 2;

/// Read-only evidence from this daemon instance. Restart loses this evidence;
/// that loss does not establish that retrying a delivery would be safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchDiagnostics {
    pub schema_version: u8,
    /// Observational 16-byte nonce in lowercase hex, or null if unavailable.
    pub instance_nonce: Option<String>,
    pub instance_started_at: At,
    pub scope: DiagnosticsScope,
    pub records: Vec<FenceDiagnostic>,
    /// The existing scheduler's global metadata saturation flag.
    pub global_capacity_exhausted: bool,
    /// Some attempted diagnostic retention was omitted, so this is not a
    /// complete list of all holds.
    pub diagnostics_partial: bool,
    pub omitted_evidence_total: u64,
    pub omitted_evidence_total_saturated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsScope {
    DaemonLifetime,
}

/// An observation of the existing suppression phase, not a scheduling state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppressionPhase {
    PrefixDraining,
    RetainedFence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStage {
    MessageLookupMissing,
    MessageLookupError,
    InitialReceiptReadError,
    TransportError,
    ReceiptWriteError,
    ReceiptWriteMissing,
    FinalReceiptReadError,
    FinalReceiptReadMissing,
    UnresolvedReceiptStatus,
    WorkerPanic,
}

/// All causes are observations; none establishes trusted nonmutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FenceReason {
    MessageLookupMissing,
    MessageLookupError,
    InitialReceiptReadError,
    TransportError,
    ReceiptWriteError,
    ReceiptWriteMissing,
    FinalReceiptReadError,
    FinalReceiptReadMissing,
    UnresolvedReceiptStatus,
    WorkerPanic,
    QueueOverflow,
    WorkerSpawnFailure,
    GlobalMetadataCap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchCallOutcome {
    Unknown,
    ReturnedOk,
    ReturnedError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAdapter {
    Claude,
    Agy,
    Codex,
    Muse,
    Grok,
    Webhook,
}

/// A safe projection of a snapshot actually passed to route dispatch. This
/// does not identify a child process, emission, live session or authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDispatchObservation {
    pub adapter: RouteAdapter,
    pub identity_digest: Option<String>,
    pub route_updated_at: Option<At>,
    pub explicit: bool,
    pub outcome: DispatchCallOutcome,
}

/// Generated rows contain at most four stages and two route observations,
/// validated recipient labels (64 bytes), and trigger IDs (256 bytes).
/// Unknown values remain null; an omitted oversize ID has a digest and flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceDiagnostic {
    pub recipient: Option<String>,
    pub recipient_digest: Option<String>,
    pub recipient_invalid: bool,
    pub trigger_message_id: Option<String>,
    pub trigger_message_id_digest: Option<String>,
    pub trigger_message_id_omitted: bool,
    pub hold_observed_at: At,
    pub phase: SuppressionPhase,
    pub first_reason: FenceReason,
    pub decisive_reason: FenceReason,
    pub failure_stages: Vec<FailureStage>,
    pub omitted_failure_stages: u64,
    pub omitted_failure_stages_saturated: bool,
    pub route_observations: Vec<RouteDispatchObservation>,
    pub omitted_route_observations: u64,
    pub omitted_route_observations_saturated: bool,
    /// Actual route-dispatch calls, independent of the observation storage cap.
    pub route_dispatch_calls: u64,
    pub route_dispatch_calls_saturated: bool,
    /// No registration generation exists in v1; always null.
    pub registration_generation: Option<u64>,
}

/// Only safe, bounded projections enter this report: never route clones,
/// arbitrary errors, panic payloads or message contents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DispatchReport {
    pub(crate) failure_stages: Vec<FailureStage>,
    pub(crate) omitted_failure_stages: u64,
    pub(crate) omitted_failure_stages_saturated: bool,
    pub(crate) route_observations: Vec<RouteDispatchObservation>,
    pub(crate) omitted_route_observations: u64,
    pub(crate) omitted_route_observations_saturated: bool,
    pub(crate) route_dispatch_calls: u64,
    pub(crate) route_dispatch_calls_saturated: bool,
}

impl DispatchReport {
    pub(crate) fn record_failure(&mut self, stage: FailureStage) {
        if self.failure_stages.len() < MAX_FAILURE_STAGES {
            self.failure_stages.push(stage);
        } else {
            // Preserve first, two earliest middle stages, and latest decisive stage.
            self.failure_stages[MAX_FAILURE_STAGES - 1] = stage;
            increment_saturating(
                &mut self.omitted_failure_stages,
                &mut self.omitted_failure_stages_saturated,
            );
        }
    }

    /// Call immediately before invocation, so an unwinding call remains an
    /// observed invocation with an explicitly unknown result.
    pub(crate) fn record_call_started(&mut self, entry: &RouteEntry) -> Option<usize> {
        increment_saturating(
            &mut self.route_dispatch_calls,
            &mut self.route_dispatch_calls_saturated,
        );
        if self.route_observations.len() == MAX_ROUTE_OBSERVATIONS {
            increment_saturating(
                &mut self.omitted_route_observations,
                &mut self.omitted_route_observations_saturated,
            );
            return None;
        }
        let slot = self.route_observations.len();
        self.route_observations.push(RouteDispatchObservation {
            adapter: match &entry.route {
                VendorRoute::Claude { .. } => RouteAdapter::Claude,
                VendorRoute::Agy { .. } => RouteAdapter::Agy,
                VendorRoute::Codex { .. } => RouteAdapter::Codex,
                VendorRoute::Muse { .. } => RouteAdapter::Muse,
                VendorRoute::Grok { .. } => RouteAdapter::Grok,
                VendorRoute::Webhook { .. } => RouteAdapter::Webhook,
            },
            identity_digest: entry.route.session_identity().map(|(adapter, identity)| {
                framed_digest(b"fl-wire-route-snapshot-v1\0", &[adapter, identity])
            }),
            route_updated_at: Some(entry.updated_at.clone()),
            explicit: entry.explicit,
            outcome: DispatchCallOutcome::Unknown,
        });
        Some(slot)
    }

    pub(crate) fn record_call_finished(
        &mut self,
        observation_slot: Option<usize>,
        outcome: DispatchCallOutcome,
    ) {
        if let Some(observation) =
            observation_slot.and_then(|slot| self.route_observations.get_mut(slot))
        {
            observation.outcome = outcome;
        }
    }
}

impl FenceDiagnostic {
    /// Prepare outside scheduler/fence locks. Delivery and suppression retain
    /// their original keys; only this observational projection normalizes them.
    pub(crate) fn from_report(
        recipient: &str,
        message_id: Option<&MessageId>,
        hold_observed_at: At,
        phase: SuppressionPhase,
        reason: FenceReason,
        report: &DispatchReport,
    ) -> Self {
        let safe_recipient = (recipient.trim().len() <= 64)
            .then(|| normalize_persona(recipient))
            .flatten();
        let recipient_invalid = safe_recipient.is_none();
        let recipient_digest =
            recipient_invalid.then(|| framed_digest(b"fl-wire-recipient-v1\0", &[recipient]));
        let trigger_message_id_omitted = message_id.is_some_and(|id| id.as_str().len() > 256);
        let trigger_message_id = message_id
            .filter(|_| !trigger_message_id_omitted)
            .map(|id| id.as_str().to_owned());
        let trigger_message_id_digest = message_id
            .filter(|_| trigger_message_id_omitted)
            .map(|id| framed_digest(b"fl-wire-message-id-v1\0", &[id.as_str()]));
        Self {
            recipient: safe_recipient,
            recipient_digest,
            recipient_invalid,
            trigger_message_id,
            trigger_message_id_digest,
            trigger_message_id_omitted,
            hold_observed_at,
            phase,
            first_reason: reason,
            decisive_reason: reason,
            failure_stages: report.failure_stages.clone(),
            omitted_failure_stages: report.omitted_failure_stages,
            omitted_failure_stages_saturated: report.omitted_failure_stages_saturated,
            route_observations: report.route_observations.clone(),
            omitted_route_observations: report.omitted_route_observations,
            omitted_route_observations_saturated: report.omitted_route_observations_saturated,
            route_dispatch_calls: report.route_dispatch_calls,
            route_dispatch_calls_saturated: report.route_dispatch_calls_saturated,
            registration_generation: None,
        }
    }
}

/// The flag means the exact count exceeded u64, rather than merely reaching it.
pub(crate) fn increment_saturating(value: &mut u64, saturated: &mut bool) {
    if *value == u64::MAX {
        *saturated = true;
    } else {
        *value += 1;
    }
}

fn framed_digest(domain: &[u8], values: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    for value in values {
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(value.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{RouteEntry, VendorRoute};
    use fl_core::{
        at::At,
        ids::{MessageId, seq_iri},
        iri::Iri,
    };

    fn entry(route: VendorRoute) -> RouteEntry {
        RouteEntry {
            persona: "ROUTE_PERSONA_SECRET".into(),
            route,
            updated_at: At::from_unix_millis(123),
            explicit: true,
        }
    }

    fn codex() -> RouteEntry {
        entry(VendorRoute::Codex {
            thread_id: "SESSION_SECRET".into(),
            cwd: Some("/CWD_SECRET".into()),
        })
    }

    fn row(report: &DispatchReport) -> FenceDiagnostic {
        FenceDiagnostic::from_report(
            " Freya ",
            Some(&MessageId(seq_iri(1))),
            At::from_unix_millis(456),
            SuppressionPhase::RetainedFence,
            FenceReason::UnresolvedReceiptStatus,
            report,
        )
    }

    #[test]
    fn diagnostic_bounds_preserve_first_and_decisive_failure() {
        let mut report = DispatchReport::default();
        for stage in [
            FailureStage::MessageLookupError,
            FailureStage::InitialReceiptReadError,
            FailureStage::TransportError,
            FailureStage::ReceiptWriteError,
            FailureStage::FinalReceiptReadMissing,
            FailureStage::WorkerPanic,
        ] {
            report.record_failure(stage);
        }
        let row = row(&report);
        assert_eq!(
            row.failure_stages,
            vec![
                FailureStage::MessageLookupError,
                FailureStage::InitialReceiptReadError,
                FailureStage::TransportError,
                FailureStage::WorkerPanic,
            ]
        );
        assert_eq!(row.omitted_failure_stages, 2);
        assert!(!row.omitted_failure_stages_saturated);
    }

    #[test]
    fn diagnostic_calls_outlive_observation_cap() {
        let mut report = DispatchReport::default();
        for outcome in [
            DispatchCallOutcome::ReturnedOk,
            DispatchCallOutcome::ReturnedError,
            DispatchCallOutcome::ReturnedOk,
            DispatchCallOutcome::ReturnedError,
            DispatchCallOutcome::ReturnedOk,
        ] {
            let slot = report.record_call_started(&codex());
            report.record_call_finished(slot, outcome);
        }
        let row = row(&report);
        assert_eq!(row.route_dispatch_calls, 5);
        assert_eq!(row.route_observations.len(), 2);
        assert_eq!(row.omitted_route_observations, 3);
        assert!(!row.route_dispatch_calls_saturated);
        assert!(!row.omitted_route_observations_saturated);
        assert_eq!(
            row.route_observations[0].outcome,
            DispatchCallOutcome::ReturnedOk
        );
        assert_eq!(
            row.route_observations[1].outcome,
            DispatchCallOutcome::ReturnedError
        );

        let mut interrupted = DispatchReport::default();
        interrupted.record_call_started(&codex());
        let row = self::row(&interrupted);
        assert_eq!(row.route_dispatch_calls, 1);
        assert_eq!(
            row.route_observations[0].outcome,
            DispatchCallOutcome::Unknown
        );
        assert_eq!(
            self::row(&DispatchReport::default()).route_dispatch_calls,
            0
        );
    }

    #[test]
    fn diagnostic_counters_saturate_independently() {
        let mut report = DispatchReport::default();
        for _ in 0..4 {
            report.record_failure(FailureStage::TransportError);
        }
        report.omitted_failure_stages = u64::MAX - 1;
        report.route_dispatch_calls = u64::MAX - 1;
        report.record_call_started(&codex());
        report.record_call_started(&codex());
        report.omitted_route_observations = u64::MAX - 1;
        report.record_call_started(&codex());
        let at_max = row(&report);
        assert_eq!(at_max.route_dispatch_calls, u64::MAX);
        assert!(at_max.route_dispatch_calls_saturated);
        assert_eq!(at_max.omitted_route_observations, u64::MAX);
        assert!(!at_max.omitted_route_observations_saturated);
        report.record_call_started(&codex());
        report.record_failure(FailureStage::ReceiptWriteError);
        assert!(!row(&report).omitted_failure_stages_saturated);
        report.record_failure(FailureStage::WorkerPanic);
        let saturated = row(&report);
        assert_eq!(saturated.omitted_failure_stages, u64::MAX);
        assert!(saturated.omitted_failure_stages_saturated);
        assert!(saturated.omitted_route_observations_saturated);
    }

    #[test]
    fn diagnostic_projection_excludes_route_secrets() {
        let routes = [
            VendorRoute::Claude {
                pid: 42,
                session_id: "SESSION_SECRET".into(),
                socket_path: "/SOCKET_SECRET".into(),
                key_path: Some("/KEY_SECRET".into()),
            },
            VendorRoute::Agy {
                conversation_id: "SESSION_SECRET".into(),
                ls_pid: Some(43),
                ls_address: Some("ADDRESS_SECRET".into()),
                csrf_token: Some("CSRF_SECRET".into()),
                agentapi_exe: Some("/EXE_SECRET".into()),
            },
            codex().route,
            VendorRoute::Muse {
                session_id: "SESSION_SECRET".into(),
                cwd: Some("/CWD_SECRET".into()),
                tmux_target: Some("TARGET_SECRET".into()),
                tmux_pane: Some("PANE_SECRET".into()),
            },
            VendorRoute::Grok {
                session_id: "SESSION_SECRET".into(),
                cwd: Some("/CWD_SECRET".into()),
                tmux_target: Some("TARGET_SECRET".into()),
                tmux_pane: Some("PANE_SECRET".into()),
            },
            VendorRoute::Webhook {
                socket_path: "/SOCKET_SECRET".into(),
            },
        ];
        let oversized = MessageId(Iri::parse(&format!("x:{}", "ID_SECRET".repeat(64))).unwrap());
        for route in routes {
            let webhook = matches!(route, VendorRoute::Webhook { .. });
            let mut report = DispatchReport::default();
            let slot = report.record_call_started(&entry(route));
            report.record_call_finished(slot, DispatchCallOutcome::ReturnedError);
            let projected = FenceDiagnostic::from_report(
                "INVALID_NAME_SECRET/\n",
                Some(&oversized),
                At::from_unix_millis(456),
                SuppressionPhase::PrefixDraining,
                FenceReason::QueueOverflow,
                &report,
            );
            let json = serde_json::to_value(&projected).unwrap();
            let text = serde_json::to_string(&projected).unwrap();
            assert!(!text.contains("SECRET"), "{text}");
            assert!(projected.recipient.is_none());
            assert!(projected.recipient_invalid);
            assert_eq!(projected.recipient_digest.as_ref().unwrap().len(), 64);
            assert!(projected.trigger_message_id.is_none());
            assert!(projected.trigger_message_id_omitted);
            assert_eq!(
                projected.trigger_message_id_digest.as_ref().unwrap().len(),
                64
            );
            assert!(json["registration_generation"].is_null());
            assert_eq!(
                projected.route_observations[0].identity_digest.is_none(),
                webhook
            );
            assert_eq!(
                projected.route_observations[0].route_updated_at,
                Some(At::from_unix_millis(123))
            );
            assert!(projected.route_observations[0].explicit);
            assert!(text.len() <= 4096);
        }
        let valid = row(&DispatchReport::default());
        assert_eq!(valid.recipient.as_deref(), Some("freya"));
        assert!(!valid.recipient_invalid);
        assert!(valid.recipient_digest.is_none());
        assert_eq!(
            valid.trigger_message_id.as_deref(),
            Some("urn:uuid:00000000-0000-7000-8000-000000000001")
        );
        assert!(!valid.trigger_message_id_omitted);
        assert!(valid.trigger_message_id_digest.is_none());
        let unknown = FenceDiagnostic::from_report(
            "freya",
            None,
            At::from_unix_millis(0),
            SuppressionPhase::RetainedFence,
            FenceReason::WorkerSpawnFailure,
            &DispatchReport::default(),
        );
        assert!(unknown.trigger_message_id.is_none());
        assert!(unknown.trigger_message_id_digest.is_none());
        assert!(!unknown.trigger_message_id_omitted);
        assert!(unknown.route_observations.is_empty());
    }

    #[test]
    fn diagnostic_maximal_row_fits_byte_bound() {
        let mut report = DispatchReport::default();
        for _ in 0..5 {
            report.record_failure(FailureStage::InitialReceiptReadError);
        }
        for _ in 0..3 {
            report.record_call_started(&codex());
        }
        report.omitted_failure_stages = u64::MAX;
        report.omitted_failure_stages_saturated = true;
        report.omitted_route_observations = u64::MAX;
        report.omitted_route_observations_saturated = true;
        report.route_dispatch_calls = u64::MAX;
        report.route_dispatch_calls_saturated = true;
        // Iri accepts non-whitespace controls; JSON expands each NUL into six bytes.
        let id = MessageId(Iri::parse(&format!("x:{}", "\0".repeat(254))).unwrap());
        let row = FenceDiagnostic::from_report(
            &"f".repeat(64),
            Some(&id),
            At::from_unix_millis(u64::MAX),
            SuppressionPhase::RetainedFence,
            FenceReason::InitialReceiptReadError,
            &report,
        );
        assert_eq!(row.trigger_message_id.as_ref().unwrap().len(), 256);
        assert!(serde_json::to_vec(&row).unwrap().len() <= 4096);
        let invalid = FenceDiagnostic::from_report(
            &"f".repeat(65),
            Some(&MessageId(
                Iri::parse(&format!("x:{}", "z".repeat(255))).unwrap(),
            )),
            At::from_unix_millis(u64::MAX),
            SuppressionPhase::RetainedFence,
            FenceReason::GlobalMetadataCap,
            &report,
        );
        assert!(invalid.recipient_invalid);
        assert!(invalid.trigger_message_id_omitted);
        assert!(serde_json::to_vec(&invalid).unwrap().len() <= 4096);
    }

    #[test]
    fn diagnostic_identity_is_framed_and_absent_sessions_stay_unknown() {
        let mut report = DispatchReport::default();
        report.record_call_started(&codex());
        let digest = row(&report).route_observations[0]
            .identity_digest
            .clone()
            .unwrap();
        // Independently calculated SHA256 of domain + LE64(5) + codex + LE64(14) + SESSION_SECRET.
        assert_eq!(
            digest,
            "360b17578089e8b93ef28ad85062e6590dd5f56ff2f96563985a4d21f9a1f746"
        );
        for route in [
            VendorRoute::Codex {
                thread_id: " \t ".into(),
                cwd: Some("/PRIVATE".into()),
            },
            VendorRoute::Webhook {
                socket_path: "/PRIVATE".into(),
            },
        ] {
            let mut report = DispatchReport::default();
            report.record_call_started(&entry(route));
            assert!(row(&report).route_observations[0].identity_digest.is_none());
        }
        assert_ne!(
            framed_digest(b"fl-wire-recipient-v1\0", &["same"]),
            framed_digest(b"fl-wire-message-id-v1\0", &["same"])
        );
        assert_ne!(
            framed_digest(b"fl-wire-route-snapshot-v1\0", &["ab", "c"]),
            framed_digest(b"fl-wire-route-snapshot-v1\0", &["a", "bc"])
        );
    }

    #[test]
    fn diagnostic_snapshot_serde_preserves_explicit_unknowns() {
        let snapshot = DispatchDiagnostics {
            schema_version: 1,
            instance_nonce: None,
            instance_started_at: At::from_unix_millis(0),
            scope: DiagnosticsScope::DaemonLifetime,
            records: vec![row(&DispatchReport::default())],
            global_capacity_exhausted: false,
            diagnostics_partial: false,
            omitted_evidence_total: 0,
            omitted_evidence_total_saturated: false,
        };
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["scope"], "daemon_lifetime");
        assert!(json["instance_nonce"].is_null());
        assert_eq!(json["records"][0]["phase"], "retained_fence");
        assert_eq!(
            json["records"][0]["first_reason"],
            "unresolved_receipt_status"
        );
        assert_eq!(
            serde_json::from_value::<DispatchDiagnostics>(json).unwrap(),
            snapshot
        );
    }
}
