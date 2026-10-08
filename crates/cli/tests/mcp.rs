//! `fl mcp`, driven as a black box (MCP spec §1.2): a git repository in a
//! private home, and the in-process fake registry. No test reaches the
//! network or the real home: `HOME` and every `XDG_*` base point into one
//! temporary directory, and `CODEX_HOME` and `FL_DB` are unset.

use assert_cmd::Command;
use fl_mcp::fake::{self, FakeRegistry};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Sys;

fn git(dir: &Path, args: &[&str]) {
    let out = Sys::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A git repository at `dir` with one commit holding `files`.
fn repo_at(dir: &Path, files: &[(&str, &str)]) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "t"]);
    for (rel, text) in files {
        fs::write(dir.join(rel), text).unwrap();
    }
    fs::write(dir.join("README.md"), "widgets\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "first"]);
}

/// The `.gitignore` lines a project that uses `fl mcp` commits.
const IGNORED: &str = "/.mcp.json\n/.codex/config.toml\n/.agents/mcp_config.json\n";

const CLAUDE: &str = ".mcp.json";
const CODEX: &str = ".codex/config.toml";
const AGY: &str = ".agents/mcp_config.json";
const CATALOG: &str = ".fl/mcp.toml";

/// One machine: a temporary home holding the repository `app`, and the fake
/// registry.
struct World {
    home: tempfile::TempDir,
    fake: Option<FakeRegistry>,
}

impl World {
    /// `app` ignores the three vendor files.
    fn new() -> World {
        World::ignoring(IGNORED)
    }

    fn ignoring(gitignore: &str) -> World {
        let home = tempfile::tempdir().unwrap();
        repo_at(&home.path().join("app"), &[(".gitignore", gitignore)]);
        World {
            home,
            fake: Some(FakeRegistry::start()),
        }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn app(&self) -> PathBuf {
        self.home().join("app")
    }

    fn fake(&self) -> &FakeRegistry {
        self.fake.as_ref().expect("the fake is running")
    }

    /// `fl` in `cwd`, with every base directory in the temporary home.
    fn fl_in(&self, cwd: &Path) -> Command {
        let h = self.home();
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", h)
            .env("XDG_CONFIG_HOME", h.join("config"))
            .env("XDG_STATE_HOME", h.join("state"))
            .env("XDG_DATA_HOME", h.join("data"))
            .env_remove("CODEX_HOME")
            .env_remove("FL_DB")
            .current_dir(cwd);
        c
    }

    /// `fl mcp <args>` in `cwd`: its exit code, stdout and stderr.
    fn run_in(&self, cwd: &Path, args: &[&str]) -> (i32, String, String) {
        let out = self.fl_in(cwd).arg("mcp").args(args).output().unwrap();
        (
            out.status.code().expect("an exit code"),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        self.run_in(&self.app(), args)
    }

    /// `fl mcp <args>` in `app`, which must succeed: stdout and stderr.
    fn ok(&self, args: &[&str]) -> (String, String) {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 0, "fl mcp {args:?}\nstdout:\n{out}\nstderr:\n{err}");
        (out, err)
    }

    /// `fl mcp <args>` in `app`, which must exit 2: stdout and stderr.
    fn refused(&self, args: &[&str]) -> (String, String) {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 2, "fl mcp {args:?}\nstdout:\n{out}\nstderr:\n{err}");
        (out, err)
    }

    /// The catalog names the fake as its registry.
    fn with_registry(&self) {
        let url = self.fake().url();
        self.ok(&["registry", &url]);
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.app().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn exists(&self, rel: &str) -> bool {
        self.app().join(rel).exists()
    }

    fn write_config(&self, text: &str) {
        let dir = self.home().join("config").join("fl");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), text).unwrap();
    }

    /// `~/.codex/config.toml` in the temporary home.
    fn write_codex(&self, text: &str) {
        let dir = self.home().join(".codex");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), text).unwrap();
    }

    fn requests(&self) -> Vec<String> {
        self.fake().state().requests.clone()
    }
}

/// `[projects."<dir>"] trust_level = "trusted"` for Codex.
fn trusted(dir: &Path) -> String {
    let key = dir.canonicalize().unwrap();
    format!(
        "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
        key.display()
    )
}

