//! `fl-gate` — Sub-project 6 (`FerroGate`): Zero-knowledge AES-256-GCM agent secrets vault,
//! credential proxy (`{{FERROGATE:KEY}}`), declarative API-to-MCP gateway, local MCP Registry
//! `v0.1`, and dual-mode outbound/inbound event bridge (spec §1–§6).

use std::path::PathBuf;

pub mod adapter;
pub mod bridge;
pub mod connector;
pub mod mcp;
pub mod proxy;
pub mod registry;
pub mod vault;

pub use adapter::{
    EdgeGuardAdapter, ExternalSecret, GuardStage, GuardVerdict, IamDecision, McpRegistryAdapter,
    McpRelayAdapter, RegistryTier, SecretProviderAdapter, TieredRegistryEntry, authorize_mcp_tool,
    evaluate_edge_guard, forward_mcp_relay, handle_tiered_registry_v01_get,
    publish_connectors_to_registry, resolve_vault_secret,
};
pub use bridge::{
    BridgeConfig, BridgeDispatchPlan, BridgeTarget, InboundBridgeEvent,
    evaluate_inbound_bridge_event, verify_inbound_bridge_webhook,
};
pub use connector::{
    ConnectorManifest, ConnectorToolDef, discover_connectors, import_openapi_to_connector,
    project_response_body,
};
pub use mcp::{
    build_mcp_tools_list, handle_mcp_jsonrpc_request,
    handle_mcp_jsonrpc_request_with_secret_provider,
};
pub use proxy::{
    PLACEHOLDER_PREFIX, PLACEHOLDER_SUFFIX, PreparedProxyRequest, ProxyRequest, ProxyResponse,
    execute_proxied_request, execute_proxied_request_with_provider, extract_placeholder_keys,
    host_matches_pattern, is_private_or_loopback_ip, parse_url_scheme_host_port,
    prepare_proxied_request, prepare_proxied_request_with_provider, scrub_secrets,
    verify_host_ssrf,
};
pub use registry::{connector_to_registry_entry, handle_registry_v01_get};
pub use vault::{
    EncryptedSecretRecord, MASTER_KEY_LEN, NONCE_LEN, SetSecretArgs, VAULT_SCHEMA_VERSION, Vault,
    VaultSecretMetadata, canonical_aad, load_or_init_master_key, normalize_secret_name,
    resolve_master_key_path, resolve_vault_path,
};

