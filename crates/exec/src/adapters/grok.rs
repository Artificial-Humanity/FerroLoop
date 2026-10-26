//! The xAI Grok CLI adapter.

use crate::adapters::harness::{
    CliDialect, LivenessSink, ParsedOutput, ProcessHarness, parse_json_or_jsonl_envelope,
};
use crate::runner::{AttemptError, AttemptOutcome, AttemptSpec, Runner};
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::telemetry::{AdapterKind, RateCard};
use tokio::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct GrokDialect;

impl CliDialect for GrokDialect {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Grok
    }

    fn default_binary(&self) -> &'static str {
        "grok"
    }

    fn argv(&self, spec: &AttemptSpec) -> Vec<String> {
        vec![
            "-p".into(),
            spec.instruction.clone(),
            "--output-format".into(),
            "json".into(),
        ]
    }

    fn parse_output(&self, stdout: &[u8], stderr: &[u8]) -> ParsedOutput {
        parse_json_or_jsonl_envelope(stdout, stderr, false)
    }
}

/// Drives the `grok` CLI as a [`Runner`].
pub struct GrokAdapter {
    harness: ProcessHarness<GrokDialect>,
}

impl GrokAdapter {
    pub fn new(binary: String) -> Self {
        Self {
            harness: ProcessHarness::new(binary, GrokDialect),
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

impl Runner for GrokAdapter {
    fn id(&self) -> &str {
        "grok"
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
    fn dialect_metadata_and_argv() {
        let d = tempfile::tempdir().unwrap();
        let s = spec(d.path(), 5);
        assert_eq!(GrokDialect.kind(), AdapterKind::Grok);
        assert_eq!(GrokDialect.default_binary(), "grok");
        assert_eq!(
            GrokDialect.argv(&s),
            vec!["-p", "say hello", "--output-format", "json"]
        );
        assert_eq!(GrokAdapter::new("grok".into()).id(), "grok");
    }

    #[tokio::test]
    async fn parses_openai_compatible_choices_json_and_rate_card_fallback() {
        let d = tempfile::tempdir().unwrap();
        let stub = d.path().join("grok-json.sh");
        let payload = r#"{"choices":[{"message":{"role":"assistant","content":"grok reply"}}],"usage":{"prompt_tokens":800,"completion_tokens":200}}"#;
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{payload}'\n");
        std::fs::write(&stub, script).unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let a =
            GrokAdapter::new(stub.to_string_lossy().into_owned()).with_rate_card(Some(RateCard {
                input_usd_micros_per_mtok: 2_000_000,
                output_usd_micros_per_mtok: 10_000_000,
            }));
        let out = a.attempt(spec(d.path(), 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
        assert_eq!(out.output_excerpt, "grok reply");
        assert_eq!(out.tokens_in, 800);
        assert_eq!(out.tokens_out, 200);
        // 800 * 2 + 200 * 10 = 1600 + 2000 = 3600 micro-USD
        assert_eq!(out.cost_usd_micros, 3600);
    }

    #[tokio::test]
    async fn zero_budget_is_refused_before_spawn() {
        let d = tempfile::tempdir().unwrap();
        let a = GrokAdapter::new("grok".into());
        let mut s = spec(d.path(), 30);
        s.budget_usd_micros = 0;
        let out = a.attempt(s).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Refused);
        assert_eq!(out.duration_ms, 0);
    }

    #[tokio::test]
    async fn missing_binary_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let a = GrokAdapter::new("missing-grok-binary-999".into());
        let out = a.attempt(spec(d.path(), 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Refused);
        assert!(out.output_excerpt.contains("missing-grok-binary-999"));
    }
}
