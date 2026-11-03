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

/// A registered agent's role. The eleven default roles are first-class
/// variants; `Custom` is only for genuinely custom roles (`custom:<slug>`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentRole {
    Lead,
    Assistant,
    LabManager,
    UiDeveloper,
    WebDeveloper,
    MobileDeveloper,
    SystemDeveloper,
    DataEngineer,
    Tester,
    Strategist,
    Journalist,
    Custom(String),
}

impl AgentRole {
    /// Parse a role from its wire form (case-insensitive, trimmed). Legacy
    /// spellings normalize to the built-in variant: `lab-manager`,
    /// underscore forms of the hyphenated roles, `custom:<built-in>`, and
    /// `scribe` / `custom:scribe` (now `journalist`).
    pub fn from_wire(s: &str) -> Result<Self, StateError> {
        let lower = s.trim().to_ascii_lowercase();
        // Bare `lead` / `assistant` / `lab_manager` / `lab-manager` are the structural
        // built-ins. Every other default (`tester`, `scribe`, `ui-developer`, ...) is
        // accepted bare or as `custom:<name>`; `custom:lead`, `custom:assistant` and
        // `custom:lab_manager` stay `Custom`, distinct from the structural roles.
        match lower.as_str() {
            "lead" => return Ok(Self::Lead),
            "assistant" => return Ok(Self::Assistant),
            "lab_manager" | "lab-manager" => return Ok(Self::LabManager),
            _ => {}
        }
        let name = lower.strip_prefix("custom:").unwrap_or(&lower);
        let default = match name {
            "ui-developer" | "ui_developer" => Some(Self::UiDeveloper),
            "web-developer" | "web_developer" => Some(Self::WebDeveloper),
            "mobile-developer" | "mobile_developer" => Some(Self::MobileDeveloper),
            "system-developer" | "system_developer" => Some(Self::SystemDeveloper),
            "data-engineer" | "data_engineer" => Some(Self::DataEngineer),
            "tester" => Some(Self::Tester),
            "strategist" => Some(Self::Strategist),
            "journalist" | "scribe" => Some(Self::Journalist),
            _ => None,
        };
        if let Some(role) = default {
            return Ok(role);
        }
        match lower.strip_prefix("custom:") {
            Some(slug) => Ok(Self::Custom(validate_slug(slug)?)),
            None => Err(StateError::InvalidRole(s.to_string())),
        }
    }

