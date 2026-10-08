//! The vendor files (MCP spec §4.1, §4.2): each agent CLI's own project MCP
//! file, written from the catalog. Each vendor turns a catalog entry into its
//! own shape, or refuses it naming the reason, and reads and rewrites its file
//! keeping every entry fl does not touch. Nothing here reads the environment:
//! a secret is written as a reference, never as a value (MCP spec §2.1).

mod antigravity;
mod claude;
mod codex;
mod json;

pub use antigravity::Antigravity;
pub use claude::Claude;
pub use codex::Codex;

use crate::catalog::{EnvValue, HeaderValue, Server, VendorName};
use std::fmt;
use std::path::{Path, PathBuf};

/// One agent CLI's project MCP file.
pub trait Vendor: Sync {
    fn name(&self) -> VendorName;
    /// The vendor's name for a person.
    fn title(&self) -> &'static str;
    /// The file, relative to the project root.
    fn target(&self) -> &'static str;
    /// The entry exactly as fl writes it for the server `name`, or why this
    /// vendor cannot run the server; the other vendors still get it (MCP spec
    /// §4.2).
    fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal>;
    /// The file as it stands: `None` when it does not exist yet. `path` only
    /// names the file in a refusal.
    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal>;
}

/// The vendor behind `name`.
pub fn vendor(name: VendorName) -> &'static dyn Vendor {
    match name {
        VendorName::Claude => &Claude,
        VendorName::Codex => &Codex,
        VendorName::Antigravity => &Antigravity,
    }
}

/// An entry as fl writes it. Its bytes are the entry's own bytes in the file
/// once written, which [`VendorFile::entry`] gives back, so hashing both tells
/// whether the entry is still what fl wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    text: String,
}

impl Rendered {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }
}

/// A server one vendor cannot run as the catalog gives it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct VendorRefusal {
    pub vendor: VendorName,
    pub server: String,
    /// The reason, as a clause.
    pub reason: String,
}

impl fmt::Display for VendorRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = vendor(self.vendor);
        write!(
            f,
            "{} cannot run server `{}`: {}. It is left out of {}; the other vendors still get \
             it. To say so in the catalog, give the server a `vendors` list without `{}`",
            v.title(),
            self.server,
            self.reason,
            v.target(),
            self.vendor.as_str()
        )
    }
}

/// A vendor file fl will not rewrite.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {problem}. {next}", path.display())]
pub struct FileRefusal {
    pub path: PathBuf,
    pub problem: String,
    pub next: String,
}

/// A vendor file read into memory, its entries by name. Changing one entry
/// leaves every other entry, and everything outside the servers' section,
/// as it was.
#[derive(Debug, Clone)]
pub struct VendorFile {
    kind: Kind,
}

#[derive(Debug, Clone)]
enum Kind {
    Json(json::JsonFile),
    Toml(codex::TomlFile),
}

impl VendorFile {
    /// Every server's name, in the file's order.
    pub fn names(&self) -> Vec<String> {
        match &self.kind {
            Kind::Json(f) => f.names(),
            Kind::Toml(f) => f.names(),
        }
    }

    /// The server's entry as it stands in the file, in the same form as
    /// [`Rendered::bytes`].
    pub fn entry(&self, name: &str) -> Option<Vec<u8>> {
        match &self.kind {
            Kind::Json(f) => f.entry(name),
            Kind::Toml(f) => f.entry(name),
        }
    }

    /// Writes the entry: in place of the one with this name, or last.
    /// `entry` is this file's vendor's rendering of the server `name`.
    pub fn set(&mut self, name: &str, entry: &Rendered) {
        match &mut self.kind {
            Kind::Json(f) => f.set(name, entry),
            Kind::Toml(f) => f.set(name, entry),
        }
    }

    /// Removes the entry; `false` when there is none.
    pub fn remove(&mut self, name: &str) -> bool {
        match &mut self.kind {
            Kind::Json(f) => f.remove(name),
            Kind::Toml(f) => f.remove(name),
        }
    }

    /// The whole file.
    pub fn to_bytes(&self) -> Vec<u8> {
        match &self.kind {
            Kind::Json(f) => f.to_bytes(),
            Kind::Toml(f) => f.to_bytes(),
        }
    }
}

/// An entry before it takes a vendor's syntax: each field is a string, a
/// list of strings, or a table of strings. Every field a vendor writes is one
/// of these.
enum Field {
    Text(String),
    List(Vec<String>),
    Map(Vec<(String, String)>),
}

