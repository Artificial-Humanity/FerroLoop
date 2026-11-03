//! The engine on fixtures: every check x every status path, no false pass, determinism and the
//! JSON contract FerroDeck reads.

use super::*;
use crate::build_info::BuildInfo;
use crate::install::{
    BUDGET_EXCEEDED, FileFingerprint, InstallReceipt, MapProbe, OwnedAlias, OwnedPath,
    OwnedPathKind, OwnedUnit, PathState, Product, Profile, RECEIPT_SCHEMA_VERSION, ReceiptLayout,
    ReceiptRelease, ReleaseKind,
};
use serde_json::{Map, Value, json};

const C: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_C: &str = "fedcba9876543210fedcba9876543210fedcba98";

fn h(c: char) -> String {
    c.to_string().repeat(64)
}

fn clean() -> BuildInfo {
    BuildInfo::from_stamp("0.1.0", C, "false", "1", None)
}

fn receipt() -> InstallReceipt {
    InstallReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        release: ReceiptRelease {
            version: "0.1.0".into(),
            name: "0.1.0+0123456789ab".into(),
            kind: ReleaseKind::SourceBuild,
            commit: Some(C.into()),
        },
        profile: Profile::Full,
        products: vec![Product::Ferroloop],
        layout: ReceiptLayout {
            prefix: "/i/share/fl".into(),
            bin_dir: "/i/bin".into(),
            unit_dir: "/i/units".into(),
        },
        artifacts: vec![],
        paths: vec![
            OwnedPath {
                path: "/i/share/fl/releases/r/bin/fl".into(),
                kind: OwnedPathKind::File,
                sha256: Some(h('b')),
                link_target: None,
            },
            OwnedPath {
                path: "/i/share/fl/releases/r".into(),
                kind: OwnedPathKind::Dir,
                sha256: None,
                link_target: None,
            },
            OwnedPath {
                path: "/i/share/fl/current".into(),
                kind: OwnedPathKind::Symlink,
                sha256: None,
                link_target: Some("releases/r".into()),
            },
        ],
        aliases: vec![OwnedAlias {
            name: "agent-msg".into(),
            path: "/i/bin/agent-msg".into(),
            target: "fl".into(),
        }],
        units: vec![OwnedUnit {
            name: "ferrowire.service".into(),
            path: "/i/units/ferrowire.service".into(),
            sha256: h('c'),
            release: "0.1.0+0123456789ab".into(),
        }],
        catalog_entries: vec![],
        config_blocks: vec![],
        journal: vec![],
    }
}

fn file(sha: &str, marker: bool) -> PathState {
    PathState::File(FileFingerprint {
        sha256: sha.into(),
        has_unit_marker: marker,
    })
}

/// A probe that matches `receipt()` exactly.
fn healthy_probe() -> MapProbe {
    MapProbe::new()
        .with("/i/share/fl/releases/r/bin/fl", file(&h('b'), false))
        .with("/i/share/fl/releases/r", PathState::Dir)
        .with(
            "/i/share/fl/current",
            PathState::Symlink {
                target: "releases/r".into(),
            },
        )
        .with(
            "/i/bin/agent-msg",
            PathState::Symlink {
                target: "fl".into(),
            },
        )
        .with("/i/units/ferrowire.service", file(&h('c'), true))
}

fn verified(format: Option<u64>) -> StoreOutcome {
    StoreOutcome::Verified {
        engine: "fls".into(),
        format,
        newest: 8,
        backup_beside: false,
    }
}

fn vault_node(role: &str, mode: u32, uid: u32) -> VaultNode {
    VaultNode {
        role: role.into(),
        path: format!("/h/{role}"),
        node: NodeMeta::Regular { mode, uid },
        parent: NodeMeta::Dir { mode: 0o40700, uid },
    }
}

fn ui_info(commit: &str, dirty: Option<bool>) -> Option<Map<String, Value>> {
    let mut v = json!({"commit": commit, "build_time_unix": 1, "version": "0.1.0"});
    if let Some(d) = dirty {
        v["dirty"] = d.into();
    }
    crate::ui_bundle::parse_ui_build_info(&v.to_string())
}

