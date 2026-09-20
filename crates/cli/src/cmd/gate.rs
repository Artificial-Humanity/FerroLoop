use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::{GateId, ProjectId};
use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
use fl_core::store::Store;
use fl_core::verdict::Verdict;
use fl_exec::evaluate::run_single_gate;

#[derive(Subcommand)]
pub enum Cmd {
    Add {
        #[arg(long)]
        project: u64,
        #[arg(long)]
        name: String,
        /// `command` or `agent`.
        #[arg(long, default_value = "command")]
        kind: String,
        #[arg(long)]
        glob: String,
        #[arg(long)]
        program: String,
        #[arg(long, num_args = 0..)]
        arg: Vec<String>,
        #[arg(long, default_value_t = 1)]
        min_population: u64,
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
        #[arg(long, default_value = "unknown")]
        authored_by: String,
    },
    List {
        #[arg(long)]
        project: u64,
    },
    Show {
        id: u64,
    },
    /// Re-stamp a gate against HEAD: "I looked, and it still holds."
    Affirm {
        id: u64,
        #[arg(long, default_value = "unknown")]
        by: String,
    },
    /// Run one gate against the live working tree and print its verdict.
    Run {
        id: u64,
    },
    /// TESTING AFFORDANCE: rewrite a command gate's program in place, with no
    /// re-authoring and no new commit stamp. This is how a test simulates a
    /// repair without a model in the loop — and, just as directly, how a
    /// person could weaken a reproduction by hand and have it read as a
    /// legitimate fix. Spec §12 records that as an open question; this
    /// command does not resolve it, only makes it possible.
    SetProgram {
        id: u64,
        #[arg(long)]
        program: String,
    },
}

pub fn run(store: &mut impl Store, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Add {
            project,
            name,
            kind,
            glob,
            program,
            arg,
            min_population,
            timeout_secs,
            authored_by,
        } => {
            if kind != "command" {
                bail!(
                    "`{kind}` is not a gate kind. Milestone 1 implements `command`. \
                     `agent` is defined but not yet built."
                );
            }
            let p = ProjectId(project);
            let Some(proj) = store.get_project(p)? else {
                bail!(
                    "no project with id {project}. Run `flctl project list` to see the ids that exist."
                );
            };
            let head = fl_exec::git::Git::head(std::path::Path::new(&proj.root)).map_err(|e| {
                anyhow::anyhow!(
                    "{e}. A project must be a git working tree, because a gate's provenance is a commit."
                )
            })?;
            let id = store.add_gate(
                p,
                &name,
                GateKind::Command(CommandSpec {
                    program,
                    args: arg,
                    delivery: PopulationDelivery::Args,
                    timeout_secs,
                    pass_codes: vec![0],
                }),
                Selector::Glob { pattern: glob },
                min_population,
                &head,
                &authored_by,
            )?;
            println!("{id}\t{name}\t{head}");
        }
        Cmd::List { project } => {
            for g in store.list_gates(ProjectId(project))? {
                println!("{}\t{}\t{}", g.id, g.name, g.authored_at_commit);
            }
        }
        Cmd::Show { id } => {
            let Some(g) = store.get_gate(GateId(id))? else {
                bail!(
                    "no gate with id {id}. Use `flctl gate list --project <id>` to see gates that exist."
                );
            };
            println!("{}", serde_json::to_string_pretty(&g)?);
        }
        Cmd::Affirm { id, by } => {
            let Some(mut g) = store.get_gate(GateId(id))? else {
                bail!(
                    "no gate with id {id}. Use `flctl gate list --project <id>` to see gates that exist."
                );
            };
            let Some(proj) = store.get_project(g.project)? else {
                bail!("gate {id} belongs to project {}, which no longer exists.", g.project);
            };
            let head = fl_exec::git::Git::head(std::path::Path::new(&proj.root))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            g.authored_at_commit = head.clone();
            g.authored_by = by;
            store.update_gate(&g)?;
            println!("{id}\t{head}");
        }
        Cmd::Run { id } => {
            let Some(g) = store.get_gate(GateId(id))? else {
                bail!(
                    "no gate with id {id}. Use `flctl gate list --project <id>` to see gates that exist."
                );
            };
            let report = run_single_gate(store, g.project, GateId(id))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let (label, detail) = match &report.verdict {
                Verdict::Pass { population, .. } => {
                    ("PASS".to_string(), format!("{} examined", population.get()))
                }
                Verdict::Fail { population, reason, .. } => (
                    "FAIL".to_string(),
                    format!("{reason:?}, {population} examined"),
                ),
                Verdict::Error { detail, .. } => ("ERROR".to_string(), detail.clone()),
            };
            println!("{label}\t{}\t{detail}", g.name);
            return Ok(report.verdict.exit_code());
        }
        Cmd::SetProgram { id, program } => {
            let Some(mut g) = store.get_gate(GateId(id))? else {
                bail!(
                    "no gate with id {id}. Use `flctl gate list --project <id>` to see gates that exist."
                );
            };
            let GateKind::Command(spec) = &mut g.kind else {
                bail!(
                    "gate {id} is not a command gate, so it has no `program` field to rewrite."
                );
            };
            spec.program = program;
            store.update_gate(&g)?;
            println!("{id}\t{}", g.name);
        }
    }
    Ok(0)
}
