//! An in-memory GitHub tier, for the tests of the router and of what drives
//! it (routing spec §5). Ids are issue URLs of `acme/widgets`; records and
//! findings share one numbering, as issues do; no project is checked — the
//! router checks it, as it does for the real tracker. Never in the binary.

use crate::finding::{Finding, FindingState};
use crate::ids::{FindingId, Kind, ProjectId, RecordId};
use crate::iri::Iri;
use crate::model::{Record, State};
use crate::routing::{ForeignRecord, GithubTier, RoutingFault, Tier};
use crate::store::{StoreError, Tracker};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

/// Every issue URL this tier mints starts with this.
pub const ISSUES: &str = "https://github.com/acme/widgets/issues/";
const LABEL: &str = "github:acme/widgets";

#[derive(Default)]
pub struct MemIssues {
    inner: RefCell<Issues>,
    down: Cell<bool>,
    unbound: Cell<bool>,
    public: Cell<bool>,
    scan_fails: Cell<bool>,
    visibility_unread: Cell<bool>,
    asked: Cell<u32>,
}

/// What `items_in_area` answers once `set_scan_fails` is on.
pub const SCAN_FAILED: &str = "the area scan failed";
/// What `require_private` answers once `set_visibility_unread` is on.
pub const VISIBILITY_UNREAD: &str = "the visibility could not be read";

#[derive(Default)]
struct Issues {
    last: u64,
    records: BTreeMap<u64, Record>,
    findings: BTreeMap<u64, Finding>,
    aliases: BTreeMap<Iri, u64>,
}

impl MemIssues {
    /// GitHub cannot be reached: every call fails as unreachable.
    pub fn set_down(&self, down: bool) {
        self.down.set(down);
    }

    /// This machine binds no repository: the tier is not available.
    pub fn set_unbound(&self, unbound: bool) {
        self.unbound.set(unbound);
    }

    /// The repository is public.
    pub fn set_public(&self, public: bool) {
        self.public.set(public);
    }

    /// The scan for an area's items fails; every other call answers.
    pub fn set_scan_fails(&self, fails: bool) {
        self.scan_fails.set(fails);
    }

    /// The repository's visibility cannot be read; the tracker still opens
    /// and every other call answers.
    pub fn set_visibility_unread(&self, unread: bool) {
        self.visibility_unread.set(unread);
    }

    /// How many times the router asked for this tier's tracker.
    pub fn asked(&self) -> u32 {
        self.asked.get()
    }

    pub fn issue(n: u64) -> Iri {
        Iri::parse(&format!("{ISSUES}{n}")).expect("an issue URL is an IRI")
    }

    fn up(&self) -> Result<(), StoreError> {
        if self.down.get() {
            return Err(StoreError::Unreachable {
                store: LABEL.into(),
                cause: "connection refused".into(),
            });
        }
        Ok(())
    }

    /// The issue `id` names, by its own URL or an alias. ⚠ Any other id is
    /// `NotOwned`, never an answer of `None`.
    fn number(&self, id: &Iri) -> Result<u64, StoreError> {
        if let Some(n) = id
            .as_str()
            .strip_prefix(ISSUES)
            .and_then(|n| n.parse().ok())
        {
            return Ok(n);
        }
        self.inner
            .borrow()
            .aliases
            .get(id)
            .copied()
            .ok_or_else(|| StoreError::NotOwned {
                id: id.clone(),
                searched: vec![LABEL.into()],
            })
    }

    fn mint(&self) -> u64 {
        let mut s = self.inner.borrow_mut();
        s.last += 1;
        s.last
    }

    fn insert_finding(
        &self,
        mut finding: Finding,
        record: RecordId,
    ) -> Result<FindingId, StoreError> {
        if finding.security && self.public.get() {
            return Err(StoreError::SecurityNotPrivate {
                repo: "acme/widgets".into(),
                visibility: "public".into(),
            });
        }
        let n = self.mint();
        let id = FindingId(Self::issue(n));
        finding.id = id.clone();
        finding.record = record;
        self.inner.borrow_mut().findings.insert(n, finding);
        Ok(id)
    }
}

