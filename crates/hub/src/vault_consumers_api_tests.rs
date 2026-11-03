//! `GET /api/v1/gate/vault-consumers`: Owner only, configuration facts and nothing more. Every
//! test works in temporary directories with throwaway keys and a fake environment; the HTTP tests
//! that must read the process environment hold a lock and put it back.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use fl_core::pr_policy::ReviewerApps;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::{HubServerConfig, HubState, handle_http_request};

const OWNER: &str = "owner-tok-secret";
const PEER: &str = "peer-tok-secret";

/// Planted as environment values, file contents and vault values: none may reach an answer.
const CANARY_ENV: &str = "CANARY-ENV-VALUE-5c1e77";
const CANARY_FILE: &str = "CANARY-FILE-CONTENT-a93b02";
const CANARY_VAULT: &str = "CANARY-VAULT-VALUE-d4408f";

fn write(path: &Path, text: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn names(items: &[&str]) -> Option<BTreeSet<String>> {
    Some(items.iter().map(|s| s.to_string()).collect())
}

fn no_tail() -> AuditTail {
    AuditTail {
        state: "absent",
        denied: Vec::new(),
    }
}

/// A fake environment: only the pairs given.
fn fake(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn run(
    config: Option<&VaultConsumerConfig>,
    env: &BTreeMap<String, String>,
    vendors: Vec<VendorKey>,
    key_path: &Path,
    vault_names: Option<BTreeSet<String>>,
    audit: AuditTail,
    tokens: (bool, bool),
) -> Value {
    let lookup = |k: &str| env.get(k).cloned();
    build(&Inputs {
        config,
        owner_token_in_use: tokens.0,
        peer_token_in_use: tokens.1,
        vendors,
        key_path: key_path.to_path_buf(),
        vault_names,
        audit,
        env: &lookup,
    })
}

/// The simple case: a config, an environment, nothing else.
fn report_of(config: Option<&VaultConsumerConfig>, env: &[(&str, &str)]) -> Value {
    run(
        config,
        &fake(env),
        Vec::new(),
        Path::new("/nonexistent-g6c/vault.key"),
        names(&[]),
        no_tail(),
        (false, false),
    )
}

fn consumer<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["consumers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no consumer {id} in {v}"))
}

fn class<'a>(v: &'a Value, id: &str) -> &'a str {
    consumer(v, id)["class"].as_str().unwrap()
}

fn github(file: Option<&Path>, secret: Option<&str>) -> VaultConsumerConfig {
    VaultConsumerConfig {
        github_app: Some(GithubAppKeys {
            private_key_file: file.map(Path::to_path_buf),
            private_key_ref: secret.map(str::to_string),
        }),
        ..Default::default()
    }
}

const KEY: &str = "github-app-private-key";

// --- The GitHub App private key ---------------------------------------------------------------

#[test]
fn a_private_key_file_is_a_plaintext_file_and_the_report_says_whether_it_exists() {
    let tmp = TempDir::new().unwrap();
    let pem = tmp.path().join("app.pem");
    let cfg = github(Some(&pem), None);
    let v = report_of(Some(&cfg), &[]);
    assert_eq!(class(&v, KEY), "plaintext-file");
    assert_eq!(consumer(&v, KEY)["plaintext_path_present"], false);
    write(&pem, CANARY_FILE, 0o600);
    let v = report_of(Some(&cfg), &[]);
    assert_eq!(class(&v, KEY), "plaintext-file");
    assert_eq!(consumer(&v, KEY)["plaintext_path_present"], true);
    assert!(!v.to_string().contains(CANARY_FILE), "file contents leaked");
}

