//! Git-backed CAS state synchronization over the `fl/state` orphan branch (spec §4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use fl_core::agent::RegistryEvent;
use fl_core::at::At;
use fl_core::ferrowire::{DeliveryReceipt, DeliveryStatus, WireMessage};
use fl_core::ids::{MessageId, StateId};
use fl_core::iri::Iri;
use fl_core::state::{StateRevision, StateScope, StateSyncCursor};
use fl_core::store::{AgentRegistry, MessageStore, StateStore, StoreError};
use sha2::{Digest, Sha256};

use crate::git::{Git, GitTreeEntry, PushCasOutcome};
use crate::population::ExecError;

/// Maximum size in bytes of a single `revisions/<entry_hash>/<n:04>.jsonl` segment file.
pub const MAX_SEGMENT_BYTES: usize = 262_144;

/// Maximum compare-and-swap push attempts before returning `StoreError::Contended`.
pub const MAX_CAS_TRIES: usize = 5;

const STATE_BRANCH_FORMAT: &str = "1\n";
const STATE_BRANCH_README: &str = "\
This orphan branch (`fl/state`) is managed automatically by FerroLoop (`fl state sync`).
Do not edit files on this branch by hand; use `fl state` and `fl agent` CLI commands.
";

#[derive(Debug, Clone)]
pub struct StateSyncTarget {
    pub repo_dir: PathBuf,
    pub remote: String,
    pub branch: String,
    pub private: bool,
    pub sync_registry: bool,
    pub sync_wire: bool,
    pub scope_filter: Option<StateScope>,
    pub by: String,
}

impl StateSyncTarget {
    /// Distinct per-checkout cursor key so multiple projects sharing a single `RedbStore`
    /// do not overwrite each other's `StateSyncCursor`.
    pub fn cursor_key(&self) -> String {
        format!("{}::{}", self.repo_dir.display(), self.remote)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSyncReport {
    pub remote: String,
    pub branch: String,
    pub previous_commit: Option<String>,
    pub committed_oid: Option<String>,
    pub imported_registry_events: usize,
    pub imported_revisions: usize,
    pub imported_wire_messages: usize,
    pub imported_wire_receipts: usize,
    pub published_registry_events: usize,
    pub published_revisions: usize,
    pub published_wire_messages: usize,
    pub published_wire_receipts: usize,
    pub active_conflicts: usize,
}

pub type StateSyncOutcome = StateSyncReport;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateVerifyReport {
    pub remote: String,
    pub branch: String,
    pub tip_commit: Option<String>,
    pub verified_commits: usize,
    pub verified_revisions: usize,
    pub verified_registry_events: usize,
    pub verified_wire_messages: usize,
    pub verified_wire_receipts: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum StateSyncError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Git(#[from] ExecError),
    #[error(
        "remote branch `{branch}` on `{remote}` was rewritten (expected ancestor `{expected_ancestor}`, found `{remote_tip}`)"
    )]
    NonFastForward {
        remote: String,
        branch: String,
        expected_ancestor: String,
        remote_tip: String,
    },
    #[error("unsupported fl/state branch format `{0}` (expected `1`)")]
    UnsupportedBranchFormat(String),
    #[error("tampered history on `fl/state` in commit `{commit}` at path `{path}`: {reason}")]
    HistoryTampered {
        commit: String,
        path: String,
        reason: String,
    },
    #[error("corrupt JSONL in `{path}` line {line}: {reason}")]
    CorruptJsonl {
        path: String,
        line: usize,
        reason: String,
    },
}

/// First 32 lowercase hex chars of SHA-256 of the canonical `StateId` IRI string (spec §4.1).
pub fn entry_id_hash(id: &StateId) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id.as_str().as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for b in &digest[..16] {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[derive(Default, Clone)]
struct BranchSnapshot {
    format: Option<String>,
    areas_events_raw: Vec<u8>,
    registry_events_raw: Vec<u8>,
    registry_events: Vec<RegistryEvent>,
    /// `revisions/<hash>/<seg>.jsonl` -> raw bytes
    segments: BTreeMap<(String, u32), Vec<u8>>,
    revisions: Vec<StateRevision>,
    /// `wire/messages/<prefix2>/<filename>.json` (`<prefix2>/<filename>.json`) -> raw bytes
    wire_messages_raw: BTreeMap<String, Vec<u8>>,
    wire_messages: Vec<WireMessage>,
    /// `wire/receipts/<seg>.jsonl` -> raw bytes
    wire_receipt_segments: BTreeMap<u32, Vec<u8>>,
    wire_receipts: Vec<DeliveryReceipt>,
}

fn wire_message_rel_path(id: &MessageId) -> String {
    let fname = id.as_str().replace(':', "_");
    let hex_tail: String = id
        .as_str()
        .chars()
        .rev()
        .filter(|c| c.is_ascii_hexdigit())
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let prefix = if hex_tail.len() == 2 {
        hex_tail.to_ascii_lowercase()
    } else {
        "00".to_string()
    };
    format!("{prefix}/{fname}.json")
}

fn parse_wire_receipt_segment_path(path: &str) -> Option<u32> {
    let rest = path.strip_prefix("wire/receipts/")?;
    let stem = rest.strip_suffix(".jsonl")?;
    if stem.len() != 4 || !stem.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

fn parse_segment_path(path: &str) -> Option<(String, u32)> {
    let rest = path.strip_prefix("revisions/")?;
    let (hash, file) = rest.split_once('/')?;
    if hash.len() != 32 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let stem = file.strip_suffix(".jsonl")?;
    if stem.len() != 4 || !stem.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: u32 = stem.parse().ok()?;
    Some((hash.to_owned(), n))
}

fn load_snapshot(
    repo_dir: &std::path::Path,
    commit: &str,
) -> Result<BranchSnapshot, StateSyncError> {
    let entries = Git::ls_tree(repo_dir, commit)?;
    let mut snap = BranchSnapshot::default();
    for entry in entries {
        if entry.kind != "blob" {
            continue;
        }
        let raw = Git::cat_blob(repo_dir, &entry.oid)?;
        match entry.path.as_str() {
            "format" => {
                let text = String::from_utf8_lossy(&raw).trim().to_owned();
                if text != "1" {
                    return Err(StateSyncError::UnsupportedBranchFormat(text));
                }
                snap.format = Some(text);
            }
            "README" => {}
            "areas.jsonl" => {
                let text =
                    String::from_utf8(raw.clone()).map_err(|e| StateSyncError::CorruptJsonl {
                        path: entry.path.clone(),
                        line: 1,
                        reason: e.to_string(),
                    })?;
                for (idx, line) in text.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let ev: RegistryEvent =
                        serde_json::from_str(line).map_err(|e| StateSyncError::CorruptJsonl {
                            path: entry.path.clone(),
                            line: idx + 1,
                            reason: e.to_string(),
                        })?;
                    snap.registry_events.push(ev);
                }
                snap.areas_events_raw = raw;
            }
            "agents.jsonl" => {
                let text =
                    String::from_utf8(raw.clone()).map_err(|e| StateSyncError::CorruptJsonl {
                        path: entry.path.clone(),
                        line: 1,
                        reason: e.to_string(),
                    })?;
                for (idx, line) in text.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let ev: RegistryEvent =
                        serde_json::from_str(line).map_err(|e| StateSyncError::CorruptJsonl {
                            path: entry.path.clone(),
                            line: idx + 1,
                            reason: e.to_string(),
                        })?;
                    snap.registry_events.push(ev);
                }
                snap.registry_events_raw = raw;
            }
            other => {
                if let Some((hash, seg)) = parse_segment_path(other) {
                    let text = String::from_utf8(raw.clone()).map_err(|e| {
                        StateSyncError::CorruptJsonl {
                            path: other.to_owned(),
                            line: 1,
                            reason: e.to_string(),
                        }
                    })?;
                    let mut line_count = 0usize;
                    for (idx, line) in text.lines().enumerate() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        line_count += 1;
                        let rev: StateRevision = serde_json::from_str(line).map_err(|e| {
                            StateSyncError::CorruptJsonl {
                                path: other.to_owned(),
                                line: idx + 1,
                                reason: e.to_string(),
                            }
                        })?;
                        let expected_hash = entry_id_hash(&rev.entry_id);
                        if expected_hash != hash {
                            return Err(StateSyncError::HistoryTampered {
                                commit: commit.to_owned(),
                                path: other.to_owned(),
                                reason: format!(
                                    "revision entry_id `{}` hashes to `{expected_hash}`, not `{hash}`",
                                    rev.entry_id
                                ),
                            });
                        }
                        snap.revisions.push(rev);
                    }
                    if line_count > 1 && raw.len() > MAX_SEGMENT_BYTES {
                        return Err(StateSyncError::HistoryTampered {
                            commit: commit.to_owned(),
                            path: other.to_owned(),
                            reason: format!(
                                "multi-line segment size {} exceeds MAX_SEGMENT_BYTES ({MAX_SEGMENT_BYTES})",
                                raw.len()
                            ),
                        });
                    }
                    snap.segments.insert((hash, seg), raw);
                } else if let Some(rel) = other.strip_prefix("wire/messages/") {
                    let text = String::from_utf8(raw.clone()).map_err(|e| {
                        StateSyncError::CorruptJsonl {
                            path: other.to_owned(),
                            line: 1,
                            reason: e.to_string(),
                        }
                    })?;
                    let msg: WireMessage = serde_json::from_str(&text).map_err(|e| {
                        StateSyncError::CorruptJsonl {
                            path: other.to_owned(),
                            line: 1,
                            reason: e.to_string(),
                        }
                    })?;
                    let expected_rel = wire_message_rel_path(&msg.id);
                    if rel != expected_rel {
                        return Err(StateSyncError::HistoryTampered {
                            commit: commit.to_owned(),
                            path: other.to_owned(),
                            reason: format!(
                                "wire message `{}` belongs at `wire/messages/{expected_rel}`, not `{other}`",
                                msg.id
                            ),
                        });
                    }
                    snap.wire_messages.push(msg);
                    snap.wire_messages_raw.insert(rel.to_owned(), raw);
                } else if let Some(seg) = parse_wire_receipt_segment_path(other) {
                    let text = String::from_utf8(raw.clone()).map_err(|e| {
                        StateSyncError::CorruptJsonl {
                            path: other.to_owned(),
                            line: 1,
                            reason: e.to_string(),
                        }
                    })?;
                    let mut line_count = 0usize;
                    for (idx, line) in text.lines().enumerate() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        line_count += 1;
                        let rcpt: DeliveryReceipt = serde_json::from_str(line).map_err(|e| {
                            StateSyncError::CorruptJsonl {
                                path: other.to_owned(),
                                line: idx + 1,
                                reason: e.to_string(),
                            }
                        })?;
                        snap.wire_receipts.push(rcpt);
                    }
                    if line_count > 1 && raw.len() > MAX_SEGMENT_BYTES {
                        return Err(StateSyncError::HistoryTampered {
                            commit: commit.to_owned(),
                            path: other.to_owned(),
                            reason: format!(
                                "multi-line receipt segment size {} exceeds MAX_SEGMENT_BYTES ({MAX_SEGMENT_BYTES})",
                                raw.len()
                            ),
                        });
                    }
                    snap.wire_receipt_segments.insert(seg, raw);
                }
            }
        }
    }
    Ok(snap)
}

