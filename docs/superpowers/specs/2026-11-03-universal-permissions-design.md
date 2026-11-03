# Record #103 — Universal cross-vendor granular permission & auto-accept mapping

Draft: Fenris (Muse seat). Review: Fiona (Codex seat). Lead: Ferris.
Evidence: local CLI `--help`, config files and exported schemas on ai-lab-0 at main `975bcac`, 2026-11-03
(Muse installed, codex-cli 0.166.0 stable+experimental schema exports, Muse with MSP schema export, Grok Build, Antigravity CLI).
Schema presence is not runtime proof; each vendor row names which state its claims are in
(documented, schema-defined, runtime-proven, unsupported).
Design only; authorizes no setting changes on live sessions.

## 0. The shape that works everywhere

All five runtimes decompose permissions into the same three layers. The names
differ; the structure does not:

| Layer | What it decides |
|---|---|
| Posture | Ask, auto-accept edits, plan/read-only, or bypass-all |
| Rules | Per-tool allow/deny/ask with argument patterns |
| Sandbox | OS enforcement under the rules (fs + network) |

FerroLoop's job is not to invent a sixth permission system. It is to hold one
canonical policy per agent, compile it into each vendor's three layers at
spin-up, re-compile the layers that are live-editable, and answer live
requests for the layers that are not.

Two honesty rules recur below. First, sandboxing and permission prompts are
different mechanisms: a vendor can run unsandboxed with prompts, or sandboxed
with `--yolo`. The universal schema keeps them as separate axes. Second,
several vendor surfaces are coarse (whole-tool allow/deny, home-directory
trust); any capability a vendor cannot express natively is compiled to the
nearest *stricter* native form and reported, never silently widened.

## 1. Spin-up (launch-time) vs live-editable, per vendor

### Claude Code

- **Launch flags:** `--permission-mode {acceptEdits|auto|bypassPermissions|manual|dontAsk|plan}`,
  `--dangerously-skip-permissions`, `--allow-dangerously-skip-permissions`
  (offers bypass without enabling it), `--allowedTools` / `--disallowedTools`
  with patterns such as `Bash(git *)`, `--permission-prompt-tool <mcp>` and
  `--permission-prompts host|none` (print mode).
- **Live-editable:** `permissions.{allow,deny,ask}` and `defaultMode` in
  settings (`~/.claude/settings.json`, project `.claude/settings.json`,
  `.claude/settings.local.json`). The CLI watches settings files; rule edits
  take effect without restart. `/permissions` edits the same state in-session.
  Permission mode itself is switchable in-session (Shift+Tab / mode commands),
  including into `bypassPermissions` only when it was offered at launch.
- **Restart required:** the launch-time offer of bypass
  (`--allow-dangerously-skip-permissions` cannot be granted mid-session), the
  permission-prompt tool binding, and hook definitions
  (`PreToolUse`/`PermissionRequest` hooks in settings).

### Codex

- **Launch flags:** `-a/--ask-for-approval {on-request|never|...}`,
  `-s/--sandbox {read-only|workspace-write|danger-full-access}`,
  `--dangerously-bypass-approvals-and-sandbox`, `--add-dir` (extra writable
  roots), `-c key=value` config overrides (for example
  `sandbox_permissions=["disk-full-read-access"]`),
  `--approve-for-me` (automatic approval review).
- **Live-editable:** `~/.codex/config.toml` (`approval_policy`,
  `sandbox_mode`, `sandbox_workspace_write.writable_roots`,
  `shell_environment_policy`) is read per process start, so file edits apply
  to *new* invocations, not the running TUI. Command-prefix rules
  (`~/.codex/rules/*.rules`, Starlark `prefix_rule`) are what the approval UI
  writes when a user picks "always allow"; a running process re-reads its
  rules for subsequent commands.
- **Restart required:** sandbox mode and approval policy of the interactive
  session (chosen at launch; the app-server instead takes them per thread).

### Muse

- **Launch flags:** `--approval-mode {untrusted|on-request|never}`,
  `--permission-profile <ID>`, `--approval-judge off|on`, `--yolo`,
  `--disable-approval`, `--disable-sandbox`, `--sandbox-network
  {restricted|enabled|proxy-only}`, `--trust-workspace`, and the pair of
  process-level `--disable-approval` / `--disable-sandbox` bypasses.
  `muse serve` fixes the sandbox posture at host construction.
