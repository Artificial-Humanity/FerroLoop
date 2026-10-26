//! The Claude Code adapter.
//!
//! ⚠ Flags below were read from `claude --help` and `claude --version` on
//! **this machine**, version `2.1.273 (Claude Code)`, on 2026-09-20. Re-read
//! them before changing anything here: the flags are a property of the
//! installed binary, not of this file.
//!
//! Verified flags this adapter relies on (all present in the real `--help`
//! output, none carried over from memory):
//! - `prompt` — a positional argument: "Your prompt".
//! - `-p, --print` — "Print response and exit (useful for pipes)"; without
//!   it the CLI "starts an interactive session by default", which is not
//!   usable from a piped, non-tty child process.
//! - `--output-format json` — structured JSON output envelope containing
//!   `result`, `total_cost_usd`, and `usage` fields.
//! - `--max-budget-usd <amount>` — "Maximum dollar amount to spend on API
//!   calls (only works with --print)". This adapter derives it from
//!   `AttemptSpec::budget_usd_micros` and passes it through so the vendor
//!   binary enforces the same ceiling this adapter already refuses on at
//!   zero — defense in depth, not a substitute for the pre-flight check.

use crate::adapters::harness::{
    CliDialect, LivenessSink, ParsedOutput, ProcessHarness, parse_json_or_jsonl_envelope,
};
use crate::runner::{AttemptError, AttemptOutcome, AttemptSpec, Runner};
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::telemetry::{AdapterKind, RateCard};
use tokio::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeDialect;

impl CliDialect for ClaudeDialect {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Claude
    }

    fn default_binary(&self) -> &'static str {
        AdapterKind::Claude.default_binary()
    }

    fn argv(&self, spec: &AttemptSpec) -> Vec<String> {
        let budget_usd = spec.budget_usd_micros as f64 / 1_000_000.0;
        vec![
            "-p".into(),
            spec.instruction.clone(),
            "--output-format".into(),
            "json".into(),
            "--max-budget-usd".into(),
            format!("{budget_usd:.6}"),
        ]
    }

    fn parse_output(&self, stdout: &[u8], stderr: &[u8]) -> ParsedOutput {
        parse_json_or_jsonl_envelope(stdout, stderr, false)
    }
}

/// Drives the real `claude` CLI as a [`Runner`].
pub struct ClaudeAdapter {
    harness: ProcessHarness<ClaudeDialect>,
}

impl ClaudeAdapter {
    pub fn new(binary: String) -> Self {
        Self {
            harness: ProcessHarness::new(binary, ClaudeDialect),
        }
    }

    pub fn with_rate_card(mut self, rate_card: Option<RateCard>) -> Self {
        self.harness = self.harness.with_rate_card(rate_card);
        self
    }

    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.harness = self.harness.with_heartbeat_interval(interval);
        self
    }

    pub fn with_liveness(
        mut self,
        attempt_id: Iri,
        project: ProjectId,
        agent: Option<String>,
        sink: LivenessSink,
    ) -> Self {
        self.harness = self.harness.with_liveness(attempt_id, project, agent, sink);
        self
    }
}

impl Runner for ClaudeAdapter {
    fn id(&self) -> &str {
        "claude"
    }

