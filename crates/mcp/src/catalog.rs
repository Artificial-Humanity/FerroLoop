//! The catalog, `.fl/mcp.toml` (MCP spec §2.1): committed, and the only source
//! of truth for which MCP servers a project uses. It is read with `toml`, every
//! table refusing a field it does not know, and edited with `toml_edit`, so a
//! person's comments and layout survive every change fl makes to it.

use crate::McpError;
use serde::Deserialize;
use serde::de::{self, MapAccess, Visitor};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;
use std::ops::Range;
use std::path::{Path, PathBuf};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value};

/// Server names Claude Code skips, so a server under either would reach no
/// Claude session.
pub const RESERVED_NAMES: [&str; 3] = ["workspace", "computer-use", "claude-in-chrome"];

/// The first lines of a catalog `fl mcp` creates.
pub const HEADER: &str = "\
# The MCP servers this project uses (docs/mcp.md). Commit this file: `fl mcp sync`
# writes each agent CLI's own MCP file from it, and `fl mcp` keeps your comments.
";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// The registry `fl mcp search`, `add --from` and `upgrade` read.
    #[serde(default)]
    pub registry: Option<String>,
    /// By name: `toml` reads a table's keys in sorted order. The editor keeps
    /// the file's own order.
    #[serde(default, rename = "server")]
    pub servers: BTreeMap<String, Server>,
}

/// One `[server.<name>]` entry. The launch spec is frozen into it when it is
/// added, so writing the vendor files reads nothing else (MCP spec §2.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// The registry name; `None` for a server added by hand.
    #[serde(default)]
    pub from: Option<String>,
    /// The exact registry version, present exactly when `from` is.
    #[serde(default)]
    pub version: Option<String>,
    /// The team default.
    #[serde(default = "on")]
    pub enabled: bool,
    /// `None`: every vendor.
    #[serde(default)]
    pub vendors: Option<Vec<VendorName>>,
    pub transport: Transport,
    /// stdio only, and required there.
    #[serde(default)]
    pub command: Option<String>,
    /// stdio only.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// stdio only.
    #[serde(default)]
    pub env: Option<BTreeMap<String, EnvValue>>,
    /// http and sse only, and required there.
    #[serde(default)]
    pub url: Option<String>,
    /// http and sse only.
    #[serde(default)]
    pub headers: Option<BTreeMap<String, HeaderValue>>,
}

fn on() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VendorName {
    Claude,
    Codex,
    Antigravity,
}

impl VendorName {
    pub const ALL: [VendorName; 3] = [
        VendorName::Claude,
        VendorName::Codex,
        VendorName::Antigravity,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            VendorName::Claude => "claude",
            VendorName::Codex => "codex",
            VendorName::Antigravity => "antigravity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Stdio,
    Http,
    Sse,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Stdio => "stdio",
            Transport::Http => "http",
            Transport::Sse => "sse",
        }
    }
}

/// An `env` value: a literal, committed as written and public if the
/// repository is (MCP spec §6), or a reference to a variable the agent reads
/// when it starts the server. fl never reads a secret's value (MCP spec §2.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "LiteralOr<EnvSecretTable>")]
pub enum EnvValue {
    Literal(String),
    /// `{ secret = true }`, with `env = "…"` when the variable's name is not
    /// the key's.
    Secret {
        env: Option<String>,
    },
}

impl EnvValue {
    /// The variable a secret reads: `env`, else the key itself.
    pub fn secret_var<'a>(&'a self, key: &'a str) -> Option<&'a str> {
        match self {
            EnvValue::Literal(_) => None,
            EnvValue::Secret { env } => Some(env.as_deref().unwrap_or(key)),
        }
    }
}

/// A header value: a literal, or a reference to the variable `env`, sent as
/// `<scheme> <value>` when `scheme` is given and as the value alone otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "LiteralOr<HeaderSecretTable>")]
pub enum HeaderValue {
    Literal(String),
    Secret { env: String, scheme: Option<String> },
}

impl Server {
    /// Whether this server is written for `vendor`.
    pub fn is_for(&self, vendor: VendorName) -> bool {
        self.vendors.as_ref().is_none_or(|v| v.contains(&vendor))
    }

    /// The variables this server reads its secrets from: each secret
    /// environment variable's, and each secret header's. Names only, never a
    /// value (MCP spec §6).
    pub fn secret_vars(&self) -> BTreeSet<&str> {
        let env = (self.env.iter().flatten()).filter_map(|(k, v)| v.secret_var(k));
        let headers = (self.headers.iter().flatten()).filter_map(|(_, v)| match v {
            HeaderValue::Secret { env, .. } => Some(env.as_str()),
            HeaderValue::Literal(_) => None,
        });
        env.chain(headers).collect()
    }

    /// Every literal `env` and header value, as `env.<NAME>` and
    /// `headers.<NAME>`: each is committed with the catalog, so `add` warns
    /// about each (MCP spec §6).
    pub fn literal_values(&self) -> Vec<String> {
        let env = self.env.iter().flatten();
        let env = env.filter(|(_, v)| matches!(v, EnvValue::Literal(_)));
        let headers = self.headers.iter().flatten();
        let headers = headers.filter(|(_, v)| matches!(v, HeaderValue::Literal(_)));
        env.map(|(k, _)| format!("env.{k}"))
            .chain(headers.map(|(k, _)| format!("headers.{k}")))
            .collect()
    }
}

