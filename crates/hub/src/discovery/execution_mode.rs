//! Execution (permission) mode of a discovered vendor session. Strictly read-only: each reader
//! looks at a transcript or config file the vendor wrote and never changes one.
//!
//! The vocabulary is `auto`, `yolo`, `plan`, `default` and `accept-edits`. A vendor value this
//! module has no name for passes through lower-cased; a mode is never invented, and a vendor
//! whose files state none reports `None`.
//!
//! | Vendor      | Source (first that states a mode wins)                                              |
//! |-------------|-------------------------------------------------------------------------------------|
//! | Claude Code | newest `permissionMode` in the transcript (`permission-mode` records, user turns), |
//! |             | else `~/.claude/settings.json` `permissions.defaultMode`                            |
//! | Codex       | newest `turn_context` (else `session_meta`) `collaboration_mode.mode == plan`, or   |
//! |             | `approval_policy` + `sandbox_policy.type`; else `~/.codex/config.toml`              |
//! |             | `approval_policy` + `sandbox_mode`                                                  |
//! | Grok        | `~/.grok/config.toml` `[ui] yolo = true`, else `[ui] permission_mode`               |
//! | Antigravity | `~/.gemini/antigravity-cli/settings.json` `toolPermission`                          |
//! | Muse        | `~/.config/muse/settings.json` `permissions.default_profile`                        |

use super::*;

/// Longest accepted vendor mode value.
const EXECUTION_MODE_MAX_CHARS: usize = 64;

/// Canonical name for a vendor mode value. `bypassPermissions` and `yolo` are `yolo`;
/// `acceptEdits` is `accept-edits`; `auto`, `plan` and `default` keep their names; any other
/// plain value passes through lower-cased. Empty, over-long and control-character values are
/// `None`.
pub(super) fn normalize_execution_mode(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.is_empty()
        || lower.chars().count() > EXECUTION_MODE_MAX_CHARS
        || lower.chars().any(char::is_control)
    {
        return None;
    }
    Some(match lower.as_str() {
        "bypasspermissions" | "bypass-permissions" | "yolo" => "yolo".to_string(),
        "acceptedits" | "accept-edits" | "accept_edits" => "accept-edits".to_string(),
        _ => lower,
    })
}

/// The effective mode of one route session, read from its transcript (through `files`, so the
/// caller's other reads share the file) and the vendor's config under `home`.
pub(super) fn session_execution_mode(
    adapter: &str,
    transcript_path: Option<&str>,
    home: Option<&Path>,
    files: &mut TranscriptReads,
) -> Option<String> {
    match adapter {
        "claude" => claude_execution_mode(transcript_path.and_then(|p| files.raw(p)), home),
        "codex" => codex_execution_mode(transcript_path.and_then(|p| files.raw(p)), home),
        "grok" => grok_execution_mode(home),
        "antigravity" => antigravity_execution_mode(home),
        "muse" => muse_execution_mode(home),
        _ => None,
    }
}

// --- Claude Code ------------------------------------------------------------

/// Newest `permissionMode` in Claude transcript JSONL. Claude writes a
/// `{"type":"permission-mode","permissionMode":...}` record when the mode starts or changes,
/// and stamps `permissionMode` on each user turn. Subagent (sidechain) lines are skipped.
fn claude_transcript_mode(jsonl: &str) -> Option<String> {
    jsonl.lines().rev().find_map(|line| {
        if !line.contains("\"permissionMode\"") {
            return None;
        }
        let val: Value = serde_json::from_str(line.trim()).ok()?;
        if val.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        normalize_execution_mode(val.get("permissionMode")?.as_str()?)
    })
}

fn claude_execution_mode(transcript: Option<&str>, home: Option<&Path>) -> Option<String> {
    transcript.and_then(claude_transcript_mode).or_else(|| {
        let settings = read_json_file(&home?.join(".claude").join("settings.json"))?;
        normalize_execution_mode(settings.pointer("/permissions/defaultMode")?.as_str()?)
    })
}

// --- Codex ------------------------------------------------------------------

