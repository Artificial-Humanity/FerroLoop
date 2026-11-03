use super::manifest::tests::{linux, signed_manifest};
use super::receipt::tests::h;
use super::*;
use std::io::Read;

fn host() -> HostInfo {
    HostInfo {
        os: Os::Linux,
        arch: Arch::X86_64,
    }
}

fn req(profile: Option<Profile>, add: &[Product], remove: &[Product]) -> InstallRequest {
    InstallRequest {
        profile,
        add: add.to_vec(),
        remove: remove.to_vec(),
        layout: Layout::explicit("/work/fl").unwrap(),
        adopt: false,
    }
}

fn plan_with(r: &InstallRequest) -> InstallPlan {
    plan(r, &default_manifest(), None, host(), &MapProbe::new()).expect("plan")
}

fn ids(p: &InstallPlan) -> Vec<&'static str> {
    p.products.iter().map(|s| s.id.as_wire()).collect()
}

fn action_ids(p: &InstallPlan) -> Vec<String> {
    p.actions.iter().map(|a| a.id.clone()).collect()
}

#[test]
fn each_profile_selects_its_products() {
    let min = plan_with(&req(Some(Profile::Minimal), &[], &[]));
    assert_eq!(ids(&min), ["ferroloop", "ferrostore"]);
    let head = plan_with(&req(Some(Profile::Headless), &[], &[]));
    assert_eq!(
        ids(&head),
        [
            "ferroloop",
            "ferrostore",
            "ferrowire",
            "ferrostate",
            "ferrogate",
            "ferrotrack",
            "ferrowatch"
        ]
    );
    let full = plan_with(&req(Some(Profile::Full), &[], &[]));
    assert_eq!(full.products.len(), 9);
    assert!(ids(&full).contains(&"ferrodeck") && ids(&full).contains(&"ferrostep"));
    let lab = plan_with(&req(Some(Profile::Lab), &[], &[]));
    assert_eq!(ids(&lab), ids(&full));
    assert!(
        plan_with(&req(None, &[], &[])).products.len() == 2,
        "default is minimal"
    );
}

#[test]
fn lab_is_a_superset_of_full_in_products_and_actions() {
    let full = plan_with(&req(Some(Profile::Full), &[], &[]));
    let lab = plan_with(&req(Some(Profile::Lab), &[], &[]));
    let full_actions = action_ids(&full);
    let lab_actions = action_ids(&lab);
    for a in &full_actions {
        assert!(lab_actions.contains(a), "lab lacks full's action {a}");
    }
    let extra: Vec<_> = lab_actions
        .iter()
        .filter(|a| !full_actions.contains(a))
        .collect();
    assert_eq!(extra, ["ferrostore:register_service:ferrostore@.service"]);
}

#[test]
fn the_hidden_store_is_always_there_and_explained() {
    let p = plan_with(&req(Some(Profile::Minimal), &[], &[]));
    let store = &p.products[1];
    assert_eq!(store.id, Product::Ferrostore);
    assert!(store.hidden);
    assert_eq!(store.origin, Origin::Dependency);
    assert_eq!(
        store.included_because,
        [
            "hidden default dependency",
            "included because ferroloop needs ferrostore"
        ]
    );
    assert_eq!(p.products[0].origin, Origin::Required);
}

#[test]
fn adding_a_product_expands_only_its_real_closure_and_says_why() {
    let p = plan_with(&req(Some(Profile::Minimal), &[Product::Ferrodeck], &[]));
    assert_eq!(
        ids(&p),
        ["ferroloop", "ferrostore", "ferrowire", "ferrodeck"]
    );
    let wire = p
        .products
        .iter()
        .find(|s| s.id == Product::Ferrowire)
        .unwrap();
    assert_eq!(wire.origin, Origin::Dependency);
    assert_eq!(
        wire.included_because,
        ["included because ferrodeck needs ferrowire"]
    );
    let store = p
        .products
        .iter()
        .find(|s| s.id == Product::Ferrostore)
        .unwrap();
    assert_eq!(
        store.included_because,
        [
            "hidden default dependency",
            "included because ferroloop needs ferrostore",
            "included because ferrowire needs ferrostore",
            "included because ferrodeck needs ferrostore",
        ]
    );
    // the optional modules of the hub are NOT pulled in
    for m in ["ferrogate", "ferrowatch", "ferrotrack"] {
        assert!(!ids(&p).contains(&m), "{m} must stay out");
    }
    let added = plan_with(&req(Some(Profile::Minimal), &[Product::Ferrostate], &[]));
    assert_eq!(
        added
            .products
            .iter()
            .find(|s| s.id == Product::Ferrostate)
            .unwrap()
            .origin,
        Origin::Added
    );
}

