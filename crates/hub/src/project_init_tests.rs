//! Project initialization: the shared flow and `POST /api/v1/projects`. Every path lives under
//! a temporary directory, and the flow reads no environment variable, so nothing here reaches
//! a real config or data directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use fl_core::routing::{Routes, Tier};
use fl_core::store::{AgentRegistry, Catalog};
use fl_store::AnyStore;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::project_init::{
    Credential, DEFAULT_ROUTING_AREAS, INITIAL_COMMIT_MESSAGE, InitError, InitOutcome, InitPaths,
    InitRequest, init_project, user_config_path,
};
use crate::{HttpRequest, HttpResponse, HubServerConfig, HubState, handle_http_request};

// --- the shared flow --------------------------------------------------------------------------

struct Sandbox {
    _tmp: TempDir,
    /// Canonical base of everything the test makes.
    base: PathBuf,
    paths: InitPaths,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let paths = InitPaths {
            config_path: base.join("cfg/fl/config.toml"),
            store_dir: base.join("data/fl"),
            workspace_store: base.join("data/fl/fl.fls"),
        };
        Self {
            _tmp: tmp,
            base,
            paths,
        }
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.base.join("work").join(name)
    }

    fn config(&self) -> String {
        fs::read_to_string(&self.paths.config_path).unwrap_or_default()
    }

    fn request(&self, path: &Path) -> InitRequest {
        InitRequest {
            path: path.to_path_buf(),
            name: None,
            github: None,
            credential: None,
            purpose: None,
            by: "tester".into(),
        }
    }

    fn init(&self, req: &InitRequest) -> Result<InitOutcome, InitError> {
        init_project(req, &self.paths, |r| {
            let project = fl_store::open_store(&r.store)
                .map_err(|e| InitError::Failed(format!("project store: {e}")))?;
            let workspace = fl_store::open_store(&self.paths.workspace_store)
                .map_err(|e| InitError::Failed(format!("workspace store: {e}")))?;
            Ok((Box::new(project), Box::new(workspace)))
        })
    }

    fn workspace(&self) -> AnyStore {
        fl_store::open_store(&self.paths.workspace_store).unwrap()
    }

    /// The `[[project]]` blocks of the config, parsed.
    fn blocks(&self) -> Vec<toml::Table> {
        let doc: toml::Table = toml::from_str(&self.config()).unwrap();
        doc.get("project")
            .and_then(|p| p.as_array())
            .map(|a| a.iter().map(|t| t.as_table().unwrap().clone()).collect())
            .unwrap_or_default()
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_count(dir: &Path) -> usize {
    git(dir, &["rev-list", "--count", "HEAD"]).parse().unwrap()
}

fn git_with_history(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    for (n, msg) in [(1, "first"), (2, "second")] {
        fs::write(dir.join(format!("f{n}.txt")), msg).unwrap();
        git(dir, &["add", "-A"]);
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-qm",
                msg,
            ],
        );
    }
}

fn invalid(r: Result<InitOutcome, InitError>) -> String {
    match r {
        Err(InitError::Invalid(m)) => m,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

fn conflict(r: Result<InitOutcome, InitError>) -> String {
    match r {
        Err(InitError::Conflict(m)) => m,
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[test]
fn a_fresh_directory_becomes_a_registered_project() {
    let sb = Sandbox::new();
    let root = sb.dir("alpha");
    assert!(!root.exists());
    let out = sb.init(&sb.request(&root)).unwrap();

    assert_eq!(out.name, "alpha");
    assert_eq!(out.root, root.display().to_string());
    assert!(out.registered);
    assert!(out.git.initialized && out.git.initial_commit);
    assert!(root.join(".git").exists());
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), out.git.head);
    assert_eq!(
        git(&root, &["log", "-1", "--format=%s"]),
        INITIAL_COMMIT_MESSAGE
    );
    assert_eq!(git(&root, &["log", "-1", "--format=%an"]), "tester");
    assert_eq!(commit_count(&root), 1);
    assert!(
        git(&root, &["show", "--stat", "--format=", "HEAD"]).is_empty(),
        "the initial commit is empty"
    );

    let blocks = sb.blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["name"].as_str(), Some("alpha"));
    assert_eq!(blocks[0]["root"].as_str(), Some(out.root.as_str()));
    let store = sb.paths.store_dir.join("alpha.fls");
    assert_eq!(blocks[0]["store"].as_str(), Some(store.to_str().unwrap()));
    assert!(blocks[0].get("tracker").is_none());
    assert!(store.is_file());
}

#[test]
fn an_existing_directory_that_is_not_a_repository_gets_git_and_an_empty_commit() {
    let sb = Sandbox::new();
    let root = sb.dir("plain");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("notes.txt"), "keep me").unwrap();

    let out = sb.init(&sb.request(&root)).unwrap();
    assert!(out.git.initialized && out.git.initial_commit);
    assert_eq!(commit_count(&root), 1);
    // The commit is empty: the user's file is untouched and still untracked.
    assert_eq!(
        fs::read_to_string(root.join("notes.txt")).unwrap(),
        "keep me"
    );
    assert!(git(&root, &["status", "--porcelain"]).contains("notes.txt"));
}

