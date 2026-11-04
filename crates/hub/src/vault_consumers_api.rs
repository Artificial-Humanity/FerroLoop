//! `GET /api/v1/gate/vault-consumers`: an OBSERVATIONAL report of where each credential this hub
//! process depends on is configured to come from (Record #86, Gate G6).
//!
//! **Owner only.** A Peer gets a 403 and an unauthenticated caller a 401 (before this module).
//!
//! **Facts, not claims.** The report states configuration facts the hub can observe: which source
//! each consumer is set to read (`vault-ref`, `plaintext-file`, `env`, `unconfigured`, or
//! `unknown` when the facts conflict or the hub was not told), the custody mode, and the outcome
//! of the most recent refused Vault use. It never says a cutover is finished, never says a
//! plaintext secret is gone, and never says a source works: `vault-ref` means a reference is
//! configured, nothing more. The cutover check is the Owner-window drill (G6 acceptance), which
//! this endpoint does not perform.
//!
//! **Body.**
//!
//! ```json
//! { "schema_version": 1,
//!   "disclaimer": "...",
//!   "custody": { "mode": "same-unix-user", "note": "...",
//!                "vault_key_readable_by_group_or_other": false, "vault_key_mode": "0600" },
//!   "consumers": [ { "id", "label", "class", "evidence", "plaintext_path_present",
//!                    "last_denied": { "outcome": "denied:host" } | null } ],
//!   "audit": { "state": "ok", "last_denied": { "outcome": "denied:host" } | null } }
//! ```
//!
//! `plaintext_path_present` is `true` when a plaintext FILE path is configured and that file
//! exists, `false` when none is configured (or the consumer's value is held directly in an
//! environment variable), and `null` when the hub cannot tell. `last_denied` on a consumer is the
//! newest `denied:*` audit entry that names the consumer's Vault secret; it is `null` for a
//! consumer with no Vault secret. There are no timestamps.
//! The App-key row's `binding_absent` is true or false only when its configured Vault
//! signing secret was authenticated; null means the binding could not be established.
//!
//! **What is read.** The hub's own configuration (given by `fl hub serve` through
//! [`VaultConsumerConfig`]), the PRESENCE of environment variables (a boolean, never a value),
//! file METADATA (existence and mode bits, never contents), the Vault file's secret NAMES (the
//! metadata list, plus an authenticated snapshot of the configured App key when available;
//! the existing master key may be read but is never created, and nothing is written), and
//! the last 64 KiB of `gate-audit.jsonl` (opened without following a symlink, a regular file
//! only; a missing, corrupt, oversized or symlinked log is reported as a state, never an error).
//! No path, environment value or secret value leaves the hub: the text names config keys,
//! variable names and Vault secret names only.
//!
//! The work is blocking file I/O, bounded as above, and runs on the HTTP worker that already runs
//! blocking handlers (`spawn_blocking` in `ws.rs`).

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use fl_core::vendor::{VendorRegistry, VendorTransport};
use serde_json::{Value, json};

use crate::auth::HubAuthority;
use crate::{HttpRequest, HttpResponse, HubState};

/// The route.
const ROUTE: &str = "/api/v1/gate/vault-consumers";
/// How much of the end of `gate-audit.jsonl` is read.
const AUDIT_TAIL_BYTES: u64 = 64 * 1024;
/// An audit line longer than this is skipped.
const AUDIT_MAX_LINE: usize = 16 * 1024;
/// The Vault file is not listed when it is larger than this.
const VAULT_LIST_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// The most `denied:*` entries kept from the tail.
const MAX_DENIED_KEPT: usize = 256;

const DISCLAIMER: &str = "Configuration facts observed by this hub process: which source each \
    consumer is set to read. A class of vault-ref means a Vault reference is configured, not that \
    it works, and nothing here says a plaintext secret is gone. Verifying a cutover is the \
    Owner-window drill (Gate G6 acceptance); this report does not perform it and never reports it.";

const CUSTODY_NOTE: &str = "The Vault master key and the Vault file belong to one Unix user, and \
    every FerroLoop process runs as that user. Any process of that user can read the key and \
    decrypt every secret. Custody is file permission, not a separate signer boundary.";

