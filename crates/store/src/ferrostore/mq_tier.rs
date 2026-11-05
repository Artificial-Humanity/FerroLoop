//! FerroMQ durability hints and adaptive tier policy.

use serde::{Deserialize, Serialize};
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

/// One deterministic snapshot supplied by a future host-probe implementation.
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
}
