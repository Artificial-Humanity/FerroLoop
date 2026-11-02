//! The workspace runtime entries FerroWatch reads, decoded in one `list_state` pass.
//!
//! - **Attempt heartbeats** (`liveness-*`): [`AttemptHeartbeat`] values that `fl attempt` writes
//!   under a record slug and, when the attempt names an agent, an agent slug as well.
//! - **Hosted-session records** (`hosted-session-*`): written by the hub. Their type lives in
//!   `fl-hub`, which this crate must not depend on, so the JSON is read field by field
//!   ([`WatchHostedSession::from_value`]): a missing or mistyped field takes its default, and an
//!   entry without a `session_id` string is counted as undecodable, not guessed at.
//! - **Stored quota windows** (`watch-quota-*`): [`WatchQuotaWindow`] values (one, or an array)
//!   that any tool may record. No fl component writes them today; they are the only quota source
//!   a hub-less caller (the CLI) has.

use fl_core::state::{StateConcern, StatePayload, StateScope};
use fl_core::store::{StateStore, StoreError};
use fl_core::telemetry::AttemptHeartbeat;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::alerts::WatchQuotaWindow;

/// Slug prefix of every attempt-heartbeat runtime entry (`liveness-record-*`, `liveness-agent-*`).
pub const HEARTBEAT_SLUG_PREFIX: &str = "liveness-";
/// Slug prefix of every hub hosted-session runtime entry.
pub const HOSTED_SESSION_SLUG_PREFIX: &str = "hosted-session-";
/// Slug prefix of stored quota windows.
pub const QUOTA_SLUG_PREFIX: &str = "watch-quota-";

/// The hosted-session status the hub writes for a session that is up.
pub const HOSTED_STATUS_RUNNING: &str = "running";
/// The hosted-session status the hub writes once a session is torn down.
pub const HOSTED_STATUS_DECOMMISSIONED: &str = "decommissioned";

/// The fields FerroWatch reads from a hub hosted-session record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchHostedSession {
    pub session_id: String,
    pub persona: String,
    pub project: String,
    pub role: String,
    pub vendor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub node_id: String,
    /// `running`, `decommissioning` or `decommissioned` (the hub's words).
    pub status: String,
    pub restart_count: u64,
    pub updated_at_ms: u64,
}

impl WatchHostedSession {
    /// Read a hosted-session record from its JSON. `None` when it has no non-empty
    /// `session_id` string; every other field falls back to its default.
    pub fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let text = |key: &str| {
            obj.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default()
        };
        let number = |key: &str| obj.get(key).and_then(Value::as_u64).unwrap_or(0);
        let session_id = text("session_id");
        if session_id.trim().is_empty() {
            return None;
        }
        Some(Self {
            session_id,
            persona: text("persona"),
            project: text("project"),
            role: text("role"),
            vendor: text("vendor"),
            model: obj.get("model").and_then(Value::as_str).map(str::to_string),
            node_id: text("node_id"),
            status: text("status"),
            restart_count: number("restart_count"),
            updated_at_ms: number("updated_at_ms"),
        })
    }

    /// Whether the hub has torn this session down.
    pub fn is_decommissioned(&self) -> bool {
        self.status
            .eq_ignore_ascii_case(HOSTED_STATUS_DECOMMISSIONED)
    }

    /// Whether the hub reports this session up.
    pub fn is_running(&self) -> bool {
        self.status.eq_ignore_ascii_case(HOSTED_STATUS_RUNNING)
    }
}

/// One heartbeat entry: the slug it was stored under and the heartbeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatEntry {
    pub slug: String,
    pub heartbeat: AttemptHeartbeat,
}

/// Every runtime entry FerroWatch reads, from one `list_state` call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeSnapshot {
    /// Heartbeat entries in slug order. The same attempt usually appears twice (record and
    /// agent slug); [`RuntimeSnapshot::latest_per_attempt`] folds them.
    pub heartbeats: Vec<HeartbeatEntry>,
    /// `liveness-*` entries whose value is not an [`AttemptHeartbeat`].
    pub heartbeats_undecodable: u64,
    /// Hosted-session records, sorted by `session_id`.
    pub hosted_sessions: Vec<WatchHostedSession>,
    /// `hosted-session-*` entries without a `session_id` string.
    pub hosted_sessions_undecodable: u64,
    /// Stored quota windows, in slug order.
    pub quotas: Vec<WatchQuotaWindow>,
    /// `watch-quota-*` entries that are neither a window nor an array of windows.
    pub quotas_undecodable: u64,
}

impl RuntimeSnapshot {
    /// The newest heartbeat of each attempt, keyed by attempt id. Ties keep the entry seen
    /// first (slug order), as `fl agent liveness` does.
    pub fn latest_per_attempt(&self) -> BTreeMap<String, &AttemptHeartbeat> {
        let mut out: BTreeMap<String, &AttemptHeartbeat> = BTreeMap::new();
        for entry in &self.heartbeats {
            let hb = &entry.heartbeat;
            let key = hb.attempt_id.as_str().to_string();
            match out.get(&key) {
                Some(prev) if prev.heartbeat_at.as_str() >= hb.heartbeat_at.as_str() => {}
                _ => {
                    out.insert(key, hb);
                }
            }
        }
        out
    }
}