#[test]
fn a_repository_with_history_is_left_exactly_as_it_is() {
    let sb = Sandbox::new();
    let root = sb.dir("history");
    git_with_history(&root);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let log = git(&root, &["log", "--format=%H %s"]);

    let out = sb.init(&sb.request(&root)).unwrap();
    assert!(!out.git.initialized && !out.git.initial_commit);
    assert_eq!(out.git.head, head);
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&root, &["log", "--format=%H %s"]), log);
    assert_eq!(commit_count(&root), 2);
}

#[test]
fn a_repository_without_a_head_gets_the_commit_and_no_second_init() {
    let sb = Sandbox::new();
    let root = sb.dir("unborn");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    fs::write(root.join(".git/marker"), "same repository").unwrap();

    let out = sb.init(&sb.request(&root)).unwrap();
    assert!(!out.git.initialized, "the repository was already there");
    assert!(out.git.initial_commit);
    assert_eq!(commit_count(&root), 1);
    assert_eq!(
        fs::read_to_string(root.join(".git/marker")).unwrap(),
        "same repository"
    );
}

#[test]
fn the_name_comes_from_the_flag_or_the_lowercase_directory_name() {
    let sb = Sandbox::new();
    assert_eq!(
        sb.init(&sb.request(&sb.dir("My-App"))).unwrap().name,
        "my-app"
    );

    let mut req = sb.request(&sb.dir("dir-two"));
    req.name = Some("Chosen_Name".into());
    let out = sb.init(&req).unwrap();
    assert_eq!(out.name, "chosen_name");
    assert!(sb.paths.store_dir.join("chosen_name.fls").is_file());

    for bad in [
        "",
        "   ",
        "9lives",
        "has space",
        "a/b",
        "-dash",
        "x.y",
        &"a".repeat(65),
    ] {
        let mut req = sb.request(&sb.dir("never"));
        req.name = Some(bad.into());
        let msg = invalid(sb.init(&req));
        assert!(msg.contains("project name"), "{bad:?}: {msg}");
    }
    // A directory name that is no name needs --name, and the refusal says so.
    let msg = invalid(sb.init(&sb.request(&sb.dir("9 lives"))));
    assert!(msg.contains("--name"), "{msg}");
    assert!(!sb.dir("never").exists() && !sb.dir("9 lives").exists());
}

#[test]
fn running_it_again_changes_nothing() {
    let sb = Sandbox::new();
    let root = sb.dir("again");
    let mut req = sb.request(&root);
    req.github = Some("acme/widgets".into());
    let first = sb.init(&req).unwrap();
    let config = sb.config();
    let head = git(&root, &["rev-parse", "HEAD"]);

    let second = sb.init(&req).unwrap();
    assert!(first.registered && !second.registered);
    assert_eq!(
        second.project, first.project,
        "the project is not made twice"
    );
    assert_eq!(second.git.head, head);
    assert!(!second.git.initialized && !second.git.initial_commit);
    assert!(!second.mcp_catalog_created);
    assert_eq!(sb.config(), config, "the config is byte for byte the same");
    assert_eq!(sb.blocks().len(), 1);
    assert_eq!(commit_count(&root), 1);

    let store = fl_store::open_store(&sb.paths.store_dir.join("again.fls")).unwrap();
    assert_eq!(store.list_projects().unwrap().len(), 1);
    // With no name given, the name the config already gives the root is used.
    let mut other = sb.request(&root);
    other.name = None;
    assert_eq!(sb.init(&other).unwrap().name, "again");
}

