//! State database domain types: scopes, the three concerns (`Memory`,
//! `Handoff`, `Runtime`), revisions, conflicts, and sync cursors (spec §1, §3).

use crate::at::At;
use crate::ids::{RevisionId, StateId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::str::FromStr;

pub const MAX_SUMMARY_CHARS: usize = 240;
pub const MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_NOTES_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error(
        "invalid slug `{0}`: must be 1..=64 bytes of ASCII lowercase letters, digits, `-`, or `_`, starting with an ASCII letter (`^[a-z][a-z0-9_-]{{0,63}}$`)"
    )]
    InvalidSlug(String),
    #[error(
        "invalid state scope `{0}`: expected `workspace`, `group:<slug>`, `project:<slug>`, `agent:<name>`, or `workflow:<slug>`"
    )]
    InvalidScope(String),
    #[error(
        "invalid agent role `{0}`: expected `lead`, `assistant`, `lab_manager`, or `custom:<slug>`"
    )]
    InvalidRole(String),
    #[error("summary must be a single non-empty line of at most 240 characters")]
    InvalidSummary,
    #[error("memory body exceeds maximum size of 262144 bytes (got {0} bytes)")]
    BodyTooLarge(usize),
    #[error("handoff notes exceed maximum size of 65536 bytes (got {0} bytes)")]
    NotesTooLarge(usize),
    #[error("runtime value exceeds maximum serialized size of 262144 bytes (got {0} bytes)")]
    RuntimeValueTooLarge(usize),
    #[error("runtime entry with `merge_mode = \"append\"` requires a JSON array value")]
    AppendRequiresArray,
    #[error("memory with `lifecycle = \"superseded\"` requires `superseded_by`")]
    SupersededRequiresTarget,
    #[error("memory with `lifecycle = \"{0}\"` cannot set `superseded_by`")]
    UnexpectedSupersededBy(String),
    #[error("memory `{0}` cannot supersede itself")]
    SelfSupersede(StateId),
}

/// Normalize to ASCII lowercase and validate `^[a-z][a-z0-9_-]{0,63}$`.
pub fn validate_slug(raw: &str) -> Result<String, StateError> {
    let lower = raw.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return Err(StateError::InvalidSlug(raw.to_string()));
    }
    if !bytes[0].is_ascii_lowercase() {
        return Err(StateError::InvalidSlug(raw.to_string()));
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
    {
        return Err(StateError::InvalidSlug(raw.to_string()));
    }
    Ok(lower)
}

pub fn validate_summary(s: &str) -> Result<String, StateError> {
    if s.contains('\n') || s.contains('\r') {
        return Err(StateError::InvalidSummary);
    }
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_SUMMARY_CHARS {
        return Err(StateError::InvalidSummary);
    }
    Ok(trimmed.to_string())
}

/// Hierarchical state scope (spec §1.2).
/// Order of variants defines the deterministic sort order:
/// `Workspace < Group < Project < Agent < Workflow`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateScope {
    Workspace,
    Group(String),
    Project(String),
    Agent(String),
    Workflow(String),
}

impl StateScope {
    pub fn from_wire(s: &str) -> Result<Self, StateError> {
        if s.eq_ignore_ascii_case("workspace") {
            return Ok(Self::Workspace);
        }
        let Some((prefix, rest)) = s.split_once(':') else {
            return Err(StateError::InvalidScope(s.to_string()));
        };
        let slug = validate_slug(rest).map_err(|_| StateError::InvalidScope(s.to_string()))?;
        match prefix.to_ascii_lowercase().as_str() {
            "group" => Ok(Self::Group(slug)),
            "project" => Ok(Self::Project(slug)),
            "agent" => Ok(Self::Agent(slug)),
            "workflow" => Ok(Self::Workflow(slug)),
            _ => Err(StateError::InvalidScope(s.to_string())),
        }
    }

    pub fn as_wire(&self) -> String {
        match self {
            Self::Workspace => "workspace".to_string(),
            Self::Group(s) => format!("group:{s}"),
            Self::Project(s) => format!("project:{s}"),
            Self::Agent(s) => format!("agent:{s}"),
            Self::Workflow(s) => format!("workflow:{s}"),
        }
    }
}

impl fmt::Display for StateScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_wire())
    }
}

