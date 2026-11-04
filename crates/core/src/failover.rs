//! Bounded failover for Owner-approved endpoint sets (Record #98 §3).
//!
//! Fail-closed rules:
//! - No approved endpoint set means a single endpoint and no failover.
//! - Only `NoExecutionProven` (a transport witness that zero request bytes
//!   were written on a fresh connection, or a pre-dispatch refusal) may move
//!   to another endpoint.
//! - `Executed` (including a contract failure after execution) and
//!   `UnknownAfterSend` are terminal for the operation. They never replay.
//! - `ReusedConnectionReset` is deliberately *not* a [`ZeroBytesWitness`]
//!   variant and can never be one at the type level: on a reused keep-alive
//!   connection a reset can follow written bytes, so it classifies as
//!   `UnknownAfterSend`.

use crate::governor::SideEffectClass;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FailoverError {
    #[error("endpoint set is not Owner-approved")]
    UnapprovedEndpointSet,
    #[error("endpoint set must declare at least one endpoint")]
    NoEndpoints,
    #[error("endpoint at index {0} is empty")]
    EmptyEndpoint(usize),
    #[error("duplicate endpoint: {0}")]
    DuplicateEndpoint(String),
    #[error("IdempotentWithKey requires a named idempotency conformance test")]
    MissingConformanceTest,
    #[error("max_attempts must be at least 1")]
    MaxAttemptsZero,
    #[error("max_attempts {max_attempts} exceeds endpoint set size {set_size}")]
    MaxAttemptsExceedsSetSize { max_attempts: u32, set_size: usize },
    #[error("worst-case reservation overflow")]
    ReservationOverflow,
    #[error("finish called before begin for attempt {attempt_index} of {logical_operation_id}")]
    FinishBeforeBegin {
        logical_operation_id: String,
        attempt_index: u32,
    },
    #[error("duplicate begin for attempt {attempt_index} of {logical_operation_id}")]
    DuplicateBegin {
        logical_operation_id: String,
        attempt_index: u32,
    },
    #[error("attempt {attempt_index} of {logical_operation_id} is already finished")]
    AttemptAlreadyFinished {
        logical_operation_id: String,
        attempt_index: u32,
    },
    #[error("missing or empty idempotency key")]
    MissingIdempotencyKey,
    #[error("endpoint at index {0} is not an absolute http(s) URL with a host: {1}")]
    InvalidEndpoint(usize, String),
    #[error("failover journal unavailable or corrupt")]
    JournalUnavailable,
    #[error("uncertain prior attempt for logical operation {0}: dispatching without finish")]
    UncertainPriorAttempt(String),
}

fn sha256_hex(bytes: &[u8]) -> String {
    // SHA-256 (same digest as ring::SHA256; fl-core hashes with sha2).
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Canonical JSON: object keys sorted recursively, for digest inputs.
pub fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canonical_json(&map[*k])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        serde_json::Value::Array(items) => {
            format!(
                "[{}]",
                items
                    .iter()
                    .map(canonical_json)
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".into()),
    }
}

pub fn is_absolute_http_url(ep: &str) -> bool {
    let rest = ep
        .strip_prefix("http://")
        .or_else(|| ep.strip_prefix("https://"));
    match rest {
        Some(r) => {
            let authority = r.split(['/', '?', '#']).next().unwrap_or("");
            let host = authority
                .rsplit('@')
                .next()
                .unwrap_or("")
                .split(':')
                .next()
                .unwrap_or("");
            !host.is_empty()
        }
        None => false,
    }
}

/// One connector tool's Owner-approved endpoint set (§3.1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EndpointSet {
    pub connector_id: String,
    pub tool: String,
    /// Canonical endpoint strings, in policy order.
    pub endpoints: Vec<String>,
    pub side_effect_class: SideEffectClass,
    pub idempotency_conformance_test: Option<String>,
    /// Advisory only, kept for serde compatibility. It is IGNORED for
    /// authorization: failover is authorized solely by the computed
    /// [`EndpointSet::computed_policy_digest`] being present in
    /// `GateGovernor::approved_policy_digests`.
    pub owner_approved: bool,
    /// Advisory/stored value, kept for serde compatibility. Never trusted:
    /// authorization and fingerprints use [`EndpointSet::computed_policy_digest`].
    pub policy_digest: String,
}

