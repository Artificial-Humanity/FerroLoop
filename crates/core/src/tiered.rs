//! The routing tracker (routing spec §2): one `Tracker` over a project's
//! two tiers — the local store for developer-level items, a GitHub
//! repository for human-level ones — routing each new item by its area.

use crate::escalation::{Escalations, Mark};
use crate::finding::Finding;
use crate::ids::{FindingId, Kind, ProjectId, RecordId};
use crate::iri::Iri;
use crate::model::{Record, State};
use crate::routing::{ForeignRecord, GithubTier, Routes, RoutingFault, RoutingMap, Tier};
use crate::store::{Catalog, StoreError, Tracker, as_clause};

/// `Tracker` over a project's two tiers (routing spec §2). The CLI builds
/// it for a routed store; the gate engine sees one tracker, as before.
pub struct TieredTracker<'a> {
    /// The local store's catalog. A create on the GitHub tier checks its
    /// project here: the GitHub tracker cannot see the catalog (§2.5).
    pub catalog: &'a dyn Catalog,
    /// The local tier.
    pub local: &'a dyn Tracker,
    /// Each project's routing map: the local store, which holds the map it
    /// authored or imported.
    pub routes: &'a dyn Routes,
    /// The GitHub tier, opened on the first call that needs it (§2.6).
    pub github: &'a dyn GithubTier,
    /// The local tier's marks and tombstones (§3.6): the local store.
    pub escalations: &'a dyn Escalations,
}

/// Where a new record goes, decided before anything is written (§2.1).
/// Only the router builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    tier: Tier,
    area: String,
    sensitive: bool,
}

impl Placement {
    pub fn tier(&self) -> Tier {
        self.tier
    }

    pub fn area(&self) -> &str {
        &self.area
    }

    /// Whether the area is sensitive: a finding placed here is a security
    /// finding (decision 13).
    pub fn sensitive(&self) -> bool {
        self.sensitive
    }
}

/// A finding's record, as the tier that holds it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordSeen {
    pub id: RecordId,
    pub title: String,
    pub tier: Tier,
}

/// Where a new finding goes, and the record it was checked against (§2.1,
/// §2.5). Only the router builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingPlacement {
    at: Placement,
    inherited: bool,
    record: RecordSeen,
    /// The record's area is sensitive, or one the map no longer declares
    /// (decisions 21, 22): the finding is a security item, wherever it goes.
    about_sensitive: bool,
}

impl FindingPlacement {
    pub fn at(&self) -> &Placement {
        &self.at
    }

    /// Whether the area came from the record (§1.1).
    pub fn inherited(&self) -> bool {
        self.inherited
    }

    pub fn record(&self) -> &RecordSeen {
        &self.record
    }

    /// Whether the finding and its record are in different tiers (§2.5).
    pub fn crosses(&self) -> bool {
        self.record.tier != self.at.tier
    }
}

impl<'a> TieredTracker<'a> {
    /// The tier an id belongs to by its form alone: an issue of the bound
    /// repository is GitHub's; anything else is asked of the local tier
    /// first (§2.2).
    pub fn tier_of(&self, id: &Iri) -> Tier {
        if self.github.claims(id) {
            Tier::Github
        } else {
            Tier::Local
        }
    }

