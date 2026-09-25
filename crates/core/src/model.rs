use crate::ids::{GateId, ProjectId, RecordId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Todo,
    Doing,
    Review,
    Done,
    NeedsHuman,
}

crate::wire::wire_names!(State as state_wire {
    Todo => "todo",
    Doing => "doing",
    Review => "review",
    Done => "done",
    NeedsHuman => "needs_human",
});

// `transition add --from/--to` and `record move` take a state by name.
crate::wire::wire_parse!(State as state_parse);

/// How bad it is if this transition proceeds on a false pass.
///
/// Declared by the project. Telemetry audits the declaration but never
/// changes it — irreversibility is half of regret and no measurement sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Regret {
    #[default]
    Low,
    High,
}

crate::wire::wire_names!(Regret as regret_wire {
    Low => "low",
    High => "high",
});

// `transition add --regret` takes a regret by name.
crate::wire::wire_parse!(Regret as regret_parse);

/// How a gate names the things it must examine. Resolved fresh at run time
/// and never persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selector {
    Glob { pattern: String },
    Changed { base: String },
    Command { program: String, args: Vec<String> },
}

/// How the resolved population reaches a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PopulationDelivery {
    Args,
    Stdin,
    FileList,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub delivery: PopulationDelivery,
    pub timeout_secs: u64,
    pub pass_codes: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub adapter: String,
    pub question: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    Command(CommandSpec),
    Agent(AgentSpec),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateDef {
    pub id: GateId,
    pub project: ProjectId,
    pub name: String,
    pub kind: GateKind,
    pub selector: Selector,
    pub min_population: u64,
    pub authored_at_commit: String,
    pub authored_by: String,
    pub last_pass_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub project: ProjectId,
    pub name: String,
    pub from: State,
    pub to: State,
    pub regret: Regret,
    pub gates: Vec<GateId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: RecordId,
    pub project: ProjectId,
    pub title: String,
    pub state: State,
}

crate::wire::wire_tags!(Selector as selector_wire {
    Selector::Glob { .. } => "glob", Selector::Glob { pattern: "*.rs".into() };
    Selector::Changed { .. } => "changed", Selector::Changed { base: "main".into() };
    Selector::Command { .. } => "command", Selector::Command {
        program: "ls".into(),
        args: Vec::new(),
    };
});

crate::wire::wire_tags!(PopulationDelivery as population_delivery_wire {
    PopulationDelivery::Args => "args", PopulationDelivery::Args;
    PopulationDelivery::Stdin => "stdin", PopulationDelivery::Stdin;
    PopulationDelivery::FileList => "file_list", PopulationDelivery::FileList;
});

crate::wire::wire_tags!(GateKind as gate_kind_wire {
    GateKind::Command(_) => "command", GateKind::Command(CommandSpec {
        program: "true".into(),
        args: Vec::new(),
        delivery: PopulationDelivery::Args,
        timeout_secs: 1,
        pass_codes: vec![0],
    });
    GateKind::Agent(_) => "agent", GateKind::Agent(AgentSpec {
        adapter: "a".into(),
        question: "q".into(),
        timeout_secs: 1,
    });
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::seq_iri;

    #[test]
    fn a_state_round_trips_through_its_wire_name() {
        assert_eq!(State::from_wire("todo"), Some(State::Todo));
        assert_eq!(State::Todo.as_wire(), "todo");
        assert_eq!(State::from_wire("doing"), Some(State::Doing));
        assert_eq!(State::Doing.as_wire(), "doing");
        assert_eq!(State::from_wire("review"), Some(State::Review));
        assert_eq!(State::Review.as_wire(), "review");
        assert_eq!(State::from_wire("done"), Some(State::Done));
        assert_eq!(State::Done.as_wire(), "done");
        assert_eq!(State::from_wire("needs_human"), Some(State::NeedsHuman));
        assert_eq!(State::NeedsHuman.as_wire(), "needs_human");
        assert_eq!(State::from_wire("nonsense"), None);
    }

    #[test]
    fn regret_defaults_to_low_so_high_is_always_a_deliberate_act() {
        assert_eq!(Regret::default(), Regret::Low);
    }

    #[test]
    fn a_gate_definition_carries_its_provenance_and_its_floor() {
        let g = GateDef {
            id: GateId(seq_iri(1)),
            project: ProjectId(seq_iri(1)),
            name: "fmt".into(),
            kind: GateKind::Command(CommandSpec {
                program: "cargo".into(),
                args: vec!["fmt".into(), "--check".into()],
                delivery: PopulationDelivery::Args,
                timeout_secs: 60,
                pass_codes: vec![0],
            }),
            selector: Selector::Glob {
                pattern: "src/**/*.rs".into(),
            },
            min_population: 1,
            authored_at_commit: "abc1234".into(),
            authored_by: "owner".into(),
            last_pass_commit: None,
        };
        assert_eq!(g.min_population, 1);
        assert!(g.last_pass_commit.is_none());
    }

    #[test]
    fn a_transition_names_its_gates_and_declares_its_regret() {
        let t = Transition {
            project: ProjectId(seq_iri(1)),
            name: "launch".into(),
            from: State::Review,
            to: State::Done,
            regret: Regret::High,
            gates: vec![GateId(seq_iri(1)), GateId(seq_iri(2))],
        };
        assert_eq!(t.regret, Regret::High);
        assert_eq!(t.gates.len(), 2);
    }

    #[test]
    fn the_remaining_wire_enums_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&Selector::Glob {
                pattern: "*.rs".into()
            })
            .expect("serialize"),
            r#"{"glob":{"pattern":"*.rs"}}"#
        );
        assert_eq!(
            serde_json::to_string(&PopulationDelivery::FileList).expect("serialize"),
            r#""file_list""#
        );
        assert_eq!(
            serde_json::to_string(&GateKind::Agent(AgentSpec {
                adapter: "claude".into(),
                question: "?".into(),
                timeout_secs: 1,
            }))
            .expect("serialize"),
            r#"{"agent":{"adapter":"claude","question":"?","timeout_secs":1}}"#
        );
    }
}
