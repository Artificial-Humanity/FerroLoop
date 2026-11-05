//! Agent and role registry domain types (spec §2, activating identity spec §6a).

use crate::at::At;
use crate::ids::AgentId;
use crate::iri::Iri;
use crate::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StateError, StatePayload, StateScope,
    UpsertStateArgs, validate_slug, validate_summary,
};
use crate::store::{StateStore, StoreError};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
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
    /// A virtual scope is an area with no path: it names a group of agents and has no
    /// repository, project block or project store behind it. A repo-backed project's area
    /// carries the project root as its path.
    pub fn is_virtual(&self) -> bool {
        self.path.is_none()
    }

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

fn deserialize_stored_role<'de, D: Deserializer<'de>>(d: D) -> Result<AgentRole, D::Error> {
    let raw = String::deserialize(d)?;
    AgentRole::from_wire(&raw)
        .or_else(|_| validate_slug(&raw).map(AgentRole::Custom))
        .map_err(serde::de::Error::custom)
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
    #[serde(deserialize_with = "deserialize_stored_role")]
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
///
/// A virtual scope (an area with no path, see [`AreaRecord::is_virtual`]) follows the same rule:
/// its agents report to the scope's active `Lead` when it has one, else to `"Owner"`.
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
    /// The area is gone (a project or virtual group was deleted). A later `AreaUpsert` makes it
    /// again.
    AreaDelete { area_key: String },
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
    RegistryEventKind::AreaDelete { .. } => "area_delete", RegistryEventKind::AreaDelete {
        area_key: "ferroloop".into(),
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

/// Errors for the configurable role registry (Record #74).
#[derive(Debug, thiserror::Error)]
pub enum RoleError {
    #[error(
        "invalid review policy `{0}`: expected one of `lead-review`, `lab-manager-review`, `peer-review` (snake_case aliases `lead_review`, `lab_manager_review`, `peer_review` also accepted)"
    )]
    InvalidReviewPolicy(String),
    #[error(transparent)]
    InvalidSlug(#[from] StateError),
    #[error("role label must be non-empty")]
    EmptyLabel,
    #[error("cannot delete built-in role `{0}`: built-in roles cannot be deleted")]
    BuiltinRole(String),
    #[error("no such role `{0}`: run `fl role list` to see available roles")]
    NotFound(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Who reviews work done in a role. Serializes as kebab-case; deserialization
/// also accepts snake_case aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoleReviewPolicy {
    LeadReview,
    LabManagerReview,
    PeerReview,
}

impl RoleReviewPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LeadReview => "lead-review",
            Self::LabManagerReview => "lab-manager-review",
            Self::PeerReview => "peer-review",
        }
    }
    pub fn from_wire(s: &str) -> Result<Self, RoleError> {
        match s.trim() {
            "lead-review" | "lead_review" => Ok(Self::LeadReview),
            "lab-manager-review" | "lab_manager_review" => Ok(Self::LabManagerReview),
            "peer-review" | "peer_review" => Ok(Self::PeerReview),
            _ => Err(RoleError::InvalidReviewPolicy(s.to_string())),
        }
    }
}

impl fmt::Display for RoleReviewPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RoleReviewPolicy {
    type Err = RoleError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_wire(s)
    }
}

impl Serialize for RoleReviewPolicy {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RoleReviewPolicy {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::from_wire(&raw).map_err(serde::de::Error::custom)
    }
}

/// Normalize a role slug: `lab_manager` aliases to `lab-manager`, then the
/// canonical slug rules apply.
pub fn normalize_role_slug(raw: &str) -> Result<String, RoleError> {
    let trimmed = raw.trim();
    let aliased = if trimmed.eq_ignore_ascii_case("lab_manager") {
        "lab-manager"
    } else {
        trimmed
    };
    Ok(validate_slug(aliased)?)
}

const BUILTIN_ROLE_SLUGS: [&str; 11] = [
    "lead",
    "assistant",
    "strategist",
    "ui-developer",
    "lab-manager",
    "qa-tester",
    "security-engineer",
    "devops-release",
    "data-ml-engineer",
    "technical-writer",
    "journalist",
];

pub fn is_builtin_role(slug: &str) -> bool {
    let normalized = if slug.trim().eq_ignore_ascii_case("lab_manager") {
        "lab-manager".to_string()
    } else {
        slug.trim().to_ascii_lowercase()
    };
    BUILTIN_ROLE_SLUGS.contains(&normalized.as_str())
}