/// A string, or a table `T` read with its own field names: an untagged enum
/// would hide which field a table got wrong.
enum LiteralOr<T> {
    Literal(String),
    Table(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for LiteralOr<T> {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Either<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for Either<T> {
            type Value = LiteralOr<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string, or a table `{ secret = true, … }`")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(LiteralOr::Literal(v.to_string()))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(LiteralOr::Table)
            }
        }
        d.deserialize_any(Either(PhantomData))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvSecretTable {
    secret: bool,
    #[serde(default)]
    env: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderSecretTable {
    secret: bool,
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    scheme: Option<String>,
}

const SECRET_FALSE: &str = "`secret = false` is not allowed: write a literal value as a string, \
                            or a secret as `{ secret = true }`";

const HEADER_NEEDS_ENV: &str = "a secret header needs `env = \"…\"`, the environment variable \
                                that holds its value";

impl TryFrom<LiteralOr<EnvSecretTable>> for EnvValue {
    type Error = String;
    fn try_from(v: LiteralOr<EnvSecretTable>) -> Result<Self, String> {
        match v {
            LiteralOr::Literal(s) => Ok(EnvValue::Literal(s)),
            LiteralOr::Table(t) if !t.secret => Err(SECRET_FALSE.to_string()),
            LiteralOr::Table(t) => Ok(EnvValue::Secret { env: t.env }),
        }
    }
}

impl TryFrom<LiteralOr<HeaderSecretTable>> for HeaderValue {
    type Error = String;
    fn try_from(v: LiteralOr<HeaderSecretTable>) -> Result<Self, String> {
        match v {
            LiteralOr::Literal(s) => Ok(HeaderValue::Literal(s)),
            LiteralOr::Table(t) if !t.secret => Err(SECRET_FALSE.to_string()),
            LiteralOr::Table(t) => match t.env {
                Some(env) => Ok(HeaderValue::Secret {
                    env,
                    scheme: t.scheme,
                }),
                None => Err(HEADER_NEEDS_ENV.to_string()),
            },
        }
    }
}

/// `[a-z0-9-]`, 1 to 32 characters (MCP spec §2.1).
pub fn is_server_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `[A-Za-z_][A-Za-z0-9_]*`: a name every vendor and shell takes as a variable.
pub fn is_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `[A-Z_][A-Z0-9_]*`: a variable's name as fl asks a person to type one
/// (`fl mcp add --env`, `--header`). Stricter than [`is_env_name`], so a
/// token a person pastes by mistake is not taken for a name, and a message
/// repeats a name only when it passes this (MCP spec §6).
pub fn is_variable_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// `[A-Za-z][A-Za-z-]{0,63}`: a header's name as fl asks a person to type one
/// (`fl mcp add --header`). No digits and no `_`, so a token pasted where a
/// name goes (`sk-proj-Ab3dEf9GhIjK2LmN`) is not taken for a name, and a
/// message repeats a name only when it passes this (MCP spec §6). Real names
/// (`Authorization`, `X-Api-Key`, `X-Goog-Api-Key`) pass. A header a registry
/// names keeps the looser RFC token rule in `freeze`: that text is printed
/// through `printable`, and a registry may name `X-Api-Key2`.
pub fn is_header_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    s.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphabetic() || b == b'-')
}

/// Why fl will not read a registry at `url`, as a clause. A registry is read
/// over https, or over http to this machine only, and never with a user name
/// or password in the URL (MCP spec §3.1).
pub fn check_registry_url(url: &str) -> Result<(), String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("it contains a space or a control character".to_string());
    }
    let (rest, https) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, true)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (rest, false)
    } else {
        return Err("it is neither https:// nor http://".to_string());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err("it carries a user name or password before the host".to_string());
    }
    // An IPv6 host keeps its brackets: `[::1]`.
    let host_end = if authority.starts_with('[') {
        authority.find(']').map_or(authority.len(), |i| i + 1)
    } else {
        authority.find(':').unwrap_or(authority.len())
    };
    let (host, port) = authority.split_at(host_end);
    if host.is_empty() {
        return Err("it names no host".to_string());
    }
    let port_ok = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !port_ok {
        return Err("its port is not a number".to_string());
    }
    if !https && !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return Err(
            "it is not https, and its host is not this machine (127.0.0.1, localhost or [::1])"
                .to_string(),
        );
    }
    Ok(())
}

/// A rule an entry breaks, before it is placed in a file.
struct Problem {
    field: String,
    problem: String,
    next: &'static str,
}

impl Problem {
    fn new(field: impl Into<String>, problem: impl Into<String>, next: &'static str) -> Self {
        Problem {
            field: field.into(),
            problem: problem.into(),
            next,
        }
    }

    fn at(self, path: &Path, server: Option<&str>) -> McpError {
        McpError::Invalid {
            path: path.to_path_buf(),
            server: server.map(str::to_string),
            field: self.field,
            problem: self.problem,
            next: self.next.to_string(),
        }
    }
}

const ENV_NAME_NEXT: &str = "Name the variable with `env = \"…\"`: letters, digits and _, not \
                             starting with a digit";

fn check_server(name: &str, s: &Server) -> Result<(), Problem> {
    if !is_server_name(name) {
        return Err(Problem::new(
            "name",
            format!(
                "`{name}` is not a valid server name: use 1 to 32 characters of a-z, 0-9 and -"
            ),
            "Rename the entry",
        ));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(Problem::new(
            "name",
            format!("`{name}` is reserved by Claude Code, which skips a server of that name"),
            "Rename the entry",
        ));
    }
    match (&s.from, &s.version) {
        (Some(_), None) => {
            return Err(Problem::new(
                "version",
                "`version` is required when `from` is given: a registry entry is pinned to an \
                 exact version",
                "Add the version the registry gave, or remove `from` for a server added by hand",
            ));
        }
        (None, Some(_)) => {
            return Err(Problem::new(
                "version",
                "`version` is only allowed together with `from`",
                "Add the registry name as `from`, or remove `version`",
            ));
        }
        _ => {}
    }
    match s.transport {
        Transport::Stdio => {
            if s.command.is_none() {
                return Err(Problem::new(
                    "command",
                    "a stdio server needs `command`",
                    "Add the program to start, or set `transport` to \"http\" or \"sse\" with a \
                     `url`",
                ));
            }
            for (field, present) in [("url", s.url.is_some()), ("headers", s.headers.is_some())] {
                if present {
                    return Err(Problem::new(
                        field,
                        format!("`{field}` is not allowed for a stdio server"),
                        "Remove it, or set `transport` to \"http\" or \"sse\"",
                    ));
                }
            }
        }
        Transport::Http | Transport::Sse => {
            if s.url.is_none() {
                return Err(Problem::new(
                    "url",
                    "an http or sse server needs `url`",
                    "Add the server's URL, or set `transport` to \"stdio\" with a `command`",
                ));
            }
            for (field, present) in [
                ("command", s.command.is_some()),
                ("args", s.args.is_some()),
                ("env", s.env.is_some()),
            ] {
                if present {
                    return Err(Problem::new(
                        field,
                        format!("`{field}` is not allowed for an http or sse server"),
                        "Remove it: a remote server takes `url` and `headers`",
                    ));
                }
            }
        }
    }
    for (key, value) in s.env.iter().flatten() {
        if let Some(var) = value.secret_var(key)
            && !is_env_name(var)
        {
            return Err(Problem::new(
                format!("env.{key}"),
                "the variable it reads is not a valid environment variable name",
                ENV_NAME_NEXT,
            ));
        }
    }
    for (key, value) in s.headers.iter().flatten() {
        if let HeaderValue::Secret { env, .. } = value
            && !is_env_name(env)
        {
            return Err(Problem::new(
                format!("headers.{key}"),
                "the variable it reads is not a valid environment variable name",
                ENV_NAME_NEXT,
            ));
        }
    }
    Ok(())
}

impl Catalog {
    /// `<root>/.fl/mcp.toml`.
    pub fn path(root: &Path) -> PathBuf {
        root.join(".fl").join("mcp.toml")
    }

