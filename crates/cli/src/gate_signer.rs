//! Signs the GitHub App's JWT through the FerroGate Vault, for `[github] private_key_ref`.
//!
//! [`BrokeredSigner`] is the `fl-cli` side of `fl_github::JwtSigner` (`fl-github` does not depend
//! on `fl-gate`). Every signing opens the Vault again, READ-ONLY, and calls
//! [`fl_gate::Vault::sign_app_jwt`]: the policy (`allowed_hosts`, `allowed_agents`, lease) is
//! checked and audited by the Vault, and only the JWT comes back. No PEM enters this process
//! outside `fl-gate`'s zeroizing buffers.
//!
//! ⚠ There is no fallback. A missing Vault, a missing master key, a missing secret or a policy
//! denial is an error that names the secret and the reason; it never sends the caller to a key
//! file or to `$FL_GITHUB_PRIVATE_KEY_PATH`.
//!
//! ⚠ A read never creates anything: a missing Vault file or master key is refused here, before
//! `load_or_init_master_key` could generate a key (the rule `vendor_keys` follows).
//!
//! The `host` handed to the Vault is the host of the API URL the command will actually send the
//! JWT to (`api.github.com` by default; a GitHub Enterprise host, or `$FL_GITHUB_API_URL`'s host,
//! otherwise; see [`api_host`]). So the secret's `allowed_hosts` constrains where fl sends the
//! JWT: a URL whose host the secret does not list is denied and audited before the key is
//! decrypted. ⚠ The signer itself makes no connection, and the caller builds the host from the
//! URL it then uses; a process that skips this code is not constrained (see `docs/ferrogate.md`).
//! The local path is also advisory for `allowed_agents`.
//!
//! The master key is loaded with `fl_gate::load_existing_master_key`, which cannot create one, so
//! there is no window between a check and a load.

use crate::config::{AppKey, GithubApp};
use anyhow::Result;
use fl_core::StoreError;
use fl_gate::{
    Vault, VaultRef, load_existing_master_key, resolve_master_key_path, resolve_vault_path,
};
use fl_github::{FileKeySigner, JwtSigner};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// The host of the default API, which the Vault is told when no other API URL is in use.
pub const GITHUB_API_HOST: &str = "api.github.com";

/// The host a JWT sent to `api` goes to, for the Vault's `allowed_hosts` check: the URL's host,
/// lower-case, with no port. The default API is `api.github.com`; any other URL is parsed as
/// `$FL_GITHUB_API_URL` is (https, or http to this machine), and a URL that does not parse is an
/// error. A host the Vault cannot accept (for example a bracketed IPv6 address) is denied there.
pub fn api_host(api: &str) -> Result<String> {
    if api.trim_end_matches('/') == fl_github::DEFAULT_API {
        return Ok(GITHUB_API_HOST.to_string());
    }
    Ok(crate::api_override_host(api)?.to_ascii_lowercase())
}

/// The caller a CLI signing is attributed to when `$AGENT_PERSONA` is not set.
pub const CLI_CALLER: &str = "fl";

/// The caller the Hub's signings are attributed to.
pub const HUB_CALLER: &str = "fl-hub";

/// The caller a CLI command is: `$AGENT_PERSONA` when it is set and not blank (as `fl vault call`
/// reads it), else [`CLI_CALLER`]. The Vault trims, lower-cases and bounds it itself.
pub fn cli_caller() -> String {
    caller_from(std::env::var("AGENT_PERSONA").ok().as_deref())
}

fn caller_from(persona: Option<&str>) -> String {
    persona
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(CLI_CALLER)
        .to_string()
}

/// Where the Vault files are looked for, each time a JWT is signed.
#[derive(Debug, Clone)]
enum Locations {
    /// `fl vault`'s own rule: `FL_VAULT_PATH`, `FL_VAULT_KEY_PATH`, XDG, `$HOME`.
    Default,
    /// Named by the caller (tests).
    #[cfg(test)]
    Fixed { vault: PathBuf, key: TestKey },
}

/// Where a test's master key comes from.
#[cfg(test)]
#[derive(Debug, Clone)]
enum TestKey {
    /// Handed over directly, so the test never reads `FL_VAULT_MASTER_KEY` or a key file.
    Bytes([u8; fl_gate::MASTER_KEY_LEN]),
    /// The production loader, on this file.
    File(PathBuf),
}

/// A [`JwtSigner`] that asks the Vault to sign. See the module docs.
#[derive(Debug, Clone)]
pub struct BrokeredSigner {
    vault_ref: VaultRef,
    caller: String,
    host: String,
    at: Locations,
}

