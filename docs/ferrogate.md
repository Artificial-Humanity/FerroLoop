# FerroGate: the audit log, vault references and the RS256 signer

This page covers what `crates/gate` records when it refuses or allows a use of a Vault secret, the
`vault://` reference syntax, `Vault::sign_app_jwt`, and `[github] private_key_ref`, the config key
that makes fl sign its GitHub App JWT through the Vault. It states the rules as they are; the last
section says where the protection ends.

## The audit log

`gate-audit.jsonl` sits beside `vault.enc`, one JSON object per line, mode `0600`:

```json
{"timestamp":"…","caller":"ferris","secrets":["UPSTREAM_KEY"],"method":"GET","host":"127.0.0.1","outcome":"allowed"}
```

An entry holds the caller, the secret NAMES, the method, the destination host and the outcome. It
never holds a secret value, a header, a body, or a URL path or query.

The caller is written trimmed and lower-cased (the form policy matching uses), with every
character outside `A-Za-z0-9._@-` replaced by `?`, at most 255 characters; a missing or blank
caller is `anonymous`. Caller text therefore cannot add a line, a field or a look-alike entry.
The file is opened with `O_NOFOLLOW` and refused when it is a symlink. It has no rotation and no
size cap: each refusal appends at most about a kilobyte, and any caller that can reach the Vault
can cause refusals. Rotating or capping it is a known follow-up.

Every refusal on the credential proxy path (`prepare_proxied_request`,
`execute_proxied_request`) writes an entry before the error is returned. A refusal happens before
any connection is made. `secrets` lists the valid secret names the request referenced; a malformed
name is not written down, because it may be a pasted value. `host` is empty when the URL did not
parse. A method that is not 1 to 16 ASCII capitals is written as `INVALID`.

| Refusal | `outcome` |
| --- | --- |
| Host not in `allowed_hosts` | `denied:host` |
| Method not in `allowed_methods`, or an empty method | `denied:method` |
| Caller not in `allowed_agents` | `denied:agent` |
| `http://` to a secret without `allow_private_network` | `denied:https_required` |
| Lease expired | `denied:expired` |
| Policy refused for another reason (for example from a provider) | `denied:policy` |
| SSRF guard (private, loopback or link-local address) | `denied:ssrf` |
| Placeholder in the URL path, or unclosed | `denied:placeholder` |
| Placeholder name that is not `A-Z0-9_` (1 to 64) | `denied:invalid_secret_name` |
| Secret does not exist | `denied:no_such_secret` |
| URL that does not parse, or a scheme other than http(s) | `denied:invalid_url` |
| Vault tampered, corrupt, or a crypto failure | `denied:vault_error` |
| External secret provider failed | `denied:provider_error` |
| Method the client cannot send (after `allowed`) | `denied:unsupported_method` |

Two further entries follow an `allowed` one: `error:upstream` (the call failed) and
`redirect_not_followed` (the upstream answered 3xx; the proxy never follows a redirect, because
the credential would go with it, and returns the 3xx as it is).

`allowed` is written once the policy passed and only when the request referenced a secret. It is
unchanged.

The audit write is best effort for the proxy path: a failed write never turns a denial into an
allow, and on the allowed proxy path a failed write is ignored and the request goes on. The signer
below is stricter.

## The outbound client

The proxy ignores `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY`: the request goes
direct to the policy-checked host, with the injected secret in it. It follows no redirect.

## Vault references

`vault://NAME[#field]` names a secret and the field of it a caller wants:

```text
vault-ref = "vault://" NAME [ "#" FIELD ]
NAME      = 1*64 ( "A".."Z" / "0".."9" / "_" )
FIELD     = "pem"
```

`parse_vault_ref` accepts that and nothing else. A lowercase name, a space or control or non-ASCII
character anywhere, `/`, `?`, a second `#`, another scheme, another field, an empty name and a name
over 64 characters are all refused. `Display` gives back the same string. A `VaultRefError` names
the kind of mistake and echoes none of the input.

A reference holds no value and no policy. Whether a caller may use the secret is the Vault's
decision at use time.

## `Vault::sign_app_jwt`

