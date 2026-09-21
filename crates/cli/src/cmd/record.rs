use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::model::State;
use fl_core::store::Store;
use fl_exec::evaluate::evaluate_transition;

#[derive(Subcommand)]
pub enum Cmd {
    Add {
        #[arg(long)]
        project: u64,
        #[arg(long)]
        title: String,
    },
    List {
        #[arg(long)]
        project: u64,
    },
    Move {
        id: u64,
        #[arg(long = "to")]
        to: String,
    },
}

pub fn run(store: &mut impl Store, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Add { project, title } => {
            let p = ProjectId(project);
            if store.get_project(p)?.is_none() {
                bail!(
                    "no project with id {project}. Run `flctl project list` to see the ids that exist."
                );
            }
            let id = store.add_record(p, &title)?;
            println!("{id}\t{title}");
        }
        Cmd::List { project } => {
            for r in store.list_records(ProjectId(project))? {
                println!("{}\t{}\t{}", r.id, r.state.as_wire(), r.title);
            }
        }
        Cmd::Move { id, to } => {
            let Some(state) = State::from_wire(&to) else {
                bail!(
                    "`{to}` is not a state. Valid states are: {}.",
                    State::wire_values()
                );
            };
            let r = RecordId(id);
            let Some(record) = store.get_record(r)? else {
                bail!(
                    "no record with id {id}. Use `flctl record list --project <id>` to see records that exist."
                );
            };

            // ⚠ This used to be `set_record_state` and nothing else. `check`
            // would refuse the transition and `record move` would perform the
            // very state change those gates exist to protect — reading
            // nothing, running nothing, exiting 0. A gate that the guarded
            // action does not consult is decoration.
            //
            // A transition is addressed by name; a move is addressed by the
            // pair it performs. So the move asks which declarations cover
            // (from, to) and runs every one of them.
            let declared: Vec<_> = store
                .list_transitions(record.project)?
                .into_iter()
                .filter(|t| t.from == record.state && t.to == state)
                .collect();

            if declared.is_empty() {
                // Nothing declared this move, so there is nothing to bypass.
                // Say so rather than printing the same line a gated move
                // prints: "allowed" and "not checked" must not look alike.
                store.set_record_state(r, state)?;
                println!(
                    "{id}\t{}\tungated: project {} declares no transition from `{}` to `{}`",
                    state.as_wire(),
                    record.project,
                    record.state.as_wire(),
                    state.as_wire()
                );
                return Ok(0);
            }

            let mut worst = 0;
            for t in &declared {
                let report = evaluate_transition(store, record.project, &t.name, Some(r))
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                for g in &report.gates {
                    let (label, detail) = g.verdict.describe();
                    println!(
                        "{label}\t{}\t{}\t{detail}\t{}ms{}",
                        t.name,
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
                let code = if report.gates.is_empty() {
                    println!(
                        "FAIL\t{}\tthe transition declares no gates, so nothing was verified",
                        t.name
                    );
                    1
                } else {
                    report.exit_code()
                };
                worst = worst.max(code);
            }

            if worst != 0 {
                println!("REFUSED\t{id}\tstays `{}`", record.state.as_wire());
                return Ok(worst);
            }

            store.set_record_state(r, state)?;
            println!("{id}\t{}", state.as_wire());
        }
    }
    Ok(0)
}