#[test]
fn a_private_key_ref_is_a_vault_ref_that_is_configured_and_nothing_more() {
    let cfg = github(None, Some("GITHUB_APP_KEY"));
    let v = report_of(Some(&cfg), &[]);
    let c = consumer(&v, KEY);
    assert_eq!(c["class"], "vault-ref");
    assert_eq!(c["plaintext_path_present"], false);
    // The secret is not in the (empty) vault: the class still says what is configured, and the
    // evidence says what the vault lists.
    assert!(c["evidence"].as_str().unwrap().contains("lists no secret"));
    let present = run(
        Some(&cfg),
        &fake(&[]),
        Vec::new(),
        Path::new("/nonexistent-g6c/vault.key"),
        names(&["GITHUB_APP_KEY"]),
        no_tail(),
        (false, false),
    );
    assert!(
        consumer(&present, KEY)["evidence"]
            .as_str()
            .unwrap()
            .contains("lists a secret")
    );
    // A vault that could not be listed is said to be unlistable, not empty.
    let unlisted = run(
        Some(&cfg),
        &fake(&[]),
        Vec::new(),
        Path::new("/nonexistent-g6c/vault.key"),
        None,
        no_tail(),
        (false, false),
    );
    assert_eq!(class(&unlisted, KEY), "unknown");
    assert!(
        consumer(&unlisted, KEY)["evidence"]
            .as_str()
            .unwrap()
            .contains("could not be listed")
    );
}

#[test]
fn the_key_variable_with_the_app_id_is_env_and_replaces_the_config() {
    let tmp = TempDir::new().unwrap();
    let pem = tmp.path().join("env.pem");
    write(&pem, CANARY_FILE, 0o600);
    let pem_s = pem.to_str().unwrap();
    let env = [
        ("FL_GITHUB_PRIVATE_KEY_PATH", pem_s),
        ("FL_GITHUB_APP_ID", "7"),
    ];
    let v = report_of(Some(&github(Some(Path::new("/k/a.pem")), None)), &env);
    assert_eq!(class(&v, KEY), "env");
    assert_eq!(consumer(&v, KEY)["plaintext_path_present"], true);
    assert!(
        consumer(&v, KEY)["evidence"]
            .as_str()
            .unwrap()
            .contains("not used")
    );
    // No `[github]` at all: the variables alone are the source.
    assert_eq!(
        class(&report_of(Some(&Default::default()), &env), KEY),
        "env"
    );
    let body = v.to_string();
    assert!(!body.contains(pem_s) && !body.contains(CANARY_FILE));
}

#[test]
fn the_key_variable_without_the_app_id_is_not_a_source() {
    let v = report_of(
        Some(&Default::default()),
        &[("FL_GITHUB_PRIVATE_KEY_PATH", "/nonexistent-g6c/x.pem")],
    );
    assert_eq!(class(&v, KEY), "unconfigured");
    assert!(
        consumer(&v, KEY)["evidence"]
            .as_str()
            .unwrap()
            .contains("not used without")
    );
}

#[test]
fn nothing_configured_is_unconfigured() {
    let v = report_of(Some(&Default::default()), &[]);
    assert_eq!(class(&v, KEY), "unconfigured");
    assert_eq!(consumer(&v, KEY)["plaintext_path_present"], false);
    assert_eq!(consumer(&v, KEY)["last_denied"], Value::Null);
}

#[test]
fn a_ref_with_the_key_variable_is_ambiguous_and_reported_as_unknown() {
    let cfg = github(None, Some("GITHUB_APP_KEY"));
    let v = report_of(
        Some(&cfg),
        &[("FL_GITHUB_PRIVATE_KEY_PATH", "/nonexistent-g6c/x.pem")],
    );
    assert_eq!(class(&v, KEY), "unknown");
    assert!(
        consumer(&v, KEY)["evidence"]
            .as_str()
            .unwrap()
            .starts_with("ambiguous")
    );
}

#[test]
fn a_file_and_a_ref_together_are_unknown() {
    let cfg = github(Some(Path::new("/k/a.pem")), Some("GITHUB_APP_KEY"));
    assert_eq!(class(&report_of(Some(&cfg), &[]), KEY), "unknown");
}

#[test]
fn a_hub_never_told_its_config_reports_unknown_not_unconfigured() {
    let v = report_of(None, &[]);
    assert_eq!(class(&v, KEY), "unknown");
    assert_eq!(consumer(&v, KEY)["plaintext_path_present"], Value::Null);
    assert_eq!(class(&v, "github-token"), "unknown");
}

// --- Tokens -----------------------------------------------------------------------------------

