//! `fl vault` (alias `fl gateway`) CLI subcommands for managing `FerroGate`:
//! zero-knowledge AES-256-GCM secret vault, credential proxy (`fl vault call`),
//! declarative API-to-MCP connectors, OpenAPI importer, unified MCP server (`fl vault mcp`),
//! and single-registration vendor installation (`fl vault install`) (spec §6.1).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use clap::Subcommand;
use fl_core::governor::{AdmissionRequest, Principal};
use fl_gate::{
    AppBinding, DispatchPhaseError, DispatchResult, GateGovernor, GovernedOrGateError,
    ProxyRequest, SecretUsage, SetSecretArgs, Vault, admission_refusal_text,
    approved_policy_digests_from_env, discover_connectors,
    execute_proxied_request_with_provider_and_contract_phased_with_deadline,
    handle_mcp_jsonrpc_request_with_governor_and_secret_provider, import_openapi_to_connector,
    load_or_init_master_key, resolve_master_key_path, resolve_vault_path,
};
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum VaultCmd {
    /// Initialize the FerroGate master key (`0600`) and encrypted `vault.enc` store.
    Init {
        /// Output JSON summary.
        #[arg(long)]
        json: bool,
    },
    /// Encrypt and store a secret in `vault.enc` with AEAD AAD-bound host/method/agent policies.
    Set {
        /// Secret key name (e.g. `DISCORD_BOT_TOKEN`, `ANTHROPIC_API_KEY`).
        name: String,
        /// Secret plaintext value (omit and pass `--stdin` to read from standard input).
        #[arg(long)]
        value: Option<String>,
        /// Read the secret plaintext value from standard input.
        #[arg(long)]
        stdin: bool,
        /// Optional human-readable description.
        #[arg(long)]
        description: Option<String>,
        /// Allowed destination hostnames (e.g. `discord.com`, `*.openai.com`). Repeatable.
        #[arg(long = "host")]
        allowed_hosts: Vec<String>,
        /// Allowed HTTP methods (e.g. `GET`, `POST`). Repeatable; empty permits any method.
        #[arg(long = "method")]
        allowed_methods: Vec<String>,
        /// Allowed caller agent personas (e.g. `ferris`, `felicia`, `*`). Repeatable.
        #[arg(long = "agent")]
        allowed_agents: Vec<String>,
        /// Allow substituting this secret on loopback/RFC1918 local network endpoints.
        #[arg(long)]
        allow_private_network: bool,
        /// Optional lease time-to-live in seconds.
        #[arg(long)]
        ttl_secs: Option<u64>,
        /// Secret usage: proxy (default) or sign-jwt-rs256.
        #[arg(long, default_value = "proxy")]
        usage: String,
        /// Bound App issuer for signing records.
        #[arg(long)]
        issuer: Option<String>,
        /// Bound HTTPS origin for signing records.
        #[arg(long)]
        origin: Option<String>,
        /// Require live bundle authority for use.
        #[arg(long)]
        bundle_required: bool,
        /// Output JSON summary.
        #[arg(long)]
        json: bool,
    },
    /// List redacted secret metadata stored in `vault.enc` (never prints secret values).
    List {
        /// Output JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Remove a secret from `vault.enc`.
    Remove {
        /// Secret key name to remove.
        name: String,
        /// Output JSON result.
        #[arg(long)]
        json: bool,
    },
    /// Execute an outbound HTTP call through the zero-knowledge proxy and output scrubber.
    Call {
        /// Target URL (`https://...`).
        #[arg(long)]
        url: String,
        /// HTTP method (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`).
        #[arg(long, default_value = "GET")]
        method: String,
        /// Request header in `Name: Value` format (supports `{{FERROGATE:KEY}}`). Repeatable.
        #[arg(long = "header")]
        headers: Vec<String>,
        /// Optional request body string (supports `{{FERROGATE:KEY}}`).
        #[arg(long)]
        body: Option<String>,
        /// Calling agent persona for ACL verification.
        #[arg(long)]
        agent: Option<String>,
        /// Request timeout in seconds.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        /// Output full JSON response envelope.
        #[arg(long)]
        json: bool,
    },
    /// List discovered `.fl/gate/*.toml` and `~/.config/fl/gate/*.toml` connectors.
    Connectors {
        /// Output JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Import an OpenAPI 3.x JSON specification into a `.fl/gate/<id>.toml` connector manifest.
    ImportOpenapi {
        /// Path to the OpenAPI 3.x `.json` file.
        spec_path: PathBuf,
        /// Connector identifier (lowercase alphanumeric / `-` / `_`).
        #[arg(long)]
        id: String,
        /// Optional vault secret name to wire into `Authorization: Bearer {{FERROGATE:<SECRET>}}`.
        #[arg(long)]
        secret: Option<String>,
        /// Output `.toml` path (defaults to `.fl/gate/<id>.toml`).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Run the unified FerroGate MCP JSON-RPC 2.0 server over `stdio`.
    Mcp {
        /// Optional connector ID filter to expose only a single connector's tools.
        #[arg(long)]
        connector: Option<String>,
    },
    /// Register the single unified `ferrogate` MCP server across vendor CLI configs.
    Install {
        /// Write to user-home vendor configurations (`~/.claude.json`, `~/.codex/config.toml`,
        /// `~/.gemini/antigravity-cli/mcp_config.json`, `~/.config/muse/settings.json`).
        #[arg(long)]
        user: bool,
        /// Output JSON summary of updated configuration files.
        #[arg(long)]
        json: bool,
    },
}

/// Run with the CLI's state environment (as wired in `main` the way other
/// CLI state commands are): `fl vault mcp` opens the configured store
/// through it so connector dispatch is governed.
pub fn run_with_state_env(
    cwd: &Path,
    cmd: VaultCmd,
    env: &crate::cmd::state::StateEnv<'_>,
) -> Result<i32, (i32, String)> {
    run_inner(cwd, cmd, Some(env))
}

fn run_inner(
    cwd: &Path,
    cmd: VaultCmd,
    env: Option<&crate::cmd::state::StateEnv<'_>>,
) -> Result<i32, (i32, String)> {
    match cmd {
        VaultCmd::Init { json } => cmd_init(json),
        VaultCmd::Set {
            name,
            value,
            stdin,
            description,
            allowed_hosts,
            allowed_methods,
            allowed_agents,
            allow_private_network,
            ttl_secs,
            usage,
            issuer,
            origin,
            bundle_required,
            json,
        } => cmd_set(
            name,
            value,
            stdin,
            description,
            allowed_hosts,
            allowed_methods,
            allowed_agents,
            allow_private_network,
            ttl_secs,
            usage,
            issuer,
            origin,
            bundle_required,
            json,
        ),
        VaultCmd::List { json } => cmd_list(json),
        VaultCmd::Remove { name, json } => cmd_remove(&name, json),
        VaultCmd::Call {
            url,
            method,
            headers,
            body,
            agent,
            timeout_secs,
            json,
        } => cmd_call(
            cwd,
            url,
            method,
            headers,
            body,
            agent,
            timeout_secs,
            json,
            env,
        ),
        VaultCmd::Connectors { json } => cmd_connectors(cwd, json),
        VaultCmd::ImportOpenapi {
            spec_path,
            id,
            secret,
            out,
        } => cmd_import_openapi(cwd, &spec_path, &id, secret.as_deref(), out.as_deref()),
        VaultCmd::Mcp { connector } => cmd_mcp(cwd, connector.as_deref(), env),
        VaultCmd::Install { user, json } => cmd_install(cwd, user, json),
    }
}

fn cmd_init(as_json: bool) -> Result<i32, (i32, String)> {
    let key_path = resolve_master_key_path(None);
    let vault_path = resolve_vault_path(None);
    let master_key = load_or_init_master_key(&key_path).map_err(|e| (1, e.to_string()))?;
    let vault = Vault::open(&vault_path, master_key).map_err(|e| (1, e.to_string()))?;
    // Write only a vault that does not exist yet, under the vault lock. A plain `save` would
    // replace the file with the view read above, losing a secret set in between.
    let created = vault.save_if_absent().map_err(|e| (1, e.to_string()))?;
    let vault = if created {
        vault
    } else {
        // The file existed, possibly changed since the open above: report what is on disk.
        let master_key = load_or_init_master_key(&key_path).map_err(|e| (1, e.to_string()))?;
        Vault::open(&vault_path, master_key).map_err(|e| (1, e.to_string()))?
    };

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "initialized": true,
                "vault_path": vault_path,
                "key_path": key_path,
                "secrets_count": vault.list().len(),
            }))
            .unwrap_or_default()
        );
    } else {
        println!(
            "Initialized FerroGate vault at {} (master key: {})",
            vault_path.display(),
            key_path.display()
        );
    }
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
fn cmd_set(
    name: String,
    value: Option<String>,
    from_stdin: bool,
    description: Option<String>,
    allowed_hosts: Vec<String>,
    allowed_methods: Vec<String>,
    allowed_agents: Vec<String>,
    allow_private_network: bool,
    ttl_secs: Option<u64>,
    usage: String,
    issuer: Option<String>,
    origin: Option<String>,
    bundle_required: bool,
    as_json: bool,
) -> Result<i32, (i32, String)> {
    let usage = match usage.as_str() {
        "proxy" => SecretUsage::Proxy,
        "sign-jwt-rs256" => SecretUsage::SignJwtRs256,
        _ => return Err((2, "usage must be proxy or sign-jwt-rs256".into())),
    };
    if usage == SecretUsage::SignJwtRs256 && (!from_stdin || value.is_some()) {
        return Err((
            2,
            "signing secrets require --stdin; --value is not permitted".into(),
        ));
    }
    let app_binding = match (usage, issuer, origin) {
        (SecretUsage::SignJwtRs256, Some(issuer), Some(origin)) => {
            Some(AppBinding { issuer, origin })
        }
        (SecretUsage::Proxy, None, None) => None,
        _ => {
            return Err((
                2,
                "signing secrets require --issuer and --origin; proxy secrets cannot set them"
                    .into(),
            ));
        }
    };
    let secret_val = match (value, from_stdin) {
        (Some(v), false) => v,
        (None, true) => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| (1, format!("failed to read secret from stdin: {e}")))?;
            buf.trim_end_matches(['\r', '\n']).to_string()
        }
        (Some(_), true) => {
            return Err((
                2,
                "pass either `--value` or `--stdin`, not both".to_string(),
            ));
        }
        (None, false) => {
            return Err((2, "provide `--value <SECRET>` or `--stdin`".to_string()));
        }
    };

    let mut vault = Vault::open_default(None).map_err(|e| (1, e.to_string()))?;
    let meta = vault
        .set(SetSecretArgs {
            name,
            value: secret_val,
            description,
            allowed_hosts,
            allowed_methods,
            allowed_agents,
            allow_private_network,
            lease_ttl_secs: ttl_secs,
            usage,
            bundle_required,
            app_binding,
        })
        .map_err(|e| (1, e.to_string()))?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&meta).unwrap_or_default()
        );
    } else {
        println!(
            "Stored encrypted secret `{}` (allowed_hosts: {:?}, allowed_methods: {:?})",
            meta.name, meta.allowed_hosts, meta.allowed_methods
        );
    }
    Ok(0)
}

