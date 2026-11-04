#![cfg(unix)]
use base64::Engine as _;
use fl_hub::{
    discovery::{DiscoveredSessionSummary, QuotaWindowUsage, SessionQuotaUsage},
    runtime_matrix::{ProbeInputs, QuotaSource, build_matrix},
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tempfile::TempDir;

#[test]
fn codex_matrix_account_is_bound_to_credential_home() {
    const CHILD: &str = "FL_TEST_CODEX_MATRIX_ACCOUNT_CHILD";
    const EXPECTED: &str = "FL_TEST_CODEX_MATRIX_EXPECTED";
    if let Ok(rollout) = std::env::var(CHILD) {
        let mode = std::env::var(EXPECTED).unwrap();
        let session = DiscoveredSessionSummary {
            id: "session-a".into(),
            source: "route".into(),
            adapter: "codex".into(),
            persona: None,
            status: "active".into(),
            updated_at: Some("2026-11-01T10:00:00Z".into()),
            cwd: None,
            record_id: None,
            transcript_path: Some(rollout),
            quota_usage: Some(SessionQuotaUsage {
                tier: Some("plus".into()),
                primary_window: Some(QuotaWindowUsage {
                    used_percent: 7.0,
                    window_minutes: Some(300),
                    resets_at_ms: None,
                }),
                secondary_window: None,
                exhausted_reason: None,
            }),
            model: None,
            thinking_level: None,
            available_models: vec![],
            context_usage: None,
            execution_mode: None,
        };
        let inputs = ProbeInputs {
            path_env: String::new(),
            home: None,
            sessions: vec![session],
            vault: Ok(vec![]),
            local_engines: vec![],
            now_ms: 1_800_000_000_000,
            home_override: (mode == "override")
                .then(|| PathBuf::from(std::env::var_os("HOME").unwrap())),
            quota_source: QuotaSource::HOST,
            quota_budget: Duration::from_secs(3),
            quota_cache: Arc::default(),
        };
        let matrix = build_matrix(&inputs, Some("codex")).unwrap();
        if mode == "switch" {
            let first = &matrix.vendor_quotas["codex"];
            assert_eq!(first.tier.as_deref(), Some("pro"));
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                serde_json::json!({"https://api.openai.com/auth":{"chatgpt_plan_type":"enterprise"}}).to_string(),
            );
            let auth = PathBuf::from(std::env::var_os("CODEX_HOME").unwrap()).join("auth.json");
            std::fs::write(
                auth,
                serde_json::json!({"tokens":{"id_token":format!("e30.{payload}.sig")}}).to_string(),
            )
            .unwrap();
            let updated = build_matrix(&inputs, Some("codex")).unwrap();
            let current = &updated.vendor_quotas["codex"];
            assert_eq!(current.tier.as_deref(), Some("enterprise"));
            assert!(current.primary_window.is_none());
            return;
        }
        match mode.as_str() {
            "same" | "override" => {
                let reading = &matrix.vendor_quotas["codex"];
                assert_eq!(reading.tier.as_deref(), Some("pro"));
                assert!(reading.primary_window.is_none());
            }
            "other" => {
                let reading = &matrix.vendor_quotas["codex"];
                assert_eq!(reading.tier.as_deref(), Some("enterprise"));
                assert!(reading.primary_window.is_none());
            }
            "missing" => assert!(!matrix.vendor_quotas.contains_key("codex")),
            _ => panic!("unexpected test mode"),
        }
        return;
    }
    let account_a = TempDir::new().unwrap();
    let account_b = TempDir::new().unwrap();
    std::fs::create_dir_all(account_a.path().join(".codex/sessions")).unwrap();
    std::fs::create_dir_all(account_b.path().join(".codex")).unwrap();
    let rollout = account_a.path().join(".codex/sessions/rollout.jsonl");
    std::fs::write(&rollout, "{}\n").unwrap();
    for (home, plan) in [(account_a.path(), "pro"), (account_b.path(), "enterprise")] {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({"https://api.openai.com/auth":{"chatgpt_plan_type":plan}})
                .to_string(),
        );
        std::fs::write(
            home.join(".codex/auth.json"),
            serde_json::json!({"tokens":{"id_token":format!("e30.{payload}.sig")}}).to_string(),
        )
        .unwrap();
    }
    for (mode, configured) in [
        ("same", account_a.path().join(".codex")),
        ("other", account_b.path().join(".codex")),
        ("override", account_b.path().join(".codex")),
        ("missing", account_b.path().join("missing")),
        ("switch", account_a.path().join(".codex")),
    ] {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "codex_matrix_account_is_bound_to_credential_home",
            ])
            .env(CHILD, &rollout)
            .env(EXPECTED, mode)
            .env("HOME", account_a.path())
            .env("CODEX_HOME", configured)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
}
