//! `[github] private_key_ref = "vault://NAME"`: the App's JWT is signed by the FerroGate Vault,
//! through the real `fl` binary, against the in-process fake GitHub. Everything is hermetic: a
//! temp Vault holding a throwaway RSA key made by the `openssl` CLI, a temp config, and the
//! fake on 127.0.0.1. No real credential, Vault or network is touched.

use assert_cmd::Command;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use fl_gate::{SetSecretArgs, Vault, load_or_init_master_key};
use fl_github::fake::{APP_SLUG, FakeGithub};
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaKeyPair, UnparsedPublicKey};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Sys;

const SECRET: &str = "GITHUB_APP_KEY";

/// A throwaway RSA key made by the `openssl` CLI. `None` (and a notice) when the CLI is missing,
/// except in CI, where losing this coverage silently is a failure.
fn throwaway_pem() -> Option<String> {
    match Sys::new("openssl")
        .args(["genrsa", "-traditional", "2048"])
        .output()
    {
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

fn pem_body(pem: &str) -> String {
    pem.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("-----"))
        .collect()
}

fn verifies(pem: &str, jwt: &str) -> bool {
    let key = RsaKeyPair::from_der(&STANDARD.decode(pem_body(pem)).unwrap()).unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    parts.len() == 3
        && UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref())
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
            )
            .is_ok()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Sys::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct G {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    /// Where the Vault files live; the directory may not exist.
    vault_dir: PathBuf,
    fake: FakeGithub,
}

impl G {
    /// A project bound to the fake's `acme/widgets` with `credential = "app"` and the given
    /// `[github]` key lines.
    fn new(github_keys: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.email", "t@example.com"]);
        git(repo.path(), &["config", "user.name", "t"]);
        fs::write(repo.path().join("a.txt"), "a").unwrap();
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-qm", "first"]);
        let cfg = format!(
            "[github]\napp_id = 42\n{github_keys}\n\
             [[project]]\nroot = \"{}\"\nstore = \"{}\"\n\
             tracker = {{ github = \"acme/widgets\", credential = \"app\" }}\n",
            repo.path().canonicalize().unwrap().display(),
            home.path().join("fl.redb").display()
        );
        fs::create_dir_all(home.path().join("config/fl")).unwrap();
        fs::write(home.path().join("config/fl/config.toml"), cfg).unwrap();
        let vault_dir = home.path().join("vault");
        Self {
            home,
            repo,
            vault_dir,
            fake: FakeGithub::start("acme/widgets"),
        }
    }

    fn with_ref() -> Self {
        Self::new(&format!("private_key_ref = \"vault://{SECRET}\"\n"))
    }

    fn vault_path(&self) -> PathBuf {
        self.vault_dir.join("vault.enc")
    }
    fn key_path(&self) -> PathBuf {
        self.vault_dir.join("vault.key")
    }
    fn audit_path(&self) -> PathBuf {
        self.vault_dir.join("gate-audit.jsonl")
    }

    /// Store `pem` as the secret, with the given policy. Creates the Vault and its master key.
    fn store(&self, pem: &str, hosts: &[&str], agents: &[&str]) {
        fs::create_dir_all(&self.vault_dir).unwrap();
        let master = load_or_init_master_key(&self.key_path()).unwrap();
        let mut vault = Vault::open(self.vault_path(), master).unwrap();
        vault
            .set(SetSecretArgs {
                name: SECRET.into(),
                value: pem.into(),
                allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
                allowed_agents: agents.iter().map(|a| a.to_string()).collect(),
                ..SetSecretArgs::default()
            })
            .unwrap();
    }