fn cmd_list(as_json: bool) -> Result<i32, (i32, String)> {
    let vault = Vault::open_default(None).map_err(|e| (1, e.to_string()))?;
    let items = vault.list();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&items).unwrap_or_default()
        );
    } else if items.is_empty() {
        println!("No secrets stored in FerroGate vault.");
    } else {
        for item in items {
            println!(
                "{}\thosts={:?}\tmethods={:?}\tagents={:?}\tupdated={}",
                item.name,
                item.allowed_hosts,
                item.allowed_methods,
                item.allowed_agents,
                item.updated_at
            );
        }
    }
    Ok(0)
}

fn cmd_remove(name: &str, as_json: bool) -> Result<i32, (i32, String)> {
    let mut vault = Vault::open_default(None).map_err(|e| (1, e.to_string()))?;
    let removed = vault.remove(name).map_err(|e| (1, e.to_string()))?;
    if !removed {
        return Err((1, format!("secret `{name}` not found in vault")));
    }
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "removed": true, "name": name }))
                .unwrap_or_default()
        );
    } else {
        println!("Removed secret `{name}` from FerroGate vault.");
    }
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
fn cmd_call(
    cwd: &Path,
    url: String,
    method: String,
    raw_headers: Vec<String>,
    body: Option<String>,
    agent: Option<String>,
    timeout_secs: u64,
    as_json: bool,
    env: Option<&crate::cmd::state::StateEnv<'_>>,
) -> Result<i32, (i32, String)> {
    let mut headers = BTreeMap::new();
    for h in raw_headers {
        let Some((k, v)) = h.split_once(':') else {
            return Err((
                2,
                format!("invalid header `{h}` (expected `Header-Name: value`)"),
            ));
        };
        headers.insert(k.trim().to_string(), v.trim().to_string());
    }

    let caller_agent = agent.or_else(|| std::env::var("AGENT_PERSONA").ok());
    let vault = Vault::open_default(None).map_err(|e| (1, e.to_string()))?;
    let req = ProxyRequest {
        method,
        url,
        headers,
        body,
        caller_agent,
        timeout_secs: Some(timeout_secs),
    };
    // Raw proxy only: no connector tool is named, so no response contract
    // applies — `fl vault call` is Uncontracted by definition (Record #96).
    // It is still a governed dispatch: admit first, reserving the
    // worst-case `LEGACY_MAX_RESPONSE_BYTES` (settlement is capped at the
    // reserve). When the CLI store cannot open, the governor has no store
    // and dispatch is refused with `admission_unavailable`, never a
    // legacy fallback.
    let governor = cli_gate_governor(env);
    let (host, path) = match fl_gate::parse_url_scheme_host_port(&req.url) {
        Ok((_, h, _)) => {
            let after = req.url.split_once("://").map(|(_, r)| r).unwrap_or("");
            let p = after
                .find('/')
                .map(|i| {
                    after[i..]
                        .split(['?', '#'])
                        .next()
                        .unwrap_or("/")
                        .to_string()
                })
                .unwrap_or_else(|| "/".to_string());
            (h, p)
        }
        Err(_) => (String::new(), req.url.clone()),
    };
    // Canonicalise against discovered connector aliases so a raw call to
    // a connector endpoint shares that endpoint's budget scopes.
    let connectors = discover_connectors(Some(cwd), None).unwrap_or_default();
    let aliases = fl_gate::connector_endpoint_aliases(&connectors);
    let endpoint =
        fl_core::governor::resolve_canonical_endpoint(&aliases, None, None, &host, &path);
    let reserved = fl_gate::LEGACY_MAX_RESPONSE_BYTES;
    let mut adm_req = AdmissionRequest::new(Principal::Owner, &host, &endpoint, reserved);
    adm_req.caller_agent = req.caller_agent.clone().unwrap_or_default();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let vault_ref = &vault;
    let req_ref = &req;
    let resp = match governor.governed_dispatch(&adm_req, now_ms, |permit| {
        // Shared helper re-checks the deadline with a fresh now after prepare.
        match execute_proxied_request_with_provider_and_contract_phased_with_deadline(
            vault_ref,
            req_ref,
            None,
            None,
            Some(permit.deadline_mono_ms),
            &|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            },
        ) {
            Ok(r) => {
                let actual = r.body.len() as u64;
                DispatchResult::Executed(r, actual)
            }
            Err(DispatchPhaseError::BeforeSend(e)) => DispatchResult::RefusedBeforeDispatch(e),
            Err(DispatchPhaseError::AfterSend(e)) => DispatchResult::FailedAfterSend(e),
        }
    }) {
        Ok((r, _permit)) => r,
        Err(GovernedOrGateError::Refused(outcome)) => {
            return Err((1, admission_refusal_text(&outcome)));
        }
        Err(GovernedOrGateError::Gate(e)) => return Err((1, e.to_string())),
    };
    if as_json {
        // Structured raw-call output carries the same `outcome` flag as the
        // hub raw gate call: this surface is uncontracted (Record #96).
        let mut out = serde_json::to_value(&resp).unwrap_or_default();
        if let Some(obj) = out.as_object_mut() {
            obj.insert("outcome".to_string(), json!("uncontracted"));
        }
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        print!("{}", resp.body);
        if !resp.body.ends_with('\n') {
            println!();
        }
    }
    if resp.status >= 400 { Ok(1) } else { Ok(0) }
}

