//! Metadata-only recovery inventory. Dispatch precedes generic config/store opening.
use crate::{config, version};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use fl_core::state::StateScope;
use fl_store::recovery::inventory::{
    BindingInput, ConsumerEdge, DeploymentMetadata, InventoryInputs, collect_inventory,
    metadata_digest, read_metadata_artifact, scan_candidates,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const MAX_HUB_DISCOVERY_ENTRIES: usize = 4096;

#[derive(Subcommand)]
pub enum Cmd {
    Inventory(InventoryArgs),
    Batch(BatchArgs),
    Status(BatchArgs),
}
#[derive(Args)]
pub struct BatchArgs {
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    policy: String,
    #[arg(long)]
    batch: String,
    #[arg(long = "member", required = true)]
    members: Vec<String>,
}
fn batch(args: BatchArgs, method: &str) -> Result<i32> {
    use fl_store::recovery::control::{BatchRequest, MemberRequest, call};
    let request = BatchRequest {
        policy_id: args.policy,
        batch_id: args.batch.parse().context("invalid batch ID")?,
        members: args
            .members
            .iter()
            .map(|m| {
                let (store_id, id) = m
                    .split_once('=')
                    .context("member must be STORE=OPERATION_ID")?;
                Ok(MemberRequest {
                    store_id: store_id.into(),
                    operation_id: id.parse().context("invalid operation ID")?,
                    scheduled_at_unix_seconds: None,
                    byte_cap: None,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let report = call(&args.endpoint, method, &request)?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(if report.complete { 0 } else { 1 })
}
#[derive(Args)]
pub struct InventoryArgs {
    /// Explicit storage scan root. Repeat for every deployment root; never opens a store.
    #[arg(long = "root", required = true)]
    roots: Vec<PathBuf>,
    /// Config file (default: normal XDG fl config). Read bounded, with store aliases refused.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Versioned supplied unit/consumer/recovery metadata; observations need source and time.
    #[arg(long)]
    deployment: Option<PathBuf>,
    /// Host identity supplied by the operator; unknown when absent.
    #[arg(long)]
    host_id: Option<String>,
    #[arg(long)]
    json: bool,
}

fn consumers(scope: &str) -> Vec<ConsumerEdge> {
    let revision = version::build_info().commit;
    let mut entries = vec![
        (
            "CLI",
            "read,write",
            "per invocation",
            "crates/cli/src/main.rs::run",
            "unspecified_legacy_auto",
        ),
        (
            "MCP",
            "read,write,index",
            "per tool call; primary selection",
            "crates/cli/src/cmd/state_mcp.rs::open_workflow_store",
            "unspecified_legacy_auto",
        ),
        (
            "hub",
            "read,write",
            "per request; per-path serialization",
            "crates/hub/src/records.rs::open_project_store",
            "unspecified_legacy_auto",
        ),
        (
            "store maintenance",
            "read,write",
            "per invocation",
            "crates/store/src/cli/data.rs::open_local_locked",
            "direct_embedded",
        ),
        (
            "code index",
            "read,write,index",
            "per sync/search; hub cached read handle",
            "crates/store/src/code_knowledge.rs::CodeSidecar",
            "direct_embedded",
        ),
    ];
    if scope == "workspace" {
        entries.extend([
            (
                "wire",
                "read,write",
                "daemon lifetime",
                "crates/wire/src/daemon.rs::spawn_daemon",
                "unspecified_legacy_auto",
            ),
            (
                "governor",
                "read,write",
                "hub lifetime after first use",
                "crates/hub/src/api.rs::hub_gate_governor",
                "unspecified_legacy_auto",
            ),
            (
                "host client",
                "read",
                "client invocation; host daemon opens no store",
                "crates/cli/src/cmd/host.rs::execute_with_db",
                "unspecified_legacy_auto",
            ),
        ]);
    }
    entries
        .into_iter()
        .map(|(caller, effects, lifetime, symbol, mode)| ConsumerEdge {
            caller: caller.into(),
            binding_callers: match caller {
                "CLI" | "MCP" => vec!["config", "CLI root", "CLI override"],
                "hub" | "governor" => vec!["hub"],
                "code index" => vec!["code index"],
                "store maintenance" => vec!["config", "CLI root", "CLI override", "hub"],
                _ => vec!["config"],
            }
            .into_iter()
            .map(str::to_owned)
            .collect(),
            scope: scope.into(),
            effects: effects.split(',').map(str::to_owned).collect(),
            handle_lifetime: lifetime.into(),
            source_symbol: symbol.into(),
            source_revision: revision.clone(),
            declared_mode: mode.into(),
            provenance: "source-derived possibility; not observed traffic".into(),
        })
        .collect()
}

pub fn inventory_inputs(
    config: &config::Config,
    config_path: &Path,
    roots: &[PathBuf],
    explicit_db: Option<&Path>,
) -> Result<InventoryInputs> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let default = crate::default_state_store_path()?;
    let workspace = config.workspace_state_store(&default);
    let cwd = std::env::current_dir()?;
    let hub_workspace = super::hub::resolve_hub_store_path(&super::state::StateEnv {
        explicit_db,
        default_store: &default,
        cfg: config,
        cwd: &cwd,
    });
    let hub_workspace = if hub_workspace.is_absolute() {
        hub_workspace
    } else {
        cwd.join(hub_workspace)
    };
    let env = std::env::var(fl_store::BACKEND_ENV).ok();
    let mut inputs = InventoryInputs {
        roots: roots.to_vec(),
        home: home.clone(),
        config_path: Some(config_path.to_path_buf()),
        collector_build: serde_json::to_value(version::build_info())?,
        ..Default::default()
    };
    let mut workspace_edge = BindingInput::config(
        "workspace",
        workspace.clone(),
        env.clone(),
        &format!("{}:[state].store or XDG default", config_path.display()),
    );
    workspace_edge.precedence = "[state].store > XDG_DATA_HOME > HOME".into();
    inputs.bindings.push(workspace_edge);
    inputs.bindings.push(BindingInput {
        caller: "hub".into(),
        scope: "workspace".into(),
        logical_path: hub_workspace.clone(),
        backend_env: env.clone(),
        source: "crates/cli/src/cmd/hub.rs::resolve_hub_store_path; source-derived invocation/config/cwd selection, not observed daemon binding".into(),
        precedence: "--db > FL_DB > [state].store > cwd-bound project > workspace default".into(),
        declared_mode: "unspecified_legacy_auto".into(),
        ..Default::default()
    });
    inputs.consumers.extend(consumers("workspace"));
    // Root-based CLI/MCP resolution must retain every entry, even when no
    // valid slug exists or multiple roots have the same basename.
    for (index, entry) in config.projects.iter().enumerate() {
        let label = format!("root:{}", entry.root.display());
        let mut binding = BindingInput::config(
            &label,
            entry.store.clone(),
            env.clone(),
            &format!("{}:[[project]][{index}].store", config_path.display()),
        );
        binding.caller = "CLI root".into();
        binding.project_root = Some(entry.root.clone());
        binding.precedence =
            "config::bound_entry: longest matching root; ambiguous roots refused".into();
        inputs.bindings.push(binding);
        inputs
            .consumers
            .extend(consumers(&label).into_iter().filter(|c| c.caller != "hub"));
    }
    inputs.consumers.push(ConsumerEdge {
        caller: "MCP cross-store".into(),
        binding_callers: vec!["config".into(), "CLI root".into()],
        scope: "*".into(),
        effects: vec!["read".into()],
        handle_lifetime: "IRI ownership scan; per tool call".into(),
        source_symbol: "crates/cli/src/cmd/state_mcp.rs::open_workflow_store".into(),
        source_revision: version::build_info().commit,
        declared_mode: "unspecified_legacy_auto".into(),
        provenance: "source-derived cross-store scan possibility; not observed traffic".into(),
    });
    let mut slugs: BTreeSet<String> = config
        .projects
        .iter()
        .filter_map(|entry| entry.inferred_slug())
        .collect();
    if let Some(home) = &home {
        let hub_root = home.join(".local/share/fl");
        if let Ok(entries) = std::fs::read_dir(&hub_root) {
            let mut discovered = BTreeSet::new();
            for (visited, entry) in entries.enumerate() {
                if visited >= MAX_HUB_DISCOVERY_ENTRIES {
                    // Keep no enumeration-dependent subset after truncation.
                    // Configured bindings remain intact, and the main metadata
                    // scan still accounts for candidates in declared roots.
                    inputs
                        .warnings
                        .push("hub discovery limit; directory-derived bindings omitted".into());
                    discovered.clear();
                    break;
                }
                let entry = match entry {
                    Ok(v) => v,
                    Err(_) => {
                        inputs
                            .warnings
                            .push("unreadable hub directory entry".into());
                        continue;
                    }
                };
                let path = entry.path();
                if matches!(
                    path.extension().and_then(|s| s.to_str()),
                    Some("fls" | "redb")
                ) && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                    && stem != "fl"
                    && !stem.ends_with(".code")
                {
                    discovered.insert(stem.to_owned());
                }
            }
            slugs.extend(discovered);
        } else {
            inputs
                .warnings
                .push("hub directory unavailable; directory-derived bindings unknown".into());
        }
    } else {
        inputs.warnings.push("hub home path unknown".into());
    }
    for slug in slugs {
        let scope = StateScope::Project(slug.clone());
        let label = format!("project:{slug}");
        let path = config.state_store_for_scope(&scope, &default);
        let mut binding = BindingInput::config(
            &label,
            path,
            env.clone(),
            &format!("{}:[[project]].store", config_path.display()),
        );
        binding.project_root = config.find_project_entry(&slug).map(|e| e.root.clone());
        binding.fallback_target = Some(workspace.clone());
        binding.precedence = "matching project store > workspace state store".into();
        if config.find_project_entry(&slug).is_none() {
            binding.source = format!(
                "{}:unbound project falls back to workspace",
                config_path.display()
            );
        }
        inputs.bindings.push(binding);
        if let Some(home) = &home {
            inputs.bindings.push(BindingInput {
                caller: "hub".into(),
                scope: label.clone(),
                logical_path: home.join(".local/share/fl").join(format!("{slug}.redb")),
                backend_env: env.clone(),
                source: "crates/hub/src/records.rs::{normalize_project,project_redb_with}; HOME slug lookup".into(),
                precedence: "plain project file > hub workspace fallback; unmigrated redb refused".into(),
                fallback_target: Some(fl_store::physical_path(
                    &hub_workspace,
                    fl_store::choose_backend(&hub_workspace, env.as_deref())?,
                )),
                hub_slug: Some(slug),
                declared_mode: "unspecified_legacy_auto".into(),
                ..Default::default()
            });
        }
        inputs.consumers.extend(consumers(&label));
    }
    Ok(inputs)
}

pub fn run(cmd: Cmd, explicit_db: Option<PathBuf>) -> Result<i32> {
    let args = match cmd {
        Cmd::Inventory(args) => args,
        Cmd::Batch(args) => return batch(args, "recovery_start"),
        Cmd::Status(args) => return batch(args, "recovery_status"),
    };
    if args.roots.iter().any(|r| !r.is_absolute()) {
        bail!("inventory scan roots must be absolute");
    }
    let config_path = args
        .config
        .or_else(config::path)
        .context("inventory needs a config path or HOME/XDG_CONFIG_HOME")?;
    let scan = scan_candidates(&args.roots)?;
    if args.deployment.is_some() {
        scan.require_complete()?;
    }
    let mut candidates = scan.paths.clone();
    let config_bytes = match std::fs::symlink_metadata(&config_path) {
        Ok(_) => {
            scan.require_complete()?;
            Some(read_metadata_artifact(&config_path, &candidates)?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => bail!(
            "inventory config metadata unavailable: {}",
            config_path.display()
        ),
    };
    let config = match &config_bytes {
        Some(bytes) => config::parse(
            &config_path,
            std::str::from_utf8(bytes).context("inventory config must be UTF-8")?,
        )
        .map_err(|_| anyhow::anyhow!("inventory config is invalid: {}", config_path.display()))?,
        None => config::Config::default(),
    };
    let source = if explicit_db
        .as_ref()
        .is_some_and(|p| !p.as_os_str().is_empty())
    {
        "--db"
    } else {
        "FL_DB"
    };
    let override_path = crate::explicit_db(explicit_db);
    let mut inputs =
        inventory_inputs(&config, &config_path, &args.roots, override_path.as_deref())?;
    inputs.warnings.extend(scan.warnings);
    inputs.config_digest = config_bytes.as_deref().map(metadata_digest);
    inputs.host_identifier = args.host_id;
    if let Some(path) = override_path {
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        if !args.roots.iter().any(|root| path.starts_with(root)) {
            bail!(
                "inventory {source} override is outside explicit scan roots; add its root explicitly"
            );
        }
        let mut edge = BindingInput::config(
            "invocation",
            path,
            std::env::var(fl_store::BACKEND_ENV).ok(),
            source,
        );
        edge.caller = "CLI override".into();
        edge.precedence = "--db > FL_DB > config > XDG default".into();
        inputs.bindings.push(edge);
    }
    if let Some(path) = args.deployment {
        candidates.extend(inputs.bindings.iter().filter_map(|b| {
            fl_store::choose_backend(&b.logical_path, b.backend_env.as_deref())
                .ok()
                .map(|k| fl_store::physical_path(&b.logical_path, k))
        }));
        let bytes = read_metadata_artifact(&path, &candidates)?;
        inputs.deployment = serde_json::from_slice::<DeploymentMetadata>(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid deployment metadata: {}", path.display()))?;
    }
    let map = collect_inventory(&inputs)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&map)?);
    } else {
        println!(
            "{} store candidates, {} bindings; complete: {}",
            map.stores.len(),
            map.bindings.len(),
            map.complete
        );
        for warning in &map.warnings {
            println!("warning: {warning}");
        }
    }
    Ok(0)
}
