//! Owner warrants: Ed25519-signed, scoped, expiring grants of Owner authority (Local Record 64).
//!
//! The Owner's Ed25519 key pair is created on the first grant. Its PKCS#8 seed lives in the
//! FerroGate [`Vault`] under [`WARRANT_KEY_SECRET`], and only the public key is written to the
//! ledger (`warrants.json`, next to `vault.enc`). So **verifying** a warrant needs only the
//! ledger, never `vault.key`, and **issuing** one needs the Vault.
//!
//! What a warrant proves is "someone who can open the Vault signed this". Every agent that runs
//! as the Owner's Unix user can read `vault.key`, so this module is only as Owner-only as the
//! Vault is. A caller that needs more must gate [`issue_owner_grant`] itself (the hub does, with
//! the bearer authority).
//!
//! Two forms identify a warrant. The short `grant_id` is looked up in the ledger. The compact
//! token `flw1.<payload>.<signature>` carries the signed fields, so it can be pasted into a
//! message. Both are checked against the **pinned** ledger public key: a key that a token or a
//! stored grant brings with it is never trusted.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as B64URL};
use fl_core::At;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

use crate::GateError;
use crate::vault::{
    SetSecretArgs, Vault, VaultLock, now_unix_millis, reject_if_symlink, resolve_vault_path,
    write_atomic_0600,
};

/// Vault secret that holds the Owner's base64 (standard) Ed25519 PKCS#8 seed.
pub const WARRANT_KEY_SECRET: &str = "FERROGATE_OWNER_WARRANT_ED25519_PKCS8";
/// Ledger file name, beside `vault.enc`.
pub const WARRANT_LEDGER_FILE: &str = "warrants.json";
/// Prefix of the compact, portable token.
pub const TOKEN_PREFIX: &str = "flw1.";
/// Ledger schema version.
pub const WARRANT_SCHEMA_VERSION: u32 = 1;
/// `issued_by` when the caller names no one.
pub const DEFAULT_ISSUER: &str = "lmcfarlin";
/// Lifetime of a grant that names no `ttl_secs` and is not `no_expiry`.
pub const DEFAULT_TTL_SECS: u64 = 3_600;
/// Longest lifetime `issue_owner_grant` accepts: ten years.
pub const MAX_TTL_SECS: u64 = 315_360_000;

const MAX_NAME_LEN: usize = 64;
const MAX_STATEMENT_LEN: usize = 2_000;

/// The signed fields of a grant, in canonical order. `serde_json` writes struct fields in
/// declaration order, so the bytes of this struct are the signed message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantPayload {
    grant_id: String,
    issued_by: String,
    target_agent: String,
    scope: String,
    statement: String,
    created_at: String,
    expires_at: Option<String>,
    single_use: bool,
}

impl GrantPayload {
    fn canonical_bytes(&self) -> Result<Vec<u8>, GateError> {
        serde_json::to_vec(self).map_err(|e| GateError::Crypto {
            reason: format!("could not encode the grant payload: {e}"),
        })
    }
}

/// A signed Owner grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerGrant {
    pub grant_id: String,
    pub issued_by: String,
    /// Lowercase agent persona, or `*` for any agent.
    pub target_agent: String,
    pub scope: String,
    pub statement: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub single_use: bool,
    /// Base64url Ed25519 public key that signed this grant. Informational: it must equal the
    /// ledger's pinned key, and it is not trusted on its own.
    pub public_key_b64: String,
    /// Base64url Ed25519 signature over the canonical payload.
    pub signature_b64: String,
    /// `flw1.<base64url payload>.<signature_b64>`.
    pub compact_token: String,
}

impl OwnerGrant {
    fn payload(&self) -> GrantPayload {
        GrantPayload {
            grant_id: self.grant_id.clone(),
            issued_by: self.issued_by.clone(),
            target_agent: self.target_agent.clone(),
            scope: self.scope.clone(),
            statement: self.statement.clone(),
            created_at: self.created_at.clone(),
            expires_at: self.expires_at.clone(),
            single_use: self.single_use,
        }
    }
}

/// A grant plus its life-cycle state in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredOwnerGrant {
    #[serde(flatten)]
    pub grant: OwnerGrant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_by: Option<String>,
}

/// The `warrants.json` ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerWarrantLedger {
    pub schema_version: u32,
    /// Base64url Ed25519 public key, pinned when the first grant was issued.
    pub public_key_b64: String,
    #[serde(default)]
    pub grants: BTreeMap<String, StoredOwnerGrant>,
}

/// What `issue_owner_grant` needs.
#[derive(Debug, Clone, Default)]
pub struct IssueGrantArgs {
    pub target_agent: String,
    pub scope: String,
    pub statement: String,
    /// Lifetime in seconds. `None` means [`DEFAULT_TTL_SECS`] unless `no_expiry`.
    pub ttl_secs: Option<u64>,
    pub no_expiry: bool,
    pub single_use: bool,
    /// Who the grant names as issuer. `None` means [`DEFAULT_ISSUER`].
    pub issued_by: Option<String>,
}

/// A grant that passed every check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifiedGrant {
    pub grant: OwnerGrant,
    /// `false` for a portable token whose `grant_id` this ledger does not hold. Revocation
    /// cannot be checked for such a token, and a single-use token is refused instead.
    pub in_ledger: bool,
    /// Whether this call marked the grant as consumed.
    pub consumed: bool,
}