impl EndpointSet {
    /// Computed (never manifest-trusted) policy digest: ring SHA-256 over
    /// the canonical serialization of connector/tool/endpoints/class/
    /// conformance-test name.
    pub fn computed_policy_digest(&self) -> String {
        let canonical = format!(
            "endpoint-set/v1\nconnector={}\ntool={}\nendpoints={}\nclass={:?}\nconformance={}",
            self.connector_id,
            self.tool,
            self.endpoints.join("\n"),
            self.side_effect_class,
            self.idempotency_conformance_test.as_deref().unwrap_or("")
        );
        sha256_hex(canonical.as_bytes())
    }

    pub fn validate(&self) -> Result<(), FailoverError> {
        if self.endpoints.is_empty() {
            return Err(FailoverError::NoEndpoints);
        }
        for (i, ep) in self.endpoints.iter().enumerate() {
            if ep.trim().is_empty() {
                return Err(FailoverError::EmptyEndpoint(i));
            }
            if !is_absolute_http_url(ep) {
                return Err(FailoverError::InvalidEndpoint(i, ep.clone()));
            }
            if self.endpoints[..i].contains(ep) {
                return Err(FailoverError::DuplicateEndpoint(ep.clone()));
            }
        }
        if self.side_effect_class == SideEffectClass::IdempotentWithKey {
            match &self.idempotency_conformance_test {
                Some(name) if !name.trim().is_empty() => {}
                _ => return Err(FailoverError::MissingConformanceTest),
            }
        }
        Ok(())
    }

    /// Constructor for a declared set. `owner_approved` and `policy_digest`
    /// arguments are advisory/ignored for authorization (kept for call-site
    /// compatibility); the stored digest is replaced by the computed one.
    /// Returns `None` when no endpoints are declared or the set is invalid.
    #[allow(clippy::too_many_arguments)]
    pub fn declared(
        connector_id: &str,
        tool: &str,
        endpoints: Option<Vec<String>>,
        side_effect_class: SideEffectClass,
        idempotency_conformance_test: Option<String>,
        owner_approved: bool,
        _policy_digest: &str,
    ) -> Option<Self> {
        let mut set = Self {
            connector_id: connector_id.to_string(),
            tool: tool.to_string(),
            endpoints: endpoints?,
            side_effect_class,
            idempotency_conformance_test,
            owner_approved,
            policy_digest: String::new(),
        };
        set.policy_digest = set.computed_policy_digest();
        set.validate().ok()?;
        Some(set)
    }
}

/// Helper mirroring manifest resolution: `None` when no approved endpoint
/// set is declared for the tool. `side_effect_class: None` is the undeclared
/// case and defaults to `SideEffectful`.
#[allow(clippy::too_many_arguments)]
pub fn effective_endpoint_set(
    connector_id: &str,
    tool: &str,
    endpoints: Option<Vec<String>>,
    side_effect_class: Option<SideEffectClass>,
    idempotency_conformance_test: Option<String>,
    owner_approved: bool,
    policy_digest: &str,
) -> Option<EndpointSet> {
    EndpointSet::declared(
        connector_id,
        tool,
        endpoints,
        side_effect_class.unwrap_or(SideEffectClass::SideEffectful),
        idempotency_conformance_test,
        owner_approved,
        policy_digest,
    )
}

/// Witness that zero request bytes were written. There is intentionally no
/// variant for a reused-connection reset: that event cannot prove zero bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ZeroBytesWitness {
    /// Fresh, non-pooled connection on which zero bytes were written.
    FreshConnectionZeroBytes,
    /// Refused before dispatch began (e.g. admission refusal).
    PreDispatchRefusal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AttemptOutcome {
    NoExecutionProven {
        witness: ZeroBytesWitness,
    },
    /// A response was received (any status). Contract validation may turn
    /// this into `ContractFailedAfterExecution`; that is still `Executed`
    /// here and is terminal — it never fails over and never replays.
    Executed,
    UnknownAfterSend,
}

/// Raw transport observation used for classification. Classification is by
/// witness, never by error kind alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportWitnessInput {
    FreshConnectionZeroBytes,
    PreDispatchRefusal,
    /// Reset/error on a reused keep-alive connection. Never a witness.
    ReusedConnectionReset,
    BytesMayHaveBeenSent,
    ResponseReceived,
}

