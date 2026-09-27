//! An in-process fake of the GitHub endpoints fl uses.
//!
//! ⚠ It proves structure, not integration (spec §8.4): it agrees with fl
//! because both were written from the same reading of GitHub's docs. The
//! live tests are the check against GitHub itself.

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

pub const INSTALLATION_TOKEN: &str = "fake-installation-token";
pub const USER_LOGIN: &str = "fake-user";
pub const APP_SLUG: &str = "fake-app";

#[derive(Debug, Clone)]
pub struct Repo {
    pub id: u64,
    pub node_id: String,
    pub full_name: String,
    pub visibility: String,
}

#[derive(Debug, Clone, Default)]
pub struct Issue {
    pub number: u64,
    pub node_id: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub state: String,
    pub state_reason: Option<String>,
    pub pull_request: bool,
    pub gone: bool,
    pub moved_to: Option<String>,
    /// (id, kind) of every timeline event, oldest first.
    pub events: Vec<(u64, String)>,
    /// Ids of the body's edit history, oldest first.
    pub edits: Vec<String>,
    pub comments: Vec<String>,
}

/// Everything the fake holds, and the knobs a test turns. Every knob is
/// one-shot or explicit; none is on by default.
#[derive(Debug, Default)]
pub struct State {
    pub base: String,
    pub repos: Vec<Repo>,
    /// Lowercase old name → repository id: GitHub's redirect after a rename.
    pub redirects: BTreeMap<String, u64>,
    pub labels: BTreeSet<String>,
    pub issues: BTreeMap<u64, Issue>,
    pub next_number: u64,
    pub next_event: u64,
    pub installations: BTreeMap<String, u64>,
    pub token_requests: u64,
    /// "METHOD /path?query" of every request, in order.
    pub requests: Vec<String>,
    /// Largest page the fake serves, whatever `per_page` asks. 0 = 100.
    pub max_per_page: usize,
    /// (path prefix, page number): that page answers 500.
    pub fail_page: Option<(String, u32)>,
    pub rate_limited: bool,
    pub graphql_rate_limited: bool,
    pub fail_repo_read: bool,
    pub drop_labels: bool,
    pub fail_after_create: bool,
    /// The create lands, then the connection breaks mid-answer.
    pub hang_up_after_create: bool,
    /// The create answers 502 and nothing lands.
    pub fail_before_create: bool,
    pub foreign_label_on_next_patch: bool,
    pub foreign_edit_on_next_patch: bool,
    /// The next request answers 502 with an HTML body — what a load
    /// balancer sends, not GitHub's JSON. One-shot.
    pub html_502_next: bool,
    /// The next installation lookup answers 301 with this `Location`,
    /// which is off the API's own origin. One-shot.
    pub off_origin_redirect_next: Option<String>,
}

pub struct FakeGithub {
    server: Arc<tiny_http::Server>,
    thread: Option<JoinHandle<()>>,
    state: Arc<Mutex<State>>,
    url: String,
}

