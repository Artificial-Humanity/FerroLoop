//! `fl doctor` (#205 phase A3), driven as a black box. It only observes: these tests check the
//! report (check IDs, statuses, the JSON contract), the exit codes (0 no failure, 1 a
//! failure, 3 a refused repair) and, above all, that NOTHING was written, anywhere.

use assert_cmd::Command;
use fl_core::at::At;
use fl_core::doctor::ids::{VENDOR_UPDATE_POLICY, VENDOR_VERSION_DRIFT};
use fl_core::install::{
    InstallReceipt, OwnedAlias, OwnedPath, OwnedPathKind, OwnedUnit, Product, Profile,
    RECEIPT_SCHEMA_VERSION, ReceiptLayout, ReceiptRelease, ReleaseKind,
};
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StatePayload, StateScope, UpsertStateArgs,
};
use fl_core::store::StateStore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const IDS: [&str; 7] = [
    "G7.release-identity",
    "G7.receipt-integrity",
    "G7.path-ownership",
    "G7.store-health",
    "G7.vault-perms",
    "G7.code-ui-agree",
    "G7.unit-order",
];

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Every path under `dir`: kind, mode, size, mtime and content hash, so any create, write,
/// chmod, touch or delete changes it.
fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            let extra = if meta.file_type().is_symlink() {
                std::fs::read_link(&p).unwrap().display().to_string()
            } else if meta.is_file() {
                sha(&std::fs::read(&p).unwrap())
            } else {
                String::new()
            };
            out.insert(
                p.strip_prefix(base).unwrap().display().to_string(),
                format!(
                    "{:?} {:o} {} {:?} {extra}",
                    meta.file_type(),
                    meta.permissions().mode(),
                    meta.len(),
                    meta.modified().unwrap()
                ),
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

/// A scratch world: a HOME (stores, vault, config), an isolated install root and a cwd.
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

    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env_clear()
            .env("HOME", self.home.path())
            .env("FL_INSTALL_ROOT", self.root.path())
            .env("PATH", self.home.path().join(".local/bin"))
            .env(
                "DOCTOR_PROBE_SENTINEL",
                self.home.path().join("vendor-executed"),
            )
            .current_dir(self.cwd.path());
        c
    }

    fn prefix(&self) -> PathBuf {
        self.root.path().join(".local/share/fl")
    }

    fn release_dir(&self) -> PathBuf {
        self.prefix().join("releases/0.1.0+0123456789ab")
    }

    fn trees(&self) -> [BTreeMap<String, String>; 3] {
        [
            snapshot(self.home.path()),
            snapshot(self.root.path()),
            snapshot(self.cwd.path()),
        ]
    }

    /// Run `fl doctor --json` plus `args`: the exit code and the parsed report.
    fn doctor(&self, args: &[&str]) -> (i32, Value) {
        self.doctor_with(self.fl(), args)
    }

    fn doctor_with(&self, mut command: Command, args: &[&str]) -> (i32, Value) {
        let before = self.trees();
        let out = command
            .arg("doctor")
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(self.trees(), before, "`fl doctor` wrote something");
        let code = out.status.code().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
        (code, v)
    }

    fn fake_vendor(&self, relative: &str) -> PathBuf {
        let path = self.home.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            b"#!/bin/sh\nprintf executed > \"$DOCTOR_PROBE_SENTINEL\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    /// A real install under the isolated root, and the receipt that owns it.
    fn install(&self) {
        self.install_with_commit(None);
    }

    /// [`Self::install`], the receipt naming `commit`.
    fn install_with_commit(&self, commit: Option<&str>) {
        let root = self.root.path();
        let rel = self.release_dir();
        std::fs::create_dir_all(rel.join("bin")).unwrap();
        std::fs::create_dir_all(root.join(".local/bin")).unwrap();
        std::fs::create_dir_all(root.join(".config/systemd/user")).unwrap();
        let bin = b"fl binary bytes";
        std::fs::write(rel.join("bin/fl"), bin).unwrap();
        std::os::unix::fs::symlink("releases/0.1.0+0123456789ab", self.prefix().join("current"))
            .unwrap();
        std::os::unix::fs::symlink(
            self.prefix().join("current/bin/fl"),
            root.join(".local/bin/fl"),
        )
        .unwrap();
        std::os::unix::fs::symlink("fl", root.join(".local/bin/agent-msg")).unwrap();
        let unit = b"# generated by fl install 0.1.0+0123456789ab\n[Service]\nExecStart=fl\n";
        let unit_path = root.join(".config/systemd/user/ferrowire.service");
        std::fs::write(&unit_path, unit).unwrap();
        let s = |p: &Path| p.display().to_string();
        let receipt = InstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            release: ReceiptRelease {
                version: env!("CARGO_PKG_VERSION").into(),
                name: format!("{}+0123456789ab", env!("CARGO_PKG_VERSION")),
                kind: ReleaseKind::SourceBuild,
                commit: commit.map(str::to_string),
                build: None,
            },
            profile: Profile::Full,
            products: vec![Product::Ferroloop],
            layout: ReceiptLayout {
                prefix: s(&self.prefix()),
                bin_dir: s(&root.join(".local/bin")),
                unit_dir: s(&root.join(".config/systemd/user")),
            },
            artifacts: vec![],
            paths: vec![
                OwnedPath {
                    path: s(&rel),
                    kind: OwnedPathKind::Dir,
                    sha256: None,
                    link_target: None,
                },
                OwnedPath {
                    path: s(&rel.join("bin/fl")),
                    kind: OwnedPathKind::File,
                    sha256: Some(sha(bin)),
                    link_target: None,
                },
                OwnedPath {
                    path: s(&self.prefix().join("current")),
                    kind: OwnedPathKind::Symlink,
                    sha256: None,
                    link_target: Some("releases/0.1.0+0123456789ab".into()),
                },
                OwnedPath {
                    path: s(&root.join(".local/bin/fl")),
                    kind: OwnedPathKind::Symlink,
                    sha256: None,
                    link_target: Some(s(&self.prefix().join("current/bin/fl"))),
                },
            ],
            aliases: vec![OwnedAlias {
                name: "agent-msg".into(),
                path: s(&root.join(".local/bin/agent-msg")),
                target: "fl".into(),
            }],
            units: vec![OwnedUnit {
                name: "ferrowire.service".into(),
                path: s(&unit_path),
                sha256: sha(unit),
                release: "0.1.0+0123456789ab".into(),
            }],
            catalog_entries: vec![],
            config_blocks: vec![],
            journal: vec![],
        };
        std::fs::write(self.prefix().join("receipt.json"), receipt.to_json()).unwrap();
    }

    fn receipt_path(&self) -> PathBuf {
        self.prefix().join("receipt.json")
    }

    /// A healthy FerroStore at the default store location.
    fn store(&self) {
        let dir = self.home.path().join(".local/share/fl");
        std::fs::create_dir_all(&dir).unwrap();
        let s = fl_store::FerroStore::open(dir.join("fl.fls")).unwrap();
        s.put_doc("c", "a", &json!(1)).unwrap();
    }

    /// Vault and key files at the default locations, with these modes.
    fn vault(&self, vault_mode: u32, key_mode: u32) {
        let v = self.home.path().join(".local/share/fl/vault.enc");
        let k = self.home.path().join(".config/fl/vault.key");
        for (p, mode) in [(&v, vault_mode), (&k, key_mode)] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "SENTINEL-SECRET-CONTENT").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        for d in [v.parent().unwrap(), k.parent().unwrap()] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    /// A UI bundle in the installed release.
    fn ui(&self, info: Value) {
        let dist = self.release_dir().join("ui/dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("build-info.json"), info.to_string()).unwrap();
    }
}

