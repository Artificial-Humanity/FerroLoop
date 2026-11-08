# Bounded stdout fragments implementation plan

> **For agentic workers:** Use `subagent-dev` and `code-review` per WORKFLOW.md.

**Goal:** Keep children running while promptly framing stdout and explicitly
reporting any unattended retention loss.

**Architecture:** Add negotiated minor-2 termination metadata to both existing output
paths. Coalesce never-exposed tails and evict oldest reliable data under pressure
only without a live cursor. Report gaps explicitly, preserve lifecycle records,
retain bounded memory, and keep reliable ACK semantics separate from eviction.

**Tech Stack:** Rust, serde, existing host transport and budget machinery.

**Spec:** ../specs/2026-11-08-stdout-fragments-178-design.md

## Global constraints

- One MiB maximum logical record; existing transport, memory, and event caps remain.
- No invented LF, silent loss, implicit ACK, child cleanup due to long output, or
  mutation after a reader could have observed a record.
- Retained bytes reconstruct exactly. With a live reliable cursor, no eviction;
  without one, bounded data eviction permits continued production and reports gaps.
- Protocol minor 2; old terminated frames unchanged, unsupported fragment stops
  only the old subscription. New decoders interpret missing metadata as true.
- Isolated synthetic child fixtures only; no service changes, installs, or vendors.
- Commit with `Co-authored-by: Fiona <fiona@artificialhumanity.io>`.

## Review focus

- Unobserved versus previously observed tails, including dropped replay references.
- Cap-boundary LF and EOF tails, CR/ANSI/binary bytes, and multipart disagreement.
- Quota exhaustion, pending ownership, and non-reliable queue byte accounting.
- Legacy subscriber refusal without completion credit or producer cleanup.
- Unattended record/byte exhaustion, sparse retained sequences, preserved lifecycle
  records, gap-first replay, and cursor-attachment races with eviction.

## Task 1: End-to-end framing and retention repair

This is one reviewable task: schema, producers, and consumers must change together
to avoid passing tests that cover only one side of the stream contract.

**Files:** `crates/host-proto/src/{types,frame,extension,client,lib}.rs`,
`crates/host/src/{reliable,output,daemon,authority}.rs`,
`crates/host/src/bin/fl-host-testchild.rs`, `crates/host-proto/tests/frame.rs`,
`crates/host/tests/{host_ipc,reliable_events}.rs`. Touch further callers only if the
new field requires it. Unit tests stay in each module; subprocess tests in tests/.

**Interfaces:** Add `terminated: bool` to stdout records and public line events.
Keep existing `fragment_line` behavior as terminated=true; add a fragment helper
accepting termination for producer use. Reliable parts default missing to true and
omit true serialization. Binary flags use 0x01 for more and 0x02 for unterminated.
Pass negotiated minor to both emission paths and validate it on client decode.
Add a negotiated gap/notice that names the first receivable sequence, and carry
that information through the client replay path without pretending it was ACKed.

- [ ] Write failing regressions for >2 MiB without LF in both real host paths;
  reconstruct bytes from records plus LF iff terminated, assert no ProtocolFailure
  and running child, and exercise LF at a boundary and EOF without LF.
- [ ] Add focused failing unit tests for 100,000 tiny unattended admissions (spec
  input), later exact replay, record count/budget bounds, frozen-tail behavior after
  reference drop, and eligible coalescing when the record cap is reached.
- [ ] Add protocol tests for legacy byte-identical frames and strict old decoders,
  false marker round trips, mixed termination rejection, and minor-1 refusal scoped
  to subscription with no ACK/completion and no cleanup. Demonstrate failures.
- [ ] Add failing tests for 100,000 mixed LF/unterminated reliable admissions with
  no subscriber, bounded data count and bytes, no stalls, lifecycle preservation,
  and unchanged ACK watermark. Force both byte and record pressure. Attach a late
  subscriber and assert gap first, then exact retained bytes. Hold a live cursor
  and prove lossless backpressure instead of eviction.
- [ ] Implement termination propagation, negotiated emission/decoding, bounded
  read-boundary flushing, and coalescing with permanent exposure tracking. Replace
  overflow-cleanup behavior in drain, EOF, and release_tail. Preserve pending data
  under live-cursor quota pressure. With no live cursor, evict oldest data under
  pressure, retain lifecycle records, and expose the explicit gap on later replay.
  Serialize cursor admission and eviction; update journal indexing for sparse
  retained sequences. Audit any unreachable failure helpers for removal.
- [ ] Update existing tests that assume one full logical line per read-independent
  record to reconstruct bytes correctly; retain their original behavioral coverage.
- [ ] Run `cargo test -p fl-host-proto -p fl-host` and focused clippy; commit changes
  in coherent milestones and report hashes, test evidence, and remaining concerns.
- [ ] Coordinator verifies commits and dispatches fresh read-only code review;
  accepted findings go to a fresh fixer followed by scoped re-review (max 3 rounds).

## Final verification and publication

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace` including host subprocess integration tests.
- [ ] Fresh whole-branch review; verify current main, clean tree, trailers.
- [ ] `fl github pr create`, send Ferris review request, checkpoint FerroState.

Use `CARGO_TARGET_DIR=/home/lmcfarlin/Projects/Artificial-Humanity/scratch/ferroloop/fiona-target`.
Keep logs in `scratch/ferroloop/`. Do not repeat a passing suite without new changes
or a concrete unresolved concern.
