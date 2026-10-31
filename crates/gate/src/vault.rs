//! `ring` AES-256-GCM encrypted secrets vault (`vault.enc`) with AEAD AAD policy binding (spec §2).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fl_core::At;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::GateError;

pub const VAULT_SCHEMA_VERSION: u32 = 1;
pub const MASTER_KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;

/// Public, redacted metadata for a secret stored in [`Vault`] (spec §2.2).
/// Never holds plaintext secret values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultSecretMetadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub allowed_agents: Vec<String>,
    #[serde(default)]
    pub allow_private_network: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Parameters for inserting or updating a secret in [`Vault`].
#[derive(Debug, Clone, Default)]
pub struct SetSecretArgs {
    pub name: String,
    pub value: String,
    pub description: Option<String>,
    pub allowed_hosts: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub allowed_agents: Vec<String>,
    pub allow_private_network: bool,
    pub lease_ttl_secs: Option<u64>,
}

/// On-disk encrypted record inside `vault.enc`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedSecretRecord {
    pub metadata: VaultSecretMetadata,
    pub nonce_b64: String,
    pub ciphertext_b64: String,
}

/// On-disk `vault.enc` container.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultFile {
    pub schema_version: u32,
    #[serde(default)]
    pub secrets: BTreeMap<String, EncryptedSecretRecord>,
}

impl Default for VaultFile {
    fn default() -> Self {
        Self {
            schema_version: VAULT_SCHEMA_VERSION,
            secrets: BTreeMap::new(),
        }
    }
}

/// Zero-knowledge AES-256-GCM encrypted secrets vault (`crates/gate/src/vault.rs`).
#[derive(Debug, Clone)]
pub struct Vault {
    path: PathBuf,
    master_key: [u8; MASTER_KEY_LEN],
    file: VaultFile,
}

impl Vault {
    /// Open or initialize a [`Vault`] at `vault_path` using `master_key`.
    pub fn open(
        vault_path: impl Into<PathBuf>,
        master_key: [u8; MASTER_KEY_LEN],
    ) -> Result<Self, GateError> {
        let path = vault_path.into();
        let file = if path.exists() {
            reject_if_symlink(&path)?;
            let raw = fs::read_to_string(&path).map_err(|e| GateError::Io {
                op: "read",
                path: path.clone(),
                cause: e.to_string(),
            })?;
            serde_json::from_str::<VaultFile>(&raw).map_err(|e| GateError::VaultCorrupt {
                path: path.clone(),
                cause: e.to_string(),
            })?
        } else {
            VaultFile::default()
        };
        if file.schema_version != VAULT_SCHEMA_VERSION {
            return Err(GateError::VaultCorrupt {
                path,
                cause: format!("unsupported vault schema_version {}", file.schema_version),
            });
        }
        Ok(Self {
            path,
            master_key,
            file,
        })
    }

