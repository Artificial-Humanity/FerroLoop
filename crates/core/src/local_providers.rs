//! Local model providers (Record #72).
//! Field names and defaults match `ui/src/components/SettingsModal.tsx`
//! (`LocalModelProviderPreset` / `DEFAULT_LOCAL_MODEL_PROVIDERS`).
//! Persisted as a JSON array in a Workspace Runtime entry.
//! Credentials are never raw keys: only `keyless (loopback)` or a
//! `vault://ferrogate/…` reference is accepted.

use crate::at::At;
use crate::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs, validate_slug,
};
use crate::store::{StateStore, StoreError};
use serde::{Deserialize, Serialize};

pub const LOCAL_MODEL_PROVIDERS_SLUG: &str = "local-model-providers";
pub const KEYLESS_LOOPBACK_REF: &str = "keyless (loopback)";
pub const VAULT_REF_PREFIX: &str = "vault://ferrogate/";

#[derive(Debug, thiserror::Error)]
pub enum LocalProviderError {
    #[error("id `{0}` is not a valid slug")]
    InvalidId(String),
    #[error("{0} must be non-empty")]
    EmptyField(&'static str),
    #[error(
        "endpoint_url `{0}` must be http(s):// with a non-empty host and no whitespace/control characters"
    )]
    InvalidEndpoint(String),
    #[error("context_window_tokens must be > 0")]
    InvalidContextWindow,
    #[error("max_output_tokens must be > 0 and <= context_window_tokens")]
    InvalidMaxOutput,
    #[error(
        "vault_credential_ref must be `keyless (loopback)` or `vault://ferrogate/<name>` (raw keys are never accepted)"
    )]
    InvalidVaultRef,
    #[error("id `{0}` already belongs to a different provider preset")]
    DuplicateId(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalModelProviderKind {
    Ollama,
    Lemonade,
    OpenaiCompatible,
}

impl LocalModelProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Lemonade => "lemonade",
            Self::OpenaiCompatible => "openai-compatible",
        }
    }
    fn default_label(&self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::Lemonade => "Lemonade",
            Self::OpenaiCompatible => "OpenAI-Compatible",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalLifecycleOp {
    SpinUp,
    Compact,
    Reforge,
    Recast,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalModelProviderPreset {
    pub id: String,
    pub kind: LocalModelProviderKind,
    pub label: String,
    pub endpoint_url: String,
    pub default_model: String,
    pub vault_credential_ref: String,
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    pub lifecycle_ops: Vec<LocalLifecycleOp>,
}

/// Replace any userinfo in `url` with `***` so errors never echo credentials.
fn redact_endpoint(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        if let Some((_, host_part)) = authority.rsplit_once('@') {
            return format!("{scheme}://***@{host_part}{}", &rest[authority_end..]);
        }
        return url.to_string();
    }
    if let Some((userinfo, host_part)) = url.rsplit_once('@')
        && userinfo.contains(':')
        && !userinfo.contains('/')
    {
        return format!("***@{host_part}");
    }
    url.to_string()
}

fn invalid_endpoint(url: &str) -> LocalProviderError {
    LocalProviderError::InvalidEndpoint(redact_endpoint(url))
}

/// Reject userinfo, percent-encoded host chars and `%20` anywhere (documented
/// simplification: encoded space is never needed in a provider endpoint).
fn validate_endpoint(url: &str) -> Result<(), LocalProviderError> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid_endpoint(url));
    }
    if url.to_ascii_lowercase().contains("%20") {
        return Err(invalid_endpoint(url));
    }
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return Err(invalid_endpoint(url));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(invalid_endpoint(url));
    }
    let host_name = if authority.starts_with('[') {
        authority
            .split(']')
            .next()
            .unwrap_or_default()
            .trim_start_matches('[')
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    if host_name.is_empty() || host_name.contains('%') {
        return Err(invalid_endpoint(url));
    }
    Ok(())
}

fn validate_vault_ref(v: &str) -> Result<(), LocalProviderError> {
    if v == KEYLESS_LOOPBACK_REF {
        return Ok(());
    }
    if let Some(rest) = v.strip_prefix(VAULT_REF_PREFIX)
        && !rest.is_empty()
        && !rest.chars().any(|c| c.is_whitespace() || c.is_control())
        && !rest.split('/').any(|seg| seg == "..")
    {
        return Ok(());
    }
    Err(LocalProviderError::InvalidVaultRef)
}

