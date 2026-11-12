//! Portable, content-free documents for retained per-source usage.
use serde::{Deserialize, Serialize};
mod report;
pub use report::*;

pub const USAGE_PARSER_REVISION: u32 = 2;

/// Classify only the known Claude project layout; unrelated files must never
/// be discarded as duplicate usage or legacy workflow journals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeWorkflowPath {
    Subagent { canonical: String, workflow: bool },
    Journal,
}
pub fn claude_workflow_path(path: &str) -> Option<ClaudeWorkflowPath> {
    let parts: Vec<_> = path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    let i = parts
        .windows(2)
        .position(|w| w == [".claude", "projects"])?;
    let tail = &parts[i + 2..];
    if tail.len() < 4 || tail[2] != "subagents" {
        return None;
    }
    let uuid = tail[1];
    if uuid.len() != 36
        || uuid.bytes().enumerate().any(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b != b'-'
            } else {
                !b.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    let (file, workflow) = match &tail[3..] {
        [file] => (*file, false),
        ["workflows", wf, file] if wf.starts_with("wf_") => (*file, true),
        _ => return None,
    };
    if workflow && file == "journal.jsonl" {
        return Some(ClaudeWorkflowPath::Journal);
    }
    if !file.starts_with("agent-") || !file.ends_with(".jsonl") {
        return None;
    }
    let canonical = format!("{}/{}/{}/{}", parts[..i].join("/"), tail[0], uuid, file);
    Some(ClaudeWorkflowPath::Subagent {
        canonical,
        workflow,
    })
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AntigravityLaunch {
    pub session_id: String,
    pub launch_time_ms: u64,
}

// Filters were originally persisted as arrays of JSON numbers. Keep reading
// those documents while encoding new writes as compact base64 little-endian words.
mod compact_filter {
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    fn encode(bytes: &[u8]) -> String {
        let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let n = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            encoded.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            encoded.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            encoded.push(if chunk.len() > 1 {
                ALPHABET[((n >> 6) & 63) as usize] as char
            } else {
                '='
            });
            encoded.push(if chunk.len() > 2 {
                ALPHABET[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        encoded
    }
    pub fn serialize<S: Serializer>(words: &[u64], serializer: S) -> Result<S::Ok, S::Error> {
        let dense_len = words.len() * 8;
        let nonzero = words.iter().filter(|&&word| word != 0).count();
        if !words.is_empty() && 2 + nonzero * 10 < dense_len {
            let mut sparse = Vec::with_capacity(2 + nonzero * 10);
            sparse.extend_from_slice(&(words.len() as u16).to_le_bytes());
            for (index, &word) in words.iter().enumerate().filter(|(_, word)| **word != 0) {
                sparse.extend_from_slice(&(index as u16).to_le_bytes());
                sparse.extend_from_slice(&word.to_le_bytes());
            }
            format!("s:{}", encode(&sparse)).serialize(serializer)
        } else {
            let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
            encode(&bytes).serialize(serializer)
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u64>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Encoded {
            Compact(String),
            Legacy(Vec<u64>),
        }
        let words = match Encoded::deserialize(deserializer)? {
            Encoded::Legacy(words) => words,
            Encoded::Compact(encoded) => {
                let (sparse, encoded) = match encoded.strip_prefix("s:") {
                    Some(rest) => (true, rest),
                    None => (false, encoded.as_str()),
                };
                if encoded.len() > 87_384 || encoded.len() % 4 != 0 {
                    return Err(D::Error::custom("invalid usage filter length"));
                }
                let decode = |c: u8| ALPHABET.iter().position(|x| *x == c).map(|x| x as u32);
                let mut bytes = Vec::with_capacity(encoded.len() / 4 * 3);
                let chunks = encoded.as_bytes().as_chunks::<4>().0;
                for (i, chunk) in chunks.iter().enumerate() {
                    let a = decode(chunk[0])
                        .ok_or_else(|| D::Error::custom("invalid usage filter encoding"))?;
                    let b = decode(chunk[1])
                        .ok_or_else(|| D::Error::custom("invalid usage filter encoding"))?;
                    let pad2 = chunk[2] == b'=';
                    let pad3 = chunk[3] == b'=';
                    if (pad2 && !pad3) || ((pad2 || pad3) && i + 1 != chunks.len()) {
                        return Err(D::Error::custom("invalid usage filter padding"));
                    }
                    let c = if pad2 {
                        0
                    } else {
                        decode(chunk[2])
                            .ok_or_else(|| D::Error::custom("invalid usage filter encoding"))?
                    };
                    let d = if pad3 {
                        0
                    } else {
                        decode(chunk[3])
                            .ok_or_else(|| D::Error::custom("invalid usage filter encoding"))?
                    };
                    if (pad2 && b & 15 != 0) || (pad3 && !pad2 && c & 3 != 0) {
                        return Err(D::Error::custom("invalid usage filter padding"));
                    }
                    let n = (a << 18) | (b << 12) | (c << 6) | d;
                    bytes.push((n >> 16) as u8);
                    if !pad2 {
                        bytes.push((n >> 8) as u8);
                    }
                    if !pad3 {
                        bytes.push(n as u8);
                    }
                }
                if sparse {
                    if bytes.len() < 2 || (bytes.len() - 2) % 10 != 0 {
                        return Err(D::Error::custom("invalid sparse usage filter"));
                    }
                    let count = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
                    if count == 0 || count > 8192 {
                        return Err(D::Error::custom("invalid sparse usage filter length"));
                    }
                    let mut words = vec![0; count];
                    for item in bytes[2..].as_chunks::<10>().0 {
                        let index = u16::from_le_bytes([item[0], item[1]]) as usize;
                        if index >= count || words[index] != 0 {
                            return Err(D::Error::custom("invalid sparse usage filter index"));
                        }
                        let word =
                            u64::from_le_bytes(item[2..].try_into().map_err(D::Error::custom)?);
                        if word == 0 {
                            return Err(D::Error::custom("invalid sparse usage filter word"));
                        }
                        words[index] = word;
                    }
                    words
                } else {
                    if bytes.len() % 8 != 0 {
                        return Err(D::Error::custom("invalid usage filter length"));
                    }
                    bytes
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .copied()
                        .map(u64::from_le_bytes)
                        .collect()
                }
            }
        };
        if words.len() > 8192 {
            return Err(D::Error::custom("usage filter too large"));
        }
        Ok(words)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParserState {
    #[serde(default)]
    pub parser_revision: Option<u32>,
    #[serde(default)]
    pub observed_len: Option<u64>,
    #[serde(default)]
    pub observed_mtime_ns: Option<u64>,
    pub codex_totals: Option<TokenCounts>,
    pub codex_unsafe: bool,
    #[serde(default)]
    pub codex_baseline_only: bool,
    #[serde(default)]
    pub codex_fork_start_ordinal: Option<u64>,
    #[serde(default)]
    pub codex_fork_unbounded: bool,
    #[serde(default)]
    pub codex_parent_id: Option<String>,
    #[serde(default)]
    pub codex_launch_time_ms: Option<u64>,
    #[serde(default)]
    pub codex_fork_baseline_seen: bool,
    #[serde(default)]
    pub codex_model_changed: bool,
    #[serde(default)]
    pub antigravity_last_step: Option<u64>,
    #[serde(default)]
    pub antigravity_last_planner_ms: Option<u64>,
    #[serde(default)]
    pub antigravity_last_planner_step: Option<u64>,
    #[serde(default)]
    pub antigravity_unsafe: bool,
    #[serde(default, with = "compact_filter")]
    pub antigravity_seen: Vec<u64>,
    #[serde(default)]
    pub antigravity_parents: Vec<String>,
    #[serde(default)]
    pub antigravity_launches: Vec<AntigravityLaunch>,
    #[serde(default, with = "compact_filter")]
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

#[cfg(test)]
mod report_tests {
    use super::*;
    fn doc(
        session: &str,
        vendor: UsageVendor,
        hour: Option<u64>,
        count: Option<u64>,
    ) -> SourceUsageDocument {
        SourceUsageDocument {
            source_id: session.into(),
            session_id: session.into(),
            vendor,
            offset: 0,
            generation: 0,
            file_identity: None,
            anchor: String::new(),
            discarding_line: false,
            parser: ParserState {
                parser_revision: Some(USAGE_PARSER_REVISION),
                ..Default::default()
            },
            coverage: Default::default(),
            buckets: vec![UsageBucket {
                model: Some("m".into()),
                hour_ms: hour,
                counts: TokenCounts {
                    input: count,
                    output: Some(0),
                    cache_read: None,
                    cache_write: None,
                },
                turns: 0,
                turns_unavailable: true,
                unavailable: CategoryUnavailable {
                    input: count.is_none(),
                    output: false,
                    cache_read: true,
                    cache_write: true,
                },
                missing: Default::default(),
                observations: 1,
            }],
        }
    }
    fn mapping(
        id: &str,
        persona: Option<&str>,
        parent: Option<&str>,
        launch: Option<u64>,
    ) -> IdentityMappingDocument {
        IdentityMappingDocument {
            session_id: id.into(),
            persona: persona.map(str::to_owned),
            parent_id: parent.map(str::to_owned),
            launch_time_ms: launch,
            provenance: "test".into(),
        }
    }
    #[test]
    fn rounded_window_and_late_identity_preserve_unknown_and_undated() {
        let docs = vec![
            doc("child", UsageVendor::Antigravity, Some(3_600_000), Some(10)),
            doc("orphan", UsageVendor::Claude, None, Some(7)),
        ];
        let identities = vec![
            mapping("child", None, Some("parent"), Some(3_700_000)),
            mapping("parent", Some("Felix"), None, None),
        ];
        let query = UsageReportQuery {
            since_ms: 3_700_000,
            by: UsageGroupBy::Persona,
        };
        let report = aggregate_documents(&docs, &identities, None, &query, 4_000_000).unwrap();
        assert_eq!(
            (report.requested_start_ms, report.requested_end_ms),
            (3_700_000, 4_000_000)
        );
        assert_eq!(
            (report.effective_start_ms, report.effective_end_ms),
            (3_600_000, 7_200_000)
        );
        let row = report.rows.iter().find(|r| r.key == "Felix").unwrap();
        assert_eq!(row.input, Some(10));
        assert_eq!(row.output, Some(0));
        assert_eq!(row.cache_read, None);
        assert_eq!(row.subagents, 1);
        assert_eq!(report.undated.input, Some(7));
        let no_mapping = aggregate_documents(&docs, &[], None, &query, 4_000_000).unwrap();
        assert_eq!(
            no_mapping
                .rows
                .iter()
                .find(|r| r.key == "[unattributed]")
                .unwrap()
                .input,
            Some(10)
        );
    }
    #[test]
    fn unavailable_turns_are_not_reported_as_zero() {
        let missing = doc("missing", UsageVendor::Codex, Some(0), Some(1));
        let query = UsageReportQuery {
            since_ms: 1,
            by: UsageGroupBy::Vendor,
        };
        let report =
            aggregate_documents(std::slice::from_ref(&missing), &[], None, &query, 100).unwrap();
        assert_eq!(report.rows[0].turns, None);
        assert!(report.rows[0].turns_unavailable);
        let mut known = doc("known", UsageVendor::Codex, Some(0), Some(1));
        known.buckets[0].turns = 3;
        known.buckets[0].turns_unavailable = false;
        let report = aggregate_documents(&[missing, known], &[], None, &query, 100).unwrap();
        assert_eq!(report.rows[0].turns, Some(3));
        assert!(report.rows[0].turns_unavailable);
    }
    #[test]
    fn many_deep_parent_mappings_resolve_without_quadratic_work() {
        let n = 10_000;
        let mut ids = Vec::new();
        let mut docs = Vec::new();
        ids.push(mapping("root", Some("Felix"), None, None));
        for i in 0..n {
            let id = format!("child-{i}");
            let parent = if i == 0 {
                "root".to_string()
            } else {
                format!("child-{}", i - 1)
            };
            ids.push(mapping(&id, None, Some(&parent), Some(10)));
            docs.push(doc(&id, UsageVendor::Codex, Some(0), Some(1)));
        }
        let report = aggregate_documents(
            &docs,
            &ids,
            None,
            &UsageReportQuery {
                since_ms: 1,
                by: UsageGroupBy::Persona,
            },
            100,
        )
        .unwrap();
        assert_eq!(report.rows[0].key, "Felix");
        assert_eq!(report.rows[0].input, Some(n));
        assert_eq!(report.rows[0].subagents, n);
    }
    #[test]
    fn aligned_window_overflow_and_invalid_input() {
        let docs = vec![
            doc("a", UsageVendor::Codex, Some(3_600_000), Some(u64::MAX)),
            doc("b", UsageVendor::Codex, Some(3_600_000), Some(2)),
        ];
        let report = aggregate_documents(
            &docs,
            &[],
            None,
            &UsageReportQuery {
                since_ms: 3_600_000,
                by: UsageGroupBy::Vendor,
            },
            7_200_000,
        )
        .unwrap();
        assert_eq!(report.effective_start_ms, 3_600_000);
        assert_eq!(report.effective_end_ms, 7_200_000);
        assert!(report.coverage.incomplete);
        assert_eq!(report.rows[0].input, Some(u64::MAX));
        assert!(parse_since("0h", 10_000_000).is_err());
        assert!(parse_since("nonsense", 10_000_000).is_err());
        assert!(
            aggregate_documents(
                &[],
                &[],
                None,
                &UsageReportQuery {
                    since_ms: 7_200_000,
                    by: UsageGroupBy::Model
                },
                7_200_000
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod store_report_tests {
    use super::*;
    #[test]
    fn ferro_reads_isolated_documents_and_redb_is_explicitly_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let options = fl_store::StoreOptions {
            backend: Some(fl_store::BackendKind::Ferro),
            ..Default::default()
        };
        let store = fl_store::open_store_with(&dir.path().join("w.fls"), &options).unwrap();
        let ferro = store.as_ferro().unwrap();
        let source = SourceUsageDocument {
            source_id: "grok".into(),
            session_id: "grok".into(),
            vendor: UsageVendor::Grok,
            offset: 0,
            generation: 0,
            file_identity: None,
            anchor: String::new(),
            discarding_line: false,
            parser: ParserState {
                parser_revision: Some(USAGE_PARSER_REVISION),
                observed_mtime_ns: Some(150_000_000),
                ..Default::default()
            },
            buckets: vec![],
            coverage: UsageCoverage {
                unavailable: true,
                ..Default::default()
            },
        };
        ferro
            .put_doc(
                COLLECTION,
                "x",
                &serde_json::json!({"kind":"source","document":source}),
            )
            .unwrap();
        ferro.put_doc(COLLECTION,"collector-status",&serde_json::json!({"kind":"status","last_success_ms":100,"pending":false,"incomplete":false})).unwrap();
        let report = aggregate_usage(
            &store,
            &UsageReportQuery {
                since_ms: 100,
                by: UsageGroupBy::Vendor,
            },
            200,
        )
        .unwrap();
        assert_eq!(report.rows[0].key, "grok");
        assert_eq!(report.rows[0].input, None);
        assert_eq!(report.coverage.unavailable_sources, 1);
        assert_eq!(report.coverage.last_success_ms, Some(100));
        let options = fl_store::StoreOptions {
            backend: Some(fl_store::BackendKind::Redb),
            ..Default::default()
        };
        let store = fl_store::open_store_with(&dir.path().join("legacy.redb"), &options).unwrap();
        assert!(matches!(
            aggregate_usage(
                &store,
                &UsageReportQuery {
                    since_ms: 100,
                    by: UsageGroupBy::Persona
                },
                200
            ),
            Err(UsageError::Unsupported)
        ));
    }
    #[test]
    fn timestamps_and_positive_durations_are_validated() {
        let now = 1_700_000_000_000;
        assert_eq!(parse_since("48h", now).unwrap(), now - 172_800_000);
        assert_eq!(
            parse_since("2023-11-14T22:13:19Z", now).unwrap(),
            now - 1_000
        );
        assert!(parse_since("2023-11-14T22:13:20Z", now).is_err());
        assert!(parse_since("999999999999999999999999h", now).is_err());
    }
}

#[cfg(test)]
mod partial_report_tests {
    use super::*;
    #[test]
    fn mixed_unknown_and_known_categories_keep_the_known_partial_total() {
        let mk = |id: &str, count: Option<u64>| SourceUsageDocument {
            source_id: id.into(),
            session_id: id.into(),
            vendor: UsageVendor::Codex,
            offset: 0,
            generation: 0,
            file_identity: None,
            anchor: String::new(),
            discarding_line: false,
            parser: ParserState {
                parser_revision: Some(USAGE_PARSER_REVISION),
                ..Default::default()
            },
            coverage: Default::default(),
            buckets: vec![UsageBucket {
                model: None,
                hour_ms: Some(0),
                counts: TokenCounts {
                    input: count,
                    ..Default::default()
                },
                turns: 0,
                turns_unavailable: true,
                unavailable: CategoryUnavailable {
                    input: count.is_none(),
                    output: true,
                    cache_read: true,
                    cache_write: true,
                },
                missing: Default::default(),
                observations: 1,
            }],
        };
        let report = aggregate_documents(
            &[mk("a", Some(42)), mk("b", None)],
            &[],
            None,
            &UsageReportQuery {
                since_ms: 1,
                by: UsageGroupBy::Vendor,
            },
            100,
        )
        .unwrap();
        assert_eq!(report.rows[0].input, Some(42));
        assert!(report.rows[0].unavailable.input);
    }
}