    fn audit(&self) -> Vec<Value> {
        fs::read_to_string(self.audit_path())
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("FL_VAULT_PATH", self.vault_path())
            .env("FL_VAULT_KEY_PATH", self.key_path())
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env("AGENT_PERSONA", "ferris")
            .env_remove("FL_VAULT_MASTER_KEY")
            .env_remove("FL_GITHUB_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .env_remove("FL_GITHUB_APP_ID")
            .env_remove("FL_GITHUB_PRIVATE_KEY_PATH")
            .env_remove("FL_DB")
            .current_dir(self.repo.path());
        c
    }

    fn whoami(&self) -> assert_cmd::assert::Assert {
        self.fl().args(["github", "whoami"]).assert()
    }

    fn nothing_reached_github(&self) {
        let s = self.fake.state();
        assert!(s.requests.is_empty(), "requests: {:?}", s.requests);
        assert!(s.jwts.is_empty());
    }
}

#[test]
fn the_vault_signs_the_apps_jwt_and_the_audit_log_holds_the_name_only() {
    let Some(pem) = throwaway_pem() else {
        return;
    };
    let g = G::with_ref();
    g.store(&pem, &["127.0.0.1"], &["ferris"]);
    g.whoami().success().stdout(
        contains(format!("{APP_SLUG}[bot]")).and(contains("GitHub App 42 (installation token)")),
    );
    let s = g.fake.state();
    assert!(!s.jwts.is_empty(), "the App called GitHub with a JWT");
    for jwt in &s.jwts {
        assert!(
            verifies(&pem, jwt),
            "the JWT verifies against the key's public half"
        );
        let claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(jwt.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["iss"], "42");
    }
    let log = g.audit();
    assert!(!log.is_empty());
    assert_eq!(
        log.len(),
        s.jwts.len(),
        "one audit entry per JWT the Vault signed"
    );
    for entry in &log {
        assert_eq!(entry["outcome"], "allowed");
        assert_eq!(entry["method"], "SIGN_RS256_JWT");
        // The host of the API URL the JWT is sent to (the stub), not a fixed label.
        assert_eq!(entry["host"], "127.0.0.1");
        assert_eq!(entry["caller"], "ferris");
        assert_eq!(entry["secrets"], json!([SECRET]));
    }
    // The log holds a name, never a value or a JWT.
    let text = fs::read_to_string(g.audit_path()).unwrap();
    assert!(!text.contains(&pem_body(&pem)[..40]) && !text.contains("PRIVATE KEY"));
    assert!(s.jwts.iter().all(|j| !text.contains(j)));
}

#[test]
fn a_policy_denial_fails_the_command_audits_it_and_sends_nothing_to_github() {
    let Some(pem) = throwaway_pem() else {
        return;
    };
    // The API URL is not GitHub's (it is the stub on 127.0.0.1) and allowed_hosts names only
    // GitHub: the JWT may not go there.
    let g = G::with_ref();
    g.store(&pem, &["api.github.com"], &[]);
    g.whoami()
        .failure()
        .stderr(contains(SECRET).and(contains("allowed_hosts")))
        .stderr(contains("PRIVATE KEY").not())
        .stderr(contains(&pem_body(&pem)[..40]).not());
    g.nothing_reached_github();
    let log = g.audit();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["outcome"], "denied:host");
    assert_eq!(
        log[0]["host"], "127.0.0.1",
        "audited under the host it would have gone to"
    );
    assert_eq!(log[0]["secrets"], json!([SECRET]));

    // allowed_agents names someone else: refused on the caller, audited, nothing sent.
    let g = G::with_ref();
    g.store(&pem, &["127.0.0.1"], &["somebody-else"]);
    g.whoami().failure().stderr(contains("allowed_agents"));
    g.nothing_reached_github();
    assert_eq!(g.audit()[0]["outcome"], "denied:agent");
}

#[test]
fn the_host_the_vault_is_asked_about_is_the_apis_own_and_a_port_is_not_part_of_it() {
    let Some(pem) = throwaway_pem() else {
        return;
    };
    // An enterprise-style host (`localhost` here, reached through the same stub) listed in
    // allowed_hosts works, and the audit names that host with no port.
    let g = G::with_ref();
    g.store(&pem, &["localhost"], &[]);
    let url = g.fake.url().replace("127.0.0.1", "localhost");
    g.fl()
        .env("FL_GITHUB_API_URL", &url)
        .args(["github", "whoami"])
        .assert()
        .success()
        .stderr(contains("talking to"));
    let log = g.audit();
    assert!(!log.is_empty());
    assert!(
        log.iter()
            .all(|e| e["host"] == "localhost" && e["outcome"] == "allowed")
    );
    assert!(g.fake.state().jwts.iter().all(|j| verifies(&pem, j)));

    // A secret that lists only `127.0.0.1` does not admit `localhost`: nothing is sent.
    let g = G::with_ref();
    g.store(&pem, &["127.0.0.1"], &[]);
    let url = g.fake.url().replace("127.0.0.1", "localhost");
    g.fl()
        .env("FL_GITHUB_API_URL", &url)
        .args(["github", "whoami"])
        .assert()
        .failure()
        .stderr(contains("allowed_hosts"));
    g.nothing_reached_github();
    assert_eq!(g.audit()[0]["outcome"], "denied:host");
}

