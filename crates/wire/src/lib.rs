//! FerroWire local Unix-socket daemon, RFC 6455 WebSocket/TLS peer transport,
//! sender identity resolution, and native vendor push dispatcher (spec §3).

mod codex_control;
pub mod daemon;
pub mod dispatch_diagnostics;
pub mod dispatcher;
pub mod host_runtime;
pub mod identity;
pub mod peer;
pub mod scanner;
pub mod vendor_adapter;
pub mod ws;

pub use dispatcher::is_composer_occupied_zero_keys_error;
