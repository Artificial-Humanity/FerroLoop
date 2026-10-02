use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Args;
use fl_core::ids::RecordId;
use fl_core::log::{Attempt, AttemptStatus, PathsTouched};
use fl_core::store::{Catalog, Ledger, StoreError};
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
    /// `iris()` and `has_handle()` both derive from.
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
    // ⚠ Decision 8: GitHub is checked before the adapter spends anything.
    crate::preflight::check(ctx, &record.project)?;

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
    let (code, said) = settle(ctx.ledger, &entry, &attempt);
    for line in said {
        match line {
            Said::Stdout(l) => println!("{l}"),
            Said::Stderr(l) => eprintln!("{l}"),
        }
    }
    Ok(code)
}

/// One line the command prints, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Said {
    Stdout(String),
    Stderr(String),
}

/// Save the attempt, publish it, and give its exit code and what to print,
/// in order.
///
/// ⚠⚠ Decisions 14 and 15: the attempt already ran and cost what it cost,
/// so the code is the attempt's own — 0 when it completed, 1 otherwise —
/// whatever became of its save or its publish. Exit 2 means "refused"
/// everywhere else, and a script that retries on 2 must never run a paid
/// attempt again. The outcome comes first, whatever became of the save,
/// so a failed save never hides what the attempt did.
fn settle(ledger: &dyn Ledger, id: &Iri, attempt: &Attempt) -> (i32, Vec<Said>) {
    let saved = ledger.append_attempt(attempt.clone());
    let mut said = vec![Said::Stdout(format!(
        "{}\t{}ms",
        attempt.status.as_wire(),
        attempt.duration_ms
    ))];
    if let Some(excerpt) = attempt.output_excerpt.as_deref() {
        said.extend(
            excerpt
                .lines()
                .take(40)
                .map(|l| Said::Stdout(format!("\t| {l}"))),
        );
    }
    match saved {
        // ⚠ Published only once saved: a decision cannot rest on an
        // attempt the local store does not hold.
        Ok(()) => {
            if let Some(warning) = conclude(ledger, id, attempt) {
                said.push(Said::Stderr(format!("warning: {warning}")));
            }
        }
        Err(e) => said.push(Said::Stderr(format!("error: {}", unsaved(&e)))),
    }
    let code = match attempt.status {
        AttemptStatus::Completed => 0,
        _ => 1,
    };
    (code, said)
}

/// What a failed local save says (decision 15).
fn unsaved(e: &StoreError) -> String {
    format!(
        "the attempt ran, but it could not be saved to the local store: {}. It is recorded \
         nowhere, so it was not published either; what it did is printed above, and what it \
         cost was spent",
        fl_core::as_clause(e)
    )
}

