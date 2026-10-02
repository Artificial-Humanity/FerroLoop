mod cmd;
mod config;
mod ctx;
mod preflight;
mod refs;
#[cfg(test)]
mod testing;

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
    /// so the store bound to `<path>` is the one it belongs in. Every other
    /// command works on the current project.
    fn project_root(&self) -> Option<&Path> {
        match self {
            Command::Project(c) => c.root(),
            Command::Manifest(c) => c.root(),
            _ => None,
        }
    }

    /// Whether the chosen subcommand names any item by handle rather than
    /// IRI, dispatched to its own `has_handle()`.
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
/// ⚠ `--db` and `$FL_DB` CONFINE the command to exactly the store they
/// name. An IRI that store does not hold is `NotOwned` naming only that one
/// store — never a search across every store any project happens to be
/// bound to in the config. Only the config-binding and XDG-default tiers
/// search; `choose_store` reads this back to decide whether to expand its
/// candidate list at all.
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
/// uses the bound store. `confined` is true only for `--db`/`$FL_DB`: an
/// explicit store CONFINES the search to itself — the config's other stores
/// are never even considered — so an IRI it does not hold is `NotOwned`
/// naming only that one store.
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

    // ⚠ Confined mode has exactly one candidate —
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

/// Whether two store paths name the same store: the same path, or, when
/// both exist, the same file once symlinks are resolved.
fn same_store(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(x), Ok(y)) if x == y
        )
}

/// How a config entry's tracker reads in a refusal.
fn tracker_name(t: Option<&config::TrackerBinding>) -> String {
    match t {
        Some(t) if t.github_ledger() => format!("GitHub `{}` with its GitHub ledger", t.github),
        Some(t) => format!("GitHub `{}`", t.github),
        None => "the store's own tracker".to_string(),
    }
}

