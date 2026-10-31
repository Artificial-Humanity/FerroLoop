# Sub-project 6: `FerroGate` — Zero-Knowledge Agent Secrets Vault, Credential Proxy, Declarative API-to-MCP Gateway, and Event Bridge (`crates/gate` / `fl vault`)

* **Date:** 2026-10-27
* **Author:** Ferris (`ferris@artificialhumanity.io`)
* **Status:** Ratified by Owner (`lmcfarlin`)
* **Target Crates & Surfaces:**
  * `crates/gate` (`fl-gate` standalone library crate)
  * `crates/hub` (`/api/v1/gate/*` REST + MCP Registry `v0.1` + webhook bridge endpoints)
  * `crates/cli` (`fl vault` subcommand family, keeping `fl gate` for verification gates; `fl vault` with `fl gateway` visible alias)
  * `ui/` (`FerroDeck` Vault & Connectors management pane)

---

## 1. Purpose, Scope, and Standalone Modularity

`FerroGate` (`crates/gate`) is Sub-project 6 in the `FerroLoop` ecosystem. It unifies four capabilities into a single, dependency-light Rust crate that works both standalone and integrated with `fl-hub`, `fl-wire`, `fl-mcp`, and `FerroDeck`:

1. **Zero-Knowledge Encrypted Secrets Vault (`crates/gate/src/vault.rs`):**
   * Stores API keys, tokens, and webhook secrets in an AES-256-GCM encrypted vault file (`~/.local/share/fl/vault.enc`) with AEAD Additional Authenticated Data (AAD) binding each ciphertext to its policy (`secret_name | allowed_hosts | allowed_methods | allowed_agents`).
   * Agents never receive raw secret values in environment variables, CLI arguments, or MCP tool responses.
2. **Zero-Knowledge Credential Proxy & Output Scrubber (`crates/gate/src/proxy.rs`):**
   * Substitutes `{{FERROGATE:KEY}}` placeholders on outbound HTTP/REST/GraphQL requests only after verifying that the destination host, HTTP method, and calling agent identity satisfy the secret's policy in `vault.enc`.
   * Enforces DNS/IP SSRF blocking (refusing loopback, link-local, and RFC1918 private addresses unless explicitly allowlisted on the secret/connector for a local service), strips `Authorization` and secret headers on cross-origin HTTP redirects, and scrubs raw secret values and high-entropy credential patterns from HTTP bodies, stdout, and stderr before returning output to the agent.
3. **Declarative API-to-MCP Gateway, Local MCP Registry `v0.1`, and Zero-Hoop Multi-Vendor Registration (`crates/gate/src/connector.rs`, `mcp.rs`, `registry.rs`):**
   * Loads human-readable TOML connector manifests from project-level `.fl/gate/*.toml` and user-level `~/.config/fl/gate/*.toml` (plus an OpenAPI 3.x importer `fl vault import-openapi`).
   * Exposes **both**:
     1. A **Unified MCP JSON-RPC 2.0 Server** (`fl vault mcp`) that aggregates all enabled connectors in the catalog behind a single MCP server entry, so registering `ferrogate` once across user-level (`~/.claude.json`, `~/.codex/config.toml`, `~/.gemini/antigravity-cli/mcp_config.json`, `~/.config/muse/settings.json`, `~/.grok/config.toml`) or project-level (`.fl/mcp.toml`) vendor files makes every newly added `.toml` connector immediately available to all 5 vendor CLIs without touching vendor config files again.
     2. A **Standard MCP Registry `v0.1` HTTP API** (`/v0.1/servers`, `/v0.1/servers/{name}/versions/latest`, `/v0.1/servers/{name}/versions/{version}`) compatible with `crates/mcp/src/registry.rs` (`fl mcp search`, `fl mcp add --from`, `fl mcp upgrade`).
4. **Dual-Mode Outbound Tools & Inbound Event Bridge (`crates/gate/src/bridge.rs`):**
   * Every connector supports **both** outbound MCP tools (`[[tools]]` for agent-initiated API calls and polling) and declarative inbound bridges (`[[bridges]]` for webhooks and bot events, using Discord as the reference integration).
   * Inbound events validate sender identity against an explicit `allowed_user_ids` / `allowed_guild_ids` allowlist and route either to a workspace resident agent over `FerroWire` (`target = "wire:<persona>"`) or directly to a configured LLM connector (`target = "connector:<id>"`).

### 1.1 Standalone Crate Boundary & Future Repository Split

