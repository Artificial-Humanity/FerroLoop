//! A policy-checked RS256 signer: [`Vault::sign_app_jwt`] signs a GitHub-App-style JWT with an
//! RSA private key held in the Vault and returns the JWT, never the key.
//!
//! The policy runs BEFORE the key is decrypted: the secret must exist, its lease must be alive,
//! the caller must be in `allowed_agents` (when that list is not empty), and the destination
//! `host` must match `allowed_hosts` (an empty list denies). Every outcome, the allowed one and
//! each denial, is appended to `gate-audit.jsonl` with the secret's NAME only. If the audit
//! entry for an allowed signature cannot be written, no JWT is returned.
//!
//! ⚠ The `host` argument is a LABEL the caller supplies. `sign_app_jwt` makes no connection, so the
//! `allowed_hosts` check proves nothing about where the JWT is sent: any caller that
//! `allowed_agents` admits can pass any host that `allowed_hosts` lists, and then send the JWT
//! wherever it likes. An EMPTY `allowed_agents` admits every caller that can reach the vault. The
//! gates that hold are `allowed_agents` and who can call at all (the hub authenticates its
//! callers; the local CLI path does not). `allowed_hosts` narrows an honest caller's mistakes,
//! and makes the audit entry say where the caller claimed the JWT was for.
//!
//! ⚠ The Vault handle is a snapshot taken by `Vault::open`. A long-lived handle does not see a
//! policy change (or a rotated key) written to disk afterwards: reopen the Vault for each call.
//!
//! ⚠ Custody: this is in-process signing. The calling process decrypts the key with the master
//! key it already holds (`vault.key`, same Unix user), so `allowed_agents` is advisory on the
//! local CLI path and authoritative only where the hub holds the key and authenticates the
//! caller. Nothing here is an enclave or an HSM.
//!
//! The PEM plaintext lives in [`Zeroizing`] buffers and is wiped as soon as the signature is
//! made. Each buffer on the PEM-to-DER path is allocated once at its final size and never grown
//! (a grown `String` or `Vec` frees its old block un-wiped): the decrypted bytes, the base64
//! body, and the DER. The PEM is read in place as `&str`; nothing clones or formats it. `ring`
//! copies the parsed key into its own key pair, which this crate does not control; that key pair
//! is dropped immediately after signing. The JWT input, the signature and the claims hold no
//! key material.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use zeroize::Zeroizing;

use crate::GateError;
use crate::proxy::host_matches_pattern;
use crate::vault::{LEASE_EXPIRED_REASON, Vault, VaultSecretMetadata, check_lease};
use crate::vaultref::{VaultField, VaultRef};

/// The `method` an audit entry carries for a signature.
pub const SIGN_AUDIT_METHOD: &str = "SIGN_RS256_JWT";

/// Back-date `iat` by this much for clock drift. Same value as `crates/github/src/creds.rs`.
const IAT_BACKDATE_SECS: u64 = 60;
/// `exp` is this far after `now`: nine minutes (GitHub allows ten). Same as `creds.rs`.
const EXP_AFTER_SECS: u64 = 540;
/// A PEM private key is a few KiB; refuse anything absurd before parsing it.
const MAX_PEM_BYTES: usize = 16 * 1024;
const MAX_ISSUER_LEN: usize = 64;
const MAX_AUDIT_FIELD_LEN: usize = 255;

impl Vault {
    /// Sign a compact RS256 JWT with the RSA key stored under `secret`, for `issuer` (the App
    /// id), on behalf of `caller`, for use against `host` (for example `api.github.com`).
    ///
    /// The JWT is built exactly as `crates/github/src/creds.rs` builds the App JWT (that crate is
    /// not a dependency of this one, so the construction is copied): header
    /// `{"alg":"RS256","typ":"JWT"}`, claims `iss` = `issuer`, `iat` = `now - 60 s`,
    /// `exp` = `now + 540 s`, signed `RSA_PKCS1_SHA256`. `secret` may name no field or `#pem`.
    ///
    /// Policy first, decrypt second, as the module docs say. Every outcome is audited under the
    /// secret's name; an `allowed` entry that cannot be written turns the call into an error.
    pub fn sign_app_jwt(
        &self,
        secret: &VaultRef,
        issuer: &str,
        caller: &str,
        host: &str,
        now: SystemTime,
    ) -> Result<String, GateError> {
        let name = secret.name().to_string();
        // The audit log normalises and bounds the caller itself (`Vault::try_append_audit_log`).
        let audit_caller = caller;
        let audit_host = audit_host(host);
        let deny = |reason: &str, err: GateError| {
            let _ = self.try_append_audit_log(
                Some(audit_caller),
                std::slice::from_ref(&name),
                SIGN_AUDIT_METHOD,
                &audit_host,
                &format!("denied:{reason}"),
            );
            err
        };

        // Adding a field to `VaultField` makes this match a compile error until it is handled.
        match secret.field() {
            None | Some(VaultField::Pem) => {}
        }
        if !valid_issuer(issuer) {
            return Err(deny(
                "issuer",
                GateError::InvalidSecret {
                    name: name.clone(),
                    reason: format!(
                        "the JWT issuer must be 1 to {MAX_ISSUER_LEN} characters of `A-Za-z0-9._-`"
                    ),
                },
            ));
        }
        let now_unix = match now.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(_) => {
                return Err(deny(
                    "clock",
                    GateError::Crypto {
                        reason: "the system clock is before 1970".into(),
                    },
                ));
            }
        };

