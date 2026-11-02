//! Owner warrant routes (Local Record 64), over `fl_gate::warrant`.
//!
//! - `GET  /api/v1/auth/grants`            the pinned public key and every grant
//! - `POST /api/v1/auth/grants`            issue a grant (Owner only)
//! - `POST /api/v1/auth/grants/verify`     check a grant by id or `flw1.` token
//! - `POST /api/v1/auth/grants/:id/revoke` revoke a grant (Owner only)
//!
//! A compact token is a credential: whoever holds it can present it. So only the Owner sees
//! `compact_token` in a listing. A peer gets the same grants with that field left out.
//!
//! `grant_id` is as much a capability as the token: by-id verify has the same power. Hiding
//! `compact_token` from peers only stops copying the portable form.
//!
//! Listing does not check signatures; `verify` is the authority.
//!
//! A peer may verify, which is the point of a warrant. A peer may spend (`consume`) a grant only
//! as the agent the request names and that the hub resolved for it: a peer cannot burn another
//! agent's single-use grant. That check is by name, and a peer chooses its own `X-Fl-Agent`, so
//! it stops accidents and casual abuse, not a determined peer.

use fl_gate::{
    GateError, IssueGrantArgs, StoredOwnerGrant, issue_owner_grant, list_owner_grants,
    owner_warrant_public_key, revoke_owner_grant, verify_owner_grant,
};
use serde_json::{Value, json};

use crate::HubState;
use crate::api::{HttpRequest, HttpResponse};
use crate::auth::HubAuthority;

const GRANTS: &str = "/api/v1/auth/grants";

/// Answer the warrant routes. `None` means the request is not one of them.
pub(crate) fn route(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> Option<HttpResponse> {
    let method = req.method.as_str();
    let path = req.path.trim_end_matches('/');
    match (method, path) {
        ("GET", GRANTS) => return Some(get_grants(state, authority)),
        ("POST", GRANTS) => return Some(post_grant(state, authority, req)),
        ("POST", p) if p == format!("{GRANTS}/verify") => {
            return Some(post_verify(state, authority, req));
        }
        _ => {}
    }
    let id = path
        .strip_prefix(GRANTS)?
        .strip_prefix('/')?
        .strip_suffix("/revoke")?;
    if method != "POST" || id.is_empty() || id.contains('/') {
        return None;
    }
    Some(post_revoke(state, authority, id))
}

fn owner_only() -> HttpResponse {
    HttpResponse::json(403, json!({ "error": "owner authority required" }))
}

fn gate_failure(err: GateError) -> HttpResponse {
    match err {
        GateError::WarrantInput { reason } => HttpResponse::json(400, json!({ "error": reason })),
        GateError::WarrantRejected { reason } => {
            HttpResponse::json(422, json!({ "valid": false, "error": reason }))
        }
        other => {
            // The detail has filesystem paths, and a peer can trigger this through verify.
            eprintln!("warrant store error: {other}");
            HttpResponse::json(500, json!({ "error": "warrant store error" }))
        }
    }
}

/// A grant as a peer sees it: everything but the token.
fn grant_view(stored: &StoredOwnerGrant, owner: bool) -> Value {
    let mut value = json!(stored);
    if !owner && let Some(map) = value.as_object_mut() {
        map.remove("compact_token");
    }
    value
}

/// Every grant, as stored. Listing does not check signatures; `verify` is the authority.
fn get_grants(state: &HubState, authority: &HubAuthority) -> HttpResponse {
    let home = state.config.home_override.as_deref();
    let public_key = match owner_warrant_public_key(home) {
        Ok(key) => key,
        Err(err) => return gate_failure(err),
    };
    let grants = match list_owner_grants(home, false) {
        Ok(grants) => grants,
        Err(err) => return gate_failure(err),
    };
    let owner = authority.is_owner();
    HttpResponse::json(
        200,
        json!({
            "public_key_b64": public_key,
            "grants": grants.iter().map(|g| grant_view(g, owner)).collect::<Vec<_>>(),
        }),
    )
}

fn body_object(req: &HttpRequest) -> Result<Value, HttpResponse> {
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<Value>(&req.body) {
        Ok(value) if value.is_object() => Ok(value),
        Ok(_) => Err(HttpResponse::json(
            400,
            json!({ "error": "JSON body must be an object" }),
        )),
        Err(err) => Err(HttpResponse::json(
            400,
            json!({ "error": format!("invalid JSON body: {err}") }),
        )),
    }
}

fn str_field(body: &Value, key: &str) -> Result<Option<String>, String> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn bool_field(body: &Value, key: &str) -> Result<bool, String> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(format!("`{key}` must be a boolean")),
    }
}

