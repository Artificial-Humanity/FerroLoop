//! Strict, pinned source-derived registration evidence.
//!
//! This deliberately does not use `RouteTable::load`: permissive legacy recovery and
//! passive discovery cannot establish the provenance needed for terminal mutation.
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::identity::{
    RouteEntry, RouteTable, normalize_persona, parse_hosted_identity, route_entry_proven_live,
};

// Unlike Value's normal deserializer, reject duplicate keys at *every* nesting level.
struct UniqueJson(Value);
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate keys")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(v.into())))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(v.into())))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Number::from_f64(v)
                    .map(|v| UniqueJson(Value::Number(v)))
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(v.into())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(v)))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                UniqueJson::deserialize(d)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueJson(v)) = seq.next_element()? {
                    values.push(v);
                }
                Ok(UniqueJson(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some((key, UniqueJson(v))) = map.next_entry::<String, UniqueJson>()? {
                    if values.insert(key, v).is_some() {
                        return Err(de::Error::custom("duplicate JSON key"));
                    }
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }
        d.deserialize_any(UniqueVisitor)
    }
}

fn strict_table(source: &Path) -> Result<RouteTable, &'static str> {
    let raw = std::fs::read(source)
        .map_err(|_| "registration source unavailable; re-register and resubmit")?;
    let UniqueJson(value) = serde_json::from_slice(&raw)
        .map_err(|_| "invalid or ambiguous registration source; re-register and resubmit")?;
    let entries = value
        .get("entries")
        .and_then(Value::as_object)
        .ok_or("invalid registration source; re-register and resubmit")?;
    let mut aliases = BTreeSet::new();
    let mut personas = BTreeSet::new();
    for (key, entry) in entries {
        let alias = normalize_persona(key)
            .ok_or("invalid registration source; re-register and resubmit")?;
        if !aliases.insert(alias) {
            return Err("ambiguous registration source; re-register and resubmit");
        }
        // Modern typed entries only; never reconstruct explicit identity from legacy defaults.
        if !entry.is_object() || entry.get("route").is_none() {
            return Err("incomplete registration source; re-register and resubmit");
        }
        let persona = entry
            .get("persona")
            .and_then(Value::as_str)
            .and_then(normalize_persona)
            .ok_or("invalid registration source; re-register and resubmit")?;
        if !personas.insert(persona) {
            return Err("ambiguous registration source; re-register and resubmit");
        }
    }
    serde_json::from_value(value)
        .map_err(|_| "invalid registration source; re-register and resubmit")
}