    /// The project's catalog; `None` when it has none. A catalog that exists
    /// but cannot be read is an error, never `None`.
    pub fn load(root: &Path) -> Result<Option<Catalog>, McpError> {
        let path = Self::path(root);
        match read(&path)? {
            Some(text) => Self::parse(&text, &path).map(Some),
            None => Ok(None),
        }
    }

    /// Reads and checks a catalog's text; `file` names it in a refusal.
    pub fn parse(text: &str, file: &Path) -> Result<Catalog, McpError> {
        let catalog: Catalog =
            toml::from_str(text).map_err(|e| parse_error(file, text, e.span(), e.message()))?;
        if let Some(url) = &catalog.registry
            && let Err(why) = check_registry_url(url)
        {
            return Err(Problem::new(
                "registry",
                format!("fl will not read a registry there: {why}"),
                "Use an https URL, or an http URL to this machine",
            )
            .at(file, None));
        }
        for (name, server) in &catalog.servers {
            check_server(name, server).map_err(|p| p.at(file, Some(name)))?;
        }
        Ok(catalog)
    }
}

fn read(path: &Path) -> Result<Option<String>, McpError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(McpError::Io {
            op: "read",
            path: path.to_path_buf(),
            cause: e.to_string(),
        }),
    }
}

/// The catalog does not parse. The parser's own text quotes the offending
/// line, and a line can hold a pasted token (MCP spec §6), so only the line,
/// the column and the parser's message are shown.
fn parse_error(path: &Path, text: &str, span: Option<Range<usize>>, message: &str) -> McpError {
    let at = span.map_or(0, |s| s.start);
    let (mut line, mut column) = (1, 1);
    for (_, c) in text.char_indices().take_while(|&(i, _)| i < at) {
        if c == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    McpError::Parse {
        path: path.to_path_buf(),
        cause: format!(
            "line {line}, column {column}: {}",
            redact(message.trim_end())
        ),
    }
}

/// The parser's message without the value it quotes from the file: serde
/// names the key or the value it could not read (``unknown variant `…` ``,
/// `invalid type: string "…"`), and either can be a pasted token (MCP spec
/// §6). What follows `expected` is the catalog's own vocabulary, and fl's own
/// messages and `missing field` quote nothing from the file, so they stay.
fn redact(message: &str) -> String {
    if [SECRET_FALSE, HEADER_NEEDS_ENV, "missing field "]
        .iter()
        .any(|own| message.starts_with(own))
    {
        return message.to_string();
    }
    let split = message.rfind(", expected").unwrap_or(message.len());
    let (head, tail) = message.split_at(split);
    // From the first quote to the last, so a value holding a quote of its own
    // is hidden whole.
    match (head.find(['`', '"']), head.rfind(['`', '"'])) {
        (Some(first), Some(last)) if last > first => {
            format!("{}<value>{}{tail}", &head[..first], &head[last + 1..])
        }
        (Some(first), _) => format!("{}<value>{tail}", &head[..first]),
        _ => message.to_string(),
    }
}

/// Edits the catalog's text in place. Each edit is checked as a whole
/// catalog before it is kept, so a refused edit changes nothing; only the
/// edited entry or value is written fresh, and every other byte is kept.
#[derive(Debug)]
pub struct Editor {
    path: PathBuf,
    doc: DocumentMut,
    /// `false` while the file does not exist: its text then starts with
    /// [`HEADER`].
    existed: bool,
    catalog: Catalog,
}

impl Editor {
    /// The project's catalog, or an empty one when it has none. Nothing is
    /// written until [`Editor::save`].
    pub fn open(root: &Path) -> Result<Editor, McpError> {
        let path = Catalog::path(root);
        let (text, existed) = match read(&path)? {
            Some(text) => (text, true),
            None => (String::new(), false),
        };
        let catalog = Catalog::parse(&text, &path)?;
        let doc = text
            .parse::<DocumentMut>()
            .map_err(|e| parse_error(&path, &text, e.span(), e.message()))?;
        Ok(Editor {
            path,
            doc,
            existed,
            catalog,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The catalog as edited so far.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The file's text as edited so far.
    pub fn text(&self) -> String {
        let body = self.doc.to_string();
        if self.existed {
            body
        } else {
            format!("{HEADER}\n{}", body.trim_start_matches('\n'))
        }
    }

    /// Writes the file, creating `.fl/` when it is absent.
    pub fn save(&self) -> Result<(), McpError> {
        let io = |e: std::io::Error| McpError::Io {
            op: "write",
            path: self.path.clone(),
            cause: e.to_string(),
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        std::fs::write(&self.path, self.text()).map_err(io)
    }

    /// Adds `[server.<name>]` after every other entry.
    pub fn add(&mut self, name: &str, server: &Server) -> Result<(), McpError> {
        if self.catalog.servers.contains_key(name) {
            return Err(McpError::AlreadyPresent {
                path: self.path.clone(),
                name: name.to_string(),
            });
        }
        // toml_edit writes a table with no position of its own right after
        // the one before it among the servers, which it keeps in the order
        // their headers stand in the file: here, the last. An inline
        // `server` takes the entry as an inline table.
        self.edit(|doc| {
            table_like(servers_mut(doc)).insert(name, Item::Table(server_table(server)));
        })
    }

    /// Removes the entry, with the comment directly above it.
    pub fn remove(&mut self, name: &str) -> Result<(), McpError> {
        self.known(name)?;
        self.edit(|doc| {
            table_like(servers_mut(doc)).remove(name);
        })
    }

    /// Sets the entry's team default, keeping the rest of its text.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), McpError> {
        self.known(name)?;
        self.edit(|doc| {
            let entry = table_like(servers_mut(doc))
                .get_mut(name)
                .and_then(Item::as_table_like_mut)
                .expect("a server the catalog read is a table");
            set_value(entry, "enabled", Value::from(enabled));
        })
    }

    /// Sets the top-level `registry`, keeping its comments.
    pub fn set_registry(&mut self, url: &str) -> Result<(), McpError> {
        self.edit(|doc| set_value(doc.as_table_mut(), "registry", Value::from(url)))
    }

    /// Writes `server` in place of the entry, keeping the comments above it
    /// and its place in the file (a table with no position of its own is
    /// written where it stands among the servers). Comments inside the old
    /// entry go with it.
    pub fn replace(&mut self, name: &str, server: &Server) -> Result<(), McpError> {
        self.known(name)?;
        self.edit(|doc| {
            let old = table_like(servers_mut(doc))
                .get_mut(name)
                .expect("a server the catalog read is present");
            let mut table = server_table(server);
            match old {
                Item::Table(old) => {
                    *table.decor_mut() = old.decor().clone();
                    *old = table;
                }
                Item::Value(old) => {
                    let mut new = Value::InlineTable(table.into_inline_table());
                    *new.decor_mut() = old.decor().clone();
                    *old = new;
                }
                _ => unreachable!("a server the catalog read is a table"),
            }
        })
    }

    fn known(&self, name: &str) -> Result<(), McpError> {
        if self.catalog.servers.contains_key(name) {
            Ok(())
        } else {
            Err(McpError::NoSuchServer {
                path: self.path.clone(),
                name: name.to_string(),
            })
        }
    }

    fn edit(&mut self, change: impl FnOnce(&mut DocumentMut)) -> Result<(), McpError> {
        let mut doc = self.doc.clone();
        change(&mut doc);
        self.catalog = Catalog::parse(&doc.to_string(), &self.path)?;
        self.doc = doc;
        Ok(())
    }
}

/// The `server` item, created as a table with no header of its own when the
/// catalog has none.
fn servers_mut(doc: &mut DocumentMut) -> &mut Item {
    doc.entry("server").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    })
}

fn table_like(item: &mut Item) -> &mut dyn TableLike {
    item.as_table_like_mut()
        .expect("the catalog read `server` as a table")
}

/// Sets `key` to `value`, keeping the spaces and comment around the old value.
fn set_value(table: &mut dyn TableLike, key: &str, mut value: Value) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(old) => {
            *value.decor_mut() = old.decor().clone();
            *old = value;
        }
        None => {
            table.insert(key, Item::Value(value));
        }
    }
}

