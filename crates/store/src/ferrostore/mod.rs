//! Document store and FerroMQ log in a `.fls` file, and a unix socket server.
//! See `docs/ferrostore-spec.md`.

mod doc;
mod layout;
mod migrate;
mod server;
mod state;
mod traits_catalog;
mod traits_state;
mod traits_wire;
mod traits_workflow;

#[cfg(test)]
mod tests;

use fl_core::StoreError;
use layout::{
    HEADER_BYTES, Op, SLOT_LEN, Superblock, backend, corrupt, decode_snapshot, decode_wal,
    encode_frame, encode_snapshot, encode_superblock, parse_superblock,
};
use serde_json::{Value, json};
use state::{
    DocState, MQ_HIGH_WATER_ID, apply_commit, apply_frames, commit_ops, mq_hw_collection,
    mq_message_collection, next_mq_seq, unread_mq,
};

pub use migrate::migrate_redb_to_ferrostore;
pub use server::FerroStoreServer;
pub use state::MqRecord;
use std::collections::{BTreeMap, HashMap};
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
    Remote(server::RemoteClient),
}

struct Disk {
    write: Mutex<File>,
    /// Separate `O_RDONLY` description. Reads never flock it.
    read: File,
}

/// One transaction: a clone of the document map plus the ops that produced it.
/// `get` / `put` / `delete` / `scan` all see earlier writes in this transaction.
struct Txn {
    docs: BTreeMap<(String, String), Value>,
    ops: Vec<Op>,
}

impl Txn {
    fn get(&self, collection: &str, id: &str) -> Option<Value> {
        self.docs
            .get(&(collection.to_string(), id.to_string()))
            .cloned()
    }

    fn put(&mut self, collection: impl Into<String>, id: impl Into<String>, doc: Value) {
        let collection = collection.into();
        let id = id.into();
        self.docs
            .insert((collection.clone(), id.clone()), doc.clone());
        self.ops.push(Op::Put {
            collection,
            id,
            doc,
        });
    }

    fn delete(&mut self, collection: impl Into<String>, id: impl Into<String>) {
        let collection = collection.into();
        let id = id.into();
        self.docs.remove(&(collection.clone(), id.clone()));
        self.ops.push(Op::Delete { collection, id });
    }

    /// Every document in `collection`, in document-id order.
    fn scan(&self, collection: &str) -> Vec<(String, Value)> {
        let start = (collection.to_string(), String::new());
        let mut out = Vec::new();
        for ((coll, id), doc) in self.docs.range(start..) {
            if coll != collection {
                break;
            }
            out.push((id.clone(), doc.clone()));
        }
        out
    }
}

/// Document store and FerroMQ log.
///
/// `mem://` keeps the map in this process. `file://` is a `.fls` container:
/// two superblock slots, a copy-on-write snapshot, and a CRC32-framed WAL.
/// `unix://` is a client of a [`FerroStoreServer`] and does not open the file.
pub struct FerroStore {
    inner: Inner,
    subs: Mutex<HashMap<String, Vec<Sender<MqRecord>>>>,
    /// Named in `NotOwned`: where a lookup looked.
    label: String,
}

