use crate::config::{self, Config};
use crate::refs::{self, Ref};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use fl_core::store::Catalog;
use fl_core::{Iri, Kind};
use fl_hub::lifecycle::LifecycleEnv;
use fl_hub::project_delete::{self, DeleteContext, DeleteError, DeleteRequest};
use fl_hub::project_init::{self, Credential, InitError, InitPaths, InitRequest, VirtualRequest};
use fl_hub::project_list;
use fl_store::AnyStore;
use std::path::{Component, Path, PathBuf};

#[derive(Subcommand)]
pub enum Cmd {
    /// Register a project directory. The directory need not be a Git repository.
    Add { path: String },
    /// List directory-backed projects and virtual groups: kind, slug, path, tracker and the
    /// number of active agents.
    List {
        /// Print the projects as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Create a project. With `--virtual`, a virtual group: a name for a set of agents,
    /// addressable as `@<slug>`, with no directory, store, config block or git. Otherwise a
    /// directory-backed project at `--path` (a Git repository or not): a `[[project]]` block in
    /// the config, a store holding the project and the default local routing areas, its
    /// workspace area and a starter `.fl/mcp.toml`. Git runs only with `--git-init`. Running it
    /// again changes nothing already right. `fl project init <path>` is the same command, and
    /// without `--path` or `--virtual` the argument is the path.
    #[command(alias = "init")]
    Create(CreateArgs),
    /// Delete a project or virtual group: its `[[project]]` block in the config and its area
    /// in the workspace store. Nothing on disk is deleted unless asked: `--delete-store`
    /// removes the project's store file (and its `.sock` and `.migrate.lock` siblings),
    /// `--delete-dir` the project directory, after safety checks (it must be the registered
    /// directory, deeper than two levels, and not `/`, the home directory, the workspace root,
    /// a parent of either, another project, or a store or config directory). A project with
    /// active agents is refused unless `--reassign-to` moves them or `--decommission-agents`
    /// retires them.
    Delete(DeleteArgs),
}

/// The arguments of `fl project delete`.
#[derive(Args)]
pub struct DeleteArgs {
    /// The slug of the project or virtual group.
    slug: String,
    /// Move the project's active agents to this project (it must exist).
    #[arg(
        long,
        value_name = "TARGET_SLUG",
        conflicts_with = "decommission_agents"
    )]
    reassign_to: Option<String>,
    /// Retire the project's active agents and tear their sessions down.
    #[arg(long)]
    decommission_agents: bool,
    /// Also delete the project's store file and its `.sock` and `.migrate.lock` siblings.
    #[arg(long)]
    delete_store: bool,
    /// Also delete the project's directory (after the safety checks).
    #[arg(long)]
    delete_dir: bool,
    /// Author recording the deletion. Default: `fl`.
    #[arg(long)]
    by: Option<String>,
    /// Print the outcome as JSON, the body `DELETE /api/v1/projects/:slug` answers.
    #[arg(long)]
    json: bool,
}

/// The arguments of `fl project create` (alias `init`).
#[derive(Args)]
pub struct CreateArgs {
    /// With `--path` or `--virtual`: the project's name, as shown (`"My Project"`); its slug is
    /// the name in lowercase kebab-case unless `--slug` says. Without either: the project root
    /// (the `init <path>` form), made if it does not exist.
    target: String,
    /// The project root, an existing or new directory. It need not be a Git repository.
    #[arg(long, conflicts_with = "virtual_group")]
    path: Option<String>,
    /// Make a virtual group: an area with no path. No directory, store, config block or git.
    #[arg(long = "virtual", conflicts_with_all = ["path", "git_init", "github", "credential"])]
    virtual_group: bool,
    /// The project's name, as shown, in the `init <path>` form. Its slug is the name in
    /// lowercase kebab-case: lowercase letters, digits, `-` and `_`, starting with a letter.
    /// Without it, the root's lowercase directory name.
    #[arg(long, conflicts_with_all = ["path", "virtual_group"])]
    name: Option<String>,
    /// The project's slug, when it is not the kebab-case of its name.
    #[arg(long)]
    slug: Option<String>,
    /// Run `git init` in the directory when it is not a repository, and make an empty initial
    /// commit when the repository has no HEAD. Without this flag git is not run at all.
    #[arg(long)]
    git_init: bool,
    /// Bind the project's tracker to this GitHub repository, as `owner/repo`.
    #[arg(long, value_name = "OWNER/REPO")]
    github: Option<String>,
    /// Where the GitHub credential comes from: `app` (the default with --github) or `env`.
    #[arg(long, value_parser = parse_credential, requires = "github")]
    credential: Option<Credential>,
    /// What the project is for; the workspace area's purpose.
    #[arg(long)]
    purpose: Option<String>,
    /// Author recording the creation (the initial commit and the area). Default: `fl`.
    #[arg(long)]
    by: Option<String>,
    /// Print the created project as JSON.
    #[arg(long)]
    json: bool,
}

