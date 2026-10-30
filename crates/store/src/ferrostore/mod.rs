//! Document store and FerroMQ log in a `.fls` file.
//! See `docs/ferrostore-spec.md`.

mod layout;
mod state;

#[cfg(test)]
mod tests;

use fl_core::StoreError;
use layout::{
    HEADER_BYTES, Op, SLOT_LEN, Superblock, backend, corrupt, decode_snapshot, decode_wal,
    encode_frame, encode_snapshot, encode_superblock, parse_superblock,
};
use serde_json::Value;
use state::{DocState, apply_commit, apply_frames, next_mq_seq, unread_mq};

pub use state::MqRecord;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, MutexGuard};

const LOCK_EX: i32 = 2;
const LOCK_UN: i32 = 8;

// libc `flock`. No extra crate: the process already links libc.
unsafe extern "C" {
    fn flock(fd: i32, op: i32) -> i32;
}

struct FlockGuard {
    fd: i32,
}

impl FlockGuard {
    fn acquire(fd: i32) -> Result<Self, StoreError> {
        // SAFETY: `fd` is an open file descriptor owned by the caller. `LOCK_EX`
        // waits until this description holds the advisory lock, or returns -1.
        let rc = unsafe { flock(fd, LOCK_EX) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            return Err(backend(format!("exclusive flock failed: {err}")));
        }
        Ok(Self { fd })
    }
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        // SAFETY: the descriptor stays open until after this guard is dropped.
        // `LOCK_UN` releases the advisory lock held by that description.
        unsafe {
            flock(self.fd, LOCK_UN);
        }
    }
}

enum Inner {
    Memory(Mutex<DocState>),
    Disk(Disk),
}

struct Disk {
    write: Mutex<File>,
    /// Separate `O_RDONLY` description. Reads never flock it.
    read: File,
}

/// Phase 1 document store and FerroMQ log.
///
/// `mem://` keeps the map in this process. `file://` is a `.fls` container:
/// two superblock slots, a copy-on-write snapshot, and a CRC32-framed WAL.
pub struct FerroStore {
    inner: Inner,
    subs: Mutex<HashMap<String, Vec<Sender<MqRecord>>>>,
}

impl FerroStore {
    /// Empty in-memory store. `checkpoint` succeeds and writes nothing.
    pub fn in_memory() -> Self {
        Self {
            inner: Inner::Memory(Mutex::new(DocState::default())),
            subs: Mutex::new(HashMap::new()),
        }
    }