- **Live-editable:** `permissions.default_profile` and named profiles in
  `~/.config/muse/settings.json` govern new sessions; the interactive
  approval prompt itself offers session-scoped and persistent choices (the
  open `ApprovalDecision` enum in the MSP schema includes `approved`,
  `approvedForSession`, `approvedPolicyAmendment`, `denied`,
  `deniedPolicyAmendment`), so rule growth happens *by answering prompts*,
  not by editing files under a running session.
- **Restart required:** sandbox on/off and network mode (process launch
  posture), trust (workspace trust is a launch decision), approval mode for
  an existing conversation.

### Grok (Grok Build)

- **Launch flags:** `--permission-mode {default|acceptEdits|auto|dontAsk|bypassPermissions|plan}`,
  `--allow <RULE>` / `--deny <RULE>` (Claude-compatible aliases
  `--allowedTools`/`--disallowedTools`), `--always-approve`,
  `--disallowed-tools`, `--disable-web-search`, `--sandbox <PROFILE>`
  (env `GROK_SANDBOX`), `--add-dir`.
- **Live-editable:** `[ui] permission_mode` and `yolo` in
  `~/.grok/config.toml` set defaults for new sessions; the TUI's own mode
  switching mirrors Claude's set, so posture is session-switchable, while
  allow/deny *rules* arrive via launch flags or prompt answers.
- **Restart required:** `--sandbox` profile selection and `--add-dir` roots.

### Antigravity (`agy`)

- **Launch flags:** `--dangerously-skip-permissions`, `--sandbox`
  (terminal restrictions), `--settings <json|path>` (session-only settings
  override, never saved), `--mode {accept-edits|auto|plan}`.
- **Live-editable:** `~/.gemini/antigravity-cli/settings.json` —
  `permissions.allow` entries such as `command(git status)` (exact-command
  patterns minted by past approvals), `allowNonWorkspaceAccess`,
  `artifactReviewPolicy`. Approvals in-session append `command(...)` entries;
  hand edits are picked up by new invocations.