/// One configurable role (Record #74). JSON field names match the FerroDeck UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleRecord {
    pub slug: String,
    pub label: String,
    pub description: String,
    pub builtin: bool,
    pub review_policy: RoleReviewPolicy,
    pub prompt_template: String,
}

impl<'de> Deserialize<'de> for RoleRecord {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            slug: String,
            label: String,
            description: String,
            // Read only to reject missing/extra shape; value never trusted.
            #[allow(dead_code)]
            builtin: bool,
            review_policy: RoleReviewPolicy,
            prompt_template: String,
        }
        let raw = Raw::deserialize(d)?;
        // `builtin` is recomputed authoritatively, never trusted from input.
        let builtin = is_builtin_role(&raw.slug);
        Ok(Self {
            slug: raw.slug,
            label: raw.label,
            description: raw.description,
            builtin,
            review_policy: raw.review_policy,
            prompt_template: raw.prompt_template,
        })
    }
}

impl RoleRecord {
    pub fn new(
        slug: &str,
        label: &str,
        description: &str,
        review_policy: RoleReviewPolicy,
        prompt_template: &str,
    ) -> Result<Self, RoleError> {
        Self {
            slug: slug.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            builtin: false,
            review_policy,
            prompt_template: prompt_template.to_string(),
        }
        .validate()
    }

    /// Validate and normalize: slug normalized/validated, label non-empty,
    /// and `builtin` recomputed from the slug (input flag never trusted).
    pub fn validate(&self) -> Result<Self, RoleError> {
        let slug = normalize_role_slug(&self.slug)?;
        let label = self.label.trim().to_string();
        if label.is_empty() {
            return Err(RoleError::EmptyLabel);
        }
        Ok(Self {
            builtin: is_builtin_role(&slug),
            slug,
            label,
            description: self.description.clone(),
            review_policy: self.review_policy,
            prompt_template: self.prompt_template.clone(),
        })
    }
}

/// The 11 built-in roles, verbatim from the UI `BUILTIN_ROLE_REGISTRY`.
pub fn builtin_role_records() -> Vec<RoleRecord> {
    let raw: [(&str, &str, &str, RoleReviewPolicy, &str); 11] = [
        (
            "lead",
            "Project Lead",
            "Owns project planning, architecture, subordinate PR reviews, and delegation.",
            RoleReviewPolicy::LabManagerReview,
            "Read .agents/roles/lead.md and lead project architecture and reviews.",
        ),
        (
            "assistant",
            "Assisting Engineer",
            "Executes delegated engineering tasks with local subagent pre-review.",
            RoleReviewPolicy::LeadReview,
            "Read .agents/roles/assistant.md and execute delegated implementation tasks.",
        ),
        (
            "strategist",
            "Research & Architecture Strategist",
            "Serves on the Strategy Council synthesizing research literature and architecture.",
            RoleReviewPolicy::LeadReview,
            "Read .agents/roles/strategist.md and synthesize research against empirical findings.",
        ),
        (
            "ui-developer",
            "UI & Full-Stack Application Developer",
            "Specializes in React, TypeScript, Rust web frameworks, and native cross-platform UI.",
            RoleReviewPolicy::LeadReview,
            "Read .agents/roles/ui-developer.md and maintain cockpit UI parity and tests.",
        ),
        (
            "lab-manager",
            "Lab Manager",
            "Workspace-wide authority over ai-lab-0, GPU arbitration, and Lead PR reviews.",
            RoleReviewPolicy::PeerReview,
            "Read .agents/roles/lab-manager.md and manage workspace infrastructure and governance.",
        ),
        (
            "qa-tester",
            "QA & Verification Engineer",
            "Owns regression harnesses, conformance suites, and end-to-end verification.",
            RoleReviewPolicy::LeadReview,
            "Run exhaustive verification suites and report deterministic reproduction cases.",
        ),
        (
            "security-engineer",
            "Security & Vault Engineer",
            "Audits FerroGate vault policies, credential boundaries, and sandbox isolation.",
            RoleReviewPolicy::LeadReview,
            "Audit secret boundaries, capability matrices, and sandbox enforcement.",
        ),
        (
            "devops-release",
            "DevOps & Release Engineer",
            "Maintains CI/CD pipelines, versioned release slots, and systemd supervisor units.",
            RoleReviewPolicy::LeadReview,
            "Manage release slot rollouts, build provenance stamps, and CI workflows.",
        ),
        (
            "data-ml-engineer",
            "Data & ML Systems Engineer",
            "Builds dataset pipelines, training runs, and evaluation benchmarks.",
            RoleReviewPolicy::LeadReview,
            "Maintain training hygiene, dataset lineage, and benchmark reproducibility.",
        ),
        (
            "technical-writer",
            "Technical Writer & Spec Maintainer",
            "Maintains architecture specifications, runbooks, and operator documentation.",
            RoleReviewPolicy::LeadReview,
            "Keep specifications, runbooks, and documentation accurate and concise.",
        ),
        (
            "journalist",
            "Journalist & Technical Scribe",
            "Chronicles project milestones, experiment logs, and publication drafts.",
            RoleReviewPolicy::LeadReview,
            "Record empirical milestones, experiment notes, and technical dispatches.",
        ),
    ];
    raw.into_iter()
        .map(
            |(slug, label, description, review_policy, prompt_template)| RoleRecord {
                slug: slug.into(),
                label: label.into(),
                description: description.into(),
                builtin: true,
                review_policy,
                prompt_template: prompt_template.into(),
            },
        )
        .collect()
}

