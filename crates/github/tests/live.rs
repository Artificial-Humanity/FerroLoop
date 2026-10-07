//! Against GitHub itself (GitHub tracker spec §8.3; GitHub ledger spec
//! §8.4; routing spec §5, the escalation). Ignored by default.
//!
//! Run only against THROWAWAY repositories. The tracker's tests and the
//! escalation's create issues and never delete them. The ledger's append
//! to `fl/ledger`, leave a branch `fl-live/root` at its first commit, and
//! delete nothing: a ledger under a ruleset cannot be deleted, so every
//! test is safe to run again on what earlier runs left.
//!
//! - `FL_GITHUB_LIVE_REPO`: a private repository (the tracker's tests, the
//!   escalation's, and most of the ledger's) holding at least one commit:
//!   `init` refuses an empty repository.
//! - `FL_GITHUB_LIVE_PUBLIC_REPO`: a public repository holding only test
//!   data and one commit, with an active ruleset on `refs/heads/fl/ledger`
//!   holding `non_fast_forward` and `deletion` (`fl github ledger init`
//!   prints the command that adds it), which the credential cannot bypass:
//!   the force-update test reads the ruleset and refuses to write unless
//!   GitHub says the credential's bypass is `never`.
//! - `FL_GITHUB_LIVE_EMPTY_REPO`: a private repository with no commit at
//!   all.
//! - `FL_GITHUB_LIVE_READ_ONLY_TOKEN`: a fine-grained token on
//!   `FL_GITHUB_LIVE_REPO` only, with Contents: read and Metadata: read.
//!
//! A ledger test whose variable is unset skips, saying which; the tracker's
//! tests and the escalation's still fail without `FL_GITHUB_LIVE_REPO`.
//!
//! GitHub's replicas can lag a write. fl's ledger allows for that on its own
//! path; a test that writes around fl, or reads GitHub directly after a
//! write, first waits until GitHub shows the write, and fails only once the
//! same allowance — five reads, a second apart — is spent.
//!
//! Export each token in your shell first, from a secret store (never typed
//! inline, where shell history keeps it), then:
//!
//!   FL_GITHUB_LIVE_REPO=owner/repo \
//!     cargo test -p fl-github --test live -- --ignored --nocapture --test-threads=1
//!
//! The token is read from FL_GITHUB_TOKEN, then GITHUB_TOKEN. For the App
//! instead, set BOTH FL_GITHUB_APP_ID and FL_GITHUB_APP_KEY (the path of its
//! private key file); one without the other is refused, never a fallback to
//! the token. No test prints a token, a header or a client.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fl_core::LedgerFault;
use fl_core::MemStore;
use fl_core::at::At;
use fl_core::decision::{Decision, Outcome, TransitionOutcome};
use fl_core::finding::Finding;
use fl_core::ids::ProjectId;
use fl_core::ids::{GateId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::GateRun;
use fl_core::model::State;
use fl_core::split::{Batch, LedgerCache, RemoteLedger};
use fl_core::store::Bindings;
use fl_core::store::{Catalog, StoreError, Tracker};
use fl_core::verdict::Verdict;
use fl_core::{Escalations, Kind, RoutingMap, TieredTracker, Tombstone};
use fl_github::ledger::layout::{self, Area, BRANCH, Line, SEGMENT_LIMIT};
use fl_github::ledger::render::{self, DecisionView, RunRow};
use fl_github::ledger::{InitOutcome, Mode};
use fl_github::ledger::{Visibility, ruleset_command};
use fl_github::meta::{self, EscalatedFrom};
use fl_github::tracker;
use fl_github::{
    AppCredentials, Client, Credentials, DEFAULT_API, EnvToken, GithubTracker, Method,
};
use fl_github::{GithubLedger, Repo};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Barrier;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

fn repo() -> String {
    std::env::var("FL_GITHUB_LIVE_REPO").expect(
        "set FL_GITHUB_LIVE_REPO=owner/repo (a private throwaway repository) to run the live tests",
    )
}

/// The credential for `repo`: the App when both of its variables are set,
/// else the token.
fn credentials_for(repo: &str) -> Box<dyn Credentials> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    match (var("FL_GITHUB_APP_ID"), var("FL_GITHUB_APP_KEY")) {
        (Some(id), Some(key)) => Box::new(
            AppCredentials::from_file(
                DEFAULT_API,
                id.parse()
                    .expect("FL_GITHUB_APP_ID must be the App's numeric id"),
                key.as_ref(),
                repo,
            )
            .expect("the App credential"),
        ),
        // ⚠ Half an App is refused, never a silent fallback to the token:
        // the run would write as someone other than the one meant.
        (Some(_), None) => panic!(
            "FL_GITHUB_APP_ID is set but FL_GITHUB_APP_KEY is not: set both to write as the \
             App, or neither to use the token"
        ),
        (None, Some(_)) => panic!(
            "FL_GITHUB_APP_KEY is set but FL_GITHUB_APP_ID is not: set both to write as the \
             App, or neither to use the token"
        ),
        (None, None) => Box::new(EnvToken::from_env().expect("FL_GITHUB_TOKEN or GITHUB_TOKEN")),
    }
}

fn client_for(repo: &str) -> Client {
    Client::new(DEFAULT_API, credentials_for(repo))
}

fn client() -> Client {
    client_for(&repo())
}

fn tracker() -> GithubTracker {
    // The repository first: its absence is the message a person needs.
    let repo = repo();
    let client = client();
    let reply = client
        .send(Method::Get, &format!("/repos/{repo}"), None)
        .expect("read the live repository");
    assert_eq!(
        reply.status, 200,
        "GitHub answered {} when the live test read `{repo}`: check FL_GITHUB_LIVE_REPO and \
         that the credential can read that repository",
        reply.status
    );
    let visibility = reply.body["visibility"].as_str().unwrap_or("").to_string();
    assert_eq!(
        visibility, "private",
        "the live tests run only against a PRIVATE repository"
    );
    GithubTracker::open(client, &repo, &MemStore::default())
        .expect("open the live repository")
        .0
}

fn project() -> ProjectId {
    ProjectId(Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7())).unwrap())
}

/// A fresh id: UUIDv7, so a re-run never meets its own earlier entries.
fn fresh() -> Iri {
    Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7())).unwrap()
}

/// This machine's clock, in unix milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_millis() as u64
}

fn now() -> At {
    At::from_unix_millis(now_ms())
}

/// `createCommitOnBranch`, as fl sends it.
const APPEND: &str = "mutation ledgerAppend($input: CreateCommitOnBranchInput!) { \
    createCommitOnBranch(input: $input) { commit { oid } } }";

/// Where the first run on a repository leaves the ledger's first commit.
const ROOT_BRANCH: &str = "fl-live/root";

/// GitHub's replicas can lag a write: read again this many times, this far
/// apart, before a lag counts.
const LAG_READS: u32 = 5;
const LAG_PAUSE: Duration = Duration::from_secs(1);

/// `read`, then read again within the lag allowance until `done` holds.
/// Returns the last read, whether `done` held or not: the caller asserts.
fn eventually<T>(mut read: impl FnMut() -> T, done: impl Fn(&T) -> bool) -> T {
    let mut got = read();
    for _ in 0..LAG_READS {
        if done(&got) {
            break;
        }
        std::thread::sleep(LAG_PAUSE);
        got = read();
    }
    got
}

/// Whether `var` is unset: then the test skips, saying which variable it
/// needs. A resource the owner has not provided is no failure of fl's.
fn unset(var: &str) -> bool {
    let missing = std::env::var(var).map_or(true, |v| v.trim().is_empty());
    if missing {
        eprintln!("skipped: set {var} to run this live test");
    }
    missing
}

