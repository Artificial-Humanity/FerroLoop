//! Hosted agent session lifecycle: spin-up, two-phase decommission, restart, and remodel (a move
//! to another vendor) of sessions that FerroDeck and `fl agent` manage (Issue #82 / #69, Local
//! Record 59).
//!
//! The functions are generic over the agent registry and state store, so the hub's HTTP handlers
//! (`crates/hub/src/api.rs`) and `fl agent spin-up|decommission` share one implementation. Hub-only
//! concerns (the in-memory operator prompt queue) reach this module through caller closures or the
//! returned `handshake_dispatched` flag.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fl_core::agent::{AgentRecord, AgentRole, AgentStatus, RegisterAgentArgs};
use fl_core::state::{
    HandoffEntry, MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload,
    StateScope, UpsertStateArgs, validate_slug,
};
use fl_core::store::{AgentRegistry, StateStore, StoreError};
use fl_core::{VendorDescriptor, VendorRegistry, VendorTransport, lookup_vendor};
use fl_exec::stamp;
use fl_wire::dispatcher::{PushError, push_operator_prompt_to_route};
use fl_wire::identity::{RouteTable, VendorRoute, default_routes_path, legacy_routes_path};
use serde::{Deserialize, Serialize};

use crate::api::{
    canonical_reforge_prompt, reforge_role_label, route_matches_session_id,
    synthesize_decommission_prompt,
};

/// Roles `spin-up` accepts (the FerroDeck role list).
pub const SPIN_UP_ROLES: [&str; 5] = [
    "lead",
    "assistant",
    "ui-developer",
    "strategist",
    "lab-manager",
];
/// The five subscription-CLI vendors. Not the list `spin-up` accepts: every decision about a
/// vendor goes through the vendor registry ([`spin_up_vendor_ids`]), which also holds the
/// direct-API vendors and any vendor registered at run time.
pub const SPIN_UP_VENDORS: [&str; 5] = ["claude", "antigravity", "codex", "grok", "muse"];

/// Every vendor id `spin-up` and `remodel` accept: the ids in the vendor registry, sorted.
pub fn spin_up_vendor_ids() -> Vec<String> {
    VendorRegistry::global()
        .list()
        .into_iter()
        .map(|v| v.id)
        .collect()
}

/// Whether a vendor runs as an interactive process in a `tmux` window. An HTTP API vendor has no
/// CLI to host; an id the registry does not know is treated as a CLI (a legacy record).
fn vendor_has_runtime(vendor: &str) -> bool {
    !matches!(
        lookup_vendor(vendor),
        Some(VendorDescriptor {
            transport: VendorTransport::HttpApi { .. },
            ..
        })
    )
}
/// Node a session lands on when the request names none.
pub const DEFAULT_NODE_ID: &str = "ai-lab-0";

/// `session_id` prefix of every hosted-session runtime entry.
const HOSTED_SLUG_PREFIX: &str = "hosted-session-";
/// Upper cap on a persona. Whether it fits `hosted-session-<vendor>-<persona>` in the 64-char
/// state slug also depends on the vendor's id length ([`max_persona_len`]).
const MAX_PERSONA_LEN: usize = 36;
/// Longest state slug (`fl_core::state::validate_slug`).
const MAX_STATE_SLUG_LEN: usize = 64;
const MAX_FIELD_LEN: usize = 128;

pub const STATUS_RUNNING: &str = "running";
pub const STATUS_DECOMMISSIONING: &str = "decommissioning";
pub const STATUS_DECOMMISSIONED: &str = "decommissioned";

/// Failure of a lifecycle operation, with the HTTP status the hub reports for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    /// The request is malformed (400).
    Invalid(String),
    /// No such session or agent (404).
    NotFound(String),
    /// The caller's authority does not cover the request (403).
    Forbidden(String),
    /// The request conflicts with registry state (409).
    Conflict(String),
    /// The store failed (500).
    Store(String),
    /// A multi-step operation failed after its first side effect (502). The message says what
    /// state it left (see [`remodel_session`]); `restored_session_id` names the session a
    /// restore brought back, `None` when nothing was restored.
    Partial {
        message: String,
        restored_session_id: Option<String>,
    },
}

impl LifecycleError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::NotFound(_) => 404,
            Self::Forbidden(_) => 403,
            Self::Conflict(_) => 409,
            Self::Store(_) => 500,
            Self::Partial { .. } => 502,
        }
    }
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m)
            | Self::NotFound(m)
            | Self::Forbidden(m)
            | Self::Conflict(m)
            | Self::Store(m)
            | Self::Partial { message: m, .. } => f.write_str(m),
        }
    }
}

impl std::error::Error for LifecycleError {}

impl From<StoreError> for LifecycleError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::LeadTaken { .. } | StoreError::AgentCollision { .. } => {
                Self::Conflict(e.to_string())
            }
            other => Self::Store(other.to_string()),
        }
    }
}

/// Where lifecycle side effects go. `spawn_runtime` gates every runtime call.
#[derive(Debug, Clone, Copy)]
pub struct LifecycleEnv<'a> {
    /// Replaces `$HOME` when it locates the wire route table (tests, hub `home_override`).
    pub home_override: Option<&'a Path>,
    /// `false`: no runtime is started, stopped or looked up.
    pub spawn_runtime: bool,
    /// The runtime calls when `spawn_runtime` is on: [`TmuxRuntime`], or a fake in tests.
    pub runtime: &'a dyn SessionRuntime,
    /// Tells whether an API vendor's key is provisioned: [`HostKeyProbe`], or a fake in tests.
    pub key_probe: &'a dyn ApiKeyProbe,
}

impl<'a> LifecycleEnv<'a> {
    /// Real `tmux` calls are off under `cfg(test)` and when `FL_TEST_SPINUP_NO_SPAWN=1`.
    pub fn new(home_override: Option<&'a Path>) -> Self {
        let disabled =
            cfg!(test) || std::env::var("FL_TEST_SPINUP_NO_SPAWN").is_ok_and(|v| v.trim() == "1");
        Self {
            home_override,
            spawn_runtime: !disabled,
            runtime: &TmuxRuntime,
            key_probe: &HostKeyProbe,
        }
    }
}

/// Whether an API vendor's key is provisioned on this host. Only the presence is asked: no
/// implementation returns, logs or decrypts a key value.
///
/// "Provisioned" means PRESENT and UNEXPIRED in the Hub process's own environment or Vault. It
/// does not mean the key may be used: a Vault secret's policy (`allowed_agents`, `allowed_hosts`,
/// `allowed_methods`) is checked when the executing process (agent `fl-exec`, the request's host
/// and method) tries to use it, not here. The Hub's environment is what is checked: a key set
/// only in the agent's own environment, or in another user's Vault, does not count.
pub trait ApiKeyProbe: std::fmt::Debug {
    /// `true` when the environment variable `env_key_name` is set to a non-blank value, or the
    /// Vault holds a usable secret named `vault_secret_name`.
    fn key_provisioned(
        &self,
        home_override: Option<&Path>,
        env_key_name: Option<&str>,
        vault_secret_name: Option<&str>,
    ) -> bool;
}

/// Whether an environment variable's value counts as a key: set and not blank.
fn env_value_present(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

/// The real probe: the Hub process's environment, and the Vault's metadata (never its values, and
/// no master key is read or created). A Vault secret is ready when it exists and has not expired;
/// its `allowed_agents`, `allowed_hosts` and `allowed_methods` are NOT consulted (they are
/// checked, for the executing agent, host and method, when the key is used).
#[derive(Debug, Clone, Copy)]
pub struct HostKeyProbe;

impl ApiKeyProbe for HostKeyProbe {
    fn key_provisioned(
        &self,
        home_override: Option<&Path>,
        env_key_name: Option<&str>,
        vault_secret_name: Option<&str>,
    ) -> bool {
        if let Some(name) = env_key_name
            && env_value_present(std::env::var_os(name).as_deref().and_then(|v| v.to_str()))
        {
            return true;
        }
        let Some(secret) = vault_secret_name else {
            return false;
        };
        // A missing, unreadable or expired secret is not usable, so it does not count.
        crate::runtime_matrix::vault_metadata(home_override).is_ok_and(|meta| {
            crate::runtime_matrix::vault_statuses(&[("", secret)], &meta, now_ms())
                .iter()
                .any(|s| s.status == "ready")
        })
    }
}

/// The key probe of a remodel's restore: every key counts as provisioned. The restore puts back
/// the session that was running, and refusing it over a key that has vanished since would leave
/// the persona down; a session on a missing key fails at its first attempt instead.
#[derive(Debug, Clone, Copy)]
struct RestoreKeys;

impl ApiKeyProbe for RestoreKeys {
    fn key_provisioned(&self, _: Option<&Path>, _: Option<&str>, _: Option<&str>) -> bool {
        true
    }
}

/// Refuse (409) a spin-up on an API vendor whose key is not provisioned, naming the vendor, the
/// environment variable and the Vault secret. A vendor that needs no key (a local engine), and a
/// vendor with a CLI, pass. Runs before a spin-up's first side effect.
///
/// The 409 text says the key is not "provisioned": present and unexpired in the Hub's own
/// environment or Vault. A key that is provisioned can still be refused at attempt time by its
/// policy for the executing process (see [`ApiKeyProbe`]); this check does not predict that.
fn check_api_key(env: &LifecycleEnv<'_>, vendor: &str) -> Result<(), LifecycleError> {
    let Some(VendorDescriptor {
        transport:
            VendorTransport::HttpApi {
                vault_secret_name,
                env_key_name,
                ..
            },
        ..
    }) = lookup_vendor(vendor)
    else {
        return Ok(());
    };
    if vault_secret_name.is_none() && env_key_name.is_none() {
        return Ok(());
    }
    if env.key_probe.key_provisioned(
        env.home_override,
        env_key_name.as_deref(),
        vault_secret_name.as_deref(),
    ) {
        return Ok(());
    }
    let remedies: Vec<String> = [
        env_key_name
            .as_ref()
            .map(|n| format!("set the environment variable `{n}`")),
        vault_secret_name
            .as_ref()
            .map(|n| format!("store a Vault secret named `{n}`")),
    ]
    .into_iter()
    .flatten()
    .collect();
    Err(LifecycleError::Conflict(format!(
        "vendor `{vendor}` needs an API key and none is provisioned: {}",
        remedies.join(" or ")
    )))
}

/// The runtime calls a lifecycle makes on a session's `tmux` session (named by the capitalised
/// persona). Only [`LifecycleEnv::runtime`] makes them, and only when `spawn_runtime` is on.
pub trait SessionRuntime: std::fmt::Debug {
    /// Start a detached session; `false` when it did not start (tmux failed, or the name exists).
    /// With `launch` the window runs that command (the vendor's CLI) instead of a blank shell.
    /// Only Muse is launched so far ([`launch_spec_for`]); every other vendor gets a blank window.
    fn spawn(
        &self,
        session: &str,
        window: &str,
        cwd: Option<&str>,
        launch: Option<&LaunchSpec>,
    ) -> bool;
    /// Stop a session; `true` when it is gone afterwards.
    fn kill(&self, session: &str) -> bool;
    /// `true` when a session of this name runs.
    fn exists(&self, session: &str) -> bool;
}

/// The command a runtime window starts in place of a blank shell: run as an argument vector,
/// never through a shell, so no argument needs quoting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Environment variables set for the command.
    pub env: Vec<(String, String)>,
    /// The program, then its arguments.
    pub argv: Vec<String>,
}

/// The vendor CLI to start in a new runtime window, with `prompt` as its first input; `None`
/// where the window stays a blank shell. Only Muse is launched so far.
fn launch_spec_for(
    env: &LifecycleEnv<'_>,
    vendor: &str,
    persona: &str,
    prompt: &str,
) -> Option<LaunchSpec> {
    (vendor == "muse").then(|| LaunchSpec {
        env: vec![("AGENT_PERSONA".to_string(), persona.to_string())],
        argv: vec![
            resolve_muse_bin(
                std::env::var_os("FL_WIRE_MUSE_BIN"),
                env.home_override
                    .map(Path::to_path_buf)
                    .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                    .as_deref(),
            ),
            prompt.to_string(),
        ],
    })
}

/// The Muse binary: `configured` (`FL_WIRE_MUSE_BIN`, the one the dispatcher honours) when set,
/// else `<home>/.local/bin/muse` when that file exists, else `muse` from `PATH`.
fn resolve_muse_bin(configured: Option<std::ffi::OsString>, home: Option<&Path>) -> String {
    if let Some(bin) = configured.filter(|b| !b.is_empty()) {
        return bin.to_string_lossy().into_owned();
    }
    home.map(|h| h.join(".local").join("bin").join("muse"))
        .filter(|p| p.is_file())
        .map_or_else(|| "muse".to_string(), |p| p.to_string_lossy().into_owned())
}

/// The real runtime: the host's `tmux`.
#[derive(Debug, Clone, Copy)]
pub struct TmuxRuntime;

impl SessionRuntime for TmuxRuntime {
    fn spawn(
        &self,
        session: &str,
        window: &str,
        cwd: Option<&str>,
        launch: Option<&LaunchSpec>,
    ) -> bool {
        spawn_tmux_runtime(session, window, cwd, launch)
    }

    fn kill(&self, session: &str) -> bool {
        kill_tmux_runtime(session)
    }

    fn exists(&self, session: &str) -> bool {
        tmux_session_exists(session)
    }
}

/// A runtime that only records: `live` holds the session names it runs. A spawn fails when the
/// name already runs or its window (the vendor) is in `fail_windows`.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FakeRuntime {
    pub(crate) live: std::cell::RefCell<std::collections::BTreeSet<String>>,
    pub(crate) fail_windows: std::cell::RefCell<Vec<String>>,
    pub(crate) calls: std::cell::RefCell<Vec<String>>,
    /// The launch command of each `spawn` call that was not refused, by `session:window`.
    pub(crate) launches: std::cell::RefCell<Vec<(String, Option<LaunchSpec>)>>,
}

#[cfg(test)]
impl FakeRuntime {
    pub(crate) fn running(sessions: &[&str]) -> Self {
        let fake = Self::default();
        fake.live
            .borrow_mut()
            .extend(sessions.iter().map(ToString::to_string));
        fake
    }

    pub(crate) fn env<'a>(&'a self, home: &'a Path) -> LifecycleEnv<'a> {
        LifecycleEnv {
            home_override: Some(home),
            spawn_runtime: true,
            runtime: self,
            key_probe: &ProvisionedKeys,
        }
    }

    pub(crate) fn is_live(&self, session: &str) -> bool {
        self.live.borrow().contains(session)
    }
}

/// A key probe that finds every key provisioned.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProvisionedKeys;

#[cfg(test)]
impl ApiKeyProbe for ProvisionedKeys {
    fn key_provisioned(&self, _: Option<&Path>, _: Option<&str>, _: Option<&str>) -> bool {
        true
    }
}

/// A key probe that finds only the named keys (an environment variable or a Vault secret name).
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FakeKeys {
    pub(crate) have: Vec<String>,
    pub(crate) asked: std::cell::RefCell<Vec<String>>,
}

#[cfg(test)]
impl ApiKeyProbe for FakeKeys {
    fn key_provisioned(
        &self,
        _: Option<&Path>,
        env_key_name: Option<&str>,
        vault_secret_name: Option<&str>,
    ) -> bool {
        self.asked
            .borrow_mut()
            .push(format!("{env_key_name:?}/{vault_secret_name:?}"));
        [env_key_name, vault_secret_name]
            .into_iter()
            .flatten()
            .any(|n| self.have.iter().any(|h| h == n))
    }
}

#[cfg(test)]
impl SessionRuntime for FakeRuntime {
    fn spawn(
        &self,
        session: &str,
        window: &str,
        _cwd: Option<&str>,
        launch: Option<&LaunchSpec>,
    ) -> bool {
        self.calls
            .borrow_mut()
            .push(format!("spawn {session}:{window}"));
        if self.fail_windows.borrow().iter().any(|w| w == window) {
            return false;
        }
        let started = self.live.borrow_mut().insert(session.to_string());
        if started {
            self.launches
                .borrow_mut()
                .push((format!("{session}:{window}"), launch.cloned()));
        }
        started
    }

    fn kill(&self, session: &str) -> bool {
        self.calls.borrow_mut().push(format!("kill {session}"));
        self.live.borrow_mut().remove(session);
        true
    }

    fn exists(&self, session: &str) -> bool {
        self.is_live(session)
    }
}

/// `POST /api/v1/sessions/spin-up` body; the field names are the FerroDeck contract.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SpinUpRequest {
    #[serde(default)]
    pub persona: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub hosted: Option<bool>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// `SpinUpRequest` after validation and defaulting.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidSpinUp {
    persona: String,
    project: String,
    role: String,
    vendor: String,
    model: Option<String>,
    node_id: String,
    hosted: bool,
    cwd: Option<String>,
}

