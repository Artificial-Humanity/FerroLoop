# `fl-gate` security and correctness audit (requested as "gate + collab")

* **Date:** 2026-10-31
* **Base:** `origin/main` at `84741e0`
* **Branch:** `felicity/cloud-gate-collab-audit`. The autonomous cloud agent produced it on the
  origin branch `cloud/audit-2026-10-31-gate-collab` (commit `6a6b60e`, based on `fcb61f7`); an
  independent review followed, and this branch lands the result as a fresh commit.
* **Scope as requested:** `crates/gate/src/` (`policy`, `guard`, `redact`, `filter`, `secrets`,
  `mcp`, `proxy`, `ratelimit`, `budget`, `circuit`, `wasm`, `adapter_sockets`) and
  `crates/collab/src/` (`lock`, `auction`, `consensus`, `handoff`, `checkpoint`, `plan`,
  `review`, `roster`, `swarm`).
* **Scope as audited:** every file in `crates/gate/src/` that exists on this commit: `adapter.rs`,
  `bridge.rs`, `connector.rs`, `lib.rs`, `mcp.rs`, `proxy.rs`, `registry.rs`, `vault.rs`.
* **Method:** a single-reviewer read of the code, then a regression test for each behaviour that
  was changed. The new tests that can run on the original code were run against it and fail
  there (see *Verification*). An independent security review of the result then found the gaps
  recorded below as "review follow-up"; those are fixed in this commit.

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
branch or repository, this audit does not cover them. The cloud agent kept the
requested branch name (`cloud/audit-2026-10-31-gate-collab`).

## Summary

| ID | Severity | Area | Status |
|---|---|---|---|
| F-1 | High | Tool arguments could name a secret (`{{FERROGATE:…}}`) and have it substituted into the outbound body | Fixed |
| F-2 | Medium | An unsupported-method error printed the URL with the injected secret in its query | Fixed |
| F-3 | Medium | The host allow-list and the HTTP client could read one URL as two different hosts | Fixed |
| F-4 | Medium (address ranges); Low (trailing dot) | The SSRF classifier missed reserved v4 ranges and IPv4 embedded in IPv6; `localhost.` skipped the name filter | Fixed |
| F-5 | Medium | A path parameter of `..` climbed out of the tool's endpoint | Fixed |
| F-6 | Medium | A symlinked connector manifest made the TOML error quote the target file | Fixed (symlinks are skipped with a warning; errors quote no source) |
| F-7 | Medium | A vault record copied under another key decrypted as that other secret | Fixed |
| F-8 | Medium | Two first runs could overwrite each other's master key and orphan the vault | Fixed |
| F-9 | Low | One temp file name per process: concurrent saves collided; failed writes leaked the temp file | Fixed |
| F-10 | Low | Lowercase or padded placeholders passed the policy check but were never replaced; values were re-expanded | Fixed |
| F-11 | Low | A failed or non-UTF-8 response body became an empty `200` | Fixed |
| F-12 | Low | `{:?}` of a `PreparedProxyRequest`, of a `Vault` (master key) or of `SetSecretArgs` (value) printed plaintext secrets | Fixed |
| F-13 | Low | Any registry path other than `/v0.1/servers…` returned the full server list | Fixed |
| F-14 | Low | An audit entry and its newline were two writes | Fixed |
| F-15 | Low | The hex parsers accepted a leading `+` | Fixed |
| F-16 | Low | A manifest id with padding, a repeated tool name, or colliding OpenAPI operations shadowed tools | Fixed |
| O-1 | Medium | DNS rebinding: the SSRF check and the HTTP client resolve the host separately | **Open** |
| O-2 | Medium | No cross-process lock: concurrent `fl vault` writers lose each other's updates | **Open** |
| O-3 | Low | A vault file can be rolled back to an older, broader policy | **Open** |
| O-4 | Low | `ferrogate_list_secrets` ignores `allowed_agents` and the IAM provider | **Open** |
| O-5 | Low (cloud rating); the review rates it the most dangerous open item | A bridge sender can pick the connector and tool inline; owner bypass when `verify_secret` is unprovisioned | **Partly fixed** (inline routing and owner bypass fixed; `bridge_id` compare open) |
| O-6 | Low | Response scrubbing misses secrets under 4 bytes and secrets inside larger base64 | **Open** |
| O-7 | Info | The registry route is unauthenticated; duplicate tool names across connectors; audit-log limits | **Open** |

