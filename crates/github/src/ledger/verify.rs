//! What a person runs by hand (GitHub ledger spec §3.5, §3.6): `verify`,
//! which walks every commit from the anchor and checks that each only adds
//! lines or segments, and `quarantine`, which marks a line readers skip
//! without removing it.

use super::GithubLedger;
use super::append::NewLine;
use super::git::{CommitObject, TreeFile};
use super::layout::{
    self, BRANCH, FORMAT, FORMAT_FILE, QUARANTINE_FILE, QuarantineLine, README_FILE,
};
use fl_core::at::At;
use fl_core::iri::Iri;
use fl_core::{LedgerFault, StoreError};
use std::collections::{BTreeMap, BTreeSet};

/// What `verify` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// How many commits it walked.
    pub commits: usize,
    /// The oldest commit that does anything but add, and what it does.
    pub first_bad: Option<BadCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadCommit {
    pub commit: String,
    pub what: String,
}

/// What one verify has already read: blob bytes and tree listings, by id.
#[derive(Default)]
struct Seen {
    blobs: BTreeMap<String, Vec<u8>>,
    trees: BTreeMap<String, BTreeMap<String, TreeFile>>,
}

/// What is wrong with a brand-new segment or `quarantine.jsonl`'s bytes,
/// if anything: fl never creates one empty (there is always at least one
/// line to write), and never leaves one without its final newline.
fn incomplete_new_file(path: &str, bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        Some(format!("adds `{path}` empty, which fl never writes"))
    } else if !bytes.ends_with(b"\n") {
        Some(format!(
            "adds `{path}` without a final newline, which fl never writes"
        ))
    } else {
        None
    }
}

