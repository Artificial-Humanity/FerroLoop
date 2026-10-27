//! Declarative architecture model, static validation, code-versus-model
//! conformance checking, and deterministic LikeC4 `.c4` DSL export (spec §1).

use crate::state::validate_slug;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Default committed architecture manifest path, relative to the repository root (spec §1.1).
pub const DEFAULT_ARCH_MANIFEST_PATH: &str = ".fl/architecture.toml";

fn default_arch_kind() -> ArchComponentKind {
    ArchComponentKind::Crate
}

/// Kind of component in an [`ArchitectureModel`] (spec §1.1).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ArchComponentKind {
    #[default]
    Crate,
    Service,
    Ui,
    Store,
    External,
}

crate::wire::wire_names!(ArchComponentKind as arch_component_kind_wire {
    Crate => "crate",
    Service => "service",
    Ui => "ui",
    Store => "store",
    External => "external",
});
crate::wire::wire_parse!(ArchComponentKind as arch_component_kind_parse);

impl fmt::Display for ArchComponentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// A single component node inside an [`ArchitectureModel`] (spec §1.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchComponent {
    pub id: String,
    pub title: String,
    #[serde(default = "default_arch_kind")]
    pub kind: ArchComponentKind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Allowed directed dependency edge between two components (spec §1.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchEdge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub label: String,
}

/// Committed declarative architecture model (spec §1.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchitectureModel {
    pub system_id: String,
    pub title: String,
    #[serde(default)]
    pub strict_coverage: bool,
    #[serde(default)]
    pub components: Vec<ArchComponent>,
    #[serde(default)]
    pub edges: Vec<ArchEdge>,
}

/// Extracted dependency between two workspace/external packages from a manifest (spec §1.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageDep {
    pub from_package: String,
    pub to_package: String,
    pub manifest_path: String,
}

/// Extracted source-level reference from a repository file to a component or package (spec §1.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceReference {
    pub from_path: String,
    pub target_component_or_package: String,
}

/// Conformance violation detected when comparing repository dependencies/files against an [`ArchitectureModel`] (spec §1.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArchViolation {
    UndeclaredDependency {
        from_component: String,
        to_component: String,
        evidence: String,
    },
    UnmappedPackage {
        package: String,
        manifest_path: String,
    },
    UnmappedFile {
        path: String,
    },
}

impl fmt::Display for ArchViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UndeclaredDependency {
                from_component,
                to_component,
                evidence,
            } => write!(
                f,
                "undeclared dependency `{from_component} -> {to_component}` ({evidence})"
            ),
            Self::UnmappedPackage {
                package,
                manifest_path,
            } => write!(
                f,
                "unmapped workspace package `{package}` in `{manifest_path}`"
            ),
            Self::UnmappedFile { path } => write!(f, "unmapped repository file `{path}`"),
        }
    }
}

/// Result of running [`check_architecture_conformance`] (spec §1.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchConformanceReport {
    pub system_id: String,
    pub passed: bool,
    pub checked_packages: usize,
    pub checked_files: usize,
    pub violations: Vec<ArchViolation>,
}

/// Errors produced when validating an [`ArchitectureModel`] (spec §1.2).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArchError {
    #[error("invalid architecture slug `{0}`")]
    InvalidSlug(String),
    #[error("architecture system title must not be empty")]
    EmptySystemTitle,
    #[error("architecture model must declare at least one component")]
    EmptyComponents,
    #[error("duplicate component id `{0}` in architecture model")]
    DuplicateComponentId(String),
    #[error("component `{component_id}` must have a non-empty title")]
    EmptyComponentTitle { component_id: String },
    #[error(
        "non-external component `{component_id}` must declare at least one path glob or package name"
    )]
    MissingComponentMapping { component_id: String },
    #[error("component `{component_id}` has invalid path glob `{pattern}`: {reason}")]
    InvalidPathGlob {
        component_id: String,
        pattern: String,
        reason: String,
    },
    #[error("architecture edge `{from} -> {to}` references unknown component `{unknown}`")]
    UnknownEdgeEndpoint {
        from: String,
        to: String,
        unknown: String,
    },
    #[error("architecture edge `{component_id} -> {component_id}` cannot be a self-loop")]
    SelfLoopEdge { component_id: String },
    #[error("duplicate architecture edge `{from} -> {to}`")]
    DuplicateEdge { from: String, to: String },
}