fn cmd_connectors(cwd: &Path, as_json: bool) -> Result<i32, (i32, String)> {
    let connectors = discover_connectors(Some(cwd), None).map_err(|e| (1, e.to_string()))?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&connectors).unwrap_or_default()
        );
    } else if connectors.is_empty() {
        println!("No FerroGate connectors discovered in .fl/gate/ or ~/.config/fl/gate/.");
    } else {
        for c in connectors {
            println!(
                "{} ({}) — {} tools, {} bridges [{}]",
                c.id,
                c.title,
                c.tools.len(),
                c.bridges.len(),
                if c.enabled { "enabled" } else { "disabled" }
            );
        }
    }
    Ok(0)
}

fn cmd_import_openapi(
    cwd: &Path,
    spec_path: &Path,
    id: &str,
    secret: Option<&str>,
    out_opt: Option<&Path>,
) -> Result<i32, (i32, String)> {
    let raw_json = fs::read_to_string(spec_path)
        .map_err(|e| (1, format!("failed to read {}: {e}", spec_path.display())))?;
    let manifest =
        import_openapi_to_connector(&raw_json, id, secret).map_err(|e| (1, e.to_string()))?;
    let toml_text =
        toml::to_string_pretty(&manifest).map_err(|e| (1, format!("TOML encode error: {e}")))?;

    let target_path = out_opt.map(Path::to_path_buf).unwrap_or_else(|| {
        cwd.join(".fl")
            .join("gate")
            .join(format!("{}.toml", manifest.id))
    });
    if let Some(parent) = target_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| (1, format!("failed to create {}: {e}", parent.display())))?;
    }
    fs::write(&target_path, toml_text)
        .map_err(|e| (1, format!("failed to write {}: {e}", target_path.display())))?;

    println!(
        "Imported OpenAPI connector `{}` ({} tools) to {}",
        manifest.id,
        manifest.tools.len(),
        target_path.display()
    );
    Ok(0)
}

