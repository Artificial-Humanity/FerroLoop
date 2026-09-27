mod cmd;
mod config;
mod ctx;
mod refs;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ctx::Ctx;
use fl_core::{CatalogChecked, Iri, KindRouted, StoreError};
use fl_store::RedbStore;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "fl", version, about = "Gate an action before it costs you")]
struct Cli {
    /// Path to the store. CONFINES the command to it: an IRI it does not
    /// hold is refused, not searched for elsewhere. Falls back to $FL_DB
    /// (same), then the project bound in $XDG_CONFIG_HOME/fl/config.toml
    /// (default ~/.config/fl/config.toml), then the XDG data directory.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(subcommand)]
    Project(cmd::project::Cmd),
    #[command(subcommand)]
    Gate(cmd::gate::Cmd),
    #[command(subcommand)]
    Transition(cmd::transition::Cmd),
    #[command(subcommand)]
    Record(cmd::record::Cmd),
    /// Evaluate a transition's gates and exit per the check contract.
    Check(cmd::check::Cmd),
    #[command(subcommand)]
    Finding(cmd::finding::Cmd),
    /// Run one adapter attempt against a record and record the outcome.
    Attempt(cmd::attempt::Cmd),
    /// Report what a project's recorded attempts cost.
    Stats(cmd::stats::Cmd),
    /// Share a project's gates through a committed manifest.
    #[command(subcommand)]
    Manifest(cmd::manifest::Cmd),
    /// GitHub tracker: who fl writes as, and repair of a diverged issue.
    #[command(subcommand)]
    Github(cmd::github::Cmd),
}

impl Command {
    /// Every item the chosen subcommand names, dispatched to its own
    /// `iris()` (spec §2.6).
    fn iris(&self) -> Vec<Iri> {
        match self {
            Command::Project(c) => c.iris(),
            Command::Gate(c) => c.iris(),
            Command::Transition(c) => c.iris(),
            Command::Record(c) => c.iris(),
            Command::Check(c) => c.iris(),
            Command::Finding(c) => c.iris(),
            Command::Attempt(c) => c.iris(),
            Command::Stats(c) => c.iris(),
            Command::Manifest(c) => c.iris(),
            Command::Github(c) => c.iris(),
        }
    }

    /// The directory whose binding in the config picks the store, when it
    /// is not the current directory: `project add <path>` registers `<path>`,
    /// so the store bound to `<path>` is the one it belongs in (Final
    /// review, item 2). Every other command works on the current project.
    fn project_root(&self) -> Option<&Path> {
        match self {
            Command::Project(c) => c.root(),
            Command::Manifest(c) => c.root(),
            _ => None,
        }
    }

    /// Whether the chosen subcommand names any item by handle rather than
    /// IRI, dispatched to its own `has_handle()` (Fix round 1, item 1).
    fn has_handle(&self) -> bool {
        match self {
            Command::Project(c) => c.has_handle(),
            Command::Gate(c) => c.has_handle(),
            Command::Transition(c) => c.has_handle(),
            Command::Record(c) => c.has_handle(),
            Command::Check(c) => c.has_handle(),
            Command::Finding(c) => c.has_handle(),
            Command::Attempt(c) => c.has_handle(),
            Command::Stats(c) => c.has_handle(),
            Command::Manifest(c) => c.has_handle(),
            Command::Github(c) => c.has_handle(),
        }
    }

    /// Whether the command reads or writes records or findings. Only these
    /// open the tracker, so a catalog command never contacts GitHub.
    fn needs_tracker(&self) -> bool {
        match self {
            Command::Record(_) | Command::Finding(_) | Command::Attempt(_) => true,
            Command::Github(_) => true,
            // `check` is the CI gate: it touches the tracker only to resolve
            // `--record`, and must not need GitHub otherwise.
            Command::Check(c) => c.record.is_some(),
            Command::Project(_)
            | Command::Gate(_)
            | Command::Transition(_)
            | Command::Stats(_)
            | Command::Manifest(_) => false,
        }
    }
}