fn validate_canonical_slug(raw: &str) -> Result<(), ArchError> {
    let normalized = validate_slug(raw).map_err(|_| ArchError::InvalidSlug(raw.to_string()))?;
    if normalized != raw {
        return Err(ArchError::InvalidSlug(raw.to_string()));
    }
    Ok(())
}

/// Validate static structural invariants on an [`ArchitectureModel`] (spec §1.2).
pub fn validate_architecture_model(model: &ArchitectureModel) -> Result<(), ArchError> {
    validate_canonical_slug(&model.system_id)?;
    if model.title.trim().is_empty() {
        return Err(ArchError::EmptySystemTitle);
    }
    if model.components.is_empty() {
        return Err(ArchError::EmptyComponents);
    }

    let mut component_ids = BTreeSet::new();
    for component in &model.components {
        validate_canonical_slug(&component.id)?;
        if !component_ids.insert(component.id.as_str()) {
            return Err(ArchError::DuplicateComponentId(component.id.clone()));
        }
        if component.title.trim().is_empty() {
            return Err(ArchError::EmptyComponentTitle {
                component_id: component.id.clone(),
            });
        }
        let has_package = component
            .package
            .as_ref()
            .is_some_and(|p| !p.trim().is_empty());
        let has_non_empty_path = component.paths.iter().any(|p| !p.trim().is_empty());
        if component.kind != ArchComponentKind::External && !has_package && !has_non_empty_path {
            return Err(ArchError::MissingComponentMapping {
                component_id: component.id.clone(),
            });
        }
        for pattern in &component.paths {
            let trimmed = pattern.trim();
            if trimmed.is_empty() {
                return Err(ArchError::InvalidPathGlob {
                    component_id: component.id.clone(),
                    pattern: pattern.clone(),
                    reason: "path glob must not be empty".to_string(),
                });
            }
            Glob::new(trimmed).map_err(|e| ArchError::InvalidPathGlob {
                component_id: component.id.clone(),
                pattern: pattern.clone(),
                reason: e.to_string(),
            })?;
        }
    }

    let mut seen_edges = BTreeSet::new();
    for edge in &model.edges {
        if !component_ids.contains(edge.from.as_str()) {
            return Err(ArchError::UnknownEdgeEndpoint {
                from: edge.from.clone(),
                to: edge.to.clone(),
                unknown: edge.from.clone(),
            });
        }
        if !component_ids.contains(edge.to.as_str()) {
            return Err(ArchError::UnknownEdgeEndpoint {
                from: edge.from.clone(),
                to: edge.to.clone(),
                unknown: edge.to.clone(),
            });
        }
        if edge.from == edge.to {
            return Err(ArchError::SelfLoopEdge {
                component_id: edge.from.clone(),
            });
        }
        if !seen_edges.insert((edge.from.as_str(), edge.to.as_str())) {
            return Err(ArchError::DuplicateEdge {
                from: edge.from.clone(),
                to: edge.to.clone(),
            });
        }
    }

    Ok(())
}

fn normalize_repo_path(path: &str) -> &str {
    path.trim().trim_start_matches("./").trim_matches('/')
}

struct CompiledComponentMatcher<'a> {
    id: &'a str,
    glob_set: GlobSet,
}

