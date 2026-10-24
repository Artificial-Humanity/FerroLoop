//! Agent and role registry domain types (spec §2, activating identity spec §6a).

use crate::at::At;
use crate::ids::AgentId;
use crate::iri::Iri;
use crate::state::{StateError, validate_slug, validate_summary};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaRecord {
    pub area_key: String,
    pub name: String,
    pub path: Option<String>,
    pub purpose: String,
    pub updated_at: At,
    pub by: String,
}

impl AreaRecord {
    pub fn new(
        name: &str,
        path: Option<String>,
        purpose: &str,
        updated_at: At,
        by: &str,
    ) -> Result<Self, StateError> {
        let area_key = validate_slug(name.trim())?;
        let purpose = validate_summary(purpose)?;
        Ok(Self {
            area_key,
            name: name.trim().to_string(),
            path,
            purpose,
            updated_at,
            by: by.trim().to_string(),
        })
    }

    pub fn normalized(&self) -> Result<Self, StateError> {
        let raw_key = if self.area_key.trim().is_empty() {
            self.name.trim()
        } else {
            self.area_key.trim()
        };
        let area_key = validate_slug(raw_key)?;
        let name = if self.name.trim().is_empty() {
            area_key.clone()
        } else {
            self.name.trim().to_string()
        };
        let purpose = validate_summary(&self.purpose)?;
        Ok(Self {
            area_key,
            name,
            path: self.path.clone(),
            purpose,
            updated_at: self.updated_at.clone(),
            by: self.by.trim().to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentRole {
    Lead,
    Assistant,
    LabManager,
    Custom(String),
}

impl AgentRole {
    pub fn from_wire(s: &str) -> Result<Self, StateError> {
        let lower = s.trim().to_ascii_lowercase();
        match lower.as_str() {
            "lead" => Ok(Self::Lead),
            "assistant" => Ok(Self::Assistant),
            "lab_manager" => Ok(Self::LabManager),
            other => {
                if let Some(slug) = other.strip_prefix("custom:") {
                    Ok(Self::Custom(validate_slug(slug)?))
                } else {
                    Err(StateError::InvalidRole(s.to_string()))
                }
            }
        }
    }

    pub fn as_wire(&self) -> String {
        match self {
            Self::Lead => "lead".to_string(),
            Self::Assistant => "assistant".to_string(),
            Self::LabManager => "lab_manager".to_string(),
            Self::Custom(s) => format!("custom:{s}"),
        }
    }
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_wire())
    }
}

impl FromStr for AgentRole {
    type Err = StateError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_wire(s)
    }
}

impl Serialize for AgentRole {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_wire())
    }
}

impl<'de> Deserialize<'de> for AgentRole {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::from_wire(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Active,
    Retired,
}

crate::wire::wire_names!(AgentStatus as agent_status_wire {
    Active => "active",
    Retired => "retired",
});
crate::wire::wire_parse!(AgentStatus as agent_status_parse);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRecord {
    pub id: AgentId,
    pub name_key: String,
    pub display_name: String,
    pub email: String,
    pub project: String,
    pub role: AgentRole,
    pub extra_areas: Vec<String>,
    pub status: AgentStatus,
    pub updated_at: At,
    pub by: String,
}

impl AgentRecord {
    pub fn co_author_trailer(&self) -> String {
        format!("Co-authored-by: {} <{}>", self.display_name, self.email)
    }