/// One live repository, a fresh machine's memory of its ledger, and a
/// record of its own for entries to be tied to.
struct Live {
    client: Client,
    repo: Repo,
    local: MemStore,
    record: RecordId,
}

impl Live {
    /// The repository `var` names, checked to be private or public, as
    /// `private` says. ⚠ Exactly: an `internal` repository is neither, so
    /// a test that needs a public one never runs on it.
    fn on(var: &str, private: bool) -> Live {
        let name = std::env::var(var)
            .unwrap_or_else(|_| panic!("set {var}=owner/repo to run this live test"));
        let client = client_for(&name);
        let r = client
            .send(Method::Get, &format!("/repos/{name}"), None)
            .expect("read the live repository");
        assert_eq!(
            r.status, 200,
            "GitHub answered {} when the live test read `{name}` ({var})",
            r.status
        );
        let want = if private { "private" } else { "public" };
        assert_eq!(
            r.body["visibility"].as_str(),
            Some(want),
            "`{name}` ({var}) must be {want}"
        );
        let text = |k: &str| {
            r.body[k]
                .as_str()
                .unwrap_or_else(|| panic!("`{name}` has no `{k}`"))
                .to_string()
        };
        let repo = Repo {
            full_name: text("full_name"),
            node_id: text("node_id"),
        };
        // A record of its own, so a re-run never reads the decisions the
        // runs before it filed; the issue need not exist for the ledger.
        let n = uuid::Uuid::now_v7().as_u128() as u64 % 1_000_000_000 + 1_000_000;
        let record = RecordId(
            Iri::parse(&format!("https://github.com/{}/issues/{n}", repo.full_name)).unwrap(),
        );
        Live {
            client,
            repo,
            local: MemStore::default(),
            record,
        }
    }

    /// The private throwaway, `FL_GITHUB_LIVE_REPO`.
    fn private() -> Live {
        Live::on("FL_GITHUB_LIVE_REPO", true)
    }

    /// The ledger, with the same lag allowance the tests take.
    fn ledger(&self) -> GithubLedger<'_> {
        GithubLedger::new(&self.client, self.repo.clone(), &self.local)
            .with_lag(LAG_READS, LAG_PAUSE)
    }

    fn path(&self, rest: &str) -> String {
        format!("/repos/{}{rest}", self.repo.full_name)
    }

    /// The commit `branch` points at, if the branch exists.
    fn head_of(&self, branch: &str) -> Option<String> {
        let r = self
            .client
            .send(
                Method::Get,
                &self.path(&format!("/git/ref/heads/{branch}")),
                None,
            )
            .expect("read a branch");
        match r.status {
            200 => Some(
                r.body["object"]["sha"]
                    .as_str()
                    .expect("a commit")
                    .to_string(),
            ),
            404 => None,
            s => panic!("GitHub answered {s} for the branch `{branch}`"),
        }
    }

    fn head(&self) -> String {
        self.head_of(BRANCH).expect("the ledger's branch")
    }

    /// Waits until the ledger's branch shows `oid`, within the lag
    /// allowance; a branch that never does fails the test, naming `oid`.
    fn settle(&self, oid: &str) {
        let seen = eventually(|| self.head_of(BRANCH), |h| h.as_deref() == Some(oid));
        assert_eq!(
            seen.as_deref(),
            Some(oid),
            "the ledger's branch did not show commit {oid} within {LAG_READS} more reads, a \
             second apart"
        );
    }

    /// The commit this machine's last publish recorded, once GitHub shows
    /// it on the ledger's branch.
    fn published_head(&self) -> String {
        let head = self
            .local
            .last_head(&self.repo.node_id)
            .unwrap()
            .expect("a head this machine recorded");
        self.settle(&head);
        head
    }

    /// The parents of commit `oid`. A commit never changes, so a lagging
    /// replica can only not have it yet: read until it does.
    fn parents(&self, oid: &str) -> Vec<String> {
        let r = eventually(
            || {
                self.client
                    .send(
                        Method::Get,
                        &self.path(&format!("/git/commits/{oid}")),
                        None,
                    )
                    .expect("read a commit")
            },
            |r| r.status == 200,
        );
        assert_eq!(
            r.status, 200,
            "GitHub answered {} for commit {oid}",
            r.status
        );
        r.body["parents"]
            .as_array()
            .expect("its parents")
            .iter()
            .map(|p| p["sha"].as_str().expect("a parent").to_string())
            .collect()
    }

    /// This `Live`'s record, for entries to be tied to.
    fn record(&self) -> RecordId {
        self.record.clone()
    }

    fn by(&self) -> String {
        self.client.identity().expect("who the credential is")
    }

    /// The ledger, set up and recorded on this machine with a cut-over.
    ///
    /// ⚠ Safe to run again, and never a walk of the history: the first run
    /// on a repository leaves `fl-live/root` at the ledger's first commit,
    /// and every later run records that commit, as an imported manifest
    /// would, and runs `init` through its "already set up" path.
    fn set_up(&self) -> String {
        let l = self.ledger();
        if let Some(root) = self.head_of(ROOT_BRANCH) {
            self.local
                .set_ledger_root(&self.repo.node_id, &root)
                .unwrap();
            match l.init(&fresh(), None).expect("init over the recorded root") {
                InitOutcome::AlreadySetUp { root: r, .. } => assert_eq!(r, root),
                other => panic!("expected the ledger set up, got {other:?}"),
            }
            return root;
        }
        let root = match l.init(&fresh(), None).expect("init") {
            InitOutcome::Created { root } => {
                self.settle(&root);
                root
            }
            InitOutcome::Confirm { root } => {
                match l.init(&fresh(), Some(&root)).expect("confirm") {
                    InitOutcome::Adopted { root } => root,
                    other => panic!("expected the ledger adopted, got {other:?}"),
                }
            }
            other => panic!("a machine with no root got {other:?}"),
        };
        let made = self
            .client
            .send(
                Method::Post,
                &self.path("/git/refs"),
                Some(&json!({"ref": format!("refs/heads/{ROOT_BRANCH}"), "sha": root})),
            )
            .expect("create the root's branch");
        assert_eq!(made.status, 201, "{:?}", made.body);
        root
    }

    /// One commit on the ledger writing `text` at `path`, as anyone with
    /// write access can. Returns it once GitHub shows it.
    fn hand_commit(&self, path: &str, text: &str) -> String {
        let answer = self
            .client
            .graphql_answer(
                APPEND,
                json!({"input": {
                    "branch": {
                        "repositoryNameWithOwner": self.repo.full_name,
                        "branchName": BRANCH,
                    },
                    "message": {"headline": "fl live test: a hand edit"},
                    "expectedHeadOid": self.head(),
                    "fileChanges": {"additions": [
                        {"path": path, "contents": STANDARD.encode(text)},
                    ]},
                }}),
            )
            .expect("an answer");
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let oid = answer
            .data
            .as_ref()
            .and_then(|d| d.pointer("/createCommitOnBranch/commit/oid"))
            .and_then(Value::as_str)
            .expect("the commit")
            .to_string();
        self.settle(&oid);
        oid
    }

    /// The id of what a POST to `rest` created.
    fn created(&self, rest: &str, body: Value) -> String {
        let r = self
            .client
            .send(Method::Post, &self.path(rest), Some(&body))
            .expect("an answer");
        assert_eq!(r.status, 201, "{:?}", r.body);
        r.body["sha"].as_str().expect("an id").to_string()
    }
}

/// A run of `gate` tied to `record`, stamped now, with `excerpt`.
fn run_on(gate: &GateId, record: &RecordId, excerpt: &str) -> GateRun {
    GateRun {
        id: Some(fresh()),
        at: Some(now()),
        gate: gate.clone(),
        record: Some(record.clone()),
        commit: "live".into(),
        verdict: Verdict::from_predicate(true, 1),
        population: 1,
        output_excerpt: Some(excerpt.into()),
        duration_ms: 1,
        cost_usd_micros: 0,
    }
}