impl BrokeredSigner {
    /// A signer for the secret `vault_ref` names, acting as `caller`, for a JWT that goes to
    /// `host` (see [`api_host`]), with the Vault found by `fl vault`'s rule at each signing.
    pub fn new(vault_ref: VaultRef, caller: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            vault_ref,
            caller: caller.into(),
            host: host.into(),
            at: Locations::Default,
        }
    }

    /// As [`BrokeredSigner::new`], with the Vault file and master key named (tests).
    #[cfg(test)]
    fn at(
        vault_ref: VaultRef,
        caller: impl Into<String>,
        host: impl Into<String>,
        vault: PathBuf,
        key: TestKey,
    ) -> Self {
        Self {
            vault_ref,
            caller: caller.into(),
            host: host.into(),
            at: Locations::Fixed { vault, key },
        }
    }

    fn refuse(&self, why: impl std::fmt::Display) -> StoreError {
        StoreError::Credential(format!(
            "the App private key `{}` could not be used to sign: {why}",
            self.vault_ref
        ))
    }

    /// Opens the Vault read-only, fresh: the handle is a snapshot, so a policy change or a
    /// rotated key is seen only by a new one.
    fn open(&self) -> Result<Vault, StoreError> {
        match &self.at {
            Locations::Default => {
                let key_path = resolve_master_key_path(None);
                open_existing(&resolve_vault_path(None), &|| {
                    load_existing_master_key(&key_path)
                })
            }
            #[cfg(test)]
            Locations::Fixed { vault, key } => open_existing(vault, &|| match key {
                TestKey::Bytes(b) => Ok(*b),
                TestKey::File(path) => load_existing_master_key(path),
            }),
        }
        .map_err(|why| self.refuse(why))
    }
}

/// The Vault at `vault_path`, which must exist, with the master key `master` loads, which must
/// exist: `master` is [`load_existing_master_key`], which cannot create a key. Nothing is created.
fn open_existing(
    vault_path: &Path,
    master: &dyn Fn() -> Result<[u8; fl_gate::MASTER_KEY_LEN], fl_gate::GateError>,
) -> Result<Vault, String> {
    if !vault_path.is_file() {
        return Err(format!(
            "there is no Vault file at {}",
            vault_path.display()
        ));
    }
    let master = master().map_err(|e| e.to_string())?;
    Vault::open(vault_path, master).map_err(|e| e.to_string())
}

impl JwtSigner for BrokeredSigner {
    fn sign_app_jwt(&self, app_id: &str, now: SystemTime) -> Result<String, StoreError> {
        let vault = self.open()?;
        vault
            .sign_app_jwt(&self.vault_ref, app_id, &self.caller, &self.host, now)
            .map_err(|e| self.refuse(e))
    }
}