fn trimmed_opt(raw: &Option<String>) -> Option<String> {
    raw.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn invalid_field(name: &str, why: &str) -> LifecycleError {
    LifecycleError::Invalid(format!("{name} {why}"))
}

/// Single-line free text no longer than `MAX_FIELD_LEN`.
fn check_text(name: &str, value: &str) -> Result<(), LifecycleError> {
    if value.chars().count() > MAX_FIELD_LEN || value.chars().any(char::is_control) {
        return Err(invalid_field(
            name,
            "must be single-line text of at most 128 characters",
        ));
    }
    Ok(())
}

/// Longest persona the hosted-session state slug `hosted-session-<vendor>-<persona>` can hold for
/// `vendor`, capped at [`MAX_PERSONA_LEN`]. `0`: the vendor id alone leaves no room.
fn max_persona_len(vendor: &str) -> usize {
    MAX_STATE_SLUG_LEN
        .saturating_sub(HOSTED_SLUG_PREFIX.len() + vendor.len() + 1)
        .min(MAX_PERSONA_LEN)
}

/// Refuse (400) a persona whose hosted-session slug would not fit this vendor, or a vendor id
/// that cannot appear in a state slug at all, before the record is written.
fn check_persona_fits_slug(vendor: &str, persona: &str) -> Result<(), LifecycleError> {
    let max = max_persona_len(vendor);
    if max == 0 {
        return Err(invalid_field(
            "vendor",
            &format!(
                "`{vendor}` is too long to host a session: its state slug cannot fit 64 characters"
            ),
        ));
    }
    if persona.len() > max {
        return Err(invalid_field(
            "persona",
            &format!(
                "must be at most {max} characters with vendor `{vendor}` (the state slug \
                 `{HOSTED_SLUG_PREFIX}{vendor}-<persona>` holds at most {MAX_STATE_SLUG_LEN})"
            ),
        ));
    }
    if validate_slug(&hosted_session_slug(&format!("{vendor}:{persona}"))).is_err() {
        return Err(invalid_field(
            "vendor",
            &format!(
                "`{vendor}` cannot host a session: its id is not usable in a state slug (a-z, 0-9, '-', '_')"
            ),
        ));
    }
    if let Some(other) = colliding_session_id(vendor, persona, &spin_up_vendor_ids()) {
        return Err(LifecycleError::Invalid(format!(
            "session `{vendor}:{persona}` would share its hosted-session state slug `{}` with \
             session `{other}` of the longer vendor id: choose a persona that does not start with \
             that vendor's remainder, or rename one of the vendors",
            hosted_session_slug(&format!("{vendor}:{persona}"))
        )));
    }
    Ok(())
}

/// Whether `persona` is a name a session can have (the rules [`SpinUpRequest::validate`] applies).
fn is_valid_persona(persona: &str) -> bool {
    !persona.is_empty()
        && persona.len() <= MAX_PERSONA_LEN
        && persona.as_bytes()[0].is_ascii_lowercase()
        && persona
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !fl_wire::dispatcher::is_owner_sender(persona)
}

/// A session id `o:p` of a LONGER vendor id `o` whose hosted-session slug equals the slug of
/// `vendor:persona`, over the vendor ids `vendor_ids`. The slug turns `:` into `-`, so `x:y-ada`
/// and `x-y:ada` share `hosted-session-x-y-ada` when both `x` and `x-y` are vendors.
///
/// Only the shorter id's persona is refused (`x:y-ada`); every persona of the longer id (`x-y`)
/// is allowed. A plugin vendor `x` can therefore never disable a built-in or other longer-id
/// vendor, and the longer id wins as it does in [`vendor_of_session_id`]. A session of the longer
/// id that is already stored blocks the shorter one's through [`check_slug_free`], and the other
/// way round. `None` when the slug is the session id's alone.
fn colliding_session_id(vendor: &str, persona: &str, vendor_ids: &[String]) -> Option<String> {
    let tail = format!("{vendor}-{persona}");
    vendor_ids
        .iter()
        .filter(|o| o.len() > vendor.len())
        .find_map(|o| {
            let rest = tail.strip_prefix(o.as_str())?.strip_prefix('-')?;
            is_valid_persona(rest).then(|| format!("{o}:{rest}"))
        })
}

/// Refuse (409) a hosted session whose state slug already holds the record of a DIFFERENT
/// session id, before anything is written. Two ids can share a slug only through a hyphenated
/// vendor id (see [`colliding_session_id`]); the second write would replace the first's record.
fn check_slug_free<S: StateStore>(store: &S, session_id: &str) -> Result<(), LifecycleError> {
    let slug = hosted_session_slug(session_id);
    let Some(entry) =
        store.get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)?
    else {
        return Ok(());
    };
    if let StatePayload::Runtime(rt) = entry.payload
        && let Ok(stored) = serde_json::from_value::<HostedSessionRecord>(rt.value)
        && stored.session_id != session_id
    {
        return Err(LifecycleError::Conflict(format!(
            "session `{session_id}` would overwrite the hosted record of session `{}`: both use \
             the state slug `{slug}`",
            stored.session_id
        )));
    }
    Ok(())
}

impl SpinUpRequest {
    fn validate(&self) -> Result<ValidSpinUp, LifecycleError> {
        let persona = self.persona.trim().to_string();
        let slug_ok = persona
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if persona.is_empty()
            || persona.len() > MAX_PERSONA_LEN
            || !persona.as_bytes()[0].is_ascii_lowercase()
            || !slug_ok
        {
            return Err(invalid_field(
                "persona",
                "must be a lowercase slug (a-z, 0-9, '-'; starts with a letter; at most 36 characters)",
            ));
        }
        if fl_wire::dispatcher::is_owner_sender(&persona) {
            return Err(invalid_field(
                "persona",
                "must not be the owner's name (`owner`, `lmcfarlin`)",
            ));
        }
        let project = self.project.trim().to_string();
        if project.is_empty() {
            return Err(invalid_field("project", "must not be empty"));
        }
        validate_slug(&project)
            .map_err(|_| invalid_field("project", "must be a valid area slug"))?;
        let role = self.role.trim().to_ascii_lowercase();
        if !SPIN_UP_ROLES.contains(&role.as_str()) {
            return Err(LifecycleError::Invalid(format!(
                "role must be one of: {}",
                SPIN_UP_ROLES.join(", ")
            )));
        }
        let vendor = self.vendor.trim().to_ascii_lowercase();
        if lookup_vendor(&vendor).is_none() {
            return Err(LifecycleError::Invalid(format!(
                "vendor must be one of: {}",
                spin_up_vendor_ids().join(", ")
            )));
        }
        check_persona_fits_slug(&vendor, &persona)?;
        let model = trimmed_opt(&self.model);
        if let Some(model) = &model {
            check_text("model", model)?;
        }
        let node_id = trimmed_opt(&self.node_id).unwrap_or_else(|| DEFAULT_NODE_ID.to_string());
        if !node_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || node_id.len() > 64
        {
            return Err(invalid_field(
                "node_id",
                "must be at most 64 characters of letters, digits, '-', '_' or '.'",
            ));
        }
        let cwd = trimmed_opt(&self.cwd);
        if let Some(cwd) = &cwd {
            check_text("cwd", cwd)?;
        }
        Ok(ValidSpinUp {
            persona,
            project,
            role,
            vendor,
            model,
            node_id,
            hosted: self.hosted.unwrap_or(true),
            cwd,
        })
    }
}

/// `POST /api/v1/sessions/spin-up` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpinUpResponse {
    pub session_id: String,
    pub persona: String,
    pub project: String,
    pub role: String,
    pub vendor: String,
    pub model: Option<String>,
    pub node_id: String,
    pub hosted: bool,
    pub status: String,
    pub tmux_target: Option<String>,
    pub hydration_dispatched: bool,
    pub prompt: Option<String>,
    /// Whether this spin-up started the session's `tmux` runtime. `false` when the session is
    /// not hosted, runtime calls are off, or the start failed (tmux failed, or a session of the
    /// name already runs).
    #[serde(default)]
    pub runtime_spawned: bool,
}

/// Durable record of a hosted session, one `hosted-session-*` workspace runtime entry each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedSessionRecord {
    pub session_id: String,
    pub persona: String,
    pub project: String,
    pub role: String,
    pub vendor: String,
    pub model: Option<String>,
    pub node_id: String,
    pub hosted: bool,
    /// `running`, `decommissioning`, or `decommissioned`.
    pub status: String,
    pub restart_policy: String,
    pub restart_count: u32,
    pub tmux_target: Option<String>,
    pub cwd: Option<String>,
    pub hydration_dispatched: bool,
    /// Whether this lifecycle created the tmux session; only then does teardown kill it.
    #[serde(default)]
    pub runtime_spawned: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub decommission_prepared_at_ms: Option<u64>,
}

/// Slug of the runtime entry for `session_id` (`:` is not a slug character).
pub fn hosted_session_slug(session_id: &str) -> String {
    format!("{HOSTED_SLUG_PREFIX}{}", session_id.replace(':', "-"))
}

pub fn persist_hosted_session<S: StateStore>(
    store: &S,
    record: &HostedSessionRecord,
    by: &str,
) -> Result<(), LifecycleError> {
    let value = serde_json::to_value(record).map_err(|e| LifecycleError::Store(e.to_string()))?;
    // Never replace another session's record (see `check_slug_free`).
    check_slug_free(store, &record.session_id)?;
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: hosted_session_slug(&record.session_id),
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Runtime(RuntimeEntry {
            value,
            merge_mode: MergeMode::LastWriter,
        }),
        at: stamp::now(),
        by: by.to_string(),
    })?;
    Ok(())
}

/// Every hosted-session record, sorted by `session_id`. Entries that do not decode are skipped.
pub fn list_hosted_sessions<S: StateStore>(
    store: &S,
) -> Result<Vec<HostedSessionRecord>, LifecycleError> {
    let entries = store.list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))?;
    let mut records: Vec<HostedSessionRecord> = entries
        .into_iter()
        .filter(|e| e.slug.starts_with(HOSTED_SLUG_PREFIX))
        .filter_map(|e| match e.payload {
            StatePayload::Runtime(rt) => serde_json::from_value(rt.value).ok(),
            _ => None,
        })
        .collect();
    records.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(records)
}

/// Vendor and persona of a `vendor:persona` session id, when the vendor is a known one.
fn split_vendor_id(id: &str) -> Option<(String, &str)> {
    let (vendor, persona) = id.trim().split_once(':')?;
    let vendor = vendor.to_ascii_lowercase();
    lookup_vendor(&vendor)
        .is_some()
        .then_some((vendor, persona))
}

/// Whether `key` names `record`'s persona. A `vendor:persona` key only matches that vendor's
/// record, so `codex:ada` never reaches `claude:ada`; any other key is a bare persona.
fn persona_key_matches(record: &HostedSessionRecord, key: &str) -> bool {
    match split_vendor_id(key) {
        Some((vendor, persona)) => {
            record.vendor == vendor && record.persona.eq_ignore_ascii_case(persona)
        }
        None => record.persona.eq_ignore_ascii_case(key.trim()),
    }
}

/// The record whose `session_id` or `persona` is one of `keys`. An exact `session_id` match wins;
/// the persona fallback applies only when no record matches exactly.
pub fn find_hosted_session<S: StateStore>(
    store: &S,
    keys: &[&str],
) -> Result<Option<HostedSessionRecord>, LifecycleError> {
    let records = list_hosted_sessions(store)?;
    let exact = records
        .iter()
        .find(|r| keys.iter().any(|k| r.session_id == k.trim()));
    // A live record wins over a decommissioned one of the same persona (another vendor's),
    // which sorts first only by chance.
    let by_persona = || {
        let mut matches = records
            .iter()
            .filter(|r| keys.iter().any(|k| persona_key_matches(r, k)));
        let first = matches.next();
        first
            .filter(|r| is_live_hosted(r))
            .or_else(|| matches.find(|r| is_live_hosted(r)))
            .or(first)
    };
    Ok(exact.or_else(by_persona).cloned())
}

/// Record `model` on the hosted-session record that `keys` name (same lookup as
/// [`find_hosted_session`]), so the record and a later restart agree with an operator's model
/// switch. Returns the updated record, or `None` when no record matches.
pub fn set_hosted_session_model<S: StateStore>(
    store: &S,
    keys: &[&str],
    model: &str,
    by: &str,
) -> Result<Option<HostedSessionRecord>, LifecycleError> {
    let Some(mut record) = find_hosted_session(store, keys)? else {
        return Ok(None);
    };
    record.model = Some(model.to_string());
    record.updated_at_ms = now_ms();
    persist_hosted_session(store, &record, by)?;
    Ok(Some(record))
}

fn now_ms() -> u64 {
    stamp::now().unix_millis()
}

fn capitalize(persona: &str) -> String {
    let mut chars = persona.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// `AgentRole` for a spin-up role string; `ui-developer` and `strategist` are custom roles.
fn agent_role_from_label(label: &str) -> AgentRole {
    match label.trim().to_ascii_lowercase().as_str() {
        "lead" => AgentRole::Lead,
        "assistant" => AgentRole::Assistant,
        "lab-manager" | "lab_manager" => AgentRole::LabManager,
        other => match other.strip_prefix("custom:").unwrap_or(other) {
            slug if validate_slug(slug).is_ok() => AgentRole::Custom(slug.to_string()),
            _ => AgentRole::Assistant,
        },
    }
}

/// Inverse of [`agent_role_from_label`], in the spin-up spelling.
fn role_label_of(role: &AgentRole) -> String {
    match role {
        AgentRole::LabManager => "lab-manager".to_string(),
        other => reforge_role_label(other),
    }
}

fn prompt_for(display: &str, project: &str, role: &str) -> String {
    canonical_reforge_prompt(
        display,
        project,
        &reforge_role_label(&agent_role_from_label(role)),
    )
}

/// How [`check_registration`] treats the persona's own agent record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnAgent {
    /// As it stands: a spin-up.
    AsIs,
    /// As about to be retired: a remodel tears the persona's session down (which retires the
    /// agent) before it spins the persona up again.
    Retiring,
}

/// What [`register_spun_up_agent`] writes for a spin-up that passed [`check_registration`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Registration {
    email: Option<String>,
    extra_areas: Vec<String>,
    replace: bool,
}

/// The registry checks a spin-up of `v` must pass. Reads only: spin-up runs them just before it
/// registers, and a remodel runs them before its teardown, so a remodel the registry would refuse
/// is refused while the old session still runs.
fn check_registration<S: AgentRegistry>(
    store: &S,
    v: &ValidSpinUp,
    own: OwnAgent,
) -> Result<Registration, LifecycleError> {
    let role = agent_role_from_label(&v.role);
    let project_key = validate_slug(&v.project)
        .map_err(|_| invalid_field("project", "must be a valid area slug"))?;
    // The persona's own record never counts against it, so a sitting Lead may be spun up (or
    // remodelled) again in its own project.
    if role == AgentRole::Lead
        && let Some(lead) = store
            .list_agents(Some(&project_key), false)?
            .into_iter()
            .find(|a| a.role == AgentRole::Lead && a.name_key != v.persona)
    {
        // `replace` would silently retire the sitting Lead; make the caller decide.
        return Err(LifecycleError::Conflict(format!(
            "project `{project_key}` already has an active Lead: {}",
            lead.display_name
        )));
    }
    // An existing agent keeps its email and extra areas. Only a retired agent, or an active one
    // with the same project and role (a repeat spin-up), may be registered over: otherwise a
    // spin-up under a resident's persona would re-home or de-role that resident.
    Ok(match store.agent_by_name(&v.persona)? {
        None => Registration {
            email: None,
            extra_areas: Vec::new(),
            replace: false,
        },
        Some(agent) => {
            if own == OwnAgent::AsIs
                && agent.status == AgentStatus::Active
                && (agent.project != project_key || agent.role != role)
            {
                return Err(LifecycleError::Conflict(format!(
                    "agent `{}` is already active in project `{}` as {}",
                    v.persona,
                    agent.project,
                    role_label_of(&agent.role)
                )));
            }
            Registration {
                email: Some(agent.email),
                extra_areas: agent.extra_areas,
                replace: true,
            }
        }
    })
}

/// Register or revive `persona` as an Active agent, creating its coverage area.
fn register_spun_up_agent<S: AgentRegistry>(
    store: &S,
    v: &ValidSpinUp,
    by: &str,
) -> Result<AgentRecord, LifecycleError> {
    let Registration {
        email,
        extra_areas,
        replace,
    } = check_registration(store, v, OwnAgent::AsIs)?;
    let project_key = validate_slug(&v.project)
        .map_err(|_| invalid_field("project", "must be a valid area slug"))?;
    Ok(store.register_agent(RegisterAgentArgs {
        display_name: capitalize(&v.persona),
        email,
        project: project_key,
        role: agent_role_from_label(&v.role),
        extra_areas,
        create_area: true,
        replace,
        at: stamp::now(),
        by: by.to_string(),
    })?)
}

/// Longest a `tmux` call may run. Lifecycle calls run while the caller holds the store guard, so
/// a wedged tmux server must not hold it for good.
const TMUX_TIMEOUT: Duration = Duration::from_secs(10);

fn tmux_ok(args: &[&str]) -> bool {
    let mut cmd = Command::new("tmux");
    cmd.args(args);
    command_succeeds_within(&mut cmd, TMUX_TIMEOUT)
}

