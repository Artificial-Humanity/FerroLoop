//! `fl auth` end to end, on the real binary with a private HOME.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        Self { _tmp: tmp, root }
    }

    fn fl(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fl"));
        c.current_dir(&self.root)
            .env("HOME", &self.root)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_RUNTIME_DIR", &self.root)
            .env("XDG_CACHE_HOME", &self.root);
        for name in [
            "FL_DB",
            "FL_VAULT_PATH",
            "FL_VAULT_KEY_PATH",
            "FL_VAULT_MASTER_KEY",
            "FL_HUB_TOKEN",
            "FL_HUB_URL",
            "AGENT_PERSONA",
            "AGENT_NAME",
            "CLAUDECODE",
            "ANTIGRAVITY_AGENT",
            "CODEX_SANDBOX",
        ] {
            c.env_remove(name);
        }
        c
    }

    fn ledger(&self) -> PathBuf {
        self.root.join("data/fl/warrants.json")
    }
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn grant(env: &Env, extra: &[&str]) -> Value {
    let out = env
        .fl()
        .args([
            "auth",
            "grant",
            "--target",
            "feira",
            "--scope",
            "agent.replace",
            "--statement",
            "Owner approves the replacement.",
            "--json",
        ])
        .args(extra)
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(code(&out), 0, "{stdout}\n{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

#[test]
fn grant_verify_list_and_revoke_with_exit_codes() {
    let env = Env::new();
    let g = grant(&env, &["--ttl-secs", "600"]);
    let id = g["grant_id"].as_str().unwrap().to_string();
    let token = g["compact_token"].as_str().unwrap().to_string();
    assert!(env.ledger().exists());

    // Valid by id and by token, for the named agent.
    for form in [&id, &token] {
        let out = env
            .fl()
            .args(["auth", "verify", form, "--target", "Feira"])
            .output()
            .unwrap();
        let (stdout, stderr) = text(&out);
        assert_eq!(code(&out), 0, "{stdout}\n{stderr}");
        assert!(stdout.starts_with("VALID"), "{stdout}");
        assert!(stdout.contains(&id) && stdout.contains("agent.replace"));
    }
    // Wrong agent: exit 1 and a reason.
    let out = env
        .fl()
        .args(["auth", "verify", &id, "--target", "felix"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
    assert!(text(&out).1.contains("INVALID"), "{:?}", text(&out));
    // JSON mode prints the verdict and still exits 1.
    let out = env
        .fl()
        .args(["auth", "verify", &id, "--target", "felix", "--json"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
    let verdict: Value = serde_json::from_str(&text(&out).0).unwrap();
    assert_eq!(verdict["valid"], false);

    let out = env.fl().args(["auth", "list", "--json"]).output().unwrap();
    assert_eq!(code(&out), 0);
    let listed: Value = serde_json::from_str(&text(&out).0).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["grant_id"], id.as_str());

    // Plain output of `grant` carries the paste tag.
    let out = env
        .fl()
        .args([
            "auth",
            "grant",
            "--target",
            "*",
            "--scope",
            "role.recast",
            "--statement",
            "s",
            "--no-expiry",
        ])
        .output()
        .unwrap();
    let stdout = text(&out).0;
    assert_eq!(code(&out), 0, "{stdout}");
    assert!(stdout.contains("[OwnerGrant: fgw-"), "{stdout}");
    assert!(stdout.contains("expires_at: never"), "{stdout}");

    let out = env.fl().args(["auth", "revoke", &id]).output().unwrap();
    assert_eq!(code(&out), 0, "{:?}", text(&out));
    for form in [&id, &token] {
        let out = env.fl().args(["auth", "verify", form]).output().unwrap();
        assert_eq!(code(&out), 1);
        assert!(text(&out).1.contains("revoked"));
    }
    let out = env
        .fl()
        .args(["auth", "list", "--active-only", "--json"])
        .output()
        .unwrap();
    let active: Value = serde_json::from_str(&text(&out).0).unwrap();
    assert_eq!(active.as_array().unwrap().len(), 1);
}

#[test]
fn single_use_is_spent_by_the_first_consuming_verify() {
    let env = Env::new();
    let g = grant(&env, &["--single-use"]);
    let id = g["grant_id"].as_str().unwrap();
    let first = env
        .fl()
        .args(["auth", "verify", id, "--consume", "--target", "feira"])
        .output()
        .unwrap();
    assert_eq!(code(&first), 0, "{:?}", text(&first));
    let second = env
        .fl()
        .args(["auth", "verify", id, "--consume", "--target", "feira"])
        .output()
        .unwrap();
    assert_eq!(code(&second), 1);
    assert!(text(&second).1.contains("already consumed"));
}

#[test]
fn consume_without_a_target_is_a_usage_error() {
    let env = Env::new();
    let g = grant(&env, &["--single-use"]);
    let id = g["grant_id"].as_str().unwrap();
    let out = env
        .fl()
        .args(["auth", "verify", id, "--consume"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 2, "{:?}", text(&out));
    assert!(text(&out).1.contains("--target"), "{:?}", text(&out));
}

#[test]
fn concurrent_processes_spend_a_single_use_grant_exactly_once() {
    let env = Env::new();
    let g = grant(&env, &["--single-use"]);
    let id = g["grant_id"].as_str().unwrap();
    // Spawn all, then wait, so the processes really overlap.
    let children: Vec<_> = (0..6)
        .map(|_| {
            env.fl()
                .args(["auth", "verify", id, "--consume", "--target", "feira"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let codes: Vec<i32> = children
        .into_iter()
        .map(|c| code(&c.wait_with_output().unwrap()))
        .collect();
    assert_eq!(codes.iter().filter(|c| **c == 0).count(), 1, "{codes:?}");
    assert_eq!(codes.iter().filter(|c| **c == 1).count(), 5, "{codes:?}");
}

#[test]
fn an_agent_session_cannot_grant_or_revoke_without_a_hub_owner_token() {
    let env = Env::new();
    for (var, value) in [
        ("AGENT_PERSONA", "feira"),
        ("CLAUDECODE", "1"),
        ("ANTIGRAVITY_AGENT", "1"),
        ("CODEX_SANDBOX", "seatbelt"),
    ] {
        let out = env
            .fl()
            .env(var, value)
            .args([
                "auth",
                "grant",
                "--target",
                "x",
                "--scope",
                "s",
                "--statement",
                "t",
            ])
            .output()
            .unwrap();
        assert_eq!(code(&out), 3, "{var}: {:?}", text(&out));
        assert!(text(&out).1.contains(var));
    }
    assert!(
        !env.ledger().exists(),
        "a refused grant must not create a ledger"
    );

    let g = grant(&env, &[]);
    let id = g["grant_id"].as_str().unwrap();
    let out = env
        .fl()
        .env("AGENT_PERSONA", "feira")
        .args(["auth", "revoke", id])
        .output()
        .unwrap();
    assert_eq!(code(&out), 3);
    // Still valid: the refused revoke changed nothing. Verify is open to agents.
    let out = env
        .fl()
        .env("AGENT_PERSONA", "feira")
        .args(["auth", "verify", id])
        .output()
        .unwrap();
    assert_eq!(code(&out), 0);
    let out = env
        .fl()
        .env("AGENT_PERSONA", "feira")
        .args(["auth", "list"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 0);
}

/// One-shot fake Hub: answers `GET /api/v1/auth/session` with `body` and returns the request.
fn fake_hub(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        request
    });
    (url, handle)
}

#[test]
fn a_hub_confirmed_owner_token_lets_an_agent_session_grant() {
    let env = Env::new();
    let (url, hub) =
        fake_hub(r#"{"authenticated":true,"authority":{"tier":"owner","login":"lmcfarlin"}}"#);
    let out = env
        .fl()
        .env("AGENT_PERSONA", "feira")
        .env("FL_HUB_TOKEN", "owner-secret")
        .env("FL_HUB_URL", &url)
        .args([
            "auth",
            "grant",
            "--target",
            "feira",
            "--scope",
            "agent.replace",
            "--statement",
            "t",
            "--issued-by",
            "forged",
            "--json",
        ])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(code(&out), 0, "{stdout}\n{stderr}");
    let g: Value = serde_json::from_str(&stdout).unwrap();
    // The Hub's login names the issuer, not the flag.
    assert_eq!(g["issued_by"], "lmcfarlin");
    let request = hub.join().unwrap();
    assert!(
        request.starts_with("GET /api/v1/auth/session "),
        "{request}"
    );
    assert!(request.contains("Bearer owner-secret") || request.contains("bearer owner-secret"));
    // The token never reaches the process's output.
    assert!(!stdout.contains("owner-secret") && !stderr.contains("owner-secret"));
}

#[test]
fn a_hub_peer_token_or_an_unreachable_hub_does_not_unlock_grant() {
    let env = Env::new();
    let (url, hub) = fake_hub(r#"{"authenticated":true,"authority":{"tier":"peer","agent":"x"}}"#);
    let out = env
        .fl()
        .env("AGENT_PERSONA", "feira")
        .env("FL_HUB_TOKEN", "peer-secret")
        .env("FL_HUB_URL", &url)
        .args([
            "auth",
            "grant",
            "--target",
            "x",
            "--scope",
            "s",
            "--statement",
            "t",
        ])
        .output()
        .unwrap();
    hub.join().unwrap();
    assert_eq!(code(&out), 3);
    assert!(
        text(&out).1.contains("not an Owner token"),
        "{:?}",
        text(&out)
    );

    let out = env
        .fl()
        .env("AGENT_PERSONA", "feira")
        .env("FL_HUB_TOKEN", "t")
        .env("FL_HUB_URL", "http://127.0.0.1:1")
        .args([
            "auth",
            "grant",
            "--target",
            "x",
            "--scope",
            "s",
            "--statement",
            "t",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&out), 3);

    // The token is sent only to https or this machine.
    let out = env
        .fl()
        .env("AGENT_PERSONA", "feira")
        .env("FL_HUB_TOKEN", "t")
        .env("FL_HUB_URL", "http://hub.example.com")
        .args([
            "auth",
            "grant",
            "--target",
            "x",
            "--scope",
            "s",
            "--statement",
            "t",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&out), 3);
    assert!(text(&out).1.contains("refusing to send"));
    assert!(!env.ledger().exists());
}
