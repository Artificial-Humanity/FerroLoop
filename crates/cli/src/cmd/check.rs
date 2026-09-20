use anyhow::Result;
use clap::Args;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::stale::Staleness;
use fl_core::store::Store;
use fl_core::verdict::Verdict;
use fl_exec::evaluate::evaluate_transition;

#[derive(Args)]
pub struct Cmd {
    /// The transition to evaluate.
    pub transition: String,
    #[arg(long)]
    pub project: u64,
    #[arg(long)]
    pub record: Option<u64>,
}

pub fn run(store: &mut impl Store, cmd: Cmd) -> Result<i32> {
    let report = evaluate_transition(
        store,
        ProjectId(cmd.project),
        &cmd.transition,
        cmd.record.map(RecordId),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    for g in &report.gates {
        let (label, detail) = match &g.verdict {
            Verdict::Pass { population, .. } => {
                ("PASS".to_string(), format!("{} examined", population.get()))
            }
            Verdict::Fail { population, reason, .. } => (
                "FAIL".to_string(),
                format!("{reason:?}, {population} examined"),
            ),
            Verdict::Error { detail, .. } => ("ERROR".to_string(), detail.clone()),
        };
        let note = match g.staleness {
            Staleness::Fresh => "",
            Staleness::StaleWarn => "  (stale: the gate's population moved since it was stamped)",
            Staleness::StaleFail => "  (stale)",
        };
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
