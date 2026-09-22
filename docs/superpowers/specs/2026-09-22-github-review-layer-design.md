# The GitHub review layer — design

**Date:** 2026-09-22
**Status:** Roadmap. Designed ahead of need; not scheduled, not started.
**Scope:** An optional layer that runs FerroLoop's gates on a pull request and
records external instrument output to a review surface.

---

## Reading this document

Nothing here is a commitment to build. The owner's framing: *"this is a roadmap
item, not an immediate add but it still would pay to have it figured out ahead."*

Constraints carry a status tag, because a stance held for a first release is not
the same thing as a rule that must always hold:

| tag | meaning |
|---|---|
| **Invariant** | Must hold in any version. Violating it breaks the product's premise. |
| **Release scope** | True of the first release. A later version may change it. |
| **Open** | Undecided. Named here so it is not decided by accident. |

An untagged statement is descriptive, not binding.

---

## 1. Purpose and framing

FerroLoop today gates a local action: a gate names a population, examines it, and
refuses when it cannot justify a pass. That loop is **Dev Testing** — where agentic
developers and agentic gates work, and where most defects should die.

This layer is the ring outside it: **UAT/QA — the wider safety net.** It exists for
two things the local loop cannot provide on its own:

1. **Independent execution.** Gates re-run on hardware the developer does not
   control, from a definition the developer does not solely own.
2. **Visibility beyond server access.** Product owners, development leadership and
   QA staff read results without a shell on anyone's machine.

For a solo developer, the GitHub surface is likely read *more* than the local
tracker. The local tracker is agent-majority but not agent-only.

**The whole feature is an optional lever.** *(Invariant)* A project that never turns
it on behaves exactly as it does today. Nothing in the local protocol depends on
this layer existing.

### Non-goals

- Replacing the local loop. This is an addition or an alternative, never a migration.
- Executing a fork's pull request on the user's hardware. *(Invariant)*
- Two-way synchronization between GitHub and the local store. *(Release scope)*
- Hosting anything. *(Release scope — see §2.7)*

---

## 2. Architecture and components

Seven components. Five exist at first release.

| # | component | lives | first release |
|---|---|---|---|
| 1 | Acceptance manifest | committed in the repo | yes |
| 2 | `fl` in CI | GitHub Actions runner | yes |
| 3 | Source adapters | `fl-adapters` | yes (2) |
| 4 | Router | `fl-adapters`, driven by `fl` | yes |
| 5 | Sink adapters | `fl-adapters` | yes (1) |
| 6 | Local reconciler | `fl` on the dev machine | yes |
| 7 | The App | hosted | no |

### 2.1 Acceptance manifest

The gate definitions the CI side runs, exported from a store into a committed file
carrying a **provenance stamp**: source project, commit, export time.

The store remains the authoring authority. The manifest is an export, not a second
place to author gates.

`CODEOWNERS` pins the manifest path, so separation of duties is **enforced rather
than conventional** — the developer whose work is being gated cannot quietly widen
the gate in the same pull request. *(Invariant, where the acceptance set is separate
from the local set.)*

**The workflow refuses on a stale stamp.** FerroLoop's own staleness rule, applied
to itself. A manifest that no longer matches its source is not a weaker gate; it is
an unknown one.

### 2.2 `fl` in CI

The same binary and the same gate machinery as local execution. It resolves the
manifest, enumerates populations, runs gates, and emits a verdict document.

There is deliberately no second implementation of gate evaluation. That is the
reason the App can be deferred rather than being the centre of the design.

### 2.3 Adapters — one contract, two bindings

An adapter is an **interface and a contract**. That does not preclude it from being
an executable, and for third parties it usually will be.

- **In-process binding:** a Rust trait implementation, compiled into `fl-adapters`.
  Used by the adapters FerroLoop ships.
- **Out-of-process binding:** a program exchanging the same versioned JSON schema
  over stdio. Used by anyone else.

The schema is the artifact that is documented and versioned. Neither binding is
privileged: the in-process adapters are run against the same conformance suite as
external ones (§6), so the contract is exercised by our own code rather than
routed around it.

This mirrors how a FerroLoop gate already works — point it at a program, hand it a
population, read its result, never parse its internals.

**Requirement:** the contract must be authorable by third parties without FerroLoop
writing each adapter. *(Invariant.)* Owner's framing: *"Adapters allow us to
potentially support a wider range of products, hopefully without us having to even
author those adapters, ourselves."*

Wire format is `snake_case` JSON, per ratified decision 33. *(Invariant.)*

### 2.4 Source adapters (intake)

Normalize an external instrument's output into **observations** (§3). Two ship:

- **CI-native reader** — a workflow step writes its native report to disk; the
  adapter reads it.
- **GitHub events reader** — `fl` polls the API for check-run conclusions and
  annotations.

### 2.5 Router

Takes normalized observations and decides their fate: deduplicate, assign an
idempotency key, apply policy, produce sink actions.

