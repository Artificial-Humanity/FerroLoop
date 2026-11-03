//! Record #86 / Gate G6 evidence at the connection boundary: a forbidden request is refused
//! BEFORE any connection is made, and the refusal is on record in `gate-audit.jsonl`.
//!
//! Everything is hermetic: a throwaway vault in a temp directory (a fixed throwaway master key),
//! throwaway RSA keys made at test time, and listeners on 127.0.0.1 that count accepted
//! connections. No real credential, no real network.
//!
//! NOT covered: DNS rebinding (audit O-1). The SSRF guard resolves a host name once to judge it
//! and the HTTP client resolves it again to connect; a name that answers differently the second
//! time is not caught by anything tested here (the SSRF cases below use IP literals).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use fl_gate::{
    GateError, ProxyRequest, SIGN_AUDIT_METHOD, SetSecretArgs, Vault, execute_proxied_request,
    parse_vault_ref, prepare_proxied_request,
};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaKeyPair, UnparsedPublicKey};
use serde_json::Value;

const SECRET_VALUE: &str = "throwaway-boundary-secret-ABCDEF123456";
const QUERY_SENTINEL: &str = "query-sentinel-7f3a";
const PATH_SENTINEL: &str = "/path-sentinel-9c1d";

/// A 127.0.0.1 listener that counts accepted connections and answers each with `200 ok`.
struct Counter {
    port: u16,
    accepts: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Counter {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (accepts.clone(), stop.clone());
        let handle = thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        a.fetch_add(1, Ordering::SeqCst);
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self {
            port,
            accepts,
            stop,
            handle: Some(handle),
        }
    }

    /// Accepts so far, after a moment for a connection that was made to be accepted.
    fn settled_accepts(&self) -> usize {
        thread::sleep(Duration::from_millis(150));
        self.accepts.load(Ordering::SeqCst)
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    vault: Vault,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path().join("vault.enc"), [3u8; 32]).unwrap();
        Self { dir, vault }
    }

    fn set(&mut self, args: SetSecretArgs) {
        self.vault.set(args).unwrap();
    }

    fn audit_path(&self) -> std::path::PathBuf {
        self.dir.path().join("gate-audit.jsonl")
    }

    fn audit_text(&self) -> String {
        std::fs::read_to_string(self.audit_path()).unwrap_or_default()
    }

    fn audit(&self) -> Vec<Value> {
        self.audit_text()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn last_audit(&self) -> Value {
        self.audit().pop().expect("an audit entry was written")
    }
}

fn proxy_secret(name: &str) -> SetSecretArgs {
    SetSecretArgs {
        name: name.into(),
        value: SECRET_VALUE.into(),
        allowed_hosts: vec!["127.0.0.1".into()],
        allowed_methods: vec!["GET".into()],
        allowed_agents: vec!["ferris".into()],
        allow_private_network: true,
        ..Default::default()
    }
}

fn request(method: &str, url: String, caller: Option<&str>) -> ProxyRequest {
    ProxyRequest {
        method: method.into(),
        url,
        headers: BTreeMap::from([("X-Key".into(), "{{FERROGATE:UPSTREAM_KEY}}".into())]),
        body: None,
        caller_agent: caller.map(str::to_string),
        timeout_secs: Some(5),
    }
}

fn target_url(c: &Counter) -> String {
    format!(
        "http://127.0.0.1:{}{PATH_SENTINEL}?token={QUERY_SENTINEL}",
        c.port
    )
}

/// Assert the last entry is a denial with this reason, names the secret and carries no value,
/// path or query anywhere in the file.
fn assert_denied(f: &Fixture, reason: &str, caller: &str, method: &str, host: &str) {
    let entry = f.last_audit();
    assert_eq!(entry["outcome"], format!("denied:{reason}"), "{entry}");
    assert_eq!(entry["caller"], caller, "{entry}");
    assert_eq!(entry["method"], method, "{entry}");
    assert_eq!(entry["host"], host, "{entry}");
    assert_eq!(
        entry["secrets"],
        serde_json::json!(["UPSTREAM_KEY"]),
        "{entry}"
    );
    let text = f.audit_text();
    for leak in [SECRET_VALUE, QUERY_SENTINEL, PATH_SENTINEL, "X-Key"] {
        assert!(
            !text.contains(leak),
            "the audit log leaked `{leak}`: {text}"
        );
    }
}

