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

/// Check `tokens` against the sizes Grok's cache lists for `model`. The model id is matched
/// exactly (case-sensitive, no alias), as Phase 1's display does. The sizes on offer are the
/// model's `context_windows` list together with the model's own `context_window` when present:
/// a model with only `context_window` accepts exactly that size (choosing it changes nothing but
/// is valid). A model that is absent from the cache and a cache that cannot be read are
/// different refusals.
fn grok_window(model: Option<&str>, tokens: u64, home: Option<&Path>) -> Result<u64, String> {
    use crate::discovery::{GrokCatalogLookup, grok_catalog_lookup_in};
    let Some(model) = model else {
        return Err(
            "the offered sizes for grok's default model are not available: name a model"
                .to_string(),
        );
    };
    let unavailable = || format!("the offered sizes for {model} are not available");
    let grok_dir = grok_dir(home).ok_or_else(unavailable)?;
    let (window, mut offered) = match grok_catalog_lookup_in(&grok_dir, model) {
        GrokCatalogLookup::Unreadable => return Err(unavailable()),
        GrokCatalogLookup::Absent => return Err(format!("{model} is not in Grok's model list")),
        GrokCatalogLookup::Found { window, offered } => (window, offered),
    };
    offered.extend(window);
    offered.sort_unstable();
    offered.dedup();
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
/// * Grok: `tokens` must be one of the model's sizes in `<home>/.grok/models_cache.json` (its
///   `context_windows` plus its own `context_window`; `home` is the hub's `home_override`, else
///   `$HOME`). The model id is matched exactly (case-sensitive, no alias), like Phase 1's
///   `models.get(model_id)`. A model that is not in the cache and a cache that cannot be read
///   are refused with different messages.
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
/// default (a size the model no longer offers, an unreadable Grok cache) is ignored silently,
/// never an error and never logged: a spin-up must not fail because of it.
pub(crate) fn applicable_default(
    defaults: &BTreeMap<String, u64>,
    vendor: &str,
    model: Option<&str>,
    home: Option<&Path>,
) -> Option<u64> {
    let tokens = *defaults.get(vendor)?;
    validate_context_window(vendor, model, tokens, home)
        .ok()
        .map(|valid| valid.tokens)
}

/// Parse a bracketed Claude context-window tag (`[200k]` -> `200_000`, `[1m]` -> `1_000_000`)
/// validated via [`fl_wire::vendor_adapter::split_window_tag`].
#[allow(dead_code)]
pub(crate) fn parse_window_tag_tokens(tag: &str) -> Option<u64> {
    let (base, bracketed) = fl_wire::vendor_adapter::split_window_tag(tag.trim())?;
    if !base.is_empty() {
        return None;
    }
    let inner = bracketed.strip_prefix('[')?.strip_suffix(']')?;
    let (digits, mult) = if let Some(d) = inner.strip_suffix('k') {
        (d, 1_000_u64)
    } else {
        let d = inner.strip_suffix('m')?;
        (d, 1_000_000_u64)
    };
    let count = digits.parse::<u64>().ok().filter(|&n| n > 0)?;
    count.checked_mul(mult)
}

fn is_pre_5_claude_model_id(raw_model: &str) -> bool {
    let trimmed = raw_model.trim();
    let base = fl_wire::vendor_adapter::split_window_tag(trimmed).map_or(trimmed, |(b, _)| b);
    let base = base.split_once('[').map_or(base, |(b, _)| b);
    let lower = base.trim().to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("claude-") else {
        return false;
    };
    let Some(ver_seg) = rest
        .split('-')
        .find(|seg| seg.bytes().next().is_some_and(|b| b.is_ascii_digit()))
    else {
        return false;
    };
    matches!(ver_seg, "3" | "4") || ver_seg.starts_with("3.") || ver_seg.starts_with("4.")
}

/// Resolve the effective context window (in tokens) for a Claude session from existing catalog
/// and session metadata sources:
/// 1. Explicit positive `record_window` on the hosted session record.
/// 2. Explicit `[Nk]` / `[Nm]` window tag on `record_model`, then `transcript_model`.
/// 3. Pre-5.x Claude 3 / 4 model IDs on `record_model` (or `transcript_model` when `record_model`
///    is absent) -> `anthropic-api` default window (`200_000`).
/// 4. Otherwise (Claude 5.x models, CLI aliases `opus`/`sonnet`/`haiku`, or `None`) ->
///    [`crate::discovery::ASSUMED_LARGE_CONTEXT_WINDOW`] (`1_000_000`).
#[allow(dead_code)]
pub(crate) fn claude_session_context_window(
    record_window: Option<u64>,
    record_model: Option<&str>,
    transcript_model: Option<&str>,
) -> u64 {
    if let Some(w) = record_window.filter(|&w| w > 0) {
        return w;
    }

    let record_model = record_model.map(str::trim).filter(|m| !m.is_empty());
    let transcript_model = transcript_model.map(str::trim).filter(|m| !m.is_empty());

    for candidate in [record_model, transcript_model].into_iter().flatten() {
        if let Some((_, tag)) = fl_wire::vendor_adapter::split_window_tag(candidate)
            && let Some(tokens) = parse_window_tag_tokens(tag)
        {
            return tokens;
        }
    }

    if let Some(model) = record_model.or(transcript_model)
        && is_pre_5_claude_model_id(model)
    {
        return fl_core::lookup_vendor("anthropic-api")
            .map(|d| d.default_context_window)
            .filter(|&w| w > 0)
            .unwrap_or(200_000);
    }

    crate::discovery::ASSUMED_LARGE_CONTEXT_WINDOW
}

/// Model-relative active-token ceiling for `claude --resume`: `min(ceiling, 75% of window_tokens)`.
#[allow(dead_code)]
pub(crate) fn claude_resume_active_token_cap(window_tokens: u64, ceiling: u64) -> u64 {
    ceiling.min(window_tokens.saturating_mul(3) / 4)
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
        let no_models = grok_home(Some(json!({"other": 1})));
        let no_info = grok_home(Some(json!({"models": {"grok-x": {}}})));
        let no_sizes = grok_home(Some(json!({"models": {"grok-x": {"info": {}}}})));
        for home in [&missing, &unreadable, &no_models, &no_info, &no_sizes] {
            assert_eq!(
                check("grok", Some("grok-x"), 256_000, home),
                Err("the offered sizes for grok-x are not available".to_string())
            );
        }
        // Without a model there is no list to check against.
        let ok = grok_home(Some(cache()));
        let err = check("grok", None, 256_000, &ok).unwrap_err();
        assert!(err.contains("not available"), "{err}");
    }

    #[test]
    fn grok_names_a_model_that_is_not_in_the_cache_and_matches_it_exactly() {
        let home = grok_home(Some(cache()));
        for model in ["grok-unlisted", "GROK-X", "grok-x "] {
            let expect = if model.trim() == "grok-x" {
                Ok(256_000)
            } else {
                Err(format!("{} is not in Grok's model list", model.trim()))
            };
            assert_eq!(
                check("grok", Some(model), 256_000, &home).map(|v| v.tokens),
                expect,
                "{model:?}"
            );
        }
    }

    #[test]
    fn grok_accepts_a_models_own_default_size_when_it_has_no_list() {
        let home = grok_home(Some(cache()));
        // `grok-none` has only `context_window`: that size is valid, nothing else is.
        assert_eq!(
            check("grok", Some("grok-none"), 256_000, &home),
            Ok(ValidatedWindow {
                tokens: 256_000,
                launch_model: Some("grok-none".into())
            })
        );
        let err = check("grok", Some("grok-none"), 500_000, &home).unwrap_err();
        assert!(err.contains("it offers 256000"), "{err}");
        // With a list, the own size joins it without a duplicate.
        let own = grok_home(Some(
            json!({"models": {"m": {"info": {"context_window": 128_000, "context_windows": [500_000, 256_000]}}}}),
        ));
        assert!(check("grok", Some("m"), 128_000, &own).is_ok());
        let err = check("grok", Some("m"), 1, &own).unwrap_err();
        assert!(err.contains("128000, 256000, 500000"), "{err}");
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
        let own = BTreeMap::from([("grok".to_string(), 256_000)]);
        assert_eq!(
            applicable_default(&own, "grok", Some("grok-none"), Some(home.path())),
            Some(256_000)
        );
        assert_eq!(get("grok", None), None);
        assert_eq!(get("claude", Some("opus[1m]")), None);
        assert_eq!(get("codex", Some("m")), None);
        let stale = BTreeMap::from([("grok".to_string(), 123)]);
        assert_eq!(
            applicable_default(&stale, "grok", Some("grok-x"), Some(home.path())),
            None
        );
    }

    #[test]
    fn parse_window_tag_tokens_parses_bracketed_k_and_m_tags() {
        assert_eq!(parse_window_tag_tokens("[200k]"), Some(200_000));
        assert_eq!(parse_window_tag_tokens("[1m]"), Some(1_000_000));
        assert_eq!(parse_window_tag_tokens(" [128k] "), Some(128_000));
        for bad in ["", "[0k]", "[0m]", "[200K]", "[1M]", "200k", "opus[200k]"] {
            assert_eq!(parse_window_tag_tokens(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn claude_context_window_and_resume_cap_scale_by_model_and_explicit_overrides() {
        const CEILING: u64 = 750_000;

        // 1M Claude 5.x models, aliases, and None -> window 1,000,000 -> cap 750,000.
        for model in [
            Some("claude-opus-5-5"),
            Some("claude-sonnet-5-5"),
            Some("claude-fable-5-2"),
            Some("claude-haiku-5-5"),
            Some("claude-opus-5-4"),
            Some("opus"),
            Some("sonnet"),
            Some("haiku"),
            None,
        ] {
            let win = claude_session_context_window(None, model, None);
            assert_eq!(win, 1_000_000, "expected 1M window for {model:?}");
            let cap = claude_resume_active_token_cap(win, CEILING);
            assert_eq!(cap, 750_000, "expected 750k cap for {model:?}");
            assert!(
                388_396 <= cap,
                "388,396 tokens must pass on 1M model {model:?}"
            );
            assert!(
                895_176 > cap,
                "895,176 tokens must fail on 1M model {model:?}"
            );
        }

        // Pre-5.x Claude 3/4 models (via record_model or transcript_model fallback) -> 200k -> cap 150,000.
        for model in [
            "claude-opus-4-1",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "claude-opus-4-0",
            "claude-opus-4",
            "claude-3-7-sonnet-20250219",
            "claude-3.5-sonnet",
            "claude-sonnet-3-7",
        ] {
            let win_rec = claude_session_context_window(None, Some(model), Some("claude-opus-5-5"));
            let win_tx = claude_session_context_window(None, None, Some(model));
            assert_eq!(win_rec, 200_000, "expected 200k for record_model={model}");
            assert_eq!(
                win_tx, 200_000,
                "expected 200k for transcript_model={model}"
            );
            let cap = claude_resume_active_token_cap(win_rec, CEILING);
            assert_eq!(cap, 150_000, "expected 150k cap for {model}");
            assert!(
                388_396 > cap,
                "388,396 tokens must FAIL on 200k model {model}"
            );
            assert!(
                120_000 <= cap,
                "120,000 tokens must pass on 200k model {model}"
            );
        }

        // Window tags override model-family defaults; explicit record_window overrides all.
        let tagged_200k = claude_session_context_window(None, Some("claude-opus-5-5[200k]"), None);
        assert_eq!(tagged_200k, 200_000);
        let tagged_cap = claude_resume_active_token_cap(tagged_200k, CEILING);
        assert_eq!(tagged_cap, 150_000);
        assert!(388_396 > tagged_cap);
        assert!(120_000 <= tagged_cap);

        let tx_tagged_200k =
            claude_session_context_window(None, Some("claude-opus-5-5"), Some("opus[200k]"));
        assert_eq!(tx_tagged_200k, 200_000);

        let tagged_1m = claude_session_context_window(None, Some("claude-sonnet-4-5[1m]"), None);
        assert_eq!(tagged_1m, 1_000_000);
        assert_eq!(claude_resume_active_token_cap(tagged_1m, CEILING), 750_000);

        let explicit_rec =
            claude_session_context_window(Some(200_000), Some("claude-opus-5-5[1m]"), None);
        assert_eq!(explicit_rec, 200_000);
        assert_eq!(
            claude_resume_active_token_cap(explicit_rec, CEILING),
            150_000
        );

        // Zero record_window is ignored.
        assert_eq!(
            claude_session_context_window(Some(0), Some("claude-opus-5-5"), None),
            1_000_000
        );
    }
}
