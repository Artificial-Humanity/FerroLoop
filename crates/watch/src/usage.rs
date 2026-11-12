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
    pub codex_model_changed: bool,
    #[serde(default)]
    pub antigravity_last_step: Option<u64>,
    #[serde(default)]
    pub antigravity_unsafe: bool,
    #[serde(default)]
    pub antigravity_seen: Vec<u64>,
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
