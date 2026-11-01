//! `fl hub` CLI subcommands (`serve`, `pair`, `token`, `status`, `init`,
//! `enroll-token`, `join`, `nodes`) for running and managing the FerroLoop Hub
//! daemon and `FerroDeck` web command center (spec §4.3).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;
use fl_core::pr_policy::DEFAULT_OWNER_LOGIN;
use fl_hub::{
    DEFAULT_HUB_LISTEN, DEFAULT_PAIRING_TTL_SECS, DEFAULT_SESSION_TTL_SECS, EnsureHubArgs,
    HubServer, HubServerConfig, NodeView, canonical_node_id, detect_capabilities, ensure_hub_node,
    find_hub_node, generate_hex_token, list_visible_nodes, local_hostname, mint_enroll_token,
    mint_pairing_code_in_store, node_id_from_hostname, resolve_owner_token, resolve_peer_token,
};
use fl_store::{AnyStore, StoreOptions};
use serde_json::{Value, json};

use crate::cmd::agent::write_atomic_non_symlink;
use crate::cmd::state::StateEnv;

#[derive(Subcommand)]
pub enum HubCmd {
    /// Start the FerroLoop Hub HTTP/1.1 & WebSocket server (`fl-hub`).
    Serve {
        /// TCP listen address (defaults to `[hub].listen` or `127.0.0.1:8420`).
        #[arg(long)]
        listen: Option<String>,
        /// Static `FerroDeck` UI directory (defaults to `[hub].ui_dir` or `<cwd>/ui/dist`).
        #[arg(long)]
        ui_dir: Option<PathBuf>,
        /// File containing the Hub owner token (overrides `[hub].owner_token_file`).
        #[arg(long)]
        owner_token_file: Option<PathBuf>,
        /// Trust `Tailscale-User-Login` headers from `tailscale serve`.
        #[arg(long)]
        trust_tailscale_headers: bool,
        /// Disable interactive Owner terminal sessions on `/ws/v1`.
        #[arg(long)]
        no_terminal: bool,
    },
    /// Mint a single-use 10-minute pairing code and URL in the workspace store.
    Pair {
        /// Base URL of the Hub server (defaults to `http://<listen>`).
        #[arg(long)]
        url: Option<String>,
        /// Pairing code time-to-live in seconds (defaults to 600).
        #[arg(long, default_value_t = DEFAULT_PAIRING_TTL_SECS)]
        ttl_secs: u64,
        /// Format output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Generate or inspect a Hub owner bearer token.
    Token {
        /// Generate a fresh 256-bit hex owner token.
        #[arg(long)]
        generate: bool,
        /// Optional file path to write the token to (with `0600` permissions on Unix).
        #[arg(long, visible_alias = "out")]
        output: Option<PathBuf>,
        /// Format output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Query health and session status of a running FerroLoop Hub.
    Status {
        /// Base URL of the Hub server (defaults to `http://<listen>`).
        #[arg(long)]
        url: Option<String>,
        /// Format output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Write hub config, tokens, and the primary hub node.
    Init {
        /// TCP listen address (defaults to the configured address or `127.0.0.1:8420`).
        #[arg(long)]
        listen: Option<String>,
        /// Hub node id (defaults to the existing hub node or the local hostname).
        #[arg(long)]
        name: Option<String>,
        /// Write a user service unit for `fl hub serve` without starting it.
        #[arg(long)]
        service: bool,
        /// Format output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Mint a one-time worker enrollment code in the workspace store.
    EnrollToken {
        /// Label included in the printed join command.
        #[arg(long)]
        label: Option<String>,
        /// Base URL of the Hub server (defaults to `http://<listen>`).
        #[arg(long)]
        url: Option<String>,
        /// Enrollment code time-to-live in seconds (defaults to 600).
        #[arg(long, default_value_t = 600)]
        ttl_secs: u64,
        /// Format output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Enroll this machine with a hub using a one-time code.
    Join {
        /// Base URL of the hub to join.
        hub_url: String,
        /// One-time enrollment code.
        #[arg(long)]
        token: String,
        /// Node id (defaults to the local hostname).
        #[arg(long)]
        name: Option<String>,
        /// Label stored on the enrolled node. Repeat to set more than one.
        #[arg(long)]
        label: Vec<String>,
        /// Format output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List hub and worker nodes from the store or a running hub.
    Nodes {
        /// Base URL of a running hub. Omit to read the local store.
        #[arg(long)]
        url: Option<String>,
        /// Format output as JSON.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(env: &StateEnv<'_>, cmd: HubCmd) -> Result<i32, (i32, String)> {
    match cmd {
        HubCmd::Serve {
            listen,
            ui_dir,
            owner_token_file,
            trust_tailscale_headers,
            no_terminal,
        } => cmd_serve(
            env,
            listen,
            ui_dir,
            owner_token_file,
            trust_tailscale_headers,
            no_terminal,
        ),
        HubCmd::Pair {
            url,
            ttl_secs,
            json,
        } => cmd_pair(env, url, ttl_secs, json),
        HubCmd::Token {
            generate,
            output,
            json,
        } => cmd_token(env, generate, output.as_deref(), json),
        HubCmd::Status { url, json } => cmd_status(env, url, json),
        HubCmd::Init {
            listen,
            name,
            service,
            json,
        } => cmd_init(env, listen, name, service, json),
        HubCmd::EnrollToken {
            label,
            url,
            ttl_secs,
            json,
        } => cmd_enroll_token(env, label, url, ttl_secs, json),
        HubCmd::Join {
            hub_url,
            token,
            name,
            label,
            json,
        } => cmd_join(hub_url, token, name, label, json),
        HubCmd::Nodes { url, json } => cmd_nodes(env, url, json),
    }
}

fn resolve_hub_store_path(env: &StateEnv<'_>) -> PathBuf {
    if let Some(db) = env.explicit_db {
        return db.to_path_buf();
    }
    if let Some(st) = env.cfg.state.as_ref().and_then(|s| s.store.clone()) {
        return st;
    }
    if let Ok(Some(entry)) = crate::config::bound_entry(&env.cfg.projects, env.cwd) {
        return entry.store;
    }
    env.cfg.workspace_state_store(env.default_store)
}

/// Open the hub's store on whichever engine the path and `FL_STORE_BACKEND` select.
fn open_hub_store_with_retry(store_path: &Path) -> Result<AnyStore, (i32, String)> {
    open_hub_store_with_options(store_path, &StoreOptions::default())
}

/// [`open_hub_store_with_retry`] with explicit [`StoreOptions`]: the hub's private-directory
/// creation, then up to 40 tries at the open, 5 ms apart. A test passes the engine here instead
/// of changing the process environment.
fn open_hub_store_with_options(
    store_path: &Path,
    options: &StoreOptions,
) -> Result<AnyStore, (i32, String)> {
    if let Some(dir) = store_path.parent()
        && !dir.as_os_str().is_empty()
    {
        fl_store::private::create_private_dir_all(dir)
            .map_err(|e| (2, format!("{}: {e}", dir.display())))?;
    }
    let mut last_err = None;
    for attempt in 0..40 {
        match fl_store::open_store_with(store_path, options) {
            Ok(store) => return Ok(store),
            Err(e) => {
                last_err = Some(e);
                if attempt < 39 {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
    Err((
        2,
        format!(
            "could not open the store at {}: {}",
            store_path.display(),
            last_err.expect("at least one attempt ran")
        ),
    ))
}

fn resolve_listen_addr(env: &StateEnv<'_>, listen_override: Option<String>) -> String {
    listen_override
        .or_else(|| env.cfg.hub.as_ref().and_then(|h| h.listen.clone()))
        .unwrap_or_else(|| DEFAULT_HUB_LISTEN.to_string())
}

fn resolve_base_url(env: &StateEnv<'_>, url_override: Option<String>) -> String {
    if let Some(u) = url_override {
        let trimmed = u.trim().trim_end_matches('/').to_string();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return trimmed;
        }
        return format!("http://{trimmed}");
    }
    let listen = resolve_listen_addr(env, None);
    format!("http://{}", listen.trim().trim_end_matches('/'))
}

fn cmd_serve(
    env: &StateEnv<'_>,
    listen_flag: Option<String>,
    ui_dir_flag: Option<PathBuf>,
    owner_token_file_flag: Option<PathBuf>,
    trust_tailscale_flag: bool,
    no_terminal_flag: bool,
) -> Result<i32, (i32, String)> {
    // The service log is not a terminal, but it is where a loose store should be recorded: ask for
    // the store-permission warning on every start, before any store is opened.
    // `FL_QUIET_STORE_PERMS` still silences it.
    fl_store::private::warn_always();
    let hub_cfg_sec = env.cfg.hub.as_ref();
    let listen = resolve_listen_addr(env, listen_flag);
    let store_path = resolve_hub_store_path(env);
    let repo_root = env.cwd.to_path_buf();

    let ui_dir = ui_dir_flag
        .map(|p| if p.is_absolute() { p } else { env.cwd.join(p) })
        .or_else(|| hub_cfg_sec.and_then(|h| h.ui_dir.clone()))
        .or_else(|| {
            let candidate = env.cwd.join("ui").join("dist");
            candidate.is_dir().then_some(candidate)
        });

    let owner_token_file = owner_token_file_flag
        .map(|p| if p.is_absolute() { p } else { env.cwd.join(p) })
        .or_else(|| hub_cfg_sec.and_then(|h| h.owner_token_file.clone()));
    let owner_token = resolve_owner_token(owner_token_file.as_deref());

    let wire_token_file = env.cfg.wire.as_ref().and_then(|w| w.token_file.as_deref());
    let peer_token = resolve_peer_token(wire_token_file);

    let trust_tailscale_headers =
        trust_tailscale_flag || hub_cfg_sec.is_some_and(|h| h.trust_tailscale_headers);
    let enable_terminal = if no_terminal_flag {
        false
    } else {
        hub_cfg_sec.and_then(|h| h.enable_terminal).unwrap_or(true)
    };
    let session_ttl_secs = hub_cfg_sec
        .and_then(|h| h.session_ttl_hours)
        .map(|hrs| hrs.saturating_mul(3600))
        .unwrap_or(DEFAULT_SESSION_TTL_SECS);

    let server_config = HubServerConfig {
        listen,
        store_path,
        repo_root,
        ui_dir,
        owner_login: DEFAULT_OWNER_LOGIN.to_string(),
        reviewer_apps: crate::config::reviewer_apps(env.cfg.github.as_ref()),
        owner_token,
        peer_token,
        trust_tailscale_headers,
        enable_terminal,
        session_ttl_secs,
        home_override: None,
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| (2, format!("failed to initialize tokio runtime: {e}")))?;

    rt.block_on(async move {
        let server = HubServer::bind(server_config)
            .await
            .map_err(|e| (2, format!("failed to bind Hub server: {e}")))?;
        let addr = server.local_addr();
        println!("ferroloop-hub listening on http://{addr}");
        let _ = std::io::stdout().flush();
        server
            .serve()
            .await
            .map_err(|e| (2, format!("Hub server error: {e}")))?;
        Ok(0)
    })
}

fn cmd_pair(
    env: &StateEnv<'_>,
    url_override: Option<String>,
    ttl_secs: u64,
    json_out: bool,
) -> Result<i32, (i32, String)> {
    let base_url = resolve_base_url(env, url_override);
    let store_path = resolve_hub_store_path(env);
    let store = open_hub_store_with_retry(&store_path)?;
    let info = mint_pairing_code_in_store(&store, &base_url, ttl_secs).map_err(|e| (2, e))?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&info)
                .map_err(|e| (2, format!("failed to serialize pairing info: {e}")))?
        );
    } else {
        println!("Pairing code: {}", info.code);
        println!("Pair URL:     {}", info.pair_url);
        println!("Expires in:   {}s", info.ttl_secs);
    }
    Ok(0)
}

fn cmd_token(
    env: &StateEnv<'_>,
    generate: bool,
    output_opt: Option<&Path>,
    json_out: bool,
) -> Result<i32, (i32, String)> {
    let configured_file = env
        .cfg
        .hub
        .as_ref()
        .and_then(|h| h.owner_token_file.as_deref());
    let (token, source) = if !generate && let Some(existing) = resolve_owner_token(configured_file)
    {
        (existing, "configured")
    } else {
        (generate_hex_token(32), "generated")
    };

    let written_path = if let Some(out_rel) = output_opt {
        let out_path = if out_rel.is_absolute() {
            out_rel.to_path_buf()
        } else {
            env.cwd.join(out_rel)
        };
        write_atomic_non_symlink(&out_path, format!("{token}\n").as_bytes()).map_err(|e| (2, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(0o600));
        }
        Some(out_path.display().to_string())
    } else {
        None
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "token": token,
                "source": source,
                "output": written_path,
            }))
            .unwrap()
        );
    } else {
        println!("{token}");
    }
    Ok(0)
}

fn cmd_status(
    env: &StateEnv<'_>,
    url_override: Option<String>,
    json_out: bool,
) -> Result<i32, (i32, String)> {
    let base_url = resolve_base_url(env, url_override);
    let health_url = format!("{base_url}/api/v1/health");

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .http_status_as_error(false)
        .build()
        .into();

    let health_resp = match agent.get(&health_url).call() {
        Ok(r) => r,
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "url": base_url,
                        "reachable": false,
                        "error": e.to_string(),
                    }))
                    .unwrap()
                );
                return Ok(1);
            }
            return Err((1, format!("hub at {base_url} is unreachable: {e}")));
        }
    };

    let status_code = health_resp.status().as_u16();
    let health_body_str = health_resp
        .into_body()
        .read_to_string()
        .unwrap_or_else(|_| "{}".to_string());
    let health_json: Value = serde_json::from_str(&health_body_str)
        .unwrap_or_else(|_| json!({ "raw": health_body_str }));

    if status_code != 200 {
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "url": base_url,
                    "reachable": false,
                    "status": status_code,
                    "health": health_json,
                }))
                .unwrap()
            );
            return Ok(1);
        }
        return Err((
            1,
            format!("hub health check at {health_url} returned HTTP {status_code}"),
        ));
    }

    let owner_token_file = env
        .cfg
        .hub
        .as_ref()
        .and_then(|h| h.owner_token_file.as_deref());
    let wire_token_file = env.cfg.wire.as_ref().and_then(|w| w.token_file.as_deref());
    let bearer_tok =
        resolve_owner_token(owner_token_file).or_else(|| resolve_peer_token(wire_token_file));

    let mut session_json: Option<Value> = None;
    if let Some(tok) = bearer_tok {
        let sess_url = format!("{base_url}/api/v1/auth/session");
        if let Ok(resp) = agent
            .get(&sess_url)
            .header("Authorization", &format!("Bearer {tok}"))
            .call()
            && resp.status().as_u16() == 200
            && let Ok(body_str) = resp.into_body().read_to_string()
            && let Ok(parsed) = serde_json::from_str::<Value>(&body_str)
        {
            session_json = Some(parsed);
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "url": base_url,
                "reachable": true,
                "health": health_json,
                "authenticated": session_json.is_some(),
                "session": session_json,
            }))
            .unwrap()
        );
    } else {
        println!("Hub URL:       {base_url}");
        println!(
            "Health:        {}",
            health_json
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("ok")
        );
        if let Some(ref s) = session_json {
            let tier = s
                .pointer("/authority/tier")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            println!("Authenticated: yes (tier: {tier})");
        } else {
            println!("Authenticated: no");
        }
    }
    Ok(0)
}