fn rejected(reason: impl Into<String>) -> GateError {
    GateError::WarrantRejected {
        reason: reason.into(),
    }
}

fn bad_input(reason: impl Into<String>) -> GateError {
    GateError::WarrantInput {
        reason: reason.into(),
    }
}

/// Path of `warrants.json`: beside `vault.enc`, so `FL_VAULT_PATH` and a home override move both.
pub fn resolve_warrant_ledger_path(home_override: Option<&Path>) -> PathBuf {
    let vault = resolve_vault_path(home_override);
    match vault.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(WARRANT_LEDGER_FILE),
        _ => PathBuf::from(WARRANT_LEDGER_FILE),
    }
}

fn io_err(op: &'static str, path: &Path, e: &std::io::Error) -> GateError {
    GateError::Io {
        op,
        path: path.to_path_buf(),
        cause: e.to_string(),
    }
}

fn read_ledger(path: &Path) -> Result<Option<OwnerWarrantLedger>, GateError> {
    reject_if_symlink(path)?;
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("read", path, &e)),
    };
    let ledger: OwnerWarrantLedger = serde_json::from_str(&raw).map_err(|e| GateError::Io {
        op: "parse",
        path: path.to_path_buf(),
        cause: e.to_string(),
    })?;
    if ledger.schema_version != WARRANT_SCHEMA_VERSION {
        return Err(GateError::Io {
            op: "parse",
            path: path.to_path_buf(),
            cause: format!(
                "unsupported warrant ledger schema {}",
                ledger.schema_version
            ),
        });
    }
    Ok(Some(ledger))
}

fn write_ledger(path: &Path, ledger: &OwnerWarrantLedger) -> Result<(), GateError> {
    let encoded = serde_json::to_string_pretty(ledger).map_err(|e| GateError::Io {
        op: "encode",
        path: path.to_path_buf(),
        cause: e.to_string(),
    })?;
    write_atomic_0600(path, format!("{encoded}\n").as_bytes())
}

/// The Owner key pair from the Vault, created on first use.
fn load_or_create_signing_key(vault: &mut Vault) -> Result<Ed25519KeyPair, GateError> {
    if vault.get_metadata(WARRANT_KEY_SECRET).is_none() {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| GateError::Crypto {
            reason: "failed to generate the Owner warrant Ed25519 key".to_string(),
        })?;
        vault.set(SetSecretArgs {
            name: WARRANT_KEY_SECRET.to_string(),
            value: BASE64.encode(pkcs8.as_ref()),
            description: Some("Owner warrant Ed25519 signing key (PKCS#8)".to_string()),
            ..SetSecretArgs::default()
        })?;
    }
    // Re-open: another process may have created the key first, and its copy is the one on disk.
    let (_meta, encoded) = vault.decrypt_raw(WARRANT_KEY_SECRET)?;
    let pkcs8 = BASE64
        .decode(encoded.trim())
        .map_err(|e| GateError::Crypto {
            reason: format!("the Owner warrant key in the Vault is not base64: {e}"),
        })?;
    Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| GateError::Crypto {
        reason: "the Owner warrant key in the Vault is not a valid Ed25519 PKCS#8 key".to_string(),
    })
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.')
}

fn clean_name(what: &str, raw: &str, extra: &[char]) -> Result<String, GateError> {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() || value.len() > MAX_NAME_LEN {
        return Err(bad_input(format!(
            "{what} must be 1 to {MAX_NAME_LEN} characters"
        )));
    }
    if !value.chars().all(|c| is_name_char(c) || extra.contains(&c)) {
        return Err(bad_input(format!(
            "{what} may only use a-z, 0-9, `_`, `-`, `.`{}",
            if extra.is_empty() { "" } else { " and `:`" }
        )));
    }
    Ok(value)
}

fn clean_target(raw: &str) -> Result<String, GateError> {
    let value = raw.trim();
    if value == "*" {
        return Ok("*".to_string());
    }
    clean_name("target agent", value, &[])
}

fn clean_issuer(raw: &str) -> Result<String, GateError> {
    let value = raw.trim();
    if value.is_empty() || value.len() > MAX_NAME_LEN || value.chars().any(char::is_control) {
        return Err(bad_input(format!(
            "issuer must be 1 to {MAX_NAME_LEN} characters with no control characters"
        )));
    }
    Ok(value.to_string())
}

fn clean_statement(raw: &str) -> Result<String, GateError> {
    let value = raw.trim();
    if value.is_empty() || value.len() > MAX_STATEMENT_LEN {
        return Err(bad_input(format!(
            "statement must be 1 to {MAX_STATEMENT_LEN} bytes"
        )));
    }
    if value.chars().any(|c| c.is_control() && c != '\n') {
        return Err(bad_input("statement must not contain control characters"));
    }
    Ok(value.to_string())
}

fn new_grant_id() -> Result<String, GateError> {
    let mut bytes = [0u8; 8];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| GateError::Crypto {
            reason: "failed to generate a grant id".to_string(),
        })?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("fgw-{hex}"))
}

/// Issue a signed Owner grant and record it in the ledger.
pub fn issue_owner_grant(
    home_override: Option<&Path>,
    args: IssueGrantArgs,
) -> Result<StoredOwnerGrant, GateError> {
    issue_owner_grant_at(home_override, args, now_unix_millis())
}

