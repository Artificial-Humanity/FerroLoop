//! Against GitHub itself (GitHub tracker spec §8.3). Ignored by default.
//!
//! Run against a PRIVATE THROWAWAY repository — these tests create issues
//! and never delete them. Export the token in your shell first, from a
//! secret store (never typed inline, where shell history keeps it), then:
//!
//!   FL_GITHUB_LIVE_REPO=owner/repo \
//!     cargo test -p fl-github --test live -- --ignored --nocapture --test-threads=1
//!
//! The token is read from FL_GITHUB_TOKEN, then GITHUB_TOKEN. For the App
//! instead, set BOTH FL_GITHUB_APP_ID and FL_GITHUB_APP_KEY (the path of its
//! private key file); one without the other is refused, never a fallback to
//! the token.

use fl_core::MemStore;
use fl_core::finding::Finding;
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::model::State;
use fl_core::store::{StoreError, Tracker};
use fl_github::{
    AppCredentials, Client, Credentials, DEFAULT_API, EnvToken, GithubTracker, Method,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

fn repo() -> String {
    std::env::var("FL_GITHUB_LIVE_REPO").expect(
        "set FL_GITHUB_LIVE_REPO=owner/repo (a private throwaway repository) to run the live tests",
    )
}

fn client() -> Client {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let creds: Box<dyn Credentials> = match (var("FL_GITHUB_APP_ID"), var("FL_GITHUB_APP_KEY")) {
        (Some(id), Some(key)) => Box::new(
            AppCredentials::from_file(
                DEFAULT_API,
                id.parse()
                    .expect("FL_GITHUB_APP_ID must be the App's numeric id"),
                key.as_ref(),
                &repo(),
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
    };
    Client::new(DEFAULT_API, creds)
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
/// edit history, and how many of each state-changing timeline event.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    edits: u64,
    events: BTreeMap<String, u64>,
}

impl Seen {
    fn events(&self, kind: &str) -> u64 {
        self.events.get(kind).copied().unwrap_or(0)
    }
}

/// The timeline events `check_window` counts.
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
    let mut events = BTreeMap::new();
    for e in raw
        .get_all(&format!("/repos/{repo}/issues/{n}/timeline?per_page=100"))
        .unwrap()
    {
        if let Some(k) = e["event"].as_str().filter(|k| STATE_EVENTS.contains(k)) {
            *events.entry(k.to_string()).or_default() += 1;
        }
    }
    Seen {
        edits: total,
        events,
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
    println!("{what}: immediately {immediate:?}; after 2 s {settled:?}");
    assert_eq!(
        immediate, settled,
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