fn status<'a>(v: &'a Value, id: &str) -> &'a str {
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no {id}"))["status"]
        .as_str()
        .unwrap()
}

fn detail<'a>(v: &'a Value, id: &str) -> &'a str {
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()["detail"]
        .as_str()
        .unwrap()
}

/// What this binary says about itself: (the full commit, whether it is clean).
fn this_build() -> (String, bool) {
    let out = Command::cargo_bin("fl")
        .unwrap()
        .args(["--version", "--json"])
        .env_clear()
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    (
        v["commit"].as_str().unwrap().to_string(),
        v["dirty"] == json!(false),
    )
}

#[test]
fn an_empty_home_is_unmanaged_not_healthy_and_nothing_is_written() {
    let w = World::new();
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 0, "nothing installed is not a failure: {v}");
    let ids: Vec<&str> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            IDS.as_slice(),
            &[VENDOR_VERSION_DRIFT, VENDOR_UPDATE_POLICY]
        ]
        .concat()
    );
    for id in &IDS[1..] {
        assert_eq!(status(&v, id), "not-installed", "{id}: {}", detail(&v, id));
    }
    assert_eq!(
        detail(&v, "G7.receipt-integrity"),
        "unmanaged (no install receipt)"
    );
    assert_ne!(v["overall"], "pass");
    assert_eq!(v["schema_version"], 1);
    // The empty world is still empty.
    for t in w.trees() {
        assert!(t.is_empty(), "{t:?}");
    }
}