/// Inputs that should make every check pass.
fn healthy<'a>(probe: &'a MapProbe) -> DoctorInputs<'a> {
    DoctorInputs {
        binary: clean(),
        binary_name: "fl",
        receipt_shown: "/i/share/fl/receipt.json".into(),
        receipt: ReceiptSource::Text(receipt().to_json()),
        probe,
        stores: StoreFacts {
            stores: vec![StoreFact {
                path: "/d/fl.redb".into(),
                outcome: verified(Some(8)),
            }],
            notes: vec![],
        },
        vault: VaultFacts {
            supported: true,
            euid: Some(1000),
            key_in_env: false,
            files: vec![
                vault_node("vault", 0o100600, 1000),
                vault_node("master key", 0o100600, 1000),
            ],
        },
        ui: UiFacts {
            dir: Some("/i/share/fl/current/ui/dist".into()),
            dir_exists: true,
            info: ui_info(&C[..12], Some(false)),
        },
    }
}

fn status(r: &DoctorReport, id: &str) -> CheckStatus {
    r.check(id)
        .unwrap_or_else(|| panic!("no check {id}"))
        .status
}

fn detail(r: &DoctorReport, id: &str) -> String {
    r.check(id).unwrap().detail.clone()
}

#[test]
fn the_report_has_exactly_the_six_ids_in_order_and_a_healthy_install_passes_them_all() {
    let probe = healthy_probe();
    let r = run(&healthy(&probe));
    let ids: Vec<&str> = r.checks.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "G7.release-identity",
            "G7.receipt-integrity",
            "G7.path-ownership",
            "G7.store-health",
            "G7.vault-perms",
            "G7.code-ui-agree"
        ]
    );
    assert_eq!(ids, CHECK_IDS);
    for c in &r.checks {
        assert_eq!(c.status, CheckStatus::Pass, "{}: {}", c.id, c.detail);
    }
    assert_eq!(r.overall, CheckStatus::Pass);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(
        r.summary,
        Summary {
            pass: 6,
            ..Summary::default()
        }
    );
}

#[test]
fn no_check_passes_on_empty_inputs() {
    let probe = MapProbe::new();
    let r = run(&DoctorInputs::empty(&probe));
    assert_eq!(r.checks.len(), 6);
    for c in &r.checks {
        assert_ne!(c.status, CheckStatus::Pass, "{} passed on nothing", c.id);
    }
    assert_eq!(r.overall, CheckStatus::Warn);
    assert_ne!(r.overall, CheckStatus::Pass);
    assert_eq!(r.exit_code(), 0, "nothing installed is not a failure");
    // Not installed, not pass, for every product check.
    for id in [
        ids::RECEIPT_INTEGRITY,
        ids::PATH_OWNERSHIP,
        ids::STORE_HEALTH,
        ids::VAULT_PERMS,
        ids::CODE_UI_AGREE,
    ] {
        assert_eq!(status(&r, id), CheckStatus::NotInstalled, "{id}");
    }
    assert_eq!(
        detail(&r, ids::RECEIPT_INTEGRITY),
        "unmanaged (no install receipt)"
    );
}

