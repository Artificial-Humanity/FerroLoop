//! Reading the `fl/ledger` branch (GitHub ledger spec §3.3, §3.5): the
//! checked head, a snapshot of the directories one read or append needs,
//! and their lines, parsed strictly. Each of the seven checks is named
//! where it is made.

use super::GithubLedger;
use super::git::Object;
use super::layout::{
    self, Area, BRANCH, FORMAT, FORMAT_FILE, Line, QUARANTINE_FILE, QuarantineLine,
};
use fl_core::decision::Decision;
use fl_core::ids::{GateId, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::split::CachedSegment;
use fl_core::{LedgerFault, StoreError};
use std::collections::BTreeMap;

/// What a read reports without refusing (spec §3.3, §3.6).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Note {
    /// A quarantined line, skipped.
    Quarantined {
        file: String,
        line: u64,
        reason: String,
    },
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::Quarantined { file, line, reason } => write!(
                f,
                "`{file}` line {line} of the GitHub ledger is quarantined ({reason}), so fl \
                 skipped it"
            ),
        }
    }
}

/// One segment of a directory, read and checked. Segments are numbered
/// from 1 with no gap, so a directory's `n`th is at index `n - 1`.
#[derive(Debug)]
pub(crate) struct Segment {
    pub path: String,
    /// Exactly the bytes GitHub holds: each line is decoded on its own,
    /// strictly, when it is read (`layout::lines`).
    pub bytes: Vec<u8>,
}

