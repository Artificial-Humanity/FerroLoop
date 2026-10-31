//! Dual-mode inbound event bridge (`[[bridges]]` in `.fl/gate/*.toml`) for Discord and webhook
//! channels, supporting both `FerroWire` resident agent targets (`wire:<persona>`) and direct
//! LLM connector targets (`connector:<id>`) (spec §5).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::GateError;
use crate::connector::is_valid_connector_id;
use crate::vault::Vault;

fn default_true() -> bool {
    true
}

/// Verify webhook caller authority and optional `verify_secret` header before evaluating an inbound bridge event.
///
/// - When `bridge.verify_secret` is configured, the named secret must be provisioned in `vault` and
///   `provided_secret` must match it in constant time, for owner and peer callers alike. A configured
///   name that is empty or not in the vault is a misconfiguration and refuses every caller (fail
///   closed), so a misspelt `verify_secret` can never switch verification off.
/// - When no `verify_secret` is configured, owner callers are accepted and non-owner (`Peer`)
///   callers are refused: a peer is never allowed to invoke a bridge webhook without a verified
///   webhook secret.
pub fn verify_inbound_bridge_webhook(
    bridge: &BridgeConfig,
    vault: &Vault,
    is_owner: bool,
    provided_secret: Option<&str>,
) -> Result<(), GateError> {
    if let Some(ref secret_name) = bridge.verify_secret {
        let trimmed_name = secret_name.trim();
        if trimmed_name.is_empty() {
            return Err(GateError::BridgeUnauthorized {
                bridge_id: bridge.id.clone(),
                sender_id: String::new(),
                reason: "`verify_secret` is set but empty; name the vault secret that signs this \
                         webhook, or remove `verify_secret`"
                    .to_string(),
            });
        }
        if vault.get_metadata(trimmed_name).is_none() {
            return Err(GateError::BridgeUnauthorized {
                bridge_id: bridge.id.clone(),
                sender_id: String::new(),
                reason: format!(
                    "webhook secret `{trimmed_name}` named by `verify_secret` is not provisioned in \
                     the vault; provision it with `fl vault set {trimmed_name}` or correct the \
                     `verify_secret` spelling"
                ),
            });
        }
        let Some(candidate) = provided_secret.map(str::trim).filter(|s| !s.is_empty()) else {
            return Err(GateError::BridgeUnauthorized {
                bridge_id: bridge.id.clone(),
                sender_id: String::new(),
                reason: format!("missing webhook verification header for secret `{trimmed_name}`"),
            });
        };
        let matched = vault
            .verify_secret_constant_time(trimmed_name, candidate)
            .unwrap_or(false);
        if !matched {
            return Err(GateError::BridgeUnauthorized {
                bridge_id: bridge.id.clone(),
                sender_id: String::new(),
                reason: format!("invalid webhook verification secret for `{trimmed_name}`"),
            });
        }
        return Ok(());
    }

    if !is_owner {
        return Err(GateError::BridgeUnauthorized {
            bridge_id: bridge.id.clone(),
            sender_id: String::new(),
            reason: "peer callers cannot invoke bridge webhooks without a verified webhook secret"
                .to_string(),
        });
    }

    Ok(())
}

/// Declarative `[[bridges]]` configuration inside a [`crate::connector::ConnectorManifest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeConfig {
    pub id: String,
    pub kind: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_secret: Option<String>,
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub allowed_guild_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_target: Option<String>,
    #[serde(default)]
    pub channel_routes: BTreeMap<String, String>,
    /// Connector ids a sender may pick with an inline `/model <connector>` directive. Empty (the
    /// default) refuses every inline connector choice. Exact ids only: `*` is not a wildcard, and a
    /// connector tool can never be chosen from chat text.
    #[serde(default)]
    pub allowed_inline_connectors: Vec<String>,
}

/// Normalized inbound message event arriving from Discord or a webhook bridge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboundBridgeEvent {
    pub bridge_id: String,
    pub sender_id: String,
    #[serde(default)]
    pub sender_name: Option<String>,
    #[serde(default)]
    pub guild_id: Option<String>,
    pub channel_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    pub content: String,
}

/// Resolved destination for an authorized [`InboundBridgeEvent`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeTarget {
    /// Route to a workspace resident agent over `FerroWire` / `fl-hub` operator prompt channel.
    WireAgent { persona: String },
    /// Route directly to a declarative LLM/API connector (`claude`, `gemini`, `openai`, `muse`, etc.).
    Connector {
        connector_id: String,
        tool_name: Option<String>,
    },
}

