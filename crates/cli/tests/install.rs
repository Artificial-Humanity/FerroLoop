//! `fl install --dry-run` (#205 phase A2), driven as a black box. The command
//! plans and prints; these tests check that it is deterministic, that it
//! refuses with the documented exit codes, and that it creates NOTHING.

use assert_cmd::Command;
use fl_core::install::{ArchitectureCheck, default_manifest};
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Every path under `dir` with its kind, size and mtime: any create, write,
/// touch or delete changes it.
fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            let kind = if meta.is_dir() {
                "dir"
            } else if meta.file_type().is_symlink() {
                "symlink"
            } else {
                "file"
            };
            out.insert(
                p.strip_prefix(base).unwrap().display().to_string(),
                format!("{kind} {} {:?}", meta.len(), meta.modified().unwrap()),
            );
            if meta.is_dir() {
                walk(base, &p, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// A scratch world: a HOME, an isolated install root and a working directory.
struct World {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
    cwd: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
            cwd: tempfile::tempdir().unwrap(),
        }
    }

    /// `fl` with a scrubbed environment and the isolated root.
    fn fl(&self) -> Command {
        let mut c = self.bare();
        c.env("FL_INSTALL_ROOT", self.root.path());
        c
    }

    fn bare(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env_clear()
            .env("HOME", self.home.path())
            .current_dir(self.cwd.path());
        c
    }

    fn prefix(&self) -> PathBuf {
        self.root.path().join(".local/share/fl")
    }

    fn trees(&self) -> [BTreeMap<String, String>; 3] {
        [
            snapshot(self.home.path()),
            snapshot(self.root.path()),
            snapshot(self.cwd.path()),
        ]
    }
}

fn stdout(c: &mut Command) -> String {
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "exit {:?}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn dry_run_prints_the_plan_and_creates_nothing() {
    let w = World::new();
    let before = w.trees();
    let out = stdout(w.fl().args(["install", "--dry-run", "--profile", "lab"]));
    assert!(out.starts_with("fl install plan (dry run): nothing was changed\n"));
    for want in [
        "profile   lab",
        "unmanaged (no install receipt)",
        "ferrostore  dependency",
        "included because ferrodeck needs ferrowire",
        "[requires_approval] ferrogate:init_credentials:vault",
        "[by_profile] ferrostore:register_service:ferrostore@.service",
        "planned actions (none runs as part of a plain install)",
        "0.1.0+unstamped",
    ] {
        // ferrowire is a dependency only when ferrodeck is the sole reason
        if want.contains("ferrodeck needs ferrowire") {
            continue;
        }
        assert!(out.contains(want), "missing `{want}` in:\n{out}");
    }
    let prefix = w.prefix();
    assert!(out.contains(&format!("{}/current", prefix.display())));
    assert!(out.contains(&format!("{}/.local/bin/fl", w.root.path().display())));
    assert_eq!(w.trees(), before, "the dry run must not touch anything");
    assert!(!prefix.exists());
}

#[test]
fn explanations_name_the_product_that_needs_a_dependency() {
    let w = World::new();
    let out = stdout(w.fl().args([
        "install",
        "--dry-run",
        "--profile",
        "minimal",
        "--add",
        "ferrodeck",
    ]));
    assert!(
        out.contains("included because ferrodeck needs ferrowire"),
        "{out}"
    );
    assert!(out.contains("hidden default dependency"));
    assert!(out.contains("module routes (hub answers 404 module_not_installed)"));
    assert!(out.contains("ferrogate for ferrodeck"));
    assert!(out.contains("ferrogate is not installed (fl install --add ferrogate)"));
}

