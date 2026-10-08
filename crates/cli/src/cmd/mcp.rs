//! `fl mcp` (MCP spec §1.2): the project's catalog of MCP servers, the
//! registry it freezes them from, and each agent CLI's own MCP file written
//! from it. It opens no store: `run()` in `main.rs` hands over before any
//! store path is resolved. `sync` and `check` build no registry client, so
//! they have no network path (MCP spec §5).

use crate::config::{self, McpEntry};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};
use fl_exec::git::Git;
use fl_exec::population::ExecError;
use fl_mcp::McpError;
use fl_mcp::catalog::{
    Catalog, Editor, EnvValue, HeaderValue, Server, Transport, VendorName, is_variable_name,
};
use fl_mcp::freeze::{self, FreezeOptions, Route};
use fl_mcp::registry::{Registry, printable};
use fl_mcp::sync::{self, Action, Plan, Switches};
use fl_mcp::vendor;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum Cmd {
    /// Set the registry the catalog's servers come from.
    Registry { url: String },
    /// List the registry's servers whose name holds TEXT.
    Search { text: String },
    /// Add a server: from the registry (--from), or by hand (--url, or -- <command>).
    Add(Add),
    /// Remove a server from the catalog.
    Remove { name: String },
    /// Turn a server on for the team: its default in the catalog.
    Enable { name: String },
    /// Turn a server off for the team: its default in the catalog.
    Disable { name: String },
    /// Move a registry server to its newer version, showing the difference.
    /// Writes the catalog only: commit it, then `fl mcp sync`.
    Upgrade {
        name: String,
        /// This version, newer or not.
        #[arg(long, value_name = "VERSION")]
        to: Option<String>,
        /// NAME=VALUE: a literal the new version needs, committed with the
        /// catalog.
        #[arg(long = "env", value_name = "NAME=VALUE")]
        env: Vec<String>,
        /// Include an optional variable or argument that needs a secret.
        #[arg(long = "with", value_name = "NAME")]
        with: Vec<String>,
    },
    /// Write each agent CLI's MCP file from the catalog. Reads no registry.
    Sync {
        /// Overwrite this entry although it was changed by hand, or is not
        /// fl's. Repeatable.
        #[arg(long, value_name = "NAME")]
        replace: Vec<String>,
    },
    /// Whether `sync` would change anything: exit 0 (no), 1 (yes), 2 (a
    /// refusal or an error). Writes nothing and reads no registry.
    Check,
}