#[test]
fn deselected_optional_modules_become_module_not_installed_routes() {
    let p = plan_with(&req(Some(Profile::Minimal), &[Product::Ferrodeck], &[]));
    let routes: Vec<_> = p
        .module_routes
        .iter()
        .map(|r| {
            (
                r.module.as_wire(),
                r.consumer.as_wire(),
                r.response.as_str(),
            )
        })
        .collect();
    assert_eq!(
        routes,
        [
            ("ferrogate", "ferrodeck", "404 module_not_installed"),
            ("ferrotrack", "ferrodeck", "404 module_not_installed"),
            ("ferrowatch", "ferrodeck", "404 module_not_installed"),
        ]
    );
    let n = p
        .not_installed
        .iter()
        .find(|n| n.product == Product::Ferrogate)
        .unwrap();
    assert_eq!(
        n.cli,
        "ferrogate is not installed (fl install --add ferrogate)"
    );
    assert!(n.hub.contains("module_not_installed") && n.hub.contains("ferrogate"));

    let full_minus_gate = plan_with(&req(Some(Profile::Full), &[], &[Product::Ferrogate]));
    assert_eq!(full_minus_gate.module_routes.len(), 1);
    assert_eq!(full_minus_gate.module_routes[0].module, Product::Ferrogate);
    assert!(
        !action_ids(&full_minus_gate)
            .iter()
            .any(|a| a.contains("ferrogate"))
    );

    let full = plan_with(&req(Some(Profile::Full), &[], &[]));
    assert!(full.module_routes.is_empty() && full.not_installed.is_empty());
    let headless = plan_with(&req(Some(Profile::Headless), &[], &[]));
    assert!(headless.module_routes.is_empty(), "no consumer, no route");
}

#[test]
fn removals_that_break_a_selection_are_refused() {
    let m = default_manifest();
    let go = |r: InstallRequest| plan(&r, &m, None, host(), &MapProbe::new()).unwrap_err();
    assert_eq!(
        go(req(Some(Profile::Full), &[], &[Product::Ferroloop])),
        PlanError::RequiredProduct(Product::Ferroloop)
    );
    assert_eq!(
        go(req(Some(Profile::Full), &[], &[Product::Ferrowire])),
        PlanError::RemoveNeeded {
            product: Product::Ferrowire,
            needed_by: vec![Product::Ferrodeck]
        }
    );
    let e = go(req(Some(Profile::Minimal), &[], &[Product::Ferrostore]));
    assert!(matches!(
        e,
        PlanError::RemoveNeeded {
            product: Product::Ferrostore,
            ..
        }
    ));
    assert_eq!(
        go(req(None, &[Product::Ferrogate], &[Product::Ferrogate])),
        PlanError::AddAndRemove(Product::Ferrogate)
    );
    // removing both the dependent and the dependency is fine
    let ok = plan_with(&req(
        Some(Profile::Full),
        &[],
        &[Product::Ferrodeck, Product::Ferrowire],
    ));
    assert!(!ids(&ok).contains(&"ferrodeck") && !ids(&ok).contains(&"ferrowire"));
}

#[test]
fn exit_codes_follow_usage_versus_refusal() {
    assert_eq!(PlanError::UnknownProduct("x".into()).exit_code(), 2);
    assert_eq!(PlanError::UnknownProfile("x".into()).exit_code(), 2);
    assert_eq!(PlanError::AddAndRemove(Product::Ferrogate).exit_code(), 2);
    assert_eq!(PlanError::PrefixInvalid("x".into()).exit_code(), 2);
    assert_eq!(PlanError::PrefixForbidden("x".into()).exit_code(), 3);
    assert_eq!(PlanError::AdoptNotImplemented.exit_code(), 3);
    assert_eq!(
        PlanError::RequiredProduct(Product::Ferroloop).exit_code(),
        3
    );
    assert_eq!(
        PlanError::UnsupportedPlatform {
            host: linux(),
            products: vec![]
        }
        .exit_code(),
        3
    );
    assert!(InstallRequest::parse_products(&["ferrogate".into(), "nope".into()]).is_err());
    assert_eq!(
        InstallRequest::parse_profile("nope")
            .unwrap_err()
            .to_string(),
        "unknown profile `nope` (accepted: minimal, headless, full, lab)"
    );
}

