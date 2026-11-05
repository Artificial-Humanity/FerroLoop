//! L3 live whole-slice proof experiments (Record #83 Track C). SKELETON ONLY.
//!
//! OWNER-RUN ONLY, NEVER CI. Every test here is `#[ignore]` and, even when run with
//! `--ignored`, returns immediately unless `FL_LIVE_PROOF=1`. A skipped live test therefore
//! reports `ok` (not `ignored`); its skip message goes to stderr and is visible only with
//! `--nocapture`. A run without `FL_LIVE_PROOF=1` proves nothing. These live tests are meant to
//! spend real (tiny) money and tokens; this file in its present form spawns no vendor binary
//! and makes no network call. The live step is `unimplemented!()` behind the gate so that
//! `FL_LIVE_PROOF=1 cargo test -p fl-exec --test slice_proof_live -- --ignored` fails loudly
//! until a reviewed PR fills it in, instead of silently passing.
//!
//! Whole-matrix spend budget (all six adapters, all three tasks): `<PLACEHOLDER: Owner to set
//! before the first live run; the TDD names L3 spend authorisation as an Owner decision>`.
//!
//! A candidate cap is UNPROVEN during its own evaluation, so every experiment runs under an
//! explicit Owner experiment allocation, separate from admission budgets, with isolated test
//! credentials and data, no automatic retries, no subagents beyond the fixture under test,
//! an outer process deadline and cost alerts. A self-cap of a few cents is an aspiration for
//! the first run, not a known safety bound. Normal admissions stay denied throughout.
//! Overrun, missing telemetry or inability to prove is reported as ManualOnly, never retried
//! into compliance. L3 only corroborates the reviewed pre-dispatch derivation; it never
//! establishes a proof by itself.
//!
//! Engineered tasks, run per adapter where the adapter can express them:
//!   (a) COUNT ALOUD: "count aloud in separate steps as high as you can" (many turns).
//!   (b) TOOL LOOP: a task that forces a multi-iteration tool loop.
//!   (c) SUBAGENT: a task that forces exactly one subagent spawn (muse and claude only;
//!       without (b) and (c) the matrix's tool-loop and subagent-counted columns stay empty).
//!
//! Record for each run: observed stop behaviour; final total tokens and cost from the
//! vendor's own usage output; overshoot against the cap (observed final cost minus cap).

/// What one experiment must be told before it may run. Filling in real values is the
/// Owner's authorisation; the skeleton carries placeholders only.
struct Experiment {
    adapter: &'static str,
    /// Owner experiment allocation in micro-USD, separate from any admission budget.
    allocation_usd_micros: u64,
    /// Candidate cap under test, in the unit of the adapter's knob (unproven while tested).
    candidate_cap: &'static str,
    /// Outer process deadline that kills the vendor whatever the cap does.
    outer_deadline_secs: u64,
    /// Which of the engineered tasks apply: (a) count aloud, (b) tool loop, (c) subagent.
    tasks: &'static [char],
}

/// `Some(skip message)` unless the Owner has opted in with `FL_LIVE_PROOF=1`.
fn gate() -> Option<&'static str> {
    if std::env::var("FL_LIVE_PROOF") == Ok("1".to_string()) {
        None
    } else {
        Some("FL_LIVE_PROOF=1 is not set: live whole-slice proof skipped, nothing spawned or spent")
    }
}

/// The only place the live step may ever be written. It is deliberately unimplemented, so an
/// opted-in run fails loudly. Implementing it needs a reviewed PR and the Owner's allocation.
fn run_live(exp: &Experiment) -> ! {
    let _ = (
        exp.allocation_usd_micros,
        exp.candidate_cap,
        exp.outer_deadline_secs,
        exp.tasks,
    );
    unimplemented!(
        "L3 live proof for `{}` is a skeleton: no vendor spawn or network call is implemented",
        exp.adapter
    )
}

macro_rules! live_proof {
    ($name:ident, $adapter:literal, $cap:literal, $tasks:expr, $doc:literal) => {
        #[test]
        #[ignore = "owner-run live spend: set FL_LIVE_PROOF=1 and run with --ignored"]
        #[doc = $doc]
        fn $name() {
            if let Some(why) = gate() {
                eprintln!("{why} ({})", $adapter);
                return;
            }
            run_live(&Experiment {
                adapter: $adapter,
                allocation_usd_micros: 0, // PLACEHOLDER: Owner sets the experiment allocation
                candidate_cap: $cap,
                outer_deadline_secs: 0, // PLACEHOLDER: Owner sets the outer deadline
                tasks: $tasks,
            })
        }
    };
}

live_proof!(
    live_claude,
    "claude",
    "--max-budget-usd (P-A: cap + epsilon; check subagents, subscription vs API, stream-json)",
    &['a', 'b', 'c'],
    "claude: does the cap cover the whole agentic loop including subagents; overshoot <= epsilon?"
);
live_proof!(
    live_muse,
    "muse",
    "--max-model-steps with --max-tool-output-bytes (P-B: steps x (input + output) x price)",
    &['a', 'b', 'c'],
    "muse: are subagent steps, auto-compaction and internal planner calls inside the step count?"
);
live_proof!(
    live_codex,
    "codex",
    "TurnStartParams fields only (expected Unproven: internal steps in a turn are vendor-driven)",
    &['a', 'b'],
    "codex: is anything but per-request output cappable inside one app-server turn?"
);
live_proof!(
    live_antigravity,
    "antigravity",
    "--print-timeout is time, not tokens (expected Unproven; borrowed endpoint, no automated work)",
    &['a', 'b'],
    "antigravity: is there any token or step accounting for a cascade?"
);
live_proof!(
    live_grok,
    "grok",
    "headless -p output or token caps (unknown; Unproven until a knob is found and tested)",
    &['a', 'b'],
    "grok: can one -p invocation loop internally without bound?"
);
live_proof!(
    live_api,
    "api",
    "api.rs output cap (P-B candidate, NOT proven: input is estimated as instruction_len/4)",
    &['a', 'b'],
    "api: the first test subject; are request input, all billed output classes and retries bounded?"
);
