# Hydration outcome (Issue 383 D2b) — proposed design

Status: awaiting Ferris's design decision. No implementation is authorized by
this document. D2a trust preflight is separate and already in progress.

## Meaning and evidence

Feira confirmed #402's vocabulary: Delivered means vendor-queued but unacknowledged;
Acked means direct active-turn acceptance or explicit agent read/ack. Both are
resolved for push retry purposes, so queued input is not sent again.

Add `hydration_status` with `confirmed`, `unconfirmed`, and `failed`. Retain
`hydration_dispatched`, true exactly when the status is confirmed. Process status
and runtime ownership remain separate. Legacy records with no acceptance evidence
are unconfirmed even when their old boolean was true.

- App-server acceptance of this exact submission on its pinned native route can
  confirm it. This is acceptance, not proof of completion or correct hydration.
- Argv delivery, a live owned process, PTY echo, a discovered route, idle status,
  transport success and vendor queue success are unconfirmed.
- A known pre-delivery refusal or launch failure is failed. An ambiguous timeout
  is unconfirmed. Neither unconfirmed nor failed automatically resends a prompt.
- Preserve ordinary delivery intent. Do not escalate to OwnerSendNow to obtain
  stronger evidence. Preserve existing safe retry only for proven non-delivery.

## Source constraints

Spin-up and restart currently turn argv plus process survival into true
(`hub/lifecycle.rs:3558`, `:4943`). The delivery helper also erases transport
detail into a boolean (`:3268`).

New Codex launches have no persisted native thread ID (`:3472`), so existing
`probe_codex_thread_readiness` cannot establish acceptance of their initial prompt.
Wire hooks acknowledge Wire inbox messages, not the initial argv prompt.

The existing operator outcome distinguishes AppServerAccepted, VendorQueued and
TransportAccepted (`wire/dispatcher.rs:211`). On current main, ordinary Codex
delivery queues; direct acceptance is reached through explicit OwnerSendNow.
Therefore typed outcomes alone do not confirm normal fresh launches.

## Approaches for Ferris's decision

**A. Small honest-status PR (recommended within the requested small PR scope).**
Replace false positives with the tri-state and preserve available typed evidence.
Fresh argv launches return unconfirmed; a trust-dialog process and a delayed
startup remain unconfirmed without proof. Definite failures return failed. Do not
poll vendor APIs or resend. This fixes the truthfulness defect but deliberately
does not promise eventual confirmation for every new session. Tests cover alive
dialogs, delayed starts, definite exit/refusal, ambiguous delivery and no resend.

**B. Confirm ordinary fresh launches with explicit per-launch acknowledgement.**
Persist a unique generation token before launch and include an explicit generic
`fl agent hydration-ack` instruction in that prompt. An acknowledgement writes a
separate receipt keyed by session and generation; it never rewrites the hosted
session record. Read paths project confirmation only from a receipt for the
current generation. Use a bounded check; later reads can confirm a delayed ack
without resending or adding a background supervisor.

This is a moderate change: public CLI command, durable receipts, projection,
generation fencing and concurrency tests. Whole-record last-writer persistence
(`hub/lifecycle.rs:1954`) has no CAS (`core/state.rs:435`); a callback that rewrites
the record can lose its update or restore stale lifecycle fields. Separate
receipts avoid that race, but concurrent lifecycle completion still needs a
defined cross-process fence before this option is implementable. A hub-only mutex
is insufficient. Stale-generation and early/delayed/duplicate acknowledgement
tests are required. A receipt proves the agent handled the acknowledgement
instruction, not that it completed the rest of hydration.

## Decision requested

Approve A as the small D2b truthfulness fix, with fresh launches explicitly
unconfirmed until a later acceptance mechanism is designed; or authorize B's
larger acknowledgement/fencing scope. Do not build either until Ferris chooses.
No live launches, service changes or trust-config writes are part of this work.
