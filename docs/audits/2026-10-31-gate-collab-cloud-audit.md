# `fl-gate` security and correctness audit (requested as "gate + collab")

* **Date:** 2026-10-31
* **Base:** `origin/main` at `fcb61f7`
* **Branch:** `cloud/audit-2026-10-31-gate-collab`
* **Scope as requested:** `crates/gate/src/` (`policy`, `guard`, `redact`, `filter`, `secrets`,
  `mcp`, `proxy`, `ratelimit`, `budget`, `circuit`, `wasm`, `adapter_sockets`) and
  `crates/collab/src/` (`lock`, `auction`, `consensus`, `handoff`, `checkpoint`, `plan`,
  `review`, `roster`, `swarm`).
* **Scope as audited:** every file in `crates/gate/src/` that exists on this commit: `adapter.rs`,
  `bridge.rs`, `connector.rs`, `lib.rs`, `mcp.rs`, `proxy.rs`, `registry.rs`, `vault.rs`.
* **Method:** a single-reviewer read of the code, then a regression test for each behaviour that
  was changed. The new tests that can run on the original code were run against it and fail
  there (see *Verification*).

## Scope mismatch — read this first

**`crates/collab` does not exist, and there is no `fl-collab` package.** The workspace members are
`core store exec cli github mcp wire hub gate`. Of the twelve `gate` files in the request, only
`mcp.rs` and `proxy.rs` exist. `policy`, `guard`, `redact`, `filter`, `secrets`, `ratelimit`,
`budget`, `circuit`, `wasm` and `adapter_sockets` appear on neither `main` nor this branch's
remote, and none of the nine `collab` files does. `adapter.rs` carries the "edge guard" and
"cloud socket" traits (`EdgeGuardAdapter`, `SecretProviderAdapter`, `McpRelayAdapter`), and
`vault.rs` holds what the request calls "secrets".

Nothing in this report covers `collab`, the rate limiter, the budget, the circuit breaker, the
WASM host, or a policy engine, because there is no code for them to read. If those live on another
branch or repository, this audit does not cover them. The requested branch name
(`cloud/audit-2026-10-31-gate-collab`) was kept as asked.

## Summary

| ID | Severity | Area | Status |
|---|---|---|---|
| F-1 | High | Tool arguments could name a secret (`{{FERROGATE:…}}`) and have it substituted into the outbound body | Fixed |
| F-2 | Medium | An unsupported-method error printed the URL with the injected secret in its query | Fixed |
| F-3 | Medium | The host allow-list and the HTTP client could read one URL as two different hosts | Fixed |
| F-4 | Medium | The SSRF classifier missed reserved v4 ranges and IPv4 embedded in IPv6; `localhost.` skipped the name filter | Fixed |
| F-5 | Medium | A path parameter of `..` climbed out of the tool's endpoint | Fixed |
| F-6 | Medium | A symlinked connector manifest made the TOML error quote the target file | Fixed |
| F-7 | Medium | A vault record copied under another key decrypted as that other secret | Fixed |
| F-8 | Medium | Two first runs could overwrite each other's master key and orphan the vault | Fixed |
| F-9 | Low | One temp file name per process: concurrent saves collided; failed writes leaked the temp file | Fixed |
| F-10 | Low | Lowercase or padded placeholders passed the policy check but were never replaced; values were re-expanded | Fixed |
| F-11 | Low | A failed or non-UTF-8 response body became an empty `200` | Fixed |
| F-12 | Low | `{:?}` of a `PreparedProxyRequest` printed plaintext secrets | Fixed |
| F-13 | Low | Any registry path other than `/v0.1/servers…` returned the full server list | Fixed |
| F-14 | Low | An audit entry and its newline were two writes | Fixed |
| F-15 | Low | The hex parsers accepted a leading `+` | Fixed |
| F-16 | Low | A manifest id with padding, a repeated tool name, or colliding OpenAPI operations shadowed tools | Fixed |
| O-1 | Medium | DNS rebinding: the SSRF check and the HTTP client resolve the host separately | **Open** |
| O-2 | Medium | No cross-process lock: concurrent `fl vault` writers lose each other's updates | **Open** |
| O-3 | Low | A vault file can be rolled back to an older, broader policy | **Open** |
| O-4 | Low | `ferrogate_list_secrets` ignores `allowed_agents` and the IAM provider | **Open** |
| O-5 | Low | A bridge sender can pick the connector and tool inline; owner bypass when `verify_secret` is unprovisioned | **Open** |
| O-6 | Low | Response scrubbing misses secrets under 4 bytes and secrets inside larger base64 | **Open** |
| O-7 | Info | The registry route is unauthenticated; duplicate tool names across connectors; audit-log limits | **Open** |