impl FromStr for StateScope {
    type Err = StateError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_wire(s)
    }
}

impl Serialize for StateScope {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_wire())
    }
}

impl<'de> Deserialize<'de> for StateScope {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::from_wire(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateConcern {
    Memory,
    Handoff,
    Runtime,
}

crate::wire::wire_names!(StateConcern as state_concern_wire {
    Memory => "memory",
    Handoff => "handoff",
    Runtime => "runtime",
});
crate::wire::wire_parse!(StateConcern as state_concern_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    Warning,
    Rule,
    Decision,
    Note,
}

crate::wire::wire_names!(MemoryCategory as memory_category_wire {
    Warning => "warning",
    Rule => "rule",
    Decision => "decision",
    Note => "note",
});
crate::wire::wire_parse!(MemoryCategory as memory_category_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBasis {
    Ruling,
    Measured,
    Inference,
}

crate::wire::wire_names!(MemoryBasis as memory_basis_wire {
    Ruling => "ruling",
    Measured => "measured",
    Inference => "inference",
});
crate::wire::wire_parse!(MemoryBasis as memory_basis_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLifecycle {
    Active,
    Retired,
    Superseded,
}

crate::wire::wire_names!(MemoryLifecycle as memory_lifecycle_wire {
    Active => "active",
    Retired => "retired",
    Superseded => "superseded",
});
crate::wire::wire_parse!(MemoryLifecycle as memory_lifecycle_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    Conflict,
    Append,
    LastWriter,
}

crate::wire::wire_names!(MergeMode as merge_mode_wire {
    Conflict => "conflict",
    Append => "append",
    LastWriter => "last_writer",
});
crate::wire::wire_parse!(MergeMode as merge_mode_parse);

/// How an update changes `sensitive` on a `StateEntry` (spec §1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitivityPatch {
    /// Omit flag: keep existing `sensitive` on update, or `false` on create.
    Inherit,
    /// `--sensitive` / `sensitive = true`: mark entry sensitive.
    SetSensitive,
    /// `--not-sensitive` / `clear_sensitive = true`: clear current `sensitive`
    /// while preserving `ever_sensitive` if any prior revision was sensitive.
    ClearSensitive,
}

impl SensitivityPatch {
    pub fn apply(self, existing: Option<bool>) -> bool {
        match self {
            Self::SetSensitive => true,
            Self::ClearSensitive => false,
            Self::Inherit => existing.unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEntry {
    pub category: MemoryCategory,
    pub basis: MemoryBasis,
    pub lifecycle: MemoryLifecycle,
    pub superseded_by: Option<StateId>,
    pub pinned: bool,
    pub summary: String,
    pub body: String,
}

impl MemoryEntry {
    pub fn validate(&self, entry_id: &StateId) -> Result<(), StateError> {
        validate_summary(&self.summary)?;
        if self.body.len() > MAX_BODY_BYTES {
            return Err(StateError::BodyTooLarge(self.body.len()));
        }
        if let Some(ref target) = self.superseded_by
            && target == entry_id
        {
            return Err(StateError::SelfSupersede(entry_id.clone()));
        }
        match (self.lifecycle, &self.superseded_by) {
            (MemoryLifecycle::Superseded, None) => Err(StateError::SupersededRequiresTarget),
            (MemoryLifecycle::Active | MemoryLifecycle::Retired, Some(_)) => Err(
                StateError::UnexpectedSupersededBy(self.lifecycle.as_wire().to_string()),
            ),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffEntry {
    pub summary: String,
    pub shipped: Vec<String>,
    pub in_flight: Vec<String>,
    pub blockers: Vec<String>,
    pub next_steps: Vec<String>,
    pub notes: Option<String>,
}

impl HandoffEntry {
    pub fn validate(&self) -> Result<(), StateError> {
        validate_summary(&self.summary)?;
        if let Some(notes) = &self.notes
            && notes.len() > MAX_NOTES_BYTES
        {
            return Err(StateError::NotesTooLarge(notes.len()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeEntry {
    pub value: Value,
    pub merge_mode: MergeMode,
}

impl RuntimeEntry {
    pub fn validate(&self) -> Result<(), StateError> {
        if self.merge_mode == MergeMode::Append && !self.value.is_array() {
            return Err(StateError::AppendRequiresArray);
        }
        let encoded = serde_json::to_string(&self.value).unwrap_or_default();
        if encoded.len() > MAX_BODY_BYTES {
            return Err(StateError::RuntimeValueTooLarge(encoded.len()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatePayload {
    Memory(MemoryEntry),
    Handoff(HandoffEntry),
    Runtime(RuntimeEntry),
}

impl StatePayload {
    pub fn concern(&self) -> StateConcern {
        match self {
            Self::Memory(_) => StateConcern::Memory,
            Self::Handoff(_) => StateConcern::Handoff,
            Self::Runtime(_) => StateConcern::Runtime,
        }
    }

    pub fn validate(&self, entry_id: &StateId) -> Result<(), StateError> {
        match self {
            Self::Memory(m) => m.validate(entry_id),
            Self::Handoff(h) => h.validate(),
            Self::Runtime(r) => r.validate(),
        }
    }
}

crate::wire::wire_tags!(StatePayload as state_payload_wire {
    StatePayload::Memory(_) => "memory", StatePayload::Memory(MemoryEntry {
        category: MemoryCategory::Rule,
        basis: MemoryBasis::Ruling,
        lifecycle: MemoryLifecycle::Active,
        superseded_by: None,
        pinned: true,
        summary: "one line".into(),
        body: "details".into(),
    });
    StatePayload::Handoff(_) => "handoff", StatePayload::Handoff(HandoffEntry {
        summary: "status".into(),
        shipped: vec![],
        in_flight: vec![],
        blockers: vec![],
        next_steps: vec![],
        notes: None,
    });
    StatePayload::Runtime(_) => "runtime", StatePayload::Runtime(RuntimeEntry {
        value: serde_json::json!(["step-1"]),
        merge_mode: MergeMode::Append,
    });
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateEntry {
    pub id: StateId,
    pub scope: StateScope,
    pub concern: StateConcern,
    pub slug: String,
    pub sensitive: bool,
    pub ever_sensitive: bool,
    pub payload: StatePayload,
    pub head_revision: RevisionId,
    pub updated_at: At,
    pub by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRevision {
    pub revision_id: RevisionId,
    pub entry_id: StateId,
    pub parent_revisions: Vec<RevisionId>,
    pub scope: StateScope,
    pub concern: StateConcern,
    pub slug: String,
    pub sensitive: bool,
    pub ever_sensitive: bool,
    pub payload: StatePayload,
    pub at: At,
    pub by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConflict {
    pub entry_id: StateId,
    pub local_head: RevisionId,
    pub remote_head: RevisionId,
    pub detected_at: At,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateSyncCursor {
    pub remote: String,
    pub branch: String,
    pub last_pushed_commit: Option<String>,
    pub last_revision_seq: u64,
    pub last_registry_seq: u64,
}

#[derive(Debug, Clone)]
pub struct UpsertStateArgs {
    pub scope: StateScope,
    pub slug: String,
    pub sensitivity: SensitivityPatch,
    pub payload: StatePayload,
    pub at: At,
    pub by: String,
}

/// Deterministic two-parent list sorted by IRI string (spec §3.3).
pub fn sorted_parents(a: RevisionId, b: RevisionId) -> Vec<RevisionId> {
    if a.as_str() <= b.as_str() {
        vec![a, b]
    } else {
        vec![b, a]
    }
}

/// Returns `true` if `candidate_ancestor` is equal to `start` or reachable via
/// `parent_revisions` in `revs`.
pub fn is_ancestor_in_dag(
    candidate_ancestor: &RevisionId,
    start: &RevisionId,
    revs: &BTreeMap<RevisionId, StateRevision>,
) -> bool {
    if candidate_ancestor == start {
        return true;
    }
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(start.clone());
    visited.insert(start.clone());
    while let Some(cur) = queue.pop_front() {
        if let Some(rev) = revs.get(&cur) {
            for p in &rev.parent_revisions {
                if p == candidate_ancestor {
                    return true;
                }
                if visited.insert(p.clone()) {
                    queue.push_back(p.clone());
                }
            }
        }
    }
    false
}

/// Lowest common ancestor of `a` and `b` in `revs` by BFS distance from `a`.
pub fn lowest_common_ancestor_in_dag(
    a: &RevisionId,
    b: &RevisionId,
    revs: &BTreeMap<RevisionId, StateRevision>,
) -> Option<RevisionId> {
    let mut ancestors_b = BTreeSet::new();
    let mut q = VecDeque::new();
    q.push_back(b.clone());
    ancestors_b.insert(b.clone());
    while let Some(cur) = q.pop_front() {
        if let Some(rev) = revs.get(&cur) {
            for p in &rev.parent_revisions {
                if ancestors_b.insert(p.clone()) {
                    q.push_back(p.clone());
                }
            }
        }
    }
    let mut visited_a = BTreeSet::new();
    let mut qa = VecDeque::new();
    qa.push_back(a.clone());
    visited_a.insert(a.clone());
    while let Some(cur) = qa.pop_front() {
        if ancestors_b.contains(&cur) {
            return Some(cur);
        }
        if let Some(rev) = revs.get(&cur) {
            for p in &rev.parent_revisions {
                if visited_a.insert(p.clone()) {
                    qa.push_back(p.clone());
                }
            }
        }
    }
    None
}

/// Compute `MergeMode::Append` array merge from `base`, `local`, and `remote` (spec §3.3).
pub fn compute_append_merge(base: Vec<Value>, local: &[Value], remote: &[Value]) -> Vec<Value> {
    let mut merged = local.to_vec();
    for item in remote {
        if !base.contains(item) && !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    merged
}

/// Choose the winning revision for `MergeMode::LastWriter` by comparing
/// `(revision.at.as_str(), revision.revision_id.as_str())` (spec §3.3).
pub fn compute_last_writer_merge<'a>(
    local: &'a StateRevision,
    remote: &'a StateRevision,
) -> &'a StateRevision {
    if (remote.at.as_str(), remote.revision_id.as_str())
        >= (local.at.as_str(), local.revision_id.as_str())
    {
        remote
    } else {
        local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_requires_leading_ascii_letter_and_normalizes_case() {
        assert_eq!(validate_slug("FerroLoop_1-a").unwrap(), "ferroloop_1-a");
        for bad in [
            "",
            "1abc",
            "42",
            "-abc",
            "_abc",
            "a/b",
            "a\\b",
            "a..b",
            "a b",
            "a\nb",
            &"a".repeat(65),
        ] {
            assert!(
                matches!(validate_slug(bad), Err(StateError::InvalidSlug(_))),
                "expected InvalidSlug for {bad:?}"
            );
        }
    }

    #[test]
    fn state_scope_flat_wire_round_trips_and_sorts_by_hierarchy() {
        let scopes = [
            StateScope::from_wire("Workspace").unwrap(),
            StateScope::from_wire("group:Core-Team").unwrap(),
            StateScope::from_wire("project:FerroLoop").unwrap(),
            StateScope::from_wire("agent:Ferris").unwrap(),
            StateScope::from_wire("workflow:Release_1").unwrap(),
        ];
        let wires: Vec<String> = scopes.iter().map(StateScope::as_wire).collect();
        assert_eq!(
            wires,
            vec![
                "workspace",
                "group:core-team",
                "project:ferroloop",
                "agent:ferris",
                "workflow:release_1",
            ]
        );
        for scope in &scopes {
            let json = serde_json::to_string(scope).unwrap();
            assert_eq!(json, format!("\"{}\"", scope.as_wire()));
            let back: StateScope = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, scope);
        }
        let mut shuffled = scopes.to_vec();
        shuffled.reverse();
        shuffled.sort();
        assert_eq!(shuffled, scopes);

        for bad in ["", "project:", "project:123", "other:abc", "workspace:x"] {
            assert!(StateScope::from_wire(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn append_requires_json_array_and_summary_forbids_newlines() {
        let bad_append = RuntimeEntry {
            value: serde_json::json!({"not": "an array"}),
            merge_mode: MergeMode::Append,
        };
        assert_eq!(bad_append.validate(), Err(StateError::AppendRequiresArray));
        let ok_append = RuntimeEntry {
            value: serde_json::json!([1, 2]),
            merge_mode: MergeMode::Append,
        };
        assert!(ok_append.validate().is_ok());
        assert_eq!(
            validate_summary("line1\nline2"),
            Err(StateError::InvalidSummary)
        );
        assert_eq!(validate_summary("   "), Err(StateError::InvalidSummary));
    }
}