/// A `check` about `record`, resting on `runs`, published with them.
fn batch(record: &RecordId, runs: Vec<GateRun>) -> Batch {
    let rests_on = runs.iter().filter_map(|r| r.id.clone()).collect();
    Batch {
        decision: Decision {
            id: fresh(),
            at: now(),
            record: record.clone(),
            finding: None,
            outcome: Outcome::Check {
                transition: TransitionOutcome {
                    transition: "live".into(),
                    passed: true,
                },
            },
            rests_on,
        },
        runs,
        attempts: vec![],
    }
}

#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_record_and_a_finding_round_trip_on_github() {
    let t = tracker();
    let p = project();
    let r = t.add_record(&p, "fl live test: a record").unwrap();
    for s in [State::Doing, State::Review, State::Done] {
        t.set_record_state(&r, s).unwrap();
        assert_eq!(t.get_record(&r).unwrap().unwrap().state, s);
    }
    let f = t
        .add_finding(Finding::raise(p.clone(), r.clone(), "live", "a live claim"))
        .unwrap();
    let mut fin = t.get_finding(&f).unwrap().unwrap();
    fin.withdraw("live test").unwrap();
    t.update_finding(&fin).unwrap();
    assert_eq!(t.list_records(&p).unwrap().len(), 1);
    assert_eq!(t.list_findings(&p).unwrap().len(), 1);
}