/// An entry in the editor's own layout: one line per field, `env` and
/// `headers` as dotted keys.
fn server_table(s: &Server) -> Table {
    let mut t = Table::new();
    t.decor_mut().set_prefix("\n");
    if let Some(from) = &s.from {
        t.insert("from", toml_edit::value(from.as_str()));
    }
    if let Some(version) = &s.version {
        t.insert("version", toml_edit::value(version.as_str()));
    }
    t.insert("enabled", toml_edit::value(s.enabled));
    if let Some(vendors) = &s.vendors {
        let list: Array = vendors.iter().map(|v| v.as_str()).collect();
        t.insert("vendors", toml_edit::value(list));
    }
    t.insert("transport", toml_edit::value(s.transport.as_str()));
    if let Some(command) = &s.command {
        t.insert("command", toml_edit::value(command.as_str()));
    }
    if let Some(args) = &s.args {
        let list: Array = args.iter().map(String::as_str).collect();
        t.insert("args", toml_edit::value(list));
    }
    if let Some(env) = &s.env {
        let values = env.iter().map(|(k, v)| {
            let value = match v {
                EnvValue::Literal(s) => Value::from(s.as_str()),
                EnvValue::Secret { env } => secret(env.as_deref(), None),
            };
            (k, value)
        });
        t.insert("env", dotted(values));
    }
    if let Some(url) = &s.url {
        t.insert("url", toml_edit::value(url.as_str()));
    }
    if let Some(headers) = &s.headers {
        let values = headers.iter().map(|(k, v)| {
            let value = match v {
                HeaderValue::Literal(s) => Value::from(s.as_str()),
                HeaderValue::Secret { env, scheme } => secret(Some(env), scheme.as_deref()),
            };
            (k, value)
        });
        t.insert("headers", dotted(values));
    }
    t
}

fn secret(env: Option<&str>, scheme: Option<&str>) -> Value {
    let mut t = InlineTable::new();
    t.insert("secret", Value::from(true));
    if let Some(env) = env {
        t.insert("env", Value::from(env));
    }
    if let Some(scheme) = scheme {
        t.insert("scheme", Value::from(scheme));
    }
    Value::InlineTable(t)
}