#[test]
fn every_check_defaults_away_from_pass_whatever_single_fact_is_missing() {
    // Take a healthy input and remove one fact at a time: the check that needs it must not
    // keep its pass.
    let probe = healthy_probe();
    type Strip = fn(&mut DoctorInputs<'_>);
    let strips: [(&str, Strip); 6] = [
        (ids::RELEASE_IDENTITY, |i| {
            i.binary = BuildInfo::from_stamp("0.1.0", "", "false", "", None)
        }),
        (ids::RECEIPT_INTEGRITY, |i| {
            i.receipt = ReceiptSource::Absent
        }),
        (ids::PATH_OWNERSHIP, |i| i.receipt = ReceiptSource::Absent),
        (ids::STORE_HEALTH, |i| i.stores = StoreFacts::default()),
        (ids::VAULT_PERMS, |i| i.vault.files.clear()),
        (ids::CODE_UI_AGREE, |i| i.ui.info = None),
    ];
    for (id, strip) in strips {
        let mut i = healthy(&probe);
        strip(&mut i);
        assert_ne!(status(&run(&i), id), CheckStatus::Pass, "{id}");
    }
}

#[test]
fn release_identity_table() {
    let probe = MapProbe::new();
    let case = |binary: BuildInfo, receipt: ReceiptSource| {
        let mut i = DoctorInputs::empty(&probe);
        i.binary = binary;
        i.receipt = receipt;
        let r = run(&i);
        (
            status(&r, ids::RELEASE_IDENTITY),
            detail(&r, ids::RELEASE_IDENTITY),
        )
    };
    let none = || ReceiptSource::Absent;

    let (s, d) = case(clean(), none());
    assert_eq!(s, CheckStatus::Pass, "{d}");
    assert!(d.contains("source: source-build"), "{d}");
    assert!(d.contains("not an official release"), "{d}");

    // Unknown commit: never a pass.
    let (s, d) = case(
        BuildInfo::from_stamp("0.1.0", "unknown", "false", "", None),
        none(),
    );
    assert_eq!(s, CheckStatus::Warn);
    assert!(d.contains("commit is unknown"), "{d}");
    // Dirty, and dirty unknown.
    let (s, d) = case(BuildInfo::from_stamp("0.1.0", C, "true", "", None), none());
    assert_eq!(
        (s, d.contains("dirty working tree")),
        (CheckStatus::Warn, true)
    );
    let (s, d) = case(BuildInfo::from_stamp("0.1.0", C, "?", "", None), none());
    assert_eq!((s, d.contains("unknown")), (CheckStatus::Warn, true));

    // A receipt that agrees.
    let text = |r: &InstallReceipt| ReceiptSource::Text(r.to_json());
    let (s, d) = case(clean(), text(&receipt()));
    assert_eq!(s, CheckStatus::Pass, "{d}");
    // A receipt whose release commit differs from the running binary's.
    let mut r = receipt();
    r.release.commit = Some(OTHER_C.into());
    let (s, d) = case(clean(), text(&r));
    assert_eq!(s, CheckStatus::Warn);
    assert!(
        d.contains("fedcba987654") && d.contains("0123456789ab"),
        "{d}"
    );
    // A different version.
    let mut r = receipt();
    r.release.version = "0.2.0".into();
    r.release.name = "0.2.0+0123456789ab".into();
    let (s, d) = case(clean(), text(&r));
    assert_eq!((s, d.contains("version 0.1.0")), (CheckStatus::Warn, true));
    // A different release id.
    let with_id = BuildInfo::from_stamp("0.1.0", C, "false", "", Some("9.9.9+aaaaaaaaaaaa"));
    let (s, d) = case(with_id, text(&receipt()));
    assert_eq!(s, CheckStatus::Warn);
    assert!(d.contains("9.9.9+aaaaaaaaaaaa"), "{d}");
    // A receipt that records no commit gives nothing to compare: not a pass.
    let mut r = receipt();
    r.release.commit = None;
    let (s, d) = case(clean(), text(&r));
    assert_eq!(s, CheckStatus::Warn);
    assert!(
        d.contains("the receipt records no commit to compare"),
        "{d}"
    );
    // With the binary's own commit unknown the one warning is about that.
    let (_, d) = case(
        BuildInfo::from_stamp("0.1.0", "", "false", "", None),
        text(&r),
    );
    assert!(!d.contains("no commit to compare"), "{d}");
    // A corrupt receipt is not compared with: the integrity check owns that.
    let (s, _) = case(clean(), ReceiptSource::Text("{".into()));
    assert_eq!(s, CheckStatus::Pass);
}

#[test]
fn receipt_integrity_table() {
    let probe = MapProbe::new();
    let case = |src: ReceiptSource| {
        let mut i = DoctorInputs::empty(&probe);
        i.receipt_shown = "/i/share/fl/receipt.json".into();
        i.receipt = src;
        let r = run(&i);
        (
            status(&r, ids::RECEIPT_INTEGRITY),
            detail(&r, ids::RECEIPT_INTEGRITY),
        )
    };
    let (s, d) = case(ReceiptSource::Absent);
    assert_eq!(s, CheckStatus::NotInstalled);
    assert_eq!(d, "unmanaged (no install receipt)");

    let (s, d) = case(ReceiptSource::Text(receipt().to_json()));
    assert_eq!(s, CheckStatus::Pass, "{d}");
    assert!(
        d.contains("schema_version 1") && d.contains("0.1.0+0123456789ab"),
        "{d}"
    );

    // Corrupt JSON, truncated, an unknown (secret-shaped) field.
    for text in ["{", "", "not json", "[]"] {
        let (s, d) = case(ReceiptSource::Text(text.into()));
        assert_eq!(s, CheckStatus::Fail, "{text:?}: {d}");
        assert!(d.contains("not treated as unmanaged"), "{d}");
    }
    let mut v = serde_json::to_value(receipt()).unwrap();
    v["api_key"] = "hunter2".into();
    let (s, d) = case(ReceiptSource::Text(v.to_string()));
    assert_eq!(s, CheckStatus::Fail);
    assert!(d.contains("unknown field"), "{d}");
    assert!(!d.contains("hunter2"), "{d}");

    // An unsupported schema names the version.
    let mut v = serde_json::to_value(receipt()).unwrap();
    v["schema_version"] = 2.into();
    let (s, d) = case(ReceiptSource::Text(v.to_string()));
    assert_eq!(s, CheckStatus::Fail);
    assert!(d.contains("schema_version 2"), "{d}");

    // A field-level reason.
    let mut r = receipt();
    r.paths[0].sha256 = Some("xyz".into());
    let (s, d) = case(ReceiptSource::Text(r.to_json()));
    assert_eq!(s, CheckStatus::Fail);
    assert!(d.contains("paths.sha256"), "{d}");

    // Oversized and unreadable arrive as `Unreadable`: a failure, never "unmanaged".
    let (s, d) = case(ReceiptSource::Unreadable(
        "is larger than 8388608 bytes".into(),
    ));
    assert_eq!(s, CheckStatus::Fail);
    assert!(d.contains("larger than"), "{d}");
    let (s, _) = case(ReceiptSource::Unreadable("is not a regular file".into()));
    assert_eq!(s, CheckStatus::Fail);

    // The oversized TEXT is refused by the loader itself.
    let (s, d) = case(ReceiptSource::Text(
        " ".repeat(crate::install::MAX_RECEIPT_BYTES + 1),
    ));
    assert_eq!(s, CheckStatus::Fail);
    assert!(d.contains("bytes"), "{d}");

    let (s, d) = case(ReceiptSource::Unlocatable("HOME is not set".into()));
    assert_eq!(s, CheckStatus::Unsupported);
    assert!(d.contains("HOME"), "{d}");
}

#[test]
fn path_ownership_table() {
    type Edit = fn(MapProbe) -> MapProbe;
    let cases: Vec<(&str, Edit, CheckStatus, &str)> = vec![
        (
            "all match",
            |p| p,
            CheckStatus::Pass,
            "all 5 owned entries match",
        ),
        (
            "edited release file",
            |p| p.with("/i/share/fl/releases/r/bin/fl", file(&h('f'), false)),
            CheckStatus::Warn,
            "/i/share/fl/releases/r/bin/fl was edited",
        ),
        (
            "missing release file",
            |p| p.with("/i/share/fl/releases/r/bin/fl", PathState::Missing),
            CheckStatus::Fail,
            "/i/share/fl/releases/r/bin/fl is missing",
        ),
        (
            "current repointed",
            |p| {
                p.with(
                    "/i/share/fl/current",
                    PathState::Symlink {
                        target: "releases/other".into(),
                    },
                )
            },
            CheckStatus::Warn,
            "points to releases/other",
        ),
        (
            "current missing",
            |p| p.with("/i/share/fl/current", PathState::Missing),
            CheckStatus::Fail,
            "/i/share/fl/current is missing",
        ),
        (
            "current replaced by a directory",
            |p| p.with("/i/share/fl/current", PathState::Dir),
            CheckStatus::Fail,
            "should be a symlink but is a directory",
        ),
        (
            "alias retargeted",
            |p| {
                p.with(
                    "/i/bin/agent-msg",
                    PathState::Symlink {
                        target: "elsewhere".into(),
                    },
                )
            },
            CheckStatus::Warn,
            "alias /i/bin/agent-msg was edited",
        ),
        (
            "alias replaced by a foreign file",
            |p| p.with("/i/bin/agent-msg", file(&h('9'), false)),
            CheckStatus::Fail,
            "should be a symlink but is a file",
        ),
        (
            "release dir is a file",
            |p| p.with("/i/share/fl/releases/r", file(&h('9'), false)),
            CheckStatus::Fail,
            "should be a directory",
        ),
        (
            "unit edited, marker kept",
            |p| p.with("/i/units/ferrowire.service", file(&h('9'), true)),
            CheckStatus::Warn,
            "unit /i/units/ferrowire.service was edited",
        ),
        (
            "unit foreign, no marker",
            |p| p.with("/i/units/ferrowire.service", file(&h('9'), false)),
            CheckStatus::Fail,
            "is foreign",
        ),
        (
            "unit missing",
            |p| p.with("/i/units/ferrowire.service", PathState::Missing),
            CheckStatus::Fail,
            "unit /i/units/ferrowire.service is missing",
        ),
        (
            "a fifo where a file belongs",
            |p| p.with("/i/share/fl/releases/r/bin/fl", PathState::Other),
            CheckStatus::Fail,
            "should be a file but is neither",
        ),
        (
            "unreadable",
            |p| {
                p.with(
                    "/i/share/fl/releases/r/bin/fl",
                    PathState::Unreadable {
                        reason: "permission denied".into(),
                    },
                )
            },
            CheckStatus::Fail,
            "cannot be read (permission denied)",
        ),
        (
            "time budget ran out",
            |p| {
                p.with(
                    "/i/share/fl/releases/r/bin/fl",
                    PathState::Unreadable {
                        reason: BUDGET_EXCEEDED.into(),
                    },
                )
            },
            CheckStatus::Warn,
            "was not checked",
        ),
    ];
    for (name, edit, want, needle) in cases {
        let probe = edit(healthy_probe());
        let r = run(&healthy(&probe));
        assert_eq!(
            status(&r, ids::PATH_OWNERSHIP),
            want,
            "{name}: {}",
            detail(&r, ids::PATH_OWNERSHIP)
        );
        assert!(
            detail(&r, ids::PATH_OWNERSHIP).contains(needle),
            "{name}: wanted `{needle}` in `{}`",
            detail(&r, ids::PATH_OWNERSHIP)
        );
    }
}

#[test]
fn path_ownership_failures_outrank_edits_and_the_list_is_bounded_and_sorted() {
    let probe = healthy_probe()
        .with("/i/share/fl/releases/r/bin/fl", file(&h('f'), false))
        .with("/i/share/fl/current", PathState::Missing);
    let r = run(&healthy(&probe));
    assert_eq!(status(&r, ids::PATH_OWNERSHIP), CheckStatus::Fail);
    let d = detail(&r, ids::PATH_OWNERSHIP);
    assert!(d.starts_with("1 of 5 owned entries failed"), "{d}");
    assert!(d.contains("was edited"), "{d}");

    // Many problems: bounded to 8 and counted.
    let mut rc = receipt();
    for n in 0..20 {
        rc.paths.push(OwnedPath {
            path: format!("/i/extra/{n:02}"),
            kind: OwnedPathKind::Dir,
            sha256: None,
            link_target: None,
        });
    }
    let mut i = healthy(&probe);
    i.receipt = ReceiptSource::Text(rc.to_json());
    let r = run(&i);
    let d = detail(&r, ids::PATH_OWNERSHIP);
    assert!(d.contains("(+"), "{d}");
    assert!(d.contains("/i/extra/00 is missing"), "sorted: {d}");
    assert!(!d.contains("/i/extra/19"), "bounded: {d}");
}

#[test]
fn path_ownership_with_no_usable_receipt_proves_nothing() {
    let probe = healthy_probe();
    for src in [
        ReceiptSource::Text("{".into()),
        ReceiptSource::Unreadable("denied".into()),
        ReceiptSource::Unlocatable("HOME is not set".into()),
    ] {
        let mut i = healthy(&probe);
        i.receipt = src;
        assert_eq!(
            status(&run(&i), ids::PATH_OWNERSHIP),
            CheckStatus::Unsupported
        );
    }
    let mut i = healthy(&probe);
    i.receipt = ReceiptSource::Absent;
    assert_eq!(
        status(&run(&i), ids::PATH_OWNERSHIP),
        CheckStatus::NotInstalled
    );
    // A valid receipt that owns nothing has nothing to verify: not a pass.
    let mut empty = receipt();
    empty.paths.clear();
    empty.aliases.clear();
    empty.units.clear();
    let mut i = healthy(&probe);
    i.receipt = ReceiptSource::Text(empty.to_json());
    assert_eq!(status(&run(&i), ids::PATH_OWNERSHIP), CheckStatus::Warn);
}

#[test]
fn path_ownership_notes_the_config_it_does_not_compare() {
    let probe = healthy_probe();
    let mut rc = receipt();
    rc.config_blocks.push(crate::install::OwnedConfigBlock {
        file: "/i/c.toml".into(),
        block_id: "install".into(),
        sha256: h('e'),
    });
    let mut i = healthy(&probe);
    i.receipt = ReceiptSource::Text(rc.to_json());
    let d = detail(&run(&i), ids::PATH_OWNERSHIP);
    assert!(d.contains("not compared in this release"), "{d}");
}

#[test]
fn store_health_table() {
    let case = |stores: Vec<StoreOutcome>, notes: Vec<String>| {
        let probe = MapProbe::new();
        let mut i = DoctorInputs::empty(&probe);
        i.stores = StoreFacts {
            stores: stores
                .into_iter()
                .enumerate()
                .map(|(n, outcome)| StoreFact {
                    path: format!("/d/{n}.redb"),
                    outcome,
                })
                .collect(),
            notes,
        };
        let r = run(&i);
        (status(&r, ids::STORE_HEALTH), detail(&r, ids::STORE_HEALTH))
    };
    let (s, d) = case(vec![], vec![]);
    assert_eq!(
        (s, d.as_str()),
        (CheckStatus::NotInstalled, "no store is configured")
    );
    let (s, d) = case(vec![StoreOutcome::Absent], vec![]);
    assert_eq!(s, CheckStatus::NotInstalled);
    assert!(d.contains("absent"), "{d}");
    let (s, d) = case(vec![verified(Some(8))], vec![]);
    assert_eq!(s, CheckStatus::Pass, "{d}");
    assert!(d.contains("opened read-only"), "{d}");
    // A verified store and an absent one: the verified one is the evidence.
    assert_eq!(
        case(vec![verified(Some(8)), StoreOutcome::Absent], vec![]).0,
        CheckStatus::Pass
    );
    let (s, d) = case(
        vec![StoreOutcome::Verified {
            engine: "fls".into(),
            format: Some(8),
            newest: 8,
            backup_beside: true,
        }],
        vec![],
    );
    assert_eq!(s, CheckStatus::Pass);
    assert!(d.contains("not current data"), "{d}");

    let (s, d) = case(vec![StoreOutcome::WouldMigrate { format: Some(6) }], vec![]);
    assert_eq!(s, CheckStatus::Warn);
    assert!(
        d.contains("would migrate") && d.contains("doctor does not migrate"),
        "{d}"
    );
    let (s, d) = case(vec![StoreOutcome::Locked], vec![]);
    assert_eq!(s, CheckStatus::Warn);
    assert!(d.contains("took no lock"), "{d}");
    let (s, d) = case(vec![StoreOutcome::NeedsRecovery], vec![]);
    assert_eq!(s, CheckStatus::Warn);
    assert!(
        d.contains("needs recovery by a normal open") && d.contains("doctor does not repair"),
        "{d}"
    );
    let (s, d) = case(
        vec![StoreOutcome::Unsupported {
            reason: "the store holds format 99, which a newer fl wrote".into(),
        }],
        vec![],
    );
    assert_eq!(s, CheckStatus::Fail);
    assert!(d.contains("format 99"), "{d}");
    let (s, _) = case(
        vec![StoreOutcome::Invalid {
            reason: "garbled".into(),
        }],
        vec![],
    );
    assert_eq!(s, CheckStatus::Fail);
    // Fail outranks warn outranks pass.
    let (s, _) = case(
        vec![
            verified(Some(8)),
            StoreOutcome::Locked,
            StoreOutcome::Invalid { reason: "x".into() },
        ],
        vec![],
    );
    assert_eq!(s, CheckStatus::Fail);
    // A note (an incomplete list) keeps a pass from standing.
    let (s, d) = case(
        vec![verified(Some(8))],
        vec!["the config could not be read".into()],
    );
    assert_eq!(s, CheckStatus::Warn);
    assert!(d.contains("could not be read"), "{d}");
    let (s, _) = case(vec![], vec!["no store location".into()]);
    assert_eq!(s, CheckStatus::Warn);
}

#[test]
fn vault_perms_table() {
    let me = 1000;
    let case = |files: Vec<VaultNode>, key_in_env: bool, euid: Option<u32>, supported: bool| {
        let probe = MapProbe::new();
        let mut i = DoctorInputs::empty(&probe);
        i.vault = VaultFacts {
            supported,
            euid,
            key_in_env,
            files,
        };
        let r = run(&i);
        (status(&r, ids::VAULT_PERMS), detail(&r, ids::VAULT_PERMS))
    };
    let absent = |role: &str| VaultNode {
        role: role.into(),
        path: format!("/h/{role}"),
        node: NodeMeta::Absent,
        parent: NodeMeta::Absent,
    };
    let both = |vm: u32, km: u32| {
        vec![
            vault_node("vault", 0o100000 | vm, me),
            vault_node("master key", 0o100000 | km, me),
        ]
    };

    let (s, d) = case(
        vec![absent("vault"), absent("master key")],
        false,
        Some(me),
        true,
    );
    assert_eq!(s, CheckStatus::NotInstalled, "{d}");
    let (s, d) = case(vec![], true, Some(me), true);
    assert_eq!(s, CheckStatus::NotInstalled);
    assert!(d.contains("environment"), "{d}");

    let (s, d) = case(both(0o600, 0o600), false, Some(me), true);
    assert_eq!(s, CheckStatus::Pass, "{d}");
    assert!(d.contains("contents not read"), "{d}");

    // Group- or world-readable: fail. World-writable: fail.
    for (mode, word) in [
        (0o644, "world-readable"),
        (0o640, "group or others"),
        (0o660, "group or others"),
        (0o666, "world-writable"),
        (0o602, "world-writable"),
        (0o777, "world-writable"),
    ] {
        let (s, d) = case(both(mode, 0o600), false, Some(me), true);
        assert_eq!(s, CheckStatus::Fail, "{mode:o}: {d}");
        assert!(d.contains(word), "{mode:o}: {d}");
        let (s, _) = case(both(0o600, mode), false, Some(me), true);
        assert_eq!(s, CheckStatus::Fail, "key {mode:o}");
    }
    // Owner-private but not 0600: a warning.
    let (s, d) = case(both(0o400, 0o600), false, Some(me), true);
    assert_eq!(s, CheckStatus::Warn, "{d}");
    // A wrong owner.
    let mut files = both(0o600, 0o600);
    files[0].node = NodeMeta::Regular {
        mode: 0o100600,
        uid: 0,
    };
    let (s, d) = case(files, false, Some(me), true);
    assert_eq!(s, CheckStatus::Fail);
    assert!(d.contains("uid 0"), "{d}");
    // The running user unknown: cannot compare.
    let (s, _) = case(both(0o600, 0o600), false, None, true);
    assert_eq!(s, CheckStatus::Warn);
    // A loose parent directory warns.
    let mut files = both(0o600, 0o600);
    files[1].parent = NodeMeta::Dir {
        mode: 0o40777,
        uid: me,
    };
    let (s, d) = case(files, false, Some(me), true);
    assert_eq!(s, CheckStatus::Warn);
    assert!(d.contains("directory"), "{d}");
    // Symlinks, directories and unreadable entries.
    for node in [
        NodeMeta::Symlink,
        NodeMeta::Other,
        NodeMeta::Dir {
            mode: 0o40700,
            uid: me,
        },
        NodeMeta::Unreadable("denied".into()),
    ] {
        let mut files = both(0o600, 0o600);
        files[0].node = node;
        assert_eq!(case(files, false, Some(me), true).0, CheckStatus::Fail);
    }
    // A vault without any master key would generate a new one on open.
    let files = vec![vault_node("vault", 0o100600, me), absent("master key")];
    let (s, d) = case(files.clone(), false, Some(me), true);
    assert_eq!(s, CheckStatus::Warn);
    assert!(d.contains("generate a new key"), "{d}");
    assert_eq!(case(files, true, Some(me), true).0, CheckStatus::Pass);
    // A platform that cannot say.
    assert_eq!(case(vec![], false, None, false).0, CheckStatus::Unsupported);
}

#[test]
fn code_ui_agree_table() {
    let probe = MapProbe::new();
    let case = |binary: BuildInfo, ui: UiFacts| {
        let mut i = DoctorInputs::empty(&probe);
        i.binary = binary;
        i.ui = ui;
        let r = run(&i);
        (
            status(&r, ids::CODE_UI_AGREE),
            detail(&r, ids::CODE_UI_AGREE),
        )
    };
    let ui = |info| UiFacts {
        dir: Some("/ui".into()),
        dir_exists: true,
        info,
    };
    let (s, _) = case(clean(), UiFacts::default());
    assert_eq!(s, CheckStatus::NotInstalled);
    let (s, d) = case(
        clean(),
        UiFacts {
            dir: Some("/ui".into()),
            dir_exists: false,
            info: None,
        },
    );
    assert_eq!(s, CheckStatus::NotInstalled);
    assert!(d.contains("/ui"), "{d}");
    // A bundle with no usable build-info.json cannot prove anything.
    let (s, d) = case(clean(), ui(None));
    assert_eq!(s, CheckStatus::Unsupported);
    assert!(d.contains("cannot be proven"), "{d}");
    // The binary's commit is unknown.
    let unknown = BuildInfo::from_stamp("0.1.0", "", "false", "", None);
    let (s, _) = case(unknown, ui(ui_info(&C[..12], Some(false))));
    assert_eq!(s, CheckStatus::Unsupported);
    // Agreement, full or abbreviated.
    for commit in [C, &C[..12], &C[..7]] {
        let (s, d) = case(clean(), ui(ui_info(commit, Some(false))));
        assert_eq!(s, CheckStatus::Pass, "{commit}: {d}");
    }
    // A UI built from another commit.
    let (s, d) = case(clean(), ui(ui_info(&OTHER_C[..12], Some(false))));
    assert_eq!(s, CheckStatus::Fail);
    assert!(
        d.contains("fedcba987654") && d.contains("0123456789ab"),
        "{d}"
    );
    // Same commit, dirty on either side: not proven.
    let (s, _) = case(clean(), ui(ui_info(&C[..12], Some(true))));
    assert_eq!(s, CheckStatus::Warn);
    let dirty = BuildInfo::from_stamp("0.1.0", C, "true", "", None);
    let (s, _) = case(dirty, ui(ui_info(&C[..12], Some(false))));
    assert_eq!(s, CheckStatus::Warn);
    // No configured bundle at all.
    let (s, _) = case(
        clean(),
        UiFacts {
            dir: None,
            dir_exists: true,
            info: ui_info(&C[..12], None),
        },
    );
    assert_eq!(s, CheckStatus::NotInstalled);
}

#[test]
fn overall_and_exit_code_follow_the_worst_check() {
    let probe = healthy_probe();
    let mut i = healthy(&probe);
    i.vault.files[0].node = NodeMeta::Regular {
        mode: 0o100644,
        uid: 1000,
    };
    let r = run(&i);
    assert_eq!(r.overall, CheckStatus::Fail);
    assert_eq!(r.exit_code(), 1);
    assert_eq!(r.summary.fail, 1);
    assert_eq!(r.summary.pass, 5);

    let mut i = healthy(&probe);
    i.ui.info = None;
    let r = run(&i);
    assert_eq!(r.overall, CheckStatus::Unsupported);
    assert_eq!(r.exit_code(), 0);

    let mut i = healthy(&probe);
    i.stores.stores[0].outcome = StoreOutcome::Locked;
    i.ui.info = None;
    assert_eq!(run(&i).overall, CheckStatus::Warn);
}

#[test]
fn the_report_is_deterministic_and_carries_no_timestamp() {
    let probe = healthy_probe();
    let a = run(&healthy(&probe)).to_json();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let b = run(&healthy(&probe)).to_json();
    assert_eq!(a, b);
    assert!(a.ends_with("}\n"));
    let v: Value = serde_json::from_str(&a).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["checks", "overall", "schema_version", "summary"]);
    assert!(!a.contains("generated_at") && !a.contains("time"), "{a}");
    // The text form is stable too.
    assert_eq!(
        run(&healthy(&probe)).render_text(),
        run(&healthy(&probe)).render_text()
    );
}

