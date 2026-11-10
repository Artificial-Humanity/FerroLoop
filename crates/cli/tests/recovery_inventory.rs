//! Metadata-only inventory, driven through the real CLI with an isolated home.
use assert_cmd::Command;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

struct World {
    home: tempfile::TempDir,
    stores: PathBuf,
}
impl World {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let stores = home.path().join(".local/share/fl");
        std::fs::create_dir_all(&stores).unwrap();
        std::fs::create_dir_all(home.path().join(".config/fl")).unwrap();
        Self { home, stores }
    }
    fn fl(&self) -> Command {
        let mut cmd = Command::cargo_bin("fl").unwrap();
        cmd.env_clear()
            .env_remove("AGENT_PERSONA")
            .env_remove("AGENT_NAME")
            .env_remove("FL_HOSTED_ID")
            .env_remove("AGENT_HYDRATION_GENERATION")
            .env("HOME", self.home.path())
            .current_dir(self.home.path())
            .timeout(Duration::from_secs(10));
        cmd
    }
    fn config(&self) {
        std::fs::write(self.home.path().join(".config/fl/config.toml"), format!(
            "[state]\nstore = {:?}\n[[project]]\nname = \"example\"\nroot = {:?}\nstore = {:?}\n",
            self.stores.join("fl.redb"), self.home.path().join("Example"), self.stores.join("different.redb")
        )).unwrap();
    }
    fn inventory(&self) -> Value {
        let output = self
            .fl()
            .args(["recovery", "inventory", "--root"])
            .arg(&self.stores)
            .arg("--json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&output).unwrap()
    }
}
fn has_path(map: &Value, path: &Path) -> bool {
    map["stores"].as_array().unwrap().iter().any(|s| {
        s["identity"]["given_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str() == path.to_str())
    })
}
#[test]
fn inventory_uses_config_and_hub_edges_without_opening_or_creating_stores() {
    let world = World::new();
    world.config();
    let primary = world.stores.join("fl.fls");
    std::fs::write(&primary, b"not a store: opening this must fail").unwrap();
    std::fs::write(world.stores.join("example.fls"), b"not a store either").unwrap();
    let before = std::fs::metadata(&primary).unwrap().modified().unwrap();
    let map = world.inventory();
    assert_eq!(map["schema_version"], 1);
    assert_eq!(map["complete"], false);
    assert!(has_path(&map, &primary));
    assert!(has_path(&map, &world.stores.join("different.fls")));
    assert!(!world.stores.join("different.fls").exists());
    let edges = map["bindings"].as_array().unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e["caller"] == "config" && e["scope"] == "project:example")
    );
    assert!(
        edges.iter().any(|e| e["caller"] == "hub"
            && e["scope"] == "project:example"
            && e["mismatch"] == true)
    );
    assert_eq!(
        std::fs::metadata(&primary).unwrap().modified().unwrap(),
        before
    );
    let mut again = world.inventory();
    let mut normalized = map;
    again
        .as_object_mut()
        .unwrap()
        .remove("collected_at_unix_ms");
    normalized
        .as_object_mut()
        .unwrap()
        .remove("collected_at_unix_ms");
    assert_eq!(again, normalized);
}
#[test]
fn inherited_database_override_outside_scan_scope_is_refused() {
    let world = World::new();
    world
        .fl()
        .env("FL_DB", "/outside-fixture/live.fls")
        .args(["recovery", "inventory", "--root"])
        .arg(&world.stores)
        .arg("--json")
        .assert()
        .failure();
    assert!(!world.stores.join("fl.fls").exists());
}
#[cfg(target_os = "linux")]
#[test]
fn inventory_never_reads_store_contents_or_connects_to_its_socket() {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::net::UnixListener;
    let world = World::new();
    let path = world.stores.join("fl.fls");
    std::fs::write(&path, b"invalid store").unwrap();
    for name in ["example.fls", "unbound.fls", "fl.code.fls"] {
        std::fs::write(world.stores.join(name), b"invalid store").unwrap();
    }
    // A directory-fd path keeps the socket address below SUN_LEN even when
    // the caller selects a long TMPDIR; the socket still lives beside the store.
    let directory = std::fs::File::open(&world.stores).unwrap();
    let socket = format!("/proc/self/fd/{}/fl.fls.sock", directory.as_raw_fd());
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    // IN_OPEN catches reads even if a create-capable opener suppresses its errors.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    assert!(fd >= 0);
    let fd = unsafe { std::fs::File::from_raw_fd(fd) };
    for entry in std::fs::read_dir(&world.stores).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "fls") {
            let watched = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            assert!(
                unsafe {
                    libc::inotify_add_watch(
                        fd.as_raw_fd(),
                        watched.as_ptr(),
                        libc::IN_OPEN | libc::IN_MODIFY,
                    )
                } >= 0
            );
        }
    }
    // Give the child a short address to the same fixture, so an accidental
    // connection would reach the listener rather than fail at SUN_LEN first.
    let home_directory = std::fs::File::open(world.home.path()).unwrap();
    let short_home = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        home_directory.as_raw_fd()
    ));
    world
        .fl()
        .env("HOME", &short_home)
        .args(["recovery", "inventory", "--root"])
        .arg(short_home.join(".local/share/fl"))
        .arg("--json")
        .assert()
        .success();
    let mut bytes = [0u8; 4096];
    let count = unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
    assert_eq!(count, -1, "store contents were opened or modified");
    assert_eq!(
        std::io::Error::last_os_error().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}

