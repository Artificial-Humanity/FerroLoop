//! Durable destination reservations. A shared batch owns one reservation;
//! member owners validate their subset without double-reserving the same set.
use super::{
    OperationLimits, RecoveryError,
    control::BatchRequest,
    fs::{self, Dir},
    verify::PrivateFile,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::AsRawFd,
    sync::Arc,
};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    version: u32,
    batch_id: uuid::Uuid,
    owner_store_id: String,
    members: Vec<(String, uuid::Uuid, u64)>,
    bytes: u64,
}
fn error(l: &OperationLimits) -> RecoveryError {
    RecoveryError::Quota {
        operation_id: l.operation_id,
        resource: "twice prospective batch plus retained reservations",
    }
}
fn lock(root: &Dir, l: &OperationLimits) -> Result<File, RecoveryError> {
    let file = super::scheduler::private_file(&root.path.join(".capacity.lock"), true)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(error(l));
    }
    root.same_name(l)?;
    Ok(file)
}
fn read(dir: &Dir, name: &str, l: &OperationLimits) -> Result<Reservation, RecoveryError> {
    let held = PrivateFile::open(&dir.path.join(name), false, l)?;
    let mut bytes = Vec::new();
    (&held.file)
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| error(l))?;
    if bytes.len() > 65536 {
        return Err(error(l));
    }
    let r: Reservation = serde_json::from_slice(&bytes).map_err(|_| l.verification())?;
    held.unchanged(l)?;
    if r.version != 1
        || r.owner_store_id.is_empty()
        || r.owner_store_id.len() > 128
        || format!("{}.json", r.batch_id) != name
        || r.members.is_empty()
        || r.members.len() > 128
        || r.members
            .iter()
            .try_fold(0u64, |n, m| n.checked_add(m.2))
            .and_then(|n| n.checked_mul(2))
            .is_none_or(|n| n > r.bytes)
    {
        return Err(l.verification());
    }
    Ok(r)
}
pub(crate) fn reserve(
    root: &Dir,
    request: &BatchRequest,
    owner_store_id: &str,
    config_bytes: u64,
    margin: u64,
    io: Arc<dyn fs::RecoveryIo>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    let _lock = lock(root, l)?;
    let dir = root.child(".reservations", false, l)?;
    let name = format!("{}.json", request.batch_id);
    if dir.exists(&name, l)? {
        let old = read(&dir, &name, l)?;
        if !request.members.iter().all(|m| {
            old.members
                .iter()
                .any(|o| o.0 == m.store_id && o.1 == m.operation_id && m.byte_cap == Some(o.2))
        }) {
            return Err(l.verification());
        }
        return Ok(());
    }
    let names = dir.members(l)?;
    if names.len() >= 1024 {
        return Err(error(l));
    }
    let mut outstanding = 0u64;
    for name in names {
        outstanding = outstanding
            .checked_add(read(&dir, &name, l)?.bytes)
            .ok_or_else(|| error(l))?;
    }
    let members: Vec<_> = request
        .members
        .iter()
        .map(|m| {
            m.byte_cap
                .filter(|n| *n != 0)
                .map(|n| (m.store_id.clone(), m.operation_id, n))
                .ok_or_else(|| error(l))
        })
        .collect::<Result<_, _>>()?;
    let required = members
        .iter()
        .try_fold(config_bytes, |n, m| n.checked_add(m.2))
        .and_then(|n| n.checked_mul(2))
        .ok_or_else(|| error(l))?;
    let available = io
        .available_bytes(&root.file)
        .map_err(|_| fs::destination(l))?
        .checked_sub(margin)
        .and_then(|n| n.checked_sub(outstanding))
        .ok_or_else(|| error(l))?;
    if required > available {
        return Err(error(l));
    }
    let value = Reservation {
        version: 1,
        batch_id: request.batch_id,
        owner_store_id: owner_store_id.into(),
        members,
        bytes: required,
    };
    let bytes = serde_json::to_vec(&value).map_err(|_| error(l))?;
    if bytes.len() > 65536 {
        return Err(error(l));
    }
    let temporary = format!("{}.tmp", request.batch_id);
    let target = PrivateFile::open(&dir.path.join(&temporary), true, l)?;
    (&target.file)
        .write_all(&bytes)
        .and_then(|()| target.file.sync_all())
        .map_err(|_| fs::destination(l))?;
    target.same_name(l)?;
    dir.rename(
        &temporary,
        &dir,
        &name,
        &*io,
        fs::RecoveryEvent::CatalogPublish,
        l,
    )?;
    dir.sync(&*io, fs::Directory::CatalogParent, l)?;
    root.sync(&*io, fs::Directory::PublicationParent, l)
}
pub(crate) fn release(
    root: &Dir,
    request: &BatchRequest,
    owner_store_id: &str,
    io: Arc<dyn fs::RecoveryIo>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    let _lock = lock(root, l)?;
    let dir = Dir::open(&root.path.join(".reservations"), l)?;
    let name = format!("{}.json", request.batch_id);
    if !dir.exists(&name, l)? {
        return Ok(());
    }
    let old = read(&dir, &name, l)?;
    // A member must never release the coordinator's whole-batch reservation.
    if old.owner_store_id != owner_store_id || old.members.len() != request.members.len() {
        return Ok(());
    }
    if !request.members.iter().all(|m| {
        old.members
            .iter()
            .any(|o| o.0 == m.store_id && o.1 == m.operation_id)
    }) {
        return Err(l.verification());
    }
    let file = PrivateFile::open(&dir.path.join(&name), false, l)?;
    dir.remove_file(&name, file.identity(), l)?;
    dir.sync(&*io, fs::Directory::CatalogParent, l)
}

#[cfg(test)]
mod tests {
    use super::super::{CancelToken, control::MemberRequest, fs::NativeRecoveryIo};
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };
    struct Space;
    impl fs::RecoveryIo for Space {
        fn available_bytes(&self, _: &File) -> std::io::Result<u64> {
            Ok(100_000)
        }
    }
    fn request() -> BatchRequest {
        BatchRequest {
            policy_id: "fixture".into(),
            batch_id: uuid::Uuid::now_v7(),
            members: vec![MemberRequest {
                store_id: "fixture".into(),
                operation_id: uuid::Uuid::now_v7(),
                scheduled_at_unix_seconds: None,
                byte_cap: Some(30_000),
            }],
        }
    }
    #[test]
    fn persisted_inflight_reservation_prevents_double_spend_after_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let l = OperationLimits::new(
            Instant::now() + Duration::from_secs(30),
            1 << 20,
            32 << 20,
            CancelToken::new(),
        );
        let root = Dir::open(tmp.path(), &l).unwrap();
        let a = request();
        reserve(&root, &a, "fixture", 0, 0, Arc::new(Space), &l).unwrap();
        drop(root);
        let reopened = Dir::open(tmp.path(), &l).unwrap();
        reserve(&reopened, &a, "fixture", 0, 0, Arc::new(Space), &l).unwrap();
        let b = request();
        assert!(reserve(&reopened, &b, "fixture", 0, 0, Arc::new(Space), &l).is_err());
        release(&reopened, &a, "fixture", Arc::new(NativeRecoveryIo), &l).unwrap();
        reserve(&reopened, &b, "fixture", 0, 0, Arc::new(Space), &l).unwrap();
    }
}