fn cli_gate_governor(env: Option<&crate::cmd::state::StateEnv<'_>>) -> GateGovernor {
    // Open the CLI's configured store the way other CLI state commands do
    // (`StateEnv::open_for_scope(None)` — the default `fl state` store).
    // Failure yields a governor with no store, which refuses governed
    // dispatch with `admission_unavailable`, never a legacy fallback.
    // Owner policy channel: FL_GATE_APPROVED_POLICY_DIGESTS (empty = no failover).
    let approved = approved_policy_digests_from_env();
    let Some(env) = env else {
        return GateGovernor::none().with_approved_policy_digests(approved);
    };
    match env.open_for_scope(None) {
        Ok(store) => GateGovernor::new(std::sync::Arc::new(
            fl_store::PersistentGovernorStore::new(store),
        ))
        .with_approved_policy_digests(approved),
        Err(_) => GateGovernor::none().with_approved_policy_digests(approved),
    }
}

fn cmd_mcp(
    cwd: &Path,
    connector_filter: Option<&str>,
    env: Option<&crate::cmd::state::StateEnv<'_>>,
) -> Result<i32, (i32, String)> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let caller_agent = std::env::var("AGENT_PERSONA").ok();
    let governor = cli_gate_governor(env);

    for line_res in stdin.lock().lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(req_val) = serde_json::from_str::<Value>(trimmed) else {
            let err_reply = json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": "Parse error" }
            });
            let _ = writeln!(stdout, "{err_reply}");
            let _ = stdout.flush();
            continue;
        };

        let vault = match Vault::open_default(None) {
            Ok(v) => v,
            Err(e) => {
                let err_reply = json!({
                    "jsonrpc": "2.0",
                    "id": req_val.get("id").cloned().unwrap_or(Value::Null),
                    "error": { "code": -32603, "message": e.to_string() }
                });
                let _ = writeln!(stdout, "{err_reply}");
                let _ = stdout.flush();
                continue;
            }
        };
        let connectors = discover_connectors(Some(cwd), None).unwrap_or_default();
        // Governed MCP: connector tools go through the governor (store
        // opened via StateEnv above; project binds to the connector id
        // inside the MCP layer when no explicit project is supplied).
        if let Some(reply) = handle_mcp_jsonrpc_request_with_governor_and_secret_provider(
            &req_val,
            &vault,
            &connectors,
            connector_filter,
            caller_agent.clone(),
            Some(&governor),
            &Principal::Owner,
            "",
            None,
        ) {
            let _ = writeln!(stdout, "{reply}");
            let _ = stdout.flush();
        }
    }
    Ok(0)
}

