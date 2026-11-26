//! Private exit evidence survives the independently bounded PTY inventory.

use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::Arc;

use fl_host_proto::{
    DEFAULT_MAX_SESSIONS, ExtensionRefusal, HostedExitEvidence, HostedExitEvidenceStatus,
    MAX_HOSTED_EXIT_TAIL_BYTES, MAX_HOSTED_EXIT_TAIL_LINES, MAX_HOSTED_OWNER_BYTES, StreamId,
};

use crate::session::Session;
use crate::slice_budget::{ChargeKind, Reservation, SliceBudget, reserve};

const PER_NAME: usize = 2;
const TOTAL: usize = PER_NAME * DEFAULT_MAX_SESSIONS;

struct Entry {
    evidence: HostedExitEvidence,
    owner: [u8; MAX_HOSTED_OWNER_BYTES],
    owner_len: usize,
}

pub(crate) struct ExitEvidenceCache {
    entries: VecDeque<Entry>,
    _storage: Reservation,
}

impl ExitEvidenceCache {
    pub(crate) fn new(budget: &Arc<SliceBudget>) -> Result<Self, ExtensionRefusal> {
        // Charge the fixed inventory, dynamic session names and tail capacities,
        // and one more snapshot used while replacing an entry.
        let payload = 64 + MAX_HOSTED_EXIT_TAIL_BYTES;
        let storage = reserve(
            budget,
            ChargeKind::Metadata,
            TOTAL * (size_of::<Entry>() + payload) + payload,
        )?;
        Ok(Self {
            entries: VecDeque::with_capacity(TOTAL),
            _storage: storage,
        })
    }

    pub(crate) fn remember(&mut self, session: &Session) {
        let HostedExitEvidenceStatus::Exited(evidence) = session.capture_hosted_exit() else {
            return;
        };
        let mut matches = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.evidence.stream.session == evidence.stream.session);
        if let Some((first, _)) = matches.next()
            && matches.count() >= PER_NAME - 1
        {
            self.entries.remove(first);
        }
        if self.entries.len() == TOTAL {
            self.entries.pop_front();
        }
        let hosted_owner = session.hosted_owner();
        let mut owner = [0; MAX_HOSTED_OWNER_BYTES];
        owner[..hosted_owner.len()].copy_from_slice(hosted_owner);
        self.entries.push_back(Entry {
            evidence,
            owner,
            owner_len: hosted_owner.len(),
        });
    }

    pub(crate) fn get(&self, stream: &StreamId, owner: &str) -> HostedExitEvidenceStatus {
        self.entries
            .iter()
            .find(|entry| {
                entry.evidence.stream == *stream
                    && entry.owner[..entry.owner_len] == *owner.as_bytes()
            })
            .map_or(HostedExitEvidenceStatus::Unavailable, |entry| {
                HostedExitEvidenceStatus::Exited(entry.evidence.clone())
            })
    }
}

#[derive(Clone, Copy, Default)]
enum Escape {
    #[default]
    Ground,
    Esc,
    Intermediate,
    Csi,
    String {
        osc: bool,
    },
    StringEsc {
        osc: bool,
    },
}

// Keep the parser across reads so split escape strings and UTF-8 scalars
// are interpreted correctly even after their raw bytes leave the backlog.
#[derive(Default)]
struct TerminalParser {
    escape: Escape,
    utf8: [u8; 4],
    utf8_len: usize,
}

impl TerminalParser {
    fn scan(&mut self, bytes: &[u8], mut emit: impl FnMut(char)) {
        for &byte in bytes {
            if self.utf8_len != 0 {
                self.utf8[self.utf8_len] = byte;
                self.utf8_len += 1;
                match std::str::from_utf8(&self.utf8[..self.utf8_len]) {
                    Ok(text) => {
                        let ch = text.chars().next().expect("complete UTF-8 scalar");
                        self.utf8_len = 0;
                        if let Some(printable) = self.filter(ch) {
                            emit(printable);
                        }
                        continue;
                    }
                    Err(err) if err.error_len().is_none() => continue,
                    // Drop the broken prefix and reconsider this byte: it
                    // may be an ESC or the start of a valid scalar.
                    Err(_) => self.utf8_len = 0,
                }
            }
            match byte {
                0xc2..=0xf4 => {
                    self.utf8[0] = byte;
                    self.utf8_len = 1;
                }
                0x00..=0x9f => {
                    // Standalone C1 also occurs in 8-bit terminal streams.
                    if let Some(ch) = self.filter(char::from(byte)) {
                        emit(ch);
                    }
                }
                _ => {}
            }
        }
    }

    fn filter(&mut self, ch: char) -> Option<char> {
        use Escape::*;
        self.escape = match (self.escape, ch) {
            (_, '\x18' | '\x1a' | '\u{9c}') => Ground,
            (String { osc: true } | StringEsc { osc: true }, '\x07') | (StringEsc { .. }, '\\') => {
                Ground
            }
            (String { osc } | StringEsc { osc }, '\x1b') => StringEsc { osc },
            (String { osc } | StringEsc { osc }, _) => String { osc },
            (_, '\x1b') => Esc,
            (Esc, '[') | (_, '\u{9b}') => Csi,
            (Esc, ']') | (_, '\u{9d}') => String { osc: true },
            (Esc, 'P' | 'X' | '^' | '_') | (_, '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}') => {
                String { osc: false }
            }
            (Esc, ' '..='/') => Intermediate,
            (Csi, '@'..='~') | (Esc | Intermediate, '0'..='~') => Ground,
            (Ground, ch) if !ch.is_control() || matches!(ch, '\n' | '\t') => return Some(ch),
            (state, _) => state,
        };
        None
    }
}

const CAP: usize = MAX_HOSTED_EXIT_TAIL_BYTES;

pub(crate) struct TerminalTail {
    parser: TerminalParser,
    bytes: [u8; CAP],
    end: usize,
    len: usize,
}

impl TerminalTail {
    pub(crate) const STORAGE_BYTES: usize = size_of::<Self>() + size_of::<Option<Box<Self>>>();

    pub(crate) fn new() -> Self {
        Self {
            parser: TerminalParser::default(),
            bytes: [0; CAP],
            end: 0,
            len: 0,
        }
    }

    pub(crate) fn push(&mut self, raw: &[u8]) {
        self.parser.scan(raw, |ch| {
            for &byte in ch.encode_utf8(&mut [0; 4]).as_bytes() {
                self.bytes[self.end] = byte;
                self.end = (self.end + 1) % CAP;
                self.len = (self.len + 1).min(CAP);
            }
        });
    }

    pub(crate) fn snapshot(&self) -> String {
        let end = self.end + CAP;
        let mut start = end - self.len;
        let mut lines = 0;
        let trailing_lf = self.len > 0 && self.bytes[(end - 1) % CAP] == b'\n';
        for index in (start..end - usize::from(trailing_lf)).rev() {
            if self.bytes[index % CAP] == b'\n' {
                lines += 1;
                if lines == MAX_HOSTED_EXIT_TAIL_LINES {
                    start = index + 1;
                    break;
                }
            }
        }
        while start < end && self.bytes[start % CAP] & 0xc0 == 0x80 {
            start += 1;
        }
        let mut tail = Vec::with_capacity(end - start);
        tail.extend((start..end).map(|index| self.bytes[index % CAP]));
        String::from_utf8(tail).expect("terminal tail contains complete UTF-8 scalars")
    }
}
