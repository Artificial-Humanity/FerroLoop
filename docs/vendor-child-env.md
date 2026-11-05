# The environment of a vendor CLI child

`fl-exec` starts a vendor agent CLI (`claude`, `codex`, `agy`, `muse`, `grok`) for each attempt.
The child gets an allow-listed environment and nothing it would inherit: the spawn calls
`env_clear()` and then sets the variables below. The hub holds the vault master key, hub and wire
tokens, GitHub credentials and every vendor's API key in its own environment; a vendor CLI, and
every tool it starts, never sees them.

The allow-lists and the filter are pure and live in `crates/core/src/child_env.rs` (`fl_core::child_env`;
`fl_exec::child_env` re-exports them and adds the wrappers that read the process environment).
`ProcessHarness` calls it for every vendor spawn, and the wire dispatcher calls it for its headless
fallbacks (below).

## What passes

A variable passes when its name is in one of these sets and no denied prefix matches it.

| Set | Names |
| --- | --- |
| Process | `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `TERM`, `COLORTERM`, `NO_COLOR`, `TZ` |
| Locale | `LANG`, `LANGUAGE`, `LC_*` |
| Vendor config homes | `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME` |
| Agent identity (a name, not a secret; `agent-msg` in the child needs it) | `AGENT_PERSONA`, `AGENT_NAME` |
| Toolchains the agent's builds and tests need | `CARGO_HOME`, `CARGO_TARGET_DIR`, `RUSTUP_HOME`, `NVM_DIR`, `PNPM_HOME`, `BUN_INSTALL`, `GOPATH`, `GOROOT`, `JAVA_HOME`, `PYENV_ROOT`, `VIRTUAL_ENV`, `PYTHONPATH`, `PYTHONHOME`, `NODE_PATH`, `NODE_OPTIONS`, `NPM_CONFIG_PREFIX`, `NPM_CONFIG_CACHE`, `LD_LIBRARY_PATH`, `NIX_PATH`, `NIX_PROFILES` |
| Certificates | `SSL_CERT_FILE`, `SSL_CERT_DIR`, `NIX_SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE` |

Each dialect also receives its own vendor's variables, by prefix, and no other vendor's:

| Dialect | Prefixes |
| --- | --- |
| Claude | `ANTHROPIC_*`, `CLAUDE_*` |
| Codex | `OPENAI_*`, `CODEX_*` |
| Antigravity | `GEMINI_*`, `GOOGLE_*`, `ANTIGRAVITY_*`, `AGY_*` |
| Grok | `XAI_*`, `GROK_*` |
| Muse | `MUSE_*` |

These include endpoint overrides such as `ANTHROPIC_BASE_URL`. A Claude child does not see
`OPENAI_API_KEY`. The `GOOGLE_*` prefix of Antigravity also passes `GOOGLE_CLOUD_*` and
`GOOGLE_APPLICATION_CREDENTIALS`: that is Antigravity's own cloud identity.

## Adding a variable: `env_pass`

An operator lists variable names in `[adapters.<name>]` in the fl config. Each name is copied from
fl's own environment to that CLI's child when the child starts, if fl has it set. The config holds
names, never values.

```toml
# A corporate proxy for Claude Code.
[adapters.claude]
env_pass = ["HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY"]

