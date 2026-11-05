//! The artifact directory hierarchy: default and effective directories, the path vetting, the
//! relocation endpoints with and without migration, the project routes and serving. Every hub
//! here lives under a temporary directory; nothing touches the real home.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use fl_core::pr_policy::ReviewerApps;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::artifact_dirs::{
    MigrateError, RELOCATE_LOCK, Scope, check_relation, copy_then_remove, count_tree,
    default_project_dir, default_system_dir, effective_project_dir, effective_system_dir,
    ensure_startup_dirs, hub_data_home, is_cross_device, migrate_dir, migrate_dir_with,
    order_entries, resolve_path, rollback, serving_roots, validate_target,
};
use crate::settings;
use crate::{HttpRequest, HttpResponse, HubServerConfig, HubState, handle_http_request};

const OWNER: &str = "owner-tok-secret";
const PEER: &str = "peer-tok-secret";

/// Relocations share one process-wide lock, so every test that relocates runs one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

struct Env {
    _serial: MutexGuard<'static, ()>,
    _tmp: TempDir,
    /// The base of everything the test makes (canonical).
    base: PathBuf,
    state: HubState,
}

impl Env {
    /// `$HOME`, apart from `[home]`.
    fn home(&self) -> PathBuf {
        self.base.join("home")
    }

    /// `[home]`: where the store file is.
    fn data(&self) -> PathBuf {
        self.base.join("data")
    }

    /// A path in a directory that is neither home nor under the old artifacts directory.
    fn target(&self, name: &str) -> PathBuf {
        self.base.join("targets").join(name)
    }
}

fn env() -> Env {
    let serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = TempDir::new().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    for dir in ["home", "data", "targets", "repo"] {
        fs::create_dir_all(base.join(dir)).unwrap();
    }
    let mut state = HubState::new(HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: base.join("data/store.redb"),
        repo_root: base.join("repo"),
        ui_dir: None,
        owner_login: "lmcfarlin".into(),
        reviewer_apps: ReviewerApps::default(),
        owner_token: Some(OWNER.into()),
        peer_token: Some(PEER.into()),
        trust_tailscale_headers: false,
        enable_terminal: false,
        session_ttl_secs: 3600,
        home_override: Some(base.join("home")),
        github: crate::github_pr::HubGithub::default(),
    });
    state.store_backend = Some(fl_store::BackendKind::Ferro);
    Env {
        _serial: serial,
        _tmp: tmp,
        base,
        state,
    }
}

fn request(method: &str, path: &str, token: Option<&str>, body: Option<&Value>) -> HttpRequest {
    let mut req = HttpRequest::new(method, path);
    if let Some(token) = token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    match body {
        Some(body) => req.with_json_body(body),
        None => req,
    }
}