fn parse_credential(s: &str) -> Result<Credential, String> {
    s.parse()
}

impl Cmd {
    /// Every item this command names, by `Ref` — the single source `iris()`
    /// and `has_handle()` both derive from, so a `Ref` field added to a
    /// variant here is picked up by both at once.
    /// No variant names an existing item: `Add`'s `path` is a filesystem path
    /// for a project not yet registered, `Create` makes one and `List` takes
    /// none at all.
    fn refs(&self) -> Vec<&Ref> {
        Vec::new()
    }

    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }

    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }

    /// The directory `Add` registers. The store is chosen by ITS binding in
    /// the config, not the current directory's: a project registered from
    /// elsewhere must land in the store it is bound to.
    pub fn root(&self) -> Option<&std::path::Path> {
        match self {
            Cmd::Add { path } => Some(std::path::Path::new(path)),
            Cmd::List { .. } | Cmd::Create(_) | Cmd::Delete(_) => None,
        }
    }
}

pub fn run(store: &AnyStore, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Create(_) | Cmd::List { .. } | Cmd::Delete(_) => {
            bail!(
                "`fl project create`, `list` and `delete` open their own stores; they are not run on a bound one"
            )
        }
        Cmd::Add { path } => {
            // ⚠ This used to store the string unexamined, so a typo became a
            // registered project and surfaced later as a git error from
            // `gate add` — a refusal about the wrong thing, at the wrong
            // time, naming neither the path nor the mistake.
            let given = std::path::Path::new(&path);
            if !given.is_dir() {
                bail!("`{path}` does not exist as a directory, so it cannot be a project root.");
            }
            // Store an absolute, symlink-resolved root: gates resolve their
            // population against it from whatever directory the CLI is run
            // in, so a relative root would name a different tree each time.
            let root = given.canonicalize().map_err(|e| {
                anyhow::anyhow!("`{path}` could not be resolved to a real path: {e}")
            })?;
            let root = root.display().to_string();
            // Git is not required: a directory that is not a repository is a project too.
            // A gate stamps its provenance with a commit, so `fl gate add` asks for a HEAD
            // when it runs, and says so then.
            let id = store.add_project(&root)?;
            println!("{}\t{root}", refs::show(store, Kind::Project, id.iri())?);
        }
    }
    Ok(0)
}

/// `path` made absolute against `cwd`, with `.` and `..` resolved lexically.
fn absolute(cwd: &Path, path: &str) -> Result<PathBuf> {
    if path.trim().is_empty() {
        bail!("the path is empty");
    }
    let mut out = PathBuf::new();
    for c in cwd.join(path).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// The places `fl project create` and `fl project list` work in: this machine's config file, the
/// workspace store (`--db` / `$FL_DB`, else `[state].store`, else the default) and the directory
/// of per-project stores beside it.
fn places(
    cfg: &Config,
    cwd: &Path,
    explicit_db: Option<PathBuf>,
    default_store: &Path,
) -> Result<InitPaths> {
    let config_path = config::path().context(
        "neither an absolute $XDG_CONFIG_HOME nor $HOME is set, so there is no config file to \
         register the project in",
    )?;
    let configured = cfg.workspace_state_store(default_store);
    let store_dir = configured
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf());
    Ok(InitPaths {
        config_path,
        store_dir,
        workspace_store: explicit_db.unwrap_or(configured),
    })
}

