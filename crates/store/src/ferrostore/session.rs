//! Remote transactions on the server (spec §6.1): read sessions pinned at
//! one snapshot, the read primitives a remote `Txn` asks for, the commit
//! that validates a read set by value, and the table of recent request ids
//! that makes a write exactly-once.
//!
//! A read primitive's answer is canonical JSON. Its hash is SHA-256 of that
//! JSON's bytes, computed here on the server both when the read is served
//! and when the commit validates it, so the client never re-encodes a value
//! to hash it.

use super::columnar;
use super::doc;
use super::layout::{Op, backend};
use super::state::{self, DocState};
use super::vector::{self, VectorParams};
use super::{FerroStore, lock_mutex, wait_mutex};
use fl_core::StoreError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A session no read or commit has used for this long is gone.
pub(super) const SESSION_IDLE: Duration = Duration::from_secs(30);
/// The most sessions one connection holds open.
pub(super) const SESSIONS_PER_CONN: usize = 64;
/// The most sessions one server holds open.
pub(super) const SESSIONS_PER_SERVER: usize = 1024;
/// The most request ids the exactly-once table remembers.
pub(super) const DEDUP_ENTRIES: usize = 10_000;
/// How long the exactly-once table remembers a request id.
pub(super) const DEDUP_TTL: Duration = Duration::from_secs(600);
/// Room a read's reply keeps in its frame for the envelope around the result.
const ENVELOPE_BYTES: usize = 4096;

/// A transaction's reads, each with its answer's hash, in the order read.
pub(super) type ReadSet = Vec<(ReadKey, String)>;

/// An open read session: its id and the `last_seq` of the state it pinned.
pub(super) type Pinned = (u64, u64);

/// One read primitive of a remote `Txn` (spec §6.1). Its JSON form is the
/// `read` of `txn_read` and of each entry of `commit_txn`'s `reads`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "p", rename_all = "snake_case")]
pub(super) enum ReadKey {
    /// `null`, or `{"doc": value}`.
    Get { collection: String, id: String },
    /// `[[id, doc], ...]` in id order.
    Scan { collection: String },
    /// The ids whose top-level `field` matches `value`, ascending.
    Query {
        collection: String,
        field: String,
        value: String,
    },
    /// `null`, or the column table's columns.
    Columns { name: String },
    /// `null`, or the vector collection's parameters.
    VectorParams { name: String },
    /// `{"err": message}`, `null` for a missing id, or `{"values", "metadata"?}`.
    GetVector { collection: String, id: String },
    /// `{"collections", "tables", "vectors"}`: the names that hold anything.
    Inventory,
}

impl ReadKey {
    /// The primitive's name, as an error names it.
    pub(super) fn primitive(&self) -> &'static str {
        match self {
            Self::Get { .. } => "get",
            Self::Scan { .. } => "scan",
            Self::Query { .. } => "query",
            Self::Columns { .. } => "table_columns",
            Self::VectorParams { .. } => "vector",
            Self::GetVector { .. } => "get_vector",
            Self::Inventory => "inventory",
        }
    }

    /// The collection, table or vector collection the primitive reads.
    pub(super) fn subject(&self) -> &str {
        match self {
            Self::Get { collection, .. }
            | Self::Scan { collection }
            | Self::Query { collection, .. }
            | Self::GetVector { collection, .. } => collection,
            Self::Columns { name } | Self::VectorParams { name } => name,
            Self::Inventory => "*",
        }
    }
}