/// Read every workspace runtime entry FerroWatch uses, in one call.
pub fn read_runtime<S: StateStore + ?Sized>(store: &S) -> Result<RuntimeSnapshot, StoreError> {
    let entries = store.list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))?;
    let mut snap = RuntimeSnapshot::default();
    for entry in entries {
        let StatePayload::Runtime(rt) = entry.payload else {
            continue;
        };
        if entry.slug.starts_with(HEARTBEAT_SLUG_PREFIX) {
            match serde_json::from_value::<AttemptHeartbeat>(rt.value) {
                Ok(heartbeat) => snap.heartbeats.push(HeartbeatEntry {
                    slug: entry.slug,
                    heartbeat,
                }),
                Err(_) => snap.heartbeats_undecodable += 1,
            }
        } else if entry.slug.starts_with(HOSTED_SESSION_SLUG_PREFIX) {
            match WatchHostedSession::from_value(&rt.value) {
                Some(s) => snap.hosted_sessions.push(s),
                None => snap.hosted_sessions_undecodable += 1,
            }
        } else if entry.slug.starts_with(QUOTA_SLUG_PREFIX) {
            match decode_quotas(rt.value) {
                Some(q) => snap.quotas.extend(q),
                None => snap.quotas_undecodable += 1,
            }
        }
    }
    snap.heartbeats.sort_by(|a, b| a.slug.cmp(&b.slug));
    snap.hosted_sessions
        .sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(snap)
}

fn decode_quotas(value: Value) -> Option<Vec<WatchQuotaWindow>> {
    if value.is_array() {
        serde_json::from_value::<Vec<WatchQuotaWindow>>(value).ok()
    } else {
        serde_json::from_value::<WatchQuotaWindow>(value)
            .ok()
            .map(|q| vec![q])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{heartbeat, put_runtime};
    use fl_core::MemStore;
    use fl_core::telemetry::LivenessStatus;
    use serde_json::json;

    #[test]
    fn hosted_session_json_is_read_field_by_field() {
        let full = json!({
            "session_id": "claude:ada", "persona": "ada", "project": "ferroloop",
            "role": "lead", "vendor": "claude", "model": "opus", "node_id": "ai-lab-0",
            "hosted": true, "status": "running", "restart_policy": "never",
            "restart_count": 2, "created_at_ms": 1, "updated_at_ms": 9,
            "some_future_field": {"x": 1}
        });
        let s = WatchHostedSession::from_value(&full).unwrap();
        assert_eq!(s.session_id, "claude:ada");
        assert_eq!(s.persona, "ada");
        assert_eq!(s.model.as_deref(), Some("opus"));
        assert_eq!(s.restart_count, 2);
        assert_eq!(s.updated_at_ms, 9);
        assert!(s.is_running());
        // A mistyped field takes its default instead of dropping the record.
        let odd = json!({"session_id": "codex:bo", "restart_count": "two", "persona": 5});
        let s = WatchHostedSession::from_value(&odd).unwrap();
        assert_eq!(s.restart_count, 0);
        assert_eq!(s.persona, "");
        // No session id: not a record.
        assert!(WatchHostedSession::from_value(&json!({"persona": "ada"})).is_none());
        assert!(WatchHostedSession::from_value(&json!({"session_id": " "})).is_none());
        assert!(WatchHostedSession::from_value(&json!([1, 2])).is_none());
    }

    #[test]
    fn read_runtime_sorts_entries_by_kind_and_counts_the_undecodable() {
        let store = MemStore::default();
        let hb = heartbeat("a1", "ada", "host-a", 1_000, LivenessStatus::Live);
        put_runtime(
            &store,
            "liveness-agent-ada",
            serde_json::to_value(&hb).unwrap(),
        );
        put_runtime(&store, "liveness-record-bad", json!({"not": "a heartbeat"}));
        put_runtime(
            &store,
            "hosted-session-claude-ada",
            json!({"session_id": "claude:ada", "persona": "ada", "status": "running"}),
        );
        put_runtime(&store, "hosted-session-broken", json!({"persona": "x"}));
        put_runtime(
            &store,
            "watch-quota-claude",
            json!([{"vendor": "claude", "window": "primary", "used_percent": 81.0}]),
        );
        put_runtime(
            &store,
            "watch-quota-codex",
            json!({"vendor": "codex", "window": "secondary", "used_percent": 10.0,
                   "resets_at_ms": 5}),
        );
        put_runtime(&store, "watch-quota-junk", json!("nope"));
        put_runtime(&store, "unrelated-entry", json!({"x": 1}));

        let snap = read_runtime(&store).unwrap();
        assert_eq!(snap.heartbeats.len(), 1);
        assert_eq!(snap.heartbeats_undecodable, 1);
        assert_eq!(snap.hosted_sessions.len(), 1);
        assert_eq!(snap.hosted_sessions_undecodable, 1);
        assert_eq!(snap.quotas.len(), 2);
        assert_eq!(snap.quotas[0].vendor, "claude");
        assert_eq!(snap.quotas[1].resets_at_ms, Some(5));
        assert_eq!(snap.quotas_undecodable, 1);
    }

    #[test]
    fn latest_per_attempt_folds_record_and_agent_copies() {
        let older = heartbeat("a1", "ada", "h", 1_000, LivenessStatus::Live);
        let newer = heartbeat("a1", "ada", "h", 2_000, LivenessStatus::Live);
        let snap = RuntimeSnapshot {
            heartbeats: vec![
                HeartbeatEntry {
                    slug: "liveness-agent-ada".into(),
                    heartbeat: older,
                },
                HeartbeatEntry {
                    slug: "liveness-record-x".into(),
                    heartbeat: newer.clone(),
                },
            ],
            ..RuntimeSnapshot::default()
        };
        let latest = snap.latest_per_attempt();
        assert_eq!(latest.len(), 1);
        assert_eq!(
            latest.values().next().unwrap().heartbeat_at,
            newer.heartbeat_at
        );
    }
}
