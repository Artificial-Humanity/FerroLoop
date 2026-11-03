//! Where a bearer token comes from (spec §5). One named source per binding,
//! and no fallback from one to the other.

use crate::client::{Method, agent, send};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use fl_core::StoreError;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub trait Credentials {
    /// A bearer token for the next request.
    fn token(&self) -> Result<String, StoreError>;
    /// Where the credential comes from, for `fl github whoami`. Never the
    /// secret itself.
    fn describe(&self) -> String;
    /// Who GitHub says fl writes as: a user's login, or an App's
    /// `<slug>[bot]` (spec §5.4).
    fn identity(&self, api: &str) -> Result<String, StoreError>;
}

fn text_field(v: &Value, k: &str, what: &str) -> Result<String, StoreError> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| StoreError::Credential(format!("GitHub's answer about {what} has no `{k}`")))
}

/// The environment variables `credential = "env"` reads, in order.
pub const ENV_VARS: [&str; 2] = ["FL_GITHUB_TOKEN", "GITHUB_TOKEN"];

/// A token from the environment (spec §5.3), or one already resolved by
/// the caller (for example `gh auth token` output).
pub struct EnvToken {
    source: String,
    token: String,
}

impl EnvToken {
    pub fn from_env() -> Result<Self, StoreError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, StoreError> {
        for var in ENV_VARS {
            if let Some(t) = get(var).filter(|t| !t.trim().is_empty()) {
                return Ok(Self::from_token(format!("${var}"), t));
            }
        }
        Err(StoreError::Credential(format!(
            "`credential = \"env\"` reads {} from the environment, and neither is set",
            ENV_VARS.join(" or ")
        )))
    }

    /// Wrap a token the caller already resolved. `source` names where it
    /// came from for `describe` (never the secret itself).
    pub fn from_token(source: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            token: token.into().trim().to_string(),
        }
    }
}

impl Credentials for EnvToken {
    fn token(&self) -> Result<String, StoreError> {
        Ok(self.token.clone())
    }
    fn describe(&self) -> String {
        format!("the token in {}", self.source)
    }
    fn identity(&self, api: &str) -> Result<String, StoreError> {
        let r = send(
            &agent(),
            Method::Get,
            &format!("{api}/user"),
            &self.token,
            None,
            api,
            true,
        )?;
        if r.status != 200 {
            return Err(StoreError::Credential(format!(
                "GitHub answered {} when fl asked whose token ({}) is",
                r.status, self.source
            )));
        }
        text_field(&r.body, "login", "the token's user")
    }
}

/// An installation token lives an hour; renew with ten minutes to spare.
const TOKEN_LIFE: Duration = Duration::from_secs(50 * 60);

/// Signs the GitHub App's JWT. The App's private key never leaves the signer: a caller gets the
/// JWT, or an error that holds no key bytes. [`AppCredentials`] asks its signer for a fresh JWT
/// every time it needs one and keeps neither a JWT nor a PEM.
///
/// `FileKeySigner` reads a PEM file; the CLI supplies a signer that asks the FerroGate Vault
/// (`fl_gate` is not a dependency of this crate).
pub trait JwtSigner: Send + Sync {
    /// A compact RS256 JWT for the App `app_id`: `iss` = `app_id`, `iat` = `now` minus 60 s,
    /// `exp` = `now` plus 540 s (nine minutes; GitHub allows ten).
    fn sign_app_jwt(&self, app_id: &str, now: SystemTime) -> Result<String, StoreError>;
}

/// Back-date `iat` a minute for clock drift.
const IAT_BACKDATE_SECS: u64 = 60;
/// `exp` is nine minutes after `now`.
const EXP_AFTER_SECS: u64 = 540;

fn unix_secs(now: SystemTime) -> Result<u64, StoreError> {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| StoreError::Credential("the system clock is before 1970".into()))
}

/// The JWT for `app_id`, signed with `key`.
fn sign_jwt(key: &RsaKeyPair, app_id: &str, now_unix: u64) -> Result<String, StoreError> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iat": now_unix.saturating_sub(IAT_BACKDATE_SECS),
        "exp": now_unix + EXP_AFTER_SECS,
        "iss": app_id,
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
    .map_err(|_| StoreError::Credential("signing the App's JWT failed".into()))?;
    Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

