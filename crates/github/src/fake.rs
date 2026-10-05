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
/// The id of the repository a transferred issue moves to.
pub const TRANSFERRED_REPO: u64 = 99;

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

/// One timeline event: its id, its kind and, for a label event, the label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: u64,
    pub kind: String,
    pub label: Option<String>,
}

impl Event {
    fn new(id: u64, kind: &str, label: Option<&str>) -> Self {
        Self {
            id,
            kind: kind.to_string(),
            label: label.map(str::to_string),
        }
    }
}

/// Where `labeled_copies` puts the second copy of a `labeled` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copies {
    /// Right after the event, in the timeline at once.
    After,
    /// Right before the event, in the timeline at once.
    Before,
    /// Right after the event, but out of the timeline until the issue's
    /// next PATCH — inside that write's window.
    Held,
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
    /// Every timeline event, oldest first.
    pub events: Vec<Event>,
    /// Ids of the body's edit history, oldest first.
    pub edits: Vec<String>,
    pub comments: Vec<String>,
    /// Who wrote each comment, by index. A comment this does not list was
    /// written by [`USER_LOGIN`].
    pub comment_authors: Vec<String>,
    /// When the issue was created, in milliseconds after the Unix epoch:
    /// the fake's clock at the create. GraphQL answers it as `createdAt`,
    /// to the second, as GitHub spells it.
    pub created_ms: u64,
    /// Created while `rest_list_lags` was set: the REST issue list leaves
    /// it out.
    pub rest_list_hidden: bool,
    /// `labeled` events of labels set in the create while
    /// `creation_labels_late` was set: they join `events` at the issue's
    /// next PATCH.
    pub held_events: Vec<Event>,
    /// Copies held back by `labeled_copies: Some(Copies::Held)`: they join
    /// `events`, in id order, at the issue's next PATCH.
    pub held_copies: Vec<Event>,
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
    /// The next GraphQL request answers 200 with `data: null` and one error
    /// of this `type` — neither RATE_LIMITED nor NOT_FOUND, so `graphql`
    /// must still refuse it. One-shot.
    pub graphql_error_next: Option<String>,
    pub fail_repo_read: bool,
    /// The repository read that follows this many more answers 500;
    /// `Some(0)` is the next one. Lets a test pass over the reads a command
    /// makes before the one it means to fail. One-shot.
    pub fail_repo_read_after: Option<u32>,
    pub drop_labels: bool,
    pub fail_after_create: bool,
    /// The create lands, then the connection breaks mid-answer.
    pub hang_up_after_create: bool,
    /// The create answers 502 and nothing lands.
    pub fail_before_create: bool,
    /// The next comment answers 500 and is not posted. One-shot.
    pub fail_comment_next: bool,
    /// The comment post that follows this many more answers 500 and is
    /// not posted; `Some(0)` is the next one. Lets a test pass over the
    /// posts a command makes before the one it means to fail. One-shot.
    pub fail_comment_after: Option<u32>,
    /// The next create that `fail_before_create` does not fail answers 403
    /// with GitHub's rate-limit headers, and nothing lands. One-shot.
    pub rate_limited_next_create: bool,
    /// Inside the next PATCH, someone else adds the label `foreign`, with
    /// its event. One-shot.
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
    /// How many requests for a list of issues — the REST list endpoint or
    /// GraphQL's `issues` connection, not a single-issue GET — the fake has
    /// answered so far, this fake's lifetime.
    pub list_issue_requests: u32,
    /// (request number, issue number): right before answering that request
    /// for a list of issues, mark that issue `gone` — simulating the
    /// filtered set changing while a multi-page read is under way (spec
    /// §3.7). Counts every list request, REST and GraphQL. One-shot.
    pub vanish_after_list_request: Option<(u32, u64)>,
    /// GitHub's REST issue list lags a create. Measured live on 2026-10-05
    /// against a private repository: `GET /repos/o/r/issues?labels=…`
    /// left a new issue out for 31–93 s, once for more than 180 s, while
    /// GraphQL's `issues` connection showed it within 1 s (5 of 5). While
    /// this is set, every issue created is left out of the REST list for
    /// good; GraphQL is not affected. A setting, not one-shot.
    pub rest_list_lags: bool,
    /// The GraphQL `issues` request that follows this many more answers
    /// 502; `Some(0)` is the next one. Lets a test fail a later page of one
    /// list. One-shot.
    pub fail_issues_query_after: Option<u32>,
    /// The next GraphQL `issues` request answers 200 with a `repository`
    /// that holds no `issues` connection. One-shot.
    pub issues_query_without_connection_next: bool,
    /// Every GraphQL `issues` page says another follows, under the cursor
    /// `stuck`, whatever cursor it was asked for. After 20 such pages the
    /// fake answers 502, so a reader that never stops still ends. A
    /// setting.
    pub issues_cursor_stuck: bool,
    /// The `first` each GraphQL `issues` request asked for, in order.
    pub issues_firsts: Vec<Option<u64>>,
    /// GraphQL `issues` pages served under `issues_cursor_stuck`.
    pub(crate) stuck_pages: u32,
    /// The next GraphQL `issues` page is empty and says another follows.
    /// One-shot.
    pub issues_empty_page_with_more_next: bool,
    /// GitHub's clock runs this many milliseconds behind this machine's:
    /// an issue created now is stamped that much earlier. A setting.
    pub clock_behind_ms: u64,
    /// Labels set IN a create (`POST /issues` with `labels`) show their
    /// `labeled` events late. Measured live on 2026-10-05: 28-88 s after
    /// the create, once more than 180 s, in the REST and GraphQL timelines
    /// alike — past fl's wait, so they landed in the next update's window.
    /// Labels added by their own call after the create showed in 1-2 s
    /// (3 of 3). Modelled here as: held back until that issue's next PATCH,
    /// whose window they then land in. A setting, not one-shot.
    pub creation_labels_late: bool,
    /// The next `POST /issues/{n}/labels` answers 500 and adds nothing.
    /// One-shot.
    pub fail_label_add_next: bool,
    /// GitHub records a `labeled` event a second time. Measured live on
    /// 2026-10-05: two labels added in one call doubled each event, about
    /// 0-1 s apart, in 4 of 10 calls; one label per call did so in 0 of 22
    /// plain `gh api` probes but in 2 of 33 issues fl created in a live
    /// run, so the call pattern does not control it. Re-adding a label the
    /// issue already carries makes no event at all. While set, every
    /// `labeled` event a label add or a PATCH makes gets a copy, placed as
    /// `Copies` says. A setting.
    pub labeled_copies: Option<Copies>,
    /// Right after the next timeline read is answered, someone adds the
    /// label `bug` to that issue, with its event — a write landing after
    /// fl's window opened and before fl reads the issue inside it.
    /// One-shot.
    pub foreign_label_after_next_timeline: bool,
    /// Inside the next PATCH, someone else makes these label changes —
    /// (`labeled` or `unlabeled`, label) — each with its event. One-shot.
    pub foreign_label_changes_on_next_patch: Vec<(String, String)>,
    /// GitHub's timeline lags a write: an event made by a request stays
    /// out of that issue's timeline for this many timeline reads after it.
    /// Measured live on 2026-10-05: the `labeled` events of labels added
    /// to a new issue by their own call appeared 1-2 s after it (labels set
    /// in the create itself: see `creation_labels_late`). Events made
    /// during a timeline read, and by `web_edit`, are not lagged. A
    /// setting, not one-shot.
    pub timeline_lag_reads: u32,
    /// Event id → timeline reads left before it shows.
    pub(crate) lag_left: BTreeMap<u64, u32>,
    /// The next timeline read answers 502. One-shot.
    pub fail_next_timeline: bool,
    /// The same for the body's edit history: an entry made by a request
    /// stays out of it for this many edit-history reads. Measured live: an
    /// update's entries showed about 0.5 s after its timeline events did.
    pub edit_lag_reads: u32,
    /// Edit id → edit-history reads left before it shows.
    pub(crate) edit_lag_left: BTreeMap<String, u32>,
    /// This issue exists (and a direct `GET` of it succeeds), but every
    /// list of issues — REST and GraphQL — omits it, so a create-key search
    /// can never find it. Not one-shot: the point is that every one of the
    /// search's attempts misses, not just the first.
    pub omit_from_list: Option<u64>,
    /// The create lands and its answer's status line and headers arrive
    /// (201), but its body breaks off partway, so it cannot be read in
    /// full. One-shot.
    pub broken_create_body_next: bool,
    /// The create lands and is answered 200, not 201, with the issue as its
    /// body. GitHub documents 201; this models a 2xx it has not been seen to
    /// send (unmeasured; no live test checks it yet). One-shot.
    pub create_answers_200_next: bool,
    /// The next create lands with this text appended to its body, and
    /// answers it so — a body GitHub kept differently from what was sent.
    /// One-shot.
    pub create_body_appended_next: Option<String>,
    /// Labels every new issue gets as it is created, whatever the create
    /// sent — an automation labelling issues on open. The create's answer
    /// carries them. A setting.
    pub labels_on_open: Vec<String>,
    /// Node ids a `node(id: …)` lookup answers as living in ANOTHER
    /// repository — simulating a transferred issue (spec's `Moved` case).
    /// Modelled: this is the fake's guess at the shape of GitHub's real
    /// answer (unmeasured; no live test checks it yet). Not one-shot.
    pub transferred_nodes: BTreeSet<String>,
    /// Issues transferred out of the bound repository, by number, as GitHub
    /// serves them where they are now:
    /// `/repositories/{TRANSFERRED_REPO}/issues/{n}`.
    pub transferred: BTreeMap<u64, Issue>,
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
    /// The next request answers with GitHub's secondary rate limit: this
    /// status (403 or 429), a `retry-after` of this many seconds when set,
    /// and the primary budget untouched (`x-ratelimit-remaining: 4999`).
    /// Modelled from GitHub's documentation; not measured. One-shot.
    pub secondary_rate_limit_next: Option<(u16, Option<u64>)>,
    /// The next request answers 403 for want of a permission — not a rate
    /// limit. One-shot.
    pub forbidden_next: bool,
    /// The repository's git objects and refs (GitHub ledger spec §8.1).
    pub git: crate::fake_git::Git,
    /// Rulesets; only `active` ones apply. A setting, not one-shot.
    pub rulesets: Vec<crate::fake_git::Ruleset>,
    /// `rules/branches` answers 403 with an "Upgrade to GitHub" message.
    /// ⚠ Defensive only: no live answer has shown it. Measured on
    /// 2026-10-02, a private repository on GitHub Free answers `200 []` —
    /// the fake's default, with no ruleset — confirmed by live test
    /// `a_private_repository_without_a_ruleset_is_detection_only`.
    pub rules_need_upgrade: bool,
    /// Every request breaks off before an answer: GitHub unreachable. A
    /// setting, not one-shot.
    pub down: bool,
    /// The next this-many compares answer `behind` whatever the commits —
    /// a replica lagging a write (spec §3.5 check 2).
    pub compare_behind_next: u32,
    /// The next ref creation answers 500 and creates nothing. One-shot.
    pub fail_next_ref_create: bool,
    /// Right before the next ref creation, someone else creates the same
    /// branch. One-shot.
    pub race_next_ref_create: bool,
    /// The next request answers 403 naming this permission in
    /// `x-accepted-github-permissions`. One-shot.
    pub permission_refused_next: Option<String>,
    /// The next this-many branch reads answer the commit before the head
    /// (its first parent) — a replica that has not seen the last write yet
    /// (spec §3.5 check 2).
    pub ref_behind_next: u32,
    /// The next this-many compares answer 404 — a replica that does not
    /// know a commit yet, such as the one this machine just wrote.
    pub compare_unknown_next: u32,
    /// The repository answer names no `visibility`. A setting.
    pub omit_visibility: bool,
    /// The next tree or commit creation answers 500 and creates nothing.
    /// One-shot.
    pub fail_next_git_create: bool,
    /// The next read of a branch's rules answers 500. One-shot.
    pub fail_rules_next: bool,
    /// Before each ledger commit is judged, someone else appends this line
    /// to this path — another machine landing first. Consumed one per
    /// commit, oldest first.
    pub foreign_appends: Vec<(String, String)>,
    /// The next this-many commits that would land answer 502 instead (a
    /// stale commit, refused before this is consulted, does not consume
    /// it).
    pub fail_commits: u32,
    /// The next ledger commit lands, then its answer breaks off. One-shot.
    pub hang_up_after_next_commit: bool,
    /// The next ledger commit answers 403 naming this permission. One-shot.
    pub refuse_next_commit_for: Option<String>,
    /// The next ledger commit answers GraphQL's RATE_LIMITED. One-shot.
    pub rate_limit_next_commit: bool,
    /// The next ledger commit answers 200 with a GraphQL error of type
    /// FORBIDDEN, rather than an HTTP 403 naming a permission — a second
    /// shape live GraphQL can use to refuse a missing permission. One-shot.
    /// ⚠ Modelled — confirmed by live test
    /// `create_commit_on_branch_without_contents_write_is_refused`.
    pub refuse_next_commit_as_forbidden: bool,
    /// The next ledger commit lands, then answers 200 with a body that is
    /// not JSON. One-shot.
    pub garble_next_commit_answer: bool,
    /// The next ledger commit lands, then answers 200 with a body that
    /// breaks off. One-shot.
    pub break_next_commit_answer: bool,
    /// A queue of overrides: the first request whose path contains a
    /// queued fragment answers with that entry's status and body instead
    /// of its normal handling — a malformed GitHub answer none of the
    /// fake's real routes produce on their own (a 200 missing a field it
    /// always sends, a non-`Blob`/`Tree` `__typename`). Each entry is
    /// one-shot once matched (removed from the queue); a request that
    /// matches none of them falls through to normal handling. Usually
    /// holds at most one entry, but a test that needs to override TWO
    /// distinct requests (two different blob ids, say) within one call can
    /// queue both up front.
    pub body_next: Vec<(String, u16, Value)>,
    /// The next this-many `ledgerObjects` reads answer as if GraphQL does
    /// not yet know the newest commit at all: `head` and every `eN` object
    /// come back null, even when the commit and paths are real — a GraphQL
    /// replica that has not caught up with a write this machine (or
    /// another) just made (spec §3.5 / ruling 24).
    pub graphql_commit_unknown_next: u32,
    /// The next this-many branch-ref reads answer 404 outright, as if the
    /// branch does not exist yet — distinct from `ref_behind_next` (which
    /// answers an older commit): a replica that has not learned of the
    /// branch at all yet (ruling 24).
    pub ref_404_next: u32,
    /// Every recursive tree listing says GitHub cut it short. A setting.
    pub truncate_trees: bool,
    /// Every git data request answers 409, as for a repository with no
    /// commit. A setting.
    pub empty_repository: bool,
    /// The next commit lands, and its answer is GitHub's timeout error: a
    /// 200 with no data and an error saying it may be a timeout. One-shot.
    pub timeout_after_next_commit: bool,
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

    /// A person changing an issue in the web interface, outside fl. Its
    /// timeline gets the events the change makes: one per label added or
    /// removed, a close or a reopen, a retitle.
    pub fn web_edit(&self, n: u64, f: impl FnOnce(&mut Issue)) {
        let mut s = self.state();
        let old = s.issues.get(&n).expect("an issue to edit").clone();
        let issue = s.issues.get_mut(&n).unwrap();
        f(issue);
        let new = issue.clone();
        for (kind, label) in changes(&old, &new) {
            let e = s.tick();
            s.issues
                .get_mut(&n)
                .unwrap()
                .events
                .push(Event::new(e, kind, label.as_deref()));
        }
    }

    /// Transfers issue `n` to another repository, as GitHub does: the old
    /// address answers `301` to where the issue is now, and the issue —
    /// its body and its comments so far — is served there. Returns that
    /// address.
    pub fn transfer(&self, n: u64) -> String {
        let mut s = self.state();
        let to = format!("{}/repositories/{TRANSFERRED_REPO}/issues/{n}", s.base);
        let issue = s.issues.get_mut(&n).expect("an issue to transfer");
        let moved = issue.clone();
        issue.moved_to = Some(to.clone());
        s.transferred.insert(n, moved);
        to
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
            created_ms: now_ms(),
            ..Issue::default()
        };
        s.issues.insert(n, issue);
        n
    }

    /// `fl/ledger` started as `fl github ledger init` starts it — `format`
    /// and `README.md` in a commit with no parent. Returns that commit.
    pub fn seed_ledger(&self) -> String {
        use crate::ledger::layout::{FORMAT_FILE, README, README_FILE};
        self.seed_ledger_with(&[(FORMAT_FILE, "1\n"), (README_FILE, README)])
    }

    /// `fl/ledger` started with `files` in its first commit.
    pub fn seed_ledger_with(&self, files: &[(&str, &str)]) -> String {
        crate::fake_git::seed(&mut self.state().git, files)
    }

    pub fn ledger_head(&self) -> Option<String> {
        self.state().git.head(crate::ledger::layout::BRANCH)
    }

    /// Every file on `fl/ledger` at its head; empty when there is no branch.
    pub fn ledger_files(&self) -> BTreeMap<String, String> {
        let s = self.state();
        s.git
            .head(crate::ledger::layout::BRANCH)
            .and_then(|h| s.git.files_at(&h))
            .unwrap_or_default()
    }

    /// A person with write access committing to `fl/ledger` by hand: each
    /// path written (`Some`) or removed (`None`).
    pub fn hand_commit(&self, changes: &[(&str, Option<&str>)]) -> String {
        let changes: Vec<(String, Option<String>)> = changes
            .iter()
            .map(|(p, t)| (p.to_string(), t.map(str::to_string)))
            .collect();
        self.state()
            .git
            .commit_on(crate::ledger::layout::BRANCH, &changes, "a hand edit")
    }

    /// A person merging a side commit into `fl/ledger`: two parents.
    pub fn hand_merge(&self) -> String {
        let branch = crate::ledger::layout::BRANCH;
        let mut s = self.state();
        let git = &mut s.git;
        let head = git.head(branch).expect("a ledger to merge into");
        let tree = git.commits[&head].tree.clone();
        let side = git.put_commit(&tree, vec![head.clone()], "a side commit");
        let merge = git.put_commit(&tree, vec![head, side], "a merge");
        git.refs.insert(format!("heads/{branch}"), merge.clone());
        merge
    }

    /// A person rewriting `fl/ledger`: a new first commit holding `files`,
    /// forced onto the branch.
    pub fn rewrite_ledger(&self, files: &[(&str, &str)]) -> String {
        crate::fake_git::seed(&mut self.state().git, files)
    }

    pub fn delete_ledger(&self) {
        let branch = crate::ledger::layout::BRANCH;
        self.state().git.refs.remove(&format!("heads/{branch}"));
    }

    /// How many commits `fl/ledger` holds along first parents.
    pub fn ledger_commits(&self) -> usize {
        self.state()
            .git
            .first_parents(crate::ledger::layout::BRANCH)
            .len()
    }

    /// Every commit message `fl/ledger` holds along first parents — the
    /// headline each append or hand edit used, so a test can scan commit
    /// messages for anything machine-specific as well as file contents.
    pub fn ledger_commit_messages(&self) -> Vec<String> {
        let s = self.state();
        s.git
            .first_parents(crate::ledger::layout::BRANCH)
            .iter()
            .filter_map(|id| s.git.commits.get(id).map(|c| c.message.clone()))
            .collect()
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
    pub(crate) status: u16,
    pub(crate) body: Value,
    /// When set, this exact text is sent instead of `body.to_string()` — for
    /// answers that are not JSON at all (an HTML 502 from a load balancer).
    pub(crate) raw_body: Option<String>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) hang_up: bool,
    /// Send the status line and headers, then a body that cannot be read.
    pub(crate) break_body: bool,
}

