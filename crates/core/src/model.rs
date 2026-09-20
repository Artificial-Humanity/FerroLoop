use crate::ids::{GateId, ProjectId, RecordId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Todo,
    Doing,
    Review,
    Done,
    NeedsHuman,
}

impl State {
    pub fn as_wire(self) -> &'static str {
        match self {
            State::Todo => "todo",
            State::Doing => "doing",
            State::Review => "review",
            State::Done => "done",
            State::NeedsHuman => "needs_human",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "todo" => State::Todo,
            "doing" => State::Doing,
            "review" => State::Review,
            "done" => State::Done,
            "needs_human" => State::NeedsHuman,
            _ => return None,
        })
    }
}

/// How bad it is if this transition proceeds on a false pass.
///
/// Declared by the project. Telemetry audits the declaration but never
/// changes it — irreversibility is half of regret and no measurement sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Regret {
    #[default]
    Low,
    High,
}

/// How a gate names the things it must examine. Resolved fresh at run time
/// and never persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Selector {
    Glob { pattern: String },
    Changed { base: String },
    Command { program: String, args: Vec<String> },
}

/// How the resolved population reaches a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_round_trips_through_its_wire_name() {
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
            id: GateId(1),
            project: ProjectId(1),
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
            project: ProjectId(1),
            name: "launch".into(),
            from: State::Review,
            to: State::Done,
            regret: Regret::High,
            gates: vec![GateId(1), GateId(2)],
        };
        assert_eq!(t.regret, Regret::High);
        assert_eq!(t.gates.len(), 2);
    }
}
