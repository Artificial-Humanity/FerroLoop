//! The public multi-op transaction ([`FerroStore::transaction`]) and the
//! staging of each write the public API shares with it.

use super::columnar::{self, ColumnDef, ColumnValue};
use super::layout::{Op, backend};
use super::{FerroStore, Txn, VectorMetric, VectorStorage, doc, state, vector};
use fl_core::StoreError;
use serde_json::Value;

/// The vector a `get_vector` returns: its dequantized values and metadata.
type StoredVector = (Vec<f32>, Option<Value>);

/// One transaction's view of the store, and the writes it stages: the
/// argument of [`FerroStore::transaction`].
///
/// Every read sees the transaction's own earlier writes. Nothing is visible
/// to anyone else until the body returns `Ok` and the whole transaction
/// commits as one sequence. A write method that returns `Err` wrote nothing,
/// and the transaction then does not commit at all, even if the body goes on
/// and returns `Ok`.
pub struct Transaction<'a> {
    txn: &'a mut Txn,
}

impl Transaction<'_> {
    /// `result`, and on `Err` the transaction is failed: it cannot commit.
    fn checked<T>(&mut self, result: Result<T, StoreError>) -> Result<T, StoreError> {
        if result.is_err() {
            self.txn.failed = true;
        }
        result
    }

    /// The document at `(collection, id)`, or `None`. A miss is not filled
    /// from a sync adapter, as [`FerroStore::get_doc`] may fill it.
    pub fn get_doc(&self, collection: &str, id: &str) -> Option<Value> {
        self.txn.get(collection, id)
    }

    /// Put one document. A reserved collection (a name starting `__`) is
    /// refused, as [`FerroStore::put_doc`] refuses it.
    pub fn put_doc(&mut self, collection: &str, id: &str, doc: &Value) -> Result<(), StoreError> {
        self.checked(doc::refuse_reserved(collection, "put_doc"))?;
        self.txn.put(collection, id, doc.clone());
        Ok(())
    }

    /// Delete one document. A missing id still stages a delete. A reserved
    /// collection is refused.
    pub fn delete_doc(&mut self, collection: &str, id: &str) -> Result<(), StoreError> {
        self.checked(doc::refuse_reserved(collection, "delete_doc"))?;
        self.txn.delete(collection, id);
        Ok(())
    }

    /// Every document in `collection`, in id order.
    pub fn scan(&self, collection: &str) -> Vec<(String, Value)> {
        self.txn.scan(collection)
    }

    /// Ids in `collection` whose top-level `field` matches `value`, in
    /// ascending order: the same match as [`FerroStore::query_by_field`].
    pub fn query_by_field(&self, collection: &str, field: &str, value: &str) -> Vec<String> {
        let mut ids = Vec::new();
        self.txn.visit(collection, |id, doc| {
            if state::field_matches(doc, field, value) {
                ids.push(id.to_string());
            }
        });
        ids
    }

    /// See [`FerroStore::create_column_table`].
    pub fn create_column_table(
        &mut self,
        name: &str,
        columns: &[ColumnDef],
    ) -> Result<(), StoreError> {
        let result = self.txn.stage_create_column_table(name, columns);
        self.checked(result)
    }

    /// See [`FerroStore::append_column_rows`].
    pub fn append_column_rows(
        &mut self,
        name: &str,
        rows: &[Vec<ColumnValue>],
    ) -> Result<(), StoreError> {
        let result = self.txn.stage_append_column_rows(name, rows);
        self.checked(result)
    }

    /// `true` when vector collection `name` exists, with this transaction's
    /// creates. Never creates it.
    pub fn has_vector_collection(&self, name: &str) -> bool {
        self.txn.vector(name).is_some()
    }

    /// See [`FerroStore::create_vector_collection`].
    #[allow(clippy::too_many_arguments)]
    pub fn create_vector_collection(
        &mut self,
        name: &str,
        dim: u32,
        storage: VectorStorage,
        metric: VectorMetric,
        m: u32,
        ef_construction: u32,
        ef_search: u32,
    ) -> Result<(), StoreError> {
        let result = self.txn.stage_create_vector_collection(
            name,
            dim,
            storage,
            metric,
            m,
            ef_construction,
            ef_search,
        );
        self.checked(result)
    }

    /// See [`FerroStore::upsert_vector`].
    pub fn upsert_vector(
        &mut self,
        collection: &str,
        id: &str,
        values: &[f32],
        metadata: Option<&Value>,
    ) -> Result<(), StoreError> {
        let result = self
            .txn
            .stage_upsert_vectors(collection, &[(id, values, metadata)]);
        self.checked(result)
    }

    /// See [`FerroStore::delete_vector`].
    pub fn delete_vector(&mut self, collection: &str, id: &str) -> Result<(), StoreError> {
        let result = self.txn.stage_delete_vector(collection, id);
        self.checked(result)
    }

    /// See [`FerroStore::get_vector`].
    pub fn get_vector(
        &self,
        collection: &str,
        id: &str,
    ) -> Result<Option<StoredVector>, StoreError> {
        self.txn.read_vector(collection, id)
    }
}