pub(crate) fn answer(status: u16, body: Value) -> Answer {
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
    pub(crate) fn is_bound(&self, o: &str, r: &str) -> bool {
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
        let mut v = json!({
            "id": r.id, "node_id": r.node_id, "full_name": r.full_name,
            "visibility": r.visibility, "private": r.visibility == "private",
            "has_issues": r.has_issues,
        });
        if self.omit_visibility
            && let Some(map) = v.as_object_mut()
        {
            map.remove("visibility");
        }
        v
    }

    /// One page of `items`, with a `Link` header when more remain.
    pub(crate) fn page(
        &self,
        path: &str,
        q: &BTreeMap<String, String>,
        items: Vec<Value>,
    ) -> Answer {
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

/// A `301` to `to`, as GitHub answers for an issue that moved.
fn redirect(to: &str) -> Answer {
    let mut a = answer(301, json!({"message": "Moved Permanently"}));
    a.headers.push(("Location".into(), to.to_string()));
    a
}

/// An issue's comments as GitHub lists them, oldest first, each with its
/// author.
fn comment_items(i: &Issue) -> Vec<Value> {
    i.comments
        .iter()
        .enumerate()
        .map(|(k, body)| {
            let by = i.comment_authors.get(k).map_or(USER_LOGIN, String::as_str);
            json!({"id": k as u64 + 1, "body": body, "user": {"login": by}})
        })
        .collect()
}

/// Who a request's credential writes as: the App's bot for its
/// installation token, else the token's user.
///
/// Unmeasured for the App: that an installation token's comment is
/// authored by `{slug}[bot]`, the login `GET /app` names, is GitHub's
/// documentation; the live run as the App is still owed.
fn author(auth: &str) -> String {
    if auth == format!("Bearer {INSTALLATION_TOKEN}") {
        format!("{APP_SLUG}[bot]")
    } else {
        USER_LOGIN.to_string()
    }
}

/// Appends the comment a `POST` carries in `body` to `i`, written by `by`,
/// and answers as GitHub does for a comment it took.
fn add_comment(i: &mut Issue, body: &str, by: String) -> Answer {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let k = i.comments.len();
    i.comment_authors.resize(k, USER_LOGIN.to_string());
    i.comment_authors.push(by);
    i.comments
        .push(v["body"].as_str().unwrap_or("").to_string());
    answer(201, json!({"id": i.comments.len()}))
}

/// The fake's clock: milliseconds after the Unix epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `createdAt` as GitHub's GraphQL spells it: RFC 3339, UTC, to the second.
fn created_at(ms: u64) -> String {
    let at = fl_core::At::from_unix_millis(ms);
    format!("{}Z", &at.as_str()[..19])
}

/// Where an issue sorts by creation, and what a cursor names: the fake's
/// cursor is `{created_ms}:{number}`, a position, never an offset.
fn sort_key(i: &Issue) -> (u64, u64) {
    (i.created_ms, i.number)
}

/// One page of GraphQL's `repository.issues` connection (spec §3.7): label
/// filter, `orderBy: {field: CREATED_AT, direction: $direction}`, and
/// `after: $cursor` with a real `pageInfo`. The page size is `first`, at
/// most `max_per_page` (0 = 100).
///
/// ⚠ Modelled: the connection lists issues only, never a pull request
/// (GitHub's schema keeps those in `pullRequests`); its cursor names the
/// last issue served, so an issue leaving the filtered set cannot shift
/// another across a page boundary. Unmeasured; no live test checks the
/// cursor yet.
/// Its `labels` filter matches an issue carrying ANY of the labels given:
/// measured live on 2026-10-05, OR, the union (`fl:record` 73 issues,
/// `fl:finding` 70, both 143). fl filters by one label.
fn issues_page(s: &mut State, vars: &Value) -> Answer {
    s.list_issue_requests += 1;
    let first = vars["first"].as_u64();
    s.issues_firsts.push(first);
    if let Some((request, issue)) = s.vanish_after_list_request
        && s.list_issue_requests == request
    {
        s.vanish_after_list_request = None;
        if let Some(i) = s.issues.get_mut(&issue) {
            i.gone = true;
        }
    }
    match s.fail_issues_query_after {
        Some(0) => {
            s.fail_issues_query_after = None;
            return answer(502, json!({"message": "fake issues page failure"}));
        }
        Some(n) => s.fail_issues_query_after = Some(n - 1),
        None => {}
    }
    if std::mem::take(&mut s.issues_query_without_connection_next) {
        return answer(200, json!({"data": {"repository": {}}}));
    }
    if std::mem::take(&mut s.issues_empty_page_with_more_next) {
        return answer(
            200,
            json!({"data": {"repository": {"issues": {
                "pageInfo": {"hasNextPage": true, "endCursor": "0:0"},
                "nodes": [],
            }}}}),
        );
    }
    if s.issues_cursor_stuck {
        s.stuck_pages += 1;
        if s.stuck_pages > 20 {
            return answer(502, json!({"message": "fake: a reader that never stops"}));
        }
    }
    let known = match (vars["owner"].as_str(), vars["name"].as_str()) {
        (Some(o), Some(n)) => s.is_bound(o, n),
        _ => false,
    };
    if !known {
        return answer(
            200,
            json!({"data": {"repository": null}, "errors": [{"type": "NOT_FOUND"}]}),
        );
    }
    let want = str_list(&vars["labels"]);
    let newest_first = vars["direction"].as_str() == Some("DESC");
    let after = vars["after"].as_str().map(|c| {
        let (ms, n) = c.split_once(':').unwrap_or(("0", "0"));
        (ms.parse().unwrap_or(0), n.parse().unwrap_or(0))
    });
    let mut items: Vec<&Issue> = s
        .issues
        .values()
        .filter(|i| !i.gone && i.moved_to.is_none() && !i.pull_request)
        .filter(|i| s.omit_from_list != Some(i.number))
        .filter(|i| want.is_empty() || want.iter().any(|w| i.labels.contains(w)))
        .collect();
    items.sort_by_key(|i| sort_key(i));
    if newest_first {
        items.reverse();
    }
    if let Some(cursor) = after {
        items.retain(|i| {
            if newest_first {
                sort_key(i) < cursor
            } else {
                sort_key(i) > cursor
            }
        });
    }
    let cap = if s.max_per_page == 0 {
        100
    } else {
        s.max_per_page
    };
    let size = first.map_or(cap, |f| (f as usize).min(cap)).max(1);
    let page: Vec<&Issue> = items.iter().take(size).copied().collect();
    let end = page
        .last()
        .map(|i| format!("{}:{}", i.created_ms, i.number));
    let nodes: Vec<Value> = page
        .iter()
        .map(|i| {
            json!({
                "number": i.number,
                "id": i.node_id,
                "url": format!("https://github.com/{}/issues/{}", s.bound().full_name, i.number),
                "title": i.title,
                "body": i.body,
                "state": i.state.to_ascii_uppercase(),
                "stateReason": i.state_reason.as_deref().map(str::to_ascii_uppercase),
                "createdAt": created_at(i.created_ms),
                "labels": {
                    "totalCount": i.labels.len(),
                    "nodes": i.labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
                },
            })
        })
        .collect();
    let (more, end) = if s.issues_cursor_stuck {
        (true, Some("stuck".to_string()))
    } else {
        (items.len() > size, end)
    };
    answer(
        200,
        json!({"data": {"repository": {"issues": {
            "pageInfo": {"hasNextPage": more, "endCursor": end},
            "nodes": nodes,
        }}}}),
    )
}

/// The timeline events a change from `old` to `new` makes, in order: each
/// label added, each removed, a close or reopen, a retitle.
fn changes(old: &Issue, new: &Issue) -> Vec<(&'static str, Option<String>)> {
    let mut out: Vec<(&'static str, Option<String>)> = Vec::new();
    for l in new.labels.iter().filter(|l| !old.labels.contains(l)) {
        out.push(("labeled", Some(l.clone())));
    }
    for l in old.labels.iter().filter(|l| !new.labels.contains(l)) {
        out.push(("unlabeled", Some(l.clone())));
    }
    if old.state != new.state {
        out.push((
            if new.state == "closed" {
                "closed"
            } else {
                "reopened"
            },
            None,
        ));
    }
    if old.title != new.title {
        out.push(("renamed", None));
    }
    out
}

impl State {
    /// Records an event a request made on issue `n`. A `labeled` one gets
    /// its copy under `labeled_copies`.
    fn record(&mut self, n: u64, kind: &str, label: Option<&str>) {
        let copies = if kind == "labeled" {
            self.labeled_copies
        } else {
            None
        };
        if copies == Some(Copies::Before) {
            let c = self.tick();
            self.issues
                .get_mut(&n)
                .unwrap()
                .events
                .push(Event::new(c, kind, label));
        }
        let e = self.tick();
        self.issues
            .get_mut(&n)
            .unwrap()
            .events
            .push(Event::new(e, kind, label));
        match copies {
            Some(Copies::After) => {
                let c = self.tick();
                self.issues
                    .get_mut(&n)
                    .unwrap()
                    .events
                    .push(Event::new(c, kind, label));
            }
            Some(Copies::Held) => {
                let c = self.tick();
                self.issues
                    .get_mut(&n)
                    .unwrap()
                    .held_copies
                    .push(Event::new(c, kind, label));
            }
            Some(Copies::Before) | None => {}
        }
    }
}

/// Every route the fake serves. Later tasks add arms above the final `_`.
pub(crate) fn route(s: &mut State, method: &str, url: &str, auth: &str, body: &str) -> Answer {
    s.requests.push(format!("{method} {url}"));
    if s.down {
        let mut a = answer(503, Value::Null);
        a.hang_up = true;
        return a;
    }
    if let Some(i) = s
        .body_next
        .iter()
        .position(|(frag, _, _)| url.contains(frag.as_str()))
    {
        let (_, status, body) = s.body_next.remove(i);
        return answer(status, body);
    }
    if let Some(needs) = s.permission_refused_next.take() {
        let mut a = answer(
            403,
            json!({"message": "Resource not accessible by personal access token"}),
        );
        a.headers
            .push(("x-accepted-github-permissions".into(), needs));
        return a;
    }
    if method == "POST" && url == "/graphql" && std::mem::take(&mut s.graphql_rate_limited) {
        return answer(
            200,
            json!({"data": null, "errors": [{"type": "RATE_LIMITED"}]}),
        );
    }
    if method == "POST"
        && url == "/graphql"
        && let Some(t) = s.graphql_error_next.take()
    {
        return answer(200, json!({"data": null, "errors": [{"type": t}]}));
    }
    if let Some((status, retry_after)) = s.secondary_rate_limit_next.take() {
        let mut a = answer(
            status,
            json!({"message": "You have exceeded a secondary rate limit. Please wait a few \
                   minutes before you try again. If you reach out to GitHub Support for help, \
                   please include the request ID 0000:0000:0000000:0000000:00000000."}),
        );
        a.headers
            .push(("x-ratelimit-remaining".into(), "4999".into()));
        a.headers
            .push(("x-ratelimit-reset".into(), "1700000000".into()));
        if let Some(secs) = retry_after {
            a.headers.push(("retry-after".into(), secs.to_string()));
        }
        // ⚠ Modelled: `x-accepted-github-permissions` can ride along on ANY
        // 403, not only a true missing-permission refusal. GitHub ledger
        // spec §1.6 / §6.3; unmeasured — no live test provokes either.
        a.headers.push((
            "x-accepted-github-permissions".into(),
            "contents=write".into(),
        ));
        return a;
    }
    if std::mem::take(&mut s.forbidden_next) {
        let mut a = answer(
            403,
            json!({"message": "Resource not accessible by personal access token"}),
        );
        a.headers
            .push(("x-ratelimit-remaining".into(), "4999".into()));
        return a;
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
    if let Some(a) = crate::fake_git::rest(s, method, &parts, &q, body) {
        return a;
    }
    match (method, parts.as_slice()) {
        ("GET", ["repos", o, r]) => {
            if s.fail_repo_read {
                s.fail_repo_read = false;
                return answer(500, json!({"message": "fake repository failure"}));
            }
            match s.fail_repo_read_after {
                Some(0) => {
                    s.fail_repo_read_after = None;
                    return answer(500, json!({"message": "fake repository failure"}));
                }
                Some(n) => s.fail_repo_read_after = Some(n - 1),
                None => {}
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
            labels.extend(s.labels_on_open.iter().cloned());
            let mut events = Vec::new();
            for l in &labels {
                s.labels.insert(l.clone());
                events.push(Event::new(s.tick(), "labeled", Some(l)));
            }
            let held_events = if s.creation_labels_late {
                std::mem::take(&mut events)
            } else {
                Vec::new()
            };
            let issue = Issue {
                number: n,
                node_id: format!("I_{n}"),
                title: v["title"].as_str().unwrap_or("").into(),
                body: format!(
                    "{}{}",
                    v["body"].as_str().unwrap_or(""),
                    s.create_body_appended_next.take().unwrap_or_default()
                ),
                labels,
                state: "open".into(),
                events,
                created_ms: now_ms().saturating_sub(s.clock_behind_ms),
                rest_list_hidden: s.rest_list_lags,
                held_events,
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
                .filter(|i| !i.rest_list_hidden)
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
                    if std::mem::take(&mut s.fail_next_timeline) {
                        return answer(502, json!({"message": "fake timeline failure"}));
                    }
                    let n: u64 = n.parse().unwrap_or(0);
                    if std::mem::take(&mut s.foreign_label_on_next_timeline) {
                        let e = s.tick();
                        s.labels.insert("bug".into());
                        let i = s.issues.get_mut(&n).unwrap();
                        i.labels.push("bug".into());
                        i.events.push(Event::new(e, "labeled", Some("bug")));
                    }
                    let (issues, lag_left) = (&s.issues, &mut s.lag_left);
                    let mut items: Vec<Value> = Vec::new();
                    for ev in &issues[&n].events {
                        match lag_left.get_mut(&ev.id) {
                            Some(left) if *left > 0 => *left -= 1,
                            _ => {
                                let mut item = json!({"id": ev.id, "event": ev.kind});
                                if let Some(l) = &ev.label {
                                    item["label"] = json!({"name": l});
                                }
                                items.push(item);
                            }
                        }
                    }
                    if let Some(odd) = s.odd_timeline_item_next.take() {
                        items.push(odd);
                    }
                    let page = s.page(&path, &q, items);
                    if std::mem::take(&mut s.foreign_label_after_next_timeline) {
                        let e = s.tick();
                        s.labels.insert("bug".into());
                        let i = s.issues.get_mut(&n).unwrap();
                        i.labels.push("bug".into());
                        i.events.push(Event::new(e, "labeled", Some("bug")));
                    }
                    page
                }
            }
        }
        // Adds labels to an issue and answers every label it now carries.
        // `drop_labels` makes GitHub keep the old ones silently, as for a
        // PATCH (a reading of the docs for this call; unmeasured).
        ("POST", ["repos", o, r, "issues", n, "labels"]) if s.is_bound(o, r) => {
            if std::mem::take(&mut s.fail_label_add_next) {
                return answer(500, json!({"message": "fake label failure"}));
            }
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let Some(n) = n.parse::<u64>().ok().filter(|n| s.issues.contains_key(n)) else {
                return answer(404, json!({"message": "Not Found"}));
            };
            if s.issues[&n].gone {
                return answer(410, json!({"message": "This issue was deleted"}));
            }
            let mut added = Vec::new();
            if !s.drop_labels {
                for l in str_list(&v["labels"]) {
                    if !s.issues[&n].labels.contains(&l) {
                        added.push(l);
                    }
                }
            }
            for l in added {
                s.labels.insert(l.clone());
                s.issues.get_mut(&n).unwrap().labels.push(l.clone());
                s.record(n, "labeled", Some(&l));
            }
            let names: Vec<Value> = s.issues[&n]
                .labels
                .iter()
                .map(|l| json!({"name": l}))
                .collect();
            answer(200, Value::Array(names))
        }
        ("POST", ["repos", o, r, "issues", n, "comments"]) if s.is_bound(o, r) => {
            if std::mem::take(&mut s.fail_comment_next) {
                return answer(500, json!({"message": "fake comment failure"}));
            }
            match s.fail_comment_after {
                Some(0) => {
                    s.fail_comment_after = None;
                    return answer(500, json!({"message": "fake comment failure"}));
                }
                Some(n) => s.fail_comment_after = Some(n - 1),
                None => {}
            }
            let by = author(auth);
            match n.parse::<u64>().ok().and_then(|n| s.issues.get_mut(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                // Unmeasured: GitHub's answers for a deleted or transferred
                // issue's comments, as its documentation describes them; no
                // live test deletes or transfers an issue.
                Some(i) if i.gone => answer(410, json!({"message": "This issue was deleted"})),
                Some(i) if i.moved_to.is_some() => redirect(i.moved_to.as_deref().unwrap_or("")),
                Some(i) => add_comment(i, body, by),
            }
        }
        // ⚠ Modelled: an issue's comments are listed with each one's `body`
        // and its author's `user.login`. Confirmed by live test
        // `a_decision_comment_round_trips_with_its_marker`, which reads one
        // page; paging is the `Link` header every list GitHub answers uses.
        ("GET", ["repos", o, r, "issues", n, "comments"]) if s.is_bound(o, r) => {
            let items = match n.parse::<u64>().ok().and_then(|n| s.issues.get(&n)) {
                None => return answer(404, json!({"message": "Not Found"})),
                Some(i) if i.gone => {
                    return answer(410, json!({"message": "This issue was deleted"}));
                }
                Some(i) if i.moved_to.is_some() => {
                    return redirect(i.moved_to.as_deref().unwrap_or(""));
                }
                Some(i) => comment_items(i),
            };
            s.page(&path, &q, items)
        }
        // Unmeasured: a transferred issue, its comments and a post there,
        // as GitHub's documentation describes them; no live test transfers
        // an issue.
        ("GET", ["repositories", id, "issues", n])
            if id.parse::<u64>().ok() == Some(TRANSFERRED_REPO) =>
        {
            match n.parse::<u64>().ok().and_then(|n| s.transferred.get(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                Some(i) => {
                    let mut v = s.issue_json(i);
                    v["html_url"] = json!(format!(
                        "https://github.com/elsewhere/transferred/issues/{}",
                        i.number
                    ));
                    answer(200, v)
                }
            }
        }
        ("GET", ["repositories", id, "issues", n, "comments"])
            if id.parse::<u64>().ok() == Some(TRANSFERRED_REPO) =>
        {
            let items = match n.parse::<u64>().ok().and_then(|n| s.transferred.get(&n)) {
                None => return answer(404, json!({"message": "Not Found"})),
                Some(i) => comment_items(i),
            };
            s.page(&path, &q, items)
        }
        ("POST", ["repositories", id, "issues", n, "comments"])
            if id.parse::<u64>().ok() == Some(TRANSFERRED_REPO) =>
        {
            let by = author(auth);
            match n
                .parse::<u64>()
                .ok()
                .and_then(|n| s.transferred.get_mut(&n))
            {
                None => answer(404, json!({"message": "Not Found"})),
                Some(i) => add_comment(i, body, by),
            }
        }
        ("PATCH", ["repos", o, r, "issues", n]) if s.is_bound(o, r) => {
            let Some(n) = n.parse::<u64>().ok().filter(|n| s.issues.contains_key(n)) else {
                return answer(404, json!({"message": "Not Found"}));
            };
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            // A create's late `labeled` events, and held copies, land now,
            // inside this write's window (`creation_labels_late`,
            // `labeled_copies`), in id order.
            let i = s.issues.get_mut(&n).unwrap();
            let held = std::mem::take(&mut i.held_events);
            i.events.extend(held);
            let copies = std::mem::take(&mut i.held_copies);
            i.events.extend(copies);
            i.events.sort_by_key(|e| e.id);
            // A write by someone else that lands inside fl's window: the
            // label `foreign` added.
            if std::mem::take(&mut s.foreign_label_on_next_patch) {
                s.foreign_label_changes_on_next_patch
                    .push(("labeled".into(), "foreign".into()));
            }
            for (kind, label) in std::mem::take(&mut s.foreign_label_changes_on_next_patch) {
                s.labels.insert(label.clone());
                let i = s.issues.get_mut(&n).unwrap();
                let carried = i.labels.contains(&label);
                // As GitHub: adding a label the issue carries, or removing
                // one it does not, changes nothing and records no event.
                if (kind == "labeled") == carried {
                    continue;
                }
                if kind == "labeled" {
                    i.labels.push(label.clone());
                } else {
                    i.labels.retain(|l| *l != label);
                }
                let e = s.tick();
                s.issues
                    .get_mut(&n)
                    .unwrap()
                    .events
                    .push(Event::new(e, &kind, Some(&label)));
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
            let made = changes(&old, &new);
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
            for (kind, label) in made {
                s.record(n, kind, label.as_deref());
            }
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
            if let Some(a) = crate::fake_git::graphql(s, &v) {
                return a;
            }
            // An issue's body edit history (Task 6), checked FIRST: it is
            // not a node lookup. Answered in the order the fake recorded.
            let query = v["query"].as_str().unwrap_or("");
            if query.contains("userContentEdits") {
                let n = v
                    .pointer("/variables/number")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                // A pull request, or a missing, deleted or transferred issue,
                // answers `issue: null` with a NOT_FOUND error (a reading
                // of GitHub's docs; unmeasured; no live test checks it
                // yet).
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
            if query.contains("issues(") {
                return issues_page(s, &v["variables"]);
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
