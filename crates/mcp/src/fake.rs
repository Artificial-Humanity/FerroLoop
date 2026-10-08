//! An in-process fake of an MCP registry's read API, `v0.1` (MCP spec §7.1).
//!
//! ⚠ It proves structure, not integration: it agrees with fl because both
//! were written from the same reading of the registry's OpenAPI document. The
//! live test reads the real registry.

use serde_json::{Value, json};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// npm; four versions ([`NOTES_VERSIONS`]), the last one latest; a secret
/// and a plain environment variable, a positional package argument.
pub const NOTES: &str = "io.example/notes";
/// Every version of [`NOTES`], oldest first. The first carries build
/// metadata, so its path segment holds an encoded `+`.
pub const NOTES_VERSIONS: [&str; 4] = ["0.9.0+build.7", "1.0.0", "1.1.0", "1.2.0"];
/// PyPI; a named package argument with a default, a secret variable.
pub const WEATHER: &str = "io.example/weather";
/// OCI, its identifier tagged ([`TRACKER_IMAGE`]); its runtime arguments
/// are `-e TRACKER_PORT=8085` and `-e TRACKER_TOKEN={token}`, `token` secret.
pub const TRACKER: &str = "io.example/tracker";
pub const TRACKER_IMAGE: &str = "ghcr.io/example/tracker-mcp:2.0.1";
/// A streamable-http remote whose `Authorization` header is secret and has
/// no value.
pub const DOCS: &str = "io.example/docs";
pub const DOCS_URL: &str = "https://docs.example.com/mcp";
/// Three launch routes: an npm package, an OCI package and an sse remote.
pub const MULTI: &str = "io.example/multi";
/// Deprecated, with [`LEGACY_MESSAGE`].
pub const LEGACY: &str = "io.example/legacy";
pub const LEGACY_MESSAGE: &str = "Replaced by io.example/notes.";
/// Deleted, with [`GONE_MESSAGE`]: hidden unless a request asks with
/// `include_deleted=true`, as the real registry hides it.
pub const GONE: &str = "io.example/gone";
pub const GONE_MESSAGE: &str = "Withdrawn by its publisher.";
/// npm, with a package argument of type `flag`, which the registry's schema
/// does not define and the registry serves.
pub const VERBOSE: &str = "io.example/verbose";
/// npm, two versions: [`UPGRADING_VERSIONS`]. The second, the latest, adds
/// a required variable that is not secret and has no value,
/// `UPGRADING_HOME`, and an optional secret, `UPGRADING_TOKEN`.
pub const UPGRADING: &str = "io.example/upgrading";
pub const UPGRADING_VERSIONS: [&str; 2] = ["1.0.0", "2.0.0"];
/// Every fixture a search shows by default, in name order: all but [`GONE`].
pub const LISTED: [&str; 8] = [
    DOCS, LEGACY, MULTI, NOTES, TRACKER, UPGRADING, VERBOSE, WEATHER,
];

const SCHEMA: &str = "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json";
const WHEN: &str = "2026-10-01T00:00:00Z";

#[derive(Debug, Default)]
pub struct State {
    /// Every request, as `GET <path and query>`, in order.
    pub requests: Vec<String>,
    /// Each request's headers, names lowercased, in the order of `requests`.
    pub headers: Vec<Vec<(String, String)>>,
    /// What the fake serves: one `ServerResponse` per server version, in
    /// the order they were published.
    pub entries: Vec<Value>,
    /// The most servers one page of `GET /v0.1/servers` holds, whatever
    /// `limit` asks. A setting; the registry's own maximum, 100, by default.
    pub page_limit: usize,
    /// The next request answers 302 with this `Location`. One-shot.
    pub redirect_next: Option<String>,
    /// The next request answers 502 with an HTML page, as a proxy in front
    /// of a registry does. One-shot.
    pub html_502_next: bool,
    /// The next request answers 200 with a valid list over 4 MiB. One-shot.
    pub oversized_next: bool,
    /// The next request answers 200 with a valid list, gzip-encoded: a few
    /// KiB on the wire, 32 MiB decoded. One-shot.
    pub gzip_bomb_next: bool,
    /// The next request answers 200, gzip-encoded, with 5 MiB on the wire: a
    /// valid empty list, then empty gzip members. One-shot.
    pub gzip_padded_next: bool,
    /// The next request answers 200 with a list whose one name holds the byte
    /// 0xFF, which is not UTF-8. One-shot.
    pub invalid_utf8_next: bool,
    /// The next request answers 500 with `application/problem+json`, as the
    /// real registry once did. One-shot.
    pub problem_500_next: bool,
    /// `GET /v0.1/servers` ignores `search` and lists every server, as a
    /// registry without the parameter would. A setting; off by default.
    pub ignores_search: bool,
}