/// Run `cmd` with no stdio. `true` only when it exits 0 within `timeout`; a command still
/// running at the deadline is killed and reaped.
fn command_succeeds_within(cmd: &mut Command, timeout: Duration) -> bool {
    let Ok(mut child) = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Best-effort detached `tmux` session. `false` when tmux is absent or the session exists.
fn spawn_tmux_runtime(
    session: &str,
    window: &str,
    cwd: Option<&str>,
    launch: Option<&LaunchSpec>,
) -> bool {
    let args = tmux_new_session_args(session, window, cwd, launch);
    tmux_ok(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The `tmux new-session` arguments: a detached session, and with `launch` the command as an
/// argument vector after `--` (tmux 3.2 and later run it without a shell).
fn tmux_new_session_args(
    session: &str,
    window: &str,
    cwd: Option<&str>,
    launch: Option<&LaunchSpec>,
) -> Vec<String> {
    let mut args: Vec<String> = ["new-session", "-d", "-s", session, "-n", window]
        .map(String::from)
        .into();
    if let Some(dir) = cwd.filter(|d| Path::new(d).is_dir()) {
        args.extend(["-c".to_string(), dir.to_string()]);
    }
    if let Some(launch) = launch {
        for (key, value) in &launch.env {
            args.extend(["-e".to_string(), format!("{key}={value}")]);
        }
        args.push("--".to_string());
        args.extend(launch.argv.iter().cloned());
    }
    args
}

/// Kill a `tmux` session this lifecycle created; `true` when it is gone afterwards.
fn kill_tmux_runtime(session: &str) -> bool {
    let target = format!("={session}");
    tmux_ok(&["kill-session", "-t", &target]) || !tmux_session_exists(session)
}

/// `true` when a `tmux` session of exactly this name runs.
fn tmux_session_exists(session: &str) -> bool {
    tmux_ok(&["has-session", "-t", &format!("={session}")])
}

/// The route for `persona`, else any route that matches `fallback_session_id`, in `routes`.
fn route_in(
    routes: &RouteTable,
    persona: Option<&str>,
    fallback_session_id: &str,
) -> Option<VendorRoute> {
    let lookup_key = persona.unwrap_or(fallback_session_id);
    if let Some(entry) = routes.get(lookup_key) {
        return Some(entry.route.clone());
    }
    routes
        .entries
        .values()
        .find(|e| route_matches_session_id(&e.route, fallback_session_id))
        .map(|e| e.route.clone())
}

/// The cheap liveness checks discovery itself uses, nothing heavier: a Claude route's process
/// must exist (only where discovery checks `/proc`: no home override, host discovery on) and a
/// webhook route's socket must exist. Other routes have no check this cheap; a dispatch checks
/// more before it sends ([`undeliverable`]).
fn route_looks_live(route: &VendorRoute, home_override: Option<&Path>) -> bool {
    let check_proc = home_override.is_none()
        && std::env::var("FL_WIRE_DISABLE_HOST_DISCOVERY").as_deref() != Ok("1");
    match route {
        VendorRoute::Claude { pid, .. } if check_proc && *pid > 0 => {
            Path::new(&format!("/proc/{pid}")).exists()
        }
        VendorRoute::Webhook { socket_path } => socket_path.exists(),
        _ => true,
    }
}

/// Where [`lookup_route`] found a route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RouteLookup {
    /// From the saved route table, without a host scan. It may be stale.
    Saved(VendorRoute),
    /// From a host scan made for this lookup.
    Scanned(VendorRoute),
}

impl RouteLookup {
    pub(crate) fn into_route(self) -> VendorRoute {
        match self {
            Self::Saved(route) | Self::Scanned(route) => route,
        }
    }
}

/// The session a route lookup is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RouteQuery<'a> {
    /// The persona whose route is wanted. `None` looks the route up by `fallback_session_id`.
    pub(crate) persona: Option<&'a str>,
    /// The id a route must match when `persona` has no route.
    pub(crate) fallback_session_id: &'a str,
    /// The vendor session the caller resolved the request to (discovery's session id), when it
    /// resolved one. A saved route that names another session is stale: the persona opened a new
    /// conversation or thread after the route was saved, and the old one still accepts input.
    pub(crate) session_id: Option<&'a str>,
}

impl<'a> RouteQuery<'a> {
    /// A lookup by persona alone (a hosted-session id is not a vendor session id).
    pub(crate) fn persona(persona: &'a str, fallback_session_id: &'a str) -> Self {
        Self {
            persona: Some(persona),
            fallback_session_id,
            session_id: None,
        }
    }
}

/// `false` when `route` names a vendor session other than `session_id`. A route that names no
/// session (a webhook route, a Claude route with no session id) serves any.
fn route_serves_session(route: &VendorRoute, session_id: &str) -> bool {
    match route {
        VendorRoute::Webhook { .. } => true,
        VendorRoute::Claude {
            session_id: own, ..
        } if own.trim().is_empty() => true,
        _ => route_matches_session_id(route, session_id),
    }
}

/// The route for the query's persona, else any route that matches its fallback id. A route in
/// the saved table is used without a host scan only when all of these hold:
///
/// - the hub scanned the host and saved the scan to this route file within fl-wire's
///   [`ROUTE_REDISCOVERY_INTERVAL`] (the bound the daemon's dispatch uses), so a saved route is
///   never older than that;
/// - when the query names a vendor session, the route serves it ([`route_serves_session`]);
/// - the route passes the cheap liveness check ([`route_looks_live`]).
///
/// Otherwise the host is scanned, the scan saved, and the route taken from the scan.
///
/// [`ROUTE_REDISCOVERY_INTERVAL`]: fl_wire::dispatcher::ROUTE_REDISCOVERY_INTERVAL
pub(crate) fn lookup_route(
    home_override: Option<&Path>,
    query: RouteQuery<'_>,
) -> Option<RouteLookup> {
    if crate::discovery::saved_routes_fresh(home_override, Instant::now()) {
        let saved = RouteTable::load(crate::discovery::route_table_path(home_override).as_deref());
        if let Some(route) = route_in(&saved, query.persona, query.fallback_session_id)
            .filter(|route| {
                query
                    .session_id
                    .is_none_or(|id| route_serves_session(route, id))
            })
            .filter(|route| route_looks_live(route, home_override))
        {
            return Some(RouteLookup::Saved(route));
        }
    }
    rescan_route(home_override, query).map(RouteLookup::Scanned)
}

/// Scan the host for the session's route (never from the shared scan cache), save what the scan
/// learned, and look the route up in it.
pub(crate) fn rescan_route(
    home_override: Option<&Path>,
    query: RouteQuery<'_>,
) -> Option<VendorRoute> {
    let scanned = crate::discovery::scan_and_save_route_table(home_override);
    route_in(&scanned, query.persona, query.fallback_session_id)
}

/// The live route for the query's session (see [`lookup_route`]).
pub(crate) fn find_route(
    home_override: Option<&Path>,
    query: RouteQuery<'_>,
) -> Option<VendorRoute> {
    lookup_route(home_override, query).map(RouteLookup::into_route)
}

/// Why a route cannot take a delivery, found before anything is sent on it. These are the
/// refusals fl-wire's dispatch itself raises before it writes a byte, so a route refused here
/// has received nothing, and sending to another route instead can never deliver twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Undeliverable {
    /// The Claude process the route names is no longer running.
    ProcessGone { pid: u32 },
    /// The webhook socket file does not exist.
    NoSocket,
    /// No tmux pane or target the route names exists any more.
    PaneGone,
}

/// The pre-delivery check of [`dispatch_with_rediscovery`]: `Some` when `route` would refuse a
/// delivery before receiving anything. `None` does not promise the delivery works; it means
/// only that no cheap check proves it cannot.
///
/// - Claude: the route's pid (when it names one) has no `/proc` entry.
/// - Webhook: the socket file does not exist. The check never connects: fl-wire's webhook
///   contract is one connection per JSON line, so a probe connection would reach the receiver
///   as a delivery. A socket that exists but refuses is found by the real push instead (see
///   [`dispatch_with_rediscovery`]).
/// - Muse, Grok: the route names tmux targets and none of them exists. fl-wire would then run
///   the vendor's headless CLI on the old session id instead.
/// - Antigravity, Codex: no check; a stale route is bounded by [`lookup_route`] instead.
pub(crate) fn undeliverable(route: &VendorRoute) -> Option<Undeliverable> {
    match route {
        VendorRoute::Claude { pid, .. } if *pid != 0 => (!Path::new(&format!("/proc/{pid}"))
            .exists())
        .then_some(Undeliverable::ProcessGone { pid: *pid }),
        VendorRoute::Claude { .. } | VendorRoute::Agy { .. } | VendorRoute::Codex { .. } => None,
        VendorRoute::Webhook { socket_path } => socket_path
            .symlink_metadata()
            .is_err()
            .then_some(Undeliverable::NoSocket),
        VendorRoute::Muse {
            tmux_target,
            tmux_pane,
            ..
        }
        | VendorRoute::Grok {
            tmux_target,
            tmux_pane,
            ..
        } => {
            let mut targets = [tmux_target, tmux_pane]
                .into_iter()
                .flatten()
                .map(|t| t.trim())
                .filter(|t| !t.is_empty())
                .peekable();
            targets.peek()?;
            (!targets.any(tmux_target_exists)).then_some(Undeliverable::PaneGone)
        }
    }
}

/// `true` when tmux knows `target` (the binary fl-wire uses: `FL_WIRE_TMUX_BIN`, else `tmux`).
fn tmux_target_exists(target: &str) -> bool {
    let tmux =
        std::env::var_os("FL_WIRE_TMUX_BIN").map_or_else(|| PathBuf::from("tmux"), PathBuf::from);
    let mut cmd = Command::new(tmux);
    cmd.args(["display-message", "-p", "-t", target, "#{pane_id}"]);
    command_succeeds_within(&mut cmd, TMUX_TIMEOUT)
}

/// Send to the session's route; `None` when the session has no route.
///
/// Nothing `send` delivers is ever delivered again. A route from the saved table first passes
/// the pre-delivery check ([`undeliverable`]). When the check refuses it, nothing has been sent:
/// the host is scanned, the scan saved, and `send` runs on the route the scan found instead.
/// When the scan finds the same route or none, `send` runs on the saved route, so the caller
/// gets fl-wire's own refusal (or its headless fallback).
///
/// `send` runs a second time only when its first run on a saved route failed with
/// [`PushError::NotConnected`] (the receiver was never reached, so nothing was delivered) and a
/// scan then finds a different route. Every other failure of `send` is returned as it is, never
/// retried: by then part of the delivery may have landed.
pub(crate) fn dispatch_with_rediscovery<T>(
    home_override: Option<&Path>,
    query: RouteQuery<'_>,
    mut send: impl FnMut(&VendorRoute) -> Result<T, PushError>,
) -> Option<Result<T, String>> {
    let (route, from_saved) = match lookup_route(home_override, query)? {
        RouteLookup::Saved(saved) if undeliverable(&saved).is_some() => {
            (rescan_route(home_override, query).unwrap_or(saved), false)
        }
        RouteLookup::Saved(saved) => (saved, true),
        RouteLookup::Scanned(scanned) => (scanned, false),
    };
    Some(match send(&route) {
        Err(PushError::NotConnected(refused)) if from_saved => {
            match rescan_route(home_override, query).filter(|fresh| *fresh != route) {
                Some(fresh) => send(&fresh).map_err(String::from),
                None => Err(refused),
            }
        }
        sent => sent.map_err(String::from),
    })
}

/// Run `send` once on the route [`lookup_route`] finds, with no pre-delivery check and no second
/// route. For a session reset, which is several steps (a `/clear` or `/new`, then the reforge
/// prompt): once one step lands the session has changed, so a reset is never run again.
pub(crate) fn dispatch_once<T>(
    home_override: Option<&Path>,
    query: RouteQuery<'_>,
    send: impl FnOnce(&VendorRoute) -> Result<T, String>,
) -> Option<Result<T, String>> {
    let route = lookup_route(home_override, query)?.into_route();
    Some(send(&route))
}

/// Best-effort delivery of `prompt` to a live route for the session; `true` when delivered.
fn deliver_prompt(
    env: &LifecycleEnv<'_>,
    persona: &str,
    session_id: &str,
    by: &str,
    prompt: &str,
) -> bool {
    dispatch_with_rediscovery(
        env.home_override,
        RouteQuery::persona(persona, session_id),
        |route| push_operator_prompt_to_route(route, by, prompt),
    )
    .is_some_and(|delivered| delivered.is_ok())
}

/// Route-table files a teardown prunes: the override, or the default and legacy paths.
fn route_table_paths(home_override: Option<&Path>) -> Vec<PathBuf> {
    match home_override {
        Some(home) => vec![home.join(".cache").join("agent-bus").join("routes.json")],
        None => std::iter::once(default_routes_path())
            .chain(legacy_routes_path())
            .collect(),
    }
}

/// Drop every route keyed by one of `personas` or matching one of `session_ids`; returns the count.
/// Holds the hub's route-file lock, so a scan the hub saves at the same time cannot put a removed
/// route back (see [`crate::discovery::with_route_file_lock`] for the window left with the
/// `fl wire daemon`).
pub(crate) fn remove_routes(
    home_override: Option<&Path>,
    personas: &[&str],
    session_ids: &[&str],
) -> usize {
    crate::discovery::with_route_file_lock(|| {
        remove_routes_locked(home_override, personas, session_ids)
    })
}

fn remove_routes_locked(
    home_override: Option<&Path>,
    personas: &[&str],
    session_ids: &[&str],
) -> usize {
    let mut removed = 0;
    for path in route_table_paths(home_override) {
        if !path.is_file() {
            continue;
        }
        let mut table = RouteTable::load(Some(&path));
        let before = table.entries.len();
        table.entries.retain(|key, entry| {
            let by_key = personas.iter().any(|p| key.eq_ignore_ascii_case(p.trim()));
            let by_id = session_ids
                .iter()
                .any(|id| route_matches_session_id(&entry.route, id));
            !(by_key || by_id)
        });
        let dropped = before - table.entries.len();
        if dropped > 0 && table.save(Some(&path)).is_ok() {
            removed += dropped;
        }
    }
    removed
}

/// Spin up a session: register the agent, persist its record, open its tmux runtime, hydrate it.
///
/// The record is written before tmux starts, so a runtime this call opens always has a record
/// that teardown can find; the record is then updated with what actually started.
///
/// `caller` gates hosting an unmanaged agent: an Active agent with no LIVE hosted record for this
/// session (none, or a decommissioned one left from an earlier teardown) becomes hosted by the
/// spin-up, after which a peer's teardown would be allowed, so only the owner may do it. A peer
/// may still spin up a new or retired persona, or re-spin a session whose hosted record is live.
pub fn spin_up<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    req: &SpinUpRequest,
    by: &str,
    caller: Caller,
) -> Result<SpinUpResponse, LifecycleError> {
    let v = req.validate()?;
    // Before any side effect: an API vendor whose key is missing would register an agent that
    // can never run.
    check_api_key(env, &v.vendor)?;
    // An HTTP API vendor has no CLI to host, so no tmux window is ever opened for it.
    let has_runtime = vendor_has_runtime(&v.vendor);
    let session_id = format!("{}:{}", v.vendor, v.persona);
    // Before the agent is registered: another session's record may hold this session's slug.
    check_slug_free(store, &session_id)?;
    // The unfiltered record carries `created_at_ms`, `restart_count` and `runtime_spawned` over;
    // the gate below looks only at whether it is LIVE.
    let previous =
        find_hosted_session(store, &[&session_id])?.filter(|r| r.session_id == session_id);
    // Order: this gate runs first, before the Lead and project/role Conflict checks in
    // `register_spun_up_agent`, because a refusal here reveals nothing about the agent's
    // project or role. It sits before every side effect.
    require_owner_for_unmanaged(
        store,
        caller,
        &v.persona,
        previous.as_ref(),
        OwnerGate::SpinUp,
    )?;
    register_spun_up_agent(store, &v, by)?;

    let display = capitalize(&v.persona);
    let tmux_target = has_runtime.then(|| format!("{display}:{}", v.vendor));
    let prompt = prompt_for(&display, &v.project, &v.role);

    let now = now_ms();
    let mut record = HostedSessionRecord {
        session_id: session_id.clone(),
        persona: v.persona.clone(),
        project: v.project.clone(),
        role: v.role.clone(),
        vendor: v.vendor.clone(),
        model: v.model.clone(),
        node_id: v.node_id.clone(),
        hosted: v.hosted,
        status: STATUS_RUNNING.to_string(),
        restart_policy: "on-failure".to_string(),
        restart_count: previous.as_ref().map_or(0, |r| r.restart_count),
        tmux_target: tmux_target.clone(),
        cwd: v.cwd.clone(),
        hydration_dispatched: false,
        // A decommissioned record's runtime is gone; the tmux session now under the name, if any,
        // is someone else's and a later teardown must not kill it.
        runtime_spawned: has_runtime
            && previous
                .as_ref()
                .is_some_and(|r| r.runtime_spawned && is_live_hosted(r)),
        created_at_ms: previous.as_ref().map_or(now, |r| r.created_at_ms),
        updated_at_ms: now,
        decommission_prepared_at_ms: None,
    };
    persist_hosted_session(store, &record, by)?;

    let launch = launch_spec_for(env, &v.vendor, &v.persona, &prompt);
    let spawned = has_runtime
        && v.hosted
        && env.spawn_runtime
        && env
            .runtime
            .spawn(&display, &v.vendor, v.cwd.as_deref(), launch.as_ref());
    // A runtime launched with the prompt as its first input has the prompt already.
    let hydration_dispatched =
        (spawned && launch.is_some()) || deliver_prompt(env, &v.persona, &session_id, by, &prompt);
    record.runtime_spawned |= spawned;
    record.hydration_dispatched = hydration_dispatched;
    record.updated_at_ms = now_ms();
    // A runtime may have started: the next discovery must scan the host again.
    crate::discovery::invalidate_route_discovery_cache();
    if let Err(e) = persist_hosted_session(store, &record, by) {
        if spawned {
            // The record cannot learn about this runtime, so teardown could never stop it.
            env.runtime.kill(&display);
        }
        return Err(e);
    }

    Ok(SpinUpResponse {
        runtime_spawned: spawned,
        session_id,
        persona: v.persona,
        project: v.project,
        role: v.role,
        vendor: v.vendor,
        model: v.model,
        node_id: v.node_id,
        hosted: v.hosted,
        status: STATUS_RUNNING.to_string(),
        tmux_target,
        hydration_dispatched,
        prompt: Some(prompt),
    })
}

