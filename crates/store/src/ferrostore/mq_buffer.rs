//! Isolated, test-only FerroMQ buffer model. No store operations use this module.

use super::MqDurability;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
enum MqBufferError {
    #[error("topic already exists")]
    AlreadySeeded,
    #[error("sequence overflow")]
    SequenceOverflow,
    #[error("sequence must increase")]
    SequenceNotIncreasing,
    #[error("durable messages must be committed through the durable store")]
    DurableRequiresStore,
    #[error("serialized message size overflow")]
    SizeOverflow,
    #[error("snapshot no longer matches the topic")]
    StaleSnapshot,
}

#[derive(Debug, Clone, PartialEq)]
struct BufferedMessage {
    seq: u64,
    key: String,
    payload: Value,
    durability: MqDurability,
    enqueued_at: Duration,
    bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct BufferStats {
    memory_messages: usize,
    memory_bytes: u64,
    acked_in_memory_total: u64,
}

#[derive(Debug, Default)]
struct Topic {
    high_water: u64,
    persisted_high_water: u64,
    revision: u128,
    messages: BTreeMap<u64, BufferedMessage>,
    acks: BTreeMap<String, BTreeSet<u64>>,
    pending_acks: BTreeMap<String, BTreeSet<u64>>,
    memory_bytes: u64,
    acked_in_memory_total: u64,
}

/// A snapshot is not a commit: the caller must persist it successfully before confirming it.
/// Confirming a stale snapshot fails rather than discarding messages or acknowledgements.
#[derive(Debug, Clone, PartialEq)]
struct FlushSnapshot {
    topic: String,
    high_water: u64,
    revision: u128,
    messages: Vec<BufferedMessage>,
    acks: BTreeMap<String, BTreeSet<u64>>,
}

#[derive(Debug, Default)]
struct MqBuffer {
    topics: BTreeMap<String, Topic>,
    evict_on_first_ack: bool,
}

impl MqBuffer {
    fn new(evict_on_first_ack: bool) -> Self {
        Self {
            topics: BTreeMap::new(),
            evict_on_first_ack,
        }
    }

    fn seed(&mut self, topic: &str, high_water: u64) -> Result<(), MqBufferError> {
        if self.topics.contains_key(topic) {
            return Err(MqBufferError::AlreadySeeded);
        }
        self.topics.insert(
            topic.to_owned(),
            Topic {
                high_water,
                persisted_high_water: high_water,
                ..Topic::default()
            },
        );
        Ok(())
    }

    /// Record a sequence the durable store has already committed. This method does no I/O.
    fn observe_durable(&mut self, topic: &str, seq: u64) -> Result<(), MqBufferError> {
        let state = self.topics.entry(topic.to_owned()).or_default();
        if seq <= state.high_water {
            return Err(MqBufferError::SequenceNotIncreasing);
        }
        state.high_water = seq;
        state.persisted_high_water = seq;
        state.revision += 1;
        Ok(())
    }

    /// Represent an already-persisted group ACK, including one recorded before publication.
    fn observe_persisted_ack(&mut self, topic: &str, group: &str, seq: u64) {
        let state = self.topics.entry(topic.to_owned()).or_default();
        if state.acks.entry(group.to_owned()).or_default().insert(seq) {
            state.revision += 1;
        }
    }

    fn push(
        &mut self,
        topic: &str,
        key: &str,
        payload: Value,
        durability: MqDurability,
        enqueued_at: Duration,
    ) -> Result<u64, MqBufferError> {
        if durability == MqDurability::Durable {
            return Err(MqBufferError::DurableRequiresStore);
        }
        let bytes = u64::try_from(key.len())
            .ok()
            .and_then(|n| {
                serde_json::to_vec(&payload)
                    .ok()
                    .and_then(|v| u64::try_from(v.len()).ok())
                    .and_then(|n2| n.checked_add(n2))
            })
            .ok_or(MqBufferError::SizeOverflow)?;
        let state = self.topics.entry(topic.to_owned()).or_default();
        let seq = state
            .high_water
            .checked_add(1)
            .ok_or(MqBufferError::SequenceOverflow)?;
        let total = state
            .memory_bytes
            .checked_add(bytes)
            .ok_or(MqBufferError::SizeOverflow)?;
        state.messages.insert(
            seq,
            BufferedMessage {
                seq,
                key: key.to_owned(),
                payload,
                durability,
                enqueued_at,
                bytes,
            },
        );
        state.high_water = seq;
        state.memory_bytes = total;
        state.revision += 1;
        Ok(seq)
    }