#[test]
fn the_github_token_is_env_when_a_variable_or_a_binding_says_so() {
    let cfg = VaultConsumerConfig::default();
    let v = report_of(Some(&cfg), &[("GITHUB_TOKEN", CANARY_ENV)]);
    assert_eq!(class(&v, "github-token"), "env");
    let text = consumer(&v, "github-token")["evidence"].as_str().unwrap();
    assert!(text.contains("held by `gh`"));
    assert!(
        consumer(&report_of(Some(&cfg), &[]), "github-token")["evidence"]
            .as_str()
            .unwrap()
            .contains("held by `gh`")
    );
    assert!(text.contains("$GITHUB_TOKEN"));
    assert!(!v.to_string().contains(CANARY_ENV));
    assert_eq!(
        class(&report_of(Some(&cfg), &[]), "github-token"),
        "unconfigured"
    );
    // A blank variable is not a token.
    assert_eq!(
        class(
            &report_of(Some(&cfg), &[("GITHUB_TOKEN", "  ")]),
            "github-token"
        ),
        "unconfigured"
    );
    let bound = VaultConsumerConfig {
        github_env_credential_bindings: 2,
        ..Default::default()
    };
    assert_eq!(class(&report_of(Some(&bound), &[]), "github-token"), "env");
}

#[test]
fn the_owner_and_wire_tokens_follow_the_variable_then_the_file() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("owner.tok");
    let wire = tmp.path().join("wire.tok");
    write(&file, CANARY_FILE, 0o600);
    let cfg = VaultConsumerConfig {
        owner_token_file: Some(file.clone()),
        wire_token_file: Some(wire.clone()),
        ..Default::default()
    };
    let v = report_of(Some(&cfg), &[]);
    assert_eq!(class(&v, "hub-owner-token"), "plaintext-file");
    assert_eq!(
        consumer(&v, "hub-owner-token")["plaintext_path_present"],
        true
    );
    assert_eq!(class(&v, "wire-peer-token"), "plaintext-file");
    assert_eq!(
        consumer(&v, "wire-peer-token")["plaintext_path_present"],
        false
    );
    assert!(
        consumer(&v, "hub-owner-token")["evidence"]
            .as_str()
            .unwrap()
            .contains("cannot broker")
    );

    // The variable wins, and the file's setting is noted as unused.
    let v = report_of(
        Some(&cfg),
        &[
            ("FL_HUB_OWNER_TOKEN", CANARY_ENV),
            ("FL_WIRE_TOKEN", CANARY_ENV),
        ],
    );
    assert_eq!(class(&v, "hub-owner-token"), "env");
    assert_eq!(class(&v, "wire-peer-token"), "env");
    assert!(
        consumer(&v, "hub-owner-token")["evidence"]
            .as_str()
            .unwrap()
            .contains("not used while")
    );

    // No setting and no token held: unconfigured. A token held with no known source: unknown.
    let none = VaultConsumerConfig::default();
    assert_eq!(
        class(&report_of(Some(&none), &[]), "hub-owner-token"),
        "unconfigured"
    );
    let held = run(
        Some(&none),
        &fake(&[]),
        Vec::new(),
        Path::new("/nonexistent-g6c/vault.key"),
        names(&[]),
        no_tail(),
        (true, true),
    );
    assert_eq!(class(&held, "hub-owner-token"), "unknown");
    assert_eq!(class(&held, "wire-peer-token"), "unknown");
    let body = v.to_string();
    assert!(!body.contains(CANARY_ENV) && !body.contains(CANARY_FILE));
    assert!(!body.contains(file.to_str().unwrap()));
}

// --- Vendors ----------------------------------------------------------------------------------

fn vendor(id: &str, env: Option<&str>, secret: Option<&str>) -> VendorKey {
    VendorKey {
        id: id.into(),
        env_key_name: env.map(str::to_string),
        vault_secret_name: secret.map(str::to_string),
    }
}

