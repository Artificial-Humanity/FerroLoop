//! Bounded, volatile observations of dispatch fences. These snapshots confer no
//! delivery, recovery, liveness or authorization authority.

use fl_core::{at::At, ids::MessageId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::identity::{RouteEntry, VendorRoute, normalize_persona};

const MAX_FAILURE_STAGES: usize = 4;
const MAX_ROUTE_OBSERVATIONS: usize = 2;
pub const MAX_REGISTRATION_DIAGNOSTICS: usize = 64;

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
    DeadExplicitRoute,
    ComposerOccupiedByNonWireInput,
    ReceiptWriteError,
    ReceiptWriteMissing,
    FinalReceiptReadError,
    FinalReceiptReadMissing,
    UnresolvedReceiptStatus,
    WorkerPanic,
    #[serde(other)]
    Unsupported,
}

/// All causes are observations; none establishes trusted nonmutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FenceReason {
    MessageLookupMissing,
    MessageLookupError,
    InitialReceiptReadError,
    TransportError,
    DeadExplicitRoute,
    ComposerOccupiedByNonWireInput,
    ReceiptWriteError,
    ReceiptWriteMissing,
    FinalReceiptReadError,
    FinalReceiptReadMissing,
    UnresolvedReceiptStatus,
    WorkerPanic,
    QueueOverflow,
    WorkerSpawnFailure,
    GlobalMetadataCap,
    #[serde(other)]
    Unsupported,
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
    HostCli,
    Webhook,
}

impl RouteAdapter {
    pub fn from_route(route: &VendorRoute) -> Self {
        match route {
            VendorRoute::Claude { .. } => Self::Claude,
            VendorRoute::Agy { .. } => Self::Agy,
            VendorRoute::Codex { .. } => Self::Codex,
            VendorRoute::Muse { .. } => Self::Muse,
            VendorRoute::Grok { .. } => Self::Grok,
            VendorRoute::HostCli { .. } => Self::HostCli,
            VendorRoute::Webhook { .. } => Self::Webhook,
        }
    }