    fn ack(&mut self, topic: &str, group: &str, seq: u64) -> bool {
        let Some(state) = self.topics.get_mut(topic) else {
            return false;
        };
        let Some(message) = state.messages.get(&seq) else {
            return false;
        };
        let durability = message.durability;
        if !state.acks.entry(group.to_owned()).or_default().insert(seq) {
            return false;
        }
        state
            .pending_acks
            .entry(group.to_owned())
            .or_default()
            .insert(seq);
        state.acked_in_memory_total = state.acked_in_memory_total.saturating_add(1);
        if self.evict_on_first_ack || durability == MqDurability::Ephemeral {
            let removed = state.messages.remove(&seq).expect("message exists");
            state.memory_bytes -= removed.bytes;
            if durability == MqDurability::Ephemeral {
                for acked in state.acks.values_mut() {
                    acked.remove(&seq);
                }
            }
        }
        state.revision += 1;
        true
    }

    fn poll(&self, topic: &str, group: &str, limit: usize) -> Vec<BufferedMessage> {
        let Some(state) = self.topics.get(topic) else {
            return Vec::new();
        };
        let acked = state.acks.get(group);
        state
            .messages
            .values()
            .filter(|m| !acked.is_some_and(|a| a.contains(&m.seq)))
            .take(limit)
            .cloned()
            .collect()
    }

    fn stats(&self, topic: &str) -> Option<BufferStats> {
        let state = self.topics.get(topic)?;
        Some(BufferStats {
            memory_messages: state.messages.len(),
            memory_bytes: state.memory_bytes,
            acked_in_memory_total: state.acked_in_memory_total,
        })
    }

    fn oldest_unacked_lag(&self, topic: &str, group: &str, now: Duration) -> Duration {
        let Some(state) = self.topics.get(topic) else {
            return Duration::ZERO;
        };
        let acked = state.acks.get(group);
        state
            .messages
            .values()
            .filter(|m| !acked.is_some_and(|a| a.contains(&m.seq)))
            .map(|m| now.saturating_sub(m.enqueued_at))
            .max()
            .unwrap_or_default()
    }

    fn snapshot(&self, topic: &str) -> Option<FlushSnapshot> {
        let state = self.topics.get(topic)?;
        Some(FlushSnapshot {
            topic: topic.to_owned(),
            high_water: state.high_water,
            revision: state.revision,
            messages: state
                .messages
                .values()
                .filter(|m| m.durability == MqDurability::Adaptive)
                .cloned()
                .collect(),
            acks: state.pending_acks.clone(),
        })
    }

    fn confirm_flush(&mut self, snapshot: &FlushSnapshot) -> Result<(), MqBufferError> {
        let state = self
            .topics
            .get_mut(&snapshot.topic)
            .ok_or(MqBufferError::StaleSnapshot)?;
        if state.revision != snapshot.revision {
            return Err(MqBufferError::StaleSnapshot);
        }
        for message in &snapshot.messages {
            let removed = state
                .messages
                .remove(&message.seq)
                .expect("unchanged snapshot");
            state.memory_bytes -= removed.bytes;
        }
        for (group, seqs) in &snapshot.acks {
            if let Some(acked) = state.acks.get_mut(group) {
                for seq in seqs {
                    acked.remove(seq);
                }
            }
        }
        state.acks.retain(|_, seqs| !seqs.is_empty());
        state.pending_acks.clear();
        state.persisted_high_water = snapshot.high_water;
        state.revision += 1;
        Ok(())
    }

