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

/// A live (model, thinking level) pair as discovery reported it for a session.
type LivePair = (Option<String>, Option<String>);

/// A model the operator chose for a session through `POST /api/v1/sessions/:id/model`.
/// It lives in memory (see `HubState::set_session_model_override`) and is lost on hub restart.
///
/// `model` and `thinking_level` are the operator's choice. The rest is runtime state that
/// [`apply_model_overrides`] keeps, so a model the vendor reports later can supersede the
/// choice: `baseline` is the live pair seen the first time the override was applied, and
/// `confirmed` is set once the vendor was seen reporting the choice. [`ModelOverride::new`]
/// starts both empty, so a fresh `set` always starts over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOverride {
    pub model: String,
    pub thinking_level: Option<String>,
    baseline: Option<LivePair>,
    confirmed: bool,
}

impl ModelOverride {
    pub fn new(model: String, thinking_level: Option<String>) -> Self {
        Self {
            model,
            thinking_level,
            baseline: None,
            confirmed: false,
        }
    }

    /// Whether the live pair is the operator's choice: the model is equal ignoring ASCII case,
    /// and so is the effort unless the choice has none (any live effort satisfies it) or the
    /// live effort is `None`. A live effort of `None` is "unknown", not a mismatch: Muse and
    /// Claude without an effort signal never report one while the UI always sends one, so the
    /// override stays and its effort is overlaid.
    fn matches_live(&self, live_model: &str, live_effort: Option<&str>) -> bool {
        live_model.eq_ignore_ascii_case(&self.model)
            && match (&self.thinking_level, live_effort) {
                (None, _) | (_, None) => true,
                (Some(want), Some(have)) => want.eq_ignore_ascii_case(have),
            }
    }
}

