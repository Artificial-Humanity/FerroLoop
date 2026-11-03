//! Operator push notifications (Record #67): Web Push (VAPID) + APNs + FCM.
//! Field names and defaults match `ui/src/components/SettingsModal.tsx`
//! (`OperatorPushSubscriptionState` / `DEFAULT_OPERATOR_PUSH_STATE`).
//!
//! Quiet-hours timezone: only `UTC` (case-insensitive) is supported until a
//! timezone database lands; any other value fails validation.
//! Corrupt-state fallback: a missing or corrupt stored operator-push state
//! (including one that fails validation) yields `Default` — this is the
//! spec'd Ferris behaviour, applied deliberately in
//! [`get_operator_push_state`], unlike receipts (see [`append_push_receipt`]).

use crate::at::At;
use crate::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use crate::store::{StateStore, StoreError};
use serde::{Deserialize, Serialize};

pub const OPERATOR_PUSH_STATE_SLUG: &str = "operator-push-state";
pub const OPERATOR_PUSH_RECEIPTS_SLUG: &str = "operator-push-receipts";
pub const MAX_PUSH_RECEIPTS: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum NotificationError {
    #[error("invalid HH:MM time `{0}`: expected strict HH:MM (00-23:00-59)")]
    InvalidTime(String),
    #[error("quiet hours timezone must be non-empty")]
    EmptyTimezone,
    #[error(
        "quiet hours timezone `{0}` is not supported: only UTC is supported until a timezone database lands; set timezone to \"UTC\""
    )]
    UnsupportedTimezone(String),
    #[error("project_scope must be non-empty")]
    EmptyProjectScope,
    #[error("vapid_public_key_fingerprint must be non-empty")]
    EmptyFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebPushPermissionStatus {
    Granted,
    Default,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MobilePushChannelStatus {
    Active,
    Pending,
    Unregistered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorPushTriggers {
    pub question_badge: bool,
    pub blocker_error_badge: bool,
    pub urgent_wire_mail: bool,
    pub pr_review_requests: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorPushQuietHours {
    pub enabled: bool,
    pub start_time: String,
    pub end_time: String,
    pub timezone: String,
}

/// Parse strict `HH:MM` (exactly two digits, colon, two digits; 00-23:00-59)
/// into minutes since midnight.
fn parse_hh_mm(s: &str) -> Option<u16> {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return None;
    }
    if !b[0].is_ascii_digit()
        || !b[1].is_ascii_digit()
        || !b[3].is_ascii_digit()
        || !b[4].is_ascii_digit()
    {
        return None;
    }
    let hh: u16 = s[0..2].parse().ok()?;
    let mm: u16 = s[3..5].parse().ok()?;
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(hh * 60 + mm)
}

impl OperatorPushQuietHours {
    pub fn validate(&self) -> Result<(), NotificationError> {
        if self.timezone.trim().is_empty() {
            return Err(NotificationError::EmptyTimezone);
        }
        if !self.timezone.trim().eq_ignore_ascii_case("UTC") {
            return Err(NotificationError::UnsupportedTimezone(
                self.timezone.clone(),
            ));
        }
        // Times are validated even when disabled, so a stored disabled
        // window cannot carry a broken time that only fails on enabling.
        parse_hh_mm(&self.start_time)
            .ok_or_else(|| NotificationError::InvalidTime(self.start_time.clone()))?;
        parse_hh_mm(&self.end_time)
            .ok_or_else(|| NotificationError::InvalidTime(self.end_time.clone()))?;
        Ok(())
    }

    /// Whether `hh_mm` falls in the quiet window. Returns `false` when
    /// disabled or when any time is invalid. A window with
    /// `start_time == end_time` is empty (never quiet) — chosen so an
    /// equal start/end does not silently mute a full day.
    pub fn is_quiet_at(&self, hh_mm: &str) -> bool {
        if !self.enabled {
            return false;
        }
        let (Some(start), Some(end), Some(t)) = (
            parse_hh_mm(&self.start_time),
            parse_hh_mm(&self.end_time),
            parse_hh_mm(hh_mm),
        ) else {
            return false;
        };
        if start == end {
            false
        } else if start < end {
            start <= t && t < end
        } else {
            start <= t || t < end
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorPushSubscriptionState {
    pub web_push_permission: WebPushPermissionStatus,
    pub vapid_service_worker_registered: bool,
    pub vapid_public_key_fingerprint: String,
    pub apns_ios_status: MobilePushChannelStatus,
    pub fcm_android_status: MobilePushChannelStatus,
    pub triggers: OperatorPushTriggers,
    pub project_scope: String,
    pub quiet_hours: OperatorPushQuietHours,
}

impl Default for OperatorPushSubscriptionState {
    /// Verbatim `DEFAULT_OPERATOR_PUSH_STATE` from the UI.
    fn default() -> Self {
        Self {
            web_push_permission: WebPushPermissionStatus::Granted,
            vapid_service_worker_registered: true,
            vapid_public_key_fingerprint: "VAPID-SHA256:8f4e2b91c0d3".to_string(),
            apns_ios_status: MobilePushChannelStatus::Active,
            fcm_android_status: MobilePushChannelStatus::Active,
            triggers: OperatorPushTriggers {
                question_badge: true,
                blocker_error_badge: true,
                urgent_wire_mail: true,
                pr_review_requests: true,
            },
            project_scope: "all".to_string(),
            quiet_hours: OperatorPushQuietHours {
                enabled: true,
                start_time: "22:00".to_string(),
                end_time: "07:00".to_string(),
                timezone: "UTC".to_string(),
            },
        }
    }
}

impl OperatorPushSubscriptionState {
    pub fn validate(&self) -> Result<(), NotificationError> {
        self.quiet_hours.validate()?;
        if self.project_scope.trim().is_empty() {
            return Err(NotificationError::EmptyProjectScope);
        }
        if self.vapid_public_key_fingerprint.trim().is_empty() {
            return Err(NotificationError::EmptyFingerprint);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushTriggerKind {
    QuestionBadge,
    BlockerErrorBadge,
    UrgentWireMail,
    PrReviewRequests,
}

impl PushTriggerKind {
    fn enabled_by(self, triggers: &OperatorPushTriggers) -> bool {
        match self {
            Self::QuestionBadge => triggers.question_badge,
            Self::BlockerErrorBadge => triggers.blocker_error_badge,
            Self::UrgentWireMail => triggers.urgent_wire_mail,
            Self::PrReviewRequests => triggers.pr_review_requests,
        }
    }
}

/// Deliver when the trigger is on, the project is in scope (`"all"`
/// case-insensitive, or scope equals project case-insensitively; a `None`
/// project passes scope), and quiet hours are not active.
pub fn should_deliver(
    state: &OperatorPushSubscriptionState,
    kind: PushTriggerKind,
    project: Option<&str>,
    current_hh_mm: &str,
) -> bool {
    if !kind.enabled_by(&state.triggers) {
        return false;
    }
    let scope_passes = match project {
        None => true,
        Some(p) => {
            state.project_scope.eq_ignore_ascii_case("all")
                || state.project_scope.eq_ignore_ascii_case(p)
        }
    };
    scope_passes && !state.quiet_hours.is_quiet_at(current_hh_mm)
}

/// Reachable channels, in `vapid`, `apns`, `fcm` order.
pub fn active_channels(state: &OperatorPushSubscriptionState) -> Vec<&'static str> {
    let mut out = Vec::new();
    if state.vapid_service_worker_registered
        && state.web_push_permission == WebPushPermissionStatus::Granted
    {
        out.push("vapid");
    }
    if state.apns_ios_status == MobilePushChannelStatus::Active {
        out.push("apns");
    }
    if state.fcm_android_status == MobilePushChannelStatus::Active {
        out.push("fcm");
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushDispatchReceipt {
    pub deep_link: String,
    pub project_scope: Option<String>,
    pub channels: Vec<String>,
    pub quiet_hours_active: bool,
    pub dispatched: bool,
    pub at_ms: u64,
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

/// Stored operator push state, or `Default` when missing or corrupt.
/// The corrupt/invalid-to-`Default` fallback is spec'd (Ferris spec) and
/// deliberate; receipts differ — see [`append_push_receipt`].
pub fn get_operator_push_state(
    store: &impl StateStore,
) -> Result<OperatorPushSubscriptionState, StoreError> {
    let Some(value) = stored_value(store, OPERATOR_PUSH_STATE_SLUG)? else {
        return Ok(OperatorPushSubscriptionState::default());
    };
    let Ok(state) = serde_json::from_value::<OperatorPushSubscriptionState>(value) else {
        return Ok(OperatorPushSubscriptionState::default());
    };
    if state.validate().is_err() {
        return Ok(OperatorPushSubscriptionState::default());
    }
    Ok(state)
}

/// Validate, then persist the operator push state. `project_scope` is
/// stored trimmed (validation already rejects a trim-empty scope).
pub fn set_operator_push_state(
    store: &impl StateStore,
    mut state: OperatorPushSubscriptionState,
) -> Result<OperatorPushSubscriptionState, StoreError> {
    state.project_scope = state.project_scope.trim().to_string();
    state
        .validate()
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    let value = serde_json::to_value(&state).map_err(|e| StoreError::Backend(e.to_string()))?;
    upsert_runtime(store, OPERATOR_PUSH_STATE_SLUG, value)?;
    Ok(state)
}

/// All stored receipts, newest last. Missing/corrupt/unreadable yields an
/// empty vec: reads (GET events) stay tolerant of a corrupt stored value.
pub fn list_push_receipts(store: &impl StateStore) -> Vec<PushDispatchReceipt> {
    let Ok(Some(value)) = stored_value(store, OPERATOR_PUSH_RECEIPTS_SLUG) else {
        return Vec::new();
    };
    serde_json::from_value(value).unwrap_or_default()
}

/// Strict receipts read for the write path: missing yields an empty vec,
/// but a present-but-corrupt or unreadable value is an error.
fn stored_receipts_strict(store: &impl StateStore) -> Result<Vec<PushDispatchReceipt>, StoreError> {
    let Some(entry) = store.get_state_by_key(
        &StateScope::Workspace,
        StateConcern::Runtime,
        OPERATOR_PUSH_RECEIPTS_SLUG,
    )?
    else {
        return Ok(Vec::new());
    };
    let StatePayload::Runtime(rt) = &entry.payload else {
        return Err(StoreError::Backend(format!(
            "corrupt stored push receipts ({OPERATOR_PUSH_RECEIPTS_SLUG}): not a runtime entry; refusing to discard receipt history"
        )));
    };
    let value = rt.value.clone();
    serde_json::from_value(value).map_err(|e| {
        StoreError::Backend(format!(
            "corrupt stored push receipts ({OPERATOR_PUSH_RECEIPTS_SLUG}): {e}; refusing to discard receipt history"
        ))
    })
}

/// Append a receipt, keeping at most the newest 50. Unlike
/// [`list_push_receipts`], an existing corrupt/unreadable receipts value is
/// an error here — appending must never silently truncate receipt history.
pub fn append_push_receipt(
    store: &impl StateStore,
    receipt: PushDispatchReceipt,
) -> Result<(), StoreError> {
    let mut receipts = stored_receipts_strict(store)?;
    receipts.push(receipt);
    if receipts.len() > MAX_PUSH_RECEIPTS {
        receipts.drain(0..receipts.len() - MAX_PUSH_RECEIPTS);
    }
    let value = serde_json::to_value(&receipts).map_err(|e| StoreError::Backend(e.to_string()))?;
    upsert_runtime(store, OPERATOR_PUSH_RECEIPTS_SLUG, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;

    #[test]
    fn default_matches_ui_values() {
        let s = OperatorPushSubscriptionState::default();
        assert_eq!(s.web_push_permission, WebPushPermissionStatus::Granted);
        assert!(s.vapid_service_worker_registered);
        assert_eq!(s.vapid_public_key_fingerprint, "VAPID-SHA256:8f4e2b91c0d3");
        assert_eq!(s.apns_ios_status, MobilePushChannelStatus::Active);
        assert_eq!(s.fcm_android_status, MobilePushChannelStatus::Active);
        assert!(s.triggers.question_badge && s.triggers.blocker_error_badge);
        assert!(s.triggers.urgent_wire_mail && s.triggers.pr_review_requests);
        assert_eq!(s.project_scope, "all");
        assert!(s.quiet_hours.enabled);
        assert_eq!(s.quiet_hours.start_time, "22:00");
        assert_eq!(s.quiet_hours.end_time, "07:00");
        assert_eq!(s.quiet_hours.timezone, "UTC");
        // Serde wire forms match the UI unions.
        assert_eq!(
            serde_json::to_string(&s.web_push_permission).unwrap(),
            "\"granted\""
        );
        assert_eq!(
            serde_json::to_string(&s.apns_ios_status).unwrap(),
            "\"active\""
        );
        assert_eq!(
            serde_json::to_string(&PushTriggerKind::PrReviewRequests).unwrap(),
            "\"pr_review_requests\""
        );
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["quiet_hours"]["start_time"], "22:00");
    }

    fn quiet(enabled: bool, start: &str, end: &str) -> OperatorPushQuietHours {
        OperatorPushQuietHours {
            enabled,
            start_time: start.into(),
            end_time: end.into(),
            timezone: "UTC".into(),
        }
    }

    #[test]
    fn quiet_same_day_overnight_disabled_and_empty() {
        let day = quiet(true, "09:00", "17:00");
        assert!(day.is_quiet_at("09:00") && day.is_quiet_at("12:00"));
        assert!(!day.is_quiet_at("17:00") && !day.is_quiet_at("08:59"));
        let night = quiet(true, "22:00", "07:00");
        assert!(night.is_quiet_at("22:00") && night.is_quiet_at("23:30"));
        assert!(night.is_quiet_at("00:00") && night.is_quiet_at("06:59"));
        assert!(!night.is_quiet_at("07:00") && !night.is_quiet_at("12:00"));
        assert!(!quiet(false, "22:00", "07:00").is_quiet_at("23:00"));
        assert!(!quiet(true, "22:00", "22:00").is_quiet_at("22:00"));
        assert!(!night.is_quiet_at("bad") && !night.is_quiet_at("9:00"));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn hhmm_validation_rejects_bad_times() {
        for bad in ["24:00", "9:00", "aa:bb", "22:60", "2200", "22:0"] {
            assert!(quiet(true, bad, "07:00").validate().is_err(), "{bad}");
            assert!(quiet(true, "22:00", bad).validate().is_err(), "{bad}");
        }
        assert!(quiet(true, "22:00", "07:00").validate().is_ok());
        // Times are validated even when disabled.
        assert!(quiet(false, "24:00", "07:00").validate().is_err());
        assert!(quiet(false, "22:00", "07:60").validate().is_err());
        let mut q = quiet(true, "22:00", "07:00");
        q.timezone = " ".into();
        assert!(q.validate().is_err());
        // Only UTC (case-insensitive) until a timezone database lands.
        q.timezone = "America/New_York".into();
        let err = q.validate().unwrap_err().to_string();
        assert!(err.contains("only UTC"), "{err}");
        q.timezone = "utc".into();
        assert!(q.validate().is_ok());
        // Trim-empty fingerprint / scope are invalid.
        let mut s = OperatorPushSubscriptionState::default();
        s.vapid_public_key_fingerprint = "  ".into();
        assert!(s.validate().is_err());
        let mut s = OperatorPushSubscriptionState::default();
        s.project_scope = "  ".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn should_deliver_matrix() {
        let mut s = OperatorPushSubscriptionState::default();
        s.quiet_hours.enabled = false;
        assert!(should_deliver(
            &s,
            PushTriggerKind::QuestionBadge,
            Some("ferroloop"),
            "12:00"
        ));
        assert!(should_deliver(
            &s,
            PushTriggerKind::QuestionBadge,
            None,
            "12:00"
        ));
        s.triggers.question_badge = false;
        assert!(!should_deliver(
            &s,
            PushTriggerKind::QuestionBadge,
            Some("ferroloop"),
            "12:00"
        ));
        assert!(should_deliver(
            &s,
            PushTriggerKind::UrgentWireMail,
            Some("ferroloop"),
            "12:00"
        ));
        s.project_scope = "FerroLoop".into();
        assert!(should_deliver(
            &s,
            PushTriggerKind::UrgentWireMail,
            Some("ferroloop"),
            "12:00"
        ));
        assert!(!should_deliver(
            &s,
            PushTriggerKind::UrgentWireMail,
            Some("other"),
            "12:00"
        ));
        assert!(should_deliver(
            &s,
            PushTriggerKind::UrgentWireMail,
            None,
            "12:00"
        ));
        s.project_scope = "ALL".into();
        assert!(should_deliver(
            &s,
            PushTriggerKind::UrgentWireMail,
            Some("other"),
            "12:00"
        ));
        // Quiet hours block (default 22:00-07:00).
        s.quiet_hours.enabled = true;
        assert!(!should_deliver(
            &s,
            PushTriggerKind::UrgentWireMail,
            Some("other"),
            "23:00"
        ));
        assert!(should_deliver(
            &s,
            PushTriggerKind::UrgentWireMail,
            Some("other"),
            "12:00"
        ));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn channels_matrix() {
        let s = OperatorPushSubscriptionState::default();
        assert_eq!(active_channels(&s), vec!["vapid", "apns", "fcm"]);
        let mut s = OperatorPushSubscriptionState::default();
        s.web_push_permission = WebPushPermissionStatus::Denied;
        assert_eq!(active_channels(&s), vec!["apns", "fcm"]);
        s.vapid_service_worker_registered = false;
        s.web_push_permission = WebPushPermissionStatus::Granted;
        assert_eq!(active_channels(&s), vec!["apns", "fcm"]);
        s.apns_ios_status = MobilePushChannelStatus::Pending;
        s.fcm_android_status = MobilePushChannelStatus::Unregistered;
        assert!(active_channels(&s).is_empty());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn persistence_roundtrip_fallback_and_receipt_cap() {
        let store = MemStore::default();
        assert_eq!(
            get_operator_push_state(&store).unwrap(),
            OperatorPushSubscriptionState::default()
        );
        let mut s = OperatorPushSubscriptionState::default();
        s.project_scope = "ferroloop".into();
        set_operator_push_state(&store, s.clone()).unwrap();
        assert_eq!(get_operator_push_state(&store).unwrap(), s);
        // Invalid state is rejected on set.
        let mut bad = s.clone();
        bad.quiet_hours.start_time = "24:00".into();
        assert!(set_operator_push_state(&store, bad).is_err());
        // Receipts: newest last, capped at 50.
        assert!(list_push_receipts(&store).is_empty());
        for i in 0..55u64 {
            append_push_receipt(
                &store,
                PushDispatchReceipt {
                    deep_link: format!("/r/{i}"),
                    project_scope: Some("all".into()),
                    channels: vec!["vapid".into()],
                    quiet_hours_active: false,
                    dispatched: true,
                    at_ms: i,
                },
            )
            .unwrap();
        }
        let receipts = list_push_receipts(&store);
        assert_eq!(receipts.len(), 50);
        assert_eq!(receipts.first().unwrap().at_ms, 5);
        assert_eq!(receipts.last().unwrap().at_ms, 54);
    }

    #[test]
    fn corrupt_receipts_append_errors_but_list_tolerates() {
        let store = MemStore::default();
        upsert_runtime(
            &store,
            OPERATOR_PUSH_RECEIPTS_SLUG,
            serde_json::json!({"bogus": 1}),
        )
        .unwrap();
        assert!(list_push_receipts(&store).is_empty());
        let receipt = PushDispatchReceipt {
            deep_link: "/r/1".into(),
            project_scope: None,
            channels: vec!["vapid".into()],
            quiet_hours_active: false,
            dispatched: true,
            at_ms: 1,
        };
        assert!(append_push_receipt(&store, receipt).is_err());
        // History was not silently truncated/replaced.
        assert!(list_push_receipts(&store).is_empty());
        assert_eq!(
            stored_value(&store, OPERATOR_PUSH_RECEIPTS_SLUG)
                .unwrap()
                .unwrap(),
            serde_json::json!({"bogus": 1})
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn set_trims_project_scope() {
        let store = MemStore::default();
        let mut s = OperatorPushSubscriptionState::default();
        s.project_scope = "  ferroloop ".into();
        let saved = set_operator_push_state(&store, s).unwrap();
        assert_eq!(saved.project_scope, "ferroloop");
        assert_eq!(
            get_operator_push_state(&store).unwrap().project_scope,
            "ferroloop"
        );
    }

    #[test]
    fn corrupt_stored_state_falls_back_to_default() {
        let store = MemStore::default();
        upsert_runtime(
            &store,
            OPERATOR_PUSH_STATE_SLUG,
            serde_json::json!({"bogus": 1}),
        )
        .unwrap();
        assert_eq!(
            get_operator_push_state(&store).unwrap(),
            OperatorPushSubscriptionState::default()
        );
    }
}
