//! Pluggable route discovery for custom vendor plugins.
//!
//! [`RouteTable::discover_routes`](crate::identity::RouteTable::discover_routes) scans the
//! built-in vendors' own session files. A plugin vendor (one added with
//! `fl_core::register_vendor`) contributes routes through a [`WireVendorScanner`] registered
//! with [`register_wire_scanner`]. The registry is process-global.
//!
//! A scanner can only produce the generic [`VendorRoute::Webhook`] kind, so delivery to its
//! routes goes through the existing webhook push in `dispatcher`.

use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::sync::{Arc, OnceLock, RwLock};

use fl_core::vendor::VendorRegistry;

use crate::identity::VendorRoute;

/// Ids that name a vendor wire already discovers itself, in the spellings wire uses. The
/// vendor registry's own built-in ids are refused too (see [`register_wire_scanner`]).
const WIRE_BUILTIN_IDS: &[&str] = &[
    "claude",
    "agy",
    "antigravity",
    "codex",
    "muse",
    "grok",
    "webhook",
];

/// A vendor plugin's source of routes.
pub trait WireVendorScanner: Send + Sync {
    /// The vendor id this scanner serves. Read once, at registration.
    fn vendor_id(&self) -> &str;

    /// The routes this vendor has right now, keyed by agent (persona) name. `home` is the home
    /// directory discovery runs against. Only [`VendorRoute::Webhook`] routes are accepted; any
    /// other kind is dropped.
    fn discover(&self, home: &Path) -> BTreeMap<String, VendorRoute>;
}

/// Why [`register_wire_scanner`] refused a scanner.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterScannerError {
    #[error("scanner vendor id is empty")]
    EmptyId,
    #[error("`{0}` is a built-in vendor; its routes are discovered by wire itself")]
    BuiltinVendor(String),
    #[error("a scanner for vendor `{0}` is already registered")]
    Duplicate(String),
}

struct Registered {
    id: String,
    scanner: Arc<dyn WireVendorScanner>,
}

fn registry() -> &'static RwLock<Vec<Registered>> {
    static REGISTRY: OnceLock<RwLock<Vec<Registered>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Add `scanner` to the process-global registry. Ids compare without case. Refuses an empty id,
/// a built-in vendor id and an id that is already registered.
///
/// The vendor need not be in `fl_core`'s vendor registry.
pub fn register_wire_scanner(
    scanner: Arc<dyn WireVendorScanner>,
) -> Result<(), RegisterScannerError> {
    let id = scanner.vendor_id().trim().to_ascii_lowercase();
    if id.is_empty() {
        return Err(RegisterScannerError::EmptyId);
    }
    if WIRE_BUILTIN_IDS.contains(&id.as_str()) || VendorRegistry::is_builtin(&id) {
        return Err(RegisterScannerError::BuiltinVendor(id));
    }
    let mut scanners = registry().write().unwrap_or_else(|e| e.into_inner());
    if scanners.iter().any(|r| r.id == id) {
        return Err(RegisterScannerError::Duplicate(id));
    }
    scanners.push(Registered { id, scanner });
    Ok(())
}

/// The registered scanners in registration order, cloned out so no lock is held while one runs.
pub(crate) fn registered_scanners() -> Vec<(String, Arc<dyn WireVendorScanner>)> {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|r| (r.id.clone(), Arc::clone(&r.scanner)))
        .collect()
}