    /// Open the default or environment-configured vault and master key.
    pub fn open_default(home_override: Option<&Path>) -> Result<Self, GateError> {
        let vault_path = resolve_vault_path(home_override);
        let key_path = resolve_master_key_path(home_override);
        let master_key = load_or_init_master_key(&key_path)?;
        Self::open(vault_path, master_key)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// List redacted metadata for all secrets in deterministic key order.
    pub fn list(&self) -> Vec<VaultSecretMetadata> {
        self.file
            .secrets
            .values()
            .map(|r| r.metadata.clone())
            .collect()
    }

    /// Look up redacted metadata for a single secret by name.
    pub fn get_metadata(&self, name: &str) -> Option<&VaultSecretMetadata> {
        let key = normalize_secret_name(name).ok()?;
        self.file.secrets.get(&key).map(|r| &r.metadata)
    }

    /// Encrypt and persist a secret in `vault.enc` with AEAD AAD policy binding.
    pub fn set(&mut self, args: SetSecretArgs) -> Result<VaultSecretMetadata, GateError> {
        let name = normalize_secret_name(&args.name)?;
        if args.value.is_empty() {
            return Err(GateError::InvalidSecret {
                name,
                reason: "secret value must not be empty".to_string(),
            });
        }

        let allowed_hosts = normalize_string_list(&args.allowed_hosts, true);
        let allowed_methods = normalize_string_list(&args.allowed_methods, false)
            .into_iter()
            .map(|m| m.to_ascii_uppercase())
            .collect::<Vec<_>>();
        let allowed_agents = normalize_string_list(&args.allowed_agents, true);

        for item in allowed_hosts
            .iter()
            .chain(allowed_methods.iter())
            .chain(allowed_agents.iter())
        {
            if item.contains('|') || item.contains(',') {
                return Err(GateError::InvalidSecret {
                    name,
                    reason: format!(
                        "policy item `{item}` must not contain AAD delimiter characters (`|` or `,`)"
                    ),
                });
            }
        }

        let now_ms = now_unix_millis();
        let now_str = At::from_unix_millis(now_ms).to_string();
        let created_at = self
            .file
            .secrets
            .get(&name)
            .map(|existing| existing.metadata.created_at.clone())
            .unwrap_or_else(|| now_str.clone());
        let expires_at = args.lease_ttl_secs.map(|ttl| {
            let exp_ms = now_ms.saturating_add(ttl.saturating_mul(1_000));
            At::from_unix_millis(exp_ms).to_string()
        });

        let metadata = VaultSecretMetadata {
            name: name.clone(),
            description: args
                .description
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            allowed_hosts,
            allowed_methods,
            allowed_agents,
            allow_private_network: args.allow_private_network,
            created_at,
            updated_at: now_str,
            lease_ttl_secs: args.lease_ttl_secs,
            expires_at,
        };

        let rng = SystemRandom::new();
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rng.fill(&mut nonce_bytes).map_err(|_| GateError::Crypto {
            reason: "failed to generate 96-bit random nonce".to_string(),
        })?;

        let aad_str = canonical_aad(&metadata);
        let unbound =
            UnboundKey::new(&AES_256_GCM, &self.master_key).map_err(|_| GateError::Crypto {
                reason: "invalid AES-256-GCM master key".to_string(),
            })?;
        let key = LessSafeKey::new(unbound);
        let mut in_out = args.value.into_bytes();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(aad_str.as_bytes()),
            &mut in_out,
        )
        .map_err(|_| GateError::Crypto {
            reason: "AES-256-GCM encryption failed".to_string(),
        })?;

        let record = EncryptedSecretRecord {
            metadata: metadata.clone(),
            nonce_b64: BASE64.encode(nonce_bytes),
            ciphertext_b64: BASE64.encode(in_out),
        };
        self.file.secrets.insert(name, record);
        self.save()?;
        Ok(metadata)
    }

    /// Remove a secret from `vault.enc` and persist.
    pub fn remove(&mut self, name: &str) -> Result<bool, GateError> {
        let key = normalize_secret_name(name)?;
        let existed = self.file.secrets.remove(&key).is_some();
        if existed {
            self.save()?;
        }
        Ok(existed)
    }

    /// Decrypt a secret for internal proxy substitution, verifying its AAD policy and expiration.
    pub(crate) fn decrypt_raw(
        &self,
        name: &str,
    ) -> Result<(VaultSecretMetadata, String), GateError> {
        let key_name = normalize_secret_name(name)?;
        let Some(record) = self.file.secrets.get(&key_name) else {
            return Err(GateError::NoSuchSecret { name: key_name });
        };
        // The AAD binds `metadata.name`, not the map key. A record copied under another key
        // would otherwise decrypt as that other secret, with its own value and policy.
        if record.metadata.name != key_name {
            return Err(GateError::VaultTampered { secret: key_name });
        }

        if let Some(ref exp) = record.metadata.expires_at {
            let now_str = At::from_unix_millis(now_unix_millis()).to_string();
            if now_str.as_str() > exp.as_str() {
                return Err(GateError::PolicyDenied {
                    secret: key_name,
                    reason: format!("secret lease expired at {exp}"),
                });
            }
        }

        let nonce_vec = BASE64
            .decode(&record.nonce_b64)
            .map_err(|e| GateError::VaultCorrupt {
                path: self.path.clone(),
                cause: format!("invalid nonce base64 for `{key_name}`: {e}"),
            })?;
        let nonce_bytes: [u8; NONCE_LEN] =
            nonce_vec.try_into().map_err(|_| GateError::VaultCorrupt {
                path: self.path.clone(),
                cause: format!("nonce for `{key_name}` is not {NONCE_LEN} bytes"),
            })?;

        let mut in_out =
            BASE64
                .decode(&record.ciphertext_b64)
                .map_err(|e| GateError::VaultCorrupt {
                    path: self.path.clone(),
                    cause: format!("invalid ciphertext base64 for `{key_name}`: {e}"),
                })?;

        let aad_str = canonical_aad(&record.metadata);
        let unbound =
            UnboundKey::new(&AES_256_GCM, &self.master_key).map_err(|_| GateError::Crypto {
                reason: "invalid AES-256-GCM master key".to_string(),
            })?;
        let key = LessSafeKey::new(unbound);
        let plaintext_bytes = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(aad_str.as_bytes()),
                &mut in_out,
            )
            .map_err(|_| GateError::VaultTampered {
                secret: key_name.clone(),
            })?;