/// The canonical answer to `key` at `state`. An error that depends on the
/// state (a missing vector collection) is part of the answer, so a commit
/// validates it as it validates a value.
pub(super) fn eval(state: &DocState, key: &ReadKey) -> Value {
    match key {
        ReadKey::Get { collection, id } => match state.docs.get(collection, id) {
            Some(doc) => json!({ "doc": doc }),
            None => Value::Null,
        },
        ReadKey::Scan { collection } => Value::Array(
            state
                .docs
                .in_collection(collection)
                .map(|(id, doc)| json!([id, doc]))
                .collect(),
        ),
        ReadKey::Query {
            collection,
            field,
            value,
        } => Value::Array(
            state::query_by_field(state, collection, field, value)
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
        ReadKey::Columns { name } => match state.tables.get(name) {
            Some(table) => Value::Array(
                table
                    .columns
                    .iter()
                    .map(columnar::column_def_json)
                    .collect(),
            ),
            None => Value::Null,
        },
        ReadKey::VectorParams { name } => match state.vectors.get(name) {
            Some(col) => VectorParams::of(col).to_json(),
            None => Value::Null,
        },
        ReadKey::GetVector { collection, id } => match stored_vector(state, collection, id) {
            Ok(value) => value,
            Err(err) => json!({ "err": err.to_string() }),
        },
        ReadKey::Inventory => json!({
            "collections": state.docs.collections().collect::<Vec<_>>(),
            "tables": state.tables.keys().collect::<Vec<_>>(),
            "vectors": state.vectors.keys().collect::<Vec<_>>(),
        }),
    }
}

fn stored_vector(state: &DocState, collection: &str, id: &str) -> Result<Value, StoreError> {
    let Some(col) = state.vectors.get(collection) else {
        return Err(backend(format!("no vector collection `{collection}`")));
    };
    vector::check_vector_id(id)?;
    let Some(record) = col.vectors.get(id) else {
        return Ok(Value::Null);
    };
    let values = vector::dequantize(col.storage, &record.bytes)?;
    vector::vector_get_json(&values, &record.metadata)
}

/// SHA-256 of `bytes`, as 64 lower-case hex digits.
fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn encode(result: &Value) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(result).map_err(|err| backend(format!("encode read result: {err}")))
}

/// The answer to `key` at `state` and its hash, for a reply. An answer
/// larger than a frame holds is an error that names the primitive and the
/// collection (spec §6.1).
pub(super) fn answer(state: &DocState, key: &ReadKey) -> Result<(Value, String), StoreError> {
    let result = eval(state, key);
    let bytes = encode(&result)?;
    let cap = super::server::MAX_FRAME_BYTES as usize - ENVELOPE_BYTES;
    if bytes.len() > cap {
        return Err(backend(format!(
            "{} of `{}`: the result is {} bytes, more than one 16 MiB frame holds \
             (a paginated read is a known limit)",
            key.primitive(),
            key.subject(),
            bytes.len()
        )));
    }
    Ok((result, hex_digest(&bytes)))
}

/// The hash of `key`'s answer at `state`, as a commit recomputes it.
fn fingerprint(state: &DocState, key: &ReadKey) -> Result<String, StoreError> {
    Ok(hex_digest(&encode(&eval(state, key))?))
}

/// A reserved op a remote commit may carry: the schema version, which only
/// the store's own bodies write (a raise to a version on this build's
/// ladder, or the migration's copy or removal of it). Any other reserved op
/// is refused, as public `put_doc` and `delete_doc` refuse it.
fn admit_remote_op(op: &Op) -> Result<(), StoreError> {
    let (collection, id, version) = match op {
        Op::Put {
            collection,
            id,
            doc,
        } => (collection, id, Some(doc.as_u64())),
        Op::Delete { collection, id } => (collection, id, None),
        _ => return Ok(()),
    };
    if !doc::is_reserved(collection) {
        return Ok(());
    }
    if collection == doc::SCHEMA_COLLECTION && id == doc::SCHEMA_KEY {
        match version {
            None => return Ok(()),
            Some(Some(to)) if crate::check_format(Some(to)).is_ok() => return Ok(()),
            Some(_) => {}
        }
    }
    doc::refuse_reserved(collection, "commit_txn")
}

/// `commit_txn` (spec §6.1): inside the write lock, as one write body of
/// the group, recompute every read against the committed state. All equal:
/// stage `ops`, each checked as a local write checks it, and commit. Any
/// different: write nothing and reply `committed: false` (lost).
pub(super) fn commit_txn(
    store: &FerroStore,
    sessions: &Sessions,
    conn: u64,
    params: &Value,
) -> Result<Value, StoreError> {
    // The session ends with its commit, lost or not.
    if let Some(session) = optional_u64(params, "session")? {
        let pinned = sessions.close(conn, session);
        if let (Some(pinned), Some(base_seq)) = (pinned, optional_u64(params, "base_seq")?)
            && pinned.last_seq != base_seq
        {
            return Err(backend(format!(
                "commit_txn base_seq {base_seq} is not session {session}'s snapshot {}",
                pinned.last_seq
            )));
        }
    }
    let reads = parse_reads(params.get("reads"))?;
    let ops = super::server::parse_ops(
        params
            .get("ops")
            .ok_or_else(|| backend("missing param `ops`"))?,
    )?;
    let silent = match params.get("silent") {
        None => false,
        Some(Value::Bool(silent)) => *silent,
        Some(_) => return Err(backend("param `silent` is not a bool")),
    };
    for op in &ops {
        admit_remote_op(op)?;
    }
    let (committed, seq) = store.write_local(silent, |txn| {
        let current = txn.local_arc()?;
        for (key, hash) in &reads {
            if fingerprint(&current, key)? != *hash {
                return Ok(false);
            }
        }
        drop(current);
        for op in &ops {
            txn.stage(op.clone())?;
        }
        Ok(true)
    })?;
    Ok(json!({ "committed": committed, "seq": seq }))
}

