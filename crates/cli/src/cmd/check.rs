use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Args;
use fl_core::decision::Flushed;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::store::Ledger;
use fl_core::{Iri, Kind};
use fl_exec::evaluate::{TransitionReport, evaluate_transition};

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
    /// Every item this command names, by `Ref` — the single source `iris()`
    /// and `has_handle()` both derive from, so a `Ref` field added here is
    /// picked up by both at once. The transition name
    /// is not an id.
    fn refs(&self) -> Vec<&Ref> {
        let mut out: Vec<&Ref> = vec![&self.project];
        if let Some(r) = &self.record {
            out.push(r);
        }
        out
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
    let project = ProjectId(refs::resolve(
        ctx.handles,
        store.label(),
        Kind::Project,
        &cmd.project,
    )?);
    let record = match &cmd.record {
        Some(r) => {
            let id = RecordId(refs::resolve(
                ctx.handles,
                &ctx.tracker_label,
                Kind::Record,
                r,
            )?);
            // The record's PRIMARY id, never the alias the person typed:
            // every run tied to one record must name it the same way.
            let Some(rec) = ctx.tracker.get_record(&id)? else {
                bail!(
                    "`{r}` is not a record in the store at {}. Use \
                     `fl record list --project <project>` to see records that exist.",
                    ctx.tracker_label
                );
            };
            Some(rec.id)
        }
        None => None,
    };
    crate::cmd::manifest::ensure_import_current(store, &project)?;
    let roles = ctx.roles();
    let report = evaluate_transition(
        roles.catalog,
        roles.ledger,
        &project,
        &cmd.transition,
        record.as_ref(),
    )
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

    let code = if report.gates.is_empty() {
        println!(
            "FAIL\t{}\tthe transition declares no gates, so nothing was verified",
            report.transition
        );
        1
    } else {
        report.exit_code()
    };

    let flushed = publish(roles.ledger, record.as_ref(), &report)?;
    crate::ctx::report_flush(&flushed);
    Ok(code)
}

/// `check --record` is a decision (GitHub ledger spec §2.2): flushed at the
/// end, whatever the verdict. A plain `check` names no record, decides
/// nothing, and stays local.
fn publish(
    ledger: &dyn Ledger,
    record: Option<&RecordId>,
    report: &TransitionReport,
) -> Result<Flushed> {
    let Some(record) = record else {
        return Ok(Flushed::NOTHING);
    };
    ledger
        .flush(fl_exec::decision::for_check(record, report))
        .map_err(|e| {
            // ⚠ Only a transient refusal promises a later publish (spec §7).
            let after = if e.is_transient() {
                "Its runs are kept in the local store, and the next decision that reaches the \
                 ledger publishes them."
            } else {
                "Its runs are kept in the local store. Fix what is named above before deciding \
                 again."
            };
            anyhow::anyhow!(
                "refused: the check ran, but its decision could not be published to the shared \
                 ledger ({e}). {after}"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Flushes;
    use fl_core::decision::{Outcome, TransitionOutcome};
    use fl_core::ids::{GateId, seq_iri};
    use fl_core::model::Regret;
    use fl_core::stale::Staleness;
    use fl_core::verdict::Verdict;
    use fl_exec::evaluate::GateReport;

    fn report(pass: bool) -> TransitionReport {
        TransitionReport {
            transition: "launch".into(),
            regret: Regret::Low,
            gates: vec![GateReport {
                gate: GateId(seq_iri(1)),
                name: "g".into(),
                verdict: Verdict::from_predicate(pass, 1),
                staleness: Staleness::Fresh,
                output_excerpt: String::new(),
                duration_ms: 1,
                run: seq_iri(7),
            }],
        }
    }

    // Spec §2.1: a plain `check` stays local and needs no network.
    #[test]
    fn a_plain_check_decides_nothing_and_flushes_nothing() {
        let ledger = Flushes::refusing();
        let flushed = publish(&ledger, None, &report(true)).expect("no record, no flush, no error");
        assert_eq!(flushed, fl_core::decision::Flushed::NOTHING);
    }

    #[test]
    fn a_checks_decision_names_the_record_the_transition_and_its_runs() {
        let ledger = Flushes::default();
        let r = RecordId(seq_iri(3));
        publish(&ledger, Some(&r), &report(true)).unwrap();
        let decisions = ledger.decisions.borrow();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].record, r);
        assert_eq!(
            decisions[0].outcome,
            Outcome::Check {
                transition: TransitionOutcome {
                    transition: "launch".into(),
                    passed: true
                }
            }
        );
        assert_eq!(decisions[0].rests_on, vec![seq_iri(7)]);
    }

    // Decision 11: a failed check is a decision too.
    #[test]
    fn a_failed_check_is_flushed_too() {
        let ledger = Flushes::default();
        publish(&ledger, Some(&RecordId(seq_iri(3))), &report(false)).unwrap();
        assert_eq!(ledger.decisions.borrow().len(), 1);
    }

    // Spec §7: unreachable at a check's flush refuses it; the runs stay.
    #[test]
    fn a_check_whose_decision_cannot_be_published_is_refused_and_says_the_runs_are_kept() {
        let err = publish(
            &Flushes::refusing(),
            Some(&RecordId(seq_iri(3))),
            &report(true),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("refused"), "{msg}");
        assert!(msg.contains("kept in the local store"), "{msg}");
    }

    // Spec §7: only a transient refusal promises that the next decision
    // publishes the runs.
    #[test]
    fn a_check_refused_for_a_cause_a_retry_cannot_cure_promises_no_retry() {
        let ledger = Flushes::refusing_with(|| fl_core::store::StoreError::Tampered {
            id: seq_iri(99),
            detail: "edited".into(),
        });
        let err = publish(&ledger, Some(&RecordId(seq_iri(3))), &report(true)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("refused"), "{msg}");
        assert!(msg.contains("kept in the local store"), "{msg}");
        assert!(!msg.contains("next decision"), "{msg}");
    }

    #[test]
    fn a_check_that_could_not_reach_github_promises_the_next_decision_publishes_it() {
        let err = publish(
            &Flushes::refusing(),
            Some(&RecordId(seq_iri(3))),
            &report(true),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("next decision"), "{err:#}");
    }
}
