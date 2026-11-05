use crate::config::{self, Config};
use crate::refs::{self, Ref};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use fl_core::store::Catalog;
use fl_core::{Iri, Kind};
use fl_hub::project_init::{self, Credential, InitError, InitPaths, InitRequest};
use fl_store::AnyStore;
use std::path::{Component, Path, PathBuf};

#[derive(Subcommand)]
pub enum Cmd {
    /// Register a project directory.
    Add { path: String },
    /// List registered projects.
    List,
    /// Make a directory a project in one step: a git working tree with a HEAD (`git init` and
    /// an initial empty commit when it has none), a `[[project]]` block in the config, a
    /// store holding the project and the default local routing areas, its workspace area,
    /// and a starter `.fl/mcp.toml`. Running it again changes nothing already right.
    Init(InitArgs),
}

/// The arguments of `fl project init`.
#[derive(Args)]
pub struct InitArgs {
    /// The project root. It is made if it does not exist.
    path: String,
    /// The project's name: lowercase letters, digits, `-` and `_`, starting with a letter.
    /// Without it, the root's lowercase directory name.
    #[arg(long)]
    name: Option<String>,
    /// Bind the project's tracker to this GitHub repository, as `owner/repo`.
    #[arg(long, value_name = "OWNER/REPO")]
    github: Option<String>,
    /// Where the GitHub credential comes from: `app` (the default with --github) or `env`.
    #[arg(long, value_parser = parse_credential, requires = "github")]
    credential: Option<Credential>,
    /// What the project is for; the workspace area's purpose.
    #[arg(long)]
    purpose: Option<String>,
    /// Author recording the initialization (the initial commit and the area). Default: `fl`.
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
    /// Neither variant names an existing item: `Add`'s `path` is a
    /// filesystem path for a project not yet registered, and `List` takes
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
            Cmd::List | Cmd::Init(_) => None,
        }
    }
}

pub fn run(store: &AnyStore, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Init(_) => {
            bail!("`fl project init` opens its own stores; it is not run on a bound one")
        }
        Cmd::Add { path } => {
            // ⚠ This used to store the string unexamined, so a typo became a
            // registered project and surfaced later as a git error from
            // `gate add` — a refusal about the wrong thing, at the wrong
            // time, naming neither the path nor the mistake.
            let given = std::path::Path::new(&path);
            if !given.is_dir() {
                bail!(
                    "`{path}` does not exist as a directory, so it cannot be a project root. \
                     A project is a git working tree on this machine."
                );
            }
            // Store an absolute, symlink-resolved root: gates resolve their
            // population against it from whatever directory the CLI is run
            // in, so a relative root would name a different tree each time.
            let root = given.canonicalize().map_err(|e| {
                anyhow::anyhow!("`{path}` could not be resolved to a real path: {e}")
            })?;
            let root = root.display().to_string();
            fl_exec::git::Git::head(std::path::Path::new(&root)).map_err(|e| {
                anyhow::anyhow!(
                    "`{root}` is not a git working tree: {e}. A project must be one, because a \
                     gate's provenance is a commit — there has to be a HEAD to stamp it against."
                )
            })?;
            let id = store.add_project(&root)?;
            println!("{}\t{root}", refs::show(store, Kind::Project, id.iri())?);
        }
        Cmd::List => {
            for p in store.list_projects()? {
                println!(
                    "{}\t{}",
                    refs::show(store, Kind::Project, p.id.iri())?,
                    p.root
                );
            }
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

/// `fl project init`: runs [`project_init::init_project`] with this machine's config file and
/// stores. The workspace store is `--db` / `$FL_DB`, else `[state].store`, else the default;
/// the project's own store goes beside the workspace store.
pub fn init(
    args: InitArgs,
    cfg: &Config,
    cwd: &Path,
    explicit_db: Option<PathBuf>,
    default_store: &Path,
) -> Result<i32> {
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
    let workspace_store = explicit_db.unwrap_or(configured);
    let request = InitRequest {
        path: absolute(cwd, &args.path)?,
        name: args.name,
        github: args.github,
        credential: args.credential,
        purpose: args.purpose,
        by: args.by.unwrap_or_else(|| "fl".to_string()),
    };
    let paths = InitPaths {
        config_path,
        store_dir,
        workspace_store: workspace_store.clone(),
    };
    let open_err = |what: &str, path: &Path, e: &dyn std::fmt::Display| {
        InitError::Failed(format!(
            "could not open the {what} at {}: {e}",
            path.display()
        ))
    };
    let outcome = project_init::init_project(&request, &paths, |r| {
        let project =
            fl_store::open_store(&r.store).map_err(|e| open_err("store", &r.store, &e))?;
        if let Some(dir) = workspace_store
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
        {
            fl_store::private::create_private_dir_all(dir)
                .map_err(|e| open_err("workspace store", &workspace_store, &e))?;
        }
        let workspace = fl_store::open_store(&workspace_store)
            .map_err(|e| open_err("workspace store", &workspace_store, &e))?;
        Ok((Box::new(project), Box::new(workspace)))
    })
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    if outcome.git.initialized {
        eprintln!("notice: ran `git init` in {}", outcome.root);
    }
    if outcome.git.initial_commit {
        eprintln!(
            "notice: made the initial empty commit {} so the project has a HEAD",
            outcome.git.head
        );
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!("{}\t{}\t{}", outcome.name, outcome.root, outcome.store);
    }
    Ok(0)
}