Per the owner's modularity rule:
* `crates/gate` (`fl-gate`) depends **only** on `fl-core` (and standard workspace crates: `ring`, `serde`, `serde_json`, `toml`, `ureq`, `sha2`, `base64`, `thiserror`, `uuid`).
* `fl-gate` does **not** depend on `fl-store`, `fl-exec`, `fl-wire`, `fl-github`, `fl-mcp`, or `fl-hub`.
* This boundary is enforced in `.fl/architecture.toml` via `fl arch check` so `fl-gate` can be used standalone by any end user or split into its own Git repository/submodule in the future with zero refactoring.

---

## 2. Encrypted Vault & Master Key Architecture (`crates/gate/src/vault.rs`)

### 2.1 File Layout & Permissions
* **Vault File:** `$XDG_DATA_HOME/fl/vault.enc` (default `~/.local/share/fl/vault.enc`), written atomically via temp-file + `0600` chmod + rename, refusing symlinks.
* **Master Key Resolution Order:**
  1. `FL_VAULT_MASTER_KEY` environment variable (64-character hex string = 32 bytes), for CI or headless overrides.
  2. `$XDG_CONFIG_HOME/fl/vault.key` (default `~/.config/fl/vault.key`), a 32-byte random master key generated via `ring::rand::SystemRandom` on `fl vault init` and enforced at mode `0600` on Unix.

### 2.2 Cryptographic Construction (`ring` AES-256-GCM + AAD Policy Binding)
Each secret entry in `vault.enc` stores:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultSecretMetadata {
    pub name: String,
    pub description: Option<String>,
    pub allowed_hosts: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub allowed_agents: Vec<String>,
    pub allow_private_network: bool,
    pub created_at: String,
    pub updated_at: String,
    pub lease_ttl_secs: Option<u64>,
    pub expires_at: Option<String>,
}
```
* **Nonce:** 96-bit (12-byte) random nonce generated per encryption via `ring::rand::SystemRandom`.
* **Additional Authenticated Data (AAD):** Canonical pipe-delimited string:
  ```text
  ferrogate:v1|<name>|<comma-sorted-allowed_hosts>|<comma-sorted-allowed_methods>|<comma-sorted-allowed_agents>|<allow_private_network>|<expires_at_or_none>
  ```
* **Tamper Resistance:** Because the policy fields are bound into the AES-256-GCM AAD, any manual edit to `vault.enc` that widens `allowed_hosts`, `allowed_methods`, `allowed_agents`, or `allow_private_network` causes `OpeningKey::open_in_place` to fail authentication immediately.
* **Redacted Inspection:** `Vault::list()` returns ` Vec<VaultSecretMetadata>` and **never** exposes plaintext bytes.

---

## 3. Zero-Knowledge Credential Proxy & Output Scrubber (`crates/gate/src/proxy.rs`)

### 3.1 Placeholder Substitution (`{{FERROGATE:KEY}}`)
* Outbound HTTP requests (`ProxyRequest`) may contain `{{FERROGATE:<SECRET_NAME>}}` placeholders in:
  * URL query parameters
  * Request headers (e.g., `Authorization: Bearer {{FERROGATE:DISCORD_BOT_TOKEN}}` or `x-api-key: {{FERROGATE:ANTHROPIC_API_KEY}}`)
  * Request body string
* Before substituting `<SECRET_NAME>`:
  1. The proxy verifies the secret has not expired (`expires_at`).
  2. The proxy parses the destination URL scheme (`https` required by default; `http` allowed only when `allow_private_network = true` for local services) and extracts the normalized host.
  3. **Host Pinning:** The destination host must match `allowed_hosts` (exact match or wildcard subdomain `*.example.com`). An empty `allowed_hosts` list refuses all outbound network substitution until at least one host is pinned.
  4. **Method Pinning:** If `allowed_methods` is non-empty, the uppercase HTTP method (`GET`, `POST`, etc.) must be in `allowed_methods`.
  5. **Agent ACL:** If `allowed_agents` is non-empty, the caller persona (or `"*"`) must match `allowed_agents`.
  6. **SSRF Guard:** Unless `allow_private_network = true` on the secret, the proxy resolves the target host and refuses loopback (`127.0.0.0/8`, `::1`, `localhost`), link-local (`169.254.0.0/16`, `fe80::/10`), and RFC1918 private ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`).
  7. **Cross-Origin Redirect Protection:** Outbound HTTP execution disables automatic cross-origin credential forwarding so an upstream `301`/`302`/`307`/`308` redirect can never leak an injected `Authorization` header to a third-party host.

