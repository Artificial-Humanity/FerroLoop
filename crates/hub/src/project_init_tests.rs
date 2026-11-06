//! Project initialization: the shared flow and `POST /api/v1/projects`. Every path lives under
//! a temporary directory, and the flow reads no environment variable, so nothing here reaches
//! a real config or data directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use fl_core::routing::{Routes, Tier};
use fl_core::store::{AgentRegistry, Catalog, Tracker};
use fl_store::AnyStore;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::project_init::{
    Credential, DEFAULT_ROUTING_AREAS, INITIAL_COMMIT_MESSAGE, InitError, InitOutcome, InitPaths,
    InitRequest, ProjectKind, VirtualRequest, create_virtual, has_git, init_project,
    user_config_path,
};
use crate::project_list::{ProjectSummary, UpdateRequest, list_projects, update_project};
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
            slug: None,
            display_name: None,
            github: None,
            credential: None,
            purpose: None,
            git_init: true,
            by: "tester".into(),
        }
    }

    fn open_ws(&self) -> Result<Box<AnyStore>, InitError> {
        fl_store::open_store(&self.paths.workspace_store)
            .map(Box::new)
            .map_err(|e| InitError::Failed(format!("workspace store: {e}")))
    }

    fn open_project(path: &Path) -> Result<Box<AnyStore>, InitError> {
        fl_store::open_store(path)
            .map(Box::new)
            .map_err(|e| InitError::Failed(format!("project store: {e}")))
    }

    fn init(&self, req: &InitRequest) -> Result<InitOutcome, InitError> {
        init_project(
            req,
            &self.paths,
            || self.open_ws(),
            |r| Self::open_project(&r.store),
        )
    }

    fn virtual_group(&self, req: &VirtualRequest) -> Result<InitOutcome, InitError> {
        create_virtual(req, &self.paths, || self.open_ws())
    }

    fn vrequest(&self, name: &str) -> VirtualRequest {
        VirtualRequest {
            slug: None,
            display_name: name.into(),
            purpose: None,
            by: "tester".into(),
        }
    }

    fn list(&self) -> Vec<ProjectSummary> {
        {
            let ws = self.open_ws().unwrap();
            let held = ws.list_projects().unwrap();
            list_projects(&self.paths.config_path, &[], &held, &*ws).unwrap()
        }
    }

    fn update(&self, slug: &str, req: &UpdateRequest) -> Result<ProjectSummary, InitError> {
        update_project(
            slug,
            req,
            &self.paths,
            &[],
            || self.open_ws(),
            Self::open_project,
        )
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

fn config_invalid(r: Result<InitOutcome, InitError>) -> String {
    match r {
        Err(InitError::ConfigInvalid(m)) => m,
        other => panic!("expected ConfigInvalid, got {other:?}"),
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

    assert_eq!(out.slug, "alpha");
    assert_eq!(
        out.root.as_deref(),
        Some(root.display().to_string().as_str())
    );
    assert!(out.registered);
    assert!(out.git.as_ref().unwrap().initialized && out.git.as_ref().unwrap().initial_commit);
    assert!(root.join(".git").exists());
    assert_eq!(
        git(&root, &["rev-parse", "HEAD"]),
        out.git.as_ref().unwrap().head
    );
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
    assert_eq!(blocks[0]["root"].as_str(), out.root.as_deref());
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
    assert!(out.git.as_ref().unwrap().initialized && out.git.as_ref().unwrap().initial_commit);
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
    assert!(!out.git.as_ref().unwrap().initialized && !out.git.as_ref().unwrap().initial_commit);
    assert_eq!(out.git.as_ref().unwrap().head, head);
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
    assert!(
        !out.git.as_ref().unwrap().initialized,
        "the repository was already there"
    );
    assert!(out.git.as_ref().unwrap().initial_commit);
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
        sb.init(&sb.request(&sb.dir("My-App"))).unwrap().slug,
        "my-app"
    );

    let mut req = sb.request(&sb.dir("dir-two"));
    req.slug = Some("Chosen_Name".into());
    let out = sb.init(&req).unwrap();
    assert_eq!(out.slug, "chosen_name");
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
        req.slug = Some(bad.into());
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
    assert_eq!(second.git.as_ref().unwrap().head, head);
    assert!(
        !second.git.as_ref().unwrap().initialized && !second.git.as_ref().unwrap().initial_commit
    );
    assert!(!second.mcp_catalog_created);
    assert_eq!(sb.config(), config, "the config is byte for byte the same");
    assert_eq!(sb.blocks().len(), 1);
    assert_eq!(commit_count(&root), 1);

    let store = fl_store::open_store(&sb.paths.store_dir.join("again.fls")).unwrap();
    assert_eq!(store.list_projects().unwrap().len(), 1);
    // With no name given, the name the config already gives the root is used.
    let mut other = sb.request(&root);
    other.slug = None;
    assert_eq!(sb.init(&other).unwrap().slug, "again");
}

#[test]
fn a_name_taken_at_another_root_is_refused_and_nothing_is_made() {
    let sb = Sandbox::new();
    let mut req = sb.request(&sb.dir("first"));
    req.slug = Some("shared".into());
    sb.init(&req).unwrap();
    let config = sb.config();

    let second = sb.dir("second");
    let mut clash = sb.request(&second);
    clash.slug = Some("SHARED".into());
    let msg = conflict(sb.init(&clash));
    assert!(msg.contains("`shared` is already registered"), "{msg}");
    assert!(msg.contains("--name"), "{msg}");
    assert!(!second.exists(), "a refused request makes no directory");
    assert_eq!(sb.config(), config);

    // The directory name collides the same way.
    let mut by_dir = sb.request(&sb.base.join("elsewhere/shared"));
    by_dir.slug = None;
    conflict(sb.init(&by_dir));

    // And one root cannot be registered under a second name.
    let mut rename = sb.request(&sb.dir("first"));
    rename.slug = Some("renamed".into());
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
        out.store.as_deref(),
        Some(own_store.display().to_string().as_str()),
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
    req.slug = Some("fl".into());
    let msg = conflict(sb.init(&req));
    assert!(msg.contains("workspace store"), "{msg}");
    assert!(!sb.dir("fl").exists());
}

#[test]
fn a_root_that_holds_the_stores_is_refused() {
    let sb = Sandbox::new();
    for root in [sb.base.join("data"), sb.paths.store_dir.join("inside")] {
        let mut req = sb.request(&root);
        req.slug = Some("holder".into());
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
    req.slug = Some("taken".into());
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
    assert_eq!(Some(projects[0].id.iri().as_str()), out.project.as_deref());
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
    assert_eq!(area.path.as_deref(), out.root.as_deref());
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
    let msg = config_invalid(sb.init(&sb.request(&sb.dir("p"))));
    assert!(msg.contains("not valid TOML"), "{msg}");
    assert!(
        msg.contains(&sb.paths.config_path.display().to_string()),
        "{msg}"
    );
    assert_eq!(sb.config(), "this is [not toml");
    assert!(!sb.dir("p").exists());

    fs::write(&sb.paths.config_path, "project = [1, 2]\n").unwrap();
    let msg = config_invalid(sb.init(&sb.request(&sb.dir("p"))));
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
        // The Hub's repository is the test's workspace: a deletion that finds no workspace root
        // refuses to remove a directory.
        fs::create_dir_all(base.join("repo/Notes")).unwrap();
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
            .map(|p| p["slug"].as_str().unwrap().to_string())
            .collect()
    }

    fn get(&self, path: &str) -> HttpResponse {
        self.send("GET", path, Some(OWNER), None)
    }

    fn patch(&self, token: &str, slug: &str, body: &Value) -> HttpResponse {
        self.send(
            "PATCH",
            &format!("/api/v1/projects/{slug}"),
            Some(token),
            Some(body),
        )
    }

    /// The entry of one project in `GET /api/v1/projects`.
    fn entry(&self, slug: &str) -> Value {
        let resp = self.get("/api/v1/projects");
        assert_eq!(resp.status, 200);
        resp.json_value().unwrap()["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["slug"] == slug)
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn workspace(&self) -> crate::HubStoreGuard<'_> {
        self.state.open_store().unwrap()
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

fn text_of(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

// --- git is optional --------------------------------------------------------------------------

#[test]
fn git_is_not_required_and_not_touched_unless_asked() {
    let sb = Sandbox::new();
    let root = sb.dir("nogit");
    let mut req = sb.request(&root);
    req.git_init = false;
    let out = sb.init(&req).unwrap();

    assert!(out.registered);
    assert!(out.git.is_none(), "no git outcome when none was asked for");
    assert!(!root.join(".git").exists(), "no repository was made");
    assert!(root.is_dir() && root.join(".fl/mcp.toml").is_file());
    assert_eq!(sb.blocks().len(), 1);
    let listed = sb.list();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].has_git);

    // A repository with history is left exactly as it is, without --git-init too.
    let repo = sb.dir("history-nogit");
    git_with_history(&repo);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let mut req = sb.request(&repo);
    req.git_init = false;
    assert!(sb.init(&req).unwrap().git.is_none());
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(commit_count(&repo), 2);

    // A repository with no commit stays without one: no `git init`, no initial commit.
    let bare = sb.dir("headless");
    fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q"]);
    let mut req = sb.request(&bare);
    req.git_init = false;
    sb.init(&req).unwrap();
    assert!(
        Command::new("git")
            .args(["rev-parse", "--verify", "-q", "HEAD"])
            .current_dir(&bare)
            .env_remove("GIT_DIR")
            .output()
            .unwrap()
            .stdout
            .is_empty(),
        "still no HEAD"
    );
}

#[test]
fn git_init_runs_git_only_when_asked_and_a_repeat_changes_nothing() {
    let sb = Sandbox::new();
    let root = sb.dir("asked");
    let out = sb.init(&sb.request(&root)).unwrap(); // git_init: true
    let git_outcome = out.git.unwrap();
    assert!(git_outcome.initialized && git_outcome.initial_commit);
    assert!(sb.list()[0].has_git);
    let again = sb.init(&sb.request(&root)).unwrap().git.unwrap();
    assert!(!again.initialized && !again.initial_commit);
    assert_eq!(again.head, git_outcome.head);
    assert_eq!(commit_count(&root), 1);
}

#[test]
fn git_init_gives_a_directory_nested_in_another_repository_its_own_repository() {
    let sb = Sandbox::new();
    let parent = sb.dir("parent");
    fs::create_dir_all(&parent).unwrap();
    git(&parent, &["init", "-q"]);
    git(
        &parent,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "parent",
        ],
    );
    let parent_head = git(&parent, &["rev-parse", "HEAD"]);
    let nested = parent.join("child");
    let out = sb.init(&sb.request(&nested)).unwrap();
    let outcome = out.git.unwrap();
    assert!(
        outcome.initialized && outcome.initial_commit,
        "the nested directory got its own repository: {outcome:?}"
    );
    assert!(nested.join(".git").exists());
    assert_ne!(outcome.head, parent_head);
    assert_eq!(commit_count(&nested), 1);
    assert_eq!(
        commit_count(&parent),
        1,
        "the parent repository is untouched"
    );
    assert_eq!(git(&parent, &["rev-parse", "HEAD"]), parent_head);
    // A repeat changes nothing.
    let again = sb.init(&sb.request(&nested)).unwrap().git.unwrap();
    assert!(!again.initialized && !again.initial_commit);
    assert_eq!(again.head, outcome.head);
}

#[test]
fn has_git_sees_a_repository_with_or_without_a_commit_and_changes_nothing() {
    let tmp = TempDir::new().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let plain = base.join("plain");
    fs::create_dir_all(&plain).unwrap();
    assert!(!has_git(&plain));
    assert!(!has_git(&base.join("missing")));
    let headless = base.join("headless");
    fs::create_dir_all(&headless).unwrap();
    git(&headless, &["init", "-q"]);
    assert!(has_git(&headless), "a repository with no commit counts");
    // A linked worktree's `.git` is a file.
    let linked = base.join("linked");
    fs::create_dir_all(&linked).unwrap();
    fs::write(linked.join(".git"), "gitdir: /elsewhere\n").unwrap();
    assert!(has_git(&linked));
    // Asking changed nothing.
    assert!(!plain.join(".git").exists());
    assert_eq!(fs::read_dir(&plain).unwrap().count(), 0);
}

// --- virtual groups ---------------------------------------------------------------------------

#[test]
fn a_virtual_group_writes_its_area_and_nothing_else() {
    let sb = Sandbox::new();
    let mut req = sb.vrequest("Council");
    req.purpose = Some("the strategists".into());
    let out = sb.virtual_group(&req).unwrap();

    assert_eq!(out.slug, "council");
    assert_eq!(out.kind, ProjectKind::Virtual);
    assert!(out.registered);
    assert!(out.root.is_none() && out.store.is_none() && out.project.is_none());
    assert!(out.git.is_none() && out.config.is_none() && out.areas.is_empty());

    let area = sb.workspace().get_area("council").unwrap().unwrap();
    assert_eq!(area.name, "Council");
    assert_eq!(area.path, None);
    assert!(area.is_virtual());
    assert_eq!(area.purpose, "the strategists");
    assert_eq!(area.by, "tester");

    // Nothing else exists: no config, no per-project store, no directory but the one the
    // workspace store lives in.
    assert!(!sb.paths.config_path.exists(), "no config file");
    assert!(!sb.paths.config_path.parent().unwrap().exists());
    let made: Vec<String> = fs::read_dir(&sb.paths.store_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(made, ["fl.fls"], "only the workspace store");
    assert!(!sb.base.join("work").exists(), "no project directory");

    let listed = sb.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].kind, ProjectKind::Virtual);
    assert_eq!(listed[0].name, "Council");
    assert!(listed[0].path.is_none() && listed[0].store_path.is_none() && !listed[0].has_git);
}

#[test]
fn a_virtual_group_is_idempotent_and_updates_what_it_is_given() {
    let sb = Sandbox::new();
    let mut req = sb.vrequest("Ops Team");
    req.purpose = Some("on call".into());
    let first = sb.virtual_group(&req).unwrap();
    assert_eq!(first.slug, "ops-team");
    // A new name keeps the slug only when the slug is given: the slug is what names the group.
    req.slug = Some("ops-team".into());
    let area = sb.workspace().get_area("ops-team").unwrap().unwrap();

    let second = sb.virtual_group(&req).unwrap();
    assert!(!second.registered);
    assert_eq!(
        sb.workspace().get_area("ops-team").unwrap().unwrap(),
        area,
        "an identical request does not even rewrite the area"
    );
    // No purpose keeps the group's; a new purpose and name replace them.
    req.purpose = None;
    assert_eq!(sb.virtual_group(&req).unwrap().purpose, "on call");
    req.purpose = Some("pager duty".into());
    req.display_name = "Ops".into();
    assert!(!sb.virtual_group(&req).unwrap().registered);
    let area = sb.workspace().get_area("ops-team").unwrap().unwrap();
    assert_eq!(
        (area.name.as_str(), area.purpose.as_str()),
        ("Ops", "pager duty")
    );
    assert_eq!(sb.workspace().list_areas().unwrap().len(), 1);
}

#[test]
fn a_config_that_is_not_toml_is_a_500_naming_the_file_for_every_route() {
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "path": hub.work("alpha") }));
    let config = hub.config();
    fs::write(&config, "this is [not toml").unwrap();
    let named = config.display().to_string();
    let check = |what: &str, resp: HttpResponse| {
        assert_eq!(resp.status, 500, "{what}: {}", text_of(&resp));
        let msg = error_of(&resp);
        assert!(
            msg.contains(&named) && msg.contains("not valid TOML"),
            "{what}: {msg}"
        );
    };
    check("GET", hub.get("/api/v1/projects"));
    check(
        "POST",
        hub.post(OWNER, &json!({ "path": hub.work("beta") })),
    );
    check(
        "POST virtual",
        hub.post(OWNER, &json!({ "kind": "virtual", "name": "Council" })),
    );
    check(
        "PATCH",
        hub.patch(OWNER, "alpha", &json!({ "name": "Renamed" })),
    );
    check("DELETE", hub.delete(OWNER, "alpha", None));
    assert_eq!(fs::read_to_string(&config).unwrap(), "this is [not toml");
    assert!(!hub.work("beta").exists());
}