/// ⚠ One clean round is not evidence: this runs rounds and counts. Two
/// writers each add an alias to the same finding — a read-modify-write of
/// one list. A round in which both succeed and one alias is missing is a
/// SILENT lost update, and must never happen.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn concurrent_writers_are_detected_never_silently_lost() {
    const ROUNDS: usize = 10;
    let (mut clean, mut detected, mut lost, mut other) = (0, 0, 0, 0);
    for round in 0..ROUNDS {
        let t = tracker();
        let p = project();
        let r = t.add_record(&p, &format!("fl live race {round}")).unwrap();
        let f = t.add_finding(Finding::raise(p, r, "live", "race")).unwrap();
        let aliases: Vec<Iri> = (0..2)
            .map(|_| Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7())).unwrap())
            .collect();
        // Both writers open, then wait at the barrier, so their writes overlap.
        let gate = std::sync::Barrier::new(2);
        let results: Vec<Result<(), StoreError>> = std::thread::scope(|s| {
            let handles: Vec<_> = aliases
                .iter()
                .map(|a| {
                    let (f, a, gate) = (f.clone(), a.clone(), &gate);
                    s.spawn(move || {
                        let t = tracker();
                        gate.wait();
                        t.add_alias(f.iri(), a)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        // Let GitHub's reads catch up before judging what landed.
        std::thread::sleep(std::time::Duration::from_secs(2));
        let back = t.get_finding(&f).unwrap().unwrap();
        let both_ok = results.iter().all(Result::is_ok);
        let any_conflict = results
            .iter()
            .any(|r| matches!(r, Err(StoreError::Conflict { .. })));
        let all_present = aliases.iter().all(|a| back.also_known_as.contains(a));
        match (both_ok, any_conflict, all_present) {
            (true, _, true) => clean += 1,
            (true, _, false) => lost += 1,
            (false, true, _) => detected += 1,
            (false, false, _) => other += 1,
        }
        println!("round {round}: {results:?}");
    }
    println!(
        "clean {clean}, conflict detected {detected}, silently lost {lost}, other errors {other}"
    );
    assert_eq!(lost, 0, "a lost update went undetected");
    assert_eq!(
        other, 0,
        "a round failed for a reason other than a detected conflict"
    );
}

/// What GitHub has recorded about one issue's changes: the size of its body
/// edit history, and how many of each state-changing timeline event fl
/// counts — replayed as fl replays them (`tracker::counted_events`), so a
/// `labeled` event GitHub records a second time, for a label already on, is
/// not counted here either. `raw` is printed, never asserted.
#[derive(Debug, Clone)]
struct Seen {
    edits: u64,
    events: BTreeMap<String, u64>,
    /// The `labeled` and `unlabeled` events as GitHub lists them, copies
    /// included — printed so a live run shows how often GitHub records a
    /// label event twice.
    raw: BTreeMap<String, u64>,
}

impl Seen {
    fn events(&self, kind: &str) -> u64 {
        self.events.get(kind).copied().unwrap_or(0)
    }

    /// What the model check compares: the edits and the counted changes,
    /// never the raw label events.
    fn counted(&self) -> (u64, &BTreeMap<String, u64>) {
        (self.edits, &self.events)
    }
}

/// The timeline event kinds `check_window` counts.
const STATE_EVENTS: [&str; 5] = ["labeled", "unlabeled", "closed", "reopened", "renamed"];

/// Reads what GitHub has recorded about issue `n`, and checks on every read
/// that the edit history's `totalCount` counts every entry `last: 100`
/// lists — `check_window` counts by `totalCount`.
fn seen(raw: &Client, repo: &str, n: u64) -> Seen {
    let (owner, name) = repo.split_once('/').unwrap();
    let data = raw
        .graphql(
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, \
             name: $name) { issue(number: $number) { userContentEdits(last: 100) { totalCount \
             nodes { id } } } } }",
            json!({"owner": owner, "name": name, "number": n}),
        )
        .unwrap();
    let history = &data["repository"]["issue"]["userContentEdits"];
    let nodes = history["nodes"].as_array().unwrap().len() as u64;
    let total = history["totalCount"].as_u64().unwrap();
    assert!(nodes < 100, "this test assumes fewer than 100 entries");
    assert_eq!(
        total, nodes,
        "`totalCount` does not count the entries `last: 100` lists: the model `check_window` \
         counts edits by is wrong"
    );
    let items = raw
        .get_all(&format!("/repos/{repo}/issues/{n}/timeline?per_page=100"))
        .unwrap();
    let mut events = BTreeMap::new();
    for e in tracker::counted_events(n, &items).unwrap() {
        *events.entry(e.kind).or_default() += 1;
    }
    let mut raw_labels = BTreeMap::new();
    for e in &items {
        if let Some(k) = e["event"]
            .as_str()
            .filter(|k| ["labeled", "unlabeled"].contains(k))
        {
            *raw_labels.entry(k.to_string()).or_default() += 1;
        }
    }
    Seen {
        edits: total,
        events,
        raw: raw_labels,
    }
}

/// What changed between two reads: edits, then each event kind.
fn delta(before: &Seen, after: &Seen) -> (u64, BTreeMap<&'static str, u64>) {
    let events = STATE_EVENTS
        .iter()
        .map(|k| (*k, after.events(k) - before.events(k)))
        .collect();
    (after.edits - before.edits, events)
}

/// After an fl write: read what GitHub shows at once (no pause), and again
/// after two seconds. fl returns only once its own write shows (its
/// `window_after`), and takes everything written before its own events to
/// show with them — so nothing more may appear after it returns.
fn after_fl_write(raw: &Client, repo: &str, n: u64, what: &str) -> Seen {
    let immediate = seen(raw, repo, n);
    std::thread::sleep(Duration::from_secs(2));
    let settled = seen(raw, repo, n);
    println!(
        "{what}: immediately {:?} (raw label events {:?}); after 2 s {:?} (raw label events \
         {:?})",
        immediate.counted(),
        immediate.raw,
        settled.counted(),
        settled.raw
    );
    assert_eq!(
        immediate.counted(),
        settled.counted(),
        "{what}: GitHub showed more two seconds after fl's write returned than at once. fl \
         waits only until its OWN write shows, so something that lands later — its own or \
         someone else's — falls in the next write's window, or is missed"
    );
    settled
}

/// Raw PATCH of issue `n`, answered 200.
fn raw_patch(raw: &Client, repo: &str, n: u64, body: Value) {
    let r = raw
        .send(
            Method::Patch,
            &format!("/repos/{repo}/issues/{n}"),
            Some(&body),
        )
        .unwrap();
    assert_eq!(r.status, 200, "raw PATCH of issue {n}: {:?}", r.body);
}

fn number(id: &Iri) -> u64 {
    id.as_str().rsplit('/').next().unwrap().parse().unwrap()
}

/// Measures, by exact counts, the model `check_window` rests on:
///
/// - `totalCount` counts every entry `last: 100` lists (checked on every read);
/// - a FIRST body edit adds two edit-history entries and a later one adds one
///   (on a record, and again on a finding);
/// - once fl's write returns — fl waits for its own write to show — nothing
///   more appears in the next two seconds: what GitHub shows first is
///   complete (read at once, and again two seconds later);
/// - each label added or removed is one `labeled`/`unlabeled` event, a close
///   one `closed`, a reopen one `reopened` and a retitle one `renamed`;
/// - a rewrite that changes only line endings (CRLF): whether GitHub records
///   an entry is printed, and an fl write after it must not be a conflict.
///
/// Events are counted as fl counts them (`Seen`): a `labeled` event GitHub
/// records a second time for a label already on is not a change, in fl or
/// here, so the "nothing more in two seconds" check is over those changes
/// too. Counted raw, such a copy (measured 2026-10-05) would fail this test
/// while fl handles it.
///
/// Not checked here: the ORDER `last: 100` lists entries in past a hundred
/// entries, and an entry deleted and another added in the same window. If
/// this fails, fix `check_window` and the fake together.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn the_edit_history_and_timeline_counts_match_fls_model() {
    let t = tracker();
    let raw = client();
    let repo = repo();
    let p = project();

    // A record: a first body edit, then a later one.
    let r = t.add_record(&p, "fl live test: edit history").unwrap();
    let rn = number(r.iri());
    let s0 = seen(&raw, &repo, rn);
    t.set_record_state(&r, State::Doing).unwrap(); // first edit; one label off, one on
    let s1 = after_fl_write(&raw, &repo, rn, "record: first edit");
    t.set_record_state(&r, State::Review).unwrap(); // a later edit
    let s2 = after_fl_write(&raw, &repo, rn, "record: later edit");
    let (e1, v1) = delta(&s0, &s1);
    let (e2, v2) = delta(&s1, &s2);
    println!("record: edits {e1} then {e2}; events {v1:?} then {v2:?}");
    assert_eq!(e1, 2, "a first body edit adds two entries (the model)");
    assert_eq!(e2, 1, "a later body edit adds one entry (the model)");
    for v in [&v1, &v2] {
        assert_eq!(
            (
                v["labeled"],
                v["unlabeled"],
                v["closed"],
                v["reopened"],
                v["renamed"]
            ),
            (1, 1, 0, 0, 0),
            "a state change is one `labeled` and one `unlabeled` event, and nothing else"
        );
    }

    // A rewrite that changes only line endings, then an fl write over it.
    let body = raw
        .send(Method::Get, &format!("/repos/{repo}/issues/{rn}"), None)
        .unwrap()
        .body["body"]
        .as_str()
        .unwrap()
        .replace("\r\n", "\n");
    raw_patch(&raw, &repo, rn, json!({"body": body.replace('\n', "\r\n")}));
    std::thread::sleep(Duration::from_secs(2));
    let s3 = seen(&raw, &repo, rn);
    println!(
        "a CRLF-only rewrite added {} edit-history entr(ies)",
        s3.edits - s2.edits
    );
    let crossed = t.set_record_state(&r, State::Doing);
    assert!(
        !matches!(crossed, Err(StoreError::Conflict { .. })),
        "an fl write after a CRLF-only rewrite is not a conflict: {crossed:?}"
    );
    crossed.unwrap();

    // A finding: retitled by fl (a later-edited claim), closed by fl
    // (withdrawn), then reopened by hand.
    let f = t
        .add_finding(Finding::raise(p, r, "live", "fl live test: a finding"))
        .unwrap();
    let fnum = number(f.iri());
    let f0 = seen(&raw, &repo, fnum);
    let mut fin = t.get_finding(&f).unwrap().unwrap();
    fin.claim = "fl live test: a finding, retitled".into();
    t.update_finding(&fin).unwrap(); // first edit, and a retitle
    let f1 = after_fl_write(&raw, &repo, fnum, "finding: retitle");
    let mut fin = t.get_finding(&f).unwrap().unwrap();
    fin.withdraw("live test").unwrap();
    t.update_finding(&fin).unwrap(); // a later edit, and a close
    let f2 = after_fl_write(&raw, &repo, fnum, "finding: withdraw");
    raw_patch(&raw, &repo, fnum, json!({"state": "open"}));
    std::thread::sleep(Duration::from_secs(2));
    let f3 = seen(&raw, &repo, fnum);
    // ⚠ Before the measurements are asserted: a withdrawn finding left open is diverged, and
    // every later full scan of this repository — the next run's lists and
    // alias lookups — would refuse on it.
    let repaired = t.repair(f.iri(), "the live test").unwrap();
    assert!(repaired.changed, "the reopen was undone from the block");
    let (e1, v1) = delta(&f0, &f1);
    let (e2, v2) = delta(&f1, &f2);
    let (e3, v3) = delta(&f2, &f3);
    println!("finding: edits {e1}, {e2}, {e3}; events {v1:?}, {v2:?}, {v3:?}");
    assert_eq!(e1, 2, "a first body edit adds two entries (the model)");
    assert_eq!(
        (
            v1["labeled"],
            v1["unlabeled"],
            v1["closed"],
            v1["reopened"],
            v1["renamed"]
        ),
        (0, 0, 0, 0, 1),
        "a retitle is one `renamed` event"
    );
    assert_eq!(e2, 1, "a later body edit adds one entry (the model)");
    assert_eq!(
        (
            v2["labeled"],
            v2["unlabeled"],
            v2["closed"],
            v2["reopened"],
            v2["renamed"]
        ),
        (1, 1, 1, 0, 0),
        "a withdrawal is one `labeled`, one `unlabeled` and one `closed` event"
    );
    assert_eq!(e3, 0, "a reopen does not edit the body");
    assert_eq!(
        (
            v3["labeled"],
            v3["unlabeled"],
            v3["closed"],
            v3["reopened"],
            v3["renamed"]
        ),
        (0, 0, 0, 1, 0),
        "a reopen is one `reopened` event"
    );
}

/// Routing spec §5's one live test, through the routing tracker over a
/// local `MemStore` and the live repository: a local record in `code`, with
/// an open finding and a security finding, escalates to one open issue in
/// the record's state, labelled with its area, whose block names the old
/// IRI as its create key, an alias and where it came from, and whose text
/// lists the open finding and not the security one. The local item is a
/// tombstone, and the router reads the old id as the issue.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_local_record_escalates_to_an_issue_and_leaves_a_tombstone() {
    const BY: &str = "fl live test";
    const WHY: &str = "the escalation live test";
    const OPEN: &str = "fl live test: an open finding";
    const SECRET: &str = "fl live test: a security finding";
    let github = tracker();
    let raw = client();
    let repo = repo();
    // ⚠ An escalated record's IRI stays an alias of its issue for good, so
    // each run's ids must be its own: a run that reused an earlier run's
    // record IRI would be refused, the IRI already naming an issue.
    let local = MemStore::starting_at(now_ms());
    let p = local.add_project("/live").unwrap();
    local.set_routes(&p, &RoutingMap::starting()).unwrap();
    let router = TieredTracker {
        catalog: &local,
        local: &local,
        routes: &local,
        github: &github,
        escalations: &local,
    };

    // The starting map routes `code` to the local tier. A state other than
    // `todo` shows the issue carries the record's own.
    let old = router
        .add_record_with_area(&p, "fl live test: an escalated record", Some("code"))
        .unwrap();
    router.set_record_state(&old, State::Doing).unwrap();
    let open = router
        .add_finding(Finding::raise(p.clone(), old.clone(), "live", OPEN))
        .unwrap();
    let mut secret = Finding::raise(p.clone(), old.clone(), "live", SECRET);
    secret.security = true;
    let secret = router.add_finding(secret).unwrap();
    assert!(
        local.get_record(&old).unwrap().is_some(),
        "the record is local"
    );
    assert!(
        local.get_finding(&open).unwrap().is_some(),
        "the finding is local"
    );
    assert!(local.get_finding(&secret).unwrap().unwrap().security);

    let at = router.prepare_escalation(old.iri(), Kind::Record).unwrap();
    assert_eq!((at.resumes(), at.found()), (None, None), "a first run");
    let now = now_ms();
    let issue = router.escalate(&at, BY, WHY, now).unwrap();

    // The issue: one, open, in this repository, labelled and in the record's
    // state.
    let (name, n) = meta::parse_issue_url(&issue).expect("an issue URL");
    assert!(
        name.eq_ignore_ascii_case(&repo),
        "{issue} is not in `{repo}`"
    );
    let labels = |v: &Value| -> Vec<String> {
        v["labels"]
            .as_array()
            .map(|ls| {
                ls.iter()
                    .filter_map(|l| l["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let got = eventually(
        || {
            raw.send(Method::Get, &format!("/repos/{repo}/issues/{n}"), None)
                .unwrap()
                .body
        },
        |v| labels(v).iter().any(|l| l == "fl:area/code"),
    );
    assert_eq!(got["state"].as_str(), Some("open"), "{got}");
    let on = labels(&got);
    for want in ["fl:record", "fl:record/doing", "fl:area/code"] {
        assert!(on.iter().any(|l| l == want), "no `{want}` among {on:?}");
    }
    let listed = eventually(|| github.list_records(&p).unwrap(), |rs| !rs.is_empty());
    let ids: Vec<&Iri> = listed.iter().map(|r| r.id.iri()).collect();
    assert_eq!(ids, vec![&issue], "one issue for the record, never two");

    // Its block: the old IRI as create key, first alias and provenance.
    let body = got["body"].as_str().unwrap().replace("\r\n", "\n");
    let (prose, block) = meta::parse_body(&body).expect("the issue's block");
    assert_eq!(block.fl_format, meta::FL_FORMAT_ESCALATED);
    assert_eq!(block.state, "doing");
    assert_eq!(block.area.as_deref(), Some("code"));
    assert_eq!(block.create_key, old.iri().as_str());
    assert_eq!(block.also_known_as.first(), Some(old.iri()));
    assert_eq!(
        block.escalated,
        Some(EscalatedFrom {
            from: old.iri().clone(),
            by: BY.into(),
            reason: WHY.into(),
        })
    );

    // Its text: the escalation line, and the open finding — not the
    // security one.
    let line = meta::escalation_line(&block).expect("an escalation line");
    assert!(body.contains(&line), "no `{line}` in:\n{body}");
    assert!(
        prose.contains("Open findings when this record was escalated:"),
        "no findings list in:\n{prose}"
    );
    for shown in [OPEN, open.iri().as_str()] {
        assert!(
            prose.contains(&render::escape(shown)),
            "`{shown}` is not listed in:\n{prose}"
        );
    }
    for hidden in [SECRET, secret.iri().as_str()] {
        assert!(
            !body.contains(hidden) && !body.contains(&render::escape(hidden)),
            "the security finding's `{hidden}` is published in:\n{body}"
        );
    }

    // The local item is a tombstone, its mark gone, and the router reads
    // the old id as the issue.
    assert_eq!(
        local.tombstone_of(old.iri()).unwrap(),
        Some(Tombstone {
            from: old.iri().clone(),
            to: issue.clone(),
            by: BY.into(),
            reason: WHY.into(),
            at_ms: now,
        })
    );
    assert_eq!(local.mark_of(old.iri()).unwrap(), None);
    let read = local.get_record(&old);
    assert!(
        matches!(&read, Err(StoreError::Escalated { to, .. }) if *to == issue),
        "the local store still answers the old id: {read:?}"
    );
    let seen = router
        .get_record(&old)
        .unwrap()
        .expect("the router reads the old id");
    assert_eq!(seen.id.iri(), &issue);
    assert_eq!(seen.state, State::Doing);
}

/// ⚠ Spec §6.1: `init`'s first commit holds `format` and `README.md` and
/// no parent; a second `fl/ledger` is refused with 422 "Reference already
/// exists" (modelled in `create_branch`); and a machine that records the
/// root records its own cut-over once.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn init_sets_up_a_ledger_on_a_private_repository() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    let root = live.set_up();
    assert_eq!(live.parents(&root), Vec::<String>::new(), "an orphan");
    let tree = live
        .client
        .send(Method::Get, &live.path(&format!("/git/trees/{root}")), None)
        .unwrap();
    let paths: Vec<&str> = tree.body["tree"]
        .as_array()
        .expect("the tree")
        .iter()
        .filter_map(|e| e["path"].as_str())
        .collect();
    assert_eq!(paths, vec!["README.md", "format"], "and no `.github/`");
    let again = live.client.send(
        Method::Post,
        &live.path("/git/refs"),
        Some(&json!({"ref": format!("refs/heads/{BRANCH}"), "sha": root})),
    );
    let err = again
        .expect_err("a second fl/ledger is refused")
        .to_string();
    assert!(err.contains("already exists"), "{err}");
    let other = Live::private();
    other
        .local
        .set_ledger_root(&other.repo.node_id, &root)
        .unwrap();
    assert!(matches!(
        other.ledger().init(&fresh(), None).unwrap(),
        InitOutcome::AlreadySetUp {
            cutover_recorded: true,
            ..
        }
    ));
    assert!(matches!(
        other.ledger().init(&fresh(), None).unwrap(),
        InitOutcome::AlreadySetUp {
            cutover_recorded: false,
            ..
        }
    ));
}

/// ⚠ Spec §3.2, §8.4: two machines appending at once both land; neither
/// entry is lost or written twice.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn two_flushes_racing_both_land() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let gate = GateId(fresh());
    let barrier = Barrier::new(2);
    // ⚠ A racer cannot wait for its own commit to show: the other's may
    // already stand on top of it. Each returns the head it recorded instead.
    let (ids, heads): (Vec<Iri>, Vec<String>) = std::thread::scope(|s| {
        let racers: Vec<_> = (0..2)
            .map(|i| {
                let (gate, barrier) = (&gate, &barrier);
                s.spawn(move || {
                    let live = Live::private();
                    live.set_up();
                    let run = run_on(gate, &live.record(), &format!("racer {i}"));
                    let id = run.id.clone().unwrap();
                    barrier.wait();
                    live.ledger()
                        .publish(&batch(&live.record(), vec![run]))
                        .expect("each flush lands");
                    let head = live.local.last_head(&live.repo.node_id).unwrap();
                    (id, head.expect("the head this racer's flush recorded"))
                })
            })
            .collect();
        racers.into_iter().map(|h| h.join().unwrap()).unzip()
    });
    // Two commits, one on top of the other: the later is the one whose
    // parent is the earlier.
    assert_ne!(heads[0], heads[1], "each flush is a commit of its own");
    let later = if live.parents(&heads[0]).contains(&heads[1]) {
        &heads[0]
    } else {
        assert!(
            live.parents(&heads[1]).contains(&heads[0]),
            "neither racer's commit stands on the other's: {heads:?}"
        );
        &heads[1]
    };
    live.settle(later);
    let reader = Live::private();
    reader.set_up();
    let back: Vec<Option<Iri>> = reader
        .ledger()
        .runs(&gate)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(back.len(), 2, "{back:?}");
    for id in &ids {
        assert_eq!(
            back.iter().filter(|b| b.as_ref() == Some(id)).count(),
            1,
            "{id}"
        );
    }
}

/// ⚠ Confirms what `judge` and the fake take: a stale `expectedHeadOid` is refused
/// with `STALE_DATA`, or a message saying where the branch was expected to
/// point, and nothing lands.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn create_commit_on_branch_is_refused_when_the_head_moved() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let l = live.ledger();
    let before = l.check_format().unwrap();
    let record = live.record();
    l.publish(&batch(
        &record,
        vec![run_on(&GateId(fresh()), &record, "moves the head")],
    ))
    .unwrap();
    let after = live.published_head();
    assert_ne!(before, after);
    let probe = run_on(&GateId(fresh()), &record, "a stale append");
    let path = layout::segment_path(&layout::dir(Area::Runs, probe.gate.iri()), 1);
    let text = format!("{}\n", Line::Run(probe).encode(&live.by()));
    let answer = live
        .client
        .graphql_answer(
            APPEND,
            json!({"input": {
                "branch": {
                    "repositoryNameWithOwner": live.repo.full_name,
                    "branchName": BRANCH,
                },
                "message": {"headline": "fl live test: a stale append"},
                "expectedHeadOid": before,
                "fileChanges": {"additions": [{"path": path, "contents": STANDARD.encode(text)}]},
            }}),
        )
        .expect("an answer");
    println!(
        "a stale append: status {}, errors {:?}",
        answer.status, answer.errors
    );
    assert_eq!(answer.status, 200);
    let moved = answer.errors.iter().any(|e| {
        e["type"] == "STALE_DATA"
            || e["message"]
                .as_str()
                .is_some_and(|m| m.contains("Expected branch to point to"))
    });
    assert!(moved, "refused as a moved head: {:?}", answer.errors);
    assert_eq!(live.head(), after, "nothing landed");
}

/// ⚠ Spec §3.5: an edit of a line this machine read is caught (check 4),
/// naming the file and the commit; a line fl cannot read is named with the
/// commit that added it — GitHub's blame (modelled in `git.rs`); and a
/// compare asked one commit per page still says `ahead` across many
/// (modelled in `compare`).
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_hand_edit_is_detected_and_named() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    let root = live.set_up();
    let l = live.ledger();
    let record = live.record();

    let gate = GateId(fresh());
    let run = run_on(&gate, &record, "as published");
    l.publish(&batch(&record, vec![run.clone()]))
        .expect("published");
    live.published_head();
    assert_eq!(l.runs(&gate).expect("read, and cached").len(), 1);
    let seg = layout::segment_path(&layout::dir(Area::Runs, gate.iri()), 1);
    let mut edited = run;
    edited.output_excerpt = Some("edited by hand".into());
    let edit = live.hand_commit(&seg, &format!("{}\n", Line::Run(edited).encode(&live.by())));
    let err = l
        .runs(&gate)
        .expect_err("an edited line is caught")
        .to_string();
    assert!(
        err.contains(&format!("`{seg}`")) && err.contains(&edit),
        "{err}"
    );

    let other = GateId(fresh());
    let seg2 = layout::segment_path(&layout::dir(Area::Runs, other.iri()), 1);
    let added = live.hand_commit(&seg2, "not a line fl wrote\n");
    match l.runs(&other) {
        Err(StoreError::Ledger(LedgerFault::Unreadable {
            file, line, commit, ..
        })) => assert_eq!((file, line, commit), (seg2, 1, added)),
        got => panic!("expected an unreadable line named by its commit, got {got:?}"),
    }

    let head = live.head();
    let r = live
        .client
        .send(
            Method::Get,
            &live.path(&format!("/compare/{root}...{head}?per_page=1")),
            None,
        )
        .expect("a compare");
    assert_eq!(r.body["status"], "ahead", "{}", r.body["status"]);
    assert!(
        r.body["total_commits"].as_u64().unwrap_or(0) > 1,
        "{}",
        r.body["total_commits"]
    );
    assert!(r.body["commits"].as_array().map_or(0, Vec::len) <= 1);
    let machine = Live::private();
    machine.set_up();
    assert_eq!(
        machine
            .ledger()
            .check_head()
            .expect("checked from the root"),
        head
    );
}