#[test]
fn each_vendor_reports_which_source_resolves() {
    let vendors = vec![
        vendor("zeta", Some("ZETA_KEY"), Some("ZETA_KEY")),
        vendor("alpha", Some("ALPHA_KEY"), Some("ALPHA_KEY")),
        vendor("beta", Some("BETA_KEY"), Some("BETA_KEY")),
        vendor("gamma", None, Some("GAMMA_SECRET")),
        vendor("delta", Some("DELTA_KEY"), None),
    ];
    let v = run(
        Some(&Default::default()),
        &fake(&[("ALPHA_KEY", CANARY_ENV)]),
        vendors,
        Path::new("/nonexistent-g6c/vault.key"),
        names(&["BETA_KEY", "GAMMA_SECRET"]),
        no_tail(),
        (false, false),
    );
    // Environment first, as the resolver does.
    assert_eq!(class(&v, "vendor:alpha"), "env");
    // Not in the environment, in the vault.
    assert_eq!(class(&v, "vendor:beta"), "vault-ref");
    assert_eq!(class(&v, "vendor:gamma"), "vault-ref");
    // A secret name the vault does not list is not a source.
    assert_eq!(class(&v, "vendor:zeta"), "unconfigured");
    assert_eq!(class(&v, "vendor:delta"), "unconfigured");
    // Sorted by id, between the fixed consumers and the master key.
    let ids: Vec<&str> = v["consumers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.first(), Some(&KEY));
    assert_eq!(ids.last(), Some(&"vault-master-key"));
    let vendor_ids: Vec<&&str> = ids.iter().filter(|i| i.starts_with("vendor:")).collect();
    assert_eq!(
        vendor_ids,
        [
            &"vendor:alpha",
            &"vendor:beta",
            &"vendor:delta",
            &"vendor:gamma",
            &"vendor:zeta"
        ]
    );
    assert!(!v.to_string().contains(CANARY_ENV));
}

#[test]
fn a_vendor_secret_in_a_vault_that_could_not_be_listed_is_unknown() {
    let listed = |env: &[(&str, &str)]| {
        run(
            Some(&Default::default()),
            &fake(env),
            vec![vendor("a", Some("A_KEY"), Some("A_KEY"))],
            Path::new("/nonexistent-g6c/vault.key"),
            None,
            no_tail(),
            (false, false),
        )
    };
    let v = listed(&[]);
    assert_eq!(class(&v, "vendor:a"), "unknown");
    let text = consumer(&v, "vendor:a")["evidence"].as_str().unwrap();
    assert!(text.contains("configured to a secret named A_KEY"));
    assert!(text.contains("could not be listed"));
    // A variable that is set still resolves first.
    assert_eq!(class(&listed(&[("A_KEY", CANARY_ENV)]), "vendor:a"), "env");
}

// --- Custody and the master key ----------------------------------------------------------------

fn custody_of(key: &Path, env: &[(&str, &str)]) -> Value {
    run(
        Some(&Default::default()),
        &fake(env),
        Vec::new(),
        key,
        names(&[]),
        no_tail(),
        (false, false),
    )
}

#[test]
fn the_key_mode_is_reported_for_0600_and_0644() {
    let tmp = TempDir::new().unwrap();
    let key = tmp.path().join("vault.key");
    write(&key, CANARY_FILE, 0o600);
    let v = custody_of(&key, &[]);
    assert_eq!(v["custody"]["mode"], "same-unix-user");
    assert_eq!(v["custody"]["vault_key_mode"], "0600");
    assert_eq!(v["custody"]["vault_key_readable_by_group_or_other"], false);
    assert_eq!(class(&v, "vault-master-key"), "plaintext-file");
    assert_eq!(
        consumer(&v, "vault-master-key")["plaintext_path_present"],
        true
    );

    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    let v = custody_of(&key, &[]);
    assert_eq!(v["custody"]["vault_key_mode"], "0644");
    assert_eq!(v["custody"]["vault_key_readable_by_group_or_other"], true);
    fs::set_permissions(&key, fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(
        custody_of(&key, &[])["custody"]["vault_key_readable_by_group_or_other"],
        true
    );
    assert!(!v.to_string().contains(CANARY_FILE));
    assert!(!v.to_string().contains(key.to_str().unwrap()));
}

#[test]
fn a_missing_or_symlinked_key_has_no_mode_and_the_env_key_is_env() {
    let tmp = TempDir::new().unwrap();
    let v = custody_of(&tmp.path().join("absent.key"), &[]);
    assert_eq!(v["custody"]["vault_key_mode"], Value::Null);
    assert_eq!(
        v["custody"]["vault_key_readable_by_group_or_other"],
        Value::Null
    );
    assert_eq!(class(&v, "vault-master-key"), "unconfigured");

    let target = tmp.path().join("real.key");
    write(&target, CANARY_FILE, 0o644);
    let link = tmp.path().join("link.key");
    symlink(&target, &link).unwrap();
    let v = custody_of(&link, &[]);
    assert_eq!(v["custody"]["vault_key_mode"], Value::Null);

    let v = custody_of(&target, &[("FL_VAULT_MASTER_KEY", CANARY_ENV)]);
    assert_eq!(class(&v, "vault-master-key"), "env");
    assert!(!v.to_string().contains(CANARY_ENV));
}

// --- The audit tail ----------------------------------------------------------------------------

fn entry(secret: &str, outcome: &str) -> String {
    json!({
        "timestamp": "2026-01-01T00:00:00Z",
        "caller": "ferris",
        "secrets": [secret],
        "method": "GET",
        "host": "CANARY-HOST.example",
        "outcome": outcome,
    })
    .to_string()
}

#[test]
fn the_last_denied_outcome_is_read_from_the_log() {
    let tmp = TempDir::new().unwrap();
    let log = tmp.path().join("gate-audit.jsonl");
    let text = [
        entry("A_KEY", "denied:host"),
        entry("B_KEY", "allowed"),
        entry("A_KEY", "denied:agent"),
        entry("B_KEY", "denied:expired"),
        entry("A_KEY", "allowed"),
        "not json at all".to_string(),
        "{\"outcome\": 5}".to_string(),
        "{\"outcome\": \"denied:host\"".to_string(),
    ]
    .join("\n");
    write(&log, &text, 0o600);
    let tail = read_audit_tail(&log);
    assert_eq!(tail.state, "ok");
    assert_eq!(tail.denied.len(), 3);
    let v = run(
        Some(&Default::default()),
        &fake(&[]),
        vec![
            vendor("a", None, Some("A_KEY")),
            vendor("c", None, Some("C_KEY")),
        ],
        Path::new("/nonexistent-g6c/vault.key"),
        names(&["A_KEY", "C_KEY"]),
        tail,
        (false, false),
    );
    assert_eq!(v["audit"]["state"], "ok");
    assert_eq!(
        v["audit"]["last_denied"],
        json!({ "outcome": "denied:expired" })
    );
    assert_eq!(
        consumer(&v, "vendor:a")["last_denied"],
        json!({ "outcome": "denied:agent" })
    );
    assert_eq!(consumer(&v, "vendor:c")["last_denied"], Value::Null);
    // Outcome only: no caller, host or timestamp.
    let body = v.to_string();
    assert!(!body.contains("CANARY-HOST") && !body.contains("ferris") && !body.contains("2026"));
}

#[test]
fn an_outcome_that_is_not_a_plain_reason_is_never_echoed() {
    assert_eq!(
        sanitize_outcome("denied:host").as_deref(),
        Some("denied:host")
    );
    assert_eq!(sanitize_outcome("allowed"), None);
    assert_eq!(sanitize_outcome("error:upstream"), None);
    for bad in [
        "denied:",
        "denied:Host",
        "denied:a b",
        "denied:host\nx",
        "denied:CANARY-HOST.example",
        &format!("denied:{}", "a".repeat(33)),
    ] {
        assert_eq!(
            sanitize_outcome(bad).as_deref(),
            Some("denied:other"),
            "{bad}"
        );
    }
}

#[test]
fn a_missing_log_is_absent_and_a_directory_is_not_a_regular_file() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(
        read_audit_tail(&tmp.path().join("gate-audit.jsonl")).state,
        "absent"
    );
    let dir = tmp.path().join("dir.jsonl");
    fs::create_dir(&dir).unwrap();
    let tail = read_audit_tail(&dir);
    assert_eq!(tail.state, "not-a-regular-file");
    assert!(tail.denied.is_empty());
}

#[test]
fn a_symlinked_log_is_refused_and_not_followed() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("elsewhere.jsonl");
    write(&target, &entry("A_KEY", "denied:host"), 0o600);
    let log = tmp.path().join("gate-audit.jsonl");
    symlink(&target, &log).unwrap();
    let tail = read_audit_tail(&log);
    assert_eq!(tail.state, "symlink");
    assert!(tail.denied.is_empty());
}