/// Verify that every append-only file (`areas.jsonl`, `agents.jsonl`, `revisions/<hash>/<n>.jsonl`,
/// `wire/receipts/<n>.jsonl`) in `next` is a byte-for-byte prefix extension of `prev`, and every
/// `wire/messages/<prefix2>/<file>.json` in `prev` is present in `next` with identical bytes.
fn verify_append_only_step(
    commit: &str,
    prev: &BranchSnapshot,
    next: &BranchSnapshot,
) -> Result<(), StateSyncError> {
    if !prev.areas_events_raw.is_empty()
        && !next.areas_events_raw.starts_with(&prev.areas_events_raw)
    {
        return Err(StateSyncError::HistoryTampered {
            commit: commit.to_owned(),
            path: "areas.jsonl".into(),
            reason: "areas.jsonl is not a prefix extension of its parent commit".into(),
        });
    }
    if !prev.registry_events_raw.is_empty()
        && !next
            .registry_events_raw
            .starts_with(&prev.registry_events_raw)
    {
        return Err(StateSyncError::HistoryTampered {
            commit: commit.to_owned(),
            path: "agents.jsonl".into(),
            reason: "agents.jsonl is not a prefix extension of its parent commit".into(),
        });
    }
    for (key, prev_bytes) in &prev.segments {
        let path = format!("revisions/{}/{:04}.jsonl", key.0, key.1);
        let Some(next_bytes) = next.segments.get(key) else {
            return Err(StateSyncError::HistoryTampered {
                commit: commit.to_owned(),
                path,
                reason: "segment file was deleted".into(),
            });
        };
        if !next_bytes.starts_with(prev_bytes) {
            return Err(StateSyncError::HistoryTampered {
                commit: commit.to_owned(),
                path,
                reason: "segment file was modified or truncated".into(),
            });
        }
    }
    for (rel_path, prev_bytes) in &prev.wire_messages_raw {
        let path = format!("wire/messages/{rel_path}");
        let Some(next_bytes) = next.wire_messages_raw.get(rel_path) else {
            return Err(StateSyncError::HistoryTampered {
                commit: commit.to_owned(),
                path,
                reason: "wire message file was deleted".into(),
            });
        };
        if next_bytes != prev_bytes {
            return Err(StateSyncError::HistoryTampered {
                commit: commit.to_owned(),
                path,
                reason: "wire message file was modified".into(),
            });
        }
    }
    for (seg, prev_bytes) in &prev.wire_receipt_segments {
        let path = format!("wire/receipts/{seg:04}.jsonl");
        let Some(next_bytes) = next.wire_receipt_segments.get(seg) else {
            return Err(StateSyncError::HistoryTampered {
                commit: commit.to_owned(),
                path,
                reason: "wire receipt segment file was deleted".into(),
            });
        };
        if !next_bytes.starts_with(prev_bytes) {
            return Err(StateSyncError::HistoryTampered {
                commit: commit.to_owned(),
                path,
                reason: "wire receipt segment file was modified or truncated".into(),
            });
        }
    }
    Ok(())
}

/// Order revisions so parents always appear before children.
fn topo_sort_revisions(revs: &[StateRevision]) -> Vec<StateRevision> {
    let mut remaining: BTreeMap<String, StateRevision> = revs
        .iter()
        .map(|r| (r.revision_id.to_string(), r.clone()))
        .collect();
    let mut emitted = BTreeSet::new();
    let mut ordered = Vec::with_capacity(revs.len());
    while !remaining.is_empty() {
        let ready: Vec<String> = remaining
            .iter()
            .filter(|(_, r)| {
                r.parent_revisions.iter().all(|p| {
                    let k = p.to_string();
                    !remaining.contains_key(&k) || emitted.contains(&k)
                })
            })
            .map(|(k, _)| k.clone())
            .collect();
        if ready.is_empty() {
            for (_, r) in remaining {
                ordered.push(r);
            }
            break;
        }
        for k in ready {
            if let Some(r) = remaining.remove(&k) {
                emitted.insert(k);
                ordered.push(r);
            }
        }
    }
    ordered
}

