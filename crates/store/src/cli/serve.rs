//! `serve`: one store served on a Unix socket, or on TCP as `ws://` or
//! `wss://`, under the authentication and bind policy of
//! [`FerroStoreServer`] and [`ServerAuth`] (spec §6), until the process gets
//! SIGINT or SIGTERM.
//!
//! Shutdown: before anything is opened, the calling thread blocks SIGINT and
//! SIGTERM (`pthread_sigmask`), so every thread the store and the server
//! start inherits the mask and no default handler kills the process part way
//! through a write. Once the server listens, the thread waits for one of the
//! two (`sigwait`), then drops the server (it stops accepting, closes its
//! connections and removes its socket) and the store, and returns.
//!
//! Nothing secret is printed: the report names files and the methods in
//! force, never a key, a token or a hash.

use super::data::open_local;
use super::{Error, Result, Wording, line};
use crate::{
    BackendKind, FerroStoreServer, ServerAuth, default_socket_path_for_store, physical_path,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Where and how to serve. The fields map one to one onto
/// [`FerroStoreServer::bind_unix_with`], [`FerroStoreServer::bind_tcp`] and
/// [`ServerAuth`].
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// The Unix socket; default `<store>.fls.sock`. Not with `tcp`.
    pub socket: Option<PathBuf>,
    /// Serve TCP on this address (`127.0.0.1:7878`) instead of a socket.
    pub tcp: Option<String>,
    /// The PSK file (64+ hex digits, mode 0600), with `server_id`.
    pub psk_file: Option<PathBuf>,
    /// The name the server proves itself under with its PSK.
    pub server_id: Option<String>,
    /// The bearer token file (sha256, scope and label per line). TCP only.
    pub token_file: Option<PathBuf>,
    /// The TLS certificate chain (PEM, leaf first), with `tls_key`. TCP only.
    pub tls_cert: Option<PathBuf>,
    /// The TLS private key (PEM, mode 0600). TCP only.
    pub tls_key: Option<PathBuf>,
    /// Interfaces whose own addresses plain `ws://` may bind. TCP only.
    pub allow_plaintext_on: Vec<String>,
}

/// What `serve` is serving, reported once the server listens.
#[derive(Debug, Clone, Serialize)]
pub struct Serving {
    /// The `.fls` file served.
    #[serde(serialize_with = "super::shown")]
    pub store: PathBuf,
    /// The URI a client connects to: `unix://<socket>`, `ws://<addr>` or
    /// `wss://<addr>`.
    pub uri: String,
    /// The checks a connection passes: `peer-uid`, `psk`, `token`.
    pub auth: Vec<&'static str>,
    pub tls: bool,
    /// Interfaces plain `ws://` may bind, as given.
    pub allow_plaintext_on: Vec<String>,
}

impl Serving {
    /// One line: what is served, where, under which checks.
    pub fn text(&self) -> String {
        let mut out = String::new();
        line(
            &mut out,
            format!(
                "serving {} on {} (auth: {})",
                self.store.display(),
                self.uri,
                self.auth.join(" + ")
            ),
        );
        out
    }

    /// The report as one line of JSON (keys sorted, as every report's), for
    /// a reader that waits for a line.
    pub fn json_line(&self) -> Result<String> {
        let value = serde_json::to_value(self).map_err(Error::Json)?;
        let mut text = serde_json::to_string(&value).map_err(Error::Json)?;
        text.push('\n');
        Ok(text)
    }
}

/// The checks of `options` against each other, before anything is opened.
fn check(options: &ServeOptions) -> Result<()> {
    let tcp_only = options.token_file.is_some()
        || options.tls_cert.is_some()
        || options.tls_key.is_some()
        || !options.allow_plaintext_on.is_empty();
    if options.tcp.is_some() && options.socket.is_some() {
        refuse!("--socket and --tcp are two places to serve: give one");
    }
    if options.tcp.is_none() && tcp_only {
        refuse!(
            "--token-file, --tls-cert, --tls-key and --allow-plaintext-on are for --tcp; a Unix \
             socket checks the peer uid and, with --psk-file, the PSK"
        );
    }
    if options.tls_cert.is_some() != options.tls_key.is_some() {
        refuse!("--tls-cert and --tls-key go together");
    }
    if options.psk_file.is_some() != options.server_id.is_some() {
        refuse!("--psk-file and --server-id go together");
    }
    if options.tcp.is_some() && options.psk_file.is_none() && options.token_file.is_none() {
        refuse!("--tcp needs authentication: --psk-file with --server-id, --token-file, or both");
    }
    Ok(())
}

fn server_auth(options: &ServeOptions) -> Result<ServerAuth> {
    let mut auth = ServerAuth::new();
    if let (Some(psk), Some(id)) = (&options.psk_file, &options.server_id) {
        auth = auth.with_psk_file(id, psk)?;
    }
    if let Some(tokens) = &options.token_file {
        auth = auth.with_token_file(tokens)?;
    }
    if let (Some(cert), Some(key)) = (&options.tls_cert, &options.tls_key) {
        auth = auth.with_tls_files(cert, key)?;
    }
    if !options.allow_plaintext_on.is_empty() {
        auth = auth.allow_plaintext_on(options.allow_plaintext_on.iter().cloned());
    }
    Ok(auth)
}

