use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Args;
use fl_core::decision::Flushed;
use fl_core::ids::RecordId;
use fl_core::log::{Attempt, AttemptStatus, PathsTouched};
use fl_core::store::{Catalog, Ledger};
use fl_core::{Iri, Kind};
use fl_exec::adapters::ClaudeAdapter;
use fl_exec::runner::{AttemptSpec, Runner};
use fl_exec::stamp;

const KNOWN_ADAPTERS: &str = "claude";

#[derive(Args)]
pub struct Cmd {
    pub record: Ref,
    #[arg(long, default_value = "claude")]
    pub adapter: String,
    #[arg(long)]
    pub instruction: Option<String>,
    #[arg(long, default_value_t = 900)]
    pub timeout_secs: u64,
    #[arg(long, default_value_t = 1_000_000)]
    pub budget_usd_micros: u64,
    /// The binary to invoke. Override when it is not on PATH under this name.
    #[arg(long, default_value = "claude")]
    pub binary: String,
}

impl Cmd {
    /// The one item this command names, by `Ref` — the single source
    /// `iris()` and `has_handle()` both derive from (Fix round 2, item 5).
    fn refs(&self) -> Vec<&Ref> {
        vec![&self.record]
    }

    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }

    /// Whether this command names its item by handle rather than IRI.
    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }
}

pub fn run(ctx: &Ctx<'_>, cmd: Cmd) -> Result<i32> {
    let store = ctx.store;
    if cmd.adapter != "claude" {
        bail!(
            "`{}` is not a known adapter. Milestone 1 ships: {KNOWN_ADAPTERS}.",
            cmd.adapter
        );
    }
    let id = RecordId(refs::resolve(
        ctx.handles,
        &ctx.tracker_label,
        Kind::Record,
        &cmd.record,
    )?);
    let Some(record) = ctx.tracker.get_record(&id)? else {
        bail!(
            "`{}` is not a record in the store at {}. Run `fl record list --project <project>` \
             to see the ones that exist.",
            cmd.record,
            ctx.tracker_label
        );
    };
    let Some(project) = store.get_project(&record.project)? else {
        bail!(
            "record {} belongs to project {}, which no longer exists.",
            cmd.record,
            refs::show(ctx.handles, Kind::Project, record.project.iri())?
        );
    };

    let adapter = ClaudeAdapter::new(cmd.binary);
    let spec = AttemptSpec {
        project_root: std::path::PathBuf::from(&project.root),
        // The record's PRIMARY id, never the alias the person typed: every
        // attempt against one record must name it the same way.
        record: record.id.clone(),
        instruction: cmd.instruction.unwrap_or_else(|| record.title.clone()),
        timeout_secs: cmd.timeout_secs,
        budget_usd_micros: cmd.budget_usd_micros,
    };

    let rt = tokio::runtime::Runtime::new()?;
    let outcome = rt
        .block_on(adapter.attempt(spec))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // ⚠ Recorded whatever the outcome. A crash, a timeout and a refusal all
    // cost something, even when that something is only the wall clock.
    let entry = stamp::entry_id();
    let attempt = Attempt {
        id: Some(entry.clone()),
        at: Some(stamp::now()),
        project: record.project,
        record: record.id,
        adapter: "claude".into(),
        status: outcome.status,
        duration_ms: outcome.duration_ms,
        tokens_in: outcome.tokens_in,
        tokens_out: outcome.tokens_out,
        cost_usd_micros: outcome.cost_usd_micros,
        paths_touched: PathsTouched::Listed(outcome.paths_touched.clone()),
        output_excerpt: Some(outcome.output_excerpt.clone()),
    };
    ctx.ledger.append_attempt(attempt.clone())?;

    println!("{}\t{}ms", outcome.status.as_wire(), outcome.duration_ms);
    if !outcome.output_excerpt.is_empty() {
        for line in outcome.output_excerpt.lines().take(40) {
            println!("\t| {line}");
        }
    }

    let flushed = publish(ctx.ledger, &entry, &attempt)?;
    crate::ctx::report_flush(&flushed);

    Ok(match outcome.status {
        AttemptStatus::Completed => 0,
        _ => 1,
    })
}

/// Publish the attempt after its local append (GitHub ledger spec §2.2).
/// It already ran and cost what it cost, so a failure here refuses nothing
/// (§7): the outcome is printed, the error is reported, and the attempt
/// goes out with the next flush that succeeds (decision 8).
fn publish(ledger: &dyn Ledger, id: &Iri, attempt: &Attempt) -> Result<Flushed> {
    ledger
        .flush(fl_exec::decision::for_attempt(id, attempt))
        .map_err(|e| {
            anyhow::anyhow!(
                "the attempt ran and is recorded in the local store, but it could not be \
                 published to the shared ledger ({e}). Nothing is lost: the next decision that \
                 reaches the ledger publishes it."
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Flushes;
    use fl_core::decision::Outcome;
    use fl_core::ids::{ProjectId, seq_iri};

    fn attempt() -> Attempt {
        Attempt {
            id: Some(seq_iri(50)),
            at: None,
            project: ProjectId(seq_iri(1)),
            record: RecordId(seq_iri(2)),
            adapter: "claude".into(),
            status: AttemptStatus::Timeout,
            duration_ms: 1,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd_micros: 0,
            paths_touched: PathsTouched::Listed(vec![]),
            output_excerpt: Some(String::new()),
        }
    }

    #[test]
    fn an_attempts_decision_rests_on_the_attempt() {
        let ledger = Flushes::default();
        publish(&ledger, &seq_iri(50), &attempt()).unwrap();
        let decisions = ledger.decisions.borrow();
        assert_eq!(decisions[0].rests_on, vec![seq_iri(50)]);
        assert_eq!(
            decisions[0].outcome,
            Outcome::Attempt {
                status: AttemptStatus::Timeout
            }
        );
    }

    // Spec §7 and decision 8: the attempt already ran, so a failed publish
    // refuses nothing — it is reported, and the attempt is kept for the next
    // flush.
    #[test]
    fn an_attempt_that_cannot_be_published_is_reported_as_kept_not_refused() {
        let err = publish(&Flushes::refusing(), &seq_iri(50), &attempt()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("recorded in the local store"), "{msg}");
        assert!(msg.contains("next decision"), "{msg}");
        assert!(!msg.contains("refused:"), "not a refusal: {msg}");
    }
}
