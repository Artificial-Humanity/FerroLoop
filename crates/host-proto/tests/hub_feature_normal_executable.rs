//! Feature unification must not disable host discovery in other executables.
#![cfg(all(feature = "hub-test-isolation", unix))]

#[test]
fn unified_feature_keeps_normal_executable_discovery() {
    const CHILD: &str = "FL_HUB_FEATURE_NORMAL_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let configured = std::env::var_os("FL_HOST_SOCK").unwrap();
        assert_eq!(
            fl_host_proto::HostEndpoint::discover()
                .unwrap()
                .socket_path(),
            std::path::Path::new(&configured)
        );
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "unified_feature_keeps_normal_executable_discovery",
        ])
        .env(CHILD, "1")
        .env("FL_HOST_SOCK", tmp.path().join("mock.sock"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