/// ⚠ Confirms what `git.rs` takes: GraphQL sends `TreeEntry.mode` as an Int whose
/// value is the octal mode; REST sends it as a string.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn tree_entry_modes_are_integers() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    let root = live.set_up();
    let (owner, name) = live.repo.full_name.split_once('/').unwrap();
    let data = live
        .client
        .graphql(
            "query($owner: String!, $name: String!, $e: String!) { repository(owner: $owner, \
             name: $name) { object(expression: $e) { ... on Tree { entries { name mode } } } } }",
            json!({"owner": owner, "name": name, "e": format!("{root}:")}),
        )
        .unwrap();
    let entries = data["repository"]["object"]["entries"]
        .as_array()
        .expect("the first commit's tree");
    let format = entries
        .iter()
        .find(|e| e["name"] == "format")
        .expect("`format`");
    assert_eq!(format["mode"], json!(0o100644), "{format}");
    let rest = live
        .client
        .send(Method::Get, &live.path(&format!("/git/trees/{root}")), None)
        .unwrap();
    let listed = rest.body["tree"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "format")
        .cloned()
        .expect("`format`");
    assert_eq!(listed["mode"], json!("100644"), "{listed}");
}

/// ⚠ Measured on 2026-10-02 and pinned here: a private repository on
/// GitHub Free answers `rules/branches/fl/ledger` with `200 []`, which
/// `mode()` reads as detection-only (decision 12).
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_private_repository_without_a_ruleset_is_detection_only() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    let r = live
        .client
        .send(
            Method::Get,
            &live.path(&format!("/rules/branches/{BRANCH}?per_page=100")),
            None,
        )
        .expect("the rules are readable");
    assert_eq!((r.status, &r.body), (200, &json!([])), "{:?}", r.body);
    let mode = live.ledger().mode().unwrap();
    assert!(matches!(mode, Mode::DetectionOnly { .. }), "{mode:?}");
}

