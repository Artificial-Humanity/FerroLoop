//! Dedicated artifact layout & retention (Records #77/#186/#207).
//! Field names and defaults match `ui/src/components/SettingsModal.tsx`
//! (`DedicatedArtifactLayoutState` / `DEFAULT_DEDICATED_ARTIFACT_LAYOUT`).
//! Persisted as JSON in a Workspace Runtime entry.
//!
//! Persistence semantics:
//! - Reads ([`get_artifact_layout`]) are tolerant: missing or corrupt
//!   stored values fall back to `Default`.
//! - [`get_artifact_layout_strict`] is the strict getter: missing yields
//!   `Default`, but a present-but-corrupt stored value is an error.
//! - Writes ([`set_artifact_layout`], [`consolidate_artifact_layout`]) are
//!   strict: a present-but-corrupt stored value is an error and is never
//!   overwritten blindly (mirrors `local_providers.rs`).

use crate::at::At;
use crate::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use crate::store::{StateStore, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DEDICATED_ARTIFACT_LAYOUT_SLUG: &str = "dedicated-artifact-layout";

#[derive(Debug, thiserror::Error)]
pub enum ArtifactLayoutError {
    #[error("{0} must be non-empty")]
    EmptyField(&'static str),
    #[error("{0} must contain no control characters")]
    ControlChars(&'static str),
    #[error("keep_latest_versions must be in 1..=1000, got {0}")]
    InvalidKeepLatestVersions(u32),
    #[error("auto_archive_days must be in 1..=3650, got {0}")]
    InvalidAutoArchiveDays(u32),
    #[error(
        "by_kind_bytes is missing kind `{0}` (expected exactly the kinds: image, markdown, diff, log, bundle)"
    )]
    MissingByKindKind(&'static str),
    #[error(
        "by_kind_bytes contains unknown kind `{0}` (expected exactly the kinds: image, markdown, diff, log, bundle; remove or rename `{0}`)"
    )]
    UnknownByKindKind(String),
}

