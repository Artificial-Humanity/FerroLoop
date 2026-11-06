//! Unavailable Linux local admission facade. Descriptors do not prove emitter closure.
use crate::RedbStore;
use fl_core::StoreError;
use fl_core::wire_delivery::{
    ConnectionId, InstanceId, LifetimeRegistration, WireDeliveryStore, WorkerId,
};
use std::os::fd::OwnedFd;

/// No public, serialized, cloned, or feature-gated candidate constructor exists.
///
/// ```compile_fail
/// let candidate = fl_store::wire_delivery_lifetime::LocalAdmissionInput { _private: () };
/// ```
pub struct LocalAdmissionInput {
    _private: (),
}

#[derive(Debug, thiserror::Error)]
pub enum LocalAdmissionError {
    #[error("local lifetime admission: unsupported backend")]
    UnsupportedBackend,
    #[error("local lifetime admission: unproven emitter scope")]
    Unproven,
    #[error("local lifetime admission: capacity")]
    Capacity,
    #[error("local lifetime admission: identity conflict")]
    IdentityConflict,
    #[error("local lifetime admission: store error")]
    Store(#[source] StoreError),
}

impl From<LocalAdmissionError> for StoreError {
    fn from(error: LocalAdmissionError) -> Self {
        // Never render nested storage diagnostics or treat a string as authority.
        StoreError::Backend(error.to_string())
    }
}

pub struct LocalWorkerBinding {
    pub instance: InstanceId,
    pub worker: WorkerId,
    pub connection: ConnectionId,
}
pub struct BrokerScopeHandles {
    pub home_dir: OwnedFd,
    pub worker_pidfd: OwnedFd,
    pub containment: OwnedFd,
    pub creation_control: OwnedFd,
    pub egress: Vec<OwnedFd>,
}
pub struct HookScopeHandles {
    pub home_dir: OwnedFd,
    pub peer_socket: OwnedFd,
    pub containment: OwnedFd,
    pub creation_control: OwnedFd,
    pub egress: Vec<OwnedFd>,
}
impl LocalAdmissionInput {
    pub fn broker_owned(
        _binding: LocalWorkerBinding,
        _handles: BrokerScopeHandles,
    ) -> Result<Self, LocalAdmissionError> {
        Err(LocalAdmissionError::Unproven)
    }
    pub fn hook_peer(
        _binding: LocalWorkerBinding,
        _handles: HookScopeHandles,
    ) -> Result<Self, LocalAdmissionError> {
        Err(LocalAdmissionError::Unproven)
    }
}
pub trait LocalLifetimeAdmissionStore: WireDeliveryStore {
    fn admit_local_lifetime(
        &self,
        input: LocalAdmissionInput,
    ) -> Result<LifetimeRegistration, LocalAdmissionError>;
    fn close_local_lifetime(
        &self,
        registration: &LifetimeRegistration,
    ) -> Result<(), LocalAdmissionError>;
}
impl LocalLifetimeAdmissionStore for RedbStore {
    fn admit_local_lifetime(
        &self,
        _input: LocalAdmissionInput,
    ) -> Result<LifetimeRegistration, LocalAdmissionError> {
        Err(LocalAdmissionError::Unproven)
    }
    fn close_local_lifetime(
        &self,
        _registration: &LifetimeRegistration,
    ) -> Result<(), LocalAdmissionError> {
        Err(LocalAdmissionError::Unproven)
    }
}

#[cfg(test)]
pub(crate) fn assert_preparatory_admit_refusal<S: LocalLifetimeAdmissionStore + ?Sized>(
    store: &S,
    remote: bool,
) {
    tests::assert_preparatory_admit_refusal(store, remote);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe_preparatory_writes;
    pub(super) fn assert_preparatory_admit_refusal<S: LocalLifetimeAdmissionStore + ?Sized>(
        store: &S,
        remote: bool,
    ) {
        let (result, attempts) = observe_preparatory_writes(|| {
            store.admit_local_lifetime(LocalAdmissionInput { _private: () })
        });
        assert_eq!(attempts, 0);
        assert!(if remote {
            matches!(result, Err(LocalAdmissionError::UnsupportedBackend))
        } else {
            matches!(result, Err(LocalAdmissionError::Unproven))
        });
    }
    fn fd() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }
    fn binding() -> LocalWorkerBinding {
        LocalWorkerBinding {
            instance: InstanceId([1; 32]),
            worker: WorkerId([2; 32]),
            connection: ConnectionId([3; 32]),
        }
    }
    #[test]
    fn managed_local_admission_preparatory_redb_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("test.redb")).unwrap();
        assert_preparatory_admit_refusal(&store, false);
        let (result, writes) = observe_preparatory_writes(|| {
            LocalAdmissionInput::broker_owned(
                binding(),
                BrokerScopeHandles {
                    home_dir: fd(),
                    worker_pidfd: fd(),
                    containment: fd(),
                    creation_control: fd(),
                    egress: vec![fd()],
                },
            )
        });
        assert_eq!(writes, 0);
        assert!(matches!(result, Err(LocalAdmissionError::Unproven)));
        let (result, writes) = observe_preparatory_writes(|| {
            LocalAdmissionInput::hook_peer(
                binding(),
                HookScopeHandles {
                    home_dir: fd(),
                    peer_socket: fd(),
                    containment: fd(),
                    creation_control: fd(),
                    egress: vec![fd()],
                },
            )
        });
        assert_eq!(writes, 0);
        assert!(matches!(result, Err(LocalAdmissionError::Unproven)));
        let secret = StoreError::Backend("secret diagnostic".into());
        let redacted: StoreError = LocalAdmissionError::Store(secret).into();
        assert!(!redacted.to_string().contains("secret"));
        drop(store);
        let store = RedbStore::open(&dir.path().join("test.redb")).unwrap();
        assert_preparatory_admit_refusal(&store, false);
    }
}