/// ⚠ Confirms what `branches_under` takes: `git/matching-refs/heads/<prefix>`
/// lists every branch whose name starts with the prefix, and `200 []`
/// when none does. A branch under `fl/ledger/` cannot sit beside
/// `fl/ledger`, so the listing is checked on `fl-live/`.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_branch_under_the_ledger_branch_is_found() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let under = |prefix: &str| -> Vec<String> {
        live.client
            .get_all(&live.path(&format!("/git/matching-refs/heads/{prefix}")))
            .expect("matching-refs")
            .iter()
            .map(|r| r["ref"].as_str().expect("a ref").to_string())
            .collect()
    };
    let root_ref = format!("refs/heads/{ROOT_BRANCH}");
    let found = eventually(|| under("fl-live/"), |f| f.contains(&root_ref));
    assert!(found.contains(&root_ref), "{found:?}");
    assert!(
        found.iter().all(|r| r.starts_with("refs/heads/fl-live/")),
        "{found:?}"
    );
    assert_eq!(under("fl/ledger/"), Vec::<String>::new());
}

/// Spec §3.5 check 1: a head whose history is not the anchor's is read
/// as a rewrite, whatever GitHub answers a compare of unrelated histories
/// (printed).
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn an_unrelated_history_is_read_as_a_rewrite() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let tree = live.created(
        "/git/trees",
        json!({"tree": [
            {"path": "probe", "mode": "100644", "type": "blob", "content": "unrelated\n"},
        ]}),
    );
    let stray = live.created(
        "/git/commits",
        json!({"message": "fl live test: an unrelated history", "tree": tree, "parents": []}),
    );
    let head = live.head();
    let raw = live.client.send(
        Method::Get,
        &live.path(&format!("/compare/{stray}...{head}?per_page=1")),
        None,
    );
    match &raw {
        Ok(r) => println!("a compare of unrelated histories: {} {}", r.status, r.body),
        Err(e) => println!("a compare of unrelated histories: {e}"),
    }
    let other = Live::private();
    other
        .local
        .set_ledger_root(&other.repo.node_id, &stray)
        .unwrap();
    let err = other.ledger().check_head().unwrap_err();
    assert!(
        matches!(err, StoreError::Ledger(LedgerFault::Rewritten { .. })),
        "{err:?}"
    );
}