impl FakeGithub {
    /// A fake whose one repository is `full_name`, private, with the App
    /// installed on it.
    pub fn start(full_name: &str) -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind a local port"));
        let port = server.server_addr().to_ip().expect("an IP listener").port();
        let url = format!("http://127.0.0.1:{port}");
        let state = Arc::new(Mutex::new(State {
            base: url.clone(),
            repos: vec![Repo {
                id: 1,
                node_id: "R_1".into(),
                full_name: full_name.into(),
                visibility: "private".into(),
            }],
            installations: BTreeMap::from([(full_name.to_ascii_lowercase(), 7)]),
            next_number: 1,
            next_event: 1,
            ..State::default()
        }));
        let (srv, st) = (Arc::clone(&server), Arc::clone(&state));
        let thread = std::thread::spawn(move || {
            while let Ok(mut req) = srv.recv() {
                let method = req.method().to_string();
                let url = req.url().to_string();
                let auth = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .map(|h| h.value.to_string())
                    .unwrap_or_default();
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
                let answer = route(&mut st.lock().unwrap(), &method, &url, &auth, &body);
                if answer.hang_up {
                    // A broken answer after the server acted: the client
                    // fails at once with a transport error. (Dropping the
                    // request unanswered would make tiny_http answer 500;
                    // a short body would hang the client until its timeout.)
                    let mut w = req.into_writer();
                    let _ = std::io::Write::write_all(&mut w, b"HTTP/9 broken\r\n\r\n");
                    continue;
                }
                let content_type = if answer.raw_body.is_some() {
                    "text/html"
                } else {
                    "application/json"
                };
                let text = answer.raw_body.unwrap_or_else(|| answer.body.to_string());
                let mut resp = tiny_http::Response::from_string(text)
                    .with_status_code(answer.status)
                    .with_header(header("Content-Type", content_type));
                for (k, v) in answer.headers {
                    resp = resp.with_header(header(&k, &v));
                }
                let _ = req.respond(resp);
            }
        });
        Self {
            server,
            thread: Some(thread),
            state,
            url,
        }
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Renames the fake's one repository, leaving a redirect from its old
    /// (lowercased) name to its id — GitHub's behaviour after a rename.
    /// ⚠ `state()` locks the fake's mutex: take one guard and reuse it,
    /// never call `state()` again while a guard from this call is alive, or
    /// the second call deadlocks on the first's lock.
    pub fn rename(&self, full_name: &str) {
        let mut s = self.state();
        let id = s.repos[0].id;
        let old = s.repos[0].full_name.to_ascii_lowercase();
        s.redirects.insert(old, id);
        s.repos[0].full_name = full_name.to_string();
    }

    pub fn issue(&self, n: u64) -> Issue {
        self.state().issues[&n].clone()
    }

    pub fn issue_count(&self) -> usize {
        self.state().issues.len()
    }

    /// A person changing an issue in the web interface, outside fl.
    pub fn web_edit(&self, n: u64, f: impl FnOnce(&mut Issue)) {
        let mut s = self.state();
        let e = s.tick();
        let issue = s.issues.get_mut(&n).expect("an issue to edit");
        f(issue);
        issue.events.push((e, "labeled".into()));
    }

    /// An issue fl did not make: `labels` as given, no block.
    pub fn plain_issue(&self, labels: &[&str], pull_request: bool) -> u64 {
        let mut s = self.state();
        let n = s.next_number;
        s.next_number += 1;
        let issue = Issue {
            number: n,
            node_id: format!("I_{n}"),
            title: "someone else's".into(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            state: "open".into(),
            pull_request,
            ..Issue::default()
        };
        s.issues.insert(n, issue);
        n
    }
}

impl Drop for FakeGithub {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn header(k: &str, v: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("an ASCII header")
}

pub(crate) struct Answer {
    status: u16,
    body: Value,
    /// When set, this exact text is sent instead of `body.to_string()` — for
    /// answers that are not JSON at all (an HTML 502 from a load balancer).
    raw_body: Option<String>,
    headers: Vec<(String, String)>,
    hang_up: bool,
}

fn answer(status: u16, body: Value) -> Answer {
    Answer {
        status,
        body,
        raw_body: None,
        headers: vec![],
        hang_up: false,
    }
}

/// An answer whose body is not JSON at all (spec's reading of what a load
/// balancer, not GitHub itself, sends on a 502/504).
fn raw_answer(status: u16, body: &str) -> Answer {
    Answer {
        status,
        body: Value::Null,
        raw_body: Some(body.to_string()),
        headers: vec![],
        hang_up: false,
    }
}

/// `path?query` → (path, query map).
fn split(url: &str) -> (String, BTreeMap<String, String>) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let q = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    (path.to_string(), q)
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

impl State {
    /// The repository every issue belongs to: the fake's first.
    fn bound(&self) -> &Repo {
        &self.repos[0]
    }

    /// `{o}/{r}` names the bound repository under its current name.
    fn is_bound(&self, o: &str, r: &str) -> bool {
        self.bound()
            .full_name
            .eq_ignore_ascii_case(&format!("{o}/{r}"))
    }

    fn tick(&mut self) -> u64 {
        let e = self.next_event;
        self.next_event += 1;
        e
    }