#[test]
fn an_absent_vault_or_secret_fails_naming_the_secret_and_creates_nothing() {
    // No Vault at all: the failure names the secret, and the read creates no Vault and no key.
    let g = G::with_ref();
    g.whoami()
        .failure()
        .stderr(contains(format!("vault://{SECRET}")).and(contains("no Vault file")));
    g.nothing_reached_github();
    assert!(!g.vault_dir.exists(), "a read must not create a Vault");

    // A Vault that does not hold the secret.
    let Some(pem) = throwaway_pem() else {
        return;
    };
    let g = G::with_ref();
    g.store(&pem, &["127.0.0.1"], &[]);
    let other = G::new("private_key_ref = \"vault://SOME_OTHER_KEY\"\n");
    fs::create_dir_all(&other.vault_dir).unwrap();
    fs::copy(g.vault_path(), other.vault_path()).unwrap();
    fs::copy(g.key_path(), other.key_path()).unwrap();
    other
        .whoami()
        .failure()
        .stderr(contains("SOME_OTHER_KEY").and(contains("does not exist")));
    other.nothing_reached_github();
}

#[test]
fn a_private_key_ref_never_falls_back_to_the_environments_key_file() {
    let Some(pem) = throwaway_pem() else {
        return;
    };
    // A perfectly good legacy key file and App id in the environment, and a ref in the config
    // whose Vault is absent: the command is refused as ambiguous, it does not sign with the file.
    let g = G::with_ref();
    let legacy = g.home.path().join("legacy.pem");
    fs::write(&legacy, &pem).unwrap();
    g.fl()
        .env("FL_GITHUB_APP_ID", "42")
        .env("FL_GITHUB_PRIVATE_KEY_PATH", &legacy)
        .args(["github", "whoami"])
        .assert()
        .failure()
        .stderr(
            contains("FL_GITHUB_PRIVATE_KEY_PATH")
                .and(contains("private_key_ref"))
                .and(contains("ambiguous")),
        );
    g.nothing_reached_github();
    assert!(!g.vault_dir.exists());

    // The same with a working Vault: still refused, and the Vault is not asked.
    let g = G::with_ref();
    g.store(&pem, &["127.0.0.1"], &[]);
    let legacy = g.home.path().join("legacy.pem");
    fs::write(&legacy, &pem).unwrap();
    g.fl()
        .env("FL_GITHUB_PRIVATE_KEY_PATH", &legacy)
        .args(["github", "whoami"])
        .assert()
        .failure()
        .stderr(contains("ambiguous"));
    g.nothing_reached_github();
    assert!(g.audit().is_empty(), "the Vault was not asked to sign");
}

#[test]
fn a_config_with_both_keys_or_a_bad_ref_is_refused_at_load() {
    let g = G::new("private_key = \"/k/key.pem\"\nprivate_key_ref = \"vault://X\"\n");
    g.whoami()
        .failure()
        .stderr(contains("`private_key`").and(contains("`private_key_ref`")));
    g.nothing_reached_github();

    let g = G::new("private_key_ref = \"not-a-ref-SECRETISH\"\n");
    g.whoami()
        .failure()
        .stderr(contains("`private_key_ref` is not a vault reference"))
        .stderr(contains("SECRETISH").not());
    g.nothing_reached_github();
    assert!(!g.vault_dir.exists());
}

#[test]
fn loading_a_ref_config_for_a_command_that_needs_no_key_touches_no_vault() {
    // `fl --version`-style commands load nothing; `project list` loads the config and does not
    // build the App's credential. The Vault directory must stay absent: no Vault, no master key.
    let g = G::with_ref();
    g.fl().args(["project", "list"]).assert().success();
    assert!(
        !g.vault_dir.exists(),
        "config load must not create a Vault or a master key"
    );
}

#[test]
fn the_legacy_key_file_still_signs_when_no_ref_is_set() {
    let Some(pem) = throwaway_pem() else {
        return;
    };
    let g = G::new("");
    let key = g.home.path().join("app.pem");
    fs::write(&key, &pem).unwrap();
    // Rewrite the config's `[github]` with a file key.
    let cfg_path = g.home.path().join("config/fl/config.toml");
    let cfg = fs::read_to_string(&cfg_path).unwrap().replace(
        "app_id = 42\n",
        &format!("app_id = 42\nprivate_key = \"{}\"\n", key.display()),
    );
    fs::write(&cfg_path, cfg).unwrap();
    g.whoami()
        .success()
        .stdout(contains(format!("{APP_SLUG}[bot]")));
    assert!(g.fake.state().jwts.iter().all(|j| verifies(&pem, j)));
    assert!(!g.vault_dir.exists());
}
