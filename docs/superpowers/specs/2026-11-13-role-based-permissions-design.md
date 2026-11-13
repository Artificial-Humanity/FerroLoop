# Record #191 — Role-based permissions

Draft: Ferris (Lead). Extends [Record #103](2026-11-03-universal-permissions-design.md); read
that spec first, this one does not repeat it.
Evidence: code at main `361e75f7`, 2026-11-13; installed CLI `--help` output on ai-lab-0; the
incidents of 2026-11-13 named in §0. Each vendor claim says whether it is code-verified,
help-text evidence, or unmeasured.
Design only; authorizes no change to a live session, a settings file or a role file.

## 0. The problem

Role authority is written in prose (`.agents/roles/*.md`, `WORKFLOW.md`). No software applies
it. The vendor permission layers decide instead, and they do not know the roles. Two errors
follow, and both occurred on 2026-11-13:

- **A permitted action is refused.** A Lead's `gh issue create` for a defect in its own project
  was refused by the Claude Code auto-mode classifier. `WORKFLOW.md` documents the same
  refusal for `gh pr merge` of an approved own pull request. A Lead and the Lab Manager could
  not reforge a subordinate's session at the owner's request: the hub route is Owner-only.
- **An action outside the role is not stopped.** A raw `gh pr merge`, `gh issue create` or
  `git push` passes through no FerroLoop check. Only the GitHub ruleset is a backstop.

What exists today (code-verified):

| Part | State |
|---|---|
| `fl_core::permissions` compiler (posture, sandbox, ≤256 capability rules, five vendors) | Built. Only `compile_posture` has a production caller; `compile()` has none. |
| Rule file writers for `CompiledPlan.config_files` | Not built. Targets are symbolic. |
| Spin-up API | Takes `posture` only (default `auto` for every role). FerroDeck sends `permission_policy`; `SpinUpRequest` drops it silently. |
| `RoleRecord` | `slug, label, description, builtin, review_policy, prompt_template`. No permission field. |
| PR authority (`fl_core::pr_policy`) | Enforced on `fl github pr merge` and the hub merge route only. The caller is the self-asserted `--by` / `X-Fl-Agent`. |
| Hooks | `fl wire hook` handles `Stop`, `SessionStart`, `UserPromptSubmit`, `PostToolUse`. No `PreToolUse` or `PermissionRequest` logic. Hooks are registered by hand at user level. |
| Hand-written allow rules | `~/.claude/settings.json` allows `gh pr merge`, `gh issue create`, `gh issue comment`, `fl github pr review` for every Claude agent, whatever its role. |

## 1. Owner decisions (2026-11-13)

1. **Approved.** The role is the default source of an agent's permission policy at spin-up.
   The owner may override it for one agent.
2. **Approved.** GitHub writes go only through the `fl github` broker. Raw `gh` writes are
   denied for agents; reads stay allowed.
3. **Approved.** Protection against mistakes is sufficient for now. Per-agent isolation is a
   separate roadmap record (Record #192) with its own design.
4. **Approved.** The Lead drafts one permission profile per role; no profile is applied before
   the owner approves it.
5. **Review chain.** The Project Manager has pull-request review authority. An author asks
   the lowest available tier above it first: Project Lead, then Project Manager, then
   Director. The next tier is asked only when the lower one is offline or unavailable. The
   same rule applies to those roles as authors: a Project Lead asks its Project Manager before
   the Director. System-level changes stay with the Director.
6. **Council.** A Project Manager may petition The Council. A Project Lead may still petition
   directly and need not go through its Project Manager.

Owner direction of the same day, recorded as Record #193 and used for the names below: the hierarchy
is **Director** (was Lab Manager) → **Project Manager** (was Project Director) → **Project
Lead** → the other project roles; Council roles stay **Council Chair** → **Strategist**; the
`custom:` prefix is removed from role names.

## 2. Model

### 2.1 Tier is a field, not a name

Structural authority must not hang on a role's spelling. `RoleRecord` gains a `tier`:

| Tier | Roles today | Reports to |
|---|---|---|
| `director` | Director (one per workspace) | Owner |
| `project-manager` | Project Manager of a parent project | Owner |
| `lead` | Project Lead | Owner, and the Project Manager for portfolio matters |
| `member` | assistant, ui-developer, strategist, journalist, every specialised or user-defined role | the project's Lead |
| `council-chair` | Council Chair | Owner |
| `council-member` | Council Strategist | Council Chair |

A user-defined role is an ordinary `RoleRecord` with tier `member` unless the owner sets
another tier. The wire form of every role is its bare slug. `custom:<slug>` is never written.

Removing the prefix is a change of spelling only. It never changes an agent's tier, review
authority or profile, and a user-defined role keeps its own role record. The bare slug cannot
always stand for the prefixed one: today `AgentRole::from_wire` keeps `custom:lead`,
`custom:assistant`, `custom:lab_manager` and `custom:lab-manager` as custom roles, distinct
from the structural built-ins, while `custom:<other built-in>` already reads as that built-in
(code-verified). Two rules follow:

- **Structural names are reserved.** They are `lead`, `assistant`, `director`,
  `project-manager`, `council-chair` and the legacy spellings `lab-manager` and `lab_manager`.
  Creating a user-defined role with one of these slugs is refused. The migration does not
  strip the prefix from such a role by itself (§7.4).
- **`custom:<slug>` on input** is accepted after the migration only as a spelling of a
  non-structural role. `custom:lead` and the like are refused with a message that names the
  role, the bare structural slug and the Owner mapping of §7.4. They are never read as the
  structural role.

### 2.1.1 Review chain

The reviewer of a pull request is the first *available* agent in the chain above its author:

| Author tier | Chain |
|---|---|
| `member` | Lead → Project Manager → Director → Owner |
| `lead` | Project Manager → Director → Owner |
| `project-manager` | Director → Owner |
| `director` | Owner |

"Available" keeps today's meaning: an active registry record with a live route that answers a
review request within 15 minutes. A tier with no agent for the project (most projects have no
Project Manager) is skipped at once, not waited for. The Project Manager of a project is an
agent of tier `project-manager` whose registered areas include that project; this uses the
extra areas a manager already registers and needs no parent link between projects. A
co-author is never its own reviewer, and the Owner-tier triggers of `pr_policy` (protected
paths, missing trailers, instruction files) are unchanged and come before the chain.
System-level changes (the workspace, the lab host, shared services) stay with the Director:
the chain does not move them to a Project Manager.

`fl_core::pr_policy` gains the `project-manager` tier between `lead` and `director`, and
`fl github pr create` reports the whole chain with the first available reviewer as the
target. An approval from any tier at or above the target counts, as an approval from a higher
tier does today.

### 2.2 Two kinds of grant

A role profile holds two lists. They answer different questions and are enforced in different
places. It also holds the `defaults` map described after the example.

- **Tool rules** — the canonical capability rules of #103 §2 (`fs_*`, `shell_exec`,
  `network_web`, `mcp_tool_call`) plus `posture` and `sandbox`. They say what the vendor tool
  layer may run. They are compiled per vendor.
- **Action grants** — named FerroLoop actions with a scope. They say what the role may ask a
  broker to do. A tool rule cannot express "merge your own approved pull request"; an action
  grant can, because the broker knows the pull request, its approvals and the caller.

```json
{
  "schema_version": 1,
  "tier": "lead",
  "policy": { "posture": "auto", "sandbox": "vendor-default", "rules": [ … ] },
  "defaults": {
    "fs_read_workspace": "ask",  "fs_read_external": "ask",
    "fs_write_workspace": "ask", "fs_write_external": "ask",
    "fs_write_protected": "deny",
    "shell_exec": "ask", "network_web": "ask", "mcp_tool_call": "ask"
  },
  "actions": [
    {"action": "github.issue.create",  "scope": "own-project"},
    {"action": "github.pr.review",     "scope": "own-team"},
    {"action": "github.pr.merge",      "scope": "own-approved"},
    {"action": "session.reforge",      "scope": "own-team", "effect": "ask-owner"}
  ]
}
```

`policy` is exactly the #103 `PermissionPolicy`; this spec adds no capability and does not
change that object.

`defaults` belongs to the profile, not to `policy`. It is a required map with one effect
(`allow`, `ask` or `deny`) for each of the eight capabilities. The effect applies to a call of
that capability that matches no rule, whether or not the capability has rules (§3.3). A
profile without a complete map is refused; a missing entry is never filled in. The map is part
of the profile that the owner approves (§1 decision 4, §6), and the values above only show the
form. It is layered as §2.3 layers rules: a lower layer may tighten an entry freely, and
widening one needs Owner authority.

Action names
form a closed enum in `fl-core`, generated into docs (capability truth lives in code). Scopes:
`own-project`, `own-team` (agents whose registry `reports_to` is the caller), `own-approved`
(the caller opened it and the required reviewer approved the current head), `portfolio` (a
Project Manager's sub-projects), `workspace`. Effects: `allow`, `deny`, `ask-owner` (the broker
needs a matching warrant, Record #64, and consumes it).

### 2.3 Effective policy

`system defaults → project → role profile → agent override`. Precedence inside the result is
deny > ask > allow, as in #103. A lower layer may tighten freely. A layer that widens what the
role profile allows needs Owner authority (an Owner session or a warrant with scope
`permissions.widen`); the hub refuses it otherwise and says which rule widened. The effective
policy and its compile receipt are stored on the hosted record and shown in FerroDeck.

## 3. Enforcement points

### 3.1 The broker

`fl github` becomes the only way an agent writes to GitHub. It already brokers review and
merge. It gains `issue create|comment|edit|close`, labels, assignees and review requests,
`pr create|edit|merge`, each mapped to one action name. For each call the broker:

1. resolves the caller (below),
2. loads the caller's effective action grants,
3. checks the action and scope against facts it re-reads from GitHub (never from the caller),
4. performs the write with the App or the author token, and journals caller, action, target
   and decision.

A refusal is actionable: it names the action, the caller's role, and who can do it instead.

**Caller identity.** `--by` and `X-Fl-Agent` are self-asserted today. The broker must not
trust them. A brokered command connects to the hub's Unix socket; the hub reads the peer
credentials (`SO_PEERCRED` pid on Linux, the equivalent per platform), walks the process
ancestry to a process that `fl-host` launched, and takes persona and role from that hosted
record. `--by` stays as an optional cross-check that must agree. A caller that is not under a
hosted session is refused for role-gated actions unless it holds Owner authority. This removes
the shared-peer-token ambiguity for brokered actions without issuing per-agent secrets.

**Limit (stated to the owner, decision 3).** Every agent runs as the same Unix user. An agent
that deliberately evades the broker can read the App key or the machine token. This design
stops mistakes and makes role limits uniform; it is not a boundary against a hostile agent.
That boundary is Record #192.

The same broker pattern later carries session lifecycle actions (`session.clear`,
`session.reforge`, `agent.remodel`) so that a role grant, not the Owner-only route alone, can
authorize them. That is in scope for the action enum now and for implementation after the
GitHub actions.

### 3.2 Compiled vendor rules, per session

The hub accepts `permission_policy` at spin-up (unknown fields refused), defaults it from the
role profile, runs `compile()`, and applies the plan. Two corrections to the current symbolic
targets:

- **Rules go to the session, not to a shared project file.** Several agents of different
  roles share one project directory, so `.claude/settings.local.json` cannot carry role rules.
  Claude: a per-session settings file passed with `--settings <file>` (flag present in the
  installed CLI help; load order against user and project settings is unmeasured, §4). Codex:
  `-c key=value` overrides or a per-session profile (flags present in help; unmeasured). Muse,
  Grok, Antigravity: per #103 §1; where no per-session target exists the capability is
  launch-fixed and compiled to the stricter form, with the downgrade in the receipt.
- **Raw `gh` writes are denied by rule.** The role profiles carry `shell_exec` deny rules for
  the `gh` write subcommands and for `gh api` with a write method, and an allow rule for
  `fl github`. The hand-written user-level allow rules are removed once the broker covers
  those commands.

### 3.3 The tool gate

`fl wire hook` gains `PreToolUse` (and `PermissionRequest` where the vendor has it). The gate
evaluates the caller's complete effective policy (§2.3).

**The gate is total.** The canonical default of #103 §2 covers only a capability that no rule
names (the compiler's `omitted()`, code-verified). It gives no outcome for a call of a
capability that has rules and matches none of them, and every role profile has `shell_exec`
rules (§3.2): a shell command that is neither a raw `gh` write nor `fl github` matches no
rule. The `defaults` map of the role profile (§2.2) closes this. The gate uses the map's
effect for a call of a capability that matches no rule, whether or not the capability has
rules. Every call therefore gets one of three outcomes. There is no "no rule" outcome, and the
gate never hands an undecided call to the vendor layer.

The map relates to #103 as follows:

- `policy` stays exactly the #103 `PermissionPolicy`, and the #103 canonical default stays the
  floor. For a capability with no rules, the entry must not be more permissive than that
  default (deny for `fs_write_protected`, ask for the others; code-verified). A profile that
  breaks this is refused.
- For a capability with rules, the entry is the owner's choice, approved with the profile
  (§1 decision 4, §6).
- The compiler receives the map with the policy. It compiles an entry where the vendor can
  express a default for that capability. An `allow` entry that the vendor cannot express is
  reported in the receipt as gate-only; like an allow rule, it never stops the vendor layer
  from tightening. A `deny` or `ask` entry is a restriction and follows the rule below: a
  native form, or a refused plan.

The three outcomes:

- **deny** → the hook blocks the call and returns the reason and, where one exists, the broker
  command to use instead;
- **ask** → the gate never allows the call by itself. The call goes to a real prompt channel:
  the vendor's own prompt, or a FerroDeck channel on a transport where #103 §3 proves one.
  Where no prompt channel is proven for that vendor, transport and posture (M7), ask is
  treated as deny (#103 §2: a deny never degrades to ask, and an ask with no prompt is a run);
- **allow** → the hook pre-answers only where Phase 0 (M1, M2) proves that the vendor honours
  it. Elsewhere the gate does not answer for the vendor: the vendor layer may still prompt or
  refuse, which can only tighten the result.

It is a local policy engine with a bounded timeout: pre-answer by rule, not remote resolution
(#103 §3). On a timeout or a gate error (the effective policy is missing or unreadable, or
the call cannot be classified) the result is deny, for every call. This is the #103 rule:
bounded timeout, deny on timeout.

**The gate is never the sole control for a restriction.** Every deny and every ask in the
effective policy, the `defaults` entries included, must have a compiled native restriction
(§3.2) that is at least as strict:

- a deny has a native deny;
- an ask has a native ask where the vendor has one and a prompt channel is proven for that
  vendor, transport and posture (M7). Otherwise it has a native deny.

A plan that cannot give a restriction such a native form is refused at spin-up. This is the
#103 §2 rule: an unsupported deny must remain denied through a proven native restriction or
the plan is refused, and an ask holds only through a proven interactive channel. Posture
follows the same rule. A profile whose restrictions cannot be expressed natively under the
requested posture is refused or compiled to the stricter form. It is never applied with the
gate as the only control.

On top of the native rules the gate adds what they do not give: the reason, the broker hint,
the journal, and the pre-answer of allowed calls.

Fallback never widens. No path through the gate yields a more permissive result than the
canonical policy: the gate can refuse a call or leave it to a layer that can only tighten,
and it cannot turn a deny or an ask into a run. If the hook returns no decision at all (the
hook process crashes, or the vendor's own hook timeout fires), the native rules decide, so
the result is never more permissive than the canonical policy. That a vendor applies its
native rules in that case is vendor behaviour and is unmeasured (M8). Until M8 proves it for a
vendor, the gate hook is not installed for that vendor, and the compiled rules of §3.2 work
alone.

Every decision is journaled. FerroLoop installs the hook through the same per-session
settings as §3.2; the hand-registered user-level entries are retired with it.

### 3.4 What is not enforcement

Prompts, role prose, hydration text and compile receipts instruct or report. None of them is
counted as a control in this design.

## 4. Phase 0 — measure before building §3.2 and §3.3

Three facts decide how much §3.2 and §3.3 can deliver, and each is unmeasured for every
vendor: which mechanism takes priority over the vendor's own reviewer (M1 to M6), whether an
ask reaches a prompt that an operator can answer (M7), and what the vendor does with a call
when a hook returns no decision (M8). The only observation so far concerns the first fact and
is not conclusive: with `Bash(gh issue create:*)` allowed at user level and posture `auto`, a
compound command ending in `gh issue create` was refused by the Claude classifier, and a
later one succeeded after the owner granted permission in the conversation.

For each vendor and transport, on a disposable session and a disposable repository, record as
runtime-proven rows in `docs/permissions.md`:

| # | Question |
|---|---|
| M1 | Does an explicit allow rule run without the vendor reviewer (Claude `auto` classifier, Codex and Muse approval reviewer) judging it? |
| M2 | Does a hook `allow` decision do so? |
| M3 | Does a deny rule hold under `auto`, and under `bypass`? |
| M4 | Do prefix rules match a compound command (`cd x && gh …`, pipes, `env A=B gh …`)? |
| M5 | Are per-session settings loaded, in what order against user and project settings, and are edits picked up live? |
| M6 | Can the session itself rewrite the per-session settings or hook file, and does the vendor's protected-path handling stop it? |
| M7 | Under each posture, is a call that matches a native ask rule held for a prompt that an operator can answer on this transport (the vendor's own prompt, or a channel of #103 §3), or does it run or fail without one? |
| M8 | When a `PreToolUse` or `PermissionRequest` hook returns no decision (the hook process crashes, or the vendor's own hook timeout fires), does the vendor apply its native rules to the call (a native deny blocks, a native ask prompts), or does the call run? |

§3.1 depends on Phase 0 least: it needs one allow rule (`fl github`) per vendor, and if M1
fails for a vendor the fallback is that vendor's reviewer judging one stable command instead
of every `gh` form.

## 5. Phases

| Phase | Content | Depends on |
|---|---|---|
| P0 | Measurements M1–M8 on five vendors | — |
| P1 | Role model: `tier`, bare slugs, the Record #193 rename and migration (§7); action enum; role profiles in the store; `fl role` and `/api/v1/roles` read and write them | Owner-approved profiles (§6) |
| P2 | Broker: hosted-caller identity, the new `fl github` commands, action checks, journal; raw `gh` write deny rules through the existing hand-written settings as an interim | P1 |
| P3 | Compiler wiring: `permission_policy` in spin-up, per-session writers, receipts on the hosted record and in FerroDeck (closes Record #160, completes Record #103 §4) | P0, P1 |
| P4 | Tool gate: `PreToolUse` handling, hook install, decision journal, drift check against the applied plan | P0, P3 |
| P5 | Lifecycle actions through the broker (`session.reforge` and the rest) | P2 |
| — | Per-agent isolation | Record #192, separate spec |

Each phase is its own plan and pull request series under `subagent-dev`.

## 6. Draft action matrix (not applied; each profile needs the owner's approval)

Taken from the role files and `WORKFLOW.md`. "Open" cells are not answered by today's rules.

| Action | Member | Lead | Project Manager | Director |
|---|---|---|---|---|
| Open a pull request | allow | allow | allow | allow |
| Approve a member's pull request | deny | allow, own team | allow when the Lead is unavailable | allow when Lead and Project Manager are unavailable |
| Approve a Lead's pull request | deny | deny | allow, portfolio | allow when the Project Manager is unavailable or absent |
| Approve a Project Manager's pull request | deny | deny | deny | allow |
| Merge own approved pull request | allow | allow | allow | allow |
| Merge another agent's pull request | deny | allow when a permission blocks the author | open (O2), for a pull request it approved | same, for a pull request it approved |
| Change instruction files or protected paths | pull request, owner approves | pull request, owner approves | pull request, owner approves | pull request, owner approves |
| File an issue in own project | open (O2) | allow | open (O2) | open (O2) |
| Clear or reforge another agent's session | deny | open (O2), own team | open (O2) | open (O2) |
| Petition The Council | deny | allow | allow | deny (today's rule) |

## 7. Migration (Record #193)

The slug `director` changes meaning. The rewrite must therefore run exactly once, a binary
that still writes the old names must be shut out, and every role value must be read in the
namespace it was written in.

### 7.1 Role namespaces and the epoch

| Meaning | Namespace 1 (before the epoch) | Namespace 2 (at and after the epoch) |
|---|---|---|
| Director tier (was Lab Manager) | `lab_manager`, `lab-manager` | `director` |
| Project Manager tier (was Project Director) | `director` | `project-manager` |
| User-defined role | `custom:<slug>` | `<slug>` |

The epoch is a store format version: a new constant on the `fl_store` format ladder, in the
style of `FORMAT_WITH_WORKFLOW` and `FORMAT_WITH_AREA_DELETE`, called
`FORMAT_WITH_ROLE_NAMESPACE` here. It takes the next free number on the ladder (the newest is
9 at main `361e75f7`). It is durable and it only rises: `raise_format` never lowers a format.
Its job is fencing: a build older than the epoch cannot open the store file (§7.5).

The format number alone does not state the namespace. It has max semantics, and paths other
than the migration raise it (code-verified): an inbound sync batch that carries a
schema-version put on the ladder raises it, and each ladder constant is raised on the first
use of its feature, so a constant added after the epoch would lift a store past the epoch the
same way. A store can then stand at or above the epoch without the rewrite of §7.2.

The statement is a dedicated, durable **role-namespace marker** in the store. This is design
for the P1 plan, which also sets its key and encoding. The migration writes the marker in the
same transaction as the rewrite. The marker, not the ladder number alone, says which namespace
the live role state of a store is in: namespace 2 with the marker, namespace 1 without it. The
live role state is the role value of every registry record and every role record.

Invariant:

- Only the migration writes the marker. Inbound sync never writes it and never applies a
  peer's copy of it.
- A build at the epoch that finds the format at or above the epoch and no marker refuses role
  reads and role writes, the import of role-bearing events included, and names the migration.

An inbound sync batch applies a peer's documents in ordinary collections as they are today
(code-verified). The P1 plan must say what such a batch does when it carries role-bearing
documents to a store without the marker: it must not leave namespace-2 values in a store that
the migration will then rewrite.

### 7.2 The rewrite

One store migration, one transaction. In this order it:

1. reads the role-namespace marker (§7.1). With the marker present it changes nothing and
   reports that. The format number does not decide this: a store at or above the epoch
   without the marker is not yet migrated, so the rewrite runs;
2. checks the `custom:` collisions of §7.4 and refuses, before any write, if one has no Owner
   mapping;
3. rewrites every `director` role value and role record to `project-manager`;
4. then rewrites every `lab-manager` / `lab_manager` value to `director`;
5. strips `custom:` where §7.4 allows it, and makes `council-chair` a built-in role with tier
   `council-chair`;
6. appends a namespace-2 `AgentUpsert` event to the registry event log for every agent record
   that steps 3 to 5 rewrote (§7.3);
7. raises the store format to the epoch and writes the role-namespace marker.

Steps 3 to 7 commit together or not at all. No committed store holds new names without the
marker or old names with it. Step 1 guards on the marker, which only the migration writes and
nothing removes, so the rewrite cannot run a second time on a store: a repeated run is a
no-op, and in particular it does not turn the new `director` into `project-manager`. No agent
holds `director` today, so step 3 is empty in this workspace; it still runs before step 4.

### 7.3 Decoding by namespace

A role value is decoded in the namespace it was written under. The namespace is an explicit
input to the decoder; no reader infers it from the text of the value.

| Input | Namespace 1 | Namespace 2 |
|---|---|---|
| `director` | Project Director: the Project Manager tier, `project-manager` | Director |
| `lab-manager`, `lab_manager` | Lab Manager: the Director tier | `director` (legacy spelling: read, never written) |
| `project-manager` | not a built-in role | Project Manager |
| `custom:<slug>` | as `from_wire` reads it today | a non-structural role only (§2.1) |

- A value with no namespace information is namespace 1. That covers every record, marker and
  message written before the epoch. An unversioned or pre-epoch `director` means Project
  Director and is read as `project-manager`. It never silently acquires the new Director
  meaning.
- `director` means the new Director only in the live role state of a store that holds the
  role-namespace marker (§7.1), and in records, events and messages that state namespace 2.
- Every stored role value is in one of three classes. There is no fourth class.
  - **Live state**: the role value of a registry record or a role record. The migration
    rewrites it in the transaction of §7.2.
  - **History**: posted markers, FerroWire messages and journal entries. It is only read, and
    it is decoded in the namespace it was written in.
  - **Replayable events**: the registry event log. It is not only read:
    `RegistryEventKind::AgentUpsert` carries a whole agent record, the role included, and
    `import_registry_events` writes that record into live state (code-verified). It is
    translated on import, as described below.
- A build at the epoch reads a role that its operator gives it as a command argument as
  namespace 2. A role that arrives from another process is covered by §7.5.

**History is not rewritten.** Posted `fl-review` markers, FerroWire messages (their `@role:`
addresses included) and journal entries keep their text. Each is read in its original
namespace: a pre-epoch `lab_manager` or `lab-manager` is the Director tier, and a pre-epoch
`director` is the Project Manager tier. New markers carry an explicit role-namespace field
(`role_ns: 2`), and so do new messages and journal entries that carry a role. A marker without
the field is namespace 1. Verification decodes the role of a marker under that marker's
namespace and then compares tiers, not spellings.

The marker struct refuses unknown fields today, and a review whose marker does not parse is
left out of the approval fold (code-verified). A build older than the epoch therefore cannot
read a new marker as an old one: it ignores the review. This fails closed for an approval
only: a post-epoch approval does not count for the older build. It does not fail closed for a
rejection. The older build does not see a post-epoch `CHANGES_REQUESTED` review, or a
dismissed review that carries a new marker, so an earlier approval by the same reviewer on
the same head stays in force for it. The supported upgrade order of §7.5 leaves no older build
that verifies approvals; the rollback contract of §7.5 covers the case where one returns.

**Replayable events are translated on import.** The registry event log reaches live state
through `import_registry_events`, which the `fl/state` branch sync and the FerroWire peer
frames call (code-verified: `state_sync.rs`, `daemon.rs`). The branch log is append-only, so
the migration cannot rewrite the events already in it. The rules below are design for the P1
plan:

- Every registry event that a build at the epoch writes carries the role namespace. An event
  without it is namespace 1.
- A build at the epoch translates a namespace-1 event to namespace 2 when it imports it, with
  the mapping of §7.2 and §7.4. It never applies the role of such an event verbatim, and
  never reads it as namespace 2. An event whose role needs an Owner mapping (§7.4) that the
  store does not hold is refused, not applied.
- The translation applies to the live record that the import writes. The stored copy of the
  event keeps its text: the import compares a repeated event with its stored copy today
  (code-verified), and the log stays append-only.
- For every agent record that it rewrites, the migration appends a new namespace-2
  `AgentUpsert` event in the transaction of §7.2 (step 6). A replay of the whole log, in
  order, therefore ends at the migrated state. The log carries agent records, not role
  records (code-verified: its four event kinds).
- `RegistryEvent` and `AgentRecord` refuse unknown fields today (code-verified), so a build
  older than the epoch cannot decode an event with a new field and cannot apply a namespace-2
  role as namespace 1. The P1 plan must keep that property in the form it gives the
  namespace.

### 7.4 The `custom:` prefix

The migration classifies every namespace-1 `custom:<slug>` value before it writes:

| Namespace-1 value | Migration |
|---|---|
| `custom:<slug>`, slug not a structural name of §2.1 (`custom:reviewer`) | Becomes `<slug>`. Same role record, same tier (`member` unless the owner set another), same profile. |
| `custom:<built-in>` that namespace 1 already reads as the built-in (`custom:tester`, `custom:director`) | Handled as that namespace-1 built-in. `custom:director` is Project Director there, so it becomes `project-manager`, never the new `director`. |
| `custom:council-chair` | Becomes `council-chair`. This is the one mapping to a structural name that Record #193 fixes (§1): the existing Council Chair role becomes the built-in, with the tier §2.1 gives it. That tier is not in the review chain of §2.1.1. |
| `custom:<slug>`, slug any other structural name (`custom:lead`, `custom:assistant`, `custom:lab_manager`, `custom:lab-manager`, `custom:project-manager`) | **Collision.** Not stripped. |

A user-defined role record whose slug is a structural name of namespace 2 (`project-manager`)
is a collision in the same way.

On a collision the migration refuses. It reports each colliding role and the agents that hold
it, writes nothing, and leaves the store below the epoch. It runs again only with an Owner
mapping for every colliding role, given with Owner authority (§2.3) and journaled. The mapping
is one of:

- **rename**: the custom role gets a non-colliding slug. It stays a user-defined role with its
  own role record and tier `member` unless the owner sets another;
- **structural**: an explicit Owner decision that these agents hold the structural role.

The migration never chooses between the two. With a complete mapping it applies the mapping in
the same transaction as the rest of §7.2.

### 7.5 Older writers, upgrade order and rollback

**Fencing.** Two fences are required. The first exists. The second is new work for the P1
plan.

*Fence 1, the file format (existing).* A process that opens the store file itself checks the
stored format against its ladder (`check_format`). A build older than the epoch gets
`FormatVersion` from a store at the epoch, as an older build does today from a store raised to
`FORMAT_WITH_WORKFLOW` or `FORMAT_WITH_AREA_DELETE` (code-verified). This fences a direct or
embedded open by an older build, and with it an older store server, which opens the file
itself.

It does not fence a client of a store server. The shared store open (`FerroStore::auto`)
returns a client of the `ferrostore serve` server when one listens at the store's socket, and
opens the file only when none does. The server handshake is authentication only: it carries
no format check and no version of the role namespace. The body of a remote write runs in the
client (code-verified: `ferrostore/mod.rs`, `auth.rs`, `server.rs`, `remote_txn.rs`). In the
deployed topology a store server runs, so the hub, the wire daemon, the CLI and the hooks are
its clients. Without a second fence, an older client could write old role spellings through
the socket of a store at the epoch.

*Fence 2, the store server handshake (new, P1 plan).* The store server protocol carries the
client's role-namespace capability, or a protocol version that implies it, in the handshake.
A server at the epoch refuses a client that does not state it. An older client cannot state
it, so it is refused before any write: it fails closed. This is not existing behaviour, and
the migration must not be released without it.

With both fences, once the epoch is committed a hub, wire daemon, CLI, store server or hosted
hook older than the epoch cannot write to the store, by a direct open or as a client. A
mixed-version live write fails closed. It is never accepted with an old spelling and never
translated on the way in. Translation exists only for the events of the registry log (§7.3).

Two cases need more than the fences, and the P1 plan must close both:

- A process that opened the store, or connected to its server, before the epoch was
  committed. Whether it meets a fence again on a later write is not verified here. The
  upgrade order below removes the case; the migration must also refuse to start while another
  process holds the store or a connection to its server, or the plan must prove that such a
  write is refused.
- A live role-bearing write that reaches a build at the epoch from another process (a hub
  request, a FerroWire message) and not through the store. It must state namespace 2, by a
  field or by a protocol version that implies it. A live write without that statement is
  refused; it is not decoded as namespace 2. This rule does not apply to a registry event,
  which is a replayable record and not a live request: an event that states namespace 2 is
  applied, and an event without a namespace is namespace 1 and is translated on import
  (§7.3). It is neither refused for the missing statement nor applied verbatim.

**Supported upgrade order.**

1. Stop every writer: the hub, the wire daemon, the store server and the hosted sessions,
   whose hooks call `fl`.
2. Upgrade every binary to a build at the epoch.
3. Run the migration once. It takes its backup (below) and then commits §7.2.
4. Start the writers.

No other order is supported. A build at the epoch that finds a store below the epoch refuses
role writes and names the migration; it does not migrate implicitly on first write. It does
the same, for role reads also, on a store at or above the epoch without the marker (§7.1). A
new, empty store is the trivial case: the migration has nothing to rewrite and only raises
the format and writes the marker. Whether a build at the epoch creates a new store at the
epoch, with the marker, directly is for the P1 plan.

**Rollback.** Before it writes, the migration takes a backup of the pre-migration store and
reports where the backup is. Rollback is: stop every writer, restore that backup, reinstall
the builds older than the epoch, and request the approvals of every open pull request again.
There is no in-place downgrade: the format never lowers, and a reverse rewrite would be
ambiguous for the same reason the forward one must run once. Writes made after the migration
are lost by a restore.

The approvals are requested again because a review posted after the epoch is not readable by
an older build (§7.3). That build does not count a post-epoch approval, and it does not see a
post-epoch rejection, so an earlier approval by the same reviewer on the same head would still
count. After a rollback, no pull request is merged on a review posted before the rollback.

### 7.6 Renames that follow

- `RoleReviewPolicy` `lab-manager-review` is renamed `director-review`; the old spelling is
  read, not written. No `director-review` value exists in namespace 1 (code-verified), so
  this rename is not ambiguous. `pr_policy` tiers are renamed the same way and gain
  `project-manager` (§2.1.1).
- The virtual area `lab-manager`, `@role:` addressing, hydration role-file names and FerroDeck
  labels follow. Role files and workflow files in `.agents` and in each repository are
  instruction files: Owner-tier pull requests, coordinated with the Director.

## 8. Acceptance sketches

- A member calls `fl github pr merge` on another agent's pull request: refused, naming the
  action and the roles that hold it; nothing is sent to GitHub.
- A Lead calls `fl github issue create` with `--by` naming another agent: refused on the
  identity mismatch.
- The same command from a shell that is not under a hosted session: refused without Owner
  authority.
- Spin-up with no `permission_policy`: the hosted record shows the role's profile and its
  compile receipt. With a widening override and no Owner authority: refused, naming the rule.
- Two agents of different roles in one project directory get different compiled rules, and
  neither run changes a shared settings file.
- A raw `gh pr merge` in a gated session is denied with a message that names
  `fl github pr merge`.
- Migration fixtures: a store below the epoch holding `lab_manager`, `lab-manager`,
  `director`, `custom:director`, `custom:council-chair` and `custom:reviewer` reads back as
  `director`, `director`, `project-manager`, `project-manager`, `council-chair` and
  `reviewer`, at the epoch format and with the role-namespace marker. The agent that held
  `custom:reviewer` has the same role record, tier `member` and the same profile as before the
  run.
- Repeated run: a second run of the migration on that store is a no-op. Role values and the
  format are unchanged, and the agent that became `director` is still `director`, not
  `project-manager`.
- Structural-name collisions: a store holding `custom:lead` and `custom:lab_manager` makes the
  migration stop with the Owner-mapping refusal. The refusal names both roles and the agents
  that hold them, and the store is unchanged and still below the epoch. With an Owner mapping
  for both, the run completes and each agent has exactly the mapped role.
- After the migration, `custom:reviewer` on input is read as `reviewer`; `custom:lead`,
  `custom:director` and `custom:lab_manager` on input are refused with the message of §2.1;
  creating a user-defined role named `lead` or `project-manager` is refused.
- Mixed version, direct open: after the epoch is committed, a build older than the epoch that
  opens the store file itself (an embedded open, or an older store server) is refused with
  `FormatVersion`, and the store is unchanged.
- Mixed version, client: a client older than the epoch that connects to a store server at the
  epoch states no role-namespace capability and is refused in the handshake, before any
  write. The store is unchanged; no `lab_manager` or `custom:` value is written. A
  role-bearing request without namespace 2 to a hub at the epoch is refused.
- Event replay: a new store and a lagging store each import a registry log that holds
  pre-epoch `AgentUpsert` events with `lab_manager`, `director` and `custom:reviewer`,
  followed by the migration's namespace-2 events. Each store ends with the same roles as the
  migrated store. A pre-epoch `director` event, imported alone by a build at the epoch, gives
  `project-manager` and never the new Director. The stored copy of every event is unchanged.
- Marker decoding: a posted marker without a namespace field and with role `lab_manager`
  verifies as the Director tier, and one with role `director` verifies as the Project Manager
  tier. A marker with `role_ns: 2` and role `director` verifies as the Director tier. Neither
  old marker gains or loses a tier by the migration.

## 9. Open decisions

- **O1.** Resolved by the owner on 2026-11-13: see §1 items 5 and 6 and §2.1.1.
- **O2 (owner).** The open cells of §6: issues filed by members, the Project Manager and the
  Director; session clear and reforge by a Lead for its own team, by the Project Manager and
  by the Director; and whether a Project Manager may merge a blocked author's pull request
  that it approved, as a Lead and the Director may.
- **O3.** The gate timeout duration, and how a timeout or a gate error is reported to the
  operator, after M2 and M3. The authorization semantics of §3.3 are fixed and not open: the
  profile's `defaults` map, an ask is never auto-allowed, deny on timeout or error, and no
  restriction rests on the gate alone.
- **O4.** Whether the interim deny rules for raw `gh` writes (P2) ship before or with the new
  broker commands; before means a short period in which members cannot file issues at all.
