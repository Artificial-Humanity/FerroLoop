//! Bounded, persistent PTY projection. The session mutex orders updates and resize.
use crate::slice_budget::{ChargeKind, Reservation, SliceBudget, reserve};
use fl_host_proto::{ExtensionRefusal, HostError, PtyOptions};
use std::num::NonZeroU16;
use std::sync::Arc;

/// Text is bounded independently of the response envelope. vt100 0.18.1 stores
/// at most 22 UTF-8 bytes/cell: 8192 * 22 + two newlines/row < this bound.
/// For JSON, a cell has at most one width-bearing character, hence at most
/// one extra quote/backslash escape. 8192 * 23 + 4 * 128 escaped newline
/// bytes + 4096 envelope bytes = 193024, below the 256 KiB frame limit.
/// The response writer must still reserve and check its actual envelope.
pub use fl_host_proto::MAX_RENDERED_TEXT;
pub(crate) struct ProjectionSnapshot {
    pub contents: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub rows: u16,
    pub cols: u16,
    pub generation: u64,
    pub output_seq: u64,
    pub output_bytes: u64,
    // The response owner may move this permit alongside contents, keeping it
    // alive through serialization and write. It must separately admit JSON.
    pub(crate) _reservation: Reservation,
}

pub(crate) struct Projection {
    parser: vt100::Parser,
    filter: StringFilter,
    budget: Arc<SliceBudget>,
    reservation: Reservation,
    charged: usize,
    high_rows: u16,
    high_cols: u16,
    scrollback: u16,
    generation: u64,
    output_seq: u64,
    output_bytes: u64,
}

// Pinned vt100 cells are 32 bytes and never allocate content. Each row owns a
// Vec<Cell>; the primary/alternate row Vecs and scrollback VecDeque can retain
// capacity after shrinking. High-water rows/cols bound every retained row,
// including old-width scrollback. Factor four covers geometric Vec growth plus
// old/new allocations coexisting at resize; 64 bytes/row covers row/vector
// metadata and spare deque slots. One extra row covers scrolling's temporary.
// This also reserves resize peak storage at initial admission, before spawn.
fn storage_bound(rows: u16, cols: u16, scrollback: u16) -> usize {
    4 * (2 * usize::from(rows) + usize::from(scrollback) + 1) * (usize::from(cols) * 32 + 64)
        + std::mem::size_of::<Projection>()
        + 4096
}

pub(crate) fn default_storage_bound() -> usize {
    let opts = PtyOptions::default();
    storage_bound(opts.rows, opts.cols, opts.scrollback_rows)
}