/// Signs with a PEM key the caller already holds in memory.
struct PemSigner {
    key: RsaKeyPair,
}

impl JwtSigner for PemSigner {
    fn sign_app_jwt(&self, app_id: &str, now: SystemTime) -> Result<String, StoreError> {
        sign_jwt(&self.key, app_id, unix_secs(now)?)
    }
}

/// Signs with the PEM file the config names (`[github] private_key`). The file is read and parsed
/// at each signing, so a rotated key is picked up and no parsed key outlives the call.
/// ⚠ The path, never the key, may appear in a message.
#[derive(Debug, Clone)]
pub struct FileKeySigner {
    path: PathBuf,
}

impl FileKeySigner {
    /// A signer for the key at `path`. The file is read and parsed once now, so an unreadable or
    /// malformed key is refused where the credential is made (as it always was); that parse is
    /// dropped, and every signing reads the file again.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let signer = Self {
            path: path.to_path_buf(),
        };
        signer.load()?;
        Ok(signer)
    }

    fn load(&self) -> Result<RsaKeyPair, StoreError> {
        let pem = std::fs::read_to_string(&self.path).map_err(|e| {
            StoreError::Credential(format!(
                "could not read the App private key at {}: {e}",
                self.path.display()
            ))
        })?;
        parse_pem(&pem)
    }
}

impl JwtSigner for FileKeySigner {
    fn sign_app_jwt(&self, app_id: &str, now: SystemTime) -> Result<String, StoreError> {
        sign_jwt(&self.load()?, app_id, unix_secs(now)?)
    }
}

/// The GitHub App's installation token for one repository (spec §5.2).
pub struct AppCredentials {
    app_id: u64,
    signer: Arc<dyn JwtSigner>,
    repo: String,
    api: String,
    agent: ureq::Agent,
    cached: RefCell<Option<(String, Instant)>>,
}

impl AppCredentials {
    /// An App whose JWTs `signer` makes.
    pub fn with_signer(api: &str, app_id: u64, signer: Arc<dyn JwtSigner>, repo: &str) -> Self {
        Self {
            app_id,
            signer,
            repo: repo.to_string(),
            api: api.trim_end_matches('/').to_string(),
            agent: agent(),
            cached: RefCell::new(None),
        }
    }

    pub fn new(api: &str, app_id: u64, pem: &str, repo: &str) -> Result<Self, StoreError> {
        let signer = PemSigner {
            key: parse_pem(pem)?,
        };
        Ok(Self::with_signer(api, app_id, Arc::new(signer), repo))
    }

    /// The key is read from a file the config names. ⚠ The path, never the
    /// key, may appear in a message.
    pub fn from_file(api: &str, app_id: u64, path: &Path, repo: &str) -> Result<Self, StoreError> {
        Ok(Self::with_signer(
            api,
            app_id,
            Arc::new(FileKeySigner::open(path)?),
            repo,
        ))
    }

    /// A JWT from the signer, valid for nine minutes (GitHub allows ten) and
    /// back-dated a minute for clock drift. A new one on every call.
    pub fn jwt(&self, now_unix: u64) -> Result<String, StoreError> {
        self.signer.sign_app_jwt(
            &self.app_id.to_string(),
            UNIX_EPOCH + Duration::from_secs(now_unix),
        )
    }

