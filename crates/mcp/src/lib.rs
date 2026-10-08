//! The project's MCP catalog (MCP spec): one committed list of the MCP servers
//! a project uses, and the writer of each agent CLI's own MCP file from it.

use std::path::PathBuf;

pub mod catalog;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod freeze;
pub mod registry;
pub mod sync;
pub mod vendor;

/// Every refusal names the file, the entry and what to do next (MCP spec §5).
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The file could not be read, written or locked. `op` is `read`,
    /// `write` or `lock`.
    #[error("could not {op} {}: {cause}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        cause: String,
    },
    /// The catalog is not TOML, or not the catalog's shape: an unknown field,
    /// a value of the wrong type, `secret = false`. The cause is the line,
    /// the column and the parser's message, never the file's text.
    #[error(
        "{} is not a valid MCP catalog: {cause}\nFix it by hand; docs/mcp.md describes every field",
        path.display()
    )]
    Parse { path: PathBuf, cause: String },
    /// An entry, or the registry, breaks a catalog rule (MCP spec §2.1).
    /// `server` is `None` for a top-level field.
    #[error("{}: {}field `{field}`: {problem}. {next}", path.display(), on_server(server))]
    Invalid {
        path: PathBuf,
        server: Option<String>,
        field: String,
        problem: String,
        next: String,
    },
    /// An edit names a server the catalog does not have.
    #[error(
        "{}: there is no server `{name}` in the catalog. `fl mcp add` adds one",
        path.display()
    )]
    NoSuchServer { path: PathBuf, name: String },
    /// `add` names a server the catalog already has.
    #[error(
        "{}: server `{name}` is already in the catalog. Remove it first with \
         `fl mcp remove {name}`, or move it to a new version with `fl mcp upgrade {name}`",
        path.display()
    )]
    AlreadyPresent { path: PathBuf, name: String },
    /// A registry address the client will not read (MCP spec §3.1).
    #[error(
        "{url} is not a registry address fl will use: {clause}. Use an https:// address, or \
         http:// to this machine"
    )]
    RegistryAddress { url: String, clause: String },
    /// The registry could not be reached. `sync` and `check` never need it
    /// (MCP spec §5).
    #[error(
        "cannot reach the registry at {registry}: {cause}. Check the address and the network, \
         then retry; `fl mcp sync` and `fl mcp check` need no registry"
    )]
    Unreachable { registry: String, cause: String },
    /// The registry answered, but not with the registry API: a redirect,
    /// an HTML page, a body over 4 MiB, an error status, or a body of the
    /// wrong shape. `request` is `GET <path>`; `problem` says which, and
    /// what to do.
    #[error("the registry at {registry} failed {request}: {problem}")]
    Registry {
        registry: String,
        request: String,
        problem: String,
    },
    /// The registry has no such server, or no such version of it.
    #[error(
        "the registry at {registry} has no {}. `fl mcp search <text>` lists the servers it has",
        missing(name, version)
    )]
    NotFound {
        registry: String,
        name: String,
        version: Option<String>,
    },
    /// A registry entry fl cannot freeze into the catalog honestly (MCP spec
    /// §3.2). `from` and `version` are the registry's; `next` says what to
    /// do instead.
    #[error("`{from}` {version} cannot be frozen into the catalog: {problem}. {next}")]
    Unfreezable {
        from: String,
        version: String,
        problem: String,
        next: String,
    },
    /// `upgrade` to a version that is not newer than the pinned one, without
    /// `--to` (MCP spec §3.3).
    #[error("server `{name}`: {problem}. {next}")]
    NotNewer {
        name: String,
        problem: String,
        next: String,
    },
    /// A vendor file fl will not rewrite.
    #[error(transparent)]
    VendorFile(#[from] vendor::FileRefusal),
    /// An ownership record that is not one.
    #[error(
        "{} is not an ownership record fl can read: {cause}. Delete it; the next \
         `fl mcp sync` adopts every entry that still matches the catalog",
        path.display()
    )]
    Record { path: PathBuf, cause: String },
    /// `sync` refused one entry or more, so it wrote nothing (MCP spec §4.3).
    #[error("nothing was written:\n{}", lines(refusals))]
    Refused { refusals: Vec<sync::Refusal> },
    /// `sync --replace` names an entry no target refused, so it would be
    /// ignored (MCP spec §4.3). `names` holds those that are server names,
    /// which are safe to repeat; `others` counts the rest, never repeated.
    #[error("{}", unmatched(names, *others))]
    UnmatchedReplace { names: Vec<String>, others: usize },
    /// A target changed between the plan and the write (MCP spec §4.3).
    #[error(
        "{} changed while fl was planning its write: another program or a person wrote it. \
         Nothing was written; run `fl mcp sync` again",
        path.display()
    )]
    Changed { path: PathBuf },
}

fn on_server(server: &Option<String>) -> String {
    match server {
        Some(name) => format!("server `{name}`, "),
        None => String::new(),
    }
}

fn missing(name: &str, version: &Option<String>) -> String {
    match version {
        Some(v) => format!("version `{v}` of server `{name}`"),
        None => format!("server `{name}`"),
    }
}

fn unmatched(names: &[String], others: usize) -> String {
    let mut parts: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match others {
        0 => {}
        1 => parts.push("a name that is not a server name, which fl does not repeat".into()),
        n => parts.push(format!(
            "{n} names that are not server names, which fl does not repeat"
        )),
    }
    format!(
        "nothing was written: `--replace` names {}, and no target refused an entry by that \
         name. Run `fl mcp sync` without it to see which entries fl refuses, or check the name",
        parts.join(", ")
    )
}

fn lines(refusals: &[sync::Refusal]) -> String {
    let lines: Vec<String> = refusals.iter().map(ToString::to_string).collect();
    lines.join("\n")
}