#[test]
fn an_unsupported_platform_is_refused_before_the_host_is_touched() {
    let probe = MapProbe::new();
    let win = HostInfo {
        os: Os::Windows,
        arch: Arch::X86_64,
    };
    let e = plan(
        &req(Some(Profile::Full), &[], &[]),
        &default_manifest(),
        None,
        win,
        &probe,
    )
    .unwrap_err();
    match &e {
        PlanError::UnsupportedPlatform { host, products } => {
            assert_eq!(host.to_string(), "windows/x86_64");
            assert_eq!(products.len(), 9);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(e.exit_code(), 3);
    assert!(probe.asked().is_empty(), "no path was looked at");

    // only products that are SELECTED matter
    let mut m = default_manifest();
    for e in &mut m.products {
        if e.id == "ferrodeck" {
            e.targets = vec![Target {
                os: Os::Macos,
                arch: Arch::Aarch64,
            }];
        }
    }
    assert!(
        plan(
            &req(Some(Profile::Headless), &[], &[]),
            &m,
            None,
            host(),
            &MapProbe::new()
        )
        .is_ok()
    );
    let e = plan(
        &req(Some(Profile::Headless), &[Product::Ferrodeck], &[]),
        &m,
        None,
        host(),
        &probe,
    )
    .unwrap_err();
    assert_eq!(
        e,
        PlanError::UnsupportedPlatform {
            host: linux(),
            products: vec![Product::Ferrodeck]
        }
    );
    // a dependency of a selected product counts too
    for e in &mut m.products {
        if e.id == "ferrowire" {
            e.targets = vec![Target {
                os: Os::Macos,
                arch: Arch::Aarch64,
            }];
        }
    }
    let e = plan(
        &req(Some(Profile::Minimal), &[Product::Ferrodeck], &[]),
        &m,
        None,
        host(),
        &probe,
    )
    .unwrap_err();
    assert!(
        matches!(e, PlanError::UnsupportedPlatform { ref products, .. }
        if products == &vec![Product::Ferrowire, Product::Ferrodeck])
    );
    assert!(probe.asked().is_empty());
}

#[test]
fn refusals_never_reach_the_probe() {
    let probe = MapProbe::new();
    let m = default_manifest();
    let mut r = req(Some(Profile::Full), &[], &[Product::Ferrowire]);
    assert!(plan(&r, &m, None, host(), &probe).is_err());
    r = req(Some(Profile::Full), &[], &[]);
    r.adopt = true;
    assert_eq!(
        plan(&r, &m, None, host(), &probe).unwrap_err(),
        PlanError::AdoptNotImplemented
    );
    let mut bad = default_manifest();
    bad.schema_version = 9;
    assert!(matches!(
        plan(&req(None, &[], &[]), &bad, None, host(), &probe).unwrap_err(),
        PlanError::InvalidManifest(_)
    ));
    assert!(probe.asked().is_empty());
}

#[test]
fn a_signed_release_needs_an_artifact_for_the_host() {
    let m = signed_manifest();
    let p = plan(
        &req(Some(Profile::Full), &[], &[]),
        &m,
        None,
        host(),
        &MapProbe::new(),
    )
    .unwrap();
    assert_eq!(p.artifacts.len(), 1);
    assert_eq!(p.artifacts[0].id, "fl-linux-x86_64");
    assert_eq!(p.release.name, "0.1.0+0123456789ab");
    assert!(!p.notes.iter().any(|n| n.contains("source build")));
    let arm = HostInfo {
        os: Os::Linux,
        arch: Arch::Aarch64,
    };
    let e = plan(
        &req(Some(Profile::Full), &[], &[]),
        &m,
        None,
        arm,
        &MapProbe::new(),
    )
    .unwrap_err();
    assert!(
        matches!(
            e,
            PlanError::NoArtifact {
                product: Product::Ferroloop,
                ..
            }
        ),
        "{e:?}"
    );
    assert_eq!(e.exit_code(), 3);

    let src = plan_with(&req(Some(Profile::Full), &[], &[]));
    assert!(src.artifacts.is_empty());
    assert_eq!(src.release.name, "0.1.0+unstamped");
    assert!(src.notes.iter().any(|n| n.contains("source build")));
}

#[test]
fn actions_are_separate_and_never_implied_by_a_plain_install() {
    for profile in Profile::ALL {
        let p = plan_with(&req(Some(*profile), &[], &[]));
        for a in &p.actions {
            if a.kind.explicit_only()
                || matches!(
                    a.kind,
                    ActionKind::EnableAtBoot | ActionKind::StartListener | ActionKind::PathChange
                )
            {
                assert_eq!(
                    a.approval,
                    Approval::RequiresApproval,
                    "{profile}: {}",
                    a.id
                );
            }
        }
        // no unit, catalog or PATH change is among the paths a plain install writes
        for path in &p.paths {
            assert!(!path.path.starts_with(&p.layout.unit_dir), "{}", path.path);
            for forbidden in [
                ".gitignore",
                "trust",
                "settings",
                "config.toml",
                ".bashrc",
                ".zshrc",
                ".profile",
            ] {
                assert!(
                    !path.path.contains(forbidden),
                    "{} touches {forbidden}",
                    path.path
                );
            }
        }
        for a in &p.actions {
            if let Some(path) = &a.path {
                assert!(
                    path.starts_with(&p.layout.unit_dir) && a.kind == ActionKind::RegisterService
                );
            }
        }
    }
    let full = plan_with(&req(Some(Profile::Full), &[], &[]));
    let by = |id: &str| full.actions.iter().find(|a| a.id == id).unwrap().approval;
    assert_eq!(
        by("ferrowire:register_service:ferrowire.service"),
        Approval::ByProfile
    );
    assert_eq!(
        by("ferrowire:enable_at_boot:ferrowire.service"),
        Approval::RequiresApproval
    );
    assert_eq!(
        by("ferrogate:init_credentials:vault"),
        Approval::RequiresApproval
    );
    assert_eq!(by("ferrostate:mcp_catalog:ferrostate"), Approval::ByProfile);
    assert_eq!(
        by("ferrostore:migrate_state:store"),
        Approval::RequiresApproval
    );
    let head = plan_with(&req(Some(Profile::Headless), &[], &[]));
    let h = head
        .actions
        .iter()
        .find(|a| a.id == "ferrowire:register_service:ferrowire.service")
        .unwrap();
    assert_eq!(
        h.approval,
        Approval::RequiresApproval,
        "headless does not approve services"
    );
    assert!(!head.actions.iter().any(|a| a.product == Product::Ferrodeck));
    let min = plan_with(&req(Some(Profile::Minimal), &[], &[]));
    assert_eq!(
        action_ids(&min),
        [
            "ferroloop:path_change:launcher-dir",
            "ferrostore:migrate_state:store"
        ],
        "actions sort by kind, then id"
    );
}

#[test]
fn service_actions_are_skipped_off_linux_with_a_note() {
    let mac = HostInfo {
        os: Os::Macos,
        arch: Arch::Aarch64,
    };
    let p = plan(
        &req(Some(Profile::Full), &[], &[]),
        &default_manifest(),
        None,
        mac,
        &MapProbe::new(),
    )
    .unwrap();
    assert!(
        !p.actions
            .iter()
            .any(|a| a.kind == ActionKind::RegisterService)
    );
    assert!(
        p.notes
            .iter()
            .any(|n| n.contains("ferrowire:register_service:ferrowire.service is not planned"))
    );
}

#[test]
fn layouts_are_per_user_and_never_system_wide() {
    for bad in [
        "/",
        "/usr",
        "/usr/local",
        "/usr/local/fl",
        "/etc",
        "/etc/fl",
        "/bin",
        "/sbin",
        "/lib",
        "/boot",
        "/proc/1",
        "/sys",
        "/dev",
        "/home",
        "/root",
        "/var",
        "/opt",
        "/tmp",
        "/home/alice",
        "/Users/bob",
        "/usr/",
        "//usr",
    ] {
        let e = Layout::explicit(bad).unwrap_err();
        assert!(
            matches!(
                e,
                PlanError::PrefixForbidden(_) | PlanError::PrefixInvalid(_)
            ),
            "{bad}: {e:?}"
        );
    }
    for bad in ["", "rel/path", "~/fl", "/a/../b", "/a/./b", "/a\0b"] {
        assert_eq!(Layout::explicit(bad).unwrap_err().exit_code(), 2, "{bad:?}");
    }
    for ok in [
        "/work/fl",
        "/home/alice/.local/share/fl",
        "/var/tmp/x",
        "/tmp/x/",
        "/opt/fl",
    ] {
        Layout::explicit(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    assert_eq!(Layout::explicit("/tmp/x/").unwrap().prefix, "/tmp/x");
    assert!(matches!(
        Layout::explicit("/"),
        Err(PlanError::PrefixForbidden(_))
    ));
    assert!(matches!(
        Layout::under_root("/"),
        Err(PlanError::PrefixForbidden(_))
    ));
    assert!(Layout::under_root("/usr").is_err());
    assert!(Layout::under_root("relative").is_err());
    let root = Layout::under_root("/home/alice").unwrap();
    assert_eq!(root.prefix, "/home/alice/.local/share/fl");
    assert_eq!(root.bin_dir, "/home/alice/.local/bin");
    let d = Layout::user_default();
    assert_eq!(
        (d.prefix.as_str(), d.bin_dir.as_str()),
        ("~/.local/share/fl", "~/.local/bin")
    );
}

#[test]
fn the_plan_names_the_spec_layout_under_the_prefix() {
    let p = plan_with(&req(Some(Profile::Full), &[], &[]));
    let paths: Vec<&str> = p.paths.iter().map(|x| x.path.as_str()).collect();
    for want in [
        "/work/fl",
        "/work/fl/releases",
        "/work/fl/releases/0.1.0+unstamped",
        "/work/fl/releases/0.1.0+unstamped/bin/fl",
        "/work/fl/releases/0.1.0+unstamped/bin/ferrostore",
        "/work/fl/releases/0.1.0+unstamped/ui/dist",
        "/work/fl/current",
        "/work/fl/receipt.json",
        "/work/fl/install.lock",
        "/work/fl/bin/fl",
        "/work/fl/bin/agent-msg",
        "/work/fl/bin/agent-bus-mcp",
    ] {
        assert!(paths.contains(&want), "missing {want} in {paths:?}");
    }
    for x in &p.paths {
        assert!(
            x.path.starts_with("/work/fl"),
            "{} escapes the prefix",
            x.path
        );
    }
    let cur = p
        .paths
        .iter()
        .find(|x| x.role == PathRole::Current)
        .unwrap();
    assert_eq!(cur.link_target.as_deref(), Some("releases/0.1.0+unstamped"));
    let launcher = p
        .paths
        .iter()
        .find(|x| x.role == PathRole::Launcher)
        .unwrap();
    assert_eq!(
        launcher.link_target.as_deref(),
        Some("/work/fl/current/bin/fl")
    );
    let unit = p
        .actions
        .iter()
        .find(|a| a.subject == "ferrowire.service" && a.path.is_some())
        .unwrap();
    assert_eq!(
        unit.path.as_deref(),
        Some("/work/fl/units/ferrowire.service")
    );
    // the default layout stays unexpanded
    let mut r = req(Some(Profile::Minimal), &[], &[]);
    r.layout = Layout::user_default();
    let d = plan_with(&r);
    assert!(
        d.paths.iter().all(|x| x.path.starts_with("~/")),
        "{:?}",
        d.paths
    );
    let text = d.render_text();
    assert!(!text.contains("/home/"));
}

#[test]
fn the_probe_is_only_asked_about_planned_paths() {
    let probe = MapProbe::new();
    let r = req(Some(Profile::Lab), &[], &[]);
    let p = plan(&r, &default_manifest(), None, host(), &probe).unwrap();
    let mut planned: Vec<String> = p.paths.iter().map(|x| x.path.clone()).collect();
    planned.extend(p.actions.iter().filter_map(|a| a.path.clone()));
    let asked = probe.asked();
    assert!(!asked.is_empty());
    for a in &asked {
        assert!(
            planned.contains(a),
            "probed {a}, which is not a planned path"
        );
    }
    let mut unique = asked.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), asked.len(), "no path is read twice");
}

// ------------------------------------------------------------- conflicts

fn file(sha: &str) -> PathState {
    PathState::File(FileFingerprint {
        sha256: sha.into(),
        has_unit_marker: false,
    })
}

fn unit(sha: &str, marker: bool) -> PathState {
    PathState::File(FileFingerprint {
        sha256: sha.into(),
        has_unit_marker: marker,
    })
}

fn link(t: &str) -> PathState {
    PathState::Symlink { target: t.into() }
}

fn conflict_of<'a>(p: &'a InstallPlan, path: &str) -> &'a PlanConflict {
    p.conflicts
        .iter()
        .find(|c| c.path == path)
        .unwrap_or_else(|| panic!("no conflict at {path}: {:?}", p.conflicts))
}

