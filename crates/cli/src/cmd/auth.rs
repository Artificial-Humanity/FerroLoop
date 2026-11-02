//! `fl auth` — Owner warrants (Local Record 64): `grant`, `verify`, `list`, `revoke`.
//!
//! A warrant is an Ed25519-signed, scoped, expiring statement of Owner authority. `verify` and
//! `list` read only the ledger. `grant` and `revoke` change it and need the FerroGate Vault.
//!
//! Every agent that runs as the Owner's Unix user can open the Vault, so the Vault alone does
//! not prove the Owner is at the keyboard. As a second line of defence, `grant` and `revoke`
//! refuse to run inside an agent harness (`AGENT_PERSONA`, `CLAUDECODE=1`, `ANTIGRAVITY_AGENT=1`
//! or `CODEX_SANDBOX` is set) unless the Hub confirms an Owner bearer token. This is a guard
//! against a mistake, not a boundary: a process that unsets those variables is not stopped.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use fl_gate::{
    GateError, IssueGrantArgs, StoredOwnerGrant, issue_owner_grant, list_owner_grants,
    revoke_owner_grant, verify_owner_grant,
};
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum AuthCmd {
    /// Issue a signed Owner warrant (needs the Vault; see the module notes on the agent guard).
    Grant {
        /// Agent persona the warrant names, or `*` for any agent.
        #[arg(long)]
        target: String,
        /// Scope slug, e.g. `agent.replace`, `agents.md.edit`, `role.recast`.
        #[arg(long)]
        scope: String,
        /// Human-readable Owner directive.
        #[arg(long)]
        statement: String,
        /// Lifetime in seconds (default 3600).
        #[arg(long, conflicts_with = "no_expiry")]
        ttl_secs: Option<u64>,
        /// Never expire.
        #[arg(long)]
        no_expiry: bool,
        /// The first successful `verify --consume` spends the warrant.
        #[arg(long)]
        single_use: bool,
        /// Name recorded as the issuer (default `lmcfarlin`).
        #[arg(long)]
        issued_by: Option<String>,
        #[command(flatten)]
        hub: HubProof,
        /// Output JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check a warrant by id or `flw1.` token. Exits 0 when valid.
    Verify {
        /// A `grant_id` (`fgw-…`) or a compact `flw1.…` token.
        id_or_token: String,
        /// Agent the warrant must name (or be for `*`).
        #[arg(long)]
        target: Option<String>,
        /// Mark the warrant consumed in the same step as the check. Needs `--target`.
        #[arg(long, requires = "target")]
        consume: bool,
        /// Output JSON.
        #[arg(long)]
        json: bool,
    },
    /// List warrants in the ledger.
    List {
        /// Leave out revoked, consumed and expired warrants.
        #[arg(long)]
        active_only: bool,
        /// Output JSON.
        #[arg(long)]
        json: bool,
    },
    /// Revoke a warrant.
    Revoke {
        /// The `grant_id` to revoke.
        grant_id: String,
        #[command(flatten)]
        hub: HubProof,
        /// Output JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Proof of Owner authority for `grant` and `revoke` inside an agent harness.
#[derive(clap::Args, Clone, Default)]
pub struct HubProof {
    /// Hub Owner bearer token (default `$FL_HUB_TOKEN`). A command-line value is visible to
    /// `ps`; prefer the environment variable or `--hub-token-file`.
    #[arg(long)]
    hub_token: Option<String>,
    /// File that holds the Hub Owner bearer token.
    #[arg(long, conflicts_with = "hub_token")]
    hub_token_file: Option<PathBuf>,
    /// Hub base URL (default `$FL_HUB_URL`, else `http://127.0.0.1:8420`). The token is sent
    /// only to `https` or to this machine.
    #[arg(long)]
    hub_url: Option<String>,
}

/// The harness variable that makes this process look like an agent, if any.
fn agent_harness_marker(get: &dyn Fn(&str) -> Option<String>) -> Option<&'static str> {
    let set = |name: &str| get(name).is_some_and(|v| !v.trim().is_empty());
    let is_one = |name: &str| get(name).is_some_and(|v| v.trim() == "1");
    if set("AGENT_PERSONA") {
        Some("AGENT_PERSONA")
    } else if is_one("CLAUDECODE") {
        Some("CLAUDECODE")
    } else if is_one("ANTIGRAVITY_AGENT") {
        Some("ANTIGRAVITY_AGENT")
    } else if set("CODEX_SANDBOX") {
        Some("CODEX_SANDBOX")
    } else {
        None
    }
}

/// Whether a bearer token may be sent to `url`: `https`, or `http` to this machine.
pub(crate) fn may_receive_token(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Userinfo (`@`) would make the real host something else; a backslash is a
    // path delimiter to some parsers, so the authority could end earlier there.
    if authority.contains(['@', '\\']) {
        return false;
    }
    let (host, port) = match authority.strip_prefix('[') {
        // `[v6]` or `[v6]:port`; anything else after the bracket is not a port.
        Some(v6) => match v6.split_once(']') {
            Some((h, "")) => (h, ""),
            Some((h, tail)) => match tail.strip_prefix(':') {
                Some(p) => (h, p),
                None => return false,
            },
            None => return false,
        },
        None => authority.rsplit_once(':').unwrap_or((authority, "")),
    };
    // A port, if present, is digits only.
    if !port.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn hub_base_url(proof: &HubProof, get: &dyn Fn(&str) -> Option<String>) -> String {
    let raw = proof
        .hub_url
        .clone()
        .or_else(|| get("FL_HUB_URL"))
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("http://{}", fl_hub::DEFAULT_HUB_LISTEN));
    let raw = raw.trim_end_matches('/').to_string();
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw
    } else {
        format!("http://{raw}")
    }
}

/// Ask the Hub who `token` is. `Ok(login)` only for the Owner tier.
fn hub_owner_login(base_url: &str, token: &str) -> Result<String, String> {
    if !may_receive_token(base_url) {
        return Err(format!(
            "refusing to send the Hub token to {base_url}: use https or a local address"
        ));
    }
    if token.is_empty() || token.chars().any(|c| c.is_control()) {
        return Err("the Hub token is empty or malformed".to_string());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let mut resp = agent
        .get(&format!("{base_url}/api/v1/auth/session"))
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| format!("could not reach the Hub at {base_url}: {e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!(
            "the Hub did not accept the token (HTTP {})",
            resp.status().as_u16()
        ));
    }
    let body: Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("unreadable Hub response: {e}"))?;
    let authority = &body["authority"];
    if authority["tier"].as_str() == Some("owner") {
        Ok(authority["login"]
            .as_str()
            .unwrap_or(fl_gate::DEFAULT_ISSUER)
            .to_string())
    } else {
        Err("the Hub token is not an Owner token".to_string())
    }
}

