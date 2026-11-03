//! What the hub's accept loop does with an `accept(2)` error: keep going, back off, or stop.
//!
//! A hub that stops serving on the first transient error (a full file-descriptor table, a client
//! that reset before it was accepted) takes every session with it, so only an error that proves
//! the listener itself is broken ends [`crate::HubServer::serve`].

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};

/// The first pause after a failed accept; each further consecutive failure doubles it.
pub(crate) const ACCEPT_BACKOFF_INITIAL: Duration = Duration::from_millis(50);
/// The longest pause between two accept attempts after failures.
pub(crate) const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);
/// Accept errors of an unrecognised kind, counted since the last successful accept, after which
/// the hub gives up and returns the last one, so a listener that is dead in a way this module
/// does not know still surfaces. A recognised transient error neither adds to the count nor
/// resets it: fd exhaustion can last as long as the load does.
pub(crate) const ACCEPT_UNKNOWN_ERROR_LIMIT: u32 = 64;
/// The least time between two log lines about failing accepts; the lines in between are counted
/// into the next one.
pub(crate) const ACCEPT_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// How the accept loop treats one `accept(2)` error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceptErrorKind {
    /// The listener is fine; the call failed for a reason that passes (resource exhaustion, a
    /// connection that died in the queue, an interrupted call). Back off and accept again.
    Transient,
    /// The listener itself is unusable. Accepting again can only fail again.
    Fatal,
    /// Not recognised. Treated as transient until [`ACCEPT_UNKNOWN_ERROR_LIMIT`] of them have
    /// accumulated since the last successful accept.
    Unknown,
}

/// Transient errno values that exist on some platforms only. `ENONET` is Linux's; macOS and the
/// BSDs have no such constant in `libc`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn is_platform_transient(code: i32) -> bool {
    code == libc::ENONET
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn is_platform_transient(_code: i32) -> bool {
    false
}

/// Classify an error from `accept(2)` (see `man 2 accept`, ERRORS). The hub is unix-only, so the
/// table uses unix errno values; on Windows `raw_os_error` returns WSA codes and every error
/// would classify as `Unknown`.
pub(crate) fn classify_accept_error(err: &io::Error) -> AcceptErrorKind {
    if let Some(code) = err.raw_os_error() {
        return match code {
            // Out of descriptors or memory: the process or system, not the listener.
            libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM => {
                AcceptErrorKind::Transient
            }
            // A connection that died before it was accepted, or an interrupted call.
            libc::ECONNABORTED | libc::ECONNRESET | libc::EINTR | libc::EAGAIN | libc::EPROTO => {
                AcceptErrorKind::Transient
            }
            // The man page: network errors pending on the new socket "should be treated like
            // EAGAIN by retrying".
            libc::ENETDOWN
            | libc::ENOPROTOOPT
            | libc::EHOSTDOWN
            | libc::EHOSTUNREACH
            | libc::EOPNOTSUPP
            | libc::ENETUNREACH
            | libc::ETIMEDOUT => AcceptErrorKind::Transient,
            code if is_platform_transient(code) => AcceptErrorKind::Transient,
            // The descriptor is not a listening socket: nothing a retry can fix.
            libc::EBADF | libc::EINVAL | libc::ENOTSOCK | libc::EFAULT => AcceptErrorKind::Fatal,
            _ => AcceptErrorKind::Unknown,
        };
    }
    match err.kind() {
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::Interrupted
        | io::ErrorKind::WouldBlock
        | io::ErrorKind::TimedOut
        | io::ErrorKind::OutOfMemory => AcceptErrorKind::Transient,
        _ => AcceptErrorKind::Unknown,
    }
}

/// What the loop does after one `accept` call.
#[derive(Debug)]
pub(crate) enum AcceptAction {
    /// A connection: hand it to the connection gate.
    Serve(TcpStream, SocketAddr),
    /// Pause `delay` before accepting again. `log` is the line to print, if the rate limit lets
    /// one through.
    Retry {
        delay: Duration,
        log: Option<String>,
    },
    /// Stop serving and return this error.
    Stop(io::Error),
}

/// Backoff, the bound on unrecognised errors and the log rate limit for one accept loop. The
/// clock is a parameter, so tests step it by hand.
#[derive(Debug, Clone)]
pub(crate) struct AcceptGuard {
    initial: Duration,
    max: Duration,
    unknown_limit: u32,
    log_interval: Duration,
    /// Failures since the last success.
    streak: u32,
    /// Unrecognised failures since the last success.
    unknown_streak: u32,
    last_log: Option<Instant>,
    /// Failures not yet reported by a log line.
    unreported: u64,
}

