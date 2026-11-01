//! Multi-operator identity (Record 11): operator accounts with a password, TOTP and WebAuthn
//! passkeys, persisted at `<home>/.config/fl/operators.json` (mode 0600).
//!
//! # What a role is worth today
//!
//! A hub session is always `HubAuthority::Owner`: `AuthManager::verify_session_token` has no
//! other answer. So a login may mint a session only for an operator whose role is `owner`.
//! `admin`, `operator` and `viewer` are stored, listed and validated, but they grant no authority
//! yet: a correct credential for one of them is refused with 403 and mints nothing, until RBAC
//! enforcement exists. There is no `HubAuthority` variant for them on purpose.
//!
//! # Secrets
//!
//! Password hashes, salts, TOTP secrets and passkey public keys never leave the hub. Every
//! response and broadcast is built by [`operator_view`], an explicit whitelist. The only place
//! a TOTP secret is ever returned is the `totp/enroll` response, once.
//!
//! # Failure handling
//!
//! An unreadable or corrupt `operators.json` is never re-seeded: that would drop real accounts.
//! Every route that needs the store answers 500 and leaves the file untouched.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};
use ring::{agreement, hmac, pbkdf2, signature};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use fl_core::pr_policy::DEFAULT_OWNER_LOGIN;
use fl_store::AnyStore;

use crate::HubState;
use crate::api::{HttpRequest, HttpResponse, hub_home_dir, percent_decode};
use crate::auth::{HubAuthority, format_session_set_cookie, hex_encode};

// --- limits ---------------------------------------------------------------------------------

const MIN_USERNAME_LEN: usize = 2;
const MAX_USERNAME_LEN: usize = 64;
const MAX_DISPLAY_NAME_CHARS: usize = 100;
const MIN_PASSWORD_CHARS: usize = 8;
const MAX_PASSWORD_CHARS: usize = 256;
const MAX_TOTP_CODE_LEN: usize = 16;
/// Stands in for a passkey-login `totp_code` that is not a string or is oversized. Never six
/// digits, so it is always a wrong code.
const UNUSABLE_TOTP_CODE: &str = "?";
const MIN_CREDENTIAL_ID_LEN: usize = 16;
const MAX_CREDENTIAL_ID_LEN: usize = 1024;
const MAX_PASSKEYS_PER_OPERATOR: usize = 10;
/// Most operators the file holds. With [`MAX_PASSKEYS_TOTAL`] it keeps the file far below
/// [`MAX_FILE_BYTES`], so a file the hub wrote is never one it refuses to read.
const MAX_OPERATORS: usize = 256;
/// Most passkeys the file holds across all operators.
const MAX_PASSKEYS_TOTAL: usize = 1024;
/// Longest base64 field of a passkey assertion the login accepts.
const MAX_ASSERTION_FIELD_LEN: usize = 8192;
/// Most operator ids a path segment may name.
const MAX_ID_SEGMENT_LEN: usize = 64;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const FILE_VERSION: u32 = 1;

/// Iteration count for new password hashes. The strong value is the production path; only a
/// test build lowers it, so the suite does not spend minutes in PBKDF2.
const PRODUCTION_PBKDF2_ITERATIONS: u32 = 600_000;
const _: () = assert!(PRODUCTION_PBKDF2_ITERATIONS >= 600_000);
const PBKDF2_ITERATIONS: u32 = if cfg!(test) {
    1_000
} else {
    PRODUCTION_PBKDF2_ITERATIONS
};
/// A stored iteration count above this is a damaged file, not a slow hash.
const MAX_STORED_PBKDF2_ITERATIONS: u32 = 10_000_000;
/// A stored iteration count below this is a downgraded or tampered hash and never verifies. It
/// is the production cost; a test build keeps the cheap test constant as its floor.
const MIN_STORED_PBKDF2_ITERATIONS: u32 = PBKDF2_ITERATIONS;
const SALT_LEN: usize = 16;
const HASH_LEN: usize = 32;
static PBKDF2_ALG: pbkdf2::Algorithm = pbkdf2::PBKDF2_HMAC_SHA256;

const TOTP_SECRET_BYTES: usize = 20;
/// RFC 4226 section 4 (R6): a shared secret is at least 128 bits. A stored secret below this
/// (an empty one above all: HMAC accepts an empty key, so its codes are computable by anyone)
/// never verifies.
const MIN_TOTP_SECRET_BYTES: usize = 16;
const TOTP_STEP_SECS: u64 = 30;
const TOTP_DIGITS: u32 = 6;
const TOTP_ISSUER: &str = "FerroLoop";

const CHALLENGE_BYTES: usize = 32;
const CHALLENGE_TTL: Duration = Duration::from_secs(300);
const MAX_LIVE_CHALLENGES: usize = 256;
/// Challenge requests the whole hub serves in one minute. Under the 256 cap at the 5 minute TTL
/// (30 x 5 = 150), so a flood cannot evict a live challenge.
const MAX_CHALLENGES_PER_MINUTE: usize = 30;

const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(300);
const LOCKOUT: Duration = Duration::from_secs(60);
const MAX_TRACKED_USERS: usize = 1024;
const MAX_GLOBAL_FAILURES_PER_MINUTE: usize = 100;
/// Credential verifications (PBKDF2, ECDSA) that may run at once, hub wide. A request that finds
/// them all busy is answered 429 and charged nothing.
const MAX_INFLIGHT_VERIFICATIONS: usize = 4;
const MINUTE: Duration = Duration::from_secs(60);

pub(crate) const NON_OWNER_LOGIN_MESSAGE: &str =
    "this role cannot log in until RBAC enforcement exists";

// --- roles and stored shape -----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    Owner,
    Admin,
    Operator,
    Viewer,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Operator => "operator",
            Role::Viewer => "viewer",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "owner" => Some(Role::Owner),
            "admin" => Some(Role::Admin),
            "operator" => Some(Role::Operator),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredPasskey {
    credential_id: String,
    label: String,
    /// The 65-byte uncompressed SEC1 P-256 point, base64url without padding.
    public_key_b64: String,
    created_at_ms: u64,
    /// Last signature counter seen; 0 for an authenticator that keeps none.
    #[serde(default)]
    sign_count: u32,
    /// The one origin this credential may log in from (`https://host[:port]`, or `http://` for a
    /// loopback host), pinned at registration. A credential saved before origins were pinned has
    /// none and is refused at login: its origin was never established, so none is guessed.
    #[serde(default)]
    origin: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredOperator {
    id: String,
    username: String,
    display_name: String,
    role: Role,
    password_hash_hex: Option<String>,
    password_salt_hex: Option<String>,
    password_iterations: Option<u32>,
    totp_secret_base32: Option<String>,
    totp_pending_secret_base32: Option<String>,
    #[serde(default)]
    totp_enabled: bool,
    /// The newest TOTP step a login or verification accepted. A code is accepted only for a
    /// strictly newer step, so a code works once.
    last_totp_step: Option<u64>,
    #[serde(default)]
    passkeys: Vec<StoredPasskey>,
    created_at_ms: u64,
}

// A derived `Debug` would print the hash and the TOTP secrets.
impl std::fmt::Debug for StoredOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredOperator")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl StoredOperator {
    fn has_password(&self) -> bool {
        self.password_hash_hex.is_some()
            && self.password_salt_hex.is_some()
            && self.password_iterations.is_some()
    }
}

#[derive(Serialize, Deserialize)]
struct OperatorsFile {
    version: u32,
    operators: Vec<StoredOperator>,
}

impl OperatorsFile {
    /// A file that breaks these rules is treated as corrupt, not repaired: two accounts for one
    /// name or one credential would make a login ambiguous.
    fn is_consistent(&self) -> bool {
        if self.version != FILE_VERSION {
            return false;
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        let mut credentials = std::collections::BTreeSet::new();
        for op in &self.operators {
            if !ids.insert(op.id.clone()) || !names.insert(op.username.to_ascii_lowercase()) {
                return false;
            }
            for key in &op.passkeys {
                if !credentials.insert(key.credential_id.clone()) {
                    return false;
                }
            }
        }
        true
    }

    fn owner_count(&self) -> usize {
        self.operators
            .iter()
            .filter(|op| op.role == Role::Owner)
            .count()
    }

    fn find_username(&self, username: &str) -> Option<usize> {
        self.operators
            .iter()
            .position(|op| op.username.eq_ignore_ascii_case(username.trim()))
    }

    fn find_id_or_username(&self, raw: &str) -> Option<usize> {
        self.operators
            .iter()
            .position(|op| op.id == raw)
            .or_else(|| self.find_username(raw))
    }

    fn find_credential(&self, credential_id: &str) -> Option<(usize, usize)> {
        self.operators.iter().enumerate().find_map(|(i, op)| {
            op.passkeys
                .iter()
                .position(|k| k.credential_id == credential_id)
                .map(|j| (i, j))
        })
    }
}

// --- errors ---------------------------------------------------------------------------------

/// Why an operator operation did not apply. No variant carries a secret or a path.
#[derive(Debug)]
pub(crate) enum OpError {
    /// `operators.json` exists but cannot be trusted. Never re-seeded.
    Unreadable,
    Io,
    /// An exclusive create found the file already there. Internal: [`locked`] turns it into a
    /// re-read, so it never reaches a response.
    Exists,
    Rng,
    BadRequest(String),
    NotFound(String),
    Conflict(String),
}

fn error_json(status: u16, message: &str) -> HttpResponse {
    HttpResponse::json(status, json!({ "ok": false, "error": message }))
}

fn invalid_credentials() -> HttpResponse {
    error_json(401, "invalid credentials")
}

fn throttled(retry_after_secs: u64) -> HttpResponse {
    error_json(429, "too many attempts; try again later")
        .with_header("Retry-After", retry_after_secs.to_string())
}

/// Every verification slot is taken. Nothing was charged and nothing was checked.
fn busy() -> HttpResponse {
    throttled(1)
}

fn op_error_response(err: OpError) -> HttpResponse {
    match err {
        OpError::Unreadable => error_json(
            500,
            "the operator store is unreadable; it was left untouched",
        ),
        OpError::Io | OpError::Exists => error_json(500, "could not save the operator store"),
        OpError::Rng => error_json(500, "no secure randomness available"),
        OpError::BadRequest(msg) => error_json(400, &msg),
        OpError::NotFound(msg) => error_json(404, &msg),
        OpError::Conflict(msg) => error_json(409, &msg),
    }
}

// --- randomness, hex, time ------------------------------------------------------------------

/// Fails closed: unlike `generate_hex_token` there is no weaker fallback for a secret.
fn random_array<const N: usize>() -> Result<[u8; N], OpError> {
    let mut out = [0u8; N];
    SystemRandom::new()
        .fill(&mut out)
        .map_err(|_| OpError::Rng)?;
    Ok(out)
}

/// Equality with no early exit on content; a length difference is not a secret here.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex_decode(raw: &str) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) || !raw.is_ascii() {
        return None;
    }
    raw.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            u8::try_from(hi * 16 + lo).ok()
        })
        .collect()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn now_unix_secs() -> u64 {
    // A test build can hold the clock still: a TOTP code is valid for one 30 s step, and a
    // test that crosses a step boundary mid-way sees its notion of "now" change.
    #[cfg(test)]
    if let Some(secs) = TEST_CLOCK.with(std::cell::Cell::get) {
        return secs;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_operator_id() -> Result<String, OpError> {
    Ok(format!("op-{}", hex_encode(&random_array::<8>()?)))
}

/// Base64 or base64url, padded or not. The alphabet picks the engine, so a string that mixes
/// the two alphabets is refused.
fn decode_b64_flexible(raw: &str) -> Option<Vec<u8>> {
    let trimmed = raw.trim().trim_end_matches('=');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains(['+', '/']) {
        STANDARD_NO_PAD.decode(trimmed).ok()
    } else {
        URL_SAFE_NO_PAD.decode(trimmed).ok()
    }
}

// --- passwords ------------------------------------------------------------------------------

/// `(hash_hex, salt_hex, iterations)` with a fresh random salt.
fn hash_password(password: &str) -> Result<(String, String, u32), OpError> {
    let salt = random_array::<SALT_LEN>()?;
    let iterations = NonZeroU32::new(PBKDF2_ITERATIONS).ok_or(OpError::Rng)?;
    let mut out = [0u8; HASH_LEN];
    pbkdf2::derive(PBKDF2_ALG, iterations, &salt, password.as_bytes(), &mut out);
    Ok((hex_encode(&out), hex_encode(&salt), iterations.get()))
}

/// Constant-time check against the account's own salt and iteration count.
fn verify_password(op: &StoredOperator, password: &str) -> bool {
    let (Some(hash), Some(salt), Some(iterations)) = (
        op.password_hash_hex.as_deref(),
        op.password_salt_hex.as_deref(),
        op.password_iterations,
    ) else {
        return false;
    };
    let (Some(hash), Some(salt)) = (hex_decode(hash), hex_decode(salt)) else {
        return false;
    };
    if !(MIN_STORED_PBKDF2_ITERATIONS..=MAX_STORED_PBKDF2_ITERATIONS).contains(&iterations) {
        return false;
    }
    let Some(iterations) = NonZeroU32::new(iterations) else {
        return false;
    };
    pbkdf2::verify(PBKDF2_ALG, iterations, &salt, password.as_bytes(), &hash).is_ok()
}

/// The work of a password check for an account that has none, so an unknown name and a wrong
/// password cost the same.
fn dummy_password_check(password: &str) {
    const DUMMY_SALT: [u8; SALT_LEN] = *b"ferroloop-dummy!";
    const DUMMY_HASH: [u8; HASH_LEN] = [0x5a; HASH_LEN];
    if let Some(iterations) = NonZeroU32::new(PBKDF2_ITERATIONS) {
        let _ = pbkdf2::verify(
            PBKDF2_ALG,
            iterations,
            &DUMMY_SALT,
            password.as_bytes(),
            &DUMMY_HASH,
        );
    }
}

// --- TOTP (RFC 6238, HMAC-SHA1) -------------------------------------------------------------

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding.
fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for &byte in data {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(BASE32_ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// RFC 4648 base32, upper or lower case, trailing `=` tolerated. Anything else is refused.
fn base32_decode(raw: &str) -> Option<Vec<u8>> {
    let trimmed = raw.trim_end_matches('=');
    let mut out = Vec::with_capacity(trimmed.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for ch in trimmed.bytes() {
        let value = match ch.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | u32::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// RFC 4226 HOTP value for `counter`, truncated to `digits` digits.
fn hotp(secret: &[u8], counter: u64, digits: u32) -> u32 {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let tag = hmac::sign(&key, &counter.to_be_bytes());
    let mac = tag.as_ref();
    let offset = usize::from(mac[mac.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        mac[offset] & 0x7f,
        mac[offset + 1],
        mac[offset + 2],
        mac[offset + 3],
    ]);
    binary % 10u32.pow(digits)
}

/// The step whose code `code` is, within one step either side of `now_secs`, and strictly newer
/// than `last_step`. Every candidate is compared in constant time with no early exit.
fn verify_totp(secret: &[u8], code: &str, now_secs: u64, last_step: Option<u64>) -> Option<u64> {
    let code = code.trim();
    if secret.len() < MIN_TOTP_SECRET_BYTES
        || code.len() != TOTP_DIGITS as usize
        || !code.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let current = now_secs / TOTP_STEP_SECS;
    let mut accepted: Option<u64> = None;
    for step in [
        current.saturating_sub(1),
        current,
        current.saturating_add(1),
    ] {
        let expected = format!(
            "{:0width$}",
            hotp(secret, step, TOTP_DIGITS),
            width = TOTP_DIGITS as usize
        );
        let equal = ct_eq(expected.as_bytes(), code.as_bytes());
        let fresh = last_step.is_none_or(|last| step > last);
        if equal && fresh {
            accepted = Some(accepted.map_or(step, |best| best.max(step)));
        }
    }
    accepted
}

fn percent_encode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn otpauth_uri(username: &str, secret_base32: &str) -> String {
    format!(
        "otpauth://totp/{TOTP_ISSUER}:{}?secret={secret_base32}&issuer={TOTP_ISSUER}&algorithm=SHA1&digits={TOTP_DIGITS}&period={TOTP_STEP_SECS}",
        percent_encode_component(username)
    )
}

// --- login throttling -----------------------------------------------------------------------

#[derive(Debug)]
struct UserFailures {
    count: u32,
    window_start: Instant,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

/// Login-attempt accounting. The clock is an argument, so a test never sleeps.
///
/// An attempt is charged BEFORE its credential is checked: [`Throttle::reserve`] tests the limit
/// and counts the attempt in one critical section, so concurrent requests cannot all pass a
/// check that none of them has charged yet. A success clears the name; every other outcome
/// leaves the charge in place, so a failure needs no later bookkeeping.
///
/// After [`MAX_FAILURES`] charged attempts inside [`FAILURE_WINDOW`] a name is refused for
/// [`LOCKOUT`]. The count is kept when the lockout ends, so every further attempt locks it
/// again. Names are capped at [`MAX_TRACKED_USERS`]; a locked name is evicted last. A coarse
/// global limit bounds the attempt rate across all names.
#[derive(Debug, Default)]
pub(crate) struct Throttle {
    users: HashMap<String, UserFailures>,
    global: VecDeque<Instant>,
    challenge_hits: VecDeque<Instant>,
}

fn ceil_secs(d: Duration) -> u64 {
    d.as_millis().div_ceil(1000).max(1) as u64
}

fn prune(window: &mut VecDeque<Instant>, now: Instant) {
    while let Some(&oldest) = window.front() {
        if now.saturating_duration_since(oldest) >= MINUTE {
            window.pop_front();
        } else {
            break;
        }
    }
}

impl Throttle {
    /// Seconds to wait when `key` or the whole hub is over its limit.
    pub(crate) fn retry_after(&mut self, key: &str, now: Instant) -> Option<u64> {
        if let Some(wait) = self.global_retry_after(now) {
            return Some(wait);
        }
        let until = self.users.get(key)?.locked_until?;
        (until > now).then(|| ceil_secs(until - now))
    }

    pub(crate) fn global_retry_after(&mut self, now: Instant) -> Option<u64> {
        prune(&mut self.global, now);
        if self.global.len() >= MAX_GLOBAL_FAILURES_PER_MINUTE {
            let oldest = *self.global.front()?;
            return Some(ceil_secs((oldest + MINUTE).saturating_duration_since(now)));
        }
        None
    }

    /// Check the limits and charge one attempt to `key`, atomically. `Err(wait)` is the seconds
    /// to wait; nothing is charged then. Call it under the throttle mutex and BEFORE verifying
    /// the credential.
    pub(crate) fn reserve(&mut self, key: &str, now: Instant) -> Result<(), u64> {
        if let Some(wait) = self.retry_after(key, now) {
            return Err(wait);
        }
        self.charge(key, now);
        Ok(())
    }

    /// Count one attempt against `key` and the whole hub, whatever the limits say. Used directly
    /// only for a second step of a request that [`Throttle::reserve`] already admitted.
    pub(crate) fn charge(&mut self, key: &str, now: Instant) {
        self.global.push_back(now);
        while self.global.len() > MAX_GLOBAL_FAILURES_PER_MINUTE * 4 {
            self.global.pop_front();
        }
        let entry = self
            .users
            .entry(key.to_string())
            .or_insert_with(|| UserFailures {
                count: 0,
                window_start: now,
                locked_until: None,
                last_seen: now,
            });
        if now.saturating_duration_since(entry.window_start) >= FAILURE_WINDOW {
            entry.count = 0;
            entry.window_start = now;
            entry.locked_until = None;
        }
        entry.count = entry.count.saturating_add(1);
        entry.last_seen = now;
        if entry.count >= MAX_FAILURES {
            entry.locked_until = Some(now + LOCKOUT);
        }
        while self.users.len() > MAX_TRACKED_USERS {
            let victim = self
                .users
                .iter()
                .min_by_key(|(_, u)| (u.locked_until.is_some_and(|t| t > now), u.last_seen))
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    self.users.remove(&k);
                }
                None => break,
            }
        }
    }

    pub(crate) fn clear(&mut self, key: &str) {
        self.users.remove(key);
    }

    /// Seconds to wait when the hub has served its challenge quota this minute, else counts
    /// this request.
    fn challenge_retry_after(&mut self, now: Instant) -> Option<u64> {
        if let Some(wait) = self.global_retry_after(now) {
            return Some(wait);
        }
        prune(&mut self.challenge_hits, now);
        if self.challenge_hits.len() >= MAX_CHALLENGES_PER_MINUTE {
            let oldest = *self.challenge_hits.front()?;
            return Some(ceil_secs((oldest + MINUTE).saturating_duration_since(now)));
        }
        self.challenge_hits.push_back(now);
        None
    }

    #[cfg(test)]
    fn tracked_users(&self) -> usize {
        self.users.len()
    }
}

/// Single-use passkey challenges: 32 random bytes, five minute life, at most
/// [`MAX_LIVE_CHALLENGES`] at once (oldest dropped).
#[derive(Debug, Default)]
pub(crate) struct Challenges {
    live: HashMap<String, Instant>,
    order: VecDeque<String>,
}

impl Challenges {
    fn issue(&mut self, now: Instant) -> Result<String, OpError> {
        self.live.retain(|_, expires| *expires > now);
        self.order.retain(|c| self.live.contains_key(c));
        while self.live.len() >= MAX_LIVE_CHALLENGES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.live.remove(&oldest);
        }
        let challenge = URL_SAFE_NO_PAD.encode(random_array::<CHALLENGE_BYTES>()?);
        self.live.insert(challenge.clone(), now + CHALLENGE_TTL);
        self.order.push_back(challenge.clone());
        Ok(challenge)
    }

    /// Remove `challenge`; true only when it was issued, unused and not expired.
    fn consume(&mut self, challenge: &str, now: Instant) -> bool {
        match self.live.remove(challenge) {
            Some(expires) => expires > now,
            None => false,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.live.len()
    }
}

/// The in-memory half of operator login: throttling, passkey challenges and the cap on
/// credential verifications running at once.
#[derive(Default)]
pub(crate) struct LoginGuard {
    throttle: Mutex<Throttle>,
    challenges: Mutex<Challenges>,
    inflight: AtomicUsize,
    /// How many credential verifications started (test builds only).
    #[cfg(test)]
    verifications: AtomicUsize,
    /// Run at the start of every verification (test builds only).
    #[cfg(test)]
    verify_hook: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
}

/// One of the [`MAX_INFLIGHT_VERIFICATIONS`] slots. Dropping it, on any path including a panic,
/// frees the slot.
pub(crate) struct VerifyPermit<'a>(&'a AtomicUsize);

impl Drop for VerifyPermit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl std::fmt::Debug for LoginGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginGuard").finish_non_exhaustive()
    }
}

impl LoginGuard {
    fn throttle(&self) -> MutexGuard<'_, Throttle> {
        self.throttle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn challenges(&self) -> MutexGuard<'_, Challenges> {
        self.challenges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Take a verification slot, or `None` when all are busy. Never waits.
    fn try_verify_permit(&self) -> Option<VerifyPermit<'_>> {
        // A plain CAS loop: the std atomic update helper was renamed across
        // toolchains, so this stays warning-free on old and new Rust.
        let mut n = self.inflight.load(Ordering::Acquire);
        loop {
            if n >= MAX_INFLIGHT_VERIFICATIONS {
                return None;
            }
            match self
                .inflight
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(VerifyPermit(&self.inflight)),
                Err(observed) => n = observed,
            }
        }
    }

    /// Marks the start of one credential verification. A no-op outside test builds.
    #[cfg(not(test))]
    fn note_verification(&self) {}

    #[cfg(test)]
    fn note_verification(&self) {
        self.verifications.fetch_add(1, Ordering::SeqCst);
        let hook = self
            .verify_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }
}

// --- persistence ----------------------------------------------------------------------------

/// Serialises every read-modify-write of `operators.json`.
static OPERATORS_LOCK: Mutex<()> = Mutex::new(());

fn operators_path(home: &Path) -> PathBuf {
    home.join(".config/fl/operators.json")
}

/// The saved file, `None` when there is none. A symlink, an oversize file, bad JSON or an
/// inconsistent file is `Unreadable`.
fn read_file(home: &Path) -> Result<Option<OperatorsFile>, OpError> {
    let path = operators_path(home);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(OpError::Unreadable),
        Ok(meta) if !meta.is_file() || meta.len() > MAX_FILE_BYTES => {
            return Err(OpError::Unreadable);
        }
        Ok(_) => {}
    }
    let bytes = fs::read(&path).map_err(|_| OpError::Unreadable)?;
    let file: OperatorsFile = serde_json::from_slice(&bytes).map_err(|_| OpError::Unreadable)?;
    if !file.is_consistent() {
        return Err(OpError::Unreadable);
    }
    Ok(Some(file))
}

/// Write `file` through a 0600 temp file in the same directory. `exclusive` links it into place
/// so an existing file is never replaced (the seed); otherwise it renames over the old one.
fn write_file(home: &Path, file: &OperatorsFile, exclusive: bool) -> Result<(), OpError> {
    let path = operators_path(home);
    let dir = path.parent().ok_or(OpError::Io)?;
    fs::create_dir_all(dir).map_err(|_| OpError::Io)?;
    let bytes = serde_json::to_vec_pretty(file).map_err(|_| OpError::Io)?;
    // Never write a file `read_file` would refuse to read back.
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(OpError::Io);
    }
    let tmp = dir.join(format!(
        ".operators.json.tmp-{}-{}",
        std::process::id(),
        hex_encode(&random_array::<6>()?)
    ));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&bytes).and_then(|()| f.sync_all()));
    let placed = written.and_then(|()| {
        if !exclusive {
            return fs::rename(&tmp, &path);
        }
        match fs::hard_link(&tmp, &path) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
            // A filesystem without hard links: check, then rename.
            Err(_) if fs::symlink_metadata(&path).is_err() => fs::rename(&tmp, &path),
            other => other,
        }
    });
    let _ = fs::remove_file(&tmp);
    match placed {
        Ok(()) => Ok(()),
        Err(e) if exclusive && e.kind() == io::ErrorKind::AlreadyExists => Err(OpError::Exists),
        Err(_) => Err(OpError::Io),
    }
}