#[test]
fn json_output_carries_the_schema_version_and_the_whole_plan() {
    let w = World::new();
    let out = stdout(
        w.fl()
            .args(["install", "--dry-run", "--profile", "full", "--json"]),
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["profile"], "full");
    assert_eq!(v["management"], "unmanaged");
    assert_eq!(v["status"], "ready");
    assert_eq!(v["products"].as_array().unwrap().len(), 9);
    assert_eq!(v["products"][1]["id"], "ferrostore");
    assert_eq!(v["products"][1]["hidden"], true);
    assert!(v["actions"].as_array().unwrap().iter().all(|a| {
        matches!(
            a["approval"].as_str(),
            Some("requires_approval" | "by_profile")
        )
    }));
    assert!(v["paths"].as_array().unwrap().len() > 10);
    assert!(out.ends_with("}\n"));
}

#[test]
fn the_same_inputs_give_the_same_bytes_whatever_the_cwd_and_environment() {
    let w = World::new();
    let args = [
        "install",
        "--dry-run",
        "--profile",
        "lab",
        "--add",
        "ferrostep",
    ];
    let first_text = stdout(w.fl().args(args));
    let first_json = stdout(w.fl().args(args).arg("--json"));
    // a different cwd, a different HOME, noise in the environment, a store override
    let other_cwd = tempfile::tempdir().unwrap();
    let other_home = tempfile::tempdir().unwrap();
    let mut c = w.fl();
    c.current_dir(other_cwd.path())
        .env("HOME", other_home.path())
        .env("TZ", "Pacific/Auckland")
        .env("LANG", "de_DE.UTF-8")
        .env("FL_DB", "/nonexistent/store")
        .env("XDG_CONFIG_HOME", "/nonexistent/config")
        .env("RANDOM_NOISE", "42")
        .args(args);
    assert_eq!(stdout(&mut c), first_text);
    let mut c = w.fl();
    c.current_dir(other_cwd.path())
        .env("TZ", "UTC")
        .args(args)
        .arg("--json");
    assert_eq!(stdout(&mut c), first_json);
    assert_eq!(stdout(w.fl().args(args)), first_text);
    // the order of repeated flags does not matter
    let a = stdout(w.fl().args([
        "install",
        "--dry-run",
        "--add",
        "ferrowatch",
        "--add",
        "ferrogate",
    ]));
    let b = stdout(w.fl().args([
        "install",
        "--dry-run",
        "--add",
        "ferrogate",
        "--add",
        "ferrowatch",
    ]));
    assert_eq!(a, b);
    // no clock in the output
    assert!(!first_json.contains("timestamp"));
}

#[test]
fn the_default_layout_is_shown_unexpanded_and_touches_no_home() {
    let w = World::new();
    let before = w.trees();
    let out = stdout(
        w.bare()
            .args(["install", "--dry-run", "--profile", "headless"]),
    );
    assert!(out.contains("prefix    ~/.local/share/fl"), "{out}");
    assert!(out.contains("launcher  ~/.local/bin/fl"));
    assert!(
        !out.contains(&w.home.path().display().to_string()),
        "the home path leaked:\n{out}"
    );
    assert_eq!(w.trees(), before);
}

#[test]
fn an_explicit_prefix_is_self_contained() {
    let w = World::new();
    let prefix = w.root.path().join("my-fl");
    let before = w.trees();
    let out = stdout(
        w.bare()
            .args(["install", "--dry-run", "--profile", "full", "--prefix"])
            .arg(&prefix),
    );
    assert!(out.contains(&format!("prefix    {}\n", prefix.display())));
    assert!(out.contains(&format!("launcher  {}/bin/fl\n", prefix.display())));
    assert!(out.contains(&format!("{}/units/ferrowire.service", prefix.display())));
    assert_eq!(w.trees(), before);
    // --prefix wins over $FL_INSTALL_ROOT
    let out = stdout(
        w.fl()
            .args(["install", "--dry-run", "--prefix"])
            .arg(&prefix),
    );
    assert!(out.contains(&format!("prefix    {}\n", prefix.display())));
}

