//! `fl flow` CLI subcommands for authoring, inspecting, and executing directed workflow graphs (workflow spec §4.1).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fl_core::ids::{Kind, ProjectId, WorkflowId, WorkflowRunId};
use fl_core::iri::Iri;
use fl_core::model::State;
use fl_core::pr_policy::DEFAULT_OWNER_LOGIN;
use fl_core::state::{HandoffEntry, validate_slug};
use fl_core::store::{AgentRegistry, Catalog, WorkflowStore};
use fl_core::telemetry::AdapterKind;
use fl_core::workflow::{
    StepCompletion, StepOutcome, WorkflowDef, WorkflowError, WorkflowRun, WorkflowRunStatus,
};
use fl_exec::stamp;
use fl_exec::workflow::{
    FLOWS_DIR, StepExecutionAction, StepExecutionOptions, WorkflowEnv, WorkflowExecError,
    abort_workflow_run, approve_workflow_step, assign_workflow_step, execute_workflow_step,
    export_workflow_toml, import_workflow_manifest, list_workflow_facts, list_workflow_handoffs,
    parse_workflow_file, record_step_handoff, reject_workflow_step, start_workflow_run,
    write_workflow_fact,
};
use fl_store::AnyStore;
use serde_json::{Value, json};

use crate::cmd::agent::write_atomic_non_symlink;
use crate::config::AdapterConfig;
use crate::ctx::Ctx;
use crate::refs::{self, Ref};

