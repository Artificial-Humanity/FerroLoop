//! The router's escalation (routing spec §3): every check the GitHub create
//! would make, before anything is written; then the three steps — mark the
//! local item, find or create its issue, replace the local item with a
//! tombstone — each of which a rerun resumes.

use crate::escalation::{EscalationFault, Mark, Outgoing, Provenance};
use crate::finding::{Finding, FindingState};
use crate::ids::{FindingId, Kind, RecordId};
use crate::iri::Iri;
use crate::model::{Record, State};
use crate::routing::{RoutingMap, Tier};
use crate::store::StoreError;
use crate::tiered::{RecordSeen, TieredTracker};

/// The longest issue title the GitHub tracker writes: GitHub's limit, in
/// characters (routing spec §3.2). `fl_github`'s `TITLE_MAX` is this one,
/// so the check before the mark and the tracker's own cannot drift.
pub const ISSUE_TITLE_MAX: usize = 256;

/// GitHub's limit on an issue's body, in characters: a longer one is
/// refused (a 422).
pub const ISSUE_BODY_MAX: usize = 65_536;

/// The most an escalation's unbounded parts may weigh in its issue's body
/// (routing spec §3.2): in bytes of UTF-8, so the bound holds however
/// GitHub counts characters, and each byte at the most its escaping can
/// make it ([`body_parts`]). It leaves 15,536 of [`ISSUE_BODY_MAX`] for
/// what the GitHub tracker bounds by construction: a record's list of open
/// findings (at most about 11,000 bytes), the words of the lines around
/// the parts, and the block's other fields — short IRIs, names and words.
pub const ESCALATION_BODY_BUDGET: usize = 50_000;

/// The most bytes one byte of a part becomes where the issue's text shows
/// it, escaped as Markdown: eight (`@` becomes `@&#8203;`).
const SHOWN: usize = 8;

/// The most bytes one byte of a part becomes in the issue's block, escaped
/// as JSON: six (`<` becomes `\u003c`, a control character `\u0001`).
const KEPT: usize = 6;

/// An escalation checked and ready to run (routing spec §3.2): what goes
/// out, and where an earlier run stopped. Only the router builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    id: Iri,
    kind: Kind,
    outgoing: Outgoing,
    resumes: Option<Mark>,
    found: Option<Iri>,
}

impl Prepared {
    /// The item's primary local IRI: its issue's create key and first alias.
    pub fn id(&self) -> &Iri {
        &self.id
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// What the escalation writes to GitHub, for the warning the CLI prints
    /// before it publishes to a repository that is not private.
    pub fn outgoing(&self) -> &Outgoing {
        &self.outgoing
    }

    /// The mark an earlier run left: this run resumes it, with its own who,
    /// why and time.
    pub fn resumes(&self) -> Option<&Mark> {
        self.resumes.as_ref()
    }

    /// The issue an earlier run made — found by its create key, from the
    /// mark an earlier run left, or because it holds the item's own IRI as
    /// an alias although the item is not marked (an `--abandon` whose
    /// search could not see it yet): `escalate` finishes it — its labels,
    /// if a stop left them off, then the tombstone.
    pub fn found(&self) -> Option<&Iri> {
        self.found.as_ref()
    }
}

/// A local item, read for its escalation.
enum Local {
    Record(Record),
    Finding(Finding),
}

impl Local {
    /// The primary IRI.
    fn id(&self) -> &Iri {
        match self {
            Local::Record(r) => r.id.iri(),
            Local::Finding(f) => f.id.iri(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            Local::Record(_) => Kind::Record,
            Local::Finding(_) => Kind::Finding,
        }
    }

    /// The closed state it is in, if any (routing spec §3.2): a record
    /// `done`, a finding `fixed` or `withdrawn`.
    fn closed(&self) -> Option<&'static str> {
        match self {
            Local::Record(r) => (r.state == State::Done).then(|| r.state.as_wire()),
            Local::Finding(f) => matches!(f.state, FindingState::Fixed | FindingState::Withdrawn)
                .then(|| f.state.as_wire()),
        }
    }
}

/// Whether an item in `area` is sensitive (decisions 21, 22): an area the
/// map marks so, or one it no longer declares, which may have been. An item
/// with no area — made before the project was routed — is not.
fn sensitive(map: &RoutingMap, area: Option<&str>) -> bool {
    area.is_some_and(|a| map.route(a).is_none_or(|r| r.sensitive))
}

/// The parts of an escalated item's issue body that nothing else bounds:
/// what each is, its length in bytes, and the most bytes one of its bytes
/// becomes there. (A record's open findings are not among them: the
/// tracker bounds that list itself.)
fn body_parts(out: &Outgoing, by: &str, reason: &str) -> Vec<(&'static str, usize, usize)> {
    let mut parts = Vec::new();
    let aliases = match out {
        Outgoing::Record { record, .. } => &record.also_known_as,
        Outgoing::Finding { finding, record } => {
            // The issue's text, as written.
            parts.push(("the finding's claim", finding.claim.len(), 1));
            // A local record is named in a line, and its title kept in the
            // block; a record on GitHub is named by its issue alone.
            if record.tier == Tier::Local {
                parts.push((
                    "the title of the record it is about",
                    record.title.len(),
                    SHOWN + KEPT,
                ));
            }
            &finding.also_known_as
        }
    };
    // Who and why are shown in the line naming where the issue came from,
    // and kept in the block.
    parts.push(("the name of who escalates it", by.len(), SHOWN + KEPT));
    parts.push(("the reason", reason.len(), SHOWN + KEPT));
    // Kept in the block; six bytes a byte covers each one's quotes and comma.
    let aliases = aliases.iter().map(|a| a.as_str().len()).sum();
    parts.push(("the list of its aliases", aliases, KEPT));
    parts
}

/// Why GitHub would refuse the body of `out`'s issue, escalated by `by`
/// for `reason`, if it might: its parts outweigh [`ESCALATION_BODY_BUDGET`].
/// The refusal names the heaviest part, and the most of it that fits
/// beside the others.
fn body_refused(id: &Iri, out: &Outgoing, by: &str, reason: &str) -> Option<EscalationFault> {
    let parts = body_parts(out, by, reason);
    let weight = |(_, len, per): &(&str, usize, usize)| len.saturating_mul(*per);
    let total = parts.iter().map(weight).fold(0, usize::saturating_add);
    if total <= ESCALATION_BODY_BUDGET {
        return None;
    }
    let heaviest = parts.iter().max_by_key(|p| weight(p))?;
    let (what, len, per) = *heaviest;
    let others = total - weight(heaviest);
    Some(EscalationFault::TooLong {
        id: id.clone(),
        what: what.to_string(),
        len,
        max: ESCALATION_BODY_BUDGET.saturating_sub(others) / per,
    })
}

/// Why the GitHub tracker would refuse `title` for an issue, if it would.
fn title_refused(title: &str) -> Option<String> {
    if title.is_empty() {
        return Some("it is empty, and an issue needs a title".into());
    }
    let n = title.chars().count();
    if n > ISSUE_TITLE_MAX {
        return Some(format!(
            "it is {n} characters, and GitHub's limit is {ISSUE_TITLE_MAX}"
        ));
    }
    if title.trim() != title {
        return Some("it starts or ends with whitespace, which GitHub may trim".into());
    }
    None
}

impl TieredTracker<'_> {
    /// The local item `id` names, as the `kind` asked for (routing spec
    /// §3.1): an id GitHub claims is not local, even when the local tier
    /// holds it as an alias; a tombstoned one names where it went.
    fn local_item(&self, id: &Iri, kind: Kind) -> Result<Local, StoreError> {
        let not_local = || StoreError::from(EscalationFault::NotLocal { id: id.clone() });
        if self.github.claims(id) {
            return Err(not_local());
        }
        if let Some(t) = self.escalations.tombstone_of(id)? {
            return Err(EscalationFault::AlreadyEscalated {
                id: t.from,
                to: t.to,
            }
            .into());
        }
        let found = match self.catalog.kind_of(id) {
            Err(StoreError::NotOwned { .. }) => return Err(not_local()),
            other => other?,
        };
        let expected = match kind {
            Kind::Finding => Kind::Finding,
            _ => Kind::Record,
        };
        if found != expected {
            return Err(StoreError::WrongKind {
                id: id.clone(),
                expected,
                found,
            });
        }
        if expected == Kind::Record {
            let id = RecordId(id.clone());
            let r = self.local.get_record(&id)?;
            r.map(Local::Record).ok_or(StoreError::NoSuchRecord(id))
        } else {
            let id = FindingId(id.clone());
            let f = self.local.get_finding(&id)?;
            f.map(Local::Finding).ok_or(StoreError::NoSuchFinding(id))
        }
    }