#[test]
fn the_json_matches_the_contract_ferrodeck_reads() {
    let probe = healthy_probe();
    let mut i = healthy(&probe);
    i.stores.stores[0].outcome = StoreOutcome::WouldMigrate { format: None };
    i.ui.dir = None;
    i.receipt = ReceiptSource::Absent;
    i.vault.files.clear();
    let v: Value = serde_json::from_str(&run(&i).to_json()).unwrap();
    let checks = v["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 6);
    let allowed = ["pass", "warn", "fail", "not-installed", "unsupported"];
    for c in checks {
        let mut keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["detail", "id", "status", "title"]);
        assert!(allowed.contains(&c["status"].as_str().unwrap()), "{c}");
        assert!(!c["title"].as_str().unwrap().is_empty());
    }
    assert!(v["summary"]["not_installed"].is_u64());
    assert_eq!(v["schema_version"], 1);
    // Every status round-trips through its kebab-case wire string.
    for s in [
        CheckStatus::Pass,
        CheckStatus::Warn,
        CheckStatus::Fail,
        CheckStatus::NotInstalled,
        CheckStatus::Unsupported,
    ] {
        assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        let back: CheckStatus = serde_json::from_value(json!(s.as_str())).unwrap();
        assert_eq!(back, s);
    }
}

#[test]
fn a_secret_shaped_receipt_field_is_never_echoed() {
    let probe = MapProbe::new();
    let mut v = serde_json::to_value(receipt()).unwrap();
    v["layout"]["vault_key"] = "s3cr3t-value".into();
    let mut i = DoctorInputs::empty(&probe);
    i.receipt = ReceiptSource::Text(v.to_string());
    let r = run(&i);
    let all = r.to_json() + &r.render_text();
    assert!(!all.contains("s3cr3t-value"), "{all}");
}