fn bad_request(error: String) -> HttpResponse {
    HttpResponse::json(400, json!({ "error": error }))
}

fn post_grant(state: &HubState, authority: &HubAuthority, req: &HttpRequest) -> HttpResponse {
    if !authority.is_owner() {
        return owner_only();
    }
    let body = match body_object(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let args = (|| -> Result<IssueGrantArgs, String> {
        let ttl_secs = match body.get("ttl_secs") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .ok_or_else(|| "`ttl_secs` must be a non-negative integer".to_string())?,
            ),
        };
        Ok(IssueGrantArgs {
            target_agent: str_field(&body, "target_agent")?.unwrap_or_default(),
            scope: str_field(&body, "scope")?.unwrap_or_default(),
            statement: str_field(&body, "statement")?.unwrap_or_default(),
            ttl_secs,
            no_expiry: bool_field(&body, "no_expiry")?,
            single_use: bool_field(&body, "single_use")?,
            // The issuer is who the hub authenticated, never what the body says.
            issued_by: Some(authority.actor_name().to_string()),
        })
    })();
    let args = match args {
        Ok(args) => args,
        Err(error) => return bad_request(error),
    };
    match issue_owner_grant(state.config.home_override.as_deref(), args) {
        Ok(stored) => HttpResponse::json(201, grant_view(&stored, true)),
        Err(err) => gate_failure(err),
    }
}

fn post_verify(state: &HubState, authority: &HubAuthority, req: &HttpRequest) -> HttpResponse {
    let body = match body_object(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let parsed = (|| -> Result<(String, Option<String>, bool), String> {
        Ok((
            str_field(&body, "id_or_token")?.unwrap_or_default(),
            str_field(&body, "target_agent")?,
            bool_field(&body, "consume")?,
        ))
    })();
    let (id_or_token, target_agent, consume) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => return bad_request(error),
    };
    if consume && !authority.is_owner() {
        let me = authority.actor_name();
        let named = target_agent.as_deref().map(str::trim);
        if !named.is_some_and(|t| t.eq_ignore_ascii_case(me)) {
            return HttpResponse::json(
                403,
                json!({ "error": "a peer may consume a grant only as itself: set target_agent to your own agent name" }),
            );
        }
    }
    match verify_owner_grant(
        state.config.home_override.as_deref(),
        &id_or_token,
        target_agent.as_deref(),
        consume,
    ) {
        Ok(verified) => {
            let mut grant = json!(verified.grant);
            if !authority.is_owner()
                && let Some(map) = grant.as_object_mut()
            {
                map.remove("compact_token");
            }
            HttpResponse::json(
                200,
                json!({
                    "valid": true,
                    "grant": grant,
                    "in_ledger": verified.in_ledger,
                    "consumed": verified.consumed,
                }),
            )
        }
        Err(err) => gate_failure(err),
    }
}