#[test]
fn registry_and_add_from_it_then_sync_write_every_vendor_file() {
    let w = World::new();
    let (out, _) = w.ok(&["registry", &w.fake().url()]);
    assert!(out.contains("the catalog reads servers from"), "{out}");
    assert!(
        w.read(CATALOG)
            .contains(&format!("registry = \"{}\"", w.fake().url()))
    );

    // npm: the only route on offer.
    let (out, err) = w.ok(&["add", "notes", "--from", fake::NOTES]);
    assert!(out.contains("added `notes` to"), "{out}");
    // The launch it recorded.
    assert!(
        out.contains("\n  stdio: npx -y @example/notes-mcp@1.2.0 ./notes\n"),
        "{out}"
    );
    assert!(
        err.contains(
            "warning: `env.NOTES_LOG` is a literal value: it will be committed with the catalog"
        ),
        "{err}"
    );
    assert!(
        out.contains("set NOTES_TOKEN in the environment the agent CLI starts in"),
        "{out}"
    );
    let catalog = w.read(CATALOG);
    assert!(catalog.contains("from = \"io.example/notes\""), "{catalog}");
    assert!(catalog.contains("version = \"1.2.0\""), "{catalog}");
    // OCI, with its optional secret named.
    let (out, err) = w.ok(&[
        "add",
        "tracker",
        "--from",
        fake::TRACKER,
        "--with",
        "TRACKER_TOKEN",
    ]);
    assert!(out.contains("set TRACKER_TOKEN"), "{out}");
    assert!(!err.contains("note:"), "{err}");
    // OCI, and a remote, each out of three routes.
    w.ok(&["add", "multi", "--from", fake::MULTI, "--package", "oci"]);
    let (out, _) = w.ok(&["add", "events", "--from", fake::MULTI, "--remote"]);
    assert!(
        out.contains("\n  sse: https://multi.example.com/sse\n"),
        "{out}"
    );
    w.ok(&[
        "add",
        "multi-npm",
        "--from",
        fake::MULTI,
        "--package",
        "npm",
    ]);
    // The only route, named.
    w.ok(&[
        "add",
        "weather",
        "--from",
        fake::WEATHER,
        "--package",
        "pypi",
    ]);
    // A remote with a secret header.
    let (out, _) = w.ok(&["add", "docs", "--from", fake::DOCS]);
    assert!(out.contains("set DOCS_AUTHORIZATION"), "{out}");
    let catalog = w.read(CATALOG);
    assert!(
        catalog.contains("\"ghcr.io/example/multi-mcp:3.0.0\""),
        "{catalog}"
    );
    assert!(
        catalog.contains("transport = \"sse\"\nurl = \"https://multi.example.com/sse\""),
        "{catalog}"
    );
    assert!(
        catalog.contains("args = [\"-y\", \"@example/multi-mcp@3.0.0\"]"),
        "{catalog}"
    );
    assert!(
        catalog.contains("headers.Authorization = { secret = true, env = \"DOCS_AUTHORIZATION\" }"),
        "{catalog}"
    );

    let (out, _) = w.ok(&["sync"]);
    assert!(out.contains("add notes"), "{out}");
    // Antigravity cannot send a secret header; the others still get `docs`.
    assert!(
        out.contains("Antigravity cannot run server `docs`"),
        "{out}"
    );
    let claude = w.read(CLAUDE);
    for name in ["notes", "tracker", "multi", "docs"] {
        assert!(
            claude.contains(&format!("\"{name}\": {{")),
            "{name}: {claude}"
        );
    }
    assert!(
        claude.contains("\"NOTES_TOKEN\": \"${NOTES_TOKEN}\""),
        "{claude}"
    );
    assert!(claude.contains(fake::TRACKER_IMAGE), "{claude}");
    assert!(claude.contains("\"${DOCS_AUTHORIZATION}\""), "{claude}");
    let codex = w.read(CODEX);
    for name in ["notes", "tracker", "multi", "docs"] {
        assert!(
            codex.contains(&format!("[mcp_servers.{name}]")),
            "{name}: {codex}"
        );
    }
    assert!(
        codex.contains("env_http_headers = { Authorization = \"DOCS_AUTHORIZATION\" }"),
        "{codex}"
    );
    let agy = w.read(AGY);
    for name in ["notes", "tracker", "multi"] {
        assert!(agy.contains(&format!("\"{name}\": {{")), "{name}: {agy}");
    }
    assert!(!agy.contains("\"docs\""), "{agy}");
}

