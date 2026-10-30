//! Document store and FerroMQ log in a `.fls` file, and a socket server.
//! See `docs/ferrostore-spec.md`.

mod columnar;
mod doc;
mod layout;
mod migrate;
mod server;
mod state;
mod traits_catalog;
mod traits_state;
mod traits_wire;
mod traits_workflow;
mod vector;
mod ws;

#[cfg(test)]
mod tests;

use fl_core::StoreError;
use fl_core::ids::{GateId, ProjectId};
use layout::{
    HEADER_BYTES, Op, SLOT_LEN, Superblock, backend, corrupt, decode_snapshot, decode_wal,
    encode_frame, encode_snapshot, encode_snapshot_v2, encode_snapshot_v3, encode_superblock,
    parse_superblock,
};
use serde_json::{Value, json};
use state::{
    DocState, MQ_HIGH_WATER_ID, apply_commit, apply_frames, commit_ops, mq_hw_collection,
    mq_message_collection, next_mq_seq, stage_ops, unread_mq,
};

pub use columnar::{
    AttemptRollup, ColumnDef, ColumnFilter, ColumnPredicate, ColumnScan, ColumnType, ColumnValue,
    GateRunRollup, ZoneMap,
};
pub use migrate::migrate_redb_to_ferrostore;
pub use server::FerroStoreServer;
pub use state::MqRecord;
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, MutexGuard};
pub use vector::{Fusion, KnowledgeHit, VectorHit, VectorMetric, VectorStorage};

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
    tables: BTreeMap<String, columnar::ColumnTable>,
    vectors: BTreeMap<String, vector::VectorCollection>,
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
/// `unix://` and `ws://` are clients of a [`FerroStoreServer`] and do not open the file.
/// [`Self::auto`] is that client when the default socket is listening, and [`Self::open`] otherwise.
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

    /// If `default_socket_path_for_store(path)` accepts a Unix connection, return a client of that server.
    /// Otherwise `open(path)` in embedded mode.
    ///
    /// The successful `UnixStream` is the client. It is not dropped in favor of a second connect.
    /// A refused connect (nothing listening, or no socket file) is the embedded fallback, not an error.
    /// `open` errors are still returned.
    pub fn auto(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let sock = default_socket_path_for_store(path);
        match UnixStream::connect(&sock) {
            Ok(stream) => {
                let remote = server::client_from_unix(stream)?;
                Ok(Self {
                    inner: Inner::Remote(remote),
                    subs: Mutex::new(HashMap::new()),
                    label: sock.display().to_string(),
                })
            }
            // Nothing listening, or the socket file is missing. Other connect
            // errors stay errors. `open` errors are not swallowed.
            Err(err) if refused_connect(&err) => Self::open(path),
            Err(err) => Err(backend(format!("connect {}: {err}", sock.display()))),
        }
    }

    /// `mem://`, `file://`, `unix://`, and `ws://`. `wss://` is recognized and refused.
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
        if uri.starts_with("wss://") {
            return Err(backend("wss is not implemented"));
        }
        if let Some(rest) = uri.strip_prefix("ws://") {
            let remote = server::connect_ws(rest)?;
            return Ok(Self {
                inner: Inner::Remote(remote),
                subs: Mutex::new(HashMap::new()),
                label: uri.to_string(),
            });
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

    /// Create a column table. An existing name is an error and writes nothing.
    pub fn create_column_table(&self, name: &str, columns: &[ColumnDef]) -> Result<(), StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            let defs = columns
                .iter()
                .map(columnar::column_def_json)
                .collect::<Vec<_>>();
            return remote.call_unit(
                "create_column_table",
                json!({"name": name, "columns": defs}),
            );
        }
        let name = name.to_string();
        let columns = columns.to_vec();
        self.with_write(move |txn| {
            columnar::check_new_table(&txn.tables, &name, &columns)?;
            txn.tables.insert(
                name.clone(),
                columnar::ColumnTable {
                    columns: columns.clone(),
                    groups: Vec::new(),
                },
            );
            txn.ops.push(Op::CreateColumnTable {
                name: name.clone(),
                columns: columns.clone(),
            });
            Ok(())
        })
    }

    /// Append one row group. An empty `rows` writes nothing. A bad row writes nothing.
    pub fn append_column_rows(
        &self,
        name: &str,
        rows: &[Vec<ColumnValue>],
    ) -> Result<(), StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            let mut encoded = Vec::with_capacity(rows.len());
            for row in rows {
                let mut cells = Vec::with_capacity(row.len());
                for cell in row {
                    cells.push(columnar::cell_json(cell)?);
                }
                encoded.push(Value::Array(cells));
            }
            return remote.call_unit("append_column_rows", json!({"name": name, "rows": encoded}));
        }
        let name = name.to_string();
        let rows = rows.to_vec();
        self.with_write(move |txn| {
            let Some(table) = txn.tables.get(&name) else {
                return Err(backend(format!("no column table `{name}`")));
            };
            if rows.is_empty() {
                return Ok(());
            }
            let bytes = columnar::encode_row_group(&table.columns, &rows)?;
            let Some(table) = txn.tables.get_mut(&name) else {
                return Err(backend(format!("no column table `{name}`")));
            };
            table.groups.push(bytes.clone());
            txn.ops.push(Op::AppendColumnGroup {
                name: name.clone(),
                bytes,
            });
            Ok(())
        })
    }

    /// Scan `name`. A row group whose zone map excludes any filter is skipped
    /// before its values are decoded. Filters are AND.
    pub fn scan_column_table(
        &self,
        name: &str,
        filters: &[ColumnFilter],
    ) -> Result<ColumnScan, StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            let mut encoded = Vec::with_capacity(filters.len());
            for filter in filters {
                encoded.push(columnar::filter_json(filter)?);
            }
            let value = remote.call(
                "scan_column_table",
                json!({"name": name, "filters": encoded}),
            )?;
            return columnar::parse_scan(&value);
        }
        let name = name.to_string();
        let filters = filters.to_vec();
        self.with_read(move |txn| {
            let Some(table) = txn.tables.get(&name) else {
                return Err(backend(format!("no column table `{name}`")));
            };
            columnar::scan_groups(&table.columns, &table.groups, &filters)
        })
    }

    /// Sum attempt ledger rows through an in-memory columnar scan. `None` is every attempt.
    pub fn attempt_rollup(&self, project: Option<&ProjectId>) -> Result<AttemptRollup, StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            let project = match project {
                Some(id) => Value::String(id.as_str().to_string()),
                None => Value::Null,
            };
            let value = remote.call("attempt_rollup", json!({"project": project}))?;
            return columnar::parse_attempt_rollup(&value);
        }
        let project = project.cloned();
        self.with_read(move |txn| columnar::attempt_rollup(txn, project.as_ref()))
    }

    /// Sum gate-run ledger rows through an in-memory columnar scan. `None` is every run.
    pub fn gate_run_rollup(&self, gate: Option<&GateId>) -> Result<GateRunRollup, StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            let gate = match gate {
                Some(id) => Value::String(id.as_str().to_string()),
                None => Value::Null,
            };
            let value = remote.call("gate_run_rollup", json!({"gate": gate}))?;
            return columnar::parse_gate_rollup(&value);
        }
        let gate = gate.cloned();
        self.with_read(move |txn| columnar::gate_run_rollup(txn, gate.as_ref()))
    }

    /// Create an empty vector collection. An existing name is an error and writes nothing.
    /// A document collection may use the same name; the two maps are distinct.
    #[allow(clippy::too_many_arguments)]
    pub fn create_vector_collection(
        &self,
        name: &str,
        dim: u32,
        storage: VectorStorage,
        metric: VectorMetric,
        m: u32,
        ef_construction: u32,
        ef_search: u32,
    ) -> Result<(), StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            return remote.call_unit(
                "create_vector_collection",
                json!({
                    "name": name,
                    "dim": dim,
                    "storage": vector::storage_name(storage),
                    "metric": vector::metric_name(metric),
                    "m": m,
                    "ef_construction": ef_construction,
                    "ef_search": ef_search,
                }),
            );
        }
        let name = name.to_string();
        self.with_write(move |txn| {
            vector::check_create(&name, dim, storage, m, ef_construction, ef_search)?;
            if txn.vectors.contains_key(&name) {
                return Err(backend(format!(
                    "vector collection `{name}` already exists"
                )));
            }
            txn.ops.push(Op::CreateVectorCollection {
                name: name.clone(),
                dim,
                storage,
                metric,
                m,
                ef_construction,
                ef_search,
            });
            Ok(())
        })
    }

    /// Insert or replace one vector. `None` metadata is absent. `Some(Null)` stores JSON null.
    pub fn upsert_vector(
        &self,
        collection: &str,
        id: &str,
        values: &[f32],
        metadata: Option<&Value>,
    ) -> Result<(), StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            let mut params = serde_json::Map::new();
            params.insert("collection".into(), json!(collection));
            params.insert("id".into(), json!(id));
            params.insert("values".into(), vector::values_json(values)?);
            if let Some(metadata) = metadata {
                params.insert("metadata".into(), metadata.clone());
            }
            return remote.call_unit("upsert_vector", Value::Object(params));
        }
        let collection = collection.to_string();
        let id = id.to_string();
        let values = values.to_vec();
        let metadata = metadata.cloned();
        self.with_write(move |txn| {
            let Some(col) = txn.vectors.get(&collection) else {
                return Err(backend(format!("no vector collection `{collection}`")));
            };
            let dim = col.dim;
            let storage = col.storage;
            vector::check_vector_id(&id)?;
            if u32::try_from(values.len()).ok() != Some(dim) {
                return Err(backend(format!(
                    "vector values have length {}, expected {dim}",
                    values.len()
                )));
            }
            let bytes = vector::quantize(storage, &values)?;
            txn.ops.push(Op::UpsertVector {
                collection: collection.clone(),
                id: id.clone(),
                bytes,
                metadata: metadata.clone(),
            });
            Ok(())
        })
    }

    /// Delete one vector. A missing id still commits. A missing collection writes nothing.
    pub fn delete_vector(&self, collection: &str, id: &str) -> Result<(), StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            return remote.call_unit("delete_vector", json!({"collection": collection, "id": id}));
        }
        let collection = collection.to_string();
        let id = id.to_string();
        self.with_write(move |txn| {
            if !txn.vectors.contains_key(&collection) {
                return Err(backend(format!("no vector collection `{collection}`")));
            }
            vector::check_vector_id(&id)?;
            txn.ops.push(Op::DeleteVector {
                collection: collection.clone(),
                id: id.clone(),
            });
            Ok(())
        })
    }

    /// Dequantized values and metadata. `Ok(None)` when the id is missing.
    #[allow(clippy::type_complexity)]
    pub fn get_vector(
        &self,
        collection: &str,
        id: &str,
    ) -> Result<Option<(Vec<f32>, Option<Value>)>, StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            return vector::parse_get(
                &remote.call("get_vector", json!({"collection": collection, "id": id}))?,
            );
        }
        let collection = collection.to_string();
        let id = id.to_string();
        self.with_read(move |txn| {
            let Some(col) = txn.vectors.get(&collection) else {
                return Err(backend(format!("no vector collection `{collection}`")));
            };
            vector::check_vector_id(&id)?;
            let Some(record) = col.vectors.get(&id) else {
                return Ok(None);
            };
            let values = vector::dequantize(col.storage, &record.bytes)?;
            Ok(Some((values, record.metadata.clone())))
        })
    }

    /// Higher-better scores, at most `limit`. `ef` of `None` uses the collection `ef_search`.
    /// `limit == 0` or `ef == Some(0)` is an empty vec.
    pub fn search_vectors(
        &self,
        collection: &str,
        values: &[f32],
        limit: usize,
        ef: Option<u32>,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            if limit == 0 || ef == Some(0) {
                return Ok(Vec::new());
            }
            let mut params = serde_json::Map::new();
            params.insert("collection".into(), json!(collection));
            params.insert("values".into(), vector::values_json(values)?);
            params.insert("limit".into(), json!(limit));
            if let Some(ef) = ef {
                params.insert("ef".into(), json!(ef));
            }
            return vector::parse_hits(&remote.call("search_vectors", Value::Object(params))?);
        }
        let collection = collection.to_string();
        let values = values.to_vec();
        self.with_read(move |txn| {
            let Some(col) = txn.vectors.get(&collection) else {
                return Err(backend(format!("no vector collection `{collection}`")));
            };
            vector::search(col, &values, limit, ef)
        })
    }

    /// Fuse at most `limit` BM25 hits with at most `limit` vector hits, then keep `limit`.
    pub fn search_hybrid(
        &self,
        doc_collection: &str,
        vector_collection: &str,
        query: &str,
        values: &[f32],
        limit: usize,
        fusion: Fusion,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            if limit == 0 {
                return Ok(Vec::new());
            }
            return vector::parse_hits(&remote.call(
                "search_hybrid",
                hybrid_params(
                    doc_collection,
                    vector_collection,
                    query,
                    values,
                    limit,
                    fusion,
                )?,
            )?);
        }
        let doc_collection = doc_collection.to_string();
        let vector_collection = vector_collection.to_string();
        let query = query.to_string();
        let values = values.to_vec();
        self.with_read(move |txn| {
            state::search_hybrid(
                &txn.docs,
                &txn.vectors,
                &doc_collection,
                &vector_collection,
                &query,
                &values,
                limit,
                fusion,
            )
        })
    }

    /// Same fusion as [`Self::search_hybrid`]. `doc` is the document at that id, if any.
    /// `metadata` is the vector metadata when the id is in the vector collection.
    pub fn search_knowledge(
        &self,
        doc_collection: &str,
        vector_collection: &str,
        query: &str,
        values: &[f32],
        limit: usize,
        fusion: Fusion,
    ) -> Result<Vec<KnowledgeHit>, StoreError> {
        if let Inner::Remote(remote) = &self.inner {
            if limit == 0 {
                return Ok(Vec::new());
            }
            return vector::parse_knowledge(&remote.call(
                "search_knowledge",
                hybrid_params(
                    doc_collection,
                    vector_collection,
                    query,
                    values,
                    limit,
                    fusion,
                )?,
            )?);
        }
        let doc_collection = doc_collection.to_string();
        let vector_collection = vector_collection.to_string();
        let query = query.to_string();
        let values = values.to_vec();
        self.with_read(move |txn| {
            state::search_knowledge(
                &txn.docs,
                &txn.vectors,
                &doc_collection,
                &vector_collection,
                &query,
                &values,
                limit,
                fusion,
            )
        })
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
        let state = self.read_state()?;
        let txn = Txn {
            docs: state.docs,
            tables: state.tables,
            vectors: state.vectors,
            ops: Vec::new(),
        };
        body(&txn)
    }

    fn read_state(&self) -> Result<DocState, StoreError> {
        match &self.inner {
            Inner::Memory(state) => Ok(lock_mutex(state).clone()),
            Inner::Disk(disk) => Ok(read_consistent(&disk.read)?.2),
            Inner::Remote(remote) => remote.read_state(),
        }
    }

    /// Compare-and-swap `ops` onto a store whose `last_seq` is still `expected_seq`.
    ///
    /// `Ok(false)` means the sequence moved and nothing was written. An empty `ops`
    /// list that matches returns `Ok(true)` and does not advance `last_seq`. A
    /// non-empty disk success is one WAL frame at `expected_seq + 1`. Ops that do
    /// not apply return `Err` and write nothing. Remote handles do not call this;
    /// the server's store is memory or disk.
    fn commit_if_seq(&self, expected_seq: u64, ops: &[Op]) -> Result<bool, StoreError> {
        match &self.inner {
            Inner::Memory(slot) => {
                let mut state = lock_mutex(slot);
                if state.last_seq != expected_seq {
                    return Ok(false);
                }
                commit_ops(&mut state, ops)?;
                Ok(true)
            }
            Inner::Disk(disk) => commit_if_seq_disk(disk, expected_seq, ops),
            Inner::Remote(_) => Err(remote_unsupported()),
        }
    }

    /// Run `body` against a cloned document map.
    ///
    /// Memory holds the document mutex for the whole call. Disk takes `LOCK_EX`,
    /// re-reads a consistent snapshot, then builds the transaction from that
    /// state. `Ok` with at least one op appends one WAL frame (`seq = last_seq + 1`)
    /// and publishes one superblock. `Err` or an empty op list writes nothing.
    /// A remote handle compare-and-swaps those ops and runs `body` again when
    /// `last_seq` changed. The body may run more than once only on a remote handle.
    fn with_write<T>(
        &self,
        mut body: impl FnMut(&mut Txn) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        match &self.inner {
            Inner::Remote(remote) => loop {
                let state = remote.read_state()?;
                let seq_base = state.last_seq;
                let mut txn = Txn {
                    docs: state.docs,
                    tables: state.tables,
                    vectors: state.vectors,
                    ops: Vec::new(),
                };
                let result = body(&mut txn)?;
                if txn.ops.is_empty() {
                    return Ok(result);
                }
                if remote.commit_ops(seq_base, &txn.ops)? {
                    return Ok(result);
                }
            },
            Inner::Memory(slot) => {
                let mut state = lock_mutex(slot);
                let mut txn = Txn {
                    docs: state.docs.clone(),
                    tables: state.tables.clone(),
                    vectors: state.vectors.clone(),
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

fn hybrid_params(
    doc_collection: &str,
    vector_collection: &str,
    query: &str,
    values: &[f32],
    limit: usize,
    fusion: Fusion,
) -> Result<Value, StoreError> {
    vector::validate_fusion(fusion)?;
    let fusion = match fusion {
        Fusion::Rrf { k } => json!({"method": "rrf", "k": k}),
        Fusion::Weighted { bm25, vector } => {
            json!({
                "method": "weighted",
                "bm25": bm25,
                "vector": vector,
            })
        }
    };
    Ok(json!({
        "doc_collection": doc_collection,
        "vector_collection": vector_collection,
        "query": query,
        "values": vector::values_json(values)?,
        "limit": limit,
        "fusion": fusion,
    }))
}

/// `dir/store.fls` -> `dir/store.fls.sock`.
/// A path with no file name (`/` or `..`) -> that path joined with `ferrostore.sock`.
pub fn default_socket_path_for_store(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    match path.file_name() {
        Some(name) => {
            let mut sock_name = name.to_os_string();
            sock_name.push(".sock");
            path.with_file_name(sock_name)
        }
        None => path.join("ferrostore.sock"),
    }
}

fn refused_connect(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
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

fn commit_if_seq_disk(disk: &Disk, expected_seq: u64, ops: &[Op]) -> Result<bool, StoreError> {
    let file = lock_mutex(&disk.write);
    let _locked = FlockGuard::acquire(file.as_raw_fd())?;
    let (index, sb, state) = read_consistent(&file)?;
    if state.last_seq != expected_seq {
        return Ok(false);
    }
    if ops.is_empty() {
        return Ok(true);
    }
    let seq = stage_ops(state, ops)?.last_seq;
    let frame = encode_frame(seq, ops)?;
    append_frame(&file, index, &sb, &frame)?;
    Ok(true)
}

fn with_write_disk<T>(
    disk: &Disk,
    mut body: impl FnMut(&mut Txn) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let file = lock_mutex(&disk.write);
    let _locked = FlockGuard::acquire(file.as_raw_fd())?;
    let (index, sb, state) = read_consistent(&file)?;
    let baseline = state.clone();
    let mut txn = Txn {
        docs: state.docs,
        tables: state.tables,
        vectors: state.vectors,
        ops: Vec::new(),
    };
    let result = body(&mut txn)?;
    if txn.ops.is_empty() {
        return Ok(result);
    }
    let seq = stage_ops(baseline, &txn.ops)?.last_seq;
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
    let seq = stage_ops(state, std::slice::from_ref(&op))?.last_seq;
    let frame = encode_frame(seq, std::slice::from_ref(&op))?;
    append_frame(&file, index, &sb, &frame)?;
    Ok(extra)
}

fn checkpoint_disk(disk: &Disk) -> Result<(), StoreError> {
    let file = lock_mutex(&disk.write);
    let _locked = FlockGuard::acquire(file.as_raw_fd())?;
    let (index, sb, state) = read_consistent(&file)?;
    let blob = if !state.vectors.is_empty() {
        encode_snapshot_v3(state.last_seq, &state.docs, &state.tables, &state.vectors)?
    } else if state.tables.is_empty() {
        encode_snapshot(state.last_seq, &state.docs)?
    } else {
        encode_snapshot_v2(state.last_seq, &state.docs, &state.tables)?
    };
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
            tables: snap.tables,
            vectors: snap.vectors,
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