#[test]
fn without_a_receipt_existing_state_is_unmanaged_and_conflicts() {
    let probe = MapProbe::new()
        .with("/work/fl", PathState::Dir)
        .with("/work/fl/bin/fl", file(&h('f')))
        .with("/work/fl/bin/agent-msg", link("/elsewhere/fl"))
        .with("/work/fl/units/ferrowire.service", unit(&h('1'), false))
        .with("/work/fl/units/ferroloop-hub.service", unit(&h('2'), true))
        .with("/work/fl/current", link("releases/old"));
    let p = plan(
        &req(Some(Profile::Full), &[], &[]),
        &default_manifest(),
        None,
        host(),
        &probe,
    )
    .unwrap();
    assert_eq!(p.management, Management::Unmanaged);
    assert_eq!(p.status, PlanStatus::NeedsDecision);
    assert_eq!(
        conflict_of(&p, "/work/fl/bin/fl").kind,
        ConflictKind::Foreign
    );
    assert_eq!(
        conflict_of(&p, "/work/fl/bin/agent-msg").kind,
        ConflictKind::Symlink
    );
    assert_eq!(
        conflict_of(&p, "/work/fl/current").kind,
        ConflictKind::Symlink
    );
    assert_eq!(
        conflict_of(&p, "/work/fl/units/ferrowire.service").kind,
        ConflictKind::UnmarkedUnit
    );
    assert_eq!(
        conflict_of(&p, "/work/fl/units/ferroloop-hub.service").kind,
        ConflictKind::Foreign
    );
    assert_eq!(p.conflicts.len(), 5);
    // the prefix directory merely existing is not a conflict
    assert_eq!(p.paths[0].disposition, Disposition::Exists);
    // what exists is REPORTED, never claimed
    assert!(
        p.unmanaged_existing
            .contains(&"/work/fl/bin/fl".to_string())
    );
    assert!(!p.unmanaged_existing.contains(&"/work/fl".to_string()));
    assert!(
        p.notes
            .iter()
            .any(|n| n.contains("UNMANAGED") && n.contains("--adopt"))
    );
    assert!(p.render_text().contains("unmanaged"));
    // conflicts block, they never plan an overwrite
    assert!(
        p.paths
            .iter()
            .filter(|x| x.path == "/work/fl/bin/fl")
            .all(|x| x.disposition == Disposition::Conflict)
    );
}