    /// Open or create a `.fls` file. A second open of the same path succeeds.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let write = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|err| backend(format!("open {}: {err}", path.display())))?;
        if write.metadata().map_err(io_err)?.len() == 0 {
            let _locked = FlockGuard::acquire(write.as_raw_fd())?;
            if write.metadata().map_err(io_err)?.len() == 0 {
                init_empty(&write)?;
            }
        }
        read_consistent(&write)?;
        // `OpenOptions::read` without write is `O_RDONLY`.
        let read = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|err| backend(format!("open {}: {err}", path.display())))?;
        Ok(Self {
            inner: Inner::Disk(Disk {
                write: Mutex::new(write),
                read,
            }),
            subs: Mutex::new(HashMap::new()),
        })
    }

    /// `mem://`, `file://`, and the refused served-mode schemes.
    pub fn connect_uri(uri: &str) -> Result<Self, StoreError> {
        if uri == "mem://" || uri == "mem:///" {
            return Ok(Self::in_memory());
        }
        if let Some(rest) = uri.strip_prefix("file://") {
            return Self::open(file_uri_path(rest));
        }
        if uri.starts_with("unix://") || uri.starts_with("ws://") || uri.starts_with("wss://") {
            return Err(backend(
                "Phase 2 serves unix socket and websocket transports; this build does not",
            ));
        }
        Err(backend(format!("unknown FerroStore URI scheme in `{uri}`")))
    }

    pub fn checkpoint(&self) -> Result<(), StoreError> {
        match &self.inner {
            Inner::Memory(_) => Ok(()),
            Inner::Disk(disk) => checkpoint_disk(disk),
        }
    }

    pub fn put_doc(&self, collection: &str, id: &str, doc: &Value) -> Result<(), StoreError> {
        let collection = collection.to_string();
        let id = id.to_string();
        let doc = doc.clone();
        self.commit(move |_| {
            Ok((
                Op::Put {
                    collection,
                    id,
                    doc,
                },
                (),
            ))
        })
    }

    pub fn get_doc(&self, collection: &str, id: &str) -> Result<Option<Value>, StoreError> {
        let state = self.read_state()?;
        Ok(state
            .docs
            .get(&(collection.to_string(), id.to_string()))
            .cloned())
    }

    pub fn delete_doc(&self, collection: &str, id: &str) -> Result<(), StoreError> {
        let collection = collection.to_string();
        let id = id.to_string();
        self.commit(move |_| Ok((Op::Delete { collection, id }, ())))
    }

    pub fn query_by_field(
        &self,
        collection: &str,
        field: &str,
        value: &str,
    ) -> Result<Vec<String>, StoreError> {
        let state = self.read_state()?;
        Ok(state::query_by_field(&state, collection, field, value))
    }

    pub fn search_bm25(
        &self,
        collection: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        let state = self.read_state()?;
        Ok(state::search_bm25(&state, collection, query, limit))
    }

    /// Append one message. The returned sequence starts at 1 per topic.
    pub fn publish_mq(&self, topic: &str, key: &str, payload: &Value) -> Result<u64, StoreError> {
        let topic_owned = topic.to_string();
        let key_owned = key.to_string();
        let payload = payload.clone();
        let record = self.commit(move |state| {
            let seq = next_mq_seq(state, &topic_owned)?;
            let record = MqRecord {
                seq,
                key: key_owned.clone(),
                payload: payload.clone(),
            };
            let doc = serde_json::json!({
                "seq": seq,
                "key": key_owned,
                "payload": payload,
            });
            Ok((
                Op::Put {
                    collection: format!("mq:{topic_owned}"),
                    id: seq.to_string(),
                    doc,
                },
                record,
            ))
        })?;
        self.notify(topic, &record);
        Ok(record.seq)
    }

    /// Oldest messages for `topic` that `group` has not acked, without acking them.
    pub fn poll_mq_group(
        &self,
        topic: &str,
        group: &str,
        limit: usize,
    ) -> Result<Vec<MqRecord>, StoreError> {
        let state = self.read_state()?;
        unread_mq(&state, topic, group, limit)
    }

    /// Record an ack. An unknown sequence is still stored.
    pub fn ack_mq(&self, topic: &str, group: &str, seq: u64) -> Result<(), StoreError> {
        let topic = topic.to_string();
        let group = group.to_string();
        self.commit(move |_| {
            Ok((
                Op::Put {
                    collection: format!("mq_ack:{topic}:{group}"),
                    id: seq.to_string(),
                    doc: serde_json::json!({ "seq": seq }),
                },
                (),
            ))
        })
    }

    /// Later publishes on this handle. History is not replayed.
    pub fn subscribe_mq(&self, topic: &str) -> Receiver<MqRecord> {
        let (tx, rx) = mpsc::channel();
        lock_mutex(&self.subs)
            .entry(topic.to_string())
            .or_default()
            .push(tx);
        rx
    }

    fn read_state(&self) -> Result<DocState, StoreError> {
        match &self.inner {
            Inner::Memory(state) => Ok(lock_mutex(state).clone()),
            Inner::Disk(disk) => Ok(read_consistent(&disk.read)?.2),
        }
    }

    fn commit<T>(
        &self,
        build: impl FnOnce(&DocState) -> Result<(Op, T), StoreError>,
    ) -> Result<T, StoreError> {
        match &self.inner {
            Inner::Memory(slot) => {
                let mut state = lock_mutex(slot);
                let (op, extra) = build(&state)?;
                apply_commit(&mut state, op)?;
                Ok(extra)
            }
            Inner::Disk(disk) => commit_disk(disk, build),
        }
    }

    fn notify(&self, topic: &str, record: &MqRecord) {
        let mut subs = lock_mutex(&self.subs);
        let Some(senders) = subs.get_mut(topic) else {
            return;
        };
        senders.retain(|sender| sender.send(record.clone()).is_ok());
        if senders.is_empty() {
            subs.remove(topic);
        }
    }
}

