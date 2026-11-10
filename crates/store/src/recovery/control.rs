//! Payload-free owner controls. IDs select configured owners; clients cannot supply paths.
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberRequest {
    pub store_id: String,
    pub operation_id: Uuid,
    #[serde(default)]
    pub scheduled_at_unix_seconds: Option<u64>,
    #[serde(default)]
    pub byte_cap: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRequest {
    pub policy_id: String,
    pub batch_id: Uuid,
    pub members: Vec<MemberRequest>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberStatus {
    Running,
    Durable,
    OutcomeUnknown,
    Failed,
    Unserved,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberReport {
    pub admitted_bytes: u64,
    pub manifest_sha256: Option<String>,
    pub store_id: String,
    pub operation_id: Uuid,
    pub status: MemberStatus,
    /// Sequence belongs only to this physical store; batches promise no global sequence.
    pub sequence: Option<u64>,
    pub reason: Option<String>,
    pub catalog_identity: Option<String>,
    pub dependencies: Vec<super::profiles::ReferenceDependency>,
    pub shared_config: Option<super::manifest::SharedConfigReference>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchReport {
    pub batch_id: Uuid,
    pub members: Vec<MemberReport>,
    pub complete: bool,
    pub overdue: bool,
    pub outbox_full: bool,
    pub missing_gates: Vec<String>,
}
impl BatchReport {
    pub(crate) fn refresh(&mut self) {
        self.complete = !self.members.is_empty()
            && self.members.iter().all(|m| {
                m.status == MemberStatus::Durable
                    && m.dependencies.iter().all(|dependency| {
                        self.members.iter().any(|target| {
                            target.catalog_identity.as_ref() == Some(&dependency.catalog_identity)
                                && dependency
                                    .required_sequence
                                    .is_none_or(|required| target.sequence == Some(required))
                        })
                    })
            });
        let configs: std::collections::BTreeSet<_> = self
            .members
            .iter()
            .filter_map(|m| {
                m.shared_config
                    .as_ref()
                    .map(|r| (r.batch_id, &r.descriptor_sha256))
            })
            .collect();
        if configs.iter().any(|(batch, _)| *batch != self.batch_id)
            || configs.len() > 1
            || (!configs.is_empty() && self.members.iter().any(|m| m.shared_config.is_none()))
        {
            self.complete = false;
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryEvent {
    pub operation_id: Uuid,
    pub catalog_identity: String,
    pub outcome: RecoveryEventOutcome,
    pub sizes: RecoverySizes,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryEventOutcome {
    pub code: RecoveryOutcomeCode,
    pub consecutive_failures: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOutcomeCode {
    ScheduledOwnerFailure,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverySizes {
    pub image_bytes: u64,
    pub bundle_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryEventEnvelope {
    pub batch_id: Uuid,
    pub sink_id: String,
    pub event: RecoveryEvent,
}
/// Evidence about whether an owner could have admitted this dispatch.
#[derive(Debug)]
pub(crate) enum OwnerCallError {
    NotDispatched,
    OutcomeUnknown,
}
pub(crate) fn call_until(
    endpoint: &str,
    method: &str,
    request: &BatchRequest,
    deadline: std::time::Instant,
) -> Result<BatchReport, OwnerCallError> {
    let path = endpoint
        .strip_prefix("unix://")
        .filter(|p| p.starts_with('/'))
        .ok_or(OwnerCallError::NotDispatched)?;
    if !matches!(method, "recovery_start" | "recovery_status") {
        return Err(OwnerCallError::NotDispatched);
    }
    crate::FerroStore::recovery_owner_call(std::path::Path::new(path), method, request, deadline)
}
/// Explicit connection only. A failed Unix connect cannot fall back to an embedded store.
pub fn call(
    endpoint: &str,
    method: &str,
    request: &BatchRequest,
) -> Result<BatchReport, super::RecoveryError> {
    if !endpoint.starts_with("unix:///") || !matches!(method, "recovery_start" | "recovery_status")
    {
        return Err(super::RecoveryError::Refused(
            "local owner recovery connection required".into(),
        ));
    }
    crate::FerroStore::connect_uri(endpoint)
        .and_then(|store| store.recovery_control(method, request))
        .map_err(|_| {
            super::RecoveryError::Refused(
                "configured owner unavailable or recovery request refused".into(),
            )
        })
}

/// Adapter must honor the deadline and deduplicate `delivery_key` durably at
/// its receiving boundary. This stable key survives a sender crash after send.
pub trait RecoveryEventSink: Send + Sync {
    fn emit(
        &self,
        event: &RecoveryEventEnvelope,
        delivery_key: &str,
        deadline: std::time::Instant,
    ) -> Result<(), super::RecoveryError>;
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending,
    Delivered,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventDelivery {
    pub attempts: u8,
    pub status: DeliveryStatus,
}

/// Immutable recovery-set evidence excludes mutable polling/alert/gate status.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableBatchV1 {
    pub version: u32,
    pub batch_id: Uuid,
    pub members: Vec<DurableBatchMember>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableBatchMember {
    pub operation_id: Uuid,
    pub catalog_identity: String,
    pub sequence: u64,
    pub manifest_sha256: String,
    pub shared_config: Option<super::manifest::SharedConfigReference>,
}