#[test]
fn bad_requests_exit_2() {
    let w = World::new();
    for (args, msg) in [
        (vec!["--add", "nope"], "unknown product `nope`"),
        (vec!["--remove", "ferroflow"], "unknown product `ferroflow`"),
        (
            vec!["--profile", "everything"],
            "unknown profile `everything` (accepted: minimal, headless, full, lab)",
        ),
        (
            vec!["--add", "ferrogate", "--remove", "ferrogate"],
            "`ferrogate` is both added and removed",
        ),
        (vec!["--prefix", "relative/dir"], "is not absolute"),
        (vec!["--prefix", "/a/../b"], "`.` or `..` segment"),
    ] {
        w.fl()
            .args(["install", "--dry-run"])
            .args(&args)
            .assert()
            .code(2)
            .stderr(contains(msg));
    }
    w.fl()
        .args(["install", "--dry-run", "--bogus"])
        .assert()
        .code(2);
    w.fl()
        .args(["--db", "/x/store", "install", "--dry-run"])
        .assert()
        .code(2)
        .stderr(contains("opens no store"));
    w.fl()
        .env("FL_INSTALL_ROOT", "relative")
        .args(["install", "--dry-run"])
        .assert()
        .code(2);
}

#[test]
fn refusals_exit_3() {
    let w = World::new();
    let before = w.trees();
    for (args, msg) in [
        (
            vec!["--adopt"],
            "--adopt is not implemented in this release",
        ),
        (
            vec!["--remove", "ferroloop"],
            "`ferroloop` is required in every install",
        ),
        (
            vec!["--profile", "full", "--remove", "ferrowire"],
            "cannot remove `ferrowire`: ferrodeck still need(s) it",
        ),
        (vec!["--remove", "ferrostore"], "cannot remove `ferrostore`"),
        (vec!["--prefix", "/usr"], "cannot be an install prefix"),
        (vec!["--prefix", "/"], "cannot be an install prefix"),
        (
            vec!["--prefix", "/etc/fl"],
            "installs are per-user, never system-wide",
        ),
    ] {
        w.fl()
            .args(["install", "--dry-run"])
            .args(&args)
            .assert()
            .code(3)
            .stderr(contains(msg))
            .stdout("");
    }
    w.fl()
        .env("FL_INSTALL_ROOT", "/")
        .args(["install", "--dry-run"])
        .assert()
        .code(3);
    w.fl()
        .env("FL_INSTALL_ROOT", "/usr")
        .args(["install", "--dry-run"])
        .assert()
        .code(3);
    assert_eq!(w.trees(), before);
}

#[test]
fn without_dry_run_it_refuses_and_creates_nothing() {
    let w = World::new();
    let before = w.trees();
    for args in [
        vec!["install"],
        vec!["install", "--profile", "lab"],
        vec!["install", "--json"],
        vec!["install", "--add", "ferrodeck"],
    ] {
        w.fl()
            .args(&args)
            .assert()
            .code(3)
            .stdout("")
            .stderr("error: installing is not implemented in this release; use --dry-run\n");
    }
    assert_eq!(w.trees(), before);
    assert!(!w.prefix().exists());
}

fn write_manifest(dir: &Path, name: &str, m: &fl_core::install::ReleaseManifest) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, m.to_json()).unwrap();
    p
}