/// What the guard decided for `grant` and `revoke`.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    /// Run. `Some(login)` when the Hub confirmed an Owner token.
    Allow(Option<String>),
    Refuse(String),
}

fn gate_issuance(
    get: &dyn Fn(&str) -> Option<String>,
    proof: &HubProof,
    verify_with_hub: &dyn Fn(&str, &str) -> Result<String, String>,
) -> Gate {
    let token = match (&proof.hub_token, &proof.hub_token_file) {
        (Some(t), _) if !t.trim().is_empty() => Some(t.trim().to_string()),
        (_, Some(path)) => match fs::read_to_string(path) {
            Ok(text) => Some(text.trim().to_string()),
            Err(e) => return Gate::Refuse(format!("could not read {}: {e}", path.display())),
        },
        _ => get("FL_HUB_TOKEN")
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
    };
    match (token, agent_harness_marker(get)) {
        (Some(token), _) => match verify_with_hub(&hub_base_url(proof, get), &token) {
            Ok(login) => Gate::Allow(Some(login)),
            Err(why) => Gate::Refuse(why),
        },
        (None, Some(marker)) => Gate::Refuse(format!(
            "this looks like an agent session (`{marker}` is set); the Owner must run this, or \
             pass --hub-token / --hub-token-file / $FL_HUB_TOKEN so the Hub can confirm Owner \
             authority"
        )),
        (None, None) => Gate::Allow(None),
    }
}