fn role_store_slug(slug: &str) -> String {
    format!("role-{slug}")
}

fn role_now() -> At {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    At::from_unix_millis(ms)
}

fn is_role_tombstone(value: &serde_json::Value) -> bool {
    value
        .get("__role_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Decode one stored role entry. Corrupt values (non-Runtime payload,
/// tombstone, undecodable or invalid record) yield `None`: callers skip
/// them rather than failing the whole listing.
fn decode_stored_role(entry: &crate::state::StateEntry) -> Option<RoleRecord> {
    // The entry key is authoritative: a value whose embedded slug does not
    // match the key suffix after `role-` is ignored, never re-keyed.
    let expected_slug = entry.slug.strip_prefix("role-")?;
    let StatePayload::Runtime(rt) = &entry.payload else {
        return None;
    };
    if is_role_tombstone(&rt.value) {
        return None;
    }
    let record: RoleRecord = serde_json::from_value(rt.value.clone()).ok()?;
    let record = record.validate().ok()?;
    (record.slug == expected_slug).then_some(record)
}

fn stored_roles(store: &impl StateStore) -> Result<BTreeMap<String, RoleRecord>, StoreError> {
    let entries = store.list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))?;
    let mut out = BTreeMap::new();
    for entry in entries.iter().filter(|e| e.slug.starts_with("role-")) {
        // Skip corrupt entries: one bad value must not break the registry.
        if let Some(record) = decode_stored_role(entry) {
            out.insert(record.slug.clone(), record);
        }
    }
    Ok(out)
}

/// All roles: builtins in registry order with stored overrides applied,
/// then custom roles sorted by slug. An empty store yields the 11 builtins.
/// Corrupt stored role entries are skipped.
pub fn list_roles(store: &impl StateStore) -> Result<Vec<RoleRecord>, StoreError> {
    let stored = stored_roles(store)?;
    let mut out = Vec::new();
    for builtin in builtin_role_records() {
        out.push(stored.get(&builtin.slug).cloned().unwrap_or(builtin));
    }
    let mut customs: Vec<RoleRecord> = stored
        .into_values()
        .filter(|r| !is_builtin_role(&r.slug))
        .collect();
    customs.sort_by(|a, b| a.slug.cmp(&b.slug));
    out.extend(customs);
    Ok(out)
}

pub fn get_role(store: &impl StateStore, slug: &str) -> Result<Option<RoleRecord>, RoleError> {
    let slug = normalize_role_slug(slug)?;
    if let Some(entry) = store.get_state_by_key(
        &StateScope::Workspace,
        StateConcern::Runtime,
        &role_store_slug(&slug),
    )? {
        if let Some(record) = decode_stored_role(&entry) {
            return Ok(Some(record));
        }
        // Tombstone/corrupt: fall through to the builtin default, if any.
        if is_builtin_role(&slug) {
            return Ok(builtin_role_records().into_iter().find(|r| r.slug == slug));
        }
        return Ok(None);
    }
    Ok(builtin_role_records().into_iter().find(|r| r.slug == slug))
}

/// Validate, normalize and persist a role (custom or builtin override).
pub fn upsert_role(store: &impl StateStore, record: RoleRecord) -> Result<RoleRecord, RoleError> {
    let record = record.validate()?;
    let value = serde_json::to_value(&record)
        .map_err(|e| RoleError::Store(StoreError::Backend(e.to_string())))?;
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: role_store_slug(&record.slug),
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Runtime(RuntimeEntry {
            value,
            merge_mode: MergeMode::LastWriter,
        }),
        at: role_now(),
        by: "fl".to_string(),
    })?;
    Ok(record)
}

