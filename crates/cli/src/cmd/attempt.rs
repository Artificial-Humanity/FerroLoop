use anyhow::{Result, bail};
use clap::Args;
use fl_core::ids::RecordId;
use fl_core::log::{Attempt, AttemptStatus};
use fl_core::store::Store;
use fl_exec::adapters::ClaudeAdapter;
use fl_exec::runner::{AttemptSpec, Runner};

const KNOWN_ADAPTERS: &str = "claude";

#[derive(Args)]
pub struct Cmd {
    pub record: u64,
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

pub fn run(store: &mut impl Store, cmd: Cmd) -> Result<i32> {
    if cmd.adapter != "claude" {
        bail!(
            "`{}` is not a known adapter. Milestone 1 ships: {KNOWN_ADAPTERS}.",
            cmd.adapter
        );
    }
    let id = RecordId(cmd.record);
    let Some(record) = store.get_record(id)? else {
        bail!(
            "no record with id {}. Run `flctl record list` to see the ids that exist.",
            cmd.record
        );
    };
    let Some(project) = store.get_project(record.project)? else {
        bail!(
            "record {} belongs to project {}, which no longer exists.",
            cmd.record,
            record.project
        );
    };

    let adapter = ClaudeAdapter::new(cmd.binary);
    let spec = AttemptSpec {
        project_root: std::path::PathBuf::from(&project.root),
        record: id,
        instruction: cmd.instruction.unwrap_or_else(|| record.title.clone()),
        timeout_secs: cmd.timeout_secs,
        budget_usd_micros: cmd.budget_usd_micros,
    };

    let rt = tokio::runtime::Runtime::new()?;
    let outcome = rt.block_on(adapter.attempt(spec)).map_err(|e| anyhow::anyhow!("{e}"))?;

    // ⚠ Recorded whatever the outcome. A crash, a timeout and a refusal all
    // cost something, even when that something is only the wall clock.
    store.append_attempt(Attempt {
        project: record.project,
        record: id,
        adapter: "claude".into(),
        status: outcome.status,
        duration_ms: outcome.duration_ms,
        tokens_in: outcome.tokens_in,
        tokens_out: outcome.tokens_out,
        cost_usd_micros: outcome.cost_usd_micros,
        paths_touched: outcome.paths_touched.clone(),
        output_excerpt: outcome.output_excerpt.clone(),
    })?;

    println!("{:?}\t{}ms", outcome.status, outcome.duration_ms);
    if !outcome.output_excerpt.is_empty() {
        for line in outcome.output_excerpt.lines().take(40) {
            println!("\t| {line}");
        }
    }

    Ok(match outcome.status {
        AttemptStatus::Completed => 0,
        _ => 1,
    })
}