/// The only keys allowed in `by_kind_bytes`, all of which are required.
pub const ARTIFACT_KINDS: [&str; 5] = ["image", "markdown", "diff", "log", "bundle"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactScatterVendor {
    Antigravity,
    Claude,
    Codex,
}

impl ArtifactScatterVendor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Antigravity => "antigravity",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// Alias matching the UI's vendor naming.
pub type ScatterVendor = ArtifactScatterVendor;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorArtifactScatterOrigin {
    pub vendor: ArtifactScatterVendor,
    pub vendor_label: String,
    pub legacy_path: String,
    pub discovered_files: u64,
    pub discovered_bytes: u64,
    pub consolidated: bool,
}

fn validate_text(field: &'static str, v: &str) -> Result<(), ArtifactLayoutError> {
    if v.trim().is_empty() {
        return Err(ArtifactLayoutError::EmptyField(field));
    }
    if v.chars().any(|c| c.is_control()) {
        return Err(ArtifactLayoutError::ControlChars(field));
    }
    Ok(())
}

impl VendorArtifactScatterOrigin {
    pub fn validate(&self) -> Result<(), ArtifactLayoutError> {
        validate_text("vendor_label", &self.vendor_label)?;
        validate_text("legacy_path", &self.legacy_path)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRetentionPolicyConfig {
    pub keep_latest_versions: u32,
    pub auto_archive_days: u32,
    pub pin_ratified_specs: bool,
}

impl ArtifactRetentionPolicyConfig {
    pub fn validate(&self) -> Result<(), ArtifactLayoutError> {
        if !(1..=1000).contains(&self.keep_latest_versions) {
            return Err(ArtifactLayoutError::InvalidKeepLatestVersions(
                self.keep_latest_versions,
            ));
        }
        if !(1..=3650).contains(&self.auto_archive_days) {
            return Err(ArtifactLayoutError::InvalidAutoArchiveDays(
                self.auto_archive_days,
            ));
        }
        Ok(())
    }
}

impl Default for ArtifactRetentionPolicyConfig {
    fn default() -> Self {
        Self {
            keep_latest_versions: 5,
            auto_archive_days: 30,
            pin_ratified_specs: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedicatedArtifactLayoutState {
    pub canonical_root: String,
    pub session_layout_pattern: String,
    pub vendor_origins: Vec<VendorArtifactScatterOrigin>,
    pub by_kind_bytes: BTreeMap<String, u64>,
    pub by_project_bytes: BTreeMap<String, u64>,
    pub retention_policy: ArtifactRetentionPolicyConfig,
}

fn validate_by_kind_bytes(
    by_kind_bytes: &BTreeMap<String, u64>,
) -> Result<(), ArtifactLayoutError> {
    for kind in ARTIFACT_KINDS {
        if !by_kind_bytes.contains_key(kind) {
            return Err(ArtifactLayoutError::MissingByKindKind(kind));
        }
    }
    for key in by_kind_bytes.keys() {
        if !ARTIFACT_KINDS.contains(&key.as_str()) {
            return Err(ArtifactLayoutError::UnknownByKindKind(key.clone()));
        }
    }
    Ok(())
}

impl DedicatedArtifactLayoutState {
    pub fn validate(&self) -> Result<(), ArtifactLayoutError> {
        validate_text("canonical_root", &self.canonical_root)?;
        validate_text("session_layout_pattern", &self.session_layout_pattern)?;
        for origin in &self.vendor_origins {
            origin.validate()?;
        }
        validate_by_kind_bytes(&self.by_kind_bytes)?;
        self.retention_policy.validate()?;
        Ok(())
    }
}

/// Verbatim `DEFAULT_DEDICATED_ARTIFACT_LAYOUT` from the UI.
impl Default for DedicatedArtifactLayoutState {
    fn default() -> Self {
        Self {
            canonical_root: "~/.local/share/fl/artifacts/".into(),
            session_layout_pattern: "[home]/artifacts/[project-name]/[session-id]/".into(),
            vendor_origins: vec![
                VendorArtifactScatterOrigin {
                    vendor: ArtifactScatterVendor::Antigravity,
                    vendor_label: "Antigravity CLI Brain".into(),
                    legacy_path: "~/.gemini/antigravity-cli/brain/".into(),
                    discovered_files: 18,
                    discovered_bytes: 4194304,
                    consolidated: false,
                },
                VendorArtifactScatterOrigin {
                    vendor: ArtifactScatterVendor::Claude,
                    vendor_label: "Claude Code Projects".into(),
                    legacy_path: "~/.claude/projects/".into(),
                    discovered_files: 11,
                    discovered_bytes: 1572864,
                    consolidated: false,
                },
                VendorArtifactScatterOrigin {
                    vendor: ArtifactScatterVendor::Codex,
                    vendor_label: "Codex CLI Sessions".into(),
                    legacy_path: "~/.codex/sessions/".into(),
                    discovered_files: 7,
                    discovered_bytes: 851968,
                    consolidated: true,
                },
            ],
            by_kind_bytes: BTreeMap::from([
                ("image".to_string(), 3145728),
                ("markdown".to_string(), 1441792),
                ("diff".to_string(), 917504),
                ("log".to_string(), 655360),
                ("bundle".to_string(), 458752),
            ]),
            by_project_bytes: BTreeMap::from([
                ("FerroLoop".to_string(), 3670016),
                ("FerroDeck".to_string(), 1835008),
                ("Sonora".to_string(), 1114112),
            ]),
            retention_policy: ArtifactRetentionPolicyConfig::default(),
        }
    }
}

/// `(files, bytes)` still scattered: totals over unconsolidated origins.
pub fn scattered_totals(state: &DedicatedArtifactLayoutState) -> (u64, u64) {
    state
        .vendor_origins
        .iter()
        .filter(|o| !o.consolidated)
        .fold((0u64, 0u64), |(f, b), o| {
            (
                f.saturating_add(o.discovered_files),
                b.saturating_add(o.discovered_bytes),
            )
        })
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
    state: &DedicatedArtifactLayoutState,
) -> Result<(), StoreError> {
    let value = serde_json::to_value(state).map_err(|e| StoreError::Backend(e.to_string()))?;
    upsert_runtime(store, DEDICATED_ARTIFACT_LAYOUT_SLUG, value)
}

/// Strict read: missing yields `Default`, corrupt errors. Write paths and
/// dry-run previews use this so corrupt state is never silently treated
/// as `Default`.
pub fn get_artifact_layout_strict(
    store: &impl StateStore,
) -> Result<DedicatedArtifactLayoutState, StoreError> {
    let Some(entry) = store.get_state_by_key(
        &StateScope::Workspace,
        StateConcern::Runtime,
        DEDICATED_ARTIFACT_LAYOUT_SLUG,
    )?
    else {
        return Ok(DedicatedArtifactLayoutState::default());
    };
    let StatePayload::Runtime(rt) = &entry.payload else {
        return Err(StoreError::Backend(format!(
            "corrupt stored artifact layout ({DEDICATED_ARTIFACT_LAYOUT_SLUG}): not a runtime entry; refusing to overwrite"
        )));
    };
    let state: DedicatedArtifactLayoutState =
        serde_json::from_value(rt.value.clone()).map_err(|e| {
            StoreError::Backend(format!(
                "corrupt stored artifact layout ({DEDICATED_ARTIFACT_LAYOUT_SLUG}): {e}; refusing to overwrite"
            ))
        })?;
    state.validate().map_err(|e| {
        StoreError::Backend(format!(
            "corrupt stored artifact layout ({DEDICATED_ARTIFACT_LAYOUT_SLUG}): {e}; refusing to overwrite"
        ))
    })?;
    Ok(state)
}

/// Stored layout, or `Default` when missing or corrupt.
pub fn get_artifact_layout(
    store: &impl StateStore,
) -> Result<DedicatedArtifactLayoutState, StoreError> {
    let Some(value) = stored_value(store, DEDICATED_ARTIFACT_LAYOUT_SLUG)? else {
        return Ok(DedicatedArtifactLayoutState::default());
    };
    let Ok(state) = serde_json::from_value::<DedicatedArtifactLayoutState>(value) else {
        return Ok(DedicatedArtifactLayoutState::default());
    };
    if state.validate().is_err() {
        return Ok(DedicatedArtifactLayoutState::default());
    }
    Ok(state)
}

/// Validate and persist `state`. Strict: a present-but-corrupt stored
/// value is an error and is never overwritten blindly.
pub fn set_artifact_layout(
    store: &impl StateStore,
    state: DedicatedArtifactLayoutState,
) -> Result<DedicatedArtifactLayoutState, StoreError> {
    state
        .validate()
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    // Strict check of existing value before overwriting.
    let _ = get_artifact_layout_strict(store)?;
    persist(store, &state)?;
    Ok(state)
}

/// Strict-read current layout, apply `retention` if given (validated),
/// set `canonical_root` to the resolved root if given, mark every origin
/// consolidated, persist, and return
/// `(updated_state, scattered_files, scattered_bytes)` as they were
/// before consolidation. A second call therefore returns `(state, 0, 0)`.
///
/// The caller resolves `canonical_root` (the hub passes its effective
/// system dir when the stored root is the `Default` placeholder, else the
/// stored root); persisting it here makes the root stable even if the
/// caller's settings later change.
pub fn consolidate_artifact_layout(
    store: &impl StateStore,
    retention: Option<ArtifactRetentionPolicyConfig>,
    canonical_root: Option<String>,
) -> Result<(DedicatedArtifactLayoutState, u64, u64), StoreError> {
    if let Some(r) = &retention {
        r.validate()
            .map_err(|e| StoreError::Backend(e.to_string()))?;
    }
    let mut state = get_artifact_layout_strict(store)?;
    let (files, bytes) = scattered_totals(&state);
    if let Some(r) = retention {
        state.retention_policy = r;
    }
    if let Some(root) = canonical_root {
        state.canonical_root = root;
    }
    for origin in &mut state.vendor_origins {
        origin.consolidated = true;
    }
    state
        .validate()
        .map_err(|e| StoreError::Backend(e.to_string()))?;
    persist(store, &state)?;
    Ok((state, files, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;

    #[test]
    fn artifact_default_verbatim() {
        let d = DedicatedArtifactLayoutState::default();
        assert_eq!(d.canonical_root, "~/.local/share/fl/artifacts/");
        assert_eq!(
            d.session_layout_pattern,
            "[home]/artifacts/[project-name]/[session-id]/"
        );
        assert_eq!(d.vendor_origins.len(), 3);
        assert_eq!(
            d.vendor_origins[0].vendor,
            ArtifactScatterVendor::Antigravity
        );
        assert_eq!(d.vendor_origins[0].vendor_label, "Antigravity CLI Brain");
        assert_eq!(
            d.vendor_origins[0].legacy_path,
            "~/.gemini/antigravity-cli/brain/"
        );
        assert_eq!(
            (
                d.vendor_origins[0].discovered_files,
                d.vendor_origins[0].discovered_bytes,
                d.vendor_origins[0].consolidated
            ),
            (18, 4194304, false)
        );
        assert_eq!(d.vendor_origins[1].vendor, ArtifactScatterVendor::Claude);
        assert_eq!(
            (
                d.vendor_origins[1].discovered_files,
                d.vendor_origins[1].discovered_bytes,
                d.vendor_origins[1].consolidated
            ),
            (11, 1572864, false)
        );
        assert_eq!(d.vendor_origins[2].vendor, ArtifactScatterVendor::Codex);
        assert!(d.vendor_origins[2].consolidated);
        assert_eq!(
            (
                d.vendor_origins[2].discovered_files,
                d.vendor_origins[2].discovered_bytes
            ),
            (7, 851968)
        );
        assert_eq!(d.by_kind_bytes["image"], 3145728);
        assert_eq!(d.by_kind_bytes["markdown"], 1441792);
        assert_eq!(d.by_kind_bytes["diff"], 917504);
        assert_eq!(d.by_kind_bytes["log"], 655360);
        assert_eq!(d.by_kind_bytes["bundle"], 458752);
        assert_eq!(d.by_project_bytes["FerroLoop"], 3670016);
        assert_eq!(d.by_project_bytes["FerroDeck"], 1835008);
        assert_eq!(d.by_project_bytes["Sonora"], 1114112);
        assert_eq!(
            (
                d.retention_policy.keep_latest_versions,
                d.retention_policy.auto_archive_days,
                d.retention_policy.pin_ratified_specs
            ),
            (5, 30, true)
        );
        d.validate().unwrap();
        assert_eq!(
            serde_json::to_string(&ArtifactScatterVendor::Antigravity).unwrap(),
            "\"antigravity\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactScatterVendor::Codex).unwrap(),
            "\"codex\""
        );
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["vendor_origins"][0]["vendor"], "antigravity");
        assert!(v["by_kind_bytes"].is_object());
    }

    #[test]
    fn artifact_scattered_totals_default() {
        assert_eq!(
            scattered_totals(&DedicatedArtifactLayoutState::default()),
            (29, 4194304 + 1572864)
        );
    }

    #[test]
    fn artifact_retention_range_rejects() {
        for (keep, days) in [(0, 30), (1001, 30), (5, 0), (5, 3651)] {
            let r = ArtifactRetentionPolicyConfig {
                keep_latest_versions: keep,
                auto_archive_days: days,
                pin_ratified_specs: true,
            };
            assert!(r.validate().is_err(), "{keep}/{days}");
            let s = DedicatedArtifactLayoutState {
                retention_policy: r,
                ..Default::default()
            };
            assert!(s.validate().is_err());
        }
        for (keep, days) in [(1, 1), (1000, 3650)] {
            assert!(
                ArtifactRetentionPolicyConfig {
                    keep_latest_versions: keep,
                    auto_archive_days: days,
                    pin_ratified_specs: false
                }
                .validate()
                .is_ok()
            );
        }
    }

    #[test]
    fn artifact_validate_rejects_empty_and_control() {
        let mut s = DedicatedArtifactLayoutState {
            canonical_root: "  ".into(),
            ..Default::default()
        };
        assert!(s.validate().is_err());
        s = DedicatedArtifactLayoutState {
            session_layout_pattern: "".into(),
            ..Default::default()
        };
        assert!(s.validate().is_err());
        s.vendor_origins[0].vendor_label = " ".into();
        assert!(s.validate().is_err());
        s.vendor_origins[0].legacy_path = "a\u{1}b".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn artifact_set_get_roundtrip() {
        let store = MemStore::default();
        assert_eq!(
            get_artifact_layout(&store).unwrap(),
            DedicatedArtifactLayoutState::default()
        );
        let s = DedicatedArtifactLayoutState {
            canonical_root: "/tmp/artifacts/".into(),
            ..Default::default()
        };
        set_artifact_layout(&store, s.clone()).unwrap();
        assert_eq!(get_artifact_layout(&store).unwrap(), s);
        let bad = DedicatedArtifactLayoutState {
            retention_policy: ArtifactRetentionPolicyConfig {
                keep_latest_versions: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(set_artifact_layout(&store, bad).is_err());
    }

    #[test]
    fn artifact_consolidate_flips_all_and_second_is_zero() {
        let store = MemStore::default();
        let (state, files, bytes) = consolidate_artifact_layout(&store, None, None).unwrap();
        assert_eq!((files, bytes), (29, 4194304 + 1572864));
        assert!(state.vendor_origins.iter().all(|o| o.consolidated));
        assert_eq!(scattered_totals(&state), (0, 0));
        assert_eq!(get_artifact_layout(&store).unwrap(), state);
        let (state2, f2, b2) = consolidate_artifact_layout(
            &store,
            Some(ArtifactRetentionPolicyConfig {
                keep_latest_versions: 3,
                auto_archive_days: 7,
                pin_ratified_specs: false,
            }),
            None,
        )
        .unwrap();
        assert_eq!((f2, b2), (0, 0));
        assert_eq!(state2.retention_policy.keep_latest_versions, 3);
        // Invalid retention rejected.
        assert!(
            consolidate_artifact_layout(
                &store,
                Some(ArtifactRetentionPolicyConfig {
                    keep_latest_versions: 0,
                    auto_archive_days: 7,
                    pin_ratified_specs: false
                }),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn artifact_corrupt_write_protection() {
        let store = MemStore::default();
        let corrupt = serde_json::json!({"bogus": 1});
        upsert_runtime(&store, DEDICATED_ARTIFACT_LAYOUT_SLUG, corrupt.clone()).unwrap();
        // Read falls back, writes error and preserve corrupt value.
        assert_eq!(
            get_artifact_layout(&store).unwrap(),
            DedicatedArtifactLayoutState::default()
        );
        assert!(set_artifact_layout(&store, DedicatedArtifactLayoutState::default()).is_err());
        assert!(get_artifact_layout_strict(&store).is_err());
        assert!(consolidate_artifact_layout(&store, None, None).is_err());
        assert_eq!(
            stored_value(&store, DEDICATED_ARTIFACT_LAYOUT_SLUG).unwrap(),
            Some(corrupt)
        );
    }

    #[test]
    fn artifact_consolidate_persists_resolved_canonical_root() {
        let store = MemStore::default();
        let (state, _, _) =
            consolidate_artifact_layout(&store, None, Some("/data/artifacts/".to_string()))
                .unwrap();
        assert_eq!(state.canonical_root, "/data/artifacts/");
        assert_eq!(
            get_artifact_layout(&store).unwrap().canonical_root,
            "/data/artifacts/"
        );
        // An empty resolved root is rejected and the persisted root survives.
        assert!(consolidate_artifact_layout(&store, None, Some(" ".into())).is_err());
        assert_eq!(
            get_artifact_layout(&store).unwrap().canonical_root,
            "/data/artifacts/"
        );
        // No root keeps the persisted one.
        let (again, _, _) = consolidate_artifact_layout(&store, None, None).unwrap();
        assert_eq!(again.canonical_root, "/data/artifacts/");
    }

    #[test]
    fn artifact_by_kind_bytes_requires_exactly_the_known_kinds() {
        let mut s = DedicatedArtifactLayoutState::default();
        s.by_kind_bytes.remove("log");
        let err = s.validate().unwrap_err().to_string();
        assert!(err.contains("missing kind `log`"), "{err}");
        assert!(err.contains("image, markdown, diff, log, bundle"), "{err}");
        let store = MemStore::default();
        assert!(set_artifact_layout(&store, s).is_err());

        let mut s = DedicatedArtifactLayoutState::default();
        s.by_kind_bytes.insert("video".into(), 1);
        let err = s.validate().unwrap_err().to_string();
        assert!(err.contains("unknown kind `video`"), "{err}");
        assert!(err.contains("remove or rename `video`"), "{err}");
        assert!(set_artifact_layout(&store, s).is_err());
        assert_eq!(
            get_artifact_layout(&store).unwrap(),
            DedicatedArtifactLayoutState::default()
        );
    }
}