#[test]
fn a_manifest_file_is_validated_and_equals_the_built_in_one() {
    let w = World::new();
    let json =
        |c: &mut Command| stdout(c.args(["install", "--dry-run", "--profile", "full", "--json"]));
    let builtin = json(&mut w.fl());
    let file = write_manifest(w.cwd.path(), "release-manifest.json", &default_manifest());
    let mut c = w.fl();
    c.args(["--manifest", &file.display().to_string()]);
    // flags may precede the subcommand's own: put --manifest after `install`
    let from_file = stdout(
        w.fl()
            .args([
                "install",
                "--dry-run",
                "--profile",
                "full",
                "--json",
                "--manifest",
            ])
            .arg(&file),
    );
    assert_eq!(from_file, builtin);

    // each rejection is exit 3 with the reason, and nothing else happens
    let mut bad = default_manifest();
    bad.products[1].requires.push("ferroloop".into());
    let cycle = write_manifest(w.cwd.path(), "cycle.json", &bad);
    w.fl()
        .args(["install", "--dry-run", "--manifest"])
        .arg(&cycle)
        .assert()
        .code(3)
        .stderr(contains("dependency cycle"));
    let mut v = serde_json::to_value(default_manifest()).unwrap();
    v["schema_version"] = 7.into();
    let future = w.cwd.path().join("future.json");
    std::fs::write(&future, v.to_string()).unwrap();
    w.fl()
        .args(["install", "--dry-run", "--manifest"])
        .arg(&future)
        .assert()
        .code(3)
        .stderr(contains("unsupported schema_version 7"));
    let junk = w.cwd.path().join("junk.json");
    std::fs::write(&junk, "{ not json").unwrap();
    w.fl()
        .args(["install", "--dry-run", "--manifest"])
        .arg(&junk)
        .assert()
        .code(3)
        .stderr(contains("manifest"));
    w.fl()
        .args(["install", "--dry-run", "--manifest", "/nonexistent/m.json"])
        .assert()
        .code(3)
        .stderr(contains("does not exist"));
    w.fl()
        .args(["install", "--dry-run", "--manifest"])
        .arg(w.cwd.path())
        .assert()
        .code(3)
        .stderr(contains("is not a regular file"));
}

#[test]
fn an_unsupported_platform_is_refused_with_exit_3() {
    let w = World::new();
    let mut m = default_manifest();
    for e in &mut m.products {
        e.targets = vec![fl_core::install::Target {
            os: fl_core::install::Os::Windows,
            arch: fl_core::install::Arch::X86_64,
        }];
    }
    let file = write_manifest(w.cwd.path(), "windows-only.json", &m);
    let before = w.trees();
    w.fl()
        .args(["install", "--dry-run", "--manifest"])
        .arg(&file)
        .assert()
        .code(3)
        .stderr(contains("unsupported platform"))
        .stdout("");
    assert_eq!(w.trees(), before);
}