/// Delete a custom role. Builtins are refused. The store offers no delete
/// for Runtime entries (`retire_memory` is Memory-only), so deletion writes
/// a tombstone Runtime value that `list_roles`/`get_role` skip; a later
/// `upsert_role` overwrites it and revives the slug.
pub fn delete_role(store: &impl StateStore, slug: &str) -> Result<(), RoleError> {
    let slug = normalize_role_slug(slug)?;
    if is_builtin_role(&slug) {
        return Err(RoleError::BuiltinRole(slug));
    }
    if get_role(store, &slug)?.is_none() {
        return Err(RoleError::NotFound(slug));
    }
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: role_store_slug(&slug),
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Runtime(RuntimeEntry {
            value: serde_json::json!({ "__role_deleted": true, "slug": slug }),
            merge_mode: MergeMode::LastWriter,
        }),
        at: role_now(),
        by: "fl".to_string(),
    })?;
    Ok(())
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

    #[test]
    fn stored_agent_record_deserializes_unknown_role_slug_as_custom_while_keeping_agent_role_strict()
     {
        assert!(
            serde_json::from_str::<AgentRole>("\"future-specialist\"").is_err(),
            "AgentRole deserialization must remain strict for CLI/Hub inputs"
        );
        let current = agent(1, "Kayla", AgentRole::Lead, "kaggle-gemma4");
        let future_stored = serde_json::to_string(&current)
            .unwrap()
            .replace("\"role\":\"lead\"", "\"role\":\"future-specialist\"");
        let rec: AgentRecord = serde_json::from_str(&future_stored).unwrap();
        assert_eq!(rec.role, AgentRole::Custom("future-specialist".into()));

        let invalid_stored = serde_json::to_string(&current)
            .unwrap()
            .replace("\"role\":\"lead\"", "\"role\":\"123 not a slug\"");
        assert!(serde_json::from_str::<AgentRecord>(&invalid_stored).is_err());
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

    #[test]
    fn reports_to_in_a_virtual_scope_is_the_scopes_active_lead_else_owner() {
        // A scope with no lead: every role reports to the Owner.
        let strategist = agent(1, "Ada", AgentRole::Strategist, "council");
        let assistant = agent(2, "Bea", AgentRole::Assistant, "council");
        let manager = agent(3, "Mgr", AgentRole::LabManager, "council");
        let mut all = vec![strategist.clone(), assistant.clone(), manager.clone()];
        for a in &all {
            assert_eq!(reports_to(a, &all), "Owner", "{}", a.role);
        }
        // A lead of another scope does not count; the scope's own lead does, but only while
        // it is active.
        all.push(agent(4, "Zed", AgentRole::Lead, "ferroloop"));
        assert_eq!(reports_to(&strategist, &all), "Owner");
        let mut lead = agent(5, "Cal", AgentRole::Lead, "council");
        all.push(lead.clone());
        assert_eq!(reports_to(&strategist, &all), "Cal");
        assert_eq!(reports_to(&assistant, &all), "Cal");
        assert_eq!(reports_to(&lead, &all), "Owner");
        assert_eq!(reports_to(&manager, &all), "Owner");
        all.pop();
        lead.status = AgentStatus::Retired;
        all.push(lead);
        assert_eq!(reports_to(&strategist, &all), "Owner");
    }

    #[test]
    fn role_review_policy_serde_kebab_and_snake_alias() {
        for (policy, kebab, snake) in [
            (RoleReviewPolicy::LeadReview, "lead-review", "lead_review"),
            (
                RoleReviewPolicy::LabManagerReview,
                "lab-manager-review",
                "lab_manager_review",
            ),
            (RoleReviewPolicy::PeerReview, "peer-review", "peer_review"),
        ] {
            assert_eq!(
                serde_json::to_string(&policy).unwrap(),
                format!("\"{kebab}\"")
            );
            assert_eq!(policy.to_string(), kebab);
            assert_eq!(kebab.parse::<RoleReviewPolicy>().unwrap(), policy);
            assert_eq!(snake.parse::<RoleReviewPolicy>().unwrap(), policy);
            let back: RoleReviewPolicy = serde_json::from_str(&format!("\"{snake}\"")).unwrap();
            assert_eq!(back, policy);
        }
        let err = "bogus".parse::<RoleReviewPolicy>().unwrap_err().to_string();
        assert!(err.contains("lead-review"), "{err}");
    }

    #[test]
    fn role_record_serde_ui_fields_and_builtin_spoof_corrected() {
        let json = r#"{"slug":"lead","label":"X","description":"d","builtin":false,"review_policy":"lead_review","prompt_template":"p"}"#;
        let rec: RoleRecord = serde_json::from_str(json).unwrap();
        assert!(rec.builtin, "builtin slug must recompute to true");
        assert_eq!(rec.review_policy, RoleReviewPolicy::LeadReview);
        let out = serde_json::to_value(&rec).unwrap();
        assert_eq!(out["review_policy"], "lead-review");
        assert_eq!(out["builtin"], true);
        let custom_json = r#"{"slug":"reviewer","label":"R","description":"","builtin":true,"review_policy":"peer-review","prompt_template":""}"#;
        let custom: RoleRecord = serde_json::from_str(custom_json).unwrap();
        assert!(!custom.builtin, "custom slug must recompute to false");
        // validate() also corrects a directly-constructed spoof.
        let spoof = RoleRecord {
            slug: "lead".into(),
            label: "L".into(),
            description: String::new(),
            builtin: false,
            review_policy: RoleReviewPolicy::LeadReview,
            prompt_template: String::new(),
        };
        assert!(spoof.validate().unwrap().builtin);
    }

    #[test]
    fn role_slug_lab_manager_normalization() {
        assert_eq!(normalize_role_slug("lab_manager").unwrap(), "lab-manager");
        assert!(is_builtin_role("lab_manager"));
        let rec = RoleRecord::new(
            "lab_manager",
            "Lab Manager",
            "",
            RoleReviewPolicy::PeerReview,
            "",
        )
        .unwrap();
        assert_eq!(rec.slug, "lab-manager");
        assert!(rec.builtin);
    }

    #[test]
    fn role_registry_empty_store_and_persistence() {
        use crate::MemStore;
        let store = MemStore::default();
        let roles = list_roles(&store).unwrap();
        assert_eq!(roles.len(), 11);
        let slugs: Vec<&str> = roles.iter().map(|r| r.slug.as_str()).collect();
        assert_eq!(
            slugs,
            [
                "lead",
                "assistant",
                "strategist",
                "ui-developer",
                "lab-manager",
                "qa-tester",
                "security-engineer",
                "devops-release",
                "data-ml-engineer",
                "technical-writer",
                "journalist"
            ]
        );
        assert!(roles.iter().all(|r| r.builtin));
        // Custom upsert appears and persists on re-read (same store = reopen).
        let custom = RoleRecord::new(
            "reviewer",
            "Reviewer",
            "Reviews things",
            RoleReviewPolicy::PeerReview,
            "Review it",
        )
        .unwrap();
        upsert_role(&store, custom).unwrap();
        assert_eq!(
            get_role(&store, "reviewer").unwrap().unwrap().label,
            "Reviewer"
        );
        assert_eq!(list_roles(&store).unwrap().len(), 12);
        // Lab-manager alias lookup finds the builtin.
        assert_eq!(
            get_role(&store, "lab_manager").unwrap().unwrap().slug,
            "lab-manager"
        );
        // Override a builtin label: persists, builtin stays true.
        let mut lead = get_role(&store, "lead").unwrap().unwrap();
        lead.label = "Boss".into();
        lead.builtin = false;
        let saved = upsert_role(&store, lead).unwrap();
        assert!(saved.builtin);
        let lead2 = get_role(&store, "lead").unwrap().unwrap();
        assert_eq!(lead2.label, "Boss");
        assert!(lead2.builtin);
        assert_eq!(list_roles(&store).unwrap().len(), 12);
        // Delete builtin refused; delete custom gone.
        assert!(matches!(
            delete_role(&store, "lead"),
            Err(RoleError::BuiltinRole(_))
        ));
        delete_role(&store, "reviewer").unwrap();
        assert!(get_role(&store, "reviewer").unwrap().is_none());
        assert_eq!(list_roles(&store).unwrap().len(), 11);
        assert!(matches!(
            delete_role(&store, "reviewer"),
            Err(RoleError::NotFound(_))
        ));
    }

    fn insert_raw_role(store: &crate::MemStore, store_slug: &str, value: serde_json::Value) {
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: store_slug.to_string(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Runtime(RuntimeEntry {
                    value,
                    merge_mode: MergeMode::LastWriter,
                }),
                at: At::from_unix_millis(1),
                by: "test".into(),
            })
            .unwrap();
    }

    fn role_value(slug: &str, label: &str) -> serde_json::Value {
        serde_json::to_value(
            RoleRecord::new(slug, label, "d", RoleReviewPolicy::PeerReview, "p").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn role_key_value_slug_mismatch_ignored_by_list_and_get() {
        use crate::MemStore;
        let store = MemStore::default();
        // Value claims slug `intruder` but lives under key `role-reviewer`.
        insert_raw_role(&store, "role-reviewer", role_value("intruder", "Intruder"));
        // Value tries to hijack the builtin `lead` from a custom key.
        insert_raw_role(&store, "role-ghost", role_value("lead", "Hijacked"));
        let roles = list_roles(&store).unwrap();
        assert_eq!(roles.len(), 11);
        assert!(
            roles
                .iter()
                .all(|r| r.slug != "intruder" && r.slug != "reviewer")
        );
        assert_eq!(
            roles.iter().find(|r| r.slug == "lead").unwrap().label,
            "Project Lead"
        );
        assert!(get_role(&store, "reviewer").unwrap().is_none());
        assert!(get_role(&store, "intruder").unwrap().is_none());
        assert_eq!(
            get_role(&store, "lead").unwrap().unwrap().label,
            "Project Lead"
        );
    }

    #[test]
    fn role_corrupt_stored_value_skipped() {
        use crate::MemStore;
        let store = MemStore::default();
        insert_raw_role(&store, "role-broken", serde_json::json!({"not": "a role"}));
        insert_raw_role(&store, "role-lead", serde_json::json!("garbage"));
        let roles = list_roles(&store).unwrap();
        assert_eq!(roles.len(), 11);
        assert_eq!(
            roles.iter().find(|r| r.slug == "lead").unwrap().label,
            "Project Lead"
        );
        assert!(get_role(&store, "broken").unwrap().is_none());
        assert_eq!(
            get_role(&store, "lead").unwrap().unwrap().label,
            "Project Lead"
        );
    }

    #[test]
    fn role_slug_beginning_with_role_prefix_roundtrips() {
        use crate::MemStore;
        let store = MemStore::default();
        let rec = RoleRecord::new(
            "role-player",
            "Role Player",
            "d",
            RoleReviewPolicy::LeadReview,
            "p",
        )
        .unwrap();
        let saved = upsert_role(&store, rec).unwrap();
        assert_eq!(saved.slug, "role-player");
        assert_eq!(
            get_role(&store, "role-player").unwrap().unwrap().label,
            "Role Player"
        );
        assert!(
            list_roles(&store)
                .unwrap()
                .iter()
                .any(|r| r.slug == "role-player")
        );
        delete_role(&store, "role-player").unwrap();
        assert!(get_role(&store, "role-player").unwrap().is_none());
        assert!(
            list_roles(&store)
                .unwrap()
                .iter()
                .all(|r| r.slug != "role-player")
        );
    }

    #[test]
    fn role_tombstone_revival_returns_new_record() {
        use crate::MemStore;
        let store = MemStore::default();
        upsert_role(
            &store,
            RoleRecord::new("reviewer", "Old", "", RoleReviewPolicy::PeerReview, "").unwrap(),
        )
        .unwrap();
        delete_role(&store, "reviewer").unwrap();
        assert!(get_role(&store, "reviewer").unwrap().is_none());
        let revived = upsert_role(
            &store,
            RoleRecord::new("reviewer", "New", "", RoleReviewPolicy::LeadReview, "").unwrap(),
        )
        .unwrap();
        assert_eq!(revived.label, "New");
        let got = get_role(&store, "reviewer").unwrap().unwrap();
        assert_eq!(got.label, "New");
        assert_eq!(got.review_policy, RoleReviewPolicy::LeadReview);
        assert!(
            list_roles(&store)
                .unwrap()
                .iter()
                .any(|r| r.slug == "reviewer" && r.label == "New")
        );
    }

    #[test]
    fn role_record_rejects_unknown_fields() {
        // RoleRecord's Deserialize impl uses deny_unknown_fields (Raw struct).
        let json = r#"{"slug":"reviewer","label":"R","description":"","builtin":false,"review_policy":"peer-review","prompt_template":"","bogus":1}"#;
        assert!(serde_json::from_str::<RoleRecord>(json).is_err());
    }
}
