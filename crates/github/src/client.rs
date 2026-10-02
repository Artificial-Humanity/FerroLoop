//! One blocking client for the REST and GraphQL endpoints fl uses. It
//! classifies every answer once, so no caller can read a failure as data.

use crate::creds::Credentials;
use fl_core::StoreError;
use serde_json::Value;
use std::time::Duration;

pub const DEFAULT_API: &str = "https://api.github.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
}

/// An answer that is not a transport failure and not already an error:
/// 2xx, 3xx, 404, 410 and 5xx — and, for the one caller that judges every
/// refusal itself ([`Client::graphql_write`]), any other status but a rate
/// limit or a 401. The caller decides what each means.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub location: Option<String>,
    /// GitHub's `x-accepted-github-permissions`: what a refused request
    /// needed (spec §6.3), when it says.
    pub accepted_permissions: Option<String>,
    link_next: Option<String>,
}

/// A GraphQL answer for a caller that judges it itself (the ledger's
/// commit, GitHub ledger spec §3.2): the HTTP status (200, or a 5xx a write
/// may hide behind), `data` when present, and every error.
#[derive(Debug, Clone)]
pub struct GraphqlAnswer {
    pub status: u16,
    pub data: Option<Value>,
    pub errors: Vec<Value>,
    /// GitHub's `message`, when the answer is a refusal that carries one.
    pub message: Option<String>,
    /// What a refused request needed, from `x-accepted-github-permissions`.
    pub needs: Option<String>,
}

impl GraphqlAnswer {
    /// The `data` of an answer to a query, judged as [`Client::graphql`]
    /// judges it: any status but 200 is an error, and so is an `errors`
    /// member — never partial data. For a caller that reads the status
    /// first (the ledger's reads, which take a 5xx as transient).
    pub(crate) fn into_data(self) -> Result<Value, StoreError> {
        if self.status != 200 {
            return Err(StoreError::Backend(format!(
                "GitHub answered {} to a GraphQL query; retry",
                self.status
            )));
        }
        // GitHub is taken to answer a lookup of a missing node with `null`
        // data AND a NOT_FOUND error — an answer, not a failure. A reading
        // of GitHub's docs: unmeasured; no live test checks it yet.
        if !self.errors.is_empty()
            && !self
                .errors
                .iter()
                .all(|e| e.get("type").and_then(Value::as_str) == Some("NOT_FOUND"))
        {
            return Err(StoreError::Backend(format!(
                "GitHub refused a GraphQL query: {}",
                Value::Array(self.errors)
            )));
        }
        self.data.ok_or_else(|| {
            StoreError::Backend("GitHub answered a GraphQL query with no `data`".into())
        })
    }
}

pub struct Client {
    agent: ureq::Agent,
    api: String,
    creds: Box<dyn Credentials>,
}

impl Client {
    pub fn new(api: &str, creds: Box<dyn Credentials>) -> Self {
        Self {
            agent: agent(),
            api: api.trim_end_matches('/').to_string(),
            creds,
        }
    }

    /// Who fl writes as, from the credential.
    pub fn describe(&self) -> String {
        self.creds.describe()
    }

    /// `path_or_url` is a path under the API (`/repos/…`) or a full URL
    /// GitHub handed back (`Link`, `Location`) — which must be on the API's
    /// own origin, because the credential goes wherever it points.
    pub fn send(
        &self,
        method: Method,
        path_or_url: &str,
        body: Option<&Value>,
    ) -> Result<Reply, StoreError> {
        let url = self.url(path_or_url)?;
        let token = self.creds.token()?;
        send(&self.agent, method, &url, &token, body, &self.api, true)
    }

    /// Like `send`, but a 2xx body that fails to parse as JSON becomes
    /// `Value::Null` rather than an error, the same treatment `send` already
    /// gives a non-2xx body. For a caller that must judge an ambiguous write
    /// for itself (an issue create, spec §3.3): a garbled 201 body does not
    /// prove the write failed, so it must not be reported as a plain
    /// `Backend` error before that caller gets a chance to search for what
    /// it may have already created. A 2xx whose body cannot be read in full
    /// is the same case: `Value::Null`, not `Unreachable` — the status proves
    /// the write landed.
    pub(crate) fn send_unchecked_json(
        &self,
        method: Method,
        path_or_url: &str,
        body: Option<&Value>,
    ) -> Result<Reply, StoreError> {
        let url = self.url(path_or_url)?;
        let token = self.creds.token()?;
        send(&self.agent, method, &url, &token, body, &self.api, false)
    }

