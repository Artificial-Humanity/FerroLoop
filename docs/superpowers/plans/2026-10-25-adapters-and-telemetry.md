# Sub-Project 3: Adapters and Telemetry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver five-vendor parity (`claude`, `codex`, `antigravity`, `muse`, `grok`) across `fl-mcp` and `fl-exec`, real token and cost telemetry with optional rate-card fallback, vendor-agnostic `paths_touched` git snapshot diffing, wrapper-driven `RuntimeEntry` liveness heartbeats (`live` / `unverifiable` / `exited`), and rich `fl stats` reporting with Decision 21 regret declaration auditing.

**Architecture:** We keep `FORMAT_WITH_WIRE = 7` unchanged and reuse the existing `Attempt`, `GateRun`, and `RuntimeEntry` schemas. Pure domain types (`AdapterKind`, `RateCard`, `AttemptHeartbeat`, `LivenessStatus`, `UnverifiableReason`, `LivenessVerdict`, `evaluate_liveness`, `RegretChallenge`, `audit_transition_regret`, and `At::unix_millis`) live in `crates/core`. A shared `ProcessHarness` and `GitSnapshot` in `crates/exec/src/adapters/harness.rs` drive five lightweight `CliDialect` implementations (`ClaudeAdapter`, `CodexAdapter`, `AntigravityAdapter`, `MuseAdapter`, `GrokAdapter`) and `AnyAdapter`, while `crates/mcp` adds `Muse` (`.muse/settings.json`) and `Grok` (`.grok/config.toml`) vendor writers and `crates/cli` wires `[adapters.<name>]`, `fl attempt`, `fl agent liveness`, and `fl stats [--json]`.

**Tech Stack:** Rust 2024 edition (`cargo`, `tokio`, `serde`, `serde_json`, `toml`, `toml_edit`, `indexmap`, `sha2`, `clap`, `thiserror`, `anyhow`).

**Spec:** `docs/superpowers/specs/2026-10-25-adapters-and-telemetry-design.md`

## Global Constraints

- **No store format bump:** `FORMAT_WITH_WIRE = 7` remains the current `RedbStore` format version.
- **One wire spelling (`snake_case`):** Every enum crossing a boundary uses `#[serde(rename_all = "snake_case")]` and `wire_names!` / `wire_parse!` where applicable (except `fl-mcp` `VendorName` which uses `#[serde(rename_all = "lowercase")]`).
- **Permission safety (`north-star.md` §4):** No adapter adds `--dangerously-skip-permissions` or `--dangerously-bypass-approvals-and-sandbox`.
- **Self-hosted privacy (`north-star.md` §4):** Nothing is sent home; no vendor environment variable value is ever written into an MCP file (`crates/mcp/src/vendor/` must never call `env::var` or `var_os`).
- **Verification trio before every task completion:**
  - `export PATH="$HOME/.cargo/bin:$PATH" && cargo fmt --all -- --check`
  - `export PATH="$HOME/.cargo/bin:$PATH" && cargo clippy --workspace --all-targets --offline -- -D warnings`
  - `export PATH="$HOME/.cargo/bin:$PATH" && cargo test --workspace --no-fail-fast --offline`
- **Commit co-author trailer:** Every commit must end with `Co-authored-by: Ferris <ferris@artificialhumanity.io>`.

---

## File Structure Map

| File | Action | Responsibility |
| --- | --- | --- |
| `crates/mcp/src/catalog.rs` | Modify | Expand `VendorName` and `VendorName::ALL` from 3 to 5 (`Claude`, `Codex`, `Antigravity`, `Muse`, `Grok`). |
| `crates/mcp/src/vendor/json.rs` | Modify | Store `servers_key: &'static str` on `JsonFile` and add `open_with(path, bytes, servers_key, default_top)`. |
| `crates/mcp/src/vendor/codex.rs` | Modify | Expose `pub(super) fn alone` and `pub(super) fn open` for reuse by `grok.rs`. |
| `crates/mcp/src/vendor/muse.rs` | Create | Meta Muse `.muse/settings.json` (`"schema_version": 1`, `"mcp_servers"`) vendor writer and unit tests. |
| `crates/mcp/src/vendor/grok.rs` | Create | xAI Grok `.grok/config.toml` (`[mcp_servers.<name>]`) vendor writer and unit tests. |
| `crates/mcp/src/vendor/mod.rs` | Modify | Register `Muse` and `Grok` in `vendor(name)` and update vendor unit tests for 5 vendors. |
| `crates/mcp/src/sync.rs` | Modify | Update `value(vendor, name, bytes)` to handle `Muse` (JSON) and `Grok` (TOML) and update unit tests. |
| `docs/mcp.md` | Modify | Document `.muse/settings.json` and `.grok/config.toml` and the 5 `.gitignore` lines. |
| `crates/cli/tests/mcp.rs` | Modify | Update `IGNORED`, `MUSE`, `GROK`, `Secrets::texts()`, and 5-vendor assertions. |
| `crates/core/src/at.rs` | Modify | Add `pub fn unix_millis(&self) -> u64` on `At` and round-trip unit test. |
| `crates/core/src/telemetry.rs` | Create | `AdapterKind`, `RateCard`, `LivenessStatus`, `UnverifiableReason`, `AttemptHeartbeat`, `LivenessVerdict`, `liveness_record_slug`, `liveness_agent_slug`, `evaluate_liveness`, `RegretMetric`, `RegretChallenge`, `audit_transition_regret`, and unit tests. |
| `crates/core/src/lib.rs` | Modify | Export `pub mod telemetry;`. |
| `crates/core/tests/wire_refs.rs` | Modify | Register `AttemptHeartbeat` (`floor = 3`) in `every_reference_field_on_the_wire_is_a_full_iri`. |
| `crates/exec/Cargo.toml` | Modify | Ensure `sha2.workspace = true` is present in `[dependencies]`. |
| `crates/exec/src/adapters/harness.rs` | Create | `GitSnapshot`, `LivenessSink`, and `ProcessHarness` for streaming stdout/stderr, `ETXTBSY` retry, timeout/crash partial telemetry, `paths_touched` delta, and liveness heartbeats. |
| `crates/exec/src/adapters/mod.rs` | Modify | Define `ParsedOutput`, `CliDialect`, `AnyAdapter`, and re-export all 5 adapters. |
| `crates/exec/src/adapters/claude.rs` | Modify | Refactor `ClaudeAdapter` onto `CliDialect` + `ProcessHarness` with JSON/JSONL/plain-text parsing. |
| `crates/exec/src/adapters/codex.rs` | Create | `CodexAdapter` (`codex exec --json --skip-git-repo-check -C <root> <instruction>`) and tests. |
| `crates/exec/src/adapters/antigravity.rs` | Create | `AntigravityAdapter` (`agy -p <instruction> --output-format json`) and tests. |
| `crates/exec/src/adapters/muse.rs` | Create | `MuseAdapter` (`muse exec --json <instruction>`) and tests. |
| `crates/exec/src/adapters/grok.rs` | Create | `GrokAdapter` (`grok -p <instruction> --output-format json`) and tests. |
| `crates/exec/src/lib.rs` | Modify | Re-export `AnyAdapter`, `AntigravityAdapter`, `ClaudeAdapter`, `CodexAdapter`, `GrokAdapter`, `MuseAdapter`. |
| `crates/cli/src/config.rs` | Modify | Add `AdapterConfig` and `pub adapters: BTreeMap<AdapterKind, AdapterConfig>` on `Config`. |
| `crates/cli/src/main.rs` | Modify | Pass `&store` and `&cfg.adapters` into `cmd::attempt::run`. |
| `crates/cli/src/cmd/attempt.rs` | Modify | Support all 5 adapters, `[adapters.<name>]` binary/rate card, and `LivenessSink` writing `RuntimeEntry`. |
| `crates/cli/src/cmd/agent.rs` | Modify | Add `fl agent liveness [<target>] [--stale-after-ms <ms>] [--json]`. |
| `crates/cli/src/cmd/stats.rs` | Modify | Add `tokens in`, `tokens out`, partial-floor note, `by adapter:`, `audit_transition_regret`, and `--json`. |
| `crates/cli/tests/attempt.rs` | Modify | Add end-to-end tests for all 5 adapters, rate cards, `paths_touched`, `fl agent liveness`, and `fl stats [--json]`. |

---

### Task 1: Five-Vendor MCP Catalog (`crates/mcp`, `docs/mcp.md`, and `crates/cli/tests/mcp.rs`)

**Files:**
- Modify: `crates/mcp/src/catalog.rs`
- Modify: `crates/mcp/src/vendor/json.rs`
- Modify: `crates/mcp/src/vendor/codex.rs`
- Create: `crates/mcp/src/vendor/muse.rs`
- Create: `crates/mcp/src/vendor/grok.rs`
- Modify: `crates/mcp/src/vendor/mod.rs`
- Modify: `crates/mcp/src/sync.rs`
- Modify: `docs/mcp.md`
- Modify: `crates/cli/tests/mcp.rs`

**Interfaces:**
- Consumes: existing `crates/mcp::catalog::{Server, Transport, EnvValue, HeaderValue}` and `crates/mcp::vendor::{Vendor, VendorFile, Rendered, VendorRefusal, FileRefusal}`.
- Produces:
  - `VendorName::{Claude, Codex, Antigravity, Muse, Grok}` and `VendorName::ALL: [VendorName; 5]` in `crates/mcp/src/catalog.rs`.
  - `pub use muse::Muse;` and `pub use grok::Grok;` in `crates/mcp/src/vendor/mod.rs`.
  - `json::open_with(path: &Path, bytes: Option<&[u8]>, servers_key: &'static str, default_top: &[(&'static str, &'static str)]) -> Result<JsonFile, FileRefusal>` in `crates/mcp/src/vendor/json.rs`.

- [ ] **Step 1: Expand `VendorName` in `crates/mcp/src/catalog.rs`**

Update `VendorName` in `crates/mcp/src/catalog.rs` (lines 77–99) and any unit tests in `crates/mcp/src/catalog.rs` that assert the list of vendor names:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VendorName {
    Claude,
    Codex,
    Antigravity,
    Muse,
    Grok,
}