impl Tracker for MemIssues {
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        self.up()?;
        let n = self.mint();
        let id = RecordId(Self::issue(n));
        self.inner.borrow_mut().records.insert(
            n,
            Record {
                id: id.clone(),
                project: project.clone(),
                title: title.to_string(),
                state: State::Todo,
                also_known_as: vec![],
                area: area.map(str::to_string),
            },
        );
        Ok(id)
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.up()?;
        let n = self.number(id.iri())?;
        Ok(self.inner.borrow().records.get(&n).cloned())
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        self.up()?;
        let s = self.inner.borrow();
        Ok(s.records
            .values()
            .filter(|r| r.project == *project)
            .cloned()
            .collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.up()?;
        let n = self.number(id.iri())?;
        let mut s = self.inner.borrow_mut();
        let r = s
            .records
            .get_mut(&n)
            .ok_or_else(|| StoreError::NoSuchRecord(id.clone()))?;
        r.state = state;
        Ok(())
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        self.up()?;
        let n = self.number(finding.record.iri())?;
        let primary = {
            let s = self.inner.borrow();
            if s.findings.contains_key(&n) {
                return Err(StoreError::WrongKind {
                    id: finding.record.iri().clone(),
                    expected: Kind::Record,
                    found: Kind::Finding,
                });
            }
            s.records
                .get(&n)
                .map(|r| r.id.clone())
                .ok_or_else(|| StoreError::NoSuchRecord(finding.record.clone()))?
        };
        self.insert_finding(finding, primary)
    }

    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        self.up()?;
        if record.tier() != Tier::Local {
            return Err(StoreError::Backend(format!(
                "{} is a record on GitHub, so a finding about it is raised with `add_finding`, \
                 which checks it here",
                record.id()
            )));
        }
        self.insert_finding(finding, record.id().clone())
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        self.up()?;
        let n = self.number(id.iri())?;
        Ok(self.inner.borrow().findings.get(&n).cloned())
    }

    /// As the GitHub tracker: the stored id, aliases, record, raiser,
    /// security mark and area are kept; everything else is the caller's.
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.up()?;
        let n = self.number(finding.id.iri())?;
        let mut s = self.inner.borrow_mut();
        let stored = s
            .findings
            .get_mut(&n)
            .ok_or_else(|| StoreError::NoSuchFinding(finding.id.clone()))?;
        let kept = stored.clone();
        *stored = finding.clone();
        stored.id = kept.id;
        stored.also_known_as = kept.also_known_as;
        stored.record = kept.record;
        stored.raised_by = kept.raised_by;
        stored.security = kept.security;
        stored.area = kept.area;
        Ok(())
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        self.up()?;
        let s = self.inner.borrow();
        Ok(s.findings
            .values()
            .filter(|f| f.project == *project)
            .cloned()
            .collect())
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        self.up()?;
        let s = self.inner.borrow();
        Ok(s.findings
            .values()
            .filter(|f| f.raised_by == actor && f.state == FindingState::Withdrawn)
            .count() as u64)
    }

    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        self.up()?;
        // One namespace: an issue URL of this repository, or another
        // item's alias, is taken.
        let taken =
            alias.as_str().starts_with(ISSUES) || self.inner.borrow().aliases.contains_key(&alias);
        if taken {
            return Err(StoreError::AlreadyExists(alias));
        }
        let n = self.number(primary)?;
        let mut s = self.inner.borrow_mut();
        if let Some(r) = s.records.get_mut(&n) {
            r.also_known_as.push(alias.clone());
        } else if let Some(f) = s.findings.get_mut(&n) {
            f.also_known_as.push(alias.clone());
        } else {
            return Err(StoreError::NotOwned {
                id: primary.clone(),
                searched: vec![LABEL.into()],
            });
        }
        s.aliases.insert(alias, n);
        Ok(())
    }
}

impl GithubTier for MemIssues {
    fn available(&self) -> bool {
        !self.unbound.get()
    }

    /// Like the CLI's tier: with no binding it claims nothing.
    fn claims(&self, id: &Iri) -> bool {
        !self.unbound.get() && id.as_str().starts_with(ISSUES)
    }

    fn issue_form(&self, id: &Iri) -> bool {
        id.as_str().starts_with("https://github.com/") && id.as_str().contains("/issues/")
    }

    fn tracker(&self) -> Result<&dyn Tracker, StoreError> {
        self.asked.set(self.asked.get() + 1);
        if self.unbound.get() {
            return Err(RoutingFault::TierUnavailable {
                tier: Tier::Github,
                why: "this test binds no repository".into(),
            }
            .into());
        }
        self.up()?;
        Ok(self)
    }

    fn require_private(&self) -> Result<(), StoreError> {
        self.up()?;
        if self.visibility_unread.get() {
            return Err(StoreError::Backend(VISIBILITY_UNREAD.into()));
        }
        if self.public.get() {
            return Err(StoreError::SecurityNotPrivate {
                repo: "acme/widgets".into(),
                visibility: "public".into(),
            });
        }
        Ok(())
    }

