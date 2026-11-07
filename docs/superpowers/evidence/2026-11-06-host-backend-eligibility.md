# Task 0: backend eligibility and portable snapshot evidence

**Approved contract:** [design addendum](../specs/2026-11-06-host-replay-pty-addendum.md) and [implementation plan, Task 0](../plans/2026-11-06-host-replay-pty.md). Evidence baseline: `414c95306e017eb6bbd3afe4e57b36a342a2bcec` (approved documents `08e9146a` plus main `02d8d9d8`). Product execution gates and accepted recovery remain unchanged.

Status: **BLOCKED for PTY stage eligibility**. Source-only research on 2026-11-07; no compilation, dependency edits, candidate-library/vendor invocation, native tests or measurements. Only this evidence document was created. Reliable Tasks 1–4 are independent. This report proposes a schema for review; it does not register/approve it or choose a product dependency.

## 1. Reproducible source provenance

Official crates were retrieved directly from static.crates.io, extracted as data under `/tmp/felicia-host-task0-sources`, and read without building/executing them. Package manifests and included licenses were read. Git commits below come from each archive's `.cargo_vcs_info.json`; archive digests pin the actual reviewed bytes.

| Package | Manifest license | Package Git commit | Archive SHA256 |
| --- | --- | --- | --- |
| vt100 0.15.2 (archived candidate) | MIT | `4c9e5da3de10d838d792929f1691024d6bf59c85` | `84cd863bf0db7e392ba3bd04994be3473491b31e66340672af5d11943c6274de` |
| vt100 0.16.2 (additional candidate) | MIT | `eb66ffaf7d771c13303ef73b29f6f2a56fdacecf` | `054ff75fb8fa83e609e685106df4faeffdf3a735d3c74ebce97ec557d5d36fd9` |
| vte 0.15.0 (vt100 0.16.2 declared dependency) | Apache-2.0 OR MIT | `3b3da71c34cc1256c7e20981cf03f8eb95e08ffc` | `a5924018406ce0063cd67f8e008104968b74b563ee1b85dde3ed1f7cb87d3dbd` |
| portable-pty 0.9.0 | MIT | `f8921727a11b9f8b073e8c24821d72fd41283500`, subdirectory `pty` | `b4a596a2b3d2752d94f51fac2d4a96737b8705dddd311a32b9af47211f08671e` |