/// Decommission phase named by the request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecommissionMode {
    Prepare,
    Teardown,
}

impl DecommissionMode {
    pub fn parse(raw: &str) -> Result<Self, LifecycleError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "prepare" => Ok(Self::Prepare),
            "teardown" => Ok(Self::Teardown),
            _ => Err(LifecycleError::Invalid(
                "mode must be \"prepare\" or \"teardown\"".to_string(),
            )),
        }
    }
}

/// `POST /api/v1/sessions/:id/decommission` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecommissionResponse {
    pub session_id: String,
    pub persona: Option<String>,
    pub mode: DecommissionMode,
    pub status: String,
    pub handshake_dispatched: bool,
    pub unregistered: bool,
    pub runtime_stopped: bool,
    pub prompt: Option<String>,
}

/// A session a decommission acts on: its hosted record when one exists, else the caller's guess.
#[derive(Debug, Clone)]
pub struct SessionTarget {
    pub raw_id: String,
    pub session_id: String,
    pub persona: String,
    /// The caller's canonical id for the session, which can differ from `raw_id` and
    /// `session_id`; queues and routes may be keyed by it.
    pub discovered_id: String,
    pub hosted: Option<HostedSessionRecord>,
}

impl SessionTarget {
    /// `discovered` is the caller's `(canonical id, persona)` for `raw_id`, when it has one.
    pub fn resolve<S: StateStore>(
        store: &S,
        raw_id: &str,
        discovered: Option<(String, Option<String>)>,
    ) -> Result<Self, LifecycleError> {
        let raw = raw_id.trim();
        let (disc_id, disc_persona) = discovered.unwrap_or_else(|| (raw.to_string(), None));
        // `vendor:persona` ids name their persona even when no record exists.
        let derived = raw.rsplit(':').next().unwrap_or(raw).to_ascii_lowercase();
        let persona_guess = disc_persona
            .map(|p| p.rsplit(':').next().unwrap_or(&p).to_ascii_lowercase())
            .unwrap_or(derived);
        // A `vendor:persona` id is exact: it never falls back to another vendor's record of the
        // same persona, so the bare persona is not offered as a lookup key for it.
        let mut keys = vec![raw, disc_id.as_str()];
        if split_vendor_id(raw).is_none() {
            keys.push(persona_guess.as_str());
        }
        let hosted = find_hosted_session(store, &keys)?;
        Ok(match hosted {
            Some(rec) => Self {
                raw_id: raw.to_string(),
                session_id: rec.session_id.clone(),
                persona: rec.persona.clone(),
                discovered_id: disc_id,
                hosted: Some(rec),
            },
            None => {
                // Reject a guess no agent can have, before anything looks it up or acts on it.
                if validate_slug(&persona_guess).is_err() {
                    return Err(LifecycleError::Invalid(format!(
                        "session id `{raw}` does not name a valid persona"
                    )));
                }
                Self {
                    raw_id: raw.to_string(),
                    session_id: disc_id.clone(),
                    persona: persona_guess,
                    discovered_id: disc_id,
                    hosted: None,
                }
            }
        })
    }

    /// Distinct ids that can key this session in a queue or route table.
    fn keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = Vec::with_capacity(4);
        for key in [
            self.session_id.as_str(),
            self.raw_id.as_str(),
            self.discovered_id.as_str(),
            self.persona.as_str(),
        ] {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }
}

/// Vendor named by the start of a session id (`claude:ada`, `muse-<uuid>`, `kimi-api:ada`), if
/// registered: the LONGEST registered id that the session id begins with and that is followed by
/// `:` or `-` (or ends the id), so a hyphenated id and an id that is a prefix of another both
/// resolve to the right vendor.
fn vendor_of_session_id(session_id: &str) -> Option<String> {
    longest_vendor_prefix(&spin_up_vendor_ids(), session_id)
}

fn longest_vendor_prefix(vendors: &[String], session_id: &str) -> Option<String> {
    vendors
        .iter()
        .filter(|v| {
            session_id
                .strip_prefix(v.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '-']))
        })
        .max_by_key(|v| v.len())
        .cloned()
}

/// Project and role for a session: its hosted record, else its agent registration.
fn project_and_role<S: AgentRegistry>(
    store: &S,
    target: &SessionTarget,
) -> Result<Option<(String, String)>, LifecycleError> {
    if let Some(rec) = &target.hosted {
        return Ok(Some((rec.project.clone(), rec.role.clone())));
    }
    Ok(store
        .agent_by_name(&target.persona)?
        .map(|a| (a.project, role_label_of(&a.role))))
}

/// The hosted record for `target`, adopting an unmanaged session when none exists.
fn record_for_target<S: AgentRegistry>(
    store: &S,
    target: &SessionTarget,
) -> Result<HostedSessionRecord, LifecycleError> {
    if let Some(rec) = &target.hosted {
        return Ok(rec.clone());
    }
    let (project, role) = project_and_role(store, target)?
        .unwrap_or_else(|| ("unknown".to_string(), "assistant".to_string()));
    let now = now_ms();
    Ok(HostedSessionRecord {
        session_id: target.session_id.clone(),
        persona: target.persona.clone(),
        project,
        role,
        vendor: vendor_of_session_id(&target.session_id).unwrap_or_else(|| "unknown".to_string()),
        model: None,
        node_id: DEFAULT_NODE_ID.to_string(),
        hosted: false,
        status: STATUS_RUNNING.to_string(),
        // A session this lifecycle never started is not restarted by it.
        restart_policy: "never".to_string(),
        restart_count: 0,
        tmux_target: None,
        cwd: None,
        hydration_dispatched: false,
        runtime_spawned: false,
        created_at_ms: now,
        updated_at_ms: now,
        decommission_prepared_at_ms: None,
    })
}

/// Phase one: ask the agent to save its work and hand off. Nothing is stopped or unregistered.
///
/// `handshake_dispatched` is `false` when no live route took the prompt; the caller may queue it.
/// `caller` gates adopting a session that has no hosted record (owner only).
pub fn prepare_decommission<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    by: &str,
    caller: Caller,
) -> Result<DecommissionResponse, LifecycleError> {
    // Prepare adopts an unmanaged session into a hosted record, which teardown then accepts from
    // any caller, so the adoption gate applies here too, before any side effect. Order: this
    // gate runs before the decommissioned 409 below. A Peer is refused with 403 when the agent
    // is Active and unmanaged (no record, or only a stale decommissioned one), which reveals
    // nothing; the 409 is reached only by the Owner, or by a Peer when no Active agent exists.
    require_owner_for_unmanaged(
        store,
        caller,
        &target.persona,
        target.hosted.as_ref(),
        OwnerGate::Decommission,
    )?;
    if target.hosted.as_ref().is_some_and(is_decommissioned) {
        return Err(decommissioned_conflict(&target.session_id));
    }
    let Some((project, role)) = project_and_role(store, target)? else {
        return Err(LifecycleError::NotFound(format!(
            "no hosted session or agent record for `{}`",
            target.raw_id
        )));
    };
    let display = capitalize(&target.persona);
    let prompt = synthesize_decommission_prompt(
        &display,
        &project,
        &reforge_role_label(&agent_role_from_label(&role)),
    );
    let handshake_dispatched =
        deliver_prompt(env, &target.persona, &target.session_id, by, &prompt);

    let now = now_ms();
    let mut record = record_for_target(store, target)?;
    record.status = STATUS_DECOMMISSIONING.to_string();
    record.decommission_prepared_at_ms = Some(now);
    record.updated_at_ms = now;
    persist_hosted_session(store, &record, by)?;

    Ok(DecommissionResponse {
        session_id: record.session_id,
        persona: Some(target.persona.clone()),
        mode: DecommissionMode::Prepare,
        status: STATUS_DECOMMISSIONING.to_string(),
        handshake_dispatched,
        unregistered: false,
        runtime_stopped: false,
        prompt: Some(prompt),
    })
}

/// Who asks for a spin-up or teardown. A session with no live hosted record is adopted by a
/// teardown, which retires whatever agent its id names, and an Active agent with no live hosted
/// record is adopted by a spin-up, so only the owner may adopt one
/// (see `require_owner_for_unmanaged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    Owner,
    Peer,
}

fn is_decommissioned(record: &HostedSessionRecord) -> bool {
    record.status == STATUS_DECOMMISSIONED
}

/// A hosted record that still manages its session. A decommissioned record is history: it is kept
/// after teardown, and the persona may since have been registered again outside the lifecycle.
fn is_live_hosted(record: &HostedSessionRecord) -> bool {
    !is_decommissioned(record)
}

/// The action a Peer asks for, which decides what a missing or stale hosted record means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerGate {
    /// Hosting a persona: needs the Owner only when it adopts an Active agent.
    SpinUp,
    /// Prepare or teardown: needs the Owner for any session with no record, and for an Active
    /// agent behind a decommissioned one.
    Decommission,
}

/// The one decision on whether a non-Owner caller needs Owner authority for a target. The Owner
/// always passes. For anyone else a LIVE hosted record always passes. Without one (none, or only
/// a decommissioned record), the action adopts whatever the persona now names: a spin-up is
/// refused when an Active agent exists, a prepare or teardown when no record exists or an Active
/// agent exists. A new or Retired persona still passes a spin-up, and a repeated teardown of a
/// decommissioned record whose agent is Retired or absent stays harmless and allowed.
///
/// Every gate calls this before its first side effect.
fn require_owner_for_unmanaged<S: AgentRegistry>(
    store: &S,
    caller: Caller,
    persona: &str,
    hosted: Option<&HostedSessionRecord>,
    gate: OwnerGate,
) -> Result<(), LifecycleError> {
    if caller == Caller::Owner || hosted.is_some_and(is_live_hosted) {
        return Ok(());
    }
    let active = store
        .agent_by_name(persona)?
        .is_some_and(|a| a.status == AgentStatus::Active);
    match (gate, hosted) {
        (OwnerGate::SpinUp, _) if active => Err(LifecycleError::Forbidden(format!(
            "only Owner authority may spin up `{persona}`, an active agent with no live hosted \
             session record"
        ))),
        (OwnerGate::Decommission, None) => Err(LifecycleError::Forbidden(format!(
            "only Owner authority may decommission `{persona}`, which has no hosted session record"
        ))),
        (OwnerGate::Decommission, Some(_)) if active => Err(LifecycleError::Forbidden(format!(
            "only Owner authority may decommission `{persona}`, an active agent whose hosted \
             session record is decommissioned"
        ))),
        _ => Ok(()),
    }
}

fn decommissioned_conflict(session_id: &str) -> LifecycleError {
    LifecycleError::Conflict(format!(
        "hosted session `{session_id}` is decommissioned; spin it up again"
    ))
}

/// `tmux` session name of a `Session:window` target.
fn tmux_session_name(tmux_target: &str) -> &str {
    tmux_target.split(':').next().unwrap_or(tmux_target)
}

/// The other live hosted records whose runtime is `record`'s tmux session. The tmux session name
/// is the capitalised persona, so `claude:ada` and `codex:ada` share session `Ada`; killing it
/// would end the other, still-live session.
fn runtime_sharers<S: StateStore>(
    store: &S,
    record: &HostedSessionRecord,
) -> Result<Vec<HostedSessionRecord>, LifecycleError> {
    let Some(name) = record.tmux_target.as_deref().map(tmux_session_name) else {
        return Ok(Vec::new());
    };
    Ok(list_hosted_sessions(store)?
        .into_iter()
        .filter(|r| {
            r.session_id != record.session_id
                && !is_decommissioned(r)
                && r.tmux_target.as_deref().map(tmux_session_name) == Some(name)
        })
        .collect())
}

/// The gate `teardown_session` runs before its first effect. A caller that wants to act on a
/// teardown's behalf (such as archiving the session's artifacts) asks this first, so a refused
/// teardown never causes a side effect and the decision stays in one place.
pub fn authorize_teardown<S: AgentRegistry>(
    store: &S,
    target: &SessionTarget,
    caller: Caller,
) -> Result<(), LifecycleError> {
    require_owner_for_unmanaged(
        store,
        caller,
        &target.persona,
        target.hosted.as_ref(),
        OwnerGate::Decommission,
    )
}

/// Phase two: drop queued prompts and routes, stop the runtime this lifecycle started, retire
/// the agent, and mark the record `decommissioned`. Safe to repeat.
///
/// `caller` gates adopting a session that has no hosted record (owner only). `clear_queue` is
/// called with each id the session may be queued under. Everything that can fail on a lookup
/// runs before the first side effect.
pub fn teardown_session<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    by: &str,
    caller: Caller,
    mut clear_queue: impl FnMut(&str),
) -> Result<DecommissionResponse, LifecycleError> {
    authorize_teardown(store, target, caller)?;
    let mut record = record_for_target(store, target)?;
    let agent = store.agent_by_name(&target.persona)?;
    let sharing = runtime_sharers(store, &record)?;
    let shared_runtime = record.runtime_spawned && !sharing.is_empty();
    if shared_runtime {
        // The runtime stays up for the other sessions, so ownership of it moves to them: the
        // last one torn down then stops it.
        for mut other in sharing {
            other.runtime_spawned = true;
            other.updated_at_ms = now_ms();
            persist_hosted_session(store, &other, by)?;
        }
        record.runtime_spawned = false;
    }

    for key in target.keys() {
        clear_queue(key);
    }
    remove_routes(
        env.home_override,
        &[target.persona.as_str()],
        &[target.session_id.as_str(), target.raw_id.as_str()],
    );

    // `true` only for a runtime this lifecycle started and has now stopped. A runtime it did not
    // start, one still running for the sessions that share it, or none at all (runtime calls are
    // off) is not reported as stopped.
    let runtime_stopped = match (&record.tmux_target, record.runtime_spawned) {
        (Some(tmux_target), true) if env.spawn_runtime => {
            let gone = env.runtime.kill(tmux_session_name(tmux_target));
            // The runtime is gone, so the decommissioned record must not claim it any more.
            record.runtime_spawned &= !gone;
            gone
        }
        _ => false,
    };
    // Its routes are gone and its runtime may be: the next discovery must scan the host again.
    crate::discovery::invalidate_route_discovery_cache();

    if let Some(agent) = agent
        && agent.status == AgentStatus::Active
    {
        store.retire_agent(&agent.id, stamp::now(), by)?;
    }

    record.status = STATUS_DECOMMISSIONED.to_string();
    record.updated_at_ms = now_ms();
    persist_hosted_session(store, &record, by)?;

    Ok(DecommissionResponse {
        session_id: record.session_id,
        persona: Some(target.persona.clone()),
        mode: DecommissionMode::Teardown,
        status: STATUS_DECOMMISSIONED.to_string(),
        handshake_dispatched: false,
        unregistered: true,
        runtime_stopped,
        prompt: None,
    })
}

/// `POST /api/v1/hosted-sessions/:id/restart` response: the updated record plus the reforge prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartResponse {
    #[serde(flatten)]
    pub record: HostedSessionRecord,
    pub prompt: String,
}