Line numbers are from the files on this branch (re-derived after the review follow-up). Function names are the stable reference.

---

## Fixed

### F-1 — Tool arguments could name a secret (High)

`ConnectorManifest::build_tool_request` (`crates/gate/src/connector.rs:178`) put agent-supplied
arguments into the JSON body untouched. `prepare_proxied_request` then scans the **whole** body
for `{{FERROGATE:KEY}}`, resolves each key and substitutes it. A call such as
`chat_post {content: "{{FERROGATE:CHAT_TOKEN}}"}` therefore sent the real token to the upstream
as message text, where anyone who can read the channel sees it. The response scrubber does not
help: it cleans what comes back to the agent, not what the upstream stored.

The vault policy still applies, so only a secret whose `allowed_hosts` and `allowed_agents` fit the
destination can leak this way. That is exactly the secret the connector already uses.

**Fix.** `find_placeholder` (`connector.rs:351`) walks every string, array element and object key in
the arguments, and `build_tool_request` (`connector.rs:186`) refuses any that holds
`{{FERROGATE:` with `InvalidToolArgs`. Path and query values were already percent-encoded
(`{` becomes `%7B`), so the body was the only route. The manifest's own placeholders are untouched.
**Test:** `connector::tests::an_argument_cannot_name_a_secret`.

### F-2 — An error message printed the injected secret (Medium)

`execute_proxied_request_with_provider` substituted placeholders first and rejected an
unsupported method second. The `GateError::InvalidUrl` it returned carried `prepared.url`,
which held the plaintext of any secret placed in the query. With
`method = "OPTIONS"` and `?key={{FERROGATE:API_KEY}}` the secret reached the caller's error string,
and through `fl vault mcp` the agent's tool output.

**Fix.** The error reports `req.url`, the unsubstituted URL (`proxy.rs:716`).
**Test:** `proxy::tests::an_unsupported_method_error_never_prints_the_injected_secret`.

### F-3 — Host allow-list and HTTP client could disagree about the host (Medium)