impl VendorName {
    pub const ALL: [VendorName; 5] = [
        VendorName::Claude,
        VendorName::Codex,
        VendorName::Antigravity,
        VendorName::Muse,
        VendorName::Grok,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            VendorName::Claude => "claude",
            VendorName::Codex => "codex",
            VendorName::Antigravity => "antigravity",
            VendorName::Muse => "muse",
            VendorName::Grok => "grok",
        }
    }
}
```

Also check `crates/mcp/src/catalog.rs` for any error strings or tests listing `"claude, codex or antigravity"` and update them if needed (or keep existing error message format if generated from `VendorName::ALL`).

- [ ] **Step 2: Parameterize `JsonFile` in `crates/mcp/src/vendor/json.rs` and expose `codex` helpers in `crates/mcp/src/vendor/codex.rs`**

In `crates/mcp/src/vendor/json.rs`, update `JsonFile` to store `servers_key: &'static str` and add `open_with`:

```rust
#[derive(Debug, Clone)]
pub(super) struct JsonFile {
    servers_key: &'static str,
    /// The top level; `servers_key`, when present, holds its old bytes, and
    /// `servers` stands in for it.
    top: IndexMap<String, Box<RawValue>>,
    /// `None` when the file has no `servers_key` and fl has written nothing.
    servers: Option<IndexMap<String, Box<RawValue>>>,
}

pub(super) fn open(path: &Path, bytes: Option<&[u8]>) -> Result<JsonFile, FileRefusal> {
    open_with(path, bytes, SERVERS, &[])
}

pub(super) fn open_with(
    path: &Path,
    bytes: Option<&[u8]>,
    servers_key: &'static str,
    default_top: &[(&'static str, &'static str)],
) -> Result<JsonFile, FileRefusal> {
    let Some(bytes) = bytes else {
        let mut top = IndexMap::new();
        for &(k, raw_json) in default_top {
            let raw = RawValue::from_string(raw_json.to_string()).expect("valid default JSON");
            top.insert(k.to_string(), raw);
        }
        let mut file = JsonFile {
            servers_key,
            top,
            servers: None,
        };
        file.servers_mut();
        return Ok(file);
    };
    let refuse = |problem: String, next: &str| FileRefusal {
        path: path.to_path_buf(),
        problem,
        next: next.to_string(),
    };
    let not_strict = |cause: String| {
        refuse(
            format!(
                "it is not strict JSON ({cause}), so fl will not rewrite it and lose its \
                 comments, trailing commas or byte-order mark"
            ),
            "Remove them by hand, then run `fl mcp sync` again",
        )
    };
    let text = std::str::from_utf8(bytes).map_err(|e| not_strict(e.to_string()))?;
    let value: &RawValue = serde_json::from_str(text).map_err(|e| not_strict(e.to_string()))?;
    if !value.get().starts_with('{') {
        return Err(refuse(
            "its top level is not a JSON object".into(),
            FIX_BY_HAND,
        ));
    }
    let top: Members = serde_json::from_str(text).expect("a strict JSON object");
    if let Some(key) = top.twice {
        let problem = format!("the key `{key}` appears twice at the top level");
        return Err(refuse(problem, FIX_BY_HAND));
    }
    let servers = match top.members.get(servers_key) {
        None => None,
        Some(raw) if !raw.get().starts_with('{') => {
            return Err(refuse(
                format!("`{servers_key}` is not a JSON object"),
                FIX_BY_HAND,
            ));
        }
        Some(raw) => {
            let servers: Members = serde_json::from_str(raw.get()).expect("a strict JSON object");
            if let Some(name) = servers.twice {
                let problem = format!("the server `{name}` appears twice in `{servers_key}`");
                return Err(refuse(problem, FIX_BY_HAND));
            }
            Some(servers.members)
        }
    };
    Ok(JsonFile {
        servers_key,
        top: top.members,
        servers,
    })
}
```

And in `impl JsonFile`, replace `SERVERS` with `self.servers_key` in `servers_mut` and `to_bytes`:

```rust
    fn servers_mut(&mut self) -> &mut IndexMap<String, Box<RawValue>> {
        if self.servers.is_none() {
            let empty = RawValue::from_string("{}".into()).expect("valid JSON");
            self.top.insert(self.servers_key.into(), empty);
        }
        self.servers.get_or_insert_default()
    }
```

```rust
    pub(super) fn to_bytes(&self) -> Vec<u8> {
        let top = self.top.iter().map(|(key, raw)| match &self.servers {
            Some(servers) if key == self.servers_key => {
                let servers = servers.iter().map(|(k, v)| (k, v.get().to_string()));
                (key, object(servers, ENTRY_INDENT))
            }
            _ => (key, raw.get().to_string()),
        });
        let mut out = object(top, "  ");
        out.push('\n');
        out.into_bytes()
    }
```

In `crates/mcp/src/vendor/codex.rs`, change `fn alone` and `fn open` visibility to `pub(super) fn alone` and `pub(super) fn open` so `grok.rs` can call them directly.

- [ ] **Step 3: Create `crates/mcp/src/vendor/muse.rs` and `crates/mcp/src/vendor/grok.rs`**

Create `crates/mcp/src/vendor/muse.rs`:

```rust
//! Meta Muse's `<root>/.muse/settings.json`. Muse reads MCP servers from the
//! `"mcp_servers"` object in `.muse/settings.json` (with `"schema_version": 1`
//! on newly created files) and expands `${NAME}` in `env` and in headers.

use super::{Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, json, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::Path;

const SERVERS: &str = "mcp_servers";
const DEFAULT_TOP: &[(&str, &str)] = &[("schema_version", "1")];

pub struct Muse;

impl Vendor for Muse {
    fn name(&self) -> VendorName {
        VendorName::Muse
    }

    fn title(&self) -> &'static str {
        "Muse"
    }

    fn target(&self) -> &'static str {
        ".muse/settings.json"
    }

    fn render(&self, _name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let env = env(server).map(|(key, value)| {
                    let value = match value {
                        EnvValue::Literal(v) => v.clone(),
                        EnvValue::Secret { .. } => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            format!("${{{var}}}")
                        }
                    };
                    (key.clone(), value)
                });
                fields.map("env", env.collect());
            }
            Transport::Http | Transport::Sse => {
                fields.text("type", server.transport.as_str());
                fields.text("url", url(server));
                let headers = headers(server).map(|(name, value)| {
                    let value = match value {
                        HeaderValue::Literal(v) => v.clone(),
                        HeaderValue::Secret { env, scheme: None } => format!("${{{env}}}"),
                        HeaderValue::Secret {
                            env,
                            scheme: Some(scheme),
                        } => format!("{scheme} ${{{env}}}"),
                    };
                    (name.clone(), value)
                });
                fields.map("headers", headers.collect());
            }
        }
        Ok(json::render(&fields.0))
    }

    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        let kind = Kind::Json(json::open_with(path, bytes, SERVERS, DEFAULT_TOP)?);
        Ok(VendorFile { kind })
    }
}

#[cfg(test)]
mod tests {
    use super::Muse;
    use crate::vendor::fixtures::*;

    #[test]
    fn a_stdio_server_writes_schema_version_and_mcp_servers_with_secret_reference() {
        assert_eq!(
            file_with(&Muse, "files", STDIO),
            r#"{
  "schema_version": 1,
  "mcp_servers": {
    "files": {
      "command": "npx",
      "args": [
        "-y",
        "@example/files-mcp@1.4.2"
      ],
      "env": {
        "FILES_TOKEN": "${FILES_TOKEN}",
        "LOG_LEVEL": "debug"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn a_secret_under_another_name_reads_that_variable() {
        assert_eq!(
            file_with(&Muse, "gh", RENAMED),
            r#"{
  "schema_version": 1,
  "mcp_servers": {
    "gh": {
      "command": "files-mcp",
      "env": {
        "GITHUB_TOKEN": "${GH_PAT}"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn an_http_remote_carries_its_type_and_a_secret_bearer_header_as_a_reference() {
        assert_eq!(
            file_with(&Muse, "docs", BEARER),
            r#"{
  "schema_version": 1,
  "mcp_servers": {
    "docs": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${DOCS_TOKEN}",
        "X-Client": "fl"
      }
    }
  }
}
"#
        );
    }
}
```

Create `crates/mcp/src/vendor/grok.rs`:

```rust
//! Grok's `<root>/.grok/config.toml`, edited with `toml_edit` so that its
//! comments and layout survive. Each server is a `[mcp_servers.<name>]` table.

use super::codex::{alone, open};
use super::{Field, Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, refuse, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::Path;
use toml_edit::{Array, InlineTable, Item, Table, Value};

pub struct Grok;

impl Vendor for Grok {
    fn name(&self) -> VendorName {
        VendorName::Grok
    }

    fn title(&self) -> &'static str {
        "Grok"
    }

    fn target(&self) -> &'static str {
        ".grok/config.toml"
    }

    fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        let refuse = |reason: String| refuse(VendorName::Grok, name, reason);
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let (mut literals, mut forwarded) = (Vec::new(), Vec::new());
                for (key, value) in env(server) {
                    match value {
                        EnvValue::Literal(v) => literals.push((key.clone(), v.clone())),
                        EnvValue::Secret { .. } => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            if var != key {
                                return Err(refuse(format!(
                                    "`env.{key}` reads the variable `{var}`, and Grok forwards \
                                     a secret variable under its own name"
                                )));
                            }
                            forwarded.push(var.to_string());
                        }
                    }
                }
                fields.map("env", literals);
                fields.list("env_vars", forwarded);
            }
            Transport::Sse => {
                return Err(refuse(
                    "Grok connects to streamable HTTP servers only, not SSE".into(),
                ));
            }
            Transport::Http => {
                fields.text("url", url(server));
                let (mut literals, mut from_env, mut bearer) = (Vec::new(), Vec::new(), None);
                for (header, value) in headers(server) {
                    match value {
                        HeaderValue::Literal(v) => literals.push((header.clone(), v.clone())),
                        HeaderValue::Secret { env, scheme: None } => {
                            from_env.push((header.clone(), env.clone()));
                        }
                        HeaderValue::Secret {
                            env,
                            scheme: Some(scheme),
                        } if header.eq_ignore_ascii_case("authorization")
                            && scheme.eq_ignore_ascii_case("bearer") =>
                        {
                            bearer = Some(env);
                        }
                        HeaderValue::Secret {
                            scheme: Some(scheme),
                            ..
                        } => {
                            return Err(refuse(format!(
                                "the secret header `{header}` has the scheme `{scheme}`, and \
                                 Grok sends a secret header either whole or as \
                                 `Authorization: Bearer`"
                            )));
                        }
                    }
                }
                if let Some(env) = bearer {
                    fields.text("bearer_token_env_var", env);
                }
                fields.map("http_headers", literals);
                fields.map("env_http_headers", from_env);
            }
        }
        let mut table = Table::new();
        for (key, field) in fields.0 {
            let value = match field {
                Field::Text(s) => Value::from(s),
                Field::List(values) => Value::Array(values.into_iter().collect::<Array>()),
                Field::Map(values) => {
                    Value::InlineTable(values.into_iter().collect::<InlineTable>())
                }
            };
            table.insert(key, Item::Value(value));
        }
        Ok(Rendered {
            text: alone(name, &Item::Table(table)),
        })
    }

    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        let kind = Kind::Toml(open(path, bytes)?);
        Ok(VendorFile { kind })
    }
}

#[cfg(test)]
mod tests {
    use super::Grok;
    use crate::vendor::fixtures::*;

    #[test]
    fn a_stdio_server_forwards_a_secret_by_name_and_writes_literals_only() {
        assert_eq!(
            file_with(&Grok, "files", STDIO),
            r#"[mcp_servers.files]
command = "npx"
args = ["-y", "@example/files-mcp@1.4.2"]
env = { LOG_LEVEL = "debug" }
env_vars = ["FILES_TOKEN"]
"#
        );
    }

    #[test]
    fn a_renamed_secret_is_refused_for_grok() {
        assert_eq!(
            refusal(&Grok, RENAMED),
            "`env.GITHUB_TOKEN` reads the variable `GH_PAT`, and Grok forwards a secret variable \
             under its own name"
        );
    }

    #[test]
    fn an_sse_remote_is_refused_for_grok() {
        assert_eq!(
            refusal(&Grok, SSE),
            "Grok connects to streamable HTTP servers only, not SSE"
        );
    }
}
```

- [ ] **Step 4: Register `Muse` and `Grok` in `crates/mcp/src/vendor/mod.rs` and `crates/mcp/src/sync.rs`, and update tests + `docs/mcp.md`**

In `crates/mcp/src/vendor/mod.rs`:
1. Add `mod grok;` and `mod muse;`, `pub use grok::Grok;`, `pub use muse::Muse;`.
2. Update `vendor(name: VendorName)`:
   ```rust
   pub fn vendor(name: VendorName) -> &'static dyn Vendor {
       match name {
           VendorName::Claude => &Claude,
           VendorName::Codex => &Codex,
           VendorName::Antigravity => &Antigravity,
           VendorName::Muse => &Muse,
           VendorName::Grok => &Grok,
       }
   }
   ```
3. In `crates/mcp/src/vendor/mod.rs` `mod tests`:
   - Update `each_vendor_writes_its_own_project_file` to include `(VendorName::Muse, "Muse", ".muse/settings.json")` and `(VendorName::Grok, "Grok", ".grok/config.toml")`.
   - In `an_entry_fl_writes_reads_back_as_the_bytes_it_rendered` and `removing_what_was_added_leaves_the_file_as_it_was`, match `VendorName::Codex | VendorName::Grok` for TOML, `VendorName::Muse` for `"mcp_servers"`, and `VendorName::Claude | VendorName::Antigravity` for `"mcpServers"`.
   - In `a_secret_value_in_the_environment_reaches_no_rendered_entry`, update `assert_eq!(written.len(), 9)` (5 stdio + 4 http) and add `include_str!("muse.rs")` and `include_str!("grok.rs")` to the source scan array.

In `crates/mcp/src/sync.rs`:
- Update `value(vendor: VendorName, name: &str, bytes: &[u8])`:
  ```rust
  fn value(vendor: VendorName, name: &str, bytes: &[u8]) -> Option<serde_json::Value> {
      match vendor {
          VendorName::Codex | VendorName::Grok => {
              let doc: toml::Table = toml::from_str(std::str::from_utf8(bytes).ok()?).ok()?;
              let entry = doc.get("mcp_servers")?.get(name)?;
              serde_json::to_value(entry).ok()
          }
          VendorName::Claude | VendorName::Antigravity | VendorName::Muse => {
              serde_json::from_slice(bytes).ok()
          }
      }
  }
  ```
- Update any unit tests in `crates/mcp/src/sync.rs` and `crates/cli/tests/mcp.rs` that assert 3 vendor targets/records (`IGNORED` in `crates/cli/tests/mcp.rs` adds `/.muse/settings.json\n/.grok/config.toml\n`, `MUSE = ".muse/settings.json"`, `GROK = ".grok/config.toml"` added to `Secrets::texts()` and the project directory allowlist, and vendor record counts updated from `3` to `5`).
- Update `docs/mcp.md` to list `.muse/settings.json` and `.grok/config.toml` alongside `.mcp.json`, `.codex/config.toml`, and `.agents/mcp_config.json`.

- [ ] **Step 5: Run tests and commit Task 1**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p fl-mcp --offline
cargo test -p fl-cli --test mcp --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
git add crates/mcp crates/cli/tests/mcp.rs docs/mcp.md
git commit -m "feat(mcp): add Meta Muse and xAI Grok vendor writers

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

### Task 2: Pure Telemetry, Rate Card, Liveness, and Regret Audit (`crates/core`)

**Files:**
- Modify: `crates/core/src/at.rs`
- Create: `crates/core/src/telemetry.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/core/tests/wire_refs.rs`

**Interfaces:**
- Consumes: `At` (`crates/core/src/at.rs`), `Iri`, `ProjectId`, `RecordId` (`crates/core/src/ids.rs`), `AttemptStatus`, `GateRun` (`crates/core/src/log.rs`), `Regret`, `State`, `Transition` (`crates/core/src/model.rs`).
- Produces:
  - `At::unix_millis(&self) -> u64` in `crates/core/src/at.rs`.
  - `AdapterKind`, `RateCard`, `DEFAULT_STALE_AFTER_MS`, `LivenessStatus`, `UnverifiableReason`, `AttemptHeartbeat`, `LivenessVerdict`, `liveness_record_slug`, `liveness_agent_slug`, `evaluate_liveness`, `REGRET_AUDIT_MULTIPLIER`, `REGRET_AUDIT_MIN_COST_MICROS`, `REGRET_AUDIT_MIN_DURATION_MS`, `RegretMetric`, `RegretChallenge`, and `audit_transition_regret` in `crates/core/src/telemetry.rs`.

- [ ] **Step 1: Add `At::unix_millis(&self) -> u64` in `crates/core/src/at.rs`**

In `crates/core/src/at.rs`, extract a helper `fn raw_millis(s: &str) -> i64` used by both `At::parse` and `At::unix_millis`:

```rust
fn raw_millis(s: &str) -> i64 {
    let n = |from: usize, to: usize| -> i64 {
        s[from..to].parse().expect("checked: 24-byte ASCII RFC 3339")
    };
    let days = days_from_civil(n(0, 4), n(5, 7), n(8, 10));
    days * 86_400_000
        + n(11, 13) * 3_600_000
        + n(14, 16) * 60_000
        + n(17, 19) * 1000
        + n(20, 23)
}
```

And add on `impl At`:

```rust
    /// Milliseconds after the Unix epoch for this timestamp.
    pub fn unix_millis(&self) -> u64 {
        raw_millis(&self.0) as u64
    }
```

Add a unit test `unix_millis_round_trips_with_from_unix_millis` in `crates/core/src/at.rs`.

- [ ] **Step 2: Create `crates/core/src/telemetry.rs` with full unit tests**

Create `crates/core/src/telemetry.rs`:

```rust
//! Adapter kinds, token rate cards, wrapper liveness heartbeats and evaluation,
//! and Decision 21 transition regret declaration auditing.

use crate::at::At;
use crate::ids::{ProjectId, RecordId};
use crate::iri::Iri;
use crate::log::{AttemptStatus, GateRun};
use crate::model::{Regret, State, Transition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_STALE_AFTER_MS: u64 = 30_000;
pub const REGRET_AUDIT_MULTIPLIER: u64 = 10;
pub const REGRET_AUDIT_MIN_COST_MICROS: u64 = 1_000;
pub const REGRET_AUDIT_MIN_DURATION_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Claude,
    Codex,
    Antigravity,
    Muse,
    Grok,
}

crate::wire::wire_names!(AdapterKind as adapter_kind_wire {
    Claude => "claude",
    Codex => "codex",
    Antigravity => "antigravity",
    Muse => "muse",
    Grok => "grok",
});
crate::wire::wire_parse!(AdapterKind as adapter_kind_parse);

impl AdapterKind {
    pub const fn default_binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "agy",
            Self::Muse => "muse",
            Self::Grok => "grok",
        }
    }
}

/// Optional per-adapter token pricing in micro-USD per 1,000,000 tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateCard {
    #[serde(default)]
    pub input_usd_micros_per_mtok: u64,
    #[serde(default)]
    pub output_usd_micros_per_mtok: u64,
}

impl RateCard {
    /// Compute cost in micro-USD when vendor-reported cost is absent or zero.
    /// Uses u128 intermediate math to prevent overflow.
    pub fn resolve_cost(
        self,
        tokens_in: u64,
        tokens_out: u64,
        vendor_cost_usd_micros: Option<u64>,
    ) -> u64 {
        if let Some(c) = vendor_cost_usd_micros
            && c > 0
        {
            return c;
        }
        if self.input_usd_micros_per_mtok == 0 && self.output_usd_micros_per_mtok == 0 {
            return 0;
        }
        let total = (tokens_in as u128)
            .saturating_mul(self.input_usd_micros_per_mtok as u128)
            .saturating_add(
                (tokens_out as u128).saturating_mul(self.output_usd_micros_per_mtok as u128),
            )
            / 1_000_000;
        u64::try_from(total).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LivenessStatus {
    Live,
    Unverifiable,
    Exited,
}

crate::wire::wire_names!(LivenessStatus as liveness_status_wire {
    Live => "live",
    Unverifiable => "unverifiable",
    Exited => "exited",
});
crate::wire::wire_parse!(LivenessStatus as liveness_status_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnverifiableReason {
    MissingStatus,
    StaleStatus,
    HostUnavailable,
}

crate::wire::wire_names!(UnverifiableReason as unverifiable_reason_wire {
    MissingStatus => "missing_status",
    StaleStatus => "stale_status",
    HostUnavailable => "host_unavailable",
});
crate::wire::wire_parse!(UnverifiableReason as unverifiable_reason_parse);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptHeartbeat {
    pub attempt_id: Iri,
    pub project: ProjectId,
    pub record: RecordId,
    pub adapter: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub host: String,
    pub pid: u32,
    pub started_at: At,
    pub heartbeat_at: At,
    pub status: LivenessStatus,
    #[serde(default)]
    pub outcome: Option<AttemptStatus>,
    #[serde(default)]
    pub exit_at: Option<At>,
    pub duration_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LivenessVerdict {
    Live {
        pid: u32,
        host: String,
        heartbeat_at: At,
        duration_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd_micros: u64,
    },
    Unverifiable {
        reason: UnverifiableReason,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        heartbeat_at: Option<At>,
    },
    Exited {
        outcome: AttemptStatus,
        #[serde(default)]
        exit_at: Option<At>,
        duration_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd_micros: u64,
    },
}

fn sanitize_slug_tail(raw: &str, max_len: usize) -> Option<String> {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' {
            out.push(lower);
            last_dash = false;
        } else if !out.is_empty() && !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() || out.len() > max_len {
        None
    } else {
        Some(out)
    }
}

fn sha256_hex_36(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex.truncate(36);
    hex
}

/// Deterministic `StateEntry.key` for a record's liveness entry, guaranteed to
/// pass `validate_slug` (`^[a-z][a-z0-9_-]{0,63}$`).
pub fn liveness_record_slug(record: &RecordId) -> String {
    let raw = record.iri().as_str();
    let stripped = raw.strip_prefix("urn:uuid:").unwrap_or(raw);
    match sanitize_slug_tail(stripped, 47) {
        Some(tail) => format!("liveness-record-{tail}"),
        None => format!("liveness-record-{}", sha256_hex_36(raw)),
    }
}

/// Deterministic `StateEntry.key` for an agent's liveness entry, guaranteed to
/// pass `validate_slug` (`^[a-z][a-z0-9_-]{0,63}$`).
pub fn liveness_agent_slug(persona: &str) -> String {
    match sanitize_slug_tail(persona.trim(), 48) {
        Some(tail) => format!("liveness-agent-{tail}"),
        None => format!("liveness-agent-{}", sha256_hex_36(persona.trim())),
    }
}

/// Evaluate an optional `AttemptHeartbeat` into the three-state liveness vocabulary.
pub fn evaluate_liveness<F>(
    hb: Option<&AttemptHeartbeat>,
    now: &At,
    stale_after_ms: u64,
    local_host: &str,
    pid_is_alive: F,
) -> LivenessVerdict
where
    F: Fn(u32) -> bool,
{
    let Some(hb) = hb else {
        return LivenessVerdict::Unverifiable {
            reason: UnverifiableReason::MissingStatus,
            host: None,
            heartbeat_at: None,
        };
    };
    match hb.status {
        LivenessStatus::Exited => LivenessVerdict::Exited {
            outcome: hb.outcome.unwrap_or(AttemptStatus::Crashed),
            exit_at: hb.exit_at.clone(),
            duration_ms: hb.duration_ms,
            tokens_in: hb.tokens_in,
            tokens_out: hb.tokens_out,
            cost_usd_micros: hb.cost_usd_micros,
        },
        LivenessStatus::Unverifiable => LivenessVerdict::Unverifiable {
            reason: UnverifiableReason::StaleStatus,
            host: Some(hb.host.clone()),
            heartbeat_at: Some(hb.heartbeat_at.clone()),
        },
        LivenessStatus::Live => {
            let age_ms = now
                .unix_millis()
                .saturating_sub(hb.heartbeat_at.unix_millis());
            if hb.host == local_host {
                if hb.pid == 0 || !pid_is_alive(hb.pid) {
                    return LivenessVerdict::Exited {
                        outcome: hb.outcome.unwrap_or(AttemptStatus::Crashed),
                        exit_at: None,
                        duration_ms: hb.duration_ms,
                        tokens_in: hb.tokens_in,
                        tokens_out: hb.tokens_out,
                        cost_usd_micros: hb.cost_usd_micros,
                    };
                }
                if age_ms <= stale_after_ms {
                    LivenessVerdict::Live {
                        pid: hb.pid,
                        host: hb.host.clone(),
                        heartbeat_at: hb.heartbeat_at.clone(),
                        duration_ms: hb.duration_ms,
                        tokens_in: hb.tokens_in,
                        tokens_out: hb.tokens_out,
                        cost_usd_micros: hb.cost_usd_micros,
                    }
                } else {
                    LivenessVerdict::Unverifiable {
                        reason: UnverifiableReason::StaleStatus,
                        host: Some(hb.host.clone()),
                        heartbeat_at: Some(hb.heartbeat_at.clone()),
                    }
                }
            } else if age_ms <= stale_after_ms {
                LivenessVerdict::Live {
                    pid: hb.pid,
                    host: hb.host.clone(),
                    heartbeat_at: hb.heartbeat_at.clone(),
                    duration_ms: hb.duration_ms,
                    tokens_in: hb.tokens_in,
                    tokens_out: hb.tokens_out,
                    cost_usd_micros: hb.cost_usd_micros,
                }
            } else {
                LivenessVerdict::Unverifiable {
                    reason: UnverifiableReason::HostUnavailable,
                    host: Some(hb.host.clone()),
                    heartbeat_at: Some(hb.heartbeat_at.clone()),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegretMetric {
    CostUsdMicros,
    DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegretChallenge {
    pub name: String,
    pub from: State,
    pub to: State,
    pub metric: RegretMetric,
    pub measured_mean: u64,
    pub project_median: u64,
    pub ratio: u64,
}

/// Decision 21 advisory regret declaration audit.
pub fn audit_transition_regret(
    transitions: &[Transition],
    gate_runs: &[GateRun],
) -> Vec<RegretChallenge> {
    struct Measured<'a> {
        transition: &'a Transition,
        mean_cost: u64,
        mean_duration: u64,
    }

    let mut measured = Vec::new();
    for t in transitions {
        if t.gates.is_empty() {
            continue;
        }
        let mut count = 0u64;
        let mut sum_cost = 0u128;
        let mut sum_dur = 0u128;
        for run in gate_runs {
            if t.gates.contains(&run.gate) {
                count += 1;
                sum_cost += run.cost_usd_micros as u128;
                sum_dur += run.duration_ms as u128;
            }
        }
        if count == 0 {
            continue;
        }
        measured.push(Measured {
            transition: t,
            mean_cost: u64::try_from(sum_cost / count as u128).unwrap_or(u64::MAX),
            mean_duration: u64::try_from(sum_dur / count as u128).unwrap_or(u64::MAX),
        });
    }

    if measured.len() < 2 {
        return Vec::new();
    }

    let non_zero_cost_count = measured.iter().filter(|m| m.mean_cost > 0).count();
    let (metric, floor) = if non_zero_cost_count >= 2 {
        (RegretMetric::CostUsdMicros, REGRET_AUDIT_MIN_COST_MICROS)
    } else {
        (RegretMetric::DurationMs, REGRET_AUDIT_MIN_DURATION_MS)
    };

    let mut values: Vec<u64> = measured
        .iter()
        .map(|m| match metric {
            RegretMetric::CostUsdMicros => m.mean_cost,
            RegretMetric::DurationMs => m.mean_duration,
        })
        .collect();
    values.sort_unstable();
    let mid = values.len() / 2;
    let project_median = if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2
    };
    if project_median == 0 {
        return Vec::new();
    }

    let mut challenges = Vec::new();
    for m in &measured {
        if m.transition.regret != Regret::Low {
            continue;
        }
        let val = match metric {
            RegretMetric::CostUsdMicros => m.mean_cost,
            RegretMetric::DurationMs => m.mean_duration,
        };
        if val >= floor && val >= project_median.saturating_mul(REGRET_AUDIT_MULTIPLIER) {
            challenges.push(RegretChallenge {
                name: m.transition.name.clone(),
                from: m.transition.from,
                to: m.transition.to,
                metric,
                measured_mean: val,
                project_median,
                ratio: val / project_median,
            });
        }
    }
    challenges.sort_by(|a, b| {
        (a.from.as_wire(), a.to.as_wire(), a.name.as_str()).cmp(&(
            b.from.as_wire(),
            b.to.as_wire(),
            b.name.as_str(),
        ))
    });
    challenges
}
```

Include unit tests in `crates/core/src/telemetry.rs` covering `AdapterKind` wire names, `RateCard::resolve_cost`, `liveness_record_slug` and `liveness_agent_slug` (asserting `crate::state::validate_slug(&slug).is_ok()`), `evaluate_liveness` (all 6 branches), and `audit_transition_regret` (both duration and cost metrics, low vs high regret, and noise floor).

- [ ] **Step 3: Export `telemetry` in `crates/core/src/lib.rs` and add `AttemptHeartbeat` to `crates/core/tests/wire_refs.rs`**

In `crates/core/src/lib.rs`, add `pub mod telemetry;`.
In `crates/core/tests/wire_refs.rs`, add `AttemptHeartbeat` with `floor = 3` inside `every_reference_field_on_the_wire_is_a_full_iri`:

```rust
    assert_every_reference_refuses_a_handle(
        &fl_core::telemetry::AttemptHeartbeat {
            attempt_id: seq_iri(1),
            project: ProjectId(seq_iri(2)),
            record: RecordId(seq_iri(3)),
            adapter: "claude".into(),
            agent: Some("ferris".into()),
            host: "ai-lab-0".into(),
            pid: 1234,
            started_at: At::from_unix_millis(1_000),
            heartbeat_at: At::from_unix_millis(2_000),
            status: fl_core::telemetry::LivenessStatus::Live,
            outcome: None,
            exit_at: None,
            duration_ms: 1_000,
            tokens_in: 10,
            tokens_out: 20,
            cost_usd_micros: 50,
        },
        3,
    );
```

- [ ] **Step 4: Run tests and commit Task 2**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p fl-core --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
git add crates/core
git commit -m "feat(core): add telemetry, rate card, liveness, and regret audit types

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```


---

## Task 3: Shared `ProcessHarness`, `GitSnapshot`, `LivenessSink`, and `ClaudeAdapter` Refactor (`crates/exec`)

**Files:**
- Create: `crates/exec/src/adapters/harness.rs`
- Modify: `crates/exec/src/adapters/claude.rs`
- Modify: `crates/exec/src/adapters/mod.rs`

- [ ] **Step 1: Create `crates/exec/src/adapters/harness.rs` with `CliDialect`, `ParsedOutput`, `GitSnapshot`, `LivenessSink`, and `ProcessHarness`**

Write `crates/exec/src/adapters/harness.rs`:

```rust
//! Shared process harness, pre/post git snapshot delta, and wrapper liveness emitter.

use crate::runner::{AttemptOutcome, AttemptSpec};
use fl_core::at::At;
use fl_core::ids::{Iri, ProjectId};
use fl_core::log::AttemptStatus;
use fl_core::telemetry::{AdapterKind, AttemptHeartbeat, LivenessStatus, RateCard};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

pub const EXCERPT_LIMIT: usize = 8192;
const SPAWN_RETRY_LIMIT: u32 = 20;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(2);
const HEARTBEAT_TICK: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedOutput {
    pub excerpt_text: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub vendor_cost_usd_micros: Option<u64>,
}

pub trait CliDialect: Send + Sync {
    fn kind(&self) -> AdapterKind;
    fn default_binary(&self) -> &'static str;
    fn argv(&self, spec: &AttemptSpec) -> Vec<String>;
    fn parse_output(&self, stdout: &[u8], stderr: &[u8]) -> ParsedOutput;
}

pub type LivenessSink = Arc<dyn Fn(&AttemptHeartbeat) + Send + Sync>;

pub struct ProcessHarness<D: CliDialect> {
    pub binary: String,
    pub dialect: D,
    pub rate_card: Option<RateCard>,
    pub liveness_sink: Option<LivenessSink>,
    pub project: Option<ProjectId>,
    pub agent: Option<String>,
    pub host: String,
}

impl<D: CliDialect> ProcessHarness<D> {
    pub fn new(binary: String, dialect: D) -> Self {
        Self {
            binary,
            dialect,
            rate_card: None,
            liveness_sink: None,
            project: None,
            agent: None,
            host: detect_host(),
        }
    }

    pub fn with_rate_card(mut self, rate_card: Option<RateCard>) -> Self {
        self.rate_card = rate_card;
        self
    }

    pub fn with_liveness(
        mut self,
        project: ProjectId,
        agent: Option<String>,
        sink: LivenessSink,
    ) -> Self {
        self.project = Some(project);
        self.agent = agent;
        self.liveness_sink = Some(sink);
        self
    }

    pub async fn run(&self, spec: AttemptSpec) -> AttemptOutcome {
        if spec.budget_usd_micros == 0 {
            return AttemptOutcome::refused(
                "the attempt's spend ceiling is zero, so nothing was started",
            );
        }

        let snap_before = GitSnapshot::capture(&spec.project_root).await;
        let started_wall = now_at();
        let started = Instant::now();

        let mut cmd = Command::new(&self.binary);
        cmd.args(self.dialect.argv(&spec))
            .current_dir(&spec.project_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut retries = 0u32;
        let mut child = loop {
            match cmd.spawn() {
                Ok(c) => break c,
                Err(e)
                    if e.kind() == ErrorKind::ExecutableFileBusy && retries < SPAWN_RETRY_LIMIT =>
                {
                    retries += 1;
                    tokio::time::sleep(SPAWN_RETRY_DELAY).await;
                }
                Err(e) => {
                    return AttemptOutcome::refused(format!(
                        "could not start `{}`: {e}",
                        self.binary
                    ));
                }
            }
        };

        let pid = child.id().unwrap_or(0);
        let attempt_id = Iri::parse(format!("urn:uuid:{}", Uuid::new_v4()))
            .expect("uuid v4 forms a valid urn:uuid IRI");
        let fallback_project = ProjectId(
            Iri::parse("urn:uuid:00000000-0000-4000-8000-000000000000")
                .expect("static urn is valid"),
        );
        let project = self.project.clone().unwrap_or(fallback_project);

        let emit = |hb: &AttemptHeartbeat| {
            if let Some(sink) = &self.liveness_sink {
                sink(hb);
            }
        };

        let mut hb = AttemptHeartbeat {
            attempt_id,
            project,
            record: spec.record.clone(),
            adapter: self.dialect.kind().as_wire().to_owned(),
            agent: self.agent.clone(),
            host: self.host.clone(),
            pid,
            started_at: started_wall,
            heartbeat_at: now_at(),
            status: LivenessStatus::Live,
            outcome: None,
            exit_at: None,
            duration_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd_micros: 0,
        };
        emit(&hb);

        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");

        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf).await;
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf).await;
            buf
        });

        let limit = Duration::from_secs(spec.timeout_secs.max(1));
        let deadline = tokio::time::Instant::now() + limit;
        let mut interval = tokio::time::interval(HEARTBEAT_TICK);
        interval.tick().await; // consume immediate first tick

        let wait_result = loop {
            tokio::select! {
                res = child.wait() => {
                    break Some(res);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    break None;
                }
                _ = interval.tick() => {
                    hb.heartbeat_at = now_at();
                    hb.duration_ms = started.elapsed().as_millis() as u64;
                    emit(&hb);
                }
            }
        };

        if wait_result.is_none() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        let stdout_bytes = timeout(Duration::from_millis(200), stdout_task)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
        let stderr_bytes = timeout(Duration::from_millis(200), stderr_task)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();

        let parsed = self.dialect.parse_output(&stdout_bytes, &stderr_bytes);
        let cost_usd_micros = RateCard::resolve_cost(
            parsed.vendor_cost_usd_micros,
            parsed.tokens_in,
            parsed.tokens_out,
            self.rate_card.as_ref(),
        );
        let paths_touched = match snap_before {
            Some(before) => before.diff_after(&spec.project_root).await,
            None => Vec::new(),
        };

        let (status, raw_excerpt) = match wait_result {
            None => {
                let prefix = format!(
                    "`{}` exceeded its {}s timeout",
                    self.binary, spec.timeout_secs
                );
                let excerpt = if parsed.excerpt_text.trim().is_empty() {
                    prefix
                } else {
                    format!("{prefix}\n{}", parsed.excerpt_text)
                };
                (AttemptStatus::Timeout, excerpt)
            }
            Some(Err(e)) => (AttemptStatus::Crashed, e.to_string()),
            Some(Ok(exit_status)) => {
                let st = if exit_status.success() {
                    AttemptStatus::Completed
                } else {
                    AttemptStatus::Crashed
                };
                (st, parsed.excerpt_text)
            }
        };

        let output_excerpt = truncate_utf8(raw_excerpt, EXCERPT_LIMIT);

        let now = now_at();
        hb.heartbeat_at = now.clone();
        hb.exit_at = Some(now);
        hb.status = LivenessStatus::Exited;
        hb.outcome = Some(status);
        hb.duration_ms = duration_ms;
        hb.tokens_in = parsed.tokens_in;
        hb.tokens_out = parsed.tokens_out;
        hb.cost_usd_micros = cost_usd_micros;
        emit(&hb);

        AttemptOutcome {
            status,
            output_excerpt,
            duration_ms,
            tokens_in: parsed.tokens_in,
            tokens_out: parsed.tokens_out,
            cost_usd_micros,
            paths_touched,
        }
    }
}

pub fn truncate_utf8(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

pub fn now_at() -> At {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    At::from_unix_millis(millis)
}

pub fn detect_host() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.trim().is_empty()
    {
        return h.trim().to_owned();
    }
    if let Ok(h) = std::fs::read_to_string("/etc/hostname")
        && !h.trim().is_empty()
    {
        return h.trim().to_owned();
    }
    "localhost".to_owned()
}

/// Converts a dollar `f64` value into integer microdollars (`1 USD = 1_000_000`).
pub fn usd_f64_to_micros(usd: f64) -> Option<u64> {
    if !usd.is_finite() || usd < 0.0 {
        return None;
    }
    Some((usd * 1_000_000.0).round() as u64)
}

/// Extracts usage and cost fields from a JSON object using standard vendor shapes.
pub fn extract_usage_from_value(
    v: &serde_json::Value,
    tokens_in: &mut u64,
    tokens_out: &mut u64,
    vendor_cost_micros: &mut Option<u64>,
     accumulate_tokens: bool,
) {
    if let Some(usage) = v.get("usage").or_else(|| v.get("stats")) {
        let in_tok = usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .or_else(|| usage.get("tokens_in"))
            .and_then(|n| n.as_u64());
        let out_tok = usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .or_else(|| usage.get("tokens_out"))
            .and_then(|n| n.as_u64());
        if let Some(i) = in_tok {
            if accumulate_tokens {
                *tokens_in = tokens_in.saturating_add(i);
            } else {
                *tokens_in = i;
            }
        }
        if let Some(o) = out_tok {
            if accumulate_tokens {
                *tokens_out = tokens_out.saturating_add(o);
            } else {
                *tokens_out = o;
            }
        }
        if let Some(c) = usage.get("cost_usd_micros").and_then(|n| n.as_u64()) {
            *vendor_cost_micros = Some(c);
        } else if let Some(c) = usage
            .get("cost_usd")
            .or_else(|| usage.get("total_cost_usd"))
            .and_then(|n| n.as_f64())
            .and_then(usd_f64_to_micros)
        {
            *vendor_cost_micros = Some(c);
        }
    }

    if let Some(c) = v.get("cost_usd_micros").and_then(|n| n.as_u64()) {
        *vendor_cost_micros = Some(c);
    } else if let Some(c) = v
        .get("total_cost_usd")
        .or_else(|| v.get("cost_usd"))
        .and_then(|n| n.as_f64())
        .and_then(usd_f64_to_micros)
    {
        *vendor_cost_micros = Some(c);
    }
}