impl GithubLedger<'_> {
    /// `fl github ledger verify` (spec §3.5): every commit from the anchor
    /// to the head, along first parents, each checked to only add lines or
    /// segments. About one request per commit, plus the files it compares.
    pub fn verify(&self) -> Result<Verified, StoreError> {
        let repo = self.repo.full_name.clone();
        let (head, anchor) = match (
            self.branch_head(BRANCH)?,
            self.local.ledger_root(&self.repo.node_id)?,
        ) {
            (Some(h), Some(a)) => (h, a),
            (None, None) => return Err(LedgerFault::NotSetUp { repo }.into()),
            (None, Some(root)) => return Err(LedgerFault::Deleted { repo, root }.into()),
            (Some(_), None) => return Err(LedgerFault::NoAnchor { repo }.into()),
        };
        // Newest first: from the head back to the anchor, or to a commit
        // with no parent that is not the anchor.
        let mut chain: Vec<(String, CommitObject)> = Vec::new();
        let mut bad: Vec<(String, String)> = Vec::new();
        let mut at = head;
        loop {
            let c = self.commit_object(&at)?;
            let parents = c.parents.clone();
            chain.push((at.clone(), c));
            if at == anchor {
                break;
            }
            match parents.as_slice() {
                [] => {
                    bad.push((
                        at.clone(),
                        format!(
                            "starts a history of its own: it has no parent, and it is not the \
                             ledger's first commit {anchor}"
                        ),
                    ));
                    break;
                }
                [p] => at = p.clone(),
                [p, ..] => {
                    bad.push((
                        at.clone(),
                        "merges two histories, and fl only ever adds one commit on top of the \
                         last"
                            .into(),
                    ));
                    at = p.clone();
                }
            }
        }
        chain.reverse();
        let mut seen = Seen::default();
        if chain.first().is_some_and(|(c, _)| *c == anchor) {
            if let Some(what) = self.fls_first_commit(&chain[0].1, &mut seen)? {
                bad.push((anchor.clone(), what));
            }
            for pair in chain.windows(2) {
                if let Some(what) = self.only_adds(&pair[0].1, &pair[1].1, &mut seen)? {
                    bad.push((pair[1].0.clone(), what));
                }
            }
        }
        let age: BTreeMap<&str, usize> = chain
            .iter()
            .enumerate()
            .map(|(i, (c, _))| (c.as_str(), i))
            .collect();
        let first_bad = bad
            .into_iter()
            .min_by_key(|(c, _)| age.get(c.as_str()).copied().unwrap_or(0))
            .map(|(commit, what)| BadCommit { commit, what });
        Ok(Verified {
            commits: chain.len(),
            first_bad,
        })
    }

    /// A blob's raw bytes, memoized. ⚠ Never decoded: two different
    /// invalid-UTF-8 sequences must never compare equal just because a
    /// lossy decode of both collapses to the same replacement characters.
    fn bytes_of(&self, oid: &str, seen: &mut Seen) -> Result<Vec<u8>, StoreError> {
        if let Some(t) = seen.blobs.get(oid) {
            return Ok(t.clone());
        }
        let t = self.blob_bytes(oid)?;
        seen.blobs.insert(oid.to_string(), t.clone());
        Ok(t)
    }

    /// A tree's files, listed once per verify: each commit's tree is both
    /// the "after" of one comparison and the "before" of the next.
    fn files_of(
        &self,
        tree: &str,
        seen: &mut Seen,
    ) -> Result<BTreeMap<String, TreeFile>, StoreError> {
        if let Some(files) = seen.trees.get(tree) {
            return Ok(files.clone());
        }
        let files = self.tree_files(tree)?;
        seen.trees.insert(tree.to_string(), files.clone());
        Ok(files)
    }

    /// What is wrong with the anchor, if anything: it holds exactly
    /// `format` (reading 1) and `README.md`, both ordinary files.
    fn fls_first_commit(
        &self,
        anchor: &CommitObject,
        seen: &mut Seen,
    ) -> Result<Option<String>, StoreError> {
        let files = self.files_of(&anchor.tree, seen)?;
        let names: BTreeSet<&str> = files.keys().map(String::as_str).collect();
        if names != BTreeSet::from([FORMAT_FILE, README_FILE]) {
            return Ok(Some(format!(
                "starts the ledger with {names:?}, where fl writes only `format` and `README.md`"
            )));
        }
        for path in [FORMAT_FILE, README_FILE] {
            if let Some(kind) = files[path].irregular {
                return Ok(Some(format!(
                    "starts the ledger with `{path}` as {kind}, which fl never writes"
                )));
            }
        }
        let format = self.bytes_of(&files[FORMAT_FILE].oid, seen)?;
        let format = String::from_utf8_lossy(&format);
        if format.strip_suffix('\n').unwrap_or(&format) != FORMAT {
            return Ok(Some(format!(
                "starts the ledger at format `{}`",
                format.trim()
            )));
        }
        Ok(None)
    }

    /// What `after` does besides add, compared with its parent `before`.
    fn only_adds(
        &self,
        before: &CommitObject,
        after: &CommitObject,
        seen: &mut Seen,
    ) -> Result<Option<String>, StoreError> {
        let old = self.files_of(&before.tree, seen)?;
        let new = self.files_of(&after.tree, seen)?;
        for (path, file) in &old {
            let Some(now) = new.get(path) else {
                return Ok(Some(format!("deletes `{path}`")));
            };
            // ⚠ Checked before anything path-shaped: a commit that turns a
            // segment (or any other path) into a symlink or an executable
            // is a departure regardless of what the path looks like, and
            // `verify` must report it, never error out and leave every
            // older departure hidden behind it. An entry that was ALREADY
            // irregular and is unchanged was reported when it first
            // appeared; only a fresh change to (or within) it is reported
            // here.
            if let Some(kind) = now.irregular {
                if now != file {
                    return Ok(Some(format!(
                        "changes `{path}` to {kind}, which fl never writes"
                    )));
                }
                continue;
            }
            if now.oid == file.oid {
                continue;
            }
            if path == FORMAT_FILE || path == README_FILE {
                return Ok(Some(format!("changes `{path}`")));
            }
            match layout::parse_segment_path(path) {
                Some((_, dir, n)) => {
                    let closed = old.keys().any(|p| {
                        layout::parse_segment_path(p).is_some_and(|(_, d, m)| d == dir && m > n)
                    });
                    if closed {
                        return Ok(Some(format!("changes `{path}` after it was closed")));
                    }
                }
                None if path == QUARANTINE_FILE => {}
                None => return Ok(Some(format!("changes `{path}`, which fl never writes"))),
            }
            let (was, is) = (
                self.bytes_of(&file.oid, seen)?,
                self.bytes_of(&now.oid, seen)?,
            );
            // ⚠ `grows_only` (shared with `read.rs`'s `grown`, so the rule
            // can never drift) refuses completing a line `was` itself left
            // cut short, even though the completion trivially "starts
            // with" it. It is not enough on its own, though: it only
            // examines `was`, so a commit that grows a segment but leaves
            // ITS OWN result cut short — with no later commit to complete
            // it, there may be none — must be caught here too, not
            // deferred to a comparison that might never happen.
            if !layout::grows_only(&was, &is) || !is.ends_with(b"\n") {
                return Ok(Some(format!("rewrites lines of `{path}`")));
            }
        }
        let mut numbers: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for (path, file) in &new {
            // ⚠ A path this commit introduces, already irregular: never
            // silently accepted — fl writes only ordinary files, whether
            // it is a segment-shaped name or not.
            if !old.contains_key(path)
                && let Some(kind) = file.irregular
            {
                return Ok(Some(format!(
                    "adds `{path}` as {kind}, which fl never writes"
                )));
            }
            match layout::parse_segment_path(path) {
                Some((_, dir, n)) => {
                    numbers.entry(dir).or_default().push(n);
                    // ⚠ A brand-new segment is held to the same rule: fl
                    // never writes a line without its trailing newline
                    // (nor an empty segment at all), so one introduced
                    // without one is reported here, at the commit that
                    // introduced it — never silently accepted because
                    // there was nothing yet to compare it against.
                    if !old.contains_key(path) {
                        let text = self.bytes_of(&file.oid, seen)?;
                        if let Some(what) = incomplete_new_file(path, &text) {
                            return Ok(Some(what));
                        }
                    }
                }
                None if path == QUARANTINE_FILE => {
                    // ⚠ The same rule applies to a `quarantine.jsonl`
                    // this commit creates for the first time.
                    if !old.contains_key(path) {
                        let text = self.bytes_of(&file.oid, seen)?;
                        if let Some(what) = incomplete_new_file(path, &text) {
                            return Ok(Some(what));
                        }
                    }
                }
                None if old.contains_key(path) => {}
                None => return Ok(Some(format!("adds `{path}`, which fl never writes"))),
            }
        }
        for (dir, mut ns) in numbers {
            ns.sort_unstable();
            if ns.iter().copied().ne(1..=ns.len() as u64) {
                return Ok(Some(format!("leaves a gap in the segments of `{dir}`")));
            }
        }
        Ok(None)
    }

    /// `fl github ledger quarantine <file> <line> --by <name> --reason
    /// <text>` (spec §3.6): readers skip that line from now on.
    ///
    /// ⚠ Nothing is removed: the damage and its repair both stay in the
    /// history. `id` is minted by the caller, so a retry adds one line.
    pub fn quarantine(
        &self,
        id: &Iri,
        at: &At,
        file: &str,
        line: u64,
        by_name: &str,
        reason: &str,
    ) -> Result<Option<String>, StoreError> {
        let refuse = |why: String| {
            StoreError::Backend(format!(
                "fl will not quarantine `{file}` line {line}: {why}"
            ))
        };
        if file == QUARANTINE_FILE {
            return Err(refuse(
                "the quarantine file cannot quarantine its own lines; run `fl github ledger \
                 verify` to find the commit that damaged it"
                    .into(),
            ));
        }
        let Some((_, dir, _)) = layout::parse_segment_path(file) else {
            return Err(refuse("it is not a segment of the ledger".into()));
        };
        if by_name.trim().is_empty() {
            return Err(refuse("name who decided, with --by".into()));
        }
        if reason.trim().is_empty() {
            return Err(refuse("say why, with --reason".into()));
        }
        let snap = self.snapshot(std::slice::from_ref(&dir))?;
        let held = snap
            .dirs
            .get(&dir)
            .and_then(|segs| segs.iter().find(|s| s.path == file))
            .map(|s| layout::lines(&s.bytes).len() as u64);
        match held {
            None => {
                return Err(refuse(format!(
                    "the ledger holds no such file at {}",
                    snap.head
                )));
            }
            Some(n) if line == 0 || line > n => {
                let noun = if n == 1 { "line" } else { "lines" };
                return Err(refuse(format!("it has {n} {noun}, numbered from 1")));
            }
            Some(_) => {}
        }
        let q = QuarantineLine {
            id: id.clone(),
            at: at.clone(),
            file: file.to_string(),
            line,
            quarantined_by: by_name.to_string(),
            reason: reason.to_string(),
            by: self.identity()?,
        };
        self.append(
            &[],
            &[NewLine {
                id: id.clone(),
                text: q.encode(),
            }],
            &format!("fl: quarantine {file} line {line}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::creds::EnvToken;
    use crate::fake::{FakeGithub, USER_LOGIN};
    use crate::tracker::Repo;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use fl_core::MemStore;
    use fl_core::conformance::{sample_decision, sample_record_run};
    use fl_core::ids::{GateId, RecordId, seq_iri};
    use fl_core::log::GateRun;
    use fl_core::split::{Batch, LedgerCache, RemoteLedger};
    use fl_core::store::Bindings;
    use serde_json::{Value, json};
    use std::time::Duration;

    /// The next request whose URL contains `frag` answers `status`/`body`
    /// instead, once.
    fn body_next(fake: &FakeGithub, frag: &str, status: u16, body: Value) {
        fake.state().body_next.push((frag.into(), status, body));
    }

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

    fn world() -> (FakeGithub, MemStore, String) {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        (fake, local, root)
    }

    fn open<'a>(c: &'a Client, local: &'a MemStore) -> GithubLedger<'a> {
        GithubLedger::new(c, repo(), local).with_lag(0, Duration::ZERO)
    }

    fn gate() -> GateId {
        GateId(seq_iri(7))
    }

    fn record() -> RecordId {
        RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap())
    }

    fn run(n: u64) -> GateRun {
        sample_record_run(n, &gate(), Some(&record()))
    }

    fn seg(n: u64) -> String {
        layout::segment_path(&layout::dir(layout::Area::Runs, gate().iri()), n)
    }

    fn file(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    fn line(r: &GateRun) -> String {
        layout::Line::Run(r.clone()).encode("someone")
    }

    fn publish(l: &GithubLedger<'_>, n: u64) {
        l.publish(&Batch {
            decision: sample_decision(n, &record(), vec![run(n).id.unwrap()]),
            runs: vec![run(n)],
            attempts: vec![],
        })
        .unwrap();
    }

    // Spec §3.5: a ledger fl wrote only ever adds.
    #[test]
    fn a_ledger_fl_wrote_verifies_clean() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        for n in 1..=3 {
            publish(&l, n);
        }
        let v = l.verify().unwrap();
        assert_eq!(v.first_bad, None, "{v:?}");
        assert_eq!(v.commits, fake.ledger_commits());
    }

    // Spec §3.5: verify reports the first commit that does anything but
    // add lines or segments.
    #[test]
    #[allow(clippy::type_complexity)]
    fn each_departure_is_reported_at_the_commit_that_made_it() {
        let cases: Vec<(&str, Vec<(String, Option<String>)>, &str)> = vec![
            ("a deletion", vec![(seg(1), None)], "deletes"),
            (
                "the README",
                vec![(README_FILE.into(), Some("edited".into()))],
                "changes `README.md`",
            ),
            (
                "the format",
                vec![(FORMAT_FILE.into(), Some("1\n\n".into()))],
                "changes `format`",
            ),
            (
                "a rewritten line",
                vec![(seg(1), Some(file(&[line(&run(9))])))],
                "rewrites lines",
            ),
            (
                "a stray file",
                vec![("notes.txt".into(), Some("x".into()))],
                "adds `notes.txt`",
            ),
            ("a gap", vec![(seg(3), Some(file(&[line(&run(3))])))], "gap"),
        ];
        for (case, changes, what) in cases {
            let (fake, local, _root) = world();
            fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
            let changes: Vec<(&str, Option<&str>)> = changes
                .iter()
                .map(|(p, t)| (p.as_str(), t.as_deref()))
                .collect();
            let bad = fake.hand_commit(&changes);
            fake.hand_commit(&[(seg(1).as_str(), None), ("other.txt", Some("y"))]);
            let c = client(&fake);
            let v = open(&c, &local).verify().unwrap();
            let first = v
                .first_bad
                .unwrap_or_else(|| panic!("{case}: nothing reported"));
            assert_eq!(first.commit, bad, "{case}: {first:?}");
            assert!(first.what.contains(what), "{case}: {}", first.what);
            // ⚠ "changes `{path}`" is a strict PREFIX of the generic
            // "changes `{path}`, which fl never writes" fallback that
            // every other unknown file gets — `contains` alone cannot
            // tell the format/README guard from that fallback having
            // fired instead. Pin it exactly, so disabling the guard (and
            // falling through to the fallback) turns this red.
            if case == "the format" || case == "the README" {
                assert_eq!(
                    first.what, what,
                    "{case}: the guard-specific message, not the generic fallback"
                );
            }
        }
    }

    // Spec §3.5 check 4 as `verify` sees it: `quarantine.jsonl` only grows.
    #[test]
    fn a_quarantine_file_that_lost_a_line_is_reported() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(QUARANTINE_FILE, Some("a\nb\n"))]);
        let bad = fake.hand_commit(&[(QUARANTINE_FILE, Some("b\n"))]);
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, bad);
        assert!(
            first.what.contains("rewrites lines of `quarantine.jsonl`"),
            "{}",
            first.what
        );
    }

    // About one request per commit (spec §3.5): each tree is listed once,
    // though it is compared twice.
    #[test]
    fn verify_lists_each_tree_once() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        for n in 1..=3 {
            publish(&l, n);
        }
        let before = fake.state().requests.len();
        let v = l.verify().unwrap();
        let listed = fake.state().requests[before..]
            .iter()
            .filter(|r| r.contains("/git/trees/"))
            .count();
        assert_eq!(listed, v.commits);
    }

    #[test]
    fn a_closed_segment_that_changes_is_reported() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        fake.hand_commit(&[(seg(2).as_str(), Some(file(&[line(&run(2))]).as_str()))]);
        let bad = fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(3))]).as_str()),
        )]);
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, bad);
        assert!(first.what.contains("closed"), "{}", first.what);
    }

    // Spec §3.5 check 4, extended (the hole a bare `starts_with` left
    // open): a commit that completes a line an earlier one left cut short
    // must be reported — not read as ordinary growth just because the
    // completion trivially "starts with" the cut copy. The commit that
    // FIRST left it cut is reported, since it is the older departure
    // (fl never writes an incomplete line either); either commit's
    // message is "rewrites lines", which is why both qualify.
    #[test]
    fn a_commit_that_leaves_or_completes_a_cut_line_is_reported() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let cut = format!("{}second line, not terminated", file(&[line(&run(1))]));
        let left_it_cut = fake.hand_commit(&[(seg(1).as_str(), Some(cut.as_str()))]);
        let completed = format!("{cut}, now finished\n");
        fake.hand_commit(&[(seg(1).as_str(), Some(completed.as_str()))]);
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(
            first.commit, left_it_cut,
            "the commit that first left it cut: the oldest departure"
        );
        assert!(first.what.contains("rewrites lines"), "{}", first.what);
    }

    // Spec §3.5 check 4, extended: a brand-new segment that is itself cut
    // short — never completed by a later commit, so `grows_only` alone
    // would never see it — must still be reported, at the commit that
    // introduced it.
    #[test]
    fn a_new_segment_without_a_final_newline_is_reported() {
        let (fake, local, _root) = world();
        let cut = format!("{}second line, not terminated", file(&[line(&run(1))]));
        let bad = fake.hand_commit(&[(seg(1).as_str(), Some(cut.as_str()))]);
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, bad);
        assert!(
            first.what.contains("without a final newline"),
            "{}",
            first.what
        );
    }

    // An empty new segment is a different defect from
    // one merely missing its final newline — fl never creates an empty
    // one at all (there is always at least one line to write), and the
    // message says so rather than the misleading "without a final
    // newline" (true of an empty file too, but not the real problem).
    #[test]
    fn an_empty_new_segment_is_worded_as_empty() {
        let (fake, local, _root) = world();
        let bad = fake.hand_commit(&[(seg(1).as_str(), Some(""))]);
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, bad);
        assert_eq!(
            first.what,
            format!("adds `{}` empty, which fl never writes", seg(1))
        );
        assert!(
            !first.what.contains("without a final newline"),
            "{}",
            first.what
        );
    }

    // The same completeness rule applies to a BRAND-NEW
    // `quarantine.jsonl`, not only to a segment — the adds-loop's check
    // previously covered segment paths only.
    #[test]
    fn a_new_quarantine_file_without_a_final_newline_is_reported() {
        let (fake, local, _root) = world();
        let bad = fake.hand_commit(&[(QUARANTINE_FILE, Some("not terminated"))]);
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, bad);
        assert!(
            first.what.contains("without a final newline"),
            "{}",
            first.what
        );
    }

    #[test]
    fn a_second_history_or_a_merge_is_reported() {
        let (fake, local, root) = world();
        let new_root = fake.rewrite_ledger(&[("format", "1\n")]);
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, new_root);
        assert!(
            first.what.contains(&root),
            "names the real first commit: {}",
            first.what
        );

        let (fake2, local2, _root2) = world();
        let merge = fake2.hand_merge();
        let c2 = client(&fake2);
        let first = open(&c2, &local2).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, merge);
        assert!(first.what.contains("merges"), "{}", first.what);
    }

    // The walk must keep following first parents PAST a merge, all the
    // way to the anchor: an older rewrite, behind a newer merge, is still
    // the oldest departure and must be the one reported — not the merge,
    // which would hide it.
    #[test]
    fn verify_reports_an_older_rewrite_even_past_a_newer_merge() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let rewrite = fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(9))]).as_str()))]);
        fake.hand_merge();
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, rewrite, "the rewrite, not the merge");
        assert!(first.what.contains("rewrites"), "{}", first.what);
    }

    /// Overrides `commit`'s own tree listing so `path` is a symlink (mode
    /// `120000`) instead of whatever `hand_commit` actually wrote —
    /// `hand_commit` can only ever write an ordinary file, so a test that
    /// needs an irregular entry must fake the listing GitHub would answer
    /// for one.
    fn make_a_symlink(fake: &FakeGithub, commit: &str, path: &str) {
        let tree_sha = fake.state().git.commits[commit].tree.clone();
        let real = fake.state().git.trees[&tree_sha].clone();
        let items: Vec<Value> = real
            .iter()
            .map(|(p, oid)| {
                let mode = if p == path { "120000" } else { "100644" };
                json!({"path": p, "mode": mode, "type": "blob", "sha": oid})
            })
            .collect();
        body_next(
            fake,
            &format!("/git/trees/{tree_sha}"),
            200,
            json!({"sha": tree_sha, "truncated": false, "tree": items}),
        );
    }

    // A non-regular tree entry must be a `verify`
    // DEPARTURE, reported like any other — never a hard error that leaves
    // `verify` unable to finish past it at all.
    #[test]
    fn a_commit_introducing_a_symlink_is_reported_alone() {
        let (fake, local, _root) = world();
        let bad = fake.hand_commit(&[("link", Some("target"))]);
        make_a_symlink(&fake, &bad, "link");
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, bad);
        assert_eq!(
            first.what,
            "adds `link` as a symlink, which fl never writes"
        );
    }

    // An OLDER rewrite is still the oldest
    // departure, even behind a LATER commit that adds a symlink.
    #[test]
    fn a_symlink_entry_is_reported_behind_an_older_rewrite() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let rewrite = fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(9))]).as_str()))]);
        let bad = fake.hand_commit(&[("link", Some("target"))]);
        make_a_symlink(&fake, &bad, "link");
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(
            first.commit, rewrite,
            "the older rewrite, not the later symlink"
        );
        assert!(first.what.contains("rewrites lines"), "{}", first.what);
    }

    // The other direction: a path fl ALREADY wrote, turned into a symlink
    // by a later commit without ever being deleted first.
    #[test]
    fn a_path_turned_into_a_symlink_is_reported() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let bad = fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
        )]);
        make_a_symlink(&fake, &bad, &seg(1));
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, bad);
        assert_eq!(
            first.what,
            format!("changes `{}` to a symlink, which fl never writes", seg(1))
        );
    }

    #[test]
    fn a_first_commit_holding_more_than_fl_writes_is_reported() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger_with(&[
            ("format", "1\n"),
            ("README.md", "x"),
            (".github/workflows/ci.yml", "x"),
        ]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, root);
    }

    // The sibling check `fls_first_commit` also makes: the right two
    // files, but the wrong format VALUE.
    #[test]
    fn an_anchor_whose_format_is_not_1_is_reported() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger_with(&[("format", "2\n"), ("README.md", "x")]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, root);
        assert!(first.what.contains("format"), "{}", first.what);
    }

    #[test]
    fn a_tree_listing_cut_short_is_an_error_not_a_pass() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        fake.state().truncate_trees = true;
        let c = client(&fake);
        let err = open(&c, &local).verify().unwrap_err();
        assert!(err.to_string().contains("short"), "{err}");
    }

    #[test]
    fn verify_says_why_when_there_is_no_ledger_to_walk() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        assert!(matches!(
            open(&c, &local).verify().unwrap_err(),
            StoreError::Ledger(LedgerFault::NotSetUp { .. })
        ));
        fake.seed_ledger();
        assert!(matches!(
            open(&c, &local).verify().unwrap_err(),
            StoreError::Ledger(LedgerFault::NoAnchor { .. })
        ));
        local
            .set_ledger_root("R_1", &fake.ledger_head().unwrap())
            .unwrap();
        fake.delete_ledger();
        assert!(matches!(
            open(&c, &local).verify().unwrap_err(),
            StoreError::Ledger(LedgerFault::Deleted { .. })
        ));
    }

    // Spec §3.6: quarantine appends; readers skip the line; nothing is
    // removed.
    #[test]
    fn an_unreadable_line_once_quarantined_is_skipped_and_its_damage_stays() {
        let (fake, local, _root) = world();
        let damaged = format!("{}not json\n", file(&[line(&run(1))]));
        fake.hand_commit(&[(seg(1).as_str(), Some(damaged.as_str()))]);
        let c = client(&fake);
        let l = open(&c, &local);
        assert!(l.runs(&gate()).is_err());
        let committed = l
            .quarantine(
                &seq_iri(60),
                &At::from_unix_millis(60),
                &seg(1),
                2,
                "Ada",
                "a hand edit",
            )
            .unwrap();
        assert_eq!(committed, fake.ledger_head());
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
        assert_eq!(l.take_notes().len(), 1);
        let files = fake.ledger_files();
        assert!(files[&seg(1)].contains("not json"), "nothing was removed");
        let q = QuarantineLine::decode(files[QUARANTINE_FILE].trim_end()).unwrap();
        assert_eq!((q.file.as_str(), q.line), (seg(1).as_str(), 2));
        assert_eq!(
            (q.quarantined_by.as_str(), q.by.as_str()),
            ("Ada", USER_LOGIN)
        );
        assert_eq!(
            l.verify().unwrap().first_bad,
            None,
            "a quarantine only adds"
        );
    }

    // An invalid UTF-8 byte in one line must not make
    // `snapshot` (and so `quarantine`, which only needs a snapshot) fail
    // hard — only `lines()`-based reads (`runs`), which actually decode
    // each line, see it, and only as the usual `Unreadable` for that one
    // line.
    #[test]
    fn an_invalid_byte_in_a_line_does_not_block_snapshot_or_quarantine() {
        let (fake, local, _root) = world();
        let good = file(&[line(&run(1))]);
        fake.hand_commit(&[(seg(1).as_str(), Some(good.as_str()))]);
        let seg_oid = fake
            .state()
            .git
            .blobs
            .iter()
            .find(|(_, t)| **t == good)
            .map(|(id, _)| id.clone())
            .expect("seg(1)'s blob exists on the fake");
        let raw = [good.as_bytes(), &[0xffu8], b"\n"].concat();
        body_next(
            &fake,
            &format!("/git/blobs/{seg_oid}"),
            200,
            json!({"content": STANDARD.encode(&raw)}),
        );
        let c = client(&fake);
        let l = open(&c, &local);
        // The one bad line fails the whole decode, exactly as "not json"
        // already does — as `Unreadable`, for line 2 alone.
        let err = l.runs(&gate()).unwrap_err();
        match &err {
            StoreError::Ledger(LedgerFault::Unreadable {
                file: f,
                line: n,
                cause,
                ..
            }) => {
                assert_eq!((f.as_str(), *n), (seg(1).as_str(), 2));
                assert!(cause.contains("not valid UTF-8"), "{cause}");
            }
            other => panic!("{other:?}"),
        }
        // ⚠ Cached as the bytes GitHub sent — never as lossy text, which
        // `quarantine` (and every later read) would then work from.
        let cached = local.cached("R_1", &seg(1)).unwrap().expect("cached");
        assert_eq!(cached.bytes, raw);
        let committed = l
            .quarantine(
                &seq_iri(60),
                &At::from_unix_millis(60),
                &seg(1),
                2,
                "Ada",
                "a hand edit",
            )
            .unwrap();
        assert_eq!(committed, fake.ledger_head());
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1)]);
    }

    // `verify` must still tell two DIFFERENT invalid
    // byte sequences apart — the whole point of comparing raw bytes
    // instead of a lossy-decoded string, which would collapse both to the
    // same replacement characters and see no rewrite at all.
    #[test]
    fn verify_tells_apart_two_different_invalid_byte_sequences() {
        let (fake, local, _root) = world();
        // The fake can only commit valid UTF-8 (`&str`), so these are mere
        // placeholders: each commit's REAL download is overridden below to
        // answer invalid bytes instead, each ending in a newline (so the
        // new-segment and growth checks have something to compare).
        let placeholder_a = "placeholder a";
        let placeholder_b = "placeholder b";
        fake.hand_commit(&[(seg(1).as_str(), Some(placeholder_a))]);
        let bad = fake.hand_commit(&[(seg(1).as_str(), Some(placeholder_b))]);
        let oid_of = |text: &str| {
            fake.state()
                .git
                .blobs
                .iter()
                .find(|(_, t)| t.as_str() == text)
                .map(|(id, _)| id.clone())
                .unwrap_or_else(|| panic!("{text} was not committed"))
        };
        let (oid_a, oid_b) = (oid_of(placeholder_a), oid_of(placeholder_b));
        body_next(
            &fake,
            &format!("/git/blobs/{oid_a}"),
            200,
            json!({"content": STANDARD.encode([0xffu8, b'\n'])}),
        );
        body_next(
            &fake,
            &format!("/git/blobs/{oid_b}"),
            200,
            json!({"content": STANDARD.encode([0xfeu8, b'\n'])}),
        );
        let c = client(&fake);
        let first = open(&c, &local).verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, bad);
        assert!(first.what.contains("rewrites lines"), "{}", first.what);
    }

    // Spec §3.6: a quarantined line cut short (no final newline) is
    // skipped, and the next publish must not lose its entry to it. Written
    // straight after the cut bytes, the new line would merge into the
    // quarantined one: readers would skip it too, though the publish
    // answered with a commit. It starts the next segment instead, and fl's
    // own commit only adds — the damage stays at the commit that made it.
    #[test]
    fn a_publish_after_a_quarantined_cut_line_starts_a_new_segment_and_reads_back() {
        let (fake, local, _root) = world();
        let cut = format!("{}{{\"cut", file(&[line(&run(1))]));
        let damage = fake.hand_commit(&[(seg(1).as_str(), Some(cut.as_str()))]);
        let c = client(&fake);
        let l = open(&c, &local);
        l.quarantine(
            &seq_iri(60),
            &At::from_unix_millis(60),
            &seg(1),
            2,
            "Ada",
            "cut short",
        )
        .unwrap();
        let before = fake.ledger_head().unwrap();
        let published = l
            .publish(&Batch {
                decision: sample_decision(2, &record(), vec![run(2).id.unwrap()]),
                runs: vec![run(2)],
                attempts: vec![],
            })
            .unwrap();
        assert_eq!(published, fake.ledger_head());
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1), run(2)]);
        let files = fake.ledger_files();
        assert_eq!(files[&seg(1)], cut, "the cut segment is never rewritten");
        assert!(files[&seg(2)].ends_with('\n'), "{}", files[&seg(2)]);
        let mut seen = Seen::default();
        let (was, is) = (
            l.commit_object(&before).unwrap(),
            l.commit_object(&published.unwrap()).unwrap(),
        );
        assert_eq!(
            l.only_adds(&was, &is, &mut seen).unwrap(),
            None,
            "fl's own commit only adds"
        );
        let first = l.verify().unwrap().first_bad.unwrap();
        assert_eq!(first.commit, damage, "only the hand edit is reported");
    }

    // Ruling 17: a line in the wrong directory can be quarantined too.
    #[test]
    fn a_misplaced_line_can_be_quarantined() {
        let (fake, local, _root) = world();
        let elsewhere = sample_record_run(1, &GateId(seq_iri(8)), Some(&record()));
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&elsewhere)]).as_str()))]);
        let c = client(&fake);
        let l = open(&c, &local);
        l.quarantine(
            &seq_iri(61),
            &At::from_unix_millis(61),
            &seg(1),
            1,
            "Ada",
            "misfiled",
        )
        .unwrap();
        assert!(l.runs(&gate()).unwrap().is_empty());
    }

    #[test]
    fn a_quarantine_retried_after_a_lost_answer_adds_one_line() {
        let (fake, local, _root) = world();
        let damaged = format!("{}not json\n", file(&[line(&run(1))]));
        fake.hand_commit(&[(seg(1).as_str(), Some(damaged.as_str()))]);
        fake.state().hang_up_after_next_commit = true;
        let c = client(&fake);
        let l = open(&c, &local);
        l.quarantine(
            &seq_iri(60),
            &At::from_unix_millis(60),
            &seg(1),
            2,
            "Ada",
            "r",
        )
        .unwrap();
        l.quarantine(
            &seq_iri(60),
            &At::from_unix_millis(60),
            &seg(1),
            2,
            "Ada",
            "r",
        )
        .unwrap();
        assert_eq!(
            layout::lines(fake.ledger_files()[QUARANTINE_FILE].as_bytes()).len(),
            1
        );
    }

    #[test]
    fn a_quarantine_must_name_a_line_of_a_segment_and_who_decided_why() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let c = client(&fake);
        let l = open(&c, &local);
        let at = At::from_unix_millis(60);
        // ⚠ The quarantine-file, not-a-segment, --by and --reason checks
        // all refuse BEFORE any request (none needs a snapshot to judge).
        // `read.rs`'s own `directory()` can refuse an unrelated "not a
        // segment" shape too (if a bogus directory were ever read), so the
        // message alone is not always enough to tell this guard fired
        // rather than that one — pin the exact, guard-specific wording
        // ("of the ledger", which `directory()`'s "fl writes" never says)
        // AND that no request was made, so disabling the guard and
        // falling through to a snapshot read turns this red either way.
        for (file, line, by, reason, says, no_request) in [
            (
                QUARANTINE_FILE.to_string(),
                1,
                "Ada",
                "r",
                "its own lines",
                true,
            ),
            (
                FORMAT_FILE.to_string(),
                1,
                "Ada",
                "r",
                "not a segment of the ledger",
                true,
            ),
            (seg(2), 1, "Ada", "r", "no such file", false),
            (seg(1), 0, "Ada", "r", "it has 1 line,", false),
            (seg(1), 2, "Ada", "r", "it has 1 line,", false),
            (seg(1), 1, " ", "r", "--by", true),
            (seg(1), 1, "Ada", "", "--reason", true),
        ] {
            let before = fake.state().requests.len();
            let err = l
                .quarantine(&seq_iri(60), &at, &file, line, by, reason)
                .unwrap_err();
            assert!(err.to_string().contains(says), "{file} {line}: {err}");
            if no_request {
                assert_eq!(
                    fake.state().requests.len(),
                    before,
                    "{file} {line}: refused before any request"
                );
            }
        }
        assert!(
            !fake.ledger_files().contains_key(QUARANTINE_FILE),
            "nothing written"
        );
    }
}
