//! Fail-closed, process-local permit and revocation state machine.
//!
//! This module alone is **not** a deployed authority: it has no authenticated
//! host transport, ledger verifier, or broker singleton. The embedding host must
//! install an authenticated source and restrict completion to the permit holder.
//! `UnavailableSource` is the production default until those boundaries exist.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::GateError;
use ring::rand::{SecureRandom, SystemRandom};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeKey {
    Secret(String),
    Bundle(String),
    Incarnation(String),
}
impl ScopeKey {
    fn valid(&self) -> bool {
        let s = match self {
            Self::Secret(s) | Self::Bundle(s) | Self::Incarnation(s) => s,
        };
        !s.trim().is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmitRequest {
    pub secret: String,
    pub bundle: Option<String>,
    pub incarnation: Option<String>,
}
impl AdmitRequest {
    pub fn secret(name: &str) -> Self {
        Self {
            secret: name.into(),
            bundle: None,
            incarnation: None,
        }
    }
    fn required_scopes(&self) -> Option<BTreeSet<ScopeKey>> {
        let valid =
            |s: &str| !s.trim().is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control);
        if !valid(&self.secret)
            || self.bundle.as_deref().is_some_and(|s| !valid(s))
            || self.incarnation.as_deref().is_some_and(|s| !valid(s))
        {
            return None;
        }
        let mut out = BTreeSet::from([ScopeKey::Secret(self.secret.clone())]);
        match (&self.bundle, &self.incarnation) {
            (Some(bundle), Some(incarnation)) => {
                out.insert(ScopeKey::Bundle(bundle.clone()));
                out.insert(ScopeKey::Incarnation(incarnation.clone()));
            }
            (None, None) => {}
            _ => return None,
        }
        out.iter().all(ScopeKey::valid).then_some(out)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PermitId([u8; 16]);
impl PermitId {
    /// Canonical transport encoding; possession of this identifier is not authorization.
    pub fn to_wire(&self) -> String {
        use std::fmt::Write;
        let mut out = String::with_capacity(32);
        for b in self.0 {
            write!(out, "{b:02x}").expect("writing to String cannot fail");
        }
        out
    }
    /// Parse exactly 32 lowercase hexadecimal digits.
    pub fn from_wire(input: &str) -> Option<Self> {
        let bytes = input.as_bytes();
        if bytes.len() != 32
            || !bytes
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        {
            return None;
        }
        let mut out = [0; 16];
        for (dst, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            let hex = |b: u8| {
                if b.is_ascii_digit() {
                    b - b'0'
                } else {
                    b - b'a' + 10
                }
            };
            *dst = (hex(pair[0]) << 4) | hex(pair[1]);
        }
        Some(Self(out))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerIdentity {
    pub pid: u32,
    pub start_identity: u64,
}
/// A termination observation authenticated by the configured trusted host verifier.
/// This value is evidence only when returned by that verifier; caller-supplied values
/// must never be accepted by a host transport as proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTermination {
    pub worker: WorkerIdentity,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permit {
    pub id: PermitId,
    pub deadline_mono_ns: u64,
    pub record_epoch: String,
    pub resources: Vec<String>,
}
impl Permit {
    pub fn remaining(&self, clock: &dyn MonotonicClock) -> Result<Duration, GateError> {
        let remaining = self
            .deadline_mono_ns
            .checked_sub(clock.now_ns()?)
            .filter(|n| *n > 0)
            .ok_or(GateError::DispatchAuthority("permit expired"))?;
        Ok(Duration::from_nanos(remaining))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAdmission {
    /// Authenticated state revision; all changes to grant/revocation state must advance it.
    pub generation: u64,
    pub scope: BTreeSet<ScopeKey>,
    pub record_epoch: String,
    pub resources: Vec<String>,
    /// Obtained and authenticated by the host, never from request text.
    pub worker: WorkerIdentity,
}
/// Trusted host adapter. It must authenticate all evidence, expand aliases, ensure
/// revisions cannot be replayed, and fence pre-crash grants on every reconcile.
/// This trait is a trust boundary, not a public proof that values are authenticated.
pub trait AuthoritySource: Send + Sync {
    fn reconcile(&self) -> Result<u64, GateError>;
    fn verify_admission(&self, request: &AdmitRequest) -> Result<VerifiedAdmission, GateError>;
    fn resolve_revocation(
        &self,
        requested: &BTreeSet<ScopeKey>,
    ) -> Result<(u64, BTreeSet<ScopeKey>), GateError>;
    fn persist_fence(&self, scope: &BTreeSet<ScopeKey>, reason: &str) -> Result<(), GateError>;
    fn worker_terminated(
        &self,
        worker: &WorkerIdentity,
    ) -> Result<Option<VerifiedTermination>, GateError>;
    /// Request cancellation only; success is not a quiescence receipt.
    fn request_cancellation(&self, id: &PermitId, worker: &WorkerIdentity)
    -> Result<(), GateError>;
}
#[derive(Debug, Default)]
pub struct UnavailableSource;
impl AuthoritySource for UnavailableSource {
    fn reconcile(&self) -> Result<u64, GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
    fn verify_admission(&self, _: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
    fn resolve_revocation(
        &self,
        _: &BTreeSet<ScopeKey>,
    ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
    fn persist_fence(&self, _: &BTreeSet<ScopeKey>, _: &str) -> Result<(), GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
    fn worker_terminated(
        &self,
        _: &WorkerIdentity,
    ) -> Result<Option<VerifiedTermination>, GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
    fn request_cancellation(&self, _: &PermitId, _: &WorkerIdentity) -> Result<(), GateError> {
        Err(GateError::DispatchAuthority("authority unavailable"))
    }
}
pub trait MonotonicClock: Send + Sync {
    fn now_ns(&self) -> Result<u64, GateError>;
}
#[derive(Debug, Default)]
pub struct HostMonotonicClock;
#[cfg(unix)]
impl MonotonicClock for HostMonotonicClock {
    fn now_ns(&self) -> Result<u64, GateError> {
        let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: clock_gettime initializes `value` on success and pointer is valid.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, value.as_mut_ptr()) } != 0 {
            return Err(GateError::DispatchAuthority("monotonic clock unavailable"));
        }
        // SAFETY: successful clock_gettime initialized the timespec.
        let value = unsafe { value.assume_init() };
        let secs = u64::try_from(value.tv_sec)
            .map_err(|_| GateError::DispatchAuthority("invalid monotonic clock"))?;
        let nanos = u64::try_from(value.tv_nsec)
            .map_err(|_| GateError::DispatchAuthority("invalid monotonic clock"))?;
        if nanos >= 1_000_000_000 {
            return Err(GateError::DispatchAuthority("invalid monotonic clock"));
        }
        secs.checked_mul(1_000_000_000)
            .and_then(|s| s.checked_add(nanos))
            .ok_or(GateError::DispatchAuthority("monotonic clock overflow"))
    }
}
#[cfg(not(unix))]
impl MonotonicClock for HostMonotonicClock {
    fn now_ns(&self) -> Result<u64, GateError> {
        Err(GateError::DispatchAuthority(
            "host monotonic clock unsupported",
        ))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityHealth {
    Sealed,
    Ready,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeAck {
    Complete,
    PendingUnconfirmed,
}
pub trait DispatchAuthority: Send + Sync {
    fn admit(&self, request: &AdmitRequest) -> Result<Permit, GateError>;
    fn complete(&self, id: &PermitId) -> Result<(), GateError>;
    fn revoke(
        &self,
        scope: &BTreeSet<ScopeKey>,
        reason: &str,
        wait: Duration,
    ) -> Result<RevokeAck, GateError>;
    fn health(&self) -> Result<AuthorityHealth, GateError>;
    fn reconcile(&self) -> Result<(), GateError>;
}
#[derive(Debug)]
struct LivePermit {
    scope: BTreeSet<ScopeKey>,
    worker: WorkerIdentity,
    deadline_mono_ns: u64,
    cancellation_requested: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FencePhase {
    Pending,
    Persisting,
    Persisted,
    Failed,
}
#[derive(Debug)]
struct Fence {
    scope: BTreeSet<ScopeKey>,
    reason: String,
    phase: FencePhase,
}
#[derive(Default)]
struct State {
    generation: Option<u64>,
    reconciled: bool,
    /// A revocation whose scope could not be established must never be erased by
    /// an in-progress initial reconciliation.
    revocation_failed: bool,
    live: BTreeMap<PermitId, LivePermit>,
    fences: Vec<Fence>,
}
/// A pure in-process kernel; singleton hosting, authenticated transport, and durable
/// source are deliberately not implemented here. Starts sealed after every restart.
pub struct BrokerState {
    state: Mutex<State>,
    changed: Condvar,
    source: Arc<dyn AuthoritySource>,
    clock: Arc<dyn MonotonicClock>,
    operation_ns: u64,
}
impl BrokerState {
    pub fn unavailable() -> Self {
        Self::new(
            Arc::new(UnavailableSource),
            Arc::new(HostMonotonicClock),
            Duration::from_secs(30),
        )
        .expect("valid default")
    }
    pub fn new(
        source: Arc<dyn AuthoritySource>,
        clock: Arc<dyn MonotonicClock>,
        timeout: Duration,
    ) -> Result<Self, GateError> {
        let operation_ns = u64::try_from(timeout.as_nanos())
            .ok()
            .filter(|n| *n > 0)
            .ok_or(GateError::DispatchAuthority("invalid operation timeout"))?;
        Ok(Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            source,
            clock,
            operation_ns,
        })
    }
    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, GateError> {
        self.state
            .lock()
            .map_err(|_| GateError::DispatchAuthority("authority poisoned"))
    }
    fn ready_to_persist(s: &mut State) -> Vec<(usize, BTreeSet<ScopeKey>, String)> {
        let mut ready = Vec::new();
        for (index, fence) in s.fences.iter_mut().enumerate() {
            if fence.phase == FencePhase::Pending
                && !s.live.values().any(|p| !p.scope.is_disjoint(&fence.scope))
            {
                fence.phase = FencePhase::Persisting;
                ready.push((index, fence.scope.clone(), fence.reason.clone()));
            }
        }
        ready
    }
    fn persist_ready(
        &self,
        ready: Vec<(usize, BTreeSet<ScopeKey>, String)>,
    ) -> Result<(), GateError> {
        let mut failed = false;
        for (index, scope, reason) in ready {
            // No broker lock is held during external storage work.
            let ok = self.source.persist_fence(&scope, &reason).is_ok();
            let mut state = self.state()?;
            state.fences[index].phase = if ok {
                FencePhase::Persisted
            } else {
                failed = true;
                FencePhase::Failed
            };
            self.changed.notify_all();
        }
        if failed {
            Err(GateError::DispatchAuthority(
                "revocation persistence failed",
            ))
        } else {
            Ok(())
        }
    }
    /// Request cancellation of expired workers without treating expiry or a successful
    /// cancellation request as confirmation of quiescence. A host should call this on its
    /// bounded tick; it performs all host I/O outside the state lock.
    pub fn cancel_expired(&self) -> Result<(), GateError> {
        let now = self.clock.now_ns()?;
        let workers = {
            let mut s = self.state()?;
            s.live
                .iter_mut()
                .filter_map(|(id, p)| {
                    if now >= p.deadline_mono_ns && !p.cancellation_requested {
                        p.cancellation_requested = true;
                        Some((id.clone(), p.worker.clone()))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        let mut failed = false;
        for (id, worker) in workers {
            if self.source.request_cancellation(&id, &worker).is_err() {
                failed = true;
                // A later tick may retry a failed cancellation; it is not completion.
                let mut s = self.state()?;
                if let Some(permit) = s.live.get_mut(&id).filter(|p| p.worker == worker) {
                    permit.cancellation_requested = false;
                }
            }
        }
        if failed {
            Err(GateError::DispatchAuthority("cancellation unconfirmed"))
        } else {
            Ok(())
        }
    }
    /// Retry failed durable fences; failed scopes remain denied throughout.
    pub fn retry_failed(&self) -> Result<(), GateError> {
        let ready = {
            let mut s = self.state()?;
            for f in &mut s.fences {
                if f.phase == FencePhase::Failed {
                    f.phase = FencePhase::Pending;
                }
            }
            Self::ready_to_persist(&mut s)
        };
        self.persist_ready(ready)
    }
    /// Ask the trusted host to confirm termination of the exact recorded process instance.
    /// The verifier must compare both PID and start identity, not PID alone.
    pub fn confirm_terminated(&self, id: &PermitId) -> Result<bool, GateError> {
        let worker = {
            let s = self.state()?;
            match s.live.get(id) {
                Some(p) => p.worker.clone(),
                None => return Ok(false),
            }
        };
        if self
            .source
            .worker_terminated(&worker)?
            .is_none_or(|receipt| receipt.worker != worker)
        {
            return Ok(false);
        }
        let ready = {
            let mut s = self.state()?;
            if !s.live.get(id).is_some_and(|p| p.worker == worker) {
                return Ok(false);
            }
            s.live.remove(id);
            Self::ready_to_persist(&mut s)
        };
        self.changed.notify_all();
        self.persist_ready(ready)?;
        Ok(true)
    }
}
impl DispatchAuthority for BrokerState {
    fn admit(&self, req: &AdmitRequest) -> Result<Permit, GateError> {
        let requested = req
            .required_scopes()
            .ok_or(GateError::DispatchAuthority("invalid scope"))?;
        // Refuse a sealed broker without consulting a possibly unavailable source.
        if self.state()?.generation.is_none() {
            return Err(GateError::DispatchAuthority("authority sealed"));
        }
        // Authentication and I/O are deliberately outside the broker mutex.
        let verified = self.source.verify_admission(req)?;
        if verified.generation == 0
            || verified.record_epoch.len() != 64
            || !verified
                .record_epoch
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || verified.resources.len() > 128
            || verified
                .resources
                .iter()
                .any(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control))
            || verified.worker.pid == 0
            || verified.worker.start_identity == 0
            || verified.scope.len() > 128
            || !requested.is_subset(&verified.scope)
            || verified.scope.iter().any(|s| !s.valid())
        {
            return Err(GateError::DispatchAuthority("invalid verified admission"));
        }
        let deadline = self
            .clock
            .now_ns()?
            .checked_add(self.operation_ns)
            .ok_or(GateError::DispatchAuthority("permit deadline overflow"))?;
        let mut id = [0u8; 16];
        SystemRandom::new()
            .fill(&mut id)
            .map_err(|_| GateError::DispatchAuthority("permit randomness unavailable"))?;
        let id = PermitId(id);
        let mut s = self.state()?;
        if s.generation != Some(verified.generation) {
            return Err(GateError::DispatchAuthority("sealed or stale authority"));
        }
        if s.fences
            .iter()
            .any(|f| !f.scope.is_disjoint(&verified.scope))
        {
            return Err(GateError::DispatchAuthority("scope revoked or pending"));
        }
        if s.live.len() >= 4096 {
            return Err(GateError::DispatchAuthority("permit capacity reached"));
        }
        if s.live.contains_key(&id) {
            return Err(GateError::DispatchAuthority("permit collision"));
        }
        if self.clock.now_ns()? >= deadline {
            return Err(GateError::DispatchAuthority("permit expired"));
        }
        s.live.insert(
            id.clone(),
            LivePermit {
                scope: verified.scope,
                worker: verified.worker,
                deadline_mono_ns: deadline,
                cancellation_requested: false,
            },
        );
        Ok(Permit {
            id,
            deadline_mono_ns: deadline,
            record_epoch: verified.record_epoch,
            resources: verified.resources,
        })
    }
    fn complete(&self, id: &PermitId) -> Result<(), GateError> {
        let ready = {
            let mut s = self.state()?;
            s.live.remove(id);
            Self::ready_to_persist(&mut s)
        };
        self.changed.notify_all();
        self.persist_ready(ready)
    }
    fn revoke(
        &self,
        scope: &BTreeSet<ScopeKey>,
        reason: &str,
        wait: Duration,
    ) -> Result<RevokeAck, GateError> {
        if wait > Duration::from_secs(30) {
            return Err(GateError::DispatchAuthority(
                "revocation wait exceeds limit",
            ));
        }
        if scope.is_empty()
            || scope.len() > 128
            || scope.iter().any(|s| !s.valid())
            || reason.trim().is_empty()
            || reason.len() > 1024
            || reason.chars().any(char::is_control)
        {
            return Err(GateError::DispatchAuthority("invalid revocation"));
        }
        let resolved = self.source.resolve_revocation(scope);
        let (generation, expanded) = match resolved {
            Ok(pair) => pair,
            Err(error) => {
                // If aliases cannot be authenticated, do not leave an equivalent scope open.
                let mut state = self.state()?;
                state.generation = None;
                state.revocation_failed = true;
                return Err(error);
            }
        };
        if expanded.is_empty()
            || expanded.len() > 128
            || !scope.is_subset(&expanded)
            || expanded.iter().any(|s| !s.valid())
        {
            let mut state = self.state()?;
            state.generation = None;
            state.revocation_failed = true;
            return Err(GateError::DispatchAuthority("invalid revocation aliases"));
        }
        let (index, ready) = {
            let mut s = self.state()?;
            if s.generation != Some(generation) {
                s.generation = None;
                return Err(GateError::DispatchAuthority("sealed or stale authority"));
            }
            // Capacity exhaustion seals all new work; it never allows a revocation to fail open.
            if s.fences.len() >= 4096 {
                s.generation = None;
                return Err(GateError::DispatchAuthority("revocation capacity reached"));
            }
            // This insertion is the revocation linearization point.
            s.fences.push(Fence {
                scope: expanded,
                reason: reason.into(),
                phase: FencePhase::Pending,
            });
            let index = s.fences.len() - 1;
            let ready = Self::ready_to_persist(&mut s);
            (index, ready)
        };
        self.persist_ready(ready)?;
        let started = Instant::now();
        let mut s = self.state()?;
        loop {
            match s.fences[index].phase {
                FencePhase::Persisted => return Ok(RevokeAck::Complete),
                FencePhase::Failed => {
                    return Err(GateError::DispatchAuthority(
                        "revocation persistence failed",
                    ));
                }
                FencePhase::Pending | FencePhase::Persisting => {}
            }
            let remaining = wait.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(RevokeAck::PendingUnconfirmed);
            }
            let (next, _) = self
                .changed
                .wait_timeout(s, remaining)
                .map_err(|_| GateError::DispatchAuthority("authority poisoned"))?;
            s = next;
        }
    }
    fn health(&self) -> Result<AuthorityHealth, GateError> {
        Ok(if self.state()?.generation.is_some() {
            AuthorityHealth::Ready
        } else {
            AuthorityHealth::Sealed
        })
    }
    fn reconcile(&self) -> Result<(), GateError> {
        // Source must authenticate ledger/health outside this lock and ensure no pre-crash grant reopens.
        let generation = self.source.reconcile()?;
        if generation == 0 {
            return Err(GateError::DispatchAuthority("invalid authority generation"));
        }
        let mut s = self.state()?;
        if s.reconciled
            || s.revocation_failed
            || !s.live.is_empty()
            || !s.fences.is_empty()
            || s.generation.is_some()
        {
            return Err(GateError::DispatchAuthority("authority cannot be reopened"));
        }
        s.generation = Some(generation);
        s.reconciled = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;

    #[derive(Default)]
    struct TestClock(AtomicU64);
    impl MonotonicClock for TestClock {
        fn now_ns(&self) -> Result<u64, GateError> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }
    struct TestSource {
        generation: AtomicU64,
        failed: AtomicBool,
        aliases_failed: AtomicBool,
        cancellation_failed: AtomicBool,
        stopped: AtomicBool,
        cancellations: AtomicU64,
        persisted: Mutex<Vec<BTreeSet<ScopeKey>>>,
    }
    impl Default for TestSource {
        fn default() -> Self {
            Self {
                generation: AtomicU64::new(1),
                failed: AtomicBool::new(false),
                aliases_failed: AtomicBool::new(false),
                cancellation_failed: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                cancellations: AtomicU64::new(0),
                persisted: Mutex::new(vec![]),
            }
        }
    }
    impl AuthoritySource for TestSource {
        fn reconcile(&self) -> Result<u64, GateError> {
            Ok(self.generation.load(Ordering::SeqCst))
        }
        fn verify_admission(&self, req: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            let mut scope = req
                .required_scopes()
                .ok_or(GateError::DispatchAuthority("bad request"))?;
            if req.secret == "KEY" {
                scope.insert(ScopeKey::Secret("KEY_ALIAS".into()));
            }
            Ok(VerifiedAdmission {
                generation: self.generation.load(Ordering::SeqCst),
                scope,
                record_epoch: "a".repeat(64),
                resources: vec!["repo".into()],
                worker: WorkerIdentity {
                    pid: 42,
                    start_identity: 77,
                },
            })
        }
        fn resolve_revocation(
            &self,
            scope: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            if self.aliases_failed.load(Ordering::SeqCst) {
                return Err(GateError::DispatchAuthority("test alias failure"));
            }
            let mut expanded = scope.clone();
            if scope.contains(&ScopeKey::Secret("KEY".into()))
                || scope.contains(&ScopeKey::Secret("KEY_ALIAS".into()))
            {
                expanded.insert(ScopeKey::Secret("KEY".into()));
                expanded.insert(ScopeKey::Secret("KEY_ALIAS".into()));
            }
            Ok((self.generation.load(Ordering::SeqCst), expanded))
        }
        fn persist_fence(&self, scope: &BTreeSet<ScopeKey>, _: &str) -> Result<(), GateError> {
            if self.failed.load(Ordering::SeqCst) {
                return Err(GateError::DispatchAuthority("test persistence"));
            }
            self.persisted.lock().unwrap().push(scope.clone());
            Ok(())
        }
        fn worker_terminated(
            &self,
            worker: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            Ok((worker.pid == 42
                && worker.start_identity == 77
                && self.stopped.load(Ordering::SeqCst))
            .then(|| VerifiedTermination {
                worker: worker.clone(),
            }))
        }
        fn request_cancellation(&self, _: &PermitId, _: &WorkerIdentity) -> Result<(), GateError> {
            self.cancellations.fetch_add(1, Ordering::SeqCst);
            if self.cancellation_failed.load(Ordering::SeqCst) {
                Err(GateError::DispatchAuthority("test cancellation failure"))
            } else {
                Ok(())
            }
        }
    }
    fn broker() -> (Arc<BrokerState>, Arc<TestSource>, Arc<TestClock>) {
        let source = Arc::new(TestSource::default());
        let clock = Arc::new(TestClock::default());
        let broker = Arc::new(
            BrokerState::new(source.clone(), clock.clone(), Duration::from_secs(30)).unwrap(),
        );
        broker.reconcile().unwrap();
        (broker, source, clock)
    }
    fn scope(secret: &str) -> BTreeSet<ScopeKey> {
        BTreeSet::from([ScopeKey::Secret(secret.into())])
    }

    #[test]
    fn permit_id_wire_encoding_is_canonical_and_bounded() {
        let id = PermitId([0xab; 16]);
        assert_eq!(id.to_wire(), "abababababababababababababababab");
        assert_eq!(PermitId::from_wire(&id.to_wire()), Some(id));
        assert_eq!(
            PermitId::from_wire("ABABABABABABABABABABABABABABABAB"),
            None
        );
        assert_eq!(PermitId::from_wire("abab"), None);
        assert_eq!(
            PermitId::from_wire("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"),
            None
        );
    }

    struct BlockingReconcile {
        source: TestSource,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    impl AuthoritySource for BlockingReconcile {
        fn reconcile(&self) -> Result<u64, GateError> {
            let generation = self.source.reconcile()?;
            self.entered.send(()).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
            Ok(generation)
        }
        fn verify_admission(&self, r: &AdmitRequest) -> Result<VerifiedAdmission, GateError> {
            self.source.verify_admission(r)
        }
        fn resolve_revocation(
            &self,
            s: &BTreeSet<ScopeKey>,
        ) -> Result<(u64, BTreeSet<ScopeKey>), GateError> {
            self.source.resolve_revocation(s)
        }
        fn persist_fence(&self, s: &BTreeSet<ScopeKey>, r: &str) -> Result<(), GateError> {
            self.source.persist_fence(s, r)
        }
        fn worker_terminated(
            &self,
            w: &WorkerIdentity,
        ) -> Result<Option<VerifiedTermination>, GateError> {
            self.source.worker_terminated(w)
        }
        fn request_cancellation(&self, id: &PermitId, w: &WorkerIdentity) -> Result<(), GateError> {
            self.source.request_cancellation(id, w)
        }
    }

    #[test]
    fn failed_revoke_during_initial_reconcile_never_unseals() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let source = Arc::new(BlockingReconcile {
            source: TestSource::default(),
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
        });
        source.source.aliases_failed.store(true, Ordering::SeqCst);
        let broker = Arc::new(
            BrokerState::new(
                source,
                Arc::new(TestClock::default()),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        let worker = {
            let broker = broker.clone();
            std::thread::spawn(move || broker.reconcile())
        };
        entered_rx.recv().unwrap();
        assert!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .is_err()
        );
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
    }

    #[test]
    fn unavailable_authority_is_sealed_and_refuses_new_work() {
        let broker = BrokerState::unavailable();
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(broker.reconcile().is_err());
    }
    #[test]
    fn revoke_before_admission_blocks_aliases_but_not_unrelated_scope() {
        let (b, source, _) = broker();
        assert_eq!(
            b.revoke(&scope("KEY_ALIAS"), "owner", Duration::ZERO)
                .unwrap(),
            RevokeAck::Complete
        );
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(b.admit(&AdmitRequest::secret("KEY_ALIAS")).is_err());
        assert!(b.admit(&AdmitRequest::secret("OTHER")).is_ok());
        assert_eq!(source.persisted.lock().unwrap().len(), 1);
    }
    #[test]
    fn admission_before_revoke_stays_unconfirmed_until_completion() {
        let (b, source, clock) = broker();
        let permit = b.admit(&AdmitRequest::secret("KEY")).unwrap();
        assert_eq!(permit.deadline_mono_ns, 30_000_000_000);
        assert_eq!(
            b.revoke(&scope("KEY"), "owner", Duration::ZERO).unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        assert!(b.admit(&AdmitRequest::secret("KEY_ALIAS")).is_err());
        let unrelated = b.admit(&AdmitRequest::secret("OTHER")).unwrap();
        b.complete(&unrelated.id).unwrap();
        clock.0.store(30_000_000_000, Ordering::SeqCst);
        assert!(permit.remaining(clock.as_ref()).is_err());
        b.cancel_expired().unwrap();
        assert_eq!(source.cancellations.load(Ordering::SeqCst), 1);
        assert!(
            source.persisted.lock().unwrap().is_empty(),
            "expiry is not completion"
        );
        b.complete(&permit.id).unwrap();
        b.complete(&permit.id).unwrap();
        assert_eq!(source.persisted.lock().unwrap().len(), 1);
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
    }
    #[test]
    fn failed_fence_persistence_stays_denied_and_can_retry() {
        let (b, source, _) = broker();
        source.failed.store(true, Ordering::SeqCst);
        assert!(b.revoke(&scope("KEY"), "owner", Duration::ZERO).is_err());
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(b.admit(&AdmitRequest::secret("OTHER")).is_ok());
        source.failed.store(false, Ordering::SeqCst);
        b.retry_failed().unwrap();
        assert_eq!(source.persisted.lock().unwrap().len(), 1);
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
    }
    #[test]
    fn exact_worker_termination_can_confirm_quiescence() {
        let (b, source, _) = broker();
        let permit = b.admit(&AdmitRequest::secret("KEY")).unwrap();
        assert_eq!(
            b.revoke(&scope("KEY"), "owner", Duration::ZERO).unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        assert!(!b.confirm_terminated(&permit.id).unwrap());
        source.stopped.store(true, Ordering::SeqCst);
        assert!(b.confirm_terminated(&permit.id).unwrap());
        assert_eq!(source.persisted.lock().unwrap().len(), 1);
    }
    #[test]
    fn stale_verified_generation_does_not_grant() {
        let (b, source, _) = broker();
        source.generation.store(2, Ordering::SeqCst);
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(b.revoke(&scope("KEY"), "owner", Duration::ZERO).is_err());
    }
    #[test]
    fn bundle_admission_requires_both_bundle_and_incarnation_scopes() {
        let (b, _, _) = broker();
        assert!(
            b.admit(&AdmitRequest {
                secret: "KEY".into(),
                bundle: Some("b".into()),
                incarnation: None
            })
            .is_err()
        );
        let req = AdmitRequest {
            secret: "KEY".into(),
            bundle: Some("b".into()),
            incarnation: Some("i".into()),
        };
        let permit = b.admit(&req).unwrap();
        assert_eq!(
            b.revoke(
                &BTreeSet::from([ScopeKey::Incarnation("i".into())]),
                "owner",
                Duration::ZERO
            )
            .unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        assert!(b.admit(&req).is_err());
        b.complete(&permit.id).unwrap();
    }
    #[test]
    fn unresolvable_revocation_seals_new_work() {
        let (broker, source, _) = broker();
        source.aliases_failed.store(true, Ordering::SeqCst);
        assert!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .is_err()
        );
        source.aliases_failed.store(false, Ordering::SeqCst);
        assert_eq!(broker.health().unwrap(), AuthorityHealth::Sealed);
        assert!(
            broker.reconcile().is_err(),
            "failed revocation cannot be reopened"
        );
        assert!(broker.admit(&AdmitRequest::secret("KEY")).is_err());
        assert!(broker.admit(&AdmitRequest::secret("OTHER")).is_err());
    }
    #[test]
    fn cancellation_failure_keeps_permit_and_can_be_retried() {
        let (broker, source, clock) = broker();
        let permit = broker.admit(&AdmitRequest::secret("KEY")).unwrap();
        assert_eq!(
            broker
                .revoke(&scope("KEY"), "owner", Duration::ZERO)
                .unwrap(),
            RevokeAck::PendingUnconfirmed
        );
        clock.0.store(30_000_000_000, Ordering::SeqCst);
        source.cancellation_failed.store(true, Ordering::SeqCst);
        assert!(broker.cancel_expired().is_err());
        assert!(source.persisted.lock().unwrap().is_empty());
        source.cancellation_failed.store(false, Ordering::SeqCst);
        broker.cancel_expired().unwrap();
        assert_eq!(source.cancellations.load(Ordering::SeqCst), 2);
        assert!(source.persisted.lock().unwrap().is_empty());
        broker.complete(&permit.id).unwrap();
        assert_eq!(source.persisted.lock().unwrap().len(), 1);
    }
    #[test]
    fn drain_wait_releases_mutex_and_unrelated_traffic_proceeds() {
        let (b, _, _) = broker();
        let permit = b.admit(&AdmitRequest::secret("KEY")).unwrap();
        let (sent, received) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let clone = b.clone();
        let handle = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            sent.send(clone.revoke(&scope("KEY"), "owner", Duration::from_secs(2)))
                .unwrap();
        });
        ready_rx.recv().unwrap();
        // Wait until revocation's linearization point using observable state, not a sleep.
        let started = Instant::now();
        while !b
            .state()
            .unwrap()
            .fences
            .iter()
            .any(|f| f.scope.contains(&ScopeKey::Secret("KEY".into())))
        {
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::yield_now();
        }
        assert!(b.admit(&AdmitRequest::secret("OTHER")).is_ok());
        assert!(b.admit(&AdmitRequest::secret("KEY")).is_err());
        b.complete(&permit.id).unwrap();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            RevokeAck::Complete
        );
        handle.join().unwrap();
    }
}
