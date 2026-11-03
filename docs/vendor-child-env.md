# The environment of a vendor CLI child

`fl-exec` starts a vendor agent CLI (`claude`, `codex`, `agy`, `muse`, `grok`) for each attempt.
The child gets an allow-listed environment and nothing it would inherit: the spawn calls
`env_clear()` and then sets the variables below. The hub holds the vault master key, hub and wire
tokens, GitHub credentials and every vendor's API key in its own environment; a vendor CLI, and
every tool it starts, never sees them.

The code is `crates/exec/src/child_env.rs`. `ProcessHarness` calls it for every vendor spawn.

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
`OPENAI_API_KEY`.

The caller can add explicit variables with `ProcessHarness::with_env`. They are applied last and
replace a same-named inherited variable. This is how a per-attempt vendor key is handed to a
child, and how an operator opts in to a setting that is off by default.

## What never passes

* Names starting with `FL_`, `FERROGATE_`, `GITHUB_` or `GH_`, even if an allow-list entry would
  match. This covers `FL_VAULT_MASTER_KEY`, `FL_VAULT_*`, `FL_HUB_*`, `FL_WIRE_*`, `FL_GITHUB_*`,
  `FERROGATE_*`, `GITHUB_TOKEN` and `GH_TOKEN`.
* `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` and their lower-case forms. A proxy sees
  the vendor's traffic and credentials. Pass one with `with_env` to opt in.
* `SSH_AUTH_SOCK`. The child has no git-over-ssh through the parent's agent.
* `DISPLAY`, `WAYLAND_DISPLAY`. An unattended attempt starts no browser login.
* `AWS_*`. Claude on Bedrock needs them through `with_env`.
* Every other variable that is not listed, such as `NPM_TOKEN`, `CARGO_REGISTRY_TOKEN` and
  `npm_config_*`. A variable whose name is not UTF-8 is dropped.

## Not covered

Other processes that start a vendor CLI are not changed by this: the hub's `--version` and quota
probes (`runtime_matrix.rs`, `direct_quota.rs`), the interactive sessions the hub starts in tmux
or a PTY, and command gates (`crates/exec/src/command.rs`, which run a project's own programs).