At first release the router runs **inside `fl` in CI**. *(Release scope.)* Nothing
is hosted. When the App arrives it hosts this same router — the component does not
move, only its address does.

### 2.6 Sink adapters (review surfaces)

A `publish`/`poll` pair, either half optional. **GitHub Issues is the only adapter
planned at first release.** *(Release scope.)*

Other destinations — Sonar and its peers are the obvious examples — are the reason
the destination is adapter-defined. They are not targets for the first release.
They are the argument for the boundary.

### 2.7 The App

Not built at first release. Timing rule: **the workflow comes first; the App arrives
when an instrument genuinely needs it.** A GitHub-installed App makes orchestration
simpler to provide and gives repository workflows a place to reach for instruments
we have not written yet. It is not a precondition for the layer to be useful.

When the App exists, a plausible first job is receiving review findings by webhook
and creating entries in the adapted target or targets of choice.

**No webhook at first release** *(Release scope)* — there is nothing hosted to
receive one. This is a consequence of not hosting, not a position against webhooks.

**The local side polls rather than listens.** *(Release scope.)* Owner's stance for
the release of this feature, explicitly not a maxim.

---

## 3. Data flow

Everything crossing a boundary is `snake_case` JSON. Every record carries
provenance: commit sha, run id, adapter id, adapter version.

### On the pull request

1. Workflow triggers. `fl` reads the acceptance manifest and checks its provenance
   stamp. Stale → refuse, non-zero exit, message naming what drifted. Nothing else
   runs.
2. `fl` enumerates each gate's population. Empty → **FAIL**. Lister failed →
   **ERROR**, not empty. Both are existing invariants and are unchanged here.
3. Gates run. `fl` writes a **verdict document**.
4. Independently, other CI steps — dependency scanning, SAST, linters — write their
   native reports to disk.
5. **Intake:** source adapters read those reports; the GitHub events adapter polls
   check-run conclusions. Both normalize to observations.
6. **Router:** deduplicate by idempotency key, apply policy, produce sink actions.
7. **Sink:** publish or update GitHub Issues. The verdict posts as the pull request
   check.
8. A human reviews and merges — or does not.

### Later, on the developer's machine

9. `fl` polls the sink. External state — closed, relabelled, commented — reconciles
   into the local store against the observation's key.

### 3.1 Observations are not findings

*(Invariant.)*

A **finding** is a protocol object: it requires a falsifiable reproduction, and it
is not actionable until it carries one. A dependency-scanner hit has no
reproduction — it is an assertion by an instrument.

These are **separate types**, so the empty-population rule and the finding protocol
cannot be diluted by external imports, and the compiler enforces the separation
rather than a convention.

Whether an observation can ever be **promoted** into a finding is **Open**. Promotion
would have to mint a reproduction, which is the correct price for the promotion.

### 3.2 Gates gate; observations record

*(Invariant.)*

A verdict blocks the merge. An observation does not — it has no reproduction, and a
claim without a reproduction cannot block. This is the rule the finding protocol
already enforces locally, applied at the outer ring.

Policy may promote specific observation classes to blocking. The default is
record-only, and the promotion is an explicit act.

### 3.3 Gate evaluation and intake are independent

Steps 3 and 5–7 do not depend on each other. **A red gate must not suppress the
publication of a security observation.** Failing the check does not abort intake.

### 3.4 Direction

Pull everywhere except the sink write. Intake is CI-local file reads plus polling;
reconciliation is polling. The only outbound push is the sink publishing into
GitHub Issues, which is unavoidable — recording *is* a write.

### 3.5 Reconciliation is one-way

*(Release scope.)*

GitHub state flows down into the local store. Local state does not flow up. Two-way
synchronization needs conflict resolution, and there is no evidence yet about which
direction should win. Deferring it costs nothing and buys the evidence.

### 3.6 Which gates run on the GitHub side

**Open**, and configurable by the end user: re-run the same set the local loop runs,
run a separate acceptance set, or both. The expected default is a **separate set,
owned separately** — that is what makes §2.1's separation of duties meaningful.

---

## 4. Error handling

One principle carries almost all of it:

> **Every failure path must distinguish "nothing" from "didn't look."**

That is the empty-population rule, generalized from gates to intake and recording.
*(Invariant.)*

### 4.1 Intake failures

The design is most likely to lie here. If a SAST step crashes and writes no report,
a naive adapter reads an empty file and reports zero observations — and the pull
request looks clean *because* the scanner died.

- A source declared in the manifest that produced no report is an **error**, not
  zero observations.
- An adapter that cannot parse its input is an **error**, not a partial read.
- A subprocess adapter that exits non-zero, times out, or emits malformed output is
  an **error**. Non-zero never means empty.

### 4.2 The blocking line

An observation does not block a merge. A **failure to collect or record**
observations **does** block, because the safety net cannot be claimed to have run.