/// Spec §3.1: a segment filled near its 256 KB limit lands through
/// `createCommitOnBranch`, and the next lines roll over to a second.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_near_full_segment_lands_through_create_commit_on_branch() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let l = live.ledger();
    let by = live.by();
    let record = live.record();
    let gate = GateId(fresh());
    let excerpt = "x".repeat(4_000);
    let (mut first, mut bytes) = (Vec::new(), 0usize);
    loop {
        let r = run_on(&gate, &record, &excerpt);
        let len = Line::Run(r.clone()).encode(&by).len() + 1;
        if bytes + len > SEGMENT_LIMIT - 8 * 1024 {
            break;
        }
        bytes += len;
        first.push(r);
    }
    assert!(bytes > SEGMENT_LIMIT - 16 * 1024, "near full: {bytes}");
    l.publish(&batch(&record, first.clone()))
        .expect("a near-full segment lands");
    live.published_head();
    let second: Vec<GateRun> = (0..4).map(|_| run_on(&gate, &record, &excerpt)).collect();
    l.publish(&batch(&record, second.clone()))
        .expect("the rollover lands");
    let head = live.published_head();
    assert_eq!(l.runs(&gate).unwrap().len(), first.len() + second.len());
    let dir = layout::dir(Area::Runs, gate.iri());
    // At the commit itself, which never changes: a lagging replica can
    // only not have it yet.
    let listing = eventually(
        || {
            live.client
                .send(
                    Method::Get,
                    &live.path(&format!("/contents/{dir}?ref={head}")),
                    None,
                )
                .unwrap()
        },
        |r| r.status == 200,
    );
    assert_eq!(listing.status, 200, "the directory at {head}");
    let names: Vec<&str> = listing
        .body
        .as_array()
        .expect("the directory")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert_eq!(names, vec!["1.jsonl", "2.jsonl"]);
}

impl Live {
    /// The public throwaway with a ruleset on `fl/ledger`,
    /// `FL_GITHUB_LIVE_PUBLIC_REPO`.
    fn public() -> Live {
        Live::on("FL_GITHUB_LIVE_PUBLIC_REPO", false)
    }
}

/// ⚠ Spec §4.2, §4.3, §8.4: a decision comment posted on an fl record
/// keeps its marker through GitHub's storage and is found among the
/// issue's comments as fl's own; and GitHub's rendering of it makes no
/// mention and no issue link of its `@`, `#` and `GH-` text. A control
/// comment posted raw must render its `#<n>` as a link, so the check
/// cannot pass by recognising nothing.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_decision_comment_round_trips_with_its_marker() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let record = tracker()
        .add_record(&project(), "fl live test: a decision comment")
        .unwrap();
    let decision = Decision {
        id: fresh(),
        at: now(),
        record: record.clone(),
        finding: None,
        outcome: Outcome::Check {
            transition: TransitionOutcome {
                transition: "live".into(),
                passed: true,
            },
        },
        rests_on: vec![],
    };
    let (_, n) = fl_github::meta::parse_issue_url(record.iri()).expect("an issue URL");
    let view = DecisionView {
        decision: decision.clone(),
        by: live.by(),
        commit: None,
        rows: vec![RunRow {
            role: "live".into(),
            gate: format!("@fl-live-test-nobody #{n} GH-{n}"),
            run: run_on(&GateId(fresh()), &record, "an excerpt"),
        }],
        attempt: None,
        missing: vec![],
    };
    let body = render::render(
        &view,
        &live.repo.full_name,
        Visibility::Private,
        Some("A check changes no state."),
    );
    let l = live.ledger();
    l.post_comment(record.iri(), &body).expect("posted");
    // Each read after a write waits until GitHub shows it, and asserts on
    // its last read.
    let at = eventually(|| l.issue_at(record.iri()), Result::is_ok).expect("the issue");
    assert_eq!(at.moved_to, None);
    let posted = eventually(
        || l.posted(&at, &BTreeSet::new()).expect("listed"),
        |p| p.contains(&decision.id),
    );
    assert!(posted.contains(&decision.id), "{posted:?}");
    assert!(!posted.contains(&fresh()));
    let ours_in = |listed: &[Value]| -> Option<String> {
        listed
            .iter()
            .filter_map(|c| c["body"].as_str())
            .find(|b| render::marked(b).as_ref() == Some(&decision.id))
            .map(str::to_string)
    };
    let listed = eventually(
        || {
            live.client
                .get_all(&format!("{}?per_page=100", at.comments))
                .expect("listed")
        },
        |listed| ours_in(listed).is_some(),
    );
    let back = ours_in(&listed).expect("the comment");
    println!(
        "GitHub kept the comment {}",
        if back == body {
            "byte for byte"
        } else {
            "with changes"
        }
    );
    assert!(back.contains("@&#8203;fl-live-test-nobody"), "{back}");

    // What GitHub renders: GraphQL's `bodyHTML` (REST's `body_html` needs
    // an Accept header fl's client does not send).
    l.post_comment(record.iri(), &format!("fl live test control: #{n} GH-{n}"))
        .expect("the control posted");
    let (owner, name) = live.repo.full_name.split_once('/').unwrap();
    let holds = |nodes: &[Value], part: &str| {
        nodes
            .iter()
            .any(|c| c["body"].as_str().is_some_and(|b| b.contains(part)))
    };
    let nodes = eventually(
        || {
            let data = live
                .client
                .graphql(
                    "query($owner: String!, $name: String!, $n: Int!) { repository(owner: \
                     $owner, name: $name) { issue(number: $n) { comments(last: 20) { nodes { \
                     body bodyHTML } } } } }",
                    json!({"owner": owner, "name": name, "n": n}),
                )
                .expect("the rendered comments");
            data["repository"]["issue"]["comments"]["nodes"]
                .as_array()
                .expect("the comments")
                .clone()
        },
        |nodes| holds(nodes, "fl live test control") && holds(nodes, decision.id.as_str()),
    );
    let html_of = |part: &str| -> String {
        nodes
            .iter()
            .find(|c| c["body"].as_str().is_some_and(|b| b.contains(part)))
            .and_then(|c| c["bodyHTML"].as_str())
            .unwrap_or_else(|| panic!("no rendered comment holds {part}"))
            .to_string()
    };
    let control = html_of("fl live test control");
    assert!(
        control.contains("issue-link"),
        "the control links: {control}"
    );
    let ours = html_of(decision.id.as_str());
    println!("rendered: {ours}");
    // No raw mention is posted as a control: it could notify a real
    // account. Instead the cell must have rendered its `@` text, with the
    // zero-width space after it, so the check below looked at something;
    // if GitHub renamed `user-mention`, this test's printout shows it.
    assert!(
        ours.contains("@\u{200b}fl-live-test-nobody")
            || ours.contains("@&#8203;fl-live-test-nobody"),
        "the gate's cell rendered: {ours}"
    );
    assert!(!ours.contains("user-mention"), "a mention: {ours}");
    assert!(!ours.contains("issue-link"), "an issue link: {ours}");
    assert!(
        !ours.contains(&format!("/issues/{n}\"")),
        "a link to #{n}: {ours}"
    );
}

