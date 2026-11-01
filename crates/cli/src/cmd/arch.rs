//! `fl arch` CLI subcommands for initializing, inspecting, checking, and exporting
//! the declarative architecture model (spec §1.5).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use fl_core::arch::{
    ArchComponent, ArchComponentKind, ArchEdge, ArchitectureModel, DEFAULT_ARCH_MANIFEST_PATH,
    check_architecture_conformance, export_likec4_dsl, validate_architecture_model,
};
use fl_core::state::validate_slug;
use fl_hub::discovery::{arch_scan_walk, scan_workspace_for_arch};
use serde_json::json;

use crate::cmd::agent::write_atomic_non_symlink;

#[derive(Subcommand)]
pub enum ArchCmd {
    /// Scan the workspace and write a starter `.fl/architecture.toml` manifest.
    Init {
        /// Output manifest path (defaults to `.fl/architecture.toml`).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Overwrite the manifest if it already exists.
        #[arg(long)]
        force: bool,
        /// Format output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Load, validate, and display the architecture model.
    Show {
        /// Architecture manifest path (defaults to `.fl/architecture.toml`).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Format output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check repository package dependencies and source files against the architecture model.
    Check {
        /// Architecture manifest path (defaults to `.fl/architecture.toml`).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Format output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Export the architecture model to deterministic LikeC4 `.c4` DSL.
    Likec4 {
        /// Architecture manifest path (defaults to `.fl/architecture.toml`).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Optional file path to write the LikeC4 `.c4` output to (prints to stdout when omitted).
        #[arg(long, visible_alias = "out")]
        output: Option<PathBuf>,
    },
}

pub fn run(cwd: &Path, cmd: ArchCmd) -> Result<i32, (i32, String)> {
    match cmd {
        ArchCmd::Init { path, force, json } => cmd_init(cwd, path.as_deref(), force, json),
        ArchCmd::Show { path, json } => cmd_show(cwd, path.as_deref(), json),
        ArchCmd::Check { path, json } => cmd_check(cwd, path.as_deref(), json),
        ArchCmd::Likec4 { path, output } => cmd_likec4(cwd, path.as_deref(), output.as_deref()),
    }
}

fn resolve_manifest_path(cwd: &Path, path_opt: Option<&Path>) -> PathBuf {
    match path_opt {
        Some(p) if p.is_absolute() => p.to_path_buf(),
        Some(p) => cwd.join(p),
        None => cwd.join(DEFAULT_ARCH_MANIFEST_PATH),
    }
}

pub fn load_architecture_manifest(
    cwd: &Path,
    path_opt: Option<&Path>,
) -> Result<(PathBuf, ArchitectureModel), (i32, String)> {
    let manifest_path = resolve_manifest_path(cwd, path_opt);
    let raw = fs::read_to_string(&manifest_path).map_err(|e| {
        (
            2,
            format!(
                "could not read architecture manifest {}: {e}",
                manifest_path.display()
            ),
        )
    })?;
    let is_json = manifest_path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    let model: ArchitectureModel = if is_json {
        serde_json::from_str(&raw).map_err(|e| {
            (
                2,
                format!(
                    "{} is not a valid JSON architecture manifest: {e}",
                    manifest_path.display()
                ),
            )
        })?
    } else {
        toml::from_str(&raw).map_err(|e| {
            (
                2,
                format!(
                    "{} is not a valid TOML architecture manifest: {e}",
                    manifest_path.display()
                ),
            )
        })?
    };
    validate_architecture_model(&model)
        .map_err(|e| (2, format!("invalid architecture model: {e}")))?;
    Ok((manifest_path, model))
}

fn slugify_identifier(raw: &str, fallback: &str) -> String {
    let mut slug = String::new();
    for ch in raw.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            slug.push(lower);
        } else if (lower == '-' || lower == '_' || lower == ' ')
            && !slug.is_empty()
            && !slug.ends_with('_')
            && !slug.ends_with('-')
        {
            slug.push(if lower == '-' { '-' } else { '_' });
        }
    }
    while slug.ends_with('-') || slug.ends_with('_') {
        slug.pop();
    }
    if slug.is_empty()
        || !slug
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return fallback.to_string();
    }
    validate_slug(&slug).unwrap_or_else(|_| fallback.to_string())
}