        let value =
            String::from_utf8(plaintext_bytes.to_vec()).map_err(|e| GateError::VaultCorrupt {
                path: self.path.clone(),
                cause: format!("decrypted secret `{key_name}` is not valid UTF-8: {e}"),
            })?;
        Ok((record.metadata.clone(), value))
    }

    /// Return all `(name, plaintext_value)` pairs for output scrubbing (`scrub_secrets`).
    pub(crate) fn all_decrypted_for_scrubbing(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for name in self.file.secrets.keys() {
            if let Ok((_meta, val)) = self.decrypt_raw(name)
                && !val.is_empty()
            {
                out.push((name.clone(), val));
            }
        }
        out
    }

    pub fn save(&self) -> Result<(), GateError> {
        let encoded =
            serde_json::to_string_pretty(&self.file).map_err(|e| GateError::VaultCorrupt {
                path: self.path.clone(),
                cause: e.to_string(),
            })?;
        write_atomic_0600(&self.path, format!("{encoded}\n").as_bytes())
    }

    /// Compare `candidate` against the decrypted value of `secret_name` in constant time.
    pub fn verify_secret_constant_time(
        &self,
        secret_name: &str,
        candidate: &str,
    ) -> Result<bool, GateError> {
        let (_meta, expected) = self.decrypt_raw(secret_name)?;
        let a = expected.as_bytes();
        let b = candidate.as_bytes();
        if a.len() != b.len() {
            return Ok(false);
        }
        let mut diff = 0u8;
        for (&x, &y) in a.iter().zip(b.iter()) {
            diff |= std::hint::black_box(x ^ y);
        }
        Ok(std::hint::black_box(diff) == 0)
    }

    /// Append a structured JSONL audit log entry next to `vault.enc` (`gate-audit.jsonl`).
    pub fn append_audit_log(
        &self,
        caller: Option<&str>,
        secrets: &[String],
        method: &str,
        host: &str,
        outcome: &str,
    ) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        let audit_path = parent.join("gate-audit.jsonl");
        if reject_if_symlink(&audit_path).is_err() {
            return;
        }
        let ts = At::from_unix_millis(now_unix_millis()).to_string();
        let entry = serde_json::json!({
            "timestamp": ts,
            "caller": caller.unwrap_or("anonymous"),
            "secrets": secrets,
            "method": method,
            "host": host,
            "outcome": outcome,
        });
        let Ok(mut line) = serde_json::to_string(&entry) else {
            return;
        };
        // One `write_all` so an `O_APPEND` write from another thread or process cannot land
        // between the entry and its newline.
        line.push('\n');
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(mut f) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&audit_path)
            {
                let _ = f.write_all(line.as_bytes());
            }
        }
        #[cfg(not(unix))]
        {
            if let Ok(mut f) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&audit_path)
            {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        for byte in &mut self.master_key {
            unsafe {
                std::ptr::write_volatile(byte, 0);
            }
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// Build the canonical AEAD Additional Authenticated Data (AAD) string for `meta`.
pub fn canonical_aad(meta: &VaultSecretMetadata) -> String {
    let hosts = meta.allowed_hosts.join(",");
    let methods = meta.allowed_methods.join(",");
    let agents = meta.allowed_agents.join(",");
    let exp = meta.expires_at.as_deref().unwrap_or("none");
    format!(
        "ferrogate:v1|{}|{}|{}|{}|{}|{}",
        meta.name, hosts, methods, agents, meta.allow_private_network, exp
    )
}

/// Validate and normalize a secret key name (`A-Z0-9_`, 1..=64 chars).
pub fn normalize_secret_name(raw: &str) -> Result<String, GateError> {
    let trimmed = raw.trim().to_ascii_uppercase();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err(GateError::InvalidSecret {
            name: raw.to_string(),
            reason: "secret name must be 1 to 64 characters".to_string(),
        });
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(GateError::InvalidSecret {
            name: raw.to_string(),
            reason: "secret name may contain only ASCII letters, digits, and `_`".to_string(),
        });
    }
    Ok(trimmed)
}

fn normalize_string_list(items: &[String], lowercase: bool) -> Vec<String> {
    let mut out: Vec<String> = items
        .iter()
        .map(|s| {
            let t = s.trim();
            if lowercase {
                t.to_ascii_lowercase()
            } else {
                t.to_string()
            }
        })
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn resolve_vault_path(home_override: Option<&Path>) -> PathBuf {
    if let Some(p) = std::env::var_os("FL_VAULT_PATH")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    if let Some(home) = home_override {
        return home
            .join(".local")
            .join("share")
            .join("fl")
            .join("vault.enc");
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("fl").join("vault.enc");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local")
        .join("share")
        .join("fl")
        .join("vault.enc")
}

pub fn resolve_master_key_path(home_override: Option<&Path>) -> PathBuf {
    if let Some(p) = std::env::var_os("FL_VAULT_KEY_PATH")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    if let Some(home) = home_override {
        return home.join(".config").join("fl").join("vault.key");
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("fl").join("vault.key");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("fl").join("vault.key")
}

/// Load a 32-byte master key from `FL_VAULT_MASTER_KEY` or `key_path`, generating and persisting
/// a fresh 256-bit key (`0600`) if `key_path` does not exist yet.
pub fn load_or_init_master_key(key_path: &Path) -> Result<[u8; MASTER_KEY_LEN], GateError> {
    if let Ok(env_hex) = std::env::var("FL_VAULT_MASTER_KEY") {
        let trimmed = env_hex.trim();
        if !trimmed.is_empty() {
            return parse_hex_master_key(trimmed, Path::new("FL_VAULT_MASTER_KEY"));
        }
    }

    if key_path.exists() {
        reject_if_symlink(key_path)?;
        let raw = fs::read_to_string(key_path).map_err(|e| GateError::Io {
            op: "read",
            path: key_path.to_path_buf(),
            cause: e.to_string(),
        })?;
        return parse_hex_master_key(raw.trim(), key_path);
    }

    let rng = SystemRandom::new();
    let mut key = [0u8; MASTER_KEY_LEN];
    rng.fill(&mut key).map_err(|_| GateError::Crypto {
        reason: "failed to generate 256-bit vault master key".to_string(),
    })?;
    install_new_master_key(key_path, key)
}

/// Publish `key` at `key_path` unless a key is already there; return whichever key now lives at
/// `key_path`. Two first runs that race each generate a key, and the loser must adopt the
/// winner's: an overwrite would orphan every secret already sealed with the first key.
fn install_new_master_key(
    key_path: &Path,
    key: [u8; MASTER_KEY_LEN],
) -> Result<[u8; MASTER_KEY_LEN], GateError> {
    let hex = encode_hex(&key);
    let tmp_path = write_tmp_0600(key_path, format!("{hex}\n").as_bytes())?;
    let linked = fs::hard_link(&tmp_path, key_path);
    let _ = fs::remove_file(&tmp_path);
    match linked {
        Ok(()) => Ok(key),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            reject_if_symlink(key_path)?;
            let raw = fs::read_to_string(key_path).map_err(|e| GateError::Io {
                op: "read",
                path: key_path.to_path_buf(),
                cause: e.to_string(),
            })?;
            parse_hex_master_key(raw.trim(), key_path)
        }
        Err(e) => Err(GateError::Io {
            op: "link",
            path: key_path.to_path_buf(),
            cause: e.to_string(),
        }),
    }
}

fn parse_hex_master_key(hex_str: &str, path: &Path) -> Result<[u8; MASTER_KEY_LEN], GateError> {
    if hex_str.len() != MASTER_KEY_LEN * 2 {
        return Err(GateError::VaultCorrupt {
            path: path.to_path_buf(),
            cause: format!(
                "expected {}-character hex master key, got {} characters",
                MASTER_KEY_LEN * 2,
                hex_str.len()
            ),
        });
    }
    // `from_str_radix` accepts a leading `+`; a key is bare hex digits and nothing else.
    if !hex_str.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(GateError::VaultCorrupt {
            path: path.to_path_buf(),
            cause: "master key contains non-hexadecimal characters".to_string(),
        });
    }
    let mut out = [0u8; MASTER_KEY_LEN];
    for (idx, chunk) in hex_str.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|_| GateError::VaultCorrupt {
            path: path.to_path_buf(),
            cause: "master key contains non-ASCII bytes".to_string(),
        })?;
        out[idx] = u8::from_str_radix(s, 16).map_err(|_| GateError::VaultCorrupt {
            path: path.to_path_buf(),
            cause: "master key contains non-hexadecimal characters".to_string(),
        })?;
    }
    Ok(out)
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

pub(crate) fn reject_if_symlink(path: &Path) -> Result<(), GateError> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(GateError::Io {
            op: "open",
            path: path.to_path_buf(),
            cause: "refusing to read or write through a symbolic link".to_string(),
        });
    }
    Ok(())
}