/// Count a restart: reopen a missing tmux session, redeliver the reforge prompt, mark running.
/// A `decommissioned` session is refused (409); spin it up again instead.
pub fn restart_hosted_session<S: StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    id: &str,
    by: &str,
) -> Result<RestartResponse, LifecycleError> {
    let mut record = find_hosted_session(store, &[id])?
        .ok_or_else(|| LifecycleError::NotFound(format!("no hosted session `{id}`")))?;
    if is_decommissioned(&record) {
        // Its agent is retired; restarting would mark it running with no agent behind it.
        return Err(decommissioned_conflict(&record.session_id));
    }
    let display = capitalize(&record.persona);
    let prompt = prompt_for(&display, &record.project, &record.role);
    let launch = launch_spec_for(env, &record.vendor, &record.persona, &prompt);
    let mut launched_with_prompt = false;
    if record.hosted
        && vendor_has_runtime(&record.vendor)
        && env.spawn_runtime
        && env.runtime.spawn(
            &display,
            &record.vendor,
            record.cwd.as_deref(),
            launch.as_ref(),
        )
    {
        record.runtime_spawned = true;
        // A runtime launched with the prompt as its first input has the prompt already.
        launched_with_prompt = launch.is_some();
    }
    record.hydration_dispatched = launched_with_prompt
        || deliver_prompt(env, &record.persona, &record.session_id, by, &prompt);
    // A runtime may have reopened: the next discovery must scan the host again.
    crate::discovery::invalidate_route_discovery_cache();
    record.restart_count += 1;
    record.status = STATUS_RUNNING.to_string();
    record.decommission_prepared_at_ms = None;
    record.updated_at_ms = now_ms();
    persist_hosted_session(store, &record, by)?;
    Ok(RestartResponse { record, prompt })
}

/// Remodel phase named by the request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemodelMode {
    /// Send the pre-decommission handoff prompt; nothing is stopped.
    Prepare,
    /// Tear the session down and spin the persona up on the new vendor.
    Execute,
}

impl RemodelMode {
    /// `None` is `execute`.
    pub fn parse(raw: Option<&str>) -> Result<Self, LifecycleError> {
        match raw.map(|r| r.trim().to_ascii_lowercase()).as_deref() {
            None | Some("execute") => Ok(Self::Execute),
            Some("prepare") => Ok(Self::Prepare),
            _ => Err(LifecycleError::Invalid(
                "mode must be \"prepare\" or \"execute\"".to_string(),
            )),
        }
    }
}

/// `POST /api/v1/sessions/:id/remodel` body. Every field but `vendor` may be left out: `model`
/// then is the new vendor's default (the old vendor's model never carries over), and `cwd`,
/// `node_id` and `hosted` keep the hosted session's values.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemodelRequest {
    /// A vendor in the vendor registry ([`spin_up_vendor_ids`]).
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub model: Option<String>,
    /// `prepare` or `execute` (the default).
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub hosted: Option<bool>,
}

/// `POST /api/v1/sessions/:id/remodel` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemodelResponse {
    pub mode: RemodelMode,
    /// The session after the remodel; for `prepare`, the session being prepared.
    pub session_id: String,
    pub previous_session_id: String,
    pub persona: String,
    pub project: String,
    pub role: String,
    pub from_vendor: String,
    pub vendor: String,
    pub model: Option<String>,
    pub node_id: String,
    pub hosted: bool,
    pub cwd: Option<String>,
    /// `decommissioning` after `prepare`, `running` after `execute`.
    pub status: String,
    /// `prepare`: whether a live route took the handoff prompt.
    pub handshake_dispatched: bool,
    /// `execute`: whether a live route took the new session's reforge prompt.
    pub hydration_dispatched: bool,
    /// `execute`: whether the old session's runtime is gone.
    pub runtime_stopped: bool,
    pub tmux_target: Option<String>,
    /// `prepare`: the handoff prompt; `execute`: the new session's reforge prompt.
    pub prompt: Option<String>,
    /// `execute`: slug of the workspace handoff entry that records the remodel.
    pub handoff: Option<String>,
    /// Problems that did not stop the remodel.
    pub warnings: Vec<String>,
}

/// Slug of the workspace handoff entry a remodel of `persona` writes.
pub fn remodel_handoff_slug(persona: &str) -> String {
    format!("remodel-{persona}")
}

/// A remodel that passed every check that can run before the old session is touched.
struct RemodelPlan {
    mode: RemodelMode,
    persona: String,
    from_session_id: String,
    from_vendor: String,
    from_model: Option<String>,
    /// The new session, validated.
    to: ValidSpinUp,
    /// The spin-up that puts the old session back, when its vendor is one spin-up can start.
    restore: Option<SpinUpRequest>,
}

impl RemodelPlan {
    fn spin_up_request(&self) -> SpinUpRequest {
        SpinUpRequest {
            persona: self.to.persona.clone(),
            project: self.to.project.clone(),
            role: self.to.role.clone(),
            vendor: self.to.vendor.clone(),
            model: self.to.model.clone(),
            node_id: Some(self.to.node_id.clone()),
            hosted: Some(self.to.hosted),
            cwd: self.to.cwd.clone(),
        }
    }
}

/// A runtime directory that does not exist would silently start the new runtime somewhere else
/// (`tmux new-session -c` is skipped for a missing directory), losing the persona's worktree.
fn check_runtime_cwd(v: &ValidSpinUp) -> Result<(), LifecycleError> {
    match &v.cwd {
        Some(cwd) if v.hosted && !Path::new(cwd).is_dir() => Err(LifecycleError::Invalid(format!(
            "cwd `{cwd}` is not a directory on this host"
        ))),
        _ => Ok(()),
    }
}

/// A hosted remodel starts its new runtime under the same tmux session name (the capitalised
/// persona), so the teardown must stop the old one first. Refused (409) when a session of the
/// name runs that the teardown will not stop: one this lifecycle did not start (no hosted
/// record, or `runtime_spawned` unset), or one other live hosted sessions share. Otherwise the
/// new runtime could not start, or the old vendor would keep running under the new registry.
fn check_old_runtime_stoppable<S: StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    to: &ValidSpinUp,
) -> Result<(), LifecycleError> {
    let name = capitalize(&target.persona);
    // An API vendor opens no tmux window, so the old name being taken costs it nothing.
    if !to.hosted
        || !vendor_has_runtime(&to.vendor)
        || !env.spawn_runtime
        || !env.runtime.exists(&name)
    {
        return Ok(());
    }
    let why = match target.hosted.as_ref() {
        None => "no hosted record started it".to_string(),
        Some(r) if !r.runtime_spawned => format!("`{}` did not start it", r.session_id),
        Some(r) if r.tmux_target.as_deref().map(tmux_session_name) != Some(name.as_str()) => {
            format!("`{}` started another tmux session", r.session_id)
        }
        Some(r) => {
            let sharers = runtime_sharers(store, r)?;
            if sharers.is_empty() {
                return Ok(());
            }
            let ids: Vec<&str> = sharers.iter().map(|s| s.session_id.as_str()).collect();
            format!("it also runs {}", ids.join(", "))
        }
    };
    Err(LifecycleError::Conflict(format!(
        "tmux session `{name}` runs and the teardown of `{}` would not stop it ({why}); stop it \
         first, or remodel with `hosted: false`",
        target.session_id
    )))
}

/// Whether [`remodel_session`] would refuse this remodel before its first side effect, with
/// nothing done: the same checks, error texts and statuses (see `plan_remodel`, whose result this
/// drops, so the two cannot drift). It only reads: the store, the runtime directory, the key
/// probe and, when runtime calls are on, whether a tmux session of the persona's name runs.
pub(crate) fn remodel_precheck<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    req: &RemodelRequest,
    caller: Caller,
) -> Result<(), LifecycleError> {
    plan_remodel(store, env, target, req, caller).map(drop)
}

/// Every check a remodel can make before its first side effect, in the order the siblings make
/// them: the teardown gate (403), a decommissioned record (409), no record at all (404), the
/// request fields as spin-up validates them (400), the runtime directory (400), the registry
/// checks spin-up makes with the persona's own agent counted as retired (409), a non-Owner
/// remodel that could not be restored (409), and an old runtime the teardown cannot stop (409).
/// It has no side effect: [`remodel_precheck`] is this, with the plan dropped.
fn plan_remodel<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    req: &RemodelRequest,
    caller: Caller,
) -> Result<RemodelPlan, LifecycleError> {
    let mode = RemodelMode::parse(req.mode.as_deref())?;
    authorize_teardown(store, target, caller)?;
    if target.hosted.as_ref().is_some_and(is_decommissioned) {
        return Err(decommissioned_conflict(&target.session_id));
    }
    let Some((project, role)) = project_and_role(store, target)? else {
        return Err(LifecycleError::NotFound(format!(
            "no hosted session or agent record for `{}`",
            target.raw_id
        )));
    };
    let old = target.hosted.as_ref();
    let from_vendor = old.map_or_else(
        || vendor_of_session_id(&target.session_id).unwrap_or_else(|| "unknown".to_string()),
        |r| r.vendor.clone(),
    );
    let old_node = old.map_or_else(|| DEFAULT_NODE_ID.to_string(), |r| r.node_id.clone());
    let old_cwd = old.and_then(|r| r.cwd.clone());
    let from_model = old.and_then(|r| r.model.clone());
    let to = SpinUpRequest {
        persona: target.persona.clone(),
        project: project.clone(),
        role: role.clone(),
        vendor: req.vendor.clone(),
        model: req.model.clone(),
        node_id: req.node_id.clone().or_else(|| Some(old_node.clone())),
        // The new session keeps the old one's `hosted`; with no record, spin-up's default.
        hosted: req.hosted.or(old.map(|r| r.hosted)),
        cwd: req.cwd.clone().or_else(|| old_cwd.clone()),
    }
    .validate()?;
    // Before the teardown: a remodel onto an API vendor with no key would retire the old
    // session for nothing.
    check_api_key(env, &to.vendor)?;
    check_slug_free(store, &format!("{}:{}", to.vendor, to.persona))?;
    check_runtime_cwd(&to)?;
    check_registration(store, &to, OwnAgent::Retiring)?;
    // The restore puts back what was there: a session with no record was not hosted, and a
    // restore must not turn it into a managed one.
    let restore = lookup_vendor(&from_vendor)
        .is_some()
        .then(|| SpinUpRequest {
            persona: target.persona.clone(),
            project,
            role,
            vendor: from_vendor.clone(),
            model: from_model.clone(),
            node_id: Some(old_node),
            hosted: Some(old.is_some_and(|r| r.hosted)),
            cwd: old_cwd,
        });
    // With no restore, a failed spin-up leaves the persona down; only the Owner may take that
    // risk (the remodel then warns that no restore is possible).
    if restore.is_none() && caller != Caller::Owner {
        return Err(LifecycleError::Conflict(format!(
            "`{}` runs on vendor `{from_vendor}`, which spin-up cannot start, so a failed remodel \
             could not be restored; only Owner authority may remodel it",
            target.session_id
        )));
    }
    if mode == RemodelMode::Execute {
        check_old_runtime_stoppable(store, env, target, &to)?;
    }
    Ok(RemodelPlan {
        mode,
        persona: target.persona.clone(),
        from_session_id: target.session_id.clone(),
        from_vendor,
        from_model,
        to,
        restore,
    })
}

/// How a remodel ended, as its handoff entry records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemodelOutcome {
    /// Written before the teardown; still there means the remodel never finished.
    InProgress,
    Remodeled,
    /// The new session failed and the old vendor's session was spun up again.
    Restored,
    /// The new session failed and so did the restore: the persona runs nowhere.
    Down,
}

impl RemodelOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Remodeled => "remodeled",
            Self::Restored => "restored",
            Self::Down => "down",
        }
    }
}

/// What a remodel's handoff entry records beyond the plan.
#[derive(Default)]
struct RemodelReport<'a> {
    /// The new session, once it runs.
    to_session_id: Option<&'a str>,
    /// The old vendor's session, once a restore brought it back.
    restored_session_id: Option<&'a str>,
    agent: Option<&'a AgentRecord>,
    error: Option<&'a str>,
    restore_error: Option<&'a str>,
}

/// Write the remodel's durable workspace handoff entry (`remodel-<persona>`). Each write is a
/// new revision of the one entry, so its history shows every step the remodel reached.
fn record_remodel_handoff<S: StateStore>(
    store: &S,
    plan: &RemodelPlan,
    outcome: RemodelOutcome,
    report: &RemodelReport<'_>,
    by: &str,
) -> Result<String, LifecycleError> {
    let slug = remodel_handoff_slug(&plan.persona);
    let to = &plan.to;
    let notes = serde_json::json!({
        "kind": "remodel",
        "outcome": outcome.as_str(),
        "persona": plan.persona,
        "from_session_id": plan.from_session_id,
        "to_session_id": report.to_session_id,
        "restored_session_id": report.restored_session_id,
        "from_vendor": plan.from_vendor,
        "to_vendor": to.vendor,
        "from_model": plan.from_model,
        "model": to.model,
        "project": to.project,
        "role": to.role,
        "cwd": to.cwd,
        "node_id": to.node_id,
        "hosted": to.hosted,
        "email": report.agent.map(|a| a.email.as_str()),
        "extra_areas": report.agent.map(|a| a.extra_areas.as_slice()),
        "error": report.error,
        "restore_error": report.restore_error,
    });
    let mut blockers = Vec::new();
    blockers.extend(report.error.map(|e| format!("remodel: {e}")));
    blockers.extend(report.restore_error.map(|e| format!("restore: {e}")));
    let (shipped, in_flight, next_steps) = match outcome {
        RemodelOutcome::InProgress => (
            Vec::new(),
            vec![format!(
                "tear down `{}` and spin `{}` up on {}",
                plan.from_session_id, plan.persona, to.vendor
            )],
            Vec::new(),
        ),
        RemodelOutcome::Remodeled => (
            vec![format!(
                "`{}` runs on {} as `{}`",
                plan.persona,
                to.vendor,
                report.to_session_id.unwrap_or_default()
            )],
            Vec::new(),
            Vec::new(),
        ),
        RemodelOutcome::Restored => (
            vec![format!(
                "`{}` was restored on {}",
                plan.persona, plan.from_vendor
            )],
            Vec::new(),
            vec![format!(
                "fix the cause, then remodel `{}` again",
                plan.persona
            )],
        ),
        RemodelOutcome::Down => (
            Vec::new(),
            Vec::new(),
            vec![format!(
                "spin `{}` up again: it runs nowhere after the failed remodel",
                plan.persona
            )],
        ),
    };
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: slug.clone(),
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Handoff(HandoffEntry {
            summary: format!(
                "Remodel of {}: {} -> {} ({})",
                plan.persona,
                plan.from_vendor,
                to.vendor,
                outcome.as_str()
            ),
            shipped,
            in_flight,
            blockers,
            next_steps,
            notes: Some(notes.to_string()),
        }),
        at: stamp::now(),
        by: by.to_string(),
    })?;
    Ok(slug)
}

/// The spin-up step of a remodel; tests replace it to make the new session or the restore fail.
type SpinUpFn<'f, S> = dyn FnMut(
        &S,
        &LifecycleEnv<'_>,
        &SpinUpRequest,
        &str,
        Caller,
    ) -> Result<SpinUpResponse, LifecycleError>
    + 'f;

/// Move a session to another vendor subscription: an atomic decommission plus spin-up that keeps
/// the persona, project, role, email, extra areas, worktree (`cwd`) and node, and records a
/// FerroState handoff (`remodel-<persona>`, workspace scope).
///
/// - `prepare` runs every check, then [`prepare_decommission`]: the session is asked to save its
///   state and tidy its worktree, and its record turns `decommissioning`. Nothing is stopped.
/// - `execute` (with or without an earlier `prepare`) runs every check before touching anything
///   (see `plan_remodel`), writes the handoff, tears the session down ([`teardown_session`]),
///   and spins the persona up on the new vendor ([`spin_up`]). When anything after the handoff
///   fails, the old vendor and model are spun up again in the old directory, and the error
///   ([`LifecycleError::Partial`], 502) says whether that restore worked or the persona is down.
///   The handoff records the outcome either way.
///
/// A same-vendor remodel ends with the same session id, `running`, on the new model and `cwd`.
///
/// Concurrency: the caller must hold the store exclusively for the whole call, so a remodel is
/// never interleaved with another lifecycle call for the same persona. The hub does: every
/// handler holds its `HubStoreGuard` (one process-wide store lock) from target resolution to
/// the end of the call. `fl agent remodel` is a separate process and is not serialized with the
/// hub, as `fl agent spin-up` and `decommission` are not.
pub fn remodel_session<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    req: &RemodelRequest,
    by: &str,
    caller: Caller,
    clear_queue: impl FnMut(&str),
) -> Result<RemodelResponse, LifecycleError> {
    remodel_with(
        store,
        env,
        target,
        req,
        by,
        caller,
        clear_queue,
        &mut |store, env, req, by, caller| {
            #[cfg(test)]
            if FAIL_SPIN_UP_VENDORS.with_borrow(|v| v.contains(&req.vendor)) {
                return Err(LifecycleError::Store(format!(
                    "{} binary is missing",
                    req.vendor
                )));
            }
            spin_up(store, env, req, by, caller)
        },
    )
}

