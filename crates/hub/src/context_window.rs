//! Which context windows a session may be given (Local Record #134, Phase 2).
//!
//! Only Grok has a window the hub can choose (its offered sizes were verified with a throwaway
//! session). Claude's selection is NOT verified (every Claude 5.5 model reports 1M whatever the
//! `[1m]` tag says), so Claude is refused like the other vendors until it is. The hub validates a
//! choice here and records it; it never edits a vendor file, and applying the choice to a launch
//! is not done here. The window is its own value, never part of the model string.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A validated window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedWindow {
    pub tokens: u64,
    /// The model the window was checked for (trimmed); `None` when none was named.
    pub launch_model: Option<String>,
}

fn not_selectable(vendor: &str) -> String {
    if vendor == "claude" {
        "claude's context window is not selectable yet".to_string()
    } else {
        format!("{vendor}'s context window is not selectable")
    }
}

fn list(sizes: &[u64]) -> String {
    sizes
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a vendor's window must satisfy whatever the model: Grok, and a positive size (the
/// model's list decides at use time). Used for the hub-wide defaults, which name a vendor and
/// no model.
pub(crate) fn validate_default(vendor: &str, tokens: u64) -> Result<(), String> {
    match vendor {
        "grok" if tokens > 0 => Ok(()),
        "grok" => Err("grok's context window must be a positive number of tokens".to_string()),
        other => Err(not_selectable(other)),
    }
}

fn grok_window(model: Option<&str>, tokens: u64, home: Option<&Path>) -> Result<u64, String> {
    let Some(model) = model else {
        return Err(
            "the offered sizes for grok's default model are not available: name a model"
                .to_string(),
        );
    };
    let unavailable = || format!("the offered sizes for {model} are not available");
    let grok_dir = grok_dir(home).ok_or_else(unavailable)?;
    let (_, offered) =
        crate::discovery::grok_catalog_windows_in(&grok_dir, model).ok_or_else(unavailable)?;
    if offered.is_empty() {
        return Err(unavailable());
    }
    if offered.contains(&tokens) {
        Ok(tokens)
    } else {
        Err(format!(
            "{model} does not offer a window of {tokens} tokens; it offers {}",
            list(&offered)
        ))
    }
}

/// `<home>/.grok`, with `home` the override, else `$HOME`.
fn grok_dir(home: Option<&Path>) -> Option<PathBuf> {
    home.map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .map(|h| h.join(".grok"))
}

/// Accept `tokens` as the context window of a `vendor` session running `model`, or refuse with
/// a message that names the sizes on offer when they are known.
///
/// * Grok: `tokens` must be in the model's `context_windows` in `<home>/.grok/models_cache.json`
///   (`home` is the hub's `home_override`, else `$HOME`); a cache that cannot be read refuses.
/// * Any other vendor, Claude included (not verified yet): refused.
pub(crate) fn validate_context_window(
    vendor: &str,
    model: Option<&str>,
    tokens: u64,
    home: Option<&Path>,
) -> Result<ValidatedWindow, String> {
    if tokens == 0 {
        return Err("context_window must be a positive number of tokens".to_string());
    }
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    match vendor {
        "grok" => grok_window(model, tokens, home).map(|tokens| ValidatedWindow {
            tokens,
            launch_model: model.map(str::to_string),
        }),
        other => Err(not_selectable(other)),
    }
}

/// The hub-wide default for `vendor`, when it is valid for `model`; `None` otherwise. A stale
/// default (a size the model no longer offers, an unreadable Grok cache) is ignored, never an
/// error: a spin-up must not fail because of it.
pub(crate) fn applicable_default(
    defaults: &BTreeMap<String, u64>,
    vendor: &str,
    model: Option<&str>,
    home: Option<&Path>,
) -> Option<u64> {
    let tokens = *defaults.get(vendor)?;
    match validate_context_window(vendor, model, tokens, home) {
        Ok(valid) => Some(valid.tokens),
        Err(why) => {
            if std::env::var_os("FL_HUB_DEBUG").is_some() {
                eprintln!("fl-hub: ignoring the {vendor} context window default {tokens}: {why}");
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn grok_home(cache: Option<serde_json::Value>) -> TempDir {
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(".grok")).unwrap();
        if let Some(cache) = cache {
            std::fs::write(
                home.path().join(".grok/models_cache.json"),
                cache.to_string(),
            )
            .unwrap();
        }
        home
    }

    fn cache() -> serde_json::Value {
        json!({"models": {
            "grok-x": {"info": {"context_window": 256_000, "context_windows": [500_000, 256_000]}},
            "grok-none": {"info": {"context_window": 256_000}},
        }})
    }

    fn check(
        vendor: &str,
        model: Option<&str>,
        tokens: u64,
        home: &TempDir,
    ) -> Result<ValidatedWindow, String> {
        validate_context_window(vendor, model, tokens, Some(home.path()))
    }

    #[test]
    fn grok_accepts_only_the_sizes_the_model_offers() {
        let home = grok_home(Some(cache()));
        for tokens in [256_000, 500_000] {
            assert_eq!(
                check("grok", Some("grok-x"), tokens, &home),
                Ok(ValidatedWindow {
                    tokens,
                    launch_model: Some("grok-x".into())
                })
            );
        }
        let err = check("grok", Some("grok-x"), 300_000, &home).unwrap_err();
        assert!(err.contains("256000, 500000"), "{err}");
        assert!(err.contains("grok-x"), "{err}");
        assert!(check("grok", Some("grok-x"), 0, &home).is_err());
    }

    #[test]
    fn grok_refuses_when_the_offered_sizes_cannot_be_read() {
        let missing = grok_home(None);
        let unreadable = grok_home(None);
        std::fs::write(unreadable.path().join(".grok/models_cache.json"), "{ nope").unwrap();
        let unlisted = grok_home(Some(cache()));
        let no_list = grok_home(Some(cache()));
        for (home, model) in [
            (&missing, "grok-x"),
            (&unreadable, "grok-x"),
            (&unlisted, "grok-unlisted"),
            (&no_list, "grok-none"),
        ] {
            assert_eq!(
                check("grok", Some(model), 256_000, home),
                Err(format!("the offered sizes for {model} are not available"))
            );
        }
        // Without a model there is no list to check against.
        let err = check("grok", None, 256_000, &no_list).unwrap_err();
        assert!(err.contains("not available"), "{err}");
    }

    #[test]
    fn every_other_vendor_is_not_selectable() {
        let home = TempDir::new().unwrap();
        assert_eq!(
            check("claude", Some("opus[1m]"), 1_000_000, &home),
            Err("claude's context window is not selectable yet".to_string())
        );
        for vendor in ["codex", "muse", "antigravity", "unknown", ""] {
            assert_eq!(
                check(vendor, Some("m"), 200_000, &home),
                Err(format!("{vendor}'s context window is not selectable"))
            );
        }
    }

    #[test]
    fn defaults_are_checked_by_vendor_without_a_model() {
        assert_eq!(
            validate_default("claude", 200_000),
            Err("claude's context window is not selectable yet".to_string())
        );
        assert!(validate_default("grok", 256_000).is_ok());
        assert!(validate_default("grok", 0).is_err());
        assert!(
            validate_default("codex", 200_000)
                .unwrap_err()
                .contains("not selectable")
        );
    }

    #[test]
    fn a_stale_default_is_ignored_not_an_error() {
        let home = grok_home(Some(cache()));
        let defaults =
            BTreeMap::from([("grok".to_string(), 500_000), ("claude".into(), 1_000_000)]);
        let get = |vendor: &str, model: Option<&str>| {
            applicable_default(&defaults, vendor, model, Some(home.path()))
        };
        assert_eq!(get("grok", Some("grok-x")), Some(500_000));
        // The model does not offer it, or its list is unavailable.
        assert_eq!(get("grok", Some("grok-none")), None);
        assert_eq!(get("grok", None), None);
        assert_eq!(get("claude", Some("opus[1m]")), None);
        assert_eq!(get("codex", Some("m")), None);
        let stale = BTreeMap::from([("grok".to_string(), 123)]);
        assert_eq!(
            applicable_default(&stale, "grok", Some("grok-x"), Some(home.path())),
            None
        );
    }
}