/// A source failure is distinct from a source-backed identity refusal, so an
/// independently classified non-Claude request can only bypass the former.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    Source(&'static str),
    Refused(&'static str),
}
impl From<&'static str> for RegistrationError {
    fn from(message: &'static str) -> Self {
        Self::Refused(message)
    }
}
impl fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(message) | Self::Refused(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for RegistrationError {}

/// A source-backed registration snapshot. Only the adapter facade can acquire one;
/// raw inputs express intent and never constitute authority.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RegistrationBinding {
    source: PathBuf,
    entry: RouteEntry,
    persona: String,
    native_id: String,
}
impl fmt::Debug for RegistrationBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RegistrationBinding(<redacted>)")
    }
}
impl RegistrationBinding {
    pub(crate) fn validate_source(source: &Path) -> Result<(), RegistrationError> {
        strict_table(source)
            .map(|_| ())
            .map_err(RegistrationError::Source)
    }
    pub(crate) fn from_intent(
        source: PathBuf,
        intent: &str,
        policy: &crate::vendor_adapter::HostedPastePolicy,
    ) -> Result<Option<Self>, RegistrationError> {
        let adapter = policy.adapter();
        let table = strict_table(&source).map_err(RegistrationError::Source)?;
        let intent = intent.trim();
        let canonical = parse_hosted_identity(intent).ok();
        let bare = normalize_persona(intent);
        let mut found = Vec::new();
        for (key, entry) in &table.entries {
            let entry_persona = normalize_persona(&entry.persona);
            let key_persona = normalize_persona(key);
            let matches = entry
                .route
                .session_identity()
                .is_some_and(|(_, id)| id == intent)
                || bare.as_ref().is_some_and(|name| {
                    key_persona.as_ref() == Some(name) || entry_persona.as_ref() == Some(name)
                })
                || canonical.as_ref().is_some_and(|(_, name)| {
                    key_persona.as_ref() == Some(name) || entry_persona.as_ref() == Some(name)
                });
            if matches {
                found.push((entry, entry_persona));
            }
        }
        if found.len() > 1 {
            return Err("ambiguous registration; re-register and resubmit".into());
        }
        if canonical
            .as_ref()
            .is_some_and(|(vendor, _)| vendor != adapter.vendor_id())
        {
            return Ok(None);
        }
        if !found
            .iter()
            .any(|(entry, _)| entry.route.canonical_vendor() == adapter.vendor_id())
        {
            return Ok(None);
        }
        let (entry, persona) = &found[0];
        let persona = persona
            .as_deref()
            .ok_or("invalid registration; re-register and resubmit")?;
        let native_id = adapter
            .registration_native_id(&entry.route)
            .ok_or("unsupported registration; re-register and resubmit")?;
        #[cfg(test)]
        AFTER_INTENT_SELECTION.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        let acquired = Self::acquire(source, native_id, persona, policy)?;
        if !entry.same_registration_identity(&acquired.entry)
            || entry.persona != acquired.entry.persona
        {
            return Err("registration changed; re-register and resubmit".into());
        }
        Ok(Some(acquired))
    }
    pub(crate) fn acquire(
        source: PathBuf,
        native_id: &str,
        persona: &str,
        policy: &crate::vendor_adapter::HostedPastePolicy,
    ) -> Result<Self, RegistrationError> {
        let native_id = native_id.trim();
        let persona =
            normalize_persona(persona).ok_or("invalid target; re-register and resubmit")?;
        let entry = Self::select(&source, native_id, &persona, policy)?;
        Ok(Self {
            source,
            entry,
            persona,
            native_id: native_id.to_string(),
        })
    }
    fn select(
        source: &Path,
        native_id: &str,
        persona: &str,
        policy: &crate::vendor_adapter::HostedPastePolicy,
    ) -> Result<RouteEntry, RegistrationError> {
        let adapter = policy.adapter();
        let table = strict_table(source).map_err(RegistrationError::Source)?;
        // Find every full-intent collision before checking vendor or invoking policy.
        let mut candidates = table.entries.into_iter().filter(|(key, entry)| {
            normalize_persona(key).as_deref() == Some(persona)
                || normalize_persona(&entry.persona).as_deref() == Some(persona)
                || entry
                    .route
                    .session_identity()
                    .is_some_and(|(_, id)| id.trim() == native_id)
        });
        let (key, entry) = candidates
            .next()
            .ok_or("registration missing; re-register and resubmit")?;
        if candidates.next().is_some() {
            return Err("ambiguous registration; re-register and resubmit".into());
        }
        let valid = entry.route.canonical_vendor() == adapter.vendor_id()
            && entry.explicit
            && normalize_persona(&key).as_deref() == Some(persona)
            && normalize_persona(&entry.persona).as_deref() == Some(persona)
            && entry
                .hydration_generation
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
            && adapter.validate_registration(&entry, persona, native_id)
            && route_entry_proven_live(&entry, None, None);
        if !valid {
            return Err(
                "registration is incomplete or no longer current; re-register and resubmit".into(),
            );
        }
        Ok(entry)
    }
    pub(crate) fn revalidate(
        &self,
        policy: &crate::vendor_adapter::HostedPastePolicy,
    ) -> Result<&RouteEntry, RegistrationError> {
        let current = Self::select(&self.source, &self.native_id, &self.persona, policy)?;
        if !self.entry.same_registration_identity(&current) || self.entry.persona != current.persona
        {
            return Err("registration changed; re-register and resubmit".into());
        }
        Ok(&self.entry)
    }
    pub(crate) fn persona(&self) -> &str {
        &self.persona
    }
    pub(crate) fn native_session(&self) -> &str {
        &self.native_id
    }
}

