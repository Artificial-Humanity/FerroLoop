use super::scan::{ScanLimits, UsageSource, scan_source};
use fl_store::AnyStore;
use fl_watch::usage::{IdentityMappingDocument, SourceUsageDocument, UsageVendor};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

pub const COLLECTION: &str = "watch_usage_v1";
static COLLECTING: Mutex<()> = Mutex::new(());

fn key(prefix: &str, id: &str) -> String {
    format!("{prefix}-{:x}", Sha256::digest(id.as_bytes()))
}

pub fn read_sources(store: &AnyStore) -> anyhow::Result<Vec<SourceUsageDocument>> {
    let ferro = store
        .as_ferro()
        .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
    let mut result = Vec::new();
    for id in ferro.query_by_field(COLLECTION, "kind", "source")? {
        if let Some(value) = ferro.get_doc(COLLECTION, &id)? {
            result.push(serde_json::from_value(value["document"].clone())?);
        }
    }
    Ok(result)
}

pub fn read_identities(store: &AnyStore) -> anyhow::Result<Vec<IdentityMappingDocument>> {
    let ferro = store
        .as_ferro()
        .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
    let mut result = Vec::new();
    for id in ferro.query_by_field(COLLECTION, "kind", "identity")? {
        if let Some(value) = ferro.get_doc(COLLECTION, &id)? {
            result.push(serde_json::from_value(value["document"].clone())?);
        }
    }
    Ok(result)
}

fn put_identity(store: &AnyStore, identity: &IdentityMappingDocument) -> anyhow::Result<bool> {
    let ferro = store
        .as_ferro()
        .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
    let id = key(
        "identity",
        &format!("{}\0{}", identity.session_id, identity.provenance),
    );
    let document = serde_json::json!({"kind":"identity", "document":identity});
    if ferro.get_doc(COLLECTION, &id)?.as_ref() == Some(&document) {
        return Ok(false);
    }
    ferro.put_doc(COLLECTION, &id, &document)?;
    Ok(true)
}

fn read_source(
    store: &AnyStore,
    source: &UsageSource,
) -> anyhow::Result<Option<SourceUsageDocument>> {
    let ferro = store
        .as_ferro()
        .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
    ferro
        .get_doc(COLLECTION, &key("source", &source.source_id))?
        .map(|v| serde_json::from_value(v["document"].clone()).map_err(Into::into))
        .transpose()
}