    /// The tracker of `tier`; GitHub's is opened now if it is not yet.
    pub(crate) fn tracker_in(&self, tier: Tier) -> Result<&'a dyn Tracker, StoreError> {
        match tier {
            Tier::Local => Ok(self.local),
            Tier::Github => self.github.tracker(),
        }
    }

    fn map_of(&self, project: &ProjectId) -> Result<RoutingMap, StoreError> {
        self.routes.routes(project)?.ok_or_else(|| {
            RoutingFault::Unrouted {
                project: project.clone(),
            }
            .into()
        })
    }

    /// `act` in the tier that owns `id` (§2.2), given the id to act on: an
    /// issue of the bound repository in GitHub; any other id in the local
    /// tier, then — if the local tier never held it — in GitHub, whose alias
    /// scan finds an item another machine moved there. With no binding, an
    /// issue URL the local tier does not hold is the missing tier's (§1.3).
    /// An id the local tier holds as a tombstone is GitHub's, asked once
    /// for the tombstone's target (§3.6): GitHub's answer for that issue is
    /// the answer. An id the local tier holds marked escalating is its
    /// issue once the issue exists and GitHub reads it (§2.2); otherwise,
    /// best effort, the local item, which refuses every write.
    /// ⚠ An id neither tier holds is `Elsewhere`, never `NotOwned`; outside
    /// that best effort, a tier that cannot be reached is its own error,
    /// never "not held".
    pub(crate) fn route<T>(
        &self,
        id: &Iri,
        act: impl Fn(&dyn Tracker, &Iri) -> Result<T, StoreError>,
    ) -> Result<(Tier, T), StoreError> {
        if self.github.claims(id) {
            return Ok((Tier::Github, act(self.github.tracker()?, id)?));
        }
        if let Some(issue) = self.issue_of_marked(id)? {
            return Ok((Tier::Github, act(self.github.tracker()?, &issue)?));
        }
        let mut searched = match act(self.local, id) {
            Err(StoreError::NotOwned { searched, .. }) => searched,
            Err(StoreError::Escalated { to, .. }) => {
                return Ok((Tier::Github, act(self.github.tracker()?, &to)?));
            }
            other => return Ok((Tier::Local, other?)),
        };
        if !self.github.available() {
            // An issue URL is GitHub's all the same: with no binding it is
            // refused as the missing tier, never as held elsewhere (§1.3).
            if self.github.issue_form(id) {
                self.github.tracker()?;
            }
            return Err(RoutingFault::Elsewhere {
                id: id.clone(),
                searched,
            }
            .into());
        }
        match act(self.github.tracker()?, id) {
            Err(StoreError::NotOwned {
                searched: theirs, ..
            }) => {
                searched.extend(theirs);
                Err(RoutingFault::Elsewhere {
                    id: id.clone(),
                    searched,
                }
                .into())
            }
            other => Ok((Tier::Github, other?)),
        }
    }

    /// The record `id` names, and the tier that holds it.
    pub(crate) fn record_of(&self, id: &RecordId) -> Result<(Tier, Record), StoreError> {
        match self.route(id.iri(), |t, id| t.get_record(&RecordId(id.clone())))? {
            (tier, Some(r)) => Ok((tier, r)),
            (_, None) => Err(StoreError::NoSuchRecord(id.clone())),
        }
    }

    /// The mark on `id` when the local tier holds it marked escalating
    /// (§2.4, §3.3 step 1). An id GitHub claims is GitHub's, so it has none,
    /// and GitHub is not asked.
    pub fn escalating(&self, id: &Iri) -> Result<Option<Mark>, StoreError> {
        if self.tier_of(id) == Tier::Github {
            return Ok(None);
        }
        self.escalations.mark_of(id)
    }

    /// The issue of an item the local tier holds marked escalating, when
    /// its escalation made one that GitHub reads as an fl item (§2.2: a
    /// marked item and the GitHub issue whose alias is its IRI are one
    /// item). The search is the escalation's own — by the item's primary
    /// IRI, back to the mark's time — and only a marked item pays for it.
    /// ⚠ Best effort: no issue found, an issue fl cannot read (a stop
    /// between the create and its labels leaves one), and a GitHub that
    /// cannot be opened or reached all answer `None`, so the local item
    /// answers — as it last was, and refusing every write (§3.3 step 1). No
    /// write lands in two places.
    fn issue_of_marked(&self, id: &Iri) -> Result<Option<Iri>, StoreError> {
        let Some(mark) = self.escalations.mark_of(id)? else {
            return Ok(None);
        };
        let Ok(github) = self.github.tracker() else {
            return Ok(None);
        };
        let kind = self.catalog.kind_of(id)?;
        let primary = match kind {
            Kind::Finding => self
                .local
                .get_finding(&FindingId(id.clone()))?
                .map(|f| f.id.0),
            _ => self
                .local
                .get_record(&RecordId(id.clone()))?
                .map(|r| r.id.0),
        };
        let Some(primary) = primary else {
            return Ok(None);
        };
        let Ok(Some(issue)) = self.github.find_escalated(&primary, mark.at_ms) else {
            return Ok(None);
        };
        let readable = match kind {
            Kind::Finding => github
                .get_finding(&FindingId(issue.clone()))
                .is_ok_and(|f| f.is_some()),
            _ => github
                .get_record(&RecordId(issue.clone()))
                .is_ok_and(|r| r.is_some()),
        };
        Ok(readable.then_some(issue))
    }

    /// `finding` with its record as the record now is (§2.5 "Evidence",
    /// §3.5): a local record escalated since the finding was raised is named
    /// by its issue. The stored reference is not rewritten — stores ignore
    /// the record on an update. An id GitHub claims is never looked up
    /// locally.
    fn as_now(&self, mut finding: Finding) -> Result<Finding, StoreError> {
        if self.tier_of(finding.record.iri()) == Tier::Local
            && let Some(t) = self.escalations.tombstone_of(finding.record.iri())?
        {
            finding.record = RecordId(t.to);
        }
        Ok(finding)
    }

    fn check_project(&self, project: &ProjectId) -> Result<(), StoreError> {
        match self.catalog.kind_of(project.iri())? {
            Kind::Project => Ok(()),
            found => Err(StoreError::WrongKind {
                id: project.iri().clone(),
                expected: Kind::Project,
                found,
            }),
        }
    }

    /// The private-repository rule (GitHub tracker spec §6). When the map
    /// chose the tier, the refusal names `--tier local` (§2.1); when the
    /// person named it, the refusal says what was refused and does not claim
    /// the map sent it.
    fn private_or_refuse(&self, by_map: bool, what: &str) -> Result<(), StoreError> {
        match self.github.require_private() {
            Err(StoreError::SecurityNotPrivate { repo, visibility }) => {
                let what = what.to_string();
                Err(if by_map {
                    RoutingFault::SensitiveToPublic {
                        what,
                        repo,
                        visibility,
                    }
                } else {
                    RoutingFault::SensitiveNamedPublic {
                        what,
                        repo,
                        visibility,
                    }
                }
                .into())
            }
            other => other,
        }
    }

    fn place(
        &self,
        map: &RoutingMap,
        area: &str,
        tier: Option<Tier>,
        security: bool,
        what: &str,
    ) -> Result<Placement, StoreError> {
        let route = map.route(area).ok_or_else(|| RoutingFault::Undeclared {
            area: area.to_string(),
            declared: map.declared(),
        })?;
        let (tier, by_map) = match tier {
            Some(t) => (t, false),
            None => (route.tier, true),
        };
        if tier == Tier::Github {
            // Before anything is written: a tier this machine cannot open
            // refuses the create here, never after a local write.
            self.github.tracker()?;
            if security || route.sensitive {
                self.private_or_refuse(by_map, what)?;
            }
        }
        Ok(Placement {
            tier,
            area: area.to_string(),
            sensitive: route.sensitive,
        })
    }

    /// Where a new record in `area` goes (§2.1): `tier` when given, else
    /// the area's tier through the project's map.
    pub fn place_record(
        &self,
        project: &ProjectId,
        area: Option<&str>,
        tier: Option<Tier>,
    ) -> Result<Placement, StoreError> {
        let map = self.map_of(project)?;
        let area = area.ok_or_else(|| RoutingFault::NoArea {
            declared: map.declared(),
        })?;
        self.place(&map, area, tier, false, "this record")
    }

    /// Where a new finding goes (§1.1, §2.1): its area is the one it names,
    /// or else its record's, read in the tier that holds the record.
    pub fn place_finding(
        &self,
        finding: &Finding,
        tier: Option<Tier>,
    ) -> Result<FindingPlacement, StoreError> {
        let map = self.map_of(&finding.project)?;
        let (record_tier, record) = self.record_of(&finding.record)?;
        let (area, inherited) = match (&finding.area, &record.area) {
            (Some(a), _) => (a.clone(), false),
            (None, Some(a)) => (a.clone(), true),
            (None, None) => {
                return Err(RoutingFault::NothingToInherit {
                    record: record.id.clone(),
                    declared: map.declared(),
                }
                .into());
            }
        };
        // Routing spec decision 21: a finding about a record in a sensitive
        // area publishes that record's title, so it is a security item too.
        // ⚠ Decision 22, failing closed: an area the map no longer declares
        // may have been sensitive, so it counts as sensitive.
        let record_sensitive = match record.area.as_deref() {
            Some(a) => map.route(a).is_none_or(|r| r.sensitive),
            None => false,
        };
        let what = if record_sensitive && !finding.security {
            "this finding, about a record in a sensitive area,"
        } else {
            "this finding"
        };
        let at = self.place(
            &map,
            &area,
            tier,
            finding.security || record_sensitive,
            what,
        )?;
        Ok(FindingPlacement {
            at,
            inherited,
            record: RecordSeen {
                id: record.id,
                title: record.title,
                tier: record_tier,
            },
            about_sensitive: record_sensitive,
        })
    }

    pub fn add_record_at(
        &self,
        project: &ProjectId,
        title: &str,
        at: &Placement,
    ) -> Result<RecordId, StoreError> {
        if at.tier == Tier::Github {
            self.check_project(project)?;
        }
        self.tracker_in(at.tier)?
            .add_record_with_area(project, title, Some(&at.area))
    }

    /// Write the finding where `at` says: with its area, as a security
    /// finding when its area or its record's is sensitive (decisions 13,
    /// 21, 22), naming its record's primary IRI —
    /// through a `ForeignRecord` when the record is in the other tier.
    pub fn add_finding_at(
        &self,
        mut finding: Finding,
        at: &FindingPlacement,
    ) -> Result<FindingId, StoreError> {
        finding.area = Some(at.at.area.clone());
        finding.security |= at.at.sensitive || at.about_sensitive;
        finding.record = at.record.id.clone();
        if at.at.tier == Tier::Github {
            self.check_project(&finding.project)?;
        }
        let tracker = self.tracker_in(at.at.tier)?;
        if at.crosses() {
            let proof = ForeignRecord::checked(
                at.record.id.clone(),
                at.record.title.clone(),
                at.record.tier,
            );
            tracker.add_finding_checked(finding, proof)
        } else {
            tracker.add_finding(finding)
        }
    }
}

/// Both tiers, local first, or the one asked for.
fn tiers(only: Option<Tier>) -> Vec<Tier> {
    match only {
        Some(t) => vec![t],
        None => Tier::ALL.to_vec(),
    }
}