#[test]
fn check_exits_0_when_synced_1_after_a_catalog_change_and_2_on_a_refusal() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    let (code, out, _) = w.run(&["check"]);
    assert_eq!(code, 1, "never synced: {out}");
    w.ok(&["sync"]);
    let (code, out, _) = w.run(&["check"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(".mcp.json: no change"), "{out}");

    w.ok(&["disable", "notes"]);
    let before = w.read(CLAUDE);
    let (code, out, _) = w.run(&["check"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("  remove notes"), "{out}");
    assert_eq!(w.read(CLAUDE), before, "check writes nothing");

    w.ok(&["enable", "notes"]);
    let edited = w.read(CLAUDE).replace("notes-mcp@1.2.0", "notes-mcp@9.9.9");
    fs::write(w.app().join(CLAUDE), &edited).unwrap();
    let (code, out, _) = w.run(&["check"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("was changed by hand since fl wrote it"),
        "{out}"
    );
    assert_eq!(w.read(CLAUDE), edited, "check writes nothing");
    let (out, err) = w.refused(&["sync"]);
    assert!(
        out.contains("was changed by hand since fl wrote it"),
        "{out}"
    );
    assert!(
        err.contains("error: nothing was written; each refusal above names its remedy"),
        "{err}"
    );
    assert_eq!(w.read(CLAUDE), edited);
}

#[test]
fn disable_then_sync_removes_the_server_from_every_file_and_enable_brings_it_back() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    w.ok(&["add", "weather", "--from", fake::WEATHER]);
    w.ok(&["sync"]);
    let (out, _) = w.ok(&["disable", "notes"]);
    assert!(out.contains("`notes` is now off for the team"), "{out}");
    assert!(w.read(CATALOG).contains("enabled = false"));
    w.ok(&["sync"]);
    for rel in [CLAUDE, CODEX, AGY] {
        let text = w.read(rel);
        assert!(!text.contains("notes"), "{rel}: {text}");
        assert!(text.contains("weather"), "{rel}: {text}");
    }
    let (out, _) = w.ok(&["enable", "notes"]);
    assert!(out.contains("`notes` is now on for the team"), "{out}");
    w.ok(&["sync"]);
    for rel in [CLAUDE, CODEX, AGY] {
        assert!(w.read(rel).contains("notes"), "{rel}");
    }
}

#[test]
fn a_machine_switch_turns_a_server_off_on_this_machine_only() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    w.ok(&["add", "weather", "--from", fake::WEATHER]);
    w.ok(&["sync"]);
    let catalog = w.read(CATALOG);
    w.write_config(&format!(
        "[[mcp]]\nroot = \"{}\"\ndisable = [\"notes\"]\n",
        w.app().display()
    ));
    let (out, _) = w.ok(&["sync"]);
    assert!(out.contains("  remove notes"), "{out}");
    for rel in [CLAUDE, CODEX, AGY] {
        let text = w.read(rel);
        assert!(!text.contains("notes"), "{rel}: {text}");
        assert!(text.contains("weather"), "{rel}: {text}");
    }
    assert_eq!(w.read(CATALOG), catalog, "the team default is unchanged");
    // A switch naming a server the catalog lacks is a warning, and the rest
    // goes on: `notes` is on here again.
    w.write_config(&format!(
        "[[mcp]]\nroot = \"{}\"\ndisable = [\"gone\"]\n",
        w.app().display()
    ));
    let (out, err) = w.ok(&["sync"]);
    assert!(
        err.contains(
            "warning: the `[[mcp]]` entry for this project in your fl config has `gone` in \
             `disable`"
        ),
        "{err}"
    );
    assert!(err.contains("has no such server; fl ignores it"), "{err}");
    assert!(out.contains("  add notes"), "{out}");
    assert!(w.read(CLAUDE).contains("notes"));
    let (code, _, err) = w.run(&["check"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("fl ignores it"), "{err}");
    w.write_config("");
    let (_, err) = w.ok(&["sync"]);
    assert!(!err.contains("fl ignores it"), "{err}");
}

#[test]
fn remove_takes_the_server_out_of_the_catalog_and_sync_out_of_every_file() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    w.ok(&["add", "weather", "--from", fake::WEATHER]);
    w.ok(&["sync"]);
    let (out, _) = w.ok(&["remove", "notes"]);
    assert!(out.contains("removed `notes` from"), "{out}");
    assert!(!w.read(CATALOG).contains("[server.notes]"));
    let (code, _, _) = w.run(&["check"]);
    assert_eq!(code, 1, "remove does not sync");
    w.ok(&["sync"]);
    for rel in [CLAUDE, CODEX, AGY] {
        assert!(!w.read(rel).contains("notes"), "{rel}");
    }
    let (_, err) = w.refused(&["remove", "notes"]);
    assert!(
        err.contains("there is no server `notes` in the catalog"),
        "{err}"
    );
}

#[test]
fn upgrade_shows_the_difference_and_rewrites_only_that_entry_without_syncing() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES, "--version", "1.1.0"]);
    w.ok(&["add", "weather", "--from", fake::WEATHER]);
    w.ok(&["sync"]);
    // A person's own choices survive the upgrade.
    w.ok(&["disable", "notes"]);
    let catalog = w.read(CATALOG).replacen(
        "enabled = false\n",
        "enabled = false\nvendors = [\"claude\"]\n",
        1,
    );
    fs::write(w.app().join(CATALOG), &catalog).unwrap();
    let weather = &catalog[catalog.find("[server.weather]").unwrap()..];
    let claude = w.read(CLAUDE);

    let (out, _) = w.ok(&["upgrade", "notes"]);
    assert!(out.contains("upgrade `notes`:"), "{out}");
    assert!(out.contains("  version: \"1.1.0\" -> \"1.2.0\""), "{out}");
    assert!(out.contains("@example/notes-mcp@1.2.0"), "{out}");
    assert!(out.contains("Review and commit it"), "{out}");
    let now = w.read(CATALOG);
    assert!(now.contains("version = \"1.2.0\""), "{now}");
    assert!(
        now.contains("enabled = false\nvendors = [\"claude\"]\n"),
        "{now}"
    );
    assert!(now.ends_with(weather), "only `notes` is rewritten:\n{now}");
    assert_eq!(w.read(CLAUDE), claude, "upgrade does not sync");
    let (code, _, _) = w.run(&["check"]);
    assert_eq!(code, 1);

    let (_, err) = w.refused(&["upgrade", "notes"]);
    assert!(err.contains("is not newer"), "{err}");
    // `--to` moves it anyway, even back.
    let (out, _) = w.ok(&["upgrade", "notes", "--to", "1.1.0"]);
    assert!(out.contains("  version: \"1.2.0\" -> \"1.1.0\""), "{out}");
    let pinned = w.read(CATALOG);
    let (out, _) = w.ok(&["upgrade", "notes", "--to", "1.1.0"]);
    assert!(out.contains("nothing to change"), "{out}");
    assert_eq!(w.read(CATALOG), pinned);
    w.ok(&["add", "local", "--", "node", "server.js"]);
    let (_, err) = w.refused(&["upgrade", "local"]);
    assert!(err.contains("was added by hand"), "{err}");
}