fn save_candidate(
    store: &AnyStore,
    source: &UsageSource,
    old: Option<&SourceUsageDocument>,
    candidate: Result<SourceUsageDocument, super::scan::UsageScanError>,
) -> anyhow::Result<()> {
    let ferro = store
        .as_ferro()
        .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
    let candidate = match candidate {
        Ok(doc) => doc,
        Err(super::scan::UsageScanError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(_) => {
            ferro.put_doc(
                COLLECTION,
                &key("source-error", &source.source_id),
                &serde_json::json!({"kind":"source_error"}),
            )?;
            return Ok(());
        }
    };
    let error_id = key("source-error", &source.source_id);
    if ferro.get_doc(COLLECTION, &error_id)?.is_some() {
        ferro.delete_doc(COLLECTION, &error_id)?;
    }
    if old != Some(&candidate) {
        ferro.put_doc(
            COLLECTION,
            &key("source", &source.source_id),
            &serde_json::json!({"kind":"source", "document":candidate}),
        )?;
    }
    for launch in &candidate.parser.antigravity_launches {
        put_identity(
            store,
            &IdentityMappingDocument {
                session_id: launch.session_id.clone(),
                persona: None,
                parent_id: Some(candidate.session_id.clone()),
                launch_time_ms: Some(launch.launch_time_ms),
                provenance: format!(
                    "antigravity-launch:{}:{}",
                    candidate.session_id, launch.launch_time_ms
                ),
            },
        )?;
    }
    for parent in &candidate.parser.antigravity_parents {
        put_identity(
            store,
            &IdentityMappingDocument {
                session_id: candidate.session_id.clone(),
                persona: None,
                parent_id: Some(parent.clone()),
                launch_time_ms: None,
                provenance: format!("antigravity-parent:{parent}"),
            },
        )?;
    }
    Ok(())
}

/// Collect on a standalone store. Hub production uses short store-guard phases below.
pub fn collect_once(
    store: &AnyStore,
    sources: &[UsageSource],
    identities: &[IdentityMappingDocument],
    limits: ScanLimits,
) -> anyhow::Result<()> {
    let _flight = COLLECTING.lock().unwrap_or_else(|e| e.into_inner());
    // Validate even when both inputs are empty.
    store
        .as_ferro()
        .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
    for identity in identities {
        put_identity(store, identity)?;
    }
    for source in sources {
        let old = read_source(store, source)?;
        let candidate = scan_source(source, old.as_ref(), limits);
        save_candidate(store, source, old.as_ref(), candidate)?;
    }
    Ok(())
}

/// Enumerate only canonical files; the full Antigravity sibling overlaps the compact log.
fn discover_usage_sources(home: &std::path::Path) -> (Vec<UsageSource>, bool) {
    let roots = [
        (
            home.join(".gemini/antigravity-cli/brain"),
            UsageVendor::Antigravity,
        ),
        (home.join(".codex/sessions"), UsageVendor::Codex),
        (home.join(".claude/projects"), UsageVendor::Claude),
        (home.join(".local/share/muse/sessions"), UsageVendor::Muse),
        (home.join(".grok/sessions"), UsageVendor::Grok),
    ];
    let mut sources = Vec::new();
    let mut incomplete = false;
    for (root, vendor) in roots {
        match root.try_exists() {
            Ok(false) => continue,
            Err(_) => {
                incomplete = true;
                continue;
            }
            Ok(true) => {}
        }
        for (index, entry) in walkdir::WalkDir::new(&root)
            .max_depth(8)
            .into_iter()
            .enumerate()
        {
            if index >= 100_000 {
                incomplete = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    incomplete = true;
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let session = match vendor {
                UsageVendor::Antigravity if name == "transcript.jsonl" => path
                    .strip_prefix(&root)
                    .ok()
                    .and_then(|p| p.components().next())
                    .and_then(|p| p.as_os_str().to_str())
                    .map(str::to_owned),
                UsageVendor::Codex if name.starts_with("rollout-") && name.ends_with(".jsonl") => {
                    name.strip_suffix(".jsonl")
                        .and_then(|n| n.get(n.len().saturating_sub(36)..))
                        .and_then(|n| uuid::Uuid::parse_str(n).ok())
                        .map(|u| u.to_string())
                }
                UsageVendor::Claude if name.ends_with(".jsonl") => {
                    path.file_stem().and_then(|n| n.to_str()).map(str::to_owned)
                }
                UsageVendor::Muse if name == "session.jsonl" => path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(str::to_owned),
                UsageVendor::Grok
                    if path.parent() == Some(root.as_path()) && name.ends_with(".jsonl") =>
                {
                    path.file_stem().and_then(|n| n.to_str()).map(str::to_owned)
                }
                UsageVendor::Grok if name == "chat_history.jsonl" => path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(str::to_owned),
                _ => None,
            };
            if let Some(session_id) = session {
                sources.push(UsageSource {
                    path: path.to_owned(),
                    source_id: path.to_string_lossy().into_owned(),
                    session_id,
                    vendor,
                });
            }
        }
    }
    sources.sort_by(|a, b| a.source_id.cmp(&b.source_id));
    (sources, incomplete)
}

/// Run one bounded sweep in the hub's background task. The rotating start prevents
/// a busy source or a long source list from starving later conversations.
pub fn collect_hub(
    state: &crate::HubState,
    start: usize,
    prior_pending: bool,
) -> anyhow::Result<(usize, bool)> {
    collect_hub_with_hook(state, start, prior_pending, || {})
}

fn collect_hub_with_hook(
    state: &crate::HubState,
    start: usize,
    prior_pending: bool,
    mut hook: impl FnMut(),
) -> anyhow::Result<(usize, bool)> {
    let home = state
        .config
        .home_override
        .clone()
        .or_else(|| std::env::var_os("HOME").map(Into::into))
        .ok_or_else(|| anyhow::anyhow!("usage source home unavailable"))?;
    // The flight spans snapshots, scans, and writes, while each store guard is brief.
    let _flight = COLLECTING.lock().unwrap_or_else(|e| e.into_inner());
    let (sources, enumeration_incomplete) = discover_usage_sources(&home);
    let (snap, hosted) = {
        let store = state.open_store()?;
        store
            .as_ferro()
            .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
        (
            crate::discovery::snapshot_discovery_store(&store),
            crate::lifecycle::list_hosted_sessions(&*store).unwrap_or_default(),
        )
    };
    let mut identities: Vec<_> =
        crate::discovery::usage_route_claims(&snap, state.config.home_override.as_deref())
            .into_iter()
            .map(|(session_id, persona, vendor)| IdentityMappingDocument {
                session_id,
                persona: Some(persona.clone()),
                parent_id: None,
                launch_time_ms: None,
                provenance: format!("route:{vendor}:{persona}"),
            })
            .collect();
    for record in hosted {
        let mut native_ids = crate::lifecycle::record_native_ids(&record);
        if let Some(id) = record.vendor_session_id.as_deref()
            && uuid::Uuid::parse_str(id).is_ok()
            && !native_ids.iter().any(|old| old == id)
        {
            native_ids.push(id.to_owned());
        }
        for id in native_ids {
            identities.push(IdentityMappingDocument {
                session_id: id,
                persona: Some(record.persona.clone()),
                parent_id: None,
                launch_time_ms: None,
                provenance: format!("hosted:{}:{}", record.vendor, record.persona),
            });
        }
    }
    {
        let store = state.open_store()?;
        for identity in &identities {
            put_identity(&store, identity)?;
        }
    }
    let start = if start >= sources.len() { 0 } else { start };
    let end = (start + 8).min(sources.len());
    let selected = &sources[start..end];
    for source in selected {
        let old = {
            let store = state.open_store()?;
            read_source(&store, source)?
        };
        hook();
        let candidate = scan_source(source, old.as_ref(), ScanLimits::default());
        let store = state.open_store()?;
        save_candidate(&store, source, old.as_ref(), candidate)?;
    }
    let store = state.open_store()?;
    let next = if end >= sources.len() { 0 } else { end };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let ferro = store
        .as_ferro()
        .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
    let mut batch_pending = false;
    for source in selected {
        let value = ferro.get_doc(COLLECTION, &key("source", &source.source_id))?;
        batch_pending |= value
            .and_then(|v| v["document"]["coverage"]["pending"].as_bool())
            .unwrap_or(true);
    }
    let cumulative = (start != 0 && prior_pending) || batch_pending;
    ferro.put_doc(COLLECTION, "collector-status", &serde_json::json!({"kind":"status", "last_success_ms":now, "pending":next != 0 || cumulative, "incomplete":enumeration_incomplete || !ferro.query_by_field(COLLECTION, "kind", "source_error")?.is_empty()}))?;
    Ok((next, cumulative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn store() -> (tempfile::TempDir, fl_store::AnyStore) {
        let dir = tempfile::tempdir().unwrap();
        let opts = fl_store::StoreOptions {
            backend: Some(fl_store::BackendKind::Ferro),
            ..Default::default()
        };
        let store = fl_store::open_store_with(&dir.path().join("test.fls"), &opts).unwrap();
        (dir, store)
    }
    #[test]
    fn identical_identity_is_not_written_again() {
        let (_dir, store) = store();
        let identity = IdentityMappingDocument {
            session_id: "child".into(),
            persona: Some("Felix".into()),
            parent_id: None,
            launch_time_ms: None,
            provenance: "route".into(),
        };
        assert!(put_identity(&store, &identity).unwrap());
        assert!(!put_identity(&store, &identity).unwrap());
        let changed = IdentityMappingDocument {
            launch_time_ms: Some(1794313800000),
            ..identity
        };
        assert!(put_identity(&store, &changed).unwrap());
    }
    #[test]
    fn discovered_launch_result_counts_once_in_window() {
        let (_dir, store) = store();
        let home = tempfile::tempdir().unwrap();
        let logs = home
            .path()
            .join(".gemini/antigravity-cli/brain/parent/.system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        let mut file = std::fs::File::create(logs.join("transcript.jsonl")).unwrap();
        let child = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let content = format!(
            "Created At: 2026-11-10T12:30:00Z\nCompleted At: 2026-11-10T12:30:01Z\nCreated the following subagent:\n{{\n  \"conversationId\": \"{child}\",\n  \"logAbsoluteUri\": \"private text\",\n  \"workspaceUris\": []\n}}\nThe subagent will send you a message."
        );
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"GENERIC", "content":content})
        )
        .unwrap();
        drop(file);
        let (mut sources, incomplete) = discover_usage_sources(home.path());
        assert!(!incomplete);
        assert_eq!(sources.len(), 1);
        let source = sources.pop().unwrap();
        assert_eq!(source.session_id, "parent");
        let identity = IdentityMappingDocument {
            session_id: "parent".into(),
            persona: Some("Felix".into()),
            parent_id: None,
            launch_time_ms: None,
            provenance: "route".into(),
        };
        collect_once(
            &store,
            std::slice::from_ref(&source),
            std::slice::from_ref(&identity),
            ScanLimits::default(),
        )
        .unwrap();
        collect_once(&store, &[source], &[identity], ScanLimits::default()).unwrap();
        let sources = read_sources(&store).unwrap();
        let identities = read_identities(&store).unwrap();
        assert!(
            !serde_json::to_string(&identities)
                .unwrap()
                .contains("private text")
        );
        assert!(identities.iter().any(|m| m.session_id == child
            && m.parent_id.as_deref() == Some("parent")
            && m.launch_time_ms == Some(1794313800000)));
        let query = fl_watch::usage::UsageReportQuery {
            since_ms: 1794312000000,
            by: fl_watch::usage::UsageGroupBy::Persona,
        };
        let report = fl_watch::usage::aggregate_documents(
            &sources,
            &identities,
            None,
            &query,
            1794315600000,
        )
        .unwrap();
        assert_eq!(
            report
                .rows
                .iter()
                .find(|r| r.key == "Felix")
                .unwrap()
                .subagents,
            1
        );
        assert_eq!(report.coverage.unknown_launch_times, 0);

        // The child has no transcript yet; the launch result still identifies its vendor.
        let vendor_query = fl_watch::usage::UsageReportQuery {
            by: fl_watch::usage::UsageGroupBy::Vendor,
            ..query
        };
        let vendor_report = fl_watch::usage::aggregate_documents(
            &sources,
            &identities,
            None,
            &vendor_query,
            1794315600000,
        )
        .unwrap();
        assert_eq!(
            vendor_report
                .rows
                .iter()
                .find(|r| r.key == "antigravity")
                .unwrap()
                .subagents,
            1
        );
        assert!(
            !vendor_report
                .rows
                .iter()
                .any(|r| r.key == "unknown" && r.subagents > 0)
        );

        // A contradictory child source must not be silently assigned to either vendor.
        let mut conflicting = sources.clone();
        let mut child_source = sources[0].clone();
        child_source.session_id = child.into();
        child_source.vendor = UsageVendor::Codex;
        conflicting.push(child_source);
        let conflict_report = fl_watch::usage::aggregate_documents(
            &conflicting,
            &identities,
            None,
            &vendor_query,
            1794315600000,
        )
        .unwrap();
        assert_eq!(
            conflict_report
                .rows
                .iter()
                .find(|r| r.key == "unknown")
                .unwrap()
                .subagents,
            1
        );
        assert!(conflict_report.coverage.incomplete);
    }
    #[test]
    fn scan_commit_and_deleted_source_are_idempotent() {
        let (_dir, store) = store();
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":42}}").unwrap();
        let source = UsageSource {
            path: f.path().into(),
            source_id: "src".into(),
            session_id: "s".into(),
            vendor: UsageVendor::Antigravity,
        };
        collect_once(
            &store,
            std::slice::from_ref(&source),
            &[],
            ScanLimits::default(),
        )
        .unwrap();
        let before = read_sources(&store).unwrap();
        collect_once(
            &store,
            std::slice::from_ref(&source),
            &[],
            ScanLimits::default(),
        )
        .unwrap();
        assert_eq!(before, read_sources(&store).unwrap());
        drop(f);
        collect_once(&store, &[source], &[], ScanLimits::default()).unwrap();
        assert_eq!(before, read_sources(&store).unwrap());
    }
    #[test]
    fn enumerates_canonical_transcript_and_retained_vendor_files() {
        let root = tempfile::tempdir().unwrap();
        let logs = root
            .path()
            .join(".gemini/antigravity-cli/brain/child/.system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("transcript.jsonl"), "").unwrap();
        std::fs::write(logs.join("transcript_full.jsonl"), "").unwrap();
        let claude = root.path().join(".claude/projects/p");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("session.jsonl"), "").unwrap();
        let (sources, incomplete) = discover_usage_sources(root.path());
        assert!(!incomplete);
        assert_eq!(sources.len(), 2);
        assert!(
            sources
                .iter()
                .any(|s| s.session_id == "child" && s.path.ends_with("transcript.jsonl"))
        );
        assert!(sources.iter().any(|s| s.session_id == "session"));
    }
    #[test]
    fn parent_and_persona_join_without_storing_message_content() {
        let (_dir, store) = store();
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let parent = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        writeln!(
            file,
            "{{\"type\":\"GENERIC\",\"content\":\"Some leading text. Message sent to \\\"{}\\\".\"}}",
            parent
        )
        .unwrap();
        writeln!(file, "{{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":42,\"content\":\"private fixture text\"}}").unwrap();
        let source = UsageSource {
            path: file.path().into(),
            source_id: "src".into(),
            session_id: "child".into(),
            vendor: UsageVendor::Antigravity,
        };
        collect_once(
            &store,
            std::slice::from_ref(&source),
            &[],
            ScanLimits::default(),
        )
        .unwrap();
        let docs = read_sources(&store).unwrap();
        assert!(
            !serde_json::to_string(&docs)
                .unwrap()
                .contains("private fixture text")
        );
        let identities = read_identities(&store).unwrap();
        assert_eq!(
            fl_watch::usage::resolve_persona("child", &identities),
            fl_watch::usage::Attribution::Unattributed
        );
        let identity = IdentityMappingDocument {
            session_id: parent.into(),
            persona: Some("Felix".into()),
            parent_id: None,
            launch_time_ms: None,
            provenance: "route".into(),
        };
        collect_once(&store, &[source], &[identity], ScanLimits::default()).unwrap();
        assert_eq!(
            fl_watch::usage::resolve_persona("child", &read_identities(&store).unwrap()),
            fl_watch::usage::Attribution::Persona("Felix".into())
        );
        assert_eq!(read_sources(&store).unwrap(), docs);
    }
    #[test]
    fn direct_grok_session_is_discovered_and_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join(".grok/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("session-id.jsonl"), "").unwrap();
        let (sources, incomplete) = discover_usage_sources(root.path());
        assert!(!incomplete);
        let source = sources
            .iter()
            .find(|s| s.session_id == "session-id")
            .unwrap();
        let doc = scan_source(source, None, ScanLimits::default()).unwrap();
        assert!(doc.coverage.unavailable);
    }

    #[test]
    fn conflicting_route_claims_remain_ambiguous() {
        let home = tempfile::tempdir().unwrap();
        let mut routes = fl_wire::identity::RouteTable::default();
        for persona in ["alice", "bob"] {
            let mut one = fl_wire::identity::RouteTable::default();
            one.upsert_explicit(
                persona,
                fl_wire::identity::VendorRoute::Codex {
                    thread_id: "shared-session".into(),
                    cwd: None,
                },
                fl_core::at::At::from_unix_millis(1),
            );
            routes.entries.extend(one.entries);
        }
        routes
            .save(Some(&home.path().join(".cache/agent-bus/routes.json")))
            .unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        collect_hub(&state, 0, false).unwrap();
        let store = state.open_store().unwrap();
        let mappings = read_identities(&store).unwrap();
        assert_eq!(
            fl_watch::usage::resolve_persona("shared-session", &mappings),
            fl_watch::usage::Attribution::Ambiguous
        );
    }

    #[test]
    fn collector_releases_store_guard_before_scanning() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".claude/projects/p");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(projects.join("s.jsonl"), "").unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        let mut scanned = false;
        collect_hub_with_hook(&state, 0, false, || {
            scanned = true;
            assert!(
                state.store_lock.try_lock().is_ok(),
                "store lock held while scanning"
            );
        })
        .unwrap();
        assert!(scanned);
    }
    #[test]
    fn legacy_backend_reports_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let opts = fl_store::StoreOptions {
            backend: Some(fl_store::BackendKind::Redb),
            ..Default::default()
        };
        let store = fl_store::open_store_with(&dir.path().join("legacy.redb"), &opts).unwrap();
        assert!(
            read_sources(&store)
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
        assert!(
            collect_once(&store, &[], &[], ScanLimits::default())
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
    }
    #[test]
    fn failed_source_does_not_starve_later_sources() {
        let (dir, store) = store();
        let bad = UsageSource {
            path: dir.path().into(),
            source_id: "bad".into(),
            session_id: "bad".into(),
            vendor: UsageVendor::Antigravity,
        };
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":42}}"
        )
        .unwrap();
        let good = UsageSource {
            path: file.path().into(),
            source_id: "good".into(),
            session_id: "good".into(),
            vendor: UsageVendor::Antigravity,
        };
        collect_once(&store, &[bad, good], &[], ScanLimits::default()).unwrap();
        assert_eq!(read_sources(&store).unwrap().len(), 1);
        assert_eq!(
            store
                .as_ferro()
                .unwrap()
                .query_by_field(COLLECTION, "kind", "source_error")
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn hub_collects_historical_sources_across_bounded_batches() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".claude/projects/p");
        std::fs::create_dir_all(&projects).unwrap();
        for i in 0..9 {
            std::fs::write(projects.join(format!("{i:03}.jsonl")), "").unwrap();
        }
        let config = crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        };
        let state = crate::HubState::new(config);
        let (next, pending) = collect_hub(&state, 0, false).unwrap();
        assert_eq!(next, 8);
        let (next, _) = collect_hub(&state, next, pending).unwrap();
        assert_eq!(next, 0);
        let store = state.open_store().unwrap();
        assert_eq!(read_sources(&store).unwrap().len(), 9);
        let status = store
            .as_ferro()
            .unwrap()
            .get_doc(COLLECTION, "collector-status")
            .unwrap()
            .unwrap();
        assert_eq!(status["pending"], false);
    }
    #[test]
    fn candidate_before_write_does_not_change_committed_cursor_or_counts() {
        let (_dir, store) = store();
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":42}}").unwrap();
        let source = UsageSource {
            path: f.path().into(),
            source_id: "src".into(),
            session_id: "s".into(),
            vendor: UsageVendor::Antigravity,
        };
        collect_once(
            &store,
            std::slice::from_ref(&source),
            &[],
            ScanLimits::default(),
        )
        .unwrap();
        let committed = read_sources(&store).unwrap();
        writeln!(f, "{{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":43}}").unwrap();
        let candidate = scan_source(&source, Some(&committed[0]), ScanLimits::default()).unwrap();
        assert_ne!(candidate, committed[0]);
        assert_eq!(read_sources(&store).unwrap(), committed);
        collect_once(&store, &[source], &[], ScanLimits::default()).unwrap();
        assert_eq!(read_sources(&store).unwrap()[0], candidate);
    }
}