fn now_ms() -> u64 {
    fl_exec::stamp::now().unix_millis()
}

fn path_env() -> String {
    std::env::var("PATH").unwrap_or_default()
}

fn absolute_path(cwd: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn config_file_path() -> Result<PathBuf, (i32, String)> {
    crate::config::path().ok_or_else(|| {
        (
            2,
            "neither an absolute XDG_CONFIG_HOME nor HOME is set, so there is nowhere to write the hub config".to_string(),
        )
    })
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn print_json(value: &Value) -> Result<(), (i32, String)> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| (2, format!("failed to serialize JSON: {e}")))?;
    println!("{text}");
    Ok(())
}

fn configured_bearer(env: &StateEnv<'_>) -> Option<String> {
    let owner_token_file = env
        .cfg
        .hub
        .as_ref()
        .and_then(|h| h.owner_token_file.as_deref());
    let wire_token_file = env.cfg.wire.as_ref().and_then(|w| w.token_file.as_deref());
    resolve_owner_token(owner_token_file).or_else(|| resolve_peer_token(wire_token_file))
}

fn ensure_table<'a>(
    doc: &'a mut toml::Value,
    key: &str,
) -> Result<&'a mut toml::Table, (i32, String)> {
    let root = doc
        .as_table_mut()
        .ok_or_else(|| (2, "fl config root is not a table".to_string()))?;
    match root.get(key) {
        Some(toml::Value::Table(_)) => {}
        Some(_) => {
            return Err((2, format!("`{key}` in the fl config is not a table")));
        }
        None => {
            root.insert(key.to_string(), toml::Value::Table(toml::Table::new()));
        }
    }
    match root.get_mut(key) {
        Some(toml::Value::Table(table)) => Ok(table),
        _ => Err((2, format!("`{key}` in the fl config is not a table"))),
    }
}

