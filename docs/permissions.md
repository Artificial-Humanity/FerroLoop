# Permissions

One canonical permission policy per agent, compiled into each vendor's native launch arguments
and files by a pure, deterministic, fail-closed compiler. The module is `fl_core::permissions`.
The design is
[the universal permissions spec](superpowers/specs/2026-11-03-universal-permissions-design.md)
(sections 0 to 2).

The compiler reads three arguments (a vendor, the caller's facts, a policy) and returns a
`CompiledPlan` or a typed `CompileRefusal`. It does no I/O, reads no clock or environment, and
never runs or probes a vendor. The same arguments give the same bytes.

## The policy

```json
{
  "schema_version": 1,
  "posture": "ask",
  "sandbox": "vendor-default",
  "require_fs_enforcement": false,
  "unenforced_defaults": "refuse",
  "rules": [
    {"capability": "fs_read_workspace", "effect": "allow"},
    {"capability": "shell_exec", "effect": "deny",
     "pattern": {"kind": "argv_prefix", "argv": ["git", "push"]}}
  ]
}
```

Every input type refuses unknown fields. `schema_version` is required and is `1`.

Three separate axes:

| Field | Values | Meaning |
|---|---|---|
| `posture` | `ask`, `accept-edits`, `auto`, `plan`, `bypass` | How an "ask" is resolved. Never compiled into rules. |
| `sandbox` | `off`, `vendor-default` | The OS sandbox under the rules. |
| `rules` | up to 256 | Per-capability `allow`, `deny` or `ask`. |

Two switches on the policy:

* `require_fs_enforcement` (default `false`): see the fail-closed rules.
* `unenforced_defaults` (`refuse`, the default, or `acknowledge`): what a compile does when a
  canonical default deny, or a requested deny, has no native enforcement on the vendor. `refuse`
  returns `UnenforcedRequest` naming the capability and vendor. `acknowledge` emits the plan with
  its `unsupported` receipt line and lists the capability in `plan.unenforced` (kind `deny`). The compiler
  never decides to acknowledge: the Hub's spin-up UI must collect the Owner's explicit
  acknowledgement before it sets `acknowledge`, and must show `plan.unenforced` wherever it shows
  the plan.

`bypass` is the only posture that produces a bypass flag (`--yolo`, `--disable-approval`,
`--dangerously-skip-permissions`, `bypassPermissions`, approval `never`); `sandbox: off` is the
only source of a sandbox-off flag (`--disable-sandbox`, `danger-full-access`). Rules never
produce either. A final audit refuses a plan that carries one without its axis.

### Capabilities and pattern grammars

A rule's `pattern` is typed. Omitting it addresses the whole capability (for the external
capabilities, every path outside the workspace; for `network_web`, every domain).

| Capability | Pattern `kind` | Grammar |
|---|---|---|
| `fs_read_workspace`, `fs_write_workspace` | `path_glob` | Relative to the workspace. No `..`, `.`, empty segment, trailing `/`, leading `/` or `~`, backslash, `[ ] { } ( )`, leading `!` or `#`, partial `**`. |
| `fs_read_external`, `fs_write_external` | `abs_path_glob` | Absolute. The same rules. |
| `fs_write_protected` | `path_class` | `settings`, `git` or `tool-config`. |
| `shell_exec` | `argv_prefix` | 1 to 32 tokens compared exactly; the command name does not start with `-`. |
| `shell_exec` | `shell_glob` | A string glob; `*` is the only wildcard (at most 8); no `\ ( ) ? [ ] { }` or `**`; it does not end in `:*`, which Claude reads as the legacy prefix form. |
| `network_web` | `domain` | A lowercase ASCII host, or `*.host`. `*` means every domain. |
| `mcp_tool_call` | `mcp` | `server`, or `server` and `tool`. A server name has no `__`. |

All patterns refuse control characters, NUL, empty values, and values over 512 bytes (256 per
argv token). `**` and `/**` collapse to "the whole capability".

### Normalisation and validation

`PermissionPolicy::normalized` validates, canonicalises, removes duplicates and sorts rules by
capability, then deny before ask before allow, then pattern. Precedence is deny over ask over
allow. It refuses:

* the same capability and pattern with two different effects;
* `bypass` with any `deny` or `ask` rule (bypass makes both meaningless);
* `auto` with any `ask` rule, and `accept-edits` with an `ask` rule on a write capability;
* `require_fs_enforcement` with `sandbox: off` or `bypass`.

### Canonical defaults

A capability no rule mentions takes the canonical default, never an unknown vendor default:
`deny` for `fs_write_protected`, `ask` for the other seven. `ask` means "defer to the posture's
approval path". Defaults are not applied under `bypass`. A default is compiled where the vendor
can state it (the protected-path denies on Claude and Grok) and is otherwise reported in a
receipt line (`origin: default`) that says whether the vendor enforces it. A default *deny* that
a vendor cannot enforce follows `unenforced_defaults`. Codex's read-only sandbox and Muse's
read-only profile block every write, so they count as enforcing the protected-path default.
The protected-path default is judged per path class: a rule that names one class (an allow of
`git`, say) leaves the other classes at the default, and they are refused or listed as above.