/// Verified dispatch plan produced by [`evaluate_inbound_bridge_event`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeDispatchPlan {
    pub bridge_id: String,
    pub sender_id: String,
    pub channel_id: String,
    pub target: BridgeTarget,
    pub prompt_body: String,
}

impl BridgeTarget {
    /// Parse `"wire:<persona>"`, `"session:<persona>"`, or `"connector:<id>[:<tool>]"`.
    pub fn parse(spec: &str) -> Result<Self, GateError> {
        let trimmed = spec.trim();
        if let Some(persona) = trimmed
            .strip_prefix("wire:")
            .or_else(|| trimmed.strip_prefix("session:"))
        {
            let p = persona.trim().to_ascii_lowercase();
            if p.is_empty() {
                return Err(GateError::BridgeError {
                    bridge_id: String::new(),
                    reason: format!("empty persona in bridge target `{trimmed}`"),
                });
            }
            return Ok(Self::WireAgent { persona: p });
        }
        if let Some(rest) = trimmed.strip_prefix("connector:") {
            let (conn_id, tool_opt) = match rest.split_once(':') {
                Some((c, t)) => (c.trim().to_ascii_lowercase(), Some(t.trim().to_string())),
                None => (rest.trim().to_ascii_lowercase(), None),
            };
            if conn_id.is_empty() {
                return Err(GateError::BridgeError {
                    bridge_id: String::new(),
                    reason: format!("empty connector id in bridge target `{trimmed}`"),
                });
            }
            return Ok(Self::Connector {
                connector_id: conn_id,
                tool_name: tool_opt.filter(|s| !s.is_empty()),
            });
        }
        Err(GateError::BridgeError {
            bridge_id: String::new(),
            reason: format!(
                "invalid bridge target `{trimmed}` (expected `wire:<persona>` or `connector:<id>`)"
            ),
        })
    }
}

/// Verify sender/guild allowlists and resolve the [`BridgeTarget`] for `event` (spec §5).
pub fn evaluate_inbound_bridge_event(
    bridge: &BridgeConfig,
    event: &InboundBridgeEvent,
) -> Result<BridgeDispatchPlan, GateError> {
    if !bridge.enabled {
        return Err(GateError::BridgeError {
            bridge_id: bridge.id.clone(),
            reason: "bridge is disabled".to_string(),
        });
    }

    if bridge.allowed_user_ids.is_empty()
        || !bridge
            .allowed_user_ids
            .iter()
            .any(|u| u == "*" || u == event.sender_id.trim())
    {
        return Err(GateError::BridgeUnauthorized {
            bridge_id: bridge.id.clone(),
            sender_id: event.sender_id.clone(),
            reason: "sender_id is not in allowed_user_ids".to_string(),
        });
    }

    if !bridge.allowed_guild_ids.is_empty() {
        let guild = event.guild_id.as_deref().unwrap_or("").trim();
        if !bridge
            .allowed_guild_ids
            .iter()
            .any(|g| g == "*" || g == guild)
        {
            return Err(GateError::BridgeUnauthorized {
                bridge_id: bridge.id.clone(),
                sender_id: event.sender_id.clone(),
                reason: format!("guild_id `{guild}` is not in allowed_guild_ids"),
            });
        }
    }

    let (override_target, cleaned_body) = extract_inline_routing_directive(bridge, event)?;
    let target = if let Some(t) = override_target {
        t
    } else if let Some(route_str) = bridge.channel_routes.get(event.channel_id.trim()) {
        BridgeTarget::parse(route_str)?
    } else if let Some(ref def_str) = bridge.default_target {
        BridgeTarget::parse(def_str)?
    } else {
        return Err(GateError::BridgeError {
            bridge_id: bridge.id.clone(),
            reason: format!(
                "no channel_route or default_target configured for channel `{}`",
                event.channel_id
            ),
        });
    };

    if cleaned_body.trim().is_empty() {
        return Err(GateError::BridgeError {
            bridge_id: bridge.id.clone(),
            reason: "inbound bridge message body is empty".to_string(),
        });
    }

    Ok(BridgeDispatchPlan {
        bridge_id: bridge.id.clone(),
        sender_id: event.sender_id.clone(),
        channel_id: event.channel_id.clone(),
        target,
        prompt_body: cleaned_body,
    })
}

/// Longest connector id accepted from chat text.
const MAX_INLINE_CONNECTOR_ID_LEN: usize = 64;