Line numbers are from this commit, after the fixes. Function names are the stable reference.

---

## Fixed

### F-1 — Tool arguments could name a secret (High)

`ConnectorManifest::build_tool_request` (`crates/gate/src/connector.rs:138`) put agent-supplied
arguments into the JSON body untouched. `prepare_proxied_request` then scans the **whole** body
for `{{FERROGATE:KEY}}`, resolves each key and substitutes it. A call such as
`chat_post {content: "{{FERROGATE:CHAT_TOKEN}}"}` therefore sent the real token to the upstream
as message text, where anyone who can read the channel sees it. The response scrubber does not
help: it cleans what comes back to the agent, not what the upstream stored.

The vault policy still applies, so only a secret whose `allowed_hosts` and `allowed_agents` fit the
destination can leak this way. That is exactly the secret the connector already uses.

**Fix.** `find_placeholder` (`connector.rs:311`) walks every string, array element and object key in
the arguments, and `build_tool_request` (`connector.rs:146`) refuses any that holds
`{{FERROGATE:` with `InvalidToolArgs`. Path and query values were already percent-encoded
(`{` becomes `%7B`), so the body was the only route. The manifest's own placeholders are untouched.
**Test:** `connector::tests::an_argument_cannot_name_a_secret`.

### F-2 — An error message printed the injected secret (Medium)

`execute_proxied_request_with_provider` substituted placeholders first and rejected an
unsupported method second. The `GateError::InvalidUrl` it returned carried `prepared.url`
(`proxy.rs:702`), which holds the plaintext of any secret placed in the query. With
`method = "OPTIONS"` and `?key={{FERROGATE:API_KEY}}` the secret reached the caller's error string,
and through `fl vault mcp` the agent's tool output.

**Fix.** The error reports `req.url`, the unsubstituted URL.
**Test:** `proxy::tests::an_unsupported_method_error_never_prints_the_injected_secret`.

### F-3 — Host allow-list and HTTP client could disagree about the host (Medium)

