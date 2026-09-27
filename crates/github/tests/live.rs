//! Against GitHub itself (GitHub tracker spec §8.3). Ignored by default.
//!
//! Run against a PRIVATE THROWAWAY repository — these tests create issues
//! and never delete them:
//!
//!   FL_GITHUB_LIVE_REPO=owner/repo FL_GITHUB_TOKEN=… \
//!     cargo test -p fl-github --test live -- --ignored --nocapture --test-threads=1
//!
//! For the App instead of a token, set FL_GITHUB_APP_ID and FL_GITHUB_APP_KEY
//! (the path of its private key file).

use fl_core::MemStore;
use fl_core::finding::Finding;
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::model::State;
use fl_core::store::{StoreError, Tracker};
use fl_github::{AppCredentials, Client, Credentials, DEFAULT_API, EnvToken, GithubTracker};

fn repo() -> String {
    std::env::var("FL_GITHUB_LIVE_REPO").expect(
        "set FL_GITHUB_LIVE_REPO=owner/repo (a private throwaway repository) to run the live tests",
    )
}

fn client() -> Client {
    let creds: Box<dyn Credentials> = match (
        std::env::var("FL_GITHUB_APP_ID"),
        std::env::var("FL_GITHUB_APP_KEY"),
    ) {
        (Ok(id), Ok(key)) => Box::new(
            AppCredentials::from_file(
                DEFAULT_API,
                id.parse().expect("a numeric App id"),
                key.as_ref(),
                &repo(),
            )
            .expect("the App credential"),
        ),
        _ => Box::new(EnvToken::from_env().expect("FL_GITHUB_TOKEN or GITHUB_TOKEN")),
    };
    Client::new(DEFAULT_API, creds)
}

fn tracker() -> GithubTracker {
    // The repository first: its absence is the message a person needs.
    let repo = repo();
    let client = client();
    let visibility = client
        .send(fl_github::Method::Get, &format!("/repos/{repo}"), None)
        .expect("read the live repository")
        .body["visibility"]
        .as_str()
        .unwrap_or("")
        .to_string();
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

/// Measures the model `check_window` rests on, EXACTLY: a first body edit
/// adds two edit-history entries and a later one adds one, and each label
/// change adds one timeline event. `check_window` only tolerates up to its
/// model, so only a direct count can show the model is wrong. If this
/// fails, fix `check_window` and the fake together.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn the_edit_history_and_timeline_counts_match_fls_model() {
    let t = tracker();
    let raw = client();
    let repo = repo();
    let (owner, name) = repo.split_once('/').unwrap();
    let r = t
        .add_record(&project(), "fl live test: edit history")
        .unwrap();
    let n: u64 = r
        .iri()
        .as_str()
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let edits = || {
        raw.graphql(
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { userContentEdits(last: 100) { nodes { id } } } } }",
            serde_json::json!({"owner": owner, "name": name, "number": n}),
        )
        .unwrap()["repository"]["issue"]["userContentEdits"]["nodes"]
            .as_array()
            .unwrap()
            .len()
    };
    let labelled = || {
        raw.get_all(&format!("/repos/{repo}/issues/{n}/timeline?per_page=100"))
            .unwrap()
            .iter()
            .filter(|e| matches!(e["event"].as_str(), Some("labeled" | "unlabeled")))
            .count()
    };
    let (e0, l0) = (edits(), labelled());
    t.set_record_state(&r, State::Doing).unwrap(); // first edit; one label off, one on
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (e1, l1) = (edits(), labelled());
    t.set_record_state(&r, State::Review).unwrap(); // a later edit
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (e2, l2) = (edits(), labelled());
    println!("edits {e0} -> {e1} -> {e2}; label events {l0} -> {l1} -> {l2}");
    assert_eq!(e1 - e0, 2, "a first body edit adds two entries (the model)");
    assert_eq!(e2 - e1, 1, "a later body edit adds one entry (the model)");
    assert_eq!(
        (l1 - l0, l2 - l1),
        (2, 2),
        "each label change is one timeline event"
    );
}
