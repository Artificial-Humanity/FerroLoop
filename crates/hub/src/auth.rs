//! Hub authentication, two-tier authority (`Owner` vs. `Peer`), one-time pairing codes,
//! `HttpOnly; SameSite=Strict` session cookies, Tailscale header trust, and constant-time
//! token comparison (spec §2.2).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fl_core::pr_policy::DEFAULT_OWNER_LOGIN;
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::StateStore;
use fl_exec::stamp;
use fl_store::RedbStore;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

/// Cookie name used for browser `FerroDeck` sessions (spec §2.2).
pub const SESSION_COOKIE_NAME: &str = "fl_hub_session";

/// Default pairing code time-to-live in seconds (10 minutes, spec §2.2).
pub const DEFAULT_PAIRING_TTL_SECS: u64 = 600;

/// Default owner session time-to-live in seconds (24 hours, spec §2.2).
pub const DEFAULT_SESSION_TTL_SECS: u64 = 24 * 3600;

/// Authority tier resolved for an authenticated HTTP or WebSocket request (spec §2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum HubAuthority {
    Owner {
        login: String,
    },
    Peer {
        #[serde(default)]
        agent: Option<String>,
    },
}

impl HubAuthority {
    pub fn is_owner(&self) -> bool {
        matches!(self, Self::Owner { .. })
    }

    pub fn actor_name(&self) -> &str {
        match self {
            Self::Owner { login } => login.as_str(),
            Self::Peer { agent } => agent.as_deref().unwrap_or("peer"),
        }
    }
}

/// Newly minted one-time pairing code and URL returned by `fl hub pair` (spec §2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingInfo {
    pub code: String,
    pub expires_at_ms: u64,
    pub ttl_secs: u64,
    pub pair_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredPairingEntry {
    pub kind: String,
    pub key: String,
    pub code: String,
    pub expires_at_ms: u64,
    pub used: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSessionEntry {
    pub kind: String,
    pub key: String,
    pub token_hash: String,
    pub login: String,
    pub expires_at_ms: u64,
    pub revoked: bool,
}

#[derive(Debug, Default)]
struct AuthCache {
    pairings: BTreeMap<String, StoredPairingEntry>,
    sessions: BTreeMap<String, StoredSessionEntry>,
}

/// Generate a cryptographically random lowercase hex token with `byte_len` bytes (`2 * byte_len` hex chars).
pub fn generate_hex_token(byte_len: usize) -> String {
    let len = byte_len.max(16);
    let rng = SystemRandom::new();
    let mut bytes = vec![0u8; len];
    if rng.fill(&mut bytes).is_err() {
        let u1 = uuid::Uuid::now_v7();
        let u2 = uuid::Uuid::now_v7();
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = if i < 16 {
                u1.as_bytes()[i]
            } else {
                u2.as_bytes()[i % 16]
            };
        }
    }
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex_encode(&digest)
}

/// Constant-time byte comparison for bearer tokens and secrets (spec §2.2).
/// Returns `false` if either input is empty.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    if a_bytes.is_empty() || b_bytes.is_empty() {
        return false;
    }
    let max_len = a_bytes.len().max(b_bytes.len());
    let mut diff = a_bytes.len() ^ b_bytes.len();
    for i in 0..max_len {
        let x = a_bytes.get(i).copied().unwrap_or(0);
        let y = b_bytes.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

fn sanitize_slug_tail(raw: &str, max_len: usize) -> String {
    let mut out = String::new();
    for ch in raw.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '-' || lower == '_' {
            out.push(lower);
        }
        if out.len() >= max_len {
            break;
        }
    }
    if out.is_empty() {
        sha256_hex(raw)[..max_len.min(36)].to_string()
    } else {
        out
    }
}

/// Deterministic `StateEntry.slug` for pairing code `code` under `StateScope::Workspace`.
pub fn pairing_state_slug(code: &str) -> String {
    let clean = code
        .trim()
        .strip_prefix("hub/pairing/")
        .unwrap_or(code.trim());
    format!("hub-pairing-{}", sanitize_slug_tail(clean, 48))
}