### 3.2 Output Scrubber (`scrub_secrets`)
* Before any `ProxyResponse` (status, headers, body) or MCP tool result is returned to the caller:
  * Every known plaintext secret value in the active vault (plus its URL-encoded and base64-encoded forms) is replaced with `[REDACTED:FERROGATE:<NAME>]`.
  * Standard bearer/token leak patterns (`sk-ant-...`, `sk-proj-...`, `ghp_...`, `github_pat_...`, `xoxb-...`) are scrubbed if they appear in response payloads or error traces.

---

## 4. Declarative API-to-MCP Connectors & Local MCP Registry (`crates/gate/src/connector.rs`, `mcp.rs`, `registry.rs`)

### 4.1 Connector Manifest Schema (`.fl/gate/<id>.toml` and `~/.config/fl/gate/<id>.toml`)

```toml
id = "discord"
title = "Discord API & Bot Gateway"
version = "0.1.0"
description = "Outbound Discord REST tools and inbound channel/thread bridge"
base_url = "https://discord.com/api/v10"
allowed_hosts = ["discord.com"]
default_headers = { Authorization = "Bot {{FERROGATE:DISCORD_BOT_TOKEN}}", "Content-Type" = "application/json" }
enabled = true

[[tools]]
name = "discord_send_message"
description = "Post a message to a Discord channel or thread"
method = "POST"
path = "/channels/{channel_id}/messages"
path_params = ["channel_id"]
body_params = ["content"]
response_fields = ["id", "channel_id", "content", "timestamp"]

[tools.input_schema]
type = "object"
required = ["channel_id", "content"]
[tools.input_schema.properties.channel_id]
type = "string"
description = "Target Discord channel or thread snowflake ID"
[tools.input_schema.properties.content]
type = "string"
description = "Message content to post"

[[tools]]
name = "discord_list_messages"
description = "Poll recent messages in a Discord channel or thread"
method = "GET"
path = "/channels/{channel_id}/messages"
path_params = ["channel_id"]
query_params = ["limit", "after"]
response_fields = ["id", "author", "content", "timestamp"]

[[bridges]]
id = "discord-workspace"
kind = "discord"
enabled = true
verify_secret = "DISCORD_PUBLIC_KEY"
bot_token_secret = "DISCORD_BOT_TOKEN"
allowed_user_ids = ["owner-discord-snowflake"]
allowed_guild_ids = []
# Connector ids a sender may pick with `/model <connector>`; empty refuses every inline choice.
allowed_inline_connectors = []
default_target = "wire:ferris"

[bridges.channel_routes]
"123456789012345678" = "wire:ferris"
"234567890123456789" = "wire:jarvis"
"345678901234567890" = "connector:claude"
```

### 4.2 OpenAPI 3.x Importer (`import_openapi_to_connector`)
* `fl vault import-openapi <spec.json> --id <id> [--out .fl/gate/<id>.toml] [--secret <SECRET_NAME>]` parses an OpenAPI 3.x JSON document (`servers[0].url`, `paths.<path>.<method>`, `operationId`, `summary`, `parameters`, `requestBody`) and generates a curated `.fl/gate/<id>.toml` connector manifest with `[[tools]]` definitions and `{{FERROGATE:<SECRET_NAME>}}` auth placeholders.

### 4.3 Unified MCP Server (`fl vault mcp`) & Local MCP Registry `v0.1`
* **Unified MCP Server (`fl vault mcp`):**
  * Speaks JSON-RPC 2.0 over `stdio` (`initialize`, `notifications/initialized`, `tools/list`, `tools/call`, `ping`).
  * Aggregates all enabled connectors from `.fl/gate/*.toml` and `~/.config/fl/gate/*.toml` (or a single connector when `--connector <id>` is passed).
  * Also exposes `ferrogate_list_connectors` and `ferrogate_list_secrets` (metadata only, zero plaintext values) so an agent can discover available integrations and secret key names.
* **User-Level & Project-Level Single Registration (`fl vault install`):**
  * Registers the single unified `ferrogate` MCP server (`fl vault mcp`) into `.fl/mcp.toml` (and syncs project vendor files) **or** with `--user` writes the `ferrogate` MCP server entry directly into the user's home vendor configuration files (`~/.claude.json`, `~/.codex/config.toml`, `~/.gemini/antigravity-cli/mcp_config.json`, `~/.config/muse/settings.json`) so any new `.toml` connector dropped into `~/.config/fl/gate/` or `.fl/gate/` becomes immediately available to every vendor CLI without per-vendor reconfiguration.
* **Local MCP Registry `v0.1` (`crates/gate/src/registry.rs` + `/api/v1/gate/registry/v0.1/servers`):**
  * Formats every discovered `ConnectorManifest` as a standard MCP Registry `v0.1` server record (`io.ferroloop.gate/<id>`) so `fl mcp search`, `fl mcp add --from io.ferroloop.gate/<id>`, and `fl mcp upgrade` work seamlessly against `http://127.0.0.1:8420/api/v1/gate/registry`.