fn post_revoke(state: &HubState, authority: &HubAuthority, id: &str) -> HttpResponse {
    if !authority.is_owner() {
        return owner_only();
    }
    let id = crate::api::percent_decode(id);
    match revoke_owner_grant(state.config.home_override.as_deref(), &id) {
        Ok(stored) => HttpResponse::json(200, grant_view(&stored, true)),
        Err(GateError::WarrantRejected { reason }) => {
            HttpResponse::json(404, json!({ "error": reason }))
        }
        Err(err) => gate_failure(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handle_http_request;
    use crate::{HubServerConfig, ReviewerApps};
    use std::fs;
    use tempfile::TempDir;

    const OWNER: &str = "Bearer owner-tok-secret";
    const PEER: &str = "Bearer peer-tok-secret";

    fn state(tmp: &TempDir) -> HubState {
        HubState::new(HubServerConfig {
            listen: "127.0.0.1:0".into(),
            store_path: tmp.path().join("store.redb"),
            repo_root: tmp.path().to_path_buf(),
            ui_dir: None,
            owner_login: "lmcfarlin".into(),
            reviewer_apps: ReviewerApps::default(),
            owner_token: Some("owner-tok-secret".into()),
            peer_token: Some("peer-tok-secret".into()),
            trust_tailscale_headers: true,
            enable_terminal: false,
            session_ttl_secs: 3600,
            home_override: Some(tmp.path().to_path_buf()),
        })
    }

    fn call(
        state: &HubState,
        method: &str,
        path: &str,
        auth: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = HttpRequest::new(method, path).with_header("Authorization", auth);
        if let Some(body) = body {
            req = req.with_json_body(&body);
        }
        let resp = handle_http_request(state, &req);
        (resp.status, resp.json_value().unwrap_or(Value::Null))
    }

    fn issue(state: &HubState, body: Value) -> Value {
        let (status, grant) = call(state, "POST", GRANTS, OWNER, Some(body));
        assert_eq!(status, 201, "{grant}");
        grant
    }

    fn wanted(target: &str) -> Value {
        json!({
            "target_agent": target,
            "scope": "agent.replace",
            "statement": "Owner approves the replacement.",
        })
    }

    #[test]
    fn only_the_owner_issues_and_the_issuer_is_the_authenticated_owner() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let (status, _) = call(&state, "POST", GRANTS, PEER, Some(wanted("ferris")));
        assert_eq!(status, 403);
        assert!(!fl_gate::resolve_warrant_ledger_path(Some(tmp.path())).exists());

        let mut body = wanted("Ferris");
        body["issued_by"] = json!("forged");
        body["ttl_secs"] = json!(120);
        body["single_use"] = json!(true);
        let grant = issue(&state, body);
        assert_eq!(grant["issued_by"], "lmcfarlin");
        assert_eq!(grant["target_agent"], "ferris");
        assert_eq!(grant["single_use"], true);
        assert!(
            grant["compact_token"]
                .as_str()
                .unwrap()
                .starts_with("flw1.")
        );
        assert!(grant["grant_id"].as_str().unwrap().starts_with("fgw-"));
    }

    #[test]
    fn issue_validates_the_body() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        for body in [
            json!({}),
            json!({ "target_agent": "ferris", "scope": "BAD SCOPE", "statement": "x" }),
            json!({ "target_agent": "ferris", "scope": "a", "statement": "x", "ttl_secs": 0 }),
            json!({ "target_agent": "ferris", "scope": "a", "statement": "x", "ttl_secs": "9" }),
            json!({ "target_agent": 7, "scope": "a", "statement": "x" }),
            json!({ "target_agent": "f", "scope": "a", "statement": "x", "single_use": "yes" }),
            json!({ "target_agent": "f", "scope": "a", "statement": "x", "ttl_secs": 5, "no_expiry": true }),
        ] {
            let (status, out) = call(&state, "POST", GRANTS, OWNER, Some(body.clone()));
            assert_eq!(status, 400, "{body} -> {out}");
        }
        let (status, _) = call(&state, "POST", GRANTS, "Bearer wrong", Some(wanted("f")));
        assert_eq!(status, 401);
    }

    #[test]
    fn listing_shows_the_key_and_only_the_owner_sees_tokens() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let (status, empty) = call(&state, "GET", GRANTS, PEER, None);
        assert_eq!(status, 200);
        assert!(empty["public_key_b64"].is_null());
        assert_eq!(empty["grants"], json!([]));

        let grant = issue(&state, wanted("ferris"));
        let (_, owner_view) = call(&state, "GET", GRANTS, OWNER, None);
        assert_eq!(owner_view["public_key_b64"], grant["public_key_b64"]);
        assert_eq!(
            owner_view["grants"][0]["compact_token"],
            grant["compact_token"]
        );
        let (_, peer_view) = call(&state, "GET", GRANTS, PEER, None);
        let seen = &peer_view["grants"][0];
        assert_eq!(seen["grant_id"], grant["grant_id"]);
        assert!(seen.get("compact_token").is_none());
        assert!(!peer_view.to_string().contains("flw1."));
    }

    #[test]
    fn verify_accepts_an_id_or_a_token_and_rejects_with_422() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let grant = issue(&state, wanted("ferris"));
        for key in ["grant_id", "compact_token"] {
            let (status, out) = call(
                &state,
                "POST",
                "/api/v1/auth/grants/verify",
                PEER,
                Some(json!({ "id_or_token": grant[key], "target_agent": "FERRIS" })),
            );
            assert_eq!(status, 200, "{out}");
            assert_eq!(out["valid"], true);
            assert_eq!(out["grant"]["scope"], "agent.replace");
            assert_eq!(out["in_ledger"], true);
            // A peer does not get the token back from a verify either.
            assert!(out["grant"].get("compact_token").is_none());
        }
        let (status, out) = call(
            &state,
            "POST",
            "/api/v1/auth/grants/verify",
            PEER,
            Some(json!({ "id_or_token": grant["grant_id"], "target_agent": "felix" })),
        );
        assert_eq!((status, out["valid"].clone()), (422, json!(false)));
        assert!(out["error"].as_str().unwrap().contains("felix"));
        let (status, out) = call(
            &state,
            "POST",
            "/api/v1/auth/grants/verify",
            PEER,
            Some(json!({ "id_or_token": "fgw-0000000000000000" })),
        );
        assert_eq!(status, 422, "{out}");
        let (status, _) = call(
            &state,
            "POST",
            "/api/v1/auth/grants/verify",
            PEER,
            Some(json!({})),
        );
        assert_eq!(status, 400);
        let (status, _) = call(
            &state,
            "POST",
            "/api/v1/auth/grants/verify",
            PEER,
            Some(json!({ "id_or_token": "x", "consume": "yes" })),
        );
        assert_eq!(status, 400);
    }

    #[test]
    fn a_tampered_token_is_a_422() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let grant = issue(&state, wanted("ferris"));
        let token = grant["compact_token"].as_str().unwrap();
        let mut bytes = token.as_bytes().to_vec();
        let last = bytes.len() - 2;
        bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
        let forged = String::from_utf8(bytes).unwrap();
        let (status, out) = call(
            &state,
            "POST",
            "/api/v1/auth/grants/verify",
            OWNER,
            Some(json!({ "id_or_token": forged })),
        );
        assert_eq!(status, 422, "{out}");
        assert_eq!(out["valid"], false);
    }

    #[test]
    fn a_peer_consumes_only_as_itself_and_a_single_use_grant_goes_once() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let grant = issue(
            &state,
            json!({
                "target_agent": "*",
                "scope": "role.recast",
                "statement": "once",
                "single_use": true,
            }),
        );
        let id = grant["grant_id"].clone();
        let verify = |auth: &str, body: Value| {
            call(
                &state,
                "POST",
                "/api/v1/auth/grants/verify",
                auth,
                Some(body),
            )
        };
        // Without a target, or for someone else, a peer cannot spend it.
        for body in [
            json!({ "id_or_token": id, "consume": true }),
            json!({ "id_or_token": id, "consume": true, "target_agent": "felix" }),
        ] {
            assert_eq!(verify(PEER, body).0, 403);
        }
        // The Owner must name an agent too: an unnamed consume is a bad request.
        let (status, out) = verify(OWNER, json!({ "id_or_token": id, "consume": true }));
        assert_eq!(status, 400, "{out}");
        // The Owner may spend it for any agent.
        let (status, out) = verify(
            OWNER,
            json!({ "id_or_token": id, "consume": true, "target_agent": "felix" }),
        );
        assert_eq!(status, 200, "{out}");
        assert_eq!(out["consumed"], true);
        let (status, out) = verify(OWNER, json!({ "id_or_token": id }));
        assert_eq!(status, 422);
        assert!(out["error"].as_str().unwrap().contains("already consumed"));
    }

    #[test]
    fn a_store_failure_is_a_generic_500_without_paths() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let grant = issue(&state, wanted("ferris"));
        let ledger = fl_gate::resolve_warrant_ledger_path(Some(tmp.path()));
        fs::write(&ledger, "{ not json").unwrap();
        let (status, out) = call(
            &state,
            "POST",
            "/api/v1/auth/grants/verify",
            PEER,
            Some(json!({ "id_or_token": grant["grant_id"] })),
        );
        assert_eq!(status, 500, "{out}");
        let text = out.to_string();
        assert!(text.contains("warrant store error"), "{text}");
        assert!(!text.contains("warrants.json"), "{text}");
        assert!(!text.contains(tmp.path().to_str().unwrap()), "{text}");
    }

    #[test]
    fn a_peer_may_consume_as_its_own_agent_name() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let grant = issue(
            &state,
            json!({
                "target_agent": "felix",
                "scope": "agent.replace",
                "statement": "felix only",
                "single_use": true,
            }),
        );
        let req = HttpRequest::new("POST", "/api/v1/auth/grants/verify")
            .with_header("Authorization", PEER)
            .with_header("X-Fl-Agent", "Felix")
            .with_json_body(&json!({
                "id_or_token": grant["grant_id"],
                "consume": true,
                "target_agent": "felix",
            }));
        let resp = handle_http_request(&state, &req);
        assert_eq!(resp.status, 200, "{:?}", resp.json_value());
        assert_eq!(resp.json_value().unwrap()["consumed"], true);
    }

    #[test]
    fn revoke_is_owner_only_and_the_grant_then_fails_verification() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        let grant = issue(&state, wanted("ferris"));
        let id = grant["grant_id"].as_str().unwrap();
        let path = format!("{GRANTS}/{id}/revoke");
        assert_eq!(call(&state, "POST", &path, PEER, None).0, 403);
        let (status, out) = call(&state, "POST", &path, OWNER, None);
        assert_eq!(status, 200, "{out}");
        assert!(out["revoked_at"].is_string());
        let (status, out) = call(
            &state,
            "POST",
            "/api/v1/auth/grants/verify",
            PEER,
            Some(json!({ "id_or_token": id })),
        );
        assert_eq!(status, 422);
        assert!(out["error"].as_str().unwrap().contains("revoked"));
        assert_eq!(
            call(
                &state,
                "POST",
                &format!("{GRANTS}/fgw-nope/revoke"),
                OWNER,
                None
            )
            .0,
            404
        );
        // Not a revoke route.
        assert_eq!(call(&state, "GET", &path, OWNER, None).0, 404);
        assert_eq!(
            call(&state, "POST", &format!("{GRANTS}//revoke"), OWNER, None).0,
            404
        );
    }

    #[test]
    fn warrant_routes_need_authentication() {
        let tmp = TempDir::new().unwrap();
        let state = state(&tmp);
        for (method, path) in [
            ("GET", GRANTS),
            ("POST", GRANTS),
            ("POST", "/api/v1/auth/grants/verify"),
            ("POST", "/api/v1/auth/grants/fgw-x/revoke"),
        ] {
            let resp = handle_http_request(&state, &HttpRequest::new(method, path));
            assert_eq!(resp.status, 401, "{method} {path}");
        }
    }
}