fn cmd_install(cwd: &Path, user_level: bool, as_json: bool) -> Result<i32, (i32, String)> {
    run_mcp_install(
        cwd,
        user_level,
        as_json,
        "ferrogate",
        &["vault", "mcp"],
        "unified `ferrogate` MCP server",
    )
}

/// Register one MCP server (`server_name`, invoked as `fl <args...>`)
/// across the vendor CLI configs, project-scoped under `cwd` or at user
/// level under `$HOME`. `display_label` only feeds the human summary line
/// (`Registered {display_label} (`<fl> <args>`) in N files.`); the JSON
/// summary and the written files are the same shape for every caller.
pub(crate) fn run_mcp_install(
    cwd: &Path,
    user_level: bool,
    as_json: bool,
    server_name: &str,
    args: &[&str],
    display_label: &str,
) -> Result<i32, (i32, String)> {
    let fl_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "fl".to_string());

    let mut updated_files = Vec::new();

    if user_level {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| (1, "$HOME is not set".to_string()))?;

        for rel in [
            ".claude.json",
            ".gemini/antigravity-cli/mcp_config.json",
            ".config/muse/settings.json",
        ] {
            let path = home.join(rel);
            upsert_json_mcp_server(&path, &fl_bin, server_name, args)?;
            updated_files.push(path.display().to_string());
        }
        let codex_path = home.join(".codex/config.toml");
        upsert_toml_mcp_server(
            &codex_path,
            "mcp_servers",
            &fl_bin,
            false,
            server_name,
            args,
        )?;
        updated_files.push(codex_path.display().to_string());
    } else {
        let fl_mcp_path = cwd.join(".fl/mcp.toml");
        upsert_toml_mcp_server(&fl_mcp_path, "server", &fl_bin, true, server_name, args)?;
        updated_files.push(fl_mcp_path.display().to_string());

        for rel in [
            ".mcp.json",
            ".agents/mcp_config.json",
            ".muse/settings.json",
        ] {
            let path = cwd.join(rel);
            upsert_json_mcp_server(&path, &fl_bin, server_name, args)?;
            updated_files.push(path.display().to_string());
        }
        let codex_path = cwd.join(".codex/config.toml");
        upsert_toml_mcp_server(
            &codex_path,
            "mcp_servers",
            &fl_bin,
            false,
            server_name,
            args,
        )?;
        updated_files.push(codex_path.display().to_string());
    }

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "installed": true,
                "scope": if user_level { "user" } else { "project" },
                "updated_files": updated_files,
            }))
            .unwrap_or_default()
        );
    } else {
        println!(
            "Registered {display_label} (`{} {}`) in {} files.",
            fl_bin,
            args.join(" "),
            updated_files.len()
        );
    }
    Ok(0)
}