/// Serve the store `path` names (its `.fls`, created 0600 when nothing is
/// there; a lone `.redb` is refused) under `options`. `ready` is called once
/// the server listens; then this waits for SIGINT or SIGTERM, shuts the
/// server down, and returns the report with the signal's name.
pub fn serve(
    path: &Path,
    options: &ServeOptions,
    w: &Wording,
    ready: impl FnOnce(&Serving) -> Result<()>,
) -> Result<(Serving, &'static str)> {
    check(options)?;
    // Before the store or the server starts a thread.
    let signals = Signals::block()?;
    let auth = server_auth(options)?;
    let store = open_local(path, true, "serve", w)?;
    let physical = physical_path(path, BackendKind::Ferro);
    let mut methods = Vec::new();
    let (server, uri, tls) = match &options.tcp {
        Some(addr) => {
            let tls = options.tls_cert.is_some();
            let server = FerroStoreServer::bind_tcp(addr.as_str(), store, auth)?;
            let scheme = if tls { "wss" } else { "ws" };
            let uri = format!("{scheme}://{}", server.local_addr());
            (server, uri, tls)
        }
        None => {
            let socket = options
                .socket
                .clone()
                .unwrap_or_else(|| default_socket_path_for_store(&physical));
            let server = FerroStoreServer::bind_unix_with(&socket, store, auth)?;
            methods.push("peer-uid");
            (server, format!("unix://{}", socket.display()), false)
        }
    };
    if options.psk_file.is_some() {
        methods.push("psk");
    }
    if options.token_file.is_some() {
        methods.push("token");
    }
    let serving = Serving {
        store: physical,
        uri,
        auth: methods,
        tls,
        allow_plaintext_on: options.allow_plaintext_on.clone(),
    };
    ready(&serving)?;
    let signal = signals.wait()?;
    drop(server);
    Ok((serving, signal))
}

/// SIGINT and SIGTERM, blocked in the calling thread (and so in every thread
/// it starts) until dropped, when the previous mask comes back.
struct Signals {
    set: libc::sigset_t,
    previous: libc::sigset_t,
}

impl Signals {
    fn block() -> Result<Self> {
        // SAFETY: `sigemptyset` and `sigaddset` initialise and fill the set
        // they are given; `pthread_sigmask` reads `set` and writes `previous`,
        // both valid for the call.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            let mut previous: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, libc::SIGINT);
            libc::sigaddset(&raw mut set, libc::SIGTERM);
            let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, &raw mut previous);
            if rc != 0 {
                return Err(Error::context(
                    "could not block SIGINT and SIGTERM",
                    std::io::Error::from_raw_os_error(rc),
                ));
            }
            Ok(Self { set, previous })
        }
    }

    /// Wait for SIGINT or SIGTERM; its name.
    fn wait(&self) -> Result<&'static str> {
        loop {
            let mut signal: libc::c_int = 0;
            // SAFETY: `self.set` was initialised in `block`; `signal` is
            // valid for the call.
            let rc = unsafe { libc::sigwait(&raw const self.set, &raw mut signal) };
            if rc == libc::EINTR {
                continue;
            }
            if rc != 0 {
                return Err(Error::context(
                    "could not wait for SIGINT or SIGTERM",
                    std::io::Error::from_raw_os_error(rc),
                ));
            }
            return Ok(match signal {
                libc::SIGINT => "SIGINT",
                _ => "SIGTERM",
            });
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        // SAFETY: `previous` is the mask `block` read.
        unsafe {
            libc::pthread_sigmask(
                libc::SIG_SETMASK,
                &raw const self.previous,
                std::ptr::null_mut(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_that_cannot_go_together_are_refused_before_anything_opens() {
        let tcp = || ServeOptions {
            tcp: Some("127.0.0.1:0".into()),
            ..ServeOptions::default()
        };
        let refused = |o: ServeOptions| check(&o).unwrap_err().to_string();
        assert!(refused(tcp()).contains("needs authentication"));
        assert!(
            refused(ServeOptions {
                socket: Some("/s".into()),
                ..tcp()
            })
            .contains("give one")
        );
        assert!(
            refused(ServeOptions {
                token_file: Some("/t".into()),
                ..ServeOptions::default()
            })
            .contains("are for --tcp")
        );
        assert!(
            refused(ServeOptions {
                tls_cert: Some("/c".into()),
                token_file: Some("/t".into()),
                ..tcp()
            })
            .contains("go together")
        );
        assert!(
            refused(ServeOptions {
                psk_file: Some("/k".into()),
                ..ServeOptions::default()
            })
            .contains("--server-id")
        );
        assert!(check(&ServeOptions::default()).is_ok());
        assert!(
            check(&ServeOptions {
                token_file: Some("/t".into()),
                ..tcp()
            })
            .is_ok()
        );
    }
}
