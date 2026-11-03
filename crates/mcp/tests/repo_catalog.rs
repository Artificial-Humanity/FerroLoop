//! This repository's own committed catalog, `.fl/mcp.toml` (docs/mcp.md): it parses and
//! validates through the library, names exactly the workspace's two MCP servers with literal
//! launches (no secret reference, no env), and is canonical, i.e. the text the library's own
//! editor writes for those servers and the text it reads back unchanged.

use std::path::{Path, PathBuf};

use fl_mcp::catalog::{Catalog, Editor, Server, Transport};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn committed() -> (PathBuf, String) {
    let path = Catalog::path(&repo_root());
    let text = std::fs::read_to_string(&path).expect("the repo commits .fl/mcp.toml");
    (path, text)
}

fn stdio(command: &str, args: &[&str]) -> Server {
    Server {
        from: None,
        version: None,
        enabled: true,
        vendors: None,
        transport: Transport::Stdio,
        command: Some(command.into()),
        args: (!args.is_empty()).then(|| args.iter().map(|a| a.to_string()).collect()),
        env: None,
        url: None,
        headers: None,
    }
}

/// The servers the workspace's agents use: FerroWire's stdio MCP server (`fl wire mcp`) and the
/// research-papers server installed at `/usr/local/bin/research-papers-mcp`.
fn expected() -> [(&'static str, Server); 2] {
    [
        ("ferrowire", stdio("fl", &["wire", "mcp"])),
        (
            "research-papers",
            stdio("/usr/local/bin/research-papers-mcp", &[]),
        ),
    ]
}

#[test]
fn the_committed_catalog_parses_and_validates() {
    let (path, text) = committed();
    let catalog = Catalog::parse(&text, &path).expect("the committed catalog is valid");
    assert!(catalog.registry.is_none(), "no registry is needed to sync");
    let want = expected();
    let names: Vec<&str> = catalog.servers.keys().map(String::as_str).collect();
    let mut want_names: Vec<&str> = want.iter().map(|(n, _)| *n).collect();
    want_names.sort_unstable();
    assert_eq!(names, want_names);
    for (name, server) in &want {
        assert_eq!(&catalog.servers[*name], server, "server {name}");
    }
}

#[test]
fn the_committed_catalog_carries_no_secret_and_no_value() {
    let (path, text) = committed();
    let catalog = Catalog::parse(&text, &path).unwrap();
    for (name, server) in &catalog.servers {
        assert!(server.env.is_none(), "{name}: no env");
        assert!(server.headers.is_none(), "{name}: no headers");
        assert!(server.secret_vars().is_empty(), "{name}: no secret");
        assert!(server.literal_values().is_empty(), "{name}: no literal");
    }
}

#[test]
fn the_committed_catalog_is_what_the_librarys_editor_writes() {
    let (_, text) = committed();
    let dir = tempfile::tempdir().unwrap();
    let mut editor = Editor::open(dir.path()).unwrap();
    for (name, server) in expected() {
        editor.add(name, &server).unwrap();
    }
    assert_eq!(
        editor.text(),
        text,
        "regenerate .fl/mcp.toml with the editor's text"
    );
}

#[test]
fn the_committed_catalog_reads_back_unchanged() {
    let (_, text) = committed();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".fl")).unwrap();
    std::fs::write(Catalog::path(dir.path()), &text).unwrap();
    let editor = Editor::open(dir.path()).unwrap();
    assert_eq!(editor.text(), text);
}
