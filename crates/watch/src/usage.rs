//! Portable, content-free documents for retained per-source usage.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageVendor {
    Antigravity,
    Codex,
    Claude,
    Muse,
    Grok,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryUnavailable {
    pub input: bool,
    pub output: bool,
    pub cache_read: bool,
    pub cache_write: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingCategoryCounts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageBucket {
    pub model: Option<String>,
    pub hour_ms: Option<u64>,
    pub counts: TokenCounts,
    pub turns: u64,
    pub turns_unavailable: bool,
    pub unavailable: CategoryUnavailable,
    #[serde(default)]
    pub missing: MissingCategoryCounts,
    #[serde(default)]
    pub observations: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeBaseline {
    pub message_id: String,
    pub model: Option<String>,
    pub hour_ms: Option<u64>,
    pub counts: TokenCounts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParserState {
    pub codex_totals: Option<TokenCounts>,
    pub codex_unsafe: bool,
    #[serde(default)]
    pub codex_baseline_only: bool,
    #[serde(default)]
    pub codex_model_changed: bool,
    #[serde(default)]
    pub antigravity_last_step: Option<u64>,
    #[serde(default)]
    pub antigravity_unsafe: bool,
    #[serde(default)]
    pub antigravity_seen: Vec<u64>,
    #[serde(default)]
    pub antigravity_parents: Vec<String>,
    #[serde(default)]
    pub claude_seen: Vec<u64>,
    pub model: Option<String>,
    pub claude_recent: Vec<ClaudeBaseline>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCoverage {
    pub incomplete: bool,
    pub pending: bool,
    pub malformed_lines: u64,
    pub oversized_lines: u64,
    pub ambiguous_generation: bool,
    pub unavailable: bool,
    #[serde(default)]
    pub unverified_prefix: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceUsageDocument {
    pub source_id: String,
    pub session_id: String,
    pub vendor: UsageVendor,
    pub offset: u64,
    pub generation: u64,
    pub file_identity: Option<String>,
    pub anchor: String,
    pub discarding_line: bool,
    pub parser: ParserState,
    pub buckets: Vec<UsageBucket>,
    pub coverage: UsageCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityMappingDocument {
    pub session_id: String,
    pub persona: Option<String>,
    pub parent_id: Option<String>,
    pub launch_time_ms: Option<u64>,
    pub provenance: String,
}

/// Identity joins remain independent of the source usage documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribution {
    Persona(String),
    Unattributed,
    Ambiguous,
}

pub fn resolve_persona(session: &str, mappings: &[IdentityMappingDocument]) -> Attribution {
    use std::collections::HashSet;
    let mut current = session;
    let mut resolved: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current.to_owned()) {
            return Attribution::Ambiguous;
        }
        let mut persona = None;
        let mut parent = None;
        let mut found = false;
        for entry in mappings.iter().filter(|m| m.session_id == current) {
            found = true;
            if let Some(p) = entry.persona.as_deref() {
                if persona.is_some_and(|old| old != p) {
                    return Attribution::Ambiguous;
                }
                persona = Some(p);
            }
            if let Some(p) = entry.parent_id.as_deref() {
                if parent.is_some_and(|old| old != p) {
                    return Attribution::Ambiguous;
                }
                parent = Some(p);
            }
        }
        if let Some(p) = persona {
            if resolved.as_deref().is_some_and(|old| old != p) {
                return Attribution::Ambiguous;
            }
            resolved = Some(p.to_owned());
        }
        if !found || parent.is_none() {
            return resolved.map_or(Attribution::Unattributed, Attribution::Persona);
        }
        current = parent.unwrap();
    }
}

#[cfg(test)]
mod attribution_tests {
    use super::*;
    fn identity(id: &str, persona: Option<&str>, parent: Option<&str>) -> IdentityMappingDocument {
        IdentityMappingDocument {
            session_id: id.into(),
            persona: persona.map(str::to_owned),
            parent_id: parent.map(str::to_owned),
            launch_time_ms: None,
            provenance: "fixture".into(),
        }
    }
    #[test]
    fn late_parent_and_persona_resolve_without_rewriting_usage() {
        let mut mappings = vec![identity("child", None, Some("parent"))];
        assert_eq!(
            resolve_persona("child", &mappings),
            Attribution::Unattributed
        );
        mappings.push(identity("parent", Some("Felix"), None));
        assert_eq!(
            resolve_persona("child", &mappings),
            Attribution::Persona("Felix".into())
        );
    }
    #[test]
    fn cycles_and_conflicting_evidence_are_never_guessed() {
        let cycle = vec![
            identity("a", None, Some("b")),
            identity("b", None, Some("a")),
        ];
        assert_eq!(resolve_persona("a", &cycle), Attribution::Ambiguous);
        let conflict = vec![
            identity("a", Some("Felix"), None),
            identity("a", Some("Ferris"), None),
        ];
        assert_eq!(resolve_persona("a", &conflict), Attribution::Ambiguous);
    }
}