impl FerroStore {
    /// Run `body` as one transaction: every write it makes commits together,
    /// as one sequence (one WAL frame on disk), or none of them does.
    ///
    /// `Err` from `body` commits nothing and is returned. A body that wrote
    /// nothing commits nothing. A write method that returned `Err` to the
    /// body also stops the commit, even if the body goes on and returns `Ok`.
    ///
    /// **It closes read-modify-write races.** On `mem://` and `file://` the
    /// body runs under the store's write lock: on disk it joins a group
    /// commit (spec §4.3), under the file's `flock`, against the state other
    /// handles committed. A read inside the body and the write that depends
    /// on it see no commit between them. On a remote handle the commit is a
    /// compare-and-swap on the server's sequence, and `body` runs again
    /// when another writer committed first.
    ///
    /// **`body` must be free of side effects**: it may run more than once
    /// (on a remote handle), and its writes are discarded on any error. It
    /// must not call this store: on `mem://` and `file://` that deadlocks.
    /// The sync adapter is called after the commit, as for any other write.
    pub fn transaction<T>(
        &self,
        mut body: impl FnMut(&mut Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.with_write(|txn| body(&mut Transaction { txn }))
    }
}

impl Txn {
    pub(super) fn stage_create_column_table(
        &mut self,
        name: &str,
        columns: &[ColumnDef],
    ) -> Result<(), StoreError> {
        self.stage(Op::CreateColumnTable {
            name: name.to_string(),
            columns: columns.to_vec(),
        })
    }

    /// One row group. An empty `rows` stages nothing.
    pub(super) fn stage_append_column_rows(
        &mut self,
        name: &str,
        rows: &[Vec<ColumnValue>],
    ) -> Result<(), StoreError> {
        let Some(columns) = self.table_columns(name) else {
            return Err(backend(format!("no column table `{name}`")));
        };
        if rows.is_empty() {
            return Ok(());
        }
        let bytes = columnar::encode_row_group(columns, rows)?;
        self.stage(Op::AppendColumnGroup {
            name: name.to_string(),
            bytes,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn stage_create_vector_collection(
        &mut self,
        name: &str,
        dim: u32,
        storage: VectorStorage,
        metric: VectorMetric,
        m: u32,
        ef_construction: u32,
        ef_search: u32,
    ) -> Result<(), StoreError> {
        vector::check_create(name, dim, storage, m, ef_construction, ef_search)?;
        if self.vector(name).is_some() {
            return Err(backend(format!(
                "vector collection `{name}` already exists"
            )));
        }
        self.stage(Op::CreateVectorCollection {
            name: name.to_string(),
            dim,
            storage,
            metric,
            m,
            ef_construction,
            ef_search,
        })
    }

    /// Upsert every item into `collection`, in order: a repeated id ends as
    /// its last item. Every item is checked (id, length, finite components)
    /// before the first is staged, so a bad item stages nothing. The
    /// collection is copied once for the whole batch (spec §4.2).
    pub(super) fn stage_upsert_vectors(
        &mut self,
        collection: &str,
        items: &[(&str, &[f32], Option<&Value>)],
    ) -> Result<(), StoreError> {
        let Some(col) = self.vector(collection) else {
            return Err(backend(format!("no vector collection `{collection}`")));
        };
        let (dim, storage) = (col.dim, col.storage);
        let mut checked = Vec::with_capacity(items.len());
        for (id, values, metadata) in items {
            vector::check_vector_id(id)?;
            if u32::try_from(values.len()).ok() != Some(dim) {
                return Err(backend(format!(
                    "vector values have length {}, expected {dim}",
                    values.len()
                )));
            }
            checked.push(Op::UpsertVector {
                collection: collection.to_string(),
                id: (*id).to_string(),
                bytes: vector::quantize(storage, values)?,
                metadata: metadata.cloned(),
            });
        }
        for op in checked {
            self.stage(op)?;
        }
        Ok(())
    }

    pub(super) fn stage_delete_vector(
        &mut self,
        collection: &str,
        id: &str,
    ) -> Result<(), StoreError> {
        if self.vector(collection).is_none() {
            return Err(backend(format!("no vector collection `{collection}`")));
        }
        vector::check_vector_id(id)?;
        self.stage(Op::DeleteVector {
            collection: collection.to_string(),
            id: id.to_string(),
        })
    }

    /// Dequantized values and metadata. `Ok(None)` when the id is missing.
    pub(super) fn read_vector(
        &self,
        collection: &str,
        id: &str,
    ) -> Result<Option<StoredVector>, StoreError> {
        let Some(col) = self.vector(collection) else {
            return Err(backend(format!("no vector collection `{collection}`")));
        };
        vector::check_vector_id(id)?;
        let Some(record) = col.vectors.get(id) else {
            return Ok(None);
        };
        let values = vector::dequantize(col.storage, &record.bytes)?;
        Ok(Some((values, record.metadata.clone())))
    }
}