    pub fn from_vendor_str(vendor: &str) -> Self {
        match vendor.trim().to_ascii_lowercase().as_str() {
            "claude" => Self::Claude,
            "agy" | "antigravity" => Self::Agy,
            "codex" => Self::Codex,
            "muse" => Self::Muse,
            "grok" => Self::Grok,
            "webhook" => Self::Webhook,
            _ => Self::HostCli,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationDiagnosticOutcome {
    Refused,
    Replaced,
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationConflictReason {
    ForeignHostedEnv,
    VendorMismatch,
    HostedIdentityMismatch,
    HydrationGenerationMismatch,
    RegistrationContention,
    #[serde(other)]
    Unsupported,
}

fn default_one() -> u64 {
    1
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Bounded, secret-free observation of a refused or replaced explicit route registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRegistrationDiagnostic {
    pub at: At,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<At>,
    #[serde(default = "default_one")]
    pub count: u64,
    pub persona: String,
    pub outcome: RegistrationDiagnosticOutcome,
    pub reason: RegistrationConflictReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incumbent_adapter: Option<RouteAdapter>,
    pub challenger_adapter: RouteAdapter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incumbent_hosted_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenger_hosted_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incumbent_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenger_generation: Option<String>,
    pub replace: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unverified_process: bool,
}

pub fn sanitize_generation_token(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return None;
    }
    if trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

impl RouteRegistrationDiagnostic {
    pub fn sanitized(mut self) -> Self {
        if self.count == 0 {
            self.count = 1;
        }
        self.persona =
            normalize_persona(&self.persona).unwrap_or_else(|| "invalid-persona".to_string());
        self.incumbent_hosted_id = self
            .incumbent_hosted_id
            .as_deref()
            .and_then(|raw| crate::identity::parse_hosted_identity(raw).ok())
            .map(|(v, p)| format!("{v}:{p}"));
        self.challenger_hosted_id = self
            .challenger_hosted_id
            .as_deref()
            .and_then(|raw| crate::identity::parse_hosted_identity(raw).ok())
            .map(|(v, p)| format!("{v}:{p}"));
        self.incumbent_generation = sanitize_generation_token(self.incumbent_generation.as_deref());
        self.challenger_generation =
            sanitize_generation_token(self.challenger_generation.as_deref());
        self
    }

    pub fn same_dedup_key(&self, other: &Self) -> bool {
        self.persona == other.persona
            && self.outcome == other.outcome
            && self.reason == other.reason
            && self.incumbent_adapter == other.incumbent_adapter
            && self.challenger_adapter == other.challenger_adapter
            && self.incumbent_hosted_id == other.incumbent_hosted_id
            && self.challenger_hosted_id == other.challenger_hosted_id
            && self.incumbent_generation == other.incumbent_generation
            && self.challenger_generation == other.challenger_generation
            && self.replace == other.replace
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paste_settle_ms: Option<u64>,
}

thread_local! {
    static LAST_PASTE_SETTLE_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

pub(crate) fn record_last_paste_settle_ms(ms: u64) {
    LAST_PASTE_SETTLE_MS.with(|cell| cell.set(Some(ms)));
}

pub(crate) fn take_last_paste_settle_ms() -> Option<u64> {
    LAST_PASTE_SETTLE_MS.with(|cell| cell.take())
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
        let _ = take_last_paste_settle_ms();
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
            adapter: RouteAdapter::from_route(&entry.route),
            identity_digest: entry.route.session_identity().map(|(adapter, identity)| {
                framed_digest(b"fl-wire-route-snapshot-v1\0", &[adapter, identity])
            }),
            route_updated_at: Some(entry.updated_at.clone()),
            explicit: entry.explicit,
            outcome: DispatchCallOutcome::Unknown,
            paste_settle_ms: None,
        });
        Some(slot)
    }

    pub(crate) fn record_call_finished(
        &mut self,
        observation_slot: Option<usize>,
        outcome: DispatchCallOutcome,
    ) {
        let paste_settle_ms = take_last_paste_settle_ms();
        if let Some(observation) =
            observation_slot.and_then(|slot| self.route_observations.get_mut(slot))
        {
            observation.outcome = outcome;
            observation.paste_settle_ms = paste_settle_ms;
        }
    }
}

impl FenceDiagnostic {
    /// Prepare outside scheduler/fence locks. Delivery and suppression retain
    /// their original keys; this projection preserves the scheduler's ASCII
    /// case-folding without merging distinct whitespace-bearing keys.
    pub(crate) fn from_report(
        recipient: &str,
        message_id: Option<&MessageId>,
        hold_observed_at: At,
        phase: SuppressionPhase,
        reason: FenceReason,
        report: &DispatchReport,
    ) -> Self {
        let safe_recipient = (recipient.len() <= 64 && recipient.trim() == recipient)
            .then(|| normalize_persona(recipient))
            .flatten();
        let recipient_invalid = safe_recipient.is_none();
        let recipient_digest = recipient_invalid.then(|| {
            // Imported names need not be valid slugs. Match exactly the
            // scheduler's key equivalence, without retaining raw names.
            framed_digest(
                b"fl-wire-recipient-v1\0",
                &[&recipient.to_ascii_lowercase()],
            )
        });
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

/// Bounded safe DTOs only; recipient matching uses their safe label/digest rather
/// than retaining a second copy of scheduler keys.
pub(crate) struct DiagnosticsState {
    pub(crate) snapshot: DispatchDiagnostics,
}

impl DiagnosticsState {
    pub(crate) fn new(started_at: At, nonce: Option<String>) -> Self {
        Self {
            snapshot: DispatchDiagnostics {
                schema_version: 1,
                instance_nonce: nonce,
                instance_started_at: started_at,
                scope: DiagnosticsScope::DaemonLifetime,
                records: Vec::new(),
                global_capacity_exhausted: false,
                diagnostics_partial: false,
                omitted_evidence_total: 0,
                omitted_evidence_total_saturated: false,
            },
        }
    }

    /// Called with the diagnostics lock paired with the authoritative marker.
    /// Return only the first retained hold, for logging after all guards drop.
    pub(crate) fn retain(&mut self, row: FenceDiagnostic) -> Option<FenceDiagnostic> {
        if let Some(first) = self.snapshot.records.iter_mut().find(|first| {
            first.recipient == row.recipient && first.recipient_digest == row.recipient_digest
        }) {
            if first.phase == SuppressionPhase::PrefixDraining
                && row.phase == SuppressionPhase::RetainedFence
            {
                first.phase = SuppressionPhase::RetainedFence;
                first.decisive_reason = row.decisive_reason;
                let mut report = DispatchReport {
                    failure_stages: first.failure_stages.clone(),
                    omitted_failure_stages: first.omitted_failure_stages,
                    omitted_failure_stages_saturated: first.omitted_failure_stages_saturated,
                    ..DispatchReport::default()
                };
                for stage in row.failure_stages {
                    report.record_failure(stage);
                }
                first.failure_stages = report.failure_stages;
                first.omitted_failure_stages = report.omitted_failure_stages;
                first.omitted_failure_stages_saturated = report.omitted_failure_stages_saturated;
                // Prefix marking has no dispatch report. The decisive worker's
                // bounded report is appended on promotion, never a later ACK.
                first.route_observations = row.route_observations;
                first.omitted_route_observations = row.omitted_route_observations;
                first.omitted_route_observations_saturated =
                    row.omitted_route_observations_saturated;
                first.route_dispatch_calls = row.route_dispatch_calls;
                first.route_dispatch_calls_saturated = row.route_dispatch_calls_saturated;
                if row.omitted_failure_stages > 0 {
                    let (total, overflow) = first
                        .omitted_failure_stages
                        .overflowing_add(row.omitted_failure_stages);
                    first.omitted_failure_stages = if overflow { u64::MAX } else { total };
                    first.omitted_failure_stages_saturated |= overflow;
                }
                first.omitted_failure_stages_saturated |= row.omitted_failure_stages_saturated;
            }
            return None;
        }
        if self.snapshot.records.len() == 128 {
            self.snapshot.diagnostics_partial = true;
            increment_saturating(
                &mut self.snapshot.omitted_evidence_total,
                &mut self.snapshot.omitted_evidence_total_saturated,
            );
            return None;
        }
        self.snapshot.records.push(row.clone());
        Some(row)
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
            identity_at: None,
            explicit: true,
            hosted_id: None,
            hydration_generation: None,
            pid_start_time: None,
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
            "Freya",
            Some(&MessageId(seq_iri(1))),
            At::from_unix_millis(456),
            SuppressionPhase::RetainedFence,
            FenceReason::UnresolvedReceiptStatus,
            report,
        )
    }

    #[test]
    fn diagnostic_distinct_scheduler_keys_keep_independent_prefix_evidence() {
        let mut state = DiagnosticsState::new(At::from_unix_millis(1), None);
        let names = ["Alice", " alice ", "\talice\t"];
        for (n, name) in names.iter().enumerate() {
            let first = FenceDiagnostic::from_report(
                name,
                Some(&MessageId(seq_iri(n as u64 + 10))),
                At::from_unix_millis(n as u64 + 100),
                SuppressionPhase::PrefixDraining,
                FenceReason::QueueOverflow,
                &DispatchReport::default(),
            );
            assert!(state.retain(first).is_some(), "distinct key: {name:?}");
        }
        assert_eq!(state.snapshot.records.len(), 3);
        let reasons = [
            FenceReason::WorkerPanic,
            FenceReason::WorkerSpawnFailure,
            FenceReason::UnresolvedReceiptStatus,
        ];
        for n in (0..names.len()).rev() {
            let later = FenceDiagnostic::from_report(
                names[n],
                Some(&MessageId(seq_iri(99))),
                At::from_unix_millis(999),
                SuppressionPhase::RetainedFence,
                reasons[n],
                &DispatchReport::default(),
            );
            assert!(state.retain(later).is_none());
            for (index, kept) in state.snapshot.records.iter().enumerate() {
                assert_eq!(
                    kept.trigger_message_id.as_deref(),
                    Some(seq_iri(index as u64 + 10).as_str())
                );
                assert_eq!(
                    kept.hold_observed_at,
                    At::from_unix_millis(index as u64 + 100)
                );
                assert_eq!(kept.first_reason, FenceReason::QueueOverflow);
                assert_eq!(
                    kept.phase,
                    if index >= n {
                        SuppressionPhase::RetainedFence
                    } else {
                        SuppressionPhase::PrefixDraining
                    }
                );
                assert_eq!(
                    kept.decisive_reason,
                    if index >= n {
                        reasons[index]
                    } else {
                        FenceReason::QueueOverflow
                    }
                );
            }
        }
    }

    #[test]
    fn diagnostic_recipient_identity_matches_scheduler_casefold_without_trimming() {
        // Independently calculated SHA256(domain || LE64(length) || case-folded key).
        for (name, case_variant, digest) in [
            ("Alice", "aLiCe", None),
            (
                " ALICE ",
                " alice ",
                Some("b71cb4eb2d70d79d3b12dc7446cc4da923797942e348bf3556376fdb1fcf9589"),
            ),
            (
                "BAD/NAME",
                "bad/name",
                Some("43ee3c3a2b38f8e783b8e6a2ff0d1687ee8d75ea059c1e25a8a7d3f4c2054b66"),
            ),
        ] {
            let project = |name| {
                FenceDiagnostic::from_report(
                    name,
                    None,
                    At::from_unix_millis(1),
                    SuppressionPhase::RetainedFence,
                    FenceReason::WorkerSpawnFailure,
                    &DispatchReport::default(),
                )
            };
            let first = project(name);
            assert_eq!(first, project(case_variant));
            assert_eq!(first.recipient_invalid, digest.is_some());
            assert_eq!(
                first.recipient.as_deref(),
                digest.is_none().then_some("alice")
            );
            assert_eq!(first.recipient_digest.as_deref(), digest);
        }
    }

    #[test]
    fn diagnostic_retention_promotes_without_replacing_first_hold() {
        let mut state = DiagnosticsState::new(At::from_unix_millis(1), None);
        let mut first = row(&DispatchReport::default());
        first.phase = SuppressionPhase::PrefixDraining;
        first.first_reason = FenceReason::QueueOverflow;
        first.decisive_reason = FenceReason::QueueOverflow;
        assert!(state.retain(first.clone()).is_some());
        let mut report = DispatchReport::default();
        report.record_call_started(&codex());
        report.record_failure(FailureStage::WorkerPanic);
        let later = FenceDiagnostic::from_report(
            "freya",
            None,
            At::from_unix_millis(999),
            SuppressionPhase::RetainedFence,
            FenceReason::WorkerPanic,
            &report,
        );
        assert!(state.retain(later).is_none());
        let kept = &state.snapshot.records[0];
        assert_eq!(kept.trigger_message_id, first.trigger_message_id);
        assert_eq!(kept.hold_observed_at, At::from_unix_millis(456));
        assert_eq!(kept.first_reason, FenceReason::QueueOverflow);
        assert_eq!(kept.decisive_reason, FenceReason::WorkerPanic);
        assert_eq!(kept.phase, SuppressionPhase::RetainedFence);
        assert_eq!(kept.failure_stages, vec![FailureStage::WorkerPanic]);
        assert_eq!(kept.route_dispatch_calls, 1);
        // A later ACK, route observation or duplicate retention cannot rewrite a fence.
        assert!(state.retain(first).is_none());
        assert_eq!(
            state.snapshot.records[0].decisive_reason,
            FenceReason::WorkerPanic
        );
    }

    #[test]
    fn diagnostic_prefix_promotion_keeps_bounded_repeated_failure_observations() {
        let mut state = DiagnosticsState::new(At::from_unix_millis(1), None);
        let mut first = row(&DispatchReport::default());
        first.phase = SuppressionPhase::PrefixDraining;
        first.first_reason = FenceReason::QueueOverflow;
        first.decisive_reason = FenceReason::QueueOverflow;
        state.retain(first);
        let mut report = DispatchReport::default();
        for stage in [
            FailureStage::TransportError,
            FailureStage::TransportError,
            FailureStage::ReceiptWriteError,
            FailureStage::FinalReceiptReadError,
            FailureStage::WorkerPanic,
        ] {
            report.record_failure(stage);
        }
        let mut later = row(&report);
        later.decisive_reason = FenceReason::WorkerPanic;
        state.retain(later);
        assert_eq!(
            state.snapshot.records[0].failure_stages,
            vec![
                FailureStage::TransportError,
                FailureStage::TransportError,
                FailureStage::ReceiptWriteError,
                FailureStage::WorkerPanic
            ]
        );
        assert_eq!(state.snapshot.records[0].omitted_failure_stages, 1);
    }

    #[test]
    fn diagnostic_retention_budget_counts_omitted_admissions_without_global_saturation() {
        let mut state = DiagnosticsState::new(At::from_unix_millis(1), None);
        for n in 0..128 {
            let mut entry = row(&DispatchReport::default());
            entry.recipient = Some(format!("recipient-{n}"));
            assert!(state.retain(entry).is_some());
        }
        for _ in 0..2 {
            assert!(state.retain(row(&DispatchReport::default())).is_none());
        }
        assert_eq!(state.snapshot.records.len(), 128);
        assert!(state.snapshot.diagnostics_partial);
        assert_eq!(state.snapshot.omitted_evidence_total, 2);
        assert!(!state.snapshot.global_capacity_exhausted);
        state.snapshot.omitted_evidence_total = u64::MAX;
        state.retain(row(&DispatchReport::default()));
        assert!(state.snapshot.omitted_evidence_total_saturated);
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
            VendorRoute::HostCli {
                vendor: "lemonade".into(),
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
            report.record_failure(FailureStage::ComposerOccupiedByNonWireInput);
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
            FenceReason::ComposerOccupiedByNonWireInput,
            &report,
        );
        assert_eq!(row.trigger_message_id.as_ref().unwrap().len(), 256);
        assert!(serde_json::to_vec(&row).unwrap().len() <= 4096);
        assert_eq!(
            serde_json::to_value(FailureStage::ComposerOccupiedByNonWireInput).unwrap(),
            "composer_occupied_by_non_wire_input"
        );
        assert_eq!(
            serde_json::to_value(FenceReason::ComposerOccupiedByNonWireInput).unwrap(),
            "composer_occupied_by_non_wire_input"
        );
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

        let future_stage: FailureStage =
            serde_json::from_str(r#""future_stage_v2""#).expect("unknown stage deserializes");
        assert_eq!(future_stage, FailureStage::Unsupported);
        let future_reason: FenceReason =
            serde_json::from_str(r#""future_reason_v2""#).expect("unknown reason deserializes");
        assert_eq!(future_reason, FenceReason::Unsupported);
    }
}
