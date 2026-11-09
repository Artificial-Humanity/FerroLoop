//! Metadata-only store census. Never uses a store opener, header reader or socket.
use super::RecoveryError;
pub use super::inventory_metadata::{
    MAX_ARTIFACT_BYTES, metadata_digest, preflight_candidates, read_metadata_artifact,
};
use super::inventory_metadata::{absolute, epoch, record, scan, unit_metadata};
pub use super::inventory_types::*;
use crate::{choose_backend, physical_path};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug)]
pub struct HubLookup {
    pub valid_slug: bool,
    pub logical_path: PathBuf,
    pub refusal_status: Option<u16>,
    pub fallback: bool,
}
/// Metadata counterpart of hub::records::{normalize_project,project_redb_with}.
/// Cross-crate parity tests pin both branches without importing hub into store.
pub fn hub_lookup(home: &Path, raw: &str, env: Option<&str>) -> Result<HubLookup, RecoveryError> {
    let raw = raw.trim();
    let slug = if raw.is_empty() {
        "ferroloop".to_string()
    } else {
        raw.to_ascii_lowercase()
    };
    let valid = slug.len() <= 64
        && slug != "fl"
        && slug
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_');
    let logical_path = home.join(".local/share/fl").join(format!("{slug}.redb"));
    if !valid {
        return Ok(HubLookup {
            valid_slug: false,
            logical_path,
            refusal_status: Some(400),
            fallback: false,
        });
    }
    let backend =
        choose_backend(&logical_path, env).map_err(|e| RecoveryError::Refused(e.to_string()))?;
    let physical = physical_path(&logical_path, backend);
    let (refusal_status, fallback) = match fs::symlink_metadata(&physical) {
        Ok(m) if m.file_type().is_symlink() || !m.is_file() => (Some(400), false),
        Ok(_) => (None, false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if physical != logical_path
                && fs::symlink_metadata(&logical_path).is_ok_and(|m| m.is_file())
            {
                (Some(409), false)
            } else {
                (None, true)
            }
        }
        Err(_) => (Some(503), false),
    };
    Ok(HubLookup {
        valid_slug: true,
        logical_path,
        refusal_status,
        fallback,
    })
}
fn normalize<T: Serialize>(items: &mut Vec<T>) {
    items.sort_by_cached_key(|item| {
        serde_json::to_string(item).expect("inventory metadata serializes")
    });
    items.dedup_by(|a, b| serde_json::to_string(a).ok() == serde_json::to_string(b).ok());
}
/// Collect only paths, metadata and separately bounded metadata artifacts. This never
/// opens `.fls`/`.redb`, invokes a store helper, repairs a binding or contacts an endpoint.
pub fn collect_inventory(inputs: &InventoryInputs) -> Result<StoreMap, RecoveryError> {
    if inputs.roots.is_empty() || inputs.roots.iter().any(|p| !absolute(p)) {
        return Err(RecoveryError::Refused(
            "explicit absolute scan roots without parent traversal are required".into(),
        ));
    }
    let mut roots = inputs.roots.clone();
    roots.sort();
    roots.dedup();
    let mut warnings = inputs.warnings.clone();
    let mut paths = BTreeSet::new();
    for root in &roots {
        scan(root, &mut paths, &mut warnings);
    }
    let mut bindings = Vec::new();
    let mut additional = BTreeSet::new();
    let mut derived = BTreeSet::new();
    for input in inputs.bindings.iter().chain(&inputs.deployment.bindings) {
        if !absolute(&input.logical_path) {
            return Err(RecoveryError::Refused(
                "binding paths must be absolute and contain no parent traversal".into(),
            ));
        }
        let mut logical = input.logical_path.clone();
        let mut refusal = None;
        let mut fallback = false;
        let mut valid_slug = true;
        if let Some(slug) = &input.hub_slug {
            let home = inputs
                .home
                .as_deref()
                .ok_or_else(|| RecoveryError::Refused("hub lookup needs explicit home".into()))?;
            let lookup = hub_lookup(home, slug, input.backend_env.as_deref())?;
            logical = lookup.logical_path;
            refusal = lookup.refusal_status;
            fallback = lookup.fallback;
            valid_slug = lookup.valid_slug;
        }
        let backend = choose_backend(&logical, input.backend_env.as_deref())
            .map_err(|e| RecoveryError::Refused(e.to_string()))?;
        let physical = physical_path(&logical, backend);
        let target = if refusal.is_some() {
            None
        } else if fallback {
            input.fallback_target.clone()
        } else {
            Some(physical.clone())
        };
        if valid_slug {
            paths.insert(physical.clone());
            if logical != physical && fs::symlink_metadata(&logical).is_ok() {
                paths.insert(logical.clone());
            }
            if let Some(target) = &target {
                paths.insert(target.clone());
            }
            if backend == crate::BackendKind::Ferro
                && !physical
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(".code.fls"))
            {
                let sidecar = physical.with_extension("code.fls");
                derived.insert(sidecar.clone());
                paths.insert(sidecar);
            }
        }
        if let Some(code) = refusal {
            warnings.push(format!(
                "{} binding refused with status {code}: {}",
                input.caller, input.scope
            ));
        }
        bindings.push(BindingEdge {
            caller: input.caller.clone(),
            scope: input.scope.clone(),
            project_root: input.project_root.clone(),
            logical_path: logical,
            selected_backend: backend.as_str().into(),
            backend_source: if input.logical_path.extension().is_some_and(|e| e == "fls") {
                "suffix".into()
            } else {
                input
                    .backend_env
                    .clone()
                    .unwrap_or_else(|| "default:ferro".into())
            },
            physical_path: physical,
            effective_path: target,
            source: input.source.clone(),
            precedence: input.precedence.clone(),
            fallback_target: input.fallback_target.clone(),
            fallback_used: fallback,
            refusal_status: refusal,
            declared_mode: input.declared_mode.clone(),
            mismatch: false,
        });
    }
    let mut index_bindings = Vec::new();
    for primary in &bindings {
        if primary.selected_backend != "ferro" || primary.refusal_status.is_some() {
            continue;
        }
        let Some(target) = &primary.effective_path else {
            continue;
        };
        if target
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(".code.fls"))
        {
            continue;
        }
        let sidecar = target.with_extension("code.fls");
        paths.insert(sidecar.clone());
        derived.insert(sidecar.clone());
        let mut edge = primary.clone();
        edge.caller = "code index".into();
        edge.logical_path = sidecar.clone();
        edge.physical_path = sidecar.clone();
        edge.effective_path = Some(sidecar);
        edge.source = "crates/store/src/code_knowledge.rs::sidecar_path".into();
        edge.precedence = "derived beside the resolved ferro primary".into();
        edge.fallback_target = None;
        edge.fallback_used = false;
        edge.declared_mode = "direct_embedded".into();
        edge.backend_source = "derived sidecar".into();
        index_bindings.push(edge);
    }
    bindings.extend(index_bindings);
    for unit in &inputs.deployment.units {
        if !absolute(&unit.store_path) {
            return Err(RecoveryError::Refused(
                "unit store path must be absolute".into(),
            ));
        }
        paths.insert(unit.store_path.clone());
    }
    for reference in &inputs.deployment.recovery_references {
        if !absolute(&reference.store_path) {
            return Err(RecoveryError::Refused(
                "recovery reference store path must be absolute".into(),
            ));
        }
        paths.insert(reference.store_path.clone());
    }
    for edge in &mut bindings {
        if edge.caller == "hub" {
            edge.mismatch = inputs
                .bindings
                .iter()
                .filter(|b| b.caller == "config" && b.scope == edge.scope)
                .any(|b| {
                    choose_backend(&b.logical_path, b.backend_env.as_deref()).is_ok_and(|kind| {
                        physical_path(&b.logical_path, kind)
                            != edge
                                .effective_path
                                .clone()
                                .unwrap_or_else(|| edge.physical_path.clone())
                    })
                });
            if edge.mismatch {
                warnings.push(format!("config/hub binding mismatch: {}", edge.scope));
            }
        }
    }
    let all_paths: Vec<_> = paths.iter().cloned().collect();
    let mut records: BTreeMap<String, StoreRecord> = BTreeMap::new();
    for path in &paths {
        if !roots.iter().any(|r| path.starts_with(r)) {
            if let Some(parent) = path.parent() {
                additional.insert(parent.to_path_buf());
            }
            warnings.push(format!(
                "binding outside declared scan roots: {}",
                path.display()
            ));
        }
        let mut item = record(path, &mut warnings);
        if let Some(canonical) = &item.identity.canonical_path
            && !roots.iter().any(|r| canonical.starts_with(r))
        {
            if let Some(parent) = canonical.parent() {
                additional.insert(parent.to_path_buf());
            }
            warnings.push(format!(
                "canonical target outside declared scan roots: {}",
                canonical.display()
            ));
        }
        if derived.contains(path) {
            item.classification = "derived".into();
            item.classification_evidence
                .push("code sidecar derived from a primary binding".into());
        } else if bindings
            .iter()
            .any(|b| b.physical_path == *path || b.effective_path.as_ref() == Some(path))
        {
            item.classification = "authoritative_candidate".into();
            item.classification_evidence
                .push("configured consumer binding; ownership not established".into());
        } else if path.extension().is_some_and(|e| e == "redb") {
            item.classification = "historical_candidate".into();
            item.classification_evidence
                .push("unbound redb file; not a current recovery artifact".into());
        } else {
            warnings.push(format!("unclassified store candidate: {}", path.display()));
        }
        for edge in &bindings {
            if edge.physical_path == *path || edge.effective_path.as_ref() == Some(path) {
                item.ownership
                    .declared_modes
                    .push(edge.declared_mode.clone());
                item.ownership.mode_sources.push(edge.source.clone());
                if edge.effective_path.as_ref() != Some(path) {
                    continue;
                }
                item.consumers.extend(
                    inputs
                        .consumers
                        .iter()
                        .chain(&inputs.deployment.consumers)
                        .filter(|c| {
                            (c.scope == edge.scope || c.scope == "*")
                                && if c.binding_callers.is_empty() {
                                    c.caller == edge.caller
                                } else {
                                    c.binding_callers.contains(&edge.caller)
                                }
                        })
                        .cloned(),
                );
            }
        }
        for unit in &inputs.deployment.units {
            if unit.store_path == *path {
                if let Some(mode) = &unit.declared_mode {
                    item.ownership.declared_modes.push(mode.clone());
                    item.ownership
                        .mode_sources
                        .push(format!("supplied unit metadata:{}", unit.name));
                }
                let unit = unit_metadata(unit, &all_paths);
                warnings.extend(unit.validation_warnings.clone());
                item.ownership.units.push(unit);
            }
        }
        item.recovery.extend(
            inputs
                .deployment
                .recovery_references
                .iter()
                .filter(|r| r.store_path == *path)
                .cloned(),
        );
        let key = item
            .identity
            .filesystem_identity
            .clone()
            .unwrap_or_else(|| format!("path:{}", path.display()));
        if let Some(existing) = records.get_mut(&key) {
            existing
                .identity
                .given_paths
                .extend(item.identity.given_paths);
            existing
                .identity
                .symlink_chain
                .extend(item.identity.symlink_chain);
            existing
                .ownership
                .declared_modes
                .extend(item.ownership.declared_modes);
            existing
                .ownership
                .mode_sources
                .extend(item.ownership.mode_sources);
            existing.ownership.units.extend(item.ownership.units);
            existing.consumers.extend(item.consumers);
            existing.recovery.extend(item.recovery);
            existing
                .classification_evidence
                .extend(item.classification_evidence);
            if existing.classification == "unclassified" {
                existing.classification = item.classification;
            } else if item.classification != "unclassified"
                && item.classification != existing.classification
            {
                existing
                    .ownership
                    .conflicts
                    .push("aliases have different classifications".into());
                warnings.push("aliases have different classifications".into());
            }
        } else {
            records.insert(key, item);
        }
    }
    for (name, obs) in [
        ("external consumer", &inputs.deployment.external_coverage),
        ("unit", &inputs.deployment.unit_coverage),
    ] {
        if !obs
            .as_ref()
            .is_some_and(|o| o.attributed() && o.value == "complete")
        {
            warnings.push(format!("{name} coverage unknown or not complete"));
        }
    }
    let mut stores: Vec<_> = records.into_values().collect();
    for store in &mut stores {
        store.identity.given_paths.sort();
        store.identity.given_paths.dedup();
        store.identity.symlink_chain.sort();
        store.identity.symlink_chain.dedup();
        store.ownership.declared_modes.sort();
        store.ownership.declared_modes.dedup();
        store.ownership.mode_sources.sort();
        store.ownership.mode_sources.dedup();
        store.classification_evidence.sort();
        store.classification_evidence.dedup();
        normalize(&mut store.consumers);
        normalize(&mut store.ownership.units);
        normalize(&mut store.recovery);
    }
    stores.sort_by(|a, b| a.identity.given_paths.cmp(&b.identity.given_paths));
    bindings.sort_by(|a, b| {
        (&a.caller, &a.scope, &a.logical_path).cmp(&(&b.caller, &b.scope, &b.logical_path))
    });
    for consumer in &inputs.deployment.consumers {
        if consumer.source_symbol.is_empty()
            || consumer.source_revision.is_empty()
            || consumer.provenance.is_empty()
        {
            warnings.push(format!(
                "unattributed external consumer: {}",
                consumer.caller
            ));
        }
    }
    let mut coverage = inputs.deployment.clone();
    normalize(&mut coverage.units);
    normalize(&mut coverage.bindings);
    normalize(&mut coverage.consumers);
    normalize(&mut coverage.recovery_references);
    warnings.sort();
    warnings.dedup();
    Ok(StoreMap {
        schema_version: 1,
        collected_at_unix_ms: epoch(SystemTime::now()).unwrap_or(0),
        host_identifier: inputs.host_identifier.clone(),
        collector_build: inputs.collector_build.clone(),
        scan_roots: roots,
        required_additional_roots: additional.into_iter().collect(),
        config_path: inputs.config_path.clone(),
        config_digest: inputs.config_digest.clone(),
        coverage,
        stores,
        bindings,
        complete: warnings.is_empty(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    fn inputs(root: &Path) -> InventoryInputs {
        InventoryInputs {
            roots: vec![root.to_path_buf()],
            ..Default::default()
        }
    }
    #[test]
    fn suffix_and_backend_use_production_resolution() {
        let root = tempfile::tempdir().unwrap();
        let mut input = inputs(root.path());
        for (name, env) in [
            ("forced.fls", Some("redb")),
            ("mapped.redb", None),
            ("native.redb", Some("redb")),
        ] {
            input.bindings.push(BindingInput::config(
                "workspace",
                root.path().join(name),
                env.map(str::to_owned),
                "fixture",
            ));
        }
        let map = collect_inventory(&input).unwrap();
        assert_eq!(map.bindings[0].selected_backend, "ferro");
        assert!(
            map.bindings
                .iter()
                .any(|e| e.physical_path.ends_with("mapped.fls"))
        );
        assert!(
            map.bindings
                .iter()
                .any(|e| e.physical_path.ends_with("native.redb"))
        );
        assert!(!map.complete);
        assert!(!root.path().join("mapped.fls").exists());
    }
    #[test]
    fn aliases_history_missing_roots_and_permissions_are_not_silently_complete() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("one.fls");
        std::fs::write(&file, b"not a database").unwrap();
        std::fs::hard_link(&file, root.path().join("two.fls")).unwrap();
        symlink("one.fls", root.path().join("alias.fls")).unwrap();
        std::fs::create_dir(root.path().join("archive")).unwrap();
        std::fs::write(root.path().join("archive/old.fls"), b"history").unwrap();
        std::fs::write(root.path().join("old.redb"), b"legacy").unwrap();
        let map = collect_inventory(&inputs(root.path())).unwrap();
        assert!(!map.complete);
        assert_eq!(map.stores.len(), 3);
        let aliased = map
            .stores
            .iter()
            .find(|s| s.identity.given_paths.contains(&file))
            .unwrap();
        assert_eq!(aliased.identity.given_paths.len(), 3);
        assert_eq!(aliased.metadata.hardlink_count, Some(2));
        assert!(!aliased.identity.symlink_chain.is_empty());
        assert!(
            map.stores
                .iter()
                .any(|s| s.classification == "historical_candidate")
        );
        let hidden = root.path().join("hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o0)).unwrap();
        let hidden_map = collect_inventory(&inputs(root.path())).unwrap();
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!hidden_map.complete);
        assert!(hidden_map.warnings.iter().any(|w| w.contains("unreadable")));
    }
    #[test]
    fn derived_sidecars_keep_their_consumer_and_rebuild_reference() {
        let root = tempfile::tempdir().unwrap();
        let primary = root.path().join("primary.fls");
        let sidecar = root.path().join("primary.code.fls");
        let mut input = inputs(root.path());
        input
            .bindings
            .push(BindingInput::config("workspace", primary, None, "fixture"));
        input.consumers.push(ConsumerEdge {
            caller: "code index".into(),
            scope: "workspace".into(),
            source_symbol: "CodeSidecar".into(),
            ..Default::default()
        });
        input
            .deployment
            .recovery_references
            .push(RecoveryReferences {
                store_path: sidecar.clone(),
                index_rebuild_config: Some("config:fixture:index".into()),
                ..Default::default()
            });
        let map = collect_inventory(&input).unwrap();
        let item = map
            .stores
            .iter()
            .find(|s| s.identity.given_paths.contains(&sidecar))
            .unwrap();
        assert_eq!(item.classification, "derived");
        assert!(item.consumers.iter().any(|c| c.caller == "code index"));
        assert_eq!(
            item.recovery[0].index_rebuild_config.as_deref(),
            Some("config:fixture:index")
        );
        assert!(
            map.bindings
                .iter()
                .any(|e| e.caller == "code index" && e.physical_path == sidecar)
        );
    }
    #[test]
    fn hub_refusal_and_fallback_are_distinct() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".local/share/fl")).unwrap();
        assert_eq!(
            hub_lookup(root.path(), "fl", None).unwrap().refusal_status,
            Some(400)
        );
        assert_eq!(
            hub_lookup(root.path(), "../escape", None)
                .unwrap()
                .refusal_status,
            Some(400)
        );
        assert!(hub_lookup(root.path(), "missing", None).unwrap().fallback);
        std::fs::write(root.path().join(".local/share/fl/legacy.redb"), b"not a db").unwrap();
        assert_eq!(
            hub_lookup(root.path(), "legacy", None)
                .unwrap()
                .refusal_status,
            Some(409)
        );
        symlink(
            "legacy.redb",
            root.path().join(".local/share/fl/linked.fls"),
        )
        .unwrap();
        assert_eq!(
            hub_lookup(root.path(), "linked", None)
                .unwrap()
                .refusal_status,
            Some(400)
        );
    }
    #[test]
    fn deployment_observations_do_not_hide_missing_roots_or_unverified_units() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = outside.path().join("owned.fls");
        fs::write(&store, b"fixture").unwrap();
        let unit_file = root.path().join("store.service");
        fs::write(
            &unit_file,
            format!("[Service]\nExecStart=/bin/echo {}\n", store.display()),
        )
        .unwrap();
        let mut input = inputs(root.path());
        let observed = Observation {
            source: "fixture operator".into(),
            observed_at: "2026-11-09T00:00:00Z".into(),
            value: "complete".into(),
        };
        input.deployment.external_coverage = Some(observed.clone());
        input.deployment.unit_coverage = Some(observed);
        input.deployment.units.push(UnitInput {
            name: "fixture".into(),
            file: unit_file,
            store_path: store.clone(),
            declared_mode: Some("managed".into()),
            ..Default::default()
        });
        let map = collect_inventory(&input).unwrap();
        assert!(!map.complete);
        assert!(
            map.required_additional_roots
                .contains(&outside.path().to_path_buf())
        );
        let item = map
            .stores
            .iter()
            .find(|s| s.identity.given_paths.contains(&store))
            .unwrap();
        assert!(!item.ownership.units[0].file_binding_verified);
        assert_eq!(item.ownership.observed_access_mode, None);
        assert_eq!(item.ownership.desired_mode, None);
        assert!(item.ownership.declared_modes.iter().any(|m| m == "managed"));
    }
    #[test]
    fn invalid_hub_slug_keeps_a_refused_edge_without_statting_traversal() {
        let root = tempfile::tempdir().unwrap();
        let mut input = inputs(root.path());
        input.home = Some(root.path().to_path_buf());
        input.bindings.push(BindingInput {
            caller: "hub".into(),
            scope: "invalid".into(),
            logical_path: root.path().join("placeholder.redb"),
            hub_slug: Some("../escape".into()),
            ..Default::default()
        });
        let map = collect_inventory(&input).unwrap();
        assert_eq!(map.bindings.len(), 1);
        assert_eq!(map.bindings[0].refusal_status, Some(400));
        assert!(map.stores.is_empty());
    }
    #[test]
    fn artifact_reads_reject_store_extensions_and_aliases() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("one.fls");
        std::fs::write(&source, b"secret contents").unwrap();
        assert!(read_metadata_artifact(&source, &[]).is_err());
        let link = root.path().join("unit.service");
        symlink(&source, &link).unwrap();
        assert!(read_metadata_artifact(&link, &[]).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::hard_link(&source, &link).unwrap();
        assert!(read_metadata_artifact(&link, &[]).is_err());
        let config = root.path().join("config.toml");
        std::fs::write(&config, b"[state]").unwrap();
        assert_eq!(read_metadata_artifact(&config, &[]).unwrap(), b"[state]");
    }
}
