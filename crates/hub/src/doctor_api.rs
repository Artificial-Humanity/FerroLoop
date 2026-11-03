//! `GET /api/v1/doctor`: the observational doctor (#205 A3) for this hub process.
//!
//! **Owner only.** The answer names filesystem paths and permissions, so a Peer gets a 403 and an
//! unauthenticated caller a 401. The body is the [`fl_core::doctor::DoctorReport`]:
//! `{ "schema_version", "overall", "summary", "checks": [ { "id", "title", "status", "detail" } ] }`
//! with the six `G7.*` checks in a fixed order and no timestamp.
//!
//! The hub reports on its own context: its build stamp (`fl-hub`), the install receipt under
//! `$FL_INSTALL_ROOT` or the home directory, its configured store (verified read-only through
//! [`fl_store::health`]: never migrated, no write lock, no wait beyond about 300 ms, and not
//! behind the hub's `store_lock`, so other requests never queue behind it), the vault files'
//! modes (contents never read) and the UI bundle it serves. Nothing is written, no key is
//! generated, nothing is started, and no network call is made. A request runs on the HTTP
//! worker thread that already runs blocking handlers; the path hashing is capped at
//! [`PATH_BUDGET`].

use std::path::PathBuf;
use std::time::Duration;

use fl_core::doctor::{StoreFact, StoreFacts};
use fl_core::install::resolve_layout;
use fl_install::{Observe, UiDir, observe};
use serde_json::{Value, json};

use crate::auth::HubAuthority;
use crate::{HttpResponse, HubState};

/// The most time the receipt's paths may take to hash.
const PATH_BUDGET: Duration = Duration::from_secs(5);

/// The report for this hub, as JSON.
pub(crate) fn report(state: &HubState) -> Result<Value, String> {
    let home = state.config.home_override.clone().or_else(|| {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
    });
    let layout =
        resolve_layout(None, std::env::var_os("FL_INSTALL_ROOT")).map_err(|e| e.to_string())?;
    let store = &state.config.store_path;
    let stores = StoreFacts {
        stores: vec![StoreFact {
            path: store.display().to_string(),
            outcome: fl_store::health::verify_read_only(store, state.store_backend),
        }],
        notes: Vec::new(),
    };
    let home_override = state.config.home_override.as_deref();
    let report = observe(Observe {
        binary: crate::version_api::build_info(),
        binary_name: "fl-hub",
        home,
        layout,
        ui_dir: state
            .config
            .ui_dir
            .clone()
            .map_or(UiDir::NotServed, UiDir::Explicit),
        vault_path: fl_gate::vault::resolve_vault_path(home_override),
        key_path: fl_gate::vault::resolve_master_key_path(home_override),
        key_in_env: std::env::var_os("FL_VAULT_MASTER_KEY").is_some_and(|v| !v.is_empty()),
        stores,
        budget: PATH_BUDGET,
    });
    serde_json::to_value(&report).map_err(|e| e.to_string())
}

/// `GET /api/v1/doctor`.
pub(crate) fn handle_get(state: &HubState, authority: &HubAuthority) -> HttpResponse {
    if !authority.is_owner() {
        return HttpResponse::json(403, json!({ "error": "owner authority required" }));
    }
    match report(state) {
        Ok(v) => HttpResponse::json(200, v),
        Err(e) => HttpResponse::json(500, json!({ "error": format!("doctor failed: {e}") })),
    }
}

#[cfg(test)]
#[path = "doctor_api_tests.rs"]
mod tests;
