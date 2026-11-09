//! Bounded completed-PTY diagnostic history. Inventory remains in HostInner outputs.
use crate::slice_budget::{ChargeKind, Reservation, SliceBudget, reserve};
use fl_host_proto::{ExtensionRefusal, StreamId};
use std::io::Write;
use std::sync::Arc;

pub(crate) const COMPLETED_PTY_LIMIT: usize = 8;
pub(crate) const RECENT_EVICTIONS: usize = 64;

pub(crate) struct Retention {
    recent: [Option<StreamId>; RECENT_EVICTIONS],
    next: usize,
    pub(crate) evicted_total: u64,
    _metadata: Reservation,
}
impl Retention {
    pub(crate) fn new(budget: &Arc<SliceBudget>) -> Result<Self, ExtensionRefusal> {
        let metadata = reserve(
            budget,
            ChargeKind::Metadata,
            std::mem::size_of::<Self>() + RECENT_EVICTIONS * 64,
        )?;
        Ok(Self {
            recent: std::array::from_fn(|_| None),
            next: 0,
            evicted_total: 0,
            _metadata: metadata,
        })
    }

    pub(crate) fn evict(&mut self, stream: StreamId, notices: &mut EvictionNotices) {
        notices.push(&stream);
        self.recent[self.next] = Some(stream);
        self.next = (self.next + 1) % RECENT_EVICTIONS;
        self.evicted_total = self.evicted_total.saturating_add(1);
    }

    // Callers must pre-admit response storage before requesting a snapshot.
    pub(crate) fn recent(&self) -> Vec<StreamId> {
        let mut recent = Vec::with_capacity(RECENT_EVICTIONS);
        for offset in 0..RECENT_EVICTIONS {
            if let Some(stream) = &self.recent[(self.next + offset) % RECENT_EVICTIONS] {
                recent.push(stream.clone());
            }
        }
        recent
    }
}

// A synchronous gated operation can evict at most the admitted inventory. Keep
// notices inline: no heap allocation, extra Arc-held journal, or background queue.
// A line needs at most 181 bytes: prefix (31), JSON keys/punctuation (33), host
// hex (32), SessionId (64 ASCII bytes without JSON escapes), epoch (20), LF (1).
const NOTICE_BYTES: usize = 256;
pub(crate) struct EvictionNotices {
    lines: [[u8; NOTICE_BYTES]; fl_host_proto::DEFAULT_MAX_SESSIONS],
    lengths: [usize; fl_host_proto::DEFAULT_MAX_SESSIONS],
    len: usize,
}
impl EvictionNotices {
    pub(crate) fn new() -> Self {
        Self {
            lines: [[0; NOTICE_BYTES]; fl_host_proto::DEFAULT_MAX_SESSIONS],
            lengths: [0; fl_host_proto::DEFAULT_MAX_SESSIONS],
            len: 0,
        }
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len == self.lines.len()
    }

    fn push(&mut self, stream: &StreamId) {
        let mut line = std::io::Cursor::new(self.lines[self.len].as_mut_slice());
        line.write_all(b"fl-host: completed PTY evicted ")
            .expect("fixed notice prefix fits");
        // HostInstance's Debug is deliberately redacted; serialize the full
        // public identity into bounded memory, never into an IO sink under locks.
        serde_json::to_writer(&mut line, stream).expect("bounded StreamId fits notice");
        line.write_all(b"\n").expect("bounded notice newline fits");
        self.lengths[self.len] = line.position() as usize;
        self.len += 1;
    }

    pub(crate) fn emit(&self, sink: &mut dyn Write) -> std::io::Result<()> {
        for (line, len) in self.lines[..self.len].iter().zip(&self.lengths) {
            sink.write_all(&line[..*len])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_host_proto::{HostInstance, SessionEpoch, SessionId};

    #[test]
    fn notice_batch_preserves_full_maximum_identities_and_order() {
        let mut notices = EvictionNotices::new();
        let session = "a".repeat(64);
        let mut expected = Vec::new();
        for offset in 0..fl_host_proto::DEFAULT_MAX_SESSIONS {
            let stream = StreamId {
                host: HostInstance::new([0xff; 16]),
                session: SessionId::new(&session).unwrap(),
                epoch: SessionEpoch(u64::MAX - offset as u64),
            };
            notices.push(&stream);
            expected.push(stream);
        }
        assert!(notices.is_full());
        let mut output = Vec::new();
        notices.emit(&mut output).unwrap();
        let emitted: Vec<StreamId> = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str(
                    line.strip_prefix("fl-host: completed PTY evicted ")
                        .unwrap(),
                )
                .unwrap()
            })
            .collect();
        assert_eq!(emitted, expected);
    }
}
