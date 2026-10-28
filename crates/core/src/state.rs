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
    #[error(
        "invalid wire address `{0}`: expected `<agent>`, `urn:uuid:...`, `@all`, `@area:<slug>`, `@role:<role>`, or `@area:<slug>/<role>`"
    )]
    InvalidAddress(String),
    #[error("message subject must be a single non-empty line of at most 512 bytes")]
    InvalidMessageSubject,
    #[error("message body must be non-empty and at most 262144 bytes (got {0} bytes)")]
    InvalidMessageBody(usize),
    #[error("reply message requires both `reply_to` and `correlation_id`")]
    ReplyRequiresParent,
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
    #[serde(default)]
    pub last_wire_msg_seq: u64,
    #[serde(default)]
    pub last_wire_rcpt_seq: u64,
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

/// One ranked hit from [`search_state_entries`] (FerroState Phase 2 spec §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSearchHit {
    pub entry: StateEntry,
    pub score_millis: u64,
    pub matched_terms: Vec<String>,
    pub snippet: String,
    pub estimated_tokens: u64,
}

/// Rough token estimate reused by recall budgets and the code-index
/// savings ledger: `ceil(chars / 4)`, with empty text at `0`.
pub fn estimate_tokens(text: &str) -> u64 {
    let n = text.chars().count() as u64;
    n.saturating_add(3) / 4
}

fn tokenize_search_text(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            cur.push(ch.to_ascii_lowercase());
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// `(summary, body)` searchable fields for an entry payload; the slug is
/// handled separately. `Runtime` has no summary and searches over its
/// serialized JSON value.
fn searchable_summary_and_body(payload: &StatePayload) -> (String, String) {
    match payload {
        StatePayload::Memory(m) => (m.summary.clone(), m.body.clone()),
        StatePayload::Handoff(h) => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(notes) = &h.notes {
                parts.push(notes.clone());
            }
            parts.extend(h.shipped.iter().cloned());
            parts.extend(h.in_flight.iter().cloned());
            parts.extend(h.blockers.iter().cloned());
            parts.extend(h.next_steps.iter().cloned());
            (h.summary.clone(), parts.join("\n"))
        }
        StatePayload::Runtime(r) => (
            String::new(),
            serde_json::to_string(&r.value).unwrap_or_default(),
        ),
    }
}

fn search_snippet(text: &str, summary: &str, matched_terms: &[String]) -> String {
    let lower = text.to_ascii_lowercase();
    let mut first: Option<usize> = None;
    for term in matched_terms {
        if let Some(pos) = lower.find(term.as_str()) {
            first = Some(match first {
                Some(prev) => prev.min(pos),
                None => pos,
            });
        }
    }
    let Some(byte_pos) = first else {
        let fallback = if summary.is_empty() { text } else { summary };
        return fallback
            .chars()
            .take(200)
            .collect::<String>()
            .trim()
            .to_string();
    };
    let char_idx = text[..byte_pos].chars().count();
    let chars: Vec<char> = text.chars().collect();
    let start = char_idx.saturating_sub(80);
    let end = (start + 200).min(chars.len());
    chars[start..end]
        .iter()
        .copied()
        .collect::<String>()
        .trim()
        .to_string()
}