"The scanner found nothing" and "the scanner did not run" must never land in the
same bucket, and only one of them is allowed to be quiet.

### 4.3 Sink failures

Publication is partial by nature; rate limits and network faults land mid-batch.

- The idempotency key makes retry safe, so retry is the first response.
- A partial publish reports `published 4 of 7`, never success. The count is the
  postcondition; the absence of an error is not.

### 4.4 The idempotency key

Derived from source id, rule id, path, and a normalized snippet. It travels in the
sink record so a republish finds the existing item.

**It must not include a line number.** *(Invariant.)* Lines move on every rebase; a
key containing one opens a fresh issue on every push and floods a human's inbox.
This is the one place the design can silently corrupt something a person depends on.

### 4.5 Reconciliation failures

A failed poll must not be read as "the issue is closed." Absence of data is not data
saying absent. On poll failure the local record keeps its last known state and is
marked stale. A stale record is honest; a silently reverted one is not.

### 4.6 Version skew

An out-of-process adapter announces its contract version at handshake. A mismatch
refuses. There is no best-effort parsing of an unrecognized schema — that is the
path where an unknown field is silently dropped and a current install returns
success on something it discarded.

### 4.7 Duplicates

Two sources reporting the same defect under different keys produce two issues. This
is accepted. A duplicate is visible and cheap to close; a swallowed observation is
neither.

### 4.8 No durable queue

*(Release scope.)* If the sink is unreachable, the run fails loudly and is re-run. A
queue means hosting, retry state, and ordering guarantees — which the App may
justify later, and which a first release does not earn.

### 4.9 Trust and capabilities

An out-of-process adapter running in GitHub Actions runs in GitHub's sandbox. The
same adapter running inside a hosted App would run on our infrastructure. That is a
different trust question.

No App exists, so nothing is blocked today — but **the contract carries a declared
capability set from the first version**, so it is not retrofitted under pressure
later.

---

## 5. Security and disclosure

- A fork's pull request never executes on the user's hardware. *(Invariant.)*
- The manifest path is `CODEOWNERS`-pinned; widening a gate is a reviewable act.
- Adapter credentials are passed by environment, never in argv, which is
  process-table readable.

---

## 6. Testing

### 6.1 The conformance suite is the contract

If third parties author adapters, prose cannot be the specification — it drifts the
moment behaviour changes, and nobody discovers the drift until an adapter silently
misbehaves.

Ship `fl adapter verify <adapter>`: an executable conformance runner that any
adapter is run against, ours or a stranger's. This is the same realization the
finding protocol arrived at, where a reproduction and a gate turned out to be one
object. **The documented contract and the test that enforces it are one artifact.**

FerroLoop's own adapters pass it in CI alongside everyone else's. An in-process
adapter with a privileged path is an adapter whose contract is never tested.

### 6.2 The "didn't look" battery

Each §4 rule gets a reproduction, and each is mutation-tested by deleting the guard
and watching the test go red.

| scenario | required outcome |
|---|---|
| declared source wrote no report | ERROR |
| report file is malformed | ERROR |
| adapter exits non-zero | ERROR |
| adapter hangs past timeout | ERROR |
| adapter announces unknown contract version | refuse |
| adapter writes a valid, genuinely empty report | zero observations, clean |

The last row makes the other six meaningful. Without it, the battery passes against
an adapter that errors unconditionally.

### 6.3 Idempotency, tested where it breaks

Publishing twice and asserting one issue is trivially passable. The real test is:
publish → rebase so every line number moves → publish again → assert still one
issue. That is the case that would flood a human's inbox, so that is the case the
test must cover.

### 6.4 Fakes prove structure, not integration

A fake sink agreeing with itself says nothing about GitHub's API. Three layers:

1. Fixture-driven source adapter tests, against captured real instrument output.
2. A fake sink, for router and reconciler logic.
3. A small **opt-in live smoke test** against a throwaway repository. It runs
   rarely. It exists because the first two cannot fail in the way GitHub actually
   fails.

### 6.5 Manifest staleness

Export the manifest, change the store, assert CI refuses. The refusal is the
feature, so it needs a test that fails when the refusal is removed.

### 6.6 Dogfooding is the acceptance test

FerroLoop's own repository runs this layer against its own pull requests. Same
precedent as making the getting-started guide an executed test rather than a
document we trust: the system's claim about itself has to be something that can go
red.

---

## 7. Open questions

Named so they are not decided by accident.

1. **Promotion.** Can an observation ever become a protocol finding, and what mints
   its reproduction? (§3.1)
2. **The App.** Does it gain hosting and event reception, and what is the trigger
   for building it? (§2.7)
3. **Gate set.** Same set, separate acceptance set, or both — and what is shipped as
   the default? (§3.6)
4. **Policy language.** How a project declares that a given observation class blocks.
   (§3.2)
5. **Adapter distribution.** How a third-party adapter is discovered, pinned, and
   version-checked by a project that wants to use it.
