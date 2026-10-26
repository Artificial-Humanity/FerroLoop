//! The Meta Muse CLI adapter.

use crate::adapters::harness::{
    CliDialect, LivenessSink, ParsedOutput, ProcessHarness, parse_json_or_jsonl_envelope,
};
use crate::runner::{AttemptError, AttemptOutcome, AttemptSpec, Runner};
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::telemetry::{AdapterKind, RateCard};
use tokio::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct MuseDialect;

impl CliDialect for MuseDialect {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Muse
    }

    fn default_binary(&self) -> &'static str {
        "muse"
    }

    fn argv(&self, spec: &AttemptSpec) -> Vec<String> {
        vec!["exec".into(), "--json".into(), spec.instruction.clone()]
    }

    fn parse_output(&self, stdout: &[u8], stderr: &[u8]) -> ParsedOutput {
        parse_json_or_jsonl_envelope(stdout, stderr, true)
    }
}

/// Drives the `muse` CLI as a [`Runner`].
pub struct MuseAdapter {
    harness: ProcessHarness<MuseDialect>,
}

impl MuseAdapter {
    pub fn new(binary: String) -> Self {
        Self {
            harness: ProcessHarness::new(binary, MuseDialect),
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

impl Runner for MuseAdapter {
    fn id(&self) -> &str {
        "muse"
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
        assert_eq!(MuseDialect.kind(), AdapterKind::Muse);
        assert_eq!(MuseDialect.default_binary(), "muse");
        assert_eq!(MuseDialect.argv(&s), vec!["exec", "--json", "say hello"]);
        assert_eq!(MuseAdapter::new("muse".into()).id(), "muse");
    }

    #[tokio::test]
    async fn parses_jsonl_output_and_applies_rate_card_fallback() {
        let d = tempfile::tempdir().unwrap();
        let stub = d.path().join("muse-jsonl.sh");
        let script = r#"#!/bin/sh
cat <<'EOF'
{"text":"muse step 1","usage":{"input_tokens":300,"output_tokens":100}}
{"text":"muse step 2","usage":{"input_tokens":700,"output_tokens":200}}
EOF
"#;
        std::fs::write(&stub, script).unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let a =
            MuseAdapter::new(stub.to_string_lossy().into_owned()).with_rate_card(Some(RateCard {
                input_usd_micros_per_mtok: 1_000_000,
                output_usd_micros_per_mtok: 4_000_000,
            }));
        let out = a.attempt(spec(d.path(), 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
        assert_eq!(out.output_excerpt, "muse step 1\nmuse step 2");
        assert_eq!(out.tokens_in, 1000);
        assert_eq!(out.tokens_out, 300);
        // 1000 * 1 + 300 * 4 = 1000 + 1200 = 2200 micro-USD
        assert_eq!(out.cost_usd_micros, 2200);
    }

    #[tokio::test]
    async fn zero_budget_is_refused_before_spawn() {
        let d = tempfile::tempdir().unwrap();
        let a = MuseAdapter::new("muse".into());
        let mut s = spec(d.path(), 30);
        s.budget_usd_micros = 0;
        let out = a.attempt(s).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Refused);
        assert_eq!(out.duration_ms, 0);
    }

    #[tokio::test]
    async fn missing_binary_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let a = MuseAdapter::new("missing-muse-binary-999".into());
        let out = a.attempt(spec(d.path(), 5)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Refused);
        assert!(out.output_excerpt.contains("missing-muse-binary-999"));
    }
}