#[test]
fn only_the_tail_of_an_oversized_log_is_read_and_a_cut_line_is_dropped() {
    let tmp = TempDir::new().unwrap();
    let log = tmp.path().join("gate-audit.jsonl");
    let mut text = String::new();
    // An old denial far outside the tail window, then filler, then the recent one.
    text.push_str(&entry("OLD_KEY", "denied:ssrf"));
    text.push('\n');
    let filler = entry("F_KEY", "allowed");
    while text.len() < 300 * 1024 {
        text.push_str(&filler);
        text.push('\n');
    }
    // A line longer than the line cap is skipped, whatever it says.
    text.push_str(&format!(
        "{{\"outcome\":\"denied:policy\",\"pad\":\"{}\"}}\n",
        "x".repeat(AUDIT_MAX_LINE + 1)
    ));
    text.push_str(&entry("NEW_KEY", "denied:method"));
    text.push('\n');
    write(&log, &text, 0o600);
    let tail = read_audit_tail(&log);
    assert_eq!(tail.state, "ok");
    assert_eq!(tail.denied.len(), 1, "{:?}", tail.denied);
    assert_eq!(tail.denied[0].outcome, "denied:method");

    // A whole log that is one enormous line has no usable tail.
    write(
        &log,
        &format!("{{\"pad\":\"{}\"}}", "y".repeat(200 * 1024)),
        0o600,
    );
    let tail = read_audit_tail(&log);
    assert_eq!(tail.state, "ok");
    assert!(tail.denied.is_empty());
}