fn issue_owner_grant_at(
    home_override: Option<&Path>,
    args: IssueGrantArgs,
    now_ms: u64,
) -> Result<StoredOwnerGrant, GateError> {
    let target_agent = clean_target(&args.target_agent)?;
    let scope = clean_name("scope", &args.scope, &[':'])?;
    let statement = clean_statement(&args.statement)?;
    let issued_by = clean_issuer(args.issued_by.as_deref().unwrap_or(DEFAULT_ISSUER))?;
    if args.no_expiry && args.ttl_secs.is_some() {
        return Err(bad_input("ttl_secs and no_expiry exclude each other"));
    }
    let expires_at = if args.no_expiry {
        None
    } else {
        let ttl = args.ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
        if ttl == 0 || ttl > MAX_TTL_SECS {
            return Err(bad_input(format!(
                "ttl_secs must be 1 to {MAX_TTL_SECS} seconds"
            )));
        }
        Some(At::from_unix_millis(now_ms.saturating_add(ttl * 1_000)).to_string())
    };

    let mut vault = Vault::open_default(home_override)?;
    let key = load_or_create_signing_key(&mut vault)?;
    let public_key_b64 = B64URL.encode(key.public_key().as_ref());

    let ledger_path = resolve_warrant_ledger_path(home_override);
    // Read, change and replace the ledger as one step. The lock is the Vault's directory lock.
    let _lock = VaultLock::acquire(&ledger_path)?;
    let mut ledger = match read_ledger(&ledger_path)? {
        Some(ledger) => ledger,
        None => OwnerWarrantLedger {
            schema_version: WARRANT_SCHEMA_VERSION,
            public_key_b64: public_key_b64.clone(),
            grants: BTreeMap::new(),
        },
    };
    if ledger.public_key_b64 != public_key_b64 {
        return Err(GateError::Crypto {
            reason: "the Vault's Owner warrant key does not match the key pinned in the ledger; \
                     refusing to sign"
                .to_string(),
        });
    }

    let mut grant_id = new_grant_id()?;
    while ledger.grants.contains_key(&grant_id) {
        grant_id = new_grant_id()?;
    }
    let payload = GrantPayload {
        grant_id: grant_id.clone(),
        issued_by,
        target_agent,
        scope,
        statement,
        created_at: At::from_unix_millis(now_ms).to_string(),
        expires_at,
        single_use: args.single_use,
    };
    let bytes = payload.canonical_bytes()?;
    let signature_b64 = B64URL.encode(key.sign(&bytes).as_ref());
    let grant = OwnerGrant {
        grant_id: payload.grant_id.clone(),
        issued_by: payload.issued_by.clone(),
        target_agent: payload.target_agent.clone(),
        scope: payload.scope.clone(),
        statement: payload.statement.clone(),
        created_at: payload.created_at.clone(),
        expires_at: payload.expires_at.clone(),
        single_use: payload.single_use,
        public_key_b64,
        compact_token: format!("{TOKEN_PREFIX}{}.{signature_b64}", B64URL.encode(&bytes)),
        signature_b64,
    };
    let stored = StoredOwnerGrant {
        grant,
        revoked_at: None,
        consumed_at: None,
        consumed_by: None,
    };
    ledger.grants.insert(grant_id, stored.clone());
    write_ledger(&ledger_path, &ledger)?;
    Ok(stored)
}

fn verify_signature(
    pinned_b64: &str,
    message: &[u8],
    signature_b64: &str,
) -> Result<(), GateError> {
    let public = B64URL
        .decode(pinned_b64)
        .map_err(|_| rejected("the ledger's pinned public key is not base64url"))?;
    let signature = B64URL
        .decode(signature_b64)
        .map_err(|_| rejected("the signature is not base64url"))?;
    UnparsedPublicKey::new(&ED25519, &public)
        .verify(message, &signature)
        .map_err(|_| rejected("the signature does not verify against the pinned Owner key"))
}

/// A parsed `flw1.` token. The payload bytes are the ones that were signed.
struct ParsedToken {
    payload: GrantPayload,
    payload_bytes: Vec<u8>,
    signature_b64: String,
}

fn parse_token(token: &str) -> Result<ParsedToken, GateError> {
    let rest = token
        .strip_prefix(TOKEN_PREFIX)
        .ok_or_else(|| rejected("not a flw1 token"))?;
    let (payload_b64, signature_b64) = rest
        .split_once('.')
        .ok_or_else(|| rejected("malformed flw1 token"))?;
    let payload_bytes = B64URL
        .decode(payload_b64)
        .map_err(|_| rejected("the token payload is not base64url"))?;
    let payload: GrantPayload = serde_json::from_slice(&payload_bytes)
        .map_err(|_| rejected("the token payload is not a grant"))?;
    // The signature covers these exact bytes. Another encoding of the same fields is refused.
    if payload.canonical_bytes()? != payload_bytes {
        return Err(rejected("the token payload is not in canonical form"));
    }
    Ok(ParsedToken {
        payload,
        payload_bytes,
        signature_b64: signature_b64.to_string(),
    })
}

