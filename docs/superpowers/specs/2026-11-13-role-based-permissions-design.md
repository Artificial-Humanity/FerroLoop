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
another tier. The wire form of every role is its bare slug. `custom:<slug>` is accepted on
input for compatibility and never written.

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
places.

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
  "actions": [
    {"action": "github.issue.create",  "scope": "own-project"},
    {"action": "github.pr.review",     "scope": "own-team"},
    {"action": "github.pr.merge",      "scope": "own-approved"},
    {"action": "session.reforge",      "scope": "own-team", "effect": "ask-owner"}
  ]
}
```

`policy` is exactly the #103 `PermissionPolicy`; this spec adds no capability. Action names
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
evaluates the caller's canonical tool rules:

- **deny** → the hook blocks the call and returns the reason and, where one exists, the broker
  command to use instead;
- **allow** → the hook pre-answers, where measurement (§4) shows the vendor honours it;
- **no rule** → the vendor layer decides as today.

It is a local policy engine with a bounded timeout. On timeout a call that matches a deny
pattern is denied and every other call falls through to the vendor layer. Every decision is
journaled. FerroLoop installs the hook through the same per-session settings as §3.2; the
hand-registered user-level entries are retired with it.

### 3.4 What is not enforcement

Prompts, role prose, hydration text and compile receipts instruct or report. None of them is
counted as a control in this design.

## 4. Phase 0 — measure before building §3.2 and §3.3

One fact decides how much §3.2 and §3.3 can deliver, and it is unmeasured for every vendor:
which mechanism takes priority over the vendor's own reviewer. The only observation so far is
not conclusive: with `Bash(gh issue create:*)` allowed at user level and posture `auto`, a
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

§3.1 depends on Phase 0 least: it needs one allow rule (`fl github`) per vendor, and if M1
fails for a vendor the fallback is that vendor's reviewer judging one stable command instead
of every `gh` form.

## 5. Phases

| Phase | Content | Depends on |
|---|---|---|
| P0 | Measurements M1–M6 on five vendors | — |
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

The slug `director` changes meaning, so order and atomicity matter.

1. One store migration, one transaction: every `director` role value and role record becomes
   `project-manager`; then every `lab-manager` / `lab_manager` value becomes `director`. No
   agent holds `director` today, so the first step is empty in this workspace; it still runs
   first.
2. After the migration `from_wire` reads `lab-manager` and `lab_manager` as `director` (the
   old name of the new Director). It never maps `director` to `project-manager`: that reading
   existed only before the migration.
3. `RoleReviewPolicy` `lab-manager-review` is renamed `director-review`; the old spelling is
   read, not written. `pr_policy` tiers are renamed the same way and gain `project-manager`
   (§2.1.1). `fl-review` markers already posted keep their `role` text; verification accepts
   both spellings.
4. `custom:<slug>` values are rewritten to `<slug>`. `council-chair` becomes a built-in role
   with tier `council-chair`.
5. The virtual area `lab-manager`, `@role:` addressing, hydration role-file names and FerroDeck
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
- Migration fixtures: a store holding `lab_manager`, `lab-manager`, `director` and
  `custom:council-chair` reads back as `director`, `director`, `project-manager` and
  `council-chair`, and a second run changes nothing.

## 9. Open decisions

- **O1.** Resolved by the owner on 2026-11-13: see §1 items 5 and 6 and §2.1.1.
- **O2 (owner).** The open cells of §6: issues filed by members, the Project Manager and the
  Director; session clear and reforge by a Lead for its own team, by the Project Manager and
  by the Director; and whether a Project Manager may merge a blocked author's pull request
  that it approved, as a Lead and the Director may.
- **O3.** Gate timeout and the deny-on-timeout class in §3.3, after M2 and M3.
- **O4.** Whether the interim deny rules for raw `gh` writes (P2) ship before or with the new
  broker commands; before means a short period in which members cannot file issues at all.