// ---------------------------------------------------------------- proxy: refused and audited

#[test]
fn proxy_host_not_in_allowed_hosts_is_refused_before_any_connection_and_audited() {
    let target = Counter::start();
    let mut f = Fixture::new();
    f.set(proxy_secret("UPSTREAM_KEY"));
    // `localhost` is a different name than the allowed `127.0.0.1`, though it is the same machine.
    let url = format!(
        "http://localhost:{}{PATH_SENTINEL}?token={QUERY_SENTINEL}",
        target.port
    );
    let err = execute_proxied_request(&f.vault, &request("GET", url, Some("ferris"))).unwrap_err();
    assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
    assert_eq!(target.settled_accepts(), 0, "no connection may be made");
    assert_denied(&f, "host", "ferris", "GET", "localhost");
}

#[test]
fn proxy_wrong_method_is_refused_before_any_connection_and_audited() {
    let target = Counter::start();
    let mut f = Fixture::new();
    f.set(proxy_secret("UPSTREAM_KEY"));
    let err = execute_proxied_request(
        &f.vault,
        &request("POST", target_url(&target), Some("ferris")),
    )
    .unwrap_err();
    assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
    assert_eq!(target.settled_accepts(), 0);
    assert_denied(&f, "method", "ferris", "POST", "127.0.0.1");
}

#[test]
fn proxy_wrong_caller_is_refused_before_any_connection_and_audited() {
    let target = Counter::start();
    let mut f = Fixture::new();
    f.set(proxy_secret("UPSTREAM_KEY"));
    for caller in [Some("mallory"), None] {
        let err = execute_proxied_request(&f.vault, &request("GET", target_url(&target), caller))
            .unwrap_err();
        assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
        assert_eq!(target.settled_accepts(), 0);
        assert_denied(
            &f,
            "agent",
            caller.unwrap_or("anonymous"),
            "GET",
            "127.0.0.1",
        );
    }
}

#[test]
fn proxy_plain_http_to_a_secret_without_private_network_is_refused_and_audited() {
    let target = Counter::start();
    let mut f = Fixture::new();
    f.set(SetSecretArgs {
        allow_private_network: false,
        ..proxy_secret("UPSTREAM_KEY")
    });
    let err = execute_proxied_request(
        &f.vault,
        &request("GET", target_url(&target), Some("ferris")),
    )
    .unwrap_err();
    assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
    assert_eq!(target.settled_accepts(), 0);
    assert_denied(&f, "https_required", "ferris", "GET", "127.0.0.1");
}

#[test]
fn proxy_ssrf_targets_are_refused_before_any_connection_and_audited() {
    let target = Counter::start();
    let mut f = Fixture::new();
    f.set(SetSecretArgs {
        allowed_hosts: vec!["127.0.0.1".into(), "169.254.169.254".into()],
        allow_private_network: false,
        ..proxy_secret("UPSTREAM_KEY")
    });
    for (url, host) in [
        (
            format!(
                "https://127.0.0.1:{}{PATH_SENTINEL}?token={QUERY_SENTINEL}",
                target.port
            ),
            "127.0.0.1",
        ),
        (
            format!("https://169.254.169.254{PATH_SENTINEL}?token={QUERY_SENTINEL}"),
            "169.254.169.254",
        ),
    ] {
        let err =
            execute_proxied_request(&f.vault, &request("GET", url, Some("ferris"))).unwrap_err();
        assert!(matches!(err, GateError::SsrfBlocked { .. }), "{err}");
        assert_eq!(target.settled_accepts(), 0);
        assert_denied(&f, "ssrf", "ferris", "GET", host);
    }
}

#[test]
fn proxy_expired_secret_is_refused_before_any_connection_and_audited() {
    let target = Counter::start();
    let mut f = Fixture::new();
    f.set(SetSecretArgs {
        lease_ttl_secs: Some(0),
        ..proxy_secret("UPSTREAM_KEY")
    });
    thread::sleep(Duration::from_millis(50));
    let err = execute_proxied_request(
        &f.vault,
        &request("GET", target_url(&target), Some("ferris")),
    )
    .unwrap_err();
    assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
    assert_eq!(target.settled_accepts(), 0);
    assert_denied(&f, "expired", "ferris", "GET", "127.0.0.1");
}