fn infer_architecture_model(cwd: &Path) -> Result<ArchitectureModel, (i32, String)> {
    let raw_dir_name = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("system")
        .to_string();
    let system_id = slugify_identifier(&raw_dir_name.replace('-', "_"), "system");
    let title = if raw_dir_name.trim().is_empty() {
        "System".to_string()
    } else {
        raw_dir_name
    };

    let mut manifest_paths = BTreeSet::new();
    for entry in arch_scan_walk(cwd) {
        let p = entry.path();
        if p.is_file() && p.file_name().and_then(|s| s.to_str()) == Some("Cargo.toml") {
            manifest_paths.insert(p.to_path_buf());
        }
    }

    let mut components = Vec::new();
    let mut pkg_to_comp_id = BTreeMap::new();
    let mut used_ids = BTreeSet::new();

    for manifest in &manifest_paths {
        let Ok(raw) = fs::read_to_string(manifest) else {
            continue;
        };
        let Ok(parsed) = toml::from_str::<toml::Value>(&raw) else {
            continue;
        };
        let Some(pkg_name) = parsed
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        let base_id = slugify_identifier(&pkg_name.replace('-', "_"), "component");
        let mut comp_id = base_id.clone();
        let mut n = 2usize;
        while !used_ids.insert(comp_id.clone()) {
            comp_id = format!("{base_id}_{n}");
            n += 1;
        }

        let rel_dir = manifest
            .parent()
            .and_then(|p| p.strip_prefix(cwd).ok())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let paths = if rel_dir.is_empty() {
            vec!["src/**".to_string(), "Cargo.toml".to_string()]
        } else {
            vec![format!("{rel_dir}/**")]
        };

        let kind = if comp_id.ends_with("_store") {
            ArchComponentKind::Store
        } else if comp_id.ends_with("_hub") {
            ArchComponentKind::Service
        } else {
            ArchComponentKind::Crate
        };

        let description = parsed
            .get("package")
            .and_then(|p| p.get("description"))
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_string();

        pkg_to_comp_id.insert(pkg_name.to_string(), comp_id.clone());
        components.push(ArchComponent {
            id: comp_id,
            title: pkg_name.to_string(),
            kind,
            description,
            package: Some(pkg_name.to_string()),
            paths,
        });
    }

    if cwd.join("ui").is_dir() && used_ids.insert("ferrodeck_ui".to_string()) {
        components.push(ArchComponent {
            id: "ferrodeck_ui".to_string(),
            title: "FerroDeck UI".to_string(),
            kind: ArchComponentKind::Ui,
            description: "Web UI".to_string(),
            package: None,
            paths: vec!["ui/**".to_string()],
        });
    }

    if components.is_empty() {
        components.push(ArchComponent {
            id: system_id.clone(),
            title: title.clone(),
            kind: ArchComponentKind::Crate,
            description: String::new(),
            package: None,
            paths: vec!["**".to_string()],
        });
    }

    let scan = scan_workspace_for_arch(cwd).map_err(|e| (2, e))?;
    let mut edge_pairs = BTreeSet::new();
    for dep in &scan.package_deps {
        if let (Some(from_c), Some(to_c)) = (
            pkg_to_comp_id.get(&dep.from_package),
            pkg_to_comp_id.get(&dep.to_package),
        ) && from_c != to_c
        {
            edge_pairs.insert((from_c.clone(), to_c.clone()));
        }
    }

    let edges = edge_pairs
        .into_iter()
        .map(|(from, to)| ArchEdge {
            from,
            to,
            label: "depends on".to_string(),
        })
        .collect();

    let model = ArchitectureModel {
        system_id,
        title,
        strict_coverage: true,
        components,
        edges,
    };
    validate_architecture_model(&model)
        .map_err(|e| (2, format!("generated invalid architecture model: {e}")))?;
    Ok(model)
}

fn cmd_init(
    cwd: &Path,
    path_opt: Option<&Path>,
    force: bool,
    json_out: bool,
) -> Result<i32, (i32, String)> {
    let manifest_path = resolve_manifest_path(cwd, path_opt);
    if manifest_path.exists() && !force {
        return Err((
            2,
            format!(
                "architecture manifest {} already exists (pass `--force` to overwrite)",
                manifest_path.display()
            ),
        ));
    }

    let model = infer_architecture_model(cwd)?;
    let is_json = manifest_path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    let serialized = if is_json {
        let mut s = serde_json::to_string_pretty(&model)
            .map_err(|e| (2, format!("failed to serialize JSON manifest: {e}")))?;
        s.push('\n');
        s
    } else {
        toml::to_string_pretty(&model)
            .map_err(|e| (2, format!("failed to serialize TOML manifest: {e}")))?
    };

    write_atomic_non_symlink(&manifest_path, serialized.as_bytes()).map_err(|e| (2, e))?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "path": manifest_path.display().to_string(),
                "model": model,
            }))
            .unwrap()
        );
    } else {
        println!(
            "initialized architecture manifest {} ({} components, {} edges)",
            manifest_path.display(),
            model.components.len(),
            model.edges.len()
        );
    }
    Ok(0)
}

