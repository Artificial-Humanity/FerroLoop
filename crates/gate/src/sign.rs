//! A policy-checked RS256 signer: [`Vault::sign_app_jwt`] signs a GitHub-App-style JWT with an
//! RSA private key held in the Vault and returns the JWT, never the key.
//!
//! The policy runs BEFORE the key is decrypted: the secret must exist, its lease must be alive,
//! the caller must be in `allowed_agents` (when that list is not empty), and the destination
//! `host` must match `allowed_hosts` (an empty list denies). Every outcome, the allowed one and
//! each denial, is appended to `gate-audit.jsonl` with the secret's NAME only. If the audit
//! entry for an allowed signature cannot be written, no JWT is returned.
//!
//! ⚠ Custody: this is in-process signing. The calling process decrypts the key with the master
//! key it already holds (`vault.key`, same Unix user), so `allowed_agents` is advisory on the
//! local CLI path and authoritative only where the hub holds the key and authenticates the
//! caller. Nothing here is an enclave or an HSM.
//!
//! The PEM plaintext lives in [`Zeroizing`] buffers and is wiped as soon as the signature is
//! made. `ring` copies the parsed key into its own key pair, which this crate does not control;
//! that key pair is dropped immediately after signing.

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
        let audit_caller = clip(caller);
        let audit_host = audit_host(host);
        let deny = |reason: &str, err: GateError| {
            let _ = self.try_append_audit_log(
                Some(&audit_caller),
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

        // Policy, from the redacted metadata, before any decryption.
        let Some(meta) = self.get_metadata(&name) else {
            return Err(deny(
                "no_such_secret",
                GateError::NoSuchSecret { name: name.clone() },
            ));
        };
        if let Err(err) = check_signing_policy(meta, caller, host) {
            let reason = policy_reason(&err);
            return Err(deny(reason, err));
        }

        let (_meta, pem) = match self.decrypt_bytes(&name) {
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
            Some(&audit_caller),
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
    let host_ok = !host_norm.is_empty()
        && host_norm
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
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

/// A caller or host value as the audit log holds it: bounded.
fn clip(value: &str) -> String {
    value.chars().take(MAX_AUDIT_FIELD_LEN).collect()
}

/// A destination host as the audit log holds it: a hostname-shaped value or nothing.
fn audit_host(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    if h.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        clip(&h)
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
    let b64: Zeroizing<String> = Zeroizing::new(
        text.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("-----"))
            .collect(),
    );
    let der = Zeroizing::new(
        STANDARD
            .decode(b64.as_bytes())
            .map_err(|_| refuse("the body is not base64"))?,
    );
    let key = if pkcs1 {
        RsaKeyPair::from_der(&der)
    } else {
        RsaKeyPair::from_pkcs8(&der)
    };
    key.map_err(|_| refuse("it is not a valid RSA key"))
}