    fn exchange(&self) -> Result<String, StoreError> {
        let jwt = self.jwt(unix_secs(SystemTime::now())?)?;
        let url = format!("{}/repos/{}/installation", self.api, self.repo);
        let mut found = send(&self.agent, Method::Get, &url, &jwt, None, &self.api, true)?;
        // After a rename GitHub is taken to redirect the installation lookup
        // of the old name — unmeasured; no live test checks it yet. Follow
        // ONE redirect, and only on this API's own origin: the JWT goes
        // where it points.
        if matches!(found.status, 301 | 302 | 307 | 308) {
            let to = found
                .location
                .clone()
                .filter(|l| l.starts_with(&format!("{}/", self.api)))
                .ok_or_else(|| {
                    StoreError::Credential(
                        "GitHub redirected the App's installation lookup off its own origin".into(),
                    )
                })?;
            found = send(&self.agent, Method::Get, &to, &jwt, None, &self.api, true)?;
        }
        if found.status == 404 {
            return Err(StoreError::Credential(format!(
                "the App {} is not installed on {}. Install it on that repository",
                self.app_id, self.repo
            )));
        }
        let id = (found.status == 200)
            .then(|| found.body.get("id").and_then(Value::as_u64))
            .flatten()
            .ok_or_else(|| {
                StoreError::Credential(format!(
                    "GitHub answered {} when fl looked for the App's installation on {}",
                    found.status, self.repo
                ))
            })?;
        let url = format!("{}/app/installations/{id}/access_tokens", self.api);
        // Ask for a token for this one repository: without a body GitHub
        // grants every repository the installation can reach.
        let only_this =
            json!({"repositories": [self.repo.rsplit('/').next().unwrap_or(&self.repo)]});
        let made = send(
            &self.agent,
            Method::Post,
            &url,
            &jwt,
            Some(&only_this),
            &self.api,
            true,
        )?;
        let token = (made.status == 201)
            .then(|| made.body.get("token").and_then(Value::as_str))
            .flatten()
            .ok_or_else(|| {
                StoreError::Credential(format!(
                    "GitHub answered {} when fl asked for the App's installation token",
                    made.status
                ))
            })?;
        Ok(token.to_string())
    }
}

impl Credentials for AppCredentials {
    fn token(&self) -> Result<String, StoreError> {
        if let Some((t, at)) = self.cached.borrow().as_ref()
            && at.elapsed() < TOKEN_LIFE
        {
            return Ok(t.clone());
        }
        let t = self.exchange()?;
        *self.cached.borrow_mut() = Some((t.clone(), Instant::now()));
        Ok(t)
    }
    fn describe(&self) -> String {
        format!("GitHub App {} (installation token)", self.app_id)
    }
    fn identity(&self, api: &str) -> Result<String, StoreError> {
        let r = send(
            &self.agent,
            Method::Get,
            &format!("{api}/app"),
            &self.jwt(unix_secs(SystemTime::now())?)?,
            None,
            api,
            true,
        )?;
        if r.status != 200 {
            return Err(StoreError::Credential(format!(
                "GitHub answered {} when fl asked which App {} is",
                r.status, self.app_id
            )));
        }
        Ok(format!("{}[bot]", text_field(&r.body, "slug", "the App")?))
    }
}

/// An RSA private key in PEM form: PKCS#1 (`BEGIN RSA PRIVATE KEY`, what
/// GitHub hands out) or PKCS#8 (`BEGIN PRIVATE KEY`).
fn parse_pem(pem: &str) -> Result<RsaKeyPair, StoreError> {
    let refuse = |why: &str| {
        StoreError::Credential(format!(
            "the App private key is not an RSA key in PEM form ({why}). Download a new key \
             from the App's settings"
        ))
    };
    let pkcs1 = pem.contains("-----BEGIN RSA PRIVATE KEY-----");
    if !pkcs1 && !pem.contains("-----BEGIN PRIVATE KEY-----") {
        return Err(refuse("it has no BEGIN line"));
    }
    let b64: String = pem
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let der = STANDARD.decode(b64).map_err(|e| refuse(&e.to_string()))?;
    let key = if pkcs1 {
        RsaKeyPair::from_der(&der)
    } else {
        RsaKeyPair::from_pkcs8(&der)
    };
    key.map_err(|e| refuse(&e.to_string()))
}