fn parse_reads(value: Option<&Value>) -> Result<Vec<(ReadKey, String)>, StoreError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| backend("param `reads` is not an array"))?;
    let mut reads = Vec::with_capacity(items.len());
    for item in items {
        let key = parse_key(item.get("read"))?;
        let hash = item
            .get("hash")
            .and_then(Value::as_str)
            .ok_or_else(|| backend("a read is missing its hash"))?;
        reads.push((key, hash.to_string()));
    }
    Ok(reads)
}

pub(super) fn parse_key(value: Option<&Value>) -> Result<ReadKey, StoreError> {
    let value = value.ok_or_else(|| backend("missing param `read`"))?;
    ReadKey::deserialize(value).map_err(|err| backend(format!("read primitive: {err}")))
}

pub(super) fn optional_u64(params: &Value, key: &str) -> Result<Option<u64>, StoreError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| backend(format!("param `{key}` is not an integer"))),
    }
}

// ---- sessions ----------------------------------------------------------------

struct Session {
    conn: u64,
    state: Arc<DocState>,
    last_used: Instant,
}

/// Every open read session of one server, keyed by a server-wide id.
pub(super) struct Sessions {
    open: Mutex<HashMap<u64, Session>>,
    next: AtomicU64,
    /// [`SESSION_IDLE`] in milliseconds; a test lowers it.
    idle_ms: AtomicU64,
}

impl Sessions {
    pub(super) fn new() -> Self {
        Self {
            open: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            idle_ms: AtomicU64::new(u64::try_from(SESSION_IDLE.as_millis()).unwrap_or(u64::MAX)),
        }
    }

    fn idle(&self) -> Duration {
        Duration::from_millis(self.idle_ms.load(Ordering::Relaxed))
    }