#[test]
fn the_json_has_the_contract_ferrodeck_reads_and_is_deterministic() {
    let w = World::new();
    w.install();
    let a = w.fl().args(["doctor", "--json"]).output().unwrap();
    let b = w.fl().args(["doctor", "--json"]).output().unwrap();
    assert_eq!(a.stdout, b.stdout, "two runs differ");
    let v: Value = serde_json::from_slice(&a.stdout).unwrap();
    let mut top: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    top.sort_unstable();
    assert_eq!(top, ["checks", "overall", "schema_version", "summary"]);
    for c in v["checks"].as_array().unwrap() {
        let mut keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["detail", "id", "status", "title"]);
    }
    let text = String::from_utf8(a.stdout).unwrap();
    assert!(
        !text.contains("generated_at") && !text.contains("timestamp"),
        "{text}"
    );
}

#[test]
fn a_healthy_install_reports_pass_where_it_can_prove_it() {
    let w = World::new();
    let (commit, clean) = this_build();
    w.install_with_commit((commit != "unknown").then_some(commit.as_str()));
    w.store();
    w.vault(0o600, 0o600);
    w.ui(json!({"commit": commit, "dirty": false, "version": "0.1.0"}));
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        status(&v, "G7.receipt-integrity"),
        "pass",
        "{}",
        detail(&v, "G7.receipt-integrity")
    );
    assert_eq!(
        status(&v, "G7.path-ownership"),
        "pass",
        "{}",
        detail(&v, "G7.path-ownership")
    );
    assert_eq!(
        status(&v, "G7.store-health"),
        "pass",
        "{}",
        detail(&v, "G7.store-health")
    );
    assert_eq!(
        status(&v, "G7.vault-perms"),
        "pass",
        "{}",
        detail(&v, "G7.vault-perms")
    );
    // Identity and UI agreement pass only for a clean build of a known commit.
    let known = commit != "unknown";
    let want = if known && clean { "pass" } else { "warn" };
    assert_eq!(
        status(&v, "G7.release-identity"),
        want,
        "{}",
        detail(&v, "G7.release-identity")
    );
    let want_ui = if !known {
        "unsupported"
    } else if clean {
        "pass"
    } else {
        "warn"
    };
    assert_eq!(
        status(&v, "G7.code-ui-agree"),
        want_ui,
        "{}",
        detail(&v, "G7.code-ui-agree")
    );
    // The secret never appears in the output.
    let out = w.fl().args(["doctor"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("SENTINEL"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("SENTINEL"));
}

#[test]
fn an_edited_owned_file_warns_and_a_missing_one_fails_with_the_documented_exit() {
    let w = World::new();
    w.install();
    let bin = w.release_dir().join("bin/fl");
    std::fs::write(&bin, "edited by hand").unwrap();
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 0, "an edit is a warning: {v}");
    assert_eq!(status(&v, "G7.path-ownership"), "warn");
    assert!(detail(&v, "G7.path-ownership").contains("bin/fl was edited"));

    std::fs::remove_file(&bin).unwrap();
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(status(&v, "G7.path-ownership"), "fail");
    assert!(detail(&v, "G7.path-ownership").contains("is missing"));
    assert_eq!(v["overall"], "fail");

    // A foreign file where the alias belongs is a failure too.
    let w = World::new();
    w.install();
    let alias = w.root.path().join(".local/bin/agent-msg");
    std::fs::remove_file(&alias).unwrap();
    std::fs::write(&alias, "#!/bin/sh\n").unwrap();
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 1);
    assert!(detail(&v, "G7.path-ownership").contains("alias"));
}