/// Publish the attempt after its local append (GitHub ledger spec §2.2) and
/// report what stayed local. A failure refuses nothing (§7): it comes back
/// as the warning to print, and the attempt goes out with the next flush
/// that succeeds (decision 8).
fn conclude(ledger: &dyn Ledger, id: &Iri, attempt: &Attempt) -> Option<String> {
    match ledger.flush(fl_exec::decision::for_attempt(id, attempt)) {
        Ok(flushed) => {
            crate::ctx::report_flush(&flushed);
            None
        }
        Err(e) => {
            let after = if e.is_transient() {
                "Nothing is lost: the next decision that reaches the ledger publishes it."
            } else {
                "It stays there until that cause is fixed; the first decision that reaches the \
                 ledger afterwards publishes it."
            };
            Some(format!(
                "the attempt ran and is recorded in the local store, but it could not be \
                 published to the shared ledger: {}. {after}",
                fl_core::as_clause(&e)
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Flushes;
    use fl_core::decision::Outcome;
    use fl_core::ids::{ProjectId, seq_iri};

    fn attempt(status: AttemptStatus) -> Attempt {
        Attempt {
            id: Some(seq_iri(50)),
            at: None,
            project: ProjectId(seq_iri(1)),
            record: RecordId(seq_iri(2)),
            adapter: "claude".into(),
            status,
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
        assert_eq!(
            conclude(&ledger, &seq_iri(50), &attempt(AttemptStatus::Timeout)),
            None
        );
        let decisions = ledger.decisions.borrow();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].record, RecordId(seq_iri(2)));
        assert_eq!(decisions[0].rests_on, vec![seq_iri(50)]);
        assert_eq!(
            decisions[0].outcome,
            Outcome::Attempt {
                status: AttemptStatus::Timeout
            }
        );
    }

    // ⚠ Decision 14: an attempt that ran but could not be published exits
    // with the attempt's own code — never 2, which a script retries on —
    // and the publish failure is a warning.
    #[test]
    fn an_attempt_that_cannot_be_published_keeps_its_own_exit_code() {
        for (status, code) in [(AttemptStatus::Completed, 0), (AttemptStatus::Timeout, 1)] {
            assert_eq!(
                settle(&Flushes::refusing(), &seq_iri(50), &attempt(status)).0,
                code,
                "{status:?}"
            );
        }
    }

    // ⚠ Decision 15: an attempt whose local save fails keeps its own exit
    // code too, prints its outcome first and the failed save after it as
    // an error, and is not flushed — a decision cannot rest on an attempt
    // the local store does not hold.
    #[test]
    fn an_attempt_that_cannot_be_saved_keeps_its_own_exit_code_and_is_not_published() {
        for (status, code) in [(AttemptStatus::Completed, 0), (AttemptStatus::Timeout, 1)] {
            let ledger = Flushes::failing_append();
            let (got, said) = settle(&ledger, &seq_iri(50), &attempt(status));
            assert_eq!(got, code, "{status:?}");
            assert_eq!(
                said.first(),
                Some(&Said::Stdout(format!("{}\t1ms", status.as_wire()))),
                "the outcome comes first: {said:?}"
            );
            assert!(
                matches!(said.last(), Some(Said::Stderr(l)) if l.starts_with("error: the attempt ran")),
                "the failed save comes last, as an error: {said:?}"
            );
            assert!(
                ledger.decisions.borrow().is_empty(),
                "{status:?}: nothing flushed"
            );
        }
    }

    #[test]
    fn a_failed_save_says_the_attempt_ran_and_names_its_cause_once() {
        let msg = unsaved(&StoreError::Backend("the disk is full.".into()));
        assert!(
            msg.starts_with(
                "the attempt ran, but it could not be saved to the local store: backend \
                 failure: the disk is full. It is recorded nowhere"
            ),
            "{msg}"
        );
    }

    // Spec §7 and decision 8: kept, not refused; and the next decision
    // publishes it only when the failure was transient.
    #[test]
    fn the_warning_promises_a_later_publish_only_when_one_can_work() {
        let w = conclude(
            &Flushes::refusing(),
            &seq_iri(50),
            &attempt(AttemptStatus::Timeout),
        )
        .expect("a warning");
        assert!(w.contains("recorded in the local store"), "{w}");
        assert!(w.contains("next decision"), "{w}");
        assert!(!w.contains("refused:"), "not a refusal: {w}");

        let w = conclude(
            &Flushes::refusing_with(|| StoreError::Tampered {
                id: seq_iri(99),
                detail: "edited".into(),
            }),
            &seq_iri(50),
            &attempt(AttemptStatus::Timeout),
        )
        .expect("a warning");
        assert!(w.contains("recorded in the local store"), "{w}");
        assert!(!w.contains("next decision"), "{w}");
    }

    #[test]
    fn a_publish_warning_has_one_period_after_its_cause_and_no_above() {
        let w = conclude(
            &Flushes::refusing_with(|| StoreError::RestsOnLocalEntry {
                decision: seq_iri(1),
                entry: seq_iri(50),
            }),
            &seq_iri(50),
            &attempt(AttemptStatus::Timeout),
        )
        .expect("a warning");
        assert!(!w.contains(".."), "{w}");
        assert!(!w.contains(".)"), "{w}");
        assert!(!w.contains("above"), "{w}");
    }
}