/// A temp path next to `path` that no other writer in this process or another one shares.
///
/// The process id alone made two threads saving one vault, or a key and a vault in the same
/// directory (`vault.key` and `vault.enc` both became `vault.tmp.<pid>`), write one temp file.
fn unique_tmp_path(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    path.with_file_name(format!(
        ".{}.tmp.{}.{n}",
        name.as_deref().unwrap_or("fl"),
        std::process::id()
    ))
}

/// Write `bytes` to a fresh `0600` temp file beside `path` and return its path. A failed write
/// removes the temp file instead of leaving partial key or ciphertext bytes behind.
fn write_tmp_0600(path: &Path, bytes: &[u8]) -> Result<PathBuf, GateError> {
    reject_if_symlink(path)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| GateError::Io {
            op: "create_dir_all",
            path: parent.to_path_buf(),
            cause: e.to_string(),
        })?;
    }
    let tmp_path = unique_tmp_path(path);
    let written = (|| -> std::io::Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp_path);
        return Err(GateError::Io {
            op: "write",
            path: tmp_path,
            cause: e.to_string(),
        });
    }
    Ok(tmp_path)
}

pub(crate) fn write_atomic_0600(path: &Path, bytes: &[u8]) -> Result<(), GateError> {
    let tmp_path = write_tmp_0600(path, bytes)?;
    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        GateError::Io {
            op: "rename",
            path: path.to_path_buf(),
            cause: e.to_string(),
        }
    })
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn args(name: &str, value: &str) -> SetSecretArgs {
        SetSecretArgs {
            name: name.into(),
            value: value.into(),
            allowed_hosts: vec!["example.com".into()],
            ..Default::default()
        }
    }

    fn leftover_temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect()
    }

    #[test]
    fn a_record_moved_under_another_name_is_not_served_as_that_secret() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("vault.enc");
        let mut vault = Vault::open(&path, [3u8; 32]).unwrap();
        vault.set(args("LOW_VALUE", "low-value-token")).unwrap();
        vault.set(args("PROD_TOKEN", "prod-token-value")).unwrap();

        let mut raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        raw["secrets"]["PROD_TOKEN"] = raw["secrets"]["LOW_VALUE"].clone();
        fs::write(&path, raw.to_string()).unwrap();

        let vault = Vault::open(&path, [3u8; 32]).unwrap();
        assert!(matches!(
            vault.decrypt_raw("PROD_TOKEN"),
            Err(GateError::VaultTampered { .. })
        ));
        assert_eq!(vault.decrypt_raw("LOW_VALUE").unwrap().1, "low-value-token");
    }

    #[test]
    fn temp_paths_are_unique_per_call_and_per_target_file() {
        let dir = Path::new("/data/fl");
        let key = unique_tmp_path(&dir.join("vault.key"));
        let vault = unique_tmp_path(&dir.join("vault.enc"));
        let vault_again = unique_tmp_path(&dir.join("vault.enc"));
        assert_ne!(
            key, vault,
            "vault.key and vault.enc once shared one temp name"
        );
        assert_ne!(
            vault, vault_again,
            "two saves in one process once shared one temp name"
        );
        assert_eq!(key.parent(), Some(dir));
    }

    /// Stress, not a deterministic repro: before the fix concurrent saves of one path raced on a
    /// shared `vault.tmp.<pid>` and failed with a missing-file rename or wrote interleaved bytes.
    #[test]
    fn concurrent_saves_of_one_vault_all_succeed_and_leave_it_readable() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("vault.enc");
        let base = Vault::open(&path, [5u8; 32]).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let mut vault = base.clone();
                std::thread::spawn(move || {
                    for i in 0..15 {
                        vault.set(args(&format!("S{t}_{i}"), "value-1234")).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let reopened = Vault::open(&path, [5u8; 32]).unwrap();
        assert!(!reopened.list().is_empty());
        assert_eq!(leftover_temp_files(tmp.path()), Vec::<String>::new());
    }

    #[test]
    fn a_failed_publish_removes_its_temp_file() {
        let tmp = TempDir::new().unwrap();
        // Renaming a file onto a directory fails after the temp file was written.
        let target = tmp.path().join("vault.enc");
        fs::create_dir(&target).unwrap();
        assert!(write_atomic_0600(&target, b"{}").is_err());
        assert_eq!(leftover_temp_files(tmp.path()), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn temp_files_are_born_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let tmp_path = write_tmp_0600(&tmp.path().join("vault.key"), b"k").unwrap();
        let mode = fs::metadata(&tmp_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn the_loser_of_a_first_run_race_adopts_the_existing_master_key() {
        let tmp = TempDir::new().unwrap();
        let key_path = tmp.path().join("vault.key");
        let winner = [0xAAu8; MASTER_KEY_LEN];
        let loser = [0xBBu8; MASTER_KEY_LEN];
        assert_eq!(install_new_master_key(&key_path, winner).unwrap(), winner);
        assert_eq!(install_new_master_key(&key_path, loser).unwrap(), winner);
        assert_eq!(
            parse_hex_master_key(fs::read_to_string(&key_path).unwrap().trim(), &key_path).unwrap(),
            winner
        );
        assert_eq!(leftover_temp_files(tmp.path()), Vec::<String>::new());
    }

    #[test]
    fn a_master_key_must_be_bare_hex_digits() {
        let p = Path::new("key");
        assert!(parse_hex_master_key(&"+1".repeat(MASTER_KEY_LEN), p).is_err());
        assert!(parse_hex_master_key(&"-1".repeat(MASTER_KEY_LEN), p).is_err());
        assert!(parse_hex_master_key(&" 1".repeat(MASTER_KEY_LEN), p).is_err());
        assert_eq!(
            parse_hex_master_key(&"0f".repeat(MASTER_KEY_LEN), p).unwrap(),
            [0x0f; MASTER_KEY_LEN]
        );
    }

    /// Stress: with two writes per entry, concurrent appenders could split an entry from its newline.
    #[test]
    fn concurrent_audit_entries_stay_one_json_object_per_line() {
        let tmp = TempDir::new().unwrap();
        let vault =
            std::sync::Arc::new(Vault::open(tmp.path().join("vault.enc"), [1u8; 32]).unwrap());
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let vault = vault.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        vault.append_audit_log(
                            Some(&format!("agent{t}")),
                            &["K".to_string()],
                            "GET",
                            "example.com",
                            "allowed",
                        );
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let log = fs::read_to_string(tmp.path().join("gate-audit.jsonl")).unwrap();
        assert!(log.ends_with('\n'));
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 200);
        for line in lines {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
    }
}