    fn issue_json(&self, i: &Issue) -> Value {
        let mut v = json!({
            "number": i.number,
            "node_id": i.node_id,
            "html_url": format!("https://github.com/{}/issues/{}", self.bound().full_name, i.number),
            "title": i.title,
            "body": i.body,
            "labels": i.labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
            "state": i.state,
            "state_reason": i.state_reason,
        });
        if i.pull_request {
            v["pull_request"] = json!({"url": "pull"});
        }
        v
    }

    fn repo_named(&self, name: &str) -> Option<&Repo> {
        self.repos
            .iter()
            .find(|r| r.full_name.eq_ignore_ascii_case(name))
    }

    fn repo_json(&self, r: &Repo) -> Value {
        json!({
            "id": r.id, "node_id": r.node_id, "full_name": r.full_name,
            "visibility": r.visibility, "private": r.visibility == "private",
        })
    }

    /// One page of `items`, with a `Link` header when more remain.
    fn page(&self, path: &str, q: &BTreeMap<String, String>, items: Vec<Value>) -> Answer {
        let asked: usize = q.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(30);
        let cap = if self.max_per_page == 0 {
            100
        } else {
            self.max_per_page
        };
        let size = asked.min(cap).max(1);
        let page: u32 = q.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
        if let Some((prefix, bad)) = &self.fail_page
            && path.starts_with(prefix.as_str())
            && *bad == page
        {
            return answer(500, json!({"message": "fake page failure"}));
        }
        let start = (page as usize - 1) * size;
        let chunk: Vec<Value> = items.iter().skip(start).take(size).cloned().collect();
        let mut a = answer(200, Value::Array(chunk));
        if start + size < items.len() {
            let mut next = q.clone();
            next.insert("page".into(), (page + 1).to_string());
            let query: Vec<String> = next.iter().map(|(k, v)| format!("{k}={v}")).collect();
            a.headers.push((
                "Link".into(),
                format!("<{}{path}?{}>; rel=\"next\"", self.base, query.join("&")),
            ));
        }
        a
    }
}

/// The installation of the repository with id `repo`, keyed by the name it
/// was installed under — which a rename does not change.
fn installation(s: &State, repo: Option<u64>) -> Answer {
    let installed = repo.and_then(|id| {
        s.installations
            .iter()
            .find(|(name, _)| {
                s.repos
                    .iter()
                    .any(|r| r.id == id && r.full_name.eq_ignore_ascii_case(name))
                    || s.redirects.get(*name) == Some(&id)
            })
            .map(|(_, inst)| *inst)
    });
    match installed {
        Some(inst) => answer(200, json!({"id": inst})),
        None => answer(404, json!({"message": "Not Found"})),
    }
}

/// Every route the fake serves. Later tasks add arms above the final `_`.
pub(crate) fn route(s: &mut State, method: &str, url: &str, auth: &str, body: &str) -> Answer {
    s.requests.push(format!("{method} {url}"));
    if method == "POST" && url == "/graphql" && std::mem::take(&mut s.graphql_rate_limited) {
        return answer(
            200,
            json!({"data": null, "errors": [{"type": "RATE_LIMITED"}]}),
        );
    }
    if s.rate_limited {
        s.rate_limited = false;
        let mut a = answer(403, json!({"message": "API rate limit exceeded"}));
        a.headers.push(("x-ratelimit-remaining".into(), "0".into()));
        a.headers
            .push(("x-ratelimit-reset".into(), "1700000000".into()));
        return a;
    }
    // What a load balancer sends on a 502, never GitHub itself: an HTML page,
    // not JSON. The caller must still see the status.
    if std::mem::take(&mut s.html_502_next) {
        return raw_answer(502, "<html>bad gateway</html>");
    }
    let (path, q) = split(url);
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, parts.as_slice()) {
        ("GET", ["repos", o, r]) => {
            if s.fail_repo_read {
                s.fail_repo_read = false;
                return answer(500, json!({"message": "fake repository failure"}));
            }
            let name = format!("{o}/{r}");
            if let Some(repo) = s.repo_named(&name) {
                return answer(200, s.repo_json(repo));
            }
            match s.redirects.get(&name.to_ascii_lowercase()) {
                Some(id) => {
                    let mut a = answer(301, json!({"message": "Moved Permanently"}));
                    a.headers
                        .push(("Location".into(), format!("{}/repositories/{id}", s.base)));
                    a
                }
                None => answer(404, json!({"message": "Not Found"})),
            }
        }
        ("GET", ["repositories", id]) => match s.repos.iter().find(|r| r.id.to_string() == *id) {
            Some(repo) => answer(200, s.repo_json(repo)),
            None => answer(404, json!({"message": "Not Found"})),
        },
        ("GET", ["repos", o, r, "installation"]) => {
            // A JWT is three dot-separated parts; the signature is checked
            // by the unit test, not here.
            if auth.trim_start_matches("Bearer ").split('.').count() != 3 {
                return answer(
                    401,
                    json!({"message": "A JSON web token could not be decoded"}),
                );
            }
            if let Some(to) = s.off_origin_redirect_next.take() {
                let mut a = answer(301, json!({"message": "Moved Permanently"}));
                a.headers.push(("Location".into(), to));
                return a;
            }
            let name = format!("{o}/{r}").to_ascii_lowercase();
            if s.repo_named(&name).is_none()
                && let Some(id) = s.redirects.get(&name)
            {
                let mut a = answer(301, json!({"message": "Moved Permanently"}));
                a.headers.push((
                    "Location".into(),
                    format!("{}/repositories/{id}/installation", s.base),
                ));
                return a;
            }
            let current = s.repo_named(&name).map(|r| r.id);
            installation(s, current)
        }
        ("GET", ["repositories", id, "installation"]) => {
            let current = s
                .repos
                .iter()
                .find(|r| r.id.to_string() == *id)
                .map(|r| r.id);
            installation(s, current)
        }
        ("GET", ["user"]) => answer(200, json!({"login": USER_LOGIN})),
        ("GET", ["app"]) => {
            if auth.trim_start_matches("Bearer ").split('.').count() != 3 {
                return answer(
                    401,
                    json!({"message": "A JSON web token could not be decoded"}),
                );
            }
            answer(200, json!({"slug": APP_SLUG}))
        }
        ("POST", ["app", "installations", _, "access_tokens"]) => {
            s.token_requests += 1;
            answer(
                201,
                json!({"token": INSTALLATION_TOKEN, "expires_at": "2099-01-01T00:00:00Z"}),
            )
        }
        ("GET", ["repos", _, _, "labels"]) => {
            let items = s.labels.iter().map(|l| json!({"name": l})).collect();
            s.page(&path, &q, items)
        }
        ("POST", ["repos", _, _, "labels"]) => {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            match v.get("name").and_then(Value::as_str) {
                Some(n) => {
                    s.labels.insert(n.to_string());
                    answer(201, json!({"name": n}))
                }
                None => answer(422, json!({"message": "name is missing"})),
            }
        }
        ("POST", ["repos", o, r, "issues"]) if s.is_bound(o, r) => {
            if std::mem::take(&mut s.fail_before_create) {
                return answer(502, json!({"message": "fake failure before the create"}));
            }
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let n = s.next_number;
            s.next_number += 1;
            let mut labels = str_list(&v["labels"]);
            if s.drop_labels {
                labels.clear();
            }
            let mut events = Vec::new();
            for l in &labels {
                s.labels.insert(l.clone());
                events.push((s.tick(), "labeled".to_string()));
            }
            let issue = Issue {
                number: n,
                node_id: format!("I_{n}"),
                title: v["title"].as_str().unwrap_or("").into(),
                body: v["body"].as_str().unwrap_or("").into(),
                labels,
                state: "open".into(),
                events,
                ..Issue::default()
            };
            s.issues.insert(n, issue);
            if std::mem::take(&mut s.fail_after_create) {
                return answer(
                    502,
                    json!({"message": "fake failure after the create landed"}),
                );
            }
            if std::mem::take(&mut s.hang_up_after_create) {
                let mut a = answer(201, Value::Null);
                a.hang_up = true;
                return a;
            }
            answer(201, s.issue_json(&s.issues[&n]))
        }
        ("GET", ["repos", o, r, "issues"]) if s.is_bound(o, r) => {
            let want: Vec<String> = q
                .get("labels")
                .map(|l| l.split(',').map(str::to_string).collect())
                .unwrap_or_default();
            let items = s
                .issues
                .values()
                .filter(|i| !i.gone && i.moved_to.is_none())
                .filter(|i| want.iter().all(|w| i.labels.contains(w)))
                .map(|i| s.issue_json(i))
                .collect();
            s.page(&path, &q, items)
        }
        ("GET", ["repos", o, r, "issues", n]) if s.is_bound(o, r) => {
            match n.parse::<u64>().ok().and_then(|n| s.issues.get(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                Some(i) if i.gone => answer(410, json!({"message": "This issue was deleted"})),
                Some(i) if i.moved_to.is_some() => {
                    let mut a = answer(301, json!({"message": "Moved Permanently"}));
                    a.headers
                        .push(("Location".into(), i.moved_to.clone().unwrap_or_default()));
                    a
                }
                Some(i) => answer(200, s.issue_json(i)),
            }
        }
        ("PATCH", ["repos", o, r, "issues", n]) if s.is_bound(o, r) => {
            let Some(n) = n.parse::<u64>().ok().filter(|n| s.issues.contains_key(n)) else {
                return answer(404, json!({"message": "Not Found"}));
            };
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            // A write by someone else that lands inside fl's window.
            if std::mem::take(&mut s.foreign_label_on_next_patch) {
                let e = s.tick();
                s.issues
                    .get_mut(&n)
                    .unwrap()
                    .events
                    .push((e, "labeled".into()));
            }
            if std::mem::take(&mut s.foreign_edit_on_next_patch) {
                // The fake's own rule: a FIRST edit also records the original.
                let first = s.issues[&n].edits.is_empty();
                for _ in 0..if first { 2 } else { 1 } {
                    let e = format!("E_{}", s.tick());
                    s.issues.get_mut(&n).unwrap().edits.push(e);
                }
            }
            let old = s.issues[&n].clone();
            let mut new = old.clone();
            if let Some(t) = v.get("title").and_then(Value::as_str) {
                new.title = t.into();
            }
            if let Some(b) = v.get("body").and_then(Value::as_str) {
                new.body = b.into();
            }
            if let Some(ls) = v.get("labels") {
                // GitHub silently keeps the old labels when the caller may
                // not set them.
                if !s.drop_labels {
                    new.labels = str_list(ls);
                }
            }
            if let Some(st) = v.get("state").and_then(Value::as_str) {
                new.state = st.into();
            }
            new.state_reason = v
                .get("state_reason")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut kinds: Vec<&str> = Vec::new();
            kinds.extend(
                new.labels
                    .iter()
                    .filter(|l| !old.labels.contains(l))
                    .map(|_| "labeled"),
            );
            kinds.extend(
                old.labels
                    .iter()
                    .filter(|l| !new.labels.contains(l))
                    .map(|_| "unlabeled"),
            );
            if old.state != new.state {
                kinds.push(if new.state == "closed" {
                    "closed"
                } else {
                    "reopened"
                });
            }
            if old.title != new.title {
                kinds.push("renamed");
            }
            for k in kinds {
                let e = s.tick();
                new.events.push((e, k.into()));
            }
            if old.body != new.body {
                // ⚠ Modelled, not measured: GitHub is taken to record the
                // original body as an entry at the FIRST edit, so a first
                // edit adds two entries. The live test (Task 10) checks it.
                if new.edits.is_empty() {
                    let e = format!("E_{}", s.tick());
                    new.edits.push(e);
                }
                let e = format!("E_{}", s.tick());
                new.edits.push(e);
            }
            for l in &new.labels {
                s.labels.insert(l.clone());
            }
            s.issues.insert(n, new);
            answer(200, s.issue_json(&s.issues[&n]))
        }
        // A lookup of an unknown node: GitHub answers 200 with `data.node`
        // null AND a NOT_FOUND error — an answer, not a failure (spec's
        // reading of GitHub's GraphQL error shape). Tasks 5-7 extend this
        // arm with real node lookups.
        ("POST", ["graphql"]) => answer(
            200,
            json!({"data": {"node": null}, "errors": [{"type": "NOT_FOUND"}]}),
        ),
        _ => answer(
            404,
            json!({"message": format!("the fake does not serve {method} {path}")}),
        ),
    }
}