#[test]
fn a_virtual_groups_slug_is_explicit_or_derived_and_checked() {
    let sb = Sandbox::new();
    let mut req = sb.vrequest("Anything Goes");
    req.slug = Some("ag".into());
    assert_eq!(sb.virtual_group(&req).unwrap().slug, "ag");
    for (name, slug, why) in [
        ("Bad", Some("Has Space"), "project name"),
        ("Bad", Some("9lives"), "project name"),
        ("9 lives", None, "cannot derive a slug"),
        ("   ", None, "name"),
        ("two\nlines", None, "name"),
        ("all", None, "@all"),
        ("Everyone", Some("all"), "@all"),
        ("fl", None, "`fl`"),
    ] {
        let mut req = sb.vrequest(name);
        req.slug = slug.map(String::from);
        let msg = match sb.virtual_group(&req) {
            Err(InitError::Invalid(m)) => m,
            other => panic!("{name:?}/{slug:?}: expected Invalid, got {other:?}"),
        };
        assert!(msg.contains(why), "{name:?}/{slug:?}: {msg}");
    }
    let mut long = sb.vrequest("ok");
    long.display_name = "x".repeat(81);
    assert!(matches!(
        sb.virtual_group(&long),
        Err(InitError::Invalid(_))
    ));
    let mut bad_purpose = sb.vrequest("fine");
    bad_purpose.purpose = Some("a\nb".into());
    assert!(matches!(
        sb.virtual_group(&bad_purpose),
        Err(InitError::Invalid(_))
    ));
    assert_eq!(sb.workspace().list_areas().unwrap().len(), 1, "only `ag`");
}

#[test]
fn a_group_is_refused_on_a_slug_the_list_shows_as_a_project() {
    let sb = Sandbox::new();
    // The default project is a project though it has no config block and no store here.
    let mut req = sb.vrequest("ferroloop");
    req.slug = Some("ferroloop".into());
    let why = conflict(sb.virtual_group(&req));
    assert!(why.contains("already a project"), "{why}");
    // A project the workspace store holds a row for (`fl project add`).
    sb.workspace().add_project("/work/held-one").unwrap();
    let mut req = sb.vrequest("Held One");
    req.slug = Some("held-one".into());
    let why = conflict(sb.virtual_group(&req));
    assert!(why.contains("already a project"), "{why}");
    assert!(sb.workspace().get_area("ferroloop").unwrap().is_none());
    assert!(sb.workspace().get_area("held-one").unwrap().is_none());
    // A name no project has still makes a group.
    sb.virtual_group(&sb.vrequest("Council")).unwrap();
}

#[test]
fn a_project_does_not_re_point_an_area_that_records_another_directory() {
    let sb = Sandbox::new();
    let elsewhere = sb.dir("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    // `alpha` is an area for `elsewhere` (made by `fl agent register`), with no config block.
    sb.workspace()
        .upsert_area(fl_core::agent::AreaRecord {
            area_key: "alpha".into(),
            name: "Alpha".into(),
            path: Some(elsewhere.display().to_string()),
            purpose: "p".into(),
            updated_at: fl_core::At::from_unix_millis(1),
            by: "t".into(),
        })
        .unwrap();
    let root = sb.dir("alpha");
    let why = conflict(sb.init(&sb.request(&root)));
    assert!(
        why.contains("already an area") && why.contains("elsewhere"),
        "{why}"
    );
    assert!(!root.exists(), "nothing was made");
    assert!(!sb.paths.config_path.exists());
    let area = sb.workspace().get_area("alpha").unwrap().unwrap();
    assert_eq!(area.path.as_deref(), Some(elsewhere.to_str().unwrap()));
    // The same directory is the same project: it registers.
    let mut same = sb.request(&elsewhere);
    same.slug = Some("alpha".into());
    sb.init(&same).unwrap();
    assert_eq!(
        sb.workspace()
            .get_area("alpha")
            .unwrap()
            .unwrap()
            .path
            .as_deref(),
        Some(elsewhere.to_str().unwrap())
    );
}

#[test]
fn a_slug_names_one_kind() {
    let sb = Sandbox::new();
    // A virtual group first: a project onto its slug is refused, with a way out, before
    // anything is made.
    sb.virtual_group(&sb.vrequest("Taken")).unwrap();
    let root = sb.dir("taken");
    let msg = conflict(sb.init(&sb.request(&root)));
    assert!(msg.contains("virtual group"), "{msg}");
    assert!(
        msg.contains("fl agent area set taken --path"),
        "the way out is named: {msg}"
    );
    assert!(!root.exists(), "no directory");
    assert!(sb.blocks().is_empty(), "no config block");
    assert!(!sb.paths.store_dir.join("taken.fls").exists(), "no store");

    // The way out works: give the area a path and the project can be made.
    let mut area = sb.workspace().get_area("taken").unwrap().unwrap();
    area.path = Some(root.display().to_string());
    sb.workspace().upsert_area(area).unwrap();
    sb.init(&sb.request(&root)).unwrap();
    assert_eq!(sb.list()[0].kind, ProjectKind::Project);

    // The other way round: a virtual group onto a project's slug is refused ...
    let msg = conflict(sb.virtual_group(&sb.vrequest("Taken")));
    assert!(msg.contains("already"), "{msg}");
    // ... whether the project is in the config, only a store file, or only an area with a path.
    let mut by_config = sb.vrequest("x");
    by_config.slug = Some("taken".into());
    assert!(conflict(sb.virtual_group(&by_config)).contains("directory-backed project"));
    fs::write(sb.paths.store_dir.join("stored.fls"), "x").unwrap();
    let mut by_store = sb.vrequest("Stored");
    by_store.slug = Some("stored".into());
    assert!(conflict(sb.virtual_group(&by_store)).contains("store"));
    sb.workspace()
        .upsert_area(AreaRecordFixture::with_path("pathy", "/work/pathy"))
        .unwrap();
    let mut by_area = sb.vrequest("Pathy");
    by_area.slug = Some("pathy".into());
    assert!(conflict(sb.virtual_group(&by_area)).contains("/work/pathy"));
    assert_eq!(sb.blocks().len(), 1, "the config is as it was");
}

/// Areas as tests need them.
struct AreaRecordFixture;

impl AreaRecordFixture {
    fn with_path(key: &str, path: &str) -> fl_core::agent::AreaRecord {
        fl_core::agent::AreaRecord {
            area_key: key.into(),
            name: key.into(),
            path: Some(path.into()),
            purpose: "p".into(),
            updated_at: fl_core::at::At::from_unix_millis(1),
            by: "t".into(),
        }
    }
}

#[test]
fn a_display_name_gives_the_slug_unless_a_slug_or_the_config_says() {
    let sb = Sandbox::new();
    let mut req = sb.request(&sb.dir("whatever"));
    req.display_name = Some("My Project".into());
    let out = sb.init(&req).unwrap();
    assert_eq!(out.slug, "my-project");
    let listed = sb.list();
    assert_eq!(
        (listed[0].slug.as_str(), listed[0].name.as_str()),
        ("my-project", "My Project")
    );

    // Naming it again without a slug keeps the slug the config has, and the new name shows.
    let mut again = sb.request(&sb.dir("whatever"));
    again.display_name = Some("Renamed Entirely".into());
    assert_eq!(sb.init(&again).unwrap().slug, "my-project");
    assert_eq!(sb.list()[0].name, "Renamed Entirely");

    // An explicit slug wins over the name; a name that gives no slug asks for one.
    let mut explicit = sb.request(&sb.dir("other"));
    explicit.display_name = Some("Pretty Name".into());
    explicit.slug = Some("plain".into());
    assert_eq!(sb.init(&explicit).unwrap().slug, "plain");
    let mut hopeless = sb.request(&sb.dir("third"));
    hopeless.display_name = Some("1 2 3".into());
    assert!(invalid(sb.init(&hopeless)).contains("cannot derive a slug"));
    let mut long = sb.request(&sb.dir("fourth"));
    long.display_name = Some("n".repeat(81));
    assert!(invalid(sb.init(&long)).contains("name"));
}

// --- the list ---------------------------------------------------------------------------------

fn register(
    sb: &Sandbox,
    name: &str,
    project: &str,
    extra: &[&str],
    role: fl_core::agent::AgentRole,
) -> fl_core::agent::AgentRecord {
    sb.workspace()
        .register_agent(fl_core::agent::RegisterAgentArgs {
            display_name: name.into(),
            email: None,
            project: project.into(),
            role,
            extra_areas: extra.iter().map(|s| s.to_string()).collect(),
            create_area: true,
            replace: false,
            at: fl_core::at::At::from_unix_millis(5),
            by: "t".into(),
        })
        .unwrap()
}

#[test]
fn the_list_has_both_kinds_by_slug_with_their_facts() {
    use fl_core::agent::AgentRole;
    let sb = Sandbox::new();
    let mut a = sb.request(&sb.dir("alpha"));
    a.github = Some("acme/alpha".into());
    a.credential = Some(Credential::Env);
    a.purpose = Some("the alpha".into());
    sb.init(&a).unwrap();
    let mut b = sb.request(&sb.dir("beta"));
    b.git_init = false;
    sb.init(&b).unwrap();
    let mut g = sb.vrequest("Council");
    g.purpose = Some("seats".into());
    sb.virtual_group(&g).unwrap();
    register(&sb, "ada", "alpha", &[], AgentRole::Lead);
    register(&sb, "bea", "council", &["alpha"], AgentRole::Strategist);
    register(&sb, "cy", "council", &[], AgentRole::Journalist);
    let retired = register(&sb, "di", "alpha", &[], AgentRole::Tester);
    sb.workspace()
        .retire_agent(&retired.id, fl_core::at::At::from_unix_millis(9), "t")
        .unwrap();

    let listed = sb.list();
    let slugs: Vec<_> = listed.iter().map(|p| p.slug.as_str()).collect();
    assert_eq!(slugs, ["alpha", "beta", "council"]);
    let alpha = &listed[0];
    assert_eq!(alpha.kind, ProjectKind::Project);
    assert_eq!(
        alpha.path.as_deref(),
        Some(sb.dir("alpha").to_str().unwrap())
    );
    assert!(alpha.has_git);
    assert_eq!(alpha.purpose, "the alpha");
    assert_eq!(
        alpha
            .tracker
            .as_ref()
            .map(|t| (t.github.as_str(), t.credential)),
        Some(("acme/alpha", Credential::Env))
    );
    assert_eq!(
        alpha.store_path.as_deref(),
        Some(sb.paths.store_dir.join("alpha.fls").to_str().unwrap())
    );
    // ada (project) and bea (extra area) count; the retired agent does not.
    assert_eq!(alpha.active_agent_count, 2);
    assert!(!listed[1].has_git && listed[1].tracker.is_none());
    assert_eq!(listed[1].active_agent_count, 0);
    let council = &listed[2];
    assert_eq!(council.kind, ProjectKind::Virtual);
    assert_eq!(council.active_agent_count, 2);
    assert!(council.path.is_none() && council.store_path.is_none());
}

#[test]
fn a_store_known_to_the_caller_is_a_project_even_without_a_config_block() {
    let sb = Sandbox::new();
    sb.virtual_group(&sb.vrequest("Ghost")).unwrap();
    let hints = vec![
        ("ferroloop".to_string(), None),
        (
            "ghost".to_string(),
            Some(sb.paths.store_dir.join("ghost.fls")),
        ),
    ];
    let listed =
        list_projects(&sb.paths.config_path, &hints, &[], &*sb.open_ws().unwrap()).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].slug, "ferroloop");
    assert_eq!(listed[0].kind, ProjectKind::Project);
    assert!(listed[0].path.is_none() && listed[0].store_path.is_none());
    // A store file beats a same-named area with no path: one slug, one kind.
    assert_eq!(listed[1].slug, "ghost");
    assert_eq!(listed[1].kind, ProjectKind::Project);
    assert!(listed[1].store_path.is_some());
}