`sign_app_jwt(&VaultRef, issuer, caller, host, now)` signs a compact RS256 JWT with the RSA
private key stored as the secret's value and returns only the JWT.

The JWT is built as `crates/github/src/creds.rs` builds the GitHub App JWT: header
`{"alg":"RS256","typ":"JWT"}`, claims `iss` (the issuer, 1 to 64 characters of `A-Za-z0-9._-`),
`iat` = now minus 60 seconds, `exp` = now plus 540 seconds, signed `RSA_PKCS1_SHA256`. The key is
PKCS#1 (`BEGIN RSA PRIVATE KEY`) or PKCS#8 (`BEGIN PRIVATE KEY`) PEM; anything else is refused with
an error that holds no key bytes.

The policy runs before the key is decrypted, from the secret's metadata:

1. the secret exists;
2. its lease has not expired;
3. `host` is a hostname (non-empty dot-separated labels of `A-Za-z0-9-_`; no leading or trailing
   dot, no `..`) and matches `allowed_hosts` (an empty list denies);
4. the caller is in `allowed_agents`, when that list is not empty (`*` admits any caller).

Every outcome is audited under the secret's name with method `SIGN_RS256_JWT`: `allowed`, or
`denied:` plus `host`, `agent`, `expired`, `no_such_secret`, `issuer`, `clock`, `vault_error`, `bad_key` or
`policy`. If the `allowed` entry cannot be written, the call returns an error and no JWT.

The PEM is held in zeroizing buffers and wiped as soon as the signature is made. Every buffer on
the PEM-to-DER path (the decrypted bytes, the base64 body, the DER) is allocated once at its final
size and never grown, because a grown buffer frees its old block un-wiped; a test checks the
capacities. The PEM is read in place and never cloned or formatted. The key pair that `ring`
builds from it is `ring`'s own memory, dropped straight after signing, and is not wiped by this
crate.