`parse_url_scheme_host_port` (`proxy.rs:96`) ended the authority at `/ ? #` only. For
`https://evil.com\.allowed.com/x` it returned the host `evil.com\.allowed.com`, which satisfies a
`*.allowed.com` pattern. Whether `ureq` itself rejects the backslash was not tested here. A client that
follows the WHATWG URL rules reads `\` as the end of the authority and would send the secret to
`evil.com`, so the allow-list must not depend on the client's leniency. The same function ignored trailing text after
an IPv6 literal (`[::1]evil`) and accepted a port written `+80` (`u16::from_str` allows `+`).

**Fix.** A non-bracketed host may hold only ASCII letters, digits, `.`, `-` and `_`
(`proxy.rs:158`); a bracketed host must parse as an `Ipv6Addr`; text after `]` must be empty or
`:port`; a port is one or more ASCII digits (`parse_port`, `proxy.rs:175`).
**Test:** `proxy::tests::authorities_that_clients_read_differently_are_refused`.

### F-4 — SSRF classifier gaps (Medium for the address ranges, Low for the trailing dot)

`is_private_or_loopback_ip` (`proxy.rs:209`) missed `0.0.0.0/8` apart from `0.0.0.0`,
`240.0.0.0/4`, `192.0.0.0/24`, `198.18.0.0/15`, site-local `fec0::/10`, `2001:db8::/32`, and any
IPv4 address embedded in IPv6 other than the `::ffff:` form: IPv4-compatible (`::127.0.0.1`),
NAT64 (`64:ff9b::/96`, so `64:ff9b::a9fe:a9fe` reached `169.254.169.254` through a NAT64
gateway) and 6to4 (`2002::/16`). A literal address in one of these forms passed the classifier, so
the Medium rating stands for the ranges. Separately `verify_host_ssrf` compared `localhost`,
`.localhost`, `.local` and `.internal` against the host as written, so a trailing dot
(`metadata.google.internal.`) skipped the name filter. That part is **Low**: the DNS check that
follows already blocked `localhost.` in practice, because it resolves to the loopback address;
the trim only restores the name filter as a first line of defence.

**Fix.** `is_private_v4` (`proxy.rs:241`) and the extended IPv6 branch cover the ranges above,
embedded addresses are classified by the IPv4 they hold, and the trailing dot is trimmed
(`proxy.rs:266`). Public addresses, including a NAT64 or 6to4 wrapper around one, stay open.

**Review follow-up.** The review found four more forms still unclassified. They are added, and the
table test covers each:

* Blocked **outright**, whatever they embed: NAT64 local-use `64:ff9b:1::/48` (RFC 8215, an operator
  may map any address into it), and `192.88.99.0/24` (the 6to4 relay anycast block, also when it
  appears embedded, for example `::ffff:192.88.99.1` or `2002:c058:6301::`).
* Judged **by the embedded IPv4 address**, like the mapped, NAT64 and 6to4 forms: ISATAP
  (`<any prefix>:0:5efe:a.b.c.d`, also with the universal bit set as `:200:5efe:`), and Teredo
  `2001::/32`, where the address is blocked when either the server address (bits 32 to 63) or the
  client address (the last 32 bits, stored XOR `0xffff`) is private. A Teredo or ISATAP address
  around a public IPv4 address stays open.

**Tests:** `proxy::tests::reserved_and_embedded_addresses_count_as_private` (a table of blocked and
open addresses, including the four forms above), `proxy::tests::a_trailing_dot_does_not_dodge_the_local_name_filter`.

### F-5 — Path parameter `..` escaped the endpoint (Medium)

`encode_uri_component` leaves `.` unencoded, so `channel_id = ".."` rendered
`/channels/../messages`, which a server or intermediary normalizes to `/messages`. A tool pinned
to one endpoint could reach another on the same host.

**Fix.** `build_tool_request` refuses an empty, `.` or `..` path value (`connector.rs:204`). A
value like `a/../b` is still safe: the `/` becomes `%2F`.
**Test:** `connector::tests::a_path_parameter_cannot_climb_out_of_its_segment`.

### F-6 — Symlinked manifest disclosed its target through the parse error (Medium)

`load_connectors_from_dir` used `Path::is_file`, which follows symlinks, and parsed the result.
`toml` quotes the offending line in its error. A repository that ships
`.fl/gate/x.toml -> ~/.config/fl/vault.key` therefore had the master key printed into the error
that `fl` shows (and that an agent running `fl vault mcp` reads). The vault already refuses
symlinks for its own files; the connector directory did not.

**First fix (cloud agent).** The scan refused a symlinked manifest with an `Err`. The review found
two problems: any symlinked manifest, even a valid one, then made `discover_connectors` fail for
the **whole** directory, so a user who manages `~/.config/fl/gate` with stow or dotfiles lost every
connector (and the hub, which calls `discover_connectors(..).unwrap_or_default()` at
`crates/hub/src/api.rs:387`, silently served an empty registry); and the leak was only partly
closed, because a symlinked `.fl/gate` **directory** still passed `is_dir()`, and `toml` still
echoed source lines for any other `*.toml` that failed to parse.

**Current behaviour.**

* **TOML errors carry no source.** `toml_error_reason` (`connector.rs:74`) formats
  `line L, column C: <message>`. `redact_toml_message` (`connector.rs:91`) hides the key or value
  the parser quotes (``unknown field `…` ``, `invalid type: string "…"`), because either can be a
  pasted token; the text after `, expected` and `missing field` name this crate's own schema and
  stay. `ConnectorManifest::parse_toml` is the only place in `fl-gate` that formats a
  `toml::de::Error`.
* **A symlinked manifest is skipped, not fatal.** `load_connectors_from_dir` (`connector.rs:495`)
  uses `symlink_metadata`: a symlink named `*.toml` is not opened, and the other manifests still
  load. A regular file that is not valid TOML is still an error (its text holds the position and the
  parser message only). Only regular files are read, so a FIFO is not opened.
* **Warnings.** The new `discover_connectors_with_warnings` (`connector.rs:419`) returns
  `ConnectorDiscovery { connectors, warnings }`, one line per skip naming the path of the link
  only, never its target or any content. `discover_connectors` (`connector.rs:403`) keeps its
  signature (the CLI and the hub call it, so neither changed), calls the new function and prints
  each distinct warning once per process on stderr (`warn_once`). The new function is reachable as
  `fl_gate::connector::discover_connectors_with_warnings`; it is not re-exported from `lib.rs`.
* **A symlinked directory is not followed.** `connector_dir_usable` (`connector.rs:464`) refuses
  `~/.config/fl/gate` and `<project>/.fl/gate` when either is a symlink, and also a symlinked
  `<project>/.fl`, which a repository controls. They are skipped with a warning, never read. The
  rule is the same for the user and project directories, so a user who symlinks the whole
  `~/.config/fl/gate` directory, or its manifests, to a dotfiles repository gets a warning and no
  connectors from it; copying the files in place restores them. Parents above that (for example a
  symlinked `~/.config/fl`) are not checked, because they are the user's own.

**Tests:** `connector::tests::a_symlinked_manifest_is_skipped_with_a_warning_and_the_rest_still_load`,
`connector::tests::a_symlinked_connector_directory_is_not_followed`,
`connector::tests::a_malformed_manifest_error_quotes_no_source` (the first and second are Unix
only).

### F-7 — A vault record decrypted under any key (Medium)

The AAD binds `metadata.name` (`canonical_aad`), but `decrypt_raw` looked the record up by the map
key and never compared the two. Someone who can write `vault.enc` but does not hold the master
key could copy the record of a low-value secret under the name of a high-value one. A request for
the high-value name then resolved to the low-value plaintext and its policy, and scrubbing
labelled it with the wrong name. This is an integrity break, not a disclosure.

**Fix.** `decrypt_raw` returns `VaultTampered` when `record.metadata.name != key_name`
(`vault.rs:297`). **Test:**
`vault::tests::a_record_moved_under_another_name_is_not_served_as_that_secret`.

### F-8 — First-run race overwrote the master key (Medium)

`load_or_init_master_key` checked `key_path.exists()`, generated a key and renamed it into place.
Two processes starting together on a new machine (for example two `fl` invocations or a hub and a
CLI) both passed the check; the second `rename` replaced the first key, and every secret the first
process had already sealed became undecryptable.

**Fix.** `install_new_master_key` (`vault.rs:592`) writes a private temp file and publishes it with
`hard_link`, which fails if the destination exists. The loser reads and adopts the winner's key.

**Review follow-up.** `hard_link` has no fallback in that version, so on a filesystem without hard
links (some FUSE, SMB or bind mounts) first-run key creation failed with op `link`. Now any error
other than `AlreadyExists` (`link_failure_needs_fallback`, `vault.rs:603`) falls back to
`create_new_0600` (`vault.rs:647`): an exclusive create with mode `0600` that writes the whole key.
It stays exclusive: `AlreadyExists` from either step still means another process won, and the key
at `key_path` is read and adopted. The temp file is removed in every case. One residual window
exists in the fallback only: between the exclusive create and the write a reader can see a short
key file and report it corrupt (it never overwrites it).

**Tests:** `vault::tests::the_loser_of_a_first_run_race_adopts_the_existing_master_key`,
`vault::tests::a_filesystem_without_hard_links_still_gets_a_private_master_key`,
`vault::tests::the_fallback_still_adopts_a_key_that_another_process_won`,
`vault::tests::only_already_exists_skips_the_exclusive_create_fallback`. The publish step is a
parameter of `install_new_master_key_with` (`vault.rs:609`), so the tests stand in for a filesystem
without hard links. They exercise the install step directly; a true two-process race is not
reproduced.

### F-9 — Shared temp file name; leaked temp files (Low)

`write_atomic_0600` named its temp file `<stem>.tmp.<pid>`. Two threads saving one vault shared
that name, and `vault.key` and `vault.enc` in the same directory also mapped to the same
`vault.tmp.<pid>`; interleaved writes or a missing-file `rename` followed. A failed write or
rename left the temp file, which can hold ciphertext or a master key, on disk. The file was also
created with the default mode and `chmod`ed afterwards.

**Fix.** `unique_tmp_path` (`vault.rs:721`) adds the full file name and a process-wide counter
(`AtomicU64::fetch_add`); `write_tmp_0600` (`vault.rs:737`) opens with `create_new` and mode
`0600` from the start and removes the file on error; `write_atomic_0600` removes it if the rename
fails (`vault.rs:802`).

**Review follow-up.** With `create_new`, a temp file left by a crash made a later process that
reuses the same PID fail with `EEXIST` on its first save (the old code truncated and tolerated it).
`write_tmp_0600_with` (`vault.rs:745`) now skips a name that already exists and tries the next
counter value, up to `TMP_NAME_ATTEMPTS` (8, `vault.rs:733`), then returns an error. It never
truncates or removes a file it did not create.

**Tests:** `temp_paths_are_unique_per_call_and_per_target_file`, `a_failed_publish_removes_its_temp_file`,
`temp_files_are_born_private`, `a_leftover_temp_file_from_a_crashed_process_does_not_block_a_save`,
`temp_name_retries_are_bounded`, and the stress test
`concurrent_saves_of_one_vault_all_succeed_and_leave_it_readable`. That stress test is a
detector, not a deterministic reproduction; it failed against the original code in the cloud
agent's run.

### F-10 — Placeholder replacement disagreed with the policy check (Low)

`extract_placeholder_keys` normalizes names (trim, uppercase), so `{{FERROGATE:api_token}}` was
authorized and logged as `API_TOKEN`. The replacement then searched for the exact text
`{{FERROGATE:API_TOKEN}}`, left the lowercase placeholder in the request and sent it upstream: a
broken request that the audit log recorded as allowed. Replacement also ran one secret at a
time, so a secret value that itself contained another referenced placeholder was expanded a
second time.

**Fix.** `substitute_placeholders` (`proxy.rs:497`) replaces in a single pass, normalizing each key
the way the policy check does, and never rescans a substituted value.
**Test:** `proxy::tests::placeholders_are_replaced_as_the_policy_check_read_them`.

### F-11 — A failed response body became an empty `200` (Low)

`read_to_string().unwrap_or_default()` (formerly `proxy.rs`, now `proxy.rs:742`) turned a
non-UTF-8 body, a read error and an over-limit body into `""`, with status and headers intact.
**Fix.** The body is read as bytes and decoded lossily; a read failure or over-limit body returns
`UpstreamHttp` with the cause scrubbed.
**Behaviour change.** A body over `ureq`'s default 10 MiB limit used to return an empty `200`; it
now returns `UpstreamHttp`. A caller that fetched large responses and ignored the empty body now
sees an error.
**Test:** `proxy::tests::a_non_utf8_response_body_is_decoded_not_dropped`.

### F-12 — `Debug` printed secrets (Low)

`PreparedProxyRequest` derived `Debug`, so `{:?}`, `unwrap()` on an `Err`, or a log of one printed
the substituted URL, headers and body. `ExternalSecret` already redacts for the same reason.
**Fix (cloud agent).** A manual `Debug` (`proxy.rs:48`) prints method, host, the URL without its
query, header names, body length and the names of injected secrets.
**Test:** `proxy::tests::debug_of_a_prepared_request_holds_no_plaintext`.

**Review follow-up.** The first pass checked only that type. Two more types leaked and are fixed in
this commit:

* `Vault` derived `Debug`, so `{:?}` printed `master_key: [u8; 32]`, and so did `unwrap_err()` or
  `expect_err()` on an `Ok(Vault)` (the panic message prints the `Ok` value). A manual `Debug`
  (`vault.rs:113`) prints the path and the number of secrets only.
* `SetSecretArgs` derived `Debug` and printed the plaintext `value`. A manual `Debug`
  (`vault.rs:62`) prints `value: "<redacted>"` and the other fields.

**Test:** `vault::tests::debug_of_a_vault_and_of_set_args_holds_no_key_or_value` formats `{:?}` and
`{:#?}` of both, wrapped in `Result` and `Option`, and the panic text of `unwrap_err` and
`expect_err`, and asserts that neither the key (decimal, hex and field name) nor the value appears.
It fails if the manual impls are replaced with `derive(Debug)`.

**Types checked** for a derived or hand-written `Debug` that could print a secret value, a master
key, a token or a credential (every `derive(Debug)` and `Debug` impl in `crates/gate/src`, and the
gate types as used in `crates/hub`, `crates/cli` and `crates/mcp`):

* *Leaked, fixed:* `Vault`, `SetSecretArgs`, `PreparedProxyRequest` (F-12).
* *Redacts already:* `ExternalSecret` (`adapter.rs`, `value` is `[REDACTED]`).
* *Clean:* `VaultSecretMetadata` (names, host and method lists, timestamps; no value),
  `EncryptedSecretRecord` and the private `VaultFile` (ciphertext, nonce and policy only; no key),
  `ProxyRequest` (holds the unsubstituted URL, headers and body, so placeholders, not values),
  `ProxyResponse` (already scrubbed), `ConnectorManifest` and `ConnectorToolDef` (placeholder
  templates), `BridgeConfig` (`verify_secret` and `bot_token_secret` are secret **names**),
  `InboundBridgeEvent`, `BridgeTarget`, `BridgeDispatchPlan`, `RegistryTier`, `TieredRegistryEntry`,
  `IamDecision`, `GuardStage`, `GuardVerdict`, `GateError` (see next paragraph), and the private
  `PreparedSecrets`, which has no `Debug` at all.
* *Hub, CLI, MCP:* no gate type in them holds a secret value or key and derives `Debug` over one.
  `SetSecretPayload` in `crates/hub/src/api.rs` derives only `Deserialize`. `CloudHttp`,
  `HttpCloudSyncAdapter`, `HttpCloudRegistryAdapter`, `HttpCloudRelayAdapter` and `EndpointUrl` in
  `crates/hub/src/cloud_adapters.rs` hold a validated URL and an HTTP agent with no credentials.

`GateError` is the error type that can embed a URL, so each place that builds an `InvalidUrl` was
read: URL parse failures use the caller's unsubstituted `req.url` (`proxy.rs:335`, `proxy.rs:716`
after F-2), and `UpstreamHttp` causes go through `scrub_with_external`. No error built from the
substituted URL remains.

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
**Fix.** The newline is appended to the string and written with one `write_all` (`vault.rs:426`).
**Test:** `vault::tests::concurrent_audit_entries_stay_one_json_object_per_line` (a stress
detector).