#[test]
fn a_name_taken_at_another_root_is_refused_and_nothing_is_made() {
    let sb = Sandbox::new();
    let mut req = sb.request(&sb.dir("first"));
    req.name = Some("shared".into());
    sb.init(&req).unwrap();
    let config = sb.config();

    let second = sb.dir("second");
    let mut clash = sb.request(&second);
    clash.name = Some("SHARED".into());
    let msg = conflict(sb.init(&clash));
    assert!(msg.contains("`shared` is already registered"), "{msg}");
    assert!(msg.contains("--name"), "{msg}");
    assert!(!second.exists(), "a refused request makes no directory");
    assert_eq!(sb.config(), config);

    // The directory name collides the same way.
    let mut by_dir = sb.request(&sb.base.join("elsewhere/shared"));
    by_dir.name = None;
    conflict(sb.init(&by_dir));

    // And one root cannot be registered under a second name.
    let mut rename = sb.request(&sb.dir("first"));
    rename.name = Some("renamed".into());
    let msg = conflict(sb.init(&rename));
    assert!(
        msg.contains("already registered as project `shared`"),
        "{msg}"
    );
    assert_eq!(sb.config(), config);
}

#[test]
fn the_config_keeps_every_other_project_and_comment() {
    let sb = Sandbox::new();
    let original = "\
# my fl config
[state]
store = \"/somewhere/fl.redb\" # the workspace store

[[project]]
name = \"old\"
root = \"/srv/old\"
store = \"/srv/old.fls\"
tracker = { github = \"acme/old\", credential = \"env\" }

[hub]
listen = \"127.0.0.1:9\"
";
    fs::create_dir_all(sb.paths.config_path.parent().unwrap()).unwrap();
    fs::write(&sb.paths.config_path, original).unwrap();

    sb.init(&sb.request(&sb.dir("new"))).unwrap();
    let after = sb.config();
    assert!(
        after.starts_with(original),
        "everything that was there is kept as it was:\n{after}"
    );
    let blocks = sb.blocks();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["name"].as_str(), Some("old"));
    assert_eq!(blocks[1]["name"].as_str(), Some("new"));
    // Still the file the CLI's loader reads: every key is one it knows.
    assert!(toml::from_str::<toml::Table>(&after).is_ok());
}

#[test]
fn a_block_is_written_with_the_tracker_only_when_one_is_asked_for() {
    let sb = Sandbox::new();
    sb.init(&sb.request(&sb.dir("local"))).unwrap();

    let mut gh = sb.request(&sb.dir("hosted"));
    gh.github = Some("acme/widgets".into());
    let out = sb.init(&gh).unwrap();
    assert_eq!(out.tracker.as_ref().unwrap().github, "acme/widgets");
    assert_eq!(out.tracker.as_ref().unwrap().credential, Credential::App);

    let mut envc = sb.request(&sb.dir("envy"));
    envc.github = Some("acme/envy".into());
    envc.credential = Some(Credential::Env);
    sb.init(&envc).unwrap();

    let blocks = sb.blocks();
    assert!(blocks[0].get("tracker").is_none());
    assert_eq!(
        blocks[1]["tracker"]["github"].as_str(),
        Some("acme/widgets")
    );
    assert_eq!(blocks[1]["tracker"]["credential"].as_str(), Some("app"));
    assert_eq!(blocks[2]["tracker"]["credential"].as_str(), Some("env"));
    assert!(sb.config().contains("[project.tracker]"));
}

#[test]
fn a_repository_or_credential_that_cannot_be_used_is_refused() {
    let sb = Sandbox::new();
    for bad in [
        "widgets",
        "acme/",
        "/widgets",
        "a/b/c",
        "acme/wid gets",
        "../x",
        "a/..",
    ] {
        let mut req = sb.request(&sb.dir("p"));
        req.github = Some(bad.into());
        let msg = invalid(sb.init(&req));
        assert!(msg.contains("owner/repo"), "{bad:?}: {msg}");
    }
    let mut lone = sb.request(&sb.dir("p"));
    lone.credential = Some(Credential::Env);
    let msg = invalid(sb.init(&lone));
    assert!(msg.contains("`github`"), "{msg}");
    assert!(!sb.dir("p").exists() && !sb.paths.config_path.exists());

    assert_eq!("app".parse::<Credential>(), Ok(Credential::App));
    assert_eq!("env".parse::<Credential>(), Ok(Credential::Env));
    assert!("token".parse::<Credential>().is_err());
    assert!("".parse::<Credential>().is_err());
}