/// The first file: one owner, named `owner_login` trimmed. A blank name falls back to the
/// default owner login, so an empty username is never seeded.
fn seed_file(owner_login: &str) -> Result<OperatorsFile, OpError> {
    let owner_login = match owner_login.trim() {
        "" => DEFAULT_OWNER_LOGIN,
        name => name,
    };
    Ok(OperatorsFile {
        version: FILE_VERSION,
        operators: vec![StoredOperator {
            id: new_operator_id()?,
            username: owner_login.to_string(),
            display_name: owner_login.to_string(),
            role: Role::Owner,
            password_hash_hex: None,
            password_salt_hex: None,
            password_iterations: None,
            totp_secret_base32: None,
            totp_pending_secret_base32: None,
            totp_enabled: false,
            last_totp_step: None,
            passkeys: Vec::new(),
            created_at_ms: now_millis(),
        }],
    })
}

#[cfg(test)]
thread_local! {
    /// The Unix second `now_unix_secs` reports in this test's thread, when set (test builds
    /// only). Unset, the real clock.
    static TEST_CLOCK: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
type SeedRaceHook = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
thread_local! {
    /// Runs once, after a missing file was seen and before the seed is created (test builds
    /// only): the place another process would win the race.
    static SEED_RACE_HOOK: std::cell::RefCell<Option<SeedRaceHook>> =
        const { std::cell::RefCell::new(None) };
}

/// The file as it is now, seeding it first when it is missing. The seed is created without ever
/// replacing a file: when another process created one meanwhile, that file is read and used.
fn read_or_seed(home: &Path, owner_login: &str) -> Result<OperatorsFile, OpError> {
    if let Some(file) = read_file(home)? {
        return Ok(file);
    }
    #[cfg(test)]
    if let Some(hook) = SEED_RACE_HOOK.with(|h| h.borrow_mut().take()) {
        hook(home);
    }
    let seed = seed_file(owner_login)?;
    match write_file(home, &seed, true) {
        Ok(()) => Ok(seed),
        // Lost the race for the first file: the winner's file is the truth, not ours.
        Err(OpError::Exists) => read_file(home)?.ok_or(OpError::Io),
        Err(e) => Err(e),
    }
}

/// Run `f` on the operator file under the process-wide lock. A missing file is seeded with the
/// configured owner first (so it exists afterwards even when `f` fails). `f` returns its result
/// and whether it changed the file; an error writes nothing more.
fn locked<R>(
    home: &Path,
    owner_login: &str,
    f: impl FnOnce(&mut OperatorsFile) -> Result<(R, bool), OpError>,
) -> Result<R, OpError> {
    let _guard = OPERATORS_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let mut file = read_or_seed(home, owner_login)?;
    let (out, changed) = f(&mut file)?;
    if changed {
        write_file(home, &file, false)?;
    }
    Ok(out)
}

// --- views ----------------------------------------------------------------------------------

/// The only shape an operator leaves the hub in. An explicit whitelist: a field added to
/// [`StoredOperator`] is invisible until it is added here.
fn operator_view(op: &StoredOperator) -> Value {
    json!({
        "id": op.id,
        "username": op.username,
        "display_name": op.display_name,
        "role": op.role.as_str(),
        "has_password": op.has_password(),
        "totp_enabled": op.totp_enabled,
        "passkeys_count": op.passkeys.len(),
        "passkeys": op.passkeys.iter().map(|k| json!({
            "credential_id": k.credential_id,
            "label": k.label,
            "origin": k.origin,
            "created_at_ms": k.created_at_ms,
        })).collect::<Vec<_>>(),
        "created_at_ms": op.created_at_ms,
    })
}

// --- request helpers ------------------------------------------------------------------------

fn home_or_500(state: &HubState) -> Result<PathBuf, HttpResponse> {
    hub_home_dir(state).ok_or_else(|| error_json(500, "home directory unavailable"))
}

fn require_owner(authority: &HubAuthority, what: &str) -> Result<(), HttpResponse> {
    if authority.is_owner() {
        Ok(())
    } else {
        Err(error_json(403, &format!("only Owner authority may {what}")))
    }
}

fn authority_login(authority: &HubAuthority) -> &str {
    match authority {
        HubAuthority::Owner { login } => login,
        HubAuthority::Peer { .. } => "",
    }
}

/// The JSON object body. An empty body is an empty object.
fn object_body(req: &HttpRequest) -> Result<Map<String, Value>, HttpResponse> {
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(&req.body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(error_json(400, "body must be a JSON object")),
    }
}

/// A 400 for a body that does not fit `T`. The serde text is passed on only for a missing or
/// unknown field; a type error would echo the value, which may be a secret.
fn parse_body<T: serde::de::DeserializeOwned>(map: Map<String, Value>) -> Result<T, HttpResponse> {
    serde_json::from_value(Value::Object(map)).map_err(|e| {
        let text = e.to_string();
        if text.starts_with("unknown field") || text.starts_with("missing field") {
            error_json(400, &format!("invalid body: {text}"))
        } else {
            error_json(400, "invalid body: a field has the wrong type")
        }
    })
}

/// `op-` and 16 lowercase hex digits: the shape of an operator id.
fn looks_like_operator_id(raw: &str) -> bool {
    raw.strip_prefix("op-").is_some_and(|hex| {
        hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

fn valid_username(raw: &str) -> bool {
    // A username shaped like an id would make `DELETE /operators/<it>` ambiguous: ids are
    // matched before names.
    !looks_like_operator_id(raw)
        && (MIN_USERNAME_LEN..=MAX_USERNAME_LEN).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn valid_text(raw: &str, max_chars: usize) -> bool {
    raw.chars().count() <= max_chars && !raw.chars().any(char::is_control)
}

/// HMAC zero-pads a key shorter than its block, so PBKDF2 hashes `pw` and `pw\0` (and `pw\0\0`)
/// alike. A NUL is therefore refused in a password rather than silently ignored at its tail.
fn valid_password(raw: &str) -> bool {
    (MIN_PASSWORD_CHARS..=MAX_PASSWORD_CHARS).contains(&raw.chars().count()) && !raw.contains('\0')
}

fn valid_credential_id(raw: &str) -> bool {
    (MIN_CREDENTIAL_ID_LEN..=MAX_CREDENTIAL_ID_LEN).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A `host[:port]` as a browser writes it: lowercase, with its name apart from the port.
struct HostInfo {
    host: String,
    hostname: String,
    /// The port as written, if any.
    port: Option<String>,
}

/// `raw` as a plain `name[:port]` (or `[v6][:port]`), else `None`.
fn parse_host(raw: &str) -> Option<HostInfo> {
    let raw = raw.trim().to_ascii_lowercase();
    if raw.is_empty()
        || raw.len() > 255
        || !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
    {
        return None;
    }
    let (hostname, port) = if let Some(rest) = raw.strip_prefix('[') {
        let end = rest.find(']')?;
        let after = &rest[end + 1..];
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':')?.to_string())
        };
        (format!("[{}]", &rest[..end]), port)
    } else {
        match raw.split_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.to_string())),
            None => (raw.clone(), None),
        }
    };
    if hostname.is_empty() || hostname == "[]" {
        return None;
    }
    if let Some(port) = &port
        && (port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some(HostInfo {
        host: raw,
        hostname,
        port,
    })
}

impl HostInfo {
    fn is_loopback(&self) -> bool {
        matches!(self.hostname.as_str(), "localhost" | "127.0.0.1" | "[::1]")
    }
}

/// The origin a passkey is pinned to, and the relying-party id (its host name) that goes with it.
#[derive(Debug, PartialEq, Eq)]
struct PinnedOrigin {
    /// `scheme://host[:port]`, lowercase, no default port, no path: the form a browser puts in
    /// `clientData.origin`.
    origin: String,
    /// The host name without the port; the relying-party id whose SHA-256 the authenticator signs.
    rp_id: String,
}

/// `https://<host>[:port]` for any host, `http://<host>[:port]` only for a loopback host (the one
/// place browsers run WebAuthn without TLS). No path, query, userinfo or other scheme. A default
/// port is dropped, as a browser drops it.
fn parse_origin(raw: &str) -> Option<PinnedOrigin> {
    let raw = raw.trim().to_ascii_lowercase();
    let (scheme, rest) = match raw.strip_prefix("https://") {
        Some(rest) => ("https", rest),
        None => ("http", raw.strip_prefix("http://")?),
    };
    let host = parse_host(rest)?;
    if scheme == "http" && !host.is_loopback() {
        return None;
    }
    let default_port = if scheme == "https" { "443" } else { "80" };
    let origin = match host.port.as_deref() {
        Some(port) if port != default_port => format!("{scheme}://{}:{port}", host.hostname),
        _ => format!("{scheme}://{}", host.hostname),
    };
    Some(PinnedOrigin {
        origin,
        rp_id: host.hostname,
    })
}

/// The origin a registering request came from, by its `Host` header: `http` for a loopback host,
/// `https` otherwise. `None` when the header is absent or not a plain `name[:port]`.
fn origin_of_request(req: &HttpRequest) -> Option<PinnedOrigin> {
    let host = parse_host(req.headers.get("host")?)?;
    let scheme = if host.is_loopback() { "http" } else { "https" };
    parse_origin(&format!("{scheme}://{}", host.host))
}

// --- passkeys -------------------------------------------------------------------------------

/// A 65-byte uncompressed SEC1 P-256 point (`0x04 || X || Y`), as base64 or base64url. A COSE
/// key is not accepted. Returns the canonical base64url form.
fn normalize_public_key(raw: &str) -> Option<String> {
    let bytes = decode_b64_flexible(raw)?;
    (bytes.len() == 65 && bytes[0] == 0x04 && is_on_p256_curve(&bytes))
        .then(|| URL_SAFE_NO_PAD.encode(bytes))
}

/// True when `point` is a valid uncompressed P-256 point: both coordinates below the field prime
/// and the point on the curve. ring parses a peer key for ECDH with exactly that check (SEC 1
/// section 3.2.2.1), and exposes no other way to ask, so a throwaway agreement is the test. Without
/// it a malformed key registers and then fails every login, with nothing to say why.
fn is_on_p256_curve(point: &[u8]) -> bool {
    let rng = SystemRandom::new();
    let Ok(ephemeral) = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
    else {
        return false;
    };
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, point);
    agreement::agree_ephemeral(ephemeral, &peer, |_| ()).is_ok()
}

#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    kind: Option<String>,
    challenge: Option<String>,
    origin: Option<String>,
    #[serde(rename = "crossOrigin")]
    cross_origin: Option<bool>,
}

fn parse_client_data(bytes: &[u8]) -> Option<ClientData> {
    serde_json::from_slice(bytes).ok()
}

struct Assertion {
    client_data: Vec<u8>,
    authenticator_data: Vec<u8>,
    signature: Vec<u8>,
}

/// Everything about an assertion except its challenge, which the caller has already consumed:
/// the ceremony type, origin, relying party hash, user presence, the signature over
/// `authenticatorData || SHA256(clientDataJSON)`, and the counter. `Some(new_count)` only when
/// all of them hold.
///
/// The origin and the relying party come from `pinned`, the origin stored with the credential,
/// never from the login request: a request's `Host` header is the caller's to choose, so
/// checking an assertion against it would check the assertion against itself. `clientData.origin`
/// must equal the pinned origin exactly, port included.
///
/// The counter is stricter than WebAuthn asks: once the stored count is nonzero, a new count
/// must be greater, so a drop back to 0 is refused too.
fn check_assertion(
    assertion: &Assertion,
    client_data: &ClientData,
    pinned: &PinnedOrigin,
    public_key: &[u8],
    stored_count: u32,
) -> Option<u32> {
    if client_data.kind.as_deref() != Some("webauthn.get") {
        return None;
    }
    if client_data.cross_origin == Some(true) {
        return None;
    }
    if client_data.origin.as_deref()? != pinned.origin {
        return None;
    }
    let auth = &assertion.authenticator_data;
    if auth.len() < 37 {
        return None;
    }
    let rp_id_hash = Sha256::digest(pinned.rp_id.as_bytes());
    if !ct_eq(&auth[..32], rp_id_hash.as_slice()) {
        return None;
    }
    if auth[32] & 0x01 == 0 {
        return None;
    }
    let mut signed = auth.clone();
    signed.extend_from_slice(Sha256::digest(&assertion.client_data).as_slice());
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, public_key)
        .verify(&signed, &assertion.signature)
        .ok()?;
    let new_count = u32::from_be_bytes([auth[33], auth[34], auth[35], auth[36]]);
    if stored_count != 0 && new_count <= stored_count {
        return None;
    }
    Some(new_count)
}

// --- login ----------------------------------------------------------------------------------

/// How a verified login ended, decided under the operators lock.
enum LoginResult {
    Minted {
        token: String,
        username: String,
    },
    /// The account is not an owner (any more): it gets no session.
    NotOwner,
    /// The account was deleted, or its password changed, since the credential was checked.
    Gone,
    MintFailed,
}

/// Mint the session for operator `id`, from inside [`locked`]: the existence and the role are
/// re-read from the very `file` the lock protects, so a delete or demotion that landed while the
/// credential was being verified wins, and one that lands next revokes this session (the revoke
/// runs after the lock is released). A hub session is always Owner tier, so only an `owner`
/// account may be given one. `password_hash`, when given, is the hash the password was verified
/// against: a different one means the password was changed meanwhile.
fn mint_under_lock(
    state: &HubState,
    store: Option<&AnyStore>,
    file: &OperatorsFile,
    id: &str,
    password_hash: Option<&str>,
) -> LoginResult {
    let Some(op) = file.operators.iter().find(|o| o.id == id) else {
        return LoginResult::Gone;
    };
    if password_hash.is_some_and(|hash| op.password_hash_hex.as_deref() != Some(hash)) {
        return LoginResult::Gone;
    }
    if op.role != Role::Owner {
        return LoginResult::NotOwner;
    }
    match state.auth.mint_session(store, &op.username) {
        Ok(token) => LoginResult::Minted {
            token,
            username: op.username.clone(),
        },
        Err(_) => LoginResult::MintFailed,
    }
}

/// The response for a [`LoginResult`], and whether the credential was good enough to clear the
/// name's charged attempts (it was, unless the account vanished or the mint failed).
fn login_response(result: LoginResult, state: &HubState) -> (HttpResponse, bool) {
    match result {
        LoginResult::Minted { token, username } => (
            HttpResponse::json(
                200,
                json!({
                    "ok": true,
                    "authority": { "tier": "owner", "login": username, "role": "owner" },
                    "session_token": token,
                }),
            )
            .with_header(
                "Set-Cookie",
                format_session_set_cookie(&token, state.auth.session_ttl_secs()),
            ),
            true,
        ),
        LoginResult::NotOwner => (error_json(403, NON_OWNER_LOGIN_MESSAGE), true),
        LoginResult::Gone => (invalid_credentials(), false),
        LoginResult::MintFailed => (error_json(500, "could not start a session"), false),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenLogin {
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordLogin {
    username: String,
    password: String,
    totp_code: Option<String>,
}

/// `POST /api/v1/auth/login` (public). Modes: `token`, `password`, `passkey`.
pub(crate) fn handle_post_login(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let mut body = match object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let Some(Value::String(mode)) = body.remove("mode") else {
        return error_json(400, "`mode` must be token, password or passkey");
    };
    match mode.as_str() {
        "token" => match parse_body::<TokenLogin>(body) {
            Ok(login) => login_token(state, &login.token),
            Err(resp) => resp,
        },
        "password" => match parse_body::<PasswordLogin>(body) {
            Ok(login) => login_password(state, login),
            Err(resp) => resp,
        },
        "passkey" => login_passkey(state, &body),
        _ => error_json(400, "`mode` must be token, password or passkey"),
    }
}

/// The owner token exchange. Not throttled, like `/auth/pair`: the token is a long random
/// secret, and this is the recovery path when an account is locked out.
fn login_token(state: &HubState, token: &str) -> HttpResponse {
    let store = state.open_store().ok();
    match state
        .auth
        .exchange_pairing_or_token(store.as_deref(), None, Some(token))
    {
        Ok((session_token, authority, ttl_secs)) => {
            let login = authority_login(&authority).to_string();
            HttpResponse::json(
                200,
                json!({
                    "ok": true,
                    "authenticated": true,
                    "authority": { "tier": "owner", "login": login, "role": "owner" },
                    "session_token": session_token,
                }),
            )
            .with_header(
                "Set-Cookie",
                format_session_set_cookie(&session_token, ttl_secs),
            )
        }
        Err(_) => invalid_credentials(),
    }
}

/// The TOTP check every login path shares: the live secret, one step either side of now, a
/// constant-time compare, and a step strictly newer than the last one this operator spent.
/// Returns the step to record as `last_totp_step`; the caller records it under the operators
/// lock, in the same critical section as the check.
fn verify_login_totp(op: &StoredOperator, code: &str) -> Option<u64> {
    let secret = op.totp_secret_base32.as_deref().and_then(base32_decode)?;
    verify_totp(&secret, code, now_unix_secs(), op.last_totp_step)
}

/// The throttle key shared by every login name that is not a valid username. It holds `:`, which
/// `valid_username` refuses, so no real operator can have a login that lands in this bucket.
const INVALID_USERNAME_THROTTLE_KEY: &str = "login-invalid:name";

fn login_password(state: &HubState, login: PasswordLogin) -> HttpResponse {
    let username = login.username.trim();
    if username.len() > MAX_USERNAME_LEN
        || login.password.chars().count() > MAX_PASSWORD_CHARS
        || login
            .totp_code
            .as_ref()
            .is_some_and(|c| c.len() > MAX_TOTP_CODE_LEN)
    {
        return error_json(400, "a login field is too long");
    }
    if login.password.contains('\0') {
        // No password is stored with a NUL (see `valid_password`), and one that ends in NULs would
        // otherwise verify as the password without them.
        return invalid_credentials();
    }
    let name = username.to_ascii_lowercase();
    // The throttle shares one map between login names and the hub's own keys (`passkey:<id>`,
    // `totp-verify:<name>`). A real username never holds `:` or a control character, so a name
    // that does is charged to one shared bucket instead of the internal key it spells: otherwise
    // an anonymous caller could lock a victim's TOTP enrolment by "logging in" as
    // `totp-verify:<victim>`. The bucket's own key holds a `:`, so no valid username spells it.
    let key = if name.contains(':') || name.chars().any(char::is_control) {
        INVALID_USERNAME_THROTTLE_KEY.to_string()
    } else {
        name.clone()
    };
    let home = match home_or_500(state) {
        Ok(home) => home,
        Err(resp) => return resp,
    };
    // Bound the verifications running at once, then charge this attempt to the name BEFORE any
    // password is checked: the check and the charge are one critical section, so a burst of
    // concurrent requests for one name can have at most five of them evaluated.
    let Some(_permit) = state.login_guard.try_verify_permit() else {
        return busy();
    };
    if let Err(wait) = state.login_guard.throttle().reserve(&key, Instant::now()) {
        return throttled(wait);
    }
    let owner_login = state.auth.owner_login().to_string();
    let found = locked(&home, &owner_login, |file| {
        Ok((
            file.find_username(&name).map(|i| file.operators[i].clone()),
            false,
        ))
    });
    let op = match found {
        Ok(op) => op,
        Err(e) => return op_error_response(e),
    };
    // An unknown name, an account with no password and a wrong password all do one PBKDF2 and
    // answer alike.
    state.login_guard.note_verification();
    let op = match op {
        Some(op) if op.has_password() => {
            if !verify_password(&op, &login.password) {
                return invalid_credentials();
            }
            op
        }
        _ => {
            dummy_password_check(&login.password);
            return invalid_credentials();
        }
    };
    let store = state.open_store().ok();
    let result = if op.totp_enabled {
        // Only a verified password reaches here, so this reveals nothing to a guesser.
        let Some(code) = login.totp_code.as_deref().filter(|c| !c.trim().is_empty()) else {
            return HttpResponse::json(200, json!({ "ok": false, "requires_totp": true }));
        };
        // The code is a second guess inside a request that was already admitted: charge it
        // before it is checked, but never turn the request away halfway, which would tell a
        // guesser the password was right.
        state.login_guard.throttle().charge(&key, Instant::now());
        let id = op.id.clone();
        let hash = op.password_hash_hex.clone();
        let verified = locked(&home, &owner_login, |file| {
            let Some(i) = file.operators.iter().position(|o| o.id == id) else {
                return Ok((None, false));
            };
            let target = &mut file.operators[i];
            let Some(step) = verify_login_totp(target, code) else {
                return Ok((None, false));
            };
            target.last_totp_step = Some(step);
            Ok((
                Some(mint_under_lock(
                    state,
                    store.as_deref(),
                    file,
                    &id,
                    hash.as_deref(),
                )),
                true,
            ))
        });
        match verified {
            Ok(Some(result)) => result,
            Ok(None) => return invalid_credentials(),
            Err(e) => return op_error_response(e),
        }
    } else {
        let id = op.id.clone();
        let hash = op.password_hash_hex.clone();
        // The account may have been demoted or deleted while the password was hashed.
        let minted = locked(&home, &owner_login, |file| {
            Ok((
                mint_under_lock(state, store.as_deref(), file, &id, hash.as_deref()),
                false,
            ))
        });
        match minted {
            Ok(result) => result,
            Err(e) => return op_error_response(e),
        }
    };
    let (resp, verified) = login_response(result, state);
    if verified {
        state.login_guard.throttle().clear(&key);
    }
    resp
}

/// A string field of a passkey login body, or `None` if absent, not a string or oversized.
fn assertion_field<'a>(body: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    body.get(name)
        .and_then(Value::as_str)
        .filter(|s| s.len() <= MAX_ASSERTION_FIELD_LEN)
}

/// WebAuthn-style assertion login. Every failure, a missing field included, is the same 401.
fn login_passkey(state: &HubState, body: &Map<String, Value>) -> HttpResponse {
    let home = match home_or_500(state) {
        Ok(home) => home,
        Err(resp) => return resp,
    };
    let Some(_permit) = state.login_guard.try_verify_permit() else {
        return busy();
    };
    let now = Instant::now();
    // This route is public and reads the operator file: a request the hub-wide limit will turn
    // away must not cost a file read, so the limit is tested first.
    if let Some(wait) = state.login_guard.throttle().global_retry_after(now) {
        return throttled(wait);
    }
    let owner_login = state.auth.owner_login().to_string();
    // The optional second factor. An absent, null, empty or all-blank code is "no code". A code
    // that is not a string or is oversized is a wrong code, not a missing one: it is mapped to
    // a value `verify_totp` always refuses, so it is charged and refused like any other guess.
    let totp_code: Option<&str> = match body.get("totp_code") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .filter(|c| c.len() <= MAX_TOTP_CODE_LEN)
                .unwrap_or(UNUSABLE_TOTP_CODE),
        ),
    }
    .filter(|c| !c.trim().is_empty());
    let credential_id = assertion_field(body, "credential_id")
        .filter(|c| c.len() <= MAX_CREDENTIAL_ID_LEN)
        .unwrap_or("");
    let known = locked(&home, &owner_login, |file| {
        let found = file.find_credential(credential_id).map(|(i, j)| {
            let op = &file.operators[i];
            (op.clone(), op.passkeys[j].clone())
        });
        Ok((found, false))
    });
    let known = match known {
        Ok(known) => known,
        Err(e) => return op_error_response(e),
    };
    // A credential nobody holds is throttled under its own id, so the 429 comes after the same
    // five attempts whether or not the credential exists. The attempt is charged here, before
    // the assertion is checked.
    let key = match &known {
        Some((op, _)) => op.username.to_ascii_lowercase(),
        None => format!(
            "passkey:{}",
            credential_id.chars().take(64).collect::<String>()
        ),
    };
    if let Err(wait) = state.login_guard.throttle().reserve(&key, now) {
        return throttled(wait);
    }

    let assertion = (|| {
        Some(Assertion {
            client_data: decode_b64_flexible(assertion_field(body, "client_data_json_b64")?)?,
            authenticator_data: decode_b64_flexible(assertion_field(
                body,
                "authenticator_data_b64",
            )?)?,
            signature: decode_b64_flexible(assertion_field(body, "signature_b64")?)?,
        })
    })();
    let client_data = assertion
        .as_ref()
        .and_then(|a| parse_client_data(&a.client_data));
    // The challenge is spent whatever happens next.
    let challenge_live = client_data
        .as_ref()
        .and_then(|cd| cd.challenge.as_deref())
        .is_some_and(|c| state.login_guard.challenges().consume(c, now));

    let verified = (|| {
        let (op, key_entry) = known.as_ref()?;
        let (assertion, client_data) = (assertion.as_ref()?, client_data.as_ref()?);
        if !challenge_live {
            return None;
        }
        // A credential saved before origins were pinned has no origin. Its origin was never
        // established, so it cannot be checked, and it is refused rather than guessed from the
        // `Host` header of this request, which the caller controls.
        let pinned = parse_origin(key_entry.origin.as_deref()?)?;
        let public_key = decode_b64_flexible(&key_entry.public_key_b64)?;
        state.login_guard.note_verification();
        let new_count = check_assertion(
            assertion,
            client_data,
            &pinned,
            &public_key,
            key_entry.sign_count,
        )?;
        Some((op.id.clone(), key_entry.credential_id.clone(), new_count))
    })();
    let Some((operator_id, credential, new_count)) = verified else {
        return invalid_credentials();
    };

    // Everything that decides the login now happens in ONE critical section on the file as it is
    // now: the stored counter, the account's TOTP state and its last spent step are all read
    // here, not from the snapshot taken before the assertion was checked. Two concurrent requests
    // with the same assertion or the same code are serialised by the lock, so the second finds the
    // counter or step already moved and is refused. Nothing is written unless every check
    // passed: a missing or wrong code leaves the passkey counter and `last_totp_step` untouched,
    // so a failed second factor never spends the authenticator's counter.
    let store = state.open_store().ok();
    let outcome = locked(&home, &owner_login, |file| {
        let Some(i) = file.operators.iter().position(|o| o.id == operator_id) else {
            return Ok((PasskeyOutcome::Refused, false));
        };
        let op = &mut file.operators[i];
        let Some(entry) = op.passkeys.iter().find(|k| k.credential_id == credential) else {
            return Ok((PasskeyOutcome::Refused, false));
        };
        if entry.sign_count != 0 && new_count <= entry.sign_count {
            return Ok((PasskeyOutcome::Refused, false));
        }
        let mut spent_step = None;
        if op.totp_enabled {
            // The assertion already checked out, so asking for the code reveals nothing to a
            // caller who does not hold the passkey.
            let Some(code) = totp_code else {
                return Ok((PasskeyOutcome::RequiresTotp, false));
            };
            // The code is a second guess inside a request that was already admitted: charge it
            // before it is checked, to the same name bucket the password path charges, and
            // never turn the request away halfway.
            state.login_guard.throttle().charge(&key, Instant::now());
            let Some(step) = verify_login_totp(op, code) else {
                return Ok((PasskeyOutcome::Refused, false));
            };
            spent_step = Some(step);
        }
        let entry = op
            .passkeys
            .iter_mut()
            .find(|k| k.credential_id == credential)
            .expect("the entry was found above under the same lock");
        let changed = entry.sign_count != new_count || spent_step.is_some();
        entry.sign_count = new_count;
        if let Some(step) = spent_step {
            op.last_totp_step = Some(step);
        }
        let result = mint_under_lock(state, store.as_deref(), file, &operator_id, None);
        Ok((PasskeyOutcome::Done(result), changed))
    });
    match outcome {
        Ok(PasskeyOutcome::Done(result)) => {
            let (resp, verified) = login_response(result, state);
            if verified {
                state.login_guard.throttle().clear(&key);
            }
            resp
        }
        Ok(PasskeyOutcome::RequiresTotp) => HttpResponse::json(
            401,
            json!({
                "ok": false,
                "requires_totp": true,
                "error": "TOTP code is required for this account",
            }),
        ),
        Ok(PasskeyOutcome::Refused) => invalid_credentials(),
        Err(e) => op_error_response(e),
    }
}

