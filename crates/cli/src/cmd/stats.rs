use crate::refs::{self, Ref};
use anyhow::Result;
use clap::Args;
use fl_core::ids::ProjectId;
use fl_core::log::Attempt;
use fl_core::split::{Coverage, SplitLedger};
use fl_core::store::Ledger;
use fl_core::{Iri, Kind};
use fl_store::RedbStore;
use std::collections::BTreeMap;

/// Where `fl stats` reads attempts from (GitHub ledger spec §2.5, §2.6).
pub enum Source<'a> {
    /// No GitHub ledger is involved: the local store is the whole ledger.
    Local,
    /// The store records a GitHub ledger this command does not read, and
    /// why: the count covers the local store only, and says so.
    LocalOnly(String),
    /// The local store and the project's GitHub ledger, merged.
    Split(&'a SplitLedger<'a>),
}

#[derive(Args)]
pub struct Cmd {
    #[arg(long)]
    pub project: Ref,
}

impl Cmd {
    /// The one item this command names, by `Ref` — the single source
    /// `iris()` and `has_handle()` both derive from.
    fn refs(&self) -> Vec<&Ref> {
        vec![&self.project]
    }

    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }

    /// Whether this command names its item by handle rather than IRI.
    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }
}

pub fn run(store: &RedbStore, cmd: Cmd, source: Source<'_>) -> Result<i32> {
    let project = ProjectId(refs::resolve(
        store,
        store.label(),
        Kind::Project,
        &cmd.project,
    )?);
    let (attempts, coverage) = match source {
        Source::Local => (store.attempts(&project)?, Coverage::Complete),
        Source::LocalOnly(reason) => (store.attempts(&project)?, Coverage::LocalOnly { reason }),
        // ⚠ Falls back to the local store only when GitHub could not be
        // read; a damaged or missing ledger is an error.
        Source::Split(split) => split.attempts_for_stats(&project)?,
    };
    for line in report(&attempts, &coverage) {
        println!("{line}");
    }
    Ok(0)
}

/// The report's lines.
///
/// ⚠ The count is printed even when it is zero: a report that prints
/// nothing when it found nothing is indistinguishable from a report that
/// did not run. And a report over the local store alone says so (GitHub
/// ledger spec §2.5): a count that silently omitted GitHub would read as the
/// total.
fn report(attempts: &[Attempt], coverage: &Coverage) -> Vec<String> {
    let mut out = vec![format!("attempts: {}", attempts.len())];
    if let Coverage::LocalOnly { reason } = coverage {
        out.push(format!(
            "note: this covers the local store only, because {reason}"
        ));
    }
    if attempts.is_empty() {
        return out;
    }

    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_cost = 0u64;
    let mut total_ms = 0u64;
    for a in attempts {
        *by_status.entry(a.status.as_wire().to_string()).or_default() += 1;
        total_cost += a.cost_usd_micros;
        total_ms += a.duration_ms;
    }
    for (status, n) in by_status {
        out.push(format!("  {status}: {n}"));
    }
    out.push(format!("wall clock: {total_ms}ms"));
    out.push(format!("cost: {total_cost} micro-USD"));
    if total_cost == 0 {
        out.push(
            "note: no adapter reported a cost, so the figure above is a floor and not a total"
                .to_string(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Spec §2.5: a report over the local store alone says so — a count that
    // silently omitted GitHub would read as the total.
    #[test]
    fn a_local_only_report_says_so_even_when_it_found_nothing() {
        let lines = report(
            &[],
            &Coverage::LocalOnly {
                reason: "GitHub could not be read: connection refused".into(),
            },
        );
        assert_eq!(lines[0], "attempts: 0");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("note: this covers the local store only")
                    && l.contains("connection refused")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_complete_report_adds_no_note() {
        let lines = report(&[], &Coverage::Complete);
        assert_eq!(lines, vec!["attempts: 0".to_string()]);
    }
}
