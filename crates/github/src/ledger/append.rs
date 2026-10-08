//! Appending to the `fl/ledger` branch (GitHub ledger spec §3.2): read and
//! check, add only what is missing, commit on the head that was read, and
//! read again when the head moved or an answer was lost.

use super::GithubLedger;
use super::disclose;
use super::layout::{self, Area, BRANCH, Line, QUARANTINE_FILE};
use crate::client::GraphqlAnswer;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fl_core::StoreError;
use fl_core::iri::Iri;
use fl_core::split::Batch;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// How many times an append reads the ledger and tries to land (spec §3.2
/// step 5: "up to five tries").
pub const TRIES: u32 = 5;

/// ⚠ Modelled from GitHub's documentation: `createCommitOnBranch` lands a
/// signed commit only on `expectedHeadOid`. Confirmed by live test
/// `create_commit_on_branch_is_refused_when_the_head_moved`.
const APPEND: &str = "mutation ledgerAppend($input: CreateCommitOnBranchInput!) { \
    createCommitOnBranch(input: $input) { commit { oid } } }";

const HEAD_MOVED: &str = "someone else appended first";

/// The permission an append needs (spec §6.3), named when GitHub refuses
/// one for want of it.
const NEEDS: &str = "Appending to the ledger needs the credential's Contents: write permission";

/// One line to append, and the id that makes a retry safe.
pub(crate) struct NewLine {
    pub id: Iri,
    pub text: String,
}

/// What became of one commit.
#[derive(Debug, PartialEq, Eq)]
enum Landed {
    Commit(String),
    HeadMoved,
    /// The answer does not say: only a fresh read can tell.
    Unknown(String),
}

/// Whether `message` says the branch "is at" one commit "but expected"
/// another, each named by its full id — the moved-head refusal observed
/// live. Words that merely resemble it, with no ids, are not one.
fn names_two_heads(message: &str) -> bool {
    let is_oid = |w: &str| (40..=64).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_hexdigit());
    message.match_indices("is at ").any(|(i, _)| {
        let mut words = message[i + "is at ".len()..].split_whitespace();
        matches!(
            (words.next(), words.next(), words.next(), words.next()),
            (Some(at), Some("but"), Some("expected"), Some(exp))
                if is_oid(at) && is_oid(exp.trim_end_matches('.'))
        )
    })
}

/// What one answer to the commit means.
///
/// ⚠ Modelled: a stale `expectedHeadOid` is refused with an error of type
/// `STALE_DATA`, or one whose message says where the branch was expected
/// to point; a 5xx, or a 200 with no commit in it (a body that was not
/// JSON, or broke off), may hide a commit that landed. Confirmed by live
/// test `create_commit_on_branch_is_refused_when_the_head_moved`.
///
/// ⚠ Observed once live, on 2026-10-05, by the losing flush of
/// `two_flushes_racing_both_land`: a 200 carrying
/// `{"type": "FORBIDDEN", "path": ["createCommitOnBranch"], "message": "is at
/// 2f4befc022adbf7d97077306abea79d23d07e734 but expected
/// 89bef3b68d7ca1dc30eed0a98197473d70d7502c"}`. Read as a moved head, not a
/// missing permission. Why GitHub chose this shape over `STALE_DATA` is
/// inferred, not measured: the branch moved after GitHub's own check.
///
/// ⚠ Modelled: a missing permission can come back as a 200 carrying an
/// error of type `FORBIDDEN` rather than as a 403 naming it — refused, and
/// named (spec §6.3). Confirmed by live test
/// `create_commit_on_branch_without_contents_write_is_refused`.
fn judge(answer: GraphqlAnswer) -> Result<Landed, StoreError> {
    match answer.status {
        200..=299 => {}
        500..=599 => {
            return Ok(Landed::Unknown(format!(
                "GitHub answered {}",
                answer.status
            )));
        }
        // ⚠ Spec §6.3: only a 403 is a refusal for want of a permission,
        // named from `x-accepted-github-permissions` when GitHub sends it;
        // any other refusal (a 400, a 409, a 422) carries no such hint.
        s => {
            let said = answer.message.as_deref().unwrap_or("it gave no message");
            return Err(StoreError::Backend(if s == 403 {
                let needs = answer
                    .needs
                    .map(|n| format!("; GitHub says this needs: {n}"))
                    .unwrap_or_default();
                format!(
                    "GitHub answered 403 to fl's commit to the ledger ({said}{needs}), so \
                     nothing was published. {NEEDS}: grant it, then retry"
                )
            } else {
                format!(
                    "GitHub answered {s} to fl's commit to the ledger ({said}), so nothing was \
                     published"
                )
            }));
        }
    }
    if !answer.errors.is_empty() {
        // ⚠ Checked before FORBIDDEN below: a lost race has also come back
        // as FORBIDDEN, and is a moved head.
        let moved = answer.errors.iter().any(|e| {
            e.get("type").and_then(Value::as_str) == Some("STALE_DATA")
                || e.get("message").and_then(Value::as_str).is_some_and(|m| {
                    m.contains("Expected branch to point to") || names_two_heads(m)
                })
        });
        if moved {
            return Ok(Landed::HeadMoved);
        }
        let forbidden = answer
            .errors
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("FORBIDDEN"));
        // ⚠ Modelled from GitHub's documented error shape, with no live
        // test: a request that runs past GitHub's time limit is answered 200
        // with an error saying it "may be the result of a timeout", and the
        // commit may have landed; only a fresh read can tell (spec §3.2
        // step 5). Provoking it takes a request built to run past GitHub's
        // limit, which would abuse the API. A refusal for want of a
        // permission stays a refusal.
        let timed_out = answer.errors.iter().any(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .is_some_and(|m| m.to_ascii_lowercase().contains("timeout"))
        });
        if timed_out && !forbidden {
            return Ok(Landed::Unknown(format!(
                "GitHub answered fl's commit with a timeout ({})",
                Value::Array(answer.errors)
            )));
        }
        let errors = Value::Array(answer.errors);
        return Err(StoreError::Backend(if forbidden {
            format!(
                "GitHub refused fl's commit to the ledger for want of a permission ({errors}), \
                 so nothing was published. {NEEDS}: grant it, then retry"
            )
        } else {
            // ⚠ An error fl does not know may come with a commit that
            // landed: fl cannot tell, so it claims neither. Nothing is
            // marked published, and the next flush reads the ledger and
            // adds only what is missing.
            format!(
                "GitHub answered fl's commit to the ledger with an error ({errors}), so fl \
                 cannot tell whether it was published. Nothing is marked published: the next \
                 flush reads the ledger and adds only what is missing"
            )
        }));
    }
    match answer
        .data
        .as_ref()
        .and_then(|d| d.pointer("/createCommitOnBranch/commit/oid"))
        .and_then(Value::as_str)
    {
        Some(oid) => Ok(Landed::Commit(oid.to_string())),
        None => Ok(Landed::Unknown(
            "GitHub answered the commit with no commit id".into(),
        )),
    }
}