/// Parses a single JSON object or JSONL stream for standard CLI envelopes (`claude`, `agy`, `muse`, `grok`),
/// falling back to raw stdout + stderr if no JSON object can be parsed.
pub fn parse_json_or_jsonl_envelope(
    stdout: &[u8],
    stderr: &[u8],
    accumulate_jsonl_tokens: bool,
) -> ParsedOutput {
    let stdout_str = String::from_utf8_lossy(stdout);
    let stderr_str = String::from_utf8_lossy(stderr);
    let trimmed = stdout_str.trim();

    if trimmed.is_empty() {
        return ParsedOutput {
            excerpt_text: stderr_str.into_owned(),
            tokens_in: 0,
            tokens_out: 0,
            vendor_cost_usd_micros: None,
        };
    }

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
        && val.is_object()
    {
        let mut tokens_in = 0u64;
        let mut tokens_out = 0u64;
        let mut vendor_cost = None;
        extract_usage_from_value(&val, &mut tokens_in, &mut tokens_out, &mut vendor_cost, false);
        let text = extract_text_from_object(&val).unwrap_or_else(|| stdout_str.clone().into_owned());
        let excerpt_text = append_stderr(text, &stderr_str);
        return ParsedOutput {
            excerpt_text,
            tokens_in,
            tokens_out,
            vendor_cost_usd_micros: vendor_cost,
        };
    }

    let mut any_json_obj = false;
    let mut tokens_in = 0u64;
    let mut tokens_out = 0u64;
    let mut vendor_cost = None;
    let mut text_parts: Vec<String> = Vec::new();

    for line in trimmed.lines() {
        let line_trim = line.trim();
        if line_trim.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line_trim)
            && val.is_object()
        {
            any_json_obj = true;
            extract_usage_from_value(
                &val,
                &mut tokens_in,
                &mut tokens_out,
                &mut vendor_cost,
                accumulate_jsonl_tokens,
            );
            if let Some(t) = extract_text_from_object(&val)
                && !t.is_empty()
            {
                text_parts.push(t);
            }
        }
    }

    if any_json_obj {
        let base_text = if text_parts.is_empty() {
            stdout_str.into_owned()
        } else {
            text_parts.join("\n")
        };
        return ParsedOutput {
            excerpt_text: append_stderr(base_text, &stderr_str),
            tokens_in,
            tokens_out,
            vendor_cost_usd_micros: vendor_cost,
        };
    }

    let mut raw = stdout_str.into_owned();
    raw.push_str(&stderr_str);
    ParsedOutput {
        excerpt_text: raw,
        tokens_in: 0,
        tokens_out: 0,
        vendor_cost_usd_micros: None,
    }
}

fn extract_text_from_object(val: &serde_json::Value) -> Option<String> {
    for key in ["result", "response", "output", "text", "content", "message"] {
        if let Some(s) = val.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_owned());
        }
    }
    if let Some(item) = val.get("item") {
        if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
            return Some(s.to_owned());
        }
    }
    None
}

fn append_stderr(mut base: String, stderr: &str) -> String {
    if !stderr.is_empty() {
        if !base.is_empty() && !base.ends_with('\n') {
            base.push('\n');
        }
        base.push_str(stderr);
    }
    base
}

#[derive(Debug, Clone)]
pub struct GitSnapshot {
    head: Option<String>,
    dirty: BTreeMap<String, Option<[u8; 32]>>,
}

impl GitSnapshot {
    pub async fn capture(root: &Path) -> Option<Self> {
        let status_out = Command::new("git")
            .args(["status", "--porcelain=v1", "-z", "-uall"])
            .current_dir(root)
            .output()
            .await
            .ok()?;
        if !status_out.status.success() {
            return None;
        }

        let head = Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(root)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty());

        let dirty = parse_porcelain_z_and_hash(root, &status_out.stdout);
        Some(Self { head, dirty })
    }

    pub async fn diff_after(&self, root: &Path) -> Vec<String> {
        let Some(after) = Self::capture(root).await else {
            return Vec::new();
        };
        let mut touched = BTreeSet::new();

        for (path, after_digest) in &after.dirty {
            match self.dirty.get(path) {
                None => {
                    touched.insert(path.clone());
                }
                Some(before_digest) if before_digest != after_digest => {
                    touched.insert(path.clone());
                }
                _ => {}
            }
        }

        if let (Some(h_before), Some(h_after)) = (&self.head, &after.head)
            && h_before != h_after
            && let Ok(diff_out) = Command::new("git")
                .args(["diff", "--name-only", "-z", h_before, h_after])
                .current_dir(root)
                .output()
                .await
            && diff_out.status.success()
        {
            for entry in diff_out.stdout.split(|b| *b == 0) {
                if !entry.is_empty() {
                    touched.insert(String::from_utf8_lossy(entry).into_owned());
                }
            }
        } else if self.head.is_none()
            && let Some(h_after) = &after.head
            && let Ok(tree_out) = Command::new("git")
                .args(["ls-tree", "-r", "--name-only", "-z", h_after])
                .current_dir(root)
                .output()
                .await
            && tree_out.status.success()
        {
            for entry in tree_out.stdout.split(|b| *b == 0) {
                if !entry.is_empty() {
                    touched.insert(String::from_utf8_lossy(entry).into_owned());
                }
            }
        }

        for (path, before_digest) in &self.dirty {
            if !after.dirty.contains_key(path) {
                let current_digest = hash_worktree_path(&root.join(path));
                if &current_digest != before_digest {
                    touched.insert(path.clone());
                }
            }
        }

        touched.into_iter().collect()
    }
}

fn parse_porcelain_z_and_hash(
    root: &Path,
    bytes: &[u8],
) -> BTreeMap<String, Option<[u8; 32]>> {
    let mut map = BTreeMap::new();
    let mut fields = bytes.split(|b| *b == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let xy = &field[0..2];
        let path_bytes = &field[3..];
        let path = String::from_utf8_lossy(path_bytes).into_owned();
        let digest = hash_worktree_path(&root.join(&path));
        map.insert(path, digest);
        if xy[0] == b'R' || xy[0] == b'C' || xy[1] == b'R' || xy[1] == b'C' {
            if let Some(orig_bytes) = fields.next()
                && !orig_bytes.is_empty()
            {
                let orig_path = String::from_utf8_lossy(orig_bytes).into_owned();
                let orig_digest = hash_worktree_path(&root.join(&orig_path));
                map.insert(orig_path, orig_digest);
            }
        }
    }
    map
}

fn hash_worktree_path(full_path: &Path) -> Option<[u8; 32]> {
    let meta = std::fs::symlink_metadata(full_path).ok()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(full_path).ok()?;
        let mut hasher = Sha256::new();
        hasher.update(b"symlink:");
        hasher.update(target.to_string_lossy().as_bytes());
        return Some(hasher.finalize().into());
    }
    if meta.is_file() {
        let bytes = std::fs::read(full_path).ok()?;
        let mut hasher = Sha256::new();
        hasher.update(b"file:");
        hasher.update(&bytes);
        return Some(hasher.finalize().into());
    }
    None
}
```

Include unit tests in `crates/exec/src/adapters/harness.rs` testing:
1. `GitSnapshot::capture` and `diff_after`: pre-existing dirty file untouched vs modified during attempt, new untracked file created during attempt, and committed change (`HEAD` advanced during attempt) — plus verifying `.git/objects` has no loose objects written when hashing an untracked file.
2. `ProcessHarness` emitting `Live` on spawn and `Exited` on completion/timeout to a `LivenessSink`, and preserving partial stdout token usage on `Timeout`.

- [ ] **Step 2: Refactor `crates/exec/src/adapters/claude.rs` onto `ProcessHarness` and `ClaudeDialect`**

Update `crates/exec/src/adapters/claude.rs` so `ClaudeAdapter` wraps `ProcessHarness<ClaudeDialect>`:
- `ClaudeDialect::argv`:
  ```rust
  let budget_usd = spec.budget_usd_micros as f64 / 1_000_000.0;
  vec![
      "-p".into(),
      spec.instruction.clone(),
      "--output-format".into(),
      "json".into(),
      "--max-budget-usd".into(),
      format!("{budget_usd:.6}"),
  ]
  ```
- `ClaudeDialect::parse_output(stdout, stderr)` calls `parse_json_or_jsonl_envelope(stdout, stderr, false)`.
- `ClaudeAdapter` provides `new(binary: String)`, `with_rate_card(mut self, rate_card: Option<RateCard>) -> Self`, and `with_liveness(mut self, project: ProjectId, agent: Option<String>, sink: LivenessSink) -> Self`.
- Keep all 6 existing tests in `crates/exec/src/adapters/claude.rs` and add unit tests for:
  1. Parsing a real Claude JSON result (`{"type":"result","result":"done","total_cost_usd":0.012345,"usage":{"input_tokens":120,"output_tokens":45}}`) into `tokens_in = 120`, `tokens_out = 45`, `cost_usd_micros = 12_345`, `output_excerpt = "done"`.
  2. Falling back to `RateCard` when `total_cost_usd` is absent.
  3. Recording `paths_touched` when the stub script modifies a file inside a temporary git repo.

- [ ] **Step 3: Run tests and commit Task 3**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p fl-exec --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
git add crates/exec
git commit -m "feat(exec): add ProcessHarness, GitSnapshot, LivenessSink, and Claude JSON telemetry

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```

---

## Task 4: `CodexAdapter`, `AntigravityAdapter`, `MuseAdapter`, `GrokAdapter`, and `AnyAdapter` (`crates/exec`)

**Files:**
- Create: `crates/exec/src/adapters/codex.rs`
- Create: `crates/exec/src/adapters/antigravity.rs`
- Create: `crates/exec/src/adapters/muse.rs`
- Create: `crates/exec/src/adapters/grok.rs`
- Modify: `crates/exec/src/adapters/mod.rs`
- Modify: `crates/exec/src/lib.rs`