#[test]
fn a_corrupt_receipt_fails_and_is_not_called_unmanaged() {
    let w = World::new();
    w.install();
    let good = std::fs::read_to_string(w.receipt_path()).unwrap();
    std::fs::write(w.receipt_path(), "{ \"schema_version\": 1, ").unwrap();
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(status(&v, "G7.receipt-integrity"), "fail");
    assert!(detail(&v, "G7.receipt-integrity").contains("not treated as unmanaged"));
    assert_eq!(status(&v, "G7.path-ownership"), "unsupported");

    // An unsupported schema names its version; an unknown field is refused by name.
    let mut r: Value = serde_json::from_str(&good).unwrap();
    r["schema_version"] = 7.into();
    std::fs::write(w.receipt_path(), r.to_string()).unwrap();
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 1);
    assert!(detail(&v, "G7.receipt-integrity").contains("schema_version 7"));

    // Oversized: refused unread.
    let f = std::fs::File::create(w.receipt_path()).unwrap();
    f.set_len(9 * 1024 * 1024).unwrap();
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 1);
    assert!(detail(&v, "G7.receipt-integrity").contains("larger than"));
}

#[test]
fn a_vault_readable_by_others_fails() {
    let w = World::new();
    w.vault(0o644, 0o600);
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(status(&v, "G7.vault-perms"), "fail");
    assert!(detail(&v, "G7.vault-perms").contains("0644"));
    assert!(!v.to_string().contains("SENTINEL"));

    let w = World::new();
    w.vault(0o600, 0o666);
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 1);
    assert!(detail(&v, "G7.vault-perms").contains("world-writable"));
}

#[test]
fn a_store_newer_than_this_build_supports_fails_and_is_not_migrated() {
    let w = World::new();
    let dir = w.home.path().join(".local/share/fl");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fl.redb");
    drop(fl_store::RedbStore::open(&path).unwrap());
    {
        let meta: redb::TableDefinition<&str, u64> = redb::TableDefinition::new("meta");
        let db = redb::Database::open(&path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(meta).unwrap();
            t.insert("format_version", 99u64).unwrap();
        }
        tx.commit().unwrap();
    }
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(status(&v, "G7.store-health"), "fail");
    assert!(
        detail(&v, "G7.store-health").contains("99"),
        "{}",
        detail(&v, "G7.store-health")
    );
    assert!(!dir.join("fl.fls").exists(), "doctor migrated the store");
}

#[test]
fn a_lone_redb_would_migrate_and_doctor_does_not() {
    let w = World::new();
    let dir = w.home.path().join(".local/share/fl");
    std::fs::create_dir_all(&dir).unwrap();
    drop(fl_store::RedbStore::open(&dir.join("fl.redb")).unwrap());
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(status(&v, "G7.store-health"), "warn");
    assert!(detail(&v, "G7.store-health").contains("doctor does not migrate"));
    assert!(!dir.join("fl.fls").exists());
}