pub struct FakeRegistry {
    server: Arc<tiny_http::Server>,
    thread: Option<JoinHandle<()>>,
    state: Arc<Mutex<State>>,
    url: String,
}

impl FakeRegistry {
    /// A registry on `127.0.0.1`, serving the fixtures above.
    pub fn start() -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind a local port"));
        let port = server.server_addr().to_ip().expect("an IP listener").port();
        let url = format!("http://127.0.0.1:{port}");
        let state = Arc::new(Mutex::new(State {
            entries: fixtures(),
            page_limit: 100,
            ..State::default()
        }));
        let (srv, st) = (Arc::clone(&server), Arc::clone(&state));
        let thread = std::thread::spawn(move || {
            while let Ok(req) = srv.recv() {
                let url = req.url().to_string();
                let headers = req
                    .headers()
                    .iter()
                    .map(|h| {
                        (
                            h.field.as_str().as_str().to_ascii_lowercase(),
                            h.value.as_str().to_string(),
                        )
                    })
                    .collect();
                let answer = {
                    let mut s = st.lock().unwrap();
                    s.requests.push(format!("{} {url}", req.method()));
                    s.headers.push(headers);
                    route(&mut s, &url)
                };
                let mut resp = tiny_http::Response::from_data(answer.body)
                    .with_status_code(answer.status)
                    .with_header(header("Content-Type", answer.content_type));
                if let Some(encoding) = answer.encoding {
                    resp = resp.with_header(header("Content-Encoding", encoding));
                }
                if let Some(location) = answer.location {
                    resp = resp.with_header(header("Location", &location));
                }
                let _ = req.respond(resp);
            }
        });
        Self {
            server,
            thread: Some(thread),
            state,
            url,
        }
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// ⚠ Locks the fake: take one guard and drop it before the next call to
    /// the registry, or the fake deadlocks on its own lock.
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Publishes `name` at `version`: an npm package, active, latest.
    pub fn add_server(&self, name: &str, version: &str) {
        let package = json!({
            "registryType": "npm",
            "identifier": format!("@example/{}", name.rsplit('/').next().unwrap_or(name)),
            "version": version,
            "transport": { "type": "stdio" }
        });
        let entry = entry(name, version, true, vec![package], vec![]);
        self.state().entries.push(entry);
    }
}

impl Drop for FakeRegistry {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn header(k: &str, v: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("an ASCII header")
}

struct Answer {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    location: Option<String>,
    /// The `Content-Encoding`, when the body is encoded.
    encoding: Option<&'static str>,
}

impl Answer {
    fn json(status: u16, body: Value) -> Self {
        Answer {
            status,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
            location: None,
            encoding: None,
        }
    }