    /// Every page of a list, and how many requests it took. ⚠ A failure on
    /// ANY page is an error, never a short list (spec §3.7).
    pub fn get_all_paged(&self, path: &str) -> Result<(Vec<Value>, usize), StoreError> {
        let mut out = Vec::new();
        let mut next = Some(path.to_string());
        let mut pages = 0usize;
        while let Some(page) = next {
            pages += 1;
            let reply = self.send(Method::Get, &page, None)?;
            if reply.status != 200 {
                return Err(StoreError::Backend(format!(
                    "GitHub answered {} to a page of {path}. A list with a missing page is \
                     not a list; retry",
                    reply.status
                )));
            }
            let Value::Array(items) = reply.body else {
                return Err(StoreError::Backend(format!(
                    "GitHub answered a page of {path} with something that is not a list"
                )));
            };
            out.extend(items);
            next = reply.link_next;
        }
        Ok((out, pages))
    }

    /// Every page of a list. ⚠ A failure on ANY page is an error, never a
    /// short list (spec §3.7).
    pub fn get_all(&self, path: &str) -> Result<Vec<Value>, StoreError> {
        self.get_all_paged(path).map(|(items, _)| items)
    }

    /// One GraphQL request, judged only for a spent rate limit: the caller
    /// reads the status, the data and the errors.
    pub fn graphql_answer(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<GraphqlAnswer, StoreError> {
        self.graphql_reply(query, variables, Judging::Strict)
    }

    /// Like [`Self::graphql_answer`], for a write whose answer may be lost
    /// after it landed (the ledger's commit, GitHub ledger spec §3.2 step
    /// 5), and whose every refusal the caller judges itself:
    ///
    /// - a 2xx whose body is not JSON, or breaks off, is an answer with no
    ///   `data` and no errors — which says nothing about whether the write
    ///   landed — never an error that would read as "it did not";
    /// - any status but a rate limit or a 401 is an answer, with GitHub's
    ///   `message` and the permission it says was needed, so the caller can
    ///   tell a 403 for want of a permission from any other refusal.
    pub(crate) fn graphql_write(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<GraphqlAnswer, StoreError> {
        self.graphql_reply(query, variables, Judging::CallerJudges)
    }

    fn graphql_reply(
        &self,
        query: &str,
        variables: Value,
        judging: Judging,
    ) -> Result<GraphqlAnswer, StoreError> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let url = self.url("/graphql")?;
        let token = self.creds.token()?;
        let reply = exchange(
            &self.agent,
            Method::Post,
            &url,
            &token,
            Some(&body),
            &self.api,
            judging,
        )?;
        let errors: Vec<Value> = reply
            .body
            .get("errors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // A spent GraphQL rate limit is taken to be a 200 with an error of
        // type RATE_LIMITED: a reading of GitHub's docs, unmeasured.
        if errors
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("RATE_LIMITED"))
        {
            return Err(StoreError::RateLimited {
                reset: "GitHub's GraphQL limit resets (it did not say when)".into(),
            });
        }
        Ok(GraphqlAnswer {
            status: reply.status,
            data: reply.body.get("data").cloned(),
            errors,
            message: reply
                .body
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string),
            needs: reply.accepted_permissions,
        })
    }

    /// One GraphQL query. An `errors` member is an error, never partial data.
    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value, StoreError> {
        self.graphql_answer(query, variables)?.into_data()
    }

    /// Who GitHub says fl writes as (spec §5.4).
    pub fn identity(&self) -> Result<String, StoreError> {
        self.creds.identity(&self.api)
    }

    fn url(&self, path_or_url: &str) -> Result<String, StoreError> {
        if path_or_url.starts_with('/') {
            return Ok(format!("{}{path_or_url}", self.api));
        }
        if path_or_url.starts_with(&format!("{}/", self.api)) {
            return Ok(path_or_url.to_string());
        }
        Err(StoreError::Backend(format!(
            "refused to send the GitHub credential to {path_or_url}, which is not under {}",
            self.api
        )))
    }
}