#[test]
fn db_names_the_one_store_to_verify() {
    let w = World::new();
    w.store();
    let other = w.cwd.path().join("other.fls");
    drop(fl_store::FerroStore::open(&other).unwrap());
    let (_, v) = w.doctor(&["--db", other.to_str().unwrap()]);
    assert_eq!(status(&v, "G7.store-health"), "pass");
    assert!(detail(&v, "G7.store-health").contains("other.fls"));
    assert!(
        !detail(&v, "G7.store-health").contains("fl.fls: "),
        "{}",
        detail(&v, "G7.store-health")
    );
    // A store that does not exist at the named path is absent, and is not created.
    let nope = w.cwd.path().join("nope/x.redb");
    let before = w.trees();
    let (_, v) = w.doctor(&["--db", nope.to_str().unwrap()]);
    assert_eq!(status(&v, "G7.store-health"), "not-installed");
    assert_eq!(w.trees(), before);
}

#[test]
fn the_prefix_flag_looks_for_the_install_there() {
    let w = World::new();
    w.install();
    // Without the flag the isolated root is found; with --prefix elsewhere, nothing is.
    let elsewhere = w.cwd.path().join("elsewhere");
    let (_, v) = w.doctor(&["--prefix", elsewhere.to_str().unwrap()]);
    assert_eq!(status(&v, "G7.receipt-integrity"), "not-installed");
    let (_, v) = w.doctor(&["--prefix", w.prefix().to_str().unwrap()]);
    assert_eq!(status(&v, "G7.receipt-integrity"), "pass");
    // A prefix that cannot be an install is refused, exit 3.
    w.fl().args(["doctor", "--prefix", "/usr"]).assert().code(3);
    w.fl()
        .args(["doctor", "--prefix", "relative/dir"])
        .assert()
        .code(3);
}

#[test]
fn fix_is_parsed_and_refused_with_exit_3_changing_nothing() {
    let w = World::new();
    w.install();
    let before = w.trees();
    for id in IDS.iter().chain(&["no-such-check"]) {
        let out = w.fl().args(["doctor", "--fix", id]).output().unwrap();
        assert_eq!(out.status.code(), Some(3), "{id}");
        assert!(out.stdout.is_empty());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("repairs are not implemented in this release"),
            "{err}"
        );
    }
    assert_eq!(w.trees(), before);
}

#[test]
fn the_human_report_lists_every_check_and_the_overall() {
    let w = World::new();
    w.install();
    let out = w.fl().arg("doctor").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    for id in IDS
        .into_iter()
        .chain([VENDOR_VERSION_DRIFT, VENDOR_UPDATE_POLICY])
    {
        assert!(text.contains(id), "{id} missing: {text}");
    }
    assert!(text.contains("overall:"), "{text}");
    assert!(text.contains("nothing was changed"), "{text}");
}

