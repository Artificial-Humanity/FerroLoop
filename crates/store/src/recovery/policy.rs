//! Versioned, explicitly bounded owner policy. Loading policy never proves capture safety.
use super::{RecoveryError, ValidationProfile};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub store_id: String,
    pub endpoint: String,
    pub interval_seconds: u64,
    pub prospective_bundle_bytes: u64,
    pub catalog_identity: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryLimits {
    pub retained_pin_bytes: u64,
    pub preparation_bytes: u64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub image_bytes: u64,
    pub metadata_bytes: u64,
    pub margin_bytes: u64,
    pub total_seconds: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPolicy {
    pub version: u32,
    pub enabled: bool,
    pub coordinator: bool,
    pub coverage_interval_seconds: u64,
    pub policy_id: String,
    pub store_id: String,
    pub destination: PathBuf,
    pub source_map_sha256: String,
    pub profile: ValidationProfile,
    pub limits: Option<RecoveryLimits>,
    pub members: Vec<Schedule>,
    pub failure_sink: Option<String>,
    pub owner_sink: Option<String>,
    pub pinned_operations: BTreeSet<uuid::Uuid>,
    pub configs: Vec<super::backup::ApprovedConfig>,
    pub excluded_vault_key: Option<super::backup::ExcludedVaultKey>,
}
impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            version: 1,
            enabled: false,
            coordinator: false,
            coverage_interval_seconds: 0,
            policy_id: String::new(),
            store_id: String::new(),
            destination: PathBuf::new(),
            source_map_sha256: String::new(),
            profile: ValidationProfile::new(super::ProfileKind::SmallEmpty, "disabled"),
            limits: None,
            members: Vec::new(),
            failure_sink: None,
            owner_sink: None,
            pinned_operations: BTreeSet::new(),
            configs: Vec::new(),
            excluded_vault_key: None,
        }
    }
}
fn refuse(reason: &str) -> RecoveryError {
    RecoveryError::Refused(reason.into())
}
impl RecoveryPolicy {
    pub fn validate(&self) -> Result<(), RecoveryError> {
        if self.version != 1 {
            return Err(refuse("unsupported recovery policy version"));
        }
        if !self.enabled && self.limits.is_none() {
            return Ok(());
        }
        if self.enabled && !matches!(self.coverage_interval_seconds, 3600 | 21600) {
            return Err(refuse("explicit owner coverage interval required"));
        }
        let l = self
            .limits
            .as_ref()
            .ok_or_else(|| refuse("explicit recovery limits required"))?;
        if l.retained_pin_bytes == 0
            || l.retained_pin_bytes > 512 << 20
            || l.preparation_bytes == 0
            || l.memory_bytes == 0
            || l.disk_bytes == 0
            || l.image_bytes == 0
            || l.metadata_bytes == 0
            || l.metadata_bytes > 512 << 20
            || l.total_seconds == 0
            || l.total_seconds > 600
            || l.preparation_bytes > l.memory_bytes
            || l.retained_pin_bytes > l.memory_bytes
            || l.image_bytes
                .checked_mul(2)
                .and_then(|n| n.checked_add(l.metadata_bytes))
                .is_none_or(|n| n > l.disk_bytes)
        {
            return Err(refuse("invalid explicit recovery limits"));
        }
        if self.policy_id.is_empty()
            || self.policy_id.len() > 128
            || self.store_id.is_empty()
            || self.store_id.len() > 128
            || !self.destination.is_absolute()
            || self.source_map_sha256.len() != 64
            || !self
                .source_map_sha256
                .bytes()
                .all(|c| c.is_ascii_hexdigit())
        {
            return Err(refuse("invalid configured recovery identity"));
        }
        if self.configs.len() > 3
            || (!self.coordinator && !self.configs.is_empty())
            || (!self.configs.is_empty() && self.excluded_vault_key.is_none())
        {
            return Err(refuse(
                "only coordinator may retain explicitly approved batch configs with key exclusion",
            ));
        }
        if self.members.len() > 128 || self.pinned_operations.len() > 4096 {
            return Err(refuse("recovery policy count limit"));
        }
        let mut seen = BTreeSet::new();
        for m in &self.members {
            if m.store_id.is_empty()
                || m.store_id.len() > 128
                || !seen.insert(&m.store_id)
                || !m.endpoint.starts_with("unix:///")
                || m.endpoint.len() > 4096
                || !matches!(m.interval_seconds, 3600 | 21600)
                || m.prospective_bundle_bytes == 0
                || m.catalog_identity.is_empty()
                || m.catalog_identity.len() > 256
            {
                return Err(refuse("invalid explicit owner endpoint or interval"));
            }
        }
        if self.coordinator && self.members.is_empty() {
            return Err(refuse("coordinator needs explicit members"));
        }
        if !self.coordinator && !self.members.is_empty() {
            return Err(refuse("only coordinator may configure member schedules"));
        }
        for sink in [&self.failure_sink, &self.owner_sink].into_iter().flatten() {
            if sink.is_empty()
                || sink.len() > 128
                || !sink
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-:.".contains(&b))
            {
                return Err(refuse("invalid recovery event sink ID"));
            }
        }
        Ok(())
    }
    pub fn load(path: &std::path::Path) -> Result<Self, RecoveryError> {
        use std::{io::Read, os::unix::fs::OpenOptionsExt};
        // A supplied FIFO/device must not hold data-daemon startup hostage.
        // Validate the held descriptor, retaining accepted symlink semantics.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| refuse("recovery policy unavailable"))?;
        let metadata = file
            .metadata()
            .map_err(|_| refuse("recovery policy unreadable"))?;
        if !metadata.is_file() {
            return Err(refuse("regular recovery policy file required"));
        }
        if metadata.len() > 1_048_576 {
            return Err(refuse("recovery policy byte limit"));
        }
        let mut bytes = Vec::new();
        file.by_ref()
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(|_| refuse("recovery policy unreadable"))?;
        if bytes.len() > 1_048_576 {
            return Err(refuse("recovery policy byte limit"));
        }
        let value: Self =
            serde_json::from_slice(&bytes).map_err(|_| refuse("invalid recovery policy"))?;
        value.validate()?;
        Ok(value)
    }
}

