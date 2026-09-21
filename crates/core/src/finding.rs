use crate::ids::{FindingId, GateId, ProjectId, RecordId};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FindingError {
    #[error(
        "this finding has no reproduction, so it cannot be assigned. \
         Attach a check that fails because of the defect, or withdraw the finding."
    )]
    NoReproduction,
    #[error("this finding already has a reproduction and it cannot be swapped")]
    AlreadyReproduced,
    #[error(
        "a finding in state `{}` cannot make that move. {}",
        .0.as_wire(),
        if .0.is_terminal() {
            "That state is terminal: raise a new finding instead."
        } else {
            "Check where the finding actually is with `finding list`, and take the step that \
             state allows: a raised finding needs a reproduction, a reproduced one needs \
             assigning, and an assigned one is closed by verifying the repair."
        }
    )]
    WrongState(FindingState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingState {
    Raised,
    Reproduced,
    Assigned,
    Fixed,
    Withdrawn,
}

crate::wire::wire_names!(FindingState as finding_state_wire {
    Raised => "raised",
    Reproduced => "reproduced",
    Assigned => "assigned",
    Fixed => "fixed",
    Withdrawn => "withdrawn",
});

impl FindingState {
    pub fn is_terminal(self) -> bool {
        matches!(self, FindingState::Fixed | FindingState::Withdrawn)
    }
}

/// A claim that something is wrong.
///
/// ⚠⚠ There is deliberately NO edge from `Raised` to `Assigned`. That single
/// missing transition is decision 27 expressed in code: a judgement with no
/// reproduction has nowhere to go except concrete, or gone. There is also no
/// acknowledgement state, because agreement carried no information and the
/// step is removed rather than repaired (decision 25).
///
/// Re-assignment from one agent to another is deliberate (Decision 14): when a
/// fixer is escalated, the reproduction remains valid; re-assigning avoids the
/// cost of detach-and-reattach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: FindingId,
    pub project: ProjectId,
    pub record: RecordId,
    pub raised_by: String,
    pub claim: String,
    pub reproduction: Option<GateId>,
    pub state: FindingState,
    pub assigned_to: Option<String>,
    pub withdrawn_reason: Option<String>,
}

impl Finding {
    /// The id is a placeholder until the store assigns one.
    pub fn raise(project: ProjectId, record: RecordId, raised_by: &str, claim: &str) -> Self {
        Self {
            id: FindingId(0),
            project,
            record,
            raised_by: raised_by.to_string(),
            claim: claim.to_string(),
            reproduction: None,
            state: FindingState::Raised,
            assigned_to: None,
            withdrawn_reason: None,
        }
    }

    pub fn attach_reproduction(&mut self, gate: GateId) -> Result<(), FindingError> {
        if self.state.is_terminal() {
            return Err(FindingError::WrongState(self.state));
        }
        if self.reproduction.is_some() {
            return Err(FindingError::AlreadyReproduced);
        }
        self.reproduction = Some(gate);
        self.state = FindingState::Reproduced;
        Ok(())
    }

    pub fn assign(&mut self, to: &str) -> Result<(), FindingError> {
        match self.state {
            FindingState::Raised => Err(FindingError::NoReproduction),
            FindingState::Reproduced | FindingState::Assigned => {
                self.assigned_to = Some(to.to_string());
                self.state = FindingState::Assigned;
                Ok(())
            }
            other => Err(FindingError::WrongState(other)),
        }
    }

    pub fn mark_fixed(&mut self) -> Result<(), FindingError> {
        if self.state != FindingState::Assigned {
            return Err(FindingError::WrongState(self.state));
        }
        self.state = FindingState::Fixed;
        Ok(())
    }