/// Seed the host-local writer's persisted baseline shape without starting a hosted session.
fn seed_vendor_baseline(
    store: &impl StateStore,
    vendor: &str,
    cli: Option<&str>,
    daemon: Option<&str>,
) {
    let identity = std::fs::read_to_string("/etc/machine-id")
        .ok()
        .filter(|id| !id.trim().is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .unwrap_or_else(|| "workspace-local".into());
    let host = sha(identity.trim().as_bytes());
    let digest = sha(format!("{}/{vendor}", &host[..40]).as_bytes());
    let known = |version: &str| json!({ "version": version, "launch": "01936a64-ae00-7000-8000-000000000001" });
    store
        .upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: format!("hosted-vendor-baseline-{}", &digest[..40]),
            sensitivity: SensitivityPatch::SetSensitive,
            payload: StatePayload::Runtime(RuntimeEntry {
                value: json!({
                    "cli": cli.map(known),
                    "daemon": daemon.map(known),
                    "last_applied": null,
                }),
                merge_mode: MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(1),
            by: "test".into(),
        })
        .unwrap();
}

#[test]
fn vendor_checks_use_passive_facts_and_the_selected_hub_history() {
    let w = World::new();
    let muse = w.fake_vendor(".local/bin/muse-bin-1.4.25-R1");
    std::os::unix::fs::symlink(&muse, muse.with_file_name("muse")).unwrap();
    let project_store = w.cwd.path().join("project.fls");
    let workspace_store = w.home.path().join("workspace.fls");
    let config = w.home.path().join(".config/fl/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let project = format!(
        "[[project]]\nroot = {:?}\nstore = {:?}\n",
        w.cwd.path(),
        project_store
    );
    std::fs::write(&config, &project).unwrap();
    let store = fl_store::FerroStore::open(&project_store).unwrap();
    seed_vendor_baseline(&store, "muse", Some("1.4.25-R1"), None);
    drop(store);

    let (code, report) = w.doctor(&[]);
    assert_eq!(code, 0);
    assert_eq!(status(&report, VENDOR_VERSION_DRIFT), "pass");
    assert_eq!(status(&report, VENDOR_UPDATE_POLICY), "warn");
    assert!(
        detail(&report, VENDOR_VERSION_DRIFT)
            .contains("muse CLI: installed 1.4.25-R1, last hosted 1.4.25-R1")
    );
    assert!(
        detail(&report, VENDOR_UPDATE_POLICY)
            .contains("muse CLI: configured enabled, hosted launch disabled")
    );

    let store = fl_store::FerroStore::open(&workspace_store).unwrap();
    seed_vendor_baseline(&store, "muse", Some("1.4.24-R1"), None);
    seed_vendor_baseline(&store, "antigravity", Some("0.10.0"), None);
    drop(store);
    std::fs::write(
        &config,
        format!("[state]\nstore = {workspace_store:?}\n{project}"),
    )
    .unwrap();
    w.fake_vendor(".local/bin/agy");
    let (code, report) = w.doctor(&[]);
    assert_eq!(code, 0);
    assert_eq!(status(&report, VENDOR_VERSION_DRIFT), "warn");
    let versions = detail(&report, VENDOR_VERSION_DRIFT);
    assert!(versions.contains("muse CLI: installed 1.4.25-R1, last hosted 1.4.24-R1 (changed)"));
    assert!(versions.contains("antigravity CLI: installed unknown, last hosted 0.10.0"));
    assert!(versions.contains("grok CLI: installed absent, last hosted unknown"));

    let mut command = w.fl();
    command
        .env("FL_DB", &project_store)
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
        .env("MUSE_NO_AUTO_UPDATE", "1");
    let (_, report) = w.doctor_with(command, &[]);
    assert_eq!(status(&report, VENDOR_VERSION_DRIFT), "unsupported");
    assert_eq!(status(&report, VENDOR_UPDATE_POLICY), "pass");
    assert!(
        detail(&report, VENDOR_UPDATE_POLICY)
            .contains("muse CLI: configured disabled, hosted launch disabled")
    );

    let missing_store = w.cwd.path().join("missing.fls");
    let mut command = w.fl();
    command.env("FL_DB", &workspace_store);
    let (_, report) = w.doctor_with(command, &["--db", missing_store.to_str().unwrap()]);
    assert_eq!(status(&report, VENDOR_VERSION_DRIFT), "unsupported");
    assert!(
        detail(&report, VENDOR_VERSION_DRIFT)
            .contains("muse CLI: installed 1.4.25-R1, last hosted unknown")
    );
    assert!(!w.home.path().join("vendor-executed").exists());
}

#[test]
fn a_writer_locked_vendor_history_is_unknown_and_observational() {
    let w = World::new();
    let muse = w.fake_vendor(".local/bin/muse-bin-1.4.25-R1");
    std::os::unix::fs::symlink(&muse, muse.with_file_name("muse")).unwrap();
    let store_path = w.cwd.path().join("host.redb");
    let writer = fl_store::RedbStore::open(&store_path).unwrap();
    seed_vendor_baseline(&writer, "muse", Some("1.4.24-R1"), None);
    let (code, report) = w.doctor(&["--db", store_path.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(status(&report, VENDOR_VERSION_DRIFT), "unsupported");
    assert!(
        detail(&report, VENDOR_VERSION_DRIFT)
            .contains("muse CLI: installed 1.4.25-R1, last hosted unknown")
    );
    assert!(!w.cwd.path().join("host.fls").exists());
}

#[test]
fn codex_client_and_shared_daemon_have_independent_versions_and_update_settings() {
    use std::os::unix::net::UnixListener;

    let w = World::new();
    let cli = w.fake_vendor(
        ".codex/packages/standalone/releases/0.171.0-x86_64-unknown-linux-gnu/bin/codex",
    );
    let cli_link = w.home.path().join(".local/bin/codex");
    std::fs::create_dir_all(cli_link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&cli, &cli_link).unwrap();
    let daemon = w.fake_vendor(
        ".codex/packages/app-server-daemon/releases/0.172.0-x86_64-unknown-linux-gnu/bin/codex",
    );
    let release = daemon.parent().unwrap().parent().unwrap();
    let package = release.parent().unwrap().parent().unwrap();
    std::os::unix::fs::symlink(release, package.join("current")).unwrap();
    let config = w.home.path().join(".codex");
    std::fs::write(
        config.join("config.toml"),
        "check_for_update_on_startup = false",
    )
    .unwrap();
    let daemon_config = config.join("app-server-daemon");
    std::fs::create_dir_all(&daemon_config).unwrap();
    std::fs::write(
        daemon_config.join("settings.json"),
        r#"{"updater":{"autoUpdateEnabled":true,"updateIntervalMinutes":15}}"#,
    )
    .unwrap();
    let socket = config.join("app-server-control/app-server-control.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();

    let store_path = w.cwd.path().join("host.fls");
    let store = fl_store::FerroStore::open(&store_path).unwrap();
    seed_vendor_baseline(&store, "codex", Some("0.171.0"), Some("0.170.0"));
    drop(store);
    let (code, report) = w.doctor(&["--db", store_path.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(status(&report, VENDOR_VERSION_DRIFT), "warn");
    assert_eq!(status(&report, VENDOR_UPDATE_POLICY), "warn");
    let versions = detail(&report, VENDOR_VERSION_DRIFT);
    assert!(versions.contains("codex CLI: installed 0.171.0, last hosted 0.171.0 |"));
    assert!(versions.contains("codex daemon: installed 0.172.0, last hosted 0.170.0 (changed)"));
    let policies = detail(&report, VENDOR_UPDATE_POLICY);
    assert!(policies.contains("codex CLI: configured disabled, hosted launch disabled"));
    assert!(policies.contains("codex daemon: configured enabled (shared daemon)"));

    let oversized = format!("1.2.3-{}", "a".repeat(128));
    for malformed in ["", "1.2.3\u{1b}[31m", oversized.as_str()] {
        let store = fl_store::FerroStore::open(&store_path).unwrap();
        seed_vendor_baseline(&store, "codex", Some(malformed), Some("0.172.0"));
        drop(store);
        let (_, report) = w.doctor(&["--db", store_path.to_str().unwrap()]);
        assert_eq!(status(&report, VENDOR_VERSION_DRIFT), "unsupported");
        let versions = detail(&report, VENDOR_VERSION_DRIFT);
        assert!(versions.contains("codex CLI: installed 0.171.0, last hosted unknown |"));
        assert!(versions.contains("codex daemon: installed 0.172.0, last hosted 0.172.0 |"));
        assert!(!versions.contains("(changed)"));
    }

    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "doctor contacted the Codex daemon"
    );
}

#[test]
fn hub_owner_doctor_uses_the_isolated_home_and_store() {
    const ISOLATED_HOME: &str = "FL_TEST_DOCTOR_ISOLATED_HOME";
    const OWNER: &str = "doctor-owner-test-token";

    if let Some(home) = std::env::var_os(ISOLATED_HOME).map(PathBuf::from) {
        fl_hub::lifecycle::set_global_vendor_version_adapters(
            fl_vendor_adapters::default_vendor_version_registry(),
        );
        let hub = fl_hub::HubState::new(fl_hub::HubServerConfig {
            home_override: Some(home.clone()),
            store_path: home.join("host.fls"),
            repo_root: home,
            owner_token: Some(OWNER.into()),
            ..Default::default()
        });
        let request = fl_hub::HttpRequest::new("GET", "/api/v1/doctor")
            .with_header("Authorization", format!("Bearer {OWNER}"));
        let response = fl_hub::handle_http_request(&hub, &request);
        assert_eq!(response.status, 200);
        let report = response.json_value().unwrap();
        let versions = detail(&report, VENDOR_VERSION_DRIFT);
        assert!(
            versions.contains("codex CLI: installed 0.171.0, last hosted 0.171.0 |"),
            "{versions}"
        );
        assert!(
            versions.contains("codex daemon: installed 0.172.0, last hosted 0.170.0 (changed)"),
            "{versions}"
        );
        assert!(
            detail(&report, VENDOR_UPDATE_POLICY)
                .contains("codex CLI: configured disabled, hosted launch disabled")
        );
        return;
    }

    let local = World::new();
    let ambient = World::new();
    for (world, version, checks_for_updates) in
        [(&local, "0.171.0", false), (&ambient, "0.175.0", true)]
    {
        let exe = world.fake_vendor(&format!(
            ".codex/packages/standalone/releases/{version}-x86_64-unknown-linux-gnu/bin/codex"
        ));
        let launch = world.home.path().join(".local/bin/codex");
        std::fs::create_dir_all(launch.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(exe, launch).unwrap();
        std::fs::write(
            world.home.path().join(".codex/config.toml"),
            format!("check_for_update_on_startup = {checks_for_updates}"),
        )
        .unwrap();
    }
    let daemon = local.fake_vendor(
        ".codex/packages/app-server-daemon/releases/0.172.0-x86_64-unknown-linux-gnu/bin/codex",
    );
    let release = daemon.parent().unwrap().parent().unwrap();
    let package = release.parent().unwrap().parent().unwrap();
    std::os::unix::fs::symlink(release, package.join("current")).unwrap();
    let store = fl_store::FerroStore::open(local.home.path().join("host.fls")).unwrap();
    seed_vendor_baseline(&store, "codex", Some("0.171.0"), Some("0.170.0"));
    drop(store);

    let before = (local.trees(), ambient.trees());
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "hub_owner_doctor_uses_the_isolated_home_and_store",
        ])
        .env_clear()
        .env(ISOLATED_HOME, local.home.path())
        .env("HOME", ambient.home.path())
        .env("FL_INSTALL_ROOT", local.root.path())
        .env("PATH", ambient.home.path().join(".local/bin"))
        .env("CODEX_HOME", ambient.home.path().join(".codex"))
        .env(
            "DOCTOR_PROBE_SENTINEL",
            ambient.home.path().join("vendor-executed"),
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!((local.trees(), ambient.trees()), before);
}

#[test]
fn a_broken_config_does_not_stop_the_doctor_but_keeps_the_store_check_honest() {
    let w = World::new();
    w.store();
    let cfg = w.home.path().join(".config/fl");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.toml"), "this is = = not toml [[[").unwrap();
    let (code, v) = w.doctor(&[]);
    assert_eq!(code, 0, "{v}");
    // The default store was verified, but the project stores could not be listed: not a pass.
    assert_eq!(status(&v, "G7.store-health"), "warn");
    assert!(detail(&v, "G7.store-health").contains("config could not be read"));
    // The broken config's text is never echoed.
    assert!(!v.to_string().contains("this is = ="));
}

#[test]
fn doctor_spawns_nothing_and_needs_no_network_or_path() {
    // An empty environment: no PATH to find a helper on, no proxy, no HOME-wide tools. If the
    // command ran a subprocess or reached the network it would fail or hang here.
    let w = World::new();
    w.install();
    let out = w
        .fl()
        .args(["doctor", "--json"])
        .env_clear()
        .env("HOME", w.home.path())
        .env("FL_INSTALL_ROOT", w.root.path())
        .timeout(std::time::Duration::from_secs(20))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
}