#[test]
fn type_mismatches_and_unreadable_paths_are_conflicts() {
    let probe = MapProbe::new()
        .with("/work/fl", file(&h('a')))
        .with("/work/fl/releases", link("/etc"))
        .with("/work/fl/bin/fl", PathState::Dir)
        .with(
            "/work/fl/units/ferrowire.service",
            PathState::Unreadable {
                reason: "permission denied".into(),
            },
        );
    let p = plan(
        &req(Some(Profile::Full), &[], &[]),
        &default_manifest(),
        None,
        host(),
        &probe,
    )
    .unwrap();
    assert_eq!(conflict_of(&p, "/work/fl").kind, ConflictKind::TypeMismatch);
    assert_eq!(
        conflict_of(&p, "/work/fl/releases").kind,
        ConflictKind::Symlink
    );
    assert_eq!(
        conflict_of(&p, "/work/fl/bin/fl").kind,
        ConflictKind::Foreign
    );
    let u = conflict_of(&p, "/work/fl/units/ferrowire.service");
    assert_eq!(u.kind, ConflictKind::Unreadable);
    assert!(u.detail.contains("permission denied"));
}

/// A receipt that owns exactly what `p` plans, and a probe that shows it.
fn owned_world(p: &InstallPlan) -> (InstallReceipt, MapProbe) {
    use super::receipt::*;
    let mut probe = MapProbe::new();
    let mut paths = Vec::new();
    let mut aliases = Vec::new();
    let mut units = Vec::new();
    for x in &p.paths {
        match (x.role, x.kind) {
            (PathRole::Receipt | PathRole::Lock, _) => {}
            (PathRole::Prefix | PathRole::Releases, _) => {
                probe = probe.with(&x.path, PathState::Dir);
            }
            (PathRole::Alias, _) => {
                let t = x.link_target.clone().unwrap();
                probe = probe.with(&x.path, link(&t));
                aliases.push(OwnedAlias {
                    name: "a".into(),
                    path: x.path.clone(),
                    target: t,
                });
            }
            (_, PlannedPathKind::Dir) => {
                probe = probe.with(&x.path, PathState::Dir);
                paths.push(OwnedPath {
                    path: x.path.clone(),
                    kind: OwnedPathKind::Dir,
                    sha256: None,
                    link_target: None,
                });
            }
            (_, PlannedPathKind::File) => {
                probe = probe.with(&x.path, file(&h('1')));
                paths.push(OwnedPath {
                    path: x.path.clone(),
                    kind: OwnedPathKind::File,
                    sha256: Some(h('1')),
                    link_target: None,
                });
            }
            (_, PlannedPathKind::Symlink) => {
                let t = x.link_target.clone().unwrap();
                probe = probe.with(&x.path, link(&t));
                paths.push(OwnedPath {
                    path: x.path.clone(),
                    kind: OwnedPathKind::Symlink,
                    sha256: None,
                    link_target: Some(t),
                });
            }
        }
    }
    for a in &p.actions {
        if let Some(path) = &a.path {
            probe = probe.with(path, unit(&h('2'), true));
            units.push(OwnedUnit {
                name: a.subject.clone(),
                path: path.clone(),
                sha256: h('2'),
                release: p.release.name.clone(),
            });
        }
    }
    let r = InstallReceipt {
        schema_version: 1,
        release: ReceiptRelease {
            version: p.release.version.clone(),
            name: p.release.name.clone(),
            kind: p.release.kind,
            commit: None,
        },
        profile: p.profile,
        products: p.products.iter().map(|s| s.id).collect(),
        layout: ReceiptLayout {
            prefix: p.layout.prefix.clone(),
            bin_dir: p.layout.bin_dir.clone(),
            unit_dir: p.layout.unit_dir.clone(),
        },
        artifacts: vec![],
        paths,
        aliases,
        units,
        catalog_entries: vec![],
        config_blocks: vec![],
        journal: vec![],
    };
    r.validate().unwrap();
    (r, probe)
}