pub fn classify_transport_witness(input: TransportWitnessInput) -> AttemptOutcome {
    match input {
        TransportWitnessInput::FreshConnectionZeroBytes => AttemptOutcome::NoExecutionProven {
            witness: ZeroBytesWitness::FreshConnectionZeroBytes,
        },
        TransportWitnessInput::PreDispatchRefusal => AttemptOutcome::NoExecutionProven {
            witness: ZeroBytesWitness::PreDispatchRefusal,
        },
        TransportWitnessInput::ReusedConnectionReset
        | TransportWitnessInput::BytesMayHaveBeenSent => AttemptOutcome::UnknownAfterSend,
        TransportWitnessInput::ResponseReceived => AttemptOutcome::Executed,
    }
}

/// Classify from connection facts: only a fresh connection with exactly
/// zero written bytes proves no execution. A reused-connection reset (or
/// any non-fresh connection, or any written byte) is `UnknownAfterSend`.
pub fn classify_from_connection_facts(
    fresh_connection: bool,
    bytes_written: u64,
    reused_connection_reset: bool,
) -> AttemptOutcome {
    if reused_connection_reset || !fresh_connection {
        return AttemptOutcome::UnknownAfterSend;
    }
    if bytes_written == 0 {
        AttemptOutcome::NoExecutionProven {
            witness: ZeroBytesWitness::FreshConnectionZeroBytes,
        }
    } else {
        AttemptOutcome::UnknownAfterSend
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AttemptState {
    Dispatching,
    NoExecutionProven,
    Executed,
    UnknownAfterSend,
}
impl From<AttemptOutcome> for AttemptState {
    fn from(o: AttemptOutcome) -> Self {
        match o {
            AttemptOutcome::NoExecutionProven { .. } => Self::NoExecutionProven,
            AttemptOutcome::Executed => Self::Executed,
            AttemptOutcome::UnknownAfterSend => Self::UnknownAfterSend,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttemptJournalEntry {
    pub logical_operation_id: String,
    pub attempt_index: u32,
    pub endpoint: String,
    pub state: AttemptState,
    pub at_ms: u64,
}

/// Journal contract. Engine contract: `begin` (state `Dispatching`) MUST be
/// recorded before any byte is sent; `finish` before `begin` is an error.
pub trait JournalStore {
    fn begin(&mut self, entry: AttemptJournalEntry) -> Result<(), FailoverError>;
    fn finish(
        &mut self,
        logical_operation_id: &str,
        attempt_index: u32,
        outcome: AttemptOutcome,
    ) -> Result<(), FailoverError>;
    fn entries(&self) -> &[AttemptJournalEntry];
    fn record_spend(&mut self, units: u64) {
        let _ = units;
    }
    fn spent_units(&self) -> u64 {
        0
    }
    fn attempts_used(&self) -> u32 {
        u32::try_from(self.entries().len()).unwrap_or(u32::MAX)
    }
    fn last(&self) -> Option<&AttemptJournalEntry> {
        self.entries().last()
    }
    /// Entries for one logical operation still `Dispatching` (no finish):
    /// on load these are uncertain and must refuse that logical id.
    fn has_uncertain(&self, logical_operation_id: &str) -> bool {
        self.entries().iter().any(|e| {
            e.logical_operation_id == logical_operation_id && e.state == AttemptState::Dispatching
        })
    }
}

/// In-memory journal. `spent_units` is the wrapper accounting for the parent
/// reservation: each finished (or begun) attempt can charge against it via
/// [`AttemptJournal::record_spend`]; unknown outcomes count as spent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttemptJournal {
    entries: Vec<AttemptJournalEntry>,
    pub spent_units: u64,
}
pub type MemJournal = AttemptJournal;

impl AttemptJournal {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn from_entries(entries: Vec<AttemptJournalEntry>, spent_units: u64) -> Self {
        Self {
            entries,
            spent_units,
        }
    }
    pub fn record_spend(&mut self, units: u64) {
        self.spent_units = self.spent_units.saturating_add(units);
    }
    pub fn spent_units(&self) -> u64 {
        self.spent_units
    }
    pub fn last(&self) -> Option<&AttemptJournalEntry> {
        self.entries.last()
    }
    pub fn attempts_used(&self) -> u32 {
        u32::try_from(self.entries.len()).unwrap_or(u32::MAX)
    }
}

impl JournalStore for AttemptJournal {
    fn begin(&mut self, entry: AttemptJournalEntry) -> Result<(), FailoverError> {
        if self.entries.iter().any(|e| {
            e.logical_operation_id == entry.logical_operation_id
                && e.attempt_index == entry.attempt_index
        }) {
            return Err(FailoverError::DuplicateBegin {
                logical_operation_id: entry.logical_operation_id,
                attempt_index: entry.attempt_index,
            });
        }
        self.entries.push(entry);
        Ok(())
    }
    fn finish(
        &mut self,
        logical_operation_id: &str,
        attempt_index: u32,
        outcome: AttemptOutcome,
    ) -> Result<(), FailoverError> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| {
                e.logical_operation_id == logical_operation_id && e.attempt_index == attempt_index
            })
            .ok_or_else(|| FailoverError::FinishBeforeBegin {
                logical_operation_id: logical_operation_id.to_string(),
                attempt_index,
            })?;
        if entry.state != AttemptState::Dispatching {
            return Err(FailoverError::AttemptAlreadyFinished {
                logical_operation_id: logical_operation_id.to_string(),
                attempt_index,
            });
        }
        entry.state = outcome.into();
        Ok(())
    }
    fn entries(&self) -> &[AttemptJournalEntry] {
        &self.entries
    }
    fn record_spend(&mut self, units: u64) {
        self.spent_units = self.spent_units.saturating_add(units);
    }
    fn spent_units(&self) -> u64 {
        self.spent_units
    }
    fn attempts_used(&self) -> u32 {
        u32::try_from(self.entries.len()).unwrap_or(u32::MAX)
    }
    fn last(&self) -> Option<&AttemptJournalEntry> {
        self.entries.last()
    }
}

/// One finite parent allocation fixed before the first dispatch (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParentEnvelope {
    pub max_attempts: u32,
    /// Absolute monotonic deadline shared by all attempts.
    pub deadline_mono_ms: u64,
    /// Worst-case reservation across all attempts.
    pub reserved_units: u64,
}

impl ParentEnvelope {
    /// Build with `max_attempts <= set size` enforced and worst-case
    /// reservation `per_attempt_worst_units * max_attempts`.
    pub fn build(
        max_attempts: u32,
        set_size: usize,
        deadline_mono_ms: u64,
        per_attempt_worst_units: u64,
    ) -> Result<Self, FailoverError> {
        if max_attempts == 0 {
            return Err(FailoverError::MaxAttemptsZero);
        }
        if max_attempts as usize > set_size {
            return Err(FailoverError::MaxAttemptsExceedsSetSize {
                max_attempts,
                set_size,
            });
        }
        let reserved_units = per_attempt_worst_units
            .checked_mul(u64::from(max_attempts))
            .ok_or(FailoverError::ReservationOverflow)?;
        Ok(Self {
            max_attempts,
            deadline_mono_ms,
            reserved_units,
        })
    }
    /// Remaining time, `None` when expired. Per-try timeout is this value
    /// only — it is never reset per attempt.
    pub fn remaining(&self, now_ms: u64) -> Option<u64> {
        self.deadline_mono_ms.checked_sub(now_ms).filter(|&r| r > 0)
    }
    pub fn per_try_timeout_ms(&self, now_ms: u64) -> Option<u64> {
        self.remaining(now_ms)
    }
    /// Worst-case units attributable to one attempt under this envelope.
    fn per_attempt_worst_units(&self) -> u64 {
        if self.max_attempts == 0 {
            return self.reserved_units;
        }
        self.reserved_units / u64::from(self.max_attempts)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PolicyFingerprint {
    pub contract_digest: String,
    pub policy_digest: String,
    pub credential_scope: String,
}

/// Pure drift check. Drift refuses *new* dispatch only; an in-flight attempt
/// is untouched (the engine consults this pre-dispatch, never mid-flight).
pub fn drift_invalidates(
    fingerprint_at_admission: &PolicyFingerprint,
    current: &PolicyFingerprint,
) -> bool {
    fingerprint_at_admission != current
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    ExecutedTerminal,
    UnknownAfterSendTerminal,
    AttemptsExhausted,
    DeadlineExpired,
    ReservationExhausted,
    DriftInvalidated,
    MissingIdempotencyKey,
    AttemptInFlight,
    NoEndpoints,
    UncertainPriorAttempt,
    UnapprovedPolicyDigest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverAction {
    Attempt {
        endpoint_index: usize,
        /// Required `Some` for `IdempotentWithKey` moves (same key resent).
        idempotency_key: Option<String>,
    },
    Stop {
        reason: StopReason,
    },
}

/// Pure failover decision engine.
pub struct FailoverDecision;

impl FailoverDecision {
    /// Decide the next pre-dispatch action. This is consulted only before a
    /// new dispatch, so drift here never disturbs an in-flight attempt.
    #[allow(clippy::too_many_arguments)]
    pub fn next_action(
        set: &EndpointSet,
        journal: &AttemptJournal,
        envelope: &ParentEnvelope,
        now_ms: u64,
        idempotency_key: Option<&str>,
        fingerprint_at_admission: Option<&PolicyFingerprint>,
        current_fingerprint: Option<&PolicyFingerprint>,
    ) -> FailoverAction {
        if set.endpoints.is_empty() {
            return FailoverAction::Stop {
                reason: StopReason::NoEndpoints,
            };
        }
        if let (Some(a), Some(c)) = (fingerprint_at_admission, current_fingerprint)
            && drift_invalidates(a, c)
        {
            return FailoverAction::Stop {
                reason: StopReason::DriftInvalidated,
            };
        }
        if envelope.remaining(now_ms).is_none() {
            return FailoverAction::Stop {
                reason: StopReason::DeadlineExpired,
            };
        }
        let used = journal.attempts_used();
        if let Some(last) = journal.last() {
            match last.state {
                AttemptState::Dispatching => {
                    return FailoverAction::Stop {
                        reason: StopReason::AttemptInFlight,
                    };
                }
                AttemptState::Executed => {
                    return FailoverAction::Stop {
                        reason: StopReason::ExecutedTerminal,
                    };
                }
                AttemptState::UnknownAfterSend => {
                    return FailoverAction::Stop {
                        reason: StopReason::UnknownAfterSendTerminal,
                    };
                }
                AttemptState::NoExecutionProven => {}
            }
        }
        if used >= envelope.max_attempts || used as usize >= set.endpoints.len() {
            return FailoverAction::Stop {
                reason: StopReason::AttemptsExhausted,
            };
        }
        // Parent reservation: the next attempt's worst case must fit.
        let next_cost = envelope.per_attempt_worst_units();
        if next_cost > 0
            && journal.spent_units().saturating_add(next_cost) > envelope.reserved_units
        {
            return FailoverAction::Stop {
                reason: StopReason::ReservationExhausted,
            };
        }
        // Only NoExecutionProven (or no attempt yet) reaches here, for both
        // classes. IdempotentWithKey additionally requires the same key.
        let key = match set.side_effect_class {
            SideEffectClass::IdempotentWithKey => match idempotency_key {
                Some(k) if !k.trim().is_empty() => Some(k.to_string()),
                _ => {
                    return FailoverAction::Stop {
                        reason: StopReason::MissingIdempotencyKey,
                    };
                }
            },
            SideEffectClass::SideEffectful => None,
        };
        FailoverAction::Attempt {
            endpoint_index: used as usize,
            idempotency_key: key,
        }
    }
}

/// Free-function form of [`FailoverDecision::next_action`].
#[allow(clippy::too_many_arguments)]
pub fn next_action(
    set: &EndpointSet,
    journal: &AttemptJournal,
    envelope: &ParentEnvelope,
    now_ms: u64,
    idempotency_key: Option<&str>,
    fingerprint_at_admission: Option<&PolicyFingerprint>,
    current_fingerprint: Option<&PolicyFingerprint>,
) -> FailoverAction {
    FailoverDecision::next_action(
        set,
        journal,
        envelope,
        now_ms,
        idempotency_key,
        fingerprint_at_admission,
        current_fingerprint,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governor::{
        AdmissionOutcome, AdmissionRequest, GovernorStore, MemGovernorStore, Principal,
        SettleRequest, SettledOutcome,
    };

    fn set(class: SideEffectClass) -> EndpointSet {
        EndpointSet {
            connector_id: "c1".into(),
            tool: "t1".into(),
            endpoints: vec![
                "https://h/a".into(),
                "https://h/b".into(),
                "https://h/c".into(),
            ],
            side_effect_class: class,
            idempotency_conformance_test: if class == SideEffectClass::IdempotentWithKey {
                Some("idem_conformance_v1".into())
            } else {
                None
            },
            owner_approved: true,
            policy_digest: "pd1".into(),
        }
    }
    fn envelope(set: &EndpointSet) -> ParentEnvelope {
        ParentEnvelope::build(3, set.endpoints.len(), 10_000, 10).unwrap()
    }
    fn begin(j: &mut AttemptJournal, idx: u32, ep: &str) {
        j.begin(AttemptJournalEntry {
            logical_operation_id: "op1".into(),
            attempt_index: idx,
            endpoint: ep.into(),
            state: AttemptState::Dispatching,
            at_ms: 100,
        })
        .unwrap();
    }
    fn journal_with(
        outcome: AttemptOutcome,
        class: SideEffectClass,
    ) -> (EndpointSet, AttemptJournal) {
        let s = set(class);
        let mut j = AttemptJournal::new();
        begin(&mut j, 0, "h/a");
        j.finish("op1", 0, outcome).unwrap();
        (s, j)
    }
    fn fp(policy: &str) -> PolicyFingerprint {
        PolicyFingerprint {
            contract_digest: "cd".into(),
            policy_digest: policy.into(),
            credential_scope: "scope".into(),
        }
    }

    #[test]
    fn reused_connection_reset_is_unknown_after_send() {
        // Fresh + zero bytes proves no execution.
        assert_eq!(
            classify_from_connection_facts(true, 0, false),
            AttemptOutcome::NoExecutionProven {
                witness: ZeroBytesWitness::FreshConnectionZeroBytes
            }
        );
        // Reused + reset is never a witness.
        assert_eq!(
            classify_from_connection_facts(false, 0, true),
            AttemptOutcome::UnknownAfterSend
        );
        assert_eq!(
            classify_transport_witness(TransportWitnessInput::ReusedConnectionReset),
            AttemptOutcome::UnknownAfterSend
        );
    }

    #[test]
    fn unknown_after_send_never_replays() {
        for class in [
            SideEffectClass::SideEffectful,
            SideEffectClass::IdempotentWithKey,
        ] {
            let (s, j) = journal_with(AttemptOutcome::UnknownAfterSend, class);
            let e = envelope(&s);
            assert_eq!(
                next_action(&s, &j, &e, 200, Some("k1"), None, None),
                FailoverAction::Stop {
                    reason: StopReason::UnknownAfterSendTerminal
                }
            );
        }
    }

    #[test]
    fn identical_side_effectful_redispatch_after_unknown_refused() {
        // Same endpoint, same everything, after Unknown: still Stop.
        let (s, j) = journal_with(
            AttemptOutcome::UnknownAfterSend,
            SideEffectClass::SideEffectful,
        );
        let e = envelope(&s);
        assert!(matches!(
            next_action(&s, &j, &e, 200, None, None, None),
            FailoverAction::Stop { .. }
        ));
    }

    #[test]
    fn contract_failure_never_fails_over() {
        // Contract failure after execution is modelled as Executed: terminal.
        for class in [
            SideEffectClass::SideEffectful,
            SideEffectClass::IdempotentWithKey,
        ] {
            let (s, j) = journal_with(AttemptOutcome::Executed, class);
            let e = envelope(&s);
            assert_eq!(
                next_action(&s, &j, &e, 200, Some("k1"), None, None),
                FailoverAction::Stop {
                    reason: StopReason::ExecutedTerminal
                }
            );
        }
    }

    #[test]
    fn side_effectful_after_dispatch_never_moves() {
        // After dispatch without a zero-bytes witness: Unknown, never moves.
        let outcome = classify_from_connection_facts(true, 42, false);
        assert_eq!(outcome, AttemptOutcome::UnknownAfterSend);
        let (s, j) = journal_with(outcome, SideEffectClass::SideEffectful);
        let e = envelope(&s);
        assert!(matches!(
            next_action(&s, &j, &e, 200, None, None, None),
            FailoverAction::Stop { .. }
        ));
        // And a proven pre-dispatch failure does move for SideEffectful.
        let (s2, j2) = journal_with(
            AttemptOutcome::NoExecutionProven {
                witness: ZeroBytesWitness::PreDispatchRefusal,
            },
            SideEffectClass::SideEffectful,
        );
        let e2 = envelope(&s2);
        assert_eq!(
            next_action(&s2, &j2, &e2, 200, None, None, None),
            FailoverAction::Attempt {
                endpoint_index: 1,
                idempotency_key: None
            }
        );
    }

    #[test]
    fn idempotent_requires_key_conformance() {
        // Validate refuses IdempotentWithKey without a conformance test name.
        let mut bad = set(SideEffectClass::IdempotentWithKey);
        bad.idempotency_conformance_test = None;
        assert_eq!(bad.validate(), Err(FailoverError::MissingConformanceTest));
        bad.idempotency_conformance_test = Some("  ".into());
        assert_eq!(bad.validate(), Err(FailoverError::MissingConformanceTest));
        // Decision requires the same key to be resent on a move.
        let (s, j) = journal_with(
            AttemptOutcome::NoExecutionProven {
                witness: ZeroBytesWitness::FreshConnectionZeroBytes,
            },
            SideEffectClass::IdempotentWithKey,
        );
        let e = envelope(&s);
        assert_eq!(
            next_action(&s, &j, &e, 200, None, None, None),
            FailoverAction::Stop {
                reason: StopReason::MissingIdempotencyKey
            }
        );
        assert_eq!(
            next_action(&s, &j, &e, 200, Some("k1"), None, None),
            FailoverAction::Attempt {
                endpoint_index: 1,
                idempotency_key: Some("k1".into())
            }
        );
    }

    #[test]
    fn parent_deadline_spans_all_tries() {
        let s = set(SideEffectClass::SideEffectful);
        let e = ParentEnvelope::build(3, 3, 1_000, 10).unwrap();
        assert_eq!(e.remaining(0), Some(1_000));
        assert_eq!(e.remaining(400), Some(600));
        assert_eq!(e.per_try_timeout_ms(400), Some(600));
        assert_eq!(e.remaining(1_000), None);
        let (_, j) = journal_with(
            AttemptOutcome::NoExecutionProven {
                witness: ZeroBytesWitness::FreshConnectionZeroBytes,
            },
            SideEffectClass::SideEffectful,
        );
        assert_eq!(
            next_action(&s, &j, &e, 1_000, None, None, None),
            FailoverAction::Stop {
                reason: StopReason::DeadlineExpired
            }
        );
    }

    #[test]
    fn parent_reservation_exhaustion_stops_failover() {
        let s = set(SideEffectClass::SideEffectful);
        // reserved = 10 * 3 = 30; journal wrapper has already spent it all.
        let e = envelope(&s);
        assert_eq!(e.reserved_units, 30);
        let (_, mut j) = journal_with(
            AttemptOutcome::NoExecutionProven {
                witness: ZeroBytesWitness::FreshConnectionZeroBytes,
            },
            SideEffectClass::SideEffectful,
        );
        j.record_spend(30);
        assert_eq!(
            next_action(&s, &j, &e, 200, None, None, None),
            FailoverAction::Stop {
                reason: StopReason::ReservationExhausted
            }
        );
    }

    #[test]
    fn drift_invalidates_new_dispatch_not_inflight() {
        let a = fp("pd1");
        let b = fp("pd2");
        assert!(drift_invalidates(&a, &b));
        assert!(!drift_invalidates(&a, &a));
        // Pre-dispatch: drift stops a new attempt after NoExecutionProven.
        let (s, j) = journal_with(
            AttemptOutcome::NoExecutionProven {
                witness: ZeroBytesWitness::FreshConnectionZeroBytes,
            },
            SideEffectClass::SideEffectful,
        );
        let e = envelope(&s);
        assert_eq!(
            next_action(&s, &j, &e, 200, None, Some(&a), Some(&b)),
            FailoverAction::Stop {
                reason: StopReason::DriftInvalidated
            }
        );
        // In-flight untouched: a Dispatching row stays Dispatching and the
        // engine refuses a parallel new dispatch (it does not rewrite it).
        let mut j2 = AttemptJournal::new();
        begin(&mut j2, 0, "h/a");
        assert_eq!(
            next_action(&s, &j2, &e, 200, None, Some(&a), Some(&b)),
            FailoverAction::Stop {
                reason: StopReason::DriftInvalidated
            }
        );
        assert_eq!(j2.last().unwrap().state, AttemptState::Dispatching);
    }

    #[test]
    fn attempt_journal_dispatching_before_bytes() {
        let mut j = MemJournal::new();
        // finish-before-begin is an error.
        assert_eq!(
            j.finish("op1", 0, AttemptOutcome::Executed),
            Err(FailoverError::FinishBeforeBegin {
                logical_operation_id: "op1".into(),
                attempt_index: 0
            })
        );
        // begin records Dispatching before any outcome can be recorded.
        begin(&mut j, 0, "h/a");
        assert_eq!(j.entries()[0].state, AttemptState::Dispatching);
        j.finish("op1", 0, AttemptOutcome::Executed).unwrap();
        assert_eq!(j.entries()[0].state, AttemptState::Executed);
    }

    #[test]
    fn unconfirmed_permit_stays_counted_against_concurrency() {
        use crate::governor::GovernorPolicy;
        use std::collections::BTreeMap;
        let policy = GovernorPolicy {
            concurrency_per_scope: 1,
            rate_window_ms: 60_000,
            rate_max_per_window: 100,
            budget_units_per_scope: 10_000,
            freshness_max_age_ms: 5_000,
            replay_guard_window_ms: 60_000,
            deadline_window_ms: 60_000,
            per_scope: BTreeMap::new(),
        };
        let store = MemGovernorStore::new(policy);
        let req = AdmissionRequest::new(Principal::Owner, "p", "h/a", 10);
        let permit = match store.reserve(&req, 1_000).unwrap() {
            AdmissionOutcome::Admitted(p) => p,
            other => panic!("expected admitted, got {other:?}"),
        };
        // Settled as Unknown without confirmed completion: stays counted.
        store
            .settle(&SettleRequest {
                permit_id: permit.id.clone(),
                actual_units: 10,
                outcome: SettledOutcome::UnknownAfterSend,
                complete: false,
            })
            .unwrap();
        assert!(
            store
                .unconfirmed_permits()
                .iter()
                .any(|p| p.id == permit.id)
        );
        // Concurrency cap (1) is still consumed: a second admit is deferred.
        assert!(matches!(
            store
                .reserve(
                    &AdmissionRequest::new(Principal::Owner, "p", "h/a", 10),
                    1_100
                )
                .unwrap(),
            AdmissionOutcome::Deferred { .. }
        ));
    }

    #[test]
    fn endpoint_set_validation_and_declared_gating() {
        let s = set(SideEffectClass::SideEffectful);
        assert!(s.validate().is_ok());
        // owner_approved is advisory/ignored: validate still passes, and
        // authorization depends only on the computed digest.
        let mut unapproved = s.clone();
        unapproved.owner_approved = false;
        assert!(unapproved.validate().is_ok());
        assert_eq!(
            unapproved.computed_policy_digest(),
            s.computed_policy_digest()
        );
        // Computed digest ignores a forged stored digest.
        let mut forged = s.clone();
        forged.policy_digest = "forged".into();
        assert_eq!(forged.computed_policy_digest(), s.computed_policy_digest());
        assert_ne!(forged.computed_policy_digest(), "forged");
        assert!(effective_endpoint_set("c1", "t1", None, None, None, true, "pd").is_none());
        let declared = effective_endpoint_set(
            "c1",
            "t1",
            Some(vec!["https://h/a".into(), "https://h/b".into()]),
            None,
            None,
            false,
            "forged",
        )
        .unwrap();
        assert_eq!(declared.side_effect_class, SideEffectClass::SideEffectful);
        assert_eq!(declared.policy_digest, declared.computed_policy_digest());
        // Non-URL endpoints rejected at validate.
        let mut bad = s.clone();
        bad.endpoints = vec!["h/a".into()];
        assert!(matches!(
            bad.validate(),
            Err(FailoverError::InvalidEndpoint(0, _))
        ));
        bad.endpoints = vec!["ftp://h/a".into()];
        assert!(matches!(
            bad.validate(),
            Err(FailoverError::InvalidEndpoint(0, _))
        ));
        bad.endpoints = vec!["https:///a".into()];
        assert!(matches!(
            bad.validate(),
            Err(FailoverError::InvalidEndpoint(0, _))
        ));
        // Duplicate / empty endpoints refused.
        let mut dup = s.clone();
        dup.endpoints = vec!["https://h/a".into(), "https://h/a".into()];
        assert!(matches!(
            dup.validate(),
            Err(FailoverError::DuplicateEndpoint(_))
        ));
        // max_attempts <= set size enforced; reserved is worst-case.
        assert!(ParentEnvelope::build(4, 3, 1_000, 10).is_err());
        assert_eq!(
            ParentEnvelope::build(2, 3, 1_000, 10)
                .unwrap()
                .reserved_units,
            20
        );
    }
}
