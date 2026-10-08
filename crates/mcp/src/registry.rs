//! A read-only client for an MCP registry, API `v0.1` (MCP spec §3.1): the
//! official registry, or any registry that serves the same OpenAPI document.
//!
//! It sends no credential and follows no redirect. It judges the status and
//! the content type before it reads the body as data, and reads no more than
//! 4 MiB of it, so a proxy's HTML page, a moved registry or a runaway answer
//! is reported as the registry's failure, never as a parse error.

use crate::McpError;
use crate::catalog::check_registry_url;
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;
use std::time::Duration;

/// The registry API version fl reads (MCP spec §3.1).
pub const API_VERSION: &str = "v0.1";
/// The most fl reads of one answer (MCP spec §3.1).
pub const BODY_LIMIT: u64 = 4 << 20;
/// The most pages `search` reads (MCP spec §3.1).
pub const SEARCH_PAGES: usize = 20;
/// Servers per page: the registry's own maximum.
const PAGE_SIZE: usize = 100;
const USER_AGENT: &str = concat!("fl/", env!("CARGO_PKG_VERSION"));

pub struct Registry {
    agent: ureq::Agent,
    /// The address as given, without a trailing `/`.
    base: String,
    /// `scheme://host[:port]` of `base`.
    origin: String,
}

/// What `search` read: the servers whose name holds the text, at their
/// latest version, and whether it stopped before the registry's last page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    pub servers: Vec<Summary>,
    pub stopped_early: bool,
}

/// One server as a search shows it. Only these fields are read, so a server
/// whose launch spec breaks the registry's own schema, which the registry
/// serves, never fails a search.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "SummaryJson")]
pub struct Summary {
    pub name: String,
    pub description: String,
    pub version: String,
    /// As the registry spells it: `active`, `deprecated`, or one fl does not
    /// know, shown as it is.
    pub status: String,
}

impl Registry {
    /// A client for the registry at `url`: https, or http to this machine,
    /// with no user name or password (MCP spec §3.1).
    pub fn new(url: &str) -> Result<Registry, McpError> {
        check_registry_url(url).map_err(|clause| McpError::RegistryAddress {
            url: url.to_string(),
            clause,
        })?;
        let base = url.trim_end_matches('/').to_string();
        let scheme_end = base.find("://").map_or(0, |i| i + 3);
        let origin_end = base[scheme_end..]
            .find(['/', '?', '#'])
            .map_or(base.len(), |i| scheme_end + i);
        Ok(Registry {
            agent: agent(),
            origin: base[..origin_end].to_string(),
            base,
        })
    }

    /// The registry's address, without a trailing `/`.
    pub fn url(&self) -> &str {
        &self.base
    }

    /// The servers whose name holds `text`, in any case, at their latest
    /// version: the registry searches names only. Reads up to
    /// [`SEARCH_PAGES`] pages; a failure on any page is an error, never a
    /// short list.
    pub fn search(&self, text: &str) -> Result<Search, McpError> {
        let mut servers = Vec::new();
        let mut cursor: Option<String> = None;
        let wanted = text.to_lowercase();
        for _ in 0..SEARCH_PAGES {
            let mut path = format!(
                "/{API_VERSION}/servers?search={}&version=latest&limit={PAGE_SIZE}",
                encode(text)
            );
            if let Some(c) = &cursor {
                path.push_str("&cursor=");
                path.push_str(&encode(c));
            }
            let page: ServerList<Summary> = self.get(&path, None)?;
            // By name only (MCP spec §3.1), whatever the registry sends: one
            // that ignores `search` would otherwise list every server it has.
            let named = |s: &Summary| s.name.to_lowercase().contains(&wanted);
            servers.extend(page.servers.into_iter().filter(named));
            match page.metadata.next_cursor.filter(|c| !c.is_empty()) {
                Some(next) => cursor = Some(next),
                None => {
                    return Ok(Search {
                        servers,
                        stopped_early: false,
                    });
                }
            }
        }
        Ok(Search {
            servers,
            stopped_early: true,
        })
    }

    /// Every version of `name`, deleted ones included, in the registry's
    /// order (which it does not document).
    pub fn versions(&self, name: &str) -> Result<Vec<ServerResponse>, McpError> {
        let path = format!(
            "/{API_VERSION}/servers/{}/versions?include_deleted=true",
            encode(name)
        );
        let list: ServerList<ServerResponse> = self.get(&path, Some((name, None)))?;
        Ok(list.servers)
    }

    /// One version of `name`, or its latest with `version` `latest`, even
    /// when deleted: its status says so (MCP spec §3.2 step 2).
    pub fn version(&self, name: &str, version: &str) -> Result<ServerResponse, McpError> {
        let path = format!(
            "/{API_VERSION}/servers/{}/versions/{}?include_deleted=true",
            encode(name),
            encode(version)
        );
        let asked = (version != "latest").then_some(version);
        self.get(&path, Some((name, asked)))
    }