#[test]
fn a_tracker_is_added_to_a_project_that_has_none_and_never_rebound() {
    let sb = Sandbox::new();
    let root = sb.dir("grows");
    sb.init(&sb.request(&root)).unwrap();

    let mut gh = sb.request(&root);
    gh.github = Some("acme/grows".into());
    let out = sb.init(&gh).unwrap();
    assert!(!out.registered);
    assert_eq!(sb.blocks().len(), 1);
    assert_eq!(
        sb.blocks()[0]["tracker"]["github"].as_str(),
        Some("acme/grows")
    );

    // The same binding again is a no-op, in either spelling of the case.
    let before = sb.config();
    gh.github = Some("ACME/grows".into());
    sb.init(&gh).unwrap();
    assert_eq!(sb.config(), before);

    // Another repository, or another credential, is a different binding: refused.
    let mut other = sb.request(&root);
    other.github = Some("acme/else".into());
    let msg = conflict(sb.init(&other));
    assert!(msg.contains("will not rebind"), "{msg}");
    let mut cred = sb.request(&root);
    cred.github = Some("acme/grows".into());
    cred.credential = Some(Credential::Env);
    conflict(sb.init(&cred));
    assert_eq!(sb.config(), before);
}

#[test]
fn adding_a_tracker_keeps_the_blocks_own_comments() {
    let sb = Sandbox::new();
    let root = sb.dir("annotated");
    fs::create_dir_all(&root).unwrap();
    let own_store = sb.base.join("stores/a.fls");
    let original = format!(
        "# top\n[[project]] # mine\nname = \"annotated\" # why\nroot = \"{}\"\nstore = \"{}\"\n\n[[project]]\nroot = \"/other\"\nstore = \"/s/o.fls\"\n",
        root.display(),
        own_store.display()
    );
    fs::create_dir_all(sb.paths.config_path.parent().unwrap()).unwrap();
    fs::write(&sb.paths.config_path, &original).unwrap();

    let mut req = sb.request(&root);
    req.github = Some("acme/annotated".into());
    let out = sb.init(&req).unwrap();
    assert!(!out.registered);
    assert_eq!(
        out.store,
        own_store.display().to_string(),
        "an existing block keeps its store"
    );
    assert!(own_store.is_file());
    let after = sb.config();
    for kept in [
        "# top",
        "[[project]] # mine",
        "name = \"annotated\" # why",
        "root = \"/other\"",
    ] {
        assert!(after.contains(kept), "{kept}: {after}");
    }
    let blocks = sb.blocks();
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        blocks[0]["tracker"]["github"].as_str(),
        Some("acme/annotated")
    );
    assert!(blocks[1].get("tracker").is_none());
}

#[test]
fn a_config_with_no_final_newline_gets_a_separate_block() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.paths.config_path.parent().unwrap()).unwrap();
    fs::write(&sb.paths.config_path, "[wire]\nsync_wire = true").unwrap();
    sb.init(&sb.request(&sb.dir("tail"))).unwrap();
    let after = sb.config();
    assert!(
        after.starts_with("[wire]\nsync_wire = true\n\n[[project]]\n"),
        "{after:?}"
    );
    assert_eq!(sb.blocks().len(), 1);
}

#[test]
fn unsafe_paths_are_refused_before_anything_is_written() {
    let sb = Sandbox::new();
    let file = sb.base.join("a-file");
    fs::write(&file, "x").unwrap();
    let cases: Vec<(PathBuf, &str)> = vec![
        (PathBuf::from(""), "empty"),
        (PathBuf::from("   "), "empty"),
        (PathBuf::from("/tmp/bad\0path"), "NUL"),
        (PathBuf::from("relative/dir"), "absolute"),
        (PathBuf::from("."), "absolute"),
        (sb.base.join("a/../b"), ".."),
        (file.clone(), "not a directory"),
        (file.join("below"), "cannot be resolved"),
    ];
    for (path, why) in cases {
        let msg = invalid(sb.init(&sb.request(&path)));
        assert!(msg.contains(why), "{path:?}: {msg}");
    }
    assert!(!sb.paths.config_path.exists());
    assert!(!sb.paths.store_dir.exists());
    assert_eq!(fs::read_to_string(&file).unwrap(), "x");
}

#[test]
fn a_name_that_would_be_the_workspace_store_is_refused() {
    let sb = Sandbox::new();
    let mut req = sb.request(&sb.dir("fl"));
    req.name = Some("fl".into());
    let msg = conflict(sb.init(&req));
    assert!(msg.contains("workspace store"), "{msg}");
    assert!(!sb.dir("fl").exists());
}