#[derive(Args)]
pub struct Add {
    /// The server's name in the catalog and in every vendor file.
    name: String,
    /// The registry's name for the server.
    #[arg(long, value_name = "REGISTRY_NAME", conflicts_with_all = ["url", "command"])]
    from: Option<String>,
    /// The registry version to freeze. Default: its latest.
    #[arg(long, value_name = "VERSION", requires = "from")]
    version: Option<String>,
    /// The package to launch, when the registry offers more than one route.
    #[arg(long, value_enum, requires = "from", conflicts_with = "remote")]
    package: Option<Package>,
    /// Connect to the registry's remote, when it offers more than one route.
    #[arg(long, requires = "from")]
    remote: bool,
    /// NAME=VALUE: a literal, committed with the catalog. By hand, NAME
    /// alone is a secret the agent CLI reads from its environment.
    #[arg(long = "env", value_name = "NAME[=VALUE]", conflicts_with = "url")]
    env: Vec<String>,
    /// Include an optional variable or argument that needs a secret.
    #[arg(long = "with", value_name = "NAME", requires = "from")]
    with: Vec<String>,
    /// The URL of a streamable HTTP server, added by hand.
    #[arg(long, conflicts_with = "command")]
    url: Option<String>,
    /// NAME[=ENV[:SCHEME]]: a secret header, read from the variable ENV
    /// (default <SERVER>_<NAME>) and sent after SCHEME (such as Bearer).
    #[arg(long = "header", value_name = "NAME[=ENV[:SCHEME]]", requires = "url")]
    header: Vec<String>,
    /// The command that starts the server, and its arguments, added by hand.
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Package {
    Npm,
    Pypi,
    Oci,
}

pub fn run(cmd: Cmd, switches: &[McpEntry], cwd: &Path) -> Result<i32> {
    let root = root(cwd)?;
    match cmd {
        Cmd::Registry { url } => {
            let mut editor = Editor::open(&root)?;
            editor.set_registry(&url)?;
            editor.save()?;
            println!(
                "the catalog reads servers from {url}: {}",
                editor.path().display()
            );
        }
        Cmd::Search { text } => search(&root, &text)?,
        Cmd::Add(add) => {
            let form = match (&add.from, &add.url, add.command.is_empty()) {
                (Some(_), _, _) => Form::From,
                (None, Some(_), _) => Form::Url,
                (None, None, false) => Form::Command,
                (None, None, true) => bail!(
                    "say where `{}` comes from: `--from <registry-name>`, `--url <url>`, or \
                     `-- <command> [args…]`",
                    add.name
                ),
            };
            refuse_strays(&add, form)?;
            match form {
                Form::From => add_from(&root, add)?,
                Form::Url | Form::Command => add_by_hand(&root, add)?,
            }
        }
        Cmd::Remove { name } => {
            let mut editor = Editor::open(&root)?;
            editor.remove(&name)?;
            editor.save()?;
            println!(
                "removed `{name}` from {}. `fl mcp sync` takes it out of each agent CLI's file",
                editor.path().display()
            );
        }
        Cmd::Enable { name } => set_enabled(&root, &name, true)?,
        Cmd::Disable { name } => set_enabled(&root, &name, false)?,
        Cmd::Upgrade {
            name,
            to,
            env,
            with,
        } => {
            let env = literals(&env, "`fl mcp upgrade`")?;
            upgrade(&root, &name, to.as_deref(), env, with)?
        }
        Cmd::Sync { replace } => {
            let plan = plan(&root, switches, &replace)?;
            println!("{plan}");
            sync::apply(&plan).map_err(|e| match e {
                McpError::Refused { .. } => {
                    anyhow!("nothing was written; each refusal above names its remedy")
                }
                e => e.into(),
            })?;
        }
        Cmd::Check => {
            let plan = plan(&root, switches, &[])?;
            println!("{plan}");
            return Ok(plan.check().exit_code().into());
        }
    }
    Ok(0)
}

/// The three forms of `add` (MCP spec §1.2): from the registry, a server at
/// a URL, or a command.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    From,
    Url,
    Command,
}

impl Form {
    fn shape(self) -> &'static str {
        match self {
            Form::From => "--from <registry-name>",
            Form::Url => "--url <url>",
            Form::Command => "-- <command>",
        }
    }
}

/// A flag of one form of `add` given to another is refused by name, never
/// ignored. clap refuses `--from`, `--url` and a command together, and
/// `--env` with `--url`; its `requires` does not hold once a form's own flag
/// is given, so the flags that need one form are checked here.
fn refuse_strays(add: &Add, form: Form) -> Result<()> {
    let needs = [
        ("--version", add.version.is_some(), Form::From),
        ("--package", add.package.is_some(), Form::From),
        ("--remote", add.remote, Form::From),
        ("--with", !add.with.is_empty(), Form::From),
        ("--header", !add.header.is_empty(), Form::Url),
    ];
    for (flag, given, own) in needs {
        if given && form != own {
            bail!(
                "`{flag}` has no place in `fl mcp add {} {}`: it goes with `{}` only. Drop it",
                add.name,
                form.shape(),
                own.shape()
            );
        }
    }
    Ok(())
}

