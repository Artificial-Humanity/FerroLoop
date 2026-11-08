//! Checks the manifest's product table against `.fl/architecture.toml`, which
//! stays the architecture model. Products only NAME the components that
//! implement them ([`Product::arch_components`]); this module compares those
//! names and the declared dependencies with the model, and enforces the
//! dependency-inversion boundary between `fl_hub`, `fl_wire`, `fl_host` and
//! `fl_vendor_adapters`.

use super::{Product, ReleaseManifest};
use crate::arch::{ArchitectureModel, PackageDep};
use std::collections::{BTreeMap, BTreeSet};

/// Components that belong to no product: the ones that serve every product (the binary's entry
/// point, the MCP catalog writer and the installer's I/O), FerroBrowse, which ships inside
/// the `fl` binary and has no product of its own, the standalone host crates, and the concrete
/// vendor host adapters (`fl_vendor_adapters`) composed at `fl_cli`.
pub const SHARED_COMPONENTS: &[&str] = &[
    "fl_cli",
    "fl_mcp",
    "fl_install",
    "fl_browse",
    "fl_host",
    "fl_host_proto",
    "fl_vendor_adapters",
];

/// Architecture component id of the concrete vendor host adapters crate.
pub const VENDOR_ADAPTER_COMPONENT: &str = "fl_vendor_adapters";

/// Workspace Cargo package name of the concrete vendor host adapters crate.
pub const VENDOR_ADAPTER_PACKAGE: &str = "fl-vendor-adapters";

/// Components (`(component_id, package_name)`) that must never depend directly or transitively
/// on [`VENDOR_ADAPTER_COMPONENT`] / [`VENDOR_ADAPTER_PACKAGE`]. They depend only on the pure
/// `VendorHostAdapter` and `RuntimePort` traits in `fl-core`.
pub const FORBIDDEN_VENDOR_ADAPTER_CONSUMERS: &[(&str, &str)] = &[
    ("fl_hub", "fl-hub"),
    ("fl_wire", "fl-wire"),
    ("fl_host", "fl-host"),
];

/// Non-growing allowlist of pre-existing legacy vendor module paths in `crates/hub`,
/// `crates/wire`, and `crates/host`. No new vendor-specific module files may be added to
/// those crates; all new vendor host adapters belong in `crates/vendor-adapters`.
pub const LEGACY_VENDOR_MODULE_ALLOWLIST: &[&str] = &[
    "crates/hub/src/cloud_adapters.rs",
    "crates/hub/src/lifecycle/muse_launch_tests.rs",
    "crates/hub/src/lifecycle/vendor_registry_tests.rs",
    "crates/wire/src/codex_control.rs",
    "crates/wire/src/vendor_adapter.rs",
];

/// Dependencies that are real at run time but have no edge in the model: the
/// dependant reaches the store through its caller (the CLI or the hub), not
/// through a crate dependency. The check pins that they stay edge-less, so an
/// edge added later makes this list stale instead of silently wrong.
pub const RUNTIME_ONLY: &[(Product, Product)] = &[
    (Product::Ferroloop, Product::Ferrostore),
    (Product::Ferrotrack, Product::Ferrostore),
];

fn reaches_in_graph<'a>(
    adj: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    start: &'a str,
    target: &'a str,
) -> Option<Vec<String>> {
    let mut visited = BTreeSet::new();
    let mut queue = std::collections::VecDeque::new();
    let mut parent: BTreeMap<&'a str, &'a str> = BTreeMap::new();
    visited.insert(start);
    queue.push_back(start);
    while let Some(curr) = queue.pop_front() {
        let Some(neighbors) = adj.get(curr) else {
            continue;
        };
        for &next in neighbors {
            if !visited.insert(next) {
                continue;
            }
            parent.insert(next, curr);
            if next == target {
                let mut chain = vec![target.to_string()];
                let mut step = target;
                while let Some(&prev) = parent.get(step) {
                    chain.push(prev.to_string());
                    if prev == start {
                        break;
                    }
                    step = prev;
                }
                chain.reverse();
                return Some(chain);
            }
            queue.push_back(next);
        }
    }
    None
}