#[cfg(test)]
thread_local! {
    static AFTER_INTENT_SELECTION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::VendorRoute;
    use crate::identity::proc_start_time_ticks;
    use crate::vendor_adapter::ClaudeRegistrationBinding;
    use fl_core::at::At;

    fn fixture() -> (tempfile::TempDir, PathBuf, RouteEntry) {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("routes.json");
        let pid = std::process::id();
        let entry = RouteEntry {
            persona: "ada".into(),
            route: VendorRoute::Claude {
                pid,
                session_id: "dce797a0-d963-4c1b-a06b-7fa6bffda7fa".into(),
                socket_path: dir.path().join("peer.sock"),
                key_path: None,
            },
            updated_at: At::from_unix_millis(1),
            identity_at: None,
            explicit: true,
            hosted_id: Some("claude:ada".into()),
            hydration_generation: Some("generation".into()),
            pid_start_time: proc_start_time_ticks(pid),
        };
        (dir, source, entry)
    }
    fn write_entry(source: &Path, entry: &RouteEntry) {
        let mut table = RouteTable::default();
        table.entries.insert("ada".into(), entry.clone());
        std::fs::write(source, serde_json::to_vec(&table).unwrap()).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn strict_primary_pins_identity_ignores_legacy_and_allows_restamps() {
        let (dir, source, mut entry) = fixture();
        let legacy = dir.path().join("legacy.json");
        write_entry(&legacy, &entry);
        write_entry(&source, &entry);
        let binding = ClaudeRegistrationBinding::from_intent(source.clone(), "ada")
            .unwrap()
            .unwrap();
        assert_eq!(
            binding.native_session(),
            "dce797a0-d963-4c1b-a06b-7fa6bffda7fa"
        );
        entry.updated_at = At::from_unix_millis(2);
        write_entry(&source, &entry);
        assert!(binding.revalidate().is_ok());
        entry.hydration_generation = Some("replacement".into());
        write_entry(&source, &entry);
        assert!(binding.revalidate().is_err());
        std::fs::remove_file(&source).unwrap();
        assert!(binding.revalidate().is_err());
        std::fs::write(&source, "{broken").unwrap();
        assert!(binding.revalidate().is_err());
        std::fs::remove_file(&source).unwrap();
        std::fs::create_dir(&source).unwrap();
        assert!(binding.revalidate().is_err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn intent_acquisition_refuses_replaced_registration_but_accepts_restamp() {
        let (_dir, source, original) = fixture();
        write_entry(&source, &original);
        let mut replacement = original.clone();
        replacement.hydration_generation = Some("replacement".into());
        let hook_source = source.clone();
        AFTER_INTENT_SELECTION.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || write_entry(&hook_source, &replacement)));
        });
        assert!(matches!(
            ClaudeRegistrationBinding::from_intent(source.clone(), "ada"),
            Err(RegistrationError::Refused(_))
        ));

        write_entry(&source, &original);
        let mut restamped = original;
        restamped.updated_at = At::from_unix_millis(2);
        restamped.identity_at = Some(At::from_unix_millis(2));
        let hook_source = source.clone();
        AFTER_INTENT_SELECTION.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || write_entry(&hook_source, &restamped)));
        });
        let binding = ClaudeRegistrationBinding::from_intent(source, "ada")
            .unwrap()
            .unwrap();
        assert!(binding.revalidate().is_ok());

        let (_dir, source, original) = fixture();
        write_entry(&source, &original);
        let hook_source = source.clone();
        AFTER_INTENT_SELECTION.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                std::fs::remove_file(hook_source).unwrap();
            }));
        });
        assert!(matches!(
            ClaudeRegistrationBinding::from_intent(source.clone(), "ada"),
            Err(RegistrationError::Source(_))
        ));

        write_entry(&source, &original);
        let hook_source = source.clone();
        AFTER_INTENT_SELECTION.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                std::fs::write(hook_source, r#"{"entries":{}}"#).unwrap();
            }));
        });
        assert!(matches!(
            ClaudeRegistrationBinding::from_intent(source.clone(), "ada"),
            Err(RegistrationError::Refused(_))
        ));

        write_entry(&source, &original);
        let encoded = serde_json::to_string(&original).unwrap();
        let hook_source = source.clone();
        AFTER_INTENT_SELECTION.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                std::fs::write(
                    hook_source,
                    format!(r#"{{"entries":{{"ada":{encoded},"ada":{encoded}}}}}"#),
                )
                .unwrap();
            }));
        });
        assert!(matches!(
            ClaudeRegistrationBinding::from_intent(source, "ada"),
            Err(RegistrationError::Source(_))
        ));
    }

    #[test]
    fn strict_primary_rejects_duplicate_keys_and_normalized_aliases() {
        let (_dir, source, entry) = fixture();
        let encoded = serde_json::to_string(&entry).unwrap();
        std::fs::write(
            &source,
            format!(r#"{{"entries":{{"ada":{encoded},"ada":{encoded}}}}}"#),
        )
        .unwrap();
        assert!(strict_table(&source).is_err());
        std::fs::write(
            &source,
            format!(r#"{{"entries":{{"ada":{encoded},"ADA":{encoded}}}}}"#),
        )
        .unwrap();
        assert!(strict_table(&source).is_err());
        std::fs::write(
            &source,
            format!(r#"{{"entries":{{"ada":{encoded},"bea":{encoded}}}}}"#),
        )
        .unwrap();
        assert!(strict_table(&source).is_err());
        std::fs::write(&source, r#"{"entries":{},"entries":{}}"#).unwrap();
        assert!(strict_table(&source).is_err());
    }
    #[test]
    fn unsupported_adapter_and_wrong_vendor_cannot_acquire_registration() {
        use crate::vendor_adapter::{CodexCliAdapter, VendorCliAdapter};
        let (_dir, source, entry) = fixture();
        write_entry(&source, &entry);
        let native = "dce797a0-d963-4c1b-a06b-7fa6bffda7fa";
        assert!(
            CodexCliAdapter
                .registration_native_id(&entry.route)
                .is_none()
        );
        assert!(!CodexCliAdapter.validate_registration(&entry, "ada", native));
        assert!(
            ClaudeRegistrationBinding::from_intent(source.clone(), "codex:ada")
                .unwrap()
                .is_none()
        );
        let mut wrong = entry;
        wrong.route = VendorRoute::Codex {
            thread_id: native.into(),
            cwd: None,
        };
        write_entry(&source, &wrong);
        assert!(ClaudeRegistrationBinding::acquire(source, native, "ada").is_err());
    }
    #[test]
    fn cross_vendor_native_collision_is_refused_before_vendor_filter() {
        let (_dir, source, entry) = fixture();
        let mut other = entry.clone();
        other.persona = "bea".into();
        other.hosted_id = Some("codex:bea".into());
        other.route = VendorRoute::Codex {
            thread_id: "dce797a0-d963-4c1b-a06b-7fa6bffda7fa".into(),
            cwd: None,
        };
        let mut table = RouteTable::default();
        table.entries.insert("ada".into(), entry);
        table.entries.insert("bea".into(), other);
        std::fs::write(&source, serde_json::to_vec(&table).unwrap()).unwrap();
        assert!(
            ClaudeRegistrationBinding::from_intent(
                source.clone(),
                "dce797a0-d963-4c1b-a06b-7fa6bffda7fa"
            )
            .is_err()
        );
        assert!(
            ClaudeRegistrationBinding::acquire(
                source,
                "dce797a0-d963-4c1b-a06b-7fa6bffda7fa",
                "ada"
            )
            .is_err()
        );
    }
    #[test]
    fn duplicate_identity_and_wrong_persona_are_refused_before_selection() {
        let (_dir, source, entry) = fixture();
        let mut table = RouteTable::default();
        let mut other = entry.clone();
        other.persona = "bea".into();
        other.hosted_id = Some("claude:bea".into());
        table.entries.insert("ada".into(), entry.clone());
        table.entries.insert("bea".into(), other);
        std::fs::write(&source, serde_json::to_vec(&table).unwrap()).unwrap();
        assert!(ClaudeRegistrationBinding::from_intent(source.clone(), "ada").is_err());
        let mut invalid = entry;
        invalid.hosted_id = Some("claude:bea".into());
        write_entry(&source, &invalid);
        assert!(ClaudeRegistrationBinding::from_intent(source, "ada").is_err());
    }
}