    /// The one-namespace rule (routing spec §3.2): no name of the item may
    /// already name something on GitHub — except the item's own issue.
    /// Answers that issue, when there is one.
    ///
    /// ⚠ An `--abandon` whose search could not see the issue yet removes
    /// the mark although the issue exists, and holds the item's primary IRI
    /// as an alias. The search by its create key, with no time bound, tells
    /// it from another issue's alias; only that case pays for it. The
    /// issue it finds passed every check when it was made, and carries
    /// every alias of the item, so the rest are not asked.
    fn names_free(&self, id: &Iri, aliases: &[Iri]) -> Result<Option<Iri>, StoreError> {
        for alias in std::iter::once(id).chain(aliases) {
            if let Some(issue) = self.github.alias_taken(alias)? {
                if alias == id && self.github.find_escalated(id, 0)?.as_ref() == Some(&issue) {
                    return Ok(Some(issue));
                }
                return Err(EscalationFault::AliasTaken {
                    alias: alias.clone(),
                    issue,
                }
                .into());
            }
        }
        Ok(None)
    }

    /// The record's open findings in both tiers, about it by its IRI or any
    /// of its aliases (decision 18), as the record's issue lists them: never
    /// a security finding, nor one in a sensitive area or an area `map` no
    /// longer declares (decisions 21, 22) — the list is published.
    fn open_findings(&self, record: &Record, map: &RoutingMap) -> Result<Vec<Finding>, StoreError> {
        let about =
            |f: &Finding| f.record == record.id || record.also_known_as.contains(f.record.iri());
        Ok(self
            .findings(&record.project, None)?
            .into_iter()
            .map(|(_, f)| f)
            .filter(|f| !matches!(f.state, FindingState::Fixed | FindingState::Withdrawn))
            .filter(|f| !f.security && !sensitive(map, f.area.as_deref()))
            .filter(about)
            .collect())
    }

    /// Every check the GitHub create would make, before anything is written
    /// (routing spec §3.2), and what the escalation would write. Writes
    /// nothing.
    ///
    /// ⚠ On an item an earlier run marked, the search comes first: an issue
    /// it finds passed these checks when it was made — and its aliases now
    /// include the item's own IRI — so only finishing it is left. With
    /// none found, every check runs again: the repository may have turned
    /// public since.
    pub fn prepare_escalation(&self, id: &Iri, kind: Kind) -> Result<Prepared, StoreError> {
        let item = self.local_item(id, kind)?;
        let primary = item.id().clone();
        let project = match &item {
            Local::Record(r) => &r.project,
            Local::Finding(f) => &f.project,
        };
        let map = self.map_of(project)?;
        self.github.tracker()?;
        let resumes = self.escalations.mark_of(&primary)?;
        let mut found = match &resumes {
            Some(mark) => self.github.find_escalated(&primary, mark.at_ms)?,
            None => None,
        };
        let check = found.is_none();
        if check && let Some(state) = item.closed() {
            return Err(EscalationFault::Closed {
                id: primary,
                state: state.to_string(),
            }
            .into());
        }
        let outgoing = match item {
            Local::Record(record) => {
                if check {
                    if let Some(why) = title_refused(&record.title) {
                        return Err(EscalationFault::Title { id: primary, why }.into());
                    }
                    if sensitive(&map, record.area.as_deref()) {
                        self.private_or_refuse(false, "this record")?;
                    }
                    found = self.names_free(&primary, &record.also_known_as)?;
                }
                let findings = self.open_findings(&record, &map)?;
                Outgoing::Record { record, findings }
            }
            Local::Finding(finding) => {
                let (tier, record) = self.record_of(&finding.record)?;
                if check {
                    // Routing spec decision 21: a finding about a local record in a
                    // sensitive area would publish that record's title.
                    let own = finding.security || sensitive(&map, finding.area.as_deref());
                    let about = tier == Tier::Local && sensitive(&map, record.area.as_deref());
                    if own || about {
                        self.private_or_refuse(
                            false,
                            if own {
                                "this finding"
                            } else {
                                "this finding, about a record in a sensitive area,"
                            },
                        )?;
                    }
                    found = self.names_free(&primary, &finding.also_known_as)?;
                }
                let record = RecordSeen {
                    id: record.id,
                    title: record.title,
                    tier,
                };
                Outgoing::Finding { finding, record }
            }
        };
        Ok(Prepared {
            id: primary,
            kind: outgoing.kind(),
            outgoing,
            resumes,
            found,
        })
    }

    /// Run the escalation `at` (routing spec §3.3): mark the local item,
    /// find or create its issue, replace the local item with a tombstone.
    /// Answers the issue.
    ///
    /// ⚠ A run that resumes a mark keeps the mark's who, why and time, and
    /// ignores `by`, `reason` and `now_ms`. Any error after the mark leaves
    /// it: the item stays unwritable, and a rerun resumes. A run that marks
    /// first checks that `by` and `reason` fit the issue's body (routing
    /// spec §3.2), so GitHub's refusal of it never strands a mark.
    ///
    /// ⚠ An issue an earlier run made goes through `create_escalated` too:
    /// it searches first and makes no second issue, and it gives a found
    /// issue the labels a stop between the create and the label call left
    /// off (routing spec §3.3) — a tombstone alone would point at an issue
    /// fl cannot read.
    pub fn escalate(
        &self,
        at: &Prepared,
        by: &str,
        reason: &str,
        now_ms: u64,
    ) -> Result<Iri, StoreError> {
        let mark = match &at.resumes {
            Some(mark) => mark.clone(),
            None => {
                if let Some(fault) = body_refused(&at.id, &at.outgoing, by, reason) {
                    return Err(fault.into());
                }
                let mark = Mark {
                    by: by.to_string(),
                    reason: reason.to_string(),
                    at_ms: now_ms,
                };
                self.escalations.mark(&at.id, &mark)?;
                mark
            }
        };
        let from = Provenance {
            from: at.id.clone(),
            by: mark.by,
            reason: mark.reason,
        };
        // A found issue is found again by the create's search, however old:
        // the search stops at it. One an `--abandon` left behind is older
        // than this run's mark, so the search reaches back without bound.
        let since_ms = if at.found.is_some() { 0 } else { mark.at_ms };
        let issue = self
            .github
            .create_escalated(&at.outgoing, &from, since_ms)?;
        self.escalations.tombstone(&at.id, &issue)?;
        Ok(issue)
    }