/// What `fl hub serve` tells the hub about its own configuration, so the report can name the
/// source each consumer is set to read. It holds paths and names, never a secret value.
/// Without it (`set_vault_consumer_config` never called) the configuration-derived consumers
/// report `unknown`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultConsumerConfig {
    /// The `[github]` table, if the config has one.
    pub github_app: Option<GithubAppKeys>,
    /// How many project trackers say `credential = "env"`.
    pub github_env_credential_bindings: usize,
    /// The Hub owner token file (`--owner-token-file` or `[hub] owner_token_file`).
    pub owner_token_file: Option<PathBuf>,
    /// The wire peer token file (`[wire] token_file`).
    pub wire_token_file: Option<PathBuf>,
}

/// How `[github]` names the App's private key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GithubAppKeys {
    /// `private_key`: a PEM file.
    pub private_key_file: Option<PathBuf>,
    /// `private_key_ref`: the NAME of the Vault secret it refers to.
    pub private_key_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    VaultRef,
    PlaintextFile,
    Env,
    Unconfigured,
    Unknown,
}

impl Class {
    fn as_str(self) -> &'static str {
        match self {
            Class::VaultRef => "vault-ref",
            Class::PlaintextFile => "plaintext-file",
            Class::Env => "env",
            Class::Unconfigured => "unconfigured",
            Class::Unknown => "unknown",
        }
    }
}

#[derive(Debug)]
struct Consumer {
    id: String,
    label: String,
    class: Class,
    evidence: String,
    plaintext_path_present: Option<bool>,
    /// The Vault secret this consumer is set to use, for matching audit entries.
    secret_name: Option<String>,
}

/// One API vendor that needs a key.
#[derive(Debug, Clone)]
struct VendorKey {
    id: String,
    env_key_name: Option<String>,
    vault_secret_name: Option<String>,
}

/// One refused Vault use from the audit tail.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Denied {
    outcome: String,
    secrets: Vec<String>,
}

#[derive(Debug)]
struct AuditTail {
    state: &'static str,
    /// Oldest first.
    denied: Vec<Denied>,
}

/// Everything the report is built from, so a test can supply it without a process environment.
struct Inputs<'a> {
    config: Option<&'a VaultConsumerConfig>,
    owner_token_in_use: bool,
    peer_token_in_use: bool,
    vendors: Vec<VendorKey>,
    key_path: PathBuf,
    /// The Vault's secret names; `None` when they could not be listed.
    vault_names: Option<BTreeSet<String>>,
    audit: AuditTail,
    env: &'a dyn Fn(&str) -> Option<String>,
}

impl Inputs<'_> {
    /// Whether the variable is set and not blank. The value goes no further than this test.
    fn env_set(&self, name: &str) -> bool {
        (self.env)(name).is_some_and(|v| !v.trim().is_empty())
    }

    /// Whether the variable names an existing file. The path goes no further than this test.
    fn env_names_file(&self, name: &str) -> bool {
        (self.env)(name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .is_some_and(|v| is_file(Path::new(&v)))
    }

    /// Whether the Vault holds `name`: `None` when the Vault could not be listed.
    fn vault_has(&self, name: &str) -> Option<bool> {
        self.vault_names
            .as_ref()
            .map(|n| n.contains(&name.to_ascii_uppercase()))
    }
}

/// An existing regular file or a symlink to one; metadata only.
fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn vault_presence_text(has: Option<bool>) -> &'static str {
    match has {
        Some(true) => "the Vault lists a secret of that name",
        Some(false) => "the Vault lists no secret of that name",
        None => "the Vault could not be listed",
    }
}