    #[cfg(test)]
    pub(super) fn set_idle(&self, idle: Duration) {
        self.idle_ms.store(
            u64::try_from(idle.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    #[cfg(test)]
    pub(super) fn count(&self) -> usize {
        lock_mutex(&self.open).len()
    }

    /// Pin `state` for `conn`. Sessions idle past the limit are dropped
    /// first; a connection or a server still at its limit refuses the new
    /// session, and the sessions already open are kept.
    pub(super) fn open(&self, conn: u64, state: Arc<DocState>) -> Result<u64, StoreError> {
        let now = Instant::now();
        let idle = self.idle();
        let mut open = lock_mutex(&self.open);
        open.retain(|_, session| now.duration_since(session.last_used) <= idle);
        let mine = open.values().filter(|session| session.conn == conn).count();
        if mine >= SESSIONS_PER_CONN {
            return Err(backend(format!(
                "too many open read sessions: this connection holds {SESSIONS_PER_CONN}"
            )));
        }
        if open.len() >= SESSIONS_PER_SERVER {
            return Err(backend(format!(
                "too many open read sessions: this server holds {SESSIONS_PER_SERVER}"
            )));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        open.insert(
            id,
            Session {
                conn,
                state,
                last_used: now,
            },
        );
        Ok(id)
    }

    /// The state `session` pinned, which is used again from now. A session
    /// of another connection is unknown here.
    pub(super) fn state(&self, conn: u64, session: u64) -> Result<Arc<DocState>, StoreError> {
        let now = Instant::now();
        let idle = self.idle();
        let mut open = lock_mutex(&self.open);
        match open.get_mut(&session) {
            Some(held) if held.conn == conn => {
                if now.duration_since(held.last_used) > idle {
                    open.remove(&session);
                    return Err(backend(format!(
                        "read session {session} expired after {} ms idle",
                        idle.as_millis()
                    )));
                }
                held.last_used = now;
                Ok(Arc::clone(&held.state))
            }
            _ => Err(backend(format!(
                "no open read session {session} on this connection"
            ))),
        }
    }

    /// End `session`; its pinned state, when it was open on `conn`.
    pub(super) fn close(&self, conn: u64, session: u64) -> Option<Arc<DocState>> {
        let mut open = lock_mutex(&self.open);
        match open.get(&session) {
            Some(held) if held.conn == conn => open.remove(&session).map(|held| held.state),
            _ => None,
        }
    }

    /// End every session of a connection that closed.
    pub(super) fn drop_conn(&self, conn: u64) {
        lock_mutex(&self.open).retain(|_, session| session.conn != conn);
    }
}

// ---- exactly-once --------------------------------------------------------------

enum Entry {
    /// The first request with this id is running.
    Running(Instant),
    /// Its reply, without the frame id.
    Done(Instant, Value),
}

impl Entry {
    fn at(&self) -> Instant {
        match self {
            Self::Running(at) | Self::Done(at, _) => *at,
        }
    }
}

#[derive(Default)]
struct Table {
    entries: HashMap<u128, Entry>,
    /// Ids in arrival order, with the time each was claimed.
    order: VecDeque<(Instant, u128)>,
}

impl Table {
    /// Forget ids older than [`DEDUP_TTL`], then the oldest past [`DEDUP_ENTRIES`].
    fn evict(&mut self, now: Instant) {
        while let Some((at, req)) = self.order.front().copied() {
            let stale = now.duration_since(at) > DEDUP_TTL;
            if !stale && self.order.len() <= DEDUP_ENTRIES {
                break;
            }
            self.order.pop_front();
            if self.entries.get(&req).is_some_and(|entry| entry.at() == at) {
                self.entries.remove(&req);
            }
        }
    }
}

/// The server's recent request ids and their replies (spec §6.1).
#[derive(Default)]
pub(super) struct Dedup {
    table: Mutex<Table>,
    cv: Condvar,
}

/// What a request with an id does.
pub(super) enum Claim<'a> {
    /// The first with this id: run it and [`Running::finish`] with its reply.
    Run(Running<'a>),
    /// Seen before: send this reply and apply nothing.
    Replay(Value),
}

/// The claim of a request that runs. Dropped without a reply (an unwind),
/// it releases the id, so a later request with it runs.
pub(super) struct Running<'a> {
    dedup: &'a Dedup,
    req: u128,
    at: Instant,
    done: bool,
}

impl Running<'_> {
    pub(super) fn finish(mut self, reply: &Value) {
        let mut table = lock_mutex(&self.dedup.table);
        if table
            .entries
            .get(&self.req)
            .is_some_and(|entry| entry.at() == self.at)
        {
            table
                .entries
                .insert(self.req, Entry::Done(self.at, reply.clone()));
        }
        self.done = true;
        self.dedup.cv.notify_all();
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut table = lock_mutex(&self.dedup.table);
        if table
            .entries
            .get(&self.req)
            .is_some_and(|entry| entry.at() == self.at)
        {
            table.entries.remove(&self.req);
        }
        self.dedup.cv.notify_all();
    }
}

impl Dedup {
    /// Claim `req`. A request with the same id still running elsewhere is
    /// waited for, then replayed.
    pub(super) fn claim(&self, req: u128) -> Claim<'_> {
        let mut table = lock_mutex(&self.table);
        loop {
            let now = Instant::now();
            table.evict(now);
            match table.entries.get(&req) {
                Some(Entry::Done(_, reply)) => return Claim::Replay(reply.clone()),
                Some(Entry::Running(_)) => table = wait_mutex(&self.cv, table),
                None => {
                    table.entries.insert(req, Entry::Running(now));
                    table.order.push_back((now, req));
                    return Claim::Run(Running {
                        dedup: self,
                        req,
                        at: now,
                        done: false,
                    });
                }
            }
        }
    }
}

/// The request id of `params`: 32 hex digits, or absent.
pub(super) fn request_id(params: &Value) -> Result<Option<u128>, StoreError> {
    match params.get("req") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.len() == 32 => u128::from_str_radix(text, 16)
            .map(Some)
            .map_err(|_| backend("param `req` is not 32 hex digits")),
        Some(_) => Err(backend("param `req` is not 32 hex digits")),
    }
}

/// A new request id for one logical attempt of a write: 32 hex digits.
pub(super) fn new_request_id() -> String {
    format!("{:032x}", uuid::Uuid::now_v7().as_u128())
}
