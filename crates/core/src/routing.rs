//! Two-tier routing (routing spec §1): a project's areas, the tier each one
//! routes a new item to, and whether it is sensitive.

use crate::ids::ProjectId;
use crate::store::StoreError;
use serde::{Deserialize, Serialize};

/// One of a routed project's two trackers (routing spec decision 8): the
/// local store, or the GitHub repository the machine's config binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Local,
    Github,
}

crate::wire::wire_names!(Tier as tier_wire {
    Local => "local",
    Github => "github",
});

// `--tier` and `fl routing set` take a tier by name.
crate::wire::wire_parse!(Tier as tier_parse);

/// The longest area name: GitHub's label limit is 50 characters, and
/// `fl:area/` takes 8 (routing spec §1.1).
pub const AREA_MAX: usize = 32;

/// Whether `name` can be an area: 1 to 32 of lowercase `a-z`, `0-9` and
/// `-` (routing spec §1.1). The `Err` says what an area name is.
pub fn area_name(name: &str) -> Result<(), String> {
    let chars = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if name.is_empty() || name.len() > AREA_MAX || !chars {
        return Err(format!(
            "`{name}` is not an area name: 1 to {AREA_MAX} of lowercase letters, digits and `-`"
        ));
    }
    Ok(())
}

/// Where one area routes a new item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaRoute {
    pub area: String,
    pub tier: Tier,
    /// An item made in a sensitive area is treated as a security item
    /// (routing spec decision 13).
    pub sensitive: bool,
}

/// A project's routing map (routing spec §1.2), sorted by area, each area
/// once. ⚠ A list, not a map: the manifest hashes its bytes, and a list in
/// a fixed order serializes the same way every time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoutingMap {
    pub areas: Vec<AreaRoute>,
}

impl RoutingMap {
    /// What the project's first `fl routing set` writes before the area it
    /// names (routing spec §1.2): developer-level areas local, human-level
    /// ones on GitHub, `security` on GitHub and sensitive.
    pub fn starting() -> Self {
        let route = |area: &str, tier, sensitive| AreaRoute {
            area: area.to_string(),
            tier,
            sensitive,
        };
        Self {
            areas: vec![
                route("code", Tier::Local, false),
                route("design", Tier::Github, false),
                route("product", Tier::Github, false),
                route("security", Tier::Github, true),
                route("tests", Tier::Local, false),
            ],
        }
    }

    pub fn route(&self, area: &str) -> Option<&AreaRoute> {
        self.areas.iter().find(|a| a.area == area)
    }

    /// Every area this map declares, in order, for a refusal to list.
    pub fn declared(&self) -> Vec<String> {
        self.areas.iter().map(|a| a.area.clone()).collect()
    }

    /// This map with `area` routed to `tier` — added, or changed in place.
    pub fn with(&self, area: &str, tier: Tier, sensitive: bool) -> Self {
        let mut areas: Vec<AreaRoute> = self
            .areas
            .iter()
            .filter(|a| a.area != area)
            .cloned()
            .collect();
        areas.push(AreaRoute {
            area: area.to_string(),
            tier,
            sensitive,
        });
        areas.sort_by(|a, b| a.area.cmp(&b.area));
        Self { areas }
    }

    /// This map without `area`.
    pub fn without(&self, area: &str) -> Self {
        Self {
            areas: self
                .areas
                .iter()
                .filter(|a| a.area != area)
                .cloned()
                .collect(),
        }
    }

    /// Whether this map is one fl writes: every name an area name, sorted,
    /// each once. A map read from a manifest or a store is checked with
    /// this before it routes anything.
    pub fn check(&self) -> Result<(), String> {
        for a in &self.areas {
            area_name(&a.area)?;
        }
        for pair in self.areas.windows(2) {
            if pair[0].area >= pair[1].area {
                return Err(format!(
                    "its areas are not each listed once in order: `{}` then `{}`",
                    pair[0].area, pair[1].area
                ));
            }
        }
        Ok(())
    }
}

/// The map after `fl routing set <area> <tier>` (routing spec §1.2), and
/// whether this was the project's first set — which writes the starting
/// set first.
///
/// `sensitive`: `Some` sets the area's sensitivity; `None` keeps what the
/// map — on the first set, the starting set — says, and `false` for a new
/// area. ⚠ A set that only changes a tier never clears a sensitivity
/// (decision 22).
pub fn after_set(
    current: Option<&RoutingMap>,
    area: &str,
    tier: Tier,
    sensitive: Option<bool>,
) -> (RoutingMap, bool) {
    let (base, first) = match current {
        Some(map) => (map.clone(), false),
        None => (RoutingMap::starting(), true),
    };
    let sensitive = sensitive.unwrap_or_else(|| base.route(area).is_some_and(|r| r.sensitive));
    (base.with(area, tier, sensitive), first)
}

