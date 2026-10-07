//! `fl routing` (routing spec §1.2): the areas a project declares, the tier
//! each one routes a new item to, and whether it is sensitive.

use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::ProjectId;
use fl_core::routing::{self, AreaRoute, Routes, RoutingMap, Tier};
use fl_core::store::Catalog;
use fl_core::{Iri, Kind};

/// A tier by its name, for `fl routing set` and `--tier`.
pub fn parse_tier(s: &str) -> Result<Tier, String> {
    Tier::from_wire(s).ok_or_else(|| {
        format!(
            "`{s}` is not a tier. The tiers are: {}",
            Tier::wire_values()
        )
    })
}

/// The refusal for `--area` or `--tier` in a store with no routing map.
pub fn not_routed(flag: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`{flag}` needs a routing map, and this project declares none: an area is a name its \
         routing map declares, and a tier one it routes to. `fl routing set --project \
         <project> <area> <tier>` declares one"
    )
}

/// Routing spec decision 12: a routed project keeps its runs and decisions
/// in the local ledger in this release — `SplitLedger::flush` refuses a
/// decision about a record the repository does not hold, so every gated
/// move of a local record would fail.
pub fn github_ledger_refusal(repo: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the binding of `{repo}` names `ledger = \"github\"`, and a routed project keeps its runs \
         and decisions in the local ledger: fl does not publish a routed project's ledger to \
         GitHub yet. Remove `ledger = \"github\"` from the binding"
    )
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Route an area to a tier for new items. The project's first `set`
    /// writes the starting set first.
    Set {
        #[arg(long)]
        project: Ref,
        area: String,
        #[arg(value_parser = parse_tier)]
        tier: Tier,
        /// Items made in this area are security items, which the map never
        /// sends to a repository that is not private. Without this flag or
        /// `--not-sensitive`, an area keeps its sensitivity; an area the
        /// map does not declare needs one of the two, except on the
        /// project's first set.
        #[arg(long, conflicts_with = "not_sensitive")]
        sensitive: bool,
        /// Clear the area's sensitivity, or add an area as not sensitive.
        /// Refused while any item in either tier names the area.
        #[arg(long)]
        not_sensitive: bool,
    },
    /// Remove an area. Refused while any item in either tier names it.
    Remove {
        #[arg(long)]
        project: Ref,
        area: String,
    },
    /// Print the project's routing map.
    Show {
        #[arg(long)]
        project: Ref,
    },
}

impl Cmd {
    fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Set { project, .. } | Cmd::Remove { project, .. } | Cmd::Show { project } => {
                vec![project]
            }
        }
    }

    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }

    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }

    /// Whether this command writes a routing map.
    pub fn sets_routing(&self) -> bool {
        matches!(self, Cmd::Set { .. })
    }

    /// Whether this command reads records or findings, in either tier.
    pub fn needs_tracker(&self) -> bool {
        match self {
            // Every item that names the area, in both tiers (routing spec
            // §1.2, decision 22).
            Cmd::Set { not_sensitive, .. } => *not_sensitive,
            Cmd::Show { .. } => false,
            Cmd::Remove { .. } => true,
        }
    }
}

/// `was_github`: whether the project's config entry binds it to GitHub, when
/// fl read the entry for this store: the first `set` says how handles change
/// from the project's mode before (routing spec §2.3). `None`: fl does not
/// know the mode before, and says nothing rather than guess.
pub fn run(ctx: &Ctx<'_>, cmd: Cmd, was_github: Option<bool>) -> Result<i32> {
    let store = ctx.store;
    match cmd {
        Cmd::Set {
            project,
            area,
            tier,
            sensitive,
            not_sensitive,
        } => {
            let p = project_of(ctx, &project)?;
            // Before either tier is read: a store that imported the project
            // never changes its map.
            store.refuse_if_imported(&p, fl_store::CHANGE_ROUTING)?;
            routing::area_name(&area).map_err(|why| anyhow::anyhow!("{why}"))?;
            let current = store.routes(&p)?;
            // Routing spec decision 22: neither flag keeps the area's
            // sensitivity; clearing it is refused while any item names it.
            let asked = match (sensitive, not_sensitive) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            // Routing spec decision 23: a later set that adds an area the
            // map does not declare names its sensitivity. The project's
            // first set keeps its default.
            let added = current.as_ref().is_some_and(|m| m.route(&area).is_none());
            if added && asked.is_none() {
                bail!(
                    "`{area}` is not an area project {} declares, so this set must say whether it \
                     is sensitive: add `--sensitive` or `--not-sensitive`. An area once removed \
                     may have been sensitive, and another machine's items may still name it",
                    refs::show(store, Kind::Project, p.iri())?
                );
            }
            // Clearing a sensitivity, or adding an area as not sensitive —
            // which an item naming it would count as (decision 22).
            let clears = asked == Some(false)
                && current
                    .as_ref()
                    .is_some_and(|m| m.route(&area).is_none_or(|r| r.sensitive));
            if clears {
                refuse_while_named(
                    ctx,
                    &p,
                    &area,
                    "Its sensitivity is not cleared: an item made in a sensitive area stays \
                     protected",
                )?;
            }
            let (map, first) = routing::after_set(current.as_ref(), &area, tier, asked);
            store.set_routes(&p, &map)?;
            if first {
                eprintln!(
                    "notice: project {} had no routing map, so fl wrote the starting set first: {}",
                    refs::show(store, Kind::Project, p.iri())?,
                    starting_set()
                );
                if let Some(was_github) = was_github {
                    eprintln!("{}", handle_change(was_github));
                }
            }
            // Routing spec decision 23: the check passed on what this
            // machine can see, which is where the risk is.
            if clears {
                eprintln!(
                    "note: fl checked only this machine's local tier, and GitHub, for items that \
                     name `{area}`: another machine's local items may still name it, and no \
                     longer count as sensitive"
                );
            }
            print_route(map.route(&area).expect("the area was just set"));
        }
        Cmd::Remove { project, area } => {
            let p = project_of(ctx, &project)?;
            store.refuse_if_imported(&p, fl_store::CHANGE_ROUTING)?;
            let shown = refs::show(store, Kind::Project, p.iri())?;
            let Some(map) = store.routes(&p)? else {
                bail!("project {shown} has no routing map, so it declares no area to remove");
            };
            if map.route(&area).is_none() {
                bail!(
                    "`{area}` is not an area project {shown} declares. The declared areas: {}",
                    map.declared().join(", ")
                );
            }
            refuse_while_named(
                ctx,
                &p,
                &area,
                "It is not removed: an item keeps its area for life; remove the area once none \
                 names it",
            )?;
            store.set_routes(&p, &map.without(&area))?;
            println!("removed\t{area}");
            eprintln!(
                "note: another machine's local items that name `{area}` keep it as history; only \
                 new items are refused it"
            );
        }
        Cmd::Show { project } => {
            let p = project_of(ctx, &project)?;
            match store.routes(&p)? {
                Some(map) => map.areas.iter().for_each(print_route),
                None => eprintln!(
                    "note: project {} has no routing map; `fl routing set` declares its areas",
                    refs::show(store, Kind::Project, p.iri())?
                ),
            }
        }
    }
    Ok(0)
}