// --- No claim of completion --------------------------------------------------------------------

#[test]
fn the_report_never_says_complete_and_says_it_does_not_verify() {
    let tmp = TempDir::new().unwrap();
    let key = tmp.path().join("vault.key");
    write(&key, CANARY_FILE, 0o600);
    // The best case the report can show: a ref, a closed-down key, nothing denied.
    let cfg = github(None, Some("GITHUB_APP_KEY"));
    let v = run(
        Some(&cfg),
        &fake(&[]),
        vec![vendor("a", None, Some("A_KEY"))],
        &key,
        names(&["GITHUB_APP_KEY", "A_KEY"]),
        no_tail(),
        (false, false),
    );
    let body = v.to_string().to_ascii_lowercase();
    assert!(!body.contains("complete"), "{body}");
    assert!(!body.contains("cutover-"), "{body}");
    assert!(!body.contains("plaintext=0"), "{body}");
    assert!(
        v["disclaimer"]
            .as_str()
            .unwrap()
            .contains("Owner-window drill")
    );
    assert!(
        v["disclaimer"]
            .as_str()
            .unwrap()
            .contains("not that it works")
    );
    assert_eq!(v["schema_version"], 1);
    let classes = [
        "vault-ref",
        "plaintext-file",
        "env",
        "unconfigured",
        "unknown",
    ];
    for c in v["consumers"].as_array().unwrap() {
        assert!(classes.contains(&c["class"].as_str().unwrap()), "{c}");
        let mut keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "class",
                "evidence",
                "id",
                "label",
                "last_denied",
                "plaintext_path_present"
            ]
        );
    }
}

// --- The endpoint ------------------------------------------------------------------------------

/// Every variable the endpoint reads, and the Vault's location ones: a developer's real values
/// must never reach a test.
const READ_ENV: [&str; 15] = [
    "FL_VAULT_PATH",
    "FL_VAULT_KEY_PATH",
    "FL_VAULT_MASTER_KEY",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "FL_GITHUB_PRIVATE_KEY_PATH",
    "FL_GITHUB_APP_ID",
    "FL_GITHUB_TOKEN",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "FL_HUB_OWNER_TOKEN",
    "FL_WIRE_TOKEN",
    "ANTHROPIC_API_KEY",
    "MOONSHOT_API_KEY",
    "FL_STORE_BACKEND",
];

