//! An in-process fake of the GitHub endpoints fl uses.
//!
//! ⚠ It proves structure, not integration (spec §8.4): it agrees with fl
//! because both were written from the same reading of GitHub's docs. The
//! live tests (`tests/live.rs`) check part of that reading against GitHub
//! itself; a behaviour below that no live test checks says so.

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
    /// GitHub turns this off per-repository; a 410 on an issue can mean
    /// either "deleted" or "this repository has no Issues at all".
    pub has_issues: bool,
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
    /// The next comment answers 500 and is not posted. One-shot.
    pub fail_comment_next: bool,
    /// The next create that `fail_before_create` does not fail answers 403
    /// with GitHub's rate-limit headers, and nothing lands. One-shot.
    pub rate_limited_next_create: bool,
    pub foreign_label_on_next_patch: bool,
    pub foreign_edit_on_next_patch: bool,
    /// The next request answers 502 with an HTML body — what a load
    /// balancer sends, not GitHub's JSON. One-shot.
    pub html_502_next: bool,
    /// The next installation lookup answers 301 with this `Location`,
    /// which is off the API's own origin. One-shot.
    pub off_origin_redirect_next: Option<String>,
    /// The create answers 201 with a body that cannot be read as an issue —
    /// a garbled proxy answer, not GitHub's own. The issue is still created.
    /// One-shot.
    pub unreadable_create_body_next: bool,
    /// The next issue PATCH answers 200 with a body that cannot be read as
    /// an issue. The PATCH is still applied. One-shot.
    pub unreadable_patch_body_next: bool,
    /// How many requests the issues-LIST endpoint (not a single-issue GET)
    /// has answered so far, this fake's lifetime.
    pub list_issue_requests: u32,
    /// (request number, issue number): right before answering that request
    /// to the issues list, mark that issue `gone` — simulating the filtered
    /// set changing while a multi-page read is under way (spec §3.7). Counts
    /// every list request across every `list()` call, both passes. One-shot.
    pub vanish_after_list_request: Option<(u32, u64)>,
    /// GitHub's timeline lags a write: an event made by a request stays
    /// out of that issue's timeline for this many timeline reads after it.
    /// Measured live: a create's `labeled` events appeared 1.5-3.5 s after
    /// the create was answered. Events made during a timeline read, and by
    /// `web_edit`, are not lagged. A setting, not one-shot.
    pub timeline_lag_reads: u32,
    /// Event id → timeline reads left before it shows.
    pub(crate) lag_left: BTreeMap<u64, u32>,
    /// The same for the body's edit history: an entry made by a request
    /// stays out of it for this many edit-history reads. Measured live: the
    /// history showed an update about 0.5 s after the timeline did.
    pub edit_lag_reads: u32,
    /// Edit id → edit-history reads left before it shows.
    pub(crate) edit_lag_left: BTreeMap<String, u32>,
    /// This issue exists (and a direct `GET` of it succeeds), but every
    /// issues-LIST answer omits it — simulating GitHub's list index lagging
    /// a create indefinitely, so a create-key search can never find it. Not
    /// one-shot: the point is that every one of the search's attempts
    /// misses, not just the first.
    pub omit_from_list: Option<u64>,
    /// The create lands and its answer's status line and headers arrive
    /// (201), but its body breaks off partway, so it cannot be read in
    /// full. One-shot.
    pub broken_create_body_next: bool,
    /// The create lands and is answered 200, not 201, with the issue as its
    /// body. GitHub documents 201; this models a 2xx it has not been seen to
    /// send (unmeasured; no live test checks it yet). One-shot.
    pub create_answers_200_next: bool,
    /// Node ids a `node(id: …)` lookup answers as living in ANOTHER
    /// repository — simulating a transferred issue (spec's `Moved` case).
    /// Modelled: this is the fake's guess at the shape of GitHub's real
    /// answer (unmeasured; no live test checks it yet). Not one-shot.
    pub transferred_nodes: BTreeSet<String>,
    /// On the next timeline read, before answering, someone else adds the
    /// label `bug` to that issue (with its `labeled` event) — a write landing
    /// between fl's first read and its window. One-shot.
    pub foreign_label_on_next_timeline: bool,
    /// The next timeline answer carries this raw item as well — a malformed
    /// event, which GitHub has not been seen to send. One-shot.
    pub odd_timeline_item_next: Option<Value>,
    /// The next edit-history answer carries a `null` node as well. One-shot.
    pub null_edit_node_next: bool,
    /// When set, the edit-history `nodes` show only the OLDEST this many
    /// entries, while `totalCount` stays true — what `last: 100` returns if
    /// GitHub orders the history newest-first (modelled; unmeasured; no
    /// live test checks it yet). Not one-shot.
    pub edit_nodes_cap: Option<usize>,
    /// Inside the next PATCH, someone deletes this many entries from the
    /// issue's body edit history (GitHub lets a person delete a revision).
    /// One-shot.
    pub delete_edits_on_next_patch: usize,
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
                has_issues: true,
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
                let answer = {
                    let mut s = st.lock().unwrap();
                    let first_new = s.next_event;
                    let answer = route(&mut s, &method, &url, &auth, &body);
                    let lag = s.timeline_lag_reads;
                    if lag > 0 && !url.contains("/timeline") {
                        for e in first_new..s.next_event {
                            s.lag_left.insert(e, lag);
                        }
                    }
                    let lag = s.edit_lag_reads;
                    if lag > 0 && url != "/graphql" {
                        for e in first_new..s.next_event {
                            s.edit_lag_left.insert(format!("E_{e}"), lag);
                        }
                    }
                    answer
                };
                if answer.hang_up {
                    // A broken answer after the server acted: the client
                    // fails at once with a transport error. (Dropping the
                    // request unanswered would make tiny_http answer 500;
                    // a short body would hang the client until its timeout.)
                    // ⚠ The explicit `flush` is load-bearing: tiny_http's
                    // writer sits on a `BufWriter`, and without a flush these
                    // few bytes stay in that buffer — never reaching the
                    // socket — until the writer's own `Drop` gets around to
                    // it, which measurably (~30s) loses the race against
                    // ureq's read, and the client blocks on its global
                    // timeout instead of seeing the broken response at once.
                    let mut w = req.into_writer();
                    let _ = std::io::Write::write_all(&mut w, b"HTTP/9 broken\r\n\r\n");
                    let _ = std::io::Write::flush(&mut w);
                    continue;
                }
                if answer.break_body {
                    // A 2xx status line and headers, then a body that breaks
                    // off: one chunk of a chunked body, then a chunk header
                    // that is not a size. The client has read the status by
                    // then, and fails at once on the body. (A `Content-Length`
                    // promising bytes that never come would do the same only
                    // once the socket closed, and tiny_http keeps it open for
                    // the next request, so the client would wait out its
                    // timeout.) The `flush` is load-bearing, as above.
                    let mut w = req.into_writer();
                    let head = format!(
                        "HTTP/1.1 {} Created\r\nContent-Type: application/json\r\n\
                         Transfer-Encoding: chunked\r\n\r\n9\r\n{{\"number\"\r\n\
                         not-a-chunk-size\r\n",
                        answer.status
                    );
                    let _ = std::io::Write::write_all(&mut w, head.as_bytes());
                    let _ = std::io::Write::flush(&mut w);
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

    /// Someone creates a new repository at `name`, which ends its redirect.
    pub fn reuse_name(&self, name: &str) {
        let mut s = self.state();
        s.redirects.remove(&name.to_ascii_lowercase());
        let id = s.repos.len() as u64 + 1;
        s.repos.push(Repo {
            id,
            node_id: format!("R_{id}"),
            full_name: name.into(),
            visibility: "public".into(),
            has_issues: true,
        });
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
    /// Send the status line and headers, then a body that cannot be read.
    break_body: bool,
}

fn answer(status: u16, body: Value) -> Answer {
    Answer {
        status,
        body,
        raw_body: None,
        headers: vec![],
        hang_up: false,
        break_body: false,
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
        break_body: false,
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
            "has_issues": r.has_issues,
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
            if std::mem::take(&mut s.rate_limited_next_create) {
                let mut a = answer(403, json!({"message": "API rate limit exceeded"}));
                a.headers.push(("x-ratelimit-remaining".into(), "0".into()));
                a.headers
                    .push(("x-ratelimit-reset".into(), "1700000000".into()));
                return a;
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
            if std::mem::take(&mut s.unreadable_create_body_next) {
                // The create landed, but the answer a caller actually reads
                // back is garbage — a garbled proxy body, not GitHub's own.
                return raw_answer(201, "not json at all");
            }
            if std::mem::take(&mut s.hang_up_after_create) {
                let mut a = answer(201, Value::Null);
                a.hang_up = true;
                return a;
            }
            if std::mem::take(&mut s.broken_create_body_next) {
                let mut a = answer(201, Value::Null);
                a.break_body = true;
                return a;
            }
            if std::mem::take(&mut s.create_answers_200_next) {
                return answer(200, s.issue_json(&s.issues[&n]));
            }
            answer(201, s.issue_json(&s.issues[&n]))
        }
        ("GET", ["repos", o, r, "issues"]) if s.is_bound(o, r) => {
            s.list_issue_requests += 1;
            if let Some((request, issue)) = s.vanish_after_list_request
                && s.list_issue_requests == request
            {
                s.vanish_after_list_request = None;
                if let Some(i) = s.issues.get_mut(&issue) {
                    i.gone = true;
                }
            }
            let want: Vec<String> = q
                .get("labels")
                .map(|l| l.split(',').map(str::to_string).collect())
                .unwrap_or_default();
            let items = s
                .issues
                .values()
                .filter(|i| !i.gone && i.moved_to.is_none())
                .filter(|i| s.omit_from_list != Some(i.number))
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
        ("GET", ["repos", o, r, "issues", n, "timeline"]) if s.is_bound(o, r) => {
            match n.parse::<u64>().ok().and_then(|n| s.issues.get(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                Some(i) if i.gone => answer(410, json!({"message": "This issue was deleted"})),
                Some(i) if i.moved_to.is_some() => {
                    let mut a = answer(301, json!({"message": "Moved Permanently"}));
                    a.headers
                        .push(("Location".into(), i.moved_to.clone().unwrap_or_default()));
                    a
                }
                Some(_) => {
                    let n: u64 = n.parse().unwrap_or(0);
                    if std::mem::take(&mut s.foreign_label_on_next_timeline) {
                        let e = s.tick();
                        s.labels.insert("bug".into());
                        let i = s.issues.get_mut(&n).unwrap();
                        i.labels.push("bug".into());
                        i.events.push((e, "labeled".into()));
                    }
                    let (issues, lag_left) = (&s.issues, &mut s.lag_left);
                    let mut items: Vec<Value> = Vec::new();
                    for (id, kind) in &issues[&n].events {
                        match lag_left.get_mut(id) {
                            Some(left) if *left > 0 => *left -= 1,
                            _ => items.push(json!({"id": id, "event": kind})),
                        }
                    }
                    if let Some(odd) = s.odd_timeline_item_next.take() {
                        items.push(odd);
                    }
                    s.page(&path, &q, items)
                }
            }
        }
        ("POST", ["repos", o, r, "issues", n, "comments"]) if s.is_bound(o, r) => {
            if std::mem::take(&mut s.fail_comment_next) {
                return answer(500, json!({"message": "fake comment failure"}));
            }
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            match n.parse::<u64>().ok().and_then(|n| s.issues.get_mut(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                Some(i) => {
                    i.comments
                        .push(v["body"].as_str().unwrap_or("").to_string());
                    answer(201, json!({"id": i.comments.len()}))
                }
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
            for _ in 0..std::mem::take(&mut s.delete_edits_on_next_patch) {
                s.issues.get_mut(&n).unwrap().edits.pop();
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
                // ⚠ Modelled: GitHub is taken to record the original body
                // as an entry at the FIRST edit, so a first edit adds two
                // entries. Checked by the live test
                // `the_edit_history_and_timeline_counts_match_fls_model`.
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
            if std::mem::take(&mut s.unreadable_patch_body_next) {
                // The PATCH landed, but the answer a caller reads back is
                // garbage — a garbled proxy body, not GitHub's own.
                return raw_answer(200, "not json at all");
            }
            answer(200, s.issue_json(&s.issues[&n]))
        }
        // A node lookup by id (Task 5): the fake's issues all belong to its
        // one bound repository, so a match is answered under that
        // repository's CURRENT name and node id. An unknown or deleted id
        // answers 200 with `data.node` null AND a NOT_FOUND error — an
        // answer, not a failure (spec's reading of GitHub's GraphQL error
        // shape; unmeasured; no live test checks it yet).
        ("POST", ["graphql"]) => {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            // An issue's body edit history (Task 6), checked FIRST: it is
            // not a node lookup. Answered in the order the fake recorded.
            let query = v["query"].as_str().unwrap_or("");
            if query.contains("userContentEdits") {
                let n = v
                    .pointer("/variables/number")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                // A pull request, or a missing, deleted or transferred issue,
                // answers `issue: null` with a NOT_FOUND error (the
                // reviewer's reading of GitHub; unmeasured; no live test
                // checks it yet).
                let Some(i) = s
                    .issues
                    .get(&n)
                    .filter(|i| !i.pull_request && !i.gone && i.moved_to.is_none())
                else {
                    return answer(
                        200,
                        json!({"data": {"repository": {"issue": null}}, "errors": [{"type": "NOT_FOUND"}]}),
                    );
                };
                let all = i.edits.clone();
                let mut edits = Vec::new();
                for e in all {
                    match s.edit_lag_left.get_mut(&e) {
                        Some(left) if *left > 0 => *left -= 1,
                        _ => edits.push(e),
                    }
                }
                let shown = s.edit_nodes_cap.unwrap_or(usize::MAX);
                let mut nodes: Vec<Value> =
                    edits.iter().take(shown).map(|e| json!({"id": e})).collect();
                let total = edits.len();
                if std::mem::take(&mut s.null_edit_node_next) {
                    nodes.push(Value::Null);
                }
                return answer(
                    200,
                    json!({"data": {"repository": {"issue": {"userContentEdits": {"totalCount": total, "nodes": nodes}}}}}),
                );
            }
            let id = v
                .pointer("/variables/id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let node = s
                .issues
                .values()
                .find(|i| i.node_id == id && !i.gone)
                .map(|i| {
                    if s.transferred_nodes.contains(&i.node_id) {
                        // A transferred issue: answered under a repository
                        // this fake never otherwise models, with a node id
                        // that is never the bound repository's.
                        json!({
                            "url": format!(
                                "https://github.com/elsewhere/transferred/issues/{}",
                                i.number
                            ),
                            "repository": {"id": "R_elsewhere"},
                        })
                    } else {
                        json!({
                            "url": format!("https://github.com/{}/issues/{}", s.bound().full_name, i.number),
                            "repository": {"id": s.bound().node_id},
                        })
                    }
                });
            match node {
                Some(n) => answer(200, json!({"data": {"node": n}})),
                None => answer(
                    200,
                    json!({"data": {"node": null}, "errors": [{"type": "NOT_FOUND"}]}),
                ),
            }
        }
        _ => answer(
            404,
            json!({"message": format!("the fake does not serve {method} {path}")}),
        ),
    }
}