impl LocalModelProviderPreset {
    pub fn validate(&self) -> Result<(), LocalProviderError> {
        validate_slug(&self.id).map_err(|_| LocalProviderError::InvalidId(self.id.clone()))?;
        if self.label.trim().is_empty() {
            return Err(LocalProviderError::EmptyField("label"));
        }
        if self.default_model.trim().is_empty() {
            return Err(LocalProviderError::EmptyField("default_model"));
        }
        validate_endpoint(&self.endpoint_url)?;
        validate_vault_ref(&self.vault_credential_ref)?;
        if self.context_window_tokens == 0 {
            return Err(LocalProviderError::InvalidContextWindow);
        }
        if self.max_output_tokens == 0 || self.max_output_tokens > self.context_window_tokens {
            return Err(LocalProviderError::InvalidMaxOutput);
        }
        Ok(())
    }
}

/// Verbatim `DEFAULT_LOCAL_MODEL_PROVIDERS` from the UI.
pub fn default_local_model_providers() -> Vec<LocalModelProviderPreset> {
    let ops = || {
        vec![
            LocalLifecycleOp::SpinUp,
            LocalLifecycleOp::Compact,
            LocalLifecycleOp::Reforge,
            LocalLifecycleOp::Recast,
        ]
    };
    vec![
        LocalModelProviderPreset {
            id: "ollama-local".into(),
            kind: LocalModelProviderKind::Ollama,
            label: "Ollama Local Runtime".into(),
            endpoint_url: "http://127.0.0.1:11434/v1".into(),
            default_model: "qwen3-coder:30b".into(),
            vault_credential_ref: KEYLESS_LOOPBACK_REF.into(),
            context_window_tokens: 131072,
            max_output_tokens: 16384,
            lifecycle_ops: ops(),
        },
        LocalModelProviderPreset {
            id: "lemonade-rocm".into(),
            kind: LocalModelProviderKind::Lemonade,
            label: "Lemonade (AMD Ryzen AI / ROCm)".into(),
            endpoint_url: "http://127.0.0.1:8000/v1".into(),
            default_model: "Llama-3.3-70B-Instruct-ROCm".into(),
            vault_credential_ref: KEYLESS_LOOPBACK_REF.into(),
            context_window_tokens: 131072,
            max_output_tokens: 16384,
            lifecycle_ops: ops(),
        },
        LocalModelProviderPreset {
            id: "openai-compat-vllm".into(),
            kind: LocalModelProviderKind::OpenaiCompatible,
            label: "OpenAI-Compatible (vLLM / llama.cpp / SGLang)".into(),
            endpoint_url: "http://127.0.0.1:8080/v1".into(),
            default_model: "deepseek-r1-distill-qwen-32b".into(),
            vault_credential_ref: "vault://ferrogate/providers/local-vllm-key".into(),
            context_window_tokens: 65536,
            max_output_tokens: 8192,
            lifecycle_ops: ops(),
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalProviderUpdate {
    pub kind: LocalModelProviderKind,
    pub endpoint_url: String,
    pub vault_credential_ref: String,
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    pub id: Option<String>,
    pub label: Option<String>,
    pub default_model: Option<String>,
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
    presets: &[LocalModelProviderPreset],
) -> Result<(), StoreError> {
    let value = serde_json::to_value(presets).map_err(|e| StoreError::Backend(e.to_string()))?;
    upsert_runtime(store, LOCAL_MODEL_PROVIDERS_SLUG, value)
}

/// Strict read for write paths: missing yields defaults, corrupt errors.
fn list_local_model_providers_strict(
    store: &impl StateStore,
) -> Result<Vec<LocalModelProviderPreset>, StoreError> {
    let Some(entry) = store.get_state_by_key(
        &StateScope::Workspace,
        StateConcern::Runtime,
        LOCAL_MODEL_PROVIDERS_SLUG,
    )?
    else {
        return Ok(default_local_model_providers());
    };
    let StatePayload::Runtime(rt) = &entry.payload else {
        return Err(StoreError::Backend(format!(
            "corrupt stored local providers ({LOCAL_MODEL_PROVIDERS_SLUG}): not a runtime entry; refusing to overwrite"
        )));
    };
    let presets: Vec<LocalModelProviderPreset> =
        serde_json::from_value(rt.value.clone()).map_err(|e| {
            StoreError::Backend(format!(
                "corrupt stored local providers ({LOCAL_MODEL_PROVIDERS_SLUG}): {e}; refusing to overwrite"
            ))
        })?;
    if let Some(p) = presets.iter().find(|p| p.validate().is_err()) {
        return Err(StoreError::Backend(format!(
            "corrupt stored local providers ({LOCAL_MODEL_PROVIDERS_SLUG}): preset `{}` fails validation; refusing to overwrite",
            p.id
        )));
    }
    Ok(presets)
}

/// Stored providers, or defaults when missing or corrupt.
pub fn list_local_model_providers(
    store: &impl StateStore,
) -> Result<Vec<LocalModelProviderPreset>, StoreError> {
    let Some(value) = stored_value(store, LOCAL_MODEL_PROVIDERS_SLUG)? else {
        return Ok(default_local_model_providers());
    };
    let Ok(presets) = serde_json::from_value::<Vec<LocalModelProviderPreset>>(value) else {
        return Ok(default_local_model_providers());
    };
    if presets.iter().any(|p| p.validate().is_err()) {
        return Ok(default_local_model_providers());
    }
    Ok(presets)
}

/// Validate and persist `preset`, replacing by `id` or appending.
pub fn upsert_local_model_provider(
    store: &impl StateStore,
    preset: LocalModelProviderPreset,
) -> Result<LocalModelProviderPreset, StoreError> {
    preset
        .validate()
        .map_err(|e| StoreError::ProviderValidation(e.to_string()))?;
    let mut presets = list_local_model_providers_strict(store)?;
    if let Some(existing) = presets.iter_mut().find(|p| p.id == preset.id) {
        *existing = preset.clone();
    } else {
        presets.push(preset.clone());
    }
    persist(store, &presets)?;
    Ok(preset)
}

/// Update the preset for `update.kind`. Existing preset keeps its
/// id/label/default_model/lifecycle_ops unless optionals are given.
/// When no preset for the kind exists, one is created with id defaulting
/// to the kind slug and label/default_model defaults naming the kind.
pub fn update_local_model_provider_by_kind(
    store: &impl StateStore,
    update: LocalProviderUpdate,
) -> Result<LocalModelProviderPreset, StoreError> {
    let mut presets = list_local_model_providers_strict(store)?;
    if let Some(new_id) = &update.id {
        let target_idx = presets.iter().position(|p| p.kind == update.kind);
        if presets
            .iter()
            .enumerate()
            .any(|(i, p)| Some(i) != target_idx && &p.id == new_id)
        {
            return Err(StoreError::ProviderValidation(
                LocalProviderError::DuplicateId(new_id.clone()).to_string(),
            ));
        }
    }
    let preset = if let Some(existing) = presets.iter_mut().find(|p| p.kind == update.kind) {
        existing.endpoint_url = update.endpoint_url.clone();
        existing.vault_credential_ref = update.vault_credential_ref.clone();
        existing.context_window_tokens = update.context_window_tokens;
        existing.max_output_tokens = update.max_output_tokens;
        if let Some(id) = &update.id {
            existing.id = id.clone();
        }
        if let Some(label) = &update.label {
            existing.label = label.clone();
        }
        if let Some(model) = &update.default_model {
            existing.default_model = model.clone();
        }
        existing.clone()
    } else {
        let created = LocalModelProviderPreset {
            id: update
                .id
                .clone()
                .unwrap_or_else(|| update.kind.as_str().to_string()),
            kind: update.kind,
            label: update
                .label
                .clone()
                .unwrap_or_else(|| update.kind.default_label().to_string()),
            endpoint_url: update.endpoint_url.clone(),
            default_model: update
                .default_model
                .clone()
                .unwrap_or_else(|| format!("{}-default", update.kind.as_str())),
            vault_credential_ref: update.vault_credential_ref.clone(),
            context_window_tokens: update.context_window_tokens,
            max_output_tokens: update.max_output_tokens,
            lifecycle_ops: vec![
                LocalLifecycleOp::SpinUp,
                LocalLifecycleOp::Compact,
                LocalLifecycleOp::Reforge,
                LocalLifecycleOp::Recast,
            ],
        };
        presets.push(created.clone());
        created
    };
    preset
        .validate()
        .map_err(|e| StoreError::ProviderValidation(e.to_string()))?;
    persist(store, &presets)?;
    Ok(preset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;

    fn preset() -> LocalModelProviderPreset {
        default_local_model_providers()[0].clone()
    }

    #[test]
    fn local_defaults_verbatim() {
        let d = default_local_model_providers();
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].id, "ollama-local");
        assert_eq!(d[0].kind, LocalModelProviderKind::Ollama);
        assert_eq!(d[0].label, "Ollama Local Runtime");
        assert_eq!(d[0].endpoint_url, "http://127.0.0.1:11434/v1");
        assert_eq!(d[0].default_model, "qwen3-coder:30b");
        assert_eq!(d[0].vault_credential_ref, "keyless (loopback)");
        assert_eq!(
            (d[0].context_window_tokens, d[0].max_output_tokens),
            (131072, 16384)
        );
        assert_eq!(d[1].id, "lemonade-rocm");
        assert_eq!(d[1].endpoint_url, "http://127.0.0.1:8000/v1");
        assert_eq!(d[1].default_model, "Llama-3.3-70B-Instruct-ROCm");
        assert_eq!(d[2].id, "openai-compat-vllm");
        assert_eq!(d[2].kind, LocalModelProviderKind::OpenaiCompatible);
        assert_eq!(d[2].endpoint_url, "http://127.0.0.1:8080/v1");
        assert_eq!(d[2].default_model, "deepseek-r1-distill-qwen-32b");
        assert_eq!(
            d[2].vault_credential_ref,
            "vault://ferrogate/providers/local-vllm-key"
        );
        assert_eq!(
            (d[2].context_window_tokens, d[2].max_output_tokens),
            (65536, 8192)
        );
        for p in &d {
            assert_eq!(p.lifecycle_ops.len(), 4);
            p.validate().unwrap();
        }
        assert_eq!(
            serde_json::to_string(&LocalModelProviderKind::OpenaiCompatible).unwrap(),
            "\"openai-compatible\""
        );
        assert_eq!(
            serde_json::to_string(&LocalLifecycleOp::SpinUp).unwrap(),
            "\"spin-up\""
        );
    }

    #[test]
    fn local_validation_matrix() {
        let mut p = preset();
        p.endpoint_url = "ftp://127.0.0.1/v1".into();
        assert!(p.validate().is_err());
        p.endpoint_url = "http:///v1".into();
        assert!(p.validate().is_err());
        p.endpoint_url = "http://127.0.0.1:11434 /v1".into();
        assert!(p.validate().is_err());
        p.endpoint_url = "127.0.0.1:11434".into();
        assert!(p.validate().is_err());
        p = preset();
        p.context_window_tokens = 0;
        assert!(p.validate().is_err());
        p = preset();
        p.max_output_tokens = 0;
        assert!(p.validate().is_err());
        p.max_output_tokens = p.context_window_tokens + 1;
        assert!(p.validate().is_err());
        p = preset();
        p.vault_credential_ref = "sk-1234567890abcdef".into();
        assert!(p.validate().is_err());
        p.vault_credential_ref = "Bearer abc".into();
        assert!(p.validate().is_err());
        p.vault_credential_ref = "vault://ferrogate/".into();
        assert!(p.validate().is_err());
        p.vault_credential_ref = "vault://ferrogate/providers/k".into();
        assert!(p.validate().is_ok());
        p.vault_credential_ref = "keyless (loopback)".into();
        assert!(p.validate().is_ok());
        p = preset();
        p.id = "Bad Slug!".into();
        assert!(p.validate().is_err());
        p = preset();
        p.label = " ".into();
        assert!(p.validate().is_err());
        p = preset();
        p.default_model = "".into();
        assert!(p.validate().is_err());
        assert!(preset().validate().is_ok());
    }

    #[test]
    fn local_update_by_kind_keeps_id_and_roundtrip() {
        let store = MemStore::default();
        assert_eq!(
            list_local_model_providers(&store).unwrap(),
            default_local_model_providers()
        );
        let updated = update_local_model_provider_by_kind(
            &store,
            LocalProviderUpdate {
                kind: LocalModelProviderKind::Ollama,
                endpoint_url: "http://127.0.0.1:9999/v1".into(),
                vault_credential_ref: "vault://ferrogate/providers/ollama".into(),
                context_window_tokens: 65536,
                max_output_tokens: 4096,
                id: None,
                label: None,
                default_model: None,
            },
        )
        .unwrap();
        assert_eq!(updated.id, "ollama-local");
        assert_eq!(updated.label, "Ollama Local Runtime");
        assert_eq!(updated.default_model, "qwen3-coder:30b");
        assert_eq!(updated.endpoint_url, "http://127.0.0.1:9999/v1");
        let list = list_local_model_providers(&store).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].endpoint_url, "http://127.0.0.1:9999/v1");
        // Invalid update rejected.
        assert!(
            update_local_model_provider_by_kind(
                &store,
                LocalProviderUpdate {
                    kind: LocalModelProviderKind::Ollama,
                    endpoint_url: "ftp://x".into(),
                    vault_credential_ref: "keyless (loopback)".into(),
                    context_window_tokens: 10,
                    max_output_tokens: 11,
                    id: None,
                    label: None,
                    default_model: None,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn local_upsert_append_replace_roundtrip() {
        let store = MemStore::default();
        let mut p = preset();
        p.id = "custom-ollama".into();
        p.label = "Custom".into();
        upsert_local_model_provider(&store, p.clone()).unwrap();
        assert_eq!(list_local_model_providers(&store).unwrap().len(), 4);
        p.label = "Custom2".into();
        upsert_local_model_provider(&store, p).unwrap();
        let list = list_local_model_providers(&store).unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(
            list.iter().find(|x| x.id == "custom-ollama").unwrap().label,
            "Custom2"
        );
        let mut bad = preset();
        bad.vault_credential_ref = "sk-raw".into();
        assert!(upsert_local_model_provider(&store, bad).is_err());
    }

    #[test]
    fn local_corrupt_falls_back_to_defaults() {
        let store = MemStore::default();
        upsert_runtime(
            &store,
            LOCAL_MODEL_PROVIDERS_SLUG,
            serde_json::json!([{"bogus": 1}]),
        )
        .unwrap();
        assert_eq!(
            list_local_model_providers(&store).unwrap(),
            default_local_model_providers()
        );
    }
    #[test]
    fn local_endpoint_rejects_credentials_percent_and_encoded_space() {
        let mut p = preset();
        for bad in [
            "http://user:secretpass@127.0.0.1:11434/v1",
            "http://user@127.0.0.1:11434/v1",
            "http://127.0.0.1%2e:11434/v1",
            "http://%31%32%37.0.0.1/v1",
            "http://127.0.0.1:11434/v1%20x",
            "http://127.0.0.1:11434/v%20",
            "https://:11434/v1",
            "http://\t127.0.0.1/v1",
        ] {
            p.endpoint_url = bad.into();
            let err = p.validate().unwrap_err().to_string();
            assert!(!err.contains("secretpass"), "{err}");
            assert!(p.validate().is_err(), "{bad}");
        }
        p.endpoint_url = "http://127.0.0.1:11434/v1".into();
        assert!(p.validate().is_ok());
    }

    #[test]
    fn local_vault_ref_rejects_traversal_and_whitespace() {
        let mut p = preset();
        for bad in [
            "vault://ferrogate/../secret",
            "vault://ferrogate/a/../b",
            "vault://ferrogate/a b",
            "vault://ferrogate/a\tb",
        ] {
            p.vault_credential_ref = bad.into();
            assert!(p.validate().is_err(), "{bad}");
        }
        p.vault_credential_ref = "vault://ferrogate/providers/k".into();
        assert!(p.validate().is_ok());
    }

    #[test]
    fn local_update_by_kind_id_conflict() {
        let store = MemStore::default();
        let mk = |id: Option<String>| LocalProviderUpdate {
            kind: LocalModelProviderKind::Ollama,
            endpoint_url: "http://127.0.0.1:9999/v1".into(),
            vault_credential_ref: "keyless (loopback)".into(),
            context_window_tokens: 65536,
            max_output_tokens: 4096,
            id,
            label: None,
            default_model: None,
        };
        // Collides with lemonade-rocm (different preset)
        let err = update_local_model_provider_by_kind(&store, mk(Some("lemonade-rocm".into())))
            .unwrap_err();
        assert!(matches!(err, StoreError::ProviderValidation(_)), "{err:?}");
        // Own id ok
        assert!(
            update_local_model_provider_by_kind(&store, mk(Some("ollama-local".into()))).is_ok()
        );
        // Free id ok
        assert!(
            update_local_model_provider_by_kind(&store, mk(Some("ollama-renamed".into()))).is_ok()
        );
    }

    #[test]
    fn local_corrupt_preserved_after_failed_writes() {
        let store = MemStore::default();
        let corrupt = serde_json::json!([{"bogus": 1}]);
        upsert_runtime(&store, LOCAL_MODEL_PROVIDERS_SLUG, corrupt.clone()).unwrap();
        assert_eq!(
            list_local_model_providers(&store).unwrap(),
            default_local_model_providers()
        );
        assert!(upsert_local_model_provider(&store, preset()).is_err());
        assert!(
            update_local_model_provider_by_kind(
                &store,
                LocalProviderUpdate {
                    kind: LocalModelProviderKind::Ollama,
                    endpoint_url: "http://127.0.0.1:9999/v1".into(),
                    vault_credential_ref: "keyless (loopback)".into(),
                    context_window_tokens: 10,
                    max_output_tokens: 1,
                    id: None,
                    label: None,
                    default_model: None,
                }
            )
            .is_err()
        );
        assert_eq!(
            stored_value(&store, LOCAL_MODEL_PROVIDERS_SLUG).unwrap(),
            Some(corrupt)
        );
    }
}
