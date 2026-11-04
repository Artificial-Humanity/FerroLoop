//! Recorded help/schema tripwires. These do not inspect or prove a currently installed binary.

const CLAUDE: &str = include_str!("fixtures/slice-proof/claude-2.1.317-help.txt");
const MUSE_EXEC: &str = include_str!("fixtures/slice-proof/muse-1.4.14-exec-help.txt");
const MUSE_TOP: &str = include_str!("fixtures/slice-proof/muse-1.4.14-help.txt");
const CODEX_TURN: &str = include_str!("fixtures/slice-proof/codex-0.166.0-turn-start.json");

/// Match an actual option token, not a substring in prose or in a longer flag.
fn option_present(help: &str, flag: &str) -> bool {
    help.lines().any(|line| {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('-') {
            return false;
        }
        trimmed
            .split(|c: char| c.is_ascii_whitespace() || c == ',' || c == '=' || c == '<')
            .any(|word| word == flag)
    })
}
fn missing_muse_exec_flags(help: &str) -> Vec<&'static str> {
    [
        "--max-model-steps",
        "--max-turns",
        "--max-tool-output-bytes",
    ]
    .into_iter()
    .filter(|flag| !option_present(help, flag))
    .collect()
}

#[test]
fn claude_budget_flag_is_pinned_to_recorded_help() {
    assert!(option_present(CLAUDE, "--max-budget-usd"));
    assert!(!option_present(
        &CLAUDE.replace("--max-budget-usd", "--removed-budget"),
        "--max-budget-usd"
    ));
}

#[test]
fn muse_record_shows_both_present_knobs_and_missing_max_turns() {
    assert!(option_present(MUSE_EXEC, "--max-model-steps"));
    assert!(option_present(MUSE_EXEC, "--max-tool-output-bytes"));
    assert!(!option_present(MUSE_TOP, "--max-turns"));
    assert_eq!(missing_muse_exec_flags(MUSE_EXEC), ["--max-turns"]);
    let removed = MUSE_EXEC.replace("--max-model-steps", "--removed-model-steps");
    assert_eq!(
        missing_muse_exec_flags(&removed),
        ["--max-model-steps", "--max-turns"]
    );
}

/// Run explicitly when evaluating the candidate: this is red on the captured 1.4.14 help.
/// An absent required knob is not proof that a vendor is safe to admit.
#[test]
#[ignore = "known missing --max-turns in captured Muse 1.4.14 help; candidate remains unproven"]
fn muse_all_candidate_flags_are_available() {
    assert!(
        missing_muse_exec_flags(MUSE_EXEC).is_empty(),
        "missing: {:?}",
        missing_muse_exec_flags(MUSE_EXEC)
    );
}

#[test]
fn codex_turn_schema_pins_input_thread_and_absence_of_step_controls() {
    let schema: serde_json::Value = serde_json::from_str(CODEX_TURN).unwrap();
    assert_eq!(schema["title"], "TurnStartParams");
    let required = schema["required"].as_array().unwrap();
    for field in ["input", "threadId"] {
        assert!(required.iter().any(|x| x == field));
        assert!(schema["properties"].get(field).is_some());
    }
    // Neither a single output limit nor a turn request is evidence of a bound on internal steps.
    let fields = schema["properties"].as_object().unwrap();
    assert!(!fields.contains_key("maxOutputTokens"));
    assert!(!fields.contains_key("maxSteps"));
}
