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
use std::sync::{Arc, Mutex, OnceLock, RwLock};

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
///
/// Trust model:
/// - Registering a scanner needs code running in this process, so a scanner is trusted code. It
///   is trusted not to forge routes for other vendors' agents; wire cannot tell whose agent a
///   name belongs to.
/// - Discovery calls [`discover`](Self::discover) synchronously, one scanner after another, while
///   the caller holds the route table. A scanner must return quickly and must not block on the
///   network or on a lock the caller may hold.
/// - A scanner only ever ADDS routes for names the table does not hold, and refreshes a route it
///   produced itself (the entry must still equal what this scanner last produced for the name).
///   It never replaces a route that an agent registered, a built-in scan found, or another scanner
///   produced.
/// - A route a scanner produced stays in the table when the scanner stops returning it: stale
///   routes are not pruned. Which routes a scanner produced is remembered for the life of the
///   process only (it is not written to disk).
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

/// How many recent routes are remembered per agent name.
const PRODUCED_PER_NAME: usize = 8;

/// The routes scanners recently put in a route table: agent name to (scanner vendor id, route),
/// oldest first. Process-global and never persisted; only [`scanner_may_write`] and
/// [`record_produced`] use it. It holds a few routes per name, not one, because a process can
/// hold several route tables (every table's discovery records here) and a scanner's route in one
/// of them may be older than its latest route in another.
fn produced() -> &'static Mutex<BTreeMap<String, Vec<(String, VendorRoute)>>> {
    static PRODUCED: OnceLock<Mutex<BTreeMap<String, Vec<(String, VendorRoute)>>>> =
        OnceLock::new();
    PRODUCED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Whether scanner `vendor_id` may write a route for `name`, given the route `existing` that the
/// table holds for it: only when the table holds none, or holds exactly a route this same
/// scanner produced for the name (see [`record_produced`]). A route that an agent, a built-in
/// scan or another scanner put there is never replaced.
pub(crate) fn scanner_may_write(
    vendor_id: &str,
    name: &str,
    existing: Option<&VendorRoute>,
) -> bool {
    let Some(existing) = existing else {
        return true;
    };
    produced()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .is_some_and(|list| list.iter().any(|(id, r)| id == vendor_id && r == existing))
}