---

## 5. Dual-Mode Event Bridge (`crates/gate/src/bridge.rs`)

Each `[[bridges]]` entry in a connector manifest supports inbound event normalization and routing:
1. **Allowlist Verification:**
   * Checks `sender_id` against `allowed_user_ids` (and `guild_id` against `allowed_guild_ids` when non-empty). Unlisted senders are rejected with `BridgeVerdict::UnauthorizedSender` before any agent or LLM token is invoked.
2. **Target Resolution:**
   * Checks an explicit `@<persona>` or `/model <connector>` prefix in the message content, then `channel_routes.get(channel_id)`, then `default_target`.
   * `@<persona>` routes to `wire:<persona>`. `/model <connector> <message>` is honoured only when the bridge lists that exact connector id in `allowed_inline_connectors` (default empty: every inline `/model` is refused with an error that says why). The match is exact and case-sensitive; `*` is not a wildcard and is rejected when the manifest loads. A connector tool can never be chosen from chat text: `/model <connector>:<tool>` is always refused. Only a leading `/model` word is a directive; a connector id outside lowercase ASCII alphanumeric, `-` and `_` is refused. An operator's own `channel_routes` and `default_target` may still name `connector:<id>:<tool>`.
   * Migration: a deployment that relied on inline `/model <connector>` must list the connector in `allowed_inline_connectors`, or its senders now get a refusal.
   * Produces a structured `BridgeDispatchPlan`:
     * `BridgeTarget::WireAgent { persona }` — dispatched by `fl-hub` via `dispatch_operator_prompt_to_route` / `FerroWire`.
     * `BridgeTarget::Connector { connector_id, tool_name }` — invoked directly through `FerroGate`'s proxy using the target LLM connector (`claude`, `gemini`, `openai`, `muse`, or local model).

---

## 6. CLI & Hub REST Surface

### 6.1 CLI (`fl vault` / alias `fl gateway`)
* `fl vault init` — Initialize `~/.config/fl/vault.key` (`0600`) and `~/.local/share/fl/vault.enc`.
* `fl vault set <NAME> [--value <VAL> | --stdin] [--host <HOST>...] [--method <METHOD>...] [--agent <AGENT>...] [--allow-private-network] [--ttl-secs <SECS>]` — Encrypt and store a secret with AAD-bound policy.
* `fl vault list [--json]` — List secret metadata (`name`, `allowed_hosts`, `allowed_methods`, `allowed_agents`, `expires_at`) without exposing values.
* `fl vault remove <NAME>` — Remove a secret from the vault.
* `fl vault call --url <URL> [--method <METHOD>] [--header <K:V>...] [--body <BODY>] [--agent <PERSONA>]` — Execute an outbound HTTP call through the zero-knowledge credential proxy and output scrubber.
* `fl vault connectors [--json]` — List discovered `.fl/gate/*.toml` and `~/.config/fl/gate/*.toml` connectors and their tools/bridges.
* `fl vault import-openapi <SPEC_PATH> --id <ID> [--secret <SECRET_NAME>] [--out <TOML_PATH>]` — Generate a `.fl/gate/<id>.toml` connector manifest from an OpenAPI 3.x JSON document.
* `fl vault mcp [--connector <ID>]` — Run the `FerroGate` JSON-RPC 2.0 MCP server over `stdio`.
* `fl vault install [--user]` — Register the unified `ferrogate` MCP server in `.fl/mcp.toml` + project vendor files (or user-home vendor dot-folders with `--user`).

### 6.2 Hub REST Endpoints (`crates/hub/src/api.rs`)
* `GET /api/v1/gate/secrets` — List redacted secret metadata (`VaultSecretMetadata`).
* `POST /api/v1/gate/secrets` — Owner-only endpoint to create/update a secret in `vault.enc`.
* `DELETE /api/v1/gate/secrets/:name` — Owner-only endpoint to delete a secret.
* `GET /api/v1/gate/connectors` — List discovered connectors, tools, and bridges.
* `POST /api/v1/gate/call` — Execute a proxied, secret-scrubbed HTTP call or connector tool invocation.
* `POST /api/v1/gate/bridges/:bridge_id/webhook` — Receive an inbound bridge event (e.g., Discord webhook/bot relay), verify `allowed_user_ids`, and route to `wire:<persona>` or `connector:<id>`.
* `GET /api/v1/gate/registry/v0.1/servers` (and `/versions/latest`, `/versions/:version`) — Standard MCP Registry `v0.1` endpoints for `fl mcp`.