- **Restart required:** `--sandbox` and `--dangerously-skip-permissions`
  posture; the cascade/LS surface has no per-tool approval RPC comparable to
  Codex app-server (Track A/#217 record).

**Pattern across all five:** prompt answers grow rules on every vendor
(session or durable). Beyond that, no global hot-edit claim survives:
change classes are keyed by **(vendor, transport, installed version,
capability)**. A config-file write proves only *submitted* state; hot
application requires a supported mutation channel plus an effective
read-back or a conformance test, otherwise the change is shown as
*pending-next-start* or *restart-required*. Posture is launch-fixed on
most transports (session-switchable in the Claude/Grok TUIs inside the
launch-offered set; Codex app-server thread/turn overrides are a separate
surface from TUI launch settings, with effective policy read back from
start/resume replies). A tighter policy fences new work until its
effective restrictions are proven. The editor labels every change with
its keyed class — *hot*, *session*, *pending-next-start*, *respawn* —
before the user makes it, and never upgrades a live-test hypothesis into
a hot-edit promise.

## 2. The universal capability schema

One canonical rule set per agent, stored natively, compiled per vendor. A
rule is `{capability, effect: allow|deny|ask, pattern?}` where the pattern
grammar is deliberately the intersection all vendors can approximate:

| Capability | Pattern field |
|---|---|
| `fs_read_workspace` | path glob, relative to workspace |
| `fs_read_external` | absolute path glob |
| `fs_write_workspace` | path glob |
| `fs_write_external` | absolute path glob |
| `fs_write_protected` | path class: settings, git, tool config |
| `shell_exec` | command prefix or glob |
| `network_web` | domain, or `*` |
| `mcp_tool_call` | `server` or `server/tool` |

Posture is a ninth, separate field (`posture: ask|accept-edits|auto|plan|bypass`),
plus a sandbox choice (`sandbox: off|vendor-default`). Posture is *not* a
capability and is never compiled into rules; bypass in particular must stay
visible as a posture so an audit can distinguish "wide open by rule" from
"checks disabled".

Compilation compares the composed native policy against the requested
restrictions, per vendor and deterministically. An unsupported *allow* can
become deny, or ask only through a proven interactive channel. An
unsupported *deny* must remain denied through a proven native restriction;
otherwise the plan is refused — a deny never degrades to ask (an ask with
no prompt, under a bypass posture, is silently a run). The compiler
rejects any posture that disables checks its rules need, and never treats
a receipt or an operator prompt as enforcement.

Patterns are typed grammars, not interchangeable text: shell argv prefix,
shell string glob, path glob and domain matcher each have their own
escaping and version-specific matching tests. Omitted capabilities take
the canonical *deny/ask* default from the profile, never an unknown
vendor default; rule precedence is deny > ask > allow unless an item
states otherwise.

A tool rule is tool-scoped: an `Edit`-deny does not protect a path from
shell commands, MCP tools or plugins. `fs_write_protected` may claim
*filesystem* enforcement only when an OS/runtime boundary covers every
enabled access path; without that coverage it is labelled tool-scoped
emulation in the receipt, and a plan that requires filesystem enforcement
is refused. Fixtures cover deny fallback, bypass-vs-deny, mixed external
read/write, shell writes around tool rules and pattern escaping.

### Claude Code

- Posture: `permissions.defaultMode` / `--permission-mode`; `bypass` maps to
  `bypassPermissions` and additionally requires the launch offer.
- Rules: `Read(<glob>)` (fs read), `Edit(<glob>)` / `Write(<glob>)` (fs
  write), `Bash(<prefix *>)` (shell), `WebFetch(domain:<d>)` and `WebSearch`
  (network), `mcp__<server>` / `mcp__<server>__<tool>`.
- Effects map one-to-one onto `allow` / `deny` / `ask` arrays.
- `fs_write_protected` has no name in settings; compile it as explicit
  deny/ask entries for `Edit(.claude/**)`, `Edit(.git/**)` and settings-file
  paths. This is emulation, and the receipt says so.

### Codex

- Posture splits across two native axes and must stay split:
  `approval_policy` (when to ask) and `sandbox_mode`
  (`read-only|workspace-write|danger-full-access`).
- fs compilation uses the installed sandbox/permission schema, not help
  examples. Stable `SandboxPolicy.workspaceWrite` carries explicit
  `writableRoots` (absolute paths): workspace-write with explicit writable
  roots can grant extra directories without `danger-full-access`. A
  coarse workspace grant must not satisfy a narrower path policy by
  silently widening it: requested read/write/deny patterns retain their
  exact scope, or the compiler refuses / returns a proven stricter plan.
  An arbitrary glob can still be unsupported; the dimension is then
  marked unsupported rather than approximated wider.
- `shell_exec` patterns compile to execpolicy `prefix_rule(pattern=[],
  decision=allow|prompt|forbidden)` entries.
- `network_web` splits three ways and the split is normative: sandbox
  network *transport* policy, managed *host approvals* (the exported
  command decision includes persistent `applyNetworkPolicyAmendment`
  with host/action — a host-scoped representation exists, without
  proving complete domain enforcement over every tool or transport), and
  the WebSearch *tool*. Each compiles and receipts separately.
- `mcp_tool_call`: the exported config contains per-tool `AppToolConfig`
  (enabled/approval_mode) for app/connector tools — not proof of
  identical controls for arbitrary MCP servers. Classify the supported
  surface precisely; server disablement is never substituted for a
  different requested scope without a receipt and proven non-widening.

### Muse

- Posture: `--approval-mode` + the sandbox pair
  (`--disable-sandbox`, `--sandbox-network`). `--yolo` is exactly our
  `bypass` posture and must never be produced by compiling rules.
- Rules: named permission profiles (`permissions.default_profile`,
  `--permission-profile`). Where a profile cannot express a glob, the
  compiler emits the closest stricter profile and records the downgrade.
- `network_web` maps onto the sandbox network modes
  (`restricted|proxy-only|enabled`): domain-level allows have no native
  grammar and degrade to the mode choice — `proxy-only` is the honest
  middle setting.
- Prompt answers (`approvedForSession`, `approvedPolicyAmendment`) are the
  native rule-growth path; FerroLoop mirrors them back into the canonical
  set (§3).

### Grok

- Posture: `--permission-mode` accepts the Claude-shaped value set
  (`default|acceptEdits|auto|dontAsk|bypassPermissions|plan`) and
  `[ui] permission_mode` / `yolo` in `~/.grok/config.toml`.
- Rules: `--allow` / `--deny` accept the Claude-compatible rule syntax
  (compat aliases `--allowedTools` / `--disallowedTools` exist), so the
  Claude compiler output is reused almost verbatim.
- `network_web` additionally has `--disable-web-search` as a hard off
  switch; `--sandbox <PROFILE>` carries the sandbox axis separately.

### Antigravity

- Posture: `--mode` plus `--dangerously-skip-permissions`; terminal
  restriction via `--sandbox`.
- Rules: `permissions.allow` holds `command(<exact command>)` entries —
  whole-command strings, not prefixes or globs. A canonical `shell_exec`
  prefix therefore compiles by *enumeration*: only previously approved exact
  commands can be pre-seeded; everything else stays `ask` and grows by
  prompt answer. This is the single largest expressiveness gap of the five.
- fs capabilities: `allowNonWorkspaceAccess` is one boolean standing in for
  both `fs_read_external` and `fs_write_external`; there is no path grammar.
  Both capabilities compile to that boolean plus `ask`, receipt-flagged.
- `artifactReviewPolicy` (`always-proceed` seen live) is a vendor-local
  review posture with no canonical counterpart; leave it unmanaged and say so.

## 3. Live interception and remote resolution

Rule of the section: FerroLoop may only advertise remote resolution on a
transport where it truly owns a bidirectional approval channel. Where it
does not, the honest features are *notification* (FerroDeck shows the
session is blocked on a prompt, with the prompt text when observable) and
*pre-answer by rule* (§2 compiled rules already in place). Screen-scraping
a keystroke into a TUI prompt is a last-resort bridge under #217's PTY
snapshot rules, labelled as such wherever it is used, and never counted as
an approval protocol.

The four operator answers, canonical:

| Answer | Persisted as |
|---|---|
| Approve Once | nothing; single decision |
| Approve for Session | session-scoped grant, dies with the incarnation |
| Always Allow | canonical allow rule, recompiled natively |
| Deny | single decision (or deny rule, only if asked twice) |

Every answer is authorized like any other session control (Owner, or the
controller acting on an already-compiled `allow` rule). A prompt answer is
not inference: protocol replies do not themselves invoke inference. A
reviewer model and every new automated continuation still need
admission. Resumption stays within the original unexhausted execution
reservation, or the original single interactive Owner turn. Approval
does not mint a new allocation. Do not refund an in-flight reservation
merely because an action was declined, cancelled or timed out.

### Where a real channel exists

- **Codex (app-server transport).** The stable 0.166.0 schema carries
  ServerRequests `item/commandExecution/requestApproval` and
  `item/fileChange/requestApproval` (replies: `accept`, `acceptForSession`,
  `decline`, `cancel`, plus execpolicy/network-policy amendments on the
  command kind only) and `item/permissions/requestApproval`, whose reply
  grants a *subset profile* with `scope: turn|session` — a different shape
  from the decision enums and never flattened into them. Legacy
  `applyPatchApproval`/`execCommandApproval` replies use different
  spellings (`approved`, `approved_for_session`, …): never send a v2
  decision into a legacy reply. `availableDecisions`,
  `additionalPermissions` and `thread/settings/update` are **experimental**
  export only and require explicit capability negotiation before reliance;
  stable hot posture change is not advertised. Multiple callbacks can
  share one `itemId` (command vs stdin kinds): the JSON-RPC request id is
  the reply key, with `approvalId` kept as separate metadata. The
  `grantRoot` field's runtime effect is expressly uncertain in its own
  schema description and is unclaimed until a conformance test proves it.
  `serverRequest/resolved` means answered-or-cleared, never executed;
  execution truth comes from the item/turn lifecycle. Stable
  thread start/resume replies expose effective `approvalPolicy`,
  `approvalsReviewer` and `sandbox` — use them as read-back evidence;
  desired, submitted and observed policy stay separate states.
  `never` is no-prompt, not sandbox bypass; `auto_review` is
  model-assisted review that spends tokens, not unconditional acceptance,
  and stays inside #217 admission. Implementation truth today: the fl
  Codex adapter runs `codex exec --json` only (codex.rs:27) — there is no
  live app-server approval interception on that path, and none is claimed
  until the #217 adapter and its tests land. This channel exists **only**
  on the app-server transport; Codex in a PTY is §3.1 territory.
- **Muse (MSP transport).** The exported MSP schema carries
  `approval/request` → `approval/decide` with an open choice set whose
  decision kinds include `approved`, `approvedForSession`,
  `approvedPolicyAmendment`, `denied`, `deniedPolicyAmendment`. The four
  canonical answers map one-to-one (`approvedPolicyAmendment` *is*
  Always Allow, persisted by the vendor). Choices are host-defined with
  open ids: FerroDeck renders the vendor's `label`s and never hardcodes
  choice ids.
- **Claude Code (print/SDK transport).** `--permission-prompt-tool`
  delegates every prompt to a named MCP tool; `--permission-prompts host`
  routes them to the SDK host. FerroLoop will ship a `ferro-permissions` MCP
  server implementing that tool (planned, not implemented; the flag and
  hook surfaces cited here are schema/configuration evidence, not live
  conformance results): the hub presents the request in FerroDeck,
  the tool returns allow/deny (with updated input untouched). Approve for
  Session and Always Allow are synthesized on our side: session grants in
  the hub, Always Allow by an authorized canonical policy action whose
  compiled settings write is then submitted state until read back (§1). In the *interactive TUI* there is
  no such hook: the `PermissionRequest`/`PreToolUse` hooks run local
  commands and can auto-decide, which makes them a local policy engine, not
  a remote channel; a FerroLoop hook shim may consult the canonical rules
  synchronously (bounded timeout, deny on timeout), which is pre-answer by
  rule, not live remote resolution. Advertise remote resolution for
  Claude only on print/SDK transports.

### 3.1 Where no channel exists (PTY bridge, honestly labelled)

Grok TUI, Claude TUI, Codex TUI and Antigravity present prompts inside
their own terminal UI. A screen template can match before the vendor
clears its prompt, so keys can land on a different prompt or an ordinary
shell — before/after snapshots only record that race; they do not bind
keys to a request. V1 PTY integrations therefore provide *notification*
(FerroDeck shows the blocked session, from the vt100 snapshot, never byte
timing) and the existing manual terminal-input surface. A templated key
action, if retained at all as an optional bridge, is labelled **"Send
keys — unverified"**, never "Approve": it binds the session incarnation
and expected snapshot revision, rechecks the screen under the exclusive
writer fence, refuses stale or ambiguous state, and is never retried
automatically after an uncertain write. Those guards still do not
establish vendor request identity, so the bridge never records a
verified permission grant and never satisfies an approval-conformance
gate. A real permission-response UI requires the native request ID and
a supported response contract. Antigravity stays notification-only: no
approval RPC is verified for it, so remote *resolution* is not
advertised for it in v1.

### Sync-back and drift

Prompt answers change vendor state behind our back (Codex rules files,
Muse profile growth, Claude settings edits by `/permissions`, Antigravity
`command(...)` accretions, Grok prompt-minted rules). Native readers
populate *observed* state with provenance and exact scope; they never
widen desired policy — an agent-writable native file or an opaque vendor
choice cannot authorize a wider canonical rule, and a lossy compiler has
no general reversible mapping. A supported observed rule may become a
*staged import proposal*; only a previously authorized matching amendment
or a fresh authenticated policy action commits it. Unknown or
non-round-trippable native rules stay opaque. Desired, submitted and
effective revisions remain separate, and drift is reported as
desired/applied/observed (Track 2 FI-1). Cross-session or shared-home
persistence is an explicit Owner choice with a diff and blast-radius
plan. A generic Always Allow button is unavailable when the native
request lacks a supported persistence action (the current Codex
file-change enum, for example, has no persisted-amendment decision), and
no Deny-twice gesture creates a durable rule without its own distinct
authorized action.

### Cross-vendor approval contracts (all real channels)

1. Only the #217 SessionController answers server requests; it owns the
   RPC stream, request ids, policy revision, session incarnation and
   writer lease. Raw attach viewers, CLI senders and second connections
   never become competing answerers.
2. Persist the request before display. An answer cites the opaque
   request/approval id, expected request revision and current policy
   revision, and atomically claims the unanswered request before
   dispatch. Stale browsers, duplicate clicks, resets and decommissions
   cannot authorize new work.
3. Persist answer-dispatch intent before any byte. A lost answer receipt
   is `ambiguous`: never replay an old acceptance into a replacement
   connection or a newly raised request. Timeout or expiry never means
   acceptance; without a proven safe denial, the request stays blocked
   and visible.
4. One-request grants, session grants and persisted rule amendments are
   distinct actions with distinct authority: broad policy change needs
   authenticated Owner authority or explicit scoped delegation, and a
   persisted amendment's exact diff and future scope are shown before
   confirmation. A persona string, message body or repo file is never
   that authority.
5. Unknown native decision values are preserved as unsupported evidence
   and never coerced to allow; UI offers only decisions supported by the
   current request (constrained by experimental `availableDecisions`
   where negotiated).
6. No scope leakage: thread-local posture never rewrites shared vendor
   settings by default; dedicated children use isolated homes with config
   receipts; if tightening cannot revoke a vendor-side cache, fence new
   work and report restart/thread-replacement required instead of
   pretending a stored row changed a running process.
7. Tool approval never grants model spend; reviewer calls and automatic
   continuations keep #217 admission checks, and no preset may enable an
   unbounded reviewer loop.

## 4. Spin-up and settings UX (the Owner's ask)

- **At creation:** the spin-up form gains a Permissions section: posture
  (default `ask`), sandbox (vendor-default), and a preset selector over
  canonical rule sets (`Read-only`, `Workspace`, `Workspace + web`,
  `Custom`). The compile receipt (native forms + downgrades) is shown
  before launch; what will require respawn to change later is labelled.
- **In agent settings:** the same editor over the canonical set. Every
  edit is labelled with its keyed change class from §1 (by vendor,
  transport, installed version, capability): only rows proven hot —
  supported mutation channel plus effective read-back — apply at once;
  the rest show *pending-next-start* or *restart-required* (sandbox
  changes on launch-fixed transports always say *respawn*). A tighter
  policy fences new work until its effective restrictions are proven.
- **For non-auto-accept users specifically:** `ask` is the default posture,
  prompts queue in the FerroDeck triage view (Track 2 G6) with the four
  canonical answers, and nothing in the default flow requires bypass. The
  whole design exists so that *not* using auto-accept stays usable at
  20-agent scale.

## 5. Acceptance sketches

- Compiler golden tests per vendor: canonical set in, native files/flags
  out, downgrade receipt asserted, including the Antigravity enumeration
  and Codex sandbox-degradation cases. Approval suites additionally prove:
  command/file allow+deny, subset permission grants, turn vs session
  scopes, amendment persistence, two concurrent callbacks for one item,
  stale-reply rejection, reconnect, duplicate click, tightening after a
  cached grant, two threads on one endpoint, and a raw viewer that cannot
  answer — all without live credentials or model inference by default.
- Round-trip tests: answer Always Allow on each real channel (Codex
  app-server, Muse MSP, Claude prompt-tool), then read back and assert the
  canonical set gained exactly that rule.
- PTY bridge tests: template detection on recorded snapshots, refuse to
  answer on unknown screen states, journal completeness.
- Live-edit tests: edit a rule in settings against each running vendor
  (fake first, live canary opt-in) and assert the next tool call obeys it
  without respawn — or record the vendor as respawn-class for that layer.
- Negative: a prompt-injected tool call cannot widen a rule; an
  uncompilable allow never compiles to allow on any vendor.

## 6. Decisions for Ferris / Owner

1. **D5.** Ship order: spec recommends compiler + Claude/Codex/Muse real
   channels first; PTY bridge last and optional per vendor.
2. **D6.** Always Allow from FerroDeck writes vendor-native persisted rules
   (with mirror-back), rather than hub-side-only grants, so a session run
   outside FerroLoop later sees the same rules. Confirm or invert.
3. **D7.** Antigravity remote *resolution* is out of v1 (notification +
   pre-answer only). Confirm.

No live vendor settings were changed during this research.
