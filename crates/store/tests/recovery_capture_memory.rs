//! Isolated requested-heap measurement: no production allocator or live inputs.
use fl_store::ferrostore::{
    ColumnDef, ColumnType, ColumnValue, FerroStore, VectorMetric, VectorStorage,
};
use fl_store::recovery::{CancelToken, CaptureBudget};
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct Meter;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn added(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::SeqCst) + bytes;
    PEAK.fetch_max(live, Ordering::SeqCst);
}
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            added(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            added(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        if !ptr.is_null() {
            // Charge both sides briefly: realloc may allocate before freeing.
            added(size);
            LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        }
        ptr
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;

#[test]
fn mixed_eighty_mib_capture_fits_one_gib() {
    let Ok(shape) = std::env::var("FL_CAPTURE_MEMORY_CHILD") else {
        // Both fixed-size fixtures are ~80MiB: one stresses vector payloads,
        // the other graph node count (the old N² reservation exceeds 16GiB).
        for shape in ["wide", "many"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mixed_eighty_mib_capture_fits_one_gib",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("FL_CAPTURE_MEMORY_CHILD", shape)
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(180);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "isolated {shape} measurement failed");
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("isolated measurement exceeded 180 seconds");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        return;
    };
    let (vectors, dimensions) = match shape.as_str() {
        "wide" => (128, 16384),
        "many" => (1024, 2048),
        _ => panic!("unknown fixture shape"),
    };
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mixed.fls");
    let store = FerroStore::open(&path).unwrap();
    store
        .transaction(|txn| {
            for n in 0..1024 {
                txn.put_doc(
                    "documents",
                    &n.to_string(),
                    &json!({
                        "body": "d".repeat(40 * 1024), "ordinal": n,
                        "labels": ["fixture", "capture", "mixed"],
                        "nested": {"enabled": true, "score": n as f64 / 7.0}
                    }),
                )?;
            }
            Ok(())
        })
        .unwrap();
    store
        .create_column_table(
            "rows",
            &[
                ColumnDef {
                    name: "index".into(),
                    ty: ColumnType::Int64,
                },
                ColumnDef {
                    name: "payload".into(),
                    ty: ColumnType::Bytes,
                },
            ],
        )
        .unwrap();
    for group in 0..32 {
        let rows: Vec<_> = (0..128)
            .map(|row| {
                vec![
                    ColumnValue::Int64(group * 128 + row),
                    ColumnValue::Bytes(
                        (0..8192)
                            .map(|byte| ((byte * 17 + row * 31 + group) % 251) as u8)
                            .collect(),
                    ),
                ]
            })
            .collect();
        store.append_column_rows("rows", &rows).unwrap();
    }
    store
        .create_vector_collection(
            "vectors",
            dimensions,
            VectorStorage::F32,
            VectorMetric::L2,
            4,
            8,
            8,
        )
        .unwrap();
    for n in 0..vectors {
        let values: Vec<_> = (0..dimensions)
            .map(|x| ((x + n) % 257) as f32 / 257.0)
            .collect();
        store
            .upsert_vector(
                "vectors",
                &format!("v{n:04}"),
                &values,
                Some(&json!({"ordinal": n, "labels": ["vector", "fixture"]})),
            )
            .unwrap();
    }
    store.compact().unwrap();
    for n in 0..8 {
        store
            .upsert_vector(
                "vectors",
                &format!("v{n:04}"),
                &vec![n as f32; dimensions as usize],
                Some(&json!({"wal": true, "ordinal": n})),
            )
            .unwrap();
    }
    let file_bytes = std::fs::metadata(&path).unwrap().len();
    assert!(
        (80 << 20..84 << 20).contains(&file_bytes),
        "fixture bytes: {file_bytes}"
    );
    // One doc transaction, two collection creates, 32 appends, vector upserts.
    let sequence = 1 + 2 + 32 + u64::from(vectors) + 8;
    let budget = CaptureBudget::new(
        Instant::now() + Duration::from_secs(120),
        1 << 30,
        1 << 30,
        Duration::from_secs(1),
    );
    let cancel = CancelToken::new();
    // Baseline includes fixture construction and the owner's existing cache.
    // One child, one test thread, no concurrent writers. Only capture allocations
    // contribute to peak minus baseline; keep the immutable pin alive at reading.
    let baseline = LIVE.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    let capture = store.capture_recovery(&budget, &cancel).unwrap();
    let peak = PEAK.load(Ordering::SeqCst) - baseline;
    assert_eq!(capture.sequence(), sequence);
    let charged = capture.preparation_bytes();
    assert!(
        charged >= peak as u64,
        "charged {charged} < measured peak {peak}"
    );
    assert!(charged <= 1 << 30);
    println!(
        "capture_memory shape={shape} vectors={vectors} dimensions={dimensions} M=4 wal_upserts=8 file_bytes={file_bytes} baseline={baseline} peak_extra={peak} charged={charged} model_safety_factor=2"
    );
}