    fn persisted_high_water(&self, topic: &str) -> Option<u64> {
        self.topics
            .get(topic)
            .map(|state| state.persisted_high_water)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn seeded_and_supplied_sequences_are_monotonic_across_durability_modes() {
        let mut q = MqBuffer::new(false);
        q.seed("events", 40).unwrap();
        assert_eq!(
            q.push(
                "events",
                "a",
                json!(1),
                MqDurability::Adaptive,
                Duration::ZERO
            )
            .unwrap(),
            41
        );
        assert_eq!(
            q.push(
                "events",
                "b",
                json!(2),
                MqDurability::Ephemeral,
                Duration::ZERO
            )
            .unwrap(),
            42
        );
        q.observe_durable("events", 43).unwrap();
        assert!(q.observe_durable("events", 42).is_err());
        assert_eq!(
            q.push(
                "events",
                "c",
                json!(3),
                MqDurability::Adaptive,
                Duration::ZERO
            )
            .unwrap(),
            44
        );
        assert!(q.seed("events", 1).is_err());
        q.seed("exhausted", u64::MAX).unwrap();
        assert_eq!(
            q.push(
                "exhausted",
                "x",
                json!(1),
                MqDurability::Adaptive,
                Duration::ZERO
            ),
            Err(MqBufferError::SequenceOverflow)
        );
        assert!(
            q.push(
                "events",
                "x",
                json!(1),
                MqDurability::Durable,
                Duration::ZERO
            )
            .is_err()
        );
    }

    #[test]
    fn groups_ack_independently_and_default_does_not_evict() {
        let mut q = MqBuffer::new(false);
        let seq = q
            .push(
                "events",
                "first",
                json!({"v":1}),
                MqDurability::Adaptive,
                Duration::ZERO,
            )
            .unwrap();
        assert!(q.ack("events", "group-a", seq));
        assert!(!q.ack("events", "group-a", seq));
        assert!(q.poll("events", "group-a", 10).is_empty());
        assert_eq!(q.poll("events", "group-b", 10)[0].seq, seq);
        assert!(q.ack("events", "group-b", seq));
        assert_eq!(q.poll("events", "later-group", 10)[0].seq, seq);
        assert_eq!(q.stats("events").unwrap().acked_in_memory_total, 2);
    }

    #[test]
    fn an_earlier_persisted_unknown_ack_filters_a_later_memory_sequence() {
        let mut q = MqBuffer::new(false);
        q.observe_persisted_ack("events", "group-a", 1);
        assert_eq!(
            q.push(
                "events",
                "first",
                json!(1),
                MqDurability::Adaptive,
                Duration::ZERO
            )
            .unwrap(),
            1
        );
        assert!(q.poll("events", "group-a", 10).is_empty());
        assert_eq!(q.poll("events", "group-b", 10).len(), 1);
    }

    #[test]
    fn first_ack_eviction_is_explicit_and_ephemeral_records_can_be_evicted() {
        let mut q = MqBuffer::new(true);
        let adaptive = q
            .push(
                "events",
                "a",
                json!("one"),
                MqDurability::Adaptive,
                Duration::ZERO,
            )
            .unwrap();
        let ephemeral = q
            .push(
                "events",
                "b",
                json!("two"),
                MqDurability::Ephemeral,
                Duration::ZERO,
            )
            .unwrap();
        assert!(q.ack("events", "g", adaptive));
        assert!(q.ack("events", "g", ephemeral));
        assert!(q.poll("events", "other", 10).is_empty());
        let snapshot = q.snapshot("events").unwrap();
        assert_eq!(snapshot.high_water, ephemeral);
        assert!(snapshot.messages.is_empty());
        assert_eq!(
            snapshot.acks["g"].iter().copied().collect::<Vec<_>>(),
            vec![adaptive, ephemeral]
        );
    }

    #[test]
    fn snapshot_and_confirm_retain_high_water_and_group_acks() {
        let mut q = MqBuffer::new(false);
        let a = q
            .push(
                "events",
                "a",
                json!("one"),
                MqDurability::Adaptive,
                Duration::ZERO,
            )
            .unwrap();
        let e = q
            .push(
                "events",
                "e",
                json!("two"),
                MqDurability::Ephemeral,
                Duration::ZERO,
            )
            .unwrap();
        assert!(q.ack("events", "alpha", a));
        let snapshot = q.snapshot("events").unwrap();
        assert_eq!(snapshot.high_water, e);
        assert_eq!(
            snapshot.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
            vec![a]
        );
        assert!(snapshot.acks["alpha"].contains(&a));
        q.confirm_flush(&snapshot).unwrap();
        assert_eq!(q.persisted_high_water("events"), Some(e));
        assert_eq!(
            q.poll("events", "other", 10)
                .iter()
                .map(|m| m.seq)
                .collect::<Vec<_>>(),
            vec![e]
        );
        assert!(q.snapshot("events").unwrap().acks.is_empty());
    }

    #[test]
    fn mutation_during_flush_refuses_stale_snapshot_without_dropping_records() {
        let mut q = MqBuffer::new(false);
        let a = q
            .push(
                "events",
                "a",
                json!(1),
                MqDurability::Adaptive,
                Duration::ZERO,
            )
            .unwrap();
        let stale = q.snapshot("events").unwrap();
        assert!(q.ack("events", "g", a));
        assert_eq!(q.confirm_flush(&stale), Err(MqBufferError::StaleSnapshot));
        assert_eq!(q.poll("events", "other", 10)[0].seq, a);
        let latest = q.snapshot("events").unwrap();
        assert!(latest.acks["g"].contains(&a));
        q.confirm_flush(&latest).unwrap();
    }

    #[test]
    fn bytes_and_group_lag_are_deterministic_and_clock_safe() {
        let mut q = MqBuffer::new(false);
        let a = q
            .push(
                "events",
                "x",
                json!("abc"),
                MqDurability::Adaptive,
                Duration::from_secs(5),
            )
            .unwrap();
        q.push(
            "events",
            "yz",
            json!(10),
            MqDurability::Ephemeral,
            Duration::from_secs(7),
        )
        .unwrap();
        assert_eq!(
            q.stats("events").unwrap().memory_bytes,
            (1 + 5 + 2 + 2) as u64
        );
        assert_eq!(
            q.oldest_unacked_lag("events", "g", Duration::from_secs(4)),
            Duration::ZERO
        );
        assert_eq!(
            q.oldest_unacked_lag("events", "g", Duration::from_secs(10)),
            Duration::from_secs(5)
        );
        assert!(q.ack("events", "g", a));
        assert_eq!(
            q.oldest_unacked_lag("events", "g", Duration::from_secs(10)),
            Duration::from_secs(3)
        );
    }
}
