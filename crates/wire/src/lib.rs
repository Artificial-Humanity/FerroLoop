//! FerroWire local Unix-socket daemon, RFC 6455 WebSocket/TLS peer transport,
//! sender identity resolution, and native vendor push dispatcher (spec §3).

pub mod daemon;
pub mod dispatcher;
pub mod identity;
pub mod peer;
pub mod ws;