fn send(
    env: &Env,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> HttpResponse {
    handle_http_request(&env.state, &request(method, path, token, body))
}

fn owner_get(env: &Env, path: &str) -> HttpResponse {
    send(env, "GET", path, Some(OWNER), None)
}

fn post_system(env: &Env, token: &str, body: &Value) -> HttpResponse {
    send(
        env,
        "POST",
        "/api/v1/settings/artifacts-dir",
        Some(token),
        Some(body),
    )
}

fn post_project(env: &Env, id: &str, token: &str, body: &Value) -> HttpResponse {
    send(
        env,
        "POST",
        &format!("/api/v1/projects/{id}/artifacts-dir"),
        Some(token),
        Some(body),
    )
}

fn json_of(resp: &HttpResponse) -> Value {
    resp.json_value().unwrap()
}

fn error_of(resp: &HttpResponse) -> String {
    json_of(resp)["error"].as_str().unwrap_or("").to_string()
}

fn stored(env: &Env) -> settings::Settings {
    settings::load(&env.home())
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A system artifacts directory with the files a real one holds: 5 files, 16 bytes.
fn seed_system_dir(dir: &Path) {
    write(&dir.join("registry.json"), "{\"entries\":[]}"); // 14
    write(&dir.join("managed/a.md"), "a"); // 1
    write(&dir.join("managed/sub/b.md"), "b"); // 1
    write(&dir.join("versions/h/1.bin"), ""); // 0
    write(&dir.join("notes.txt"), ""); // 0
}

fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let Ok(entries) = fs::read_dir(&cur) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            out.push(path.strip_prefix(dir).unwrap().display().to_string());
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
}

// --- default and effective directories --------------------------------------------------------

#[test]
fn default_directories_hang_off_the_directory_that_holds_the_store() {
    let env = env();
    let data = env.data();
    assert_eq!(hub_data_home(&env.state), data);
    assert_eq!(default_system_dir(&env.state), data.join("artifacts"));
    assert_eq!(effective_system_dir(&env.state), data.join("artifacts"));
    assert_eq!(
        default_project_dir(&env.state, "ferroloop"),
        data.join("artifacts/ferroloop")
    );
    assert_eq!(
        effective_project_dir(&env.state, "ferroloop"),
        data.join("artifacts/ferroloop")
    );
    // The settings live under $HOME, not beside the store.
    assert!(!data.join(".config").exists());
}

#[test]
fn effective_directories_follow_the_overrides_and_the_default_stays_put() {
    let env = env();
    let sys = env.target("sys");
    let proj = env.target("proj");
    settings::update_artifact_dirs(&env.home(), |s| {
        s.artifacts_dir = Some(sys.display().to_string());
        s.project_artifacts_dirs
            .insert("alpha".into(), proj.display().to_string());
    })
    .unwrap();
    assert_eq!(default_system_dir(&env.state), env.data().join("artifacts"));
    assert_eq!(effective_system_dir(&env.state), sys);
    // A default project dir follows the effective system dir; an override wins outright.
    assert_eq!(default_project_dir(&env.state, "alpha"), sys.join("alpha"));
    assert_eq!(effective_project_dir(&env.state, "alpha"), proj);
    assert_eq!(effective_project_dir(&env.state, "beta"), sys.join("beta"));
}

#[test]
fn settings_views_carry_the_computed_directories_and_agree_with_the_broadcast() {
    let env = env();
    let mut rx = env.state.subscribe();
    let got = json_of(&owner_get(&env, "/api/v1/settings"));
    assert_eq!(got["home_dir"], env.data().display().to_string());
    assert_eq!(
        got["default_artifacts_dir"],
        env.data().join("artifacts").display().to_string()
    );
    assert_eq!(got["effective_artifacts_dir"], got["default_artifacts_dir"]);
    assert_eq!(got["artifacts_dir"], Value::Null);
    assert_eq!(got["project_artifacts_dirs"], json!({}));
    assert_eq!(got["attachment_retention_days"], 7);

    // The PUT response has the same extra fields, and the directories cannot be patched in.
    let put = send(
        &env,
        "PUT",
        "/api/v1/settings",
        Some(OWNER),
        Some(&json!({ "timezone": "Asia/Tokyo" })),
    );
    assert_eq!(put.status, 200);
    let put = json_of(&put);
    assert_eq!(put["home_dir"], got["home_dir"]);
    assert_eq!(
        put["effective_artifacts_dir"],
        got["effective_artifacts_dir"]
    );
    for bad in [
        json!({ "artifacts_dir": "/x" }),
        json!({ "project_artifacts_dirs": { "ferroloop": "/x" } }),
    ] {
        let resp = send(&env, "PUT", "/api/v1/settings", Some(OWNER), Some(&bad));
        assert_eq!(resp.status, 400, "{bad}");
    }
    assert_eq!(stored(&env).artifacts_dir, None);
    while rx.try_recv().is_ok() {}

    // The relocation broadcast and a following GET describe the same settings.
    let new = env.target("moved");
    let resp = post_system(&env, OWNER, &json!({ "new_path": new, "migrate": false }));
    assert_eq!(resp.status, 200, "{}", error_of(&resp));
    let frame = rx.try_recv().unwrap();
    assert_eq!(
        (frame.channel.as_str(), frame.event.as_str()),
        ("settings", "settings.updated")
    );
    let after = json_of(&owner_get(&env, "/api/v1/settings"));
    assert_eq!(frame.data, after);
    assert_eq!(after["effective_artifacts_dir"], new.display().to_string());
    assert_eq!(after["artifacts_dir"], new.display().to_string());
    assert_eq!(
        after["default_artifacts_dir"],
        env.data().join("artifacts").display().to_string()
    );
}

// --- path validation --------------------------------------------------------------------------

fn check(env: &Env, raw: &str, old: &Path) -> Result<PathBuf, String> {
    validate_target(&env.state, raw, old, Scope::System)
}

#[test]
fn validate_target_refuses_unsafe_paths() {
    let env = env();
    let old = env.data().join("artifacts");
    let long = format!("/{}", "a".repeat(5000));
    let cases: Vec<(String, &str)> = vec![
        (String::new(), "empty"),
        ("   ".into(), "empty"),
        (long, "longer"),
        ("/tmp/a\0b".into(), "NUL"),
        ("relative/dir".into(), "absolute"),
        ("./dir".into(), "absolute"),
        ("/tmp/../etc".into(), "`.`"),
        ("/tmp/./x".into(), "`.`"),
        ("/".into(), "root"),
        ("/etc/x".into(), "protected directory"),
        ("/usr/local/art".into(), "protected directory"),
        ("/proc/1".into(), "protected directory"),
        ("/var/run/art".into(), "protected directory"),
        (env.home().display().to_string(), "home directory"),
        (
            env.base.display().to_string(),
            "home directory", // an ancestor of both homes
        ),
        (env.data().display().to_string(), "home directory"),
        (
            env.data().join("attachments").display().to_string(),
            "attachments",
        ),
        (
            env.data().join("attachments/sub").display().to_string(),
            "attachments",
        ),
        (
            env.target(".ssh/art").display().to_string(),
            "protected component",
        ),
        (
            env.target("x/.GnuPG").display().to_string(),
            "protected component",
        ),
        (
            env.target("a/.git/b").display().to_string(),
            "protected component",
        ),
        (
            env.target("secrets").display().to_string(),
            "protected component",
        ),
        (
            env.target(".fl/x").display().to_string(),
            "protected component",
        ),
        (
            env.target("db.redb").display().to_string(),
            "protected path",
        ),
        (
            env.target("store.fls").display().to_string(),
            "protected path",
        ),
        (
            env.target(".env.prod").display().to_string(),
            "protected path",
        ),
    ];
    for (raw, want) in cases {
        let got = check(&env, &raw, &old);
        let err = got.expect_err(&format!("{raw:?} must be refused"));
        assert!(err.contains(want), "{raw:?}: {err}");
    }
    // $HOME's parent is refused as an ancestor.
    assert!(check(&env, &env.base.display().to_string(), &old).is_err());
    assert!(!env.target(".ssh").exists(), "refusal creates nothing");
}

#[test]
fn validate_target_checks_the_resolved_path_not_the_spelling() {
    let env = env();
    let old = env.data().join("artifacts");
    let to_etc = env.target("to-etc");
    std::os::unix::fs::symlink("/etc", &to_etc).unwrap();
    let err = check(&env, &format!("{}/sub", to_etc.display()), &old).unwrap_err();
    assert!(err.contains("/etc"), "{err}");
    assert!(check(&env, &to_etc.display().to_string(), &old).is_err());

    let to_home = env.target("to-home");
    std::os::unix::fs::symlink(env.home(), &to_home).unwrap();
    let err = check(&env, &to_home.display().to_string(), &old).unwrap_err();
    assert!(err.contains("home directory"), "{err}");

    let to_git = env.target("to-git");
    fs::create_dir_all(env.target("repo/.git")).unwrap();
    std::os::unix::fs::symlink(env.target("repo/.git"), &to_git).unwrap();
    assert!(check(&env, &format!("{}/x", to_git.display()), &old).is_err());

    // The error leaves nothing behind.
    assert!(!Path::new("/etc/sub").exists());
}

#[test]
fn validate_target_accepts_a_new_directory_resolved_and_creates_nothing() {
    let env = env();
    let old = env.data().join("artifacts");
    let want = env.target("fresh/deeper");
    let got = check(&env, &format!("  {}  ", want.display()), &old).unwrap();
    assert_eq!(got, want);
    assert!(!want.exists(), "validate_target creates nothing");
    // A symlink to a fine directory is resolved to it.
    let real = env.target("real");
    fs::create_dir_all(&real).unwrap();
    let link = env.target("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(
        check(&env, &format!("{}/new", link.display()), &old).unwrap(),
        real.join("new")
    );
    // A path inside [home] other than attachments is fine.
    assert_eq!(
        check(&env, &env.data().join("custom").display().to_string(), &old).unwrap(),
        env.data().join("custom")
    );
    // The current directory itself is a no-op, not an error.
    assert_eq!(check(&env, &old.display().to_string(), &old).unwrap(), old);
}

#[test]
fn validate_target_refuses_moving_a_directory_into_itself_or_over_its_parent() {
    let env = env();
    let old = env.target("old");
    fs::create_dir_all(&old).unwrap();
    let inside = check(&env, &old.join("sub").display().to_string(), &old).unwrap_err();
    assert!(inside.contains("inside the current one"), "{inside}");
    assert!(inside.contains("itself"), "{inside}");
    let over = check(&env, &env.target("").display().to_string(), &old).unwrap_err();
    assert!(over.contains("contains the current one"), "{over}");
    // Through a symlink too.
    let link = env.target("old-link");
    std::os::unix::fs::symlink(&old, &link).unwrap();
    assert!(check(&env, &format!("{}/deep", link.display()), &old).is_err());
    assert!(check_relation(&old, &old).is_ok());
}

#[test]
fn a_project_directory_may_not_contain_the_system_directory_or_the_registry_files() {
    let env = env();
    let sys = env.target("sys");
    settings::update_artifact_dirs(&env.home(), |s| {
        s.artifacts_dir = Some(sys.display().to_string());
    })
    .unwrap();
    let old = env.base.join("elsewhere/p");
    let scope = Scope::Project("alpha");
    let err = validate_target(
        &env.state,
        &env.target("").display().to_string(),
        &old,
        scope,
    )
    .unwrap_err();
    assert!(err.contains("system artifacts directory"), "{err}");
    let err = validate_target(
        &env.state,
        &sys.join("versions/x").display().to_string(),
        &old,
        scope,
    )
    .unwrap_err();
    assert!(err.contains("registry"), "{err}");
    // Its default place, inside the system directory, is fine.
    assert!(
        validate_target(
            &env.state,
            &sys.join("alpha").display().to_string(),
            &old,
            scope
        )
        .is_ok()
    );
}

#[test]
fn a_system_directory_may_not_be_contain_or_sit_inside_a_project_override() {
    let env = env();
    let proj = env.target("work/proj");
    fs::create_dir_all(&proj).unwrap();
    let alias = env.target("alias");
    std::os::unix::fs::symlink(&proj, &alias).unwrap();
    settings::update_artifact_dirs(&env.home(), |s| {
        s.project_artifacts_dirs
            .insert("alpha".into(), proj.display().to_string());
        // A hand-edited override that does not resolve is skipped, not fatal.
        s.project_artifacts_dirs
            .insert("beta".into(), "/nonexistent-root-xyz/p".into());
    })
    .unwrap();
    let old = env.data().join("artifacts");
    for (raw, what) in [
        (proj.clone(), "equal"),
        (proj.join("inner"), "inside"),
        (env.target("work"), "containing"),
        (alias.join("inner"), "inside through a symlink"),
    ] {
        let err = validate_target(&env.state, &raw.display().to_string(), &old, Scope::System)
            .unwrap_err();
        assert!(err.contains("project `alpha`"), "{what}: {err}");
    }
    let ok = env.target("elsewhere");
    assert_eq!(
        validate_target(&env.state, &ok.display().to_string(), &old, Scope::System).unwrap(),
        ok
    );
    // Through the endpoint it is a 400 and nothing changes.
    let resp = post_system(&env, OWNER, &json!({ "new_path": proj }));
    assert_eq!(resp.status, 400, "{}", error_of(&resp));
    assert_eq!(stored(&env).artifacts_dir, None);
}

// --- migration helpers ------------------------------------------------------------------------

#[test]
fn registry_json_moves_last() {
    let mut names: Vec<OsString> = [
        "zeta",
        "registry.json",
        "alpha",
        "managed",
        "registry.json.corrupt",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    order_entries(&mut names);
    let names: Vec<&str> = names.iter().map(|n| n.to_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "alpha",
            "managed",
            "registry.json.corrupt",
            "zeta",
            "registry.json"
        ]
    );
}

#[test]
fn count_tree_counts_regular_files_and_never_follows_symlinks() {
    let env = env();
    let dir = env.target("count");
    write(&dir.join("a"), "12345");
    write(&dir.join("d/b"), "123");
    let big = env.target("outside/big");
    write(&big, &"x".repeat(1000));
    std::os::unix::fs::symlink(&big, dir.join("link-file")).unwrap();
    std::os::unix::fs::symlink(big.parent().unwrap(), dir.join("link-dir")).unwrap();
    assert_eq!(count_tree(&dir), (2, 8));
    assert_eq!(count_tree(&dir.join("a")), (1, 5));
    assert_eq!(count_tree(&dir.join("link-dir")), (0, 0));
}

#[test]
fn the_cross_device_error_is_recognised() {
    assert!(is_cross_device(&io::Error::from_raw_os_error(18)));
    assert!(is_cross_device(&io::Error::from(
        io::ErrorKind::CrossesDevices
    )));
    assert!(!is_cross_device(&io::Error::from_raw_os_error(2)));
    assert!(!is_cross_device(&io::Error::from(
        io::ErrorKind::PermissionDenied
    )));
}

#[test]
fn the_copy_fallback_moves_a_tree_keeps_symlinks_as_symlinks_and_removes_the_source() {
    let env = env();
    let src = env.target("src");
    write(&src.join("top.md"), "top");
    write(&src.join("sub/deep/f.md"), "deep");
    let outside = env.target("outside.txt");
    write(&outside, "secret");
    std::os::unix::fs::symlink(&outside, src.join("sub/abs-link")).unwrap();
    std::os::unix::fs::symlink("top.md", src.join("rel-link")).unwrap();
    std::os::unix::fs::symlink("missing-target", src.join("dangling")).unwrap();
    std::os::unix::fs::symlink(src.join("sub"), src.join("dir-link")).unwrap();
    let dst = env.target("dst");

    copy_then_remove(&src, &dst).unwrap();

    assert!(!src.exists(), "the source is removed");
    assert_eq!(fs::read_to_string(dst.join("top.md")).unwrap(), "top");
    assert_eq!(
        fs::read_to_string(dst.join("sub/deep/f.md")).unwrap(),
        "deep"
    );
    for (name, target) in [
        ("sub/abs-link", outside.clone()),
        ("rel-link", PathBuf::from("top.md")),
        ("dangling", PathBuf::from("missing-target")),
        ("dir-link", src.join("sub")),
    ] {
        let meta = fs::symlink_metadata(dst.join(name)).unwrap();
        assert!(meta.file_type().is_symlink(), "{name} must stay a symlink");
        assert_eq!(fs::read_link(dst.join(name)).unwrap(), target, "{name}");
    }
    // Removing the source did not follow a link out of it.
    assert_eq!(fs::read_to_string(&outside).unwrap(), "secret");
    assert!(dst.join("sub/deep").is_dir());
}

#[test]
fn a_failed_copy_removes_the_partial_destination_and_leaves_the_source() {
    let env = env();
    let src = env.target("src");
    write(&src.join("a.md"), "a");
    write(&src.join("sub/b.md"), "b");
    // A socket cannot be copied, so the copy fails part-way.
    let _socket = std::os::unix::net::UnixListener::bind(src.join("sub/sock")).unwrap();
    let dst = env.target("dst");

    let err = copy_then_remove(&src, &dst).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
    assert!(!dst.exists(), "the partial copy is removed");
    assert_eq!(fs::read_to_string(src.join("a.md")).unwrap(), "a");
    assert_eq!(fs::read_to_string(src.join("sub/b.md")).unwrap(), "b");
    assert!(fs::symlink_metadata(src.join("sub/sock")).is_ok());
}

#[test]
fn migrate_dir_refuses_on_a_clash_and_moves_nothing() {
    let env = env();
    let old = env.target("old");
    let new = env.target("new");
    seed_system_dir(&old);
    write(&new.join("managed/existing.md"), "x");
    write(&new.join("notes.txt"), "y");
    let before = (tree(&old), tree(&new));
    let Err(MigrateError::Conflict(names)) = migrate_dir(&old, &new) else {
        panic!("a clash must refuse");
    };
    assert_eq!(names, ["managed", "notes.txt"]);
    assert_eq!((tree(&old), tree(&new)), before);
}

#[test]
fn rollback_puts_moved_entries_back() {
    let env = env();
    let old = env.target("old");
    let new = env.target("new");
    seed_system_dir(&old);
    let before = tree(&old);
    let done = migrate_dir(&old, &new).unwrap();
    assert!(tree(&old).is_empty());
    assert!(rollback(&old, &new, &done.moved));
    assert_eq!(tree(&old), before);
    assert!(tree(&new).is_empty());
}

#[test]
fn a_copy_that_cannot_remove_its_source_is_reported_as_kept_in_both_places() {
    use std::os::unix::fs::PermissionsExt;
    let env = env();
    let old = env.target("old");
    let new = env.target("new");
    write(&old.join("a/one.md"), "1");
    write(&old.join("b/locked/two.md"), "22");
    // A read-only directory: the copy works, deleting the original fails part-way.
    let locked = old.join("b/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let probe = copy_then_remove(&old.join("b"), &env.target("probe"));
    if probe.is_ok() {
        // Running as a user the permission bits do not bind (root): nothing to test.
        return;
    }
    let _ = fs::remove_dir_all(env.target("probe"));
    let result = migrate_dir_with(&old, &new, &copy_then_remove);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let Err(MigrateError::Failed {
        message,
        files_moved,
        rolled_back,
    }) = result
    else {
        panic!("the removal failure must fail the migration: {result:?}");
    };
    assert!(
        !rolled_back,
        "an entry left in both places is not rolled back"
    );
    assert_eq!(files_moved, 1);
    assert!(message.contains("`b`"), "{message}");
    assert!(message.contains("both directories"), "{message}");
    // The first entry went back; `b` is still whole in both places.
    assert_eq!(fs::read_to_string(old.join("a/one.md")).unwrap(), "1");
    assert!(!new.join("a").exists());
    assert_eq!(
        fs::read_to_string(new.join("b/locked/two.md")).unwrap(),
        "22"
    );
    assert!(old.join("b").exists());
}

#[test]
fn an_entry_that_appears_in_the_target_after_the_preflight_is_a_clash() {
    let env = env();
    let old = env.target("old");
    let new = env.target("new");
    write(&old.join("a/one.md"), "1");
    write(&old.join("b"), "old b");
    let before = tree(&old);
    // The mover plants `b` in the target while the first entry moves, after the preflight.
    let result = migrate_dir_with(&old, &new, &|from, to| {
        write(&new.join("b"), "newcomer");
        fs::rename(from, to)
    });
    let Err(MigrateError::Conflict(names)) = result else {
        panic!("a late clash must be a conflict: {result:?}");
    };
    assert_eq!(names, ["b"]);
    assert_eq!(tree(&old), before, "what moved is put back");
    assert_eq!(fs::read_to_string(new.join("b")).unwrap(), "newcomer");
    assert_eq!(fs::read_to_string(old.join("b")).unwrap(), "old b");
}

// --- the system endpoint ----------------------------------------------------------------------

#[test]
fn migrating_moves_files_subdirectories_and_the_registry_and_counts_them() {
    let env = env();
    let old = env.data().join("artifacts");
    seed_system_dir(&old);
    let new = env.target("moved");
    let resp = post_system(&env, OWNER, &json!({ "new_path": new, "migrate": true }));
    assert_eq!(resp.status, 200, "{}", error_of(&resp));
    let out = json_of(&resp);
    assert_eq!(out["old_path"], old.display().to_string());
    assert_eq!(out["new_path"], new.display().to_string());
    assert_eq!(out["migrated_files"], 5);
    assert_eq!(out["migrated_bytes"], 16);
    assert_eq!(out["artifacts_dir"], new.display().to_string());
    assert_eq!(out["effective_artifacts_dir"], new.display().to_string());
    assert_eq!(out["default_artifacts_dir"], old.display().to_string());
    assert_eq!(
        tree(&new),
        [
            "managed",
            "managed/a.md",
            "managed/sub",
            "managed/sub/b.md",
            "notes.txt",
            "registry.json",
            "versions",
            "versions/h",
            "versions/h/1.bin"
        ]
    );
    assert_eq!(
        fs::read_to_string(new.join("registry.json")).unwrap(),
        "{\"entries\":[]}"
    );
    assert!(tree(&old).is_empty(), "the old directory is emptied");
    assert_eq!(stored(&env).artifacts_dir, Some(new.display().to_string()));
    assert_eq!(effective_system_dir(&env.state), new);
}

#[test]
fn the_registry_and_the_storage_summary_follow_the_setting() {
    let env = env();
    let old = env.data().join("artifacts");
    let listed = |env: &Env| json_of(&owner_get(env, "/api/v1/artifacts/list"));
    assert_eq!(
        listed(&env)["storage"]["root_dir"],
        old.display().to_string()
    );
    let new = env.target("moved");
    let resp = post_system(&env, OWNER, &json!({ "new_path": new }));
    assert_eq!(resp.status, 200);
    assert_eq!(
        listed(&env)["storage"]["root_dir"],
        new.display().to_string()
    );
}

#[test]
fn a_migration_clash_is_a_409_that_names_at_most_five_and_moves_nothing() {
    let env = env();
    let old = env.data().join("artifacts");
    for i in 0..8 {
        write(&old.join(format!("f{i}.md")), "x");
        write(&env.target("new").join(format!("f{i}.md")), "y");
    }
    let before = (tree(&old), tree(&env.target("new")));
    let resp = post_system(
        &env,
        OWNER,
        &json!({ "new_path": env.target("new"), "migrate": true }),
    );
    assert_eq!(resp.status, 409);
    let err = error_of(&resp);
    assert_eq!(err.matches("`f").count(), 5, "{err}");
    assert!(err.contains("and 3 more"), "{err}");
    assert!(err.contains("nothing was moved"), "{err}");
    assert_eq!((tree(&old), tree(&env.target("new"))), before);
    assert_eq!(stored(&env).artifacts_dir, None, "the setting is unchanged");
    assert_eq!(fs::read_to_string(old.join("f0.md")).unwrap(), "x");
}

#[test]
fn a_clash_does_not_create_a_missing_target() {
    let env = env();
    let old = env.data().join("artifacts");
    write(&old.join("a.md"), "x");
    // A clash needs the target to exist, so this one cannot clash and is created.
    let new = env.target("not-yet/deeper");
    assert!(!new.exists());
    let resp = post_system(&env, OWNER, &json!({ "new_path": new, "migrate": true }));
    assert_eq!(resp.status, 200);
    assert!(new.join("a.md").is_file());
    // A refused request creates nothing.
    let refused = env.target("no/.ssh/dir");
    let resp = post_system(
        &env,
        OWNER,
        &json!({ "new_path": refused, "migrate": true }),
    );
    assert_eq!(resp.status, 400);
    assert!(!env.target("no").exists());
}

#[test]
fn without_migrate_only_the_setting_changes() {
    let env = env();
    let old = env.data().join("artifacts");
    seed_system_dir(&old);
    let before = tree(&old);
    let new = env.target("elsewhere");
    for body in [
        json!({ "new_path": new }),
        json!({ "new_path": new, "migrate": false }),
    ] {
        let resp = post_system(&env, OWNER, &body);
        assert_eq!(resp.status, 200, "{}", error_of(&resp));
        let out = json_of(&resp);
        assert_eq!(out["migrated_files"], 0);
        assert_eq!(out["migrated_bytes"], 0);
        assert_eq!(tree(&old), before, "old files stay where they are");
        assert!(new.is_dir(), "the new directory is created for use");
        assert!(tree(&new).is_empty());
        assert_eq!(effective_system_dir(&env.state), new);
    }
}

#[test]
fn resetting_returns_to_the_default_and_can_migrate_back() {
    let env = env();
    let default = env.data().join("artifacts");
    let new = env.target("moved");
    seed_system_dir(&default);
    assert_eq!(
        post_system(&env, OWNER, &json!({ "new_path": new, "migrate": true })).status,
        200
    );
    assert!(tree(&default).is_empty());

    for blank in [Value::Null, json!(""), json!("   ")] {
        let resp = post_system(&env, OWNER, &json!({ "new_path": blank, "migrate": true }));
        assert_eq!(resp.status, 200, "{blank}: {}", error_of(&resp));
        let out = json_of(&resp);
        assert_eq!(out["new_path"], default.display().to_string());
        assert_eq!(out["artifacts_dir"], Value::Null);
        assert_eq!(
            out["effective_artifacts_dir"],
            default.display().to_string()
        );
        assert_eq!(stored(&env).artifacts_dir, None);
        assert_eq!(effective_system_dir(&env.state), default);
        assert!(default.join("registry.json").is_file());
        // Put it back for the next blank spelling.
        assert_eq!(
            post_system(&env, OWNER, &json!({ "new_path": new, "migrate": true })).status,
            200
        );
    }
    // Reset without migrating leaves the files where they were.
    let resp = post_system(&env, OWNER, &json!({ "new_path": null }));
    assert_eq!(resp.status, 200);
    assert!(new.join("registry.json").is_file());
    assert_eq!(effective_system_dir(&env.state), default);
}

#[test]
fn only_the_owner_may_relocate_and_a_bad_request_is_a_400() {
    let env = env();
    let new = env.target("moved");
    let body = json!({ "new_path": new, "migrate": true });
    let peer = post_system(&env, PEER, &body);
    assert_eq!(peer.status, 403);
    assert_eq!(
        send(
            &env,
            "POST",
            "/api/v1/settings/artifacts-dir",
            None,
            Some(&body)
        )
        .status,
        401
    );
    assert!(!new.exists());
    assert_eq!(stored(&env).artifacts_dir, None);
    assert!(
        !settings::settings_path(&env.home()).exists(),
        "no settings file written"
    );

    for bad in [
        json!({}),
        json!({ "migrate": true }),
        json!({ "new_path": new, "extra": 1 }),
        json!({ "new_path": 5 }),
        json!({ "new_path": new, "migrate": "yes" }),
        json!({ "new_path": "relative" }),
        json!({ "new_path": "/etc/art" }),
        json!({ "new_path": env.home().display().to_string() }),
    ] {
        let resp = post_system(&env, OWNER, &bad);
        assert_eq!(resp.status, 400, "{bad}: {}", error_of(&resp));
    }
    assert_eq!(stored(&env).artifacts_dir, None);
    // Not JSON at all.
    let mut req = request("POST", "/api/v1/settings/artifacts-dir", Some(OWNER), None);
    req.body = b"not json".to_vec();
    assert_eq!(handle_http_request(&env.state, &req).status, 400);
}

#[test]
fn a_second_relocation_while_one_runs_is_a_409() {
    let env = env();
    let new = env.target("moved");
    let held = RELOCATE_LOCK.lock().unwrap();
    let resp = post_system(&env, OWNER, &json!({ "new_path": new, "migrate": true }));
    assert_eq!(resp.status, 409);
    assert!(error_of(&resp).contains("another"), "{}", error_of(&resp));
    assert!(!new.exists());
    assert_eq!(stored(&env).artifacts_dir, None);
    drop(held);
    assert_eq!(
        post_system(&env, OWNER, &json!({ "new_path": new })).status,
        200
    );
}

#[test]
fn a_damaged_settings_file_is_a_409_and_nothing_moves() {
    let env = env();
    let old = env.data().join("artifacts");
    seed_system_dir(&old);
    let path = settings::settings_path(&env.home());
    write(&path, "{ not json");
    let new = env.target("moved");
    for migrate in [true, false] {
        let resp = post_system(&env, OWNER, &json!({ "new_path": new, "migrate": migrate }));
        assert_eq!(resp.status, 409, "{}", error_of(&resp));
        assert!(error_of(&resp).contains("damaged"), "{}", error_of(&resp));
    }
    assert!(!new.exists());
    assert!(old.join("registry.json").is_file());
    assert_eq!(fs::read(&path).unwrap(), b"{ not json");
    let resp = post_project(&env, "ferroloop", OWNER, &json!({ "new_path": new }));
    assert_eq!(resp.status, 409);
}

// --- the project routes -----------------------------------------------------------------------

fn add_project(env: &Env, slug: &str) {
    write(&env.home().join(format!(".local/share/fl/{slug}.fls")), "x");
}

#[test]
fn get_projects_lists_every_project_with_its_directories() {
    let env = env();
    add_project(&env, "alpha");
    let sys = env.data().join("artifacts");
    let proj = env.target("alpha-art");
    settings::update_artifact_dirs(&env.home(), |s| {
        s.project_artifacts_dirs
            .insert("alpha".into(), proj.display().to_string());
    })
    .unwrap();
    let resp = owner_get(&env, "/api/v1/projects");
    assert_eq!(resp.status, 200);
    assert_eq!(
        json_of(&resp),
        json!({ "projects": [
            {
                "id": "alpha",
                "slug": "alpha",
                "name": "alpha",
                "kind": "project",
                "path": null,
                "has_git": false,
                "purpose": "",
                "tracker": null,
                "store_path": env.home().join(".local/share/fl/alpha.fls").display().to_string(),
                "active_agent_count": 0,
                "artifacts_dir": proj.display().to_string(),
                "default_artifacts_dir": sys.join("alpha").display().to_string(),
                "effective_artifacts_dir": proj.display().to_string(),
            },
            {
                "id": "ferroloop",
                "slug": "ferroloop",
                "name": "ferroloop",
                "kind": "project",
                "path": null,
                "has_git": false,
                "purpose": "",
                "tracker": null,
                "store_path": null,
                "active_agent_count": 0,
                "artifacts_dir": null,
                "default_artifacts_dir": sys.join("ferroloop").display().to_string(),
                "effective_artifacts_dir": sys.join("ferroloop").display().to_string(),
            },
        ] })
    );
    assert_eq!(
        send(&env, "GET", "/api/v1/projects", None, None).status,
        401
    );
}

#[test]
fn the_project_routes_normalize_the_id_and_decode_it() {
    let env = env();
    let sys = env.data().join("artifacts");
    for spelling in ["FerroLoop", "ferroloop", "%66erroloop", "FERROLOOP"] {
        let resp = owner_get(&env, &format!("/api/v1/projects/{spelling}/artifacts-dir"));
        assert_eq!(resp.status, 200, "{spelling}: {}", error_of(&resp));
        assert_eq!(
            json_of(&resp),
            json!({
                "id": "ferroloop",
                "artifacts_dir": null,
                "default_artifacts_dir": sys.join("ferroloop").display().to_string(),
                "effective_artifacts_dir": sys.join("ferroloop").display().to_string(),
            }),
            "{spelling}"
        );
    }
    let missing = owner_get(&env, "/api/v1/projects/nosuch/artifacts-dir");
    assert_eq!(missing.status, 404);
    for bad in ["bad%20slug", "a.b", "%20%20", &"x".repeat(65)] {
        let resp = owner_get(&env, &format!("/api/v1/projects/{bad}/artifacts-dir"));
        assert_eq!(resp.status, 400, "{bad}");
    }
    assert_eq!(
        send(
            &env,
            "GET",
            "/api/v1/projects/ferroloop/artifacts-dir",
            None,
            None
        )
        .status,
        401
    );
}

#[test]
fn a_project_override_is_set_read_back_and_reset() {
    let env = env();
    add_project(&env, "alpha");
    let new = env.target("alpha-art");
    let resp = post_project(
        &env,
        "Alpha",
        OWNER,
        &json!({ "new_path": new, "migrate": false }),
    );
    assert_eq!(resp.status, 200, "{}", error_of(&resp));
    let out = json_of(&resp);
    assert_eq!(out["project"], "alpha");
    assert_eq!(out["new_path"], new.display().to_string());
    assert_eq!(out["artifacts_dir"], new.display().to_string());
    assert_eq!(out["effective_artifacts_dir"], new.display().to_string());
    assert_eq!(
        out["default_artifacts_dir"],
        env.data().join("artifacts/alpha").display().to_string()
    );
    assert!(new.is_dir());
    assert_eq!(
        stored(&env).project_artifacts_dirs.get("alpha"),
        Some(&new.display().to_string())
    );
    assert_eq!(effective_project_dir(&env.state, "alpha"), new);
    let got = json_of(&owner_get(&env, "/api/v1/projects/alpha/artifacts-dir"));
    assert_eq!(got["artifacts_dir"], new.display().to_string());
    // The system directory is untouched.
    assert_eq!(stored(&env).artifacts_dir, None);

    let reset = post_project(&env, "alpha", OWNER, &json!({ "new_path": null }));
    assert_eq!(reset.status, 200, "{}", error_of(&reset));
    assert!(
        stored(&env).project_artifacts_dirs.is_empty(),
        "the key is removed"
    );
    assert_eq!(
        effective_project_dir(&env.state, "alpha"),
        env.data().join("artifacts/alpha")
    );
}

#[test]
fn a_project_override_can_migrate_the_old_project_directory() {
    let env = env();
    let default = env.data().join("artifacts/ferroloop");
    write(&default.join("one.md"), "1");
    write(&default.join("d/two.md"), "22");
    let new = env.target("fl");
    let resp = post_project(
        &env,
        "ferroloop",
        OWNER,
        &json!({ "new_path": new, "migrate": true }),
    );
    assert_eq!(resp.status, 200, "{}", error_of(&resp));
    let out = json_of(&resp);
    assert_eq!(out["migrated_files"], 2);
    assert_eq!(out["migrated_bytes"], 3);
    assert_eq!(tree(&new), ["d", "d/two.md", "one.md"]);
    assert!(tree(&default).is_empty());
}

#[test]
fn project_relocation_is_owner_only_and_unknown_projects_are_404() {
    let env = env();
    let new = env.target("fl");
    let body = json!({ "new_path": new });
    assert_eq!(post_project(&env, "ferroloop", PEER, &body).status, 403);
    assert_eq!(post_project(&env, "nosuch", OWNER, &body).status, 404);
    assert_eq!(post_project(&env, "bad%20slug", OWNER, &body).status, 400);
    assert_eq!(
        send(
            &env,
            "POST",
            "/api/v1/projects/ferroloop/artifacts-dir",
            None,
            Some(&body)
        )
        .status,
        401
    );
    assert!(!new.exists());
    assert!(stored(&env).project_artifacts_dirs.is_empty());
    // A peer is refused before the project is even looked up.
    assert_eq!(post_project(&env, "nosuch", PEER, &body).status, 403);
}

#[test]
fn the_artifacts_dir_route_does_not_shadow_the_project_merge_routes() {
    let env = env();
    let unconfigured = "GitHub is not configured on this Hub: no repository is bound to project \
                        `ferroloop`";
    // `/prs/:n/merge` reaches the merge handler: the number is in the path, and the test hub has
    // no GitHub repository, so the handler answers 503.
    let resp = send(
        &env,
        "POST",
        "/api/v1/projects/ferroloop/prs/5/merge",
        Some(OWNER),
        Some(&json!({})),
    );
    assert_eq!((resp.status, error_of(&resp)), (503, unconfigured.into()));
    // `/pr/merge` reaches it too: with no number in the path or the body it is a 400 from the
    // handler; with one it gets as far as the missing repository.
    let resp = send(
        &env,
        "POST",
        "/api/v1/projects/ferroloop/pr/merge",
        Some(OWNER),
        Some(&json!({})),
    );
    assert_eq!(
        (resp.status, error_of(&resp)),
        (400, "a pull request number is required".into())
    );
    let resp = send(
        &env,
        "POST",
        "/api/v1/projects/ferroloop/pr/merge",
        Some(OWNER),
        Some(&json!({ "number": 5 })),
    );
    assert_eq!((resp.status, error_of(&resp)), (503, unconfigured.into()));

    // The artifacts-dir path is five segments with that exact last segment and takes GET and
    // POST only: any other method falls through the router to its 404.
    let resp = send(
        &env,
        "DELETE",
        "/api/v1/projects/ferroloop/artifacts-dir",
        Some(OWNER),
        None,
    );
    assert_eq!(resp.status, 404);
    assert_eq!(error_of(&resp), "not found");
    // A different last segment, or an extra one, is not the artifacts-dir route either.
    for path in [
        "/api/v1/projects/ferroloop/artifacts",
        "/api/v1/projects/ferroloop/artifacts-dir/x",
    ] {
        for method in ["GET", "POST"] {
            let resp = send(&env, method, path, Some(OWNER), Some(&json!({})));
            assert_eq!(resp.status, 404, "{method} {path}");
            assert_eq!(error_of(&resp), "not found", "{method} {path}");
        }
    }
    // And the real thing is the 200 these tests use elsewhere.
    let resp = owner_get(&env, "/api/v1/projects/ferroloop/artifacts-dir");
    assert_eq!(resp.status, 200);
    assert_eq!(json_of(&resp)["id"], "ferroloop");
}

// --- serving ----------------------------------------------------------------------------------

fn preview(env: &Env, path: &Path) -> HttpResponse {
    let enc = crate::api::percent_encode_query_value(&path.display().to_string());
    owner_get(env, &format!("/api/v1/artifacts/preview?path={enc}"))
}

#[test]
fn a_new_system_directory_and_a_project_override_become_servable() {
    let env = env();
    let sys = env.target("sys");
    let proj = env.target("proj");
    write(&sys.join("managed/note.md"), "# system");
    write(&sys.join("loose.md"), "# loose");
    write(&proj.join("p.md"), "# project");
    // Before the settings name them, neither is served.
    assert_eq!(preview(&env, &sys.join("loose.md")).status, 403);
    assert_eq!(preview(&env, &proj.join("p.md")).status, 403);

    assert_eq!(
        post_system(&env, OWNER, &json!({ "new_path": sys })).status,
        200
    );
    assert_eq!(
        post_project(&env, "ferroloop", OWNER, &json!({ "new_path": proj })).status,
        200
    );

    for (path, content) in [
        (sys.join("managed/note.md"), "# system"),
        (sys.join("loose.md"), "# loose"),
        (proj.join("p.md"), "# project"),
    ] {
        let resp = preview(&env, &path);
        assert_eq!(resp.status, 200, "{}: {}", path.display(), error_of(&resp));
        assert_eq!(json_of(&resp)["content"], content);
    }
    // The default system directory is still served, and the registry's own files never are.
    let default = env.data().join("artifacts");
    write(&default.join("managed/d.md"), "# default");
    assert_eq!(preview(&env, &default.join("managed/d.md")).status, 200);
    write(&sys.join("registry.json"), "{}");
    write(&sys.join("registry.json.corrupt"), "{}");
    write(&sys.join("versions/h/x.md"), "# v");
    for path in [
        sys.join("registry.json"),
        sys.join("registry.json.corrupt"),
        sys.join("versions/h/x.md"),
        default.join("registry.json"),
    ] {
        assert_eq!(preview(&env, &path).status, 403, "{}", path.display());
    }
}

#[test]
fn an_override_never_makes_a_forbidden_path_servable() {
    let env = env();
    let proj = env.target("proj");
    write(&proj.join(".git/config.md"), "# git");
    write(&proj.join("secrets/s.md"), "# secret");
    write(&proj.join("ok.md"), "# ok");
    assert_eq!(
        post_project(&env, "ferroloop", OWNER, &json!({ "new_path": proj })).status,
        200
    );
    assert_eq!(preview(&env, &proj.join("ok.md")).status, 200);
    assert_eq!(preview(&env, &proj.join(".git/config.md")).status, 403);
    assert_eq!(preview(&env, &proj.join("secrets/s.md")).status, 403);
    assert_eq!(preview(&env, Path::new("/etc/passwd")).status, 403);

    // A hand-edited settings file naming a protected directory serves nothing from it.
    settings::update_artifact_dirs(&env.home(), |s| {
        s.artifacts_dir = Some("/etc".into());
        s.project_artifacts_dirs
            .insert("ferroloop".into(), "/etc".into());
    })
    .unwrap();
    assert_eq!(preview(&env, Path::new("/etc/passwd")).status, 403);
    assert_eq!(preview(&env, Path::new("/etc/hostname")).status, 403);
    // A symlink inside the system directory is not a root.
    let sys = env.target("sys");
    fs::create_dir_all(&sys).unwrap();
    std::os::unix::fs::symlink("/etc", sys.join("link")).unwrap();
    settings::update_artifact_dirs(&env.home(), |s| {
        s.artifacts_dir = Some(sys.display().to_string());
        s.project_artifacts_dirs.clear();
    })
    .unwrap();
    assert_eq!(preview(&env, &sys.join("link/passwd")).status, 403);
}

#[test]
fn a_project_root_that_reaches_the_registry_files_is_not_served() {
    let env = env();
    let sys = env.target("sys");
    write(&sys.join("registry.json"), "{}");
    write(&sys.join("versions/h/x.md"), "# v");
    write(&sys.join("managed/m.md"), "# m");
    write(&env.target("sibling.md"), "# sibling");
    let inner = sys.join("versions/h");
    // Hand-edited settings: the project roots are the parent of the system directory and a
    // directory inside the registry's versions.
    settings::update_artifact_dirs(&env.home(), |s| {
        s.artifacts_dir = Some(sys.display().to_string());
        s.project_artifacts_dirs
            .insert("alpha".into(), env.target("").display().to_string());
        s.project_artifacts_dirs
            .insert("beta".into(), inner.display().to_string());
    })
    .unwrap();
    for path in [
        sys.join("registry.json"),
        sys.join("versions/h/x.md"),
        env.target("sibling.md"),
    ] {
        assert_eq!(preview(&env, &path).status, 403, "{}", path.display());
    }
    // The system directory's own entries are still served.
    assert_eq!(preview(&env, &sys.join("managed/m.md")).status, 200);
    let roots = serving_roots(&env.state);
    assert!(
        roots
            .iter()
            .all(|r| !sys.starts_with(r) && !r.starts_with(sys.join("versions"))),
        "{roots:?}"
    );
}

#[test]
fn temporary_entries_of_the_system_directory_are_not_served() {
    let env = env();
    let sys = env.target("sys");
    write(&sys.join(".tmp-abc123/half.md"), "# partial");
    write(&sys.join("managed/ok.md"), "# ok");
    assert_eq!(
        post_system(&env, OWNER, &json!({ "new_path": sys })).status,
        200
    );
    assert_eq!(preview(&env, &sys.join("managed/ok.md")).status, 200);
    assert_eq!(preview(&env, &sys.join(".tmp-abc123/half.md")).status, 403);
}

#[test]
fn a_hand_edited_protected_directory_is_never_written_to_or_created() {
    let env = env();
    let bad_sys = env.target("x/.ssh/arts");
    let bad_proj = env.target("y/.git/proj");
    settings::update_artifact_dirs(&env.home(), |s| {
        s.artifacts_dir = Some(bad_sys.display().to_string());
        s.project_artifacts_dirs
            .insert("ferroloop".into(), bad_proj.display().to_string());
        s.project_artifacts_dirs
            .insert("home".into(), env.home().display().to_string());
    })
    .unwrap();
    let default = env.data().join("artifacts");
    assert_eq!(effective_system_dir(&env.state), default);
    assert_eq!(
        effective_project_dir(&env.state, "ferroloop"),
        default.join("ferroloop")
    );
    assert_eq!(
        effective_project_dir(&env.state, "home"),
        default.join("home")
    );
    let view = json_of(&owner_get(&env, "/api/v1/settings"));
    assert_eq!(
        view["effective_artifacts_dir"],
        default.display().to_string()
    );
    // The stored value is still shown as stored; only the directory in use changes.
    assert_eq!(view["artifacts_dir"], bad_sys.display().to_string());

    ensure_startup_dirs(&env.state);
    assert!(default.is_dir());
    assert!(!env.target("x").exists() && !env.target("y").exists());

    // A relocation measures from the directory in use, not the stored one, and does not create it.
    let new = env.target("fine");
    let resp = post_system(&env, OWNER, &json!({ "new_path": new, "migrate": true }));
    assert_eq!(resp.status, 200, "{}", error_of(&resp));
    assert_eq!(json_of(&resp)["old_path"], default.display().to_string());
    assert!(!env.target("x").exists());
}

// --- startup ----------------------------------------------------------------------------------

#[test]
fn startup_creates_the_hub_directories_and_survives_a_failure() {
    let env = env();
    let sys = env.target("sys");
    settings::update_artifact_dirs(&env.home(), |s| {
        s.artifacts_dir = Some(sys.display().to_string());
    })
    .unwrap();
    ensure_startup_dirs(&env.state);
    assert!(env.data().join("artifacts").is_dir());
    assert!(env.data().join("attachments").is_dir());
    assert!(sys.is_dir());

    // A store path under a plain file cannot get directories; startup logs and carries on.
    let blocker = env.target("blocker");
    write(&blocker, "file");
    let mut cfg = env.state.config.clone();
    cfg.store_path = blocker.join("store.redb");
    ensure_startup_dirs(&HubState::new(cfg));
}

#[test]
fn resolve_path_resolves_symlinks_of_the_nearest_existing_ancestor() {
    let env = env();
    let real = env.target("real");
    fs::create_dir_all(&real).unwrap();
    let link = env.target("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(resolve_path(&link.join("a/b")).unwrap(), real.join("a/b"));
    assert_eq!(resolve_path(&real).unwrap(), real);
}