#[test]
fn proxy_unknown_secret_bad_placeholders_and_bad_urls_are_audited_without_their_text() {
    let target = Counter::start();
    let mut f = Fixture::new();
    f.set(proxy_secret("UPSTREAM_KEY"));

    // NoSuchSecret: the (valid) name is on record.
    let mut req = request("GET", target_url(&target), Some("ferris"));
    req.headers = BTreeMap::from([("X-Key".into(), "{{FERROGATE:MISSING_ONE}}".into())]);
    let err = execute_proxied_request(&f.vault, &req).unwrap_err();
    assert!(matches!(err, GateError::NoSuchSecret { .. }), "{err}");
    let e = f.last_audit();
    assert_eq!(e["outcome"], "denied:no_such_secret");
    assert_eq!(e["secrets"], serde_json::json!(["MISSING_ONE"]));

    // A placeholder in the URL path, an unclosed placeholder, a malformed name: each refused.
    // A malformed name may be a pasted secret, so it is never written down.
    for (url, header, reason) in [
        (
            format!(
                "http://127.0.0.1:{}/{{{{FERROGATE:UPSTREAM_KEY}}}}",
                target.port
            ),
            "x".to_string(),
            "placeholder",
        ),
        (
            target_url(&target),
            "{{FERROGATE:UPSTREAM_KEY".to_string(),
            "placeholder",
        ),
        (
            target_url(&target),
            format!("{{{{FERROGATE:{SECRET_VALUE}}}}}"),
            "invalid_secret_name",
        ),
        (
            "ftp://127.0.0.1/x".to_string(),
            "x".to_string(),
            "invalid_url",
        ),
        (
            format!("http://user:{SECRET_VALUE}@127.0.0.1/x"),
            "x".to_string(),
            "invalid_url",
        ),
    ] {
        let mut req = request("GET", url, Some("ferris"));
        req.headers = BTreeMap::from([("X-Key".into(), header)]);
        assert!(execute_proxied_request(&f.vault, &req).is_err());
        let e = f.last_audit();
        assert_eq!(e["outcome"], format!("denied:{reason}"), "{e}");
        assert!(
            !f.audit_text().contains(SECRET_VALUE),
            "value leaked into the audit"
        );
    }

    // An empty method.
    let err = execute_proxied_request(
        &f.vault,
        &request("  ", target_url(&target), Some("ferris")),
    )
    .unwrap_err();
    assert!(matches!(err, GateError::InvalidUrl { .. }), "{err}");
    assert_eq!(f.last_audit()["outcome"], "denied:method");
    assert_eq!(target.settled_accepts(), 0);
}

#[test]
fn proxy_allowed_request_works_and_is_audited_allowed() {
    let target = Counter::start();
    let mut f = Fixture::new();
    f.set(proxy_secret("UPSTREAM_KEY"));
    let resp = execute_proxied_request(
        &f.vault,
        &request("GET", target_url(&target), Some("ferris")),
    )
    .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "ok");
    assert_eq!(target.settled_accepts(), 1);
    let entries = f.audit();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["outcome"], "allowed");
    assert_eq!(entries[0]["caller"], "ferris");
    assert_eq!(entries[0]["method"], "GET");
    assert_eq!(entries[0]["host"], "127.0.0.1");
    assert_eq!(entries[0]["secrets"], serde_json::json!(["UPSTREAM_KEY"]));
    assert!(!f.audit_text().contains(SECRET_VALUE));
}

#[cfg(unix)]
#[test]
fn an_audit_log_that_cannot_be_written_never_turns_a_denial_into_an_allow() {
    let target = Counter::start();
    let mut f = Fixture::new();
    f.set(proxy_secret("UPSTREAM_KEY"));
    // The audit file is a symlink, which the log refuses to write through.
    std::os::unix::fs::symlink("/dev/null", f.audit_path()).unwrap();

    let err = prepare_proxied_request(
        &f.vault,
        &request("POST", target_url(&target), Some("ferris")),
    )
    .unwrap_err();
    assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
    let err = execute_proxied_request(&f.vault, &request("GET", target_url(&target), Some("eve")))
        .unwrap_err();
    assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
    assert_eq!(target.settled_accepts(), 0);

    // Documented current behaviour, not a promise to change: on the ALLOWED proxy path an audit
    // write that fails is ignored and the request proceeds. (`sign_app_jwt` is stricter.)
    let resp = execute_proxied_request(
        &f.vault,
        &request("GET", target_url(&target), Some("ferris")),
    )
    .unwrap();
    assert_eq!(resp.status, 200);
}