#[test]
fn add_by_hand_records_a_command_and_a_remote_with_secret_headers_by_reference() {
    let w = World::new();
    w.with_registry();
    let (out, err) = w.ok(&[
        "add",
        "local",
        "--env",
        "LOG=debug",
        "--env",
        "API_TOKEN",
        "--",
        "node",
        "server.js",
        "--port",
        "1",
    ]);
    assert!(out.contains("set API_TOKEN"), "{out}");
    assert!(
        out.contains("\n  stdio: node server.js --port 1\n"),
        "{out}"
    );
    assert!(!out.contains("debug"), "a value is never shown: {out}");
    // An argument with a space, an empty one, and one with a control
    // character are shown so they read as one each.
    let (out, _) = w.ok(&[
        "add",
        "spaced",
        "--",
        "node",
        "my server.js",
        "",
        "a\u{1b}b",
    ]);
    assert!(
        out.contains("\n  stdio: node \"my server.js\" \"\" ab\n"),
        "{out}"
    );
    assert!(
        err.contains("warning: `env.LOG` is a literal value: it will be committed"),
        "{err}"
    );
    let catalog = w.read(CATALOG);
    for line in [
        "command = \"node\"",
        "args = [\"server.js\", \"--port\", \"1\"]",
        "env.API_TOKEN = { secret = true }",
        "env.LOG = \"debug\"",
    ] {
        assert!(catalog.contains(line), "{line}: {catalog}");
    }

    let (out, _) = w.ok(&[
        "add",
        "docs",
        "--url",
        fake::DOCS_URL,
        "--header",
        "Authorization=DOCS_TOKEN:Bearer",
        "--header",
        "X-Team",
    ]);
    assert!(out.contains("set DOCS_TOKEN, DOCS_X_TEAM"), "{out}");
    assert!(
        out.contains(&format!(
            "\n  http: {}, headers Authorization, X-Team\n",
            fake::DOCS_URL
        )),
        "{out}"
    );
    let catalog = w.read(CATALOG);
    for line in [
        "transport = \"http\"",
        "headers.Authorization = { secret = true, env = \"DOCS_TOKEN\", scheme = \"Bearer\" }",
        "headers.X-Team = { secret = true, env = \"DOCS_X_TEAM\" }",
    ] {
        assert!(catalog.contains(line), "{line}: {catalog}");
    }
    w.ok(&["sync"]);
    let claude = w.read(CLAUDE);
    assert!(
        claude.contains("\"Authorization\": \"Bearer ${DOCS_TOKEN}\""),
        "{claude}"
    );
    assert!(
        claude.contains("\"API_TOKEN\": \"${API_TOKEN}\""),
        "{claude}"
    );
    assert_eq!(w.requests().len(), 0, "nothing by hand reads the registry");

    // A value where a variable's name goes is refused, and never repeated.
    for given in [
        "Authorization=Bearer ghp_example0token",
        "Authorization=ghp_example0token",
        "Authorization=GHP_EXAMPLE0TOKEN:Bearer abc",
        "Bearer ghp_example0token",
    ] {
        let (_, err) = w.refused(&["add", "leak", "--url", fake::DOCS_URL, "--header", given]);
        assert!(
            err.contains("never a value") || err.contains("starts with a header name"),
            "{given}: {err}"
        );
        assert!(!err.to_lowercase().contains("example0token"), "{err}");
        assert!(!err.contains("abc"), "{err}");
    }
    // `--env NAME` is a secret's variable: a token pasted there is refused,
    // and never repeated.
    for given in ["ghp_example0token", "sk live example0token", "Api_Token"] {
        let (_, err) = w.refused(&["add", "leak", "--env", given, "--", "node"]);
        assert!(
            err.contains("`--env` with no `=` names a secret's environment variable"),
            "{given}: {err}"
        );
        assert!(!err.to_lowercase().contains("example0token"), "{err}");
        assert!(!err.contains("Api_Token"), "{err}");
    }
    assert!(!w.read(CATALOG).contains("leak"));
}

// `upgrade` takes `--env` and `--with` for what the new version needs, so
// the remedies its own refusals name work.
#[test]
fn upgrade_takes_env_and_with_for_what_the_new_version_needs() {
    let w = World::new();
    w.with_registry();
    w.ok(&[
        "add",
        "home",
        "--from",
        fake::UPGRADING,
        "--version",
        "1.0.0",
    ]);
    w.ok(&[
        "add",
        "full",
        "--from",
        fake::UPGRADING,
        "--version",
        "1.0.0",
    ]);
    let (_, err) = w.refused(&["upgrade", "home"]);
    assert!(
        err.contains("its environment variable `UPGRADING_HOME` needs a value"),
        "{err}"
    );
    assert!(
        err.contains("Give it with `--env UPGRADING_HOME=<value>`"),
        "{err}"
    );
    let (out, err) = w.ok(&["upgrade", "home", "--env", "UPGRADING_HOME=/srv/notes"]);
    assert!(out.contains("  version: \"1.0.0\" -> \"2.0.0\""), "{out}");
    assert!(
        err.contains("`--with UPGRADING_TOKEN` includes it"),
        "{err}"
    );
    let (out, err) = w.ok(&[
        "upgrade",
        "full",
        "--env",
        "UPGRADING_HOME=/srv/notes",
        "--with",
        "UPGRADING_TOKEN",
    ]);
    assert!(out.contains("upgrade `full`:"), "{out}");
    assert!(!err.contains("note:"), "{err}");
    let catalog = w.read(CATALOG);
    let full = catalog.find("[server.full]").unwrap();
    let home = catalog.find("[server.home]").unwrap();
    let (full, home) = if full < home {
        (&catalog[full..home], &catalog[home..])
    } else {
        (&catalog[full..], &catalog[home..full])
    };
    for entry in [full, home] {
        assert!(
            entry.contains("env.UPGRADING_HOME = \"/srv/notes\""),
            "{entry}"
        );
    }
    assert!(
        full.contains("env.UPGRADING_TOKEN = { secret = true }"),
        "{full}"
    );
    assert!(!home.contains("UPGRADING_TOKEN"), "{home}");
    let (_, err) = w.refused(&[
        "upgrade",
        "full",
        "--to",
        "2.0.0",
        "--env",
        "UPGRADING_HOME",
    ]);
    assert!(
        err.contains("`--env UPGRADING_HOME` needs `=<value>` with `fl mcp upgrade`"),
        "{err}"
    );
}