#[derive(Subcommand)]
pub enum FlowCmd {
    /// Add a workflow definition from a `.toml` or `.json` file.
    #[command(visible_alias = "create")]
    Add {
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        project: Option<Ref>,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// List workflow definitions in the store.
    List {
        #[arg(long)]
        project: Option<Ref>,
        #[arg(long)]
        json: bool,
    },
    /// Display a workflow definition by handle, IRI, or slug.
    Show {
        workflow: String,
        #[arg(long)]
        project: Option<Ref>,
        #[arg(long)]
        json: bool,
    },
    /// Export a workflow definition to `.fl/flows/<slug>.toml` (or `--out <path>`).
    Export {
        workflow: String,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        project: Option<Ref>,
        #[arg(long)]
        json: bool,
    },
    /// Import a `.fl/flows/<slug>.toml` (or `.json`) workflow definition into the store.
    Import {
        path: PathBuf,
        #[arg(long)]
        project: Option<Ref>,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Create and persist a new workflow run with `start_step` active.
    Start {
        workflow: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        project: Option<Ref>,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Show status of a workflow run, or list workflow runs when `<run>` is omitted.
    Status {
        run: Option<String>,
        #[arg(long)]
        project: Option<Ref>,
        #[arg(long)]
        json: bool,
    },
    /// List workflow runs.
    Runs {
        #[arg(long)]
        project: Option<Ref>,
        #[arg(long)]
        json: bool,
    },
    /// Assign an agent (and optional adapter vendor) to a workflow step, enforcing review independence.
    Assign {
        run: String,
        step_id: String,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        adapter: Option<String>,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Execute (or dispatch a FerroWire task offer for) an active workflow step.
    Step {
        run: String,
        #[arg(long)]
        step: Option<String>,
        #[arg(long)]
        adapter: Option<String>,
        #[arg(long)]
        binary: Option<String>,
        #[arg(long = "offer-only")]
        offer_only: bool,
        #[arg(long = "timeout-secs", default_value_t = 900)]
        timeout_secs: u64,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Repeatedly execute ready active steps until the run reaches Completed, NeedsHuman, or Failed.
    Run {
        run: String,
        #[arg(long)]
        adapter: Option<String>,
        #[arg(long)]
        binary: Option<String>,
        #[arg(long = "timeout-secs", default_value_t = 900)]
        timeout_secs: u64,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Record a curated step handoff and optional declared workflow runtime facts.
    Handoff {
        run: String,
        step_id: String,
        #[arg(long)]
        summary: String,
        #[arg(long = "shipped")]
        shipped: Vec<String>,
        #[arg(long = "in-flight")]
        in_flight: Vec<String>,
        #[arg(long = "blocker")]
        blockers: Vec<String>,
        #[arg(long = "next")]
        next_steps: Vec<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long = "fact")]
        facts: Vec<String>,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Approve a `NeedsHuman` workflow step and advance the workflow with `StepOutcome::Pass`.
    Approve {
        run: String,
        step_id: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Reject a `NeedsHuman` workflow step and advance the workflow with `StepOutcome::Fail`.
    Reject {
        run: String,
        step_id: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },
    /// Cancel an active or `NeedsHuman` workflow run.
    #[command(visible_alias = "cancel")]
    Abort {
        run: String,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
}

impl FlowCmd {
    fn project_ref(&self) -> Option<&Ref> {
        match self {
            Self::Add { project, .. }
            | Self::List { project, .. }
            | Self::Show { project, .. }
            | Self::Export { project, .. }
            | Self::Import { project, .. }
            | Self::Start { project, .. }
            | Self::Status { project, .. }
            | Self::Runs { project, .. } => project.as_ref(),
            Self::Assign { .. }
            | Self::Step { .. }
            | Self::Run { .. }
            | Self::Handoff { .. }
            | Self::Approve { .. }
            | Self::Reject { .. }
            | Self::Abort { .. } => None,
        }
    }

    fn item_str(&self) -> Option<&str> {
        match self {
            Self::Show { workflow, .. }
            | Self::Export { workflow, .. }
            | Self::Start { workflow, .. } => Some(workflow.as_str()),
            Self::Status { run: Some(r), .. }
            | Self::Assign { run: r, .. }
            | Self::Step { run: r, .. }
            | Self::Run { run: r, .. }
            | Self::Handoff { run: r, .. }
            | Self::Approve { run: r, .. }
            | Self::Reject { run: r, .. }
            | Self::Abort { run: r, .. } => Some(r.as_str()),
            _ => None,
        }
    }

    pub fn iris(&self) -> Vec<Iri> {
        let mut out = Vec::new();
        if let Some(Ref::Iri(i)) = self.project_ref() {
            out.push(i.clone());
        }
        if let Some(raw) = self.item_str()
            && let Ok(Ref::Iri(i)) = raw.parse::<Ref>()
        {
            out.push(i);
        }
        out
    }

    pub fn has_handle(&self) -> bool {
        if let Some(r) = self.project_ref()
            && refs::has_handle(&[r])
        {
            return true;
        }
        if let Some(raw) = self.item_str()
            && let Ok(r) = raw.parse::<Ref>()
        {
            return refs::has_handle(&[&r]);
        }
        false
    }

    pub fn needs_tracker(&self) -> bool {
        matches!(
            self,
            Self::Step { .. } | Self::Run { .. } | Self::Approve { .. } | Self::Reject { .. }
        )
    }
}

fn resolve_project(ctx: &Ctx<'_>, explicit: Option<&Ref>, cwd: &Path) -> Result<ProjectId> {
    if let Some(r) = explicit {
        let id = ProjectId(refs::resolve(
            ctx.handles,
            ctx.store.label(),
            Kind::Project,
            r,
        )?);
        if ctx.store.get_project(&id)?.is_none() {
            bail!(
                "`{r}` is not a project in the store at {}. Run `fl project list` to see the ones that exist.",
                ctx.store.label()
            );
        }
        return Ok(id);
    }

    let projects = ctx.store.list_projects()?;
    let canon_cwd = cwd.canonicalize().ok();
    let mut best: Option<(usize, ProjectId)> = None;
    for p in &projects {
        let p_path = Path::new(&p.root);
        let matched = cwd.starts_with(p_path)
            || match (&canon_cwd, p_path.canonicalize().ok()) {
                (Some(c), Some(cp)) => c.starts_with(&cp),
                _ => false,
            };
        if matched {
            let depth = p_path.components().count();
            if best.as_ref().is_none_or(|(best_d, _)| depth >= *best_d) {
                best = Some((depth, p.id.clone()));
            }
        }
    }
    if let Some((_, id)) = best {
        return Ok(id);
    }
    if projects.len() == 1 {
        return Ok(projects[0].id.clone());
    }
    bail!(
        "could not infer a project for `{}` in the store at {}; pass `--project <project>` or register one with `fl project add`",
        cwd.display(),
        ctx.store.label()
    )
}

fn resolve_workflow_def(
    ctx: &Ctx<'_>,
    raw: &str,
    explicit_project: Option<&Ref>,
    cwd: &Path,
) -> Result<WorkflowDef> {
    if let Ok(r) = raw.parse::<Ref>() {
        match r {
            Ref::Iri(iri) => {
                if let Some(def) = ctx.store.get_workflow_def(&WorkflowId(iri))? {
                    return Ok(def);
                }
            }
            Ref::Handle(h) | Ref::Issue(h) => {
                let Some(iri) = ctx.handles.resolve_handle(Kind::Workflow, h)? else {
                    bail!(
                        "there is no workflow {h} in the store at {}. Run `fl flow list` to see the ones that exist.",
                        ctx.store.label()
                    );
                };
                if let Some(def) = ctx.store.get_workflow_def(&WorkflowId(iri))? {
                    return Ok(def);
                }
            }
        }
    }

    let slug = validate_slug(raw).unwrap_or_else(|_| raw.trim().to_ascii_lowercase());
    if explicit_project.is_some() {
        let proj_id = resolve_project(ctx, explicit_project, cwd)?;
        if let Some(def) = ctx.store.get_workflow_def_by_slug(&proj_id, &slug)? {
            return Ok(def);
        }
        bail!(
            "no workflow with slug `{slug}` in project {} at {}",
            refs::show(ctx.handles, Kind::Project, proj_id.iri())?,
            ctx.store.label()
        );
    }

    if let Ok(proj_id) = resolve_project(ctx, None, cwd)
        && let Some(def) = ctx.store.get_workflow_def_by_slug(&proj_id, &slug)?
    {
        return Ok(def);
    }

    let matches: Vec<WorkflowDef> = ctx
        .store
        .list_workflow_defs(None)?
        .into_iter()
        .filter(|d| d.slug == slug)
        .collect();
    match matches.len() {
        0 => bail!(
            "no workflow matching `{raw}` in the store at {}. Run `fl flow list` to see the ones that exist.",
            ctx.store.label()
        ),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => bail!(
            "workflow slug `{slug}` exists in multiple projects in {}; pass `--project <project>`",
            ctx.store.label()
        ),
    }
}

fn resolve_workflow_run(ctx: &Ctx<'_>, raw: &str) -> Result<WorkflowRun> {
    let r = raw.parse::<Ref>().map_err(|e| anyhow::anyhow!("{e}"))?;
    let iri = refs::resolve(ctx.handles, ctx.store.label(), Kind::WorkflowRun, &r)?;
    ctx.store
        .get_workflow_run(&WorkflowRunId(iri))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`{raw}` is not a workflow run in the store at {}. Run `fl flow status` to see the ones that exist.",
                ctx.store.label()
            )
        })
}

fn workflow_def_json(ctx: &Ctx<'_>, def: &WorkflowDef) -> Result<Value> {
    let handle = refs::show(ctx.handles, Kind::Workflow, def.id.iri())?;
    let mut val = serde_json::to_value(def)?;
    if let Some(obj) = val.as_object_mut() {
        obj.insert("handle".into(), Value::String(handle));
    }
    Ok(val)
}

fn workflow_run_json(ctx: &Ctx<'_>, run: &WorkflowRun) -> Result<Value> {
    let handle = refs::show(ctx.handles, Kind::WorkflowRun, run.id.iri())?;
    let mut val = serde_json::to_value(run)?;
    if let Some(obj) = val.as_object_mut() {
        obj.insert("handle".into(), Value::String(handle));
    }
    Ok(val)
}

fn emit_exec_err(err: WorkflowExecError, json_out: bool) -> Result<i32> {
    if err.is_policy_refusal() {
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "error": err.to_string(),
                    "policy_refusal": true,
                }))?
            );
        }
        eprintln!("error: {err}");
        return Ok(2);
    }
    Err(err.into())
}