/// A throwaway RSA key, made by the `openssl` CLI so no private key is
/// ever committed. `traditional` selects PKCS#1 over PKCS#8. Tests only:
/// these tests and any other that opens a client as the App.
#[cfg(test)]
pub(crate) fn throwaway_key(traditional: bool) -> String {
    let mut args = vec!["genrsa"];
    if traditional {
        args.push("-traditional");
    }
    args.push("2048");
    let out = std::process::Command::new("openssl")
        .args(&args)
        .output()
        .expect("these tests need the `openssl` CLI to make a throwaway key");
    assert!(out.status.success(), "openssl genrsa failed");
    String::from_utf8(out.stdout).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeGithub;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    #[test]
    fn the_environment_token_is_read_in_order_and_its_absence_is_refused() {
        let t = EnvToken::from_lookup(|k| match k {
            "FL_GITHUB_TOKEN" => Some("first".into()),
            "GITHUB_TOKEN" => Some("second".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(t.token().unwrap(), "first");
        assert!(t.describe().contains("FL_GITHUB_TOKEN"));
        let t = EnvToken::from_lookup(|k| (k == "GITHUB_TOKEN").then(|| "second".into())).unwrap();
        assert_eq!(t.token().unwrap(), "second");
        let err = EnvToken::from_lookup(|_| Some("  ".into())).err().unwrap();
        assert!(
            matches!(err, StoreError::Credential(ref m) if m.contains("GITHUB_TOKEN")),
            "{err:?}"
        );
    }

    #[test]
    fn the_jwt_is_signed_with_the_apps_key_and_names_the_app() {
        for traditional in [true, false] {
            let pem = throwaway_key(traditional);
            let app = AppCredentials::new("http://127.0.0.1:9", 42, &pem, "acme/widgets").unwrap();
            let jwt = app.jwt(1_000_000).unwrap();
            let parts: Vec<&str> = jwt.split('.').collect();
            assert_eq!(parts.len(), 3);
            let claims: Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
            assert_eq!(claims["iss"], "42");
            assert_eq!(claims["iat"], 1_000_000 - 60);
            assert_eq!(claims["exp"], 1_000_000 + 540);
            let key = parse_pem(&pem).unwrap();
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref())
                .verify(
                    format!("{}.{}", parts[0], parts[1]).as_bytes(),
                    &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
                )
                .expect("the signature verifies against the App's public key");
        }
    }

    #[test]
    fn a_key_that_is_not_pem_is_refused_without_echoing_it() {
        let err = AppCredentials::new("http://x", 1, "not a key at all", "a/b")
            .err()
            .unwrap();
        let msg = err.to_string();
        assert!(msg.contains("not an RSA key"), "{msg}");
        assert!(
            !msg.contains("not a key at all"),
            "the input must not be echoed: {msg}"
        );
    }

    #[test]
    fn the_installation_token_is_fetched_once_and_reused() {
        let fake = FakeGithub::start("acme/widgets");
        let app =
            AppCredentials::new(&fake.url(), 42, &throwaway_key(true), "acme/widgets").unwrap();
        assert_eq!(app.token().unwrap(), crate::fake::INSTALLATION_TOKEN);
        assert_eq!(app.token().unwrap(), crate::fake::INSTALLATION_TOKEN);
        assert_eq!(fake.state().token_requests, 1);
    }

    #[test]
    fn the_installation_token_is_requested_for_the_one_repository() {
        let fake = FakeGithub::start("acme/widgets");
        let app =
            AppCredentials::new(&fake.url(), 42, &throwaway_key(true), "acme/widgets").unwrap();
        app.token().unwrap();
        let bodies = fake.state().token_bodies.clone();
        assert_eq!(bodies.len(), 1);
        let sent: Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(sent, json!({"repositories": ["widgets"]}));
    }

    #[test]
    fn identity_names_the_user_or_the_apps_bot() {
        let fake = FakeGithub::start("acme/widgets");
        let env = EnvToken::from_lookup(|_| Some("t".into())).unwrap();
        assert_eq!(env.identity(&fake.url()).unwrap(), crate::fake::USER_LOGIN);
        let app =
            AppCredentials::new(&fake.url(), 42, &throwaway_key(true), "acme/widgets").unwrap();
        assert_eq!(
            app.identity(&fake.url()).unwrap(),
            format!("{}[bot]", crate::fake::APP_SLUG)
        );
    }

    #[test]
    fn the_app_still_finds_its_installation_after_a_rename() {
        let fake = FakeGithub::start("acme/widgets");
        fake.rename("acme/gadgets");
        let app =
            AppCredentials::new(&fake.url(), 42, &throwaway_key(true), "acme/widgets").unwrap();
        assert_eq!(app.token().unwrap(), crate::fake::INSTALLATION_TOKEN);
    }

    #[test]
    fn the_app_refuses_to_follow_a_redirect_off_the_apis_own_origin() {
        // A second fake stands in for wherever an off-origin redirect could
        // point: the JWT must never reach it.
        let elsewhere = FakeGithub::start("acme/widgets");
        let fake = FakeGithub::start("acme/widgets");
        fake.state().off_origin_redirect_next = Some(format!("{}/steal", elsewhere.url()));
        let app =
            AppCredentials::new(&fake.url(), 42, &throwaway_key(true), "acme/widgets").unwrap();
        let err = app.token().unwrap_err();
        assert!(err.to_string().contains("off its own origin"), "{err}");
        assert!(
            elsewhere.state().requests.is_empty(),
            "the off-origin target must never be contacted: {:?}",
            elsewhere.state().requests
        );
    }

    #[test]
    fn an_app_not_installed_on_the_repository_is_named_as_such() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().installations.clear();
        let app =
            AppCredentials::new(&fake.url(), 42, &throwaway_key(true), "acme/widgets").unwrap();
        let err = app.token().unwrap_err();
        assert!(
            err.to_string().contains("not installed on acme/widgets"),
            "{err}"
        );
    }
    /// A signer that counts its calls and can be told to refuse. It signs with a real throwaway
    /// key so the fake GitHub accepts the JWT and a test can verify it.
    struct CountingSigner {
        inner: PemSigner,
        calls: std::sync::Mutex<Vec<(String, SystemTime)>>,
        refuse: std::sync::atomic::AtomicBool,
    }

    impl CountingSigner {
        fn new(pem: &str) -> Arc<Self> {
            Arc::new(Self {
                inner: PemSigner {
                    key: parse_pem(pem).unwrap(),
                },
                calls: Default::default(),
                refuse: Default::default(),
            })
        }
        fn count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl JwtSigner for CountingSigner {
        fn sign_app_jwt(&self, app_id: &str, now: SystemTime) -> Result<String, StoreError> {
            self.calls.lock().unwrap().push((app_id.to_string(), now));
            if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(StoreError::Credential(
                    "the signer refused `vault://SOME_NAME`: denied".into(),
                ));
            }
            self.inner.sign_app_jwt(app_id, now)
        }
    }

    #[test]
    fn the_signer_is_asked_for_every_jwt_and_none_is_kept() {
        let fake = FakeGithub::start("acme/widgets");
        let pem = throwaway_key(true);
        let signer = CountingSigner::new(&pem);
        let app = AppCredentials::with_signer(&fake.url(), 42, signer.clone(), "acme/widgets");
        // Each identity lookup needs its own JWT.
        app.identity(&fake.url()).unwrap();
        app.identity(&fake.url()).unwrap();
        assert_eq!(signer.count(), 2);
        // The first token exchange asks once; the cached token needs no JWT.
        app.token().unwrap();
        app.token().unwrap();
        assert_eq!(signer.count(), 3);
        assert_eq!(fake.state().token_requests, 1);
        // Once the installation token is stale the signer is asked again, not a stored JWT.
        let stale = Instant::now() - TOKEN_LIFE - Duration::from_secs(1);
        *app.cached.borrow_mut() = Some(("old".into(), stale));
        app.token().unwrap();
        assert_eq!(signer.count(), 4);
        assert_eq!(fake.state().token_requests, 2);
        // Every call named the App, and the fake saw four distinct JWT requests.
        assert!(
            signer
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(id, _)| id == "42")
        );
        let jwts = fake.state().jwts.clone();
        assert_eq!(jwts.len(), 4, "{jwts:?}");
        // The JWTs the fake received verify against the key's public half.
        let key = parse_pem(&pem).unwrap();
        for jwt in &jwts {
            let parts: Vec<&str> = jwt.split('.').collect();
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref())
                .verify(
                    format!("{}.{}", parts[0], parts[1]).as_bytes(),
                    &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
                )
                .expect("a JWT the fake received verifies");
        }
    }

    #[test]
    fn a_jwt_is_made_for_the_time_asked_and_never_reused() {
        let signer = CountingSigner::new(&throwaway_key(false));
        let app = AppCredentials::with_signer("http://127.0.0.1:9", 7, signer.clone(), "a/b");
        let claims = |jwt: &str| -> Value {
            serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(jwt.split('.').nth(1).unwrap())
                    .unwrap(),
            )
            .unwrap()
        };
        let first = app.jwt(1_000_000).unwrap();
        let later = app.jwt(1_000_000 + 3_600).unwrap();
        assert_eq!(signer.count(), 2, "a JWT is never served from a cache");
        assert_eq!(claims(&first)["exp"], 1_000_540);
        assert_eq!(claims(&later)["iat"], 1_000_000 + 3_600 - 60);
        assert_eq!(claims(&later)["exp"], 1_000_000 + 3_600 + 540);
    }

    #[test]
    fn a_signer_refusal_stops_the_exchange_and_carries_no_key_bytes() {
        let fake = FakeGithub::start("acme/widgets");
        let pem = throwaway_key(true);
        let signer = CountingSigner::new(&pem);
        signer
            .refuse
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let app = AppCredentials::with_signer(&fake.url(), 42, signer.clone(), "acme/widgets");
        let err = app.token().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("vault://SOME_NAME"), "{msg}");
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        assert!(!msg.contains(&body[..40]), "no key bytes in the error");
        assert!(
            fake.state().requests.is_empty(),
            "nothing is sent when there is no JWT: {:?}",
            fake.state().requests
        );
        assert!(app.identity(&fake.url()).is_err());
        assert!(fake.state().requests.is_empty());
    }

    #[test]
    fn the_file_signer_reads_the_key_at_every_use_and_names_only_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.pem");
        // A file that is not a key is refused where the credential is made.
        std::fs::write(&path, "not a key at all").unwrap();
        let err = FileKeySigner::open(&path).unwrap_err().to_string();
        assert!(err.contains("not an RSA key"), "{err}");
        assert!(!err.contains("not a key at all"), "{err}");
        let missing = dir.path().join("nope.pem");
        let err = AppCredentials::from_file("http://x", 1, &missing, "a/b")
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("could not read the App private key at"),
            "{err}"
        );
        assert!(err.contains("nope.pem"), "{err}");
        // A key is read when it signs: replacing the file changes the key the next JWT uses.
        let first = throwaway_key(true);
        std::fs::write(&path, &first).unwrap();
        let signer = FileKeySigner::open(&path).unwrap();
        let verifies = |pem: &str, jwt: &str| {
            let key = parse_pem(pem).unwrap();
            let parts: Vec<&str> = jwt.split('.').collect();
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref())
                .verify(
                    format!("{}.{}", parts[0], parts[1]).as_bytes(),
                    &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
                )
                .is_ok()
        };
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let a = signer.sign_app_jwt("42", now).unwrap();
        assert!(verifies(&first, &a));
        let second = throwaway_key(false);
        std::fs::write(&path, &second).unwrap();
        let b = signer.sign_app_jwt("42", now).unwrap();
        assert!(verifies(&second, &b) && !verifies(&first, &b));
        // The file is gone: the next signing says so, and names the path.
        std::fs::remove_file(&path).unwrap();
        let err = signer.sign_app_jwt("42", now).unwrap_err().to_string();
        assert!(err.contains("app.pem"), "{err}");
    }

    #[test]
    fn app_credentials_from_a_file_work_against_the_fake() {
        let fake = FakeGithub::start("acme/widgets");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.pem");
        std::fs::write(&path, throwaway_key(true)).unwrap();
        let app = AppCredentials::from_file(&fake.url(), 42, &path, "acme/widgets").unwrap();
        assert_eq!(app.token().unwrap(), crate::fake::INSTALLATION_TOKEN);
    }
}