#[cfg(unix)]
#[test]
fn config_and_deployment_artifacts_cannot_alias_a_store() {
    let world = World::new();
    let source = world.stores.join("fl.fls");
    std::fs::write(&source, b"secret data that is not metadata").unwrap();
    let config = world.home.path().join(".config/fl/config.toml");
    std::fs::hard_link(&source, &config).unwrap();
    world
        .fl()
        .args(["recovery", "inventory", "--root"])
        .arg(&world.stores)
        .arg("--json")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "unsafe or oversized metadata artifact",
        ));
    std::fs::remove_file(&config).unwrap();
    let deployment = world.home.path().join("deployment.json");
    std::os::unix::fs::symlink(&source, &deployment).unwrap();
    world
        .fl()
        .args(["recovery", "inventory", "--root"])
        .arg(&world.stores)
        .arg("--deployment")
        .arg(&deployment)
        .arg("--json")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "unsafe or oversized metadata artifact",
        ));
    assert_eq!(
        std::fs::read(&source).unwrap(),
        b"secret data that is not metadata"
    );
}

#[test]
fn explicit_database_override_is_a_separate_provenance_edge() {
    let world = World::new();
    let explicit = world.stores.join("chosen.fls");
    let output = world
        .fl()
        .env("FL_DB", world.stores.join("shadowed.fls"))
        .arg("--db")
        .arg(&explicit)
        .args(["recovery", "inventory", "--root"])
        .arg(&world.stores)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let map: Value = serde_json::from_slice(&output).unwrap();
    let edge = map["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["caller"] == "CLI override")
        .unwrap();
    assert_eq!(edge["source"], "--db");
    assert_eq!(edge["physical_path"], explicit.to_str().unwrap());
    assert!(!explicit.exists());
}

#[cfg(target_os = "linux")]
fn reverse_alias_is_refused_without_open(deployment: bool) {
    use std::os::fd::{AsRawFd, FromRawFd};
    let world = World::new();
    let artifact = world.home.path().join(if deployment {
        "deployment.json"
    } else {
        ".config/fl/config.toml"
    });
    std::fs::write(&artifact, if deployment { "{}" } else { "# valid config" }).unwrap();
    std::os::unix::fs::symlink(&artifact, world.stores.join("unbound.fls")).unwrap();
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    assert!(fd >= 0);
    let fd = unsafe { std::fs::File::from_raw_fd(fd) };
    let watched = std::ffi::CString::new(artifact.to_str().unwrap()).unwrap();
    assert!(
        unsafe {
            libc::inotify_add_watch(
                fd.as_raw_fd(),
                watched.as_ptr(),
                libc::IN_OPEN | libc::IN_ACCESS | libc::IN_MODIFY,
            )
        } >= 0
    );
    let mut command = world.fl();
    command
        .args(["recovery", "inventory", "--root"])
        .arg(&world.stores)
        .arg("--json");
    if deployment {
        command.arg("--deployment").arg(&artifact);
    }
    let output = command.output().unwrap();
    let mut bytes = [0u8; 4096];
    let count = unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
    assert_eq!(count, -1, "aliased store was opened or read");
    assert_eq!(
        std::io::Error::last_os_error().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(
        !output.status.success(),
        "reverse store alias must be refused"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unsafe or oversized metadata artifact")
    );
    assert!(!world.stores.join("fl.fls").exists());
}
#[cfg(target_os = "linux")]
#[test]
fn reverse_symlink_config_candidate_is_never_opened() {
    reverse_alias_is_refused_without_open(false);
}
#[cfg(target_os = "linux")]
#[test]
fn reverse_symlink_unbound_deployment_candidate_is_never_opened() {
    reverse_alias_is_refused_without_open(true);
}

fn config_root_entries_are_preserved(roots: &[&str]) {
    let world = World::new();
    let mut config = String::new();
    let mut stores = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let store = world
            .home
            .path()
            .join(format!("outside-{index}/bound.redb"));
        config.push_str(&format!(
            "[[project]]\nroot = {root:?}\nstore = {store:?}\n"
        ));
        stores.push(store.with_extension("fls"));
    }
    std::fs::write(world.home.path().join(".config/fl/config.toml"), config).unwrap();
    let map = world.inventory();
    for (root, store) in roots.iter().zip(stores) {
        assert!(has_path(&map, &store), "missing binding for {root}");
        assert!(
            map["bindings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["caller"] == "CLI root"
                    && b["project_root"] == *root
                    && b["physical_path"] == store.to_str().unwrap())
        );
        assert!(
            map["required_additional_roots"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == store.parent().unwrap().to_str().unwrap())
        );
        assert!(!store.exists());
    }
    assert_eq!(map["complete"], false);
}
#[test]
fn unnamed_non_slug_config_root_is_preserved() {
    config_root_entries_are_preserved(&["/work/My.Project"]);
}
#[test]
fn distinct_config_roots_with_same_basename_are_preserved() {
    config_root_entries_are_preserved(&["/work/one/Shared", "/work/two/Shared"]);
}

#[test]
fn mismatched_resolvers_attach_only_their_applicable_consumers() {
    let world = World::new();
    world.config();
    std::fs::write(world.stores.join("example.fls"), b"fixture").unwrap();
    let map = world.inventory();
    let callers = |name: &str| {
        let path = world.stores.join(name);
        let store = map["stores"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| {
                s["identity"]["given_paths"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p == path.to_str().unwrap())
            })
            .unwrap();
        let mut result: Vec<_> = store["consumers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["caller"].as_str().unwrap())
            .collect();
        result.sort();
        result.dedup();
        result
    };
    assert_eq!(
        callers("different.fls"),
        vec!["CLI", "MCP", "MCP cross-store", "store maintenance"]
    );
    assert_eq!(callers("example.fls"), vec!["hub", "store maintenance"]);
    assert_eq!(callers("example.code.fls"), vec!["code index"]);
}

#[test]
fn hub_discovery_is_bounded_and_reports_omitted_bindings() {
    let world = World::new();
    for index in 0..4097 {
        std::fs::write(world.stores.join(format!("ignored-{index}.txt")), b"").unwrap();
    }
    std::fs::write(world.stores.join("discovered.fls"), b"fixture").unwrap();
    let map = world.inventory();
    assert!(
        map["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("hub discovery limit"))
    );
    assert!(
        !map["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["caller"] == "hub" && b["scope"] != "workspace"),
        "a truncated directory must not choose an enumeration-dependent subset"
    );
    assert!(
        has_path(&map, &world.stores.join("discovered.fls")),
        "metadata scan still accounts for unbound candidates"
    );
    assert_eq!(map["complete"], false);
}

#[cfg(unix)]
#[test]
fn incomplete_scan_without_artifact_reads_returns_partial_map() {
    let world = World::new();
    let hidden = world.stores.join("hidden");
    std::fs::create_dir(&hidden).unwrap();
    std::os::unix::fs::symlink(&hidden, world.stores.join("linked-directory")).unwrap();
    let map = world.inventory();
    assert_eq!(map["complete"], false);
    assert!(
        map["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("symlinked directory"))
    );
}

#[test]
fn matching_config_and_hub_keep_both_edges_including_workspace() {
    let world = World::new();
    std::fs::write(
        world.home.path().join(".config/fl/config.toml"),
        format!(
            "[state]\nstore = {:?}\n[[project]]\nname = \"example\"\nroot = {:?}\nstore = {:?}\n",
            world.stores.join("fl.redb"),
            world.home.path().join("Example"),
            world.stores.join("example.redb")
        ),
    )
    .unwrap();
    std::fs::write(world.stores.join("example.fls"), b"fixture").unwrap();
    let map = world.inventory();
    for scope in ["workspace", "project:example"] {
        for caller in ["config", "hub"] {
            let edge = map["bindings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["caller"] == caller && e["scope"] == scope)
                .unwrap();
            assert_eq!(edge["mismatch"], false);
        }
    }
}

#[test]
fn hub_workspace_and_project_fallback_follow_cwd_bound_store() {
    let world = World::new();
    let selected = world.stores.join("bound.redb");
    std::fs::write(
        world.home.path().join(".config/fl/config.toml"),
        format!(
            "[[project]]\nname = \"example\"\nroot = {:?}\nstore = {:?}\n",
            world.home.path(),
            selected
        ),
    )
    .unwrap();
    let map = world.inventory();
    for scope in ["workspace", "project:example"] {
        let edge = map["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["caller"] == "hub" && e["scope"] == scope)
            .unwrap();
        assert_eq!(
            edge["effective_path"],
            selected.with_extension("fls").to_str().unwrap()
        );
        assert_eq!(edge["mismatch"], scope == "workspace");
    }
}

#[test]
fn invalid_backend_is_refused_by_inventory() {
    let world = World::new();
    world
        .fl()
        .env("FL_STORE_BACKEND", "sqlite")
        .args(["recovery", "inventory", "--root"])
        .arg(&world.stores)
        .arg("--json")
        .assert()
        .failure();
    assert!(!world.stores.join("fl.fls").exists());
}

#[test]
fn hub_workspace_edge_tracks_explicit_override_and_default() {
    let world = World::new();
    let map = world.inventory();
    let hub = map["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["caller"] == "hub" && e["scope"] == "workspace")
        .unwrap();
    assert_eq!(
        hub["physical_path"],
        world.stores.join("fl.fls").to_str().unwrap()
    );
    for flag in [false, true] {
        let chosen = world.stores.join(if flag { "flag.fls" } else { "env.fls" });
        let mut cmd = world.fl();
        cmd.env("FL_DB", world.stores.join("env.fls"));
        if flag {
            cmd.arg("--db").arg(&chosen);
        }
        let output = cmd
            .args(["recovery", "inventory", "--root"])
            .arg(&world.stores)
            .arg("--json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let map: Value = serde_json::from_slice(&output).unwrap();
        let hub = map["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["caller"] == "hub" && e["scope"] == "workspace")
            .unwrap();
        assert_eq!(hub["effective_path"], chosen.to_str().unwrap());
        assert_eq!(hub["mismatch"], true);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn incomplete_scan_refuses_hidden_reverse_alias_before_artifact_open() {
    use std::os::fd::{AsRawFd, FromRawFd};
    for deployment in [false, true] {
        let world = World::new();
        let artifact = world.home.path().join(if deployment {
            "deployment.json"
        } else {
            ".config/fl/config.toml"
        });
        std::fs::write(&artifact, if deployment { "{}" } else { "# fixture" }).unwrap();
        let hidden = world.home.path().join("hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::os::unix::fs::symlink(&artifact, hidden.join("reverse.fls")).unwrap();
        std::os::unix::fs::symlink(&hidden, world.stores.join("hidden-directory")).unwrap();
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0);
        let fd = unsafe { std::fs::File::from_raw_fd(fd) };
        let watched = std::ffi::CString::new(artifact.to_str().unwrap()).unwrap();
        assert!(
            unsafe {
                libc::inotify_add_watch(
                    fd.as_raw_fd(),
                    watched.as_ptr(),
                    libc::IN_OPEN | libc::IN_ACCESS,
                )
            } >= 0
        );
        let mut cmd = world.fl();
        cmd.args(["recovery", "inventory", "--root"])
            .arg(&world.stores)
            .arg("--json");
        if deployment {
            cmd.arg("--deployment").arg(&artifact);
        }
        cmd.assert()
            .failure()
            .stderr(predicates::str::contains("preflight incomplete"));
        let mut bytes = [0u8; 4096];
        assert_eq!(
            unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
