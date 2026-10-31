# Wave C security audit — multi-operator identity and cloud adapters

* **Date:** 2026-10-30
* **Base:** `origin/main` at `6e9ff06`
* **Branch:** `felicity/cloud-wave-c-audit` (the cloud agent's branch on origin is
  `cloud/audit-2026-10-30-wave-c`, commit `dcc5dd5`)
* **Scope:** `crates/hub/src/operators.rs`, `crates/hub/src/cloud_adapters.rs`,
  `crates/hub/src/auth.rs`, `ui/src/api/client.ts`, `ui/src/components/PairingModal.tsx`,
  `ui/src/components/SettingsModal.tsx`. The FLEN framing in
  `crates/store/src/ferrostore/sync.rs` was read where the sync adapter feeds it.
* **Method:** a single-reviewer read of the code, then a regression test for every behaviour
  that was changed or pinned. Several of the new tests were not run against the original code
  (see the verification notes), so they are not all proven as regressions.

**Line numbers were re-derived from the files as they stand on this commit** (after the
fixes). A finding marked *Fixed* is closed by the code at the cited line; the original defect
is described in the text. Function names are the stable reference when lines move.

## Summary

| ID | Severity | Area | Status |
|---|---|---|---|
| F-1 | Medium | A single-use pairing code could be redeemed more than once | Fixed |
| F-2 | Medium | A revoked session could be put back in the cache by a racing lookup | Fixed |
| F-3 | Medium | Passkey private key is stored extractable, in plaintext, in `localStorage`, and a passkey login skips TOTP | **Open** |
| F-4 | Medium | `login` / `pair` with the owner token are not throttled | **Open** |
| F-5 | Medium | The SSRF guard let Azure's WireServer (`168.63.129.16`) through | Fixed |
| F-6 | Low | An anonymous login could lock the owner's TOTP enrolment (shared throttle keys) | Fixed (the shared bucket key is `login-invalid:name`; see F-6) |
| F-7 | Low | Passkey registration hub-side origin differs from the origin login asserts | Fixed |
| F-8 | Low | A refused passkey registration left an orphan private key, and made it the "last" passkey | Fixed |
| F-9 | Low | Off-curve P-256 public keys were accepted at registration | Fixed |
| F-10 | Low | Passkey login does not require user verification | **Open** |
| F-11 | Low | Credentials built from a weak fallback RNG path | Fixed (hub); **open** in the CLI |
| F-12 | Low | TOTP accepted a secret of any length, an empty one included | Fixed |
| F-13 | Low | PBKDF2 treats `pw` and `pw\0` alike | Fixed (at the API edge) |
| F-14 | Low | A username shaped like an operator id made `DELETE` ambiguous | Fixed |
| F-15 | Low | UI: fixed fallback TOTP secret, and `ok !== false` read as success | Already fixed on main (earlier audit, #109); tests kept as regression coverage |
| F-16 | Low | UI: a path starting `//` or `/\` becomes a URL for another host | Fixed |
| F-17 | Low | UI: guessable, clock-based credential ids; wrong id sent when falling back to another key | Fixed |
| F-18 | Low | UI: keystore reader walks prototypes and trusts any shape | Fixed |
| F-19 | Low | A registry could inflate the count `publish_servers` returns | Fixed |
| F-20 | Low | Sign counter at `u32::MAX` (or a drop to 0) ends a credential's logins for good | Open, pinned by a test |
| F-21 | Low | Lockout is per name, with no source address: any anonymous caller can lock a name | Open, by design |
| F-22 | Low | `GuardedResolver` starts an unbounded OS thread per lookup | Open |
| F-23 | Info | Several smaller observations | Open |

No finding allows an unauthenticated caller to obtain an owner session. F-1 and F-2 break
the intent of two controls (single-use pairing, revocation) under a race. F-3 and F-4 are the
two weaknesses worth deciding on soon.

---

## Fixed

### F-1 — A single-use pairing code could be redeemed more than once (Medium)

`AuthManager::exchange_pairing_or_token` (`crates/hub/src/auth.rs:512`) read the pairing
entry, checked `used`, wrote `used = true` to the store, then took the cache lock again to
update the cache. Nothing held the cache lock across those steps, and the store write sits in
the middle of the window. Two concurrent requests with one code both saw `used == false`,
and each minted an owner session.

*Impact:* a leaked or shoulder-surfed code could be redeemed by an attacker and the owner at
once, and the owner would not learn of it (neither gets an error). Needs the code; no
bypass of the 128-bit code itself.

*Fix:* check, mark and persist now happen under one cache lock (`auth.rs:525-552`). The
session is minted after the lock is released, so `mint_session` can take it.

*Tests:* `a_pairing_code_redeemed_by_racing_requests_yields_exactly_one_session` (with and
without a store), `a_pairing_code_minted_by_the_cli_redeems_once_through_a_cold_cache`,
`an_expired_or_unknown_pairing_code_mints_nothing`.

### F-2 — A revoked session could be put back in the cache by a racing lookup (Medium)

`verify_session_token` (`auth.rs:579`) on a cache miss read the session from the store with
no lock held, then took the lock to insert it. `revoke_sessions_for_login` (`auth.rs:657`)
swept the cache, then listed and rewrote the store, each under a different lock scope. A
lookup that read the live entry before the revocation wrote, and inserted it after the
revocation's cache sweep, left a live copy in memory. The session then worked until it
expired, even though the store said `revoked`. `revoke_session_token` (logout, `auth.rs:620`)
had the same shape.

*Impact:* deleting or demoting an operator, or changing their password, is supposed to end
their sessions. The window is narrow (a lookup racing the revocation on a cold cache, for
example just after a restart) but the result is a session that survives the one event meant
to kill it.

*Fix:* the store read and the cache insert in `verify_session_token` now happen under the
cache lock, and both revoke paths hold that lock from the first sweep to the last store
write. A poisoned lock is recovered with `into_inner` rather than skipped, so a panic
elsewhere cannot make revocation fail open.

*Test:* `a_revocation_racing_a_cold_cache_lookup_is_never_undone` (60 rounds, each checks the
cache and then a second manager reading the store), plus
`logging_out_through_a_cold_cache_sticks_in_cache_and_store` and
`revocation_matches_the_whole_login_and_ignores_expired_sessions`.

*Trade-off (found in review):* store I/O now runs under the single global auth cache mutex.
Every cache miss does a redb read under it, and a garbage bearer token is a cache miss, so an
anonymous caller can make the hub do a store read under the lock per request. `POST
/auth/pair` is unthrottled. `revoke_sessions_for_login` does a full store scan and write
under the lock. An unauthenticated flood can therefore serialise against authenticated cache
hits. The throughput cost is modest, and this is not an authentication bypass. Possible
mitigations: a negative-lookup guard (a bounded cache of tokens known to be absent) or a
revocation epoch that lets a lookup detect a revocation without holding the lock across I/O.

### F-5 — Azure WireServer was not in the forbidden address set (Medium)

`is_forbidden_ip` (`crates/hub/src/cloud_adapters.rs:514`) covers the private, link-local,
CGNAT, documentation and reserved ranges, which include the AWS, GCP, Alibaba and Oracle
metadata addresses. Azure's host service address `168.63.129.16` is a public-looking
address outside all of them. A hostname resolving to it, or the literal in a URL, passed
both `validate_endpoint_url` and the connect-time guard.

*Fix:* `AZURE_WIRESERVER` (`cloud_adapters.rs:523`) is refused in `v4_forbidden`, which also
covers its IPv4-mapped and NAT64 forms because those are judged by the address inside.

*Test:* `the_azure_wireserver_is_forbidden_but_its_neighbours_are_not`.

Severity is Medium and not higher because the endpoint URLs are set by the owner.

### F-6 — An anonymous login could lock the owner's TOTP enrolment (Low)

The throttle map is shared by three kinds of key: a login name, `passkey:<credential id>`
and `totp-verify:<name>`. `login_password` used the lower-cased name the caller typed as the
key, with no restriction (`login_password`, `operators.rs:1395`). Posting `mode: password` with username
`totp-verify:lmcfarlin` five times charged the internal key and locked
`POST /auth/totp/verify` for the owner (429). The same worked for `passkey:<id>`.

*Fix:* a name containing `:` or a control character is charged to one shared
shared bucket. Real operator names cannot contain either (`valid_username`,
`operators.rs:1013`).

The first version of this fix keyed that bucket with the literal `invalid-username`, which is
itself a valid username (lowercase letters and `-`). An owner could create that operator, and
an anonymous caller could then lock its password login for 60 s with five logins under a name
containing `:`. The independent review found this; the bucket key is now
`INVALID_USERNAME_THROTTLE_KEY = "login-invalid:name"` (`operators.rs:1370`), which holds a
`:` and so cannot be spelled by any valid username. The claim that no real account shares the
bucket is true only for this key.

*Tests:* `an_anonymous_login_cannot_charge_the_internal_totp_verify_or_passkey_keys`,
`control_characters_in_a_login_name_share_the_invalid_bucket`, and
`the_invalid_name_bucket_cannot_be_spelled_by_a_real_operator` (creates an operator named
`invalid-username`, makes eight failed logins as `a:b`, and checks the operator is not
throttled; it fails with the old key, where the operator's login answers 429).

### F-7 — Registered origin and asserted origin could differ (Low)

`registerPasskey` (`ui/src/api/client.ts:1386`) never sent `origin`, so the hub pinned the
credential to `origin_of_request` (`operators.rs:1131`): `https://<Host>`, or
`http://<Host>` for loopback. Login signs `clientData.origin` as `window.location.origin`.
They differ when the hub is behind a proxy that rewrites `Host`, when `baseUrl` is another
origin than the page, and in the Vite dev setup (page on `:5173`, hub on `:8420`). Every
login for such a credential failed, and nothing said why.

*Fix:* one function, `resolvePasskeyOrigin` (`client.ts:420`), supplies the origin for both
calls, and registration now sends it as `origin`. *Behaviour change:* `registerPasskey` used to rely
on the hub's `Host`-derived origin and now always sends one: the page origin, or
`http://localhost:9000` when there is no page and no `baseUrl`.

*Tests:* "pins the same origin at registration that it asserts at login" and "falls back to
baseUrl for the origin only when there is no page origin".

### F-8 — A refused registration left an orphan key, and it became the "last" passkey (Low)

The private JWK was saved to the keystore, and `LAST_PASSKEY_ID` was set to it, before the
hub answered. A 409 or 400 left a key the hub did not know about, and sign-in then offered
that credential (`getLastRegisteredPasskeyId`, `client.ts:379`) instead of one that worked.

*Fix:* the key is generated first, saved only after the hub accepted the registration
(`registerPasskey`, `client.ts:1386`). *Trade-off:* if the hub accepts the registration but the
response is lost (network drop, timeout), the generated key is never saved, so the hub holds a
credential this browser cannot use.

*Test:* "keeps the private key only after the hub accepted the registration".

### F-9 — Off-curve P-256 public keys were accepted (Low)

`normalize_public_key` (`operators.rs:1141`) checked length 65 and a leading `0x04`. A key
that is not a point on P-256 (for example all zeros, or one bit flipped) registered with a
200 and then failed every login with the generic 401.

*Fix:* `is_on_p256_curve` (`operators.rs:1151`) hands the point to ring's ECDH peer-key
parser, which performs the SEC 1 §3.2.2.1 validation (coordinates below `p`, on the curve)
and is the only public ring API that does. No new dependency.

*Test:* `registration_refuses_points_that_are_not_on_the_p256_curve` (off by one bit, zeros,
ones, `y = p`, compressed, hybrid, truncated, extended).

### F-11 — Credentials built from a fallback RNG (Low; hub fixed, CLI open)

`generate_hex_token` (`auth.rs:99`) falls back to two UUIDv7 values when
`SystemRandom::fill` fails. UUIDv7 is mostly a timestamp, and the fallback repeats the first
16 bytes for a 32-byte token. In practice `uuid` needs the same OS source, so this is
unlikely to be reachable, but a credential should fail closed.

*Fix:* `try_generate_hex_token` (`auth.rs:120`) returns an error instead. `mint_session`,
`mint_pairing_code`, `mint_pairing_code_in_store`, the node enrol code and the node token
use it (`auth.rs`, `crates/hub/src/nodes.rs`). It is re-exported from `fl_hub`.

*Not fixed:* `crates/cli/src/cmd/hub.rs:359` and `:619` still call `generate_hex_token` to
generate the owner token. Left alone because that command is outside this audit's scope;
switching it is a one-line change.

*Test:* `credentials_come_from_the_system_rng_and_never_the_fallback`. The failure branch
itself cannot be forced in a test.

### F-12 — TOTP accepted a secret of any length (Low)

`verify_totp` (`operators.rs:489`) took whatever `base32_decode` returned. HMAC accepts an
empty key, so a stored `totp_secret_base32` of `""` with `totp_enabled: true` (a damaged or
edited file) made every code computable by anyone, turning TOTP into a no-op. RFC 4226 §4 R6
requires at least 128 bits.

*Fix:* `MIN_TOTP_SECRET_BYTES = 16` (`operators.rs:93`); a shorter secret never verifies.
Generated secrets are 20 bytes.

*Tests:* `totp_refuses_a_secret_below_128_bits`,
`a_tampered_empty_totp_secret_does_not_let_a_password_alone_in`.

### F-13 — PBKDF2 treats `pw` and `pw\0` alike (Low)

HMAC zero-pads a key shorter than its block (64 bytes), so PBKDF2-HMAC-SHA256 returns the
same output for a password and for that password with trailing NULs. A login with
`<password>\0` verified. Not exploitable in itself (the attacker needs the password), but it
means the stored hash does not distinguish two inputs.

*Fix:* a password containing NUL is refused when set (`valid_password`, `operators.rs:1029`)
and answered with the plain 401 at login before any hashing.

*Trade-off:* a stored password that contains NUL, or ends in NUL, is now permanently refused
at login. A legacy NUL password is therefore locked out until an owner resets it. The refusal
returns before the throttle charge, the verification permit and the 429 path, and leaks
nothing (the same plain 401).

*Tests:* `a_password_with_a_nul_is_refused_when_set_and_when_used`;
`password_hashing_pins_the_salt_and_cost_it_stores` documents the HMAC property.

### F-14 — A username could be shaped like an operator id (Low)

`find_id_or_username` (`operators.rs:249`) matches ids before names. `valid_username` allows
`op-` followed by 16 hex digits, the shape of an id, so `DELETE /operators/<that>` could hit
a different operator than the one named. Only an owner can create operators, so there is no
privilege gain, only an ambiguity.

*Fix:* `looks_like_operator_id` (`operators.rs:1007`) is refused as a new username.

*Test:* `a_username_shaped_like_an_operator_id_is_refused`.

### F-15 — UI: invented TOTP secret, and any 200 counted as verified (Low)

`SettingsModal.tsx` (`handleEnrollTotp`) fell back to the fixed secret `JBSWY3DPEHPK3PXP` if
the response was empty, and `handleVerifyTotp` treated anything but `ok: false` as a
successful verification. A hub that
answered 200 with `{}` therefore showed a made-up secret, or a "verified" badge. The
fallback is a publicly known string.

*Status:* already fixed on main by the earlier audit (#109). This commit does not change
`SettingsModal.tsx` behaviour, and the two tests below pass without it; they are kept as
regression coverage. On main the secret is shown only if the hub returned `secret_base32`
and `otpauth_uri` strings, and verification counts only on `ok === true`; both show an error
otherwise.

*Tests:* "shows no secret when the hub returns none, and never a hard-coded one" and "does
not mark TOTP verified unless the hub said ok:true".

### F-16 — UI: a request path could become a URL for another host (Low)

`HubApiClient.request` (`client.ts:508`) built `${baseUrl}${path}`. With an empty `baseUrl`
a path of `//evil.test/x` is a protocol-relative URL, and the request carries
`Authorization: Bearer <session token>` (`client.ts:521`). No current caller passes an
unencoded user value, so this is hardening.

*Fix:* leading `/` and `\` characters collapse to one `/`.

*Test:* "never lets a path become a URL for another host".

### F-17 — UI: guessable credential ids, wrong id on fallback (Low)

`handleRegisterPasskey` used `passkey-${Date.now()}`: two clicks in one millisecond collide
(409), and the id can be guessed within a narrow range. A credential id selects the account
whose throttle bucket a failed passkey login charges, so a guessable id lets an anonymous
caller lock a known operator's logins. Separately, `loginWithPasskey` signed with the newest
stored key when the requested id was unknown, but still sent the requested id.

*Fix:* `generatePasskeyCredentialId` (`client.ts:402`, 24 random bytes, 32 base64url chars).
It throws when `crypto.getRandomValues` is missing instead of falling back to a clock-based
id; `SettingsModal` shows the error and registers nothing. The request now carries
`stored.credential_id`, the credential whose key signed.

*Tests:* "is 32 unguessable characters the hub accepts, and never repeats", "throws instead
of falling back to a guessable id when no secure random generator exists", "shows an error,
and registers nothing, when no secure random generator exists", "registers passkeys under
random credential ids", "names the credential whose key signed when the requested one is
unknown".

### F-18 — UI: keystore reader trusted any shape (Low)

`readStoredPasskeyMap` (`client.ts:313`) merged parsed `localStorage` JSON into a plain
object with `Object.assign`, so a `__proto__` key (an own property after `JSON.parse`)
replaced the prototype, and a lookup by credential id could walk into it. Entries of any
shape were accepted.

*Fix:* a null-prototype map, own entries only, `__proto__` skipped, and an entry must carry a
`private_jwk` object. An entry that lacks a string `credential_id` takes the key it is filed
under (otherwise the login would send no `credential_id` and the sign counter would be saved
under `"undefined"`); the test "signs with a keystore entry that has a key but no
credential_id" covers it. Anyone who can write `localStorage` already holds the key (F-3), so
this limits damage from malformed data, not from an attacker.

*Test:* "survives a corrupt, hostile or unavailable keystore without throwing or polluting
prototypes" (ten bad shapes, and a `localStorage` that throws).

### F-19 — A registry could inflate the published count (Low)

`publish_servers` (`cloud_adapters.rs:1277`) returned the server's `published` number
unchecked. It is now capped at the number of entries sent.

*Test:* `a_registry_cannot_inflate_the_published_count`.

---

## Open

### F-3 — Passkey private key in `localStorage`; passkey login skips TOTP (Medium)

`registerPasskey` generates the key with `extractable: true` (`client.ts:1400`) and stores
the private JWK as plaintext JSON under `fl_hub_passkey_keystore`. `loginWithPasskey` imports
it and signs. This is a software emulation of a passkey, not a WebAuthn authenticator:
anything that can read the page's `localStorage` (an XSS, a browser extension, a shared
profile) holds a credential that logs in as an owner.

The hub compounds this. `login_passkey` (`operators.rs:1508`) never consults
`totp_enabled`, so an operator who turned on TOTP is still reachable by passkey alone, and
`check_assertion` accepts user-presence only (see F-10). TOTP therefore protects the password
path and nothing else.

*Options:* (a) use `navigator.credentials.create/get` so the key lives in the
authenticator; (b) keep software keys, but make them non-extractable `CryptoKey`s in
IndexedDB; (c) at minimum, require TOTP (or user verification) at passkey login when the
operator has TOTP enabled. Each changes login behaviour, so this audit left it for a
decision.

### F-4 — Owner-token exchange is not throttled (Medium if the token is weak)

`login_token` (`operators.rs:1342`) is deliberately unthrottled, "the recovery path when an
account is locked out". `handle_auth_pair` (`crates/hub/src/api.rs:1020`) has no throttle
either, and also accepts the owner token. Both compare in constant time. A generated token
(`generate_hex_token(32)`, 256 bits) cannot be guessed online. `resolve_owner_token` also
accepts a configured one, and nothing enforces its strength.

*Suggestion:* enforce a minimum length when a token is configured, or throttle by source
address (the hub has none today) with a ceiling high enough to keep recovery usable.

### F-10 — Passkey login does not require user verification (Low)

`check_assertion` requires the UP flag (`operators.rs:1217`, `auth[32] & 0x01`) but not UV
(`0x04`). With a hardware key that is a possession-only login. Requiring UV would refuse
authenticators that cannot verify the user and the software keys in F-3, which always send
`0x01`, so it needs F-3 decided first.

### F-20 — Counter edge cases end a credential (Low)

The strict-increase rule (`operators.rs:1226` and the repeat under the lock) has no wrap
rule: a credential that has seen `u32::MAX`, or whose authenticator reports 0 after a
non-zero value, never logs in again and has to be re-registered. This is stricter than
WebAuthn asks, and the doc comment says so. `a_counter_that_reaches_u32_max_ends_the_credentials_logins`
pins the behaviour so a change is deliberate.

### F-21 — Lockout is per name and has no source (Low, by design)

Five charged attempts lock a name for 60 seconds, and the count survives the lockout
(`Throttle::charge`, `operators.rs:611`). An anonymous caller can keep a named account from
logging in by password (one request a minute holds it). The credential id of a registered
passkey selects the same bucket, which is public to any owner session (F-17). Recovery is the
unthrottled token login (F-4). Also, 1024 tracked names are kept and the oldest unlocked
ones are evicted first, so a flood of fresh names can reset an unlocked counter; it cannot
free a locked one, and the hub-wide 100 per minute bounds the rate.

### F-22 — `GuardedResolver` spawns a thread per lookup (Low)

`lookup_within` (`cloud_adapters.rs:768`, spawn at `:777`) runs `getaddrinfo` on a new thread
so a stuck resolver cannot outlast the timeout, but the thread is never reclaimed until the
resolver returns. A name whose DNS server stalls makes each call leave a thread behind.
Nothing builds an adapter from the saved configuration yet, so no caller reaches this; fix
before one does (a single resolver thread with a bounded queue).

### F-23 — Smaller observations (Info)

* `requires_totp` (`operators.rs:1442`) is answered only after the password verified, so it
  tells a password guesser they guessed right. By design; the TOTP step is still charged.
* `dummy_password_check` (`operators.rs:411`) runs the current iteration count; a stored hash
  made at a higher count takes longer, so timing can reveal that an account's cost differs.
  Not a secret.
* `cloud_adapters::load` (`cloud_adapters.rs:242`) follows symlinks (`fs::metadata`) where the
  operators file refuses them (`symlink_metadata`). The result is still validated, and a
  failure is a fixed message, so nothing is disclosed.
* `fetch_document` maps any HTTP 404 to `Ok(None)` (`cloud_adapters.rs:1181`), so a wrong base
  path reads as "no such document". `prewarm` accepts unordered and repeated `seq` on purpose
  (a test pins it as "a snapshot, not a cursor"). `forward_jsonrpc` returns the relay's JSON
  without checking its `id` against the request.
* The auth session cache has no eviction; sessions accumulate until restart.
* `getLastRegisteredPasskeyId` returns the fixed `passkey-default-0001` when nothing is stored
  (`client.ts:393`); the hub treats it as an unknown credential.
* `pair({ token })` stores the raw owner token when the response carries no session token
  (`client.ts:557`).
* `handleAddOperator` shows a locally invented operator if the response has none
  (`SettingsModal.tsx:172`).

---

## Reviewed and found sound

Checked against the code and, where noted, against tests that already existed:

* **PBKDF2:** 600 000 iterations of HMAC-SHA256 for new hashes, a 16-byte random salt per
  hash, verification by `ring::pbkdf2::verify` (constant time) with the account's own salt and
  count, a floor at the production cost and a ceiling of 10 000 000 on a stored count, and the
  test-only constant gated on `cfg!(test)` with a compile-time assertion on the production one
  (`operators.rs:73-84`).
* **Unknown user, no password, wrong password:** one PBKDF2 each, the same 401, the same
  throttle accounting (`login_password`, `operators.rs:1423-1436`).
* **TOTP:** RFC 6238 and 4226 vectors pass; one step of skew either side; a step must be
  strictly newer than the last accepted, updated under the file lock (so concurrent logins
  cannot reuse a code); candidates compared in constant time; the pending secret has its own
  replay state; replacing a live secret revokes sessions.
* **Origin pinning:** the origin and the relying-party id come from the credential, never from
  the request's `Host`; `crossOrigin: true` and a type other than `webauthn.get` are refused;
  the origin must match exactly including port; `http` only for loopback hosts, and lookalike
  names such as `localhost.evil.com` and `127.0.0.1.nip.io` are refused (new test
  `origin_parsing_rejects_lookalike_loopback_hosts_and_smuggled_parts`).
* **Assertion verification:** rpIdHash compared in constant time, UP required, the signature
  over `authData || SHA-256(clientDataJSON)` verified by ring, which parses DER strictly. New
  test `a_der_signature_must_be_strict_der` sends eleven malformed encodings (trailing byte,
  truncation, long-form and indefinite length, non-minimal and negative integers, zero `r` and
  `s`, raw `r||s`, swapped values, wrong tag): all get the plain 401 and none spends the counter.
* **Challenges:** 32 random bytes, 5-minute life, single use whatever happens next, capped at
  256 live and 30 issued a minute. `one_challenge_can_log_in_only_one_of_many_racing_assertions`
  and `two_assertions_with_the_same_counter_cannot_both_log_in` race eight and six threads.
* **In-flight cap:** four verification permits, taken with a CAS loop, released by `Drop` on
  every path including a panic; a request that finds them taken is answered 429 and charged
  nothing. The cap also means a flood can briefly make legitimate logins answer 429.
* **Persistence:** every read-modify-write under one process-wide lock; a 0600 temp file in the
  same directory, `fsync`, then `rename` (or `hard_link` for the exclusive seed); a symlink,
  an oversize file or an inconsistent file is refused and never re-seeded; the writer refuses to
  write what the reader would refuse. New test
  `the_operator_file_survives_concurrent_writers_without_losing_an_update` runs eight writers
  of five operators each.
* **Endpoint URLs:** ASCII only, no userinfo, query, fragment or backslash; canonical dotted
  quad or bracketed IPv6 only (`127.1`, `2130706433`, `0x7f000001`, `0177.0.0.1` all refused);
  IPv4-mapped, compatible, NAT64, 6to4 and Teredo forms judged by the address inside;
  `http` only for exactly `127.0.0.1`, `localhost`, `[::1]`. New tests
  `ipv6_forms_that_carry_a_private_ipv4_address_are_refused` and
  `malformed_authorities_and_numeric_hosts_are_refused` add 15 and 38 vectors.
* **Connect-time guard:** the resolver is ureq's own `Resolver`, so the address that is checked
  is the address that is connected to (no second lookup to rebind), every resolved address
  must pass, a loopback endpoint admits loopback only, the lookup has a timeout, and a request
  for any other host is refused. Redirects are off (`max_redirects(0)`), the proxy is off, and
  TLS uses the URL host.
* **Body caps:** a declared oversize answer is refused unread; a streamed or gzip answer is cut
  at the cap on the decoded bytes; a request over the cap is never sent; total, connect and
  read timeouts. Existing tests cover each.
* **FLEN framing** (`crates/store/src/ferrostore/sync.rs:491`): magic, version, kind and flag
  bytes checked; a length prefix never allocates beyond the bytes that remain; trailing bytes
  are an error; a batch's envelopes must share its `seq` and form a dense `op_index` range.
  `pull_since` demands `seq` strictly above the cursor and strictly ascending.
* **Path encoding:** every non-unreserved byte of a collection, id or connector id is
  percent-encoded (including `/`, `%`, `?`, `#`, `\`, CR/LF and non-ASCII), and `.`, `..` and
  the empty segment are refused before a request is built. New test
  `path_segments_cannot_escape_their_route`.
* **UI `p1363ToDer`:** correct for all 64-byte inputs: minimal integers, a `0x00` for a high
  bit, a short-form length (the longest result is 72 bytes). The new tests check it against a
  strict DER reader on 120 real WebCrypto signatures, and check that an input of any other
  length is returned unchanged (which the hub then refuses).

## Verification notes

* **Toolchain.** The workspace requires rustc 1.98 (`Cargo.toml`); the container had 1.97. The
  results below were produced with `rustup toolchain install 1.98` and `cargo +1.98`. No
  repository file pins the toolchain, and none was changed.
* **Commands run on this commit** (all pass):
  * `cargo fmt --all -- --check`
  * `cargo clippy --workspace --all-targets -- -D warnings`
  * `cargo test --workspace --no-fail-fast` - 2421 passed, 0 failed (measured)
  * `cd ui && npx tsc --noEmit` - clean; `npx vitest run` - 21 files, 183 tests passed (measured)
  The cloud agent's report claimed 2229 Rust tests, 171 UI tests and 20 files; those were
  measured on its own base and are superseded by the figures above.
* **New tests:** 7 in `auth.rs`, 17 in `operators.rs`, 5 in `cloud_adapters.rs`, and 22 in
  `ui/src/__tests__/WaveCAudit.test.tsx`.
* **Regression check.** The cloud agent reported that, with the original `auth.rs` production
  code swapped in, `a_pairing_code_redeemed_by_racing_requests_yields_exactly_one_session`,
  `a_pairing_code_minted_by_the_cli_redeems_once_through_a_cold_cache` and
  `a_revocation_racing_a_cold_cache_lookup_is_never_undone` fail. That was not reproduced
  during landing. Several of the new regression tests, the probabilistic races for F-1 and
  F-2 among them, were not run against the original code, so they are unproven as
  regressions. Race tests of this kind can also pass by luck on unfixed code. Three tests
  were shown to fail without their fix during landing: the throttle-key test
  (`the_invalid_name_bucket_cannot_be_spelled_by_a_real_operator`, with the old key) and the
  two client tests for a missing `credential_id` and a missing secure random generator (with
  the original `client.ts`).
* **Not done.** Production-cost PBKDF2 (600 000 iterations) was not timed; the test build uses
  1 000. No dynamic test of the TLS path or a real DNS rebind was run (the existing tests use a
  fake lookup and a local server). `PairingModal.tsx` was read, and has no storage, origin or
  secret handling of its own beyond what `client.ts` does; it needed no change.
* **Process.** `WORKFLOW.md` asks that review and remediation be run by separate subagents.
  The cloud agent wrote this audit and its fixes in one session; the landing commit was
  reviewed independently (see the landing note).

## Landing note

This commit was produced by an autonomous cloud agent (`claude --cloud`, branch
`cloud/audit-2026-10-30-wave-c`, commit `dcc5dd5`) and independently reviewed before landing.
The review confirmed that all the defects it claimed as fixed are real: F-1, F-2, F-5 to F-9,
F-11 to F-14 and F-16 to F-19. F-6 was only partly fixed (the shared bucket key was itself a
valid username) until this commit's key change. F-15 was already on main from the earlier
audit (#109). The review's other accepted findings are fixed here: `registerPasskey` and the
keystore reader tolerate an entry without `credential_id`, and `generatePasskeyCredentialId`
no longer falls back to a clock-based id. The work landed as a fresh commit on top of
`origin/main` `6e9ff06`; two `SettingsModal.tsx` conflict hunks were resolved in favour of the
version already on main, and the `WaveCAudit.test.tsx` expectations follow main's wording.