fn cmd_show(cwd: &Path, path_opt: Option<&Path>, json_out: bool) -> Result<i32, (i32, String)> {
    let (manifest_path, model) = load_architecture_manifest(cwd, path_opt)?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&model)
                .map_err(|e| (2, format!("failed to serialize architecture model: {e}")))?
        );
    } else {
        println!(
            "System: {} ({}) [strict_coverage = {}]",
            model.title, model.system_id, model.strict_coverage
        );
        println!("Manifest: {}", manifest_path.display());
        println!("Components ({}):", model.components.len());
        for comp in &model.components {
            let pkg = comp.package.as_deref().unwrap_or("-");
            println!(
                "  - {} [{}] title={:?} package={} paths={:?}",
                comp.id, comp.kind, comp.title, pkg, comp.paths
            );
        }
        println!("Edges ({}):", model.edges.len());
        for edge in &model.edges {
            if edge.label.trim().is_empty() {
                println!("  - {} -> {}", edge.from, edge.to);
            } else {
                println!("  - {} -> {} ({})", edge.from, edge.to, edge.label);
            }
        }
    }
    Ok(0)
}

fn cmd_check(cwd: &Path, path_opt: Option<&Path>, json_out: bool) -> Result<i32, (i32, String)> {
    let (_manifest_path, model) = load_architecture_manifest(cwd, path_opt)?;
    let scan = scan_workspace_for_arch(cwd).map_err(|e| (2, e))?;
    let report = check_architecture_conformance(
        &model,
        &scan.package_deps,
        &scan.source_refs,
        &scan.repo_files,
    )
    .map_err(|e| (2, format!("architecture conformance check failed: {e}")))?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|e| (2, format!("failed to serialize conformance report: {e}")))?
        );
    } else if report.passed {
        println!(
            "arch check passed: {} ({} packages, {} files, 0 violations)",
            report.system_id, report.checked_packages, report.checked_files
        );
    } else {
        eprintln!(
            "arch check failed: {} ({} violations across {} packages, {} files):",
            report.system_id,
            report.violations.len(),
            report.checked_packages,
            report.checked_files
        );
        for v in &report.violations {
            eprintln!("  - {v}");
        }
    }

    Ok(if report.passed { 0 } else { 1 })
}

fn cmd_likec4(
    cwd: &Path,
    path_opt: Option<&Path>,
    output_opt: Option<&Path>,
) -> Result<i32, (i32, String)> {
    let (_manifest_path, model) = load_architecture_manifest(cwd, path_opt)?;
    let dsl =
        export_likec4_dsl(&model).map_err(|e| (2, format!("failed to export LikeC4 DSL: {e}")))?;

    if let Some(out_rel) = output_opt {
        let out_path = if out_rel.is_absolute() {
            out_rel.to_path_buf()
        } else {
            cwd.join(out_rel)
        };
        write_atomic_non_symlink(&out_path, dsl.as_bytes()).map_err(|e| (2, e))?;
        println!("wrote LikeC4 DSL to {}", out_path.display());
    } else {
        print!("{dsl}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component_packages(model: &ArchitectureModel) -> BTreeSet<&str> {
        model
            .components
            .iter()
            .filter_map(|c| c.package.as_deref())
            .collect()
    }

    #[test]
    fn init_inference_honours_the_root_manifest_workspace_exclude() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let write = |rel: &str, body: &str| {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        };
        write(
            "crates/a/Cargo.toml",
            "[package]\nname = \"pkg-a\"\nversion = \"0.1.0\"\n",
        );
        write(
            "submodules/Standalone/Cargo.toml",
            "[package]\nname = \"pkg-standalone\"\nversion = \"0.1.0\"\n",
        );

        write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/a\"]\nexclude = [\"submodules\"]\n",
        );
        let model = infer_architecture_model(root).unwrap();
        assert_eq!(component_packages(&model), BTreeSet::from(["pkg-a"]));

        write("Cargo.toml", "[workspace]\nmembers = [\"crates/a\"]\n");
        let model = infer_architecture_model(root).unwrap();
        assert_eq!(
            component_packages(&model),
            BTreeSet::from(["pkg-a", "pkg-standalone"])
        );
    }
}