fn put_string(table: &mut toml::Table, key: &str, value: String) {
    table.insert(key.to_string(), toml::Value::String(value));
}

fn put_string_if_absent(table: &mut toml::Table, key: &str, value: String) {
    if !table.contains_key(key) {
        put_string(table, key, value);
    }
}

fn restrict_secret_file(path: &Path) -> Result<(), (i32, String)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| (2, format!("{}: {e}", path.display())))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn ensure_secret_file(path: &Path) -> Result<(), (i32, String)> {
    let keep = match fs::read_to_string(path) {
        Ok(text) => !text.trim().is_empty(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err((2, format!("{}: {e}", path.display()))),
    };
    if keep {
        return Ok(());
    }
    let token = generate_hex_token(32);
    write_atomic_non_symlink(path, format!("{token}\n").as_bytes()).map_err(|e| (2, e))?;
    restrict_secret_file(path)
}

fn load_config_value(path: &Path) -> Result<toml::Value, (i32, String)> {
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err((2, format!("{}: {e}", path.display()))),
    };
    if raw.trim().is_empty() {
        return Ok(toml::Value::Table(toml::Table::new()));
    }
    toml::from_str(&raw).map_err(|e| (2, format!("{} is not valid toml: {e}", path.display())))
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

fn existing_listen(doc: &toml::Value) -> Option<String> {
    doc.get("hub")
        .and_then(|hub| hub.get("listen"))
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|listen| !listen.is_empty())
        .map(str::to_string)
}