fn compile_component_matchers(
    model: &ArchitectureModel,
) -> Result<Vec<CompiledComponentMatcher<'_>>, ArchError> {
    let mut matchers = Vec::with_capacity(model.components.len());
    for component in &model.components {
        let mut builder = GlobSetBuilder::new();
        for pattern in &component.paths {
            let trimmed = normalize_repo_path(pattern);
            let glob = Glob::new(trimmed).map_err(|e| ArchError::InvalidPathGlob {
                component_id: component.id.clone(),
                pattern: pattern.clone(),
                reason: e.to_string(),
            })?;
            builder.add(glob);
        }
        let glob_set = builder.build().map_err(|e| ArchError::InvalidPathGlob {
            component_id: component.id.clone(),
            pattern: component.paths.join(", "),
            reason: e.to_string(),
        })?;
        matchers.push(CompiledComponentMatcher {
            id: component.id.as_str(),
            glob_set,
        });
    }
    Ok(matchers)
}

fn push_violation_unique(violations: &mut Vec<ArchViolation>, violation: ArchViolation) {
    if !violations.contains(&violation) {
        violations.push(violation);
    }
}

/// Check code-versus-model architecture conformance across package dependencies,
/// source-level references, and repository files (spec §1.3).
pub fn check_architecture_conformance(
    model: &ArchitectureModel,
    package_deps: &[PackageDep],
    source_refs: &[SourceReference],
    repo_files: &[String],
) -> Result<ArchConformanceReport, ArchError> {
    validate_architecture_model(model)?;

    let mut package_to_component: BTreeMap<&str, &str> = BTreeMap::new();
    let mut id_or_pkg_to_component: BTreeMap<&str, &str> = BTreeMap::new();
    for component in &model.components {
        id_or_pkg_to_component.insert(component.id.as_str(), component.id.as_str());
        if let Some(pkg) = component.package.as_deref() {
            let pkg_trimmed = pkg.trim();
            if !pkg_trimmed.is_empty() {
                package_to_component.insert(pkg_trimmed, component.id.as_str());
                id_or_pkg_to_component.insert(pkg_trimmed, component.id.as_str());
            }
        }
    }

    let allowed_edges: BTreeSet<(&str, &str)> = model
        .edges
        .iter()
        .map(|e| (e.from.as_str(), e.to.as_str()))
        .collect();

    let matchers = compile_component_matchers(model)?;

    let mut violations = Vec::new();
    let mut unmapped_packages_seen = BTreeSet::new();
    let mut distinct_packages = BTreeSet::new();

    for dep in package_deps {
        let from_pkg = dep.from_package.trim();
        let to_pkg = dep.to_package.trim();
        if !from_pkg.is_empty() {
            distinct_packages.insert(from_pkg);
        }

        let from_comp = package_to_component.get(from_pkg).copied();
        let to_comp = package_to_component.get(to_pkg).copied();

        match (from_comp, to_comp) {
            (Some(from_c), Some(to_c)) => {
                if from_c != to_c && !allowed_edges.contains(&(from_c, to_c)) {
                    push_violation_unique(
                        &mut violations,
                        ArchViolation::UndeclaredDependency {
                            from_component: from_c.to_string(),
                            to_component: to_c.to_string(),
                            evidence: format!(
                                "package `{from_pkg}` depends on `{to_pkg}` in `{}`",
                                dep.manifest_path
                            ),
                        },
                    );
                }
            }
            (None, _)
                if model.strict_coverage
                    && !from_pkg.is_empty()
                    && unmapped_packages_seen.insert(from_pkg.to_string()) =>
            {
                violations.push(ArchViolation::UnmappedPackage {
                    package: from_pkg.to_string(),
                    manifest_path: dep.manifest_path.clone(),
                });
            }
            _ => {}
        }
    }

    for src_ref in source_refs {
        let norm_path = normalize_repo_path(&src_ref.from_path);
        if norm_path.is_empty() {
            continue;
        }
        let target_key = src_ref.target_component_or_package.trim();
        let Some(to_c) = id_or_pkg_to_component.get(target_key).copied() else {
            continue;
        };
        for matcher in &matchers {
            if matcher.glob_set.is_match(norm_path) {
                let from_c = matcher.id;
                if from_c != to_c && !allowed_edges.contains(&(from_c, to_c)) {
                    push_violation_unique(
                        &mut violations,
                        ArchViolation::UndeclaredDependency {
                            from_component: from_c.to_string(),
                            to_component: to_c.to_string(),
                            evidence: format!(
                                "source file `{norm_path}` references `{target_key}`"
                            ),
                        },
                    );
                }
            }
        }
    }

    if model.strict_coverage {
        for file in repo_files {
            let norm_path = normalize_repo_path(file);
            if norm_path.is_empty() {
                continue;
            }
            let matched = matchers.iter().any(|m| m.glob_set.is_match(norm_path));
            if !matched {
                push_violation_unique(
                    &mut violations,
                    ArchViolation::UnmappedFile {
                        path: norm_path.to_string(),
                    },
                );
            }
        }
    }

    let checked_packages = if distinct_packages.is_empty() {
        package_to_component.len()
    } else {
        distinct_packages.len()
    };

    Ok(ArchConformanceReport {
        system_id: model.system_id.clone(),
        passed: violations.is_empty(),
        checked_packages,
        checked_files: repo_files.len(),
        violations,
    })
}