// --- the list reports the physical store file -------------------------------------------------

/// A config with one hand-written `[[project]]` block, `slug`, whose `store` names `configured`
/// (a legacy `.redb` alias, say).
fn config_with_store(sb: &Sandbox, slug: &str, configured: &Path) {
    let root = sb.dir(slug);
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(sb.paths.config_path.parent().unwrap()).unwrap();
    fs::write(
        &sb.paths.config_path,
        format!(
            "[[project]]\nname = \"{slug}\"\nroot = \"{}\"\nstore = \"{}\"\n",
            root.display(),
            configured.display()
        ),
    )
    .unwrap();
}

fn listed_store(sb: &Sandbox, slug: &str) -> Option<String> {
    sb.list()
        .into_iter()
        .find(|p| p.slug == slug)
        .unwrap_or_else(|| panic!("no project {slug}"))
        .store_path
}

/// A real FerroStore file at `fls` (the physical store of the default backend).
fn make_fls(fls: &Path) {
    drop(fl_store::open_store(fls).unwrap());
    assert!(fls.is_file());
}

/// A real redb file at `redb`.
fn make_redb(redb: &Path) {
    drop(fl_store::open_redb(redb).unwrap());
    assert!(redb.is_file());
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_redb_alias_in_the_config_is_listed_as_the_fls_that_exists_on_disk() {
    let sb = Sandbox::new();
    let dir = sb.base.join("stores");
    fs::create_dir_all(&dir).unwrap();
    let redb = dir.join("site.redb");
    let fls = dir.join("site.fls");
    make_fls(&fls);
    config_with_store(&sb, "site", &redb);

    assert_eq!(listed_store(&sb, "site"), Some(fls.display().to_string()));
    assert!(!redb.exists(), "listing makes no .redb");
}

#[test]
fn a_redb_alias_with_both_files_beside_it_is_listed_as_the_fls() {
    let sb = Sandbox::new();
    let dir = sb.base.join("stores");
    fs::create_dir_all(&dir).unwrap();
    let redb = dir.join("site.redb");
    let fls = dir.join("site.fls");
    make_redb(&redb);
    make_fls(&fls);
    config_with_store(&sb, "site", &redb);

    assert_eq!(listed_store(&sb, "site"), Some(fls.display().to_string()));
}

#[test]
fn a_lone_unmigrated_redb_is_listed_as_the_redb_and_listing_touches_nothing() {
    let sb = Sandbox::new();
    let dir = sb.base.join("stores");
    fs::create_dir_all(&dir).unwrap();
    let redb = dir.join("site.redb");
    make_redb(&redb);
    config_with_store(&sb, "site", &redb);
    // The workspace store is opened (and made) by the list's caller, not by the list itself.
    drop(sb.open_ws().unwrap());
    let bytes = fs::read(&redb).unwrap();
    let modified = fs::metadata(&redb).unwrap().modified().unwrap();
    let names = file_names(&dir);

    assert_eq!(listed_store(&sb, "site"), Some(redb.display().to_string()));

    let fls = dir.join("site.fls");
    assert!(!fls.exists(), "listing must not migrate the redb");
    assert!(
        !fl_store::MigrationLock::path_for(&fls).exists(),
        "listing must not take the migration lock"
    );
    assert_eq!(file_names(&dir), names, "listing created or removed a file");
    assert_eq!(fs::read(&redb).unwrap(), bytes);
    assert_eq!(fs::metadata(&redb).unwrap().modified().unwrap(), modified);
}

#[test]
fn a_redb_alias_with_nothing_on_disk_is_listed_as_the_fls_it_would_be_created_at() {
    let sb = Sandbox::new();
    let dir = sb.base.join("stores");
    fs::create_dir_all(&dir).unwrap();
    config_with_store(&sb, "site", &dir.join("site.redb"));

    assert_eq!(
        listed_store(&sb, "site"),
        Some(dir.join("site.fls").display().to_string())
    );
    assert!(file_names(&dir).is_empty(), "listing creates nothing");
}

#[test]
fn a_scanned_hint_and_a_stores_only_project_are_listed_by_their_physical_file() {
    let sb = Sandbox::new();
    let dir = sb.base.join("stores");
    fs::create_dir_all(&dir).unwrap();
    let hint_fls = dir.join("hinted.fls");
    make_fls(&hint_fls);
    // A config block with no `store`: the hint names the file, here as a `.redb` alias.
    let root = sb.dir("hinted");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(sb.paths.config_path.parent().unwrap()).unwrap();
    fs::write(
        &sb.paths.config_path,
        format!(
            "[[project]]\nname = \"hinted\"\nroot = \"{}\"\n",
            root.display()
        ),
    )
    .unwrap();
    let only_fls = dir.join("only.fls");
    make_fls(&only_fls);
    let hints = vec![
        ("hinted".to_string(), Some(dir.join("hinted.redb"))),
        ("only".to_string(), Some(dir.join("only.redb"))),
    ];
    let listed =
        list_projects(&sb.paths.config_path, &hints, &[], &*sb.open_ws().unwrap()).unwrap();
    let store = |slug: &str| {
        listed
            .iter()
            .find(|p| p.slug == slug)
            .unwrap()
            .store_path
            .clone()
    };
    assert_eq!(store("hinted"), Some(hint_fls.display().to_string()));
    assert_eq!(store("only"), Some(only_fls.display().to_string()));
}

#[test]
fn effective_store_path_follows_the_backend_and_the_files_on_disk() {
    use crate::project_list::effective_store_path_with as eff;
    let tmp = TempDir::new().unwrap();
    let dir = fs::canonicalize(tmp.path()).unwrap();
    let redb = dir.join("x.redb");
    let fls = dir.join("x.fls");

    // The explicit redb override keeps the configured file, whatever is on disk.
    assert_eq!(eff(&redb, Some("redb")), redb);
    make_fls(&fls);
    assert_eq!(eff(&redb, Some("redb")), redb);
    assert_eq!(eff(&redb, Some(" REDB ")), redb);
    // An `.fls` path is ferro whatever the environment says.
    assert_eq!(eff(&fls, Some("redb")), fls);
    assert_eq!(eff(&fls, None), fls);
    assert_eq!(eff(&fls, Some("ferro")), fls);
    // Default and explicit ferro map the alias onto the `.fls`.
    assert_eq!(eff(&redb, None), fls);
    assert_eq!(eff(&redb, Some("ferro")), fls);
    assert_eq!(eff(&redb, Some("")), fls);
    // An environment value that names no engine leaves the path as configured.
    assert_eq!(eff(&redb, Some("sqlite")), redb);
    assert_eq!(
        eff(Path::new("/nowhere/y.redb"), Some("bogus")),
        Path::new("/nowhere/y.redb")
    );
    // A name that is no store file's stays as configured (the delete flow refuses it by name).
    assert_eq!(eff(&dir.join("x.txt"), None), dir.join("x.txt"));
    assert_eq!(eff(&dir.join("x"), None), dir.join("x"));
    // Nothing is made by asking.
    assert_eq!(file_names(&dir), ["x.fls"]);
}

#[test]
fn effective_store_path_keeps_a_lone_legacy_file_and_is_idempotent() {
    use crate::project_list::effective_store_path_with as eff;
    let tmp = TempDir::new().unwrap();
    let dir = fs::canonicalize(tmp.path()).unwrap();
    let redb = dir.join("x.redb");
    make_redb(&redb);
    assert_eq!(
        eff(&redb, None),
        redb,
        "a lone legacy redb is what is on disk"
    );
    // A directory or a missing file is not "a file on disk": the `.fls` is where it would be.
    let odd = dir.join("odd.redb");
    fs::create_dir(&odd).unwrap();
    assert_eq!(eff(&odd, None), dir.join("odd.fls"));
    assert_eq!(eff(&dir.join("gone.redb"), None), dir.join("gone.fls"));
    // Idempotent.
    for p in [&redb, &odd, &dir.join("gone.redb"), &dir.join("z.fls")] {
        let once = eff(p, None);
        assert_eq!(eff(&once, None), once, "{}", p.display());
    }
}

#[test]
fn a_virtual_group_and_a_project_with_no_store_keep_a_null_store_path() {
    let sb = Sandbox::new();
    sb.virtual_group(&sb.vrequest("Council")).unwrap();
    let root = sb.dir("bare");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(sb.paths.config_path.parent().unwrap()).unwrap();
    fs::write(
        &sb.paths.config_path,
        format!(
            "[[project]]\nname = \"bare\"\nroot = \"{}\"\n",
            root.display()
        ),
    )
    .unwrap();
    assert_eq!(listed_store(&sb, "council"), None);
    assert_eq!(listed_store(&sb, "bare"), None);
}

#[test]
fn the_agent_matcher_ignores_case_matches_the_area_name_and_skips_the_inactive() {
    use crate::project_list::agent_in_project;
    use fl_core::agent::{AgentRecord, AgentRole, AgentStatus};
    use fl_core::ids::AgentId;
    let agent = |project: &str, extra: &[&str], status| AgentRecord {
        id: AgentId(fl_core::ids::seq_iri(1)),
        name_key: "a".into(),
        display_name: "a".into(),
        email: "a@artificialhumanity.io".into(),
        project: project.into(),
        role: AgentRole::Tester,
        extra_areas: extra.iter().map(|s| s.to_string()).collect(),
        status,
        updated_at: fl_core::at::At::from_unix_millis(1),
        by: "t".into(),
    };
    for stored in ["ferroloop", "FerroLoop", "FERROLOOP", " ferroloop "] {
        assert!(
            agent_in_project(&agent(stored, &[], AgentStatus::Active), "ferroloop", None),
            "{stored:?}"
        );
    }
    assert!(!agent_in_project(
        &agent("sonora", &[], AgentStatus::Active),
        "ferroloop",
        None
    ));
    assert!(!agent_in_project(
        &agent("ferroloop2", &[], AgentStatus::Active),
        "ferroloop",
        None
    ));
    // The area's name matches too, as spelt in the registry.
    let named = agent("Ferro-Loop", &[], AgentStatus::Active);
    assert!(!agent_in_project(&named, "ferroloop", None));
    assert!(agent_in_project(&named, "ferroloop", Some("ferro-LOOP")));
    // An extra area counts; a retired agent never does.
    assert!(agent_in_project(
        &agent("sonora", &["FerroLoop"], AgentStatus::Active),
        "ferroloop",
        None
    ));
    assert!(!agent_in_project(
        &agent("ferroloop", &[], AgentStatus::Retired),
        "ferroloop",
        None
    ));
}

#[test]
fn the_active_agent_count_uses_the_matcher() {
    use fl_core::agent::{AgentRole, AreaRecord};
    let sb = Sandbox::new();
    sb.init(&sb.request(&sb.dir("ferroloop"))).unwrap();
    sb.init(&sb.request(&sb.dir("other"))).unwrap();
    // An agent whose project is stored in another case is counted for the slug.
    {
        let ws = sb.workspace();
        for (n, key) in [(1u64, "FerroLoop"), (2, "FERROLOOP")] {
            ws.upsert_area(AreaRecord {
                area_key: key.to_ascii_lowercase(),
                name: key.into(),
                path: Some(sb.dir("ferroloop").display().to_string()),
                purpose: "p".into(),
                updated_at: fl_core::at::At::from_unix_millis(n),
                by: "t".into(),
            })
            .unwrap();
        }
    }
    // Registry rules keep agent projects lowercase, so the mixed case is written through the
    // in-memory list the matcher reads: the count itself is `agent_in_project` over it.
    register(&sb, "ada", "ferroloop", &[], AgentRole::Lead);
    register(&sb, "bea", "other", &[], AgentRole::Lead);
    let listed = sb.list();
    assert_eq!(
        listed
            .iter()
            .find(|p| p.slug == "ferroloop")
            .unwrap()
            .active_agent_count,
        1
    );
    assert_eq!(
        listed
            .iter()
            .find(|p| p.slug == "other")
            .unwrap()
            .active_agent_count,
        1
    );
}

// --- the golden JSON --------------------------------------------------------------------------

#[test]
fn a_project_summary_serializes_with_exactly_these_fields() {
    let project = ProjectSummary {
        slug: "ferroloop".into(),
        name: "FerroLoop".into(),
        kind: ProjectKind::Project,
        path: Some("/abs/root".into()),
        has_git: true,
        purpose: "p".into(),
        tracker: Some(crate::project_init::Tracker {
            github: "owner/repo".into(),
            credential: Credential::App,
        }),
        store_path: Some("/abs/ferroloop.fls".into()),
        active_agent_count: 5,
    };
    assert_eq!(
        serde_json::to_value(&project).unwrap(),
        json!({
            "slug": "ferroloop", "name": "FerroLoop", "kind": "project", "path": "/abs/root",
            "has_git": true, "purpose": "p",
            "tracker": { "kind": "github", "repo": "owner/repo", "credential": "app" },
            "store_path": "/abs/ferroloop.fls", "active_agent_count": 5
        })
    );
    let group = ProjectSummary {
        slug: "council".into(),
        name: "Council".into(),
        kind: ProjectKind::Virtual,
        path: None,
        has_git: false,
        purpose: "p".into(),
        tracker: None,
        store_path: None,
        active_agent_count: 3,
    };
    let value = serde_json::to_value(&group).unwrap();
    assert_eq!(
        value,
        json!({
            "slug": "council", "name": "Council", "kind": "virtual", "path": null,
            "has_git": false, "purpose": "p", "tracker": null, "store_path": null,
            "active_agent_count": 3
        })
    );
    // Null, not absent.
    let object = value.as_object().unwrap();
    for key in ["path", "tracker", "store_path"] {
        assert!(object.contains_key(key), "{key} is present as null");
    }
    assert_eq!(object.len(), 9);
}

// --- PATCH, as a flow -------------------------------------------------------------------------

fn change() -> UpdateRequest {
    UpdateRequest {
        by: "tester".into(),
        ..UpdateRequest::default()
    }
}

#[test]
fn an_update_changes_the_name_and_purpose_and_never_the_slug() {
    let sb = Sandbox::new();
    sb.init(&sb.request(&sb.dir("proj"))).unwrap();
    let config = sb.config();
    let mut req = change();
    req.name = Some("The Project".into());
    req.purpose = Some("new purpose".into());
    let after = sb.update("proj", &req).unwrap();
    assert_eq!(after.slug, "proj");
    assert_eq!(after.name, "The Project");
    assert_eq!(after.purpose, "new purpose");
    let area = sb.workspace().get_area("proj").unwrap().unwrap();
    assert_eq!(
        (area.name.as_str(), area.purpose.as_str()),
        ("The Project", "new purpose")
    );
    assert_eq!(
        area.path.as_deref(),
        Some(sb.dir("proj").to_str().unwrap()),
        "the path stays"
    );
    assert_eq!(sb.config(), config, "the config block is untouched");
    // Repeating it changes nothing at all.
    let stamp = area.updated_at.clone();
    assert_eq!(sb.update("proj", &req).unwrap(), after);
    assert_eq!(
        sb.workspace().get_area("proj").unwrap().unwrap().updated_at,
        stamp
    );
    assert_eq!(sb.config(), config);
}

#[test]
fn an_empty_purpose_resets_it_to_the_default_for_the_kind_and_a_repeat_is_a_no_op() {
    let sb = Sandbox::new();
    let mut with_purpose = sb.request(&sb.dir("proj"));
    with_purpose.purpose = Some("custom".into());
    sb.init(&with_purpose).unwrap();
    let mut group = sb.vrequest("Council");
    group.purpose = Some("the strategists".into());
    sb.virtual_group(&group).unwrap();
    let config = sb.config();
    for (slug, default) in [
        ("proj", "Project proj"),
        ("council", "Virtual group council"),
    ] {
        for blank in ["", "   "] {
            // Put a custom purpose back first, so each blank form is a real reset.
            let mut set = change();
            set.purpose = Some("custom".into());
            assert_eq!(sb.update(slug, &set).unwrap().purpose, "custom");
            let mut reset = change();
            reset.purpose = Some(blank.into());
            let after = sb.update(slug, &reset).unwrap();
            assert_eq!(after.purpose, default, "{slug} {blank:?}");
            let area = sb.workspace().get_area(slug).unwrap().unwrap();
            assert_eq!(area.purpose, default);
            assert_eq!(
                sb.list().into_iter().find(|p| p.slug == slug).unwrap(),
                after,
                "the list says the default"
            );
            // Asking again changes nothing, the stamp included.
            assert_eq!(sb.update(slug, &reset).unwrap(), after);
            assert_eq!(
                sb.workspace().get_area(slug).unwrap().unwrap().updated_at,
                area.updated_at
            );
        }
    }
    assert_eq!(sb.config(), config, "the purpose is not in the config");
}

#[test]
fn a_non_empty_purpose_that_cannot_be_used_is_still_refused_and_changes_nothing() {
    let sb = Sandbox::new();
    let mut with_purpose = sb.request(&sb.dir("proj"));
    with_purpose.purpose = Some("custom".into());
    sb.init(&with_purpose).unwrap();
    for bad in ["a\nb".to_string(), "x".repeat(10_000)] {
        let mut set = change();
        set.purpose = Some(bad);
        assert!(matches!(
            sb.update("proj", &set),
            Err(InitError::Invalid(m)) if m.contains("purpose")
        ));
    }
    assert_eq!(
        sb.workspace().get_area("proj").unwrap().unwrap().purpose,
        "custom"
    );
}

#[test]
fn a_failed_config_write_puts_the_previous_purpose_back_after_a_reset() {
    let sb = Sandbox::new();
    let mut with_purpose = sb.request(&sb.dir("proj"));
    with_purpose.purpose = Some("custom".into());
    sb.init(&with_purpose).unwrap();
    let config = sb.config();
    crate::project_delete::hooks::reset();
    crate::project_delete::hooks::inject(crate::project_delete::hooks::Fault::WriteConfig);
    let mut set = change();
    set.purpose = Some(String::new());
    set.github = Some("acme/one".into());
    let err = sb.update("proj", &set).unwrap_err();
    assert!(matches!(err, InitError::Failed(_)), "{err:?}");
    assert_eq!(
        sb.workspace().get_area("proj").unwrap().unwrap().purpose,
        "custom",
        "the reset is undone with the rest"
    );
    assert_eq!(sb.config(), config);
    crate::project_delete::hooks::reset();
}

#[test]
fn an_update_refuses_what_cannot_be_used_and_changes_nothing() {
    let sb = Sandbox::new();
    sb.init(&sb.request(&sb.dir("proj"))).unwrap();
    sb.virtual_group(&sb.vrequest("Group")).unwrap();
    let config = sb.config();
    let area = sb.workspace().get_area("proj").unwrap().unwrap();
    let bad = |f: &dyn Fn(&mut UpdateRequest)| {
        let mut r = change();
        f(&mut r);
        r
    };
    let cases: Vec<(&str, UpdateRequest, &str)> = vec![
        ("proj", change(), "nothing to change"),
        ("proj", bad(&|r| r.name = Some("  ".into())), "name"),
        ("proj", bad(&|r| r.name = Some("a\nb".into())), "name"),
        ("proj", bad(&|r| r.purpose = Some("a\nb".into())), "purpose"),
        (
            "proj",
            bad(&|r| r.github = Some("nope".into())),
            "owner/repo",
        ),
        (
            "proj",
            bad(&|r| r.credential = Some(Credential::Env)),
            "no tracker",
        ),
        (
            "proj",
            bad(&|r| {
                r.github = Some(String::new());
                r.credential = Some(Credential::Env);
            }),
            "clears",
        ),
        (
            "proj",
            bad(&|r| r.path = Some(PathBuf::from("rel/dir"))),
            "absolute",
        ),
        (
            "proj",
            bad(&|r| r.path = Some(sb.base.join("a/../b"))),
            "..",
        ),
        (
            "proj",
            bad(&|r| r.path = Some(sb.dir("not-there"))),
            "existing directory",
        ),
        (
            "proj",
            bad(&|r| r.path = Some(sb.base.join("data"))),
            "contain one another",
        ),
        ("group", bad(&|r| r.path = Some(sb.base.clone())), "no path"),
        (
            "group",
            bad(&|r| r.github = Some("a/b".into())),
            "no tracker",
        ),
        (
            "group",
            bad(&|r| r.credential = Some(Credential::App)),
            "no tracker",
        ),
    ];
    for (slug, req, why) in cases {
        match sb.update(slug, &req) {
            Err(InitError::Invalid(m)) => assert!(m.contains(why), "{slug}/{why}: {m}"),
            other => panic!("{slug}/{why}: expected Invalid, got {other:?}"),
        }
    }
    let mut blank = change();
    blank.name = Some("x".into());
    blank.by = " ".into();
    assert!(matches!(
        sb.update("proj", &blank),
        Err(InitError::Invalid(_))
    ));
    let mut missing = change();
    missing.name = Some("x".into());
    assert!(matches!(
        sb.update("nosuch", &missing),
        Err(InitError::NotFound(_))
    ));
    assert_eq!(sb.config(), config);
    assert_eq!(sb.workspace().get_area("proj").unwrap().unwrap(), area);
    assert_eq!(
        sb.workspace().get_area("group").unwrap().unwrap().name,
        "Group"
    );
}

#[test]
fn a_failed_config_write_removes_the_area_the_update_made() {
    let sb = Sandbox::new();
    sb.init(&sb.request(&sb.dir("proj"))).unwrap();
    // The project has a config block and a store but no area (a hand-edited registry).
    sb.workspace()
        .delete_area("proj", fl_core::At::from_unix_millis(3), "t")
        .unwrap();
    let config = sb.config();
    crate::project_delete::hooks::reset();
    crate::project_delete::hooks::inject(crate::project_delete::hooks::Fault::WriteConfig);
    let mut set = change();
    set.name = Some("Renamed".into());
    set.github = Some("acme/one".into());
    let err = sb.update("proj", &set).unwrap_err();
    assert!(matches!(err, InitError::Failed(_)), "{err:?}");
    assert!(
        sb.workspace().get_area("proj").unwrap().is_none(),
        "the area the update made is gone again"
    );
    assert_eq!(sb.config(), config);
    // Without the fault the same update makes the area and writes the config.
    crate::project_delete::hooks::reset();
    sb.update("proj", &set).unwrap();
    assert_eq!(
        sb.workspace().get_area("proj").unwrap().unwrap().name,
        "Renamed"
    );
}

#[test]
fn a_move_of_a_project_whose_store_file_is_gone_is_refused_and_makes_no_store() {
    let sb = Sandbox::new();
    let out = sb.init(&sb.request(&sb.dir("proj"))).unwrap();
    let store = PathBuf::from(out.store.unwrap());
    fs::remove_file(&store).unwrap();
    let there = sb.dir("there");
    fs::create_dir_all(&there).unwrap();
    let config = sb.config();
    let mut mv = change();
    mv.path = Some(there.clone());
    let err = sb.update("proj", &mv).unwrap_err();
    assert!(
        matches!(&err, InitError::Conflict(m) if m.contains("does not exist")),
        "{err:?}"
    );
    assert!(!store.exists(), "no empty store was made");
    assert_eq!(sb.config(), config);
    let area = sb.workspace().get_area("proj").unwrap().unwrap();
    assert_eq!(area.path.as_deref(), Some(sb.dir("proj").to_str().unwrap()));
}

#[test]
fn an_update_sets_replaces_and_clears_the_tracker_and_keeps_the_rest_of_the_config() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.paths.config_path.parent().unwrap()).unwrap();
    fs::write(
        &sb.paths.config_path,
        "# my machine\n[github]\napp_id = 7\n\n[[project]]\n# the second one\nname = \"zeta\"\nroot = \"/nowhere/zeta\"\nstore = \"/nowhere/zeta.fls\"\n\n",
    )
    .unwrap();
    sb.init(&sb.request(&sb.dir("proj"))).unwrap();

    let mut set = change();
    set.github = Some("acme/one".into());
    let after = sb.update("proj", &set).unwrap();
    let t = after.tracker.unwrap();
    assert_eq!(
        (t.github.as_str(), t.credential),
        ("acme/one", Credential::App)
    );
    let blocks = sb.blocks();
    assert_eq!(blocks[1]["tracker"]["github"].as_str(), Some("acme/one"));
    assert!(
        sb.config()
            .starts_with("# my machine\n[github]\napp_id = 7\n")
    );
    assert!(sb.config().contains("# the second one"));

    // The credential alone changes the credential; a replacement keeps it unless one is given.
    let mut cred = change();
    cred.credential = Some(Credential::Env);
    assert_eq!(
        sb.update("proj", &cred)
            .unwrap()
            .tracker
            .unwrap()
            .credential,
        Credential::Env
    );
    let mut replace = change();
    replace.github = Some("acme/two".into());
    let t = sb.update("proj", &replace).unwrap().tracker.unwrap();
    assert_eq!(
        (t.github.as_str(), t.credential),
        ("acme/two", Credential::Env)
    );
    let mut both = change();
    both.github = Some("acme/three".into());
    both.credential = Some(Credential::App);
    let t = sb.update("proj", &both).unwrap().tracker.unwrap();
    assert_eq!(
        (t.github.as_str(), t.credential),
        ("acme/three", Credential::App)
    );

    // Repeating a request is a no-op, byte for byte.
    let text = sb.config();
    sb.update("proj", &both).unwrap();
    assert_eq!(sb.config(), text);

    let mut clear = change();
    clear.github = Some(String::new());
    assert!(sb.update("proj", &clear).unwrap().tracker.is_none());
    assert!(sb.blocks()[1].get("tracker").is_none());
    assert_eq!(sb.blocks().len(), 2, "the other block stayed");
    assert!(sb.config().contains("# the second one"));
}