/// How a passkey login that passed its assertion ended, decided under the operators lock.
enum PasskeyOutcome {
    Done(LoginResult),
    /// The account has TOTP on and the request carried no code. Nothing was spent.
    RequiresTotp,
    /// The counter did not advance, the account is gone, or the code was wrong. Nothing was
    /// spent.
    Refused,
}

/// `POST /api/v1/auth/passkey/challenge` (public).
pub(crate) fn handle_post_passkey_challenge(state: &HubState, _req: &HttpRequest) -> HttpResponse {
    let now = Instant::now();
    if let Some(wait) = state.login_guard.throttle().challenge_retry_after(now) {
        return throttled(wait);
    }
    match state.login_guard.challenges().issue(now) {
        Ok(challenge) => HttpResponse::json(
            200,
            json!({ "challenge": challenge, "expires_in_secs": CHALLENGE_TTL.as_secs() }),
        ),
        Err(e) => op_error_response(e),
    }
}

// --- operators CRUD -------------------------------------------------------------------------

/// `GET /api/v1/auth/operators`: any authenticated caller.
pub(crate) fn handle_get_operators(state: &HubState) -> HttpResponse {
    let home = match home_or_500(state) {
        Ok(home) => home,
        Err(resp) => return resp,
    };
    let owner_login = state.auth.owner_login().to_string();
    match locked(&home, &owner_login, |file| {
        Ok((
            file.operators.iter().map(operator_view).collect::<Vec<_>>(),
            false,
        ))
    }) {
        Ok(operators) => HttpResponse::json(200, json!({ "operators": operators })),
        Err(e) => op_error_response(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpsertBody {
    username: String,
    display_name: Option<String>,
    role: Option<String>,
    password: Option<String>,
}

/// `POST /api/v1/auth/operators` (Owner): create an operator, or update an existing username.
pub(crate) fn handle_post_operator(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if let Err(resp) = require_owner(authority, "manage operators") {
        return resp;
    }
    let body = match object_body(req).and_then(parse_body::<UpsertBody>) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let home = match home_or_500(state) {
        Ok(home) => home,
        Err(resp) => return resp,
    };
    let role = match body.role.as_deref().map(|r| (r, Role::parse(r))) {
        Some((_, Some(role))) => Some(role),
        Some((_, None)) => {
            return error_json(400, "`role` must be owner, admin, operator or viewer");
        }
        None => None,
    };
    let username = body.username.trim().to_string();
    if let Some(name) = &body.display_name
        && !valid_text(name, MAX_DISPLAY_NAME_CHARS)
    {
        return error_json(
            400,
            "`display_name` must be 100 characters or fewer, with no control characters",
        );
    }
    if let Some(pw) = &body.password
        && !valid_password(pw)
    {
        return error_json(400, "`password` must be 8 to 256 characters");
    }
    // Hashing is slow: do it before the lock, and only for a body that is otherwise valid.
    let password = match body.password.as_deref().map(hash_password).transpose() {
        Ok(hash) => hash,
        Err(e) => return op_error_response(e),
    };
    let new_id = match new_operator_id() {
        Ok(id) => id,
        Err(e) => return op_error_response(e),
    };
    let owner_login = state.auth.owner_login().to_string();
    let result = locked(&home, &owner_login, |file| {
        let owners = file.owner_count();
        if let Some(i) = file.find_username(&username) {
            let op = &mut file.operators[i];
            // Sessions of this operator, and only this one, end when it loses owner authority or
            // its password changes: a session is a credential that outlives the one it came from.
            let mut revoke = false;
            if let Some(role) = role {
                if op.role == Role::Owner && role != Role::Owner && owners == 1 {
                    return Err(OpError::Conflict(
                        "cannot demote the last owner".to_string(),
                    ));
                }
                revoke |= op.role == Role::Owner && role != Role::Owner;
                op.role = role;
            }
            if let Some(name) = &body.display_name {
                op.display_name = display_name_or(name, &op.username);
            }
            if let Some((hash, salt, iterations)) = password {
                op.password_hash_hex = Some(hash);
                op.password_salt_hex = Some(salt);
                op.password_iterations = Some(iterations);
                revoke = true;
            }
            let revoke = revoke.then(|| op.username.clone());
            return Ok(((operator_view(op), false, revoke), true));
        }
        if file.operators.len() >= MAX_OPERATORS {
            return Err(OpError::Conflict(format!(
                "the hub already has {MAX_OPERATORS} operators"
            )));
        }
        if !valid_username(&username) {
            return Err(OpError::BadRequest(
                "`username` must be 2 to 64 characters of a-z, 0-9, `-` and `_`".to_string(),
            ));
        }
        let Some(role) = role else {
            return Err(OpError::BadRequest("`role` is required".to_string()));
        };
        let mut op = StoredOperator {
            id: new_id,
            display_name: display_name_or(body.display_name.as_deref().unwrap_or(""), &username),
            username,
            role,
            password_hash_hex: None,
            password_salt_hex: None,
            password_iterations: None,
            totp_secret_base32: None,
            totp_pending_secret_base32: None,
            totp_enabled: false,
            last_totp_step: None,
            passkeys: Vec::new(),
            created_at_ms: now_millis(),
        };
        if let Some((hash, salt, iterations)) = password {
            op.password_hash_hex = Some(hash);
            op.password_salt_hex = Some(salt);
            op.password_iterations = Some(iterations);
        }
        let view = operator_view(&op);
        file.operators.push(op);
        Ok(((view, true, None), true))
    });
    match result {
        Ok((view, created, revoke)) => {
            if let Some(login) = revoke {
                revoke_login_sessions(state, &login);
            }
            state.broadcast("operators", "operator.updated", view.clone());
            HttpResponse::json(if created { 201 } else { 200 }, json!({ "operator": view }))
        }
        Err(e) => op_error_response(e),
    }
}

/// End every live session minted for `login`. The login of a session is the operator's username,
/// matched the way operators are: ignoring ASCII case.
fn revoke_login_sessions(state: &HubState, login: &str) -> usize {
    let store = state.open_store().ok();
    state
        .auth
        .revoke_sessions_for_login(store.as_deref(), login)
}

fn display_name_or(name: &str, fallback: &str) -> String {
    let name = name.trim();
    if name.is_empty() { fallback } else { name }.to_string()
}

/// The operator id or username a path segment names: bounded, `[a-z0-9_-]` only, so no `/`,
/// `..`, NUL or `%` survives into a lookup.
fn decode_id_segment(raw: &str) -> Result<String, HttpResponse> {
    let decoded = percent_decode(raw);
    let ok = !decoded.is_empty()
        && decoded.len() <= MAX_ID_SEGMENT_LEN
        && !decoded.contains("..")
        && decoded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(decoded)
    } else {
        Err(error_json(400, "invalid operator id"))
    }
}

/// `DELETE /api/v1/auth/operators/:id` (Owner), by id or username.
///
/// Every live session of the deleted operator's login is revoked, in memory and in the store.
pub(crate) fn handle_delete_operator(
    state: &HubState,
    authority: &HubAuthority,
    raw_id: &str,
) -> HttpResponse {
    if let Err(resp) = require_owner(authority, "manage operators") {
        return resp;
    }
    let target = match decode_id_segment(raw_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let home = match home_or_500(state) {
        Ok(home) => home,
        Err(resp) => return resp,
    };
    let owner_login = state.auth.owner_login().to_string();
    let result = locked(&home, &owner_login, |file| {
        let Some(i) = file.find_id_or_username(&target) else {
            return Err(OpError::NotFound("operator not found".to_string()));
        };
        if file.operators[i].role == Role::Owner && file.owner_count() == 1 {
            return Err(OpError::Conflict(
                "cannot delete the last owner".to_string(),
            ));
        }
        let gone = file.operators.remove(i);
        Ok(((gone.id, gone.username), true))
    });
    match result {
        Ok((id, username)) => {
            revoke_login_sessions(state, &username);
            state.broadcast(
                "operators",
                "operator.deleted",
                json!({ "id": id, "username": username }),
            );
            HttpResponse::json(200, json!({ "ok": true }))
        }
        Err(e) => op_error_response(e),
    }
}

// --- TOTP and passkey enrolment -------------------------------------------------------------

/// The operator a TOTP or passkey call is about: the named username, else the calling owner's
/// own record, else the configured owner's.
fn resolve_target(
    file: &OperatorsFile,
    named: Option<&str>,
    caller: &str,
    owner_login: &str,
) -> Result<usize, OpError> {
    match named.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => file
            .find_username(name)
            .ok_or_else(|| OpError::NotFound("operator not found".to_string())),
        None => file
            .find_username(caller)
            .or_else(|| file.find_username(owner_login))
            .ok_or_else(|| OpError::NotFound("operator not found".to_string())),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollBody {
    username: Option<String>,
}

/// `POST /api/v1/auth/totp/enroll` (Owner): store a new pending secret and return it, once.
/// The live secret, if any, stays in force until `totp/verify` promotes the pending one.
pub(crate) fn handle_post_totp_enroll(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if let Err(resp) = require_owner(authority, "enrol TOTP") {
        return resp;
    }
    let body = match object_body(req).and_then(parse_body::<EnrollBody>) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let home = match home_or_500(state) {
        Ok(home) => home,
        Err(resp) => return resp,
    };
    let secret = match random_array::<TOTP_SECRET_BYTES>() {
        Ok(bytes) => base32_encode(&bytes),
        Err(e) => return op_error_response(e),
    };
    let owner_login = state.auth.owner_login().to_string();
    let caller = authority_login(authority).to_string();
    let result = locked(&home, &owner_login, |file| {
        let i = resolve_target(file, body.username.as_deref(), &caller, &owner_login)?;
        let op = &mut file.operators[i];
        op.totp_pending_secret_base32 = Some(secret.clone());
        Ok((op.username.clone(), true))
    });
    match result {
        Ok(username) => HttpResponse::json(
            200,
            json!({
                "ok": true,
                "otpauth_uri": otpauth_uri(&username, &secret),
                "secret_base32": secret,
            }),
        ),
        Err(e) => op_error_response(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TotpVerifyBody {
    username: Option<String>,
    code: String,
}

enum TotpVerify {
    NoSecret,
    WrongCode,
    /// The updated operator, and the login whose sessions end because its live secret was
    /// replaced.
    Verified(Value, Option<String>),
}

/// `POST /api/v1/auth/totp/verify` (Owner): check a code against the pending secret (else the
/// live one) and, when it matches, make that secret the live one and turn TOTP on.
pub(crate) fn handle_post_totp_verify(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if let Err(resp) = require_owner(authority, "verify TOTP") {
        return resp;
    }
    let body = match object_body(req).and_then(parse_body::<TotpVerifyBody>) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    if body.code.len() > MAX_TOTP_CODE_LEN {
        return error_json(400, "`code` is too long");
    }
    let home = match home_or_500(state) {
        Ok(home) => home,
        Err(resp) => return resp,
    };
    let owner_login = state.auth.owner_login().to_string();
    let caller = authority_login(authority).to_string();
    let key = format!(
        "totp-verify:{}",
        body.username
            .as_deref()
            .unwrap_or(&caller)
            .trim()
            .to_ascii_lowercase()
    );
    // Charged before the code is checked; a success clears it.
    let Some(_permit) = state.login_guard.try_verify_permit() else {
        return busy();
    };
    if let Err(wait) = state.login_guard.throttle().reserve(&key, Instant::now()) {
        return throttled(wait);
    }
    let result = locked(&home, &owner_login, |file| {
        let i = resolve_target(file, body.username.as_deref(), &caller, &owner_login)?;
        let op = &mut file.operators[i];
        let pending = op.totp_pending_secret_base32.clone();
        let Some(secret_b32) = pending.clone().or_else(|| op.totp_secret_base32.clone()) else {
            return Ok((TotpVerify::NoSecret, false));
        };
        let step = base32_decode(&secret_b32).and_then(|s| {
            // A new secret has no replay history of its own; the live one does.
            let last = if pending.is_some() {
                None
            } else {
                op.last_totp_step
            };
            verify_totp(&s, &body.code, now_unix_secs(), last)
        });
        let Some(step) = step else {
            return Ok((TotpVerify::WrongCode, false));
        };
        // Replacing a live secret ends the operator's sessions; enabling TOTP the first time
        // does not.
        let replaced = pending.is_some() && op.totp_secret_base32.is_some();
        op.totp_secret_base32 = Some(secret_b32);
        op.totp_pending_secret_base32 = None;
        op.totp_enabled = true;
        op.last_totp_step = Some(step);
        let revoke = replaced.then(|| op.username.clone());
        Ok((TotpVerify::Verified(operator_view(op), revoke), true))
    });
    match result {
        Ok(TotpVerify::Verified(view, revoke)) => {
            state.login_guard.throttle().clear(&key);
            if let Some(login) = revoke {
                revoke_login_sessions(state, &login);
            }
            state.broadcast("operators", "operator.updated", view);
            HttpResponse::json(200, json!({ "ok": true, "totp_enabled": true }))
        }
        Ok(TotpVerify::NoSecret) => error_json(400, "no TOTP secret to verify; enrol first"),
        Ok(TotpVerify::WrongCode) => error_json(401, "invalid code"),
        Err(e) => op_error_response(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasskeyRegisterBody {
    credential_id: String,
    label: Option<String>,
    public_key_b64: String,
    username: Option<String>,
    /// The origin the credential was created on. Default: the origin of this request, from its
    /// `Host` header (`http` for a loopback host, `https` otherwise).
    origin: Option<String>,
}

/// `POST /api/v1/auth/passkey/register` (Owner). The public key is the 65-byte uncompressed
/// SEC1 P-256 point; a COSE key is refused. The credential is pinned to one origin, and login
/// accepts it only from there.
pub(crate) fn handle_post_passkey_register(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> HttpResponse {
    if let Err(resp) = require_owner(authority, "register a passkey") {
        return resp;
    }
    let body = match object_body(req).and_then(parse_body::<PasskeyRegisterBody>) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    if !valid_credential_id(&body.credential_id) {
        return error_json(
            400,
            "`credential_id` must be 16 to 1024 base64url characters",
        );
    }
    let label = body
        .label
        .as_deref()
        .unwrap_or("passkey")
        .trim()
        .to_string();
    if label.is_empty() || !valid_text(&label, MAX_DISPLAY_NAME_CHARS) {
        return error_json(
            400,
            "`label` must be 1 to 100 characters, with no control characters",
        );
    }
    let Some(public_key_b64) = normalize_public_key(&body.public_key_b64) else {
        return error_json(
            400,
            "`public_key_b64` must be a 65-byte uncompressed SEC1 P-256 point (0x04 || X || Y)",
        );
    };
    let pinned = match body.origin.as_deref() {
        Some(raw) => parse_origin(raw),
        None => origin_of_request(req),
    };
    let Some(pinned) = pinned else {
        return error_json(
            400,
            "`origin` must be https://host[:port] (http only for localhost, 127.0.0.1 or [::1]); \
             without it the request needs a plain `Host` header",
        );
    };
    let home = match home_or_500(state) {
        Ok(home) => home,
        Err(resp) => return resp,
    };
    let owner_login = state.auth.owner_login().to_string();
    let caller = authority_login(authority).to_string();
    let result = locked(&home, &owner_login, |file| {
        let i = resolve_target(file, body.username.as_deref(), &caller, &owner_login)?;
        if file.find_credential(&body.credential_id).is_some() {
            return Err(OpError::Conflict(
                "that credential is already registered".to_string(),
            ));
        }
        if file
            .operators
            .iter()
            .map(|o| o.passkeys.len())
            .sum::<usize>()
            >= MAX_PASSKEYS_TOTAL
        {
            return Err(OpError::Conflict(format!(
                "the hub already has {MAX_PASSKEYS_TOTAL} passkeys"
            )));
        }
        let op = &mut file.operators[i];
        if op.passkeys.len() >= MAX_PASSKEYS_PER_OPERATOR {
            return Err(OpError::Conflict(
                "this operator already has 10 passkeys".to_string(),
            ));
        }
        op.passkeys.push(StoredPasskey {
            credential_id: body.credential_id.clone(),
            label,
            public_key_b64,
            created_at_ms: now_millis(),
            sign_count: 0,
            origin: Some(pinned.origin.clone()),
        });
        Ok(((op.passkeys.len(), operator_view(op)), true))
    });
    match result {
        Ok((count, view)) => {
            state.broadcast("operators", "operator.updated", view);
            HttpResponse::json(200, json!({ "ok": true, "passkeys_count": count }))
        }
        Err(e) => op_error_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handle_http_request;
    use crate::{HubServerConfig, HubWsFrame};
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    const OWNER_TOKEN: &str = "owner-tok-secret";
    const PEER_TOKEN: &str = "peer-tok-secret";
    const HOST: &str = "localhost:8420";
    const PASSWORD: &str = "correct horse battery";

    /// Set this test's clock to Unix second `secs`.
    fn set_clock(secs: u64) {
        TEST_CLOCK.with(|c| c.set(Some(secs)));
    }

    /// The middle of the TOTP step the real clock is in now.
    fn mid_step_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / TOTP_STEP_SECS
            * TOTP_STEP_SECS
            + TOTP_STEP_SECS / 2
    }

    fn state_for(tmp: &TempDir, owner_login: &str) -> HubState {
        // Every test that builds a hub gets a clock that stands still mid-step, so no test
        // meets a TOTP step boundary half way through (its "this step" and "next step" codes
        // would shift under it). A test that wants a boundary moves the clock itself.
        set_clock(mid_step_now());
        HubState::new(HubServerConfig {
            listen: "127.0.0.1:0".into(),
            store_path: tmp.path().join("store.redb"),
            repo_root: tmp.path().to_path_buf(),
            ui_dir: None,
            owner_login: owner_login.into(),
            reviewer_apps: fl_core::pr_policy::ReviewerApps::default(),
            owner_token: Some(OWNER_TOKEN.into()),
            peer_token: Some(PEER_TOKEN.into()),
            trust_tailscale_headers: false,
            enable_terminal: false,
            session_ttl_secs: 3600,
            home_override: Some(tmp.path().to_path_buf()),
        })
    }

    fn state(tmp: &TempDir) -> HubState {
        state_for(tmp, "lmcfarlin")
    }

    fn file_path(tmp: &TempDir) -> PathBuf {
        operators_path(tmp.path())
    }

    fn authed(method: &str, path: &str, token: &str, body: &Value) -> HttpRequest {
        HttpRequest::new(method, path)
            .with_header("Authorization", format!("Bearer {token}"))
            .with_header("Host", HOST)
            .with_json_body(body)
    }

    fn owner(state: &HubState, method: &str, path: &str, body: &Value) -> HttpResponse {
        handle_http_request(state, &authed(method, path, OWNER_TOKEN, body))
    }

    fn peer(state: &HubState, method: &str, path: &str, body: &Value) -> HttpResponse {
        handle_http_request(state, &authed(method, path, PEER_TOKEN, body))
    }

    /// A public (unauthenticated) call as a browser on `host` would make it.
    fn public(state: &HubState, path: &str, host: Option<&str>, body: &Value) -> HttpResponse {
        let mut req = HttpRequest::new("POST", path).with_json_body(body);
        if let Some(host) = host {
            req = req.with_header("Host", host);
        }
        handle_http_request(state, &req)
    }

    fn login(state: &HubState, body: &Value) -> HttpResponse {
        public(state, "/api/v1/auth/login", Some(HOST), body)
    }

    fn password_login(state: &HubState, username: &str, password: &str) -> HttpResponse {
        login(
            state,
            &json!({ "mode": "password", "username": username, "password": password }),
        )
    }

    fn body_text(resp: &HttpResponse) -> String {
        String::from_utf8_lossy(&resp.body).to_string()
    }

    fn json_of(resp: &HttpResponse) -> Value {
        resp.json_value().unwrap()
    }

    fn set_cookie(resp: &HttpResponse) -> Option<String> {
        resp.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, v)| v.clone())
    }

    fn header(resp: &HttpResponse, name: &str) -> Option<String> {
        resp.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    /// The session behind a login response's cookie, as `GET /auth/session` sees it.
    fn session_via_cookie(state: &HubState, login_resp: &HttpResponse) -> HttpResponse {
        let cookie = set_cookie(login_resp).expect("a session cookie");
        let pair = cookie.split(';').next().unwrap().to_string();
        handle_http_request(
            state,
            &HttpRequest::new("GET", "/api/v1/auth/session").with_header("Cookie", pair),
        )
    }

    fn is_invalid_credentials(resp: &HttpResponse) -> bool {
        resp.status == 401
            && json_of(resp) == json!({ "ok": false, "error": "invalid credentials" })
            && set_cookie(resp).is_none()
    }

    fn create_operator(
        state: &HubState,
        username: &str,
        role: &str,
        password: Option<&str>,
    ) -> HttpResponse {
        let mut body = json!({ "username": username, "role": role });
        if let Some(pw) = password {
            body["password"] = json!(pw);
        }
        owner(state, "POST", "/api/v1/auth/operators", &body)
    }

    fn stored(tmp: &TempDir) -> OperatorsFile {
        stored_in(tmp.path())
    }

    fn stored_in(home: &Path) -> OperatorsFile {
        read_file(home).unwrap().unwrap()
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn drain(rx: &mut tokio::sync::broadcast::Receiver<HubWsFrame>) -> Vec<HubWsFrame> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(frame);
        }
        out
    }

    /// The code an authenticator would show `steps` steps from now.
    fn code_at(secret_b32: &str, steps: i64) -> String {
        let secret = base32_decode(secret_b32).unwrap();
        let step = (now_unix_secs() / TOTP_STEP_SECS) as i64 + steps;
        format!("{:06}", hotp(&secret, step as u64, 6))
    }

    // --- pure pieces ------------------------------------------------------------------------

    #[test]
    fn production_pbkdf2_cost_is_strong_and_tests_use_the_cheap_one() {
        const { assert!(PRODUCTION_PBKDF2_ITERATIONS >= 600_000) };
        // Only a test build is cheap; any other build is the strong value.
        assert_eq!(PBKDF2_ITERATIONS, 1_000);
        let (_, _, iterations) = hash_password("a long enough password").unwrap();
        assert_eq!(iterations, PBKDF2_ITERATIONS);
    }

    #[test]
    fn pbkdf2_verifies_the_right_password_with_the_accounts_own_salt_and_count() {
        let (hash, salt, iterations) = hash_password(PASSWORD).unwrap();
        assert_eq!(salt.len(), SALT_LEN * 2);
        assert_eq!(hash.len(), HASH_LEN * 2);
        let mut op = seed_file("x").unwrap().operators.remove(0);
        op.password_hash_hex = Some(hash);
        op.password_salt_hex = Some(salt.clone());
        op.password_iterations = Some(iterations);
        assert!(verify_password(&op, PASSWORD));
        assert!(!verify_password(&op, "correct horse battery!"));
        assert!(!verify_password(&op, ""));
        // The stored iteration count is what verifies: a different one cannot.
        op.password_iterations = Some(iterations + 1);
        assert!(!verify_password(&op, PASSWORD));
        op.password_iterations = Some(0);
        assert!(!verify_password(&op, PASSWORD));
        op.password_iterations = Some(MAX_STORED_PBKDF2_ITERATIONS + 1);
        assert!(!verify_password(&op, PASSWORD));
        // A fresh random salt per hash.
        let (_, salt2, _) = hash_password(PASSWORD).unwrap();
        assert_ne!(salt, salt2);
    }

    #[test]
    fn base32_matches_rfc_4648_vectors_and_round_trips() {
        for (raw, enc) in [
            ("", ""),
            ("f", "MY"),
            ("fo", "MZXQ"),
            ("foo", "MZXW6"),
            ("foob", "MZXW6YQ"),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI"),
        ] {
            assert_eq!(base32_encode(raw.as_bytes()), enc, "{raw}");
            assert_eq!(base32_decode(enc).unwrap(), raw.as_bytes(), "{raw}");
        }
        assert_eq!(base32_decode("mzxw6ytboi======").unwrap(), b"foobar");
        for bad in ["MZXW6 YTBOI", "MZXW1", "MZ-XW", "MZXW6\u{e9}"] {
            assert!(base32_decode(bad).is_none(), "{bad:?}");
        }
        let secret = random_array::<TOTP_SECRET_BYTES>().unwrap();
        let enc = base32_encode(&secret);
        assert_eq!(enc.len(), 32);
        assert_eq!(base32_decode(&enc).unwrap(), secret);
    }

    #[test]
    fn totp_matches_the_rfc_6238_and_rfc_4226_vectors() {
        let secret = b"12345678901234567890";
        // RFC 6238 appendix B, SHA-1, 8 digits.
        for (time, code) in [
            (59u64, 94_287_082u32),
            (1_111_111_109, 7_081_804),
            (1_111_111_111, 14_050_471),
            (1_234_567_890, 89_005_924),
            (2_000_000_000, 69_279_037),
            (20_000_000_000, 65_353_130),
        ] {
            assert_eq!(hotp(secret, time / 30, 8), code, "T={time}");
            // The 6-digit form is the same value truncated.
            assert_eq!(hotp(secret, time / 30, 6), code % 1_000_000, "T={time}");
        }
        assert_eq!(hotp(secret, 1, 6), 287_082);
        // RFC 4226 appendix D.
        for (counter, code) in [(0u64, 755_224u32), (1, 287_082), (2, 359_152), (9, 520_489)] {
            assert_eq!(hotp(secret, counter, 6), code);
        }
    }

    #[test]
    fn totp_accepts_one_step_either_side_and_nothing_further() {
        let secret = b"12345678901234567890";
        let now = 1_111_111_111u64;
        let step = now / 30;
        let code = |s: u64| format!("{:06}", hotp(secret, s, 6));
        assert_eq!(verify_totp(secret, &code(step), now, None), Some(step));
        assert_eq!(
            verify_totp(secret, &code(step - 1), now, None),
            Some(step - 1)
        );
        assert_eq!(
            verify_totp(secret, &code(step + 1), now, None),
            Some(step + 1)
        );
        assert_eq!(verify_totp(secret, &code(step - 2), now, None), None);
        assert_eq!(verify_totp(secret, &code(step + 2), now, None), None);
        for bad in ["", "12345", "1234567", "12345a", " ", "２８７０８２"] {
            assert_eq!(verify_totp(secret, bad, now, None), None, "{bad:?}");
        }
        // Surrounding whitespace is a typing slip, not a different code.
        assert_eq!(
            verify_totp(secret, &format!(" {} ", code(step)), now, None),
            Some(step)
        );
    }

    #[test]
    fn totp_replay_needs_a_strictly_newer_step() {
        let secret = b"12345678901234567890";
        let now = 1_111_111_111u64;
        let step = now / 30;
        let code = |s: u64| format!("{:06}", hotp(secret, s, 6));
        assert_eq!(
            verify_totp(secret, &code(step), now, Some(step - 1)),
            Some(step)
        );
        // The step already used, and every older one, is refused.
        assert_eq!(verify_totp(secret, &code(step), now, Some(step)), None);
        assert_eq!(verify_totp(secret, &code(step - 1), now, Some(step)), None);
        assert_eq!(verify_totp(secret, &code(step), now, Some(step + 1)), None);
        // A newer step in the window still works, and the newest match wins.
        assert_eq!(
            verify_totp(secret, &code(step + 1), now, Some(step)),
            Some(step + 1)
        );
    }

    #[test]
    fn otpauth_uri_url_encodes_the_label() {
        let uri = otpauth_uri("alice", "ABCDEFGH");
        assert_eq!(
            uri,
            "otpauth://totp/FerroLoop:alice?secret=ABCDEFGH&issuer=FerroLoop&algorithm=SHA1&digits=6&period=30"
        );
        assert!(otpauth_uri("a b/c:d", "X").starts_with("otpauth://totp/FerroLoop:a%20b%2Fc%3Ad?"));
    }

    #[test]
    fn throttle_locks_after_five_failures_and_clears_on_success() {
        let mut t = Throttle::default();
        let t0 = Instant::now();
        for i in 0..4 {
            assert_eq!(t.retry_after("alice", t0), None, "attempt {i}");
            t.charge("alice", t0);
        }
        assert_eq!(t.retry_after("alice", t0), None);
        t.charge("alice", t0);
        assert_eq!(t.retry_after("alice", t0), Some(60));
        assert_eq!(
            t.retry_after("alice", t0 + Duration::from_secs(30)),
            Some(30)
        );
        // Another name is unaffected.
        assert_eq!(t.retry_after("bob", t0), None);
        // The lockout ends after 60 s, but the count is kept: one more failure locks again.
        let later = t0 + Duration::from_secs(61);
        assert_eq!(t.retry_after("alice", later), None);
        t.charge("alice", later);
        assert_eq!(t.retry_after("alice", later), Some(60));
        // A success clears it.
        t.clear("alice");
        assert_eq!(t.retry_after("alice", later), None);
        for _ in 0..4 {
            t.charge("alice", later);
        }
        assert_eq!(t.retry_after("alice", later), None);
    }

    #[test]
    fn throttle_window_expires_after_five_minutes() {
        let mut t = Throttle::default();
        let t0 = Instant::now();
        for _ in 0..4 {
            t.charge("alice", t0);
        }
        // Slow guessing never accumulates: the fifth failure starts a fresh count.
        t.charge("alice", t0 + Duration::from_secs(301));
        assert_eq!(t.retry_after("alice", t0 + Duration::from_secs(301)), None);
    }

    #[test]
    fn throttle_caps_tracked_names_and_evicts_unlocked_ones_first() {
        let mut t = Throttle::default();
        let t0 = Instant::now();
        for _ in 0..5 {
            t.charge("locked-victim", t0);
        }
        for i in 0..(MAX_TRACKED_USERS + 200) {
            let now = t0 + Duration::from_millis(i as u64 + 1);
            t.charge(&format!("user-{i}"), now);
            // Keep the global window out of the way: this test is about the name cap.
            t.global.clear();
        }
        assert_eq!(t.tracked_users(), MAX_TRACKED_USERS);
        assert_eq!(
            t.retry_after("locked-victim", t0 + Duration::from_millis(10)),
            Some(60),
            "a locked name is evicted last"
        );
    }

    #[test]
    fn throttle_has_a_global_limit_across_names() {
        let mut t = Throttle::default();
        let t0 = Instant::now();
        for i in 0..MAX_GLOBAL_FAILURES_PER_MINUTE {
            assert_eq!(t.retry_after(&format!("u{i}"), t0), None);
            t.charge(&format!("u{i}"), t0);
        }
        // A name never seen before is refused: the hub as a whole is over its rate.
        assert!(t.retry_after("fresh", t0).is_some());
        assert_eq!(t.retry_after("fresh", t0 + Duration::from_secs(61)), None);
    }

    #[test]
    fn challenges_are_single_use_expire_and_are_capped() {
        let mut c = Challenges::default();
        let t0 = Instant::now();
        let a = c.issue(t0).unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(&a).unwrap().len(), 32);
        assert!(c.consume(&a, t0));
        assert!(!c.consume(&a, t0), "a challenge works once");
        assert!(!c.consume("never-issued", t0));

        let b = c.issue(t0).unwrap();
        assert!(!c.consume(&b, t0 + CHALLENGE_TTL), "expired at the TTL");
        assert!(!c.consume(&b, t0), "an expired one is spent");

        let first = c.issue(t0).unwrap();
        for _ in 0..(MAX_LIVE_CHALLENGES + 20) {
            c.issue(t0).unwrap();
        }
        assert!(c.len() <= MAX_LIVE_CHALLENGES);
        assert!(!c.consume(&first, t0), "the oldest is dropped at the cap");
    }

    #[test]
    fn host_and_origin_rules() {
        let host = |h: &str| parse_host(h);
        assert_eq!(
            host("hub.example.ts.net").unwrap().hostname,
            "hub.example.ts.net"
        );
        assert_eq!(host("localhost:8420").unwrap().hostname, "localhost");
        assert_eq!(host("[::1]:8420").unwrap().hostname, "[::1]");
        for bad in [
            "",
            " ",
            "a b",
            "a/b",
            "host:",
            "host:abc",
            "host:1:2",
            "[::1",
            "[]",
            "a@b",
            "host:123456",
        ] {
            assert!(host(bad).is_none(), "{bad:?}");
        }
        let origin = |o: &str| parse_origin(o).map(|p| (p.origin, p.rp_id));
        let some = |o: &str, rp: &str| Some((o.to_string(), rp.to_string()));
        assert_eq!(
            origin("https://hub.example.ts.net"),
            some("https://hub.example.ts.net", "hub.example.ts.net")
        );
        assert_eq!(
            origin("HTTPS://Hub.Example.ts.net:8443"),
            some("https://hub.example.ts.net:8443", "hub.example.ts.net")
        );
        assert_eq!(
            origin("http://localhost:8420"),
            some("http://localhost:8420", "localhost")
        );
        assert_eq!(
            origin("https://localhost:8420"),
            some("https://localhost:8420", "localhost")
        );
        assert_eq!(
            origin("http://[::1]:8420"),
            some("http://[::1]:8420", "[::1]")
        );
        // A default port is what a browser leaves out.
        assert_eq!(
            origin("https://hub.example.ts.net:443"),
            some("https://hub.example.ts.net", "hub.example.ts.net")
        );
        for bad in [
            "http://hub.example.ts.net",
            "ftp://localhost",
            "hub.example.ts.net",
            "https://",
            "https://hub.example.ts.net/",
            "https://hub.example.ts.net/path",
            "https://user@hub.example.ts.net",
            "https://hub.example.ts.net:",
            "",
        ] {
            assert_eq!(origin(bad), None, "{bad:?}");
        }
        let req = |h: Option<&str>| {
            let mut r = HttpRequest::new("POST", "/");
            if let Some(h) = h {
                r = r.with_header("Host", h);
            }
            origin_of_request(&r).map(|p| p.origin)
        };
        assert_eq!(
            req(Some("localhost:8420")).as_deref(),
            Some("http://localhost:8420")
        );
        assert_eq!(
            req(Some("hub.example.ts.net")).as_deref(),
            Some("https://hub.example.ts.net")
        );
        assert_eq!(req(Some("a b")), None);
        assert_eq!(req(None), None, "no Host, no origin");
    }

    // --- persistence and the seed -----------------------------------------------------------

    #[test]
    fn the_first_read_seeds_the_configured_owner_with_no_password_at_mode_0600() {
        let tmp = TempDir::new().unwrap();
        let state = state_for(&tmp, "alice-owner");
        assert!(!file_path(&tmp).exists());
        let resp = peer(&state, "GET", "/api/v1/auth/operators", &json!({}));
        assert_eq!(resp.status, 200);
        let list = json_of(&resp);
        let ops = list["operators"].as_array().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0]["username"], "alice-owner");
        assert_eq!(ops[0]["display_name"], "alice-owner");
        assert_eq!(ops[0]["role"], "owner");
        assert_eq!(ops[0]["has_password"], false);
        assert_eq!(ops[0]["totp_enabled"], false);
        assert_eq!(ops[0]["passkeys_count"], 0);
        assert!(ops[0]["id"].as_str().unwrap().starts_with("op-"));
        assert_eq!(ops[0]["id"].as_str().unwrap().len(), 3 + 16);
        assert_eq!(mode_of(&file_path(&tmp)), 0o600);
        // A second read does not seed again: same id.
        let again = json_of(&owner(&state, "GET", "/api/v1/auth/operators", &json!({})));
        assert_eq!(again["operators"][0]["id"], ops[0]["id"]);
    }

    #[test]
    fn every_rewrite_keeps_mode_0600() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        assert_eq!(
            create_operator(&state, "bob", "viewer", Some(PASSWORD)).status,
            201
        );
        assert_eq!(mode_of(&file_path(&tmp)), 0o600);
        fs::set_permissions(file_path(&tmp), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(create_operator(&state, "carol", "viewer", None).status, 201);
        assert_eq!(
            mode_of(&file_path(&tmp)),
            0o600,
            "a rewrite does not inherit a loose mode"
        );
        // No temp file is left behind.
        let leftovers: Vec<_> = fs::read_dir(file_path(&tmp).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n != "operators.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn an_existing_file_is_never_overwritten_by_the_seed() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let mut file = seed_file("someone-else").unwrap();
        file.operators[0].role = Role::Admin;
        file.operators.push({
            let mut op = seed_file("second").unwrap().operators.remove(0);
            op.role = Role::Owner;
            op
        });
        write_file(tmp.path(), &file, true).unwrap();
        let before = fs::read(file_path(&tmp)).unwrap();
        let list = json_of(&owner(&state, "GET", "/api/v1/auth/operators", &json!({})));
        let names: Vec<_> = list["operators"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["username"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["someone-else", "second"]);
        assert_eq!(fs::read(file_path(&tmp)).unwrap(), before);
        // The exclusive write refuses to replace a file, whatever the caller thinks.
        assert!(write_file(tmp.path(), &seed_file("x").unwrap(), true).is_err());
        assert_eq!(fs::read(file_path(&tmp)).unwrap(), before);
    }

    #[test]
    fn a_corrupt_file_is_a_500_everywhere_and_is_left_untouched() {
        let cases: [(&str, Vec<u8>); 4] = [
            ("garbage", b"{ this is not json".to_vec()),
            ("empty", Vec::new()),
            ("wrong shape", b"[1,2,3]".to_vec()),
            ("duplicate username", {
                let mut file = seed_file("dup").unwrap();
                let again = file.operators[0].clone();
                let mut twin = again;
                twin.id = "op-0000000000000000".into();
                file.operators.push(twin);
                serde_json::to_vec(&file).unwrap()
            }),
        ];
        for (name, bytes) in cases {
            let tmp = TempDir::new().unwrap();
            let state = state(&tmp);
            let path = file_path(&tmp);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &bytes).unwrap();
            for resp in [
                owner(&state, "GET", "/api/v1/auth/operators", &json!({})),
                owner(
                    &state,
                    "POST",
                    "/api/v1/auth/operators",
                    &json!({ "username": "newbie", "role": "viewer" }),
                ),
                owner(
                    &state,
                    "DELETE",
                    "/api/v1/auth/operators/someone",
                    &json!({}),
                ),
                owner(&state, "POST", "/api/v1/auth/totp/enroll", &json!({})),
                password_login(&state, "lmcfarlin", PASSWORD),
                login(
                    &state,
                    &json!({ "mode": "passkey", "credential_id": "x".repeat(20) }),
                ),
            ] {
                assert_eq!(resp.status, 500, "{name}: {}", body_text(&resp));
                assert!(set_cookie(&resp).is_none(), "{name}");
            }
            assert_eq!(
                fs::read(&path).unwrap(),
                bytes,
                "{name}: file must be untouched"
            );
        }
    }

    #[test]
    fn a_symlinked_operator_file_is_refused() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let real = tmp.path().join("elsewhere.json");
        fs::write(&real, serde_json::to_vec(&seed_file("x").unwrap()).unwrap()).unwrap();
        let path = file_path(&tmp);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        assert_eq!(
            owner(&state, "GET", "/api/v1/auth/operators", &json!({})).status,
            500
        );
    }

    // --- authorisation ----------------------------------------------------------------------

    #[test]
    fn a_peer_is_refused_on_every_mutating_route_and_the_file_is_not_touched() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let mut rx = state.subscribe();
        let routes = [
            (
                "POST",
                "/api/v1/auth/operators",
                json!({ "username": "mallory", "role": "owner" }),
            ),
            ("DELETE", "/api/v1/auth/operators/lmcfarlin", json!({})),
            ("POST", "/api/v1/auth/totp/enroll", json!({})),
            (
                "POST",
                "/api/v1/auth/totp/verify",
                json!({ "code": "123456" }),
            ),
            (
                "POST",
                "/api/v1/auth/passkey/register",
                json!({
                    "credential_id": "a".repeat(20),
                    "label": "x",
                    "public_key_b64": URL_SAFE_NO_PAD.encode([4u8; 65]),
                }),
            ),
        ];
        for (method, path, body) in &routes {
            let resp = peer(&state, method, path, body);
            assert_eq!(resp.status, 403, "{method} {path}");
            let unauth = handle_http_request(
                &state,
                &HttpRequest::new(*method, *path).with_json_body(body),
            );
            assert_eq!(unauth.status, 401, "{method} {path} unauthenticated");
        }
        assert!(
            !file_path(&tmp).exists(),
            "a refused call must not create the file"
        );
        assert!(
            drain(&mut rx).is_empty(),
            "a refused call must not broadcast"
        );

        // Once the file exists, a refused call leaves its bytes alone.
        assert_eq!(
            owner(&state, "GET", "/api/v1/auth/operators", &json!({})).status,
            200
        );
        let before = fs::read(file_path(&tmp)).unwrap();
        for (method, path, body) in &routes {
            assert_eq!(peer(&state, method, path, body).status, 403);
        }
        assert_eq!(fs::read(file_path(&tmp)).unwrap(), before);
    }

    #[test]
    fn a_peer_may_list_operators() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let resp = peer(&state, "GET", "/api/v1/auth/operators", &json!({}));
        assert_eq!(resp.status, 200);
        assert_eq!(json_of(&resp)["operators"][0]["username"], "lmcfarlin");
        let unauth =
            handle_http_request(&state, &HttpRequest::new("GET", "/api/v1/auth/operators"));
        assert_eq!(unauth.status, 401);
    }

    // --- operators CRUD ---------------------------------------------------------------------

    #[test]
    fn create_update_list_and_delete() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let mut rx = state.subscribe();

        let created = create_operator(&state, "dana", "operator", None);
        assert_eq!(created.status, 201);
        let view = json_of(&created)["operator"].clone();
        assert!(
            json_of(&created).as_object().unwrap().len() == 1,
            "the operator is nested, and only that"
        );
        assert_eq!(view["username"], "dana");
        assert_eq!(view["display_name"], "dana", "defaults to the username");
        assert_eq!(view["role"], "operator");
        assert_eq!(view["has_password"], false);
        let id = view["id"].as_str().unwrap().to_string();

        let updated = owner(
            &state,
            "POST",
            "/api/v1/auth/operators",
            &json!({ "username": "dana", "display_name": " Dana D ", "role": "admin", "password": PASSWORD }),
        );
        assert_eq!(updated.status, 200);
        let view = json_of(&updated)["operator"].clone();
        assert_eq!(view["id"], id, "an update keeps the id");
        assert_eq!(view["display_name"], "Dana D");
        assert_eq!(view["role"], "admin");
        assert_eq!(view["has_password"], true);

        // Changing only the password resets nothing else.
        let again = owner(
            &state,
            "POST",
            "/api/v1/auth/operators",
            &json!({ "username": "dana", "password": "another password" }),
        );
        assert_eq!(again.status, 200);
        assert_eq!(json_of(&again)["operator"]["role"], "admin");
        assert_eq!(json_of(&again)["operator"]["display_name"], "Dana D");

        let frames = drain(&mut rx);
        let events: Vec<_> = frames
            .iter()
            .map(|f| (f.channel.as_str(), f.event.as_str()))
            .collect();
        assert_eq!(events, vec![("operators", "operator.updated"); 3]);

        let list = json_of(&owner(&state, "GET", "/api/v1/auth/operators", &json!({})));
        assert_eq!(list["operators"].as_array().unwrap().len(), 2);

        // Delete by username, then by id.
        let gone = owner(&state, "DELETE", "/api/v1/auth/operators/dana", &json!({}));
        assert_eq!(gone.status, 200);
        assert_eq!(json_of(&gone), json!({ "ok": true }));
        let frame = drain(&mut rx).pop().unwrap();
        assert_eq!(
            (frame.channel.as_str(), frame.event.as_str()),
            ("operators", "operator.deleted")
        );
        assert_eq!(frame.data["username"], "dana");
        assert_eq!(
            owner(&state, "DELETE", "/api/v1/auth/operators/dana", &json!({})).status,
            404
        );
        let eve = json_of(&create_operator(&state, "eve", "viewer", None))["operator"].clone();
        let by_id = format!("/api/v1/auth/operators/{}", eve["id"].as_str().unwrap());
        assert_eq!(owner(&state, "DELETE", &by_id, &json!({})).status, 200);
        assert_eq!(stored(&tmp).operators.len(), 1);
    }

    #[test]
    fn the_last_owner_cannot_be_demoted_or_deleted() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        owner(&state, "GET", "/api/v1/auth/operators", &json!({}));
        let mut rx = state.subscribe();
        let demote = owner(
            &state,
            "POST",
            "/api/v1/auth/operators",
            &json!({ "username": "lmcfarlin", "role": "viewer" }),
        );
        assert_eq!(demote.status, 409);
        for target in ["lmcfarlin", &stored(&tmp).operators[0].id.clone()] {
            let del = owner(
                &state,
                "DELETE",
                &format!("/api/v1/auth/operators/{target}"),
                &json!({}),
            );
            assert_eq!(del.status, 409, "{target}");
        }
        assert_eq!(stored(&tmp).operators[0].role, Role::Owner);
        assert!(drain(&mut rx).is_empty());

        // With a second owner, either may go.
        assert_eq!(create_operator(&state, "second", "owner", None).status, 201);
        assert_eq!(
            owner(
                &state,
                "POST",
                "/api/v1/auth/operators",
                &json!({ "username": "lmcfarlin", "role": "admin" }),
            )
            .status,
            200
        );
        // Now `second` is the last owner.
        assert_eq!(
            owner(
                &state,
                "DELETE",
                "/api/v1/auth/operators/second",
                &json!({})
            )
            .status,
            409
        );
        assert_eq!(
            owner(
                &state,
                "POST",
                "/api/v1/auth/operators",
                &json!({ "username": "second", "role": "operator" }),
            )
            .status,
            409
        );
        // Re-stating the owner role is not a demotion.
        assert_eq!(
            owner(
                &state,
                "POST",
                "/api/v1/auth/operators",
                &json!({ "username": "second", "role": "owner" }),
            )
            .status,
            200
        );
    }

    #[test]
    fn operator_bodies_are_validated() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let post = |body: Value| owner(&state, "POST", "/api/v1/auth/operators", &body).status;
        assert_eq!(
            post(json!({ "username": "ok-name_1", "role": "viewer" })),
            201
        );
        for bad in [
            json!({ "username": "Upper", "role": "viewer" }),
            json!({ "username": "a", "role": "viewer" }),
            json!({ "username": "a".repeat(65), "role": "viewer" }),
            json!({ "username": "has space", "role": "viewer" }),
            json!({ "username": "dots.no", "role": "viewer" }),
            json!({ "username": "../etc", "role": "viewer" }),
            json!({ "username": "newone" }),
            json!({ "username": "newone", "role": "root" }),
            json!({ "username": "newone", "role": "Owner" }),
            json!({ "username": "newone", "role": "viewer", "display_name": "x".repeat(101) }),
            json!({ "username": "newone", "role": "viewer", "display_name": "bad\u{7}name" }),
            json!({ "username": "newone", "role": "viewer", "display_name": "bad\nname" }),
            json!({ "username": "newone", "role": "viewer", "password": "short" }),
            json!({ "username": "newone", "role": "viewer", "password": "p".repeat(257) }),
            json!({ "username": "newone", "role": "viewer", "extra": 1 }),
            json!({ "username": "newone", "role": "viewer", "password": 12345678 }),
            json!({ "role": "viewer" }),
        ] {
            assert_eq!(post(bad.clone()), 400, "{bad}");
        }
        assert_eq!(
            post(json!({ "username": "newone", "role": "viewer", "password": "p".repeat(256) })),
            201
        );
        assert_eq!(
            post(json!({ "username": "pw-min", "role": "viewer", "password": "12345678" })),
            201
        );
        // A refused body created nothing.
        assert_eq!(stored(&tmp).operators.len(), 4);
        let not_object = handle_http_request(
            &state,
            &HttpRequest::new("POST", "/api/v1/auth/operators")
                .with_header("Authorization", format!("Bearer {OWNER_TOKEN}"))
                .with_body(b"[1]".to_vec()),
        );
        assert_eq!(not_object.status, 400);
        // A type error must not echo a secret back.
        let typed = owner(
            &state,
            "POST",
            "/api/v1/auth/operators",
            &json!({ "username": "newone2", "role": "viewer", "password": { "x": "hunter2-secret" } }),
        );
        assert_eq!(typed.status, 400);
        assert!(!body_text(&typed).contains("hunter2-secret"));
    }

    #[test]
    fn a_delete_path_segment_is_bounded_and_cannot_traverse() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "victim", "viewer", None);
        let long = "a".repeat(MAX_ID_SEGMENT_LEN + 1);
        for seg in [
            "..",
            "%2e%2e",
            "a%2Fb",
            "a%5Cb",
            "%00",
            "a%00b",
            "..%2Fvictim",
            "a.b",
            "a%20b",
            "%",
            &long,
        ] {
            let resp = owner(
                &state,
                "DELETE",
                &format!("/api/v1/auth/operators/{seg}"),
                &json!({}),
            );
            assert!(matches!(resp.status, 400 | 404), "{seg}: {}", resp.status);
            assert_ne!(resp.status, 200, "{seg}");
        }
        assert_eq!(stored(&tmp).operators.len(), 2, "nothing was deleted");
        assert!(decode_id_segment("op-0123456789abcdef").is_ok());
        assert!(decode_id_segment("..").is_err());
        assert!(decode_id_segment("").is_err());
    }

    // --- the secret never leaves --------------------------------------------------------------

    #[test]
    fn no_response_or_broadcast_carries_a_secret() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let mut rx = state.subscribe();
        let mut texts = Vec::new();

        texts.push(body_text(&create_operator(
            &state,
            "frank",
            "owner",
            Some(PASSWORD),
        )));
        let enroll = owner(
            &state,
            "POST",
            "/api/v1/auth/totp/enroll",
            &json!({ "username": "frank" }),
        );
        let secret = json_of(&enroll)["secret_base32"]
            .as_str()
            .unwrap()
            .to_string();
        texts.push(body_text(&owner(
            &state,
            "POST",
            "/api/v1/auth/totp/verify",
            &json!({ "username": "frank", "code": code_at(&secret, 0) }),
        )));
        let pk = new_keypair();
        texts.push(body_text(&owner(
            &state,
            "POST",
            "/api/v1/auth/passkey/register",
            &json!({
                "username": "frank",
                "credential_id": "credential-for-frank-1",
                "label": "laptop",
                "public_key_b64": URL_SAFE_NO_PAD.encode(&pk.1),
            }),
        )));
        // A second, pending secret is stored too: it must not show either.
        let pending = json_of(&owner(
            &state,
            "POST",
            "/api/v1/auth/totp/enroll",
            &json!({ "username": "frank" }),
        ));
        let pending_secret = pending["secret_base32"].as_str().unwrap().to_string();
        texts.push(body_text(&owner(
            &state,
            "POST",
            "/api/v1/auth/operators",
            &json!({ "username": "frank", "display_name": "Frank" }),
        )));
        texts.push(body_text(&owner(
            &state,
            "GET",
            "/api/v1/auth/operators",
            &json!({}),
        )));
        for frame in drain(&mut rx) {
            assert_eq!(frame.channel, "operators");
            texts.push(frame.data.to_string());
        }
        assert!(texts.len() >= 7, "the broadcasts were captured");

        let file = stored(&tmp);
        let frank = &file.operators[file.find_username("frank").unwrap()];
        let secrets = [
            frank.password_hash_hex.clone().unwrap(),
            frank.password_salt_hex.clone().unwrap(),
            frank.totp_secret_base32.clone().unwrap(),
            pending_secret,
            frank.passkeys[0].public_key_b64.clone(),
            URL_SAFE_NO_PAD.encode(&pk.1),
            PASSWORD.to_string(),
        ];
        assert_eq!(frank.totp_secret_base32.as_deref(), Some(secret.as_str()));
        let forbidden_names = [
            "password_hash",
            "password_salt",
            "password_iterations",
            "salt",
            "iterations",
            "totp_secret",
            "totp_pending",
            "secret",
            "public_key",
            "sign_count",
            "last_totp_step",
        ];
        for text in &texts {
            for s in &secrets {
                assert!(!text.contains(s.as_str()), "a secret leaked: {s} in {text}");
            }
            for name in forbidden_names {
                assert!(!text.contains(name), "field {name} leaked in {text}");
            }
        }
        // The view's passkeys list holds exactly these four fields.
        let view = operator_view(frank);
        let key = &view["passkeys"][0];
        let mut fields: Vec<_> = key.as_object().unwrap().keys().cloned().collect();
        fields.sort();
        assert_eq!(
            fields,
            ["created_at_ms", "credential_id", "label", "origin"]
        );
        assert_eq!(key["origin"], "http://localhost:8420");
        let mut top: Vec<_> = view.as_object().unwrap().keys().cloned().collect();
        top.sort();
        assert_eq!(
            top,
            [
                "created_at_ms",
                "display_name",
                "has_password",
                "id",
                "passkeys",
                "passkeys_count",
                "role",
                "totp_enabled",
                "username"
            ]
        );
        // The enroll response is the one place a secret is returned.
        assert!(body_text(&enroll).contains(&secret));
        // And a Debug print of a stored operator holds no secret either.
        let debug = format!("{frank:?}");
        assert!(!debug.contains(frank.password_hash_hex.as_deref().unwrap()));
        assert!(!debug.contains(&secret));
    }

    // --- password login -----------------------------------------------------------------------

    #[test]
    fn password_login_for_an_owner_sets_a_cookie_that_authenticates_as_that_owner() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        assert_eq!(
            create_operator(&state, "grace", "owner", Some(PASSWORD)).status,
            201
        );
        let resp = password_login(&state, "grace", PASSWORD);
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
        let body = json_of(&resp);
        assert_eq!(
            body["authority"],
            json!({ "tier": "owner", "login": "grace", "role": "owner" })
        );
        assert_eq!(body["ok"], true);
        let cookie = set_cookie(&resp).unwrap();
        // The body carries the same token as the cookie, for a client that cannot read cookies.
        let token = body["session_token"].as_str().unwrap();
        assert_eq!(token.len(), 64);
        assert!(cookie.starts_with(&format!("fl_hub_session={token};")));
        assert!(cookie.starts_with("fl_hub_session="));
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        let session = session_via_cookie(&state, &resp);
        assert_eq!(session.status, 200);
        assert_eq!(
            json_of(&session)["authority"],
            json!({ "tier": "owner", "login": "grace" })
        );
        // The username is matched without regard to case.
        assert_eq!(password_login(&state, "GRACE", PASSWORD).status, 200);
    }

    #[test]
    fn the_seeded_owner_has_no_password_and_cannot_log_in_with_one() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        owner(&state, "GET", "/api/v1/auth/operators", &json!({}));
        // Four tries: fewer than the lockout.
        for pw in [PASSWORD, "", "password", "lmcfarlin"] {
            assert!(
                is_invalid_credentials(&password_login(&state, "lmcfarlin", pw)),
                "{pw:?}"
            );
        }
        // But the owner can give the seeded account a password, and then it works.
        assert_eq!(
            owner(
                &state,
                "POST",
                "/api/v1/auth/operators",
                &json!({ "username": "lmcfarlin", "password": PASSWORD }),
            )
            .status,
            200
        );
        let resp = password_login(&state, "lmcfarlin", PASSWORD);
        assert_eq!(resp.status, 200);
        assert_eq!(
            json_of(&session_via_cookie(&state, &resp))["authority"]["login"],
            "lmcfarlin"
        );
    }

    #[test]
    fn a_role_without_authority_is_refused_with_403_and_mints_nothing() {
        for role in ["admin", "operator", "viewer"] {
            let tmp = TempDir::new().unwrap();
            let state = state(&tmp);
            assert_eq!(
                create_operator(&state, "limited", role, Some(PASSWORD)).status,
                201
            );
            let resp = password_login(&state, "limited", PASSWORD);
            assert_eq!(resp.status, 403, "{role}");
            assert_eq!(
                json_of(&resp),
                json!({ "ok": false, "error": "this role cannot log in until RBAC enforcement exists" })
            );
            assert!(set_cookie(&resp).is_none(), "{role}: no cookie");
            assert!(!body_text(&resp).contains("session_token"));
            // A wrong password for the same account is still the plain 401.
            assert!(is_invalid_credentials(&password_login(
                &state,
                "limited",
                "wrong password"
            )));
        }
    }

    #[test]
    fn a_demoted_owner_can_no_longer_log_in() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "temp-owner", "owner", Some(PASSWORD));
        assert_eq!(password_login(&state, "temp-owner", PASSWORD).status, 200);
        create_operator(&state, "keeper", "owner", None);
        owner(
            &state,
            "POST",
            "/api/v1/auth/operators",
            &json!({ "username": "temp-owner", "role": "viewer" }),
        );
        let resp = password_login(&state, "temp-owner", PASSWORD);
        assert_eq!(resp.status, 403);
        assert!(set_cookie(&resp).is_none());
    }

    #[test]
    fn unknown_user_no_password_and_wrong_password_are_indistinguishable() {
        let answer = |username: &str, password: &str| {
            let tmp = TempDir::new().unwrap();
            let state = state(&tmp);
            create_operator(&state, "real", "owner", Some(PASSWORD));
            create_operator(&state, "nopw", "owner", None);
            let resp = password_login(&state, username, password);
            (resp.status, body_text(&resp), set_cookie(&resp))
        };
        let wrong = answer("real", "not the password");
        let unknown = answer("ghost", "not the password");
        let no_password = answer("nopw", "not the password");
        assert_eq!(wrong.0, 401);
        assert_eq!(wrong.1, r#"{"error":"invalid credentials","ok":false}"#);
        assert_eq!(wrong, unknown);
        assert_eq!(wrong, no_password);
    }

    #[test]
    fn login_bodies_are_validated() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "real", "owner", Some(PASSWORD));
        for (name, body) in [
            (
                "no mode",
                json!({ "username": "real", "password": PASSWORD }),
            ),
            ("unknown mode", json!({ "mode": "magic" })),
            ("numeric mode", json!({ "mode": 7 })),
            (
                "no password",
                json!({ "mode": "password", "username": "real" }),
            ),
            (
                "no username",
                json!({ "mode": "password", "password": PASSWORD }),
            ),
            (
                "extra field",
                json!({ "mode": "password", "username": "real", "password": PASSWORD, "x": 1 }),
            ),
            (
                "long username",
                json!({ "mode": "password", "username": "a".repeat(65), "password": PASSWORD }),
            ),
            (
                "long password",
                json!({ "mode": "password", "username": "real", "password": "p".repeat(257) }),
            ),
            (
                "long totp",
                json!({ "mode": "password", "username": "real", "password": PASSWORD, "totp_code": "1".repeat(17) }),
            ),
            ("token missing", json!({ "mode": "token" })),
        ] {
            assert_eq!(login(&state, &body).status, 400, "{name}");
        }
        for raw in [&b"not json"[..], b"[1]", b"\"s\"", b""] {
            let resp = handle_http_request(
                &state,
                &HttpRequest::new("POST", "/api/v1/auth/login").with_body(raw.to_vec()),
            );
            assert_eq!(resp.status, 400, "{raw:?}");
        }
        // None of the refusals counted as a failed login.
        assert_eq!(password_login(&state, "real", PASSWORD).status, 200);
    }

    #[test]
    fn token_login_mints_an_owner_session_and_a_bad_token_is_a_plain_401() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let resp = login(&state, &json!({ "mode": "token", "token": OWNER_TOKEN }));
        assert_eq!(resp.status, 200);
        let body = json_of(&resp);
        assert_eq!(body["ok"], true);
        assert_eq!(
            body["authority"],
            json!({ "tier": "owner", "login": "lmcfarlin", "role": "owner" })
        );
        assert!(body["session_token"].as_str().unwrap().len() >= 32);
        assert_eq!(
            json_of(&session_via_cookie(&state, &resp))["authority"]["tier"],
            "owner"
        );
        for bad in [PEER_TOKEN, "nope", "", "  "] {
            let resp = login(&state, &json!({ "mode": "token", "token": bad }));
            assert!(is_invalid_credentials(&resp), "{bad:?}");
        }
    }

    #[test]
    fn five_failures_lock_a_name_out_with_429_and_retry_after() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "alice", "owner", Some(PASSWORD));
        for i in 0..5 {
            assert!(
                is_invalid_credentials(&password_login(&state, "alice", "wrong password")),
                "{i}"
            );
        }
        // Even the right password is refused while locked, and without a cookie.
        let locked = password_login(&state, "alice", PASSWORD);
        assert_eq!(locked.status, 429);
        assert_eq!(header(&locked, "Retry-After").as_deref(), Some("60"));
        assert!(set_cookie(&locked).is_none());
        // Another account is unaffected.
        create_operator(&state, "bert", "owner", Some(PASSWORD));
        assert_eq!(password_login(&state, "bert", PASSWORD).status, 200);
    }

    #[test]
    fn a_success_clears_the_failure_count() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "alice", "owner", Some(PASSWORD));
        for _ in 0..4 {
            assert_eq!(
                password_login(&state, "alice", "wrong password").status,
                401
            );
        }
        assert_eq!(password_login(&state, "alice", PASSWORD).status, 200);
        for i in 0..4 {
            assert_eq!(
                password_login(&state, "alice", "wrong password").status,
                401,
                "after reset {i}"
            );
        }
        assert_eq!(password_login(&state, "alice", PASSWORD).status, 200);
    }

    #[test]
    fn an_unknown_name_is_throttled_like_a_real_one() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        for _ in 0..5 {
            assert_eq!(
                password_login(&state, "ghost", "whatever password").status,
                401
            );
        }
        let locked = password_login(&state, "ghost", "whatever password");
        assert_eq!(locked.status, 429);
        assert_eq!(header(&locked, "Retry-After").as_deref(), Some("60"));
        // The name is case-folded for the count, so a spelling change does not escape it.
        assert_eq!(
            password_login(&state, "GHOST", "whatever password").status,
            429
        );
    }

    // --- TOTP over HTTP -----------------------------------------------------------------------

    fn enable_totp(state: &HubState, username: &str) -> String {
        let enroll = owner(
            state,
            "POST",
            "/api/v1/auth/totp/enroll",
            &json!({ "username": username }),
        );
        assert_eq!(enroll.status, 200, "{}", body_text(&enroll));
        let secret = json_of(&enroll)["secret_base32"]
            .as_str()
            .unwrap()
            .to_string();
        let verify = owner(
            state,
            "POST",
            "/api/v1/auth/totp/verify",
            &json!({ "username": username, "code": code_at(&secret, 0) }),
        );
        assert_eq!(verify.status, 200, "{}", body_text(&verify));
        secret
    }

    /// Every attempt is charged before it is checked, so a test that makes many attempts for one
    /// name clears the charge first; the lockout tests use [`totp_login_raw`].
    fn totp_login(state: &HubState, username: &str, code: Option<&str>) -> HttpResponse {
        state.login_guard.throttle().clear(username);
        totp_login_raw(state, username, code)
    }

    fn totp_login_raw(state: &HubState, username: &str, code: Option<&str>) -> HttpResponse {
        let mut body = json!({ "mode": "password", "username": username, "password": PASSWORD });
        if let Some(code) = code {
            body["totp_code"] = json!(code);
        }
        login(state, &body)
    }

    #[test]
    fn enroll_returns_the_secret_once_and_the_otpauth_uri() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let enroll = owner(&state, "POST", "/api/v1/auth/totp/enroll", &json!({}));
        assert_eq!(enroll.status, 200);
        let body = json_of(&enroll);
        let secret = body["secret_base32"].as_str().unwrap();
        assert_eq!(secret.len(), 32);
        assert_eq!(base32_decode(secret).unwrap().len(), 20);
        assert_eq!(
            body["otpauth_uri"],
            format!(
                "otpauth://totp/FerroLoop:lmcfarlin?secret={secret}&issuer=FerroLoop&algorithm=SHA1&digits=6&period=30"
            )
        );
        // Enrolling alone does not turn TOTP on.
        let list = json_of(&owner(&state, "GET", "/api/v1/auth/operators", &json!({})));
        assert_eq!(list["operators"][0]["totp_enabled"], false);
        assert_eq!(
            stored(&tmp).operators[0]
                .totp_pending_secret_base32
                .as_deref(),
            Some(secret)
        );
        assert!(stored(&tmp).operators[0].totp_secret_base32.is_none());
        // Each enrolment is a new secret.
        let second = json_of(&owner(
            &state,
            "POST",
            "/api/v1/auth/totp/enroll",
            &json!({}),
        ));
        assert_ne!(second["secret_base32"], body["secret_base32"]);
        // Unknown target, and the body is strict.
        assert_eq!(
            owner(
                &state,
                "POST",
                "/api/v1/auth/totp/enroll",
                &json!({ "username": "ghost" })
            )
            .status,
            404
        );
        assert_eq!(
            owner(
                &state,
                "POST",
                "/api/v1/auth/totp/enroll",
                &json!({ "x": 1 })
            )
            .status,
            400
        );
    }

    #[test]
    fn verify_promotes_the_pending_secret_and_rejects_a_wrong_code() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        // No secret yet.
        assert_eq!(
            owner(
                &state,
                "POST",
                "/api/v1/auth/totp/verify",
                &json!({ "code": "123456" })
            )
            .status,
            400
        );
        let enroll = json_of(&owner(
            &state,
            "POST",
            "/api/v1/auth/totp/enroll",
            &json!({}),
        ));
        let secret = enroll["secret_base32"].as_str().unwrap().to_string();
        let wrong = format!(
            "{:06}",
            (code_at(&secret, 0).parse::<u32>().unwrap() + 1) % 1_000_000
        );
        let resp = owner(
            &state,
            "POST",
            "/api/v1/auth/totp/verify",
            &json!({ "code": wrong }),
        );
        assert_eq!(resp.status, 401);
        assert!(
            !stored(&tmp).operators[0].totp_enabled,
            "a wrong code enables nothing"
        );
        assert!(stored(&tmp).operators[0].totp_secret_base32.is_none());
        assert_eq!(
            owner(
                &state,
                "POST",
                "/api/v1/auth/totp/verify",
                &json!({ "code": "abcdef" })
            )
            .status,
            401
        );
        assert_eq!(
            owner(
                &state,
                "POST",
                "/api/v1/auth/totp/verify",
                &json!({ "username": "ghost", "code": "123456" })
            )
            .status,
            404
        );
        let ok = owner(
            &state,
            "POST",
            "/api/v1/auth/totp/verify",
            &json!({ "code": code_at(&secret, 0) }),
        );
        assert_eq!(ok.status, 200);
        let file = stored(&tmp);
        assert!(file.operators[0].totp_enabled);
        assert_eq!(
            file.operators[0].totp_secret_base32.as_deref(),
            Some(secret.as_str())
        );
        assert!(file.operators[0].totp_pending_secret_base32.is_none());
        // With no pending secret, verify checks the live one (a later step, so not a replay).
        let again = owner(
            &state,
            "POST",
            "/api/v1/auth/totp/verify",
            &json!({ "code": code_at(&secret, 1) }),
        );
        assert_eq!(again.status, 200);
        assert_eq!(
            owner(
                &state,
                "POST",
                "/api/v1/auth/totp/verify",
                &json!({ "code": code_at(&secret, 1) })
            )
            .status,
            401,
            "a code verified once is spent"
        );
    }

    #[test]
    fn password_login_with_totp_requires_a_fresh_valid_code() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "tina", "owner", Some(PASSWORD));
        let secret = enable_totp(&state, "tina");

        // The password alone is not enough, and nothing is minted.
        let need = totp_login(&state, "tina", None);
        assert_eq!(need.status, 200);
        assert_eq!(
            json_of(&need),
            json!({ "ok": false, "requires_totp": true })
        );
        assert!(set_cookie(&need).is_none());
        assert_eq!(
            json_of(&totp_login(&state, "tina", Some("  "))),
            json!({ "ok": false, "requires_totp": true })
        );

        // A wrong password is a plain 401 and never says TOTP is on.
        let wrong_pw = login(
            &state,
            &json!({ "mode": "password", "username": "tina", "password": "wrong password" }),
        );
        assert!(is_invalid_credentials(&wrong_pw));
        assert!(!body_text(&wrong_pw).contains("totp"));

        // A bad code is a 401.
        let bad = format!(
            "{:06}",
            (code_at(&secret, 1).parse::<u32>().unwrap() + 7) % 1_000_000
        );
        assert!(is_invalid_credentials(&totp_login(
            &state,
            "tina",
            Some(&bad)
        )));
        assert!(is_invalid_credentials(&totp_login(
            &state,
            "tina",
            Some("12ab56")
        )));

        // The code enable_totp used (this step) is spent; the next step's code is fresh.
        assert!(is_invalid_credentials(&totp_login(
            &state,
            "tina",
            Some(&code_at(&secret, 0))
        )));
        let good = code_at(&secret, 1);
        let resp = totp_login(&state, "tina", Some(&good));
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
        assert_eq!(
            json_of(&session_via_cookie(&state, &resp))["authority"]["login"],
            "tina"
        );
        // Replaying the code that just worked is refused.
        assert!(is_invalid_credentials(&totp_login(
            &state,
            "tina",
            Some(&good)
        )));
        // A code from further out of the window than one step is refused.
        assert!(is_invalid_credentials(&totp_login(
            &state,
            "tina",
            Some(&code_at(&secret, -5))
        )));
    }

    #[test]
    fn a_step_boundary_between_enrolment_and_login_keeps_the_spent_code_spent() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "tina", "owner", Some(PASSWORD));
        // Enrol on the last second of a step: enrolment spends that step's code.
        let boundary = mid_step_now() + TOTP_STEP_SECS / 2 - 1;
        set_clock(boundary);
        let secret = enable_totp(&state, "tina");
        let spent = code_at(&secret, 0);
        // The clock crosses into the next step before the login.
        set_clock(boundary + 2);
        assert_eq!(code_at(&secret, -1), spent);
        assert!(
            is_invalid_credentials(&totp_login(&state, "tina", Some(&spent))),
            "the code enrolment used stays spent after the step rolls over"
        );
        // The new step's code is fresh, works once, and is then spent too.
        let fresh = code_at(&secret, 0);
        assert_ne!(fresh, spent);
        let resp = totp_login(&state, "tina", Some(&fresh));
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
        assert!(is_invalid_credentials(&totp_login(
            &state,
            "tina",
            Some(&fresh)
        )));
    }

    #[test]
    fn a_non_owner_with_totp_is_refused_only_after_the_code_checks_out() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "vera", "viewer", Some(PASSWORD));
        let secret = enable_totp(&state, "vera");
        // Without a code it is still asking for one; the role is not revealed yet.
        assert_eq!(
            json_of(&totp_login(&state, "vera", None)),
            json!({ "ok": false, "requires_totp": true })
        );
        assert!(is_invalid_credentials(&totp_login(
            &state,
            "vera",
            Some("000000")
        )));
        let resp = totp_login(&state, "vera", Some(&code_at(&secret, 1)));
        assert_eq!(resp.status, 403);
        assert!(set_cookie(&resp).is_none());
    }

    #[test]
    fn totp_failures_count_toward_the_lockout() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "tina", "owner", Some(PASSWORD));
        let secret = enable_totp(&state, "tina");
        // A request with a code is charged twice (the password step, then the code), so the
        // lockout lands inside the third wrong code; the fourth request is never evaluated.
        for _ in 0..3 {
            assert_eq!(totp_login_raw(&state, "tina", Some("000000")).status, 401);
        }
        let locked_out = totp_login_raw(&state, "tina", Some(&code_at(&secret, 1)));
        assert_eq!(locked_out.status, 429);
        assert!(header(&locked_out, "retry-after").is_some());
    }

    #[test]
    fn enrolling_again_replaces_only_the_pending_secret() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "tina", "owner", Some(PASSWORD));
        let live = enable_totp(&state, "tina");
        let enroll = json_of(&owner(
            &state,
            "POST",
            "/api/v1/auth/totp/enroll",
            &json!({ "username": "tina" }),
        ));
        let pending = enroll["secret_base32"].as_str().unwrap().to_string();
        let file = stored(&tmp);
        let tina = &file.operators[file.find_username("tina").unwrap()];
        assert!(tina.totp_enabled, "enrolling does not switch TOTP off");
        assert_eq!(tina.totp_secret_base32.as_deref(), Some(live.as_str()));
        assert_eq!(
            tina.totp_pending_secret_base32.as_deref(),
            Some(pending.as_str())
        );
        // The live secret still logs in.
        assert_eq!(
            totp_login(&state, "tina", Some(&code_at(&live, 1))).status,
            200
        );
        // Verifying promotes the pending one; the old one stops working.
        let promote = owner(
            &state,
            "POST",
            "/api/v1/auth/totp/verify",
            &json!({ "username": "tina", "code": code_at(&pending, 0) }),
        );
        assert_eq!(promote.status, 200);
        let file = stored(&tmp);
        let tina = &file.operators[file.find_username("tina").unwrap()];
        assert_eq!(tina.totp_secret_base32.as_deref(), Some(pending.as_str()));
        assert!(tina.totp_pending_secret_base32.is_none());
        assert!(is_invalid_credentials(&totp_login(
            &state,
            "tina",
            Some(&code_at(&live, 1))
        )));
        assert_eq!(
            totp_login(&state, "tina", Some(&code_at(&pending, 1))).status,
            200
        );
    }

    // --- passkeys -----------------------------------------------------------------------------

    fn new_keypair() -> (EcdsaKeyPair, Vec<u8>) {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let public = key.public_key().as_ref().to_vec();
        assert_eq!(public.len(), 65);
        assert_eq!(public[0], 0x04);
        (key, public)
    }

    struct Fx {
        _tmp: TempDir,
        state: HubState,
        key: EcdsaKeyPair,
        cred: String,
        username: String,
    }

    /// A hub with an operator of `role` holding one registered passkey, pinned to the origin of
    /// the registering request (`http://localhost:8420`).
    fn fx(role: &str) -> Fx {
        fx_with_origin(role, None)
    }

    /// [`fx`] with an explicit pinned origin.
    fn fx_with_origin(role: &str, origin: Option<&str>) -> Fx {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let username = format!("pk-{role}");
        assert_eq!(create_operator(&state, &username, role, None).status, 201);
        let (key, public) = new_keypair();
        let cred = URL_SAFE_NO_PAD.encode(random_array::<24>().unwrap());
        let mut reg_body = json!({
            "username": username,
            "credential_id": cred,
            "label": "test key",
            "public_key_b64": URL_SAFE_NO_PAD.encode(&public),
        });
        if let Some(origin) = origin {
            reg_body["origin"] = json!(origin);
        }
        let reg = owner(&state, "POST", "/api/v1/auth/passkey/register", &reg_body);
        assert_eq!(reg.status, 200, "{}", body_text(&reg));
        Fx {
            _tmp: tmp,
            state,
            key,
            cred,
            username,
        }
    }

    struct Opts {
        challenge: String,
        kind: &'static str,
        origin: String,
        rp_host: &'static str,
        flags: u8,
        count: u32,
    }

    fn challenge(state: &HubState) -> String {
        let resp = public(state, "/api/v1/auth/passkey/challenge", None, &json!({}));
        assert_eq!(resp.status, 200);
        let body = json_of(&resp);
        assert_eq!(body["expires_in_secs"], 300);
        body["challenge"].as_str().unwrap().to_string()
    }

    fn opts(state: &HubState, count: u32) -> Opts {
        Opts {
            challenge: challenge(state),
            kind: "webauthn.get",
            origin: format!("http://{HOST}"),
            rp_host: "localhost",
            flags: 0x01,
            count,
        }
    }

    /// The client-data bytes, authenticator data and a signature by `key` over them.
    fn assertion_parts(key: &EcdsaKeyPair, o: &Opts) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let client_data = json!({
            "type": o.kind,
            "challenge": o.challenge,
            "origin": o.origin,
            "crossOrigin": false,
        })
        .to_string()
        .into_bytes();
        let mut auth = Sha256::digest(o.rp_host.as_bytes()).to_vec();
        auth.push(o.flags);
        auth.extend_from_slice(&o.count.to_be_bytes());
        let mut signed = auth.clone();
        signed.extend_from_slice(Sha256::digest(&client_data).as_slice());
        let sig = key.sign(&SystemRandom::new(), &signed).unwrap();
        (client_data, auth, sig.as_ref().to_vec())
    }

    fn assertion_body(cred: &str, parts: &(Vec<u8>, Vec<u8>, Vec<u8>)) -> Value {
        json!({
            "mode": "passkey",
            "credential_id": cred,
            "client_data_json_b64": URL_SAFE_NO_PAD.encode(&parts.0),
            "authenticator_data_b64": URL_SAFE_NO_PAD.encode(&parts.1),
            "signature_b64": URL_SAFE_NO_PAD.encode(&parts.2),
        })
    }

    fn passkey_login(fx: &Fx, o: &Opts) -> HttpResponse {
        login(
            &fx.state,
            &assertion_body(&fx.cred, &assertion_parts(&fx.key, o)),
        )
    }

    #[test]
    fn a_valid_passkey_assertion_logs_in_the_owner_and_the_cookie_works() {
        let fx = fx("owner");
        let resp = passkey_login(&fx, &opts(&fx.state, 1));
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
        let body = json_of(&resp);
        assert_eq!(body["ok"], true);
        assert_eq!(
            body["authority"],
            json!({ "tier": "owner", "login": fx.username, "role": "owner" })
        );
        let token = body["session_token"].as_str().unwrap();
        assert!(
            set_cookie(&resp)
                .unwrap()
                .starts_with(&format!("fl_hub_session={token};"))
        );
        let session = session_via_cookie(&fx.state, &resp);
        assert_eq!(
            json_of(&session)["authority"],
            json!({ "tier": "owner", "login": fx.username })
        );
        // The counter was stored.
        let file = read_file(fx._tmp.path()).unwrap().unwrap();
        let (i, j) = file.find_credential(&fx.cred).unwrap();
        assert_eq!(file.operators[i].passkeys[j].sign_count, 1);
        // The next ceremony with a higher counter works too.
        assert_eq!(passkey_login(&fx, &opts(&fx.state, 2)).status, 200);
    }

    #[test]
    fn a_passkey_over_https_on_a_real_host_logs_in() {
        let fx = fx_with_origin("owner", Some("https://hub.example.ts.net"));
        let mut o = opts(&fx.state, 1);
        o.origin = "https://hub.example.ts.net".into();
        o.rp_host = "hub.example.ts.net";
        let body = assertion_body(&fx.cred, &assertion_parts(&fx.key, &o));
        let resp = public(
            &fx.state,
            "/api/v1/auth/login",
            Some("hub.example.ts.net"),
            &body,
        );
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
    }

    #[test]
    fn counters_that_stay_zero_are_allowed() {
        let fx = fx("owner");
        assert_eq!(passkey_login(&fx, &opts(&fx.state, 0)).status, 200);
        assert_eq!(passkey_login(&fx, &opts(&fx.state, 0)).status, 200);
    }

    fn assert_refused(_fx: &Fx, resp: &HttpResponse, what: &str) {
        assert!(
            is_invalid_credentials(resp),
            "{what}: {} {}",
            resp.status,
            body_text(resp)
        );
    }

    #[test]
    fn a_signature_by_the_wrong_key_is_refused() {
        let fx = fx("owner");
        let (other, _) = new_keypair();
        let o = opts(&fx.state, 1);
        let resp = login(
            &fx.state,
            &assertion_body(&fx.cred, &assertion_parts(&other, &o)),
        );
        assert_refused(&fx, &resp, "wrong key");
    }

    #[test]
    fn tampered_client_data_is_refused() {
        let fx = fx("owner");
        let o = opts(&fx.state, 1);
        let mut parts = assertion_parts(&fx.key, &o);
        // Still valid JSON with the right challenge, but no longer what was signed.
        parts.0.extend_from_slice(b" ");
        assert_refused(
            &fx,
            &login(&fx.state, &assertion_body(&fx.cred, &parts)),
            "tampered client data",
        );
        // Tampered authenticator data (counter bumped) and signature bytes.
        let o = opts(&fx.state, 1);
        let mut parts = assertion_parts(&fx.key, &o);
        parts.1[36] ^= 0xff;
        assert_refused(
            &fx,
            &login(&fx.state, &assertion_body(&fx.cred, &parts)),
            "tampered auth data",
        );
        let o = opts(&fx.state, 1);
        let mut parts = assertion_parts(&fx.key, &o);
        let last = parts.2.len() - 1;
        parts.2[last] ^= 0x01;
        assert_refused(
            &fx,
            &login(&fx.state, &assertion_body(&fx.cred, &parts)),
            "tampered signature",
        );
    }

    #[test]
    fn a_challenge_the_server_did_not_issue_is_refused() {
        let fx = fx("owner");
        let mut o = opts(&fx.state, 1);
        o.challenge = URL_SAFE_NO_PAD.encode([7u8; 32]);
        assert_refused(&fx, &passkey_login(&fx, &o), "unissued challenge");
    }

    #[test]
    fn a_challenge_works_once_even_with_a_valid_signature() {
        let fx = fx("owner");
        let o = opts(&fx.state, 1);
        let body = assertion_body(&fx.cred, &assertion_parts(&fx.key, &o));
        assert_eq!(login(&fx.state, &body).status, 200);
        // The identical assertion replayed.
        assert_refused(&fx, &login(&fx.state, &body), "replayed assertion");
        // The same challenge re-signed with a higher counter.
        let mut o2 = opts(&fx.state, 2);
        o2.challenge = o.challenge.clone();
        assert_refused(&fx, &passkey_login(&fx, &o2), "reused challenge");
    }

    #[test]
    fn a_failed_attempt_still_spends_the_challenge() {
        let fx = fx("owner");
        let mut bad = opts(&fx.state, 1);
        bad.rp_host = "evil.example";
        assert_refused(&fx, &passkey_login(&fx, &bad), "bad rp id");
        // The same challenge, now with a correct assertion: already burnt.
        let mut good = opts(&fx.state, 1);
        good.challenge = bad.challenge.clone();
        assert_refused(&fx, &passkey_login(&fx, &good), "burnt challenge");
    }

    #[test]
    fn an_expired_challenge_is_refused() {
        let fx = fx("owner");
        let o = opts(&fx.state, 1);
        for expires in fx.state.login_guard.challenges().live.values_mut() {
            *expires = Instant::now();
        }
        assert_refused(&fx, &passkey_login(&fx, &o), "expired challenge");
    }

    #[test]
    fn the_wrong_origin_type_or_rp_id_is_refused() {
        type Tweak = Box<dyn Fn(&mut Opts)>;
        let cases: [(&str, Tweak); 7] = [
            (
                "evil origin",
                Box::new(|o| o.origin = "http://evil.example:8420".into()),
            ),
            (
                "wrong port",
                Box::new(|o| o.origin = "http://localhost:9999".into()),
            ),
            ("empty origin", Box::new(|o| o.origin = String::new())),
            ("create ceremony", Box::new(|o| o.kind = "webauthn.create")),
            ("wrong rp id", Box::new(|o| o.rp_host = "evil.example")),
            ("no user presence", Box::new(|o| o.flags = 0x04)),
            ("flags clear", Box::new(|o| o.flags = 0x00)),
        ];
        // Five failures lock a name out, so each case gets its own hub.
        for (what, tweak) in cases {
            let fx = fx("owner");
            let mut o = opts(&fx.state, 1);
            tweak(&mut o);
            assert_refused(&fx, &passkey_login(&fx, &o), what);
            // The same key and counter, untampered, would have worked.
            assert_eq!(
                passkey_login(&fx, &opts(&fx.state, 1)).status,
                200,
                "{what}: control"
            );
        }
    }

    #[test]
    fn the_login_ignores_the_host_header_and_uses_the_pinned_origin() {
        let fx = fx("owner");
        // Whatever the login request's Host says, or does not say, the stored origin decides.
        for host in [
            None,
            Some(""),
            Some("not a host"),
            Some("a/b"),
            Some("evil.example"),
        ] {
            let body = assertion_body(&fx.cred, &assertion_parts(&fx.key, &opts(&fx.state, 0)));
            let resp = public(&fx.state, "/api/v1/auth/login", host, &body);
            assert_eq!(resp.status, 200, "{host:?}: {}", body_text(&resp));
        }
    }

    #[test]
    fn an_assertion_for_another_origin_is_refused_even_when_host_is_spoofed_to_match() {
        // The key and the rp id match; only the origin differs from the pinned one.
        for (what, origin, rp_host, host) in [
            ("other port", "http://localhost:9999", "localhost", HOST),
            (
                "other port, Host spoofed to it",
                "http://localhost:9999",
                "localhost",
                "localhost:9999",
            ),
            (
                "other host, Host spoofed to it",
                "https://evil.example",
                "evil.example",
                "evil.example",
            ),
            (
                "https where http is pinned, Host spoofed",
                "https://localhost:8420",
                "localhost",
                HOST,
            ),
            (
                "right origin, other rp id",
                "http://localhost:8420",
                "evil.example",
                HOST,
            ),
        ] {
            let fx = fx("owner");
            let mut o = opts(&fx.state, 1);
            o.origin = origin.into();
            o.rp_host = rp_host;
            let body = assertion_body(&fx.cred, &assertion_parts(&fx.key, &o));
            let resp = public(&fx.state, "/api/v1/auth/login", Some(host), &body);
            assert_refused(&fx, &resp, what);
        }
    }

    #[test]
    fn the_pinned_origin_is_exact_including_port_and_works_from_any_host_header() {
        let fx = fx_with_origin("owner", Some("https://hub.example.ts.net:8443"));
        let mut o = opts(&fx.state, 1);
        o.origin = "https://hub.example.ts.net".into();
        o.rp_host = "hub.example.ts.net";
        let no_port = assertion_body(&fx.cred, &assertion_parts(&fx.key, &o));
        assert_refused(
            &fx,
            &public(
                &fx.state,
                "/api/v1/auth/login",
                Some("hub.example.ts.net"),
                &no_port,
            ),
            "port dropped",
        );
        // The same credential from its pinned origin, with a different Host header.
        let mut o = opts(&fx.state, 1);
        o.origin = "https://hub.example.ts.net:8443".into();
        o.rp_host = "hub.example.ts.net";
        let ok = assertion_body(&fx.cred, &assertion_parts(&fx.key, &o));
        let resp = public(
            &fx.state,
            "/api/v1/auth/login",
            Some("proxy.internal:3000"),
            &ok,
        );
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
    }

    #[test]
    fn a_credential_without_a_pinned_origin_is_refused() {
        let fx = fx("owner");
        // Remove the field from the file, as one written before origins were pinned has it.
        let path = file_path(&fx._tmp);
        let mut raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for op in raw["operators"].as_array_mut().unwrap() {
            for key in op["passkeys"].as_array_mut().unwrap() {
                assert!(key.as_object_mut().unwrap().remove("origin").is_some());
            }
        }
        fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        let old = stored(&fx._tmp);
        assert!(
            old.operators
                .iter()
                .flat_map(|o| &o.passkeys)
                .all(|k| k.origin.is_none())
        );
        let resp = passkey_login(&fx, &opts(&fx.state, 1));
        assert_refused(&fx, &resp, "legacy credential");
    }

    #[test]
    fn registration_pins_the_origin_from_the_body_or_the_request_and_validates_it() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let (_, public_key) = new_keypair();
        let register_with = |cred: &str, origin: Option<&str>, host: Option<&str>| {
            let mut body = json!({
                "credential_id": cred,
                "public_key_b64": URL_SAFE_NO_PAD.encode(&public_key),
            });
            if let Some(origin) = origin {
                body["origin"] = json!(origin);
            }
            let mut req = HttpRequest::new("POST", "/api/v1/auth/passkey/register")
                .with_header("Authorization", format!("Bearer {OWNER_TOKEN}"))
                .with_json_body(&body);
            if let Some(host) = host {
                req = req.with_header("Host", host);
            }
            handle_http_request(&state, &req)
        };
        let origin_of = |cred: &str| {
            let file = stored(&tmp);
            let (i, j) = file.find_credential(cred).unwrap();
            file.operators[i].passkeys[j].origin.clone()
        };
        assert_eq!(
            register_with("cred-from-host-aaaa1", None, Some("localhost:8420")).status,
            200
        );
        assert_eq!(
            origin_of("cred-from-host-aaaa1").as_deref(),
            Some("http://localhost:8420")
        );
        assert_eq!(
            register_with("cred-from-host-aaaa2", None, Some("hub.example.ts.net")).status,
            200
        );
        assert_eq!(
            origin_of("cred-from-host-aaaa2").as_deref(),
            Some("https://hub.example.ts.net")
        );
        // The body wins over the Host header.
        assert_eq!(
            register_with(
                "cred-from-body-aaaa3",
                Some("HTTPS://Hub.Example:8443"),
                Some("localhost:1")
            )
            .status,
            200
        );
        assert_eq!(
            origin_of("cred-from-body-aaaa3").as_deref(),
            Some("https://hub.example:8443")
        );
        // The listing shows it; it is not a secret.
        let list = json_of(&owner(&state, "GET", "/api/v1/auth/operators", &json!({})));
        assert!(
            body_text(&owner(&state, "GET", "/api/v1/auth/operators", &json!({})))
                .contains("https://hub.example:8443")
        );
        assert_eq!(
            list["operators"][0]["passkeys"].as_array().unwrap().len(),
            3
        );
        // Refused: no origin to pin, plain http off loopback, junk.
        for (cred, origin, host) in [
            ("cred-refused-aaaaa1", None, None),
            ("cred-refused-aaaaa2", None, Some("a b")),
            (
                "cred-refused-aaaaa3",
                Some("http://hub.example.ts.net"),
                Some("localhost:8420"),
            ),
            (
                "cred-refused-aaaaa4",
                Some("hub.example.ts.net"),
                Some("localhost:8420"),
            ),
            (
                "cred-refused-aaaaa5",
                Some("https://hub.example.ts.net/x"),
                Some("localhost:8420"),
            ),
            ("cred-refused-aaaaa6", Some(""), Some("localhost:8420")),
        ] {
            let resp = register_with(cred, origin, host);
            assert_eq!(resp.status, 400, "{cred}: {}", body_text(&resp));
            assert!(stored(&tmp).find_credential(cred).is_none());
        }
    }

    #[test]
    fn replayed_or_regressed_sign_counts_are_refused() {
        let fx = fx("owner");
        assert_eq!(passkey_login(&fx, &opts(&fx.state, 5)).status, 200);
        // Same count, lower count, and a drop to zero all fail the strict increase.
        for (what, count) in [("same", 5u32), ("lower", 3), ("zero", 0)] {
            let resp = passkey_login(&fx, &opts(&fx.state, count));
            if resp.status == 429 {
                break;
            }
            assert_refused(&fx, &resp, what);
        }
        // The refused attempts did not move the stored count.
        let file = read_file(fx._tmp.path()).unwrap().unwrap();
        let (i, j) = file.find_credential(&fx.cred).unwrap();
        assert_eq!(file.operators[i].passkeys[j].sign_count, 5);
    }

    #[test]
    fn a_higher_count_after_a_refusal_still_works() {
        let fx = fx("owner");
        assert_eq!(passkey_login(&fx, &opts(&fx.state, 5)).status, 200);
        assert_refused(&fx, &passkey_login(&fx, &opts(&fx.state, 4)), "regressed");
        assert_eq!(passkey_login(&fx, &opts(&fx.state, 6)).status, 200);
    }

    // --- passkey login with TOTP on -------------------------------------------------------------

    /// A passkey login body carrying `code` as its second factor.
    fn passkey_totp_body(fx: &Fx, count: u32, code: Option<&str>) -> Value {
        let mut body = assertion_body(&fx.cred, &assertion_parts(&fx.key, &opts(&fx.state, count)));
        if let Some(code) = code {
            body["totp_code"] = json!(code);
        }
        body
    }

    fn passkey_totp_login(fx: &Fx, count: u32, code: Option<&str>) -> HttpResponse {
        login(&fx.state, &passkey_totp_body(fx, count, code))
    }

    /// The stored passkey counter and the stored last spent TOTP step.
    fn passkey_state(fx: &Fx) -> (u32, Option<u64>) {
        let file = stored(&fx._tmp);
        let (i, j) = file.find_credential(&fx.cred).unwrap();
        (
            file.operators[i].passkeys[j].sign_count,
            file.operators[i].last_totp_step,
        )
    }

    #[test]
    fn a_passkey_login_without_the_code_is_refused_and_spends_nothing() {
        let fx = fx("owner");
        let secret = enable_totp(&fx.state, &fx.username);
        let before = passkey_state(&fx);
        for code in [None, Some(""), Some("   ")] {
            fx.state.login_guard.throttle().clear(&fx.username);
            let resp = passkey_totp_login(&fx, 1, code);
            assert_eq!(resp.status, 401, "{}", body_text(&resp));
            assert_eq!(
                json_of(&resp),
                json!({
                    "ok": false,
                    "requires_totp": true,
                    "error": "TOTP code is required for this account",
                })
            );
            assert!(set_cookie(&resp).is_none());
            assert!(!body_text(&resp).contains("session_token"));
        }
        // Neither the counter nor the spent step moved, so the same count still logs in.
        assert_eq!(passkey_state(&fx), before);
        fx.state.login_guard.throttle().clear(&fx.username);
        let resp = passkey_totp_login(&fx, 1, Some(&code_at(&secret, 1)));
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
    }

    #[test]
    fn a_passkey_login_with_a_wrong_code_is_refused_and_spends_nothing() {
        let fx = fx("owner");
        let secret = enable_totp(&fx.state, &fx.username);
        let before = passkey_state(&fx);
        let good = code_at(&secret, 1);
        let bad = format!("{:06}", (good.parse::<u32>().unwrap() + 7) % 1_000_000);
        for wrong in [bad.as_str(), "12ab56", "12345"] {
            fx.state.login_guard.throttle().clear(&fx.username);
            let resp = passkey_totp_login(&fx, 1, Some(wrong));
            assert!(is_invalid_credentials(&resp), "{}", body_text(&resp));
            assert!(
                !body_text(&resp).contains("totp"),
                "a wrong code reveals nothing"
            );
        }
        // A non-string and an oversized code are wrong codes, not missing ones.
        for junk in [json!(123456), json!(["1"]), json!("1".repeat(64))] {
            fx.state.login_guard.throttle().clear(&fx.username);
            let mut body = passkey_totp_body(&fx, 1, None);
            body["totp_code"] = junk;
            assert!(is_invalid_credentials(&login(&fx.state, &body)));
        }
        assert_eq!(passkey_state(&fx), before);
        // The right code with the same counter still works: nothing was burnt.
        fx.state.login_guard.throttle().clear(&fx.username);
        assert_eq!(passkey_totp_login(&fx, 1, Some(&good)).status, 200);
    }

    #[test]
    fn wrong_passkey_totp_codes_lock_the_name_like_password_ones() {
        let fx = fx("owner");
        let secret = enable_totp(&fx.state, &fx.username);
        let good = code_at(&secret, 1);
        // Each attempt is charged twice (the assertion, then the code), the same as the password
        // path, so the lockout lands inside the third wrong code.
        for _ in 0..3 {
            assert_eq!(passkey_totp_login(&fx, 1, Some("12ab56")).status, 401);
        }
        let locked_out = passkey_totp_login(&fx, 1, Some(&good));
        assert_eq!(locked_out.status, 429);
        assert!(header(&locked_out, "retry-after").is_some());
        assert_eq!(
            passkey_state(&fx).0,
            0,
            "no refused attempt spent the counter"
        );
        // The bucket is the operator's name, the one the password path charges.
        let pw = login(
            &fx.state,
            &json!({ "mode": "password", "username": fx.username, "password": PASSWORD }),
        );
        assert_eq!(pw.status, 429);
    }

    #[test]
    fn a_passkey_login_with_the_right_code_spends_the_step_and_the_counter() {
        let fx = fx("owner");
        let secret = enable_totp(&fx.state, &fx.username);
        let step = now_unix_secs() / TOTP_STEP_SECS;
        let good = code_at(&secret, 1);
        let resp = passkey_totp_login(&fx, 4, Some(&good));
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
        assert_eq!(
            json_of(&session_via_cookie(&fx.state, &resp))["authority"]["login"],
            fx.username
        );
        assert_eq!(passkey_state(&fx), (4, Some(step + 1)));
        // The code is spent: a fresh assertion with a higher counter cannot reuse it.
        fx.state.login_guard.throttle().clear(&fx.username);
        let replay = passkey_totp_login(&fx, 5, Some(&good));
        assert!(is_invalid_credentials(&replay), "{}", body_text(&replay));
        // An older step's code is refused as well, and neither refusal moved the counter.
        fx.state.login_guard.throttle().clear(&fx.username);
        let older = passkey_totp_login(&fx, 5, Some(&code_at(&secret, -1)));
        assert!(is_invalid_credentials(&older));
        assert_eq!(passkey_state(&fx), (4, Some(step + 1)));
    }

    #[test]
    fn a_replayed_passkey_assertion_is_refused_even_with_a_fresh_code() {
        let fx = fx("owner");
        let secret = enable_totp(&fx.state, &fx.username);
        let body = passkey_totp_body(&fx, 2, Some(&code_at(&secret, 1)));
        assert_eq!(login(&fx.state, &body).status, 200);
        // The identical request again: its challenge is spent and so is its code.
        fx.state.login_guard.throttle().clear(&fx.username);
        assert!(is_invalid_credentials(&login(&fx.state, &body)));
        // A new challenge but a counter that did not advance fails the strict increase before
        // the code is even looked at.
        fx.state.login_guard.throttle().clear(&fx.username);
        let stale = passkey_totp_login(&fx, 2, Some(&code_at(&secret, 1)));
        assert!(is_invalid_credentials(&stale));
        assert_eq!(passkey_state(&fx).0, 2);
    }

    #[test]
    fn two_concurrent_passkey_logins_with_one_code_let_exactly_one_in() {
        for round in 0..20 {
            let fx = fx("owner");
            let secret = enable_totp(&fx.state, &fx.username);
            let code = code_at(&secret, 1);
            // Two distinct, valid assertions (own challenge, own counter) and the one code.
            let bodies = [
                passkey_totp_body(&fx, 1, Some(&code)),
                passkey_totp_body(&fx, 2, Some(&code)),
            ];
            let clock = now_unix_secs();
            let gate = std::sync::Barrier::new(2);
            let statuses: Vec<u16> = std::thread::scope(|scope| {
                let handles: Vec<_> = bodies
                    .iter()
                    .map(|body| {
                        let (state, gate) = (&fx.state, &gate);
                        scope.spawn(move || {
                            set_clock(clock);
                            gate.wait();
                            login(state, body).status
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let ok = statuses.iter().filter(|s| **s == 200).count();
            assert_eq!(ok, 1, "round {round}: {statuses:?}");
            assert!(statuses.contains(&401), "round {round}: {statuses:?}");
        }
    }

    #[test]
    fn a_passkey_login_for_an_account_without_totp_ignores_a_stray_code() {
        let fx = fx("owner");
        let resp = passkey_totp_login(&fx, 1, Some("000000"));
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
        assert_eq!(passkey_state(&fx), (1, None));
    }

    #[test]
    fn an_unknown_credential_and_a_bare_credential_id_are_refused() {
        let fx = fx("owner");
        let o = opts(&fx.state, 1);
        let body = assertion_body("unknown-credential-id-123", &assertion_parts(&fx.key, &o));
        assert_refused(&fx, &login(&fx.state, &body), "unknown credential");
        // A credential id with no assertion is never accepted.
        for body in [
            json!({ "mode": "passkey", "credential_id": fx.cred }),
            json!({ "mode": "passkey" }),
            json!({ "mode": "passkey", "credential_id": fx.cred, "client_data_json_b64": "", "authenticator_data_b64": "", "signature_b64": "" }),
            json!({ "mode": "passkey", "credential_id": fx.cred, "client_data_json_b64": 5, "authenticator_data_b64": [], "signature_b64": null }),
        ] {
            let resp = login(&fx.state, &body);
            assert_refused(&fx, &resp, &body.to_string());
        }
    }

    #[test]
    fn passkey_failures_are_throttled_whether_or_not_the_credential_exists() {
        for known in [true, false] {
            let fx = fx("owner");
            let cred = if known {
                fx.cred.clone()
            } else {
                "no-such-credential-1234".to_string()
            };
            let bare = json!({ "mode": "passkey", "credential_id": cred });
            for i in 0..5 {
                assert_eq!(login(&fx.state, &bare).status, 401, "known={known} {i}");
            }
            let locked = login(&fx.state, &bare);
            assert_eq!(locked.status, 429, "known={known}");
            assert_eq!(header(&locked, "Retry-After").as_deref(), Some("60"));
            if known {
                // The lock is on the owner's name: a perfect assertion is refused too.
                assert_eq!(passkey_login(&fx, &opts(&fx.state, 1)).status, 429);
            }
        }
    }

    #[test]
    fn a_passkey_for_a_role_without_authority_is_403_and_mints_nothing() {
        for role in ["admin", "operator", "viewer"] {
            let fx = fx(role);
            let resp = passkey_login(&fx, &opts(&fx.state, 1));
            assert_eq!(resp.status, 403, "{role}");
            assert_eq!(
                json_of(&resp),
                json!({ "ok": false, "error": "this role cannot log in until RBAC enforcement exists" })
            );
            assert!(set_cookie(&resp).is_none(), "{role}");
        }
    }

    #[test]
    fn the_challenge_endpoint_is_public_and_rate_limited() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let a = challenge(&state);
        let b = challenge(&state);
        assert_ne!(a, b);
        assert_eq!(URL_SAFE_NO_PAD.decode(&a).unwrap().len(), 32);
        assert_eq!(state.login_guard.challenges().len(), 2);
        for _ in 2..MAX_CHALLENGES_PER_MINUTE {
            assert_eq!(
                public(&state, "/api/v1/auth/passkey/challenge", None, &json!({})).status,
                200
            );
        }
        let over = public(&state, "/api/v1/auth/passkey/challenge", None, &json!({}));
        assert_eq!(over.status, 429);
        assert!(header(&over, "Retry-After").is_some());
        // The cap on live challenges holds with the rate limit in place.
        assert!(state.login_guard.challenges().len() <= MAX_LIVE_CHALLENGES);
        // GET is not a challenge request.
        let get = handle_http_request(
            &state,
            &HttpRequest::new("GET", "/api/v1/auth/passkey/challenge"),
        );
        assert_eq!(get.status, 401);
    }

    // --- passkey registration -----------------------------------------------------------------

    fn register(state: &HubState, body: Value) -> HttpResponse {
        owner(state, "POST", "/api/v1/auth/passkey/register", &body)
    }

    #[test]
    fn registration_validates_the_key_and_the_credential_id() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let (_, public) = new_keypair();
        let cred = |n: usize| "c".repeat(n);
        let ok = |id: String, key: String| json!({ "credential_id": id, "label": "k", "public_key_b64": key });
        assert_eq!(
            register(&state, ok(cred(20), URL_SAFE_NO_PAD.encode(&public))).status,
            200
        );
        // Standard base64, padded or not, is accepted and stored as base64url.
        let std_padded = base64::engine::general_purpose::STANDARD.encode(&public);
        assert_eq!(register(&state, ok(cred(21), std_padded)).status, 200);
        assert_eq!(
            register(&state, ok(cred(22), STANDARD_NO_PAD.encode(&public))).status,
            200
        );
        assert_eq!(
            register(&state, ok(cred(16), URL_SAFE_NO_PAD.encode(&public))).status,
            200
        );
        assert_eq!(
            register(&state, ok(cred(1024), URL_SAFE_NO_PAD.encode(&public))).status,
            200
        );
        for key in &stored(&tmp).operators[0].passkeys {
            assert_eq!(key.public_key_b64, URL_SAFE_NO_PAD.encode(&public));
        }
        let mut compressed = public.clone();
        compressed[0] = 0x02;
        for (what, id, key) in [
            ("short id", cred(15), URL_SAFE_NO_PAD.encode(&public)),
            ("long id", cred(1025), URL_SAFE_NO_PAD.encode(&public)),
            (
                "bad id chars",
                "has space in it 12345".to_string(),
                URL_SAFE_NO_PAD.encode(&public),
            ),
            (
                "padded id",
                format!("{}==", cred(20)),
                URL_SAFE_NO_PAD.encode(&public),
            ),
            ("empty key", cred(30), String::new()),
            (
                "64-byte key",
                cred(31),
                URL_SAFE_NO_PAD.encode(&public[..64]),
            ),
            (
                "66-byte key",
                cred(32),
                URL_SAFE_NO_PAD.encode([&public[..], &[0u8]].concat()),
            ),
            (
                "compressed prefix",
                cred(33),
                URL_SAFE_NO_PAD.encode(&compressed),
            ),
            ("not base64", cred(34), "!!!not base64!!!".to_string()),
            (
                "mixed alphabets",
                cred(35),
                format!("{}+_", URL_SAFE_NO_PAD.encode(&public[..40])),
            ),
            (
                "COSE-like map",
                cred(36),
                URL_SAFE_NO_PAD.encode([0xa5u8, 0x01, 0x02, 0x03, 0x26]),
            ),
        ] {
            assert_eq!(register(&state, ok(id, key)).status, 400, "{what}");
        }
        // A missing key is an error, never a "trust me" registration.
        assert_eq!(
            register(&state, json!({ "credential_id": cred(40), "label": "k" })).status,
            400
        );
        assert_eq!(
            register(
                &state,
                json!({ "credential_id": cred(41), "public_key_b64": null })
            )
            .status,
            400
        );
        assert_eq!(
            register(&state, json!({ "credential_id": cred(42), "public_key_b64": URL_SAFE_NO_PAD.encode(&public), "label": "a\u{0}b" })).status,
            400
        );
        assert_eq!(
            register(&state, json!({ "credential_id": cred(43), "public_key_b64": URL_SAFE_NO_PAD.encode(&public), "username": "ghost" })).status,
            404
        );
        assert_eq!(stored(&tmp).operators[0].passkeys.len(), 5);
    }

    #[test]
    fn a_credential_id_is_unique_across_operators_and_capped_per_operator() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "other", "viewer", None);
        let (_, public) = new_keypair();
        let body = |id: String, user: &str| json!({ "credential_id": id, "label": "k", "public_key_b64": URL_SAFE_NO_PAD.encode(&public), "username": user });
        let first = register(&state, body("shared-credential-id-1".into(), "lmcfarlin"));
        assert_eq!(first.status, 200);
        assert_eq!(json_of(&first), json!({ "ok": true, "passkeys_count": 1 }));
        assert_eq!(
            register(&state, body("shared-credential-id-1".into(), "lmcfarlin")).status,
            409
        );
        assert_eq!(
            register(&state, body("shared-credential-id-1".into(), "other")).status,
            409
        );
        for n in 1..MAX_PASSKEYS_PER_OPERATOR {
            let resp = register(
                &state,
                body(format!("credential-number-{n:04}"), "lmcfarlin"),
            );
            assert_eq!(resp.status, 200, "{n}");
            assert_eq!(json_of(&resp)["passkeys_count"], n + 1);
        }
        assert_eq!(
            register(&state, body("one-credential-too-many".into(), "lmcfarlin")).status,
            409
        );
        // Another operator has its own allowance.
        assert_eq!(
            register(&state, body("credential-for-other-0".into(), "other")).status,
            200
        );
        // With no username it goes to the calling owner's own record.
        let file = stored(&tmp);
        assert_eq!(
            file.operators[file.find_username("lmcfarlin").unwrap()]
                .passkeys
                .len(),
            10
        );
    }

    #[test]
    fn totp_and_passkey_default_to_the_callers_own_record_then_the_seeded_owner() {
        let tmp = TempDir::new().unwrap();
        let state = state_for(&tmp, "boss");
        // The owner token acts as the configured login "boss": it has a record (the seed).
        let enroll = owner(&state, "POST", "/api/v1/auth/totp/enroll", &json!({}));
        assert_eq!(enroll.status, 200);
        assert!(
            json_of(&enroll)["otpauth_uri"]
                .as_str()
                .unwrap()
                .contains("FerroLoop:boss?")
        );
        // A Tailscale-style owner with no record falls back to the seeded owner.
        let ts_state = {
            let mut config = state.config.clone();
            config.trust_tailscale_headers = true;
            HubState::new(config)
        };
        let resp = handle_http_request(
            &ts_state,
            &HttpRequest::new("POST", "/api/v1/auth/totp/enroll")
                .with_header("Tailscale-User-Login", "stranger@example.com")
                .with_json_body(&json!({})),
        );
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
        assert!(
            json_of(&resp)["otpauth_uri"]
                .as_str()
                .unwrap()
                .contains("FerroLoop:boss?")
        );
    }

    // --- throttling under concurrency (charge before check) -----------------------------------

    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn set_hook(state: &HubState, hook: impl Fn() + Send + Sync + 'static) {
        *state
            .login_guard
            .verify_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(hook));
    }

    fn clear_hook(state: &HubState) {
        *state
            .login_guard
            .verify_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn evaluated(state: &HubState) -> usize {
        state.login_guard.verifications.load(Ordering::SeqCst)
    }

    #[test]
    fn a_burst_of_concurrent_logins_for_one_name_evaluates_at_most_five() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "burst", "owner", Some(PASSWORD));
        // A slow verification, so requests overlap the way 600k-round PBKDF2 makes them.
        set_hook(&state, || std::thread::sleep(Duration::from_millis(15)));
        let refused = AtomicUsize::new(0);
        let throttled_count = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..64 {
                scope.spawn(|| {
                    for _ in 0..40 {
                        let resp = password_login(&state, "burst", "not the password");
                        match resp.status {
                            401 => refused.fetch_add(1, Ordering::SeqCst),
                            429 => throttled_count.fetch_add(1, Ordering::SeqCst),
                            other => panic!("unexpected status {other}"),
                        };
                        std::thread::sleep(Duration::from_millis(1));
                    }
                });
            }
        });
        let seen = evaluated(&state);
        assert!(
            (1..=MAX_FAILURES as usize).contains(&seen),
            "{seen} guesses were evaluated; the limit is {MAX_FAILURES}"
        );
        assert_eq!(
            refused.load(Ordering::SeqCst),
            seen,
            "every 401 was an evaluation"
        );
        assert!(throttled_count.load(Ordering::SeqCst) > 0);
        // The right password is refused too while the name is locked, and evaluates nothing.
        assert_eq!(password_login(&state, "burst", PASSWORD).status, 429);
        assert_eq!(evaluated(&state), seen);
    }

    #[test]
    fn sequential_guesses_stop_being_evaluated_after_five() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "seq", "owner", Some(PASSWORD));
        for i in 0..5 {
            assert!(
                is_invalid_credentials(&password_login(&state, "seq", "wrong password")),
                "{i}"
            );
        }
        assert_eq!(evaluated(&state), 5);
        for _ in 0..3 {
            let resp = password_login(&state, "seq", "wrong password");
            assert_eq!(resp.status, 429);
            assert!(header(&resp, "retry-after").is_some());
        }
        assert_eq!(evaluated(&state), 5, "a locked name is never evaluated");
    }

    #[test]
    fn a_burst_cannot_exceed_the_global_inflight_cap() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        {
            let (current, peak) = (current.clone(), peak.clone());
            set_hook(&state, move || {
                let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                current.fetch_sub(1, Ordering::SeqCst);
            });
        }
        let busy_seen = AtomicUsize::new(0);
        // Distinct names, so no per-name limit gets in the way: only the cap can hold them back.
        std::thread::scope(|scope| {
            for i in 0..48 {
                let (state, busy_seen) = (&state, &busy_seen);
                scope.spawn(move || {
                    let resp = password_login(state, &format!("name{i}"), "wrong password");
                    match resp.status {
                        401 => {}
                        429 => {
                            assert_eq!(header(&resp, "retry-after").as_deref(), Some("1"));
                            busy_seen.fetch_add(1, Ordering::SeqCst);
                        }
                        other => panic!("unexpected status {other}"),
                    }
                });
            }
        });
        let peak = peak.load(Ordering::SeqCst);
        assert!(peak <= MAX_INFLIGHT_VERIFICATIONS, "peak {peak}");
        assert!(peak >= 2, "the burst did overlap (peak {peak})");
        assert!(busy_seen.load(Ordering::SeqCst) > 0, "the overflow got 429");
        assert_eq!(
            state.login_guard.inflight.load(Ordering::SeqCst),
            0,
            "every slot came back"
        );
        // A busy answer charged nothing: only the evaluated attempts are on any name.
        assert_eq!(
            state.login_guard.throttle().tracked_users(),
            evaluated(&state)
        );
    }

    #[test]
    fn a_verification_slot_is_released_on_every_path_including_a_panic() {
        let guard = LoginGuard::default();
        let permits: Vec<_> = (0..MAX_INFLIGHT_VERIFICATIONS)
            .map(|_| guard.try_verify_permit().expect("a free slot"))
            .collect();
        assert!(
            guard.try_verify_permit().is_none(),
            "never waits, never over the cap"
        );
        drop(permits);
        assert!(guard.try_verify_permit().is_some());

        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "boom", "owner", Some(PASSWORD));
        set_hook(&state, || panic!("verifier blew up"));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            password_login(&state, "boom", "wrong password")
        }));
        assert!(outcome.is_err());
        assert_eq!(state.login_guard.inflight.load(Ordering::SeqCst), 0);
        clear_hook(&state);
        assert_eq!(password_login(&state, "boom", PASSWORD).status, 200);
    }

    #[test]
    fn a_successful_login_clears_the_charged_attempts_even_on_the_last_one() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "clara", "owner", Some(PASSWORD));
        for _ in 0..4 {
            assert!(is_invalid_credentials(&password_login(
                &state,
                "clara",
                "wrong password"
            )));
        }
        assert_eq!(
            state.login_guard.throttle().tracked_users(),
            1,
            "four attempts are charged"
        );
        // The fifth attempt is admitted (and charged, which locks the name), then it succeeds.
        assert_eq!(password_login(&state, "clara", PASSWORD).status, 200);
        assert_eq!(
            state.login_guard.throttle().tracked_users(),
            0,
            "the success cleared them"
        );
        for _ in 0..4 {
            assert!(is_invalid_credentials(&password_login(
                &state,
                "clara",
                "wrong password"
            )));
        }
    }

    #[test]
    fn a_burst_of_passkey_assertions_for_one_credential_evaluates_at_most_five() {
        let fx = fx("owner");
        let (other_key, _) = new_keypair();
        // Fresh challenges and a signature by the wrong key; the retries reuse a body whose
        // challenge the first request spent.
        let bodies: Vec<Value> = (0..16)
            .map(|_| {
                let o = opts(&fx.state, 1);
                assertion_body(&fx.cred, &assertion_parts(&other_key, &o))
            })
            .collect();
        set_hook(&fx.state, || std::thread::sleep(Duration::from_millis(10)));
        let refused = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for body in &bodies {
                let (state, refused) = (&fx.state, &refused);
                scope.spawn(move || {
                    for _ in 0..30 {
                        match login(state, body).status {
                            401 => refused.fetch_add(1, Ordering::SeqCst),
                            429 => 0,
                            other => panic!("unexpected status {other}"),
                        };
                        std::thread::sleep(Duration::from_millis(1));
                    }
                });
            }
        });
        let seen = evaluated(&fx.state);
        assert!(seen <= MAX_FAILURES as usize, "{seen} assertions evaluated");
        assert!(refused.load(Ordering::SeqCst) <= MAX_FAILURES as usize);
        // The good assertion is refused now too: the credential's owner is locked.
        assert_eq!(passkey_login(&fx, &opts(&fx.state, 1)).status, 429);
    }

    #[test]
    fn a_burst_of_totp_verifications_for_one_operator_evaluates_at_most_five() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "tess", "owner", None);
        let enroll = owner(
            &state,
            "POST",
            "/api/v1/auth/totp/enroll",
            &json!({ "username": "tess" }),
        );
        assert_eq!(enroll.status, 200);
        let refused = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..32 {
                scope.spawn(|| {
                    for _ in 0..10 {
                        let resp = owner(
                            &state,
                            "POST",
                            "/api/v1/auth/totp/verify",
                            &json!({ "username": "tess", "code": "000000" }),
                        );
                        match resp.status {
                            401 => refused.fetch_add(1, Ordering::SeqCst),
                            429 => 0,
                            other => panic!("unexpected status {other}"),
                        };
                    }
                });
            }
        });
        assert!(refused.load(Ordering::SeqCst) <= MAX_FAILURES as usize);
        assert!(refused.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn passkey_logins_are_turned_away_by_the_global_limit_before_any_file_is_read() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        {
            let mut throttle = state.login_guard.throttle();
            let now = Instant::now();
            for i in 0..MAX_GLOBAL_FAILURES_PER_MINUTE {
                throttle.charge(&format!("flood-{i}"), now);
            }
        }
        assert!(!file_path(&tmp).exists());
        let resp = login(
            &state,
            &json!({ "mode": "passkey", "credential_id": "x".repeat(24) }),
        );
        assert_eq!(resp.status, 429);
        assert!(header(&resp, "retry-after").is_some());
        // Reading the file would have seeded it.
        assert!(
            !file_path(&tmp).exists(),
            "a throttled request did no file IO"
        );
    }

    // --- session revocation -------------------------------------------------------------------

    fn session_status(state: &HubState, resp: &HttpResponse) -> u16 {
        session_via_cookie(state, resp).status
    }

    fn bearer_session_status(state: &HubState, resp: &HttpResponse) -> u16 {
        let token = json_of(resp)["session_token"].as_str().unwrap().to_string();
        handle_http_request(
            state,
            &HttpRequest::new("GET", "/api/v1/auth/session")
                .with_header("Authorization", format!("Bearer {token}")),
        )
        .status
    }

    fn logged_in_owner(state: &HubState, name: &str) -> HttpResponse {
        assert_eq!(
            create_operator(state, name, "owner", Some(PASSWORD)).status,
            201
        );
        let resp = password_login(state, name, PASSWORD);
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
        assert_eq!(session_status(state, &resp), 200);
        resp
    }

    #[test]
    fn deleting_an_owner_revokes_that_operators_sessions_only() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let alpha = logged_in_owner(&state, "alpha");
        let alpha_again = password_login(&state, "ALPHA", PASSWORD);
        let beta = logged_in_owner(&state, "beta");
        assert_eq!(
            owner(&state, "DELETE", "/api/v1/auth/operators/alpha", &json!({})).status,
            200
        );
        assert_eq!(session_status(&state, &alpha), 401);
        assert_eq!(
            bearer_session_status(&state, &alpha),
            401,
            "the token form too"
        );
        assert_eq!(
            session_status(&state, &alpha_again),
            401,
            "every session of the login"
        );
        assert_eq!(
            session_status(&state, &beta),
            200,
            "another operator is untouched"
        );
        // And the deleted operator cannot log in again.
        assert!(is_invalid_credentials(&password_login(
            &state, "alpha", PASSWORD
        )));
    }

    #[test]
    fn an_owner_session_that_deletes_another_owner_stays_valid() {
        // The caller of the delete is itself a session; it stays valid.
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let alpha = logged_in_owner(&state, "alpha");
        let beta = logged_in_owner(&state, "beta");
        let cookie = set_cookie(&beta)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let resp = handle_http_request(
            &state,
            &HttpRequest::new("DELETE", "/api/v1/auth/operators/alpha")
                .with_header("Cookie", cookie.clone()),
        );
        assert_eq!(resp.status, 200);
        assert_eq!(session_status(&state, &alpha), 401);
        assert_eq!(session_status(&state, &beta), 200);
    }

    #[test]
    fn demoting_an_owner_revokes_its_sessions() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let alpha = logged_in_owner(&state, "alpha");
        let beta = logged_in_owner(&state, "beta");
        let resp = owner(
            &state,
            "POST",
            "/api/v1/auth/operators",
            &json!({ "username": "alpha", "role": "viewer" }),
        );
        assert_eq!(resp.status, 200);
        assert_eq!(session_status(&state, &alpha), 401);
        assert_eq!(session_status(&state, &beta), 200);
    }

    #[test]
    fn an_unrelated_update_revokes_nothing() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let alpha = logged_in_owner(&state, "alpha");
        let beta = logged_in_owner(&state, "beta");
        create_operator(&state, "gamma", "viewer", Some(PASSWORD));
        for body in [
            // A display name, a same-role update, a promotion and a demotion of someone else.
            json!({ "username": "alpha", "display_name": "Alpha A" }),
            json!({ "username": "alpha", "role": "owner" }),
            json!({ "username": "gamma", "role": "admin" }),
            json!({ "username": "gamma", "password": "gamma new password" }),
            json!({ "username": "beta", "role": "owner", "display_name": "Beta" }),
        ] {
            assert_eq!(
                owner(&state, "POST", "/api/v1/auth/operators", &body).status,
                200
            );
            assert_eq!(session_status(&state, &alpha), 200, "{body}");
            assert_eq!(session_status(&state, &beta), 200, "{body}");
        }
        // Enrolling and the first TOTP enable do not end sessions either.
        enable_totp(&state, "alpha");
        assert_eq!(session_status(&state, &alpha), 200);
        assert_eq!(session_status(&state, &beta), 200);
    }

    #[test]
    fn a_password_change_ends_that_operators_sessions() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let alpha = logged_in_owner(&state, "alpha");
        let beta = logged_in_owner(&state, "beta");
        let resp = owner(
            &state,
            "POST",
            "/api/v1/auth/operators",
            &json!({ "username": "alpha", "password": "a brand new password" }),
        );
        assert_eq!(resp.status, 200);
        assert_eq!(session_status(&state, &alpha), 401);
        assert_eq!(session_status(&state, &beta), 200);
        assert_eq!(
            password_login(&state, "alpha", "a brand new password").status,
            200
        );
    }

    #[test]
    fn replacing_a_live_totp_secret_ends_that_operators_sessions() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let alpha = logged_in_owner(&state, "alpha");
        let beta = logged_in_owner(&state, "beta");
        // The first enable is not a replacement.
        enable_totp(&state, "alpha");
        assert_eq!(session_status(&state, &alpha), 200);
        // A second secret, promoted over the live one, is.
        let enroll = owner(
            &state,
            "POST",
            "/api/v1/auth/totp/enroll",
            &json!({ "username": "alpha" }),
        );
        let secret = json_of(&enroll)["secret_base32"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            session_status(&state, &alpha),
            200,
            "enrolling alone changes nothing"
        );
        let verify = owner(
            &state,
            "POST",
            "/api/v1/auth/totp/verify",
            &json!({ "username": "alpha", "code": code_at(&secret, 0) }),
        );
        assert_eq!(verify.status, 200, "{}", body_text(&verify));
        assert_eq!(session_status(&state, &alpha), 401);
        assert_eq!(session_status(&state, &beta), 200);
    }

    #[test]
    fn revocation_survives_a_cache_drop() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let alpha = logged_in_owner(&state, "alpha");
        let token = json_of(&alpha)["session_token"]
            .as_str()
            .unwrap()
            .to_string();
        let fresh = || {
            crate::auth::AuthManager::new("lmcfarlin", Some(OWNER_TOKEN.into()), None, false, 3600)
        };
        {
            let store = state.open_store().unwrap();
            assert!(
                fresh()
                    .verify_session_token(Some(&*store), &token)
                    .is_some(),
                "control: the session is in the store"
            );
        }
        assert_eq!(
            owner(&state, "DELETE", "/api/v1/auth/operators/alpha", &json!({})).status,
            200
        );
        let store = state.open_store().unwrap();
        assert!(
            fresh()
                .verify_session_token(Some(&*store), &token)
                .is_none(),
            "a manager with an empty cache reads the revocation from the store"
        );
    }

    #[test]
    fn deleting_the_operator_named_like_the_token_owner_revokes_the_token_mode_sessions() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        // The seeded owner is the token owner's login; a token login mints a session for it.
        let resp = login(&state, &json!({ "mode": "token", "token": OWNER_TOKEN }));
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
        assert_eq!(session_status(&state, &resp), 200);
        create_operator(&state, "successor", "owner", None);
        assert_eq!(
            owner(
                &state,
                "DELETE",
                "/api/v1/auth/operators/lmcfarlin",
                &json!({})
            )
            .status,
            200
        );
        assert_eq!(
            session_status(&state, &resp),
            401,
            "intended: that login no longer exists"
        );
        // The owner token itself is the recovery path and still works.
        assert_eq!(
            owner(&state, "GET", "/api/v1/auth/operators", &json!({})).status,
            200
        );
    }

    // --- a login that races a change ----------------------------------------------------------

    /// Run `change` once, from inside the next credential verification.
    fn change_during_verification(
        state: &HubState,
        change: impl Fn(&HubState) + Send + Sync + 'static,
    ) {
        let fired = AtomicBool::new(false);
        let st = state.clone();
        set_hook(state, move || {
            if !fired.swap(true, Ordering::SeqCst) {
                change(&st);
            }
        });
    }

    #[test]
    fn an_owner_demoted_while_its_password_is_checked_gets_no_session() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "dora", "owner", Some(PASSWORD));
        change_during_verification(&state, |st| {
            let resp = owner(
                st,
                "POST",
                "/api/v1/auth/operators",
                &json!({ "username": "dora", "role": "viewer" }),
            );
            assert_eq!(resp.status, 200);
        });
        let resp = password_login(&state, "dora", PASSWORD);
        assert_eq!(resp.status, 403, "{}", body_text(&resp));
        assert!(set_cookie(&resp).is_none());
    }

    #[test]
    fn an_owner_deleted_while_its_password_is_checked_gets_no_session() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "dora", "owner", Some(PASSWORD));
        change_during_verification(&state, |st| {
            assert_eq!(
                owner(st, "DELETE", "/api/v1/auth/operators/dora", &json!({})).status,
                200
            );
        });
        assert!(is_invalid_credentials(&password_login(
            &state, "dora", PASSWORD
        )));
    }

    #[test]
    fn a_password_changed_while_it_is_checked_gets_no_session() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "dora", "owner", Some(PASSWORD));
        change_during_verification(&state, |st| {
            let resp = owner(
                st,
                "POST",
                "/api/v1/auth/operators",
                &json!({ "username": "dora", "password": "changed meanwhile" }),
            );
            assert_eq!(resp.status, 200);
        });
        assert!(is_invalid_credentials(&password_login(
            &state, "dora", PASSWORD
        )));
    }

    #[test]
    fn an_owner_demoted_during_the_totp_login_gets_no_session() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "dora", "owner", Some(PASSWORD));
        let secret = enable_totp(&state, "dora");
        change_during_verification(&state, |st| {
            let resp = owner(
                st,
                "POST",
                "/api/v1/auth/operators",
                &json!({ "username": "dora", "role": "admin" }),
            );
            assert_eq!(resp.status, 200);
        });
        let resp = totp_login(&state, "dora", Some(&code_at(&secret, 1)));
        assert_eq!(resp.status, 403, "{}", body_text(&resp));
        assert!(set_cookie(&resp).is_none());
    }

    // --- limits, seed and hash floor ----------------------------------------------------------

    fn extra_operator(name: &str) -> StoredOperator {
        let mut op = seed_file(name).unwrap().operators.remove(0);
        op.role = Role::Viewer;
        op
    }

    #[test]
    fn the_operator_count_is_capped_so_the_file_stays_readable() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let mut file = seed_file("lmcfarlin").unwrap();
        for i in 1..MAX_OPERATORS {
            file.operators.push(extra_operator(&format!("filler-{i}")));
        }
        assert_eq!(file.operators.len(), MAX_OPERATORS);
        write_file(tmp.path(), &file, true).unwrap();
        let resp = create_operator(&state, "one-too-many", "viewer", None);
        assert_eq!(resp.status, 409, "{}", body_text(&resp));
        assert_eq!(stored(&tmp).operators.len(), MAX_OPERATORS);
        // An existing operator can still be updated, and one deleted makes room.
        assert_eq!(
            create_operator(&state, "filler-1", "admin", None).status,
            200
        );
        assert_eq!(
            owner(
                &state,
                "DELETE",
                "/api/v1/auth/operators/filler-2",
                &json!({})
            )
            .status,
            200
        );
        assert_eq!(
            create_operator(&state, "one-more", "viewer", None).status,
            201
        );
    }

    #[test]
    fn the_total_passkey_count_is_capped_and_the_worst_file_fits_the_read_cap() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let mut file = seed_file("lmcfarlin").unwrap();
        let (_, public) = new_keypair();
        let key = |n: usize| StoredPasskey {
            credential_id: format!("{n:0>1024}"),
            label: "\u{1F511}".repeat(MAX_DISPLAY_NAME_CHARS),
            public_key_b64: URL_SAFE_NO_PAD.encode(&public),
            created_at_ms: 0,
            sign_count: 0,
            origin: Some("https://hub.example.ts.net:8443".to_string()),
        };
        let mut n = 0;
        for i in 0..(MAX_PASSKEYS_TOTAL / MAX_PASSKEYS_PER_OPERATOR) {
            let mut op = extra_operator(&format!("holder-{i}"));
            for _ in 0..MAX_PASSKEYS_PER_OPERATOR {
                op.passkeys.push(key(n));
                n += 1;
            }
            file.operators.push(op);
        }
        // One short of the cap, on an operator with room: exactly one more fits.
        let mut spare = extra_operator("spare");
        spare.display_name = "\u{1F511}".repeat(MAX_DISPLAY_NAME_CHARS);
        for _ in 0..(MAX_PASSKEYS_TOTAL % MAX_PASSKEYS_PER_OPERATOR - 1) {
            spare.passkeys.push(key(n));
            n += 1;
        }
        assert_eq!(n, MAX_PASSKEYS_TOTAL - 1);
        file.operators.push(spare);
        write_file(tmp.path(), &file, true).unwrap();
        let register_for = |cred: &str| {
            register(
                &state,
                json!({ "credential_id": cred, "public_key_b64": URL_SAFE_NO_PAD.encode(&public), "username": "spare" }),
            )
        };
        assert_eq!(register_for("credential-last-one-aaa").status, 200);
        let refused = register_for("credential-over-the-cap");
        assert_eq!(refused.status, 409, "{}", body_text(&refused));
        // The file with every passkey at its longest still reads back: the caps hold the size.
        let size = fs::metadata(file_path(&tmp)).unwrap().len();
        assert!(size < MAX_FILE_BYTES, "{size} bytes");
        assert!(read_file(tmp.path()).unwrap().is_some());
    }

    #[test]
    fn a_file_the_reader_would_refuse_is_never_written() {
        let tmp = TempDir::new().unwrap();
        let mut file = seed_file("lmcfarlin").unwrap();
        file.operators[0].display_name = "x".repeat(MAX_FILE_BYTES as usize + 1);
        assert!(matches!(
            write_file(tmp.path(), &file, true),
            Err(OpError::Io)
        ));
        assert!(!file_path(&tmp).exists());
    }

    #[test]
    fn a_stored_hash_below_the_iteration_floor_never_verifies() {
        let salt = [7u8; SALT_LEN];
        let hash_with = |iterations: u32| {
            let mut out = [0u8; HASH_LEN];
            pbkdf2::derive(
                PBKDF2_ALG,
                NonZeroU32::new(iterations).unwrap(),
                &salt,
                PASSWORD.as_bytes(),
                &mut out,
            );
            let mut op = seed_file("floor").unwrap().operators.remove(0);
            op.password_hash_hex = Some(hex_encode(&out));
            op.password_salt_hex = Some(hex_encode(&salt));
            op.password_iterations = Some(iterations);
            op
        };
        const { assert!(MIN_STORED_PBKDF2_ITERATIONS == PBKDF2_ITERATIONS) };
        // At the floor it verifies; one below, with a hash honestly computed at that count, not.
        assert!(verify_password(
            &hash_with(MIN_STORED_PBKDF2_ITERATIONS),
            PASSWORD
        ));
        assert!(!verify_password(
            &hash_with(MIN_STORED_PBKDF2_ITERATIONS - 1),
            PASSWORD
        ));
        assert!(!verify_password(&hash_with(1), PASSWORD));
        // Over HTTP a tampered low count fails like a wrong password.
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let mut file = seed_file("lmcfarlin").unwrap();
        let mut weak = hash_with(1);
        weak.username = "weak".to_string();
        weak.role = Role::Owner;
        file.operators.push(weak);
        write_file(tmp.path(), &file, true).unwrap();
        assert!(is_invalid_credentials(&password_login(
            &state, "weak", PASSWORD
        )));
    }

    #[test]
    fn the_seeded_owner_name_is_trimmed_and_never_empty() {
        assert_eq!(
            seed_file("  alice  ").unwrap().operators[0].username,
            "alice"
        );
        assert_eq!(
            seed_file("  alice  ").unwrap().operators[0].display_name,
            "alice"
        );
        for blank in ["", "   ", "\t\n"] {
            let op = seed_file(blank).unwrap().operators.remove(0);
            assert_eq!(op.username, DEFAULT_OWNER_LOGIN, "{blank:?}");
            assert_eq!(op.display_name, DEFAULT_OWNER_LOGIN);
        }
    }

    #[test]
    fn losing_the_seed_race_reads_the_winners_file_instead_of_failing() {
        let tmp = TempDir::new().unwrap();
        let state = state_for(&tmp, "mine");
        // Another process creates the file between this one's check and its create.
        SEED_RACE_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(|home: &Path| {
                write_file(home, &seed_file("racer").unwrap(), true).unwrap();
            }));
        });
        let resp = owner(&state, "GET", "/api/v1/auth/operators", &json!({}));
        assert_eq!(resp.status, 200, "{}", body_text(&resp));
        let list = json_of(&resp);
        assert_eq!(list["operators"].as_array().unwrap().len(), 1);
        assert_eq!(
            list["operators"][0]["username"], "racer",
            "the winner's file is the truth"
        );
        assert_eq!(stored(&tmp).operators[0].username, "racer");
        let leftovers: Vec<_> = fs::read_dir(file_path(&tmp).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n != "operators.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // A write on top of the lost race goes to the winner's file as well.
        let tmp = TempDir::new().unwrap();
        let state = state_for(&tmp, "mine");
        SEED_RACE_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(|home: &Path| {
                write_file(home, &seed_file("racer").unwrap(), true).unwrap();
            }));
        });
        assert_eq!(
            create_operator(&state, "newcomer", "viewer", None).status,
            201
        );
        let names: Vec<_> = stored(&tmp)
            .operators
            .iter()
            .map(|o| o.username.clone())
            .collect();
        assert_eq!(names, ["racer", "newcomer"]);
    }

    // --- Wave C audit regressions (2026-10-30) ------------------------------------------------

    #[test]
    fn totp_refuses_a_secret_below_128_bits() {
        let now = 1_111_111_111u64;
        let step = now / 30;
        for secret in [&[][..], &[0x42u8; 15][..]] {
            let code = format!("{:06}", hotp(secret, step, 6));
            assert_eq!(
                verify_totp(secret, &code, now, None),
                None,
                "{} bytes",
                secret.len()
            );
        }
        let ok = [0x42u8; 16];
        let code = format!("{:06}", hotp(&ok, step, 6));
        assert_eq!(verify_totp(&ok, &code, now, None), Some(step));
    }

    #[test]
    fn a_tampered_empty_totp_secret_does_not_let_a_password_alone_in() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        create_operator(&state, "hollow", "owner", Some(PASSWORD));
        let mut file = stored(&tmp);
        let i = file.find_username("hollow").unwrap();
        file.operators[i].totp_secret_base32 = Some(String::new());
        file.operators[i].totp_enabled = true;
        write_file(tmp.path(), &file, false).unwrap();
        // HMAC accepts an empty key, so anyone can compute this code.
        let code = format!("{:06}", hotp(&[], now_unix_secs() / TOTP_STEP_SECS, 6));
        let resp = login(
            &state,
            &json!({ "mode": "password", "username": "hollow", "password": PASSWORD, "totp_code": code }),
        );
        assert!(is_invalid_credentials(&resp), "{}", body_text(&resp));
    }

    #[test]
    fn base32_decoding_is_strict_about_alphabet_and_padding_position() {
        for bad in [
            "A=A",
            "MZXW6===YTBOI",
            "MZXW6\n",
            "MZXW6\t",
            "０",
            "MZXW6\0",
        ] {
            assert!(base32_decode(bad).is_none(), "{bad:?}");
        }
        // Padding only ever trails.
        assert_eq!(base32_decode("MY======").unwrap(), b"f");
        assert_eq!(base32_decode("my").unwrap(), b"f");
        // Only padding decodes to an empty secret; `verify_totp` refuses such a secret.
        assert_eq!(base32_decode("=").unwrap(), b"");
    }

    #[test]
    fn an_anonymous_login_cannot_charge_the_internal_totp_verify_or_passkey_keys() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        // The token owner's own TOTP verification key is `totp-verify:lmcfarlin`.
        for _ in 0..8 {
            password_login(&state, "totp-verify:lmcfarlin", PASSWORD);
        }
        // The bucket those names share is locked ...
        assert_eq!(
            password_login(&state, "totp-verify:lmcfarlin", PASSWORD).status,
            429
        );
        // ... but the internal key was never charged.
        let resp = owner(
            &state,
            "POST",
            "/api/v1/auth/totp/verify",
            &json!({ "code": "000000" }),
        );
        assert_ne!(resp.status, 429, "{}", body_text(&resp));
        // Nor was a passkey key, and real names are untouched.
        for _ in 0..8 {
            password_login(&state, "passkey:abcdefghijklmnop", PASSWORD);
        }
        let mut t = state.login_guard.throttle();
        assert_eq!(
            t.retry_after("passkey:abcdefghijklmnop", Instant::now()),
            None
        );
        drop(t);
        create_operator(&state, "bystander", "owner", Some(PASSWORD));
        assert_eq!(password_login(&state, "bystander", PASSWORD).status, 200);
    }

    #[test]
    fn the_invalid_name_bucket_cannot_be_spelled_by_a_real_operator() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        // `invalid-username` is a valid username (lowercase letters and `-`).
        assert_eq!(
            create_operator(&state, "invalid-username", "owner", Some(PASSWORD)).status,
            201
        );
        for _ in 0..8 {
            password_login(&state, "a:b", PASSWORD);
        }
        // The invalid names are locked ...
        assert_eq!(password_login(&state, "a:b", PASSWORD).status, 429);
        // ... but the operator that happens to be named like the old bucket is not.
        assert_eq!(
            password_login(&state, "invalid-username", "wrong-password").status,
            401
        );
        assert_eq!(
            password_login(&state, "invalid-username", PASSWORD).status,
            200
        );
    }

    #[test]
    fn control_characters_in_a_login_name_share_the_invalid_bucket() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        for _ in 0..6 {
            password_login(&state, "bad\u{1}name", PASSWORD);
        }
        assert_eq!(
            password_login(&state, "other\u{2}name", PASSWORD).status,
            429
        );
        assert!(
            state
                .login_guard
                .throttle()
                .retry_after("bad\u{1}name", Instant::now())
                .is_none()
        );
    }

    #[test]
    fn a_username_shaped_like_an_operator_id_is_refused() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        for name in ["op-0123456789abcdef", "op-ffffffffffffffff"] {
            let resp = create_operator(&state, name, "viewer", None);
            assert_eq!(resp.status, 400, "{name}: {}", body_text(&resp));
        }
        // Near misses are ordinary names.
        for name in [
            "op-0123456789abcde",
            "op-0123456789abcdeg",
            "op-0123456789abcdef0",
            "op-0123456789ABCDEF",
        ] {
            let resp = create_operator(&state, name, "viewer", None);
            assert!(matches!(resp.status, 201 | 400), "{name}: {}", resp.status);
        }
        assert_eq!(
            create_operator(&state, "op-0123456789abcde", "viewer", None).status,
            200
        );
        // Deleting by a real id still hits the id's own operator.
        let id = json_of(&create_operator(&state, "victim", "viewer", None))["operator"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(looks_like_operator_id(&id));
        assert_eq!(
            owner(
                &state,
                "DELETE",
                &format!("/api/v1/auth/operators/{id}"),
                &json!({})
            )
            .status,
            200
        );
        assert!(stored(&tmp).find_username("victim").is_none());
    }

    #[test]
    fn registration_refuses_points_that_are_not_on_the_p256_curve() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let (_, good) = new_keypair();
        let cred = |n: u8| format!("credential-for-test-{n:04}");
        let register = |n: u8, key: &[u8]| {
            owner(
                &state,
                "POST",
                "/api/v1/auth/passkey/register",
                &json!({ "credential_id": cred(n), "label": "k", "public_key_b64": URL_SAFE_NO_PAD.encode(key) }),
            )
        };
        assert_eq!(register(1, &good).status, 200);
        let mut off_curve = good.clone();
        off_curve[64] ^= 1;
        let mut zeros = vec![0u8; 65];
        zeros[0] = 0x04;
        let mut ones = vec![0xffu8; 65];
        ones[0] = 0x04;
        // x or y at or above the field prime p.
        let mut prime = vec![0u8; 65];
        prime[0] = 0x04;
        prime[1..33].copy_from_slice(&good[1..33]);
        prime[33..65].copy_from_slice(
            &hex_decode("ffffffff00000001000000000000000000000000ffffffffffffffffffffffff")
                .unwrap(),
        );
        // Compressed and hybrid encodings, and a wrong length.
        let mut compressed = good[..33].to_vec();
        compressed[0] = 0x02;
        let mut hybrid = good.clone();
        hybrid[0] = 0x06;
        for (n, what, key) in [
            (2u8, "one bit off the curve", &off_curve),
            (3, "all zeros", &zeros),
            (4, "all ones", &ones),
            (5, "y equal to the prime", &prime),
            (6, "compressed", &compressed),
            (7, "hybrid", &hybrid),
            (8, "truncated", &good[..64].to_vec()),
            (9, "extended", &[good.as_slice(), &[0]].concat()),
        ] {
            let resp = register(n, key);
            assert_eq!(resp.status, 400, "{what}: {}", body_text(&resp));
        }
        assert_eq!(
            stored(&tmp).operators[0].passkeys.len(),
            1,
            "only the valid key was stored"
        );
    }

    #[test]
    fn a_der_signature_must_be_strict_der() {
        let split = |sig: &[u8]| -> (Vec<u8>, Vec<u8>) {
            assert_eq!((sig[0], sig[2]), (0x30, 0x02));
            let rl = sig[3] as usize;
            let r = sig[4..4 + rl].to_vec();
            assert_eq!(sig[4 + rl], 0x02);
            let sl = sig[5 + rl] as usize;
            (r, sig[6 + rl..6 + rl + sl].to_vec())
        };
        let der = |r: &[u8], s: &[u8]| -> Vec<u8> {
            let mut body = vec![0x02, r.len() as u8];
            body.extend_from_slice(r);
            body.extend_from_slice(&[0x02, s.len() as u8]);
            body.extend_from_slice(s);
            let mut out = vec![0x30, body.len() as u8];
            out.extend_from_slice(&body);
            out
        };
        let unpad = |v: &[u8]| -> Vec<u8> {
            let start = v.iter().position(|&b| b != 0).unwrap_or(v.len() - 1);
            let mut out = vec![0u8; 32 - (v.len() - start)];
            out.extend_from_slice(&v[start..]);
            out
        };
        type Mutation<'a> = (&'static str, Box<dyn Fn(&[u8]) -> Vec<u8> + 'a>);
        let mutations: Vec<Mutation> = vec![
            ("trailing byte", Box::new(|sig| [sig, &[0]].concat())),
            ("truncated", Box::new(|sig| sig[..sig.len() - 1].to_vec())),
            ("one byte", Box::new(|_| vec![0x30])),
            (
                "long-form sequence length",
                Box::new(|sig| [&[0x30, 0x81, sig[1]][..], &sig[2..]].concat()),
            ),
            (
                "indefinite length",
                Box::new(|sig| [&[0x30, 0x80][..], &sig[2..], &[0, 0]].concat()),
            ),
            (
                "non-minimal r",
                Box::new(move |sig| {
                    let (r, s) = split(sig);
                    der(&[&[0u8][..], &r].concat(), &s)
                }),
            ),
            (
                "negative r",
                Box::new(move |sig| {
                    let (r, s) = split(sig);
                    let mut r = unpad(&r);
                    r[0] |= 0x80;
                    der(&r, &s)
                }),
            ),
            (
                "zero r and s",
                Box::new(|_| vec![0x30, 6, 2, 1, 0, 2, 1, 0]),
            ),
            (
                "raw r||s (what WebCrypto emits)",
                Box::new(move |sig| {
                    let (r, s) = split(sig);
                    [unpad(&r), unpad(&s)].concat()
                }),
            ),
            (
                "r and s swapped",
                Box::new(move |sig| {
                    let (r, s) = split(sig);
                    der(&s, &r)
                }),
            ),
            (
                "wrong tag for the sequence",
                Box::new(|sig| [&[0x31][..], &sig[1..]].concat()),
            ),
        ];
        let fx = fx("owner");
        for (what, mutate) in &mutations {
            fx.state.login_guard.throttle().clear(&fx.username);
            let o = opts(&fx.state, 1);
            let (cd, ad, sig) = assertion_parts(&fx.key, &o);
            let resp = login(
                &fx.state,
                &assertion_body(&fx.cred, &(cd, ad, mutate(&sig))),
            );
            assert!(
                is_invalid_credentials(&resp),
                "{what}: {} {}",
                resp.status,
                body_text(&resp)
            );
        }
        // An unmangled signature over the very same inputs still works: the refusals above were
        // about the encoding, and none of them spent the counter.
        fx.state.login_guard.throttle().clear(&fx.username);
        assert_eq!(passkey_login(&fx, &opts(&fx.state, 1)).status, 200);
    }

    #[test]
    fn an_empty_or_garbage_signature_field_is_the_plain_401() {
        let fx = fx("owner");
        for sig in [
            "",
            "=",
            "!!!",
            "AAAA",
            "A",
            &"A".repeat(MAX_ASSERTION_FIELD_LEN + 1),
        ] {
            fx.state.login_guard.throttle().clear(&fx.username);
            let o = opts(&fx.state, 1);
            let (cd, ad, _) = assertion_parts(&fx.key, &o);
            let mut body = assertion_body(&fx.cred, &(cd, ad, Vec::new()));
            body["signature_b64"] = json!(sig);
            assert!(is_invalid_credentials(&login(&fx.state, &body)), "{sig:?}");
        }
    }

    #[test]
    fn a_counter_that_reaches_u32_max_ends_the_credentials_logins() {
        // Characterises a known limit (audit LOW-6): the strict-increase rule has no wrap rule,
        // so a credential that has seen u32::MAX never logs in again. The fix is to re-register.
        let fx = fx("owner");
        assert_eq!(passkey_login(&fx, &opts(&fx.state, u32::MAX)).status, 200);
        fx.state.login_guard.throttle().clear(&fx.username);
        for count in [0, 1, u32::MAX] {
            let resp = passkey_login(&fx, &opts(&fx.state, count));
            assert!(is_invalid_credentials(&resp), "count {count}");
            fx.state.login_guard.throttle().clear(&fx.username);
        }
    }

    #[test]
    fn two_assertions_with_the_same_counter_cannot_both_log_in() {
        let fx = fx("owner");
        let bodies: Vec<Value> = (0..6)
            .map(|_| assertion_body(&fx.cred, &assertion_parts(&fx.key, &opts(&fx.state, 7))))
            .collect();
        let wins = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for body in &bodies {
                scope.spawn(|| {
                    if login(&fx.state, body).status == 200 {
                        wins.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(
            wins.load(Ordering::SeqCst),
            1,
            "a counter value is spent once"
        );
        let file = stored(&fx._tmp);
        let (i, j) = file.find_credential(&fx.cred).unwrap();
        assert_eq!(file.operators[i].passkeys[j].sign_count, 7);
    }

    #[test]
    fn one_challenge_can_log_in_only_one_of_many_racing_assertions() {
        let fx = fx("owner");
        let o = opts(&fx.state, 3);
        let body = assertion_body(&fx.cred, &assertion_parts(&fx.key, &o));
        let wins = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    if login(&fx.state, &body).status == 200 {
                        wins.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(wins.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn origin_parsing_rejects_lookalike_loopback_hosts_and_smuggled_parts() {
        for raw in [
            "http://localhost.evil.com",
            "http://127.0.0.1.nip.io",
            "http://127.0.0.2",
            "http://0.0.0.0",
            "http://[::2]",
            "http://example.com",
            "https://user@host.example",
            "https://user:pw@host.example",
            "https://host.example/",
            "https://host.example/path",
            "https://host.example?x=1",
            "https://host.example#f",
            "https://host .example",
            "https://host.example:",
            "https://host.example:80a",
            "https://host.example:123456",
            "https://:443",
            "https://[]",
            "https://[::1",
            "ftp://host.example",
            "//host.example",
            "host.example",
            "",
        ] {
            assert_eq!(parse_origin(raw), None, "{raw:?}");
        }
        // A default port folds away; any other port, and a non-canonical one, is kept as written.
        assert_eq!(
            parse_origin("HTTPS://Host.Example:443").unwrap().origin,
            "https://host.example"
        );
        assert_eq!(
            parse_origin("http://[::1]:80").unwrap().origin,
            "http://[::1]"
        );
        assert_eq!(
            parse_origin("https://host.example:8443").unwrap().origin,
            "https://host.example:8443"
        );
        assert_ne!(
            parse_origin("https://host.example:0443").unwrap().origin,
            parse_origin("https://host.example").unwrap().origin,
            "a leading-zero port is not the default port"
        );
        // The relying-party id is the host name alone.
        assert_eq!(
            parse_origin("https://host.example:8443").unwrap().rp_id,
            "host.example"
        );
        assert_eq!(parse_origin("http://[::1]:3000").unwrap().rp_id, "[::1]");
    }

    #[test]
    fn password_hashing_pins_the_salt_and_cost_it_stores() {
        let (hash_a, salt_a, n_a) = hash_password(PASSWORD).unwrap();
        let (hash_b, salt_b, _) = hash_password(PASSWORD).unwrap();
        assert_ne!(salt_a, salt_b);
        assert_ne!(
            hash_a, hash_b,
            "same password, different salt, different hash"
        );
        // The stored salt decodes to exactly SALT_LEN bytes and the cost is the stored count.
        assert_eq!(hex_decode(&salt_a).unwrap().len(), SALT_LEN);
        assert_eq!(n_a, PBKDF2_ITERATIONS);
        // Case and surrounding whitespace are part of a password.
        let mut op = seed_file("x").unwrap().operators.remove(0);
        op.password_hash_hex = Some(hash_a);
        op.password_salt_hex = Some(salt_a);
        op.password_iterations = Some(n_a);
        for wrong in [
            PASSWORD.to_uppercase(),
            format!(" {PASSWORD}"),
            format!("{PASSWORD} "),
        ] {
            assert!(!verify_password(&op, &wrong), "{wrong:?}");
        }
        // HMAC zero-pads a short key, so trailing NULs do not change a PBKDF2 result. The API
        // edge refuses them (see `valid_password`) instead of relying on the hash to.
        assert!(verify_password(&op, &format!("{PASSWORD}\0")));
        assert!(!valid_password(&format!("{PASSWORD}\0")));
        assert!(valid_password(PASSWORD));
        // A salt or hash that is not even-length hex, or has the wrong length, never verifies.
        for broken in ["", "zz", "abc", "é"] {
            let mut bad = op.clone();
            bad.password_salt_hex = Some(broken.to_string());
            assert!(!verify_password(&bad, PASSWORD), "{broken:?}");
            let mut bad = op.clone();
            bad.password_hash_hex = Some(broken.to_string());
            assert!(!verify_password(&bad, PASSWORD), "{broken:?}");
        }
    }

    #[test]
    fn the_operator_file_survives_concurrent_writers_without_losing_an_update() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        assert_eq!(
            create_operator(&state, "seedhold", "viewer", None).status,
            201
        );
        std::thread::scope(|scope| {
            for t in 0..8 {
                let state = &state;
                scope.spawn(move || {
                    for n in 0..5 {
                        let resp = create_operator(state, &format!("w{t}-{n}"), "viewer", None);
                        assert_eq!(resp.status, 201, "{}", body_text(&resp));
                    }
                });
            }
        });
        let file = stored(&tmp);
        assert_eq!(
            file.operators.len(),
            1 + 1 + 40,
            "the seeded owner, seedhold and every writer's 5"
        );
        assert!(file.is_consistent());
        assert_eq!(mode_of(&file_path(&tmp)), 0o600);
        let leftovers = fs::read_dir(file_path(&tmp).parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_password_with_a_nul_is_refused_when_set_and_when_used() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let resp = create_operator(&state, "nully", "owner", Some("correct horse\u{0}"));
        assert_eq!(resp.status, 400, "{}", body_text(&resp));
        assert_eq!(
            create_operator(&state, "nully", "owner", Some(PASSWORD)).status,
            201
        );
        assert_eq!(password_login(&state, "nully", PASSWORD).status, 200);
        let before = evaluated(&state);
        let resp = password_login(&state, "nully", &format!("{PASSWORD}\u{0}"));
        assert!(is_invalid_credentials(&resp), "{}", body_text(&resp));
        assert_eq!(evaluated(&state), before, "refused before any hashing");
    }
}
