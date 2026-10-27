//! `fl vault` (alias `fl gateway`) CLI subcommands for managing `FerroGate`:
//! zero-knowledge AES-256-GCM secret vault, credential proxy (`fl vault call`),
//! declarative API-to-MCP connectors, OpenAPI importer, unified MCP server (`fl vault mcp`),
//! and single-registration vendor installation (`fl vault install`) (spec §6.1).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use clap::Subcommand;
use fl_gate::{
    ProxyRequest, SetSecretArgs, Vault, discover_connectors, execute_proxied_request,
    handle_mcp_jsonrpc_request, import_openapi_to_connector, load_or_init_master_key,
    resolve_master_key_path, resolve_vault_path,
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

pub fn run(cwd: &Path, cmd: VaultCmd) -> Result<i32, (i32, String)> {
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
        } => cmd_call(url, method, headers, body, agent, timeout_secs, json),
        VaultCmd::Connectors { json } => cmd_connectors(cwd, json),
        VaultCmd::ImportOpenapi {
            spec_path,
            id,
            secret,
            out,
        } => cmd_import_openapi(cwd, &spec_path, &id, secret.as_deref(), out.as_deref()),
        VaultCmd::Mcp { connector } => cmd_mcp(cwd, connector.as_deref()),
        VaultCmd::Install { user, json } => cmd_install(cwd, user, json),
    }
}

fn cmd_init(as_json: bool) -> Result<i32, (i32, String)> {
    let key_path = resolve_master_key_path(None);
    let vault_path = resolve_vault_path(None);
    let master_key = load_or_init_master_key(&key_path).map_err(|e| (1, e.to_string()))?;
    let vault = Vault::open(&vault_path, master_key).map_err(|e| (1, e.to_string()))?;
    vault.save().map_err(|e| (1, e.to_string()))?;

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
    as_json: bool,
) -> Result<i32, (i32, String)> {
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

fn cmd_call(
    url: String,
    method: String,
    raw_headers: Vec<String>,
    body: Option<String>,
    agent: Option<String>,
    timeout_secs: u64,
    as_json: bool,
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
    let resp = execute_proxied_request(&vault, &req).map_err(|e| (1, e.to_string()))?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
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

fn cmd_mcp(cwd: &Path, connector_filter: Option<&str>) -> Result<i32, (i32, String)> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let caller_agent = std::env::var("AGENT_PERSONA").ok();

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
        if let Some(reply) = handle_mcp_jsonrpc_request(
            &req_val,
            &vault,
            &connectors,
            connector_filter,
            caller_agent.clone(),
        ) {
            let _ = writeln!(stdout, "{reply}");
            let _ = stdout.flush();
        }
    }
    Ok(0)
}

fn cmd_install(cwd: &Path, user_level: bool, as_json: bool) -> Result<i32, (i32, String)> {
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
            upsert_json_mcp_server(&path, &fl_bin)?;
            updated_files.push(path.display().to_string());
        }
        let codex_path = home.join(".codex/config.toml");
        upsert_toml_mcp_server(&codex_path, "mcp_servers", &fl_bin, false)?;
        updated_files.push(codex_path.display().to_string());
    } else {
        let fl_mcp_path = cwd.join(".fl/mcp.toml");
        upsert_toml_mcp_server(&fl_mcp_path, "server", &fl_bin, true)?;
        updated_files.push(fl_mcp_path.display().to_string());

        for rel in [
            ".mcp.json",
            ".agents/mcp_config.json",
            ".muse/settings.json",
        ] {
            let path = cwd.join(rel);
            upsert_json_mcp_server(&path, &fl_bin)?;
            updated_files.push(path.display().to_string());
        }
        let codex_path = cwd.join(".codex/config.toml");
        upsert_toml_mcp_server(&codex_path, "mcp_servers", &fl_bin, false)?;
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
            "Registered unified `ferrogate` MCP server (`{} vault mcp`) in {} files.",
            fl_bin,
            updated_files.len()
        );
    }
    Ok(0)
}

fn upsert_json_mcp_server(path: &Path, fl_bin: &str) -> Result<(), (i32, String)> {
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
        "ferrogate".to_string(),
        json!({
            "command": fl_bin,
            "args": ["vault", "mcp"]
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
        toml::Value::Array(vec![
            toml::Value::String("vault".to_string()),
            toml::Value::String("mcp".to_string()),
        ]),
    );
    section_entry
        .as_table_mut()
        .expect("table")
        .insert("ferrogate".to_string(), toml::Value::Table(server_tbl));

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