    pub fn covers_area(&self, slug: &str) -> bool {
        self.project == slug || self.extra_areas.iter().any(|a| a == slug)
    }
}

#[derive(Debug, Clone)]
pub struct RegisterAgentArgs {
    pub display_name: String,
    pub email: Option<String>,
    pub project: String,
    pub role: AgentRole,
    pub extra_areas: Vec<String>,
    pub create_area: bool,
    pub replace: bool,
    pub at: At,
    pub by: String,
}

/// Derive `reports_to` deterministically from registry state (spec §2.3):
/// - `Lead` or `LabManager` -> `"Owner"`
/// - `Assistant` or `Custom(_)` -> `display_name` of the active `Lead` for
///   `agent.project` (or `"Owner"` if none).
pub fn reports_to(agent: &AgentRecord, all_agents: &[AgentRecord]) -> String {
    match agent.role {
        AgentRole::Lead | AgentRole::LabManager => "Owner".to_string(),
        AgentRole::Assistant | AgentRole::Custom(_) => all_agents
            .iter()
            .find(|a| {
                a.status == AgentStatus::Active
                    && a.project == agent.project
                    && a.role == AgentRole::Lead
            })
            .map(|lead| lead.display_name.clone())
            .unwrap_or_else(|| "Owner".to_string()),
    }
}

/// Validate `display_name` and optional `email` per spec §2.2.
/// Returns `(name_key, display_name, email)`.
pub fn validate_agent_identity(
    display_name: &str,
    email: Option<&str>,
) -> Result<(String, String, String), String> {
    let display = display_name.trim();
    let name_key = validate_slug(display).map_err(|e| e.to_string())?;
    if display.to_ascii_lowercase() != name_key {
        return Err(format!(
            "display_name `{display}` must equal `{name_key}` when lowercased"
        ));
    }
    let resolved_email = match email {
        Some(raw) => {
            let trimmed = raw.trim();
            let Some((local, domain)) = trimmed.split_once('@') else {
                return Err(format!("invalid email `{trimmed}`: missing `@`"));
            };
            if local.is_empty()
                || domain.is_empty()
                || local.chars().any(|c| c.is_ascii_uppercase())
            {
                return Err(format!(
                    "email `{trimmed}` must have a non-empty lowercase local-part before `@`"
                ));
            }
            format!("{local}@{}", domain.to_ascii_lowercase())
        }
        None => format!("{name_key}@artificialhumanity.io"),
    };
    Ok((name_key, display.to_string(), resolved_email))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryEventKind {
    AreaUpsert(AreaRecord),
    AgentUpsert(AgentRecord),
    AgentRetire { id: AgentId },
}

crate::wire::wire_tags!(RegistryEventKind as registry_event_kind_wire {
    RegistryEventKind::AreaUpsert(_) => "area_upsert", RegistryEventKind::AreaUpsert(AreaRecord {
        area_key: "ferroloop".into(),
        name: "FerroLoop".into(),
        path: Some("FerroLoop/".into()),
        purpose: "Agent orchestration".into(),
        updated_at: At::from_unix_millis(1),
        by: "owner".into(),
    });
    RegistryEventKind::AgentUpsert(_) => "agent_upsert", RegistryEventKind::AgentUpsert(AgentRecord {
        id: AgentId(crate::ids::seq_iri(1)),
        name_key: "ferris".into(),
        display_name: "Ferris".into(),
        email: "ferris@artificialhumanity.io".into(),
        project: "ferroloop".into(),
        role: AgentRole::Lead,
        extra_areas: vec![],
        status: AgentStatus::Active,
        updated_at: At::from_unix_millis(1),
        by: "owner".into(),
    });
    RegistryEventKind::AgentRetire { .. } => "agent_retire", RegistryEventKind::AgentRetire {
        id: AgentId(crate::ids::seq_iri(1)),
    };
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryEvent {
    pub event_id: Iri,
    pub at: At,
    pub by: String,
    pub kind: RegistryEventKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::seq_iri;

    #[test]
    fn agent_role_flat_wire_round_trips() {
        for (raw, expected) in [
            ("lead", "lead"),
            ("assistant", "assistant"),
            ("lab_manager", "lab_manager"),
            ("custom:Scribe", "custom:scribe"),
        ] {
            let role = AgentRole::from_wire(raw).unwrap();
            assert_eq!(role.as_wire(), expected);
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            let back: AgentRole = serde_json::from_str(&json).unwrap();
            assert_eq!(back, role);
        }
        assert!(AgentRole::from_wire("scribe").is_err());
        assert!(AgentRole::from_wire("leed").is_err());
        assert!(AgentRole::from_wire("custom:123").is_err());
        assert!(AgentRole::from_wire("").is_err());
    }

    #[test]
    fn reports_to_derives_owner_for_leads_and_active_lead_for_assistants() {
        let lead = AgentRecord {
            id: AgentId(seq_iri(1)),
            name_key: "kayla".into(),
            display_name: "Kayla".into(),
            email: "kayla@artificialhumanity.io".into(),
            project: "kaggle-gemma4".into(),
            role: AgentRole::Lead,
            extra_areas: vec![],
            status: AgentStatus::Active,
            updated_at: At::from_unix_millis(1),
            by: "owner".into(),
        };
        let scribe = AgentRecord {
            id: AgentId(seq_iri(2)),
            name_key: "kacey".into(),
            display_name: "Kacey".into(),
            email: "kacey@artificialhumanity.io".into(),
            project: "kaggle-gemma4".into(),
            role: AgentRole::Custom("scribe".into()),
            extra_areas: vec![],
            status: AgentStatus::Active,
            updated_at: At::from_unix_millis(2),
            by: "owner".into(),
        };
        let all = vec![lead.clone(), scribe.clone()];
        assert_eq!(reports_to(&lead, &all), "Owner");
        assert_eq!(reports_to(&scribe, &all), "Kayla");
    }
}