fn cmd_init(
    env: &StateEnv<'_>,
    listen_flag: Option<String>,
    name: Option<String>,
    service: bool,
    json_out: bool,
) -> Result<i32, (i32, String)> {
    let cfg_path = config_file_path()?;
    if let Some(dir) = cfg_path.parent()
        && !dir.as_os_str().is_empty()
    {
        fs::create_dir_all(dir).map_err(|e| (2, format!("{}: {e}", dir.display())))?;
    }
    let cfg_dir = cfg_path.parent().unwrap_or_else(|| Path::new("."));
    let owner_token_path = cfg_dir.join("hub-owner.token");
    let peer_token_path = cfg_dir.join("hub-peer.token");
    ensure_secret_file(&owner_token_path)?;
    ensure_secret_file(&peer_token_path)?;

    let mut doc = load_config_value(&cfg_path)?;
    let listen = listen_flag
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| existing_listen(&doc))
        .unwrap_or_else(|| DEFAULT_HUB_LISTEN.to_string());
    let store_path = absolute_path(env.cwd, resolve_hub_store_path(env));
    {
        let state = ensure_table(&mut doc, "state")?;
        put_string(state, "store", store_path.display().to_string());
    }
    {
        let hub = ensure_table(&mut doc, "hub")?;
        put_string(hub, "listen", listen.clone());
        put_string_if_absent(
            hub,
            "owner_token_file",
            owner_token_path.display().to_string(),
        );
    }
    {
        let wire = ensure_table(&mut doc, "wire")?;
        put_string_if_absent(wire, "token_file", peer_token_path.display().to_string());
    }
    let rendered = toml::to_string_pretty(&doc)
        .map_err(|e| (2, format!("could not encode fl config: {e}")))?;
    write_atomic_non_symlink(&cfg_path, rendered.as_bytes()).map_err(|e| (2, e))?;

    let (node_id, info) = {
        let store = open_hub_store_with_retry(&store_path)?;
        let hostname = local_hostname();
        let node_id = choose_hub_node_id(&store, name)?;
        let record = ensure_hub_node(
            &store,
            EnsureHubArgs {
                node_id,
                display_name: hostname.clone(),
                hostname,
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                listen_addr: Some(listen.clone()),
                capabilities: detect_capabilities(&path_env()),
                now_ms: now_ms(),
            },
        )
        .map_err(|e| (2, e))?;
        let info = mint_pairing_code_in_store(
            &store,
            &format!("http://{listen}"),
            DEFAULT_PAIRING_TTL_SECS,
        )
        .map_err(|e| (2, e))?;
        (record.node_id, info)
    };
    let service_file = if service {
        Some(install_user_service(env.cwd)?)
    } else {
        None
    };

    if json_out {
        print_json(&json!({
            "node_id": node_id,
            "listen": listen,
            "store": store_path.display().to_string(),
            "owner_token_file": owner_token_path.display().to_string(),
            "peer_token_file": peer_token_path.display().to_string(),
            "code": info.code,
            "expires_at_ms": info.expires_at_ms,
            "ttl_secs": info.ttl_secs,
            "pair_url": info.pair_url,
            "service_file": service_file.as_ref().map(|path| path.display().to_string()),
        }))?;
    } else {
        println!("Node id:      {node_id}");
        println!("Listen:       {listen}");
        println!("Pairing code: {}", info.code);
        println!("Pair URL:     {}", info.pair_url);
    }
    Ok(0)
}

