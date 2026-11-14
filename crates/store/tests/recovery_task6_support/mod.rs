#![allow(dead_code)]
use fl_store::{
    FerroStore,
    recovery::{
        admission::*,
        backup::*,
        fs::{NativeRecoveryFs, RecoveryFs},
        manifest::{Availability, BuildIdentity},
        *,
    },
};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
#[path = "../../src/recovery/test_support.rs"]
pub mod test_support;
pub fn limits() -> OperationLimits {
    OperationLimits::new(
        Instant::now() + Duration::from_secs(180),
        256 << 20,
        1 << 30,
        CancelToken::new(),
    )
}
pub struct Fixture {
    pub root: tempfile::TempDir,
    pub source: PathBuf,
    pub backups: PathBuf,
}
impl Fixture {
    pub fn new() -> Self {
        let root = test_support::fixture_root();
        let source = root.path().join("source.fls");
        let backups = root.path().join("backups");
        fs::create_dir(&backups).unwrap();
        fs::set_permissions(
            &backups,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let owner = FerroStore::open(&source).unwrap();
        // Real append history, never padding or a metadata accounting exemption.
        for n in 0..160 {
            owner
                .put_doc(
                    "fixture_unknown",
                    "one",
                    &serde_json::json!({"n":n,"body":"x".repeat(256<<10)}),
                )
                .unwrap();
        }
        drop(owner);
        Self {
            root,
            source,
            backups,
        }
    }
    pub fn filesystem(&self) -> NativeRecoveryFs {
        NativeRecoveryFs::new(self.backups.clone(), limits())
    }
    pub fn identity(&self) -> StoreIdentity {
        let map = collect_inventory(&InventoryInputs {
            roots: vec![self.root.path().to_path_buf()],
            bindings: vec![BindingInput::config(
                "fixture",
                self.source.clone(),
                Some("ferro".into()),
                "fixture",
            )],
            ..Default::default()
        })
        .unwrap();
        map.stores
            .iter()
            .find(|s| s.identity.canonical_path.as_deref() == Some(&self.source))
            .unwrap()
            .identity
            .clone()
    }
    pub fn backup_request(&self) -> BackupRequest {
        let store = self.identity();
        let l = limits();
        let deadline = l.deadline;
        BackupRequest {
            profile: ValidationProfile::new(ProfileKind::SmallEmpty, store.catalog_key.clone()),
            store,
            source_map_sha256: "a".repeat(64),
            operation_id: l.operation_id,
            batch_id: None,
            destination: self.backups.clone(),
            capture: CaptureBudget::new(deadline, 256 << 20, 512 << 20, Duration::from_secs(1)),
            limits: l,
            image_byte_limit: None,
            metadata_byte_limit: 1 << 20,
            space_margin_bytes: 4096,
            configs: vec![],
            excluded_vault_key: None,
            code_index_configuration: Availability::Unavailable,
        }
    }
    pub fn bundle(&self) -> VerifiedBundle {
        let owner = FerroStore::open(&self.source).unwrap();
        let bundle = backup(
            &owner,
            &self.backup_request(),
            &self.filesystem(),
            &CancelToken::new(),
        )
        .unwrap();
        match bundle {
            PublicationOutcome::Durable(bundle) => bundle,
            _ => panic!("fixture publication not durable"),
        }
    }
    pub fn lease_request(
        &self,
        action: OfflineAction,
        operation_id: uuid::Uuid,
        profile_sha256: String,
        destination: &Path,
    ) -> OfflineLeaseRequest {
        let file_id = |p: &Path| {
            let m = fs::metadata(p).unwrap();
            (m.dev(), m.ino())
        };
        let now = self.filesystem().io().unix_ms().unwrap();
        OfflineLeaseRequest {
            version: 1,
            operation_id,
            action,
            catalog_identity: self.identity().catalog_key,
            approved_root_identity: file_id(&self.backups),
            physical_path: destination.to_path_buf(),
            parent_identity: file_id(destination.parent().unwrap()),
            expected_file: inspect_file_binding(destination, &limits()).unwrap(),
            profile_sha256,
            build: BuildIdentity {
                package_version: env!("CARGO_PKG_VERSION").into(),
                source_revision: Availability::Unavailable,
                executable_sha256: Availability::Unavailable,
            },
            issued_at_unix_ms: now,
            expires_at_unix_ms: now + 600000,
            operator: "fixture operator".into(),
            exclusion: ExclusionAttestation {
                legacy_and_cached_handles_excluded: true,
                external_dispatch_excluded: true,
                active_owner_excluded: true,
                evidence: "all fixture handles closed; no dispatch".into(),
            },
        }
    }
}