// ---------------------------------------------------------------- sign_app_jwt

const HOST: &str = "api.github.com";
const NOW_SECS: u64 = 1_000_000;

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW_SECS)
}

/// A throwaway RSA key as PEM. `traditional` selects PKCS#1 over PKCS#8. `None` (with a message)
/// when the `openssl` CLI is absent, as in `crates/github/src/creds.rs`'s tests (which require it).
fn throwaway_pem(traditional: bool) -> Option<String> {
    let mut args = vec!["genrsa"];
    if traditional {
        args.push("-traditional");
    }
    args.push("2048");
    match Command::new("openssl").args(&args).output() {
        Ok(out) if out.status.success() => Some(String::from_utf8(out.stdout).unwrap()),
        _ => {
            eprintln!("SKIPPED: the `openssl` CLI is needed to make a throwaway RSA key");
            None
        }
    }
}

fn key_pair(pem: &str) -> RsaKeyPair {
    let b64: String = pem
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let der = STANDARD.decode(b64).unwrap();
    if pem.contains("BEGIN RSA PRIVATE KEY") {
        RsaKeyPair::from_der(&der).unwrap()
    } else {
        RsaKeyPair::from_pkcs8(&der).unwrap()
    }
}

fn sign_secret(pem: &str) -> SetSecretArgs {
    SetSecretArgs {
        name: "GITHUB_APP_KEY".into(),
        value: pem.into(),
        allowed_hosts: vec![HOST.into()],
        allowed_agents: vec!["ferris".into()],
        ..Default::default()
    }
}

fn assert_sign_denied(f: &Fixture, reason: &str, caller: &str, host: &str, pem_marker: &str) {
    let e = f.last_audit();
    assert_eq!(e["outcome"], format!("denied:{reason}"), "{e}");
    assert_eq!(e["caller"], caller, "{e}");
    assert_eq!(e["method"], SIGN_AUDIT_METHOD, "{e}");
    assert_eq!(e["host"], host, "{e}");
    assert_eq!(e["secrets"], serde_json::json!(["GITHUB_APP_KEY"]), "{e}");
    let text = f.audit_text();
    assert!(
        !text.contains(pem_marker) && !text.contains("PRIVATE KEY"),
        "{text}"
    );
}

#[test]
fn sign_app_jwt_returns_a_jwt_that_verifies_and_matches_the_github_app_shape() {
    for traditional in [true, false] {
        let Some(pem) = throwaway_pem(traditional) else {
            return;
        };
        let mut f = Fixture::new();
        f.set(sign_secret(&pem));
        for r in ["vault://GITHUB_APP_KEY", "vault://GITHUB_APP_KEY#pem"] {
            let secret = parse_vault_ref(r).unwrap();
            let jwt = f
                .vault
                .sign_app_jwt(&secret, "424242", "ferris", HOST, now())
                .unwrap();
            let parts: Vec<&str> = jwt.split('.').collect();
            assert_eq!(parts.len(), 3, "{jwt}");

            // Same header bytes and claim shape as `crates/github/src/creds.rs` (`jwt`).
            assert_eq!(
                URL_SAFE_NO_PAD.decode(parts[0]).unwrap(),
                br#"{"alg":"RS256","typ":"JWT"}"#
            );
            let claims: Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
            assert_eq!(claims["iss"], "424242");
            assert_eq!(claims["iat"], NOW_SECS - 60);
            assert_eq!(claims["exp"], NOW_SECS + 540);
            assert_eq!(claims.as_object().unwrap().len(), 3, "{claims}");

            let key = key_pair(&pem);
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref())
                .verify(
                    format!("{}.{}", parts[0], parts[1]).as_bytes(),
                    &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
                )
                .expect("the signature verifies against the throwaway public key");

            let e = f.last_audit();
            assert_eq!(e["outcome"], "allowed");
            assert_eq!(e["method"], SIGN_AUDIT_METHOD);
            assert_eq!(e["host"], HOST);
            assert_eq!(e["caller"], "ferris");
            assert_eq!(e["secrets"], serde_json::json!(["GITHUB_APP_KEY"]));
            let text = f.audit_text();
            assert!(
                !text.contains(&jwt) && !text.contains("PRIVATE KEY"),
                "{text}"
            );
        }
    }
}