/// Deterministic `StateEntry.slug` for session `token_hash` under `StateScope::Workspace`.
pub fn session_state_slug(token_hash: &str) -> String {
    let clean = token_hash
        .trim()
        .strip_prefix("hub/session/")
        .unwrap_or(token_hash.trim());
    format!("hub-session-{}", sanitize_slug_tail(clean, 48))
}

fn now_millis() -> u64 {
    stamp::now().unix_millis()
}

/// Resolve the configured Hub owner token from `$FL_HUB_OWNER_TOKEN` or `owner_token_file` (spec §2.2).
pub fn resolve_owner_token(owner_token_file: Option<&Path>) -> Option<String> {
    if let Ok(env_tok) = std::env::var("FL_HUB_OWNER_TOKEN") {
        let trimmed = env_tok.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(path) = owner_token_file
        && let Ok(raw) = fs::read_to_string(path)
    {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Resolve the configured FerroWire peer token from `$FL_WIRE_TOKEN` or `wire_token_file` (spec §2.2).
pub fn resolve_peer_token(wire_token_file: Option<&Path>) -> Option<String> {
    if let Ok(env_tok) = std::env::var("FL_WIRE_TOKEN") {
        let trimmed = env_tok.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(path) = wire_token_file
        && let Ok(raw) = fs::read_to_string(path)
    {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Mint and persist a single-use pairing code directly in `store` (`StateScope::Workspace`),
/// usable by `fl hub pair` while `fl hub serve` is running (spec §2.2).
pub fn mint_pairing_code_in_store(
    store: &RedbStore,
    base_url: &str,
    ttl_secs: u64,
) -> Result<PairingInfo, String> {
    let code = generate_hex_token(16);
    let ttl = if ttl_secs == 0 {
        DEFAULT_PAIRING_TTL_SECS
    } else {
        ttl_secs
    };
    let now_ms = now_millis();
    let expires_at_ms = now_ms.saturating_add(ttl.saturating_mul(1000));
    let entry = StoredPairingEntry {
        kind: "hub/pairing".to_string(),
        key: format!("hub/pairing/{code}"),
        code: code.clone(),
        expires_at_ms,
        used: false,
    };
    persist_pairing_entry(store, &entry)?;
    let pair_url = format!("{}/?pair={code}", base_url.trim_end_matches('/'));
    Ok(PairingInfo {
        code,
        expires_at_ms,
        ttl_secs: ttl,
        pair_url,
    })
}

fn persist_pairing_entry(store: &RedbStore, entry: &StoredPairingEntry) -> Result<(), String> {
    let val = serde_json::to_value(entry).map_err(|e| e.to_string())?;
    store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: pairing_state_slug(&entry.code),
            sensitivity: SensitivityPatch::SetSensitive,
            payload: StatePayload::Runtime(RuntimeEntry {
                value: val,
                merge_mode: MergeMode::LastWriter,
            }),
            at: stamp::now(),
            by: "fl-hub".to_string(),
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn load_pairing_entry(store: &RedbStore, code: &str) -> Option<StoredPairingEntry> {
    let state_entry = store
        .get_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            &pairing_state_slug(code),
        )
        .ok()??;
    let StatePayload::Runtime(rt) = state_entry.payload else {
        return None;
    };
    serde_json::from_value(rt.value).ok()
}

fn persist_session_entry(store: &RedbStore, entry: &StoredSessionEntry) -> Result<(), String> {
    let val = serde_json::to_value(entry).map_err(|e| e.to_string())?;
    store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: session_state_slug(&entry.token_hash),
            sensitivity: SensitivityPatch::SetSensitive,
            payload: StatePayload::Runtime(RuntimeEntry {
                value: val,
                merge_mode: MergeMode::LastWriter,
            }),
            at: stamp::now(),
            by: "fl-hub".to_string(),
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn load_session_entry(store: &RedbStore, token_hash: &str) -> Option<StoredSessionEntry> {
    let state_entry = store
        .get_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            &session_state_slug(token_hash),
        )
        .ok()??;
    let StatePayload::Runtime(rt) = state_entry.payload else {
        return None;
    };
    serde_json::from_value(rt.value).ok()
}

/// Extract `fl_hub_session` cookie value from a raw `Cookie` HTTP header.
pub fn extract_session_cookie(cookie_header: &str) -> Option<String> {
    for part in cookie_header.split(';') {
        let trimmed = part.trim();
        if let Some(val) = trimmed.strip_prefix("fl_hub_session=") {
            let token = val.trim().trim_matches('"');
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// Extract bearer token from an `Authorization: Bearer <token>` header.
pub fn extract_bearer_token(auth_header: &str) -> Option<String> {
    let trimmed = auth_header.trim();
    if trimmed.len() > 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        let tok = trimmed[7..].trim();
        if !tok.is_empty() {
            return Some(tok.to_string());
        }
    }
    None
}

/// Format the `Set-Cookie` header for an authenticated `fl_hub_session` (spec §2.2).
pub fn format_session_set_cookie(token: &str, ttl_secs: u64) -> String {
    format!("{SESSION_COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={ttl_secs}")
}

/// Format the `Set-Cookie` header clearing `fl_hub_session` on logout.
pub fn format_logout_set_cookie() -> String {
    format!("{SESSION_COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

/// Stateful authentication and session manager for `fl-hub` (spec §2.2).
#[derive(Debug, Clone)]
pub struct AuthManager {
    owner_login: String,
    owner_token: Option<String>,
    peer_token: Option<String>,
    trust_tailscale_headers: bool,
    session_ttl_secs: u64,
    cache: Arc<Mutex<AuthCache>>,
}

impl AuthManager {
    pub fn new(
        owner_login: impl Into<String>,
        owner_token: Option<String>,
        peer_token: Option<String>,
        trust_tailscale_headers: bool,
        session_ttl_secs: u64,
    ) -> Self {
        let login = owner_login.into();
        let owner_login = if login.trim().is_empty() {
            DEFAULT_OWNER_LOGIN.to_string()
        } else {
            login.trim().to_string()
        };
        let ttl = if session_ttl_secs == 0 {
            DEFAULT_SESSION_TTL_SECS
        } else {
            session_ttl_secs
        };
        Self {
            owner_login,
            owner_token: owner_token
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            peer_token: peer_token
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            trust_tailscale_headers,
            session_ttl_secs: ttl,
            cache: Arc::new(Mutex::new(AuthCache::default())),
        }
    }

    pub fn owner_login(&self) -> &str {
        &self.owner_login
    }

    pub fn session_ttl_secs(&self) -> u64 {
        self.session_ttl_secs
    }

    pub fn trust_tailscale_headers(&self) -> bool {
        self.trust_tailscale_headers
    }

    /// Mint a one-time pairing code and persist it in `store` (when provided) and in memory.
    pub fn mint_pairing_code(
        &self,
        store: Option<&RedbStore>,
        base_url: &str,
        ttl_secs: u64,
    ) -> Result<PairingInfo, String> {
        let code = generate_hex_token(16);
        let ttl = if ttl_secs == 0 {
            DEFAULT_PAIRING_TTL_SECS
        } else {
            ttl_secs
        };
        let expires_at_ms = now_millis().saturating_add(ttl.saturating_mul(1000));
        let entry = StoredPairingEntry {
            kind: "hub/pairing".to_string(),
            key: format!("hub/pairing/{code}"),
            code: code.clone(),
            expires_at_ms,
            used: false,
        };
        if let Some(s) = store {
            persist_pairing_entry(s, &entry)?;
        }
        if let Ok(mut guard) = self.cache.lock() {
            guard.pairings.insert(code.clone(), entry);
        }
        let pair_url = format!("{}/?pair={code}", base_url.trim_end_matches('/'));
        Ok(PairingInfo {
            code,
            expires_at_ms,
            ttl_secs: ttl,
            pair_url,
        })
    }

    /// Mint a 256-bit owner session token and persist its SHA-256 hash in `store` and memory.
    pub fn mint_session(&self, store: Option<&RedbStore>, login: &str) -> Result<String, String> {
        let session_token = generate_hex_token(32);
        let token_hash = sha256_hex(&session_token);
        let expires_at_ms = now_millis().saturating_add(self.session_ttl_secs.saturating_mul(1000));
        let entry = StoredSessionEntry {
            kind: "hub/session".to_string(),
            key: format!("hub/session/{token_hash}"),
            token_hash: token_hash.clone(),
            login: login.to_string(),
            expires_at_ms,
            revoked: false,
        };
        if let Some(s) = store {
            persist_session_entry(s, &entry)?;
        }
        if let Ok(mut guard) = self.cache.lock() {
            guard.sessions.insert(token_hash, entry);
        }
        Ok(session_token)
    }

    /// Exchange a one-time pairing `code` or owner `token` for a new session token (spec §2.2).
    pub fn exchange_pairing_or_token(
        &self,
        store: Option<&RedbStore>,
        code: Option<&str>,
        token: Option<&str>,
    ) -> Result<(String, HubAuthority, u64), String> {
        let now_ms = now_millis();

        if let Some(raw_code) = code.map(str::trim).filter(|s| !s.is_empty()) {
            let mut found = if let Ok(guard) = self.cache.lock() {
                guard.pairings.get(raw_code).cloned()
            } else {
                None
            };
            if found.is_none()
                && let Some(s) = store
            {
                found = load_pairing_entry(s, raw_code);
            }
            let Some(mut pairing) = found else {
                return Err("unknown pairing code".to_string());
            };
            if pairing.used {
                return Err("pairing code has already been used".to_string());
            }
            if now_ms > pairing.expires_at_ms {
                return Err("pairing code has expired".to_string());
            }
            if !constant_time_eq(&pairing.code, raw_code) {
                return Err("invalid pairing code".to_string());
            }

            pairing.used = true;
            if let Some(s) = store {
                persist_pairing_entry(s, &pairing)?;
            }
            if let Ok(mut guard) = self.cache.lock() {
                guard.pairings.insert(raw_code.to_string(), pairing);
            }

            let session_token = self.mint_session(store, &self.owner_login)?;
            let authority = HubAuthority::Owner {
                login: self.owner_login.clone(),
            };
            return Ok((session_token, authority, self.session_ttl_secs));
        }

        if let Some(raw_token) = token.map(str::trim).filter(|s| !s.is_empty()) {
            let Some(ref expected) = self.owner_token else {
                return Err("no owner token configured on this hub".to_string());
            };
            if !constant_time_eq(raw_token, expected) {
                return Err("invalid owner token".to_string());
            }
            let session_token = self.mint_session(store, &self.owner_login)?;
            let authority = HubAuthority::Owner {
                login: self.owner_login.clone(),
            };
            return Ok((session_token, authority, self.session_ttl_secs));
        }

        Err("either `code` or `token` must be provided".to_string())
    }

    /// Verify a session token against memory cache or `store`.
    pub fn verify_session_token(
        &self,
        store: Option<&RedbStore>,
        token: &str,
    ) -> Option<HubAuthority> {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            return None;
        }
        let token_hash = sha256_hex(trimmed);
        let now_ms = now_millis();

        let mut found = if let Ok(guard) = self.cache.lock() {
            guard.sessions.get(&token_hash).cloned()
        } else {
            None
        };
        if found.is_none()
            && let Some(s) = store
        {
            found = load_session_entry(s, &token_hash);
            if let Some(ref entry) = found
                && let Ok(mut guard) = self.cache.lock()
            {
                guard.sessions.insert(token_hash.clone(), entry.clone());
            }
        }

        let entry = found?;
        if entry.revoked || now_ms > entry.expires_at_ms {
            return None;
        }
        Some(HubAuthority::Owner { login: entry.login })
    }

    /// Revoke an active session token on logout (`POST /api/v1/auth/logout`).
    pub fn revoke_session_token(&self, store: Option<&RedbStore>, token: &str) {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            return;
        }
        let token_hash = sha256_hex(trimmed);
        let mut updated = None;
        if let Ok(mut guard) = self.cache.lock()
            && let Some(entry) = guard.sessions.get_mut(&token_hash)
        {
            entry.revoked = true;
            updated = Some(entry.clone());
        }
        if updated.is_none()
            && let Some(s) = store
            && let Some(mut entry) = load_session_entry(s, &token_hash)
        {
            entry.revoked = true;
            updated = Some(entry);
        }
        if let (Some(s), Some(entry)) = (store, updated) {
            let _ = persist_session_entry(s, &entry);
        }
    }

    /// Authenticate an HTTP or WebSocket request following the 5-step priority order in Spec §2.2:
    /// 1. `Cookie: fl_hub_session=<token>` or `Authorization: Bearer <session_token>` -> `HubAuthority::Owner`
    /// 2. `Authorization: Bearer <owner_token>` (constant-time comparison) -> `HubAuthority::Owner`
    /// 3. `Tailscale-User-Login` header when `trust_tailscale_headers == true` -> `HubAuthority::Owner { login }`
    /// 4. `Authorization: Bearer <peer_token>` (constant-time comparison) -> `HubAuthority::Peer { agent }`
    /// 5. Otherwise `None` (`401 Unauthorized`).
    pub fn authenticate_request(
        &self,
        store: Option<&RedbStore>,
        headers: &BTreeMap<String, String>,
        query: &BTreeMap<String, String>,
    ) -> Option<HubAuthority> {
        let cookie_token = headers
            .get("cookie")
            .and_then(|c| extract_session_cookie(c));
        let bearer_token = headers
            .get("authorization")
            .and_then(|a| extract_bearer_token(a))
            .or_else(|| {
                query
                    .get("token")
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
            });

        // 1. Active session cookie or session bearer token -> Owner.
        if let Some(ref c_tok) = cookie_token
            && let Some(auth) = self.verify_session_token(store, c_tok)
        {
            return Some(auth);
        }
        if let Some(ref b_tok) = bearer_token
            && let Some(auth) = self.verify_session_token(store, b_tok)
        {
            return Some(auth);
        }

        // 2. Configured owner token (constant-time equality) -> Owner.
        if let (Some(b_tok), Some(owner_tok)) = (&bearer_token, &self.owner_token)
            && constant_time_eq(b_tok, owner_tok)
        {
            return Some(HubAuthority::Owner {
                login: self.owner_login.clone(),
            });
        }

        // 3. Tailscale-User-Login header when trust_tailscale_headers == true -> Owner { login }.
        if self.trust_tailscale_headers
            && let Some(ts_login) = headers
                .get("tailscale-user-login")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
        {
            return Some(HubAuthority::Owner {
                login: ts_login.to_string(),
            });
        }

        // 4. Configured FerroWire peer token (constant-time equality) -> Peer { agent }.
        if let (Some(b_tok), Some(peer_tok)) = (&bearer_token, &self.peer_token)
            && constant_time_eq(b_tok, peer_tok)
        {
            let agent = headers
                .get("x-fl-agent")
                .or_else(|| query.get("agent"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            return Some(HubAuthority::Peer { agent });
        }

        // 5. Unauthorized.
        let _ = json!({});
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn constant_time_eq_compares_tokens_without_short_circuit_and_rejects_empty() {
        assert!(constant_time_eq("secret-token-123", "secret-token-123"));
        assert!(!constant_time_eq("secret-token-123", "secret-token-124"));
        assert!(!constant_time_eq("secret-token-123", "short"));
        assert!(!constant_time_eq("", ""));
        assert!(!constant_time_eq("non-empty", ""));
    }

    #[test]
    fn pairing_code_stored_in_redb_is_single_use_and_mints_owner_session() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("store.redb");
        let store = RedbStore::open(&store_path).unwrap();

        // Simulate `fl hub pair` minting a code directly into the workspace store.
        let info = mint_pairing_code_in_store(&store, "http://127.0.0.1:8420", 600).unwrap();
        assert!(info.pair_url.contains(&info.code));

        // A separate AuthManager (as in a running `fl hub serve` daemon) verifies the code from `store`.
        let auth = AuthManager::new(
            "lmcfarlin",
            Some("owner-secret".into()),
            Some("peer-secret".into()),
            false,
            3600,
        );
        let (session_token, authority, ttl) = auth
            .exchange_pairing_or_token(Some(&store), Some(&info.code), None)
            .unwrap();
        assert_eq!(
            authority,
            HubAuthority::Owner {
                login: "lmcfarlin".into()
            }
        );
        assert_eq!(ttl, 3600);
        assert_eq!(session_token.len(), 64);

        // Reusing the same one-time pairing code is refused!
        let err = auth
            .exchange_pairing_or_token(Some(&store), Some(&info.code), None)
            .unwrap_err();
        assert!(err.contains("already been used"), "unexpected error: {err}");

        // Session cookie authenticates as Owner.
        let mut headers = BTreeMap::new();
        headers.insert(
            "cookie".into(),
            format!("other=1; fl_hub_session={session_token}"),
        );
        let resolved = auth
            .authenticate_request(Some(&store), &headers, &BTreeMap::new())
            .unwrap();
        assert_eq!(
            resolved,
            HubAuthority::Owner {
                login: "lmcfarlin".into()
            }
        );

        // Revoking the session invalidates future requests.
        auth.revoke_session_token(Some(&store), &session_token);
        assert!(
            auth.authenticate_request(Some(&store), &headers, &BTreeMap::new())
                .is_none()
        );
    }

    #[test]
    fn request_authentication_order_respects_owner_tailscale_and_peer_tiers() {
        let auth = AuthManager::new(
            "lmcfarlin",
            Some("owner-tok-1".into()),
            Some("peer-tok-2".into()),
            true,
            3600,
        );

        // Owner bearer token
        let mut h_owner = BTreeMap::new();
        h_owner.insert("authorization".into(), "Bearer owner-tok-1".into());
        assert_eq!(
            auth.authenticate_request(None, &h_owner, &BTreeMap::new()),
            Some(HubAuthority::Owner {
                login: "lmcfarlin".into()
            })
        );

        // Tailscale header when trusted
        let mut h_ts = BTreeMap::new();
        h_ts.insert("tailscale-user-login".into(), "lmcfarlin@github".into());
        assert_eq!(
            auth.authenticate_request(None, &h_ts, &BTreeMap::new()),
            Some(HubAuthority::Owner {
                login: "lmcfarlin@github".into()
            })
        );

        // Peer bearer token with X-Fl-Agent header
        let mut h_peer = BTreeMap::new();
        h_peer.insert("authorization".into(), "Bearer peer-tok-2".into());
        h_peer.insert("x-fl-agent".into(), "ferris".into());
        assert_eq!(
            auth.authenticate_request(None, &h_peer, &BTreeMap::new()),
            Some(HubAuthority::Peer {
                agent: Some("ferris".into())
            })
        );

        // Unknown token -> None
        let mut h_bad = BTreeMap::new();
        h_bad.insert("authorization".into(), "Bearer wrong".into());
        assert_eq!(
            auth.authenticate_request(None, &h_bad, &BTreeMap::new()),
            None
        );
    }
}