/// Accept a scanner's route only when it is a webhook route with a plain absolute socket path.
pub(crate) fn scanner_route_is_valid(route: &VendorRoute) -> bool {
    let VendorRoute::Webhook { socket_path } = route else {
        return false;
    };
    socket_path.is_absolute()
        && !socket_path.as_os_str().is_empty()
        && !socket_path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        && socket_path.file_name().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::RouteTable;
    use fl_core::at::At;
    use std::path::PathBuf;

    /// A scanner that returns a fixed set of routes, or panics.
    struct Fixed {
        id: String,
        routes: BTreeMap<String, VendorRoute>,
        panics: bool,
    }

    impl Fixed {
        fn new(id: &str, routes: &[(&str, VendorRoute)]) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                routes: routes
                    .iter()
                    .map(|(n, r)| (n.to_string(), r.clone()))
                    .collect(),
                panics: false,
            })
        }
    }

    impl WireVendorScanner for Fixed {
        fn vendor_id(&self) -> &str {
            &self.id
        }
        fn discover(&self, _home: &Path) -> BTreeMap<String, VendorRoute> {
            assert!(!self.panics, "scanner blew up");
            self.routes.clone()
        }
    }

    fn webhook(path: &str) -> VendorRoute {
        VendorRoute::Webhook {
            socket_path: PathBuf::from(path),
        }
    }

    fn discover(home: &Path) -> RouteTable {
        let mut table = RouteTable::default();
        table.discover_routes(Some(home), At::from_unix_millis(1_000));
        table
    }

    fn route_of<'a>(table: &'a RouteTable, name: &str) -> Option<&'a VendorRoute> {
        table.get(name).map(|e| &e.route)
    }

    #[test]
    fn scanner_route_appears_in_discover_routes() {
        let home = tempfile::TempDir::new().unwrap();
        register_wire_scanner(Fixed::new(
            "wscan-appear",
            &[("wscan-appear-agent", webhook("/run/wscan-appear.sock"))],
        ))
        .unwrap();
        let table = discover(home.path());
        assert_eq!(
            route_of(&table, "wscan-appear-agent"),
            Some(&webhook("/run/wscan-appear.sock"))
        );
    }

    #[test]
    fn two_scanners_with_different_ids_both_merge() {
        let home = tempfile::TempDir::new().unwrap();
        register_wire_scanner(Fixed::new(
            "wscan-two-a",
            &[("wscan-two-agent-a", webhook("/run/wscan-two-a.sock"))],
        ))
        .unwrap();
        register_wire_scanner(Fixed::new(
            "wscan-two-b",
            &[("wscan-two-agent-b", webhook("/run/wscan-two-b.sock"))],
        ))
        .unwrap();
        let table = discover(home.path());
        assert_eq!(
            route_of(&table, "wscan-two-agent-a"),
            Some(&webhook("/run/wscan-two-a.sock"))
        );
        assert_eq!(
            route_of(&table, "wscan-two-agent-b"),
            Some(&webhook("/run/wscan-two-b.sock"))
        );
    }

    #[test]
    fn builtin_scan_wins_over_a_scanner_for_the_same_name() {
        let home = tempfile::TempDir::new().unwrap();
        let codex_dir = home.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            "{\"id\":\"thread-wscan\",\"agentName\":\"wscan-builtin-agent\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        register_wire_scanner(Fixed::new(
            "wscan-shadow",
            &[("wscan-builtin-agent", webhook("/run/wscan-shadow.sock"))],
        ))
        .unwrap();
        let table = discover(home.path());
        assert!(matches!(
            route_of(&table, "wscan-builtin-agent"),
            Some(VendorRoute::Codex { thread_id, .. }) if thread_id == "thread-wscan"
        ));
    }

    #[test]
    fn an_earlier_scanner_wins_over_a_later_one() {
        let home = tempfile::TempDir::new().unwrap();
        register_wire_scanner(Fixed::new(
            "wscan-order-first",
            &[("wscan-order-agent", webhook("/run/wscan-order-first.sock"))],
        ))
        .unwrap();
        register_wire_scanner(Fixed::new(
            "wscan-order-second",
            &[("wscan-order-agent", webhook("/run/wscan-order-second.sock"))],
        ))
        .unwrap();
        let table = discover(home.path());
        assert_eq!(
            route_of(&table, "wscan-order-agent"),
            Some(&webhook("/run/wscan-order-first.sock"))
        );
    }

    #[test]
    fn a_scanner_can_refresh_its_own_webhook_route() {
        let home = tempfile::TempDir::new().unwrap();
        register_wire_scanner(Fixed::new(
            "wscan-refresh",
            &[(
                "wscan-refresh-agent",
                webhook("/run/wscan-refresh-new.sock"),
            )],
        ))
        .unwrap();
        let mut table = RouteTable::default();
        table.upsert(
            "wscan-refresh-agent",
            webhook("/run/wscan-refresh-old.sock"),
            At::from_unix_millis(1),
        );
        table.discover_routes(Some(home.path()), At::from_unix_millis(2));
        assert_eq!(
            route_of(&table, "wscan-refresh-agent"),
            Some(&webhook("/run/wscan-refresh-new.sock"))
        );
    }

    #[test]
    fn a_panicking_scanner_is_skipped_and_builtin_routes_still_come_back() {
        let home = tempfile::TempDir::new().unwrap();
        let codex_dir = home.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            "{\"id\":\"thread-wscan-panic\",\"agentName\":\"wscan-panic-builtin\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        register_wire_scanner(Arc::new(Fixed {
            id: "wscan-panic".to_string(),
            routes: BTreeMap::new(),
            panics: true,
        }))
        .unwrap();
        register_wire_scanner(Fixed::new(
            "wscan-panic-after",
            &[(
                "wscan-panic-after-agent",
                webhook("/run/wscan-panic-after.sock"),
            )],
        ))
        .unwrap();
        let table = discover(home.path());
        assert!(matches!(
            route_of(&table, "wscan-panic-builtin"),
            Some(VendorRoute::Codex { .. })
        ));
        // A scanner registered after the panicking one still runs.
        assert_eq!(
            route_of(&table, "wscan-panic-after-agent"),
            Some(&webhook("/run/wscan-panic-after.sock"))
        );
    }

    #[test]
    fn invalid_scanner_routes_are_dropped() {
        let home = tempfile::TempDir::new().unwrap();
        register_wire_scanner(Fixed::new(
            "wscan-invalid",
            &[
                ("wscan-invalid-relative", webhook("run/x.sock")),
                ("wscan-invalid-dotdot", webhook("/run/../etc/x.sock")),
                (
                    "wscan-invalid-kind",
                    VendorRoute::Codex {
                        thread_id: "t".into(),
                        cwd: None,
                    },
                ),
                ("Not A Slug!", webhook("/run/wscan-invalid-slug.sock")),
                ("wscan-invalid-ok", webhook("/run/wscan-invalid-ok.sock")),
            ],
        ))
        .unwrap();
        let table = discover(home.path());
        assert!(route_of(&table, "wscan-invalid-relative").is_none());
        assert!(route_of(&table, "wscan-invalid-dotdot").is_none());
        assert!(route_of(&table, "wscan-invalid-kind").is_none());
        assert!(
            table
                .entries
                .values()
                .all(|e| e.route != webhook("/run/wscan-invalid-slug.sock"))
        );
        assert_eq!(
            route_of(&table, "wscan-invalid-ok"),
            Some(&webhook("/run/wscan-invalid-ok.sock"))
        );
    }

    #[test]
    fn duplicate_and_builtin_ids_are_refused() {
        register_wire_scanner(Fixed::new("wscan-dup", &[])).unwrap();
        assert_eq!(
            register_wire_scanner(Fixed::new("WSCAN-Dup", &[])),
            Err(RegisterScannerError::Duplicate("wscan-dup".into()))
        );
        for id in [
            "claude",
            "codex",
            "agy",
            "antigravity",
            "muse",
            "grok",
            "Claude",
            "ollama",
            "lemonade",
            "openrouter-api",
            "gemini-api",
        ] {
            assert!(
                matches!(
                    register_wire_scanner(Fixed::new(id, &[])),
                    Err(RegisterScannerError::BuiltinVendor(_))
                ),
                "{id} must be refused"
            );
        }
        assert_eq!(
            register_wire_scanner(Fixed::new("  ", &[])),
            Err(RegisterScannerError::EmptyId)
        );
    }
}
