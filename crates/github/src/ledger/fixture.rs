//! `SplitLedger` over `GithubLedger` and the fake GitHub, for the shared
//! ledger suites (GitHub ledger spec §8.2): the split ledger meets the
//! same contract over the GitHub side as over `MemRemote`.

use super::GithubLedger;
use crate::client::Client;
use crate::creds::EnvToken;
use crate::fake::FakeGithub;
use crate::tracker::GithubTracker;
use fl_core::conformance::{
    Bound, Fixture, RemoteControl, SplitFixture, entry_iri, sample_decision, sample_record_run,
};
use fl_core::ids::{GateId, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::split::{Batch, Outbox, RemoteLedger, SplitLedger};
use fl_core::store::{Bindings, StoreError};
use fl_core::{CatalogChecked, KindRouted, MemStore};
use std::cell::RefCell;
use std::time::Duration;

fn client(fake: &FakeGithub) -> Client {
    Client::new(
        &fake.url(),
        Box::new(EnvToken::from_lookup(|_| Some("t".into())).expect("a token")),
    )
}

/// The GitHub side, keeping each batch it is handed, so the suite sees
/// what a flush offered.
struct Recorded<'a> {
    inner: &'a dyn RemoteLedger,
    batches: RefCell<Vec<Batch>>,
}

impl RemoteLedger for Recorded<'_> {
    fn repo_node_id(&self) -> &str {
        self.inner.repo_node_id()
    }
    fn owns_record(&self, record: &RecordId) -> Result<bool, StoreError> {
        self.inner.owns_record(record)
    }
    fn publish(&self, batch: &Batch) -> Result<Option<String>, StoreError> {
        self.batches.borrow_mut().push(batch.clone());
        self.inner.publish(batch)
    }
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        self.inner.gate_runs(gate)
    }
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        self.inner.attempts(project)
    }
}

struct Controls<'a> {
    fake: &'a FakeGithub,
    remote: &'a Recorded<'a>,
}

impl RemoteControl for Controls<'_> {
    fn set_down(&self, down: bool) {
        self.fake.state().down = down;
    }
    fn lose_next_answer(&self) {
        self.fake.state().hang_up_after_next_commit = true;
    }
    fn foreign_record(&self) -> RecordId {
        RecordId(
            Iri::parse("https://github.com/acme/other/issues/1").expect("an issue URL is an IRI"),
        )
    }
    fn remote(&self) -> &dyn RemoteLedger {
        self.remote
    }
    fn batches(&self) -> Vec<Batch> {
        self.remote.batches.borrow().clone()
    }
}

/// A split ledger over a `MemStore`, a `GithubTracker` and a `GithubLedger`
/// on a fresh fake whose ledger is set up, with this machine's cut-over
/// before every sample entry.
struct OverFake;

impl SplitFixture for OverFake {
    fn with_split(&self, f: &mut dyn FnMut(&Bound<'_>, &dyn RemoteControl)) {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local
            .set_ledger_root("R_1", &root)
            .expect("a fresh store records a root");
        local
            .set_cutover("R_1", &entry_iri(0))
            .expect("a fresh store records a cut-over");
        let tracker = GithubTracker::open(client(&fake), "acme/widgets", &local)
            .expect("the fake's repository opens")
            .0
            .with_visibility(Duration::from_secs(10), Duration::ZERO);
        // The tracker's own client, as the CLI binds it (spec §1.1).
        let ledger = GithubLedger::new(tracker.client(), tracker.repo().clone(), &local)
            .with_lag(0, Duration::ZERO);
        let recorded = Recorded {
            inner: &ledger,
            batches: RefCell::new(Vec::new()),
        };
        let split = SplitLedger {
            local: &local,
            github: &recorded,
        };
        let checked = CatalogChecked {
            catalog: &local,
            tracker: &tracker,
        };
        let routed = KindRouted {
            catalog: &local,
            tracker: &tracker,
        };
        f(
            &Bound {
                catalog: &local,
                tracker: &checked,
                ledger: &split,
                handles: &routed,
            },
            &Controls {
                fake: &fake,
                remote: &recorded,
            },
        );
    }
}

impl Fixture for OverFake {
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
        self.with_split(&mut |b, _| f(b));
    }
}

// ⚠ Spec §8.2: the split ledger meets the shared contracts over the GitHub
// side itself, not only over `MemRemote`.
#[test]
fn a_split_ledger_over_github_meets_the_ledger_contracts() {
    fl_core::conformance::ledger(|| OverFake);
    fl_core::conformance::split_ledger(|| OverFake);
}

// ⚠ The shared lost-answer case cannot, by itself, tell a no-op
// `lose_next_answer` from a real one: a `GithubLedger` whose single try
// simply lands cleanly ends in the same state (the run published once) as
// one that lost the answer and found its own commit on a reread. This
// checks the mechanism directly: the control sets the fake's one-shot
// flag, and a commit that follows consumes it.
#[test]
fn lose_next_answer_sets_the_fakes_flag_and_a_commit_consumes_it() {
    let fake = FakeGithub::start("acme/widgets");
    let root = fake.seed_ledger();
    let local = MemStore::default();
    local
        .set_ledger_root("R_1", &root)
        .expect("a fresh store records a root");
    let tracker = GithubTracker::open(client(&fake), "acme/widgets", &local)
        .expect("the fake's repository opens")
        .0
        .with_visibility(Duration::from_secs(10), Duration::ZERO);
    let ledger = GithubLedger::new(tracker.client(), tracker.repo().clone(), &local)
        .with_lag(0, Duration::ZERO);
    let recorded = Recorded {
        inner: &ledger,
        batches: RefCell::new(Vec::new()),
    };
    let ctl = Controls {
        fake: &fake,
        remote: &recorded,
    };
    assert!(!fake.state().hang_up_after_next_commit, "nothing lost yet");
    ctl.lose_next_answer();
    assert!(
        fake.state().hang_up_after_next_commit,
        "the control set the fake's one-shot flag"
    );
    let gate = GateId(entry_iri(100));
    let record = RecordId(
        Iri::parse("https://github.com/acme/widgets/issues/1").expect("an issue URL is an IRI"),
    );
    let run = sample_record_run(1, &gate, Some(&record));
    let decision = sample_decision(1, &record, vec![run.id.clone().unwrap()]);
    recorded
        .publish(&Batch {
            decision,
            runs: vec![run],
            attempts: vec![],
        })
        .expect("the retry after the lost answer still lands");
    assert!(
        !fake.state().hang_up_after_next_commit,
        "a commit consumed the one-shot flag"
    );
}
