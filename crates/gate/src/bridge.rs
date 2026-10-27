//! Dual-mode inbound event bridge (`[[bridges]]` in `.fl/gate/*.toml`) for Discord and webhook
//! channels, supporting both `FerroWire` resident agent targets (`wire:<persona>`) and direct
//! LLM connector targets (`connector:<id>`) (spec §5).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::GateError;
use crate::vault::Vault;

fn default_true() -> bool {
    true
}

/// Verify webhook caller authority and optional `verify_secret` header before evaluating an inbound bridge event.
///
/// - When `bridge.verify_secret` is configured, `provided_secret` must match the secret value stored in `vault`
///   in constant time (unless the caller is `is_owner` and the secret has not been provisioned in `vault` yet).
/// - Non-owner (`Peer`) callers are never allowed to invoke a bridge webhook unless `bridge.verify_secret` is
///   configured and matched in constant time.
pub fn verify_inbound_bridge_webhook(
    bridge: &BridgeConfig,
    vault: &Vault,
    is_owner: bool,
    provided_secret: Option<&str>,
) -> Result<(), GateError> {
    if let Some(ref secret_name) = bridge.verify_secret {
        let trimmed_name = secret_name.trim();
        if !trimmed_name.is_empty() {
            let secret_exists = vault.get_metadata(trimmed_name).is_some();
            if secret_exists || !is_owner {
                let Some(candidate) = provided_secret.map(str::trim).filter(|s| !s.is_empty())
                else {
                    return Err(GateError::BridgeUnauthorized {
                        bridge_id: bridge.id.clone(),
                        sender_id: String::new(),
                        reason: format!(
                            "missing webhook verification header for secret `{trimmed_name}`"
                        ),
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
        }
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

    let (override_target, cleaned_body) = extract_inline_routing_directive(&event.content);
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

/// Extract an inline `@<persona>` or `/model <connector>` prefix from `content`.
fn extract_inline_routing_directive(content: &str) -> (Option<BridgeTarget>, String) {
    let trimmed = content.trim();
    if let Some(rest) = trimmed.strip_prefix("/model ") {
        let mut parts = rest.trim_start().splitn(2, char::is_whitespace);
        if let Some(conn) = parts.next()
            && !conn.is_empty()
        {
            let body = parts.next().unwrap_or("").trim().to_string();
            if let Ok(target) = BridgeTarget::parse(&format!("connector:{conn}")) {
                return (Some(target), body);
            }
        }
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
                return (Some(target), body);
            }
        }
    }
    (None, trimmed.to_string())
}
