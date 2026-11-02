//! Where an API vendor's key comes from when `fl` runs an attempt: the environment variable
//! first, then the FerroGate Vault.
//!
//! ⚠ The Vault read goes through the same policy check as `fl vault call`
//! ([`fl_gate::prepare_proxied_request`]), attributed to the agent [`CALLER`]. So a secret is
//! released only when its `allowed_hosts` name the vendor's host (and its `allow_private_network`
//! is set for a loopback or LAN engine), when its `allowed_methods` allow `POST`, when
//! `allowed_agents` is empty or lists `fl-exec` (or `*`), and while its lease is alive. The read
//! is written to the Vault's audit log. Nothing here widens that policy: a secret the owner has
//! not opened to `fl-exec` for the vendor's host stays closed, and the attempt is refused.
//!
//! ⚠ Every failure is `None` and says nothing: no Vault, no master key, a missing, expired or
//! tampered secret, a policy denial. The key is never printed, logged or put in an error, and
//! nothing here creates a Vault or a master key as a side effect of a read.

use fl_core::vendor::{VendorDescriptor, VendorTransport};
use fl_exec::adapters::ApiKeyResolver;
use fl_gate::{
    ProxyRequest, Vault, load_or_init_master_key, normalize_secret_name, prepare_proxied_request,
    resolve_master_key_path, resolve_vault_path,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// The agent name a Vault read is attributed to (the Vault's `allowed_agents` and audit log).
pub const CALLER: &str = "fl-exec";

/// The header that carries the placeholder, so the substituted value is exactly the key.
const KEY_HEADER: &str = "x-fl-key";

/// A resolver for an API vendor's key: the vendor's environment variable if it is set and not
/// blank, else its Vault secret. `base_url` is the address the attempt will send the key to
/// (an override from config, if any); `None` means the vendor's own default. The Vault policy is
/// checked against that address, so the key can only go where its secret allows.
///
/// Locations follow `fl vault`: `FL_VAULT_PATH`, `FL_VAULT_KEY_PATH`, `FL_VAULT_MASTER_KEY`,
/// else `$XDG_DATA_HOME` / `$XDG_CONFIG_HOME` / `$HOME`.
pub fn vault_key_resolver(base_url: Option<String>) -> ApiKeyResolver {
    Arc::new(move |d: &VendorDescriptor| {
        resolve_key(
            d,
            base_url.as_deref(),
            &|name| std::env::var(name).ok(),
            &resolve_vault_path(None),
            &resolve_master_key_path(None),
        )
    })
}

/// [`vault_key_resolver`] with the environment and the Vault files named by the caller.
fn resolve_key(
    d: &VendorDescriptor,
    base_url: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
    vault_path: &Path,
    key_path: &Path,
) -> Option<String> {
    let VendorTransport::HttpApi {
        default_base_url,
        vault_secret_name,
        env_key_name,
        ..
    } = &d.transport
    else {
        return None;
    };
    if let Some(name) = env_key_name
        && let Some(value) = env(name).map(|v| v.trim().to_string())
        && !value.is_empty()
    {
        return Some(value);
    }
    let secret = normalize_secret_name(vault_secret_name.as_deref()?).ok()?;
    let url = base_url.unwrap_or(default_base_url);
    from_vault(&secret, url, vault_path, key_path)
}

/// Reads `secret` from the Vault for a request to `url`, under the Vault's policy.
fn from_vault(secret: &str, url: &str, vault_path: &Path, key_path: &Path) -> Option<String> {
    // `load_or_init_master_key` would create a master key; a read must not.
    let env_key = std::env::var_os("FL_VAULT_MASTER_KEY").is_some_and(|v| !v.is_empty());
    if !vault_path.is_file() || !(env_key || key_path.exists()) {
        return None;
    }
    let master_key = load_or_init_master_key(key_path).ok()?;
    let vault = Vault::open(vault_path, master_key).ok()?;
    let request = ProxyRequest {
        method: "POST".into(),
        url: url.to_string(),
        headers: BTreeMap::from([(
            KEY_HEADER.to_string(),
            format!(
                "{}{secret}{}",
                fl_gate::PLACEHOLDER_PREFIX,
                fl_gate::PLACEHOLDER_SUFFIX
            ),
        )]),
        body: None,
        caller_agent: Some(CALLER.to_string()),
        timeout_secs: None,
    };
    let prepared = prepare_proxied_request(&vault, &request).ok()?;
    let key = prepared.headers.get(KEY_HEADER)?.trim().to_string();
    (!key.is_empty()).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::lookup_vendor;
    use fl_gate::SetSecretArgs;

    fn vendor(env_key: Option<&str>, vault_secret: Option<&str>, base: &str) -> VendorDescriptor {
        let mut d = lookup_vendor("openai-api").unwrap();
        d.id = "vendor-keys-test".into();
        d.transport = VendorTransport::HttpApi {
            default_base_url: base.into(),
            api_dialect: fl_core::ApiDialect::OpenAiChatCompletions,
            vault_secret_name: vault_secret.map(str::to_string),
            env_key_name: env_key.map(str::to_string),
        };
        d
    }

    /// A Vault in a temp dir holding `name` = `value`, open to `fl-exec` on `host`.
    struct Fixture {
        _dir: tempfile::TempDir,
        vault: std::path::PathBuf,
        key: std::path::PathBuf,
    }

    fn fixture(name: &str, value: &str, host: &str, agents: &[&str], private: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path().join("vault.enc");
        let key_path = dir.path().join("vault.key");
        let master = load_or_init_master_key(&key_path).unwrap();
        let mut vault = Vault::open(&vault_path, master).unwrap();
        vault
            .set(SetSecretArgs {
                name: name.into(),
                value: value.into(),
                allowed_hosts: vec![host.into()],
                allowed_agents: agents.iter().map(|a| a.to_string()).collect(),
                allow_private_network: private,
                ..SetSecretArgs::default()
            })
            .unwrap();
        Fixture {
            vault: vault_path,
            key: key_path,
            _dir: dir,
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn the_environment_wins_when_it_is_set() {
        let f = fixture("VK_KEY", "from-vault", "api.example.com", &[], false);
        let d = vendor(Some("VK_ENV"), Some("VK_KEY"), "https://api.example.com/v1");
        let env = |n: &str| (n == "VK_ENV").then(|| "  from-env \n".to_string());
        assert_eq!(
            resolve_key(&d, None, &env, &f.vault, &f.key).as_deref(),
            Some("from-env")
        );
    }

    #[test]
    fn a_blank_variable_falls_through_to_the_vault() {
        let f = fixture("VK_KEY", "from-vault", "api.example.com", &[], false);
        let d = vendor(Some("VK_ENV"), Some("VK_KEY"), "https://api.example.com/v1");
        let env = |_: &str| Some("   ".to_string());
        assert_eq!(
            resolve_key(&d, None, &env, &f.vault, &f.key).as_deref(),
            Some("from-vault")
        );
    }

    #[test]
    fn the_vault_is_used_when_the_environment_is_unset() {
        let f = fixture("VK_KEY", "from-vault", "api.example.com", &[], false);
        let d = vendor(Some("VK_ENV"), Some("vk_key"), "https://api.example.com/v1");
        assert_eq!(
            resolve_key(&d, None, &no_env, &f.vault, &f.key).as_deref(),
            Some("from-vault")
        );
        // The read is attributed to `fl-exec` in the audit log.
        let audit =
            std::fs::read_to_string(f.vault.parent().unwrap().join("gate-audit.jsonl")).unwrap();
        assert!(audit.contains("\"caller\":\"fl-exec\""), "{audit}");
        assert!(!audit.contains("from-vault"), "{audit}");
    }

    #[test]
    fn a_missing_secret_or_a_vendor_with_no_key_name_gives_none() {
        let f = fixture("VK_KEY", "from-vault", "api.example.com", &[], false);
        let missing = vendor(None, Some("OTHER_KEY"), "https://api.example.com/v1");
        assert_eq!(resolve_key(&missing, None, &no_env, &f.vault, &f.key), None);
        let keyless = vendor(None, None, "https://api.example.com/v1");
        assert_eq!(resolve_key(&keyless, None, &no_env, &f.vault, &f.key), None);
        let mut cli = lookup_vendor("claude").unwrap();
        cli.id = "vendor-keys-cli".into();
        assert_eq!(resolve_key(&cli, None, &no_env, &f.vault, &f.key), None);
    }

    #[test]
    fn no_vault_and_no_master_key_give_none_and_create_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (vault, key) = (dir.path().join("vault.enc"), dir.path().join("vault.key"));
        let d = vendor(None, Some("VK_KEY"), "https://api.example.com/v1");
        assert_eq!(resolve_key(&d, None, &no_env, &vault, &key), None);
        assert!(!vault.exists() && !key.exists(), "a read creates nothing");
        // A Vault with no master key is not initialised by the read either.
        let f = fixture("VK_KEY", "v", "api.example.com", &[], false);
        std::fs::remove_file(&f.key).unwrap();
        assert_eq!(resolve_key(&d, None, &no_env, &f.vault, &f.key), None);
        assert!(!f.key.exists());
    }

    #[test]
    fn a_policy_denial_gives_none() {
        let d = vendor(None, Some("VK_KEY"), "https://api.example.com/v1");
        // The secret is open to another host.
        let f = fixture("VK_KEY", "k", "elsewhere.example.org", &[], false);
        assert_eq!(resolve_key(&d, None, &no_env, &f.vault, &f.key), None);
        // The secret names agents, and `fl-exec` is not one of them.
        let f = fixture("VK_KEY", "k", "api.example.com", &["ferris"], false);
        assert_eq!(resolve_key(&d, None, &no_env, &f.vault, &f.key), None);
        // ... until it is.
        let f = fixture("VK_KEY", "k", "api.example.com", &["fl-exec"], false);
        assert_eq!(
            resolve_key(&d, None, &no_env, &f.vault, &f.key).as_deref(),
            Some("k")
        );
        // A wrong master key cannot decrypt.
        let other = tempfile::tempdir().unwrap();
        let wrong = other.path().join("vault.key");
        load_or_init_master_key(&wrong).unwrap();
        assert_eq!(resolve_key(&d, None, &no_env, &f.vault, &wrong), None);
    }

    #[test]
    fn the_policy_is_checked_against_the_address_the_key_will_go_to() {
        let f = fixture("VK_KEY", "k", "api.example.com", &[], false);
        let d = vendor(None, Some("VK_KEY"), "https://api.example.com/v1");
        // An override to another host is refused, so the key cannot be sent there.
        assert_eq!(
            resolve_key(
                &d,
                Some("https://evil.example.net/v1"),
                &no_env,
                &f.vault,
                &f.key
            ),
            None
        );
        // A loopback engine needs a secret that allows the private network.
        let local = vendor(None, Some("VK_KEY"), "http://127.0.0.1:8000/v1");
        assert_eq!(resolve_key(&local, None, &no_env, &f.vault, &f.key), None);
        let f = fixture("VK_KEY", "k", "127.0.0.1", &[], true);
        assert_eq!(
            resolve_key(&local, None, &no_env, &f.vault, &f.key).as_deref(),
            Some("k")
        );
    }

    #[test]
    fn an_expired_secret_gives_none() {
        let dir = tempfile::tempdir().unwrap();
        let (vault_path, key_path) = (dir.path().join("vault.enc"), dir.path().join("vault.key"));
        let master = load_or_init_master_key(&key_path).unwrap();
        let mut vault = Vault::open(&vault_path, master).unwrap();
        vault
            .set(SetSecretArgs {
                name: "VK_KEY".into(),
                value: "k".into(),
                allowed_hosts: vec!["api.example.com".into()],
                lease_ttl_secs: Some(1),
                ..SetSecretArgs::default()
            })
            .unwrap();
        let d = vendor(None, Some("VK_KEY"), "https://api.example.com/v1");
        assert_eq!(
            resolve_key(&d, None, &no_env, &vault_path, &key_path).as_deref(),
            Some("k")
        );
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert_eq!(resolve_key(&d, None, &no_env, &vault_path, &key_path), None);
    }
}