    fn problem(status: u16, title: &str, detail: &str) -> Self {
        Answer {
            status,
            content_type: "application/problem+json",
            body: json!({ "title": title, "status": status, "detail": detail })
                .to_string()
                .into_bytes(),
            location: None,
            encoding: None,
        }
    }
}

fn route(s: &mut State, url: &str) -> Answer {
    if let Some(location) = s.redirect_next.take() {
        // A redirect as most servers send one: with a small HTML body.
        return Answer {
            status: 302,
            content_type: "text/html; charset=utf-8",
            body: format!("<a href=\"{location}\">Found</a>.").into_bytes(),
            location: Some(location),
            encoding: None,
        };
    }
    if std::mem::take(&mut s.html_502_next) {
        return Answer {
            status: 502,
            content_type: "text/html; charset=utf-8",
            body: b"<html><body><h1>502 Bad Gateway</h1></body></html>".to_vec(),
            location: None,
            encoding: None,
        };
    }
    if std::mem::take(&mut s.problem_500_next) {
        return Answer::problem(500, "Internal Server Error", "Failed to get registry list");
    }
    if std::mem::take(&mut s.oversized_next) {
        let mut big = entry(NOTES, "1.2.0", true, vec![], vec![]);
        big["server"]["description"] = Value::String("x".repeat(9 << 19));
        return Answer::json(200, json!({ "servers": [big], "metadata": { "count": 1 } }));
    }
    if std::mem::take(&mut s.gzip_bomb_next) {
        use std::io::Write as _;
        let mut big = entry(NOTES, "1.2.0", true, vec![], vec![]);
        big["server"]["description"] = Value::String("x".repeat(32 << 20));
        let list = json!({ "servers": [big], "metadata": { "count": 1 } }).to_string();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(list.as_bytes()).expect("compress in memory");
        return Answer {
            status: 200,
            content_type: "application/json",
            body: gz.finish().expect("compress in memory"),
            location: None,
            encoding: Some("gzip"),
        };
    }
    if std::mem::take(&mut s.gzip_padded_next) {
        use std::io::Write as _;
        let member = |data: &[u8]| {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            gz.write_all(data).expect("compress in memory");
            gz.finish().expect("compress in memory")
        };
        let mut body = member(br#"{"servers":[],"metadata":{"count":0}}"#);
        let empty = member(b"");
        while body.len() < 5 << 20 {
            body.extend_from_slice(&empty);
        }
        return Answer {
            status: 200,
            content_type: "application/json",
            body,
            location: None,
            encoding: Some("gzip"),
        };
    }
    if std::mem::take(&mut s.invalid_utf8_next) {
        let list = json!({
            "servers": [entry("io.example/notes-MARK", "1.0.0", true, vec![], vec![])],
            "metadata": { "count": 1 }
        });
        let mut body = list.to_string().into_bytes();
        let at = body
            .windows(4)
            .position(|w| w == b"MARK")
            .expect("the mark");
        body.splice(at..at + 4, [0xFF]);
        return Answer {
            status: 200,
            content_type: "application/json",
            body,
            location: None,
            encoding: None,
        };
    }
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let param = |key: &str| {
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (k == key).then(|| decode(v))
        })
    };
    let deleted_too = param("include_deleted").as_deref() == Some("true");
    let shown = |e: &&Value| deleted_too || official(e)["status"] != "deleted";
    let Some(rest) = path.strip_prefix("/v0.1/servers") else {
        return Answer::problem(404, "Not Found", "no such endpoint");
    };
    if rest.is_empty() {
        let search = (!s.ignores_search)
            .then(|| param("search").map(|t| t.to_lowercase()))
            .flatten();
        let version = param("version");
        let mut found: Vec<&Value> = s
            .entries
            .iter()
            .filter(shown)
            .filter(|e| match &search {
                Some(t) => name_of(e).to_lowercase().contains(t),
                None => true,
            })
            .filter(|e| match version.as_deref() {
                Some("latest") => official(e)["isLatest"] == true,
                Some(v) => e["server"]["version"] == v,
                None => true,
            })
            .collect();
        // Stable: versions of one server stay in the order they were published.
        found.sort_by_key(|e| name_of(e));
        let start = match param("cursor") {
            Some(c) => found
                .iter()
                .position(|e| cursor_of(e) == c)
                .map_or(found.len(), |i| i + 1),
            None => 0,
        };
        let limit = param("limit")
            .and_then(|l| l.parse::<usize>().ok())
            .unwrap_or(30)
            .min(s.page_limit);
        let page: Vec<Value> = found
            .iter()
            .skip(start)
            .take(limit)
            .map(|e| (*e).clone())
            .collect();
        let mut metadata = json!({ "count": page.len() });
        if start + page.len() < found.len()
            && let Some(last) = page.last()
        {
            metadata["nextCursor"] = Value::String(cursor_of(last));
        }
        return Answer::json(200, json!({ "servers": page, "metadata": metadata }));
    }
    // `/{name}/versions[/{version}]`: an unencoded `/` in the name splits it
    // into one more segment, and finds nothing — as on the real registry.
    let segments: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
    let (name, version) = match segments.as_slice() {
        [name, "versions"] => (decode(name), None),
        [name, "versions", version] => (decode(name), Some(decode(version))),
        _ => return Answer::problem(404, "Not Found", "Server not found"),
    };
    let versions: Vec<Value> = s
        .entries
        .iter()
        .filter(|e| name_of(e) == name)
        .filter(shown)
        .cloned()
        .collect();
    if versions.is_empty() {
        return Answer::problem(404, "Not Found", "Server not found");
    }
    match version.as_deref() {
        None => {
            let count = versions.len();
            Answer::json(
                200,
                json!({ "servers": versions, "metadata": { "count": count } }),
            )
        }
        Some(v) => {
            let hit = versions.into_iter().find(|e| match v {
                "latest" => official(e)["isLatest"] == true,
                v => e["server"]["version"] == v,
            });
            match hit {
                Some(e) => Answer::json(200, e),
                None => Answer::problem(404, "Not Found", "Server version not found"),
            }
        }
    }
}

