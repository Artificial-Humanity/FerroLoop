//! Pure description of potential state paths; this module authorizes no I/O.
//!
//! The result describes conditional possibilities, not proofs of existence,
//! ownership or safety. Callers must retain and validate every applicable
//! resource, refuse unmodeled operations, and guard actual I/O separately.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unmodeled {
    MigrationTemporaryFiles,
    WorkspaceScan,
    RouteWriteTemporaryFiles,
}

#[derive(Debug)]
struct RouteInput<'a> {
    selected: &'a Path,
    default: &'a Path,
    explicit: Option<&'a Path>,
    legacy: Option<&'a Path>,
    discovery_enabled: bool,
}

#[derive(Debug)]
struct StateAccessPlan {
    logical: PathBuf,
    backend: fl_store::BackendKind,
    physical: PathBuf,
    redb_metadata_sibling: Option<PathBuf>,
    served_socket: Option<PathBuf>,
    migration_lock: Option<PathBuf>,
    fallback_store: Option<PathBuf>,
    route: PathBuf,
    legacy_route_if_primary_absent: Option<PathBuf>,
    unmodeled: Vec<Unmodeled>,
}

impl StateAccessPlan {
    fn derive(
        logical: &Path,
        selected_backend: Option<fl_store::BackendKind>,
        backend_env: Option<&str>,
        routes: RouteInput<'_>,
    ) -> Result<Self, String> {
        fn absolute(path: &Path) -> bool {
            path.is_absolute()
                && path.file_name().is_some()
                && !path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
        }
        if !absolute(logical)
            || !absolute(routes.selected)
            || !absolute(routes.default)
            || routes
                .explicit
                .is_some_and(|path| !absolute(path) || path != routes.selected)
            || routes.legacy.is_some_and(|path| !absolute(path))
        {
            return Err("state and route paths must be absolute file paths without parent traversal, and explicit routes must match the selected route".into());
        }
        let backend = match selected_backend {
            Some(kind) => kind,
            None => {
                fl_store::choose_backend(logical, backend_env).map_err(|error| error.to_string())?
            }
        };
        let physical = fl_store::physical_path(logical, backend);
        let ferro = backend == fl_store::BackendKind::Ferro;
        let migration_possible = ferro && physical != logical;
        let redb_metadata_sibling = (!ferro)
            .then(|| logical.with_extension("fls"))
            .filter(|path| path != logical);
        let eligible_legacy = routes.discovery_enabled
            && (routes.explicit.is_none() || routes.explicit == Some(routes.default));
        let mut unmodeled = vec![
            Unmodeled::WorkspaceScan,
            Unmodeled::RouteWriteTemporaryFiles,
        ];
        if migration_possible {
            unmodeled.push(Unmodeled::MigrationTemporaryFiles);
        }
        Ok(Self {
            logical: logical.to_path_buf(),
            backend,
            physical: physical.clone(),
            redb_metadata_sibling,
            served_socket: ferro.then(|| fl_store::default_socket_path_for_store(&physical)),
            migration_lock: migration_possible
                .then(|| fl_store::MigrationLock::path_for(&physical)),
            fallback_store: migration_possible.then(|| logical.to_path_buf()),
            route: routes.selected.to_path_buf(),
            legacy_route_if_primary_absent: if eligible_legacy {
                routes.legacy.map(Path::to_path_buf)
            } else {
                None
            },
            unmodeled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn absolute(path: &str) -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(format!("C:\\fixture\\{path}"))
        }
        #[cfg(not(windows))]
        {
            PathBuf::from(format!("/fixture/{path}"))
        }
    }
    fn route_input<'a>(
        selected: &'a Path,
        default: &'a Path,
        explicit: Option<&'a Path>,
        legacy: Option<&'a Path>,
    ) -> RouteInput<'a> {
        RouteInput {
            selected,
            default,
            explicit,
            legacy,
            discovery_enabled: true,
        }
    }
    #[test]
    fn plans_all_possible_ferro_accesses_without_opening_anything() {
        let db = absolute("x.redb");
        let routes = absolute("routes.json");
        let legacy = absolute("old/routes.json");
        let plan = StateAccessPlan::derive(
            &db,
            None,
            None,
            route_input(&routes, &routes, Some(&routes), Some(&legacy)),
        )
        .unwrap();
        assert_eq!(plan.logical, db);
        assert_eq!(plan.backend, fl_store::BackendKind::Ferro);
        assert_eq!(plan.physical, absolute("x.fls"));
        assert_eq!(plan.served_socket, Some(absolute("x.fls.sock")));
        assert_eq!(plan.migration_lock, Some(absolute("x.fls.migrate.lock")));
        assert_eq!(plan.fallback_store, Some(absolute("x.redb")));
        assert_eq!(plan.legacy_route_if_primary_absent, Some(legacy));
        assert!(plan.unmodeled.contains(&Unmodeled::MigrationTemporaryFiles));
        assert!(plan.unmodeled.contains(&Unmodeled::WorkspaceScan));
        assert!(
            plan.unmodeled
                .contains(&Unmodeled::RouteWriteTemporaryFiles)
        );
    }
    #[test]
    fn backend_precedence_and_distinct_route_fallback() {
        let route = absolute("custom.json");
        let default = absolute("default.json");
        let legacy = absolute("old.json");
        for (file, env, backend, physical) in [
            ("x.fls", Some("redb"), fl_store::BackendKind::Ferro, "x.fls"),
            (
                "x.redb",
                Some("redb"),
                fl_store::BackendKind::Redb,
                "x.redb",
            ),
            ("plain", None, fl_store::BackendKind::Ferro, "plain.fls"),
        ] {
            let p = StateAccessPlan::derive(
                &absolute(file),
                None,
                env,
                route_input(&route, &default, Some(&route), Some(&legacy)),
            )
            .unwrap();
            assert_eq!(p.backend, backend);
            assert_eq!(p.physical, absolute(physical));
            assert_eq!(p.route, route);
            assert_eq!(p.legacy_route_if_primary_absent, None);
            if backend == fl_store::BackendKind::Redb {
                assert_eq!(p.redb_metadata_sibling, Some(absolute("x.fls")));
                assert!(p.served_socket.is_none());
            }
        }
    }
    #[test]
    fn rejects_relative_paths_and_invalid_backend() {
        let route = absolute("route");
        let db = absolute("x.redb");
        assert!(
            StateAccessPlan::derive(
                Path::new("relative"),
                None,
                None,
                route_input(&route, &route, None, None)
            )
            .is_err()
        );
        assert!(
            StateAccessPlan::derive(
                &db,
                None,
                Some("unknown"),
                route_input(&route, &route, None, None)
            )
            .is_err()
        );
        assert!(
            StateAccessPlan::derive(
                &db,
                None,
                None,
                route_input(Path::new("relative"), &route, None, None)
            )
            .is_err()
        );
        assert!(
            StateAccessPlan::derive(
                &db,
                None,
                None,
                route_input(&route, &route, Some(&absolute("other")), None)
            )
            .is_err()
        );
        assert!(
            StateAccessPlan::derive(
                &absolute("a/../x.redb"),
                None,
                None,
                route_input(&route, &route, None, None)
            )
            .is_err()
        );
    }
    #[test]
    fn explicit_backend_and_route_fallback_inputs_are_retained() {
        let db = absolute("data.fls");
        let selected = absolute("selected.json");
        let default = absolute("default.json");
        let legacy = absolute("legacy.json");
        // StoreOptions.backend takes precedence even over the .fls extension.
        let plan = StateAccessPlan::derive(
            &db,
            Some(fl_store::BackendKind::Redb),
            Some("ferro"),
            route_input(&selected, &default, None, Some(&legacy)),
        )
        .unwrap();
        assert_eq!(plan.backend, fl_store::BackendKind::Redb);
        assert_eq!(plan.physical, db);
        assert_eq!(plan.redb_metadata_sibling, None);
        assert_eq!(plan.legacy_route_if_primary_absent, Some(legacy));
        let legacy_again = absolute("legacy.json");
        let mut routes = route_input(&selected, &default, None, Some(&legacy_again));
        routes.discovery_enabled = false;
        let plan = StateAccessPlan::derive(&db, None, None, routes).unwrap();
        assert!(plan.legacy_route_if_primary_absent.is_none());
        assert!(plan.migration_lock.is_none());
        assert!(plan.fallback_store.is_none());
    }
}