/// Verify that no forbidden consumer package (`fl-hub`, `fl-wire`, `fl-host`) has a direct or
/// transitive Cargo dependency on `fl-vendor-adapters`.
pub fn check_package_dependency_isolation(package_deps: &[PackageDep]) -> Vec<String> {
    let mut adj: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for dep in package_deps {
        let from = dep.from_package.trim();
        let to = dep.to_package.trim();
        if !from.is_empty() && !to.is_empty() && from != to {
            adj.entry(from).or_default().insert(to);
        }
    }
    let mut problems = Vec::new();
    for &(_comp, pkg) in FORBIDDEN_VENDOR_ADAPTER_CONSUMERS {
        if let Some(chain) = reaches_in_graph(&adj, pkg, VENDOR_ADAPTER_PACKAGE) {
            problems.push(format!(
                "package `{pkg}` must not depend on `{VENDOR_ADAPTER_PACKAGE}` (found path `{}`)",
                chain.join(" -> ")
            ));
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

/// Verify that no new vendor-specific module files are introduced under `crates/hub/src`,
/// `crates/wire/src`, or `crates/host/src` beyond [`LEGACY_VENDOR_MODULE_ALLOWLIST`].
pub fn check_legacy_vendor_module_allowlist(repo_files: &[String]) -> Vec<String> {
    const VENDOR_TOKENS: &[&str] = &[
        "codex",
        "antigravity",
        "agy",
        "claude",
        "muse",
        "grok",
        "ollama",
        "lemonade",
        "vendor",
    ];
    let allowed: BTreeSet<&str> = LEGACY_VENDOR_MODULE_ALLOWLIST.iter().copied().collect();
    let mut problems = Vec::new();
    for raw in repo_files {
        let norm = raw.trim().trim_start_matches("./").trim_matches('/');
        if !(norm.starts_with("crates/hub/src/")
            || norm.starts_with("crates/wire/src/")
            || norm.starts_with("crates/host/src/"))
            || !norm.ends_with(".rs")
        {
            continue;
        }
        let file_stem = norm
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .trim_end_matches(".rs")
            .to_ascii_lowercase();
        let is_vendor_named = VENDOR_TOKENS.iter().any(|tok| {
            file_stem == *tok
                || file_stem.starts_with(&format!("{tok}_"))
                || file_stem.ends_with(&format!("_{tok}"))
                || file_stem.contains(&format!("_{tok}_"))
        });
        if is_vendor_named && !allowed.contains(norm) {
            problems.push(format!(
                "unallowed vendor module `{norm}` in hub/wire/host; add concrete vendor adapters to `crates/vendor-adapters` instead"
            ));
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

pub struct ArchitectureCheck<'a> {
    manifest: &'a ReleaseManifest,
    model: &'a ArchitectureModel,
}

impl<'a> ArchitectureCheck<'a> {
    pub fn new(manifest: &'a ReleaseManifest, model: &'a ArchitectureModel) -> Self {
        Self { manifest, model }
    }

    fn has_edge(&self, from: Product, to: Product) -> bool {
        from.arch_components().iter().any(|a| {
            to.arch_components()
                .iter()
                .any(|b| self.model.edges.iter().any(|e| e.from == *a && e.to == *b))
        })
    }

    /// Every disagreement, sorted. Empty means the two agree.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let ids: BTreeSet<&str> = self
            .model
            .components
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        let mut claimed: BTreeSet<&str> = SHARED_COMPONENTS.iter().copied().collect();
        for p in Product::ALL {
            for c in p.arch_components() {
                claimed.insert(c);
                if !ids.contains(c) {
                    out.push(format!(
                        "product `{p}` names component `{c}`, which .fl/architecture.toml lacks"
                    ));
                }
            }
        }
        for c in &ids {
            if !claimed.contains(c) {
                out.push(format!(
                    "component `{c}` belongs to no product (and is not a shared component)"
                ));
            }
        }
        for e in &self.manifest.products {
            let Some(p) = e.product() else { continue };
            let deps = e
                .required_products()
                .into_iter()
                .chain(e.optional_products());
            for d in deps {
                if p.arch_components().is_empty() || d.arch_components().is_empty() {
                    continue;
                }
                let edge = self.has_edge(p, d);
                let runtime_only = RUNTIME_ONLY.contains(&(p, d));
                if !edge && !runtime_only {
                    out.push(format!(
                        "`{p}` depends on `{d}` but the model has no edge between their components"
                    ));
                }
                if edge && runtime_only {
                    out.push(format!(
                        "`{p}` -> `{d}` is listed as runtime-only but the model has an edge now"
                    ));
                }
            }
        }

        // Enforce DI/IoC boundary: `fl_hub`, `fl_wire`, and `fl_host` must never reach
        // `fl_vendor_adapters` directly or transitively in `.fl/architecture.toml`.
        let mut adj: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for edge in &self.model.edges {
            adj.entry(edge.from.as_str())
                .or_default()
                .insert(edge.to.as_str());
        }
        for &(comp, _pkg) in FORBIDDEN_VENDOR_ADAPTER_CONSUMERS {
            if let Some(chain) = reaches_in_graph(&adj, comp, VENDOR_ADAPTER_COMPONENT) {
                out.push(format!(
                    "component `{comp}` must not depend on `{VENDOR_ADAPTER_COMPONENT}` (found path `{}`)",
                    chain.join(" -> ")
                ));
            }
        }

        out.sort();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::{ArchComponent, ArchComponentKind, ArchEdge};
    use crate::install::default_manifest;

    fn component(id: &str) -> ArchComponent {
        ArchComponent {
            id: id.into(),
            title: id.into(),
            kind: ArchComponentKind::Crate,
            description: String::new(),
            package: None,
            paths: vec![],
        }
    }

    fn edge(a: &str, b: &str) -> ArchEdge {
        ArchEdge {
            from: a.into(),
            to: b.into(),
            label: String::new(),
        }
    }

    /// A model with every component the products name, and every edge the
    /// default manifest's dependencies need.
    fn good_model() -> ArchitectureModel {
        let mut ids: BTreeSet<&str> = SHARED_COMPONENTS.iter().copied().collect();
        for p in Product::ALL {
            ids.extend(p.arch_components());
        }
        ArchitectureModel {
            system_id: "t".into(),
            title: "t".into(),
            strict_coverage: false,
            components: ids.into_iter().map(component).collect(),
            edges: vec![
                edge("fl_wire", "fl_store"),
                edge("fl_watch", "fl_store"),
                edge("fl_hub", "fl_store"),
                edge("fl_hub", "fl_wire"),
                edge("fl_hub", "fl_gate"),
                edge("fl_hub", "fl_watch"),
                edge("fl_hub", "fl_github"),
                edge("fl_vendor_adapters", "fl_core"),
                edge("fl_cli", "fl_vendor_adapters"),
            ],
        }
    }

    #[test]
    fn a_consistent_model_has_no_problems() {
        let m = default_manifest();
        assert_eq!(
            ArchitectureCheck::new(&m, &good_model()).problems(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn each_kind_of_disagreement_is_reported() {
        let m = default_manifest();
        let mut model = good_model();
        model.components.retain(|c| c.id != "fl_gate");
        model.components.push(component("fl_orphan"));
        model
            .edges
            .retain(|e| !(e.from == "fl_hub" && e.to == "fl_wire"));
        model.edges.push(edge("fl_github", "fl_store"));
        let problems = ArchitectureCheck::new(&m, &model).problems();
        let has = |needle: &str| problems.iter().any(|p| p.contains(needle));
        assert!(has("names component `fl_gate`"), "{problems:?}");
        assert!(
            has("component `fl_orphan` belongs to no product"),
            "{problems:?}"
        );
        assert!(has("`ferrodeck` depends on `ferrowire`"), "{problems:?}");
        assert!(
            has("`ferrotrack` -> `ferrostore` is listed as runtime-only"),
            "{problems:?}"
        );
    }

    #[test]
    fn direct_or_transitive_edge_from_hub_wire_or_host_to_vendor_adapters_is_rejected() {
        let m = default_manifest();
        let mut direct = good_model();
        direct.edges.push(edge("fl_wire", "fl_vendor_adapters"));
        let problems = ArchitectureCheck::new(&m, &direct).problems();
        assert!(
            problems.iter().any(|p| p.contains(
                "component `fl_wire` must not depend on `fl_vendor_adapters` (found path `fl_wire -> fl_vendor_adapters`)"
            )),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains(
                "component `fl_hub` must not depend on `fl_vendor_adapters` (found path `fl_hub -> fl_wire -> fl_vendor_adapters`)"
            )),
            "{problems:?}"
        );

        let mut host_edge = good_model();
        host_edge.edges.push(edge("fl_host", "fl_vendor_adapters"));
        let host_problems = ArchitectureCheck::new(&m, &host_edge).problems();
        assert!(
            host_problems.iter().any(|p| p.contains(
                "component `fl_host` must not depend on `fl_vendor_adapters` (found path `fl_host -> fl_vendor_adapters`)"
            )),
            "{host_problems:?}"
        );
    }

    #[test]
    fn package_dependency_isolation_and_legacy_vendor_allowlist_enforce_boundary() {
        let clean_deps = vec![
            PackageDep {
                from_package: "fl-vendor-adapters".into(),
                to_package: "fl-core".into(),
                manifest_path: "crates/vendor-adapters/Cargo.toml".into(),
            },
            PackageDep {
                from_package: "fl-cli".into(),
                to_package: "fl-vendor-adapters".into(),
                manifest_path: "crates/cli/Cargo.toml".into(),
            },
            PackageDep {
                from_package: "fl-hub".into(),
                to_package: "fl-wire".into(),
                manifest_path: "crates/hub/Cargo.toml".into(),
            },
        ];
        assert!(check_package_dependency_isolation(&clean_deps).is_empty());

        let mut bad_deps = clean_deps.clone();
        bad_deps.push(PackageDep {
            from_package: "fl-wire".into(),
            to_package: "fl-vendor-adapters".into(),
            manifest_path: "crates/wire/Cargo.toml".into(),
        });
        let violations = check_package_dependency_isolation(&bad_deps);
        assert_eq!(
            violations,
            vec![
                "package `fl-hub` must not depend on `fl-vendor-adapters` (found path `fl-hub -> fl-wire -> fl-vendor-adapters`)".to_string(),
                "package `fl-wire` must not depend on `fl-vendor-adapters` (found path `fl-wire -> fl-vendor-adapters`)".to_string(),
            ]
        );

        let mut allowed_files: Vec<String> = LEGACY_VENDOR_MODULE_ALLOWLIST
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        allowed_files.push("crates/vendor-adapters/src/codex.rs".into());
        assert!(check_legacy_vendor_module_allowlist(&allowed_files).is_empty());

        allowed_files.push("crates/wire/src/agy_adapter.rs".into());
        let mod_violations = check_legacy_vendor_module_allowlist(&allowed_files);
        assert_eq!(mod_violations.len(), 1);
        assert!(mod_violations[0].contains("crates/wire/src/agy_adapter.rs"));
    }
}
