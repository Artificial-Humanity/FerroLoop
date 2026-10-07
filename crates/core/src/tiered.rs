//! The routing tracker (routing spec §2): one `Tracker` over a project's
//! two tiers — the local store for developer-level items, a GitHub
//! repository for human-level ones — routing each new item by its area.

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

    /// `act` in the tier that owns `id` (§2.2): an issue of the bound
    /// repository in GitHub; any other id in the local tier, then — if the
    /// local tier never held it — in GitHub, whose alias scan finds an item
    /// another machine moved there. With no binding, an issue URL the local
    /// tier does not hold is the missing tier's (§1.3). ⚠ An id neither tier
    /// holds is `Elsewhere`, never `NotOwned`; a tier that cannot be reached
    /// is its own error, never "not held".
    pub(crate) fn route<T>(
        &self,
        id: &Iri,
        act: impl Fn(&dyn Tracker) -> Result<T, StoreError>,
    ) -> Result<(Tier, T), StoreError> {
        if self.github.claims(id) {
            return Ok((Tier::Github, act(self.github.tracker()?)?));
        }
        let mut searched = match act(self.local) {
            Err(StoreError::NotOwned { searched, .. }) => searched,
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
        match act(self.github.tracker()?) {
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
        match self.route(id.iri(), |t| t.get_record(id))? {
            (tier, Some(r)) => Ok((tier, r)),
            (_, None) => Err(StoreError::NoSuchRecord(id.clone())),
        }
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
            Err(e) if only.is_none() && tier == Tier::Github => Err(RoutingFault::TierUnreadable {
                tier,
                cause: as_clause(&e),
            }
            .into()),
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
        Ok(out)
    }

    /// The project's findings in both tiers, or in `only`, each with its
    /// tier (§2.4).
    pub fn findings(
        &self,
        project: &ProjectId,
        only: Option<Tier>,
    ) -> Result<Vec<(Tier, Finding)>, StoreError> {
        let mut out = Vec::new();
        for tier in tiers(only) {
            let got = self.read(tier, only, |t| t.list_findings(project))?;
            out.extend(got.into_iter().map(|f| (tier, f)));
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
        self.route(id.iri(), |t| t.get_record(id)).map(|(_, r)| r)
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        Ok(self
            .records(project, None)?
            .into_iter()
            .map(|(_, r)| r)
            .collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.route(id.iri(), |t| t.set_record_state(id, state))
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
        self.route(id.iri(), |t| t.get_finding(id)).map(|(_, f)| f)
    }

    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.route(finding.id.iri(), |t| t.update_finding(finding))
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
        self.route(primary, |t| t.add_alias(primary, alias.clone()))
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
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
            }
        }

        /// A record in `area`, placed by the map.
        fn record(&self, area: &str, title: &str) -> RecordId {
            let t = self.router();
            let at = t.place_record(&self.p, Some(area), None).unwrap();
            t.add_record_at(&self.p, title, &at).unwrap()
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