    pub fn as_wire(&self) -> String {
        match self {
            Self::Lead => "lead".to_string(),
            Self::Assistant => "assistant".to_string(),
            Self::LabManager => "lab_manager".to_string(),
            Self::UiDeveloper => "ui-developer".to_string(),
            Self::WebDeveloper => "web-developer".to_string(),
            Self::MobileDeveloper => "mobile-developer".to_string(),
            Self::SystemDeveloper => "system-developer".to_string(),
            Self::DataEngineer => "data-engineer".to_string(),
            Self::Tester => "tester".to_string(),
            Self::Strategist => "strategist".to_string(),
            Self::Journalist => "journalist".to_string(),
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
/// - every other role (the built-in `Assistant`, developer, tester, strategist
///   and journalist roles, and `Custom(_)`) -> `display_name` of the active
///   `Lead` for `agent.project` (or `"Owner"` if none).
pub fn reports_to(agent: &AgentRecord, all_agents: &[AgentRecord]) -> String {
    match agent.role {
        AgentRole::Lead | AgentRole::LabManager => "Owner".to_string(),
        _ => all_agents
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
        let canonical = [
            (AgentRole::Lead, "lead"),
            (AgentRole::Assistant, "assistant"),
            (AgentRole::LabManager, "lab_manager"),
            (AgentRole::UiDeveloper, "ui-developer"),
            (AgentRole::WebDeveloper, "web-developer"),
            (AgentRole::MobileDeveloper, "mobile-developer"),
            (AgentRole::SystemDeveloper, "system-developer"),
            (AgentRole::DataEngineer, "data-engineer"),
            (AgentRole::Tester, "tester"),
            (AgentRole::Strategist, "strategist"),
            (AgentRole::Journalist, "journalist"),
            (AgentRole::Custom("reviewer".into()), "custom:reviewer"),
            (AgentRole::Custom("auditor".into()), "custom:auditor"),
        ];
        for (role, wire) in &canonical {
            assert_eq!(&role.as_wire(), wire);
            assert_eq!(&AgentRole::from_wire(wire).unwrap(), role);
            assert_eq!(&role.to_string(), wire);
            let json = serde_json::to_string(role).unwrap();
            assert_eq!(json, format!("\"{wire}\""));
            let back: AgentRole = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, role);
        }
        let aliases = [
            ("lab-manager", AgentRole::LabManager),
            ("  LEAD ", AgentRole::Lead),
            ("ui_developer", AgentRole::UiDeveloper),
            ("custom:ui-developer", AgentRole::UiDeveloper),
            ("custom:ui_developer", AgentRole::UiDeveloper),
            ("web_developer", AgentRole::WebDeveloper),
            ("custom:web-developer", AgentRole::WebDeveloper),
            ("custom:web_developer", AgentRole::WebDeveloper),
            ("mobile_developer", AgentRole::MobileDeveloper),
            ("custom:mobile-developer", AgentRole::MobileDeveloper),
            ("custom:mobile_developer", AgentRole::MobileDeveloper),
            ("system_developer", AgentRole::SystemDeveloper),
            ("custom:system-developer", AgentRole::SystemDeveloper),
            ("custom:system_developer", AgentRole::SystemDeveloper),
            ("data_engineer", AgentRole::DataEngineer),
            ("custom:data-engineer", AgentRole::DataEngineer),
            ("custom:data_engineer", AgentRole::DataEngineer),
            ("Custom:Tester", AgentRole::Tester),
            ("custom:strategist", AgentRole::Strategist),
            ("scribe", AgentRole::Journalist),
            ("Scribe", AgentRole::Journalist),
            ("custom:scribe", AgentRole::Journalist),
            ("custom:journalist", AgentRole::Journalist),
            ("custom:Reviewer", AgentRole::Custom("reviewer".into())),
        ];
        for (raw, expected) in aliases {
            assert_eq!(AgentRole::from_wire(raw).unwrap(), expected, "{raw}");
        }
        for (raw, slug) in [
            ("custom:lead", "lead"),
            ("custom:assistant", "assistant"),
            ("custom:lab_manager", "lab_manager"),
            ("custom:lab-manager", "lab-manager"),
        ] {
            let role = AgentRole::from_wire(raw).unwrap();
            assert_eq!(role, AgentRole::Custom(slug.into()), "{raw}");
            assert_ne!(role, AgentRole::Lead);
            assert_ne!(role, AgentRole::Assistant);
            assert_ne!(role, AgentRole::LabManager);
            assert_eq!(role.as_wire(), raw);
        }
        assert!(AgentRole::from_wire("leed").is_err());
        assert!(AgentRole::from_wire("reviewer").is_err());
        assert!(AgentRole::from_wire("custom:123").is_err());
        assert!(AgentRole::from_wire("custom:").is_err());
        assert!(AgentRole::from_wire("").is_err());
    }

    #[test]
    fn stored_legacy_role_records_deserialize_to_the_new_canonical_form() {
        for (old, new) in [
            ("custom:scribe", "journalist"),
            ("scribe", "journalist"),
            ("custom:ui-developer", "ui-developer"),
            ("custom:data_engineer", "data-engineer"),
            ("custom:tester", "tester"),
            ("custom:reviewer", "custom:reviewer"),
        ] {
            let role: AgentRole = serde_json::from_str(&format!("\"{old}\"")).unwrap();
            assert_eq!(serde_json::to_string(&role).unwrap(), format!("\"{new}\""));
        }
        // A whole stored record with the pre-change role spelling.
        let current = agent(1, "Uma", AgentRole::UiDeveloper, "p");
        let stored = serde_json::to_string(&current).unwrap().replace(
            "\"role\":\"ui-developer\"",
            "\"role\":\"custom:ui-developer\"",
        );
        assert!(
            stored.contains("\"role\":\"custom:ui-developer\""),
            "{stored}"
        );
        let rec: AgentRecord = serde_json::from_str(&stored).unwrap();
        assert_eq!(rec.role, AgentRole::UiDeveloper);
        let out = serde_json::to_string(&rec).unwrap();
        assert!(out.contains("\"role\":\"ui-developer\""), "{out}");
    }

    fn agent(n: u64, name: &str, role: AgentRole, project: &str) -> AgentRecord {
        AgentRecord {
            id: AgentId(seq_iri(n)),
            name_key: name.to_ascii_lowercase(),
            display_name: name.into(),
            email: format!("{}@artificialhumanity.io", name.to_ascii_lowercase()),
            project: project.into(),
            role,
            extra_areas: vec![],
            status: AgentStatus::Active,
            updated_at: At::from_unix_millis(n),
            by: "owner".into(),
        }
    }

    #[test]
    fn reports_to_derives_owner_for_leads_and_active_lead_for_everyone_else() {
        let lead = agent(1, "Kayla", AgentRole::Lead, "kaggle-gemma4");
        let manager = agent(2, "Mgr", AgentRole::LabManager, "kaggle-gemma4");
        let others = [
            agent(3, "Kacey", AgentRole::Journalist, "kaggle-gemma4"),
            agent(4, "Uma", AgentRole::UiDeveloper, "kaggle-gemma4"),
            agent(5, "Asa", AgentRole::Assistant, "kaggle-gemma4"),
            agent(
                6,
                "Rex",
                AgentRole::Custom("reviewer".into()),
                "kaggle-gemma4",
            ),
        ];
        let elsewhere = agent(7, "Tess", AgentRole::Tester, "other");
        let mut all = vec![lead.clone(), manager.clone(), elsewhere.clone()];
        all.extend(others.iter().cloned());
        assert_eq!(reports_to(&lead, &all), "Owner");
        assert_eq!(reports_to(&manager, &all), "Owner");
        for a in &others {
            assert_eq!(reports_to(a, &all), "Kayla", "{}", a.role);
        }
        assert_eq!(reports_to(&elsewhere, &all), "Owner");
    }
}
