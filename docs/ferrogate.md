# FerroGate: the audit log, vault references and the RS256 signer

This page covers what `crates/gate` records when it refuses or allows a use of a Vault secret, the
`vault://` reference syntax, and `Vault::sign_app_jwt`. It states the rules as they are; the last
section says where the protection ends.

## The audit log

`gate-audit.jsonl` sits beside `vault.enc`, one JSON object per line, mode `0600`:

```json
{"timestamp":"…","caller":"ferris","secrets":["UPSTREAM_KEY"],"method":"GET","host":"127.0.0.1","outcome":"allowed"}
```

An entry holds the caller, the secret NAMES, the method, the destination host and the outcome. It
never holds a secret value, a header, a body, or a URL path or query.

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
3. `host` matches `allowed_hosts` (an empty list denies);
4. the caller is in `allowed_agents`, when that list is not empty (`*` admits any caller).

Every outcome is audited under the secret's name with method `SIGN_RS256_JWT`: `allowed`, or
`denied:` plus `host`, `agent`, `expired`, `no_such_secret`, `issuer`, `clock`, `vault_error`, `bad_key` or
`policy`. If the `allowed` entry cannot be written, the call returns an error and no JWT.

The PEM is held in zeroizing buffers and wiped as soon as the signature is made. The key pair that
`ring` builds from it is `ring`'s own and is dropped straight after signing. The ordinary decrypt
paths (`decrypt_raw`, the proxy's substitution, scrubbing) still return plain `String`s; only the
signer and `verify_secret_constant_time` use the zeroizing path.

## What this does not protect against

* `vault.key` and `vault.enc` live under one Unix user. Any process of that user can read both and
  decrypt every secret. On the local CLI path `allowed_agents` is therefore advisory: it stops a
  mistake and leaves a record, and does not stop a process that skips the library. It is
  authoritative only where the hub holds the key and authenticates the caller.
* `sign_app_jwt` runs in the calling process: that process decrypts the key. There is no enclave,
  no HSM and no separate signing process.
* The SSRF guard resolves a host name once to judge it and the HTTP client resolves it again to
  connect. A name that answers differently the second time (DNS rebinding) is not caught.
* The audit log is a plain file the same user can edit.