    /// `--abandon` (routing spec §3.3): remove the mark, only after the
    /// search proves no issue exists. ⚠ Once the issue exists, the only way
    /// on is the tombstone: `escalate` finishes it.
    pub fn abandon_escalation(&self, id: &Iri, kind: Kind) -> Result<(), StoreError> {
        let item = self.local_item(id, kind)?;
        let primary = item.id().clone();
        let Some(mark) = self.escalations.mark_of(&primary)? else {
            return Err(EscalationFault::NotMarked { id: primary }.into());
        };
        self.github.tracker()?;
        if let Some(issue) = self.github.find_escalated(&primary, mark.at_ms)? {
            return Err(EscalationFault::IssueExists {
                id: primary,
                kind: item.kind(),
                issue,
            }
            .into());
        }
        self.escalations.unmark(&primary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::escalation::{Escalations, Tombstone};
    use crate::ids::{ProjectId, seq_iri};
    use crate::mem_issues::{MemIssues, VISIBILITY_UNREAD};
    use crate::routing::{ForeignRecord, GithubTier, RoutingFault};
    use crate::store::{Catalog, Tracker};
    use std::cell::{Cell, RefCell};

    /// The in-memory GitHub tier, every `create_escalated` it was asked —
    /// the provenance, and the time its search reached back to — and how
    /// many `find_escalated` searches it was asked. With
    /// `alias_held_by` set, every alias reads as that issue's — an alias a
    /// person wrote into another issue by hand, which the in-memory tier's
    /// one namespace would refuse.
    #[derive(Default)]
    struct Watched {
        issues: MemIssues,
        creates_asked: RefCell<Vec<(Provenance, u64)>>,
        alias_held_by: RefCell<Option<Iri>>,
        searches: Cell<u32>,
    }

    impl std::ops::Deref for Watched {
        type Target = MemIssues;

        fn deref(&self) -> &MemIssues {
            &self.issues
        }
    }

    impl GithubTier for Watched {
        fn available(&self) -> bool {
            self.issues.available()
        }

        fn claims(&self, id: &Iri) -> bool {
            self.issues.claims(id)
        }

        fn issue_form(&self, id: &Iri) -> bool {
            self.issues.issue_form(id)
        }

        fn tracker(&self) -> Result<&dyn Tracker, StoreError> {
            self.issues.tracker()
        }

        fn require_private(&self) -> Result<(), StoreError> {
            self.issues.require_private()
        }

        fn items_in_area(
            &self,
            project: &ProjectId,
            area: &str,
        ) -> Result<Vec<(Kind, Iri)>, StoreError> {
            self.issues.items_in_area(project, area)
        }

        fn find_escalated(&self, key: &Iri, since_ms: u64) -> Result<Option<Iri>, StoreError> {
            self.searches.set(self.searches.get() + 1);
            self.issues.find_escalated(key, since_ms)
        }

        fn alias_taken(&self, alias: &Iri) -> Result<Option<Iri>, StoreError> {
            if let Some(issue) = self.alias_held_by.borrow().clone() {
                return Ok(Some(issue));
            }
            self.issues.alias_taken(alias)
        }

        fn create_escalated(
            &self,
            item: &Outgoing,
            from: &Provenance,
            since_ms: u64,
        ) -> Result<Iri, StoreError> {
            self.creates_asked
                .borrow_mut()
                .push((from.clone(), since_ms));
            self.issues.create_escalated(item, from, since_ms)
        }
    }

    /// The local store, whose next `tombstone` fails before it writes
    /// anything: a stop after the issue exists and before step 3.
    struct TombstoneFailsOnce<'a> {
        local: &'a MemStore,
        fail: Cell<bool>,
    }

    impl Escalations for TombstoneFailsOnce<'_> {
        fn mark(&self, id: &Iri, mark: &Mark) -> Result<(), StoreError> {
            self.local.mark(id, mark)
        }

        fn mark_of(&self, id: &Iri) -> Result<Option<Mark>, StoreError> {
            self.local.mark_of(id)
        }

        fn unmark(&self, id: &Iri) -> Result<(), StoreError> {
            self.local.unmark(id)
        }

        fn tombstone(&self, id: &Iri, to: &Iri) -> Result<Tombstone, StoreError> {
            if self.fail.replace(false) {
                return Err(StoreError::Backend("the disk is full".into()));
            }
            self.local.tombstone(id, to)
        }

        fn tombstone_of(&self, id: &Iri) -> Result<Option<Tombstone>, StoreError> {
            self.local.tombstone_of(id)
        }
    }

    /// A project with the starting map in a local `MemStore`, and an
    /// in-memory GitHub tier.
    struct W {
        local: MemStore,
        issues: Watched,
        p: ProjectId,
    }

    fn world() -> W {
        let local = MemStore::default();
        let p = local.add_project("/p").unwrap();
        local.set_routes(&p, &RoutingMap::starting()).unwrap();
        W {
            local,
            issues: Watched::default(),
            p,
        }
    }

    const BY: &str = "alice";
    const WHY: &str = "a person decides";
    const NOW: u64 = 10;

    impl W {
        fn router(&self) -> TieredTracker<'_> {
            self.over(&self.local)
        }

        /// The router, with `escalations` as the local tier's marks and
        /// tombstones.
        fn over<'a>(&'a self, escalations: &'a dyn Escalations) -> TieredTracker<'a> {
            TieredTracker {
                catalog: &self.local,
                local: &self.local,
                routes: &self.local,
                github: &self.issues,
                escalations,
            }
        }

        /// A local record, written straight to the local store: in any
        /// area, or none.
        fn record(&self, area: Option<&str>, title: &str) -> RecordId {
            self.local
                .add_record_with_area(&self.p, title, area)
                .unwrap()
        }

        /// A local finding about `record`, written straight to the local
        /// store.
        fn finding(&self, record: &RecordId, area: &str, security: bool) -> FindingId {
            let mut f = Finding::raise(self.p.clone(), record.clone(), "bob", "the claim");
            f.area = Some(area.into());
            f.security = security;
            self.local.add_finding(f).unwrap()
        }

        fn set_finding_state(&self, id: &FindingId, state: FindingState) {
            let mut f = self.local.get_finding(id).unwrap().unwrap();
            f.state = state;
            self.local.update_finding(&f).unwrap();
        }