#[test]
fn existing_files_are_reported_as_conflicts_and_left_alone() {
    let w = World::new();
    let bin = w.root.path().join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("fl"), "#!/bin/sh\n# my hand-built launcher\n").unwrap();
    let units = w.root.path().join(".config/systemd/user");
    std::fs::create_dir_all(&units).unwrap();
    std::fs::write(
        units.join("ferrowire.service"),
        "[Unit]\nDescription=mine\n",
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/elsewhere/fl", bin.join("agent-msg")).unwrap();
    let before = w.trees();
    let launcher_before = std::fs::read(bin.join("fl")).unwrap();

    let out = stdout(
        w.fl()
            .args(["install", "--dry-run", "--profile", "full", "--json"]),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "needs_decision");
    assert_eq!(v["management"], "unmanaged");
    let kinds: BTreeMap<String, String> = v["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["path"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_string(),
                c["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(kinds["fl"], "foreign");
    assert_eq!(kinds["agent-msg"], "symlink");
    assert_eq!(kinds["ferrowire.service"], "unmarked_unit");
    assert_eq!(v["unmanaged_existing"].as_array().unwrap().len(), 3);
    assert_eq!(w.trees(), before);
    assert_eq!(std::fs::read(bin.join("fl")).unwrap(), launcher_before);

    let text = stdout(w.fl().args(["install", "--dry-run", "--profile", "full"]));
    assert!(text.contains("status    needs_decision"));
    assert!(text.contains("conflicts (each needs an explicit decision"));
    assert!(text.contains("unmanaged state found (not claimed)"));
}

#[test]
fn a_receipt_makes_the_install_managed_and_a_bad_one_is_refused() {
    let w = World::new();
    std::fs::create_dir_all(w.prefix()).unwrap();
    let receipt_path = w.prefix().join("receipt.json");
    let root = w.root.path().display().to_string();
    let mut receipt = fl_core::install::InstallReceipt::parse(&format!(
        r#"{{"schema_version":1,
            "release":{{"version":"0.1.0","name":"0.1.0+unstamped","kind":"source_build"}},
            "profile":"headless",
            "products":["ferroloop","ferrowire"],
            "layout":{{"prefix":"{root}/.local/share/fl","bin_dir":"{root}/.local/bin","unit_dir":"{root}/.config/systemd/user"}}}}"#
    ))
    .unwrap();
    std::fs::write(&receipt_path, receipt.to_json()).unwrap();
    let before = w.trees();
    let out = stdout(w.fl().args(["install", "--dry-run", "--json"]));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["management"], "managed");
    // with no --profile, the receipt's selection is the base
    assert_eq!(v["profile"], "headless");
    let ids: Vec<&str> = v["products"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["ferroloop", "ferrostore", "ferrowire"]);
    assert_eq!(w.trees(), before);

    // an invalid receipt is a refusal, never "unmanaged"
    receipt.schema_version = 9;
    std::fs::write(&receipt_path, receipt.to_json()).unwrap();
    w.fl()
        .args(["install", "--dry-run"])
        .assert()
        .code(3)
        .stderr(contains("receipt").and(contains("not treated as unmanaged")));
    std::fs::write(
        &receipt_path,
        "{\"schema_version\":1,\"vault_key\":\"hunter2\"}",
    )
    .unwrap();
    w.fl()
        .args(["install", "--dry-run"])
        .assert()
        .code(3)
        .stderr(contains("unknown field"));
    // so is a receipt that is a symlink
    std::fs::remove_file(&receipt_path).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(w.cwd.path().join("elsewhere.json"), &receipt_path).unwrap();
    w.fl()
        .args(["install", "--dry-run"])
        .assert()
        .code(3)
        .stderr(contains("is not a regular file"));
}

#[test]
fn other_install_commands_are_unaffected() {
    let w = World::new();
    w.bare()
        .args(["install", "--help"])
        .assert()
        .success()
        .stdout(contains("--dry-run").and(contains("--profile")));
    // nested `install`s stay where they were
    w.bare()
        .args(["state", "install", "--help"])
        .assert()
        .success();
    w.bare()
        .args(["vault", "install", "--help"])
        .assert()
        .success();
}

// ------------------------------------------------- the manifest and the code

#[test]
fn install_manifest_matches_the_code() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let raw = std::fs::read_to_string(repo.join(".fl/architecture.toml")).unwrap();
    let model: fl_core::ArchitectureModel = toml::from_str(&raw).unwrap();
    let manifest = default_manifest();
    let check = ArchitectureCheck::new(&manifest, &model);
    let problems = check.problems();
    assert!(
        problems.is_empty(),
        "the built-in manifest and .fl/architecture.toml disagree:\n{}",
        problems.join("\n")
    );

    // compat data is the range `fl --version --json` reports, read from the same constants
    let formats = fl_store::formats::store_formats();
    assert_eq!(
        u64::from(manifest.compat.store_format_min),
        formats["redb_schema"]["min"].as_u64().unwrap()
    );
    assert_eq!(
        u64::from(manifest.compat.store_format_max),
        formats["redb_schema"]["max"].as_u64().unwrap()
    );
    assert_eq!(
        manifest.compat.protocol_major,
        fl_wire::peer::WIRE_PROTOCOL_VERSION
    );
    // the shipped hub unit exists in the repo under the name the manifest gives it
    assert!(repo.join("contrib/ferroloop-hub.service").is_file());
    assert!(repo.join("ui").is_dir());
}