fn github_private_key(i: &Inputs<'_>) -> Consumer {
    const KEY_VAR: &str = "FL_GITHUB_PRIVATE_KEY_PATH";
    const APP_VAR: &str = "FL_GITHUB_APP_ID";
    let mut c = Consumer {
        id: "github-app-private-key".into(),
        label: "GitHub App private key".into(),
        class: Class::Unknown,
        evidence: String::new(),
        plaintext_path_present: None,
        secret_name: None,
    };
    let env_key = i.env_set(KEY_VAR);
    let env_app = i.env_set(APP_VAR);
    let env_file = i.env_names_file(KEY_VAR);
    let known = i.config.is_some();
    let keys = i.config.and_then(|cfg| cfg.github_app.as_ref());
    let cfg_ref = keys.and_then(|k| k.private_key_ref.clone());
    let cfg_file = keys.and_then(|k| k.private_key_file.as_ref());

    if cfg_ref.is_some() && env_key {
        c.evidence = format!(
            "ambiguous: `[github] private_key_ref` and ${KEY_VAR} are both set, so which key signs \
             is not decided; the commands that build the App credential refuse this"
        );
        c.plaintext_path_present = Some(env_file);
        c.secret_name = cfg_ref;
        return c;
    }
    if env_key && env_app {
        c.class = Class::Env;
        c.plaintext_path_present = Some(env_file);
        c.evidence = format!(
            "${KEY_VAR} names the key file and ${APP_VAR} is set; this replaces `[github]`"
        );
        if cfg_file.is_some() {
            c.evidence
                .push_str("; `[github] private_key` is also set and is not used");
        }
        return c;
    }
    if !known {
        c.evidence = "the hub was not given its `[github]` configuration".into();
        return c;
    }
    let unused = if env_key {
        format!("; ${KEY_VAR} is set but is not used without ${APP_VAR}")
    } else {
        String::new()
    };
    match (cfg_file, cfg_ref) {
        (Some(_), Some(_)) => {
            c.evidence = "`[github]` sets both `private_key` and `private_key_ref`".into();
        }
        (Some(path), None) => {
            c.class = Class::PlaintextFile;
            c.plaintext_path_present = Some(is_file(path));
            c.evidence = format!("`[github] private_key` names a PEM file{unused}");
        }
        (None, Some(name)) => {
            let has = i.vault_has(&name);
            // A Vault that could not be listed leaves the reference unchecked: unknown.
            c.class = if has.is_none() {
                Class::Unknown
            } else {
                Class::VaultRef
            };
            c.plaintext_path_present = Some(false);
            c.evidence = format!(
                "`[github] private_key_ref` is configured to a secret named {name}; {}{unused}",
                vault_presence_text(has)
            );
            c.secret_name = Some(name);
        }
        (None, None) => {
            c.class = Class::Unconfigured;
            c.plaintext_path_present = Some(env_key && env_file);
            c.evidence = format!("`[github]` sets no private key{unused}");
        }
    }
    c
}

fn github_token(i: &Inputs<'_>) -> Consumer {
    let mut c = github_token_source(i);
    c.evidence
        .push_str("; credentials held by `gh` (for example its hosts file) are not inspected");
    c
}

fn github_token_source(i: &Inputs<'_>) -> Consumer {
    const VARS: [&str; 3] = ["FL_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"];
    let set: Vec<&str> = VARS.into_iter().filter(|v| i.env_set(v)).collect();
    let bindings = i.config.map(|c| c.github_env_credential_bindings);
    let mut c = Consumer {
        id: "github-token".into(),
        label: "GitHub user token".into(),
        class: Class::Unknown,
        evidence: String::new(),
        plaintext_path_present: None,
        secret_name: None,
    };
    if !set.is_empty() {
        c.class = Class::Env;
        c.plaintext_path_present = Some(false);
        c.evidence = format!(
            "a token is set in the hub's environment (${})",
            set.join(", $")
        );
        return c;
    }
    match bindings {
        Some(0) => {
            c.class = Class::Unconfigured;
            c.plaintext_path_present = Some(false);
            c.evidence =
                "no token variable is set and no tracker says `credential = \"env\"`".into();
        }
        Some(n) => {
            c.class = Class::Env;
            c.plaintext_path_present = Some(false);
            c.evidence = format!(
                "{n} tracker binding(s) say `credential = \"env\"`; none of ${} is set in the hub's environment",
                VARS.join(", $")
            );
        }
        None => {
            c.evidence =
                "no token variable is set; the hub was not given its tracker configuration".into();
        }
    }
    c
}