/// Extract an inline `@<persona>` or `/model <connector>` prefix from `event.content`, returning the
/// target it names and the remaining body, both taken from the same text.
///
/// `/model` is honoured only for a connector id the bridge lists in `allowed_inline_connectors`, and
/// never with a tool (`<connector>:<tool>`): the sender is trusted with the listed connectors, not
/// with every connector (and vault credential) the hub knows. Every other `/model` form is refused
/// with an error that says why.
fn extract_inline_routing_directive(
    bridge: &BridgeConfig,
    event: &InboundBridgeEvent,
) -> Result<(Option<BridgeTarget>, String), GateError> {
    let trimmed = event.content.trim();
    if let Some(rest) = trimmed.strip_prefix("/model")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        let refuse = |reason: String| GateError::BridgeUnauthorized {
            bridge_id: bridge.id.clone(),
            sender_id: event.sender_id.clone(),
            reason,
        };
        let rest = rest.trim_start();
        let (conn, body) = match rest.split_once(char::is_whitespace) {
            Some((conn, body)) => (conn, body.trim()),
            None => (rest, ""),
        };
        if conn.is_empty() {
            return Err(GateError::BridgeError {
                bridge_id: bridge.id.clone(),
                reason: "`/model` needs a connector id: `/model <connector> <message>`".to_string(),
            });
        }
        if conn.contains(':') {
            return Err(refuse(
                "inline tool selection is not allowed: `/model` takes a connector id only, never \
                 `<connector>:<tool>`"
                    .to_string(),
            ));
        }
        if conn.len() > MAX_INLINE_CONNECTOR_ID_LEN || !is_valid_connector_id(conn) {
            return Err(refuse(
                "`/model` needs a connector id of lowercase ASCII letters, digits, `-` or `_`"
                    .to_string(),
            ));
        }
        if !bridge
            .allowed_inline_connectors
            .iter()
            .any(|allowed| allowed.trim() == conn)
        {
            return Err(refuse(format!(
                "inline connector `{conn}` is not in this bridge's allowed_inline_connectors"
            )));
        }
        return Ok((
            Some(BridgeTarget::Connector {
                connector_id: conn.to_string(),
                tool_name: None,
            }),
            body.to_string(),
        ));
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        if let Some(name) = parts.next()
            && !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            let body = parts.next().unwrap_or("").trim().to_string();
            if !body.is_empty()
                && let Ok(target) = BridgeTarget::parse(&format!("wire:{name}"))
            {
                return Ok((Some(target), body));
            }
        }
    }
    Ok((None, trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::ConnectorManifest;
    use crate::vault::SetSecretArgs;

    const SECRET_VALUE: &str = "whsec-very-secret-value-123";

    fn bridge() -> BridgeConfig {
        BridgeConfig {
            id: "hook".into(),
            kind: "webhook".into(),
            enabled: true,
            verify_secret: None,
            bot_token_secret: None,
            allowed_user_ids: vec!["u1".into()],
            allowed_guild_ids: Vec::new(),
            default_target: Some("wire:ferris".into()),
            channel_routes: BTreeMap::new(),
            allowed_inline_connectors: Vec::new(),
        }
    }

    fn event(content: &str) -> InboundBridgeEvent {
        InboundBridgeEvent {
            bridge_id: "hook".into(),
            sender_id: "u1".into(),
            sender_name: None,
            guild_id: None,
            channel_id: "chan".into(),
            thread_id: None,
            message_id: None,
            content: content.into(),
        }
    }

    /// A vault holding `WEBHOOK_SECRET` only.
    fn vault_with_secret(dir: &tempfile::TempDir) -> Vault {
        let mut vault = Vault::open(dir.path().join("vault.enc"), [7u8; 32]).unwrap();
        vault
            .set(SetSecretArgs {
                name: "WEBHOOK_SECRET".into(),
                value: SECRET_VALUE.into(),
                allowed_hosts: vec!["discord.com".into()],
                ..Default::default()
            })
            .unwrap();
        vault
    }

    fn reason(err: GateError) -> String {
        match err {
            GateError::BridgeUnauthorized { reason, .. } => reason,
            other => panic!("expected BridgeUnauthorized, got {other:?}"),
        }
    }

    #[test]
    fn unprovisioned_verify_secret_refuses_owner_and_peer_naming_the_secret_only() {
        let dir = tempfile::tempdir().unwrap();
        let vault = vault_with_secret(&dir);
        let mut b = bridge();
        b.verify_secret = Some("WEBHOOK_SECRTE".into()); // misspelt
        for is_owner in [true, false] {
            for header in [None, Some(SECRET_VALUE), Some("anything")] {
                let msg = reason(
                    verify_inbound_bridge_webhook(&b, &vault, is_owner, header).unwrap_err(),
                );
                assert!(msg.contains("WEBHOOK_SECRTE"), "{msg}");
                assert!(msg.contains("fl vault set WEBHOOK_SECRTE"), "{msg}");
                assert!(!msg.contains(SECRET_VALUE), "{msg}");
            }
        }
        // The bridge id is named too.
        let err = verify_inbound_bridge_webhook(&b, &vault, true, None).unwrap_err();
        assert!(err.to_string().contains("`hook`"), "{err}");
    }

    #[test]
    fn provisioned_verify_secret_needs_the_header_for_owner_and_peer() {
        let dir = tempfile::tempdir().unwrap();
        let vault = vault_with_secret(&dir);
        let mut b = bridge();
        b.verify_secret = Some("WEBHOOK_SECRET".into());
        for is_owner in [true, false] {
            assert!(verify_inbound_bridge_webhook(&b, &vault, is_owner, None).is_err());
            assert!(verify_inbound_bridge_webhook(&b, &vault, is_owner, Some("  ")).is_err());
            assert!(verify_inbound_bridge_webhook(&b, &vault, is_owner, Some("wrong")).is_err());
            verify_inbound_bridge_webhook(&b, &vault, is_owner, Some(SECRET_VALUE)).unwrap();
        }
        // The configured name is trimmed before the lookup.
        b.verify_secret = Some("  WEBHOOK_SECRET ".into());
        verify_inbound_bridge_webhook(&b, &vault, false, Some(SECRET_VALUE)).unwrap();
    }

    #[test]
    fn empty_verify_secret_name_fails_closed_for_everyone() {
        let dir = tempfile::tempdir().unwrap();
        let vault = vault_with_secret(&dir);
        for name in ["", "   ", "\t"] {
            let mut b = bridge();
            b.verify_secret = Some(name.into());
            for is_owner in [true, false] {
                let msg = reason(
                    verify_inbound_bridge_webhook(&b, &vault, is_owner, Some(SECRET_VALUE))
                        .unwrap_err(),
                );
                assert!(msg.contains("verify_secret"), "{msg}");
                assert!(msg.contains("empty"), "{msg}");
            }
        }
    }

    #[test]
    fn no_verify_secret_accepts_the_owner_and_refuses_a_peer() {
        let dir = tempfile::tempdir().unwrap();
        let vault = vault_with_secret(&dir);
        let b = bridge();
        verify_inbound_bridge_webhook(&b, &vault, true, None).unwrap();
        verify_inbound_bridge_webhook(&b, &vault, true, Some("ignored")).unwrap();
        for header in [None, Some(SECRET_VALUE)] {
            assert!(verify_inbound_bridge_webhook(&b, &vault, false, header).is_err());
        }
    }

    fn plan(b: &BridgeConfig, content: &str) -> Result<BridgeDispatchPlan, GateError> {
        evaluate_inbound_bridge_event(b, &event(content))
    }

    fn connector(id: &str) -> BridgeTarget {
        BridgeTarget::Connector {
            connector_id: id.into(),
            tool_name: None,
        }
    }

    #[test]
    fn inline_model_with_a_tool_is_always_refused() {
        let mut b = bridge();
        b.allowed_inline_connectors = vec!["github".into()];
        for content in [
            "/model github:delete_repo drop it",
            "/model github: drop it",
            "/model :delete_repo drop it",
            "/model github:a:b drop it",
        ] {
            let msg = reason(plan(&b, content).unwrap_err());
            assert!(msg.contains("inline tool selection"), "{content}: {msg}");
        }
    }

    #[test]
    fn inline_model_needs_the_connector_in_the_bridge_allow_list() {
        let mut b = bridge();
        let msg = reason(plan(&b, "/model github hello").unwrap_err());
        assert!(msg.contains("allowed_inline_connectors"), "{msg}");

        b.allowed_inline_connectors = vec!["gitlab".into()];
        assert!(plan(&b, "/model github hello").is_err());

        b.allowed_inline_connectors = vec!["github".into()];
        let p = plan(&b, "/model github hello there").unwrap();
        assert_eq!(p.target, connector("github"));
        assert_eq!(p.prompt_body, "hello there");
        // Surrounding whitespace in the message is ignored; the id itself is exact.
        let p = plan(&b, "  /model\tgithub   hello \n").unwrap();
        assert_eq!(p.target, connector("github"));
        assert_eq!(p.prompt_body, "hello");
        for content in [
            "/model Github hello",
            "/model GITHUB hello",
            "/model githu hello",
        ] {
            assert!(plan(&b, content).is_err(), "{content}");
        }
        // A list entry is trimmed; it is still an exact id, not a prefix or a wildcard.
        b.allowed_inline_connectors = vec![" github ".into()];
        assert!(plan(&b, "/model github hello").is_ok());
        b.allowed_inline_connectors = vec!["*".into(), "git".into()];
        assert!(plan(&b, "/model github hello").is_err());
    }

    #[test]
    fn inline_model_with_a_malformed_id_never_reaches_target_parsing() {
        let mut b = bridge();
        // Every id the check could be tricked into listing is still refused for its charset.
        b.allowed_inline_connectors = vec![
            "a:b".into(),
            "../x".into(),
            "a b".into(),
            "a\u{0}b".into(),
            "a/b".into(),
        ];
        let long = "a".repeat(MAX_INLINE_CONNECTOR_ID_LEN + 1);
        b.allowed_inline_connectors.push(long.clone());
        for content in [
            "/model a:b hi".to_string(),
            "/model ../x hi".to_string(),
            "/model a/b hi".to_string(),
            "/model a\u{0}b hi".to_string(),
            "/model a\u{7}b hi".to_string(),
            "/model a\u{202e}b hi".to_string(),
            "/model gïthub hi".to_string(),
            format!("/model {long} hi"),
        ] {
            assert!(plan(&b, &content).is_err(), "{content:?}");
        }
        // `a b` is two tokens: the id is `a`, which is not listed.
        assert!(plan(&b, "/model a b hi").is_err());
        // No connector id at all.
        for content in ["/model", "/model   ", "/model\n"] {
            assert!(
                matches!(plan(&b, content), Err(GateError::BridgeError { .. })),
                "{content:?}"
            );
        }
    }

    #[test]
    fn only_a_leading_model_word_is_a_directive() {
        let b = bridge();
        for content in [
            "please run /model github hi",
            "hello /model github",
            "/modelgithub hi",
            "/models github hi",
            "/Model github hi",
        ] {
            let p = plan(&b, content).unwrap();
            assert_eq!(
                p.target,
                BridgeTarget::WireAgent {
                    persona: "ferris".into()
                }
            );
            assert_eq!(p.prompt_body, content);
        }
        // A persona directive is not re-parsed for a nested `/model`.
        let p = plan(&b, "@jarvis /model github:x hi").unwrap();
        assert_eq!(
            p.target,
            BridgeTarget::WireAgent {
                persona: "jarvis".into()
            }
        );
        assert_eq!(p.prompt_body, "/model github:x hi");
    }

    #[test]
    fn persona_and_plain_messages_route_as_before() {
        let mut b = bridge();
        b.channel_routes
            .insert("chan".into(), "connector:claude:summarize".into());
        let p = plan(&b, "@Jarvis check GPU").unwrap();
        assert_eq!(
            p.target,
            BridgeTarget::WireAgent {
                persona: "jarvis".into()
            }
        );
        assert_eq!(p.prompt_body, "check GPU");
        // The operator's own route may still name a tool.
        let p = plan(&b, "just text").unwrap();
        assert_eq!(
            p.target,
            BridgeTarget::Connector {
                connector_id: "claude".into(),
                tool_name: Some("summarize".into()),
            }
        );
        assert_eq!(p.prompt_body, "just text");
        // An empty body after an allowed directive is still refused.
        b.allowed_inline_connectors = vec!["claude".into()];
        assert!(plan(&b, "/model claude").is_err());
    }

    fn manifest(extra: &str) -> Result<ConnectorManifest, GateError> {
        let text = format!(
            r#"
id = "hooks"
title = "Hooks"
base_url = "http://127.0.0.1:1"

[[bridges]]
id = "hook"
kind = "webhook"
allowed_user_ids = ["u1"]
{extra}
"#
        );
        ConnectorManifest::parse_toml(&text, std::path::Path::new("hooks.toml"))
    }

    #[test]
    fn manifest_validation_rejects_wildcard_and_malformed_inline_connectors() {
        let ok = manifest(r#"allowed_inline_connectors = ["github", " gitlab "]"#).unwrap();
        assert_eq!(ok.bridges[0].allowed_inline_connectors.len(), 2);
        assert!(
            manifest("").unwrap().bridges[0]
                .allowed_inline_connectors
                .is_empty()
        );
        for bad in ["*", " * ", "", "Github", "a:b", "../x", "a b"] {
            let err = manifest(&format!("allowed_inline_connectors = [{bad:?}]")).unwrap_err();
            assert!(
                err.to_string().contains("allowed_inline_connectors"),
                "{bad:?}: {err}"
            );
        }
        let err = manifest(r#"allowed_inline_connectors = ["*"]"#).unwrap_err();
        assert!(err.to_string().contains("not a wildcard"), "{err}");
    }
}
