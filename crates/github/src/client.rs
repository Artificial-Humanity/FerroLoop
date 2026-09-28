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
/// 2xx, 3xx, 404, 410 and 5xx. The caller decides what each means.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub location: Option<String>,
    link_next: Option<String>,
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

    /// One GraphQL query. An `errors` member is an error, never partial data.
    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value, StoreError> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let reply = self.send(Method::Post, "/graphql", Some(&body))?;
        if reply.status != 200 {
            return Err(StoreError::Backend(format!(
                "GitHub answered {} to a GraphQL query; retry",
                reply.status
            )));
        }
        // GitHub is taken to answer a lookup of a missing node with `null`
        // data AND a NOT_FOUND error — an answer, not a failure — and a
        // spent GraphQL rate limit the same way, as a 200 with an error of
        // type RATE_LIMITED. Both are readings of GitHub's docs: unmeasured;
        // no live test checks either yet.
        if let Some(errors) = reply
            .body
            .get("errors")
            .and_then(Value::as_array)
            .filter(|e| !e.is_empty())
        {
            let kinds: Vec<&str> = errors
                .iter()
                .map(|e| e.get("type").and_then(Value::as_str).unwrap_or(""))
                .collect();
            if kinds.contains(&"RATE_LIMITED") {
                return Err(StoreError::RateLimited {
                    reset: "GitHub's GraphQL limit resets (it did not say when)".into(),
                });
            }
            if !kinds.iter().all(|k| *k == "NOT_FOUND") {
                return Err(StoreError::Backend(format!(
                    "GitHub refused a GraphQL query: {}",
                    Value::Array(errors.clone())
                )));
            }
        }
        reply.body.get("data").cloned().ok_or_else(|| {
            StoreError::Backend("GitHub answered a GraphQL query with no `data`".into())
        })
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
    let rate_limited =
        matches!(status, 403 | 429) && (remaining.as_deref() == Some("0") || retry_after.is_some());
    match status {
        _ if rate_limited => Err(StoreError::RateLimited {
            reset: match (reset, retry_after) {
                (Some(r), _) => format!("{r} (unix seconds)"),
                (None, Some(s)) => format!("{s} seconds from now"),
                (None, None) => "an unknown time".into(),
            },
        }),
        200..=399 | 404 | 410 | 500..=599 => Ok(Reply {
            status,
            body,
            location,
            link_next,
        }),
        401 => Err(StoreError::Credential(format!(
            "GitHub refused the credential ({message}). Check the credential the binding names"
        ))),
        _ => Err(StoreError::Backend(format!(
            "GitHub answered {status} to {method:?} {url}: {message}"
        ))),
    }
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
}