        // Authenticate the actual current record before trusting any policy fields.
        let fresh = self.fresh().map_err(|err| deny("vault_error", err))?;
        let snapshot = fresh.verified_snapshot_current(&name).map_err(|err| {
            let reason = match &err {
                GateError::NoSuchSecret { .. } => "no_such_secret",
                GateError::PolicyDenied { .. } => policy_reason(&err),
                GateError::VaultTampered { .. } => "tamper",
                _ => "vault_error",
            };
            deny(reason, err)
        })?;
        let meta = &snapshot.metadata;
        if meta.bundle_required || meta.app_binding.is_some() {
            return Err(deny(
                "authority_unavailable",
                GateError::PolicyDenied {
                    secret: name.clone(),
                    reason: "bound signing requires the upgraded live authority path".into(),
                },
            ));
        }
        if let Err(err) = check_signing_policy(meta, caller, host) {
            let reason = policy_reason(&err);
            return Err(deny(reason, err));
        }

        let (_meta, pem) = match fresh.decrypt_bytes(&name) {
            Ok(pair) => pair,
            Err(err) => {
                let reason = match &err {
                    GateError::PolicyDenied { .. } => policy_reason(&err),
                    GateError::NoSuchSecret { .. } => "no_such_secret",
                    _ => "vault_error",
                };
                return Err(deny(reason, err));
            }
        };
        let signed = sign(&pem, issuer, now_unix);
        drop(pem); // wiped here, before the audit write
        let jwt = match signed {
            Ok(jwt) => jwt,
            Err(err) => return Err(deny("bad_key", err)),
        };

        // Fail closed: no entry, no JWT.
        self.try_append_audit_log(
            Some(audit_caller),
            std::slice::from_ref(&name),
            SIGN_AUDIT_METHOD,
            &audit_host,
            "allowed",
        )?;
        Ok(jwt)
    }
}

/// The allow-list checks, on metadata alone. `Err(PolicyDenied)` names which rule refused.
fn check_signing_policy(
    meta: &VaultSecretMetadata,
    caller: &str,
    host: &str,
) -> Result<(), GateError> {
    let secret = meta.name.clone();
    check_lease(meta)?;

    let host_norm = host.trim().to_ascii_lowercase();
    let host_ok = host_shape_ok(&host_norm);
    if !host_ok
        || meta.allowed_hosts.is_empty()
        || !meta
            .allowed_hosts
            .iter()
            .any(|pat| host_matches_pattern(&host_norm, pat))
    {
        return Err(GateError::PolicyDenied {
            secret,
            reason: "the destination host is not in allowed_hosts".to_string(),
        });
    }

    if !meta.allowed_agents.is_empty() {
        let caller = caller.trim().to_ascii_lowercase();
        let permitted = meta
            .allowed_agents
            .iter()
            .any(|a| a == "*" || a.eq_ignore_ascii_case(&caller));
        if !permitted {
            return Err(GateError::PolicyDenied {
                secret,
                reason: "the caller is not in allowed_agents".to_string(),
            });
        }
    }
    Ok(())
}

/// A hostname-shaped label: dot-separated labels of `A-Za-z0-9-_`, none empty (so no leading or
/// trailing dot, no `..`).
fn host_shape_ok(host: &str) -> bool {
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        })
}

fn policy_reason(err: &GateError) -> &'static str {
    match err {
        GateError::PolicyDenied { reason, .. } if reason.starts_with(LEASE_EXPIRED_REASON) => {
            "expired"
        }
        GateError::PolicyDenied { reason, .. } if reason.contains("allowed_hosts") => "host",
        GateError::PolicyDenied { reason, .. } if reason.contains("allowed_agents") => "agent",
        _ => "policy",
    }
}

fn valid_issuer(issuer: &str) -> bool {
    !issuer.is_empty()
        && issuer.len() <= MAX_ISSUER_LEN
        && issuer
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A destination host as the audit log holds it: a hostname-shaped value (bounded) or nothing.
fn audit_host(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    if host_shape_ok(&h) {
        h.chars().take(MAX_AUDIT_FIELD_LEN).collect()
    } else {
        String::new()
    }
}

/// Parse `pem` and sign the JWT. Errors never carry key material.
fn sign(pem: &[u8], issuer: &str, now_unix: u64) -> Result<String, GateError> {
    let key = parse_rsa_pem(pem)?;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iat": now_unix.saturating_sub(IAT_BACKDATE_SECS),
        "exp": now_unix.saturating_add(EXP_AFTER_SECS),
        "iss": issuer,
    });
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let input = format!("{header}.{payload}");
    let mut sig = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        input.as_bytes(),
        &mut sig,
    )
    .map_err(|_| GateError::Crypto {
        reason: "signing the JWT failed".into(),
    })?;
    Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