/// The host `$FL_GITHUB_API_URL` sends the GitHub credential to, if fl may
/// send it there: `https`, or `http` to this machine (`127.0.0.1`,
/// `localhost`, `[::1]`).
///
/// ⚠ Read with the parser the request itself uses (`http::Uri`, through
/// ureq), never by string prefix: `http://127.0.0.1:1@example.com` starts
/// like loopback, but its host is `example.com`. An authority carrying `@`
/// (a user name or password) is refused under any scheme, and so is
/// anything that does not parse.
fn api_override_host(url: &str) -> Result<String> {
    let refuse = |why: String| {
        anyhow::anyhow!(
            "$FL_GITHUB_API_URL is `{url}`: {why}. fl sends the GitHub credential there, so it \
             must be https://, or http:// to this machine (127.0.0.1, localhost or [::1]). \
             Unset it to use GitHub"
        )
    };
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|e| refuse(format!("it is not a URL ({e})")))?;
    let Some(authority) = uri.authority() else {
        return Err(refuse("it names no host".to_string()));
    };
    if authority.as_str().contains('@') {
        return Err(refuse(
            "it carries a user name or password before the host".to_string(),
        ));
    }
    let host = authority.host();
    // `host()` keeps the brackets on an IPv6 address: `[::1]`.
    match uri.scheme_str() {
        Some("https") => {}
        Some("http") if matches!(host, "127.0.0.1" | "localhost" | "[::1]") => {}
        _ => {
            return Err(refuse(format!(
                "its host `{host}` is not this machine, and it is not https"
            )));
        }
    }
    Ok(host.to_string())
}

fn open_github(
    b: &config::TrackerBinding,
    app: Option<&config::GithubApp>,
    store: &RedbStore,
) -> Result<fl_github::GithubTracker> {
    let api = match std::env::var("FL_GITHUB_API_URL") {
        Err(_) => fl_github::DEFAULT_API.to_string(),
        // ⚠ For tests. The credential goes wherever this points, so only
        // https, or plain http to this machine, is accepted — and said.
        Ok(url) => {
            let host = api_override_host(&url)?;
            eprintln!("notice: $FL_GITHUB_API_URL is set; talking to {host}, not GitHub");
            url
        }
    };
    let creds: Box<dyn fl_github::Credentials> = match b.credential {
        config::Credential::Env => Box::new(fl_github::EnvToken::from_env()?),
        config::Credential::App => {
            let Some(app) = app else {
                bail!(
                    "`credential = \"app\"` needs a `[github]` section with `app_id` and \
                     `private_key` in the config"
                );
            };
            Box::new(fl_github::AppCredentials::from_file(
                &api,
                app.app_id,
                &app.private_key,
                &b.github,
            )?)
        }
    };
    let (tracker, notice) =
        fl_github::GithubTracker::open(fl_github::Client::new(&api, creds), &b.github, store)?;
    if let Some(n) = notice {
        eprintln!("notice: {n}");
    }
    Ok(tracker)
}

/// The store `--db`, then `$FL_DB`, names, if either does. Either one
/// CONFINES the command to that store (see [`db_path`]).
fn explicit_db(flag: Option<PathBuf>) -> Option<PathBuf> {
    flag.or_else(|| std::env::var("FL_DB").ok().map(PathBuf::from))
}