/// The project root (MCP spec §2.1): the nearest ancestor of `cwd` that
/// holds `.fl/mcp.toml`, else the nearest that holds `.git`, where
/// `registry` and `add` create the catalog. No git runs to find it.
fn root(cwd: &Path) -> Result<PathBuf> {
    let found = (cwd.ancestors().find(|d| Catalog::path(d).is_file()))
        .or_else(|| cwd.ancestors().find(|d| d.join(".git").exists()));
    match found {
        Some(root) => Ok(root.to_path_buf()),
        None => bail!(
            "{} is in no git repository and below no MCP catalog (.fl/mcp.toml), so `fl mcp` \
             has no project to work on. Run it inside the project's repository",
            cwd.display()
        ),
    }
}

/// The catalog `sync` and `check` read; a project without one is refused.
fn catalog(root: &Path) -> Result<Catalog> {
    match Catalog::load(root)? {
        Some(catalog) => Ok(catalog),
        None => bail!(
            "{} has no MCP catalog ({}). `fl mcp add` creates one",
            root.display(),
            Catalog::path(root).display()
        ),
    }
}

/// A client for the registry the catalog names. Only `search`, `add --from`
/// and `upgrade` build one.
fn registry(catalog: &Catalog, path: &Path) -> Result<Registry> {
    let Some(url) = &catalog.registry else {
        bail!(
            "{} names no registry. `fl mcp registry <url>` sets one, such as \
             https://registry.modelcontextprotocol.io",
            path.display()
        );
    };
    Ok(Registry::new(url)?)
}

fn search(root: &Path, text: &str) -> Result<()> {
    let catalog = Catalog::load(root)?.unwrap_or_default();
    let found = registry(&catalog, &Catalog::path(root))?.search(text)?;
    for s in &found.servers {
        // A status other than active is shown as the registry spells it.
        let status = match s.status.as_str() {
            "active" => String::new(),
            other => format!(" ({})", printable(other)),
        };
        // Registry text reaches the terminal without its control characters.
        let (name, version) = (printable(&s.name), printable(&s.version));
        println!("{name} {version}{status}  {}", printable(&s.description));
    }
    if found.servers.is_empty() {
        println!("no server's name holds `{text}`");
    }
    if found.stopped_early {
        eprintln!(
            "note: fl stopped after {} pages of results; a longer TEXT narrows the search",
            fl_mcp::registry::SEARCH_PAGES
        );
    }
    Ok(())
}

fn add_from(root: &Path, add: Add) -> Result<()> {
    let from = add.from.as_deref().expect("add_from is called with --from");
    let mut editor = Editor::open(root)?;
    // Refused before the registry is read.
    if editor.catalog().servers.contains_key(&add.name) {
        return Err(McpError::AlreadyPresent {
            path: editor.path().to_path_buf(),
            name: add.name,
        }
        .into());
    }
    let env = literals(&add.env, "`--from`")?;
    let route = match (add.package, add.remote) {
        (Some(Package::Npm), _) => Some(Route::Npm),
        (Some(Package::Pypi), _) => Some(Route::Pypi),
        (Some(Package::Oci), _) => Some(Route::Oci),
        (None, true) => Some(Route::Remote),
        (None, false) => None,
    };
    let opts = FreezeOptions {
        name: add.name.clone(),
        route,
        env,
        with: add.with.iter().cloned().collect(),
        ..FreezeOptions::default()
    };
    let registry = registry(editor.catalog(), editor.path())?;
    let found = registry.version(from, add.version.as_deref().unwrap_or("latest"))?;
    let frozen = freeze::freeze(&found, &opts)?;
    editor.add(&add.name, &frozen.server)?;
    editor.save()?;
    added(
        &add.name,
        editor.path(),
        &frozen.server,
        &frozen.warnings,
        &frozen.secrets,
        &frozen.notes,
    );
    Ok(())
}