#[test]
fn a_credential_change_keeps_the_trackers_other_keys() {
    let sb = Sandbox::new();
    let root = sb.dir("led");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(sb.paths.config_path.parent().unwrap()).unwrap();
    fs::write(
        &sb.paths.config_path,
        format!(
            "[[project]]\nname = \"led\"\nroot = {root:?}\nstore = {:?}\ntracker = {{ github = \"acme/led\", credential = \"env\", ledger = \"github\" }}\n",
            sb.paths.store_dir.join("led.fls")
        ),
    )
    .unwrap();
    let mut cred = change();
    cred.credential = Some(Credential::App);
    sb.update("led", &cred).unwrap();
    let text = sb.config();
    assert!(text.contains("ledger"), "{text}");
    assert!(text.contains("credential = \"app\""), "{text}");
}

#[test]
fn a_moved_project_keeps_its_store_gates_and_records_and_everything_agrees() {
    let sb = Sandbox::new();
    let old = sb.dir("movable");
    sb.init(&sb.request(&old)).unwrap();
    let new = sb.dir("moved-here");
    fs::create_dir_all(&new).unwrap();
    let store_path = sb.paths.store_dir.join("movable.fls");
    let (project_id, record) = {
        let store = fl_store::open_store(&store_path).unwrap();
        let id = store.list_projects().unwrap().remove(0).id;
        let record = store.add_record(&id, "keep me").unwrap();
        (id, record)
    };

    let mut req = change();
    req.path = Some(new.clone());
    let after = sb.update("movable", &req).unwrap();
    assert_eq!(after.path.as_deref(), Some(new.to_str().unwrap()));
    assert_eq!(sb.blocks()[0]["root"].as_str(), Some(new.to_str().unwrap()));
    assert_eq!(
        sb.workspace()
            .get_area("movable")
            .unwrap()
            .unwrap()
            .path
            .as_deref(),
        Some(new.to_str().unwrap())
    );
    let store = fl_store::open_store(&store_path).unwrap();
    let held = store.list_projects().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].id, project_id, "the same project");
    assert_eq!(held[0].root, new.display().to_string());
    assert!(
        store.get_record(&record).unwrap().is_some(),
        "its records stayed"
    );
    drop(store);

    // Creating again at the new path is idempotent: the store holds the project at the new root.
    let out = sb.init(&sb.request(&new)).unwrap();
    assert!(!out.registered);
    assert_eq!(out.slug, "movable");
    // A second update to the same path is a no-op.
    let text = sb.config();
    let after = sb.list().remove(0);
    assert_eq!(sb.update("movable", &req).unwrap(), after);
    assert_eq!(sb.config(), text);
    // The old directory is not touched and may now be another project's.
    assert!(old.is_dir());
    let mut reborn = sb.request(&old);
    reborn.slug = Some("reborn".into());
    sb.init(&reborn).unwrap();
    // But the old slug cannot be taken by it: it names the project that moved.
    let mut taken = sb.request(&sb.dir("elsewhere"));
    taken.slug = Some("movable".into());
    assert!(conflict_of(sb.init(&taken)).contains("`movable`"));
}