        /// The whole command, as the CLI runs it.
        fn run(&self, id: &Iri, kind: Kind) -> Result<Iri, StoreError> {
            self.run_as(id, kind, BY, WHY, NOW)
        }

        fn run_as(
            &self,
            id: &Iri,
            kind: Kind,
            by: &str,
            why: &str,
            now: u64,
        ) -> Result<Iri, StoreError> {
            let t = self.router();
            let at = t.prepare_escalation(id, kind)?;
            t.escalate(&at, by, why, now)
        }

        /// The command refused, and nothing written: no mark, no issue.
        fn refused(&self, id: &Iri, kind: Kind) -> StoreError {
            let err = self.run(id, kind).unwrap_err();
            assert_eq!(self.local.mark_of(id).unwrap(), None, "marked: {err}");
            assert_eq!(self.issues.creates(), 0, "an issue made: {err}");
            err
        }
    }

    fn escalation(e: &StoreError) -> Option<&EscalationFault> {
        match e {
            StoreError::Escalation(f) => Some(f),
            _ => None,
        }
    }

    fn fault(e: &StoreError) -> Option<&RoutingFault> {
        match e {
            StoreError::Routing(f) => Some(f),
            _ => None,
        }
    }

    // Routing spec §3.1: an item that is not local, or was escalated
    // already, is refused; so is an id given as the wrong kind.
    #[test]
    fn an_item_that_is_not_local_or_was_escalated_already_is_refused_before_the_mark() {
        let w = world();
        let gh = w
            .issues
            .add_record_with_area(&w.p, "on github", Some("design"))
            .unwrap();
        let err = w.refused(gh.iri(), Kind::Record);
        assert!(
            matches!(escalation(&err), Some(EscalationFault::NotLocal { id }) if id == gh.iri()),
            "{err:?}"
        );
        // An issue URL is GitHub's even when a local item holds it as an
        // alias (the one-namespace rule): it never escalates that item.
        let r = w.record(Some("code"), "t");
        let url = MemIssues::issue(9);
        w.local.add_alias(r.iri(), url.clone()).unwrap();
        let err = w.refused(&url, Kind::Record);
        assert!(
            matches!(escalation(&err), Some(EscalationFault::NotLocal { id }) if *id == url),
            "{err:?}"
        );
        let err = w.refused(&seq_iri(999), Kind::Record);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::NotLocal { id }) if *id == seq_iri(999)
            ),
            "{err:?}"
        );
        // Escalated already: the refusal names where it went.
        let old = w.record(Some("code"), "gone");
        let to = w
            .issues
            .add_record_with_area(&w.p, "gone", Some("code"))
            .unwrap();
        w.local
            .mark(
                old.iri(),
                &Mark {
                    by: BY.into(),
                    reason: WHY.into(),
                    at_ms: 1,
                },
            )
            .unwrap();
        w.local.tombstone(old.iri(), to.iri()).unwrap();
        let err = w.refused(old.iri(), Kind::Record);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::AlreadyEscalated { id, to: t })
                    if id == old.iri() && t == to.iri()
            ),
            "{err:?}"
        );
        // The wrong kind: a finding's id to the record form, a record's to
        // the finding form, a project's to either.
        let f = w.finding(&r, "code", false);
        for (id, kind, found) in [
            (f.iri(), Kind::Record, Kind::Finding),
            (r.iri(), Kind::Finding, Kind::Record),
            (w.p.iri(), Kind::Record, Kind::Project),
        ] {
            let err = w.refused(id, kind);
            assert!(
                matches!(
                    err,
                    StoreError::WrongKind { expected, found: got, .. }
                        if expected == kind && got == found
                ),
                "{err:?}"
            );
        }
        // Only a record or a finding escalates.
        let err = w.refused(w.p.iri(), Kind::Project);
        assert!(
            matches!(
                err,
                StoreError::WrongKind {
                    expected: Kind::Record,
                    found: Kind::Project,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    // Routing spec §3.2: a closed state — a record `done`, a finding
    // `fixed` or `withdrawn` — is refused; GitHub takes open items only.
    #[test]
    fn a_closed_item_is_refused_before_the_mark() {
        let w = world();
        let r = w.record(Some("code"), "t");
        w.local.set_record_state(&r, State::Done).unwrap();
        let err = w.refused(r.iri(), Kind::Record);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::Closed { state, .. }) if state == "done"
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("is in a closed state"), "{err}");
        let open = w.record(Some("code"), "open");
        for (state, wire) in [
            (FindingState::Fixed, "fixed"),
            (FindingState::Withdrawn, "withdrawn"),
        ] {
            let f = w.finding(&open, "code", false);
            w.set_finding_state(&f, state);
            let err = w.refused(f.iri(), Kind::Finding);
            assert!(
                matches!(
                    escalation(&err),
                    Some(EscalationFault::Closed { id, state }) if id == f.iri() && state == wire
                ),
                "{err:?}"
            );
        }
    }

    // Routing spec §3.2: a record's title is its issue's title, so one the
    // GitHub tracker refuses — over 256 characters, or with whitespace at
    // either end — is refused. A finding's title is cut from its claim, so
    // a long claim is not.
    #[test]
    fn a_title_github_would_refuse_is_refused_before_the_mark() {
        let w = world();
        for title in [
            "é".repeat(257),
            " leading".to_string(),
            "trailing\n".to_string(),
            String::new(),
        ] {
            let r = w.record(Some("code"), &title);
            let err = w.refused(r.iri(), Kind::Record);
            assert!(
                matches!(
                    escalation(&err),
                    Some(EscalationFault::Title { id, .. }) if id == r.iri()
                ),
                "{title:?}: {err:?}"
            );
            assert!(
                err.to_string().contains("cannot be an issue's title"),
                "{err}"
            );
            assert_eq!(
                err.to_string().contains("an issue needs a title"),
                title.is_empty(),
                "{err}"
            );
        }
        // 256 characters, each two bytes: the limit counts characters.
        let r = w.record(Some("code"), &"é".repeat(256));
        w.router()
            .prepare_escalation(r.iri(), Kind::Record)
            .unwrap();
        let mut f = Finding::raise(w.p.clone(), r, "bob", &format!(" {}", "x".repeat(300)));
        f.area = Some("code".into());
        let f = w.local.add_finding(f).unwrap();
        w.router()
            .prepare_escalation(f.iri(), Kind::Finding)
            .unwrap();
    }

    // Routing spec §3.1: a project that is not routed, and a GitHub tier
    // this machine cannot open, are refused.
    #[test]
    fn an_unrouted_or_unbound_project_is_refused_before_the_mark() {
        let w = world();
        let q = w.local.add_project("/q").unwrap();
        let unrouted = w.local.add_record_with_area(&q, "t", None).unwrap();
        // The map is the first cause: GitHub is not the remedy.
        w.issues.set_unbound(true);
        let err = w.refused(unrouted.iri(), Kind::Record);
        assert!(
            matches!(fault(&err), Some(RoutingFault::Unrouted { project }) if *project == q),
            "{err:?}"
        );
        let r = w.record(Some("code"), "t");
        let err = w.refused(r.iri(), Kind::Record);
        assert!(
            matches!(
                fault(&err),
                Some(RoutingFault::TierUnavailable {
                    tier: Tier::Github,
                    ..
                })
            ),
            "{err:?}"
        );
        w.issues.set_unbound(false);
        w.issues.set_down(true);
        let err = w.refused(r.iri(), Kind::Record);
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    // Routing spec §3.2, the one-namespace rule: the item's IRI and each of
    // its aliases must name nothing on GitHub — an alias of another issue,
    // or an issue's own URL.
    #[test]
    fn an_alias_github_already_uses_is_refused_before_the_mark() {
        let w = world();
        let other = w
            .issues
            .add_record_with_area(&w.p, "other", Some("design"))
            .unwrap();
        // The item's own IRI, taken on GitHub.
        let r = w.record(Some("code"), "t");
        w.issues.add_alias(other.iri(), r.iri().clone()).unwrap();
        let err = w.refused(r.iri(), Kind::Record);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::AliasTaken { alias, issue })
                    if alias == r.iri() && issue == other.iri()
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("already names"), "{err}");
        // One of its aliases, taken by another issue's alias.
        let alias = Iri::parse("urn:x-acme:widget-7").unwrap();
        w.issues.add_alias(other.iri(), alias.clone()).unwrap();
        let s = w.record(Some("code"), "t");
        w.local
            .add_alias(s.iri(), Iri::parse("urn:x-acme:widget-6").unwrap())
            .unwrap();
        w.local.add_alias(s.iri(), alias.clone()).unwrap();
        let err = w.refused(s.iri(), Kind::Record);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::AliasTaken { alias: a, issue })
                    if *a == alias && issue == other.iri()
            ),
            "{err:?}"
        );
        // A local alias that is an issue URL of the bound repository, on a
        // finding.
        let f = w.finding(&w.record(Some("code"), "t"), "code", false);
        w.local.add_alias(f.iri(), other.iri().clone()).unwrap();
        let err = w.refused(f.iri(), Kind::Finding);
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::AliasTaken { alias, issue })
                    if alias == other.iri() && issue == other.iri()
            ),
            "{err:?}"
        );
    }

    // Routing spec decisions 21 and 22: nothing about an item in a
    // sensitive area, and no security finding, reaches a repository that is
    // not private — refused before the mark. An area the map no longer
    // declares counts as sensitive; a record with no area is not.
    #[test]
    fn nothing_sensitive_is_escalated_to_a_public_repository() {
        let w = world();
        let code = w.record(Some("code"), "fine");
        let secret = w.record(Some("security"), "the key leaks");
        let gone = w.record(Some("ops"), "undeclared");
        let refused = [
            (secret.iri().clone(), Kind::Record, "this record"),
            (gone.iri().clone(), Kind::Record, "this record"),
            (
                w.finding(&code, "code", true).0,
                Kind::Finding,
                "this finding",
            ),
            (
                w.finding(&code, "security", false).0,
                Kind::Finding,
                "this finding",
            ),
            (
                w.finding(&code, "ops", false).0,
                Kind::Finding,
                "this finding",
            ),
            (
                w.finding(&secret, "code", false).0,
                Kind::Finding,
                "this finding, about a record in a sensitive area,",
            ),
            (
                w.finding(&gone, "code", false).0,
                Kind::Finding,
                "this finding, about a record in a sensitive area,",
            ),
        ];
        // Private: every one of them may go.
        for (id, kind, _) in &refused {
            w.router().prepare_escalation(id, *kind).unwrap();
        }
        w.issues.set_public(true);
        for (id, kind, what) in &refused {
            let err = w.refused(id, *kind);
            assert!(
                matches!(
                    fault(&err),
                    Some(RoutingFault::SensitiveNamedPublic { what: w, .. }) if w == what
                ),
                "{id}: {err:?}"
            );
        }
        // Not sensitive: refused nowhere here (the CLI warns before it
        // publishes). A record with no area; a finding about a record that
        // GitHub holds already, whose title is published.
        let none = w.record(None, "no area");
        let on_github = w
            .issues
            .add_record_with_area(&w.p, "public", Some("security"))
            .unwrap();
        let mut about = Finding::raise(w.p.clone(), on_github.clone(), "bob", "c");
        about.area = Some("code".into());
        let about = w
            .local
            .add_finding_checked(
                about,
                ForeignRecord::for_tests(on_github, "public", Tier::Github),
            )
            .unwrap();
        for (id, kind) in [
            (code.iri(), Kind::Record),
            (none.iri(), Kind::Record),
            (w.finding(&code, "code", false).iri(), Kind::Finding),
            (w.finding(&none, "code", false).iri(), Kind::Finding),
            (about.iri(), Kind::Finding),
        ] {
            w.router().prepare_escalation(id, kind).unwrap();
        }
        // A visibility that cannot be read is that error, never a pass.
        w.issues.set_public(false);
        w.issues.set_visibility_unread(true);
        let err = w.refused(secret.iri(), Kind::Record);
        assert!(
            matches!(&err, StoreError::Backend(why) if why == VISIBILITY_UNREAD),
            "{err:?}"
        );
    }

    // Routing spec §3.3, decision 18: a record goes out with its own state,
    // area and aliases, and its open findings from both tiers — never a
    // security finding, nor one in a sensitive or undeclared area
    // (decisions 21, 22); then the tombstone replaces it, and the old id is
    // the issue.
    #[test]
    fn a_record_escalates_with_its_state_aliases_and_open_findings() {
        let w = world();
        let t = w.router();
        let r = w.record(Some("code"), "needs a person");
        let alias = Iri::parse("urn:x-acme:widget-7").unwrap();
        w.local.add_alias(r.iri(), alias.clone()).unwrap();
        w.local.set_record_state(&r, State::NeedsHuman).unwrap();
        let open = w.finding(&r, "code", false);
        let fixed = w.finding(&r, "code", false);
        w.set_finding_state(&fixed, FindingState::Fixed);
        let withdrawn = w.finding(&r, "code", false);
        w.set_finding_state(&withdrawn, FindingState::Withdrawn);
        let assigned = w.finding(&r, "tests", false);
        w.set_finding_state(&assigned, FindingState::Assigned);
        let security = w.finding(&r, "code", true);
        let sensitive = w.finding(&r, "security", false);
        let undeclared = w.finding(&r, "gone", false);
        let elsewhere = w.finding(&w.record(Some("code"), "other"), "code", false);
        // On GitHub: one raised through the router, one about the alias.
        let mut f = Finding::raise(w.p.clone(), r.clone(), "bob", "looks off");
        f.area = Some("design".into());
        let on_github = t.add_finding(f).unwrap();
        assert_eq!(t.tier_of(on_github.iri()), Tier::Github);
        let by_alias = w
            .issues
            .add_finding_checked(
                Finding::raise(w.p.clone(), RecordId(alias.clone()), "bob", "by alias"),
                ForeignRecord::for_tests(RecordId(alias.clone()), "needs a person", Tier::Local),
            )
            .unwrap();

        let at = t.prepare_escalation(r.iri(), Kind::Record).unwrap();
        assert_eq!(
            (at.id(), at.kind(), at.resumes(), at.found()),
            (r.iri(), Kind::Record, None, None)
        );
        let Outgoing::Record { record, findings } = at.outgoing() else {
            panic!("{:?}", at.outgoing());
        };
        assert_eq!(*record, w.local.get_record(&r).unwrap().unwrap());
        let ids: Vec<&FindingId> = findings.iter().map(|f| &f.id).collect();
        assert_eq!(ids, vec![&open, &assigned, &on_github, &by_alias]);
        assert!(!ids.contains(&&fixed) && !ids.contains(&&withdrawn));
        assert!(!ids.contains(&&security) && !ids.contains(&&elsewhere));
        assert!(!ids.contains(&&sensitive) && !ids.contains(&&undeclared));

        let issue = t.escalate(&at, BY, WHY, NOW).unwrap();
        assert_eq!(issue, MemIssues::issue(3));
        let made = w
            .issues
            .get_record(&RecordId(issue.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(
            (made.state, made.area.as_deref(), made.title.as_str()),
            (State::NeedsHuman, Some("code"), "needs a person")
        );
        assert_eq!(made.also_known_as, vec![r.iri().clone(), alias.clone()]);
        assert_eq!(
            w.issues.get_record(&r).unwrap().unwrap().id.iri(),
            &issue,
            "the old IRI names the issue"
        );
        assert_eq!(
            w.local.tombstone_of(r.iri()).unwrap(),
            Some(Tombstone {
                from: r.iri().clone(),
                to: issue.clone(),
                by: BY.into(),
                reason: WHY.into(),
                at_ms: NOW,
            })
        );
        assert_eq!(w.local.mark_of(r.iri()).unwrap(), None);
        assert_eq!(
            *w.issues.creates_asked.borrow(),
            vec![(
                Provenance {
                    from: r.iri().clone(),
                    by: BY.into(),
                    reason: WHY.into(),
                },
                NOW
            )]
        );
        // The router reads the old id, and its alias, as the issue.
        for old in [r.clone(), RecordId(alias)] {
            assert_eq!(t.get_record(&old).unwrap().unwrap().id.iri(), &issue);
        }
        // A finding raised about the old id afterwards names the issue.
        let mut later = Finding::raise(w.p.clone(), r.clone(), "bob", "later");
        later.area = Some("code".into());
        let later = t.add_finding(later).unwrap();
        assert_eq!(
            w.local.get_finding(&later).unwrap().unwrap().record.iri(),
            &issue
        );
    }

    // Routing spec §3.1, decision 5: a finding escalates while its record
    // stays local; about a record escalated already, its record is the
    // issue.
    #[test]
    fn a_finding_escalates_about_a_local_record_and_about_an_escalated_one() {
        let w = world();
        let t = w.router();
        let r = w.record(Some("code"), "the record");
        let f = w.finding(&r, "code", false);
        w.set_finding_state(&f, FindingState::Reproduced);
        let at = t.prepare_escalation(f.iri(), Kind::Finding).unwrap();
        let seen = RecordSeen {
            id: r.clone(),
            title: "the record".into(),
            tier: Tier::Local,
        };
        assert!(
            matches!(at.outgoing(), Outgoing::Finding { record, .. } if *record == seen),
            "{:?}",
            at.outgoing()
        );
        let issue = t.escalate(&at, BY, WHY, NOW).unwrap();
        let made = w
            .issues
            .get_finding(&FindingId(issue.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(
            (&made.record, made.state, &made.also_known_as),
            (&r, FindingState::Reproduced, &vec![f.iri().clone()])
        );
        assert_eq!(t.get_finding(&f).unwrap().unwrap().id.iri(), &issue);
        assert!(w.local.tombstone_of(f.iri()).unwrap().is_some());
        assert_eq!(
            w.local.get_record(&r).unwrap().unwrap().state,
            State::Todo,
            "the record stays local"
        );

        // A finding raised before its record was escalated.
        let g = w.finding(&r, "code", false);
        let to = w.run(r.iri(), Kind::Record).unwrap();
        let at = t.prepare_escalation(g.iri(), Kind::Finding).unwrap();
        assert!(
            matches!(
                at.outgoing(),
                Outgoing::Finding { record, .. }
                    if record.id.iri() == &to && record.tier == Tier::Github
            ),
            "{:?}",
            at.outgoing()
        );
        let issue = t.escalate(&at, BY, WHY, NOW).unwrap();
        let made = w.issues.get_finding(&FindingId(issue)).unwrap().unwrap();
        assert_eq!(made.record.iri(), &to);
    }

    /// The item is never live in both tiers (routing spec §2.2, §3.3):
    /// while it is marked the local store refuses a write; the router
    /// refuses one too until the issue exists, and from then on reaches the
    /// issue.
    fn assert_marked_and_unwritable(
        w: &W,
        t: &TieredTracker<'_>,
        r: &RecordId,
        issue: Option<&Iri>,
    ) {
        assert!(w.local.mark_of(r.iri()).unwrap().is_some());
        assert_eq!(w.local.tombstone_of(r.iri()).unwrap(), None);
        let err = w.local.set_record_state(r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Escalating { .. }), "{err:?}");
        match issue {
            None => {
                let err = t.set_record_state(r, State::Doing).unwrap_err();
                assert!(matches!(err, StoreError::Escalating { .. }), "{err:?}");
            }
            Some(issue) => {
                assert_eq!(t.get_record(r).unwrap().unwrap().id.iri(), issue);
            }
        }
    }

    /// The escalation finished: one issue, the tombstone, and every read and
    /// write of the old id reaching the issue, never the dead local row.
    fn assert_finished(w: &W, t: &TieredTracker<'_>, r: &RecordId, issue: &Iri) {
        assert_eq!(w.local.mark_of(r.iri()).unwrap(), None);
        assert_eq!(w.local.tombstone_of(r.iri()).unwrap().unwrap().to, *issue);
        t.set_record_state(r, State::Review).unwrap();
        let read = t.get_record(r).unwrap().unwrap();
        assert_eq!((read.id.iri(), read.state), (issue, State::Review));
        let local = w.local.get_record(r).unwrap_err();
        assert!(matches!(local, StoreError::Escalated { .. }), "{local:?}");
    }

    // Routing spec §3.3: "Each step can be run again, and running the
    // command again resumes from where it stopped." A stop before the
    // create, after the create landed with its answer lost, and after the
    // create before the tombstone: each rerun finishes with one issue.
    #[test]
    fn a_rerun_after_each_step_finishes_with_one_issue_and_one_live_copy() {
        let w = world();
        let t = w.router();

        // Stopped before the create: marked, no issue.
        let r1 = w.record(Some("code"), "one");
        w.issues.set_fail_next_create(true);
        let err = w.run(r1.iri(), Kind::Record).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert_eq!(w.issues.creates(), 0);
        assert_marked_and_unwritable(&w, &t, &r1, None);
        let at = t.prepare_escalation(r1.iri(), Kind::Record).unwrap();
        assert_eq!((at.resumes().is_some(), at.found()), (true, None));
        let issue = t.escalate(&at, BY, WHY, NOW).unwrap();
        assert_eq!(w.issues.creates(), 1);
        assert_finished(&w, &t, &r1, &issue);

        // Stopped after the create landed, its answer lost: the rerun finds
        // the issue — its own IRI is that issue's alias now, and the search
        // comes before that check — and finishes it through the create,
        // which finds it and makes none.
        let r2 = w.record(Some("code"), "two");
        w.issues.set_lose_next_create_answer(true);
        let err = w.run(r2.iri(), Kind::Record).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert_eq!(w.issues.creates(), 2);
        let landed = MemIssues::issue(2);
        assert_marked_and_unwritable(&w, &t, &r2, Some(&landed));
        let at = t.prepare_escalation(r2.iri(), Kind::Record).unwrap();
        assert_eq!(at.found(), Some(&landed));
        let asked = w.issues.creates_asked.borrow().len();
        assert_eq!(t.escalate(&at, BY, WHY, NOW).unwrap(), landed);
        assert_eq!(w.issues.creates(), 2);
        assert_eq!(
            w.issues.creates_asked.borrow().len(),
            asked + 1,
            "a found issue is finished through the create"
        );
        assert_finished(&w, &t, &r2, &landed);

        // Stopped after the create, before the tombstone.
        let r3 = w.record(Some("code"), "three");
        let failing = TombstoneFailsOnce {
            local: &w.local,
            fail: Cell::new(true),
        };
        let over = w.over(&failing);
        let at = over.prepare_escalation(r3.iri(), Kind::Record).unwrap();
        let err = over.escalate(&at, BY, WHY, NOW).unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err:?}");
        assert_eq!(w.issues.creates(), 3);
        let landed = MemIssues::issue(3);
        assert_marked_and_unwritable(&w, &t, &r3, Some(&landed));
        let at = over.prepare_escalation(r3.iri(), Kind::Record).unwrap();
        assert_eq!(at.found(), Some(&landed));
        assert_eq!(over.escalate(&at, BY, WHY, NOW).unwrap(), landed);
        assert_eq!(w.issues.creates(), 3);
        assert_finished(&w, &t, &r3, &landed);
    }

    // Routing spec §3.2, §3.3: a rerun that finds no issue makes every
    // check again — the repository may have turned public since the mark.
    #[test]
    fn a_rerun_that_finds_no_issue_makes_every_check_again() {
        let w = world();
        let r = w.record(Some("security"), "the key leaks");
        w.issues.set_fail_next_create(true);
        w.run(r.iri(), Kind::Record).unwrap_err();
        let marked = w.local.mark_of(r.iri()).unwrap();
        assert!(marked.is_some());
        w.issues.set_public(true);
        let err = w.run(r.iri(), Kind::Record).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveNamedPublic { .. })),
            "{err:?}"
        );
        assert_eq!(w.local.mark_of(r.iri()).unwrap(), marked, "the mark stays");
        assert_eq!(w.issues.creates(), 0);
    }

    // Routing spec §3.3: a rerun resumes the mark — its who, why and time —
    // whatever it is given.
    #[test]
    fn a_rerun_keeps_the_marks_who_and_why() {
        let w = world();
        let r = w.record(Some("code"), "t");
        w.issues.set_fail_next_create(true);
        w.run_as(r.iri(), Kind::Record, "alice", "first", 10)
            .unwrap_err();
        let at = w
            .router()
            .prepare_escalation(r.iri(), Kind::Record)
            .unwrap();
        let mark = Mark {
            by: "alice".into(),
            reason: "first".into(),
            at_ms: 10,
        };
        assert_eq!(at.resumes(), Some(&mark));
        w.run_as(r.iri(), Kind::Record, "bob", "second", 99)
            .unwrap();
        let from = Provenance {
            from: r.iri().clone(),
            by: "alice".into(),
            reason: "first".into(),
        };
        assert_eq!(
            *w.issues.creates_asked.borrow(),
            vec![(from.clone(), 10), (from, 10)],
            "the search reaches back to the mark's time"
        );
        let tomb = w.local.tombstone_of(r.iri()).unwrap().unwrap();
        assert_eq!(
            (tomb.by.as_str(), tomb.reason.as_str(), tomb.at_ms),
            ("alice", "first", 10)
        );
    }

    // Routing spec §3.2: GitHub refuses an issue body over 65,536
    // characters, so an escalation whose body could be longer — a long
    // claim, a long title of the local record a finding is about, a long
    // reason or name — is refused before the mark, naming the part.
    #[test]
    fn a_body_github_would_refuse_is_refused_before_the_mark() {
        let w = world();
        let r = w.record(Some("code"), "t");
        let finding = |claim: &str| {
            let mut f = Finding::raise(w.p.clone(), r.clone(), "bob", claim);
            f.area = Some("code".into());
            w.local.add_finding(f).unwrap()
        };
        let too_long = |err: &StoreError| match escalation(err) {
            Some(EscalationFault::TooLong { what, len, max, .. }) => (what.clone(), *len, *max),
            _ => panic!("{err:?}"),
        };
        // The claim is shown as written; the record's title, who and why
        // are shown escaped and kept in the block, at most fourteen bytes a
        // byte: 14 + 70 + 224 bytes beside the claim.
        let fits = ESCALATION_BODY_BUDGET - 14 - 5 * 14 - 16 * 14;
        let long = finding(&"a".repeat(fits + 1));
        let err = w.refused(long.iri(), Kind::Finding);
        assert_eq!(
            too_long(&err),
            ("the finding's claim".to_string(), fits + 1, fits)
        );
        assert!(err.to_string().contains("is too long to escalate"), "{err}");
        let just = finding(&"a".repeat(fits));
        w.run(just.iri(), Kind::Finding).unwrap();
        assert_eq!(w.issues.creates(), 1);

        // The title of the local record a finding is about.
        let titled = w.record(Some("code"), &"t".repeat(4_000));
        let mut f = Finding::raise(w.p.clone(), titled, "bob", "c");
        f.area = Some("code".into());
        let f = w.local.add_finding(f).unwrap();
        let err = w.run(f.iri(), Kind::Finding).unwrap_err();
        assert_eq!(too_long(&err).0, "the title of the record it is about");
        assert_eq!(w.local.mark_of(f.iri()).unwrap(), None);
        // A record on GitHub is named by its issue, and its title is not
        // written: beside it, a claim may take what a local record's title
        // would.
        let escalated = w.record(Some("code"), &"t".repeat(ISSUE_TITLE_MAX));
        let claim = "a".repeat(ESCALATION_BODY_BUDGET - 5 * 14 - 16 * 14);
        let mut f = Finding::raise(w.p.clone(), escalated.clone(), "bob", &claim);
        f.area = Some("code".into());
        let f = w.local.add_finding(f).unwrap();
        w.run(escalated.iri(), Kind::Record).unwrap();
        w.run(f.iri(), Kind::Finding).unwrap();
        assert_eq!(w.issues.creates(), 3);

        // The reason, and who escalates it.
        let s = w.record(Some("code"), "s");
        for (by, why, what) in [
            (BY.to_string(), "r".repeat(4_000), "the reason"),
            (
                "b".repeat(4_000),
                WHY.to_string(),
                "the name of who escalates it",
            ),
        ] {
            let err = w.run_as(s.iri(), Kind::Record, &by, &why, NOW).unwrap_err();
            assert_eq!(too_long(&err).0, what, "{err}");
            assert_eq!(w.local.mark_of(s.iri()).unwrap(), None);
        }

        // Its aliases, kept in the block.
        for n in 0..100 {
            let alias = format!("urn:x-acme:{n:03}-{}", "a".repeat(90));
            w.local
                .add_alias(s.iri(), Iri::parse(&alias).unwrap())
                .unwrap();
        }
        let err = w.run(s.iri(), Kind::Record).unwrap_err();
        assert_eq!(too_long(&err).0, "the list of its aliases", "{err}");
        assert_eq!(w.local.mark_of(s.iri()).unwrap(), None);
        assert_eq!(w.issues.creates(), 3, "no issue made");
    }

    // Routing spec §3.3: an `--abandon` whose search could not see the
    // issue yet removes the mark while the issue exists. The rerun finds
    // the issue — it holds the item's IRI as an alias, made from the item's
    // create key — and finishes it, never refusing the alias as taken.
    #[test]
    fn a_rerun_after_an_abandon_that_missed_the_issue_finishes_it() {
        let w = world();
        let t = w.router();
        let r = w.record(Some("code"), "t");
        w.issues.set_lose_next_create_answer(true);
        w.run(r.iri(), Kind::Record).unwrap_err();
        let landed = MemIssues::issue(1);
        w.local.unmark(r.iri()).unwrap();

        let at = t.prepare_escalation(r.iri(), Kind::Record).unwrap();
        assert_eq!((at.resumes(), at.found()), (None, Some(&landed)));
        assert_eq!(t.escalate(&at, BY, WHY, NOW + 5).unwrap(), landed);
        assert_eq!(w.issues.creates(), 1);
        assert_eq!(
            w.issues
                .creates_asked
                .borrow()
                .last()
                .map(|(_, since)| *since),
            Some(0),
            "the create's search reaches back past this run's mark"
        );
        assert_finished(&w, &t, &r, &landed);

        // So for a finding.
        let f = w.finding(&w.record(Some("code"), "f"), "code", false);
        w.issues.set_lose_next_create_answer(true);
        w.run(f.iri(), Kind::Finding).unwrap_err();
        let landed = MemIssues::issue(2);
        w.local.unmark(f.iri()).unwrap();
        let at = t.prepare_escalation(f.iri(), Kind::Finding).unwrap();
        assert_eq!(at.found(), Some(&landed));
        assert_eq!(t.escalate(&at, BY, WHY, NOW).unwrap(), landed);
        assert_eq!(w.issues.creates(), 2);
        assert_eq!(w.local.tombstone_of(f.iri()).unwrap().unwrap().to, landed);

        // The item's IRI held by another issue, though its own exists: that
        // is still taken.
        let s = w.record(Some("code"), "s");
        w.issues.set_lose_next_create_answer(true);
        w.run(s.iri(), Kind::Record).unwrap_err();
        w.local.unmark(s.iri()).unwrap();
        let other = MemIssues::issue(9);
        *w.issues.alias_held_by.borrow_mut() = Some(other.clone());
        let err = w.run(s.iri(), Kind::Record).unwrap_err();
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::AliasTaken { alias, issue })
                    if alias == s.iri() && *issue == other
            ),
            "{err:?}"
        );
        assert_eq!(w.local.mark_of(s.iri()).unwrap(), None);

        // Another of its names taken: refused, and the search by its create
        // key — which only the item's own IRI can need — is not asked.
        *w.issues.alias_held_by.borrow_mut() = None;
        let u = w.record(Some("code"), "u");
        let alias = Iri::parse("urn:x-acme:widget-7").unwrap();
        w.local.add_alias(u.iri(), alias.clone()).unwrap();
        w.issues
            .add_alias(&MemIssues::issue(1), alias.clone())
            .unwrap();
        let searches = w.issues.searches.get();
        let err = w.run(u.iri(), Kind::Record).unwrap_err();
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::AliasTaken { alias: a, .. }) if *a == alias
            ),
            "{err:?}"
        );
        assert_eq!(w.issues.searches.get(), searches, "a search was asked");
    }

    // Routing spec §3.3: `--abandon` removes the mark only after the search
    // proves no issue exists; once it exists, the only way on is step 3.
    #[test]
    fn an_escalation_is_abandoned_only_before_its_issue_exists() {
        let w = world();
        let t = w.router();
        let before = w.record(Some("code"), "before");
        w.issues.set_fail_next_create(true);
        w.run(before.iri(), Kind::Record).unwrap_err();
        // GitHub must answer the search: unbound, the mark stays.
        w.issues.set_unbound(true);
        let err = t
            .abandon_escalation(before.iri(), Kind::Record)
            .unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "{err:?}"
        );
        assert!(w.local.mark_of(before.iri()).unwrap().is_some());
        w.issues.set_unbound(false);
        t.abandon_escalation(before.iri(), Kind::Record).unwrap();
        assert_eq!(w.local.mark_of(before.iri()).unwrap(), None);
        t.set_record_state(&before, State::Doing).unwrap();

        let after = w.record(Some("code"), "after");
        w.issues.set_lose_next_create_answer(true);
        w.run(after.iri(), Kind::Record).unwrap_err();
        let err = t.abandon_escalation(after.iri(), Kind::Record).unwrap_err();
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::IssueExists { id, kind: Kind::Record, issue })
                    if id == after.iri() && *issue == MemIssues::issue(1)
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("its issue exists"), "{err}");
        assert!(w.local.mark_of(after.iri()).unwrap().is_some());

        // No mark: refused before GitHub is asked.
        let never = w.record(Some("code"), "never");
        w.issues.set_unbound(true);
        let err = t.abandon_escalation(never.iri(), Kind::Record).unwrap_err();
        assert!(
            matches!(
                escalation(&err),
                Some(EscalationFault::NotMarked { id }) if id == never.iri()
            ),
            "{err:?}"
        );
    }

    // Routing spec §3.3 step 1: "A finding raised against a marked record is
    // not a write to it and is allowed."
    #[test]
    fn a_finding_raised_about_a_marked_record_is_allowed() {
        let w = world();
        let t = w.router();
        let r = w.record(Some("code"), "t");
        w.issues.set_fail_next_create(true);
        w.run(r.iri(), Kind::Record).unwrap_err();
        let mut f = Finding::raise(w.p.clone(), r.clone(), "bob", "c");
        f.area = Some("code".into());
        let f = t.add_finding(f).unwrap();
        assert_eq!(w.local.get_finding(&f).unwrap().unwrap().record, r);
        // And it goes out with the record when the rerun finishes.
        let at = t.prepare_escalation(r.iri(), Kind::Record).unwrap();
        assert!(
            matches!(at.outgoing(), Outgoing::Record { findings, .. } if findings[0].id == f),
            "{:?}",
            at.outgoing()
        );
    }
}
