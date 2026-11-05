//! FerroMQ durability hints and adaptive tier policy.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// The caller's intended durability floor for a FerroMQ publish.
///
/// These types describe a policy only; this module does not enable memory-tier publishing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MqDurability {
    /// Publish through the existing durable commit path.
    #[default]
    Durable,
    /// Permit buffering with a later durable spill.
    Adaptive,
    /// Permit process-memory-only delivery.
    Ephemeral,
}

/// Configurable thresholds for deciding when buffered messages should spill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MqTierPolicy {
    pub max_memory_messages: usize,
    pub max_memory_bytes: u64,
    pub max_memory_lag: Duration,
    pub min_available_ram_bytes: u64,
    pub min_free_disk_bytes: u64,
}

impl Default for MqTierPolicy {
    fn default() -> Self {
        Self {
            max_memory_messages: 10_000,
            max_memory_bytes: 16 * 1024 * 1024,
            max_memory_lag: Duration::from_secs(30),
            min_available_ram_bytes: 256 * 1024 * 1024,
            min_free_disk_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// One deterministic snapshot supplied by a resource probe.
///
/// Unknown RAM means pressure; unknown disk capacity prevents spill. The counts should include
/// the candidate publish when deciding whether accepting it would cross a watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MqTierObservation {
    pub memory_messages: usize,
    pub memory_bytes: u64,
    pub oldest_unacked_lag: Duration,
    pub available_ram_bytes: Option<u64>,
    pub free_disk_bytes: Option<u64>,
}

/// Available resources measured on the host. Unknown values are never treated as ample capacity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MqResourceSnapshot {
    pub available_ram_bytes: Option<u64>,
    pub free_disk_bytes: Option<u64>,
}

/// Injectable host resource probe; `storage_path` identifies the filesystem used for spill.
pub trait MqResourceProbe: Send + Sync {
    fn probe(&self, storage_path: &Path) -> MqResourceSnapshot;
}

/// Reads Linux `MemAvailable` and Unix filesystem capacity. Unsupported or failed probes return
/// unknown rather than a fabricated capacity.
#[derive(Debug, Clone, Copy, Default)]
pub struct MqSystemResourceProbe;

impl MqResourceProbe for MqSystemResourceProbe {
    fn probe(&self, storage_path: &Path) -> MqResourceSnapshot {
        MqResourceSnapshot {
            available_ram_bytes: system_available_ram(),
            free_disk_bytes: system_free_disk(storage_path),
        }
    }
}

impl MqTierObservation {
    /// Combine queue metrics with a probe. Supplying the probe keeps policy tests deterministic.
    pub fn from_probe(
        probe: &dyn MqResourceProbe,
        storage_path: &Path,
        memory_messages: usize,
        memory_bytes: u64,
        oldest_unacked_lag: Duration,
    ) -> Self {
        let resources = probe.probe(storage_path);
        Self {
            memory_messages,
            memory_bytes,
            oldest_unacked_lag,
            available_ram_bytes: resources.available_ram_bytes,
            free_disk_bytes: resources.free_disk_bytes,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_mem_available(content: &str) -> Option<u64> {
    let mut available = None;
    for line in content.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() != Some("MemAvailable:") {
            continue;
        }
        if available.is_some() {
            return None;
        }
        let kib = fields.next()?.parse::<u64>().ok()?;
        if fields.next() != Some("kB") || fields.next().is_some() {
            return None;
        }
        available = Some(kib.checked_mul(1024)?);
    }
    available
}

#[cfg(target_os = "linux")]
fn system_available_ram() -> Option<u64> {
    parse_mem_available(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(not(target_os = "linux"))]
fn system_available_ram() -> Option<u64> {
    None
}

fn disk_free_bytes(blocks: u64, block_size: u64) -> Option<u64> {
    (block_size != 0).then_some(())?;
    blocks.checked_mul(block_size)
}

#[cfg(unix)]
#[allow(clippy::useless_conversion)] // libc field widths differ among Unix targets.
fn system_free_disk(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL terminated and `stats` points to writable storage. We only read
    // the initialized result when statvfs succeeds.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: successful statvfs initializes the complete output structure.
    let stats = unsafe { stats.assume_init() };
    disk_free_bytes(
        u64::try_from(stats.f_bavail).ok()?,
        u64::try_from(stats.f_frsize).ok()?,
    )
}

#[cfg(not(unix))]
fn system_free_disk(_: &Path) -> Option<u64> {
    None
}

/// The result of evaluating watermarks and disk headroom, without performing I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqTierDecision {
    Memory,
    Spill,
    SpillBlocked,
}

impl MqTierPolicy {
    /// Evaluate a snapshot. Limits on queue volume and lag trigger at the configured value;
    /// RAM at its minimum is acceptable and disk must exceed its minimum. Missing probe values
    /// are handled conservatively.
    pub fn evaluate(&self, observation: &MqTierObservation) -> MqTierDecision {
        let pressure = observation.memory_messages >= self.max_memory_messages
            || observation.memory_bytes >= self.max_memory_bytes
            || observation.oldest_unacked_lag >= self.max_memory_lag
            || observation
                .available_ram_bytes
                .is_none_or(|ram| ram < self.min_available_ram_bytes);
        if !pressure {
            return MqTierDecision::Memory;
        }
        if observation
            .free_disk_bytes
            .is_some_and(|disk| disk > self.min_free_disk_bytes)
        {
            MqTierDecision::Spill
        } else {
            MqTierDecision::SpillBlocked
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn policy() -> MqTierPolicy {
        MqTierPolicy {
            max_memory_messages: 3,
            max_memory_bytes: 100,
            max_memory_lag: Duration::from_secs(10),
            min_available_ram_bytes: 50,
            min_free_disk_bytes: 200,
        }
    }

    fn observation() -> MqTierObservation {
        MqTierObservation {
            memory_messages: 2,
            memory_bytes: 99,
            oldest_unacked_lag: Duration::from_secs(9),
            available_ram_bytes: Some(50),
            free_disk_bytes: Some(201),
        }
    }

    #[test]
    fn durable_is_the_default_and_wire_names_round_trip() {
        assert_eq!(MqDurability::default(), MqDurability::Durable);
        for (variant, wire) in [
            (MqDurability::Durable, "\"durable\""),
            (MqDurability::Adaptive, "\"adaptive\""),
            (MqDurability::Ephemeral, "\"ephemeral\""),
        ] {
            assert_eq!(serde_json::to_string(&variant).unwrap(), wire);
            assert_eq!(serde_json::from_str::<MqDurability>(wire).unwrap(), variant);
        }
    }

    #[test]
    fn policy_detects_each_watermark_at_the_boundary() {
        let policy = policy();
        let base = observation();
        assert_eq!(policy.evaluate(&base), MqTierDecision::Memory);
        let mut obs = base;
        obs.memory_messages = 3;
        assert_eq!(policy.evaluate(&obs), MqTierDecision::Spill);
        obs = base;
        obs.memory_bytes = 100;
        assert_eq!(policy.evaluate(&obs), MqTierDecision::Spill);
        obs = base;
        obs.oldest_unacked_lag = Duration::from_secs(10);
        assert_eq!(policy.evaluate(&obs), MqTierDecision::Spill);
        obs = base;
        obs.available_ram_bytes = Some(49);
        assert_eq!(policy.evaluate(&obs), MqTierDecision::Spill);
    }

    #[test]
    fn unknown_pressure_and_disk_headroom_are_conservative() {
        let policy = policy();
        let mut obs = observation();
        obs.available_ram_bytes = None;
        assert_eq!(policy.evaluate(&obs), MqTierDecision::Spill);
        obs.free_disk_bytes = Some(200);
        assert_eq!(policy.evaluate(&obs), MqTierDecision::SpillBlocked);
        obs.free_disk_bytes = Some(199);
        assert_eq!(policy.evaluate(&obs), MqTierDecision::SpillBlocked);
        obs.free_disk_bytes = None;
        assert_eq!(policy.evaluate(&obs), MqTierDecision::SpillBlocked);
        obs.available_ram_bytes = Some(50);
        assert_eq!(policy.evaluate(&obs), MqTierDecision::Memory);
    }

    #[test]
    fn default_policy_has_nonzero_watermarks() {
        let policy = MqTierPolicy::default();
        assert!(policy.max_memory_messages > 0);
        assert!(policy.max_memory_bytes > 0);
        assert!(!policy.max_memory_lag.is_zero());
        assert!(policy.min_available_ram_bytes > 0);
        assert!(policy.min_free_disk_bytes > 0);
        assert!(serde_json::from_str::<MqDurability>("\"unknown\"").is_err());
    }

    #[test]
    fn memavailable_parser_uses_kibibytes_and_refuses_bad_samples() {
        assert_eq!(
            parse_mem_available("MemTotal: 9999 kB\nMemAvailable: 123 kB\n"),
            Some(123 * 1024)
        );
        for bad in [
            "MemTotal: 99 kB\n",
            "MemAvailable: -1 kB",
            "MemAvailable: 5 MB",
            "MemAvailable: 5",
            "MemAvailable: 5 kB extra",
            "MemAvailable: 18014398509481984 kB",
            "MemAvailable: 1 kB\nMemAvailable: 2 kB\n",
        ] {
            assert_eq!(parse_mem_available(bad), None, "{bad}");
        }
    }

    #[test]
    fn disk_free_math_rejects_overflow_and_invalid_block_size() {
        assert_eq!(disk_free_bytes(3, 4096), Some(12_288));
        assert_eq!(disk_free_bytes(u64::MAX, 2), None);
        assert_eq!(disk_free_bytes(10, 0), None);
        assert_eq!(disk_free_bytes(0, 4096), Some(0));
    }

    #[test]
    fn nonexistent_disk_path_is_unknown_and_blocks_a_required_spill() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist");
        let sample = MqSystemResourceProbe.probe(&path);
        assert_eq!(sample.free_disk_bytes, None);
        let obs =
            MqTierObservation::from_probe(&MqSystemResourceProbe, &path, 3, 0, Duration::ZERO);
        assert_eq!(policy().evaluate(&obs), MqTierDecision::SpillBlocked);
    }

    #[cfg(unix)]
    #[test]
    fn existing_directory_has_a_filesystem_capacity_sample() {
        let dir = tempfile::tempdir().unwrap();
        let sample = MqSystemResourceProbe.probe(dir.path());
        assert!(sample.free_disk_bytes.is_some());
    }

    #[test]
    fn injected_probe_drives_policy_without_reading_host_state() {
        struct Fixed(MqResourceSnapshot);
        impl MqResourceProbe for Fixed {
            fn probe(&self, _: &std::path::Path) -> MqResourceSnapshot {
                self.0
            }
        }
        let probe = Fixed(MqResourceSnapshot {
            available_ram_bytes: Some(49),
            free_disk_bytes: Some(201),
        });
        let obs = MqTierObservation::from_probe(
            &probe,
            std::path::Path::new("/unused"),
            0,
            0,
            Duration::ZERO,
        );
        assert_eq!(policy().evaluate(&obs), MqTierDecision::Spill);
    }
}
