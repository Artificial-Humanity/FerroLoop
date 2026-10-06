//! The routing tracker (routing spec §2): one `Tracker` over a project's
//! two tiers — the local store for developer-level items, a GitHub
//! repository for human-level ones — routing each new item by its area.

use crate::finding::Finding;
use crate::ids::{FindingId, Kind, ProjectId, RecordId};
use crate::iri::Iri;
use crate::model::Record;
use crate::routing::{ForeignRecord, GithubTier, Routes, RoutingFault, RoutingMap, Tier};
use crate::store::{Catalog, StoreError, Tracker};

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
    /// chose the tier, the refusal names `--tier local` (§2.1).
    fn private_or_refuse(&self, by_map: bool, what: &str) -> Result<(), StoreError> {
        match self.github.require_private() {
            Err(StoreError::SecurityNotPrivate { repo, visibility }) if by_map => {
                Err(RoutingFault::SensitiveToPublic {
                    what: what.to_string(),
                    repo,
                    visibility,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::mem_issues::MemIssues;
    use crate::model::{CommandSpec, GateKind, PopulationDelivery, Selector};

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
            matches!(err, StoreError::SecurityNotPrivate { .. }),
            "{err:?}"
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
            matches!(err, StoreError::SecurityNotPrivate { .. }),
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
}