fn official(entry: &Value) -> &Value {
    &entry["_meta"]["io.modelcontextprotocol.registry/official"]
}

fn name_of(entry: &Value) -> String {
    entry["server"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The cursor format the real registry was seen to use; fl treats it as
/// opaque.
fn cursor_of(entry: &Value) -> String {
    let version = entry["server"]["version"].as_str().unwrap_or_default();
    format!("{}:{version}", name_of(entry))
}

/// `%XX` → the byte; anything else as it is.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An active version of `name`.
fn entry(
    name: &str,
    version: &str,
    latest: bool,
    packages: Vec<Value>,
    remotes: Vec<Value>,
) -> Value {
    let official = json!({
        "status": "active",
        "statusChangedAt": WHEN,
        "publishedAt": WHEN,
        "updatedAt": WHEN,
        "isLatest": latest
    });
    let short = name.rsplit('/').next().unwrap_or(name);
    json!({
        "server": {
            "$schema": SCHEMA,
            "name": name,
            "title": short,
            "description": format!("The {short} server, for fl's tests."),
            "repository": { "url": format!("https://example.com/{short}"), "source": "github" },
            "version": version,
            "packages": packages,
            "remotes": remotes
        },
        "_meta": { "io.modelcontextprotocol.registry/official": official }
    })
}

fn with_status(mut entry: Value, status: &str, message: &str) -> Value {
    let official = &mut entry["_meta"]["io.modelcontextprotocol.registry/official"];
    official["status"] = Value::String(status.to_string());
    official["statusMessage"] = Value::String(message.to_string());
    entry
}

fn fixtures() -> Vec<Value> {
    let mut all = Vec::new();
    for (i, v) in NOTES_VERSIONS.iter().enumerate() {
        let package = json!({
            "registryType": "npm",
            "identifier": "@example/notes-mcp",
            "version": v,
            "runtimeHint": "npx",
            "transport": { "type": "stdio" },
            "packageArguments": [
                { "type": "positional", "valueHint": "notes_dir", "default": "./notes",
                  "isRequired": true, "description": "Where the notes live" }
            ],
            "environmentVariables": [
                { "name": "NOTES_TOKEN", "isSecret": true, "isRequired": true,
                  "description": "The notes service token" },
                { "name": "NOTES_LOG", "default": "info", "format": "string" }
            ]
        });
        let latest = i + 1 == NOTES_VERSIONS.len();
        all.push(entry(NOTES, v, latest, vec![package], vec![]));
    }
    let weather = json!({
        "registryType": "pypi",
        "identifier": "example-weather-mcp",
        "version": "0.4.1",
        "runtimeHint": "uvx",
        "transport": { "type": "stdio" },
        "packageArguments": [
            { "type": "named", "name": "--units", "default": "metric",
              "choices": ["metric", "imperial"] }
        ],
        "environmentVariables": [
            { "name": "WEATHER_API_KEY", "isSecret": true, "isRequired": true }
        ]
    });
    all.push(entry(WEATHER, "0.4.1", true, vec![weather], vec![]));
    let tracker = json!({
        "registryType": "oci",
        "identifier": TRACKER_IMAGE,
        "transport": { "type": "stdio" },
        "runtimeArguments": [
            { "type": "named", "name": "-e", "value": "TRACKER_PORT=8085" },
            { "type": "named", "name": "-e", "value": "TRACKER_TOKEN={token}",
              "variables": {
                  "token": { "format": "string", "isSecret": true, "isRequired": true }
              } }
        ]
    });
    all.push(entry(TRACKER, "2.0.1", true, vec![tracker], vec![]));
    let docs = json!({
        "type": "streamable-http",
        "url": DOCS_URL,
        "headers": [
            { "name": "Authorization", "isSecret": true,
              "description": "Authorization header with a token" }
        ]
    });
    all.push(entry(DOCS, "1.0.0", true, vec![], vec![docs]));
    let multi = vec![
        json!({ "registryType": "npm", "identifier": "@example/multi-mcp", "version": "3.0.0",
                "transport": { "type": "stdio" } }),
        json!({ "registryType": "oci", "identifier": "ghcr.io/example/multi-mcp:3.0.0",
                "transport": { "type": "stdio" } }),
    ];
    let multi_remote = json!({ "type": "sse", "url": "https://multi.example.com/sse" });
    all.push(entry(MULTI, "3.0.0", true, multi, vec![multi_remote]));
    let legacy = json!({ "registryType": "pypi", "identifier": "example-legacy-mcp",
                         "version": "0.1.0", "transport": { "type": "stdio" } });
    let legacy = entry(LEGACY, "0.1.0", true, vec![legacy], vec![]);
    all.push(with_status(legacy, "deprecated", LEGACY_MESSAGE));
    let verbose = json!({ "registryType": "npm", "identifier": "@example/verbose-mcp",
                          "version": "1.0.0", "transport": { "type": "stdio" },
                          "packageArguments": [ { "type": "flag", "name": "--verbose" } ] });
    all.push(entry(VERBOSE, "1.0.0", true, vec![verbose], vec![]));
    for (i, v) in UPGRADING_VERSIONS.iter().enumerate() {
        let latest = i + 1 == UPGRADING_VERSIONS.len();
        let variables = if latest {
            json!([
                { "name": "UPGRADING_HOME", "isRequired": true },
                { "name": "UPGRADING_TOKEN", "isSecret": true }
            ])
        } else {
            json!([])
        };
        let package = json!({ "registryType": "npm", "identifier": "@example/upgrading-mcp",
                              "version": v, "transport": { "type": "stdio" },
                              "environmentVariables": variables });
        all.push(entry(UPGRADING, v, latest, vec![package], vec![]));
    }
    let gone = json!({ "registryType": "npm", "identifier": "@example/gone-mcp",
                       "version": "1.0.0", "transport": { "type": "stdio" } });
    let gone = entry(GONE, "1.0.0", true, vec![gone], vec![]);
    all.push(with_status(gone, "deleted", GONE_MESSAGE));
    all
}