/// The fields of one entry, in the order the vendor writes them.
#[derive(Default)]
struct Fields(Vec<(&'static str, Field)>);

impl Fields {
    fn text(&mut self, key: &'static str, value: &str) {
        self.0.push((key, Field::Text(value.to_string())));
    }

    /// Left out when empty.
    fn list(&mut self, key: &'static str, values: Vec<String>) {
        if !values.is_empty() {
            self.0.push((key, Field::List(values)));
        }
    }

    /// Left out when empty.
    fn map(&mut self, key: &'static str, values: Vec<(String, String)>) {
        if !values.is_empty() {
            self.0.push((key, Field::Map(values)));
        }
    }
}

/// A stdio server's command: the catalog requires it.
fn command(server: &Server) -> &str {
    server
        .command
        .as_deref()
        .expect("a stdio server has a command")
}

/// A remote server's url: the catalog requires it.
fn url(server: &Server) -> &str {
    server
        .url
        .as_deref()
        .expect("an http or sse server has a url")
}

fn args(server: &Server) -> Vec<String> {
    server.args.clone().unwrap_or_default()
}

fn env(server: &Server) -> impl Iterator<Item = (&String, &EnvValue)> {
    server.env.iter().flatten()
}

fn headers(server: &Server) -> impl Iterator<Item = (&String, &HeaderValue)> {
    server.headers.iter().flatten()
}

/// The refusal for `server` by `vendor`.
fn refuse(vendor: VendorName, server: &str, reason: String) -> VendorRefusal {
    VendorRefusal {
        vendor,
        server: server.to_string(),
        reason,
    }
}

/// Catalog entries the vendor tests share, each the body of one
/// `[server.<name>]` table.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::Vendor;
    use crate::catalog::{Catalog, Server};
    use std::path::Path;

    /// A stdio server with a literal and a secret environment variable.
    pub(crate) const STDIO: &str = r#"
transport = "stdio"
command = "npx"
args = ["-y", "@example/files-mcp@1.4.2"]
env.LOG_LEVEL = "debug"
env.FILES_TOKEN = { secret = true }
"#;

    /// A stdio server whose only variable is a secret.
    pub(crate) const ONLY_SECRET: &str = r#"
transport = "stdio"
command = "files-mcp"
env.FILES_TOKEN = { secret = true }
"#;

    /// A secret read from a variable whose name is not the key's.
    pub(crate) const RENAMED: &str = r#"
transport = "stdio"
command = "files-mcp"
env.GITHUB_TOKEN = { secret = true, env = "GH_PAT" }
"#;

    /// An http remote with a secret Bearer header and a literal one.
    pub(crate) const BEARER: &str = r#"
transport = "http"
url = "https://mcp.example.com/mcp"
headers.Authorization = { secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }
headers.X-Client = "fl"
"#;

    /// An http remote with a secret header that carries the whole value.
    pub(crate) const WHOLE: &str = r#"
transport = "http"
url = "https://mcp.example.com/mcp"
headers.X-Api-Key = { secret = true, env = "DOCS_KEY" }
"#;

    /// An http remote with literal headers only.
    pub(crate) const PLAIN_REMOTE: &str = r#"
transport = "http"
url = "https://mcp.example.com/mcp"
headers.X-Client = "fl"
"#;

    /// An SSE remote.
    pub(crate) const SSE: &str = r#"
transport = "sse"
url = "https://events.example.com/sse"
"#;

    /// The catalog's own reading of one entry, so a fixture keeps every
    /// catalog rule.
    pub(crate) fn server(body: &str) -> Server {
        let text = format!("[server.s]\n{body}");
        let mut catalog = Catalog::parse(&text, Path::new(".fl/mcp.toml")).unwrap();
        catalog.servers.remove("s").unwrap()
    }

    /// A new vendor file holding one entry, as fl writes it.
    pub(crate) fn file_with(vendor: &dyn Vendor, name: &str, body: &str) -> String {
        let entry = vendor.render(name, &server(body)).unwrap();
        let mut file = vendor.open(Path::new("f"), None).unwrap();
        file.set(name, &entry);
        String::from_utf8(file.to_bytes()).unwrap()
    }

    /// Why `vendor` refuses the entry.
    pub(crate) fn refusal(vendor: &dyn Vendor, body: &str) -> String {
        vendor.render("s", &server(body)).unwrap_err().reason
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::path::Path;

    #[test]
    fn each_vendor_writes_its_own_project_file() {
        let seen: Vec<_> = VendorName::ALL
            .iter()
            .map(|&v| (vendor(v).name(), vendor(v).title(), vendor(v).target()))
            .collect();
        assert_eq!(
            seen,
            [
                (VendorName::Claude, "Claude Code", ".mcp.json"),
                (VendorName::Codex, "Codex", ".codex/config.toml"),
                (
                    VendorName::Antigravity,
                    "Antigravity",
                    ".agents/mcp_config.json"
                ),
            ]
        );
    }

    #[test]
    fn an_entry_fl_writes_reads_back_as_the_bytes_it_rendered() {
        for v in VendorName::ALL {
            let v = vendor(v);
            let foreign = match v.name() {
                VendorName::Codex => "[mcp_servers.theirs]\ncommand = \"their-tool\"\n",
                _ => "{\"mcpServers\": {\"theirs\": {\"command\": \"their-tool\"}}}",
            };
            for start in [None, Some(foreign.as_bytes())] {
                let mut file = v.open(Path::new("f"), start).unwrap();
                let files = v.render("files", &server(STDIO)).unwrap();
                let tools = v.render("tools", &server(PLAIN_REMOTE)).unwrap();
                file.set("files", &v.render("files", &server(PLAIN_REMOTE)).unwrap());
                file.set("tools", &tools);
                file.set("files", &files);
                assert_eq!(file.entry("files").as_deref(), Some(files.bytes()));
                let bytes = file.to_bytes();
                let back = v.open(Path::new("f"), Some(&bytes)).unwrap();
                assert_eq!(
                    back.entry("files").as_deref(),
                    Some(files.bytes()),
                    "{}",
                    v.title()
                );
                assert_eq!(
                    back.entry("tools").as_deref(),
                    Some(tools.bytes()),
                    "{}",
                    v.title()
                );
                assert_eq!(back.entry("nope"), None);
                let mut names = vec!["files", "tools"];
                if start.is_some() {
                    names.insert(0, "theirs");
                }
                assert_eq!(back.names(), names, "{}", v.title());
            }
        }
    }

    #[test]
    fn removing_what_was_added_leaves_the_file_as_it_was() {
        for v in VendorName::ALL {
            let v = vendor(v);
            let text = match v.name() {
                VendorName::Codex => "model = \"o3\"\n\n[mcp_servers.theirs]\ncommand = \"t\"\n",
                _ => "{\n  \"mcpServers\": {\n    \"theirs\": {\"command\": \"t\"}\n  }\n}\n",
            };
            let mut file = v.open(Path::new("f"), Some(text.as_bytes())).unwrap();
            file.set("files", &v.render("files", &server(STDIO)).unwrap());
            assert_ne!(file.to_bytes(), text.as_bytes());
            assert!(file.remove("files"));
            assert!(!file.remove("files"));
            assert_eq!(
                String::from_utf8(file.to_bytes()).unwrap(),
                text,
                "{}",
                v.title()
            );
        }
    }

    #[test]
    fn a_secret_value_in_the_environment_reaches_no_rendered_entry() {
        // Set for every test run by cargo, so a vendor that read the
        // environment would find a value here.
        let value = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
        let stdio = "transport = \"stdio\"\ncommand = \"npx\"\n\
                     env.CARGO_MANIFEST_DIR = { secret = true }\n";
        let remote = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                      headers.Authorization = { secret = true, env = \"CARGO_MANIFEST_DIR\" }\n";
        let mut written = Vec::new();
        for v in VendorName::ALL {
            for body in [stdio, remote] {
                if let Ok(entry) = vendor(v).render("s", &server(body)) {
                    assert!(
                        !entry.text().contains(&value),
                        "{}: {}",
                        v.as_str(),
                        entry.text()
                    );
                    written.push(entry.text().to_string());
                }
            }
        }
        // Antigravity refuses the secret header; the other five are written.
        assert_eq!(written.len(), 5);
        assert!(written[0].contains("\"CARGO_MANIFEST_DIR\": \"${CARGO_MANIFEST_DIR}\""));
        assert!(written[1].contains("\"Authorization\": \"${CARGO_MANIFEST_DIR}\""));
        let code = include_str!("mod.rs");
        let code = &code[..code.find("#[cfg(test)]").unwrap()];
        for source in [
            code,
            include_str!("json.rs"),
            include_str!("claude.rs"),
            include_str!("codex.rs"),
            include_str!("antigravity.rs"),
        ] {
            for read in ["env::var", "var_os"] {
                assert!(
                    !source.contains(read),
                    "a vendor reads the environment: {read}"
                );
            }
        }
    }

    #[test]
    fn a_refusal_names_the_vendor_the_server_the_reason_and_the_file() {
        let refused = vendor(VendorName::Codex)
            .render("events", &server(SSE))
            .unwrap_err();
        assert_eq!(
            refused,
            VendorRefusal {
                vendor: VendorName::Codex,
                server: "events".into(),
                reason: refused.reason.clone(),
            }
        );
        assert_eq!(
            refused.to_string(),
            format!(
                "Codex cannot run server `events`: {}. It is left out of .codex/config.toml; the \
                 other vendors still get it. To say so in the catalog, give the server a \
                 `vendors` list without `codex`",
                refused.reason
            )
        );
    }
}
