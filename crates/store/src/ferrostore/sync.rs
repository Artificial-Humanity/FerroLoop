//! Process-local sync socket. Nothing here is written to the `.fls` file.

use super::columnar::{self, ColumnDef};
use super::layout::Op;
use super::vector::{self, VectorMetric, VectorStorage};
use fl_core::StoreError;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// One committed mutation, in the same shape `commit_ops` carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SyncOp {
    #[serde(rename = "put")]
    PutDoc {
        collection: String,
        id: String,
        doc: Value,
    },
    #[serde(rename = "delete")]
    DeleteDoc {
        collection: String,
        id: String,
    },
    CreateColumnTable {
        name: String,
        #[serde(serialize_with = "ser_columns", deserialize_with = "de_columns")]
        columns: Vec<ColumnDef>,
    },
    AppendColumnGroup {
        name: String,
        bytes: Vec<u8>,
    },
    CreateVectorCollection {
        name: String,
        dim: u32,
        #[serde(serialize_with = "ser_storage", deserialize_with = "de_storage")]
        storage: VectorStorage,
        #[serde(serialize_with = "ser_metric", deserialize_with = "de_metric")]
        metric: VectorMetric,
        m: u32,
        ef_construction: u32,
        ef_search: u32,
    },
    UpsertVector {
        collection: String,
        id: String,
        bytes: Vec<u8>,
        /// Absent is `None`. JSON null is `Some(Null)`, not a missing field.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_metadata"
        )]
        metadata: Option<Value>,
    },
    DeleteVector {
        collection: String,
        id: String,
    },
}

impl SyncOp {
    /// Lossless copy of a committed [`Op`].
    pub fn from_op(op: &Op) -> Self {
        match op {
            Op::Put {
                collection,
                id,
                doc,
            } => Self::PutDoc {
                collection: collection.clone(),
                id: id.clone(),
                doc: doc.clone(),
            },
            Op::Delete { collection, id } => Self::DeleteDoc {
                collection: collection.clone(),
                id: id.clone(),
            },
            Op::CreateColumnTable { name, columns } => Self::CreateColumnTable {
                name: name.clone(),
                columns: columns.clone(),
            },
            Op::AppendColumnGroup { name, bytes } => Self::AppendColumnGroup {
                name: name.clone(),
                bytes: bytes.clone(),
            },
            Op::CreateVectorCollection {
                name,
                dim,
                storage,
                metric,
                m,
                ef_construction,
                ef_search,
            } => Self::CreateVectorCollection {
                name: name.clone(),
                dim: *dim,
                storage: *storage,
                metric: *metric,
                m: *m,
                ef_construction: *ef_construction,
                ef_search: *ef_search,
            },
            Op::UpsertVector {
                collection,
                id,
                bytes,
                metadata,
            } => Self::UpsertVector {
                collection: collection.clone(),
                id: id.clone(),
                bytes: bytes.clone(),
                metadata: metadata.clone(),
            },
            Op::DeleteVector { collection, id } => Self::DeleteVector {
                collection: collection.clone(),
                id: id.clone(),
            },
        }
    }

    /// Inverse of [`Self::from_op`].
    pub fn into_op(self) -> Op {
        match self {
            Self::PutDoc {
                collection,
                id,
                doc,
            } => Op::Put {
                collection,
                id,
                doc,
            },
            Self::DeleteDoc { collection, id } => Op::Delete { collection, id },
            Self::CreateColumnTable { name, columns } => Op::CreateColumnTable { name, columns },
            Self::AppendColumnGroup { name, bytes } => Op::AppendColumnGroup { name, bytes },
            Self::CreateVectorCollection {
                name,
                dim,
                storage,
                metric,
                m,
                ef_construction,
                ef_search,
            } => Op::CreateVectorCollection {
                name,
                dim,
                storage,
                metric,
                m,
                ef_construction,
                ef_search,
            },
            Self::UpsertVector {
                collection,
                id,
                bytes,
                metadata,
            } => Op::UpsertVector {
                collection,
                id,
                bytes,
                metadata,
            },
            Self::DeleteVector { collection, id } => Op::DeleteVector { collection, id },
        }
    }
}

/// One committed sequence and the ops that advanced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncBatch {
    pub seq: u64,
    pub ops: Vec<SyncOp>,
}

/// When the store calls [`SyncAdapter::on_commit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Call `on_commit` after each commit. A failure stays pending.
    WriteThrough,
    /// Queue commits until [`super::FerroStore::sync_flush`].
    WriteBehind,
    /// Queue commits, and fill a document miss from `fetch_document`.
    LocalCache,
    /// Queue commits, fill misses, and accept `sync_prewarm` batches.
    CloudPreCache,
}

/// Cloud or peer replication. The store does not link a cloud SDK.
pub trait SyncAdapter: Send + Sync {
    fn name(&self) -> &str;

    fn on_commit(&self, batch: &SyncBatch) -> Result<(), StoreError>;

    fn pull_since(&self, _after_seq: u64) -> Result<Vec<SyncBatch>, StoreError> {
        Ok(Vec::new())
    }

    fn fetch_document(&self, _: &str, _: &str) -> Result<Option<Value>, StoreError> {
        Ok(None)
    }