`sign_app_jwt` reads the Vault handle's in-memory snapshot, taken by `Vault::open`. A long-lived
handle does not see a policy change or a rotated key written to disk afterwards: a consumer opens
the Vault again for each call. The ordinary decrypt
paths (`decrypt_raw`, the proxy's substitution, scrubbing) still return plain `String`s; only the
signer and `verify_secret_constant_time` use the zeroizing path.

## `[github] private_key_ref`

`[github]` names the App's private key in one of two ways:

```toml
[github]
app_id = 123456
private_key_ref = "vault://GITHUB_APP_KEY"   # or: private_key = "/abs/path/app.pem"
```

* Exactly one of `private_key` and `private_key_ref` is set. Both set is a config error that names
  both keys; neither is a config error. `app_id` is required with either.
* The reference is `vault://NAME` or `vault://NAME#pem` (the grammar above); anything else, and an
  empty value, is a config error that names the key and the kind of mistake and echoes nothing of
  the value.
* The config load checks the shape only. It opens no Vault, reads no master key and creates
  nothing. The Vault is used when a command first needs a JWT.
* `$FL_GITHUB_PRIVATE_KEY_PATH` is unchanged: with `$FL_GITHUB_APP_ID` it replaces a `[github]`
  that sets `private_key`. With a `private_key_ref` in `[github]` the variable is not a second
  source but an ambiguity, and the command that builds the App credential (`fl github` on a
  `credential = "app"` binding, `fl github pr` review and merge, `fl hub serve`) refuses with an
  error that names both. Unset the variable or remove the ref.

At use time each JWT is made by `BrokeredSigner` (`crates/cli/src/gate_signer.rs`), which opens the Vault again
for every signing, read-only, and calls `Vault::sign_app_jwt(ref, app_id, caller, "api.github.com",
now)`. fl holds no PEM: the JWT comes back, the key does not. The installation token that GitHub
returns is a different credential and is cached for fifty minutes as before. A JWT is never kept.

* The caller is `$AGENT_PERSONA` when it is set and not blank, else `fl`. The Hub signs as
  `fl-hub`. A secret with a non-empty `allowed_agents` must list the caller (or `*`). The Hub
  signs as `fl-hub` for every Hub-authorized request that reaches the merge and review endpoints,
  so listing `fl-hub` in `allowed_agents` grants signing to all of them; and it is advisory
  locally, because a local process can set `AGENT_PERSONA=fl-hub`.
* The host is the host of the API URL the command sends the JWT to: `api.github.com` by default,
  else the host of `$FL_GITHUB_API_URL` (or the Hub's API URL), lower-case, with no port. So a
  GitHub Enterprise host must be in the secret's `allowed_hosts`, and `allowed_hosts` constrains
  where fl sends the JWT: pointing `$FL_GITHUB_API_URL` at a host the secret does not list is
  denied (`denied:host`) before the key is decrypted, and nothing is sent. A host the Vault cannot
  read as a host name (a bracketed IPv6 address) is denied too. The check is made by fl's own
  code, from the URL it then uses; a process that skips that code is not constrained.
* There is no fallback. If the Vault file or its master key is missing, the secret does not
  exist, or the policy denies the call, the command fails with a message that names the secret
  and the reason (for example "the App private key `vault://GITHUB_APP_KEY` could not be used to
  sign: FerroGate policy denied secret `GITHUB_APP_KEY`: the destination host is not in
  allowed_hosts"). It does not read `private_key`, `$FL_GITHUB_PRIVATE_KEY_PATH` or any file. A
  policy denial is audited as above; nothing is sent to GitHub.
* A read never creates a Vault or a master key: the master key is loaded with
  `load_existing_master_key`, which fails when there is none and cannot generate one.

`fl-github` does not depend on `fl-gate`. It defines the `JwtSigner` trait; `FileKeySigner` (the
PEM file, read again at every signing) is the `private_key` implementation, and `fl-cli` supplies
the Vault one.

### Moving to a Vault-held key

1. Store the PEM as a secret open to GitHub and to the agents that run fl, for example
   `fl vault set GITHUB_APP_KEY --stdin --host api.github.com --agent ferris --agent fl
   < app.pem` (`--host` is the API host fl will send the JWT to; for GitHub Enterprise, that
   host). List every caller that needs it: `fl` is the caller when `$AGENT_PERSONA` is
   unset, `fl-hub` is the Hub.
2. In `[github]` replace `private_key = "…"` with `private_key_ref = "vault://GITHUB_APP_KEY"`.
3. Check it: `fl github whoami` prints `<slug>[bot]`, and `gate-audit.jsonl` has an `allowed`
   `SIGN_RS256_JWT` entry for `GITHUB_APP_KEY`.
4. Only then retire the PEM file, and rotate the App's key in GitHub if the old file may have
   been copied. Rotating before the consumer has passed leaves nothing that signs.

The drill that proves the plaintext path is denied (no process outside the Vault can read the
key) is an Owner-window action. This change does not perform it, and nothing here claims it.

## What this does not protect against

* The `host` that `sign_app_jwt` takes is a label the caller supplies. No connection is made, so
  the `allowed_hosts` check proves nothing about where the JWT goes: any caller that
  `allowed_agents` admits can pass any host in `allowed_hosts` and send the JWT elsewhere. An
  EMPTY `allowed_agents` admits every caller that can reach the Vault. The gates that hold are
  `allowed_agents` and who can call at all (the hub authenticates its callers; the local CLI path
  does not). `allowed_hosts` catches an honest caller's mistake and puts the claimed destination
  in the audit entry.
* `vault.key` and `vault.enc` live under one Unix user. Any process of that user can read both and
  decrypt every secret. On the local CLI path `allowed_agents` is therefore advisory: it stops a
  mistake and leaves a record, and does not stop a process that skips the library. It is
  authoritative only where the hub holds the key and authenticates the caller.
* `sign_app_jwt` runs in the calling process: that process decrypts the key. There is no enclave,
  no HSM and no separate signing process.
* The SSRF guard resolves a host name once to judge it and the HTTP client resolves it again to
  connect. A name that answers differently the second time (DNS rebinding) is not caught.
* The audit log is a plain file the same user can edit.
* `private_key_ref` moves the App's key out of a readable PEM file and into `vault.enc`. It does
  not move it out of reach of the user that runs fl: `vault.key` lives under the same Unix user,
  `allowed_agents` is advisory on the local CLI path, and fl-gate decrypts the key in the fl
  process. What it adds is a policy check and an audit line at each use, and a key that is
  encrypted at rest.