/// No redirects are followed: a 301 is information (a renamed repository,
/// a transferred issue), and following one would carry the token along.
pub(crate) fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(30)))
        .build();
    ureq::Agent::new_with_config(config)
}

fn headers<B>(rb: ureq::RequestBuilder<B>, auth: &str) -> ureq::RequestBuilder<B> {
    rb.header("Authorization", auth)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", concat!("fl/", env!("CARGO_PKG_VERSION")))
}

/// One request with a bearer token. Shared by `Client` and the App's token
/// exchange, which signs with a JWT rather than a stored token.
///
/// `require_json_on_2xx`: when true (every caller except
/// `Client::send_unchecked_json`), a 2xx body that fails to parse as JSON is
/// itself an error, since a working GitHub answer is always JSON. The one
/// exception is an issue create, which must be able to tell "the write
/// failed" from "the write may have landed and only the answer was garbled"
/// (spec §3.3) — that caller passes `false` and judges the body itself.
pub(crate) fn send(
    agent: &ureq::Agent,
    method: Method,
    url: &str,
    token: &str,
    body: Option<&Value>,
    origin: &str,
    require_json_on_2xx: bool,
) -> Result<Reply, StoreError> {
    let judging = if require_json_on_2xx {
        Judging::Strict
    } else {
        Judging::Unchecked2xx
    };
    exchange(agent, method, url, token, body, origin, judging)
}

/// How much of an answer the client judges before its caller sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Judging {
    /// Every caller but two: a 2xx must be JSON; a refusal is an error.
    Strict,
    /// An issue create or edit: a 2xx body that cannot be read is
    /// `Value::Null`; a refusal is still an error.
    Unchecked2xx,
    /// The ledger's commit: as `Unchecked2xx`, and any status but a rate
    /// limit or a 401 is a reply the caller judges.
    CallerJudges,
}