/// The mode for a Codex `approval_policy` and sandbox (`sandbox_policy.type` in a rollout,
/// `sandbox_mode` in `config.toml`; the spellings match).
///
/// `never` approval with `danger-full-access` is `yolo` (Codex's "bypass approvals and
/// sandbox"). `full-auto` is `auto`. `on-request`, `on-failure` and `untrusted` ask the user,
/// so they are `default`. Any other approval value passes through. A sandbox with no
/// approval policy states no mode.
fn codex_mode_from(approval: Option<&str>, sandbox: Option<&str>) -> Option<String> {
    let approval = approval?.trim().to_ascii_lowercase();
    let sandbox = sandbox.map(|s| s.trim().to_ascii_lowercase());
    match approval.as_str() {
        "never" if sandbox.as_deref() == Some("danger-full-access") => Some("yolo".to_string()),
        "full-auto" => Some("auto".to_string()),
        "on-request" | "on-failure" | "untrusted" => Some("default".to_string()),
        other => normalize_execution_mode(other),
    }
}

/// Mode from the newest rollout line that states one. `collaboration_mode.mode` `plan` wins
/// over the approval and sandbox pair.
fn codex_rollout_mode(jsonl: &str) -> Option<String> {
    jsonl.lines().rev().find_map(|line| {
        if !line.contains("approval_policy") && !line.contains("collaboration_mode") {
            return None;
        }
        let val: Value = serde_json::from_str(line.trim()).ok()?;
        if !matches!(
            val.get("type").and_then(Value::as_str),
            Some("turn_context" | "session_meta")
        ) {
            return None;
        }
        let payload = val.get("payload")?;
        if payload
            .pointer("/collaboration_mode/mode")
            .and_then(Value::as_str)
            .is_some_and(|m| m.eq_ignore_ascii_case("plan"))
        {
            return Some("plan".to_string());
        }
        let sandbox = payload
            .pointer("/sandbox_policy/type")
            .or_else(|| payload.get("sandbox_policy"))
            .or_else(|| payload.get("sandbox_mode"))
            .and_then(Value::as_str);
        codex_mode_from(payload.get("approval_policy")?.as_str(), sandbox)
    })
}

fn codex_execution_mode(transcript: Option<&str>, home: Option<&Path>) -> Option<String> {
    transcript.and_then(codex_rollout_mode).or_else(|| {
        let config = read_toml_file(&home?.join(".codex").join("config.toml"))?;
        codex_mode_from(
            toml_non_empty_str(&config, "approval_policy").as_deref(),
            toml_non_empty_str(&config, "sandbox_mode").as_deref(),
        )
    })
}

// --- Grok, Antigravity, Muse ------------------------------------------------

/// Grok's `[ui]` table: `yolo = true` is `yolo`, else `permission_mode` (for example
/// `always-approve`) passes through.
fn grok_execution_mode(home: Option<&Path>) -> Option<String> {
    let config = read_toml_file(&home?.join(".grok").join("config.toml"))?;
    let ui = config.get("ui")?.as_table()?;
    if ui.get("yolo").and_then(toml::Value::as_bool) == Some(true) {
        return Some("yolo".to_string());
    }
    normalize_execution_mode(&toml_non_empty_str(ui, "permission_mode")?)
}

/// Antigravity's `toolPermission` setting (for example `always-proceed`), passed through.
fn antigravity_execution_mode(home: Option<&Path>) -> Option<String> {
    let settings = read_json_file(
        &home?
            .join(".gemini")
            .join("antigravity-cli")
            .join("settings.json"),
    )?;
    normalize_execution_mode(settings.get("toolPermission")?.as_str()?)
}

