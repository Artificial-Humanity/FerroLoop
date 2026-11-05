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

/// A candidate's recorded help preflight; even AllPresent does not establish a proof.
struct Candidate<'a> {
    required_knobs: &'a [&'a str],
}
#[derive(Debug, PartialEq, Eq)]
enum KnobCheck<'a> {
    AllPresent,
    UnprovenMissingKnobs(Vec<&'a str>),
}
impl<'a> Candidate<'a> {
    fn check(&self, help: &str) -> KnobCheck<'a> {
        let missing: Vec<_> = self
            .required_knobs
            .iter()
            .copied()
            .filter(|knob| !option_present(help, knob))
            .collect();
        if missing.is_empty() {
            KnobCheck::AllPresent
        } else {
            KnobCheck::UnprovenMissingKnobs(missing)
        }
    }
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
fn muse_candidate_requiring_missing_turns_is_unproven() {
    let candidate = Candidate {
        required_knobs: &[
            "--max-model-steps",
            "--max-turns",
            "--max-tool-output-bytes",
        ],
    };
    assert!(!option_present(MUSE_TOP, "--max-turns"));
    assert_eq!(
        candidate.check(MUSE_EXEC),
        KnobCheck::UnprovenMissingKnobs(vec!["--max-turns"])
    );
}

#[test]
fn muse_observed_knobs_are_pinned_and_missing_either_rejects_candidate() {
    let candidate = Candidate {
        required_knobs: &["--max-model-steps", "--max-tool-output-bytes"],
    };
    assert_eq!(candidate.check(MUSE_EXEC), KnobCheck::AllPresent);
    for knob in candidate.required_knobs {
        let changed = MUSE_EXEC.replace(knob, "--removed-knob");
        assert_eq!(
            candidate.check(&changed),
            KnobCheck::UnprovenMissingKnobs(vec![knob])
        );
    }
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

#[test]
fn option_matcher_ignores_prose_and_longer_or_shorter_flags() {
    let help = "  --max-budget-usd-extra <n>  not the knob\n  Use --max-model-steps to cap work\n  --max-model <n>\n";
    assert!(!option_present(help, "--max-budget-usd"));
    assert!(!option_present(help, "--max-model-steps")); // prose, not an option line
    assert!(option_present(help, "--max-model"));
    assert!(option_present(
        "  -b, --max-budget-usd <amount>  cap",
        "--max-budget-usd"
    ));
    assert!(option_present(
        "      --max-model-steps=<n>",
        "--max-model-steps"
    ));
}