/// An RSA private key in PEM form: PKCS#1 (`BEGIN RSA PRIVATE KEY`, what GitHub hands out) or
/// PKCS#8 (`BEGIN PRIVATE KEY`), the two forms `creds.rs` accepts. The error says what kind of
/// mistake it was and holds no byte of the key.
fn parse_rsa_pem(pem: &[u8]) -> Result<RsaKeyPair, GateError> {
    let refuse = |why: &'static str| GateError::Crypto {
        reason: format!("the secret is not an RSA private key in PEM form ({why})"),
    };
    if pem.len() > MAX_PEM_BYTES {
        return Err(refuse("it is too large"));
    }
    let text = std::str::from_utf8(pem).map_err(|_| refuse("it is not text"))?;
    let pkcs1 = text.contains("-----BEGIN RSA PRIVATE KEY-----");
    if !pkcs1 && !text.contains("-----BEGIN PRIVATE KEY-----") {
        return Err(refuse("it has no PKCS#1 or PKCS#8 BEGIN line"));
    }
    let b64 = pem_body(text);
    let der = decode_der(&b64).map_err(|()| refuse("the body is not base64"))?;
    let key = if pkcs1 {
        RsaKeyPair::from_der(&der)
    } else {
        RsaKeyPair::from_pkcs8(&der)
    };
    key.map_err(|_| refuse("it is not a valid RSA key"))
}

/// The PEM body: every line that is not a `-----` armor line, trimmed and joined. The buffer is
/// allocated once at the PEM's own length (the body is never longer) and never grows, so no
/// superseded block holding a prefix of the key is left behind un-wiped.
fn pem_body(text: &str) -> Zeroizing<String> {
    let mut body = Zeroizing::new(String::with_capacity(text.len()));
    for line in text.lines().map(str::trim) {
        if !line.starts_with("-----") {
            body.push_str(line);
        }
    }
    body
}

/// Decode `b64` into a buffer allocated once at the decoder's own size bound (never grown).
fn decode_der(b64: &str) -> Result<Zeroizing<Vec<u8>>, ()> {
    let mut der = Zeroizing::new(vec![0u8; base64::decoded_len_estimate(b64.len())]);
    let n = STANDARD
        .decode_slice(b64.as_bytes(), &mut der)
        .map_err(|_| ())?;
    der.truncate(n);
    Ok(der)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_labels_must_be_non_empty_and_hostname_shaped() {
        for ok in [
            "api.github.com",
            "github.com",
            "localhost",
            "a-b.c_d.e",
            "127.0.0.1",
        ] {
            assert!(host_shape_ok(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".github.com",
            "github.com.",
            "a..b",
            "..x",
            "a.b..",
            "api.github.com/x",
            "a b.com",
            "a:80",
            "a@b.com",
            "ex\u{e4}mple.com",
            "a\nb.com",
            "-.",
            ".-",
        ] {
            assert!(!host_shape_ok(bad), "{bad:?}");
        }
        // And through the policy: a pattern that would match the malformed host still denies it.
        let meta = VaultSecretMetadata {
            name: "K".into(),
            description: None,
            allowed_hosts: vec!["*.github.com".into(), "github.com".into(), "*".into()],
            allowed_methods: vec![],
            allowed_agents: vec![],
            allow_private_network: false,
            created_at: String::new(),
            updated_at: String::new(),
            lease_ttl_secs: None,
            expires_at: None,
            usage: crate::vault::SecretUsage::Proxy,
            bundle_required: false,
            app_binding: None,
        };
        assert!(check_signing_policy(&meta, "x", "api.github.com").is_ok());
        for bad in [".github.com", "github.com.", "a..b", "..x", ""] {
            assert!(
                check_signing_policy(&meta, "x", bad).is_err(),
                "{bad:?} must be denied even by `*`"
            );
        }
    }

    #[test]
    fn the_pem_to_der_buffers_are_allocated_once_and_never_grow() {
        // A synthetic body (not a key), many lines, so a growing collect would reallocate.
        let line = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5YWJjZGVmZ2hpamtsbW5vcA==";
        let body: String = (0..40).map(|_| format!("{line}\n")).collect();
        let text = format!("-----BEGIN PRIVATE KEY-----\n{body}-----END PRIVATE KEY-----\n");

        let b64 = pem_body(&text);
        assert_eq!(b64.len(), 40 * line.len());
        assert_eq!(
            b64.capacity(),
            text.len(),
            "the base64 body must be built in one allocation"
        );

        // Only valid base64 decodes; build one from the unpadded body.
        let clean = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU2Nzg5";
        let many = clean.repeat(20);
        let der = decode_der(&many).unwrap();
        assert_eq!(der.len(), many.len() / 4 * 3);
        assert_eq!(
            der.capacity(),
            base64::decoded_len_estimate(many.len()),
            "the DER buffer must be allocated once at the decoder's bound"
        );
        assert!(decode_der("not base64 !!!").is_err());
    }
}