impl Default for AcceptGuard {
    fn default() -> Self {
        Self::new(
            ACCEPT_BACKOFF_INITIAL,
            ACCEPT_BACKOFF_MAX,
            ACCEPT_UNKNOWN_ERROR_LIMIT,
            ACCEPT_LOG_INTERVAL,
        )
    }
}

impl AcceptGuard {
    pub(crate) fn new(
        initial: Duration,
        max: Duration,
        unknown_limit: u32,
        log_interval: Duration,
    ) -> Self {
        Self {
            initial,
            max,
            unknown_limit,
            log_interval,
            streak: 0,
            unknown_streak: 0,
            last_log: None,
            unreported: 0,
        }
    }

    /// The pause after the `streak`-th failure in a row: `initial`, doubled each time, capped.
    fn delay(&self) -> Duration {
        // 2^16 times any sane `initial` is far past `max`; the cap keeps the shift in range.
        let doublings = self.streak.saturating_sub(1).min(16);
        self.initial.saturating_mul(1u32 << doublings).min(self.max)
    }

    /// Decide what the loop does with the result of one `accept`.
    pub(crate) fn on_result(
        &mut self,
        result: io::Result<(TcpStream, SocketAddr)>,
        now: Instant,
    ) -> AcceptAction {
        let err = match result {
            Ok((stream, peer)) => {
                self.streak = 0;
                self.unknown_streak = 0;
                return AcceptAction::Serve(stream, peer);
            }
            Err(err) => err,
        };
        let kind = classify_accept_error(&err);
        if kind == AcceptErrorKind::Fatal {
            return AcceptAction::Stop(err);
        }
        self.streak = self.streak.saturating_add(1);
        if kind == AcceptErrorKind::Unknown {
            self.unknown_streak += 1;
            if self.unknown_streak >= self.unknown_limit {
                return AcceptAction::Stop(err);
            }
        }
        let delay = self.delay();
        self.unreported += 1;
        let due = self
            .last_log
            .is_none_or(|at| now.saturating_duration_since(at) >= self.log_interval);
        let log = due.then(|| {
            let repeats = self.unreported - 1;
            self.last_log = Some(now);
            self.unreported = 0;
            let more = if repeats > 0 {
                format!(" ({repeats} more since the last report)")
            } else {
                String::new()
            };
            format!(
                "fl-hub: accept failed, retrying in {} ms: {err}{more}",
                delay.as_millis()
            )
        });
        AcceptAction::Retry { delay, log }
    }
}

/// Where the accept loop gets connections: a [`TcpListener`], or a test double that fails on cue.
pub(crate) trait Acceptor: Send {
    fn accept(&mut self) -> impl Future<Output = io::Result<(TcpStream, SocketAddr)>> + Send;
}