### F-15 — Hex parsers accepted a leading `+` (Low)

`u8::from_str_radix("+a", 16)` succeeds, so a 64-character master key written `+1+1…` parsed
as a key of mostly weak bytes, and `percent_decode` read `%+1` as `0x01`.
**Fix.** The master key must be all ASCII hex digits (`vault.rs:675`); `percent_decode` requires
two hex digits (`registry.rs:200`).
**Tests:** `vault::tests::a_master_key_must_be_bare_hex_digits`,
`registry::tests::percent_decoding_ignores_a_signed_pair`.

### F-16 — Ambiguous connector ids and tool names (Low)

`validate` checked the *trimmed* id but the manifest kept the padded one, so ` chat` was stored,
matched and published with its space. A manifest could declare two tools with one name: both
appeared in `tools/list` but only the first was callable. The OpenAPI importer produced such
duplicates whenever two paths sanitized alike (`/a-b`, `/a_b`).
**Fix.** The id is validated as written (`connector.rs:128`), duplicate tool names are an error
(`connector.rs:149`), and the importer numbers colliding names `name_2`, `name_3`
(`unique_tool_name`, `connector.rs:717`).
**Tests:** `connector::tests::ids_and_tool_names_must_be_unambiguous`,
`connector::tests::openapi_operations_that_sanitize_alike_get_distinct_tool_names`.