fn pick_registry_store<'a>(ctx_store: &'a AnyStore, ws_store: &'a AnyStore) -> &'a AnyStore {
    if ws_store
        .list_agents(None, true)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        ws_store
    } else if ctx_store
        .list_agents(None, true)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        ctx_store
    } else {
        ws_store
    }
}

fn build_step_options(
    step: Option<String>,
    adapter: Option<String>,
    binary: Option<String>,
    offer_only: bool,
    timeout_secs: u64,
    by: String,
    adapters: &BTreeMap<AdapterKind, AdapterConfig>,
) -> StepExecutionOptions {
    let resolved_kind = adapter
        .as_deref()
        .and_then(AdapterKind::from_wire)
        .unwrap_or(AdapterKind::Claude);
    let binary_override = binary.or_else(|| {
        adapters
            .get(&resolved_kind)
            .and_then(|cfg| cfg.binary.clone())
    });
    let rate_card = adapters.get(&resolved_kind).and_then(|cfg| cfg.rate_card());

    StepExecutionOptions {
        step_id: step,
        adapter_override: adapter,
        binary_override,
        rate_card,
        timeout_secs,
        budget_usd_micros: 1_000_000,
        offer_only,
        owner_login: DEFAULT_OWNER_LOGIN.to_string(),
        by,
    }
}