/// The directories one read or append needs, at one checked head.
pub(crate) struct Snapshot {
    pub head: String,
    pub dirs: BTreeMap<String, Vec<Segment>>,
    /// `quarantine.jsonl`'s bytes; empty when there is none.
    pub quarantine: Vec<u8>,
}

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
        let mut pending = Vec::new();
        self.format(&head, found[0].as_ref(), &mut pending)?;
        self.local.remember(&self.repo.node_id, &head, &pending)?;
        Ok(head)
    }

    /// `dirs` at the checked head (spec §3.5 checks 1–4 and 7), with
    /// `quarantine.jsonl`. Records the head as the last seen (ruling 10).
    ///
    /// ⚠ Every file this read validates is cached together with the new
    /// last head, in ONE call to `remember` at the very end — never one at
    /// a time as each file passes. A read that fails partway
    /// (one directory's blob download, say) must leave NEITHER applied:
    /// otherwise an earlier file in this same read, already cached on its
    /// own, would hold a position or content the last head — still the OLD
    /// one, since this read never finished — never itself confirmed, and a
    /// later read at that same, unmoved head would see a cache entry from a
    /// commit it was never recorded as having read, and raise a false
    /// `Altered` alarm (reproduced by `a_blob_download_that_fails_after_an_
    /// earlier_segment_already_passed_raises_no_later_alarm`).
    pub(crate) fn snapshot(&self, dirs: &[String]) -> Result<Snapshot, StoreError> {
        let head = self.check_head()?;
        let mut paths = vec![FORMAT_FILE.to_string(), QUARANTINE_FILE.to_string()];
        paths.extend(dirs.iter().cloned());
        let found = self.objects(&head, &paths)?;
        let mut pending: Vec<(String, CachedSegment)> = Vec::new();
        self.format(&head, found[0].as_ref(), &mut pending)?;
        let quarantine = match &found[1] {
            None => {
                if self
                    .local
                    .cached(&self.repo.node_id, QUARANTINE_FILE)?
                    .is_some()
                {
                    return Err(self.altered(QUARANTINE_FILE, "was deleted", &head));
                }
                Vec::new()
            }
            Some(Object::Blob { oid }) => {
                self.grown(&head, QUARANTINE_FILE, oid, false, &mut pending)?
            }
            Some(Object::Tree(_)) => {
                return Err(self.altered(
                    QUARANTINE_FILE,
                    "is a directory where a file belongs",
                    &head,
                ));
            }
        };
        let mut out = BTreeMap::new();
        for (dir, f) in dirs.iter().zip(&found[2..]) {
            out.insert(
                dir.clone(),
                self.directory(&head, dir, f.as_ref(), &mut pending)?,
            );
        }
        self.local.remember(&self.repo.node_id, &head, &pending)?;
        Ok(Snapshot {
            head,
            dirs: out,
            quarantine,
        })
    }

    /// `path`'s bytes at blob `oid` — from this machine's cache when it
    /// holds that blob, else downloaded — and what the cache held before.
    ///
    /// ⚠ Bytes, never decoded here: a reader decodes each LINE strictly
    /// (`layout::lines`), so a damaged line is `Unreadable` on its own; a
    /// lossy decode of the whole file would hand back U+FFFD as if the
    /// ledger held it, and let check 4 see two different damaged lines as
    /// the same.
    fn bytes_at(
        &self,
        path: &str,
        oid: &str,
    ) -> Result<(Vec<u8>, Option<CachedSegment>), StoreError> {
        let before = self.local.cached(&self.repo.node_id, path)?;
        let bytes = match &before {
            Some(c) if c.oid == oid => c.bytes.clone(),
            _ => self.blob_bytes(oid)?,
        };
        Ok((bytes, before))
    }

    /// ⚠ Check 7: the format is one this fl knows. Queues its cache entry
    /// in `pending` rather than writing it — `snapshot`/`check_format`
    /// commit every queued entry together with the new last head, only once
    /// the whole read has passed every check.
    fn format(
        &self,
        head: &str,
        found: Option<&Object>,
        pending: &mut Vec<(String, CachedSegment)>,
    ) -> Result<(), StoreError> {
        let Some(Object::Blob { oid }) = found else {
            return Err(self.altered(FORMAT_FILE, "is missing, or is not a file", head));
        };
        let (bytes, _) = self.bytes_at(FORMAT_FILE, oid)?;
        // ⚠ Ruling 19: only the exact text `1` (one trailing newline
        // allowed) is format 1 — not merely text that TRIMS to `1`, which
        // `1\r\n` or ` 1\n` also would. `found` carries the raw text,
        // escaped, never trimmed: a damaged `1\r\n` must not be reported
        // as `1`, which would read as "this is already the format fl reads"
        // and hide the damage instead of naming it.
        if bytes.strip_suffix(b"\n").unwrap_or(&bytes) != FORMAT.as_bytes() {
            let found = match std::str::from_utf8(&bytes) {
                Ok(text) => format!("{text:?}"),
                Err(_) => format!("\"{}\"", bytes.escape_ascii()),
            };
            return Err(LedgerFault::UnknownFormat {
                repo: self.repo.full_name.clone(),
                found,
            }
            .into());
        }
        pending.push((
            FORMAT_FILE.to_string(),
            CachedSegment {
                oid: oid.clone(),
                bytes,
                closed: true,
            },
        ));
        Ok(())
    }

    /// A file that may only grow, checked against the copy read before, and
    /// queued in `pending` — never written directly — only once it passed.
    fn grown(
        &self,
        head: &str,
        path: &str,
        oid: &str,
        closed: bool,
        pending: &mut Vec<(String, CachedSegment)>,
    ) -> Result<Vec<u8>, StoreError> {
        let (bytes, before) = self.bytes_at(path, oid)?;
        if let Some(c) = &before
            && c.oid != oid
        {
            // ⚠ Check 3: a closed segment never changes.
            if c.closed {
                return Err(self.altered(path, "changed after it was closed", head));
            }
            // ⚠ Check 4: the open segment only grew from the copy read
            // before. `grows_only` is shared with `verify`'s `only_adds`
            // so the rule can never drift between the two; here it is
            // always equivalent to the plain prefix check it replaces,
            // since `c.bytes` is only ever cached below when it is itself
            // empty or newline-terminated. ⚠ It compares BYTES: one
            // invalid byte rewritten into another is a rewrite, though a
            // lossy decode of both reads the same.
            if !layout::grows_only(&c.bytes, &bytes) {
                return Err(self.altered(
                    path,
                    "no longer starts with the copy this machine read before",
                    head,
                ));
            }
        }
        // ⚠ Spec §3.5 check 4, extended: a cut-short copy (no final
        // newline) must never become the trusted baseline for that
        // comparison. Completing a cut line only appends bytes after an
        // unchanged prefix, so ANY completion — including a malicious one —
        // would trivially "start with" a cached cut-short copy, making
        // check 4 vacuous for whatever follows it. Leave the cache as it
        // was; `lines()` still catches the cut line itself, as
        // `Unreadable`, before any append could ever be planned on it.
        //
        // ⚠ Cached as the bytes read, exactly — a line that is not UTF-8
        // included (`CachedSegment` stores those losslessly), so check 4
        // compares the next copy with what GitHub really held, and a
        // quarantine works from it.
        if bytes.is_empty() || bytes.ends_with(b"\n") {
            pending.push((
                path.to_string(),
                CachedSegment {
                    oid: oid.to_string(),
                    bytes: bytes.clone(),
                    closed,
                },
            ));
        }
        Ok(bytes)
    }

    /// One directory's segments: named `1.jsonl` up with no gap, none of
    /// those read before gone, each checked by [`Self::grown`].
    fn directory(
        &self,
        head: &str,
        dir: &str,
        found: Option<&Object>,
        pending: &mut Vec<(String, CachedSegment)>,
    ) -> Result<Vec<Segment>, StoreError> {
        let entries = match found {
            None => Vec::new(),
            Some(Object::Tree(entries)) => entries.clone(),
            Some(Object::Blob { .. }) => {
                return Err(self.altered(dir, "is a file where a directory belongs", head));
            }
        };
        let mut numbered: BTreeMap<u64, (String, String)> = BTreeMap::new();
        for e in &entries {
            // ⚠ Checked before the segment-name check: a symlink or an
            // executable named like a segment (`1.jsonl`) would otherwise
            // pass that check and be read as ordinary text.
            if let Some(kind) = e.irregular {
                return Err(self.altered(
                    &format!("{dir}/{}", e.name),
                    format!("is {kind}, which fl never writes"),
                    head,
                ));
            }
            let n = if e.is_blob {
                layout::segment_number(&e.name)
            } else {
                None
            };
            let Some(n) = n else {
                return Err(self.altered(
                    &format!("{dir}/{}", e.name),
                    "is not a segment fl writes",
                    head,
                ));
            };
            numbered.insert(n, (layout::segment_path(dir, n), e.oid.clone()));
        }
        let last = numbered.len() as u64;
        if numbered.keys().copied().ne(1..=last) {
            return Err(self.altered(dir, "is missing a segment", head));
        }
        for (path, _) in self.local.cached_under(&self.repo.node_id, dir)? {
            if !numbered.values().any(|(p, _)| *p == path) {
                return Err(self.altered(&path, "was deleted", head));
            }
        }
        let mut out = Vec::new();
        for (n, (path, oid)) in numbered {
            let bytes = self.grown(head, &path, &oid, n < last, pending)?;
            out.push(Segment { path, bytes });
        }
        Ok(out)
    }

    /// `quarantine.jsonl`'s lines. ⚠ One fl cannot read is `Altered`: the
    /// quarantine file cannot quarantine itself (ruling 17).
    pub(crate) fn quarantine_lines(
        &self,
        snap: &Snapshot,
    ) -> Result<Vec<QuarantineLine>, StoreError> {
        let mut out = Vec::new();
        for (n, text) in layout::lines(&snap.quarantine) {
            match text
                .map_err(str::to_string)
                .and_then(QuarantineLine::decode)
            {
                Ok(q) => out.push(q),
                Err(cause) => {
                    return Err(self.altered(
                        QUARANTINE_FILE,
                        format!(
                            "has an unreadable line {n} ({cause}), and the quarantine file \
                             cannot quarantine its own lines"
                        ),
                        &snap.head,
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Every line of `dir` in `snap`, parsed strictly (spec §3.3), checked
    /// (§3.5 checks 5 and 6), each id once. A quarantined line is skipped
    /// and noted (§3.6).
    pub(crate) fn lines(
        &self,
        snap: &Snapshot,
        area: Area,
        dir: &str,
    ) -> Result<Vec<Line>, StoreError> {
        let skipped: BTreeMap<(String, u64), String> = self
            .quarantine_lines(snap)?
            .into_iter()
            .map(|q| ((q.file, q.line), q.reason))
            .collect();
        let mut out: Vec<Line> = Vec::new();
        let mut seen: BTreeMap<Iri, (String, u64, usize)> = BTreeMap::new();
        for seg in snap.dirs.get(dir).map(Vec::as_slice).unwrap_or_default() {
            for (n, text) in layout::lines(&seg.bytes) {
                if let Some(reason) = skipped.get(&(seg.path.clone(), n)) {
                    self.notes.borrow_mut().insert(Note::Quarantined {
                        file: seg.path.clone(),
                        line: n,
                        reason: reason.clone(),
                    });
                    continue;
                }
                let line = match text
                    .map_err(str::to_string)
                    .and_then(|t| layout::decode(area, t))
                {
                    Ok((line, _by)) => line,
                    Err(cause) => {
                        return Err(LedgerFault::Unreadable {
                            repo: self.repo.full_name.clone(),
                            file: seg.path.clone(),
                            line: n,
                            commit: self.blame(&snap.head, &seg.path, n),
                            cause,
                        }
                        .into());
                    }
                };
                // ⚠ Check 6: a line's gate, project or record matches its
                // directory.
                if line.dir() != dir {
                    return Err(LedgerFault::Misplaced {
                        repo: self.repo.full_name.clone(),
                        file: seg.path.clone(),
                        line: n,
                        belongs: line.subject().to_string(),
                        commit: self.blame(&snap.head, &seg.path, n),
                    }
                    .into());
                }
                let id = line
                    .id()
                    .cloned()
                    .expect("decode refuses a line with no id");
                match seen.get(&id) {
                    // ⚠ Check 5: the same id never appears with different
                    // content. An identical copy is read once.
                    Some((file, at, i)) => {
                        if out[*i] != line {
                            return Err(StoreError::Tampered {
                                id,
                                detail: format!(
                                    "the GitHub ledger holds it twice with different content: \
                                     `{file}` line {at} and `{}` line {n}",
                                    seg.path
                                ),
                            });
                        }
                    }
                    None => {
                        seen.insert(id, (seg.path.clone(), n, out.len()));
                        out.push(line);
                    }
                }
            }
        }
        Ok(out)
    }

    fn read(&self, area: Area, subject: &Iri) -> Result<Vec<Line>, StoreError> {
        let dir = layout::dir(area, subject);
        let snap = self.snapshot(std::slice::from_ref(&dir))?;
        self.lines(&snap, area, &dir)
    }

    /// Every run of `gate` the ledger holds. ⚠ Unreachable is an error,
    /// never an empty list (spec §2.5).
    pub fn runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        Ok(self
            .read(Area::Runs, gate.iri())?
            .into_iter()
            .filter_map(|l| match l {
                Line::Run(r) => Some(r),
                _ => None,
            })
            .collect())
    }

    pub fn attempts_of(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        Ok(self
            .read(Area::Attempts, project.iri())?
            .into_iter()
            .filter_map(|l| match l {
                Line::Attempt(a) => Some(a),
                _ => None,
            })
            .collect())
    }

    /// The decisions filed under `subject`: a finding, or a record.
    pub fn decisions(&self, subject: &Iri) -> Result<Vec<Decision>, StoreError> {
        Ok(self
            .read(Area::Decisions, subject)?
            .into_iter()
            .filter_map(|l| match l {
                Line::Decision(d) => Some(d),
                _ => None,
            })
            .collect())
    }

    /// Whether `record` is an issue of this repository. ⚠ A local answer,
    /// never a request (spec §2.1).
    pub fn owns(&self, record: &RecordId) -> Result<bool, StoreError> {
        crate::owner::issue_of_repository(
            record.iri(),
            &self.repo.full_name,
            &self.repo.node_id,
            self.local,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::git::Entry;
    use super::*;
    use crate::client::Client;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use crate::tracker::Repo;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use fl_core::MemStore;
    use fl_core::at::At;
    use fl_core::conformance::{sample_attempt, sample_decision, sample_record_run};
    use fl_core::ids::seq_iri;
    use fl_core::split::{Coverage, LedgerCache, SplitLedger};
    use fl_core::store::{Bindings, Catalog, Ledger as _, Tracker as _};
    use serde_json::{Value, json};
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

    fn ref_reads(fake: &FakeGithub) -> usize {
        fake.state()
            .requests
            .iter()
            .filter(|r| r.contains("/git/ref/heads/"))
            .count()
    }

    /// The next request whose URL contains `frag` answers `status`/`body`
    /// instead, once.
    fn body_next(fake: &FakeGithub, frag: &str, status: u16, body: Value) {
        fake.state().body_next.push((frag.into(), status, body));
    }

    /// A commit on the fake's ledger that only this test reads: what it holds
    /// does not matter to the head and format checks.
    fn commit_on(fake: &FakeGithub, text: &str) -> String {
        fake.hand_commit(&[("runs/k/1.jsonl", Some(text))])
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

    fn runs_dir() -> String {
        layout::dir(Area::Runs, gate().iri())
    }

    fn seg(n: u64) -> String {
        layout::segment_path(&runs_dir(), n)
    }

    /// A segment's text: each line and its newline.
    fn file(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    fn line(r: &GateRun) -> String {
        Line::Run(r.clone()).encode("someone")
    }

    /// The blob id the fake holds `text` under.
    fn oid_of(fake: &FakeGithub, text: &str) -> String {
        fake.state()
            .git
            .blobs
            .iter()
            .find(|(_, t)| t.as_str() == text)
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| panic!("no blob holds {text:?}"))
    }

    /// The next download of `text`'s blob answers `bytes` instead. The fake
    /// commits only text, so bytes that are not UTF-8 reach a reader this
    /// way.
    fn serve_instead(fake: &FakeGithub, text: &str, bytes: &[u8]) {
        let oid = oid_of(fake, text);
        body_next(
            fake,
            &format!("/git/blobs/{oid}"),
            200,
            json!({"content": STANDARD.encode(bytes)}),
        );
    }

    /// `bytes` with the first `from` replaced by `to`.
    fn swap(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
        let at = bytes
            .windows(from.len())
            .position(|w| w == from)
            .expect("the marker is there");
        [&bytes[..at], to, &bytes[at + from.len()..]].concat()
    }

    /// `run(n)`'s line, whose output excerpt is `excerpt`.
    fn excerpted(n: u64, excerpt: &str) -> String {
        let mut r = run(n);
        r.output_excerpt = Some(excerpt.into());
        line(&r)
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
        // ⚠ Pinned: with no last head recorded yet, a 404 is `Deleted` on
        // the FIRST read — never retried. The retry in
        // `a_branch_404_right_after_this_machines_own_write_is_read_again_before_a_deletion_is_declared`
        // is earned only by a machine that already saw a head.
        let before = ref_reads(&fake);
        let err = open(&c, &local).check_head().unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Deleted { .. })),
            "{err:?}"
        );
        assert_eq!(
            ref_reads(&fake) - before,
            1,
            "no last head recorded yet: straight to `Deleted`, no retry"
        );
    }

    // ⚠⚠ Important: the 404 retry budget must be BOUNDED — otherwise a real
    // deletion retries forever. `ref_404_next` set well past `lag_reads`
    // proves it: the correct code gives up with `Deleted` after exactly
    // `lag_reads` retries, never because GitHub eventually answered.
    #[test]
    fn a_branch_that_stays_404_past_the_lag_budget_is_declared_deleted() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let mine = commit_on(&fake, "a\n");
        local.set_last_head("R_1", &mine).unwrap();
        fake.state().ref_404_next = 10;
        let before = ref_reads(&fake);
        let err = open(&c, &local).check_head().unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Deleted { .. })),
            "{err:?}"
        );
        assert!(err.to_string().contains("Restore the branch"), "{err}");
        assert_eq!(
            ref_reads(&fake) - before,
            3,
            "the initial read plus exactly `lag_reads` (2) retries, then it gives up"
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

    // A `format` that is not UTF-8 is refused like any other, and `found`
    // shows its bytes escaped — never U+FFFD in their place.
    #[test]
    fn a_format_that_is_not_utf8_is_refused_and_reported_escaped() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger_with(&[("format", "1Z\n"), ("README.md", "x")]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        serve_instead(&fake, "1Z\n", b"1\xff\n");
        let c = client(&fake);
        match open(&c, &local).check_format().unwrap_err() {
            StoreError::Ledger(LedgerFault::UnknownFormat { found, .. }) => {
                assert_eq!(found, r#""1\xff\n""#);
            }
            other => panic!("{other:?}"),
        }
    }

    // Ruling 19, the other half: `1` with NO trailing newline is also
    // accepted — the newline is optional, not required. A comparison
    // tightened to require it (`text != "1\n"`) would refuse this silently.
    #[test]
    fn a_bare_format_with_no_trailing_newline_is_accepted() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger_with(&[("format", "1"), ("README.md", "x")]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        assert_eq!(open(&c, &local).check_format().unwrap(), root);
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
        // ⚠ Transient: nothing lasting was learned, so a report falls back
        // to the local store and a refused publish is retried later.
        assert!(
            matches!(err, StoreError::Unreachable { .. }) && err.is_transient(),
            "{err:?}"
        );
        assert!(err.to_string().contains("replica"), "{err}");
    }

    // Spec §2.5: `fl stats` over the split ledger falls back to the local
    // store, saying why, when any ledger read is answered with a server
    // error — and reads both halves once GitHub answers again.
    #[test]
    fn stats_fall_back_to_the_local_store_when_a_ledger_read_is_a_server_error() {
        let (fake, local, _root) = world();
        let p = local.add_project("/p").unwrap();
        let r = local.add_record(&p, "t").unwrap();
        local.append_attempt(sample_attempt(1, &p, &r)).unwrap();
        let remote = sample_attempt(2, &p, &r);
        let adir = layout::dir(Area::Attempts, p.iri());
        fake.hand_commit(&[(
            layout::segment_path(&adir, 1).as_str(),
            Some(file(&[Line::Attempt(remote).encode("x")]).as_str()),
        )]);
        let c = client(&fake);
        let ledger = open(&c, &local);
        let split = SplitLedger {
            local: &local,
            github: &ledger,
        };
        // Each read an attempts read makes, in order; none caches anything
        // when it fails, so the next request reaches the next read.
        for frag in ["/git/ref/heads/", "/compare/", "/graphql", "/git/blobs/"] {
            body_next(&fake, frag, 502, json!({}));
            let (attempts, coverage) = split.attempts_for_stats(&p).unwrap();
            assert_eq!(attempts.len(), 1, "{frag}: the local attempt alone");
            assert!(
                matches!(
                    coverage,
                    Coverage::LocalOnly { ref reason }
                        if reason.contains("could not be read") && reason.contains("502")
                ),
                "{frag}: {coverage:?}"
            );
        }
        let (attempts, coverage) = split.attempts_for_stats(&p).unwrap();
        assert_eq!((attempts.len(), coverage), (2, Coverage::Complete));
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

    #[test]
    fn reading_while_github_is_down_is_an_error_not_empty() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        fake.state().down = true;
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    #[test]
    fn runs_read_back_in_the_order_they_were_written_across_segments() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[
            (
                seg(1).as_str(),
                Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
            ),
            (seg(2).as_str(), Some(file(&[line(&run(3))]).as_str())),
        ]);
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1), run(2), run(3)]
        );
    }

    // ⚠ Spec §3.5 check 3: a closed segment never changes — not even by
    // growing, which an open one may.
    #[test]
    fn a_closed_segment_that_changes_is_altered() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[
            (seg(1).as_str(), Some(file(&[line(&run(1))]).as_str())),
            (seg(2).as_str(), Some(file(&[line(&run(2))]).as_str())),
        ]);
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(9))]).as_str()),
        )]);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref file, ref what, .. })
                    if *file == seg(1) && what.contains("closed")
            ),
            "{err:?}"
        );
    }

    // ⚠ Spec §3.5 check 4.
    #[test]
    fn an_open_segment_that_no_longer_starts_with_the_copy_read_before_is_altered() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
        )]);
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        let mut edited = run(1);
        edited.population = 9;
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&edited), line(&run(2))]).as_str()),
        )]);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref what, .. })
                    if what.contains("no longer starts")
            ),
            "{err:?}"
        );
    }

    #[test]
    fn an_open_segment_that_grew_is_read_again_and_cached() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
        )]);
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1), run(2)]
        );
        let cached = local.cached("R_1", &seg(1)).unwrap().unwrap();
        let cached = String::from_utf8(cached.bytes).unwrap();
        assert!(cached.contains(&line(&run(2))));
    }

    // ⚠ The segment cache must not raise false alarms (checks 3 and 4):
    // another machine growing the open segment and rolling over to a new
    // one is what the ledger is for.
    #[test]
    fn appends_and_a_rollover_by_another_machine_raise_no_alarm() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        fake.hand_commit(&[
            (
                seg(1).as_str(),
                Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
            ),
            (seg(2).as_str(), Some(file(&[line(&run(3))]).as_str())),
        ]);
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1), run(2), run(3)]
        );
        fake.hand_commit(&[(
            seg(2).as_str(),
            Some(file(&[line(&run(3)), line(&run(4))]).as_str()),
        )]);
        assert_eq!(open(&c, &local).runs(&gate()).unwrap().len(), 4);
    }

    #[test]
    fn a_segment_that_disappears_or_leaves_a_gap_is_altered() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(seg(2).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref what, .. })
                    if what.contains("missing a segment")
            ),
            "{err:?}"
        );

        let (fake2, local2, _root2) = world();
        fake2.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let c2 = client(&fake2);
        open(&c2, &local2).runs(&gate()).unwrap();
        fake2.hand_commit(&[(seg(1).as_str(), None)]);
        let err = open(&c2, &local2).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref file, ref what, .. })
                    if *file == seg(1) && what.contains("deleted")
            ),
            "{err:?}"
        );
    }

    #[test]
    fn anything_in_a_directory_but_a_segment_or_a_file_where_a_directory_belongs_is_altered() {
        let (fake, local, _root) = world();
        let stray = format!("{}/notes.txt", runs_dir());
        fake.hand_commit(&[(stray.as_str(), Some("x"))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref file, .. })
                    if *file == stray
            ),
            "{err:?}"
        );

        // A directory inside a segment directory is not a segment either.
        let (fake3, local3, _root3) = world();
        let nested = format!("{}/sub/1.jsonl", runs_dir());
        fake3.hand_commit(&[(nested.as_str(), Some("x"))]);
        let c3 = client(&fake3);
        let err = open(&c3, &local3).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref file, ref what, .. })
                    if *file == format!("{}/sub", runs_dir()) && what.contains("not a segment")
            ),
            "{err:?}"
        );

        let (fake2, local2, _root2) = world();
        fake2.hand_commit(&[(runs_dir().as_str(), Some("x"))]);
        let c2 = client(&fake2);
        let err = open(&c2, &local2).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref what, .. })
                    if what.contains("directory")
            ),
            "{err:?}"
        );
    }

    // Spec §3.3: a segment read before is not downloaded again.
    #[test]
    fn a_segment_read_before_is_not_downloaded_again() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[
            (seg(1).as_str(), Some(file(&[line(&run(1))]).as_str())),
            (seg(2).as_str(), Some(file(&[line(&run(2))]).as_str())),
        ]);
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        let before = blob_reads(&fake);
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1), run(2)]
        );
        assert_eq!(blob_reads(&fake), before, "nothing downloaded twice");
    }

    // ⚠ Spec §3.3: an unreadable line names the file, the line, the commit
    // that added it, and the quarantine command.
    #[test]
    fn an_unreadable_line_names_its_file_line_commit_and_the_quarantine_command() {
        let (fake, local, _root) = world();
        let first = fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        let damaged = format!("{}not json\n", file(&[line(&run(1))]));
        let bad = fake.hand_commit(&[(seg(1).as_str(), Some(damaged.as_str()))]);
        // A later commit that reads do not look at: the head is not `bad`.
        fake.hand_commit(&[("notes.txt", Some("x"))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        match &err {
            StoreError::Ledger(LedgerFault::Unreadable {
                file: f,
                line: n,
                commit,
                ..
            }) => {
                assert_eq!(f, &seg(1));
                assert_eq!(*n, 2);
                assert_eq!(commit, &bad);
                assert_ne!(commit, &first);
            }
            other => panic!("{other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("fl github ledger quarantine {} 2", seg(1))),
            "{msg}"
        );
        assert!(
            msg.contains("upgrade fl"),
            "a newer fl's line reads as this: {msg}"
        );
    }

    // ⚠ Readers carry BYTES. A byte that is not UTF-8 inside a JSON string
    // is an unreadable line, named — never read with U+FFFD in its place,
    // which would hand back data the ledger does not hold (and the strict
    // byte-for-byte check would compare the line with its own re-encoding,
    // which carries the same replacement character).
    #[test]
    fn a_line_that_is_not_utf8_is_unreadable_never_read_with_a_replacement() {
        let (fake, local, _root) = world();
        let text = file(&[excerpted(1, "run X")]);
        let bad = fake.hand_commit(&[(seg(1).as_str(), Some(text.as_str()))]);
        serve_instead(&fake, &text, &swap(text.as_bytes(), b"run X", b"run \xff"));
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        match &err {
            StoreError::Ledger(LedgerFault::Unreadable {
                file: f,
                line: n,
                commit,
                cause,
                ..
            }) => {
                assert_eq!((f.as_str(), *n), (seg(1).as_str(), 1));
                assert_eq!(commit, &bad);
                assert!(cause.contains("not valid UTF-8"), "{cause}");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            err.to_string()
                .contains(&format!("fl github ledger quarantine {} 1", seg(1))),
            "{err}"
        );
    }

    // ⚠ Check 4 compares BYTES: a commit that rewrites one invalid byte
    // into another is a rewrite, even though both decode lossily to the
    // same text.
    #[test]
    fn a_rewrite_from_one_invalid_byte_to_another_is_caught_by_a_reader() {
        let (fake, local, _root) = world();
        let one = file(&[excerpted(1, "run X")]);
        fake.hand_commit(&[(seg(1).as_str(), Some(one.as_str()))]);
        serve_instead(&fake, &one, &swap(one.as_bytes(), b"run X", b"run \xff"));
        let c = client(&fake);
        assert!(
            open(&c, &local).runs(&gate()).is_err(),
            "line 1 is unreadable"
        );
        let two = file(&[excerpted(1, "run Y")]);
        fake.hand_commit(&[(seg(1).as_str(), Some(two.as_str()))]);
        serve_instead(&fake, &two, &swap(two.as_bytes(), b"run Y", b"run \xfe"));
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref file, ref what, .. })
                    if *file == seg(1) && what.contains("no longer starts")
            ),
            "{err:?}"
        );
    }

    // ⚠ `quarantine.jsonl` is decoded strictly too: a byte that is not
    // UTF-8 there is `Altered` (the quarantine file cannot quarantine its
    // own lines), never a reason read with U+FFFD in it.
    #[test]
    fn a_quarantine_file_line_that_is_not_utf8_is_altered() {
        let (fake, local, _root) = world();
        let q = QuarantineLine {
            id: seq_iri(50),
            at: At::from_unix_millis(50),
            file: seg(1),
            line: 1,
            quarantined_by: "Ada".into(),
            reason: "a hand edit X".into(),
            by: "fake-user".into(),
        };
        let text = file(&[q.encode()]);
        fake.hand_commit(&[(QUARANTINE_FILE, Some(text.as_str()))]);
        serve_instead(
            &fake,
            &text,
            &swap(text.as_bytes(), b"edit X", b"edit \xff"),
        );
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref file, ref what, .. })
                    if file == QUARANTINE_FILE
                        && what.contains("unreadable line 1")
                        && what.contains("not valid UTF-8")
            ),
            "{err:?}"
        );
    }

    // ⚠ Spec §3.1/§3.5 check 4: `layout::lines` refuses a cut-short last
    // segment (no final newline) as "it does not end with a newline: it was
    // cut short" — a reader must surface that as tampering BEFORE any
    // append could ever be planned on top of it.
    #[test]
    fn a_cut_short_open_segment_is_reported_as_unreadable() {
        let (fake, local, _root) = world();
        let cut = format!("{}\nnot terminated", line(&run(1)));
        let bad = fake.hand_commit(&[(seg(1).as_str(), Some(cut.as_str()))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        match &err {
            StoreError::Ledger(LedgerFault::Unreadable {
                file: f,
                line: n,
                commit,
                cause,
                ..
            }) => {
                assert_eq!(f, &seg(1));
                assert_eq!(*n, 2);
                assert_eq!(commit, &bad);
                assert!(cause.contains("cut short"), "{cause}");
            }
            other => panic!("{other:?}"),
        }
    }

    // ⚠ Spec §3.1: a new field means a new format. A line a newer fl wrote
    // with a field this fl does not know is unreadable here, and the
    // message says a newer fl may have written it.
    #[test]
    fn a_line_with_a_field_this_fl_does_not_write_is_unreadable_and_says_to_upgrade() {
        let (fake, local, _root) = world();
        let mut v: serde_json::Value = serde_json::from_str(&line(&run(1))).unwrap();
        v["retried"] = serde_json::Value::Bool(true);
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[v.to_string()]).as_str()))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Unreadable { line: 1, .. })
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("upgrade fl"), "{err}");
    }

    // Spec §3.6: a quarantined line is skipped and reported; nothing is
    // removed.
    #[test]
    fn a_quarantined_line_is_skipped_and_noted_once() {
        let (fake, local, _root) = world();
        let q = QuarantineLine {
            id: seq_iri(50),
            at: At::from_unix_millis(50),
            file: seg(1),
            line: 2,
            quarantined_by: "Ada".into(),
            reason: "a hand edit".into(),
            by: "fake-user".into(),
        };
        let damaged = format!(
            "{}not json\n{}",
            file(&[line(&run(1))]),
            file(&[line(&run(3))])
        );
        fake.hand_commit(&[
            (seg(1).as_str(), Some(damaged.as_str())),
            (QUARANTINE_FILE, Some(file(&[q.encode()]).as_str())),
        ]);
        let c = client(&fake);
        let l = open(&c, &local);
        assert_eq!(l.runs(&gate()).unwrap(), vec![run(1), run(3)]);
        l.runs(&gate()).unwrap();
        assert_eq!(
            l.take_notes(),
            vec![Note::Quarantined {
                file: seg(1),
                line: 2,
                reason: "a hand edit".into(),
            }],
            "once, however often it is read"
        );
        assert!(
            fake.ledger_files()[&seg(1)].contains("not json"),
            "nothing was removed"
        );
    }

    // ⚠ Spec §3.5 check 6 and §7: a line in the wrong directory is
    // tampering, naming the file and the commit; quarantine is offered too.
    #[test]
    fn a_line_in_the_wrong_directory_is_reported_as_tampering() {
        let (fake, local, _root) = world();
        let elsewhere = sample_record_run(1, &GateId(seq_iri(8)), Some(&record()));
        let added =
            fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&elsewhere)]).as_str()))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Misplaced { line: 1, ref commit, .. })
                    if *commit == added
            ),
            "{err:?}"
        );
        let msg = err.to_string();
        for part in [
            "was altered",
            "fl github ledger verify",
            "fl github ledger quarantine",
        ] {
            assert!(msg.contains(part), "{part}: {msg}");
        }
    }

    // ⚠ Spec §3.5 check 5: one id, one content.
    #[test]
    fn one_id_with_two_contents_is_tampered_and_an_identical_copy_is_read_once() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(1)), line(&run(2))]).as_str()),
        )]);
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1), run(2)]
        );
        let mut other = run(1);
        other.commit = "def".into();
        fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(1)), line(&run(2)), line(&other)]).as_str()),
        )]);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        let want = run(1).id.unwrap();
        assert!(
            matches!(err, StoreError::Tampered { ref id, .. } if *id == want),
            "{err:?}"
        );
    }

    #[test]
    fn an_unreadable_or_vanished_quarantine_file_is_altered() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[(QUARANTINE_FILE, Some("not json\n"))]);
        let c = client(&fake);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref file, .. })
                    if file == QUARANTINE_FILE
            ),
            "{err:?}"
        );

        let (fake2, local2, _root2) = world();
        let q = QuarantineLine {
            id: seq_iri(50),
            at: At::from_unix_millis(50),
            file: seg(1),
            line: 1,
            quarantined_by: "Ada".into(),
            reason: "r".into(),
            by: "fake-user".into(),
        };
        fake2.hand_commit(&[(QUARANTINE_FILE, Some(file(&[q.encode()]).as_str()))]);
        let c2 = client(&fake2);
        open(&c2, &local2).runs(&gate()).unwrap();
        fake2.hand_commit(&[(QUARANTINE_FILE, None)]);
        let err = open(&c2, &local2).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref what, .. })
                    if what.contains("deleted")
            ),
            "{err:?}"
        );
    }

    // ⚠ Spec §3.5 check 4 applies to `quarantine.jsonl` as to an open
    // segment: it only grows. And it is a file.
    #[test]
    fn a_quarantine_file_that_lost_a_line_or_is_a_directory_is_altered() {
        let quarantined = |n: u64| {
            QuarantineLine {
                id: seq_iri(50 + n),
                at: At::from_unix_millis(50 + n),
                file: seg(1),
                line: n,
                quarantined_by: "Ada".into(),
                reason: "r".into(),
                by: "fake-user".into(),
            }
            .encode()
        };
        let (fake, local, _root) = world();
        fake.hand_commit(&[(
            QUARANTINE_FILE,
            Some(file(&[quarantined(1), quarantined(2)]).as_str()),
        )]);
        let c = client(&fake);
        open(&c, &local).runs(&gate()).unwrap();
        fake.hand_commit(&[(QUARANTINE_FILE, Some(file(&[quarantined(2)]).as_str()))]);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref file, ref what, .. })
                    if file == QUARANTINE_FILE && what.contains("no longer starts")
            ),
            "{err:?}"
        );

        let (fake2, local2, _root2) = world();
        fake2.hand_commit(&[("quarantine.jsonl/x", Some("y"))]);
        let c2 = client(&fake2);
        let err = open(&c2, &local2).runs(&gate()).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref what, .. })
                    if what.contains("directory")
            ),
            "{err:?}"
        );
    }

    #[test]
    fn attempts_and_decisions_read_from_their_own_directories() {
        let (fake, local, _root) = world();
        let p = ProjectId(seq_iri(8));
        let a = sample_attempt(2, &p, &record());
        let d = sample_decision(3, &record(), vec![]);
        let adir = layout::dir(Area::Attempts, p.iri());
        let ddir = layout::dir(Area::Decisions, record().iri());
        fake.hand_commit(&[
            (
                layout::segment_path(&adir, 1).as_str(),
                Some(file(&[Line::Attempt(a.clone()).encode("x")]).as_str()),
            ),
            (
                layout::segment_path(&ddir, 1).as_str(),
                Some(file(&[Line::Decision(d.clone()).encode("x")]).as_str()),
            ),
        ]);
        let c = client(&fake);
        let l = open(&c, &local);
        assert_eq!(l.attempts_of(&p).unwrap(), vec![a]);
        assert_eq!(l.decisions(record().iri()).unwrap(), vec![d]);
    }

    // Spec §2.1: ownership is a local answer, with no request.
    #[test]
    fn a_record_this_repository_holds_is_owned_and_another_is_not() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let before = fake.state().requests.len();
        assert!(l.owns(&record()).unwrap());
        let theirs = RecordId(Iri::parse("https://github.com/acme/other/issues/1").unwrap());
        assert!(!l.owns(&theirs).unwrap());
        assert_eq!(fake.state().requests.len(), before, "no request");
    }

    // ⚠ Ruling 24, applied beyond the format file: right after this
    // machine's own append, a replica that does not yet know the head
    // commit must raise no alarm reading a directory's segments either.
    #[test]
    fn a_graphql_replica_that_has_not_seen_this_machines_own_append_raises_no_alarm_reading_segments()
     {
        let (fake, local, _root) = world();
        let c = client(&fake);
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        open(&c, &local).runs(&gate()).unwrap();
        let mine = fake.hand_commit(&[(
            seg(1).as_str(),
            Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
        )]);
        local.set_last_head("R_1", &mine).unwrap();
        fake.state().graphql_commit_unknown_next = 2;
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1), run(2)],
            "two lagging GraphQL reads after this machine's own append, then it \
             catches up: no alarm"
        );
    }

    // ⚠ Spec §3.5 checks 3 and 4: a segment's cache entry must land together
    // with the new last head, never before it. Steps: a normal read; seg1
    // grows AND a new seg2 appears in the same commit (so both need a fresh
    // download); seg2's blob download fails once, after seg1's has already
    // passed its own growth check — the read as a whole must fail, and
    // nothing from it may be cached, so a later read at the SAME, still
    // unmoved head raises no alarm.
    #[test]
    fn a_blob_download_that_fails_after_an_earlier_segment_already_passed_raises_no_later_alarm() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        open(&c, &local).runs(&gate()).unwrap();

        let seg2_text = file(&[line(&run(3))]);
        fake.hand_commit(&[
            (
                seg(1).as_str(),
                Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
            ),
            (seg(2).as_str(), Some(seg2_text.as_str())),
        ]);
        let seg2_oid = fake
            .state()
            .git
            .blobs
            .iter()
            .find(|(_, t)| **t == seg2_text)
            .map(|(id, _)| id.clone())
            .expect("seg2's blob exists on the fake");
        body_next(&fake, &format!("/git/blobs/{seg2_oid}"), 502, json!({}));
        open(&c, &local).runs(&gate()).unwrap_err();

        // A replica now serves the OLD head again — one behind the true,
        // current one, exactly what `check_head` sees right after this
        // machine's own write, before every replica has caught up. The
        // failed read above must not have cached anything that contradicts
        // it.
        fake.state().ref_behind_next = 1;
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1)],
            "no alarm reading the old head again"
        );

        // Once a replica serves the real head again, it reads back in
        // full — seg2's blob, never cached by the failed attempt, downloads
        // cleanly.
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1), run(2), run(3)]
        );
    }

    // ⚠ Spec §3.5 check 5, across segments: `seen` must span every segment
    // of a directory, not reset per segment — the same id can land in two
    // different segments as easily as twice in one.
    #[test]
    fn the_same_id_in_two_different_segments_with_different_content_is_tampered() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[
            (seg(1).as_str(), Some(file(&[line(&run(1))]).as_str())),
            (seg(2).as_str(), Some(file(&[line(&run(2))]).as_str())),
        ]);
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1), run(2)]
        );
        let mut other = run(1);
        other.commit = "def".into();
        fake.hand_commit(&[(
            seg(2).as_str(),
            Some(file(&[line(&run(2)), line(&other)]).as_str()),
        )]);
        let err = open(&c, &local).runs(&gate()).unwrap_err();
        let want = run(1).id.unwrap();
        assert!(
            matches!(err, StoreError::Tampered { ref id, .. } if *id == want),
            "{err:?}"
        );
    }

    #[test]
    fn the_same_id_in_two_different_segments_with_identical_content_is_read_once() {
        let (fake, local, _root) = world();
        fake.hand_commit(&[
            (seg(1).as_str(), Some(file(&[line(&run(1))]).as_str())),
            (
                seg(2).as_str(),
                Some(file(&[line(&run(1)), line(&run(2))]).as_str()),
            ),
        ]);
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).runs(&gate()).unwrap(),
            vec![run(1), run(2)]
        );
    }

    // ⚠ `blame` never errors: when GitHub's GraphQL cannot answer it, the
    // unreadable-line message still names the file and the line, and says
    // the commit is unknown rather than silently naming the wrong one.
    #[test]
    fn an_unreadable_lines_blame_that_fails_still_names_the_file_and_line_as_unknown() {
        let (fake, local, _root) = world();
        let damaged = format!("not json\n{}", file(&[line(&run(2))]));
        fake.hand_commit(&[(seg(1).as_str(), Some(damaged.as_str()))]);
        let c = client(&fake);
        let l = open(&c, &local);
        let dir = runs_dir();
        let snap = l.snapshot(std::slice::from_ref(&dir)).unwrap();
        // The directory snapshot's own GraphQL call already landed; only
        // `blame`'s call (made inside `lines`) is left to intercept.
        body_next(&fake, "/graphql", 502, json!({}));
        let err = l.lines(&snap, Area::Runs, &dir).unwrap_err();
        match &err {
            StoreError::Ledger(LedgerFault::Unreadable {
                file: f,
                line: n,
                commit,
                ..
            }) => {
                assert_eq!(f, &seg(1));
                assert_eq!(*n, 1);
                assert!(commit.contains("unknown"), "{commit}");
            }
            other => panic!("{other:?}"),
        }
    }

    // A symlink or an executable named like a segment
    // (`1.jsonl`) must never be read as ordinary text — `directory` checks
    // `Entry::irregular` before it ever asks whether the name looks like a
    // segment.
    #[test]
    fn a_segment_shaped_symlink_is_reported_as_altered() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        let l = open(&c, &local);
        let entries = vec![Entry {
            name: "1.jsonl".into(),
            oid: "a".repeat(40),
            is_blob: true,
            irregular: Some("a symlink"),
        }];
        let mut pending = Vec::new();
        let err = l
            .directory(
                "deadbeef",
                &runs_dir(),
                Some(&Object::Tree(entries)),
                &mut pending,
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Ledger(LedgerFault::Altered { ref what, .. })
                    if what.contains("a symlink")
            ),
            "{err:?}"
        );
    }

    // ⚠ Spec §3.5 check 4, extended: a cut-short copy must never become the
    // trusted baseline for a later comparison — any later text trivially
    // "starts with" it. A good read first establishes a real baseline; a
    // later cut-short growth must leave that baseline exactly as it was.
    #[test]
    fn a_cut_short_segment_is_not_cached_so_the_trusted_copy_is_unchanged() {
        let (fake, local, _root) = world();
        let c = client(&fake);
        fake.hand_commit(&[(seg(1).as_str(), Some(file(&[line(&run(1))]).as_str()))]);
        open(&c, &local).runs(&gate()).unwrap();
        let good = local.cached("R_1", &seg(1)).unwrap().unwrap();

        let cut = format!(
            "{}{}\nnot terminated",
            file(&[line(&run(1))]),
            line(&run(2))
        );
        fake.hand_commit(&[(seg(1).as_str(), Some(cut.as_str()))]);
        open(&c, &local).runs(&gate()).unwrap_err();

        assert_eq!(
            local.cached("R_1", &seg(1)).unwrap().unwrap(),
            good,
            "a cut-short read must not replace the trusted baseline"
        );
    }
}
