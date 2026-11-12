//! Portable, content-free documents for retained per-source usage.
use serde::{Deserialize, Serialize};
mod report;
pub use report::*;

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
            parser: Default::default(),
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
                .find(|r| r.key == "unattributed")
                .unwrap()
                .input,
            Some(10)
        );
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
            parser: Default::default(),
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
            parser: Default::default(),
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
