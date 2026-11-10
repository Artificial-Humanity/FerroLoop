//! Public recovery API exercised on isolated physical fixtures.
use fl_store::ferrostore::{ExternalPublishStatus, FerroStore};
use fl_store::recovery::{CancelToken, CaptureBudget, OperationLimits};
use serde_json::json;
use std::time::{Duration, Instant};
fn budget() -> CaptureBudget {
    CaptureBudget::new(
        Instant::now() + Duration::from_secs(10),
        64 << 20,
        128 << 20,
        Duration::from_secs(1),
    )
}
#[test]
fn capture_image_reopens_at_one_sequence_after_external_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source.fls");
    let store = FerroStore::open(&source).unwrap();
    store.put_doc("unknown", "x", &json!(1)).unwrap();
    let captured = store
        .capture_recovery(&budget(), &CancelToken::new())
        .unwrap();
    let other = FerroStore::open(&source).unwrap();
    other.put_doc("unknown", "x", &json!(2)).unwrap();
    other.compact().unwrap();
    let path = tmp.path().join("image.fls");
    let mut file = std::fs::File::create(&path).unwrap();
    let limits = OperationLimits::new(
        Instant::now() + Duration::from_secs(10),
        64 << 20,
        128 << 20,
        CancelToken::new(),
    );
    let descriptor = captured.write_image(&mut file, &limits).unwrap();
    assert_eq!(descriptor.sequence, 1);
    assert_eq!(
        descriptor.external_publish_status,
        ExternalPublishStatus::ExclusionUnprovenBeforeSingleOwnership
    );
    assert_eq!(descriptor.bytes, file.metadata().unwrap().len());
    drop(file);
    let reopened = FerroStore::open(&path).unwrap();
    assert_eq!(reopened.get_doc("unknown", "x").unwrap(), Some(json!(1)));
    assert_eq!(store.get_doc("unknown", "x").unwrap(), Some(json!(2)));
}

#[test]
fn external_writer_child() {
    let Ok(path) = std::env::var("RECOVERY_CAPTURE_FIXTURE") else {
        return;
    };
    let count: usize = std::env::var("RECOVERY_CAPTURE_COUNT")
        .unwrap()
        .parse()
        .unwrap();
    let store = FerroStore::open(&path).unwrap();
    let mut latencies = Vec::new();
    for n in 0..count {
        let start = Instant::now();
        store.put_doc("external", "counter", &json!(n)).unwrap();
        latencies.push(start.elapsed().as_nanos() as u64);
        if n == 0 {
            std::fs::write(format!("{path}.ready"), b"ready").unwrap();
        }
        if count > 1 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    std::fs::write(
        format!("{path}.timing"),
        serde_json::to_vec(&latencies).unwrap(),
    )
    .unwrap();
}

struct WriterChild(std::process::Child);
impl WriterChild {
    fn start(path: &std::path::Path, count: usize) -> Self {
        Self(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "external_writer_child", "--nocapture"])
                .env_clear()
                .env("RECOVERY_CAPTURE_FIXTURE", path)
                .env("RECOVERY_CAPTURE_COUNT", count.to_string())
                .env("TMPDIR", path.parent().unwrap())
                .current_dir(path.parent().unwrap())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "external fixture writer timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for WriterChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn quantiles(mut samples: Vec<u64>) -> [u64; 4] {
    assert!(!samples.is_empty());
    samples.sort_unstable();
    let n = samples.len();
    [
        samples[(n - 1) * 50 / 100],
        samples[(n - 1) * 95 / 100],
        samples[(n - 1) * 99 / 100],
        samples[n - 1],
    ]
}

/// Real disk-backed fixtures complement fake-clock unit tests. These measurements
/// qualify only this build, host and fixture; they do not activate recurrence.
/// Every mode independently decodes; "warm" does not imply live-cache reuse,
/// and "cold_refresh" means an external commit invalidated the owner's state,
/// not that the operating system's page cache was flushed.
#[test]
fn fixture_writer_delay_quantiles() {
    let tmp = tempfile::tempdir().unwrap();
    for mode in ["warm_source_handle", "cold_refresh", "external_contention"] {
        let path = tmp.path().join(format!("{mode}.fls"));
        let owner = FerroStore::open(&path).unwrap();
        for n in 0..32 {
            owner
                .put_doc("unknown", &n.to_string(), &json!("x".repeat(128)))
                .unwrap();
        }
        owner.checkpoint().unwrap();
        let mut child = if mode == "external_contention" {
            Some(WriterChild::start(&path, 80))
        } else {
            None
        };
        if child.is_some() {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !std::path::Path::new(&format!("{}.ready", path.display())).exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let mut barrier = Vec::new();
        let mut writer = Vec::new();
        let mut retained = Vec::new();
        let mut refused = 0;
        for n in 0..40 {
            if mode == "cold_refresh" {
                let mut once = WriterChild::start(&path, 1);
                once.finish();
            }
            match owner.capture_recovery(&budget(), &CancelToken::new()) {
                Ok(captured) => {
                    barrier.push(captured.writer_delay().as_nanos() as u64);
                    retained.push(captured.retained_bytes());
                    let start = Instant::now();
                    owner.put_doc("unknown", "after", &json!(n)).unwrap();
                    writer.push(start.elapsed().as_nanos() as u64);
                    assert!(captured.sequence() < owner.stats().unwrap().last_seq);
                }
                Err(fl_store::recovery::RecoveryError::Unsupported { .. })
                    if mode == "external_contention" =>
                {
                    refused += 1
                }
                Err(err) => panic!("{mode}: {err}"),
            }
        }
        if let Some(child) = &mut child {
            child.finish();
            let times: Vec<u64> = serde_json::from_slice(
                &std::fs::read(format!("{}.timing", path.display())).unwrap(),
            )
            .unwrap();
            writer.extend(times);
        }
        let barrier = quantiles(barrier);
        let writer = quantiles(writer);
        let retained = quantiles(retained);
        println!(
            "capture_fixture mode={mode} samples=40 refusals={refused} barrier_ns={barrier:?} writer_total_ns={writer:?} retained_bytes={retained:?}"
        );
        assert!(barrier[3] <= 1_000_000_000, "{mode}: barrier exceeded 1s");
        assert!(
            writer[3] <= 1_000_000_000,
            "{mode}: writer delay proof exceeded 1s"
        );
    }
}

#[test]
fn remote_handle_refuses_physical_capture() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("source.fls");
    let socket = tmp.path().join("store.sock");
    let server = fl_store::ferrostore::FerroStoreServer::bind_unix(
        &socket,
        FerroStore::open(&path).unwrap(),
    )
    .unwrap();
    let remote = FerroStore::connect_uri(&format!("unix://{}", socket.display())).unwrap();
    assert!(matches!(
        remote.capture_recovery(&budget(), &CancelToken::new()),
        Err(fl_store::recovery::RecoveryError::Unsupported { .. })
    ));
    drop(remote);
    drop(server);
}