impl Projection {
    #[cfg_attr(not(any(unix, test)), expect(dead_code, reason = "Unix PTY backend"))]
    pub(crate) fn new(
        budget: Arc<SliceBudget>,
        opts: PtyOptions,
    ) -> Result<Self, ExtensionRefusal> {
        opts.validate()
            .map_err(|_| ExtensionRefusal::ResourceLimit)?;
        let charged = storage_bound(opts.rows, opts.cols, opts.scrollback_rows);
        let reservation = reserve(&budget, ChargeKind::Projection, charged)?;
        Ok(Self {
            parser: vt100::Parser::new(
                NonZeroU16::new(opts.rows).unwrap(),
                NonZeroU16::new(opts.cols).unwrap(),
                opts.scrollback_rows.into(),
            ),
            filter: StringFilter::default(),
            budget,
            reservation,
            charged,
            high_rows: opts.rows,
            high_cols: opts.cols,
            scrollback: opts.scrollback_rows,
            generation: 0,
            output_seq: 0,
            output_bytes: 0,
        })
    }
    pub(crate) fn process(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.filter
            .process(bytes, |chunk| self.parser.process(chunk));
        self.generation = self.generation.saturating_add(1);
        self.output_bytes = self.output_bytes.saturating_add(bytes.len() as u64);
    }
    pub(crate) fn set_output_seq(&mut self, seq: u64) {
        self.output_seq = self.output_seq.max(seq);
    }
    pub(crate) fn snapshot(&self) -> Result<ProjectionSnapshot, ExtensionRefusal> {
        // contents() grows a String geometrically: final capacity is at most
        // twice the text bound, and the old buffer can coexist during growth.
        // Admit all three bounds before asking vt100 to allocate anything.
        let reservation = reserve(
            &self.budget,
            ChargeKind::Snapshot,
            3 * MAX_RENDERED_TEXT + std::mem::size_of::<ProjectionSnapshot>(),
        )?;
        let screen = self.parser.screen();
        let contents = screen.contents();
        if contents.len() > MAX_RENDERED_TEXT || contents.capacity() > 2 * MAX_RENDERED_TEXT {
            return Err(ExtensionRefusal::ResourceLimit);
        }
        let (cursor_row, cursor_col) = screen.cursor_position();
        let (rows, cols) = screen.size();
        // vt100 represents pending autowrap with col == width. Expose the
        // visible cursor cell while retaining that pending state in the parser.
        let cursor_col = cursor_col.min(cols.get() - 1);
        Ok(ProjectionSnapshot {
            contents,
            cursor_row,
            cursor_col,
            rows: rows.get(),
            cols: cols.get(),
            generation: self.generation,
            output_seq: self.output_seq,
            output_bytes: self.output_bytes,
            _reservation: reservation,
        })
    }
    #[cfg_attr(not(any(unix, test)), expect(dead_code, reason = "Unix PTY backend"))]
    pub(crate) fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        apply: impl FnOnce() -> Result<(), HostError>,
    ) -> Result<u64, HostError> {
        fl_host_proto::validate_pty_geometry(cols, rows)?;
        let high_rows = self.high_rows.max(rows);
        let high_cols = self.high_cols.max(cols);
        let charged = storage_bound(high_rows, high_cols, self.scrollback);
        let increment = reserve(&self.budget, ChargeKind::Projection, charged - self.charged)
            .map_err(|_| HostError::Protocol("projection capacity".into()))?;
        apply()?;
        self.reservation.merge(increment);
        self.charged = charged;
        self.high_rows = high_rows;
        self.high_cols = high_cols;
        self.parser.set_size(
            NonZeroU16::new(rows).unwrap(),
            NonZeroU16::new(cols).unwrap(),
        );
        self.generation = self.generation.saturating_add(1);
        Ok(self.generation)
    }
}