- [ ] **Step 1: Implement `crates/exec/src/adapters/codex.rs` (`CodexAdapter` and `CodexDialect`)**

Create `crates/exec/src/adapters/codex.rs`:
- `CodexDialect::kind(&self) -> AdapterKind { AdapterKind::Codex }`
- `CodexDialect::default_binary(&self) -> &'static str { "codex" }`
- `CodexDialect::argv(&self, spec: &AttemptSpec) -> Vec<String>`:
  ```rust
  vec![
      "exec".into(),
      "--json".into(),
      "--skip-git-repo-check".into(),
      "-C".into(),
      spec.project_root.to_string_lossy().into_owned(),
      spec.instruction.clone(),
  ]
  ```
- `CodexDialect::parse_output(&self, stdout: &[u8], stderr: &[u8]) -> ParsedOutput`:
  - Iterates JSONL lines in `stdout`.
  - When a line is JSON with `"type": "turn.completed"` and `"usage"`, adds `usage.input_tokens` to `tokens_in` and `usage.output_tokens` to `tokens_out`, plus optional `cost_usd` / `cost_usd_micros`.
  - Also supports generic `extract_usage_from_value` if `"type"` is not `"turn.completed"`.
  - Collects text from `{"type":"item.completed","item":{"type":"agent_message","text":"..."}}` or top-level `"result"` / `"message"` / `"text"`.
  - Falls back to `parse_json_or_jsonl_envelope(stdout, stderr, true)` when no Codex-specific events are present.
- Include unit tests in `crates/exec/src/adapters/codex.rs` testing `turn.completed` + `item.completed` JSONL parsing, `RateCard` cost calculation, missing binary refusal, zero budget refusal, and timeout handling.

- [ ] **Step 2: Implement `crates/exec/src/adapters/antigravity.rs`, `muse.rs`, and `grok.rs`**

1. Create `crates/exec/src/adapters/antigravity.rs`:
   - `AntigravityDialect`: `AdapterKind::Antigravity`, default binary `"agy"`, `argv`: `vec!["-p".into(), spec.instruction.clone(), "--output-format".into(), "json".into()]`, `parse_output`: `parse_json_or_jsonl_envelope(stdout, stderr, false)`.
   - `AntigravityAdapter` wrapping `ProcessHarness<AntigravityDialect>`, implementing `Runner` (`id() -> "antigravity"`).
   - Unit tests for JSON output parsing, `RateCard` fallback, zero budget refusal, and missing binary refusal.
2. Create `crates/exec/src/adapters/muse.rs`:
   - `MuseDialect`: `AdapterKind::Muse`, default binary `"muse"`, `argv`: `vec!["exec".into(), "--json".into(), spec.instruction.clone()]`, `parse_output`: `parse_json_or_jsonl_envelope(stdout, stderr, true)`.
   - `MuseAdapter` wrapping `ProcessHarness<MuseDialect>`, implementing `Runner` (`id() -> "muse"`).
   - Unit tests for JSON/JSONL output parsing, `RateCard` fallback, zero budget refusal, and missing binary refusal.
3. Create `crates/exec/src/adapters/grok.rs`:
   - `GrokDialect`: `AdapterKind::Grok`, default binary `"grok"`, `argv`: `vec!["-p".into(), spec.instruction.clone(), "--output-format".into(), "json".into()]`, `parse_output`: `parse_json_or_jsonl_envelope(stdout, stderr, false)`.
   - `GrokAdapter` wrapping `ProcessHarness<GrokDialect>`, implementing `Runner` (`id() -> "grok"`).
   - Unit tests for JSON output parsing, `RateCard` fallback, zero budget refusal, and missing binary refusal.

- [ ] **Step 3: Implement `AnyAdapter` in `crates/exec/src/adapters/mod.rs` and re-export in `crates/exec/src/lib.rs`**

Write `crates/exec/src/adapters/mod.rs`:

```rust
pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod grok;
pub mod harness;
pub mod muse;

pub use antigravity::AntigravityAdapter;
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use grok::GrokAdapter;
pub use harness::{CliDialect, GitSnapshot, LivenessSink, ParsedOutput, ProcessHarness};
pub use muse::MuseAdapter;

use crate::runner::{AttemptError, AttemptOutcome, AttemptSpec, Runner};
use fl_core::ids::ProjectId;
use fl_core::telemetry::{AdapterKind, RateCard};

pub enum AnyAdapter {
    Claude(ClaudeAdapter),
    Codex(CodexAdapter),
    Antigravity(AntigravityAdapter),
    Muse(MuseAdapter),
    Grok(GrokAdapter),
}

impl AnyAdapter {
    pub fn new(
        kind: AdapterKind,
        binary_override: Option<String>,
        rate_card: Option<RateCard>,
    ) -> Self {
        let bin = binary_override.unwrap_or_else(|| kind.default_binary().to_owned());
        match kind {
            AdapterKind::Claude => Self::Claude(ClaudeAdapter::new(bin).with_rate_card(rate_card)),
            AdapterKind::Codex => Self::Codex(CodexAdapter::new(bin).with_rate_card(rate_card)),
            AdapterKind::Antigravity => {
                Self::Antigravity(AntigravityAdapter::new(bin).with_rate_card(rate_card))
            }
            AdapterKind::Muse => Self::Muse(MuseAdapter::new(bin).with_rate_card(rate_card)),
            AdapterKind::Grok => Self::Grok(GrokAdapter::new(bin).with_rate_card(rate_card)),
        }
    }

    pub fn with_liveness(
        self,
        project: ProjectId,
        agent: Option<String>,
        sink: LivenessSink,
    ) -> Self {
        match self {
            Self::Claude(a) => Self::Claude(a.with_liveness(project, agent, sink)),
            Self::Codex(a) => Self::Codex(a.with_liveness(project, agent, sink)),
            Self::Antigravity(a) => Self::Antigravity(a.with_liveness(project, agent, sink)),
            Self::Muse(a) => Self::Muse(a.with_liveness(project, agent, sink)),
            Self::Grok(a) => Self::Grok(a.with_liveness(project, agent, sink)),
        }
    }
}

impl Runner for AnyAdapter {
    fn id(&self) -> &str {
        match self {
            Self::Claude(a) => a.id(),
            Self::Codex(a) => a.id(),
            Self::Antigravity(a) => a.id(),
            Self::Muse(a) => a.id(),
            Self::Grok(a) => a.id(),
        }
    }

    async fn attempt(&self, spec: AttemptSpec) -> Result<AttemptOutcome, AttemptError> {
        match self {
            Self::Claude(a) => a.attempt(spec).await,
            Self::Codex(a) => a.attempt(spec).await,
            Self::Antigravity(a) => a.attempt(spec).await,
            Self::Muse(a) => a.attempt(spec).await,
            Self::Grok(a) => a.attempt(spec).await,
        }
    }
}
```

In `crates/exec/src/lib.rs`, re-export:
```rust
pub use adapters::{
    AntigravityAdapter, AnyAdapter, ClaudeAdapter, CodexAdapter, GrokAdapter, LivenessSink,
    MuseAdapter,
};
```

- [ ] **Step 4: Run tests and commit Task 4**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -p fl-exec --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
git add crates/exec
git commit -m "feat(exec): add Codex, Antigravity, Muse, Grok, and AnyAdapter runners

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```


---

## Task 5: CLI Config, 5-Vendor `fl attempt`, `fl agent liveness`, and Extended `fl stats` (`crates/cli`)

**Files:**
- Modify: `crates/cli/src/config.rs`
- Modify: `crates/cli/src/cmd/attempt.rs`
- Modify: `crates/cli/src/cmd/agent.rs`
- Modify: `crates/cli/src/cmd/stats.rs`
- Modify: `crates/cli/src/main.rs`
- Modify: `crates/cli/tests/attempt.rs`

- [ ] **Step 1: Add `AdapterConfig` and `[adapters.<name>]` parsing to `crates/cli/src/config.rs`**

In `crates/cli/src/config.rs`:
1. Import `fl_core::telemetry::{AdapterKind, RateCard}` and `std::collections::BTreeMap`.
2. Define `AdapterConfig`:
   ```rust
   /// Per-adapter `[adapters.<name>]` configuration (Adapters & Telemetry spec §1.2).
   #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(deny_unknown_fields)]
   pub struct AdapterConfig {
       #[serde(default)]
       pub binary: Option<String>,
       #[serde(default)]
       pub input_usd_micros_per_mtok: Option<u64>,
       #[serde(default)]
       pub output_usd_micros_per_mtok: Option<u64>,
   }

   impl AdapterConfig {
       pub fn rate_card(&self) -> Option<RateCard> {
           if self.input_usd_micros_per_mtok.is_none()
               && self.output_usd_micros_per_mtok.is_none()
           {
               None
           } else {
               Some(RateCard {
                   input_usd_micros_per_mtok: self.input_usd_micros_per_mtok.unwrap_or(0),
                   output_usd_micros_per_mtok: self.output_usd_micros_per_mtok.unwrap_or(0),
               })
           }
       }
   }
   ```
3. Add `pub adapters: BTreeMap<AdapterKind, AdapterConfig>` to `Config`, and `#[serde(default)] adapters: BTreeMap<String, AdapterConfig>` to `File`.
4. In `load(path: Option<&Path>) -> Result<Config>`, validate each key in `file.adapters`:
   ```rust
   let mut adapters = BTreeMap::new();
   for (raw_key, adapter_cfg) in file.adapters {
       let kind = AdapterKind::from_wire(&raw_key).map_err(|_| {
           anyhow::anyhow!(
               "{}: unknown adapter `{raw_key}` in `[adapters]`; known adapters are: {}",
               path.display(),
               AdapterKind::ALL
                   .iter()
                   .map(|k| k.as_wire())
                   .collect::<Vec<_>>()
                   .join(", ")
           )
       })?;
       if let Some(bin) = &adapter_cfg.binary
           && bin.trim().is_empty()
       {
           bail!(
               "{}: `adapters.{raw_key}.binary` must not be empty",
               path.display()
           );
       }
       adapters.insert(kind, adapter_cfg);
   }
   ```
