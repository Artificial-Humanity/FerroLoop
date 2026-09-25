use crate::refs::{self, Ref};
use anyhow::Result;
use clap::Args;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::{Iri, Kind};
use fl_exec::evaluate::evaluate_transition;
use fl_store::RedbStore;

#[derive(Args)]
pub struct Cmd {
    /// The transition to evaluate.
    pub transition: String,
    #[arg(long)]
    pub project: Ref,
    #[arg(long)]
    pub record: Option<Ref>,
}

impl Cmd {
    /// Every item this command names — the transition name is not an id.
    pub fn iris(&self) -> Vec<Iri> {
        let mut refs: Vec<&Ref> = vec![&self.project];
        if let Some(r) = &self.record {
            refs.push(r);
        }
        refs::iris(&refs)
    }
}

pub fn run(store: &RedbStore, cmd: Cmd) -> Result<i32> {
    let project = ProjectId(refs::resolve(
        store,
        store.label(),
        Kind::Project,
        &cmd.project,
    )?);
    let record = match &cmd.record {
        Some(r) => Some(RecordId(refs::resolve(
            store,
            store.label(),
            Kind::Record,
            r,
        )?)),
        None => None,
    };
    let report = evaluate_transition(store, store, &project, &cmd.transition, record.as_ref())
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    for g in &report.gates {
        let (label, detail) = g.verdict.describe();
        let note = g.staleness.note();
        println!("{label}\t{}\t{detail}\t{}ms{note}", g.name, g.duration_ms);
        if !g.verdict.is_pass() && !g.output_excerpt.is_empty() {
            for line in g.output_excerpt.lines().take(20) {
                println!("\t| {line}");
            }
        }
    }

    if report.gates.is_empty() {
        println!(
            "FAIL\t{}\tthe transition declares no gates, so nothing was verified",
            report.transition
        );
        return Ok(1);
    }

    Ok(report.exit_code())
}
