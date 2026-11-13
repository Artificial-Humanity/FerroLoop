//! Bounded restore sidecar metadata; no store images or expectations.
use super::profiles::QueryProbe;
use super::{OperationLimits, RecoveryError, ValidationProfile};
use serde::{
    Serialize,
    de::{DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use sha2::{Digest, Sha256};
use std::{fmt, io::Write, mem::size_of};

// Existing admission permits metadata <=512MiB. Source-audited maximum:
// initial5 (Admitted,Prepared,ExchangeIntent,RelocateIntent,Installed),
// 12shared attempts *4 (durable bump + at most3 install/resume transitions),
// 4reserve attempts *4 (bump plus reverse/terminal/negative conservative bound).
// Total <=69; 96 leaves27 frames, without journal growth. Renewals append0.
// Terminal cleanup retries MUST append0; a future new append site requires this
// schedule to be audited before changing this constant.
pub(crate) const WRITER_MAX_FRAMES: u64 = 5 + 12 * 4 + 4 * 4;
pub(crate) const MAX_FRAMES: u64 = 96;
const _: () = assert!(WRITER_MAX_FRAMES <= MAX_FRAMES);
pub(crate) const FRAME_HEADER: u64 = 72;
const MAX_JOURNAL_BYTES: u64 = 512 << 20;
const SCRATCH: u64 = 65536;
const MAX_DEPTH: usize = 32; // Fixed typed profile/lease/evidence nesting is <16.

#[derive(Clone, Copy)]
pub(crate) struct Caps {
    pub payload: u64,
    pub model_bytes: u64,
}
impl Caps {
    pub fn new(journal_bytes: u64, l: &OperationLimits) -> Result<Self, RecoveryError> {
        l.quota(
            journal_bytes,
            MAX_JOURNAL_BYTES,
            "restore metadata capacity",
        )?;
        let payload = journal_bytes
            .checked_div(MAX_FRAMES)
            .and_then(|n| n.checked_sub(FRAME_HEADER))
            .filter(|n| *n != 0)
            .ok_or_else(|| l.verification())?;
        // Three simultaneously resident bounded representations: caller/model,
        // previous decoded frame, current decoded frame. Includes Vec allocator
        // growth and JSON scratch with64x encoded bytes per representation.
        let model_bytes = payload
            .checked_mul(64)
            .and_then(|n| n.checked_mul(3))
            .and_then(|n| n.checked_add(4 * SCRATCH))
            .ok_or_else(|| l.verification())?;
        Ok(Self {
            payload,
            model_bytes,
        })
    }
    pub fn scope(self, l: &OperationLimits) -> Result<OperationLimits, RecoveryError> {
        l.with_retained_memory(self.model_bytes)
    }
    pub fn frame(self, bytes: u64, l: &OperationLimits) -> Result<(), RecoveryError> {
        l.quota(bytes, self.payload, "restore frame schedule")
    }
    pub fn next_frame(
        self,
        previous: u64,
        bytes: u64,
        l: &OperationLimits,
    ) -> Result<u64, RecoveryError> {
        self.frame(bytes, l)?;
        let next = previous.checked_add(1).ok_or_else(|| l.verification())?;
        l.quota(next, MAX_FRAMES, "restore frame count")?;
        Ok(next)
    }
}

/// Exact bounded counting/hash pass over a borrow; never allocates payload Vec.
/// Caller must first establish Caps::scope for all retained model allocations.
pub(crate) fn measure<T: Serialize + ?Sized>(
    value: &T,
    cap: u64,
    l: &OperationLimits,
) -> Result<(u64, [u8; 32]), RecoveryError> {
    measure_prefixed(value, cap, &[], l)
}
/// Streaming append's first pass uses the existing chained previous digest.
/// Exact payload length excludes the prefix; returned digest includes it.
pub(crate) fn measure_prefixed<T: Serialize + ?Sized>(
    value: &T,
    cap: u64,
    prefix: &[u8],
    l: &OperationLimits,
) -> Result<(u64, [u8; 32]), RecoveryError> {
    l.check()?;
    l.admit_memory(SCRATCH)?;
    struct Sink<'a> {
        bytes: u64,
        cap: u64,
        hash: Sha256,
        limits: &'a OperationLimits,
        failure: Option<RecoveryError>,
    }
    impl Write for Sink<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let result = self
                .limits
                .check()
                .and_then(|_| {
                    self.bytes
                        .checked_add(bytes.len() as u64)
                        .ok_or_else(|| self.limits.verification())
                })
                .and_then(|n| {
                    self.limits
                        .quota(n, self.cap, "restore metadata bytes")
                        .map(|_| n)
                });
            match result {
                Ok(n) => {
                    self.bytes = n;
                    self.hash.update(bytes);
                    Ok(bytes.len())
                }
                Err(error) => {
                    self.failure = Some(error);
                    Err(std::io::Error::other("restore metadata bound"))
                }
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hash = Sha256::new();
    hash.update(prefix);
    let mut sink = Sink {
        bytes: 0,
        cap,
        hash,
        limits: l,
        failure: None,
    };
    serde_json::to_writer(&mut sink, value)
        .map_err(|_| sink.failure.take().unwrap_or_else(|| l.verification()))?;
    Ok((sink.bytes, sink.hash.finalize().into()))
}

/// Capacity checks inspect caller-owned values before any clone. Every length,
/// capacity and node count is finite and derived from the persisted frame cap,
/// rather than an unrelated64-entry/64KiB profile restriction. A Vec allocation
/// may retain spare capacity; this actual heap allowance is charged as well.
pub(crate) struct Shape<'a> {
    cap: u64,
    heap: u64,
    l: &'a OperationLimits,
}
impl<'a> Shape<'a> {
    pub fn new(caps: Caps, l: &'a OperationLimits) -> Self {
        Self {
            cap: caps.payload,
            heap: 0,
            l,
        }
    }
    fn add(&mut self, n: u64) -> Result<(), RecoveryError> {
        self.l.check()?;
        self.heap = self
            .heap
            .checked_add(n)
            .ok_or_else(|| self.l.verification())?;
        self.l.quota(
            self.heap,
            self.cap
                .checked_mul(64)
                .ok_or_else(|| self.l.verification())?,
            "restore model capacity",
        )
    }
    pub fn text(&mut self, text: &String) -> Result<(), RecoveryError> {
        self.l
            .quota(text.capacity() as u64, self.cap, "restore string capacity")?;
        self.add(text.capacity() as u64)
    }
    pub fn vec<T>(&mut self, values: &Vec<T>) -> Result<(), RecoveryError> {
        self.l
            .quota(values.len() as u64, self.cap, "restore collection count")?;
        self.l.quota(
            values.capacity() as u64,
            self.cap,
            "restore collection capacity",
        )?;
        self.add(
            (values.capacity() as u64)
                .checked_mul(size_of::<T>() as u64)
                .ok_or_else(|| self.l.verification())?,
        )
    }
    pub fn dependencies(
        &mut self,
        values: &Vec<super::profiles::ReferenceDependency>,
    ) -> Result<(), RecoveryError> {
        self.vec(values)?;
        for value in values {
            self.text(&value.reference_sha256)?;
            self.text(&value.catalog_identity)?;
        }
        Ok(())
    }
    pub fn resolutions(
        &mut self,
        values: &Vec<super::readiness::DependencyResolution>,
    ) -> Result<(), RecoveryError> {
        self.vec(values)?;
        for value in values {
            self.text(&value.reference_sha256)?;
            self.text(&value.catalog_identity)?;
            self.text(&value.evidence_sha256)?;
        }
        Ok(())
    }
    pub fn profile(&mut self, profile: &ValidationProfile) -> Result<(), RecoveryError> {
        self.text(&profile.catalog_identity)?;
        self.dependencies(&profile.dependencies)?;
        self.vec(&profile.preexisting_exceptions)?;
        for value in &profile.preexisting_exceptions {
            self.text(&value.reference_sha256)?;
            self.text(&value.reason)?;
        }
        self.vec(&profile.reference_rules)?;
        for value in &profile.reference_rules {
            self.text(&value.source_collection)?;
            self.text(&value.pointer)?;
            self.text(&value.target_collection)?;
        }
        self.vec(&profile.probes)?;
        for probe in &profile.probes {
            match probe {
                QueryProbe::Point { collection, id } | QueryProbe::Absent { collection, id } => {
                    self.text(collection)?;
                    self.text(id)?;
                }
                QueryProbe::Scan { collection } => self.text(collection)?,
                QueryProbe::Query {
                    collection,
                    field,
                    value,
                } => {
                    self.text(collection)?;
                    self.text(field)?;
                    self.text(value)?;
                }
                QueryProbe::Lexical {
                    collection, query, ..
                } => {
                    self.text(collection)?;
                    self.text(query)?;
                }
                QueryProbe::Vector {
                    collection, vector, ..
                } => {
                    self.text(collection)?;
                    self.vec(vector)?;
                }
            }
        }
        Ok(())
    }
    pub fn path(&mut self, value: &std::path::PathBuf) -> Result<(), RecoveryError> {
        self.l
            .quota(value.capacity() as u64, self.cap, "restore path capacity")?;
        self.add(value.capacity() as u64)
    }
    pub fn lease(
        &mut self,
        lease: &super::admission::OfflineLeaseRequest,
    ) -> Result<(), RecoveryError> {
        self.text(&lease.catalog_identity)?;
        self.text(&lease.profile_sha256)?;
        self.text(&lease.operator)?;
        self.text(&lease.exclusion.evidence)?;
        self.text(&lease.build.package_version)?;
        for value in [&lease.build.source_revision, &lease.build.executable_sha256] {
            if let super::manifest::Availability::Available(value) = value {
                self.text(value)?;
            }
        }
        // PathBuf spare capacity is real memory too. The lease path is borrowed.
        self.l.quota(
            lease.physical_path.capacity() as u64,
            self.cap,
            "restore path capacity",
        )?;
        self.add(lease.physical_path.capacity() as u64)?;
        if let super::admission::FileBinding::Readable { sha256, .. } = &lease.expected_file {
            self.text(sha256)?;
        }
        if let super::admission::OfflineAction::Restore { bundle_sha256, .. } = &lease.action {
            self.text(bundle_sha256)?;
        }
        Ok(())
    }
    pub fn reconciliation(
        &mut self,
        record: &super::readiness::ReconciliationRecord,
    ) -> Result<(), RecoveryError> {
        self.text(&record.image_sha256)?;
        self.text(&record.operator)?;
        self.text(&record.source_publication_resolution_sha256)?;
        self.text(&record.clients.evidence_sha256)?;
        for values in [&record.affected_scopes, &record.unresolved_items] {
            self.vec(values)?;
            for value in values {
                self.text(value)?;
            }
        }
        self.vec(&record.dependencies)?;
        for value in &record.dependencies {
            self.text(&value.reference_sha256)?;
            self.text(&value.catalog_identity)?;
            self.text(&value.evidence_sha256)?;
        }
        self.vec(&record.external_outcomes)?;
        for value in &record.external_outcomes {
            self.text(&value.scope)?;
            match &value.resolution {
                super::readiness::EffectResolution::Unknown => {}
                super::readiness::EffectResolution::Verified {
                    evidence_sha256, ..
                }
                | super::readiness::EffectResolution::ReconciledBeyondHorizon {
                    evidence_sha256,
                    ..
                } => self.text(evidence_sha256)?,
            }
        }
        Ok(())
    }
    pub fn report(&mut self, report: &super::VerificationReport) -> Result<(), RecoveryError> {
        for value in [
            &report.operation_id,
            &report.image_sha256,
            &report.logical_sha256,
            &report.profile_sha256,
        ] {
            self.text(value)?;
        }
        self.text(&report.verifier_build_identity.package_version)?;
        for value in [
            &report.verifier_build_identity.source_revision,
            &report.verifier_build_identity.executable_sha256,
        ] {
            if let super::manifest::Availability::Available(value) = value {
                self.text(value)?;
            }
        }
        if let Some(copy) = &report.copy_evidence {
            self.text(&copy.image_sha256)?;
        }
        Ok(())
    }
    pub fn absence(
        &mut self,
        record: &super::readiness::AbsenceReconciliationRecord,
    ) -> Result<(), RecoveryError> {
        self.text(&record.candidate_sha256)?;
        self.text(&record.operator)?;
        self.text(&record.evidence_sha256)
    }
    pub fn readiness_scratch(&mut self, dependency_count: usize) -> Result<(), RecoveryError> {
        self.l
            .quota(dependency_count as u64, self.cap, "restore readiness count")?;
        self.add(
            (dependency_count as u64)
                .checked_mul(size_of::<&super::readiness::DependencyResolution>() as u64)
                .and_then(|n| n.checked_add(4096))
                .ok_or_else(|| self.l.verification())?,
        )
    }
    pub fn finish(
        self,
        copies: u64,
        l: &OperationLimits,
    ) -> Result<OperationLimits, RecoveryError> {
        // Charge original caller capacity AND every simultaneously live clone,
        // even if clone() happens to shrink unused Vec/String capacity.
        let bytes = self
            .heap
            .checked_mul(copies)
            .ok_or_else(|| l.verification())?;
        l.with_retained_memory(bytes)
    }
}

/// JSON preflight runs on already frame-capped/precharged payload bytes BEFORE
/// owned deserialization. It allocates no user arrays/objects or key Strings.
/// serde_json may use escaped-string scratch, covered by Caps::model_bytes.
pub(crate) fn preflight_json(
    bytes: &[u8],
    caps: Caps,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    caps.frame(bytes.len() as u64, l)?;
    l.admit_memory(SCRATCH)?;
    struct Budget {
        nodes: u64,
        cap: u64,
    }
    struct Seed<'a> {
        budget: &'a mut Budget,
        depth: usize,
    }
    impl<'de> DeserializeSeed<'de> for Seed<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            if self.depth > MAX_DEPTH || self.budget.nodes >= self.budget.cap {
                return Err(serde::de::Error::custom("restore metadata shape"));
            }
            self.budget.nodes += 1;
            d.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for Seed<'_> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("bounded restore metadata")
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<(), E> {
            if value.len() as u64 > self.budget.cap {
                Err(E::custom("restore metadata string"))
            } else {
                Ok(())
            }
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
            while sequence
                .next_element_seed(Seed {
                    budget: self.budget,
                    depth: self.depth + 1,
                })?
                .is_some()
            {}
            Ok(())
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while map
                .next_key_seed(Seed {
                    budget: self.budget,
                    depth: self.depth + 1,
                })?
                .is_some()
            {
                map.next_value_seed(Seed {
                    budget: self.budget,
                    depth: self.depth + 1,
                })?;
            }
            Ok(())
        }
    }
    let mut budget = Budget {
        nodes: 0,
        cap: caps.payload,
    };
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    Seed {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(&mut deserializer)
    .map_err(|_| l.verification())?;
    deserializer.end().map_err(|_| l.verification())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::readiness::DependencyResolution;
    use crate::recovery::{CancelToken, ProfileKind};
    use std::time::{Duration, Instant};
    fn limits(memory: u64) -> OperationLimits {
        OperationLimits::new(
            Instant::now() + Duration::from_secs(10),
            128 << 20,
            memory,
            CancelToken::new(),
        )
    }
    fn caps(l: &OperationLimits) -> Caps {
        Caps::new((1 << 20) - 65536, l).unwrap()
    }
    #[test]
    fn finite_frame_schedule_has_explicit_byte_and_count_boundaries() {
        let l = limits(64 << 20);
        let c = caps(&l);
        assert_eq!(c.payload, 10168);
        assert_eq!(5 + 12 * 4 + 4 * 4, 69);
        const { assert!(WRITER_MAX_FRAMES <= MAX_FRAMES) };
        assert!(MAX_FRAMES * (c.payload + FRAME_HEADER) <= (1 << 20) - 65536);
        c.next_frame(MAX_FRAMES - 1, c.payload, &l).unwrap();
        assert!(matches!(
            c.next_frame(MAX_FRAMES, c.payload, &l),
            Err(RecoveryError::Quota { .. })
        ));
        assert!(matches!(
            c.next_frame(0, c.payload + 1, &l),
            Err(RecoveryError::Quota { .. })
        ));
    }
    #[test]
    fn borrowed_serializer_refuses_the_exact_one_byte_overrun_and_preserves_hash_chain() {
        let l = limits(64 << 20);
        let value = vec!["entry"; 128];
        let encoded = serde_json::to_vec(&value).unwrap();
        let prefix = [19; 32];
        let (bytes, digest) = measure_prefixed(&value, encoded.len() as u64, &prefix, &l).unwrap();
        let mut expected = Sha256::new();
        expected.update(prefix);
        expected.update(&encoded);
        let expected: [u8; 32] = expected.finalize().into();
        assert_eq!(digest, expected);
        assert_eq!(bytes, encoded.len() as u64);
        assert!(matches!(
            measure(&value, bytes - 1, &l),
            Err(RecoveryError::Quota { .. })
        ));
        let cancelled = limits(64 << 20);
        cancelled.cancel.cancel();
        assert!(matches!(
            measure(&value, bytes, &cancelled),
            Err(RecoveryError::Cancelled { .. })
        ));
    }
    #[test]
    fn caller_capacity_is_checked_even_when_lengths_are_empty() {
        let l = limits(64 << 20);
        let c = caps(&l);
        let empty = String::with_capacity(c.payload as usize + 1);
        assert!(empty.is_empty());
        assert!(matches!(
            Shape::new(c, &l).text(&empty),
            Err(RecoveryError::Quota { .. })
        ));
        let empty: Vec<DependencyResolution> = Vec::with_capacity(c.payload as usize + 1);
        assert!(empty.is_empty());
        assert!(matches!(
            Shape::new(c, &l).resolutions(&empty),
            Err(RecoveryError::Quota { .. })
        ));
        let path = std::path::PathBuf::with_capacity(c.payload as usize + 1);
        assert!(matches!(
            Shape::new(c, &l).path(&path),
            Err(RecoveryError::Quota { .. })
        ));
    }
    #[test]
    fn combined_profiles_and_available_resolutions_are_reserved_before_any_clone() {
        let l = limits(64 << 20);
        let c = caps(&l);
        let selected = ValidationProfile::new(ProfileKind::SmallEmpty, "selected fixture");
        let original = ValidationProfile::new(ProfileKind::SmallEmpty, "original fixture");
        let available = vec![DependencyResolution {
            reference_sha256: "d".repeat(64),
            catalog_identity: "other store".into(),
            observed_sequence: 7,
            evidence_sha256: "e".repeat(64),
        }];
        let shape = || {
            let mut s = Shape::new(c, &l);
            s.profile(&selected).unwrap();
            s.profile(&original).unwrap();
            s.resolutions(&available).unwrap();
            s
        };
        let exact = shape().heap * 3;
        assert!(exact > 0);
        let boundary = limits(exact);
        shape().finish(3, &boundary).unwrap();
        assert_eq!(boundary.admitted_memory_bytes(), exact);
        let too_small = limits(exact - 1);
        assert!(matches!(
            shape().finish(3, &too_small),
            Err(RecoveryError::Quota { .. })
        ));
        assert_eq!(
            too_small.admitted_memory_bytes(),
            0,
            "failed pre-admission must not record successful allocation allowance"
        );
    }
    #[test]
    fn many_small_entries_and_nested_text_are_frame_bounded_before_persisting() {
        let l = limits(64 << 20);
        let c = caps(&l);
        let mut profile = ValidationProfile::new(ProfileKind::SmallEmpty, "fixture");
        for _ in 0..256 {
            profile
                .dependencies
                .push(super::super::profiles::ReferenceDependency {
                    reference_sha256: "d".repeat(64),
                    catalog_identity: "x".into(),
                    required_sequence: Some(7),
                    policy: super::super::profiles::DependencyPolicy::ReconcileBeforeAdmission,
                });
        }
        assert!(matches!(
            measure(&profile, c.payload, &l),
            Err(RecoveryError::Quota { .. })
        ));
        profile.dependencies.clear();
        profile
            .reference_rules
            .push(super::super::profiles::ReferenceRule {
                source_collection: "x".into(),
                pointer: "x".repeat(c.payload as usize + 1),
                target_collection: "y".into(),
            });
        assert!(matches!(
            Shape::new(c, &l).profile(&profile),
            Err(RecoveryError::Quota { .. })
        ));
    }
    #[test]
    fn preflight_enforces_depth_and_byte_bounds_before_owned_deserialization() {
        let l = limits(64 << 20);
        let c = caps(&l);
        preflight_json(br#"{"ok":[1,true,null,"text"]}"#, c, &l).unwrap();
        let overdepth = format!(
            "{}0{}",
            "[".repeat(MAX_DEPTH + 2),
            "]".repeat(MAX_DEPTH + 2)
        );
        assert!(preflight_json(overdepth.as_bytes(), c, &l).is_err());
        let oversized = format!("\"{}\"", "x".repeat(c.payload as usize));
        assert!(matches!(
            preflight_json(oversized.as_bytes(), c, &l),
            Err(RecoveryError::Quota { .. })
        ));
        assert!(preflight_json(br#"{"x":[1,}"#, c, &l).is_err());
    }
}