/// `name.KEY = value` lines inside the entry, rather than a table of their own.
fn dotted<'a>(values: impl Iterator<Item = (&'a String, Value)>) -> Item {
    let mut t = Table::new();
    t.set_dotted(true);
    for (k, v) in values {
        t.insert(k, Item::Value(v));
    }
    Item::Table(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpError;

    fn file() -> PathBuf {
        PathBuf::from(".fl/mcp.toml")
    }

    fn parse(text: &str) -> Result<Catalog, McpError> {
        Catalog::parse(text, &file())
    }

    // The editor's own layout for these entries, in the catalog's order (by
    // name): writing them back gives these bytes.
    const FULL: &str = r#"registry = "https://registry.example.com"

[server.docs]
enabled = false
transport = "http"
url = "https://mcp.example.com/mcp"
headers.Authorization = { secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }
headers.X-Api-Key = { secret = true, env = "DOCS_KEY" }
headers.X-Team = "widgets"

[server.events]
enabled = true
transport = "sse"
url = "https://events.example.com/sse"

[server.github]
from = "io.example/github"
version = "2.0.1"
enabled = true
vendors = ["claude", "codex"]
transport = "stdio"
command = "docker"
args = ["run", "-i", "--rm", "-e", "GITHUB_TOKEN", "ghcr.io/example/github:2.0.1"]
env.API_KEY = { secret = true, env = "EXAMPLE_API_KEY" }
env.GITHUB_TOKEN = { secret = true }
env.LOG_LEVEL = "info"
"#;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_field_of_a_valid_catalog_is_read() {
        let c = parse(FULL).unwrap();
        assert_eq!(c.registry.as_deref(), Some("https://registry.example.com"));
        assert_eq!(
            c.servers.keys().collect::<Vec<_>>(),
            ["docs", "events", "github"]
        );
        let env: BTreeMap<String, EnvValue> = [
            ("GITHUB_TOKEN", EnvValue::Secret { env: None }),
            ("LOG_LEVEL", EnvValue::Literal("info".into())),
            (
                "API_KEY",
                EnvValue::Secret {
                    env: Some("EXAMPLE_API_KEY".into()),
                },
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(
            c.servers["github"],
            Server {
                from: Some("io.example/github".into()),
                version: Some("2.0.1".into()),
                enabled: true,
                vendors: Some(vec![VendorName::Claude, VendorName::Codex]),
                transport: Transport::Stdio,
                command: Some("docker".into()),
                args: Some(strings(&[
                    "run",
                    "-i",
                    "--rm",
                    "-e",
                    "GITHUB_TOKEN",
                    "ghcr.io/example/github:2.0.1",
                ])),
                env: Some(env),
                url: None,
                headers: None,
            }
        );
        let headers: BTreeMap<String, HeaderValue> = [
            (
                "Authorization",
                HeaderValue::Secret {
                    env: "DOCS_TOKEN".into(),
                    scheme: Some("Bearer".into()),
                },
            ),
            (
                "X-Api-Key",
                HeaderValue::Secret {
                    env: "DOCS_KEY".into(),
                    scheme: None,
                },
            ),
            ("X-Team", HeaderValue::Literal("widgets".into())),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(
            c.servers["docs"],
            Server {
                from: None,
                version: None,
                enabled: false,
                vendors: None,
                transport: Transport::Http,
                command: None,
                args: None,
                env: None,
                url: Some("https://mcp.example.com/mcp".into()),
                headers: Some(headers),
            }
        );
        assert_eq!(c.servers["events"].transport, Transport::Sse);
    }

    #[test]
    fn a_server_that_gives_only_its_launch_is_on_for_every_vendor() {
        let c = parse("[server.files]\ntransport = \"stdio\"\ncommand = \"npx\"\n").unwrap();
        let s = &c.servers["files"];
        assert!(s.enabled);
        assert_eq!((&s.vendors, &s.args, &s.env), (&None, &None, &None));
        for v in VendorName::ALL {
            assert!(s.is_for(v), "{v:?}");
        }
        let c = parse(FULL).unwrap();
        let gh = &c.servers["github"];
        assert!(gh.is_for(VendorName::Claude) && gh.is_for(VendorName::Codex));
        assert!(!gh.is_for(VendorName::Antigravity));
        assert_eq!(
            VendorName::ALL.map(VendorName::as_str),
            ["claude", "codex", "antigravity"]
        );
    }

    #[test]
    fn a_secret_names_its_variable_and_literals_are_listed_for_the_warning() {
        let c = parse(FULL).unwrap();
        let env = c.servers["github"].env.as_ref().unwrap();
        assert_eq!(
            env["GITHUB_TOKEN"].secret_var("GITHUB_TOKEN"),
            Some("GITHUB_TOKEN")
        );
        assert_eq!(
            env["API_KEY"].secret_var("API_KEY"),
            Some("EXAMPLE_API_KEY")
        );
        assert_eq!(env["LOG_LEVEL"].secret_var("LOG_LEVEL"), None);
        assert_eq!(c.servers["github"].literal_values(), ["env.LOG_LEVEL"]);
        assert_eq!(c.servers["docs"].literal_values(), ["headers.X-Team"]);
        assert!(c.servers["events"].literal_values().is_empty());
    }

    #[test]
    fn a_variable_name_as_fl_asks_for_one_is_capitals_digits_and_underscores() {
        for ok in ["A", "_", "GITHUB_TOKEN", "_TOKEN_2"] {
            assert!(is_variable_name(ok), "{ok}");
        }
        for bad in [
            "",
            "2A",
            "a",
            "Ab",
            "Api_Token",
            "ghp_example0token",
            "c2VjcmV0dG9rZW4",
            "A B",
            "A-B",
        ] {
            assert!(!is_variable_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_header_name_as_fl_asks_for_one_is_letters_and_hyphens_only() {
        for ok in ["Authorization", "X-Api-Key", "X-Goog-Api-Key", "A"] {
            assert!(is_header_name(ok), "{ok}");
        }
        assert!(is_header_name(&"a".repeat(64)));
        for bad in [
            "",
            "-A",
            "2A",
            "X_Key",
            "sk-proj-Ab3dEf9GhIjK2LmN",
            "ghp_example0token",
            "c2VjcmV0dG9rZW4",
            "A B",
        ] {
            assert!(!is_header_name(bad), "{bad}");
        }
        assert!(!is_header_name(&"a".repeat(65)));
    }

    #[test]
    fn a_server_lists_the_variables_its_secrets_are_read_from_by_name_only() {
        let c = parse(FULL).unwrap();
        let vars = |name: &str| {
            c.servers[name]
                .secret_vars()
                .into_iter()
                .collect::<Vec<_>>()
        };
        assert_eq!(vars("github"), ["EXAMPLE_API_KEY", "GITHUB_TOKEN"]);
        assert_eq!(vars("docs"), ["DOCS_KEY", "DOCS_TOKEN"]);
        assert!(vars("events").is_empty());
    }

    #[test]
    fn the_catalog_lives_in_dot_fl_and_an_absent_one_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Catalog::path(dir.path()),
            dir.path().join(".fl").join("mcp.toml")
        );
        assert_eq!(Catalog::load(dir.path()).unwrap(), None);
        // Opening an editor writes nothing until it is saved.
        Editor::open(dir.path()).unwrap();
        assert!(!dir.path().join(".fl").exists());
    }

    #[test]
    fn a_new_catalog_starts_with_the_header_and_one_blank_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut ed = Editor::open(dir.path()).unwrap();
        ed.add("tracker", &tracker("1.0.0")).unwrap();
        assert_eq!(
            ed.text(),
            format!(
                "{HEADER}\n[server.tracker]\n{}",
                TRACKER_BODY.replace(" # on for everyone", "")
            )
        );
    }

    #[test]
    fn a_catalog_that_cannot_be_read_is_an_error_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(Catalog::path(dir.path())).unwrap();
        let err = Catalog::load(dir.path()).unwrap_err();
        assert!(matches!(err, McpError::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("could not read"), "{err}");
        let err = Editor::open(dir.path()).err().unwrap();
        assert!(matches!(err, McpError::Io { .. }), "{err:?}");
    }

    fn refused(text: &str) -> String {
        match parse(text) {
            Err(e @ McpError::Invalid { .. }) => e.to_string(),
            other => panic!("expected a catalog rule to refuse {text:?}, got {other:?}"),
        }
    }

    fn stdio(name: &str, extra: &str) -> String {
        format!("[server.{name}]\ntransport = \"stdio\"\ncommand = \"npx\"\n{extra}")
    }

    fn remote(transport: &str, extra: &str) -> String {
        format!(
            "[server.docs]\ntransport = \"{transport}\"\nurl = \"https://mcp.example.com\"\n{extra}"
        )
    }

    #[test]
    fn each_catalog_rule_refuses_naming_the_server_and_the_field() {
        let long = "a".repeat(33);
        let cases: Vec<(String, &str, &str, &str)> = vec![
            (
                stdio(&long, ""),
                long.as_str(),
                "name",
                "is not a valid server name",
            ),
            (
                stdio("GitHub", ""),
                "GitHub",
                "name",
                "is not a valid server name",
            ),
            (
                stdio("my_server", ""),
                "my_server",
                "name",
                "is not a valid server name",
            ),
            (stdio("\"\"", ""), "", "name", "is not a valid server name"),
            (
                stdio("workspace", ""),
                "workspace",
                "name",
                "is reserved by Claude Code",
            ),
            (
                stdio("computer-use", ""),
                "computer-use",
                "name",
                "is reserved by Claude Code",
            ),
            (
                stdio("claude-in-chrome", ""),
                "claude-in-chrome",
                "name",
                "is reserved by Claude Code",
            ),
            (
                stdio("gh", "from = \"io.example/github\"\n"),
                "gh",
                "version",
                "`version` is required when `from` is given",
            ),
            (
                stdio("gh", "version = \"2.0.1\"\n"),
                "gh",
                "version",
                "is only allowed together with `from`",
            ),
            (
                "[server.gh]\ntransport = \"stdio\"\nargs = [\"x\"]\n".into(),
                "gh",
                "command",
                "a stdio server needs `command`",
            ),
            (
                stdio("gh", "url = \"https://mcp.example.com\"\n"),
                "gh",
                "url",
                "is not allowed for a stdio server",
            ),
            (
                stdio("gh", "headers.X-Team = \"widgets\"\n"),
                "gh",
                "headers",
                "is not allowed for a stdio server",
            ),
            (
                "[server.docs]\ntransport = \"http\"\n".into(),
                "docs",
                "url",
                "an http or sse server needs `url`",
            ),
            (
                "[server.docs]\ntransport = \"sse\"\n".into(),
                "docs",
                "url",
                "an http or sse server needs `url`",
            ),
            (
                remote("http", "command = \"npx\"\n"),
                "docs",
                "command",
                "is not allowed for an http or sse server",
            ),
            (
                remote("sse", "args = []\n"),
                "docs",
                "args",
                "is not allowed for an http or sse server",
            ),
            (
                remote("http", "env.LOG_LEVEL = \"info\"\n"),
                "docs",
                "env",
                "is not allowed for an http or sse server",
            ),
            (
                stdio("gh", "env.TOKEN = { secret = true, env = \"1TOKEN\" }\n"),
                "gh",
                "env.TOKEN",
                "is not a valid environment variable name",
            ),
            (
                stdio("gh", "env.MY-TOKEN = { secret = true }\n"),
                "gh",
                "env.MY-TOKEN",
                "is not a valid environment variable name",
            ),
            (
                remote(
                    "http",
                    "headers.Authorization = { secret = true, env = \"A-B\" }\n",
                ),
                "docs",
                "headers.Authorization",
                "is not a valid environment variable name",
            ),
        ];
        for (text, server, field, phrase) in cases {
            let msg = refused(&text);
            assert!(msg.starts_with(".fl/mcp.toml: "), "{msg}");
            assert!(msg.contains(&format!("server `{server}`")), "{msg}");
            assert!(msg.contains(&format!("field `{field}`")), "{msg}");
            assert!(msg.contains(phrase), "{phrase:?} not in {msg}");
        }
        let msg = refused(&format!(
            "registry = \"http://registry.example.com\"\n{}",
            stdio("gh", "")
        ));
        assert!(msg.contains("field `registry`"), "{msg}");
        assert!(msg.contains("fl will not read a registry there"), "{msg}");
        assert!(msg.contains("is not this machine"), "{msg}");
        assert!(!msg.contains("server `"), "{msg}");
    }

    // A value in the catalog can be a pasted token; a refusal names the
    // server and the field, never the value (MCP spec §6).
    #[test]
    fn a_validation_error_never_repeats_a_value() {
        let cases = [
            (
                remote(
                    "http",
                    "headers.Authorization = { secret = true, env = \"Bearer sk-live-4f9a2c\" }\n",
                ),
                "server `docs`, field `headers.Authorization`: the variable it reads is not a \
                 valid environment variable name",
            ),
            (
                stdio(
                    "gh",
                    "env.TOKEN = { secret = true, env = \"sk-live-4f9a2c-abc\" }\n",
                ),
                "server `gh`, field `env.TOKEN`: the variable it reads is not a valid \
                 environment variable name",
            ),
            (
                format!(
                    "registry = \"http://sk-live-4f9a2c.example.com\"\n{}",
                    stdio("gh", "")
                ),
                "field `registry`: fl will not read a registry there: it is not https, and its \
                 host is not this machine",
            ),
        ];
        for (text, phrase) in cases {
            let msg = refused(&text);
            assert!(msg.contains(phrase), "{phrase:?} not in {msg}");
            assert!(!msg.contains("sk-live") && !msg.contains("4f9a"), "{msg}");
        }
    }

    #[test]
    fn names_and_variables_at_the_limits_are_accepted() {
        for name in ["a", "my-server-2", &"a".repeat(32)] {
            parse(&stdio(name, "")).unwrap();
        }
        // A literal value is the server's own business; only a variable fl
        // names in a vendor file must be a valid name.
        parse(&stdio("gh", "env.MY-LEVEL = \"info\"\n")).unwrap();
        parse(&stdio(
            "gh",
            "env.MY-TOKEN = { secret = true, env = \"_TOKEN_2\" }\n",
        ))
        .unwrap();
        for ok in ["A", "_", "a_1", "GITHUB_TOKEN"] {
            assert!(is_env_name(ok), "{ok}");
        }
        for bad in ["", "1A", "A-B", "A B", "É"] {
            assert!(!is_env_name(bad), "{bad}");
        }
    }

    fn unparsed(text: &str) -> String {
        match parse(text) {
            Err(e @ McpError::Parse { .. }) => e.to_string(),
            other => panic!("expected {text:?} not to parse, got {other:?}"),
        }
    }

    #[test]
    fn unknown_fields_and_malformed_values_are_refused_at_every_level() {
        let cases: Vec<(String, &str)> = vec![
            (
                format!("colour = \"red\"\n{}", stdio("gh", "")),
                "unknown field <value>, expected `registry` or `server`",
            ),
            (
                format!("project = \"app\"\n{}", stdio("gh", "")),
                "unknown field <value>, expected `registry` or `server`",
            ),
            (
                stdio("gh", "cmd = \"npx\"\n"),
                "unknown field <value>, expected one of `from`, `version`",
            ),
            (
                stdio("gh", "env.T = { secret = true, scheme = \"Bearer\" }\n"),
                "unknown field <value>, expected `secret` or `env`",
            ),
            (
                remote(
                    "http",
                    "headers.A = { secret = true, env = \"T\", prefix = \"x\" }\n",
                ),
                "unknown field <value>, expected one of `secret`, `env`, `scheme`",
            ),
            (
                stdio("gh", "env.T = { secret = false }\n"),
                "`secret = false` is not allowed",
            ),
            (
                remote("http", "headers.A = { secret = false, env = \"T\" }\n"),
                "`secret = false` is not allowed",
            ),
            (
                stdio("gh", "env.T = { env = \"T\" }\n"),
                "missing field `secret`",
            ),
            (
                remote("http", "headers.A = { secret = true }\n"),
                "a secret header needs `env",
            ),
            (stdio("gh", "env.T = 3\n"), "a string, or a table"),
            (
                stdio("gh", "vendors = [\"gemini\"]\n"),
                "unknown variant <value>, expected one of `claude`, `codex`, `antigravity`",
            ),
            (
                "[server.gh]\ntransport = \"websocket\"\nurl = \"https://x.example\"\n".into(),
                "unknown variant <value>, expected one of `stdio`, `http`, `sse`",
            ),
            (
                "[server.gh]\ncommand = \"npx\"\n".into(),
                "missing field `transport`",
            ),
        ];
        for (text, phrase) in cases {
            let msg = unparsed(&text);
            assert!(
                msg.starts_with(".fl/mcp.toml is not a valid MCP catalog"),
                "{msg}"
            );
            assert!(msg.contains(phrase), "{phrase:?} not in {msg}");
            // The key or value the file holds is never repeated.
            assert!(
                !msg.contains("unknown field `") && !msg.contains("unknown variant `"),
                "{msg}"
            );
        }
    }

    #[test]
    fn a_parse_error_names_the_line_and_column_and_never_quotes_the_file() {
        // The parser's own message quotes the offending line, and a line can
        // hold a pasted token: a syntax error, and a shape error on a line
        // that parses as TOML.
        let cases = [
            (
                stdio("gh", "env.T = sk-live-4f9a2c\n"),
                "line 4, column 9: ",
            ),
            (
                stdio("gh", "token = \"sk-live-4f9a2c\"\n"),
                "line 4, column 1: unknown field <value>, expected one of `from`",
            ),
            // A value serde quotes back: where a boolean goes, as the
            // transport, and as the team default.
            (
                stdio("gh", "env.T = { secret = \"sk-live-4f9a2c\" }\n"),
                "line 4, column 20: invalid type: string <value>, expected a boolean",
            ),
            (
                "[server.gh]\ntransport = \"sk-live-4f9a2c\"\n".into(),
                "line 2, column 13: unknown variant <value>, expected one of `stdio`, `http`, `sse`",
            ),
            (
                stdio("gh", "enabled = \"sk-live-4f9a2c\"\n"),
                "line 4, column 11: invalid type: string <value>, expected a boolean",
            ),
            // A token holding `, expected` is hidden whole.
            (
                "[server.gh]\ntransport = \"sk-live, expected 4f9a\"\n".into(),
                "line 2, column 13: unknown variant <value>, expected one of `stdio`",
            ),
            // A token holding quotes of its own is hidden whole.
            (
                "[server.gh]\ntransport = 'sk-live`4f9a\"2c'\n".into(),
                "line 2, column 13: unknown variant <value>, expected one of `stdio`",
            ),
        ];
        for (text, phrase) in cases {
            let msg = unparsed(&text);
            assert!(!msg.contains("sk-live") && !msg.contains("4f9a"), "{msg}");
            assert!(
                msg.starts_with(&format!(
                    ".fl/mcp.toml is not a valid MCP catalog: {phrase}"
                )),
                "{phrase:?} not in {msg}"
            );
        }
    }

    #[test]
    fn a_registry_is_https_or_http_on_this_machine() {
        for ok in [
            "https://registry.example.com",
            "https://registry.example.com:8443/v0.1?x=1",
            "http://127.0.0.1:8080",
            "http://localhost",
            "http://[::1]:9/",
        ] {
            assert_eq!(check_registry_url(ok), Ok(()), "{ok}");
        }
        for (bad, phrase) in [
            (
                "ftp://registry.example.com",
                "it is neither https:// nor http://",
            ),
            ("registry.example.com", "it is neither https:// nor http://"),
            ("http://registry.example.com", "is not this machine"),
            ("http://127.0.0.1.example.com", "is not this machine"),
            ("http://[::2]/", "is not this machine"),
            (
                "https://user:pw@registry.example.com",
                "it carries a user name or password",
            ),
            (
                "http://user@127.0.0.1",
                "it carries a user name or password",
            ),
            ("https://", "it names no host"),
            ("http://:80/", "it names no host"),
            ("http://127.0.0.1:/", "its port is not a number"),
            (
                "http://127.0.0.1:80.example.com",
                "its port is not a number",
            ),
            ("http://[::1]x/", "its port is not a number"),
            (
                "https://registry.example.com/a b",
                "it contains a space or a control character",
            ),
            (
                "https://registry.example.com\u{7f}",
                "it contains a space or a control character",
            ),
        ] {
            let why = check_registry_url(bad).unwrap_err();
            assert!(why.contains(phrase), "{bad}: {phrase:?} not in {why}");
        }
    }

    fn editor_on(text: &str) -> (tempfile::TempDir, Editor) {
        let dir = tempfile::tempdir().unwrap();
        let path = Catalog::path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        let ed = Editor::open(dir.path()).unwrap();
        (dir, ed)
    }

    #[test]
    fn a_catalog_the_editor_writes_reads_back_the_same_in_its_own_layout() {
        let dir = tempfile::tempdir().unwrap();
        let c = parse(FULL).unwrap();
        let mut ed = Editor::open(dir.path()).unwrap();
        ed.set_registry(c.registry.as_deref().unwrap()).unwrap();
        for (name, s) in &c.servers {
            ed.add(name, s).unwrap();
        }
        assert_eq!(ed.text(), format!("{HEADER}\n{FULL}"));
        assert_eq!(ed.catalog(), &c);
        ed.save().unwrap();
        let path = Catalog::path(dir.path());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ed.text());
        assert_eq!(Catalog::load(dir.path()).unwrap(), Some(c));
        // Reopened, the file is kept as written: an edit adds no second header.
        let mut ed = Editor::open(dir.path()).unwrap();
        ed.set_enabled("events", false).unwrap();
        assert_eq!(
            ed.text(),
            format!(
                "{HEADER}\n{}",
                FULL.replace(
                    "enabled = true\ntransport = \"sse\"",
                    "enabled = false\ntransport = \"sse\""
                )
            )
        );
    }

    const COMMENTED: &str = r#"# Our servers.
registry = "https://registry.example.com" # the public one

# Files on disk.
[server.files]
transport = "stdio"
command = "npx"
args = ["-y", "@example/files@1.0.0"] # pinned by hand

# The issue tracker.
[server.tracker]
from = "io.example/tracker"
version = "1.0.0"
enabled = true # on for everyone
transport = "stdio"
command = "npx"
args = ["-y", "@example/tracker@1.0.0"]
# Below the tracker.

# Docs, remote.
[server.docs]
transport   =   "http"
url = "https://mcp.example.com/mcp"
"#;

    const TRACKER_BODY: &str = r#"from = "io.example/tracker"
version = "1.0.0"
enabled = true # on for everyone
transport = "stdio"
command = "npx"
args = ["-y", "@example/tracker@1.0.0"]
"#;

    fn tracker(version: &str) -> Server {
        Server {
            from: Some("io.example/tracker".into()),
            version: Some(version.into()),
            enabled: true,
            vendors: None,
            transport: Transport::Stdio,
            command: Some("npx".into()),
            args: Some(strings(&["-y", &format!("@example/tracker@{version}")])),
            env: None,
            url: None,
            headers: None,
        }
    }

    #[test]
    fn switching_a_server_changes_one_value_and_keeps_every_comment() {
        let (_dir, mut ed) = editor_on(COMMENTED);
        ed.set_enabled("tracker", false).unwrap();
        assert_eq!(
            ed.text(),
            COMMENTED.replace("enabled = true # on for", "enabled = false # on for")
        );
        assert!(!ed.catalog().servers["tracker"].enabled);
        let (_dir, mut ed) = editor_on(COMMENTED);
        ed.set_enabled("files", false).unwrap();
        assert_eq!(
            ed.text(),
            COMMENTED.replace("# pinned by hand\n", "# pinned by hand\nenabled = false\n")
        );
    }

    #[test]
    fn replacing_a_server_keeps_the_comments_around_it_and_every_other_entry() {
        let (_dir, mut ed) = editor_on(COMMENTED);
        ed.replace("tracker", &tracker("1.1.0")).unwrap();
        let new_body = r#"from = "io.example/tracker"
version = "1.1.0"
enabled = true
transport = "stdio"
command = "npx"
args = ["-y", "@example/tracker@1.1.0"]
"#;
        assert_eq!(ed.text(), COMMENTED.replace(TRACKER_BODY, new_body));
        assert_eq!(ed.catalog().servers["tracker"], tracker("1.1.0"));
        assert_eq!(
            ed.catalog().servers.keys().collect::<Vec<_>>(),
            ["docs", "files", "tracker"]
        );
    }

    #[test]
    fn removing_a_server_takes_its_comment_above_and_nothing_else() {
        let (_dir, mut ed) = editor_on(COMMENTED);
        ed.remove("tracker").unwrap();
        let gone = format!("\n# The issue tracker.\n[server.tracker]\n{TRACKER_BODY}");
        assert_eq!(ed.text(), COMMENTED.replace(&gone, ""));
        assert_eq!(
            ed.catalog().servers.keys().collect::<Vec<_>>(),
            ["docs", "files"]
        );
    }

    #[test]
    fn adding_then_removing_a_server_leaves_the_file_as_it_was() {
        let only_registry = "registry = \"https://registry.example.com\" # ours\n";
        for text in [COMMENTED, FULL, only_registry, "# Nothing yet.\n"] {
            let (_dir, mut ed) = editor_on(text);
            ed.add("extra", &tracker("3.0.0")).unwrap();
            assert!(
                ed.text()
                    .starts_with(text.trim_end_matches("# Nothing yet.\n"))
            );
            assert!(
                ed.text()
                    .contains("\n[server.extra]\nfrom = \"io.example/tracker\"\n")
            );
            assert_eq!(ed.catalog().servers["extra"], tracker("3.0.0"));
            ed.remove("extra").unwrap();
            assert_eq!(ed.text(), text);
        }
    }

    #[test]
    fn setting_the_registry_keeps_its_comments_or_adds_it_at_the_top() {
        let (_dir, mut ed) = editor_on(COMMENTED);
        ed.set_registry("https://mirror.example.com").unwrap();
        assert_eq!(
            ed.text(),
            COMMENTED.replace("registry.example.com\" #", "mirror.example.com\" #")
        );
        let (_dir, mut ed) = editor_on(&stdio("files", ""));
        ed.set_registry("http://127.0.0.1:8080").unwrap();
        assert_eq!(
            ed.text(),
            format!(
                "registry = \"http://127.0.0.1:8080\"\n{}",
                stdio("files", "")
            )
        );
    }

    #[test]
    fn entries_written_inline_are_edited_in_place() {
        let inline = "[server]\nfiles = { transport = \"stdio\", command = \"npx\" } # inline\n";
        let (_dir, mut ed) = editor_on(inline);
        ed.set_enabled("files", false).unwrap();
        // toml_edit keeps the space that stood before the closing brace.
        assert_eq!(
            ed.text(),
            "[server]\nfiles = { transport = \"stdio\", command = \"npx\" , enabled = false } \
             # inline\n"
        );
        ed.replace("files", &tracker("1.0.0")).unwrap();
        assert!(ed.text().ends_with("# inline\n"), "{}", ed.text());
        assert_eq!(ed.catalog().servers["files"], tracker("1.0.0"));

        let nested = "server = { files = { transport = \"stdio\", command = \"npx\" } }\n";
        let (_dir, mut ed) = editor_on(nested);
        ed.add("tracker", &tracker("1.0.0")).unwrap();
        assert_eq!(ed.catalog().servers["tracker"], tracker("1.0.0"));
        assert_eq!(ed.catalog().servers.len(), 2);
    }

    #[test]
    fn an_edit_the_catalog_would_refuse_changes_nothing() {
        let (_dir, mut ed) = editor_on(COMMENTED);
        let err = ed.add("tracker", &tracker("2.0.0")).unwrap_err();
        assert!(matches!(err, McpError::AlreadyPresent { .. }), "{err:?}");
        assert!(
            err.to_string().contains("is already in the catalog"),
            "{err}"
        );
        for err in [
            ed.remove("nope").unwrap_err(),
            ed.set_enabled("nope", false).unwrap_err(),
            ed.replace("nope", &tracker("1.0.0")).unwrap_err(),
        ] {
            assert!(matches!(err, McpError::NoSuchServer { .. }), "{err:?}");
            assert!(
                err.to_string().contains("there is no server `nope`"),
                "{err}"
            );
        }
        let err = ed.add("workspace", &tracker("1.0.0")).unwrap_err();
        assert!(
            err.to_string().contains("is reserved by Claude Code"),
            "{err}"
        );
        let mut no_command = tracker("1.0.0");
        no_command.command = None;
        let err = ed.add("extra", &no_command).unwrap_err();
        assert!(
            err.to_string().contains("a stdio server needs `command`"),
            "{err}"
        );
        let err = ed.replace("files", &no_command).unwrap_err();
        assert!(
            err.to_string().contains("a stdio server needs `command`"),
            "{err}"
        );
        let err = ed.set_registry("http://registry.example.com").unwrap_err();
        assert!(
            err.to_string()
                .contains("fl will not read a registry there"),
            "{err}"
        );
        assert_eq!(ed.text(), COMMENTED);
        assert_eq!(ed.catalog(), &parse(COMMENTED).unwrap());
    }
}