---

## Open

### O-1 — DNS rebinding (Medium)

`verify_host_ssrf` resolves the name (`proxy.rs:286`) and `ureq` resolves it again when it
connects. A resolver that answers a public address first and `127.0.0.1` second passes the check
and is dialled on the loopback. A resolution **failure** also passes the check (`if let Ok`).
The fix is to connect to the vetted address: a custom `ureq` resolver that filters with
`is_private_or_loopback_ip` at connect time. That changes the transport and TLS name handling, so
it was not done in an audit.

Mitigations today, stated exactly: the check and the HTTP client still resolve the name
separately, so rebinding is **not** closed. The F-3 parser hardening narrows what the check and the
client can read differently, which helps, but it does not make them agree on the address. A DNS
failure in the check still passes. What limits the exposure is that a secret carries
`allowed_hosts`: the destination must match one of them, so the risk is an allow-listed name whose
DNS the attacker controls (or a request that references no secret, where only the SSRF check
applies).

### O-2 — No cross-process lock on the vault (Medium)

`Vault` holds a snapshot and `save` rewrites the whole file (`vault.rs:368`). Two `fl vault set`
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

* **FIXED.** An allow-listed sender could override `channel_routes` with `/model <connector>[:tool]`
  in the message, including naming a connector tool. A connector tool can no longer be chosen from
  chat text, and `/model <connector>` is honoured only for an id the bridge lists in the new
  `allowed_inline_connectors` (default empty, exact ids, `*` rejected at manifest load). `@persona`
  is unchanged.