impl TieredTracker<'_> {
    /// `act` over one tier's tracker. ⚠ In a merged read (`only` is `None`)
    /// a GitHub tier that cannot be read refuses the whole read, naming
    /// `--tier local` (§2.4): a list that cannot see its whole population
    /// fails.
    fn read<T>(
        &self,
        tier: Tier,
        only: Option<Tier>,
        act: impl Fn(&dyn Tracker) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        match self.tracker_in(tier).and_then(act) {
            Err(e) if only.is_none() && tier == Tier::Github => {
                // With no binding, the cause is the missing config entry
                // alone: the create's remedy — that fl never puts an item
                // in the other tier — says nothing to a list.
                let cause = match &e {
                    StoreError::Routing(RoutingFault::TierUnavailable { why, .. }) => {
                        as_clause(why)
                    }
                    other => as_clause(other),
                };
                Err(RoutingFault::TierUnreadable { tier, cause }.into())
            }
            other => other,
        }
    }

    /// The project's records in both tiers, or in `only`, each with its
    /// tier (§2.4).
    pub fn records(
        &self,
        project: &ProjectId,
        only: Option<Tier>,
    ) -> Result<Vec<(Tier, Record)>, StoreError> {
        let mut out = Vec::new();
        for tier in tiers(only) {
            let got = self.read(tier, only, |t| t.list_records(project))?;
            out.extend(got.into_iter().map(|r| (tier, r)));
        }
        self.one_copy(out, |r| (r.id.iri(), &r.also_known_as))
    }

    /// The project's findings in both tiers, or in `only`, each with its
    /// tier (§2.4) and its record as the record now is (§2.5).
    pub fn findings(
        &self,
        project: &ProjectId,
        only: Option<Tier>,
    ) -> Result<Vec<(Tier, Finding)>, StoreError> {
        let mut out = Vec::new();
        for tier in tiers(only) {
            for f in self.read(tier, only, |t| t.list_findings(project))? {
                out.push((tier, self.as_now(f)?));
            }
        }
        self.one_copy(out, |f| (f.id.iri(), &f.also_known_as))
    }

    /// `items` without the local copy of a marked item whose issue is in
    /// the same list (§2.2: they are one item, and the issue is the one
    /// shown). The issue names the item's IRI among its aliases, so nothing
    /// more is asked of GitHub. A list of the local tier alone keeps the
    /// copy, listed with its mark (§2.4).
    fn one_copy<T>(
        &self,
        items: Vec<(Tier, T)>,
        names: impl Fn(&T) -> (&Iri, &Vec<Iri>),
    ) -> Result<Vec<(Tier, T)>, StoreError> {
        let issued: std::collections::BTreeSet<Iri> = items
            .iter()
            .filter(|(tier, _)| *tier == Tier::Github)
            .flat_map(|(_, item)| names(item).1.iter().cloned())
            .collect();
        let mut out = Vec::with_capacity(items.len());
        for (tier, item) in items {
            let id = names(&item).0;
            let its_issue_listed = tier == Tier::Local && issued.contains(id);
            if its_issue_listed && self.escalations.mark_of(id)?.is_some() {
                continue;
            }
            out.push((tier, item));
        }
        Ok(out)
    }

    /// How many findings `actor` raised and withdrew, summed over both
    /// tiers, or in `only` (§2.4).
    pub fn withdrawals_in(&self, actor: &str, only: Option<Tier>) -> Result<u64, StoreError> {
        let mut n = 0;
        for tier in tiers(only) {
            n += self.read(tier, only, |t| t.withdrawals_by(actor))?;
        }
        Ok(n)
    }

    /// Every item of `project` that names `area` (§1.2): this machine's
    /// local tier, and GitHub by the items' blocks. ⚠ A tier that cannot be
    /// read is an error: the removal this serves must see every item.
    pub fn items_naming_area(
        &self,
        project: &ProjectId,
        area: &str,
    ) -> Result<Vec<(Tier, Kind, Iri)>, StoreError> {
        let mut out = Vec::new();
        for r in self.local.list_records(project)? {
            if r.area.as_deref() == Some(area) {
                out.push((Tier::Local, Kind::Record, r.id.0));
            }
        }
        for f in self.local.list_findings(project)? {
            if f.area.as_deref() == Some(area) {
                out.push((Tier::Local, Kind::Finding, f.id.0));
            }
        }
        // Open the tier first: with no binding it is the missing tier's
        // error, whatever the tier would answer for an area (§2.4).
        self.github.tracker()?;
        for (kind, id) in self.github.items_in_area(project, area)? {
            out.push((Tier::Github, kind, id));
        }
        Ok(out)
    }
}

