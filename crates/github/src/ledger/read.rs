//! Reading the `fl/ledger` branch (GitHub ledger spec §3.3, §3.5): the
//! checked head and the format. Each check is named where it is made.

use super::GithubLedger;
use super::git::Object;
use super::layout::{BRANCH, FORMAT, FORMAT_FILE};
use fl_core::split::CachedSegment;
use fl_core::{LedgerFault, StoreError};

impl GithubLedger<'_> {
    pub(crate) fn altered(&self, file: &str, what: impl Into<String>, commit: &str) -> StoreError {
        LedgerFault::Altered {
            repo: self.repo.full_name.clone(),
            file: file.to_string(),
            what: what.into(),
            commit: commit.to_string(),
        }
        .into()
    }

    /// The head of `fl/ledger`, checked (spec §3.5 checks 1 and 2).
    ///
    /// ⚠ One compare (ruling 9): against the last head this machine saw
    /// when there is one, else against the anchor. Every head recorded as
    /// seen was itself checked to descend from the anchor, so descending
    /// from it is descending from the anchor. A `behind` answer, or a
    /// compare that does not know one of the commits, is read again
    /// `lag_reads` times before it counts: GitHub's replicas can briefly lag
    /// a write — above all this machine's own, which it records as the last
    /// head seen (ruling 24).
    pub fn check_head(&self) -> Result<String, StoreError> {
        let repo = self.repo.full_name.clone();
        let node = self.repo.node_id.as_str();
        let anchor = self.local.ledger_root(node)?;
        let last_seen = self.local.last_head(node)?;
        let mut reads = 0u32;
        loop {
            let (head, anchor) = match (self.branch_head(BRANCH)?, anchor.clone()) {
                (Some(h), Some(a)) => (h, a),
                (None, None) => return Err(LedgerFault::NotSetUp { repo }.into()),
                (Some(_), None) => return Err(LedgerFault::NoAnchor { repo }.into()),
                // ⚠ Ruling 24: when this machine already saw a head, a 404
                // can be a replica that has not learned of the branch at
                // all yet — not proof it was deleted. Read again first.
                (None, Some(_)) if last_seen.is_some() && reads < self.lag_reads => {
                    reads += 1;
                    std::thread::sleep(self.lag_pause);
                    continue;
                }
                (None, Some(root)) => return Err(LedgerFault::Deleted { repo, root }.into()),
            };
            let (against, base) = match last_seen.clone() {
                Some(seen) => ("the last head this machine saw", seen),
                None => ("the ledger's first commit", anchor),
            };
            if head == base {
                return Ok(head);
            }
            match self.compare(&base, &head)?.as_deref() {
                Some("ahead" | "identical") => return Ok(head),
                Some("behind") | None if reads < self.lag_reads => {
                    reads += 1;
                    std::thread::sleep(self.lag_pause);
                }
                other => {
                    let how = match other {
                        Some("behind") => {
                            format!("it is behind it, still after {reads} reads again")
                        }
                        Some(s) => format!("GitHub compares them as `{s}`"),
                        None => format!(
                            "GitHub does not know one of the two commits, still after {reads} \
                             reads again"
                        ),
                    };
                    return Err(LedgerFault::Rewritten {
                        repo,
                        head,
                        against,
                        base,
                        how,
                    }
                    .into());
                }
            }
        }
    }

    /// The checked head (checks 1 and 2) whose `format` this fl reads
    /// (check 7), recorded as the last head seen (ruling 10). What B2's
    /// pre-flight asks before any gate or adapter runs (spec §2.4).
    pub fn check_format(&self) -> Result<String, StoreError> {
        let head = self.check_head()?;
        let found = self.objects(&head, &[FORMAT_FILE.to_string()])?;
        self.format(&head, found[0].as_ref())?;
        self.local.set_last_head(&self.repo.node_id, &head)?;
        Ok(head)
    }

    /// `path`'s text at blob `oid` — from this machine's cache when it
    /// holds that blob, else downloaded — and what the cache held before.
    fn text_at(
        &self,
        path: &str,
        oid: &str,
    ) -> Result<(String, Option<CachedSegment>), StoreError> {
        let before = self.local.cached(&self.repo.node_id, path)?;
        let text = match &before {
            Some(c) if c.oid == oid => c.text.clone(),
            _ => self.blob_text(oid)?,
        };
        Ok((text, before))
    }

    /// ⚠ Check 7: the format is one this fl knows.
    fn format(&self, head: &str, found: Option<&Object>) -> Result<(), StoreError> {
        let Some(Object::Blob { oid }) = found else {
            return Err(self.altered(FORMAT_FILE, "is missing, or is not a file", head));
        };
        let (text, _) = self.text_at(FORMAT_FILE, oid)?;
        // ⚠ Ruling 19: only the exact text `1` (one trailing newline
        // allowed) is format 1 — not merely text that TRIMS to `1`, which
        // `1\r\n` or ` 1\n` also would. `found` carries the raw text,
        // escaped (`{:?}`), never trimmed: a damaged `1\r\n` must not be
        // reported as `1`, which would read as "this is already the format
        // fl reads" and hide the damage instead of naming it.
        if text.strip_suffix('\n').unwrap_or(&text) != FORMAT {
            return Err(LedgerFault::UnknownFormat {
                repo: self.repo.full_name.clone(),
                found: format!("{text:?}"),
            }
            .into());
        }
        self.local.cache(
            &self.repo.node_id,
            FORMAT_FILE,
            &CachedSegment {
                oid: oid.clone(),
                text,
                closed: true,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use crate::tracker::Repo;
    use fl_core::MemStore;
    use fl_core::split::LedgerCache;
    use fl_core::store::Bindings;
    use std::time::Duration;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
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

    fn blob_reads(fake: &FakeGithub) -> usize {
        fake.state()
            .requests
            .iter()
            .filter(|r| r.contains("/git/blobs/"))
            .count()
    }

    /// A commit on the fake's ledger that only this test reads: what it holds
    /// does not matter to the head and format checks.
    fn commit_on(fake: &FakeGithub, text: &str) -> String {
        fake.hand_commit(&[("runs/k/1.jsonl", Some(text))])
    }

    #[test]
    fn a_fresh_ledger_checks_and_remembers_the_head_it_checked() {
        let (fake, local, root) = world();
        let c = client(&fake);
        assert_eq!(open(&c, &local).check_format().unwrap(), root);
        assert_eq!(local.last_head("R_1").unwrap(), Some(root));
    }

    // Spec §7: a ledger that is not there says which way, and what to do.
    #[test]
    fn a_ledger_that_is_not_there_says_why() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let local = MemStore::default();
        let err = open(&c, &local).check_head().unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::NotSetUp { .. })),
            "{err:?}"
        );
        fake.seed_ledger();
        let err = open(&c, &local).check_head().unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::NoAnchor { .. })),
            "{err:?}"
        );
        local
            .set_ledger_root("R_1", &fake.ledger_head().unwrap())
            .unwrap();
        fake.delete_ledger();
        let err = open(&c, &local).check_head().unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Deleted { .. })),
            "{err:?}"
        );
    }

    #[test]
    fn checking_while_github_is_down_is_an_error_not_a_pass() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        fake.state().down = true;
        let err = open(&c, &local).check_format().unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    // ⚠ Spec §3.5 check 1.
    #[test]
    fn a_head_that_does_not_descend_from_the_anchor_is_a_rewrite() {
        let (fake, local, root) = world();
        fake.rewrite_ledger(&[("format", "1\n")]);
        let c = client(&fake);
        match open(&c, &local).check_head().unwrap_err() {
            StoreError::Ledger(LedgerFault::Rewritten { base, against, .. }) => {
                assert_eq!(base, root);
                assert!(against.contains("first commit"), "{against}");
            }
            other => panic!("{other:?}"),
        }
    }

    // ⚠ Spec §3.5 check 2: GitHub's replicas can briefly lag, so a `behind`
    // answer is read again before it counts.
    #[test]
    fn a_head_behind_the_last_one_seen_is_read_again_before_it_counts() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        open(&c, &local).check_format().unwrap();
        commit_on(&fake, "a\n");
        fake.state().compare_behind_next = 2;
        open(&c, &local)
            .check_head()
            .expect("two lagging answers, then `ahead`: no alarm");
    }

    #[test]
    fn a_head_that_stays_behind_is_a_rewrite() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let first = commit_on(&fake, "a\n");
        open(&c, &local).check_format().unwrap();
        assert_eq!(local.last_head("R_1").unwrap(), Some(first.clone()));
        commit_on(&fake, "a\nb\n");
        fake.state().compare_behind_next = 3;
        match open(&c, &local).check_head().unwrap_err() {
            StoreError::Ledger(LedgerFault::Rewritten {
                base, against, how, ..
            }) => {
                assert_eq!(base, first);
                assert!(against.contains("last head"), "{against}");
                assert!(how.contains("behind"), "{how}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_branch_reset_to_an_older_commit_is_a_rewrite() {
        let (fake, local, root) = world();
        let c = client(&fake);
        commit_on(&fake, "a\n");
        open(&c, &local).check_format().unwrap();
        fake.state()
            .git
            .refs
            .insert(format!("heads/{BRANCH}"), root);
        let err = open(&c, &local).check_head().unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Rewritten { .. })),
            "{err:?}"
        );
    }

    // ⚠ Ruling 24: right after a write, a replica may answer a branch read
    // with the head before it, or not know a commit in a compare. Both are
    // read again before they count; one that persists is a rewrite.
    #[test]
    fn a_replica_that_has_not_seen_the_last_write_raises_no_alarm() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let mine = commit_on(&fake, "a\n");
        // As after this machine's own append: its commit is the last seen.
        local.set_last_head("R_1", &mine).unwrap();
        fake.state().ref_behind_next = 1;
        assert_eq!(open(&c, &local).check_head().unwrap(), mine);
        commit_on(&fake, "a\nb\n");
        fake.state().compare_unknown_next = 2;
        open(&c, &local)
            .check_head()
            .expect("a compare that knows the commit after two reads: no alarm");
        let theirs = commit_on(&fake, "a\nb\nc\n");
        local.set_last_head("R_1", &theirs).unwrap();
        commit_on(&fake, "a\nb\nc\nd\n");
        fake.state().compare_unknown_next = 3;
        match open(&c, &local).check_head().unwrap_err() {
            StoreError::Ledger(LedgerFault::Rewritten { how, .. }) => {
                assert!(how.contains("does not know"), "{how}");
            }
            other => panic!("{other:?}"),
        }
    }

    // ⚠ Spec §3.5 check 7. Ruling 19: `found` carries the RAW text, escaped
    // — never trimmed — so a damaged file is never shown as the clean text
    // it merely trims to.
    #[test]
    fn a_format_other_than_1_is_refused_naming_an_upgrade() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger_with(&[("format", "2\n"), ("README.md", "x")]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let err = open(&c, &local).check_format().unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::UnknownFormat { ref found, .. })
                    if *found == format!("{:?}", "2\n")
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("Upgrade fl"), "{err}");
        assert_eq!(
            local.last_head("R_1").unwrap(),
            None,
            "a refusal must not record the head as checked"
        );
    }

    // Ruling 19: only the exact text `1` (one trailing newline allowed) is
    // format 1 — text that merely TRIMS to `1` (a stray `\r`, a leading
    // space) is not, and `found` must show the raw, escaped text so the
    // message never reads as "this is already format 1".
    #[test]
    fn a_format_with_hidden_whitespace_is_refused_and_reported_raw_not_trimmed() {
        for damaged in ["1\r\n", " 1\n", " 1"] {
            let fake = FakeGithub::start("acme/widgets");
            let root = fake.seed_ledger_with(&[("format", damaged), ("README.md", "x")]);
            let local = MemStore::default();
            local.set_ledger_root("R_1", &root).unwrap();
            let c = client(&fake);
            match open(&c, &local).check_format().unwrap_err() {
                StoreError::Ledger(LedgerFault::UnknownFormat { found, .. }) => {
                    assert_ne!(found, "1", "{damaged:?}: must not read as the clean format");
                    assert_eq!(found, format!("{damaged:?}"), "{damaged:?}");
                }
                other => panic!("{damaged:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_ledger_with_no_format_file_is_altered() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger_with(&[("README.md", "x")]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let err = open(&c, &local).check_format().unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref file, .. })
                    if file == FORMAT_FILE
            ),
            "{err:?}"
        );
        assert_eq!(
            local.last_head("R_1").unwrap(),
            None,
            "a refusal must not record the head as checked"
        );
    }

    // Spec §3.3: a file read before is not downloaded again.
    #[test]
    fn the_format_is_downloaded_once() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        open(&c, &local).check_format().unwrap();
        let before = blob_reads(&fake);
        assert_eq!(before, 1, "the first check must itself have downloaded it");
        open(&c, &local).check_format().unwrap();
        assert_eq!(blob_reads(&fake), before);
    }

    // The cache is reused only when its recorded oid still matches what the
    // branch names now — never merely because the path was read before.
    #[test]
    fn a_cached_file_is_not_reused_once_its_oid_has_moved_on() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        open(&c, &local).check_format().unwrap();
        fake.hand_commit(&[("format", Some("2\n"))]);
        let err = open(&c, &local).check_format().unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::UnknownFormat { ref found, .. })
                    if *found == format!("{:?}", "2\n")
            ),
            "{err:?}"
        );
    }

    // ⚠ Ruling 24: GraphQL can be served by a replica that has not caught up
    // with a write yet — separately from the REST replica `check_head`
    // already confirmed the head against. A `ledgerObjects` answer that does
    // not yet know the checked head is read again before it counts as the
    // format file being missing.
    #[test]
    fn a_graphql_replica_that_has_not_seen_the_last_write_raises_no_alarm_reading_the_format() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        open(&c, &local).check_format().unwrap();
        commit_on(&fake, "a\n");
        fake.state().graphql_commit_unknown_next = 2;
        open(&c, &local)
            .check_format()
            .expect("two lagging GraphQL reads, then it catches up: no alarm");
    }

    // The reverse of the above: a lag that never resolves must not be
    // reported as the file being missing — that would be a false alarm just
    // as much as an immediate one.
    #[test]
    fn a_graphql_replica_that_never_catches_up_is_an_error_not_a_false_altered() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        open(&c, &local).check_format().unwrap();
        commit_on(&fake, "a\n");
        fake.state().graphql_commit_unknown_next = 3;
        let err = open(&c, &local).check_format().unwrap_err();
        assert!(
            !matches!(err, StoreError::Ledger(LedgerFault::Altered { .. })),
            "a replica that never catches up must not be reported as the file missing: {err:?}"
        );
    }

    // ⚠ Ruling 24: a branch-ref 404 right after a write this machine already
    // recorded can be a replica that has not learned of the branch at all
    // yet, not proof it was deleted.
    #[test]
    fn a_branch_404_right_after_this_machines_own_write_is_read_again_before_a_deletion_is_declared()
     {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let mine = commit_on(&fake, "a\n");
        local.set_last_head("R_1", &mine).unwrap();
        fake.state().ref_404_next = 1;
        assert_eq!(
            open(&c, &local).check_head().unwrap(),
            mine,
            "one 404 read again, then the real branch: no alarm"
        );
    }
}