fn expired(expires_at: Option<&str>, now_ms: u64) -> Result<bool, GateError> {
    match expires_at {
        None => Ok(false),
        Some(text) => {
            let at = At::parse(text).map_err(|_| rejected("the grant has an invalid expiry"))?;
            Ok(at.unix_millis() <= now_ms)
        }
    }
}

/// Verify a warrant by `grant_id` or `flw1.` token. With `target_agent` set, the grant must
/// name that agent or `*`. With `consume`, a valid grant is marked consumed in the same step
/// as the check, whether or not it is single-use.
///
/// Verification needs only the ledger, not the Vault.
pub fn verify_owner_grant(
    home_override: Option<&Path>,
    id_or_token: &str,
    target_agent: Option<&str>,
    consume: bool,
) -> Result<VerifiedGrant, GateError> {
    verify_owner_grant_at(
        home_override,
        id_or_token,
        target_agent,
        consume,
        now_unix_millis(),
    )
}

fn verify_owner_grant_at(
    home_override: Option<&Path>,
    id_or_token: &str,
    target_agent: Option<&str>,
    consume: bool,
    now_ms: u64,
) -> Result<VerifiedGrant, GateError> {
    let id_or_token = id_or_token.trim();
    if id_or_token.is_empty() {
        return Err(bad_input("a grant id or token is required"));
    }
    let ledger_path = resolve_warrant_ledger_path(home_override);
    // A consuming check reads and writes under the lock, so two callers cannot both win.
    let _lock = if consume {
        Some(VaultLock::acquire(&ledger_path)?)
    } else {
        None
    };
    let mut ledger = read_ledger(&ledger_path)?
        .ok_or_else(|| rejected("no warrant ledger exists; no grant has been issued"))?;

    let (grant, in_ledger) = if id_or_token.starts_with(TOKEN_PREFIX) {
        let token = parse_token(id_or_token)?;
        verify_signature(
            &ledger.public_key_b64,
            &token.payload_bytes,
            &token.signature_b64,
        )?;
        match ledger.grants.get(&token.payload.grant_id) {
            Some(stored) => {
                // The ledger entry is the record. A token that differs from it is not that grant.
                if stored.grant.payload() != token.payload
                    || stored.grant.signature_b64 != token.signature_b64
                {
                    return Err(rejected("the token does not match the ledger entry"));
                }
                (stored.grant.clone(), true)
            }
            None => {
                if token.payload.single_use {
                    return Err(rejected(
                        "a single-use token that this ledger does not hold cannot be honored",
                    ));
                }
                let p = token.payload;
                (
                    OwnerGrant {
                        grant_id: p.grant_id,
                        issued_by: p.issued_by,
                        target_agent: p.target_agent,
                        scope: p.scope,
                        statement: p.statement,
                        created_at: p.created_at,
                        expires_at: p.expires_at,
                        single_use: p.single_use,
                        public_key_b64: ledger.public_key_b64.clone(),
                        signature_b64: token.signature_b64,
                        compact_token: id_or_token.to_string(),
                    },
                    false,
                )
            }
        }
    } else {
        let stored = ledger
            .grants
            .get(id_or_token)
            .ok_or_else(|| rejected(format!("unknown grant `{id_or_token}`")))?;
        // A key the stored grant brings is not the Owner's key unless it is the pinned one.
        if stored.grant.public_key_b64 != ledger.public_key_b64 {
            return Err(rejected(
                "the grant was signed with a key other than the pinned Owner key",
            ));
        }
        let bytes = stored.grant.payload().canonical_bytes()?;
        verify_signature(&ledger.public_key_b64, &bytes, &stored.grant.signature_b64)?;
        (stored.grant.clone(), true)
    };

    if in_ledger {
        let stored = &ledger.grants[&grant.grant_id];
        if stored.revoked_at.is_some() {
            return Err(rejected(format!("grant `{}` was revoked", grant.grant_id)));
        }
        if stored.consumed_at.is_some() && grant.single_use {
            return Err(rejected(format!(
                "single-use grant `{}` was already consumed",
                grant.grant_id
            )));
        }
    }
    if expired(grant.expires_at.as_deref(), now_ms)? {
        return Err(rejected(format!("grant `{}` has expired", grant.grant_id)));
    }
    if let Some(agent) = target_agent {
        let agent = agent.trim();
        if grant.target_agent != "*" && !grant.target_agent.eq_ignore_ascii_case(agent) {
            return Err(rejected(format!(
                "grant `{}` is for `{}`, not `{agent}`",
                grant.grant_id, grant.target_agent
            )));
        }
    }

    let mut consumed = false;
    if consume && in_ledger {
        let stored = ledger
            .grants
            .get_mut(&grant.grant_id)
            .expect("checked above");
        stored.consumed_at = Some(At::from_unix_millis(now_ms).to_string());
        stored.consumed_by = Some(
            target_agent
                .map(|a| a.trim().to_ascii_lowercase())
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| "unknown".to_string()),
        );
        write_ledger(&ledger_path, &ledger)?;
        consumed = true;
    }
    Ok(VerifiedGrant {
        grant,
        in_ledger,
        consumed,
    })
}

/// The ledger's pinned public key, or `None` before the first grant.
pub fn owner_warrant_public_key(home_override: Option<&Path>) -> Result<Option<String>, GateError> {
    Ok(read_ledger(&resolve_warrant_ledger_path(home_override))?.map(|l| l.public_key_b64))
}