#[test]
fn sync_and_check_make_no_request_and_need_no_registry() {
    let mut w = World::new();
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    assert!(!w.requests().is_empty(), "add reads the registry");
    w.fake().state().requests.clear();
    w.ok(&["sync"]);
    let (code, _, _) = w.run(&["check"]);
    assert_eq!(code, 0);
    assert_eq!(w.requests(), Vec::<String>::new());

    w.fake.take();
    w.ok(&["disable", "notes"]);
    w.ok(&["sync"]);
    assert!(!w.read(CLAUDE).contains("notes"));
    let (code, _, err) = w.run(&["check"]);
    assert_eq!(code, 0, "{err}");
    // `add --from` needs the registry, and says so; a name already taken
    // is refused before the registry is read.
    let (_, err) = w.refused(&["add", "weather", "--from", fake::WEATHER]);
    assert!(err.contains("cannot reach the registry"), "{err}");
    let (_, err) = w.refused(&["add", "notes", "--from", fake::NOTES]);
    assert!(err.contains("is already in the catalog"), "{err}");
}

#[test]
fn a_vendor_file_git_does_not_ignore_is_refused_printing_the_lines_to_add() {
    let w = World::ignoring("/.mcp.json\n/.agents/mcp_config.json\n");
    let narrow = "[server.local]\nvendors = [\"claude\", \"antigravity\"]\n\
                  transport = \"stdio\"\ncommand = \"node\"\n";
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    fs::write(w.app().join(CATALOG), narrow).unwrap();
    // A vendor file fl does not write need not be ignored, even holding an
    // entry of someone else's.
    let mine = "[mcp_servers.mine]\ncommand = \"mine\"\n";
    fs::create_dir_all(w.app().join(".codex")).unwrap();
    fs::write(w.app().join(CODEX), mine).unwrap();
    w.ok(&["sync"]);

    fs::write(
        w.app().join(CATALOG),
        narrow.replace("vendors = [\"claude\", \"antigravity\"]\n", ""),
    )
    .unwrap();
    let before = w.read(CLAUDE);
    for cmd in ["sync", "check"] {
        let (out, err) = w.refused(&[cmd]);
        assert!(out.is_empty(), "{cmd} prints no plan: {out}");
        assert!(
            err.contains("  .codex/config.toml is not ignored by git"),
            "{err}"
        );
        assert!(!err.contains("  .mcp.json"), "{err}");
        assert!(
            err.contains(&format!(
                "Make sure {} holds these lines:\n/.codex/config.toml\n",
                w.app().join(".gitignore").display()
            )),
            "{err}"
        );
    }
    assert_eq!(w.read(CODEX), mine, "nothing was written");
    assert_eq!(w.read(CLAUDE), before);
}

#[test]
fn a_vendor_file_fl_no_longer_has_an_entry_in_need_not_be_ignored() {
    let w = World::new();
    let local = "[server.local]\ntransport = \"stdio\"\ncommand = \"node\"\n";
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    fs::write(w.app().join(CATALOG), local).unwrap();
    w.ok(&["sync"]);
    // Removed from Codex's file by hand, no longer wanted there, and the
    // file no longer ignored: fl forgets the entry and writes nothing there.
    fs::write(w.app().join(CODEX), "").unwrap();
    fs::write(
        w.app().join(CATALOG),
        local.replace(
            "[server.local]\n",
            "[server.local]\nvendors = [\"claude\"]\n",
        ),
    )
    .unwrap();
    fs::write(
        w.app().join(".gitignore"),
        "/.mcp.json\n/.agents/mcp_config.json\n",
    )
    .unwrap();
    let (out, _) = w.ok(&["sync"]);
    assert!(out.contains("  forget local"), "{out}");
    assert_eq!(w.read(CODEX), "");
}

#[test]
fn a_tracked_vendor_file_is_refused_naming_git_rm_cached() {
    let w = World::new();
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    fs::write(
        w.app().join(CATALOG),
        "[server.local]\ntransport = \"stdio\"\ncommand = \"node\"\n",
    )
    .unwrap();
    w.ok(&["sync"]);
    git(&w.app(), &["add", "-f", CLAUDE]);
    let (_, err) = w.refused(&["check"]);
    assert!(
        err.contains(
            "  .mcp.json is tracked by git: `git rm --cached .mcp.json` stops tracking it"
        ),
        "{err}"
    );
    assert!(!err.contains(".codex/config.toml is"), "{err}");
}