impl Acceptor for TcpListener {
    fn accept(&mut self) -> impl Future<Output = io::Result<(TcpStream, SocketAddr)>> + Send {
        TcpListener::accept(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HubServer, HubServerConfig};
    use fl_core::pr_policy::ReviewerApps;
    use std::collections::VecDeque;
    use std::io::{Read, Write};

    fn os(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // ---- the classifier ----------------------------------------------------------------

    #[test]
    fn resource_exhaustion_and_dead_connections_are_transient() {
        for code in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::ENOMEM,
            libc::ECONNABORTED,
            libc::ECONNRESET,
            libc::EINTR,
            libc::EAGAIN,
            libc::EWOULDBLOCK,
            libc::EPROTO,
            libc::ENETDOWN,
            libc::EHOSTUNREACH,
            libc::ETIMEDOUT,
        ] {
            assert_eq!(
                classify_accept_error(&os(code)),
                AcceptErrorKind::Transient,
                "errno {code}"
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_linux_only_network_error_is_transient() {
        assert_eq!(
            classify_accept_error(&os(libc::ENONET)),
            AcceptErrorKind::Transient
        );
    }

    #[test]
    fn a_broken_listener_is_fatal() {
        for code in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK, libc::EFAULT] {
            assert_eq!(
                classify_accept_error(&os(code)),
                AcceptErrorKind::Fatal,
                "errno {code}"
            );
        }
    }

    #[test]
    fn an_unrecognised_error_is_unknown() {
        assert_eq!(
            classify_accept_error(&os(libc::EIO)),
            AcceptErrorKind::Unknown
        );
        assert_eq!(
            classify_accept_error(&io::Error::other("boom")),
            AcceptErrorKind::Unknown
        );
    }

    #[test]
    fn an_error_without_an_errno_is_classified_by_kind() {
        for kind in [
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::Interrupted,
            io::ErrorKind::WouldBlock,
        ] {
            assert_eq!(
                classify_accept_error(&io::Error::from(kind)),
                AcceptErrorKind::Transient,
                "{kind:?}"
            );
        }
    }

    // ---- the guard ---------------------------------------------------------------------

    fn retry(action: AcceptAction) -> (Duration, Option<String>) {
        match action {
            AcceptAction::Retry { delay, log } => (delay, log),
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[test]
    fn the_delay_doubles_from_50_ms_and_stops_at_one_second() {
        let mut guard = AcceptGuard::default();
        let now = Instant::now();
        let delays: Vec<Duration> = (0..9)
            .map(|_| retry(guard.on_result(Err(os(libc::EMFILE)), now)).0)
            .collect();
        assert_eq!(
            delays,
            [50, 100, 200, 400, 800, 1000, 1000, 1000, 1000].map(ms)
        );
    }

    #[test]
    fn a_success_resets_the_delay() {
        let mut guard = AcceptGuard::default();
        let now = Instant::now();
        for _ in 0..5 {
            guard.on_result(Err(os(libc::EMFILE)), now);
        }
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = std_listener.local_addr().unwrap();
        let _client = std::net::TcpStream::connect(addr).unwrap();
        let (server_side, peer) = std_listener.accept().unwrap();
        server_side.set_nonblocking(true).unwrap();
        // `from_std` needs a runtime; a stream needs no more than registration.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let stream = rt.block_on(async { TcpStream::from_std(server_side).unwrap() });
        assert!(matches!(
            guard.on_result(Ok((stream, peer)), now),
            AcceptAction::Serve(..)
        ));
        assert_eq!(retry(guard.on_result(Err(os(libc::EMFILE)), now)).0, ms(50));
    }

    #[test]
    fn a_fatal_error_stops_at_once() {
        let mut guard = AcceptGuard::default();
        match guard.on_result(Err(os(libc::EBADF)), Instant::now()) {
            AcceptAction::Stop(e) => assert_eq!(e.raw_os_error(), Some(libc::EBADF)),
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    #[test]
    fn transient_errors_never_stop_the_loop() {
        let mut guard = AcceptGuard::default();
        let now = Instant::now();
        for _ in 0..(ACCEPT_UNKNOWN_ERROR_LIMIT * 4) {
            assert!(matches!(
                guard.on_result(Err(os(libc::EMFILE)), now),
                AcceptAction::Retry { .. }
            ));
        }
    }

    #[test]
    fn unrecognised_errors_stop_the_loop_after_the_limit() {
        let mut guard = AcceptGuard::default();
        let now = Instant::now();
        for n in 1..ACCEPT_UNKNOWN_ERROR_LIMIT {
            assert!(
                matches!(
                    guard.on_result(Err(os(libc::EIO)), now),
                    AcceptAction::Retry { .. }
                ),
                "failure {n} must still be retried"
            );
        }
        match guard.on_result(Err(os(libc::EIO)), now) {
            AcceptAction::Stop(e) => assert_eq!(e.raw_os_error(), Some(libc::EIO)),
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    #[test]
    fn a_transient_error_in_between_does_not_hide_a_dead_listener_forever() {
        // The unknown streak counts unknown errors only; transient ones neither add nor reset.
        let mut guard = AcceptGuard::new(ms(1), ms(2), 3, ACCEPT_LOG_INTERVAL);
        let now = Instant::now();
        guard.on_result(Err(os(libc::EIO)), now);
        guard.on_result(Err(os(libc::EMFILE)), now);
        guard.on_result(Err(os(libc::EIO)), now);
        assert!(matches!(
            guard.on_result(Err(os(libc::EIO)), now),
            AcceptAction::Stop(_)
        ));
    }

    #[test]
    fn repeated_failures_log_once_per_interval_and_count_the_rest() {
        let mut guard = AcceptGuard::default();
        let t0 = Instant::now();
        let (_, first) = retry(guard.on_result(Err(os(libc::EMFILE)), t0));
        let first = first.expect("the first failure is logged");
        assert!(first.starts_with("fl-hub: accept failed"), "{first}");
        assert!(first.contains("retrying in 50 ms"), "{first}");
        assert!(!first.contains("more since"), "{first}");

        for i in 1..=20 {
            let (_, log) = retry(guard.on_result(Err(os(libc::EMFILE)), t0 + ms(i * 100)));
            assert!(log.is_none(), "failure {i} must stay quiet");
        }
        let (_, again) = retry(guard.on_result(Err(os(libc::EMFILE)), t0 + ACCEPT_LOG_INTERVAL));
        let again = again.expect("a line is due after the interval");
        assert!(again.contains("(20 more since the last report)"), "{again}");
    }

    // ---- the loop ----------------------------------------------------------------------

    /// A real listener that first fails with the queued errors.
    struct Flaky {
        script: VecDeque<io::Error>,
        inner: TcpListener,
    }

    impl Acceptor for Flaky {
        async fn accept(&mut self) -> io::Result<(TcpStream, SocketAddr)> {
            match self.script.pop_front() {
                Some(e) => Err(e),
                None => self.inner.accept().await,
            }
        }
    }

    /// Fails with `EIO` for ever.
    struct Dead;

    impl Acceptor for Dead {
        async fn accept(&mut self) -> io::Result<(TcpStream, SocketAddr)> {
            Err(os(libc::EIO))
        }
    }

    async fn server(tmp: &tempfile::TempDir) -> HubServer {
        HubServer::bind(HubServerConfig {
            listen: "127.0.0.1:0".into(),
            store_path: tmp.path().join("store.redb"),
            repo_root: tmp.path().to_path_buf(),
            ui_dir: None,
            owner_login: "lmcfarlin".into(),
            reviewer_apps: ReviewerApps::default(),
            owner_token: Some("owner-tok-secret".into()),
            peer_token: None,
            trust_tailscale_headers: false,
            enable_terminal: false,
            session_ttl_secs: 3600,
            home_override: Some(tmp.path().to_path_buf()),
            github: crate::github_pr::HubGithub::default(),
        })
        .await
        .unwrap()
    }

    fn health(addr: SocketAddr) -> String {
        let mut tcp = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3)).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        tcp.write_all(b"GET /api/v1/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut out = Vec::new();
        let _ = tcp.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_hub_keeps_serving_across_transient_accept_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = server(&tmp).await;
        let addr = srv.local_addr();
        let shutdown = srv.shutdown_handle();
        let script = VecDeque::from([os(libc::EMFILE), os(libc::ECONNABORTED), os(libc::ENFILE)]);
        let task = tokio::spawn(
            srv.serve_with(move |inner| Flaky { script, inner }, AcceptGuard::default()),
        );

        // The three errors cost 50 + 100 + 200 ms of backoff before the first accept succeeds;
        // the bound leaves margin under that 350 ms.
        let started = Instant::now();
        let reply = tokio::task::spawn_blocking(move || health(addr))
            .await
            .unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(
            started.elapsed() >= ms(250),
            "the loop did not back off: {:?}",
            started.elapsed()
        );
        assert!(!task.is_finished(), "serve must still be running");

        // And it still honours shutdown.
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shutdown_during_a_backoff_is_honoured() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = server(&tmp).await;
        let shutdown = srv.shutdown_handle();
        // A backoff of a minute: only the shutdown arm can end the task in time.
        let guard = AcceptGuard::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
            ACCEPT_UNKNOWN_ERROR_LIMIT,
            ACCEPT_LOG_INTERVAL,
        );
        let script = VecDeque::from([os(libc::EMFILE)]);
        let task = tokio::spawn(srv.serve_with(move |inner| Flaky { script, inner }, guard));
        tokio::time::sleep(ms(100)).await;
        shutdown.send(true).unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), task).await;
        ended.expect("serve ignored shutdown").unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fatal_accept_error_ends_serve_with_that_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = server(&tmp).await;
        let script = VecDeque::from([os(libc::EBADF)]);
        let task = tokio::spawn(
            srv.serve_with(move |inner| Flaky { script, inner }, AcceptGuard::default()),
        );
        let res = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("serve did not stop")
            .unwrap();
        assert_eq!(res.unwrap_err().raw_os_error(), Some(libc::EBADF));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_listener_of_unknown_kind_ends_serve_after_the_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let srv = server(&tmp).await;
        let guard = AcceptGuard::new(ms(1), ms(2), 5, ACCEPT_LOG_INTERVAL);
        let task = tokio::spawn(srv.serve_with(|_| Dead, guard));
        let res = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("serve did not stop")
            .unwrap();
        assert_eq!(res.unwrap_err().raw_os_error(), Some(libc::EIO));
    }
}