/// Every grant in the ledger, oldest first. `active_only` leaves out revoked, consumed
/// single-use and expired grants.
pub fn list_owner_grants(
    home_override: Option<&Path>,
    active_only: bool,
) -> Result<Vec<StoredOwnerGrant>, GateError> {
    list_owner_grants_at(home_override, active_only, now_unix_millis())
}

fn list_owner_grants_at(
    home_override: Option<&Path>,
    active_only: bool,
    now_ms: u64,
) -> Result<Vec<StoredOwnerGrant>, GateError> {
    let Some(ledger) = read_ledger(&resolve_warrant_ledger_path(home_override))? else {
        return Ok(Vec::new());
    };
    let mut grants: Vec<StoredOwnerGrant> = ledger.grants.into_values().collect();
    if active_only {
        grants.retain(|stored| {
            stored.revoked_at.is_none()
                && !(stored.grant.single_use && stored.consumed_at.is_some())
                && !expired(stored.grant.expires_at.as_deref(), now_ms).unwrap_or(true)
        });
    }
    grants.sort_by(|a, b| {
        (&a.grant.created_at, &a.grant.grant_id).cmp(&(&b.grant.created_at, &b.grant.grant_id))
    });
    Ok(grants)
}

/// Revoke a grant. Revoking a revoked grant keeps its first `revoked_at`.
pub fn revoke_owner_grant(
    home_override: Option<&Path>,
    grant_id: &str,
) -> Result<StoredOwnerGrant, GateError> {
    revoke_owner_grant_at(home_override, grant_id, now_unix_millis())
}