    pub fn withdraw(&mut self, reason: &str) -> Result<(), FindingError> {
        if self.state.is_terminal() {
            return Err(FindingError::WrongState(self.state));
        }
        self.withdrawn_reason = Some(reason.to_string());
        self.state = FindingState::Withdrawn;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raised() -> Finding {
        Finding::raise(
            ProjectId(1),
            RecordId(2),
            "reviewer",
            "off-by-one on an empty slice",
        )
    }

    #[test]
    fn a_new_finding_is_raised_and_carries_no_reproduction() {
        let f = raised();
        assert_eq!(f.state, FindingState::Raised);
        assert!(f.reproduction.is_none());
    }

    // ⚠⚠ The single missing edge. Decision 27 in one assertion.
    #[test]
    fn a_raised_finding_cannot_be_assigned() {
        let mut f = raised();
        let err = f.assign("fixer").unwrap_err();
        assert!(matches!(err, FindingError::NoReproduction));
        assert_eq!(
            f.state,
            FindingState::Raised,
            "the failed assign must not move it"
        );
    }

    #[test]
    fn attaching_a_reproduction_makes_it_assignable() {
        let mut f = raised();
        f.attach_reproduction(GateId(9)).unwrap();
        assert_eq!(f.state, FindingState::Reproduced);
        assert_eq!(f.reproduction, Some(GateId(9)));
        f.assign("fixer").unwrap();
        assert_eq!(f.state, FindingState::Assigned);
    }

    #[test]
    fn a_reproduction_cannot_be_swapped_once_attached() {
        let mut f = raised();
        f.attach_reproduction(GateId(9)).unwrap();
        let err = f.attach_reproduction(GateId(10)).unwrap_err();
        assert!(matches!(err, FindingError::AlreadyReproduced));
        assert_eq!(f.reproduction, Some(GateId(9)));
    }

    #[test]
    fn a_finding_cannot_be_fixed_before_it_is_assigned() {
        let mut f = raised();
        assert!(f.mark_fixed().is_err());
        f.attach_reproduction(GateId(9)).unwrap();
        assert!(f.mark_fixed().is_err(), "Reproduced is not Assigned");
        f.assign("fixer").unwrap();
        f.mark_fixed().unwrap();
        assert_eq!(f.state, FindingState::Fixed);
    }

    #[test]
    fn a_raised_finding_can_be_withdrawn_and_remembers_who_raised_it() {
        let mut f = raised();
        f.withdraw("cannot be made concrete").unwrap();
        assert_eq!(f.state, FindingState::Withdrawn);
        assert_eq!(f.raised_by, "reviewer");
        assert_eq!(
            f.withdrawn_reason.as_deref(),
            Some("cannot be made concrete")
        );
    }

    #[test]
    fn a_terminal_finding_stays_terminal() {
        let mut f = raised();
        f.withdraw("no").unwrap();
        assert!(f.attach_reproduction(GateId(9)).is_err());
        assert!(f.assign("fixer").is_err());
    }

    #[test]
    fn a_finding_can_be_reassigned_to_a_different_actor() {
        let mut f = raised();
        f.attach_reproduction(GateId(9)).unwrap();
        f.assign("fixer_one").unwrap();
        assert_eq!(f.assigned_to.as_deref(), Some("fixer_one"));
        assert_eq!(f.state, FindingState::Assigned);
        f.assign("fixer_two").unwrap();
        assert_eq!(f.assigned_to.as_deref(), Some("fixer_two"));
        assert_eq!(f.state, FindingState::Assigned);
        assert_eq!(f.reproduction, Some(GateId(9)), "reproduction stays intact");
    }

    #[test]
    fn a_wrong_state_refusal_names_a_remedy_in_both_branches() {
        // ⚠ The non-terminal branch used to be the empty string, so the
        // refusal named a cause and no action. It is not reachable today,
        // which is exactly why it needs a test rather than a reader.
        for st in FindingState::ALL {
            let msg = FindingError::WrongState(*st).to_string();
            assert!(msg.contains(st.as_wire()), "does not name the state: {msg}");
            assert!(
                msg.contains("raise a new finding") || msg.contains("finding list"),
                "state `{}` gets a refusal with no remedy: {msg}",
                st.as_wire()
            );
        }
    }
}
