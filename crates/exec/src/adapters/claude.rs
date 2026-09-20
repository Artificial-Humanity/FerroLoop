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
//! - `--max-budget-usd <amount>` — "Maximum dollar amount to spend on API
//!   calls (only works with --print)". This adapter derives it from
//!   `AttemptSpec::budget_usd_micros` and passes it through so the vendor
//!   binary enforces the same ceiling this adapter already refuses on at
//!   zero — defense in depth, not a substitute for the pre-flight check.
//!
//! This module does **not** parse `claude`'s cost or token output. That is
//! Task 19's open decision (parse the vendor's local usage logs, or take
//! only what the CLI reports); `AttemptOutcome::tokens_in`,
//! `tokens_out`, and `cost_usd_micros` stay `0` here deliberately. A
//! fabricated number would be worse than a missing one.

use crate::runner::{AttemptError, AttemptOutcome, AttemptSpec, Runner};
use fl_core::log::AttemptStatus;
use std::process::Stdio;
use std::time::Instant;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

/// Output beyond this many bytes is dropped rather than stored. An attempt
/// record is a note for a human and a later gate, not a full transcript.
const EXCERPT_LIMIT: usize = 8192;

/// Drives the real `claude` CLI as a [`Runner`].
pub struct ClaudeAdapter {
    binary: String,
}

impl ClaudeAdapter {
    pub fn new(binary: String) -> Self {
        Self { binary }
    }
}

impl Runner for ClaudeAdapter {
    fn id(&self) -> &str {
        "claude"
    }

    async fn attempt(&self, spec: AttemptSpec) -> Result<AttemptOutcome, AttemptError> {
        // A zero ceiling is refused before anything is spawned. Pre-flight
        // fails fast on budget exhaustion rather than letting a process run
        // that this side has already decided not to pay for.
        if spec.budget_usd_micros == 0 {
            return Ok(AttemptOutcome::refused(
                "the attempt's spend ceiling is zero, so nothing was started",
            ));
        }

        let budget_usd = spec.budget_usd_micros as f64 / 1_000_000.0;

        let started = Instant::now();
        let mut cmd = Command::new(&self.binary);
        cmd.arg("-p")
            .arg(&spec.instruction)
            .arg("--max-budget-usd")
            .arg(format!("{budget_usd:.6}"))
            .current_dir(&spec.project_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // A spawn failure is not a pre-flight error: the caller asked
                // for an attempt, and "the binary is missing" is an answer
                // about that attempt, not a reason to never have tried. It
                // costs nothing and took no time, so it is reported the same
                // way a zero-budget refusal is.
                return Ok(AttemptOutcome::refused(format!(
                    "could not start `{}`: {e}",
                    self.binary
                )));
            }
        };

        let limit = Duration::from_secs(spec.timeout_secs.max(1));
        let waited = timeout(limit, child.wait_with_output()).await;
        let duration_ms = started.elapsed().as_millis() as u64;

        match waited {
            Err(_elapsed) => Ok(AttemptOutcome {
                status: AttemptStatus::Timeout,
                output_excerpt: format!(
                    "`{}` exceeded its {}s timeout",
                    self.binary, spec.timeout_secs
                ),
                duration_ms,
                tokens_in: 0,
                tokens_out: 0,
                cost_usd_micros: 0,
                paths_touched: vec![],
            }),
            Ok(Err(e)) => Ok(AttemptOutcome {
                status: AttemptStatus::Crashed,
                output_excerpt: e.to_string(),
                duration_ms,
                tokens_in: 0,
                tokens_out: 0,
                cost_usd_micros: 0,
                paths_touched: vec![],
            }),
            Ok(Ok(out)) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                // Truncate on a `char` boundary, not a raw byte offset:
                // `String::truncate` panics if the cut point lands inside a
                // multi-byte UTF-8 sequence, and `String::from_utf8_lossy`
                // gives no guarantee that EXCERPT_LIMIT falls on one. Same
                // rule as `command.rs`'s `excerpt()`.
                if text.len() > EXCERPT_LIMIT {
                    let mut end = EXCERPT_LIMIT;
                    while end > 0 && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                let status = if out.status.success() {
                    AttemptStatus::Completed
                } else {
                    AttemptStatus::Crashed
                };
                Ok(AttemptOutcome {
                    status,
                    output_excerpt: text,
                    duration_ms,
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_usd_micros: 0,
                    paths_touched: vec![],
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ids::RecordId;

    fn spec(root: &std::path::Path, timeout: u64) -> AttemptSpec {
        AttemptSpec {
            project_root: root.to_path_buf(),
            record: RecordId(1),
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
        assert!(out.output_excerpt.contains("definitely-not-a-real-program-9f3x"));
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
}