/// The one tracker every config entry bound to `chosen`'s store agrees on
/// (`None` if none binds one), refusing when they disagree.
///
/// ⚠ Shared by every caller that trusts a config entry's tracker for a
/// store it did not itself pick by root alone: `tracker_for`, for the
/// tracker commands, and `manifest export`'s ledger-root binding — a store
/// bound to more than one tracker in the config is exactly the config state
/// in which trusting any one entry's tracker for that store can attribute
/// an item, or a ledger root, to the wrong repository.
fn store_tracker(
    chosen: &Path,
    entries: &[config::Entry],
) -> Result<Option<config::TrackerBinding>> {
    let owners: Vec<&config::Entry> = entries
        .iter()
        .filter(|e| same_store(&e.store, chosen))
        .collect();
    let mut trackers: Vec<Option<&config::TrackerBinding>> = Vec::new();
    for e in &owners {
        if !trackers.contains(&e.tracker.as_ref()) {
            trackers.push(e.tracker.as_ref());
        }
    }
    if trackers.len() > 1 {
        let names = owners
            .iter()
            .map(|e| {
                format!(
                    "{} -> {}",
                    e.root.display(),
                    tracker_name(e.tracker.as_ref())
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "the store at {} is bound to more than one tracker in the config: {names}. A store \
             has one tracker: give these entries the same `tracker`, or give each its own store",
            chosen.display()
        );
    }
    Ok(trackers.first().copied().flatten().cloned())
}

/// The tracker for a command that reads or writes records or findings, and
/// works on the store at `chosen`.
///
/// ⚠ The binding is taken from the config entries whose `store` IS `chosen`
/// — the store holds the node binding and the catalog GitHub is paired with
/// — and entries naming the same store with different trackers are refused
/// ([`store_tracker`]). And when `chosen` is not the store of `here` (the
/// current directory's entry, if any) and EITHER side is bound to GitHub,
/// the command is refused: the store's own project could then be written
/// through the wrong tracker, or this directory's GitHub tracker paired
/// with another project's store. Unbound projects on both sides pass
/// exactly as before.
fn tracker_for(
    chosen: &Path,
    here: Option<&config::Entry>,
    entries: &[config::Entry],
    explicit: bool,
) -> Result<Option<config::TrackerBinding>> {
    let store_side = store_tracker(chosen, entries)?;
    let here_is_chosen = here.is_some_and(|e| same_store(&e.store, chosen));
    let here_side = here.and_then(|e| e.tracker.as_ref());
    if !here_is_chosen && (store_side.is_some() || here_side.is_some()) {
        let owners: Vec<&config::Entry> = entries
            .iter()
            .filter(|e| same_store(&e.store, chosen))
            .collect();
        let theirs = if owners.is_empty() {
            "no project in the config".to_string()
        } else {
            let roots = owners
                .iter()
                .map(|e| e.root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "the project at {roots}, whose tracker is {}",
                tracker_name(store_side.as_ref())
            )
        };
        let ours = match here {
            Some(e) => format!(
                "the project at {} (store {}, tracker {})",
                e.root.display(),
                e.store.display(),
                tracker_name(here_side)
            ),
            None => "no project in the config".to_string(),
        };
        let without_db = if explicit {
            ", without --db (and with $FL_DB unset)"
        } else {
            ""
        };
        bail!(
            "this command works on the store at {}, which belongs to {theirs}; the current \
             directory belongs to {ours}. A project bound to GitHub keeps its records and \
             findings in its own tracker, paired with its own store, so fl will not mix the two. \
             Run the command from the root of the project that holds the item{without_db}",
            chosen.display()
        );
    }
    Ok(store_side)
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
    let entry_read = explicit.is_none() || needs_tracker;
    let entry = if entry_read {
        config::bound_entry(entries, &locus)?
    } else {
        None
    };
    let configured = entry.as_ref().map(|e| e.store.clone());
    // The tracker the CURRENT DIRECTORY's entry names. Only the early
    // refusals below and the issue-URL filter read it; the tracker a command
    // opens is `binding`, taken from the store the command ends up in.
    let here_binding = entry.as_ref().and_then(|e| e.tracker.clone());
    // Before any store's directory is created, searched or opened: `fl
    // github` with `--db` still reads the entry (it needs the tracker), so
    // this fires there too.
    if matches!(cli.command, Command::Github(_)) && here_binding.is_none() {
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
    if here_binding.is_some() && explicit.is_some() && needs_tracker {
        bail!(
            "this project's tracker is bound to GitHub in the config, so it uses the store its \
             config entry names. Drop --db (and unset $FL_DB) for this command"
        );
    }
    let explicit_given = explicit.is_some();
    let (bound, confined) = db_path(explicit, configured)?;
    let mut iris = cli.command.iris();
    // A GitHub issue URL is the tracker's to resolve: no local store holds
    // one, and searching them would refuse it as NotOwned (spec §2.2).
    if here_binding.is_some() {
        iris.retain(|i| !fl_github::meta::is_issue_url(i));
    }
    let path = choose_store(&bound, entries, &iris, confined)?;
    // ⚠ The tracker comes from the store the command
    // ends up in, never from the current directory alone — an IRI can send
    // `choose_store` to another project's store, and pairing that store with
    // this directory's tracker would write one project's records into the
    // other's tracker. Before the store is opened: a refused command writes
    // nothing to it.
    let binding = if needs_tracker {
        tracker_for(&path, entry.as_ref(), entries, explicit_given)?
    } else {
        None
    };
    // ⚠ A handle resolves only in the store it was
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
    // The GitHub ledger, when the binding names it (GitHub ledger spec
    // §1.5): over the tracker's client — one credential, one origin guard
    // (§1.1) — and the local store, which keeps its anchor, cut-over and
    // cache.
    let github_ledger = match (&github, &binding) {
        (Some(gh), Some(b)) if b.github_ledger() => Some(fl_github::GithubLedger::new(
            gh.client(),
            gh.repo().clone(),
            &store,
        )),
        _ => None,
    };
    // Mode B (GitHub ledger spec §1.2, §2.6): every entry goes to the local
    // store at once; each decision's flush publishes through `github_ledger`.
    let split = github_ledger.as_ref().map(|gl| fl_core::SplitLedger {
        local: &store,
        github: gl,
    });
    let ledger: &dyn fl_core::Ledger = match &split {
        Some(s) => s,
        None => &store,
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
                ledger,
                handles: &routed,
                github: Some(gh),
                github_ledger: github_ledger.as_ref(),
                tracker_label: format!("github:{}", gh.repo().full_name),
            }
        }
        None => Ctx {
            store: &store,
            tracker: &store,
            ledger,
            handles: &store,
            github: None,
            github_ledger: None,
            tracker_label: store.label().to_string(),
        },
    };
    // `manifest export` writes the ledger root of the repository the
    // project's ledger is in (GitHub ledger spec §6.1 step 4). Only the
    // config entry says which, so it is known only when the entry was read
    // and the command runs on that entry's store.
    let manifest_binding = if !entry_read || path != bound {
        cmd::manifest::Binding::Unread
    } else {
        // ⚠⚠ `manifest` never asks for a tracker (`needs_tracker` is
        // false), so `tracker_for`'s one-tracker-per-store check never runs
        // for it — unlike `record`, `finding`, `attempt`, `check --record`
        // and `fl github`. Once the store holds a ledger root, trusting
        // `here_binding` without that check could write one repository's
        // root into another project's manifest, from a config where two
        // entries share this store but disagree on its tracker. Run the same
        // check, and refuse the same way, before trusting it.
        if store.holds_a_ledger_root()? {
            store_tracker(&path, entries)?;
        }
        match &here_binding {
            Some(t) => cmd::manifest::Binding::Github(t.github.clone()),
            None => cmd::manifest::Binding::Local,
        }
    };
    let result = match cli.command {
        Command::Project(c) => cmd::project::run(&store, c),
        Command::Gate(c) => cmd::gate::run(&store, c),
        Command::Transition(c) => cmd::transition::run(&store, c),
        Command::Record(c) => cmd::record::run(&ctx, c),
        Command::Check(c) => cmd::check::run(&ctx, c),
        Command::Finding(c) => cmd::finding::run(&ctx, c),
        Command::Attempt(c) => cmd::attempt::run(&ctx, c),
        Command::Stats(c) => cmd::stats::run(&store, c),
        Command::Manifest(c) => cmd::manifest::run(&store, c, &manifest_binding),
        Command::Github(c) => cmd::github::run(&ctx, c, entry.as_ref().map(|e| e.root.as_path())),
    };
    // What the GitHub ledger's reads noted without refusing — a quarantined
    // line skipped (spec §3.3, §3.6) — once each, whatever became of the
    // command.
    if let Some(gl) = &github_ledger {
        for note in gl.take_notes() {
            eprintln!("note: {note}");
        }
    }
    result
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

    fn entry(root: &str, store: &str, github: Option<&str>) -> config::Entry {
        config::Entry {
            root: PathBuf::from(root),
            store: PathBuf::from(store),
            tracker: github.map(|g| config::TrackerBinding {
                github: g.into(),
                credential: config::Credential::Env,
                ledger: None,
            }),
        }
    }

    /// Two entries naming one store with different
    /// trackers leave the store's tracker unknown, from either root.
    #[test]
    fn entries_sharing_a_store_but_not_a_tracker_are_refused() {
        let a = entry("/a", "/s/shared.redb", Some("acme/widgets"));
        let b = entry("/b", "/s/shared.redb", None);
        let entries = [a.clone(), b.clone()];
        for here in [&a, &b] {
            let err = tracker_for(Path::new("/s/shared.redb"), Some(here), &entries, false)
                .expect_err("the store's tracker is ambiguous");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("more than one tracker")
                    && msg.contains("/a -> GitHub `acme/widgets`")
                    && msg.contains("/b -> the store's own tracker"),
                "{msg}"
            );
        }
    }

    // The ledger is part of a store's one tracker: two entries naming one
    // store and one repository, one with the GitHub ledger and one
    // without, are refused, naming which is which.
    #[test]
    fn entries_sharing_a_store_but_not_a_ledger_are_refused_naming_each() {
        let mut a = entry("/a", "/s/shared.redb", Some("acme/widgets"));
        a.tracker.as_mut().unwrap().ledger = Some(config::LedgerChoice::Github);
        let b = entry("/b", "/s/shared.redb", Some("acme/widgets"));
        let entries = [a.clone(), b];
        let msg = format!(
            "{:#}",
            tracker_for(Path::new("/s/shared.redb"), Some(&a), &entries, false).unwrap_err()
        );
        assert!(msg.contains("more than one tracker"), "{msg}");
        assert!(
            msg.contains("/a -> GitHub `acme/widgets` with its GitHub ledger"),
            "{msg}"
        );
        assert!(msg.contains("/b -> GitHub `acme/widgets`"), "{msg}");
        assert!(!msg.contains("/b -> GitHub `acme/widgets` with"), "{msg}");
    }

    #[test]
    fn the_tracker_is_the_chosen_stores_and_unbound_stores_mix_as_before() {
        let p = entry("/p", "/s/p.redb", Some("acme/widgets"));
        let q = entry("/q", "/s/q.redb", None);
        let r = entry("/r", "/s/r.redb", None);
        let entries = [p.clone(), q.clone(), r.clone()];
        let got = tracker_for(Path::new("/s/p.redb"), Some(&p), &entries, false).unwrap();
        assert_eq!(got.map(|t| t.github).as_deref(), Some("acme/widgets"));
        // Two local projects: an IRI may send one's command to the other's
        // store, as it always could.
        assert_eq!(
            tracker_for(Path::new("/s/r.redb"), Some(&q), &entries, false).unwrap(),
            None
        );
        assert_eq!(
            tracker_for(Path::new("/s/r.redb"), None, &entries, false).unwrap(),
            None
        );
        // `--db` naming a GitHub-bound store from elsewhere says to drop it.
        let msg = format!(
            "{:#}",
            tracker_for(Path::new("/s/p.redb"), Some(&q), &entries, true).unwrap_err()
        );
        assert!(msg.contains("without --db"), "{msg}");
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