fn exchange(
    agent: &ureq::Agent,
    method: Method,
    url: &str,
    token: &str,
    body: Option<&Value>,
    origin: &str,
    judging: Judging,
) -> Result<Reply, StoreError> {
    let require_json_on_2xx = judging == Judging::Strict;
    let unreachable = |e: ureq::Error| StoreError::Unreachable {
        store: origin.to_string(),
        cause: e.to_string(),
    };
    let auth = format!("Bearer {token}");
    let result = match (method, body) {
        (Method::Get, _) => headers(agent.get(url), &auth).call(),
        (Method::Post, Some(b)) => headers(agent.post(url), &auth).send_json(b),
        (Method::Post, None) => headers(agent.post(url), &auth).send_empty(),
        (Method::Patch, Some(b)) => headers(agent.patch(url), &auth).send_json(b),
        (Method::Patch, None) => headers(agent.patch(url), &auth).send_empty(),
    };
    let mut resp = result.map_err(unreachable)?;
    let status = resp.status().as_u16();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let location = header("location");
    let link_next = header("link").and_then(|l| next_link(&l));
    let remaining = header("x-ratelimit-remaining");
    let reset = header("x-ratelimit-reset");
    let retry_after = header("retry-after");
    // ⚠ Spec §6.3: GitHub names the permission a refused request needed.
    // Modelled from GitHub's documentation; no live test provokes it. The
    // header can ride along on ANY 403 — including a secondary rate limit,
    // or the rulesets-unavailable upgrade refusal — so it is read here but
    // only ever consulted by the LAST arm below, after every other 403
    // reading (rate-limited first) has had its chance to claim the status.
    let accepted = header("x-accepted-github-permissions");
    let is_2xx = (200..300).contains(&status);
    let text = match resp.body_mut().read_to_string() {
        Ok(text) => text,
        // ⚠ The status line was already read as 2xx: the write landed, and
        // only its answer broke off (a truncated body, a read timeout). For
        // the one caller that judges a create itself, that is a 2xx with an
        // unreadable body — never `Unreachable`, which would read as "may
        // not have happened" and could lead to a resend (spec §3.3).
        Err(_) if is_2xx && !require_json_on_2xx => {
            return Ok(Reply {
                status,
                body: Value::Null,
                location,
                accepted_permissions: accepted,
                link_next,
            });
        }
        Err(e) => return Err(unreachable(e)),
    };
    // The status is classified BEFORE the body is required to parse: GitHub's
    // load balancers answer a 502/504 with an HTML page, and a 401 or 429 can
    // be non-JSON too. Only a 2xx must be JSON — anything else that fails to
    // parse becomes `Value::Null` so the caller still sees the real status.
    // `require_json_on_2xx` lets one caller (an issue create) opt out of the
    // 2xx rule too, so it can judge a garbled answer itself.
    let body = if text.trim().is_empty() {
        Value::Null
    } else {
        match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) if is_2xx && require_json_on_2xx => {
                return Err(StoreError::Backend(format!(
                    "GitHub answered {method:?} {url} with a body that is not JSON ({e})"
                )));
            }
            Err(_) => Value::Null,
        }
    };
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // ⚠ GitHub's SECONDARY rate limit (GitHub ledger spec §1.6): a 403 or
    // 429 whose message says so. It often leaves `x-ratelimit-remaining`
    // above zero and may carry no `retry-after`, so neither header alone
    // finds it. Modelled from GitHub's documentation — "You have exceeded a
    // secondary rate limit" — and not provoked in a live test, because doing
    // so would abuse the API.
    let secondary = matches!(status, 403 | 429)
        && message
            .to_ascii_lowercase()
            .contains("secondary rate limit");
    let rate_limited = matches!(status, 403 | 429)
        && (secondary || remaining.as_deref() == Some("0") || retry_after.is_some());
    match status {
        _ if rate_limited => Err(StoreError::RateLimited {
            reset: reset_time(
                secondary,
                remaining.as_deref(),
                reset.as_deref(),
                retry_after.as_deref(),
            ),
        }),
        200..=399 | 404 | 410 | 500..=599 => Ok(Reply {
            status,
            body,
            location,
            accepted_permissions: accepted,
            link_next,
        }),
        401 => Err(StoreError::Credential(format!(
            "GitHub refused the credential ({message}). Check the credential the binding names"
        ))),
        // ⚠ The ledger's commit judges every refusal itself (a 403 for
        // want of a permission is told from a 422 by its status there).
        _ if judging == Judging::CallerJudges => Ok(Reply {
            status,
            body,
            location,
            accepted_permissions: accepted,
            link_next,
        }),
        // ⚠ Worded neutrally: the header can be present on a 403 that is
        // not a missing-permission refusal at all (a secondary rate limit,
        // or the rulesets upgrade refusal both carry it in this fake's
        // model), and GitHub's own `message` is kept verbatim so a reader
        // of the error still sees what actually happened, not just a
        // permission name.
        403 if accepted.is_some() => Err(StoreError::Backend(format!(
            "GitHub answered 403 to {method:?} {url}: {message}. GitHub says this needs: {}",
            accepted.unwrap_or_default()
        ))),
        _ => Err(StoreError::Backend(format!(
            "GitHub answered {status} to {method:?} {url}: {message}"
        ))),
    }
}

/// When a rate limit lifts, from the headers that say so.
///
/// `x-ratelimit-reset` is the PRIMARY window's reset, and means something
/// only when that window is spent (`remaining` is `0`). A secondary limit
/// is lifted by `retry-after`, or — GitHub's documentation says — after at
/// least a minute when it gives no time.
fn reset_time(
    secondary: bool,
    remaining: Option<&str>,
    reset: Option<&str>,
    retry_after: Option<&str>,
) -> String {
    if remaining == Some("0")
        && let Some(r) = reset
    {
        return format!("{r} (unix seconds)");
    }
    if let Some(s) = retry_after {
        return format!("{s} seconds from now");
    }
    if secondary {
        return "at least a minute from now: GitHub gave no time for its secondary limit, and \
                its documentation says to wait at least one minute"
            .into();
    }
    "an unknown time".into()
}