#[cfg(test)]
thread_local! {
    /// Vendors whose spin-up every [`remodel_session`] on this thread fails, for tests that
    /// reach a remodel only through the hub's handler.
    pub(crate) static FAIL_SPIN_UP_VENDORS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[allow(clippy::too_many_arguments)]
fn remodel_with<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    req: &RemodelRequest,
    by: &str,
    caller: Caller,
    clear_queue: impl FnMut(&str),
    spin: &mut SpinUpFn<'_, S>,
) -> Result<RemodelResponse, LifecycleError> {
    let plan = plan_remodel(store, env, target, req, caller)?;
    let to = &plan.to;
    if plan.mode == RemodelMode::Prepare {
        let out = prepare_decommission(store, env, target, by, caller)?;
        return Ok(RemodelResponse {
            mode: RemodelMode::Prepare,
            session_id: out.session_id.clone(),
            previous_session_id: out.session_id,
            persona: plan.persona.clone(),
            project: to.project.clone(),
            role: to.role.clone(),
            from_vendor: plan.from_vendor.clone(),
            vendor: to.vendor.clone(),
            model: to.model.clone(),
            node_id: to.node_id.clone(),
            hosted: to.hosted,
            cwd: to.cwd.clone(),
            status: out.status,
            handshake_dispatched: out.handshake_dispatched,
            hydration_dispatched: false,
            runtime_stopped: false,
            tmux_target: target.hosted.as_ref().and_then(|r| r.tmux_target.clone()),
            prompt: out.prompt,
            handoff: None,
            warnings: Vec::new(),
        });
    }

    // Read before the teardown retires it: the spin-up revives this record, and the handoff
    // names what it must keep.
    let agent = store.agent_by_name(&plan.persona)?;
    let report = RemodelReport {
        agent: agent.as_ref(),
        ..RemodelReport::default()
    };
    // What the new session's record was before the remodel, so a failed spin-up that wrote it
    // can be undone. Read before any side effect.
    let to_session_id = format!("{}:{}", to.vendor, plan.persona);
    let to_before =
        find_hosted_session(store, &[&to_session_id])?.filter(|r| r.session_id == to_session_id);
    // The last step that can fail with nothing changed: if it does, the session still runs.
    let handoff = record_remodel_handoff(store, &plan, RemodelOutcome::InProgress, &report, by)?;

    let spun = teardown_session(store, env, target, by, caller, clear_queue)
        .map_err(|e| format!("teardown of `{}` failed: {e}", plan.from_session_id))
        .and_then(|torn| {
            spin(store, env, &plan.spin_up_request(), by, caller)
                .and_then(|out| require_runtime(env, out))
                .map(|out| (torn, out))
                .map_err(|e| format!("spin-up on {} failed: {e}", to.vendor))
        });
    // A runtime may have stopped or started on any path: the next discovery must scan again.
    crate::discovery::invalidate_route_discovery_cache();

    let error = match spun {
        Ok((torn, out)) => {
            let mut warnings = Vec::new();
            if plan.restore.is_none() {
                warnings.push(format!(
                    "no restore was possible: spin-up cannot start vendor `{}`, so a failure \
                     would have left `{}` down",
                    plan.from_vendor, plan.persona
                ));
            }
            let report = RemodelReport {
                to_session_id: Some(&out.session_id),
                ..report
            };
            if let Err(e) =
                record_remodel_handoff(store, &plan, RemodelOutcome::Remodeled, &report, by)
            {
                warnings.push(format!("handoff `{handoff}` was not updated: {e}"));
            }
            return Ok(RemodelResponse {
                mode: RemodelMode::Execute,
                session_id: out.session_id,
                previous_session_id: plan.from_session_id.clone(),
                persona: out.persona,
                project: out.project,
                role: out.role,
                from_vendor: plan.from_vendor.clone(),
                vendor: out.vendor,
                model: out.model,
                node_id: out.node_id,
                hosted: out.hosted,
                cwd: to.cwd.clone(),
                status: out.status,
                handshake_dispatched: false,
                hydration_dispatched: out.hydration_dispatched,
                runtime_stopped: torn.runtime_stopped,
                tmux_target: out.tmux_target,
                prompt: out.prompt,
                handoff: Some(handoff),
                warnings,
            });
        }
        Err(error) => error,
    };

    // A spin-up that failed after its first persist left the new session's record `running`
    // with nothing behind it: put that record back as it was (marked decommissioned when it did
    // not exist), so only the restored session claims the persona.
    let mut cleanup_error = None;
    if to_session_id != plan.from_session_id
        && let Err(e) = undo_new_record(store, &to_session_id, to_before, by)
    {
        cleanup_error = Some(format!(
            "; the record of `{to_session_id}` was not reset: {e}"
        ));
    }

    // The persona may now be down and retired: put the old session back. The restore spins up
    // exactly what the caller was allowed to tear down, nothing the request chose, so it runs
    // with Owner authority: a half-registered new session must not lock its own restore out.
    // The key check is skipped (see [`RestoreKeys`]): the restore must not fail on a key that
    // vanished since the session first ran, which would leave the persona down for nothing.
    let restore_env = LifecycleEnv {
        key_probe: &RestoreKeys,
        ..*env
    };
    let restored = match &plan.restore {
        Some(restore) => spin(store, &restore_env, restore, by, Caller::Owner)
            .and_then(|out| require_runtime(env, out))
            .map(|out| out.session_id)
            .map_err(|e| e.to_string()),
        None => Err(format!(
            "vendor `{}` is not one spin-up can start",
            plan.from_vendor
        )),
    };
    crate::discovery::invalidate_route_discovery_cache();
    let (outcome, restore_error, mut message) = match &restored {
        Ok(session_id) => (
            RemodelOutcome::Restored,
            None,
            format!(
                "remodel of `{}` failed: {error}; the persona was restored on {} as `{session_id}`",
                plan.persona, plan.from_vendor
            ),
        ),
        Err(e2) => (
            RemodelOutcome::Down,
            Some(e2.as_str()),
            format!(
                "remodel of `{}` failed: {error}; restore on {} ALSO failed: {e2}; the persona is \
                 DOWN",
                plan.persona, plan.from_vendor
            ),
        ),
    };
    let report = RemodelReport {
        restored_session_id: restored.as_deref().ok(),
        error: Some(&error),
        restore_error,
        ..report
    };
    if let Some(e) = cleanup_error {
        message.push_str(&e);
    }
    if let Err(e) = record_remodel_handoff(store, &plan, outcome, &report, by) {
        message.push_str(&format!("; handoff `{handoff}` was not updated: {e}"));
    }
    Err(LifecycleError::Partial {
        message,
        restored_session_id: restored.ok(),
    })
}

/// A spin-up that should have started a runtime and did not is a failed spin-up: the persona
/// would be registered on a runtime that does not run (or, when the name was taken, on someone
/// else's).
fn require_runtime(
    env: &LifecycleEnv<'_>,
    out: SpinUpResponse,
) -> Result<SpinUpResponse, LifecycleError> {
    if env.spawn_runtime && out.hosted && vendor_has_runtime(&out.vendor) && !out.runtime_spawned {
        let session = out
            .tmux_target
            .as_deref()
            .map_or(out.persona.as_str(), tmux_session_name);
        return Err(LifecycleError::Store(format!(
            "its tmux runtime did not start (tmux failed, or session `{session}` already runs)"
        )));
    }
    Ok(out)
}

/// Put the record of `session_id` back as it was before a failed remodel's spin-up wrote it:
/// `before` when there was one, else the record marked decommissioned.
fn undo_new_record<S: StateStore>(
    store: &S,
    session_id: &str,
    before: Option<HostedSessionRecord>,
    by: &str,
) -> Result<(), LifecycleError> {
    let Some(mut now) =
        find_hosted_session(store, &[session_id])?.filter(|r| r.session_id == session_id)
    else {
        return Ok(());
    };
    match before {
        Some(before) if before != now => persist_hosted_session(store, &before, by),
        Some(_) => Ok(()),
        None if is_decommissioned(&now) => Ok(()),
        None => {
            now.status = STATUS_DECOMMISSIONED.to_string();
            now.updated_at_ms = now_ms();
            persist_hosted_session(store, &now, by)
        }
    }
}

#[cfg(test)]
mod remodel_tests;

#[cfg(test)]
mod vendor_registry_tests;