/// An inbound token the hub checks (owner or wire): the variable first, then the file.
fn inbound_token(
    i: &Inputs<'_>,
    id: &str,
    label: &str,
    var: &str,
    file_key: &str,
    file: Option<&PathBuf>,
    in_use: bool,
) -> Consumer {
    let mut c = Consumer {
        id: id.into(),
        label: label.into(),
        class: Class::Unknown,
        evidence: String::new(),
        plaintext_path_present: None,
        secret_name: None,
    };
    let file_present = file.map(|p| is_file(p));
    let inbound =
        "an inbound credential callers present to this hub, which the Vault cannot broker";
    if i.env_set(var) {
        c.class = Class::Env;
        c.plaintext_path_present = Some(file_present.unwrap_or(false));
        c.evidence = format!("${var} is set ({inbound})");
        if file.is_some() {
            c.evidence.push_str(&format!(
                "; `{file_key}` is also set and is not used while the variable is set"
            ));
        }
        return c;
    }
    match (i.config, file) {
        (Some(_), Some(_)) => {
            c.class = Class::PlaintextFile;
            c.plaintext_path_present = file_present;
            c.evidence = format!("`{file_key}` names the token file ({inbound})");
        }
        (Some(_), None) if !in_use => {
            c.class = Class::Unconfigured;
            c.plaintext_path_present = Some(false);
            c.evidence = format!("no ${var} and no `{file_key}`; the hub holds no such token");
        }
        (Some(_), None) => {
            c.evidence =
                "the hub holds a token its embedder supplied; its source is not reported".into();
        }
        (None, _) if !in_use => {
            c.class = Class::Unconfigured;
            c.plaintext_path_present = Some(false);
            c.evidence = format!("no ${var}; the hub holds no such token");
        }
        (None, _) => {
            c.evidence =
                format!("${var} is not set; the hub was not given the file setting `{file_key}`");
        }
    }
    c
}

fn vendor_key(i: &Inputs<'_>, v: &VendorKey) -> Consumer {
    let mut c = Consumer {
        id: format!("vendor:{}", v.id),
        label: format!("Vendor API key ({})", v.id),
        class: Class::Unconfigured,
        evidence: String::new(),
        plaintext_path_present: Some(false),
        secret_name: None,
    };
    let env_present = v.env_key_name.as_deref().is_some_and(|n| i.env_set(n));
    let vault_has = v.vault_secret_name.as_deref().and_then(|n| i.vault_has(n));
    if env_present {
        c.class = Class::Env;
        let name = v.env_key_name.as_deref().unwrap_or_default();
        c.evidence = format!("${name} is set in the hub's environment and resolves first");
        if let Some(secret) = &v.vault_secret_name {
            c.evidence.push_str(&format!(
                "; Vault secret {secret} is also named ({})",
                vault_presence_text(vault_has)
            ));
            c.secret_name = Some(secret.clone());
        }
        return c;
    }
    let env_note = v
        .env_key_name
        .as_deref()
        .map(|n| format!("${n} is not set"))
        .unwrap_or_else(|| "no environment variable is named".into());
    match (&v.vault_secret_name, vault_has) {
        (Some(secret), Some(false)) => {
            c.evidence = format!(
                "{env_note}; Vault secret {secret} is named but the Vault lists no secret of that name"
            );
            c.secret_name = Some(secret.clone());
        }
        (Some(secret), has) => {
            // A Vault that could not be listed leaves the name unchecked: unknown.
            c.class = if has.is_none() {
                Class::Unknown
            } else {
                Class::VaultRef
            };
            c.evidence = format!(
                "{env_note}; configured to a secret named {secret}; {}",
                vault_presence_text(has)
            );
            c.secret_name = Some(secret.clone());
        }
        (None, _) => {
            c.evidence = format!("{env_note}; no Vault secret is named");
        }
    }
    c
}

fn master_key(i: &Inputs<'_>) -> Consumer {
    const VAR: &str = "FL_VAULT_MASTER_KEY";
    let file = key_file_mode(&i.key_path).is_some();
    let (class, evidence) = if i.env_set(VAR) {
        let also = if file {
            "; a key file also exists and is not used while the variable is set"
        } else {
            ""
        };
        (Class::Env, format!("${VAR} holds the master key{also}"))
    } else if file {
        (
            Class::PlaintextFile,
            "the master key is a file on disk (`vault.key`)".to_string(),
        )
    } else {
        (Class::Unconfigured, format!("no ${VAR} and no key file"))
    };
    Consumer {
        id: "vault-master-key".into(),
        label: "Vault master key".into(),
        class,
        evidence,
        plaintext_path_present: Some(file),
        secret_name: None,
    }
}

/// The key file's permission bits, from its metadata alone. `None` when it is absent, is a
/// symlink (not followed) or is not a regular file.
fn key_file_mode(path: &Path) -> Option<u32> {
    let meta = fs::symlink_metadata(path).ok()?;
    meta.file_type().is_file().then(|| meta.mode() & 0o777)
}