#[test]
fn a_project_cannot_move_onto_another_projects_directory() {
    let sb = Sandbox::new();
    sb.init(&sb.request(&sb.dir("one"))).unwrap();
    sb.init(&sb.request(&sb.dir("two"))).unwrap();
    let text = sb.config();
    let mut req = change();
    req.path = Some(sb.dir("two"));
    let msg = conflict_of(sb.update("one", &req));
    assert!(msg.contains("`two`"), "{msg}");
    assert_eq!(sb.config(), text);
}

fn conflict_of<T: std::fmt::Debug>(r: Result<T, InitError>) -> String {
    match r {
        Err(InitError::Conflict(m)) => m,
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[test]
fn a_project_with_a_store_but_no_config_block_can_be_renamed_but_not_moved() {
    let sb = Sandbox::new();
    fs::create_dir_all(&sb.paths.store_dir).unwrap();
    let hints = vec![(
        "stored".to_string(),
        Some(sb.paths.store_dir.join("stored.fls")),
    )];
    let mut req = change();
    req.name = Some("Stored One".into());
    let after = update_project(
        "stored",
        &req,
        &sb.paths,
        &hints,
        || sb.open_ws(),
        Sandbox::open_project,
    )
    .unwrap();
    assert_eq!(after.name, "Stored One");
    assert_eq!(
        after.kind,
        ProjectKind::Project,
        "an area with no path does not make it virtual"
    );
    let mut moving = change();
    moving.path = Some(sb.base.clone());
    let msg = conflict_of(update_project(
        "stored",
        &moving,
        &sb.paths,
        &hints,
        || sb.open_ws(),
        Sandbox::open_project,
    ));
    assert!(msg.contains("no [[project]] block"), "{msg}");
}

// --- GET / POST /api/v1/projects --------------------------------------------------------------

#[test]
fn get_lists_both_kinds_in_the_canonical_shape_with_the_artifact_fields() {
    let hub = Hub::new();
    let root = hub.work("shape");
    hub.post(
        OWNER,
        &json!({ "name": "Shape", "path": root, "github": "acme/shape", "purpose": "p1" }),
    );
    hub.post(
        OWNER,
        &json!({ "kind": "virtual", "name": "Council", "purpose": "p2" }),
    );
    let artifacts = hub.base.join("data/artifacts");
    let resp = hub.get("/api/v1/projects");
    assert_eq!(resp.status, 200);
    assert_eq!(
        body_of(&resp),
        json!({ "projects": [
            {
                "id": "council", "slug": "council", "name": "Council", "kind": "virtual",
                "path": null, "has_git": false, "purpose": "p2", "tracker": null,
                "store_path": null, "active_agent_count": 0,
                "artifacts_dir": null, "default_artifacts_dir": null,
                "effective_artifacts_dir": null
            },
            {
                "id": "ferroloop", "slug": "ferroloop", "name": "ferroloop", "kind": "project",
                "path": null, "has_git": false, "purpose": "", "tracker": null,
                "store_path": null, "active_agent_count": 0,
                "artifacts_dir": null,
                "default_artifacts_dir": artifacts.join("ferroloop").display().to_string(),
                "effective_artifacts_dir": artifacts.join("ferroloop").display().to_string()
            },
            {
                "id": "shape", "slug": "shape", "name": "Shape", "kind": "project",
                "path": root.display().to_string(), "has_git": false, "purpose": "p1",
                "tracker": { "kind": "github", "repo": "acme/shape", "credential": "app" },
                "store_path": hub.base.join("home/.local/share/fl/shape.fls").display().to_string(),
                "active_agent_count": 0,
                "artifacts_dir": null,
                "default_artifacts_dir": artifacts.join("shape").display().to_string(),
                "effective_artifacts_dir": artifacts.join("shape").display().to_string()
            }
        ] })
    );
    assert_eq!(hub.send("GET", "/api/v1/projects", None, None).status, 401);
}

#[test]
fn get_counts_the_active_agents_of_each_project() {
    use fl_core::agent::AgentRole;
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "path": hub.work("counted") }));
    hub.post(OWNER, &json!({ "kind": "virtual", "name": "Crew" }));
    {
        let ws = hub.workspace();
        for (name, project, extra) in [
            ("ada", "counted", vec![]),
            ("bea", "crew", vec!["counted".to_string()]),
            ("cy", "crew", vec![]),
        ] {
            ws.register_agent(fl_core::agent::RegisterAgentArgs {
                display_name: name.into(),
                email: None,
                project: project.into(),
                role: AgentRole::Tester,
                extra_areas: extra,
                create_area: false,
                replace: false,
                at: fl_core::at::At::from_unix_millis(3),
                by: "t".into(),
            })
            .unwrap();
        }
    }
    assert_eq!(hub.entry("counted")["active_agent_count"], 2);
    assert_eq!(hub.entry("crew")["active_agent_count"], 2);
    assert_eq!(hub.entry("ferroloop")["active_agent_count"], 0);
}