    /// One GET, judged in this order: the transport, a redirect, an HTML
    /// page, a 404 on a server, the body's size, any other status, and only
    /// then the body as data. `server` names what a 404 means is missing.
    fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        server: Option<(&str, Option<&str>)>,
    ) -> Result<T, McpError> {
        let unreachable = |e: ureq::Error| McpError::Unreachable {
            registry: self.base.clone(),
            cause: e.to_string(),
        };
        let failed = |problem: String| McpError::Registry {
            registry: self.base.clone(),
            request: format!("GET {path}"),
            problem,
        };
        let mut resp = self
            .agent
            .get(&format!("{}{path}", self.base))
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(unreachable)?;
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let mime = header("content-type");
        let mime = mime
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if (300..400).contains(&status) {
            return Err(failed(self.redirect(status, &header("location"))));
        }
        if mime == "text/html" {
            return Err(failed(format!(
                "it answered with an HTML page (status {status}), not the registry API. The \
                 address may not be a registry, or a proxy in front of it failed; check the \
                 address, or retry later"
            )));
        }
        if status == 404
            && let Some((name, version)) = server
        {
            return Err(McpError::NotFound {
                registry: self.base.clone(),
                name: name.to_string(),
                version: version.map(str::to_string),
            });
        }
        // The cap counts the DECODED bytes (the reader reads one more than the
        // cap, to tell a body of exactly the cap from a longer one): ureq's own
        // limit sits under its gzip decoder, so alone it counts the wire.
        let too_big = || {
            failed(
                "its answer is larger than 4 MiB, which fl will not read. The address may \
                 not be a registry; check it"
                    .to_string(),
            )
        };
        // Bytes, not text: the registry's data is read as it came, so a byte that is
        // not UTF-8 is a body that is not the registry API's, never rewritten.
        let read = read_capped(
            resp.body_mut().with_config().limit(BODY_LIMIT).reader(),
            BODY_LIMIT,
        );
        let bytes = match read.map_err(ureq::Error::from) {
            Ok(Some(bytes)) => bytes,
            Ok(None) | Err(ureq::Error::BodyExceedsLimit(_)) => return Err(too_big()),
            Err(e) => return Err(unreachable(e)),
        };
        if !(200..300).contains(&status) {
            let detail = (mime == "application/problem+json")
                .then(|| serde_json::from_slice::<ProblemJson>(&bytes).ok())
                .flatten()
                .and_then(|p| p.detail)
                .map(|d| format!(": {}", printable(&d)))
                .unwrap_or_default();
            return Err(failed(format!(
                "it answered {status}{detail}. Retry later; if it goes on, the registry's \
                 operator can say why"
            )));
        }
        serde_json::from_slice(&bytes).map_err(|e| {
            failed(format!(
                "its answer is not the registry API's shape ({e}). Check that the address is a \
                 registry serving API {API_VERSION}"
            ))
        })
    }

    /// A redirect names where it points only when that is the registry's
    /// own origin.
    fn redirect(&self, status: u16, location: &str) -> String {
        let same_origin = (location.starts_with('/') && !location.starts_with("//"))
            || location
                .get(..self.origin.len())
                .is_some_and(|o| o.eq_ignore_ascii_case(&self.origin))
                && matches!(
                    location[self.origin.len()..].chars().next(),
                    None | Some('/' | '?' | '#')
                );
        let to = if same_origin {
            format!("to {}", printable(location))
        } else {
            "off its own origin".to_string()
        };
        format!(
            "it answered {status} with a redirect {to}, and fl follows no redirect. If the \
             registry has moved, set its new address with `fl mcp registry <url>`"
        )
    }
}

/// Everything `reader` yields, or `None` when it yields more than `cap` bytes.
/// It pulls at most `cap + 1` bytes from `reader`, so a body that decodes to
/// gigabytes never reaches memory.
fn read_capped(reader: impl Read, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader.take(cap + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= cap).then_some(bytes))
}

/// No redirect is followed, and nothing is sent but the request.
fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(30)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// One path segment or query value: the unreserved characters kept, every
/// other byte as `%XX`, so `/` is `%2F` and `+` is `%2B`.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Registry text for a terminal: no control, format or invisible characters
/// (see [`is_unseen`]), at most 300 of the rest.
pub(crate) fn printable(text: &str) -> String {
    text.chars().filter(|c| !is_unseen(*c)).take(300).collect()
}

/// A character a person cannot see for what it is: a control character, or a
/// format or invisible one (soft hyphen, zero-width, bidirectional marks and
/// overrides, the byte-order mark, language tags). Such a character in a text
/// can make it read as another.
pub(crate) fn is_unseen(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{AD}'
                | '\u{600}'..='\u{605}'
                | '\u{61C}'
                | '\u{6DD}'
                | '\u{70F}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0001}'
                | '\u{E0020}'..='\u{E007F}'
        )
}

/// Whether `text` holds an [`is_unseen`] character.
pub(crate) fn has_unseen(text: &str) -> bool {
    text.chars().any(is_unseen)
}

#[derive(Deserialize)]
struct ProblemJson {
    detail: Option<String>,
}

#[derive(Deserialize)]
struct ServerList<T> {
    servers: Vec<T>,
    metadata: ListMetadata,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListMetadata {
    next_cursor: Option<String>,
}

// The models read the fields fl uses and ignore the rest: the registry adds
// fields within `v0.1`.

/// One server version, as the registry serves it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerResponse {
    pub server: ServerJson,
    /// The registry's own record of it, `_meta."io.modelcontextprotocol.registry/official"`.
    #[serde(rename = "_meta", deserialize_with = "official")]
    pub meta: Official,
}