/// Muse's default permission profile, passed through without the leading `:` Muse puts on a
/// built-in profile name (`:unrestricted` is `unrestricted`).
fn muse_execution_mode(home: Option<&Path>) -> Option<String> {
    let settings = read_json_file(&home?.join(".config").join("muse").join("settings.json"))?;
    let profile = settings.pointer("/permissions/default_profile")?.as_str()?;
    normalize_execution_mode(profile.trim().trim_start_matches(':'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(home: &Path, rel: &str, body: &str) -> PathBuf {
        let path = home.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn lines(values: &[serde_json::Value]) -> String {
        values
            .iter()
            .map(|v| format!("{v}\n"))
            .collect::<Vec<_>>()
            .join("")
    }

    fn write_route(home: &Path, persona: &str, route: &str) {
        write(
            home,
            ".cache/agent-bus/routes.json",
            &format!(
                r#"{{"entries":{{"{persona}":{{"persona":"{persona}","updated_at":"2026-10-29T00:00:00.000Z","route":{route}}}}}}}"#
            ),
        );
    }

    fn discover(home: &Path, with_model_info: bool) -> DiscoveredSessionSummary {
        let snap = DiscoveryStoreSnapshot::default();
        let sessions = if with_model_info {
            discover_sessions_from_snapshot(&snap, Some(home))
        } else {
            discover_route_sessions_without_model_info(&snap, Some(home))
        };
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        sessions.into_iter().next().unwrap()
    }

    #[test]
    fn normalization_maps_the_vocabulary_and_passes_unknown_values_through() {
        for (raw, want) in [
            ("bypassPermissions", Some("yolo")),
            ("YOLO", Some("yolo")),
            ("acceptEdits", Some("accept-edits")),
            ("accept-edits", Some("accept-edits")),
            ("auto", Some("auto")),
            ("Plan", Some("plan")),
            ("default", Some("default")),
            (" dontAsk ", Some("dontask")),
            ("always-approve", Some("always-approve")),
            ("", None),
            ("   ", None),
            ("bad\nvalue", None),
        ] {
            assert_eq!(normalize_execution_mode(raw).as_deref(), want, "{raw:?}");
        }
        assert_eq!(normalize_execution_mode(&"x".repeat(65)), None);
        assert!(normalize_execution_mode(&"x".repeat(64)).is_some());
    }

    #[test]
    fn claude_newest_transcript_record_wins_over_older_ones_and_settings() {
        let home = TempDir::new().unwrap();
        write(
            home.path(),
            ".claude/settings.json",
            r#"{"permissions":{"defaultMode":"plan"}}"#,
        );
        let jsonl = lines(&[
            serde_json::json!({"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s"}),
            serde_json::json!({"type":"user","permissionMode":"acceptEdits","message":{"content":"hi"}}),
            serde_json::json!({"type":"assistant","message":{"content":"ok"}}),
        ]);
        // The user turn is newer than the permission-mode record.
        assert_eq!(
            claude_execution_mode(Some(&jsonl), Some(home.path())).as_deref(),
            Some("accept-edits")
        );
        let changed = format!(
            "{jsonl}{}",
            lines(&[serde_json::json!({"type":"permission-mode","permissionMode":"auto"})])
        );
        assert_eq!(
            claude_execution_mode(Some(&changed), Some(home.path())).as_deref(),
            Some("auto")
        );
        // A subagent line never decides the session's mode.
        let sidechain = format!(
            "{jsonl}{}",
            lines(&[serde_json::json!({"type":"user","isSidechain":true,"permissionMode":"plan"})])
        );
        assert_eq!(
            claude_execution_mode(Some(&sidechain), Some(home.path())).as_deref(),
            Some("accept-edits")
        );
    }

    #[test]
    fn claude_falls_back_to_settings_default_mode_and_then_to_none() {
        let home = TempDir::new().unwrap();
        let no_mode = lines(&[serde_json::json!({"type":"assistant","message":{"content":"x"}})]);
        assert_eq!(
            claude_execution_mode(Some(&no_mode), Some(home.path())),
            None
        );
        for (setting, want) in [
            ("bypassPermissions", "yolo"),
            ("acceptEdits", "accept-edits"),
            ("auto", "auto"),
            ("plan", "plan"),
            ("default", "default"),
            ("dontAsk", "dontask"),
        ] {
            write(
                home.path(),
                ".claude/settings.json",
                &format!(r#"{{"permissions":{{"defaultMode":"{setting}"}}}}"#),
            );
            assert_eq!(
                claude_execution_mode(Some(&no_mode), Some(home.path())).as_deref(),
                Some(want),
                "{setting}"
            );
            assert_eq!(
                claude_execution_mode(None, Some(home.path())).as_deref(),
                Some(want),
                "{setting} with no transcript"
            );
        }
        write(home.path(), ".claude/settings.json", r#"{"model":"opus"}"#);
        assert_eq!(claude_execution_mode(None, Some(home.path())), None);
    }

    fn turn_context(approval: serde_json::Value, sandbox: &str, collab: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "turn_context",
            "payload": {
                "approval_policy": approval,
                "sandbox_policy": {"type": sandbox},
                "collaboration_mode": {"mode": collab, "settings": {}}
            }
        })
    }

    #[test]
    fn codex_maps_approval_and_sandbox_pairs() {
        for (approval, sandbox, want) in [
            ("never", "danger-full-access", Some("yolo")),
            ("on-request", "workspace-write", Some("default")),
            ("on-failure", "workspace-write", Some("default")),
            ("untrusted", "read-only", Some("default")),
            ("on-request", "danger-full-access", Some("default")),
            ("full-auto", "workspace-write", Some("auto")),
            // Not in the table: the vendor's word, never a guess.
            ("never", "workspace-write", Some("never")),
            ("Granular-Thing", "read-only", Some("granular-thing")),
        ] {
            let jsonl = lines(&[turn_context(approval.into(), sandbox, "default")]);
            assert_eq!(
                codex_rollout_mode(&jsonl).as_deref(),
                want,
                "{approval} + {sandbox}"
            );
        }
        // A structured (granular) approval policy is not a name this reader can use.
        let granular = lines(&[turn_context(
            serde_json::json!({"granular": {"rules": true}}),
            "workspace-write",
            "default",
        )]);
        assert_eq!(codex_rollout_mode(&granular), None);
    }

    #[test]
    fn codex_plan_collaboration_mode_wins_and_newest_turn_context_decides() {
        let jsonl = lines(&[
            turn_context("never".into(), "danger-full-access", "default"),
            serde_json::json!({"type":"event_msg","payload":{"type":"token_count"}}),
            turn_context("never".into(), "danger-full-access", "plan"),
        ]);
        assert_eq!(codex_rollout_mode(&jsonl).as_deref(), Some("plan"));
        let back = format!(
            "{jsonl}{}",
            lines(&[turn_context(
                "on-request".into(),
                "workspace-write",
                "default"
            )])
        );
        assert_eq!(codex_rollout_mode(&back).as_deref(), Some("default"));
    }

    #[test]
    fn codex_session_meta_and_config_toml_supply_a_mode_when_no_turn_context_does() {
        let home = TempDir::new().unwrap();
        let meta = lines(&[serde_json::json!({
            "type": "session_meta",
            "payload": {"approval_policy": "never", "sandbox_policy": {"type": "danger-full-access"}}
        })]);
        assert_eq!(codex_rollout_mode(&meta).as_deref(), Some("yolo"));

        let none = lines(&[serde_json::json!({"type":"event_msg","payload":{"type":"x"}})]);
        assert_eq!(codex_execution_mode(Some(&none), Some(home.path())), None);
        write(
            home.path(),
            ".codex/config.toml",
            "approval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n",
        );
        assert_eq!(
            codex_execution_mode(Some(&none), Some(home.path())).as_deref(),
            Some("yolo")
        );
        write(
            home.path(),
            ".codex/config.toml",
            "approval_policy = \"on-request\"\nsandbox_mode = \"workspace-write\"\n",
        );
        assert_eq!(
            codex_execution_mode(None, Some(home.path())).as_deref(),
            Some("default")
        );
        // The rollout beats the config.
        assert_eq!(
            codex_execution_mode(Some(&meta), Some(home.path())).as_deref(),
            Some("yolo")
        );
        // A sandbox alone states no mode.
        write(
            home.path(),
            ".codex/config.toml",
            "sandbox_mode = \"danger-full-access\"\n",
        );
        assert_eq!(codex_execution_mode(None, Some(home.path())), None);
    }

    #[test]
    fn grok_antigravity_and_muse_report_only_what_their_config_states() {
        let home = TempDir::new().unwrap();
        for f in [
            grok_execution_mode,
            antigravity_execution_mode,
            muse_execution_mode,
        ] {
            assert_eq!(f(Some(home.path())), None, "no config file");
            assert_eq!(f(None), None, "no home");
        }

        write(
            home.path(),
            ".grok/config.toml",
            "[ui]\ncompact_mode = false\n",
        );
        assert_eq!(grok_execution_mode(Some(home.path())), None);
        write(
            home.path(),
            ".grok/config.toml",
            "[ui]\nyolo = false\npermission_mode = \"always-approve\"\n",
        );
        assert_eq!(
            grok_execution_mode(Some(home.path())).as_deref(),
            Some("always-approve")
        );
        write(
            home.path(),
            ".grok/config.toml",
            "[ui]\nyolo = true\npermission_mode = \"plan\"\n",
        );
        assert_eq!(
            grok_execution_mode(Some(home.path())).as_deref(),
            Some("yolo")
        );

        write(
            home.path(),
            ".gemini/antigravity-cli/settings.json",
            r#"{"model":"x"}"#,
        );
        assert_eq!(antigravity_execution_mode(Some(home.path())), None);
        write(
            home.path(),
            ".gemini/antigravity-cli/settings.json",
            r#"{"toolPermission":"Always-Proceed"}"#,
        );
        assert_eq!(
            antigravity_execution_mode(Some(home.path())).as_deref(),
            Some("always-proceed")
        );

        write(
            home.path(),
            ".config/muse/settings.json",
            r#"{"permissions":{"schema_version":1}}"#,
        );
        assert_eq!(muse_execution_mode(Some(home.path())), None);
        write(
            home.path(),
            ".config/muse/settings.json",
            r#"{"permissions":{"default_profile":":unrestricted"}}"#,
        );
        assert_eq!(
            muse_execution_mode(Some(home.path())).as_deref(),
            Some("unrestricted")
        );
    }

    #[test]
    fn discovery_sets_mode_and_context_with_model_info_and_omits_both_without() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let sid = "claude-mode-1";
        write(
            home,
            &format!(".claude/projects/p/{sid}.jsonl"),
            &lines(&[
                serde_json::json!({"type":"permission-mode","permissionMode":"bypassPermissions"}),
                serde_json::json!({"type":"assistant","message":{"model":"claude-opus-5-5",
                    "usage":{"input_tokens":100,"cache_read_input_tokens":50,"output_tokens":7}}}),
            ]),
        );
        write_route(
            home,
            "kayla",
            &format!(
                r#"{{"kind":"claude","pid":1,"session_id":"{sid}","socket_path":"{}"}}"#,
                home.join("c.sock").display()
            ),
        );

        let full = discover(home, true);
        assert_eq!(full.execution_mode.as_deref(), Some("yolo"));
        assert_eq!(
            full.context_usage,
            Some(SessionContextUsage {
                used_tokens: 157,
                max_tokens: 1_000_000
            })
        );

        let lookup = discover(home, false);
        assert_eq!(lookup.execution_mode, None);
        assert_eq!(lookup.context_usage, None);
        let json = serde_json::to_value(&lookup).unwrap();
        assert!(json.get("execution_mode").is_none(), "{json}");
        assert!(json.get("context_usage").is_none(), "{json}");

        let json = serde_json::to_value(&full).unwrap();
        assert_eq!(json["execution_mode"], "yolo");
        assert_eq!(json["context_usage"]["used_tokens"], 157);
        assert_eq!(json["context_usage"]["max_tokens"], 1_000_000);
    }

    #[test]
    fn discovery_reads_codex_rollout_mode_and_context_under_the_session_home() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        write(
            home,
            ".codex/sessions/2026/10/29/rollout-2026-10-29T01-39-41-codex-mode-1.jsonl",
            &lines(&[
                turn_context("never".into(), "danger-full-access", "default"),
                serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{
                    "last_token_usage":{"total_tokens":4242},"model_context_window":272000}}}),
            ]),
        );
        write_route(
            home,
            "katie",
            r#"{"kind":"codex","thread_id":"codex-mode-1"}"#,
        );
        let session = discover(home, true);
        assert_eq!(session.execution_mode.as_deref(), Some("yolo"));
        assert_eq!(
            session.context_usage,
            Some(SessionContextUsage {
                used_tokens: 4242,
                max_tokens: 272_000
            })
        );
    }

    #[test]
    fn discovery_reads_grok_antigravity_and_muse_config_mode() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        write(home, ".grok/config.toml", "[ui]\nyolo = true\n");
        write(
            home,
            ".grok/sessions/grok-mode-1/chat_history.jsonl",
            "{}\n",
        );
        write(
            home,
            ".grok/sessions/grok-mode-1/signals.json",
            r#"{"contextTokensUsed":900,"contextWindowTokens":2000}"#,
        );
        write_route(
            home,
            "wendy",
            r#"{"kind":"grok","session_id":"grok-mode-1"}"#,
        );
        let grok = discover(home, true);
        assert_eq!(grok.execution_mode.as_deref(), Some("yolo"));
        assert_eq!(
            grok.context_usage,
            Some(SessionContextUsage {
                used_tokens: 900,
                max_tokens: 2000
            })
        );

        write(
            home,
            ".gemini/antigravity-cli/settings.json",
            r#"{"toolPermission":"always-proceed"}"#,
        );
        write_route(
            home,
            "ferris",
            r#"{"kind":"agy","conversation_id":"agy-mode-1"}"#,
        );
        assert_eq!(
            discover(home, true).execution_mode.as_deref(),
            Some("always-proceed")
        );

        write(
            home,
            ".config/muse/settings.json",
            r#"{"permissions":{"default_profile":":unrestricted"}}"#,
        );
        write_route(home, "mia", r#"{"kind":"muse","session_id":"muse-mode-1"}"#);
        assert_eq!(
            discover(home, true).execution_mode.as_deref(),
            Some("unrestricted")
        );
    }

    #[test]
    fn discovery_populates_antigravity_and_muse_context_usage() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        write(
            home,
            ".gemini/antigravity-cli/brain/agy-ctx-1/.system_generated/logs/transcript.jsonl",
            &lines(
                &[serde_json::json!({"step_index":1,"type":"PLANNER_RESPONSE",
                    "input_tokens":1000,"cache_read_tokens":200,"output_tokens":30})],
            ),
        );
        write_route(
            home,
            "ferris",
            r#"{"kind":"agy","conversation_id":"agy-ctx-1"}"#,
        );
        assert_eq!(
            discover(home, true).context_usage,
            Some(SessionContextUsage {
                used_tokens: 1230,
                max_tokens: 1_000_000
            })
        );
        assert_eq!(discover(home, false).context_usage, None);

        write(
            home,
            ".local/share/muse/sessions/muse-ctx-1/session.jsonl",
            &lines(&[
                serde_json::json!({"payload":{"event":{"kind":"model_completed",
                "usage":{"input_tokens":70,"output_tokens":5}}}}),
            ]),
        );
        write_route(home, "mia", r#"{"kind":"muse","session_id":"muse-ctx-1"}"#);
        assert_eq!(
            discover(home, true).context_usage.map(|u| u.used_tokens),
            Some(75)
        );
    }

    #[test]
    fn a_vendor_with_no_stated_mode_leaves_it_none() {
        let tmp = TempDir::new().unwrap();
        write_route(
            tmp.path(),
            "mia",
            r#"{"kind":"muse","session_id":"muse-none-1"}"#,
        );
        let session = discover(tmp.path(), true);
        assert_eq!(session.execution_mode, None);
        assert_eq!(session.context_usage, None);
    }
}
