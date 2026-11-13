//! FerroWire local Unix-socket daemon, RFC 6455 WebSocket/TLS peer transport,
//! sender identity resolution, and native vendor push dispatcher (spec §3).

#[cfg(feature = "wire-test-isolation")]
#[doc(hidden)]
pub mod test_isolation;

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

// Activate the shared host test guard before libtest starts; dependencies do not
// inherit cfg(test), and the binary may be renamed.
#[cfg(all(
    test,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ))
))]
compile_error!("fl-wire tests require a pre-main host isolation constructor on this platform");

#[cfg(test)]
mod isolate_host_before_tests {
    extern "C" fn enable() {
        crate::test_isolation::enable();
    }
    #[used]
    #[cfg_attr(
        target_vendor = "apple",
        unsafe(link_section = "__DATA,__mod_init_func")
    )]
    #[cfg_attr(
        all(unix, not(target_vendor = "apple")),
        unsafe(link_section = ".init_array")
    )]
    #[cfg_attr(windows, unsafe(link_section = ".CRT$XCU"))]
    static INITIALIZE: extern "C" fn() = enable;
}

#[cfg(test)]
mod host_isolation_tests {
    #[cfg(unix)]
    #[test]
    fn wire_test_binary_rejects_unregistered_wire_socket() {
        let dir = tempfile::Builder::new().tempdir_in("/var/tmp").unwrap();
        let path = dir.path().join("unregistered.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let err = match crate::daemon::LocalClient::connect(
            &path,
            std::time::Duration::from_millis(50),
        ) {
            Ok(_) => panic!("unregistered socket connected"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn wire_owned_unix_fixture_works_and_symlinks_escape_are_denied() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("owned.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(
            crate::daemon::LocalClient::connect(&path, std::time::Duration::from_millis(50))
                .is_ok()
        );
        drop(listener);
        let link = dir.path().join("escape.sock");
        std::os::unix::fs::symlink("/run/user/1000/ferroloop/fl-wire.sock", &link).unwrap();
        let err = match crate::daemon::LocalClient::connect(
            &link,
            std::time::Duration::from_millis(50),
        ) {
            Ok(_) => panic!("symlink connected"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn wire_tcp_fixture_registration_is_scoped() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        assert!(crate::test_isolation::ensure_tcp(&address).is_err());
        let guard = crate::test_isolation::register_tcp_listener(&listener).unwrap();
        assert!(crate::test_isolation::ensure_tcp(&address).is_ok());
        drop(guard);
        assert!(crate::test_isolation::ensure_tcp(&address).is_err());
    }

    #[test]
    fn wire_test_binary_rejects_unregistered_tcp_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let err = match crate::ws::WsStream::connect(
            &format!("ws://{address}/"),
            std::time::Duration::from_millis(50),
        ) {
            Ok(_) => panic!("unregistered listener connected"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("unregistered wire test TCP fixture"),
            "{err}"
        );
    }

    #[test]
    fn wire_test_binary_does_not_resolve_real_tmux_by_default() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host_isolation_tests::wire_tmux_child",
                "--ignored",
            ])
            .env_remove("FL_WIRE_TMUX_BIN")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    #[ignore]
    fn wire_tmux_child() {
        assert_ne!(
            crate::dispatcher::tmux_bin(),
            std::path::PathBuf::from("tmux")
        );
    }

    #[test]
    fn wire_test_binary_rejects_unregistered_host_discovery() {
        const CHILD: &str = "FL_WIRE_HOST_ISOLATION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert!(fl_host_proto::HostEndpoint::discover().is_err());
            return;
        }
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host_isolation_tests::wire_test_binary_rejects_unregistered_host_discovery",
            ])
            .env(CHILD, "1")
            .env_remove("FL_HOST_SOCK")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