/// ⚠ Confirms what `judge` takes: a credential without Contents: write is refused
/// — a 403 naming the permission, or a 200 with `FORBIDDEN` — and nothing
/// lands.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO, FL_GITHUB_LIVE_READ_ONLY_TOKEN and a credential"]
fn create_commit_on_branch_without_contents_write_is_refused() {
    if unset("FL_GITHUB_LIVE_REPO") {
        return;
    }
    if unset("FL_GITHUB_LIVE_READ_ONLY_TOKEN") {
        return;
    }
    let live = Live::private();
    live.set_up();
    let token = std::env::var("FL_GITHUB_LIVE_READ_ONLY_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .expect(
            "set FL_GITHUB_LIVE_READ_ONLY_TOKEN to a fine-grained token on FL_GITHUB_LIVE_REPO \
             with Contents: read only",
        );
    let reader = Client::new(
        DEFAULT_API,
        Box::new(
            EnvToken::from_lookup(move |k| (k == "FL_GITHUB_TOKEN").then(|| token.clone()))
                .unwrap(),
        ),
    );
    let l =
        GithubLedger::new(&reader, live.repo.clone(), &live.local).with_lag(LAG_READS, LAG_PAUSE);
    let head = live.head();
    let record = live.record();
    let err = l
        .publish(&batch(
            &record,
            vec![run_on(&GateId(fresh()), &record, "refused")],
        ))
        .expect_err("a read-only credential cannot append")
        .to_string();
    assert!(err.contains("Contents: write"), "{err}");
    assert_eq!(live.head(), head, "nothing landed");
}

/// ⚠ Confirms what `branch_head` and the fake take: a repository with no commit
/// answers a ref read 409, and `init` says to push a first commit.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_EMPTY_REPO and a credential"]
fn an_empty_repository_is_refused_naming_a_first_commit() {
    if unset("FL_GITHUB_LIVE_EMPTY_REPO") {
        return;
    }
    let name = std::env::var("FL_GITHUB_LIVE_EMPTY_REPO")
        .expect("set FL_GITHUB_LIVE_EMPTY_REPO=owner/repo: a private repository with no commit");
    let client = client_for(&name);
    // ⚠ First: the repository really has no commit, so `init` below cannot
    // create anything.
    let raw = client.send(
        Method::Get,
        &format!("/repos/{name}/git/ref/heads/fl"),
        None,
    );
    let err = raw
        .expect_err("a ref read on a repository with no commit is refused")
        .to_string();
    assert!(
        err.contains("GitHub answered 409 ") && err.contains("Git Repository is empty"),
        "{err}"
    );
    let r = client
        .send(Method::Get, &format!("/repos/{name}"), None)
        .expect("the repository");
    let repo = Repo {
        full_name: r.body["full_name"].as_str().expect("a name").into(),
        node_id: r.body["node_id"].as_str().expect("a node").into(),
    };
    let local = MemStore::default();
    let l = GithubLedger::new(&client, repo, &local);
    // `fl github ledger init` reads the mode before it calls `init`: on a
    // repository with no commit, the rules must still read, or the person
    // would see a rules error instead of "push a first commit".
    let mode = l.mode();
    println!("the mode of a repository with no commit: {mode:?}");
    assert!(mode.is_ok(), "{mode:?}");
    let err = l
        .init(&fresh(), None)
        .expect_err("init is refused")
        .to_string();
    assert!(
        err.contains("is empty: GitHub keeps no branch until"),
        "{err}"
    );
}

/// ⚠ Confirms what `mode()` takes: the token reads `rules/branches/fl/ledger`, and
/// an active ruleset with both rules reads as protected.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_PUBLIC_REPO and a credential"]
fn rules_on_the_ledger_branch_are_readable() {
    if unset("FL_GITHUB_LIVE_PUBLIC_REPO") {
        return;
    }
    let live = Live::public();
    let mode = live
        .ledger()
        .mode()
        .expect("the rules are readable by the token");
    assert_eq!(
        mode,
        Mode::Protected,
        "`{}` needs an active ruleset on `fl/ledger` with `non_fast_forward` and `deletion`. \
         An administrator adds it with:\n{}",
        live.repo.full_name,
        ruleset_command(&live.repo.full_name)
    );
}

/// ⚠ Spec §6.2, §8.4: under the ruleset GitHub refuses a force update and a
/// deletion of `fl/ledger`, and the ledger stays as it was.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_PUBLIC_REPO and a credential"]
fn a_force_update_and_a_deletion_of_the_ledger_are_refused() {
    if unset("FL_GITHUB_LIVE_PUBLIC_REPO") {
        return;
    }
    let live = Live::public();
    // ⚠ First: without the ruleset in force, or with a credential that may
    // bypass it, what follows would succeed and destroy this repository's
    // ledger — or pass by never being refused. Nothing is written until
    // both are known.
    assert_eq!(
        live.ledger().mode().expect("the rules"),
        Mode::Protected,
        "refusing to try a force update without the ruleset in force"
    );
    let rules = live
        .client
        .get_all(&live.path(&format!("/rules/branches/{BRANCH}?per_page=100")))
        .expect("the rules");
    let mut ids: Vec<u64> = rules
        .iter()
        .map(|r| r["ruleset_id"].as_u64().expect("a ruleset id"))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert!(!ids.is_empty(), "no ruleset names fl/ledger");
    for id in ids {
        let set = live
            .client
            .send(Method::Get, &live.path(&format!("/rulesets/{id}")), None)
            .expect("the ruleset");
        let can = set.body["current_user_can_bypass"].as_str();
        assert_eq!(
            can,
            Some("never"),
            "refusing to try: the credential's bypass of ruleset {id} is {can:?} (bypass list: {})",
            set.body["bypass_actors"]
        );
    }
    let root = live.set_up();
    let record = live.record();
    live.ledger()
        .publish(&batch(
            &record,
            vec![run_on(&GateId(fresh()), &record, "public test data")],
        ))
        .expect("an append lands under the ruleset");
    let head = live.published_head();
    assert_ne!(head, root);

    // fl sends no DELETE and never forces a ref: both requests go straight
    // through ureq, with the same credential, to this repository's
    // `fl/ledger` only. A refusal is GitHub's 4xx; a 401 (the credential)
    // or a 404 (no such ref) is not the ruleset's refusal, and nor is an
    // answer that never came.
    let token = credentials_for(&live.repo.full_name)
        .token()
        .expect("a token");
    let auth = format!("Bearer {token}");
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .build(),
    );
    fn with_headers<B>(rb: ureq::RequestBuilder<B>, auth: &str) -> ureq::RequestBuilder<B> {
        rb.header("Authorization", auth)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "fl-live-test")
    }
    let answer = |what: &str, r: Result<ureq::http::Response<ureq::Body>, ureq::Error>| {
        let mut r = r.unwrap_or_else(|e| panic!("{what}: no answer from GitHub: {e}"));
        let status = r.status().as_u16();
        let text = r.body_mut().read_to_string().unwrap_or_default();
        println!("{what}: {status} {text}");
        assert!(
            (400..500).contains(&status) && status != 401 && status != 404,
            "GitHub did not refuse {what} of fl/ledger: {status} {text}"
        );
    };
    let url = format!(
        "{DEFAULT_API}/repos/{}/git/refs/heads/{BRANCH}",
        live.repo.full_name
    );
    answer(
        "a force update",
        with_headers(agent.patch(&url), &auth).send_json(json!({"sha": root, "force": true})),
    );
    answer("a deletion", with_headers(agent.delete(&url), &auth).call());
    assert_eq!(
        live.head_of(BRANCH).as_deref(),
        Some(head.as_str()),
        "the ledger is as it was"
    );
}
