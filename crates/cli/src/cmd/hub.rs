//! `fl hub` CLI subcommands (`serve`, `pair`, `token`, `status`) for running and
//! managing the FerroLoop Hub daemon and `FerroDeck` web command center (spec §4.3).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;
use fl_core::pr_policy::DEFAULT_OWNER_LOGIN;
use fl_hub::{
    DEFAULT_HUB_LISTEN, DEFAULT_PAIRING_TTL_SECS, DEFAULT_SESSION_TTL_SECS, HubServer,
    HubServerConfig, generate_hex_token, mint_pairing_code_in_store, resolve_owner_token,
    resolve_peer_token,
};
use fl_store::RedbStore;
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

fn open_hub_store_with_retry(store_path: &Path) -> Result<RedbStore, (i32, String)> {
    if let Some(dir) = store_path.parent()
        && !dir.as_os_str().is_empty()
    {
        fs::create_dir_all(dir).map_err(|e| (2, format!("{}: {e}", dir.display())))?;
    }
    let mut last_err = None;
    for attempt in 0..40 {
        match RedbStore::open(store_path) {
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
