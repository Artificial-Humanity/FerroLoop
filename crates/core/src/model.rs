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
#[serde(rename_all = "snake_case")]
pub enum Regret {
    #[default]
    Low,
    High,
}

impl Regret {
    pub fn as_wire(self) -> &'static str {
        match self {
            Regret::Low => "low",
            Regret::High => "high",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "low" => Regret::Low,
            "high" => Regret::High,
            _ => return None,
        })
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_states_serde_form_is_the_same_string_as_its_wire_name() {
        // The two spellings must not drift. `as_wire` is what the CLI accepts
        // and prints; the serde form is what lands in JSON and in the store.
        for st in [
            State::Todo,
            State::Doing,
            State::Review,
            State::Done,
            State::NeedsHuman,
        ] {
            assert_eq!(
                serde_json::to_string(&st).expect("serialize"),
                format!("\"{}\"", st.as_wire()),
                "{st:?} serializes to a different string than it prints"
            );
        }
    }

    #[test]
    fn a_regrets_serde_form_is_the_same_string_as_its_wire_name() {
        for r in [Regret::Low, Regret::High] {
            assert_eq!(
                serde_json::to_string(&r).expect("serialize"),
                format!("\"{}\"", r.as_wire())
            );
            assert_eq!(Regret::from_wire(r.as_wire()), Some(r));
        }
        assert_eq!(Regret::from_wire("Low"), None);
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
