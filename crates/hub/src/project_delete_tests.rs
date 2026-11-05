//! Deleting a project: the shared flow (`project_delete`) and the runtime directory a virtual
//! scope's agents default to. Every path lives under a temporary directory the test made; the
//! flow is given its home and workspace root, so nothing here reaches a real config, data
//! directory, `$HOME` or route table.

use std::fs;
use std::path::{Path, PathBuf};

use fl_core::At;
use fl_core::agent::{AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs};
use fl_core::store::{AgentRegistry, Catalog};
use fl_store::AnyStore;
use tempfile::TempDir;

use crate::lifecycle::{self, LifecycleEnv, SpinUpRequest};
use crate::project_delete::hooks::{self, Fault};
use crate::project_delete::{
    DeleteContext, DeleteError, DeleteOutcome, DeleteRequest, delete_project_in,
    locate_workspace_roots,
};
use crate::project_init::{InitError, InitPaths, InitRequest, init_project};
use crate::project_list::{ProjectSummary, StoreHint, list_projects};

struct Fx {
    _tmp: TempDir,
    base: PathBuf,
    paths: InitPaths,
    home: PathBuf,
    ws_root: PathBuf,
}

impl Fx {
    fn new() -> Self {
        hooks::reset();
        let tmp = TempDir::new().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        for dir in ["home", "ws", "work", "data/fl", "cfg/fl"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        Self {
            paths: InitPaths {
                config_path: base.join("cfg/fl/config.toml"),
                store_dir: base.join("data/fl"),
                workspace_store: base.join("data/fl/fl.fls"),
            },
            home: base.join("home"),
            ws_root: base.join("ws"),
            base,
            _tmp: tmp,
        }
    }

    fn open_ws(&self) -> Result<Box<AnyStore>, InitError> {
        fl_store::open_store(&self.paths.workspace_store)
            .map(Box::new)
            .map_err(|e| InitError::Failed(format!("workspace store: {e}")))
    }

    fn ws(&self) -> AnyStore {
        fl_store::open_store(&self.paths.workspace_store).unwrap()
    }

    /// A directory-backed project named `name` in `work/`, with its store.
    fn project(&self, name: &str) -> PathBuf {
        self.project_at(&self.base.join("work").join(name))
    }

    fn project_at(&self, root: &Path) -> PathBuf {
        let req = InitRequest {
            path: root.to_path_buf(),
            slug: None,
            display_name: None,
            github: None,
            credential: None,
            purpose: None,
            git_init: false,
            by: "tester".into(),
        };
        let out = init_project(
            &req,
            &self.paths,
            || self.open_ws(),
            |r| {
                fl_store::open_store(&r.store)
                    .map(Box::new)
                    .map_err(|e| InitError::Failed(e.to_string()))
            },
        )
        .unwrap();
        PathBuf::from(out.root.unwrap())
    }

    fn virtual_group(&self, name: &str) {
        let req = crate::project_init::VirtualRequest {
            slug: None,
            display_name: name.into(),
            purpose: None,
            by: "tester".into(),
        };
        crate::project_init::create_virtual(&req, &self.paths, || self.open_ws()).unwrap();
    }

    fn agent(&self, name: &str, project: &str, role: AgentRole, extra: &[&str]) {
        self.ws()
            .register_agent(RegisterAgentArgs {
                display_name: name.into(),
                email: None,
                project: project.into(),
                role,
                extra_areas: extra.iter().map(ToString::to_string).collect(),
                create_area: true,
                replace: false,
                at: At::from_unix_millis(5),
                by: "tester".into(),
            })
            .unwrap();
    }

    fn call(&self, slug: &str, req: &DeleteRequest) -> Result<DeleteOutcome, DeleteError> {
        self.call_with(slug, req, &self.home, &self.ws_root)
    }

    fn call_with(
        &self,
        slug: &str,
        req: &DeleteRequest,
        home: &Path,
        ws_root: &Path,
    ) -> Result<DeleteOutcome, DeleteError> {
        self.call_in(
            slug,
            req,
            Some(home),
            Some(home.to_path_buf()),
            &[ws_root.to_path_buf()],
        )
    }

    /// A deletion with the home directories and the known workspace roots named. Nothing here
    /// reads the process's `$HOME`.
    fn call_in(
        &self,
        slug: &str,
        req: &DeleteRequest,
        home: Option<&Path>,
        env_home: Option<PathBuf>,
        roots: &[PathBuf],
    ) -> Result<DeleteOutcome, DeleteError> {
        self.call_stores(slug, req, home, env_home, roots, &[])
    }

    /// [`Fx::call_in`] with the per-project stores the caller knows of (the Hub's data
    /// directory, or the CLI's).
    fn call_stores(
        &self,
        slug: &str,
        req: &DeleteRequest,
        home: Option<&Path>,
        env_home: Option<PathBuf>,
        roots: &[PathBuf],
        stores: &[StoreHint],
    ) -> Result<DeleteOutcome, DeleteError> {
        let env = LifecycleEnv::new(Some(&self.home));
        let ctx = DeleteContext {
            paths: &self.paths,
            stores,
            home,
            workspace_roots: roots,
            lifecycle: &env,
        };
        delete_project_in(
            slug,
            req,
            "tester",
            &ctx,
            env_home,
            || self.open_ws(),
            |_| {},
        )
    }

    fn list(&self) -> Vec<ProjectSummary> {
        let ws = self.ws();
        let held = ws.list_projects().unwrap();
        list_projects(&self.paths.config_path, &[], &held, &ws).unwrap()
    }

    fn slugs(&self) -> Vec<String> {
        self.list().into_iter().map(|p| p.slug).collect()
    }

    fn config(&self) -> String {
        fs::read_to_string(&self.paths.config_path).unwrap_or_default()
    }

    fn store_file(&self, slug: &str) -> PathBuf {
        self.paths.store_dir.join(format!("{slug}.fls"))
    }

    /// Everything a refused deletion must leave as it was: the config text, the areas, the
    /// agents, and which of the files exist.
    fn snapshot(&self, files: &[&Path]) -> String {
        let ws = self.ws();
        let areas: Vec<_> = ws
            .list_areas()
            .unwrap()
            .into_iter()
            .map(|a| (a.area_key, a.path))
            .collect();
        let agents: Vec<_> = ws
            .list_agents(None, true)
            .unwrap()
            .into_iter()
            .map(|a| (a.name_key, a.project, a.extra_areas, a.status))
            .collect();
        let exist: Vec<_> = files
            .iter()
            .map(|f| fs::symlink_metadata(f).is_ok())
            .collect();
        format!("{}\n{areas:?}\n{agents:?}\n{exist:?}", self.config())
    }

    /// Registers a `[[project]]` block by hand: a registration the flows would not write.
    fn hand_registered(&self, name: &str, root: &Path) {
        let mut text = self.config();
        text.push_str(&format!(
            "\n[[project]]\nname = \"{name}\"\nroot = \"{}\"\n",
            root.display()
        ));
        fs::write(&self.paths.config_path, text).unwrap();
    }
}

fn dir_req() -> DeleteRequest {
    DeleteRequest {
        delete_dir: true,
        ..DeleteRequest::default()
    }
}

fn refused(r: Result<DeleteOutcome, DeleteError>) -> String {
    match r {
        Err(DeleteError::Init(InitError::Invalid(m))) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// --- the default: only the registration goes --------------------------------------------------

#[test]
fn the_default_removes_the_block_and_the_area_and_deletes_no_file() {
    let fx = Fx::new();
    let root = fx.project("alpha");
    fx.project("beta");
    fs::write(root.join("keep.txt"), "mine").unwrap();
    let store = fx.store_file("alpha");
    assert!(store.is_file());

    let out = fx.call("alpha", &DeleteRequest::default()).unwrap();
    assert_eq!(
        out,
        DeleteOutcome {
            deleted: "alpha".into(),
            reassigned_agents: 0,
            decommissioned_agents: 0,
            store_deleted: false,
            dir_deleted: false,
            error: None,
        }
    );
    assert_eq!(fx.slugs(), ["beta"]);
    assert!(!fx.config().contains("alpha"), "{}", fx.config());
    assert!(fx.config().contains("beta"));
    assert!(fx.ws().get_area("alpha").unwrap().is_none());
    assert!(fx.ws().get_area("beta").unwrap().is_some());
    assert_eq!(fs::read_to_string(root.join("keep.txt")).unwrap(), "mine");
    assert!(root.join(".fl/mcp.toml").is_file());
    assert!(store.is_file(), "the store file stays");
    // The last block takes the `project` key with it.
    fx.call("beta", &DeleteRequest::default()).unwrap();
    assert!(!fx.config().contains("project"), "{}", fx.config());
    assert!(fx.slugs().is_empty());
}

#[test]
fn an_unknown_slug_is_not_found_and_a_repeat_is_not_found() {
    let fx = Fx::new();
    fx.project("alpha");
    let before = fx.snapshot(&[]);
    assert!(matches!(
        fx.call("nosuch", &DeleteRequest::default()),
        Err(DeleteError::Init(InitError::NotFound(_)))
    ));
    fx.call("alpha", &DeleteRequest::default()).unwrap();
    assert!(matches!(
        fx.call("alpha", &DeleteRequest::default()),
        Err(DeleteError::Init(InitError::NotFound(_)))
    ));
    assert_ne!(before, fx.snapshot(&[]));
}

#[test]
fn a_virtual_group_loses_only_its_area_and_refuses_store_and_dir() {
    let fx = Fx::new();
    fx.virtual_group("Council");
    assert_eq!(fx.slugs(), ["council"]);
    for req in [
        DeleteRequest {
            delete_store: true,
            ..DeleteRequest::default()
        },
        dir_req(),
    ] {
        let why = refused(fx.call("council", &req));
        assert!(why.contains("virtual group"), "{why}");
        assert_eq!(fx.slugs(), ["council"], "nothing changed");
    }
    let out = fx.call("council", &DeleteRequest::default()).unwrap();
    assert_eq!(out.deleted, "council");
    assert!(fx.slugs().is_empty());
    assert!(fx.ws().get_area("council").unwrap().is_none());
    assert!(
        !fx.paths.config_path.exists(),
        "a group never touches the config"
    );
}

// --- store and directory ----------------------------------------------------------------------

#[test]
fn delete_store_removes_the_store_and_its_siblings_and_nothing_else() {
    let fx = Fx::new();
    let root = fx.project("alpha");
    fx.project("beta");
    let store = fx.store_file("alpha");
    let sock = fl_store::default_socket_path_for_store(&store);
    let lock = fl_store::MigrationLock::path_for(&store);
    fs::write(&sock, "").unwrap();
    fs::write(&lock, "").unwrap();
    let beta = fx.store_file("beta");

    let out = fx
        .call(
            "alpha",
            &DeleteRequest {
                delete_store: true,
                ..DeleteRequest::default()
            },
        )
        .unwrap();
    assert!(
        out.store_deleted && !out.dir_deleted && out.error.is_none(),
        "{out:?}"
    );
    for gone in [&store, &sock, &lock] {
        assert!(!gone.exists(), "{} should be gone", gone.display());
    }
    assert!(beta.is_file(), "another project's store stays");
    assert!(
        fx.paths.workspace_store.is_file(),
        "the workspace store stays"
    );
    assert!(root.join(".fl/mcp.toml").is_file(), "the directory stays");
}

#[test]
fn delete_store_refuses_the_workspace_store_and_another_projects_store() {
    let fx = Fx::new();
    fx.project("alpha");
    fx.project("beta");
    // alpha's block names the workspace store, then beta's store.
    for target in [fx.paths.workspace_store.clone(), fx.store_file("beta")] {
        let text = fx.config().replacen(
            &fx.store_file("alpha").display().to_string(),
            &target.display().to_string(),
            1,
        );
        fs::write(&fx.paths.config_path, text).unwrap();
        let before = fx.snapshot(&[&fx.paths.workspace_store, &fx.store_file("beta")]);
        let why = refused(fx.call(
            "alpha",
            &DeleteRequest {
                delete_store: true,
                ..DeleteRequest::default()
            },
        ));
        assert!(
            why.contains("workspace store") || why.contains("another project"),
            "{why}"
        );
        assert_eq!(
            before,
            fx.snapshot(&[&fx.paths.workspace_store, &fx.store_file("beta")])
        );
        let text = fx.config().replacen(
            &target.display().to_string(),
            &fx.store_file("alpha").display().to_string(),
            1,
        );
        fs::write(&fx.paths.config_path, text).unwrap();
    }
}

#[test]
fn delete_dir_removes_the_directory_after_the_unregistration() {
    let fx = Fx::new();
    let root = fx.project("alpha");
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::write(root.join("src/deep/f.rs"), "x").unwrap();
    let other = fx.project("beta");
    let store = fx.store_file("alpha");

    let out = fx.call("alpha", &dir_req()).unwrap();
    assert!(
        out.dir_deleted && !out.store_deleted && out.error.is_none(),
        "{out:?}"
    );
    assert!(!root.exists());
    assert!(other.is_dir() && fx.base.join("work").is_dir());
    assert!(store.is_file(), "the store stays without delete_store");
    assert_eq!(fx.slugs(), ["beta"]);
}

#[test]
fn a_directory_that_cannot_be_deleted_is_reported_without_undoing_the_unregistration() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    let locked = fx.base.join("work/locked");
    fs::create_dir_all(&locked).unwrap();
    let root = fx.project_at(&locked.join("alpha"));
    // The parent is not writable, so the removal fails. A process that ignores permissions
    // (root) is made to fail the same removal on purpose: the test asserts either way.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    let ignores_permissions = fs::write(locked.join("probe"), "").is_ok();
    if ignores_permissions {
        let _ = fs::remove_file(locked.join("probe"));
        hooks::inject(Fault::RemoveDir);
    }
    let out = fx.call("alpha", &dir_req());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    let out = out.unwrap();
    assert!(!out.dir_deleted);
    assert!(
        out.error.as_deref().unwrap().contains("could not delete"),
        "{out:?}"
    );
    assert!(root.is_dir());
    assert!(
        fx.slugs().is_empty(),
        "the project is unregistered all the same"
    );
}

// --- the directory safety rules ---------------------------------------------------------------

/// A project is registered at `root` by hand (as an older or a hand-edited config would have
/// it), then a deletion of its directory is refused with `reason` in the message, and nothing
/// changed: config, area, store, agents and files.
fn assert_dir_refused(fx: &Fx, slug: &str, reason: &str, files: &[&Path]) {
    let before = fx.snapshot(files);
    let why = refused(fx.call(slug, &dir_req()));
    assert!(why.contains(reason), "expected `{reason}` in: {why}");
    assert_eq!(before, fx.snapshot(files), "a refusal changes nothing");
}

#[test]
fn it_refuses_the_filesystem_root() {
    let fx = Fx::new();
    fx.hand_registered("rootish", Path::new("/"));
    fx.agent("Ada", "other", AgentRole::Assistant, &[]);
    assert_dir_refused(&fx, "rootish", "filesystem root", &[Path::new("/")]);
}

#[test]
fn it_refuses_a_shallow_path() {
    let fx = Fx::new();
    // `/tmp` exists everywhere and has one component.
    fx.hand_registered("shallow", Path::new("/tmp"));
    assert_dir_refused(&fx, "shallow", "shallower than", &[Path::new("/tmp")]);
}

#[test]
fn it_refuses_an_empty_or_relative_registered_path() {
    let fx = Fx::new();
    fx.hand_registered("relative", Path::new("work/relative"));
    assert_dir_refused(&fx, "relative", "not absolute", &[]);
    fx.hand_registered("empty", Path::new(""));
    assert_dir_refused(&fx, "empty", "no registered directory", &[]);
}

#[test]
fn it_refuses_a_path_that_cannot_be_resolved() {
    let fx = Fx::new();
    fx.hand_registered("gone", &fx.base.join("work/never-made"));
    assert_dir_refused(&fx, "gone", "cannot be read", &[]);
}

#[test]
fn it_refuses_the_home_directory_and_a_parent_of_it() {
    let fx = Fx::new();
    fx.hand_registered("homeish", &fx.home);
    fs::write(fx.home.join("precious"), "x").unwrap();
    assert_dir_refused(
        &fx,
        "homeish",
        "home directory",
        &[&fx.home.join("precious")],
    );

    // A parent of the home directory: the project is `work`, the home is inside it.
    let parent = fx.base.join("work");
    let home = parent.join("me");
    fs::create_dir_all(&home).unwrap();
    fx.hand_registered("parentish", &parent);
    let why = refused(fx.call_with("parentish", &dir_req(), &home, &fx.ws_root));
    assert!(why.contains("home directory"), "{why}");
    assert!(home.is_dir());
}

#[test]
fn it_refuses_the_workspace_root_and_a_parent_of_it() {
    let fx = Fx::new();
    fs::write(fx.ws_root.join("precious"), "x").unwrap();
    fx.hand_registered("wsroot", &fx.ws_root);
    assert_dir_refused(
        &fx,
        "wsroot",
        "workspace root",
        &[&fx.ws_root.join("precious")],
    );

    let parent = fx.base.join("work");
    let deep_root = parent.join("a/ws");
    fs::create_dir_all(&deep_root).unwrap();
    fx.hand_registered("wsparent", &parent);
    let why = refused(fx.call_with("wsparent", &dir_req(), &fx.home, &deep_root));
    assert!(why.contains("workspace root"), "{why}");
    assert!(deep_root.is_dir());
}

#[test]
fn it_refuses_a_directory_holding_another_registered_project() {
    let fx = Fx::new();
    let outer = fx.project("outer");
    let inner = fx.project_at(&outer.join("inner"));
    fx.agent("Ada", "outer", AgentRole::Assistant, &[]);
    let files = [outer.as_path(), inner.as_path()];
    // Ada is an active agent, so the guard would answer 409 first; reassigning her away shows
    // the directory rule on its own.
    fx.project("elsewhere");
    let before = fx.snapshot(&files);
    let why = refused(fx.call(
        "outer",
        &DeleteRequest {
            delete_dir: true,
            reassign_to: Some("elsewhere".into()),
            ..DeleteRequest::default()
        },
    ));
    assert!(why.contains("another registered project"), "{why}");
    assert_eq!(
        before,
        fx.snapshot(&files),
        "the agent was not moved either"
    );
    assert!(inner.join(".fl/mcp.toml").is_file());
}

#[test]
fn it_refuses_a_directory_holding_or_inside_the_stores_or_config_directory() {
    let fx = Fx::new();
    // Holding the stores directory: the project root is `data`, which holds `data/fl`.
    fx.hand_registered("holder", &fx.base.join("data"));
    assert_dir_refused(
        &fx,
        "holder",
        "store or config directory",
        &[&fx.paths.workspace_store],
    );

    // Inside the stores directory.
    let inside = fx.paths.store_dir.join("sub");
    fs::create_dir_all(&inside).unwrap();
    fx.hand_registered("insider", &inside);
    assert_dir_refused(&fx, "insider", "store or config directory", &[&inside]);

    // Holding the config directory.
    fx.hand_registered("cfgholder", &fx.base.join("cfg"));
    assert_dir_refused(
        &fx,
        "cfgholder",
        "store or config directory",
        &[&fx.paths.config_path],
    );
}

#[test]
fn it_refuses_a_directory_holding_a_store_file() {
    let fx = Fx::new();
    let root = fx.project("alpha");
    // A store outside the stores directory, inside alpha's own directory.
    let own = root.join("my.fls");
    fs::write(&own, "x").unwrap();
    let text = fx.config().replacen(
        &fx.store_file("alpha").display().to_string(),
        &own.display().to_string(),
        1,
    );
    fs::write(&fx.paths.config_path, text).unwrap();
    assert_dir_refused(&fx, "alpha", "store or config file", &[&own, &root]);
}

#[test]
fn it_refuses_a_symbolic_link_even_when_it_points_inside_the_workspace() {
    let fx = Fx::new();
    let outside = fx.base.join("outside/precious");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("f"), "x").unwrap();
    let link = fx.base.join("work/link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    fx.hand_registered("linked", &link);
    assert_dir_refused(&fx, "linked", "symbolic link", &[&outside.join("f"), &link]);
}

#[test]
fn it_refuses_when_the_registered_path_no_longer_matches() {
    let fx = Fx::new();
    let root = fx.project("alpha");
    let moved = fx.base.join("work/moved");
    fs::create_dir_all(&moved).unwrap();
    // The area was moved elsewhere by hand.
    fx.ws()
        .upsert_area(AreaRecord {
            area_key: "alpha".into(),
            name: "alpha".into(),
            path: Some(moved.display().to_string()),
            purpose: "p".into(),
            updated_at: At::from_unix_millis(9),
            by: "tester".into(),
        })
        .unwrap();
    assert_dir_refused(&fx, "alpha", "no longer matches", &[&root, &moved]);
}

#[test]
fn it_refuses_a_project_with_no_directory() {
    let fx = Fx::new();
    // A store file in the data directory makes a project with no path.
    fs::write(fx.store_file("pathless"), "x").unwrap();
    let hints = vec![("pathless".to_string(), Some(fx.store_file("pathless")))];
    let env = LifecycleEnv::new(Some(&fx.home));
    let ctx = DeleteContext {
        paths: &fx.paths,
        stores: &hints,
        home: Some(&fx.home),
        workspace_roots: std::slice::from_ref(&fx.ws_root),
        lifecycle: &env,
    };
    let why = refused(delete_project_in(
        "pathless",
        &dir_req(),
        "tester",
        &ctx,
        Some(fx.home.clone()),
        || fx.open_ws(),
        |_| {},
    ));
    assert!(why.contains("no registered directory"), "{why}");
}

#[test]
fn it_refuses_a_directory_that_is_itself_a_workspace_root_wherever_the_caller_stands() {
    let fx = Fx::new();
    for (name, marker) in [("agentsy", ".agents/memory"), ("notesy", "Notes")] {
        let root = fx.project(name);
        fs::create_dir_all(root.join(marker)).unwrap();
        // The known roots name another directory: the candidate's own markers decide.
        let before = fx.snapshot(&[&root]);
        let why = refused(fx.call_in(
            name,
            &dir_req(),
            Some(&fx.home),
            Some(fx.home.clone()),
            std::slice::from_ref(&fx.ws_root),
        ));
        assert!(why.contains("is a workspace root"), "{marker}: {why}");
        assert_eq!(before, fx.snapshot(&[&root]));
    }
}

#[test]
fn it_fails_closed_when_no_workspace_root_is_known_anywhere() {
    let fx = Fx::new();
    let root = fx.project("alpha");
    let before = fx.snapshot(&[&root]);
    let why = refused(fx.call_in(
        "alpha",
        &dir_req(),
        Some(&fx.home),
        Some(fx.home.clone()),
        &[],
    ));
    assert!(why.contains("no workspace root could be found"), "{why}");
    assert_eq!(before, fx.snapshot(&[&root]), "nothing changed");
    // The same deletion goes through once a root is known.
    let out = fx.call("alpha", &dir_req()).unwrap();
    assert!(out.dir_deleted, "{out:?}");
}

#[test]
fn a_workspace_root_above_the_directory_is_found_without_one_given() {
    let fx = Fx::new();
    // The project lies inside a workspace: the marker above it satisfies the lookup.
    fs::create_dir_all(fx.base.join("work/Notes")).unwrap();
    let root = fx.project_at(&fx.base.join("work/inside/alpha"));
    let out = fx
        .call_in(
            "alpha",
            &dir_req(),
            Some(&fx.home),
            Some(fx.home.clone()),
            &[],
        )
        .unwrap();
    assert!(out.dir_deleted && !root.exists(), "{out:?}");
    assert!(fx.base.join("work/Notes").is_dir());
}

#[test]
fn locate_workspace_roots_looks_at_the_start_the_home_and_the_executable() {
    let fx = Fx::new();
    let nowhere = fx.base.join("work/none");
    fs::create_dir_all(&nowhere).unwrap();
    assert!(
        locate_workspace_roots(&[&nowhere], &[], None).is_empty(),
        "no marker anywhere: none"
    );
    let start = fx.base.join("ws/sub/deep");
    fs::create_dir_all(&start).unwrap();
    fs::create_dir_all(fx.base.join("ws/.agents/memory")).unwrap();
    assert_eq!(
        locate_workspace_roots(&[&start], &[], None),
        [fx.base.join("ws")]
    );
    // Below the home directory: `~/Projects/<name>`.
    let below = fx.home.join("Projects/mine/Notes");
    fs::create_dir_all(&below).unwrap();
    assert_eq!(
        locate_workspace_roots(&[&nowhere], &[&fx.home], None),
        [fx.home.join("Projects/mine")]
    );
    // Beside the executable.
    fs::create_dir_all(fx.base.join("ws/bin")).unwrap();
    let exe = fx.base.join("ws/bin/fl");
    assert_eq!(
        locate_workspace_roots(&[&nowhere], &[], Some(&exe)),
        [fx.base.join("ws")]
    );
    // Without duplicates.
    assert_eq!(
        locate_workspace_roots(&[&start, &start], &[], Some(&exe)),
        [fx.base.join("ws")]
    );
}

#[test]
fn it_refuses_the_directory_when_no_home_directory_is_known() {
    let fx = Fx::new();
    let root = fx.project("alpha");
    let before = fx.snapshot(&[&root]);
    let why = refused(fx.call_in(
        "alpha",
        &dir_req(),
        None,
        None,
        std::slice::from_ref(&fx.ws_root),
    ));
    assert!(why.contains("home directory is unknown"), "{why}");
    assert_eq!(before, fx.snapshot(&[&root]));
    // An empty `$HOME` is no home either.
    let why = refused(fx.call_in(
        "alpha",
        &dir_req(),
        None,
        Some(PathBuf::new()),
        std::slice::from_ref(&fx.ws_root),
    ));
    assert!(why.contains("home directory is unknown"), "{why}");
    // Either one is enough.
    assert!(
        fx.call_in(
            "alpha",
            &dir_req(),
            None,
            Some(fx.home.clone()),
            std::slice::from_ref(&fx.ws_root)
        )
        .unwrap()
        .dir_deleted
    );
}

#[test]
fn every_directory_check_runs_again_right_before_the_removal() {
    let fx = Fx::new();
    let root = fx.project("alpha");
    fs::write(root.join("precious"), "x").unwrap();
    // After the unregistration, the directory becomes a workspace root: the first run of the
    // checks passed, the second does not.
    let marker = root.join("Notes");
    hooks::after_unregister(move || fs::create_dir_all(marker).unwrap());
    let out = fx.call("alpha", &dir_req()).unwrap();
    assert!(!out.dir_deleted, "{out:?}");
    let error = out.error.as_deref().unwrap();
    assert!(
        error.contains("changed after it was checked") && error.contains("workspace root"),
        "{error}"
    );
    assert!(root.join("precious").is_file());
    assert!(fx.slugs().is_empty(), "the unregistration stands");
}

#[test]
fn it_refuses_a_registered_path_that_is_not_its_own_canonical_form() {
    let fx = Fx::new();
    let work = fx.base.join("work");
    // A `..` component.
    fs::create_dir_all(work.join("x")).unwrap();
    let real = fx.project("alpha");
    fx.hand_registered("dotdot", &work.join("x/../alpha"));
    assert_dir_refused(&fx, "dotdot", "`..` component", &[&real]);

    // A link in an ancestor.
    fs::create_dir_all(work.join("real")).unwrap();
    let under = fx.project_at(&work.join("real/beta"));
    std::os::unix::fs::symlink(work.join("real"), work.join("link")).unwrap();
    fx.hand_registered("viaLink", &work.join("link/beta"));
    assert_dir_refused(&fx, "vialink", "canonical path", &[&under]);
    assert!(under.join(".fl/mcp.toml").is_file());
}

#[test]
fn a_trailing_slash_on_the_registered_path_is_the_same_directory() {
    let fx = Fx::new();
    let root = fx.project("alpha");
    let text = fx.config().replacen(
        &root.display().to_string(),
        &format!("{}/", root.display()),
        1,
    );
    assert!(text.contains(&format!("{}/", root.display())), "{text}");
    fs::write(&fx.paths.config_path, text).unwrap();
    let out = fx.call("alpha", &dir_req()).unwrap();
    assert!(out.dir_deleted && !root.exists(), "{out:?}");
}

#[test]
fn delete_store_refuses_a_file_that_is_not_a_store_by_its_name() {
    let fx = Fx::new();
    fx.project("alpha");
    let victim = fx.paths.store_dir.join("alpha.txt");
    fs::write(&victim, "x").unwrap();
    let text = fx.config().replacen(
        &fx.store_file("alpha").display().to_string(),
        &victim.display().to_string(),
        1,
    );
    fs::write(&fx.paths.config_path, text).unwrap();
    let before = fx.snapshot(&[&victim]);
    let why = refused(fx.call(
        "alpha",
        &DeleteRequest {
            delete_store: true,
            ..DeleteRequest::default()
        },
    ));
    assert!(why.contains("not named like a store file"), "{why}");
    assert_eq!(before, fx.snapshot(&[&victim]));
    assert!(victim.is_file());
}

#[test]
fn delete_store_knows_another_projects_store_from_the_data_directory_hints() {
    let fx = Fx::new();
    fx.project("alpha");
    // `ghost` is a store file in the data directory and nothing else (no config block).
    let ghost = fx.store_file("ghost");
    fs::write(&ghost, "x").unwrap();
    let hints: Vec<StoreHint> = vec![("ghost".to_string(), Some(ghost.clone()))];
    let text = fx.config().replacen(
        &fx.store_file("alpha").display().to_string(),
        &ghost.display().to_string(),
        1,
    );
    fs::write(&fx.paths.config_path, text).unwrap();
    let req = DeleteRequest {
        delete_store: true,
        ..DeleteRequest::default()
    };
    let before = fx.snapshot(&[&ghost]);
    let why = refused(fx.call_stores(
        "alpha",
        &req,
        Some(&fx.home),
        Some(fx.home.clone()),
        std::slice::from_ref(&fx.ws_root),
        &hints,
    ));
    assert!(why.contains("another project's store"), "{why}");
    assert_eq!(before, fx.snapshot(&[&ghost]));
    assert!(ghost.is_file());
}

// --- a failure in the middle -------------------------------------------------------------------

#[test]
fn a_config_that_cannot_be_written_puts_the_area_back() {
    let fx = Fx::new();
    let root = fx.project("alpha");
    let area = fx.ws().get_area("alpha").unwrap().unwrap();
    let config = fx.config();
    hooks::inject(Fault::WriteConfig);
    let err = fx.call("alpha", &DeleteRequest::default()).unwrap_err();
    assert!(
        matches!(&err, DeleteError::Init(InitError::Failed(m)) if m.contains("could not write the config")),
        "{err:?}"
    );
    let back = fx.ws().get_area("alpha").unwrap().unwrap();
    assert_eq!(back.path, area.path, "the area is back, with its path");
    assert_eq!(fx.config(), config, "the config is as it was");
    assert_eq!(fx.slugs(), ["alpha"], "the project is still registered");
    assert!(root.is_dir());
}

#[test]
fn an_area_that_cannot_be_deleted_after_the_agents_moved_says_so() {
    let fx = Fx::new();
    fx.project("alpha");
    fx.project("beta");
    fx.agent("Ada", "alpha", AgentRole::Assistant, &[]);
    hooks::inject(Fault::DeleteArea);
    let err = fx
        .call(
            "alpha",
            &DeleteRequest {
                reassign_to: Some("beta".into()),
                ..DeleteRequest::default()
            },
        )
        .unwrap_err();
    let DeleteError::Init(InitError::Failed(m)) = &err else {
        panic!("{err:?}");
    };
    assert!(
        m.contains("1 agents were reassigned") && m.contains("the project is still registered"),
        "{m}"
    );
    assert!(fx.slugs().contains(&"alpha".to_string()));
    assert!(fx.config().contains("alpha"));
    let ada = fx.ws().agent_by_name("ada").unwrap().unwrap();
    assert_eq!(ada.project, "beta", "the agent did move");
}

// --- agents -----------------------------------------------------------------------------------

#[test]
fn active_agents_refuse_the_deletion_and_are_named() {
    let fx = Fx::new();
    fx.project("alpha");
    fx.agent("Ada", "alpha", AgentRole::Assistant, &[]);
    fx.agent("Bo", "other", AgentRole::Assistant, &["alpha"]);
    fx.agent("Cy", "other", AgentRole::Assistant, &[]);
    let before = fx.snapshot(&[&fx.store_file("alpha")]);
    match fx.call("ALPHA", &DeleteRequest::default()) {
        Err(DeleteError::ActiveAgents { agents }) => {
            let mut agents = agents;
            agents.sort();
            assert_eq!(agents, ["Ada", "Bo"]);
            assert_eq!(
                DeleteError::active_agents_body(&agents),
                serde_json::json!({
                    "error": "project has active agents",
                    "active_agent_count": 2,
                    "active_agents": ["Ada", "Bo"],
                })
            );
        }
        other => panic!("expected the agents guard, got {other:?}"),
    }
    assert_eq!(before, fx.snapshot(&[&fx.store_file("alpha")]));
    // A retired agent does not count.
    let ada = fx.ws().agent_by_name("ada").unwrap().unwrap();
    fx.ws()
        .retire_agent(&ada.id, At::from_unix_millis(6), "tester")
        .unwrap();
    let bo = fx.ws().agent_by_name("bo").unwrap().unwrap();
    fx.ws()
        .retire_agent(&bo.id, At::from_unix_millis(7), "tester")
        .unwrap();
    fx.call("alpha", &DeleteRequest::default()).unwrap();
}

#[test]
fn reassign_moves_the_agents_and_the_target_counts_them() {
    let fx = Fx::new();
    fx.project("alpha");
    fx.project("beta");
    fx.virtual_group("Council");
    fx.agent("Ada", "alpha", AgentRole::Assistant, &[]);
    fx.agent("Bo", "beta", AgentRole::Assistant, &["alpha"]);
    fx.agent("Cy", "alpha", AgentRole::Assistant, &["beta", "gamma"]);

    let out = fx
        .call(
            "alpha",
            &DeleteRequest {
                reassign_to: Some("beta".into()),
                ..DeleteRequest::default()
            },
        )
        .unwrap();
    assert_eq!((out.reassigned_agents, out.decommissioned_agents), (3, 0));
    let ws = fx.ws();
    let get = |n: &str| ws.agent_by_name(n).unwrap().unwrap();
    let ada = get("ada");
    assert_eq!(
        (ada.project.as_str(), ada.status),
        ("beta", AgentStatus::Active)
    );
    assert!(
        get("bo").extra_areas.is_empty(),
        "beta is already bo's project"
    );
    let cy = get("cy");
    assert_eq!(cy.project, "beta");
    assert_eq!(cy.extra_areas, ["gamma"]);
    let beta = fx.list().into_iter().find(|p| p.slug == "beta").unwrap();
    assert_eq!(beta.active_agent_count, 3);
    assert!(!fx.slugs().contains(&"alpha".to_string()));

    // A virtual group is a target too.
    fx.project("delta");
    fx.agent("Di", "delta", AgentRole::Assistant, &[]);
    fx.call(
        "delta",
        &DeleteRequest {
            reassign_to: Some("council".into()),
            ..DeleteRequest::default()
        },
    )
    .unwrap();
    assert_eq!(
        fx.ws().agent_by_name("di").unwrap().unwrap().project,
        "council"
    );
}

#[test]
fn reassign_refuses_an_unknown_or_same_target_and_a_second_lead() {
    let fx = Fx::new();
    fx.project("alpha");
    fx.project("beta");
    fx.agent("Ada", "alpha", AgentRole::Assistant, &[]);
    fx.agent("Lea", "alpha", AgentRole::Lead, &[]);
    fx.agent("Lou", "beta", AgentRole::Lead, &[]);
    let before = fx.snapshot(&[&fx.store_file("alpha")]);
    let reassign = |t: &str| DeleteRequest {
        reassign_to: Some(t.into()),
        ..DeleteRequest::default()
    };
    assert!(matches!(
        fx.call("alpha", &reassign("nosuch")),
        Err(DeleteError::Init(InitError::NotFound(_)))
    ));
    assert!(matches!(
        fx.call("alpha", &reassign("alpha")),
        Err(DeleteError::Init(InitError::Invalid(_)))
    ));
    assert!(matches!(
        fx.call("alpha", &reassign("beta")),
        Err(DeleteError::Init(InitError::Conflict(m))) if m.contains("already leads")
    ));
    assert_eq!(before, fx.snapshot(&[&fx.store_file("alpha")]));
}

#[test]
fn reassign_and_decommission_together_are_refused() {
    let fx = Fx::new();
    fx.project("alpha");
    fx.project("beta");
    fx.agent("Ada", "alpha", AgentRole::Assistant, &[]);
    let before = fx.snapshot(&[]);
    let why = refused(fx.call(
        "alpha",
        &DeleteRequest {
            reassign_to: Some("beta".into()),
            decommission_agents: true,
            ..DeleteRequest::default()
        },
    ));
    assert!(why.contains("exclude each other"), "{why}");
    assert_eq!(before, fx.snapshot(&[]));
}

#[test]
fn decommission_retires_the_agents_and_unregisters() {
    let fx = Fx::new();
    fx.project("alpha");
    fx.agent("Ada", "alpha", AgentRole::Assistant, &[]);
    fx.agent("Bo", "other", AgentRole::Assistant, &["alpha"]);
    fx.agent("Cy", "other", AgentRole::Assistant, &[]);
    let out = fx
        .call(
            "alpha",
            &DeleteRequest {
                decommission_agents: true,
                ..DeleteRequest::default()
            },
        )
        .unwrap();
    assert_eq!((out.reassigned_agents, out.decommissioned_agents), (0, 2));
    let ws = fx.ws();
    for (name, status) in [
        ("ada", AgentStatus::Retired),
        ("bo", AgentStatus::Retired),
        ("cy", AgentStatus::Active),
    ] {
        assert_eq!(
            ws.agent_by_name(name).unwrap().unwrap().status,
            status,
            "{name}"
        );
    }
    // `other` is a virtual group Bo and Cy made; alpha is gone.
    assert_eq!(fx.slugs(), ["other"]);
}

#[test]
fn nothing_is_deleted_when_the_agents_are_moved_but_the_directory_is_refused() {
    // The refusal happens in the validation step, before any agent moves.
    let fx = Fx::new();
    fx.hand_registered("homeish", &fx.home);
    fx.project("beta");
    fx.agent("Ada", "homeish", AgentRole::Assistant, &[]);
    let before = fx.snapshot(&[&fx.home]);
    let why = refused(fx.call(
        "homeish",
        &DeleteRequest {
            reassign_to: Some("beta".into()),
            delete_dir: true,
            ..DeleteRequest::default()
        },
    ));
    assert!(why.contains("home directory"), "{why}");
    assert_eq!(before, fx.snapshot(&[&fx.home]));
}

// --- the runtime directory of a virtual scope -------------------------------------------------

fn request(project: &str, cwd: Option<&str>) -> SpinUpRequest {
    SpinUpRequest {
        persona: "ada".into(),
        project: project.into(),
        role: "assistant".into(),
        vendor: "claude".into(),
        cwd: cwd.map(str::to_string),
        ..SpinUpRequest::default()
    }
}

#[test]
fn a_virtual_scope_defaults_to_the_workspace_root_and_a_directory_project_is_unchanged() {
    let fx = Fx::new();
    fx.virtual_group("Council");
    let dir_root = fx.project("alpha");
    let ws = fx.ws();
    let root = &fx.ws_root;

    let virt = request("council", None).with_default_cwd(&ws, root);
    assert_eq!(virt.cwd.as_deref(), Some(root.to_str().unwrap()));
    // A name no area has yet: spin-up makes it as a virtual area.
    let fresh = request("brand-new", None).with_default_cwd(&ws, root);
    assert_eq!(fresh.cwd.as_deref(), Some(root.to_str().unwrap()));
    // A directory-backed project: no default, and its path is not used.
    let dir = request("alpha", None).with_default_cwd(&ws, root);
    assert_eq!(dir.cwd, None);
    let _ = dir_root;
    // An explicit cwd always wins.
    let explicit = request("council", Some("/somewhere")).with_default_cwd(&ws, root);
    assert_eq!(explicit.cwd.as_deref(), Some("/somewhere"));
    let explicit_dir = request("alpha", Some("/elsewhere")).with_default_cwd(&ws, root);
    assert_eq!(explicit_dir.cwd.as_deref(), Some("/elsewhere"));
    // A root that is not a directory on this host is not used.
    let missing = fx.base.join("no-such-dir");
    assert_eq!(
        request("council", None).with_default_cwd(&ws, &missing).cwd,
        None
    );
}

#[test]
fn an_agent_spun_up_in_a_virtual_group_runs_in_the_workspace_root() {
    let fx = Fx::new();
    fx.virtual_group("Council");
    let ws = fx.ws();
    let env = LifecycleEnv::new(Some(&fx.home));
    let req = request("council", None).with_default_cwd(&ws, &fx.ws_root);
    lifecycle::spin_up(&ws, &env, &req, "tester", lifecycle::Caller::Owner).unwrap();
    let record = lifecycle::find_hosted_session(&ws, &["claude:ada"])
        .unwrap()
        .unwrap();
    assert_eq!(record.cwd.as_deref(), fx.ws_root.to_str());
    let ada = ws.agent_by_name("ada").unwrap().unwrap();
    assert_eq!(ada.project, "council");
    assert_eq!(
        fx.list()
            .iter()
            .find(|p| p.slug == "council")
            .unwrap()
            .active_agent_count,
        1
    );
}
