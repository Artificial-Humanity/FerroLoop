//! Codex's `<root>/.codex/config.toml`, edited with `toml_edit` so that its
//! comments and layout survive (MCP spec §4.3). Each server is a
//! `[mcp_servers.<name>]` table. Codex expands no `${NAME}`: a secret
//! variable is forwarded by name (`env_vars`), a secret header is read whole
//! from its variable (`env_http_headers`) or is the Bearer token
//! (`bearer_token_env_var`). Codex speaks streamable HTTP only, and has no
//! `type` field.

use super::{Field, Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, refuse, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::Path;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

const SERVERS: &str = "mcp_servers";

pub struct Codex;

impl Vendor for Codex {
    fn name(&self) -> VendorName {
        VendorName::Codex
    }

    fn title(&self) -> &'static str {
        "Codex"
    }

    fn target(&self) -> &'static str {
        ".codex/config.toml"
    }

    fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        let refuse = |reason: String| refuse(VendorName::Codex, name, reason);
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let (mut literals, mut forwarded) = (Vec::new(), Vec::new());
                for (key, value) in env(server) {
                    match value {
                        EnvValue::Literal(v) => literals.push((key.clone(), v.clone())),
                        EnvValue::Secret { .. } => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            if var != key {
                                return Err(refuse(format!(
                                    "`env.{key}` reads the variable `{var}`, and Codex forwards \
                                     a secret variable under its own name"
                                )));
                            }
                            forwarded.push(var.to_string());
                        }
                    }
                }
                fields.map("env", literals);
                fields.list("env_vars", forwarded);
            }
            Transport::Sse => {
                return Err(refuse(
                    "Codex connects to streamable HTTP servers only, not SSE".into(),
                ));
            }
            Transport::Http => {
                fields.text("url", url(server));
                let (mut literals, mut from_env, mut bearer) = (Vec::new(), Vec::new(), None);
                for (header, value) in headers(server) {
                    match value {
                        HeaderValue::Literal(v) => literals.push((header.clone(), v.clone())),
                        HeaderValue::Secret { env, scheme: None } => {
                            from_env.push((header.clone(), env.clone()));
                        }
                        HeaderValue::Secret {
                            env,
                            scheme: Some(scheme),
                        } if header.eq_ignore_ascii_case("authorization")
                            && scheme.eq_ignore_ascii_case("bearer") =>
                        {
                            bearer = Some(env);
                        }
                        HeaderValue::Secret {
                            scheme: Some(scheme),
                            ..
                        } => {
                            return Err(refuse(format!(
                                "the secret header `{header}` has the scheme `{scheme}`, and \
                                 Codex sends a secret header either whole or as \
                                 `Authorization: Bearer`"
                            )));
                        }
                    }
                }
                if let Some(env) = bearer {
                    fields.text("bearer_token_env_var", env);
                }
                fields.map("http_headers", literals);
                fields.map("env_http_headers", from_env);
            }
        }
        let mut table = Table::new();
        for (key, field) in fields.0 {
            let value = match field {
                Field::Text(s) => Value::from(s),
                Field::List(values) => Value::Array(values.into_iter().collect::<Array>()),
                Field::Map(values) => {
                    Value::InlineTable(values.into_iter().collect::<InlineTable>())
                }
            };
            table.insert(key, Item::Value(value));
        }
        Ok(Rendered {
            text: alone(name, &Item::Table(table)),
        })
    }

    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        let kind = Kind::Toml(open(path, bytes)?);
        Ok(VendorFile { kind })
    }
}

/// One entry as a file of its own: `[mcp_servers.<name>]` and its body,
/// without what stands above its header. This is both what fl writes and
/// what it reads back, so the two can be compared.
fn alone(name: &str, entry: &Item) -> String {
    let mut entry = entry.clone();
    if let Item::Table(t) = &mut entry {
        t.decor_mut().clear();
    }
    let mut servers = Table::new();
    servers.set_implicit(true);
    servers.insert(name, entry);
    let mut doc = DocumentMut::new();
    doc.insert(SERVERS, Item::Table(servers));
    doc.to_string()
}

#[derive(Debug, Clone)]
pub(super) struct TomlFile {
    doc: DocumentMut,
}

fn open(path: &Path, bytes: Option<&[u8]>) -> Result<TomlFile, FileRefusal> {
    let Some(bytes) = bytes else {
        return Ok(TomlFile {
            doc: DocumentMut::new(),
        });
    };
    let refuse = |problem: String| FileRefusal {
        path: path.to_path_buf(),
        problem,
        next: "Fix it by hand, then run `fl mcp sync` again".into(),
    };
    let text =
        std::str::from_utf8(bytes).map_err(|e| refuse(format!("it is not valid TOML ({e})")))?;
    // The parser's own message quotes the line; a line can hold a value, so
    // only its line number and message are shown.
    let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        let at = e.span().map_or(0, |s| s.start);
        let line = text[..at].matches('\n').count() + 1;
        refuse(format!(
            "it is not valid TOML (line {line}: {})",
            e.message().trim_end()
        ))
    })?;
    if doc.get(SERVERS).is_some_and(|item| !item.is_table()) {
        return Err(refuse(format!("`{SERVERS}` is not a table")));
    }
    Ok(TomlFile { doc })
}