#[test]
fn a_root_that_holds_the_stores_is_refused() {
    let sb = Sandbox::new();
    for root in [sb.base.join("data"), sb.paths.store_dir.join("inside")] {
        let mut req = sb.request(&root);
        req.name = Some("holder".into());
        let msg = invalid(sb.init(&req));
        assert!(msg.contains("contain one another"), "{msg}");
    }
}

#[test]
fn a_store_that_holds_another_project_is_not_taken_over() {
    let sb = Sandbox::new();
    fs::create_dir_all(&sb.paths.store_dir).unwrap();
    {
        let held = fl_store::open_store(&sb.paths.store_dir.join("taken.fls")).unwrap();
        held.add_project("/workspace/taken").unwrap();
    }
    let mut req = sb.request(&sb.dir("taken"));
    req.name = Some("taken".into());
    let msg = conflict(sb.init(&req));
    assert!(msg.contains("/workspace/taken"), "{msg}");
    assert!(sb.blocks().is_empty(), "the project was not registered");
}

#[test]
fn the_project_store_holds_the_project_and_the_seven_local_areas() {
    let sb = Sandbox::new();
    let root = sb.dir("seeded");
    let out = sb.init(&sb.request(&root)).unwrap();
    assert_eq!(out.areas, DEFAULT_ROUTING_AREAS);
    assert_eq!(
        DEFAULT_ROUTING_AREAS,
        [
            "backlog", "code", "design", "product", "security", "tests", "ux"
        ]
    );

    let store = fl_store::open_store(&sb.paths.store_dir.join("seeded.fls")).unwrap();
    let projects = store.list_projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].root, root.display().to_string());
    assert_eq!(projects[0].id.iri().as_str(), out.project);
    let map = store
        .routes(&projects[0].id)
        .unwrap()
        .expect("a routing map");
    let names: Vec<&str> = map.areas.iter().map(|a| a.area.as_str()).collect();
    assert_eq!(names, DEFAULT_ROUTING_AREAS);
    assert!(map.areas.iter().all(|a| a.tier == Tier::Local));
    assert_eq!(
        map.areas
            .iter()
            .filter(|a| a.sensitive)
            .map(|a| a.area.as_str())
            .collect::<Vec<_>>(),
        ["security"]
    );
}

#[test]
fn a_routing_map_the_user_changed_survives_a_rerun() {
    let sb = Sandbox::new();
    let root = sb.dir("custom");
    sb.init(&sb.request(&root)).unwrap();
    let store = fl_store::open_store(&sb.paths.store_dir.join("custom.fls")).unwrap();
    let id = store.list_projects().unwrap().remove(0).id;
    let changed = store
        .routes(&id)
        .unwrap()
        .unwrap()
        .with("design", Tier::Github, false);
    store.set_routes(&id, &changed).unwrap();
    drop(store);

    let out = sb.init(&sb.request(&root)).unwrap();
    assert_eq!(out.areas, DEFAULT_ROUTING_AREAS);
    let store = fl_store::open_store(&sb.paths.store_dir.join("custom.fls")).unwrap();
    assert_eq!(store.routes(&id).unwrap().unwrap(), changed);
}

#[test]
fn the_workspace_area_is_upserted_and_keeps_its_purpose() {
    let sb = Sandbox::new();
    let root = sb.dir("area");
    let mut req = sb.request(&root);
    req.purpose = Some("the area project".into());
    let out = sb.init(&req).unwrap();
    assert_eq!(out.purpose, "the area project");

    let area = sb.workspace().get_area("area").unwrap().unwrap();
    assert_eq!(area.area_key, "area");
    assert_eq!(area.name, "area");
    assert_eq!(area.path.as_deref(), Some(out.root.as_str()));
    assert_eq!(area.purpose, "the area project");
    assert_eq!(area.by, "tester");

    // No purpose on a re-run keeps the one the area has; a new one replaces it.
    assert_eq!(
        sb.init(&sb.request(&root)).unwrap().purpose,
        "the area project"
    );
    let mut again = sb.request(&root);
    again.purpose = Some("renamed purpose".into());
    sb.init(&again).unwrap();
    let areas = sb.workspace().list_areas().unwrap();
    assert_eq!(areas.len(), 1, "one area, upserted: {areas:?}");
    assert_eq!(areas[0].purpose, "renamed purpose");

    // Without any purpose a new project still gets a valid one.
    let out = sb.init(&sb.request(&sb.dir("plain"))).unwrap();
    assert_eq!(out.purpose, "Project plain");

    let mut bad = sb.request(&sb.dir("multi"));
    bad.purpose = Some("two\nlines".into());
    invalid(sb.init(&bad));
    let mut empty = sb.request(&sb.dir("multi"));
    empty.purpose = Some("  ".into());
    invalid(sb.init(&empty));
    assert!(!sb.dir("multi").exists());
}