#[test]
fn sign_app_jwt_policy_runs_before_the_key_is_used_and_every_denial_is_audited() {
    let Some(pem) = throwaway_pem(false) else {
        return;
    };
    let mut f = Fixture::new();
    f.set(sign_secret(&pem));
    let secret = parse_vault_ref("vault://GITHUB_APP_KEY#pem").unwrap();
    let marker = "BEGIN";

    // Host not in allowed_hosts (and a host that is not a hostname at all).
    for host in [
        "evil.example.com",
        "api.github.com.evil.com",
        "api.github.com/x",
        "",
    ] {
        let err = f
            .vault
            .sign_app_jwt(&secret, "42", "ferris", host, now())
            .unwrap_err();
        assert!(
            matches!(err, GateError::PolicyDenied { .. }),
            "{host}: {err}"
        );
        let shown = if host.contains('/') || host.is_empty() {
            ""
        } else {
            host
        };
        assert_sign_denied(&f, "host", "ferris", shown, marker);
    }

    // Caller not in allowed_agents, and no caller at all.
    for caller in ["mallory", ""] {
        let err = f
            .vault
            .sign_app_jwt(&secret, "42", caller, HOST, now())
            .unwrap_err();
        assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
        assert_sign_denied(&f, "agent", caller, HOST, marker);
    }

    // An unknown secret.
    let ghost = parse_vault_ref("vault://NOT_THERE").unwrap();
    let err = f
        .vault
        .sign_app_jwt(&ghost, "42", "ferris", HOST, now())
        .unwrap_err();
    assert!(matches!(err, GateError::NoSuchSecret { .. }), "{err}");
    assert_eq!(f.last_audit()["outcome"], "denied:no_such_secret");
    assert_eq!(f.last_audit()["secrets"], serde_json::json!(["NOT_THERE"]));

    // A bad issuer.
    let err = f
        .vault
        .sign_app_jwt(&secret, "42\"}", "ferris", HOST, now())
        .unwrap_err();
    assert!(matches!(err, GateError::InvalidSecret { .. }), "{err}");
    assert_eq!(f.last_audit()["outcome"], "denied:issuer");

    // Expired.
    let mut g = Fixture::new();
    g.set(SetSecretArgs {
        lease_ttl_secs: Some(0),
        ..sign_secret(&pem)
    });
    thread::sleep(Duration::from_millis(50));
    let err = g
        .vault
        .sign_app_jwt(&secret, "42", "ferris", HOST, now())
        .unwrap_err();
    assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
    assert_sign_denied(&g, "expired", "ferris", HOST, marker);

    // An empty allowed_hosts list denies.
    let mut h = Fixture::new();
    h.set(SetSecretArgs {
        allowed_hosts: vec![],
        ..sign_secret(&pem)
    });
    let err = h
        .vault
        .sign_app_jwt(&secret, "42", "ferris", HOST, now())
        .unwrap_err();
    assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
    assert_sign_denied(&h, "host", "ferris", HOST, marker);

    // An empty allowed_agents list admits any caller (as for the proxy), and `*` too.
    for agents in [vec![], vec!["*".to_string()]] {
        let mut k = Fixture::new();
        k.set(SetSecretArgs {
            allowed_agents: agents,
            ..sign_secret(&pem)
        });
        assert!(
            k.vault
                .sign_app_jwt(&secret, "42", "anyone", HOST, now())
                .is_ok()
        );
    }
}