5. Add unit tests in `crates/cli/src/config.rs` verifying valid `[adapters.codex]` / `[adapters.claude]` rate-card parsing and refusal of unknown `[adapters.telepathy]` keys.

- [ ] **Step 2: Update `crates/cli/src/cmd/attempt.rs` and `crates/cli/src/main.rs` for 5-vendor `fl attempt` and wrapper liveness**

In `crates/cli/src/cmd/attempt.rs`:
1. Change `Cmd::binary` from `pub binary: String` (`default_value = "claude"`) to `#[arg(long)] pub binary: Option<String>`.
2. Update `run(ctx: &Ctx<'_>, adapters: &BTreeMap<AdapterKind, crate::config::AdapterConfig>, cmd: Cmd) -> Result<i32>`:
   - First check in `run`:
     ```rust
     let store = ctx.store;
     let Ok(kind) = AdapterKind::from_wire(&cmd.adapter) else {
         let known = AdapterKind::ALL
             .iter()
             .map(|k| k.as_wire())
             .collect::<Vec<_>>()
             .join(", ");
         bail!(
             "`{}` is not a known adapter. Known adapters: {known}.",
             cmd.adapter
         );
     };
     ```
   - Resolve `record`, `project`, and call `crate::preflight::check(ctx, &record.project)?`.
   - Resolve `binary_override`: `cmd.binary.clone().or_else(|| adapters.get(&kind).and_then(|c| c.binary.clone()))`.
   - Resolve `rate_card`: `adapters.get(&kind).and_then(|c| c.rate_card())`.
   - Resolve optional agent persona from `std::env::var("AGENT_PERSONA").ok().map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty())`.
   - Construct a `LivenessSink` that writes `StateEntry` (`StatePayload::Runtime(RuntimeEntry { status: hb.status.as_wire().to_owned(), details: serde_json::to_value(hb).unwrap() })`, `MergeMode::LastWriter`, `sensitive: false`) into `ctx.store`:
     - Under `StateScope::Project(project_slug)` with key `liveness_record_slug(&hb.record)` (where `project_slug` is derived from `project.name` or lowercase basename of `project.root` validated via `validate_slug`, falling back to `"default"`).
     - And if `hb.agent` is `Some(ref persona)` and `validate_slug(persona).is_ok()`, also under `StateScope::Agent(persona.clone())` with key `liveness_agent_slug(persona)`.
   - Instantiate `AnyAdapter::new(kind, binary_override, rate_card).with_liveness(record.project.clone(), agent_persona, sink)`.
   - Run `adapter.attempt(spec)` and record `Attempt { adapter: kind.as_wire().to_owned(), .. }`.
3. In `crates/cli/src/main.rs`, pass `&cfg.adapters` to `cmd::attempt::run(&ctx, &cfg.adapters, c)`.

- [ ] **Step 3: Add `Liveness` subcommand to `crates/cli/src/cmd/agent.rs`**

In `crates/cli/src/cmd/agent.rs`:
1. Add `Liveness` to `AgentCmd`:
   ```rust
   /// Report wrapper-observed liveness (`live`, `unverifiable`, or `exited`) for an agent or record.
   Liveness {
       /// Optional agent persona (`ferris`) or record handle/IRI (`1`, `urn:uuid:...`).
       target: Option<String>,
       /// Maximum heartbeat age in milliseconds before a `live` status becomes `unverifiable (stale_status)`.
       #[arg(long, default_value_t = fl_core::telemetry::DEFAULT_LIVENESS_STALE_MS)]
       stale_after_ms: u64,
       #[arg(long)]
       json: bool,
   },
   ```
2. Update `pub fn run<S: AgentRegistry + Handles + StateStore + Catalog>(store: &S, cmd: AgentCmd) -> Result<(), (i32, String)>`:
   - Implement a helper `pid_is_alive(pid: u32) -> bool`:
     - On Unix (`#[cfg(unix)]`), check `/proc/{pid}/stat` first if `/proc` exists (verifying state is not `'Z'`), otherwise call `std::process::Command::new("kill").args(["-0", &pid.to_string()]).status().is_ok_and(|s| s.success())` so no `unsafe` block is required (respecting `#![forbid(unsafe_code)]` if set or avoiding raw libc FFI).
   - Look up `RuntimeEntry` heartbeats across `store.list_state(None, Some("liveness-"), true)`:
     - Parse `AttemptHeartbeat` from `StatePayload::Runtime(rt) => serde_json::from_value::<AttemptHeartbeat>(rt.details)`.
     - If `target` is `Some(t)`:
       - Try resolving `t` as a `Kind::Record` handle/IRI in `store`; if found, match `hb.record == RecordId(iri)` (deduplicating by latest `heartbeat_at`).
       - Otherwise match `hb.agent.as_deref() == Some(t.to_ascii_lowercase().as_str())` or `hb.record.iri().as_str() == t`.
       - If no heartbeat matches `target`, evaluate `evaluate_liveness(None, &now, stale_after_ms, &local_host, pid_is_alive)` which returns `LivenessVerdict::Unverifiable { reason: MissingStatus, .. }`.
     - Format output in plain text (`<target>\t<status>\t...`) or `--json` (`serde_json::to_string_pretty`).
3. In `crates/cli/src/main.rs`, when dispatching `Command::Agent { cmd }`, if `explicit` is `None` and `matches!(cmd, cmd::agent::AgentCmd::Liveness { .. })`, check `config::bound_entry(&cfg.projects, &cwd)` and if `Some(entry)` has `entry.store.exists()`, open `entry.store` so `fl agent liveness` inside a bound project inspects that project's store by default.

- [ ] **Step 4: Extend `crates/cli/src/cmd/stats.rs` with tokens, per-adapter breakdown, `--json`, and Decision 21 regret audit**

In `crates/cli/src/cmd/stats.rs`:
1. Add `#[arg(long)] pub json: bool` to `Cmd`.
2. Query `transitions = store.transitions(&project)?` and `gate_runs = store.gate_runs(&project)?` (Note: `Catalog` provides `transitions(&project)` and `Ledger` provides `gate_runs(&project)`).
3. Update `report(attempts: &[Attempt], coverage: &Coverage, transitions: &[Transition], gate_runs: &[GateRun]) -> Vec<String>`:
   - Keep `attempts: 0` and `Coverage::LocalOnly` note unchanged when `attempts.is_empty()` (and still append regret audit challenges if any exist).
   - When `attempts` is non-empty:
     - Compute `by_status`, `total_ms`, `total_tokens_in`, `total_tokens_out`, `total_cost`, `non_refused_count`, `zero_cost_non_refused_count`, and `by_adapter: BTreeMap<String, AdapterStats>`.
     - Emit:
       ```text
       attempts: <N>
         <status>: <count>
       wall clock: <total_ms>ms
       tokens in: <total_tokens_in>
       tokens out: <total_tokens_out>
       cost: <total_cost> micro-USD
       ```
     - Floor notes:
       - If `total_cost == 0 && non_refused_count > 0`:
         `"note: no adapter reported a cost, so the figure above is a floor and not a total"`
       - Else if `total_cost > 0 && zero_cost_non_refused_count > 0`:
         `format!("note: {zero_cost_non_refused_count} of {non_refused_count} non-refused attempts reported 0 cost, so the figure above is a floor and not a total")`
     - Emit `by adapter:`:
       ```text
       by adapter:
         <adapter>: <attempts> attempts, <duration_ms>ms, <tokens_in> in, <tokens_out> out, <cost_usd_micros> micro-USD
       ```
   - Call `let challenges = fl_core::telemetry::audit_transition_regret(transitions, gate_runs);` and for each challenge emit:
     - For `RegretMetric::CostUsdMicros`:
       `format!("regret audit: transition `{}` ({} -> {}) declares regret = low, but its gates average {} micro-USD ({}x the project median of {} micro-USD)", c.name, c.from.as_wire(), c.to.as_wire(), c.measured_mean, c.ratio, c.project_median)`
     - For `RegretMetric::DurationMs`:
       `format!("regret audit: transition `{}` ({} -> {}) declares regret = low, but its gates average {}ms ({}x the project median of {}ms)", c.name, c.from.as_wire(), c.to.as_wire(), c.measured_mean, c.ratio, c.project_median)`
4. When `cmd.json` is true, serialize a `StatsJson` struct with `attempts`, `coverage`, `by_status`, `duration_ms`, `tokens_in`, `tokens_out`, `cost_usd_micros`, `cost_is_floor`, `by_adapter`, and `regret_challenges` via `serde_json::to_string_pretty`.

- [ ] **Step 5: Add black-box CLI integration tests in `crates/cli/tests/attempt.rs`**

In `crates/cli/tests/attempt.rs`, keep the 3 existing tests and add:
1. `all_five_adapters_run_record_telemetry_paths_touched_and_liveness`:
   - Creates a stub executable script that touches a file in the project repo and prints `{"type":"result","result":"ok","total_cost_usd":0.005,"usage":{"input_tokens":100,"output_tokens":25}}`.
   - Runs `fl attempt 1 --adapter <vendor> --binary <stub>` for all 5 vendors (`claude`, `codex`, `antigravity`, `muse`, `grok`).
   - Runs `fl stats --project 1` and verifies `attempts: 5`, `tokens in: 500`, `tokens out: 125`, `cost: 25000 micro-USD`, and all 5 lines under `by adapter:`.
   - Runs `fl agent liveness 1 --json` and verifies `"status": "exited"` and `"outcome": "completed"`.
2. `stats_reports_partial_cost_floor_and_regret_audit_challenge`:
   - Runs one attempt with cost > 0 and one completed attempt with cost = 0, verifying `fl stats --project 1` prints `1 of 2 non-refused attempts reported 0 cost, so the figure above is a floor and not a total`.

- [ ] **Step 6: Run full workspace verification and commit Task 5**

Run:
```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo test --workspace --no-fail-fast --offline
git add crates/cli
git commit -m "feat(cli): wire 5-vendor attempt, liveness CLI, and stats telemetry/regret audit

Co-authored-by: Ferris <ferris@artificialhumanity.io>"
```