/// The project `r` names in this store.
fn project_of(ctx: &Ctx<'_>, r: &Ref) -> Result<ProjectId> {
    let store = ctx.store;
    let p = ProjectId(refs::resolve(store, store.label(), Kind::Project, r)?);
    if store.get_project(&p)?.is_none() {
        bail!(
            "`{r}` is not a project in the store at {}. Run `fl project list` to see the ones \
             that exist.",
            store.label()
        );
    }
    Ok(p)
}

/// Routing spec decision 11, and decision 22's clearing of a sensitivity:
/// refused while any item in either tier names `area`, with the count and up
/// to ten of the items. Both tiers, GitHub by its blocks; a tier that cannot
/// be read is an error, never "no item names it".
fn refuse_while_named(ctx: &Ctx<'_>, p: &ProjectId, area: &str, refused: &str) -> Result<()> {
    let shown = refs::show(ctx.store, Kind::Project, p.iri())?;
    let t = ctx
        .tiers
        .expect("a store that holds a routing map is routed, so it has tiers");
    let items = t.router.items_naming_area(p, area)?;
    if items.is_empty() {
        return Ok(());
    }
    let some: Vec<String> = items
        .iter()
        .take(10)
        .map(|(tier, kind, id)| named(ctx, *tier, *kind, id))
        .collect::<Result<_>>()?;
    let more = if items.len() > 10 { ", …" } else { "" };
    bail!(
        "`{area}` is still named by {} item(s) of project {shown}: {}{more}. {refused}. fl reads \
         only this machine's local tier: another machine's local items may name it too",
        items.len(),
        some.join(", ")
    )
}

fn print_route(a: &AreaRoute) {
    let sensitive = if a.sensitive { "sensitive" } else { "-" };
    println!("{}\t{}\t{sensitive}", a.area, a.tier.as_wire());
}

/// How a refusal names an item: a GitHub item by its issue number, read
/// from its URL — an item that lost its labels has no handle GitHub's
/// lookup would give — and a local one by its handle.
fn named(ctx: &Ctx<'_>, tier: Tier, kind: Kind, id: &Iri) -> Result<String> {
    Ok(match tier {
        Tier::Github => fl_github::meta::parse_issue_url(id)
            .map(|(_, n)| format!("#{n}"))
            .unwrap_or_else(|| id.to_string()),
        Tier::Local => refs::show(ctx.store, kind, id)?,
    })
}

/// The starting set, as a notice says it: read from
/// `RoutingMap::starting()`, so the two cannot drift.
fn starting_set() -> String {
    RoutingMap::starting()
        .areas
        .iter()
        .map(|a| {
            let sensitive = if a.sensitive { ", sensitive" } else { "" };
            format!("{} to {}{sensitive}", a.area, a.tier.as_wire())
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// What changes in how a handle reads once a store is first routed — by
/// the first `set`, or by the import that brings a routing map (routing
/// spec §2.3).
pub fn handle_change(was_github: bool) -> &'static str {
    if was_github {
        "notice: handles change in this project: a bare number such as `41` named GitHub issue \
         41 and now names local item 41; write `#41` for the issue. Local records made before \
         the binding show in lists again."
    } else {
        "notice: handles change in this project: `#3` named local item 3 and now names GitHub \
         issue 3; write `3` for the local item."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tier_is_parsed_by_its_wire_name_only() {
        assert_eq!(parse_tier("local"), Ok(Tier::Local));
        assert_eq!(parse_tier("github"), Ok(Tier::Github));
        for bad in ["Local", "remote", ""] {
            assert!(
                parse_tier(bad).unwrap_err().contains("is not a tier"),
                "{bad}"
            );
        }
    }
}
