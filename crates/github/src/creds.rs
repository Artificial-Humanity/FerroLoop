//! Where a bearer token comes from (spec §5). One named source per binding,
//! and no fallback from one to the other.

use crate::client::{Method, agent, send};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use fl_core::StoreError;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::Value;
use std::cell::RefCell;
use std::path::Path;
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

/// A token from the environment (spec §5.3).
pub struct EnvToken {
    var: &'static str,
    token: String,
}

impl EnvToken {
    pub fn from_env() -> Result<Self, StoreError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, StoreError> {
        for var in ENV_VARS {
            if let Some(t) = get(var).filter(|t| !t.trim().is_empty()) {
                return Ok(Self {
                    var,
                    token: t.trim().to_string(),
                });
            }
        }
        Err(StoreError::Credential(format!(
            "`credential = \"env\"` reads {} from the environment, and neither is set",
            ENV_VARS.join(" or ")
        )))
    }
}

impl Credentials for EnvToken {
    fn token(&self) -> Result<String, StoreError> {
        Ok(self.token.clone())
    }
    fn describe(&self) -> String {
        format!("the token in ${}", self.var)
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
                "GitHub answered {} when fl asked whose token ${} is",
                r.status, self.var
            )));
        }
        text_field(&r.body, "login", "the token's user")
    }
}

/// An installation token lives an hour; renew with ten minutes to spare.
const TOKEN_LIFE: Duration = Duration::from_secs(50 * 60);

/// The GitHub App's installation token for one repository (spec §5.2).
pub struct AppCredentials {
    app_id: u64,
    key: RsaKeyPair,
    repo: String,
    api: String,
    agent: ureq::Agent,
    cached: RefCell<Option<(String, Instant)>>,
}

impl AppCredentials {
    pub fn new(api: &str, app_id: u64, pem: &str, repo: &str) -> Result<Self, StoreError> {
        Ok(Self {
            app_id,
            key: parse_pem(pem)?,
            repo: repo.to_string(),
            api: api.trim_end_matches('/').to_string(),
            agent: agent(),
            cached: RefCell::new(None),
        })
    }

    /// The key is read from a file the config names. ⚠ The path, never the
    /// key, may appear in a message.
    pub fn from_file(api: &str, app_id: u64, path: &Path, repo: &str) -> Result<Self, StoreError> {
        let pem = std::fs::read_to_string(path).map_err(|e| {
            StoreError::Credential(format!(
                "could not read the App private key at {}: {e}",
                path.display()
            ))
        })?;
        Self::new(api, app_id, &pem, repo)
    }

    /// A JWT signed with the App's key, valid for nine minutes (GitHub
    /// allows ten) and back-dated a minute for clock drift.
    pub fn jwt(&self, now_unix: u64) -> Result<String, StoreError> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = serde_json::json!({
            "iat": now_unix.saturating_sub(60),
            "exp": now_unix + 540,
            "iss": self.app_id.to_string(),
        });
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let input = format!("{header}.{payload}");
        let mut sig = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut sig,
            )
            .map_err(|_| StoreError::Credential("signing the App's JWT failed".into()))?;
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
    }

    fn exchange(&self) -> Result<String, StoreError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StoreError::Credential("the system clock is before 1970".into()))?
            .as_secs();
        let jwt = self.jwt(now)?;
        let url = format!("{}/repos/{}/installation", self.api, self.repo);
        let mut found = send(&self.agent, Method::Get, &url, &jwt, None, &self.api, true)?;
        // After a rename GitHub redirects the old name. Follow ONE redirect,
        // and only on this API's own origin: the JWT goes where it points.
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
        let made = send(&self.agent, Method::Post, &url, &jwt, None, &self.api, true)?;
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StoreError::Credential("the system clock is before 1970".into()))?
            .as_secs();
        let r = send(
            &self.agent,
            Method::Get,
            &format!("{api}/app"),
            &self.jwt(now)?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeGithub;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    /// A throwaway RSA key, made by the `openssl` CLI so no private key is
    /// ever committed. `traditional` selects PKCS#1 over PKCS#8.
    fn throwaway_key(traditional: bool) -> String {
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
}