fn revoke_owner_grant_at(
    home_override: Option<&Path>,
    grant_id: &str,
    now_ms: u64,
) -> Result<StoredOwnerGrant, GateError> {
    let grant_id = grant_id.trim();
    let ledger_path = resolve_warrant_ledger_path(home_override);
    let _lock = VaultLock::acquire(&ledger_path)?;
    let mut ledger = read_ledger(&ledger_path)?
        .ok_or_else(|| rejected("no warrant ledger exists; no grant has been issued"))?;
    let stored = ledger
        .grants
        .get_mut(grant_id)
        .ok_or_else(|| rejected(format!("unknown grant `{grant_id}`")))?;
    if stored.revoked_at.is_none() {
        stored.revoked_at = Some(At::from_unix_millis(now_ms).to_string());
    }
    let out = stored.clone();
    write_ledger(&ledger_path, &ledger)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn args(target: &str) -> IssueGrantArgs {
        IssueGrantArgs {
            target_agent: target.to_string(),
            scope: "agent.replace".to_string(),
            statement: "Owner approves replacing the registry entry.".to_string(),
            ..IssueGrantArgs::default()
        }
    }

    fn ledger_of(home: &Path) -> OwnerWarrantLedger {
        read_ledger(&resolve_warrant_ledger_path(Some(home)))
            .unwrap()
            .unwrap()
    }

    fn put_ledger(home: &Path, ledger: &OwnerWarrantLedger) {
        write_ledger(&resolve_warrant_ledger_path(Some(home)), ledger).unwrap();
    }

    fn err_text(result: Result<VerifiedGrant, GateError>) -> String {
        result.expect_err("must be rejected").to_string()
    }

    #[test]
    fn issue_then_verify_by_id_and_by_token() {
        let home = TempDir::new().unwrap();
        let stored = issue_owner_grant(Some(home.path()), args(" Ferris ")).unwrap();
        let g = &stored.grant;
        assert!(g.grant_id.starts_with("fgw-") && g.grant_id.len() == 20);
        assert_eq!(g.target_agent, "ferris");
        assert_eq!(g.issued_by, DEFAULT_ISSUER);
        assert!(g.expires_at.is_some());
        assert!(g.compact_token.starts_with(TOKEN_PREFIX));
        assert_eq!(
            owner_warrant_public_key(Some(home.path()))
                .unwrap()
                .unwrap(),
            g.public_key_b64
        );

        let by_id =
            verify_owner_grant(Some(home.path()), &g.grant_id, Some("FERRIS"), false).unwrap();
        assert_eq!(by_id.grant, *g);
        assert!(by_id.in_ledger && !by_id.consumed);
        let by_token =
            verify_owner_grant(Some(home.path()), &g.compact_token, Some("ferris"), false).unwrap();
        assert_eq!(by_token.grant, *g);
    }

    #[test]
    fn the_signing_key_lives_in_the_vault_and_the_seed_is_not_in_the_ledger() {
        let home = TempDir::new().unwrap();
        let a = issue_owner_grant(Some(home.path()), args("*")).unwrap();
        let b = issue_owner_grant(Some(home.path()), args("*")).unwrap();
        // One key pair signs every grant.
        assert_eq!(a.grant.public_key_b64, b.grant.public_key_b64);
        assert_ne!(a.grant.grant_id, b.grant.grant_id);
        let vault = Vault::open_default(Some(home.path())).unwrap();
        let (_meta, seed) = vault.decrypt_raw(WARRANT_KEY_SECRET).unwrap();
        let ledger_raw =
            fs::read_to_string(resolve_warrant_ledger_path(Some(home.path()))).unwrap();
        assert!(!ledger_raw.contains(&seed));
        // A reader needs no Vault: verification works with the key files gone.
        let _ = fs::remove_file(crate::resolve_master_key_path(Some(home.path())));
        let _ = fs::remove_file(resolve_vault_path(Some(home.path())));
        verify_owner_grant(Some(home.path()), &a.grant.grant_id, None, false).unwrap();
    }

    #[test]
    fn a_tampered_grant_field_or_signature_is_rejected() {
        let home = TempDir::new().unwrap();
        let stored = issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        let id = stored.grant.grant_id.clone();

        let mut ledger = ledger_of(home.path());
        ledger.grants.get_mut(&id).unwrap().grant.scope = "agents.md.edit".to_string();
        put_ledger(home.path(), &ledger);
        let text = err_text(verify_owner_grant(Some(home.path()), &id, None, false));
        assert!(text.contains("signature"), "{text}");

        // A flipped signature bit.
        let mut ledger = ledger_of(home.path());
        let entry = ledger.grants.get_mut(&id).unwrap();
        entry.grant.scope = stored.grant.scope.clone();
        let mut sig = B64URL.decode(&entry.grant.signature_b64).unwrap();
        sig[0] ^= 1;
        entry.grant.signature_b64 = B64URL.encode(sig);
        put_ledger(home.path(), &ledger);
        let text = err_text(verify_owner_grant(Some(home.path()), &id, None, false));
        assert!(text.contains("signature"), "{text}");
    }

    #[test]
    fn a_tampered_token_payload_is_rejected() {
        let home = TempDir::new().unwrap();
        let stored = issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        let parsed = parse_token(&stored.grant.compact_token).unwrap();
        let mut forged = parsed.payload.clone();
        forged.target_agent = "*".to_string();
        let token = format!(
            "{TOKEN_PREFIX}{}.{}",
            B64URL.encode(forged.canonical_bytes().unwrap()),
            parsed.signature_b64
        );
        let text = err_text(verify_owner_grant(Some(home.path()), &token, None, false));
        assert!(text.contains("signature"), "{text}");
        // The same fields in another encoding are refused before the signature is tried.
        let spaced = serde_json::to_string_pretty(&parsed.payload).unwrap();
        let token = format!(
            "{TOKEN_PREFIX}{}.{}",
            B64URL.encode(spaced),
            parsed.signature_b64
        );
        let text = err_text(verify_owner_grant(Some(home.path()), &token, None, false));
        assert!(text.contains("canonical"), "{text}");
        for junk in ["flw1.", "flw1.abc", "flw1.!!.!!", "flw1.e30.AAAA"] {
            assert!(verify_owner_grant(Some(home.path()), junk, None, false).is_err());
        }
    }

    #[test]
    fn a_token_signed_with_an_attacker_key_is_rejected() {
        let home = TempDir::new().unwrap();
        let real = issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let attacker = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let payload = GrantPayload {
            grant_id: "fgw-0000000000000000".to_string(),
            issued_by: DEFAULT_ISSUER.to_string(),
            target_agent: "*".to_string(),
            scope: "agent.replace".to_string(),
            statement: "forged".to_string(),
            created_at: real.grant.created_at.clone(),
            expires_at: None,
            single_use: false,
        };
        let bytes = payload.canonical_bytes().unwrap();
        let sig = B64URL.encode(attacker.sign(&bytes).as_ref());
        let token = format!("{TOKEN_PREFIX}{}.{sig}", B64URL.encode(&bytes));
        let text = err_text(verify_owner_grant(Some(home.path()), &token, None, false));
        assert!(text.contains("pinned Owner key"), "{text}");
    }

    #[test]
    fn a_ledger_grant_carrying_an_attacker_key_is_rejected() {
        let home = TempDir::new().unwrap();
        let real = issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let attacker = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        // The attacker re-signs the grant with their own key and names that key in it.
        let mut ledger = ledger_of(home.path());
        let entry = ledger.grants.get_mut(&real.grant.grant_id).unwrap();
        entry.grant.target_agent = "*".to_string();
        let bytes = entry.grant.payload().canonical_bytes().unwrap();
        entry.grant.signature_b64 = B64URL.encode(attacker.sign(&bytes).as_ref());
        entry.grant.public_key_b64 = B64URL.encode(attacker.public_key().as_ref());
        put_ledger(home.path(), &ledger);
        let text = err_text(verify_owner_grant(
            Some(home.path()),
            &real.grant.grant_id,
            None,
            false,
        ));
        assert!(text.contains("pinned Owner key"), "{text}");
    }

    #[test]
    fn an_expired_grant_is_rejected_and_no_expiry_never_expires() {
        let home = TempDir::new().unwrap();
        let now = 1_800_000_000_000;
        let short = IssueGrantArgs {
            ttl_secs: Some(60),
            ..args("ferris")
        };
        let timed = issue_owner_grant_at(Some(home.path()), short, now).unwrap();
        let forever = issue_owner_grant_at(
            Some(home.path()),
            IssueGrantArgs {
                no_expiry: true,
                ..args("ferris")
            },
            now,
        )
        .unwrap();
        assert!(forever.grant.expires_at.is_none());
        let id = &timed.grant.grant_id;
        verify_owner_grant_at(Some(home.path()), id, None, false, now + 59_000).unwrap();
        let text = err_text(verify_owner_grant_at(
            Some(home.path()),
            id,
            None,
            false,
            now + 60_000,
        ));
        assert!(text.contains("expired"), "{text}");
        verify_owner_grant_at(
            Some(home.path()),
            &forever.grant.grant_id,
            None,
            false,
            now + 10 * 365 * 86_400_000,
        )
        .unwrap();
        let active = list_owner_grants_at(Some(home.path()), true, now + 120_000).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].grant.grant_id, forever.grant.grant_id);
        assert_eq!(
            list_owner_grants_at(Some(home.path()), false, now)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn default_ttl_is_one_hour_and_bad_ttls_are_refused() {
        let home = TempDir::new().unwrap();
        let now = 1_800_000_000_000;
        let g = issue_owner_grant_at(Some(home.path()), args("ferris"), now).unwrap();
        assert_eq!(
            g.grant.expires_at.as_deref(),
            Some(At::from_unix_millis(now + 3_600_000).to_string().as_str())
        );
        for bad in [
            IssueGrantArgs {
                ttl_secs: Some(0),
                ..args("ferris")
            },
            IssueGrantArgs {
                ttl_secs: Some(MAX_TTL_SECS + 1),
                ..args("ferris")
            },
            IssueGrantArgs {
                ttl_secs: Some(5),
                no_expiry: true,
                ..args("ferris")
            },
        ] {
            assert!(matches!(
                issue_owner_grant(Some(home.path()), bad),
                Err(GateError::WarrantInput { .. })
            ));
        }
    }

    #[test]
    fn single_use_is_consumed_once_and_a_plain_grant_can_be_reused() {
        let home = TempDir::new().unwrap();
        let once = issue_owner_grant(
            Some(home.path()),
            IssueGrantArgs {
                single_use: true,
                ..args("ferris")
            },
        )
        .unwrap();
        let id = &once.grant.grant_id;
        // A check without `consume` does not spend it.
        verify_owner_grant(Some(home.path()), id, Some("ferris"), false).unwrap();
        let spent = verify_owner_grant(Some(home.path()), id, Some("ferris"), true).unwrap();
        assert!(spent.consumed);
        let stored = &ledger_of(home.path()).grants[id];
        assert!(stored.consumed_at.is_some());
        assert_eq!(stored.consumed_by.as_deref(), Some("ferris"));
        let text = err_text(verify_owner_grant(
            Some(home.path()),
            id,
            Some("ferris"),
            true,
        ));
        assert!(text.contains("already consumed"), "{text}");
        // The token form is the same grant.
        let text = err_text(verify_owner_grant(
            Some(home.path()),
            &once.grant.compact_token,
            None,
            false,
        ));
        assert!(text.contains("already consumed"), "{text}");

        let plain = issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        for _ in 0..3 {
            verify_owner_grant(Some(home.path()), &plain.grant.grant_id, None, true).unwrap();
        }
    }

    #[test]
    fn a_consuming_check_is_atomic_across_threads() {
        let home = TempDir::new().unwrap();
        let once = issue_owner_grant(
            Some(home.path()),
            IssueGrantArgs {
                single_use: true,
                ..args("*")
            },
        )
        .unwrap();
        let id = once.grant.grant_id.clone();
        let path = home.path().to_path_buf();
        let wins: usize = (0..8)
            .map(|_| {
                let (path, id) = (path.clone(), id.clone());
                std::thread::spawn(move || {
                    verify_owner_grant(Some(&path), &id, Some("felix"), true).is_ok()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum();
        assert_eq!(wins, 1);
    }

    #[test]
    fn a_revoked_grant_is_rejected_by_id_and_by_token() {
        let home = TempDir::new().unwrap();
        let g = issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        let revoked = revoke_owner_grant(Some(home.path()), &g.grant.grant_id).unwrap();
        let first = revoked.revoked_at.clone().unwrap();
        for form in [&g.grant.grant_id, &g.grant.compact_token] {
            let text = err_text(verify_owner_grant(Some(home.path()), form, None, false));
            assert!(text.contains("revoked"), "{text}");
        }
        // A second revoke keeps the first timestamp.
        let again =
            revoke_owner_grant_at(Some(home.path()), &g.grant.grant_id, u64::MAX / 4).unwrap();
        assert_eq!(again.revoked_at.unwrap(), first);
        assert!(
            list_owner_grants(Some(home.path()), true)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            list_owner_grants(Some(home.path()), false).unwrap().len(),
            1
        );
        assert!(revoke_owner_grant(Some(home.path()), "fgw-missing").is_err());
    }

    #[test]
    fn the_target_agent_must_match_unless_the_grant_is_for_everyone() {
        let home = TempDir::new().unwrap();
        let one = issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        let any = issue_owner_grant(Some(home.path()), args("*")).unwrap();
        let text = err_text(verify_owner_grant(
            Some(home.path()),
            &one.grant.grant_id,
            Some("feira"),
            false,
        ));
        assert!(text.contains("not `feira`"), "{text}");
        verify_owner_grant(
            Some(home.path()),
            &one.grant.grant_id,
            Some("Ferris"),
            false,
        )
        .unwrap();
        verify_owner_grant(Some(home.path()), &one.grant.grant_id, None, false).unwrap();
        verify_owner_grant(Some(home.path()), &any.grant.grant_id, Some("feira"), false).unwrap();
    }

    #[test]
    fn an_unknown_id_and_a_missing_ledger_are_rejections() {
        let home = TempDir::new().unwrap();
        assert!(matches!(
            verify_owner_grant(Some(home.path()), "fgw-0000000000000000", None, false),
            Err(GateError::WarrantRejected { .. })
        ));
        assert!(
            list_owner_grants(Some(home.path()), false)
                .unwrap()
                .is_empty()
        );
        issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        assert!(matches!(
            verify_owner_grant(Some(home.path()), "fgw-0000000000000000", None, false),
            Err(GateError::WarrantRejected { .. })
        ));
        assert!(matches!(
            verify_owner_grant(Some(home.path()), "  ", None, false),
            Err(GateError::WarrantInput { .. })
        ));
    }

    #[test]
    fn a_portable_token_from_the_same_key_is_honored_only_when_it_is_not_single_use() {
        let home = TempDir::new().unwrap();
        let g = issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        let once = issue_owner_grant(
            Some(home.path()),
            IssueGrantArgs {
                single_use: true,
                ..args("ferris")
            },
        )
        .unwrap();
        // Another machine's ledger: same pinned key, without these entries.
        let mut ledger = ledger_of(home.path());
        ledger.grants.clear();
        put_ledger(home.path(), &ledger);
        let ok = verify_owner_grant(Some(home.path()), &g.grant.compact_token, None, true).unwrap();
        assert!(!ok.in_ledger && !ok.consumed);
        let text = err_text(verify_owner_grant(
            Some(home.path()),
            &once.grant.compact_token,
            None,
            false,
        ));
        assert!(text.contains("single-use"), "{text}");
    }

    #[test]
    fn a_token_that_disagrees_with_its_ledger_entry_is_rejected() {
        let home = TempDir::new().unwrap();
        let a = issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        // Replace the ledger entry's fields with another validly signed grant's fields.
        let b = issue_owner_grant(Some(home.path()), args("felix")).unwrap();
        let mut ledger = ledger_of(home.path());
        let mut swapped = ledger.grants[&b.grant.grant_id].clone();
        swapped.grant.grant_id = a.grant.grant_id.clone();
        ledger.grants.insert(a.grant.grant_id.clone(), swapped);
        put_ledger(home.path(), &ledger);
        let text = err_text(verify_owner_grant(
            Some(home.path()),
            &a.grant.compact_token,
            None,
            false,
        ));
        assert!(text.contains("does not match the ledger"), "{text}");
    }

    #[test]
    fn issue_refuses_a_vault_key_that_is_not_the_pinned_key() {
        let home = TempDir::new().unwrap();
        issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        let mut ledger = ledger_of(home.path());
        ledger.public_key_b64 = B64URL.encode([7u8; 32]);
        put_ledger(home.path(), &ledger);
        let err = issue_owner_grant(Some(home.path()), args("ferris")).unwrap_err();
        assert!(err.to_string().contains("does not match the key pinned"));
    }

    #[test]
    fn issue_validates_every_field() {
        let home = TempDir::new().unwrap();
        let cases = [
            IssueGrantArgs {
                target_agent: "".into(),
                ..args("x")
            },
            IssueGrantArgs {
                target_agent: "fer ris".into(),
                ..args("x")
            },
            IssueGrantArgs {
                target_agent: "a".repeat(65),
                ..args("x")
            },
            IssueGrantArgs {
                scope: "Bad Scope!".into(),
                ..args("ferris")
            },
            IssueGrantArgs {
                scope: "".into(),
                ..args("ferris")
            },
            IssueGrantArgs {
                statement: "  ".into(),
                ..args("ferris")
            },
            IssueGrantArgs {
                statement: "x".repeat(MAX_STATEMENT_LEN + 1),
                ..args("ferris")
            },
            IssueGrantArgs {
                statement: "bell\u{7}".into(),
                ..args("ferris")
            },
            IssueGrantArgs {
                issued_by: Some("owner\nforged".into()),
                ..args("ferris")
            },
        ];
        for case in cases {
            assert!(
                matches!(
                    issue_owner_grant(Some(home.path()), case.clone()),
                    Err(GateError::WarrantInput { .. })
                ),
                "{case:?}"
            );
        }
        // Nothing was created by the refusals.
        assert!(!resolve_warrant_ledger_path(Some(home.path())).exists());
        let ok = issue_owner_grant(
            Some(home.path()),
            IssueGrantArgs {
                scope: "Role.Recast:Lead".into(),
                issued_by: Some("lmcfarlin".into()),
                statement: "line one\nline two".into(),
                ..args("*")
            },
        )
        .unwrap();
        assert_eq!(ok.grant.scope, "role.recast:lead");
        assert_eq!(ok.grant.target_agent, "*");
    }

    #[cfg(unix)]
    #[test]
    fn the_ledger_is_private_and_a_symlinked_ledger_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let home = TempDir::new().unwrap();
        issue_owner_grant(Some(home.path()), args("ferris")).unwrap();
        let path = resolve_warrant_ledger_path(Some(home.path()));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let target = home.path().join("elsewhere.json");
        fs::copy(&path, &target).unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(list_owner_grants(Some(home.path()), false).is_err());
    }
}