#[test]
fn the_starter_mcp_catalog_is_made_only_when_absent() {
    let sb = Sandbox::new();
    let fresh = sb.dir("catalog");
    let out = sb.init(&sb.request(&fresh)).unwrap();
    assert!(out.mcp_catalog_created);
    let made = fs::read_to_string(fresh.join(".fl/mcp.toml")).unwrap();
    assert!(
        made.starts_with("# The MCP servers this project uses"),
        "{made}"
    );
    assert!(
        fl_mcp::catalog::Catalog::load(&fresh).unwrap().is_some(),
        "the starter is a valid catalog"
    );

    let kept = sb.dir("kept");
    fs::create_dir_all(kept.join(".fl")).unwrap();
    let mine = "# mine\n[server.x]\nenabled = true\ntransport = \"stdio\"\ncommand = \"x\"\n";
    fs::write(kept.join(".fl/mcp.toml"), mine).unwrap();
    let out = sb.init(&sb.request(&kept)).unwrap();
    assert!(!out.mcp_catalog_created);
    assert_eq!(fs::read_to_string(kept.join(".fl/mcp.toml")).unwrap(), mine);
}

#[test]
fn a_config_that_is_not_toml_is_never_overwritten() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.paths.config_path.parent().unwrap()).unwrap();
    fs::write(&sb.paths.config_path, "this is [not toml").unwrap();
    let msg = invalid(sb.init(&sb.request(&sb.dir("p"))));
    assert!(msg.contains("not valid TOML"), "{msg}");
    assert_eq!(sb.config(), "this is [not toml");
    assert!(!sb.dir("p").exists());

    fs::write(&sb.paths.config_path, "project = [1, 2]\n").unwrap();
    let msg = invalid(sb.init(&sb.request(&sb.dir("p"))));
    assert!(msg.contains("`[[project]]` tables"), "{msg}");
    assert_eq!(sb.config(), "project = [1, 2]\n");
}

