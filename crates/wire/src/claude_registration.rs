//! Strict, pinned registration evidence for hosted Claude operator paste.
//!
//! This deliberately does not use `RouteTable::load`: permissive legacy recovery and
//! passive discovery cannot establish the provenance needed for terminal mutation.
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::identity::{
    RouteEntry, RouteTable, VendorRoute, normalize_persona, parse_hosted_identity,
    route_entry_proven_live,
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
        .map_err(|_| "Claude registration source unavailable; re-register and resubmit")?;
    let UniqueJson(value) = serde_json::from_slice(&raw)
        .map_err(|_| "invalid or ambiguous Claude registration source; re-register and resubmit")?;
    let entries = value
        .get("entries")
        .and_then(Value::as_object)
        .ok_or("invalid Claude registration source; re-register and resubmit")?;
    let mut aliases = BTreeSet::new();
    let mut personas = BTreeSet::new();
    for (key, entry) in entries {
        let alias = normalize_persona(key)
            .ok_or("invalid Claude registration source; re-register and resubmit")?;
        if !aliases.insert(alias) {
            return Err("ambiguous Claude registration source; re-register and resubmit");
        }
        // Modern typed entries only; never reconstruct explicit identity from legacy defaults.
        if !entry.is_object() || entry.get("route").is_none() {
            return Err("incomplete Claude registration source; re-register and resubmit");
        }
        let persona = entry
            .get("persona")
            .and_then(Value::as_str)
            .and_then(normalize_persona)
            .ok_or("invalid Claude registration source; re-register and resubmit")?;
        if !personas.insert(persona) {
            return Err("ambiguous Claude registration source; re-register and resubmit");
        }
    }
    serde_json::from_value(value)
        .map_err(|_| "invalid Claude registration source; re-register and resubmit")
}

/// A source failure is distinct from a source-backed identity refusal, so an
/// independently classified non-Claude request can only bypass the former.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeRegistrationError {
    Source(&'static str),
    Refused(&'static str),
}
impl From<&'static str> for ClaudeRegistrationError {
    fn from(message: &'static str) -> Self {
        Self::Refused(message)
    }
}
impl fmt::Display for ClaudeRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(message) | Self::Refused(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for ClaudeRegistrationError {}

/// A source-backed registration snapshot. Fields are private; constructors reread evidence.
/// It is intentionally not serializable: a restored queue item has only untrusted intent.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeRegistrationBinding {
    source: PathBuf,
    entry: RouteEntry,
    persona: String,
    native_id: String,
}
impl fmt::Debug for ClaudeRegistrationBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClaudeRegistrationBinding(<redacted>)")
    }
}

impl ClaudeRegistrationBinding {
    /// Validate the selected primary without granting terminal authority. Peer delivery
    /// still needs its own route authorization; an unclassified alias may not discover
    /// routes when this source is unavailable or ambiguous.
    pub fn validate_source(source: &Path) -> Result<(), ClaudeRegistrationError> {
        strict_table(source)
            .map(|_| ())
            .map_err(ClaudeRegistrationError::Source)
    }