#[test]
fn codex_trust_is_warned_unless_the_project_root_itself_is_trusted() {
    let w = World::new();
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    fs::write(
        w.app().join(CATALOG),
        "[server.local]\ntransport = \"stdio\"\ncommand = \"node\"\n",
    )
    .unwrap();
    let key = w.app().canonicalize().unwrap();
    let warned = format!(
        "warning: Codex loads {} only in a project its user trusts",
        w.app().join(CODEX).display()
    );
    let lines = format!(
        "[projects.\"{}\"]\ntrust_level = \"trusted\"",
        key.display()
    );

    // No Codex config at all.
    let (_, err) = w.ok(&["sync"]);
    assert!(err.contains(&warned), "{err}");
    assert!(err.contains("does not exist"), "{err}");
    assert!(err.contains(&lines), "{err}");
    // An ancestor's trust does not count.
    w.write_codex(&trusted(w.home()));
    let (_, err) = w.ok(&["check"]);
    assert!(err.contains(&warned), "{err}");
    assert!(err.contains("does not trust"), "{err}");
    // Untrusted, said outright.
    w.write_codex(&trusted(&w.app()).replace("\"trusted\"", "\"untrusted\""));
    let (_, err) = w.ok(&["check"]);
    assert!(err.contains("does not trust"), "{err}");
    // A file that is not TOML is a warning, not a refusal, and shows no line.
    w.write_codex("model = \"o3\"\napi_key = \"sk-example-value\n");
    let (_, err) = w.ok(&["sync"]);
    assert!(err.contains("is not valid TOML (line 2)"), "{err}");
    assert!(!err.contains("sk-example-value"), "{err}");
    // The exact key: silence.
    w.write_codex(&format!("model = \"o3\"\n\n{}", trusted(&w.app())));
    let (_, err) = w.ok(&["sync"]);
    assert!(!err.contains("Codex"), "{err}");
    // `$CODEX_HOME` is read instead of `~/.codex`.
    let elsewhere = w.home().join("codex-home");
    fs::create_dir_all(&elsewhere).unwrap();
    let out = w
        .fl_in(&w.app())
        .env("CODEX_HOME", &elsewhere)
        .args(["mcp", "check"])
        .output()
        .unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains(&warned), "{err}");
    assert!(
        err.contains(&elsewhere.join("config.toml").display().to_string()),
        "{err}"
    );
    // An empty `$CODEX_HOME` is unset.
    let out = w
        .fl_in(&w.app())
        .env("CODEX_HOME", "")
        .args(["mcp", "check"])
        .output()
        .unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(!err.contains("Codex"), "{err}");
    // A file fl cannot read is a warning too.
    fs::create_dir_all(elsewhere.join("config.toml")).unwrap();
    let out = w
        .fl_in(&w.app())
        .env("CODEX_HOME", &elsewhere)
        .args(["mcp", "check"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("fl could not read"), "{err}");
    // A catalog with nothing for Codex needs no trust.
    fs::write(
        w.app().join(CATALOG),
        "[server.local]\nvendors = [\"claude\"]\ntransport = \"stdio\"\ncommand = \"node\"\n",
    )
    .unwrap();
    w.write_codex("");
    let (_, err) = w.ok(&["sync"]);
    assert!(!err.contains("Codex"), "{err}");
}

