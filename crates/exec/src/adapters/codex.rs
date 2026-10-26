//! The OpenAI Codex CLI adapter.
//!
//! Runs `codex exec --json --skip-git-repo-check -C <project_root> <instruction>`
//! and parses its JSONL event stream (`item.completed` and `turn.completed`).

use crate::adapters::harness::{
    CliDialect, LivenessSink, ParsedOutput, ProcessHarness, extract_text_from_object,
    extract_usage_from_value,
};
use crate::runner::{AttemptError, AttemptOutcome, AttemptSpec, Runner};
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::telemetry::{AdapterKind, RateCard};
use tokio::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct CodexDialect;

impl CliDialect for CodexDialect {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Codex
    }

    fn default_binary(&self) -> &'static str {
        "codex"
    }

    fn argv(&self, spec: &AttemptSpec) -> Vec<String> {
        vec![
            "exec".into(),
            "--json".into(),
            "--skip-git-repo-check".into(),
            "-C".into(),
            spec.project_root.to_string_lossy().into_owned(),
            spec.instruction.clone(),
        ]
    }

    fn parse_output(&self, stdout: &[u8], stderr: &[u8]) -> ParsedOutput {
        let stdout_str = String::from_utf8_lossy(stdout);
        let stderr_str = String::from_utf8_lossy(stderr);
        let trimmed = stdout_str.trim();

        if trimmed.is_empty() {
            return ParsedOutput {
                excerpt_text: stderr_str.into_owned(),
                tokens_in: 0,
                tokens_out: 0,
                vendor_cost_usd_micros: None,
            };
        }

        let mut any_json_obj = false;
        let mut tokens_in = 0u64;
        let mut tokens_out = 0u64;
        let mut vendor_cost = None;
        let mut text_parts: Vec<String> = Vec::new();

        for line in trimmed.lines() {
            let line_trim = line.trim();
            if line_trim.is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line_trim)
                && val.is_object()
            {
                any_json_obj = true;
                extract_usage_from_value(
                    &val,
                    &mut tokens_in,
                    &mut tokens_out,
                    &mut vendor_cost,
                    true,
                );
                if let Some(t) = extract_text_from_object(&val)
                    && !t.is_empty()
                {
                    text_parts.push(t);
                }
            }
        }

        if any_json_obj {
            let mut excerpt_text = if text_parts.is_empty() {
                stdout_str.into_owned()
            } else {
                text_parts.join("\n")
            };
            if !stderr_str.is_empty() {
                if !excerpt_text.is_empty() && !excerpt_text.ends_with('\n') {
                    excerpt_text.push('\n');
                }
                excerpt_text.push_str(&stderr_str);
            }
            return ParsedOutput {
                excerpt_text,
                tokens_in,
                tokens_out,
                vendor_cost_usd_micros: vendor_cost,
            };
        }

        let mut raw = stdout_str.into_owned();
        raw.push_str(&stderr_str);
        ParsedOutput {
            excerpt_text: raw,
            tokens_in: 0,
            tokens_out: 0,
            vendor_cost_usd_micros: None,
        }
    }
}

/// Drives the `codex` CLI as a [`Runner`].
pub struct CodexAdapter {
    harness: ProcessHarness<CodexDialect>,
}

impl CodexAdapter {
    pub fn new(binary: String) -> Self {
        Self {
            harness: ProcessHarness::new(binary, CodexDialect),
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

impl Runner for CodexAdapter {
    fn id(&self) -> &str {
        "codex"
    }

    async fn attempt(&self, spec: AttemptSpec) -> Result<AttemptOutcome, AttemptError> {
        Ok(self.harness.run(spec).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn dialect_constructs_expected_argv() {
        let d = tempfile::tempdir().unwrap();
        let s = spec(d.path(), 10);
        let argv = CodexDialect.argv(&s);
        assert_eq!(
            argv,
            vec![
                "exec".to_string(),
                "--json".to_string(),
                "--skip-git-repo-check".to_string(),
                "-C".to_string(),
                d.path().to_string_lossy().into_owned(),
                "say hello".to_string(),
            ]
        );
        assert_eq!(CodexDialect.kind(), AdapterKind::Codex);
        assert_eq!(CodexDialect.default_binary(), "codex");
        assert_eq!(CodexAdapter::new("codex".into()).id(), "codex");
    }

    #[tokio::test]
    async fn parses_codex_jsonl_turns_and_calculates_rate_card_cost() {
        let d = tempfile::tempdir().unwrap();
        let stub = d.path().join("codex-jsonl.sh");
        let script = r#"#!/bin/sh
cat <<'EOF'
{"type":"thread.started","thread_id":"th_1"}
{"type":"item.completed","item":{"id":"it_1","type":"agent_message","text":"first step done"}}
{"type":"turn.completed","usage":{"input_tokens":400,"cache_read_input_tokens":100,"output_tokens":150}}
{"type":"item.completed","item":{"id":"it_2","type":"agent_message","text":"final answer"}}
{"type":"turn.completed","usage":{"input_tokens":500,"output_tokens":50}}
EOF
"#;
        std::fs::write(&stub, script).unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let a =
            CodexAdapter::new(stub.to_string_lossy().into_owned()).with_rate_card(Some(RateCard {
                input_usd_micros_per_mtok: 2_000_000,
                output_usd_micros_per_mtok: 8_000_000,
            }));
        let out = a.attempt(spec(d.path(), 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
        assert_eq!(out.output_excerpt, "first step done\nfinal answer");
        // (400 + 100) + 500 = 1000 tokens_in; 150 + 50 = 200 tokens_out
        assert_eq!(out.tokens_in, 1000);
        assert_eq!(out.tokens_out, 200);
        // 1000 * 2 + 200 * 8 = 2000 + 1600 = 3600 micro-USD
        assert_eq!(out.cost_usd_micros, 3600);
    }

    #[tokio::test]
    async fn missing_binary_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let a = CodexAdapter::new("definitely-missing-codex-binary-xyz".into());
        let out = a.attempt(spec(d.path(), 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Refused);
        assert!(
            out.output_excerpt
                .contains("definitely-missing-codex-binary-xyz")
        );
    }

    #[tokio::test]
    async fn zero_budget_is_refused_before_spawn() {
        let d = tempfile::tempdir().unwrap();
        let a = CodexAdapter::new("codex".into());
        let mut s = spec(d.path(), 30);
        s.budget_usd_micros = 0;
        let out = a.attempt(s).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Refused);
        assert_eq!(out.duration_ms, 0);
    }

    #[tokio::test]
    async fn timeout_kills_child_and_reports_timeout() {
        let d = tempfile::tempdir().unwrap();
        let stub = d.path().join("codex-hang.sh");
        std::fs::write(&stub, "#!/bin/sh\nsleep 30\n").unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let a = CodexAdapter::new(stub.to_string_lossy().into_owned());
        let out = a.attempt(spec(d.path(), 1)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Timeout);
        assert!(out.duration_ms >= 1000, "got {}ms", out.duration_ms);
    }
}