#[test]
fn the_owner_creates_a_project_and_gets_its_summary() {
    let hub = Hub::new();
    assert_eq!(hub.listed(), ["ferroloop"]);
    let root = hub.work("hubby");
    let resp = hub.post(
        OWNER,
        &json!({ "name": "Hubby", "path": root, "github": "acme/hubby", "credential": "env",
                 "purpose": "from the hub", "git_init": true }),
    );
    assert_eq!(resp.status, 201, "{}", text_of(&resp));
    let artifacts = hub.base.join("data/artifacts/hubby").display().to_string();
    assert_eq!(
        body_of(&resp),
        json!({
            "id": "hubby", "slug": "hubby", "name": "Hubby", "kind": "project",
            "path": root.display().to_string(), "has_git": true, "purpose": "from the hub",
            "tracker": { "kind": "github", "repo": "acme/hubby", "credential": "env" },
            "store_path": hub.base.join("home/.local/share/fl/hubby.fls").display().to_string(),
            "active_agent_count": 0,
            "artifacts_dir": null,
            "default_artifacts_dir": artifacts, "effective_artifacts_dir": artifacts
        })
    );
    assert_eq!(hub.listed(), ["ferroloop", "hubby"]);
    assert!(hub.base.join("home/.local/share/fl/hubby.fls").is_file());
    assert_eq!(
        git(&root, &["log", "-1", "--format=%s"]),
        INITIAL_COMMIT_MESSAGE
    );
    assert!(root.join(".fl/mcp.toml").is_file());
    let config = fs::read_to_string(hub.config()).unwrap();
    assert!(config.contains("name = \"hubby\""), "{config}");
    assert!(config.contains("credential = \"env\""), "{config}");
    // The display name is the area's, not the config's.
    assert!(!config.contains("Hubby"), "{config}");
    let (name, purpose) = {
        let ws = hub.workspace();
        let area = ws.get_area("hubby").unwrap().unwrap();
        (area.name, area.purpose)
    };
    assert_eq!((name.as_str(), purpose.as_str()), ("Hubby", "from the hub"));

    // The records API reaches the new project's store.
    let board = hub.get("/api/v1/records?project=hubby");
    assert_eq!(board.status, 200, "{}", text_of(&board));
    let made = hub.send(
        "POST",
        "/api/v1/records",
        Some(OWNER),
        Some(&json!({ "project": "hubby", "title": "first card" })),
    );
    assert_eq!(made.status, 201, "{}", text_of(&made));
}

#[test]
fn a_project_is_made_without_git_unless_git_init_is_true() {
    let hub = Hub::new();
    let root = hub.work("plain");
    let resp = hub.post(OWNER, &json!({ "path": root }));
    assert_eq!(resp.status, 201, "{}", text_of(&resp));
    assert_eq!(body_of(&resp)["has_git"], false);
    assert!(!root.join(".git").exists(), "git_init defaults to false");
    // An explicit false is the same, and a later request asking for git adds it.
    assert_eq!(
        hub.post(OWNER, &json!({ "path": root, "git_init": false }))
            .status,
        200
    );
    assert!(!root.join(".git").exists());
    let resp = hub.post(OWNER, &json!({ "path": root, "git_init": true }));
    assert_eq!(resp.status, 200);
    assert_eq!(body_of(&resp)["has_git"], true);
    assert_eq!(commit_count(&root), 1);
    // An existing directory that is not a repository is a project as it is.
    let existing = hub.work("existing");
    fs::create_dir_all(&existing).unwrap();
    fs::write(existing.join("notes.txt"), "mine").unwrap();
    assert_eq!(hub.post(OWNER, &json!({ "path": existing })).status, 201);
    assert!(!existing.join(".git").exists());
    assert_eq!(
        fs::read_to_string(existing.join("notes.txt")).unwrap(),
        "mine"
    );
}

#[test]
fn the_slug_is_given_or_derived_from_the_name_or_the_directory() {
    let hub = Hub::new();
    let r = |resp: HttpResponse| {
        (
            resp.status,
            body_of(&resp)["slug"].as_str().map(String::from),
        )
    };
    assert_eq!(
        r(hub.post(
            OWNER,
            &json!({ "name": "My Project", "path": hub.work("a") })
        )),
        (201, Some("my-project".into()))
    );
    assert_eq!(
        r(hub.post(
            OWNER,
            &json!({ "name": "Pretty", "slug": "ugly_slug", "path": hub.work("b") })
        )),
        (201, Some("ugly_slug".into()))
    );
    // An empty slug or name counts as absent.
    assert_eq!(
        r(hub.post(
            OWNER,
            &json!({ "name": "", "slug": "", "path": hub.work("Dir-C") })
        )),
        (201, Some("dir-c".into()))
    );
    for (body, why) in [
        (
            json!({ "slug": "Bad Slug", "path": hub.work("d") }),
            "project name",
        ),
        (
            json!({ "name": "1 2 3", "path": hub.work("d") }),
            "cannot derive a slug",
        ),
        (
            json!({ "name": "x".repeat(81), "path": hub.work("d") }),
            "name",
        ),
    ] {
        let resp = hub.post(OWNER, &body);
        assert_eq!(resp.status, 400, "{body}: {}", text_of(&resp));
        assert!(error_of(&resp).contains(why), "{body}: {}", error_of(&resp));
    }
    assert!(!hub.work("d").exists());
}

#[test]
fn a_virtual_group_is_made_by_kind_or_by_the_earlier_flag_and_writes_only_its_area() {
    let hub = Hub::new();
    let resp = hub.post(
        OWNER,
        &json!({ "kind": "virtual", "name": "Council", "purpose": "the seats",
                 "path": "/ignored", "git_init": true, "github": "a/b", "credential": "env" }),
    );
    assert_eq!(resp.status, 201, "{}", text_of(&resp));
    assert_eq!(
        body_of(&resp),
        json!({
            "id": "council", "slug": "council", "name": "Council", "kind": "virtual",
            "path": null, "has_git": false, "purpose": "the seats", "tracker": null,
            "store_path": null, "active_agent_count": 0,
            "artifacts_dir": null, "default_artifacts_dir": null,
            "effective_artifacts_dir": null
        })
    );
    assert!(!hub.config().exists(), "no config");
    assert!(
        !hub.base.join("home/.local/share/fl").exists(),
        "no project store directory"
    );
    assert!(!Path::new("/ignored").exists());
    let area_path = hub.workspace().get_area("council").unwrap().unwrap().path;
    assert_eq!(area_path, None);

    // The earlier spelling; a repeat is a 200; the slug can be given.
    let resp = hub.post(
        OWNER,
        &json!({ "virtual": true, "name": "Ops", "slug": "oncall" }),
    );
    assert_eq!(resp.status, 201);
    assert_eq!(body_of(&resp)["slug"], "oncall");
    assert_eq!(body_of(&resp)["name"], "Ops");
    assert_eq!(
        hub.post(
            OWNER,
            &json!({ "virtual": true, "name": "Ops", "slug": "oncall" })
        )
        .status,
        200
    );
    assert_eq!(hub.listed(), ["council", "ferroloop", "oncall"]);
    assert!(!hub.config().exists());
    // Both spellings together must agree.
    let resp = hub.post(
        OWNER,
        &json!({ "kind": "project", "virtual": true, "name": "x", "path": hub.work("x") }),
    );
    assert_eq!(resp.status, 400);
    assert!(!hub.work("x").exists());
    // A virtual group needs a name.
    assert_eq!(hub.post(OWNER, &json!({ "kind": "virtual" })).status, 400);
    assert_eq!(
        hub.post(OWNER, &json!({ "kind": "bogus", "name": "x" }))
            .status,
        400
    );
}

#[test]
fn a_slug_cannot_be_both_kinds_over_http() {
    let hub = Hub::new();
    assert_eq!(
        hub.post(OWNER, &json!({ "kind": "virtual", "name": "Shared" }))
            .status,
        201
    );
    let resp = hub.post(OWNER, &json!({ "path": hub.work("shared") }));
    assert_eq!(resp.status, 409, "{}", text_of(&resp));
    assert!(
        error_of(&resp).contains("fl agent area set"),
        "{}",
        error_of(&resp)
    );
    assert!(!hub.work("shared").exists());
    assert!(!hub.base.join("home/.local/share/fl/shared.fls").exists());

    assert_eq!(
        hub.post(OWNER, &json!({ "path": hub.work("solid") }))
            .status,
        201
    );
    let resp = hub.post(OWNER, &json!({ "kind": "virtual", "name": "Solid" }));
    assert_eq!(resp.status, 409, "{}", text_of(&resp));
    assert!(error_of(&resp).contains("directory-backed project"));
    assert_eq!(hub.entry("solid")["kind"], "project");
}

