//! FerroWire local Unix-socket daemon, RFC 6455 WebSocket/TLS peer transport,
//! sender identity resolution, and native vendor push dispatcher (spec §3).

#[cfg(feature = "wire-test-isolation")]
#[doc(hidden)]
pub mod test_isolation;

pub mod claude_registration;
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
        let _fixture = crate::test_isolation::register_unix_socket(&path).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn wire_test_binary_rejects_direct_unregistered_host_clients() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("host.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let endpoint = fl_host_proto::HostEndpoint::unix(&path).unwrap();
        let err = fl_host_proto::BlockingHostClient::connect(&endpoint)
            .err()
            .expect("unregistered host refused");
        assert!(err.to_string().contains("unregistered wire test socket"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(fl_host_proto::AsyncHostClient::connect_with(
                &endpoint,
                fl_host_proto::ConnectOptions::default(),
            ))
            .unwrap_err();
        assert!(err.to_string().contains("unregistered wire test socket"));
    }

    #[cfg(unix)]
    #[test]
    fn wire_listeners_refuse_external_bind_before_filesystem_changes() {
        let external = tempfile::Builder::new().tempdir_in("/var/tmp").unwrap();
        let parent = external.path().join("uncreated");
        let host_socket = parent.join("host.sock");
        let endpoint = fl_host_proto::HostEndpoint::unix(&host_socket).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            runtime
                .block_on(fl_host_proto::HostListener::bind(
                    fl_host_proto::HostConfig::new(endpoint)
                ))
                .is_err()
        );
        assert!(
            !parent.exists(),
            "host bind created an external parent or sidecar"
        );
        assert!(
            fl_store::FerroStoreServer::bind_unix(
                parent.join("store.sock"),
                fl_store::FerroStore::in_memory()
            )
            .is_err()
        );
        assert!(
            !parent.exists(),
            "store bind created an external parent or socket"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wire_private_store_listener_remains_a_usable_fixture() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("store.sock");
        let mut server =
            fl_store::FerroStoreServer::bind_unix(&path, fl_store::FerroStore::in_memory())
                .unwrap();
        let _fixture = crate::test_isolation::register_unix_socket(&path).unwrap();
        assert!(fl_store::test_isolation::validate_unix(&path).is_ok());
        server.shutdown().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn wire_host_listener_refuses_unregistered_socket_probe() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let path = dir.path().join("host.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let endpoint = fl_host_proto::HostEndpoint::unix(&path).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(fl_host_proto::HostListener::bind(
            fl_host_proto::HostConfig::new(endpoint),
        ));
        assert!(result.is_err());
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn wire_host_validation_rechecks_after_pre_connect_hook() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let path = dir.path().join("host.sock");
        let endpoint = fl_host_proto::HostEndpoint::unix(&path).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener =
                fl_host_proto::HostListener::bind(fl_host_proto::HostConfig::new(endpoint.clone()))
                    .await
                    .unwrap();
            let _fixture = crate::test_isolation::register_unix_socket(&path).unwrap();
            let replacement = path.clone();
            let options = fl_host_proto::ConnectOptions {
                pre_connect: Some(std::sync::Arc::new(move || {
                    std::fs::remove_file(&replacement).unwrap();
                    std::os::unix::fs::symlink(
                        "/run/user/1000/ferroloop/fl-host.sock",
                        &replacement,
                    )
                    .unwrap();
                })),
                ..Default::default()
            };
            let error = fl_host_proto::AsyncHostClient::connect_with(&endpoint, options)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("unregistered wire test"));
            listener.shutdown().await.unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn wire_unix_registration_rejects_unregistered_replaced_and_symlinked_sockets() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(crate::test_isolation::ensure_socket(&path).is_err());
        assert!(
            crate::daemon::LocalClient::connect(&path, std::time::Duration::from_millis(50))
                .is_err()
        );
        let guard = crate::test_isolation::register_unix_socket(&path).unwrap();
        assert!(crate::test_isolation::ensure_socket(&path).is_ok());
        std::fs::remove_file(&path).unwrap();
        let replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(crate::test_isolation::ensure_socket(&path).is_err());
        assert!(
            crate::daemon::LocalClient::connect(&path, std::time::Duration::from_millis(50))
                .is_err()
        );
        drop(replacement);
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/run/user/1000/ferroloop/fl-host.sock", &path).unwrap();
        assert!(crate::test_isolation::register_unix_socket(&path).is_err());
        assert!(crate::test_isolation::ensure_socket(&path).is_err());
        drop(guard);
    }

    #[cfg(unix)]
    #[test]
    fn wire_executable_registration_is_exact_and_rejects_symlinks() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fake-vendor");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(crate::test_isolation::ensure_executable(&path).is_err());
        let fixture = crate::test_isolation::register_executable(&path).unwrap();
        assert!(crate::test_isolation::ensure_executable(&path).is_ok());
        let link = dir.path().join("symlink");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(crate::test_isolation::register_executable(&link).is_err());
        drop(fixture);
        assert!(crate::test_isolation::ensure_executable(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn wire_store_connections_refuse_external_and_allow_private_auto_fallback() {
        let private = tempfile::TempDir::new().unwrap();
        let private_store = private.path().join("private.fls");
        let _store = fl_store::FerroStore::auto(&private_store)
            .expect("private absent socket falls back to embedded store");
        assert!(private_store.exists());
        let listener_path = private.path().join("unregistered-store.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&listener_path).unwrap();
        assert!(
            fl_store::FerroStore::connect_uri(&format!("unix://{}", listener_path.display()))
                .is_err()
        );
        let fixture = crate::test_isolation::register_unix_socket(&listener_path).unwrap();
        assert!(fl_store::test_isolation::validate_unix(&listener_path).is_ok());
        drop(fixture);
        assert!(fl_store::test_isolation::validate_unix(&listener_path).is_err());
        assert!(
            fl_store::FerroStore::connect_uri("unix:///run/user/1000/ferroloop/store.sock")
                .is_err()
        );
        assert!(fl_store::FerroStore::connect_uri("ws://localhost:9999").is_err());
        let auth = fl_store::ServerAuth::new()
            .with_psk("fixture", &[7; 32])
            .unwrap();
        assert!(
            fl_store::FerroStoreServer::bind_tcp(
                "localhost:0",
                fl_store::FerroStore::in_memory(),
                auth
            )
            .is_err()
        );
        let external = tempfile::Builder::new().tempdir_in("/var/tmp").unwrap();
        let external_store = external.path().join("external.fls");
        assert!(fl_store::FerroStore::auto(&external_store).is_err());
        assert!(!external_store.exists());
    }

    #[test]
    fn wire_tcp_fixture_registration_is_scoped() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        assert!(crate::test_isolation::ensure_tcp(&address).is_err());
        assert!(fl_store::test_isolation::validate_tcp(&address.to_string()).is_err());
        let guard = crate::test_isolation::register_tcp_listener(&listener).unwrap();
        assert!(crate::test_isolation::ensure_tcp(&address).is_ok());
        assert!(fl_store::test_isolation::validate_tcp(&address.to_string()).is_ok());
        drop(guard);
        assert!(crate::test_isolation::ensure_tcp(&address).is_err());
        assert!(fl_store::test_isolation::validate_tcp(&address.to_string()).is_err());
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