impl GithubLedger<'_> {
    /// Append the lines of each `(area, directory, lines)` and of
    /// `quarantine` in one commit, and return only once it landed (spec
    /// §3.2).
    ///
    /// Each try reads a fresh snapshot (checks 1–7), drops every line whose
    /// id the directory already holds — in ANY of its segments, not only the
    /// last (ruling 8) — or that `quarantine.jsonl` already holds, and
    /// commits the rest on the head it read. Nothing left: no commit, and
    /// the answer is `None` — or, after a try whose answer was lost, the
    /// head that holds the lines (ruling 13).
    pub(crate) fn append(
        &self,
        dirs: &[(Area, String, Vec<NewLine>)],
        quarantine: &[NewLine],
        headline: &str,
    ) -> Result<Option<String>, StoreError> {
        let wanted: Vec<String> = dirs.iter().map(|(_, d, _)| d.clone()).collect();
        let mut sent = false;
        let mut why: Vec<String> = Vec::new();
        for _ in 0..TRIES {
            let snap = self.snapshot(&wanted)?;
            let mut writes: Vec<(String, Vec<u8>)> = Vec::new();
            for (area, dir, lines) in dirs {
                let present: BTreeSet<Iri> = self
                    .lines(&snap, *area, dir)?
                    .iter()
                    .filter_map(|l| l.id().cloned())
                    .collect();
                let new: Vec<String> = lines
                    .iter()
                    .filter(|l| !present.contains(&l.id))
                    .map(|l| l.text.clone())
                    .collect();
                let segments: Vec<(u64, Vec<u8>)> = snap
                    .dirs
                    .get(dir)
                    .map(|segs| {
                        segs.iter()
                            .enumerate()
                            .map(|(i, s)| ((i + 1) as u64, s.bytes.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                for (n, text) in layout::plan_append(&segments, &new) {
                    writes.push((layout::segment_path(dir, n), text));
                }
            }
            if !quarantine.is_empty() {
                let present: BTreeSet<Iri> = self
                    .quarantine_lines(&snap)?
                    .into_iter()
                    .map(|q| q.id)
                    .collect();
                let mut bytes = snap.quarantine.clone();
                let before = bytes.len();
                for l in quarantine.iter().filter(|l| !present.contains(&l.id)) {
                    bytes.extend_from_slice(l.text.as_bytes());
                    bytes.push(b'\n');
                }
                if bytes.len() != before {
                    writes.push((QUARANTINE_FILE.to_string(), bytes));
                }
            }
            if writes.is_empty() {
                // `snapshot` already recorded `snap.head` as the last seen.
                return Ok(sent.then_some(snap.head));
            }
            sent = true;
            match self.commit(&snap.head, &writes, headline)? {
                Landed::Commit(oid) => {
                    // ⚠ Spec §3.2 step 6: the new head is the last seen —
                    // through `remember`, the one write that moves the last
                    // head, and only now that the commit landed. It names no
                    // segment: the answer carries no blob ids, and a cache
                    // entry under a guessed id would make the next read
                    // take the segment for altered. The next read downloads
                    // each grown segment and checks it against the copy
                    // `snapshot` cached, which it extends.
                    self.local.remember(&self.repo.node_id, &oid, &[])?;
                    return Ok(Some(oid));
                }
                Landed::HeadMoved => why.push(HEAD_MOVED.to_string()),
                Landed::Unknown(cause) => why.push(cause),
            }
        }
        let store = format!("the GitHub ledger of {}", self.repo.full_name);
        if why.iter().all(|w| w == HEAD_MOVED) {
            Err(StoreError::Contended {
                store,
                tries: TRIES,
            })
        } else {
            Err(StoreError::Unreachable {
                store,
                cause: format!(
                    "{TRIES} tries ended without an answer that said whether the append landed \
                     ({}); the next flush reads the ledger and adds only what is missing",
                    why.join("; ")
                ),
            })
        }
    }

    fn commit(
        &self,
        head: &str,
        writes: &[(String, Vec<u8>)],
        headline: &str,
    ) -> Result<Landed, StoreError> {
        let additions: Vec<Value> = writes
            .iter()
            .map(|(path, bytes)| json!({"path": path, "contents": STANDARD.encode(bytes)}))
            .collect();
        let input = json!({
            "branch": {"repositoryNameWithOwner": self.repo.full_name, "branchName": BRANCH},
            "message": {"headline": headline},
            "expectedHeadOid": head,
            "fileChanges": {"additions": additions},
        });
        // ⚠ `graphql_write`, not `graphql_answer`: a 200 whose body is not
        // JSON, or breaks off, comes back as an answer with no commit in it
        // — "unknown" — never as an error that would read as "it did not
        // land" and drop the entries; and every refusal comes back as an
        // answer for `judge`, which tells a 403 from any other.
        match self.client.graphql_write(APPEND, json!({"input": input})) {
            Ok(answer) => judge(answer),
            // ⚠ The request may have reached GitHub before the connection
            // broke (spec §3.2 step 5): only a fresh read can tell.
            Err(StoreError::Unreachable { cause, .. }) => Ok(Landed::Unknown(cause)),
            Err(e) => Err(e),
        }
    }

    /// `RemoteLedger::publish`: the batch's entries, projected for the
    /// repository's visibility (decision 2), each in its own directory, in
    /// one commit.
    pub(crate) fn publish_batch(&self, batch: &Batch) -> Result<Option<String>, StoreError> {
        // ⚠ An entry with no id cannot be de-duplicated, so it is never
        // published (spec §1.3). Refused before any request.
        if batch.runs.iter().any(|r| r.id.is_none())
            || batch.attempts.iter().any(|a| a.id.is_none())
        {
            return Err(StoreError::Backend(
                "fl was about to publish an entry with no id, which it never does; nothing was \
                 published. This is a defect in fl"
                    .into(),
            ));
        }
        let visibility = self.visibility()?;
        let by = self.identity()?;
        // ⚠ Decision 2: each run and attempt is projected for the
        // visibility BEFORE it is encoded — `Line::encode` withholds
        // nothing itself.
        let lines = batch
            .runs
            .iter()
            .map(|r| Line::Run(disclose::run(r, visibility)))
            .chain(
                batch
                    .attempts
                    .iter()
                    .map(|a| Line::Attempt(disclose::attempt(a, visibility))),
            )
            .chain(std::iter::once(Line::Decision(batch.decision.clone())));
        let mut grouped: BTreeMap<String, (Area, Vec<NewLine>)> = BTreeMap::new();
        for line in lines {
            let id = line
                .id()
                .cloned()
                .expect("checked above: every entry has an id");
            let text = line.encode(&by);
            grouped
                .entry(line.dir())
                .or_insert_with(|| (line.area(), Vec::new()))
                .1
                .push(NewLine { id, text });
        }
        let dirs: Vec<(Area, String, Vec<NewLine>)> = grouped
            .into_iter()
            .map(|(dir, (area, lines))| (area, dir, lines))
            .collect();
        let d = &batch.decision;
        self.append(&dirs, &[], &format!("fl: {} {}", d.kind().as_wire(), d.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, GraphqlAnswer};
    use crate::creds::EnvToken;
    use crate::fake::{FakeGithub, USER_LOGIN};
    use crate::ledger::Visibility;
    use crate::ledger::layout::{QuarantineLine, SEGMENT_LIMIT, decode};
    use crate::tracker::Repo;
    use fl_core::MemStore;
    use fl_core::at::At;
    use fl_core::conformance::{sample_attempt, sample_decision, sample_record_run};
    use fl_core::decision::Outcome;
    use fl_core::ids::{GateId, ProjectId, RecordId, seq_iri};
    use fl_core::log::{Attempt, GateRun, PathsTouched};
    use fl_core::model::State;
    use fl_core::split::{LedgerCache, RemoteLedger};
    use fl_core::store::Bindings;
    use fl_core::verdict::Verdict;
    use std::time::Duration;

    fn client_at(url: &str) -> Client {
        Client::new(
            url,
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    fn client(fake: &FakeGithub) -> Client {
        client_at(&fake.url())
    }

    fn repo() -> Repo {
        Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        }
    }

    /// A ledger started on the fake, and a machine that records its anchor.
    fn world() -> (FakeGithub, MemStore, String) {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        (fake, local, root)
    }

    fn open<'a>(c: &'a Client, local: &'a MemStore) -> GithubLedger<'a> {
        GithubLedger::new(c, repo(), local).with_lag(2, Duration::ZERO)
    }

    fn gate() -> GateId {
        GateId(seq_iri(7))
    }

    fn project() -> ProjectId {
        ProjectId(seq_iri(8))
    }

    fn record() -> RecordId {
        RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap())
    }

    fn run(n: u64) -> GateRun {
        sample_record_run(n, &gate(), Some(&record()))
    }

    fn attempt(n: u64) -> Attempt {
        sample_attempt(n, &project(), &record())
    }

    /// A `check` decision `n`, resting on `runs` and `attempts`.
    fn batch(n: u64, runs: Vec<GateRun>, attempts: Vec<Attempt>) -> Batch {
        let rests_on = runs
            .iter()
            .filter_map(|r| r.id.clone())
            .chain(attempts.iter().filter_map(|a| a.id.clone()))
            .collect();
        Batch {
            decision: sample_decision(n, &record(), rests_on),
            runs,
            attempts,
        }
    }

    fn runs_dir() -> String {
        layout::dir(Area::Runs, gate().iri())
    }

    fn files_under(fake: &FakeGithub, dir: &str) -> Vec<(String, String)> {
        let prefix = format!("{dir}/");
        fake.ledger_files()
            .into_iter()
            .filter(|(p, _)| p.starts_with(&prefix))
            .collect()
    }

    #[test]
    fn a_publish_files_each_entry_in_its_own_directory_and_names_its_writer() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let b = batch(1, vec![run(1)], vec![attempt(2)]);
        let commit = l.publish(&b).unwrap();
        assert_eq!(commit, fake.ledger_head());
        let files = fake.ledger_files();
        for (area, subject, line) in [
            (Area::Runs, gate().iri().clone(), Line::Run(run(1))),
            (
                Area::Attempts,
                project().iri().clone(),
                Line::Attempt(attempt(2)),
            ),
            (
                Area::Decisions,
                record().iri().clone(),
                Line::Decision(b.decision.clone()),
            ),
        ] {
            let path = layout::segment_path(&layout::dir(area, &subject), 1);
            let text = files
                .get(&path)
                .unwrap_or_else(|| panic!("{path} in {:?}", files.keys()));
            assert_eq!(
                decode(area, text.trim_end()),
                Ok((line, USER_LOGIN.to_string()))
            );
        }
        assert_eq!(
            local.last_head("R_1").unwrap(),
            commit,
            "the new head is the last seen"
        );
    }

    // Spec §3.2 step 5: nothing left to add, no commit.
    #[test]
    fn publishing_what_is_already_there_makes_no_commit() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let b = batch(1, vec![run(1)], vec![]);
        open(&c, &local).publish(&b).unwrap();
        let commits = fake.ledger_commits();
        assert_eq!(open(&c, &local).publish(&b).unwrap(), None);
        assert_eq!(fake.ledger_commits(), commits);
    }

    // ⚠ Spec §3.2 (Invariant): two machines cannot both land a commit on one
    // head; the one refused reads again and adds only what is missing.
    #[test]
    fn two_flushes_racing_both_land_and_none_is_lost() {
        let (fake, local, _root) = world();
        let theirs = Line::Run(run(5)).encode("another-machine");
        fake.state()
            .foreign_appends
            .push((layout::segment_path(&runs_dir(), 1), theirs));
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        assert_eq!(
            l.runs(&gate()).unwrap(),
            vec![run(5), run(1)],
            "theirs, then mine; each once"
        );
        assert_eq!(fake.ledger_commits(), 3, "the start, theirs, mine");
    }

    // ⚠ The same invariant when GitHub reports the lost race as FORBIDDEN,
    // the shape observed live on 2026-10-05: it is a moved head, never a
    // missing permission.
    #[test]
    fn a_race_lost_as_forbidden_is_read_again_and_lands() {
        let (fake, local, _root) = world();
        let theirs = Line::Run(run(5)).encode("another-machine");
        {
            let mut s = fake.state();
            s.foreign_appends
                .push((layout::segment_path(&runs_dir(), 1), theirs));
            s.lose_next_race_as_forbidden = true;
        }
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        assert_eq!(
            l.runs(&gate()).unwrap(),
            vec![run(5), run(1)],
            "theirs, then mine; each once"
        );
        assert_eq!(fake.ledger_commits(), 3, "the start, theirs, mine");
    }

    // ⚠ The same invariant with two real machines on two threads.
    #[test]
    fn two_machines_appending_at_once_all_land_once() {
        const EACH: u64 = 4;
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let url = fake.url();
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|s| {
            for machine in 1..=2u64 {
                let (url, root, start) = (url.clone(), root.clone(), &start);
                s.spawn(move || {
                    let local = MemStore::default();
                    local.set_ledger_root("R_1", &root).unwrap();
                    let c = client_at(&url);
                    let l = GithubLedger::new(&c, repo(), &local).with_lag(3, Duration::ZERO);
                    start.wait();
                    for i in 0..EACH {
                        let b = batch(100 * machine + i, vec![], vec![]);
                        // `Contended` means someone else landed each time and
                        // nothing was lost: this machine tries again.
                        loop {
                            match l.publish(&b) {
                                Ok(_) => break,
                                Err(StoreError::Contended { .. }) => continue,
                                Err(e) => panic!("machine {machine}: {e}"),
                            }
                        }
                    }
                });
            }
        });
        let c = client(&fake);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let mut got: Vec<Iri> = open(&c, &local)
            .decisions(record().iri())
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        got.sort();
        let mut want: Vec<Iri> = (1..=2u64)
            .flat_map(|m| {
                (0..EACH).map(move |i| sample_decision(100 * m + i, &record(), vec![]).id)
            })
            .collect();
        want.sort();
        assert_eq!(got, want, "every decision once, none lost");
        assert_eq!(
            fake.ledger_commits(),
            1 + 2 * EACH as usize,
            "no empty commit"
        );
    }

    // ⚠ Spec §3.2 step 5 and §8.3: a timeout, then a retry — no duplicate
    // and no empty commit; the head that holds the lines comes back.
    #[test]
    fn a_lost_answer_is_read_again_and_nothing_is_added_twice_or_committed_empty() {
        let (fake, local, _root) = world();
        fake.state().hang_up_after_next_commit = true;
        let c = client(&fake);
        let l = open(&c, &local);
        let b = batch(1, vec![run(1)], vec![]);
        let got = l.publish(&b).unwrap();
        assert_eq!(got, fake.ledger_head(), "the head that holds the lines");
        assert_eq!(
            fake.ledger_commits(),
            2,
            "one commit, and no empty one after it"
        );
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
        assert_eq!(l.decisions(record().iri()).unwrap(), vec![b.decision]);
    }

    // ⚠ A 200 whose body is not JSON, after the commit landed: the answer
    // says nothing about whether it landed, so it is neither a success nor
    // a clean failure — the append reads again and adds only what is
    // missing.
    #[test]
    fn a_commit_answered_with_a_body_that_is_not_json_is_read_again_and_lands_once() {
        let (fake, local, _root) = world();
        fake.state().garble_next_commit_answer = true;
        let c = client(&fake);
        let l = open(&c, &local);
        let b = batch(1, vec![run(1)], vec![attempt(2)]);
        assert_eq!(l.publish(&b).unwrap(), fake.ledger_head());
        assert_eq!(
            fake.ledger_commits(),
            2,
            "one commit, none empty, none twice"
        );
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
        assert_eq!(l.attempts_of(&project()).unwrap(), vec![attempt(2)]);
        assert_eq!(l.decisions(record().iri()).unwrap(), vec![b.decision]);
    }

    // ⚠ The same with a 200 whose body breaks off part way.
    #[test]
    fn a_commit_answered_with_a_body_that_breaks_off_is_read_again_and_lands_once() {
        let (fake, local, _root) = world();
        fake.state().break_next_commit_answer = true;
        let c = client(&fake);
        let l = open(&c, &local);
        let b = batch(1, vec![run(1)], vec![]);
        assert_eq!(l.publish(&b).unwrap(), fake.ledger_head());
        assert_eq!(
            fake.ledger_commits(),
            2,
            "one commit, none empty, none twice"
        );
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
        assert_eq!(l.decisions(record().iri()).unwrap(), vec![b.decision]);
    }

    // ⚠ Spec §3.2 step 5: a timeout is read again. The commit landed, so
    // nothing is added twice and no empty commit follows.
    #[test]
    fn a_commit_answered_with_a_timeout_is_read_again_and_lands_once() {
        let (fake, local, _root) = world();
        fake.state().timeout_after_next_commit = true;
        let c = client(&fake);
        let l = open(&c, &local);
        let b = batch(1, vec![run(1)], vec![]);
        assert_eq!(l.publish(&b).unwrap(), fake.ledger_head());
        assert_eq!(
            fake.ledger_commits(),
            2,
            "one commit, none empty, none twice"
        );
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
    }

    // ⚠ Ruling 8: a lost answer whose commit rolled a segment over — the
    // retry finds the lines in the closed segment as well as the open one.
    #[test]
    fn a_lost_answer_after_a_rollover_adds_nothing_twice() {
        let (fake, local, _root) = world();
        let (a, b) = (run(1), run(2));
        let a_len = Line::Run(a.clone()).encode(USER_LOGIN).len() + 1;
        // A filler run whose line leaves room for `a`'s and no more.
        let mut filler = run(9);
        filler.output_excerpt = Some(String::new());
        let base = Line::Run(filler.clone()).encode("x").len() + 1;
        filler.output_excerpt = Some("f".repeat(SEGMENT_LIMIT - a_len - base));
        let first = format!("{}\n", Line::Run(filler.clone()).encode("x"));
        assert_eq!(first.len(), SEGMENT_LIMIT - a_len);
        let seg1 = layout::segment_path(&runs_dir(), 1);
        fake.hand_commit(&[(seg1.as_str(), Some(first.as_str()))]);
        fake.state().hang_up_after_next_commit = true;
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![a.clone(), b.clone()], vec![]))
            .unwrap();
        let segs = files_under(&fake, &runs_dir());
        assert_eq!(segs.len(), 2, "`b` rolled into a second segment");
        let lines: usize = segs
            .iter()
            .map(|(_, t)| layout::lines(t.as_bytes()).len())
            .sum();
        assert_eq!(lines, 3, "the filler, `a` and `b`, each once");
        assert_eq!(
            fake.ledger_commits(),
            3,
            "the start, the filler, one append"
        );
        assert_eq!(l.runs(&gate()).unwrap(), vec![filler, a, b]);
    }

    #[test]
    fn a_commit_that_did_not_land_is_sent_again() {
        let (fake, local, _root) = world();
        fake.state().fail_commits = 1;
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        assert_eq!(fake.ledger_commits(), 2);
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
    }

    // Ruling 14: five tries that each found the head moved is `Contended` —
    // transient, and nothing is lost.
    #[test]
    fn a_head_that_keeps_moving_is_contended_and_nothing_is_lost() {
        let (fake, local, _root) = world();
        for n in 0..u64::from(TRIES) {
            fake.state().foreign_appends.push((
                layout::segment_path(&runs_dir(), 1),
                Line::Run(run(10 + n)).encode("another-machine"),
            ));
        }
        let c = client(&fake);
        let l = open(&c, &local);
        let b = batch(1, vec![run(1)], vec![]);
        let err = l.publish(&b).unwrap_err();
        assert!(
            matches!(err, StoreError::Contended { tries: TRIES, .. }),
            "{err:?}"
        );
        assert!(err.is_transient());
        assert_eq!(
            l.runs(&gate()).unwrap().len(),
            TRIES as usize,
            "all of theirs"
        );
        l.publish(&b).unwrap();
        assert_eq!(l.runs(&gate()).unwrap().len(), TRIES as usize + 1);
    }

    // Ruling 14: five answers that never said whether the append landed is
    // `Unreachable` — transient, and nothing landed twice.
    #[test]
    fn answers_that_never_say_whether_the_append_landed_are_unreachable() {
        let (fake, local, _root) = world();
        fake.state().fail_commits = TRIES;
        let c = client(&fake);
        let l = open(&c, &local);
        let err = l.publish(&batch(1, vec![run(1)], vec![])).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert!(err.is_transient());
        assert_eq!(fake.ledger_commits(), 1, "nothing landed");
    }

    // A commit answered 5xx says nothing about whether it landed, whatever
    // the size of the body the 5xx carried: the try is `Unknown`, and the next
    // reads the ledger before it adds anything (spec §3.2).
    #[test]
    fn a_commit_answered_5xx_with_a_body_over_the_cap_is_unknown() {
        let (fake, local, _root) = world();
        fake.state().gzip_bomb_next = Some(("createCommitOnBranch".into(), 502));
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        assert_eq!(
            l.runs(&gate()).unwrap().len(),
            1,
            "landed once, on the retry"
        );
        assert_eq!(fake.ledger_commits(), 2, "the start and the retry");
    }

    // A commit refused with a 403 keeps its own refusal when the body of the
    // refusal is over the cap: nothing was published, and it is not transient.
    #[test]
    fn a_commit_answered_403_with_a_body_over_the_cap_is_the_permission_refusal() {
        let (fake, local, _root) = world();
        fake.state().gzip_bomb_next = Some(("createCommitOnBranch".into(), 403));
        let c = client(&fake);
        let err = open(&c, &local)
            .publish(&batch(1, vec![], vec![]))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("403") && msg.contains("nothing was published"),
            "{msg}"
        );
        assert!(msg.contains("grant it"), "{msg}");
        assert!(!msg.contains("larger than"), "{msg}");
        assert!(
            matches!(err, StoreError::Backend(_)) && !err.is_transient(),
            "{err:?}"
        );
        assert_eq!(fake.ledger_commits(), 1);
    }

    // ⚠ Ruling 14, the boundary: `Contended` only when EVERY try found the
    // head moved. Four that did and one answer that said nothing is
    // `Unreachable` — the one lost answer may hide a commit.
    #[test]
    fn tries_whose_causes_are_mixed_are_unreachable_not_contended() {
        let (fake, local, _root) = world();
        for n in 0..u64::from(TRIES) - 1 {
            fake.state().foreign_appends.push((
                layout::segment_path(&runs_dir(), 1),
                Line::Run(run(10 + n)).encode("another-machine"),
            ));
        }
        // Not consumed by a stale commit: it answers the fifth try.
        fake.state().fail_commits = 1;
        let c = client(&fake);
        let l = open(&c, &local);
        let err = l.publish(&batch(1, vec![run(1)], vec![])).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert!(err.to_string().contains(HEAD_MOVED), "{err}");
        assert!(err.to_string().contains("502"), "{err}");
        assert_eq!(
            fake.ledger_commits(),
            TRIES as usize,
            "the start and their four; none of mine"
        );
    }

    // Spec §6.3: a credential without Contents: write is found by the first
    // flush, whose error names it — not a retry. The first shape: an HTTP
    // 403 naming the permission in `x-accepted-github-permissions`. The
    // knob is one-shot, so a retry would have landed: one commit proves
    // there was none.
    #[test]
    fn a_commit_refused_for_want_of_a_permission_refuses_and_names_it() {
        let (fake, local, _root) = world();
        fake.state().refuse_next_commit_for = Some("contents=write".into());
        let c = client(&fake);
        let err = open(&c, &local)
            .publish(&batch(1, vec![], vec![]))
            .unwrap_err();
        assert!(err.to_string().contains("contents=write"), "{err}");
        assert!(err.to_string().contains("Contents: write"), "{err}");
        assert!(!err.is_transient());
        assert_eq!(fake.ledger_commits(), 1);
    }

    // Spec §6.3, the second shape: an HTTP 200 carrying a GraphQL error of
    // type FORBIDDEN, with no header to name the permission — the refusal
    // names the one an append needs.
    #[test]
    fn a_commit_refused_as_forbidden_refuses_and_names_the_permission_an_append_needs() {
        let (fake, local, _root) = world();
        fake.state().refuse_next_commit_as_forbidden = true;
        let c = client(&fake);
        let err = open(&c, &local)
            .publish(&batch(1, vec![run(1)], vec![]))
            .unwrap_err();
        assert!(err.to_string().contains("Contents: write"), "{err}");
        assert!(!err.is_transient());
        assert_eq!(fake.ledger_commits(), 1, "refused, and not retried");
    }

    // Spec §7: a spent rate limit refuses the decision; it is not an
    // ambiguous answer to read again.
    #[test]
    fn a_rate_limited_commit_is_refused_and_not_retried() {
        let (fake, local, _root) = world();
        fake.state().rate_limit_next_commit = true;
        let c = client(&fake);
        let err = open(&c, &local)
            .publish(&batch(1, vec![run(1)], vec![]))
            .unwrap_err();
        assert!(matches!(err, StoreError::RateLimited { .. }), "{err:?}");
        assert_eq!(fake.ledger_commits(), 1, "not retried");
    }

    // ⚠ Spec §3.5 check 2 (ruling 24): right after this machine's own
    // append, a replica may answer the branch with the head before it, or
    // not know this machine's commit in a compare. Neither is an alarm.
    #[test]
    fn a_lagging_replica_after_this_machines_own_append_raises_no_alarm() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        fake.state().ref_behind_next = 1;
        assert_eq!(
            l.runs(&gate()).unwrap(),
            vec![run(1)],
            "the branch read answered the head before this machine's own"
        );
        // Another machine appends after this one.
        let seg1 = layout::segment_path(&runs_dir(), 1);
        let theirs = format!(
            "{}{}\n",
            fake.ledger_files()[&seg1],
            Line::Run(run(5)).encode("another-machine")
        );
        fake.hand_commit(&[(seg1.as_str(), Some(theirs.as_str()))]);
        fake.state().compare_unknown_next = 1;
        assert_eq!(
            l.runs(&gate()).unwrap(),
            vec![run(1), run(5)],
            "a compare whose replica did not know this machine's commit yet"
        );
    }

    // ⚠ Ruling 24, the other two shapes: right after this machine's own
    // append, a branch read may answer 404 and a GraphQL read may not know
    // the new head. Neither is an alarm — on a read, or on the next append.
    #[test]
    fn a_replica_that_has_not_learned_of_this_machines_own_append_is_read_again() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        fake.state().ref_404_next = 1;
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)], "a branch read: 404");
        l.publish(&batch(2, vec![run(2)], vec![])).unwrap();
        fake.state().graphql_commit_unknown_next = 1;
        assert_eq!(
            l.runs(&gate()).unwrap(),
            vec![run(1), run(2)],
            "a GraphQL read that does not know the new head"
        );
        fake.state().ref_behind_next = 1;
        fake.state().graphql_commit_unknown_next = 1;
        l.publish(&batch(3, vec![run(3)], vec![])).unwrap();
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1), run(2), run(3)]);
        assert_eq!(fake.ledger_commits(), 4, "the start and three appends");
    }

    // Spec §3.1: segments roll over, and a directory of many reads back
    // whole, even on a machine that never read it.
    #[test]
    fn segments_roll_over_and_a_directory_of_many_reads_back_whole() {
        let (fake, local, root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let mut published = Vec::new();
        for n in 1..=12u64 {
            let mut r = run(n);
            r.output_excerpt = Some("e".repeat(60 * 1024));
            l.publish(&batch(n, vec![r.clone()], vec![])).unwrap();
            published.push(r);
        }
        let segs = files_under(&fake, &runs_dir());
        assert!(segs.len() >= 3, "{} segments", segs.len());
        assert!(segs.iter().all(|(_, t)| t.len() <= SEGMENT_LIMIT));
        let fresh = MemStore::default();
        fresh.set_ledger_root("R_1", &root).unwrap();
        assert_eq!(open(&c, &fresh).runs(&gate()).unwrap(), published);
    }

    #[test]
    fn a_private_repository_publishes_the_excerpts() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![attempt(2)]))
            .unwrap();
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
        assert_eq!(l.attempts_of(&project()).unwrap(), vec![attempt(2)]);
    }

    // ⚠ Decision 2 and spec §8.3: on a repository that is not private — or
    // whose answer names no visibility at all (ruling 21: the safe side,
    // since a published excerpt cannot be taken back) — nothing published,
    // in a file or a commit headline, holds an excerpt, an error's detail,
    // or a path.
    #[test]
    fn a_repository_that_is_not_private_publishes_nothing_machine_specific() {
        let secret_path = "/home/someone/work/app/src/a.rs";
        let secret_host = "build-host-7";
        let home = std::env::var("HOME").ok().filter(|h| h.len() > 1);
        for case in ["public", "internal", "omits visibility"] {
            let (fake, local, _root) = world();
            if case == "omits visibility" {
                fake.state().omit_visibility = true;
            } else {
                fake.state().repos[0].visibility = case.into();
            }
            let mut r = run(1);
            r.output_excerpt = Some(format!("{secret_path} on {secret_host}"));
            let mut errored = run(2);
            errored.output_excerpt = Some(format!("{secret_path} on {secret_host}"));
            errored.verdict =
                Verdict::error(format!("could not spawn {secret_path} on {secret_host}"));
            let mut a = attempt(3);
            a.output_excerpt = Some(format!("{secret_path} on {secret_host}"));
            a.paths_touched = PathsTouched::Listed(vec![secret_path.into()]);
            let c = client(&fake);
            let l = open(&c, &local);
            l.publish(&batch(1, vec![r.clone(), errored.clone()], vec![a.clone()]))
                .unwrap();
            assert_eq!(l.visibility().unwrap(), Visibility::NotPrivate, "{case}");
            let secrets: Vec<String> = [
                Some(secret_path.to_string()),
                Some(secret_host.to_string()),
                home.clone(),
            ]
            .into_iter()
            .flatten()
            .collect();
            for (path, text) in fake.ledger_files() {
                for secret in &secrets {
                    assert!(
                        !text.contains(secret.as_str()),
                        "{case}: `{path}` holds `{secret}`"
                    );
                }
            }
            for message in fake.ledger_commit_messages() {
                for secret in &secrets {
                    assert!(
                        !message.contains(secret.as_str()),
                        "{case}: commit `{message}` holds `{secret}`"
                    );
                }
            }
            assert_eq!(
                l.runs(&gate()).unwrap(),
                vec![
                    disclose::run(&r, Visibility::NotPrivate),
                    disclose::run(&errored, Visibility::NotPrivate),
                ],
                "{case}"
            );
            assert_eq!(
                l.attempts_of(&project()).unwrap(),
                vec![disclose::attempt(&a, Visibility::NotPrivate)],
                "{case}"
            );
        }
    }

    // ⚠ Entries already on the branch from before the repository went
    // public are never rewritten; only what a later publish newly adds is
    // projected for the visibility read at that time.
    #[test]
    fn a_batch_published_while_private_is_not_added_again_once_public() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let b = batch(1, vec![run(1)], vec![attempt(2)]);
        open(&c, &local).publish(&b).unwrap();
        let commits = fake.ledger_commits();
        fake.state().repos[0].visibility = "public".into();
        let l = open(&c, &local);
        assert_eq!(l.visibility().unwrap(), Visibility::NotPrivate);
        assert_eq!(l.publish(&b).unwrap(), None, "nothing new: no commit");
        assert_eq!(fake.ledger_commits(), commits);
        let mut extra = run(9);
        extra.output_excerpt = Some("brand new".into());
        l.publish(&batch(2, vec![run(1), extra.clone()], vec![attempt(2)]))
            .unwrap();
        assert_eq!(
            fake.ledger_commits(),
            commits + 1,
            "one commit, for the new run only"
        );
        assert_eq!(
            l.runs(&gate()).unwrap(),
            vec![run(1), disclose::run(&extra, Visibility::NotPrivate)],
            "the old run is as it was written; only the new one is projected"
        );
    }

    // Decision 2: a visibility that cannot be read is an error, never
    // "private"; nothing is published.
    #[test]
    fn a_visibility_that_cannot_be_read_refuses_and_publishes_nothing() {
        let (fake, local, _root) = world();
        fake.state().fail_repo_read = true;
        let c = client(&fake);
        let err = open(&c, &local)
            .publish(&batch(1, vec![run(1)], vec![]))
            .unwrap_err();
        assert!(err.to_string().contains("visibility"), "{err}");
        assert!(
            matches!(err, StoreError::Unreachable { .. }),
            "a 5xx is transient: {err:?}"
        );
        assert!(err.is_transient());
        // Any other status is an error that retrying does not fix.
        fake.state().body_next.push((
            "/repos/acme/widgets".into(),
            404,
            json!({"message": "Not Found"}),
        ));
        let err = open(&c, &local)
            .publish(&batch(1, vec![run(1)], vec![]))
            .unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains("visibility"), "{err}");
        assert!(!err.is_transient());
        assert_eq!(fake.ledger_commits(), 1);
    }

    // Spec §5, ruling 21: read once per ledger, which lives for one command.
    #[test]
    fn the_visibility_is_read_once_per_ledger() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        l.publish(&batch(2, vec![run(2)], vec![])).unwrap();
        let reads = fake
            .state()
            .requests
            .iter()
            .filter(|r| r.as_str() == "GET /repos/acme/widgets")
            .count();
        assert_eq!(reads, 1);
    }

    // Ruling 18: `by` is asked of GitHub once per ledger.
    #[test]
    fn the_writer_is_asked_once_per_ledger() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&batch(1, vec![run(1)], vec![])).unwrap();
        l.publish(&batch(2, vec![run(2)], vec![])).unwrap();
        let asked = fake
            .state()
            .requests
            .iter()
            .filter(|r| r.as_str() == "GET /user")
            .count();
        assert_eq!(asked, 1);
    }

    // Spec §1.3: an entry with no id is never published.
    #[test]
    fn an_entry_without_an_id_is_refused_before_anything_is_appended() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let mut headless = run(1);
        headless.id = None;
        let err = open(&c, &local)
            .publish(&batch(1, vec![headless], vec![]))
            .unwrap_err();
        assert!(err.to_string().contains("no id"), "{err}");
        let mut headless = attempt(2);
        headless.id = None;
        let err = open(&c, &local)
            .publish(&batch(2, vec![], vec![headless]))
            .unwrap_err();
        assert!(err.to_string().contains("no id"), "{err}");
        assert_eq!(fake.ledger_commits(), 1);
    }

    // Decision 11: a refused decision is published like any other.
    #[test]
    fn a_refused_decision_is_published_and_reads_back() {
        let (fake, local, _root) = world();
        let mut b = batch(1, vec![run(1)], vec![]);
        b.decision.outcome = Outcome::Move {
            from: State::Review,
            to: State::Done,
            transitions: vec![],
            allowed: false,
        };
        let c = client(&fake);
        let l = open(&c, &local);
        l.publish(&b).unwrap();
        assert_eq!(l.decisions(record().iri()).unwrap(), vec![b.decision]);
    }

    // Spec §3.6: a quarantine line is appended once; a retry of the same
    // line adds nothing and makes no commit.
    #[test]
    fn a_quarantine_line_is_appended_once() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let q = QuarantineLine {
            id: seq_iri(50),
            at: At::from_unix_millis(50),
            file: layout::segment_path(&runs_dir(), 1),
            line: 1,
            quarantined_by: "someone".into(),
            reason: "a hand edit".into(),
            by: USER_LOGIN.into(),
        };
        let new = || NewLine {
            id: q.id.clone(),
            text: q.encode(),
        };
        assert!(l.append(&[], &[new()], "fl: quarantine").unwrap().is_some());
        let commits = fake.ledger_commits();
        assert_eq!(l.append(&[], &[new()], "fl: quarantine").unwrap(), None);
        assert_eq!(fake.ledger_commits(), commits, "no empty commit");
        assert_eq!(
            fake.ledger_files()[QUARANTINE_FILE],
            format!("{}\n", q.encode()),
            "the line once"
        );
    }

    fn answer(
        status: u16,
        data: Option<serde_json::Value>,
        errors: Vec<serde_json::Value>,
    ) -> GraphqlAnswer {
        GraphqlAnswer {
            status,
            data,
            errors,
            message: None,
            needs: None,
        }
    }

    /// A refusal: `status`, GitHub's message, and what it says was needed.
    fn refusal(status: u16, needs: Option<&str>) -> GraphqlAnswer {
        GraphqlAnswer {
            message: Some("Resource not accessible by integration".into()),
            needs: needs.map(str::to_string),
            ..answer(status, None, vec![])
        }
    }

    // ⚠ Modelled: what each answer to `createCommitOnBranch` means.
    // Confirmed by live test `create_commit_on_branch_is_refused_when_the_head_moved`.
    #[test]
    fn each_answer_to_a_commit_is_judged_once() {
        let landed = judge(answer(
            200,
            Some(json!({"createCommitOnBranch": {"commit": {"oid": "c1"}}})),
            vec![],
        ));
        assert_eq!(landed.unwrap(), Landed::Commit("c1".into()));
        for e in [
            json!({"type": "STALE_DATA"}),
            json!({"message": "Expected branch to point to \"c0\" but it did not."}),
            // The shape a lost race took live on 2026-10-05.
            json!({
                "type": "FORBIDDEN",
                "path": ["createCommitOnBranch"],
                "message": "is at 2f4befc022adbf7d97077306abea79d23d07e734 but expected \
                            89bef3b68d7ca1dc30eed0a98197473d70d7502c",
            }),
        ] {
            assert_eq!(
                judge(answer(200, None, vec![e])).unwrap(),
                Landed::HeadMoved
            );
        }
        assert!(matches!(
            judge(answer(502, None, vec![])).unwrap(),
            Landed::Unknown(_)
        ));
        assert!(matches!(
            judge(answer(200, Some(json!({})), vec![])).unwrap(),
            Landed::Unknown(_)
        ));
        // A 200 whose body could not be read at all: no data, no errors.
        assert!(matches!(
            judge(answer(200, None, vec![])).unwrap(),
            Landed::Unknown(_)
        ));
        // An error fl does not know: an error, but one that claims nothing
        // about whether the commit landed.
        let unknown = judge(answer(200, None, vec![json!({"type": "NOT_FOUND"})])).unwrap_err();
        assert!(matches!(unknown, StoreError::Backend(_)), "{unknown:?}");
        assert!(!unknown.is_transient());
        let said = unknown.to_string();
        assert!(!said.contains("Contents: write"), "{said}");
        assert!(!said.contains("nothing was published"), "{said}");
        assert!(
            said.contains("cannot tell whether it was published"),
            "{said}"
        );
        // Spec §6.3: a 403 names the permission GitHub says it needed, and
        // the one an append needs.
        let refused = judge(refusal(403, Some("contents=write"))).unwrap_err();
        assert!(!refused.is_transient());
        assert!(refused.to_string().contains("contents=write"), "{refused}");
        assert!(refused.to_string().contains("Contents: write"), "{refused}");
        assert!(
            refused.to_string().contains("Resource not accessible"),
            "GitHub's own message is kept: {refused}"
        );
        // Any other refusal carries no permission hint.
        for status in [400, 409, 422] {
            let refused = judge(refusal(status, None)).unwrap_err();
            assert!(matches!(refused, StoreError::Backend(_)), "{refused:?}");
            assert!(
                !refused.to_string().contains("Contents: write"),
                "{refused}"
            );
        }
        // A status that is neither a success nor a server error says the
        // commit was refused; it does not hide one that landed.
        let refused = judge(answer(404, None, vec![])).unwrap_err();
        assert!(matches!(refused, StoreError::Backend(_)), "{refused:?}");
        // Spec §6.3: a FORBIDDEN error is a missing permission, named.
        let refused = judge(answer(
            200,
            None,
            vec![json!({"type": "FORBIDDEN", "message": "Resource not accessible"})],
        ))
        .unwrap_err();
        assert!(matches!(refused, StoreError::Backend(_)), "{refused:?}");
        assert!(refused.to_string().contains("Contents: write"), "{refused}");
        // A moved head is read only from two commit ids: words that merely
        // look like one are not a race.
        let refused = judge(answer(
            200,
            None,
            vec![json!({"type": "FORBIDDEN", "message": "is at least 1 but expected 2"})],
        ))
        .unwrap_err();
        assert!(refused.to_string().contains("Contents: write"), "{refused}");
    }

    // A timeout says nothing about whether the commit landed: unknown,
    // whatever its case. A refusal for want of a permission that rides
    // along with one is still a refusal.
    #[test]
    fn a_timeout_is_an_unknown_landing_and_a_forbidden_answer_stays_refused() {
        let timeout = json!({"message": "Something went wrong while executing your query. \
            This may be the result of a timeout, or it could be a GitHub bug."});
        assert!(matches!(
            judge(answer(200, None, vec![timeout.clone()])).unwrap(),
            Landed::Unknown(_)
        ));
        assert!(matches!(
            judge(answer(
                200,
                None,
                vec![json!({"message": "Request TIMEOUT"})]
            ))
            .unwrap(),
            Landed::Unknown(_)
        ));
        let refused = judge(answer(
            200,
            None,
            vec![
                json!({"type": "FORBIDDEN", "message": "Resource not accessible"}),
                timeout,
            ],
        ))
        .unwrap_err();
        assert!(refused.to_string().contains("Contents: write"), "{refused}");
    }
}