/// Errors produced by [`Vault`], [`proxy`], [`connector`], and [`bridge`].
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("could not {op} {}: {cause}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        cause: String,
    },
    #[error("vault file {} is corrupt: {cause}", path.display())]
    VaultCorrupt { path: PathBuf, cause: String },
    #[error(
        "AES-256-GCM authentication failed for secret `{secret}` (master key mismatch or AAD policy tampering)"
    )]
    VaultTampered { secret: String },
    #[error("cryptographic failure: {reason}")]
    Crypto { reason: String },
    #[error("invalid secret `{name}`: {reason}")]
    InvalidSecret { name: String, reason: String },
    #[error("secret `{name}` does not exist in the FerroGate vault")]
    NoSuchSecret { name: String },
    #[error("FerroGate policy denied secret `{secret}`: {reason}")]
    PolicyDenied { secret: String, reason: String },
    #[error("invalid placeholder: {reason}")]
    InvalidPlaceholder { reason: String },
    #[error("invalid URL `{url}`: {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error("SSRF guard blocked host `{host}`: {reason}")]
    SsrfBlocked { host: String, reason: String },
    #[error("upstream HTTP call failed: {cause}")]
    UpstreamHttp { cause: String },
    #[error("adapter `{provider}` failed: {reason}")]
    Adapter { provider: String, reason: String },
    #[error("invalid connector manifest {}: {reason}", path.display())]
    InvalidConnector { path: PathBuf, reason: String },
    #[error("unknown FerroGate connector tool `{tool}`")]
    NoSuchTool { tool: String },
    #[error("invalid arguments for tool `{tool}`: {reason}")]
    InvalidToolArgs { tool: String, reason: String },
    #[error("bridge `{bridge_id}` rejected unauthorized sender `{sender_id}`: {reason}")]
    BridgeUnauthorized {
        bridge_id: String,
        sender_id: String,
        reason: String,
    },
    #[error("bridge `{bridge_id}` error: {reason}")]
    BridgeError { bridge_id: String, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn vault_encrypts_with_aad_and_detects_policy_tampering() {
        let tmp = TempDir::new().unwrap();
        let key_path = tmp.path().join("vault.key");
        let vault_path = tmp.path().join("vault.enc");
        let master_key = load_or_init_master_key(&key_path).unwrap();

        let mut vault = Vault::open(&vault_path, master_key).unwrap();
        let meta = vault
            .set(SetSecretArgs {
                name: "discord_bot_token".into(),
                value: "super-secret-discord-token-9999".into(),
                description: Some("Discord bot token".into()),
                allowed_hosts: vec!["discord.com".into(), "*.discord.com".into()],
                allowed_methods: vec![" get ".into(), "post".into()],
                allowed_agents: vec!["ferris".into(), "felicia".into()],
                allow_private_network: false,
                lease_ttl_secs: None,
            })
            .unwrap();

        assert_eq!(meta.name, "DISCORD_BOT_TOKEN");
        assert_eq!(meta.allowed_hosts, vec!["*.discord.com", "discord.com"]);
        assert_eq!(meta.allowed_methods, vec!["GET", "POST"]);
        assert_eq!(meta.allowed_agents, vec!["felicia", "ferris"]);

        // Ensure `vault.enc` on disk does NOT contain the plaintext secret
        let raw_disk = fs::read_to_string(&vault_path).unwrap();
        assert!(!raw_disk.contains("super-secret-discord-token-9999"));

        // Re-open from disk and verify decryption succeeds
        let reopened = Vault::open(&vault_path, master_key).unwrap();
        let (_m, decrypted) = reopened.decrypt_raw("DISCORD_BOT_TOKEN").unwrap();
        assert_eq!(decrypted, "super-secret-discord-token-9999");

        // Tamper with `allowed_hosts` in `vault.enc` on disk and verify AAD rejects it!
        let mut tampered_json: serde_json::Value = serde_json::from_str(&raw_disk).unwrap();
        tampered_json["secrets"]["DISCORD_BOT_TOKEN"]["metadata"]["allowed_hosts"] =
            json!(["evil.example.com"]);
        fs::write(
            &vault_path,
            serde_json::to_string_pretty(&tampered_json).unwrap(),
        )
        .unwrap();

        let tampered_vault = Vault::open(&vault_path, master_key).unwrap();
        let err = tampered_vault.decrypt_raw("DISCORD_BOT_TOKEN").unwrap_err();
        assert!(
            matches!(err, GateError::VaultTampered { .. }),
            "expected VaultTampered when AAD-bound policy is modified, got: {err}"
        );
    }

    #[test]
    fn proxy_enforces_host_method_agent_ssrf_and_scrubs_leaked_secrets() {
        let tmp = TempDir::new().unwrap();
        let key_path = tmp.path().join("vault.key");
        let vault_path = tmp.path().join("vault.enc");
        let master_key = load_or_init_master_key(&key_path).unwrap();
        let mut vault = Vault::open(&vault_path, master_key).unwrap();

        vault
            .set(SetSecretArgs {
                name: "DISCORD_BOT_TOKEN".into(),
                value: "disc_secret_token_abcdef123456".into(),
                description: None,
                allowed_hosts: vec!["discord.com".into(), "127.0.0.1".into()],
                allowed_methods: vec!["POST".into()],
                allowed_agents: vec!["ferris".into()],
                allow_private_network: false,
                lease_ttl_secs: None,
            })
            .unwrap();

        // 1. Disallowed host -> refused
        let bad_host_req = ProxyRequest {
            method: "POST".into(),
            url: "https://evil.example.com/steal".into(),
            headers: BTreeMap::from([(
                "Authorization".into(),
                "Bot {{FERROGATE:DISCORD_BOT_TOKEN}}".into(),
            )]),
            body: None,
            caller_agent: Some("ferris".into()),
            timeout_secs: None,
        };
        assert!(matches!(
            prepare_proxied_request(&vault, &bad_host_req),
            Err(GateError::PolicyDenied { .. })
        ));

        // 2. Disallowed HTTP method (DELETE instead of POST) -> refused
        let bad_method_req = ProxyRequest {
            method: "DELETE".into(),
            url: "https://discord.com/api/v10/channels/123".into(),
            headers: BTreeMap::from([(
                "Authorization".into(),
                "Bot {{FERROGATE:DISCORD_BOT_TOKEN}}".into(),
            )]),
            body: None,
            caller_agent: Some("ferris".into()),
            timeout_secs: None,
        };
        assert!(matches!(
            prepare_proxied_request(&vault, &bad_method_req),
            Err(GateError::PolicyDenied { .. })
        ));

        // 3. Disallowed agent (`intruder` instead of `ferris`) -> refused
        let bad_agent_req = ProxyRequest {
            method: "POST".into(),
            url: "https://discord.com/api/v10/channels/123/messages".into(),
            headers: BTreeMap::from([(
                "Authorization".into(),
                "Bot {{FERROGATE:DISCORD_BOT_TOKEN}}".into(),
            )]),
            body: None,
            caller_agent: Some("intruder".into()),
            timeout_secs: None,
        };
        assert!(matches!(
            prepare_proxied_request(&vault, &bad_agent_req),
            Err(GateError::PolicyDenied { .. })
        ));

        // 4. SSRF guard blocks 127.0.0.1 when `allow_private_network = false`
        let ssrf_req = ProxyRequest {
            method: "POST".into(),
            url: "https://127.0.0.1:8443/api".into(),
            headers: BTreeMap::from([(
                "Authorization".into(),
                "Bot {{FERROGATE:DISCORD_BOT_TOKEN}}".into(),
            )]),
            body: None,
            caller_agent: Some("ferris".into()),
            timeout_secs: None,
        };
        assert!(matches!(
            prepare_proxied_request(&vault, &ssrf_req),
            Err(GateError::SsrfBlocked { .. })
        ));

        // 5. Valid request against `discord.com` succeeds
        let ok_req = ProxyRequest {
            method: "POST".into(),
            url: "https://discord.com/api/v10/channels/123/messages".into(),
            headers: BTreeMap::from([(
                "Authorization".into(),
                "Bot {{FERROGATE:DISCORD_BOT_TOKEN}}".into(),
            )]),
            body: Some(r#"{"content":"hello"}"#.into()),
            caller_agent: Some("ferris".into()),
            timeout_secs: None,
        };
        let prepared = prepare_proxied_request(&vault, &ok_req).unwrap();
        assert_eq!(
            prepared.headers.get("Authorization").map(String::as_str),
            Some("Bot disc_secret_token_abcdef123456")
        );

        // 6. Output scrubber redacts plaintext, base64-encoded secret values, and standard token prefixes
        let leaked = "Error: upstream echoed token disc_secret_token_abcdef123456 and sk-ant-api03-secretkey998877 in response!";
        let (scrubbed, hits) = scrub_secrets(&vault, leaked);
        assert!(!scrubbed.contains("disc_secret_token_abcdef123456"));
        assert!(!scrubbed.contains("sk-ant-api03-secretkey998877"));
        assert!(scrubbed.contains("[REDACTED:FERROGATE:DISCORD_BOT_TOKEN]"));
        assert!(scrubbed.contains("[REDACTED:FERROGATE:ANTHROPIC_TOKEN]"));
        assert_eq!(
            hits,
            vec![
                "DISCORD_BOT_TOKEN".to_string(),
                "ANTHROPIC_TOKEN".to_string()
            ]
        );

        // 7. Request with zero placeholders still enforces SSRF against 127.0.0.1
        let no_placeholder_ssrf = ProxyRequest {
            method: "GET".into(),
            url: "http://127.0.0.1:8080/internal".into(),
            headers: BTreeMap::new(),
            body: None,
            caller_agent: Some("ferris".into()),
            timeout_secs: None,
        };
        assert!(matches!(
            prepare_proxied_request(&vault, &no_placeholder_ssrf),
            Err(GateError::SsrfBlocked { .. })
        ));
    }

    #[test]
    fn connector_manifest_openapi_import_mcp_and_bridge_round_trip() {
        let tmp = TempDir::new().unwrap();
        let key_path = tmp.path().join("vault.key");
        let vault_path = tmp.path().join("vault.enc");
        let master_key = load_or_init_master_key(&key_path).unwrap();
        let mut vault = Vault::open(&vault_path, master_key).unwrap();

        // Start a local tiny_http upstream server to verify end-to-end connector tool call + scrubbing
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok(req) = server.recv() {
                let auth = req
                    .headers()
                    .iter()
                    .find(|h| {
                        h.field
                            .as_str()
                            .as_str()
                            .eq_ignore_ascii_case("authorization")
                    })
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_default();
                let resp_json = json!({
                    "id": "msg-999",
                    "channel_id": "chan-42",
                    "content": "Posted!",
                    "echoed_auth": auth,
                    "extra_verbose_field": "dropped-by-projection"
                });
                let _ = req.respond(tiny_http::Response::from_string(resp_json.to_string()));
            }
        });

        vault
            .set(SetSecretArgs {
                name: "DISCORD_BOT_TOKEN".into(),
                value: "disc_live_secret_xyz987".into(),
                description: None,
                allowed_hosts: vec!["127.0.0.1".into()],
                allowed_methods: vec!["POST".into()],
                allowed_agents: vec![],
                allow_private_network: true,
                lease_ttl_secs: None,
            })
            .unwrap();

        let toml_text = format!(
            r#"
id = "discord"
title = "Discord API"
version = "0.1.0"
description = "Discord outbound tools and inbound bridge"
base_url = "http://127.0.0.1:{port}/api/v10"
allowed_hosts = ["127.0.0.1"]
default_headers = {{ Authorization = "Bot {{{{FERROGATE:DISCORD_BOT_TOKEN}}}}", "Content-Type" = "application/json" }}
enabled = true

[[tools]]
name = "discord_send_message"
description = "Send a message to a Discord channel"
method = "POST"
path = "/channels/{{channel_id}}/messages"
path_params = ["channel_id"]
body_params = ["content"]
response_fields = ["id", "channel_id", "content", "echoed_auth"]

[[bridges]]
id = "discord-main"
kind = "discord"
enabled = true
allowed_user_ids = ["owner-123"]
default_target = "wire:ferris"

[bridges.channel_routes]
"chan-claude" = "connector:claude"
"#
        );

        let manifest =
            ConnectorManifest::parse_toml(&toml_text, std::path::Path::new("discord.toml"))
                .unwrap();

        // Execute MCP JSON-RPC `tools/call`
        let mcp_call = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "discord_send_message",
                "arguments": {
                    "channel_id": "chan-42",
                    "content": "Hello from FerroGate"
                }
            }
        });
        let mcp_resp = handle_mcp_jsonrpc_request(
            &mcp_call,
            &vault,
            std::slice::from_ref(&manifest),
            None,
            Some("ferris".into()),
        )
        .unwrap();
        handle.join().unwrap();

        let result_text = mcp_resp["result"]["content"][0]["text"].as_str().unwrap();
        // Verify secret was injected on the wire, scrubbed in the response, and `extra_verbose_field` was projected out!
        assert!(!result_text.contains("disc_live_secret_xyz987"));
        assert!(result_text.contains("[REDACTED:FERROGATE:DISCORD_BOT_TOKEN]"));
        assert!(result_text.contains("msg-999"));
        assert!(!result_text.contains("extra_verbose_field"));

        // Verify MCP Registry v0.1 endpoint formatting
        let (reg_status, reg_body) = handle_registry_v01_get(
            "/v0.1/servers",
            &BTreeMap::new(),
            std::slice::from_ref(&manifest),
            "http://127.0.0.1:8420",
        );
        assert_eq!(reg_status, 200);
        assert_eq!(reg_body["metadata"]["count"], 1);
        assert_eq!(
            reg_body["servers"][0]["server"]["name"],
            "io.ferroloop.gate/discord"
        );

        // Verify OpenAPI 3.x importer produces a valid ConnectorManifest
        let openapi_json = r#"{
            "openapi": "3.0.3",
            "info": { "title": "Mini Discord", "version": "1.0.0" },
            "servers": [{ "url": "https://discord.com/api/v10" }],
            "paths": {
                "/channels/{channel_id}/messages": {
                    "post": {
                        "operationId": "createMessage",
                        "summary": "Create a channel message",
                        "parameters": [
                            { "name": "channel_id", "in": "path", "required": true, "schema": { "type": "string" } }
                        ],
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["content"],
                                        "properties": {
                                            "content": { "type": "string" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let imported =
            import_openapi_to_connector(openapi_json, "discord", Some("DISCORD_BOT_TOKEN"))
                .unwrap();
        assert_eq!(imported.id, "discord");
        assert_eq!(imported.allowed_hosts, vec!["discord.com".to_string()]);
        assert_eq!(imported.tools.len(), 1);
        assert_eq!(imported.tools[0].name, "discord_createmessage");
        assert_eq!(
            imported.tools[0].path_params,
            vec!["channel_id".to_string()]
        );
        assert_eq!(imported.tools[0].body_params, vec!["content".to_string()]);

        // Verify Inbound Bridge allowlist and dual target routing (`wire:ferris` & `connector:claude`)
        let bridge = &manifest.bridges[0];
        let unauth_ev = InboundBridgeEvent {
            bridge_id: "discord-main".into(),
            sender_id: "stranger-999".into(),
            sender_name: None,
            guild_id: None,
            channel_id: "chan-general".into(),
            thread_id: None,
            message_id: None,
            content: "Hello".into(),
        };
        assert!(matches!(
            evaluate_inbound_bridge_event(bridge, &unauth_ev),
            Err(GateError::BridgeUnauthorized { .. })
        ));

        let wire_ev = InboundBridgeEvent {
            bridge_id: "discord-main".into(),
            sender_id: "owner-123".into(),
            sender_name: Some("lmcfarlin".into()),
            guild_id: None,
            channel_id: "chan-general".into(),
            thread_id: None,
            message_id: None,
            content: "@jarvis check GPU status".into(),
        };
        let plan1 = evaluate_inbound_bridge_event(bridge, &wire_ev).unwrap();
        assert_eq!(
            plan1.target,
            BridgeTarget::WireAgent {
                persona: "jarvis".into()
            }
        );
        assert_eq!(plan1.prompt_body, "check GPU status");

        let conn_ev = InboundBridgeEvent {
            bridge_id: "discord-main".into(),
            sender_id: "owner-123".into(),
            sender_name: Some("lmcfarlin".into()),
            guild_id: None,
            channel_id: "chan-claude".into(),
            thread_id: None,
            message_id: None,
            content: "Summarize the latest commit".into(),
        };
        let plan2 = evaluate_inbound_bridge_event(bridge, &conn_ev).unwrap();
        assert_eq!(
            plan2.target,
            BridgeTarget::Connector {
                connector_id: "claude".into(),
                tool_name: None,
            }
        );

        // Verify `verify_inbound_bridge_webhook` enforces `verify_secret` and blocks unverified Peer callers
        let tmp_vault_dir = tempfile::tempdir().unwrap();
        let mut webhook_vault =
            Vault::open(tmp_vault_dir.path().join("vault.enc"), [19u8; 32]).unwrap();
        webhook_vault
            .set(SetSecretArgs {
                name: "DISCORD_WEBHOOK_SECRET".into(),
                value: "whsec-correct-token-789".into(),
                allowed_hosts: vec!["discord.com".into()],
                ..Default::default()
            })
            .unwrap();
        let mut secured_bridge = bridge.clone();
        secured_bridge.verify_secret = Some("DISCORD_WEBHOOK_SECRET".into());

        assert!(
            verify_inbound_bridge_webhook(&secured_bridge, &webhook_vault, false, None).is_err(),
            "Peer without webhook header must be rejected"
        );
        assert!(
            verify_inbound_bridge_webhook(
                &secured_bridge,
                &webhook_vault,
                true,
                Some("wrong-token")
            )
            .is_err(),
            "Wrong webhook secret must be rejected even for Owner"
        );
        assert!(
            verify_inbound_bridge_webhook(
                &secured_bridge,
                &webhook_vault,
                false,
                Some("whsec-correct-token-789")
            )
            .is_ok(),
            "Valid webhook secret in constant time must succeed"
        );
    }
}
