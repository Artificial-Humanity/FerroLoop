//! The one test that reads the real public MCP registry (MCP spec §7.3).
//!
//! Every other test of this crate talks to the in-process fake registry; these
//! two are `#[ignore]`d so `cargo test` never touches the network. They check
//! that the client, the models and the freeze rules still fit what the
//! registry really sends:
//!
//! * `search("github")` finds a server, and every server it returns has the
//!   text in its name (the registry searches names only);
//! * the GitHub server's `latest` answers with a concrete version, a live
//!   status and a launch route, and freezing it by its remote route records
//!   that version and the `Authorization` header as a reference to an
//!   environment variable. Freezing it with no route chosen is refused, and
//!   the refusal lists the routes.
//!
//! Run them by hand:
//!
//! ```text
//! cargo test -p fl-mcp --test live -- --ignored --nocapture
//! ```
//!
//! No credential is sent and no file is written. The assertions name no exact
//! version and no count, so the registry's content may change under them; a
//! failure means the registry's shape or the GitHub server's routes changed.

use fl_mcp::McpError;
use fl_mcp::catalog::{HeaderValue, Transport};
use fl_mcp::freeze::{FreezeOptions, Route, freeze};
use fl_mcp::registry::{Registry, Status};

const REGISTRY: &str = "https://registry.modelcontextprotocol.io";
const GITHUB: &str = "io.github.github/github-mcp-server";

fn registry() -> Registry {
    Registry::new(REGISTRY).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
#[ignore = "live: reads the public MCP registry at registry.modelcontextprotocol.io"]
fn search_finds_github_servers_by_name() {
    let found = registry()
        .search("github")
        .unwrap_or_else(|e| panic!("{e}"));
    let names: Vec<&str> = found.servers.iter().map(|s| s.name.as_str()).collect();
    println!(
        "search(\"github\"): {} servers, e.g. {:?}",
        names.len(),
        &names[..names.len().min(3)]
    );
    assert!(
        !names.is_empty(),
        "search(\"github\") found no server: the search or its parsing broke"
    );
    for name in &names {
        assert!(
            name.to_lowercase().contains("github"),
            "`{name}` has no `github` in its name: the registry searches names only"
        );
    }
}

#[test]
#[ignore = "live: reads the public MCP registry at registry.modelcontextprotocol.io"]
fn github_server_latest_freezes_by_its_remote_route() {
    let resp = registry()
        .version(GITHUB, "latest")
        .unwrap_or_else(|e| panic!("{e}"));
    let version = resp.server.version.clone();
    println!(
        "{GITHUB}: version {version}, {} package(s), {} remote(s)",
        resp.server.packages.len(),
        resp.server.remotes.len()
    );

    // The registry resolves `latest`; a concrete version is what gets pinned.
    assert_eq!(resp.server.name, GITHUB);
    assert!(
        !version.is_empty() && version != "latest",
        "version is `{version}`, not a concrete version"
    );
    assert!(
        matches!(resp.meta.status, Status::Active | Status::Deprecated),
        "the GitHub server's latest version is {:?}",
        resp.meta.status
    );
    assert!(
        !resp.server.packages.is_empty() || !resp.server.remotes.is_empty(),
        "the GitHub server offers no package and no remote"
    );

    // By its remote route: the version the registry returned is the one
    // recorded, and the secret header is a reference, never a value.
    let mut opts = FreezeOptions {
        name: "github".to_string(),
        route: Some(Route::Remote),
        ..FreezeOptions::default()
    };
    let frozen = freeze(&resp, &opts).unwrap_or_else(|e| panic!("{e}"));
    let server = &frozen.server;
    assert_eq!(server.from.as_deref(), Some(GITHUB));
    assert_eq!(server.version.as_deref(), Some(version.as_str()));
    assert!(matches!(server.transport, Transport::Http | Transport::Sse));
    assert!(
        server
            .url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"))
    );
    let headers = server.headers.as_ref().expect("the remote's headers");
    let auth = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v)
        .expect("an Authorization header");
    let HeaderValue::Secret { env, .. } = auth else {
        panic!("the Authorization header is not a secret reference: {auth:?}");
    };
    assert!(!env.is_empty(), "the secret names no environment variable");
    assert!(
        frozen.secrets.contains(env),
        "{env} is not among the variables a person sets: {:?}",
        frozen.secrets
    );
    assert!(
        server.literal_values().is_empty(),
        "a literal value was frozen: {:?}",
        server.literal_values()
    );

    // With no route chosen, an entry that offers several is refused, and the
    // refusal lists them.
    opts.route = None;
    let err = freeze(&resp, &opts).expect_err("two routes and none chosen");
    let McpError::Unfreezable { problem, next, .. } = &err else {
        panic!("not the freeze refusal: {err}");
    };
    println!("refused as expected: {err}");
    assert!(
        problem.contains("more than one launch route"),
        "the refusal does not say there are several routes: {problem}"
    );
    assert!(
        problem.contains("--package oci"),
        "no OCI route listed: {problem}"
    );
    assert!(
        problem.contains("--remote"),
        "no remote route listed: {problem}"
    );
    assert!(
        next.contains("--package") && next.contains("--remote"),
        "{next}"
    );
}