#[test]
fn repeating_an_install_is_idempotent() {
    let r = req(Some(Profile::Lab), &[], &[]);
    let first = plan_with(&r);
    let (receipt, probe) = owned_world(&first);
    let again = plan(&r, &default_manifest(), Some(&receipt), host(), &probe).unwrap();
    assert!(again.conflicts.is_empty(), "{:?}", again.conflicts);
    assert_eq!(again.status, PlanStatus::Ready);
    assert_eq!(again.management, Management::Managed);
    for x in &again.paths {
        let ok = matches!(
            x.disposition,
            Disposition::Keep | Disposition::Exists | Disposition::Update | Disposition::Create
        );
        assert!(ok, "{} -> {:?}", x.path, x.disposition);
        if !matches!(x.role, PathRole::Receipt | PathRole::Lock) {
            assert_ne!(
                x.disposition,
                Disposition::Create,
                "{} would be re-created",
                x.path
            );
        }
    }
    for a in again.actions.iter().filter(|a| a.path.is_some()) {
        assert_eq!(a.disposition, Some(Disposition::Keep), "{}", a.id);
    }
    // with a receipt and no --profile the receipt's selection is the base
    let no_profile = plan(
        &req(None, &[], &[]),
        &default_manifest(),
        Some(&receipt),
        host(),
        &probe,
    )
    .unwrap();
    assert_eq!(ids(&no_profile), ids(&again));
    assert_eq!(no_profile.profile, Profile::Lab);
}

