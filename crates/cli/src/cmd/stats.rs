use anyhow::Result;
use clap::Args;
use fl_core::ids::ProjectId;
use fl_core::store::Ledger;
use fl_store::RedbStore;
use std::collections::BTreeMap;

#[derive(Args)]
pub struct Cmd {
    #[arg(long)]
    pub project: u64,
}

pub fn run(store: &RedbStore, cmd: Cmd) -> Result<i32> {
    let attempts = store.attempts(&ProjectId(cmd.project))?;

    // ⚠ Printed even when it is zero. A report that prints nothing when it
    // found nothing is indistinguishable from a report that did not run.
    println!("attempts: {}", attempts.len());

    if attempts.is_empty() {
        return Ok(0);
    }

    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_cost = 0u64;
    let mut total_ms = 0u64;
    for a in &attempts {
        *by_status.entry(a.status.as_wire().to_string()).or_default() += 1;
        total_cost += a.cost_usd_micros;
        total_ms += a.duration_ms;
    }
    for (status, n) in by_status {
        println!("  {status}: {n}");
    }
    println!("wall clock: {total_ms}ms");
    println!("cost: {total_cost} micro-USD");
    if total_cost == 0 {
        println!(
            "note: no adapter reported a cost, so the figure above is a floor and not a total"
        );
    }
    Ok(0)
}
