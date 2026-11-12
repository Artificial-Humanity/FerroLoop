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

fn claude_subagent_parent(source: &UsageSource) -> Option<&str> {
    if source.vendor != UsageVendor::Claude || !source.session_id.starts_with("agent-") {
        return None;
    }
    let subagents = source.path.parent()?;
    if subagents.file_name()?.to_str()? != "subagents" {
        return None;
    }
    let parent = subagents.parent()?.file_name()?.to_str()?;
    uuid::Uuid::parse_str(parent).ok()?;
    Some(parent)
}
/// Structural Claude child identity; read only the first bounded record for launch time.
fn claude_subagent_identity(source: &UsageSource) -> Option<IdentityMappingDocument> {
    use std::io::{BufRead, BufReader, Read};
    let parent = claude_subagent_parent(source)?;
    let file = std::fs::File::open(&source.path).ok()?;
    let mut first = Vec::new();
    BufReader::new(file)
        .take(1024 * 1024 + 1)
        .read_until(b'\n', &mut first)
        .ok()?;
    let record: Option<serde_json::Value> = if first.len() <= 1024 * 1024 {
        serde_json::from_slice(&first).ok()
    } else {
        None
    };
    if let Some(session) = record
        .as_ref()
        .and_then(|v| v.get("sessionId"))
        .and_then(|v| v.as_str())
        && session != parent
    {
        return None;
    }
    let launch_time_ms = record
        .as_ref()
        .and_then(|v| v.get("timestamp"))
        .and_then(crate::discovery::parse_timestamp_value_ms);
    Some(IdentityMappingDocument {
        session_id: source.session_id.clone(),
        persona: None,
        parent_id: Some(parent.to_owned()),
        launch_time_ms,
        provenance: format!("claude-subagent-path:{parent}"),
    })
}