/// Build the nested git tree for `BranchSnapshot` using `git hash-object` and `git mktree`.
fn write_snapshot_tree(
    repo_dir: &std::path::Path,
    snap: &BranchSnapshot,
) -> Result<String, StateSyncError> {
    let mut root_entries = Vec::new();

    let format_oid = Git::hash_object(repo_dir, STATE_BRANCH_FORMAT.as_bytes())?;
    root_entries.push(GitTreeEntry {
        mode: "100644".into(),
        kind: "blob".into(),
        oid: format_oid,
        path: "format".into(),
    });

    let readme_oid = Git::hash_object(repo_dir, STATE_BRANCH_README.as_bytes())?;
    root_entries.push(GitTreeEntry {
        mode: "100644".into(),
        kind: "blob".into(),
        oid: readme_oid,
        path: "README".into(),
    });

    if !snap.areas_events_raw.is_empty() {
        let oid = Git::hash_object(repo_dir, &snap.areas_events_raw)?;
        root_entries.push(GitTreeEntry {
            mode: "100644".into(),
            kind: "blob".into(),
            oid,
            path: "areas.jsonl".into(),
        });
    }

    if !snap.registry_events_raw.is_empty() {
        let oid = Git::hash_object(repo_dir, &snap.registry_events_raw)?;
        root_entries.push(GitTreeEntry {
            mode: "100644".into(),
            kind: "blob".into(),
            oid,
            path: "agents.jsonl".into(),
        });
    }

    if !snap.segments.is_empty() {
        let mut by_hash: BTreeMap<String, Vec<GitTreeEntry>> = BTreeMap::new();
        for ((hash, seg), bytes) in &snap.segments {
            let oid = Git::hash_object(repo_dir, bytes)?;
            by_hash.entry(hash.clone()).or_default().push(GitTreeEntry {
                mode: "100644".into(),
                kind: "blob".into(),
                oid,
                path: format!("{seg:04}.jsonl"),
            });
        }
        let mut hash_trees = Vec::new();
        for (hash, entries) in by_hash {
            let subtree_oid = Git::mktree(repo_dir, &entries)?;
            hash_trees.push(GitTreeEntry {
                mode: "040000".into(),
                kind: "tree".into(),
                oid: subtree_oid,
                path: hash,
            });
        }
        let revisions_tree_oid = Git::mktree(repo_dir, &hash_trees)?;
        root_entries.push(GitTreeEntry {
            mode: "040000".into(),
            kind: "tree".into(),
            oid: revisions_tree_oid,
            path: "revisions".into(),
        });
    }

    if !snap.wire_messages_raw.is_empty() || !snap.wire_receipt_segments.is_empty() {
        let mut wire_entries = Vec::new();
        if !snap.wire_messages_raw.is_empty() {
            let mut by_prefix: BTreeMap<String, Vec<GitTreeEntry>> = BTreeMap::new();
            for (rel_path, bytes) in &snap.wire_messages_raw {
                if let Some((prefix, file)) = rel_path.split_once('/') {
                    let oid = Git::hash_object(repo_dir, bytes)?;
                    by_prefix
                        .entry(prefix.to_owned())
                        .or_default()
                        .push(GitTreeEntry {
                            mode: "100644".into(),
                            kind: "blob".into(),
                            oid,
                            path: file.to_owned(),
                        });
                }
            }
            let mut prefix_trees = Vec::new();
            for (prefix, entries) in by_prefix {
                let subtree_oid = Git::mktree(repo_dir, &entries)?;
                prefix_trees.push(GitTreeEntry {
                    mode: "040000".into(),
                    kind: "tree".into(),
                    oid: subtree_oid,
                    path: prefix,
                });
            }
            let messages_tree_oid = Git::mktree(repo_dir, &prefix_trees)?;
            wire_entries.push(GitTreeEntry {
                mode: "040000".into(),
                kind: "tree".into(),
                oid: messages_tree_oid,
                path: "messages".into(),
            });
        }
        if !snap.wire_receipt_segments.is_empty() {
            let mut rcpt_entries = Vec::new();
            for (seg, bytes) in &snap.wire_receipt_segments {
                let oid = Git::hash_object(repo_dir, bytes)?;
                rcpt_entries.push(GitTreeEntry {
                    mode: "100644".into(),
                    kind: "blob".into(),
                    oid,
                    path: format!("{seg:04}.jsonl"),
                });
            }
            let receipts_tree_oid = Git::mktree(repo_dir, &rcpt_entries)?;
            wire_entries.push(GitTreeEntry {
                mode: "040000".into(),
                kind: "tree".into(),
                oid: receipts_tree_oid,
                path: "receipts".into(),
            });
        }
        let wire_tree_oid = Git::mktree(repo_dir, &wire_entries)?;
        root_entries.push(GitTreeEntry {
            mode: "040000".into(),
            kind: "tree".into(),
            oid: wire_tree_oid,
            path: "wire".into(),
        });
    }

    Ok(Git::mktree(repo_dir, &root_entries)?)
}

fn append_revision_to_segments(
    segments: &mut BTreeMap<(String, u32), Vec<u8>>,
    rev: &StateRevision,
) {
    let hash = entry_id_hash(&rev.entry_id);
    let mut line = serde_json::to_string(rev).expect("StateRevision serializes");
    line.push('\n');
    let line_bytes = line.as_bytes();

    let latest_seg = segments
        .keys()
        .filter(|(h, _)| h == &hash)
        .map(|(_, n)| *n)
        .max()
        .unwrap_or(0);

    let key = (hash.clone(), latest_seg);
    let current_len = segments.get(&key).map(|b| b.len()).unwrap_or(0);
    let target_seg = if current_len > 0 && current_len + line_bytes.len() > MAX_SEGMENT_BYTES {
        latest_seg + 1
    } else {
        latest_seg
    };
    segments
        .entry((hash, target_seg))
        .or_default()
        .extend_from_slice(line_bytes);
}

fn append_receipt_to_segments(segments: &mut BTreeMap<u32, Vec<u8>>, rcpt: &DeliveryReceipt) {
    let mut line = serde_json::to_string(rcpt).expect("DeliveryReceipt serializes");
    line.push('\n');
    let line_bytes = line.as_bytes();

    let latest_seg = segments.keys().next_back().copied().unwrap_or(0);
    let current_len = segments.get(&latest_seg).map_or(0, Vec::len);
    let target_seg =
        if current_len > 0 && current_len + line_bytes.len() > MAX_SEGMENT_BYTES {
            latest_seg + 1
        } else {
            latest_seg
        };
    segments
        .entry(target_seg)
        .or_default()
        .extend_from_slice(line_bytes);
}