#[test]
fn posting_the_same_project_again_is_idempotent() {
    let hub = Hub::new();
    let body = json!({ "path": hub.work("twice"), "name": "Twice" });
    let first = hub.post(OWNER, &body);
    assert_eq!(first.status, 201);
    let config = fs::read_to_string(hub.config()).unwrap();
    let second = hub.post(OWNER, &body);
    assert_eq!(second.status, 200);
    assert_eq!(body_of(&second), body_of(&first));
    assert_eq!(fs::read_to_string(hub.config()).unwrap(), config);
    assert_eq!(hub.listed(), ["ferroloop", "twice"]);
}

#[test]
fn only_the_owner_may_create_a_project() {
    let hub = Hub::new();
    for body in [
        json!({ "path": hub.work("nope") }),
        json!({ "kind": "virtual", "name": "Nope" }),
    ] {
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
    }
    // A peer is refused before its body is read.
    assert_eq!(hub.post(PEER, &json!({ "bogus": 1 })).status, 403);
    assert!(
        !hub.work("nope").exists(),
        "a refused request makes no directory"
    );
    assert!(!hub.config().exists(), "or a config");
    assert_eq!(hub.listed(), ["ferroloop"], "or a group");
}

#[test]
fn a_request_that_cannot_be_used_is_a_client_error_and_changes_nothing() {
    let hub = Hub::new();
    let work = hub.work("x");
    let cases: Vec<(Value, u16, &str)> = vec![
        (
            json!({ "path": work, "slug": "Bad Name" }),
            400,
            "project name",
        ),
        (json!({ "path": work, "slug": "1abc" }), 400, "project name"),
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
        (json!({ "path": work, "git_init": "yes" }), 400, "boolean"),
    ];
    for (body, status, why) in cases {
        let resp = hub.post(OWNER, &body);
        assert_eq!(resp.status, status, "{body}: {}", text_of(&resp));
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
        hub.post(OWNER, &json!({ "path": hub.work("one"), "slug": "taken" }))
            .status,
        201
    );
    let config = fs::read_to_string(hub.config()).unwrap();
    let resp = hub.post(OWNER, &json!({ "path": hub.work("two"), "slug": "taken" }));
    assert_eq!(resp.status, 409);
    assert!(error_of(&resp).contains("already registered"));
    assert!(!hub.work("two").exists());
    assert_eq!(fs::read_to_string(hub.config()).unwrap(), config);
    assert_eq!(hub.listed(), ["ferroloop", "taken"]);
}

#[test]
fn the_hub_announces_a_new_project() {
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
    assert_eq!(frame.data["kind"], "project");
}

// --- PATCH /api/v1/projects/:slug -------------------------------------------------------------

#[test]
fn patch_updates_the_name_purpose_and_tracker_and_returns_the_summary() {
    let hub = Hub::new();
    let root = hub.work("patched");
    hub.post(OWNER, &json!({ "path": root, "purpose": "before" }));
    let resp = hub.patch(
        OWNER,
        "patched",
        &json!({ "name": "Patched Up", "purpose": "after", "github": "acme/patched", "credential": "env" }),
    );
    assert_eq!(resp.status, 200, "{}", text_of(&resp));
    let body = body_of(&resp);
    assert_eq!(body["slug"], "patched");
    assert_eq!(body["id"], "patched");
    assert_eq!(body["name"], "Patched Up");
    assert_eq!(body["purpose"], "after");
    assert_eq!(
        body["tracker"],
        json!({ "kind": "github", "repo": "acme/patched", "credential": "env" })
    );
    assert_eq!(body["path"], root.display().to_string());
    assert_eq!(
        hub.entry("patched"),
        body,
        "the list says what the patch said"
    );
    let config = fs::read_to_string(hub.config()).unwrap();
    assert!(config.contains("acme/patched"), "{config}");

    // A repeat changes nothing; clearing the tracker removes it from config and list.
    let again = hub.patch(OWNER, "patched", &json!({ "name": "Patched Up" }));
    assert_eq!(again.status, 200);
    assert_eq!(body_of(&again), body);
    let cleared = hub.patch(OWNER, "patched", &json!({ "github": "" }));
    assert_eq!(cleared.status, 200);
    assert_eq!(body_of(&cleared)["tracker"], Value::Null);
    assert!(
        !fs::read_to_string(hub.config())
            .unwrap()
            .contains("acme/patched")
    );
    // PATCH is idempotent on a clear, too.
    assert_eq!(
        hub.patch(OWNER, "patched", &json!({ "github": "" })).status,
        200
    );
}

#[test]
fn patch_with_an_empty_purpose_resets_it_to_the_default_for_both_kinds() {
    let hub = Hub::new();
    hub.post(
        OWNER,
        &json!({ "path": hub.work("resets"), "purpose": "before" }),
    );
    hub.post(
        OWNER,
        &json!({ "kind": "virtual", "name": "Council", "purpose": "before" }),
    );
    for (slug, default) in [
        ("resets", "Project resets"),
        ("council", "Virtual group council"),
    ] {
        for blank in ["", "   "] {
            let set = hub.patch(OWNER, slug, &json!({ "purpose": "before" }));
            assert_eq!(set.status, 200, "{}", text_of(&set));
            assert_eq!(body_of(&set)["purpose"], "before");
            let resp = hub.patch(OWNER, slug, &json!({ "purpose": blank }));
            assert_eq!(resp.status, 200, "{slug} {blank:?}: {}", text_of(&resp));
            assert_eq!(body_of(&resp)["purpose"], default);
            assert_eq!(hub.entry(slug)["purpose"], default, "GET says the default");
            let again = hub.patch(OWNER, slug, &json!({ "purpose": blank }));
            assert_eq!(again.status, 200);
            assert_eq!(body_of(&again), body_of(&resp));
        }
    }
    // A purpose that cannot be used is still refused, and the default stays.
    let bad = hub.patch(OWNER, "resets", &json!({ "purpose": "a\nb" }));
    assert_eq!(bad.status, 400, "{}", text_of(&bad));
    let long = hub.patch(OWNER, "council", &json!({ "purpose": "x".repeat(10_000) }));
    assert_eq!(long.status, 400, "{}", text_of(&long));
    assert_eq!(hub.entry("resets")["purpose"], "Project resets");
    assert_eq!(hub.entry("council")["purpose"], "Virtual group council");
}

#[test]
fn patch_moves_a_project_and_keeps_the_hubs_views_in_step() {
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "path": hub.work("walker") }));
    let board = hub.send(
        "POST",
        "/api/v1/records",
        Some(OWNER),
        Some(&json!({ "project": "walker", "title": "stays" })),
    );
    assert_eq!(board.status, 201, "{}", text_of(&board));
    let there = hub.work("there");
    fs::create_dir_all(&there).unwrap();
    let resp = hub.patch(OWNER, "walker", &json!({ "path": there }));
    assert_eq!(resp.status, 200, "{}", text_of(&resp));
    assert_eq!(body_of(&resp)["path"], there.display().to_string());
    let config = fs::read_to_string(hub.config()).unwrap();
    assert!(config.contains(&there.display().to_string()), "{config}");
    let area_path = hub.workspace().get_area("walker").unwrap().unwrap().path;
    assert_eq!(area_path.as_deref(), Some(there.to_str().unwrap()));
    let records = hub.get("/api/v1/records?project=walker");
    assert_eq!(records.status, 200);
    assert!(text_of(&records).contains("stays"), "its records stayed");
}

#[test]
fn patch_refuses_what_cannot_be_used() {
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "path": hub.work("p") }));
    hub.post(OWNER, &json!({ "kind": "virtual", "name": "Grp" }));
    let before = fs::read_to_string(hub.config()).unwrap();
    let cases: Vec<(&str, Value, u16, &str)> = vec![
        ("p", json!({}), 400, "nothing to change"),
        ("p", json!({ "name": " " }), 400, "name"),
        ("p", json!({ "purpose": "a\nb" }), 400, "purpose"),
        ("p", json!({ "github": "nope" }), 400, "owner/repo"),
        ("p", json!({ "credential": "token" }), 400, "credential"),
        ("p", json!({ "credential": "env" }), 400, "no tracker"),
        ("p", json!({ "path": "rel" }), 400, "absolute"),
        (
            "p",
            json!({ "path": hub.work("absent") }),
            400,
            "existing directory",
        ),
        ("p", json!({ "path": "/etc" }), 400, "protected"),
        ("p", json!({ "path": hub.base.join("home") }), 400, "home"),
        ("p", json!({ "slug": "renamed" }), 400, "slug"),
        ("p", json!({ "kind": "virtual" }), 400, "kind"),
        ("grp", json!({ "path": hub.work("p") }), 400, "no path"),
        ("grp", json!({ "github": "a/b" }), 400, "no tracker"),
        ("nosuch", json!({ "name": "x" }), 404, "no project"),
        ("Bad%20Slug", json!({ "name": "x" }), 400, "letters"),
    ];
    for (slug, body, status, why) in cases {
        let resp = hub.patch(OWNER, slug, &body);
        assert_eq!(resp.status, status, "{slug} {body}: {}", text_of(&resp));
        assert!(
            error_of(&resp).contains(why),
            "{slug} {body}: {}",
            error_of(&resp)
        );
    }
    let bad_json = HttpRequest::new("PATCH", "/api/v1/projects/p")
        .with_header("Authorization", format!("Bearer {OWNER}"))
        .with_body(b"{nope".to_vec());
    assert_eq!(handle_http_request(&hub.state, &bad_json).status, 400);
    // A directory-backed project cannot move onto another project's directory.
    hub.post(OWNER, &json!({ "path": hub.work("q") }));
    let resp = hub.patch(OWNER, "p", &json!({ "path": hub.work("q") }));
    assert_eq!(resp.status, 409, "{}", text_of(&resp));
    assert_eq!(hub.entry("p")["path"], hub.work("p").display().to_string());
    assert!(
        fs::read_to_string(hub.config())
            .unwrap()
            .starts_with(&before)
    );
}

#[test]
fn only_the_owner_may_patch_a_project() {
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "path": hub.work("guarded") }));
    let body = json!({ "name": "Hijacked" });
    assert_eq!(
        hub.send("PATCH", "/api/v1/projects/guarded", None, Some(&body))
            .status,
        401
    );
    assert_eq!(
        hub.send(
            "PATCH",
            "/api/v1/projects/guarded",
            Some("wrong"),
            Some(&body)
        )
        .status,
        401
    );
    let resp = hub.patch(PEER, "guarded", &body);
    assert_eq!(resp.status, 403);
    assert!(error_of(&resp).contains("Owner"));
    assert_eq!(
        hub.patch(PEER, "nosuch", &body).status,
        403,
        "refused before the project is looked up"
    );
    assert_eq!(hub.entry("guarded")["name"], "guarded");
}

#[test]
fn patch_updates_a_virtual_group_and_announces_it() {
    let hub = Hub::new();
    hub.post(
        OWNER,
        &json!({ "kind": "virtual", "name": "Council", "purpose": "old" }),
    );
    let mut frames = hub.state.subscribe();
    let resp = hub.patch(
        OWNER,
        "council",
        &json!({ "name": "The Council", "purpose": "new" }),
    );
    assert_eq!(resp.status, 200, "{}", text_of(&resp));
    let body = body_of(&resp);
    assert_eq!(
        (body["name"].as_str(), body["purpose"].as_str()),
        (Some("The Council"), Some("new"))
    );
    assert_eq!(body["kind"], "virtual");
    assert_eq!(body["path"], Value::Null);
    let frame = frames.try_recv().unwrap();
    assert_eq!(frame.event, "project.updated");
    assert!(!hub.config().exists());
}

// --- consumers of the project list: virtual groups never reach a store or a root --------------