    fn items_in_area(
        &self,
        project: &ProjectId,
        area: &str,
    ) -> Result<Vec<(Kind, Iri)>, StoreError> {
        self.up()?;
        if self.scan_fails.get() {
            return Err(StoreError::Backend(SCAN_FAILED.into()));
        }
        let s = self.inner.borrow();
        let records = s
            .records
            .values()
            .filter(|r| r.project == *project && r.area.as_deref() == Some(area))
            .map(|r| (Kind::Record, r.id.0.clone()));
        let findings = s
            .findings
            .values()
            .filter(|f| f.project == *project && f.area.as_deref() == Some(area))
            .map(|f| (Kind::Finding, f.id.0.clone()));
        Ok(records.chain(findings).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::conformance::{self, Bound, Fixture};
    use crate::ids::seq_iri;

    /// A `MemStore` catalog with this tier as the tracker, as the GitHub
    /// tracker is bound.
    struct Over {
        catalog: MemStore,
        issues: MemIssues,
    }

    impl Fixture for Over {
        fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
            f(&Bound {
                catalog: &self.catalog,
                tracker: &self.issues,
                ledger: &self.catalog,
                handles: &self.catalog,
            });
        }
    }

    #[test]
    fn the_in_memory_github_tier_meets_the_tracker_contract() {
        conformance::tracker(|| Over {
            catalog: MemStore::default(),
            issues: MemIssues::default(),
        });
    }

    #[test]
    fn a_tier_that_is_down_or_unbound_answers_with_an_error_never_an_answer() {
        let t = MemIssues::default();
        let p = ProjectId(seq_iri(1));
        let r = t.add_record_with_area(&p, "t", Some("code")).unwrap();
        assert_eq!(r.iri(), &MemIssues::issue(1));
        t.set_down(true);
        assert!(matches!(
            t.get_record(&r),
            Err(StoreError::Unreachable { .. })
        ));
        assert!(matches!(
            t.list_records(&p),
            Err(StoreError::Unreachable { .. })
        ));
        assert!(t.tracker().is_err());
        assert!(matches!(
            t.items_in_area(&p, "code"),
            Err(StoreError::Unreachable { .. })
        ));
        t.set_down(false);
        t.set_unbound(true);
        assert!(!t.available());
        assert!(!t.claims(&MemIssues::issue(1)) && t.issue_form(&MemIssues::issue(1)));
        assert!(matches!(
            t.tracker().err(),
            Some(StoreError::Routing(RoutingFault::TierUnavailable { .. }))
        ));
        assert_eq!(t.asked(), 2);
    }

    #[test]
    fn a_failing_scan_or_visibility_fails_only_its_own_call() {
        let t = MemIssues::default();
        let p = ProjectId(seq_iri(1));
        t.set_scan_fails(true);
        t.set_visibility_unread(true);
        assert!(t.tracker().is_ok());
        t.add_record_with_area(&p, "t", Some("code")).unwrap();
        assert!(matches!(
            t.items_in_area(&p, "code"),
            Err(StoreError::Backend(m)) if m == SCAN_FAILED
        ));
        assert!(matches!(
            t.require_private(),
            Err(StoreError::Backend(m)) if m == VISIBILITY_UNREAD
        ));
        t.set_scan_fails(false);
        t.set_visibility_unread(false);
        assert_eq!(t.items_in_area(&p, "code").unwrap().len(), 1);
        assert!(t.require_private().is_ok());
    }

    #[test]
    fn an_id_this_tier_never_held_is_not_owned_and_a_public_one_refuses_security() {
        let t = MemIssues::default();
        let err = t.get_record(&RecordId(seq_iri(9))).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        assert!(t.claims(&MemIssues::issue(5)) && !t.claims(&seq_iri(5)));
        t.set_public(true);
        assert!(matches!(
            t.require_private(),
            Err(StoreError::SecurityNotPrivate { .. })
        ));
        let p = ProjectId(seq_iri(1));
        let r = t.add_record(&p, "t").unwrap();
        let mut f = Finding::raise(p.clone(), r, "a", "c");
        f.security = true;
        assert!(matches!(
            t.add_finding(f),
            Err(StoreError::SecurityNotPrivate { .. })
        ));
        let area = t.add_record_with_area(&p, "u", Some("design")).unwrap();
        assert_eq!(
            t.items_in_area(&p, "design").unwrap(),
            vec![(Kind::Record, area.0.clone())]
        );
        assert!(
            t.items_in_area(&ProjectId(seq_iri(2)), "design")
                .unwrap()
                .is_empty()
        );
        // Findings are found by their own project and area, as records are.
        let mut mine = Finding::raise(p.clone(), area.clone(), "a", "c");
        mine.area = Some("design".into());
        let mine = t.add_finding(mine).unwrap();
        let mut elsewhere = Finding::raise(p.clone(), area.clone(), "a", "c");
        elsewhere.area = Some("code".into());
        t.add_finding(elsewhere).unwrap();
        let other = ProjectId(seq_iri(2));
        let o = t.add_record(&other, "o").unwrap();
        let mut theirs = Finding::raise(other, o, "a", "c");
        theirs.area = Some("design".into());
        t.add_finding(theirs).unwrap();
        assert_eq!(
            t.items_in_area(&p, "design").unwrap(),
            vec![(Kind::Record, area.0.clone()), (Kind::Finding, mine.0)]
        );
    }
}