impl FerroStore {
    /// Empty in-memory store. `checkpoint` succeeds and writes nothing.
    pub fn in_memory() -> Self {
        Self {
            inner: Inner::Memory(Mutex::new(DocState::default())),
            subs: Mutex::new(HashMap::new()),
            label: "mem://".to_string(),
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
        ensure_superblock(&write)?;
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
            label: path.display().to_string(),
        })
    }

    /// `mem://`, `file://`, `unix://`, and the refused websocket schemes.
    pub fn connect_uri(uri: &str) -> Result<Self, StoreError> {
        if uri == "mem://" || uri == "mem:///" {
            return Ok(Self::in_memory());
        }
        if let Some(rest) = uri.strip_prefix("file://") {
            return Self::open(file_uri_path(rest));
        }
        if let Some(rest) = uri.strip_prefix("unix://") {
            let path = file_uri_path(rest);
            let remote = server::connect_unix(&path)?;
            return Ok(Self {
                inner: Inner::Remote(remote),
                subs: Mutex::new(HashMap::new()),
                label: path,
            });
        }
        if uri.starts_with("ws://") || uri.starts_with("wss://") {
            return Err(backend(
                "Phase 2 serves unix socket and websocket transports; this build does not",
            ));
        }
        Err(backend(format!("unknown FerroStore URI scheme in `{uri}`")))
    }

    pub fn checkpoint(&self) -> Result<(), StoreError> {
        match &self.inner {
            Inner::Remote(remote) => remote.call_unit("checkpoint", json!({})),
            Inner::Memory(_) => Ok(()),
            Inner::Disk(disk) => checkpoint_disk(disk),
        }
    }

    pub fn put_doc(&self, collection: &str, id: &str, doc: &Value) -> Result<(), StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            return remote.call_unit(
                "put_doc",
                json!({"collection": collection, "id": id, "doc": doc}),
            );
        }
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
        if let Inner::Remote(remote) = &self.inner {
            return remote.call_option("get_doc", json!({"collection": collection, "id": id}));
        }
        let state = self.read_state()?;
        Ok(state
            .docs
            .get(&(collection.to_string(), id.to_string()))
            .cloned())
    }

    pub fn delete_doc(&self, collection: &str, id: &str) -> Result<(), StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            return remote.call_unit("delete_doc", json!({"collection": collection, "id": id}));
        }
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
        if let Inner::Remote(remote) = &self.inner {
            return remote.call_strings(
                "query_by_field",
                json!({"collection": collection, "field": field, "value": value}),
            );
        }
        let state = self.read_state()?;
        Ok(state::query_by_field(&state, collection, field, value))
    }

    pub fn search_bm25(
        &self,
        collection: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            return remote.call_pairs(
                "search_bm25",
                json!({"collection": collection, "query": query, "limit": limit}),
            );
        }
        let state = self.read_state()?;
        Ok(state::search_bm25(&state, collection, query, limit))
    }

    /// Append one message. The returned sequence starts at 1 per topic and is
    /// not reused after that message document is deleted.
    pub fn publish_mq(&self, topic: &str, key: &str, payload: &Value) -> Result<u64, StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            return remote.call_u64(
                "publish_mq",
                json!({"topic": topic, "key": key, "payload": payload}),
            );
        }
        let topic_owned = topic.to_string();
        let key_owned = key.to_string();
        let payload = payload.clone();
        // One `with_write`: the message and the high-water share a WAL frame.
        // A crash must not persist either document alone.
        let record = self.with_write(move |txn| {
            let seq = next_mq_seq(&txn.docs, &topic_owned)?;
            let record = MqRecord {
                seq,
                key: key_owned.clone(),
                payload: payload.clone(),
            };
            txn.put(
                mq_message_collection(&topic_owned),
                seq.to_string(),
                serde_json::json!({
                    "seq": seq,
                    "key": key_owned,
                    "payload": payload,
                }),
            );
            txn.put(
                mq_hw_collection(&topic_owned),
                MQ_HIGH_WATER_ID,
                serde_json::json!({ "seq": seq }),
            );
            Ok(record)
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
        if let Inner::Remote(remote) = &self.inner {
            return remote.call_records(
                "poll_mq_group",
                json!({"topic": topic, "group": group, "limit": limit}),
            );
        }
        let state = self.read_state()?;
        unread_mq(&state, topic, group, limit)
    }

    /// Record an ack. An unknown sequence is still stored.
    pub fn ack_mq(&self, topic: &str, group: &str, seq: u64) -> Result<(), StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            return remote.call_unit(
                "ack_mq",
                json!({"topic": topic, "group": group, "seq": seq}),
            );
        }
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
        if let Inner::Remote(remote) = &self.inner {
            return remote.subscribe_topic(topic);
        }
        let (tx, rx) = mpsc::channel();
        lock_mutex(&self.subs)
            .entry(topic.to_string())
            .or_default()
            .push(tx);
        rx
    }

    fn with_read<T>(
        &self,
        body: impl FnOnce(&Txn) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if matches!(&self.inner, Inner::Remote(_)) {
            return Err(remote_unsupported());
        }
        let state = self.read_state()?;
        let txn = Txn {
            docs: state.docs,
            ops: Vec::new(),
        };
        body(&txn)
    }

    fn read_state(&self) -> Result<DocState, StoreError> {
        match &self.inner {
            Inner::Memory(state) => Ok(lock_mutex(state).clone()),
            Inner::Disk(disk) => Ok(read_consistent(&disk.read)?.2),
            Inner::Remote(_) => Err(remote_unsupported()),
        }
    }

    /// Run `body` against a cloned document map.
    ///
    /// Memory holds the document mutex for the whole call. Disk takes `LOCK_EX`,
    /// re-reads a consistent snapshot, then builds the transaction from that
    /// state. `Ok` with at least one op appends one WAL frame (`seq = last_seq + 1`)
    /// and publishes one superblock. `Err` or an empty op list writes nothing.
    fn with_write<T>(
        &self,
        body: impl FnOnce(&mut Txn) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        match &self.inner {
            Inner::Remote(_) => Err(remote_unsupported()),
            Inner::Memory(slot) => {
                let mut state = lock_mutex(slot);
                let mut txn = Txn {
                    docs: state.docs.clone(),
                    ops: Vec::new(),
                };
                let result = body(&mut txn)?;
                commit_ops(&mut state, &txn.ops)?;
                Ok(result)
            }
            Inner::Disk(disk) => with_write_disk(disk, body),
        }
    }

    fn commit<T>(
        &self,
        build: impl FnOnce(&DocState) -> Result<(Op, T), StoreError>,
    ) -> Result<T, StoreError> {
        match &self.inner {
            Inner::Remote(_) => Err(remote_unsupported()),
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

fn remote_unsupported() -> StoreError {
    backend("a remote FerroStore does not run a local transaction")
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

fn with_write_disk<T>(
    disk: &Disk,
    body: impl FnOnce(&mut Txn) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let file = lock_mutex(&disk.write);
    let _locked = FlockGuard::acquire(file.as_raw_fd())?;
    let (index, sb, state) = read_consistent(&file)?;
    let seq_base = state.last_seq;
    let mut txn = Txn {
        docs: state.docs,
        ops: Vec::new(),
    };
    let result = body(&mut txn)?;
    if txn.ops.is_empty() {
        return Ok(result);
    }
    let seq = seq_base
        .checked_add(1)
        .ok_or_else(|| backend("transaction sequence overflow"))?;
    let frame = encode_frame(seq, &txn.ops)?;
    append_frame(&file, index, &sb, &frame)?;
    Ok(result)
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

// No valid slot yet. Publish slot 0 only for an empty file, or when every byte
// already in a file no longer than the header is zero. A non-zero short file
// is not rewritten, and neither is a longer file.
fn ensure_superblock(file: &File) -> Result<(), StoreError> {
    if parsed_superblock(file)?.is_some() {
        return Ok(());
    }
    let _locked = FlockGuard::acquire(file.as_raw_fd())?;
    if parsed_superblock(file)?.is_some() {
        return Ok(());
    }
    let len = file.metadata().map_err(io_err)?.len();
    if len > HEADER_BYTES {
        return Ok(());
    }
    if len == 0 {
        return init_empty(file);
    }
    let n = usize::try_from(len).map_err(|_| backend("short file length exceeds usize"))?;
    let mut buf = vec![0u8; n];
    file.read_exact_at(&mut buf, 0).map_err(io_err)?;
    if buf.iter().any(|byte| *byte != 0) {
        return Ok(());
    }
    init_empty(file)
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

fn parsed_superblock(file: &File) -> Result<Option<(usize, Superblock)>, StoreError> {
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
    Ok(best)
}

fn latest_superblock(file: &File) -> Result<(usize, Superblock), StoreError> {
    parsed_superblock(file)?.ok_or_else(|| backend("no valid FerroStore superblock"))
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