struct CleanEnv {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn clean_env() -> CleanEnv {
    // The crate-wide lock the doctor and probe tests take for the same variables. These tests
    // never take the probe lock, so there is no order to keep with it.
    let lock = crate::SECRET_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = READ_ENV.iter().map(|k| (*k, std::env::var_os(k))).collect();
    for k in READ_ENV {
        // SAFETY: every test that reads these variables holds SECRET_ENV_LOCK.
        unsafe { std::env::remove_var(k) };
    }
    CleanEnv { saved, _lock: lock }
}

fn set_env(k: &str, v: &str) {
    // SAFETY: the caller holds the CleanEnv lock.
    unsafe { std::env::set_var(k, v) };
}

impl Drop for CleanEnv {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            // SAFETY: still holding SECRET_ENV_LOCK.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

fn hub(tmp: &TempDir) -> HubState {
    HubState::new(HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: tmp.path().join("store.redb"),
        repo_root: tmp.path().to_path_buf(),
        ui_dir: None,
        owner_login: "lmcfarlin".into(),
        reviewer_apps: ReviewerApps::default(),
        owner_token: Some(OWNER.into()),
        peer_token: Some(PEER.into()),
        trust_tailscale_headers: false,
        enable_terminal: false,
        session_ttl_secs: 3600,
        home_override: Some(tmp.path().to_path_buf()),
        github: crate::github_pr::HubGithub::default(),
    })
}

fn request(state: &HubState, method: &str, token: Option<&str>) -> (u16, Value) {
    let mut req = HttpRequest::new(method, ROUTE);
    if let Some(t) = token {
        req = req.with_header("Authorization", format!("Bearer {t}"));
    }
    let resp = handle_http_request(state, &req);
    (resp.status, resp.json_value().unwrap_or(Value::Null))
}

#[test]
fn only_the_owner_may_read_the_report() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let state = hub(&tmp);
    let (status, body) = request(&state, "GET", Some(OWNER));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["schema_version"], 1);
    let (status, body) = request(&state, "GET", Some(PEER));
    assert_eq!(status, 403);
    assert_eq!(body["error"], "owner authority required");
    assert!(
        body.get("consumers").is_none(),
        "a peer learned something: {body}"
    );
    assert_eq!(request(&state, "GET", None).0, 401);
    assert_eq!(request(&state, "GET", Some("wrong-token")).0, 401);
    // The Owner may only GET; a Peer is refused before the method is looked at.
    for method in ["POST", "PUT", "DELETE"] {
        assert_eq!(request(&state, method, Some(OWNER)).0, 405, "{method}");
        assert_eq!(request(&state, method, Some(PEER)).0, 403, "{method}");
    }
}