#[test]
fn a_virtual_group_has_no_records_and_no_artifacts_directory() {
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "kind": "virtual", "name": "Council" }));
    // The records API refuses the slug instead of answering from the hub store.
    let resp = hub.get("/api/v1/records?project=council");
    assert_eq!(resp.status, 404, "{}", text_of(&resp));
    assert!(error_of(&resp).contains("virtual group"));
    let made = hub.send(
        "POST",
        "/api/v1/records",
        Some(OWNER),
        Some(&json!({ "project": "council", "title": "nowhere" })),
    );
    assert_eq!(made.status, 404, "{}", text_of(&made));
    // The per-project artifacts-directory routes do not know it.
    assert_eq!(
        hub.get("/api/v1/projects/council/artifacts-dir").status,
        404
    );
    let set = hub.send(
        "POST",
        "/api/v1/projects/council/artifacts-dir",
        Some(OWNER),
        Some(&json!({ "new_path": hub.base.join("elsewhere") })),
    );
    assert_eq!(set.status, 404);
    // Nothing was made: no store for it, no directory.
    assert!(!hub.base.join("home/.local/share/fl/council.fls").exists());
    assert!(!hub.base.join("data/artifacts/council").exists());
    // The records board still works for a real project and the default.
    assert_eq!(hub.get("/api/v1/records").status, 200);
}

#[test]
fn a_project_with_a_pathless_area_still_serves_its_records() {
    let hub = Hub::new();
    // The default project served from the hub store, with an area that has no path (what
    // `fl agent register --create-area` makes), and a project the hub store holds a row for.
    hub.agent("Ada", "ferroloop", fl_core::agent::AgentRole::Assistant);
    hub.workspace().add_project("/work/held-proj").unwrap();
    hub.agent("Bo", "held-proj", fl_core::agent::AgentRole::Assistant);
    for slug in ["ferroloop", "held-proj"] {
        assert!(
            hub.workspace()
                .get_area(slug)
                .unwrap()
                .is_some_and(|a| a.is_virtual()),
            "{slug} has a pathless area"
        );
        let resp = hub.get(&format!("/api/v1/records?project={slug}"));
        assert_eq!(resp.status, 200, "{slug}: {}", text_of(&resp));
    }
    // A group no project row, no config block and no store file backs is still not served.
    hub.agent("Cy", "council", fl_core::agent::AgentRole::Assistant);
    let resp = hub.get("/api/v1/records?project=council");
    assert_eq!(resp.status, 404, "{}", text_of(&resp));
    assert!(error_of(&resp).contains("virtual group"));
}

#[test]
fn a_virtual_group_does_not_appear_among_the_available_projects_of_the_records_board() {
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "kind": "virtual", "name": "Council" }));
    hub.post(OWNER, &json!({ "path": hub.work("real") }));
    let board = body_of(&hub.get("/api/v1/records"));
    assert_eq!(board["available_projects"], json!(["ferroloop", "real"]));
}

// --- DELETE /api/v1/projects/:slug ------------------------------------------------------------

impl Hub {
    fn delete(&self, token: &str, slug: &str, body: Option<&Value>) -> HttpResponse {
        self.send(
            "DELETE",
            &format!("/api/v1/projects/{slug}"),
            Some(token),
            body,
        )
    }

    fn data_file(&self, name: &str) -> PathBuf {
        self.base.join("home/.local/share/fl").join(name)
    }

    fn agent(&self, name: &str, project: &str, role: fl_core::agent::AgentRole) {
        self.workspace()
            .register_agent(fl_core::agent::RegisterAgentArgs {
                display_name: name.into(),
                email: None,
                project: project.into(),
                role,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: fl_core::At::from_unix_millis(5),
                by: "tester".into(),
            })
            .unwrap();
    }
}

#[test]
fn delete_with_no_body_unregisters_and_deletes_no_file() {
    let hub = Hub::new();
    let root = hub.work("doomed");
    hub.post(OWNER, &json!({ "path": root }));
    hub.post(OWNER, &json!({ "path": hub.work("kept") }));
    let mut frames = hub.state.subscribe();

    let resp = hub.delete(OWNER, "doomed", None);
    assert_eq!(resp.status, 200, "{}", text_of(&resp));
    assert_eq!(
        body_of(&resp),
        json!({
            "deleted": "doomed",
            "reassigned_agents": 0,
            "decommissioned_agents": 0,
            "store_deleted": false,
            "dir_deleted": false,
        })
    );
    let frame = frames.try_recv().unwrap();
    assert_eq!(frame.event, "project.deleted");
    let config = fs::read_to_string(hub.config()).unwrap();
    assert!(
        !config.contains("doomed") && config.contains("kept"),
        "{config}"
    );
    assert!(hub.workspace().get_area("doomed").unwrap().is_none());
    assert!(root.join(".fl/mcp.toml").is_file(), "the directory stays");
    assert!(hub.data_file("doomed.fls").is_file(), "the store stays");
    // The Hub lists a project from a store file in its data directory, so the slug is still
    // listed, with no path, until the store file is deleted too.
    assert_eq!(hub.entry("doomed")["path"], Value::Null);
    assert_eq!(hub.delete(OWNER, "nosuch", None).status, 404);

    // An empty object is the same request, and deleting the store removes the slug.
    let resp = hub.delete(OWNER, "doomed", Some(&json!({ "delete_store": true })));
    assert_eq!(resp.status, 200, "{}", text_of(&resp));
    assert_eq!(body_of(&resp)["store_deleted"], true);
    assert!(!hub.data_file("doomed.fls").exists());
    assert_eq!(hub.entry("doomed"), Value::Null);
    assert_eq!(hub.delete(OWNER, "doomed", Some(&json!({}))).status, 404);
}

#[test]
fn delete_answers_409_with_the_active_agents_and_changes_nothing() {
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "path": hub.work("busy") }));
    hub.agent("Ada", "busy", fl_core::agent::AgentRole::Assistant);
    hub.agent("Bo", "busy", fl_core::agent::AgentRole::Assistant);
    let config = fs::read_to_string(hub.config()).unwrap();

    let resp = hub.delete(OWNER, "busy", Some(&json!({ "delete_dir": true })));
    assert_eq!(resp.status, 409, "{}", text_of(&resp));
    let mut body = body_of(&resp);
    body["active_agents"]
        .as_array_mut()
        .unwrap()
        .sort_by_key(|a| a.to_string());
    assert_eq!(
        body,
        json!({
            "error": "project has active agents",
            "active_agent_count": 2,
            "active_agents": ["Ada", "Bo"],
        })
    );
    assert_eq!(fs::read_to_string(hub.config()).unwrap(), config);
    assert!(hub.workspace().get_area("busy").unwrap().is_some());
    assert!(hub.work("busy").is_dir() && hub.data_file("busy.fls").is_file());
    assert_eq!(hub.entry("busy")["active_agent_count"], 2);
}

#[test]
fn delete_can_reassign_or_decommission_the_agents_but_not_both() {
    let hub = Hub::new();
    for name in ["one", "two", "three"] {
        hub.post(OWNER, &json!({ "path": hub.work(name) }));
    }
    hub.agent("Ada", "one", fl_core::agent::AgentRole::Assistant);
    hub.agent("Bo", "two", fl_core::agent::AgentRole::Assistant);

    let both = hub.delete(
        OWNER,
        "one",
        Some(&json!({ "reassign_to": "three", "decommission_agents": true })),
    );
    assert_eq!(both.status, 400, "{}", text_of(&both));
    assert!(hub.workspace().get_area("one").unwrap().is_some());
    let unknown = hub.delete(OWNER, "one", Some(&json!({ "reassign_to": "nosuch" })));
    assert_eq!(unknown.status, 404);
    let same = hub.delete(OWNER, "one", Some(&json!({ "reassign_to": "one" })));
    assert_eq!(same.status, 400);
    assert!(hub.workspace().get_area("one").unwrap().is_some());

    let moved = hub.delete(OWNER, "one", Some(&json!({ "reassign_to": "three" })));
    assert_eq!(moved.status, 200, "{}", text_of(&moved));
    assert_eq!(body_of(&moved)["reassigned_agents"], 1);
    assert_eq!(hub.entry("three")["active_agent_count"], 1);

    let retired = hub.delete(OWNER, "two", Some(&json!({ "decommission_agents": true })));
    assert_eq!(retired.status, 200, "{}", text_of(&retired));
    assert_eq!(body_of(&retired)["decommissioned_agents"], 1);
    let bo = hub.workspace().agent_by_name("bo").unwrap().unwrap();
    assert_eq!(bo.status, fl_core::agent::AgentStatus::Retired);
}

#[test]
fn delete_dir_removes_only_the_registered_directory_inside_the_hubs_home() {
    let hub = Hub::new();
    let root = hub.work("scrap");
    hub.post(OWNER, &json!({ "path": root }));
    hub.post(OWNER, &json!({ "path": hub.work("sibling") }));
    let resp = hub.delete(OWNER, "scrap", Some(&json!({ "delete_dir": true })));
    assert_eq!(resp.status, 200, "{}", text_of(&resp));
    assert_eq!(body_of(&resp)["dir_deleted"], true);
    assert!(!root.exists());
    assert!(hub.work("sibling").is_dir() && hub.base.join("work").is_dir());
}

#[test]
fn delete_dir_is_refused_for_the_hubs_home_and_changes_nothing() {
    let hub = Hub::new();
    let config = hub.config();
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let home = hub.base.join("home");
    fs::write(
        &config,
        format!(
            "[[project]]\nname = \"homeish\"\nroot = \"{}\"\n",
            home.display()
        ),
    )
    .unwrap();
    let before = fs::read_to_string(&config).unwrap();
    let resp = hub.delete(OWNER, "homeish", Some(&json!({ "delete_dir": true })));
    assert_eq!(resp.status, 400, "{}", text_of(&resp));
    assert!(
        error_of(&resp).contains("home directory"),
        "{}",
        error_of(&resp)
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), before);
    assert!(home.join(".config/fl/config.toml").is_file());
}

#[test]
fn delete_removes_a_virtual_group_and_refuses_store_and_dir_for_it() {
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "kind": "virtual", "name": "Council" }));
    for body in [
        json!({ "delete_dir": true }),
        json!({ "delete_store": true }),
    ] {
        assert_eq!(hub.delete(OWNER, "council", Some(&body)).status, 400);
        assert_ne!(hub.entry("council"), Value::Null);
    }
    let resp = hub.delete(OWNER, "council", Some(&json!({})));
    assert_eq!(resp.status, 200, "{}", text_of(&resp));
    assert_eq!(body_of(&resp)["deleted"], "council");
    assert_eq!(hub.entry("council"), Value::Null);
    assert!(hub.workspace().get_area("council").unwrap().is_none());
    assert!(!hub.config().exists());
}

#[test]
fn only_the_owner_may_delete_a_project() {
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "path": hub.work("guarded") }));
    assert_eq!(
        hub.send("DELETE", "/api/v1/projects/guarded", None, None)
            .status,
        401
    );
    let resp = hub.delete(PEER, "guarded", None);
    assert_eq!(resp.status, 403);
    assert!(error_of(&resp).contains("Owner"));
    assert_eq!(
        hub.delete(PEER, "nosuch", None).status,
        403,
        "refused before the project is looked up"
    );
    assert_ne!(hub.entry("guarded"), Value::Null);
    assert!(hub.work("guarded").is_dir());
}

#[test]
fn delete_refuses_an_unknown_field_and_a_bad_slug() {
    let hub = Hub::new();
    hub.post(OWNER, &json!({ "path": hub.work("p") }));
    let resp = hub.delete(OWNER, "p", Some(&json!({ "delete_everything": true })));
    assert_eq!(resp.status, 400, "{}", text_of(&resp));
    assert_eq!(hub.delete(OWNER, "Not%20A%20Slug", None).status, 400);
    assert_ne!(hub.entry("p"), Value::Null);
}