### Presets

Every preset ships with `unenforced_defaults: refuse`. With the default Antigravity facts, the
writable presets are refused there (the protected-path default has no native form), as are Codex
and Muse when the plan needs a writable sandbox or profile, until the policy acknowledges it.

| Preset | Posture | Rules |
|---|---|---|
| `read-only` | `plan` | allow workspace reads; deny workspace writes; deny the web |
| `workspace` | `ask` | allow workspace reads and writes; deny the web |
| `workspace-web` | `ask` | allow workspace reads and writes; allow the web |
| `custom` | `ask` | none |

Every preset uses `sandbox: vendor-default`. `Preset::policy()` builds each one.

## The compile contract

```rust
compile(vendor: Vendor, facts: &VendorFacts, policy: &PermissionPolicy)
    -> Result<CompiledPlan, CompileRefusal>
```

`VendorFacts` is everything the caller knows: the `transport` (`pty`, `print`, `app_server`,
`msp`), the `installed_version`, a set of `capabilities_proven`, and three vendor-specific
lists (`exact_commands` for Antigravity, `muse_profiles` for Muse, `sandbox_profile` for Grok).
A proven fact is one of: `interactive_ask`, `fs_enforcement_coverage`, `hard_deny(capability)`,
`hot_edit(field)`, `runtime_proven(surface)`, `execpolicy_allow_keeps_sandbox` (Codex) and
`profile_confines_external` (Muse). Nothing is assumed. Profile and sandbox ids may not start
with `-`.

A `CompiledPlan` holds `launch_args`, `config_files` (each a symbolic `ConfigTarget`, a format
and content; no `~` is resolved), `env` (always empty today; never a secret), `receipts`,
`change_classes`, `requires_respawn` (true when a field this plan actually sets has the
`respawn` class), `unenforced` (the acknowledged unenforceable denies, kind `deny`, and the ask defaults the vendor does not gate, kind `ask`, which are listed under either setting and never refuse; an older plan without the field reads as empty) and the `key` (vendor,
transport, version) it was compiled for.

### Native forms

| Vendor | Posture and sandbox | Rules |
|---|---|---|
| Claude Code | `--permission-mode manual\|acceptEdits\|auto\|plan\|bypassPermissions`; bypass adds `--allow-dangerously-skip-permissions`; the skip flag is never emitted | `.claude/settings.local.json` `permissions.{allow,ask,deny}`: `Read(./glob)`, `Edit(./glob)` and `Write(./glob)` (workspace globs are `./`-anchored: a bare `*.rs` or `Makefile` matches at the workspace root only, so to deny or allow at any depth write `**/*.rs`), `Bash(prefix)` with `Bash(prefix *)`, `WebFetch(domain:d)`, `WebSearch`, `mcp__server[__tool]`; absolute globs as `//path`; protected paths as `Edit`/`Write` denies |
| Codex | `-a` approval policy and `-s` sandbox mode, kept split; `danger-full-access` only for `sandbox: off` | execpolicy `prefix_rule(pattern=[...], decision=allow\|prompt\|forbidden)`; `config.toml` for `writable_roots`, `network_access`, `web_search`; `-c sandbox_permissions=["disk-full-read-access"]` and `--search` |
| Muse | `--approval-mode`, `--approval-judge off`; bypass is `--disable-approval`, or `--yolo` when the sandbox is also off; `--disable-sandbox` for `sandbox: off` | `--permission-profile` (a caller-verified profile) and `--sandbox-network restricted\|proxy-only\|enabled` |
| Grok | `--permission-mode default\|acceptEdits\|auto\|plan\|bypassPermissions`; `--sandbox <profile>` from the facts | the Claude rule strings as `--allow` and `--deny`; `--disable-web-search`; `--add-dir` |
| Antigravity | `--mode accept-edits\|auto\|plan` (none for `ask`); `--dangerously-skip-permissions` for bypass | inline `--settings` JSON: `permissions.allow` with `command(<exact command>)` and `allowNonWorkspaceAccess` |

Notes on what degrades:

* **Claude and Grok.** A deny or ask on a filesystem or network capability is a tool rule: it
  does not bind the shell, MCP or plugins (`tool_scoped_emulation`). "Every external path" has
  no tool-rule form, so a deny of it is refused. An argv token that needs quoting or holds a
  wildcard cannot be stated exactly: an allow with one is not granted; a deny or ask is cut at
  the first such token (matches more, so stricter), and refused if the command name itself is
  affected. A wildcard-domain deny widens to every `WebFetch`. Grok has no ask list: the
  workspace root is not known to the compiler, so an ask on any write capability withdraws every
  allow of all three write capabilities (workspace, protected, external) and every `--add-dir`
  root, and falls back to the default protected-path deny; an ask on an external read likewise
  withdraws the read allows; an ask on workspace reads is refused.