/// A snapshot of every file under `dir`: path, mode, length, modification time and bytes.
fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let meta = fs::symlink_metadata(&p).unwrap();
            let content = if meta.is_file() {
                format!("{:?}", fs::read(&p).unwrap())
            } else {
                String::new()
            };
            out.insert(
                p.strip_prefix(base).unwrap().display().to_string(),
                format!(
                    "{:o} {} {:?} {content}",
                    meta.permissions().mode(),
                    meta.len(),
                    meta.modified().unwrap()
                ),
            );
            if meta.is_dir() {
                walk(base, &p, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn the_endpoint_reports_the_real_files_and_leaves_them_untouched() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let vault_path = home.join(".local/share/fl/vault.enc");
    let key_path = home.join(".config/fl/vault.key");
    fs::create_dir_all(vault_path.parent().unwrap()).unwrap();
    // A throwaway key and a real vault holding a canary value.
    write(&key_path, &"ab".repeat(32), 0o644);
    {
        let mut vault = fl_gate::Vault::open(&vault_path, [7u8; 32]).unwrap();
        vault
            .set(fl_gate::vault::SetSecretArgs {
                name: "GITHUB_APP_KEY".into(),
                value: CANARY_VAULT.into(),
                ..Default::default()
            })
            .unwrap();
        vault
            .set(fl_gate::vault::SetSecretArgs {
                name: "MOONSHOT_API_KEY".into(),
                value: CANARY_VAULT.into(),
                ..Default::default()
            })
            .unwrap();
        vault.save().unwrap();
    }
    let log = vault_path.with_file_name("gate-audit.jsonl");
    write(
        &log,
        &format!(
            "{}\n{}\n",
            entry("GITHUB_APP_KEY", "denied:host"),
            entry("OTHER", "denied:agent")
        ),
        0o600,
    );
    let owner_file = home.join("owner.token");
    write(&owner_file, CANARY_FILE, 0o600);
    let pem = home.join("app.pem");
    write(&pem, CANARY_FILE, 0o600);

    let state = hub(&tmp);
    state.set_vault_consumer_config(VaultConsumerConfig {
        github_app: Some(GithubAppKeys {
            private_key_file: None,
            private_key_ref: Some("GITHUB_APP_KEY".into()),
        }),
        github_env_credential_bindings: 0,
        owner_token_file: Some(owner_file.clone()),
        wire_token_file: None,
    });
    set_env("FL_GITHUB_TOKEN", CANARY_ENV);
    set_env("FL_WIRE_TOKEN", CANARY_ENV);

    let before = snapshot(home);
    let (status, v) = request(&state, "GET", Some(OWNER));
    assert_eq!(status, 200, "{v}");
    assert_eq!(snapshot(home), before, "the request changed a file");

    assert_eq!(class(&v, KEY), "vault-ref");
    assert_eq!(
        consumer(&v, KEY)["last_denied"],
        json!({ "outcome": "denied:host" })
    );
    assert_eq!(class(&v, "github-token"), "env");
    assert_eq!(class(&v, "hub-owner-token"), "plaintext-file");
    assert_eq!(class(&v, "wire-peer-token"), "env");
    assert_eq!(class(&v, "vendor:kimi-api"), "vault-ref");
    assert_eq!(class(&v, "vault-master-key"), "plaintext-file");
    assert_eq!(v["custody"]["vault_key_mode"], "0644");
    assert_eq!(v["custody"]["vault_key_readable_by_group_or_other"], true);
    assert_eq!(
        v["audit"]["last_denied"],
        json!({ "outcome": "denied:agent" })
    );

    // With the vendor variable set, that vendor resolves from the environment.
    set_env("MOONSHOT_API_KEY", CANARY_ENV);
    let (_, v2) = request(&state, "GET", Some(OWNER));
    assert_eq!(class(&v2, "vendor:kimi-api"), "env");

    for body in [v.to_string(), v2.to_string()] {
        for secret in [CANARY_ENV, CANARY_FILE, CANARY_VAULT, &"ab".repeat(32)] {
            assert!(!body.contains(secret), "{secret} leaked");
        }
        for path in [&owner_file, &pem, &key_path, &vault_path, &log] {
            assert!(!body.contains(path.to_str().unwrap()), "{path:?} leaked");
        }
        assert!(!body.contains(home.to_str().unwrap()), "a path leaked");
        assert!(!body.to_ascii_lowercase().contains("complete"));
    }
}

#[test]
fn the_endpoint_survives_a_corrupt_vault_and_a_symlinked_log() {
    let _env = clean_env();
    let tmp = TempDir::new().unwrap();
    let vault_path = tmp.path().join(".local/share/fl/vault.enc");
    write(&vault_path, "{ this is not a vault", 0o600);
    let target = tmp.path().join("elsewhere.jsonl");
    write(&target, &entry("A", "denied:host"), 0o600);
    symlink(&target, vault_path.with_file_name("gate-audit.jsonl")).unwrap();
    let state = hub(&tmp);
    state.set_vault_consumer_config(VaultConsumerConfig {
        github_app: Some(GithubAppKeys {
            private_key_file: None,
            private_key_ref: Some("GITHUB_APP_KEY".into()),
        }),
        ..Default::default()
    });
    let (status, v) = request(&state, "GET", Some(OWNER));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["audit"]["state"], "symlink");
    assert_eq!(v["audit"]["last_denied"], Value::Null);
    assert_eq!(class(&v, KEY), "unknown");
    assert!(
        consumer(&v, KEY)["evidence"]
            .as_str()
            .unwrap()
            .contains("could not be listed")
    );
}