impl TomlFile {
    fn servers(&self) -> Option<&Table> {
        self.doc.get(SERVERS).and_then(Item::as_table)
    }

    /// `mcp_servers`, added with no header of its own when the file has none.
    fn servers_mut(&mut self) -> &mut Table {
        if !self.doc.contains_key(SERVERS) {
            let mut servers = Table::new();
            servers.set_implicit(true);
            self.doc.insert(SERVERS, Item::Table(servers));
        }
        self.doc[SERVERS]
            .as_table_mut()
            .expect("checked when the file was read")
    }

    pub(super) fn names(&self) -> Vec<String> {
        let servers = self.servers().into_iter();
        servers
            .flat_map(|s| s.iter().map(|(name, _)| name.to_string()))
            .collect()
    }

    pub(super) fn entry(&self, name: &str) -> Option<Vec<u8>> {
        let entry = self.servers()?.get(name)?;
        Some(alone(name, entry).into_bytes())
    }

    pub(super) fn set(&mut self, name: &str, entry: &Rendered) {
        let mut doc: DocumentMut = entry.text.parse().expect("fl renders valid TOML");
        let mut new = std::mem::take(&mut doc[SERVERS][name])
            .into_table()
            .expect("fl renders a table");
        let first = self.doc.as_table().is_empty();
        let servers = self.servers_mut();
        match servers.get(name) {
            // In place, under the same lines above it.
            Some(Item::Table(old)) => {
                new.set_position(old.position());
                *new.decor_mut() = old.decor().clone();
            }
            // Last among the servers, a blank line above it.
            _ => {
                new.set_position(None);
                new.decor_mut().set_prefix(if first { "" } else { "\n" });
            }
        }
        servers.insert(name, Item::Table(new));
    }

    pub(super) fn remove(&mut self, name: &str) -> bool {
        let servers = self.doc.get_mut(SERVERS).and_then(Item::as_table_mut);
        servers.is_some_and(|s| s.remove(name).is_some())
    }

