# Hydration outcome (Issue 383 D2b) — approved design

Status: Ferris approved the small truthful-status scope on 2026-11-09, with the
compatibility correction below. Implement after D2a, as a separate PR.

## Meaning and evidence

Feira confirmed #402's vocabulary: Delivered means vendor-queued but unacknowledged;
Acked means direct active-turn acceptance or explicit agent read/ack. Both are
resolved for push retry purposes, so queued input is not sent again.

Add `hydration_status` with `confirmed`, `unconfirmed`, and `failed`, with a serde
default of unconfirmed. Preserve `hydration_dispatched` and document its existing
meaning: the prompt was handed to the vendor. Do not redefine that boolean as
confirmed or turn nearly every fresh launch into a legacy UI failure. Process
status and runtime ownership remain separate. Legacy records without acceptance
evidence have unconfirmed status even when their old boolean was true. D2a prevents
the trust-dialog false success at its source.

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

## Approaches and decision

**A. Small honest-status PR (approved, with the boolean preserved as above).**
Replace false positives with the tri-state and preserve available typed evidence.
Fresh argv launches return unconfirmed; a trust-dialog process and a delayed
startup remain unconfirmed without proof. Definite failures return failed. Do not
poll vendor APIs or resend. This fixes the truthfulness defect but deliberately
does not promise eventual confirmation for every new session. Tests cover alive
dialogs, delayed starts, definite exit/refusal, ambiguous delivery and no resend.

**B. Explicit per-launch acknowledgement (deferred; do not implement here).**
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

## Delivery boundary

Implement A after D2a. Keep the new field additive and retain the existing boolean
behavior for compatibility. Link existing follow-up
[#406](https://github.com/Artificial-Humanity/FerroLoop/issues/406) for eventual
confirmation; do not create a duplicate. Evaluate registration from the launched
session, fenced by launch generation, as one candidate signal; route discovery
alone is insufficient. Do
not add the callback, fencing mechanism or new acknowledgement command here.
Felix owns FerroDeck presentation after this PR merges. No live launches, service
changes or trust-config writes are part of this work.

## Implemented interfaces

`HydrationStatus::{Confirmed, Unconfirmed, Failed}` uses serde snake-case values
and defaults to `Unconfirmed`. `HostedSessionRecord`, `SpinUpResponse` and
`RemodelResponse` carry a defaulted `hydration_status: HydrationStatus` immediately
after `hydration_dispatched`; `RestartResponse` exposes it through its flattened
record. Missing legacy fields never derive confirmation from the old boolean.

The private `HydrationOutcome { dispatched: bool, status: HydrationStatus }`
keeps handoff and acceptance separate. Typed dispatch retains `NotConnected` for
safe rediscovery and classifies `Failed` as ambiguous. Prepare-only remodels and
adopted records have no hydration acceptance evidence and remain unconfirmed;
an attempted hydration with no route is failed. Restart and remodel execution
replace earlier confirmation with the current submission's evidence.

Human CLI output appends `hydration_status=<value>` after the existing fields.
Private-host black-box fixtures cover both an interactive dialog and delayed
initialization, including progression after release without an acknowledgement.
Ordinary fresh launches remain unconfirmed until the signal tracked by #406 exists.