/// Admission timestamps, never a global store sequence. Failed attempts stay
/// overdue; an active owner is separately protected by its operation journal.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerClock {
    completed: BTreeMap<String, u64>,
}
impl SchedulerClock {
    pub fn due(&self, schedules: &[Schedule], now: u64) -> Vec<String> {
        schedules
            .iter()
            .filter(|s| {
                s.interval_seconds != 0
                    && now >= s.interval_seconds
                    && now / s.interval_seconds
                        > self.completed.get(&s.store_id).copied().unwrap_or(0) / s.interval_seconds
            })
            .map(|s| s.store_id.clone())
            .collect()
    }
    pub fn admit(&mut self, stores: &[String], now: u64) {
        for id in stores {
            self.completed.insert(id.clone(), now);
        }
    }
}

/// Eligibility only: deletion is allowed only after the retained reference
/// closure has been validated. This function never removes recovery material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionDecision {
    pub retained_operations: BTreeSet<String>,
    pub removable_operations: BTreeSet<String>,
    pub retained_config_batches: BTreeSet<String>,
    pub removable_config_batches: BTreeSet<String>,
}
#[derive(Clone)]
struct RetentionRecord {
    operation: String,
    store: String,
    seconds: u64,
    sequence: u64,
    dependencies: Vec<(String, Option<u64>)>,
    config_batch: Option<String>,
}
pub fn select_retained(
    catalog: &[super::backup::VerifiedBundle],
    policy: &RecoveryPolicy,
    now: std::time::SystemTime,
) -> Result<RetentionDecision, RecoveryError> {
    policy.validate()?;
    let seconds = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| refuse("UTC retention time required"))?
        .as_secs();
    if catalog.is_empty() {
        return retention_records(&[], policy, seconds);
    }
    let bound = policy
        .limits
        .as_ref()
        .ok_or_else(|| refuse("retention requires explicit limits"))?;
    if catalog.len() > 4096 {
        return Err(refuse("retention catalog count limit"));
    }
    let l = super::OperationLimits::new(
        std::time::Instant::now() + std::time::Duration::from_secs(bound.total_seconds),
        bound.disk_bytes,
        bound.memory_bytes,
        super::CancelToken::new(),
    );
    let mut records = Vec::new();
    for bundle in catalog {
        if bundle.path().parent() != Some(policy.destination.as_path()) {
            return Err(refuse("retention catalog is outside configured root"));
        }
        let m = super::catalog::retention_manifest(bundle, &l)?;
        records.push(RetentionRecord {
            operation: m.operation_id,
            store: m.catalog_store_identity,
            seconds: u64::try_from(m.captured_at_unix_ms / 1000)
                .map_err(|_| refuse("retention timestamp overflow"))?,
            sequence: m.published_sequence,
            dependencies: m
                .reference_dependencies
                .into_iter()
                .map(|d| (d.catalog_identity, d.required_sequence))
                .collect(),
            config_batch: m.shared_config.map(|r| r.batch_id.to_string()),
        });
    }
    retention_records(&records, policy, seconds)
}
fn utc_week(seconds: u64) -> u64 {
    seconds / 604800 + (seconds % 604800 + 259200) / 604800
}
fn retention_records(
    records: &[RetentionRecord],
    policy: &RecoveryPolicy,
    now: u64,
) -> Result<RetentionDecision, RecoveryError> {
    let mut chosen = BTreeSet::new();
    let mut buckets: BTreeMap<(String, u8, u64), &RetentionRecord> = BTreeMap::new();
    let newer = |a: &RetentionRecord, b: &RetentionRecord| {
        (a.seconds, a.sequence, &a.operation) > (b.seconds, b.sequence, &b.operation)
    };
    let mut all = BTreeSet::new();
    for r in records {
        if !all.insert(r.operation.clone()) {
            return Err(refuse("duplicate retention operation"));
        }
        if r.seconds > now {
            return Err(refuse("future recovery timestamp requires review"));
        }
        if policy
            .pinned_operations
            .iter()
            .any(|id| id.to_string() == r.operation)
        {
            chosen.insert(r.operation.clone());
        }
        let candidates = [
            (0, 0, true),
            (1, r.seconds / 3600, now / 3600 - r.seconds / 3600 < 24),
            (2, r.seconds / 86400, now / 86400 - r.seconds / 86400 < 7),
            (
                3,
                utc_week(r.seconds),
                utc_week(now) - utc_week(r.seconds) < 4,
            ),
        ];
        for (kind, bucket, within) in candidates {
            if within {
                let key = (r.store.clone(), kind, bucket);
                if buckets.get(&key).is_none_or(|old| newer(r, old)) {
                    buckets.insert(key, r);
                }
            }
        }
    }
    for r in buckets.values() {
        chosen.insert(r.operation.clone());
    }
    loop {
        let mut added = Vec::new();
        for r in records.iter().filter(|r| chosen.contains(&r.operation)) {
            for (store, sequence) in &r.dependencies {
                let dependency = records
                    .iter()
                    .filter(|d| d.store == *store && sequence.is_none_or(|seq| d.sequence == seq))
                    .max_by_key(|d| (d.seconds, d.sequence, &d.operation))
                    .ok_or_else(|| refuse("retained dependency set is incomplete"))?;
                if !chosen.contains(&dependency.operation) {
                    added.push(dependency.operation.clone());
                }
            }
        }
        if added.is_empty() {
            break;
        }
        chosen.extend(added);
    }
    let retained_config_batches: BTreeSet<_> = records
        .iter()
        .filter(|r| chosen.contains(&r.operation))
        .filter_map(|r| r.config_batch.clone())
        .collect();
    let all_configs: BTreeSet<_> = records
        .iter()
        .filter_map(|r| r.config_batch.clone())
        .collect();
    Ok(RetentionDecision {
        removable_operations: all.difference(&chosen).cloned().collect(),
        retained_operations: chosen,
        removable_config_batches: all_configs
            .difference(&retained_config_batches)
            .cloned()
            .collect(),
        retained_config_batches,
    })
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    fn record(n: u64, seconds: u64) -> RetentionRecord {
        RetentionRecord {
            operation: uuid::Uuid::from_u128(n as u128 + 1).to_string(),
            store: "fixture".into(),
            seconds,
            sequence: n,
            dependencies: vec![],
            config_batch: None,
        }
    }
    #[test]
    fn newest_per_utc_bucket_union_is_deterministic_and_last_verified_survives_age() {
        let now = 60 * 86400;
        let records: Vec<_> = (0..1440).map(|n| record(n, now - n * 3600)).collect();
        let p = RecoveryPolicy::default();
        let a = retention_records(&records, &p, now).unwrap();
        let expected: BTreeSet<_> = (0..24)
            .chain([25, 49, 73, 97, 121, 169, 337])
            .map(|n| record(n, 0).operation)
            .collect();
        assert_eq!(a.retained_operations, expected);
        let reversed: Vec<_> = records.into_iter().rev().collect();
        assert_eq!(a, retention_records(&reversed, &p, now).unwrap());
        let old = vec![record(1, 1)];
        assert_eq!(
            retention_records(&old, &p, now)
                .unwrap()
                .retained_operations
                .len(),
            1
        );
    }
    #[test]
    fn pinned_dependency_and_shared_config_reference_closure_prevents_gc() {
        let now = 100 * 86400;
        let mut latest = record(1, now);
        let mut dependency = record(2, 1);
        latest.dependencies.push(("other".into(), Some(2)));
        dependency.store = "other".into();
        latest.config_batch = Some("shared".into());
        dependency.config_batch = Some("shared".into());
        let mut newest_other = record(3, now);
        newest_other.store = "other".into();
        let mut rollback = record(99, 0);
        rollback.config_batch = Some("pinned".into());
        let mut expired = record(88, 0);
        expired.config_batch = Some("expired".into());
        let mut policy = RecoveryPolicy::default();
        policy
            .pinned_operations
            .insert(rollback.operation.parse().unwrap());
        let records = vec![latest, dependency, newest_other, rollback, expired];
        let decision = retention_records(&records, &policy, now).unwrap();
        assert_eq!(decision.retained_operations.len(), 4);
        assert!(decision.retained_config_batches.contains("shared"));
        assert!(decision.retained_config_batches.contains("pinned"));
        assert_eq!(
            decision.removable_config_batches,
            ["expired".to_owned()].into_iter().collect()
        );
        assert!(retention_records(&records[..1], &RecoveryPolicy::default(), now).is_err());
    }
}