#[derive(Deserialize)]
struct MetaJson {
    #[serde(rename = "io.modelcontextprotocol.registry/official")]
    official: Official,
}

fn official<'de, D: Deserializer<'de>>(d: D) -> Result<Official, D::Error> {
    MetaJson::deserialize(d).map(|m| m.official)
}

#[derive(Deserialize)]
struct SummaryJson {
    server: SummaryServer,
    #[serde(rename = "_meta")]
    meta: SummaryMeta,
}

#[derive(Deserialize)]
struct SummaryServer {
    name: String,
    #[serde(default)]
    description: String,
    version: String,
}

#[derive(Deserialize)]
struct SummaryMeta {
    #[serde(rename = "io.modelcontextprotocol.registry/official")]
    official: SummaryOfficial,
}

#[derive(Deserialize)]
struct SummaryOfficial {
    status: String,
}

impl From<SummaryJson> for Summary {
    fn from(j: SummaryJson) -> Self {
        Summary {
            name: j.server.name,
            description: j.server.description,
            version: j.server.version,
            status: j.meta.official.status,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Official {
    pub status: Status,
    pub status_message: Option<String>,
    pub is_latest: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Deprecated,
    Deleted,
}

/// A server's `server.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerJson {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub packages: Vec<Package>,
    #[serde(default)]
    pub remotes: Vec<Remote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub registry_type: RegistryType,
    pub identifier: String,
    /// npm, PyPI and NuGet only: an OCI identifier carries its own tag.
    pub version: Option<String>,
    pub transport: PackageTransport,
    #[serde(default)]
    pub runtime_arguments: Vec<Argument>,
    #[serde(default)]
    pub package_arguments: Vec<Argument>,
    #[serde(default)]
    pub environment_variables: Vec<KeyValueInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PackageTransport {
    #[serde(rename = "type")]
    pub kind: TransportKind,
    /// Where a package that serves HTTP listens, `{var}` templates and all.
    pub url: Option<String>,
}

/// `registryType`: an open string in the registry's schema.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum RegistryType {
    Npm,
    Pypi,
    Oci,
    Other(String),
}

impl From<String> for RegistryType {
    fn from(s: String) -> Self {
        match s.as_str() {
            "npm" => RegistryType::Npm,
            "pypi" => RegistryType::Pypi,
            "oci" => RegistryType::Oci,
            _ => RegistryType::Other(s),
        }
    }
}

impl RegistryType {
    pub fn as_str(&self) -> &str {
        match self {
            RegistryType::Npm => "npm",
            RegistryType::Pypi => "pypi",
            RegistryType::Oci => "oci",
            RegistryType::Other(s) => s,
        }
    }
}

/// A package's or a remote's `type`, kept as given when fl does not know it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum TransportKind {
    Stdio,
    StreamableHttp,
    Sse,
    Other(String),
}

impl From<String> for TransportKind {
    fn from(s: String) -> Self {
        match s.as_str() {
            "stdio" => TransportKind::Stdio,
            "streamable-http" => TransportKind::StreamableHttp,
            "sse" => TransportKind::Sse,
            _ => TransportKind::Other(s),
        }
    }
}

impl TransportKind {
    pub fn as_str(&self) -> &str {
        match self {
            TransportKind::Stdio => "stdio",
            TransportKind::StreamableHttp => "streamable-http",
            TransportKind::Sse => "sse",
            TransportKind::Other(s) => s,
        }
    }
}

/// A runtime or package argument.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Argument {
    #[serde(rename = "type")]
    pub kind: ArgumentKind,
    /// A named argument's flag, leading dashes included.
    pub name: Option<String>,
    /// May hold `{var}` templates, resolved from `variables`.
    pub value: Option<String>,
    pub value_hint: Option<String>,
    pub default: Option<String>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_secret: bool,
    /// `string`, `number`, `boolean` or `filepath`.
    pub format: Option<String>,
    #[serde(default)]
    pub variables: BTreeMap<String, Input>,
}

/// An argument's `type`, kept as given when fl does not know it: the
/// registry serves types its own schema does not define.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum ArgumentKind {
    Positional,
    Named,
    Other(String),
}

impl From<String> for ArgumentKind {
    fn from(s: String) -> Self {
        match s.as_str() {
            "positional" => ArgumentKind::Positional,
            "named" => ArgumentKind::Named,
            _ => ArgumentKind::Other(s),
        }
    }
}

impl ArgumentKind {
    pub fn as_str(&self) -> &str {
        match self {
            ArgumentKind::Positional => "positional",
            ArgumentKind::Named => "named",
            ArgumentKind::Other(s) => s,
        }
    }
}

/// A `{var}` an argument, a header or a remote URL names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    pub value: Option<String>,
    pub default: Option<String>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_secret: bool,
    pub format: Option<String>,
}

