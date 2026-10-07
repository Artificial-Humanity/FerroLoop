use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::model::State;
use fl_core::routing::Tier;
use fl_core::store::Catalog;
use fl_core::{Iri, Kind};
use fl_exec::record::{MoveOutcome, move_record};

#[derive(Subcommand)]
pub enum Cmd {
    Add {
        #[arg(long)]
        project: Ref,
        #[arg(long)]
        title: String,
        /// The record's area (routing spec §1.1), which routes it to a tier.
        #[arg(long)]
        area: Option<String>,
        /// The tier, over the one the area routes to (routing spec §2.1).
        #[arg(long, value_parser = crate::cmd::routing::parse_tier)]
        tier: Option<Tier>,
    },
    List {
        #[arg(long)]
        project: Ref,
        /// One tier only, in a routed project (routing spec §2.4).
        #[arg(long, value_parser = crate::cmd::routing::parse_tier)]
        tier: Option<Tier>,
    },
    Move {
        id: Ref,
        #[arg(long = "to")]
        to: String,
    },
}

impl Cmd {
    /// Every item this command names, by `Ref` — the single source `iris()`
    /// and `has_handle()` both derive from, so a `Ref` field added to a
    /// variant here is picked up by both at once. A
    /// target state is not an id and never appears here.
    fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Add { project, .. } => vec![project],
            Cmd::List { project, .. } => vec![project],
            Cmd::Move { id, .. } => vec![id],
        }
    }

    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }

    /// Whether this command names any item by handle rather than IRI.
    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }
}

pub fn run(ctx: &Ctx<'_>, cmd: Cmd) -> Result<i32> {
    let store = ctx.store;
    match cmd {
        Cmd::Add {
            project,
            title,
            area,
            tier,
        } => {
            let p = ProjectId(refs::resolve(
                ctx.handles,
                store.label(),
                Kind::Project,
                &project,
            )?);
            if store.get_project(&p)?.is_none() {
                bail!(
                    "`{project}` is not a project in the store at {}. Run `fl project list` to \
                     see the ones that exist.",
                    store.label()
                );
            }
            let id = match ctx.tiers {
                None => {
                    if area.is_some() {
                        return Err(crate::cmd::routing::not_routed("--area"));
                    }
                    if tier.is_some() {
                        return Err(crate::cmd::routing::not_routed("--tier"));
                    }
                    ctx.tracker.add_record(&p, &title)?
                }
                Some(t) => {
                    crate::cmd::manifest::ensure_routing_current(store, &p)?;
                    let at = t.router.place_record(&p, area.as_deref(), tier)?;
                    t.router.add_record_at(&p, &title, &at)?
                }
            };
            println!("{}\t{title}", ctx.show_item(Kind::Record, id.iri())?);
        }
        Cmd::List { project, tier } => {
            let p = ProjectId(refs::resolve(
                ctx.handles,
                store.label(),
                Kind::Project,
                &project,
            )?);
            match ctx.tiers {
                None => {
                    if tier.is_some() {
                        return Err(crate::cmd::routing::not_routed("--tier"));
                    }
                    for r in ctx.tracker.list_records(&p)? {
                        println!(
                            "{}\t{}\t{}",
                            ctx.show_item(Kind::Record, r.id.iri())?,
                            r.state.as_wire(),
                            r.title
                        );
                    }
                }
                // ⚠ The whole list or an error: `records` refuses when a
                // tier it must read cannot be read (routing spec §2.4).
                Some(t) => {
                    for (in_tier, r) in t.router.records(&p, tier)? {
                        println!(
                            "{}\t{}\t{}\t{}",
                            ctx.show_item(Kind::Record, r.id.iri())?,
                            in_tier.as_wire(),
                            r.state.as_wire(),
                            r.title
                        );
                    }
                }
            }
        }
        Cmd::Move { id, to } => {
            let Some(state) = State::from_wire(&to) else {
                bail!(
                    "`{to}` is not a state. Valid states are: {}.",
                    State::wire_values()
                );
            };
            let r = RecordId(ctx.resolve_item(Kind::Record, &id)?);
            let Some(record) = ctx.tracker.get_record(&r)? else {
                bail!(
                    "`{id}` is not a record in the store at {}. Use \
                     `fl record list --project <project>` to see records that exist.",
                    ctx.tracker_label
                );
            };

            let gated = store
                .list_transitions(&record.project)?
                .iter()
                .any(|t| t.from == record.state && t.to == state);
            if gated {
                crate::cmd::manifest::ensure_import_current(store, &record.project)?;
            }
            // Every move is a decision, gated or not — it is flushed (GitHub
            // ledger spec §2.2) — so its evidence must be publishable before
            // the first gate runs.
            crate::preflight::check(ctx, &record.project)?;

            let moved = move_record(ctx.roles(), &record, state);
            // Spec §4.1: the ledger commit, then the state change, then the
            // comment — posted whether the state change completed or not,
            // saying which (§4.2), before an error becomes the exit.
            crate::comment::after(ctx, &record.project, moved.is_ok());
            let report = moved.map_err(|e| anyhow::anyhow!("{e}"))?;
            crate::ctx::report_flush(&report.flushed);
            // What the person reads back: the record's handle (or its
            // primary IRI), never the alias or IRI they typed.
            let shown = ctx.show_item(Kind::Record, record.id.iri())?;

            if let MoveOutcome::Ungated = report.outcome {
                println!(
                    "{shown}\t{}\tungated: project {} declares no transition from `{}` to `{}`",
                    state.as_wire(),
                    refs::show(ctx.handles, Kind::Project, record.project.iri())?,
                    record.state.as_wire(),
                    state.as_wire()
                );
                return Ok(0);
            }

            for t in &report.transitions {
                for g in &t.gates {
                    let (label, detail) = g.verdict.describe();
                    println!(
                        "{label}\t{}\t{}\t{detail}\t{}ms{}",
                        t.transition,
                        g.name,
                        g.duration_ms,
                        g.staleness.note()
                    );
                    if !g.verdict.is_pass() && !g.output_excerpt.is_empty() {
                        for line in g.output_excerpt.lines().take(20) {
                            println!("\t| {line}");
                        }
                    }
                }
                // Same rule `check` applies: a transition that declares no
                // gates verified nothing, so it cannot authorise a move.
                if t.gates.is_empty() {
                    println!(
                        "FAIL\t{}\tthe transition declares no gates, so nothing was verified",
                        t.transition
                    );
                }
            }

            // Every variant by name: a new outcome must be a compile error
            // here, not something a catch-all prints as "moved".
            match report.outcome {
                MoveOutcome::Refused { code } => {
                    println!("REFUSED\t{shown}\tstays `{}`", record.state.as_wire());
                    return Ok(code);
                }
                MoveOutcome::Moved => {
                    println!("{shown}\t{}", state.as_wire());
                }
                MoveOutcome::Ungated => {
                    unreachable!("an ungated move returns above, before any transition is printed")
                }
            }
        }
    }
    Ok(0)
}