/// Rank `entries` against `query` with field-weighted BM25 (spec §2):
/// slug weight 3, summary weight 2, body weight 1; `k1 = 1.2`, `b = 0.75`.
/// Pinned memories score x1.5, non-active memories x0.25. Only entries
/// with score > 0 are returned, sorted by raw score desc, then
/// `updated_at` desc, then slug asc, capped at `limit`.
pub fn search_state_entries(
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> Vec<StateSearchHit> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;

    if limit == 0 {
        return Vec::new();
    }
    let query_terms: Vec<String> = tokenize_search_text(query)
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if query_terms.is_empty() {
        return Vec::new();
    }

    struct SearchDoc {
        entry: StateEntry,
        weighted_tf: BTreeMap<String, f64>,
        weighted_len: f64,
        present: BTreeSet<String>,
        summary: String,
        searchable_text: String,
    }

    let mut docs: Vec<SearchDoc> = Vec::new();
    for entry in entries {
        if let Some(want_scope) = scope
            && &entry.scope != want_scope
        {
            continue;
        }
        if let Some(want_concern) = concern
            && entry.concern != want_concern
        {
            continue;
        }
        let (summary, body) = searchable_summary_and_body(&entry.payload);
        let searchable_text = [&entry.slug, &summary, &body]
            .into_iter()
            .filter(|part| !part.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let mut weighted_tf: BTreeMap<String, f64> = BTreeMap::new();
        let mut weighted_len = 0f64;
        for (field, weight) in [(&entry.slug, 3.0f64), (&summary, 2.0f64), (&body, 1.0f64)] {
            for token in tokenize_search_text(field) {
                *weighted_tf.entry(token).or_default() += weight;
                weighted_len += weight;
            }
        }
        let present: BTreeSet<String> = weighted_tf.keys().cloned().collect();
        docs.push(SearchDoc {
            entry: entry.clone(),
            weighted_tf,
            weighted_len,
            present,
            summary,
            searchable_text,
        });
    }
    if docs.is_empty() {
        return Vec::new();
    }
    let total = docs.len() as f64;
    let avgdl = docs.iter().map(|d| d.weighted_len).sum::<f64>() / total;
    if avgdl <= 0.0 {
        return Vec::new();
    }

    // IDF over the provided (filtered) entry set, for query terms only.
    let mut idf: BTreeMap<String, f64> = BTreeMap::new();
    for term in &query_terms {
        let df = docs.iter().filter(|d| d.present.contains(term)).count() as f64;
        idf.insert(term.clone(), (1.0 + (total - df + 0.5) / (df + 0.5)).ln());
    }

    // Keep the raw score beside each hit until after sorting: distinct
    // positive scores below one milli apart would otherwise truncate to
    // equal `score_millis` and fall through to the tie-breaks.
    let mut hits: Vec<(f64, StateSearchHit)> = Vec::new();
    for doc in docs {
        let matched_terms: Vec<String> = query_terms
            .iter()
            .filter(|t| doc.present.contains(*t))
            .cloned()
            .collect();
        if matched_terms.is_empty() {
            continue;
        }
        let mut score = 0f64;
        for term in &matched_terms {
            let f = doc.weighted_tf.get(term).copied().unwrap_or(0.0);
            let denom = f + K1 * (1.0 - B + B * doc.weighted_len / avgdl);
            if denom > 0.0 {
                score += idf[term] * f * (K1 + 1.0) / denom;
            }
        }
        if let StatePayload::Memory(m) = &doc.entry.payload {
            if m.pinned {
                score *= 1.5;
            }
            if m.lifecycle != MemoryLifecycle::Active {
                score *= 0.25;
            }
        }
        if !score.is_finite() || score <= 0.0 {
            continue;
        }
        hits.push((
            score,
            StateSearchHit {
                snippet: search_snippet(&doc.searchable_text, &doc.summary, &matched_terms),
                estimated_tokens: estimate_tokens(&doc.searchable_text),
                entry: doc.entry,
                score_millis: (score * 1000.0) as u64,
                matched_terms,
            },
        ));
    }

    hits.sort_by(|a, b| {
        // `score` is finite and positive (checked above), so total_cmp is
        // a safe descending raw-score order here.
        b.0.total_cmp(&a.0)
            .then_with(|| {
                b.1.entry
                    .updated_at
                    .unix_millis()
                    .cmp(&a.1.entry.updated_at.unix_millis())
            })
            .then_with(|| a.1.entry.slug.cmp(&b.1.entry.slug))
            .then_with(|| a.1.entry.id.as_str().cmp(b.1.entry.id.as_str()))
    });
    hits.truncate(limit);
    hits.into_iter().map(|(_, hit)| hit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::seq_iri;

    fn memory_entry(
        n: u64,
        slug: &str,
        summary: &str,
        body: &str,
        pinned: bool,
        lifecycle: MemoryLifecycle,
        updated_ms: u64,
    ) -> StateEntry {
        StateEntry {
            id: StateId(seq_iri(n)),
            scope: StateScope::Workspace,
            concern: StateConcern::Memory,
            slug: slug.to_string(),
            sensitive: false,
            ever_sensitive: false,
            payload: StatePayload::Memory(MemoryEntry {
                category: MemoryCategory::Note,
                basis: MemoryBasis::Inference,
                lifecycle,
                superseded_by: if lifecycle == MemoryLifecycle::Superseded {
                    Some(StateId(seq_iri(999)))
                } else {
                    None
                },
                pinned,
                summary: summary.to_string(),
                body: body.to_string(),
            }),
            head_revision: RevisionId(seq_iri(1000 + n)),
            updated_at: At::from_unix_millis(updated_ms),
            by: "test".to_string(),
        }
    }

    fn handoff_entry(n: u64, slug: &str, summary: &str, notes: &str) -> StateEntry {
        StateEntry {
            id: StateId(seq_iri(n)),
            scope: StateScope::Workspace,
            concern: StateConcern::Handoff,
            slug: slug.to_string(),
            sensitive: false,
            ever_sensitive: false,
            payload: StatePayload::Handoff(HandoffEntry {
                summary: summary.to_string(),
                shipped: vec![],
                in_flight: vec![],
                blockers: vec![],
                next_steps: vec![],
                notes: Some(notes.to_string()),
            }),
            head_revision: RevisionId(seq_iri(1000 + n)),
            updated_at: At::from_unix_millis(1_000),
            by: "test".to_string(),
        }
    }

    fn runtime_entry(n: u64, slug: &str, value: Value) -> StateEntry {
        StateEntry {
            id: StateId(seq_iri(n)),
            scope: StateScope::Workspace,
            concern: StateConcern::Runtime,
            slug: slug.to_string(),
            sensitive: false,
            ever_sensitive: false,
            payload: StatePayload::Runtime(RuntimeEntry {
                value,
                merge_mode: MergeMode::LastWriter,
            }),
            head_revision: RevisionId(seq_iri(1000 + n)),
            updated_at: At::from_unix_millis(1_000),
            by: "test".to_string(),
        }
    }

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

    #[test]
    fn search_ranks_body_relevant_above_irrelevant() {
        let irrelevant = memory_entry(
            1,
            "cooking-notes",
            "cooking notes",
            "pasta recipes and kitchen timers",
            false,
            MemoryLifecycle::Active,
            3_000,
        );
        let partial = memory_entry(
            2,
            "partial-note",
            "ranking mention",
            "ranking appears once here",
            false,
            MemoryLifecycle::Active,
            2_000,
        );
        let relevant = memory_entry(
            3,
            "relevant-note",
            "search design",
            "bm25 ranking search ranking bm25 ranking",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        // Input order is irrelevant, partial, relevant: an unscored
        // input-order result would fail every assertion below.
        let hits = search_state_entries(
            &[irrelevant, partial, relevant],
            "bm25 ranking",
            None,
            None,
            10,
        );
        assert_eq!(hits.len(), 2, "irrelevant entry must be excluded: {hits:?}");
        assert_eq!(hits[0].entry.slug, "relevant-note");
        assert_eq!(hits[1].entry.slug, "partial-note");
        assert!(
            hits[0].score_millis > hits[1].score_millis,
            "scores must descend: {} !> {}",
            hits[0].score_millis,
            hits[1].score_millis
        );
        assert!(hits[0].score_millis > 0);
    }

    #[test]
    fn search_pinned_boost_beats_otherwise_equal_unpinned() {
        let unpinned = memory_entry(
            4,
            "alpha-note",
            "shared summary words",
            "shared body 42terms token here",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let pinned = memory_entry(
            5,
            "bravo-note",
            "shared summary words",
            "shared body 42terms token here",
            true,
            MemoryLifecycle::Active,
            1_000,
        );
        let hits = search_state_entries(&[unpinned, pinned], "42terms", None, None, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entry.slug, "bravo-note", "pinned must rank first");
        assert!(
            hits[0].score_millis > hits[1].score_millis,
            "pinned boost must raise score: {} !> {}",
            hits[0].score_millis,
            hits[1].score_millis
        );
    }

    #[test]
    fn search_slug_match_beats_body_only_match() {
        let body_only = memory_entry(
            6,
            "plain-guide",
            "general notes here",
            "bm25 words only here",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let slug_match = memory_entry(
            7,
            "bm25-guide",
            "general notes here",
            "general words only here",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let hits = search_state_entries(&[body_only, slug_match], "bm25", None, None, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entry.slug, "bm25-guide");
        assert!(hits[0].score_millis > hits[1].score_millis);
    }

    #[test]
    fn search_scope_and_concern_filters_apply() {
        let mut mem_ws = memory_entry(
            8,
            "mem-workspace",
            "alpha summary",
            "quixotic shared body",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        mem_ws.scope = StateScope::Workspace;
        let mut mem_proj = memory_entry(
            9,
            "mem-project",
            "alpha summary",
            "quixotic shared body",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        mem_proj.scope = StateScope::Project("ferroloop".to_string());
        let mut hand_proj = handoff_entry(10, "hand-project", "alpha summary", "quixotic notes");
        hand_proj.scope = StateScope::Project("ferroloop".to_string());
        let entries = vec![mem_ws, mem_proj, hand_proj];

        let all = search_state_entries(&entries, "quixotic", None, None, 10);
        assert_eq!(all.len(), 3, "unfiltered search sees every match");

        let project = StateScope::Project("ferroloop".to_string());
        let filtered = search_state_entries(
            &entries,
            "quixotic",
            Some(&project),
            Some(StateConcern::Handoff),
            10,
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].entry.slug, "hand-project");
        assert_eq!(filtered[0].entry.concern, StateConcern::Handoff);
        assert_eq!(filtered[0].entry.scope, project);

        let ws_only =
            search_state_entries(&entries, "quixotic", Some(&StateScope::Workspace), None, 10);
        assert_eq!(ws_only.len(), 1);
        assert_eq!(ws_only[0].entry.slug, "mem-workspace");
    }

    #[test]
    fn search_empty_query_and_zero_limit_return_empty() {
        let entries = vec![memory_entry(
            11,
            "some-note",
            "some summary",
            "some body token",
            false,
            MemoryLifecycle::Active,
            1_000,
        )];
        for query in ["", "   ", "\t\n", "!!!", "---"] {
            assert!(
                search_state_entries(&entries, query, None, None, 10).is_empty(),
                "query {query:?} must produce no hits"
            );
        }
        assert!(search_state_entries(&entries, "token", None, None, 0).is_empty());
        assert!(search_state_entries(&[], "token", None, None, 10).is_empty());
    }

    #[test]
    fn search_matched_terms_snippet_and_estimated_tokens_are_sane() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);

        let filler = "filler ".repeat(80);
        let body = format!("{filler}FerroLoop uses Ranking for recall {filler}");
        let entry = memory_entry(
            12,
            "recall-note",
            "recall design summary",
            &body,
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let hits = search_state_entries(&[entry], "RANKING ferroloop ranking", None, None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matched_terms, vec!["ferroloop", "ranking"]);
        let snippet_lower = hits[0].snippet.to_ascii_lowercase();
        assert!(
            snippet_lower.contains("ferroloop") || snippet_lower.contains("ranking"),
            "snippet must contain a matched term: {:?}",
            hits[0].snippet
        );
        assert!(
            hits[0].snippet.chars().count() <= 200,
            "snippet must be a ~200-char window, got {} chars",
            hits[0].snippet.chars().count()
        );
        assert!(hits[0].estimated_tokens > 0);
        // estimated_tokens is ceil(searchable chars / 4), so it must at
        // least cover the body alone.
        assert!(hits[0].estimated_tokens >= estimate_tokens(&body));
    }

    #[test]
    fn search_retired_memory_is_deprioritized_vs_identical_active() {
        let retired = memory_entry(
            13,
            "alpha-note",
            "shared summary words",
            "shared body zephyr token here",
            false,
            MemoryLifecycle::Retired,
            1_000,
        );
        let active = memory_entry(
            14,
            "bravo-note",
            "shared summary words",
            "shared body zephyr token here",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let hits = search_state_entries(&[retired, active], "zephyr", None, None, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entry.slug, "bravo-note", "active must rank first");
        assert!(
            hits[0].score_millis > hits[1].score_millis,
            "retired penalty must lower score: {} !> {}",
            hits[0].score_millis,
            hits[1].score_millis
        );
    }

    #[test]
    fn search_tie_breaks_by_updated_at_desc_then_slug_asc() {
        let older = memory_entry(
            15,
            "alpha-note",
            "shared summary words",
            "shared body lapis token here",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let newer = memory_entry(
            16,
            "bravo-note",
            "shared summary words",
            "shared body lapis token here",
            false,
            MemoryLifecycle::Active,
            2_000,
        );
        let hits = search_state_entries(&[older, newer], "lapis", None, None, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].score_millis, hits[1].score_millis);
        assert_eq!(hits[0].entry.slug, "bravo-note", "newer entry wins the tie");

        let first = memory_entry(
            17,
            "bravo-note",
            "shared summary words",
            "shared body lapis token here",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let second = memory_entry(
            18,
            "alpha-note",
            "shared summary words",
            "shared body lapis token here",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let hits = search_state_entries(&[first, second], "lapis", None, None, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entry.slug, "alpha-note", "slug asc wins full tie");
    }

    #[test]
    fn search_runtime_json_value_is_searchable() {
        let entry = runtime_entry(
            19,
            "runtime-state",
            serde_json::json!({"mode": "quixoticroot", "count": 3}),
        );
        let hits = search_state_entries(&[entry], "quixoticroot", None, None, 10);
        assert_eq!(hits.len(), 1, "runtime JSON term must hit: {hits:?}");
        assert_eq!(hits[0].entry.slug, "runtime-state");
        assert_eq!(hits[0].entry.concern, StateConcern::Runtime);
        assert_eq!(hits[0].matched_terms, vec!["quixoticroot"]);
        assert!(hits[0].snippet.contains("quixoticroot"));
    }

    #[test]
    fn search_handoff_list_fields_are_searchable() {
        let mut entry = handoff_entry(20, "handoff-lists", "plain summary", "plain notes");
        if let StatePayload::Handoff(h) = &mut entry.payload {
            h.shipped = vec!["shippedterm done".to_string()];
            h.in_flight = vec!["flightterm ongoing".to_string()];
            h.blockers = vec!["blockerterm waiting".to_string()];
            h.next_steps = vec!["nextterm planned".to_string()];
            h.notes = None;
        } else {
            panic!("expected handoff payload");
        }
        for term in ["shippedterm", "flightterm", "blockerterm", "nextterm"] {
            let hits = search_state_entries(std::slice::from_ref(&entry), term, None, None, 10);
            assert_eq!(hits.len(), 1, "handoff list term {term} must hit: {hits:?}");
            assert_eq!(hits[0].entry.slug, "handoff-lists");
            assert_eq!(hits[0].matched_terms, vec![term]);
        }
    }

    #[test]
    fn search_summary_match_outranks_body_match_with_same_counts() {
        let body_hit = memory_entry(
            21,
            "alpha-note",
            "plain shared words",
            "weightsum body words here",
            false,
            MemoryLifecycle::Active,
            2_000,
        );
        let summary_hit = memory_entry(
            22,
            "zulu-note",
            "weightsum shared words",
            "shared body words here",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let hits = search_state_entries(&[body_hit, summary_hit], "weightsum", None, None, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].entry.slug, "zulu-note",
            "summary-weighted hit must rank above body-only hit"
        );
        assert_eq!(hits[1].entry.slug, "alpha-note");
        assert!(hits[0].score_millis > hits[1].score_millis);
    }

    #[test]
    fn search_limit_truncation_keeps_top_hit_only() {
        let weak = memory_entry(
            23,
            "weak-note",
            "general summary",
            "limitprobe appears once",
            false,
            MemoryLifecycle::Active,
            2_000,
        );
        let strong = memory_entry(
            24,
            "strong-note",
            "general summary",
            "limitprobe limitprobe limitprobe limitprobe",
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let entries = vec![weak, strong];
        let all = search_state_entries(&entries, "limitprobe", None, None, 10);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].entry.slug, "strong-note");
        let limited = search_state_entries(&entries, "limitprobe", None, None, 1);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].entry.slug, "strong-note", "limit keeps top hit");
        assert_eq!(limited[0].score_millis, all[0].score_millis);
    }

    #[test]
    fn search_sorts_by_raw_score_not_truncated_millis() {
        // Scores differ only by one filler token of length normalization,
        // so both truncate to the same score_millis. The shorter (raw
        // higher) doc is older and sorts later by slug, so only raw-score
        // ordering puts it first.
        let shorter_body = format!("{}lapiscore", "filler ".repeat(500));
        let longer_body = format!("{}lapiscore", "filler ".repeat(501));
        let longer = memory_entry(
            25,
            "alpha-note",
            "shared summary words",
            &longer_body,
            false,
            MemoryLifecycle::Active,
            2_000,
        );
        let shorter = memory_entry(
            26,
            "zulu-note",
            "shared summary words",
            &shorter_body,
            false,
            MemoryLifecycle::Active,
            1_000,
        );
        let hits = search_state_entries(&[longer, shorter], "lapiscore", None, None, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].score_millis, hits[1].score_millis,
            "setup must truncate to equal millis: {hits:?}"
        );
        assert_eq!(
            hits[0].entry.slug, "zulu-note",
            "raw higher score must win despite equal millis and losing tie-breaks"
        );
    }
}
