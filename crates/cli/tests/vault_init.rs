#[allow(dead_code)]
mod common;

use assert_cmd::Command;
use predicates::str::contains;

/// `fl` with a private vault and master key, so no test touches the developer's own. HOME and the
/// XDG directories are private too: `fl` reads `$XDG_CONFIG_HOME/fl/config.toml` (a real
/// developer's project bindings) before it opens any store.
fn fl(dir: &std::path::Path) -> Command {
    let mut c = Command::from_std(common::isolated_command(env!("CARGO_BIN_EXE_fl"), dir).unwrap());
    c.env("FL_VAULT_PATH", dir.join("vault.enc"))
        .env("FL_VAULT_KEY_PATH", dir.join("vault.key"))
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env_remove("FL_DB")
        .env_remove("FL_VAULT_MASTER_KEY")
        .current_dir(dir);
    c
}

#[test]
fn vault_init_is_idempotent_and_keeps_existing_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let first = fl(dir.path()).args(["vault", "init"]).output().unwrap();
    assert!(first.status.success(), "{first:?}");
    assert!(dir.path().join("vault.enc").exists());

    fl(dir.path())
        .args(["vault", "set", "KEPT", "--value", "kept-value"])
        .assert()
        .success();
    let again = fl(dir.path()).args(["vault", "init"]).output().unwrap();
    assert!(again.status.success(), "{again:?}");
    assert_eq!(first.stdout, again.stdout, "init output is unchanged");

    fl(dir.path())
        .args(["vault", "list"])
        .assert()
        .success()
        .stdout(contains("KEPT"));
}

#[test]
fn vault_command_uses_private_fixture_paths() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = fl(dir.path());
    let env = |name: &str| {
        cmd.get_envs()
            .find_map(|(key, value)| (key == name).then_some(value).flatten())
    };
    assert_eq!(env("HOME"), Some(dir.path().as_os_str()));
    assert_eq!(env("XDG_RUNTIME_DIR"), Some(dir.path().as_os_str()));
    assert_eq!(
        env("FL_WIRE_SOCKET"),
        Some(dir.path().join("w").as_os_str())
    );
    assert_eq!(
        env("FL_VAULT_PATH"),
        Some(dir.path().join("vault.enc").as_os_str())
    );
    assert_eq!(
        env("FL_VAULT_KEY_PATH"),
        Some(dir.path().join("vault.key").as_os_str())
    );
    assert_eq!(env("PATH"), Some(dir.path().join("no-bin").as_os_str()));
}