/// `--env NAME=VALUE` for a registry server: literals only, since the
/// registry says which variables are secrets and fl records those by
/// reference itself. `with` names the command, for the refusal.
fn literals(given: &[String], with: &str) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for given in given {
        let Some((name, value)) = given.split_once('=') else {
            // A token pasted where a name goes is never repeated (MCP spec §6).
            if !is_variable_name(given) {
                bail!(
                    "an `--env` with no `=` needs `=<value>` with {with}, and this one is not a \
                     variable's name, so fl does not repeat it. The registry says which \
                     variables are secrets, and fl records those by reference itself"
                );
            }
            bail!(
                "`--env {given}` needs `=<value>` with {with}: the registry says which \
                 variables are secrets, and fl records those by reference itself"
            );
        };
        if env.insert(name.to_string(), value.to_string()).is_some() {
            bail!("`--env {name}=…` is given twice");
        }
    }
    Ok(env)
}

fn add_by_hand(root: &Path, add: Add) -> Result<()> {
    let mut env = BTreeMap::new();
    for given in &add.env {
        let (name, value) = match given.split_once('=') {
            // The part before `=` is a variable's name: a token pasted there
            // is refused, and never repeated (MCP spec §6).
            Some((name, _)) if !is_variable_name(name) => bail!(
                "`--env <NAME>=<value>` needs an environment variable's name before the `=` \
                 (capital letters, digits and `_`); this one is not such a name, so fl does \
                 not repeat it"
            ),
            Some((name, value)) => (name, EnvValue::Literal(value.to_string())),
            // A secret reference: the name is the variable, so a token pasted
            // here is refused, and never repeated (MCP spec §6).
            None if !is_variable_name(given) => bail!(
                "`--env` with no `=` names a secret's environment variable (capital letters, \
                 digits and `_`), never a value; this one is not such a name. fl records a \
                 secret by reference: put the value in that variable, and name the variable"
            ),
            None => (given.as_str(), EnvValue::Secret { env: None }),
        };
        if env.insert(name.to_string(), value).is_some() {
            bail!("`--env {name}` is given twice");
        }
    }
    let mut headers = BTreeMap::new();
    for given in &add.header {
        let (name, value) = header(&add.name, given)?;
        if headers.insert(name.clone(), value).is_some() {
            bail!("`--header {name}` is given twice");
        }
    }
    let server = match &add.url {
        Some(url) => Server {
            url: Some(url.clone()),
            headers: (!headers.is_empty()).then_some(headers),
            ..by_hand(Transport::Http)
        },
        None => {
            let (command, args) = add.command.split_first().expect("a command was given");
            Server {
                command: Some(command.clone()),
                args: (!args.is_empty()).then(|| args.to_vec()),
                env: (!env.is_empty()).then_some(env),
                ..by_hand(Transport::Stdio)
            }
        }
    };
    let mut editor = Editor::open(root)?;
    editor.add(&add.name, &server)?;
    editor.save()?;
    let warnings = freeze::literal_warnings(&server);
    let secrets: Vec<String> = (server.secret_vars().into_iter())
        .map(str::to_string)
        .collect();
    added(&add.name, editor.path(), &server, &warnings, &secrets, &[]);
    Ok(())
}

/// A server added by hand: on for the team, for every vendor, nothing set.
fn by_hand(transport: Transport) -> Server {
    Server {
        from: None,
        version: None,
        enabled: true,
        vendors: None,
        transport,
        command: None,
        args: None,
        env: None,
        url: None,
        headers: None,
    }
}

