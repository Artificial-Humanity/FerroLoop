//! The project's MCP catalog (MCP spec): one committed list of the MCP servers
//! a project uses, and the writer of each agent CLI's own MCP file from it.

use std::path::PathBuf;

pub mod catalog;

/// Every refusal names the file, the entry and what to do next (MCP spec §5).
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The file could not be read or written. `op` is `read` or `write`.
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
}

fn on_server(server: &Option<String>) -> String {
    match server {
        Some(name) => format!("server `{name}`, "),
        None => String::new(),
    }
}
