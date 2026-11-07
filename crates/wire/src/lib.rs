//! FerroWire local Unix-socket daemon, RFC 6455 WebSocket/TLS peer transport,
//! sender identity resolution, and native vendor push dispatcher (spec §3).

mod codex_control;
pub mod daemon;
pub mod dispatch_diagnostics;
pub mod dispatcher;
pub mod identity;
pub mod peer;
pub mod scanner;
pub mod ws;
