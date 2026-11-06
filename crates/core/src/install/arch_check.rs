//! Checks the manifest's product table against `.fl/architecture.toml`, which
//! stays the architecture model. Products only NAME the components that
//! implement them ([`Product::arch_components`]); this module compares those
//! names and the declared dependencies with the model, and nothing else.

use super::{Product, ReleaseManifest};
use crate::arch::ArchitectureModel;
use std::collections::BTreeSet;

/// Components that belong to no product: the ones that serve every product (the binary's entry
/// point, the MCP catalog writer and the installer's I/O), FerroBrowse, which ships inside
/// the `fl` binary and has no product of its own, and the standalone host crates, which are
/// not an install product in this release.
pub const SHARED_COMPONENTS: &[&str] = &[
    "fl_cli",
    "fl_mcp",
    "fl_install",
    "fl_browse",
    "fl_host",
    "fl_host_proto",
];

/// Dependencies that are real at run time but have no edge in the model: the
/// dependant reaches the store through its caller (the CLI or the hub), not
/// through a crate dependency. The check pins that they stay edge-less, so an
/// edge added later makes this list stale instead of silently wrong.
pub const RUNTIME_ONLY: &[(Product, Product)] = &[
    (Product::Ferroloop, Product::Ferrostore),
    (Product::Ferrotrack, Product::Ferrostore),
];

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
}