/// `--header NAME[=ENV[:SCHEME]]`: a secret header, recorded by reference
/// (MCP spec §2.1). With no ENV the variable is `<SERVER>_<NAME>`, uppercased
/// with `-` as `_`, and holds the whole value. What follows `=` must look
/// like a variable's name, so a value typed there is refused — and the
/// refusal never repeats it (MCP spec §6).
fn header(server: &str, given: &str) -> Result<(String, HeaderValue)> {
    let (name, rest) = match given.split_once('=') {
        Some((name, rest)) => (name, Some(rest)),
        None => (given, None),
    };
    if !freeze::is_scheme(name) {
        bail!(
            "a `--header` starts with a header name (letters, digits and `-`), then \
             optionally `=<ENV>` and `:<SCHEME>`; this one does not"
        );
    }
    let (env, scheme) = match rest {
        None => (freeze::derived(server, name), None),
        Some(rest) => {
            let (env, scheme) = match rest.split_once(':') {
                Some((env, scheme)) => (env, Some(scheme)),
                None => (rest, None),
            };
            if !is_variable_name(env) || !scheme.is_none_or(freeze::is_scheme) {
                bail!(
                    "`--header {name}=…`: after `=` comes the name of an environment variable \
                     (capital letters, digits and `_`), then optionally `:` and a scheme such \
                     as `Bearer`, never a value. fl records a header by reference, and the \
                     agent CLI reads the variable when it starts the server: put the value in \
                     that variable"
                );
            }
            (env.to_string(), scheme.map(str::to_string))
        }
    };
    Ok((name.to_string(), HeaderValue::Secret { env, scheme }))
}

/// What `add` says: the launch it recorded, the warnings and notes the
/// freeze made, and the variables a person sets.
fn added(
    name: &str,
    path: &Path,
    server: &Server,
    warnings: &[String],
    secrets: &[String],
    notes: &[String],
) {
    println!("added `{name}` to {}", path.display());
    println!("  {}", launch(server));
    for w in warnings {
        eprintln!("warning: {w}");
    }
    if !secrets.is_empty() {
        println!(
            "set {} in the environment the agent CLI starts in; fl records only the name",
            secrets.join(", ")
        );
    }
    for n in notes {
        eprintln!("note: {n}");
    }
    println!("`fl mcp sync` writes it into each agent CLI's file");
}

/// The launch a catalog entry records, as `add` shows it: the command and
/// its arguments, or the URL and the names of its headers — never a value
/// (MCP spec §6). Registry text loses its control characters.
fn launch(server: &Server) -> String {
    let shown = |s: &String| {
        let s = printable(s);
        if s.is_empty() || s.contains(char::is_whitespace) {
            format!("{s:?}")
        } else {
            s
        }
    };
    let transport = server.transport.as_str();
    match server.transport {
        Transport::Stdio => {
            let words = server.command.iter().chain(server.args.iter().flatten());
            let words: Vec<String> = words.map(shown).collect();
            format!("{transport}: {}", words.join(" "))
        }
        Transport::Http | Transport::Sse => {
            let url = server.url.as_ref().map(shown).unwrap_or_default();
            let names: Vec<String> = server
                .headers
                .iter()
                .flatten()
                .map(|(k, _)| shown(k))
                .collect();
            match names.is_empty() {
                true => format!("{transport}: {url}"),
                false => format!("{transport}: {url}, headers {}", names.join(", ")),
            }
        }
    }
}

fn set_enabled(root: &Path, name: &str, enabled: bool) -> Result<()> {
    let mut editor = Editor::open(root)?;
    editor.set_enabled(name, enabled)?;
    editor.save()?;
    println!(
        "`{name}` is now {} for the team in {}. `fl mcp sync` writes the change",
        if enabled { "on" } else { "off" },
        editor.path().display()
    );
    Ok(())
}