/// Resolve the store path from `explicit` (`--db`, then `$FL_DB`), then
/// `configured` — the store the config binds to the project at the locus —
/// then the XDG data directory, then `~/.local/share`, and whether that tier
/// CONFINES the command to this one store. The locus is the current
/// directory, except for `project add`, where it is the directory being
/// registered.
///
/// `$XDG_DATA_HOME` follows the rule `config::path` applies to
/// `$XDG_CONFIG_HOME`: an empty or relative value is ignored, never used
/// as-is — used as-is, it would put the store relative to whatever
/// directory the command happened to run in.
///
/// ⚠ Ruling (Fix round 1, item 0): `--db` and `$FL_DB` CONFINE the command
/// to exactly the store they name. An IRI that store does not hold is
/// `NotOwned` naming only that one store — never a search across every
/// store any project happens to be bound to in the config. Only the
/// config-binding and XDG-default tiers search; `choose_store` reads this
/// back to decide whether to expand its candidate list at all.
///
/// ⚠ Every tier ends the same way: the parent directory of the resolved path
/// is created if it does not exist. Earlier this only happened on the
/// `$XDG_DATA_HOME`/`$HOME` branch, so setting `$FL_DB` (or `--db`) to a path
/// whose directory did not exist yet fell straight through to a raw redb I/O
/// error — the same class of silent inconsistency this task's actionable-
/// refusal rule exists to close, just moved one layer down into the
/// database open call instead of being refused here. Rather than add a
/// fourth distinct refusal message for that case, every tier now gets the
/// same treatment `$XDG_DATA_HOME`/`$HOME` already had: create the directory
/// that will hold the store. `--db path/to/db` and `$FL_DB=path/to/db`
/// behave identically to each other and to the XDG fallback again.
fn db_path(explicit: Option<PathBuf>, configured: Option<PathBuf>) -> Result<(PathBuf, bool)> {
    let (path, confined) = if let Some(p) = explicit {
        (p, true)
    } else if let Some(p) = configured {
        (p, false)
    } else {
        let base = config::data_dir(
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        )
        .context("neither --db, $FL_DB, a project bound in the config, an absolute $XDG_DATA_HOME nor $HOME is set, so there is nowhere to put the store")?;
        (base.join("fl").join("fl.redb"), false)
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    Ok((path, confined))
}

/// A full IRI on the command line selects the store that holds it (spec
/// §2.6). Handles resolve only in the bound store, so a command with no IRI
/// uses the bound store. `confined` is true only for `--db`/`$FL_DB` (Fix
/// round 1, item 0, ruling): an explicit store CONFINES the search to
/// itself — the config's other stores are never even considered — so an IRI
/// it does not hold is `NotOwned` naming only that one store.
fn choose_store(
    bound: &Path,
    entries: &[config::Entry],
    iris: &[Iri],
    confined: bool,
) -> Result<PathBuf> {
    if iris.is_empty() {
        return Ok(bound.to_path_buf());
    }
    if !confined {
        let mut candidates = vec![bound.to_path_buf()];
        for e in entries {
            if !candidates.contains(&e.store) {
                candidates.push(e.store.clone());
            }
        }
        // Never create a store while searching: only files that exist are
        // stores.
        candidates.retain(|c| c.exists());
        return choose_among(&candidates, iris);
    }

    // ⚠ Fix round 2, item 2: confined mode has exactly one candidate —
    // `bound` itself — but looking up an IRI must never create a store
    // (`RedbStore::open` calls `Database::create`, which does). If `bound`
    // does not exist yet, every id in `iris` is `NotOwned` by construction;
    // report the first one, naming `bound` as searched, without ever
    // opening it.
    if !bound.exists() {
        return Err(StoreError::NotOwned {
            id: iris[0].clone(),
            searched: vec![bound.display().to_string()],
        }
        .into());
    }
    choose_among(&[bound.to_path_buf()], iris)
}

/// The common search loop, over whatever candidate list the caller already
/// decided on (confined: `[bound]`, unconfined: `bound` plus every
/// configured store that exists).
fn choose_among(candidates: &[PathBuf], iris: &[Iri]) -> Result<PathBuf> {
    let mut chosen: Option<PathBuf> = None;
    for id in iris {
        let mut owners = Vec::new();
        for c in candidates {
            // ⚠ A store that cannot be opened is an ERROR here, not a
            // "doesn't have it": skipping it would search less than it says.
            let s = RedbStore::open(c)
                .with_context(|| format!("could not open the store at {}", c.display()))?;
            if s.owns(id)? {
                owners.push(c.clone());
            }
        }
        match owners.as_slice() {
            [] => {
                return Err(StoreError::NotOwned {
                    id: id.clone(),
                    searched: candidates.iter().map(|c| c.display().to_string()).collect(),
                }
                .into());
            }
            [one] => match &chosen {
                Some(prev) if prev != one => bail!(
                    "this command names items in two different stores ({} and {}); name items from one store",
                    prev.display(),
                    one.display()
                ),
                _ => chosen = Some(one.clone()),
            },
            many => bail!(
                "{id} is held by more than one store: {}. Refusing to pick one. Name the store \
                 to use with `--db <path>` (or `$FL_DB`).",
                many.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    Ok(chosen.expect("iris is non-empty"))
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let cfg = config::load(config::path().as_deref())?;
    let entries = &cfg.projects;
    let locus = match cli.command.project_root() {
        Some(root) => cwd.join(root),
        None => cwd.clone(),
    };
    let needs_tracker = cli.command.needs_tracker();
    let explicit = explicit_db(cli.db);
    // The project's config entry, read once, and only when something needs
    // it: without `--db`/`$FL_DB` it picks the store; for a command that
    // needs the tracker it names the tracker. `--db`/`$FL_DB` with any other
    // command never reads it, so an ambiguous config cannot block that
    // escape hatch — but a tracker command with an ambiguous config is
    // refused, because it cannot know its tracker.
    let entry = if explicit.is_none() || needs_tracker {
        config::bound_entry(entries, &locus)?
    } else {
        None
    };
    let configured = entry.as_ref().map(|e| e.store.clone());
    let binding = entry.and_then(|e| e.tracker);
    // Before any store's directory is created, searched or opened: `fl
    // github` with `--db` still reads the entry (it needs the tracker), so
    // this fires there too.
    if matches!(cli.command, Command::Github(_)) && binding.is_none() {
        bail!(
            "`fl github` needs a tracker binding: add `tracker = {{ github = \"owner/repo\", \
             credential = \"env\" }}` to this project's entry in {}",
            config::path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "the config".into())
        );
    }
    // A bound project's node binding and catalog live in the store its
    // config entry names; `--db` would pair GitHub with another catalog.
    // ⚠ Before `db_path`, `choose_store` and any open: a refused command
    // must create no directory or store, and an IRI it names must not be
    // refused as not owned by the `--db` store first.
    if binding.is_some() && explicit.is_some() && needs_tracker {
        bail!(
            "this project's tracker is bound to GitHub in the config, so it uses the store its \
             config entry names. Drop --db (and unset $FL_DB) for this command"
        );
    }
    let (bound, confined) = db_path(explicit, configured)?;
    let mut iris = cli.command.iris();
    // A GitHub issue URL is the tracker's to resolve: no local store holds
    // one, and searching them would refuse it as NotOwned (spec §2.2).
    if binding.is_some() {
        iris.retain(|i| !fl_github::meta::is_issue_url(i));
    }
    let path = choose_store(&bound, entries, &iris, confined)?;
    // ⚠ Fix round 1, item 1: a handle resolves only in the store it was
    // read from. If an IRI elsewhere in this same command sent the search
    // to a DIFFERENT store than the bound one, a handle alongside it would
    // silently resolve against that other store's numbering instead —
    // refuse rather than guess which store the person meant.
    if path != bound && cli.command.has_handle() {
        bail!(
            "this command names a handle as well as an IRI held by a different store ({}); \
             a handle resolves only in the bound store at {}. Name every item by its full \
             IRI instead of a handle.",
            path.display(),
            bound.display()
        );
    }
    let store = RedbStore::open(&path)
        .with_context(|| format!("could not open the store at {}", path.display()))?;
    let github = match (&binding, needs_tracker) {
        (Some(b), true) => Some(open_github(b, cfg.github.as_ref(), &store)?),
        _ => None,
    };
    let (checked, routed);
    let ctx = match &github {
        Some(gh) => {
            checked = CatalogChecked {
                catalog: &store,
                tracker: gh,
            };
            routed = KindRouted {
                catalog: &store,
                tracker: gh,
            };
            Ctx {
                store: &store,
                tracker: &checked,
                handles: &routed,
                github: Some(gh),
                tracker_label: format!("github:{}", gh.repo().full_name),
            }
        }
        None => Ctx {
            store: &store,
            tracker: &store,
            handles: &store,
            github: None,
            tracker_label: store.label().to_string(),
        },
    };
    match cli.command {
        Command::Project(c) => cmd::project::run(&store, c),
        Command::Gate(c) => cmd::gate::run(&store, c),
        Command::Transition(c) => cmd::transition::run(&store, c),
        Command::Record(c) => cmd::record::run(&ctx, c),
        Command::Check(c) => cmd::check::run(&ctx, c),
        Command::Finding(c) => cmd::finding::run(&ctx, c),
        Command::Attempt(c) => cmd::attempt::run(&ctx, c),
        Command::Stats(c) => cmd::stats::run(&store, c),
        Command::Manifest(c) => cmd::manifest::run(&store, c),
        Command::Github(c) => cmd::github::run(&ctx, c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_api_override_is_refused_unless_https_or_this_machine() {
        for url in [
            "http://127.0.0.1:1@example.com",
            "http://localhost:x@example.com/",
            "https://user@ghe.example/api/v3",
            "http://127.0.0.1.example.com",
            "http://example.com",
            "ftp://127.0.0.1/",
            "127.0.0.1:8080",
            "",
        ] {
            let err = api_override_host(url).expect_err(url);
            assert!(format!("{err:#}").contains("https://"), "{url}: {err:#}");
        }
    }

    #[test]
    fn an_api_override_to_https_or_this_machine_is_accepted_naming_the_host() {
        for (url, host) in [
            ("http://127.0.0.1:43227", "127.0.0.1"),
            ("http://localhost:43227", "localhost"),
            ("http://[::1]:43227/", "[::1]"),
            ("https://ghe.example/api/v3", "ghe.example"),
        ] {
            assert_eq!(api_override_host(url).unwrap(), host, "{url}");
        }
    }
}