#[cfg(test)]
mod muse_launch_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;
    use fl_core::at::At;
    use tempfile::TempDir;

    fn req(persona: &str, role: &str, vendor: &str) -> SpinUpRequest {
        SpinUpRequest {
            persona: persona.into(),
            project: "FerroLoop".into(),
            role: role.into(),
            vendor: vendor.into(),
            ..SpinUpRequest::default()
        }
    }

    fn env(home: &TempDir) -> LifecycleEnv<'_> {
        LifecycleEnv {
            home_override: Some(home.path()),
            spawn_runtime: false,
            runtime: &TmuxRuntime,
            key_probe: &ProvisionedKeys,
        }
    }

    fn webhook_route_file(home: &Path, persona: &str) -> PathBuf {
        let mut routes = RouteTable::default();
        routes.upsert(
            persona,
            VendorRoute::Webhook {
                socket_path: home.join("none.sock"),
            },
            At::from_unix_millis(1),
        );
        let path = home.join(".cache").join("agent-bus").join("routes.json");
        routes.save(Some(&path)).unwrap();
        path
    }

    #[test]
    fn validation_rejects_each_bad_field() {
        let bad_persona = |p: &str| req(p, "assistant", "claude").validate().unwrap_err();
        for p in [
            "",
            "  ",
            "Felicity",
            "fel icity",
            "fel_icity",
            "-fel",
            "1fel",
            "fel:x",
        ] {
            assert!(
                matches!(bad_persona(p), LifecycleError::Invalid(m) if m.starts_with("persona"))
            );
        }
        assert!(bad_persona(&"a".repeat(MAX_PERSONA_LEN + 1)).status() == 400);

        let mut r = req("felicity", "assistant", "claude");
        r.project = "  ".into();
        assert!(
            matches!(r.validate().unwrap_err(), LifecycleError::Invalid(m) if m.starts_with("project"))
        );
        r.project = "no spaces".into();
        assert_eq!(r.validate().unwrap_err().status(), 400);

        for role in ["", "owner", "lab_manager", "custom:x"] {
            let e = req("felicity", role, "claude").validate().unwrap_err();
            assert!(
                matches!(e, LifecycleError::Invalid(m) if m.starts_with("role")),
                "{role}"
            );
        }
        for vendor in ["", "gemini", "Claude2"] {
            let e = req("felicity", "lead", vendor).validate().unwrap_err();
            assert!(
                matches!(e, LifecycleError::Invalid(m) if m.starts_with("vendor")),
                "{vendor}"
            );
        }
        let mut r = req("felicity", "lead", "claude");
        r.node_id = Some("bad node".into());
        assert_eq!(r.validate().unwrap_err().status(), 400);
        r.node_id = None;
        r.model = Some("a\nb".into());
        assert_eq!(r.validate().unwrap_err().status(), 400);
    }

    #[test]
    fn validation_defaults_and_accepts_every_listed_role_and_vendor() {
        for role in SPIN_UP_ROLES {
            for vendor in SPIN_UP_VENDORS {
                req("felicity", role, vendor).validate().unwrap();
            }
        }
        let v = req(" felicity ", "UI-Developer", "Claude")
            .validate()
            .unwrap();
        assert_eq!(
            (v.persona.as_str(), v.role.as_str(), v.vendor.as_str()),
            ("felicity", "ui-developer", "claude")
        );
        assert_eq!(v.node_id, DEFAULT_NODE_ID);
        assert!(v.hosted);
        assert_eq!(v.model, None);
        assert_eq!(v.cwd, None);
    }

    #[test]
    fn role_labels_round_trip_through_agent_roles() {
        for label in SPIN_UP_ROLES {
            assert_eq!(role_label_of(&agent_role_from_label(label)), label);
        }
        assert_eq!(
            agent_role_from_label("ui-developer"),
            AgentRole::Custom("ui-developer".into())
        );
        assert_eq!(
            agent_role_from_label("strategist"),
            AgentRole::Custom("strategist".into())
        );
        assert!(prompt_for("Faye", "FerroLoop", "strategist").contains("as the strategist."));
        assert_eq!(agent_role_from_label("lab-manager"), AgentRole::LabManager);
    }

    #[test]
    fn hosted_session_slug_is_a_valid_state_slug_for_every_vendor() {
        for vendor in SPIN_UP_VENDORS {
            let id = format!("{vendor}:{}", "a".repeat(MAX_PERSONA_LEN));
            let slug = hosted_session_slug(&id);
            assert_eq!(validate_slug(&slug).unwrap(), slug);
        }
        assert_eq!(
            hosted_session_slug("claude:ada"),
            "hosted-session-claude-ada"
        );
    }

    #[test]
    fn hosted_record_round_trips_through_the_store_and_lists_sorted() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        for (persona, vendor) in [("zed", "codex"), ("ada", "muse"), ("bob", "claude")] {
            spin_up(
                &store,
                &env,
                &req(persona, "assistant", vendor),
                "owner",
                Caller::Owner,
            )
            .unwrap();
        }
        let listed = list_hosted_sessions(&store).unwrap();
        let ids: Vec<&str> = listed.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, ["claude:bob", "codex:zed", "muse:ada"]);

        let rec = &listed[0];
        assert_eq!(rec.status, "running");
        assert_eq!(rec.restart_policy, "on-failure");
        assert_eq!(rec.restart_count, 0);
        assert_eq!(rec.tmux_target.as_deref(), Some("Bob:claude"));
        assert!(rec.hosted && !rec.runtime_spawned && !rec.hydration_dispatched);
        assert_eq!(rec.decommission_prepared_at_ms, None);

        let json = serde_json::to_value(rec).unwrap();
        assert_eq!(
            serde_json::from_value::<HostedSessionRecord>(json).unwrap(),
            *rec
        );
        assert_eq!(
            find_hosted_session(&store, &["bob"])
                .unwrap()
                .unwrap()
                .session_id,
            "claude:bob"
        );
        assert!(find_hosted_session(&store, &["nobody"]).unwrap().is_none());
    }

    #[test]
    fn spin_up_registers_agent_and_area_and_is_idempotent() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let out = spin_up(
            &store,
            &env,
            &req("felicity", "ui-developer", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(out.session_id, "claude:felicity");
        assert_eq!(out.tmux_target.as_deref(), Some("Felicity:claude"));
        assert_eq!(out.status, "running");
        let prompt = out.prompt.as_deref().unwrap();
        assert!(prompt.starts_with(
            "Take the name of Felicity and take up residence in FerroLoop/ as the ui-developer."
        ));

        let agent = store.agent_by_name("felicity").unwrap().unwrap();
        assert_eq!(agent.status, AgentStatus::Active);
        assert_eq!(agent.role, AgentRole::Custom("ui-developer".into()));
        assert!(store.get_area("ferroloop").unwrap().is_some());

        let again = spin_up(
            &store,
            &env,
            &req("felicity", "ui-developer", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(again.session_id, out.session_id);
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
    }

    #[test]
    fn spin_up_refuses_to_displace_the_sitting_lead() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ferris", "lead", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let err = spin_up(
            &store,
            &env,
            &req("felix", "lead", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap_err();
        assert_eq!(err.status(), 409);
        assert!(store.agent_by_name("felix").unwrap().is_none());
        // The sitting Lead may be spun up again.
        spin_up(
            &store,
            &env,
            &req("ferris", "lead", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
    }

    #[test]
    fn decommission_prompt_names_each_required_step() {
        let p = synthesize_decommission_prompt("Felicity", "FerroLoop", "assistant");
        for needle in [
            "Felicity",
            "Notes/FerroLoop/STATE.md",
            "fl state handoff",
            "fl state checkpoint",
            "worktrees",
            "fl wire send",
            "agent-msg send",
            "fl wire read --ack",
            "agent-msg read --ack",
            "Lead",
        ] {
            assert!(p.contains(needle), "missing {needle}: {p}");
        }
        assert!(synthesize_decommission_prompt("Ferris", "FerroLoop", "lead").contains("jarvis"));
        assert!(!p.contains("jarvis"));
    }

    #[test]
    fn prepare_marks_decommissioning_and_keeps_the_agent_and_routes() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let routes = webhook_route_file(home.path(), "felicity");

        let target = SessionTarget::resolve(&store, "claude:felicity", None).unwrap();
        let out = prepare_decommission(&store, &env, &target, "owner", Caller::Owner).unwrap();
        assert_eq!(out.mode, DecommissionMode::Prepare);
        assert_eq!(out.status, "decommissioning");
        assert!(!out.unregistered && !out.runtime_stopped);
        assert!(out.prompt.unwrap().contains("decommission"));
        // The webhook socket does not exist, so nothing took the prompt.
        assert!(!out.handshake_dispatched);

        let rec = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        assert_eq!(rec.status, "decommissioning");
        assert!(rec.decommission_prepared_at_ms.is_some());
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Active
        );
        assert!(RouteTable::load(Some(&routes)).get("felicity").is_some());
    }

    #[test]
    fn spin_up_restart_and_teardown_each_drop_the_shared_route_scan() {
        use crate::discovery::cost_counters::ROUTE_CACHE_INVALIDATIONS;
        let count = || ROUTE_CACHE_INVALIDATIONS.with(std::cell::Cell::get);
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let before = count();
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(count() - before, 1, "spin-up");
        restart_hosted_session(&store, &env, "claude:felicity", "owner").unwrap();
        assert_eq!(count() - before, 2, "restart");
        let target = SessionTarget::resolve(&store, "claude:felicity", None).unwrap();
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(count() - before, 3, "teardown");
    }

    #[test]
    fn prepare_of_an_unknown_session_is_not_found() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let target = SessionTarget::resolve(&store, "ghost", None).unwrap();
        let err =
            prepare_decommission(&store, &env(&home), &target, "owner", Caller::Owner).unwrap_err();
        assert_eq!(err.status(), 404);
    }

    #[test]
    fn teardown_retires_agent_removes_routes_clears_queues_and_is_repeatable() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        webhook_route_file(home.path(), "felicity");

        let target = SessionTarget::resolve(&store, "claude:felicity", None).unwrap();
        let mut cleared = Vec::new();
        let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |k| {
            cleared.push(k.to_string())
        })
        .unwrap();
        assert_eq!(out.status, "decommissioned");
        // Runtime calls are off: no runtime was started, so none is reported stopped.
        assert!(out.unregistered && !out.runtime_stopped && !out.handshake_dispatched);
        assert_eq!(out.prompt, None);
        assert!(cleared.contains(&"claude:felicity".to_string()));
        assert!(cleared.contains(&"felicity".to_string()));

        let routes = home.path().join(".cache/agent-bus/routes.json");
        assert!(RouteTable::load(Some(&routes)).get("felicity").is_none());
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Retired
        );
        let rec = find_hosted_session(&store, &["felicity"]).unwrap().unwrap();
        assert_eq!(rec.status, "decommissioned");

        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    }

    #[test]
    fn teardown_adopts_an_unmanaged_session() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let target = SessionTarget::resolve(
            &store,
            "claude-abc",
            Some(("claude-abc".into(), Some("Ghost".into()))),
        )
        .unwrap();
        assert_eq!(target.persona, "ghost");
        let out =
            teardown_session(&store, &env(&home), &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(out.session_id, "claude-abc");
        let rec = find_hosted_session(&store, &["claude-abc"])
            .unwrap()
            .unwrap();
        assert_eq!(
            (rec.vendor.as_str(), rec.status.as_str(), rec.hosted),
            ("claude", "decommissioned", false)
        );
        assert_eq!(rec.restart_policy, "never");
    }

    #[test]
    fn restart_counts_and_revives_a_prepared_record() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let target = SessionTarget::resolve(&store, "claude:felicity", None).unwrap();
        prepare_decommission(&store, &env, &target, "owner", Caller::Owner).unwrap();

        let out = restart_hosted_session(&store, &env, "claude:felicity", "owner").unwrap();
        assert_eq!(out.record.restart_count, 1);
        assert_eq!(out.record.status, "running");
        assert_eq!(out.record.decommission_prepared_at_ms, None);
        assert!(out.prompt.contains("take up residence in FerroLoop/"));
        let out = restart_hosted_session(&store, &env, "felicity", "owner").unwrap();
        assert_eq!(out.record.restart_count, 2);
        assert_eq!(
            restart_hosted_session(&store, &env, "nope", "owner")
                .unwrap_err()
                .status(),
            404
        );
    }

    fn decommissioned_felicity(store: &MemStore, env: &LifecycleEnv<'_>) -> SessionTarget {
        spin_up(
            store,
            env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let target = SessionTarget::resolve(store, "claude:felicity", None).unwrap();
        teardown_session(store, env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        SessionTarget::resolve(store, "claude:felicity", None).unwrap()
    }

    #[test]
    fn restart_and_prepare_of_a_decommissioned_session_conflict_and_change_nothing() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let target = decommissioned_felicity(&store, &env);
        let before = find_hosted_session(&store, &["claude:felicity"]).unwrap();

        let err = restart_hosted_session(&store, &env, "claude:felicity", "owner").unwrap_err();
        assert!(matches!(err, LifecycleError::Conflict(_)), "{err}");
        assert_eq!(err.status(), 409);
        let err = prepare_decommission(&store, &env, &target, "owner", Caller::Owner).unwrap_err();
        assert_eq!(err.status(), 409);

        assert_eq!(
            find_hosted_session(&store, &["claude:felicity"]).unwrap(),
            before
        );
        assert_eq!(before.unwrap().status, "decommissioned");
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Retired
        );
        // Teardown stays repeatable, and spin-up revives the session and its agent.
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(
            restart_hosted_session(&store, &env, "claude:felicity", "owner")
                .unwrap()
                .record
                .status,
            "running"
        );
    }

    fn register(store: &MemStore, persona: &str, project: &str, role: AgentRole) {
        store
            .register_agent(RegisterAgentArgs {
                display_name: capitalize(persona),
                email: Some(format!("{persona}@example.org")),
                project: project.into(),
                role,
                extra_areas: vec!["notes".into()],
                create_area: true,
                replace: false,
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap();
    }

    #[test]
    fn idempotent_respin_keeps_the_agents_email_and_extra_areas() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        register(&store, "felicity", "ferroloop", AgentRole::Assistant);
        spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let agent = store.agent_by_name("felicity").unwrap().unwrap();
        assert_eq!(agent.email, "felicity@example.org");
        assert_eq!(agent.extra_areas, ["notes"]);
        assert_eq!(agent.status, AgentStatus::Active);

        // A retired agent comes back in its new project, still with its email and extra areas.
        store
            .retire_agent(&agent.id, stamp::now(), "owner")
            .unwrap();
        let mut other = req("felicity", "ui-developer", "claude");
        other.project = "elsewhere".into();
        spin_up(&store, &env, &other, "owner", Caller::Owner).unwrap();
        let agent = store.agent_by_name("felicity").unwrap().unwrap();
        assert_eq!(
            (agent.status, agent.project.as_str(), agent.email.as_str()),
            (AgentStatus::Active, "elsewhere", "felicity@example.org")
        );
        assert_eq!(agent.extra_areas, ["notes"]);
    }

    #[test]
    fn spin_up_of_an_active_agent_in_another_project_or_role_conflicts() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        register(&store, "jarvis", "ferroloop", AgentRole::LabManager);
        let before = store.agent_by_name("jarvis").unwrap().unwrap();

        let mut other_project = req("jarvis", "lab-manager", "claude");
        other_project.project = "elsewhere".into();
        for r in [
            req("jarvis", "assistant", "claude"),
            req("jarvis", "ui-developer", "codex"),
            other_project,
        ] {
            // The owner reaches the conflict check; a peer is refused earlier (Forbidden), which
            // is covered by the peer_spin_up_* tests.
            let err = spin_up(&store, &env, &r, "owner", Caller::Owner).unwrap_err();
            assert!(matches!(err, LifecycleError::Conflict(_)), "{err}");
        }
        assert_eq!(store.agent_by_name("jarvis").unwrap().unwrap(), before);
        assert!(list_hosted_sessions(&store).unwrap().is_empty());
        assert!(store.get_area("elsewhere").unwrap().is_none());
    }

    fn assert_unmanaged_agent_untouched<S: AgentRegistry + StateStore>(
        store: &S,
        before: &AgentRecord,
    ) {
        assert_eq!(&store.agent_by_name("jarvis").unwrap().unwrap(), before);
        assert!(list_hosted_sessions(store).unwrap().is_empty());
    }

    #[test]
    fn peer_spin_up_of_an_unmanaged_active_agent_is_forbidden_and_changes_nothing() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        // A live webhook route for jarvis: any prompt delivery would connect to this socket.
        webhook_route_file(home.path(), "jarvis");
        let listener =
            std::os::unix::net::UnixListener::bind(home.path().join("none.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        register(&store, "jarvis", "ferroloop", AgentRole::LabManager);
        let before = store.agent_by_name("jarvis").unwrap().unwrap();

        // Same project and role: without the gate this is the idempotent re-spin that hosts it.
        let mut same = req("jarvis", "lab-manager", "claude");
        same.project = "ferroloop".into();
        for r in [same, req("jarvis", "assistant", "codex")] {
            let err = spin_up(&store, &env, &r, "peer", Caller::Peer).unwrap_err();
            assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
            assert_eq!(err.status(), 403);
        }
        assert_unmanaged_agent_untouched(&store, &before);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "no prompt may be delivered"
        );

        // The owner may host it, and keeps its email and extra areas.
        let mut same = req("jarvis", "lab-manager", "claude");
        same.project = "ferroloop".into();
        let out = spin_up(&store, &env, &same, "owner", Caller::Owner).unwrap();
        assert_eq!(out.session_id, "claude:jarvis");
        assert!(
            listener.accept().is_ok(),
            "the owner's spin-up delivers the prompt"
        );
        let after = store.agent_by_name("jarvis").unwrap().unwrap();
        assert_eq!(after.status, AgentStatus::Active);
        assert_eq!(after.email, before.email);
        assert_eq!(after.extra_areas, before.extra_areas);
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
    }

    #[test]
    fn peer_spin_up_still_works_for_new_retired_and_already_hosted_personas() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);

        // A persona with no agent record.
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        // A re-spin of a session that already has a hosted record.
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);

        // A retired agent, whether or not it was ever hosted.
        register(&store, "bob", "ferroloop", AgentRole::Assistant);
        let bob = store.agent_by_name("bob").unwrap().unwrap();
        store.retire_agent(&bob.id, stamp::now(), "owner").unwrap();
        spin_up(
            &store,
            &env,
            &req("bob", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        assert_eq!(
            store.agent_by_name("bob").unwrap().unwrap().status,
            AgentStatus::Active
        );
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 2);
    }

    #[test]
    fn respin_preserves_created_at_restart_count_and_runtime_ownership() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let mut rec = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        rec.restart_count = 3;
        rec.runtime_spawned = true;
        rec.created_at_ms = 1;
        persist_hosted_session(&store, &rec, "owner").unwrap();

        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let again = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                again.restart_count,
                again.created_at_ms,
                again.runtime_spawned
            ),
            (3, 1, true)
        );
        assert!(again.updated_at_ms > 1);
    }

    #[test]
    fn peer_teardown_of_an_unmanaged_session_is_forbidden_and_changes_nothing() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        register(&store, "jarvis", "ferroloop", AgentRole::LabManager);
        let routes = webhook_route_file(home.path(), "jarvis");
        let target = SessionTarget::resolve(&store, "claude:jarvis", None).unwrap();
        assert!(target.hosted.is_none());

        let mut cleared = Vec::new();
        let err = teardown_session(&store, &env, &target, "peer", Caller::Peer, |k| {
            cleared.push(k.to_string())
        })
        .unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
        assert_eq!(err.status(), 403);
        assert!(cleared.is_empty());
        assert!(RouteTable::load(Some(&routes)).get("jarvis").is_some());
        assert_eq!(
            store.agent_by_name("jarvis").unwrap().unwrap().status,
            AgentStatus::Active
        );
        assert!(list_hosted_sessions(&store).unwrap().is_empty());

        // The owner may adopt it.
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(
            store.agent_by_name("jarvis").unwrap().unwrap().status,
            AgentStatus::Retired
        );
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
    }

    #[test]
    fn peer_teardown_of_a_hosted_session_still_works() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        let out = teardown_session(&store, &env, &target, "peer", Caller::Peer, |_| {}).unwrap();
        assert_eq!(out.status, "decommissioned");
        assert_eq!(
            store.agent_by_name("ada").unwrap().unwrap().status,
            AgentStatus::Retired
        );
    }

    #[test]
    fn peer_prepare_of_an_unmanaged_session_is_forbidden_and_adopts_nothing() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        register(&store, "jarvis", "ferroloop", AgentRole::LabManager);
        let routes = webhook_route_file(home.path(), "jarvis");
        // A live listener on the route's socket would receive any prompt delivered.
        let listener =
            std::os::unix::net::UnixListener::bind(home.path().join("none.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let target = SessionTarget::resolve(&store, "claude:jarvis", None).unwrap();
        assert!(target.hosted.is_none());

        let err = prepare_decommission(&store, &env, &target, "peer", Caller::Peer).unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
        assert_eq!(err.status(), 403);
        assert!(list_hosted_sessions(&store).unwrap().is_empty());
        assert!(listener.accept().is_err(), "no prompt may be delivered");
        assert!(RouteTable::load(Some(&routes)).get("jarvis").is_some());
        assert_eq!(
            store.agent_by_name("jarvis").unwrap().unwrap().status,
            AgentStatus::Active
        );
        // The peer cannot reach teardown through an adopted record either.
        let target = SessionTarget::resolve(&store, "claude:jarvis", None).unwrap();
        let err =
            teardown_session(&store, &env, &target, "peer", Caller::Peer, |_| {}).unwrap_err();
        assert_eq!(err.status(), 403);

        // The owner may still adopt it.
        let out = prepare_decommission(&store, &env, &target, "owner", Caller::Owner).unwrap();
        assert_eq!(out.status, "decommissioning");
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
    }

    /// `claude:ada` hosted and torn down (record decommissioned, agent Retired), then registered
    /// Active again outside the lifecycle, as a resident does after a restart.
    fn stale_ada(store: &MemStore, env: &LifecycleEnv<'_>) {
        spin_up(
            store,
            env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let target = SessionTarget::resolve(store, "claude:ada", None).unwrap();
        teardown_session(store, env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(
            store.agent_by_name("ada").unwrap().unwrap().status,
            AgentStatus::Retired
        );
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Ada".into(),
                email: Some("ada@example.org".into()),
                project: "FerroLoop".into(),
                role: AgentRole::Assistant,
                extra_areas: Vec::new(),
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "ada".into(),
            })
            .unwrap();
        assert_eq!(
            store.agent_by_name("ada").unwrap().unwrap().status,
            AgentStatus::Active
        );
    }

    #[test]
    fn peer_cannot_use_a_stale_decommissioned_record_to_reach_a_reregistered_agent() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        stale_ada(&store, &env);
        let agent_before = store.agent_by_name("ada").unwrap().unwrap();
        let record_before = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(record_before.status, "decommissioned");
        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        assert!(target.hosted.is_some());

        let err = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
        assert_eq!(err.status(), 403);
        let err = prepare_decommission(&store, &env, &target, "peer", Caller::Peer).unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
        assert_eq!(err.status(), 403);
        let mut cleared = Vec::new();
        let err = teardown_session(&store, &env, &target, "peer", Caller::Peer, |k| {
            cleared.push(k.to_string())
        })
        .unwrap_err();
        assert!(matches!(err, LifecycleError::Forbidden(_)), "{err}");
        assert_eq!(err.status(), 403);
        assert!(cleared.is_empty());

        // Nothing changed: the record, the agent, and the record count.
        assert_eq!(
            find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap(),
            record_before
        );
        assert_eq!(store.agent_by_name("ada").unwrap().unwrap(), agent_before);
        assert_eq!(agent_before.status, AgentStatus::Active);
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);

        // The Owner still revives it, keeping the record's history.
        let out = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(out.status, "running");
        let revived = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(revived.status, "running");
        assert_eq!(revived.created_at_ms, record_before.created_at_ms);
    }

    #[test]
    fn peer_spin_up_and_repeat_teardown_still_work_on_a_decommissioned_record_with_a_retired_agent()
    {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let target = decommissioned_felicity(&store, &env);
        let before = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();

        // Teardown stays repeatable and harmless for a Peer.
        let out = teardown_session(&store, &env, &target, "peer", Caller::Peer, |_| {}).unwrap();
        assert_eq!(out.status, "decommissioned");
        // Prepare keeps its 409 for a Peer when no Active agent exists.
        let err = prepare_decommission(&store, &env, &target, "peer", Caller::Peer).unwrap_err();
        assert_eq!(err.status(), 409);

        // A Peer spin-up of the persona whose agent is Retired revives it and keeps the history.
        let out = spin_up(
            &store,
            &env,
            &req("felicity", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        assert_eq!(out.status, "running");
        let after = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        assert_eq!(after.status, "running");
        assert_eq!(after.created_at_ms, before.created_at_ms);
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Active
        );
    }

    #[test]
    fn peer_prepare_of_a_hosted_session_still_works() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "peer",
            Caller::Peer,
        )
        .unwrap();
        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        let out = prepare_decommission(&store, &env, &target, "peer", Caller::Peer).unwrap();
        assert_eq!(out.status, "decommissioning");
        assert!(out.prompt.is_some());
    }

    #[test]
    fn teardown_clears_queues_under_the_discovered_id_of_a_hosted_session() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let target = SessionTarget::resolve(
            &store,
            "claude:ada",
            Some(("claude-xyz".into(), Some("Ada".into()))),
        )
        .unwrap();
        assert!(target.hosted.is_some());
        assert_eq!(target.discovered_id, "claude-xyz");
        let mut cleared = Vec::new();
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |k| {
            cleared.push(k.to_string())
        })
        .unwrap();
        for key in ["claude:ada", "claude-xyz", "ada"] {
            assert!(cleared.contains(&key.to_string()), "{key}: {cleared:?}");
        }
        let mut distinct = cleared.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), cleared.len(), "keys repeat: {cleared:?}");
    }

    #[test]
    fn target_keys_are_distinct_even_when_equal_ids_are_not_adjacent() {
        let target = SessionTarget {
            raw_id: "ada".into(),
            session_id: "claude:ada".into(),
            persona: "ada".into(),
            discovered_id: "claude:ada".into(),
            hosted: None,
        };
        assert_eq!(target.keys(), vec!["claude:ada", "ada"]);
    }

    #[test]
    fn a_malformed_session_id_is_rejected_before_any_side_effect() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let routes = webhook_route_file(home.path(), "ada");
        for raw in ["claude:Foo Bar", "claude:", "a/b", "1abc"] {
            let err = SessionTarget::resolve(&store, raw, None).unwrap_err();
            assert!(matches!(err, LifecycleError::Invalid(_)), "{raw}: {err}");
            assert_eq!(err.status(), 400);
        }
        let err = SessionTarget::resolve(
            &store,
            "claude-abc",
            Some(("claude-abc".into(), Some("Bad Name".into()))),
        )
        .unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(RouteTable::load(Some(&routes)).get("ada").is_some());
        assert!(list_hosted_sessions(&store).unwrap().is_empty());
    }

    #[test]
    fn teardown_lookup_failure_happens_before_side_effects() {
        // A target built by hand with an unusable persona still fails before queues or routes change.
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let target = SessionTarget {
            raw_id: "claude:Bad Name".into(),
            session_id: "claude:Bad Name".into(),
            persona: "bad name".into(),
            discovered_id: "claude:Bad Name".into(),
            hosted: None,
        };
        let mut cleared = Vec::new();
        let err = teardown_session(&store, &env, &target, "owner", Caller::Owner, |k| {
            cleared.push(k.to_string())
        })
        .unwrap_err();
        assert_eq!(err.status(), 500);
        assert!(cleared.is_empty());
        assert!(list_hosted_sessions(&store).unwrap().is_empty());
    }

    #[test]
    fn a_vendor_qualified_id_never_matches_another_vendors_record() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();

        let id_of = |raw: &str| {
            SessionTarget::resolve(&store, raw, None)
                .unwrap()
                .hosted
                .map(|r| r.session_id)
        };
        assert_eq!(id_of("claude:ada").as_deref(), Some("claude:ada"));
        assert_eq!(id_of("ada").as_deref(), Some("claude:ada"));
        assert_eq!(id_of("codex:ada"), None);
        assert!(
            find_hosted_session(&store, &["codex:ada"])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            restart_hosted_session(&store, &env, "codex:ada", "owner")
                .unwrap_err()
                .status(),
            404
        );
        // A discovered session of a vendor with a record still maps onto it by persona.
        let found = SessionTarget::resolve(
            &store,
            "claude-abc",
            Some(("claude-abc".into(), Some("Ada".into()))),
        )
        .unwrap();
        assert_eq!(found.session_id, "claude:ada");

        // With both vendors present each id resolves to its own record, and the exact id wins
        // over a persona key listed before it.
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(id_of("codex:ada").as_deref(), Some("codex:ada"));
        assert_eq!(id_of("claude:ada").as_deref(), Some("claude:ada"));
        assert_eq!(
            find_hosted_session(&store, &["ada", "codex:ada"])
                .unwrap()
                .unwrap()
                .session_id,
            "codex:ada"
        );
    }

    #[test]
    fn teardown_keeps_a_tmux_session_that_another_hosted_session_shares() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let fake = FakeRuntime::running(&["Ada"]);
        let fake_env = fake.env(home.path());
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let set_spawned = |id: &str, spawned: bool| {
            let mut rec = find_hosted_session(&store, &[id]).unwrap().unwrap();
            rec.runtime_spawned = spawned;
            persist_hosted_session(&store, &rec, "owner").unwrap();
        };
        let spawned = |id: &str| {
            find_hosted_session(&store, &[id])
                .unwrap()
                .unwrap()
                .runtime_spawned
        };
        // `claude:ada` opened the shared tmux session `Ada`.
        set_spawned("claude:ada", true);

        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        let out =
            teardown_session(&store, &fake_env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(out.status, "decommissioned");
        assert!(!out.runtime_stopped, "the shared session is still running");
        assert!(fake.is_live("Ada") && fake.calls.borrow().is_empty());
        // Ownership moved to the surviving session, which stops it in its turn.
        assert!(!spawned("claude:ada") && spawned("codex:ada"));
        assert_eq!(
            find_hosted_session(&store, &["codex:ada"])
                .unwrap()
                .unwrap()
                .status,
            "running"
        );

        let target = SessionTarget::resolve(&store, "codex:ada", None).unwrap();
        let out =
            teardown_session(&store, &fake_env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        // The last sharer stops it, and its record no longer claims it.
        assert!(out.runtime_stopped);
        assert_eq!(*fake.calls.borrow(), ["kill Ada"]);
        assert!(!fake.is_live("Ada") && !spawned("codex:ada"));
    }

    #[test]
    fn teardown_stops_the_tmux_session_when_the_other_sharer_is_decommissioned() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let other = SessionTarget::resolve(&store, "codex:ada", None).unwrap();
        teardown_session(&store, &env, &other, "owner", Caller::Owner, |_| {}).unwrap();
        let mut rec = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        rec.runtime_spawned = true;
        persist_hosted_session(&store, &rec, "owner").unwrap();

        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        let fake = FakeRuntime::running(&["Ada"]);
        let out = teardown_session(
            &store,
            &fake.env(home.path()),
            &target,
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        assert!(out.runtime_stopped);
        assert!(!fake.is_live("Ada"));
        assert!(
            !find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .runtime_spawned
        );
    }

    #[test]
    fn mode_parsing_accepts_only_prepare_and_teardown() {
        assert_eq!(
            DecommissionMode::parse(" Prepare ").unwrap(),
            DecommissionMode::Prepare
        );
        assert_eq!(
            DecommissionMode::parse("teardown").unwrap(),
            DecommissionMode::Teardown
        );
        for bad in ["", "clear", "stop"] {
            assert_eq!(DecommissionMode::parse(bad).unwrap_err().status(), 400);
        }
    }

    #[test]
    fn hosted_session_slugs_stay_one_to_one_with_session_ids() {
        // The slug turns `:` into `-`, so `x:y-ada` and `x-y:ada` share one record when both `x`
        // and `x-y` are vendors. Built-ins have no such pair; a plugin vendor can. Register one
        // (ids of this test's own: the vendor registry is process-wide and tests run in parallel).
        let x = vendor_registry_tests::custom_cli("fl-slugpair");
        let xy = vendor_registry_tests::custom_cli("fl-slugpair-y");
        let collide = |vendor: &str, persona: &str| {
            SpinUpRequest {
                persona: persona.into(),
                project: "FerroLoop".into(),
                role: "assistant".into(),
                vendor: vendor.into(),
                ..SpinUpRequest::default()
            }
            .validate()
        };
        // Every vendor in the registry, with personas that read like the rest of an id: no two
        // ACCEPTED session ids may share a slug.
        let mut seen = std::collections::BTreeMap::new();
        let personas = [
            "a",
            "a-b",
            "claude-a",
            "codex-a-b",
            "x1",
            "y-ada",
            "ada",
            "code-a",
            "api-a",
            "slugpair-y-ada",
        ];
        for vendor in spin_up_vendor_ids() {
            for persona in personas {
                let Ok(v) = collide(&vendor, persona) else {
                    continue;
                };
                let id = format!("{}:{}", v.vendor, v.persona);
                let slug = hosted_session_slug(&id);
                if let Some(other) = seen.insert(slug.clone(), id.clone()) {
                    panic!("`{id}` and `{other}` share the slug `{slug}`");
                }
            }
        }
        // The shorter id's colliding persona is refused, naming the longer id's session.
        let LifecycleError::Invalid(message) = collide(&x, "y-ada").unwrap_err() else {
            panic!("{x}:y-ada must be Invalid");
        };
        assert!(message.contains(&format!("`{xy}:ada`")), "{message}");
        assert!(message.contains("state slug"), "{message}");
        // Every persona of the longer id is allowed, and so is a non-colliding one of the shorter.
        for persona in ["ada", "bob", "y-ada", "z-ada"] {
            collide(&xy, persona).unwrap_or_else(|e| panic!("{xy}:{persona}: {e}"));
        }
        // Real spin-ups on both vendors.
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        for (vendor, persona) in [
            (&x, "z-ada"),
            (&x, "y"),
            (&x, "ada"),
            (&xy, "ada"),
            (&xy, "bob"),
        ] {
            let out = spin_up(
                &store,
                &env,
                &req(persona, "assistant", vendor),
                "owner",
                Caller::Owner,
            )
            .unwrap_or_else(|e| panic!("{vendor}:{persona}: {e}"));
            assert_eq!(out.session_id, format!("{vendor}:{persona}"));
        }
        // The refused spin-up registered nothing.
        let err = spin_up(
            &store,
            &env,
            &req("y-ada", "assistant", &x),
            "owner",
            Caller::Owner,
        )
        .unwrap_err();
        assert_eq!(err.status(), 400, "{err:?}");
        assert!(store.agent_by_name("y-ada").unwrap().is_none());
        // The pure check: only the shorter id is refused; the persona rules apply to the
        // remainder; an unrelated vendor never collides.
        let ids = |l: &[&str]| l.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            colliding_session_id("x", "y-ada", &ids(&["x", "x-y"])).as_deref(),
            Some("x-y:ada")
        );
        assert_eq!(
            colliding_session_id("x-y", "ada", &ids(&["x", "x-y"])),
            None
        );
        // The remainder `9` of `x-y-9` is not a persona (starts with a digit); `xa` is another id; a
        // vendor id that is the whole tail leaves no persona.
        assert_eq!(colliding_session_id("x-9", "a", &ids(&["x", "x-9"])), None);
        assert_eq!(colliding_session_id("x", "y-9", &ids(&["x", "x-y"])), None);
        assert_eq!(colliding_session_id("x", "a", &ids(&["x", "xa"])), None);
        assert_eq!(colliding_session_id("x", "y", &ids(&["x", "x-y"])), None);
        // A shorter plugin vendor (`kimi`, `claude`) disables no built-in: every built-in id
        // hosts any persona, even one that reads like the rest of a longer id.
        let all = ids(&[
            "claude",
            "kimi",
            "kimi-api",
            "claude-code",
            "openrouter-api",
            "openrouter",
            "ollama",
            "lemonade",
        ]);
        for vendor in ["kimi-api", "claude-code", "openrouter-api", "ollama"] {
            for persona in ["ada", "api-ada", "code-ada", "y-ada", "a-b"] {
                assert_eq!(
                    colliding_session_id(vendor, persona, &all),
                    None,
                    "{vendor}:{persona}"
                );
            }
        }
        assert_eq!(
            colliding_session_id("kimi", "api-ada", &all).as_deref(),
            Some("kimi-api:ada")
        );
        assert_eq!(
            colliding_session_id("claude", "code-ada", &all).as_deref(),
            Some("claude-code:ada")
        );
    }

    // ---- audit 2026-10-30 regressions ----

    fn hosted_record(
        vendor: &str,
        persona: &str,
        status: &str,
        spawned: bool,
    ) -> HostedSessionRecord {
        HostedSessionRecord {
            session_id: format!("{vendor}:{persona}"),
            persona: persona.into(),
            project: "FerroLoop".into(),
            role: "assistant".into(),
            vendor: vendor.into(),
            model: None,
            node_id: DEFAULT_NODE_ID.into(),
            hosted: true,
            status: status.into(),
            restart_policy: "on-failure".into(),
            restart_count: 0,
            tmux_target: Some(format!("{}:{vendor}", capitalize(persona))),
            cwd: None,
            hydration_dispatched: false,
            runtime_spawned: spawned,
            created_at_ms: 1,
            updated_at_ms: 1,
            decommission_prepared_at_ms: None,
        }
    }

    #[test]
    fn command_succeeds_within_reports_exit_status_and_kills_a_wedged_command() {
        let t = Duration::from_secs(5);
        assert!(command_succeeds_within(&mut Command::new("true"), t));
        assert!(!command_succeeds_within(&mut Command::new("false"), t));
        assert!(!command_succeeds_within(
            &mut Command::new("fl-no-such-binary-for-test"),
            t
        ));
        let started = Instant::now();
        let mut hung = Command::new("sleep");
        hung.arg("60");
        assert!(!command_succeeds_within(
            &mut hung,
            Duration::from_millis(200)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "a wedged tmux must not hold the store guard"
        );
    }

    #[test]
    fn respin_of_a_decommissioned_record_does_not_inherit_runtime_ownership() {
        let home = TempDir::new().unwrap();
        let store = MemStore::default();
        // The tmux session this record owned was stopped by the teardown. Whatever tmux session
        // bears the name now belongs to someone else, and a later teardown must not kill it.
        persist_hosted_session(
            &store,
            &hosted_record("claude", "ada", STATUS_DECOMMISSIONED, true),
            "owner",
        )
        .unwrap();
        spin_up(
            &store,
            &env(&home),
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let rec = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(rec.status, STATUS_RUNNING);
        assert!(!rec.runtime_spawned);
    }

    #[test]
    fn respin_of_a_live_record_keeps_runtime_ownership() {
        let home = TempDir::new().unwrap();
        let store = MemStore::default();
        persist_hosted_session(
            &store,
            &hosted_record("claude", "ada", STATUS_RUNNING, true),
            "owner",
        )
        .unwrap();
        spin_up(
            &store,
            &env(&home),
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let rec = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert!(rec.runtime_spawned);
    }

    #[test]
    fn bare_persona_prefers_a_live_record_over_a_decommissioned_sibling() {
        let store = MemStore::default();
        // `claude:ada` sorts first, but it is history; `codex:ada` is the live session.
        persist_hosted_session(
            &store,
            &hosted_record("claude", "ada", STATUS_DECOMMISSIONED, false),
            "owner",
        )
        .unwrap();
        persist_hosted_session(
            &store,
            &hosted_record("codex", "ada", STATUS_RUNNING, false),
            "owner",
        )
        .unwrap();
        let found = find_hosted_session(&store, &["ada"]).unwrap().unwrap();
        assert_eq!(found.session_id, "codex:ada");
        // An exact id still reaches the decommissioned record.
        let exact = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(exact.status, STATUS_DECOMMISSIONED);
        // With only decommissioned matches the first one is still returned.
        persist_hosted_session(
            &store,
            &hosted_record("codex", "ada", STATUS_DECOMMISSIONED, false),
            "owner",
        )
        .unwrap();
        let found = find_hosted_session(&store, &["ada"]).unwrap().unwrap();
        assert_eq!(found.session_id, "claude:ada");
    }

    #[test]
    fn bare_persona_with_two_live_records_resolves_to_the_first_by_session_id() {
        // Open finding A4: the pick is deterministic but not the caller's choice. Pinned so a
        // change to ambiguity handling is deliberate.
        let store = MemStore::default();
        for vendor in ["codex", "claude"] {
            persist_hosted_session(
                &store,
                &hosted_record(vendor, "ada", STATUS_RUNNING, false),
                "owner",
            )
            .unwrap();
        }
        let found = find_hosted_session(&store, &["ada"]).unwrap().unwrap();
        assert_eq!(found.session_id, "claude:ada");
    }

    #[test]
    fn spin_up_rejects_hostile_persona_and_text_fields() {
        let ok = req("ada", "assistant", "claude");
        assert!(ok.validate().is_ok());
        for persona in [
            "-tfoo",         // would read as a tmux option
            "ada\0",         // NUL
            "ada\nlead",     // newline
            "ada lead",      // inner space
            "ada;rm",        // shell metacharacter
            "ada/../x",      // path separators
            "\u{ff41}da",    // full-width letter
            "ad\u{430}",     // Cyrillic homoglyph
            "Ada",           // upper case
            "9ada",          // leading digit
            &"a".repeat(37), // one over the slug budget
        ] {
            let bad = SpinUpRequest {
                persona: persona.into(),
                ..ok.clone()
            };
            assert!(
                bad.validate().is_err(),
                "persona {persona:?} must be refused"
            );
        }
        for (field, bad) in [
            (
                "cwd",
                SpinUpRequest {
                    cwd: Some("/tmp\n-x".into()),
                    ..ok.clone()
                },
            ),
            (
                "cwd",
                SpinUpRequest {
                    cwd: Some("/tmp/\0x".into()),
                    ..ok.clone()
                },
            ),
            (
                "model",
                SpinUpRequest {
                    model: Some("m\u{1b}[2J".into()),
                    ..ok.clone()
                },
            ),
            (
                "node_id",
                SpinUpRequest {
                    node_id: Some("../etc".into()),
                    ..ok.clone()
                },
            ),
            (
                "node_id",
                SpinUpRequest {
                    node_id: Some("n".repeat(65)),
                    ..ok.clone()
                },
            ),
        ] {
            assert!(bad.validate().is_err(), "{field} must be refused");
        }
    }

    #[test]
    fn a_hostile_or_oversized_session_id_is_rejected_before_any_side_effect() {
        let store = MemStore::default();
        for raw in [
            "claude:../../etc".to_string(),
            "claude:ada\0".to_string(),
            "claude:-x".to_string(),
            format!("claude:{}", "a".repeat(1024 * 1024)),
        ] {
            let err = SessionTarget::resolve(&store, &raw, None).unwrap_err();
            assert!(matches!(err, LifecycleError::Invalid(_)), "{err:?}");
        }
        assert!(list_hosted_sessions(&store).unwrap().is_empty());
    }
}