fn file_uri_path(rest: &str) -> String {
    if rest.starts_with('/') {
        return rest.to_string();
    }
    if let Some(path) = rest.strip_prefix("localhost/") {
        return format!("/{path}");
    }
    if rest == "localhost" {
        return "/".to_string();
    }
    rest.to_string()
}

fn commit_disk<T>(
    disk: &Disk,
    build: impl FnOnce(&DocState) -> Result<(Op, T), StoreError>,
) -> Result<T, StoreError> {
    let file = lock_mutex(&disk.write);
    let _locked = FlockGuard::acquire(file.as_raw_fd())?;
    let (index, sb, state) = read_consistent(&file)?;
    let (op, extra) = build(&state)?;
    let seq = state
        .last_seq
        .checked_add(1)
        .ok_or_else(|| backend("transaction sequence overflow"))?;
    let frame = encode_frame(seq, std::slice::from_ref(&op))?;
    append_frame(&file, index, &sb, &frame)?;
    Ok(extra)
}

fn checkpoint_disk(disk: &Disk) -> Result<(), StoreError> {
    let file = lock_mutex(&disk.write);
    let _locked = FlockGuard::acquire(file.as_raw_fd())?;
    let (index, sb, state) = read_consistent(&file)?;
    let blob = encode_snapshot(state.last_seq, &state.docs)?;
    let blob_len = u64::try_from(blob.len()).map_err(|_| backend("snapshot exceeds u64"))?;
    let offset = allocate_extent(&file, &sb, blob_len)?;
    file.write_all_at(&blob, offset).map_err(io_err)?;
    file.sync_all().map_err(io_err)?;
    let new_sb = Superblock {
        epoch: sb
            .epoch
            .checked_add(1)
            .ok_or_else(|| backend("epoch overflow"))?,
        snapshot_offset: offset,
        snapshot_length: blob_len,
        wal_offset: offset
            .checked_add(blob_len)
            .ok_or_else(|| backend("wal offset overflow"))?,
        wal_length: 0,
        snapshot_seq: state.last_seq,
    };
    write_slot(&file, index ^ 1, &new_sb)?;
    file.sync_all().map_err(io_err)?;
    Ok(())
}

fn allocate_extent(file: &File, sb: &Superblock, len: u64) -> Result<u64, StoreError> {
    let file_len = file.metadata().map_err(io_err)?.len();
    let live_end = sb
        .snapshot_offset
        .saturating_add(sb.snapshot_length)
        .max(sb.wal_offset.saturating_add(sb.wal_length))
        .max(HEADER_BYTES);
    let offset = file_len.max(live_end);
    if offset.checked_add(len).is_none() {
        return Err(backend("extent offset overflow"));
    }
    Ok(offset)
}

fn append_frame(
    file: &File,
    index: usize,
    sb: &Superblock,
    frame: &[u8],
) -> Result<(), StoreError> {
    let frame_len = u64::try_from(frame.len()).map_err(|_| backend("wal frame exceeds u64"))?;
    let at = sb
        .wal_offset
        .checked_add(sb.wal_length)
        .ok_or_else(|| backend("wal offset overflow"))?;
    file.write_all_at(frame, at).map_err(io_err)?;
    file.sync_all().map_err(io_err)?;
    let new_sb = Superblock {
        epoch: sb
            .epoch
            .checked_add(1)
            .ok_or_else(|| backend("epoch overflow"))?,
        snapshot_offset: sb.snapshot_offset,
        snapshot_length: sb.snapshot_length,
        wal_offset: sb.wal_offset,
        wal_length: sb
            .wal_length
            .checked_add(frame_len)
            .ok_or_else(|| backend("wal length overflow"))?,
        snapshot_seq: sb.snapshot_seq,
    };
    write_slot(file, index ^ 1, &new_sb)?;
    file.sync_all().map_err(io_err)?;
    Ok(())
}