impl Tracker for TieredTracker<'_> {
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        let at = self.place_record(project, area, None)?;
        self.add_record_at(project, title, &at)
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.route(id.iri(), |t, id| t.get_record(&RecordId(id.clone())))
            .map(|(_, r)| r)
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        Ok(self
            .records(project, None)?
            .into_iter()
            .map(|(_, r)| r)
            .collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.route(id.iri(), |t, id| {
            t.set_record_state(&RecordId(id.clone()), state)
        })
        .map(drop)
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        let at = self.place_finding(&finding, None)?;
        self.add_finding_at(finding, &at)
    }

    /// ⚠ The router checks every reference itself and never trusts a proof
    /// it did not build (§2.5): this places the finding as `add_finding`
    /// does, and drops `_record`.
    fn add_finding_checked(
        &self,
        finding: Finding,
        _record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        self.add_finding(finding)
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        match self.route(id.iri(), |t, id| t.get_finding(&FindingId(id.clone())))? {
            (_, Some(f)) => Ok(Some(self.as_now(f)?)),
            (_, None) => Ok(None),
        }
    }

    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.route(finding.id.iri(), |t, id| {
            let mut finding = finding.clone();
            finding.id = FindingId(id.clone());
            t.update_finding(&finding)
        })
        .map(drop)
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        Ok(self
            .findings(project, None)?
            .into_iter()
            .map(|(_, f)| f)
            .collect())
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        self.withdrawals_in(actor, None)
    }

    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        self.route(primary, |t, id| t.add_alias(id, alias.clone()))
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::escalation::{Escalations, Mark, Outgoing, Provenance};
    use crate::finding::FindingState;
    use crate::mem_issues::MemIssues;
    use crate::model::{CommandSpec, GateKind, PopulationDelivery, Selector, State};

    /// A project with the starting map in a local `MemStore`, and an
    /// in-memory GitHub tier.
    struct W {
        local: MemStore,
        issues: MemIssues,
        p: ProjectId,
    }

    fn world() -> W {
        let local = MemStore::default();
        let p = local.add_project("/p").unwrap();
        local.set_routes(&p, &RoutingMap::starting()).unwrap();
        W {
            local,
            issues: MemIssues::default(),
            p,
        }
    }

    impl W {
        fn router(&self) -> TieredTracker<'_> {
            TieredTracker {
                catalog: &self.local,
                local: &self.local,
                routes: &self.local,
                github: &self.issues,
                escalations: &self.local,
            }
        }

        /// A record in `area`, placed by the map.
        fn record(&self, area: &str, title: &str) -> RecordId {
            let t = self.router();
            let at = t.place_record(&self.p, Some(area), None).unwrap();
            t.add_record_at(&self.p, title, &at).unwrap()
        }

        /// `old`, a local item, marked and then replaced by a tombstone that
        /// points to `to` (routing spec §3.3 steps 1 and 3).
        fn tombstone(&self, old: &Iri, to: &Iri) {
            self.local.mark(old, &mark()).unwrap();
            self.local.tombstone(old, to).unwrap();
        }

        /// The local record `old` escalated by hand: its issue made in the
        /// GitHub tier, then the mark and the tombstone.
        fn escalated(&self, old: &RecordId, title: &str) -> RecordId {
            let issue = self
                .issues
                .add_record_with_area(&self.p, title, Some("code"))
                .unwrap();
            self.tombstone(old.iri(), issue.iri());
            issue
        }

        /// The issue an escalation of `item` makes (routing spec §3.3 step
        /// 2): keyed by the item's IRI, which is its first alias, and with no
        /// tombstone yet — a stop before step 3.
        fn issued(&self, item: Outgoing) -> Iri {
            let from = Provenance {
                from: item.id().clone(),
                by: mark().by,
                reason: mark().reason,
            };
            self.issues
                .create_escalated(&item, &from, mark().at_ms)
                .unwrap()
        }
    }

    fn mark() -> Mark {
        Mark {
            by: "alice".into(),
            reason: "a person decides".into(),
            at_ms: 1,
        }
    }

    /// Every project routed by one map, whatever its id: lets a test name
    /// an id the catalog holds as another kind.
    struct EveryProject(RoutingMap);

    impl Routes for EveryProject {
        fn routes(&self, _: &ProjectId) -> Result<Option<RoutingMap>, StoreError> {
            Ok(Some(self.0.clone()))
        }
    }

    fn fault(e: &StoreError) -> Option<&RoutingFault> {
        match e {
            StoreError::Routing(f) => Some(f),
            _ => None,
        }
    }

    // Routing spec §2.1: the tier comes from the item's area through the
    // map, and the area is recorded either way.
    #[test]
    fn a_record_goes_to_the_tier_its_area_routes_to_and_keeps_its_area() {
        let w = world();
        let t = w.router();
        let at = t.place_record(&w.p, Some("code"), None).unwrap();
        assert_eq!((at.tier(), at.area()), (Tier::Local, "code"));
        let local = t.add_record_at(&w.p, "fix", &at).unwrap();
        assert_eq!(
            w.local.get_record(&local).unwrap().unwrap().area.as_deref(),
            Some("code")
        );
        let gh = w.record("design", "look");
        assert_eq!(gh.iri(), &MemIssues::issue(1));
        assert_eq!(
            w.issues.get_record(&gh).unwrap().unwrap().area.as_deref(),
            Some("design")
        );
        assert_eq!(t.tier_of(gh.iri()), Tier::Github);
        assert_eq!(t.tier_of(local.iri()), Tier::Local);
    }

    #[test]
    fn a_tier_given_overrides_the_map_and_the_area_is_recorded_either_way() {
        let w = world();
        let t = w.router();
        let at = t
            .place_record(&w.p, Some("code"), Some(Tier::Github))
            .unwrap();
        let r = t.add_record_at(&w.p, "t", &at).unwrap();
        assert_eq!(
            w.issues.get_record(&r).unwrap().unwrap().area.as_deref(),
            Some("code")
        );
        let at = t
            .place_record(&w.p, Some("design"), Some(Tier::Local))
            .unwrap();
        assert_eq!(at.tier(), Tier::Local);
    }

    // Routing spec §2.1, §4: no area, or one the map does not declare, is
    // refused naming the declared areas; a project with no map, naming
    // `fl routing set`.
    #[test]
    fn a_create_with_no_area_an_undeclared_one_or_no_map_is_refused() {
        let w = world();
        let t = w.router();
        let err = t.place_record(&w.p, None, None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::NoArea { .. })),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains("code, design, product, security, tests"),
            "{err}"
        );
        let err = t
            .place_record(&w.p, Some("ops"), Some(Tier::Local))
            .unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::Undeclared { .. })),
            "{err:?}"
        );
        let q = w.local.add_project("/q").unwrap();
        let err = t.place_record(&q, Some("code"), None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::Unrouted { .. })),
            "{err:?}"
        );
        assert_eq!(w.issues.asked(), 0, "nothing asked of GitHub");
    }

    // Routing spec §1.3: "routing never changes tier silently".
    #[test]
    fn a_github_tier_create_with_no_binding_is_refused_and_lands_nowhere() {
        let w = world();
        w.issues.set_unbound(true);
        let t = w.router();
        let err = t.place_record(&w.p, Some("design"), None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "{err:?}"
        );
        assert!(w.local.list_records(&w.p).unwrap().is_empty());
        w.record("code", "t");
        assert_eq!(w.issues.asked(), 1, "a local create never asks GitHub");
    }

    // Routing spec §1.1: a finding's area is the one given, or else its
    // record's — which the placement says, for the note the CLI prints.
    #[test]
    fn a_finding_inherits_its_records_area_and_says_so() {
        let w = world();
        let t = w.router();
        let r = w.record("design", "t");
        let f = Finding::raise(w.p.clone(), r, "rev", "c");
        let at = t.place_finding(&f, None).unwrap();
        assert!(at.inherited());
        assert_eq!((at.at().tier(), at.at().area()), (Tier::Github, "design"));
        let id = t.add_finding_at(f.clone(), &at).unwrap();
        assert_eq!(
            w.issues.get_finding(&id).unwrap().unwrap().area.as_deref(),
            Some("design")
        );
        let mut given = f;
        given.area = Some("code".into());
        let at = t.place_finding(&given, None).unwrap();
        assert!(!at.inherited());
        assert_eq!(at.at().tier(), Tier::Local);
    }

    #[test]
    fn a_finding_whose_record_has_no_area_and_names_none_is_refused() {
        let w = world();
        let t = w.router();
        let old = w.local.add_record(&w.p, "made before areas").unwrap();
        let err = t
            .place_finding(&Finding::raise(w.p.clone(), old, "rev", "c"), None)
            .unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::NothingToInherit { .. })),
            "{err:?}"
        );
    }

    // Routing spec §2.5: a reference into the other tier is stored only
    // through a ForeignRecord the router built after reading the record —
    // and it names the record's primary IRI, even when the finding was
    // raised against an alias.
    #[test]
    fn a_finding_crosses_tiers_only_through_a_checked_foreign_record() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "local one");
        let alias = Iri::parse("https://github.com/o/r/issues/41").unwrap();
        w.local.add_alias(local.iri(), alias.clone()).unwrap();
        let mut f = Finding::raise(w.p.clone(), RecordId(alias), "rev", "c");
        f.area = Some("design".into());
        assert!(
            matches!(
                w.issues.add_finding(f.clone()),
                Err(StoreError::NotOwned { .. })
            ),
            "unchecked, the GitHub tier refuses a record it does not hold"
        );
        let at = t.place_finding(&f, None).unwrap();
        assert!(at.crosses());
        assert_eq!(at.record().title, "local one");
        let on_github = t.add_finding_at(f, &at).unwrap();
        assert_eq!(
            w.issues.get_finding(&on_github).unwrap().unwrap().record,
            local
        );

        let remote = w.record("product", "remote one");
        let mut f = Finding::raise(w.p.clone(), remote.clone(), "rev", "c");
        f.area = Some("tests".into());
        let at = t.place_finding(&f, None).unwrap();
        assert!(at.crosses());
        let on_local = t.add_finding_at(f, &at).unwrap();
        assert_eq!(
            w.local.get_finding(&on_local).unwrap().unwrap().record,
            remote
        );

        let mut same = Finding::raise(w.p.clone(), remote, "rev", "c");
        same.area = Some("design".into());
        let at = t.place_finding(&same, None).unwrap();
        assert!(!at.crosses());
        t.add_finding_at(same, &at).unwrap();
    }

    // Routing spec decision 13: a sensitive area makes a security finding.
    #[test]
    fn a_finding_in_a_sensitive_area_is_a_security_finding() {
        let w = world();
        let t = w.router();
        let r = w.record("code", "t");
        let mut f = Finding::raise(w.p.clone(), r, "rev", "c");
        f.area = Some("security".into());
        let at = t.place_finding(&f, None).unwrap();
        assert!(at.at().sensitive());
        let id = t.add_finding_at(f, &at).unwrap();
        assert!(w.issues.get_finding(&id).unwrap().unwrap().security);
    }

    // Routing spec §2.1: the map never sends a security item to a public
    // repository; fl refuses, naming `--tier local`, before it writes.
    #[test]
    fn a_sensitive_area_routed_to_a_public_repository_is_refused_before_anything_is_written() {
        let w = world();
        w.issues.set_public(true);
        let t = w.router();
        let err = t.place_record(&w.p, Some("security"), None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveToPublic { .. })),
            "{err:?}"
        );
        assert!(err.to_string().contains("--tier local"), "{err}");
        let r = w.record("code", "t");
        let mut f = Finding::raise(w.p.clone(), r, "rev", "c");
        f.area = Some("design".into());
        f.security = true;
        let err = t.place_finding(&f, None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveToPublic { .. })),
            "{err:?}"
        );
        // Named by the person, the tier is theirs: the tracker's own refusal.
        let err = t.place_finding(&f, Some(Tier::Github)).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveNamedPublic { .. })),
            "{err:?}"
        );
        let err = t
            .place_record(&w.p, Some("security"), Some(Tier::Github))
            .unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveNamedPublic { .. })),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains("this record is security-sensitive")
                && !err.to_string().contains("routing map"),
            "{err}"
        );
        assert!(w.issues.list_records(&w.p).unwrap().is_empty());
        assert!(w.issues.list_findings(&w.p).unwrap().is_empty());
        assert_eq!(
            t.place_finding(&f, Some(Tier::Local)).unwrap().at().tier(),
            Tier::Local,
            "the local tier takes it"
        );
    }

    // Routing spec decision 21: nothing about an item in a sensitive area
    // reaches a repository that is not private — not even a finding, in any
    // area, about a local record in a sensitive area.
    #[test]
    fn a_finding_about_a_record_in_a_sensitive_area_never_reaches_a_public_repository() {
        let w = world();
        w.issues.set_public(true);
        let t = w.router();
        let at = t
            .place_record(&w.p, Some("security"), Some(Tier::Local))
            .unwrap();
        let secret = t.add_record_at(&w.p, "the key leaks", &at).unwrap();
        let mut f = Finding::raise(w.p.clone(), secret, "rev", "c");
        f.area = Some("design".into());
        let err = t.place_finding(&f, None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveToPublic { .. })),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains("about a record in a sensitive area"),
            "{err}"
        );
        let err = t.place_finding(&f, Some(Tier::Github)).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveNamedPublic { .. })),
            "{err:?}"
        );
        assert!(w.issues.list_findings(&w.p).unwrap().is_empty());
        // A finding about an ordinary local record is placed; the CLI warns.
        let plain = w.record("code", "t");
        let mut g = Finding::raise(w.p.clone(), plain, "rev", "c");
        g.area = Some("design".into());
        assert_eq!(t.place_finding(&g, None).unwrap().at().tier(), Tier::Github);
        // Kept local, it is a security finding (decision 22).
        let at = t.place_finding(&f, Some(Tier::Local)).unwrap();
        let local = t.add_finding_at(f.clone(), &at).unwrap();
        assert!(w.local.get_finding(&local).unwrap().unwrap().security);
        w.issues.set_public(false);
        let at = t.place_finding(&f, None).unwrap();
        assert_eq!(at.at().tier(), Tier::Github);
        let on_github = t.add_finding_at(f, &at).unwrap();
        assert!(w.issues.get_finding(&on_github).unwrap().unwrap().security);
    }

    // Routing spec decision 22: an area the map no longer declares — removed
    // on the authoring machine while this machine's records still name it —
    // counts as sensitive: fail closed.
    #[test]
    fn a_record_whose_area_the_map_no_longer_declares_counts_as_sensitive() {
        let w = world();
        w.issues.set_public(true);
        let t = w.router();
        let old = w
            .local
            .add_record_with_area(&w.p, "made under an older map", Some("ops"))
            .unwrap();
        let mut f = Finding::raise(w.p.clone(), old, "rev", "c");
        f.area = Some("design".into());
        let err = t.place_finding(&f, None).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::SensitiveToPublic { .. })),
            "{err:?}"
        );
        assert!(err.to_string().contains("--tier local"), "{err}");
        let at = t.place_finding(&f, Some(Tier::Local)).unwrap();
        let id = t.add_finding_at(f, &at).unwrap();
        assert!(w.local.get_finding(&id).unwrap().unwrap().security);
    }

    // Routing spec §2.5: a record in a tier that cannot be reached is an
    // error, never "no such record".
    #[test]
    fn a_record_in_an_unreachable_tier_is_an_error_not_no_such_record() {
        let w = world();
        let t = w.router();
        let r = w.record("design", "t");
        w.issues.set_down(true);
        let mut f = Finding::raise(w.p.clone(), r, "rev", "c");
        f.area = Some("code".into());
        let err = t.place_finding(&f, None).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    // Routing spec §2.5: the GitHub tracker cannot see the catalog, so the
    // router checks a GitHub create's project there.
    #[test]
    fn a_github_create_checks_its_project_in_the_catalog() {
        let w = world();
        let every = EveryProject(RoutingMap::starting());
        let t = TieredTracker {
            catalog: &w.local,
            local: &w.local,
            routes: &every,
            github: &w.issues,
            escalations: &w.local,
        };
        let kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        });
        let sel = Selector::Glob {
            pattern: "**/*".into(),
        };
        let g = w.local.add_gate(&w.p, "g", kind, sel, 1, "c", "o").unwrap();
        let as_project = ProjectId(g.0);
        let at = t.place_record(&as_project, Some("design"), None).unwrap();
        let err = t.add_record_at(&as_project, "t", &at).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::WrongKind {
                    expected: Kind::Project,
                    found: Kind::Gate,
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(w.issues.list_records(&as_project).unwrap().is_empty());
    }

    // Routing spec §2.2: by IRI, the router asks the tier that owns it.
    #[test]
    fn a_lookup_asks_the_tier_whose_form_the_id_has() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "l");
        let gh = w.record("design", "g");
        let asked = w.issues.asked();
        assert_eq!(t.get_record(&local).unwrap().unwrap().title, "l");
        assert_eq!(w.issues.asked(), asked, "a local id never opens GitHub");
        assert_eq!(t.get_record(&gh).unwrap().unwrap().title, "g");
        // An issue of the bound repository is GitHub's, even when the local
        // tier holds the same IRI as an alias.
        let taken = MemIssues::issue(9);
        w.local.add_alias(local.iri(), taken.clone()).unwrap();
        assert_eq!(
            t.get_record(&RecordId(taken)).unwrap(),
            None,
            "GitHub's answer"
        );
        w.issues.set_unbound(true);
        let err = t.get_record(&gh).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "an issue URL is the missing tier's, never held elsewhere: {err:?}"
        );
    }

    // Routing spec §1.3, §2.2: with no binding, an alias that is another
    // repository's issue URL is still found in the local tier.
    #[test]
    fn with_no_binding_another_repositorys_issue_url_held_locally_is_found_there() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "l");
        let old = Iri::parse("https://github.com/o/r/issues/41").unwrap();
        w.local.add_alias(local.iri(), old.clone()).unwrap();
        w.issues.set_unbound(true);
        assert_eq!(t.get_record(&RecordId(old)).unwrap().unwrap().id, local);
        let err = t
            .get_record(&RecordId(
                Iri::parse("https://github.com/o/r/issues/42").unwrap(),
            ))
            .unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "{err:?}"
        );
    }

    // Routing spec §2.2: never `NotOwned`, as if the id were malformed.
    #[test]
    fn an_id_neither_tier_holds_is_held_elsewhere_never_not_owned() {
        let w = world();
        let t = w.router();
        let stranger = RecordId(crate::ids::seq_iri(99));
        let err = t.get_record(&stranger).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::Elsewhere { .. })),
            "{err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("another machine's local tier")
                && msg.contains("memory")
                && msg.contains("github:acme/widgets"),
            "{msg}"
        );
        w.issues.set_unbound(true);
        let asked = w.issues.asked();
        let err = t.set_record_state(&stranger, State::Doing).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::Elsewhere { .. })),
            "{err:?}"
        );
        assert_eq!(w.issues.asked(), asked, "an unbound machine asks no GitHub");
    }

    // Routing spec §2.2: a urn the local store does not hold may be an item
    // another machine moved to GitHub; GitHub's alias scan finds it.
    #[test]
    fn a_local_id_this_store_does_not_hold_is_found_by_githubs_alias_scan() {
        let w = world();
        let t = w.router();
        let gh = w.record("design", "moved here");
        let old = crate::ids::seq_iri(77);
        w.issues.add_alias(gh.iri(), old.clone()).unwrap();
        assert_eq!(t.get_record(&RecordId(old)).unwrap().unwrap().id, gh);
    }

    #[test]
    fn a_fallback_to_an_unreachable_github_is_an_error_not_not_found() {
        let w = world();
        let t = w.router();
        w.issues.set_down(true);
        let err = t
            .get_record(&RecordId(crate::ids::seq_iri(99)))
            .unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    #[test]
    fn writes_go_to_the_tier_that_holds_the_item() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "l");
        let gh = w.record("design", "g");
        t.set_record_state(&local, State::Doing).unwrap();
        t.set_record_state(&gh, State::Review).unwrap();
        assert_eq!(
            w.local.get_record(&local).unwrap().unwrap().state,
            State::Doing
        );
        assert_eq!(
            w.issues.get_record(&gh).unwrap().unwrap().state,
            State::Review
        );
        let f = t
            .add_finding(Finding::raise(w.p.clone(), gh, "rev", "c"))
            .unwrap();
        let mut back = t.get_finding(&f).unwrap().unwrap();
        back.withdraw("no").unwrap();
        t.update_finding(&back).unwrap();
        assert_eq!(
            w.issues.get_finding(&f).unwrap().unwrap().state,
            FindingState::Withdrawn
        );
    }

    // Routing spec §2.5: the router never trusts a proof it did not build.
    #[test]
    fn the_router_trusts_no_proof_it_did_not_build() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "l");
        let mut f = Finding::raise(w.p.clone(), local.clone(), "rev", "c");
        f.area = Some("design".into());
        let lie = ForeignRecord::for_tests(local, "l", Tier::Github);
        let id = t.add_finding_checked(f, lie).unwrap();
        assert_eq!(
            t.tier_of(id.iri()),
            Tier::Github,
            "placed by its area, as add_finding"
        );
    }

    // Routing spec §2.4: both tiers merged, each item with its tier.
    #[test]
    fn a_merged_list_holds_both_tiers_and_names_each_items_tier() {
        let w = world();
        let t = w.router();
        let local = w.record("code", "l");
        let gh = w.record("design", "g");
        let both: Vec<(Tier, RecordId)> = t
            .records(&w.p, None)
            .unwrap()
            .into_iter()
            .map(|(tier, r)| (tier, r.id))
            .collect();
        assert_eq!(
            both,
            vec![(Tier::Local, local.clone()), (Tier::Github, gh.clone())]
        );
        assert_eq!(t.list_records(&w.p).unwrap().len(), 2);
        assert_eq!(t.records(&w.p, Some(Tier::Local)).unwrap().len(), 1);
        assert_eq!(t.records(&w.p, Some(Tier::Github)).unwrap()[0].1.id, gh);
        t.add_finding(Finding::raise(w.p.clone(), local, "rev", "c"))
            .unwrap();
        assert_eq!(t.findings(&w.p, None).unwrap()[0].0, Tier::Local);
        assert_eq!(t.list_findings(&w.p).unwrap().len(), 1);
    }

    // Routing spec §2.4: "a list that cannot see its whole population fails".
    #[test]
    fn a_merged_list_refuses_when_the_github_tier_cannot_be_read() {
        let w = world();
        let t = w.router();
        w.record("code", "l");
        for (down, unbound) in [(true, false), (false, true)] {
            w.issues.set_down(down);
            w.issues.set_unbound(unbound);
            for err in [
                t.records(&w.p, None).unwrap_err(),
                t.findings(&w.p, None).unwrap_err(),
                t.withdrawals_in("rev", None).unwrap_err(),
                t.list_records(&w.p).unwrap_err(),
            ] {
                assert!(
                    matches!(fault(&err), Some(RoutingFault::TierUnreadable { .. })),
                    "{err:?}"
                );
                assert!(err.to_string().contains("--tier local"), "{err}");
                assert_eq!(
                    err.to_string().contains("this test binds no repository"),
                    unbound,
                    "{err}"
                );
                assert!(
                    !err.to_string().contains("never puts an item"),
                    "a create's remedy, not a list's: {err}"
                );
            }
            assert_eq!(
                t.records(&w.p, Some(Tier::Local)).unwrap().len(),
                1,
                "the local tier alone still reads"
            );
        }
        w.issues.set_unbound(false);
        w.issues.set_down(true);
        let err = t.records(&w.p, Some(Tier::Github)).unwrap_err();
        assert!(
            matches!(err, StoreError::Unreachable { .. }),
            "one tier asked for is its own error: {err:?}"
        );
    }

    #[test]
    fn withdrawals_sum_both_tiers_or_count_the_one_asked() {
        let w = world();
        let t = w.router();
        for area in ["code", "design"] {
            let r = w.record(area, "t");
            let f = t
                .add_finding(Finding::raise(w.p.clone(), r, "hasty", "c"))
                .unwrap();
            let mut back = t.get_finding(&f).unwrap().unwrap();
            back.withdraw("no").unwrap();
            t.update_finding(&back).unwrap();
        }
        assert_eq!(t.withdrawals_by("hasty").unwrap(), 2);
        assert_eq!(t.withdrawals_in("hasty", Some(Tier::Local)).unwrap(), 1);
        assert_eq!(t.withdrawals_in("hasty", Some(Tier::Github)).unwrap(), 1);
    }

    // Routing spec §1.2: removing an area needs every item that names it,
    // in both tiers; a tier that cannot be read refuses.
    #[test]
    fn items_naming_an_area_are_found_in_both_tiers_and_an_unreadable_tier_refuses() {
        let w = world();
        let t = w.router();
        let l = w.record("code", "l");
        let at = t
            .place_record(&w.p, Some("code"), Some(Tier::Github))
            .unwrap();
        let g = t.add_record_at(&w.p, "g", &at).unwrap();
        let mut f = Finding::raise(w.p.clone(), l.clone(), "rev", "c");
        f.area = Some("code".into());
        let lf = t.add_finding(f).unwrap();
        w.record("design", "other");
        let found = t.items_naming_area(&w.p, "code").unwrap();
        assert_eq!(
            found,
            vec![
                (Tier::Local, Kind::Record, l.0),
                (Tier::Local, Kind::Finding, lf.0),
                (Tier::Github, Kind::Record, g.0),
            ]
        );
        assert!(t.items_naming_area(&w.p, "ops").unwrap().is_empty());
        w.issues.set_down(true);
        assert!(t.items_naming_area(&w.p, "code").is_err());
        w.issues.set_down(false);
        w.issues.set_unbound(true);
        let err = t.items_naming_area(&w.p, "code").unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "an unbound machine cannot see GitHub's items: {err:?}"
        );
    }

    // Routing spec §1.2: the scan of GitHub's items is the removal's
    // evidence; its failure is the refusal, never "no item names it".
    #[test]
    fn a_failed_scan_of_githubs_items_is_an_error_never_none() {
        let w = world();
        let t = w.router();
        w.issues.set_scan_fails(true);
        let err = t.items_naming_area(&w.p, "ops").unwrap_err();
        assert!(
            matches!(&err, StoreError::Backend(m) if m == crate::mem_issues::SCAN_FAILED),
            "{err:?}"
        );
    }

    // GitHub tracker spec §6: an unknown visibility is not private. A record
    // in a sensitive area, or a security finding, is refused when the
    // visibility cannot be read — with that error, before anything is written.
    #[test]
    fn an_unreadable_visibility_refuses_a_sensitive_placement() {
        let w = world();
        let t = w.router();
        let r = w.record("code", "t");
        w.issues.set_visibility_unread(true);
        let unread = |e: &StoreError| matches!(e, StoreError::Backend(m) if m == crate::mem_issues::VISIBILITY_UNREAD);
        let err = t.place_record(&w.p, Some("security"), None).unwrap_err();
        assert!(unread(&err), "{err:?}");
        let mut f = Finding::raise(w.p.clone(), r, "rev", "c");
        f.area = Some("design".into());
        f.security = true;
        let err = t.place_finding(&f, None).unwrap_err();
        assert!(unread(&err), "{err:?}");
        f.security = false;
        f.area = Some("security".into());
        let err = t.place_finding(&f, Some(Tier::Github)).unwrap_err();
        assert!(unread(&err), "{err:?}");
        assert!(w.issues.list_records(&w.p).unwrap().is_empty());
        assert!(w.issues.list_findings(&w.p).unwrap().is_empty());
    }

    // Routing spec §2.5: a finding names the record its placement was
    // checked against, never one the finding was raised about afterwards.
    #[test]
    fn a_finding_is_written_about_the_record_its_placement_checked() {
        let w = world();
        let t = w.router();
        let r1 = w.record("code", "one");
        let r2 = w.record("code", "two");
        let mut about_r1 = Finding::raise(w.p.clone(), r1.clone(), "rev", "c");
        about_r1.area = Some("code".into());
        let at = t.place_finding(&about_r1, None).unwrap();
        assert!(!at.crosses() && at.at().tier() == Tier::Local);
        let about_r2 = Finding::raise(w.p.clone(), r2, "rev", "c");
        let id = t.add_finding_at(about_r2, &at).unwrap();
        assert_eq!(w.local.get_finding(&id).unwrap().unwrap().record, r1);
    }

    // Routing spec §2.2, §3.6: an escalated item's old IRI reaches its
    // issue — a read, a write, and a finding raised about it, which is
    // written about the issue and crosses tiers through the router's proof.
    #[test]
    fn a_tombstoned_id_is_followed_to_its_issue() {
        let w = world();
        let t = w.router();
        let old = w.record("code", "fix");
        let issue = w.escalated(&old, "fix");
        assert_eq!(t.get_record(&old).unwrap().unwrap().id, issue);
        t.set_record_state(&old, State::Doing).unwrap();
        assert_eq!(
            w.issues.get_record(&issue).unwrap().unwrap().state,
            State::Doing
        );
        let mut stays = Finding::raise(w.p.clone(), old.clone(), "rev", "stays");
        stays.area = Some("code".into());
        let at = t.place_finding(&stays, None).unwrap();
        assert_eq!(
            (at.record().id.clone(), at.record().tier, at.at().tier()),
            (issue.clone(), Tier::Github, Tier::Local)
        );
        let id = t.add_finding(stays).unwrap();
        assert_eq!(t.tier_of(id.iri()), Tier::Local);
        assert_eq!(w.local.get_finding(&id).unwrap().unwrap().record, issue);
        let mut moves = Finding::raise(w.p.clone(), old.clone(), "rev", "moves");
        moves.area = Some("design".into());
        let id = t.add_finding(moves).unwrap();
        assert_eq!(w.issues.get_finding(&id).unwrap().unwrap().record, issue);
        // A finding's old IRI: read, updated — from a copy read before the
        // escalation — and given an alias, on GitHub.
        let mut f = Finding::raise(w.p.clone(), old.clone(), "rev", "c");
        f.area = Some("code".into());
        let old_f = t.add_finding(f).unwrap();
        let mut before = t.get_finding(&old_f).unwrap().unwrap();
        let gh_f = w
            .issues
            .add_finding(Finding::raise(w.p.clone(), issue.clone(), "rev", "c"))
            .unwrap();
        w.tombstone(old_f.iri(), gh_f.iri());
        assert_eq!(t.get_finding(&old_f).unwrap().unwrap().id, gh_f);
        before.withdraw("no").unwrap();
        t.update_finding(&before).unwrap();
        assert_eq!(
            w.issues.get_finding(&gh_f).unwrap().unwrap().state,
            FindingState::Withdrawn
        );
        let alias = crate::ids::seq_iri(88);
        t.add_alias(old_f.iri(), alias.clone()).unwrap();
        assert_eq!(t.get_finding(&FindingId(alias)).unwrap().unwrap().id, gh_f);
    }

    // Routing spec §2.5: GitHub that cannot be reached while a tombstone is
    // followed is that tier's error, never "held elsewhere" or "no such
    // record".
    #[test]
    fn following_a_tombstone_to_an_unbound_or_unreachable_github_is_that_tiers_error() {
        let w = world();
        let t = w.router();
        let old = w.record("code", "fix");
        w.escalated(&old, "fix");
        w.issues.set_unbound(true);
        let err = t.get_record(&old).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "{err:?}"
        );
        let err = t.set_record_state(&old, State::Doing).unwrap_err();
        assert!(
            matches!(fault(&err), Some(RoutingFault::TierUnavailable { .. })),
            "{err:?}"
        );
        w.issues.set_unbound(false);
        w.issues.set_down(true);
        let err = t.get_record(&old).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    // A tombstone is followed once, to GitHub: its target is GitHub's answer
    // for that id, even when GitHub does not hold it — never "held
    // elsewhere", and never a second hop through the local tier.
    #[test]
    fn a_tombstone_is_followed_once_and_its_target_is_githubs_answer() {
        let w = world();
        let t = w.router();
        let old = w.record("code", "gone");
        let gone = MemIssues::issue(42);
        w.tombstone(old.iri(), &gone);
        assert_eq!(t.get_record(&old).unwrap(), None, "GitHub's answer");
        let err = t.set_record_state(&old, State::Doing).unwrap_err();
        assert!(
            matches!(&err, StoreError::NoSuchRecord(r) if r.iri() == &gone),
            "{err:?}"
        );
        let first = w.record("code", "first");
        let second = w.record("code", "second");
        w.escalated(&second, "second");
        w.tombstone(first.iri(), second.iri());
        let err = t.get_record(&first).unwrap_err();
        assert!(
            matches!(&err, StoreError::NotOwned { id, .. } if id == second.iri()),
            "GitHub's answer for {second}: {err:?}"
        );
    }

    // Routing spec §2.5 ("Evidence"), §3.5: a finding's record is shown as
    // the record now is — its issue, once escalated — in either tier, while
    // the stored reference stays as it was raised.
    #[test]
    fn a_findings_record_reads_as_its_issue_once_escalated() {
        let w = world();
        let t = w.router();
        let old = w.record("code", "fix");
        let mut f = Finding::raise(w.p.clone(), old.clone(), "rev", "here");
        f.area = Some("code".into());
        let local_f = t.add_finding(f).unwrap();
        let mut g = Finding::raise(w.p.clone(), old.clone(), "rev", "there");
        g.area = Some("design".into());
        let gh_f = t.add_finding(g).unwrap();
        let issue = w.escalated(&old, "fix");
        assert_eq!(t.get_finding(&local_f).unwrap().unwrap().record, issue);
        assert_eq!(t.get_finding(&gh_f).unwrap().unwrap().record, issue);
        let listed: Vec<RecordId> = t
            .findings(&w.p, None)
            .unwrap()
            .into_iter()
            .map(|(_, f)| f.record)
            .collect();
        assert_eq!(listed, vec![issue.clone(), issue.clone()]);
        assert_eq!(t.list_findings(&w.p).unwrap()[0].record, issue);
        let mut back = t.get_finding(&local_f).unwrap().unwrap();
        back.withdraw("no").unwrap();
        t.update_finding(&back).unwrap();
        let stored = w.local.get_finding(&local_f).unwrap().unwrap();
        assert_eq!(
            (stored.state, stored.record),
            (FindingState::Withdrawn, old),
            "stores ignore the record on an update"
        );
    }

    // Routing spec §2.2: an issue of the bound repository is GitHub's, even
    // when the local tier holds its URL as an alias of an escalating or
    // escalated item — no mark is read for it, and no tombstone rewrites it.
    #[test]
    fn an_issue_url_is_never_read_through_a_local_mark_or_tombstone() {
        let w = world();
        let t = w.router();
        let gh = w.record("design", "on github");
        let local = w.record("code", "local");
        let unmarked = w.record("code", "unmarked");
        w.local.add_alias(local.iri(), gh.iri().clone()).unwrap();
        w.local.mark(local.iri(), &mark()).unwrap();
        let asked = w.issues.asked();
        assert_eq!(t.escalating(local.iri()).unwrap(), Some(mark()));
        assert_eq!(t.escalating(unmarked.iri()).unwrap(), None);
        assert_eq!(t.escalating(gh.iri()).unwrap(), None);
        assert_eq!(w.issues.asked(), asked, "GitHub is not asked for a mark");
        let issue = w
            .issues
            .add_record_with_area(&w.p, "local", Some("code"))
            .unwrap();
        w.local.tombstone(local.iri(), issue.iri()).unwrap();
        let mut f = Finding::raise(w.p.clone(), gh.clone(), "rev", "c");
        f.area = Some("design".into());
        let id = t.add_finding(f).unwrap();
        assert_eq!(t.get_finding(&id).unwrap().unwrap().record, gh);
        assert_eq!(t.findings(&w.p, None).unwrap()[0].1.record, gh);
    }

    // Routing spec §2.4, §3.6: a marked item reads as itself and refuses a
    // write through the router; a tombstoned item is not listed.
    #[test]
    fn a_marked_item_refuses_a_write_and_a_tombstoned_one_is_not_listed() {
        let w = world();
        let t = w.router();
        let marked = w.record("code", "marked");
        let mut f = Finding::raise(w.p.clone(), marked.clone(), "rev", "c");
        f.area = Some("code".into());
        let kept = t.add_finding(f.clone()).unwrap();
        let gone_f = t.add_finding(f).unwrap();
        w.local.mark(marked.iri(), &mark()).unwrap();
        let err = t.set_record_state(&marked, State::Doing).unwrap_err();
        assert!(
            matches!(&err, StoreError::Escalating { id, .. } if id == marked.iri()),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains("so this store refuses to change it"),
            "{err}"
        );
        assert_eq!(t.get_record(&marked).unwrap().unwrap().id, marked);
        let gone = w.record("code", "gone");
        let issue = w.escalated(&gone, "gone");
        w.tombstone(gone_f.iri(), &MemIssues::issue(50));
        let records: Vec<(Tier, RecordId)> = t
            .records(&w.p, None)
            .unwrap()
            .into_iter()
            .map(|(tier, r)| (tier, r.id))
            .collect();
        assert_eq!(records, vec![(Tier::Local, marked), (Tier::Github, issue)]);
        let findings: Vec<(Tier, FindingId)> = t
            .findings(&w.p, None)
            .unwrap()
            .into_iter()
            .map(|(tier, f)| (tier, f.id))
            .collect();
        assert_eq!(findings, vec![(Tier::Local, kept)]);
    }

    // Routing spec §1.2: an escalated item is named once, by the tier it
    // lives in now.
    #[test]
    fn an_escalated_item_names_its_area_only_from_its_issue() {
        let w = world();
        let t = w.router();
        let old = w.record("code", "fix");
        let issue = w.escalated(&old, "fix");
        assert_eq!(
            t.items_naming_area(&w.p, "code").unwrap(),
            vec![(Tier::Github, Kind::Record, issue.0)]
        );
    }

    // Routing spec §2.2: a marked item and the issue whose alias is its IRI
    // are one item. Until the issue exists the old IRI reads the local item;
    // once it does, any name of the item reads, writes and places a finding
    // about the issue. An unmarked item asks nothing of GitHub.
    #[test]
    fn a_marked_item_is_its_issue_once_the_issue_exists() {
        let w = world();
        let t = w.router();
        let r = w.record("code", "fix");
        let alias = crate::ids::seq_iri(77);
        w.local.add_alias(r.iri(), alias.clone()).unwrap();
        let mut f = Finding::raise(w.p.clone(), r.clone(), "rev", "c");
        f.area = Some("code".into());
        let f = t.add_finding(f).unwrap();
        let asked = w.issues.asked();
        assert_eq!(t.get_record(&r).unwrap().unwrap().id, r);
        assert_eq!(w.issues.asked(), asked, "an unmarked item asks nothing");
        w.local.mark(r.iri(), &mark()).unwrap();
        w.local.mark(f.iri(), &mark()).unwrap();
        assert_eq!(t.get_record(&r).unwrap().unwrap().id, r, "no issue yet");
        assert_eq!(t.get_finding(&f).unwrap().unwrap().id, f, "no issue yet");
        let record = w.local.get_record(&r).unwrap().unwrap();
        let issue = RecordId(w.issued(Outgoing::Record {
            record,
            findings: vec![],
        }));
        let finding = w.local.get_finding(&f).unwrap().unwrap();
        let seen = RecordSeen {
            id: issue.clone(),
            title: "fix".into(),
            tier: Tier::Github,
        };
        let f_issue = FindingId(w.issued(Outgoing::Finding {
            finding,
            record: seen,
        }));
        assert_eq!(t.get_record(&r).unwrap().unwrap().id, issue);
        assert_eq!(t.get_record(&RecordId(alias)).unwrap().unwrap().id, issue);
        assert_eq!(t.get_finding(&f).unwrap().unwrap().id, f_issue);
        let mut about = Finding::raise(w.p.clone(), r.clone(), "rev", "about");
        about.area = Some("code".into());
        let at = t.place_finding(&about, None).unwrap();
        assert_eq!(
            (at.record().id.clone(), at.record().tier),
            (issue.clone(), Tier::Github)
        );
        t.set_record_state(&r, State::Doing).unwrap();
        assert_eq!(
            w.issues.get_record(&issue).unwrap().unwrap().state,
            State::Doing
        );
        assert_eq!(
            w.local.get_record(&r).unwrap().unwrap().state,
            State::Todo,
            "the local item is not written"
        );
    }

    // Routing spec §2.2, §3.3 step 1: a marked item's issue takes over only
    // when GitHub answers for it. A GitHub that cannot be opened or reached
    // leaves the local item — even when the issue exists — which reads as
    // itself and refuses a write.
    #[test]
    fn a_marked_item_with_github_unbound_or_unreachable_is_the_local_item() {
        let w = world();
        let t = w.router();
        let r = w.record("code", "fix");
        w.local.mark(r.iri(), &mark()).unwrap();
        let record = w.local.get_record(&r).unwrap().unwrap();
        w.issued(Outgoing::Record {
            record,
            findings: vec![],
        });
        for (unbound, down) in [(true, false), (false, true)] {
            w.issues.set_unbound(unbound);
            w.issues.set_down(down);
            let read = t.get_record(&r).unwrap().unwrap();
            assert_eq!(read.id, r, "unbound {unbound}, down {down}");
            let err = t.set_record_state(&r, State::Doing).unwrap_err();
            assert!(
                matches!(&err, StoreError::Escalating { id, .. } if id == r.iri()),
                "unbound {unbound}, down {down}: {err:?}"
            );
        }
    }

    // Routing spec §2.2, §2.4: a merged list shows a marked item whose issue
    // exists once — as the issue; a list of the local tier alone still shows
    // the local item. An unmarked local item stays listed even when an issue
    // names it among its aliases.
    #[test]
    fn a_merged_list_shows_a_marked_item_with_an_issue_once() {
        let w = world();
        let t = w.router();
        let r = w.record("code", "fix");
        let other = w.record("code", "other");
        let mut f = Finding::raise(w.p.clone(), r.clone(), "rev", "c");
        f.area = Some("code".into());
        let f = t.add_finding(f).unwrap();
        w.local.mark(r.iri(), &mark()).unwrap();
        w.local.mark(f.iri(), &mark()).unwrap();
        let record = w.local.get_record(&r).unwrap().unwrap();
        let issue = RecordId(w.issued(Outgoing::Record {
            record,
            findings: vec![],
        }));
        let finding = w.local.get_finding(&f).unwrap().unwrap();
        let seen = RecordSeen {
            id: issue.clone(),
            title: "fix".into(),
            tier: Tier::Github,
        };
        let f_issue = FindingId(w.issued(Outgoing::Finding {
            finding,
            record: seen,
        }));
        let stray = w
            .issues
            .add_record_with_area(&w.p, "stray", Some("code"))
            .unwrap();
        w.issues.add_alias(stray.iri(), other.0.clone()).unwrap();
        let records = |only: Option<Tier>| -> Vec<(Tier, RecordId)> {
            let got = t.records(&w.p, only).unwrap();
            got.into_iter().map(|(tier, r)| (tier, r.id)).collect()
        };
        assert_eq!(
            records(None),
            vec![
                (Tier::Local, other.clone()),
                (Tier::Github, issue),
                (Tier::Github, stray)
            ]
        );
        assert_eq!(
            records(Some(Tier::Local)),
            vec![(Tier::Local, r), (Tier::Local, other)]
        );
        let findings = |only: Option<Tier>| -> Vec<(Tier, FindingId)> {
            let got = t.findings(&w.p, only).unwrap();
            got.into_iter().map(|(tier, f)| (tier, f.id)).collect()
        };
        assert_eq!(findings(None), vec![(Tier::Github, f_issue)]);
        assert_eq!(findings(Some(Tier::Local)), vec![(Tier::Local, f)]);
    }

    /// The tracker suites make items without an area, which a routed
    /// project refuses: this gives each one `code`, which every map here
    /// declares, and passes everything else through.
    struct WithCode<'a>(&'a dyn Tracker);

    impl Tracker for WithCode<'_> {
        fn add_record_with_area(
            &self,
            p: &ProjectId,
            t: &str,
            a: Option<&str>,
        ) -> Result<RecordId, StoreError> {
            self.0.add_record_with_area(p, t, a.or(Some("code")))
        }
        fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
            self.0.get_record(id)
        }
        fn list_records(&self, p: &ProjectId) -> Result<Vec<Record>, StoreError> {
            self.0.list_records(p)
        }
        fn set_record_state(&self, id: &RecordId, s: State) -> Result<(), StoreError> {
            self.0.set_record_state(id, s)
        }
        fn add_finding(&self, f: Finding) -> Result<FindingId, StoreError> {
            self.0.add_finding(f)
        }
        fn add_finding_checked(
            &self,
            f: Finding,
            r: ForeignRecord,
        ) -> Result<FindingId, StoreError> {
            self.0.add_finding_checked(f, r)
        }
        fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
            self.0.get_finding(id)
        }
        fn update_finding(&self, f: &Finding) -> Result<(), StoreError> {
            self.0.update_finding(f)
        }
        fn list_findings(&self, p: &ProjectId) -> Result<Vec<Finding>, StoreError> {
            self.0.list_findings(p)
        }
        fn withdrawals_by(&self, a: &str) -> Result<u64, StoreError> {
            self.0.withdrawals_by(a)
        }
        fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
            self.0.add_alias(primary, alias)
        }
    }

    struct Over {
        local: MemStore,
        issues: MemIssues,
        every: EveryProject,
    }

    impl crate::conformance::Fixture for Over {
        fn with(&self, f: &mut dyn FnMut(&crate::conformance::Bound<'_>)) {
            let router = TieredTracker {
                catalog: &self.local,
                local: &self.local,
                routes: &self.every,
                github: &self.issues,
                escalations: &self.local,
            };
            let tracker = WithCode(&router);
            f(&crate::conformance::Bound {
                catalog: &self.local,
                tracker: &tracker,
                ledger: &self.local,
                handles: &self.local,
            });
        }
    }

    fn over(code: Tier) -> Over {
        Over {
            local: MemStore::default(),
            issues: MemIssues::default(),
            every: EveryProject(RoutingMap::starting().with("code", code, false)),
        }
    }

    // Routing spec §2: "The `Tracker` conformance suite runs over it."
    #[test]
    fn the_router_meets_the_tracker_contract_with_code_in_either_tier() {
        crate::conformance::tracker(|| over(Tier::Local));
        crate::conformance::tracker(|| over(Tier::Github));
    }
}