#[test]
fn a_worktree_is_trusted_through_its_main_checkout() {
    let w = World::new();
    fs::create_dir_all(w.app().join(".fl")).unwrap();
    fs::write(
        w.app().join(CATALOG),
        "[server.local]\ntransport = \"stdio\"\ncommand = \"node\"\n",
    )
    .unwrap();
    git(&w.app(), &["add", CATALOG]);
    git(&w.app(), &["commit", "-qm", "catalog"]);
    let wt = w.home().join("wt");
    git(&w.app(), &["worktree", "add", "-q", wt.to_str().unwrap()]);
    w.write_codex(&trusted(&w.app()));
    let (code, _, err) = w.run_in(&wt, &["sync"]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("Codex"), "{err}");
    assert!(wt.join(CLAUDE).exists(), "the worktree is the root");
    // The worktree's own entry comes first, and decides.
    let own = trusted(&wt).replace("\"trusted\"", "\"untrusted\"");
    w.write_codex(&format!("{own}\n{}", trusted(&w.app())));
    let (code, _, err) = w.run_in(&wt, &["check"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("does not trust"), "{err}");
}

#[test]
fn db_is_refused_with_fl_mcp_and_fl_db_is_ignored() {
    let w = World::new();
    let db = w.home().join("elsewhere").join("fl.redb");
    for args in [
        vec!["--db", db.to_str().unwrap(), "mcp", "check"],
        vec!["mcp", "check", "--db", db.to_str().unwrap()],
    ] {
        let out = w.fl_in(&w.app()).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(
            err.contains("error: `fl mcp` opens no store, so `--db` names nothing here"),
            "{err}"
        );
    }
    w.with_registry();
    w.ok(&["add", "notes", "--from", fake::NOTES]);
    let out = w
        .fl_in(&w.app())
        .env("FL_DB", &db)
        .args(["mcp", "sync"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(!db.exists() && !db.parent().unwrap().exists());
}

#[test]
fn no_fl_mcp_command_creates_or_opens_a_store() {
    let w = World::new();
    w.with_registry();
    let (out, _) = w.ok(&["search", "io.example"]);
    for name in fake::LISTED {
        assert!(out.contains(name), "{name}: {out}");
    }
    assert!(out.contains("The notes server, for fl's tests."), "{out}");
    w.ok(&["add", "notes", "--from", fake::NOTES, "--version", "1.1.0"]);
    w.ok(&["add", "local", "--", "node", "server.js"]);
    w.ok(&["disable", "local"]);
    w.ok(&["enable", "local"]);
    w.ok(&["upgrade", "notes"]);
    w.ok(&["sync"]);
    w.run(&["check"]);
    w.ok(&["sync", "--replace", "notes"]);
    w.ok(&["remove", "local"]);
    assert!(
        !w.home().join("data").exists(),
        "fl mcp created {:?}",
        fs::read_dir(w.home().join("data")).map(|d| d.count())
    );
}

#[test]
fn the_root_is_the_nearest_catalog_even_below_a_nested_repository() {
    let w = World::new();
    w.with_registry();
    w.ok(&["add", "local", "--", "node", "server.js"]);
    // A repository inside the project, such as a vendored library.
    let inner = w.app().join("vendor").join("lib");
    repo_at(&inner, &[]);
    let deep = inner.join("src");
    fs::create_dir_all(&deep).unwrap();
    let (code, out, err) = w.run_in(&deep, &["sync"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(w.exists(CLAUDE), "the catalog's directory is the root");
    assert!(!inner.join(CLAUDE).exists());
    assert!(!deep.join(CLAUDE).exists());
}

#[test]
fn with_no_catalog_the_root_is_the_nearest_git_directory() {
    let w = World::new();
    let src = w.app().join("src").join("deep");
    fs::create_dir_all(&src).unwrap();
    let (code, _, err) = w.run_in(&src, &["add", "local", "--", "node", "server.js"]);
    assert_eq!(code, 0, "{err}");
    assert!(w.exists(CATALOG), "created at the repository's root");
    assert!(!src.join(CATALOG).exists());
    // `sync` with no catalog is refused, naming how to make one.
    let other = w.home().join("other");
    repo_at(&other, &[(".gitignore", IGNORED)]);
    let (code, _, err) = w.run_in(&other, &["sync"]);
    assert_eq!(code, 2);
    assert!(err.contains("has no MCP catalog"), "{err}");
}

#[test]
fn outside_a_repository_and_any_catalog_fl_mcp_is_refused() {
    let w = World::new();
    let bare = w.home().join("plain");
    fs::create_dir_all(&bare).unwrap();
    let (code, _, err) = w.run_in(&bare, &["add", "local", "--", "node", "server.js"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("is in no git repository and below no MCP catalog"),
        "{err}"
    );
    assert!(!bare.join(".fl").exists());
}

#[test]
fn search_lists_what_the_registry_holds_and_says_when_it_stopped_early() {
    let w = World::new();
    let (_, err) = w.refused(&["search", "notes"]);
    assert!(
        err.contains("names no registry. `fl mcp registry <url>` sets one"),
        "{err}"
    );
    w.with_registry();
    // Registry text reaches the terminal without its control characters.
    w.fake()
        .add_server("io.example/shady\u{1b}[0m", "1.0.0\u{1b}[0m");
    {
        let mut state = w.fake().state();
        let shady = state.entries.last_mut().unwrap();
        shady["server"]["description"] = "\u{1b}[2J\u{7}cleared".into();
        shady["_meta"]["io.modelcontextprotocol.registry/official"]["status"] =
            "\u{1b}[31mpaused".into();
    }
    let (out, _) = w.ok(&["search", "shady"]);
    assert_eq!(
        out,
        "io.example/shady[0m 1.0.0[0m ([31mpaused)  [2Jcleared\n"
    );
    let (out, _) = w.ok(&["search", "legacy"]);
    assert_eq!(
        out,
        format!(
            "{} 0.1.0 (deprecated)  The legacy server, for fl's tests.\n",
            fake::LEGACY
        )
    );
    let (out, _) = w.ok(&["search", "notes"]);
    assert_eq!(
        out,
        format!("{} 1.2.0  The notes server, for fl's tests.\n", fake::NOTES)
    );
    let (out, _) = w.ok(&["search", "nothing-by-that-name"]);
    assert!(
        out.contains("no server's name holds `nothing-by-that-name`"),
        "{out}"
    );
    w.fake().state().page_limit = 1;
    for n in 0..15 {
        w.fake()
            .add_server(&format!("io.example/extra-{n:02}"), "1.0.0");
    }
    let (out, err) = w.ok(&["search", "io.example"]);
    assert_eq!(out.lines().count(), 20, "{out}");
    assert!(
        err.contains("note: fl stopped after 20 pages of results"),
        "{err}"
    );
}

#[test]
fn a_flag_given_twice_or_an_add_with_no_source_is_refused() {
    let w = World::new();
    w.with_registry();
    let (_, err) = w.refused(&["add", "local"]);
    assert!(err.contains("say where `local` comes from"), "{err}");
    let (_, err) = w.refused(&["add", "local", "--env", "A=1", "--env", "A=2", "--", "node"]);
    assert!(err.contains("`--env A` is given twice"), "{err}");
    let (_, err) = w.refused(&[
        "add",
        "docs",
        "--url",
        fake::DOCS_URL,
        "--header",
        "X-Key",
        "--header",
        "X-Key=K",
    ]);
    assert!(err.contains("`--header X-Key` is given twice"), "{err}");
    let (_, err) = w.refused(&[
        "add",
        "weather",
        "--from",
        fake::WEATHER,
        "--env",
        "U=1",
        "--env",
        "U=2",
    ]);
    assert!(err.contains("`--env U=…` is given twice"), "{err}");
    let (_, err) = w.refused(&["add", "weather", "--from", fake::WEATHER, "--env", "UNITS"]);
    assert!(
        err.contains("`--env UNITS` needs `=<value>` with `--from`"),
        "{err}"
    );
    assert!(!w.exists(CATALOG) || !w.read(CATALOG).contains("[server."));
    assert_eq!(
        w.requests().len(),
        0,
        "each is refused before the registry is read"
    );
}

// Each form of `add` takes its own flags; one of another form is refused by
// name, never ignored, and nothing is added.
#[test]
fn a_flag_of_another_form_of_add_is_refused_naming_it() {
    let w = World::new();
    w.with_registry();
    let before = w.read(CATALOG);
    let url = fake::DOCS_URL;
    let cases: [(&[&str], &str); 10] = [
        (
            &["add", "z", "--header", "Authorization", "--", "node"],
            "`--header` has no place in `fl mcp add z -- <command>`: it goes with `--url <url>`",
        ),
        (
            &["add", "z", "--version", "1", "--", "node"],
            "`--version` has no place in `fl mcp add z -- <command>`: it goes with \
             `--from <registry-name>`",
        ),
        (
            &["add", "z", "--package", "npm", "--", "node"],
            "`--package` has no place in `fl mcp add z -- <command>`",
        ),
        (
            &["add", "z", "--remote", "--", "node"],
            "`--remote` has no place in `fl mcp add z -- <command>`",
        ),
        (
            &["add", "z", "--with", "A", "--", "node"],
            "`--with` has no place in `fl mcp add z -- <command>`",
        ),
        (
            &["add", "z", "--version", "1", "--url", url],
            "`--version` has no place in `fl mcp add z --url <url>`: it goes with \
             `--from <registry-name>`",
        ),
        (
            &["add", "z", "--package", "npm", "--url", url],
            "`--package` has no place in `fl mcp add z --url <url>`",
        ),
        (
            &["add", "z", "--remote", "--url", url],
            "`--remote` has no place in `fl mcp add z --url <url>`",
        ),
        (
            &["add", "z", "--with", "A", "--url", url],
            "`--with` has no place in `fl mcp add z --url <url>`",
        ),
        (
            &[
                "add",
                "z",
                "--from",
                fake::NOTES,
                "--header",
                "Authorization",
            ],
            "`--header` has no place in `fl mcp add z --from <registry-name>`: it goes with \
             `--url <url>`",
        ),
    ];
    for (args, phrase) in cases {
        let (_, err) = w.refused(args);
        assert!(err.contains(phrase), "{args:?}: {phrase:?} not in {err}");
    }
    // clap refuses the forms given together, and `--env` with `--url`.
    let clap: [(&[&str], &str); 4] = [
        (
            &["add", "z", "--url", url, "--", "node"],
            "'--url <URL>' cannot be used with",
        ),
        (
            &["add", "z", "--from", fake::NOTES, "--", "node"],
            "'--from <REGISTRY_NAME>' cannot be used with",
        ),
        (
            &["add", "z", "--from", fake::NOTES, "--url", url],
            "'--from <REGISTRY_NAME>' cannot be used with '--url <URL>'",
        ),
        (
            &["add", "z", "--env", "A", "--url", url],
            "'--env <NAME[=VALUE]>' cannot be used with '--url <URL>'",
        ),
    ];
    for (args, phrase) in clap {
        let (_, err) = w.refused(args);
        assert!(err.contains(phrase), "{args:?}: {phrase:?} not in {err}");
    }
    assert_eq!(w.read(CATALOG), before, "nothing was added");
    assert_eq!(
        w.requests().len(),
        0,
        "each is refused before the registry is read"
    );
}

// A vendor file that is a link would carry fl's write into the file it
// points at — here one git tracks, which the gitignore guard, asking about
// the link's own path, cannot see: refused, and nothing is written.
#[test]
fn a_vendor_file_that_links_to_a_tracked_file_is_refused() {
    let w = World::new();
    w.ok(&["add", "local", "--", "node", "server.js"]);
    let shared = "{\"mcpServers\": {}}\n";
    fs::create_dir_all(w.app().join("docs")).unwrap();
    fs::write(w.app().join("docs/shared.json"), shared).unwrap();
    git(&w.app(), &["add", "docs/shared.json"]);
    git(&w.app(), &["commit", "-qm", "shared"]);
    std::os::unix::fs::symlink("docs/shared.json", w.app().join(CLAUDE)).unwrap();
    let (out, err) = w.refused(&["sync"]);
    assert!(
        err.contains(
            ".mcp.json: it is a symbolic link, and fl writes only plain files it can see whole"
        ),
        "{err}"
    );
    assert_eq!(out, "");
    assert_eq!(w.read("docs/shared.json"), shared);
    assert!(!w.exists(CODEX) && !w.exists(AGY), "nothing is written");
    let (code, _, _) = w.run(&["check"]);
    assert_eq!(code, 2);
}

#[test]
fn sync_with_no_state_directory_is_refused() {
    let w = World::new();
    w.ok(&["add", "local", "--", "node", "server.js"]);
    let out = w
        .fl_in(&w.app())
        .env_remove("HOME")
        .env_remove("XDG_STATE_HOME")
        .args(["mcp", "sync"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("fl has nowhere to keep the record"), "{err}");
    assert!(!w.exists(CLAUDE));
    // With a state directory and no home, Codex's config cannot be found.
    let out = w
        .fl_in(&w.app())
        .env_remove("HOME")
        .args(["mcp", "sync"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("neither $CODEX_HOME nor $HOME is set"),
        "{err}"
    );
}
