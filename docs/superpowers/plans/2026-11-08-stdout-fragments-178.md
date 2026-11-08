# Bounded stdout fragments implementation plan

> **For agentic workers:** Use `subagent-dev` and `code-review` per WORKFLOW.md.

**Goal:** Keep children alive while promptly framing arbitrary stdout with exact bytes.

**Architecture:** Add negotiated minor-2 termination metadata to both existing output
paths. Coalesce never-exposed tails to prevent unattended read-count exhaustion;
retain existing bounded backlog and explicit reliable ACK semantics.

**Tech Stack:** Rust, serde, existing host transport and budget machinery.

**Spec:** ../specs/2026-11-08-stdout-fragments-178-design.md

## Global constraints

- One MiB maximum logical record; existing transport, memory, and event caps remain.
- No invented LF, lost bytes, implicit ACK, child cleanup due to long output, or
  mutation after a reader could have observed a record.
- Protocol minor 2; old terminated frames unchanged, unsupported fragment stops
  only the old subscription. New decoders interpret missing metadata as true.
- Isolated synthetic child fixtures only; no service changes, installs, or vendors.
- Commit with `Co-authored-by: Fiona <fiona@artificialhumanity.io>`.

## Review focus

- Unobserved versus previously observed tails, including dropped replay references.
- Cap-boundary LF and EOF tails, CR/ANSI/binary bytes, and multipart disagreement.
- Quota exhaustion, pending ownership, and non-reliable queue byte accounting.
- Legacy subscriber refusal without completion credit or producer cleanup.
- Many tiny unattended reads versus real byte/line limit exhaustion.

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

- [ ] Write failing regressions for >2 MiB without LF in both real host paths;
  reconstruct bytes from records plus LF iff terminated, assert no ProtocolFailure
  and running child, and exercise LF at a boundary and EOF without LF.
- [ ] Add focused failing unit tests for 100,000 tiny unattended admissions (spec
  input), later exact replay, record count/budget bounds, frozen-tail behavior after
  reference drop, and eligible coalescing when the record cap is reached.
- [ ] Add protocol tests for legacy byte-identical frames and strict old decoders,
  false marker round trips, mixed termination rejection, and minor-1 refusal scoped
  to subscription with no ACK/completion and no cleanup. Demonstrate failures.
- [ ] Implement termination propagation, negotiated emission/decoding, bounded
  read-boundary flushing, and coalescing with permanent exposure tracking. Replace
  overflow-cleanup behavior in drain, EOF, and release_tail. Preserve pending data
  under quota pressure. Audit any unreachable failure helpers for removal.
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