fn upsert_json_mcp_server(
    path: &Path,
    fl_bin: &str,
    server_name: &str,
    args: &[&str],
) -> Result<(), (i32, String)> {
    let mut root_obj = if path.is_file() {
        let raw = fs::read_to_string(path)
            .map_err(|e| (1, format!("failed to read {}: {e}", path.display())))?;
        serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    if !root_obj.is_object() {
        root_obj = json!({});
    }
    let map = root_obj.as_object_mut().expect("object");
    let servers = map.entry("mcpServers").or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers.as_object_mut().expect("object").insert(
        server_name.to_string(),
        json!({
            "command": fl_bin,
            "args": args
        }),
    );

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| (1, format!("failed to create {}: {e}", parent.display())))?;
    }
    let formatted = serde_json::to_string_pretty(&root_obj)
        .map_err(|e| (1, format!("JSON serialize error: {e}")))?;
    fs::write(path, format!("{formatted}\n"))
        .map_err(|e| (1, format!("failed to write {}: {e}", path.display())))?;
    Ok(())
}

fn upsert_toml_mcp_server(
    path: &Path,
    section_key: &str,
    fl_bin: &str,
    include_transport: bool,
    server_name: &str,
    args: &[&str],
) -> Result<(), (i32, String)> {
    let mut doc = if path.is_file() {
        let raw = fs::read_to_string(path)
            .map_err(|e| (1, format!("failed to read {}: {e}", path.display())))?;
        toml::from_str::<toml::Value>(&raw)
            .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()))
    } else {
        toml::Value::Table(toml::map::Map::new())
    };

    if !doc.is_table() {
        doc = toml::Value::Table(toml::map::Map::new());
    }
    let root_tbl = doc.as_table_mut().expect("table");
    let section_entry = root_tbl
        .entry(section_key.to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    if !section_entry.is_table() {
        *section_entry = toml::Value::Table(toml::map::Map::new());
    }
    let mut server_tbl = toml::map::Map::new();
    if include_transport {
        server_tbl.insert("enabled".to_string(), toml::Value::Boolean(true));
        server_tbl.insert(
            "transport".to_string(),
            toml::Value::String("stdio".to_string()),
        );
    }
    server_tbl.insert(
        "command".to_string(),
        toml::Value::String(fl_bin.to_string()),
    );
    server_tbl.insert(
        "args".to_string(),
        toml::Value::Array(
            args.iter()
                .map(|a| toml::Value::String((*a).to_string()))
                .collect(),
        ),
    );
    section_entry
        .as_table_mut()
        .expect("table")
        .insert(server_name.to_string(), toml::Value::Table(server_tbl));

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| (1, format!("failed to create {}: {e}", parent.display())))?;
    }
    let formatted =
        toml::to_string_pretty(&doc).map_err(|e| (1, format!("TOML serialize error: {e}")))?;
    fs::write(path, formatted)
        .map_err(|e| (1, format!("failed to write {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_secret_rejects_value_before_opening_vault() {
        let err = cmd_set(
            "KEY".into(),
            Some("do-not-echo".into()),
            false,
            None,
            vec![],
            vec![],
            vec![],
            false,
            None,
            "sign-jwt-rs256".into(),
            Some("123".into()),
            Some("https://api.github.com".into()),
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(err.0, 2);
        assert!(err.1.contains("--stdin"));
        assert!(!err.1.contains("do-not-echo"));
    }
}