fn pair_eq_ignore_case(a: &LivePair, b: &LivePair) -> bool {
    let opt = |x: &Option<String>, y: &Option<String>| match (x, y) {
        (None, None) => true,
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    };
    opt(&a.0, &b.0) && opt(&a.1, &b.1)
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

/// Put each session's override over what discovery found, and drop an override the vendor has
/// superseded. An override is keyed by a session id or by a lowercased persona (see
/// `HubState::set_session_model_override`): a session matches on its id first and its persona
/// second. A session with no override is untouched. An override without a thinking level keeps
/// the discovered one.
///
/// The live pair is the session's discovered model and effort before the overlay. A session whose
/// `model_is_default` is set reports no model: its model is the configured default, a guess, and
/// counts as no live model. For each
/// override, the first matching session that reports a model decides for every session the
/// override matches in this call:
/// - no live model anywhere: the override is left alone and overlaid;
/// - no baseline yet: the live pair becomes the baseline; the override stays and is overlaid;
/// - live equals the choice (the model equal ignoring case, and the effort equal ignoring case,
///   or the choice has no effort, or the live effort is unknown): the override is confirmed; it
///   stays and is overlaid, so the operator's effort shows even if the vendor reports none;
/// - live differs from the choice and from the baseline: the vendor reports something newer, so
///   the override is removed and the live values show;
/// - live equals the baseline and the override was confirmed: the vendor applied the choice and
///   has since gone back, so the override is removed;
/// - live equals the baseline and the override was never confirmed: the vendor has not applied
///   the choice yet, so the override stays and is overlaid.
///
/// When an id key is removed, the session's persona key goes with it, unless another session of
/// the call has no id key and relies on that persona key.
pub fn apply_model_overrides(
    sessions: &mut [DiscoveredSessionSummary],
    overrides: &mut BTreeMap<String, ModelOverride>,
) {
    if overrides.is_empty() {
        return;
    }
    // Match every session before any override is removed, so a removed id key does not make its
    // session fall through to a persona key.
    let keys: Vec<Option<String>> = sessions
        .iter()
        .map(|session| {
            if overrides.contains_key(&session.id) {
                Some(session.id.clone())
            } else {
                session
                    .persona
                    .as_deref()
                    .map(str::to_ascii_lowercase)
                    .filter(|persona| overrides.contains_key(persona))
            }
        })
        .collect();
    // A persona key is shadowed by the id key of every session that has both. When that id key
    // is dropped the persona key goes with it, unless another session in this call has no id
    // key of its own and so relies on the persona key. Decided before any removal, so the order
    // of the sessions does not matter.
    let shadowed: Vec<Option<String>> = sessions
        .iter()
        .zip(&keys)
        .map(|(session, key)| {
            let persona = session.persona.as_deref()?.to_ascii_lowercase();
            let id_matched = key.as_deref() == Some(session.id.as_str());
            let relied_on = keys.iter().zip(sessions.iter()).any(|(other_key, other)| {
                other.id != session.id
                    && other_key.as_deref() == Some(persona.as_str())
                    && other_key.as_deref() != Some(other.id.as_str())
            });
            (id_matched && persona != session.id && overrides.contains_key(&persona) && !relied_on)
                .then_some(persona)
        })
        .collect();
    // `Some(choice)` is an override still to overlay; `None` is one that was just removed.
    let mut decided: BTreeMap<String, Option<(String, Option<String>)>> = BTreeMap::new();
    for ((session, key), shadowed) in sessions.iter_mut().zip(keys).zip(shadowed) {
        let Some(key) = key else { continue };
        // A default-model guess is no report from the vendor: treat it as no live model.
        let live_model = session
            .model
            .as_deref()
            .filter(|m| !m.trim().is_empty() && !session.model_is_default);
        if !decided.contains_key(&key)
            && let Some(live_model) = live_model
            && let Some(over) = overrides.get_mut(&key)
        {
            let live: LivePair = (Some(live_model.to_string()), session.thinking_level.clone());
            let keep = match over.baseline.clone() {
                None => {
                    over.baseline = Some(live);
                    true
                }
                Some(_) if over.matches_live(live_model, session.thinking_level.as_deref()) => {
                    over.confirmed = true;
                    true
                }
                Some(baseline) => pair_eq_ignore_case(&live, &baseline) && !over.confirmed,
            };
            if keep {
                decided.insert(
                    key.clone(),
                    Some((over.model.clone(), over.thinking_level.clone())),
                );
            } else {
                overrides.remove(&key);
                if let Some(persona) = shadowed {
                    overrides.remove(&persona);
                }
                decided.insert(key.clone(), None);
            }
        }
        let choice = match decided.get(&key) {
            Some(choice) => choice.clone(),
            // No session of this override reported a model yet: overlay it untouched.
            None => overrides
                .get(&key)
                .map(|over| (over.model.clone(), over.thinking_level.clone())),
        };
        if let Some((model, thinking_level)) = choice {
            session.model = Some(model);
            if thinking_level.is_some() {
                session.thinking_level = thinking_level;
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

    fn session(id: &str, persona: Option<&str>) -> DiscoveredSessionSummary {
        DiscoveredSessionSummary {
            id: id.to_string(),
            source: "route".to_string(),
            adapter: "claude".to_string(),
            persona: persona.map(str::to_string),
            status: "active".to_string(),
            updated_at: None,
            cwd: None,
            record_id: None,
            transcript_path: None,
            quota_usage: None,
            model: Some("discovered-model".to_string()),
            thinking_level: Some("low".to_string()),
            available_models: Vec::new(),
            context_usage: None,
            execution_mode: None,
            model_is_default: false,
        }
    }

    fn over(model: &str, thinking: Option<&str>) -> ModelOverride {
        ModelOverride::new(model.to_string(), thinking.map(str::to_string))
    }

    #[test]
    fn overrides_match_by_id_first_then_by_lowercased_persona() {
        let mut sessions = vec![
            session("claude:ada", Some("Ada")),
            session("codex:bob", Some("Bob")),
            session("grok:cy", None),
        ];
        let mut overrides = BTreeMap::from([
            ("claude:ada".to_string(), over("by-id", None)),
            // A persona key that also names session 1: the id key wins.
            ("ada".to_string(), over("by-persona-loses", Some("high"))),
            ("bob".to_string(), over("by-persona", Some("high"))),
        ]);
        apply_model_overrides(&mut sessions, &mut overrides);
        assert_eq!(sessions[0].model.as_deref(), Some("by-id"));
        // No thinking level in the override: the discovered one stays.
        assert_eq!(sessions[0].thinking_level.as_deref(), Some("low"));
        assert_eq!(sessions[1].model.as_deref(), Some("by-persona"));
        assert_eq!(sessions[1].thinking_level.as_deref(), Some("high"));
        // No id match and no persona: untouched.
        assert_eq!(sessions[2].model.as_deref(), Some("discovered-model"));
    }

    #[test]
    fn an_uppercase_override_key_is_never_matched_by_persona() {
        // `HubState::set_session_model_override` lowercases persona keys, so a key that still
        // has capitals can only match a session id exactly.
        let mut sessions = vec![session("x", Some("Ada"))];
        let mut overrides = BTreeMap::from([("Ada".to_string(), over("m", None))]);
        apply_model_overrides(&mut sessions, &mut overrides);
        assert_eq!(sessions[0].model.as_deref(), Some("discovered-model"));
    }

    fn live(
        id: &str,
        persona: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> DiscoveredSessionSummary {
        let mut s = session(id, persona);
        s.model = model.map(str::to_string);
        s.thinking_level = effort.map(str::to_string);
        s
    }

    /// One apply over a single session; returns its (model, effort) afterwards.
    fn apply_one(
        overrides: &mut BTreeMap<String, ModelOverride>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> (Option<String>, Option<String>) {
        let mut sessions = vec![live("s", None, model, effort)];
        apply_model_overrides(&mut sessions, overrides);
        (
            sessions[0].model.clone(),
            sessions[0].thinking_level.clone(),
        )
    }

    fn pair(model: &str, effort: Option<&str>) -> (Option<String>, Option<String>) {
        (Some(model.to_string()), effort.map(str::to_string))
    }

    fn one(key: &str, o: ModelOverride) -> BTreeMap<String, ModelOverride> {
        BTreeMap::from([(key.to_string(), o)])
    }

    /// [`apply_one`] for a session whose model is the configured default, not a live report.
    fn apply_default_guess(
        overrides: &mut BTreeMap<String, ModelOverride>,
        model: &str,
        effort: Option<&str>,
    ) -> (Option<String>, Option<String>) {
        let mut flagged = live("s", None, Some(model), effort);
        flagged.model_is_default = true;
        let mut sessions = vec![flagged];
        apply_model_overrides(&mut sessions, overrides);
        (
            sessions[0].model.clone(),
            sessions[0].thinking_level.clone(),
        )
    }

    #[test]
    fn a_default_model_guess_leaves_the_override_alone_and_records_no_baseline() {
        let mut map = one("s", over("chosen", Some("high")));
        let got = apply_default_guess(&mut map, "guess", Some("medium"));
        assert_eq!(got, pair("chosen", Some("high")));
        assert_eq!(map["s"].baseline, None);
        assert!(!map["s"].confirmed);
        assert_eq!(map.len(), 1);
        // The next real report records its own pair as the baseline, not the guess.
        let got = apply_one(&mut map, Some("old"), Some("low"));
        assert_eq!(got, pair("chosen", Some("high")));
        assert_eq!(map["s"].baseline, Some(pair("old", Some("low"))));
    }

    #[test]
    fn a_default_model_guess_does_not_drop_a_confirmed_override() {
        let mut map = one("s", over("chosen", Some("high")));
        apply_one(&mut map, Some("old"), Some("low"));
        apply_one(&mut map, Some("chosen"), Some("high"));
        assert!(map["s"].confirmed);
        let before = map.clone();
        // A guess that differs from the baseline and the choice would drop it as a live report.
        let got = apply_default_guess(&mut map, "third", Some("medium"));
        assert_eq!(got, pair("chosen", Some("high")));
        assert_eq!(map, before);
        // A guess equal to the baseline would drop a confirmed override too.
        let got = apply_default_guess(&mut map, "old", Some("low"));
        assert_eq!(got, pair("chosen", Some("high")));
        assert_eq!(map, before);
        // Without an override the guess still shows.
        let mut empty = BTreeMap::new();
        let got = apply_default_guess(&mut empty, "guess", None);
        assert_eq!(got, pair("guess", None));
    }

    #[test]
    fn a_default_model_guess_does_not_decide_for_a_sibling_session_of_the_override() {
        let mut map = one("ada", over("chosen", None));
        let mut guess = live("claude:a", Some("Ada"), Some("guess"), None);
        guess.model_is_default = true;
        let real = live("claude:b", Some("Ada"), Some("old"), None);
        let mut sessions = vec![guess, real];
        apply_model_overrides(&mut sessions, &mut map);
        // The first session reporting a model is the real one, so it sets the baseline.
        assert_eq!(map["ada"].baseline, Some(pair("old", None)));
        assert!(
            sessions
                .iter()
                .all(|s| s.model.as_deref() == Some("chosen"))
        );
    }

    #[test]
    fn the_first_apply_records_the_baseline_and_overlays() {
        let mut map = one("s", over("chosen", Some("high")));
        let got = apply_one(&mut map, Some("old"), Some("low"));
        assert_eq!(got, pair("chosen", Some("high")));
        let kept = &map["s"];
        assert_eq!(kept.baseline, Some(pair("old", Some("low"))));
        assert!(!kept.confirmed);
        assert_eq!(
            (kept.model.as_str(), kept.thinking_level.as_deref()),
            ("chosen", Some("high"))
        );
    }

    #[test]
    fn a_live_report_equal_to_the_choice_confirms_it() {
        let mut map = one("s", over("chosen", Some("high")));
        apply_one(&mut map, Some("old"), Some("low"));
        let got = apply_one(&mut map, Some("chosen"), Some("high"));
        assert_eq!(got, pair("chosen", Some("high")));
        assert!(map["s"].confirmed);
        assert_eq!(map["s"].baseline, Some(pair("old", Some("low"))));
    }

    #[test]
    fn a_live_report_differing_from_baseline_and_choice_drops_the_override() {
        let mut map = one("s", over("chosen", Some("high")));
        apply_one(&mut map, Some("old"), Some("low"));
        let got = apply_one(&mut map, Some("third"), Some("medium"));
        assert_eq!(got, pair("third", Some("medium")));
        assert!(map.is_empty());
        // Unconfirmed or confirmed, the newer report wins.
        let mut map = one("s", over("chosen", None));
        apply_one(&mut map, Some("old"), None);
        apply_one(&mut map, Some("chosen"), None);
        assert!(map["s"].confirmed);
        let got = apply_one(&mut map, Some("third"), None);
        assert_eq!(got, pair("third", None));
        assert!(map.is_empty());
    }

    #[test]
    fn a_confirmed_override_is_dropped_when_the_vendor_goes_back_to_the_baseline() {
        let mut map = one("s", over("chosen", Some("high")));
        apply_one(&mut map, Some("old"), Some("low"));
        apply_one(&mut map, Some("chosen"), Some("high"));
        let got = apply_one(&mut map, Some("old"), Some("low"));
        assert_eq!(got, pair("old", Some("low")));
        assert!(map.is_empty());
    }

    #[test]
    fn an_unconfirmed_override_stays_while_the_vendor_still_reports_the_baseline() {
        let mut map = one("s", over("chosen", Some("high")));
        for _ in 0..3 {
            let got = apply_one(&mut map, Some("old"), Some("low"));
            assert_eq!(got, pair("chosen", Some("high")));
            assert!(map.contains_key("s"));
        }
        assert!(!map["s"].confirmed);
    }

    #[test]
    fn a_session_with_no_live_model_leaves_the_override_untouched() {
        for empty in [None, Some(""), Some("  ")] {
            let mut map = one("s", over("chosen", None));
            let before = map.clone();
            let got = apply_one(&mut map, empty, Some("low"));
            assert_eq!(got, pair("chosen", Some("low")), "{empty:?}");
            assert_eq!(map, before, "{empty:?}: no baseline is recorded");
        }
        // It then behaves as a first apply once a model shows.
        let mut map = one("s", over("chosen", None));
        apply_one(&mut map, None, None);
        apply_one(&mut map, Some("old"), None);
        assert_eq!(map["s"].baseline, Some(pair("old", None)));
    }

    #[test]
    fn a_persona_keyed_override_is_superseded_like_an_id_keyed_one() {
        let mut map = one("ada", over("chosen", None));
        let mut sessions = vec![live("claude:ada", Some("Ada"), Some("old"), Some("low"))];
        apply_model_overrides(&mut sessions, &mut map);
        assert_eq!(sessions[0].model.as_deref(), Some("chosen"));
        assert_eq!(map["ada"].baseline, Some(pair("old", Some("low"))));
        let mut sessions = vec![live("claude:ada", Some("Ada"), Some("third"), Some("low"))];
        apply_model_overrides(&mut sessions, &mut map);
        assert_eq!(sessions[0].model.as_deref(), Some("third"));
        assert!(map.is_empty());
    }

    #[test]
    fn an_id_key_dropped_in_a_call_does_not_hand_its_session_to_the_persona_key() {
        let mut map = BTreeMap::from([
            ("claude:ada".to_string(), over("by-id", None)),
            ("ada".to_string(), over("by-persona", None)),
        ]);
        let mut sessions = vec![live("claude:ada", Some("Ada"), Some("old"), None)];
        apply_model_overrides(&mut sessions, &mut map);
        let mut sessions = vec![live("claude:ada", Some("Ada"), Some("third"), None)];
        apply_model_overrides(&mut sessions, &mut map);
        assert_eq!(sessions[0].model.as_deref(), Some("third"));
        assert!(!map.contains_key("claude:ada"));
        // The persona key was shadowed by the dropped id key, so it goes with it; otherwise the
        // next call would match it, record the live model as its baseline and keep it.
        assert!(!map.contains_key("ada"));
        let mut sessions = vec![live("claude:ada", Some("Ada"), Some("third"), None)];
        apply_model_overrides(&mut sessions, &mut map);
        assert_eq!(sessions[0].model.as_deref(), Some("third"));
        assert!(map.is_empty());
    }

    #[test]
    fn a_shadowed_persona_key_does_not_resurrect_after_its_id_key_is_dropped() {
        let mut map = BTreeMap::from([
            ("claude:ada".to_string(), over("m", None)),
            ("ada".to_string(), over("m", None)),
        ]);
        let run = |map: &mut BTreeMap<String, ModelOverride>, model: &str| {
            let mut sessions = vec![live("claude:ada", Some("ada"), Some(model), None)];
            apply_model_overrides(&mut sessions, map);
            sessions[0].model.clone()
        };
        assert_eq!(run(&mut map, "old").as_deref(), Some("m"));
        assert_eq!(run(&mut map, "third").as_deref(), Some("third"));
        assert_eq!(run(&mut map, "third").as_deref(), Some("third"));
        assert!(map.is_empty());
    }

    #[test]
    fn a_persona_key_that_still_serves_another_session_survives_an_id_key_drop() {
        for reversed in [false, true] {
            let mut map = BTreeMap::from([
                ("claude:ada1".to_string(), over("m", None)),
                ("ada".to_string(), over("m", None)),
            ]);
            let build = |m1: &str, m2: &str| {
                let mut v = vec![
                    live("claude:ada1", Some("Ada"), Some(m1), None),
                    live("claude:ada2", Some("Ada"), Some(m2), None),
                ];
                if reversed {
                    v.reverse();
                }
                v
            };
            let mut sessions = build("old", "old");
            apply_model_overrides(&mut sessions, &mut map);
            let mut sessions = build("third", "old");
            apply_model_overrides(&mut sessions, &mut map);
            assert!(!map.contains_key("claude:ada1"), "reversed={reversed}");
            assert!(map.contains_key("ada"), "reversed={reversed}");
            for s in &sessions {
                let want = if s.id == "claude:ada1" { "third" } else { "m" };
                assert_eq!(
                    s.model.as_deref(),
                    Some(want),
                    "{} reversed={reversed}",
                    s.id
                );
            }
        }
    }

    #[test]
    fn a_dropped_persona_key_with_no_id_key_behaves_as_before() {
        let mut map = BTreeMap::from([
            ("ada".to_string(), over("m", None)),
            ("bob".to_string(), over("n", None)),
        ]);
        let mut sessions = vec![
            live("claude:ada", Some("Ada"), Some("old"), None),
            live("codex:bob", Some("Bob"), Some("old"), None),
        ];
        apply_model_overrides(&mut sessions, &mut map);
        let mut sessions = vec![
            live("claude:ada", Some("Ada"), Some("third"), None),
            live("codex:bob", Some("Bob"), Some("old"), None),
        ];
        apply_model_overrides(&mut sessions, &mut map);
        assert_eq!(sessions[0].model.as_deref(), Some("third"));
        assert_eq!(sessions[1].model.as_deref(), Some("n"));
        assert!(!map.contains_key("ada"));
        assert!(map.contains_key("bob"));
    }

    #[test]
    fn an_override_without_effort_keeps_the_live_effort_and_ignores_effort_changes() {
        let mut map = one("s", over("chosen", None));
        assert_eq!(
            apply_one(&mut map, Some("old"), Some("low")),
            pair("chosen", Some("low"))
        );
        // The vendor now runs the chosen model at another effort: that is the choice, not a
        // divergence.
        assert_eq!(
            apply_one(&mut map, Some("chosen"), Some("high")),
            pair("chosen", Some("high"))
        );
        assert!(map["s"].confirmed);
        assert_eq!(
            apply_one(&mut map, Some("chosen"), None),
            pair("chosen", None)
        );
        assert!(map.contains_key("s"));
        // An override with an effort needs the live effort to match.
        let mut map = one("s", over("chosen", Some("high")));
        apply_one(&mut map, Some("old"), Some("low"));
        // chosen/low is neither the choice (chosen/high) nor the baseline (old/low).
        apply_one(&mut map, Some("chosen"), Some("low"));
        assert!(
            !map.contains_key("s"),
            "chosen/low differs from old/low and chosen/high"
        );
    }

    #[test]
    fn a_live_model_without_an_effort_confirms_an_override_that_has_one() {
        // Muse and Claude with no effort signal never report one, and the UI always sends one.
        let mut map = one("s", over("chosen", Some("high")));
        apply_one(&mut map, Some("old"), Some("low"));
        let got = apply_one(&mut map, Some("chosen"), None);
        assert_eq!(
            got,
            pair("chosen", Some("high")),
            "effort from the override"
        );
        assert!(map.contains_key("s"), "the override is kept");
        assert!(map["s"].confirmed);
        // And it stays on later reads of the same unknown effort.
        assert_eq!(
            apply_one(&mut map, Some("CHOSEN"), None),
            pair("chosen", Some("high"))
        );
        assert!(map.contains_key("s"));
        // A different reported effort is still not a match: chosen/low differs from the choice
        // and from the old/None baseline, so the vendor reports something newer.
        let mut map = one("s", over("chosen", Some("high")));
        apply_one(&mut map, Some("old"), None);
        apply_one(&mut map, Some("chosen"), Some("low"));
        assert!(!map.contains_key("s"));
        // A different model with no effort is not a match either.
        let mut map = one("s", over("chosen", Some("high")));
        apply_one(&mut map, Some("old"), Some("low"));
        apply_one(&mut map, Some("third"), None);
        assert!(!map.contains_key("s"));
    }

    #[test]
    fn comparisons_ignore_ascii_case() {
        let mut map = one("s", over("Chosen", Some("HIGH")));
        apply_one(&mut map, Some("Old"), Some("Low"));
        apply_one(&mut map, Some("chosen"), Some("high"));
        assert!(map["s"].confirmed);
        // Back to the baseline in another case: dropped, not read as a new model.
        let got = apply_one(&mut map, Some("OLD"), Some("low"));
        assert_eq!(got, pair("OLD", Some("low")));
        assert!(map.is_empty());
        // Unconfirmed and a baseline in another case: kept.
        let mut map = one("s", over("chosen", None));
        apply_one(&mut map, Some("Old"), Some("Low"));
        assert_eq!(
            apply_one(&mut map, Some("old"), Some("LOW")),
            pair("chosen", Some("LOW"))
        );
        assert!(map.contains_key("s"));
    }

    #[test]
    fn two_sessions_with_their_own_overrides_evolve_independently() {
        let mut map = BTreeMap::from([
            ("a".to_string(), over("chosen-a", None)),
            ("b".to_string(), over("chosen-b", None)),
        ]);
        let mut sessions = vec![
            live("a", None, Some("old-a"), None),
            live("b", None, Some("old-b"), None),
        ];
        apply_model_overrides(&mut sessions, &mut map);
        assert_eq!(sessions[0].model.as_deref(), Some("chosen-a"));
        assert_eq!(sessions[1].model.as_deref(), Some("chosen-b"));
        // `a` is switched inside the vendor UI; `b` is still on its baseline.
        let mut sessions = vec![
            live("a", None, Some("elsewhere"), None),
            live("b", None, Some("old-b"), None),
        ];
        apply_model_overrides(&mut sessions, &mut map);
        assert_eq!(sessions[0].model.as_deref(), Some("elsewhere"));
        assert_eq!(sessions[1].model.as_deref(), Some("chosen-b"));
        assert!(!map.contains_key("a"));
        assert!(map.contains_key("b"));
    }

    #[test]
    fn one_override_matching_two_sessions_is_decided_by_the_first_match() {
        let mut map = one("ada", over("chosen", None));
        let mut sessions = vec![
            live("claude:ada", Some("Ada"), Some("old"), None),
            live("codex:ada", Some("Ada"), Some("other"), None),
        ];
        apply_model_overrides(&mut sessions, &mut map);
        // The first match records the baseline; the second is overlaid on that decision.
        assert_eq!(map["ada"].baseline, Some(pair("old", None)));
        assert_eq!(sessions[0].model.as_deref(), Some("chosen"));
        assert_eq!(sessions[1].model.as_deref(), Some("chosen"));
        // The first match reports a newer model: both keep their live values.
        let mut sessions = vec![
            live("claude:ada", Some("Ada"), Some("third"), None),
            live("codex:ada", Some("Ada"), Some("old"), None),
        ];
        apply_model_overrides(&mut sessions, &mut map);
        assert_eq!(sessions[0].model.as_deref(), Some("third"));
        assert_eq!(sessions[1].model.as_deref(), Some("old"));
        assert!(map.is_empty());
    }

    #[test]
    fn applying_repeatedly_over_the_same_live_state_is_stable() {
        let mut map = one("s", over("chosen", Some("high")));
        apply_one(&mut map, Some("old"), Some("low"));
        let after_first = map.clone();
        for _ in 0..4 {
            assert_eq!(
                apply_one(&mut map, Some("old"), Some("low")),
                pair("chosen", Some("high"))
            );
            assert_eq!(map, after_first);
        }
        apply_one(&mut map, Some("chosen"), Some("high"));
        let confirmed = map.clone();
        for _ in 0..4 {
            assert_eq!(
                apply_one(&mut map, Some("chosen"), Some("high")),
                pair("chosen", Some("high"))
            );
            assert_eq!(map, confirmed);
        }
    }

    #[test]
    fn a_fresh_set_after_a_drop_starts_over() {
        let mut map = one("s", over("chosen", None));
        apply_one(&mut map, Some("old"), None);
        apply_one(&mut map, Some("third"), None);
        assert!(map.is_empty());
        // The operator picks again while the vendor reports `third`.
        map.insert("s".to_string(), over("again", None));
        assert_eq!(
            apply_one(&mut map, Some("third"), None),
            pair("again", None)
        );
        assert_eq!(map["s"].baseline, Some(pair("third", None)));
        assert!(!map["s"].confirmed);
        // Replacing a confirmed override also resets it.
        apply_one(&mut map, Some("again"), None);
        assert!(map["s"].confirmed);
        map.insert("s".to_string(), over("again", None));
        assert_eq!(map["s"], over("again", None));
    }

    #[test]
    fn model_validation_counts_characters_not_bytes_and_keeps_inner_spaces() {
        // 128 two-byte characters are within the cap; a model may hold spaces and unicode.
        assert!(validate_model(&"é".repeat(MAX_MODEL_CHARS)).is_ok());
        assert!(validate_model(&"é".repeat(MAX_MODEL_CHARS + 1)).is_err());
        assert_eq!(
            validate_model("gpt 5 (preview)").unwrap(),
            "gpt 5 (preview)"
        );
        // C1 controls (NEL and friends) are controls too.
        assert!(validate_model("a\u{85}b").is_err());
        assert!(validate_model("a\u{9b}31m").is_err());
    }
}