# Claude Code on Bedrock.
[adapters.claude]
env_pass = ["AWS_PROFILE", "AWS_REGION", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"]

# A workflow step that runs `gh`.
[adapters.codex]
env_pass = ["GH_TOKEN"]
```

Config load refuses a list that:

* holds a name that is not `[A-Za-z_][A-Za-z0-9_]*` or is longer than 128 characters;
* holds more than 64 names;
* holds a name starting with `FL_` or `FERROGATE_`.

A duplicate name is fine. A key other than the ones documented is refused as everywhere in the
config. Pass a secret only to the CLI that needs it: the list is per adapter.

`GITHUB_*` and `GH_*` never pass by inheritance. They pass only when `env_pass` (or code, through
`ProcessHarness::with_env`) names them.

Code can also add `(name, value)` pairs with `ProcessHarness::with_env`. They are applied last and
replace a same-named inherited variable. This is how a per-attempt vendor key is handed to a child.

## What never passes

* Names starting with `FL_` or `FERROGATE_`, by any route: not by inheritance, not through
  `env_pass`, not through `with_env`. This covers `FL_VAULT_MASTER_KEY`, `FL_VAULT_*`, `FL_HUB_*`,
  `FL_WIRE_*` and `FL_GITHUB_*`. The match is case-sensitive; the real protection is that a name
  must be listed to pass at all.
* Names starting with `GITHUB_` or `GH_` by inheritance, even if an allow-list entry would match.
  Only `env_pass` or `with_env` can add them.
* `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` and their lower-case forms. A proxy sees
  the vendor's traffic and credentials. Name one in `env_pass` to opt in.
* `SSH_AUTH_SOCK`. The child has no git-over-ssh through the parent's agent.
* `DISPLAY`, `WAYLAND_DISPLAY`. An unattended attempt starts no browser login.
* `AWS_*`. Claude on Bedrock needs them through `env_pass`.
* Every other variable that is not listed, such as `NPM_TOKEN`, `CARGO_REGISTRY_TOKEN` and
  `npm_config_*`. A variable whose name is not UTF-8 is dropped.

## The hub's probes

The hub runs these non-interactive helpers of its own. Each runs with `env_clear()` and an
allow-listed environment from `fl_exec::child_env`, so none sees `FL_VAULT_MASTER_KEY`, `FL_HUB_*`,
GitHub credentials, another vendor's keys or a proxy.

| Probe | Code | Environment |
| --- | --- | --- |
| `muse usage --json` | `direct_quota.rs` `command_stdout` | baseline + `MUSE_*` |
| `claude auth status --json` | `direct_quota.rs` `command_stdout` | baseline + `ANTHROPIC_*`, `CLAUDE_*` |
| `curl` (Claude OAuth usage) | `direct_quota.rs` `command_stdout` | baseline only |
| `<cli> --version` | `runtime_matrix.rs` `run_version` | baseline + the dialect of the file name (`claude`, `codex`, `agy`, `muse`, `grok`); baseline only for any other name |
| `agent-hydrate <persona> --print --json` | `api.rs` `read_command_stdout` | baseline + `UV_CACHE_DIR`, `UV_PYTHON_INSTALL_DIR`, `UV_PYTHON`, `UV_PROJECT_ENVIRONMENT` (where `uv` keeps its files); no vendor key |

`agent-hydrate` is a `uv run` script that reads the registry under `$HOME`; it needs `PATH` and
`HOME` and nothing else in print mode.

The probes do not read `[adapters.<name>] env_pass`, because the hub does not load adapter config.
A probe runs with the baseline and its dialect's prefixes only. A CLI that needs a proxy to reach
its quota endpoint gets none, and the probe reports no quota (follow-up: pass the adapter's
`env_pass` list to the probes once the hub loads `[adapters.*]`). This is a behaviour change: a
quota probe for a Bedrock or Vertex setup, or for a host behind a proxy, now returns no quota. The binary overrides
`FL_HUB_MUSE_BIN`, `FL_HUB_CLAUDE_BIN` and `FL_HUB_AGENT_HYDRATE_BIN` are read by the hub and are
not passed to the child.

## The wire dispatcher's headless fallbacks

When a recipient has no live tmux window, `crates/wire/src/dispatcher.rs` delivers a message by
starting the vendor CLI from the hub process. All four launches go through `run_vendor_cli`, which
calls `env_clear()` and sets the dialect's environment (`fl_core::child_env::child_env_from` over
the hub's environment) plus the variables the launch sets itself:

| Launch | Dialect | Explicit variables |
| --- | --- | --- |
| `codex queue --thread ... --message ...` (`run_codex_queue`) | Codex | none |
| `muse exec --session-id ... --json` (`run_muse_exec`) | Muse | `MUSE_NO_AUTO_UPDATE=1` |
| `grok --always-approve -r ... -p ...`, then `-s` if the session is not found (`run_grok_headless`) | Grok | none |
| `agy agentapi send-message --title=... ...` (`deliver_to_agy_route`) | Antigravity | `ANTIGRAVITY_CONVERSATION_ID`, `ANTIGRAVITY_AGENTAPI_EXE`, and `ANTIGRAVITY_LS_ADDRESS`, `ANTIGRAVITY_CSRF_TOKEN` when the route or the hub has them |

`grok` runs with `--always-approve`, so it can start any tool; it sees no `FL_*`, `GITHUB_*` or
other vendor's key. The test seams the hub reads (`FL_WIRE_CODEX_BIN`, `FL_WIRE_MUSE_BIN`,
`FL_WIRE_GROK_BIN`, `FL_WIRE_AGY_BIN`, `FL_WIRE_TMUX_BIN`) start with `FL_` and are never passed on.

The wire crate has no adapter config, so these launches have no `env_pass`: they run with the
dialect allow-list and nothing else. A proxy, `AWS_*` or `SSH_AUTH_SOCK` set on the hub does not
reach them (follow-up: the hub loads `[adapters.*].env_pass` and hands it to the dispatcher). This
is a behaviour change for a host where a fallback needed a proxy to reach its vendor.
`DBUS_SESSION_BUS_ADDRESS` is in no baseline or dialect list either, so a vendor CLI that keeps its
credentials in the desktop keyring does not find the session bus when a fallback (or a
`ProcessHarness` spawn) runs (follow-up: reconsider whether the baseline should carry it).

The dispatcher's `tmux` client calls (`list-panes`, `load-buffer`, `paste-buffer`, `send-keys`,
`capture-pane`, and the same in `identity.rs`) are not vendor launches. They keep the hub's
environment: tmux finds its server from `TMUX` and `TMUX_TMPDIR` (which the baseline does not
hold) and `XDG_RUNTIME_DIR` (which it does), and they start no agent code.

## Not covered

These still inherit the hub's whole environment:

* The interactive vendor sessions the hub starts in tmux or a PTY (`terminal.rs`, `lifecycle.rs`).
  They inherit by design: they are the Owner's own session. The wire dispatcher's tmux paste into
  such a session writes text; it starts no process of its own.
* The hub's non-vendor `git` and `openssl` spawns (`github_pr.rs`, `discovery.rs`, `lib.rs`)
  inherit everything. The `git` call in `mcp_api.rs` is already hardened.
* `crates/cli/src/cmd/wire.rs`: the operator-shell hydrate spawn.
* Command gates (`crates/exec/src/command.rs`), which run a project's own programs.
* The hub's workflow-step endpoint (`crates/hub/src/api.rs`) does not read `[adapters.*]`, so
  `env_pass` does not apply to it yet. The same gap holds for the quota and version probes and for
  the wire dispatcher's headless fallbacks above.

The hub's runtime updater (`runtime_update.rs`) clears its environment with its own list, which
keeps the proxy variables; its `--version` check goes through `run_version` and gets the probe
environment above.

## Which session variable names the caller's vendor

`fl-wire` resolves the caller's route (`capture_env_route_from`) and persona
(`resolve_persona_traced`) from the session variables in its environment. Every subshell of an
Antigravity session inherits `ANTIGRAVITY_CONVERSATION_ID`, so an explicit `CODEX_THREAD_ID`,
`MUSE_SESSION_ID`, `GROK_SESSION_ID` or `CLAUDE_SESSION_ID` takes precedence over it. A real Agy
session whose shell also exports one of those resolves as that vendor.