fn open_err(what: &str, path: &Path, e: &dyn std::fmt::Display) -> InitError {
    InitError::Failed(format!(
        "could not open the {what} at {}: {e}",
        path.display()
    ))
}

fn open_workspace(path: &Path) -> Result<Box<AnyStore>, InitError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fl_store::private::create_private_dir_all(dir)
            .map_err(|e| open_err("workspace store", path, &e))?;
    }
    fl_store::open_store(path)
        .map(Box::new)
        .map_err(|e| open_err("workspace store", path, &e))
}

/// `fl project create` (alias `init`): runs [`project_init::init_project`] or
/// [`project_init::create_virtual`] with this machine's config file and stores.
pub fn create(
    args: CreateArgs,
    cfg: &Config,
    cwd: &Path,
    explicit_db: Option<PathBuf>,
    default_store: &Path,
) -> Result<i32> {
    let paths = places(cfg, cwd, explicit_db, default_store)?;
    let by = args.by.clone().unwrap_or_else(|| "fl".to_string());
    let outcome = if args.virtual_group {
        let request = VirtualRequest {
            slug: args.slug.clone(),
            display_name: args.target.clone(),
            purpose: args.purpose.clone(),
            by,
        };
        project_init::create_virtual(&request, &paths, || open_workspace(&paths.workspace_store))
    } else {
        // With `--path` the argument is the name; without, it is the root (the `init` form).
        let (path, display_name) = match &args.path {
            Some(path) => (path.clone(), Some(args.target.clone())),
            None => (args.target.clone(), args.name.clone()),
        };
        let request = InitRequest {
            path: absolute(cwd, &path)?,
            slug: args.slug.clone(),
            display_name,
            github: args.github.clone(),
            credential: args.credential,
            purpose: args.purpose.clone(),
            git_init: args.git_init,
            by,
        };
        project_init::init_project(
            &request,
            &paths,
            || open_workspace(&paths.workspace_store),
            |r| {
                fl_store::open_store(&r.store)
                    .map(Box::new)
                    .map_err(|e| open_err("store", &r.store, &e))
            },
        )
    }
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(git) = &outcome.git {
        if git.initialized {
            eprintln!(
                "notice: ran `git init` in {}",
                outcome.root.as_deref().unwrap_or("-")
            );
        }
        if git.initial_commit {
            eprintln!(
                "notice: made the initial empty commit {} so the project has a HEAD",
                git.head
            );
        }
    }
    let workspace = open_workspace(&paths.workspace_store)?;
    let held = workspace.list_projects()?;
    let summary = project_list::list_projects(&paths.config_path, &[], &held, &*workspace)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .into_iter()
        .find(|p| p.slug == outcome.slug)
        .context("the project is not in the list after it was created")?;
    if args.json {
        let mut out = serde_json::to_value(&summary)?;
        if let (Some(map), serde_json::Value::Object(extra)) = (
            out.as_object_mut(),
            serde_json::json!({
                "registered": outcome.registered,
                "git": outcome.git,
                "mcp_catalog_created": outcome.mcp_catalog_created,
                "areas": outcome.areas,
                "config": outcome.config,
            }),
        ) {
            map.extend(extra);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "{}\t{}\t{}\t{}",
            summary.slug,
            summary.kind.as_str(),
            summary.path.as_deref().unwrap_or("-"),
            summary.store_path.as_deref().unwrap_or("-")
        );
    }
    Ok(0)
}