fn choose_hub_node_id(store: &AnyStore, name: Option<String>) -> Result<String, (i32, String)> {
    let existing = find_hub_node(store).map_err(|e| (2, e))?;
    if let Some(requested) = name {
        let canonical = canonical_node_id(&requested).map_err(|e| (2, e))?;
        if let Some(hub) = existing {
            let existing_id =
                canonical_node_id(&hub.node_id).unwrap_or_else(|_| hub.node_id.clone());
            if existing_id != canonical {
                return Err((
                    2,
                    format!(
                        "hub node `{}` already exists and `--name` `{requested}` would create a second hub",
                        hub.node_id
                    ),
                ));
            }
        }
        return Ok(canonical);
    }
    if let Some(hub) = existing {
        return Ok(hub.node_id);
    }
    Ok(node_id_from_hostname(&local_hostname()))
}

fn cmd_enroll_token(
    env: &StateEnv<'_>,
    label: Option<String>,
    url: Option<String>,
    ttl_secs: u64,
    json_out: bool,
) -> Result<i32, (i32, String)> {
    let base_url = resolve_base_url(env, url);
    let store_path = resolve_hub_store_path(env);
    let store = open_hub_store_with_retry(&store_path)?;
    let info =
        mint_enroll_token(&store, label, ttl_secs, &base_url, now_ms()).map_err(|e| (2, e))?;
    if json_out {
        print_json(&json!({
            "code": info.code,
            "expires_at_ms": info.expires_at_ms,
            "ttl_secs": info.ttl_secs,
            "join_command": info.join_command,
        }))?;
    } else {
        println!("{}", info.join_command);
    }
    Ok(0)
}