/// The signer `[github]` names: the PEM file, read and checked now as it always was, or the
/// Vault, which is not touched until a JWT is signed. `caller` is the identity the Vault is told
/// and `host` the host of the API URL the JWT will be sent to ([`api_host`]).
pub fn signer_for(app: &GithubApp, caller: &str, host: &str) -> Result<Arc<dyn JwtSigner>> {
    Ok(match app.key()? {
        AppKey::File(path) => Arc::new(FileKeySigner::open(&path)?),
        AppKey::Vault(r) => Arc::new(BrokeredSigner::new(r, caller, host)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_gate::{SetSecretArgs, parse_vault_ref};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn the_caller_is_the_persona_else_fl() {
        assert_eq!(caller_from(Some(" Ferris ")), "Ferris");
        assert_eq!(caller_from(Some("  ")), CLI_CALLER);
        assert_eq!(caller_from(None), CLI_CALLER);
    }

    const TEST_MASTER: [u8; fl_gate::MASTER_KEY_LEN] = [7; fl_gate::MASTER_KEY_LEN];

    fn app_key_ref() -> VaultRef {
        parse_vault_ref("vault://APP_KEY").unwrap()
    }

    #[test]
    fn a_missing_vault_or_master_key_is_refused_and_creates_nothing() {
        // The production loader reads `$FL_VAULT_MASTER_KEY`; a developer who has it set would
        // make a key that does not exist here appear to exist.
        if std::env::var_os("FL_VAULT_MASTER_KEY").is_some_and(|v| !v.is_empty()) {
            eprintln!("SKIPPED: FL_VAULT_MASTER_KEY is set in this environment");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("sub/vault.enc");
        let key = dir.path().join("sub/vault.key");
        let signer = BrokeredSigner::at(
            app_key_ref(),
            "fl",
            GITHUB_API_HOST,
            vault.clone(),
            TestKey::File(key.clone()),
        );
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let err = signer.sign_app_jwt("42", now).unwrap_err().to_string();
        assert!(err.contains("vault://APP_KEY"), "{err}");
        assert!(err.contains("no Vault file"), "{err}");
        // A Vault file without a master key.
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(&vault, "{}").unwrap();
        let err = signer.sign_app_jwt("42", now).unwrap_err().to_string();
        assert!(err.contains("no Vault master key"), "{err}");
        assert!(!key.exists(), "a read must not create a master key");
        assert!(!dir.path().join("sub/gate-audit.jsonl").exists());
    }

    #[test]
    fn a_missing_secret_names_the_secret_and_audits_the_denial() {
        let f = Fixture::new();
        f.set_named("OTHER", "x", &[], &[]);
        let err = f
            .signer("fl", GITHUB_API_HOST)
            .sign_app_jwt("42", SystemTime::now())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("APP_KEY") && err.contains("does not exist"),
            "{err}"
        );
        assert!(f.audit()[0]["outcome"] == "denied:no_such_secret");
    }

    #[test]
    fn the_api_host_is_the_urls_host_lowercase_without_a_port() {
        for (url, host) in [
            ("https://api.github.com", "api.github.com"),
            ("https://api.github.com/", "api.github.com"),
            ("https://GHE.Example.com/api/v3", "ghe.example.com"),
            ("https://ghe.example.com:8443/api/v3", "ghe.example.com"),
            ("http://127.0.0.1:34567", "127.0.0.1"),
            ("http://localhost:1", "localhost"),
        ] {
            assert_eq!(api_host(url).unwrap(), host, "{url}");
        }
        // Not allowed as an API URL at all.
        for bad in ["http://evil.example", "https://u:p@ghe.example.com", "nope"] {
            assert!(api_host(bad).is_err(), "{bad}");
        }
        // A bracketed IPv6 host parses; the Vault's host shape check then denies it.
        assert_eq!(api_host("http://[::1]:9").unwrap(), "[::1]");
    }

    /// A throwaway RSA key made by the `openssl` CLI, so no key is ever committed. `None` (and a
    /// printed notice) when the CLI is missing, except in CI, where that is a failure.
    fn throwaway_pem(traditional: bool) -> Option<String> {
        let mut args = vec!["genrsa"];
        if traditional {
            args.push("-traditional");
        }
        args.push("2048");
        match std::process::Command::new("openssl").args(&args).output() {
            Ok(out) if out.status.success() => Some(String::from_utf8(out.stdout).unwrap()),
            _ => {
                assert!(
                    std::env::var_os("CI").is_none(),
                    "the `openssl` CLI is required in CI to make a throwaway RSA key"
                );
                eprintln!("SKIPPED: the `openssl` CLI is needed to make a throwaway RSA key");
                None
            }
        }
    }

    fn verifies(pem: &str, jwt: &str) -> bool {
        use base64::Engine;
        use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
        let b64: String = pem
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let der = STANDARD.decode(b64).unwrap();
        let key = if pem.contains("BEGIN RSA PRIVATE KEY") {
            ring::signature::RsaKeyPair::from_der(&der)
        } else {
            ring::signature::RsaKeyPair::from_pkcs8(&der)
        }
        .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        parts.len() == 3
            && ring::signature::UnparsedPublicKey::new(
                &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                key.public().as_ref(),
            )
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
            )
            .is_ok()
    }

    struct Fixture {
        dir: tempfile::TempDir,
        vault: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let vault = dir.path().join("vault.enc");
            Self { dir, vault }
        }
        /// Store (or replace) `value` as `name`, with the given policy.
        fn set_named(&self, name: &str, value: &str, hosts: &[&str], agents: &[&str]) {
            let mut vault = Vault::open(&self.vault, TEST_MASTER).unwrap();
            vault
                .set(SetSecretArgs {
                    name: name.into(),
                    value: value.into(),
                    allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
                    allowed_agents: agents.iter().map(|a| a.to_string()).collect(),
                    ..SetSecretArgs::default()
                })
                .unwrap();
        }
        /// Store (or replace) `pem` as APP_KEY, with the given policy.
        fn set(&self, pem: &str, hosts: &[&str], agents: &[&str]) {
            self.set_named("APP_KEY", pem, hosts, agents);
        }
        fn signer(&self, caller: &str, host: &str) -> BrokeredSigner {
            BrokeredSigner::at(
                app_key_ref(),
                caller,
                host,
                self.vault.clone(),
                TestKey::Bytes(TEST_MASTER),
            )
        }
        fn audit(&self) -> Vec<serde_json::Value> {
            std::fs::read_to_string(self.dir.path().join("gate-audit.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    #[test]
    fn the_vault_signs_for_github_and_the_audit_holds_the_name_only() {
        let Some(pem) = throwaway_pem(true) else {
            return;
        };
        let f = Fixture::new();
        f.set(&pem, &[GITHUB_API_HOST], &[]);
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let jwt = f
            .signer("ferris", GITHUB_API_HOST)
            .sign_app_jwt("42", now)
            .unwrap();
        assert!(
            verifies(&pem, &jwt),
            "the JWT verifies under the stored key"
        );
        let log = f.audit();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0]["outcome"], "allowed");
        assert_eq!(log[0]["method"], fl_gate::SIGN_AUDIT_METHOD);
        assert_eq!(log[0]["host"], GITHUB_API_HOST);
        assert_eq!(log[0]["caller"], "ferris");
        assert_eq!(log[0]["secrets"], serde_json::json!(["APP_KEY"]));
        let text = std::fs::read_to_string(f.dir.path().join("gate-audit.jsonl")).unwrap();
        assert!(!text.contains(&jwt) && !text.contains("PRIVATE KEY"));
    }

    #[test]
    fn a_policy_denial_is_an_error_naming_the_secret_and_the_reason_and_is_audited() {
        let Some(pem) = throwaway_pem(false) else {
            return;
        };
        let f = Fixture::new();
        let now = SystemTime::now();
        // No allowed_hosts: denied on the host.
        f.set(&pem, &[], &[]);
        let err = f
            .signer("fl", GITHUB_API_HOST)
            .sign_app_jwt("42", now)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("vault://APP_KEY") && err.contains("allowed_hosts"),
            "{err}"
        );
        // The host fits but the caller is not listed.
        f.set(&pem, &[GITHUB_API_HOST], &["somebody-else"]);
        let err = f
            .signer("fl", GITHUB_API_HOST)
            .sign_app_jwt("42", now)
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed_agents"), "{err}");
        let outcomes: Vec<_> = f.audit().iter().map(|e| e["outcome"].clone()).collect();
        assert_eq!(outcomes, ["denied:host", "denied:agent"]);
        assert!(!err.contains("PRIVATE KEY"));
    }

    #[test]
    fn the_vault_is_opened_again_for_every_signing() {
        let (Some(first), Some(second)) = (throwaway_pem(true), throwaway_pem(false)) else {
            return;
        };
        let f = Fixture::new();
        f.set(&first, &[GITHUB_API_HOST], &[]);
        let signer = f.signer("fl", GITHUB_API_HOST);
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let a = signer.sign_app_jwt("42", now).unwrap();
        assert!(verifies(&first, &a));
        // A rotated key written after the first signing is the key the next signing uses.
        f.set(&second, &[GITHUB_API_HOST], &[]);
        let b = signer.sign_app_jwt("42", now).unwrap();
        assert!(verifies(&second, &b) && !verifies(&first, &b));
        // So is a tightened policy: the same signer value is now refused.
        f.set(&second, &["elsewhere.example"], &[]);
        let err = signer.sign_app_jwt("42", now).unwrap_err().to_string();
        assert!(err.contains("allowed_hosts"), "{err}");
    }
    #[test]
    fn allowed_hosts_constrains_the_api_host_the_jwt_goes_to() {
        let Some(pem) = throwaway_pem(true) else {
            return;
        };
        let f = Fixture::new();
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        // A GitHub Enterprise host, and a local stub, work when the secret lists them.
        f.set(&pem, &["ghe.example.com", "127.0.0.1"], &[]);
        for host in ["ghe.example.com", "127.0.0.1"] {
            let jwt = f.signer("fl", host).sign_app_jwt("42", now).unwrap();
            assert!(verifies(&pem, &jwt));
        }
        // The default host is not listed: a command pointed at GitHub is denied.
        let err = f
            .signer("fl", GITHUB_API_HOST)
            .sign_app_jwt("42", now)
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed_hosts"), "{err}");
        // A secret that lists only GitHub is denied for any other API host, and audited under
        // the host the JWT would have gone to.
        f.set(&pem, &[GITHUB_API_HOST], &[]);
        let err = f
            .signer("fl", "evil.example")
            .sign_app_jwt("42", now)
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed_hosts"), "{err}");
        let log = f.audit();
        let last = log.last().unwrap();
        assert_eq!(last["outcome"], "denied:host");
        assert_eq!(last["host"], "evil.example");
        // A host the Vault cannot read as a host name is denied too.
        assert!(f.signer("fl", "[::1]").sign_app_jwt("42", now).is_err());
    }
}
