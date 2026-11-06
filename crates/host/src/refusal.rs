//! Typed containment refusals. These are not numeric-PID fallbacks.

use fl_host_proto::HostError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentRefusal {
    PidfdUnavailable,
    IdentityUnproven,
    CorruptRegistry,
    EpochExhausted,
    CleanupUnresolved,
    UnanchoredGroup,
    SpawnRejected,
}

impl ContainmentRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PidfdUnavailable => "pidfd unavailable",
            Self::IdentityUnproven => "unproven ownership",
            Self::CorruptRegistry => "corrupt registry",
            Self::EpochExhausted => "epoch exhausted",
            Self::CleanupUnresolved => "cleanup unresolved",
            Self::UnanchoredGroup => "unanchored group",
            Self::SpawnRejected => "spawn rejected",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "pidfd unavailable" => Self::PidfdUnavailable,
            "unproven ownership" => Self::IdentityUnproven,
            "corrupt registry" => Self::CorruptRegistry,
            "epoch exhausted" => Self::EpochExhausted,
            "cleanup unresolved" => Self::CleanupUnresolved,
            "unanchored group" => Self::UnanchoredGroup,
            "spawn rejected" => Self::SpawnRejected,
            _ => return None,
        })
    }
}

impl From<ContainmentRefusal> for HostError {
    fn from(refusal: ContainmentRefusal) -> Self {
        HostError::Protocol(refusal.as_str().into())
    }
}

pub fn refusal(err: &HostError) -> Option<ContainmentRefusal> {
    match err {
        HostError::Protocol(text) => ContainmentRefusal::parse(text),
        _ => None,
    }
}