/// Synchronize `store` with the `<remote>/<branch>` orphan branch in `target.repo_dir`
/// using the 5-step compare-and-swap protocol (spec §4.2).
pub fn sync_state<S>(
    store: &S,
    target: &StateSyncTarget,
    now: At,
) -> Result<StateSyncReport, StateSyncError>
where
    S: StateStore + AgentRegistry + MessageStore,
{
    let local_tracking_ref = format!("refs/fl-sync/{}/{}", target.remote, target.branch);
    let cursor_key = target.cursor_key();

    for _attempt in 0..MAX_CAS_TRIES {
        let cursor = store
            .sync_cursor(&cursor_key)?
            .unwrap_or_else(|| StateSyncCursor {
                remote: cursor_key.clone(),
                branch: target.branch.clone(),
                last_pushed_commit: None,
                last_revision_seq: 0,
                last_registry_seq: 0,
                last_wire_msg_seq: 0,
                last_wire_rcpt_seq: 0,
            });

        // Step 1: Fetch remote tip into isolated tracking ref.
        let remote_tip = Git::ls_remote_branch(&target.repo_dir, &target.remote, &target.branch)?;
        if remote_tip.is_some() {
            Git::fetch_branch(
                &target.repo_dir,
                &target.remote,
                &target.branch,
                &local_tracking_ref,
            )?;
        }

        // Step 2: Verify remote ancestry and append-only invariants.
        if let Some(ref last_pushed) = cursor.last_pushed_commit {
            match remote_tip {
                Some(ref tip) => {
                    if !Git::is_ancestor(&target.repo_dir, last_pushed, tip)? {
                        return Err(StateSyncError::NonFastForward {
                            remote: target.remote.clone(),
                            branch: target.branch.clone(),
                            expected_ancestor: last_pushed.clone(),
                            remote_tip: tip.clone(),
                        });
                    }
                }
                None => {
                    return Err(StateSyncError::NonFastForward {
                        remote: target.remote.clone(),
                        branch: target.branch.clone(),
                        expected_ancestor: last_pushed.clone(),
                        remote_tip: "<deleted>".into(),
                    });
                }
            }
        }

        let mut base_snapshot = BranchSnapshot::default();
        if let Some(ref tip) = remote_tip {
            let commits = Git::rev_list_first_parent(
                &target.repo_dir,
                cursor.last_pushed_commit.as_deref(),
                tip,
            )?;
            let mut prev_snap = match cursor.last_pushed_commit {
                Some(ref base_sha) => load_snapshot(&target.repo_dir, base_sha)?,
                None => BranchSnapshot::default(),
            };
            for sha in &commits {
                let next_snap = load_snapshot(&target.repo_dir, sha)?;
                verify_append_only_step(sha, &prev_snap, &next_snap)?;
                prev_snap = next_snap;
            }
            base_snapshot = prev_snap;
        }

        // Step 3: Import unseen remote registry events, state revisions, and wire records.
        let imported_registry_events = if target.sync_registry {
            store.import_registry_events(&base_snapshot.registry_events)?;
            base_snapshot.registry_events.len()
        } else {
            0
        };
        let ordered_remote_revs = topo_sort_revisions(&base_snapshot.revisions);
        if !target.private {
            for rev in &ordered_remote_revs {
                if rev.sensitive || rev.ever_sensitive {
                    return Err(StateSyncError::Store(StoreError::SensitiveDisclosure(
                        rev.entry_id.clone(),
                    )));
                }
            }
        }
        store.import_state_revisions(&ordered_remote_revs, now.clone())?;
        let imported_revisions = ordered_remote_revs.len();

        let (imported_wire_messages, imported_wire_receipts) = if target.sync_wire {
            store.import_wire_batch(&base_snapshot.wire_messages, &base_snapshot.wire_receipts)?;
            (
                base_snapshot.wire_messages.len(),
                base_snapshot.wire_receipts.len(),
            )
        } else {
            (0, 0)
        };

        // Step 4: Collect unpublished local events/revisions/wire records and enforce sensitivity guard.
        if !target.private {
            for entry in store.list_state(target.scope_filter.as_ref(), None)? {
                if entry.sensitive || entry.ever_sensitive {
                    return Err(StateSyncError::Store(StoreError::SensitiveDisclosure(
                        entry.id,
                    )));
                }
            }
        }

        let all_local_revs = store.state_revisions_after(0)?;
        let max_rev_seq = all_local_revs.last().map(|(seq, _)| *seq).unwrap_or(0);
        let remote_rev_ids: BTreeSet<_> = base_snapshot
            .revisions
            .iter()
            .map(|r| r.revision_id.clone())
            .collect();
        let unpublished_revs: Vec<StateRevision> = all_local_revs
            .into_iter()
            .map(|(_, r)| r)
            .filter(|r| target.scope_filter.as_ref().is_none_or(|sc| &r.scope == sc))
            .filter(|r| !remote_rev_ids.contains(&r.revision_id))
            .collect();

        if !target.private {
            for rev in &unpublished_revs {
                if rev.sensitive || rev.ever_sensitive {
                    return Err(StateSyncError::Store(StoreError::SensitiveDisclosure(
                        rev.entry_id.clone(),
                    )));
                }
            }
        }

        let (unpublished_reg_events, max_reg_seq) = if target.sync_registry {
            let all_reg = store.registry_events_after(0)?;
            let max_seq = all_reg.last().map(|(seq, _)| *seq).unwrap_or(0);
            let remote_ev_ids: BTreeSet<_> = base_snapshot
                .registry_events
                .iter()
                .map(|e| e.event_id.clone())
                .collect();
            let unpublished: Vec<RegistryEvent> = all_reg
                .into_iter()
                .map(|(_, e)| e)
                .filter(|e| !remote_ev_ids.contains(&e.event_id))
                .collect();
            (unpublished, max_seq)
        } else {
            (Vec::new(), cursor.last_registry_seq)
        };

        let (unpublished_wire_msgs, unpublished_wire_rcpts, max_wire_msg_seq, max_wire_rcpt_seq) =
            if target.sync_wire {
                let local_msgs = store.wire_messages_after(cursor.last_wire_msg_seq)?;
                let max_msg_seq = local_msgs
                    .last()
                    .map(|(seq, _)| *seq)
                    .unwrap_or(cursor.last_wire_msg_seq);
                let remote_msg_ids: BTreeSet<MessageId> = base_snapshot
                    .wire_messages
                    .iter()
                    .map(|m| m.id.clone())
                    .collect();
                let mut unpublished_msgs = Vec::new();
                for (_, msg) in local_msgs {
                    if remote_msg_ids.contains(&msg.id) {
                        continue;
                    }
                    if msg.durable {
                        unpublished_msgs.push(msg);
                    } else {
                        let not_expired = msg.deadline_at.as_ref().is_none_or(|exp| exp > &now);
                        if not_expired {
                            let rcpts = store.list_wire_receipts(&msg.id)?;
                            if rcpts.iter().any(|r| {
                                matches!(
                                    r.status,
                                    DeliveryStatus::Pending | DeliveryStatus::Delivered
                                )
                            }) {
                                unpublished_msgs.push(msg);
                            }
                        }
                    }
                }

                let mut exported_msg_ids = remote_msg_ids;
                for m in &unpublished_msgs {
                    exported_msg_ids.insert(m.id.clone());
                }

                let mut remote_rcpt_states: BTreeMap<(MessageId, Iri), DeliveryStatus> =
                    BTreeMap::new();
                for r in &base_snapshot.wire_receipts {
                    let key = (r.message_id.clone(), r.recipient_id.clone());
                    let entry = remote_rcpt_states.entry(key).or_insert(r.status);
                    if r.status.rank() > entry.rank() {
                        *entry = r.status;
                    }
                }

                let local_rcpts = store.wire_receipts_after(cursor.last_wire_rcpt_seq)?;
                let max_rcpt_seq = local_rcpts
                    .last()
                    .map(|(seq, _)| *seq)
                    .unwrap_or(cursor.last_wire_rcpt_seq);
                let mut unpublished_rcpts = Vec::new();
                for (_, rcpt) in local_rcpts {
                    if !exported_msg_ids.contains(&rcpt.message_id) {
                        continue;
                    }
                    let key = (rcpt.message_id.clone(), rcpt.recipient_id.clone());
                    if remote_rcpt_states
                        .get(&key)
                        .is_none_or(|rem_state| rcpt.status.rank() > rem_state.rank())
                    {
                        remote_rcpt_states.insert(key, rcpt.status);
                        unpublished_rcpts.push(rcpt);
                    }
                }

                (unpublished_msgs, unpublished_rcpts, max_msg_seq, max_rcpt_seq)
            } else {
                (
                    Vec::new(),
                    Vec::new(),
                    cursor.last_wire_msg_seq,
                    cursor.last_wire_rcpt_seq,
                )
            };

        if unpublished_revs.is_empty()
            && unpublished_reg_events.is_empty()
            && unpublished_wire_msgs.is_empty()
            && unpublished_wire_rcpts.is_empty()
        {
            let new_cursor = StateSyncCursor {
                remote: cursor_key,
                branch: target.branch.clone(),
                last_pushed_commit: remote_tip.clone(),
                last_revision_seq: max_rev_seq,
                last_registry_seq: max_reg_seq,
                last_wire_msg_seq: max_wire_msg_seq,
                last_wire_rcpt_seq: max_wire_rcpt_seq,
            };
            store.set_sync_cursor(new_cursor)?;
            let active_conflicts = store.list_conflicts()?.len();
            return Ok(StateSyncReport {
                remote: target.remote.clone(),
                branch: target.branch.clone(),
                previous_commit: remote_tip.clone(),
                committed_oid: remote_tip,
                imported_registry_events,
                imported_revisions,
                imported_wire_messages,
                imported_wire_receipts,
                published_registry_events: 0,
                published_revisions: 0,
                published_wire_messages: 0,
                published_wire_receipts: 0,
                active_conflicts,
            });
        }

        let mut next_snapshot = base_snapshot.clone();
        if target.sync_registry {
            for ev in &unpublished_reg_events {
                let mut line = serde_json::to_string(ev).unwrap();
                line.push('\n');
                match ev.kind {
                    fl_core::agent::RegistryEventKind::AreaUpsert(_) => {
                        next_snapshot
                            .areas_events_raw
                            .extend_from_slice(line.as_bytes());
                    }
                    fl_core::agent::RegistryEventKind::AgentUpsert(_)
                    | fl_core::agent::RegistryEventKind::AgentRetire { .. } => {
                        next_snapshot
                            .registry_events_raw
                            .extend_from_slice(line.as_bytes());
                    }
                }
            }
        }
        for rev in &unpublished_revs {
            append_revision_to_segments(&mut next_snapshot.segments, rev);
        }
        if target.sync_wire {
            for msg in &unpublished_wire_msgs {
                let rel_path = wire_message_rel_path(&msg.id);
                let mut raw = serde_json::to_vec(msg).unwrap();
                raw.push(b'\n');
                next_snapshot.wire_messages_raw.insert(rel_path, raw);
            }
            for rcpt in &unpublished_wire_rcpts {
                append_receipt_to_segments(&mut next_snapshot.wire_receipt_segments, rcpt);
            }
        }

        let tree_oid = write_snapshot_tree(&target.repo_dir, &next_snapshot)?;
        let author = if target.by.trim().is_empty() {
            "fl"
        } else {
            target.by.trim()
        };
        let commit_msg = format!("fl state sync ({author})");
        let commit_oid = Git::commit_tree(
            &target.repo_dir,
            &tree_oid,
            remote_tip.as_deref(),
            &commit_msg,
            author,
        )?;

        match Git::push_cas(
            &target.repo_dir,
            &target.remote,
            &target.branch,
            remote_tip.as_deref(),
            &commit_oid,
        )? {
            PushCasOutcome::Updated => {
                // Step 5: Persist updated sync cursor.
                let new_cursor = StateSyncCursor {
                    remote: cursor_key,
                    branch: target.branch.clone(),
                    last_pushed_commit: Some(commit_oid.clone()),
                    last_revision_seq: max_rev_seq,
                    last_registry_seq: max_reg_seq,
                    last_wire_msg_seq: max_wire_msg_seq,
                    last_wire_rcpt_seq: max_wire_rcpt_seq,
                };
                store.set_sync_cursor(new_cursor)?;
                let active_conflicts = store.list_conflicts()?.len();
                return Ok(StateSyncReport {
                    remote: target.remote.clone(),
                    branch: target.branch.clone(),
                    previous_commit: remote_tip,
                    committed_oid: Some(commit_oid),
                    imported_registry_events,
                    imported_revisions,
                    imported_wire_messages,
                    imported_wire_receipts,
                    published_registry_events: unpublished_reg_events.len(),
                    published_revisions: unpublished_revs.len(),
                    published_wire_messages: unpublished_wire_msgs.len(),
                    published_wire_receipts: unpublished_wire_rcpts.len(),
                    active_conflicts,
                });
            }
            PushCasOutcome::Rejected => continue,
        }
    }

    Err(StateSyncError::Store(StoreError::Contended {
        store: format!("{} ({})", target.repo_dir.display(), target.remote),
        tries: MAX_CAS_TRIES as u32,
    }))
}

