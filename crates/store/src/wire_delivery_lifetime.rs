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
        let ((result, rpc), attempts) = observe_preparatory_writes(|| {
            crate::observe_preparatory_rpc(|| {
                store.admit_local_lifetime(LocalAdmissionInput { _private: () })
            })
        });
        assert_eq!(rpc, 0);
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
    #[test]
    fn managed_local_admission_preparatory_ferro_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("facade.fls");
        assert_preparatory_admit_refusal(&crate::FerroStore::in_memory(), false);
        assert_preparatory_admit_refusal(&crate::FerroStore::open(&path).unwrap(), false);
        assert_preparatory_admit_refusal(&crate::FerroStore::open(&path).unwrap(), false);
    }
    #[test]
    fn managed_local_admission_preparatory_any_refusal() {
        let dir = tempfile::tempdir().unwrap();
        assert_preparatory_admit_refusal(
            &crate::AnyStore::from(crate::FerroStore::in_memory()),
            false,
        );
        assert_preparatory_admit_refusal(
            &crate::AnyStore::from(crate::FerroStore::open(dir.path().join("facade.fls")).unwrap()),
            false,
        );
        assert_preparatory_admit_refusal(
            &crate::AnyStore::from(RedbStore::open(&dir.path().join("facade.redb")).unwrap()),
            false,
        );
    }
    #[test]
    fn managed_local_admission_preparatory_remote_refusal() {
        let dir = tempfile::tempdir().unwrap();
        for tcp in [false, true] {
            let mut server = if tcp {
                crate::FerroStoreServer::bind_tcp(
                    "127.0.0.1:0",
                    crate::FerroStore::in_memory(),
                    crate::ServerAuth::new()
                        .with_psk("fixture", &[71; 32])
                        .unwrap(),
                )
                .unwrap()
            } else {
                crate::FerroStoreServer::bind_unix(
                    dir.path().join("facade.sock"),
                    crate::FerroStore::in_memory(),
                )
                .unwrap()
            };
            let client = if tcp {
                crate::FerroStore::connect_with(
                    &format!("ws://{}", server.local_addr()),
                    &crate::ConnectOptions::psk("fixture", vec![71; 32]),
                )
                .unwrap()
            } else {
                crate::FerroStore::connect_uri(&format!(
                    "unix://{}",
                    dir.path().join("facade.sock").display()
                ))
                .unwrap()
            };
            assert_preparatory_admit_refusal(&client, true);
            let any = crate::AnyStore::from(client);
            assert_preparatory_admit_refusal(&any, true);
            server.shutdown().unwrap();
            assert_preparatory_admit_refusal(&any, true);
        }
    }
}