    fn prewarm(&self, _: &[&str]) -> Result<Vec<SyncBatch>, StoreError> {
        Ok(Vec::new())
    }
}

fn ser_columns<S: Serializer>(columns: &[ColumnDef], serializer: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(columns.len()))?;
    for column in columns {
        seq.serialize_element(&columnar::column_def_json(column))?;
    }
    seq.end()
}

fn de_columns<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<ColumnDef>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    columnar::parse_columns(&value).map_err(D::Error::custom)
}

fn ser_storage<S: Serializer>(storage: &VectorStorage, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(vector::storage_name(*storage))
}

fn de_storage<'de, D: Deserializer<'de>>(deserializer: D) -> Result<VectorStorage, D::Error> {
    let name = String::deserialize(deserializer)?;
    vector::storage_from_name(&name).map_err(D::Error::custom)
}

fn ser_metric<S: Serializer>(metric: &VectorMetric, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(vector::metric_name(*metric))
}

fn de_metric<'de, D: Deserializer<'de>>(deserializer: D) -> Result<VectorMetric, D::Error> {
    let name = String::deserialize(deserializer)?;
    vector::metric_from_name(&name).map_err(D::Error::custom)
}

/// JSON null stays `Some(Null)`. A missing field uses `#[serde(default)]` and
/// does not call this function.
fn de_metadata<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Ok(Some(Value::deserialize(deserializer)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ferrostore::{FerroStore, FerroStoreServer};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    fn assert_roundtrip(op: Op) {
        let sync = SyncOp::from_op(&op);
        let value = serde_json::to_value(&sync).unwrap();
        let decoded: SyncOp = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, sync);
        assert_eq!(SyncOp::from_op(&decoded.into_op()), sync);
    }

    #[test]
    fn sync_op_json_round_trip_covers_every_variant() {
        assert_roundtrip(Op::Put {
            collection: "docs".into(),
            id: "a".into(),
            doc: json!({"n": 1, "ok": true}),
        });
        assert_roundtrip(Op::Delete {
            collection: "docs".into(),
            id: "a".into(),
        });
        assert_roundtrip(Op::CreateColumnTable {
            name: "t".into(),
            columns: vec![
                ColumnDef {
                    name: "n".into(),
                    ty: columnar::ColumnType::Int64,
                },
                ColumnDef {
                    name: "s".into(),
                    ty: columnar::ColumnType::Utf8,
                },
            ],
        });
        assert_roundtrip(Op::AppendColumnGroup {
            name: "t".into(),
            bytes: vec![0, 255],
        });
        assert_roundtrip(Op::CreateVectorCollection {
            name: "v".into(),
            dim: 2,
            storage: VectorStorage::F16,
            metric: VectorMetric::DotProduct,
            m: 4,
            ef_construction: 8,
            ef_search: 3,
        });
        assert_roundtrip(Op::UpsertVector {
            collection: "v".into(),
            id: "a".into(),
            bytes: vec![0, 255],
            metadata: None,
        });
        assert_roundtrip(Op::UpsertVector {
            collection: "v".into(),
            id: "b".into(),
            bytes: vec![1, 2, 3],
            metadata: Some(Value::Null),
        });
        assert_roundtrip(Op::DeleteVector {
            collection: "v".into(),
            id: "a".into(),
        });

        let absent = SyncOp::from_op(&Op::UpsertVector {
            collection: "v".into(),
            id: "a".into(),
            bytes: vec![0],
            metadata: None,
        });
        let absent_json = serde_json::to_value(&absent).unwrap();
        assert_eq!(absent_json["op"], "upsert_vector");
        assert!(absent_json.get("metadata").is_none());
        assert_eq!(absent_json["bytes"], json!([0]));

        let null_meta = SyncOp::from_op(&Op::UpsertVector {
            collection: "v".into(),
            id: "b".into(),
            bytes: vec![9],
            metadata: Some(Value::Null),
        });
        let null_json = serde_json::to_value(&null_meta).unwrap();
        assert!(null_json["metadata"].is_null());
        let back: SyncOp = serde_json::from_value(null_json).unwrap();
        assert_eq!(back, null_meta);

        let table = SyncOp::from_op(&Op::CreateColumnTable {
            name: "t".into(),
            columns: vec![ColumnDef {
                name: "n".into(),
                ty: columnar::ColumnType::TimestampMs,
            }],
        });
        assert_eq!(
            serde_json::to_value(&table).unwrap(),
            json!({
                "op": "create_column_table",
                "name": "t",
                "columns": [{"name": "n", "ty": "timestamp_ms"}]
            })
        );
        let vector = SyncOp::from_op(&Op::CreateVectorCollection {
            name: "v".into(),
            dim: 2,
            storage: VectorStorage::F32,
            metric: VectorMetric::Cosine,
            m: 2,
            ef_construction: 4,
            ef_search: 1,
        });
        assert_eq!(
            serde_json::to_value(&vector).unwrap(),
            json!({
                "op": "create_vector_collection",
                "name": "v",
                "dim": 2,
                "storage": "f32",
                "metric": "cosine",
                "m": 2,
                "ef_construction": 4,
                "ef_search": 1
            })
        );
        let batch = SyncBatch {
            seq: 3,
            ops: vec![SyncOp::DeleteDoc {
                collection: "docs".into(),
                id: "a".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&batch).unwrap(),
            json!({"seq": 3, "ops": [{"op": "delete", "collection": "docs", "id": "a"}]})
        );
        let put = SyncOp::PutDoc {
            collection: "docs".into(),
            id: "a".into(),
            doc: json!({"n": 1}),
        };
        assert_eq!(serde_json::to_value(&put).unwrap()["op"], "put");
    }

    struct FakeSync {
        commits: Mutex<Vec<SyncBatch>>,
        fail: AtomicBool,
        fetches: AtomicUsize,
        fetch: Mutex<Option<Value>>,
        pull: Mutex<Vec<SyncBatch>>,
        prewarm: Mutex<Vec<SyncBatch>>,
        pull_after: Mutex<Vec<u64>>,
    }

    impl FakeSync {
        fn new() -> Self {
            Self {
                commits: Mutex::new(Vec::new()),
                fail: AtomicBool::new(false),
                fetches: AtomicUsize::new(0),
                fetch: Mutex::new(None),
                pull: Mutex::new(Vec::new()),
                prewarm: Mutex::new(Vec::new()),
                pull_after: Mutex::new(Vec::new()),
            }
        }

        fn commits(&self) -> Vec<SyncBatch> {
            self.commits.lock().unwrap().clone()
        }
    }

    impl SyncAdapter for FakeSync {
        fn name(&self) -> &str {
            "fake"
        }

        fn on_commit(&self, batch: &SyncBatch) -> Result<(), StoreError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(StoreError::Backend(
                    "sync adapter rejected the batch".into(),
                ));
            }
            self.commits.lock().unwrap().push(batch.clone());
            Ok(())
        }

        fn pull_since(&self, after_seq: u64) -> Result<Vec<SyncBatch>, StoreError> {
            self.pull_after.lock().unwrap().push(after_seq);
            Ok(self.pull.lock().unwrap().clone())
        }

        fn fetch_document(&self, _: &str, _: &str) -> Result<Option<Value>, StoreError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            Ok(self.fetch.lock().unwrap().clone())
        }

        fn prewarm(&self, _: &[&str]) -> Result<Vec<SyncBatch>, StoreError> {
            Ok(self.prewarm.lock().unwrap().clone())
        }
    }

    fn attach(mode: SyncMode) -> (FerroStore, Arc<FakeSync>) {
        let fake = Arc::new(FakeSync::new());
        let store = FerroStore::in_memory()
            .with_sync_adapter(mode, Arc::clone(&fake) as Arc<dyn SyncAdapter>)
            .unwrap();
        (store, fake)
    }

    #[test]
    fn write_through_notifies_each_commit() {
        let (store, fake) = attach(SyncMode::WriteThrough);
        store.put_doc("docs", "a", &json!({"n": 1})).unwrap();
        store.put_doc("docs", "b", &json!({"n": 2})).unwrap();
        let commits = fake.commits();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].seq, 1);
        assert_eq!(
            commits[0].ops,
            vec![SyncOp::PutDoc {
                collection: "docs".into(),
                id: "a".into(),
                doc: json!({"n": 1}),
            }]
        );
        assert_eq!(commits[1].seq, 2);
        assert_eq!(
            commits[1].ops,
            vec![SyncOp::PutDoc {
                collection: "docs".into(),
                id: "b".into(),
                doc: json!({"n": 2}),
            }]
        );
        assert_eq!(store.synced_seq(), 2);
        assert_eq!(store.sync_mode(), Some(SyncMode::WriteThrough));
    }

    #[test]
    fn write_behind_flushes_in_commit_order() {
        let (store, fake) = attach(SyncMode::WriteBehind);
        store.put_doc("docs", "a", &json!({"n": 1})).unwrap();
        store.put_doc("docs", "b", &json!({"n": 2})).unwrap();
        assert!(fake.commits().is_empty());
        assert_eq!(store.synced_seq(), 0);
        assert_eq!(store.sync_flush().unwrap(), 2);
        let commits = fake.commits();
        assert_eq!(
            commits.iter().map(|batch| batch.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(store.synced_seq(), 2);
        assert_eq!(store.sync_flush().unwrap(), 2);
        assert_eq!(fake.commits().len(), 2);
    }

    #[test]
    fn failed_write_through_stays_pending_until_flush() {
        let (store, fake) = attach(SyncMode::WriteThrough);
        fake.fail.store(true, Ordering::SeqCst);
        let err = store.put_doc("docs", "a", &json!({"n": 1})).unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)));
        assert_eq!(store.get_doc("docs", "a").unwrap(), Some(json!({"n": 1})));
        assert!(fake.commits().is_empty());
        assert_eq!(store.synced_seq(), 0);
        fake.fail.store(false, Ordering::SeqCst);
        assert_eq!(store.sync_flush().unwrap(), 1);
        assert_eq!(fake.commits().len(), 1);
        assert_eq!(fake.commits()[0].seq, 1);
        assert_eq!(store.synced_seq(), 1);
    }

    #[test]
    fn local_cache_fills_a_miss_once_and_keeps_json_null() {
        let (store, fake) = attach(SyncMode::LocalCache);
        *fake.fetch.lock().unwrap() = Some(json!({"cached": true}));
        assert_eq!(
            store.get_doc("docs", "a").unwrap(),
            Some(json!({"cached": true}))
        );
        assert!(fake.commits().is_empty());
        assert_eq!(fake.fetches.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.get_doc("docs", "a").unwrap(),
            Some(json!({"cached": true}))
        );
        assert_eq!(fake.fetches.load(Ordering::SeqCst), 1);

        let store = FerroStore::in_memory();
        store.put_doc("docs", "n", &Value::Null).unwrap();
        store
            .attach_sync_adapter(
                SyncMode::LocalCache,
                Arc::clone(&fake) as Arc<dyn SyncAdapter>,
            )
            .unwrap();
        let before = fake.fetches.load(Ordering::SeqCst);
        assert_eq!(store.get_doc("docs", "n").unwrap(), Some(Value::Null));
        assert_eq!(fake.fetches.load(Ordering::SeqCst), before);
    }

    #[test]
    fn write_through_miss_does_not_fetch() {
        let (store, fake) = attach(SyncMode::WriteThrough);
        *fake.fetch.lock().unwrap() = Some(json!({"cached": true}));
        assert_eq!(store.get_doc("docs", "missing").unwrap(), None);
        assert_eq!(fake.fetches.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cloud_pre_cache_prewarm_applies_without_on_commit() {
        let (store, fake) = attach(SyncMode::CloudPreCache);
        *fake.prewarm.lock().unwrap() = vec![SyncBatch {
            seq: 1,
            ops: vec![SyncOp::PutDoc {
                collection: "docs".into(),
                id: "warm".into(),
                doc: json!({"ready": true}),
            }],
        }];
        assert_eq!(store.sync_prewarm(&["docs"]).unwrap(), 1);
        assert_eq!(
            store.get_doc("docs", "warm").unwrap(),
            Some(json!({"ready": true}))
        );
        assert!(fake.commits().is_empty());
        assert_eq!(store.synced_seq(), 1);
    }

    #[test]
    fn sync_pull_applies_the_next_batch_and_rejects_a_gap() {
        let (store, fake) = attach(SyncMode::WriteBehind);
        *fake.pull.lock().unwrap() = vec![SyncBatch {
            seq: 1,
            ops: vec![SyncOp::PutDoc {
                collection: "docs".into(),
                id: "a".into(),
                doc: json!({"n": 1}),
            }],
        }];
        assert_eq!(store.sync_pull().unwrap(), 1);
        assert_eq!(store.get_doc("docs", "a").unwrap(), Some(json!({"n": 1})));
        assert!(fake.commits().is_empty());
        assert_eq!(fake.pull_after.lock().unwrap().as_slice(), &[0]);

        *fake.pull.lock().unwrap() = vec![SyncBatch {
            seq: 3,
            ops: vec![SyncOp::PutDoc {
                collection: "docs".into(),
                id: "gap".into(),
                doc: json!({"n": 3}),
            }],
        }];
        assert!(matches!(
            store.sync_pull().unwrap_err(),
            StoreError::Backend(_)
        ));
        assert_eq!(store.get_doc("docs", "gap").unwrap(), None);
        assert_eq!(store.get_doc("docs", "a").unwrap(), Some(json!({"n": 1})));
        assert_eq!(store.synced_seq(), 1);
    }

    #[test]
    fn detach_refuses_a_pending_batch_and_clears_mode_after_flush() {
        let (store, _fake) = attach(SyncMode::WriteBehind);
        store.put_doc("docs", "a", &json!({"n": 1})).unwrap();
        assert!(matches!(
            store.detach_sync_adapter().unwrap_err(),
            StoreError::Backend(_)
        ));
        assert_eq!(store.sync_mode(), Some(SyncMode::WriteBehind));
        assert_eq!(store.sync_flush().unwrap(), 1);
        store.detach_sync_adapter().unwrap();
        assert_eq!(store.sync_mode(), None);
        assert_eq!(store.synced_seq(), 1);
        assert!(matches!(
            store.sync_flush().unwrap_err(),
            StoreError::Backend(_)
        ));
    }

    #[test]
    fn write_through_disk_commit_survives_reopen_without_an_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.fls");
        let fake = Arc::new(FakeSync::new());
        {
            let store = FerroStore::open(&path).unwrap();
            store
                .attach_sync_adapter(
                    SyncMode::WriteThrough,
                    Arc::clone(&fake) as Arc<dyn SyncAdapter>,
                )
                .unwrap();
            store.put_doc("docs", "kept", &json!({"ok": true})).unwrap();
            assert_eq!(fake.commits().len(), 1);
            assert_eq!(store.synced_seq(), 1);
        }
        let store = FerroStore::open(&path).unwrap();
        assert_eq!(
            store.get_doc("docs", "kept").unwrap(),
            Some(json!({"ok": true}))
        );
        assert_eq!(store.sync_mode(), None);
        assert_eq!(store.synced_seq(), 0);
    }

    struct NameOnly;

    impl SyncAdapter for NameOnly {
        fn name(&self) -> &str {
            "name-only"
        }

        fn on_commit(&self, _batch: &SyncBatch) -> Result<(), StoreError> {
            Ok(())
        }
    }

    #[test]
    fn default_adapter_methods_pull_prewarm_and_miss() {
        let store = FerroStore::in_memory()
            .with_sync_adapter(
                SyncMode::LocalCache,
                Arc::new(NameOnly) as Arc<dyn SyncAdapter>,
            )
            .unwrap();
        assert_eq!(store.sync_pull().unwrap(), store.synced_seq());
        assert_eq!(store.sync_prewarm(&["docs"]).unwrap(), store.synced_seq());
        assert_eq!(store.synced_seq(), 0);
        assert_eq!(store.get_doc("docs", "missing").unwrap(), None);
    }

    #[test]
    fn failed_write_through_queues_the_next_commit_until_flush() {
        let (store, fake) = attach(SyncMode::WriteThrough);
        fake.fail.store(true, Ordering::SeqCst);
        let err = store.put_doc("docs", "a", &json!({"n": 1})).unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err}");
        assert_eq!(store.get_doc("docs", "a").unwrap(), Some(json!({"n": 1})));
        assert!(fake.commits().is_empty());
        assert_eq!(store.synced_seq(), 0);

        fake.fail.store(false, Ordering::SeqCst);
        store.put_doc("docs", "b", &json!({"n": 2})).unwrap();
        assert!(
            fake.commits().is_empty(),
            "a later commit is queued, not delivered"
        );
        assert_eq!(store.get_doc("docs", "b").unwrap(), Some(json!({"n": 2})));
        assert_eq!(store.synced_seq(), 0);

        assert_eq!(store.sync_flush().unwrap(), 2);
        assert_eq!(
            fake.commits()
                .iter()
                .map(|batch| batch.seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(store.synced_seq(), 2);

        store.put_doc("docs", "c", &json!({"n": 3})).unwrap();
        assert_eq!(fake.commits().len(), 3);
        assert_eq!(fake.commits()[2].seq, 3);
        assert_eq!(store.synced_seq(), 3);
    }

    struct HoldFirst {
        entered: Arc<(Mutex<bool>, Condvar)>,
        release: Arc<(Mutex<bool>, Condvar)>,
        commits: Mutex<Vec<u64>>,
    }

    impl SyncAdapter for HoldFirst {
        fn name(&self) -> &str {
            "hold-first"
        }

        fn on_commit(&self, batch: &SyncBatch) -> Result<(), StoreError> {
            if batch.seq == 1 {
                {
                    let (lock, cv) = &*self.entered;
                    *lock.lock().unwrap() = true;
                    cv.notify_all();
                }
                let (lock, cv) = &*self.release;
                let mut guard = lock.lock().unwrap();
                while !*guard {
                    guard = cv.wait(guard).unwrap();
                }
            }
            self.commits.lock().unwrap().push(batch.seq);
            Ok(())
        }
    }

    #[test]
    fn write_through_in_flight_queues_the_next_commit() {
        let entered = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let adapter = Arc::new(HoldFirst {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            commits: Mutex::new(Vec::new()),
        });
        let store = Arc::new(
            FerroStore::in_memory()
                .with_sync_adapter(
                    SyncMode::WriteThrough,
                    Arc::clone(&adapter) as Arc<dyn SyncAdapter>,
                )
                .unwrap(),
        );
        let worker = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.put_doc("docs", "a", &json!({"n": 1})).unwrap())
        };
        {
            let (lock, cv) = &*entered;
            let mut guard = lock.lock().unwrap();
            while !*guard {
                guard = cv.wait(guard).unwrap();
            }
        }
        store.put_doc("docs", "b", &json!({"n": 2})).unwrap();
        assert!(adapter.commits.lock().unwrap().is_empty());
        {
            let (lock, cv) = &*release;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        worker.join().unwrap();
        assert_eq!(adapter.commits.lock().unwrap().as_slice(), &[1]);
        assert_eq!(store.synced_seq(), 1);
        assert_eq!(store.sync_flush().unwrap(), 2);
        assert_eq!(adapter.commits.lock().unwrap().as_slice(), &[1, 2]);
        assert_eq!(store.synced_seq(), 2);
    }

    struct BlockingNotify {
        calls: Mutex<Vec<u64>>,
        entered: Arc<(Mutex<bool>, Condvar)>,
        release: Arc<(Mutex<bool>, Condvar)>,
        early: Arc<(Mutex<bool>, Condvar)>,
        fail_first: bool,
        saw_first: AtomicBool,
    }

    impl SyncAdapter for BlockingNotify {
        fn name(&self) -> &str {
            "blocking-notify"
        }

        fn on_commit(&self, batch: &SyncBatch) -> Result<(), StoreError> {
            let first = batch.seq == 1
                && self
                    .saw_first
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok();
            if !first {
                let (lock, _) = &*self.release;
                if !*lock.lock().unwrap() {
                    let (lock, cv) = &*self.early;
                    *lock.lock().unwrap() = true;
                    cv.notify_all();
                }
            }
            self.calls.lock().unwrap().push(batch.seq);
            if first {
                {
                    let (lock, cv) = &*self.entered;
                    *lock.lock().unwrap() = true;
                    cv.notify_all();
                }
                let (lock, cv) = &*self.release;
                let mut guard = lock.lock().unwrap();
                while !*guard {
                    guard = cv.wait(guard).unwrap();
                }
                if self.fail_first {
                    return Err(StoreError::Backend(
                        "sync adapter rejected the batch".into(),
                    ));
                }
            }
            Ok(())
        }
    }

    struct BlockingRig {
        adapter: Arc<BlockingNotify>,
        entered: Arc<(Mutex<bool>, Condvar)>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    fn blocking_adapter(fail_first: bool) -> BlockingRig {
        let entered = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let adapter = Arc::new(BlockingNotify {
            calls: Mutex::new(Vec::new()),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            early: Arc::new((Mutex::new(false), Condvar::new())),
            fail_first,
            saw_first: AtomicBool::new(false),
        });
        BlockingRig {
            adapter,
            entered,
            release,
        }
    }

    fn wait_flag(pair: &Arc<(Mutex<bool>, Condvar)>) {
        let (lock, cv) = &**pair;
        let mut guard = lock.lock().unwrap();
        while !*guard {
            guard = cv.wait(guard).unwrap();
        }
    }

    #[test]
    fn write_through_second_put_returns_while_the_first_notify_is_blocked() {
        let BlockingRig {
            adapter,
            entered,
            release,
        } = blocking_adapter(false);
        let store = Arc::new(
            FerroStore::in_memory()
                .with_sync_adapter(
                    SyncMode::WriteThrough,
                    Arc::clone(&adapter) as Arc<dyn SyncAdapter>,
                )
                .unwrap(),
        );
        let first = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.put_doc("docs", "a", &json!({"n": 1})).unwrap())
        };
        wait_flag(&entered);
        let second = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.put_doc("docs", "b", &json!({"n": 2})).unwrap())
        };
        second.join().unwrap();
        assert_eq!(adapter.calls.lock().unwrap().as_slice(), &[1]);
        {
            let (lock, cv) = &*release;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        first.join().unwrap();
        assert_eq!(adapter.calls.lock().unwrap().as_slice(), &[1]);
        assert_eq!(store.synced_seq(), 1);
        assert_eq!(store.sync_flush().unwrap(), 2);
        assert_eq!(adapter.calls.lock().unwrap().as_slice(), &[1, 2]);
        assert_eq!(store.synced_seq(), 2);
    }

    #[test]
    fn sync_flush_waits_out_a_blocked_notify_and_then_sends_the_failed_batch_first() {
        let BlockingRig {
            adapter,
            entered,
            release,
        } = blocking_adapter(true);
        let early = Arc::clone(&adapter.early);
        let store = Arc::new(
            FerroStore::in_memory()
                .with_sync_adapter(
                    SyncMode::WriteThrough,
                    Arc::clone(&adapter) as Arc<dyn SyncAdapter>,
                )
                .unwrap(),
        );
        let first = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.put_doc("docs", "a", &json!({"n": 1})))
        };
        wait_flag(&entered);
        let second = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.put_doc("docs", "b", &json!({"n": 2})).unwrap())
        };
        second.join().unwrap();
        assert_eq!(adapter.calls.lock().unwrap().as_slice(), &[1]);
        let flushed = {
            let store = Arc::clone(&store);
            thread::spawn(move || store.sync_flush())
        };
        {
            let (lock, cv) = &*early;
            let guard = lock.lock().unwrap();
            let (guard, status) = cv
                .wait_timeout(guard, std::time::Duration::from_millis(250))
                .unwrap();
            assert!(
                status.timed_out() && !*guard,
                "flush called on_commit while an older notify was still in flight"
            );
        }
        {
            let (lock, cv) = &*release;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        let err = first.join().unwrap().unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err}");
        assert_eq!(flushed.join().unwrap().unwrap(), 2);
        assert_eq!(adapter.calls.lock().unwrap().as_slice(), &[1, 1, 2]);
        assert_eq!(store.synced_seq(), 2);
        assert_eq!(store.get_doc("docs", "b").unwrap(), Some(json!({"n": 2})));
    }

    fn bad_second_batch() -> Vec<SyncBatch> {
        vec![
            SyncBatch {
                seq: 1,
                ops: vec![SyncOp::PutDoc {
                    collection: "docs".into(),
                    id: "a".into(),
                    doc: json!({"n": 1}),
                }],
            },
            SyncBatch {
                seq: 2,
                ops: vec![SyncOp::DeleteVector {
                    collection: "missing".into(),
                    id: "x".into(),
                }],
            },
        ]
    }

    #[test]
    fn multi_batch_pull_with_a_bad_later_op_writes_nothing() {
        let (store, fake) = attach(SyncMode::WriteBehind);
        *fake.pull.lock().unwrap() = bad_second_batch();
        assert!(matches!(
            store.sync_pull().unwrap_err(),
            StoreError::Backend(_)
        ));
        assert_eq!(store.get_doc("docs", "a").unwrap(), None);
        assert_eq!(store.synced_seq(), 0);
        assert_eq!(store.read_state().unwrap().last_seq, 0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pull.fls");
        let store = FerroStore::open(&path).unwrap();
        store
            .attach_sync_adapter(
                SyncMode::WriteBehind,
                Arc::clone(&fake) as Arc<dyn SyncAdapter>,
            )
            .unwrap();
        assert!(matches!(
            store.sync_pull().unwrap_err(),
            StoreError::Backend(_)
        ));
        assert_eq!(store.synced_seq(), 0);
        drop(store);
        let store = FerroStore::open(&path).unwrap();
        assert_eq!(store.get_doc("docs", "a").unwrap(), None);
        assert_eq!(store.read_state().unwrap().last_seq, 0);
    }

    #[test]
    fn silent_two_batch_disk_apply_publishes_one_superblock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("epoch.fls");
        let store = FerroStore::open(&path).unwrap();
        let before = {
            let file = std::fs::File::open(&path).unwrap();
            super::super::latest_superblock(&file).unwrap().1.epoch
        };
        let fake = Arc::new(FakeSync::new());
        store
            .attach_sync_adapter(
                SyncMode::WriteBehind,
                Arc::clone(&fake) as Arc<dyn SyncAdapter>,
            )
            .unwrap();
        *fake.pull.lock().unwrap() = vec![
            SyncBatch {
                seq: 1,
                ops: vec![SyncOp::PutDoc {
                    collection: "docs".into(),
                    id: "a".into(),
                    doc: json!({"n": 1}),
                }],
            },
            SyncBatch {
                seq: 2,
                ops: vec![SyncOp::PutDoc {
                    collection: "docs".into(),
                    id: "b".into(),
                    doc: json!({"n": 2}),
                }],
            },
        ];
        assert_eq!(store.sync_pull().unwrap(), 2);
        let after = {
            let file = std::fs::File::open(&path).unwrap();
            super::super::latest_superblock(&file).unwrap().1.epoch
        };
        assert_eq!(after, before + 1);
        drop(store);
        let store = FerroStore::open(&path).unwrap();
        assert_eq!(store.get_doc("docs", "a").unwrap(), Some(json!({"n": 1})));
        assert_eq!(store.get_doc("docs", "b").unwrap(), Some(json!({"n": 2})));
        assert_eq!(store.read_state().unwrap().last_seq, 2);
        assert!(fake.commits().is_empty());
    }

    #[test]
    fn with_sync_adapter_while_pending_keeps_the_original_adapter() {
        let original = Arc::new(FakeSync::new());
        let store = FerroStore::in_memory()
            .with_sync_adapter(
                SyncMode::WriteBehind,
                Arc::clone(&original) as Arc<dyn SyncAdapter>,
            )
            .unwrap();
        store.put_doc("docs", "a", &json!({"n": 1})).unwrap();
        let replacement = Arc::new(FakeSync::new());
        let err = store
            .attach_sync_adapter(
                SyncMode::WriteThrough,
                Arc::clone(&replacement) as Arc<dyn SyncAdapter>,
            )
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref message) if message == "cannot attach a sync adapter while batches are pending"),
            "{err}"
        );
        assert_eq!(store.sync_mode(), Some(SyncMode::WriteBehind));
        assert_eq!(store.synced_seq(), 0);
        assert_eq!(store.sync_flush().unwrap(), 1);
        assert_eq!(original.commits().len(), 1);
        assert_eq!(original.commits()[0].seq, 1);
        assert!(replacement.commits().is_empty());

        let original = Arc::new(FakeSync::new());
        let store = FerroStore::in_memory()
            .with_sync_adapter(
                SyncMode::WriteBehind,
                Arc::clone(&original) as Arc<dyn SyncAdapter>,
            )
            .unwrap();
        store.put_doc("docs", "b", &json!({"n": 2})).unwrap();
        let failed = store.with_sync_adapter(
            SyncMode::CloudPreCache,
            Arc::new(FakeSync::new()) as Arc<dyn SyncAdapter>,
        );
        let err = match failed {
            Ok(_) => panic!("attached a sync adapter while batches are pending"),
            Err(err) => err,
        };
        assert!(
            matches!(err, StoreError::Backend(ref message) if message == "cannot attach a sync adapter while batches are pending"),
            "{err}"
        );
    }

    struct FetchThenPut {
        put: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
        fetches: AtomicUsize,
    }

    impl SyncAdapter for FetchThenPut {
        fn name(&self) -> &str {
            "fetch-then-put"
        }

        fn on_commit(&self, _batch: &SyncBatch) -> Result<(), StoreError> {
            Ok(())
        }

        fn fetch_document(&self, _: &str, _: &str) -> Result<Option<Value>, StoreError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            if let Some(put) = self.put.lock().unwrap().as_ref() {
                put();
            }
            Ok(Some(json!({"stale": true})))
        }
    }

    fn assert_fill_keeps_the_callback_put(store: FerroStore) {
        let fetch = Arc::new(FetchThenPut {
            put: Mutex::new(None),
            fetches: AtomicUsize::new(0),
        });
        let store = store
            .with_sync_adapter(
                SyncMode::LocalCache,
                Arc::clone(&fetch) as Arc<dyn SyncAdapter>,
            )
            .unwrap();
        let addr = &store as *const FerroStore as usize;
        // `get_doc` calls `fetch_document` on this stack, before `store` moves.
        *fetch.put.lock().unwrap() = Some(Box::new(move || {
            let store = unsafe { &*(addr as *const FerroStore) };
            store.put_doc("docs", "a", &json!({"fresh": true})).unwrap();
        }));
        assert_eq!(
            store.get_doc("docs", "a").unwrap(),
            Some(json!({"fresh": true}))
        );
        assert_eq!(
            store.get_doc("docs", "a").unwrap(),
            Some(json!({"fresh": true}))
        );
        assert_eq!(fetch.fetches.load(Ordering::SeqCst), 1);
        store.put_doc("docs", "n", &Value::Null).unwrap();
        let before = fetch.fetches.load(Ordering::SeqCst);
        assert_eq!(store.get_doc("docs", "n").unwrap(), Some(Value::Null));
        assert_eq!(fetch.fetches.load(Ordering::SeqCst), before);
    }

    #[test]
    fn cache_fill_does_not_overwrite_a_document_inserted_during_fetch() {
        assert_fill_keeps_the_callback_put(FerroStore::in_memory());
        let dir = tempfile::tempdir().unwrap();
        assert_fill_keeps_the_callback_put(FerroStore::open(dir.path().join("fill.fls")).unwrap());
    }

    #[test]
    fn remote_put_doc_reports_the_commit_seq_not_a_later_read() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("seq.sock");
        let server = FerroStoreServer::bind_unix(&sock, FerroStore::in_memory()).unwrap();
        let uri = format!("unix://{}", sock.display());
        let client = FerroStore::connect_uri(&uri).unwrap();
        let fake = Arc::new(FakeSync::new());
        client
            .attach_sync_adapter(
                SyncMode::WriteThrough,
                Arc::clone(&fake) as Arc<dyn SyncAdapter>,
            )
            .unwrap();
        client.put_doc("docs", "a", &json!({"n": 1})).unwrap();
        assert_eq!(fake.commits()[0].seq, 1);
        assert_eq!(server.store().read_state().unwrap().last_seq, 1);

        let other = FerroStore::connect_uri(&uri).unwrap();
        other.put_doc("docs", "c", &json!({"n": 9})).unwrap();
        client.put_doc("docs", "b", &json!({"n": 2})).unwrap();
        assert_eq!(
            fake.commits()
                .iter()
                .map(|batch| batch.seq)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(server.store().read_state().unwrap().last_seq, 3);
        drop(other);
        drop(client);
        drop(server);
    }

    #[test]
    fn remote_silent_fill_pull_and_prewarm_skip_the_server_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("silent.sock");
        let server_fake = Arc::new(FakeSync::new());
        let server_store = FerroStore::in_memory();
        server_store
            .attach_sync_adapter(
                SyncMode::WriteThrough,
                Arc::clone(&server_fake) as Arc<dyn SyncAdapter>,
            )
            .unwrap();
        let server = FerroStoreServer::bind_unix(&sock, server_store).unwrap();
        let uri = format!("unix://{}", sock.display());
        let client = FerroStore::connect_uri(&uri).unwrap();
        let client_fake = Arc::new(FakeSync::new());
        client
            .attach_sync_adapter(
                SyncMode::WriteThrough,
                Arc::clone(&client_fake) as Arc<dyn SyncAdapter>,
            )
            .unwrap();
        client.put_doc("docs", "loud", &json!({"n": 1})).unwrap();
        assert_eq!(server_fake.commits().len(), 1);
        assert_eq!(server_fake.commits()[0].seq, 1);
        assert_eq!(client_fake.commits().len(), 1);
        client.detach_sync_adapter().unwrap();
        client
            .attach_sync_adapter(
                SyncMode::LocalCache,
                Arc::clone(&client_fake) as Arc<dyn SyncAdapter>,
            )
            .unwrap();

        *client_fake.fetch.lock().unwrap() = Some(json!({"cached": true}));
        let server_commits = server_fake.commits().len();
        assert_eq!(
            client.get_doc("docs", "cached").unwrap(),
            Some(json!({"cached": true}))
        );
        assert_eq!(server_fake.commits().len(), server_commits);
        assert_eq!(
            server.store().get_doc("docs", "cached").unwrap(),
            Some(json!({"cached": true}))
        );
        assert_eq!(client_fake.commits().len(), 1);

        let next = server.store().read_state().unwrap().last_seq + 1;
        *client_fake.pull.lock().unwrap() = vec![SyncBatch {
            seq: next,
            ops: vec![SyncOp::PutDoc {
                collection: "docs".into(),
                id: "pulled".into(),
                doc: json!({"from": "pull"}),
            }],
        }];
        assert_eq!(client.sync_pull().unwrap(), next);
        assert_eq!(server_fake.commits().len(), server_commits);
        assert_eq!(
            server.store().get_doc("docs", "pulled").unwrap(),
            Some(json!({"from": "pull"}))
        );
        assert_eq!(client.synced_seq(), next);

        let warm = next + 1;
        *client_fake.prewarm.lock().unwrap() = vec![SyncBatch {
            seq: warm,
            ops: vec![SyncOp::PutDoc {
                collection: "docs".into(),
                id: "warm".into(),
                doc: json!({"from": "prewarm"}),
            }],
        }];
        assert_eq!(client.sync_prewarm(&["docs"]).unwrap(), warm);
        assert_eq!(server_fake.commits().len(), server_commits);
        assert_eq!(
            server.store().get_doc("docs", "warm").unwrap(),
            Some(json!({"from": "prewarm"}))
        );
        assert_eq!(client.synced_seq(), warm);
        assert_eq!(client_fake.commits().len(), 1);
        drop(client);
        drop(server);
    }
}