fn guard(proof: &HubProof) -> Result<Option<String>, (i32, String)> {
    let get = |name: &str| std::env::var(name).ok();
    match gate_issuance(&get, proof, &hub_owner_login) {
        Gate::Allow(login) => Ok(login),
        Gate::Refuse(why) => Err((3, why)),
    }
}

fn gate_error(err: GateError) -> (i32, String) {
    match err {
        GateError::WarrantRejected { reason } => (1, format!("INVALID: {reason}")),
        GateError::WarrantInput { reason } => (2, reason),
        other => (2, other.to_string()),
    }
}

fn print_json(value: &Value) -> Result<(), (i32, String)> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| (2, format!("failed to serialize JSON: {e}")))?;
    println!("{text}");
    Ok(())
}

fn expiry_text(stored: &StoredOwnerGrant) -> &str {
    stored.grant.expires_at.as_deref().unwrap_or("never")
}

fn state_text(stored: &StoredOwnerGrant) -> &'static str {
    state_text_at(stored, now_unix_millis())
}

fn state_text_at(stored: &StoredOwnerGrant, now_ms: u64) -> &'static str {
    let expired =
        stored.grant.expires_at.as_deref().is_some_and(|text| {
            fl_core::At::parse(text).is_ok_and(|at| at.unix_millis() <= now_ms)
        });
    if stored.revoked_at.is_some() {
        "revoked"
    } else if stored.grant.single_use && stored.consumed_at.is_some() {
        "consumed"
    } else if expired {
        "expired"
    } else {
        "open"
    }
}

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub fn run(cmd: AuthCmd) -> Result<i32, (i32, String)> {
    match cmd {
        AuthCmd::Grant {
            target,
            scope,
            statement,
            ttl_secs,
            no_expiry,
            single_use,
            issued_by,
            hub,
            json,
        } => {
            let owner_login = guard(&hub)?;
            let stored = issue_owner_grant(
                None,
                IssueGrantArgs {
                    target_agent: target,
                    scope,
                    statement,
                    ttl_secs,
                    no_expiry,
                    single_use,
                    // A confirmed Owner login names the issuer; otherwise what was asked for.
                    issued_by: owner_login.or(issued_by),
                },
            )
            .map_err(gate_error)?;
            if json {
                print_json(&json!(stored))?;
            } else {
                let g = &stored.grant;
                println!("granted {}", g.grant_id);
                println!("[OwnerGrant: {}]", g.grant_id);
                println!("issued_by: {}", g.issued_by);
                println!("target: {}", g.target_agent);
                println!("scope: {}", g.scope);
                println!("expires_at: {}", expiry_text(&stored));
                println!("single_use: {}", g.single_use);
                println!("token: {}", g.compact_token);
            }
            Ok(0)
        }
        AuthCmd::Verify {
            id_or_token,
            target,
            consume,
            json,
        } => match verify_owner_grant(None, &id_or_token, target.as_deref(), consume) {
            Ok(v) => {
                if json {
                    print_json(&json!({ "valid": true, "verified": v }))?;
                } else {
                    let g = &v.grant;
                    println!("VALID");
                    println!("grant_id: {}", g.grant_id);
                    println!("issued_by: {}", g.issued_by);
                    println!("target: {}", g.target_agent);
                    println!("scope: {}", g.scope);
                    println!("statement: {}", g.statement);
                    println!("expires_at: {}", g.expires_at.as_deref().unwrap_or("never"));
                    if !v.in_ledger {
                        println!("note: not in this ledger; revocation could not be checked");
                    }
                    if v.consumed {
                        println!("consumed: yes");
                    }
                }
                Ok(0)
            }
            Err(GateError::WarrantRejected { reason }) if json => {
                print_json(&json!({ "valid": false, "error": reason }))?;
                Ok(1)
            }
            Err(err) => Err(gate_error(err)),
        },
        AuthCmd::List { active_only, json } => {
            let grants = list_owner_grants(None, active_only).map_err(gate_error)?;
            if json {
                print_json(&json!(grants))?;
            } else if grants.is_empty() {
                println!("no warrants");
            } else {
                for stored in &grants {
                    let g = &stored.grant;
                    println!(
                        "{}\t{}\t{}\t{}\t{}\texpires {}",
                        g.grant_id,
                        state_text(stored),
                        g.target_agent,
                        g.scope,
                        if g.single_use {
                            "single-use"
                        } else {
                            "reusable"
                        },
                        expiry_text(stored)
                    );
                }
            }
            Ok(0)
        }
        AuthCmd::Revoke {
            grant_id,
            hub,
            json,
        } => {
            guard(&hub)?;
            let stored = revoke_owner_grant(None, &grant_id).map_err(gate_error)?;
            if json {
                print_json(&json!(stored))?;
            } else {
                println!("revoked {}", stored.grant.grant_id);
            }
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    fn proof(token: Option<&str>) -> HubProof {
        HubProof {
            hub_token: token.map(ToOwned::to_owned),
            hub_token_file: None,
            hub_url: Some("http://127.0.0.1:1".to_string()),
        }
    }

    fn no_hub(_: &str, _: &str) -> Result<String, String> {
        panic!("the Hub must not be asked");
    }

    #[test]
    fn harness_markers_are_detected() {
        for pairs in [
            &[("AGENT_PERSONA", "feira")][..],
            &[("CLAUDECODE", "1")],
            &[("ANTIGRAVITY_AGENT", "1")],
            &[("CODEX_SANDBOX", "seatbelt")],
        ] {
            assert!(agent_harness_marker(&env(pairs)).is_some(), "{pairs:?}");
        }
        for pairs in [
            &[][..],
            &[("AGENT_PERSONA", "  ")],
            &[("CLAUDECODE", "0")],
            &[("ANTIGRAVITY_AGENT", "")],
            &[("CODEX_SANDBOX", "")],
        ] {
            assert!(agent_harness_marker(&env(pairs)).is_none(), "{pairs:?}");
        }
    }

    #[test]
    fn an_agent_session_without_a_hub_token_is_refused() {
        let get = env(&[("AGENT_PERSONA", "feira")]);
        match gate_issuance(&get, &proof(None), &no_hub) {
            Gate::Refuse(why) => assert!(why.contains("AGENT_PERSONA"), "{why}"),
            other => panic!("{other:?}"),
        }
        // An empty token is no token.
        assert!(matches!(
            gate_issuance(&get, &proof(Some("  ")), &no_hub),
            Gate::Refuse(_)
        ));
    }

    fn stored(single_use: bool, expires_at: Option<&str>) -> StoredOwnerGrant {
        serde_json::from_value(json!({
            "grant_id": "fgw-x",
            "issued_by": "lmcfarlin",
            "target_agent": "ferris",
            "scope": "agent.replace",
            "statement": "s",
            "created_at": "2026-01-01T00:00:00.000Z",
            "expires_at": expires_at,
            "single_use": single_use,
            "public_key_b64": "k",
            "signature_b64": "s",
            "compact_token": "flw1.x.y",
        }))
        .unwrap()
    }

    #[test]
    fn the_state_shows_revoked_consumed_expired_or_open() {
        let now = fl_core::At::parse("2026-06-01T00:00:00.000Z")
            .unwrap()
            .unix_millis();
        let at = Some("2026-01-01T00:00:00.000Z".to_string());

        assert_eq!(state_text_at(&stored(false, None), now), "open");
        assert_eq!(
            state_text_at(&stored(false, Some("2027-01-01T00:00:00.000Z")), now),
            "open"
        );
        assert_eq!(
            state_text_at(&stored(false, Some("2026-03-01T00:00:00.000Z")), now),
            "expired"
        );
        // A plain grant that was "consumed" is still reusable.
        let mut plain = stored(false, None);
        plain.consumed_at = at.clone();
        assert_eq!(state_text_at(&plain, now), "open");
        let mut once = stored(true, None);
        once.consumed_at = at.clone();
        assert_eq!(state_text_at(&once, now), "consumed");
        let mut revoked = stored(true, Some("2026-03-01T00:00:00.000Z"));
        revoked.consumed_at = at.clone();
        revoked.revoked_at = at;
        assert_eq!(state_text_at(&revoked, now), "revoked");
    }

    #[test]
    fn a_plain_terminal_may_issue_without_a_hub() {
        assert_eq!(
            gate_issuance(&env(&[]), &proof(None), &no_hub),
            Gate::Allow(None)
        );
    }

    #[test]
    fn an_agent_session_with_a_confirmed_owner_token_may_issue() {
        let get = env(&[("CLAUDECODE", "1")]);
        let hub = |url: &str, token: &str| {
            assert_eq!(url, "http://127.0.0.1:1");
            assert_eq!(token, "owner-token");
            Ok("lmcfarlin".to_string())
        };
        assert_eq!(
            gate_issuance(&get, &proof(Some(" owner-token ")), &hub),
            Gate::Allow(Some("lmcfarlin".to_string()))
        );
        let denied = |_: &str, _: &str| Err("the Hub token is not an Owner token".to_string());
        assert_eq!(
            gate_issuance(&get, &proof(Some("peer-token")), &denied),
            Gate::Refuse("the Hub token is not an Owner token".to_string())
        );
    }

    #[test]
    fn the_hub_token_may_come_from_the_environment() {
        let get = env(&[("CLAUDECODE", "1"), ("FL_HUB_TOKEN", " env-token ")]);
        let hub = |_: &str, token: &str| {
            assert_eq!(token, "env-token");
            Ok("lmcfarlin".to_string())
        };
        assert_eq!(
            gate_issuance(&get, &proof(None), &hub),
            Gate::Allow(Some("lmcfarlin".to_string()))
        );
    }

    #[test]
    fn a_hub_token_file_is_read_and_a_missing_one_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner.token");
        fs::write(&path, "from-file\n").unwrap();
        let hub = |_: &str, token: &str| {
            assert_eq!(token, "from-file");
            Ok("lmcfarlin".to_string())
        };
        let mut p = proof(None);
        p.hub_token_file = Some(path);
        assert_eq!(
            gate_issuance(&env(&[("AGENT_PERSONA", "x")]), &p, &hub),
            Gate::Allow(Some("lmcfarlin".to_string()))
        );
        p.hub_token_file = Some(dir.path().join("missing"));
        assert!(matches!(
            gate_issuance(&env(&[]), &p, &no_hub),
            Gate::Refuse(_)
        ));
    }

    #[test]
    fn the_token_goes_only_to_https_or_this_machine() {
        for ok in [
            "https://hub.example.com",
            "http://127.0.0.1:8420",
            "http://localhost:8420/x",
            "http://[::1]:8420",
        ] {
            assert!(may_receive_token(ok), "{ok}");
        }
        for bad in [
            "http://hub.example.com",
            "http://127.0.0.1.evil.com",
            "http://localhost.evil.com:80",
            "http://evil.com/127.0.0.1",
            "http://127.0.0.1@evil.com",
            "http://127.0.0.1:80@evil.com",
            "http://localhost:1@evil.com",
            "http://127.0.0.1:80\\@evil.com",
            "http://user:pw@127.0.0.1:80",
            "http://127.0.0.1:abc",
            "http://[::1]x",
            "ftp://127.0.0.1",
            "127.0.0.1:8420",
        ] {
            assert!(!may_receive_token(bad), "{bad}");
        }
        assert!(
            hub_owner_login("http://hub.example.com", "t")
                .unwrap_err()
                .contains("refusing to send")
        );
    }

    #[test]
    fn hub_url_defaults_and_normalizes() {
        let p = HubProof::default();
        assert_eq!(
            hub_base_url(&p, &env(&[])),
            format!("http://{}", fl_hub::DEFAULT_HUB_LISTEN)
        );
        assert_eq!(
            hub_base_url(&p, &env(&[("FL_HUB_URL", "https://hub.example.com/")])),
            "https://hub.example.com"
        );
        let p = HubProof {
            hub_url: Some("127.0.0.1:9000/".to_string()),
            ..HubProof::default()
        };
        assert_eq!(
            hub_base_url(&p, &env(&[("FL_HUB_URL", "https://other")])),
            "http://127.0.0.1:9000"
        );
    }
}