/// Never give vte a control-string introducer: its OSC accumulator otherwise
/// grows until termination. Payload storage here is zero, including malformed
/// or unterminated strings. UTF-8 tracking distinguishes C1 bytes from valid
/// continuations (including inside discarded strings).
#[derive(Default)]
struct StringFilter {
    // Tracks downstream Escape; the ESC itself has already been emitted.
    escape: bool,
    string: Option<bool>, // true for OSC, which also terminates at BEL
    string_escape: bool,
    utf8_left: u8,
    utf8_min: u8,
    utf8_max: u8,
}
impl StringFilter {
    fn continuation(&mut self, byte: u8) -> bool {
        if self.utf8_left > 0 && (self.utf8_min..=self.utf8_max).contains(&byte) {
            self.utf8_left -= 1;
            self.utf8_min = 0x80;
            self.utf8_max = 0xbf;
            return true;
        }
        self.utf8_left = match byte {
            0xc2..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf4 => 3,
            _ => 0,
        };
        self.utf8_min = match byte {
            0xe0 => 0xa0,
            0xf0 => 0x90,
            _ => 0x80,
        };
        self.utf8_max = match byte {
            0xed => 0x9f,
            0xf4 => 0x8f,
            _ => 0xbf,
        };
        false
    }
    fn process(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        let mut output = [0u8; 4096];
        let mut len = 0;
        for &byte in bytes {
            if len == output.len() {
                emit(&output[..len]);
                len = 0;
            }
            let continuation = self.continuation(byte);
            if let Some(osc) = self.string {
                if !continuation
                    && (byte == 0x9c
                        || byte == 0x18
                        || byte == 0x1a
                        || (osc && byte == 0x07)
                        || (self.string_escape && byte == b'\\'))
                {
                    self.string = None;
                }
                self.string_escape = !continuation && byte == 0x1b;
                continue;
            }
            if self.escape {
                if matches!(byte, b']' | b'P' | b'X' | b'^' | b'_') {
                    self.escape = false;
                    self.string = Some(byte == b']');
                    self.string_escape = false;
                    // Cancel any partial CSI/ESC in the downstream parser.
                    output[len] = 0x18;
                    len += 1;
                    continue;
                }
                // Match pinned vte 0.15 advance_esc: executable C0 controls,
                // repeated ESC, DEL and high bytes retain Escape. CAN/SUB
                // cancel it; intermediates and printable bytes leave it.
                self.escape = !matches!(byte, 0x18 | 0x1a | 0x20..=0x7e);
            }
            if !continuation && matches!(byte, 0x90 | 0x98 | 0x9d | 0x9e | 0x9f) {
                self.escape = false;
                self.string = Some(byte == 0x9d);
                self.string_escape = false;
                output[len] = 0x18;
                len += 1;
            } else {
                if byte == 0x1b {
                    self.escape = true;
                }
                output[len] = byte;
                len += 1;
            }
        }
        emit(&output[..len]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn projection() -> Projection {
        Projection::new(SliceBudget::new(), PtyOptions::default()).unwrap()
    }
    #[test]
    fn default_projection_capacity_admits_all_sessions_and_releases_charges() {
        let root = SliceBudget::new();
        let baseline = root.used(ChargeKind::Projection);
        let mut projections = Vec::new();
        for index in 0..fl_host_proto::DEFAULT_MAX_SESSIONS {
            let epoch = SliceBudget::epoch(&root).unwrap();
            let projection = Projection::new(epoch, PtyOptions::default())
                .unwrap_or_else(|error| panic!("default projection {index}: {error:?}"));
            projections.push(projection);
        }
        let epoch = SliceBudget::epoch(&root).unwrap();
        assert!(matches!(
            Projection::new(Arc::clone(&epoch), PtyOptions::default()),
            Err(ExtensionRefusal::ResourceLimit)
        ));
        projections.pop();
        projections.push(Projection::new(epoch, PtyOptions::default()).unwrap());
        drop(projections);
        assert_eq!(root.used(ChargeKind::Projection), baseline);
    }

    #[test]
    fn split_csi_utf8_and_alternate_screen_restore() {
        let mut p = projection();
        for chunk in [
            b"primary\r\x1b[".as_slice(),
            b"2K",
            b"caf\xc3",
            b"\xa9",
            b"\x1b[?1049",
            b"hother",
            b"\x1b[?1049l",
        ] {
            p.process(chunk);
        }
        let s = p.snapshot().unwrap();
        assert_eq!(s.contents, "café");
        assert_eq!((s.cursor_row, s.cursor_col), (0, 4));
        assert_eq!(s.generation, 7);
    }
    #[test]
    fn strings_discard_across_every_split_and_preserve_c1_utf8() {
        for seq in [
            b"\x1b]secret\x07".as_slice(),
            b"\x1bPsecret\x1b\\",
            b"\x1bXsecret\x18",
            b"\x1b^secret\x1a",
            b"\x1b_secret\x9c",
            b"\x9dsecret\x07",
            b"\x90secret\x9c",
            b"\x98secret\x9c",
            b"\x9esecret\x9c",
            b"\x9fsecret\x9c",
        ] {
            for split in 0..=seq.len() {
                let mut p = projection();
                p.process(b"a");
                p.process(&seq[..split]);
                p.process(&seq[split..]);
                for byte in "ÜÝÞß".as_bytes() {
                    p.process(std::slice::from_ref(byte));
                }
                assert_eq!(
                    p.snapshot().unwrap().contents,
                    "aÜÝÞß",
                    "{seq:?} split {split}"
                );
            }
        }
    }
    #[test]
    fn interleaved_string_introducers_never_reach_parser_across_splits() {
        // These bytes leave pinned vte 0.15 in Escape. Standalone C1 string
        // introducers are covered separately: they start a filtered string.
        for ignored in (0x00..=0x17)
            .chain([0x19, 0x1b])
            .chain(0x1c..=0x1f)
            .chain(0x7f..=0xff)
            .filter(|b| !matches!(b, 0x90 | 0x98 | 0x9d | 0x9e | 0x9f))
        {
            for introducer in *b"]PX^_" {
                let mut input = vec![0x1b, ignored, introducer];
                input.extend_from_slice(b"hidden\x1b\\visible");
                for split in 0..=input.len() {
                    let mut filter = StringFilter::default();
                    let mut sanitized = Vec::new();
                    filter.process(&input[..split], |chunk| sanitized.extend_from_slice(chunk));
                    filter.process(&input[split..], |chunk| sanitized.extend_from_slice(chunk));
                    assert!(
                        !sanitized.contains(&introducer),
                        "string introducer reached parser: {input:?}, split {split}: {sanitized:?}"
                    );
                    assert!(sanitized.ends_with(b"visible"));
                    assert!(!sanitized.windows(6).any(|bytes| bytes == b"hidden"));
                }
            }
        }
    }

    #[test]
    fn interleaved_osc_flood_never_reaches_parser() {
        for (prefix, expected) in [
            (b"\x1b\0]".as_slice(), "safe visible"),
            (b"\x1b\x7f]", "safe visible"),
            (b"\x1b\xff]", "safe visible"),
            // The interleaved tab still executes, advancing to column eight.
            (b"\x1b\0\x1b\t\x7f\xc3\x9c]", "safe     visible"),
        ] {
            let mut filter = StringFilter::default();
            let mut p = projection();
            p.process(b"safe");
            // Inspect the actual parser input, not SliceBudget counters: an
            // admitted OSC payload would allocate in vte outside that budget.
            for byte in prefix {
                filter.process(std::slice::from_ref(byte), |chunk| {
                    assert!(!chunk.contains(&b']'), "OSC admitted for {prefix:?}");
                    p.parser.process(chunk);
                });
            }
            let payload = [b'x'; 4096];
            for _ in 0..2300 {
                filter.process(&payload, |chunk| {
                    assert!(chunk.is_empty(), "OSC payload reached parser");
                    p.parser.process(chunk);
                });
            }
            filter.process(b"\x07 visible", |chunk| p.parser.process(chunk));
            assert_eq!(p.snapshot().unwrap().contents, expected);
        }
    }

    #[test]
    fn escape_controls_cancellation_and_intermediates_are_preserved() {
        for input in [
            b"a\x1b\t\x08\r\n[2Cb".as_slice(),
            b"\x1b\0\x7f[?1049halt\x1b[?1049l",
            b"\x1b\x18]visible",
            b"\x1b\x1a]visible",
            b"\x1b(]visible",
            b"\x1b\xff[2Cvisible",
            "café ÜÝÞß".as_bytes(),
        ] {
            let mut filter = StringFilter::default();
            let mut sanitized = Vec::new();
            for byte in input {
                filter.process(std::slice::from_ref(byte), |chunk| {
                    sanitized.extend_from_slice(chunk)
                });
            }
            assert_eq!(sanitized, input);
        }
    }

    #[test]
    fn unterminated_strings_remain_bounded_and_recover() {
        let budget = SliceBudget::new();
        let mut p = Projection::new(Arc::clone(&budget), PtyOptions::default()).unwrap();
        let charged = budget.used(ChargeKind::Projection);
        p.process(b"safe\x1b]");
        let chunk = [b'x'; 4096];
        for _ in 0..2300 {
            p.process(&chunk);
        }
        p.process(b"\xc3\x9cstill-hidden\x1b");
        assert_eq!(p.snapshot().unwrap().contents, "safe");
        p.process(b"\\ visible");
        let s = p.snapshot().unwrap();
        assert_eq!(s.contents, "safe visible");
        assert!(s.output_bytes > 8 * 1024 * 1024);
        assert_eq!(budget.used(ChargeKind::Projection), charged);
    }
    #[test]
    fn redraws_survive_more_than_raw_tail_and_metadata_is_monotonic() {
        let mut p = projection();
        for _ in 0..700_000 {
            p.process(b"\r\x1b[2Kworking");
        }
        p.process(b"\r\x1b[2Kdone");
        p.set_output_seq(99);
        let s = p.snapshot().unwrap();
        assert_eq!(s.contents, "done");
        assert_eq!(s.cursor_col, 4);
        assert_eq!(s.generation, 700_001);
        assert_eq!(s.output_seq, 99);
        assert!(s.output_bytes > 8 * 1024 * 1024);
        p.process(b"");
        assert_eq!(p.snapshot().unwrap().generation, s.generation);
    }
    #[test]
    fn geometry_scrollback_budget_and_snapshot_admission_release() {
        let budget = SliceBudget::epoch(&SliceBudget::new()).unwrap();
        for opts in [
            PtyOptions {
                rows: 0,
                ..PtyOptions::default()
            },
            PtyOptions {
                cols: 257,
                ..PtyOptions::default()
            },
            PtyOptions {
                rows: 129,
                ..PtyOptions::default()
            },
            PtyOptions {
                rows: 128,
                cols: 256,
                scrollback_rows: 0,
            },
            PtyOptions {
                scrollback_rows: 513,
                ..PtyOptions::default()
            },
        ] {
            assert!(Projection::new(Arc::clone(&budget), opts).is_err());
        }
        assert_eq!(budget.used(ChargeKind::Projection), 0);
        let held = reserve(&budget, ChargeKind::Projection, 128 * 1024 * 1024).unwrap();
        assert!(Projection::new(Arc::clone(&budget), PtyOptions::default()).is_err());
        drop(held);
        let p = Projection::new(Arc::clone(&budget), PtyOptions::default()).unwrap();
        let held = reserve(&budget, ChargeKind::Snapshot, 32 * 1024 * 1024).unwrap();
        assert!(p.snapshot().is_err());
        drop(held);
        let s = p.snapshot().unwrap();
        assert!(budget.used(ChargeKind::Snapshot) > 0);
        drop(s);
        assert_eq!(budget.used(ChargeKind::Snapshot), 0);
        drop(p);
        assert_eq!(budget.used(ChargeKind::Projection), 0);
    }
    #[test]
    fn failed_resize_is_atomic_and_shrink_retains_high_water_charge() {
        let budget = SliceBudget::epoch(&SliceBudget::new()).unwrap();
        let mut p = Projection::new(Arc::clone(&budget), PtyOptions::default()).unwrap();
        p.process(b"abc");
        let charged = budget.used(ChargeKind::Projection);
        assert!(
            p.resize(160, 50, || Err(HostError::Io("ioctl failed".into())))
                .is_err()
        );
        let s = p.snapshot().unwrap();
        assert_eq!((s.cols, s.rows, s.generation), (120, 40, 1));
        assert_eq!(budget.used(ChargeKind::Projection), charged);
        assert!(
            p.resize(256, 128, || panic!("invalid geometry reaches ioctl"))
                .is_err()
        );
        let held = reserve(&budget, ChargeKind::Projection, 128 * 1024 * 1024 - charged).unwrap();
        assert!(
            p.resize(160, 50, || panic!("budget refusal reaches ioctl"))
                .is_err()
        );
        drop(held);
        assert_eq!(p.resize(160, 50, || Ok(())).unwrap(), 2);
        let grown = budget.used(ChargeKind::Projection);
        assert!(grown > charged);
        assert_eq!(p.resize(10, 5, || Ok(())).unwrap(), 3);
        assert_eq!(budget.used(ChargeKind::Projection), grown);
        drop(p);
        assert_eq!(budget.used(ChargeKind::Projection), 0);
    }
    #[test]
    fn pending_wrap_cursor_is_a_visible_zero_based_cell() {
        let mut p = Projection::new(
            SliceBudget::new(),
            PtyOptions {
                rows: 2,
                cols: 4,
                scrollback_rows: 0,
            },
        )
        .unwrap();
        p.process(b"abcd");
        let s = p.snapshot().unwrap();
        assert_eq!((s.cursor_row, s.cursor_col), (0, 3));
        p.process(b"e");
        let s = p.snapshot().unwrap();
        assert_eq!(s.contents, "abcde");
        assert_eq!((s.cursor_row, s.cursor_col), (1, 1));
    }

    #[test]
    fn dense_combining_cells_and_escaped_text_fit_reserved_snapshot() {
        let budget = SliceBudget::new();
        let mut p = Projection::new(
            Arc::clone(&budget),
            PtyOptions {
                rows: 32,
                cols: 256,
                scrollback_rows: 0,
            },
        )
        .unwrap();
        // Quotes/backslashes add JSON escaping; combining marks fill cells
        // without advancing the cursor or growing per-cell content storage.
        let cell = "\"\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}";
        for _ in 0..8192 {
            p.process(cell.as_bytes());
        }
        let s = p.snapshot().unwrap();
        assert_eq!(s.contents.chars().filter(|c| *c == '"').count(), 8192);
        assert_eq!(s.contents.len(), 155_648);
        assert!(s.contents.len() <= MAX_RENDERED_TEXT);
        assert!(s.contents.capacity() <= budget.used(ChargeKind::Snapshot));
        let json = serde_json::to_string(&s.contents).unwrap();
        assert!(json.len() + 4096 < fl_host_proto::MAX_FRAME_PAYLOAD);
    }
    #[test]
    fn projection_counters_saturate_instead_of_wrapping() {
        let mut p = projection();
        p.generation = u64::MAX;
        p.output_bytes = u64::MAX;
        p.output_seq = u64::MAX;
        p.process(b"visible");
        p.set_output_seq(1);
        assert_eq!(p.resize(120, 40, || Ok(())).unwrap(), u64::MAX);
        let s = p.snapshot().unwrap();
        assert_eq!(
            (s.generation, s.output_bytes, s.output_seq),
            (u64::MAX, u64::MAX, u64::MAX)
        );
        assert_eq!(s.contents, "visible");
    }
}
