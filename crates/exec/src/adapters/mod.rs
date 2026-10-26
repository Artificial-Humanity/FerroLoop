pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod grok;
pub mod harness;
pub mod muse;

pub use antigravity::AntigravityAdapter;
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use grok::GrokAdapter;
pub use harness::{CliDialect, GitSnapshot, LivenessSink, ParsedOutput, ProcessHarness};
pub use muse::MuseAdapter;

use crate::runner::{AttemptError, AttemptOutcome, AttemptSpec, Runner};
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::telemetry::{AdapterKind, RateCard};

pub enum AnyAdapter {
    Claude(ClaudeAdapter),
    Codex(CodexAdapter),
    Antigravity(AntigravityAdapter),
    Muse(MuseAdapter),
    Grok(GrokAdapter),
}

impl AnyAdapter {
    pub fn new(
        kind: AdapterKind,
        binary_override: Option<String>,
        rate_card: Option<RateCard>,
    ) -> Self {
        let bin = binary_override.unwrap_or_else(|| kind.default_binary().to_owned());
        match kind {
            AdapterKind::Claude => Self::Claude(ClaudeAdapter::new(bin).with_rate_card(rate_card)),
            AdapterKind::Codex => Self::Codex(CodexAdapter::new(bin).with_rate_card(rate_card)),
            AdapterKind::Antigravity => {
                Self::Antigravity(AntigravityAdapter::new(bin).with_rate_card(rate_card))
            }
            AdapterKind::Muse => Self::Muse(MuseAdapter::new(bin).with_rate_card(rate_card)),
            AdapterKind::Grok => Self::Grok(GrokAdapter::new(bin).with_rate_card(rate_card)),
        }
    }

    pub fn with_liveness(
        self,
        attempt_id: Iri,
        project: ProjectId,
        agent: Option<String>,
        sink: LivenessSink,
    ) -> Self {
        match self {
            Self::Claude(a) => Self::Claude(a.with_liveness(attempt_id, project, agent, sink)),
            Self::Codex(a) => Self::Codex(a.with_liveness(attempt_id, project, agent, sink)),
            Self::Antigravity(a) => {
                Self::Antigravity(a.with_liveness(attempt_id, project, agent, sink))
            }
            Self::Muse(a) => Self::Muse(a.with_liveness(attempt_id, project, agent, sink)),
            Self::Grok(a) => Self::Grok(a.with_liveness(attempt_id, project, agent, sink)),
        }
    }
}

impl Runner for AnyAdapter {
    fn id(&self) -> &str {
        match self {
            Self::Claude(a) => a.id(),
            Self::Codex(a) => a.id(),
            Self::Antigravity(a) => a.id(),
            Self::Muse(a) => a.id(),
            Self::Grok(a) => a.id(),
        }
    }

    async fn attempt(&self, spec: AttemptSpec) -> Result<AttemptOutcome, AttemptError> {
        match self {
            Self::Claude(a) => a.attempt(spec).await,
            Self::Codex(a) => a.attempt(spec).await,
            Self::Antigravity(a) => a.attempt(spec).await,
            Self::Muse(a) => a.attempt(spec).await,
            Self::Grok(a) => a.attempt(spec).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ids::{RecordId, seq_iri};
    use fl_core::log::AttemptStatus;
    use fl_core::telemetry::LivenessStatus;
    use std::sync::{Arc, Mutex};

    #[test]
    fn any_adapter_dispatches_id_for_all_five_kinds() {
        let cases = [
            (AdapterKind::Claude, "claude"),
            (AdapterKind::Codex, "codex"),
            (AdapterKind::Antigravity, "antigravity"),
            (AdapterKind::Muse, "muse"),
            (AdapterKind::Grok, "grok"),
        ];
        for (kind, expected_id) in cases {
            let adapter = AnyAdapter::new(kind, None, None);
            assert_eq!(adapter.id(), expected_id);
        }
    }

    #[tokio::test]
    async fn any_adapter_runs_attempt_with_liveness_and_rate_card() {
        let d = tempfile::tempdir().unwrap();
        let stub = d.path().join("any-codex.sh");
        let script = r#"#!/bin/sh
printf '%s\n' '{"type":"item.completed","item":{"text":"from any adapter"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":500,"output_tokens":100}}'
"#;
        std::fs::write(&stub, script).unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let beats = Arc::new(Mutex::new(Vec::new()));
        let beats_clone = Arc::clone(&beats);
        let sink: LivenessSink = Arc::new(move |hb| {
            beats_clone.lock().unwrap().push(hb.clone());
        });

        let adapter = AnyAdapter::new(
            AdapterKind::Codex,
            Some(stub.to_string_lossy().into_owned()),
            Some(RateCard {
                input_usd_micros_per_mtok: 2_000_000,
                output_usd_micros_per_mtok: 6_000_000,
            }),
        )
        .with_liveness(
            seq_iri(10),
            ProjectId(seq_iri(20)),
            Some("ferris".into()),
            sink,
        );

        let spec = AttemptSpec {
            project_root: d.path().to_path_buf(),
            record: RecordId(seq_iri(1)),
            instruction: "run test".into(),
            timeout_secs: 5,
            budget_usd_micros: 1_000_000,
        };
        let out = adapter.attempt(spec).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Completed);
        assert_eq!(out.output_excerpt, "from any adapter");
        assert_eq!(out.tokens_in, 500);
        assert_eq!(out.tokens_out, 100);
        assert_eq!(out.cost_usd_micros, 1600);

        let recorded = beats.lock().unwrap();
        assert!(recorded.len() >= 2);
        let last = recorded.last().unwrap();
        assert_eq!(last.adapter, "codex");
        assert_eq!(last.agent.as_deref(), Some("ferris"));
        assert_eq!(last.status, LivenessStatus::Exited);
        assert_eq!(last.outcome, Some(AttemptStatus::Completed));
    }
}