/// The newest denied outcome that names `secret`.
fn denied_for(tail: &AuditTail, secret: &str) -> Option<String> {
    let secret = secret.to_ascii_uppercase();
    tail.denied
        .iter()
        .rev()
        .find(|d| d.secrets.contains(&secret))
        .map(|d| d.outcome.clone())
}

fn outcome_json(outcome: Option<String>) -> Value {
    outcome.map_or(Value::Null, |o| json!({ "outcome": o }))
}

fn build(i: &Inputs<'_>) -> Value {
    let cfg = i.config;
    let mut consumers = vec![
        github_private_key(i),
        github_token(i),
        inbound_token(
            i,
            "hub-owner-token",
            "Hub owner token",
            "FL_HUB_OWNER_TOKEN",
            "[hub] owner_token_file",
            cfg.and_then(|c| c.owner_token_file.as_ref()),
            i.owner_token_in_use,
        ),
        inbound_token(
            i,
            "wire-peer-token",
            "Wire peer token",
            "FL_WIRE_TOKEN",
            "[wire] token_file",
            cfg.and_then(|c| c.wire_token_file.as_ref()),
            i.peer_token_in_use,
        ),
    ];
    let mut vendors = i.vendors.clone();
    vendors.sort_by(|a, b| a.id.cmp(&b.id));
    consumers.extend(vendors.iter().map(|v| vendor_key(i, v)));
    consumers.push(master_key(i));

    let mode = key_file_mode(&i.key_path);
    let rows: Vec<Value> = consumers
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "label": c.label,
                "class": c.class.as_str(),
                "evidence": c.evidence,
                "plaintext_path_present": c.plaintext_path_present,
                "last_denied": outcome_json(c.secret_name.as_deref().and_then(|s| denied_for(&i.audit, s))),
            })
        })
        .collect();
    json!({
        "schema_version": 1,
        "disclaimer": DISCLAIMER,
        "custody": {
            "mode": "same-unix-user",
            "note": CUSTODY_NOTE,
            "vault_key_readable_by_group_or_other": mode.map(|m| m & 0o077 != 0),
            "vault_key_mode": mode.map(|m| format!("{m:04o}")),
        },
        "consumers": rows,
        "audit": {
            "state": i.audit.state,
            "last_denied": outcome_json(i.audit.denied.last().map(|d| d.outcome.clone())),
        },
    })
}

/// `denied:<reason>` with the reason in the form the gate writes (lowercase letters and `_`);
/// anything else is `denied:other`, so no free text from the log is passed on.
fn sanitize_outcome(raw: &str) -> Option<String> {
    let reason = raw.strip_prefix("denied:")?;
    let plain = (1..=32).contains(&reason.len())
        && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    Some(if plain {
        raw.to_string()
    } else {
        "denied:other".to_string()
    })
}

/// The `denied:*` entries in the last [`AUDIT_TAIL_BYTES`] of the audit log. The file is opened
/// without following a symlink and without blocking, and must be a regular file.
fn read_audit_tail(path: &Path) -> AuditTail {
    let state = |state: &'static str| AuditTail {
        state,
        denied: Vec::new(),
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let mut file = match file {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return state("absent"),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return state("symlink"),
        Err(_) => return state("unreadable"),
    };
    let Ok(meta) = file.metadata() else {
        return state("unreadable");
    };
    if !meta.is_file() {
        return state("not-a-regular-file");
    }
    let start = meta.len().saturating_sub(AUDIT_TAIL_BYTES);
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return state("unreadable");
    }
    let mut buf = Vec::new();
    if file.take(AUDIT_TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return state("unreadable");
    }
    // A read that began inside a line starts at the next one.
    let body: &[u8] = if start > 0 {
        match buf.iter().position(|b| *b == b'\n') {
            Some(n) => &buf[n + 1..],
            None => &[],
        }
    } else {
        &buf
    };
    let mut denied = Vec::new();
    for line in body.split(|b| *b == b'\n') {
        if line.is_empty() || line.len() > AUDIT_MAX_LINE {
            continue;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(outcome) = entry
            .get("outcome")
            .and_then(Value::as_str)
            .and_then(sanitize_outcome)
        else {
            continue;
        };
        let secrets = entry
            .get("secrets")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter(|s| s.len() <= 64)
                    .take(16)
                    .map(str::to_ascii_uppercase)
                    .collect()
            })
            .unwrap_or_default();
        denied.push(Denied { outcome, secrets });
        if denied.len() > MAX_DENIED_KEPT {
            denied.remove(0);
        }
    }
    AuditTail {
        state: "ok",
        denied,
    }
}