fn escape_likec4_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Export a validated [`ArchitectureModel`] into deterministic LikeC4 `.c4` DSL text (spec §1.4).
pub fn export_likec4_dsl(model: &ArchitectureModel) -> Result<String, ArchError> {
    validate_architecture_model(model)?;

    let mut out = String::new();
    out.push_str("specification {\n");
    out.push_str("  element system\n");
    for kind in ArchComponentKind::ALL {
        out.push_str(&format!("  element {}\n", kind.as_wire()));
    }
    out.push_str("}\n\n");

    let sys_title = escape_likec4_string(model.title.trim());
    out.push_str("model {\n");
    out.push_str(&format!(
        "  {} = system '{}' {{\n",
        model.system_id, sys_title
    ));

    for component in &model.components {
        let comp_title = escape_likec4_string(component.title.trim());
        let desc = component.description.trim();
        if desc.is_empty() {
            out.push_str(&format!(
                "    {} = {} '{}'\n",
                component.id,
                component.kind.as_wire(),
                comp_title
            ));
        } else {
            let comp_desc = escape_likec4_string(desc);
            out.push_str(&format!(
                "    {} = {} '{}' {{\n      description '{}'\n    }}\n",
                component.id,
                component.kind.as_wire(),
                comp_title,
                comp_desc
            ));
        }
    }

    if !model.edges.is_empty() {
        out.push('\n');
        for edge in &model.edges {
            let label = edge.label.trim();
            if label.is_empty() {
                out.push_str(&format!("    {} -> {}\n", edge.from, edge.to));
            } else {
                let escaped_label = escape_likec4_string(label);
                out.push_str(&format!(
                    "    {} -> {} '{}'\n",
                    edge.from, edge.to, escaped_label
                ));
            }
        }
    }

    out.push_str("  }\n");
    out.push_str("}\n\n");

    out.push_str("views {\n");
    out.push_str(&format!("  view index of {} {{\n", model.system_id));
    out.push_str(&format!("    title '{} — System Landscape'\n", sys_title));
    out.push_str("    include *\n");
    out.push_str("  }\n");
    out.push_str("}\n");

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_model() -> ArchitectureModel {
        ArchitectureModel {
            system_id: "ferroloop".into(),
            title: "FerroLoop".into(),
            strict_coverage: true,
            components: vec![
                ArchComponent {
                    id: "fl_core".into(),
                    title: "Core Domain".into(),
                    kind: ArchComponentKind::Crate,
                    description: "Pure domain model".into(),
                    package: Some("fl-core".into()),
                    paths: vec!["crates/core/**".into()],
                },
                ArchComponent {
                    id: "fl_store".into(),
                    title: "Redb Store".into(),
                    kind: ArchComponentKind::Store,
                    description: "Durable local state".into(),
                    package: Some("fl-store".into()),
                    paths: vec!["crates/store/**".into()],
                },
                ArchComponent {
                    id: "fl_hub".into(),
                    title: "Hub Daemon".into(),
                    kind: ArchComponentKind::Service,
                    description: "HTTP and WebSocket server".into(),
                    package: Some("fl-hub".into()),
                    paths: vec!["crates/hub/**".into()],
                },
                ArchComponent {
                    id: "ferrodeck_ui".into(),
                    title: "FerroDeck UI".into(),
                    kind: ArchComponentKind::Ui,
                    description: "Browser command center".into(),
                    package: None,
                    paths: vec!["ui/**".into()],
                },
                ArchComponent {
                    id: "github_api".into(),
                    title: "GitHub API".into(),
                    kind: ArchComponentKind::External,
                    description: String::new(),
                    package: None,
                    paths: vec![],
                },
            ],
            edges: vec![
                ArchEdge {
                    from: "fl_store".into(),
                    to: "fl_core".into(),
                    label: "persists domain types".into(),
                },
                ArchEdge {
                    from: "fl_hub".into(),
                    to: "fl_core".into(),
                    label: "uses domain model".into(),
                },
                ArchEdge {
                    from: "fl_hub".into(),
                    to: "fl_store".into(),
                    label: "queries store".into(),
                },
                ArchEdge {
                    from: "ferrodeck_ui".into(),
                    to: "fl_hub".into(),
                    label: "REST + WebSocket".into(),
                },
            ],
        }
    }

    #[test]
    fn validation_rejects_duplicate_components_self_loops_and_unknown_edges() {
        let model = sample_model();
        assert!(validate_architecture_model(&model).is_ok());

        // Invalid system slug
        let mut bad_sys = model.clone();
        bad_sys.system_id = "Invalid Slug!".into();
        assert!(matches!(
            validate_architecture_model(&bad_sys),
            Err(ArchError::InvalidSlug(_))
        ));

        // Duplicate component ID
        let mut dup_comp = model.clone();
        dup_comp.components.push(dup_comp.components[0].clone());
        assert_eq!(
            validate_architecture_model(&dup_comp),
            Err(ArchError::DuplicateComponentId("fl_core".into()))
        );

        // Missing mapping on non-External component
        let mut unmapped_comp = model.clone();
        unmapped_comp.components[0].package = None;
        unmapped_comp.components[0].paths.clear();
        assert_eq!(
            validate_architecture_model(&unmapped_comp),
            Err(ArchError::MissingComponentMapping {
                component_id: "fl_core".into()
            })
        );

        // Self-loop edge
        let mut self_loop = model.clone();
        self_loop.edges.push(ArchEdge {
            from: "fl_core".into(),
            to: "fl_core".into(),
            label: String::new(),
        });
        assert_eq!(
            validate_architecture_model(&self_loop),
            Err(ArchError::SelfLoopEdge {
                component_id: "fl_core".into()
            })
        );

        // Unknown edge endpoint
        let mut unknown_edge = model.clone();
        unknown_edge.edges.push(ArchEdge {
            from: "fl_hub".into(),
            to: "nonexistent".into(),
            label: String::new(),
        });
        assert_eq!(
            validate_architecture_model(&unknown_edge),
            Err(ArchError::UnknownEdgeEndpoint {
                from: "fl_hub".into(),
                to: "nonexistent".into(),
                unknown: "nonexistent".into(),
            })
        );

        // Duplicate edge
        let mut dup_edge = model.clone();
        dup_edge.edges.push(ArchEdge {
            from: "fl_store".into(),
            to: "fl_core".into(),
            label: "duplicate".into(),
        });
        assert_eq!(
            validate_architecture_model(&dup_edge),
            Err(ArchError::DuplicateEdge {
                from: "fl_store".into(),
                to: "fl_core".into(),
            })
        );
    }

    #[test]
    fn conformance_detects_undeclared_package_and_source_dependencies_and_strict_unmapped() {
        let model = sample_model();

        // Clean pass when all deps and files align with declared architecture.
        let clean_deps = vec![
            PackageDep {
                from_package: "fl-store".into(),
                to_package: "fl-core".into(),
                manifest_path: "crates/store/Cargo.toml".into(),
            },
            PackageDep {
                from_package: "fl-hub".into(),
                to_package: "fl-store".into(),
                manifest_path: "crates/hub/Cargo.toml".into(),
            },
        ];
        let clean_refs = vec![SourceReference {
            from_path: "crates/hub/src/lib.rs".into(),
            target_component_or_package: "fl_core".into(),
        }];
        let clean_files = vec![
            "crates/core/src/lib.rs".into(),
            "crates/store/src/lib.rs".into(),
            "crates/hub/src/lib.rs".into(),
            "ui/index.html".into(),
        ];

        let report =
            check_architecture_conformance(&model, &clean_deps, &clean_refs, &clean_files).unwrap();
        assert!(report.passed, "expected clean pass, got: {:?}", report);
        assert!(report.violations.is_empty());

        // Introduce:
        // 1. Undeclared package dependency: fl-core -> fl-store
        // 2. Undeclared source reference: crates/store/src/lib.rs -> fl_hub
        // 3. Unmapped workspace package: fl-rogue
        // 4. Unmapped repo file: scripts/ stray.py
        let bad_deps = vec![
            PackageDep {
                from_package: "fl-core".into(),
                to_package: "fl-store".into(),
                manifest_path: "crates/core/Cargo.toml".into(),
            },
            PackageDep {
                from_package: "fl-rogue".into(),
                to_package: "fl-core".into(),
                manifest_path: "crates/rogue/Cargo.toml".into(),
            },
        ];
        let bad_refs = vec![SourceReference {
            from_path: "crates/store/src/lib.rs".into(),
            target_component_or_package: "fl_hub".into(),
        }];
        let bad_files = vec!["crates/core/src/lib.rs".into(), "scripts/stray.py".into()];

        let failing =
            check_architecture_conformance(&model, &bad_deps, &bad_refs, &bad_files).unwrap();
        assert!(!failing.passed);
        assert_eq!(
            failing.violations,
            vec![
                ArchViolation::UndeclaredDependency {
                    from_component: "fl_core".into(),
                    to_component: "fl_store".into(),
                    evidence: "package `fl-core` depends on `fl-store` in `crates/core/Cargo.toml`"
                        .into(),
                },
                ArchViolation::UnmappedPackage {
                    package: "fl-rogue".into(),
                    manifest_path: "crates/rogue/Cargo.toml".into(),
                },
                ArchViolation::UndeclaredDependency {
                    from_component: "fl_store".into(),
                    to_component: "fl_hub".into(),
                    evidence: "source file `crates/store/src/lib.rs` references `fl_hub`".into(),
                },
                ArchViolation::UnmappedFile {
                    path: "scripts/stray.py".into(),
                },
            ]
        );
    }

    #[test]
    fn export_likec4_dsl_renders_deterministic_c4_specification_model_and_views() {
        let model = sample_model();
        let dsl = export_likec4_dsl(&model).unwrap();

        assert!(dsl.contains("specification {\n  element system\n  element crate\n  element service\n  element ui\n  element store\n  element external\n}"));
        assert!(dsl.contains("  ferroloop = system 'FerroLoop' {"));
        assert!(dsl.contains(
            "    fl_core = crate 'Core Domain' {\n      description 'Pure domain model'\n    }"
        ));
        assert!(dsl.contains("    github_api = external 'GitHub API'\n"));
        assert!(dsl.contains("    fl_store -> fl_core 'persists domain types'\n"));
        assert!(dsl.contains("  view index of ferroloop {\n    title 'FerroLoop — System Landscape'\n    include *\n  }"));

        // Deterministic across repeated invocations.
        let dsl_again = export_likec4_dsl(&model).unwrap();
        assert_eq!(dsl, dsl_again);
    }
}