* **Still open.** `evaluate_inbound_bridge_event` never compares `event.bridge_id` with
  `bridge.id`. The hub route resolves the bridge by the path id and overwrites `event.bridge_id`
  with it, and existing hub tests post a placeholder `bridge_id`, so the check is left for a
  follow-up that also changes those callers.
* **FIXED.** When `verify_secret` named a secret that is not in the vault, an owner caller was
  accepted without any header. An unprovisioned or empty `verify_secret` now refuses every caller,
  owner included, with an error naming the bridge and the secret name (never a value).

### O-6 — Scrubber coverage (Low)

`scrub_secrets` skips secrets shorter than 4 bytes (`proxy.rs:528`) and matches base64 only at
the secret's own alignment, so a secret embedded in a longer base64 stream is not caught. The
fixed token-prefix list (`sk-ant-`, `ghp_`, …) is a heuristic.

### O-7 — Smaller observations (Info)

* `host_matches_pattern` lets `*.example.com` match the apex `example.com` (`proxy.rs:183`). The
  existing test data lists both forms, so it looks intended; say so in the docs.
* Two connectors may declare one tool name; `handle_mcp_jsonrpc_request_with_secret_provider`
  calls the first in id order (`mcp.rs:178`) while `tools/list` shows both.
* The audit log records only `allowed` outcomes, ignores write errors and is never rotated
  (`vault.rs:397`).