fn save_candidate(
    store: &AnyStore,
    source: &UsageSource,
    old: Option<&SourceUsageDocument>,
    candidate: Result<SourceUsageDocument, super::scan::UsageScanError>,
    claude_identity: Option<IdentityMappingDocument>,
) -> anyhow::Result<()> {
    let ferro = store
        .as_ferro()
        .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
    let candidate = match candidate {
        Ok(doc) => doc,
        Err(super::scan::UsageScanError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = key("source-error", &source.source_id);
            if ferro.get_doc(COLLECTION, &id)?.is_some() {
                ferro.delete_doc(COLLECTION, &id)?;
            }
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
    if let Some(identity) = claude_identity {
        put_identity(store, &identity)?;
    }
    if let Some(parent) = candidate.parser.codex_parent_id.as_ref() {
        put_identity(
            store,
            &IdentityMappingDocument {
                session_id: candidate.session_id.clone(),
                persona: None,
                parent_id: Some(parent.clone()),
                launch_time_ms: candidate.parser.codex_launch_time_ms,
                provenance: format!("codex-parent:{parent}"),
            },
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
        let identity = claude_subagent_identity(source);
        save_candidate(store, source, old.as_ref(), candidate, identity)?;
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

/// Cached scheduler state is hub-owned and never shared between collectors.
#[derive(Default)]
pub struct CollectionScheduler {
    initialized: bool,
    sources: std::collections::HashMap<String, SourceProgress>,
    claude_identified: std::collections::HashSet<(String, String)>,
    claude_checked: std::collections::HashSet<(String, String)>,
    last_identity_refresh: Option<std::time::Instant>,
    identity_refresh_failed: bool,
    invalid_source_documents: std::collections::HashSet<String>,
    invalid_identity_documents: std::collections::HashSet<String>,
    last_success_ms: Option<u64>,
    tick: u64,
}
#[derive(Default)]
struct SourceProgress {
    offset: u64,
    observed_len: Option<u64>,
    observed_mtime_ns: Option<u64>,
    file_identity: Option<String>,
    pending: bool,
    needs_migration: bool,
    last_serviced: u64,
}
fn file_identity(metadata: &std::fs::Metadata) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}
fn modified_ns(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|v| v.as_nanos().min(u128::from(u64::MAX)) as u64)
}
fn source_progress(doc: &SourceUsageDocument, tick: u64) -> SourceProgress {
    SourceProgress {
        offset: doc.offset,
        observed_len: doc.parser.observed_len,
        observed_mtime_ns: doc.parser.observed_mtime_ns,
        file_identity: doc.file_identity.clone(),
        pending: doc.coverage.pending,
        needs_migration: doc.parser.parser_revision != Some(fl_watch::usage::USAGE_PARSER_REVISION),
        last_serviced: tick,
    }
}
fn unchanged_source(progress: &SourceProgress, metadata: &std::fs::Metadata) -> bool {
    !progress.pending
        && !progress.needs_migration
        && progress.observed_len == Some(metadata.len())
        && progress.observed_mtime_ns.is_some()
        && progress.observed_mtime_ns == modified_ns(metadata)
        && progress.file_identity == file_identity(metadata)
}
fn refresh_identities(state: &crate::HubState) -> anyhow::Result<()> {
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
    let store = state.open_store()?;
    for identity in &identities {
        put_identity(&store, identity)?;
    }
    Ok(())
}
/// Scan at most this many bytes per tick, prioritizing recent changes while giving
/// every source a turn before revisiting one with a large backlog.
pub fn collect_hub(
    state: &crate::HubState,
    schedule: &mut CollectionScheduler,
) -> anyhow::Result<bool> {
    collect_hub_with_hook(state, schedule, 32 * 1024 * 1024, || {})
}
fn collect_hub_with_hook(
    state: &crate::HubState,
    schedule: &mut CollectionScheduler,
    byte_budget: usize,
    hook: impl FnMut(),
) -> anyhow::Result<bool> {
    collect_hub_with_hooks(state, schedule, byte_budget, hook, || {
        refresh_identities(state)
    })
}
fn collect_hub_with_hooks(
    state: &crate::HubState,
    schedule: &mut CollectionScheduler,
    byte_budget: usize,
    mut hook: impl FnMut(),
    mut refresh: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let home = state
        .config
        .home_override
        .clone()
        .or_else(|| std::env::var_os("HOME").map(Into::into))
        .ok_or_else(|| anyhow::anyhow!("usage source home unavailable"))?;
    let _flight = COLLECTING.lock().unwrap_or_else(|e| e.into_inner());
    let (sources, enumeration_incomplete) = discover_usage_sources(&home);
    if !schedule.initialized {
        let store = state.open_store()?;
        let ferro = store
            .as_ferro()
            .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
        for id in ferro.query_by_field(COLLECTION, "kind", "source")? {
            match ferro
                .get_doc(COLLECTION, &id)
                .map_err(anyhow::Error::from)
                .and_then(|value| {
                    value
                        .map(|v| {
                            serde_json::from_value::<SourceUsageDocument>(v["document"].clone())
                                .map_err(Into::into)
                        })
                        .transpose()
                }) {
                Ok(Some(doc)) => {
                    schedule
                        .sources
                        .insert(doc.source_id.clone(), source_progress(&doc, 0));
                }
                Ok(None) => {}
                Err(_) => {
                    schedule.invalid_source_documents.insert(id);
                }
            }
        }
        for id in ferro.query_by_field(COLLECTION, "kind", "identity")? {
            match ferro
                .get_doc(COLLECTION, &id)
                .map_err(anyhow::Error::from)
                .and_then(|value| {
                    value
                        .map(|v| {
                            serde_json::from_value::<IdentityMappingDocument>(v["document"].clone())
                                .map_err(Into::into)
                        })
                        .transpose()
                }) {
                Ok(Some(mapping)) => {
                    if let Some(parent) = mapping.parent_id.as_deref()
                        && mapping.provenance == format!("claude-subagent-path:{parent}")
                    {
                        schedule
                            .claude_identified
                            .insert((mapping.session_id, parent.to_owned()));
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    schedule.invalid_identity_documents.insert(id);
                }
            }
        }
        schedule.claude_checked = schedule.claude_identified.clone();
        let ferro = store
            .as_ferro()
            .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
        schedule.last_success_ms = ferro
            .get_doc(COLLECTION, "collector-status")?
            .and_then(|v| v["last_success_ms"].as_u64());
        schedule.initialized = true;
    }
    if schedule
        .last_identity_refresh
        .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(60))
    {
        schedule.identity_refresh_failed =
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut refresh)) {
                Ok(Ok(())) => false,
                Ok(Err(error)) => {
                    eprintln!("usage collector: identity refresh failed: {error}");
                    true
                }
                Err(_) => {
                    eprintln!("usage collector: identity refresh panicked");
                    true
                }
            };
        schedule.last_identity_refresh = Some(std::time::Instant::now());
    }
    schedule.tick = schedule.tick.saturating_add(1);
    let source_ids: std::collections::HashSet<_> = sources
        .iter()
        .map(|s| key("source-error", &s.source_id))
        .collect();
    {
        let store = state.open_store()?;
        let ferro = store
            .as_ferro()
            .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
        schedule
            .invalid_source_documents
            .retain(|id| match ferro.get_doc(COLLECTION, id) {
                Ok(None) => false,
                Ok(Some(value)) => {
                    serde_json::from_value::<SourceUsageDocument>(value["document"].clone())
                        .is_err()
                }
                Err(_) => true,
            });
        schedule
            .invalid_identity_documents
            .retain(|id| match ferro.get_doc(COLLECTION, id) {
                Ok(None) => false,
                Ok(Some(value)) => {
                    serde_json::from_value::<IdentityMappingDocument>(value["document"].clone())
                        .is_err()
                }
                Err(_) => true,
            });
        if !enumeration_incomplete {
            for id in ferro.query_by_field(COLLECTION, "kind", "source_error")? {
                if !source_ids.contains(&id) {
                    ferro.delete_doc(COLLECTION, &id)?;
                }
            }
        }
    }
    let mut candidates = Vec::new();
    let mut enumeration_errors = enumeration_incomplete
        || schedule.identity_refresh_failed
        || !schedule.invalid_source_documents.is_empty()
        || !schedule.invalid_identity_documents.is_empty();
    for source in &sources {
        let metadata = match std::fs::metadata(&source.path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                enumeration_errors = true;
                continue;
            }
        };
        let progress = schedule
            .sources
            .entry(source.source_id.clone())
            .or_default();
        let needs_identity = claude_subagent_parent(source).is_some_and(|parent| {
            !schedule
                .claude_checked
                .contains(&(source.session_id.clone(), parent.to_owned()))
        });
        if unchanged_source(progress, &metadata) && !needs_identity {
            continue;
        }
        candidates.push((source.clone(), metadata, progress.last_serviced));
    }
    // Alternate freshest-first with a fair pass: active agents stay current while
    // a set of very large recent sources cannot indefinitely starve old ones.
    candidates.sort_by(|a, b| {
        let freshness = || modified_ns(&b.1).cmp(&modified_ns(&a.1));
        let fairness = || a.2.cmp(&b.2);
        (if schedule.tick % 2 == 1 {
            freshness().then_with(fairness)
        } else {
            fairness().then_with(freshness)
        })
        .then_with(|| a.0.source_id.cmp(&b.0.source_id))
    });
    let mut spent = 0usize;
    for (source, meta, _) in candidates {
        if spent >= byte_budget.max(1)
            || (spent > 0 && byte_budget.saturating_sub(spent) <= 1024 * 1024)
        {
            break;
        }
        let progress = schedule
            .sources
            .get_mut(&source.source_id)
            .expect("source indexed");
        progress.last_serviced = schedule.tick;
        let migrating = progress.needs_migration;
        let old = {
            let store = state.open_store()?;
            match read_source(&store, &source) {
                Ok(doc) => doc,
                Err(error) => {
                    eprintln!(
                        "usage collector: source {} document unreadable: {error}",
                        key("source", &source.source_id)
                    );
                    schedule
                        .invalid_source_documents
                        .insert(key("source", &source.source_id));
                    enumeration_errors = true;
                    spent = spent.saturating_add(1);
                    continue;
                }
            }
        };
        // Existing installations may already have a complete source document but
        // lack its newly supported Claude parent mapping. Backfill only the first
        // record, rather than parsing the entire unchanged transcript again.
        if schedule
            .sources
            .get(&source.source_id)
            .is_some_and(|progress| unchanged_source(progress, &meta))
        {
            if let Some(parent) = claude_subagent_parent(&source) {
                let pair = (source.session_id.clone(), parent.to_owned());
                if let Some(identity) = claude_subagent_identity(&source) {
                    let store = state.open_store()?;
                    put_identity(&store, &identity)?;
                    schedule.claude_identified.insert(pair.clone());
                }
                schedule.claude_checked.insert(pair);
            }
            spent = spent.saturating_add((meta.len().min(1024 * 1024) as usize).max(1));
            continue;
        }
        let limits = ScanLimits {
            max_batch_bytes: (byte_budget - spent).clamp(1024 * 1024 + 1, 4 * 1024 * 1024),
            ..ScanLimits::default()
        };
        let candidate = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hook();
            scan_source(&source, old.as_ref(), limits)
        }));
        let candidate = match candidate {
            Ok(result) => result,
            Err(_) => {
                eprintln!(
                    "usage collector: source {} panicked",
                    key("source", &source.source_id)
                );
                Err(super::scan::UsageScanError::Invalid)
            }
        };
        if let Err(error) = &candidate {
            eprintln!(
                "usage collector: source {} scan failed: {error}",
                key("source", &source.source_id)
            );
        }
        let identity = claude_subagent_identity(&source);
        let identified = identity.as_ref().and_then(|i| {
            i.parent_id
                .as_ref()
                .map(|p| (i.session_id.clone(), p.clone()))
        });
        let new_progress = candidate
            .as_ref()
            .ok()
            .map(|d| source_progress(d, schedule.tick));
        let old_offset = if migrating {
            0
        } else {
            old.as_ref().map_or(0, |d| d.offset)
        };
        let bytes = meta
            .len()
            .saturating_sub(old_offset)
            .min(limits.max_batch_bytes as u64) as usize;
        // Scanning also hashes the old and new prefixes. Charge those reads so a
        // small append to many medium-sized files does not evade the byte budget.
        let anchor_bytes = |offset: u64| -> usize {
            if offset <= 4 * 1024 * 1024 {
                offset as usize
            } else {
                4096
            }
        };
        let new_offset = candidate.as_ref().ok().map_or(old_offset, |d| d.offset);
        let hash_reads = anchor_bytes(old_offset).saturating_add(anchor_bytes(new_offset));
        spent = spent
            .saturating_add(bytes.max(1))
            .saturating_add(hash_reads);
        let store = state.open_store()?;
        if let Err(error) = save_candidate(&store, &source, old.as_ref(), candidate, identity) {
            eprintln!(
                "usage collector: source {} could not be saved: {error}",
                key("source", &source.source_id)
            );
            enumeration_errors = true;
        } else if let Some(p) = new_progress {
            schedule.sources.insert(source.source_id.clone(), p);
            if let Some(pair) = identified {
                schedule.claude_checked.insert(pair.clone());
                schedule.claude_identified.insert(pair);
            }
        }
    }
    // Recompute from all discovered files and committed offsets. A sweep cursor alone
    // is not evidence of unread bytes.
    let pending = sources.iter().any(|source| {
        let Some(progress) = schedule.sources.get(&source.source_id) else {
            return std::fs::metadata(&source.path).is_ok_and(|m| m.len() > 0);
        };
        std::fs::metadata(&source.path).is_ok_and(|m| {
            progress.pending
                || (progress.needs_migration && m.len() > 0)
                || m.len() > progress.offset
                || (m.len() > 0
                    && !unchanged_source(progress, &m)
                    && (m.len() <= progress.offset || file_identity(&m) != progress.file_identity))
        })
    });
    let identity_work = sources.iter().any(|source| {
        claude_subagent_parent(source).is_some_and(|parent| {
            !schedule
                .claude_checked
                .contains(&(source.session_id.clone(), parent.to_owned()))
        })
    });
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let store = state.open_store()?;
    let ferro = store
        .as_ferro()
        .ok_or_else(|| anyhow::anyhow!("usage unsupported on redb backend"))?;
    let incomplete = enumeration_errors
        || !ferro
            .query_by_field(COLLECTION, "kind", "source_error")?
            .is_empty();
    if !pending && !identity_work && !incomplete {
        schedule.last_success_ms = Some(now);
    }
    ferro.put_doc(COLLECTION, "collector-status", &serde_json::json!({"kind":"status", "last_success_ms":schedule.last_success_ms, "pending":pending, "incomplete":incomplete}))?;
    Ok(pending || identity_work)
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
    fn corrupt_source_document_does_not_block_other_sources() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join(".claude/projects/p");
        std::fs::create_dir_all(&logs).unwrap();
        let line =
            "{\"type\":\"assistant\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":42}}}\n";
        std::fs::write(logs.join("a.jsonl"), line).unwrap();
        std::fs::write(logs.join("b.jsonl"), line).unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        let sources = discover_usage_sources(home.path()).0;
        {
            let store = state.open_store().unwrap();
            store
                .as_ferro()
                .unwrap()
                .put_doc(
                    COLLECTION,
                    &key("source", &sources[0].source_id),
                    &serde_json::json!({"kind":"source","document":{"broken":true}}),
                )
                .unwrap();
        }
        let mut schedule = CollectionScheduler::default();
        collect_hub_with_hooks(&state, &mut schedule, 32 * 1024 * 1024, || {}, || Ok(())).unwrap();
        let store = state.open_store().unwrap();
        let ferro = store.as_ferro().unwrap();
        let good = ferro
            .get_doc(COLLECTION, &key("source", &sources[1].source_id))
            .unwrap()
            .unwrap();
        assert_eq!(good["document"]["buckets"][0]["counts"]["input"], 42);
        let status = ferro
            .get_doc(COLLECTION, "collector-status")
            .unwrap()
            .unwrap();
        assert_eq!(status["incomplete"], true);
    }
    #[test]
    fn failed_identity_refresh_does_not_block_source_scans() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join(".claude/projects/p");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("abc.jsonl"),
            "{\"type\":\"assistant\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":42}}}\n",
        )
        .unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        let mut schedule = CollectionScheduler::default();
        collect_hub_with_hooks(
            &state,
            &mut schedule,
            32 * 1024 * 1024,
            || {},
            || Err(anyhow::anyhow!("identity error")),
        )
        .unwrap();
        let store = state.open_store().unwrap();
        assert_eq!(
            read_sources(&store).unwrap()[0].buckets[0].counts.input,
            Some(42)
        );
        let status = store
            .as_ferro()
            .unwrap()
            .get_doc(COLLECTION, "collector-status")
            .unwrap()
            .unwrap();
        assert_eq!(status["incomplete"], true);
        drop(store);
        schedule.last_identity_refresh = None;
        collect_hub_with_hooks(
            &state,
            &mut schedule,
            32 * 1024 * 1024,
            || {},
            || panic!("fixture panic"),
        )
        .unwrap();
        let store = state.open_store().unwrap();
        assert_eq!(
            read_sources(&store).unwrap()[0].buckets[0].counts.input,
            Some(42)
        );
    }
    #[test]
    fn codex_fork_parent_metadata_attributes_child_usage_and_launch() {
        let (_dir, store) = store();
        let home = tempfile::tempdir().unwrap();
        let parent = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let child = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let path = home.path().join("fork.jsonl");
        let records = [
            serde_json::json!({"type":"session_meta", "timestamp":"2026-11-10T12:30:00Z", "payload":{"parent_thread_id":parent,"timestamp":"2026-11-10T12:30:00Z", "subagent_history_start_ordinal":5}}),
            serde_json::json!({"type":"event_msg", "ordinal":4, "timestamp":"2026-11-10T12:30:00Z", "payload":{"type":"token_count", "info":{"total_token_usage":{"input_tokens":100}}}}),
            serde_json::json!({"type":"event_msg", "ordinal":6, "timestamp":"2026-11-10T12:31:00Z", "payload":{"type":"token_count", "info":{"total_token_usage":{"input_tokens":142}}}}),
        ];
        std::fs::write(
            &path,
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let source = UsageSource {
            path,
            source_id: "fork".into(),
            session_id: child.into(),
            vendor: UsageVendor::Codex,
        };
        let identity = IdentityMappingDocument {
            session_id: parent.into(),
            persona: Some("Felix".into()),
            parent_id: None,
            launch_time_ms: None,
            provenance: "route".into(),
        };
        collect_once(&store, &[source], &[identity], ScanLimits::default()).unwrap();
        let mappings = read_identities(&store).unwrap();
        let report = fl_watch::usage::aggregate_documents(
            &read_sources(&store).unwrap(),
            &mappings,
            None,
            &fl_watch::usage::UsageReportQuery {
                since_ms: 1794312000000,
                by: fl_watch::usage::UsageGroupBy::Persona,
            },
            1794315600000,
        )
        .unwrap();
        let row = report.rows.iter().find(|r| r.key == "Felix").unwrap();
        assert_eq!(row.input, Some(42));
        assert_eq!(row.subagents, 1);
    }
    #[test]
    fn claude_subagent_path_attributes_usage_and_counts_launch() {
        let (_dir, store) = store();
        let home = tempfile::tempdir().unwrap();
        let parent = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let logs = home
            .path()
            .join(".claude/projects/project")
            .join(parent)
            .join("subagents");
        std::fs::create_dir_all(&logs).unwrap();
        let file = logs.join("agent-child.jsonl");
        std::fs::write(&file, format!("{}\n", serde_json::json!({
            "type":"assistant", "timestamp":"2026-11-10T12:30:00Z", "sessionId":parent,
            "message":{"id":"msg1", "model":"claude", "usage":{"input_tokens":42,"output_tokens":3}}
        }))).unwrap();
        let identity = IdentityMappingDocument {
            session_id: parent.into(),
            persona: Some("Felix".into()),
            parent_id: None,
            launch_time_ms: None,
            provenance: "route".into(),
        };
        let (sources, _) = discover_usage_sources(home.path());
        collect_once(&store, &sources, &[identity], ScanLimits::default()).unwrap();
        let mappings = read_identities(&store).unwrap();
        assert!(mappings.iter().any(|m| m.session_id == "agent-child"
            && m.parent_id.as_deref() == Some(parent)
            && m.launch_time_ms == Some(1794313800000)));
        let report = fl_watch::usage::aggregate_documents(
            &read_sources(&store).unwrap(),
            &mappings,
            None,
            &fl_watch::usage::UsageReportQuery {
                since_ms: 1794312000000,
                by: fl_watch::usage::UsageGroupBy::Persona,
            },
            1794315600000,
        )
        .unwrap();
        let row = report.rows.iter().find(|r| r.key == "Felix").unwrap();
        assert_eq!(row.input, Some(42));
        assert_eq!(row.subagents, 1);
    }
    #[test]
    fn deleted_source_clears_prior_source_error() {
        let (dir, store) = store();
        let path = dir.path().join("missing");
        let source = UsageSource {
            source_id: "deleted".into(),
            session_id: "s".into(),
            vendor: UsageVendor::Claude,
            path,
        };
        let ferro = store.as_ferro().unwrap();
        ferro
            .put_doc(
                COLLECTION,
                &key("source-error", &source.source_id),
                &serde_json::json!({"kind":"source_error"}),
            )
            .unwrap();
        collect_once(&store, &[source], &[], ScanLimits::default()).unwrap();
        assert!(
            ferro
                .query_by_field(COLLECTION, "kind", "source_error")
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn scheduler_skips_unchanged_and_reports_only_unread_bytes() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".claude/projects/p");
        std::fs::create_dir_all(&projects).unwrap();
        for i in 0..3 {
            let line = format!(
                "{{\"type\":\"assistant\",\"padding\":\"{}\"}}\n",
                "x".repeat(200)
            );
            std::fs::write(projects.join(format!("{i}.jsonl")), line.repeat(8000)).unwrap();
        }
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        let mut schedule = CollectionScheduler::default();
        let mut scans = 0;
        assert!(
            collect_hub_with_hook(&state, &mut schedule, 1024 * 1024 + 1, || scans += 1).unwrap()
        );
        assert_eq!(scans, 1, "byte budget must bound each scan");
        for _ in 0..20 {
            if !collect_hub_with_hook(&state, &mut schedule, 1024 * 1024 + 1, || scans += 1)
                .unwrap()
            {
                break;
            }
        }
        let before = scans;
        assert!(
            !collect_hub_with_hook(&state, &mut schedule, 1024 * 1024 + 1, || scans += 1).unwrap()
        );
        assert_eq!(
            scans, before,
            "unchanged files must not be opened to scan again"
        );
        let mut restarted = CollectionScheduler::default();
        assert!(
            !collect_hub_with_hook(&state, &mut restarted, 1024 * 1024 + 1, || scans += 1).unwrap()
        );
        assert_eq!(
            scans, before,
            "persisted metadata must also skip unchanged files after restart"
        );
        let store = state.open_store().unwrap();
        let status = store
            .as_ferro()
            .unwrap()
            .get_doc(COLLECTION, "collector-status")
            .unwrap()
            .unwrap();
        assert_eq!(status["pending"], false);
    }
    #[test]
    fn previously_scanned_claude_child_gets_parent_without_usage_rescan() {
        let home = tempfile::tempdir().unwrap();
        let parent = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let logs = home
            .path()
            .join(".claude/projects/project")
            .join(parent)
            .join("subagents");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("agent-child.jsonl"), format!("{}\n", serde_json::json!({"type":"assistant", "timestamp":"2026-11-10T12:30:00Z", "sessionId":parent, "message":{"id":"msg1","usage":{"input_tokens":42}}}))).unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        let source = discover_usage_sources(home.path()).0.pop().unwrap();
        let doc = scan_source(&source, None, ScanLimits::default()).unwrap();
        {
            let store = state.open_store().unwrap();
            store
                .as_ferro()
                .unwrap()
                .put_doc(
                    COLLECTION,
                    &key("source", &source.source_id),
                    &serde_json::json!({"kind":"source", "document":doc}),
                )
                .unwrap();
        }
        let mut scans = 0;
        let mut scheduler = CollectionScheduler::default();
        assert!(
            !collect_hub_with_hook(&state, &mut scheduler, 32 * 1024 * 1024, || scans += 1)
                .unwrap()
        );
        assert_eq!(scans, 0);
        {
            let store = state.open_store().unwrap();
            assert!(
                read_identities(&store)
                    .unwrap()
                    .iter()
                    .any(|m| m.session_id == "agent-child"
                        && m.parent_id.as_deref() == Some(parent)
                        && m.launch_time_ms == Some(1794313800000))
            );
        }
        let mut restarted = CollectionScheduler::default();
        assert!(
            !collect_hub_with_hook(&state, &mut restarted, 32 * 1024 * 1024, || scans += 1)
                .unwrap()
        );
        assert_eq!(scans, 0);
    }
    #[test]
    fn legacy_codex_document_is_rebuilt_even_if_file_metadata_is_unchanged() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join(".codex/sessions/2026/11/10");
        std::fs::create_dir_all(&logs).unwrap();
        let session = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let path = logs.join(format!("rollout-2026-11-10T12-00-00-{session}.jsonl"));
        std::fs::write(&path, format!("{}\n", serde_json::json!({"type":"event_msg","timestamp":"2026-11-10T12:30:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":42,"output_tokens":3}}}}))).unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        let source = discover_usage_sources(home.path()).0.pop().unwrap();
        let mut legacy = scan_source(&source, None, ScanLimits::default()).unwrap();
        legacy.parser.parser_revision = None;
        legacy.buckets[0].counts.input = Some(10_000);
        {
            let store = state.open_store().unwrap();
            store
                .as_ferro()
                .unwrap()
                .put_doc(
                    COLLECTION,
                    &key("source", &source.source_id),
                    &serde_json::json!({"kind":"source", "document":legacy}),
                )
                .unwrap();
        }
        let mut scans = 0;
        let mut scheduler = CollectionScheduler::default();
        assert!(
            !collect_hub_with_hook(&state, &mut scheduler, 32 * 1024 * 1024, || scans += 1)
                .unwrap()
        );
        assert_eq!(scans, 1);
        let store = state.open_store().unwrap();
        let corrected = read_sources(&store).unwrap();
        assert_eq!(corrected[0].buckets[0].counts.input, Some(42));
        assert_eq!(
            corrected[0].parser.parser_revision,
            Some(fl_watch::usage::USAGE_PARSER_REVISION)
        );
    }
    #[test]
    fn identity_evidence_refreshes_no_more_than_once_per_minute() {
        let home = tempfile::tempdir().unwrap();
        let route_path = home.path().join(".cache/agent-bus/routes.json");
        let make_routes = |names: &[&str]| {
            let mut routes = fl_wire::identity::RouteTable::default();
            for persona in names {
                routes.upsert_explicit(
                    persona,
                    fl_wire::identity::VendorRoute::Codex {
                        thread_id: format!("thread-{persona}"),
                        cwd: None,
                    },
                    fl_core::at::At::from_unix_millis(1),
                );
            }
            routes.save(Some(&route_path)).unwrap();
        };
        make_routes(&["alice"]);
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        let mut schedule = CollectionScheduler::default();
        collect_hub(&state, &mut schedule).unwrap();
        make_routes(&["alice", "bob"]);
        collect_hub(&state, &mut schedule).unwrap();
        {
            let store = state.open_store().unwrap();
            assert!(
                !read_identities(&store)
                    .unwrap()
                    .iter()
                    .any(|m| m.persona.as_deref() == Some("bob"))
            );
        }
        schedule.last_identity_refresh =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(61));
        collect_hub(&state, &mut schedule).unwrap();
        let store = state.open_store().unwrap();
        assert!(
            read_identities(&store)
                .unwrap()
                .iter()
                .any(|m| m.persona.as_deref() == Some("bob"))
        );
    }
    #[test]
    fn scheduler_prioritizes_recent_source_without_starving_backlog() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".claude/projects/p");
        std::fs::create_dir_all(&projects).unwrap();
        let line = format!(
            "{{\"type\":\"assistant\",\"padding\":\"{}\"}}\n",
            "x".repeat(200)
        );
        std::fs::write(projects.join("old.jsonl"), line.repeat(8000)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        std::fs::write(projects.join("new.jsonl"), line.repeat(8000)).unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        let mut schedule = CollectionScheduler::default();
        assert!(collect_hub_with_hook(&state, &mut schedule, 1024 * 1024 + 1, || {}).unwrap());
        {
            let store = state.open_store().unwrap();
            let docs = read_sources(&store).unwrap();
            assert_eq!(docs.len(), 1);
            assert_eq!(docs[0].session_id, "new");
        }
        assert!(collect_hub_with_hook(&state, &mut schedule, 1024 * 1024 + 1, || {}).unwrap());
        let store = state.open_store().unwrap();
        assert!(
            read_sources(&store)
                .unwrap()
                .iter()
                .any(|d| d.session_id == "old")
        );
    }
    #[test]
    fn scheduler_recovers_after_source_panic_and_retries_later() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".claude/projects/p");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(projects.join("a.jsonl"), "{}\n").unwrap();
        std::fs::write(projects.join("b.jsonl"), "{}\n").unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        let mut schedule = CollectionScheduler::default();
        let mut calls = 0;
        assert!(
            collect_hub_with_hook(&state, &mut schedule, 32 * 1024 * 1024, || {
                calls += 1;
                assert_ne!(calls, 1, "simulated poison source");
            })
            .unwrap()
        );
        assert_eq!(calls, 2, "later source must still be scanned");
        assert!(!collect_hub(&state, &mut schedule).unwrap());
        let store = state.open_store().unwrap();
        assert_eq!(read_sources(&store).unwrap().len(), 2);
        assert!(
            store
                .as_ferro()
                .unwrap()
                .query_by_field(COLLECTION, "kind", "source_error")
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn scheduler_removes_error_documents_for_disappeared_sources() {
        let home = tempfile::tempdir().unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: home.path().join("store.fls"),
            home_override: Some(home.path().into()),
            ..Default::default()
        });
        {
            let store = state.open_store().unwrap();
            store
                .as_ferro()
                .unwrap()
                .put_doc(
                    COLLECTION,
                    &key("source-error", "gone"),
                    &serde_json::json!({"kind":"source_error"}),
                )
                .unwrap();
        }
        collect_hub(&state, &mut CollectionScheduler::default()).unwrap();
        let store = state.open_store().unwrap();
        assert!(
            store
                .as_ferro()
                .unwrap()
                .query_by_field(COLLECTION, "kind", "source_error")
                .unwrap()
                .is_empty()
        );
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
        collect_hub(&state, &mut CollectionScheduler::default()).unwrap();
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
        collect_hub_with_hook(
            &state,
            &mut CollectionScheduler::default(),
            32 * 1024 * 1024,
            || {
                scanned = true;
                assert!(
                    state.store_lock.try_lock().is_ok(),
                    "store lock held while scanning"
                );
            },
        )
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
        let mut scheduler = CollectionScheduler::default();
        assert!(!collect_hub(&state, &mut scheduler).unwrap());
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