* **Codex.** Extra writable directories are explicit `writable_roots`, and only for a pattern
  that is exactly a directory subtree under the workspace-write sandbox. A narrower workspace
  write rule never widens the sandbox: a scoped write deny selects `read-only`. A coarse grant
  never satisfies a narrower policy. Shell rules are argv prefixes only; a string glob loses its
  wildcard part (stricter) or is not granted; only plain tokens become pattern elements, and a
  glob is cut at the first token that is not plain (refused if the command name is not plain).
  An execpolicy `allow` may run the command outside the sandbox (unverified), so an allow of a
  shell rule compiles to `prompt` unless the facts prove `execpolicy_allow_keeps_sandbox`, and is
  never emitted under the read-only sandbox. `network_web` has three receipt lines (sandbox
  transport, host approvals, WebSearch tool). Arbitrary MCP servers have no proven per-tool
  control, so an MCP deny is refused unless the facts prove `hard_deny(mcp_tool_call)`.
* **Muse.** A profile carries no glob grammar. The compiler picks the strongest verified profile
  that does not exceed the policy and reports the rest as a downgrade. A deny or ask on an
  external path needs a selected verified profile plus `profile_confines_external`, or a proven
  `hard_deny`, and is refused otherwise or under sandbox off. A domain allow degrades to
  `proxy-only`; the proxy's own allowlist governs and may be broader than the domains. `--yolo` and `--disable-approval` come only from `bypass`.
* **Antigravity.** A shell prefix or glob compiles by enumeration: only exact commands the caller
  lists in `exact_commands` and that no deny or ask rule matches become `command(...)` entries
  (matching work is capped in byte steps, and a request over the cap is refused as invalid facts);
  everything else stays ask. One boolean stands for both external read and external write; it
  is `true` only when both are allowed without a pattern. `artifactReviewPolicy` is left
  unmanaged. The vendor has no native deny, so a deny is refused unless the facts prove
  `hard_deny` for that capability.

## The fail-closed rules

1. An **allow** with no exact native form is not granted. It never becomes an allow. The receipt
   says whether the vendor falls back to a proven prompt channel (`interactive_ask`) or is
   treated as denied.
2. A **deny** with no native enforcement is refused (`CompileRefusal::UnenforceableDeny`), unless
   a strictly stronger native form exists; that form is used and receipted as
   `stricter_downgrade`. A deny never becomes an ask.
3. An **ask** with no native form falls to the vendor's prompt path, and every allow of the same
   capability is withdrawn, since an allow would outrank it. Where the fallback would run the
   action without a prompt, the plan is refused (`UnenforceableAsk`).
4. A posture that cannot be enforced is refused (`PostureNotEnforceable`), as is a posture and
   sandbox the vendor cannot combine (`SandboxConflict`).
5. `require_fs_enforcement: true` refuses the plan if any filesystem restriction is a
   `tool_scoped_emulation` or `unsupported`, unless the facts prove
   `fs_enforcement_coverage` (`FsEnforcementRequired`).
6. A receipt or an operator prompt is never enforcement.

## Receipts, evidence and change classes

Every rule and every omitted capability has at least one receipt line: the request, the native
form, a `class` (`exact`, `stricter_downgrade`, `tool_scoped_emulation`, `unsupported`), an
`evidence` class and a note.

Evidence is `documented` (help text or docs), `schema_defined` (an exported vendor schema;
presence is not proof), `runtime_proven` or `unsupported`. The table is
`evidence_for(vendor, surface, facts)`. A surface is `runtime_proven` only when the facts say so.

Each plan lists a change class for the posture, the sandbox and each capability's rules:

| Class | Meaning |
|---|---|
| `hot` | Applied at once. Only when the facts prove `hot_edit` for that field. |
| `session` | Applies to the running session. Only when proven (posture). |
| `pending_next_start` | A file the vendor reads at start. A write proves submitted state only. |
| `respawn` | The process must be replaced. |

Without a proof, rules in a file are `pending_next_start` and everything else, including every
sandbox change, is `respawn`. `requires_respawn` is true when any class is `respawn`.

## Refusals

`CompileRefusal` is one of: `InvalidPolicy`, `InvalidFacts`, `UnsupportedTransport`,
`UnenforceableDeny`, `UnenforceableAsk`, `PostureNotEnforceable`, `SandboxConflict` (plan with
sandbox off on Codex and Muse), `FsEnforcementRequired`, `UnenforcedRequest`,
`InternalInvariant`. A refusal emits nothing.

## What this does not do

* Live interception and remote resolution of approvals (spec section 3), the PTY bridge, and
  drift or sync-back from native files.
* Wiring into the Hub: spin-up, the settings editor, the compile receipt in the UI, writing the
  files, and applying a plan to a running session.
* Reading a vendor's installed version, schema or settings; the caller supplies facts.
* The open decisions D6 (Always Allow writes vendor-native rules) and D7 (Antigravity remote
  resolution).
