use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::{GateId, ProjectId};
use fl_core::model::{Regret, State, Transition};
use fl_core::store::Store;

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
                    "no project with id {project}. Run `fl project list` to see the ids that exist."
                );
            }
            let Some(from_state) = State::from_wire(&from) else {
                bail!(
                    "`{from}` is not a state. Valid states are: {}.",
                    State::wire_values()
                );
            };
            let Some(to_state) = State::from_wire(&to) else {
                bail!(
                    "`{to}` is not a state. Valid states are: {}.",
                    State::wire_values()
                );
            };
            let Some(regret_val) = Regret::from_wire(&regret) else {
                bail!(
                    "`{regret}` is not a regret level. Valid values are: {}.",
                    Regret::wire_values()
                );
            };
            for g in &gate {
                let Some(def) = store.get_gate(GateId(*g))? else {
                    bail!(
                        "transition `{name}` names gate {g}, which does not exist. \
                         Run `fl gate list --project {project}` to see the ids that exist."
                    );
                };
                // ⚠ Existence used to be the whole check. A gate belonging to
                // another project would have been resolved against THIS
                // project's working tree, enumerating a population from the
                // wrong repository — and passing, since a glob that matches
                // nothing here is just a small population somewhere else.
                if def.project != p {
                    bail!(
                        "transition `{name}` is in project {project}, but gate {g} (`{}`) \
                         belongs to project {}. A gate is resolved against its own project's \
                         working tree, so wiring one across projects would examine the wrong \
                         tree. Declare the gate in project {project} instead.",
                        def.name,
                        def.project
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
                     Add it with `fl transition add`, or name one of the existing ones."
                );
            };
            println!("{}", serde_json::to_string_pretty(&t)?);
        }
    }
    Ok(0)
}