/// The Vault's secret names, from its metadata alone: nothing is decrypted and no key is read.
/// `None` when the file is not a regular file, is too large, or does not parse. A missing Vault
/// has no secrets.
fn list_vault_names(vault_path: &Path) -> Option<BTreeSet<String>> {
    match fs::symlink_metadata(vault_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(BTreeSet::new()),
        Err(_) => return None,
        Ok(m) if !m.file_type().is_file() || m.len() > VAULT_LIST_MAX_BYTES => return None,
        Ok(_) => {}
    }
    fl_gate::Vault::open(vault_path, [0u8; fl_gate::vault::MASTER_KEY_LEN])
        .ok()
        .map(|v| {
            v.list()
                .into_iter()
                .map(|m| m.name.to_ascii_uppercase())
                .collect()
        })
}

/// The report for this hub, as JSON.
pub(crate) fn report(state: &HubState) -> Value {
    let home = state.config.home_override.as_deref();
    let vault_path = fl_gate::vault::resolve_vault_path(home);
    let key_path = fl_gate::vault::resolve_master_key_path(home);
    let config = state
        .vault_consumers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let vendors = VendorRegistry::global()
        .list()
        .into_iter()
        .filter_map(|d| match d.transport {
            VendorTransport::HttpApi {
                vault_secret_name,
                env_key_name,
                ..
            } if vault_secret_name.is_some() || env_key_name.is_some() => Some(VendorKey {
                id: d.id,
                env_key_name,
                vault_secret_name,
            }),
            _ => None,
        })
        .collect();
    let audit = read_audit_tail(&vault_path.with_file_name("gate-audit.jsonl"));
    let vault_names = list_vault_names(&vault_path);
    let env = |name: &str| std::env::var(name).ok();
    let mut report = build(&Inputs {
        config: config.as_deref(),
        owner_token_in_use: state.config.owner_token.is_some(),
        peer_token_in_use: state.config.peer_token.is_some(),
        vendors,
        key_path: key_path.clone(),
        vault_names,
        audit,
        env: &env,
    });
    // Only the selected Vault App-key source is checked. A missing key, inaccessible or
    // tampered record is unknown, never a positive assertion about the policy.
    if let Some(rows) = report["consumers"].as_array_mut()
        && let Some(row) = rows
            .iter_mut()
            .find(|row| row["id"] == "github-app-private-key")
    {
        let status = if row["class"] == "vault-ref" {
            config
                .as_ref()
                .and_then(|cfg| cfg.github_app.as_ref())
                .and_then(|app| app.private_key_ref.as_deref())
                .and_then(|name| {
                    if !vault_path.is_file() {
                        return None;
                    }
                    let key = fl_gate::load_existing_master_key(&key_path).ok()?;
                    let vault = fl_gate::Vault::open(&vault_path, key).ok()?;
                    let meta = vault.verified_snapshot(name).ok()?.metadata;
                    (meta.usage == fl_gate::SecretUsage::SignJwtRs256)
                        .then_some(meta.app_binding.is_none())
                })
        } else {
            None
        };
        row["binding_absent"] = json!(status);
    }
    report
}

/// `GET /api/v1/gate/vault-consumers`.
pub(crate) fn handle_get(state: &HubState) -> HttpResponse {
    HttpResponse::json(200, report(state))
}

pub(crate) fn route(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> Option<HttpResponse> {
    if req.path.trim_end_matches('/') != ROUTE {
        return None;
    }
    if !authority.is_owner() {
        return Some(HttpResponse::json(
            403,
            json!({ "error": "owner authority required" }),
        ));
    }
    if req.method != "GET" {
        return Some(
            HttpResponse::json(405, json!({ "error": format!("{ROUTE} takes only GET") }))
                .with_header("Allow", "GET"),
        );
    }
    Some(handle_get(state))
}

#[cfg(test)]
#[path = "vault_consumers_api_tests.rs"]
mod tests;