fn cmd_join(
    hub_url: String,
    token: String,
    name: Option<String>,
    labels: Vec<String>,
    json_out: bool,
) -> Result<i32, (i32, String)> {
    let base_url = normalize_hub_url(&hub_url);
    let hostname = local_hostname();
    let (node_id, display_name) = if let Some(explicit) = name {
        (explicit.clone(), explicit)
    } else {
        (node_id_from_hostname(&hostname), hostname.clone())
    };
    let enroll_url = format!("{base_url}/api/v1/nodes/enroll");
    let response = http_agent()
        .post(&enroll_url)
        .send_json(json!({
            "code": token,
            "node_id": node_id,
            "display_name": display_name,
            "hostname": hostname,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "listen_addr": null,
            "capabilities": detect_capabilities(&path_env()),
            "labels": labels,
        }))
        .map_err(|e| (1, format!("hub enroll request to {enroll_url} failed: {e}")))?;
    let status = response.status().as_u16();
    let body = response.into_body().read_to_string().unwrap_or_default();
    if status != 200 {
        return Err((1, format!("hub enroll failed (HTTP {status}): {body}")));
    }
    let parsed: Value = serde_json::from_str(&body)
        .map_err(|e| (1, format!("hub enroll returned invalid JSON: {e}: {body}")))?;
    let node_token = parsed
        .get("node_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| (1, format!("hub enroll response missing node_token: {body}")))?
        .to_string();
    let enrolled_id = parsed
        .get("node_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or(node_id.as_str())
        .to_string();
    let recorded_url = parsed
        .get("hub_url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .unwrap_or(base_url.as_str())
        .to_string();

    let cfg_path = config_file_path()?;
    let cfg_dir = cfg_path.parent().unwrap_or_else(|| Path::new("."));
    if !cfg_dir.as_os_str().is_empty() {
        fs::create_dir_all(cfg_dir).map_err(|e| (2, format!("{}: {e}", cfg_dir.display())))?;
    }
    let token_path = cfg_dir.join("hub-node.token");
    let node_toml_path = cfg_dir.join("node.toml");
    write_atomic_non_symlink(&token_path, format!("{node_token}\n").as_bytes())
        .map_err(|e| (2, e))?;
    restrict_secret_file(&token_path)?;
    let node_toml = format!(
        "hub_url = {}\nnode_id = {}\ntoken_file = {}\n",
        toml_string(&recorded_url),
        toml_string(&enrolled_id),
        toml_string(&token_path.display().to_string()),
    );
    write_atomic_non_symlink(&node_toml_path, node_toml.as_bytes()).map_err(|e| (2, e))?;

    if json_out {
        print_json(&json!({
            "node_id": enrolled_id,
            "node_token": node_token,
            "hub_url": recorded_url,
            "token_file": token_path.display().to_string(),
        }))?;
    } else {
        println!("Node id:    {enrolled_id}");
        println!("Token file: {}", token_path.display());
    }
    Ok(0)
}

fn normalize_hub_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

fn cmd_nodes(
    env: &StateEnv<'_>,
    url: Option<String>,
    json_out: bool,
) -> Result<i32, (i32, String)> {
    let nodes = if let Some(url) = url {
        nodes_from_hub(env, url)?
    } else {
        nodes_from_store(env)?
    };
    if json_out {
        print_json(&json!({ "nodes": nodes }))?;
    } else {
        for node in &nodes {
            let node_id = node.get("node_id").and_then(Value::as_str).unwrap_or("");
            let role = node.get("role").and_then(Value::as_str).unwrap_or("");
            let status = node.get("status").and_then(Value::as_str).unwrap_or("");
            let hostname = node.get("hostname").and_then(Value::as_str).unwrap_or("");
            println!("{node_id} {role} {status} {hostname}");
        }
    }
    Ok(0)
}

fn nodes_from_store(env: &StateEnv<'_>) -> Result<Vec<Value>, (i32, String)> {
    let store_path = resolve_hub_store_path(env);
    let store = open_hub_store_with_retry(&store_path)?;
    let views = list_visible_nodes(&store, now_ms()).map_err(|e| (2, e))?;
    Ok(views.iter().map(node_view_to_json).collect())
}

fn nodes_from_hub(env: &StateEnv<'_>, url: String) -> Result<Vec<Value>, (i32, String)> {
    let base_url = resolve_base_url(env, Some(url));
    let nodes_url = format!("{base_url}/api/v1/nodes");
    let agent = http_agent();
    let request = agent.get(&nodes_url);
    let response = if let Some(token) = configured_bearer(env) {
        request
            .header("Authorization", &format!("Bearer {token}"))
            .call()
    } else {
        request.call()
    }
    .map_err(|e| (1, format!("hub nodes request to {nodes_url} failed: {e}")))?;
    let status = response.status().as_u16();
    let body = response.into_body().read_to_string().unwrap_or_default();
    if status != 200 {
        return Err((
            1,
            format!("hub nodes request failed (HTTP {status}): {body}"),
        ));
    }
    let parsed: Value = serde_json::from_str(&body)
        .map_err(|e| (1, format!("hub nodes response was not JSON: {e}: {body}")))?;
    let Some(list) = parsed.get("nodes").and_then(Value::as_array) else {
        return Err((1, format!("hub nodes response has no nodes array: {body}")));
    };
    Ok(list.iter().map(node_value_to_json).collect())
}

fn node_view_to_json(view: &NodeView) -> Value {
    json!({
        "node_id": view.node_id,
        "display_name": view.display_name,
        "role": view.role,
        "hostname": view.hostname,
        "os": view.os,
        "arch": view.arch,
        "listen_addr": view.listen_addr,
        "capabilities": view.capabilities,
        "labels": view.labels,
        "status": view.status,
        "enrolled_at_ms": view.enrolled_at_ms,
        "last_seen_at_ms": view.last_seen_at_ms,
    })
}

fn node_field(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

fn node_value_to_json(value: &Value) -> Value {
    json!({
        "node_id": node_field(value, "node_id"),
        "display_name": node_field(value, "display_name"),
        "role": node_field(value, "role"),
        "hostname": node_field(value, "hostname"),
        "os": node_field(value, "os"),
        "arch": node_field(value, "arch"),
        "listen_addr": node_field(value, "listen_addr"),
        "capabilities": value.get("capabilities").cloned().unwrap_or_else(|| json!([])),
        "labels": value.get("labels").cloned().unwrap_or_else(|| json!([])),
        "status": node_field(value, "status"),
        "enrolled_at_ms": node_field(value, "enrolled_at_ms"),
        "last_seen_at_ms": node_field(value, "last_seen_at_ms"),
    })
}

fn install_user_service(cwd: &Path) -> Result<PathBuf, (i32, String)> {
    let path = user_service_path()?;
    let exe = std::env::current_exe()
        .map_err(|e| (2, format!("service file needs the current executable: {e}")))?;
    let body = service_unit_text(&exe, cwd)?;
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        fs::create_dir_all(dir).map_err(|e| (2, format!("{}: {e}", dir.display())))?;
    }
    fs::write(&path, body).map_err(|e| (2, format!("{}: {e}", path.display())))?;
    Ok(path)
}

fn user_service_path() -> Result<PathBuf, (i32, String)> {
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
        let Some(base) = base else {
            return Err((
                2,
                "service file needs an absolute XDG_CONFIG_HOME or HOME".to_string(),
            ));
        };
        Ok(base.join("systemd/user/ferroloop-hub.service"))
    }
    #[cfg(target_os = "macos")]
    {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return Err((2, "service file needs HOME".to_string()));
        };
        Ok(home.join("Library/LaunchAgents/io.artificialhumanity.ferroloop-hub.plist"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err((
            2,
            "service files are not supported on this operating system".to_string(),
        ))
    }
}

fn service_unit_text(exe: &Path, cwd: &Path) -> Result<String, (i32, String)> {
    #[cfg(target_os = "linux")]
    {
        Ok(systemd_unit(exe, cwd))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(launchd_plist(exe, cwd))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (exe, cwd);
        Err((
            2,
            "service files are not supported on this operating system".to_string(),
        ))
    }
}

#[cfg(target_os = "linux")]
fn systemd_unit(exe: &Path, cwd: &Path) -> String {
    let exe = systemd_token(&exe.display().to_string());
    let cwd = systemd_token(&cwd.display().to_string());
    format!(
        "[Unit]\n\
         Description=FerroLoop Hub\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} hub serve\n\
         WorkingDirectory={cwd}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

#[cfg(target_os = "linux")]
fn systemd_token(text: &str) -> String {
    let needs_quotes = text.is_empty()
        || text.chars().any(|ch| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '"' | '\\' | '\'' | '$' | '`' | '#' | ';' | '&' | '|' | '<' | '>'
                )
        });
    if !needs_quotes {
        return text.to_string();
    }
    let mut quoted = String::from("\"");
    for ch in text.chars() {
        if matches!(ch, '\\' | '"' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

#[cfg(target_os = "macos")]
fn launchd_plist(exe: &Path, cwd: &Path) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let cwd = xml_escape(&cwd.display().to_string());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>io.artificialhumanity.ferroloop-hub</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{exe}</string>\n\
         \t\t<string>hub</string>\n\
         \t\t<string>serve</string>\n\
         \t</array>\n\
         \t<key>WorkingDirectory</key>\n\
         \t<string>{cwd}</string>\n\
         \t<key>RunAtLoad</key>\n\
         \t<false/>\n\
         </dict>\n\
         </plist>\n"
    )
}

#[cfg(target_os = "macos")]
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_store() -> (tempfile::TempDir, AnyStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = fl_store::open_redb(&dir.path().join("store.redb")).unwrap();
        (dir, store)
    }

    fn hub_args(node_id: &str) -> EnsureHubArgs {
        EnsureHubArgs {
            node_id: node_id.to_string(),
            display_name: "Primary Hub".to_string(),
            hostname: "hubhost".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            listen_addr: None,
            capabilities: Vec::new(),
            now_ms: 1_000,
        }
    }

    fn options(backend: fl_store::BackendKind) -> StoreOptions {
        StoreOptions {
            backend: Some(backend),
            ..StoreOptions::default()
        }
    }

    #[test]
    fn the_hub_store_opens_on_the_redb_engine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("hub.redb");
        let store =
            open_hub_store_with_options(&path, &options(fl_store::BackendKind::Redb)).unwrap();
        assert_eq!(store.kind(), fl_store::BackendKind::Redb);
        assert!(path.exists());
        assert!(!path.with_extension("fls").exists());
    }

    #[test]
    fn the_hub_store_opens_on_the_ferro_engine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("hub.redb");
        let store =
            open_hub_store_with_options(&path, &options(fl_store::BackendKind::Ferro)).unwrap();
        assert_eq!(store.kind(), fl_store::BackendKind::Ferro);
        // The configured `.redb` path names the physical `.fls` file.
        assert!(path.with_extension("fls").exists());
        assert!(!path.exists());
        // The hub's own functions work on it.
        ensure_hub_node(&store, hub_args("primary")).unwrap();
        assert_eq!(
            choose_hub_node_id(&store, Some("Primary".to_string())).unwrap(),
            "primary"
        );
    }

    #[test]
    fn choose_hub_node_id_compares_canonical_ids_and_rejects_bad_names() {
        let (_dir, store) = open_store();
        ensure_hub_node(&store, hub_args("primary")).unwrap();

        assert_eq!(
            choose_hub_node_id(&store, Some("Primary".to_string())).unwrap(),
            "primary"
        );
        assert_eq!(choose_hub_node_id(&store, None).unwrap(), "primary");

        let second = choose_hub_node_id(&store, Some("other-hub".to_string())).unwrap_err();
        assert_eq!(second.0, 2);
        assert!(second.1.contains("second hub"), "{}", second.1);

        let bad = choose_hub_node_id(&store, Some("a/b".to_string())).unwrap_err();
        assert_eq!(bad.0, 2);
        assert!(bad.1.contains("[a-z0-9_-]"), "{}", bad.1);
        assert!(!bad.1.contains("second hub"), "{}", bad.1);

        let (_empty_dir, empty) = open_store();
        assert_eq!(
            choose_hub_node_id(&empty, Some("Primary".to_string())).unwrap(),
            "primary"
        );
    }
}