/// `fl project list`: [`project_list::list_projects`] over this machine's config, the workspace
/// store and the store the current directory is bound to (`bound`, with its path), the list
/// `GET /api/v1/projects` serves.
pub fn list(
    json: bool,
    cfg: &Config,
    cwd: &Path,
    explicit_db: Option<PathBuf>,
    default_store: &Path,
    bound: (&AnyStore, &Path),
) -> Result<i32> {
    let paths = places(cfg, cwd, explicit_db, default_store)?;
    // Listing creates nothing. The bound store is open already; the workspace store is the same
    // file unless it is not, and then it is looked at only if it exists (read-only, no
    // migration). With none, the registry is empty.
    let own;
    let workspace: Option<&AnyStore> =
        if fl_store::store_identity(bound.1) == fl_store::store_identity(&paths.workspace_store) {
            Some(bound.0)
        } else if fl_store::store_exists(&paths.workspace_store) {
            own = fl_store::open_store_for_scan(&paths.workspace_store)
                .map_err(|e| open_err("workspace store", &paths.workspace_store, &e))?;
            Some(&own)
        } else {
            None
        };
    let mut held = match workspace {
        Some(w) => w.list_projects()?,
        None => Vec::new(),
    };
    if !workspace.is_some_and(|w| std::ptr::eq(w, bound.0)) {
        for p in bound.0.list_projects()? {
            if !held.iter().any(|h| h.root == p.root) {
                held.push(p);
            }
        }
    }
    let projects = match workspace {
        Some(w) => project_list::list_projects(&paths.config_path, &[], &held, w),
        None => project_list::list_projects(
            &paths.config_path,
            &[],
            &held,
            &fl_core::MemStore::default(),
        ),
    }
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&projects)?);
    } else {
        for p in projects {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                p.kind.as_str(),
                p.slug,
                p.path.as_deref().unwrap_or("-"),
                p.tracker.as_ref().map_or("-", |t| t.github.as_str()),
                p.active_agent_count
            );
        }
    }
    Ok(0)
}

/// `fl project delete`: [`project_delete::delete_project`] with this machine's config file and
/// stores, the flow `DELETE /api/v1/projects/:slug` runs. Exit 0 when the project was
/// unregistered (a failed file deletion is on stderr and in `error`), 2 when active agents
/// refuse it, 1 for any other refusal.
pub fn delete(
    args: DeleteArgs,
    cfg: &Config,
    cwd: &Path,
    explicit_db: Option<PathBuf>,
    default_store: &Path,
) -> Result<i32> {
    let paths = places(cfg, cwd, explicit_db, default_store)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    // The workspace is looked for at the current directory, the home directory (and just below
    // it) and the `fl` executable; the project's own ancestors are looked at by the deletion.
    // With none found, a directory is not deleted.
    let homes: Vec<&Path> = home.iter().map(PathBuf::as_path).collect();
    let exe = std::env::current_exe().ok();
    let workspace_roots = project_delete::locate_workspace_roots(&[cwd], &homes, exe.as_deref());
    // The stores in the data directory, as the Hub knows them, so another project's store is
    // recognised here as it is there.
    let hints = project_list::scan_store_dir(&paths.store_dir, "fls");
    let env = LifecycleEnv::new(None);
    let ctx = DeleteContext {
        paths: &paths,
        stores: &hints,
        home: home.as_deref(),
        workspace_roots: &workspace_roots,
        lifecycle: &env,
    };
    let request = DeleteRequest {
        reassign_to: args.reassign_to.clone(),
        decommission_agents: args.decommission_agents,
        delete_store: args.delete_store,
        delete_dir: args.delete_dir,
    };
    let by = args.by.clone().unwrap_or_else(|| "fl".to_string());
    let outcome = match project_delete::delete_project(
        &args.slug,
        &request,
        &by,
        &ctx,
        || open_workspace(&paths.workspace_store),
        |_| {},
    ) {
        Ok(outcome) => outcome,
        Err(DeleteError::ActiveAgents { agents }) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&DeleteError::active_agents_body(&agents))?
                );
            }
            eprintln!(
                "project `{}` has active agents ({}): pass --reassign-to <slug> or \
                 --decommission-agents",
                args.slug.trim(),
                agents.join(", ")
            );
            return Ok(2);
        }
        Err(DeleteError::Init(e)) => return Err(anyhow::anyhow!("{e}")),
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!(
            "deleted\t{}\treassigned={}\tdecommissioned={}\tstore_deleted={}\tdir_deleted={}",
            outcome.deleted,
            outcome.reassigned_agents,
            outcome.decommissioned_agents,
            outcome.store_deleted,
            outcome.dir_deleted
        );
    }
    if let Some(error) = &outcome.error {
        eprintln!("error: {error}");
    }
    Ok(0)
}