fn init_empty(file: &File) -> Result<(), StoreError> {
    file.set_len(HEADER_BYTES).map_err(io_err)?;
    let sb = Superblock {
        epoch: 1,
        snapshot_offset: HEADER_BYTES,
        snapshot_length: 0,
        wal_offset: HEADER_BYTES,
        wal_length: 0,
        snapshot_seq: 0,
    };
    write_slot(file, 0, &sb)?;
    file.sync_all().map_err(io_err)?;
    Ok(())
}

fn read_consistent(file: &File) -> Result<(usize, Superblock, DocState), StoreError> {
    let (index, sb) = latest_superblock(file)?;
    let mut state = if sb.snapshot_length == 0 {
        DocState::at_seq(sb.snapshot_seq)
    } else {
        let bytes = read_range(file, sb.snapshot_offset, sb.snapshot_length)?;
        let snap = decode_snapshot(&bytes)?;
        if snap.seq != sb.snapshot_seq {
            return Err(corrupt("snapshot sequence does not match the superblock"));
        }
        DocState {
            docs: snap.docs,
            last_seq: snap.seq,
        }
    };
    let wal = read_range(file, sb.wal_offset, sb.wal_length)?;
    let frames = decode_wal(&wal)?;
    apply_frames(&mut state, sb.snapshot_seq, &frames)?;
    Ok((index, sb, state))
}

fn latest_superblock(file: &File) -> Result<(usize, Superblock), StoreError> {
    let file_len = file.metadata().map_err(io_err)?.len();
    let mut best: Option<(usize, Superblock)> = None;
    for index in 0..2 {
        let offset = slot_offset(index);
        if file_len < offset + SLOT_LEN as u64 {
            continue;
        }
        let mut buf = [0u8; SLOT_LEN];
        file.read_exact_at(&mut buf, offset).map_err(io_err)?;
        if let Some(sb) = parse_superblock(&buf) {
            match &best {
                Some((_, current)) if sb.epoch < current.epoch => {}
                Some((_, current)) if sb.epoch == current.epoch => {
                    return Err(corrupt("two valid superblocks share an epoch"));
                }
                _ => best = Some((index, sb)),
            }
        }
    }
    best.ok_or_else(|| backend("no valid FerroStore superblock"))
}

fn write_slot(file: &File, index: usize, sb: &Superblock) -> Result<(), StoreError> {
    if index > 1 {
        return Err(backend("superblock slot out of range"));
    }
    let bytes = encode_superblock(sb);
    file.write_all_at(&bytes, slot_offset(index))
        .map_err(io_err)?;
    Ok(())
}

fn read_range(file: &File, offset: u64, length: u64) -> Result<Vec<u8>, StoreError> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let len =
        usize::try_from(length).map_err(|_| corrupt("extent length does not fit in memory"))?;
    let file_len = file.metadata().map_err(io_err)?.len();
    let end = offset
        .checked_add(length)
        .ok_or_else(|| corrupt("extent range overflows"))?;
    if end > file_len {
        return Err(corrupt("extent extends past the end of the file"));
    }
    let mut buf = vec![0u8; len];
    file.read_exact_at(&mut buf, offset).map_err(io_err)?;
    Ok(buf)
}

fn slot_offset(index: usize) -> u64 {
    index as u64 * SLOT_LEN as u64
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn io_err(err: io::Error) -> StoreError {
    backend(err)
}