/// Verify integrity of the `<remote>/<branch>` orphan branch and local `store` (spec §4.2, §5.2).
pub fn verify_state<S>(
    store: &S,
    target: &StateSyncTarget,
) -> Result<StateVerifyReport, StateSyncError>
where
    S: StateStore + AgentRegistry + MessageStore,
{
    let local_tracking_ref = format!("refs/fl-sync/{}/{}", target.remote, target.branch);
    let cursor_key = target.cursor_key();
    let cursor = store.sync_cursor(&cursor_key)?;
    let remote_tip = Git::ls_remote_branch(&target.repo_dir, &target.remote, &target.branch)?;
    let Some(tip) = remote_tip else {
        if let Some(ref c) = cursor
            && let Some(ref last_pushed) = c.last_pushed_commit
        {
            return Err(StateSyncError::NonFastForward {
                remote: target.remote.clone(),
                branch: target.branch.clone(),
                expected_ancestor: last_pushed.clone(),
                remote_tip: "<deleted>".into(),
            });
        }
        return Ok(StateVerifyReport {
            remote: target.remote.clone(),
            branch: target.branch.clone(),
            tip_commit: None,
            verified_commits: 0,
            verified_revisions: 0,
            verified_registry_events: 0,
            verified_wire_messages: 0,
            verified_wire_receipts: 0,
        });
    };

    Git::fetch_branch(
        &target.repo_dir,
        &target.remote,
        &target.branch,
        &local_tracking_ref,
    )?;

    if let Some(ref c) = cursor
        && let Some(ref last_pushed) = c.last_pushed_commit
        && !Git::is_ancestor(&target.repo_dir, last_pushed, &tip)?
    {
        return Err(StateSyncError::NonFastForward {
            remote: target.remote.clone(),
            branch: target.branch.clone(),
            expected_ancestor: last_pushed.clone(),
            remote_tip: tip,
        });
    }

    let commits = Git::rev_list_first_parent(&target.repo_dir, None, &tip)?;
    let mut prev = BranchSnapshot::default();
    for sha in &commits {
        let next = load_snapshot(&target.repo_dir, sha)?;
        verify_append_only_step(sha, &prev, &next)?;
        if !target.private {
            for rev in &next.revisions {
                if rev.sensitive || rev.ever_sensitive {
                    return Err(StateSyncError::Store(StoreError::SensitiveDisclosure(
                        rev.entry_id.clone(),
                    )));
                }
            }
        }
        prev = next;
    }

    // Cross-check remote revisions against local store revisions.
    let local_revs: BTreeMap<_, _> = store
        .state_revisions_after(0)?
        .into_iter()
        .map(|(_, r)| (r.revision_id.clone(), r))
        .collect();
    for rev in &prev.revisions {
        if let Some(local) = local_revs.get(&rev.revision_id) {
            let matches_without_entry_id = local.parent_revisions == rev.parent_revisions
                && local.scope == rev.scope
                && local.concern == rev.concern
                && local.slug == rev.slug
                && local.sensitive == rev.sensitive
                && local.ever_sensitive == rev.ever_sensitive
                && local.payload == rev.payload
                && local.at == rev.at
                && local.by == rev.by;
            if !matches_without_entry_id {
                return Err(StateSyncError::Store(StoreError::Tampered {
                    id: rev.revision_id.0.clone(),
                    detail: "remote state revision differs from local stored copy".into(),
                }));
            }
        }
    }

    // Cross-check remote registry events against local store registry events.
    if target.sync_registry {
        let local_events: BTreeMap<_, _> = store
            .registry_events_after(0)?
            .into_iter()
            .map(|(_, e)| (e.event_id.clone(), e))
            .collect();
        for ev in &prev.registry_events {
            if let Some(local) = local_events.get(&ev.event_id)
                && local != ev
            {
                return Err(StateSyncError::Store(StoreError::Tampered {
                    id: ev.event_id.clone(),
                    detail: "remote registry event differs from local stored copy".into(),
                }));
            }
        }
    }

    if target.sync_wire {
        for msg in &prev.wire_messages {
            if let Some(local) = store.get_wire_message(&msg.id)?
                && &local != msg
            {
                return Err(StateSyncError::Store(StoreError::WireMessageTampered {
                    id: msg.id.clone(),
                    detail: "remote wire message differs from local stored copy".into(),
                }));
            }
        }
    }

    // Verify every local state entry's head_revision exists in its revision history.
    for entry in store.list_state(target.scope_filter.as_ref(), None)? {
        let revs = store.state_revisions(&entry.id)?;
        if !revs.iter().any(|r| r.revision_id == entry.head_revision) {
            return Err(StateSyncError::Store(StoreError::Tampered {
                id: entry.id.0,
                detail: format!(
                    "state entry head_revision `{}` is missing from state_revisions",
                    entry.head_revision
                ),
            }));
        }
    }

    Ok(StateVerifyReport {
        remote: target.remote.clone(),
        branch: target.branch.clone(),
        tip_commit: Some(tip),
        verified_commits: commits.len(),
        verified_revisions: prev.revisions.len(),
        verified_registry_events: prev.registry_events.len(),
        verified_wire_messages: prev.wire_messages.len(),
        verified_wire_receipts: prev.wire_receipts.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::agent::{AgentRole, RegisterAgentArgs};
    use fl_core::mem::MemStore;
    use fl_core::state::{
        HandoffEntry, MergeMode, RuntimeEntry, SensitivityPatch, StatePayload, StateScope,
        UpsertStateArgs,
    };
    use std::process::Command;
    use tempfile::TempDir;

    fn setup_two_clones() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("remote.git");
        let clone_a = tmp.path().join("clone_a");
        let clone_b = tmp.path().join("clone_b");

        assert!(
            Command::new("git")
                .args(["init", "--bare", bare.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        for clone in [&clone_a, &clone_b] {
            assert!(
                Command::new("git")
                    .args(["init", clone.to_str().unwrap()])
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(clone)
                    .args(["remote", "add", "origin", bare.to_str().unwrap()])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        (tmp, clone_a, clone_b)
    }

    #[test]
    fn two_stores_sync_registry_and_state_without_touching_worktree() {
        let (_tmp, clone_a, clone_b) = setup_two_clones();
        let store_a = MemStore::default();
        let store_b = MemStore::default();

        store_a
            .register_agent(RegisterAgentArgs {
                display_name: "Ferris".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: At::from_unix_millis(10),
                by: "owner".into(),
            })
            .unwrap();
        let entry = store_a
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "current".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "Synced handoff".into(),
                    shipped: vec!["task-1".into()],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec!["task-2".into()],
                    notes: None,
                }),
                at: At::from_unix_millis(11),
                by: "ferris".into(),
            })
            .unwrap();

        let target_a = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: true,
            sync_wire: true,
            scope_filter: None,
            by: "ferris".into(),
        };
        let rep_a = sync_state(&store_a, &target_a, At::from_unix_millis(12)).unwrap();
        assert_eq!(rep_a.published_revisions, 1);
        assert_eq!(rep_a.published_registry_events, 2);

        let target_b = StateSyncTarget {
            repo_dir: clone_b,
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: true,
            sync_wire: true,
            scope_filter: None,
            by: "ferris".into(),
        };
        let rep_b = sync_state(&store_b, &target_b, At::from_unix_millis(13)).unwrap();
        assert_eq!(rep_b.imported_revisions, 1);
        assert_eq!(rep_b.imported_registry_events, 2);
        assert_eq!(store_b.get_state(&entry.id).unwrap(), Some(entry));
        assert!(store_b.agent_by_name("ferris").unwrap().is_some());

        let ver = verify_state(&store_b, &target_b).unwrap();
        assert_eq!(ver.verified_commits, 1);
        assert_eq!(ver.verified_revisions, 1);
    }

    #[test]
    fn segment_rolls_over_when_exceeding_max_segment_bytes() {
        let (_tmp, clone_a, _clone_b) = setup_two_clones();
        let store = MemStore::default();
        let big_notes = "x".repeat(60_000);
        for i in 0..6 {
            store
                .upsert_state(UpsertStateArgs {
                    scope: StateScope::Workspace,
                    slug: "big_handoff".into(),
                    sensitivity: SensitivityPatch::Inherit,
                    payload: StatePayload::Handoff(HandoffEntry {
                        summary: format!("rev {i}"),
                        shipped: vec![],
                        in_flight: vec![],
                        blockers: vec![],
                        next_steps: vec![],
                        notes: Some(big_notes.clone()),
                    }),
                    at: At::from_unix_millis(10 + i),
                    by: "ferris".into(),
                })
                .unwrap();
        }
        let target = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: false,
            sync_wire: false,
            scope_filter: None,
            by: "ferris".into(),
        };
        let rep = sync_state(&store, &target, At::from_unix_millis(100)).unwrap();
        let tree = Git::ls_tree(&clone_a, rep.committed_oid.as_ref().unwrap()).unwrap();
        let seg_paths: Vec<_> = tree
            .iter()
            .filter(|e| e.path.starts_with("revisions/"))
            .map(|e| e.path.as_str())
            .collect();
        assert!(seg_paths.iter().any(|p| p.ends_with("/0000.jsonl")));
        assert!(seg_paths.iter().any(|p| p.ends_with("/0001.jsonl")));
    }

    #[test]
    fn refuses_to_sync_sensitive_or_ever_sensitive_entries() {
        let (_tmp, clone_a, _clone_b) = setup_two_clones();
        let store = MemStore::default();
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "secret_hint".into(),
                sensitivity: SensitivityPatch::SetSensitive,
                payload: StatePayload::Runtime(RuntimeEntry {
                    value: serde_json::json!({"secret": true}),
                    merge_mode: MergeMode::Conflict,
                }),
                at: At::from_unix_millis(10),
                by: "jarvis".into(),
            })
            .unwrap();
        // Even after clearing sensitivity, ever_sensitive remains true and blocks git sync.
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "secret_hint".into(),
                sensitivity: SensitivityPatch::ClearSensitive,
                payload: StatePayload::Runtime(RuntimeEntry {
                    value: serde_json::json!({"secret": false}),
                    merge_mode: MergeMode::Conflict,
                }),
                at: At::from_unix_millis(11),
                by: "jarvis".into(),
            })
            .unwrap();

        let target = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: false,
            sync_wire: false,
            scope_filter: None,
            by: "jarvis".into(),
        };
        let err = sync_state(&store, &target, At::from_unix_millis(12)).unwrap_err();
        assert!(matches!(
            err,
            StateSyncError::Store(StoreError::SensitiveDisclosure(_))
        ));

        // When `private: true` is configured on the target, sync succeeds.
        let private_target = StateSyncTarget {
            repo_dir: clone_a,
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: true,
            sync_registry: false,
            sync_wire: false,
            scope_filter: None,
            by: "jarvis".into(),
        };
        let rep = sync_state(&store, &private_target, At::from_unix_millis(13)).unwrap();
        assert_eq!(rep.published_revisions, 2);
    }

    #[test]
    fn offline_same_key_state_id_collision_resolves_and_syncs() {
        let (_tmp, clone_a, clone_b) = setup_two_clones();
        let store_a = MemStore::default();
        let store_b = MemStore::starting_at(100);

        let entry_a = store_a
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "shared_task".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "A initial".into(),
                    shipped: vec![],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec![],
                    notes: None,
                }),
                at: At::from_unix_millis(10),
                by: "agent_a".into(),
            })
            .unwrap();

        let target_a = StateSyncTarget {
            repo_dir: clone_a,
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: false,
            sync_wire: false,
            scope_filter: None,
            by: "agent_a".into(),
        };
        let rep_a = sync_state(&store_a, &target_a, At::from_unix_millis(12)).unwrap();
        assert_eq!(rep_a.published_revisions, 1);

        // Store B offline creates the same (scope, concern, slug) with a different StateId.
        let entry_b = store_b
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "shared_task".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "B offline".into(),
                    shipped: vec![],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec![],
                    notes: None,
                }),
                at: At::from_unix_millis(11),
                by: "agent_b".into(),
            })
            .unwrap();
        assert_ne!(entry_a.id, entry_b.id);

        let target_b = StateSyncTarget {
            repo_dir: clone_b.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: false,
            sync_wire: false,
            scope_filter: None,
            by: "agent_b".into(),
        };
        // Store B syncs with remote: entry_a is first-published, so entry_b is aliased to entry_a.
        let rep_b = sync_state(&store_b, &target_b, At::from_unix_millis(20)).unwrap();
        assert_eq!(rep_b.imported_revisions, 1);
        assert_eq!(rep_b.published_revisions, 1);

        // Resolving entry_b.id now resolves to entry_a.id.
        let resolved = store_b.get_state(&entry_b.id).unwrap().unwrap();
        assert_eq!(resolved.id, entry_a.id);

        let ver_b = verify_state(&store_b, &target_b).unwrap();
        assert_eq!(ver_b.verified_commits, 2);
    }

    #[test]
    fn non_fast_forward_remote_history_is_detected() {
        let (_tmp, clone_a, _clone_b) = setup_two_clones();
        let store = MemStore::default();
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "task".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "initial".into(),
                    shipped: vec![],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec![],
                    notes: None,
                }),
                at: At::from_unix_millis(10),
                by: "agent_a".into(),
            })
            .unwrap();

        let target = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: false,
            sync_wire: false,
            scope_filter: None,
            by: "agent_a".into(),
        };
        sync_state(&store, &target, At::from_unix_millis(11)).unwrap();

        // Rewrite history on origin
        let bare = clone_a.parent().unwrap().join("remote.git");
        let empty_tree = Git::mktree(&bare, &[]).unwrap();
        let rewritten_commit =
            Git::commit_tree(&bare, &empty_tree, None, "rewritten", "bad").unwrap();
        Command::new("git")
            .current_dir(&bare)
            .args(["update-ref", "refs/heads/fl/state", &rewritten_commit])
            .status()
            .unwrap();

        // Now sync_state should fail with NonFastForward error
        let err = sync_state(&store, &target, At::from_unix_millis(12)).unwrap_err();
        assert!(matches!(err, StateSyncError::NonFastForward { .. }));
    }

    #[test]
    fn history_tampered_is_detected_when_segment_modified() {
        let (_tmp, clone_a, _clone_b) = setup_two_clones();
        let store = MemStore::default();
        let entry = store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "task".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "initial".into(),
                    shipped: vec![],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec![],
                    notes: None,
                }),
                at: At::from_unix_millis(10),
                by: "agent_a".into(),
            })
            .unwrap();

        let target = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: false,
            sync_wire: false,
            scope_filter: None,
            by: "agent_a".into(),
        };
        let rep = sync_state(&store, &target, At::from_unix_millis(11)).unwrap();
        let parent_commit = rep.committed_oid.unwrap();

        // Craft a new tampered commit on origin with modified segment bytes
        let bare = clone_a.parent().unwrap().join("remote.git");
        let hash = entry_id_hash(&entry.id);
        let tampered_blob = Git::hash_object(&bare, b"corrupted content\n").unwrap();
        let subtree = Git::mktree(
            &bare,
            &[GitTreeEntry {
                mode: "100644".into(),
                kind: "blob".into(),
                oid: tampered_blob,
                path: "0000.jsonl".into(),
            }],
        )
        .unwrap();
        let revisions_tree = Git::mktree(
            &bare,
            &[GitTreeEntry {
                mode: "040000".into(),
                kind: "tree".into(),
                oid: subtree,
                path: hash,
            }],
        )
        .unwrap();
        let format_oid = Git::hash_object(&bare, b"1\n").unwrap();
        let root_tree = Git::mktree(
            &bare,
            &[
                GitTreeEntry {
                    mode: "100644".into(),
                    kind: "blob".into(),
                    oid: format_oid,
                    path: "format".into(),
                },
                GitTreeEntry {
                    mode: "040000".into(),
                    kind: "tree".into(),
                    oid: revisions_tree,
                    path: "revisions".into(),
                },
            ],
        )
        .unwrap();
        let tampered_commit = Git::commit_tree(
            &bare,
            &root_tree,
            Some(&parent_commit),
            "tampered commit",
            "attacker",
        )
        .unwrap();
        Command::new("git")
            .current_dir(&bare)
            .args(["update-ref", "refs/heads/fl/state", &tampered_commit])
            .status()
            .unwrap();

        let err = sync_state(&store, &target, At::from_unix_millis(15)).unwrap_err();
        assert!(matches!(
            err,
            StateSyncError::HistoryTampered { .. } | StateSyncError::CorruptJsonl { .. }
        ));
    }

    #[test]
    fn push_cas_detects_rejected_lease() {
        let (_tmp, clone_a, _clone_b) = setup_two_clones();
        let store = MemStore::default();
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "task".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "initial".into(),
                    shipped: vec![],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec![],
                    notes: None,
                }),
                at: At::from_unix_millis(10),
                by: "agent_a".into(),
            })
            .unwrap();

        let target = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: false,
            sync_wire: false,
            scope_filter: None,
            by: "agent_a".into(),
        };
        let rep = sync_state(&store, &target, At::from_unix_millis(11)).unwrap();
        let first_oid = rep.committed_oid.unwrap();

        // Create a new distinct commit that differs from remote tip
        let format_oid = Git::hash_object(&clone_a, b"1\n").unwrap();
        let new_tree = Git::mktree(
            &clone_a,
            &[GitTreeEntry {
                mode: "100644".into(),
                kind: "blob".into(),
                oid: format_oid,
                path: "format".into(),
            }],
        )
        .unwrap();
        let second_commit = Git::commit_tree(
            &clone_a,
            &new_tree,
            Some(&first_oid),
            "second commit",
            "agent_a",
        )
        .unwrap();

        // Stale expected_old_oid causes push_cas to return Rejected
        let stale_oid = "0000000000000000000000000000000000000000";
        let outcome = Git::push_cas(
            &clone_a,
            "origin",
            "fl/state",
            Some(stale_oid),
            &second_commit,
        )
        .unwrap();
        assert_eq!(outcome, PushCasOutcome::Rejected);
    }

    #[test]
    fn selective_wire_export_and_offline_store_and_forward_round_trip() {
        use fl_core::ferrowire::{
            MessageKind, MessageUrgency, SendWireArgs, WireAddress, WireFilter,
        };

        let (_tmp, clone_a, clone_b) = setup_two_clones();
        let store_a = MemStore::default();
        let store_b = MemStore::default();

        // 1. Locally acked transient message on Store A -> skipped by selective export policy.
        let (acked_transient, _) = store_a
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("kayla".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: false,
                subject: "local ping".into(),
                body: "already acked locally".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(10),
            })
            .unwrap();
        store_a
            .update_wire_receipt(
                &acked_transient.id,
                "kayla",
                DeliveryStatus::Acked,
                false,
                At::from_unix_millis(11),
            )
            .unwrap();

        // 2. Expired transient message on Store A -> skipped by selective export policy.
        let (_expired_transient, _) = store_a
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("kayla".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: false,
                subject: "expired ping".into(),
                body: "expired before sync".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: Some(At::from_unix_millis(15)),
                at: At::from_unix_millis(12),
            })
            .unwrap();

        // 3. Unacknowledged non-expired transient message -> exported.
        let (pending_transient, _) = store_a
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("kayla".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: false,
                subject: "offline ping".into(),
                body: "deliver via git".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: Some(At::from_unix_millis(500)),
                at: At::from_unix_millis(13),
            })
            .unwrap();

        // 4. Durable message (even if already acked) -> exported.
        let (durable_msg, _) = store_a
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("kayla".into()),
                kind: MessageKind::Handoff,
                urgency: MessageUrgency::Urgent,
                durable: true,
                subject: "durable handoff".into(),
                body: "permanent record".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(14),
            })
            .unwrap();

        let target_a = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: true,
            sync_wire: true,
            scope_filter: None,
            by: "jarvis".into(),
        };
        let rep_a = sync_state(&store_a, &target_a, At::from_unix_millis(20)).unwrap();
        assert_eq!(rep_a.published_wire_messages, 2);
        assert_eq!(rep_a.published_wire_receipts, 2);

        // Store B syncs from origin and receives only the 2 exported messages.
        let target_b = StateSyncTarget {
            repo_dir: clone_b,
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: true,
            sync_wire: true,
            scope_filter: None,
            by: "kayla".into(),
        };
        let rep_b = sync_state(&store_b, &target_b, At::from_unix_millis(25)).unwrap();
        assert_eq!(rep_b.imported_wire_messages, 2);
        assert_eq!(rep_b.imported_wire_receipts, 2);
        assert_eq!(rep_b.published_wire_messages, 0);
        assert_eq!(rep_b.published_wire_receipts, 0);
        assert!(store_b.get_wire_message(&acked_transient.id).unwrap().is_none());
        assert!(store_b.get_wire_message(&pending_transient.id).unwrap().is_some());
        assert!(store_b.get_wire_message(&durable_msg.id).unwrap().is_some());

        let inbox = store_b
            .list_wire_inbox(&WireFilter {
                recipient: Some("kayla".into()),
                unread_only: true,
                thread_id: None,
                correlation_id: None,
                limit: None,
            })
            .unwrap();
        assert_eq!(inbox.len(), 2);

        // Kayla acknowledges `pending_transient` on Store B and syncs back to origin.
        store_b
            .update_wire_receipt(
                &pending_transient.id,
                "kayla",
                DeliveryStatus::Acked,
                false,
                At::from_unix_millis(35),
            )
            .unwrap();
        let rep_b2 = sync_state(&store_b, &target_b, At::from_unix_millis(40)).unwrap();
        assert_eq!(rep_b2.published_wire_messages, 0);
        assert_eq!(rep_b2.published_wire_receipts, 1);

        // Store A syncs and observes Kayla's Acked receipt.
        let rep_a2 = sync_state(&store_a, &target_a, At::from_unix_millis(45)).unwrap();
        assert_eq!(rep_a2.imported_wire_receipts, 3);
        let rcpts_a = store_a.list_wire_receipts(&pending_transient.id).unwrap();
        assert_eq!(rcpts_a[0].status, DeliveryStatus::Acked);

        let ver = verify_state(&store_a, &target_a).unwrap();
        assert_eq!(ver.verified_commits, 2);
        assert_eq!(ver.verified_wire_messages, 2);
        assert_eq!(ver.verified_wire_receipts, 3);
    }

    #[test]
    fn wire_message_tamper_on_branch_is_detected() {
        use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};

        let (_tmp, clone_a, _clone_b) = setup_two_clones();
        let store = MemStore::default();
        let (msg, _) = store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "jarvis".into(),
                sender_host: None,
                to: WireAddress::AgentName("kayla".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "immutable".into(),
                body: "original body".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(10),
            })
            .unwrap();

        let target = StateSyncTarget {
            repo_dir: clone_a.clone(),
            remote: "origin".into(),
            branch: "fl/state".into(),
            private: false,
            sync_registry: false,
            sync_wire: true,
            scope_filter: None,
            by: "jarvis".into(),
        };
        let rep = sync_state(&store, &target, At::from_unix_millis(15)).unwrap();
        let parent_commit = rep.committed_oid.unwrap();

        // Craft a commit on origin that deletes `wire/messages/...`
        let bare = clone_a.parent().unwrap().join("remote.git");
        let format_oid = Git::hash_object(&bare, b"1\n").unwrap();
        let root_tree = Git::mktree(
            &bare,
            &[GitTreeEntry {
                mode: "100644".into(),
                kind: "blob".into(),
                oid: format_oid,
                path: "format".into(),
            }],
        )
        .unwrap();
        let tampered_commit = Git::commit_tree(
            &bare,
            &root_tree,
            Some(&parent_commit),
            "deleted wire message",
            "attacker",
        )
        .unwrap();
        Command::new("git")
            .current_dir(&bare)
            .args(["update-ref", "refs/heads/fl/state", &tampered_commit])
            .status()
            .unwrap();

        let err = verify_state(&store, &target).unwrap_err();
        assert!(matches!(err, StateSyncError::HistoryTampered { .. }));
        let _ = msg;
    }
}