/// An environment variable or a header.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValueInput {
    pub name: String,
    /// May hold `{var}` templates, resolved from `variables`.
    pub value: Option<String>,
    pub default: Option<String>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_secret: bool,
    #[serde(default)]
    pub variables: BTreeMap<String, Input>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Remote {
    #[serde(rename = "type")]
    pub kind: TransportKind,
    /// May hold `{var}` templates, resolved from `variables`.
    pub url: String,
    #[serde(default)]
    pub headers: Vec<KeyValueInput>,
    #[serde(default)]
    pub variables: BTreeMap<String, Input>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpError;
    use crate::fake::{self, FakeRegistry};

    fn client(fake: &FakeRegistry) -> Registry {
        Registry::new(&fake.url()).unwrap()
    }

    fn names(servers: &[Summary]) -> Vec<&str> {
        servers.iter().map(|s| s.name.as_str()).collect()
    }

    /// The phrase each failure is told by; a message holds its own and no
    /// other.
    const PHRASES: [&str; 7] = [
        "fl follows no redirect",
        "answered with an HTML page",
        "larger than 4 MiB",
        "has no server",
        "answered 500",
        "cannot reach the registry",
        "is not the registry API's shape",
    ];

    fn holds_only(msg: &str, phrase: &str) {
        assert!(msg.contains(phrase), "{phrase:?} not in: {msg}");
        for other in PHRASES.iter().filter(|p| **p != phrase) {
            assert!(!msg.contains(other), "{other:?} also in: {msg}");
        }
    }

    // MCP spec §3.1: the registry's own `search` parameter, a substring of
    // the name in any case, one entry per server.
    #[test]
    fn search_finds_names_in_any_case_at_their_latest_version() {
        let fake = FakeRegistry::start();
        let found = client(&fake).search("NoTeS").unwrap();
        assert_eq!(names(&found.servers), [fake::NOTES]);
        assert!(!found.stopped_early);
        let notes = &found.servers[0];
        assert_eq!(notes.version, "1.2.0");
        assert_eq!(notes.description, "The notes server, for fl's tests.");
        assert_eq!(notes.status, "active");
        let requests = fake.state().requests.clone();
        assert_eq!(
            requests,
            ["GET /v0.1/servers?search=NoTeS&version=latest&limit=100"]
        );
    }

    // A registry that ignores `search` sends every server; fl shows only
    // the names that hold the text, in any case.
    #[test]
    fn search_keeps_only_names_that_hold_the_text_whatever_the_registry_sends() {
        let fake = FakeRegistry::start();
        fake.add_server("io.example/MixedCase", "1.0.0");
        fake.state().ignores_search = true;
        let registry = client(&fake);
        let found = registry.search("NoTeS").unwrap();
        assert_eq!(names(&found.servers), [fake::NOTES]);
        let found = registry.search("mixedcase").unwrap();
        assert_eq!(names(&found.servers), ["io.example/MixedCase"]);
        let requests = fake.state().requests.clone();
        assert_eq!(
            requests,
            [
                "GET /v0.1/servers?search=NoTeS&version=latest&limit=100",
                "GET /v0.1/servers?search=mixedcase&version=latest&limit=100",
            ]
        );
    }

    #[test]
    fn search_reads_every_page_by_its_cursor() {
        let fake = FakeRegistry::start();
        fake.state().page_limit = 2;
        let found = client(&fake).search("io.example").unwrap();
        assert_eq!(names(&found.servers), fake::LISTED);
        assert!(!found.stopped_early);
        let requests = fake.state().requests.clone();
        assert_eq!(requests.len(), 4, "{requests:?}");
        assert!(
            requests[1].ends_with("&cursor=io.example%2Flegacy%3A0.1.0"),
            "{requests:?}"
        );
    }

    // MCP spec §3.1: up to 20 pages, and it says when it stopped early.
    #[test]
    fn search_stops_after_twenty_pages_and_says_so() {
        let fake = FakeRegistry::start();
        fake.state().page_limit = 1;
        for i in 0..25 {
            fake.add_server(&format!("io.example/many-{i:02}"), "1.0.0");
        }
        for i in 0..20 {
            fake.add_server(&format!("io.example/exact-{i:02}"), "1.0.0");
        }
        let found = client(&fake).search("many").unwrap();
        assert_eq!(found.servers.len(), 20);
        assert!(found.stopped_early, "five servers were left unread");
        assert_eq!(fake.state().requests.len(), 20);

        fake.state().requests.clear();
        let found = client(&fake).search("exact").unwrap();
        assert_eq!(found.servers.len(), 20);
        assert!(!found.stopped_early, "the twentieth page was the last");
        assert_eq!(fake.state().requests.len(), 20);
    }

    #[test]
    fn versions_lists_every_version_of_a_server() {
        let fake = FakeRegistry::start();
        let versions = client(&fake).versions(fake::NOTES).unwrap();
        let found: Vec<&str> = versions.iter().map(|v| v.server.version.as_str()).collect();
        assert_eq!(found, fake::NOTES_VERSIONS);
        let latest: Vec<bool> = versions.iter().map(|v| v.meta.is_latest).collect();
        assert_eq!(latest, [false, false, false, true]);
        let requests = fake.state().requests.clone();
        assert_eq!(
            requests,
            ["GET /v0.1/servers/io.example%2Fnotes/versions?include_deleted=true"]
        );
    }

    // The registry API (MCP spec §3.1): `latest` names the newest
    // version; a name and a version are each one encoded path segment.
    #[test]
    fn version_reads_one_version_or_the_latest() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);
        let latest = registry.version(fake::NOTES, "latest").unwrap();
        assert_eq!(latest.server.version, "1.2.0");
        let old = registry.version(fake::NOTES, "0.9.0+build.7").unwrap();
        assert_eq!(old.server.version, "0.9.0+build.7");
        assert!(!old.meta.is_latest);
        let requests = fake.state().requests.clone();
        assert_eq!(
            requests,
            [
                "GET /v0.1/servers/io.example%2Fnotes/versions/latest?include_deleted=true",
                concat!(
                    "GET /v0.1/servers/io.example%2Fnotes/versions/0.9.0%2Bbuild.7",
                    "?include_deleted=true"
                ),
            ]
        );
    }

    // Each field freezing reads (MCP spec §3.2), with the fields fl does not
    // read (`$schema`, `title`, `repository`, `publishedAt`, `choices`…)
    // ignored.
    #[test]
    fn the_models_carry_every_field_freezing_needs() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);

        let notes = registry.version(fake::NOTES, "latest").unwrap().server;
        assert_eq!(notes.description, "The notes server, for fl's tests.");
        assert!(notes.remotes.is_empty());
        let [npm] = notes.packages.as_slice() else {
            panic!("{:?}", notes.packages)
        };
        assert_eq!(npm.registry_type, RegistryType::Npm);
        assert_eq!(npm.identifier, "@example/notes-mcp");
        assert_eq!(npm.version.as_deref(), Some("1.2.0"));
        assert_eq!(npm.transport.kind, TransportKind::Stdio);
        assert_eq!(npm.transport.url, None);
        assert!(npm.runtime_arguments.is_empty());
        let [dir] = npm.package_arguments.as_slice() else {
            panic!("{:?}", npm.package_arguments)
        };
        assert_eq!(dir.kind, ArgumentKind::Positional);
        assert_eq!(dir.value_hint.as_deref(), Some("notes_dir"));
        assert_eq!(dir.default.as_deref(), Some("./notes"));
        assert!(dir.is_required && !dir.is_secret);
        let [token, log] = npm.environment_variables.as_slice() else {
            panic!("{:?}", npm.environment_variables)
        };
        assert_eq!(token.name, "NOTES_TOKEN");
        assert!(token.is_secret && token.is_required && token.value.is_none());
        assert_eq!(log.name, "NOTES_LOG");
        assert_eq!(log.default.as_deref(), Some("info"));
        assert!(!log.is_secret && !log.is_required);

        let weather = registry.version(fake::WEATHER, "latest").unwrap().server;
        assert_eq!(weather.packages[0].registry_type, RegistryType::Pypi);
        let units = &weather.packages[0].package_arguments[0];
        assert_eq!(units.kind, ArgumentKind::Named);
        assert_eq!(units.name.as_deref(), Some("--units"));
        assert_eq!(units.default.as_deref(), Some("metric"));

        let tracker = registry.version(fake::TRACKER, "latest").unwrap().server;
        let oci = &tracker.packages[0];
        assert_eq!(oci.registry_type, RegistryType::Oci);
        assert_eq!(oci.identifier, fake::TRACKER_IMAGE);
        assert_eq!(oci.version, None);
        let [port, token] = oci.runtime_arguments.as_slice() else {
            panic!("{:?}", oci.runtime_arguments)
        };
        assert_eq!(port.value.as_deref(), Some("TRACKER_PORT=8085"));
        assert!(port.variables.is_empty());
        assert_eq!(token.name.as_deref(), Some("-e"));
        assert_eq!(token.value.as_deref(), Some("TRACKER_TOKEN={token}"));
        assert_eq!(token.format, None);
        let var = &token.variables["token"];
        assert!(var.is_secret && var.is_required && var.value.is_none());
        assert_eq!(var.format.as_deref(), Some("string"));

        let docs = registry.version(fake::DOCS, "latest").unwrap().server;
        assert!(docs.packages.is_empty());
        let [remote] = docs.remotes.as_slice() else {
            panic!("{:?}", docs.remotes)
        };
        assert_eq!(remote.kind, TransportKind::StreamableHttp);
        assert_eq!(remote.url, fake::DOCS_URL);
        assert!(remote.variables.is_empty());
        let [auth] = remote.headers.as_slice() else {
            panic!("{:?}", remote.headers)
        };
        assert_eq!(auth.name, "Authorization");
        assert!(auth.is_secret && auth.value.is_none() && auth.variables.is_empty());

        let multi = registry.version(fake::MULTI, "latest").unwrap().server;
        let types: Vec<&str> = multi
            .packages
            .iter()
            .map(|p| p.registry_type.as_str())
            .collect();
        assert_eq!(types, ["npm", "oci"]);
        assert_eq!(multi.remotes[0].kind, TransportKind::Sse);
        assert_eq!(multi.remotes[0].kind.as_str(), "sse");
    }

    // The registry's type fields are open (any string): an unknown one is
    // kept, for freezing to refuse by name.
    #[test]
    fn an_unknown_package_or_transport_type_is_kept_by_name() {
        let text = r#"{"registryType": "mcpb", "identifier": "x", "transport":
                       {"type": "streamable-http", "url": "http://localhost:{port}/mcp"}}"#;
        let package: Package = serde_json::from_str(text).unwrap();
        assert_eq!(package.registry_type, RegistryType::Other("mcpb".into()));
        assert_eq!(package.registry_type.as_str(), "mcpb");
        assert_eq!(package.transport.kind, TransportKind::StreamableHttp);
        assert_eq!(package.transport.kind.as_str(), "streamable-http");
        assert_eq!(
            package.transport.url.as_deref(),
            Some("http://localhost:{port}/mcp")
        );
        let remote: Remote =
            serde_json::from_str(r#"{"type": "websocket", "url": "wss://x.example.com"}"#).unwrap();
        assert_eq!(remote.kind, TransportKind::Other("websocket".into()));
        assert_eq!(remote.kind.as_str(), "websocket");
        let arg: Argument =
            serde_json::from_str(r#"{"type": "flag", "name": "--verbose"}"#).unwrap();
        assert_eq!(arg.kind, ArgumentKind::Other("flag".into()));
        assert_eq!(arg.kind.as_str(), "flag");
        let named: Argument = serde_json::from_str(r#"{"type": "named", "name": "-v"}"#).unwrap();
        assert_eq!(named.kind.as_str(), "named");
    }

    // The registry serves entries its own schema does not allow: a search
    // reads only what it shows, so one such server never fails a page.
    #[test]
    fn a_server_that_breaks_the_schema_never_fails_a_search() {
        let fake = FakeRegistry::start();
        let weather = fake
            .state()
            .entries
            .iter()
            .find(|e| e["server"]["name"] == fake::WEATHER)
            .cloned();
        let mut broken = weather.unwrap();
        broken["server"]["name"] = "io.example/broken".into();
        broken["server"]["packages"][0]["transport"] = 7.into();
        broken["_meta"]["io.modelcontextprotocol.registry/official"]["status"] = "paused".into();
        fake.state().entries.push(broken);
        let registry = client(&fake);
        let found = registry.search("io.example").unwrap();
        let mut listed = vec!["io.example/broken"];
        listed.extend(fake::LISTED);
        listed.sort();
        assert_eq!(names(&found.servers), listed);
        let status = |name: &str| {
            let s = found.servers.iter().find(|s| s.name == name).unwrap();
            s.status.clone()
        };
        assert_eq!(status("io.example/broken"), "paused");
        assert_eq!(status(fake::LEGACY), "deprecated");
        // An argument type fl does not know is kept, for freezing to refuse.
        let verbose = registry.version(fake::VERBOSE, "latest").unwrap();
        let arg = &verbose.server.packages[0].package_arguments[0];
        assert_eq!(arg.kind, ArgumentKind::Other("flag".into()));
        // Anything else the full entry breaks is the registry's shape, as
        // before.
        let err = registry.version("io.example/broken", "latest").unwrap_err();
        holds_only(&err.to_string(), "is not the registry API's shape");
    }

    // The registry API: a deleted server is hidden unless asked for,
    // so fl asks, to refuse it by its status rather than as not found (MCP
    // spec §3.2 step 2).
    #[test]
    fn a_deleted_server_is_found_only_by_asking_for_it_and_says_so() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);
        let gone = registry.version(fake::GONE, "latest").unwrap();
        assert_eq!(gone.meta.status, Status::Deleted);
        assert_eq!(
            gone.meta.status_message.as_deref(),
            Some(fake::GONE_MESSAGE)
        );
        let all = registry.versions(fake::GONE).unwrap();
        assert_eq!(all[0].meta.status, Status::Deleted);
        let legacy = registry.version(fake::LEGACY, "latest").unwrap();
        assert_eq!(legacy.meta.status, Status::Deprecated);
        assert_eq!(
            legacy.meta.status_message.as_deref(),
            Some(fake::LEGACY_MESSAGE)
        );
        assert!(registry.search("gone").unwrap().servers.is_empty());

        // The fake hides it as the registry does, when not asked.
        let hidden = format!(
            "{}/v0.1/servers/io.example%2Fgone/versions/latest",
            fake.url()
        );
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build(),
        );
        assert_eq!(agent.get(&hidden).call().unwrap().status().as_u16(), 404);
    }

    // MCP spec §3.1 and §5: each misbehaviour is the registry's failure,
    // told apart by its own message; none is a parse error, and a redirect
    // is never followed.
    #[test]
    fn each_misbehaving_registry_is_refused_with_its_own_message() {
        let fake = FakeRegistry::start();
        let elsewhere = FakeRegistry::start();
        let registry = client(&fake);
        let fail = |f: fn(&mut fake::State)| {
            f(&mut fake.state());
            match registry.search("notes") {
                // Not the answer itself: the oversized one is 4.5 MiB.
                Ok(found) => panic!("read {} servers from it", found.servers.len()),
                Err(e) => e.to_string(),
            }
        };

        // Off the origin: another host, another port that begins alike, a
        // scheme-relative address. On it: a path, or the origin in full.
        let own = fake.url();
        let off = "a redirect off its own origin".to_string();
        for (location, told) in [
            (format!("{}/v0.1/servers", elsewhere.url()), off.clone()),
            (format!("{own}9/v0.1/servers"), off.clone()),
            ("//registry.example.com/v0.1/servers".to_string(), off),
            (
                "/v0.1/moved".to_string(),
                "a redirect to /v0.1/moved".to_string(),
            ),
            (
                format!("{own}/v0.1/moved"),
                format!("a redirect to {own}/v0.1/moved"),
            ),
        ] {
            fake.state().redirect_next = Some(location.clone());
            let msg = registry.search("notes").unwrap_err().to_string();
            holds_only(&msg, "fl follows no redirect");
            assert!(
                msg.contains(&format!("answered 302 with {told}")),
                "{location}: {msg}"
            );
            if told.contains("off its own origin") {
                assert!(!msg.contains(&location), "{msg}");
            }
        }
        assert!(
            elsewhere.state().requests.is_empty(),
            "a redirect was followed"
        );
        assert_eq!(fake.state().requests.len(), 5, "a redirect was followed");

        let msg = fail(|s| s.html_502_next = true);
        holds_only(&msg, "answered with an HTML page");
        assert!(msg.contains("(status 502)"), "{msg}");

        let msg = fail(|s| s.oversized_next = true);
        holds_only(&msg, "larger than 4 MiB");

        let msg = fail(|s| s.problem_500_next = true);
        holds_only(&msg, "answered 500");
        assert!(
            msg.contains("answered 500: Failed to get registry list"),
            "{msg}"
        );

        let err = registry
            .version("io.example/nowhere", "latest")
            .unwrap_err();
        assert!(matches!(err, McpError::NotFound { .. }), "{err:?}");
        let msg = err.to_string();
        holds_only(&msg, "has no server");
        assert!(msg.contains("has no server `io.example/nowhere`"), "{msg}");
        let msg = registry
            .versions("io.example/nowhere")
            .unwrap_err()
            .to_string();
        holds_only(&msg, "has no server");
        let msg = registry
            .version(fake::NOTES, "9.9.9")
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("has no version `9.9.9` of server `io.example/notes`"),
            "{msg}"
        );
        for p in PHRASES {
            assert!(!msg.contains(p), "{p:?} in: {msg}");
        }

        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let closed = Registry::new(&format!("http://127.0.0.1:{port}")).unwrap();
        let msg = closed.search("notes").unwrap_err().to_string();
        holds_only(&msg, "cannot reach the registry");

        // And a registry that behaves is read as one, after all of that.
        assert_eq!(
            names(&registry.search("notes").unwrap().servers),
            [fake::NOTES]
        );
    }

    // MCP spec §3.1: the cap counts the decoded body. A gzip answer a few KiB
    // long on the wire decodes to 32 MiB; ureq's own limit sits under its
    // decoder and would let it through.
    #[test]
    fn a_gzip_answer_that_decodes_past_the_cap_is_refused() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);
        fake.state().gzip_bomb_next = true;
        let msg = registry.search("notes").unwrap_err().to_string();
        holds_only(&msg, "larger than 4 MiB");
        assert_eq!(
            names(&registry.search("notes").unwrap().servers),
            [fake::NOTES]
        );
    }

    // MCP spec §3.1: the cap also counts the wire. A gzip answer of one tiny
    // member and a run of empty ones is 5 MiB on the wire and decodes to a
    // valid empty list, so only the wire limit can refuse it.
    #[test]
    fn an_answer_over_the_cap_on_the_wire_is_refused_whatever_it_decodes_to() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);
        fake.state().gzip_padded_next = true;
        let msg = registry.search("notes").unwrap_err().to_string();
        holds_only(&msg, "larger than 4 MiB");
    }

    /// A source of `left` zero bytes that counts what is pulled from it.
    struct Source {
        left: u64,
        pulled: u64,
    }

    impl Read for Source {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.left.min(usize::MAX as u64) as usize);
            buf[..n].fill(0);
            self.left -= n as u64;
            self.pulled += n as u64;
            Ok(n)
        }
    }

    // A body that decodes to 64 MiB never reaches memory: the read stops at the
    // cap plus the one byte that tells a longer body from one of exactly the
    // cap. `Take` hands its source a buffer no longer than what is left of that,
    // so the bound is exact; one 8 KiB read buffer is allowed on top, far below
    // the 64 MiB an unbounded read pulls.
    #[test]
    fn a_body_past_the_cap_is_refused_having_pulled_only_the_cap() {
        let mut source = Source {
            left: 64 << 20,
            pulled: 0,
        };
        assert_eq!(read_capped(&mut source, BODY_LIMIT).unwrap(), None);
        assert!(
            source.pulled <= BODY_LIMIT + 1 + 8192,
            "pulled {} bytes",
            source.pulled
        );
        let mut exact = Source {
            left: 100,
            pulled: 0,
        };
        assert_eq!(read_capped(&mut exact, 100).unwrap(), Some(vec![0; 100]));
        let mut over = Source {
            left: 101,
            pulled: 0,
        };
        assert_eq!(read_capped(&mut over, 100).unwrap(), None);
    }

    // The registry's data is read as it came: a byte that is not UTF-8 makes
    // the body not the registry API's, and is never replaced.
    #[test]
    fn a_body_that_is_not_utf8_is_not_the_registry_api() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);
        fake.state().invalid_utf8_next = true;
        let msg = registry.search("notes").unwrap_err().to_string();
        holds_only(&msg, "is not the registry API's shape");
    }

    // MCP spec §3.1: reads are unauthenticated.
    #[test]
    fn no_request_carries_a_credential_and_each_names_fl() {
        let fake = FakeRegistry::start();
        let registry = client(&fake);
        registry.search("io.example").unwrap();
        registry.versions(fake::NOTES).unwrap();
        registry.version(fake::DOCS, "latest").unwrap();
        let headers = fake.state().headers.clone();
        assert_eq!(headers.len(), 3);
        for h in headers {
            assert!(!h.iter().any(|(k, _)| k == "authorization"), "{h:?}");
            assert!(!h.iter().any(|(k, _)| k == "cookie"), "{h:?}");
            let agent = h
                .iter()
                .find(|(k, _)| k == "user-agent")
                .map(|(_, v)| v.as_str());
            assert_eq!(agent, Some(concat!("fl/", env!("CARGO_PKG_VERSION"))));
        }
    }

    // The registry API: names and versions are encoded in full, so
    // `/` and `+` reach the registry as `%2F` and `%2B`.
    #[test]
    fn a_path_segment_keeps_only_unreserved_characters() {
        assert_eq!(encode("io.example/notes"), "io.example%2Fnotes");
        assert_eq!(encode("1.0.0+build.7"), "1.0.0%2Bbuild.7");
        assert_eq!(encode("AZaz09-._~"), "AZaz09-._~");
        assert_eq!(encode("a b%c?d&e=f#g:h"), "a%20b%25c%3Fd%26e%3Df%23g%3Ah");
        assert_eq!(encode("é"), "%C3%A9");
        assert_eq!(encode(""), "");
    }

    // Registry text reaches a terminal: no escape sequence, and not a page of
    // it.
    #[test]
    fn registry_text_is_shown_without_control_characters_and_cut_short() {
        assert_eq!(printable("a\u{1b}[31mb\r\nc"), "a[31mbc");
        assert_eq!(printable(&"x".repeat(400)).len(), 300);
    }

    // Text from the registry is shown without anything that could make it
    // read as another: control, format and invisible characters go.
    #[test]
    fn printable_drops_bidirectional_and_invisible_characters_too() {
        assert_eq!(
            printable("a\u{202e}b\u{200b}c\u{feff}d\u{2066}e\u{ad}f\u{1b}g"),
            "abcdefg"
        );
        let unseen = [
            '\u{7f}',
            '\u{85}',
            '\u{ad}',
            '\u{600}',
            '\u{605}',
            '\u{61c}',
            '\u{6dd}',
            '\u{70f}',
            '\u{180e}',
            '\u{200b}',
            '\u{200f}',
            '\u{202a}',
            '\u{202e}',
            '\u{2060}',
            '\u{2064}',
            '\u{2066}',
            '\u{206f}',
            '\u{feff}',
            '\u{fff9}',
            '\u{fffb}',
            '\u{e0001}',
            '\u{e0020}',
            '\u{e007f}',
        ];
        for c in unseen {
            assert!(is_unseen(c), "U+{:04X}", c as u32);
        }
        let seen = [
            'a',
            ' ',
            '\u{e9}',
            '\u{ac}',
            '\u{ae}',
            '\u{5ff}',
            '\u{606}',
            '\u{61b}',
            '\u{61d}',
            '\u{6dc}',
            '\u{6de}',
            '\u{70e}',
            '\u{180d}',
            '\u{200a}',
            '\u{2010}',
            '\u{2029}',
            '\u{202f}',
            '\u{205f}',
            '\u{2065}',
            '\u{2070}',
            '\u{fefe}',
            '\u{fff8}',
            '\u{fffc}',
            '\u{e0000}',
            '\u{e001f}',
            '\u{e0080}',
        ];
        for c in seen {
            assert!(!is_unseen(c), "U+{:04X}", c as u32);
        }
    }

    // MCP spec §3.1: https, or http to this machine, with no user name.
    #[test]
    fn a_registry_address_fl_will_not_use_is_refused_at_construction() {
        for (url, clause) in [
            ("http://registry.example.com", "is not this machine"),
            ("http://10.0.0.1:8080", "is not this machine"),
            (
                "https://user@registry.example.com",
                "it carries a user name or password",
            ),
            (
                "ftp://registry.example.com",
                "it is neither https:// nor http://",
            ),
        ] {
            let err = Registry::new(url).err().expect(url);
            assert!(matches!(err, McpError::RegistryAddress { .. }), "{err:?}");
            let msg = err.to_string();
            assert!(
                msg.contains("is not a registry address fl will use"),
                "{msg}"
            );
            assert!(msg.contains(clause), "{url}: {msg}");
            assert!(msg.contains(url), "{msg}");
        }
        for url in [
            "http://localhost:8080",
            "http://[::1]:9",
            "https://registry.example.com",
        ] {
            assert_eq!(Registry::new(url).unwrap().url(), url);
        }
        let trimmed = Registry::new("https://registry.example.com/mirror/").unwrap();
        assert_eq!(trimmed.url(), "https://registry.example.com/mirror");
    }
}