    async fn attempt(&self, spec: AttemptSpec) -> Result<AttemptOutcome, AttemptError> {
        Ok(self.harness.run(spec).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::harness::EXCERPT_LIMIT;
    use fl_core::ids::{RecordId, seq_iri};
    use fl_core::log::AttemptStatus;

    fn spec(root: &std::path::Path, timeout: u64) -> AttemptSpec {
        AttemptSpec {
            project_root: root.to_path_buf(),
            record: RecordId(seq_iri(1)),
            instruction: "say hello".into(),
            timeout_secs: timeout,
            budget_usd_micros: 1_000_000,
        }
    }

    #[tokio::test]
    async fn a_missing_binary_is_refused_and_never_reported_as_completed() {
        let d = tempfile::tempdir().unwrap();
        let a = ClaudeAdapter::new("definitely-not-a-real-program-9f3x".into());
        let out = a.attempt(spec(d.path(), 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Refused);
        assert!(
            out.output_excerpt
                .contains("definitely-not-a-real-program-9f3x")
        );
    }

    // ⚠ The brief's own version of this test spawns the literal `sleep`
    // binary and hands it the shared `spec()` instruction ("say hello") as
    // an extra argv token. That trips the same trap already documented in
    // `command.rs`'s tests, one token earlier: this box's `sleep` is uutils
    // coreutils 0.8.0 (confirmed with `sleep --version`), which parses argv
    // strictly and refuses ANY unrecognized token — `sleep "say hello"`
    // prints "invalid time interval 'say hello'" and exits in a few
    // milliseconds, never sleeping. Verified by hand:
    //   $ sleep "say hello"; echo "exit:$?"   -> exit:1, ~0.003s
    //   $ sleep -p "say hello"; echo "exit:$?" -> exit:1, "unexpected argument '-p'"
    // The second line matters here: the production command line below
    // starts with `-p`/`--print` (the real, verified non-interactive flag),
    // so even swapping in a numeric instruction would not save a literal
    // `sleep` stand-in — it fails on the flag before it ever reaches a
    // number. A stub script that ignores its argv and always sleeps
    // decouples the test from whatever flags the adapter happens to pass,
    // which is the property the brief says these tests should have.
    #[tokio::test]
    async fn a_timeout_is_a_timeout_and_not_a_completion() {
        let d = tempfile::tempdir().unwrap();
        let stub = d.path().join("hangs-regardless-of-argv.sh");
        std::fs::write(&stub, "#!/bin/sh\nsleep 30\n").unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let a = ClaudeAdapter::new(stub.to_string_lossy().into_owned());
        let out = a.attempt(spec(d.path(), 1)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Timeout);
        assert!(out.duration_ms >= 1000, "got {}ms", out.duration_ms);
    }

    #[tokio::test]
    async fn a_zero_budget_is_refused_before_anything_is_spawned() {
        let d = tempfile::tempdir().unwrap();
        let a = ClaudeAdapter::new("sleep".into());
        let mut s = spec(d.path(), 30);
        s.budget_usd_micros = 0;
        let out = a.attempt(s).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Refused);
        assert_eq!(out.duration_ms, 0, "nothing should have been spawned");
    }

    #[test]
    fn the_adapter_identifies_itself() {
        assert_eq!(ClaudeAdapter::new("claude".into()).id(), "claude");
    }

    // The only path that emits real process output (`Completed`/`Crashed`
    // via a successful spawn) was previously untested. Real `claude` output
    // routinely contains multi-byte UTF-8 (checkmarks, box-drawing), so a
    // naive `text.truncate(EXCERPT_LIMIT)` has a real chance of landing mid
    // character and panicking. U+2713 CHECK MARK is 3 bytes in UTF-8, and
    // EXCERPT_LIMIT (8192) is not a multiple of 3 (8192 % 3 == 2), so 4000
    // repeats (12,000 bytes) guarantee the byte-8192 cut point falls inside
    // a character, not on a boundary.
    #[tokio::test]
    async fn a_completed_run_truncates_multibyte_output_without_panicking() {
        let d = tempfile::tempdir().unwrap();
        let stub = d.path().join("multibyte-output.sh");
        let payload = "✓".repeat(4000);
        let script = format!("#!/bin/sh\nprintf '%s' '{payload}'\n");
        std::fs::write(&stub, script).unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let a = ClaudeAdapter::new(stub.to_string_lossy().into_owned());
        let out = a.attempt(spec(d.path(), 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
        assert!(
            out.output_excerpt.len() <= EXCERPT_LIMIT,
            "got {} bytes",
            out.output_excerpt.len()
        );
    }

    // Exercises the real `claude` binary installed on the box. Not run by
    // default: it needs a live, authenticated session and spends real
    // budget, which this suite otherwise deliberately avoids. Run
    // deliberately with `cargo test -p fl-exec -- --ignored` when a session
    // is available. Not exercised as part of this task.
    #[tokio::test]
    #[ignore]
    async fn the_real_binary_completes_a_trivial_instruction() {
        let d = tempfile::tempdir().unwrap();
        let a = ClaudeAdapter::new("claude".into());
        let out = a.attempt(spec(d.path(), 120)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
    }

    #[tokio::test]
    async fn parses_claude_json_envelope_usage_and_vendor_cost() {
        let d = tempfile::tempdir().unwrap();
        let stub = d.path().join("claude-json.sh");
        let payload = r#"{"type":"result","result":"done","total_cost_usd":0.012345,"usage":{"input_tokens":120,"cache_creation_input_tokens":30,"cache_read_input_tokens":10,"output_tokens":45}}"#;
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{payload}'\n");
        std::fs::write(&stub, script).unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let a = ClaudeAdapter::new(stub.to_string_lossy().into_owned());
        let out = a.attempt(spec(d.path(), 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
        assert_eq!(out.output_excerpt, "done");
        assert_eq!(out.tokens_in, 160);
        assert_eq!(out.tokens_out, 45);
        assert_eq!(out.cost_usd_micros, 12_345);
    }

    #[tokio::test]
    async fn falls_back_to_rate_card_when_total_cost_usd_is_absent() {
        let d = tempfile::tempdir().unwrap();
        let stub = d.path().join("claude-no-cost.sh");
        let payload = r#"{"type":"result","result":"done","usage":{"input_tokens":1000,"output_tokens":500}}"#;
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{payload}'\n");
        std::fs::write(&stub, script).unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let a = ClaudeAdapter::new(stub.to_string_lossy().into_owned()).with_rate_card(Some(
            RateCard {
                input_usd_micros_per_mtok: 3_000_000,
                output_usd_micros_per_mtok: 15_000_000,
            },
        ));
        let out = a.attempt(spec(d.path(), 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
        assert_eq!(out.tokens_in, 1000);
        assert_eq!(out.tokens_out, 500);
        // 1000 * 3 + 500 * 15 = 3000 + 7500 = 10_500 micro-USD
        assert_eq!(out.cost_usd_micros, 10_500);
    }

    #[tokio::test]
    async fn records_paths_touched_in_git_repo_and_ignores_fl_directory() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();

        let bin_dir = tempfile::tempdir().unwrap();
        let stub = bin_dir.path().join("claude-touch.sh");
        let script = "#!/bin/sh\nmkdir -p .fl src\nprintf 'ok\\n' > src/lib.rs\nprintf 'ignored\\n' > .fl/live.json\nprintf '%s\\n' '{\"result\":\"touched\"}'\n";
        std::fs::write(&stub, script).unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let a = ClaudeAdapter::new(stub.to_string_lossy().into_owned());
        let out = a.attempt(spec(root, 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
        assert_eq!(out.output_excerpt, "touched");
        assert_eq!(out.paths_touched, vec!["src/lib.rs".to_owned()]);
    }
}