#[test]
fn owned_but_edited_or_moved_things_are_conflicts_not_overwrites() {
    let r = req(Some(Profile::Full), &[], &[]);
    let first = plan_with(&r);
    let (receipt, probe) = owned_world(&first);
    let edited = probe
        .with("/work/fl/releases/0.1.0+unstamped/bin/fl", file(&h('9')))
        .with("/work/fl/bin/fl", link("/somewhere/else"))
        .with("/work/fl/units/ferrowire.service", unit(&h('9'), true))
        .with("/work/fl/units/ferroloop-hub.service", unit(&h('2'), false));
    let p = plan(&r, &default_manifest(), Some(&receipt), host(), &edited).unwrap();
    assert_eq!(
        conflict_of(&p, "/work/fl/releases/0.1.0+unstamped/bin/fl").kind,
        ConflictKind::Edited
    );
    assert_eq!(
        conflict_of(&p, "/work/fl/bin/fl").kind,
        ConflictKind::Edited
    );
    assert_eq!(
        conflict_of(&p, "/work/fl/units/ferrowire.service").kind,
        ConflictKind::Edited
    );
    assert_eq!(
        conflict_of(&p, "/work/fl/units/ferroloop-hub.service").kind,
        ConflictKind::UnmarkedUnit
    );
    assert_eq!(p.conflicts.len(), 4);
    assert_eq!(p.status, PlanStatus::NeedsDecision);
    assert!(
        p.unmanaged_existing.is_empty(),
        "a receipt exists, so nothing is 'unmanaged'"
    );
}

#[test]
fn an_upgrade_repoints_owned_links_and_replaces_owned_units() {
    let r = req(Some(Profile::Full), &[], &[]);
    let old = plan(&r, &default_manifest(), None, host(), &MapProbe::new()).unwrap();
    let (mut receipt, probe) = owned_world(&old);
    // the receipt and disk say release 0.0.9 is active
    for p in &mut receipt.paths {
        if p.path == "/work/fl/current" {
            p.link_target = Some("releases/0.0.9+unstamped".into());
        }
    }
    for u in &mut receipt.units {
        u.release = "0.0.9+unstamped".into();
    }
    let probe = probe.with("/work/fl/current", link("releases/0.0.9+unstamped"));
    let mut newer = default_manifest();
    newer.release.version = "0.1.1".into();
    let p = plan(&r, &newer, Some(&receipt), host(), &probe).unwrap();
    assert!(p.conflicts.is_empty(), "{:?}", p.conflicts);
    let cur = p
        .paths
        .iter()
        .find(|x| x.role == PathRole::Current)
        .unwrap();
    assert_eq!(cur.disposition, Disposition::Repoint);
    assert!(
        p.actions
            .iter()
            .filter(|a| a.path.is_some())
            .all(|a| a.disposition == Some(Disposition::Replace))
    );
    assert!(
        p.notes
            .iter()
            .any(|n| n.contains("0.1.0+unstamped") && n.contains("0.1.1+unstamped"))
    );
}

#[test]
fn an_unowned_release_directory_is_never_reused() {
    let probe = MapProbe::new().with("/work/fl/releases/0.1.0+unstamped", PathState::Dir);
    let p = plan(
        &req(Some(Profile::Minimal), &[], &[]),
        &default_manifest(),
        None,
        host(),
        &probe,
    )
    .unwrap();
    assert_eq!(
        conflict_of(&p, "/work/fl/releases/0.1.0+unstamped").kind,
        ConflictKind::Foreign
    );
}

#[test]
fn dropped_products_are_reported_not_removed() {
    let first = plan_with(&req(Some(Profile::Full), &[], &[]));
    let (receipt, probe) = owned_world(&first);
    let p = plan(
        &req(None, &[], &[Product::Ferrodeck]),
        &default_manifest(),
        Some(&receipt),
        host(),
        &probe,
    )
    .unwrap();
    assert!(!ids(&p).contains(&"ferrodeck"));
    assert!(
        p.notes
            .iter()
            .any(|n| n.contains("drops") && n.contains("ferrodeck"))
    );
}

// -------------------------------------------------------------- determinism

