//! Vendor cloud sessions (Record #71).
//! Field names and defaults match `ui/src/components/SettingsModal.tsx`
//! (`VendorCloudSessionEntry` / `DEFAULT_VENDOR_CLOUD_SESSIONS`).
//! Persisted as a JSON array in a Workspace Runtime entry.

use crate::at::At;
use crate::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use crate::store::{StateStore, StoreError};
use serde::{Deserialize, Serialize};

pub const VENDOR_CLOUD_SESSIONS_SLUG: &str = "vendor-cloud-sessions";

#[derive(Debug, thiserror::Error)]
pub enum CloudSessionError {
    #[error("session_id must be non-empty and contain no control characters")]
    InvalidSessionId,
    #[error("{0} must be non-empty")]
    EmptyField(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VendorCloudSessionVendor {
    ClaudeCloud,
    CodexCloud,
    GeminiJulesCloud,
    GrokCloud,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VendorCloudSessionStatus {
    Running,
    Paused,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorCloudSessionEntry {
    pub session_id: String,
    pub vendor: VendorCloudSessionVendor,
    pub vendor_label: String,
    pub repo_branch: String,
    pub status: VendorCloudSessionStatus,
    pub turns: u64,
    pub tokens_used: u64,
    pub mirrored: bool,
    pub last_active: String,
}

impl VendorCloudSessionEntry {
    pub fn validate(&self) -> Result<(), CloudSessionError> {
        if self.session_id.trim().is_empty()
            || self
                .session_id
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
            || self.session_id.contains('/')
            || self.session_id.contains('\\')
            || self.session_id.contains("..")
        {
            return Err(CloudSessionError::InvalidSessionId);
        }
        for (name, v) in [
            ("vendor_label", &self.vendor_label),
            ("repo_branch", &self.repo_branch),
            ("last_active", &self.last_active),
        ] {
            if v.trim().is_empty() {
                return Err(CloudSessionError::EmptyField(name));
            }
        }
        Ok(())
    }
}

/// Verbatim `DEFAULT_VENDOR_CLOUD_SESSIONS` from the UI.
pub fn default_vendor_cloud_sessions() -> Vec<VendorCloudSessionEntry> {
    vec![
        VendorCloudSessionEntry {
            session_id: "cs-claude-web-901".into(),
            vendor: VendorCloudSessionVendor::ClaudeCloud,
            vendor_label: "Claude Cloud / Web".into(),
            repo_branch: "Artificial-Humanity/FerroLoop@feat/cloud-handoff".into(),
            status: VendorCloudSessionStatus::Running,
            turns: 18,
            tokens_used: 64200,
            mirrored: true,
            last_active: "2m ago".into(),
        },
        VendorCloudSessionEntry {
            session_id: "cs-codex-cloud-412".into(),
            vendor: VendorCloudSessionVendor::CodexCloud,
            vendor_label: "OpenAI Codex Cloud".into(),
            repo_branch: "Artificial-Humanity/FerroDeck@fix/remote-sync".into(),
            status: VendorCloudSessionStatus::Paused,
            turns: 11,
            tokens_used: 38900,
            mirrored: false,
            last_active: "14m ago".into(),
        },
        VendorCloudSessionEntry {
            session_id: "cs-jules-cloud-207".into(),
            vendor: VendorCloudSessionVendor::GeminiJulesCloud,
            vendor_label: "Google Jules / Gemini Cloud".into(),
            repo_branch: "Artificial-Humanity/FerroLoop@audit/acp-matrix".into(),
            status: VendorCloudSessionStatus::Completed,
            turns: 24,
            tokens_used: 91500,
            mirrored: true,
            last_active: "1h ago".into(),
        },
        VendorCloudSessionEntry {
            session_id: "cs-grok-cloud-088".into(),
            vendor: VendorCloudSessionVendor::GrokCloud,
            vendor_label: "xAI Grok Cloud".into(),
            repo_branch: "Artificial-Humanity/FerroLoop@exp/fast-triage".into(),
            status: VendorCloudSessionStatus::Running,
            turns: 7,
            tokens_used: 21400,
            mirrored: false,
            last_active: "5m ago".into(),
        },
    ]
}

fn now() -> At {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    At::from_unix_millis(ms)
}

fn upsert_runtime(
    store: &impl StateStore,
    slug: &str,
    value: serde_json::Value,
) -> Result<(), StoreError> {
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: slug.to_string(),
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Runtime(RuntimeEntry {
            value,
            merge_mode: MergeMode::LastWriter,
        }),
        at: now(),
        by: "fl".to_string(),
    })?;
    Ok(())
}

fn stored_value(
    store: &impl StateStore,
    slug: &str,
) -> Result<Option<serde_json::Value>, StoreError> {
    let Some(entry) =
        store.get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, slug)?
    else {
        return Ok(None);
    };
    let StatePayload::Runtime(rt) = &entry.payload else {
        return Ok(None);
    };
    Ok(Some(rt.value.clone()))
}

fn persist(
    store: &impl StateStore,
    sessions: &[VendorCloudSessionEntry],
) -> Result<(), StoreError> {
    let value = serde_json::to_value(sessions).map_err(|e| StoreError::Backend(e.to_string()))?;
    upsert_runtime(store, VENDOR_CLOUD_SESSIONS_SLUG, value)
}

/// Strict read for write paths: missing yields defaults, corrupt errors.
fn list_cloud_sessions_strict(
    store: &impl StateStore,
) -> Result<Vec<VendorCloudSessionEntry>, StoreError> {
    let Some(entry) = store.get_state_by_key(
        &StateScope::Workspace,
        StateConcern::Runtime,
        VENDOR_CLOUD_SESSIONS_SLUG,
    )?
    else {
        return Ok(default_vendor_cloud_sessions());
    };
    let StatePayload::Runtime(rt) = &entry.payload else {
        return Err(StoreError::Backend(format!(
            "corrupt stored cloud sessions ({VENDOR_CLOUD_SESSIONS_SLUG}): not a runtime entry; refusing to overwrite"
        )));
    };
    let sessions: Vec<VendorCloudSessionEntry> =
        serde_json::from_value(rt.value.clone()).map_err(|e| {
            StoreError::Backend(format!(
                "corrupt stored cloud sessions ({VENDOR_CLOUD_SESSIONS_SLUG}): {e}; refusing to overwrite"
            ))
        })?;
    if sessions.iter().any(|s| s.validate().is_err()) {
        return Err(StoreError::Backend(format!(
            "corrupt stored cloud sessions ({VENDOR_CLOUD_SESSIONS_SLUG}): an entry fails validation; refusing to overwrite"
        )));
    }
    Ok(sessions)
}

/// Stored sessions, or defaults when missing or corrupt (including any
/// entry that fails validation).
pub fn list_cloud_sessions(
    store: &impl StateStore,
) -> Result<Vec<VendorCloudSessionEntry>, StoreError> {
    let Some(value) = stored_value(store, VENDOR_CLOUD_SESSIONS_SLUG)? else {
        return Ok(default_vendor_cloud_sessions());
    };
    let Ok(sessions) = serde_json::from_value::<Vec<VendorCloudSessionEntry>>(value) else {
        return Ok(default_vendor_cloud_sessions());
    };
    if sessions.iter().any(|s| s.validate().is_err()) {
        return Ok(default_vendor_cloud_sessions());
    }
    Ok(sessions)
}

/// Validate and persist `entry`, replacing by `session_id` or appending.
pub fn upsert_cloud_session(
    store: &impl StateStore,
    entry: VendorCloudSessionEntry,
) -> Result<VendorCloudSessionEntry, StoreError> {
    entry
        .validate()
        .map_err(|e| StoreError::ProviderValidation(e.to_string()))?;
    let mut sessions = list_cloud_sessions_strict(store)?;
    if let Some(existing) = sessions
        .iter_mut()
        .find(|s| s.session_id == entry.session_id)
    {
        *existing = entry.clone();
    } else {
        sessions.push(entry.clone());
    }
    persist(store, &sessions)?;
    Ok(entry)
}

fn mutate_cloud_session(
    store: &impl StateStore,
    session_id: &str,
    mutate: impl FnOnce(&mut VendorCloudSessionEntry),
) -> Result<Option<VendorCloudSessionEntry>, StoreError> {
    let mut sessions = list_cloud_sessions_strict(store)?;
    let Some(entry) = sessions.iter_mut().find(|s| s.session_id == session_id) else {
        return Ok(None);
    };
    mutate(entry);
    entry
        .validate()
        .map_err(|e| StoreError::ProviderValidation(e.to_string()))?;
    let out = entry.clone();
    persist(store, &sessions)?;
    Ok(Some(out))
}

/// Mark a session mirrored (attached locally). Unknown id yields `None`.
pub fn attach_cloud_session(
    store: &impl StateStore,
    session_id: &str,
) -> Result<Option<VendorCloudSessionEntry>, StoreError> {
    mutate_cloud_session(store, session_id, |e| e.mirrored = true)
}

/// Set a session status. Unknown id yields `None`.
pub fn set_cloud_session_status(
    store: &impl StateStore,
    session_id: &str,
    status: VendorCloudSessionStatus,
) -> Result<Option<VendorCloudSessionEntry>, StoreError> {
    mutate_cloud_session(store, session_id, |e| e.status = status)
}

/// Record a handoff receipt for a session. The handoff receipt is
/// represented as a `last_active` update (`synced_at`); no separate
/// receipt record is stored. Unknown id yields `None`.
pub fn record_cloud_session_handoff(
    store: &impl StateStore,
    session_id: &str,
    synced_at: String,
) -> Result<Option<VendorCloudSessionEntry>, StoreError> {
    mutate_cloud_session(store, session_id, |e| e.last_active = synced_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;

    #[test]
    fn cloud_defaults_verbatim() {
        let d = default_vendor_cloud_sessions();
        assert_eq!(d.len(), 4);
        assert_eq!(d[0].session_id, "cs-claude-web-901");
        assert_eq!(d[0].vendor, VendorCloudSessionVendor::ClaudeCloud);
        assert_eq!(d[0].vendor_label, "Claude Cloud / Web");
        assert_eq!(
            d[0].repo_branch,
            "Artificial-Humanity/FerroLoop@feat/cloud-handoff"
        );
        assert_eq!(d[0].status, VendorCloudSessionStatus::Running);
        assert_eq!(
            (d[0].turns, d[0].tokens_used, d[0].mirrored),
            (18, 64200, true)
        );
        assert_eq!(d[0].last_active, "2m ago");
        assert_eq!(d[1].session_id, "cs-codex-cloud-412");
        assert_eq!(d[1].status, VendorCloudSessionStatus::Paused);
        assert_eq!(
            (d[1].turns, d[1].tokens_used, d[1].mirrored),
            (11, 38900, false)
        );
        assert_eq!(d[2].session_id, "cs-jules-cloud-207");
        assert_eq!(d[2].vendor, VendorCloudSessionVendor::GeminiJulesCloud);
        assert_eq!(d[2].status, VendorCloudSessionStatus::Completed);
        assert_eq!(d[3].session_id, "cs-grok-cloud-088");
        assert_eq!(d[3].vendor, VendorCloudSessionVendor::GrokCloud);
        assert_eq!(
            serde_json::to_string(&d[0].vendor).unwrap(),
            "\"claude-cloud\""
        );
        assert_eq!(
            serde_json::to_string(&d[2].vendor).unwrap(),
            "\"gemini-jules-cloud\""
        );
        assert_eq!(
            serde_json::to_string(&VendorCloudSessionStatus::Running).unwrap(),
            "\"running\""
        );
    }

    #[test]
    fn cloud_validate_rejects_bad() {
        let mut e = default_vendor_cloud_sessions()[0].clone();
        e.session_id = "  ".into();
        assert!(e.validate().is_err());
        e.session_id = "a\u{1}b".into();
        assert!(e.validate().is_err());
        e.session_id = "ok".into();
        e.vendor_label = " ".into();
        assert!(e.validate().is_err());
        e.vendor_label = "X".into();
        e.repo_branch = "".into();
        assert!(e.validate().is_err());
        e.repo_branch = "r".into();
        e.last_active = " ".into();
        assert!(e.validate().is_err());
    }

    #[test]
    fn cloud_unknown_attach_status_handoff_none() {
        let store = MemStore::default();
        assert!(attach_cloud_session(&store, "nope").unwrap().is_none());
        assert!(
            set_cloud_session_status(&store, "nope", VendorCloudSessionStatus::Failed)
                .unwrap()
                .is_none()
        );
        assert!(
            record_cloud_session_handoff(&store, "nope", "now".into())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cloud_upsert_append_replace_and_mutations_roundtrip() {
        let store = MemStore::default();
        assert_eq!(
            list_cloud_sessions(&store).unwrap(),
            default_vendor_cloud_sessions()
        );
        let mut e = default_vendor_cloud_sessions()[1].clone();
        assert!(!e.mirrored);
        let attached = attach_cloud_session(&store, &e.session_id)
            .unwrap()
            .unwrap();
        assert!(attached.mirrored);
        let s = set_cloud_session_status(&store, &e.session_id, VendorCloudSessionStatus::Running)
            .unwrap()
            .unwrap();
        assert_eq!(s.status, VendorCloudSessionStatus::Running);
        let h = record_cloud_session_handoff(&store, &e.session_id, "2026-01-01T00:00:00Z".into())
            .unwrap()
            .unwrap();
        assert_eq!(h.last_active, "2026-01-01T00:00:00Z");
        // Append new.
        e.session_id = "cs-new-1".into();
        e.turns = 1;
        upsert_cloud_session(&store, e.clone()).unwrap();
        assert_eq!(list_cloud_sessions(&store).unwrap().len(), 5);
        // Replace same id.
        e.turns = 99;
        upsert_cloud_session(&store, e).unwrap();
        let list = list_cloud_sessions(&store).unwrap();
        assert_eq!(list.len(), 5);
        assert_eq!(
            list.iter()
                .find(|x| x.session_id == "cs-new-1")
                .unwrap()
                .turns,
            99
        );
        // Mutations persisted.
        let codex = list
            .iter()
            .find(|x| x.session_id == "cs-codex-cloud-412")
            .unwrap();
        assert!(codex.mirrored);
        assert_eq!(codex.last_active, "2026-01-01T00:00:00Z");
        // Invalid upsert rejected.
        let mut bad = default_vendor_cloud_sessions()[0].clone();
        bad.session_id = "".into();
        assert!(upsert_cloud_session(&store, bad).is_err());
    }

    #[test]
    fn cloud_corrupt_falls_back_to_defaults() {
        let store = MemStore::default();
        upsert_runtime(
            &store,
            VENDOR_CLOUD_SESSIONS_SLUG,
            serde_json::json!({"bogus": 1}),
        )
        .unwrap();
        assert_eq!(
            list_cloud_sessions(&store).unwrap(),
            default_vendor_cloud_sessions()
        );
    }
    #[test]
    fn cloud_session_id_rejects_slashes_whitespace_dotdot() {
        let mut e = default_vendor_cloud_sessions()[0].clone();
        for bad in ["a/b", "a\\b", "a b", "a\tb", "..", "a/../b", "a..b"] {
            e.session_id = bad.into();
            assert!(e.validate().is_err(), "{bad:?}");
        }
        e.session_id = "cs-ok-1".into();
        assert!(e.validate().is_ok());
    }

    #[test]
    fn cloud_corrupt_preserved_after_failed_writes() {
        let store = MemStore::default();
        let corrupt = serde_json::json!({"bogus": 1});
        upsert_runtime(&store, VENDOR_CLOUD_SESSIONS_SLUG, corrupt.clone()).unwrap();
        assert_eq!(
            list_cloud_sessions(&store).unwrap(),
            default_vendor_cloud_sessions()
        );
        assert!(upsert_cloud_session(&store, default_vendor_cloud_sessions()[0].clone()).is_err());
        assert!(attach_cloud_session(&store, "cs-claude-web-901").is_err());
        assert!(
            set_cloud_session_status(
                &store,
                "cs-claude-web-901",
                VendorCloudSessionStatus::Paused
            )
            .is_err()
        );
        assert!(record_cloud_session_handoff(&store, "cs-claude-web-901", "now".into()).is_err());
        assert_eq!(
            stored_value(&store, VENDOR_CLOUD_SESSIONS_SLUG).unwrap(),
            Some(corrupt)
        );
    }
}
