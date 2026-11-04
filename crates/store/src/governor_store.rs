//! Persistent [`GovernorStore`] over an [`AnyStore`] (Record #97).
//!
//! State lives in ONE Runtime state entry (Workspace scope, slug
//! `gate-governor-state`) holding the JSON snapshot of the in-memory
//! governor engine. Every trait call does read-modify-write: load the
//! snapshot, run the pure `MemGovernorStore` logic, write it back.
//!
//! Cross-process limit (CAS finding, Record #97 #4): the state layer
//! offers no revision-checked upsert/CAS. `StateStore::upsert_state`
//! (`UpsertStateArgs`) takes no expected revision/etag, and no other
//! trait method compares a head revision before writing (revisions and
//! conflicts exist only for sync/merge, not as a write precondition).
//! So this RMW cannot be made atomic across processes without inventing
//! an API: an in-process `Mutex` serialises calls on one
//! `PersistentGovernorStore` instance, but two processes (or two
//! instances) over the same store can interleave load/save and lose the
//! loser's update. Use one governor instance per process/store.

use std::sync::Mutex;

use fl_core::governor::{
    AdmissionOutcome, AdmissionRequest, GovernorError, GovernorPolicy, GovernorStore,
    MemGovernorStore, ReplayDecision, ReplayRequest, SettleRequest,
};
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::StateStore;
use fl_core::{At, Permit};

use crate::AnyStore;

pub const GOVERNOR_STATE_SLUG: &str = "gate-governor-state";

/// [`GovernorStore`] persisted in an [`AnyStore`]'s Runtime state.
/// See module docs for the single-process/lock limit.
pub struct PersistentGovernorStore {
    store: AnyStore,
    policy: GovernorPolicy,
    lock: Mutex<()>,
}

impl PersistentGovernorStore {
    pub fn new(store: AnyStore) -> Self {
        Self::with_policy(store, GovernorPolicy::default())
    }
    pub fn with_policy(store: AnyStore, policy: GovernorPolicy) -> Self {
        Self {
            store,
            policy,
            lock: Mutex::new(()),
        }
    }

    /// Missing entry means a fresh governor. A present but corrupt or
    /// unparseable snapshot is `GovernorError::Unavailable`, never a
    /// silent reset to fresh state.
    fn load(&self) -> Result<MemGovernorStore, GovernorError> {
        let entry = self
            .store
            .get_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                GOVERNOR_STATE_SLUG,
            )
            .map_err(|_| GovernorError::Unavailable)?;
        match entry {
            Some(e) => match &e.payload {
                StatePayload::Runtime(r) => {
                    let json = r.value.as_str().ok_or(GovernorError::Unavailable)?;
                    MemGovernorStore::try_from_snapshot_json(json)
                }
                _ => Err(GovernorError::Unavailable),
            },
            None => Ok(MemGovernorStore::new(self.policy.clone())),
        }
    }

    fn save(&self, mem: &MemGovernorStore) -> Result<(), GovernorError> {
        let json = mem.snapshot_json();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: GOVERNOR_STATE_SLUG.to_string(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Runtime(RuntimeEntry {
                    value: serde_json::Value::String(json),
                    merge_mode: MergeMode::LastWriter,
                }),
                at: At::from_unix_millis(now_ms),
                by: "gate-governor".to_string(),
            })
            .map(|_| ())
            .map_err(|e| GovernorError::Invalid(e.to_string()))
    }

    fn with_state<T>(
        &self,
        f: impl FnOnce(&MemGovernorStore) -> Result<T, GovernorError>,
    ) -> Result<T, GovernorError> {
        let _g = self.lock.lock().map_err(|_| GovernorError::Unavailable)?;
        let mem = self.load()?;
        let out = f(&mem)?;
        self.save(&mem)?;
        Ok(out)
    }
}

impl GovernorStore for PersistentGovernorStore {
    fn reserve(
        &self,
        req: &AdmissionRequest,
        now_ms: u64,
    ) -> Result<AdmissionOutcome, GovernorError> {
        self.with_state(|mem| mem.reserve(req, now_ms))
    }
    fn settle(&self, req: &SettleRequest) -> Result<(), GovernorError> {
        self.with_state(|mem| mem.settle(req))
    }
    fn record_and_check_replay(
        &self,
        req: &ReplayRequest,
    ) -> Result<ReplayDecision, GovernorError> {
        self.with_state(|mem| mem.record_and_check_replay(req))
    }
    fn unconfirmed_permits(&self) -> Vec<Permit> {
        // Read-only: no save needed.
        let _g = match self.lock.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        self.load()
            .map(|m| m.unconfirmed_permits())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::governor::{Principal, SettledOutcome};

    fn req(units: u64) -> AdmissionRequest {
        AdmissionRequest::new(Principal::Owner, "proj", "api.example.com/v1", units)
    }

    fn policy1000() -> GovernorPolicy {
        GovernorPolicy {
            budget_units_per_scope: 1000,
            ..Default::default()
        }
    }

    #[test]
    fn corrupt_snapshot_is_unavailable_not_fresh() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fl.redb");
        let store = crate::open_store(&path).unwrap();
        // Plant a corrupt snapshot directly.
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: GOVERNOR_STATE_SLUG.to_string(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Runtime(RuntimeEntry {
                    value: serde_json::Value::String("not-json{{{".into()),
                    merge_mode: MergeMode::LastWriter,
                }),
                at: At::from_unix_millis(1_000),
                by: "test".to_string(),
            })
            .unwrap();
        let gov = PersistentGovernorStore::with_policy(store, policy1000());
        assert_eq!(gov.reserve(&req(1), 1_000), Err(GovernorError::Unavailable));
    }

    #[test]
    fn reserve_persists_across_instances_and_settle_persists() {
        // Two instances over the same file-backed store: a reservation made
        // on one must be visible to a fresh instance (persistence), and a
        // settle through the second must persist to a third.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fl.redb");

        let s1 =
            PersistentGovernorStore::with_policy(crate::open_store(&path).unwrap(), policy1000());
        let permit = match s1.reserve(&req(10), 1_000).unwrap() {
            AdmissionOutcome::Admitted(p) => p,
            other => panic!("expected admitted, got {other:?}"),
        };
        // Fresh instance over the same store sees the reservation:
        // budget 1000 default, reserved 10, so 991 more is denied project-wide? Use unconfirmed.
        let s2 =
            PersistentGovernorStore::with_policy(crate::open_store(&path).unwrap(), policy1000());
        let unconfirmed = s2.unconfirmed_permits();
        assert!(
            unconfirmed.iter().any(|p| p.id == permit.id),
            "new instance must see reservation: {unconfirmed:?}"
        );
        // Settle through s2 persists.
        s2.settle(&SettleRequest {
            permit_id: permit.id.clone(),
            actual_units: 4,
            outcome: SettledOutcome::Executed,
            complete: true,
        })
        .unwrap();
        let s3 =
            PersistentGovernorStore::with_policy(crate::open_store(&path).unwrap(), policy1000());
        assert!(
            !s3.unconfirmed_permits().iter().any(|p| p.id == permit.id),
            "settled permit must not remain unconfirmed on a fresh instance"
        );
        // Budget charge persisted: a fresh MemGovernorStore would have 0 used;
        // reserve 997 (4 used + 997 > 1000) must be denied on endpoint scope? Default
        // budget checks all scopes at 1000: 4+997=1001>1000 => Denied.
        match s3.reserve(&req(997), 2_000).unwrap() {
            AdmissionOutcome::Denied { .. } => {}
            other => panic!("expected denied after persisted charge, got {other:?}"),
        }
    }
}