/// Where the router reads a project's routing map: the local store, which
/// holds the map it authored or imported (routing spec §1.2). `None`: the
/// project has no map.
pub trait Routes {
    fn routes(&self, project: &ProjectId) -> Result<Option<RoutingMap>, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_area_name_is_up_to_32_lowercase_letters_digits_and_hyphens() {
        for ok in ["code", "a", "x-1", "2fa", &"a".repeat(32)] {
            assert_eq!(area_name(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "Code",
            "a b",
            "a_b",
            "a/b",
            "é",
            "fl:x",
            &"a".repeat(33),
        ] {
            let why = area_name(bad).expect_err(bad);
            assert!(why.contains("is not an area name"), "{why}");
        }
    }

    #[test]
    fn the_starting_set_routes_developer_areas_local_and_human_areas_to_github() {
        let s = RoutingMap::starting();
        let got: Vec<(&str, Tier, bool)> = s
            .areas
            .iter()
            .map(|a| (a.area.as_str(), a.tier, a.sensitive))
            .collect();
        assert_eq!(
            got,
            vec![
                ("code", Tier::Local, false),
                ("design", Tier::Github, false),
                ("product", Tier::Github, false),
                ("security", Tier::Github, true),
                ("tests", Tier::Local, false),
            ]
        );
        assert_eq!(s.check(), Ok(()));
    }

    #[test]
    fn the_first_set_writes_the_starting_set_first_and_a_later_one_changes_one_area() {
        let (first, was_first) = after_set(None, "ops", Tier::Github, None);
        assert!(was_first);
        assert_eq!(first.declared().len(), 6);
        assert_eq!(first.route("ops").unwrap().tier, Tier::Github);
        assert_eq!(first.route("code").unwrap().tier, Tier::Local);
        assert_eq!(first.check(), Ok(()), "kept in order");
        let (next, was_first) = after_set(Some(&first), "code", Tier::Github, Some(true));
        assert!(!was_first);
        let code = next.route("code").unwrap();
        assert_eq!((code.tier, code.sensitive), (Tier::Github, true));
        assert_eq!(
            next.declared(),
            first.declared(),
            "one area changed, none added"
        );
        let (named, _) = after_set(None, "code", Tier::Github, None);
        assert_eq!(
            named.route("code").unwrap().tier,
            Tier::Github,
            "the set wins"
        );
    }

    // Routing spec decision 22: a set that names no sensitivity keeps the
    // area's — a tier change never clears it — and only `Some(false)` does.
    #[test]
    fn a_set_that_names_no_sensitivity_keeps_the_areas() {
        let starting = RoutingMap::starting();
        let (moved, _) = after_set(Some(&starting), "security", Tier::Local, None);
        let security = moved.route("security").unwrap();
        assert_eq!((security.tier, security.sensitive), (Tier::Local, true));
        let (first, _) = after_set(None, "security", Tier::Local, None);
        assert!(
            first.route("security").unwrap().sensitive,
            "the starting set's, kept"
        );
        let (new, _) = after_set(Some(&starting), "ops", Tier::Local, None);
        assert!(
            !new.route("ops").unwrap().sensitive,
            "a new area is not sensitive"
        );
        let (cleared, _) = after_set(Some(&starting), "security", Tier::Github, Some(false));
        assert!(!cleared.route("security").unwrap().sensitive);
    }

    #[test]
    fn without_removes_one_area_only() {
        let m = RoutingMap::starting().without("design");
        assert_eq!(m.declared(), vec!["code", "product", "security", "tests"]);
    }

    #[test]
    fn a_map_out_of_order_with_an_area_twice_or_a_bad_name_is_refused() {
        let mut m = RoutingMap::starting();
        m.areas.swap(0, 1);
        assert!(m.check().unwrap_err().contains("in order"));
        let mut m = RoutingMap::starting();
        m.areas.insert(1, m.areas[0].clone());
        assert!(m.check().unwrap_err().contains("in order"));
        let mut m = RoutingMap::starting();
        m.areas[0].area = "Code".into();
        assert!(m.check().unwrap_err().contains("is not an area name"));
    }

    #[test]
    fn a_map_serializes_as_a_list_in_order() {
        let json = serde_json::to_string(&RoutingMap::starting().without("design")).unwrap();
        assert!(
            json.starts_with("[{\"area\":\"code\",\"tier\":\"local\",\"sensitive\":false}"),
            "{json}"
        );
    }
}
