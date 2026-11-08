//! Child environment. `env_clear`, then the host baseline and the explicit allow-list.
//!
//! The baseline is `PATH`, `HOME` or `USERPROFILE`, `LANG`, and `TERM`. Vendor credential
//! allow-lists are not consulted. No vault secret is copied.

use std::collections::BTreeMap;
use std::ffi::OsString;

use fl_host_proto::{HostError, SpawnSpec};

/// Names every pipe child may receive from the parent, when set.
pub fn baseline_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["PATH", "USERPROFILE", "LANG", "TERM"]
    }
    #[cfg(not(windows))]
    {
        &["PATH", "HOME", "LANG", "TERM"]
    }
}

/// Build the vendor environment from nothing. Overrides win over the parent value.
pub fn scrubbed_env(spec: &SpawnSpec) -> Result<Vec<(OsString, OsString)>, HostError> {
    spec.validate_fields()?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for name in baseline_names() {
        if let Some(value) = parent_value(name)? {
            out.insert((*name).to_owned(), value);
        }
    }
    for name in &spec.env_allow {
        if let Some(value) = spec.env.get(name) {
            out.insert(name.clone(), value.clone());
        } else if let Some(value) = parent_value(name)? {
            out.insert(name.clone(), value);
        }
    }
    Ok(out
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect())
}

fn parent_value(name: &str) -> Result<Option<String>, HostError> {
    match std::env::var(name) {
        Ok(value) => {
            if value.as_bytes().contains(&0) {
                return Err(HostError::Protocol("env value".into()));
            }
            Ok(Some(value))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Ok(None),
    }
}