#[test]
fn sign_app_jwt_refuses_values_that_are_not_rsa_keys_without_echoing_them() {
    let mut f = Fixture::new();
    let secret = parse_vault_ref("vault://GITHUB_APP_KEY").unwrap();
    for (value, label) in [
        ("SENTINEL_not_a_key_value_123", "plain text"),
        (
            "-----BEGIN EC PRIVATE KEY-----\nSENTINEL_ec_body_456\n-----END EC PRIVATE KEY-----\n",
            "EC key",
        ),
        (
            "-----BEGIN PRIVATE KEY-----\nSENTINEL_bad_body_789!!\n-----END PRIVATE KEY-----\n",
            "bad base64",
        ),
        (
            "-----BEGIN RSA PRIVATE KEY-----\nU0VOVElORUxfcnNhX2JvZHk=\n-----END RSA PRIVATE KEY-----\n",
            "base64 that is not a key",
        ),
    ] {
        f.set(SetSecretArgs {
            value: value.into(),
            ..sign_secret("unused")
        });
        let err = f
            .vault
            .sign_app_jwt(&secret, "42", "ferris", HOST, now())
            .unwrap_err();
        let shown = format!("{err} / {err:?}");
        assert!(!shown.contains("SENTINEL"), "{label}: {shown}");
        assert!(!shown.contains("U0VOVElORUxf"), "{label}: {shown}");
        assert!(shown.contains("RSA private key"), "{label}: {shown}");
        let e = f.last_audit();
        assert_eq!(e["outcome"], "denied:bad_key", "{label}");
        assert!(!f.audit_text().contains("SENTINEL"), "{label}");
    }
}

#[test]
fn sign_app_jwt_checks_policy_before_it_looks_at_the_key() {
    // The stored value is not a key at all. If the key were decrypted and parsed first, every
    // refusal below would be `denied:bad_key`; it is the policy that answers.
    let mut f = Fixture::new();
    f.set(SetSecretArgs {
        value: "SENTINEL_not_a_key_value_123".into(),
        ..sign_secret("unused")
    });
    let secret = parse_vault_ref("vault://GITHUB_APP_KEY").unwrap();
    f.vault
        .sign_app_jwt(&secret, "42", "ferris", "evil.example.com", now())
        .unwrap_err();
    assert_eq!(f.last_audit()["outcome"], "denied:host");
    f.vault
        .sign_app_jwt(&secret, "42", "mallory", HOST, now())
        .unwrap_err();
    assert_eq!(f.last_audit()["outcome"], "denied:agent");
    // The policy passes, so the key is read and found wanting.
    f.vault
        .sign_app_jwt(&secret, "42", "ferris", HOST, now())
        .unwrap_err();
    assert_eq!(f.last_audit()["outcome"], "denied:bad_key");
}

#[cfg(unix)]
#[test]
fn sign_app_jwt_returns_no_jwt_when_its_allowed_entry_cannot_be_written() {
    let Some(pem) = throwaway_pem(false) else {
        return;
    };
    let mut f = Fixture::new();
    f.set(sign_secret(&pem));
    std::os::unix::fs::symlink("/dev/null", f.audit_path()).unwrap();
    let secret = parse_vault_ref("vault://GITHUB_APP_KEY").unwrap();
    let err = f
        .vault
        .sign_app_jwt(&secret, "42", "ferris", HOST, now())
        .unwrap_err();
    assert!(matches!(err, GateError::Io { .. }), "{err}");
    // And a denial stays a denial.
    let err = f
        .vault
        .sign_app_jwt(&secret, "42", "mallory", HOST, now())
        .unwrap_err();
    assert!(matches!(err, GateError::PolicyDenied { .. }), "{err}");
}

#[test]
fn sign_app_jwt_does_not_leak_the_pem_through_debug_or_errors() {
    let Some(pem) = throwaway_pem(false) else {
        return;
    };
    let mut f = Fixture::new();
    f.set(sign_secret(&pem));
    let secret = parse_vault_ref("vault://GITHUB_APP_KEY").unwrap();
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let mut shown = format!("{:?}{:?}", f.vault, secret);
    for host in ["evil.example.com", HOST] {
        for caller in ["mallory", "ferris"] {
            match f.vault.sign_app_jwt(&secret, "42", caller, host, now()) {
                Ok(jwt) => shown.push_str(&jwt),
                Err(e) => shown.push_str(&format!("{e}{e:?}")),
            }
        }
    }
    shown.push_str(&f.audit_text());
    assert!(!shown.contains(&body[..40]), "the key body leaked");
    assert!(!shown.contains("PRIVATE KEY"));
}