/// MCP spec §3.3: the registry's version, frozen with the pinned entry's
/// choices, shown against the pinned one and written in its place. Never
/// syncs: the change reaches the vendor files after it is committed.
fn upgrade(
    root: &Path,
    name: &str,
    to: Option<&str>,
    env: BTreeMap<String, String>,
    with: Vec<String>,
) -> Result<()> {
    let mut editor = Editor::open(root)?;
    let Some(pinned) = editor.catalog().servers.get(name).cloned() else {
        return Err(McpError::NoSuchServer {
            path: editor.path().to_path_buf(),
            name: name.to_string(),
        }
        .into());
    };
    let (Some(from), Some(version)) = (&pinned.from, &pinned.version) else {
        bail!(
            "server `{name}` was added by hand, so the registry has no newer version of it. \
             Edit {} by hand",
            editor.path().display()
        );
    };
    let registry = registry(editor.catalog(), editor.path())?;
    let found = registry.version(from, to.unwrap_or("latest"))?;
    freeze::check_upgrade(name, version, &found.server.version, to.is_some())?;
    // `--env` and `--with` add to what the pinned entry chose, where the new
    // version needs it.
    let opts = FreezeOptions {
        env,
        with: with.into_iter().collect(),
        ..FreezeOptions::upgrading(name, &pinned)
    };
    let mut frozen = freeze::freeze(&found, &opts)?;
    frozen.server.enabled = pinned.enabled;
    frozen.server.vendors = pinned.vendors.clone();
    let changes = freeze::diff(&pinned, &frozen.server);
    if changes.is_empty() {
        println!("`{name}` is already as {from} {version} freezes it; nothing to change");
        return Ok(());
    }
    println!("upgrade `{name}`:");
    for change in &changes {
        println!("  {change}");
    }
    for w in &frozen.warnings {
        eprintln!("warning: {w}");
    }
    for n in &frozen.notes {
        eprintln!("note: {n}");
    }
    editor.replace(name, &frozen.server)?;
    editor.save()?;
    println!(
        "rewrote `{name}` in {}. Review and commit it; `fl mcp sync` then writes it into each \
         agent CLI's file",
        editor.path().display()
    );
    Ok(())
}

/// The plan `sync` applies and `check` reports, refused when git would
/// commit a file it writes, with a warning when Codex will not read its file.
fn plan(root: &Path, switches: &[McpEntry], replace: &[String]) -> Result<Plan> {
    let catalog = catalog(root)?;
    let switches = match config::mcp_entry(switches, root)? {
        Some(e) => Switches {
            enable: e.enable,
            disable: e.disable,
        },
        None => Switches::default(),
    };
    let records = config::fl_state_dir()
        .context(
            "neither an absolute $XDG_STATE_HOME nor $HOME is set, so fl has nowhere to keep \
             the record of the entries it writes",
        )?
        .join("mcp");
    let plan = sync::plan(root, &catalog, &switches, &records, replace)?;
    for warning in plan.warnings() {
        eprintln!("warning: {warning}");
    }
    ignored(root, &plan)?;
    if let Some(warning) = codex_trust(root, &plan) {
        eprintln!("warning: {warning}");
    }
    Ok(plan)
}

/// Whether the target holds, or will hold, an entry of fl's.
fn fl_writes(target: &sync::Target) -> bool {
    (target.entries.iter()).any(|e| !matches!(e.action, Action::Untouched | Action::Forget))
}

/// MCP spec §4.4: every vendor file fl writes is generated on each machine,
/// so git must ignore it. A file that is tracked, or that git would not
/// ignore, is refused, naming the lines to add; fl does not edit
/// `.gitignore`. Asked of git, which fails rather than answer `no`.
fn ignored(root: &Path, plan: &Plan) -> Result<()> {
    let (mut problems, mut lines) = (Vec::new(), Vec::new());
    for target in plan.targets().iter().filter(|t| fl_writes(t)) {
        let rel = vendor::vendor(target.vendor).target();
        let git = |e: ExecError| {
            anyhow!("{e}. fl asks git whether {rel} is ignored before writing it (MCP spec §4.4)")
        };
        if Git::is_tracked(root, rel).map_err(git)? {
            problems.push(format!(
                "  {rel} is tracked by git: `git rm --cached {rel}` stops tracking it"
            ));
        } else if !Git::is_ignored(root, rel).map_err(git)? {
            problems.push(format!("  {rel} is not ignored by git"));
        } else {
            continue;
        }
        lines.push(format!("/{rel}"));
    }
    if problems.is_empty() {
        return Ok(());
    }
    bail!(
        "fl writes these files on each machine from the catalog, so git must ignore them:\n{}\n\
         Make sure {} holds these lines:\n{}\nfl does not edit .gitignore. Nothing was written",
        problems.join("\n"),
        root.join(".gitignore").display(),
        lines.join("\n")
    )
}

