//! Logic behind the session control endpoints (`interrupt`, `model`, `bulk-action`) that does
//! not need the HTTP layer: input validation, the in-memory model override, and the interrupt
//! dispatch. The handlers live in `api.rs`.

use std::collections::BTreeMap;

use fl_wire::dispatcher::{plan_interrupt, run_interrupt};
use fl_wire::identity::VendorRoute;

use crate::discovery::DiscoveredSessionSummary;
use crate::lifecycle::LifecycleEnv;

/// Longest model id the model endpoint accepts.
pub const MAX_MODEL_CHARS: usize = 128;
/// `method` reported when the interrupt was planned but real dispatch is switched off.
pub const METHOD_NOOP_TEST: &str = "noop-test";
/// `method` reported when the model was recorded and the vendor CLI was sent `/model <id>`.
pub const METHOD_OPERATOR_PROMPT: &str = "operator-prompt";
/// `method` reported when the model was recorded but nothing reached a CLI.
pub const METHOD_OVERRIDE_ONLY: &str = "override-only";

/// A model the operator chose for a session through `POST /api/v1/sessions/:id/model`.
/// It lives in memory (see `HubState::set_session_model_override`) and is lost on hub restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOverride {
    pub model: String,
    pub thinking_level: Option<String>,
}

/// A model id the endpoint accepts: trimmed, non-empty, one line, at most
/// [`MAX_MODEL_CHARS`] characters and no control character, because it is later
/// pasted into a CLI as `/model <id>`.
pub fn validate_model(raw: &str) -> Result<String, String> {
    let model = raw.trim();
    if model.is_empty() {
        return Err("model must not be empty".to_string());
    }
    if model.chars().count() > MAX_MODEL_CHARS {
        return Err(format!(
            "model must be at most {MAX_MODEL_CHARS} characters"
        ));
    }
    if model.chars().any(char::is_control) {
        return Err("model must be a single line with no control characters".to_string());
    }
    Ok(model.to_string())
}

/// `low`, `medium` or `high` in any case, normalized to lowercase.
pub fn validate_thinking_level(raw: &str) -> Result<String, String> {
    let level = raw.trim().to_ascii_lowercase();
    match level.as_str() {
        "low" | "medium" | "high" => Ok(level),
        _ => Err("thinking_level must be one of low, medium, high".to_string()),
    }
}

/// Put each session's override over what discovery found. A session with no override is
/// untouched. An override without a thinking level keeps the discovered one.
pub fn apply_model_overrides(
    sessions: &mut [DiscoveredSessionSummary],
    overrides: &BTreeMap<String, ModelOverride>,
) {
    if overrides.is_empty() {
        return;
    }
    for session in sessions {
        if let Some(over) = overrides.get(&session.id) {
            session.model = Some(over.model.clone());
            if over.thinking_level.is_some() {
                session.thinking_level = over.thinking_level.clone();
            }
        }
    }
}

/// Vendor name of a route, as discovery names the adapter.
pub fn route_vendor(route: &VendorRoute) -> &'static str {
    match route {
        VendorRoute::Claude { .. } => "claude",
        VendorRoute::Agy { .. } => "antigravity",
        VendorRoute::Codex { .. } => "codex",
        VendorRoute::Muse { .. } => "muse",
        VendorRoute::Grok { .. } => "grok",
        VendorRoute::Webhook { .. } => "webhook",
    }
}

/// Interrupt the turn running on `route` and return the `method` label.
///
/// The plan is built first, so a route that cannot be interrupted is an error in every mode.
/// When `env.spawn_runtime` is false (`cfg(test)` or `FL_TEST_SPINUP_NO_SPAWN=1`) nothing is
/// sent to tmux or a language server and the method is [`METHOD_NOOP_TEST`].
pub fn interrupt_route(
    env: &LifecycleEnv<'_>,
    route: &VendorRoute,
    tmux_fallback: Option<&str>,
) -> Result<&'static str, String> {
    let plan = plan_interrupt(route, tmux_fallback)?;
    if !env.spawn_runtime {
        return Ok(METHOD_NOOP_TEST);
    }
    run_interrupt(&plan)?;
    Ok(plan.method())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_validation_rejects_blank_long_multiline_and_control_input() {
        assert_eq!(validate_model("  gpt-6-sol ").unwrap(), "gpt-6-sol");
        for bad in ["", "   ", "a\nb", "a\rb", "a\tb", "a\u{7}b", "a\u{1b}[31m"] {
            assert!(validate_model(bad).is_err(), "{bad:?}");
        }
        assert!(validate_model(&"m".repeat(MAX_MODEL_CHARS)).is_ok());
        assert!(validate_model(&"m".repeat(MAX_MODEL_CHARS + 1)).is_err());
    }

    #[test]
    fn thinking_level_is_one_of_three_and_lowercased() {
        assert_eq!(validate_thinking_level(" HIGH ").unwrap(), "high");
        assert_eq!(validate_thinking_level("Low").unwrap(), "low");
        for bad in ["", "max", "extreme", "med", "high\n"] {
            // A trailing newline is trimmed, so only the first four are errors.
            let r = validate_thinking_level(bad);
            assert_eq!(r.is_err(), bad != "high\n", "{bad:?}");
        }
    }
}