    /// Inspect raw admission intent against all entries in the primary source before any
    /// discovery ordering or deduplication can pick a route. A non-Claude result is not
    /// authority to deliver; it leaves existing non-Claude handling in charge.
    pub fn from_intent(
        source: PathBuf,
        intent: &str,
    ) -> Result<Option<Self>, ClaudeRegistrationError> {
        let table = strict_table(&source).map_err(ClaudeRegistrationError::Source)?;
        let intent = intent.trim();
        let canonical = parse_hosted_identity(intent).ok();
        let bare = normalize_persona(intent);
        let mut found = Vec::new();
        for (key, entry) in &table.entries {
            let entry_persona = normalize_persona(&entry.persona);
            let key_persona = normalize_persona(key);
            let vendor = entry.route.canonical_vendor();
            let matches = entry
                .route
                .session_identity()
                .is_some_and(|(_, id)| id == intent)
                || bare.as_ref().is_some_and(|name| {
                    key_persona.as_ref() == Some(name) || entry_persona.as_ref() == Some(name)
                })
                || canonical.as_ref().is_some_and(|(v, name)| {
                    v == vendor
                        && (key_persona.as_ref() == Some(name)
                            || entry_persona.as_ref() == Some(name))
                });
            if matches {
                found.push((entry, entry_persona));
            }
        }
        if !found
            .iter()
            .any(|(entry, _)| matches!(entry.route, VendorRoute::Claude { .. }))
        {
            return Ok(None);
        }
        if found.len() != 1 {
            return Err("ambiguous Claude registration; re-register and resubmit".into());
        }
        let (entry, persona) = &found[0];
        let VendorRoute::Claude { session_id, .. } = &entry.route else {
            unreachable!()
        };
        let persona = persona
            .as_deref()
            .ok_or("invalid Claude registration; re-register and resubmit")?;
        Self::acquire(source, session_id, persona).map(Some)
    }

    /// Pin an explicitly selected primary source and an intended native session/persona.
    /// Raw inputs specify intent; this constructor derives authority from the source itself.
    pub fn acquire(
        source: PathBuf,
        native_id: &str,
        persona: &str,
    ) -> Result<Self, ClaudeRegistrationError> {
        let native_id = native_id.trim();
        let persona =
            normalize_persona(persona).ok_or("invalid Claude target; re-register and resubmit")?;
        if uuid::Uuid::parse_str(native_id).is_err() {
            return Err("invalid Claude native session; re-register and resubmit".into());
        }
        let entry = Self::select(&source, native_id, &persona)?;
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
    ) -> Result<RouteEntry, ClaudeRegistrationError> {
        let table = strict_table(source).map_err(ClaudeRegistrationError::Source)?;
        let mut candidates = table.entries.into_iter().filter(|(key, entry)| {
            let route_id = match &entry.route {
                VendorRoute::Claude { session_id, .. } => session_id.trim(),
                _ => return false,
            };
            normalize_persona(key).as_deref() == Some(persona)
                || normalize_persona(&entry.persona).as_deref() == Some(persona)
                || route_id == native_id
        });
        let (key, entry) = candidates
            .next()
            .ok_or("Claude registration missing; re-register and resubmit")?;
        if candidates.next().is_some() {
            return Err("ambiguous Claude registration; re-register and resubmit".into());
        }
        let hosted = entry
            .hosted_id
            .as_deref()
            .and_then(|id| parse_hosted_identity(id).ok());
        let valid = entry.explicit
            && normalize_persona(&key).as_deref() == Some(persona)
            && normalize_persona(&entry.persona).as_deref() == Some(persona)
            && hosted
                .as_ref()
                .is_some_and(|(vendor, name)| vendor == "claude" && name == persona)
            && entry
                .hydration_generation
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
            && matches!(&entry.route, VendorRoute::Claude { session_id, .. } if session_id.trim() == native_id && !session_id.trim().is_empty())
            && route_entry_proven_live(&entry, None, None);
        if !valid {
            return Err(
                "Claude registration is incomplete or no longer current; re-register and resubmit"
                    .into(),
            );
        }
        Ok(entry)
    }

    pub(crate) fn revalidate(&self) -> Result<&RouteEntry, ClaudeRegistrationError> {
        let current = Self::select(&self.source, &self.native_id, &self.persona)?;
        if !self.entry.same_registration_identity(&current) || self.entry.persona != current.persona
        {
            return Err("Claude registration changed; re-register and resubmit".into());
        }
        Ok(&self.entry)
    }

    pub(crate) fn persona(&self) -> &str {
        &self.persona
    }
    pub fn persona_name(&self) -> &str {
        &self.persona
    }
    pub fn native_session(&self) -> &str {
        &self.native_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::proc_start_time_ticks;
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
