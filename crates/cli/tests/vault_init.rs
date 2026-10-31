use assert_cmd::Command;
use predicates::str::contains;

/// `fl` with a private vault and master key, so no test touches the developer's own.
fn fl(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("fl").unwrap();
    c.env("FL_VAULT_PATH", dir.join("vault.enc"))
        .env("FL_VAULT_KEY_PATH", dir.join("vault.key"))
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
