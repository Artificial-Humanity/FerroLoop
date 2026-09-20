use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::{GateId, ProjectId};
use fl_core::model::{Regret, State, Transition};
use fl_core::store::Store;

const STATES: &str = "todo, doing, review, done, needs_human";
const REGRETS: &str = "low, high";

#[derive(Subcommand)]
pub enum Cmd {
    Add {
        #[arg(long)]
        project: u64,
        #[arg(long)]
        name: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        /// `low` or `high`.
        #[arg(long)]
        regret: String,
        #[arg(long = "gate", num_args = 0..)]
        gate: Vec<u64>,
    },
    Show {
        #[arg(long)]
        project: u64,
        #[arg(long)]
        name: String,
    },
}

pub fn run(store: &mut impl Store, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Add {
            project,
            name,
            from,
            to,
            regret,
            gate,
        } => {
            let p = ProjectId(project);
            if store.get_project(p)?.is_none() {
                bail!(
                    "no project with id {project}. Run `flctl project list` to see the ids that exist."
                );
            }
            let Some(from_state) = State::from_wire(&from) else {
                bail!("`{from}` is not a state. Valid states are: {STATES}.");
            };
            let Some(to_state) = State::from_wire(&to) else {
                bail!("`{to}` is not a state. Valid states are: {STATES}.");
            };
            let regret_val = match regret.as_str() {
                "low" => Regret::Low,
                "high" => Regret::High,
                _ => bail!("`{regret}` is not a regret level. Valid values are: {REGRETS}."),
            };
            for g in &gate {
                if store.get_gate(GateId(*g))?.is_none() {
                    bail!(
                        "transition `{name}` names gate {g}, which does not exist. \
                         Run `flctl gate list --project {project}` to see the ids that exist."
                    );
                }
            }
            let gates = gate.into_iter().map(GateId).collect();
            store.add_transition(Transition {
                project: p,
                name: name.clone(),
                from: from_state,
                to: to_state,
                regret: regret_val,
                gates,
            })?;
            println!("{name}");
        }
        Cmd::Show { project, name } => {
            let p = ProjectId(project);
            let Some(t) = store.get_transition(p, &name)? else {
                bail!(
                    "project {project} declares no transition named `{name}`. \
                     Add it with `flctl transition add`, or name one of the existing ones."
                );
            };
            println!("{}", serde_json::to_string_pretty(&t)?);
        }
    }
    Ok(0)
}