#[cfg(unix)]
#[test]
fn a_symlinked_config_is_written_through_and_stays_a_link() {
    let sb = Sandbox::new();
    let real = sb.base.join("dotfiles/config.toml");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, "# kept\n").unwrap();
    fs::create_dir_all(sb.paths.config_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, &sb.paths.config_path).unwrap();

    sb.init(&sb.request(&sb.dir("linked"))).unwrap();
    assert!(
        fs::symlink_metadata(&sb.paths.config_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let text = fs::read_to_string(&real).unwrap();
    assert!(
        text.starts_with("# kept\n") && text.contains("name = \"linked\""),
        "{text:?}"
    );
}

#[test]
fn no_temporary_file_is_left_beside_the_config() {
    let sb = Sandbox::new();
    sb.init(&sb.request(&sb.dir("tidy"))).unwrap();
    let names: Vec<String> = fs::read_dir(sb.paths.config_path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["config.toml"]);
}

#[test]
fn the_hubs_config_is_in_its_home() {
    assert_eq!(
        user_config_path(Some(Path::new("/h"))),
        Some(PathBuf::from("/h/.config/fl/config.toml"))
    );
}

// --- POST /api/v1/projects --------------------------------------------------------------------

const OWNER: &str = "owner-tok-secret";
const PEER: &str = "peer-tok-secret";

struct Hub {
    _tmp: TempDir,
    base: PathBuf,
    state: HubState,
}

impl Hub {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        for dir in ["home", "data", "repo", "work"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        let mut state = HubState::new(HubServerConfig {
            listen: "127.0.0.1:0".into(),
            store_path: base.join("data/store.redb"),
            repo_root: base.join("repo"),
            ui_dir: None,
            owner_login: "lmcfarlin".into(),
            reviewer_apps: fl_core::pr_policy::ReviewerApps::default(),
            owner_token: Some(OWNER.into()),
            peer_token: Some(PEER.into()),
            trust_tailscale_headers: false,
            enable_terminal: false,
            session_ttl_secs: 3600,
            home_override: Some(base.join("home")),
            github: crate::github_pr::HubGithub::default(),
        });
        state.store_backend = Some(fl_store::BackendKind::Ferro);
        Self {
            _tmp: tmp,
            base,
            state,
        }
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> HttpResponse {
        let mut req = HttpRequest::new(method, path);
        if let Some(token) = token {
            req = req.with_header("Authorization", format!("Bearer {token}"));
        }
        if let Some(body) = body {
            req = req.with_json_body(body);
        }
        handle_http_request(&self.state, &req)
    }

    fn post(&self, token: &str, body: &Value) -> HttpResponse {
        self.send("POST", "/api/v1/projects", Some(token), Some(body))
    }

    fn listed(&self) -> Vec<String> {
        let resp = self.send("GET", "/api/v1/projects", Some(OWNER), None);
        assert_eq!(resp.status, 200);
        resp.json_value().unwrap()["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn work(&self, name: &str) -> PathBuf {
        self.base.join("work").join(name)
    }

    fn config(&self) -> PathBuf {
        self.base.join("home/.config/fl/config.toml")
    }
}

fn body_of(resp: &HttpResponse) -> Value {
    resp.json_value().unwrap()
}

fn error_of(resp: &HttpResponse) -> String {
    body_of(resp)["error"].as_str().unwrap_or("").to_string()
}

#[test]
fn the_owner_creates_a_project_and_it_is_listed() {
    let hub = Hub::new();
    assert_eq!(hub.listed(), ["ferroloop"]);
    let root = hub.work("hubby");
    let resp = hub.post(
        OWNER,
        &json!({ "path": root, "github": "acme/hubby", "credential": "env", "purpose": "from the hub" }),
    );
    assert_eq!(resp.status, 201, "{}", String::from_utf8_lossy(&resp.body));
    let body = body_of(&resp);
    assert_eq!(body["id"], "hubby");
    assert_eq!(body["name"], "hubby");
    assert_eq!(body["root"], root.display().to_string());
    assert_eq!(body["purpose"], "from the hub");
    assert_eq!(
        body["tracker"],
        json!({ "github": "acme/hubby", "credential": "env" })
    );
    assert_eq!(body["areas"], json!(DEFAULT_ROUTING_AREAS));
    assert_eq!(body["registered"], true);
    assert!(body["effective_artifacts_dir"].is_string());

    assert_eq!(hub.listed(), ["ferroloop", "hubby"]);
    assert!(hub.base.join("home/.local/share/fl/hubby.fls").is_file());
    assert!(root.join(".git").exists());
    assert!(root.join(".fl/mcp.toml").is_file());
    let config = fs::read_to_string(hub.config()).unwrap();
    assert!(config.contains("name = \"hubby\""), "{config}");
    assert!(config.contains("credential = \"env\""), "{config}");
    // The workspace store is the Hub's own, and holds the area.
    let ws = fl_store::open_store(&hub.base.join("data/store.redb")).unwrap();
    assert_eq!(
        ws.get_area("hubby").unwrap().unwrap().purpose,
        "from the hub"
    );

    // The records API reaches the new project's store.
    let board = hub.send("GET", "/api/v1/records?project=hubby", Some(OWNER), None);
    assert_eq!(
        board.status,
        200,
        "{}",
        String::from_utf8_lossy(&board.body)
    );
    let made = hub.send(
        "POST",
        "/api/v1/records",
        Some(OWNER),
        Some(&json!({ "project": "hubby", "title": "first card" })),
    );
    assert_eq!(made.status, 201, "{}", String::from_utf8_lossy(&made.body));
}

#[test]
fn posting_the_same_project_again_is_idempotent() {
    let hub = Hub::new();
    let body = json!({ "path": hub.work("twice") });
    let first = hub.post(OWNER, &body);
    assert_eq!(first.status, 201);
    let config = fs::read_to_string(hub.config()).unwrap();
    let second = hub.post(OWNER, &body);
    assert_eq!(second.status, 200);
    assert_eq!(body_of(&second)["registered"], false);
    assert_eq!(body_of(&second)["project"], body_of(&first)["project"]);
    assert_eq!(fs::read_to_string(hub.config()).unwrap(), config);
    assert_eq!(hub.listed(), ["ferroloop", "twice"]);
}

#[test]
fn only_the_owner_may_create_a_project() {
    let hub = Hub::new();
    let body = json!({ "path": hub.work("nope") });
    assert_eq!(
        hub.send("POST", "/api/v1/projects", None, Some(&body))
            .status,
        401
    );
    assert_eq!(
        hub.send("POST", "/api/v1/projects", Some("wrong"), Some(&body))
            .status,
        401
    );
    let resp = hub.post(PEER, &body);
    assert_eq!(resp.status, 403);
    assert!(error_of(&resp).contains("Owner"));
    // A peer is refused before its body is read.
    assert_eq!(hub.post(PEER, &json!({ "bogus": 1 })).status, 403);
    assert!(
        !hub.work("nope").exists(),
        "a refused request makes no directory"
    );
    assert!(!hub.config().exists(), "or a config");
    assert_eq!(hub.listed(), ["ferroloop"]);
}

#[test]
fn a_request_that_cannot_be_used_is_a_client_error_and_changes_nothing() {
    let hub = Hub::new();
    let work = hub.work("x");
    let cases: Vec<(Value, u16, &str)> = vec![
        (
            json!({ "path": work, "name": "Bad Name" }),
            400,
            "project name",
        ),
        (json!({ "path": work, "name": "1abc" }), 400, "project name"),
        (json!({ "path": work, "github": "nope" }), 400, "owner/repo"),
        (
            json!({ "path": work, "github": "a/b", "credential": "token" }),
            400,
            "credential",
        ),
        (json!({ "path": work, "credential": "env" }), 400, "github"),
        (json!({ "path": work, "purpose": "a\nb" }), 400, "purpose"),
        (json!({ "path": "relative/dir" }), 400, "absolute"),
        (json!({ "path": "" }), 400, "empty"),
        (json!({ "path": "/tmp/a\u{0}b" }), 400, "NUL"),
        (json!({ "path": hub.base.join("a/../b") }), 400, ".."),
        (json!({ "path": "/etc/fl-project" }), 400, "protected"),
        (json!({ "path": "/" }), 400, "home"),
        (json!({ "path": hub.base.join("home") }), 400, "home"),
        (
            json!({ "path": hub.base.join("home/.ssh/proj") }),
            400,
            "protected",
        ),
        (json!({}), 400, "path"),
        (json!({ "path": work, "extra": 1 }), 400, "extra"),
    ];
    for (body, status, why) in cases {
        let resp = hub.post(OWNER, &body);
        assert_eq!(
            resp.status,
            status,
            "{body}: {}",
            String::from_utf8_lossy(&resp.body)
        );
        assert!(error_of(&resp).contains(why), "{body}: {}", error_of(&resp));
    }
    let not_json = HttpRequest::new("POST", "/api/v1/projects")
        .with_header("Authorization", format!("Bearer {OWNER}"))
        .with_body(b"{not json".to_vec());
    assert_eq!(handle_http_request(&hub.state, &not_json).status, 400);

    let file = hub.base.join("a-file");
    fs::write(&file, "x").unwrap();
    assert_eq!(hub.post(OWNER, &json!({ "path": file })).status, 400);

    assert!(!work.exists());
    assert!(!hub.config().exists());
    assert_eq!(hub.listed(), ["ferroloop"]);
}

#[test]
fn a_name_in_use_at_another_root_is_a_409() {
    let hub = Hub::new();
    assert_eq!(
        hub.post(OWNER, &json!({ "path": hub.work("one"), "name": "taken" }))
            .status,
        201
    );
    let config = fs::read_to_string(hub.config()).unwrap();
    let resp = hub.post(OWNER, &json!({ "path": hub.work("two"), "name": "taken" }));
    assert_eq!(resp.status, 409);
    assert!(error_of(&resp).contains("already registered"));
    assert!(!hub.work("two").exists());
    assert_eq!(fs::read_to_string(hub.config()).unwrap(), config);
    assert_eq!(hub.listed(), ["ferroloop", "taken"]);
}

#[test]
fn the_hub_announces_the_new_project() {
    let hub = Hub::new();
    let mut frames = hub.state.subscribe();
    assert_eq!(
        hub.post(OWNER, &json!({ "path": hub.work("news") })).status,
        201
    );
    let frame = frames.try_recv().unwrap();
    assert_eq!(
        (frame.channel.as_str(), frame.event.as_str()),
        ("projects", "project.created")
    );
    assert_eq!(frame.data["id"], "news");
}