* The registry route needs no credentials and lists disabled connectors as `deprecated`.
* The master key file's mode is not checked on read, and `FL_VAULT_MASTER_KEY` in the environment
  takes precedence over the file.
* An OpenAPI import copies `summary` and `description` from an untrusted spec into tool
  descriptions that an agent reads.

---

## Verification

* **Cloud agent's numbers (from its environment, not reproduced here).** `cargo test -p fl-gate
  --ignore-rust-version` (35 unit + 40 integration tests, all pass), `cargo clippy -p fl-gate
  --all-targets --ignore-rust-version -- -D warnings` (clean), and `cargo test -p fl-hub -p fl-cli
  --ignore-rust-version` (all pass). Its toolchain was `rustc 1.97.0` against the workspace's
  `rust-version = "1.98"`, so it needed `--ignore-rust-version`. The `fl-collab` form of these
  commands in the request cannot run: that package does not exist.
* **Measured on this branch** (`rustc 1.98.1`, no `--ignore-rust-version`). Before the review
  follow-up, `cargo test --workspace` gave 2563 passed, 0 failed. After it, `cargo test --workspace
  --no-fail-fast` gives **2571 passed, 0 failed**; `cargo test -p fl-gate` run 10 times in a row
  had 0 failures; `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check` and `cargo run -p fl-cli -- arch check` are clean, and
  `cargo build --workspace --all-targets` prints no deprecation warning.
* **Regression proof (cloud agent).** The new test modules were copied onto the original
  (`fcb61f7`) sources in a scratch worktree. Of the 32 tests that compile there, **19 fail**; the
  13 that pass are the pre-existing ones. The three vault tests that call new helpers
  (`unique_tmp_path`, `write_tmp_0600`, `install_new_master_key`) cannot compile on the original,
  so they are not proven as regressions; they test the new code directly. The two concurrency
  tests (`concurrent_saves_…`, `concurrent_audit_entries_…`) failed on the original, but a stress
  test can pass by luck on unfixed code and should be read as detectors.
* **Mutation checks for the review follow-up (this branch).** Each fix was reverted in turn and
  the new tests failed, then the file was restored and compared with the saved copy: the
  `derive(Debug)` form of `Vault` and `SetSecretArgs` fails the Debug test; a fallback that never
  runs fails three vault tests; one temp-name attempt instead of eight fails the leftover-temp
  test; removing the new address ranges fails the classifier table; `e.to_string()` in place of
  the redacted TOML message fails the malformed-manifest test; an `Err` on a symlinked manifest
  fails the skip-with-warning test.
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

## Landing note

An autonomous cloud agent produced this audit and its fixes, and an independent review followed.
The review confirmed F-1, F-2, F-3, F-5, F-7, F-9 to F-11 and F-13 to F-16 as real and fixed.
F-4, F-6 and F-12 were only partly fixed in the cloud commit, and the follow-up in this commit
completes them (and the F-8 and F-9 side effects). The F-8, F-9 and F-14 tests do not reproduce the
races; they are detectors or direct tests of the new code. This branch lands the result as a fresh
commit, not the cloud commit.

Most dangerous item in practice, by the review's rating: **O-5**. A bridge sender could choose
the connector and the tool inline from chat text, and the owner bypass skipped verification when
`verify_secret` was misspelt or unprovisioned. F-1 stops such a sender naming a secret, but they
could still drive any connector tool with its vault credentials. Both holes are now closed; only
the `bridge_id` compare is open. **O-2** (silent loss of secrets under
concurrent `fl vault` writers) is the most likely to happen, but it is not a security hole.