`parse_url_scheme_host_port` (`proxy.rs:96`) ended the authority at `/ ? #` only. For
`https://evil.com\.allowed.com/x` it returned the host `evil.com\.allowed.com`, which satisfies a
`*.allowed.com` pattern. Whether `ureq` itself rejects the backslash was not tested here. A client that
follows the WHATWG URL rules reads `\` as the end of the authority and would send the secret to
`evil.com`, so the allow-list must not depend on the client's leniency. The same function ignored trailing text after
an IPv6 literal (`[::1]evil`) and accepted a port written `+80` (`u16::from_str` allows `+`).

**Fix.** A non-bracketed host may hold only ASCII letters, digits, `.`, `-` and `_`
(`proxy.rs:152`); a bracketed host must parse as an `Ipv6Addr`; text after `]` must be empty or
`:port`; a port is one or more ASCII digits (`parse_port`, `proxy.rs:175`).
**Test:** `proxy::tests::authorities_that_clients_read_differently_are_refused`.

### F-4 — SSRF classifier gaps (Medium)

`is_private_or_loopback_ip` (`proxy.rs:200`) missed `0.0.0.0/8` apart from `0.0.0.0`,
`240.0.0.0/4`, `192.0.0.0/24`, `198.18.0.0/15`, site-local `fec0::/10`, `2001:db8::/32`, and any
IPv4 address embedded in IPv6 other than the `::ffff:` form: IPv4-compatible (`::127.0.0.1`),
NAT64 (`64:ff9b::/96`, so `64:ff9b::a9fe:a9fe` reached `169.254.169.254` through a NAT64
gateway) and 6to4 (`2002::/16`). Separately `verify_host_ssrf` compared `localhost`, `.localhost`,
`.local` and `.internal` against the host as written, so a trailing dot (`metadata.google.internal.`)
skipped the name filter and left blocking to the DNS result alone.

**Fix.** `is_private_v4` (`proxy.rs:227`) and the extended IPv6 branch cover the ranges above,
embedded addresses are classified by the IPv4 they hold, and the trailing dot is trimmed
(`proxy.rs:251`). Public addresses, including a NAT64 or 6to4 wrapper around one, stay open.
**Tests:** `proxy::tests::reserved_and_embedded_addresses_count_as_private`,
`proxy::tests::a_trailing_dot_does_not_dodge_the_local_name_filter`.

### F-5 — Path parameter `..` escaped the endpoint (Medium)

`encode_uri_component` leaves `.` unencoded, so `channel_id = ".."` rendered
`/channels/../messages`, which a server or intermediary normalizes to `/messages`. A tool pinned
to one endpoint could reach another on the same host.

**Fix.** `build_tool_request` refuses an empty, `.` or `..` path value (`connector.rs:164`). A
value like `a/../b` is still safe: the `/` becomes `%2F`.
**Test:** `connector::tests::a_path_parameter_cannot_climb_out_of_its_segment`.

### F-6 — Symlinked manifest disclosed its target through the parse error (Medium)

`load_connectors_from_dir` used `Path::is_file`, which follows symlinks, and parsed the result.
`toml` quotes the offending line in its error. A repository that ships
`.fl/gate/x.toml -> ~/.config/fl/vault.key` therefore had the master key printed into the error
that `fl` shows (and that an agent running `fl vault mcp` reads). The vault already refuses
symlinks for its own files; the connector directory did not.

**Fix.** The directory scan lists symlinks (`connector.rs:397`) and `reject_if_symlink` refuses
them before any read (`connector.rs:409`). The scan keeps regular files only, so a FIFO is not
opened. **Test:** `connector::tests::a_manifest_symlink_is_refused_and_its_target_is_never_quoted`
(Unix only).

### F-7 — A vault record decrypted under any key (Medium)

The AAD binds `metadata.name` (`canonical_aad`), but `decrypt_raw` looked the record up by the map
key and never compared the two. Someone who can write `vault.enc` but does not hold the master
key could copy the record of a low-value secret under the name of a high-value one. A request for
the high-value name then resolved to the low-value plaintext and its policy, and scrubbing
labelled it with the wrong name. This is an integrity break, not a disclosure.

**Fix.** `decrypt_raw` returns `VaultTampered` when `record.metadata.name != key_name`
(`vault.rs:267`). **Test:**
`vault::tests::a_record_moved_under_another_name_is_not_served_as_that_secret`.

### F-8 — First-run race overwrote the master key (Medium)

`load_or_init_master_key` checked `key_path.exists()`, generated a key and renamed it into place.
Two processes starting together on a new machine (for example two `fl` invocations or a hub and a
CLI) both passed the check; the second `rename` replaced the first key, and every secret the first
process had already sealed became undecryptable.

**Fix.** `install_new_master_key` (`vault.rs:562`) writes a private temp file and publishes it with
`hard_link`, which fails if the destination exists. The loser reads and adopts the winner's key.
**Test:** `vault::tests::the_loser_of_a_first_run_race_adopts_the_existing_master_key`. The test
exercises the install step directly; a true two-process race is not reproduced.

### F-9 — Shared temp file name; leaked temp files (Low)

`write_atomic_0600` named its temp file `<stem>.tmp.<pid>`. Two threads saving one vault shared
that name, and `vault.key` and `vault.enc` in the same directory also mapped to the same
`vault.tmp.<pid>`; interleaved writes or a missing-file `rename` followed. A failed write or
rename left the temp file, which can hold ciphertext or a master key, on disk. The file was also
created with the default mode and `chmod`ed afterwards.

**Fix.** `unique_tmp_path` (`vault.rs:647`) adds the full file name and a process-wide counter
(`AtomicU64::fetch_add`); `write_tmp_0600` (`vault.rs:660`) opens with `create_new` and mode
`0600` from the start and removes the file on error; `write_atomic_0600` removes it if the rename
fails (`vault.rs:695`).
**Tests:** `temp_paths_are_unique_per_call_and_per_target_file`, `a_failed_publish_removes_its_temp_file`,
`temp_files_are_born_private`, and the stress test
`concurrent_saves_of_one_vault_all_succeed_and_leave_it_readable`. That stress test is a
detector, not a deterministic reproduction; it failed against the original code in the run
described below.

### F-10 — Placeholder replacement disagreed with the policy check (Low)

`extract_placeholder_keys` normalizes names (trim, uppercase), so `{{FERROGATE:api_token}}` was
authorized and logged as `API_TOKEN`. The replacement then searched for the exact text
`{{FERROGATE:API_TOKEN}}`, left the lowercase placeholder in the request and sent it upstream: a
broken request that the audit log recorded as allowed. Replacement also ran one secret at a
time, so a secret value that itself contained another referenced placeholder was expanded a
second time.

**Fix.** `substitute_placeholders` (`proxy.rs:482`) replaces in a single pass, normalizing each key
the way the policy check does, and never rescans a substituted value.
**Test:** `proxy::tests::placeholders_are_replaced_as_the_policy_check_read_them`.

### F-11 — A failed response body became an empty `200` (Low)

`read_to_string().unwrap_or_default()` (formerly `proxy.rs`, now `proxy.rs:727`) turned a
non-UTF-8 body, a read error and an over-limit body into `""`, with status and headers intact.
**Fix.** The body is read as bytes and decoded lossily; a read failure or over-limit body returns
`UpstreamHttp` with the cause scrubbed.
**Test:** `proxy::tests::a_non_utf8_response_body_is_decoded_not_dropped`.

### F-12 — `Debug` of a prepared request printed secrets (Low)

`PreparedProxyRequest` derived `Debug`, so `{:?}`, `unwrap()` on an `Err`, or a log of one printed
the substituted URL, headers and body. `ExternalSecret` already redacts for the same reason.
**Fix.** A manual `Debug` (`proxy.rs:48`) prints method, host, the URL without its query, header
names, body length and the names of injected secrets.
**Test:** `proxy::tests::debug_of_a_prepared_request_holds_no_plaintext`.

### F-13 — Unknown registry paths returned the whole list (Low)

`respond_registry_v01` used `strip_prefix("v0.1/servers").unwrap_or("")`, so `/other` and
`/v0.2/servers` fell into the list branch. The hub serves this route without authentication
(`crates/hub/src/api.rs:383`), and trims only `/api/v1/gate/registry`, so any path under it
answered `200` with every connector.
**Fix.** A path without the prefix is `404` (`registry.rs:81`).
**Test:** `registry::tests::a_path_outside_v01_servers_is_not_found_instead_of_the_full_list`.

### F-14 — Audit entry written in two syscalls (Low)

`writeln!(f, "{line}")` on an unbuffered `File` issues the text and the newline as separate
writes. With several threads or processes appending, an entry could be split from its newline.
**Fix.** The newline is appended to the string and written with one `write_all` (`vault.rs:396`).
**Test:** `vault::tests::concurrent_audit_entries_stay_one_json_object_per_line` (a stress
detector).

### F-15 — Hex parsers accepted a leading `+` (Low)

`u8::from_str_radix("+a", 16)` succeeds, so a 64-character master key written `+1+1…` parsed
as a key of mostly weak bytes, and `percent_decode` read `%+1` as `0x01`.
**Fix.** The master key must be all ASCII hex digits (`vault.rs:601`); `percent_decode` requires
two hex digits (`registry.rs:200`).
**Tests:** `vault::tests::a_master_key_must_be_bare_hex_digits`,
`registry::tests::percent_decoding_ignores_a_signed_pair`.

### F-16 — Ambiguous connector ids and tool names (Low)

`validate` checked the *trimmed* id but the manifest kept the padded one, so ` chat` was stored,
matched and published with its space. A manifest could declare two tools with one name: both
appeared in `tools/list` but only the first was callable. The OpenAPI importer produced such
duplicates whenever two paths sanitized alike (`/a-b`, `/a_b`).
**Fix.** The id is validated as written (`connector.rs:88`), duplicate tool names are an error
(`connector.rs:107`), and the importer numbers colliding names `name_2`, `name_3`
(`unique_tool_name`, `connector.rs:601`).
**Tests:** `connector::tests::ids_and_tool_names_must_be_unambiguous`,
`connector::tests::openapi_operations_that_sanitize_alike_get_distinct_tool_names`.

---

## Open

### O-1 — DNS rebinding (Medium)

`verify_host_ssrf` resolves the name (`proxy.rs:271`) and `ureq` resolves it again when it
connects. A resolver that answers a public address first and `127.0.0.1` second passes the check
and is dialled on the loopback. A resolution **failure** also passes the check (`if let Ok`).
The fix is to connect to the vetted address: a custom `ureq` resolver that filters with
`is_private_or_loopback_ip` at connect time. That changes the transport and TLS name handling, so
it was not done in an audit. Mitigations today: secrets carry `allowed_hosts`, and a host that
resolves privately for a non-allow-listed secret is still refused. The risk is limited to an
allow-listed name whose DNS the attacker controls.

### O-2 — No cross-process lock on the vault (Medium)

`Vault` holds a snapshot and `save` rewrites the whole file (`vault.rs:338`). Two `fl vault set`
processes, or a CLI and the hub, each read the file, add a secret and save; the later write drops
the earlier secret silently. Needs an advisory file lock around read-modify-write or a re-read
under lock in `set` and `remove`.

### O-3 — Vault rollback (Low)

The AAD binds a record's policy to its ciphertext but nothing orders versions. Someone who can
write `vault.enc` can restore an older file with a broader `allowed_hosts` and it verifies.

### O-4 — `ferrogate_list_secrets` ignores per-agent policy (Low)

The built-in list tool (`mcp.rs:160`) returns every secret's name, host list and method list to
any caller and is excluded from `authorize_mcp_tool`, as its doc comment says. It never returns
values, but it tells an agent which secrets and hosts exist, including those whose
`allowed_agents` exclude it.

### O-5 — Bridge routing and webhook trust (Low)

* An allow-listed sender can override `channel_routes` with `/model <connector>[:tool]` or
  `@persona` in the message (`bridge.rs:247`), including naming a connector tool. Allow-listing a
  user therefore means trusting them with every connector the hub knows.
* `evaluate_inbound_bridge_event` never compares `event.bridge_id` with `bridge.id`.
* When `verify_secret` names a secret that is not in the vault, an **owner** caller is accepted
  without any header (`bridge.rs:32`), as documented. A misspelt name therefore disables
  verification for owner-authenticated callers.

### O-6 — Scrubber coverage (Low)

`scrub_secrets` skips secrets shorter than 4 bytes (`proxy.rs:513`) and matches base64 only at
the secret's own alignment, so a secret embedded in a longer base64 stream is not caught. The
fixed token-prefix list (`sk-ant-`, `ghp_`, …) is a heuristic.

### O-7 — Smaller observations (Info)

* `host_matches_pattern` lets `*.example.com` match the apex `example.com` (`proxy.rs:183`). The
  existing test data lists both forms, so it looks intended; say so in the docs.
* Two connectors may declare one tool name; `handle_mcp_jsonrpc_request_with_secret_provider`
  calls the first in id order (`mcp.rs:178`) while `tools/list` shows both.
* The audit log records only `allowed` outcomes, ignores write errors and is never rotated
  (`vault.rs:367`).
* The registry route needs no credentials and lists disabled connectors as `deprecated`.
* The master key file's mode is not checked on read, and `FL_VAULT_MASTER_KEY` in the environment
  takes precedence over the file.
* An OpenAPI import copies `summary` and `description` from an untrusted spec into tool
  descriptions that an agent reads.

---

## Verification

* **Commands.** `cargo test -p fl-gate --ignore-rust-version` (35 unit + 40 integration tests, all
  pass), `cargo clippy -p fl-gate --all-targets --ignore-rust-version -- -D warnings` (clean), and
  `cargo test -p fl-hub -p fl-cli --ignore-rust-version` (all pass). The `fl-collab` form of these
  commands in the request cannot run: that package does not exist.
* **Toolchain.** The workspace declares `rust-version = "1.98"` and this environment has `rustc
  1.97.0`, so every cargo command needed `--ignore-rust-version`. Nothing else differed.
* **Regression proof.** The new test modules were copied onto the original (`fcb61f7`) sources
  in a scratch worktree. Of the 32 tests that compile there, **19 fail** — the 13 that pass are the
  pre-existing ones. The three vault tests that call new helpers (`unique_tmp_path`,
  `write_tmp_0600`, `install_new_master_key`) cannot compile on the original, so they are not
  proven as regressions; they test the new code directly. The two concurrency tests
  (`concurrent_saves_…`, `concurrent_audit_entries_…`) failed on the original, but a stress test
  can pass by luck on unfixed code and should be read as detectors.
* **No use of `Atomic*::fetch_update`/`try_update`.** The only new atomic is
  `AtomicU64::fetch_add` for the temp-file counter; nothing here needed a compare-exchange loop.
* **No integer-overflow, UTF-8 boundary or panic defect was found in the files read.** The
  slicing in `host_matches_pattern`, `redact_prefixed_tokens` and `percent_decode` is guarded;
  `lease_ttl_secs` uses saturating arithmetic and `At::from_unix_millis` clamps at year 9999.

## Not done

* Everything named in the request that does not exist (see *Scope mismatch*).
* A single-reviewer read; no fuzzing, no dynamic test of TLS, and no real DNS rebinding test.
* The hub (`crates/hub/src/api.rs`) was read only where it calls into `fl-gate`.
* `WORKFLOW.md` asks that review and remediation be run by separate agents. This audit and its
  fixes were produced in one session and need an independent review before landing.
