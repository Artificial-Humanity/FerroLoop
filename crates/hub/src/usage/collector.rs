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

/// Only the hub calls this writer; each source advances by one atomic document put.
pub fn collect_once(
    store: &AnyStore,
    sources: &[UsageSource],
    identities: &[IdentityMappingDocument],
    limits: ScanLimits,
) -> anyhow::Result<()> {
    let ferro = store
        .as_ferro()
        .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
    let _flight = COLLECTING.lock().unwrap_or_else(|e| e.into_inner());
    for identity in identities {
        let id = key(
            "identity",
            &format!("{}\0{}", identity.session_id, identity.provenance),
        );
        ferro.put_doc(
            COLLECTION,
            &id,
            &serde_json::json!({"kind":"identity", "document":identity}),
        )?;
    }
    for source in sources {
        let id = key("source", &source.source_id);
        let old: Option<SourceUsageDocument> = ferro
            .get_doc(COLLECTION, &id)?
            .map(|v| serde_json::from_value(v["document"].clone()))
            .transpose()?;
        let candidate = match scan_source(source, old.as_ref(), limits) {
            Ok(d) => d,
            Err(super::scan::UsageScanError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(_) => {
                ferro.put_doc(
                    COLLECTION,
                    &key("source-error", &source.source_id),
                    &serde_json::json!({"kind":"source_error"}),
                )?;
                continue;
            }
        };
        let error_id = key("source-error", &source.source_id);
        if ferro.get_doc(COLLECTION, &error_id)?.is_some() {
            ferro.delete_doc(COLLECTION, &error_id)?;
        }
        if old.as_ref() != Some(&candidate) {
            ferro.put_doc(
                COLLECTION,
                &id,
                &serde_json::json!({"kind":"source", "document":candidate}),
            )?;
        }
        for parent in &candidate.parser.antigravity_parents {
            let mapping = IdentityMappingDocument {
                session_id: candidate.session_id.clone(),
                persona: None,
                parent_id: Some(parent.clone()),
                launch_time_ms: None,
                provenance: format!("antigravity-parent:{parent}"),
            };
            let id = key(
                "identity",
                &format!("{}\0{}", mapping.session_id, mapping.provenance),
            );
            ferro.put_doc(
                COLLECTION,
                &id,
                &serde_json::json!({"kind":"identity", "document":mapping}),
            )?;
        }
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
    let home = state
        .config
        .home_override
        .clone()
        .or_else(|| std::env::var_os("HOME").map(Into::into))
        .ok_or_else(|| anyhow::anyhow!("usage source home unavailable"))?;
    let (sources, enumeration_incomplete) = discover_usage_sources(&home);
    let store = state.open_store()?;
    let snap = crate::discovery::snapshot_discovery_store(&store);
    let routes = crate::discovery::discover_route_sessions_without_model_info(
        &snap,
        state.config.home_override.as_deref(),
    );
    let mut identities: Vec<_> = routes
        .into_iter()
        .filter_map(|r| {
            let persona = r.persona?;
            Some(IdentityMappingDocument {
                session_id: r.id,
                persona: Some(persona.clone()),
                parent_id: None,
                launch_time_ms: None,
                provenance: format!("route:{}:{persona}", r.source),
            })
        })
        .collect();
    if let Ok(records) = crate::lifecycle::list_hosted_sessions(&*store) {
        for record in records {
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
    }
    let start = if start >= sources.len() { 0 } else { start };
    let end = (start + 8).min(sources.len());
    let selected = &sources[start..end];
    collect_once(&store, selected, &identities, ScanLimits::default())?;
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
            "{{\"type\":\"GENERIC\",\"content\":\"Message sent to \\\"{}\\\".\"}}",
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