fn print_runs_list(
    ctx: &Ctx<'_>,
    project: Option<&Ref>,
    cwd: &Path,
    json_out: bool,
) -> Result<i32> {
    let project_id = match project {
        Some(r) => Some(resolve_project(ctx, Some(r), cwd)?),
        None => None,
    };
    let runs = ctx.store.list_workflow_runs(project_id.as_ref(), None)?;
    if json_out {
        let items: Vec<Value> = runs
            .iter()
            .map(|r| workflow_run_json(ctx, r))
            .collect::<Result<Vec<_>>>()?;
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        for r in &runs {
            let handle = refs::show(ctx.handles, Kind::WorkflowRun, r.id.iri())?;
            let active = if r.active_steps.is_empty() {
                "-".to_string()
            } else {
                r.active_steps.join(",")
            };
            println!(
                "{handle}\t{}\tactive={active}\texecuted={}\ttokens={}\tcost_usd_micros={}\t{}",
                r.status, r.total_steps_executed, r.total_tokens, r.total_cost_usd_micros, r.title
            );
        }
    }
    Ok(0)
}

pub fn run(
    ctx: &Ctx<'_>,
    adapters: &BTreeMap<AdapterKind, AdapterConfig>,
    ws_store: &AnyStore,
    cwd: &Path,
    cmd: FlowCmd,
) -> Result<i32> {
    let reg_store = pick_registry_store(ctx.store, ws_store);
    let wf_env = WorkflowEnv {
        workflows: ctx.store,
        state: ctx.store,
        registry: reg_store,
        wire: reg_store,
        roles: ctx.roles(),
    };

    match cmd {
        FlowCmd::Add {
            slug,
            title,
            file,
            project,
            by,
            json,
        } => {
            let project_id = resolve_project(ctx, project.as_ref(), cwd)?;
            let proj = ctx
                .store
                .get_project(&project_id)?
                .context("resolved project does not exist")?;
            let head = fl_exec::git::Git::head(Path::new(&proj.root))
                .unwrap_or_else(|_| "HEAD".to_string());
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("could not read {}", file.display()))?;
            let mut manifest = parse_workflow_file(&text)?;
            if let Some(s) = slug {
                manifest.slug = s;
            } else if manifest.slug.trim().is_empty()
                && let Some(stem) = file.file_stem().and_then(|s| s.to_str())
            {
                manifest.slug = stem.to_string();
            }
            if let Some(t) = title {
                manifest.title = t;
            }
            let def = import_workflow_manifest(ctx.store, &project_id, manifest, &head, &by)?;
            let saved = ctx.store.upsert_workflow_def(def)?;
            let handle = refs::show(ctx.handles, Kind::Workflow, saved.id.iri())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&workflow_def_json(ctx, &saved)?)?
                );
            } else {
                println!("{handle}\t{}\t{}", saved.slug, saved.title);
            }
            Ok(0)
        }

        FlowCmd::List { project, json } => {
            let project_id = match project.as_ref() {
                Some(r) => Some(resolve_project(ctx, Some(r), cwd)?),
                None => None,
            };
            let defs = ctx.store.list_workflow_defs(project_id.as_ref())?;
            if json {
                let items: Vec<Value> = defs
                    .iter()
                    .map(|d| workflow_def_json(ctx, d))
                    .collect::<Result<Vec<_>>>()?;
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                for d in &defs {
                    let handle = refs::show(ctx.handles, Kind::Workflow, d.id.iri())?;
                    println!(
                        "{handle}\t{}\t{}\tsteps={}\tmax_steps={}",
                        d.slug,
                        d.title,
                        d.steps.len(),
                        d.max_total_steps
                    );
                }
            }
            Ok(0)
        }

        FlowCmd::Show {
            workflow,
            project,
            json,
        } => {
            let def = resolve_workflow_def(ctx, &workflow, project.as_ref(), cwd)?;
            let handle = refs::show(ctx.handles, Kind::Workflow, def.id.iri())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&workflow_def_json(ctx, &def)?)?
                );
            } else {
                println!("workflow\t{handle}\t{}\t{}", def.slug, def.title);
                println!("start_step\t{}", def.start_step);
                println!("max_total_steps\t{}", def.max_total_steps);
                if let Some(b) = def.budget_usd_micros {
                    println!("budget_usd_micros\t{b}");
                }
                if let Some(t) = def.max_tokens {
                    println!("max_tokens\t{t}");
                }
                for (k, mode) in &def.declared_keys {
                    println!("declared_key\t{k}\t{}", mode.as_wire());
                }
                for s in &def.steps {
                    println!(
                        "step\t{}\t{}\tmax_attempts={}\tmax_visits={}",
                        s.step_id, s.title, s.max_attempts, s.max_visits
                    );
                }
                for e in &def.edges {
                    println!("edge\t{}\t{}\t{}", e.from_step, e.to_step, e.on);
                }
            }
            Ok(0)
        }

        FlowCmd::Export {
            workflow,
            out,
            project,
            json,
        } => {
            let def = resolve_workflow_def(ctx, &workflow, project.as_ref(), cwd)?;
            let toml_text = export_workflow_toml(ctx.store, &def)?;
            let out_path = match out {
                Some(p) => p,
                None => {
                    let proj = ctx
                        .store
                        .get_project(&def.project)?
                        .context("workflow project does not exist")?;
                    Path::new(&proj.root)
                        .join(FLOWS_DIR)
                        .join(format!("{}.toml", def.slug))
                }
            };
            write_atomic_non_symlink(&out_path, toml_text.as_bytes())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "workflow_id": def.id,
                        "slug": def.slug,
                        "path": out_path.display().to_string(),
                    }))?
                );
            } else {
                println!("exported\t{}\t{}", def.slug, out_path.display());
            }
            Ok(0)
        }

        FlowCmd::Import {
            path,
            project,
            by,
            json,
        } => {
            let project_id = resolve_project(ctx, project.as_ref(), cwd)?;
            let proj = ctx
                .store
                .get_project(&project_id)?
                .context("resolved project does not exist")?;
            let head = fl_exec::git::Git::head(Path::new(&proj.root))
                .unwrap_or_else(|_| "HEAD".to_string());
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let mut manifest = parse_workflow_file(&text)?;
            if manifest.slug.trim().is_empty()
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                manifest.slug = stem.to_string();
            }
            let def = import_workflow_manifest(ctx.store, &project_id, manifest, &head, &by)?;
            let saved = ctx.store.upsert_workflow_def(def)?;
            let handle = refs::show(ctx.handles, Kind::Workflow, saved.id.iri())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&workflow_def_json(ctx, &saved)?)?
                );
            } else {
                println!("imported\t{handle}\t{}\t{}", saved.slug, saved.title);
            }
            Ok(0)
        }

        FlowCmd::Start {
            workflow,
            title,
            project,
            by,
            json,
        } => {
            let def = resolve_workflow_def(ctx, &workflow, project.as_ref(), cwd)?;
            let run_obj = match start_workflow_run(ctx.store, &def, title, stamp::now(), by) {
                Ok(r) => r,
                Err(e) => return emit_exec_err(e, json),
            };
            let handle = refs::show(ctx.handles, Kind::WorkflowRun, run_obj.id.iri())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&workflow_run_json(ctx, &run_obj)?)?
                );
            } else {
                println!(
                    "{handle}\t{}\t{}\t{}\t{}",
                    def.slug,
                    run_obj.status,
                    run_obj.active_steps.join(","),
                    run_obj.title
                );
            }
            Ok(0)
        }

        FlowCmd::Runs { project, json } => print_runs_list(ctx, project.as_ref(), cwd, json),

        FlowCmd::Status { run, project, json } => {
            let Some(run_ref) = run else {
                return print_runs_list(ctx, project.as_ref(), cwd, json);
            };
            let run_obj = resolve_workflow_run(ctx, &run_ref)?;
            let def = ctx.store.get_workflow_def(&run_obj.workflow_id)?;
            let handoffs = list_workflow_handoffs(ctx.store, &run_obj.id)?;
            let facts = list_workflow_facts(ctx.store, &run_obj.id)?;
            let handle = refs::show(ctx.handles, Kind::WorkflowRun, run_obj.id.iri())?;

            if json {
                let mut val = workflow_run_json(ctx, &run_obj)?;
                if let Some(obj) = val.as_object_mut() {
                    if let Some(ref d) = def {
                        obj.insert("workflow_slug".into(), Value::String(d.slug.clone()));
                    }
                    obj.insert("handoffs".into(), serde_json::to_value(&handoffs)?);
                    obj.insert("facts".into(), serde_json::to_value(&facts)?);
                }
                println!("{}", serde_json::to_string_pretty(&val)?);
            } else {
                let wf_slug = def.as_ref().map(|d| d.slug.as_str()).unwrap_or("-");
                let active = if run_obj.active_steps.is_empty() {
                    "-".to_string()
                } else {
                    run_obj.active_steps.join(",")
                };
                println!(
                    "run\t{handle}\t{}\tworkflow={wf_slug}\tstatus={}\tactive={active}\texecuted={}\ttokens={}\tcost_usd_micros={}",
                    run_obj.id,
                    run_obj.status,
                    run_obj.total_steps_executed,
                    run_obj.total_tokens,
                    run_obj.total_cost_usd_micros
                );
                for s in run_obj.steps.values() {
                    println!(
                        "step\t{}\tstate={}\tvisits={}\tattempts={}\tagent={}\tadapter={}\toutcome={}\treason={}",
                        s.step_id,
                        s.state.as_wire(),
                        s.visit_count,
                        s.attempt_count,
                        s.assigned_agent.as_deref().unwrap_or("-"),
                        s.adapter_used.as_deref().unwrap_or("-"),
                        s.last_outcome.map(|o| o.as_wire()).unwrap_or("-"),
                        s.needs_human_reason.as_deref().unwrap_or("-")
                    );
                }
            }
            Ok(0)
        }

        FlowCmd::Assign {
            run,
            step_id,
            agent,
            adapter,
            by,
            json,
        } => {
            let run_obj = resolve_workflow_run(ctx, &run)?;
            let def = ctx
                .store
                .get_workflow_def(&run_obj.workflow_id)?
                .context("workflow definition for run does not exist")?;
            let updated = match assign_workflow_step(
                ctx.store,
                reg_store,
                &def,
                &run_obj.id,
                &step_id,
                &agent,
                adapter.as_deref(),
                stamp::now(),
                &by,
            ) {
                Ok(r) => r,
                Err(e) => return emit_exec_err(e, json),
            };
            let handle = refs::show(ctx.handles, Kind::WorkflowRun, updated.id.iri())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&workflow_run_json(ctx, &updated)?)?
                );
            } else {
                let st = &updated.steps[&step_id];
                println!(
                    "assigned\t{handle}\t{step_id}\t{}\t{}",
                    st.assigned_agent.as_deref().unwrap_or("-"),
                    st.adapter_used.as_deref().unwrap_or("-")
                );
            }
            Ok(0)
        }

        FlowCmd::Step {
            run,
            step,
            adapter,
            binary,
            offer_only,
            timeout_secs,
            by,
            json,
        } => {
            let run_obj = resolve_workflow_run(ctx, &run)?;
            let steps_to_run: Vec<Option<String>> = match step {
                Some(explicit) => vec![Some(explicit)],
                None => {
                    let ready: Vec<Option<String>> = run_obj
                        .active_steps
                        .iter()
                        .filter(|sid| {
                            run_obj
                                .steps
                                .get(*sid)
                                .is_some_and(|st| st.state != State::NeedsHuman)
                        })
                        .cloned()
                        .map(Some)
                        .collect();
                    if ready.is_empty() { vec![None] } else { ready }
                }
            };

            let def = ctx
                .store
                .get_workflow_def(&run_obj.workflow_id)?
                .context("workflow definition for run does not exist")?;
            let mut reports = Vec::new();
            for target_step in steps_to_run {
                let opts = build_step_options(
                    target_step,
                    adapter.clone(),
                    binary.clone(),
                    offer_only,
                    timeout_secs,
                    by.clone(),
                    adapters,
                );
                let mut rep = match execute_workflow_step(wf_env, &run_obj.id, &opts) {
                    Ok(r) => r,
                    Err(e) => return emit_exec_err(e, json),
                };
                // If any newly active step is a manual human_sign_off gate, mark it NeedsHuman immediately.
                let mut transitioned_manual_nh = false;
                for active_id in rep.run.active_steps.clone() {
                    if let Some(sdef) = def.steps.iter().find(|s| s.step_id == active_id)
                        && matches!(sdef.assignee, fl_core::workflow::AssigneeRule::Manual)
                        && let StepCompletion::HumanSignOff { approval_tier } = sdef.completion
                        && let Some(st) = rep.run.steps.get_mut(&active_id)
                        && st.state == State::Todo
                        && st.assigned_agent.is_none()
                    {
                        st.state = State::NeedsHuman;
                        st.last_outcome = Some(StepOutcome::NeedsHuman);
                        st.needs_human_reason =
                            Some(format!("awaiting {} sign-off", approval_tier.as_wire()));
                        transitioned_manual_nh = true;
                    }
                }
                if transitioned_manual_nh {
                    rep.run.status = WorkflowRunStatus::NeedsHuman;
                    rep.run.updated_at = stamp::now();
                    rep.run = ctx.store.update_workflow_run(rep.run)?;
                }
                let stop = rep.run.status != WorkflowRunStatus::Running;
                reports.push(rep);
                if stop {
                    break;
                }
            }

            let last_rep = reports.last().expect("at least one step execution report");
            let final_run = &last_rep.run;
            let handle = refs::show(ctx.handles, Kind::WorkflowRun, final_run.id.iri())?;

            if json {
                let actions: Vec<Value> = reports
                    .iter()
                    .map(|r| {
                        json!({
                            "step_id": r.step_id,
                            "action": r.action,
                        })
                    })
                    .collect();
                let mut val = workflow_run_json(ctx, final_run)?;
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("step_id".into(), Value::String(last_rep.step_id.clone()));
                    obj.insert("action".into(), serde_json::to_value(&last_rep.action)?);
                    obj.insert("steps_executed".into(), Value::Array(actions));
                }
                println!("{}", serde_json::to_string_pretty(&val)?);
            } else {
                for r in &reports {
                    match &r.action {
                        StepExecutionAction::AwaitingAssignment => {
                            println!("step\t{handle}\t{}\tawaiting_assignment", r.step_id);
                        }
                        StepExecutionAction::Offered { message_id, agent } => {
                            println!(
                                "step\t{handle}\t{}\toffered\t{agent}\t{message_id}",
                                r.step_id
                            );
                        }
                        StepExecutionAction::Executed {
                            agent,
                            adapter,
                            outcome,
                            ..
                        } => {
                            println!(
                                "step\t{handle}\t{}\t{}\tagent={}\tadapter={adapter}\trun_status={}",
                                r.step_id,
                                outcome.as_wire(),
                                agent.as_deref().unwrap_or("-"),
                                r.run.status
                            );
                        }
                    }
                }
            }

            let any_failed_or_nh = reports.iter().any(|r| {
                matches!(
                    r.action,
                    StepExecutionAction::Executed {
                        outcome: StepOutcome::Fail | StepOutcome::NeedsHuman,
                        ..
                    }
                )
            }) || final_run.status == WorkflowRunStatus::Failed;
            Ok(if any_failed_or_nh { 1 } else { 0 })
        }

        FlowCmd::Run {
            run,
            adapter,
            binary,
            timeout_secs,
            by,
            json,
        } => {
            let mut current_run = resolve_workflow_run(ctx, &run)?;
            let def = ctx
                .store
                .get_workflow_def(&current_run.workflow_id)?
                .context("workflow definition for run does not exist")?;
            let mut actions = Vec::new();

            while current_run.status == WorkflowRunStatus::Running {
                let opts = build_step_options(
                    None,
                    adapter.clone(),
                    binary.clone(),
                    false,
                    timeout_secs,
                    by.clone(),
                    adapters,
                );
                let rep = match execute_workflow_step(wf_env, &current_run.id, &opts) {
                    Ok(r) => r,
                    Err(e) => return emit_exec_err(e, json),
                };
                let is_awaiting = matches!(rep.action, StepExecutionAction::AwaitingAssignment);
                let step_id = rep.step_id.clone();
                current_run = rep.run.clone();
                actions.push(json!({
                    "step_id": rep.step_id,
                    "action": rep.action,
                }));

                if is_awaiting {
                    if let Some(sdef) = def.steps.iter().find(|s| s.step_id == step_id)
                        && let StepCompletion::HumanSignOff { approval_tier } = sdef.completion
                    {
                        if let Some(st) = current_run.steps.get_mut(&step_id) {
                            st.state = State::NeedsHuman;
                            st.last_outcome = Some(StepOutcome::NeedsHuman);
                            st.needs_human_reason =
                                Some(format!("awaiting {} sign-off", approval_tier.as_wire()));
                        }
                        current_run.status = WorkflowRunStatus::NeedsHuman;
                        current_run.updated_at = stamp::now();
                        current_run = ctx.store.update_workflow_run(current_run)?;
                    }
                    break;
                }
            }

            let handle = refs::show(ctx.handles, Kind::WorkflowRun, current_run.id.iri())?;
            if json {
                let mut val = workflow_run_json(ctx, &current_run)?;
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("steps_executed".into(), Value::Array(actions));
                }
                println!("{}", serde_json::to_string_pretty(&val)?);
            } else {
                println!(
                    "run\t{handle}\t{}\texecuted={}\ttokens={}\tcost_usd_micros={}",
                    current_run.status,
                    current_run.total_steps_executed,
                    current_run.total_tokens,
                    current_run.total_cost_usd_micros
                );
            }

            Ok(if current_run.status == WorkflowRunStatus::Completed {
                0
            } else {
                1
            })
        }

        FlowCmd::Handoff {
            run,
            step_id,
            summary,
            shipped,
            in_flight,
            blockers,
            next_steps,
            notes,
            facts,
            by,
            json,
        } => {
            let run_obj = resolve_workflow_run(ctx, &run)?;
            let def = ctx
                .store
                .get_workflow_def(&run_obj.workflow_id)?
                .context("workflow definition for run does not exist")?;
            if !def.steps.iter().any(|s| s.step_id == step_id) {
                return emit_exec_err(WorkflowError::UnknownStep(step_id).into(), json);
            }
            let now = stamp::now();
            let mut parsed_facts = Vec::new();
            for raw_fact in &facts {
                let Some((k, v_raw)) = raw_fact.split_once('=') else {
                    bail!("invalid `--fact {raw_fact}`; expected `KEY=JSON_VALUE`");
                };
                let key = k.trim();
                if !def.declared_keys.contains_key(key) {
                    return emit_exec_err(
                        WorkflowError::UndeclaredWorkflowKey {
                            workflow: def.slug.clone(),
                            key: key.to_string(),
                        }
                        .into(),
                        json,
                    );
                }
                let val: Value = serde_json::from_str(v_raw)
                    .unwrap_or_else(|_| Value::String(v_raw.to_string()));
                parsed_facts.push((key.to_string(), val));
            }
            let mut written_facts = Vec::new();
            for (k, val) in parsed_facts {
                match write_workflow_fact(ctx.store, &def, &run_obj, &k, val, now.clone(), &by) {
                    Ok(entry) => written_facts.push(entry),
                    Err(e) => return emit_exec_err(e.into(), json),
                }
            }
            let visit_count = run_obj
                .steps
                .get(&step_id)
                .map(|s| s.visit_count.max(1))
                .unwrap_or(1);
            let handoff_entry = record_step_handoff(
                ctx.store,
                &run_obj.id,
                &step_id,
                visit_count,
                HandoffEntry {
                    summary,
                    shipped,
                    in_flight,
                    blockers,
                    next_steps,
                    notes,
                },
                now,
                &by,
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "handoff": handoff_entry,
                        "facts": written_facts,
                    }))?
                );
            } else {
                println!("handoff\t{}\t{}", handoff_entry.slug, handoff_entry.id);
            }
            Ok(0)
        }

        FlowCmd::Approve {
            run,
            step_id,
            by,
            note,
            json,
        } => {
            let run_obj = resolve_workflow_run(ctx, &run)?;
            let updated = match approve_workflow_step(
                wf_env,
                &run_obj.id,
                &step_id,
                &by,
                note.as_deref(),
                DEFAULT_OWNER_LOGIN,
            ) {
                Ok(r) => r,
                Err(e) => return emit_exec_err(e, json),
            };
            let handle = refs::show(ctx.handles, Kind::WorkflowRun, updated.id.iri())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&workflow_run_json(ctx, &updated)?)?
                );
            } else {
                println!(
                    "approved\t{handle}\t{step_id}\tby={by}\tstatus={}",
                    updated.status
                );
            }
            Ok(0)
        }

        FlowCmd::Reject {
            run,
            step_id,
            by,
            reason,
            json,
        } => {
            let run_obj = resolve_workflow_run(ctx, &run)?;
            let updated = match reject_workflow_step(
                wf_env,
                &run_obj.id,
                &step_id,
                &by,
                &reason,
                DEFAULT_OWNER_LOGIN,
            ) {
                Ok(r) => r,
                Err(e) => return emit_exec_err(e, json),
            };
            let handle = refs::show(ctx.handles, Kind::WorkflowRun, updated.id.iri())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&workflow_run_json(ctx, &updated)?)?
                );
            } else {
                println!(
                    "rejected\t{handle}\t{step_id}\tby={by}\tstatus={}",
                    updated.status
                );
            }
            Ok(0)
        }

        FlowCmd::Abort { run, by, json } => {
            let run_obj = resolve_workflow_run(ctx, &run)?;
            let updated = match abort_workflow_run(ctx.store, &run_obj.id, stamp::now(), &by) {
                Ok(r) => r,
                Err(e) => return emit_exec_err(e, json),
            };
            let handle = refs::show(ctx.handles, Kind::WorkflowRun, updated.id.iri())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&workflow_run_json(ctx, &updated)?)?
                );
            } else {
                println!("aborted\t{handle}\tstatus={}", updated.status);
            }
            Ok(0)
        }
    }
}