#[test]
fn the_same_inputs_give_the_same_bytes() {
    let r = req(Some(Profile::Lab), &[Product::Ferrostate], &[]);
    let probe = MapProbe::new().with("/work/fl/bin/fl", file(&h('f')));
    let a = plan(&r, &default_manifest(), None, host(), &probe).unwrap();
    let b = plan(
        &r,
        &default_manifest(),
        None,
        host(),
        &MapProbe::new().with("/work/fl/bin/fl", file(&h('f'))),
    )
    .unwrap();
    assert_eq!(a.to_json(), b.to_json());
    assert_eq!(a.render_text(), b.render_text());
    assert!(a.to_json().contains("\"schema_version\": 1"));
    // the order the request lists things in does not matter
    let r1 = req(
        Some(Profile::Minimal),
        &[Product::Ferrowatch, Product::Ferrogate],
        &[],
    );
    let r2 = req(
        Some(Profile::Minimal),
        &[Product::Ferrogate, Product::Ferrowatch],
        &[],
    );
    assert_eq!(plan_with(&r1).to_json(), plan_with(&r2).to_json());
    // so does the order the manifest lists them in
    let mut shuffled = default_manifest();
    shuffled.products.reverse();
    shuffled.profiles.reverse();
    for e in &mut shuffled.products {
        e.actions.reverse();
        e.files.reverse();
    }
    let c = plan(
        &r,
        &shuffled,
        None,
        host(),
        &MapProbe::new().with("/work/fl/bin/fl", file(&h('f'))),
    )
    .unwrap();
    assert_eq!(a.to_json(), c.to_json());
    // a plan carries no timestamp
    let json = a.to_json();
    for word in ["timestamp", "_ms\"", "_at\"", "random", "generated"] {
        assert!(!json.contains(word), "a plan must carry no {word}");
    }
}

#[test]
fn fingerprint_hashes_and_finds_the_marker_in_the_head_only() {
    let f = fingerprint(&b"abc"[..]).unwrap();
    assert_eq!(
        f.sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert!(!f.has_unit_marker);
    let unit = format!("[Unit]\n{UNIT_MARKER} 0.1.0+abc\n");
    assert!(fingerprint(unit.as_bytes()).unwrap().has_unit_marker);
    let late = format!("{}{UNIT_MARKER}", "x".repeat(5000));
    assert!(!fingerprint(late.as_bytes()).unwrap().has_unit_marker);
}

#[test]
fn only_a_marker_line_at_a_line_start_marks_a_unit() {
    let m = |s: &str| fingerprint(s.as_bytes()).unwrap().has_unit_marker;
    assert!(m("[Unit]\n# generated by fl install 0.1.0+abc\n"));
    assert!(m("# generated by fl install 0.1.0+abc"));
    // a foreign file that merely MENTIONS the text is not ours
    assert!(!m(
        "[Unit]\nDescription=see '# generated by fl install 0.1.0' in docs\n"
    ));
    assert!(!m("[Unit]\n  # generated by fl install 0.1.0\n"));
    assert!(!m("[Unit]\n#generated by fl install 0.1.0\n"));
    assert!(!m("[Unit]\n# generated by fl install\n"));
    assert!(!m("[Unit]\n# generated by fl installer 0.1.0\n"));
    assert!(!m(&format!(
        "{}\n# generated by fl install 0.1.0\n",
        "x".repeat(5000)
    )));
}

#[test]
fn fingerprint_stops_reading_at_the_cap() {
    struct Endless(u64);
    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0 += buf.len() as u64;
            assert!(self.0 <= 1000 + 2 * 64 * 1024 + 1, "read far past the cap");
            buf.fill(0);
            Ok(buf.len())
        }
    }
    let err = plan::fingerprint_capped(Endless(0), 1000).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("larger than"));
}

#[test]
fn a_symlinked_prefix_is_a_conflict_that_names_its_target() {
    let probe = MapProbe::new()
        .with("/work/fl", link("/data/fl"))
        .with("/work/fl/releases", link("/elsewhere"));
    let p = plan(
        &req(Some(Profile::Minimal), &[], &[]),
        &default_manifest(),
        None,
        host(),
        &probe,
    )
    .unwrap();
    let c = conflict_of(&p, "/work/fl");
    assert_eq!(c.kind, ConflictKind::Symlink);
    assert!(
        c.detail.contains("the install prefix itself") && c.detail.contains("/data/fl"),
        "{}",
        c.detail
    );
    let r = conflict_of(&p, "/work/fl/releases");
    assert!(r.detail.contains("a directory under the prefix") && r.detail.contains("/elsewhere"));
    assert_eq!(p.status, PlanStatus::NeedsDecision);
}

#[test]
fn actions_left_out_of_a_profile_are_said_so() {
    let p = plan_with(&req(Some(Profile::Minimal), &[Product::Ferrodeck], &[]));
    assert!(!p.actions.iter().any(|a| a.product == Product::Ferrodeck));
    for id in [
        "ferrodeck:register_service:ferroloop-hub.service",
        "ferrodeck:enable_at_boot:ferroloop-hub.service",
        "ferrowire:register_service:ferrowire.service",
    ] {
        assert!(
            p.notes
                .contains(&format!("action {id} is not planned in profile minimal")),
            "{id}: {:?}",
            p.notes
        );
    }
    // a profile that plans them says nothing
    let full = plan_with(&req(Some(Profile::Full), &[], &[]));
    assert!(
        !full
            .notes
            .iter()
            .any(|n| n.contains("ferroloop-hub.service is not planned"))
    );
}