Primary archive URLs: [vt100 0.15.2](https://static.crates.io/crates/vt100/vt100-0.15.2.crate), [vt100 0.16.2](https://static.crates.io/crates/vt100/vt100-0.16.2.crate), [vte 0.15.0](https://static.crates.io/crates/vte/vte-0.15.0.crate), [portable-pty 0.9.0](https://static.crates.io/crates/portable-pty/portable-pty-0.9.0.crate). vt100 declares a version range, not a resolved dependency graph: this is a precise review of vte 0.15.0, not a claim about every future/resolved 0.15.x. No Cargo.lock was generated. A preliminary vte 0.11.1 archive was also downloaded but is not used to establish the current candidate's behavior.

Microsoft's [ClosePseudoConsole source](https://github.com/MicrosoftDocs/Console-Docs/blob/d297f58259a48a2ce5d77b427bc2b5dc6a573b00/docs/closepseudoconsole.md), commit `d297f58259a48a2ce5d77b427bc2b5dc6a573b00`, raw SHA256 `8b3217074f5d29ced007c8138e87f58ec03adeb29c7a078ad171897c0477cc05`, was retained as `microsoft-closepseudoconsole.md`. This documentation is primary platform evidence, not native execution evidence.

## 2. Renderer/parser findings

Source references below are relative to the exact extracted package directories. Public views: [vt100 source](https://docs.rs/crate/vt100/0.16.2/source/src/), [vte source](https://docs.rs/crate/vte/0.15.0/source/src/lib.rs).

**vt100 0.16.2 as-is: BLOCKED.**

- `cell.rs:3–17` has a fixed 22-byte glyph array and an asserted 32-byte Cell. Thus the cell body alone at 131,072 cells is 4 MiB, not an unbounded per-cell String. However `cell.rs:55–66` silently returns when a combining addition cannot fit. No overflow/refusal reaches the caller. This violates the approved requirement to report ProjectionUnavailable rather than silently alter/truncate an exact screen. `screen.rs:748–776` calls this path for combining characters. Rejecting before mutation would require an additional proven mechanism or upstream/fork changes; none is selected here.
- `parser.rs:3–5` privately owns vte::Parser and WrappedScreen; public APIs at 48–75 expose process/screen/callbacks, but not the parser's ground/partial-UTF8 state or a continuation-state capture/restore operation. Screen cloning alone omits the parser. The approved whole-chunk quiescence criterion cannot be checked through this API as-is.
- `screen.rs:55–65` privately stores main and alternate grids, current/saved attributes and modes. `grid.rs:4–16` privately stores current/saved position, scroll region, origin/saved-origin and scrollback. `state_formatted` at 224–228 renders current contents/input mode; `write_contents_formatted` at 255–258 uses the active grid. It is not a documented complete reconstruction of inactive screen, saved cursors, scroll region and all continuation state. No portable state import API is present.
- `perform.rs:51–53` deliberately ignores shift-in/out character-set controls; `grid.rs:667–670` uses fixed 8-column tabs, without a programmable tab-stop state. A claim of complete xterm continuation cannot be based on visual contents alone. Callbacks expose several unhandled sequences but not the silent cell truncation above.
- `row.rs:5,12,44–46,73–75` uses Vec cells, creates rows, inserts cells and resizes; `grid.rs:8,13,35–43,66–81,561–570` uses Vec/VecDeque, replacement rows and push-before-pop scrollback. Length bounds alone do not account capacity, temporary replacement rows, cloning or allocator overhead. Counted row/cell admission would need audited inclusive capacities and resize peaks before reservation. Formatted exports allocate growing Vec/String; they cannot bypass snapshot byte charging.
- `vte 0.15.0/Cargo.toml` defaults to std, and vt100's dependency leaves defaults enabled. `vte/src/lib.rs:55–69` has std OSC `Vec<u8>`. `action_osc_put` at544–551 checks capacity only for non-std, then pushes every byte. An unterminated OSC can grow beyond 64 KiB before the final callback. Inspecting/bounding completed OSC callbacks is too late. Non-std selects a fixed ArrayVec, but its full-buffer branch silently returns; that configuration alone is not the approved overflow signal and does not expose quiescence through vt100. The parser's state and partial_utf8 are private.

**vt100 0.15.2 as-is: BLOCKED.** Its `cell.rs:3–10,43–57` similarly uses a fixed six-character cell and silently drops further additions. `screen.rs:1535–1549` allocates title/icon Strings from OSC contents. It does not supply the newly required bounded complete-state/quiescence API. The newer candidate therefore does not remove the material blockers, and the archived version is not grandfathered in.

No renderer candidate is eligible; this bounded review does not assert that no suitable renderer can exist. No transitive-allocation closure or measured 100-ms/RSS result is claimed after these early disqualifications.

## 3. Platform ownership findings

[portable-pty 0.9.0 source](https://docs.rs/crate/portable-pty/0.9.0/source/src/) provides useful platform setup examples, **not an eligible foundation replacement**.

- Unix `unix.rs:228–286` redirects the slave, clears signals, calls setsid and TIOCSCTTY in pre_exec, then spawns directly. This supports the basic controlling-terminal setup. Its public SlavePty interface (`lib.rs:161–166`) offers spawn_command, not the private slave FD/publication/Go ownership seam required by the existing supervisor. `UnixSlavePty` is private (`unix.rs:309–313`). The default spawn cannot replace the foundation's unreaped supervisor and identity publication-before-Go pipeline. Existing `host/src/supervisor.rs:119–140,508–520` publishes identity, waits Go and owns setsid/dup2/exec. A narrow raw-OS openpty/slave setup inside that pipeline is a plausible proposed route, but remains unimplemented/unvalidated.
- Windows `win/psuedocon.rs:127–145` creates a one-entry attribute list, sets only PSEUDOCONSOLE, and CreateProcessW runs without creation-time Job-list attributes or CREATE_SUSPENDED. `win/procthreadattr.rs:47–64` only exposes set_pty. Assigning a Job after this spawn would violate containment. Foundation `host/src/child/windows.rs:64–104,123–150` already uses Job-list+handle-list attributes, creates suspended and verifies membership before release. A proposed direct Windows backend must combine its existing authority with the pseudoconsole attribute before creation, retaining process/Job identity and publication barriers; portable-pty's standard spawn is not that backend.
- `PsuedoCon::drop` calls ClosePseudoConsole directly (`win/psuedocon.rs:73–75`); readers/writers are cloned synchronous descriptors (`win/conpty.rs:95–107`), with no bounded cancellation/drain protocol in the exposed trait. Microsoft documents an older-Windows close/drain deadlock risk and immediate-return behavior starting Windows 11 24H2. The supported OS baseline and cancellation/close schedule must be explicit and native-tested; a Drop call is not bounded shutdown proof. INHERIT_CURSOR is also requested by this package, so its query-response/drain implications need deliberate treatment.
- `win/mod.rs:25–38` polls GetExitCodeProcess and classifies 259 as live; `wait` separately uses a signaled wait. The default polling path cannot establish the foundation's exact exit-259 contract. Keep the foundation's independent signaled-handle/Job evidence instead.

Platform status: Unix setup pattern source-supported but supervisor integration/cancellation **unproven**; Windows standard portable-pty spawn **ineligible**; direct foundation-compatible Windows backend **unproven**. No native compatibility, performance or residual containment claim follows.

## 4. Portable snapshot schema candidate: ID 1, UNREGISTERED/NOT APPROVED

This is a concrete review proposal, not a backend-private dump or selected renderer representation. It must be amended if an eligible backend exposes additional continuation state. Until a complete state mapping and Lead schema disposition exist, PTY schema/codecs remain blocked. Parser quiescence is a precondition; partial UTF-8/CSI/OSC/DCS bytes are never omitted by claiming a snapshot at an unsafe point.

All integers are unsigned big-endian. No pointers, machine-sized integers, native struct padding, compression or implicit defaults. Schema 1 parts use the plan's SnapshotBegin/State-or-Row/SnapshotEnd framing; Begin carries exact identity-bound snapshot ID, W, revision, schema 1, dimensions and Coverage. Unsupported schema/profile/field flags refuse before allocating/reconstructing and never synchronize.

**State binary payload**, <=64 KiB: ordered TLVs (`tag:u16,len:u32,bytes`), strictly increasing tags, every required tag exactly once, no unknown tags or trailing bytes. Tags:

1. profile/version (fixed ID for the explicitly audited supported terminal-sequence profile; no claim of generic xterm completeness).
2. geometry/row inventory (cols,rows,mainRows,alternateRows,historyRows as u16; totalCells as u32; all checked).
3. Active screen (`u8`).
4. Current main cursor.
5. Saved main cursor.
6. Current alternate cursor.
7. Saved alternate cursor. Each cursor in tags 4–7 encodes `row,col:u16`, `wrapPending:u8`, `origin:u8`, drawing attributes (28 B), G0–G3 designation IDs (`u16` each), and `activeGL,activeGR:u8`.
8. Main scroll top/bottom (`u16` each).
9. Alternate scroll top/bottom (`u16` each).
10. tab bitmap per screen (exact ceil(cols/8), unused bits zero).
11. current modes and saved modes (fixed audited bit table u64 each: autowrap, insert, newline, origin, application cursor/keypad, bracketed paste, focus reporting, cursor visibility/blink, mouse protocols/encoding and other profile modes; reserved bits zero).
12. palette/default foreground/background/cursor colors (256 RGB24 entries plus three RGB24 defaults).
13. bounded link/title metadata (title UTF-8 <=4,096 B; at most 256 links with u32 ID, u16 byte length and UTF-8 URI <=128 B, all aggregate lengths within 64 KiB).
14. decoder/charset profile and quiescent assertion (pending byte counts all zero; supported charset designation table pinned by profile). Missing state in this field inventory from a future backend blocks that profile/schema; it cannot silently be dropped.

**Row payload**: header 8 B `{screen:u8,flags:u8,cols:u16,ordinal:u32}`, then exactly cols 96-B cells. Screens/order: main history oldest-first, main visible top-to-bottom, alternate visible top-to-bottom. flags currently softWrapped only, other bits zero. ordinal is globally contiguous across this order; no duplicates, gaps or mixed dimensions. Each cell 96-B is `{glyphLen:u8,width:u8,flags:u16,attributes:28B,glyphUTF8:[u8;64]}`. Glyph tail is zero; valid UTF-8 exactly glyphLen, width 0/1/2 and wide-continuation relations checked. Attr 28 B = fg/bg/underline colors 5 B each (tag plus u32 value), `flags:u32`, `underlineStyle:u8`, `hyperlinkID:u32`, reserved 4 B zero. At most 64 glyph bytes; an attempted overflow makes projection unavailable **before silent truncation**, not a larger cell or guessed screen. URI/title/cell overflows likewise invalidate exact projection rather than invoke external actions or truncate silently.

Reconstruction stages one bounded snapshot without displaying it as exact. Validate State inventory, complete unique ordered rows, cell/row counts, wide-cell pairing, referenced link IDs, all flags, Coverage/revision/W, then End. Atomically replace the client's screen/continuation state only after all validation and a covered contiguous suffix; then apply whole chunks W+1 onward. No schema values authorize terminal/device/vendor side effects. Any mismatch/cancel/gap/disconnect discards staged state and remains unsynchronized; an already exact same-schema viewer may use the separate ResumeReady path only with actually retained parser state.

Candidate wire arithmetic: `N <=131,072` cells and `R <=N` rows give `96N + 8R <=13 MiB`; add State 64 KiB and one largest row 96,008 B at 1,000 columns: <13.16 MiB within 16 MiB. Snapshot reservations must cover simultaneous source/copy/encoder staging; these numbers do not authorize an additional full clone. In-memory renderer representation/capacity is not proved by wire encoding. Base64 of a 96,008-B row is 128,012 B plus envelope, within 256 KiB; raw row is below 128 KiB. The approved 185-MiB category maxima remain below 192 MiB, but actual library/state/snapshot/metadata/allocator and native timing proof remains absent.

Canonical fixture specifications for future implementation (not executed): (a) 2x2 blank main/alternate, explicit all-required-State TLVs, 4 rows/8 cells, exact byte/length oracle; (b) main red cell+saved cursor/attributes, alternate active, nondefault tabs/scroll region/charset/wrap state, round-trip then suffix equals continuous oracle; (c) wide+combining glyph/link reference and oldest-first history; (d) unknown schema/profile/tag/flags, duplicate/missing/reordered rows/TLVs, malformed UTF-8/width/link/length, extra bytes, pre-End disconnect and ring gap all refuse without synchronization; (e) max 131,072 cells, 1,000-column row, State 64 KiB and glyph 65-B/control-string 65,537-B boundary refusal with precharged storage. Complete checked-in byte fixtures and a backend mapping are still required before Lead can dispose this schema.

## 5. Disposition required

Task 0 PTY result is BLOCKED: no backend+renderer combination examined meets the contract as-is; no measured memory/native/100-ms results; schema 1 above is an unregistered review candidate with no eligible-backend mapping or executed fixtures. Minimum concrete seam for further design is whether to authorize a tightly scoped audited parser/renderer fork or another source-eligible implementation exposing quiescence, full continuation export/import, overflow refusal and precharged allocation; plus a foundation-owned platform backend rather than portable-pty's default spawn. Do not silently select a fork, relax targets or begin PTY Tasks 5–8. This blocker does not stop reliable Tasks 1–4.