/// The `rel="next"` URL of a `Link` header, if any.
fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    #[test]
    fn next_link_reads_the_next_url_and_nothing_else() {
        let h = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#;
        assert_eq!(
            next_link(h).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert_eq!(
            next_link(r#"<https://api.github.com/x?page=1>; rel="prev""#),
            None
        );
    }

    #[test]
    fn the_credential_is_never_sent_off_the_api_origin() {
        let fake = FakeGithub::start("acme/widgets");
        let err = client(&fake)
            .send(Method::Get, "https://elsewhere.example/steal", None)
            .unwrap_err();
        assert!(err.to_string().contains("refused to send"), "{err}");
    }

    #[test]
    fn a_rate_limit_is_an_error_naming_the_reset() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().rate_limited = true;
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::RateLimited { ref reset } if reset.contains("1700000000")),
            "{err:?}"
        );
    }

    #[test]
    fn a_server_error_is_a_reply_the_caller_must_judge() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().fail_repo_read = true;
        let r = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap();
        assert_eq!(r.status, 500);
    }

    #[test]
    fn a_502_with_an_html_body_is_still_a_reply_the_caller_must_judge() {
        // GitHub's own answers are JSON; a load balancer's 502/504 is not.
        // The status must still reach the caller rather than being read as
        // a JSON-parse failure.
        let fake = FakeGithub::start("acme/widgets");
        fake.state().html_502_next = true;
        let r = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap();
        assert_eq!(r.status, 502);
        assert!(
            r.body.is_null(),
            "a non-JSON body becomes null: {:?}",
            r.body
        );
    }

    #[test]
    fn a_server_that_is_not_there_is_unreachable() {
        let c = Client::new(
            "http://127.0.0.1:9",
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        );
        let err = c
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    #[test]
    fn a_graphql_not_found_is_an_answer_and_a_graphql_rate_limit_is_an_error() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let data = c
            .graphql(
                "query($id: ID!) { node(id: $id) { ... on Issue { url } } }",
                serde_json::json!({"id": "I_404"}),
            )
            .unwrap();
        assert!(data["node"].is_null());
        fake.state().graphql_rate_limited = true;
        let err = c
            .graphql(
                "query($id: ID!) { node(id: $id) { ... on Issue { url } } }",
                serde_json::json!({"id": "I_404"}),
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::RateLimited { .. }), "{err:?}");
    }

    #[test]
    fn a_missing_page_fails_the_whole_list() {
        let fake = FakeGithub::start("acme/widgets");
        {
            let mut s = fake.state();
            s.max_per_page = 1;
            for name in ["a", "b", "c"] {
                s.labels.insert(name.into());
            }
            s.fail_page = Some(("/repos/acme/widgets/labels".into(), 2));
        }
        let err = client(&fake)
            .get_all("/repos/acme/widgets/labels?per_page=100")
            .unwrap_err();
        assert!(err.to_string().contains("missing page"), "{err}");
        fake.state().fail_page = None;
        let all = client(&fake)
            .get_all("/repos/acme/widgets/labels?per_page=100")
            .unwrap();
        assert_eq!(all.len(), 3, "every page, followed");
    }

    // Spec §1.6: GitHub's secondary limit leaves `x-ratelimit-remaining`
    // above zero and may send no `retry-after`; only its message names it.
    // The primary `x-ratelimit-reset` is not its reset time.
    #[test]
    fn a_secondary_rate_limit_without_a_retry_after_is_rate_limited_not_a_refusal() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().secondary_rate_limit_next = Some((403, None));
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        match err {
            StoreError::RateLimited { ref reset } => {
                assert!(reset.contains("at least a minute"), "{reset}");
                assert!(!reset.contains("1700000000"), "the primary reset: {reset}");
            }
            other => panic!("a secondary rate limit answered {other:?}"),
        }
    }

    #[test]
    fn a_secondary_rate_limit_with_a_retry_after_names_it() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().secondary_rate_limit_next = Some((429, Some(60)));
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::RateLimited { ref reset } if reset.contains("60 seconds")),
            "{err:?}"
        );
    }

    // The rule reads the message, not the status: a 403 for want of a
    // permission is a refusal, and says why.
    #[test]
    fn a_403_that_is_not_a_rate_limit_stays_a_refusal_naming_its_message() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().forbidden_next = true;
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref m) if m.contains("Resource not accessible")),
            "{err:?}"
        );
    }

    // The ledger judges a GraphQL answer itself: a stale commit is an
    // error it acts on, not one to report. Only a spent rate limit is
    // still refused here.
    #[test]
    fn a_graphql_answer_hands_its_errors_and_status_to_the_caller() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let a = c
            .graphql_answer(
                "query($id: ID!) { node(id: $id) { ... on Issue { url } } }",
                serde_json::json!({"id": "I_404"}),
            )
            .unwrap();
        assert_eq!(a.status, 200);
        assert_eq!(a.errors.len(), 1);
        assert_eq!(
            a.errors[0]["type"].as_str(),
            Some("NOT_FOUND"),
            "{:?}",
            a.errors
        );
        assert_eq!(a.data, Some(serde_json::json!({"node": null})));
        fake.state().graphql_rate_limited = true;
        let err = c
            .graphql_answer("query { viewer { login } }", serde_json::json!({}))
            .unwrap_err();
        assert!(matches!(err, StoreError::RateLimited { .. }), "{err:?}");
    }

    // `graphql` keeps refusing what it refused before `graphql_answer`
    // existed: a 5xx is not data.
    #[test]
    fn a_graphql_query_answered_with_a_5xx_is_an_error_not_data() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().html_502_next = true;
        let err = client(&fake)
            .graphql("query { viewer { login } }", serde_json::json!({}))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref m) if m.contains("502")),
            "{err:?}"
        );
    }

    // Spec §6.3: a missing permission is found by the first write, and the
    // error names it from GitHub's `x-accepted-github-permissions`.
    // Modelled from GitHub's documentation; no live test provokes it.
    #[test]
    fn a_refusal_for_want_of_a_permission_names_the_permission() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().permission_refused_next = Some("contents=write".into());
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains("contents=write"), "{err}");
    }

    // The ledger's commit judges every refusal itself: `graphql_write`
    // hands back a 403 with the permission it names, and a 422 with its
    // message, as answers — where `graphql_answer` refuses the 403. A
    // spent rate limit is still an error.
    #[test]
    fn a_graphql_write_hands_every_refusal_to_the_caller() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let q = "mutation { x }";
        fake.state().permission_refused_next = Some("contents=write".into());
        let a = c.graphql_write(q, serde_json::json!({})).unwrap();
        assert_eq!(a.status, 403);
        assert_eq!(a.needs.as_deref(), Some("contents=write"));
        assert!(a.message.is_some(), "{a:?}");
        fake.state().body_next.push((
            "/graphql".into(),
            422,
            serde_json::json!({"message": "Unprocessable"}),
        ));
        let a = c.graphql_write(q, serde_json::json!({})).unwrap();
        assert_eq!((a.status, a.needs), (422, None));
        assert_eq!(a.message.as_deref(), Some("Unprocessable"));
        fake.state().permission_refused_next = Some("contents=write".into());
        let err = c.graphql_answer(q, serde_json::json!({})).unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err:?}");
        fake.state().rate_limited = true;
        let err = c.graphql_write(q, serde_json::json!({})).unwrap_err();
        assert!(matches!(err, StoreError::RateLimited { .. }), "{err:?}");
    }

    // `graphql`'s production path (not `graphql_answer`'s): an error whose
    // type is neither RATE_LIMITED nor NOT_FOUND is still a refusal, never
    // read as partial data.
    #[test]
    fn a_graphql_error_that_is_not_all_not_found_is_refused() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().graphql_error_next = Some("FORBIDDEN".into());
        let err = client(&fake)
            .graphql("query { viewer { login } }", serde_json::json!({}))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref m) if m.contains("FORBIDDEN")),
            "{err:?}"
        );
    }

    // GitHub's `x-accepted-github-permissions` header can ride along on any
    // 403, including a secondary rate limit's — the rate-limit reading must
    // still win over the permission-named reading.
    #[test]
    fn a_secondary_rate_limit_still_rate_limits_even_carrying_a_permission_header() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().secondary_rate_limit_next = Some((403, None));
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(matches!(err, StoreError::RateLimited { .. }), "{err:?}");
    }
}
