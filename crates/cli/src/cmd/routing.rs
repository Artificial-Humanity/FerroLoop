//! `fl routing` (routing spec §1.2): the areas a project declares, the tier
//! each one routes a new item to, and whether it is sensitive.

use crate::config::TrackerBinding;
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
        /// sends to a repository that is not private. Without it, an area
        /// keeps its sensitivity (decision 22).
        #[arg(long)]
        sensitive: bool,
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
            Cmd::Set { project, .. } | Cmd::Show { project } => vec![project],
        }
    }

    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }

    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }

    /// Whether this command reads records or findings, in either tier.
    pub fn needs_tracker(&self) -> bool {
        match self {
            Cmd::Set { .. } | Cmd::Show { .. } => false,
        }
    }
}

/// `bound`: the project's tracker binding in the config, when it was read:
/// the first `set` says how handles change from the project's mode before
/// (routing spec §2.3).
pub fn run(ctx: &Ctx<'_>, cmd: Cmd, bound: Option<&TrackerBinding>) -> Result<i32> {
    let store = ctx.store;
    match cmd {
        Cmd::Set {
            project,
            area,
            tier,
            sensitive,
        } => {
            let p = project_of(ctx, &project)?;
            routing::area_name(&area).map_err(|why| anyhow::anyhow!("{why}"))?;
            // Routing spec decision 22: without `--sensitive`, an area keeps
            // its sensitivity; a tier change never clears it.
            let asked = if sensitive { Some(true) } else { None };
            let (map, first) = routing::after_set(store.routes(&p)?.as_ref(), &area, tier, asked);
            store.set_routes(&p, &map)?;
            if first {
                eprintln!(
                    "notice: project {} had no routing map, so fl wrote the starting set first: {}",
                    refs::show(store, Kind::Project, p.iri())?,
                    starting_set()
                );
                eprintln!("{}", handle_change(bound.is_some()));
            }
            print_route(map.route(&area).expect("the area was just set"));
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

fn print_route(a: &AreaRoute) {
    let sensitive = if a.sensitive { "sensitive" } else { "-" };
    println!("{}\t{}\t{sensitive}", a.area, a.tier.as_wire());
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