/// Remember that scanner `vendor_id` put `route` in a table for `name`.
pub(crate) fn record_produced(vendor_id: &str, name: &str, route: &VendorRoute) {
    let mut map = produced().lock().unwrap_or_else(|e| e.into_inner());
    let list = map.entry(name.to_string()).or_default();
    list.retain(|(id, r)| !(id == vendor_id && r == route));
    list.push((vendor_id.to_string(), route.clone()));
    if list.len() > PRODUCED_PER_NAME {
        list.remove(0);
    }
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

    /// A scanner whose routes a test changes between passes.
    struct Switch {
        id: String,
        routes: Mutex<BTreeMap<String, VendorRoute>>,
    }

    impl Switch {
        fn new(id: &str, routes: &[(&str, VendorRoute)]) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                routes: Mutex::new(
                    routes
                        .iter()
                        .map(|(n, r)| (n.to_string(), r.clone()))
                        .collect(),
                ),
            })
        }
        fn set(&self, name: &str, route: VendorRoute) {
            self.routes.lock().unwrap().insert(name.to_string(), route);
        }
    }

    impl WireVendorScanner for Switch {
        fn vendor_id(&self) -> &str {
            &self.id
        }
        fn discover(&self, _home: &Path) -> BTreeMap<String, VendorRoute> {
            self.routes.lock().unwrap().clone()
        }
    }

    fn pass(table: &mut RouteTable, home: &Path, ms: u64) {
        table.discover_routes(Some(home), At::from_unix_millis(ms));
    }

    #[test]
    fn a_scanner_can_refresh_its_own_webhook_route() {
        let home = tempfile::TempDir::new().unwrap();
        let scanner = Switch::new(
            "wscan-refresh",
            &[(
                "wscan-refresh-agent",
                webhook("/run/wscan-refresh-old.sock"),
            )],
        );
        register_wire_scanner(scanner.clone()).unwrap();
        let mut table = RouteTable::default();
        pass(&mut table, home.path(), 1);
        assert_eq!(
            route_of(&table, "wscan-refresh-agent"),
            Some(&webhook("/run/wscan-refresh-old.sock"))
        );
        // The scanner now reports another socket: it replaces the route it produced.
        scanner.set(
            "wscan-refresh-agent",
            webhook("/run/wscan-refresh-new.sock"),
        );
        pass(&mut table, home.path(), 2);
        assert_eq!(
            route_of(&table, "wscan-refresh-agent"),
            Some(&webhook("/run/wscan-refresh-new.sock"))
        );
    }

    #[test]
    fn a_scanner_cannot_replace_a_webhook_route_from_another_source() {
        let home = tempfile::TempDir::new().unwrap();
        register_wire_scanner(Fixed::new(
            "wscan-forge",
            &[("wscan-forge-agent", webhook("/run/wscan-forge.sock"))],
        ))
        .unwrap();
        // An agent registered its own webhook route before the scan.
        let mut table = RouteTable::default();
        table.upsert(
            "wscan-forge-agent",
            webhook("/run/wscan-forge-own.sock"),
            At::from_unix_millis(1),
        );
        pass(&mut table, home.path(), 2);
        assert_eq!(
            route_of(&table, "wscan-forge-agent"),
            Some(&webhook("/run/wscan-forge-own.sock"))
        );
        // Nor does it take the name on a later pass.
        pass(&mut table, home.path(), 3);
        assert_eq!(
            route_of(&table, "wscan-forge-agent"),
            Some(&webhook("/run/wscan-forge-own.sock"))
        );
    }

    #[test]
    fn a_route_the_scanner_did_not_produce_is_not_its_own_even_when_it_was_replaced() {
        let home = tempfile::TempDir::new().unwrap();
        let scanner = Switch::new(
            "wscan-swap",
            &[("wscan-swap-agent", webhook("/run/wscan-swap-a.sock"))],
        );
        register_wire_scanner(scanner.clone()).unwrap();
        let mut table = RouteTable::default();
        pass(&mut table, home.path(), 1);
        // The agent registers itself over the scanner's route; the scanner then changes its own.
        table.upsert(
            "wscan-swap-agent",
            webhook("/run/wscan-swap-agent.sock"),
            At::from_unix_millis(2),
        );
        scanner.set("wscan-swap-agent", webhook("/run/wscan-swap-b.sock"));
        pass(&mut table, home.path(), 3);
        assert_eq!(
            route_of(&table, "wscan-swap-agent"),
            Some(&webhook("/run/wscan-swap-agent.sock"))
        );
    }

    #[test]
    fn two_scanners_cannot_take_each_others_names() {
        let home = tempfile::TempDir::new().unwrap();
        let first = Switch::new(
            "wscan-take-first",
            &[("wscan-take-agent", webhook("/run/wscan-take-first.sock"))],
        );
        let second = Switch::new("wscan-take-second", &[]);
        register_wire_scanner(first.clone()).unwrap();
        register_wire_scanner(second.clone()).unwrap();
        let mut table = RouteTable::default();
        pass(&mut table, home.path(), 1);
        // The second scanner starts reporting the first one's name, on later passes too.
        second.set("wscan-take-agent", webhook("/run/wscan-take-second.sock"));
        pass(&mut table, home.path(), 2);
        pass(&mut table, home.path(), 3);
        assert_eq!(
            route_of(&table, "wscan-take-agent"),
            Some(&webhook("/run/wscan-take-first.sock"))
        );
        // The first one still refreshes it.
        first.set("wscan-take-agent", webhook("/run/wscan-take-first-2.sock"));
        pass(&mut table, home.path(), 4);
        assert_eq!(
            route_of(&table, "wscan-take-agent"),
            Some(&webhook("/run/wscan-take-first-2.sock"))
        );
        // And the second one cannot take it after that either.
        pass(&mut table, home.path(), 5);
        assert_eq!(
            route_of(&table, "wscan-take-agent"),
            Some(&webhook("/run/wscan-take-first-2.sock"))
        );
    }

    #[test]
    fn a_scanner_does_not_replace_a_newer_route_in_the_table() {
        let home = tempfile::TempDir::new().unwrap();
        let scanner = Switch::new(
            "wscan-fresh",
            &[("wscan-fresh-agent", webhook("/run/wscan-fresh-a.sock"))],
        );
        register_wire_scanner(scanner.clone()).unwrap();
        let mut table = RouteTable::default();
        pass(&mut table, home.path(), 1_000);
        scanner.set("wscan-fresh-agent", webhook("/run/wscan-fresh-b.sock"));
        // A pass stamped before the route's own time keeps the route the table has.
        pass(&mut table, home.path(), 500);
        assert_eq!(
            route_of(&table, "wscan-fresh-agent"),
            Some(&webhook("/run/wscan-fresh-a.sock"))
        );
        pass(&mut table, home.path(), 2_000);
        assert_eq!(
            route_of(&table, "wscan-fresh-agent"),
            Some(&webhook("/run/wscan-fresh-b.sock"))
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