/// MCP spec §4.1: Codex reads a project's `.codex/config.toml` only when its
/// user trusts the project, by an exact key — the project root (the
/// directory holding `.git`), else the main checkout's root; an ancestor's
/// trust does not count. `$CODEX_HOME/config.toml` (default
/// `~/.codex/config.toml`) is read, never written. A warning when fl has an
/// entry there that Codex will not load; a file fl cannot read or parse is
/// one too, never a refusal, and shows none of its text.
fn codex_trust(root: &Path, plan: &Plan) -> Option<String> {
    let codex = plan
        .targets()
        .iter()
        .find(|t| t.vendor == VendorName::Codex)?;
    let writes = (codex.entries.iter()).any(|e| {
        !matches!(
            e.action,
            Action::Untouched | Action::Remove | Action::Forget
        )
    });
    if !writes {
        return None;
    }
    let keys = trust_keys(root);
    let key = keys[0].to_string_lossy().to_string();
    let config = match std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        Some(home) => Some(PathBuf::from(home).join("config.toml")),
        None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex/config.toml")),
    };
    let why = match &config {
        None => "neither $CODEX_HOME nor $HOME is set, so fl cannot read Codex's config".into(),
        Some(file) => match std::fs::read_to_string(file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                format!("{} does not exist", file.display())
            }
            Err(e) => format!("fl could not read {} ({e})", file.display()),
            Ok(text) => match text.parse::<toml::Table>() {
                Err(e) => {
                    let at = e.span().map_or(0, |s| s.start).min(text.len());
                    let line = text[..at].matches('\n').count() + 1;
                    format!(
                        "{} is not valid TOML (line {line}), so fl cannot tell whether Codex \
                         trusts this project",
                        file.display()
                    )
                }
                Ok(table) if trusts(&table, &keys) => return None,
                Ok(_) => format!("{} does not trust {key}", file.display()),
            },
        },
    };
    let file = config.map_or("Codex's config".into(), |f| f.display().to_string());
    Some(format!(
        "Codex loads {} only in a project its user trusts, and {why}. To trust it, add to \
         {file}:\n[projects.{}]\ntrust_level = \"trusted\"",
        root.join(".codex/config.toml").display(),
        toml::Value::String(key.clone())
    ))
}

/// The keys Codex looks this project up by, in its order: the project root
/// (the nearest directory holding `.git`, canonical), then the main
/// checkout's root when that is a linked worktree. No `.git`: the root.
fn trust_keys(root: &Path) -> Vec<PathBuf> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let Some(project) = root.ancestors().find(|d| d.join(".git").exists()) else {
        return vec![root];
    };
    let mut keys = vec![project.to_path_buf()];
    if let Some(main) = main_checkout(project).filter(|m| m != project) {
        keys.push(main);
    }
    keys
}

/// The main checkout of a linked worktree: its `.git` is a file naming the
/// worktree's git directory, whose `commondir` names the main `.git`.
fn main_checkout(project: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(project.join(".git")).ok()?;
    let gitdir = project.join(text.strip_prefix("gitdir:")?.trim());
    let common = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = gitdir.join(common.trim()).canonicalize().ok()?;
    common.parent().map(Path::to_path_buf)
}

/// Codex's rule: the first key with a `[projects."<key>"]` entry decides,
/// and only `trust_level = "trusted"` trusts.
fn trusts(config: &toml::Table, keys: &[PathBuf]) -> bool {
    let Some(projects) = config.get("projects").and_then(toml::Value::as_table) else {
        return false;
    };
    let entry = (keys.iter())
        .filter_map(|k| k.to_str())
        .find_map(|k| projects.get(k));
    entry
        .and_then(|e| e.get("trust_level"))
        .and_then(toml::Value::as_str)
        == Some("trusted")
}