    pub(super) fn to_bytes(&self) -> Vec<u8> {
        self.doc.to_string().into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::Codex;
    use crate::vendor::Vendor;
    use crate::vendor::fixtures::*;
    use std::path::Path;

    #[test]
    fn a_stdio_server_forwards_a_secret_by_name_and_writes_literals_only() {
        assert_eq!(
            file_with(&Codex, "files", STDIO),
            r#"[mcp_servers.files]
command = "npx"
args = ["-y", "@example/files-mcp@1.4.2"]
env = { LOG_LEVEL = "debug" }
env_vars = ["FILES_TOKEN"]
"#
        );
    }

    #[test]
    fn a_server_whose_env_is_all_secret_gets_no_env_table() {
        assert_eq!(
            file_with(&Codex, "files", ONLY_SECRET),
            "[mcp_servers.files]\ncommand = \"files-mcp\"\nenv_vars = [\"FILES_TOKEN\"]\n"
        );
    }

    #[test]
    fn a_secret_bearer_authorization_is_the_bearer_token_variable() {
        assert_eq!(
            file_with(&Codex, "docs", BEARER),
            r#"[mcp_servers.docs]
url = "https://mcp.example.com/mcp"
bearer_token_env_var = "DOCS_TOKEN"
http_headers = { X-Client = "fl" }
"#
        );
        // A header name and a scheme are matched without regard to case.
        let lower = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                     headers.authorization = { secret = true, env = \"T\", scheme = \"bearer\" }\n";
        assert_eq!(
            file_with(&Codex, "docs", lower),
            "[mcp_servers.docs]\nurl = \"https://mcp.example.com/mcp\"\n\
             bearer_token_env_var = \"T\"\n"
        );
    }

    #[test]
    fn a_secret_header_with_no_scheme_is_read_whole_from_its_variable() {
        assert_eq!(
            file_with(&Codex, "docs", WHOLE),
            r#"[mcp_servers.docs]
url = "https://mcp.example.com/mcp"
env_http_headers = { X-Api-Key = "DOCS_KEY" }
"#
        );
    }

    #[test]
    fn an_sse_remote_is_refused_for_codex() {
        assert_eq!(
            refusal(&Codex, SSE),
            "Codex connects to streamable HTTP servers only, not SSE"
        );
    }

    #[test]
    fn a_secret_header_codex_cannot_prefix_is_refused() {
        let token = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                     headers.Authorization = { secret = true, env = \"T\", scheme = \"Token\" }\n";
        let elsewhere = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                         headers.X-Auth = { secret = true, env = \"T\", scheme = \"Bearer\" }\n";
        for (body, header, scheme) in [
            (token, "Authorization", "Token"),
            (elsewhere, "X-Auth", "Bearer"),
        ] {
            assert_eq!(
                refusal(&Codex, body),
                format!(
                    "the secret header `{header}` has the scheme `{scheme}`, and Codex sends a \
                     secret header either whole or as `Authorization: Bearer`"
                )
            );
        }
    }

    #[test]
    fn a_secret_under_another_name_is_refused_for_codex() {
        assert_eq!(
            refusal(&Codex, RENAMED),
            "`env.GITHUB_TOKEN` reads the variable `GH_PAT`, and Codex forwards a secret \
             variable under its own name"
        );
    }

    const COMMENTED: &str = r#"# Codex settings for this repository.
model = "o3"   # the default model

# A server someone added by hand.
[mcp_servers.theirs]
command = "their-tool"
args = [ "--fast" ]   # keep it quick

[mcp_servers.theirs.env]
MODE = "x"

# Profiles sit between the servers.
[profiles.fast]
model = "o4-mini"

# The docs server, as fl wrote it.
[mcp_servers.docs]
url = "https://old.example.com/mcp"
"#;

    #[test]
    fn codex_comments_and_layout_survive() {
        let mut file = Codex
            .open(Path::new("f"), Some(COMMENTED.as_bytes()))
            .unwrap();
        let theirs = file.entry("theirs").unwrap();
        file.set("docs", &Codex.render("docs", &server(BEARER)).unwrap());
        file.set(
            "files",
            &Codex.render("files", &server(ONLY_SECRET)).unwrap(),
        );
        let written = String::from_utf8(file.to_bytes()).unwrap();
        assert_eq!(
            written,
            r#"# Codex settings for this repository.
model = "o3"   # the default model

# A server someone added by hand.
[mcp_servers.theirs]
command = "their-tool"
args = [ "--fast" ]   # keep it quick

[mcp_servers.theirs.env]
MODE = "x"

# Profiles sit between the servers.
[profiles.fast]
model = "o4-mini"

# The docs server, as fl wrote it.
[mcp_servers.docs]
url = "https://mcp.example.com/mcp"
bearer_token_env_var = "DOCS_TOKEN"
http_headers = { X-Client = "fl" }

[mcp_servers.files]
command = "files-mcp"
env_vars = ["FILES_TOKEN"]
"#
        );
        let mut file = Codex
            .open(Path::new("f"), Some(written.as_bytes()))
            .unwrap();
        assert_eq!(file.entry("theirs").unwrap(), theirs);
        assert_eq!(file.names(), ["theirs", "docs", "files"]);
        assert!(file.remove("docs"));
        assert!(file.remove("files"));
        assert_eq!(
            String::from_utf8(file.to_bytes()).unwrap(),
            // The comment above an entry is the entry's, and goes with it.
            COMMENTED.replace(
                "\n# The docs server, as fl wrote it.\n[mcp_servers.docs]\n\
                 url = \"https://old.example.com/mcp\"\n",
                ""
            )
        );
    }

    #[test]
    fn a_first_entry_in_a_file_with_no_servers_goes_last() {
        let text = "model = \"o3\"\n\n[profiles.fast]\nmodel = \"o4-mini\"\n";
        let mut file = Codex.open(Path::new("f"), Some(text.as_bytes())).unwrap();
        file.set(
            "files",
            &Codex.render("files", &server(ONLY_SECRET)).unwrap(),
        );
        assert_eq!(
            String::from_utf8(file.to_bytes()).unwrap(),
            format!(
                "{text}\n[mcp_servers.files]\ncommand = \"files-mcp\"\n\
                 env_vars = [\"FILES_TOKEN\"]\n"
            )
        );
    }

    #[test]
    fn a_codex_file_fl_cannot_read_as_a_table_of_servers_is_refused() {
        // A line of the file can hold a value, so the refusal quotes none.
        let cases: [(&[u8], &str); 4] = [
            (
                b"model = \"o3\"\n\ntoken = \"sk-live-4f9a\" x\n",
                "it is not valid TOML (line 3: ",
            ),
            (b"model = \"\xff\"\n", "it is not valid TOML (invalid utf-8"),
            (b"mcp_servers = 3\n", "`mcp_servers` is not a table"),
            (
                b"mcp_servers = { x = { command = \"x\" } }\n",
                "`mcp_servers` is not a table",
            ),
        ];
        for (text, problem) in cases {
            let err = Codex.open(Path::new("p/.codex/config.toml"), Some(text));
            let err = err.unwrap_err().to_string();
            assert!(!err.contains("sk-live-4f9a"), "{err}");
            assert!(
                err.starts_with(&format!("p/.codex/config.toml: {problem}")),
                "{err}"
            );
            assert!(
                err.ends_with("Fix it by hand, then run `fl mcp sync` again"),
                "{err}"
            );
        }
    }
}
