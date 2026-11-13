//! Hosted agent session lifecycle: spin-up, two-phase decommission, restart, and remodel (a move
//! to another vendor) of sessions that FerroDeck and `fl agent` manage (Issue #82 / #69, Local
//! Record 59).
//!
//! The functions are generic over the agent registry and state store, so the hub's HTTP handlers
//! (`crates/hub/src/api.rs`) and `fl agent spin-up|decommission` share one implementation. Hub-only
//! concerns (the in-memory operator prompt queue) reach this module through caller closures or the
//! returned `handshake_dispatched` flag.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fl_core::agent::{
    AgentRecord, AgentRole, AgentStatus, RegisterAgentArgs, get_role, normalize_role_slug,
};
use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
use fl_core::state::{
    HandoffEntry, MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload,
    StateScope, UpsertStateArgs, validate_slug,
};
use fl_core::store::{AgentRegistry, Catalog, MessageStore, StateStore, StoreError};
use fl_core::{VendorDescriptor, VendorRegistry, VendorTransport, lookup_vendor};
use fl_exec::stamp;
use fl_wire::dispatcher::{
    OperatorPromptError, OperatorPromptIntent, OperatorPromptOutcome, PushError,
    push_operator_prompt_with_intent,
};
use fl_wire::identity::{
    RouteTable, VendorRoute, is_native_session_id, legacy_routes_path, resolve_routes_path,
};
use serde::{Deserialize, Serialize};

use crate::api::{reforge_role_label, route_matches_session_id, synthesize_decommission_prompt};

#[cfg(test)]
use crate::api::canonical_reforge_prompt;

/// Legacy executable role choices; spin-up also accepts roles in the current catalog.
pub const SPIN_UP_ROLES: [&str; 11] = [
    "lead",
    "assistant",
    "ui-developer",
    "web-developer",
    "mobile-developer",
    "system-developer",
    "data-engineer",
    "tester",
    "strategist",
    "journalist",
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

/// Whether a vendor runs as an interactive CLI process in a hosted runtime window. An HTTP API
/// vendor without a CLI adapter has no CLI to host; an id with a registered CLI adapter or an id
/// the registry does not know is treated as a CLI.
fn vendor_has_runtime(vendor: &str) -> bool {
    fl_wire::vendor_adapter::lookup_vendor_cli_adapter(vendor).is_some()
        || !matches!(
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
/// The tmux window was confirmed gone twice, with no route of this vendor.
/// Not decommissioned: the agent stays, and nothing is tombstoned or killed.
pub const STATUS_STOPPED: &str = "stopped";

pub const STOP_REASON_RUNTIME_GONE: &str = "runtime_gone";
pub const STOP_REASON_CLEAN_EXIT: &str = "clean_exit";
pub const STOP_REASON_CRASH_LOOP: &str = "crash_loop";
pub const AUTO_RESPAWN_ACTOR: &str = "hub-auto-respawn";
pub const AUTO_RESPAWN_MAX_IN_WINDOW: usize = 3;
pub const AUTO_RESPAWN_WINDOW_MS: u64 = 10 * 60 * 1_000;
pub const AUTO_RESPAWN_BACKOFF_SECS: [u64; 3] = [30, 60, 120];
pub const DEAD_STREAK_MIN_SPACING_MS: u64 = 10_000;
pub const DEAD_STREAK_MAX_AGE_MS: u64 = 2 * 60 * 1_000;

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
    /// Parent of the hub repository. A spin-up with no `cwd` may use a child directory
    /// whose name matches the project. `None` leaves an unresolved `cwd` unset.
    pub workspace_root: Option<&'a Path>,
    /// Log guidance warnings independently of response consumption (the hub default).
    /// CLI callers disable this and render the returned warnings once in their output format.
    pub log_hydration_warnings: bool,
}

/// Process-wide [`fl_core::VendorHostAdapterRegistry`] slot injected at startup by `fl-cli`.
static HOST_ADAPTER_REGISTRY: std::sync::OnceLock<fl_core::VendorHostAdapterRegistry> =
    std::sync::OnceLock::new();

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_HOST_ADAPTER_REGISTRY: std::cell::RefCell<Option<fl_core::VendorHostAdapterRegistry>> =
        const { std::cell::RefCell::new(None) };
}

/// Install the concrete [`fl_core::VendorHostAdapterRegistry`] composed at `fl-cli` into `fl-hub`.
pub fn set_global_vendor_host_adapters(
    registry: fl_core::VendorHostAdapterRegistry,
) -> &'static fl_core::VendorHostAdapterRegistry {
    HOST_ADAPTER_REGISTRY.get_or_init(|| registry)
}

impl<'a> LifecycleEnv<'a> {
    /// Real runtime calls are off under `cfg(test)` and when `FL_TEST_SPINUP_NO_SPAWN=1`.
    pub fn new(home_override: Option<&'a Path>) -> Self {
        let disabled =
            cfg!(test) || std::env::var("FL_TEST_SPINUP_NO_SPAWN").is_ok_and(|v| v.trim() == "1");
        Self {
            home_override,
            spawn_runtime: !disabled,
            runtime: &HostRuntime,
            key_probe: &HostKeyProbe,
            workspace_root: None,
            log_hydration_warnings: true,
        }
    }

    /// Look up a dependency-injected [`fl_core::VendorHostAdapter`] for `vendor`.
    pub fn host_vendor_adapter(
        &self,
        vendor: &str,
    ) -> Option<std::sync::Arc<dyn fl_core::VendorHostAdapter>> {
        #[cfg(test)]
        if let Some(found) = TEST_HOST_ADAPTER_REGISTRY
            .with(|slot| slot.borrow().as_ref().and_then(|reg| reg.lookup(vendor)))
        {
            return Some(found);
        }
        HOST_ADAPTER_REGISTRY
            .get()
            .and_then(|reg| reg.lookup(vendor))
    }

    /// Resolve the vendor CLI adapter for `vendor` via the adapter registry, allowing callers
    /// to interact with vendor-specific launch and startup rules through the [`fl_wire::vendor_adapter::VendorCliAdapter`] contract.
    pub fn vendor_adapter(
        &self,
        vendor: &str,
    ) -> Option<std::sync::Arc<dyn fl_wire::vendor_adapter::VendorCliAdapter>> {
        fl_wire::vendor_adapter::HostVendorAdapterRegistry::global().lookup(vendor)
    }

    /// Adapter-layer launch options (including `remote_control`, enabled by default unless
    /// `FL_WIRE_REMOTE_CONTROL=0` or `false`, and default `Posture::Auto`).
    pub fn vendor_launch_options(&self) -> fl_wire::vendor_adapter::VendorLaunchOptions {
        self.vendor_launch_options_for(fl_core::permissions::Posture::Auto)
    }

    /// Adapter-layer launch options for a specific permission `posture`.
    pub fn vendor_launch_options_for(
        &self,
        posture: fl_core::permissions::Posture,
    ) -> fl_wire::vendor_adapter::VendorLaunchOptions {
        let remote_control =
            parse_remote_control_env(std::env::var("FL_WIRE_REMOTE_CONTROL").ok().as_deref());
        fl_wire::vendor_adapter::VendorLaunchOptions {
            remote_control,
            posture,
            auto_approval: None,
        }
    }

    /// Resolve the effective [`fl_core::permissions::AutoApproval`] choice for `vendor` from the
    /// hub's saved settings (or the compiler default when no settings file exists).
    pub fn auto_approval_for(&self, vendor: &str) -> Option<fl_core::permissions::AutoApproval> {
        let home = self
            .home_override
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        match home {
            Some(home) => crate::settings::load(&home).effective_auto_approval_for(vendor),
            None => vendor
                .trim()
                .parse::<fl_core::permissions::Vendor>()
                .ok()
                .map(|v| v.default_auto_approval()),
        }
    }

    /// Adapter-layer launch options for a specific `vendor` and permission `posture`, including
    /// the configured per-vendor [`fl_core::permissions::AutoApproval`] choice.
    pub fn vendor_launch_options_for_vendor(
        &self,
        vendor: &str,
        posture: fl_core::permissions::Posture,
    ) -> fl_wire::vendor_adapter::VendorLaunchOptions {
        let mut opts = self.vendor_launch_options_for(posture);
        opts.auto_approval = self.auto_approval_for(vendor);
        opts
    }
}

pub(crate) fn parse_remote_control_env(raw: Option<&str>) -> bool {
    raw.map(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
    .unwrap_or(true)
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

/// Whether a tmux window is still the vendor's process.
///
/// [`RuntimePresence::Unknown`] means tmux failed to run, timed out, or did not
/// answer. That is not death: `exists() == false` is not [`RuntimePresence::Gone`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePresence {
    Alive,
    Gone,
    Unknown,
}

/// The runtime calls a lifecycle makes on a session's `tmux` runtime. Only
/// [`LifecycleEnv::runtime`] makes them, and only when `spawn_runtime` is on.
///
/// A runtime is one *unit* ([`runtime_unit`]): a whole tmux session (named by the capitalised
/// persona), or, for Muse, one window of the shared session [`MUSE_SESSION`] (`muse:<persona>`).
/// `kill` and `exists` take the unit, so a Muse window is never confused with the session that
/// holds other agents' windows.
pub trait SessionRuntime: std::fmt::Debug {
    /// Whether each vendor has a separate session rather than a shared persona container.
    fn separate_vendor_units(&self) -> bool {
        false
    }

    /// The addressable runtime unit for a recorded vendor target. Legacy tmux runtimes
    /// share a persona session; the native host gives each vendor a separate session.
    fn unit<'a>(&self, target: &'a str) -> &'a str {
        runtime_unit(target)
    }

    /// Check launch compatibility before lifecycle state or routes are changed.
    fn preflight_spawn(&self, _hosted_id: &str) -> Result<(), LifecycleError> {
        Ok(())
    }

    /// Check stop authority before lifecycle cleanup. `recorded` means the durable record
    /// proves this lifecycle started the exact unit; adoption requires live ownership proof.
    fn preflight_stop(
        &self,
        _unit: &str,
        _hosted_id: &str,
        _recorded: bool,
    ) -> Result<(), LifecycleError> {
        Ok(())
    }

    /// Start a detached runtime; `false` when it did not start (tmux failed, or the name exists).
    /// With `launch` the window runs that command (the vendor's CLI) instead of a blank shell.
    /// Built-in CLI vendors launch via [`launch_spec_for`]; custom vendors get a blank window.
    ///
    /// `session` is the tmux session to create, except [`MUSE_SESSION`]: that one is shared, so
    /// `window` is added to it when it exists (and the spawn is refused when a window of that
    /// name is there already: it is not ours to touch). A window it adds is marked with
    /// `hosted_id` (the hosted record's session id, [`HOSTED_MARK`]); a spawn that cannot read
    /// back and mark its window removes the window and fails.
    fn spawn(
        &self,
        session: &str,
        window: &str,
        cwd: Option<&str>,
        launch: Option<&LaunchSpec>,
        hosted_id: &str,
    ) -> bool;
    /// Stop a runtime unit; `true` when nothing of ours is left of it. A window unit
    /// (`muse:<persona>`) is stopped with `kill-window` on that one window and never takes its
    /// session with it, and only when the window carries the mark `hosted_id`: a window of the
    /// name without it (or with another) is not ours, is left alone, and counts as "nothing of
    /// ours to kill" (`true`). A session unit is killed whole, `hosted_id` unused.
    fn kill(&self, unit: &str, hosted_id: &str) -> bool;
    /// Stop `unit` for an owner teardown of a session this lifecycle did not spawn.
    /// A bare session is stopped as [`Self::kill`]. A Muse window is stopped when exactly
    /// one window has that name, even without [`HOSTED_MARK`]. A non-Muse `session:window`
    /// stops that window, or the whole session when the window is its only one; a missing
    /// window is already stopped, and no answer from tmux is not. Two Muse windows of the
    /// name answer `false`. The default is [`Self::kill`], which still requires the mark.
    fn kill_owner(&self, unit: &str, hosted_id: &str) -> bool {
        self.kill(unit, hosted_id)
    }
    /// Recent pane text, or empty when the runtime has none. Used to notice Grok's
    /// directory-trust prompt after a spawn.
    fn capture(&self, _unit: &str) -> String {
        String::new()
    }
    /// Send `keys` to `unit` (for example `y` at Grok's trust prompt).
    fn send_keys(&self, _unit: &str, _keys: &str) -> bool {
        false
    }
    /// `true` when a runtime unit of this name runs, whoever started it.
    fn exists(&self, unit: &str) -> bool;
    /// Liveness of `unit` for dead-session reconcile. The default is
    /// [`RuntimePresence::Unknown`], which must not be read as gone.
    fn presence(&self, _unit: &str) -> RuntimePresence {
        RuntimePresence::Unknown
    }
    /// `true` when `unit` runs and is the one `hosted_id` started: for a window unit, a window of
    /// that name carrying the mark `hosted_id`; for a session unit, as [`Self::exists`].
    fn owns(&self, unit: &str, hosted_id: &str) -> bool;
    /// Root OS PID recorded by the runtime for `unit` (when known).
    fn recorded_pid(&self, _unit: &str, _hosted_id: &str) -> Option<u32> {
        None
    }
    /// PID of the runtime leader, which can differ from the vendor CLI PID.
    fn recorded_leader_pid(&self, _unit: &str, _hosted_id: &str) -> Option<u32> {
        None
    }
    /// Exit status code recorded by the runtime for a non-running `unit` (when known and not
    /// terminated by signal). When `expected_pid` is `Some(pid)` (`pid > 0`) and the runtime
    /// records a non-zero child PID that differs from `pid`, returns `None`.
    fn recorded_exit_code(&self, _unit: &str, _expected_pid: Option<u32>) -> Option<i32> {
        None
    }
    /// The pause between the checks that a launched command's session is still there (see
    /// [`launch_survived`]). Zero for a runtime that answers at once.
    fn settle_delay(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

/// The tmux window option that marks a window this lifecycle created. Its value is the hosted
/// record's session id (`muse:<persona>`). A window of the shared Muse session is addressed by
/// name, and a name cannot tell our window from one `muse-sessions` or a person opened under
/// the same name later: only a window with this mark is ours to claim or kill.
pub(crate) const HOSTED_MARK: &str = "@fl_hosted";

/// The one tmux session every Muse agent's window lives in (`muse-sessions` hosts them there as
/// windows named by persona). Never created or killed per persona: only a window is.
pub(crate) const MUSE_SESSION: &str = "muse";

/// The tmux target a hosted session of `vendor` is addressed by: `muse:<persona>` for Muse (a
/// window of the shared session), `<Display>:<vendor>` for every other vendor.
fn hosted_tmux_target(vendor: &str, persona: &str) -> String {
    if vendor == "muse" {
        format!("{MUSE_SESSION}:{persona}")
    } else {
        format!("{}:{vendor}", capitalize(persona))
    }
}

/// The `(session, window)` a runtime of `vendor` is spawned with (see [`hosted_tmux_target`]).
fn runtime_names(vendor: &str, persona: &str) -> (String, String) {
    if vendor == "muse" {
        (MUSE_SESSION.to_string(), persona.to_string())
    } else {
        (capitalize(persona), vendor.to_string())
    }
}

/// `true` for a target that names a window of the shared Muse session.
fn is_muse_window_target(tmux_target: &str) -> bool {
    tmux_target
        .strip_prefix(MUSE_SESSION)
        .and_then(|rest| rest.strip_prefix(':'))
        .is_some_and(|window| !window.is_empty())
}

/// The runtime unit a `Session:window` target stands for: the whole target for a window of the
/// shared Muse session, else the tmux session name. Teardown, existence checks and the sharers
/// of a runtime all go by this, never by the session name alone: for `muse:<persona>` the
/// session is every Muse agent's, and must not be killed or counted as one runtime.
fn runtime_unit(tmux_target: &str) -> &str {
    if is_muse_window_target(tmux_target) {
        tmux_target
    } else {
        tmux_target.split(':').next().unwrap_or(tmux_target)
    }
}

/// `true` when `vendor` is Muse, the runtime may be touched, and no window `unit` carries the
/// mark of `hosted_id`. A `runtime_spawned` flag is then stale: our window is gone, and whatever
/// window of that name runs now (`muse-sessions`, a person) is one we did not make. Only a
/// successful spawn of ours may set the flag again. (A Muse window is addressed by name alone,
/// so the name cannot tell the two apart; the mark can.)
fn muse_window_not_ours(env: &LifecycleEnv<'_>, vendor: &str, unit: &str, hosted_id: &str) -> bool {
    vendor == "muse" && env.spawn_runtime && !env.runtime.owns(unit, hosted_id)
}

/// How many times [`launch_survived`] asks whether the session is there.
const LAUNCH_SURVIVAL_CHECKS: u32 = 5;

/// Positive survival window for `claude --resume <uuid>` launches (`1s` in production; post-window
/// exits before route confirmation are caught by `resume_unconfirmed_session_id` -> `resume_failed`).
pub const CLAUDE_RESUME_SURVIVAL_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

/// Poll interval inside [`CLAUDE_RESUME_SURVIVAL_WINDOW`] (`200ms` in production, at most 5 checks).
pub const CLAUDE_RESUME_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

#[cfg(test)]
thread_local! {
    static TEST_CLAUDE_RESUME_SURVIVAL_WINDOW: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// `true` when `pid` is a live (non-zombie) OS process.
pub(crate) fn claude_pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return false;
        }
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("State:")
                    && rest.trim_start().starts_with('Z')
                {
                    return false;
                }
            }
        }
        true
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let rc = unsafe { libc::kill(pid as i32, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// `true` when `entry`'s typed provenance (`hosted_id`, `hydration_generation`) is compatible with
/// `(hosted_id, expected_generation)`.
fn route_entry_matches_unit_provenance(
    entry: &fl_wire::identity::RouteEntry,
    hosted_id: &str,
    expected_generation: Option<&str>,
) -> bool {
    if let Some(hid) = entry
        .hosted_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && !hid.eq_ignore_ascii_case(hosted_id)
    {
        return false;
    }
    if let Some(g) = entry
        .hydration_generation
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && let Some(expected_gen) = expected_generation.map(str::trim).filter(|s| !s.is_empty())
        && g != expected_gen
    {
        return false;
    }
    true
}

/// `true` when `persona`'s saved Claude route in `route_table_paths(env.home_override)` confirms
/// a live `SessionStart:resume` registration for `vendor_session_id` at or after
/// `launch_started_ms` belonging to `(hosted_id, hydration_generation)`.
fn claude_resume_route_registered(
    env: &LifecycleEnv<'_>,
    persona: &str,
    hosted_id: &str,
    vendor_session_id: &str,
    hydration_generation: &str,
    launch_started_ms: u64,
) -> bool {
    if vendor_session_id.trim().is_empty() {
        return false;
    }
    for path in route_table_paths(env.home_override) {
        if !path.is_file() {
            continue;
        }
        let table = RouteTable::load(Some(&path));
        let Some(entry) = table.get(persona) else {
            continue;
        };
        let VendorRoute::Claude {
            pid, session_id, ..
        } = &entry.route
        else {
            continue;
        };
        if session_id.trim() != vendor_session_id.trim() {
            continue;
        }
        if entry.updated_at.unix_millis() < launch_started_ms {
            continue;
        }
        if *pid == 0 || !claude_pid_is_alive(*pid) {
            continue;
        }
        if !route_entry_matches_unit_provenance(entry, hosted_id, Some(hydration_generation)) {
            continue;
        }
        return true;
    }
    false
}

/// `true` when a saved Claude route for `persona` matches `vendor_session_id`, was registered at or
/// after `launch_started_ms`, and proves provenance for `(hosted_id, hydration_generation, runtime_pid)`
/// (even if the process later exited).
fn claude_route_confirmed_for_session(
    env: &LifecycleEnv<'_>,
    persona: &str,
    hosted_id: &str,
    vendor_session_id: &str,
    hydration_generation: Option<&str>,
    runtime_pid: Option<u32>,
    launch_started_ms: u64,
) -> bool {
    if vendor_session_id.trim().is_empty() {
        return false;
    }
    for path in route_table_paths(env.home_override) {
        if !path.is_file() {
            continue;
        }
        let table = RouteTable::load(Some(&path));
        let Some(entry) = table.get(persona) else {
            continue;
        };
        let VendorRoute::Claude {
            pid, session_id, ..
        } = &entry.route
        else {
            continue;
        };
        if session_id.trim() != vendor_session_id.trim() {
            continue;
        }
        if entry.updated_at.unix_millis() < launch_started_ms {
            continue;
        }
        let meta_ok = route_entry_matches_unit_provenance(entry, hosted_id, hydration_generation);
        let pid_ok = *pid > 0 && runtime_pid.is_none_or(|rp| rp == *pid);
        if meta_ok && pid_ok {
            return true;
        }
    }
    false
}

fn claude_resume_survival_window(env: &LifecycleEnv<'_>) -> Duration {
    #[cfg(test)]
    {
        let settle = env.runtime.settle_delay();
        let custom_window = TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.get());
        custom_window.unwrap_or_else(|| settle.saturating_mul(LAUNCH_SURVIVAL_CHECKS))
    }
    #[cfg(not(test))]
    {
        let _ = env;
        CLAUDE_RESUME_SURVIVAL_WINDOW
    }
}

const CLAUDE_SURVIVAL_WINDOW_MARGIN_MS: u64 = 5_000;

fn claude_survival_window_active(
    env: &LifecycleEnv<'_>,
    rstate: &ClaudeResumeState,
    now_ms: u64,
) -> bool {
    if !rstate.in_survival_window {
        return false;
    }
    let Some(launched_ms) = rstate.resume_launched_at_ms else {
        return false;
    };
    let window_ms = (claude_resume_survival_window(env).as_millis() as u64)
        .saturating_add(CLAUDE_SURVIVAL_WINDOW_MARGIN_MS);
    now_ms.saturating_sub(launched_ms) < window_ms
}

/// Positive survival confirmation for `claude --resume <uuid>` launches, returning
/// `(survived, hook_confirmed)`.
///
/// Unlike [`launch_survived`], which returns `true` on the first `t=0` `owns` check, this polls
/// across [`CLAUDE_RESUME_SURVIVAL_WINDOW`] (or returns `(true, true)` early once the resumed
/// session's `SessionStart:resume` hook registers its live Claude route) so a `--resume` process
/// that exits after `t=0` (e.g. `"No conversation found"` at `t=1s`) is caught and triggers the
/// fresh-launch fallback.
fn resume_launch_survived(
    env: &LifecycleEnv<'_>,
    unit: &str,
    hosted_id: &str,
    persona: &str,
    vendor_session_id: &str,
    hydration_generation: &str,
    launch_started_ms: u64,
) -> (bool, bool) {
    if !env.runtime.owns(unit, hosted_id) || env.runtime.presence(unit) == RuntimePresence::Gone {
        return (false, false);
    }
    if claude_resume_route_registered(
        env,
        persona,
        hosted_id,
        vendor_session_id,
        hydration_generation,
        launch_started_ms,
    ) {
        return (true, true);
    }

    #[cfg(test)]
    let settle = env.runtime.settle_delay();
    #[cfg(test)]
    let custom_window = TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.get());
    let window = claude_resume_survival_window(env);

    #[cfg(test)]
    let poll_step = if custom_window.is_some() {
        CLAUDE_RESUME_POLL_INTERVAL.min(window / LAUNCH_SURVIVAL_CHECKS.max(1))
    } else if settle.is_zero() {
        window / LAUNCH_SURVIVAL_CHECKS.max(1)
    } else {
        settle
            .min(window / LAUNCH_SURVIVAL_CHECKS.max(1))
            .max(Duration::from_millis(10))
    };
    #[cfg(not(test))]
    let poll_step = CLAUDE_RESUME_POLL_INTERVAL.min(window / LAUNCH_SURVIVAL_CHECKS.max(1));
    let deadline = Instant::now() + window;
    let mut post_checks = 0u32;

    while post_checks < LAUNCH_SURVIVAL_CHECKS || (!window.is_zero() && Instant::now() < deadline) {
        if !poll_step.is_zero() {
            let sleep_for = if window.is_zero() {
                Duration::ZERO
            } else {
                poll_step.min(deadline.saturating_duration_since(Instant::now()))
            };
            if !sleep_for.is_zero() {
                std::thread::sleep(sleep_for);
            }
        }
        post_checks = post_checks.saturating_add(1);
        if !env.runtime.owns(unit, hosted_id) || env.runtime.presence(unit) == RuntimePresence::Gone
        {
            return (false, false);
        }
        if claude_resume_route_registered(
            env,
            persona,
            hosted_id,
            vendor_session_id,
            hydration_generation,
            launch_started_ms,
        ) {
            return (true, true);
        }
        if window.is_zero() && post_checks >= LAUNCH_SURVIVAL_CHECKS {
            break;
        }
    }

    if env.runtime.owns(unit, hosted_id) && env.runtime.presence(unit) != RuntimePresence::Gone {
        (true, false)
    } else {
        (false, false)
    }
}

/// `true` when the runtime unit of a command just launched in it (`spawn` with a [`LaunchSpec`])
/// is still there: a command that exits at once (a missing binary) takes its window with it. Asks
/// `owns` (our marked window, not any window of the name: one a person opened in the meantime
/// is not the launched command) up to [`LAUNCH_SURVIVAL_CHECKS`] times,
/// [`SessionRuntime::settle_delay`] apart, and answers at the first yes.
fn launch_survived(env: &LifecycleEnv<'_>, unit: &str, hosted_id: &str) -> bool {
    let delay = env.runtime.settle_delay();
    for check in 0..LAUNCH_SURVIVAL_CHECKS {
        if env.runtime.owns(unit, hosted_id) {
            return true;
        }
        if check + 1 < LAUNCH_SURVIVAL_CHECKS && !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
    false
}

pub use fl_wire::vendor_adapter::LaunchSpec;
#[allow(unused_imports)]
pub(crate) use fl_wire::vendor_adapter::{
    MUSE_CATALOG_MODELS, MUSE_DEFAULT_MODEL, agy_cli_model_id, is_plain_model_id,
    launch_env_for_vendor, muse_prompt_argument, normalize_muse_model, normalize_plain_model,
    resolve_vendor_bin, split_window_tag, with_local_bin,
};

/// Read-only launch checks run independently of adapter selection. Return the checked
/// configuration directory so a daemon child cannot inherit a different CODEX_HOME.
fn preflight_vendor_launch(
    env: &LifecycleEnv<'_>,
    vendor: &str,
    hosted: bool,
    cwd: &mut Option<String>,
) -> Result<Option<PathBuf>, LifecycleError> {
    if vendor != "codex" || !hosted || !env.spawn_runtime {
        return Ok(None);
    }
    let base = std::env::current_dir()
        .map_err(|_| invalid_field("cwd", "cannot resolve current directory"))?;
    let explicit = std::env::var_os("CODEX_HOME");
    #[cfg(test)]
    let explicit = TEST_CODEX_HOME.with(|slot| slot.borrow().clone().unwrap_or(explicit));
    let home = std::env::var_os("HOME");
    let config_home = codex_config_home(
        &base,
        explicit.as_deref(),
        env.home_override,
        home.as_deref(),
    )?;
    if config_home.to_str().is_none() {
        return Err(LifecycleError::Invalid(
            "Codex config home must be valid UTF-8".into(),
        ));
    }
    let effective = cwd.as_ref().map(PathBuf::from).unwrap_or(base);
    let checked =
        fl_mcp::vendor::check_codex_project_trust(&effective, &config_home.join("config.toml"))
            .map_err(|e| LifecycleError::Invalid(e.to_string()))?;
    *cwd = Some(
        checked
            .cwd
            .to_str()
            .ok_or_else(|| invalid_field("cwd", "must be valid UTF-8"))?
            .to_string(),
    );
    Ok(Some(config_home))
}

fn codex_config_home(
    base: &Path,
    explicit: Option<&std::ffi::OsStr>,
    home_override: Option<&Path>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, LifecycleError> {
    let home = explicit
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_override.map(|home| home.join(".codex")))
        .or_else(|| {
            home.filter(|v| !v.is_empty())
                .map(|home| Path::new(home).join(".codex"))
        })
        .ok_or_else(|| {
            LifecycleError::Invalid(
                "Codex trust check requires CODEX_HOME or HOME to locate config.toml".into(),
            )
        })?;
    Ok(base.join(home))
}

#[cfg(test)]
thread_local! {
    // Scoped ambient input replacement, not a trust bypass or process environment edit.
    static TEST_CODEX_HOME: std::cell::RefCell<Option<Option<std::ffi::OsString>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct IsolatedCodexHome(Option<Option<std::ffi::OsString>>);
#[cfg(test)]
impl IsolatedCodexHome {
    pub(crate) fn new() -> Self {
        Self(TEST_CODEX_HOME.with(|slot| slot.replace(Some(None))))
    }
}
#[cfg(test)]
impl Drop for IsolatedCodexHome {
    fn drop(&mut self) {
        TEST_CODEX_HOME.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

fn pin_launch_config(launch: &mut Option<LaunchSpec>, config_home: Option<&Path>) {
    if let (Some(launch), Some(home)) = (launch, config_home) {
        launch.env.retain(|(key, _)| key != "CODEX_HOME");
        // preflight accepted only UTF-8 paths.
        launch.env.push((
            "CODEX_HOME".into(),
            home.to_str().expect("checked config home").into(),
        ));
    }
}

fn apply_vendor_launch_env(
    env_vec: &mut Vec<(String, String)>,
    vendor: &str,
    persona: &str,
    home: Option<&Path>,
    hydration_generation: Option<&str>,
) {
    let vendor_env = fl_wire::vendor_adapter::launch_env_for_vendor_with_generation(
        vendor,
        persona,
        home,
        hydration_generation,
    );
    env_vec.retain(|(k, _)| {
        k != "AGENT_HYDRATION_GENERATION" && !vendor_env.iter().any(|(vk, _)| vk == k)
    });
    env_vec.extend(vendor_env);
}

#[cfg(test)]
pub(crate) fn trusted_codex_home(home: &Path, cwd: &Path) -> IsolatedCodexHome {
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    let root = cwd.canonicalize().unwrap();
    let project = root
        .ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(&root);
    std::fs::write(
        home.join(".codex/config.toml"),
        format!(
            "[projects.{}]\ntrust_level = \"trusted\"\n",
            serde_json::to_string(project.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    IsolatedCodexHome::new()
}

/// Build a launch command for built-in CLI vendors; custom vendors use prompt delivery.
#[cfg(test)]
fn launch_spec_for(
    env: &LifecycleEnv<'_>,
    vendor: &str,
    persona: &str,
    model: Option<&str>,
    thinking_level: Option<&str>,
    prompt: &str,
) -> Option<LaunchSpec> {
    launch_spec_for_with_session(
        env,
        vendor,
        persona,
        model,
        thinking_level,
        fl_core::permissions::Posture::Auto,
        prompt,
        None,
        None,
        None,
    )
    .ok()
    .flatten()
}

#[allow(clippy::too_many_arguments)]
fn launch_spec_for_with_session(
    env: &LifecycleEnv<'_>,
    vendor: &str,
    persona: &str,
    model: Option<&str>,
    thinking_level: Option<&str>,
    posture: fl_core::permissions::Posture,
    prompt: &str,
    cwd: Option<&str>,
    vendor_session_id: Option<&str>,
    hydration_generation: Option<&str>,
) -> Result<Option<LaunchSpec>, LifecycleError> {
    let home = env
        .home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let options = env.vendor_launch_options_for_vendor(vendor, posture);
    if let Some(host_adapter) = env.host_vendor_adapter(vendor) {
        let mut binding = fl_core::AdapterIdentityBinding::new(
            format!("{vendor}:{persona}"),
            1,
            hosted_tmux_target(vendor, persona),
            None,
        );
        if let Some(sid) = vendor_session_id {
            binding = binding.with_vendor_session_id(sid);
        }
        let req = fl_core::VendorLaunchRequest {
            binding,
            persona: persona.to_string(),
            model: model.map(str::to_string),
            thinking_effort: thinking_level.map(str::to_string),
            initial_prompt: prompt.to_string(),
            cwd: cwd.map(PathBuf::from),
            home_dir: home.clone(),
            remote_control: options.remote_control,
            posture,
            auto_approval: options.auto_approval,
        };
        match host_adapter.prepare_launch(&req) {
            Ok(plan) => {
                let mut spec = LaunchSpec {
                    env: plan.env,
                    argv: plan.argv,
                };
                apply_vendor_launch_env(
                    &mut spec.env,
                    vendor,
                    persona,
                    home.as_deref(),
                    hydration_generation,
                );
                return Ok(Some(spec));
            }
            Err(fl_core::VendorAdapterError::PermissionRefused { reason, .. }) => {
                let detail = reason
                    .strip_prefix("posture:")
                    .map(str::trim_start)
                    .unwrap_or(&reason);
                return Err(invalid_field("posture", detail));
            }
            Err(host_err) => {
                if vendor_session_id.is_some() {
                    return Err(LifecycleError::Invalid(host_err.to_string()));
                }
                let Some(adapter) = env.vendor_adapter(vendor) else {
                    return Err(LifecycleError::Invalid(host_err.to_string()));
                };
                let mut spec = adapter
                    .build_launch_spec_with_options(
                        persona,
                        model,
                        thinking_level,
                        prompt,
                        home.as_deref(),
                        options,
                    )
                    .map_err(|why| invalid_field("posture", &why))?;
                apply_vendor_launch_env(
                    &mut spec.env,
                    vendor,
                    persona,
                    home.as_deref(),
                    hydration_generation,
                );
                return Ok(Some(spec));
            }
        }
    }
    let Some(adapter) = env.vendor_adapter(vendor) else {
        return Ok(None);
    };
    let mut spec = adapter
        .build_launch_spec_with_options(
            persona,
            model,
            thinking_level,
            prompt,
            home.as_deref(),
            options,
        )
        .map_err(|why| invalid_field("posture", &why))?;
    apply_vendor_launch_env(
        &mut spec.env,
        vendor,
        persona,
        home.as_deref(),
        hydration_generation,
    );
    Ok(Some(spec))
}

#[allow(clippy::too_many_arguments)]
fn resume_spec_for_with_session(
    env: &LifecycleEnv<'_>,
    vendor: &str,
    persona: &str,
    model: Option<&str>,
    thinking_level: Option<&str>,
    posture: fl_core::permissions::Posture,
    cwd: Option<&str>,
    vendor_session_id: &str,
    hydration_generation: Option<&str>,
) -> Result<Option<LaunchSpec>, LifecycleError> {
    let home = env
        .home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    let options = env.vendor_launch_options_for_vendor(vendor, posture);
    if let Some(host_adapter) = env.host_vendor_adapter(vendor) {
        let binding = fl_core::AdapterIdentityBinding::new(
            format!("{vendor}:{persona}"),
            1,
            hosted_tmux_target(vendor, persona),
            None,
        )
        .with_vendor_session_id(vendor_session_id);
        let req = fl_core::AttachOrResumeRequest {
            binding,
            vendor_session_id: vendor_session_id.to_string(),
            control_endpoint: None,
            cwd: cwd.map(PathBuf::from),
            home_dir: home.clone(),
            persona: Some(persona.to_string()),
            model: model.map(str::to_string),
            thinking_effort: thinking_level.map(str::to_string),
            remote_control: options.remote_control,
            posture,
            auto_approval: options.auto_approval,
        };
        match host_adapter.prepare_resume(&req) {
            Ok(plan) => {
                let mut spec = LaunchSpec {
                    env: plan.env,
                    argv: plan.argv,
                };
                apply_vendor_launch_env(
                    &mut spec.env,
                    vendor,
                    persona,
                    home.as_deref(),
                    hydration_generation,
                );
                return Ok(Some(spec));
            }
            Err(fl_core::VendorAdapterError::PermissionRefused { reason, .. }) => {
                let detail = reason
                    .strip_prefix("posture:")
                    .map(str::trim_start)
                    .unwrap_or(&reason);
                return Err(invalid_field("posture", detail));
            }
            Err(fl_core::VendorAdapterError::Unsupported { .. }) => {}
            Err(host_err) => {
                return Err(LifecycleError::Invalid(host_err.to_string()));
            }
        }
    }
    let Some(adapter) = env.vendor_adapter(vendor) else {
        return Ok(None);
    };
    let mut spec = adapter
        .build_resume_spec_with_options(
            persona,
            vendor_session_id,
            model,
            thinking_level,
            home.as_deref(),
            options,
        )
        .map_err(|why| invalid_field("posture", &why))?;
    apply_vendor_launch_env(
        &mut spec.env,
        vendor,
        persona,
        home.as_deref(),
        hydration_generation,
    );
    Ok(Some(spec))
}

fn find_active_claude_transcript_path(
    env: &LifecycleEnv<'_>,
    vendor_session_id: &str,
) -> Option<PathBuf> {
    let home = env
        .home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    let found = if let Some(host_adapter) = env.host_vendor_adapter("claude") {
        match host_adapter.find_active_session_transcript_path(&home, vendor_session_id) {
            Ok(path) => path,
            Err(fl_core::VendorAdapterError::Unsupported { .. }) => {
                env.vendor_adapter("claude").and_then(|adapter| {
                    adapter
                        .find_active_session_transcript_path(&home, vendor_session_id)
                        .ok()
                        .flatten()
                })
            }
            Err(_) => None,
        }
    } else {
        env.vendor_adapter("claude").and_then(|adapter| {
            adapter
                .find_active_session_transcript_path(&home, vendor_session_id)
                .ok()
                .flatten()
        })
    };
    found.filter(|p| fl_core::vendor_adapter::validate_claude_transcript_path(p).is_ok())
}

fn repair_claude_transcript_tail(
    env: &LifecycleEnv<'_>,
    transcript_path: &Path,
) -> Result<usize, fl_core::VendorAdapterError> {
    if let Some(host_adapter) = env.host_vendor_adapter("claude") {
        match host_adapter.repair_session_transcript_tail(transcript_path) {
            Err(fl_core::VendorAdapterError::Unsupported { .. }) => {}
            res => return res,
        }
    }
    if let Some(adapter) = env.vendor_adapter("claude") {
        return adapter.repair_session_transcript_tail(transcript_path);
    }
    Err(fl_core::VendorAdapterError::Unsupported {
        vendor: "claude".to_string(),
        operation: "repair_session_transcript_tail".to_string(),
        reason: "no Claude adapter registered".to_string(),
    })
}

fn estimate_claude_post_compaction_context(
    env: &LifecycleEnv<'_>,
    transcript_path: &Path,
) -> Result<fl_core::PostCompactionContext, fl_core::VendorAdapterError> {
    if let Some(host_adapter) = env.host_vendor_adapter("claude") {
        match host_adapter.estimate_post_compaction_context(transcript_path) {
            Err(fl_core::VendorAdapterError::Unsupported { .. }) => {}
            res => return res,
        }
    }
    if let Some(adapter) = env.vendor_adapter("claude") {
        return adapter.estimate_post_compaction_context(transcript_path);
    }
    Err(fl_core::VendorAdapterError::Unsupported {
        vendor: "claude".to_string(),
        operation: "estimate_post_compaction_context".to_string(),
        reason: "no Claude adapter registered".to_string(),
    })
}

/// Sync `record.vendor_session_id` from a `VendorRoute::Claude` for `record.persona` when an
/// interactive `/clear` rotated the active Claude session ID, the hosted runtime unit is dead,
/// the route's PID is dead (`*pid > 0 && !claude_pid_is_alive(*pid)`), and provenance matches this
/// hosted session (`(hosted_id, hydration_generation)` on the route entry or `record.pid == pid`).
fn sync_claude_vendor_session_id_from_route<S: StateStore>(
    _store: &S,
    env: &LifecycleEnv<'_>,
    record: &mut HostedSessionRecord,
) -> bool {
    if record.vendor != "claude" {
        return false;
    }
    let target = record
        .tmux_target
        .clone()
        .unwrap_or_else(|| hosted_tmux_target(&record.vendor, &record.persona));
    let unit = env.runtime.unit(&target);
    if env.spawn_runtime && env.runtime.exists(unit) {
        return false;
    }
    for path in route_table_paths(env.home_override) {
        if !path.is_file() {
            continue;
        }
        let table = RouteTable::load(Some(&path));
        let Some(entry) = table.get(&record.persona) else {
            continue;
        };
        let VendorRoute::Claude {
            pid, session_id, ..
        } = &entry.route
        else {
            continue;
        };
        if *pid == 0 || claude_pid_is_alive(*pid) {
            continue;
        }
        if !route_entry_matches_unit_provenance(
            entry,
            &record.session_id,
            record.hydration_generation.as_deref(),
        ) {
            continue;
        }
        let eff_hid = entry
            .hosted_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let eff_gen = entry
            .hydration_generation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let route_meta_matches = eff_hid
            .is_some_and(|h| h.eq_ignore_ascii_case(&record.session_id))
            && eff_gen.is_some_and(|g| record.hydration_generation.as_deref() == Some(g));
        let pid_matches = record.pid == Some(*pid);
        if !route_meta_matches && !pid_matches {
            continue;
        }
        let sid = session_id.trim();
        if !is_native_session_id(sid)
            || record
                .tombstone_ids
                .iter()
                .any(|t| t.eq_ignore_ascii_case(sid))
            || record.vendor_session_id.as_deref() == Some(sid)
        {
            continue;
        }
        if find_active_claude_transcript_path(env, sid).is_some() {
            record.vendor_session_id = Some(sid.to_string());
            return true;
        }
    }
    false
}

enum ClaudeResumeDecision {
    ResumeExisting {
        vendor_session_id: String,
        hydration_generation: String,
        launch: Option<LaunchSpec>,
    },
    FreshLaunch {
        reason_code: &'static str,
        detail: String,
    },
}

fn push_claude_resume_fallback_notice(
    env: &LifecycleEnv<'_>,
    persona: &str,
    warnings: &mut Vec<String>,
    reason_code: &str,
    detail: &str,
) {
    let notice = format!(
        "claude:{persona} resume fallback ({reason_code}): {detail}; launching fresh session"
    );
    if env.log_hydration_warnings {
        eprintln!("hub: warning ({persona} / claude:{persona}): {notice}");
    }
    warnings.push(notice);
}

#[allow(clippy::too_many_arguments)]
fn evaluate_claude_resume_eligibility<S: StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    record: &mut HostedSessionRecord,
    recast_staged: bool,
    effective_model: Option<&str>,
    effective_thinking_level: Option<&str>,
    effective_context_window: Option<u64>,
    posture: fl_core::permissions::Posture,
    cwd: Option<&str>,
) -> Result<ClaudeResumeDecision, LifecycleError> {
    let mut resume_state = load_claude_resume_state(store, &record.session_id);
    let launch_started_ms = resume_state
        .resume_launched_at_ms
        .unwrap_or(record.updated_at_ms);
    let unconfirmed_died = resume_state
        .resume_unconfirmed_session_id
        .as_deref()
        .is_some_and(|unconfirmed_sid| {
            !claude_route_confirmed_for_session(
                env,
                &record.persona,
                &record.session_id,
                unconfirmed_sid,
                record.hydration_generation.as_deref(),
                record.pid,
                launch_started_ms,
            )
        });
    if resume_state.resume_failed || unconfirmed_died {
        let failed_sid = resume_state
            .resume_unconfirmed_session_id
            .take()
            .or_else(|| record.vendor_session_id.clone())
            .unwrap_or_default();
        resume_state.resume_failed = false;
        resume_state.resume_launched_at_ms = None;
        save_claude_resume_state(store, &record.session_id, &resume_state, "hub")?;
        if !failed_sid.is_empty() {
            push_tombstone(&mut record.tombstone_ids, &failed_sid);
        }
        return Ok(ClaudeResumeDecision::FreshLaunch {
            reason_code: "resume_launch_did_not_survive",
            detail: format!(
                "previous --resume `{failed_sid}` for `{}` exited before route confirmation",
                record.persona
            ),
        });
    }
    if resume_state.resume_unconfirmed_session_id.is_some()
        || resume_state.resume_launched_at_ms.is_some()
    {
        resume_state.resume_unconfirmed_session_id = None;
        resume_state.resume_launched_at_ms = None;
        save_claude_resume_state(store, &record.session_id, &resume_state, "hub")?;
    }

    if recast_staged {
        return Ok(ClaudeResumeDecision::FreshLaunch {
            reason_code: "recast_guidance_staged",
            detail: format!(
                "recast assignment staged (recast_staged) for `{}`",
                record.persona
            ),
        });
    }
    let _ = promote_confirmed_hydration(store, record);
    let Some(existing_gen) = record
        .hydration_generation
        .clone()
        .filter(|g| is_valid_hydration_generation(g))
        .filter(|_| record.hydration_status == HydrationStatus::Confirmed)
    else {
        return Ok(ClaudeResumeDecision::FreshLaunch {
            reason_code: "unconfirmed_hydration",
            detail: format!("hydration status is `{}`", record.hydration_status),
        });
    };
    let Some(sid) = record
        .vendor_session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| {
            is_native_session_id(s)
                && !record
                    .tombstone_ids
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(s))
        })
        .map(str::to_string)
    else {
        return Ok(ClaudeResumeDecision::FreshLaunch {
            reason_code: "missing_or_tombstoned_session_id",
            detail: "vendor_session_id is missing, invalid, or tombstoned".to_string(),
        });
    };
    let Some(transcript_path) = find_active_claude_transcript_path(env, &sid) else {
        return Ok(ClaudeResumeDecision::FreshLaunch {
            reason_code: "missing_or_archived_transcript",
            detail: format!(
                "missing_transcript: no active transcript found for session `{sid}` under ~/.claude/projects/"
            ),
        });
    };
    if let Ok(meta) = std::fs::metadata(&transcript_path)
        && meta.len() > fl_core::vendor_adapter::MAX_CLAUDE_TRANSCRIPT_READ_BYTES
    {
        return Ok(ClaudeResumeDecision::FreshLaunch {
            reason_code: "transcript_too_large",
            detail: format!(
                "transcript `{}` size ({} bytes) exceeds cap ({} bytes)",
                transcript_path.display(),
                meta.len(),
                fl_core::vendor_adapter::MAX_CLAUDE_TRANSCRIPT_READ_BYTES
            ),
        });
    }
    let ctx = match estimate_claude_post_compaction_context(env, &transcript_path) {
        Ok(ctx) if ctx.turn_count > 0 => ctx,
        Ok(_) => {
            return Ok(ClaudeResumeDecision::FreshLaunch {
                reason_code: "empty_or_unparseable_transcript",
                detail: format!(
                    "transcript `{}` has 0 normalized turns",
                    transcript_path.display()
                ),
            });
        }
        Err(err) => {
            let reason_code = if err.to_string().contains("exceeds maximum size") {
                "transcript_too_large"
            } else {
                "empty_or_unparseable_transcript"
            };
            return Ok(ClaudeResumeDecision::FreshLaunch {
                reason_code,
                detail: format!(
                    "transcript parse failed for `{}`: {err}",
                    transcript_path.display()
                ),
            });
        }
    };
    if let (Some(transcript_model), Some(target_model)) =
        (ctx.latest_model.as_deref(), effective_model)
    {
        let target_window = crate::context_window::claude_session_context_window(
            effective_context_window,
            Some(target_model),
            None,
        );
        let transcript_window = crate::context_window::claude_session_context_window(
            None,
            None,
            Some(transcript_model),
        );
        if target_window < transcript_window {
            return Ok(ClaudeResumeDecision::FreshLaunch {
                reason_code: "model_context_window_shrunk",
                detail: format!(
                    "target model `{target_model}` window ({target_window}) is smaller than transcript model `{transcript_model}` window ({transcript_window})"
                ),
            });
        }
    }
    let raw_max_ceiling = std::env::var("FL_CLAUDE_MAX_RESUME_ACTIVE_TOKENS").ok();
    let (max_ceiling, max_ceiling_warning) =
        crate::context_window::parse_claude_max_resume_active_tokens_override(
            raw_max_ceiling.as_deref(),
        );
    if let Some(warn) = max_ceiling_warning
        && env.log_hydration_warnings
    {
        eprintln!(
            "hub: warning ({} / {}): {warn}",
            record.persona, record.session_id
        );
    }
    let window_tokens = crate::context_window::claude_session_context_window(
        effective_context_window,
        effective_model,
        ctx.latest_model.as_deref(),
    );
    let cap = crate::context_window::claude_resume_active_token_cap(window_tokens, max_ceiling);
    if let Some(active_tokens) = ctx.active_context_tokens {
        if active_tokens > cap {
            return Ok(ClaudeResumeDecision::FreshLaunch {
                reason_code: "context_over_cap",
                detail: format!(
                    "active_context_tokens_exceeded: post-compaction active_context_tokens ({active_tokens}) exceeds cap ({cap}) for window ({window_tokens})"
                ),
            });
        }
    } else if ctx.post_compact_conversational_bytes
        > crate::context_window::MAX_CLAUDE_RESUME_POST_COMPACT_CONVERSATIONAL_BYTES
    {
        return Ok(ClaudeResumeDecision::FreshLaunch {
            reason_code: "context_over_cap",
            detail: format!(
                "active_context_tokens_exceeded: post-compaction conversational bytes ({}) exceeds cap ({})",
                ctx.post_compact_conversational_bytes,
                crate::context_window::MAX_CLAUDE_RESUME_POST_COMPACT_CONVERSATIONAL_BYTES
            ),
        });
    }
    let launch = resume_spec_for_with_session(
        env,
        "claude",
        &record.persona,
        effective_model,
        effective_thinking_level,
        posture,
        cwd,
        &sid,
        Some(&existing_gen),
    )?;
    match repair_claude_transcript_tail(env, &transcript_path) {
        Ok(truncated_bytes) => {
            if truncated_bytes > 0 && env.log_hydration_warnings {
                eprintln!(
                    "hub: warning ({} / claude:{}): repaired {truncated_bytes} torn trailing byte(s) in `{}`",
                    record.persona,
                    record.persona,
                    transcript_path.display()
                );
            }
        }
        Err(err) => {
            return Ok(ClaudeResumeDecision::FreshLaunch {
                reason_code: "empty_or_unparseable_transcript",
                detail: format!(
                    "transcript tail repair failed for `{}`: {err}",
                    transcript_path.display()
                ),
            });
        }
    }
    Ok(ClaudeResumeDecision::ResumeExisting {
        vendor_session_id: sid,
        hydration_generation: existing_gen,
        launch,
    })
}

#[cfg(test)]
fn resolve_muse_bin(configured: Option<std::ffi::OsString>, home: Option<&Path>) -> String {
    resolve_vendor_bin(configured, home, "muse")
}

/// Build the `(env, env_allow)` pair passed to `fl-host` when spawning a hosted session.
///
/// Any agent-identity variable (`AGENT_NAME`, `AGENT_PERSONA`, `AGENT_HYDRATION_GENERATION`,
/// `FL_HOSTED_ID`) not explicitly populated in `env` is removed from `env_allow` so `fl-host`
/// never inherits ambient parent identity variables into a child session.
pub fn build_host_spawn_env(
    launch: Option<&LaunchSpec>,
    hosted_id: &str,
) -> (BTreeMap<String, String>, BTreeSet<String>) {
    let mut env_map = BTreeMap::new();
    let mut env_allow: BTreeSet<String> = fl_core::child_env::BASELINE_EXACT
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Some(spec) = launch {
        for (k, v) in &spec.env {
            env_allow.insert(k.clone());
            env_map.insert(k.clone(), v.clone());
        }
    }
    // Interactive CLIs pick their color depth from these; a launch spec's own value wins.
    for (key, default) in [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")] {
        env_allow.insert(key.to_string());
        env_map
            .entry(key.to_string())
            .or_insert_with(|| default.to_string());
    }
    if !hosted_id.is_empty() {
        env_allow.insert("FL_HOSTED_ID".to_string());
        env_map.insert("FL_HOSTED_ID".to_string(), hosted_id.to_string());
    }
    for key in [
        "AGENT_NAME",
        "AGENT_PERSONA",
        "AGENT_HYDRATION_GENERATION",
        "FL_HOSTED_ID",
    ] {
        if !env_map.contains_key(key) {
            env_allow.remove(key);
        }
    }
    (env_map, env_allow)
}
/// The native runtime: `fl-host`, falling back to `TmuxRuntime` only when `FL_WIRE_TMUX_BIN` is
/// explicitly configured and `fl-host` is not reachable.
#[derive(Debug, Clone, Copy)]
pub struct HostRuntime;

fn use_legacy_tmux_fallback() -> bool {
    std::env::var_os("FL_WIRE_TMUX_BIN").is_some()
        && fl_host_proto::HostEndpoint::discover()
            .and_then(|ep| fl_host_proto::BlockingHostClient::connect(&ep))
            .is_err()
}

fn connect_host_client() -> Option<fl_host_proto::BlockingHostClient> {
    let endpoint = fl_host_proto::HostEndpoint::discover().ok()?;
    fl_host_proto::BlockingHostClient::connect(&endpoint).ok()
}

fn exact_host_unit(unit: &str, hosted_id: &str) -> Option<fl_host_proto::SessionId> {
    let target = if unit.contains(':') || unit.contains('.') {
        unit.to_owned()
    } else {
        let (vendor, persona) = hosted_id.split_once(':')?;
        let target = hosted_tmux_target(vendor, persona);
        if target.split(':').next()? != unit {
            return None;
        }
        target
    };
    fl_wire::vendor_adapter::unit_to_host_session_id(&target)
}

fn host_supports_session_owner(client: &mut fl_host_proto::BlockingHostClient) -> Result<bool, ()> {
    match client.extension_request(fl_host_proto::ExtensionRequest::Capabilities) {
        Ok(fl_host_proto::ExtensionResponse::Capabilities { features, .. }) => {
            Ok(features & fl_host_proto::SESSION_OWNER_FEATURE != 0)
        }
        _ => Err(()),
    }
}

fn host_owner_upgrade_required() -> LifecycleError {
    LifecycleError::Conflict(
        "host lacks SESSION_OWNER_FEATURE; upgrade and restart ferroloop-host.service before launching or adopting a hosted session".to_string(),
    )
}

fn kill_host_unit(unit: &str, hosted_id: &str, owner_mode: bool) -> bool {
    if !is_killable_unit(unit) {
        return false;
    }
    if !owner_mode && is_muse_window_target(unit) && hosted_id.is_empty() {
        return false;
    }
    let Some(mut client) = connect_host_client() else {
        return false;
    };
    let Ok(sessions) = client.list_sessions() else {
        return false;
    };
    let Some(wanted) = exact_host_unit(unit, hosted_id) else {
        return false;
    };
    let ids: Vec<_> = sessions
        .iter()
        .filter(|s| {
            s.stream.session == wanted && matches!(s.state, fl_host_proto::ChildState::Running(_))
        })
        .map(|s| s.stream.session.clone())
        .collect();
    if ids.is_empty() {
        return true;
    }
    let Ok(owner_capable) = host_supports_session_owner(&mut client) else {
        return false;
    };
    // A durable runtime_spawned record is the compatibility proof for an older daemon.
    // Owner adoption has no such proof and must never use this fallback.
    if owner_mode && !owner_capable {
        return false;
    }
    let mut all_ok = true;
    for id in &ids {
        let Some(summary) = sessions.iter().find(|s| {
            s.stream.session == *id && matches!(s.state, fl_host_proto::ChildState::Running(_))
        }) else {
            continue;
        };
        if owner_capable
            && !client
                .check_session_owner(summary.stream.clone(), hosted_id)
                .unwrap_or(false)
        {
            all_ok = false;
            continue;
        }
        let killed = client
            .claim_controller(summary.stream.clone())
            .and_then(|grant| client.kill_session(summary.stream.clone(), grant.lease))
            .is_ok();
        if !killed {
            all_ok = false;
        }
    }
    for _ in 0..50 {
        let Ok(after) = client.list_sessions() else {
            break;
        };
        let still_active = after.iter().any(|s| {
            ids.contains(&s.stream.session)
                && !matches!(s.state, fl_host_proto::ChildState::CleanupComplete(_))
        });
        if !still_active {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    all_ok
}

impl SessionRuntime for HostRuntime {
    fn preflight_spawn(&self, hosted_id: &str) -> Result<(), LifecycleError> {
        if use_legacy_tmux_fallback() {
            return Ok(());
        }
        if hosted_id.is_empty() || hosted_id.len() > fl_host_proto::MAX_HOSTED_OWNER_BYTES {
            return Err(LifecycleError::Invalid(format!(
                "hosted session id must contain 1..={} bytes for SESSION_OWNER_FEATURE",
                fl_host_proto::MAX_HOSTED_OWNER_BYTES
            )));
        }
        let mut client = connect_host_client().ok_or_else(|| {
            LifecycleError::Conflict("cannot connect to ferroloop-host.service".to_string())
        })?;
        if !host_supports_session_owner(&mut client).unwrap_or(false) {
            return Err(host_owner_upgrade_required());
        }
        Ok(())
    }

    fn preflight_stop(
        &self,
        unit: &str,
        hosted_id: &str,
        recorded: bool,
    ) -> Result<(), LifecycleError> {
        if use_legacy_tmux_fallback() {
            return Ok(());
        }
        let wanted = exact_host_unit(unit, hosted_id)
            .ok_or_else(|| LifecycleError::Invalid(format!("invalid host session `{unit}`")))?;
        let refusal = || {
            let wanted = wanted.as_str();
            LifecycleError::Conflict(format!(
                "cannot verify ownership of host session `{wanted}`; inspect it and manually stop it with `fl host kill {wanted}` if appropriate"
            ))
        };
        let mut client = connect_host_client().ok_or_else(refusal)?;
        let sessions = client.list_sessions().map_err(|_| refusal())?;
        let Some(session) = sessions.iter().find(|s| {
            s.stream.session == wanted && matches!(s.state, fl_host_proto::ChildState::Running(_))
        }) else {
            return Ok(());
        };
        if !host_supports_session_owner(&mut client).map_err(|_| refusal())? {
            return if recorded {
                Ok(())
            } else {
                Err(host_owner_upgrade_required())
            };
        }
        if recorded
            && !client
                .check_session_owner(session.stream.clone(), hosted_id)
                .unwrap_or(false)
        {
            return Err(refusal());
        }
        Ok(())
    }

    fn separate_vendor_units(&self) -> bool {
        !use_legacy_tmux_fallback()
    }

    fn unit<'a>(&self, target: &'a str) -> &'a str {
        if use_legacy_tmux_fallback() {
            runtime_unit(target)
        } else {
            target
        }
    }

    fn spawn(
        &self,
        session: &str,
        window: &str,
        cwd: Option<&str>,
        launch: Option<&LaunchSpec>,
        hosted_id: &str,
    ) -> bool {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.spawn(session, window, cwd, launch, hosted_id);
        }
        let session = session.trim();
        let window = window.trim();
        if session.is_empty()
            || (session == MUSE_SESSION && (window.is_empty() || hosted_id.trim().is_empty()))
        {
            return false;
        }
        let Some(mut client) = connect_host_client() else {
            return false;
        };
        // Ownership must be verifiable before a child is created: an older host cannot
        // satisfy the survival check or safely clean up the newly launched runtime.
        if hosted_id.is_empty()
            || hosted_id.len() > fl_host_proto::MAX_HOSTED_OWNER_BYTES
            || !matches!(client.extension_request(fl_host_proto::ExtensionRequest::Capabilities),
                Ok(fl_host_proto::ExtensionResponse::Capabilities { features, .. })
                    if features & fl_host_proto::SESSION_OWNER_FEATURE != 0)
        {
            return false;
        }
        let Ok(sessions) = client.list_sessions() else {
            return false;
        };
        let raw_id = if window.is_empty() {
            session.to_string()
        } else {
            format!("{session}.{window}")
        };
        let Ok(session_id) = fl_host_proto::SessionId::new(&raw_id) else {
            return false;
        };
        if sessions.iter().any(|s| {
            s.stream.session == session_id
                && !matches!(s.state, fl_host_proto::ChildState::CleanupComplete(_))
        }) {
            return false;
        }
        let argv = match launch {
            Some(spec) if !spec.argv.is_empty() => spec.argv.clone(),
            _ => vec!["/bin/sh".to_string()],
        };
        let cwd_path = cwd
            .filter(|d| Path::new(d).is_dir())
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        let (mut env_map, mut env_allow) = build_host_spawn_env(launch, hosted_id);
        let use_pty_mode = cfg!(unix)
            && std::env::var_os("FL_HOST_DISABLE_PTY_MODE")
                .is_none_or(|v| v != "1" && !v.eq_ignore_ascii_case("true"));
        if use_pty_mode {
            let pty_spec = fl_host_proto::SpawnSpec {
                session: session_id.clone(),
                kind: fl_host_proto::SpawnKind::Pty,
                argv: argv.clone(),
                cwd: cwd_path.clone(),
                env: env_map.clone(),
                env_allow: env_allow.clone(),
            };
            match client.spawn_pty_session(pty_spec, None) {
                Ok(_) => return true,
                Err(err)
                    if matches!(err, fl_host_proto::HostError::Unsupported { .. })
                        || err.to_string().contains("Unsupported") => {}
                Err(_) => return false,
            }
        }
        env_allow.insert("FL_HOST_PTY".to_string());
        env_map.insert("FL_HOST_PTY".to_string(), "1".to_string());
        let pipe_spec = fl_host_proto::SpawnSpec {
            session: session_id,
            kind: fl_host_proto::SpawnKind::Pipe,
            argv,
            cwd: cwd_path,
            env: env_map,
            env_allow,
        };
        client.spawn_reliable_session(pipe_spec).is_ok()
    }

    fn kill(&self, unit: &str, hosted_id: &str) -> bool {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.kill(unit, hosted_id);
        }
        kill_host_unit(unit, hosted_id, false)
    }

    fn kill_owner(&self, unit: &str, hosted_id: &str) -> bool {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.kill_owner(unit, hosted_id);
        }
        kill_host_unit(unit, hosted_id, true)
    }

    fn capture(&self, unit: &str) -> String {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.capture(unit);
        }
        let Some(mut client) = connect_host_client() else {
            return String::new();
        };
        let sessions = client.list_sessions().ok().unwrap_or_default();
        let target_id = fl_wire::vendor_adapter::matching_live_host_sessions(&sessions, unit)
            .into_iter()
            .next()
            .or_else(|| fl_wire::vendor_adapter::unit_to_host_session_id(unit));
        let Some(id) = target_id else {
            return String::new();
        };
        if let Some(summary) = sessions
            .iter()
            .find(|s| {
                s.stream.session == id && matches!(s.state, fl_host_proto::ChildState::Running(_))
            })
            .or_else(|| sessions.iter().find(|s| s.stream.session == id))
            && summary.mode == fl_host_proto::SessionMode::Pty
            && let Ok(screen) = client.capture_rendered(summary.stream.clone())
        {
            return screen.contents;
        }
        client.capture_output(id, 16_384).unwrap_or_default()
    }

    fn send_keys(&self, unit: &str, keys: &str) -> bool {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.send_keys(unit, keys);
        }
        if keys.is_empty() {
            return false;
        }
        let Some(mut client) = connect_host_client() else {
            return false;
        };
        let Ok(sessions) = client.list_sessions() else {
            return false;
        };
        let target_id = fl_wire::vendor_adapter::matching_live_host_sessions(&sessions, unit)
            .into_iter()
            .next()
            .or_else(|| fl_wire::vendor_adapter::unit_to_host_session_id(unit));
        let Some(id) = target_id else {
            return false;
        };
        let Some(summary) = sessions.iter().find(|s| {
            s.stream.session == id && matches!(s.state, fl_host_proto::ChildState::Running(_))
        }) else {
            return false;
        };
        let Ok(grant) = client.claim_controller(summary.stream.clone()) else {
            return false;
        };
        match keys {
            "Escape" => matches!(
                client.write_raw(summary.stream.clone(), grant.lease, b"\x1b".to_vec()),
                Ok(fl_host_proto::SendOutcome::Written)
            ),
            "C-c" => client
                .signal_session(
                    summary.stream.clone(),
                    grant.lease,
                    fl_host_proto::SignalKind::Interrupt,
                )
                .is_ok(),
            "Enter" => matches!(
                client.write_raw(summary.stream.clone(), grant.lease, b"\r".to_vec()),
                Ok(fl_host_proto::SendOutcome::Written)
            ),
            // Text then Enter as separate raw writes: a raw-mode TUI would take a `send_line`
            // newline as part of the text and never submit it.
            other => fl_wire::dispatcher::host_write_text_then_enter(
                &mut client,
                summary.stream.clone(),
                grant.lease,
                other,
            )
            .is_ok(),
        }
    }

    fn exists(&self, unit: &str) -> bool {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.exists(unit);
        }
        let Some(mut client) = connect_host_client() else {
            return false;
        };
        client.list_sessions().is_ok_and(|sessions| {
            !fl_wire::vendor_adapter::matching_live_host_sessions(&sessions, unit).is_empty()
        })
    }

    fn presence(&self, unit: &str) -> RuntimePresence {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.presence(unit);
        }
        let Some(mut client) = connect_host_client() else {
            return RuntimePresence::Unknown;
        };
        let Ok(sessions) = client.list_sessions() else {
            return RuntimePresence::Unknown;
        };
        if !fl_wire::vendor_adapter::matching_live_host_sessions(&sessions, unit).is_empty() {
            RuntimePresence::Alive
        } else {
            RuntimePresence::Gone
        }
    }

    fn owns(&self, unit: &str, hosted_id: &str) -> bool {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.owns(unit, hosted_id);
        }
        let Some(wanted) = exact_host_unit(unit, hosted_id) else {
            return false;
        };
        let Some(mut client) = connect_host_client() else {
            return false;
        };
        let Ok(sessions) = client.list_sessions() else {
            return false;
        };
        sessions
            .into_iter()
            .find(|s| {
                s.stream.session == wanted
                    && matches!(s.state, fl_host_proto::ChildState::Running(_))
            })
            .is_some_and(|s| {
                client
                    .check_session_owner(s.stream, hosted_id)
                    .unwrap_or(false)
            })
    }

    fn recorded_pid(&self, unit: &str, hosted_id: &str) -> Option<u32> {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.recorded_pid(unit, hosted_id);
        }
        let wanted = exact_host_unit(unit, hosted_id)?;
        let mut client = connect_host_client()?;
        let sessions = client.list_sessions().ok()?;
        select_host_recorded_pid(&sessions, &wanted)
    }

    fn recorded_leader_pid(&self, unit: &str, hosted_id: &str) -> Option<u32> {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.recorded_leader_pid(unit, hosted_id);
        }
        let wanted = exact_host_unit(unit, hosted_id)?;
        let sessions = connect_host_client()?.list_sessions().ok()?;
        select_host_recorded_leader_pid(&sessions, &wanted)
    }

    fn recorded_exit_code(&self, unit: &str, expected_pid: Option<u32>) -> Option<i32> {
        if use_legacy_tmux_fallback() {
            return TmuxRuntime.recorded_exit_code(unit, expected_pid);
        }
        let wanted = fl_wire::vendor_adapter::unit_to_host_session_id(unit)?;
        let mut client = connect_host_client()?;
        let sessions = client.list_sessions().ok()?;
        select_host_recorded_exit_code(&sessions, &wanted, expected_pid)
    }

    fn settle_delay(&self) -> std::time::Duration {
        std::time::Duration::from_millis(50)
    }
}

fn select_host_recorded_pid(
    sessions: &[fl_host_proto::SessionSummary],
    wanted: &fl_host_proto::SessionId,
) -> Option<u32> {
    select_host_recorded_leader_pid(sessions, wanted)
        .and_then(fl_wire::identity::resolve_vendor_cli_pid)
}

fn select_host_recorded_leader_pid(
    sessions: &[fl_host_proto::SessionSummary],
    wanted: &fl_host_proto::SessionId,
) -> Option<u32> {
    sessions
        .iter()
        .filter(|s| &s.stream.session == wanted)
        .max_by_key(|s| {
            (
                matches!(s.state, fl_host_proto::ChildState::Running(_)),
                s.stream.epoch.0,
            )
        })
        .and_then(|s| match &s.state {
            fl_host_proto::ChildState::Running(r)
            | fl_host_proto::ChildState::LeaderExited(r)
            | fl_host_proto::ChildState::CleanupComplete(r) => (r.pid > 0).then_some(r.pid),
        })
}

fn select_host_recorded_exit_code(
    sessions: &[fl_host_proto::SessionSummary],
    wanted: &fl_host_proto::SessionId,
    expected_pid: Option<u32>,
) -> Option<i32> {
    sessions
        .iter()
        .filter(|s| &s.stream.session == wanted)
        .max_by_key(|s| s.stream.epoch.0)
        .and_then(|s| match &s.state {
            fl_host_proto::ChildState::LeaderExited(r)
            | fl_host_proto::ChildState::CleanupComplete(r) => {
                if let Some(epid) = expected_pid.filter(|&p| p > 0)
                    && r.pid > 0
                    && r.pid != epid
                {
                    return None;
                }
                r.exit
                    .as_ref()
                    .filter(|e| e.signal.is_none())
                    .and_then(|e| e.code)
            }
            fl_host_proto::ChildState::Running(_) => None,
        })
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
        hosted_id: &str,
    ) -> bool {
        spawn_tmux_runtime(session, window, cwd, launch, hosted_id)
    }

    fn kill(&self, unit: &str, hosted_id: &str) -> bool {
        kill_tmux_runtime(unit, hosted_id)
    }

    fn kill_owner(&self, unit: &str, hosted_id: &str) -> bool {
        kill_tmux_runtime_owner(unit, hosted_id)
    }

    fn capture(&self, unit: &str) -> String {
        capture_tmux_pane(unit)
    }

    fn send_keys(&self, unit: &str, keys: &str) -> bool {
        send_tmux_keys(unit, keys)
    }

    fn exists(&self, unit: &str) -> bool {
        tmux_unit_exists(unit)
    }

    fn presence(&self, unit: &str) -> RuntimePresence {
        tmux_runtime_presence(unit)
    }

    fn owns(&self, unit: &str, hosted_id: &str) -> bool {
        tmux_unit_owned(unit, hosted_id)
    }

    fn recorded_pid(&self, unit: &str, hosted_id: &str) -> Option<u32> {
        self.recorded_leader_pid(unit, hosted_id)
            .and_then(fl_wire::identity::resolve_vendor_cli_pid)
    }

    fn recorded_leader_pid(&self, unit: &str, _hosted_id: &str) -> Option<u32> {
        let out = tmux_stdout(&["list-panes", "-t", unit, "-F", "#{pane_pid}"])?;
        out.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .and_then(|l| l.parse::<u32>().ok())
            .filter(|&pid| pid > 0)
    }

    fn recorded_exit_code(&self, unit: &str, expected_pid: Option<u32>) -> Option<i32> {
        let exact = fl_wire::dispatcher::tmux_exact_target(unit);
        let out = tmux_stdout(&[
            "list-panes",
            "-t",
            &exact,
            "-F",
            "#{pane_dead}\t#{pane_dead_status}\t#{pane_pid}",
        ])?;
        for line in out.lines() {
            let mut parts = line.split('\t');
            let dead = parts.next().map(str::trim).unwrap_or("");
            let status = parts.next().map(str::trim).unwrap_or("");
            let pid_raw = parts.next().map(str::trim).unwrap_or("");
            if dead == "1" {
                if let Some(epid) = expected_pid.filter(|&p| p > 0)
                    && let Ok(pane_pid) = pid_raw.parse::<u32>()
                    && pane_pid > 0
                    && pane_pid != epid
                {
                    return None;
                }
                return status.parse::<i32>().ok();
            }
        }
        None
    }

    fn settle_delay(&self) -> std::time::Duration {
        std::time::Duration::from_millis(100)
    }
}

/// A runtime that only records: `live` holds the session names it runs, and `windows` the
/// `session:window` pairs of the shared Muse session (`muse` is in `live` while it holds one).
/// A spawn fails when the name already runs (a Muse window: when that window is there) or its
/// window is in `fail_windows`.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FakeRuntime {
    pub(crate) live: std::cell::RefCell<std::collections::BTreeSet<String>>,
    /// The windows of the shared Muse session, as `muse:<window>`.
    pub(crate) windows: std::cell::RefCell<std::collections::BTreeSet<String>>,
    /// Windows of other sessions, as `Session:window`. Empty means a kill of
    /// `Session:vendor` treats that window as the session's only window.
    pub(crate) session_windows: std::cell::RefCell<std::collections::BTreeSet<String>>,
    /// Scripted [`SessionRuntime::presence`] answers. A missing unit is [`RuntimePresence::Alive`].
    pub(crate) presence_of: std::cell::RefCell<BTreeMap<String, RuntimePresence>>,
    /// Scripted [`SessionRuntime::recorded_exit_code`] answers.
    pub(crate) exit_codes: std::cell::RefCell<BTreeMap<String, i32>>,
    /// The [`HOSTED_MARK`] of each window of the shared session that has one, by `muse:<window>`.
    /// A window opened by anyone but a spawn has none. Whatever removes a window removes its
    /// mark ([`Self::close_window`]).
    pub(crate) marks: std::cell::RefCell<std::collections::BTreeMap<String, String>>,
    pub(crate) fail_windows: std::cell::RefCell<Vec<String>>,
    pub(crate) calls: std::cell::RefCell<Vec<String>>,
    /// The launch command of each `spawn` call that was not refused, by `session:window`.
    pub(crate) launches: std::cell::RefCell<Vec<(String, Option<LaunchSpec>)>>,
    pub(crate) spawn_cwds: std::cell::RefCell<Vec<Option<String>>>,
    /// Windows whose runtime is gone right after its spawn (a launched command that exits).
    pub(crate) vanish_windows: std::cell::RefCell<Vec<String>>,
    /// Windows whose runtime is gone right after its next single spawn (then cleared).
    pub(crate) vanish_once_windows: std::cell::RefCell<Vec<String>>,
    /// Per-unit `owns` check count after which a spawned unit exits (e.g., `1` means alive on
    /// `t=0` check #1 and gone on check #2). Consumed on trigger so a fresh fallback spawn stays up.
    pub(crate) vanish_after_owns_checks: std::cell::RefCell<BTreeMap<String, u32>>,
    pub(crate) unit_owns_since_spawn: std::cell::RefCell<BTreeMap<String, u32>>,
    /// Scripted PID returned by [`SessionRuntime::recorded_pid`] for a unit.
    pub(crate) unit_pids: std::cell::RefCell<BTreeMap<String, u32>>,
    /// How many times `exists` was asked.
    pub(crate) exists_calls: std::cell::Cell<u32>,
    /// How many times `owns` was asked (the survival check of a launched command).
    pub(crate) owns_calls: std::cell::Cell<u32>,
    /// Pane text [`SessionRuntime::capture`] returns, by runtime unit or tmux target.
    pub(crate) panes: std::cell::RefCell<std::collections::BTreeMap<String, String>>,
    /// Cross-thread event log for ordering assertions in tests.
    pub(crate) shared_events: Option<std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>>,
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

    pub(crate) fn vanish_unit_after_owns_checks(&self, unit: &str, checks: u32) {
        self.vanish_after_owns_checks
            .borrow_mut()
            .insert(unit.to_string(), checks);
    }

    pub(crate) fn set_unit_pid(&self, unit: &str, pid: u32) {
        self.unit_pids.borrow_mut().insert(unit.to_string(), pid);
    }

    pub(crate) fn env<'a>(&'a self, home: &'a Path) -> LifecycleEnv<'a> {
        LifecycleEnv {
            home_override: Some(home),
            spawn_runtime: true,
            runtime: self,
            key_probe: &ProvisionedKeys,
            workspace_root: None,
            log_hydration_warnings: true,
        }
    }

    /// A fake whose shared Muse session already holds a window for each of `personas`.
    pub(crate) fn with_muse_windows(personas: &[&str]) -> Self {
        let fake = Self::default();
        for persona in personas {
            fake.windows
                .borrow_mut()
                .insert(format!("{MUSE_SESSION}:{persona}"));
        }
        fake.sync_muse_session();
        fake
    }

    pub(crate) fn is_live(&self, unit: &str) -> bool {
        if unit.contains(':') {
            self.windows.borrow().contains(unit)
        } else {
            self.live.borrow().contains(unit)
        }
    }

    /// Everything stops, as when the tmux server is gone.
    pub(crate) fn clear(&self) {
        self.live.borrow_mut().clear();
        self.windows.borrow_mut().clear();
        self.marks.borrow_mut().clear();
    }

    /// A window of the shared session ends (its command exits, a person closes it), with its mark.
    pub(crate) fn close_window(&self, unit: &str) {
        self.windows.borrow_mut().remove(unit);
        self.marks.borrow_mut().remove(unit);
        self.sync_muse_session();
    }

    /// Register `windows` (`session`, `window`) in one non-Muse session. The session stays
    /// live while any of them remains.
    pub(crate) fn add_session_windows(&self, windows: &[(&str, &str)]) {
        for (session, window) in windows {
            self.live.borrow_mut().insert((*session).to_string());
            self.session_windows
                .borrow_mut()
                .insert(format!("{session}:{window}"));
        }
    }

    pub(crate) fn has_session_window(&self, unit: &str) -> bool {
        self.session_windows.borrow().contains(unit)
    }

    /// Stop one non-Muse window. No registered windows means that window is the
    /// session's only one, so the session stops with it.
    fn kill_recorded_window(&self, session: &str, window: &str) -> bool {
        let key = format!("{session}:{window}");
        let mut windows = self.session_windows.borrow_mut();
        let in_session: Vec<String> = windows
            .iter()
            .filter(|unit| {
                unit.split_once(':')
                    .is_some_and(|(name, _)| name == session)
            })
            .cloned()
            .collect();
        if in_session.is_empty() {
            drop(windows);
            self.live.borrow_mut().remove(session);
            return true;
        }
        if !in_session.iter().any(|unit| unit == &key) {
            return true;
        }
        windows.remove(&key);
        let only = in_session.len() == 1;
        drop(windows);
        if only {
            self.live.borrow_mut().remove(session);
        }
        true
    }

    /// Someone else (`muse-sessions`, a person) opens a window of the shared session: no mark.
    pub(crate) fn open_foreign_window(&self, unit: &str) {
        self.windows.borrow_mut().insert(unit.to_string());
        self.marks.borrow_mut().remove(unit);
        self.sync_muse_session();
    }

    /// The shared session runs exactly while it holds a window (tmux ends a session with its
    /// last window).
    fn sync_muse_session(&self) {
        let any = !self.windows.borrow().is_empty();
        let mut live = self.live.borrow_mut();
        if any {
            live.insert(MUSE_SESSION.to_string());
        } else {
            live.remove(MUSE_SESSION);
        }
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
        cwd: Option<&str>,
        launch: Option<&LaunchSpec>,
        hosted_id: &str,
    ) -> bool {
        self.spawn_cwds.borrow_mut().push(cwd.map(str::to_string));
        self.calls
            .borrow_mut()
            .push(format!("spawn {session}:{window}"));
        if self.fail_windows.borrow().iter().any(|w| w == window) {
            return false;
        }
        let unit = format!("{session}:{window}");
        let started = if session == MUSE_SESSION {
            // The shared session: a window of that name there is not ours to touch.
            let started = self.windows.borrow_mut().insert(unit.clone());
            if started {
                self.marks
                    .borrow_mut()
                    .insert(unit.clone(), hosted_id.to_string());
            }
            self.sync_muse_session();
            started
        } else {
            self.live.borrow_mut().insert(session.to_string())
        };
        if started {
            self.unit_owns_since_spawn
                .borrow_mut()
                .insert(unit.clone(), 0);
            self.unit_owns_since_spawn
                .borrow_mut()
                .insert(session.to_string(), 0);
            self.launches
                .borrow_mut()
                .push((unit.clone(), launch.cloned()));
            let vanish_once = {
                let mut once = self.vanish_once_windows.borrow_mut();
                if let Some(pos) = once.iter().position(|w| w == window) {
                    once.remove(pos);
                    true
                } else {
                    false
                }
            };
            if vanish_once || self.vanish_windows.borrow().iter().any(|w| w == window) {
                if session == MUSE_SESSION {
                    self.close_window(&unit);
                } else {
                    self.live.borrow_mut().remove(session);
                }
            } else {
                self.presence_of.borrow_mut().remove(&unit);
                self.presence_of.borrow_mut().remove(session);
                self.exit_codes.borrow_mut().remove(&unit);
                self.exit_codes.borrow_mut().remove(session);
            }
        }
        started
    }

    fn kill(&self, unit: &str, hosted_id: &str) -> bool {
        if let Some(events) = &self.shared_events {
            events.lock().unwrap().push("runtime_kill");
        }
        self.calls.borrow_mut().push(format!("kill {unit}"));
        if !is_killable_unit(unit) {
            return false;
        }
        if unit.contains(':') {
            // As tmux: only a window with our mark goes; any other is "nothing of ours".
            if self.owns(unit, hosted_id) {
                self.close_window(unit);
            }
        } else {
            self.live.borrow_mut().remove(unit);
        }
        true
    }

    fn kill_owner(&self, unit: &str, _hosted_id: &str) -> bool {
        if let Some(events) = &self.shared_events {
            events.lock().unwrap().push("runtime_kill");
        }
        self.calls.borrow_mut().push(format!("kill-owner {unit}"));
        if !is_killable_unit(unit) {
            return false;
        }
        if is_muse_window_target(unit) {
            // One window of the name, mark or not. A second window of the name
            // cannot be represented here; the tmux path refuses that case.
            if self.windows.borrow().contains(unit) {
                self.close_window(unit);
            }
            true
        } else if let Some((session, window)) = unit.split_once(':') {
            self.kill_recorded_window(session, window)
        } else {
            self.live.borrow_mut().remove(unit);
            true
        }
    }

    fn capture(&self, unit: &str) -> String {
        self.panes.borrow().get(unit).cloned().unwrap_or_default()
    }

    fn send_keys(&self, unit: &str, keys: &str) -> bool {
        self.calls
            .borrow_mut()
            .push(format!("send-keys {unit} {keys}"));
        true
    }

    fn exists(&self, unit: &str) -> bool {
        self.exists_calls.set(self.exists_calls.get() + 1);
        self.is_live(unit)
    }

    fn presence(&self, unit: &str) -> RuntimePresence {
        self.presence_of
            .borrow()
            .get(unit)
            .copied()
            .unwrap_or(RuntimePresence::Alive)
    }

    fn owns(&self, unit: &str, hosted_id: &str) -> bool {
        self.owns_calls.set(self.owns_calls.get() + 1);
        let check_num = {
            let mut counts = self.unit_owns_since_spawn.borrow_mut();
            let entry = counts.entry(unit.to_string()).or_insert(0);
            *entry = entry.saturating_add(1);
            *entry
        };
        let triggered_key = {
            let vanish_map = self.vanish_after_owns_checks.borrow();
            let direct = vanish_map
                .get(unit)
                .is_some_and(|&limit| check_num > limit)
                .then(|| unit.to_string());
            direct.or_else(|| {
                unit.split_once(':').and_then(|(sess, _)| {
                    vanish_map
                        .get(sess)
                        .is_some_and(|&limit| check_num > limit)
                        .then(|| sess.to_string())
                })
            })
        };
        if let Some(key) = triggered_key {
            self.vanish_after_owns_checks.borrow_mut().remove(&key);
            if unit.contains(':') {
                self.close_window(unit);
            } else {
                self.live.borrow_mut().remove(unit);
            }
            return false;
        }
        if !unit.contains(':') {
            return self.is_live(unit);
        }
        !hosted_id.is_empty()
            && self.is_live(unit)
            && self.marks.borrow().get(unit).map(String::as_str) == Some(hosted_id)
    }

    fn recorded_pid(&self, unit: &str, _hosted_id: &str) -> Option<u32> {
        let pids = self.unit_pids.borrow();
        pids.get(unit)
            .copied()
            .or_else(|| unit.split_once(':').and_then(|(s, _)| pids.get(s).copied()))
            .and_then(fl_wire::identity::resolve_vendor_cli_pid)
    }

    fn recorded_leader_pid(&self, unit: &str, _hosted_id: &str) -> Option<u32> {
        let pids = self.unit_pids.borrow();
        pids.get(unit)
            .copied()
            .or_else(|| unit.split_once(':').and_then(|(s, _)| pids.get(s).copied()))
    }

    fn recorded_exit_code(&self, unit: &str, expected_pid: Option<u32>) -> Option<i32> {
        if let Some(epid) = expected_pid.filter(|&p| p > 0)
            && let Some(actual_pid) = self.recorded_leader_pid(unit, "").filter(|&p| p > 0)
            && actual_pid != epid
        {
            return None;
        }
        self.exit_codes.borrow().get(unit).copied()
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
    /// The vendor-native thinking level (`--effort` for Claude, `model_reasoning_effort` for
    /// Codex, the model id suffix for Antigravity). Applied at launch; ignored by other vendors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub hosted: Option<bool>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The context window to give the session, in tokens. Validated against the vendor and model
    /// ([`crate::context_window::validate_context_window`]) and recorded; Grok only. It is not
    /// applied at launch yet.
    #[serde(
        default,
        deserialize_with = "deserialize_context_window",
        skip_serializing_if = "Option::is_none"
    )]
    pub context_window: Option<u64>,
    /// Operator-facing permission posture (`ask`, `accept-edits`, `auto`, `plan`, `bypass`).
    /// Defaults to `auto`.
    #[serde(default, deserialize_with = "deserialize_posture")]
    pub posture: fl_core::permissions::Posture,
}

/// `posture` from JSON: absent or `null` is `auto`; any other value must name one of the five
/// canonical postures.
fn deserialize_posture<'de, D>(deserializer: D) -> Result<fl_core::permissions::Posture, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<serde_json::Value>>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(fl_core::permissions::Posture::Auto),
        Some(serde_json::Value::String(s)) => s.parse().map_err(serde::de::Error::custom),
        Some(_) => Err(serde::de::Error::custom(
            "posture must be ask, accept-edits, auto, plan or bypass",
        )),
    }
}

/// `context_window` from JSON: absent or `null` is none; anything but a non-negative integer is
/// refused with a message that says what is wanted.
fn deserialize_context_window<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <Option<serde_json::Value>>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| {
            serde::de::Error::custom("context_window must be a positive integer number of tokens")
        }),
    }
}

impl SpinUpRequest {
    /// The request with the runtime directory it defaults to. An explicit `cwd` is kept. Without
    /// one, an agent of a virtual scope (an area with no path, or a project no area names yet,
    /// which spin-up makes as one) starts in `workspace_root`, the one directory such a scope
    /// has; an agent of a directory-backed project keeps what the request said (no `cwd`).
    /// A virtual scope with a configured project binding or matching directory uses
    /// [`resolve_project_cwd`] instead. A `workspace_root` that is not a directory on this
    /// host is not used. Configuration follows the CLI's XDG/HOME lookup.
    pub fn with_default_cwd<S: AgentRegistry + Catalog>(
        self,
        store: &S,
        workspace_root: &Path,
    ) -> Self {
        self.with_default_cwd_for_home(store, workspace_root, None)
    }

    /// Apply the same defaults using the hub's isolated configuration home.
    pub fn with_default_cwd_for_home<S: AgentRegistry + Catalog>(
        mut self,
        store: &S,
        workspace_root: &Path,
        home_override: Option<&Path>,
    ) -> Self {
        if trimmed_opt(&self.cwd).is_some() {
            return self;
        }
        let Ok(project) = validate_slug(self.project.trim()) else {
            return self;
        };
        let virtual_scope = match store.get_area(&project) {
            Ok(Some(area)) => area.is_virtual(),
            Ok(None) => true,
            Err(_) => false,
        };
        if virtual_scope {
            if let Some(dir) =
                resolve_project_cwd(store, &project, Some(workspace_root), home_override)
            {
                self.cwd = Some(dir);
            } else if workspace_root.is_dir() {
                self.cwd = Some(workspace_root.to_string_lossy().into_owned());
            }
        }
        self
    }
}

/// `SpinUpRequest` after validation and defaulting.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidSpinUp {
    persona: String,
    project: String,
    role: String,
    vendor: String,
    model: Option<String>,
    thinking_level: Option<String>,
    node_id: String,
    hosted: bool,
    cwd: Option<String>,
    context_window: Option<u64>,
    posture: fl_core::permissions::Posture,
    permission_receipts: Vec<fl_wire::vendor_adapter::PermissionReceipt>,
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
        let slug = role.strip_prefix("custom:").unwrap_or(&role);
        validate_slug(slug)
            .map_err(|_| invalid_field("role", "must be an executable role or catalog slug"))?;
        let vendor = self.vendor.trim().to_ascii_lowercase();
        if lookup_vendor(&vendor).is_none() {
            return Err(LifecycleError::Invalid(format!(
                "vendor must be one of: {}",
                spin_up_vendor_ids().join(", ")
            )));
        }
        check_persona_fits_slug(&vendor, &persona)?;
        if vendor == "muse" && persona == MUSE_SESSION {
            // Its window `muse:muse` could not be told from the shared session itself.
            return Err(invalid_field(
                "persona",
                "must not be `muse` on vendor `muse` (its window would share the name of the \
                 shared tmux session)",
            ));
        }
        let model = trimmed_opt(&self.model);
        if let Some(model) = &model {
            check_text("model", model)?;
        }
        let thinking_level = trimmed_opt(&self.thinking_level);
        if let Some(level) = &thinking_level {
            check_text("thinking_level", level)?;
            if !fl_wire::vendor_adapter::is_plain_model_id(level) {
                return Err(invalid_field(
                    "thinking_level",
                    "must be letters, digits, '.', '_' or '-' (at most 64 characters)",
                ));
            }
            // The adapter owns what its CLI can apply; no adapter (a prompt-delivery vendor)
            // means nothing applies the level.
            match fl_wire::vendor_adapter::lookup_vendor_cli_adapter(&vendor) {
                Some(adapter) => adapter
                    .check_thinking_level(model.as_deref(), level)
                    .map_err(|msg| LifecycleError::Invalid(format!("thinking_level: {msg}")))?,
                None => {
                    return Err(LifecycleError::Invalid(format!(
                        "thinking_level: vendor `{vendor}` does not support thinking_level; omit it"
                    )));
                }
            }
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
        let perms = fl_wire::vendor_adapter::launch_permissions(&vendor, self.posture)
            .map_err(|msg| LifecycleError::Invalid(format!("posture: {msg}")))?;
        Ok(ValidSpinUp {
            persona,
            project,
            role,
            vendor,
            model,
            thinking_level,
            node_id,
            hosted: self.hosted.unwrap_or(true),
            cwd,
            context_window: self.context_window,
            posture: self.posture,
            permission_receipts: perms.receipts,
        })
    }
}

/// Acceptance evidence for the current hydration submission, independent of runtime liveness.
/// Confirmation means acceptance of this submission, not completion of hydration.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HydrationStatus {
    Confirmed,
    #[default]
    Unconfirmed,
    Failed,
}

impl std::fmt::Display for HydrationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Confirmed => "confirmed",
            Self::Unconfirmed => "unconfirmed",
            Self::Failed => "failed",
        })
    }
}

/// `POST /api/v1/sessions/spin-up` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpinUpResponse {
    #[serde(default)]
    pub warnings: Vec<String>,
    pub session_id: String,
    pub persona: String,
    pub project: String,
    pub role: String,
    pub vendor: String,
    pub model: Option<String>,
    /// The thinking level the session was started with, when one was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    pub node_id: String,
    pub hosted: bool,
    pub status: String,
    pub tmux_target: Option<String>,
    /// The `fl-host` session identifier for this runtime (`<Persona>.<vendor>`, or `muse.<persona>`),
    /// when the session has a runtime target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_session_id: Option<String>,
    /// Whether the prompt was handed to the vendor; this is not acceptance evidence.
    pub hydration_dispatched: bool,
    #[serde(default)]
    pub hydration_status: HydrationStatus,
    /// Strictly monotonic launch generation token for fencing initial hydration acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydration_generation: Option<String>,
    pub prompt: Option<String>,
    /// The context window recorded for the session, in tokens, when one was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Whether this spin-up started the session's `tmux` runtime. `false` when the session is
    /// not hosted, runtime calls are off, or the start failed (tmux failed, or a session of the
    /// name already runs).
    #[serde(default)]
    pub runtime_spawned: bool,
    /// Permission posture compiled into the session's launch flags.
    #[serde(default)]
    pub posture: fl_core::permissions::Posture,
    /// Compiler receipts describing how `posture` mapped onto the vendor's native flags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_receipts: Vec<fl_wire::vendor_adapter::PermissionReceipt>,
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
    /// The thinking level chosen for the session; a restart launches with it. Absent when none
    /// was chosen (records written before the field existed deserialize to none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    pub node_id: String,
    pub hosted: bool,
    /// `running`, `decommissioning`, `decommissioned`, or `stopped`.
    /// `stopped` means the tmux window was confirmed gone; the agent stays.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub restart_policy: String,
    pub restart_count: u32,
    pub tmux_target: Option<String>,
    /// The `fl-host` session identifier for this runtime (`<Persona>.<vendor>`, or `muse.<persona>`),
    /// when the session has a runtime target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_session_id: Option<String>,
    pub cwd: Option<String>,
    /// The context window chosen for the session, in tokens. Validated and recorded; whether and
    /// how a launch applies it is separate. Absent when none was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// The permission posture chosen for the session; a restart launches with it. Records written
    /// before the field existed deserialize to `auto`.
    #[serde(default)]
    pub posture: fl_core::permissions::Posture,
    /// Whether the prompt was handed to the vendor; this is not acceptance evidence.
    pub hydration_dispatched: bool,
    #[serde(default)]
    pub hydration_status: HydrationStatus,
    /// Strictly monotonic launch generation token for fencing initial hydration acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydration_generation: Option<String>,
    /// Whether this lifecycle created the tmux session. Teardown of a runtime it created
    /// uses [`SessionRuntime::kill`]. An owner teardown of one it did not create still
    /// stops the persona's tmux unit ([`SessionRuntime::kill_owner`]).
    #[serde(default)]
    pub runtime_spawned: bool,
    /// Native vendor session ids recorded when the session is decommissioned.
    /// Discovery skips a route that still names one of them. A hosted
    /// `vendor:persona` id and other persona-derived ids are not recorded.
    /// Empty on a running record, including one that replaced a decommissioned record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tombstone_ids: Vec<String>,
    /// Vendor-native session identifier (e.g., Claude `--session-id <uuid>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_session_id: Option<String>,
    /// Root OS PID of the spawned vendor CLI process when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Runtime leader PID used to match exit evidence to the launched process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_leader_pid: Option<u32>,
    /// `/proc/<pid>/stat` field-22 start time (clock ticks) for `pid` when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start_time: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub decommission_prepared_at_ms: Option<u64>,
}

/// Compute the `fl-host` session identifier for an optional `tmux_target` (`<Persona>.<vendor>`,
/// or `muse.<persona>`).
fn host_session_id_for_target(tmux_target: Option<&str>) -> Option<String> {
    tmux_target
        .and_then(fl_wire::vendor_adapter::unit_to_host_session_id)
        .map(|id| id.as_str().to_string())
}

/// Slug of the runtime entry for `session_id` (`:` is not a slug character).
pub fn hosted_session_slug(session_id: &str) -> String {
    format!("{HOSTED_SLUG_PREFIX}{}", session_id.replace(':', "-"))
}

/// Prefix of disjoint runtime entries that record generation-fenced hydration acceptance.
pub const HYDRATION_ACK_SLUG_PREFIX: &str = "hydration-ack-";

/// Slug of the disjoint runtime receipt for a launch `generation`.
pub fn hydration_ack_slug(generation: &str) -> String {
    format!("{HYDRATION_ACK_SLUG_PREFIX}{generation}")
}

/// Disjoint receipt written by `fl wire register ... --replace` when a hosted session
/// acknowledges its initial hydration prompt for `generation`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HydrationReceipt {
    #[serde(default)]
    pub session_id: String,
    pub persona: String,
    pub generation: String,
    pub accepted_at_ms: u64,
}

/// Prefix of disjoint runtime entries that record per-session Claude resume survival/PID state.
pub const CLAUDE_RESUME_STATE_SLUG_PREFIX: &str = "claude-resume-state-";

/// Slug of the disjoint runtime entry for a hosted Claude session's resume state.
pub fn claude_resume_state_slug(session_id: &str) -> String {
    format!(
        "{CLAUDE_RESUME_STATE_SLUG_PREFIX}{}",
        session_id.replace(':', "-")
    )
}

/// Disjoint runtime state for a hosted Claude session's `--resume` survival tracking.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeResumeState {
    /// Set when a `--resume` launch exited before positive confirmation so the next restart
    /// goes straight to a fresh launch with `resume_launch_did_not_survive`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume_failed: bool,
    /// Session ID of a `--resume` launch that survived the initial launch window without yet
    /// registering its `SessionStart:resume` route; if the unit exits before route confirmation,
    /// `reconcile_dead_hosted_sessions` / restart marks `resume_failed = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_unconfirmed_session_id: Option<String>,
    /// Timestamp (ms since epoch) when the most recent `--resume` launch was spawned, used to
    /// reject stale pre-restart routes in [`claude_route_confirmed_for_session`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_launched_at_ms: Option<u64>,
    /// Set while the `--resume` survival watch is actively polling so `reconcile_dead_hosted_sessions`
    /// never strikes a unit inside its survival window.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_survival_window: bool,
}

pub fn load_claude_resume_state<S: StateStore>(store: &S, session_id: &str) -> ClaudeResumeState {
    let slug = claude_resume_state_slug(session_id);
    store
        .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)
        .ok()
        .flatten()
        .and_then(|entry| match entry.payload {
            StatePayload::Runtime(rt) => serde_json::from_value::<ClaudeResumeState>(rt.value).ok(),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn save_claude_resume_state<S: StateStore>(
    store: &S,
    session_id: &str,
    state: &ClaudeResumeState,
    by: &str,
) -> Result<(), LifecycleError> {
    let slug = claude_resume_state_slug(session_id);
    if !state.resume_failed
        && state.resume_unconfirmed_session_id.is_none()
        && state.resume_launched_at_ms.is_none()
        && !state.in_survival_window
    {
        let _ = store.delete_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug);
        return Ok(());
    }
    let value = serde_json::to_value(state).map_err(|e| LifecycleError::Store(e.to_string()))?;
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug,
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

pub fn delete_claude_resume_state<S: StateStore>(store: &S, session_id: &str) {
    let _ = store.delete_state_by_key(
        &StateScope::Workspace,
        StateConcern::Runtime,
        &claude_resume_state_slug(session_id),
    );
}

/// Prefix of disjoint runtime entries that record per-session automatic respawn & crash-loop state.
pub const HOSTED_RESPAWN_STATE_SLUG_PREFIX: &str = "hosted-respawn-state-";

/// Slug of the disjoint runtime entry for a hosted session's automatic respawn state.
pub fn hosted_respawn_state_slug(session_id: &str) -> String {
    format!(
        "{HOSTED_RESPAWN_STATE_SLUG_PREFIX}{}",
        session_id.replace(':', "-")
    )
}

/// Disjoint runtime state for automatic respawn backoff and crash-loop protection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedRespawnState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub respawn_timestamps_ms: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_allowed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub crash_loop_notified: bool,
}

pub fn load_hosted_respawn_state<S: StateStore>(store: &S, session_id: &str) -> HostedRespawnState {
    let slug = hosted_respawn_state_slug(session_id);
    store
        .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)
        .ok()
        .flatten()
        .and_then(|entry| match entry.payload {
            StatePayload::Runtime(rt) => {
                serde_json::from_value::<HostedRespawnState>(rt.value).ok()
            }
            _ => None,
        })
        .unwrap_or_default()
}

pub fn save_hosted_respawn_state<S: StateStore>(
    store: &S,
    session_id: &str,
    state: &HostedRespawnState,
    by: &str,
) -> Result<(), LifecycleError> {
    let slug = hosted_respawn_state_slug(session_id);
    if state.respawn_timestamps_ms.is_empty()
        && state.next_allowed_at_ms.is_none()
        && !state.crash_loop_notified
    {
        let _ = store.delete_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug);
        return Ok(());
    }
    let value = serde_json::to_value(state).map_err(|e| LifecycleError::Store(e.to_string()))?;
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug,
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

pub fn delete_hosted_respawn_state<S: StateStore>(store: &S, session_id: &str) {
    let _ = store.delete_state_by_key(
        &StateScope::Workspace,
        StateConcern::Runtime,
        &hosted_respawn_state_slug(session_id),
    );
}

#[cfg(test)]
thread_local! {
    static TEST_RESPAWN_NOW_MS: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
    static TEST_DEAD_STREAK_MIN_SPACING_MS: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

fn current_respawn_clock_ms() -> u64 {
    #[cfg(test)]
    if let Some(ms) = TEST_RESPAWN_NOW_MS.with(|c| c.get()) {
        return ms;
    }
    now_ms()
}

fn dead_streak_min_spacing_ms() -> u64 {
    #[cfg(test)]
    {
        TEST_DEAD_STREAK_MIN_SPACING_MS
            .with(|c| c.get())
            .unwrap_or(0)
    }
    #[cfg(not(test))]
    {
        DEAD_STREAK_MIN_SPACING_MS
    }
}

static LAST_HYDRATION_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Mint a strictly monotonic, fixed-width lowercase hex UUIDv7-shaped generation token
/// (`xxxxxxxx-xxxx-7xxx-8xxx-xxxxxxxxxxxx`) so two tokens minted in the same millisecond satisfy
/// `g1 < g2` iff `g1` was minted before `g2`.
pub fn next_hydration_generation() -> String {
    use std::sync::atomic::Ordering;
    let now = now_ms() & 0xffff_ffff_ffff;
    let candidate = now << 16;
    let mut prev = LAST_HYDRATION_GENERATION.load(Ordering::Relaxed);
    let packed = loop {
        let next = if candidate > prev {
            candidate
        } else {
            prev.wrapping_add(1)
        };
        match LAST_HYDRATION_GENERATION.compare_exchange_weak(
            prev,
            next,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ) {
            Ok(_) => break next,
            Err(actual) => prev = actual,
        }
    };
    let ms_hi = (packed >> 32) as u32;
    let ms_lo = ((packed >> 16) & 0xffff) as u16;
    let seq_hi = 0x7000u16 | (((packed >> 4) & 0x0fff) as u16);
    let seq_lo = 0x8000u16 | (((packed & 0x0f) as u16) << 8);
    let rand_tail = (uuid::Uuid::now_v7().as_u128() as u64) & 0xffff_ffff_ffff;
    format!("{ms_hi:08x}-{ms_lo:04x}-{seq_hi:04x}-{seq_lo:04x}-{rand_tail:012x}")
}

/// `true` when `s` is a valid 36-character lowercase hex UUIDv7 generation token.
pub fn is_valid_hydration_generation(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &ch) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if ch != b'-' {
                    return false;
                }
            }
            14 => {
                if ch != b'7' {
                    return false;
                }
            }
            19 => {
                if !matches!(ch, b'8' | b'9' | b'a' | b'b') {
                    return false;
                }
            }
            _ => {
                if !matches!(ch, b'0'..=b'9' | b'a'..=b'f') {
                    return false;
                }
            }
        }
    }
    true
}

fn receipt_matches_record(receipt: &HydrationReceipt, record: &HostedSessionRecord) -> bool {
    record.hydration_generation.as_deref() == Some(receipt.generation.as_str())
        && receipt.persona.eq_ignore_ascii_case(&record.persona)
        && (receipt.session_id.is_empty()
            || receipt.session_id.eq_ignore_ascii_case(&record.session_id))
}

fn promote_confirmed_hydration<S: StateStore>(
    store: &S,
    record: &mut HostedSessionRecord,
) -> Result<(), LifecycleError> {
    if record.hydration_status == HydrationStatus::Unconfirmed
        && let Some(generation) = record.hydration_generation.as_deref()
        && is_valid_hydration_generation(generation)
        && let Some(entry) = store.get_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            &hydration_ack_slug(generation),
        )?
        && let StatePayload::Runtime(rt) = entry.payload
        && let Ok(receipt) = serde_json::from_value::<HydrationReceipt>(rt.value)
        && receipt_matches_record(&receipt, record)
    {
        record.hydration_status = HydrationStatus::Confirmed;
    }
    Ok(())
}

fn prune_stale_hydration_receipts<S: StateStore>(
    store: &S,
    persona: &str,
) -> Result<(), LifecycleError> {
    let entries = store.list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))?;
    let mut active_generations: BTreeSet<String> = BTreeSet::new();
    let mut receipt_entries: Vec<(String, HydrationReceipt)> = Vec::new();
    for entry in entries {
        let StatePayload::Runtime(rt) = entry.payload else {
            continue;
        };
        if entry.slug.starts_with(HOSTED_SLUG_PREFIX) {
            if let Ok(rec) = serde_json::from_value::<HostedSessionRecord>(rt.value)
                && rec.persona.eq_ignore_ascii_case(persona)
                && !matches!(
                    rec.status.as_str(),
                    STATUS_DECOMMISSIONING | STATUS_DECOMMISSIONED
                )
                && let Some(generation) = rec.hydration_generation
            {
                active_generations.insert(generation);
            }
        } else if entry.slug.starts_with(HYDRATION_ACK_SLUG_PREFIX)
            && let Ok(receipt) = serde_json::from_value::<HydrationReceipt>(rt.value)
            && receipt.persona.eq_ignore_ascii_case(persona)
        {
            receipt_entries.push((entry.slug, receipt));
        }
    }
    for (slug, receipt) in receipt_entries {
        if !active_generations.contains(&receipt.generation) {
            store.delete_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)?;
        }
    }
    Ok(())
}

/// Refuse `fl wire register` when `caller_generation` is present and superseded by a newer
/// launch generation or decommission for `persona`.
pub fn fence_hydration_register<S: StateStore>(
    store: &S,
    persona: &str,
    caller_generation: Option<&str>,
) -> Result<(), String> {
    let Some(g_caller) = caller_generation.map(str::trim).filter(|g| !g.is_empty()) else {
        return Ok(());
    };
    if !is_valid_hydration_generation(g_caller) {
        return Ok(());
    }
    let persona_lower = persona.trim().to_ascii_lowercase();
    let records = list_hosted_sessions(store).map_err(|e| e.to_string())?;
    let matching: Vec<&HostedSessionRecord> = records
        .iter()
        .filter(|r| r.persona.eq_ignore_ascii_case(&persona_lower))
        .collect();
    if matching.is_empty() {
        return Ok(());
    }
    let superseded_by_newer = matching.iter().any(|r| {
        r.hydration_generation
            .as_deref()
            .is_some_and(|g_rec| g_caller < g_rec)
    });
    let has_matching_live = matching.iter().any(|r| {
        !matches!(
            r.status.as_str(),
            STATUS_DECOMMISSIONING | STATUS_DECOMMISSIONED
        ) && r.hydration_generation.as_deref() == Some(g_caller)
    });
    let all_matching_decommissioned = !has_matching_live
        && matching.iter().any(|r| {
            matches!(
                r.status.as_str(),
                STATUS_DECOMMISSIONING | STATUS_DECOMMISSIONED
            )
        })
        && !matching.iter().any(|r| {
            !matches!(
                r.status.as_str(),
                STATUS_DECOMMISSIONING | STATUS_DECOMMISSIONED
            )
        });
    if !has_matching_live && (superseded_by_newer || all_matching_decommissioned) {
        return Err(format!(
            "registration refused for `{persona_lower}`: session launch generation `{g_caller}` was superseded by a newer launch; this process should exit"
        ));
    }
    Ok(())
}

/// Record a disjoint `hydration-ack-<G>` receipt when `replace == true`, `caller_generation` is a
/// valid generation token, and `hosted_id` (if present) matches `persona`. Preserves an existing
/// receipt for `(persona, G)` without overwriting `accepted_at_ms`.
pub fn record_hydration_ack<S: StateStore>(
    store: &S,
    persona: &str,
    replace: bool,
    caller_generation: Option<&str>,
    hosted_id: Option<&str>,
) -> Result<bool, LifecycleError> {
    record_hydration_ack_with_pid(store, persona, replace, caller_generation, hosted_id, None)
}

/// Record a disjoint `hydration-ack-<G>` receipt with an explicit optional `caller_pid` (e.g. from
/// a captured `VendorRoute::Claude` socket) without reading ambient environment variables.
pub fn record_hydration_ack_with_pid<S: StateStore>(
    store: &S,
    persona: &str,
    replace: bool,
    caller_generation: Option<&str>,
    hosted_id: Option<&str>,
    caller_pid: Option<u32>,
) -> Result<bool, LifecycleError> {
    record_hydration_ack_with_runtime(
        store,
        persona,
        replace,
        caller_generation,
        hosted_id,
        caller_pid,
        None,
    )
}

/// Like `record_hydration_ack_with_pid`, optionally recovering a missing reference from the runtime.
pub fn record_hydration_ack_with_runtime<S: StateStore>(
    store: &S,
    persona: &str,
    replace: bool,
    caller_generation: Option<&str>,
    hosted_id: Option<&str>,
    caller_pid: Option<u32>,
    runtime: Option<&dyn SessionRuntime>,
) -> Result<bool, LifecycleError> {
    if !replace {
        return Ok(false);
    }
    let Some(g_caller) = caller_generation.map(str::trim).filter(|g| !g.is_empty()) else {
        return Ok(false);
    };
    if !is_valid_hydration_generation(g_caller) {
        return Ok(false);
    }
    let persona_lower = persona.trim().to_ascii_lowercase();
    if let Some(hid) = hosted_id.map(str::trim).filter(|s| !s.is_empty()) {
        let Ok((_vendor, hid_persona)) = fl_wire::identity::parse_hosted_identity(hid) else {
            return Ok(false);
        };
        if !hid_persona.eq_ignore_ascii_case(&persona_lower) {
            return Ok(false);
        }
    }
    let slug = hydration_ack_slug(g_caller);
    let mut receipt_already_existed = false;
    if let Some(existing) =
        store.get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)?
        && let StatePayload::Runtime(rt) = existing.payload
        && let Ok(receipt) = serde_json::from_value::<HydrationReceipt>(rt.value)
    {
        if !receipt.persona.eq_ignore_ascii_case(&persona_lower) {
            eprintln!(
                "fl-hub: warning: hydration receipt `{slug}` already belongs to persona `{}`; ignoring ack for `{persona_lower}`",
                receipt.persona
            );
            return Ok(false);
        }
        receipt_already_existed = true;
    }
    let session_id = if let Some(hid) = hosted_id.map(str::trim).filter(|s| !s.is_empty()) {
        fl_wire::identity::parse_hosted_identity(hid)
            .map(|(v, p)| format!("{v}:{p}"))
            .unwrap_or_else(|_| hid.to_ascii_lowercase())
    } else {
        list_hosted_sessions(store)?
            .into_iter()
            .find(|r| {
                r.persona.eq_ignore_ascii_case(&persona_lower)
                    && r.hydration_generation.as_deref() == Some(g_caller)
            })
            .map(|r| r.session_id)
            .unwrap_or_default()
    };
    if !session_id.trim().is_empty()
        && let Some(mut rec) = find_hosted_session(store, &[&session_id])?
    {
        let mut dirty = false;
        if rec.pid.is_none()
            && let Some(runtime) = runtime
        {
            let target = rec
                .tmux_target
                .clone()
                .unwrap_or_else(|| hosted_tmux_target(&rec.vendor, &rec.persona));
            let unit = runtime.unit(&target);
            let leader_pid = runtime
                .owns(unit, &rec.session_id)
                .then(|| runtime.recorded_leader_pid(unit, &rec.session_id))
                .flatten();
            if let Some(rpid) = leader_pid.and_then(fl_wire::identity::resolve_vendor_cli_pid) {
                rec.pid = Some(rpid);
                rec.pid_start_time = fl_wire::identity::proc_start_time_ticks(rpid);
                rec.runtime_leader_pid = leader_pid;
                dirty = true;
            }
        }
        if let Some(cpid) = caller_pid.filter(|&p| p > 0)
            && cfg!(target_os = "linux")
            && fl_wire::vendor_adapter::tool_processes_descend_from_cli(&rec.vendor)
        {
            let vendor_pid =
                fl_wire::identity::find_nearest_vendor_cli_ancestor_pid(cpid).unwrap_or(cpid);
            if let Some(rec_pid) = rec.pid.filter(|&p| p > 0) {
                let direct_or_ancestor_match = fl_wire::identity::pid_is_nearest_vendor_ancestor(
                    cpid,
                    rec_pid,
                    rec.pid_start_time,
                ) || (rec_pid == cpid
                    && rec.pid_start_time.is_none()
                    && !fl_wire::identity::proc_alive(rec_pid));
                if direct_or_ancestor_match {
                    if rec.pid_start_time.is_none()
                        && let Some(st) = fl_wire::identity::proc_start_time_ticks(rec_pid)
                    {
                        rec.pid_start_time = Some(st);
                        dirty = true;
                    }
                } else if fl_wire::identity::proc_alive_matching_start(rec_pid, rec.pid_start_time)
                    && fl_wire::identity::proc_is_shell_wrapper(rec_pid)
                    && vendor_pid != rec_pid
                    && fl_wire::identity::resolve_vendor_cli_pid(rec_pid) == Some(vendor_pid)
                    && fl_wire::identity::pid_is_nearest_vendor_ancestor(cpid, vendor_pid, None)
                {
                    rec.runtime_leader_pid = Some(rec_pid);
                    rec.pid = Some(vendor_pid);
                    rec.pid_start_time = fl_wire::identity::proc_start_time_ticks(vendor_pid);
                    dirty = true;
                } else {
                    return Ok(false);
                }
            }
            // Without an independently recorded runtime PID, the caller cannot establish it.
        }
        if rec.hydration_generation.as_deref() == Some(g_caller)
            && rec.hydration_status != HydrationStatus::Confirmed
        {
            rec.hydration_status = HydrationStatus::Confirmed;
            dirty = true;
        }
        if dirty {
            persist_hosted_session(store, &rec, &persona_lower)?;
        }
    }
    if receipt_already_existed {
        return Ok(false);
    }
    let receipt = HydrationReceipt {
        session_id,
        persona: persona_lower.clone(),
        generation: g_caller.to_string(),
        accepted_at_ms: now_ms(),
    };
    let value = serde_json::to_value(&receipt).map_err(|e| LifecycleError::Store(e.to_string()))?;
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug,
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Runtime(RuntimeEntry {
            value,
            merge_mode: MergeMode::LastWriter,
        }),
        at: stamp::now(),
        by: persona_lower,
    })?;
    Ok(true)
}

pub fn persist_hosted_session<S: StateStore>(
    store: &S,
    record: &HostedSessionRecord,
    by: &str,
) -> Result<(), LifecycleError> {
    let mut normalized = record.clone();
    normalized.host_session_id = host_session_id_for_target(normalized.tmux_target.as_deref());
    if let Err(e) = promote_confirmed_hydration(store, &mut normalized) {
        eprintln!(
            "fl-hub: warning: failed to promote hydration receipt for `{}`: {e}",
            record.session_id
        );
    }
    let value =
        serde_json::to_value(&normalized).map_err(|e| LifecycleError::Store(e.to_string()))?;
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
    if let Err(e) = prune_stale_hydration_receipts(store, &normalized.persona) {
        eprintln!(
            "fl-hub: warning: failed to prune stale hydration receipts for `{}`: {e}",
            normalized.persona
        );
    }
    Ok(())
}

/// Every hosted-session record, sorted by `session_id`. Entries that do not decode are skipped.
pub fn list_hosted_sessions<S: StateStore>(
    store: &S,
) -> Result<Vec<HostedSessionRecord>, LifecycleError> {
    let entries = store.list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))?;
    let mut records: Vec<HostedSessionRecord> = Vec::new();
    let mut receipts: std::collections::HashMap<String, HydrationReceipt> =
        std::collections::HashMap::new();
    for e in entries {
        let StatePayload::Runtime(rt) = e.payload else {
            continue;
        };
        if e.slug.starts_with(HOSTED_SLUG_PREFIX) {
            if let Ok(mut rec) = serde_json::from_value::<HostedSessionRecord>(rt.value) {
                if rec.host_session_id.is_none() {
                    rec.host_session_id = host_session_id_for_target(rec.tmux_target.as_deref());
                }
                records.push(rec);
            }
        } else if e.slug.starts_with(HYDRATION_ACK_SLUG_PREFIX)
            && let Ok(receipt) = serde_json::from_value::<HydrationReceipt>(rt.value)
        {
            receipts.insert(receipt.generation.clone(), receipt);
        }
    }
    for rec in &mut records {
        if rec.hydration_status == HydrationStatus::Unconfirmed
            && let Some(generation) = rec.hydration_generation.as_deref()
            && let Some(receipt) = receipts.get(generation)
            && receipt_matches_record(receipt, rec)
        {
            rec.hydration_status = HydrationStatus::Confirmed;
        }
    }
    records.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(records)
}

/// Keyed lookup of a hosted-session record by exact `session_id` without scanning the state table.
pub fn get_hosted_session_by_id<S: StateStore>(
    store: &S,
    session_id: &str,
) -> Result<Option<HostedSessionRecord>, LifecycleError> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let slug = hosted_session_slug(trimmed);
    let Some(entry) =
        store.get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)?
    else {
        return Ok(None);
    };
    let StatePayload::Runtime(rt) = entry.payload else {
        return Ok(None);
    };
    let Ok(mut rec) = serde_json::from_value::<HostedSessionRecord>(rt.value) else {
        return Ok(None);
    };
    if rec.session_id != trimmed {
        return Ok(None);
    }
    if rec.host_session_id.is_none() {
        rec.host_session_id = host_session_id_for_target(rec.tmux_target.as_deref());
    }
    let _ = promote_confirmed_hydration(store, &mut rec);
    Ok(Some(rec))
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
    thinking_level: Option<&str>,
    by: &str,
) -> Result<Option<HostedSessionRecord>, LifecycleError> {
    let Some(mut record) = find_hosted_session(store, keys)? else {
        return Ok(None);
    };
    record.model = Some(model.to_string());
    // The API always sends the full model/level pair, so `None` means "no level": a stale one
    // (Claude `--effort high`, then a switch to `sonnet`) must not be relaunched on restart.
    record.thinking_level = thinking_level.map(str::to_string);
    record.updated_at_ms = now_ms();
    persist_hosted_session(store, &record, by)?;
    Ok(Some(record))
}

/// Set (`Some`) or clear (`None`) the context window on the hosted-session record that `keys`
/// name (same lookup as [`find_hosted_session`]). The caller has validated the value. Returns
/// the updated record, or `None` when no record matches.
pub fn set_hosted_session_context_window<S: StateStore>(
    store: &S,
    keys: &[&str],
    context_window: Option<u64>,
    by: &str,
) -> Result<Option<HostedSessionRecord>, LifecycleError> {
    let Some(mut record) = find_hosted_session(store, keys)? else {
        return Ok(None);
    };
    record.context_window = context_window;
    record.updated_at_ms = now_ms();
    persist_hosted_session(store, &record, by)?;
    Ok(Some(record))
}

/// Record `posture` on the hosted-session record that `keys` name (same lookup as
/// [`find_hosted_session`]). Returns the updated record, or `None` when no record matches.
pub fn set_hosted_session_posture<S: StateStore>(
    store: &S,
    keys: &[&str],
    posture: fl_core::permissions::Posture,
    by: &str,
) -> Result<Option<HostedSessionRecord>, LifecycleError> {
    let Some(mut record) = find_hosted_session(store, keys)? else {
        return Ok(None);
    };
    record.posture = posture;
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

/// `AgentRole` for a spin-up role string. Every label in [`SPIN_UP_ROLES`] is a built-in variant;
/// a label outside the list (a stored record from an older build) falls back to a custom slug, or
/// to `assistant` when it is not a slug.
fn agent_role_from_label(label: &str) -> AgentRole {
    AgentRole::from_wire(label).unwrap_or_else(|_| {
        match label.trim().to_ascii_lowercase().as_str() {
            slug if validate_slug(slug).is_ok() => AgentRole::Custom(slug.to_string()),
            _ => AgentRole::Assistant,
        }
    })
}

/// Inverse of [`agent_role_from_label`], in the spin-up spelling.
fn role_label_of(role: &AgentRole) -> String {
    match role {
        AgentRole::LabManager => "lab-manager".to_string(),
        other => reforge_role_label(other),
    }
}

#[cfg(test)]
fn prompt_for(display: &str, project: &str, role: &str) -> String {
    canonical_reforge_prompt(
        display,
        project,
        &reforge_role_label(&agent_role_from_label(role)),
    )
}

/// Resolve the executable/catalog union before registration or runtime effects. Keep the
/// runtime identity separate from catalog slug normalization (especially structural shadows).
fn validate_catalog_role<S: StateStore>(store: &S, label: &str) -> Result<(), LifecycleError> {
    let role = agent_role_from_label(label);
    if !matches!(role, AgentRole::Custom(_)) {
        return Ok(());
    }
    let slug = label.strip_prefix("custom:").unwrap_or(label);
    if get_role(store, slug)
        .map_err(|e| LifecycleError::Store(e.to_string()))?
        .is_some()
    {
        return Ok(());
    }
    Err(invalid_field(
        "role",
        &format!("`{label}` is not an executable role or available catalog role"),
    ))
}

/// A current catalog record or persisted catalog tombstone/corruption. Unknown legacy roles
/// have neither, and keep the legacy external/file hydration path.
pub(crate) fn has_catalog_guidance<S: StateStore>(
    store: &S,
    slug: &str,
) -> Result<bool, LifecycleError> {
    let normalized = normalize_role_slug(slug).map_err(|e| LifecycleError::Store(e.to_string()))?;
    Ok(get_role(store, slug)
        .map_err(|e| LifecycleError::Store(e.to_string()))?
        .is_some()
        || store
            .get_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                &format!("role-{normalized}"),
            )?
            .is_some())
}

/// Owned role guidance captured from the store before filesystem rendering.
pub(crate) struct RegisteredRoleGuidance {
    label: String,
    template: Option<String>,
    structural_custom: bool,
}

impl RegisteredRoleGuidance {
    pub(crate) fn render(
        &self,
        context: crate::hydration::Context,
        display: &str,
        project: &str,
    ) -> crate::hydration::HydrationPrompt {
        self.render_with_generation(context, display, project, None)
    }

    pub(crate) fn render_with_generation(
        &self,
        context: crate::hydration::Context,
        display: &str,
        project: &str,
        generation: Option<&str>,
    ) -> crate::hydration::HydrationPrompt {
        use crate::hydration::{RoleGuidance, render_with_generation};
        let guidance = if self.structural_custom {
            RoleGuidance::StructuralCustom
        } else if let Some(template) = &self.template {
            RoleGuidance::Catalog(template)
        } else {
            RoleGuidance::File
        };
        render_with_generation(context, display, project, &self.label, guidance, generation)
    }
}

/// Catalog authority remains fallible even when optional filesystem context is absent.
pub(crate) fn prepare_registered_role_guidance<S: StateStore>(
    store: &S,
    role: &AgentRole,
) -> Result<RegisteredRoleGuidance, LifecycleError> {
    let mut guidance = RegisteredRoleGuidance {
        label: reforge_role_label(role),
        template: None,
        structural_custom: false,
    };
    match role {
        AgentRole::Custom(slug)
            if matches!(
                slug.as_str(),
                "lead" | "assistant" | "lab_manager" | "lab-manager"
            ) =>
        {
            guidance.structural_custom = true
        }
        AgentRole::Custom(slug) => {
            if let Some(record) =
                get_role(store, slug).map_err(|e| LifecycleError::Store(e.to_string()))?
            {
                guidance.label = role.as_wire();
                guidance.template = Some(record.prompt_template);
            } else if has_catalog_guidance(store, slug)? {
                return Err(LifecycleError::Invalid(format!(
                    "catalog guidance for `{}` is missing or deleted; restore the role before hydration",
                    guidance.label
                )));
            }
        }
        _ => {}
    }
    Ok(guidance)
}

/// Catalog guidance for custom roles. Structural custom shadows may share a catalog slug,
/// but must never inherit the builtin structural template or file.
pub(crate) fn registered_role_prompt<S: StateStore + Catalog>(
    store: &S,
    env: &LifecycleEnv<'_>,
    cwd: Option<&Path>,
    display: &str,
    project: &str,
    role: &AgentRole,
) -> Result<crate::hydration::HydrationPrompt, LifecycleError> {
    registered_role_prompt_with_generation(store, env, cwd, display, project, role, None)
}

pub(crate) fn registered_role_prompt_with_generation<S: StateStore + Catalog>(
    store: &S,
    env: &LifecycleEnv<'_>,
    cwd: Option<&Path>,
    display: &str,
    project: &str,
    role: &AgentRole,
    generation: Option<&str>,
) -> Result<crate::hydration::HydrationPrompt, LifecycleError> {
    let guidance = prepare_registered_role_guidance(store, role)?;
    // workspace_root also supplies legacy runtime cwd defaults. Preserve that hint
    // for launching, but require workspace markers before using it for guidance.
    let workspace = env.workspace_root.and_then(fl_core::find_workspace_root);
    let context = crate::hydration::Context::resolve(
        store,
        env.home_override,
        workspace.as_deref(),
        cwd,
        project,
    );
    Ok(guidance.render_with_generation(context, display, project, generation))
}

/// Each staged binding has its own key, preserving earlier required guidance if a later
/// registry write fails. A binding is effective only for the same stable ID and assignment.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RecastGuidanceBinding {
    agent_id: fl_core::ids::AgentId,
    project: String,
    role: AgentRole,
}

fn guidance_binding(agent: &AgentRecord, project: &str, role: &AgentRole) -> RecastGuidanceBinding {
    RecastGuidanceBinding {
        agent_id: agent.id.clone(),
        project: project.into(),
        role: role.clone(),
    }
}

fn guidance_binding_slug(binding: &RecastGuidanceBinding) -> String {
    let encoded = serde_json::to_string(binding).expect("binding serializes");
    format!(
        "recast-guidance-{}",
        &crate::auth::sha256_hex(&encoded)[..48]
    )
}

pub(crate) fn recast_guidance_required<S: StateStore>(
    store: &S,
    agent: &AgentRecord,
) -> Result<bool, StoreError> {
    let binding = guidance_binding(agent, &agent.project, &agent.role);
    let Some(entry) = store.get_state_by_key(
        &StateScope::Workspace,
        StateConcern::Runtime,
        &guidance_binding_slug(&binding),
    )?
    else {
        return Ok(false);
    };
    let StatePayload::Runtime(rt) = entry.payload else {
        return Err(StoreError::Backend(
            "invalid recast guidance metadata".into(),
        ));
    };
    let stored: RecastGuidanceBinding = serde_json::from_value(rt.value)
        .map_err(|e| StoreError::Backend(format!("invalid recast guidance metadata: {e}")))?;
    if stored != binding {
        return Err(StoreError::Backend(
            "recast guidance binding mismatch".into(),
        ));
    }
    Ok(true)
}

fn current_recast_assignment<S: AgentRegistry + StateStore>(
    store: &S,
    persona: &str,
) -> Result<Option<AgentRecord>, LifecycleError> {
    let Some(agent) = store.agent_by_name(persona)? else {
        return Ok(None);
    };
    // Retirement ends the current assignment. A later spin-up may reuse this stable identity
    // in a new project or role; only an active recast overrides stale hosted/request fields.
    if agent.status != AgentStatus::Active {
        return Ok(None);
    }
    Ok(recast_guidance_required(store, &agent)?.then_some(agent))
}

pub(crate) fn stage_recast_guidance<S: StateStore>(
    store: &S,
    agent: &AgentRecord,
    project: &str,
    role: &AgentRole,
    by: &str,
) -> Result<(), StoreError> {
    let binding = guidance_binding(agent, project, role);
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: guidance_binding_slug(&binding),
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Runtime(RuntimeEntry {
            value: serde_json::to_value(&binding)
                .map_err(|e| StoreError::Backend(e.to_string()))?,
            merge_mode: MergeMode::LastWriter,
        }),
        at: stamp::now(),
        by: by.into(),
    })?;
    Ok(())
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

/// The tmux binary: `FL_WIRE_TMUX_BIN` (the one fl-wire uses) when set, else `tmux`.
fn tmux_bin() -> PathBuf {
    std::env::var_os("FL_WIRE_TMUX_BIN").map_or_else(|| PathBuf::from("tmux"), PathBuf::from)
}

fn scrub_parent_identity_env(cmd: &mut Command) {
    for key in [
        "AGENT_NAME",
        "AGENT_PERSONA",
        "AGENT_HYDRATION_GENERATION",
        "FL_HOSTED_ID",
    ] {
        cmd.env_remove(key);
    }
}

fn tmux_ok(args: &[&str]) -> bool {
    let mut cmd = Command::new(tmux_bin());
    scrub_parent_identity_env(&mut cmd);
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

/// The stdout of a `tmux` call that exits 0 within [`TMUX_TIMEOUT`]; `None` otherwise.
pub(crate) fn tmux_stdout(args: &[&str]) -> Option<String> {
    tmux_stdout_of(&tmux_bin(), args)
}

/// [`tmux_stdout`] on the tmux binary `bin`.
pub(crate) fn tmux_stdout_of(bin: &Path, args: &[&str]) -> Option<String> {
    use std::io::Read;

    let mut cmd = Command::new(bin);
    scrub_parent_identity_env(&mut cmd);
    let mut child = cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drained on its own thread, so a full pipe cannot hold tmux until the timeout.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = tx.send(bytes);
    });
    let deadline = Instant::now() + TMUX_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let bytes = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Best-effort detached `tmux` runtime. `false` when tmux is absent or the session exists. For
/// [`MUSE_SESSION`] the runtime is a window: see [`spawn_muse_window`].
fn spawn_tmux_runtime(
    session: &str,
    window: &str,
    cwd: Option<&str>,
    launch: Option<&LaunchSpec>,
    hosted_id: &str,
) -> bool {
    if session == MUSE_SESSION {
        return spawn_muse_window(window, cwd, launch, hosted_id);
    }
    let args = tmux_new_session_args_with_hosted_id(session, window, cwd, launch, hosted_id);
    tmux_ok(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Open a window named `window` in the shared Muse session: a new window when the session
/// runs, else the session itself. `false` when a window of that name is there already (it
/// belongs to whoever started it, such as `muse-sessions`; it is left alone) or when tmux does
/// not answer about the windows.
///
/// The new window's id is read back (`-P -F '#{window_id}'`) and the window is marked
/// ([`HOSTED_MARK`]) with `hosted_id` at once, by that id: the mark is what later tells our
/// window from one of the same name that someone else opens. When the id cannot be read or the
/// mark cannot be set, the window is removed by its id (it is ours: this call just made it) and
/// the spawn fails.
fn spawn_muse_window(
    window: &str,
    cwd: Option<&str>,
    launch: Option<&LaunchSpec>,
    hosted_id: &str,
) -> bool {
    if hosted_id.is_empty() {
        return false;
    }
    let session_runs = tmux_session_exists(MUSE_SESSION);
    if session_runs && listed_window_count(MUSE_SESSION, window) != Some(0) {
        return false;
    }
    let args = tmux_muse_spawn_args_with_hosted_id(session_runs, window, cwd, launch, hosted_id);
    let Some(out) = tmux_stdout(&args.iter().map(String::as_str).collect::<Vec<_>>()) else {
        return false;
    };
    let id = out.trim();
    if !is_window_id(id) {
        // The window may exist, but without its id it cannot be marked or removed safely.
        return false;
    }
    if tmux_ok(&["set-option", "-w", "-t", id, HOSTED_MARK, hosted_id]) {
        return true;
    }
    let _ = tmux_ok(&["kill-window", "-t", id]);
    false
}

/// `true` for a tmux window id (`@12`): unique within one tmux server's life, never reused.
fn is_window_id(id: &str) -> bool {
    id.strip_prefix('@')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The tail every spawn shares: the working directory (when it is a directory), then with
/// `launch` its environment and the command as an argument vector after `--` (tmux 3.2 and later
/// run it without a shell).
fn push_cwd_and_launch_with_hosted_id(
    args: &mut Vec<String>,
    cwd: Option<&str>,
    launch: Option<&LaunchSpec>,
    hosted_id: &str,
) {
    if let Some(dir) = cwd.filter(|d| Path::new(d).is_dir()) {
        args.extend(["-c".to_string(), dir.to_string()]);
    }
    let has_hosted_env = launch.is_some_and(|l| l.env.iter().any(|(k, _)| k == "FL_HOSTED_ID"));
    if let Some(launch) = launch {
        for (key, value) in &launch.env {
            args.extend(["-e".to_string(), format!("{key}={value}")]);
        }
    }
    if !hosted_id.trim().is_empty() && !has_hosted_env {
        args.extend([
            "-e".to_string(),
            format!("FL_HOSTED_ID={}", hosted_id.trim()),
        ]);
    }
    if let Some(launch) = launch {
        args.push("--".to_string());
        args.extend(launch.argv.iter().cloned());
    }
}

/// The `tmux new-session` arguments: a detached session, and with `launch` the command.
#[cfg(test)]
fn tmux_new_session_args(
    session: &str,
    window: &str,
    cwd: Option<&str>,
    launch: Option<&LaunchSpec>,
) -> Vec<String> {
    tmux_new_session_args_with_hosted_id(session, window, cwd, launch, "")
}

fn tmux_new_session_args_with_hosted_id(
    session: &str,
    window: &str,
    cwd: Option<&str>,
    launch: Option<&LaunchSpec>,
    hosted_id: &str,
) -> Vec<String> {
    let mut args: Vec<String> = ["new-session", "-d", "-s", session, "-n", window]
        .map(String::from)
        .into();
    push_cwd_and_launch_with_hosted_id(&mut args, cwd, launch, hosted_id);
    args
}

/// The tmux arguments that open `window` in the shared Muse session: `new-window` into the
/// session when it runs (`-t =muse:` is the session by exact name, the next free index), else
/// `new-session`.
#[cfg(test)]
fn tmux_muse_spawn_args(
    session_runs: bool,
    window: &str,
    cwd: Option<&str>,
    launch: Option<&LaunchSpec>,
) -> Vec<String> {
    tmux_muse_spawn_args_with_hosted_id(session_runs, window, cwd, launch, "")
}

fn tmux_muse_spawn_args_with_hosted_id(
    session_runs: bool,
    window: &str,
    cwd: Option<&str>,
    launch: Option<&LaunchSpec>,
    hosted_id: &str,
) -> Vec<String> {
    // `-P -F '#{window_id}'` prints the new window's id, to mark the window by.
    let mut args: Vec<String> = if session_runs {
        let target = format!("={MUSE_SESSION}:");
        [
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{window_id}",
            "-t",
            &target,
            "-n",
            window,
        ]
        .map(String::from)
        .into()
    } else {
        [
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{window_id}",
            "-s",
            MUSE_SESSION,
            "-n",
            window,
        ]
        .map(String::from)
        .into()
    };
    push_cwd_and_launch_with_hosted_id(&mut args, cwd, launch, hosted_id);
    args
}

/// `false` for a unit that must never be killed: the shared Muse session itself (other agents'
/// windows live in it, so a hand-edited record naming `muse` cannot `kill-session -t =muse`), a
/// window with an empty session or window part (`muse:`), and an empty unit.
fn is_killable_unit(unit: &str) -> bool {
    if unit.is_empty() || unit == MUSE_SESSION {
        return false;
    }
    match unit.split_once(':') {
        Some((session, window)) => !session.is_empty() && !window.is_empty(),
        None => true,
    }
}

/// Stop a runtime unit this lifecycle created; `true` when nothing of ours is left of it. A
/// window unit (`muse:<persona>`) is stopped with `kill-window` on that window's id, and only
/// when exactly one window has that name and it carries the mark `hosted_id` ([`HOSTED_MARK`]):
/// never the session, whose other windows belong to other agents, and never a window of the
/// name that someone else opened (no mark, or another one), which is "nothing of ours to kill"
/// and answers `true`. Two windows of the name, or no answer from tmux: `false`.
fn kill_tmux_runtime(unit: &str, hosted_id: &str) -> bool {
    if !is_killable_unit(unit) {
        return false;
    }
    let Some((session, window)) = unit.split_once(':') else {
        let target = format!("={unit}");
        return tmux_ok(&["kill-session", "-t", &target]) || !tmux_session_exists(unit);
    };
    if hosted_id.is_empty() {
        return false;
    }
    let Some(windows) = marked_windows(session) else {
        return false;
    };
    let named: Vec<&TmuxWindow> = windows.iter().filter(|w| w.name == window).collect();
    match named.as_slice() {
        [] => true,
        [ours] if ours.mark == hosted_id && is_window_id(&ours.id) => {
            tmux_ok(&["kill-window", "-t", &ours.id])
                || marked_windows(session)
                    .is_some_and(|after| after.iter().all(|w| w.id != ours.id))
        }
        // Someone else's window of the name: not ours to kill.
        [foreign] if foreign.mark != hosted_id => true,
        // Two windows of the name, or an id tmux did not give as one.
        _ => false,
    }
}

/// As [`kill_tmux_runtime`], except a Muse window is stopped when exactly one window has
/// that name, whether or not it carries `hosted_id`. Two windows of the name are left
/// alone (`false`). A non-Muse `session:window` stops that window when the session has
/// another window, or the session when the window is its only one. No answer from tmux
/// is `false` (the session is not claimed gone). A missing window is `true` and does
/// not kill the session. A bare session is killed whole, as [`kill_tmux_runtime`].
fn kill_tmux_runtime_owner(unit: &str, hosted_id: &str) -> bool {
    if !is_killable_unit(unit) {
        return false;
    }
    if is_muse_window_target(unit) {
        let Some((session, window)) = unit.split_once(':') else {
            return false;
        };
        let Some(windows) = marked_windows(session) else {
            return false;
        };
        let named: Vec<&TmuxWindow> = windows.iter().filter(|w| w.name == window).collect();
        return match named.as_slice() {
            [] => true,
            [one] if is_window_id(&one.id) => {
                tmux_ok(&["kill-window", "-t", &one.id])
                    || marked_windows(session)
                        .is_some_and(|after| after.iter().all(|w| w.id != one.id))
            }
            _ => false,
        };
    }
    let Some((session, window)) = unit.split_once(':') else {
        return kill_tmux_runtime(unit, hosted_id);
    };
    kill_non_muse_window_owner(session, window)
}

/// Stop `session:window` without taking another vendor's window with it.
fn kill_non_muse_window_owner(session: &str, window: &str) -> bool {
    let Some(windows) = list_windows_all() else {
        return false;
    };
    let in_session: Vec<_> = windows.iter().filter(|(name, _)| name == session).collect();
    if !in_session.iter().any(|(_, name)| name == window) {
        return true;
    }
    if in_session.len() > 1 {
        let exact = fl_wire::dispatcher::tmux_exact_target(&format!("{session}:{window}"));
        return tmux_ok(&["kill-window", "-t", &exact]);
    }
    let target = format!("={session}");
    if tmux_ok(&["kill-session", "-t", &target]) {
        return true;
    }
    // A failed kill is not proof the session is gone. Only a later successful list is.
    match list_windows_all() {
        Some(after) => !after.iter().any(|(name, _)| name == session),
        None => false,
    }
}

fn capture_tmux_pane(target: &str) -> String {
    let exact = fl_wire::dispatcher::tmux_exact_target(target);
    tmux_stdout(&["capture-pane", "-p", "-t", &exact, "-S", "-40"]).unwrap_or_default()
}

fn send_tmux_keys(target: &str, keys: &str) -> bool {
    let exact = fl_wire::dispatcher::tmux_exact_target(target);
    !keys.is_empty() && tmux_ok(&["send-keys", "-t", &exact, keys])
}

/// The text Grok Build 1.0.70 prints when `--yolo` is not enough for a new directory.
const GROK_DIRECTORY_TRUST_PROMPT: &str = "Do you trust the contents of this directory";

/// How many times a fresh Grok pane is read for [`GROK_DIRECTORY_TRUST_PROMPT`]. The prompt
/// is not on screen the instant `tmux new-session -d` returns.
const GROK_TRUST_CAPTURES: u32 = 10;

fn accept_grok_directory_trust(runtime: &dyn SessionRuntime, target: &str) {
    poll_grok_directory_trust(
        target,
        runtime.settle_delay(),
        || runtime.capture(target),
        |keys| {
            let _ = runtime.send_keys(target, keys);
        },
    );
}

/// Reads `capture` up to [`GROK_TRUST_CAPTURES`] times, [`SessionRuntime::settle_delay`] apart
/// after a miss. A zero delay does not sleep. Sends `y` on the first capture that contains
/// [`GROK_DIRECTORY_TRUST_PROMPT`] and then returns. Ten misses send nothing and warn once.
fn poll_grok_directory_trust(
    target: &str,
    delay: Duration,
    mut capture: impl FnMut() -> String,
    mut send_keys: impl FnMut(&str),
) {
    for attempt in 0..GROK_TRUST_CAPTURES {
        if capture().contains(GROK_DIRECTORY_TRUST_PROMPT) {
            send_keys("y");
            return;
        }
        if attempt + 1 < GROK_TRUST_CAPTURES && !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
    eprintln!(
        "warning: Grok directory trust prompt was not seen on `{target}` after {GROK_TRUST_CAPTURES} captures; the match text is the Grok Build 1.0.70 sentence `{GROK_DIRECTORY_TRUST_PROMPT}`"
    );
}

/// The modal Claude Code prints on first launch when auto mode asks for environment setup.
const CLAUDE_AUTO_MODE_PROMPT: &str = "Teach auto mode about your environment";

fn dismiss_claude_auto_mode_prompt(runtime: &dyn SessionRuntime, target: &str) {
    poll_claude_auto_mode_prompt(
        runtime.settle_delay(),
        || runtime.capture(target),
        |keys| {
            let _ = runtime.send_keys(target, keys);
        },
    );
}

fn poll_claude_auto_mode_prompt(
    delay: Duration,
    mut capture: impl FnMut() -> String,
    mut send_keys: impl FnMut(&str),
) {
    for attempt in 0..GROK_TRUST_CAPTURES {
        if capture().contains(CLAUDE_AUTO_MODE_PROMPT) {
            send_keys("Escape");
            return;
        }
        if attempt + 1 < GROK_TRUST_CAPTURES && !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
}

fn apply_startup_prompt_rules(runtime: &dyn SessionRuntime, vendor: &str, target: &str) {
    if vendor == "grok" {
        accept_grok_directory_trust(runtime, target);
        return;
    }
    if vendor == "claude" {
        dismiss_claude_auto_mode_prompt(runtime, target);
        return;
    }
    let Some(adapter) = fl_wire::vendor_adapter::lookup_vendor_cli_adapter(vendor) else {
        return;
    };
    let delay = runtime.settle_delay();
    for rule in adapter.startup_prompt_rules() {
        let mut matched = false;
        for attempt in 0..GROK_TRUST_CAPTURES {
            if runtime.capture(target).contains(rule.match_text) {
                let _ = runtime.send_keys(target, rule.response_keys);
                matched = true;
                break;
            }
            if attempt + 1 < GROK_TRUST_CAPTURES && !delay.is_zero() {
                std::thread::sleep(delay);
            }
        }
        if !matched && rule.warn_on_miss {
            eprintln!(
                "warning: startup prompt `{}` was not seen on `{target}` after {GROK_TRUST_CAPTURES} captures",
                rule.match_text
            );
        }
    }
}

/// One window of a tmux session as [`marked_windows`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TmuxWindow {
    /// `#{window_id}` (`@12`).
    id: String,
    /// `#{window_name}`.
    name: String,
    /// `#{@fl_hosted}` ([`HOSTED_MARK`]); empty for a window nobody marked.
    mark: String,
}

/// The windows of `session` with their ids and marks: empty when the session does not run,
/// `None` when tmux did not answer about a session that runs.
fn marked_windows(session: &str) -> Option<Vec<TmuxWindow>> {
    if !tmux_session_exists(session) {
        return Some(Vec::new());
    }
    let out = tmux_stdout(&[
        "list-windows",
        "-t",
        &format!("={session}"),
        "-F",
        &format!("#{{window_id}}\t#{{window_name}}\t#{{{HOSTED_MARK}}}"),
    ])?;
    Some(out.lines().filter_map(parse_window_line).collect())
}

/// `<id>\t<name>\t<mark>`: the id has no tab and neither has the mark (a session id), so a tab
/// in the name stays in the name.
fn parse_window_line(line: &str) -> Option<TmuxWindow> {
    let (id, rest) = line.split_once('\t')?;
    let (name, mark) = rest.rsplit_once('\t')?;
    Some(TmuxWindow {
        id: id.to_string(),
        name: name.to_string(),
        mark: mark.to_string(),
    })
}

/// `true` when the runtime unit is the one `hosted_id` started: for a window, one window of that
/// exact name that carries the mark `hosted_id`; for a session, as [`tmux_unit_exists`].
fn tmux_unit_owned(unit: &str, hosted_id: &str) -> bool {
    let Some((session, window)) = unit.split_once(':') else {
        return tmux_session_exists(unit);
    };
    !hosted_id.is_empty()
        && marked_windows(session)
            .is_some_and(|ws| ws.iter().any(|w| w.name == window && w.mark == hosted_id))
}

/// `true` when a `tmux` session of exactly this name runs.
fn tmux_session_exists(session: &str) -> bool {
    tmux_ok(&["has-session", "-t", &format!("={session}")])
}

/// Every live window, or `None` when tmux did not answer. A failed command is not
/// an empty list: callers must not treat it as "the window is gone".
fn list_windows_all() -> Option<Vec<(String, String)>> {
    let out = tmux_stdout(&[
        "list-windows",
        "-a",
        "-F",
        "#{session_name}\t#{window_name}",
    ])?;
    Some(
        out.lines()
            .filter_map(|line| {
                let (session, window) = line.split_once('\t')?;
                let session = session.trim();
                let window = window.trim();
                if session.is_empty() || window.is_empty() {
                    return None;
                }
                Some((session.to_string(), window.to_string()))
            })
            .collect(),
    )
}

/// Confirmed liveness of a hosted window. `exists() == false` is not gone: a tmux
/// command that fails to run, times out, or exits non-zero is [`RuntimePresence::Unknown`].
/// Gone means tmux answered and the window is absent, or `list-panes` on the exact
/// target shows every pane dead and its process gone. A dead pane whose process
/// still exists is not gone. A `list-panes` failure is unknown.
fn tmux_runtime_presence(target: &str) -> RuntimePresence {
    let Some(windows) = list_windows_all() else {
        return RuntimePresence::Unknown;
    };
    let Some((session, window)) = target.split_once(':') else {
        return if windows.iter().any(|(name, _)| name == target) {
            RuntimePresence::Alive
        } else {
            RuntimePresence::Gone
        };
    };
    if !windows
        .iter()
        .any(|(name, win)| name == session && win == window)
    {
        return RuntimePresence::Gone;
    }
    panes_presence(&fl_wire::dispatcher::tmux_exact_target(target))
}

fn panes_presence(exact: &str) -> RuntimePresence {
    let Some(out) = tmux_stdout(&["list-panes", "-t", exact, "-F", "#{pane_dead}\t#{pane_pid}"])
    else {
        return RuntimePresence::Unknown;
    };
    let mut saw_pane = false;
    for line in out.lines().map(str::trim).filter(|line| !line.is_empty()) {
        saw_pane = true;
        let Some((dead, pid)) = line.split_once('\t') else {
            return RuntimePresence::Unknown;
        };
        let dead = dead.trim() == "1";
        let process_gone = match pid.trim().parse::<u32>() {
            Ok(pid) if pid > 0 => !Path::new(&format!("/proc/{pid}")).exists(),
            _ => true,
        };
        if !dead || !process_gone {
            return RuntimePresence::Alive;
        }
    }
    if saw_pane {
        RuntimePresence::Gone
    } else {
        // A successful empty list has no live pane.
        RuntimePresence::Gone
    }
}

/// `true` when the runtime unit runs: a session by its name, a window (`session:window`) by the
/// exact name of one of the session's windows.
fn tmux_unit_exists(unit: &str) -> bool {
    match unit.split_once(':') {
        Some((session, window)) => tmux_window_count(session, window).is_some_and(|n| n > 0),
        None => tmux_session_exists(unit),
    }
}

/// How many windows of the session are named exactly `window`: `Some(0)` when the session does
/// not run or has none, `None` when tmux did not answer about a session that runs.
fn tmux_window_count(session: &str, window: &str) -> Option<usize> {
    if !tmux_session_exists(session) {
        return Some(0);
    }
    listed_window_count(session, window)
}

/// As [`tmux_window_count`] for a session known to run: `None` when tmux did not answer.
fn listed_window_count(session: &str, window: &str) -> Option<usize> {
    let names = tmux_stdout(&[
        "list-windows",
        "-t",
        &format!("={session}"),
        "-F",
        "#{window_name}",
    ])?;
    Some(names.lines().filter(|name| *name == window).count())
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
        }
        | VendorRoute::HostCli {
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

/// `true` when `fl-host` or tmux knows `target` (the binary fl-wire uses: `FL_WIRE_TMUX_BIN`, else `tmux`).
fn tmux_target_exists(target: &str) -> bool {
    if std::env::var_os("FL_WIRE_TMUX_BIN").is_none() && HostRuntime.exists(target) {
        return true;
    }
    let mut cmd = Command::new(tmux_bin());
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
pub(crate) fn dispatch_with_rediscovery_typed<T>(
    home_override: Option<&Path>,
    query: RouteQuery<'_>,
    mut send: impl FnMut(&VendorRoute) -> Result<T, PushError>,
) -> Option<Result<T, PushError>> {
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
                Some(fresh) => send(&fresh),
                None => Err(PushError::NotConnected(refused)),
            }
        }
        sent => sent,
    })
}

pub(crate) fn dispatch_with_rediscovery<T>(
    home_override: Option<&Path>,
    query: RouteQuery<'_>,
    send: impl FnMut(&VendorRoute) -> Result<T, PushError>,
) -> Option<Result<T, String>> {
    dispatch_with_rediscovery_typed(home_override, query, send)
        .map(|result| result.map_err(String::from))
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

/// Preserve the handoff result separately from evidence of acceptance.
struct HydrationOutcome {
    dispatched: bool,
    status: HydrationStatus,
}

impl HydrationOutcome {
    fn from_launch(survived: bool) -> Self {
        Self {
            dispatched: survived,
            status: if survived {
                HydrationStatus::Unconfirmed
            } else {
                HydrationStatus::Failed
            },
        }
    }

    fn from_delivery(delivery: Option<Result<OperatorPromptOutcome, PushError>>) -> Self {
        let dispatched = matches!(&delivery, Some(Ok(_)));
        let status = match delivery {
            Some(Ok(OperatorPromptOutcome::AppServerAccepted { .. })) => HydrationStatus::Confirmed,
            Some(Ok(
                OperatorPromptOutcome::VendorQueued
                | OperatorPromptOutcome::TransportAccepted
                | OperatorPromptOutcome::CompactionAccepted,
            ))
            | Some(Err(PushError::Failed(_))) => HydrationStatus::Unconfirmed,
            None | Some(Err(PushError::NotConnected(_))) => HydrationStatus::Failed,
        };
        Self { dispatched, status }
    }
}

/// Best-effort ordinary delivery. Only proven non-delivery permits rediscovery and retry.
fn deliver_prompt(
    env: &LifecycleEnv<'_>,
    persona: &str,
    session_id: &str,
    by: &str,
    prompt: &str,
) -> HydrationOutcome {
    HydrationOutcome::from_delivery(dispatch_with_rediscovery_typed(
        env.home_override,
        RouteQuery::persona(persona, session_id),
        |route| {
            push_operator_prompt_with_intent(route, by, prompt, OperatorPromptIntent::Ordinary)
                .map_err(|error| match error {
                    OperatorPromptError::Push(error) => error,
                    // Ordinary delivery does not steer. If it ever returns such an error,
                    // conservatively treat it as ambiguous and never retry another route.
                    OperatorPromptError::Steer(error) => PushError::Failed(error.to_string()),
                })
        },
    ))
}

/// Route-table files a teardown prunes: the override, or the default and legacy paths.
pub(crate) fn route_table_paths(home_override: Option<&Path>) -> Vec<PathBuf> {
    match home_override {
        Some(home) => vec![home.join(".cache").join("agent-bus").join("routes.json")],
        None => std::iter::once(resolve_routes_path(
            None,
            std::env::var_os("FL_WIRE_ROUTES").as_deref(),
            None,
        ))
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

fn unregister_routes_in_daemon<'a>(
    home_override: Option<&Path>,
    personas: impl IntoIterator<Item = &'a str>,
) {
    #[cfg(test)]
    let _ = (home_override, personas);
    #[cfg(not(test))]
    {
        if home_override.is_some() {
            return;
        }
        let sock = fl_wire::daemon::resolve_socket_path(None, None, None);
        for persona in personas {
            let persona = persona.trim().to_ascii_lowercase();
            if persona.is_empty() {
                continue;
            }
            if let Ok(mut client) = fl_wire::daemon::LocalClient::connect(
                &sock,
                fl_wire::daemon::DEFAULT_LOCAL_RPC_TIMEOUT,
            ) {
                let _ = client.request(&fl_wire::daemon::LocalRequest::UnregisterRoute { persona });
            }
        }
    }
}

fn remove_routes_locked(
    home_override: Option<&Path>,
    personas: &[&str],
    session_ids: &[&str],
) -> usize {
    let mut removed = 0;
    let mut dropped_personas: BTreeSet<String> = personas
        .iter()
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
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
            let drop_entry = by_key || by_id;
            if drop_entry {
                dropped_personas.insert(key.to_ascii_lowercase());
            }
            !drop_entry
        });
        let dropped = before - table.entries.len();
        if dropped > 0 && table.save(Some(&path)).is_ok() {
            removed += dropped;
        }
    }
    unregister_routes_in_daemon(home_override, dropped_personas.iter().map(String::as_str));
    removed
}

/// Spin up a session: register the agent, persist its record, open its tmux runtime, hydrate it.
///
/// The record is written before tmux starts, so a runtime this call opens always has a record
/// that teardown can find; the record is then updated with what actually started.
///
/// Directory a spin-up should start in when the request names none.
///
/// A named config binding wins. Within the workspace, its top-level project umbrella is
/// the runtime directory; outside it, the registered root is retained. Invalid or ambiguous
/// bindings leave cwd unset. Without a readable binding, preserve catalog/directory fallback.
fn resolve_project_cwd<S: Catalog>(
    store: &S,
    project: &str,
    workspace_root: Option<&Path>,
    home_override: Option<&Path>,
) -> Option<String> {
    use crate::project_init::{config_entries, read_config, user_config_path};

    if let Some(config) = user_config_path(home_override)
        && let Ok(entries) = read_config(&config).and_then(|doc| config_entries(&doc, &config))
    {
        let bindings: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry
                    .slug()
                    .is_some_and(|slug| slug.eq_ignore_ascii_case(project))
            })
            .collect();
        if !bindings.is_empty() {
            // Every matching binding must resolve before any may supply a runtime path.
            let mut roots: Vec<_> = bindings
                .into_iter()
                .map(|entry| {
                    let root = Path::new(&entry.root);
                    root.is_absolute()
                        .then(|| root.canonicalize().ok().filter(|path| path.is_dir()))
                        .flatten()
                })
                .collect::<Option<_>>()?;
            roots.sort();
            roots.dedup();
            if roots.len() != 1 {
                return None;
            }
            let root = roots.pop()?;
            let cwd = workspace_root
                .and_then(|workspace| workspace.canonicalize().ok())
                .and_then(|workspace| {
                    let child = root.strip_prefix(&workspace).ok()?.components().next()?;
                    Some(workspace.join(child))
                })
                .unwrap_or(root);
            return Some(cwd.display().to_string());
        }
    }
    if let Ok(projects) = store.list_projects() {
        for item in projects {
            let root = Path::new(item.root.trim());
            let Some(name) = root.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.eq_ignore_ascii_case(project) && root.is_dir() {
                return Some(root.display().to_string());
            }
        }
    }
    let workspace = workspace_root?;
    let entries = std::fs::read_dir(workspace).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(project)
        {
            return Some(path.display().to_string());
        }
    }
    None
}

type HostedSessionLockKey = (Option<PathBuf>, Option<PathBuf>, String);

fn hosted_session_lifecycle_mutex(
    env: &LifecycleEnv<'_>,
    session_id: &str,
) -> std::sync::Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<HostedSessionLockKey, std::sync::Arc<std::sync::Mutex<()>>>,
        >,
    > = std::sync::OnceLock::new();
    let map = LOCKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = (
        env.home_override.map(Path::to_path_buf),
        env.workspace_root.map(Path::to_path_buf),
        session_id.trim().to_ascii_lowercase(),
    );
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(key)
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
        .clone()
}

/// `caller` gates hosting an unmanaged agent: an Active agent with no LIVE hosted record for this
/// session (none, or a decommissioned one left from an earlier teardown) becomes hosted by the
/// spin-up, after which a peer's teardown would be allowed, so only the owner may do it. A peer
/// may still spin up a new or retired persona, or re-spin a session whose hosted record is live.
pub fn spin_up<S: AgentRegistry + StateStore + Catalog + MessageStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    req: &SpinUpRequest,
    by: &str,
    caller: Caller,
) -> Result<SpinUpResponse, LifecycleError> {
    spin_up_with_store(store, || Ok(store), env, req, by, caller)
}

/// Like [`spin_up`], but releases `store_guard` across [`resume_launch_survived`] and re-opens it
/// via `reopen_store` afterward so hub-wide store locks are never held during the survival watch.
pub fn spin_up_with_store<S, G, F>(
    store_guard: G,
    mut reopen_store: F,
    env: &LifecycleEnv<'_>,
    req: &SpinUpRequest,
    by: &str,
    caller: Caller,
) -> Result<SpinUpResponse, LifecycleError>
where
    S: AgentRegistry + StateStore + Catalog + MessageStore,
    G: std::ops::Deref<Target = S>,
    F: FnMut() -> Result<G, LifecycleError>,
{
    let mut v = req.validate()?;
    let session_id = format!("{}:{}", v.vendor, v.persona);
    let session_mutex = hosted_session_lifecycle_mutex(env, &session_id);
    let _session_lifecycle_guard = match session_mutex.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            drop(store_guard);
            let _waited = session_mutex.lock().unwrap_or_else(|e| e.into_inner());
            return Err(LifecycleError::Conflict(format!(
                "hosted session `{session_id}` already had a lifecycle operation in progress"
            )));
        }
    };
    let store: &S = &store_guard;
    v.permission_receipts = fl_wire::vendor_adapter::launch_permissions_with_auto_approval(
        &v.vendor,
        v.posture,
        env.auto_approval_for(&v.vendor),
    )
    .map_err(|msg| LifecycleError::Invalid(format!("posture: {msg}")))?
    .receipts;
    let recast_assignment = current_recast_assignment(store, &v.persona)?;
    let recast_staged_in_store = recast_assignment.is_some();
    if let Some(agent) = recast_assignment {
        v.project = agent.project;
        v.role = role_label_of(&agent.role);
    }
    validate_catalog_role(store, &v.role)?;
    let display = capitalize(&v.persona);
    // Before any side effect: a window the vendor does not offer registers nothing. It is only
    // recorded here; the launch below does not apply it.
    if let Some(tokens) = v.context_window {
        crate::context_window::validate_context_window(
            &v.vendor,
            v.model.as_deref(),
            tokens,
            env.home_override,
        )
        .map_err(LifecycleError::Invalid)?;
    }
    if v.cwd.is_none() {
        v.cwd = resolve_project_cwd(store, &v.project, env.workspace_root, env.home_override);
    }
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
    let mut previous =
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
    let config_home = preflight_vendor_launch(env, &v.vendor, v.hosted, &mut v.cwd)?;
    let tmux_target = has_runtime.then(|| hosted_tmux_target(&v.vendor, &v.persona));
    let host_session_id = host_session_id_for_target(tmux_target.as_deref());
    let unit = tmux_target
        .as_deref()
        .map_or("", |t| env.runtime.unit(t))
        .to_string();
    let existing_gen = previous
        .as_ref()
        .filter(|r| !is_decommissioned(r))
        .and_then(|r| r.hydration_generation.clone())
        .filter(|g| is_valid_hydration_generation(g));
    let should_check_unit = has_runtime
        && v.hosted
        && env.spawn_runtime
        && (existing_gen.is_some()
            || previous
                .as_ref()
                .is_some_and(|r| !is_decommissioned(r) && r.vendor == "claude"));
    let unit_not_running = unit.is_empty() || (should_check_unit && !env.runtime.exists(&unit));
    let will_spawn = has_runtime
        && v.hosted
        && env.spawn_runtime
        && (existing_gen.is_none() || unit_not_running);

    let mut resume_decision: Option<ClaudeResumeDecision> = None;
    if v.vendor == "claude"
        && has_runtime
        && v.hosted
        && env.spawn_runtime
        && unit_not_running
        && let Some(prev) = previous
            .as_mut()
            .filter(|r| !is_decommissioned(r) && r.vendor == "claude")
    {
        env.runtime.preflight_spawn(&session_id)?;
        let _ = sync_claude_vendor_session_id_from_route(store, env, prev);
        let recast_staged = recast_staged_in_store
            || !prev.project.eq_ignore_ascii_case(&v.project)
            || !prev.role.eq_ignore_ascii_case(&v.role);
        resume_decision = Some(evaluate_claude_resume_eligibility(
            store,
            env,
            prev,
            recast_staged,
            v.model.as_deref(),
            v.thinking_level.as_deref(),
            v.context_window,
            v.posture,
            v.cwd.as_deref(),
        )?);
    }

    let mut hydration_generation = match &resume_decision {
        Some(ClaudeResumeDecision::ResumeExisting {
            hydration_generation: g,
            ..
        }) => g.clone(),
        _ if will_spawn => next_hydration_generation(),
        _ => existing_gen
            .clone()
            .unwrap_or_else(next_hydration_generation),
    };
    let guidance = registered_role_prompt_with_generation(
        store,
        env,
        v.cwd.as_deref().map(Path::new),
        &display,
        &v.project,
        &agent_role_from_label(&v.role),
        Some(&hydration_generation),
    )?;
    if env.log_hydration_warnings {
        crate::hydration::log_warnings(&v.persona, &session_id, &guidance.warnings);
    }
    let mut warnings = guidance.warnings;
    let mut prompt = guidance.text;

    let mut is_claude_resume = false;
    let (mut vendor_session_id, mut launch) = match resume_decision.take() {
        Some(ClaudeResumeDecision::ResumeExisting {
            vendor_session_id: sid,
            launch: resume_launch,
            ..
        }) => {
            is_claude_resume = true;
            (Some(sid), resume_launch)
        }
        Some(ClaudeResumeDecision::FreshLaunch {
            reason_code,
            detail,
        }) => {
            push_claude_resume_fallback_notice(
                env,
                &v.persona,
                &mut warnings,
                reason_code,
                &detail,
            );
            if let Some(prev_g) = existing_gen.as_ref() {
                let _ = store.delete_state_by_key(
                    &StateScope::Workspace,
                    StateConcern::Runtime,
                    &hydration_ack_slug(prev_g),
                );
            }
            let fresh_sid = Some(uuid::Uuid::now_v7().to_string());
            let spec = launch_spec_for_with_session(
                env,
                &v.vendor,
                &v.persona,
                v.model.as_deref(),
                v.thinking_level.as_deref(),
                v.posture,
                &prompt,
                v.cwd.as_deref(),
                fresh_sid.as_deref(),
                Some(&hydration_generation),
            )?;
            (fresh_sid, spec)
        }
        None => {
            let sid = (v.vendor == "claude").then(|| uuid::Uuid::now_v7().to_string());
            let spec = if has_runtime && v.hosted && env.spawn_runtime {
                env.runtime.preflight_spawn(&session_id)?;
                launch_spec_for_with_session(
                    env,
                    &v.vendor,
                    &v.persona,
                    v.model.as_deref(),
                    v.thinking_level.as_deref(),
                    v.posture,
                    &prompt,
                    v.cwd.as_deref(),
                    sid.as_deref(),
                    Some(&hydration_generation),
                )?
            } else {
                None
            };
            (sid, spec)
        }
    };
    pin_launch_config(&mut launch, config_home.as_deref());
    register_spun_up_agent(store, &v, by)?;

    let now = now_ms();
    let mut record = HostedSessionRecord {
        session_id: session_id.clone(),
        persona: v.persona.clone(),
        project: v.project.clone(),
        role: v.role.clone(),
        vendor: v.vendor.clone(),
        model: v.model.clone(),
        thinking_level: v.thinking_level.clone(),
        node_id: v.node_id.clone(),
        hosted: v.hosted,
        status: STATUS_RUNNING.to_string(),
        stop_reason: None,
        restart_policy: "on-failure".to_string(),
        restart_count: previous.as_ref().map_or(0, |r| r.restart_count),
        tmux_target: tmux_target.clone(),
        host_session_id: host_session_id.clone(),
        cwd: v.cwd.clone(),
        context_window: v.context_window,
        posture: v.posture,
        hydration_dispatched: is_claude_resume,
        hydration_status: if is_claude_resume {
            crate::lifecycle::HydrationStatus::Confirmed
        } else {
            crate::lifecycle::HydrationStatus::Unconfirmed
        },
        hydration_generation: Some(hydration_generation.clone()),
        tombstone_ids: previous
            .as_ref()
            .filter(|r| !is_decommissioned(r))
            .map(|r| r.tombstone_ids.clone())
            .unwrap_or_default(),
        vendor_session_id: vendor_session_id.clone(),
        pid: None,
        runtime_leader_pid: None,
        pid_start_time: None,
        // A decommissioned record's runtime is gone; the tmux session now under the name, if any,
        // is someone else's and a later teardown must not kill it.
        // Only when the record named this same target: a runtime under another name (an older
        // record's session) is not the one now under this name.
        runtime_spawned: has_runtime
            && previous.as_ref().is_some_and(|r| {
                r.runtime_spawned
                    && is_live_hosted(r)
                    && r.tmux_target == tmux_target
                    && !muse_window_not_ours(env, &v.vendor, &unit, &session_id)
            }),
        created_at_ms: previous.as_ref().map_or(now, |r| r.created_at_ms),
        updated_at_ms: now,
        decommission_prepared_at_ms: None,
    };
    if !will_spawn && !is_claude_resume && existing_gen.is_some() {
        let _ = store.delete_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            &hydration_ack_slug(&hydration_generation),
        );
    }
    let dropped_ids = previous.as_ref().map(record_native_ids).unwrap_or_default();
    persist_hosted_session(store, &record, by)?;
    delete_hosted_respawn_state(store, &session_id);
    with_dead_streaks(|streaks| {
        streaks.remove(&(env.home_override.map(Path::to_path_buf), session_id.clone()));
    });
    // The running record replaced any decommissioned one on this slug. Drop that
    // tombstone before hydration, or discovery would hide the new session.
    refresh_decommission_tombstones_dropping(store, env.home_override, &dropped_ids);
    // A route left by another vendor would take the hydration prompt.
    purge_cross_vendor_routes(store, env.home_override, &v.persona, &v.vendor);

    let (rt_session, rt_window) = runtime_names(&v.vendor, &v.persona);
    let resume_launch_started_ms = now_ms();
    let mut raw_spawned = has_runtime
        && v.hosted
        && env.spawn_runtime
        && env.runtime.spawn(
            &rt_session,
            &rt_window,
            v.cwd.as_deref(),
            launch.as_ref(),
            &session_id,
        );
    if raw_spawned && let Some(target) = tmux_target.as_deref() {
        apply_startup_prompt_rules(env.runtime, &v.vendor, target);
    }
    let mut launched = raw_spawned && launch.is_some();
    let survived;
    let mut resumed_existing = false;
    let _reopened_guard;
    let store: &S = if is_claude_resume {
        let resumed_sid = vendor_session_id.clone().unwrap_or_default();
        if raw_spawned {
            record.runtime_spawned = true;
            let initial_pid = env.runtime.recorded_pid(&unit, &session_id);
            record.pid = initial_pid;
            record.pid_start_time = initial_pid.and_then(fl_wire::identity::proc_start_time_ticks);
            record.updated_at_ms = now_ms();
            persist_hosted_session(store, &record, by)?;
            let mut pre_rstate = load_claude_resume_state(store, &session_id);
            pre_rstate.resume_failed = false;
            pre_rstate.resume_unconfirmed_session_id = Some(resumed_sid.clone());
            pre_rstate.resume_launched_at_ms = Some(resume_launch_started_ms);
            pre_rstate.in_survival_window = true;
            save_claude_resume_state(store, &session_id, &pre_rstate, by)?;
        }
        drop(store_guard);
        let (survived_resume, hook_confirmed) = if raw_spawned {
            if launch.is_some() {
                resume_launch_survived(
                    env,
                    &unit,
                    &session_id,
                    &v.persona,
                    &resumed_sid,
                    &hydration_generation,
                    resume_launch_started_ms,
                )
            } else {
                (true, true)
            }
        } else {
            (false, false)
        };
        _reopened_guard = reopen_store()?;
        let store: &S = &_reopened_guard;
        let Some(fresh_record) = get_hosted_session_by_id(store, &session_id)? else {
            if raw_spawned {
                let _ = env.runtime.kill(&unit, &session_id);
            }
            delete_claude_resume_state(store, &session_id);
            return Err(LifecycleError::Conflict(format!(
                "hosted session `{session_id}` was removed during launch survival watch"
            )));
        };
        if is_decommissioned(&fresh_record) || fresh_record.status == STATUS_DECOMMISSIONING {
            if raw_spawned {
                let _ = env.runtime.kill(&unit, &session_id);
            }
            delete_claude_resume_state(store, &session_id);
            return Err(decommissioned_conflict(&session_id));
        }
        if fresh_record.hydration_generation.as_deref() != Some(hydration_generation.as_str()) {
            if raw_spawned {
                let _ = env.runtime.kill(&unit, &session_id);
            }
            let mut rstate = load_claude_resume_state(store, &session_id);
            if rstate.in_survival_window
                && rstate.resume_launched_at_ms == Some(resume_launch_started_ms)
            {
                rstate.in_survival_window = false;
                let _ = save_claude_resume_state(store, &session_id, &rstate, by);
            }
            return Err(LifecycleError::Conflict(format!(
                "hosted session `{session_id}` launch generation changed during survival watch"
            )));
        }
        record = fresh_record;
        if raw_spawned && survived_resume {
            survived = true;
            resumed_existing = true;
            record.runtime_spawned = true;
            if let Some(new_pid) = env.runtime.recorded_pid(&unit, &session_id) {
                record.pid = Some(new_pid);
                record.pid_start_time = fl_wire::identity::proc_start_time_ticks(new_pid);
            }
            let mut rstate = load_claude_resume_state(store, &session_id);
            rstate.resume_failed = false;
            rstate.in_survival_window = false;
            rstate.resume_unconfirmed_session_id = if hook_confirmed {
                None
            } else {
                Some(resumed_sid)
            };
            rstate.resume_launched_at_ms = if hook_confirmed {
                None
            } else {
                Some(resume_launch_started_ms)
            };
            save_claude_resume_state(store, &session_id, &rstate, by)?;
        } else {
            let _ = env.runtime.kill(&unit, &session_id);
            let failed_sid = resumed_sid;
            push_tombstone(&mut record.tombstone_ids, &failed_sid);
            push_claude_resume_fallback_notice(
                env,
                &v.persona,
                &mut warnings,
                "resume_launch_did_not_survive",
                &format!("--resume `{failed_sid}` did not survive startup"),
            );
            let _ = store.delete_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                &hydration_ack_slug(&hydration_generation),
            );
            hydration_generation = next_hydration_generation();
            let fresh_sid = uuid::Uuid::now_v7().to_string();
            vendor_session_id = Some(fresh_sid.clone());
            let fresh_guidance = registered_role_prompt_with_generation(
                store,
                env,
                v.cwd.as_deref().map(Path::new),
                &display,
                &v.project,
                &agent_role_from_label(&v.role),
                Some(&hydration_generation),
            )?;
            prompt = fresh_guidance.text;
            launch = launch_spec_for_with_session(
                env,
                &v.vendor,
                &v.persona,
                v.model.as_deref(),
                v.thinking_level.as_deref(),
                v.posture,
                &prompt,
                v.cwd.as_deref(),
                vendor_session_id.as_deref(),
                Some(&hydration_generation),
            )?;
            pin_launch_config(&mut launch, config_home.as_deref());
            record.vendor_session_id = vendor_session_id;
            record.hydration_generation = Some(hydration_generation.clone());
            record.hydration_status = HydrationStatus::Unconfirmed;
            record.hydration_dispatched = false;
            record.runtime_spawned = false;
            record.pid = None;
            record.runtime_leader_pid = None;
            record.pid_start_time = None;
            record.updated_at_ms = now_ms();
            persist_hosted_session(store, &record, by)?;
            raw_spawned = env.runtime.spawn(
                &rt_session,
                &rt_window,
                v.cwd.as_deref(),
                launch.as_ref(),
                &session_id,
            );
            if raw_spawned && let Some(target) = tmux_target.as_deref() {
                apply_startup_prompt_rules(env.runtime, &v.vendor, target);
            }
            launched = raw_spawned && launch.is_some();
            survived = !launched || launch_survived(env, &unit, &session_id);
            let new_pid = if raw_spawned && survived {
                env.runtime.recorded_pid(&unit, &session_id)
            } else {
                None
            };
            record.pid = new_pid;
            record.pid_start_time = new_pid.and_then(fl_wire::identity::proc_start_time_ticks);
            let mut rstate = load_claude_resume_state(store, &session_id);
            rstate.resume_failed = false;
            rstate.in_survival_window = false;
            rstate.resume_unconfirmed_session_id = None;
            rstate.resume_launched_at_ms = None;
            save_claude_resume_state(store, &session_id, &rstate, by)?;
        }
        store
    } else {
        survived = !launched || launch_survived(env, &unit, &session_id);
        let new_pid = if raw_spawned && survived {
            env.runtime.recorded_pid(&unit, &session_id)
        } else if will_spawn {
            None
        } else {
            previous
                .as_ref()
                .filter(|r| !is_decommissioned(r))
                .and_then(|r| r.pid)
        };
        record.pid = new_pid;
        record.pid_start_time = new_pid.and_then(fl_wire::identity::proc_start_time_ticks);
        if v.vendor == "claude" {
            let mut rstate = load_claude_resume_state(store, &session_id);
            rstate.resume_failed = false;
            rstate.in_survival_window = false;
            rstate.resume_unconfirmed_session_id = None;
            rstate.resume_launched_at_ms = None;
            save_claude_resume_state(store, &session_id, &rstate, by)?;
        }
        if will_spawn
            && !raw_spawned
            && let Some(prev_g) = existing_gen.as_ref()
        {
            record.hydration_generation = Some(prev_g.clone());
            let _ = store.delete_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                &hydration_ack_slug(prev_g),
            );
        }
        store
    };
    // Argv handed the prompt to the vendor, but survival does not prove acceptance.
    // A command that exited at once took its session with it: nothing to deliver to.
    let spawned = raw_spawned && survived;
    let hydration = if resumed_existing {
        HydrationOutcome {
            dispatched: true,
            status: HydrationStatus::Confirmed,
        }
    } else if launched {
        HydrationOutcome::from_launch(survived)
    } else {
        deliver_prompt(env, &v.persona, &session_id, by, &prompt)
    };
    record.runtime_spawned = (record.runtime_spawned || spawned) && survived;
    record.hydration_dispatched = hydration.dispatched;
    record.hydration_status = hydration.status;
    refresh_record_runtime_pid(env, &unit, &mut record);
    let _ = promote_confirmed_hydration(store, &mut record);
    record.updated_at_ms = now_ms();
    // A runtime may have started: the next discovery must scan the host again.
    crate::discovery::invalidate_route_discovery_cache();
    if let Err(e) = persist_hosted_session(store, &record, by) {
        if spawned {
            // The record cannot learn about this runtime, so teardown could never stop it. Only
            // the unit this call created: for Muse its window, never the shared session.
            env.runtime.kill(&unit, &session_id);
        }
        return Err(e);
    }
    if resumed_existing {
        let baseline_ms = crate::boot_reconcile::hydration_baseline_ms(store, &record);
        if let Err(err) = crate::boot_reconcile::send_service_resume_notice_in_store(
            store,
            &record.persona,
            record.cwd.as_deref().map(Path::new),
            Some(&record.role),
            baseline_ms,
        ) {
            eprintln!(
                "hub: warning ({} / {}): failed to enqueue Host restarted notice on resume: {err}",
                record.persona, record.session_id
            );
        }
    }

    Ok(SpinUpResponse {
        warnings,
        runtime_spawned: spawned,
        session_id,
        persona: v.persona,
        project: v.project,
        role: v.role,
        vendor: v.vendor,
        model: v.model,
        thinking_level: v.thinking_level,
        node_id: v.node_id,
        hosted: v.hosted,
        status: STATUS_RUNNING.to_string(),
        tmux_target,
        host_session_id,
        hydration_dispatched: hydration.dispatched,
        hydration_status: record.hydration_status,
        hydration_generation: record.hydration_generation,
        prompt: Some(prompt),
        context_window: v.context_window,
        posture: v.posture,
        permission_receipts: v.permission_receipts,
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
    /// What the teardown did to the tmux restore source (the newest tmux-resurrect save), so a
    /// host reboot does not start the stopped session again. What is pruned matches what is gone:
    /// the whole session when it is gone, only the window when a window of a session that stays
    /// (the shared Muse session) is gone. A runtime that still runs after the teardown is
    /// reported as `running` and left alone. `None` when the teardown made no runtime call. A
    /// failure is reported here and never fails the teardown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_source: Option<crate::restore_source::RestoreSourceOutcome>,
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
        thinking_level: None,
        node_id: DEFAULT_NODE_ID.to_string(),
        hosted: false,
        status: STATUS_RUNNING.to_string(),
        stop_reason: None,
        // A session this lifecycle never started is not restarted by it.
        restart_policy: "never".to_string(),
        restart_count: 0,
        tmux_target: None,
        host_session_id: None,
        cwd: None,
        context_window: None,
        posture: fl_core::permissions::Posture::Auto,
        hydration_dispatched: false,
        hydration_status: crate::lifecycle::HydrationStatus::Unconfirmed,
        hydration_generation: None,
        runtime_spawned: false,
        tombstone_ids: Vec::new(),
        vendor_session_id: None,
        pid: None,
        runtime_leader_pid: None,
        pid_start_time: None,
        created_at_ms: now,
        updated_at_ms: now,
        decommission_prepared_at_ms: None,
    })
}

/// `true` when the persona has a live route outside a hosted record (see [`find_route`]).
fn has_live_route(env: &LifecycleEnv<'_>, target: &SessionTarget) -> bool {
    find_route(
        env.home_override,
        RouteQuery::persona(&target.persona, &target.session_id),
    )
    .is_some()
}

/// Whether a decommissioned hosted record may be treated as absent: only for the Owner, and
/// only when the persona has a live route (issue #485). A Peer keeps the 403/409 it had.
fn may_adopt_over_decommissioned(
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    caller: Caller,
) -> bool {
    caller == Caller::Owner
        && target.hosted.as_ref().is_some_and(is_decommissioned)
        && has_live_route(env, target)
}

/// `target` with its decommissioned record dropped, so adoption creates a new hosted record
/// as it does when no record exists. The new record's id is the discovered (live) id when it
/// differs from the stale record's, else the raw id, else the persona.
fn adopted_target(target: &SessionTarget) -> SessionTarget {
    let stale_id = target
        .hosted
        .as_ref()
        .map_or(target.session_id.as_str(), |r| r.session_id.as_str());
    let session_id = if target.discovered_id != stale_id {
        target.discovered_id.clone()
    } else if target.raw_id != stale_id {
        target.raw_id.clone()
    } else {
        target.persona.clone()
    };
    SessionTarget {
        session_id,
        hosted: None,
        ..target.clone()
    }
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
    // A decommissioned record is history: for the Owner, a persona that is live again via a
    // route outside fl-host is adopted as a new hosted record, as when no record exists
    // (issue #485). The 409 stays when there is no live route.
    let adopted;
    let target = if target.hosted.as_ref().is_some_and(is_decommissioned) {
        if !may_adopt_over_decommissioned(env, target, caller) {
            return Err(decommissioned_conflict(&target.session_id));
        }
        adopted = adopted_target(target);
        &adopted
    } else {
        target
    };
    // Other running records are updated too. This target may become `stopped`
    // here and is then rewritten to `decommissioning` below.
    reconcile_dead_hosted_sessions(store, env, by)?;
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
        deliver_prompt(env, &target.persona, &target.session_id, by, &prompt).dispatched;

    let now = now_ms();
    let mut record = record_for_target(store, target)?;
    if record.vendor == "unknown"
        && let Some(vendor) = infer_vendor_from_routes(env.home_override, &record, target)
            .or_else(|| infer_vendor_from_hosted_siblings(store, &record))
    {
        record.vendor = vendor;
    }
    record.status = STATUS_DECOMMISSIONING.to_string();
    record.hydration_generation = None;
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
        restore_source: None,
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
/// `stopped` is not decommissioned, so the persona does not become unmanaged.
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

fn is_same_vendor_sibling(a: &HostedSessionRecord, b: &HostedSessionRecord) -> bool {
    a.session_id != b.session_id
        && a.persona.eq_ignore_ascii_case(&b.persona)
        && a.vendor != "unknown"
        && b.vendor.eq_ignore_ascii_case(&a.vendor)
}

fn infer_vendor_from_hosted_siblings<S: StateStore>(
    store: &S,
    record: &HostedSessionRecord,
) -> Option<String> {
    let records = list_hosted_sessions(store).ok()?;
    let mut vendors: BTreeSet<String> = records
        .into_iter()
        .filter(|other| {
            other.session_id != record.session_id
                && other.persona.eq_ignore_ascii_case(&record.persona)
                && is_live_hosted(other)
                && other.vendor != "unknown"
        })
        .map(|other| other.vendor.to_ascii_lowercase())
        .collect();
    (vendors.len() == 1).then(|| vendors.pop_first().unwrap())
}

/// The other live hosted records whose runtime is `record`'s runtime unit ([`runtime_unit`]).
/// The tmux session name is the capitalised persona, so `claude:ada` and `codex:ada` share
/// session `Ada`; killing it would end the other, still-live session. A Muse window is its own
/// unit: another persona's window in the shared `muse` session is not a sharer. Same-vendor
/// sibling records of the same persona (such as `codex:ada` alongside an adopted `<uuid>` record)
/// represent the same session rather than a second vendor.
fn runtime_sharers<S: StateStore>(
    store: &S,
    runtime: &dyn SessionRuntime,
    record: &HostedSessionRecord,
) -> Result<Vec<HostedSessionRecord>, LifecycleError> {
    let Some(name) = record.tmux_target.as_deref().map(|t| runtime.unit(t)) else {
        return Ok(Vec::new());
    };
    Ok(list_hosted_sessions(store)?
        .into_iter()
        .filter(|r| {
            r.session_id != record.session_id
                && !is_decommissioned(r)
                && !is_same_vendor_sibling(record, r)
                && r.tmux_target.as_deref().map(|t| runtime.unit(t)) == Some(name)
        })
        .collect())
}

/// Drop a saved route for `persona` when it belongs to a vendor other than `vendor`
/// and that vendor has no other Running hosted record of the persona.
/// A webhook route stays. A route of `vendor` stays. A decommissioned or
/// decommissioning sibling does not protect the route.
fn purge_cross_vendor_routes<S: StateStore>(
    store: &S,
    home_override: Option<&Path>,
    persona: &str,
    vendor: &str,
) {
    let records = list_hosted_sessions(store).unwrap_or_default();
    crate::discovery::with_route_file_lock(|| {
        let mut dropped_personas = BTreeSet::new();
        for path in route_table_paths(home_override) {
            if !path.is_file() {
                continue;
            }
            let mut table = RouteTable::load(Some(&path));
            let Some(key) = table
                .entries
                .keys()
                .find(|key| key.eq_ignore_ascii_case(persona))
                .cloned()
            else {
                continue;
            };
            let keep = table.entries.get(&key).is_some_and(|entry| {
                matches!(entry.route, VendorRoute::Webhook { .. })
                    || route_is_vendor(&entry.route, vendor)
                    || running_vendor_owns_route(&records, persona, &entry.route)
            });
            if keep {
                continue;
            }
            if table.entries.remove(&key).is_some() {
                dropped_personas.insert(key.to_ascii_lowercase());
            }
            let _ = table.save(Some(&path));
        }
        unregister_routes_in_daemon(home_override, dropped_personas.iter().map(String::as_str));
    });
}

/// `true` when a Running hosted record of `persona` uses the vendor `route` names.
fn running_vendor_owns_route(
    records: &[HostedSessionRecord],
    persona: &str,
    route: &VendorRoute,
) -> bool {
    let Some(route_vendor) = vendor_id_of_route(route) else {
        return false;
    };
    records.iter().any(|record| {
        record.persona.eq_ignore_ascii_case(persona)
            && record.vendor == route_vendor
            && record.status.eq_ignore_ascii_case(STATUS_RUNNING)
    })
}

fn vendor_id_of_route(route: &VendorRoute) -> Option<&str> {
    match route {
        VendorRoute::Claude { .. } => Some("claude"),
        VendorRoute::Agy { .. } => Some("antigravity"),
        VendorRoute::Codex { .. } => Some("codex"),
        VendorRoute::Muse { .. } => Some("muse"),
        VendorRoute::Grok { .. } => Some("grok"),
        VendorRoute::HostCli { vendor, .. } => Some(vendor.as_str()),
        VendorRoute::Webhook { .. } => None,
    }
}

fn route_is_vendor(route: &VendorRoute, vendor: &str) -> bool {
    match (vendor, route) {
        ("claude", VendorRoute::Claude { .. })
        | ("antigravity", VendorRoute::Agy { .. })
        | ("codex", VendorRoute::Codex { .. })
        | ("muse", VendorRoute::Muse { .. })
        | ("grok", VendorRoute::Grok { .. }) => true,
        (v, VendorRoute::HostCli { vendor: rv, .. }) => rv.eq_ignore_ascii_case(v),
        _ => false,
    }
}

fn push_tombstone(ids: &mut Vec<String>, id: &str) {
    let id = id.trim();
    if !is_native_session_id(id) || ids.iter().any(|have| have == id) {
        return;
    }
    ids.push(id.to_string());
}

const AGY_DELETE_CASCADE_PATH: &str =
    "/exa.language_server_pb.LanguageServerService/DeleteCascadeTrajectory";
const AGY_DELETE_CASCADE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const AGY_DELETE_CASCADE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// Issue `POST /exa.language_server_pb.LanguageServerService/DeleteCascadeTrajectory` with
/// `{"cascadeId": "<cascade_id>"}` against the Antigravity session's loopback language server.
#[cfg(test)]
fn post_agy_delete_cascade_trajectory(
    ls_address: &str,
    csrf_token: Option<&str>,
    cascade_id: &str,
) -> Result<(), String> {
    post_agy_delete_cascade_trajectory_with_timeout(
        ls_address,
        csrf_token,
        cascade_id,
        AGY_DELETE_CASCADE_TIMEOUT,
    )
}

/// Issue `POST /exa.language_server_pb.LanguageServerService/DeleteCascadeTrajectory` with
/// `{"cascadeId": "<cascade_id>"}` against the Antigravity session's loopback language server.
fn post_agy_delete_cascade_trajectory_with_timeout(
    ls_address: &str,
    csrf_token: Option<&str>,
    cascade_id: &str,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let Some(port) = fl_wire::identity::agy_ls_port(ls_address) else {
        return Err("non-loopback language-server address".to_string());
    };
    let url = format!("http://127.0.0.1:{port}{AGY_DELETE_CASCADE_PATH}");
    let body = serde_json::json!({ "cascadeId": cascade_id }).to_string();
    let cfg = ureq::config::Config::builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = cfg.into();
    let mut req = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1");
    if let Some(token) = csrf_token.map(str::trim).filter(|t| !t.is_empty()) {
        req = req.header("x-codeium-csrf-token", token);
    }
    let resp = req.send(body.as_bytes()).map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    Ok(())
}

/// Best-effort `DeleteCascadeTrajectory` RPC for an Antigravity session during teardown, invoked
/// before routes are removed and before the runtime process is stopped. When the language server
/// is missing or unreachable, logs a diagnostic to stderr and continues so teardown never aborts
/// or fails silently.
fn delete_agy_cascade_trajectories(
    home_override: Option<&Path>,
    record: &HostedSessionRecord,
    target: &SessionTarget,
    tombstone_ids: &[String],
) {
    let mut route_ls_addr: Option<String> = None;
    let mut route_csrf: Option<String> = None;
    let mut cascade_ids: Vec<String> = Vec::new();
    let mut found_agy_route = false;

    for path in route_table_paths(home_override) {
        if !path.is_file() {
            continue;
        }
        let table = RouteTable::load(Some(&path));
        let Some(VendorRoute::Agy {
            conversation_id,
            ls_address,
            csrf_token,
            ..
        }) = route_for_teardown(&table, record, target)
        else {
            continue;
        };
        found_agy_route = true;
        push_tombstone(&mut cascade_ids, &conversation_id);
        if route_ls_addr.is_none()
            && let Some(addr) = ls_address
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
        {
            route_ls_addr = Some(addr);
            route_csrf = csrf_token
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty());
        }
    }

    let is_agy = found_agy_route || record.vendor.eq_ignore_ascii_case("antigravity");
    if !is_agy {
        return;
    }
    for id in tombstone_ids {
        push_tombstone(&mut cascade_ids, id);
    }
    if cascade_ids.is_empty() {
        return;
    }

    let (ls_addr, csrf) = match route_ls_addr {
        Some(addr) => (Some(addr), route_csrf),
        None if crate::discovery::agy_env_fallback_allowed(home_override) => {
            let env_addr = std::env::var("ANTIGRAVITY_LS_ADDRESS")
                .ok()
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty());
            let env_csrf = env_addr.as_ref().and_then(|_| {
                std::env::var("ANTIGRAVITY_CSRF_TOKEN")
                    .ok()
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
            });
            (env_addr, env_csrf)
        }
        None => (None, None),
    };

    let Some(addr) = ls_addr else {
        eprintln!(
            "fl-hub: Antigravity DeleteCascadeTrajectory skipped for session `{}` (cascades {:?}): \
             no language-server address recorded on route or environment; continuing teardown",
            record.session_id, cascade_ids
        );
        return;
    };

    let deadline = std::time::Instant::now() + AGY_DELETE_CASCADE_DEADLINE;
    for cascade_id in &cascade_ids {
        let Some(remaining) = deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|d| !d.is_zero())
        else {
            eprintln!(
                "fl-hub: Antigravity DeleteCascadeTrajectory deadline elapsed for session `{}` \
                 (skipping cascade `{cascade_id}` at `{addr}`); continuing teardown",
                record.session_id
            );
            break;
        };
        let timeout = remaining.min(AGY_DELETE_CASCADE_TIMEOUT);
        if let Err(reason) = post_agy_delete_cascade_trajectory_with_timeout(
            &addr,
            csrf.as_deref(),
            cascade_id,
            timeout,
        ) {
            eprintln!(
                "fl-hub: Antigravity DeleteCascadeTrajectory failed for session `{}` \
                 (cascade `{cascade_id}` at `{addr}`): {reason}; continuing teardown",
                record.session_id
            );
            if !reason.starts_with("HTTP ") {
                break;
            }
        }
    }
}

/// Native session ids discovery should treat as gone. Loaded from the route
/// files before they are pruned. A persona-derived id is not recorded, even
/// when the record or the target still stores one.
fn teardown_route_ids(
    home_override: Option<&Path>,
    record: &HostedSessionRecord,
    target: &SessionTarget,
) -> Vec<String> {
    let mut ids = Vec::new();
    for id in [
        record.session_id.as_str(),
        target.session_id.as_str(),
        target.raw_id.as_str(),
        target.discovered_id.as_str(),
    ] {
        push_tombstone(&mut ids, id);
    }
    if let Some(sid) = record.vendor_session_id.as_deref() {
        push_tombstone(&mut ids, sid);
    }
    for path in route_table_paths(home_override) {
        if !path.is_file() {
            continue;
        }
        let table = RouteTable::load(Some(&path));
        // The persona key may be a sibling vendor's route. Only this vendor's route,
        // or a route that names this session, contributes a native id.
        let Some(route) = route_for_teardown(&table, record, target) else {
            continue;
        };
        if let Some((_, id)) = route.session_identity() {
            push_tombstone(&mut ids, id);
        }
    }
    ids
}

fn route_for_teardown(
    table: &RouteTable,
    record: &HostedSessionRecord,
    target: &SessionTarget,
) -> Option<VendorRoute> {
    if let Some(entry) = table.get(&record.persona)
        && (record.vendor == "unknown" || route_is_vendor(&entry.route, &record.vendor))
    {
        return Some(entry.route.clone());
    }
    table
        .entries
        .values()
        .find(|entry| {
            [&record.session_id, &target.raw_id, &target.discovered_id]
                .into_iter()
                .any(|id| route_matches_session_id(&entry.route, id))
        })
        .map(|entry| entry.route.clone())
}

fn infer_vendor_from_routes(
    home_override: Option<&Path>,
    record: &HostedSessionRecord,
    target: &SessionTarget,
) -> Option<String> {
    for path in route_table_paths(home_override) {
        if !path.is_file() {
            continue;
        }
        let table = RouteTable::load(Some(&path));
        if let Some(route) = route_for_teardown(&table, record, target)
            && let Some(vendor) = vendor_id_of_route(&route)
        {
            return Some(vendor.to_string());
        }
    }
    None
}

/// Native session ids of a decommissioned record. Persona-derived ids stored on
/// an old record are left out, so a stale `vendor:persona` tombstone does not
/// keep banning the name.
pub(crate) fn record_native_ids(record: &HostedSessionRecord) -> Vec<String> {
    if !record.status.eq_ignore_ascii_case(STATUS_DECOMMISSIONED) {
        return Vec::new();
    }
    let mut ids = Vec::new();
    for id in std::iter::once(record.session_id.as_str())
        .chain(record.vendor_session_id.as_deref())
        .chain(record.tombstone_ids.iter().map(String::as_str))
    {
        push_tombstone(&mut ids, id);
    }
    ids
}

/// Rewrite the decommission sidecar from records that are still decommissioned
/// and agents that are currently retired. A re-spin overwrites the same slug
/// with a running record and re-activates the agent, so both leave the file.
/// Only native session ids are written to `session_ids`.
pub(crate) fn refresh_decommission_tombstones<S: AgentRegistry + StateStore>(
    store: &S,
    home_override: Option<&Path>,
) {
    refresh_decommission_tombstones_dropping(store, home_override, &[]);
}

pub(crate) fn refresh_decommission_tombstones_dropping<S: AgentRegistry + StateStore>(
    store: &S,
    home_override: Option<&Path>,
    dropped_ids: &[String],
) {
    let Ok(records) = list_hosted_sessions(store) else {
        return;
    };
    let mut ids: BTreeSet<String> =
        fl_wire::identity::load_decommissioned_session_ids(home_override)
            .into_iter()
            .filter(|id| is_native_session_id(id))
            .collect();
    for id in dropped_ids {
        ids.remove(id);
    }
    for record in &records {
        if !record.status.eq_ignore_ascii_case(STATUS_DECOMMISSIONED) {
            ids.remove(&record.session_id);
            if let Some(sid) = record.vendor_session_id.as_deref() {
                ids.remove(sid);
            }
        }
    }
    for record in records {
        ids.extend(record_native_ids(&record));
    }
    let mut active_names = BTreeSet::new();
    let mut retired_candidates = BTreeSet::new();
    for agent in store.list_agents(None, true).unwrap_or_default() {
        let key = agent.name_key.to_ascii_lowercase();
        match agent.status {
            AgentStatus::Active => {
                active_names.insert(key);
            }
            AgentStatus::Retired => {
                retired_candidates.insert(key);
            }
        }
    }
    let retired_personas: BTreeSet<String> = retired_candidates
        .difference(&active_names)
        .cloned()
        .collect();
    let _ =
        fl_wire::identity::save_decommissioned_tombstones(home_override, &ids, &retired_personas);
}

type DeadStreakKey = (Option<PathBuf>, String);

fn dead_session_streaks() -> &'static std::sync::Mutex<BTreeMap<DeadStreakKey, (u32, u64)>> {
    static STREAKS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<DeadStreakKey, (u32, u64)>>> =
        std::sync::OnceLock::new();
    STREAKS.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

fn with_dead_streaks<T>(f: impl FnOnce(&mut BTreeMap<DeadStreakKey, (u32, u64)>) -> T) -> T {
    let mut guard = dead_session_streaks()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    f(&mut guard)
}

fn bump_dead_streak(streak_key: &DeadStreakKey) -> u32 {
    let now = current_respawn_clock_ms();
    let min_spacing = dead_streak_min_spacing_ms();
    with_dead_streaks(|streaks| match streaks.get_mut(streak_key) {
        None => {
            streaks.insert(streak_key.clone(), (1, now));
            1
        }
        Some((count, last_ms)) => {
            if *count == 0 || now.saturating_sub(*last_ms) > DEAD_STREAK_MAX_AGE_MS {
                *count = 1;
                *last_ms = now;
                1
            } else if now.saturating_sub(*last_ms) < min_spacing {
                *count
            } else {
                *count = count.saturating_add(1);
                *last_ms = now;
                *count
            }
        }
    })
}

/// Mark a hosted runtime `stopped` when its tmux window is confirmed gone twice
/// and no route of its vendor is present.
///
/// Call sites: [`prepare_decommission`] (after the owner gate and the decommissioned
/// 409, before the target is rewritten to `decommissioning`) and `GET /api/v1/sessions`
/// (`handle_get_sessions`, before the discovery snapshot). Feira's boot reconcile
/// (Record #129) calls this the same way: `reconcile_dead_hosted_sessions(store, env, by)?`.
///
/// A hosted record is considered only when its status is `running` and
/// `vendor_has_runtime` is true. HTTP API vendors are never marked `stopped` for
/// lack of tmux.
///
/// The route is present when the route table has a route for that persona whose
/// vendor is this record's vendor. A present route resets the streak.
///
/// Runtime presence is three-way ([`RuntimePresence`]). `Unknown` means tmux failed
/// to run, timed out, or did not answer, and does not change the streak.
/// `exists() == false` is not proof of death.
///
/// Confirmed gone means tmux answered successfully and the pane or process is gone:
/// the window is absent, or `list-panes` on the exact target shows the pane dead
/// and the process gone. A `pane_dead` flag with a live process is not gone. A
/// `list-panes` failure is `Unknown`. The target checked is `record.tmux_target`
/// when set, otherwise the hosted window (`Ada:grok`), not the stripped session.
///
/// The streak is process-local, not a record field. The first consecutive
/// observation of gone with no route increments the streak and leaves status
/// `running`. The second (spaced at least [`DEAD_STREAK_MIN_SPACING_MS`] after the
/// first in production) sets status to [`STATUS_STOPPED`] (`"stopped"`) and clears
/// the streak. Alive, or a route of this vendor, resets the streak to 0. A hub
/// restart resets the streak. When `spawn_runtime` is false the runtime is not
/// called and the streak is left as it is.
///
/// This does not retire the agent, write a tombstone, kill tmux, or delete files.
/// [`prepare_decommission`] still continues after this and may set the target to
/// `decommissioning`. Other running records are updated too. A stopped record is
/// not decommissioned, so it does not become unmanaged.
pub fn reconcile_dead_hosted_sessions<S: StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    by: &str,
) -> Result<(), LifecycleError> {
    let records = list_hosted_sessions(store)?;
    for record in records {
        if !record.status.eq_ignore_ascii_case(STATUS_RUNNING)
            || !vendor_has_runtime(&record.vendor)
        {
            continue;
        }
        let session_mutex = hosted_session_lifecycle_mutex(env, &record.session_id);
        let _session_lifecycle_guard = match session_mutex.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                continue;
            }
        };
        let mut record = match get_hosted_session_by_id(store, &record.session_id) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(err) => {
                eprintln!(
                    "hub: warning ({} / {}): failed to load hosted session during dead-session reconcile: {err}",
                    record.persona, record.session_id
                );
                continue;
            }
        };
        if !record.status.eq_ignore_ascii_case(STATUS_RUNNING)
            || !vendor_has_runtime(&record.vendor)
        {
            continue;
        }
        if record.vendor == "claude" {
            let mut rstate = load_claude_resume_state(store, &record.session_id);
            if claude_survival_window_active(env, &rstate, current_respawn_clock_ms()) {
                continue;
            }
            if rstate.in_survival_window {
                rstate.in_survival_window = false;
                let _ = save_claude_resume_state(store, &record.session_id, &rstate, by);
            }
        }
        let streak_key = (
            env.home_override.map(Path::to_path_buf),
            record.session_id.clone(),
        );
        if vendor_route_live_for_record(env.home_override, &record) {
            if record.vendor == "claude" {
                let mut rstate = load_claude_resume_state(store, &record.session_id);
                let launch_started_ms =
                    rstate.resume_launched_at_ms.unwrap_or(record.updated_at_ms);
                if let Some(ref unconfirmed_sid) = rstate.resume_unconfirmed_session_id
                    && claude_route_confirmed_for_session(
                        env,
                        &record.persona,
                        &record.session_id,
                        unconfirmed_sid,
                        record.hydration_generation.as_deref(),
                        record.pid,
                        launch_started_ms,
                    )
                {
                    rstate.resume_unconfirmed_session_id = None;
                    rstate.resume_launched_at_ms = None;
                    let _ = save_claude_resume_state(store, &record.session_id, &rstate, by);
                }
            }
            with_dead_streaks(|streaks| {
                streaks.remove(&streak_key);
            });
            continue;
        }
        if !env.spawn_runtime {
            continue;
        }
        let target = record
            .tmux_target
            .clone()
            .unwrap_or_else(|| hosted_tmux_target(&record.vendor, &record.persona));
        match env.runtime.presence(&target) {
            RuntimePresence::Alive => {
                with_dead_streaks(|streaks| {
                    streaks.remove(&streak_key);
                });
            }
            RuntimePresence::Unknown => {}
            RuntimePresence::Gone => {
                let streak = bump_dead_streak(&streak_key);
                if streak >= 2 {
                    if record.vendor == "claude" {
                        let mut rstate = load_claude_resume_state(store, &record.session_id);
                        rstate.in_survival_window = false;
                        let launch_started_ms =
                            rstate.resume_launched_at_ms.unwrap_or(record.updated_at_ms);
                        if let Some(unconfirmed_sid) = rstate.resume_unconfirmed_session_id.clone()
                            && !claude_route_confirmed_for_session(
                                env,
                                &record.persona,
                                &record.session_id,
                                &unconfirmed_sid,
                                record.hydration_generation.as_deref(),
                                record.pid,
                                launch_started_ms,
                            )
                        {
                            rstate.resume_failed = true;
                            rstate.resume_unconfirmed_session_id = None;
                            rstate.resume_launched_at_ms = None;
                            let _ =
                                save_claude_resume_state(store, &record.session_id, &rstate, by);
                        } else {
                            if rstate.resume_unconfirmed_session_id.is_some()
                                || rstate.resume_launched_at_ms.is_some()
                            {
                                rstate.resume_unconfirmed_session_id = None;
                                rstate.resume_launched_at_ms = None;
                                let _ = save_claude_resume_state(
                                    store,
                                    &record.session_id,
                                    &rstate,
                                    by,
                                );
                            }
                            let _ =
                                sync_claude_vendor_session_id_from_route(store, env, &mut record);
                        }
                    }
                    let clean_exit = env
                        .runtime
                        .recorded_exit_code(&target, record.runtime_leader_pid.or(record.pid))
                        == Some(0);
                    record.status = STATUS_STOPPED.to_string();
                    record.stop_reason = Some(
                        if clean_exit {
                            STOP_REASON_CLEAN_EXIT
                        } else {
                            STOP_REASON_RUNTIME_GONE
                        }
                        .to_string(),
                    );
                    record.updated_at_ms = now_ms();
                    if let Err(err) = persist_hosted_session(store, &record, by) {
                        eprintln!(
                            "hub: warning ({} / {}): failed to persist stopped session during dead-session reconcile: {err}",
                            record.persona, record.session_id
                        );
                        continue;
                    }
                    with_dead_streaks(|streaks| {
                        streaks.remove(&streak_key);
                    });
                }
            }
        }
    }
    Ok(())
}

/// `true` when the route table has a route for `record.persona` on `record.vendor` whose
/// underlying process is proven live (`route_entry_proven_live` confirms a live PID + matching
/// start time).
fn vendor_route_live_for_record(
    home_override: Option<&Path>,
    record: &HostedSessionRecord,
) -> bool {
    route_table_paths(home_override).into_iter().any(|path| {
        path.is_file()
            && RouteTable::load(Some(&path))
                .get(&record.persona)
                .is_some_and(|entry| {
                    route_is_vendor(&entry.route, &record.vendor)
                        && fl_wire::identity::route_entry_proven_live(
                            entry,
                            record.pid,
                            record.pid_start_time,
                        )
                })
    })
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

/// Resolve and preflight exactly the runtime teardown would stop, without changing state.
/// Remodel also calls this during planning, before writing its handoff or entering rollback.
struct TeardownRuntimePlan {
    record: HostedSessionRecord,
    same_vendor_siblings: Vec<HostedSessionRecord>,
    runtime_owner_id: String,
    sharing: Vec<HostedSessionRecord>,
    shared_runtime: bool,
    persona_still_running: bool,
}

fn preflight_teardown_runtime<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    caller: Caller,
) -> Result<TeardownRuntimePlan, LifecycleError> {
    let mut record = record_for_target(store, target)?;
    if record.vendor == "unknown"
        && let Some(vendor) = infer_vendor_from_routes(env.home_override, &record, target)
            .or_else(|| infer_vendor_from_hosted_siblings(store, &record))
    {
        record.vendor = vendor;
    }
    let all_hosted = list_hosted_sessions(store)?;
    let same_vendor_siblings: Vec<HostedSessionRecord> = all_hosted
        .iter()
        .filter(|other| is_same_vendor_sibling(&record, other))
        .cloned()
        .collect();
    let mut runtime_owner_id = record.session_id.clone();
    if (record.tmux_target.is_none() || !record.runtime_spawned)
        && let Some(sib) = same_vendor_siblings
            .iter()
            .find(|s| !is_decommissioned(s) && (s.tmux_target.is_some() || s.runtime_spawned))
    {
        if record.tmux_target.is_none() {
            record.tmux_target = sib.tmux_target.clone();
            record.host_session_id = host_session_id_for_target(record.tmux_target.as_deref());
        }
        if !record.runtime_spawned && sib.runtime_spawned {
            record.runtime_spawned = true;
            runtime_owner_id = sib.session_id.clone();
        }
    }
    let sharing = runtime_sharers(store, env.runtime, &record)?;
    let shared_runtime = record.runtime_spawned && !sharing.is_empty();
    // Another running session of this persona (a second vendor) still owns the registry
    // record, the persona route, and the persona-keyed queue. One lookup covers the route
    // and queue decision and the retire guard below.
    let persona_still_running = all_hosted.iter().any(|other| {
        other.session_id != record.session_id
            && other.persona.eq_ignore_ascii_case(&target.persona)
            && other.status.eq_ignore_ascii_case(STATUS_RUNNING)
            && !is_same_vendor_sibling(&record, other)
    });
    if env.spawn_runtime && !shared_runtime {
        if record.runtime_spawned {
            if let Some(target) = &record.tmux_target {
                env.runtime
                    .preflight_stop(env.runtime.unit(target), &runtime_owner_id, true)?;
            }
        } else if caller == Caller::Owner
            && !persona_still_running
            && vendor_has_runtime(&record.vendor)
        {
            let target = record
                .tmux_target
                .clone()
                .unwrap_or_else(|| hosted_tmux_target(&record.vendor, &record.persona));
            env.runtime
                .preflight_stop(env.runtime.unit(&target), &runtime_owner_id, false)?;
        }
    }
    Ok(TeardownRuntimePlan {
        record,
        same_vendor_siblings,
        runtime_owner_id,
        sharing,
        shared_runtime,
        persona_still_running,
    })
}

/// Phase two: drop queued prompts and routes, stop the runtime this lifecycle started, retire
/// the agent, and mark the record `decommissioned`. Safe to repeat.
///
/// `caller` gates adopting a session that has no hosted record (owner only). `clear_queue` is
/// called with each id the session may be queued under. Another running session of the same
/// persona keeps the persona key and the persona route; the session ids are still cleared.
/// Everything that can fail on a lookup runs before the first side effect.
pub fn teardown_session<S: AgentRegistry + StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    by: &str,
    caller: Caller,
    mut clear_queue: impl FnMut(&str),
) -> Result<DecommissionResponse, LifecycleError> {
    authorize_teardown(store, target, caller)?;
    let TeardownRuntimePlan {
        mut record,
        same_vendor_siblings,
        runtime_owner_id,
        sharing,
        shared_runtime,
        persona_still_running,
    } = preflight_teardown_runtime(store, env, target, caller)?;
    let agent = store.agent_by_name(&target.persona)?;
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
        if persona_still_running && key.eq_ignore_ascii_case(&target.persona) {
            continue;
        }
        clear_queue(key);
    }
    let session_ids = [
        target.session_id.as_str(),
        target.raw_id.as_str(),
        target.discovered_id.as_str(),
    ];
    // Capture native ids before the routes are removed. Discovery uses them to skip a
    // conversation the hosted `vendor:persona` id does not name.
    let mut tombstone_ids = teardown_route_ids(env.home_override, &record, target);
    for sib in &same_vendor_siblings {
        push_tombstone(&mut tombstone_ids, &sib.session_id);
        if let Some(sid) = sib.vendor_session_id.as_deref() {
            push_tombstone(&mut tombstone_ids, sid);
        }
        for tid in &sib.tombstone_ids {
            push_tombstone(&mut tombstone_ids, tid);
        }
    }
    delete_agy_cascade_trajectories(env.home_override, &record, target, &tombstone_ids);
    if (env.spawn_runtime || env.home_override.is_some())
        && let Some(adapter) = env.host_vendor_adapter(&record.vendor)
    {
        let home_dir = env
            .home_override
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        let host_session = record.tmux_target.as_deref().unwrap_or(&record.session_id);
        let binding =
            fl_core::AdapterIdentityBinding::new(record.session_id.clone(), 1, host_session, None);
        for tid in &tombstone_ids {
            let _ = adapter.archive(&fl_core::TeardownTarget {
                binding: binding.clone(),
                vendor_session_id: Some(tid.clone()),
                control_endpoint: None,
                home_dir: home_dir.clone(),
            });
        }
    }
    if persona_still_running {
        remove_routes(env.home_override, &[], &session_ids);
    } else {
        remove_routes(env.home_override, &[target.persona.as_str()], &session_ids);
    }

    // `true` only for a runtime this call stopped. A runtime still shared, or none at all
    // (runtime calls are off), is not reported as stopped. A runtime this lifecycle started
    // is stopped with `kill` (a Muse window only when it carries our mark), then pruned as
    // `runtime_unit` (the session, or `muse:<persona>`). An owner teardown of a session this
    // lifecycle never recorded a target for stops that vendor's window (`Ada:grok`), and the
    // session only when the window is the session's only one, even though `runtime_spawned`
    // is false, including an unmarked Muse window. That path prunes the same
    // `session:window` string `kill_owner` received, so a sibling window keeps the session
    // in the resurrect save. A recorded target with a stale cleared flag is stopped only
    // when this runtime proves ownership; foreign or unverifiable units stay up.
    // An HTTP API vendor has no tmux unit, so its teardown never stops one.
    let mut restore_source = None;
    let runtime_stopped = if env.spawn_runtime && record.runtime_spawned {
        match &record.tmux_target {
            Some(tmux_target) => {
                let unit = env.runtime.unit(tmux_target);
                let gone = env.runtime.kill(unit, &runtime_owner_id);
                restore_source = Some(prune_unit_after_stop(env, unit));
                record.runtime_spawned &= !gone;
                gone && !(is_muse_window_target(unit) && env.runtime.exists(unit))
            }
            None => false,
        }
    } else if env.spawn_runtime
        && caller == Caller::Owner
        && !shared_runtime
        && !persona_still_running
        && record.tmux_target.is_none()
        && vendor_has_runtime(&record.vendor)
    {
        let target_name = hosted_tmux_target(&record.vendor, &record.persona);
        let gone = env.runtime.kill_owner(&target_name, &runtime_owner_id);
        restore_source = Some(prune_unit_after_stop(env, &target_name));
        gone && !(is_muse_window_target(&target_name) && env.runtime.exists(&target_name))
    } else if env.spawn_runtime
        && !record.runtime_spawned
        && (record.tmux_target.is_some() || vendor_has_runtime(&record.vendor))
    {
        let target = record
            .tmux_target
            .clone()
            .unwrap_or_else(|| hosted_tmux_target(&record.vendor, &record.persona));
        // A stale flag is not proof that the native unit is foreign. Require ownership
        // of the recorded target before using the owner cleanup path; this also keeps
        // legacy tmux's unmarked owner-adoption behavior out of this branch.
        let stopped = caller == Caller::Owner
            && !shared_runtime
            && !persona_still_running
            && env.runtime.owns(&target, &runtime_owner_id)
            && env.runtime.kill_owner(&target, &runtime_owner_id);
        restore_source = Some(prune_unit_after_stop(env, env.runtime.unit(&target)));
        stopped && !env.runtime.exists(&target)
    } else {
        false
    };
    // Its routes are gone and its runtime may be: the next discovery must scan the host again.
    crate::discovery::invalidate_route_discovery_cache();

    if let Some(agent) = agent
        && agent.status == AgentStatus::Active
        && !persona_still_running
    {
        store.retire_agent(&agent.id, stamp::now(), by)?;
    }

    let updated_ms = now_ms();
    record.status = STATUS_DECOMMISSIONED.to_string();
    record.stop_reason = None;
    record.hydration_generation = None;
    record.tombstone_ids = tombstone_ids;
    record.updated_at_ms = updated_ms;
    persist_hosted_session(store, &record, by)?;
    delete_claude_resume_state(store, &record.session_id);
    delete_hosted_respawn_state(store, &record.session_id);
    for mut sib in same_vendor_siblings {
        if !is_decommissioned(&sib) {
            sib.status = STATUS_DECOMMISSIONED.to_string();
            sib.stop_reason = None;
            sib.hydration_generation = None;
            sib.runtime_spawned = false;
            for tid in &record.tombstone_ids {
                push_tombstone(&mut sib.tombstone_ids, tid);
            }
            sib.updated_at_ms = updated_ms;
            persist_hosted_session(store, &sib, by)?;
        }
        delete_claude_resume_state(store, &sib.session_id);
        delete_hosted_respawn_state(store, &sib.session_id);
    }
    // The native conversation id is tombstoned above. Conversation files stay.
    refresh_decommission_tombstones(store, env.home_override);

    Ok(DecommissionResponse {
        session_id: record.session_id,
        persona: Some(target.persona.clone()),
        mode: DecommissionMode::Teardown,
        status: STATUS_DECOMMISSIONED.to_string(),
        handshake_dispatched: false,
        unregistered: true,
        runtime_stopped,
        restore_source,
        prompt: None,
    })
}

/// [`prune_after_stop`] for a runtime unit ([`runtime_unit`]): a window unit (`muse:<persona>`)
/// is `session:window`, a session unit has no window.
fn prune_unit_after_stop(
    env: &LifecycleEnv<'_>,
    unit: &str,
) -> crate::restore_source::RestoreSourceOutcome {
    match unit.split_once(':') {
        Some((session, window)) => prune_after_stop(env, session, Some(window)),
        None => prune_after_stop(env, unit, None),
    }
}

/// Prune what a teardown stopped from the restore source. The one call a kill path makes after
/// it has stopped (or found stopped) the tmux runtime `session`, or its window `window`:
///
/// - the session is gone: the whole session is pruned;
/// - the session runs and `window` is given and gone: only that window is pruned (the session's
///   other windows stay in the save);
/// - the session or the window still runs: nothing is pruned, because the next resurrect save
///   would write it back; the result is [`RestoreSourceOutcome::Running`].
///
/// [`RestoreSourceOutcome::Running`]: crate::restore_source::RestoreSourceOutcome::Running
pub(crate) fn prune_after_stop(
    env: &LifecycleEnv<'_>,
    session: &str,
    window: Option<&str>,
) -> crate::restore_source::RestoreSourceOutcome {
    use crate::restore_source::{RestoreSourceOutcome, prune_session, prune_window};
    if !env.runtime.exists(session) {
        return prune_session(env.home_override, session);
    }
    match window {
        Some(window) if !env.runtime.exists(&format!("{session}:{window}")) => {
            prune_window(env.home_override, session, window)
        }
        _ => RestoreSourceOutcome::Running,
    }
}

/// `POST /api/v1/hosted-sessions/:id/restart` response: the updated record plus the reforge prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartResponse {
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(flatten)]
    pub record: HostedSessionRecord,
    pub prompt: String,
}

/// Count a restart: reopen a missing tmux session, redeliver the reforge prompt, mark running.
/// A `decommissioned` session is refused (409); spin it up again instead.
pub fn restart_hosted_session<S: StateStore + AgentRegistry + Catalog + MessageStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    id: &str,
    by: &str,
) -> Result<RestartResponse, LifecycleError> {
    restart_hosted_session_with_store(store, || Ok(store), env, id, by)
}

/// Like [`restart_hosted_session`], but releases `store_guard` across [`resume_launch_survived`]
/// and re-opens it via `reopen_store` afterward so hub-wide store locks are never held during the
/// survival watch.
pub fn restart_hosted_session_with_store<S, G, F>(
    store_guard: G,
    mut reopen_store: F,
    env: &LifecycleEnv<'_>,
    id: &str,
    by: &str,
) -> Result<RestartResponse, LifecycleError>
where
    S: StateStore + AgentRegistry + Catalog + MessageStore,
    G: std::ops::Deref<Target = S>,
    F: FnMut() -> Result<G, LifecycleError>,
{
    let store: &S = &store_guard;
    let initial_record = find_hosted_session(store, &[id])?
        .ok_or_else(|| LifecycleError::NotFound(format!("no hosted session `{id}`")))?;
    let session_mutex = hosted_session_lifecycle_mutex(env, &initial_record.session_id);
    let _session_lifecycle_guard = match session_mutex.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            let sid = initial_record.session_id;
            drop(store_guard);
            let _waited = session_mutex.lock().unwrap_or_else(|e| e.into_inner());
            return Err(LifecycleError::Conflict(format!(
                "hosted session `{sid}` already had a lifecycle operation in progress"
            )));
        }
    };
    let mut record = get_hosted_session_by_id(store, &initial_record.session_id)?
        .ok_or_else(|| LifecycleError::NotFound(format!("no hosted session `{id}`")))?;
    if is_decommissioned(&record) {
        // Its agent is retired; restarting would mark it running with no agent behind it.
        return Err(decommissioned_conflict(&record.session_id));
    }
    if by == AUTO_RESPAWN_ACTOR {
        if !record.status.eq_ignore_ascii_case(STATUS_STOPPED)
            || record.stop_reason.as_deref() != Some(STOP_REASON_RUNTIME_GONE)
        {
            return Err(LifecycleError::Conflict(format!(
                "hosted session `{}` is no longer eligible for automatic respawn",
                record.session_id
            )));
        }
    } else {
        delete_hosted_respawn_state(store, &record.session_id);
    }
    let recast_assignment = current_recast_assignment(store, &record.persona)?;
    let recast_staged = recast_assignment.is_some();
    if let Some(agent) = recast_assignment {
        record.project = agent.project;
        record.role = role_label_of(&agent.role);
    }
    let config_home = preflight_vendor_launch(env, &record.vendor, record.hosted, &mut record.cwd)?;
    if record.hosted && vendor_has_runtime(&record.vendor) && env.spawn_runtime {
        env.runtime.preflight_spawn(&record.session_id)?;
    }
    fl_wire::vendor_adapter::launch_permissions_with_auto_approval(
        &record.vendor,
        record.posture,
        env.auto_approval_for(&record.vendor),
    )
    .map_err(|why| invalid_field("posture", &why))?;
    let (rt_session, rt_window) = runtime_names(&record.vendor, &record.persona);
    let want_target = hosted_tmux_target(&record.vendor, &record.persona);
    let unit = env.runtime.unit(&want_target).to_string();
    let _ = sync_claude_vendor_session_id_from_route(store, env, &mut record);
    // A stale `<Display>:muse` record whose old session still runs: that one Muse is the agent.
    // No second Muse is opened beside it; the prompt goes to it by delivery and the ownership
    // flag stays as recorded.
    let mut adopted_old_session = false;
    if record.hosted
        && record.vendor == "muse"
        && env.spawn_runtime
        && record.tmux_target.as_deref() != Some(want_target.as_str())
    {
        if record
            .tmux_target
            .as_deref()
            .map(|t| env.runtime.unit(t))
            .is_some_and(|old| env.runtime.exists(old))
        {
            adopted_old_session = true;
        } else {
            // The window now under the shared name is not what that record created, so it does
            // not own it: a teardown must not kill it unless this restart opens it.
            record.tmux_target = Some(want_target.clone());
            record.host_session_id = host_session_id_for_target(record.tmux_target.as_deref());
            record.runtime_spawned = false;
        }
    }
    record.host_session_id = host_session_id_for_target(record.tmux_target.as_deref());
    if record.hosted
        && !adopted_old_session
        && record.runtime_spawned
        && muse_window_not_ours(env, &record.vendor, &unit, &record.session_id)
    {
        // Our window (the one carrying our mark) is gone. A window of the name that exists after
        // this point is someone else's.
        record.runtime_spawned = false;
    }
    let existing_gen = record
        .hydration_generation
        .clone()
        .filter(|g| is_valid_hydration_generation(g));
    let should_check_unit = record.hosted
        && !adopted_old_session
        && vendor_has_runtime(&record.vendor)
        && env.spawn_runtime
        && (existing_gen.is_some() || record.vendor == "claude");
    let unit_not_running = should_check_unit && !env.runtime.exists(&unit);
    let will_spawn = record.hosted
        && !adopted_old_session
        && vendor_has_runtime(&record.vendor)
        && env.spawn_runtime
        && (existing_gen.is_none() || unit_not_running);

    let mut resume_decision: Option<ClaudeResumeDecision> = None;
    if record.vendor == "claude"
        && record.hosted
        && !adopted_old_session
        && vendor_has_runtime(&record.vendor)
        && env.spawn_runtime
        && unit_not_running
    {
        let model = record.model.clone();
        let thinking = record.thinking_level.clone();
        let ctx_win = record.context_window;
        let posture = record.posture;
        let cwd = record.cwd.clone();
        resume_decision = Some(evaluate_claude_resume_eligibility(
            store,
            env,
            &mut record,
            recast_staged,
            model.as_deref(),
            thinking.as_deref(),
            ctx_win,
            posture,
            cwd.as_deref(),
        )?);
    }

    let mut hydration_generation = match &resume_decision {
        Some(ClaudeResumeDecision::ResumeExisting {
            hydration_generation: g,
            ..
        }) => g.clone(),
        _ if will_spawn => next_hydration_generation(),
        _ => existing_gen
            .clone()
            .unwrap_or_else(next_hydration_generation),
    };
    let display = capitalize(&record.persona);
    let guidance = registered_role_prompt_with_generation(
        store,
        env,
        record.cwd.as_deref().map(Path::new),
        &display,
        &record.project,
        &agent_role_from_label(&record.role),
        Some(&hydration_generation),
    )?;
    if env.log_hydration_warnings {
        crate::hydration::log_warnings(&record.persona, &record.session_id, &guidance.warnings);
    }
    let mut warnings = guidance.warnings;
    let mut prompt = guidance.text;

    let mut is_claude_resume = false;
    let mut launch = match resume_decision.take() {
        Some(ClaudeResumeDecision::ResumeExisting {
            vendor_session_id: sid,
            launch: resume_launch,
            ..
        }) => {
            is_claude_resume = true;
            record.vendor_session_id = Some(sid);
            resume_launch
        }
        Some(ClaudeResumeDecision::FreshLaunch {
            reason_code,
            detail,
        }) => {
            push_claude_resume_fallback_notice(
                env,
                &record.persona,
                &mut warnings,
                reason_code,
                &detail,
            );
            if let Some(prev_g) = existing_gen.as_ref() {
                let _ = store.delete_state_by_key(
                    &StateScope::Workspace,
                    StateConcern::Runtime,
                    &hydration_ack_slug(prev_g),
                );
            }
            record.vendor_session_id = Some(uuid::Uuid::now_v7().to_string());
            launch_spec_for_with_session(
                env,
                &record.vendor,
                &record.persona,
                record.model.as_deref(),
                record.thinking_level.as_deref(),
                record.posture,
                &prompt,
                record.cwd.as_deref(),
                record.vendor_session_id.as_deref(),
                Some(&hydration_generation),
            )?
        }
        None => {
            if record.vendor == "claude"
                && record
                    .vendor_session_id
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
            {
                record.vendor_session_id = Some(uuid::Uuid::now_v7().to_string());
            }
            if record.hosted
                && !adopted_old_session
                && vendor_has_runtime(&record.vendor)
                && env.spawn_runtime
            {
                launch_spec_for_with_session(
                    env,
                    &record.vendor,
                    &record.persona,
                    record.model.as_deref(),
                    record.thinking_level.as_deref(),
                    record.posture,
                    &prompt,
                    record.cwd.as_deref(),
                    record.vendor_session_id.as_deref(),
                    Some(&hydration_generation),
                )?
            } else {
                None
            }
        }
    };
    pin_launch_config(&mut launch, config_home.as_deref());
    if !will_spawn && !is_claude_resume && existing_gen.is_some() {
        let _ = store.delete_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            &hydration_ack_slug(&hydration_generation),
        );
    }
    record.status = STATUS_RUNNING.to_string();
    record.stop_reason = None;
    record.decommission_prepared_at_ms = None;
    record.hydration_status = if is_claude_resume {
        HydrationStatus::Confirmed
    } else {
        HydrationStatus::Unconfirmed
    };
    if is_claude_resume {
        record.hydration_dispatched = true;
    }
    if will_spawn {
        record.pid = None;
        record.runtime_leader_pid = None;
        record.pid_start_time = None;
    }
    record.hydration_generation = Some(hydration_generation.clone());
    record.updated_at_ms = now_ms();
    persist_hosted_session(store, &record, by)?;
    with_dead_streaks(|streaks| {
        streaks.remove(&(
            env.home_override.map(Path::to_path_buf),
            record.session_id.clone(),
        ));
    });
    let mut launched_with_prompt = false;
    let mut launch_died = false;
    let mut resumed_existing = false;
    let resume_launch_started_ms = now_ms();
    let raw_spawned = record.hosted
        && !adopted_old_session
        && vendor_has_runtime(&record.vendor)
        && env.spawn_runtime
        && env.runtime.spawn(
            &rt_session,
            &rt_window,
            record.cwd.as_deref(),
            launch.as_ref(),
            &record.session_id,
        );
    let _reopened_guard;
    let store: &S = if is_claude_resume {
        let resumed_sid = record.vendor_session_id.clone().unwrap_or_default();
        if raw_spawned {
            record.runtime_spawned = true;
            let initial_pid = env.runtime.recorded_pid(&unit, &record.session_id);
            record.pid = initial_pid;
            record.pid_start_time = initial_pid.and_then(fl_wire::identity::proc_start_time_ticks);
            record.updated_at_ms = now_ms();
            persist_hosted_session(store, &record, by)?;
            let mut pre_rstate = load_claude_resume_state(store, &record.session_id);
            pre_rstate.resume_failed = false;
            pre_rstate.resume_unconfirmed_session_id = Some(resumed_sid.clone());
            pre_rstate.resume_launched_at_ms = Some(resume_launch_started_ms);
            pre_rstate.in_survival_window = true;
            save_claude_resume_state(store, &record.session_id, &pre_rstate, by)?;
        }
        drop(store_guard);
        let (survived_resume, hook_confirmed) = if raw_spawned {
            if launch.is_some() {
                resume_launch_survived(
                    env,
                    &unit,
                    &record.session_id,
                    &record.persona,
                    &resumed_sid,
                    &hydration_generation,
                    resume_launch_started_ms,
                )
            } else {
                (true, true)
            }
        } else {
            (false, false)
        };
        _reopened_guard = reopen_store()?;
        let store: &S = &_reopened_guard;
        let Some(fresh_record) = get_hosted_session_by_id(store, &record.session_id)? else {
            if raw_spawned {
                let _ = env.runtime.kill(&unit, &record.session_id);
            }
            delete_claude_resume_state(store, &record.session_id);
            return Err(LifecycleError::Conflict(format!(
                "hosted session `{}` was removed during launch survival watch",
                record.session_id
            )));
        };
        if is_decommissioned(&fresh_record) || fresh_record.status == STATUS_DECOMMISSIONING {
            if raw_spawned {
                let _ = env.runtime.kill(&unit, &record.session_id);
            }
            delete_claude_resume_state(store, &record.session_id);
            return Err(decommissioned_conflict(&record.session_id));
        }
        if fresh_record.hydration_generation.as_deref() != Some(hydration_generation.as_str()) {
            if raw_spawned {
                let _ = env.runtime.kill(&unit, &record.session_id);
            }
            let mut rstate = load_claude_resume_state(store, &record.session_id);
            if rstate.in_survival_window
                && rstate.resume_launched_at_ms == Some(resume_launch_started_ms)
            {
                rstate.in_survival_window = false;
                let _ = save_claude_resume_state(store, &record.session_id, &rstate, by);
            }
            return Err(LifecycleError::Conflict(format!(
                "hosted session `{}` launch generation changed during survival watch",
                record.session_id
            )));
        }
        record = fresh_record;
        if raw_spawned && survived_resume {
            record.runtime_spawned = true;
            resumed_existing = true;
            if let Some(new_pid) = env.runtime.recorded_pid(&unit, &record.session_id) {
                record.pid = Some(new_pid);
                record.pid_start_time = fl_wire::identity::proc_start_time_ticks(new_pid);
            }
            let mut rstate = load_claude_resume_state(store, &record.session_id);
            rstate.resume_failed = false;
            rstate.in_survival_window = false;
            rstate.resume_unconfirmed_session_id = if hook_confirmed {
                None
            } else {
                Some(resumed_sid)
            };
            rstate.resume_launched_at_ms = if hook_confirmed {
                None
            } else {
                Some(resume_launch_started_ms)
            };
            save_claude_resume_state(store, &record.session_id, &rstate, by)?;
        } else {
            let _ = env.runtime.kill(&unit, &record.session_id);
            let failed_sid = resumed_sid;
            push_tombstone(&mut record.tombstone_ids, &failed_sid);
            push_claude_resume_fallback_notice(
                env,
                &record.persona,
                &mut warnings,
                "resume_launch_did_not_survive",
                &format!("--resume `{failed_sid}` did not survive startup"),
            );
            let _ = store.delete_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                &hydration_ack_slug(&hydration_generation),
            );
            hydration_generation = next_hydration_generation();
            let fresh_sid = uuid::Uuid::now_v7().to_string();
            record.vendor_session_id = Some(fresh_sid);
            record.hydration_generation = Some(hydration_generation.clone());
            record.hydration_status = HydrationStatus::Unconfirmed;
            record.hydration_dispatched = false;
            record.runtime_spawned = false;
            record.pid = None;
            record.runtime_leader_pid = None;
            record.pid_start_time = None;
            record.updated_at_ms = now_ms();
            let fresh_guidance = registered_role_prompt_with_generation(
                store,
                env,
                record.cwd.as_deref().map(Path::new),
                &display,
                &record.project,
                &agent_role_from_label(&record.role),
                Some(&hydration_generation),
            )?;
            prompt = fresh_guidance.text;
            launch = launch_spec_for_with_session(
                env,
                &record.vendor,
                &record.persona,
                record.model.as_deref(),
                record.thinking_level.as_deref(),
                record.posture,
                &prompt,
                record.cwd.as_deref(),
                record.vendor_session_id.as_deref(),
                Some(&hydration_generation),
            )?;
            pin_launch_config(&mut launch, config_home.as_deref());
            persist_hosted_session(store, &record, by)?;
            let retry_raw = env.runtime.spawn(
                &rt_session,
                &rt_window,
                record.cwd.as_deref(),
                launch.as_ref(),
                &record.session_id,
            );
            if retry_raw {
                if launch.is_some() && !launch_survived(env, &unit, &record.session_id) {
                    launch_died = true;
                    record.runtime_spawned = false;
                } else {
                    record.runtime_spawned = true;
                    launched_with_prompt = launch.is_some();
                }
            } else {
                launch_died = true;
                record.runtime_spawned = false;
            }
            let new_pid = if record.runtime_spawned {
                env.runtime.recorded_pid(&unit, &record.session_id)
            } else {
                None
            };
            record.pid = new_pid;
            record.pid_start_time = new_pid.and_then(fl_wire::identity::proc_start_time_ticks);
            let mut rstate = load_claude_resume_state(store, &record.session_id);
            rstate.resume_failed = false;
            rstate.in_survival_window = false;
            rstate.resume_unconfirmed_session_id = None;
            rstate.resume_launched_at_ms = None;
            save_claude_resume_state(store, &record.session_id, &rstate, by)?;
        }
        store
    } else if raw_spawned {
        if launch.is_some() && !launch_survived(env, &unit, &record.session_id) {
            // The command exited at once and took its session with it: nothing to deliver to.
            launch_died = true;
            record.runtime_spawned = false;
        } else {
            record.runtime_spawned = true;
            // Argv already handed off the prompt; acceptance remains unconfirmed.
            launched_with_prompt = launch.is_some();
        }
        let new_pid = if record.runtime_spawned {
            env.runtime.recorded_pid(&unit, &record.session_id)
        } else {
            None
        };
        record.pid = new_pid;
        record.pid_start_time = new_pid.and_then(fl_wire::identity::proc_start_time_ticks);
        if record.vendor == "claude" {
            let mut rstate = load_claude_resume_state(store, &record.session_id);
            rstate.resume_failed = false;
            rstate.in_survival_window = false;
            rstate.resume_unconfirmed_session_id = None;
            rstate.resume_launched_at_ms = None;
            save_claude_resume_state(store, &record.session_id, &rstate, by)?;
        }
        store
    } else {
        if will_spawn && let Some(prev_g) = existing_gen.as_ref() {
            record.hydration_generation = Some(prev_g.clone());
            let _ = store.delete_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                &hydration_ack_slug(prev_g),
            );
        }
        store
    };
    let hydration = if resumed_existing {
        HydrationOutcome {
            dispatched: true,
            status: HydrationStatus::Confirmed,
        }
    } else if launch_died || launched_with_prompt {
        HydrationOutcome::from_launch(!launch_died)
    } else {
        deliver_prompt(env, &record.persona, &record.session_id, by, &prompt)
    };
    record.hydration_dispatched = hydration.dispatched;
    record.hydration_status = hydration.status;
    refresh_record_runtime_pid(env, &unit, &mut record);
    let _ = promote_confirmed_hydration(store, &mut record);
    // A runtime may have reopened: the next discovery must scan the host again.
    crate::discovery::invalidate_route_discovery_cache();
    record.restart_count += 1;
    record.status = STATUS_RUNNING.to_string();
    record.stop_reason = None;
    record.decommission_prepared_at_ms = None;
    record.updated_at_ms = now_ms();
    persist_hosted_session(store, &record, by)?;
    if resumed_existing {
        let baseline_ms = crate::boot_reconcile::hydration_baseline_ms(store, &record);
        if let Err(err) = crate::boot_reconcile::send_service_resume_notice_in_store(
            store,
            &record.persona,
            record.cwd.as_deref().map(Path::new),
            Some(&record.role),
            baseline_ms,
        ) {
            eprintln!(
                "hub: warning ({} / {}): failed to enqueue Host restarted notice on resume: {err}",
                record.persona, record.session_id
            );
        }
    }
    Ok(RestartResponse {
        record,
        prompt,
        warnings,
    })
}

const AUTO_RESPAWN_WARN_INTERVAL_MS: u64 = 5 * 60 * 1_000;

fn should_log_auto_respawn_warning(session_id: &str, now_ms: u64) -> bool {
    static LAST_WARN_MS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, u64>>> =
        std::sync::OnceLock::new();
    let map = LAST_WARN_MS.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&last) = guard.get(session_id)
        && now_ms.saturating_sub(last) < AUTO_RESPAWN_WARN_INTERVAL_MS
    {
        return false;
    }
    guard.insert(session_id.to_string(), now_ms);
    true
}

fn resolve_project_lead<S: AgentRegistry>(
    store: &S,
    project: &str,
) -> Result<Option<AgentRecord>, StoreError> {
    let project_key =
        validate_slug(project.trim()).unwrap_or_else(|_| project.trim().to_ascii_lowercase());
    let active_leads: Vec<AgentRecord> = store
        .list_agents(Some(&project_key), false)?
        .into_iter()
        .filter(|a| a.status == AgentStatus::Active && a.role == AgentRole::Lead)
        .collect();
    if let Some(own) = active_leads
        .iter()
        .find(|a| a.project.eq_ignore_ascii_case(&project_key))
    {
        return Ok(Some(own.clone()));
    }
    Ok(active_leads.into_iter().next())
}

fn try_notify_crash_loop_lead<S: AgentRegistry + MessageStore>(
    store: &S,
    fresh: &HostedSessionRecord,
) -> bool {
    let subject = format!(
        "Crash loop: {} stopped after {} automatic respawns",
        fresh.session_id, AUTO_RESPAWN_MAX_IN_WINDOW
    );
    let body = format!(
        "Hosted session `{}` (persona `{}`, vendor `{}`) stopped again after {} automatic respawns within a 10-minute window and has been left stopped (`stop_reason = \"crash_loop\"`). Inspect the failure and run manual restart once resolved.",
        fresh.session_id, fresh.persona, fresh.vendor, AUTO_RESPAWN_MAX_IN_WINDOW
    );
    match resolve_project_lead(store, &fresh.project) {
        Ok(Some(lead)) if !lead.name_key.eq_ignore_ascii_case(&fresh.persona) => {
            match store.send_wire(SendWireArgs {
                id: None,
                sender_id: None,
                sender_name: "system".to_string(),
                sender_host: None,
                to: WireAddress::AgentName(lead.name_key.clone()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject,
                body,
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: stamp::now(),
            }) {
                Ok(_) => true,
                Err(err) => {
                    if should_log_auto_respawn_warning(
                        &fresh.session_id,
                        current_respawn_clock_ms(),
                    ) {
                        eprintln!(
                            "hub: warning ({} / {}): failed to send crash-loop wire notice to `{}`: {err}",
                            fresh.persona, fresh.session_id, lead.name_key
                        );
                    }
                    false
                }
            }
        }
        Ok(Some(_lead)) => {
            // When the crash-looping session is itself the project Lead, do not send a
            // self-addressed wire notice to a stopped session; the `session.crash_loop`
            // FerroDeck WebSocket broadcast and hub warning log are the notice.
            true
        }
        Ok(None) => {
            // No active Lead is registered in this project; leave `crash_loop_notified`
            // false so the wire notice is delivered if a Lead is registered before
            // manual restart.
            false
        }
        Err(err) => {
            if should_log_auto_respawn_warning(&fresh.session_id, current_respawn_clock_ms()) {
                eprintln!(
                    "hub: warning ({} / {}): failed to resolve project Lead for crash-loop notice: {err}",
                    fresh.persona, fresh.session_id
                );
            }
            false
        }
    }
}

/// Reconcile dead hosted sessions and automatically respawn eligible `on-failure` sessions that
/// stopped due to [`STOP_REASON_RUNTIME_GONE`], enforcing exponential backoff (`30s`, `60s`, `120s`)
/// and a crash-loop guard (`3` automatic respawns in `10` minutes).
pub fn reconcile_and_respawn_hosted_sessions<S>(
    store: &S,
    env: &LifecycleEnv<'_>,
) -> Result<Vec<RestartResponse>, LifecycleError>
where
    S: StateStore + AgentRegistry + Catalog + MessageStore,
{
    reconcile_and_respawn_hosted_sessions_with_store(store, || Ok(store), env, |_, _| {})
}

pub fn reconcile_and_respawn_hosted_sessions_with_store<S, G, F>(
    store_guard: G,
    mut reopen_store: F,
    env: &LifecycleEnv<'_>,
    mut on_event: impl FnMut(&str, serde_json::Value),
) -> Result<Vec<RestartResponse>, LifecycleError>
where
    S: StateStore + AgentRegistry + Catalog + MessageStore,
    G: std::ops::Deref<Target = S>,
    F: FnMut() -> Result<G, LifecycleError>,
{
    reconcile_dead_hosted_sessions(&*store_guard, env, AUTO_RESPAWN_ACTOR)?;
    drop(store_guard);
    if !env.spawn_runtime {
        return Ok(Vec::new());
    }
    let candidates: Vec<HostedSessionRecord> = {
        let guard = reopen_store()?;
        list_hosted_sessions(&*guard)?
            .into_iter()
            .filter(|r| {
                r.hosted
                    && vendor_has_runtime(&r.vendor)
                    && r.status.eq_ignore_ascii_case(STATUS_STOPPED)
                    && r.restart_policy.eq_ignore_ascii_case("on-failure")
                    && matches!(
                        r.stop_reason.as_deref(),
                        Some(STOP_REASON_RUNTIME_GONE | STOP_REASON_CRASH_LOOP)
                    )
            })
            .collect()
    };
    let mut respawned = Vec::new();
    for record in candidates {
        let session_mutex = hosted_session_lifecycle_mutex(env, &record.session_id);
        match session_mutex.try_lock() {
            Ok(guard) => drop(guard),
            Err(std::sync::TryLockError::Poisoned(e)) => drop(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => continue,
        }
        let guard = match reopen_store() {
            Ok(g) => g,
            Err(err) => {
                eprintln!(
                    "hub: warning ({} / {}): failed to reopen store during auto-respawn: {err}",
                    record.persona, record.session_id
                );
                continue;
            }
        };
        let mut fresh = match get_hosted_session_by_id(&*guard, &record.session_id) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(err) => {
                eprintln!(
                    "hub: warning ({} / {}): failed to load hosted session during auto-respawn: {err}",
                    record.persona, record.session_id
                );
                continue;
            }
        };
        if !fresh.hosted
            || !vendor_has_runtime(&fresh.vendor)
            || !fresh.status.eq_ignore_ascii_case(STATUS_STOPPED)
            || !fresh.restart_policy.eq_ignore_ascii_case("on-failure")
            || !matches!(
                fresh.stop_reason.as_deref(),
                Some(STOP_REASON_RUNTIME_GONE | STOP_REASON_CRASH_LOOP)
            )
        {
            continue;
        }
        let mut rstate = load_hosted_respawn_state(&*guard, &record.session_id);
        if fresh.stop_reason.as_deref() == Some(STOP_REASON_CRASH_LOOP) {
            if !rstate.crash_loop_notified && try_notify_crash_loop_lead(&*guard, &fresh) {
                rstate.crash_loop_notified = true;
                if let Err(err) = save_hosted_respawn_state(
                    &*guard,
                    &fresh.session_id,
                    &rstate,
                    AUTO_RESPAWN_ACTOR,
                ) {
                    eprintln!(
                        "hub: warning ({} / {}): failed to save crash-loop respawn state: {err}",
                        fresh.persona, fresh.session_id
                    );
                }
            }
            continue;
        }
        let now = current_respawn_clock_ms();
        rstate
            .respawn_timestamps_ms
            .retain(|ts| now.saturating_sub(*ts) < AUTO_RESPAWN_WINDOW_MS);
        if rstate.respawn_timestamps_ms.len() >= AUTO_RESPAWN_MAX_IN_WINDOW {
            fresh.stop_reason = Some(STOP_REASON_CRASH_LOOP.to_string());
            fresh.updated_at_ms = now;
            if let Err(err) = persist_hosted_session(&*guard, &fresh, AUTO_RESPAWN_ACTOR) {
                eprintln!(
                    "hub: warning ({} / {}): failed to persist crash-loop session record: {err}",
                    fresh.persona, fresh.session_id
                );
                continue;
            }
            if !rstate.crash_loop_notified {
                rstate.crash_loop_notified = try_notify_crash_loop_lead(&*guard, &fresh);
            }
            if let Err(err) =
                save_hosted_respawn_state(&*guard, &fresh.session_id, &rstate, AUTO_RESPAWN_ACTOR)
            {
                eprintln!(
                    "hub: warning ({} / {}): failed to save crash-loop respawn state: {err}",
                    fresh.persona, fresh.session_id
                );
            }
            eprintln!(
                "hub: warning ({} / {}): crash-loop guard tripped after {} automatic respawns in 10 minutes; leaving session stopped",
                fresh.persona, fresh.session_id, AUTO_RESPAWN_MAX_IN_WINDOW
            );
            on_event(
                "session.crash_loop",
                serde_json::to_value(&fresh).unwrap_or_default(),
            );
            continue;
        }
        if let Some(next_ms) = rstate.next_allowed_at_ms
            && now < next_ms
        {
            if let Err(err) =
                save_hosted_respawn_state(&*guard, &record.session_id, &rstate, AUTO_RESPAWN_ACTOR)
            {
                eprintln!(
                    "hub: warning ({} / {}): failed to save respawn state: {err}",
                    record.persona, record.session_id
                );
            }
            continue;
        }
        let idx = rstate
            .respawn_timestamps_ms
            .len()
            .min(AUTO_RESPAWN_BACKOFF_SECS.len() - 1);
        let backoff_ms = AUTO_RESPAWN_BACKOFF_SECS[idx] * 1_000;
        let prev_next_allowed = rstate.next_allowed_at_ms;
        rstate.respawn_timestamps_ms.push(now);
        rstate.next_allowed_at_ms = Some(now.saturating_add(backoff_ms));
        if let Err(err) =
            save_hosted_respawn_state(&*guard, &record.session_id, &rstate, AUTO_RESPAWN_ACTOR)
        {
            eprintln!(
                "hub: warning ({} / {}): failed to save respawn state before restart: {err}",
                record.persona, record.session_id
            );
            continue;
        }
        match restart_hosted_session_with_store(
            guard,
            &mut reopen_store,
            env,
            &record.session_id,
            AUTO_RESPAWN_ACTOR,
        ) {
            Ok(out) => {
                // `restart_hosted_session_with_store` returns `Ok` even when `runtime.spawn()`
                // returns `false` (or the newly spawned process fails `launch_survived`). Counting
                // that attempt in `respawn_timestamps_ms` and emitting `session.restarted` is
                // intentional: a broken binary or failing runtime trips the 3-in-10m crash-loop
                // guard rather than retrying indefinitely.
                on_event(
                    "session.restarted",
                    serde_json::to_value(&out).unwrap_or_default(),
                );
                respawned.push(out);
            }
            Err(LifecycleError::Conflict(msg)) => {
                let mut now_running = false;
                if let Ok(rollback_store) = reopen_store() {
                    now_running = get_hosted_session_by_id(&*rollback_store, &record.session_id)
                        .ok()
                        .flatten()
                        .is_some_and(|r| r.status.eq_ignore_ascii_case(STATUS_RUNNING));
                    if now_running {
                        delete_hosted_respawn_state(&*rollback_store, &record.session_id);
                    } else {
                        let mut current_rstate =
                            load_hosted_respawn_state(&*rollback_store, &record.session_id);
                        if !current_rstate.respawn_timestamps_ms.is_empty() {
                            if let Some(pos) = current_rstate
                                .respawn_timestamps_ms
                                .iter()
                                .rposition(|&ts| ts == now)
                            {
                                current_rstate.respawn_timestamps_ms.remove(pos);
                            }
                            current_rstate.next_allowed_at_ms = prev_next_allowed;
                            let _ = save_hosted_respawn_state(
                                &*rollback_store,
                                &record.session_id,
                                &current_rstate,
                                AUTO_RESPAWN_ACTOR,
                            );
                        }
                    }
                }
                if !now_running && should_log_auto_respawn_warning(&record.session_id, now) {
                    eprintln!(
                        "hub: warning ({} / {}): auto-respawn failed: {msg}",
                        record.persona, record.session_id
                    );
                }
            }
            Err(err) => {
                if should_log_auto_respawn_warning(&record.session_id, now) {
                    eprintln!(
                        "hub: warning ({} / {}): auto-respawn failed: {err}",
                        record.persona, record.session_id
                    );
                }
            }
        }
    }
    Ok(respawned)
}

pub fn reconcile_and_respawn_hosted_sessions_in_hub(
    state: &crate::HubState,
) -> Result<Vec<RestartResponse>, LifecycleError> {
    let store_guard = state
        .open_store()
        .map_err(|e| LifecycleError::Store(e.to_string()))?;
    let env = crate::api::lifecycle_env(state);
    reconcile_and_respawn_hosted_sessions_with_store(
        store_guard,
        || {
            state
                .open_store()
                .map_err(|e| LifecycleError::Store(e.to_string()))
        },
        &env,
        |event, payload| {
            state.broadcast("sessions", event, payload);
        },
    )
}

fn refresh_record_runtime_pid(
    env: &LifecycleEnv<'_>,
    unit: &str,
    record: &mut HostedSessionRecord,
) {
    if !record.runtime_spawned {
        return;
    }
    refresh_owned_runtime_pid(env, unit, record);
}

fn refresh_owned_runtime_pid(env: &LifecycleEnv<'_>, unit: &str, record: &mut HostedSessionRecord) {
    if unit.is_empty() {
        return;
    }
    let leader_pid = env.runtime.recorded_leader_pid(unit, &record.session_id);
    let vendor_pid = leader_pid
        .and_then(fl_wire::identity::resolve_vendor_cli_pid)
        .or_else(|| env.runtime.recorded_pid(unit, &record.session_id));
    if let Some(new_pid) = vendor_pid.filter(|&p| p > 0) {
        if record.pid.is_none()
            || record.pid_start_time.is_none()
            || record.pid == Some(new_pid)
            || record.pid.is_some_and(|p| {
                fl_wire::identity::proc_alive_matching_start(p, record.pid_start_time)
                    && fl_wire::identity::proc_is_shell_wrapper(p)
                    && fl_wire::identity::resolve_vendor_cli_pid(p) == Some(new_pid)
            })
            || !record.pid.is_some_and(|p| {
                fl_wire::identity::proc_alive_matching_start(p, record.pid_start_time)
            })
        {
            if env.runtime.owns(unit, &record.session_id) {
                record.runtime_leader_pid = leader_pid;
                record.pid = Some(new_pid);
                record.pid_start_time = fl_wire::identity::proc_start_time_ticks(new_pid);
            }
        } else if record.pid_start_time.is_none()
            && let Some(rec_pid) = record.pid
        {
            record.pid_start_time = fl_wire::identity::proc_start_time_ticks(rec_pid);
        }
    }
}

/// Backfill or refresh `record.pid` and `record.pid_start_time` for running hosted sessions whose
/// recorded PID is missing, lacks `pid_start_time`, is a wrapper shell that now has a vendor CLI
/// child, or is not proven live while the runtime unit is live.
pub fn reconcile_running_session_pids_with_store<S: StateStore>(
    env: &LifecycleEnv<'_>,
    store: &S,
) -> Result<usize, LifecycleError> {
    let records = list_hosted_sessions(store)?;
    let mut updated = 0usize;
    for record in records {
        if !record.status.eq_ignore_ascii_case(STATUS_RUNNING) {
            continue;
        }
        let session_mutex = hosted_session_lifecycle_mutex(env, &record.session_id);
        let _session_lifecycle_guard = match session_mutex.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => continue,
        };
        let Some(mut record) = get_hosted_session_by_id(store, &record.session_id)? else {
            continue;
        };
        if !record.status.eq_ignore_ascii_case(STATUS_RUNNING) {
            continue;
        }
        let target = record
            .tmux_target
            .clone()
            .unwrap_or_else(|| hosted_tmux_target(&record.vendor, &record.persona));
        let unit = env.runtime.unit(&target).to_string();
        if env.spawn_runtime
            && vendor_has_runtime(&record.vendor)
            && !matches!(env.runtime.presence(&target), RuntimePresence::Gone)
        {
            let before = (record.pid, record.pid_start_time, record.runtime_leader_pid);
            refresh_owned_runtime_pid(env, &unit, &mut record);
            if before != (record.pid, record.pid_start_time, record.runtime_leader_pid) {
                record.updated_at_ms = now_ms();
                persist_hosted_session(store, &record, AUTO_RESPAWN_ACTOR)?;
                updated += 1;
            }
        }
    }
    Ok(updated)
}

pub fn reconcile_running_session_pids_in_hub(
    state: &crate::HubState,
) -> Result<usize, LifecycleError> {
    let store_guard = state
        .open_store()
        .map_err(|e| LifecycleError::Store(e.to_string()))?;
    let env = crate::api::lifecycle_env(state);
    reconcile_running_session_pids_with_store(&env, &*store_guard)
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
    /// Thinking level for the new session (see [`SpinUpRequest::thinking_level`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    /// `prepare` or `execute` (the default).
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub hosted: Option<bool>,
    /// Permission posture for the new session (`ask`, `accept-edits`, `auto`, `plan`, `bypass`;
    /// default `auto`).
    #[serde(default, deserialize_with = "deserialize_posture")]
    pub posture: fl_core::permissions::Posture,
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
    /// The thinking level the new session was given, when one was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    pub node_id: String,
    pub hosted: bool,
    pub cwd: Option<String>,
    /// `decommissioning` after `prepare`, `running` after `execute`.
    pub status: String,
    /// `prepare`: whether a live route took the handoff prompt.
    pub handshake_dispatched: bool,
    /// `execute`: whether a live route took the new session's reforge prompt.
    /// Whether the prompt was handed to the vendor; this is not acceptance evidence.
    pub hydration_dispatched: bool,
    #[serde(default)]
    pub hydration_status: HydrationStatus,
    /// Strictly monotonic launch generation token for fencing initial hydration acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydration_generation: Option<String>,
    /// `execute`: whether the old session's runtime is gone.
    pub runtime_stopped: bool,
    pub tmux_target: Option<String>,
    /// The `fl-host` session identifier for this runtime (`<Persona>.<vendor>`, or `muse.<persona>`),
    /// when the session has a runtime target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_session_id: Option<String>,
    /// `prepare`: the handoff prompt; `execute`: the new session's reforge prompt.
    pub prompt: Option<String>,
    /// `execute`: slug of the workspace handoff entry that records the remodel.
    pub handoff: Option<String>,
    /// Problems that did not stop the remodel.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Permission posture compiled into the new session's launch flags.
    #[serde(default)]
    pub posture: fl_core::permissions::Posture,
    /// Compiler receipts describing how `posture` mapped onto the new vendor's native flags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_receipts: Vec<fl_wire::vendor_adapter::PermissionReceipt>,
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
            thinking_level: self.to.thinking_level.clone(),
            node_id: Some(self.to.node_id.clone()),
            hosted: Some(self.to.hosted),
            cwd: self.to.cwd.clone(),
            // A window is chosen for one vendor and model: a remodel starts without one.
            context_window: None,
            posture: self.to.posture,
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

/// A hosted remodel starts its new runtime under the persona's tmux session name (the
/// capitalised persona; for Muse a window of the shared `muse` session), so the teardown must
/// stop the old one first. Refused (409) when a runtime unit that matters runs and the teardown
/// will not stop it: one this lifecycle did not start (no hosted record, or `runtime_spawned`
/// unset), or one other live hosted sessions share. The units are the old session's own (its
/// record's target, else the persona's session name) and the persona's session name, which a
/// new non-Muse runtime would take and a non-Muse old runtime may be in. Otherwise the new runtime could not start,
/// or the old vendor would keep running under the new registry. A Muse window already there is
/// never a reason to refuse by itself: the spin-up adopts it (see `require_runtime`).
fn check_old_runtime_stoppable<S: StateStore>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    to: &ValidSpinUp,
) -> Result<(), LifecycleError> {
    let name = capitalize(&target.persona);
    // An API vendor opens no tmux window, so the old name being taken costs it nothing.
    if !to.hosted || !vendor_has_runtime(&to.vendor) || !env.spawn_runtime {
        return Ok(());
    }
    let old_unit = target
        .hosted
        .as_ref()
        .and_then(|r| r.tmux_target.as_deref())
        .map_or(name.as_str(), |t| env.runtime.unit(t));
    let mut units = vec![old_unit];
    // Native vendors have separate units. A new non-Muse target must also be free;
    // legacy tmux maps this back to the same persona session as before.
    let new_target = hosted_tmux_target(&to.vendor, &target.persona);
    let new_unit = env.runtime.unit(&new_target);
    if env.runtime.separate_vendor_units() {
        if to.vendor != "muse" && old_unit != new_unit {
            units.push(new_unit);
        }
    } else if old_unit != name && !(to.vendor == "muse" && is_muse_window_target(old_unit)) {
        // A legacy record may name a different container. Its persona container can
        // still block the remodel, including a move from a non-Muse window to Muse.
        units.push(name.as_str());
    }
    for unit in units {
        if !env.runtime.exists(unit) {
            continue;
        }
        let why = match target.hosted.as_ref() {
            None => "no hosted record started it".to_string(),
            Some(r) if !r.runtime_spawned => format!("`{}` did not start it", r.session_id),
            Some(r) if r.tmux_target.as_deref().map(|t| env.runtime.unit(t)) != Some(unit) => {
                format!("`{}` started another tmux session", r.session_id)
            }
            // The flag says it started a Muse window, but the window under the name carries no
            // mark of its own: it is someone else's, and the teardown would leave it running.
            Some(r) if is_muse_window_target(unit) && !env.runtime.owns(unit, &r.session_id) => {
                format!(
                    "`{}` did not start the window now under that name",
                    r.session_id
                )
            }
            Some(r) => {
                let sharers = runtime_sharers(store, env.runtime, r)?;
                if sharers.is_empty() {
                    continue;
                }
                let ids: Vec<&str> = sharers.iter().map(|s| s.session_id.as_str()).collect();
                format!("it also runs {}", ids.join(", "))
            }
        };
        let kind = if unit.contains(':') {
            "window"
        } else {
            "session"
        };
        return Err(LifecycleError::Conflict(format!(
            "tmux {kind} `{unit}` runs and the teardown of `{}` would not stop it ({why}); stop \
             it first, or remodel with `hosted: false`",
            target.session_id
        )));
    }
    Ok(())
}

/// Whether [`remodel_session`] would refuse this remodel before its first side effect, with
/// nothing done: the same checks, error texts and statuses (see `plan_remodel`, whose result this
/// drops, so the two cannot drift). It only reads: the store, the runtime directory, the key
/// probe and, when runtime calls are on, whether a tmux session of the persona's name runs.
pub(crate) fn remodel_precheck<S: AgentRegistry + StateStore + Catalog>(
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
fn plan_remodel<S: AgentRegistry + StateStore + Catalog>(
    store: &S,
    env: &LifecycleEnv<'_>,
    target: &SessionTarget,
    req: &RemodelRequest,
    caller: Caller,
) -> Result<RemodelPlan, LifecycleError> {
    let mode = RemodelMode::parse(req.mode.as_deref())?;
    authorize_teardown(store, target, caller)?;
    // Same adoption as Prepare (issue #485): a decommissioned record with a live route is
    // treated as no record for the Owner; without a live route the 409 stays.
    let adopted;
    let target = if target.hosted.as_ref().is_some_and(is_decommissioned) {
        if !may_adopt_over_decommissioned(env, target, caller) {
            return Err(decommissioned_conflict(&target.session_id));
        }
        adopted = adopted_target(target);
        &adopted
    } else {
        target
    };
    let assignment = match current_recast_assignment(store, &target.persona)? {
        Some(agent) => Some((agent.project, role_label_of(&agent.role))),
        None => project_and_role(store, target)?,
    };
    let Some((project, role)) = assignment else {
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
    let from_thinking_level = old.and_then(|r| r.thinking_level.clone());
    let from_posture = old.map_or(fl_core::permissions::Posture::Auto, |r| r.posture);
    let resolved_cwd = if trimmed_opt(&req.cwd).is_some() {
        req.cwd.clone()
    } else {
        let old_is_ws_root = match (old_cwd.as_deref(), env.workspace_root) {
            (Some(old_path), Some(ws_root)) => Path::new(old_path.trim()) == ws_root,
            _ => false,
        };
        if old_cwd.is_none() || old_is_ws_root {
            resolve_project_cwd(store, &project, env.workspace_root, env.home_override)
                .or_else(|| old_cwd.clone())
        } else {
            old_cwd.clone()
        }
    };
    let mut to = SpinUpRequest {
        persona: target.persona.clone(),
        project: project.clone(),
        role: role.clone(),
        vendor: req.vendor.clone(),
        model: req.model.clone(),
        thinking_level: req.thinking_level.clone(),
        node_id: req.node_id.clone().or_else(|| Some(old_node.clone())),
        // The new session keeps the old one's `hosted`; with no record, spin-up's default.
        hosted: req.hosted.or(old.map(|r| r.hosted)),
        cwd: resolved_cwd,
        context_window: None,
        posture: req.posture,
    }
    .validate()?;
    to.permission_receipts = fl_wire::vendor_adapter::launch_permissions_with_auto_approval(
        &to.vendor,
        to.posture,
        env.auto_approval_for(&to.vendor),
    )
    .map_err(|msg| LifecycleError::Invalid(format!("posture: {msg}")))?
    .receipts;
    validate_catalog_role(store, &to.role)?;
    registered_role_prompt(
        store,
        env,
        to.cwd.as_deref().map(Path::new),
        &capitalize(&to.persona),
        &to.project,
        &agent_role_from_label(&to.role),
    )?;
    // Before the teardown: a remodel onto an API vendor with no key would retire the old
    // session for nothing.
    check_api_key(env, &to.vendor)?;
    check_slug_free(store, &format!("{}:{}", to.vendor, to.persona))?;
    check_runtime_cwd(&to)?;
    check_registration(store, &to, OwnAgent::Retiring)?;
    // The restore puts back what was there: a session with no record was not hosted, and a
    // restore must not turn it into a managed one.
    let restore = if lookup_vendor(&from_vendor).is_some() {
        fl_wire::vendor_adapter::launch_permissions_with_auto_approval(
            &from_vendor,
            from_posture,
            env.auto_approval_for(&from_vendor),
        )
        .map_err(|why| {
            LifecycleError::Invalid(format!(
                "cannot build restore plan for `{}` on `{from_vendor}` with recorded posture `{from_posture}`: {why}",
                target.session_id
            ))
        })?;
        Some(SpinUpRequest {
            persona: target.persona.clone(),
            project,
            role,
            vendor: from_vendor.clone(),
            model: from_model.clone(),
            thinking_level: from_thinking_level,
            node_id: Some(old_node),
            hosted: Some(old.is_some_and(|r| r.hosted)),
            cwd: old_cwd,
            // A restore must not fail on a window: it puts the session back as it can start.
            context_window: None,
            posture: from_posture,
        })
    } else {
        None
    };
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
        if to.hosted && vendor_has_runtime(&to.vendor) && env.spawn_runtime {
            env.runtime
                .preflight_spawn(&format!("{}:{}", to.vendor, to.persona))?;
        }
        // The old session is torn down even when the destination has no hosted runtime.
        preflight_teardown_runtime(store, env, target, caller)?;
        check_old_runtime_stoppable(store, env, target, &to)?;
    }
    preflight_vendor_launch(env, &to.vendor, to.hosted, &mut to.cwd)?;
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
pub fn remodel_session<S: AgentRegistry + StateStore + Catalog + MessageStore>(
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
fn remodel_with<S: AgentRegistry + StateStore + Catalog + MessageStore>(
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
        let tmux_target = target.hosted.as_ref().and_then(|r| r.tmux_target.clone());
        let host_session_id = tmux_target
            .as_deref()
            .and_then(fl_wire::vendor_adapter::unit_to_host_session_id)
            .map(|id| id.as_str().to_string());
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
            thinking_level: to.thinking_level.clone(),
            node_id: to.node_id.clone(),
            hosted: to.hosted,
            cwd: to.cwd.clone(),
            status: out.status,
            handshake_dispatched: out.handshake_dispatched,
            hydration_dispatched: false,
            hydration_status: crate::lifecycle::HydrationStatus::Unconfirmed,
            hydration_generation: None,
            runtime_stopped: false,
            tmux_target,
            host_session_id,
            prompt: out.prompt,
            handoff: None,
            warnings: Vec::new(),
            posture: to.posture,
            permission_receipts: to.permission_receipts.clone(),
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
            warnings.extend(out.warnings);
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
                thinking_level: out.thinking_level,
                node_id: out.node_id,
                hosted: out.hosted,
                cwd: to.cwd.clone(),
                status: out.status,
                handshake_dispatched: false,
                hydration_dispatched: out.hydration_dispatched,
                hydration_status: out.hydration_status,
                hydration_generation: out.hydration_generation,
                runtime_stopped: torn.runtime_stopped,
                tmux_target: out.tmux_target,
                host_session_id: out.host_session_id,
                prompt: out.prompt,
                handoff: Some(handoff),
                warnings,
                posture: out.posture,
                permission_receipts: out.permission_receipts,
            });
        }
        Err(error) => error,
    };

    // A spin-up that failed after its first persist left the new session's record `running`
    // with nothing behind it: put that record back as it was (marked decommissioned when it did
    // not exist), so only the restored session claims the persona.
    let mut cleanup_error = None;
    if to_session_id != plan.from_session_id
        && let Err(e) = undo_new_record(store, &to_session_id, to_before, by, env.home_override)
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
        let unit = out.tmux_target.as_deref().map(runtime_unit);
        // A Muse window that was there already (started by `muse-sessions`) is adopted, not
        // an error: the prompt went to it through the route.
        if out.vendor == "muse" && unit.is_some_and(|u| env.runtime.exists(u)) {
            return Ok(out);
        }
        let session = unit.unwrap_or(out.persona.as_str());
        return Err(LifecycleError::Store(format!(
            "its tmux runtime did not start (tmux failed, or session `{session}` already runs)"
        )));
    }
    Ok(out)
}

/// Put the record of `session_id` back as it was before a failed remodel's spin-up wrote it:
/// `before` when there was one, else the record marked decommissioned.
fn undo_new_record<S: AgentRegistry + StateStore>(
    store: &S,
    session_id: &str,
    before: Option<HostedSessionRecord>,
    by: &str,
    home_override: Option<&Path>,
) -> Result<(), LifecycleError> {
    let Some(mut now) =
        find_hosted_session(store, &[session_id])?.filter(|r| r.session_id == session_id)
    else {
        return Ok(());
    };
    let changed = match before {
        Some(before) if before != now => {
            persist_hosted_session(store, &before, by)?;
            true
        }
        Some(_) => false,
        None if is_decommissioned(&now) => false,
        None => {
            now.status = STATUS_DECOMMISSIONED.to_string();
            now.hydration_generation = None;
            now.updated_at_ms = now_ms();
            persist_hosted_session(store, &now, by)?;
            true
        }
    };
    if changed {
        refresh_decommission_tombstones(store, home_override);
    }
    Ok(())
}

#[cfg(test)]
mod remodel_tests;

#[cfg(test)]
mod vendor_registry_tests;

#[cfg(test)]
mod muse_launch_tests;

#[cfg(test)]
mod restore_source_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;
    use fl_core::at::At;
    use tempfile::TempDir;

    #[test]
    fn codex_config_home_uses_explicit_then_selected_home_and_resolves_relative_paths() {
        use std::ffi::OsStr;
        let base = Path::new("/base");
        for (explicit, override_home, home, expected) in [
            (
                Some("/configured"),
                Some("/isolated"),
                Some("/ambient"),
                "/configured",
            ),
            (Some("relative"), None, Some("/ambient"), "/base/relative"),
            (
                Some(""),
                Some("/isolated"),
                Some("/ambient"),
                "/isolated/.codex",
            ),
            (None, None, Some("/ambient"), "/ambient/.codex"),
        ] {
            assert_eq!(
                codex_config_home(
                    base,
                    explicit.map(OsStr::new),
                    override_home.map(Path::new),
                    home.map(OsStr::new)
                )
                .unwrap(),
                Path::new(expected)
            );
        }
        assert!(codex_config_home(base, None, None, None).is_err());
    }

    #[test]
    fn codex_trust_refuses_spinup_before_registration_without_host_registry() {
        let home = TempDir::new().unwrap();
        let _config_env = IsolatedCodexHome::new();
        let fake = FakeRuntime::default();
        let env = fake.env(home.path());
        let store = MemStore::default();
        let mut request = req("ada", "assistant", "codex");
        request.cwd = Some(home.path().display().to_string());
        let route = webhook_route_file(home.path(), "ada");
        let before_route = std::fs::read(&route).unwrap();
        for config in ["", "token = \"PRIVATE_TOKEN\n"] {
            std::fs::create_dir_all(home.path().join(".codex")).unwrap();
            std::fs::write(home.path().join(".codex/config.toml"), config).unwrap();
            let err = spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap_err();
            assert!(matches!(err, LifecycleError::Invalid(_)), "{err}");
            assert!(err.to_string().contains("trust_level"));
            assert!(!err.to_string().contains("PRIVATE_TOKEN"));
            assert!(store.agent_by_name("ada").unwrap().is_none());
            assert!(list_hosted_sessions(&store).unwrap().is_empty());
            assert!(fake.calls.borrow().is_empty());
            assert_eq!(std::fs::read(&route).unwrap(), before_route);
        }
    }

    #[test]
    fn codex_trust_refuses_restart_and_remodel_before_any_mutation() {
        for vendor in ["codex", "claude"] {
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let store = MemStore::default();
            let _config_env = trusted_codex_home(home.path(), home.path());
            let mut request = req("ada", "assistant", vendor);
            request.cwd = Some(home.path().display().to_string());
            spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap();
            std::fs::write(home.path().join(".codex/config.toml"), "").unwrap();
            let before = serde_json::to_value(list_hosted_sessions(&store).unwrap()).unwrap();
            let agent = serde_json::to_value(store.agent_by_name("ada").unwrap()).unwrap();
            let route = webhook_route_file(home.path(), "ada");
            let routes = std::fs::read(&route).unwrap();
            let calls = fake.calls.borrow().clone();
            let id = format!("{vendor}:ada");
            if vendor == "codex" {
                assert!(matches!(
                    restart_hosted_session(&store, &env, &id, "owner"),
                    Err(LifecycleError::Invalid(_))
                ));
            } else {
                let target = SessionTarget::resolve(&store, &id, None).unwrap();
                for mode in ["prepare", "execute"] {
                    let request = RemodelRequest {
                        vendor: "codex".into(),
                        mode: Some(mode.into()),
                        ..Default::default()
                    };
                    assert!(matches!(
                        remodel_session(
                            &store,
                            &env,
                            &target,
                            &request,
                            "owner",
                            Caller::Owner,
                            |_| panic!("queue changed")
                        ),
                        Err(LifecycleError::Invalid(_))
                    ));
                    assert!(
                        store
                            .list_state(None, None)
                            .unwrap()
                            .iter()
                            .all(|entry| entry.slug != remodel_handoff_slug("ada"))
                    );
                }
            }
            assert_eq!(
                serde_json::to_value(list_hosted_sessions(&store).unwrap()).unwrap(),
                before
            );
            assert_eq!(
                serde_json::to_value(store.agent_by_name("ada").unwrap()).unwrap(),
                agent
            );
            assert_eq!(std::fs::read(&route).unwrap(), routes);
            assert_eq!(*fake.calls.borrow(), calls);
        }
    }

    #[test]
    fn codex_trust_pins_launch_inputs_and_rejects_invalid_explicit_cwd() {
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = fake.env(home.path());
        for cwd in [None, Some(home.path().join(".").display().to_string())] {
            let store = MemStore::default();
            let expected = cwd
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap())
                .canonicalize()
                .unwrap();
            let _config_env = trusted_codex_home(home.path(), &expected);
            let mut request = req("ada", "assistant", "codex");
            request.cwd = cwd;
            fake.live.borrow_mut().clear();
            let out = spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap();
            assert_eq!(
                find_hosted_session(&store, &[&out.session_id])
                    .unwrap()
                    .unwrap()
                    .cwd
                    .as_deref(),
                expected.to_str()
            );
            assert_eq!(
                fake.spawn_cwds.borrow().last().unwrap().as_deref(),
                expected.to_str()
            );
            let launches = fake.launches.borrow();
            let launch = launches.last().unwrap().1.as_ref().unwrap();
            assert_eq!(
                launch
                    .env
                    .iter()
                    .filter(|(key, _)| key == "CODEX_HOME")
                    .collect::<Vec<_>>(),
                vec![&(
                    "CODEX_HOME".to_string(),
                    home.path().join(".codex").display().to_string()
                )]
            );
        }
        let store = MemStore::default();
        let mut request = req("bad", "assistant", "codex");
        request.cwd = Some(home.path().join("missing").display().to_string());
        assert!(matches!(
            spin_up(&store, &env, &request, "owner", Caller::Owner),
            Err(LifecycleError::Invalid(_))
        ));
        assert!(store.agent_by_name("bad").unwrap().is_none());
    }

    #[test]
    fn reforge_role_label_round_trips_through_agent_role_from_label() {
        let mut roles = vec![
            AgentRole::Lead,
            AgentRole::Assistant,
            AgentRole::LabManager,
            AgentRole::UiDeveloper,
            AgentRole::WebDeveloper,
            AgentRole::MobileDeveloper,
            AgentRole::SystemDeveloper,
            AgentRole::DataEngineer,
            AgentRole::Tester,
            AgentRole::Strategist,
            AgentRole::Journalist,
        ];
        for slug in [
            "lead",
            "assistant",
            "lab_manager",
            "lab-manager",
            "reviewer",
        ] {
            roles.push(AgentRole::Custom(slug.into()));
        }
        for role in roles {
            let label = reforge_role_label(&role);
            assert_eq!(agent_role_from_label(&label), role, "{label}");
        }
        assert_eq!(
            reforge_role_label(&AgentRole::Custom("lead".into())),
            "custom:lead"
        );
        assert_eq!(
            reforge_role_label(&AgentRole::Custom("reviewer".into())),
            "reviewer"
        );
    }

    #[test]
    fn host_runtime_send_keys_writes_text_then_a_separate_carriage_return() {
        use fl_host_proto::{
            ChildRecord, ChildState, ControllerLease, ExtensionRequest, ExtensionResponse,
            HostConfig, HostEndpoint, HostInstance, HostListener, HostRequest, HostResponse,
            InputState, SendOutcome, SessionEpoch, SessionId, SessionMode, SessionSummary,
            SpawnKind, StreamId,
        };
        use std::os::unix::fs::PermissionsExt;
        use std::sync::{Arc, Mutex};

        let _lock = crate::TMUX_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let sock_path = tmp.path().join("fl-host.sock");
        let endpoint = HostEndpoint::unix(&sock_path).unwrap();
        let sid = SessionId::new("Fay.lemonade").unwrap();
        let summary = SessionSummary {
            stream: StreamId {
                host: HostInstance::new([1u8; 16]),
                session: sid.clone(),
                epoch: SessionEpoch(1),
            },
            mode: SessionMode::ReliablePipe,
            state: ChildState::Running(ChildRecord {
                pid: std::process::id(),
                session: sid,
                session_epoch: SessionEpoch(1),
                kind: SpawnKind::Pipe,
                input: InputState::Writable,
                started_at_ms: 1000,
                exit: None,
                consumer_stalled: false,
            }),
            acked: 0,
            latest: 0,
            input: InputState::Writable,
        };
        let writes = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let writes_srv = Arc::clone(&writes);
        let ep_srv = endpoint.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut cfg = HostConfig::new(ep_srv);
                cfg.minor = 1;
                let mut listener = HostListener::bind(cfg).await.unwrap();
                let _ = ready_tx.send(());
                while let Ok(mut conn) = listener.accept().await {
                    let summary = summary.clone();
                    let writes = Arc::clone(&writes_srv);
                    tokio::spawn(async move {
                        while let Ok(req) = conn.recv().await {
                            let HostRequest::Extended { id, request } = req else {
                                break;
                            };
                            let resp = match request {
                                ExtensionRequest::ListSessions => ExtensionResponse::Sessions {
                                    records: vec![summary.clone()],
                                },
                                ExtensionRequest::ClaimController { .. } => {
                                    ExtensionResponse::Controller {
                                        lease: ControllerLease(1),
                                        acked: 0,
                                        latest: 0,
                                    }
                                }
                                ExtensionRequest::Write { bytes, .. } => {
                                    writes.lock().unwrap().push(bytes.into_vec());
                                    ExtensionResponse::Input {
                                        outcome: SendOutcome::Written,
                                    }
                                }
                                _ => break,
                            };
                            if conn
                                .respond(&HostResponse::Extended { id, response: resp })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                }
            });
        });
        ready_rx.recv().unwrap();

        let prev_sock = std::env::var_os("FL_HOST_SOCK");
        let prev_tmux = std::env::var_os("FL_WIRE_TMUX_BIN");
        // SAFETY: TMUX_ENV_LOCK is held; both variables are restored below.
        unsafe {
            std::env::set_var("FL_HOST_SOCK", &sock_path);
            std::env::remove_var("FL_WIRE_TMUX_BIN");
        }
        let sent = SessionRuntime::send_keys(&HostRuntime, "Fay.lemonade", "/model opus");
        let seen = writes.lock().unwrap().clone();
        // SAFETY: see above.
        unsafe {
            match prev_sock {
                Some(v) => std::env::set_var("FL_HOST_SOCK", v),
                None => std::env::remove_var("FL_HOST_SOCK"),
            }
            if let Some(v) = prev_tmux {
                std::env::set_var("FL_WIRE_TMUX_BIN", v);
            }
        }
        assert!(sent);
        assert_eq!(seen, vec![b"/model opus".to_vec(), b"\r".to_vec()]);
        drop(server_thread);
    }

    #[test]
    fn agy_cli_model_id_maps_to_an_effort_suffixed_id() {
        for (model, level, want) in [
            // A suffix on the model is kept.
            ("claude-sonnet-5-5-high", None, "claude-sonnet-5-5-high"),
            ("gemini-4-argon-low", None, "gemini-4-argon-low"),
            ("gemini-3.8-flash-medium", None, "gemini-3.8-flash-medium"),
            // An explicit effort replaces it.
            ("claude-opus-5-5-low", Some("high"), "claude-opus-5-5-high"),
            (
                "claude-opus-5-5-high",
                Some("medium"),
                "claude-opus-5-5-medium",
            ),
            // No suffix: the effort is appended, `medium` by default.
            ("claude-sonnet-5-5", None, "claude-sonnet-5-5-medium"),
            ("claude-sonnet-5-5", Some("high"), "claude-sonnet-5-5-high"),
            ("gemini-4-argon", Some("low"), "gemini-4-argon-low"),
            // Argon shorthand aliases normalize to `gemini-4-argon` before appending effort.
            ("argon-4.0", None, "gemini-4-argon-medium"),
            ("argon-4.0", Some("high"), "gemini-4-argon-high"),
            ("argon-4.0-low", None, "gemini-4-argon-low"),
            ("argon-4", None, "gemini-4-argon-medium"),
            ("argon", Some("low"), "gemini-4-argon-low"),
            ("gemini-4.0-argon", None, "gemini-4-argon-medium"),
            // The effort is read without case or surrounding space; anything else is ignored.
            ("gemini-4-argon", Some("HIGH"), "gemini-4-argon-high"),
            ("gemini-4-argon", Some(" Low "), "gemini-4-argon-low"),
            ("gemini-4-argon", Some("extreme"), "gemini-4-argon-medium"),
            ("gemini-4-argon-low", Some(""), "gemini-4-argon-low"),
            // GPT-OSS exists only as `-medium`.
            ("gpt-oss-120b", None, "gpt-oss-120b-medium"),
            ("gpt-oss-120b", Some("low"), "gpt-oss-120b-medium"),
            ("gpt-oss-120b", Some("high"), "gpt-oss-120b-medium"),
            ("gpt-oss-120b-low", None, "gpt-oss-120b-medium"),
            ("gpt-oss-120b-high", Some("high"), "gpt-oss-120b-medium"),
            // The model is trimmed; an empty model is absent.
            ("  claude-sonnet-5-5  ", None, "claude-sonnet-5-5-medium"),
            ("", Some("high"), ""),
            ("   ", None, ""),
            // The model, suffix included, is read without case.
            ("Claude-Sonnet-5-5-HIGH", None, "claude-sonnet-5-5-high"),
            ("Gemini-4-Argon-Low", None, "gemini-4-argon-low"),
            ("Claude-Opus-5-5-LOW", Some("high"), "claude-opus-5-5-high"),
            ("GPT-OSS-120B-High", None, "gpt-oss-120b-medium"),
            // Ids `normalize_plain_model` rejects still map to a string.
            ("--evil", None, "--evil-medium"),
            ("bad id", None, "bad id-medium"),
        ] {
            assert_eq!(agy_cli_model_id(model, level), want, "{model:?} {level:?}");
        }
    }

    /// The argv after the binary, which `FL_WIRE_*_BIN` or `PATH` decides.
    fn launch_args(vendor: &str, model: Option<&str>) -> Vec<String> {
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let spec = launch_spec_for(&env, vendor, "ada", model, None, "hello").unwrap();
        spec.argv[1..].to_vec()
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn antigravity_launch_maps_the_model_to_an_effort_suffixed_id() {
        let want = |model: &[&str]| {
            let mut args = vec!["--mode", "auto", "--remote-control"];
            args.extend_from_slice(model);
            args.extend(["--prompt-interactive", "hello"]);
            strings(&args)
        };
        assert_eq!(
            launch_args("antigravity", Some("claude-sonnet-5-5")),
            want(&["--model", "claude-sonnet-5-5-medium"])
        );
        assert_eq!(
            launch_args("antigravity", Some("claude-sonnet-5-5-high")),
            want(&["--model", "claude-sonnet-5-5-high"])
        );
        assert_eq!(
            launch_args("antigravity", Some("gpt-oss-120b-high")),
            want(&["--model", "gpt-oss-120b-medium"])
        );
        assert_eq!(launch_args("antigravity", None), want(&[]));
        // Empty and option-like ids are never passed to the CLI.
        assert_eq!(launch_args("antigravity", Some("  ")), want(&[]));
        assert_eq!(launch_args("antigravity", Some("--evil")), want(&[]));
    }

    #[test]
    fn other_vendors_launch_with_the_model_unchanged() {
        assert_eq!(
            launch_args("claude", Some("claude-sonnet-5-5")),
            strings(&[
                "--remote-control",
                "--permission-mode",
                "auto",
                "--model",
                "claude-sonnet-5-5",
                "hello"
            ])
        );
        assert_eq!(
            launch_args("claude", None),
            strings(&["--remote-control", "--permission-mode", "auto", "hello"])
        );
        assert_eq!(
            launch_args("codex", Some("gpt-6-sol")),
            strings(&[
                "--approve-for-me",
                "--remote",
                "unix://",
                "--model",
                "gpt-6-sol",
                "hello"
            ])
        );
        assert_eq!(
            launch_args("grok", Some("grok-5")),
            strings(&["--permission-mode", "auto", "--model", "grok-5", "hello"])
        );
        assert_eq!(
            launch_args("muse", Some("muse-2")),
            strings(&[
                "--model",
                "muse-2",
                "--approval-mode",
                "on-request",
                "--approval-judge",
                "on",
                "--disable-sandbox",
                "hello"
            ])
        );
        assert!(parse_remote_control_env(None));
        for on in ["", "1", "true", " TRUE ", "yes", "on"] {
            assert!(parse_remote_control_env(Some(on)), "{on:?}");
        }
        for off in ["0", "false", " FALSE ", "off", "no"] {
            assert!(!parse_remote_control_env(Some(off)), "{off:?}");
        }
    }

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
            workspace_root: None,
            log_hydration_warnings: true,
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

        for role in [
            "",
            "custom:",
            "web developer",
            "web-developer\u{0}",
            "web/developer",
            "tester!",
            "journalist:lead",
        ] {
            let e = req("felicity", role, "claude").validate().unwrap_err();
            assert_eq!(e.status(), 400, "{role}");
            assert!(
                matches!(&e, LifecycleError::Invalid(m) if m.starts_with("role")),
                "{role}"
            );
        }
        // Catalog existence is checked by spin-up/remodel against the store.
        for role in [
            "council-chair",
            "custom:council-chair",
            "custom:lead",
            "missing-role",
        ] {
            assert!(req("felicity", role, "claude").validate().is_ok(), "{role}");
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
            AgentRole::UiDeveloper
        );
        assert_eq!(agent_role_from_label("strategist"), AgentRole::Strategist);
        assert_eq!(agent_role_from_label("scribe"), AgentRole::Journalist);
        assert_eq!(
            agent_role_from_label("reviewer"),
            AgentRole::Custom("reviewer".into())
        );
        assert!(prompt_for("Faye", "FerroLoop", "strategist").contains("as the strategist."));
        assert_eq!(agent_role_from_label("lab-manager"), AgentRole::LabManager);
    }

    #[test]
    fn every_spin_up_label_is_a_first_class_role_that_round_trips_the_registry_wire_form() {
        let expected = [
            ("lead", AgentRole::Lead),
            ("assistant", AgentRole::Assistant),
            ("ui-developer", AgentRole::UiDeveloper),
            ("web-developer", AgentRole::WebDeveloper),
            ("mobile-developer", AgentRole::MobileDeveloper),
            ("system-developer", AgentRole::SystemDeveloper),
            ("data-engineer", AgentRole::DataEngineer),
            ("tester", AgentRole::Tester),
            ("strategist", AgentRole::Strategist),
            ("journalist", AgentRole::Journalist),
            ("lab-manager", AgentRole::LabManager),
        ];
        assert_eq!(expected.len(), SPIN_UP_ROLES.len());
        for (label, want) in expected {
            assert!(SPIN_UP_ROLES.contains(&label), "{label}");
            let role = agent_role_from_label(label);
            assert_eq!(role, want, "{label}");
            assert!(!matches!(role, AgentRole::Custom(_)), "{label}");
            assert_eq!(role_label_of(&role), label);
            assert_eq!(AgentRole::from_wire(&role.as_wire()).unwrap(), role);
            assert!(!role.as_wire().starts_with("custom:"), "{label}");
            assert!(
                prompt_for("Faye", "FerroLoop", label)
                    .contains(&format!("as the {}.", reforge_role_label(&role)))
            );
        }
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
    fn hydration_status_typed_evidence_and_ambiguous_failures_do_not_resend() {
        use fl_wire::dispatcher::OperatorPromptOutcome;
        let cases = [
            (
                Ok(OperatorPromptOutcome::AppServerAccepted {
                    turn_id: "accepted-turn".into(),
                }),
                true,
                "confirmed",
            ),
            (Ok(OperatorPromptOutcome::VendorQueued), true, "unconfirmed"),
            (
                Ok(OperatorPromptOutcome::TransportAccepted),
                true,
                "unconfirmed",
            ),
            (
                Ok(OperatorPromptOutcome::CompactionAccepted),
                true,
                "unconfirmed",
            ),
            (
                Err(PushError::Failed("timeout after write".into())),
                false,
                "unconfirmed",
            ),
            (
                Err(PushError::NotConnected("refused before write".into())),
                false,
                "failed",
            ),
        ];
        for (delivery, dispatched, status) in cases {
            let home = TempDir::new().unwrap();
            webhook_route_file(home.path(), "ada");
            let _listener =
                std::os::unix::net::UnixListener::bind(home.path().join("none.sock")).unwrap();
            crate::discovery::note_route_file_scan(Some(home.path()), Instant::now());
            let mut sends = 0;
            let result = dispatch_with_rediscovery_typed(
                Some(home.path()),
                RouteQuery::persona("ada", "claude:ada"),
                |_| {
                    sends += 1;
                    delivery.clone()
                },
            );
            let outcome = HydrationOutcome::from_delivery(result);
            assert_eq!(outcome.dispatched, dispatched);
            assert_eq!(serde_json::to_value(outcome.status).unwrap(), status);
            assert_eq!(
                sends, 1,
                "accepted or ambiguous input must not be sent again"
            );
        }
        let missing = HydrationOutcome::from_delivery(None);
        assert!(!missing.dispatched);
        assert_eq!(serde_json::to_value(missing.status).unwrap(), "failed");
    }

    #[test]
    fn hydration_status_webhook_transport_is_unconfirmed_and_no_route_is_failed() {
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let missing = deliver_prompt(&env, "ada", "claude:ada", "owner", "hydrate once");
        assert!(!missing.dispatched);
        assert_eq!(serde_json::to_value(missing.status).unwrap(), "failed");
        webhook_route_file(home.path(), "ada");
        let listener =
            std::os::unix::net::UnixListener::bind(home.path().join("none.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        crate::discovery::note_route_file_scan(Some(home.path()), Instant::now());
        let sent = deliver_prompt(&env, "ada", "claude:ada", "owner", "hydrate once");
        assert!(sent.dispatched);
        assert_eq!(serde_json::to_value(sent.status).unwrap(), "unconfirmed");
        let (mut stream, _) = listener.accept().unwrap();
        let mut body = String::new();
        std::io::Read::read_to_string(&mut stream, &mut body).unwrap();
        assert!(body.contains("hydrate once"));
        assert_eq!(body.lines().count(), 1);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    // Hydration acceptance is independent of the legacy handoff boolean.
    #[test]
    fn hydration_status_legacy_default_and_record_round_trip() {
        for dispatched in [false, true] {
            let mut json =
                serde_json::to_value(hosted_record("claude", "ada", "running", false)).unwrap();
            json["hydration_dispatched"] = dispatched.into();
            json.as_object_mut().unwrap().remove("hydration_status");
            let record: HostedSessionRecord = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(record).unwrap()["hydration_status"],
                "unconfirmed"
            );
            for status in ["confirmed", "unconfirmed", "failed"] {
                json["hydration_status"] = status.into();
                let record: HostedSessionRecord = serde_json::from_value(json.clone()).unwrap();
                let encoded = serde_json::to_value(record).unwrap();
                assert_eq!(encoded["hydration_status"], status);
                assert_eq!(encoded["hydration_dispatched"], dispatched);
            }
        }
    }

    #[test]
    fn hydration_status_responses_default_legacy_values_and_round_trip() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let spun = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let target = SessionTarget::resolve(&store, "ada", None).unwrap();
        let remodeled = remodel_session(
            &store,
            &env,
            &target,
            &RemodelRequest {
                vendor: "muse".into(),
                ..Default::default()
            },
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        for (mut json, spin_response) in [
            (serde_json::to_value(spun).unwrap(), true),
            (serde_json::to_value(remodeled).unwrap(), false),
        ] {
            json["hydration_dispatched"] = true.into();
            json.as_object_mut().unwrap().remove("hydration_status");
            json.as_object_mut().unwrap().remove("warnings");
            for status in [None, Some("confirmed"), Some("unconfirmed"), Some("failed")] {
                if let Some(status) = status {
                    json["hydration_status"] = status.into();
                }
                let encoded = if spin_response {
                    serde_json::to_value(
                        serde_json::from_value::<SpinUpResponse>(json.clone()).unwrap(),
                    )
                    .unwrap()
                } else {
                    serde_json::to_value(
                        serde_json::from_value::<RemodelResponse>(json.clone()).unwrap(),
                    )
                    .unwrap()
                };
                assert_eq!(encoded["hydration_status"], status.unwrap_or("unconfirmed"));
                assert_eq!(encoded["warnings"], serde_json::json!([]));
            }
        }
    }

    #[test]
    fn hydration_status_restart_and_remodel_replace_previous_confirmation() {
        for (hosted, spawn) in [(true, true), (true, false), (false, false)] {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let runtime = FakeRuntime::default();
            let mut env = env(&home);
            env.runtime = &runtime;
            let mut request = req("ada", "assistant", "claude");
            request.hosted = Some(hosted);
            spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap();
            let confirm_previous = || {
                let record = find_hosted_session(&store, &["ada"]).unwrap().unwrap();
                let mut json = serde_json::to_value(record).unwrap();
                json["hydration_status"] = "confirmed".into();
                json["hydration_dispatched"] = true.into();
                persist_hosted_session(&store, &serde_json::from_value(json).unwrap(), "owner")
                    .unwrap();
            };
            confirm_previous();
            env.spawn_runtime = spawn;
            let restarted = restart_hosted_session(&store, &env, "ada", "owner").unwrap();
            let expected = if spawn { "unconfirmed" } else { "failed" };
            assert_eq!(
                serde_json::to_value(&restarted).unwrap()["hydration_status"],
                expected
            );
            assert_eq!(restarted.record.hydration_dispatched, spawn);
            confirm_previous();
            let target = SessionTarget::resolve(&store, "ada", None).unwrap();
            let remodeled = remodel_session(
                &store,
                &env,
                &target,
                &RemodelRequest {
                    vendor: "muse".into(),
                    ..Default::default()
                },
                "owner",
                Caller::Owner,
                |_| {},
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(&remodeled).unwrap()["hydration_status"],
                expected
            );
            assert_eq!(remodeled.hydration_dispatched, spawn);
            let record = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
            assert_eq!(
                serde_json::to_value(record).unwrap()["hydration_status"],
                expected
            );
        }
    }

    #[test]
    fn hydration_status_immediate_exit_is_failed_on_spin_and_restart() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let runtime = FakeRuntime::default();
        runtime.vanish_windows.borrow_mut().push("claude".into());
        let mut env = env(&home);
        env.runtime = &runtime;
        env.spawn_runtime = true;
        let spun = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(!spun.hydration_dispatched);
        assert_eq!(
            serde_json::to_value(spun).unwrap()["hydration_status"],
            "failed"
        );
        let restarted = restart_hosted_session(&store, &env, "ada", "owner").unwrap();
        assert!(!restarted.record.hydration_dispatched);
        assert_eq!(
            serde_json::to_value(restarted).unwrap()["hydration_status"],
            "failed"
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
    fn recast_startup_and_restart_use_current_registration_over_stale_hosted_assignment() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let runtime = FakeRuntime::default();
        let mut test_env = env(&home);
        test_env.runtime = &runtime;
        test_env.spawn_runtime = true;
        spin_up(
            &store,
            &test_env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let old = store.agent_by_name("ada").unwrap().unwrap();
        let target_role = AgentRole::Custom("council-chair".into());
        crate::agents::register_recast(
            &store,
            &old,
            RegisterAgentArgs {
                display_name: old.display_name.clone(),
                email: Some(old.email.clone()),
                project: "sonora".into(),
                role: target_role.clone(),
                extra_areas: vec![],
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "owner".into(),
            },
        )
        .unwrap();
        let restarted = restart_hosted_session(&store, &test_env, "claude:ada", "owner").unwrap();
        assert_eq!(restarted.record.project, "sonora");
        assert!(
            restarted
                .prompt
                .contains("--role custom:council-chair --replace")
        );
        let spun = spin_up(
            &store,
            &test_env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(spun.prompt.unwrap().contains("--project sonora"));
        assert_eq!(
            store.agent_by_name("ada").unwrap().unwrap().role,
            target_role
        );
    }

    #[test]
    fn retired_recast_persona_reuses_id_with_requested_assignment_and_hydration() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let runtime = FakeRuntime::default();
        let mut test_env = env(&home);
        test_env.runtime = &runtime;
        test_env.spawn_runtime = true;
        spin_up(
            &store,
            &test_env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let old = store.agent_by_name("ada").unwrap().unwrap();
        crate::agents::register_recast(
            &store,
            &old,
            RegisterAgentArgs {
                display_name: old.display_name.clone(),
                email: Some(old.email.clone()),
                project: "sonora".into(),
                role: AgentRole::Custom("council-chair".into()),
                extra_areas: vec![],
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "owner".into(),
            },
        )
        .unwrap();
        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        teardown_session(&store, &test_env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(
            store.agent_by_name("ada").unwrap().unwrap().status,
            AgentStatus::Retired
        );
        let mut request = req("ada", "tester", "claude");
        request.project = "new-project".into();
        let spun = spin_up(&store, &test_env, &request, "owner", Caller::Owner).unwrap();
        let agent = store.agent_by_name("ada").unwrap().unwrap();
        assert_eq!(agent.id, old.id);
        assert_eq!(agent.project, "new-project");
        assert_eq!(agent.role, AgentRole::Tester);
        assert_eq!(agent.status, AgentStatus::Active);
        let prompt = spun.prompt.unwrap();
        assert!(prompt.contains("--project new-project"), "{prompt}");
        assert!(prompt.contains("--role tester --replace"), "{prompt}");
        assert!(spun.hydration_dispatched);
        let launches = runtime.launches.borrow();
        let launch = launches.last().unwrap().1.as_ref().unwrap();
        assert!(launch.argv.contains(&prompt), "{launch:?}");
    }

    #[test]
    fn hydration_paths_use_registered_case_worktree_identity_and_missing_warnings() {
        let home = TempDir::new().unwrap();
        let ws = home.path().join("workspace");
        let root = ws.join("Actual-Checkout");
        let worktree = ws.join("scratch/renamed");
        for dir in [
            root.join(".git/worktrees/topic"),
            worktree.join("src"),
            ws.join(".agents/roles/Logical-Name"),
            ws.join("Notes"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let ws = ws.canonicalize().unwrap();
        let root = root.canonicalize().unwrap();
        let worktree = worktree.canonicalize().unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}", root.join(".git/worktrees/topic").display()),
        )
        .unwrap();
        std::fs::write(root.join(".git/worktrees/topic/commondir"), "../..").unwrap();
        for base in [&root, &worktree] {
            for file in ["AGENTS.md", "WORKFLOW.md", "PERSONA.md"] {
                std::fs::write(base.join(file), "guidance").unwrap();
            }
        }
        std::fs::write(ws.join(".agents/AGENTS.md"), "workspace").unwrap();
        std::fs::write(ws.join(".agents/roles/Logical-Name/assistant.md"), "role").unwrap();
        let config = home.path().join(".config/fl");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.toml"),
            format!(
                "[[project]]\nname = 'logical-name'\nroot = '{}'\n",
                root.display()
            ),
        )
        .unwrap();
        for (cwd, expected, outside) in [
            (&root, &root, false),
            (&worktree.join("src"), &worktree, false),
            (&ws, &root, true),
        ] {
            let store = MemStore::default();
            let mut test_env = env(&home);
            test_env.workspace_root = Some(&ws);
            let mut request = req("ada", "assistant", "claude");
            request.project = "logical-name".into();
            request.cwd = Some(cwd.display().to_string());
            let out = spin_up(&store, &test_env, &request, "owner", Caller::Owner).unwrap();
            let prompt = out.prompt.as_ref().unwrap();
            assert!(
                prompt.contains(&format!("`{}`", expected.join("AGENTS.md").display())),
                "{prompt}"
            );
            assert!(
                prompt.contains(
                    &ws.join(".agents/roles/Logical-Name/assistant.md")
                        .display()
                        .to_string()
                ),
                "{prompt}"
            );
            assert!(!prompt.contains("STATE.md"));
            let json = serde_json::to_value(&out).unwrap();
            let warnings = json["warnings"].as_array().unwrap();
            assert_eq!(
                warnings.iter().any(|w| w
                    .as_str()
                    .unwrap()
                    .contains("cwd is outside project logical-name")),
                outside
            );
        }
    }

    #[test]
    fn hydration_missing_guidance_is_response_data_on_all_lifecycle_paths() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let test_env = env(&home);
        let spun = spin_up(
            &store,
            &test_env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        crate::hydration::WARNING_LOG.with_borrow_mut(Vec::clear);
        let restarted =
            restart_hosted_session(&store, &test_env, &spun.session_id, "owner").unwrap();
        let logged = crate::hydration::WARNING_LOG.with_borrow(Clone::clone);
        assert_eq!(logged.len(), restarted.warnings.len());
        for (line, warning) in logged.iter().zip(&restarted.warnings) {
            assert!(line.contains("persona=ada session=claude:ada"), "{line}");
            assert!(line.ends_with(warning), "{line}");
        }
        assert!(
            serde_json::to_value(&restarted.record)
                .unwrap()
                .get("warnings")
                .is_none()
        );
        let mut legacy = serde_json::to_value(&restarted).unwrap();
        legacy.as_object_mut().unwrap().remove("warnings");
        assert!(
            serde_json::from_value::<RestartResponse>(legacy)
                .unwrap()
                .warnings
                .is_empty()
        );
        let target = SessionTarget::resolve(&store, &spun.session_id, None).unwrap();
        let remodeled = remodel_session(
            &store,
            &test_env,
            &target,
            &RemodelRequest {
                vendor: "claude".into(),
                ..Default::default()
            },
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        for value in [
            serde_json::to_value(spun).unwrap(),
            serde_json::to_value(restarted).unwrap(),
            serde_json::to_value(remodeled).unwrap(),
        ] {
            assert!(
                value["warnings"].as_array().is_some_and(|w| w.len() >= 5),
                "{value}"
            );
            assert!(
                value["prompt"]
                    .as_str()
                    .unwrap()
                    .contains("Missing guidance")
            );
        }
    }

    #[test]
    fn legacy_unknown_role_hydration_preserves_file_fallback() {
        let store = MemStore::default();
        let role = AgentRole::Custom("legacy-reviewer".into());
        assert!(!has_catalog_guidance(&store, "legacy-reviewer").unwrap());
        let home = TempDir::new().unwrap();
        std::fs::create_dir(home.path().join("Notes")).unwrap();
        let file = home.path().join(".agents/roles/legacy-reviewer.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "legacy role").unwrap();
        let mut env = env(&home);
        env.workspace_root = Some(home.path());
        let prompt = registered_role_prompt(&store, &env, None, "Ada", "ferroloop", &role)
            .unwrap()
            .text;
        assert!(prompt.contains(&format!("Read your role file(s): `{}`", file.display())));
        assert!(prompt.contains("--role legacy-reviewer --replace"));
    }

    #[test]
    fn catalog_chair_spin_up_uses_current_template_without_inventing_role_file() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let runtime = FakeRuntime::default();
        let mut test_env = env(&home);
        test_env.runtime = &runtime;
        test_env.spawn_runtime = true;
        let mut chair = fl_core::agent::get_role(&store, "council-chair")
            .unwrap()
            .unwrap();
        chair.prompt_template =
            "Read .agents/roles/strategist.md; chair edited evidence review.".into();
        fl_core::agent::upsert_role(&store, chair).unwrap();
        let out = spin_up(
            &store,
            &test_env,
            &req("ada", "custom:council-chair", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(
            store.agent_by_name("ada").unwrap().unwrap().role,
            AgentRole::Custom("council-chair".into())
        );
        let prompt = out.prompt.unwrap();
        assert!(prompt.contains("chair edited evidence review"), "{prompt}");
        assert!(!prompt.contains("roles/council-chair.md"), "{prompt}");
    }

    #[test]
    fn catalog_invalid_unknown_and_deleted_roles_have_no_registration_or_runtime_effects() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let runtime = FakeRuntime::default();
        let mut test_env = env(&home);
        test_env.runtime = &runtime;
        test_env.spawn_runtime = true;
        fl_core::agent::delete_role(&store, "council-chair").unwrap();
        for role in [
            "custom:council-chair",
            "missing-adviser",
            "custom:missing-adviser",
            "custom:bad role",
        ] {
            assert_eq!(
                spin_up(
                    &store,
                    &test_env,
                    &req("ada", role, "claude"),
                    "owner",
                    Caller::Owner
                )
                .unwrap_err()
                .status(),
                400,
                "{role}"
            );
            assert!(store.agent_by_name("ada").unwrap().is_none());
            assert!(list_hosted_sessions(&store).unwrap().is_empty());
            assert!(store.get_area("ferroloop").unwrap().is_none());
            assert!(runtime.calls.borrow().is_empty());
        }
    }

    #[test]
    fn catalog_custom_structural_shadows_keep_identity_without_builtin_guidance() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let runtime = FakeRuntime::default();
        let mut test_env = env(&home);
        test_env.runtime = &runtime;
        test_env.spawn_runtime = true;
        for (i, slug) in ["lead", "assistant", "lab_manager", "lab-manager"]
            .into_iter()
            .enumerate()
        {
            let role = format!("custom:{slug}");
            let persona = format!("agent-{i}");
            let out = spin_up(
                &store,
                &test_env,
                &req(&persona, &role, "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            assert_eq!(
                store.agent_by_name(&persona).unwrap().unwrap().role,
                AgentRole::Custom(slug.into())
            );
            let prompt = out.prompt.unwrap();
            assert!(!prompt.contains(&format!("roles/{slug}.md")), "{prompt}");
            assert!(!prompt.contains("lead project architecture"), "{prompt}");
            assert!(
                prompt.contains(&format!("--role {role} --replace")),
                "{prompt}"
            );
        }
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
            "Take the name of Felicity and take up residence in the unresolved project context as the ui-developer."
        ));

        let agent = store.agent_by_name("felicity").unwrap().unwrap();
        assert_eq!(agent.status, AgentStatus::Active);
        assert_eq!(agent.role, AgentRole::UiDeveloper);
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
    fn every_listed_role_spins_up_and_registers_its_agent_role() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        for (i, role) in SPIN_UP_ROLES.iter().enumerate() {
            // `lead` and `lab-manager` are distinct personas in one project: only one Lead sits.
            let persona = format!("agent-{i}");
            let out = spin_up(
                &store,
                &env,
                &req(&persona, role, "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            assert_eq!(out.role, *role);
            let agent = store.agent_by_name(&persona).unwrap().unwrap();
            assert_eq!(agent.status, AgentStatus::Active);
            assert_eq!(agent.role, agent_role_from_label(role), "{role}");
            assert_eq!(agent.role, AgentRole::from_wire(role).unwrap(), "{role}");
            assert!(!matches!(agent.role, AgentRole::Custom(_)), "{role}");
        }
        // The new labels did not displace the one Lead.
        let leads = store
            .list_agents(Some("ferroloop"), false)
            .unwrap()
            .into_iter()
            .filter(|a| a.role == AgentRole::Lead)
            .count();
        assert_eq!(leads, 1);
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
            "fl state handoff set --scope project:ferroloop --key felicity --summary",
            "fl state checkpoint --scope project:ferroloop",
            "worktrees",
            "fl wire send <lead> \"Felicity decommission handoff\"",
            "fl wire read --ack",
            "decommission-ready",
            "Lead",
        ] {
            assert!(p.contains(needle), "missing {needle}: {p}");
        }
        assert!(synthesize_decommission_prompt("Ferris", "FerroLoop", "lead").contains("jarvis"));
        assert!(!p.contains("jarvis"));
    }

    #[test]
    fn decommission_prompt_sends_the_final_handoff_exactly_once() {
        // `agent-msg` is a symlink to `fl`, so a second send line delivers the handoff twice.
        for role in ["lead", "lab_manager", "assistant"] {
            let p = synthesize_decommission_prompt("Felicity", "FerroLoop", role);
            let sends = p.matches("fl wire send").count() + p.matches("agent-msg send").count();
            assert_eq!(sends, 1, "{role}: {p}");
            // One ack command only, and the prompt counts four steps.
            assert_eq!(p.matches("read --ack").count(), 1, "{role}: {p}");
            assert!(p.contains("all four steps"), "{role}: {p}");
            let numbered = p
                .lines()
                .filter(|l| {
                    let t = l.trim_start();
                    t.len() > 2 && t.as_bytes()[0].is_ascii_digit() && t[1..].starts_with(". ")
                })
                .count();
            assert_eq!(numbered, 4, "{role}: {p}");
        }
    }

    #[test]
    fn decommission_prompt_makes_ferrostate_primary_and_state_md_optional() {
        let p = synthesize_decommission_prompt("Felicity", "FerroLoop", "assistant");
        let step1 = p
            .lines()
            .find(|l| l.trim_start().starts_with("1."))
            .expect("step 1");
        assert!(step1.contains("fl state handoff set"), "{step1}");
        assert!(step1.contains("fl state checkpoint"), "{step1}");
        assert!(step1.contains("primary"), "{step1}");
        for mention in p.match_indices("STATE.md") {
            let around = &p[mention.0.saturating_sub(40)..(mention.0 + 120).min(p.len())];
            assert!(around.contains("optional legacy"), "{around}");
        }
    }

    #[test]
    fn decommission_prompt_names_the_right_recipient_per_role() {
        let lead = synthesize_decommission_prompt("Ferris", "FerroLoop", "lead");
        assert!(lead.contains("to `jarvis` once"), "{lead}");
        assert!(lead.contains("fl wire send jarvis \"Ferris decommission handoff\""));
        let manager = synthesize_decommission_prompt("Wendy", "FerroLoop", "lab_manager");
        assert!(manager.contains("to the owner once"), "{manager}");
        assert!(!manager.contains("jarvis"));
        let other = synthesize_decommission_prompt("Felicity", "FerroLoop", "ui-developer");
        assert!(other.contains("to the FerroLoop Lead"), "{other}");
        assert!(other.contains("fl wire send <lead> \"Felicity decommission handoff\""));
        assert!(!other.contains("jarvis"));
    }

    #[test]
    fn reforge_prompt_reads_ferrostate_first_and_state_md_only_as_legacy() {
        let p = canonical_reforge_prompt("Felicity", "FerroLoop", "assistant");
        assert!(
            p.contains("fl state render --scope workspace --scope project:ferroloop"),
            "{p}"
        );
        assert!(
            p.contains("fl state handoff get --scope project:ferroloop --key felicity"),
            "{p}"
        );
        // A first reforge has no per-agent handoff yet: the shared `current` one is the fallback.
        assert!(
            p.contains("`fl state handoff get --scope project:ferroloop` reads the shared `current` handoff"),
            "{p}"
        );
        assert!(p.contains("primary state source"), "{p}");
        // A context without an existing legacy file must not invent one.
        assert!(!p.contains("STATE.md"));
        // Registration and inbox steps stay.
        assert!(p.contains("fl agent roster"), "{p}");
        assert!(!p.contains("REGISTRY.md"), "{p}");
        assert!(
            p.contains("fl wire register Felicity --project FerroLoop --role assistant --replace")
        );
        assert!(p.contains("AGENT_PERSONA=felicity fl wire read --ack"));
    }

    /// Every `--scope <token>` in the generated prompts must be a token the `fl state` parser
    /// accepts, whatever the project name looks like.
    #[test]
    fn prompt_scope_tokens_parse_as_state_scopes() {
        use fl_core::state::StateScope;
        for project in ["FerroLoop", "ArtificialHumanity.io", "!!!", ""] {
            let prompts = [
                canonical_reforge_prompt("Felicity", project, "assistant"),
                synthesize_decommission_prompt("Felicity", project, "assistant"),
            ];
            for p in prompts {
                let tokens: Vec<&str> = p
                    .split("--scope ")
                    .skip(1)
                    .map(|rest| rest.split([' ', '`']).next().unwrap())
                    .collect();
                assert!(tokens.len() >= 2, "{project}: {p}");
                for t in tokens {
                    StateScope::from_wire(t)
                        .unwrap_or_else(|e| panic!("{project}: scope {t:?} rejected: {e}"));
                }
            }
        }
        let p = synthesize_decommission_prompt("Ada", "ArtificialHumanity.io", "assistant");
        assert!(p.contains("--scope project:artificialhumanity-io"), "{p}");
        let p = synthesize_decommission_prompt("Ada", "!!!", "assistant");
        assert!(p.contains("--scope workspace"), "{p}");
        assert!(!p.contains("project:"), "{p}");
    }

    /// The `--key` token in the prompts is the lowercase persona, which must be a valid state
    /// slug, so concurrent agents in one project never share (and overwrite) a handoff key.
    #[test]
    fn prompt_handoff_key_is_the_lowercase_persona_and_a_valid_slug() {
        use fl_core::state::validate_slug;
        for (name, role) in [
            ("Felicity", "assistant"),
            ("Ferris", "lead"),
            ("Ada-2", "lab_manager"),
        ] {
            let lower = name.to_ascii_lowercase();
            // The name is one a session can really have (`is_valid_persona` is lowercase letters,
            // digits and `-`, always a valid slug).
            assert!(is_valid_persona(&lower), "{lower}");
            let prompts = [
                canonical_reforge_prompt(name, "FerroLoop", role),
                synthesize_decommission_prompt(name, "FerroLoop", role),
            ];
            for p in prompts {
                let keys: Vec<&str> = p
                    .split("--key ")
                    .skip(1)
                    .map(|rest| rest.split([' ', '`']).next().unwrap())
                    .collect();
                assert!(!keys.is_empty(), "{name}: {p}");
                for k in keys {
                    assert_eq!(k, lower, "{p}");
                    assert_eq!(validate_slug(k).unwrap(), k, "{p}");
                    assert_ne!(k, "current", "{p}");
                }
            }
        }
    }

    #[test]
    fn decommission_prompt_bounds_the_handoff_summary() {
        let p = synthesize_decommission_prompt("Felicity", "FerroLoop", "assistant");
        assert!(p.contains("ONE line of at most 240 characters"), "{p}");
        for flag in ["--shipped", "--in-flight", "--blocker", "--next", "--notes"] {
            assert!(p.contains(flag), "missing {flag}: {p}");
        }
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
    fn spin_up_uses_the_catalog_project_directory_when_cwd_is_unset() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let project_dir = workspace.path().join("FerroLoop");
        std::fs::create_dir(&project_dir).unwrap();
        store
            .add_project(&project_dir.display().to_string())
            .unwrap();
        let out = spin_up(
            &store,
            &env(&home),
            &req("felicity", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let rec = find_hosted_session(&store, &[&out.session_id])
            .unwrap()
            .unwrap();
        assert_eq!(rec.cwd.as_deref(), Some(project_dir.to_str().unwrap()));
    }

    #[test]
    fn spin_up_uses_a_workspace_sibling_when_the_catalog_has_no_project() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let project_dir = workspace.path().join("notes");
        std::fs::create_dir(&project_dir).unwrap();
        let mut request = req("felicity", "assistant", "claude");
        request.project = "notes".into();
        let mut env = env(&home);
        env.workspace_root = Some(workspace.path());
        spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap();
        let rec = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        assert_eq!(rec.cwd.as_deref(), Some(project_dir.to_str().unwrap()));
    }

    fn write_project_bindings(home: &Path, roots: &[PathBuf]) {
        let config = home.join(".config/fl");
        std::fs::create_dir_all(&config).unwrap();
        let text: String = roots
            .iter()
            .map(|root| {
                format!(
                    "[[project]]\nname = 'Audio-Suite'\nroot = {:?}\n",
                    root.to_str().unwrap()
                )
            })
            .collect();
        std::fs::write(config.join("config.toml"), text).unwrap();
    }

    #[test]
    fn named_binding_cwd_uses_umbrella_before_catalog_or_workspace_names() {
        let home = TempDir::new().unwrap();
        let workspace = home.path().join("workspace");
        let umbrella = workspace.join("Sonora");
        let root = umbrella.join("github");
        let decoy = workspace.join("audio-suite");
        for dir in [&root, &decoy] {
            std::fs::create_dir_all(dir).unwrap();
        }
        write_project_bindings(home.path(), std::slice::from_ref(&root));
        let store = MemStore::default();
        store.add_project(decoy.to_str().unwrap()).unwrap();
        let mut test_env = env(&home);
        test_env.workspace_root = Some(&workspace);
        let mut request = req("ada", "assistant", "claude");
        request.project = "audio-suite".into();
        spin_up(&store, &test_env, &request, "owner", Caller::Owner).unwrap();
        let record = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(record.cwd.as_deref(), umbrella.to_str());
        request.persona = "bea".into();
        request.cwd = Some(root.display().to_string());
        spin_up(&store, &test_env, &request, "owner", Caller::Owner).unwrap();
        let record = find_hosted_session(&store, &["claude:bea"])
            .unwrap()
            .unwrap();
        assert_eq!(record.cwd.as_deref(), root.to_str());
    }

    #[test]
    fn named_binding_cwd_keeps_external_roots_and_deduplicates_bindings() {
        let home = TempDir::new().unwrap();
        let workspace = home.path().join("workspace");
        let root = home.path().join("external/github");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        write_project_bindings(home.path(), &[root.clone(), root.clone()]);
        let store = MemStore::default();
        let mut test_env = env(&home);
        test_env.workspace_root = Some(&workspace);
        let mut request = req("ada", "assistant", "claude");
        request.project = "audio-suite".into();
        spin_up(&store, &test_env, &request, "owner", Caller::Owner).unwrap();
        let record = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(record.cwd.as_deref(), root.to_str());
    }

    #[cfg(unix)]
    #[test]
    fn named_binding_cwd_symlink_escape_keeps_canonical_external_root() {
        let home = TempDir::new().unwrap();
        let workspace = home.path().join("workspace");
        let root = home.path().join("external/github");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        let link = workspace.join("Sonora");
        std::os::unix::fs::symlink(root.parent().unwrap(), &link).unwrap();
        write_project_bindings(home.path(), &[link.join("github")]);
        let store = MemStore::default();
        let mut test_env = env(&home);
        test_env.workspace_root = Some(&workspace);
        let mut request = req("ada", "assistant", "claude");
        request.project = "audio-suite".into();
        spin_up(&store, &test_env, &request, "owner", Caller::Owner).unwrap();
        let record = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(record.cwd.as_deref(), root.to_str());
    }

    #[test]
    fn named_binding_cwd_invalid_or_ambiguous_never_selects_a_decoy() {
        let home = TempDir::new().unwrap();
        let workspace = home.path().join("workspace");
        let root = workspace.join("Sonora/github");
        let other = workspace.join("Other/github");
        let decoy = workspace.join("audio-suite");
        for dir in [&root, &other, &decoy] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for roots in [
            vec![root.clone(), other],
            vec![PathBuf::from(".")],
            vec![root.clone(), workspace.join("missing")],
        ] {
            write_project_bindings(home.path(), &roots);
            let store = MemStore::default();
            store.add_project(decoy.to_str().unwrap()).unwrap();
            let mut test_env = env(&home);
            test_env.workspace_root = Some(&workspace);
            let mut request = req("ada", "assistant", "claude");
            request.project = "audio-suite".into();
            spin_up(&store, &test_env, &request, "owner", Caller::Owner).unwrap();
            let record = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap();
            assert_eq!(record.cwd, None, "bindings: {roots:?}");
        }
    }

    #[test]
    fn named_binding_cwd_unavailable_or_unbound_config_preserves_catalog_fallback() {
        let home = TempDir::new().unwrap();
        let root = home.path().join("audio-suite");
        std::fs::create_dir(&root).unwrap();
        let config = home.path().join(".config/fl/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        for contents in [None, Some(""), Some("not valid [toml"), Some("directory")] {
            if let Some(text) = contents {
                if text == "directory" {
                    std::fs::remove_file(&config).unwrap();
                    std::fs::create_dir(&config).unwrap();
                } else {
                    std::fs::write(&config, text).unwrap();
                }
            }
            let store = MemStore::default();
            store.add_project(root.to_str().unwrap()).unwrap();
            let mut request = req("ada", "assistant", "claude");
            request.project = "audio-suite".into();
            spin_up(&store, &env(&home), &request, "owner", Caller::Owner).unwrap();
            let record = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap();
            assert_eq!(record.cwd.as_deref(), root.to_str(), "config: {contents:?}");
        }
    }

    #[test]
    fn an_explicit_cwd_is_kept() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let mut request = req("felicity", "assistant", "claude");
        request.cwd = Some("/tmp/kept".into());
        spin_up(&store, &env(&home), &request, "owner", Caller::Owner).unwrap();
        let rec = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        assert_eq!(rec.cwd.as_deref(), Some("/tmp/kept"));
    }

    #[test]
    fn teardown_does_not_retire_a_persona_that_still_has_a_running_session() {
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
        let mut other = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        other.session_id = "codex:felicity".into();
        other.vendor = "codex".into();
        other.tmux_target = Some("Felicity:codex".into());
        other.status = STATUS_RUNNING.into();
        persist_hosted_session(&store, &other, "owner").unwrap();

        let claude = SessionTarget::resolve(&store, "claude:felicity", None).unwrap();
        teardown_session(&store, &env, &claude, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Active
        );

        let codex = SessionTarget::resolve(&store, "codex:felicity", None).unwrap();
        teardown_session(&store, &env, &codex, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Retired
        );
    }

    #[test]
    fn teardown_keeps_a_running_siblings_route_and_persona_queue() {
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
        let mut other = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        other.session_id = "codex:felicity".into();
        other.vendor = "codex".into();
        other.tmux_target = Some("Felicity:codex".into());
        other.status = STATUS_RUNNING.into();
        persist_hosted_session(&store, &other, "owner").unwrap();

        let mut routes = RouteTable::default();
        routes.upsert(
            "felicity",
            VendorRoute::Codex {
                thread_id: "thread-felicity".into(),
                cwd: None,
            },
            At::from_unix_millis(1),
        );
        let path = home
            .path()
            .join(".cache")
            .join("agent-bus")
            .join("routes.json");
        routes.save(Some(&path)).unwrap();

        let claude = SessionTarget::resolve(&store, "claude:felicity", None).unwrap();
        let mut cleared = Vec::new();
        teardown_session(&store, &env, &claude, "owner", Caller::Owner, |k| {
            cleared.push(k.to_string())
        })
        .unwrap();

        let table = RouteTable::load(Some(&path));
        let route = &table.get("felicity").expect("felicity route").route;
        assert!(
            matches!(
                route,
                VendorRoute::Codex {
                    thread_id,
                    cwd: None,
                } if thread_id == "thread-felicity"
            ),
            "{route:?}"
        );
        assert!(cleared.iter().any(|key| key == "claude:felicity"));
        assert!(
            !cleared
                .iter()
                .any(|key| key.eq_ignore_ascii_case("felicity"))
        );
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Active
        );
        let rec = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        assert_eq!(rec.status, STATUS_DECOMMISSIONED);
        assert!(
            !rec.tombstone_ids.iter().any(|id| id == "thread-felicity"),
            "a running sibling's thread is not a tombstone: {:?}",
            rec.tombstone_ids
        );
    }

    #[test]
    fn owner_teardown_stops_an_unmanaged_tmux_session_and_an_unmarked_muse_window() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::running(&["Ada"]);
        fake.open_foreign_window("muse:ada");
        let env = fake.env(home.path());

        let claude = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        let peer = teardown_session(&store, &env, &claude, "peer", Caller::Peer, |_| {});
        assert!(matches!(peer, Err(LifecycleError::Forbidden(_))));
        assert!(fake.is_live("Ada"));

        let out = teardown_session(&store, &env, &claude, "owner", Caller::Owner, |_| {}).unwrap();
        assert!(out.runtime_stopped);
        assert!(!fake.is_live("Ada"));
        assert!(
            fake.calls
                .borrow()
                .iter()
                .any(|call| call == "kill-owner Ada:claude")
        );

        let muse = SessionTarget::resolve(&store, "muse:ada", None).unwrap();
        let out = teardown_session(&store, &env, &muse, "owner", Caller::Owner, |_| {}).unwrap();
        assert!(out.runtime_stopped);
        assert!(!fake.is_live("muse:ada"));
        assert!(
            fake.calls
                .borrow()
                .iter()
                .any(|call| call == "kill-owner muse:ada")
        );
    }

    #[test]
    fn owner_teardown_of_one_vendor_leaves_another_vendors_window() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        fake.add_session_windows(&[("Ada", "grok"), ("Ada", "claude")]);
        let env = fake.env(home.path());

        let grok = SessionTarget::resolve(&store, "grok:ada", None).unwrap();
        let peer = teardown_session(&store, &env, &grok, "peer", Caller::Peer, |_| {});
        assert!(matches!(peer, Err(LifecycleError::Forbidden(_))));
        assert!(fake.is_live("Ada"));
        assert!(fake.has_session_window("Ada:claude"));
        assert!(fake.has_session_window("Ada:grok"));

        let out = teardown_session(&store, &env, &grok, "owner", Caller::Owner, |_| {}).unwrap();
        assert!(out.runtime_stopped);
        assert!(fake.is_live("Ada"), "session Ada stays");
        assert!(fake.has_session_window("Ada:claude"), "claude window stays");
        assert!(!fake.has_session_window("Ada:grok"), "grok window is gone");
        let calls = fake.calls.borrow();
        assert!(calls.iter().any(|call| call == "kill-owner Ada:grok"));
        assert!(
            !calls
                .iter()
                .any(|call| call == "kill-owner Ada" || call == "kill Ada")
        );
    }

    #[test]
    fn owner_teardown_leaves_a_running_siblings_runtime_up() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::running(&["Ada"]);
        let env = fake.env(home.path());
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(fake.is_live("Ada"));
        let grok = SessionTarget::resolve(&store, "grok:ada", None).unwrap();
        let out = teardown_session(&store, &env, &grok, "owner", Caller::Owner, |_| {}).unwrap();
        assert!(!out.runtime_stopped);
        assert!(fake.is_live("Ada"));
        assert!(
            !fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("kill-owner"))
        );
    }

    #[test]
    fn grok_spin_up_accepts_the_directory_trust_prompt() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        fake.panes.borrow_mut().insert(
            "Ada:grok".into(),
            "Do you trust the contents of this directory? Yes, proceed (y)".into(),
        );
        let env = fake.env(home.path());
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "grok"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(
            fake.calls
                .borrow()
                .iter()
                .any(|call| call == "send-keys Ada:grok y")
        );
    }

    #[test]
    fn grok_trust_poll_sends_y_once_after_two_misses_and_nothing_after_ten() {
        let mut seen = 0u32;
        let mut sent = Vec::new();
        poll_grok_directory_trust(
            "Ada:grok",
            Duration::ZERO,
            || {
                seen += 1;
                if seen <= 2 {
                    String::new()
                } else {
                    "Do you trust the contents of this directory?".into()
                }
            },
            |keys| sent.push(keys.to_string()),
        );
        assert_eq!(seen, 3, "a hit stops the poll");
        assert_eq!(sent, ["y".to_string()]);

        let mut misses = 0u32;
        let mut sent = Vec::new();
        poll_grok_directory_trust(
            "Ada:grok",
            Duration::ZERO,
            || {
                misses += 1;
                String::new()
            },
            |keys| sent.push(keys.to_string()),
        );
        assert_eq!(misses, 10);
        assert!(sent.is_empty());
    }

    #[test]
    fn claude_spin_up_dismisses_the_auto_mode_prompt() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        fake.panes.borrow_mut().insert(
            "Ada:claude".into(),
            "Teach auto mode about your environment?\n❯ Yes, open in external editor\n  Skip for now (Esc)".into(),
        );
        let env = fake.env(home.path());
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(
            fake.calls
                .borrow()
                .iter()
                .any(|call| call == "send-keys Ada:claude Escape")
        );
    }

    #[test]
    fn antigravity_spin_up_accepts_the_project_trust_prompt() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        fake.panes.borrow_mut().insert(
            "Ada:antigravity".into(),
            "Do you trust the contents of this project?\n● 1. Trust this project\n  2. Trust parent folder".into(),
        );
        let env = fake.env(home.path());
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "antigravity"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(
            fake.calls
                .borrow()
                .iter()
                .any(|call| call == "send-keys Ada:antigravity Enter")
        );
    }

    #[test]
    fn claude_auto_mode_poll_sends_escape_once_after_two_misses_and_nothing_after_ten() {
        let mut seen = 0u32;
        let mut sent = Vec::new();
        poll_claude_auto_mode_prompt(
            Duration::ZERO,
            || {
                seen += 1;
                if seen <= 2 {
                    String::new()
                } else {
                    "Teach auto mode about your environment?".into()
                }
            },
            |keys| sent.push(keys.to_string()),
        );
        assert_eq!(seen, 3, "a hit stops the poll");
        assert_eq!(sent, ["Escape".to_string()]);

        let mut misses = 0u32;
        let mut sent = Vec::new();
        poll_claude_auto_mode_prompt(
            Duration::ZERO,
            || {
                misses += 1;
                String::new()
            },
            |keys| sent.push(keys.to_string()),
        );
        assert_eq!(misses, 10);
        assert!(sent.is_empty());
    }

    #[test]
    fn hydration_prompt_does_not_contain_the_grok_trust_sentence() {
        let prompt = prompt_for("Ada", "FerroLoop", "assistant");
        assert!(!prompt.contains(GROK_DIRECTORY_TRUST_PROMPT), "{prompt}");
        let canonical = canonical_reforge_prompt("Ada", "FerroLoop", "assistant");
        assert!(!canonical.contains(GROK_DIRECTORY_TRUST_PROMPT));
    }

    #[test]
    fn spin_up_drops_a_stale_other_vendor_route_and_keeps_a_webhook() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let path = home
            .path()
            .join(".cache")
            .join("agent-bus")
            .join("routes.json");
        let mut routes = RouteTable::default();
        routes.upsert(
            "ada",
            VendorRoute::Codex {
                thread_id: "thread-ada".into(),
                cwd: None,
            },
            At::from_unix_millis(1),
        );
        routes.save(Some(&path)).unwrap();
        spin_up(
            &store,
            &env(&home),
            &req("ada", "assistant", "grok"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(RouteTable::load(Some(&path)).get("ada").is_none());

        let mut routes = RouteTable::default();
        routes.upsert(
            "bea",
            VendorRoute::Webhook {
                socket_path: home.path().join("none.sock"),
            },
            At::from_unix_millis(1),
        );
        routes.save(Some(&path)).unwrap();
        spin_up(
            &store,
            &env(&home),
            &req("bea", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(matches!(
            RouteTable::load(Some(&path)).get("bea").map(|e| &e.route),
            Some(VendorRoute::Webhook { .. })
        ));
    }

    fn save_claude_route(home: &Path, persona: &str, session_id: &str) -> PathBuf {
        let mut routes = RouteTable::load(Some(
            &home.join(".cache").join("agent-bus").join("routes.json"),
        ));
        routes.upsert(
            persona,
            VendorRoute::Claude {
                pid: 4_000_000_001,
                session_id: session_id.into(),
                socket_path: home.join("claude.sock"),
                key_path: None,
            },
            At::from_unix_millis(1),
        );
        let path = home.join(".cache").join("agent-bus").join("routes.json");
        routes.save(Some(&path)).unwrap();
        path
    }

    #[test]
    fn spin_up_keeps_a_running_siblings_route() {
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
        let path = save_claude_route(home.path(), "ada", "sess-claude");
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "grok"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(
            matches!(
                RouteTable::load(Some(&path)).get("ada").map(|entry| &entry.route),
                Some(VendorRoute::Claude { session_id, .. }) if session_id == "sess-claude"
            ),
            "a running claude sibling keeps its route"
        );

        spin_up(
            &store,
            &env,
            &req("bea", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let mut bea = find_hosted_session(&store, &["claude:bea"])
            .unwrap()
            .unwrap();
        bea.status = STATUS_DECOMMISSIONING.into();
        persist_hosted_session(&store, &bea, "owner").unwrap();
        save_claude_route(home.path(), "bea", "sess-bea");
        spin_up(
            &store,
            &env,
            &req("bea", "assistant", "grok"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(
            RouteTable::load(Some(&path)).get("bea").is_none(),
            "a decommissioning sibling does not protect the route"
        );
    }

    #[test]
    fn antigravity_teardown_removes_conversation_files_and_a_respin_clears_the_tombstone() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let id = "11111111-1111-1111-1111-111111111111";
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "antigravity"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let brain = home.path().join(".gemini/antigravity-cli/brain").join(id);
        let pb_dir = home.path().join(".gemini/antigravity/conversations");
        std::fs::create_dir_all(brain.join("logs")).unwrap();
        std::fs::write(brain.join("logs/transcript.jsonl"), "kept-then-removed").unwrap();
        std::fs::create_dir_all(&pb_dir).unwrap();
        std::fs::write(pb_dir.join(format!("{id}.pb")), "pb").unwrap();
        let mut routes = RouteTable::default();
        routes.upsert(
            "ada",
            VendorRoute::Agy {
                conversation_id: id.into(),
                ls_pid: None,
                ls_address: Some("127.0.0.1:9".into()),
                csrf_token: None,
                agentapi_exe: None,
            },
            At::from_unix_millis(1),
        );
        let path = home
            .path()
            .join(".cache")
            .join("agent-bus")
            .join("routes.json");
        routes.save(Some(&path)).unwrap();

        let target = SessionTarget::resolve(&store, "antigravity:ada", None).unwrap();
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert!(
            brain.join("logs/transcript.jsonl").is_file(),
            "teardown leaves the brain directory"
        );
        assert!(
            pb_dir.join(format!("{id}.pb")).is_file(),
            "teardown leaves the conversation file"
        );
        let banned = fl_wire::identity::load_decommissioned_session_ids(Some(home.path()));
        assert!(banned.contains(id), "{banned:?}");
        assert!(
            !banned.contains("antigravity:ada"),
            "the hosted name is not a tombstone: {banned:?}"
        );

        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "antigravity"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let banned = fl_wire::identity::load_decommissioned_session_ids(Some(home.path()));
        assert!(!banned.contains(id), "{banned:?}");
        assert!(!banned.contains("antigravity:ada"), "{banned:?}");
    }

    #[test]
    fn a_decommissioned_persona_reregistered_by_upsert_stays_discoverable() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        let old = "conv-old";
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "antigravity"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let mut routes = RouteTable::default();
        routes.upsert(
            "ada",
            VendorRoute::Agy {
                conversation_id: old.into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            At::from_unix_millis(1),
        );
        let path = home
            .path()
            .join(".cache")
            .join("agent-bus")
            .join("routes.json");
        routes.save(Some(&path)).unwrap();
        let target = SessionTarget::resolve(&store, "antigravity:ada", None).unwrap();
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        let banned = fl_wire::identity::load_decommissioned_session_ids(Some(home.path()));
        assert!(banned.contains(old), "{banned:?}");
        assert!(!banned.contains("antigravity:ada"), "{banned:?}");

        let new_source = crate::latency_tests::write_agy_identity(home.path(), "conv-new", "ada");
        let at = At::from_unix_millis(2);
        let mut table = RouteTable::default();
        table.upsert(
            "ada",
            VendorRoute::Agy {
                conversation_id: "conv-new".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            at.clone(),
        );
        table.discover_routes(Some(home.path()), at.clone());
        assert!(
            matches!(
                table.get("ada").map(|entry| &entry.route),
                Some(VendorRoute::Agy { conversation_id, .. }) if conversation_id == "conv-new"
            ),
            "a new native id stays deliverable"
        );

        std::fs::remove_file(new_source).unwrap();
        table.upsert(
            "ada",
            VendorRoute::Grok {
                session_id: String::new(),
                cwd: None,
                tmux_target: Some("Ada:grok".into()),
                tmux_pane: None,
            },
            at.clone(),
        );
        table.discover_routes(Some(home.path()), at.clone());
        assert!(
            matches!(
                table.get("ada").map(|entry| &entry.route),
                Some(VendorRoute::Grok { session_id, .. }) if session_id.is_empty()
            ),
            "an empty grok id stays"
        );

        crate::latency_tests::write_agy_identity(home.path(), old, "ada");
        table.upsert(
            "ada",
            VendorRoute::Agy {
                conversation_id: old.into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            at.clone(),
        );
        table.discover_routes(Some(home.path()), at);
        assert!(table.get("ada").is_none(), "the old native id stays banned");
    }

    #[test]
    fn refresh_drops_persona_derived_ids_left_on_an_old_record() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let mut rec = hosted_record("codex", "bob", STATUS_DECOMMISSIONED, false);
        rec.tombstone_ids = vec![
            "thread-bob".into(),
            "codex:bob".into(),
            "grok-bob".into(),
            "claude-bob-99".into(),
        ];
        persist_hosted_session(&store, &rec, "owner").unwrap();
        refresh_decommission_tombstones(&store, Some(home.path()));
        let banned = fl_wire::identity::load_decommissioned_session_ids(Some(home.path()));
        assert_eq!(
            banned,
            std::collections::BTreeSet::from(["thread-bob".to_string()])
        );
    }

    fn script_presence(
        persona: &str,
        vendor: &str,
        presence: RuntimePresence,
    ) -> (MemStore, FakeRuntime, TempDir) {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let mut rec = hosted_record(vendor, persona, STATUS_RUNNING, false);
        rec.tmux_target = Some(hosted_tmux_target(vendor, persona));
        persist_hosted_session(&store, &rec, "owner").unwrap();
        fake.presence_of
            .borrow_mut()
            .insert(hosted_tmux_target(vendor, persona), presence);
        (store, fake, home)
    }

    fn hosted_status(store: &MemStore, session_id: &str) -> String {
        find_hosted_session(store, &[session_id])
            .unwrap()
            .unwrap()
            .status
    }

    #[test]
    fn one_gone_observation_leaves_running_and_the_second_stops() {
        let (store, fake, home) = script_presence("streakgone", "grok", RuntimePresence::Gone);
        let env = fake.env(home.path());
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(hosted_status(&store, "grok:streakgone"), STATUS_RUNNING);
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(hosted_status(&store, "grok:streakgone"), STATUS_STOPPED);
        let banned = fl_wire::identity::load_decommissioned_session_ids(Some(home.path()));
        assert!(banned.is_empty(), "{banned:?}");
        let calls = fake.calls.borrow();
        assert!(calls.is_empty(), "reconcile does not kill tmux: {calls:?}");
    }

    #[test]
    fn an_unknown_between_gone_observations_does_not_count() {
        let (store, fake, home) = script_presence("streakunk", "grok", RuntimePresence::Gone);
        let env = fake.env(home.path());
        let target = hosted_tmux_target("grok", "streakunk");
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(hosted_status(&store, "grok:streakunk"), STATUS_RUNNING);
        fake.presence_of
            .borrow_mut()
            .insert(target.clone(), RuntimePresence::Unknown);
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(hosted_status(&store, "grok:streakunk"), STATUS_RUNNING);
        fake.presence_of
            .borrow_mut()
            .insert(target, RuntimePresence::Gone);
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(hosted_status(&store, "grok:streakunk"), STATUS_STOPPED);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_present_route_never_stops_the_record() {
        let (store, fake, home) = script_presence("streakroute", "grok", RuntimePresence::Gone);
        let env = fake.env(home.path());
        let mut rec = find_hosted_session(&store, &["grok:streakroute"])
            .unwrap()
            .unwrap();
        rec.pid = Some(std::process::id());
        rec.pid_start_time = fl_wire::identity::proc_start_time_ticks(std::process::id());
        persist_hosted_session(&store, &rec, "owner").unwrap();
        let mut routes = RouteTable::default();
        routes.upsert(
            "streakroute",
            VendorRoute::Grok {
                session_id: "gs-live".into(),
                cwd: None,
                tmux_target: Some("Streakroute:grok".into()),
                tmux_pane: None,
            },
            At::from_unix_millis(1),
        );
        let path = home
            .path()
            .join(".cache")
            .join("agent-bus")
            .join("routes.json");
        routes.save(Some(&path)).unwrap();
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(hosted_status(&store, "grok:streakroute"), STATUS_RUNNING);
    }

    #[test]
    fn a_transient_tmux_failure_does_not_stop_a_session() {
        let (store, fake, home) = script_presence("streakmiss", "grok", RuntimePresence::Unknown);
        let env = fake.env(home.path());
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(hosted_status(&store, "grok:streakmiss"), STATUS_RUNNING);
    }

    #[test]
    fn an_api_vendor_is_not_stopped_for_lack_of_tmux() {
        let (store, fake, home) = script_presence("streakapi", "openai-api", RuntimePresence::Gone);
        let env = fake.env(home.path());
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(
            hosted_status(&store, "openai-api:streakapi"),
            STATUS_RUNNING
        );
    }

    #[test]
    fn teardown_retires_a_persona_whose_only_sibling_is_decommissioning() {
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
        let mut other = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        other.session_id = "codex:felicity".into();
        other.vendor = "codex".into();
        other.tmux_target = Some("Felicity:codex".into());
        // `decommissioning` is live to `is_live_hosted`, but it must not block retire.
        other.status = STATUS_DECOMMISSIONING.into();
        persist_hosted_session(&store, &other, "owner").unwrap();

        let claude = SessionTarget::resolve(&store, "claude:felicity", None).unwrap();
        teardown_session(&store, &env, &claude, "owner", Caller::Owner, |_| {}).unwrap();
        let rec = find_hosted_session(&store, &["claude:felicity"])
            .unwrap()
            .unwrap();
        assert_eq!(rec.status, STATUS_DECOMMISSIONED);
        assert_eq!(
            store.agent_by_name("felicity").unwrap().unwrap().status,
            AgentStatus::Retired
        );
    }

    #[test]
    fn launch_path_prepends_the_user_local_bin_once() {
        let home = TempDir::new().unwrap();
        let spec = launch_spec_for(
            &env(&home),
            "codex",
            "felicia",
            None,
            None,
            "Take up residence.",
        )
        .unwrap();
        let path = spec
            .env
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.as_str())
            .unwrap();
        let local = home.path().join(".local/bin");
        let local_bin = local.to_string_lossy();
        assert!(path.starts_with(local_bin.as_ref()), "{path}");
        let components = |value: &str| {
            std::env::split_paths(value)
                .filter(|entry| entry == &local)
                .count()
        };
        assert_eq!(components(path), 1, "{path}");
        // Process PATH is shared with other hub tests. A string that already
        // contains this directory once must come back unchanged.
        let seeded = format!("{local_bin}:/usr/bin");
        let again = with_local_bin(&local, &seeded);
        assert_eq!(again, seeded);
        assert_eq!(components(&again), 1, "{again}");
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
        assert!(out.prompt.contains("--project FerroLoop"));
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
    fn owner_prepare_adopts_a_live_route_over_a_decommissioned_record() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        stale_ada(&store, &env);
        webhook_route_file(home.path(), "ada");
        let listener =
            std::os::unix::net::UnixListener::bind(home.path().join("none.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        // Resolved by persona, as FerroDeck does: the only hosted record is stale history.
        let target = SessionTarget::resolve(&store, "ada", None).unwrap();
        assert_eq!(target.hosted.as_ref().unwrap().status, "decommissioned");

        // A Peer on the active agent is still refused, and adopts nothing.
        let err = prepare_decommission(&store, &env, &target, "peer", Caller::Peer).unwrap_err();
        assert_eq!(err.status(), 403);
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
        assert!(listener.accept().is_err(), "no prompt may be delivered");

        let out = prepare_decommission(&store, &env, &target, "owner", Caller::Owner).unwrap();
        assert_eq!(out.status, "decommissioning");
        assert!(
            out.handshake_dispatched,
            "the live route took the handshake"
        );
        assert!(listener.accept().is_ok(), "the handshake was delivered");
        // A new adopted record is decommissioning; the stale record stays history.
        let adopted = list_hosted_sessions(&store)
            .unwrap()
            .into_iter()
            .find(|r| r.session_id == out.session_id)
            .unwrap();
        assert_eq!(adopted.status, "decommissioning");
        assert_ne!(adopted.session_id, "claude:ada");
        assert_eq!(
            record_status(&store, "claude:ada"),
            Some("decommissioned".to_string())
        );
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 2);
    }

    #[test]
    fn owner_prepare_of_a_decommissioned_record_without_a_live_route_still_conflicts() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        stale_ada(&store, &env);
        let target = SessionTarget::resolve(&store, "ada", None).unwrap();
        let err = prepare_decommission(&store, &env, &target, "owner", Caller::Owner).unwrap_err();
        assert_eq!(err.status(), 409);
        assert_eq!(list_hosted_sessions(&store).unwrap().len(), 1);
        assert_eq!(
            record_status(&store, "claude:ada"),
            Some("decommissioned".to_string())
        );
    }

    fn record_status(store: &MemStore, id: &str) -> Option<String> {
        list_hosted_sessions(store)
            .unwrap()
            .into_iter()
            .find(|r| r.session_id == id)
            .map(|r| r.status)
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
        let tmux_target = Some(format!("{}:{vendor}", capitalize(persona)));
        let host_session_id = host_session_id_for_target(tmux_target.as_deref());
        HostedSessionRecord {
            session_id: format!("{vendor}:{persona}"),
            persona: persona.into(),
            project: "FerroLoop".into(),
            role: "assistant".into(),
            vendor: vendor.into(),
            model: None,
            thinking_level: None,
            posture: fl_core::permissions::Posture::Auto,
            node_id: DEFAULT_NODE_ID.into(),
            hosted: true,
            status: status.into(),
            stop_reason: None,
            restart_policy: "on-failure".into(),
            restart_count: 0,
            tmux_target,
            host_session_id,
            cwd: None,
            context_window: None,
            hydration_dispatched: false,
            hydration_status: crate::lifecycle::HydrationStatus::Unconfirmed,
            hydration_generation: None,
            runtime_spawned: spawned,
            tombstone_ids: Vec::new(),
            vendor_session_id: None,
            pid: None,
            runtime_leader_pid: None,
            pid_start_time: None,
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

    #[test]
    fn codex_launch_env_omits_agent_persona_while_other_vendors_include_it() {
        let home = TempDir::new().unwrap();
        let codex_env = launch_env_for_vendor("codex", "fiona", Some(home.path()));
        assert!(
            codex_env.iter().all(|(k, _)| k != "AGENT_PERSONA"),
            "codex shares one app-server daemon and must not set AGENT_PERSONA: {codex_env:?}"
        );
        for vendor in ["claude", "grok", "antigravity", "muse"] {
            let env = launch_env_for_vendor(vendor, "fiona", Some(home.path()));
            assert!(
                env.iter()
                    .any(|(k, v)| k == "AGENT_PERSONA" && v == "fiona"),
                "{vendor} must include AGENT_PERSONA: {env:?}"
            );
            assert_eq!(
                env.iter()
                    .any(|(k, v)| k == "MUSE_NO_AUTO_UPDATE" && v == "1"),
                vendor == "muse",
                "{vendor} MUSE_NO_AUTO_UPDATE mismatch: {env:?}"
            );
        }
    }

    #[test]
    fn teardown_of_adopted_native_uuid_also_decommissions_same_vendor_sibling_record_and_retires_agent()
     {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let _trusted = trusted_codex_home(home.path(), &std::env::current_dir().unwrap());
        let fake = FakeRuntime::default();
        let env = LifecycleEnv {
            runtime: &fake,
            spawn_runtime: true,
            ..env(&home)
        };
        spin_up(
            &store,
            &env,
            &req("fiona", "assistant", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();

        let native_id = "019d8f34-89cd-7e43-8a66-31f3b49788b7";
        let mut routes = RouteTable::default();
        routes.upsert(
            "fiona",
            VendorRoute::Codex {
                thread_id: native_id.into(),
                cwd: None,
            },
            At::from_unix_millis(100),
        );
        let rpath = home.path().join(".cache/agent-bus/routes.json");
        routes.save(Some(&rpath)).unwrap();

        // Simulate prepare_decommission having persisted an adopted native-UUID record alongside
        // the original `codex:fiona` record.
        let mut adopted = hosted_record("codex", "fiona", STATUS_DECOMMISSIONING, false);
        adopted.session_id = native_id.to_string();
        adopted.tmux_target = None;
        adopted.runtime_spawned = false;
        persist_hosted_session(&store, &adopted, "owner").unwrap();

        let target = SessionTarget::resolve(
            &store,
            native_id,
            Some((native_id.to_string(), Some("fiona".to_string()))),
        )
        .unwrap();
        assert_eq!(target.session_id, native_id);

        let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert!(
            out.runtime_stopped,
            "inherited sibling runtime must be stopped"
        );

        let all = list_hosted_sessions(&store).unwrap();
        assert_eq!(all.len(), 2, "{all:?}");
        for rec in &all {
            assert_eq!(rec.status, STATUS_DECOMMISSIONED, "{rec:?}");
            assert!(!rec.runtime_spawned, "{rec:?}");
            assert!(
                rec.tombstone_ids.iter().any(|id| id == native_id),
                "{rec:?}"
            );
        }
        assert_eq!(
            store.agent_by_name("fiona").unwrap().unwrap().status,
            AgentStatus::Retired
        );
        let retired = fl_wire::identity::load_retired_personas(Some(home.path()));
        assert!(retired.contains("fiona"), "{retired:?}");
    }

    #[test]
    fn pre_existing_native_tombstones_survive_refresh_and_spin_up_and_filter_discovery() {
        let store = fl_store::AnyStore::in_memory();
        let home = TempDir::new().unwrap();
        let env = env(&home);

        // Seed a pre-existing native session ID in decommissioned-sessions.json that has no
        // corresponding hosted-session-* record in the store, alongside a persona-derived ID.
        fl_wire::identity::save_decommissioned_session_ids(
            Some(home.path()),
            &BTreeSet::from([
                "conv-orphan-tombstone".to_string(),
                "codex:ghost".to_string(),
            ]),
        )
        .unwrap();

        refresh_decommission_tombstones(&store, Some(home.path()));
        let after_refresh = fl_wire::identity::load_decommissioned_session_ids(Some(home.path()));
        assert!(
            after_refresh.contains("conv-orphan-tombstone"),
            "pre-existing native tombstone must survive refresh: {after_refresh:?}"
        );
        assert!(
            !after_refresh.contains("codex:ghost"),
            "non-native persona-derived tombstone must still be cleaned up: {after_refresh:?}"
        );

        // Spin up and decommission a hosted session, then re-spin it: its own old tombstone
        // is dropped on re-spin while the orphan native tombstone remains.
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "antigravity"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let mut routes = RouteTable::default();
        routes.upsert(
            "ada",
            VendorRoute::Agy {
                conversation_id: "conv-ada-old".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            At::from_unix_millis(10),
        );
        routes.upsert(
            "orphan",
            VendorRoute::Agy {
                conversation_id: "conv-orphan-tombstone".into(),
                ls_pid: None,
                ls_address: None,
                csrf_token: None,
                agentapi_exe: None,
            },
            At::from_unix_millis(10),
        );
        let rpath = home.path().join(".cache/agent-bus/routes.json");
        routes.save(Some(&rpath)).unwrap();

        let target = SessionTarget::resolve(&store, "antigravity:ada", None).unwrap();
        teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        let after_teardown = fl_wire::identity::load_decommissioned_session_ids(Some(home.path()));
        assert!(after_teardown.contains("conv-orphan-tombstone"));
        assert!(after_teardown.contains("conv-ada-old"));

        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "antigravity"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let after_respin = fl_wire::identity::load_decommissioned_session_ids(Some(home.path()));
        assert!(
            after_respin.contains("conv-orphan-tombstone"),
            "orphan tombstone must survive spin_up of another session: {after_respin:?}"
        );
        assert!(
            !after_respin.contains("conv-ada-old"),
            "re-spun session's own old tombstone must be dropped: {after_respin:?}"
        );

        let snap = crate::discovery::snapshot_discovery_store(&store);
        let discovered =
            crate::discovery::discover_route_sessions_without_model_info(&snap, Some(home.path()));
        assert!(
            !discovered.iter().any(|s| s.id == "conv-orphan-tombstone"),
            "discovery must skip pre-existing sidecar tombstone: {discovered:?}"
        );
    }

    #[test]
    fn teardown_of_agy_route_issues_delete_cascade_trajectory_against_language_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeRuntime {
            shared_events: Some(Arc::clone(&events)),
            ..FakeRuntime::default()
        };
        let fake_env = fake.env(home.path());
        spin_up(
            &store,
            &fake_env,
            &req("ada", "assistant", "antigravity"),
            "owner",
            Caller::Owner,
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let ls_addr = listener.local_addr().unwrap().to_string();
        let captured_req = Arc::new(Mutex::new(String::new()));
        let captured_srv = Arc::clone(&captured_req);
        let events_srv = Arc::clone(&events);
        let server_handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(1500)));
                let mut buf = [0u8; 4096];
                let mut raw = Vec::new();
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            raw.extend_from_slice(&buf[..n]);
                            if let Some(hdr_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                                let headers = String::from_utf8_lossy(&raw[..hdr_end]);
                                let content_len = headers
                                    .lines()
                                    .find_map(|line| {
                                        let (k, v) = line.split_once(':')?;
                                        k.trim()
                                            .eq_ignore_ascii_case("content-length")
                                            .then(|| v.trim().parse::<usize>().ok())
                                            .flatten()
                                    })
                                    .unwrap_or(0);
                                if raw.len() >= hdr_end + 4 + content_len {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                events_srv.lock().unwrap().push("delete_rpc");
                *captured_srv.lock().unwrap() = String::from_utf8_lossy(&raw).to_string();
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            }
        });

        let mut routes = RouteTable::default();
        routes.upsert(
            "ada",
            VendorRoute::Agy {
                conversation_id: "conv-ada-cascade-159".into(),
                ls_pid: None,
                ls_address: Some(ls_addr),
                csrf_token: Some("csrf-secret-159".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(10),
        );
        let rpath = home.path().join(".cache/agent-bus/routes.json");
        routes.save(Some(&rpath)).unwrap();

        let target = SessionTarget::resolve(&store, "antigravity:ada", None).unwrap();
        let out =
            teardown_session(&store, &fake_env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(out.status, "decommissioned");
        assert!(out.runtime_stopped);
        server_handle.join().unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec!["delete_rpc", "runtime_kill"],
            "DeleteCascadeTrajectory RPC must run before the runtime process is killed"
        );

        let raw_http = captured_req.lock().unwrap().clone();
        assert!(
            raw_http.starts_with(
                "POST /exa.language_server_pb.LanguageServerService/DeleteCascadeTrajectory HTTP/1.1"
            ),
            "expected DeleteCascadeTrajectory POST, got: {raw_http:?}"
        );
        let lower_http = raw_http.to_ascii_lowercase();
        assert!(lower_http.contains("connect-protocol-version: 1"));
        assert!(lower_http.contains("content-type: application/json"));
        assert!(
            lower_http.contains("x-codeium-csrf-token: csrf-secret-159"),
            "expected CSRF token header in request: {raw_http:?}"
        );
        let body = raw_http
            .split_once("\r\n\r\n")
            .map(|(_, b)| b)
            .unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).expect("JSON request body");
        assert_eq!(
            parsed.get("cascadeId").and_then(|v| v.as_str()),
            Some("conv-ada-cascade-159")
        );
    }

    #[test]
    fn teardown_of_agy_route_logs_and_continues_when_language_server_is_missing_or_unreachable() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "antigravity"),
            "owner",
            Caller::Owner,
        )
        .unwrap();

        let dead_addr = "127.0.0.1:1".to_string();

        let err =
            post_agy_delete_cascade_trajectory(&dead_addr, Some("csrf-dead"), "conv-ada-dead-159")
                .expect_err("unreachable language server must return error for logging");
        assert!(!err.is_empty());

        let mut routes = RouteTable::default();
        routes.upsert(
            "ada",
            VendorRoute::Agy {
                conversation_id: "conv-ada-dead-159".into(),
                ls_pid: None,
                ls_address: Some(dead_addr),
                csrf_token: Some("csrf-dead".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(10),
        );
        let rpath = home.path().join(".cache/agent-bus/routes.json");
        routes.save(Some(&rpath)).unwrap();

        let target = SessionTarget::resolve(&store, "antigravity:ada", None).unwrap();
        let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(out.status, "decommissioned");
        assert!(RouteTable::load(Some(&rpath)).get("ada").is_none());
        let tombstones = fl_wire::identity::load_decommissioned_session_ids(Some(home.path()));
        assert!(tombstones.contains("conv-ada-dead-159"));
    }

    #[test]
    fn teardown_of_antigravity_session_delete_cascade_trajectory_refuses_non_loopback_addresses() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();

        for bad_addr in [
            "10.0.0.5:1234".to_string(),
            format!("10.0.0.5:{port}"),
            "https://127.0.0.1:1234".to_string(),
            format!("https://127.0.0.1:{port}"),
            "https://localhost:1234".to_string(),
            format!("https://localhost:{port}"),
        ] {
            let err = post_agy_delete_cascade_trajectory(
                &bad_addr,
                Some("csrf-secret-159"),
                "conv-ada-non-loopback-159",
            )
            .expect_err("non-loopback or scheme-prefixed address must be refused");
            assert!(
                err.contains("non-loopback language-server address"),
                "unexpected error for {bad_addr:?}: {err:?}"
            );
            assert!(
                listener.accept().is_err(),
                "no TCP request should be sent for {bad_addr:?}"
            );
        }

        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "antigravity"),
            "owner",
            Caller::Owner,
        )
        .unwrap();

        let mut routes = RouteTable::default();
        routes.upsert(
            "ada",
            VendorRoute::Agy {
                conversation_id: "conv-ada-non-loopback-159".into(),
                ls_pid: None,
                ls_address: Some(format!("https://127.0.0.1:{port}")),
                csrf_token: Some("csrf-secret-159".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(10),
        );
        let rpath = home.path().join(".cache/agent-bus/routes.json");
        routes.save(Some(&rpath)).unwrap();

        let target = SessionTarget::resolve(&store, "antigravity:ada", None).unwrap();
        let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(out.status, "decommissioned");
        assert!(
            listener.accept().is_err(),
            "teardown must not dial non-loopback or scheme-prefixed LS address"
        );
    }

    #[test]
    fn teardown_of_antigravity_session_bounds_delete_cascade_trajectory_loop_when_ls_stalls() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let env = env(&home);
        spin_up(
            &store,
            &env,
            &req("ada", "assistant", "antigravity"),
            "owner",
            Caller::Owner,
        )
        .unwrap();

        let mut rec = find_hosted_session(&store, &["antigravity:ada"])
            .unwrap()
            .unwrap();
        rec.tombstone_ids = vec![
            "conv-ada-stall-2".into(),
            "conv-ada-stall-3".into(),
            "conv-ada-stall-4".into(),
        ];
        persist_hosted_session(&store, &rec, "owner").unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let ls_addr = listener.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_srv = Arc::clone(&accepted);
        let server_handle = std::thread::spawn(move || {
            let mut held_streams = Vec::new();
            while !stop_srv.load(Ordering::Relaxed) {
                if let Ok((stream, _)) = listener.accept() {
                    accepted_srv.fetch_add(1, Ordering::Relaxed);
                    held_streams.push(stream);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });

        let mut routes = RouteTable::default();
        routes.upsert(
            "ada",
            VendorRoute::Agy {
                conversation_id: "conv-ada-stall-1".into(),
                ls_pid: None,
                ls_address: Some(ls_addr),
                csrf_token: Some("csrf-stall-159".into()),
                agentapi_exe: None,
            },
            At::from_unix_millis(10),
        );
        let rpath = home.path().join(".cache/agent-bus/routes.json");
        routes.save(Some(&rpath)).unwrap();

        let target = SessionTarget::resolve(&store, "antigravity:ada", None).unwrap();
        let started = std::time::Instant::now();
        let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        let elapsed = started.elapsed();
        stop.store(true, Ordering::Relaxed);
        server_handle.join().unwrap();

        assert_eq!(out.status, "decommissioned");
        assert!(
            elapsed < std::time::Duration::from_millis(3500),
            "expected stalled LS teardown within 3.5s overall deadline, took {elapsed:?}"
        );
        assert_eq!(
            accepted.load(Ordering::Relaxed),
            1,
            "loop must stop after the first stalled transport timeout"
        );
    }

    #[test]
    fn teardown_of_agy_route_with_home_override_ignores_env_language_server() {
        struct RestoreAgyEnv {
            addr: Option<std::ffi::OsString>,
            csrf: Option<std::ffi::OsString>,
        }
        impl Drop for RestoreAgyEnv {
            fn drop(&mut self) {
                // SAFETY: caller holds `with_probe_lock`.
                unsafe {
                    match &self.addr {
                        Some(v) => std::env::set_var("ANTIGRAVITY_LS_ADDRESS", v),
                        None => std::env::remove_var("ANTIGRAVITY_LS_ADDRESS"),
                    }
                    match &self.csrf {
                        Some(v) => std::env::set_var("ANTIGRAVITY_CSRF_TOKEN", v),
                        None => std::env::remove_var("ANTIGRAVITY_CSRF_TOKEN"),
                    }
                }
            }
        }

        crate::direct_quota::with_probe_lock(|| {
            let _restore = RestoreAgyEnv {
                addr: std::env::var_os("ANTIGRAVITY_LS_ADDRESS"),
                csrf: std::env::var_os("ANTIGRAVITY_CSRF_TOKEN"),
            };

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let env_addr = listener.local_addr().unwrap().to_string();
            // SAFETY: serialized by `with_probe_lock` and restored on drop.
            unsafe {
                std::env::set_var("ANTIGRAVITY_LS_ADDRESS", &env_addr);
                std::env::set_var("ANTIGRAVITY_CSRF_TOKEN", "csrf-env-159");
            }

            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let env = env(&home);
            spin_up(
                &store,
                &env,
                &req("ada", "assistant", "antigravity"),
                "owner",
                Caller::Owner,
            )
            .unwrap();

            let mut routes = RouteTable::default();
            routes.upsert(
                "ada",
                VendorRoute::Agy {
                    conversation_id: "conv-ada-no-addr-159".into(),
                    ls_pid: None,
                    ls_address: None,
                    csrf_token: None,
                    agentapi_exe: None,
                },
                At::from_unix_millis(10),
            );
            let rpath = home.path().join(".cache/agent-bus/routes.json");
            routes.save(Some(&rpath)).unwrap();

            let target = SessionTarget::resolve(&store, "antigravity:ada", None).unwrap();
            let out =
                teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
            assert_eq!(out.status, "decommissioned");
            assert!(
                listener.accept().is_err(),
                "home_override must not fall back to ANTIGRAVITY_LS_ADDRESS"
            );
        });
    }

    #[test]
    fn claude_spin_up_restart_and_teardown_route_through_vendor_host_adapter_with_session_uuid() {
        use fl_core::vendor_adapter::{
            AdapterReadiness, AttachOrResumeOutcome, AttachOrResumeRequest, ConfigEffectiveScope,
            TeardownOutcome, TeardownTarget, TranscriptNormalizeReport, TurnDeliveryOutcome,
            TurnDeliveryRequest, VendorAdapterError, VendorConfigSpec, VendorHostAdapter,
            VendorHostAdapterRegistry, VendorLaunchPlan, VendorLaunchRequest,
            VendorStartupPromptRule,
        };
        use std::sync::{Arc, Mutex};

        #[derive(Debug, Default)]
        struct StubClaudeHostAdapter {
            archived: Mutex<Vec<(Option<String>, Option<PathBuf>)>>,
        }

        impl VendorHostAdapter for StubClaudeHostAdapter {
            fn vendor_id(&self) -> &str {
                "claude"
            }
            fn default_binary(&self) -> &str {
                "claude"
            }
            fn binary_env_var(&self) -> Option<&str> {
                Some("FL_WIRE_CLAUDE_BIN")
            }
            fn probe_readiness(
                &self,
                _binding: &fl_core::AdapterIdentityBinding,
                _vendor_session_id: Option<&str>,
                _control_endpoint: Option<&str>,
            ) -> Result<AdapterReadiness, VendorAdapterError> {
                Ok(AdapterReadiness::ReadyIdle)
            }
            fn prepare_launch(
                &self,
                req: &VendorLaunchRequest,
            ) -> Result<VendorLaunchPlan, VendorAdapterError> {
                let sid = req.binding.vendor_session_id.clone().ok_or_else(|| {
                    VendorAdapterError::MissingSessionIdentity {
                        vendor: "claude".into(),
                        reason: "missing session id".into(),
                    }
                })?;
                let display = capitalize(&req.persona);
                let mut argv = vec![
                    "claude".to_string(),
                    "--remote-control".to_string(),
                    "--enable-auto-mode".to_string(),
                    "--session-id".to_string(),
                    sid,
                    "--name".to_string(),
                    display,
                ];
                if !req.initial_prompt.is_empty() {
                    argv.push(req.initial_prompt.clone());
                }
                Ok(VendorLaunchPlan {
                    vendor_id: "claude".into(),
                    binary: "claude".into(),
                    argv,
                    env: vec![("AGENT_PERSONA".into(), req.persona.clone())],
                    cwd: req.cwd.clone(),
                    remote_control: req.remote_control,
                    startup_rules: Vec::<VendorStartupPromptRule>::new(),
                })
            }
            fn prepare_resume(
                &self,
                req: &AttachOrResumeRequest,
            ) -> Result<VendorLaunchPlan, VendorAdapterError> {
                let persona = req.persona.clone().unwrap_or_else(|| "kayla".into());
                let display = capitalize(&persona);
                let argv = vec![
                    "claude".to_string(),
                    "--remote-control".to_string(),
                    "--enable-auto-mode".to_string(),
                    "--resume".to_string(),
                    req.vendor_session_id.clone(),
                    "--name".to_string(),
                    display,
                ];
                Ok(VendorLaunchPlan {
                    vendor_id: "claude".into(),
                    binary: "claude".into(),
                    argv,
                    env: vec![("AGENT_PERSONA".into(), persona)],
                    cwd: req.cwd.clone(),
                    remote_control: req.remote_control,
                    startup_rules: Vec::<VendorStartupPromptRule>::new(),
                })
            }
            fn validate_config(
                &self,
                model: Option<&str>,
                thinking_effort: Option<&str>,
            ) -> Result<VendorConfigSpec, VendorAdapterError> {
                Ok(VendorConfigSpec {
                    normalized_model: model.map(str::to_string),
                    normalized_thinking_effort: thinking_effort.map(str::to_string),
                    model_scope: ConfigEffectiveScope::NextLaunch,
                    thinking_effort_scope: ConfigEffectiveScope::NextLaunch,
                })
            }
            fn attach_or_resume(
                &self,
                _req: &AttachOrResumeRequest,
            ) -> Result<AttachOrResumeOutcome, VendorAdapterError> {
                unreachable!()
            }
            fn submit_turn(&self, _req: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
                unreachable!()
            }
            fn steer_turn(&self, _req: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
                unreachable!()
            }
            fn cancel_turn(&self, _req: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
                unreachable!()
            }
            fn normalize_transcript(
                &self,
                _raw_jsonl: &str,
                _after_cursor: Option<&str>,
            ) -> Result<TranscriptNormalizeReport, VendorAdapterError> {
                unreachable!()
            }
            fn stop(
                &self,
                _target: &TeardownTarget,
            ) -> Result<TeardownOutcome, VendorAdapterError> {
                Ok(TeardownOutcome::Completed)
            }
            fn detach(
                &self,
                _target: &TeardownTarget,
            ) -> Result<TeardownOutcome, VendorAdapterError> {
                Ok(TeardownOutcome::Completed)
            }
            fn archive(
                &self,
                target: &TeardownTarget,
            ) -> Result<TeardownOutcome, VendorAdapterError> {
                self.archived
                    .lock()
                    .unwrap()
                    .push((target.vendor_session_id.clone(), target.home_dir.clone()));
                Ok(TeardownOutcome::Completed)
            }
            fn delete(
                &self,
                _target: &TeardownTarget,
            ) -> Result<TeardownOutcome, VendorAdapterError> {
                unreachable!()
            }
        }

        struct ResetRegistry;
        impl Drop for ResetRegistry {
            fn drop(&mut self) {
                TEST_HOST_ADAPTER_REGISTRY.with(|slot| *slot.borrow_mut() = None);
            }
        }

        let stub = Arc::new(StubClaudeHostAdapter::default());
        let reg = VendorHostAdapterRegistry::new();
        reg.register(stub.clone());
        TEST_HOST_ADAPTER_REGISTRY.with(|slot| *slot.borrow_mut() = Some(reg));
        let _reset = ResetRegistry;

        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = fake.env(home.path());

        let out = spin_up(
            &store,
            &env,
            &req("kayla", "lead", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(out.runtime_spawned);

        let rec = find_hosted_session(&store, &["claude:kayla"])
            .unwrap()
            .unwrap();
        let sid = rec
            .vendor_session_id
            .clone()
            .expect("claude spin_up must persist vendor_session_id");
        assert!(uuid::Uuid::parse_str(&sid).is_ok(), "invalid uuid: {sid}");

        let launches = fake.launches.borrow().clone();
        assert_eq!(launches.len(), 1);
        let spec = launches[0].1.as_ref().expect("launch spec present");
        assert_eq!(
            &spec.argv[..7],
            &[
                "claude".to_string(),
                "--remote-control".to_string(),
                "--enable-auto-mode".to_string(),
                "--session-id".to_string(),
                sid.clone(),
                "--name".to_string(),
                "Kayla".to_string(),
            ]
        );

        // Restart reuses the existing vendor_session_id when hydration is confirmed and active transcript exists.
        record_hydration_ack(
            &store,
            "kayla",
            true,
            rec.hydration_generation.as_deref(),
            Some("claude:kayla"),
        )
        .unwrap();
        let proj_dir = home.path().join(".claude/projects/test");
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(
            proj_dir.join(format!("{sid}.jsonl")),
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"{sid}\",\"uuid\":\"u1\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n\
                 {{\"type\":\"assistant\",\"sessionId\":\"{sid}\",\"uuid\":\"a1\",\"message\":{{\"model\":\"claude-opus-5-5\",\"role\":\"assistant\",\"stop_reason\":\"end_turn\",\"usage\":{{\"input_tokens\":10,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":100}},\"content\":[{{\"type\":\"text\",\"text\":\"ok\"}}]}}}}\n"
            ),
        )
        .unwrap();
        fake.clear();
        let restarted = restart_hosted_session(&store, &env, "claude:kayla", "owner").unwrap();
        assert_eq!(
            restarted.record.vendor_session_id.as_deref(),
            Some(sid.as_str())
        );

        // Active running record removes its vendor_session_id from the decommission sidecar
        // even when refresh_decommission_tombstones_dropping is called with dropped_ids = &[].
        let seeded_ids = BTreeSet::from([sid.clone()]);
        fl_wire::identity::save_decommissioned_tombstones(
            Some(home.path()),
            &seeded_ids,
            &BTreeSet::new(),
        )
        .unwrap();
        refresh_decommission_tombstones_dropping(&store, Some(home.path()), &[]);
        assert!(
            !fl_wire::identity::load_decommissioned_session_ids(Some(home.path())).contains(&sid),
            "running record vendor_session_id must be removed from tombstones"
        );

        // Teardown includes vendor_session_id in tombstones, record_native_ids, and archive()
        // even when no route file exists.
        let target = SessionTarget::resolve(&store, "claude:kayla", None).unwrap();
        let torn = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        assert_eq!(torn.status, STATUS_DECOMMISSIONED);

        let after_rec = find_hosted_session(&store, &["claude:kayla"])
            .unwrap()
            .unwrap();
        assert!(after_rec.tombstone_ids.contains(&sid));
        assert!(record_native_ids(&after_rec).contains(&sid));
        let archived = stub.archived.lock().unwrap().clone();
        assert!(
            archived.contains(&(Some(sid), Some(home.path().to_path_buf()))),
            "expected archive called with vendor_session_id and home_override, got {archived:?}"
        );

        // When env.home_override is None and env.spawn_runtime is true (production path),
        // teardown_session resolves home_dir from $HOME and passes it to adapter.archive().
        struct RestoreHome(Option<std::ffi::OsString>);
        impl Drop for RestoreHome {
            fn drop(&mut self) {
                // SAFETY: serialized by with_probe_lock and TMUX_ENV_LOCK.
                unsafe {
                    match &self.0 {
                        Some(v) => std::env::set_var("HOME", v),
                        None => std::env::remove_var("HOME"),
                    }
                }
            }
        }
        crate::direct_quota::with_probe_lock(|| {
            let _tmux_lock = crate::TMUX_ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let _restore_home = RestoreHome(std::env::var_os("HOME"));
            // SAFETY: serialized by with_probe_lock and TMUX_ENV_LOCK; restored by _restore_home.
            unsafe {
                std::env::set_var("HOME", home.path());
            }
            spin_up(
                &store,
                &env,
                &req("kacey", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let kacey_sid = find_hosted_session(&store, &["claude:kacey"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            stub.archived.lock().unwrap().clear();
            let prod_env = LifecycleEnv {
                home_override: None,
                ..env
            };
            let target = SessionTarget::resolve(&store, "claude:kacey", None).unwrap();
            teardown_session(&store, &prod_env, &target, "owner", Caller::Owner, |_| {}).unwrap();
            let archived = stub.archived.lock().unwrap().clone();
            assert!(
                archived.contains(&(Some(kacey_sid), Some(home.path().to_path_buf()))),
                "expected archive called with $HOME fallback when home_override is None, got {archived:?}"
            );
        });
    }

    #[test]
    fn host_runtime_capture_prefers_capture_rendered_for_pty_and_falls_back_on_err() {
        use fl_host_proto::{
            ChildRecord, ChildState, ExtensionRequest, ExtensionResponse, HostConfig, HostEndpoint,
            HostInstance, HostListener, HostRequest, HostResponse, InputState, RenderedScreen,
            SessionEpoch, SessionId, SessionMode, SessionSummary, SpawnKind, StreamId,
        };
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let _lock = crate::TMUX_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let sock_path = tmp.path().join("fl-host-cap.sock");
        let endpoint = HostEndpoint::unix(&sock_path).unwrap();
        let sid = SessionId::new("Kayla.claude").unwrap();
        let summary = SessionSummary {
            stream: StreamId {
                host: HostInstance::new([2u8; 16]),
                session: sid.clone(),
                epoch: SessionEpoch(1),
            },
            mode: SessionMode::Pty,
            state: ChildState::Running(ChildRecord {
                pid: std::process::id(),
                session: sid,
                session_epoch: SessionEpoch(1),
                kind: SpawnKind::Pty,
                input: InputState::Writable,
                started_at_ms: 1000,
                exit: None,
                consumer_stalled: false,
            }),
            acked: 0,
            latest: 0,
            input: InputState::Writable,
        };
        let fail_rendered = Arc::new(AtomicBool::new(false));
        let fail_srv = Arc::clone(&fail_rendered);
        let ep_srv = endpoint.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut cfg = HostConfig::new(ep_srv);
                cfg.minor = 1;
                let mut listener = HostListener::bind(cfg).await.unwrap();
                let _ = ready_tx.send(());
                while let Ok(mut conn) = listener.accept().await {
                    let summary = summary.clone();
                    let fail_srv = Arc::clone(&fail_srv);
                    tokio::spawn(async move {
                        while let Ok(req) = conn.recv().await {
                            let HostRequest::Extended { id, request } = req else {
                                break;
                            };
                            let resp = match request {
                                ExtensionRequest::ListSessions => ExtensionResponse::Sessions {
                                    records: vec![summary.clone()],
                                },
                                ExtensionRequest::Capabilities => ExtensionResponse::Capabilities {
                                    host: HostInstance::new([2u8; 16]),
                                    limits: fl_host_proto::ExtensionLimits::default(),
                                    features: if fail_srv.load(Ordering::Relaxed) {
                                        0
                                    } else {
                                        fl_host_proto::RENDERED_PTY_FEATURE
                                    },
                                },
                                ExtensionRequest::CaptureRendered { stream } => {
                                    ExtensionResponse::RenderedCapture {
                                        screen: RenderedScreen {
                                            schema: 1,
                                            stream,
                                            contents: "Teach auto mode about your environment"
                                                .into(),
                                            cursor_row: 0,
                                            cursor_col: 0,
                                            rows: 24,
                                            cols: 80,
                                            generation: 1,
                                            output_seq: 1,
                                            output_bytes: 1,
                                        },
                                    }
                                }
                                ExtensionRequest::CaptureOutput { .. } => {
                                    ExtensionResponse::CapturedOutput {
                                        text: "fallback raw output".into(),
                                    }
                                }
                                _ => break,
                            };
                            if conn
                                .respond(&HostResponse::Extended { id, response: resp })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                }
            });
        });
        ready_rx.recv().unwrap();

        let prev_sock = std::env::var_os("FL_HOST_SOCK");
        let prev_tmux = std::env::var_os("FL_WIRE_TMUX_BIN");
        // SAFETY: TMUX_ENV_LOCK is held; both variables are restored below.
        unsafe {
            std::env::set_var("FL_HOST_SOCK", &sock_path);
            std::env::remove_var("FL_WIRE_TMUX_BIN");
        }
        let rendered_out = SessionRuntime::capture(&HostRuntime, "Kayla:claude");
        fail_rendered.store(true, Ordering::Relaxed);
        let fallback_out = SessionRuntime::capture(&HostRuntime, "Kayla:claude");
        // SAFETY: see above.
        unsafe {
            match prev_sock {
                Some(v) => std::env::set_var("FL_HOST_SOCK", v),
                None => std::env::remove_var("FL_HOST_SOCK"),
            }
            if let Some(v) = prev_tmux {
                std::env::set_var("FL_WIRE_TMUX_BIN", v);
            }
        }
        assert_eq!(rendered_out, "Teach auto mode about your environment");
        assert_eq!(fallback_out, "fallback raw output");
        drop(server_thread);
    }

    #[test]
    fn host_adapter_prepare_launch_fails_closed_on_permission_refusal_and_falls_back_on_non_permission_error()
     {
        use fl_core::permissions::Posture;
        use fl_core::{
            AdapterIdentityBinding, AdapterReadiness, AttachOrResumeOutcome, AttachOrResumeRequest,
            ConfigEffectiveScope, TeardownOutcome, TeardownTarget, TranscriptNormalizeReport,
            TurnDeliveryOutcome, TurnDeliveryRequest, VendorAdapterError, VendorConfigSpec,
            VendorHostAdapter, VendorHostAdapterRegistry, VendorLaunchPlan, VendorLaunchRequest,
        };
        use std::sync::Arc;

        #[derive(Debug)]
        struct StubHostAdapter {
            vendor: &'static str,
            err: Option<VendorAdapterError>,
        }

        impl VendorHostAdapter for StubHostAdapter {
            fn vendor_id(&self) -> &str {
                self.vendor
            }
            fn default_binary(&self) -> &str {
                self.vendor
            }
            fn prepare_launch(
                &self,
                _req: &VendorLaunchRequest,
            ) -> Result<VendorLaunchPlan, VendorAdapterError> {
                match &self.err {
                    Some(err) => Err(err.clone()),
                    None => Ok(VendorLaunchPlan {
                        vendor_id: self.vendor.into(),
                        binary: self.vendor.into(),
                        argv: vec![self.vendor.into()],
                        env: vec![("CODEX_HOME".into(), "/wrong-adapter-default".into())],
                        cwd: _req.cwd.clone(),
                        remote_control: false,
                        startup_rules: Vec::new(),
                    }),
                }
            }
            fn validate_config(
                &self,
                model: Option<&str>,
                thinking_effort: Option<&str>,
            ) -> Result<VendorConfigSpec, VendorAdapterError> {
                Ok(VendorConfigSpec {
                    normalized_model: model.map(str::to_string),
                    normalized_thinking_effort: thinking_effort.map(str::to_string),
                    model_scope: ConfigEffectiveScope::NextLaunch,
                    thinking_effort_scope: ConfigEffectiveScope::NextLaunch,
                })
            }
            fn attach_or_resume(
                &self,
                _req: &AttachOrResumeRequest,
            ) -> Result<AttachOrResumeOutcome, VendorAdapterError> {
                unreachable!()
            }
            fn probe_readiness(
                &self,
                _binding: &AdapterIdentityBinding,
                _vendor_session_id: Option<&str>,
                _control_endpoint: Option<&str>,
            ) -> Result<AdapterReadiness, VendorAdapterError> {
                Ok(AdapterReadiness::Unknown)
            }
            fn submit_turn(&self, _req: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
                unreachable!()
            }
            fn steer_turn(&self, _req: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
                unreachable!()
            }
            fn cancel_turn(&self, _req: &TurnDeliveryRequest) -> TurnDeliveryOutcome {
                unreachable!()
            }
            fn normalize_transcript(
                &self,
                _raw_jsonl: &str,
                _after_cursor: Option<&str>,
            ) -> Result<TranscriptNormalizeReport, VendorAdapterError> {
                unreachable!()
            }
            fn stop(
                &self,
                _target: &TeardownTarget,
            ) -> Result<TeardownOutcome, VendorAdapterError> {
                Ok(TeardownOutcome::Completed)
            }
            fn detach(
                &self,
                _target: &TeardownTarget,
            ) -> Result<TeardownOutcome, VendorAdapterError> {
                Ok(TeardownOutcome::Completed)
            }
            fn archive(
                &self,
                _target: &TeardownTarget,
            ) -> Result<TeardownOutcome, VendorAdapterError> {
                Ok(TeardownOutcome::Completed)
            }
            fn delete(
                &self,
                _target: &TeardownTarget,
            ) -> Result<TeardownOutcome, VendorAdapterError> {
                Ok(TeardownOutcome::Completed)
            }
        }

        struct ResetRegistry;
        impl Drop for ResetRegistry {
            fn drop(&mut self) {
                TEST_HOST_ADAPTER_REGISTRY.with(|slot| *slot.borrow_mut() = None);
            }
        }
        let _reset = ResetRegistry;

        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = fake.env(home.path());

        // 1. PermissionRefused from host adapter never falls back to CLI adapter, even if CLI
        //    adapter supports that posture (`claude` + `Posture::Plan`).
        let reg = VendorHostAdapterRegistry::new();
        reg.register(Arc::new(StubHostAdapter {
            vendor: "claude",
            err: Some(VendorAdapterError::PermissionRefused {
                vendor: "claude".into(),
                reason: "host adapter refused posture".into(),
            }),
        }));
        TEST_HOST_ADAPTER_REGISTRY.with(|slot| *slot.borrow_mut() = Some(reg));

        let store = MemStore::default();
        let mut r = req("ada", "assistant", "claude");
        r.posture = Posture::Plan;
        let err = spin_up(&store, &env, &r, "owner", Caller::Owner).unwrap_err();
        assert!(matches!(err, LifecycleError::Invalid(_)), "{err:?}");
        assert!(err.to_string().contains("host adapter refused posture"));
        assert!(
            store.agent_by_name("ada").unwrap().is_none(),
            "permission refusal in launch_spec_for_with_session must abort before registering agent"
        );
        assert!(list_hosted_sessions(&store).unwrap().is_empty());
        assert!(fake.launches.borrow().is_empty());

        // 2. Non-permission error from host adapter (e.g. InvalidConfig on a window-tagged model
        //    for Codex) falls back to the CLI adapter when the CLI adapter can launch with the
        //    same posture, never producing a blank runtime (`None`).
        let reg2 = VendorHostAdapterRegistry::new();
        reg2.register(Arc::new(StubHostAdapter {
            vendor: "codex",
            err: Some(VendorAdapterError::InvalidConfig {
                vendor: "codex".into(),
                reason: "invalid Codex model id `opus[1m]`".into(),
            }),
        }));
        TEST_HOST_ADAPTER_REGISTRY.with(|slot| *slot.borrow_mut() = Some(reg2));

        let mut codex_req = req("ada", "assistant", "codex");
        codex_req.model = Some("opus[1m]".into());
        codex_req.posture = Posture::Bypass;
        let _config_env = IsolatedCodexHome::new();
        let err = spin_up(&store, &env, &codex_req, "owner", Caller::Owner).unwrap_err();
        assert!(err.to_string().contains("trust_level"));
        assert!(store.agent_by_name("ada").unwrap().is_none());
        assert!(fake.launches.borrow().is_empty());
        let _trusted = trusted_codex_home(home.path(), &std::env::current_dir().unwrap());
        let out = spin_up(&store, &env, &codex_req, "owner", Caller::Owner).unwrap();
        assert!(out.runtime_spawned);
        let launches = fake.launches.borrow();
        assert_eq!(launches.len(), 1);
        let spec = launches[0]
            .1
            .as_ref()
            .expect("built-in vendor must never spawn a blank runtime");
        assert!(
            spec.argv.windows(2).any(|w| w == ["-a", "never"]),
            "CLI fallback must preserve requested Bypass posture: {:?}",
            spec.argv
        );
        assert!(
            !spec.argv.iter().any(|a| a == "--model"),
            "CLI fallback must normalize out window-tagged model on codex: {:?}",
            spec.argv
        );
        assert!(spec.env.contains(&(
            "CODEX_HOME".into(),
            home.path().join(".codex").display().to_string()
        )));
        assert_eq!(
            fake.spawn_cwds.borrow().last().unwrap().as_deref(),
            std::env::current_dir()
                .unwrap()
                .canonicalize()
                .unwrap()
                .to_str()
        );
        drop(launches);

        // 3. Non-permission error from host adapter when the CLI adapter cannot launch with the
        //    requested posture (`codex` + `Posture::Ask`) fails closed with `LifecycleError::Invalid`.
        let err_ask = launch_spec_for_with_session(
            &env,
            "codex",
            "ada",
            Some("opus[1m]"),
            None,
            Posture::Ask,
            "Hydrate",
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(err_ask, LifecycleError::Invalid(_)), "{err_ask:?}");

        // A successful host adapter cannot override the checked configuration either.
        let successful = VendorHostAdapterRegistry::new();
        successful.register(Arc::new(StubHostAdapter {
            vendor: "codex",
            err: None,
        }));
        TEST_HOST_ADAPTER_REGISTRY.with(|slot| *slot.borrow_mut() = Some(successful));
        let host_request = req("pat", "assistant", "codex");
        spin_up(&store, &env, &host_request, "owner", Caller::Owner).unwrap();
        let launches = fake.launches.borrow();
        let host_spec = launches.last().unwrap().1.as_ref().unwrap();
        assert_eq!(
            host_spec
                .env
                .iter()
                .filter(|(key, _)| key == "CODEX_HOME")
                .collect::<Vec<_>>(),
            vec![&(
                "CODEX_HOME".into(),
                home.path().join(".codex").display().to_string()
            )]
        );
        drop(launches);

        // 4. Non-permission error from a host adapter that has no CLI adapter fallback surfaces
        //    `LifecycleError::Invalid` instead of `Ok(None)`.
        let reg3 = VendorHostAdapterRegistry::new();
        reg3.register(Arc::new(StubHostAdapter {
            vendor: "host-only-vendor",
            err: Some(VendorAdapterError::Transport {
                vendor: "host-only-vendor".into(),
                reason: "control socket unreachable".into(),
            }),
        }));
        TEST_HOST_ADAPTER_REGISTRY.with(|slot| *slot.borrow_mut() = Some(reg3));
        let err_no_cli = launch_spec_for_with_session(
            &env,
            "host-only-vendor",
            "ada",
            None,
            None,
            Posture::Auto,
            "Hydrate",
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err_no_cli, LifecycleError::Invalid(_)),
            "{err_no_cli:?}"
        );
        assert!(
            err_no_cli
                .to_string()
                .contains("control socket unreachable")
        );

        // 5. Non-permission error from host adapter when a `vendor_session_id` is bound (such as
        //    Claude spin-up) must NOT fall back to the CLI adapter, because the CLI fallback lacks
        //    `--session-id` / `--name` and would leave an orphaned session on teardown.
        let reg4 = VendorHostAdapterRegistry::new();
        reg4.register(Arc::new(StubHostAdapter {
            vendor: "claude",
            err: Some(VendorAdapterError::InvalidConfig {
                vendor: "claude".into(),
                reason: "invalid Claude model id".into(),
            }),
        }));
        TEST_HOST_ADAPTER_REGISTRY.with(|slot| *slot.borrow_mut() = Some(reg4));

        let claude_store = MemStore::default();
        let claude_fake = FakeRuntime::default();
        let claude_env = claude_fake.env(home.path());
        let mut claude_req = req("bea", "assistant", "claude");
        claude_req.model = Some("bad/model".into());
        let err_claude = spin_up(
            &claude_store,
            &claude_env,
            &claude_req,
            "owner",
            Caller::Owner,
        )
        .unwrap_err();
        assert!(
            matches!(err_claude, LifecycleError::Invalid(_)),
            "{err_claude:?}"
        );
        assert!(err_claude.to_string().contains("invalid Claude model id"));
        assert!(
            claude_store.agent_by_name("bea").unwrap().is_none(),
            "Claude host-adapter error must abort before registering agent"
        );
        assert!(list_hosted_sessions(&claude_store).unwrap().is_empty());
        assert!(claude_fake.live.borrow().is_empty());
        assert!(claude_fake.calls.borrow().is_empty());
        assert!(claude_fake.launches.borrow().is_empty());
    }

    #[test]
    fn spin_up_restart_and_remodel_read_configured_auto_approval_setting() {
        use fl_core::permissions::ReceiptClass;

        let home = tempfile::tempdir().unwrap();
        let _trusted = trusted_codex_home(home.path(), &std::env::current_dir().unwrap());
        let store = MemStore::default();
        let fake = FakeRuntime::default();
        let env = fake.env(home.path());

        // 1. Default settings (`vendor-reviewer`): Codex `Posture::Auto` launches with
        //    `--approve-for-me` (never `-a` or `-s`) and returns `Exact` receipts.
        let codex_req = req("ada", "assistant", "codex");
        let spun = spin_up(&store, &env, &codex_req, "owner", Caller::Owner).unwrap();
        assert!(
            spun.permission_receipts
                .iter()
                .all(|r| r.class == ReceiptClass::Exact),
            "default vendor-reviewer receipts must be Exact: {:?}",
            spun.permission_receipts
        );
        {
            let launches = fake.launches.borrow();
            assert_eq!(launches.len(), 1);
            let spec = launches[0].1.as_ref().unwrap();
            assert!(
                spec.argv.iter().any(|a| a == "--approve-for-me"),
                "expected --approve-for-me in {:?}",
                spec.argv
            );
            assert!(
                spec.argv.iter().all(|a| !matches!(
                    a.as_str(),
                    "-a" | "--ask-for-approval" | "-s" | "--sandbox"
                )),
                "Codex vendor-reviewer must never emit -a or -s: {:?}",
                spec.argv
            );
        }

        // 2. Update settings to `ask-human` for Codex and Muse; restart reads the updated setting.
        let patch: crate::settings::SettingsPatch = serde_json::from_value(serde_json::json!({
            "auto_approval": {
                "codex": "ask-human",
                "muse": "ask-human"
            }
        }))
        .unwrap();
        crate::settings::update(home.path(), patch).unwrap();

        fake.live.borrow_mut().clear();
        restart_hosted_session(&store, &env, "codex:ada", "owner").unwrap();
        {
            let launches = fake.launches.borrow();
            assert_eq!(launches.len(), 2);
            let spec = launches[1].1.as_ref().unwrap();
            assert!(
                spec.argv.windows(2).any(|w| w == ["-a", "on-request"]),
                "restart with ask-human must emit -a on-request: {:?}",
                spec.argv
            );
            assert!(
                spec.argv.windows(2).any(|w| w == ["-s", "workspace-write"]),
                "restart with ask-human must emit -s workspace-write: {:?}",
                spec.argv
            );
            assert!(!spec.argv.iter().any(|a| a == "--approve-for-me"));
        }

        // 3. Remodel onto Muse with `ask-human` configured emits `--approval-judge off` and
        //    records `StricterDowngrade` on the Posture receipt (both Prepare and Execute).
        let remodel_target = SessionTarget::resolve(&store, "codex:ada", None).unwrap();
        let prepared = remodel_session(
            &store,
            &env,
            &remodel_target,
            &RemodelRequest {
                vendor: "muse".into(),
                mode: Some("prepare".into()),
                ..Default::default()
            },
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        assert!(
            prepared
                .permission_receipts
                .iter()
                .any(|r| r.class == ReceiptClass::StricterDowngrade
                    && r.requested.contains("ask-human")),
            "Muse Prepare ask-human receipt must record StricterDowngrade: {:?}",
            prepared.permission_receipts
        );
        let remodeled = remodel_session(
            &store,
            &env,
            &remodel_target,
            &RemodelRequest {
                vendor: "muse".into(),
                ..Default::default()
            },
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        assert!(
            remodeled
                .permission_receipts
                .iter()
                .any(|r| r.class == ReceiptClass::StricterDowngrade
                    && r.requested.contains("ask-human")),
            "Muse ask-human receipt must record StricterDowngrade: {:?}",
            remodeled.permission_receipts
        );
        {
            let launches = fake.launches.borrow();
            let spec = launches.last().unwrap().1.as_ref().unwrap();
            assert!(
                spec.argv
                    .windows(2)
                    .any(|w| w == ["--approval-judge", "off"]),
                "Muse ask-human must emit --approval-judge off: {:?}",
                spec.argv
            );
        }
    }

    #[test]
    fn spin_up_and_remodel_expose_host_session_id() {
        let home = tempfile::tempdir().unwrap();
        let _trusted = trusted_codex_home(home.path(), &std::env::current_dir().unwrap());
        let store = MemStore::default();
        let fake = FakeRuntime::default();
        let mut env = fake.env(home.path());
        env.key_probe = &RestoreKeys;

        let spun = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(spun.session_id, "codex:ada");
        assert_eq!(spun.tmux_target.as_deref(), Some("Ada:codex"));
        assert_eq!(spun.host_session_id.as_deref(), Some("Ada.codex"));
        let stored_codex = find_hosted_session(&store, &["codex:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(stored_codex.host_session_id.as_deref(), Some("Ada.codex"));

        let target = SessionTarget::resolve(&store, "codex:ada", None).unwrap();
        let prepared = remodel_session(
            &store,
            &env,
            &target,
            &RemodelRequest {
                vendor: "muse".into(),
                mode: Some("prepare".into()),
                ..Default::default()
            },
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        assert_eq!(prepared.host_session_id.as_deref(), Some("Ada.codex"));

        let remodeled = remodel_session(
            &store,
            &env,
            &target,
            &RemodelRequest {
                vendor: "muse".into(),
                ..Default::default()
            },
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        assert_eq!(remodeled.session_id, "muse:ada");
        assert_eq!(remodeled.tmux_target.as_deref(), Some("muse:ada"));
        assert_eq!(remodeled.host_session_id.as_deref(), Some("muse.ada"));
        let stored_muse = find_hosted_session(&store, &["muse:ada"]).unwrap().unwrap();
        assert_eq!(stored_muse.host_session_id.as_deref(), Some("muse.ada"));

        // API-only vendor session (`tmux_target: None`) populates `host_session_id: None` across
        // SpinUpResponse, HostedSessionRecord, and RemodelResponse (both Prepare and Execute).
        let api_spun = spin_up(
            &store,
            &env,
            &req("bea", "assistant", "kimi-api"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(api_spun.tmux_target, None);
        assert_eq!(api_spun.host_session_id, None);
        let api_rec = find_hosted_session(&store, &["kimi-api:bea"])
            .unwrap()
            .unwrap();
        assert_eq!(api_rec.tmux_target, None);
        assert_eq!(api_rec.host_session_id, None);

        let api_target = SessionTarget::resolve(&store, "kimi-api:bea", None).unwrap();
        let api_prepared = remodel_session(
            &store,
            &env,
            &api_target,
            &RemodelRequest {
                vendor: "openrouter-api".into(),
                mode: Some("prepare".into()),
                ..Default::default()
            },
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        assert_eq!(api_prepared.tmux_target, None);
        assert_eq!(api_prepared.host_session_id, None);

        let api_remodeled = remodel_session(
            &store,
            &env,
            &api_target,
            &RemodelRequest {
                vendor: "openrouter-api".into(),
                ..Default::default()
            },
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        assert_eq!(api_remodeled.tmux_target, None);
        assert_eq!(api_remodeled.host_session_id, None);
        let api_rec_after = find_hosted_session(&store, &["openrouter-api:bea"])
            .unwrap()
            .unwrap();
        assert_eq!(api_rec_after.tmux_target, None);
        assert_eq!(api_rec_after.host_session_id, None);

        // Legacy HostedSessionRecord JSON serialized without `host_session_id` deserializes cleanly
        // and backfills `host_session_id` when read via `list_hosted_sessions`.
        let mut legacy_json = serde_json::to_value(&stored_muse).unwrap();
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("host_session_id");
        let deserialized: HostedSessionRecord =
            serde_json::from_value(legacy_json.clone()).unwrap();
        assert_eq!(deserialized.host_session_id, None);

        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: hosted_session_slug("muse:ada"),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Runtime(RuntimeEntry {
                    value: legacy_json,
                    merge_mode: MergeMode::LastWriter,
                }),
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap();
        let listed = list_hosted_sessions(&store).unwrap();
        let backfilled = listed.iter().find(|r| r.session_id == "muse:ada").unwrap();
        assert_eq!(backfilled.host_session_id.as_deref(), Some("muse.ada"));
    }

    #[test]
    fn next_hydration_generation_is_strictly_monotonic_even_within_same_millisecond_and_validates_tokens()
     {
        let mut prev = next_hydration_generation();
        assert!(is_valid_hydration_generation(&prev), "{prev}");
        for _ in 0..512 {
            let cur = next_hydration_generation();
            assert!(is_valid_hydration_generation(&cur), "{cur}");
            assert!(prev < cur, "expected {prev} < {cur}");
            assert!(
                validate_slug(&hydration_ack_slug(&cur)).is_ok(),
                "hydration-ack slug must be valid for {cur}"
            );
            prev = cur;
        }
        // Force a clock tie/regression by bumping LAST_HYDRATION_GENERATION ahead by 10ms.
        let future_ms = (now_ms().saturating_add(10) & 0xffff_ffff_ffff) << 16;
        LAST_HYDRATION_GENERATION.fetch_max(future_ms, std::sync::atomic::Ordering::SeqCst);
        let g1 = next_hydration_generation();
        let g2 = next_hydration_generation();
        let g3 = next_hydration_generation();
        assert!(is_valid_hydration_generation(&g1));
        assert!(is_valid_hydration_generation(&g2));
        assert!(is_valid_hydration_generation(&g3));
        assert!(g1 < g2 && g2 < g3, "expected {g1} < {g2} < {g3}");

        for bad in [
            "",
            "not-a-uuid",
            "019d8f34-89cd-4e43-8a66-31f3b49788b7", // version 4, not 7
            "019d8f34-89cd-7e43-0a66-31f3b49788b7", // variant 0, not 8..=b
            "019D8F34-89CD-7E43-8A66-31F3B49788B7", // uppercase
            "../019d8f34-89cd-7e43-8a66-31f3b49788",
        ] {
            assert!(
                !is_valid_hydration_generation(bad),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn channel_binding_and_codex_prompt_channel_transition_unconfirmed_to_confirmed() {
        assert!(
            fl_core::child_env::BASELINE_EXACT.contains(&"AGENT_HYDRATION_GENERATION"),
            "BASELINE_EXACT must allow AGENT_HYDRATION_GENERATION"
        );
        let home = TempDir::new().unwrap();
        let _trusted = trusted_codex_home(home.path(), &std::env::current_dir().unwrap());

        // Static preview without generation keeps Step 6 unchanged.
        let preview = canonical_reforge_prompt("Ada", "FerroLoop", "assistant");
        assert!(
            preview.contains(
                "AGENT_PERSONA=ada fl wire register Ada --project FerroLoop --role assistant --replace"
            ),
            "{preview}"
        );
        assert!(
            !preview.contains("AGENT_HYDRATION_GENERATION="),
            "static preview must not mint a generation: {preview}"
        );

        // Per-session CLI vendors receive AGENT_HYDRATION_GENERATION in launch env; codex omits it.
        let g_sample = next_hydration_generation();
        let codex_env = fl_wire::vendor_adapter::launch_env_for_vendor_with_generation(
            "codex",
            "ada",
            Some(home.path()),
            Some(&g_sample),
        );
        assert!(
            codex_env
                .iter()
                .all(|(k, _)| k != "AGENT_HYDRATION_GENERATION" && k != "AGENT_PERSONA"),
            "codex launch env must omit AGENT_HYDRATION_GENERATION: {codex_env:?}"
        );
        for vendor in ["claude", "antigravity", "muse", "grok"] {
            let env_vars = fl_wire::vendor_adapter::launch_env_for_vendor_with_generation(
                vendor,
                "ada",
                Some(home.path()),
                Some(&g_sample),
            );
            assert!(
                env_vars
                    .iter()
                    .any(|(k, v)| k == "AGENT_HYDRATION_GENERATION" && v == &g_sample),
                "{vendor} launch env must carry AGENT_HYDRATION_GENERATION: {env_vars:?}"
            );
        }

        // End-to-end Codex spin_up + Step 6 execution transitions unconfirmed -> confirmed.
        let store = MemStore::default();
        let fake = FakeRuntime::default();
        let env = LifecycleEnv {
            runtime: &fake,
            spawn_runtime: true,
            ..env(&home)
        };
        let out = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(out.hydration_status, HydrationStatus::Unconfirmed);
        let generation = out.hydration_generation.clone().expect("generation minted");
        assert!(is_valid_hydration_generation(&generation));
        let launches = fake.launches.borrow();
        let codex_spec = launches[0].1.as_ref().expect("codex launch spec");
        assert!(
            codex_spec
                .env
                .iter()
                .all(|(k, _)| k != "AGENT_HYDRATION_GENERATION"),
            "spawned codex spec must omit AGENT_HYDRATION_GENERATION: {:?}",
            codex_spec.env
        );
        let prompt = out.prompt.as_deref().expect("prompt");
        let expected_step6 = format!(
            "AGENT_PERSONA=ada AGENT_HYDRATION_GENERATION={generation} fl wire register Ada --project FerroLoop --role assistant --replace"
        );
        assert!(
            prompt.contains(&expected_step6),
            "prompt missing {expected_step6:?}: {prompt}"
        );

        // Extract AGENT_HYDRATION_GENERATION=<G> from the prompt line and execute registration ack.
        let extracted_gen = prompt
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix("AGENT_HYDRATION_GENERATION="))
            .expect("generation token in prompt");
        assert_eq!(extracted_gen, generation);
        fence_hydration_register(&store, "ada", Some(extracted_gen)).unwrap();
        assert!(record_hydration_ack(&store, "ada", true, Some(extracted_gen), None).unwrap());
        let after = find_hosted_session(&store, &["codex:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(after.hydration_status, HydrationStatus::Confirmed);
    }

    #[test]
    fn hydration_ordering_projection_duplicate_idempotence_stale_fencing_and_70_restart_pruning() {
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = LifecycleEnv {
            runtime: &fake,
            spawn_runtime: true,
            ..env(&home)
        };

        // Case 1: Early acknowledgement before post-spawn persist_hosted_session promotes record.
        let store1 = MemStore::default();
        let g_early = next_hydration_generation();
        assert!(
            record_hydration_ack(&store1, "ada", true, Some(&g_early), Some("claude:ada"),)
                .unwrap()
        );
        let mut early_rec = hosted_record("claude", "ada", STATUS_RUNNING, true);
        early_rec.hydration_dispatched = true;
        early_rec.hydration_status = HydrationStatus::Unconfirmed;
        early_rec.hydration_generation = Some(g_early.clone());
        persist_hosted_session(&store1, &early_rec, "owner").unwrap();
        let raw_entry = store1
            .get_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                &hosted_session_slug("claude:ada"),
            )
            .unwrap()
            .unwrap();
        let StatePayload::Runtime(rt) = raw_entry.payload else {
            panic!("expected runtime payload");
        };
        let persisted_rec: HostedSessionRecord = serde_json::from_value(rt.value).unwrap();
        assert_eq!(
            persisted_rec.hydration_status,
            HydrationStatus::Confirmed,
            "Case 1: post-spawn persist_hosted_session must promote status to Confirmed"
        );

        // Case 2: Normal delayed ack observed across list_hosted_sessions, find_hosted_session,
        // set_hosted_session_model, and set_hosted_session_posture without another spin_up/restart.
        let store2 = MemStore::default();
        let spun = spin_up(
            &store2,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let g1 = spun.hydration_generation.clone().unwrap();
        assert_eq!(spun.hydration_status, HydrationStatus::Unconfirmed);
        assert!(record_hydration_ack(&store2, "ada", true, Some(&g1), Some("claude:ada")).unwrap());
        assert_eq!(
            list_hosted_sessions(&store2).unwrap()[0].hydration_status,
            HydrationStatus::Confirmed
        );
        assert_eq!(
            find_hosted_session(&store2, &["claude:ada"])
                .unwrap()
                .unwrap()
                .hydration_status,
            HydrationStatus::Confirmed
        );
        assert_eq!(
            set_hosted_session_model(&store2, &["claude:ada"], "opus", Some("high"), "owner")
                .unwrap()
                .unwrap()
                .hydration_status,
            HydrationStatus::Confirmed
        );
        assert_eq!(
            set_hosted_session_posture(
                &store2,
                &["claude:ada"],
                fl_core::permissions::Posture::Plan,
                "owner",
            )
            .unwrap()
            .unwrap()
            .hydration_status,
            HydrationStatus::Confirmed
        );

        // Case 3: Duplicate ack is idempotent and preserves accepted_at_ms.
        let ack_entry_1 = store2
            .get_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                &hydration_ack_slug(&g1),
            )
            .unwrap()
            .unwrap();
        let StatePayload::Runtime(rt1) = ack_entry_1.payload else {
            panic!("expected runtime");
        };
        let receipt1: HydrationReceipt = serde_json::from_value(rt1.value).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        assert!(
            !record_hydration_ack(&store2, "ada", true, Some(&g1), Some("claude:ada")).unwrap(),
            "duplicate ack must not rewrite receipt"
        );
        let ack_entry_2 = store2
            .get_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                &hydration_ack_slug(&g1),
            )
            .unwrap()
            .unwrap();
        let StatePayload::Runtime(rt2) = ack_entry_2.payload else {
            panic!("expected runtime");
        };
        let receipt2: HydrationReceipt = serde_json::from_value(rt2.value).unwrap();
        assert_eq!(receipt1.accepted_at_ms, receipt2.accepted_at_ms);

        // Qualification: replace=false or mismatched FL_HOSTED_ID never writes a receipt.
        let g_unqual = next_hydration_generation();
        assert!(!record_hydration_ack(&store2, "ada", false, Some(&g_unqual), None).unwrap());
        assert!(
            !record_hydration_ack(&store2, "ada", true, Some(&g_unqual), Some("claude:other"),)
                .unwrap()
        );
        assert!(
            store2
                .get_state_by_key(
                    &StateScope::Workspace,
                    StateConcern::Runtime,
                    &hydration_ack_slug(&g_unqual),
                )
                .unwrap()
                .is_none()
        );

        // Case 4 & 70-restart pruning across 70 consecutive restart cycles.
        let mut prev_gen = g1.clone();
        for _ in 0..70 {
            fake.clear();
            let restarted = restart_hosted_session(&store2, &env, "claude:ada", "owner").unwrap();
            let cur_gen = restarted.record.hydration_generation.clone().unwrap();
            assert!(prev_gen < cur_gen, "expected {prev_gen} < {cur_gen}");
            // Stale generationprev_gen is refused with exact message.
            let err = fence_hydration_register(&store2, "ada", Some(&prev_gen)).unwrap_err();
            assert_eq!(
                err,
                format!(
                    "registration refused for `ada`: session launch generation `{prev_gen}` was superseded by a newer launch; this process should exit"
                )
            );
            // Registration without AGENT_HYDRATION_GENERATION is never refused.
            assert!(fence_hydration_register(&store2, "ada", None).is_ok());
            // Current generation succeeds and writes receipt.
            fence_hydration_register(&store2, "ada", Some(&cur_gen)).unwrap();
            assert!(
                record_hydration_ack(&store2, "ada", true, Some(&cur_gen), Some("claude:ada"))
                    .unwrap()
            );
            let ack_count = store2
                .list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))
                .unwrap()
                .iter()
                .filter(|e| e.slug.starts_with(HYDRATION_ACK_SLUG_PREFIX))
                .count();
            assert!(
                ack_count <= 1,
                "expected at most 1 hydration-ack-* entry, found {ack_count}"
            );
            prev_gen = cur_gen;
        }

        // Decommission clears hydration_generation, prunes remaining receipts to 0, and fences stale gen.
        let target = SessionTarget::resolve(&store2, "claude:ada", None).unwrap();
        let _ = teardown_session(&store2, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
        let decom_rec = find_hosted_session(&store2, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(decom_rec.hydration_generation, None);
        let remaining_acks = store2
            .list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))
            .unwrap()
            .iter()
            .filter(|e| e.slug.starts_with(HYDRATION_ACK_SLUG_PREFIX))
            .count();
        assert_eq!(
            remaining_acks, 0,
            "decommission must prune all hydration-ack-* entries"
        );
        let err = fence_hydration_register(&store2, "ada", Some(&prev_gen)).unwrap_err();
        assert_eq!(
            err,
            format!(
                "registration refused for `ada`: session launch generation `{prev_gen}` was superseded by a newer launch; this process should exit"
            )
        );
        assert!(fence_hydration_register(&store2, "ada", None).is_ok());
    }

    #[test]
    fn hydration_early_spawn_ack_multivendor_reuse_best_effort_and_prepare_clearing() {
        // 1. Early ack landing *during* `spawn` (Spec Plan 4 bullet 1 + M3 restart from decommissioning).
        struct EarlyAckRuntime<'a> {
            inner: FakeRuntime,
            store: &'a MemStore,
            persona: &'a str,
        }
        impl std::fmt::Debug for EarlyAckRuntime<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("EarlyAckRuntime")
                    .field("inner", &self.inner)
                    .field("persona", &self.persona)
                    .finish()
            }
        }
        impl SessionRuntime for EarlyAckRuntime<'_> {
            fn spawn(
                &self,
                session: &str,
                window: &str,
                cwd: Option<&str>,
                launch: Option<&LaunchSpec>,
                hosted_id: &str,
            ) -> bool {
                let started = self.inner.spawn(session, window, cwd, launch, hosted_id);
                if started && let Some(spec) = launch {
                    let g = spec
                        .env
                        .iter()
                        .find_map(|(k, v)| {
                            (k == "AGENT_HYDRATION_GENERATION").then_some(v.as_str())
                        })
                        .expect("AGENT_HYDRATION_GENERATION in launch.env");
                    fence_hydration_register(self.store, self.persona, Some(g))
                        .expect("early ack during spawn must not be refused");
                    assert!(
                        record_hydration_ack(
                            self.store,
                            self.persona,
                            true,
                            Some(g),
                            Some(hosted_id),
                        )
                        .unwrap()
                    );
                }
                started
            }
            fn kill(&self, unit: &str, hosted_id: &str) -> bool {
                self.inner.kill(unit, hosted_id)
            }
            fn exists(&self, unit: &str) -> bool {
                self.inner.exists(unit)
            }
            fn owns(&self, unit: &str, hosted_id: &str) -> bool {
                self.inner.owns(unit, hosted_id)
            }
        }

        let home = TempDir::new().unwrap();
        let early_store = MemStore::default();
        let early_rt = EarlyAckRuntime {
            inner: FakeRuntime::default(),
            store: &early_store,
            persona: "ada",
        };
        let early_env = LifecycleEnv {
            runtime: &early_rt,
            spawn_runtime: true,
            ..env(&home)
        };
        let spun = spin_up(
            &early_store,
            &early_env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert!(spun.runtime_spawned);
        assert_eq!(spun.hydration_status, HydrationStatus::Confirmed);
        let stored_after_spin = find_hosted_session(&early_store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(
            stored_after_spin.hydration_status,
            HydrationStatus::Confirmed
        );
        assert!(stored_after_spin.runtime_spawned);

        // Prepare decommission clears hydration_generation to None and sets status=decommissioning.
        let target_ada = SessionTarget::resolve(&early_store, "claude:ada", None).unwrap();
        let prepared = prepare_decommission(
            &early_store,
            &early_env,
            &target_ada,
            "owner",
            Caller::Owner,
        )
        .unwrap();
        assert_eq!(prepared.status, STATUS_DECOMMISSIONING);
        let decom_prep_rec = find_hosted_session(&early_store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(decom_prep_rec.hydration_generation, None);

        // Restart from `decommissioning` when runtime died: M3 persists running record before spawn
        // so early ack during `spawn` passes `fence_hydration_register` and promotes RestartResponse.
        early_rt.inner.clear();
        let restarted =
            restart_hosted_session(&early_store, &early_env, "claude:ada", "owner").unwrap();
        assert!(restarted.record.runtime_spawned);
        assert_eq!(restarted.record.restart_count, 1);
        assert_eq!(
            restarted.record.hydration_status,
            HydrationStatus::Confirmed
        );
        assert_eq!(
            find_hosted_session(&early_store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .hydration_status,
            HydrationStatus::Confirmed
        );

        // 2. I3: Restart when runtime already exists (or spawn_runtime = false) reuses existing
        // hydration_generation so the live process's env G still passes fence_hydration_register.
        let i3_home = TempDir::new().unwrap();
        webhook_route_file(i3_home.path(), "ada");
        let _i3_listener =
            std::os::unix::net::UnixListener::bind(i3_home.path().join("none.sock")).unwrap();
        let i3_store = MemStore::default();
        let i3_fake = FakeRuntime::default();
        let i3_env = LifecycleEnv {
            runtime: &i3_fake,
            spawn_runtime: true,
            ..env(&i3_home)
        };
        let i3_spun = spin_up(
            &i3_store,
            &i3_env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let g_orig = i3_spun.hydration_generation.clone().unwrap();
        assert!(
            record_hydration_ack(&i3_store, "ada", true, Some(&g_orig), Some("claude:ada"))
                .unwrap()
        );
        // Runtime "Ada" is still live in `i3_fake`: restart delivers prompt without spawning a new process.
        let i3_restarted =
            restart_hosted_session(&i3_store, &i3_env, "claude:ada", "owner").unwrap();
        assert_eq!(
            i3_restarted.record.hydration_generation.as_deref(),
            Some(g_orig.as_str()),
            "I3: restart without new process spawn must reuse existing hydration_generation"
        );
        assert_eq!(
            i3_restarted.record.hydration_status,
            HydrationStatus::Unconfirmed,
            "I3: prior receipt for reused generation must be cleared before redelivery"
        );
        fence_hydration_register(&i3_store, "ada", Some(&g_orig)).unwrap();
        assert!(
            record_hydration_ack(&i3_store, "ada", true, Some(&g_orig), Some("claude:ada"))
                .unwrap()
        );
        assert_eq!(
            find_hosted_session(&i3_store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .hydration_status,
            HydrationStatus::Confirmed
        );

        // 3. I2: Multi-vendor persona (`claude:ada` on G1 and `codex:ada` on G2 > G1) + M2 collision guard.
        let _trusted = trusted_codex_home(home.path(), &std::env::current_dir().unwrap());
        let mv_store = MemStore::default();
        let mv_fake = FakeRuntime::default();
        let mv_env = LifecycleEnv {
            runtime: &mv_fake,
            spawn_runtime: true,
            ..env(&home)
        };
        let claude_spun = spin_up(
            &mv_store,
            &mv_env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let g_claude = claude_spun.hydration_generation.unwrap();
        mv_fake.clear();
        let codex_spun = spin_up(
            &mv_store,
            &mv_env,
            &req("ada", "assistant", "codex"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let g_codex = codex_spun.hydration_generation.unwrap();
        assert!(g_claude < g_codex);
        // `claude:ada` is still live on `g_claude`: registering with `g_claude` must NOT be refused!
        fence_hydration_register(&mv_store, "ada", Some(&g_claude)).unwrap();
        assert!(
            record_hydration_ack(&mv_store, "ada", true, Some(&g_claude), Some("claude:ada"))
                .unwrap()
        );
        // M2: another persona attempting to record an ack with `g_claude` does not overwrite `ada`'s receipt.
        assert!(!record_hydration_ack(&mv_store, "bob", true, Some(&g_claude), None).unwrap());
        // Tearing down `codex:ada` must preserve `claude:ada`'s receipt for `g_claude`.
        let codex_target = SessionTarget::resolve(&mv_store, "codex:ada", None).unwrap();
        let _ = teardown_session(
            &mv_store,
            &mv_env,
            &codex_target,
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        assert_eq!(
            find_hosted_session(&mv_store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .hydration_status,
            HydrationStatus::Confirmed,
            "I2: tearing down codex:ada must not prune claude:ada's active receipt"
        );

        // 4. Remodel Prepare and undo_new_record clear hydration_generation to None.
        let rem_target = SessionTarget::resolve(&mv_store, "claude:ada", None).unwrap();
        let rem_prep = remodel_session(
            &mv_store,
            &mv_env,
            &rem_target,
            &RemodelRequest {
                vendor: "grok".into(),
                mode: Some("prepare".into()),
                ..Default::default()
            },
            "owner",
            Caller::Owner,
            |_| {},
        )
        .unwrap();
        assert_eq!(rem_prep.hydration_generation, None);
        assert_eq!(
            find_hosted_session(&mv_store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .hydration_generation,
            None
        );
        let mut undo_rec = hosted_record("grok", "ada", STATUS_RUNNING, true);
        undo_rec.hydration_generation = Some(next_hydration_generation());
        persist_hosted_session(&mv_store, &undo_rec, "owner").unwrap();
        undo_new_record(&mv_store, "grok:ada", None, "owner", Some(home.path())).unwrap();
        let undone = find_hosted_session(&mv_store, &["grok:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(undone.status, STATUS_DECOMMISSIONED);
        assert_eq!(undone.hydration_generation, None);

        // 5. I4: `persist_hosted_session` succeeds even if receipt promotion or pruning hits a store error.
        struct FailingReceiptStore {
            inner: MemStore,
        }
        impl StateStore for FailingReceiptStore {
            fn upsert_state(
                &self,
                args: UpsertStateArgs,
            ) -> Result<fl_core::state::StateEntry, StoreError> {
                self.inner.upsert_state(args)
            }
            fn retire_memory(
                &self,
                scope: &StateScope,
                slug: &str,
                lifecycle: fl_core::state::MemoryLifecycle,
                superseded_by: Option<fl_core::ids::StateId>,
                at: fl_core::At,
                by: &str,
            ) -> Result<fl_core::state::StateEntry, StoreError> {
                self.inner
                    .retire_memory(scope, slug, lifecycle, superseded_by, at, by)
            }
            fn get_state(
                &self,
                id: &fl_core::ids::StateId,
            ) -> Result<Option<fl_core::state::StateEntry>, StoreError> {
                self.inner.get_state(id)
            }
            fn get_state_by_key(
                &self,
                scope: &StateScope,
                concern: StateConcern,
                slug: &str,
            ) -> Result<Option<fl_core::state::StateEntry>, StoreError> {
                if slug.starts_with(HYDRATION_ACK_SLUG_PREFIX) {
                    return Err(StoreError::Credential("injected receipt read error".into()));
                }
                self.inner.get_state_by_key(scope, concern, slug)
            }
            fn delete_state_by_key(
                &self,
                _scope: &StateScope,
                _concern: StateConcern,
                _slug: &str,
            ) -> Result<bool, StoreError> {
                Err(StoreError::Credential(
                    "injected receipt delete error".into(),
                ))
            }
            fn list_state(
                &self,
                scope: Option<&StateScope>,
                concern: Option<StateConcern>,
            ) -> Result<Vec<fl_core::state::StateEntry>, StoreError> {
                self.inner.list_state(scope, concern)
            }
            fn state_revisions(
                &self,
                id: &fl_core::ids::StateId,
            ) -> Result<Vec<fl_core::state::StateRevision>, StoreError> {
                self.inner.state_revisions(id)
            }
            fn list_conflicts(&self) -> Result<Vec<fl_core::state::StateConflict>, StoreError> {
                self.inner.list_conflicts()
            }
            fn resolve_conflict(
                &self,
                id: &fl_core::ids::StateId,
                keep: &fl_core::ids::RevisionId,
                at: fl_core::At,
                by: &str,
            ) -> Result<fl_core::state::StateEntry, StoreError> {
                self.inner.resolve_conflict(id, keep, at, by)
            }
            fn state_revisions_after(
                &self,
                after_seq: u64,
            ) -> Result<Vec<(u64, fl_core::state::StateRevision)>, StoreError> {
                self.inner.state_revisions_after(after_seq)
            }
            fn import_state_revisions(
                &self,
                revisions: &[fl_core::state::StateRevision],
                merge_at: fl_core::At,
            ) -> Result<(), StoreError> {
                self.inner.import_state_revisions(revisions, merge_at)
            }
            fn sync_cursor(
                &self,
                remote: &str,
            ) -> Result<Option<fl_core::state::StateSyncCursor>, StoreError> {
                self.inner.sync_cursor(remote)
            }
            fn set_sync_cursor(
                &self,
                cursor: fl_core::state::StateSyncCursor,
            ) -> Result<(), StoreError> {
                self.inner.set_sync_cursor(cursor)
            }
        }

        let failing_store = FailingReceiptStore {
            inner: MemStore::default(),
        };
        let g_fail = next_hydration_generation();
        let g_stale = next_hydration_generation();
        assert!(
            record_hydration_ack(&failing_store.inner, "ada", true, Some(&g_stale), None).unwrap()
        );
        let mut rec_i4 = hosted_record("claude", "ada", STATUS_RUNNING, true);
        rec_i4.hydration_status = HydrationStatus::Unconfirmed;
        rec_i4.hydration_generation = Some(g_fail);
        persist_hosted_session(&failing_store, &rec_i4, "owner")
            .expect("I4: receipt promotion/pruning errors must not fail persist_hosted_session");
        assert!(
            find_hosted_session(&failing_store.inner, &["claude:ada"])
                .unwrap()
                .is_some()
        );
    }

    fn write_claude_transcript(
        home: &Path,
        archived: bool,
        sid: &str,
        model: &str,
        active_tokens: u64,
        include_bridge_and_torn_tail: bool,
    ) -> PathBuf {
        let sub = if archived { "archived" } else { "projects" };
        let dir = home.join(".claude").join(sub).join("ferroloop");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{sid}.jsonl"));
        let mut content = format!(
            "{{\"type\":\"user\",\"sessionId\":\"{sid}\",\"uuid\":\"u-1\",\"message\":{{\"role\":\"user\",\"content\":\"hydrate\"}}}}\n\
             {{\"type\":\"assistant\",\"sessionId\":\"{sid}\",\"uuid\":\"a-1\",\"message\":{{\"model\":\"{model}\",\"role\":\"assistant\",\"stop_reason\":\"end_turn\",\"usage\":{{\"input_tokens\":2,\"cache_creation_input_tokens\":94,\"cache_read_input_tokens\":{}}},\"content\":[{{\"type\":\"text\",\"text\":\"ready\"}}]}}}}\n",
            active_tokens.saturating_sub(96)
        );
        if include_bridge_and_torn_tail {
            content.push_str(&format!(
                "{{\"type\":\"bridge-session\",\"sessionId\":\"{sid}\",\"bridgeSessionId\":\"cse_01SP4ortBQU3hQGNi4NtM8pN\"}}\n\
                 {{\"type\":\"assistant\",\"sessionId\":\"{sid}\",\"uuid\":\"a-2\",\"message\":{{\"model\":\"{model}\",\"role\":\"assistant\",\"stop_reason\":\"tool_use\",\"usage\":{{\"input_tokens\":2,\"cache_creation_input_tokens\":94,\"cache_read_input_tokens\":{}}},\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_01CSgTocp6F5DXW47yNSYL82\",\"name\":\"Bash\",\"input\":{{\"command\":\"sleep 25\"}}}}]}}}}\n\
                 {{\"type\":\"assistant\",\"message\":",
                active_tokens.saturating_sub(96)
            ));
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn claude_resume_happy_path_on_restart_and_stopped_spin_up_preserves_identity_and_sends_notice()
    {
        use fl_core::ferrowire::WireFilter;
        use fl_core::store::MessageStore;

        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let cwd_dir = home.path().join("worktree");
        std::fs::create_dir_all(&cwd_dir).unwrap();
        std::fs::write(cwd_dir.join("AGENTS.md"), "initial agents\n").unwrap();
        std::fs::write(cwd_dir.join("WORKFLOW.md"), "initial workflow\n").unwrap();

        let fake = FakeRuntime::default();
        let env = fake.env(home.path());
        let mut request = req("ada", "assistant", "claude");
        request.cwd = Some(cwd_dir.to_string_lossy().into_owned());

        let spun = spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap();
        assert!(spun.runtime_spawned);
        let g1 = spun.hydration_generation.clone().unwrap();
        let rec = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        let sid = rec.vendor_session_id.clone().unwrap();

        assert!(
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap(),
            "hydration ack must record for G1"
        );
        write_claude_transcript(home.path(), false, &sid, "claude-opus-5-5", 388_396, false);

        // Touch WORKFLOW.md so its mtime strictly exceeds hydration-ack-<G1>.accepted_at_ms.
        std::thread::sleep(Duration::from_millis(15));
        std::fs::write(cwd_dir.join("WORKFLOW.md"), "updated workflow rules\n").unwrap();

        // 1. Stop runtime and run restart_hosted_session -> resumes with `--resume <sid>`.
        fake.clear();
        fake.launches.borrow_mut().clear();
        let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
        assert!(restarted.record.runtime_spawned);
        assert_eq!(
            restarted.record.vendor_session_id.as_deref(),
            Some(sid.as_str())
        );
        assert_eq!(
            restarted.record.hydration_generation.as_deref(),
            Some(g1.as_str())
        );
        assert_eq!(
            restarted.record.hydration_status,
            HydrationStatus::Confirmed
        );
        assert!(restarted.record.hydration_dispatched);
        assert!(
            store
                .get_state_by_key(
                    &StateScope::Workspace,
                    StateConcern::Runtime,
                    &hydration_ack_slug(&g1),
                )
                .unwrap()
                .is_some(),
            "hydration-ack-<G1> must stay intact on --resume"
        );

        let launches = fake.launches.borrow().clone();
        assert_eq!(launches.len(), 1);
        let resume_spec = launches[0].1.as_ref().expect("resume launch spec");
        assert!(
            resume_spec.argv.windows(2).any(|w| w == ["--resume", &sid]),
            "expected --resume {sid} in {:?}",
            resume_spec.argv
        );
        assert!(
            resume_spec.argv.contains(&"--remote-control".to_string()),
            "expected --remote-control in {:?}",
            resume_spec.argv
        );
        assert!(
            resume_spec.argv.windows(2).any(|w| w == ["--name", "Ada"]),
            "expected --name Ada in {:?}",
            resume_spec.argv
        );
        assert!(
            !resume_spec.argv.iter().any(|a| a == "--session-id"),
            "--resume argv must not contain --session-id: {:?}",
            resume_spec.argv
        );
        assert_eq!(
            resume_spec.argv.last().map(String::as_str),
            Some("Ada"),
            "--resume argv must have no trailing positional hydration prompt: {:?}",
            resume_spec.argv
        );

        let inbox = store
            .list_wire_inbox(&WireFilter {
                recipient: Some("ada".into()),
                unread_only: true,
                ..WireFilter::default()
            })
            .unwrap();
        assert_eq!(inbox.len(), 1, "expected 1 Host restarted notice in inbox");
        assert_eq!(inbox[0].message.subject, "Host restarted");
        assert!(
            inbox[0].message.body.contains("`WORKFLOW.md`"),
            "expected modified `WORKFLOW.md` in notice body, got: {}",
            inbox[0].message.body
        );

        // Simulate SessionStart:resume hook registering the route for the resumed session before
        // it stops again in part 2.
        let routes_path = home.path().join(".cache/agent-bus/routes.json");
        let mut table = RouteTable::default();
        table.upsert(
            "ada",
            VendorRoute::Claude {
                pid: std::process::id(),
                session_id: sid.clone(),
                socket_path: home.path().join("claude.sock"),
                key_path: None,
            },
            stamp::now(),
        );
        table.save(Some(&routes_path)).unwrap();

        // 2. Mark session stopped and re-run spin_up -> also resumes `<sid>` and preserves G1.
        fake.clear();
        fake.launches.borrow_mut().clear();
        let mut stopped = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        stopped.status = STATUS_STOPPED.to_string();
        persist_hosted_session(&store, &stopped, "owner").unwrap();

        let respun = spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap();
        assert!(respun.runtime_spawned);
        assert_eq!(respun.hydration_status, HydrationStatus::Confirmed);
        assert_eq!(respun.hydration_generation.as_deref(), Some(g1.as_str()));
        let after_respun = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(
            after_respun.vendor_session_id.as_deref(),
            Some(sid.as_str())
        );
        let launches = fake.launches.borrow().clone();
        assert_eq!(launches.len(), 1);
        let respun_spec = launches[0].1.as_ref().unwrap();
        assert!(
            respun_spec.argv.windows(2).any(|w| w == ["--resume", &sid]),
            "spin_up on stopped confirmed record must resume {sid}: {:?}",
            respun_spec.argv
        );
    }

    #[test]
    fn claude_resume_sigkill_mid_tool_call_repairs_only_torn_tail_and_preserves_bridge_session() {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = fake.env(home.path());

        let spun = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let g1 = spun.hydration_generation.unwrap();
        let sid = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap()
            .vendor_session_id
            .unwrap();
        record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();

        let path =
            write_claude_transcript(home.path(), false, &sid, "claude-opus-5-5", 94_671, true);

        fake.clear();
        fake.launches.borrow_mut().clear();
        let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
        assert_eq!(
            restarted.record.vendor_session_id.as_deref(),
            Some(sid.as_str())
        );
        assert_eq!(
            restarted.record.hydration_status,
            HydrationStatus::Confirmed
        );

        let repaired_text = std::fs::read_to_string(&path).unwrap();
        assert!(
            repaired_text.contains("\"bridgeSessionId\":\"cse_01SP4ortBQU3hQGNi4NtM8pN\""),
            "bridge-session line must remain intact: {repaired_text}"
        );
        assert!(
            repaired_text.contains("\"id\":\"toolu_01CSgTocp6F5DXW47yNSYL82\""),
            "dangling tool_use line must remain intact for Claude Code recovery: {repaired_text}"
        );
        assert!(
            !repaired_text.ends_with("{\"type\":\"assistant\",\"message\":"),
            "torn trailing fragment must be truncated: {repaired_text}"
        );
        let ctx = estimate_claude_post_compaction_context(&env, &path).unwrap();
        assert!(ctx.has_persisted_bridge_session());
        assert_eq!(
            ctx.bridge_session_id.as_deref(),
            Some("cse_01SP4ortBQU3hQGNi4NtM8pN")
        );
    }

    #[test]
    fn claude_resume_fallback_ladder_covers_all_reasons_and_emits_lifecycle_notices() {
        // 1. unconfirmed_hydration
        {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let spun = spin_up(
                &store,
                &env,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            write_claude_transcript(home.path(), false, &sid1, "claude-opus-5-5", 10_000, false);

            fake.clear();
            let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
            assert_ne!(
                restarted.record.vendor_session_id.as_deref(),
                Some(sid1.as_str())
            );
            assert_ne!(
                restarted.record.hydration_generation.as_deref(),
                Some(g1.as_str())
            );
            assert!(
                restarted
                    .warnings
                    .iter()
                    .any(|w| w.contains("unconfirmed_hydration")),
                "expected unconfirmed_hydration notice in {:?}",
                restarted.warnings
            );
        }

        // 1b. missing_or_tombstoned_session_id (both missing None and tombstoned session_id)
        for is_tombstoned in [false, true] {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let spun = spin_up(
                &store,
                &env,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
            write_claude_transcript(home.path(), false, &sid1, "claude-opus-5-5", 10_000, false);

            let mut rec = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap();
            if is_tombstoned {
                rec.tombstone_ids.push(sid1.clone());
            } else {
                rec.vendor_session_id = None;
            }
            persist_hosted_session(&store, &rec, "owner").unwrap();

            fake.clear();
            let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
            assert_ne!(
                restarted.record.vendor_session_id.as_deref(),
                Some(sid1.as_str())
            );
            assert!(
                restarted.record.vendor_session_id.is_some(),
                "fresh vendor_session_id must be minted on fallback"
            );
            assert!(
                restarted
                    .warnings
                    .iter()
                    .any(|w| w.contains("missing_or_tombstoned_session_id")),
                "expected missing_or_tombstoned_session_id (is_tombstoned={is_tombstoned}) notice in {:?}",
                restarted.warnings
            );
            assert!(
                store
                    .get_state_by_key(
                        &StateScope::Workspace,
                        StateConcern::Runtime,
                        &hydration_ack_slug(&g1),
                    )
                    .unwrap()
                    .is_none(),
                "hydration-ack-<G1> must be pruned on missing_or_tombstoned_session_id fallback"
            );
            if is_tombstoned {
                // Re-spinning a non-decommissioned record must also preserve tombstone_ids.
                let respun = spin_up(
                    &store,
                    &env,
                    &req("ada", "assistant", "claude"),
                    "owner",
                    Caller::Owner,
                )
                .unwrap();
                let after = find_hosted_session(&store, &[&respun.session_id])
                    .unwrap()
                    .unwrap();
                assert!(
                    after.tombstone_ids.contains(&sid1),
                    "spin_up on non-decommissioned record must preserve tombstone_ids, got {:?}",
                    after.tombstone_ids
                );
            }
        }

        // 2. missing_or_archived_transcript (transcript only in .claude/archived/)
        {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let spun = spin_up(
                &store,
                &env,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
            write_claude_transcript(home.path(), true, &sid1, "claude-opus-5-5", 10_000, false);

            fake.clear();
            let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
            assert_ne!(
                restarted.record.vendor_session_id.as_deref(),
                Some(sid1.as_str())
            );
            assert!(
                restarted
                    .warnings
                    .iter()
                    .any(|w| w.contains("missing_or_archived_transcript")),
                "expected missing_or_archived_transcript notice in {:?}",
                restarted.warnings
            );
            assert!(
                store
                    .get_state_by_key(
                        &StateScope::Workspace,
                        StateConcern::Runtime,
                        &hydration_ack_slug(&g1),
                    )
                    .unwrap()
                    .is_none(),
                "hydration-ack-<G1> must be pruned on missing_or_archived_transcript fallback"
            );
        }

        // 3. empty_or_unparseable_transcript (0 turns)
        {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let spun = spin_up(
                &store,
                &env,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
            let proj_dir = home.path().join(".claude/projects/ferroloop");
            std::fs::create_dir_all(&proj_dir).unwrap();
            std::fs::write(proj_dir.join(format!("{sid1}.jsonl")), "").unwrap();

            fake.clear();
            let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
            assert_ne!(
                restarted.record.vendor_session_id.as_deref(),
                Some(sid1.as_str())
            );
            assert!(
                restarted
                    .warnings
                    .iter()
                    .any(|w| w.contains("empty_or_unparseable_transcript")),
                "expected empty_or_unparseable_transcript notice in {:?}",
                restarted.warnings
            );
            assert!(
                store
                    .get_state_by_key(
                        &StateScope::Workspace,
                        StateConcern::Runtime,
                        &hydration_ack_slug(&g1),
                    )
                    .unwrap()
                    .is_none(),
                "hydration-ack-<G1> must be pruned on empty_or_unparseable_transcript fallback"
            );
        }

        // 4. context_over_cap (model-relative: 388,396 fails on claude-opus-4-1 200k window,
        //    895,176 fails on claude-opus-5-5 1M window)
        for (model, tokens) in [
            ("claude-opus-4-1", 388_396u64),
            ("claude-opus-5-5", 895_176u64),
        ] {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let mut request = req("ada", "assistant", "claude");
            request.model = Some(model.to_string());
            let spun = spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
            write_claude_transcript(home.path(), false, &sid1, model, tokens, false);

            fake.clear();
            let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
            assert_ne!(
                restarted.record.vendor_session_id.as_deref(),
                Some(sid1.as_str()),
                "{model} at {tokens} tokens must refuse resume"
            );
            assert!(
                restarted
                    .warnings
                    .iter()
                    .any(|w| w.contains("context_over_cap")),
                "expected context_over_cap notice for {model} at {tokens} tokens in {:?}",
                restarted.warnings
            );
            assert!(
                store
                    .get_state_by_key(
                        &StateScope::Workspace,
                        StateConcern::Runtime,
                        &hydration_ack_slug(&g1),
                    )
                    .unwrap()
                    .is_none(),
                "hydration-ack-<G1> must be pruned on context_over_cap fallback"
            );
        }

        // 5. recast_guidance_staged
        {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let spun = spin_up(
                &store,
                &env,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
            write_claude_transcript(home.path(), false, &sid1, "claude-opus-5-5", 10_000, false);

            let agent = store.agent_by_name("ada").unwrap().unwrap();
            stage_recast_guidance(&store, &agent, &agent.project, &agent.role, "owner").unwrap();

            fake.clear();
            let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
            assert_ne!(
                restarted.record.vendor_session_id.as_deref(),
                Some(sid1.as_str())
            );
            assert!(
                restarted
                    .warnings
                    .iter()
                    .any(|w| w.contains("recast_guidance_staged")),
                "expected recast_guidance_staged notice in {:?}",
                restarted.warnings
            );
            assert!(
                store
                    .get_state_by_key(
                        &StateScope::Workspace,
                        StateConcern::Runtime,
                        &hydration_ack_slug(&g1),
                    )
                    .unwrap()
                    .is_none(),
                "hydration-ack-<G1> must be pruned on recast_guidance_staged fallback"
            );
        }

        // 6. resume_launch_did_not_survive (--resume exits immediately, retries once with fresh UUIDv7 + G2)
        {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let spun = spin_up(
                &store,
                &env,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
            write_claude_transcript(home.path(), false, &sid1, "claude-opus-5-5", 10_000, false);

            fake.clear();
            fake.launches.borrow_mut().clear();
            fake.vanish_once_windows.borrow_mut().push("claude".into());

            let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
            assert!(
                restarted.record.runtime_spawned,
                "fallback fresh launch must survive"
            );
            let sid2 = restarted.record.vendor_session_id.clone().unwrap();
            let g2 = restarted.record.hydration_generation.clone().unwrap();
            assert_ne!(sid2, sid1, "fallback must mint fresh vendor_session_id");
            assert_ne!(g2, g1, "fallback must mint fresh hydration_generation");
            assert_eq!(
                restarted.record.hydration_status,
                HydrationStatus::Unconfirmed
            );
            assert!(
                store
                    .get_state_by_key(
                        &StateScope::Workspace,
                        StateConcern::Runtime,
                        &hydration_ack_slug(&g1),
                    )
                    .unwrap()
                    .is_none(),
                "hydration-ack-<G1> must be pruned when --resume fails launch_survived"
            );
            assert!(
                restarted
                    .warnings
                    .iter()
                    .any(|w| w.contains("resume_launch_did_not_survive")),
                "expected resume_launch_did_not_survive notice in {:?}",
                restarted.warnings
            );
            let launches = fake.launches.borrow().clone();
            assert_eq!(
                launches.len(),
                2,
                "expected 1st spawn (--resume) + 2nd spawn (fresh --session-id)"
            );
            let first_argv = &launches[0].1.as_ref().unwrap().argv;
            let second_argv = &launches[1].1.as_ref().unwrap().argv;
            assert!(first_argv.windows(2).any(|w| w == ["--resume", &sid1]));
            assert!(!second_argv.iter().any(|a| a == "--resume"));
            assert!(
                second_argv.last().is_some_and(|p| p.contains(&g2)),
                "retry fresh launch must pass hydration prompt with new generation {g2}: {second_argv:?}"
            );
        }
    }

    #[test]
    fn claude_clear_route_sync_updates_vendor_session_id_and_ignores_tombstoned_and_foreign_routes()
    {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        fake.set_unit_pid("Ada", 999_999_991);
        let env = fake.env(home.path());

        let spun = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let g1 = spun.hydration_generation.unwrap();
        let old_sid = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap()
            .vendor_session_id
            .unwrap();
        record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
        write_claude_transcript(
            home.path(),
            false,
            &old_sid,
            "claude-opus-5-5",
            10_000,
            false,
        );

        let new_sid = uuid::Uuid::now_v7().to_string();
        let foreign_sid = uuid::Uuid::now_v7().to_string();
        let live_sid = uuid::Uuid::now_v7().to_string();
        let tombstoned_sid = uuid::Uuid::now_v7().to_string();
        for sid in [&new_sid, &foreign_sid, &live_sid, &tombstoned_sid] {
            write_claude_transcript(home.path(), false, sid, "claude-opus-5-5", 12_000, false);
        }

        let routes_path = home.path().join(".cache/agent-bus/routes.json");
        let mut table = RouteTable::default();
        table.upsert(
            "ada",
            VendorRoute::Claude {
                pid: 999_999_991,
                session_id: tombstoned_sid.clone(),
                socket_path: home.path().join("claude.sock"),
                key_path: None,
            },
            At::from_unix_millis(10),
        );
        table.save(Some(&routes_path)).unwrap();

        let mut rec = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        rec.tombstone_ids.push(tombstoned_sid);
        persist_hosted_session(&store, &rec, "owner").unwrap();

        // 1. Tombstoned route session_id is ignored.
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(
            find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .as_deref(),
            Some(old_sid.as_str())
        );

        // 2. I2: A LIVE Claude process (`pid = std::process::id()`) registered under `ada` is
        //    never adopted even if `record.pid` matches.
        fake.clear();
        fake.launches.borrow_mut().clear();
        table.upsert(
            "ada",
            VendorRoute::Claude {
                pid: std::process::id(),
                session_id: live_sid.clone(),
                socket_path: home.path().join("claude.sock"),
                key_path: None,
            },
            At::from_unix_millis(15),
        );
        table.save(Some(&routes_path)).unwrap();
        let mut probe_rec = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap();
        probe_rec.pid = Some(std::process::id());
        assert!(
            !sync_claude_vendor_session_id_from_route(&store, &env, &mut probe_rec),
            "I2: live process route must never be adopted"
        );
        assert_eq!(
            probe_rec.vendor_session_id.as_deref(),
            Some(old_sid.as_str())
        );

        // 3. I2: A DEAD Claude process (`pid = 999_999_992`) with a different PID than
        //    `record.pid = 999_999_991` and no matching `(hosted_id, hydration_generation)` is
        //    never adopted.
        probe_rec.pid = Some(999_999_991);
        persist_hosted_session(&store, &probe_rec, "owner").unwrap();
        table.upsert(
            "ada",
            VendorRoute::Claude {
                pid: 999_999_992,
                session_id: foreign_sid.clone(),
                socket_path: home.path().join("claude.sock"),
                key_path: None,
            },
            At::from_unix_millis(18),
        );
        table.save(Some(&routes_path)).unwrap();
        assert!(
            !sync_claude_vendor_session_id_from_route(&store, &env, &mut probe_rec),
            "I2: dead foreign route with mismatched PID and no provenance must never be adopted"
        );
        assert_eq!(
            probe_rec.vendor_session_id.as_deref(),
            Some(old_sid.as_str())
        );

        // 4. Minor #5: Dead route from this session's own process (`pid == record.pid == 999_999_991`)
        //    is rejected while the unit `"Ada"` is still running (`env.runtime.exists("Ada")`),
        //    and then adopted once `"Ada"` stops.
        table.upsert(
            "ada",
            VendorRoute::Claude {
                pid: 999_999_991,
                session_id: new_sid.clone(),
                socket_path: home.path().join("claude.sock"),
                key_path: None,
            },
            At::from_unix_millis(20),
        );
        table.save(Some(&routes_path)).unwrap();
        fake.live.borrow_mut().insert("Ada".to_string());
        assert!(
            !sync_claude_vendor_session_id_from_route(&store, &env, &mut probe_rec),
            "Minor #5: matching dead-PID route must be rejected while unit Ada is still running"
        );
        assert_eq!(
            probe_rec.vendor_session_id.as_deref(),
            Some(old_sid.as_str())
        );

        fake.clear();
        fake.launches.borrow_mut().clear();
        assert!(
            sync_claude_vendor_session_id_from_route(&store, &env, &mut probe_rec),
            "Minor #5: matching dead-PID route must be adopted once unit Ada is stopped"
        );
        assert_eq!(
            probe_rec.vendor_session_id.as_deref(),
            Some(new_sid.as_str())
        );
        let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
        assert_eq!(
            restarted.record.vendor_session_id.as_deref(),
            Some(new_sid.as_str())
        );
        let launches = fake.launches.borrow().clone();
        assert_eq!(launches.len(), 1);
        let argv = &launches[0].1.as_ref().unwrap().argv;
        assert!(
            argv.windows(2).any(|w| w == ["--resume", &new_sid]),
            "expected --resume {new_sid} after /clear sync, got {argv:?}"
        );
    }

    #[test]
    fn claude_resume_launch_survived_detects_post_t0_exit_and_unconfirmed_route_death() {
        // 1. Unit exists at t=0 (owns check #1) and exits at t=1s (owns check #2) for both
        //    `restart_hosted_session` and `spin_up`.
        for use_spin_up in [false, true] {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let spun = spin_up(
                &store,
                &env,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
            write_claude_transcript(home.path(), false, &sid1, "claude-opus-5-5", 10_000, false);

            fake.clear();
            fake.launches.borrow_mut().clear();
            // Alive at t=0 (check #1), dead at t=1s (check #2).
            fake.vanish_unit_after_owns_checks("Ada", 1);

            if use_spin_up {
                let mut stopped = find_hosted_session(&store, &["claude:ada"])
                    .unwrap()
                    .unwrap();
                stopped.status = STATUS_STOPPED.to_string();
                persist_hosted_session(&store, &stopped, "owner").unwrap();
                let respun = spin_up(
                    &store,
                    &env,
                    &req("ada", "assistant", "claude"),
                    "owner",
                    Caller::Owner,
                )
                .unwrap();
                assert!(respun.runtime_spawned);
                assert_eq!(respun.hydration_status, HydrationStatus::Unconfirmed);
                assert_ne!(respun.hydration_generation.as_deref(), Some(g1.as_str()));
                assert!(
                    respun
                        .warnings
                        .iter()
                        .any(|w| w.contains("resume_launch_did_not_survive")),
                    "spin_up must emit resume_launch_did_not_survive when unit exits at check #2: {:?}",
                    respun.warnings
                );
            } else {
                let restarted =
                    restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
                assert!(restarted.record.runtime_spawned);
                assert_eq!(
                    restarted.record.hydration_status,
                    HydrationStatus::Unconfirmed
                );
                assert_ne!(
                    restarted.record.hydration_generation.as_deref(),
                    Some(g1.as_str())
                );
                assert!(
                    restarted
                        .warnings
                        .iter()
                        .any(|w| w.contains("resume_launch_did_not_survive")),
                    "restart must emit resume_launch_did_not_survive when unit exits at check #2: {:?}",
                    restarted.warnings
                );
            }

            let rec_after = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap();
            assert_ne!(
                rec_after.vendor_session_id.as_deref(),
                Some(sid1.as_str()),
                "fresh vendor_session_id must be minted after post-t=0 resume exit"
            );
            assert!(
                rec_after.tombstone_ids.contains(&sid1),
                "old session ID `{sid1}` must be tombstoned after resume failure, got {:?}",
                rec_after.tombstone_ids
            );
            assert!(
                store
                    .get_state_by_key(
                        &StateScope::Workspace,
                        StateConcern::Runtime,
                        &hydration_ack_slug(&g1),
                    )
                    .unwrap()
                    .is_none(),
                "hydration-ack-<G1> must be pruned after post-t=0 resume exit"
            );
            let launches = fake.launches.borrow().clone();
            assert_eq!(
                launches.len(),
                2,
                "expected 1st spawn (--resume) + 2nd spawn (fresh --session-id)"
            );
            assert!(
                launches[0]
                    .1
                    .as_ref()
                    .unwrap()
                    .argv
                    .windows(2)
                    .any(|w| w == ["--resume", &sid1])
            );
            assert!(
                !launches[1]
                    .1
                    .as_ref()
                    .unwrap()
                    .argv
                    .iter()
                    .any(|a| a == "--resume")
            );
        }

        // 2. Resumed unit survives the initial window without `SessionStart:resume` hook
        //    confirmation (`resume_unconfirmed_session_id = Some(sid)`), then exits before
        //    confirmation: `reconcile_dead_hosted_sessions` marks `resume_failed = true` and the
        //    next `restart_hosted_session` goes straight to a fresh launch with
        //    `resume_launch_did_not_survive`.
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = fake.env(home.path());
        let spun = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let g1 = spun.hydration_generation.unwrap();
        let sid1 = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap()
            .vendor_session_id
            .unwrap();
        record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
        write_claude_transcript(home.path(), false, &sid1, "claude-opus-5-5", 10_000, false);

        fake.clear();
        fake.launches.borrow_mut().clear();
        let resumed = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
        assert_eq!(
            resumed.record.vendor_session_id.as_deref(),
            Some(sid1.as_str())
        );
        let rstate_after_resume = load_claude_resume_state(&store, "claude:ada");
        assert_eq!(
            rstate_after_resume.resume_unconfirmed_session_id.as_deref(),
            Some(sid1.as_str()),
            "survival without SessionStart:resume route must record resume_unconfirmed_session_id"
        );

        // Now the resumed unit dies before registering its route.
        fake.clear();
        fake.presence_of
            .borrow_mut()
            .insert("Ada:claude".to_string(), RuntimePresence::Gone);
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();

        let rstate_after_death = load_claude_resume_state(&store, "claude:ada");
        assert!(
            rstate_after_death.resume_failed,
            "reconcile_dead_hosted_sessions must set resume_failed = true when unconfirmed resumed unit dies"
        );
        assert!(rstate_after_death.resume_unconfirmed_session_id.is_none());

        fake.presence_of.borrow_mut().clear();
        fake.launches.borrow_mut().clear();
        let fresh_after_death =
            restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
        assert_ne!(
            fresh_after_death.record.vendor_session_id.as_deref(),
            Some(sid1.as_str()),
            "next restart after unconfirmed resume death must go straight to a fresh session"
        );
        assert!(
            fresh_after_death.record.tombstone_ids.contains(&sid1),
            "failed resumed session ID must be tombstoned"
        );
        assert!(
            fresh_after_death
                .warnings
                .iter()
                .any(|w| w.contains("resume_launch_did_not_survive")),
            "expected resume_launch_did_not_survive warning on next restart, got {:?}",
            fresh_after_death.warnings
        );
        let launches = fake.launches.borrow().clone();
        assert_eq!(
            launches.len(),
            1,
            "must go straight to fresh launch without trying --resume again"
        );
        assert!(
            !launches[0]
                .1
                .as_ref()
                .unwrap()
                .argv
                .iter()
                .any(|a| a == "--resume")
        );
    }

    #[test]
    fn claude_resume_tail_repair_runs_only_after_eligibility_and_backs_up_torn_tail() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        // 1. Over-cap transcript with a torn trailing line is rejected by
        //    `evaluate_claude_resume_eligibility` (`context_over_cap`) and the transcript file on
        //    disk is NOT modified (and no `.fl-torn-tail-*` backup is created).
        {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let mut request = req("ada", "assistant", "claude");
            request.model = Some("claude-opus-4-1".into());
            let spun = spin_up(&store, &env, &request, "owner", Caller::Owner).unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
            let path = write_claude_transcript(
                home.path(),
                false,
                &sid1,
                "claude-opus-4-1",
                388_396,
                true,
            );
            let original_bytes = std::fs::read(&path).unwrap();

            fake.clear();
            let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
            assert!(
                restarted
                    .warnings
                    .iter()
                    .any(|w| w.contains("context_over_cap")),
                "expected context_over_cap in {:?}",
                restarted.warnings
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                original_bytes,
                "I3: ineligible over-cap transcript must not be truncated on disk"
            );
            let backups: Vec<_> = std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().contains(".fl-torn-tail-"))
                .collect();
            assert!(
                backups.is_empty(),
                "I3: no .fl-torn-tail-* backup may be created when resume is rejected"
            );
        }

        // 2. Eligible transcript with a torn trailing line after valid lines creates
        //    `<sid>.jsonl.fl-torn-tail-<ts>` with mode `0600` containing the exact removed tail
        //    bytes, and truncates the transcript.
        {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let spun = spin_up(
                &store,
                &env,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
            let path =
                write_claude_transcript(home.path(), false, &sid1, "claude-opus-5-5", 10_000, true);

            fake.clear();
            let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
            assert_eq!(
                restarted.record.vendor_session_id.as_deref(),
                Some(sid1.as_str())
            );
            let backups: Vec<_> = std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().contains(".fl-torn-tail-"))
                .collect();
            assert_eq!(backups.len(), 1, "expected 1 .fl-torn-tail-* backup file");
            let backup_path = backups[0].path();
            assert_eq!(
                std::fs::read_to_string(&backup_path).unwrap(),
                "{\"type\":\"assistant\",\"message\":"
            );
            #[cfg(unix)]
            {
                let mode = std::fs::metadata(&backup_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "backup file must have 0600 permissions");
            }
        }

        // 3. File with zero valid lines (single torn line) is never truncated on disk and causes
        //    fallback with `empty_or_unparseable_transcript`.
        {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let spun = spin_up(
                &store,
                &env,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();
            let proj_dir = home.path().join(".claude/projects/ferroloop");
            std::fs::create_dir_all(&proj_dir).unwrap();
            let path = proj_dir.join(format!("{sid1}.jsonl"));
            let single_torn = b"{\"type\":\"user\",\"mess";
            std::fs::write(&path, single_torn).unwrap();

            fake.clear();
            let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
            assert!(
                restarted
                    .warnings
                    .iter()
                    .any(|w| w.contains("empty_or_unparseable_transcript")),
                "expected empty_or_unparseable_transcript in {:?}",
                restarted.warnings
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                single_torn,
                "I3: file with 0 valid lines must not be truncated to 0 bytes"
            );
        }
    }

    #[test]
    fn claude_spin_up_stopped_fallback_ladder_and_model_context_window_shrunk() {
        // Minor 5: spin_up fallback ladder tests beyond tombstone:
        // (a) missing_transcript, (b) empty_or_unparseable_transcript,
        // (c) transcript_too_large (> 64 MiB, Minor #6),
        // (d) active_context_tokens_exceeded, (e) recast_staged,
        // plus Minor 8: (f) model_context_window_shrunk.
        enum Case {
            MissingTranscript,
            EmptyTranscript,
            TranscriptTooLarge,
            OverCap,
            RecastStaged,
            WindowShrunk,
        }
        for (case, expected_tag) in [
            (Case::MissingTranscript, "missing_transcript"),
            (Case::EmptyTranscript, "empty_or_unparseable_transcript"),
            (Case::TranscriptTooLarge, "transcript_too_large"),
            (Case::OverCap, "active_context_tokens_exceeded"),
            (Case::RecastStaged, "recast_staged"),
            (Case::WindowShrunk, "model_context_window_shrunk"),
        ] {
            let store = MemStore::default();
            let home = TempDir::new().unwrap();
            let fake = FakeRuntime::default();
            let env = fake.env(home.path());
            let mut initial_req = req("ada", "assistant", "claude");
            initial_req.model = Some("claude-opus-4-6[1m]".into());
            let spun = spin_up(&store, &env, &initial_req, "owner", Caller::Owner).unwrap();
            let g1 = spun.hydration_generation.unwrap();
            let sid1 = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap();

            let mut next_req = initial_req.clone();
            match case {
                Case::MissingTranscript => {}
                Case::EmptyTranscript => {
                    let proj_dir = home.path().join(".claude/projects/ferroloop");
                    std::fs::create_dir_all(&proj_dir).unwrap();
                    std::fs::write(proj_dir.join(format!("{sid1}.jsonl")), "").unwrap();
                }
                Case::TranscriptTooLarge => {
                    let proj_dir = home.path().join(".claude/projects/ferroloop");
                    std::fs::create_dir_all(&proj_dir).unwrap();
                    let f = std::fs::File::create(proj_dir.join(format!("{sid1}.jsonl"))).unwrap();
                    f.set_len(fl_core::vendor_adapter::MAX_CLAUDE_TRANSCRIPT_READ_BYTES + 1)
                        .unwrap();
                }
                Case::OverCap => {
                    write_claude_transcript(
                        home.path(),
                        false,
                        &sid1,
                        "claude-opus-4-6[1m]",
                        895_176,
                        false,
                    );
                }
                Case::RecastStaged => {
                    write_claude_transcript(
                        home.path(),
                        false,
                        &sid1,
                        "claude-opus-4-6[1m]",
                        10_000,
                        false,
                    );
                    let agent = store.agent_by_name("ada").unwrap().unwrap();
                    stage_recast_guidance(&store, &agent, &agent.project, &agent.role, "owner")
                        .unwrap();
                }
                Case::WindowShrunk => {
                    write_claude_transcript(
                        home.path(),
                        false,
                        &sid1,
                        "claude-opus-4-6[1m]",
                        50_000,
                        false,
                    );
                    next_req.model = Some("claude-sonnet-4-5".into());
                }
            }

            fake.clear();
            fake.launches.borrow_mut().clear();
            let mut stopped = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap();
            stopped.status = STATUS_STOPPED.to_string();
            persist_hosted_session(&store, &stopped, "owner").unwrap();

            let respun = spin_up(&store, &env, &next_req, "owner", Caller::Owner).unwrap();
            assert_eq!(respun.hydration_status, HydrationStatus::Unconfirmed);
            assert_ne!(respun.hydration_generation.as_deref(), Some(g1.as_str()));
            let after = find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap();
            assert_ne!(after.vendor_session_id.as_deref(), Some(sid1.as_str()));
            assert!(
                respun.warnings.iter().any(|w| w.contains(expected_tag)),
                "expected `{expected_tag}` in spin_up warnings: {:?}",
                respun.warnings
            );
            assert!(
                store
                    .get_state_by_key(
                        &StateScope::Workspace,
                        StateConcern::Runtime,
                        &hydration_ack_slug(&g1),
                    )
                    .unwrap()
                    .is_none(),
                "hydration-ack-<G1> must be pruned on spin_up fallback ({expected_tag})"
            );
        }
    }

    #[test]
    fn build_host_spawn_env_isolates_child_identity_from_ambient_parent_env() {
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = fake.env(home.path());
        let g_ada = next_hydration_generation();
        let sid_ada = uuid::Uuid::now_v7().to_string();

        let spec = resume_spec_for_with_session(
            &env,
            "claude",
            "ada",
            Some("claude-opus-5-5"),
            None,
            fl_core::permissions::Posture::Auto,
            None,
            &sid_ada,
            Some(&g_ada),
        )
        .unwrap()
        .expect("claude resume spec");

        let (env_map, env_allow) = build_host_spawn_env(Some(&spec), "claude:ada");
        assert_eq!(
            env_map.get("AGENT_PERSONA").map(String::as_str),
            Some("ada")
        );
        assert_eq!(
            env_map
                .get("AGENT_HYDRATION_GENERATION")
                .map(String::as_str),
            Some(g_ada.as_str())
        );
        assert_eq!(
            env_map.get("FL_HOSTED_ID").map(String::as_str),
            Some("claude:ada")
        );
        assert!(
            !env_map.contains_key("AGENT_NAME"),
            "AGENT_NAME must not be injected into env_map"
        );
        assert!(
            !env_allow.contains("AGENT_NAME"),
            "AGENT_NAME must be scrubbed from env_allow so ambient parent AGENT_NAME cannot leak"
        );

        // Blank launch with no persona or generation scrubs all four identity keys unless set.
        let (blank_map, blank_allow) = build_host_spawn_env(None, "");
        for key in [
            "AGENT_NAME",
            "AGENT_PERSONA",
            "AGENT_HYDRATION_GENERATION",
            "FL_HOSTED_ID",
        ] {
            assert!(
                !blank_map.contains_key(key),
                "{key} must not be in blank_map"
            );
            assert!(
                !blank_allow.contains(key),
                "{key} must not be in blank_allow"
            );
        }
    }

    #[test]
    fn claude_resume_round2_r1_r2_r3_r4_guards() {
        // R2 + Minor #7: survival window is bounded to 1s with 200ms poll interval (<= 5 checks).
        assert_eq!(CLAUDE_RESUME_SURVIVAL_WINDOW, Duration::from_secs(1));
        assert_eq!(CLAUDE_RESUME_POLL_INTERVAL, Duration::from_millis(200));

        // R1: Explicitly set `CLAUDE_CODE_MESSAGING_SOCKET` in the process environment and prove
        // `record_hydration_ack` and `--resume` still pass without reading ambient socket PIDs,
        // while `record_hydration_ack_with_pid` rejects a mismatched caller PID.
        struct RestoreMessagingSocket(Option<std::ffi::OsString>);
        impl Drop for RestoreMessagingSocket {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => unsafe { std::env::set_var("CLAUDE_CODE_MESSAGING_SOCKET", v) },
                    None => unsafe { std::env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET") },
                }
            }
        }
        let _socket_guard =
            RestoreMessagingSocket(std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET"));
        unsafe {
            std::env::set_var("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/999999.sock");
        }

        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        fake.set_unit_pid("Ada", 4242);
        let env = fake.env(home.path());

        let spun = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let g1 = spun.hydration_generation.unwrap();
        let sid1 = find_hosted_session(&store, &["claude:ada"])
            .unwrap()
            .unwrap()
            .vendor_session_id
            .unwrap();
        assert_eq!(
            find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .pid,
            Some(4242)
        );
        // Mismatched caller PID (99999 != 4242) is refused.
        assert!(
            !record_hydration_ack_with_pid(
                &store,
                "ada",
                true,
                Some(&g1),
                Some("claude:ada"),
                Some(99_999),
            )
            .unwrap(),
            "R1: mismatched caller_pid must be refused when record.pid is Some(4242)"
        );
        // `record_hydration_ack` (`caller_pid = None`) succeeds even with
        // `CLAUDE_CODE_MESSAGING_SOCKET=/tmp/999999.sock` set in `std::env` and preserves `record.pid == Some(4242)`.
        assert!(
            record_hydration_ack(&store, "ada", true, Some(&g1), Some("claude:ada")).unwrap(),
            "R1: record_hydration_ack with None caller_pid must succeed when CLAUDE_CODE_MESSAGING_SOCKET is set"
        );
        assert_eq!(
            find_hosted_session(&store, &["claude:ada"])
                .unwrap()
                .unwrap()
                .pid,
            Some(4242),
            "R1: record.pid must not be overwritten by ambient CLAUDE_CODE_MESSAGING_SOCKET"
        );

        // R3 + R4:
        // 1. Pre-restart route on disk (with `updated_at < launch_started_ms` or old generation)
        //    must NOT confirm a resume even if `recorded_pid` returns `None` on respawn!
        write_claude_transcript(home.path(), false, &sid1, "claude-opus-5-5", 10_000, false);
        let routes_path = home.path().join(".cache/agent-bus/routes.json");
        std::fs::create_dir_all(routes_path.parent().unwrap()).unwrap();
        let pre_restart_ms = 1_000u64;
        let launch_started_ms = 5_000u64;
        let stale_route_json = serde_json::json!({
            "version": 1,
            "entries": {
                "ada": {
                    "persona": "ada",
                    "route": {
                        "kind": "claude",
                        "pid": 4242,
                        "session_id": sid1,
                        "socket_path": home.path().join("claude.sock"),
                    },
                    "updated_at": At::from_unix_millis(pre_restart_ms),
                    "hosted_id": "claude:ada",
                    "hydration_generation": g1,
                }
            }
        });
        std::fs::write(
            &routes_path,
            serde_json::to_string_pretty(&stale_route_json).unwrap(),
        )
        .unwrap();

        assert!(
            !claude_route_confirmed_for_session(
                &env,
                "ada",
                "claude:ada",
                &sid1,
                Some(&g1),
                None,
                launch_started_ms,
            ),
            "R3: pre-restart route with updated_at < launch_started_ms must not confirm resume"
        );

        // Route at/after `launch_started_ms` with a mismatched generation is also rejected.
        let wrong_gen = next_hydration_generation();
        let wrong_gen_json = serde_json::json!({
            "version": 1,
            "entries": {
                "ada": {
                    "persona": "ada",
                    "route": {
                        "kind": "claude",
                        "pid": 4242,
                        "session_id": sid1,
                        "socket_path": home.path().join("claude.sock"),
                    },
                    "updated_at": At::from_unix_millis(launch_started_ms),
                    "hosted_id": "claude:ada",
                    "hydration_generation": wrong_gen,
                }
            }
        });
        std::fs::write(
            &routes_path,
            serde_json::to_string_pretty(&wrong_gen_json).unwrap(),
        )
        .unwrap();
        assert!(
            !claude_route_confirmed_for_session(
                &env,
                "ada",
                "claude:ada",
                &sid1,
                Some(&g1),
                Some(4242),
                launch_started_ms,
            ),
            "R3/R4: route with mismatched hydration_generation must not confirm resume"
        );

        // Route at/after `launch_started_ms` with matching `hosted_id`, `hydration_generation`, and PID confirms.
        let valid_json = serde_json::json!({
            "version": 1,
            "entries": {
                "ada": {
                    "persona": "ada",
                    "route": {
                        "kind": "claude",
                        "pid": 4242,
                        "session_id": sid1,
                        "socket_path": home.path().join("claude.sock"),
                    },
                    "updated_at": At::from_unix_millis(launch_started_ms),
                    "hosted_id": "claude:ada",
                    "hydration_generation": g1,
                }
            }
        });
        std::fs::write(
            &routes_path,
            serde_json::to_string_pretty(&valid_json).unwrap(),
        )
        .unwrap();
        assert!(
            claude_route_confirmed_for_session(
                &env,
                "ada",
                "claude:ada",
                &sid1,
                Some(&g1),
                Some(4242),
                launch_started_ms,
            ),
            "R3/R4: route at/after launch_started_ms with matching typed RouteEntry provenance must confirm"
        );

        // 2. When `recorded_pid` returns `None` on respawn after `record.pid` was `Some(4242)`,
        //    `restart_hosted_session` clears `record.pid` to `None` instead of keeping `Some(4242)`.
        std::fs::remove_file(&routes_path).unwrap();
        fake.clear();
        fake.unit_pids.borrow_mut().remove("Ada");
        let restarted = restart_hosted_session(&store, &env, "claude:ada", "owner").unwrap();
        assert_eq!(
            restarted.record.pid, None,
            "R3/R4: respawn when recorded_pid is None must clear stale record.pid"
        );
        assert_eq!(restarted.record.pid_start_time, None);

        // R2: Concurrent store-backed hub request (`state.open_store()` + `list_hosted_sessions`)
        // finishes *during* a resume watch (`restart_hosted_session_with_store`) because the
        // hub store guard is released across `resume_launch_survived`.
        let hub_dir = TempDir::new().unwrap();
        let hub_home = TempDir::new().unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: hub_dir.path().join("store.redb"),
            repo_root: hub_dir.path().to_path_buf(),
            home_override: Some(hub_home.path().to_path_buf()),
            ..crate::HubServerConfig::default()
        });

        let sid_concurrent = {
            let fake_init = FakeRuntime::default();
            let env_init = fake_init.env(hub_home.path());
            let guard = state.open_store().unwrap();
            let spun_c = spin_up_with_store(
                guard,
                || state.open_store().map_err(LifecycleError::from),
                &env_init,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g_c = spun_c.hydration_generation.unwrap();
            let guard = state.open_store().unwrap();
            let sid_c = find_hosted_session(&*guard, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&*guard, "ada", true, Some(&g_c), Some("claude:ada")).unwrap();
            sid_c
        };
        write_claude_transcript(
            hub_home.path(),
            false,
            &sid_concurrent,
            "claude-opus-5-5",
            10_000,
            false,
        );

        let watch_window = Duration::from_millis(300);
        let (concurrent_done_tx, concurrent_done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            let state_ref = &state;
            let reader = s.spawn(move || {
                // Wait 50ms so the main thread is mid-watch inside `resume_launch_survived`.
                std::thread::sleep(Duration::from_millis(50));
                let t0 = Instant::now();
                let guard = state_ref.open_store().unwrap();
                let sessions = list_hosted_sessions(&*guard).unwrap();
                drop(guard);
                let elapsed = t0.elapsed();
                concurrent_done_tx.send((sessions.len(), elapsed)).unwrap();
            });

            TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(Some(watch_window)));
            let fake_restart = FakeRuntime::default();
            let env_restart = fake_restart.env(hub_home.path());
            let restart_t0 = Instant::now();
            let guard = state.open_store().unwrap();
            let restarted_c = restart_hosted_session_with_store(
                guard,
                || state.open_store().map_err(LifecycleError::from),
                &env_restart,
                "claude:ada",
                "owner",
            )
            .unwrap();
            let restart_elapsed = restart_t0.elapsed();
            TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(None));

            assert_eq!(
                restarted_c.record.vendor_session_id.as_deref(),
                Some(sid_concurrent.as_str())
            );
            assert!(
                restart_elapsed >= Duration::from_millis(250),
                "expected resume watch to run for ~300ms, got {restart_elapsed:?}"
            );
            reader.join().unwrap();
        });

        let (count, reader_elapsed) = concurrent_done_rx.recv().unwrap();
        assert_eq!(count, 1);
        assert!(
            reader_elapsed < Duration::from_millis(120),
            "R2: concurrent store-backed hub request must finish during the 300ms resume watch without waiting for it to end (took {reader_elapsed:?})"
        );
    }

    #[test]
    fn claude_resume_round3_concurrent_restart_ack_and_decommission_during_watch() {
        let hub_dir = TempDir::new().unwrap();
        let hub_home = TempDir::new().unwrap();
        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: hub_dir.path().join("store.redb"),
            repo_root: hub_dir.path().to_path_buf(),
            home_override: Some(hub_home.path().to_path_buf()),
            ..crate::HubServerConfig::default()
        });

        let (sid, g1) = {
            let fake_init = FakeRuntime::default();
            let env_init = fake_init.env(hub_home.path());
            let guard = state.open_store().unwrap();
            let spun = spin_up_with_store(
                guard,
                || state.open_store().map_err(LifecycleError::from),
                &env_init,
                &req("ada", "assistant", "claude"),
                "owner",
                Caller::Owner,
            )
            .unwrap();
            let g = spun.hydration_generation.unwrap();
            let guard = state.open_store().unwrap();
            let sid = find_hosted_session(&*guard, &["claude:ada"])
                .unwrap()
                .unwrap()
                .vendor_session_id
                .unwrap();
            record_hydration_ack(&*guard, "ada", true, Some(&g), Some("claude:ada")).unwrap();
            (sid, g)
        };
        write_claude_transcript(
            hub_home.path(),
            false,
            &sid,
            "claude-opus-5-5",
            10_000,
            false,
        );

        let watch_window = Duration::from_millis(250);

        // 1. Two concurrent restarts of the same session produce ONE spawn, and the second waits
        //    on the per-session lifecycle lock and reports a Conflict.
        let (t2_tx, t2_rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            let state_ref = &state;
            let home_path = hub_home.path();
            let t2 = s.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let t0 = Instant::now();
                let fake2 = FakeRuntime::default();
                let env2 = fake2.env(home_path);
                let guard = state_ref.open_store().unwrap();
                let res = restart_hosted_session_with_store(
                    guard,
                    || state_ref.open_store().map_err(LifecycleError::from),
                    &env2,
                    "claude:ada",
                    "owner",
                );
                let elapsed = t0.elapsed();
                let spawns = fake2.launches.borrow().len();
                t2_tx.send((res, elapsed, spawns)).unwrap();
            });

            TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(Some(watch_window)));
            let fake1 = FakeRuntime::default();
            let env1 = fake1.env(hub_home.path());
            let guard = state.open_store().unwrap();
            let res1 = restart_hosted_session_with_store(
                guard,
                || state.open_store().map_err(LifecycleError::from),
                &env1,
                "claude:ada",
                "owner",
            );
            TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(None));
            assert!(res1.is_ok(), "first restart must succeed: {res1:?}");
            assert_eq!(
                fake1.launches.borrow().len(),
                1,
                "first restart must spawn exactly 1 runtime unit"
            );
            t2.join().unwrap();
        });
        let (res2, t2_elapsed, t2_spawns) = t2_rx.recv().unwrap();
        assert_eq!(
            t2_spawns, 0,
            "second concurrent restart must not spawn a duplicate unit"
        );
        assert!(
            matches!(res2, Err(LifecycleError::Conflict(_))),
            "second concurrent restart must report Conflict, got {res2:?}"
        );
        assert!(
            t2_elapsed >= Duration::from_millis(120),
            "second concurrent restart must wait on the per-session lock until the first finishes (took {t2_elapsed:?})"
        );
        {
            let guard = state.open_store().unwrap();
            delete_claude_resume_state(&*guard, "claude:ada");
        }

        // 2. A hydration ack (`record_hydration_ack_with_pid`) written during the survival watch
        //    survives the post-watch write-back (is not overwritten by a stale in-memory record).
        std::thread::scope(|s| {
            let state_ref = &state;
            let g1_ref = &g1;
            let sid_ref = &sid;
            let ack_thread = s.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let guard = state_ref.open_store().unwrap();
                // Pre-watch persistence wrote `pid = None` and `resume_unconfirmed_session_id = Some(sid)`.
                let mid_rec = find_hosted_session(&*guard, &["claude:ada"])
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    mid_rec.pid, None,
                    "pre-watch record in store should have cleared stale pid before dropping store_guard"
                );
                let mid_rstate = load_claude_resume_state(&*guard, "claude:ada");
                assert_eq!(
                    mid_rstate.resume_unconfirmed_session_id.as_deref(),
                    Some(sid_ref.as_str()),
                    "pre-watch resume_state must be persisted before dropping store_guard"
                );
                // Simulate the new process's SessionStart hook recording hydration ack + PID during watch:
                // also set hydration_status = Unconfirmed beforehand to verify it promotes back to Confirmed.
                let mut unconfirmed_rec = mid_rec.clone();
                unconfirmed_rec.hydration_status = HydrationStatus::Unconfirmed;
                persist_hosted_session(&*guard, &unconfirmed_rec, "ada").unwrap();
                let _ = guard.delete_state_by_key(
                    &StateScope::Workspace,
                    StateConcern::Runtime,
                    &hydration_ack_slug(g1_ref),
                );
                let acked = record_hydration_ack_with_pid(
                    &*guard,
                    "ada",
                    true,
                    Some(g1_ref),
                    Some("claude:ada"),
                    Some(7777),
                )
                .unwrap();
                assert!(acked, "hydration ack during watch should record receipt");
            });

            TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(Some(watch_window)));
            let fake_no_pid = FakeRuntime::default();
            fake_no_pid.unit_pids.borrow_mut().remove("Ada");
            let env_no_pid = fake_no_pid.env(hub_home.path());
            let guard = state.open_store().unwrap();
            let restarted = restart_hosted_session_with_store(
                guard,
                || state.open_store().map_err(LifecycleError::from),
                &env_no_pid,
                "claude:ada",
                "owner",
            )
            .unwrap();
            TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(None));
            ack_thread.join().unwrap();

            assert_eq!(
                restarted.record.pid, None,
                "an ack without runtime evidence must not supply a PID during the watch"
            );
            assert_eq!(
                restarted.record.hydration_status,
                HydrationStatus::Confirmed,
                "hydration confirmation written during watch must survive post-watch write-back"
            );
        });
        {
            let guard = state.open_store().unwrap();
            let persisted = find_hosted_session(&*guard, &["claude:ada"])
                .unwrap()
                .unwrap();
            assert_eq!(persisted.pid, None);
            assert_eq!(persisted.hydration_status, HydrationStatus::Confirmed);
            delete_claude_resume_state(&*guard, "claude:ada");
        }

        // 3. A decommission during the watch is NOT undone by the post-watch write-back, and the
        //    newly spawned runtime unit is stopped.
        std::thread::scope(|s| {
            let state_ref = &state;
            let home_path = hub_home.path();
            let decomm_thread = s.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let fake_d = FakeRuntime::default();
                let env_d = fake_d.env(home_path);
                let guard = state_ref.open_store().unwrap();
                let target = SessionTarget::resolve(&*guard, "claude:ada", None).unwrap();
                let resp =
                    teardown_session(&*guard, &env_d, &target, "owner", Caller::Owner, |_| {})
                        .unwrap();
                assert_eq!(resp.status, STATUS_DECOMMISSIONED);
            });

            TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(Some(watch_window)));
            let fake_r = FakeRuntime::default();
            let env_r = fake_r.env(hub_home.path());
            let guard = state.open_store().unwrap();
            let res = restart_hosted_session_with_store(
                guard,
                || state.open_store().map_err(LifecycleError::from),
                &env_r,
                "claude:ada",
                "owner",
            );
            TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(None));
            decomm_thread.join().unwrap();

            assert!(
                matches!(res, Err(LifecycleError::Conflict(_))),
                "restart must return Conflict when session was decommissioned during watch, got {res:?}"
            );
            assert!(
                !fake_r.exists("Ada"),
                "restart must kill the runtime unit it spawned when session was decommissioned during watch"
            );
        });
        {
            let guard = state.open_store().unwrap();
            let after_decomm = find_hosted_session(&*guard, &["claude:ada"])
                .unwrap()
                .unwrap();
            assert_eq!(
                after_decomm.status, STATUS_DECOMMISSIONED,
                "decommissioned status must not be overwritten by the finishing survival watch"
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn stale_route_and_pid_reuse_fall_through_to_runtime_presence_while_live_route_is_untouched() {
        let store = MemStore::default();
        let hub_home = TempDir::new().unwrap();
        let routes_path = hub_home.path().join(".cache/agent-bus/routes.json");
        let self_pid = std::process::id();
        let self_st = fl_wire::identity::proc_start_time_ticks(self_pid).expect("self start time");

        // 1. Live route (alive pid + matching pid_start_time) -> untouched even if runtime says Gone.
        let mut live_rec = hosted_record("claude", "live", STATUS_RUNNING, true);
        live_rec.pid = Some(self_pid);
        live_rec.pid_start_time = Some(self_st);
        persist_hosted_session(&store, &live_rec, "owner").unwrap();

        // 2. Stale route with dead pid (u32::MAX - 1) + RuntimePresence::Gone -> stopped after 2 ticks.
        let mut dead_rec = hosted_record("claude", "dead", STATUS_RUNNING, true);
        dead_rec.pid = Some(u32::MAX - 1);
        dead_rec.pid_start_time = Some(self_st);
        persist_hosted_session(&store, &dead_rec, "owner").unwrap();

        // 3a. PID reuse: alive pid (`self_pid`) with wrong `pid_start_time` -> not proven live, falls through to Gone.
        let mut reused_rec = hosted_record("claude", "reused", STATUS_RUNNING, true);
        reused_rec.pid = Some(self_pid);
        reused_rec.pid_start_time = Some(self_st.wrapping_add(999));
        persist_hosted_session(&store, &reused_rec, "owner").unwrap();

        // 3b. Bare PID without `pid_start_time` -> not proven live, falls through to Gone.
        let mut bare_rec = hosted_record("claude", "bare", STATUS_RUNNING, true);
        bare_rec.pid = Some(self_pid);
        bare_rec.pid_start_time = None;
        persist_hosted_session(&store, &bare_rec, "owner").unwrap();

        let mut table = RouteTable::default();
        let now = stamp::now();
        table.upsert(
            "live",
            VendorRoute::Claude {
                pid: self_pid,
                session_id: "sid-live".into(),
                socket_path: hub_home.path().join("live.sock"),
                key_path: None,
            },
            now.clone(),
        );
        table.entries.get_mut("live").unwrap().pid_start_time = Some(self_st);
        table.upsert(
            "dead",
            VendorRoute::Claude {
                pid: u32::MAX - 1,
                session_id: "sid-dead".into(),
                socket_path: hub_home.path().join("dead.sock"),
                key_path: None,
            },
            now.clone(),
        );
        table.entries.get_mut("dead").unwrap().pid_start_time = Some(self_st);
        table.upsert(
            "reused",
            VendorRoute::Claude {
                pid: self_pid,
                session_id: "sid-reused".into(),
                socket_path: hub_home.path().join("reused.sock"),
                key_path: None,
            },
            now.clone(),
        );
        table.entries.get_mut("reused").unwrap().pid_start_time = Some(self_st.wrapping_add(999));
        table.upsert(
            "bare",
            VendorRoute::Claude {
                pid: self_pid,
                session_id: "sid-bare".into(),
                socket_path: hub_home.path().join("bare.sock"),
                key_path: None,
            },
            now,
        );
        table.entries.get_mut("bare").unwrap().pid_start_time = None;
        table.save(Some(&routes_path)).unwrap();

        let fake = FakeRuntime::default();
        for unit in ["Live:claude", "Dead:claude", "Reused:claude", "Bare:claude"] {
            fake.presence_of
                .borrow_mut()
                .insert(unit.into(), RuntimePresence::Gone);
        }
        let env = fake.env(hub_home.path());

        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        for id in ["claude:live", "claude:dead", "claude:reused", "claude:bare"] {
            let rec = find_hosted_session(&store, &[id]).unwrap().unwrap();
            assert_eq!(
                rec.status, STATUS_RUNNING,
                "first Gone strike must not stop {id}"
            );
        }

        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();

        let live_after = find_hosted_session(&store, &["claude:live"])
            .unwrap()
            .unwrap();
        assert_eq!(live_after.status, STATUS_RUNNING);
        assert_eq!(live_after.stop_reason, None);

        for id in ["claude:dead", "claude:reused", "claude:bare"] {
            let rec = find_hosted_session(&store, &[id]).unwrap().unwrap();
            assert_eq!(rec.status, STATUS_STOPPED, "{id} should be stopped");
            assert_eq!(
                rec.stop_reason.as_deref(),
                Some(STOP_REASON_RUNTIME_GONE),
                "{id} should record runtime_gone"
            );
        }
    }

    #[test]
    fn reconcile_and_respawn_respawns_claude_resume_and_non_claude_fresh_after_two_gone_ticks() {
        let tmp = TempDir::new().unwrap();
        let hub_home = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: cwd.clone(),
            home_override: Some(hub_home.path().to_path_buf()),
            ..crate::HubServerConfig::default()
        });

        let claude_sid = "019d9000-0000-7000-8000-000000000482";
        write_claude_transcript(
            hub_home.path(),
            false,
            claude_sid,
            "claude-opus-5-5",
            10_000,
            false,
        );
        let mut claude_rec = hosted_record("claude", "ada", STATUS_RUNNING, true);
        claude_rec.cwd = Some(cwd.to_string_lossy().into_owned());
        claude_rec.vendor_session_id = Some(claude_sid.into());
        claude_rec.hydration_status = HydrationStatus::Confirmed;
        claude_rec.hydration_generation = Some(next_hydration_generation());
        claude_rec.pid = Some(u32::MAX - 1);
        claude_rec.pid_start_time = Some(12345);

        let mut codex_rec = hosted_record("codex", "bob", STATUS_RUNNING, true);
        codex_rec.cwd = Some(cwd.to_string_lossy().into_owned());
        codex_rec.pid = Some(u32::MAX - 2);
        codex_rec.pid_start_time = Some(12345);

        {
            let guard = state.open_store().unwrap();
            persist_hosted_session(&*guard, &claude_rec, "owner").unwrap();
            persist_hosted_session(&*guard, &codex_rec, "owner").unwrap();
        }

        // Write stale routes in routes.json (simulating same-boot host restart where routes.json still exists).
        let routes_path = hub_home.path().join(".cache/agent-bus/routes.json");
        let mut table = RouteTable::default();
        table.upsert(
            "ada",
            VendorRoute::Claude {
                pid: u32::MAX - 1,
                session_id: claude_sid.into(),
                socket_path: hub_home.path().join("ada.sock"),
                key_path: None,
            },
            stamp::now(),
        );
        table.entries.get_mut("ada").unwrap().pid_start_time = Some(12345);
        table.upsert(
            "bob",
            VendorRoute::Codex {
                thread_id: "thread-bob".into(),
                cwd: Some(cwd.clone()),
            },
            stamp::now(),
        );
        table.save(Some(&routes_path)).unwrap();

        let fake = FakeRuntime::default();
        fake.presence_of
            .borrow_mut()
            .insert("Ada:claude".into(), RuntimePresence::Gone);
        fake.presence_of
            .borrow_mut()
            .insert("Bob:codex".into(), RuntimePresence::Gone);
        let _trusted_codex = trusted_codex_home(hub_home.path(), &cwd);
        let env = fake.env(hub_home.path());

        // Tick 1: first Gone strike, no stop or respawn yet.
        let r1 = reconcile_and_respawn_hosted_sessions_with_store(
            state.open_store().unwrap(),
            || state.open_store().map_err(LifecycleError::from),
            &env,
            |_, _| {},
        )
        .unwrap();
        assert!(r1.is_empty());
        assert!(fake.launches.borrow().is_empty());

        // Tick 2: second Gone strike marks both stopped (runtime_gone) and immediately respawns them.
        TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(Some(Duration::from_millis(10))));
        let mut events = Vec::new();
        let r2 = reconcile_and_respawn_hosted_sessions_with_store(
            state.open_store().unwrap(),
            || state.open_store().map_err(LifecycleError::from),
            &env,
            |ev, payload| events.push((ev.to_string(), payload)),
        )
        .unwrap();
        TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(None));
        assert_eq!(r2.len(), 2);

        let launches = fake.launches.borrow();
        assert_eq!(launches.len(), 2);
        let claude_launch = launches
            .iter()
            .find(|(s, _)| s == "Ada:claude")
            .unwrap()
            .1
            .as_ref()
            .unwrap();
        assert!(
            claude_launch
                .argv
                .windows(2)
                .any(|w| w[0] == "--resume" && w[1] == claude_sid),
            "Claude respawn must preserve vendor_session_id via --resume: {:?}",
            claude_launch.argv
        );
        let codex_launch = launches
            .iter()
            .find(|(s, _)| s == "Bob:codex")
            .unwrap()
            .1
            .as_ref()
            .unwrap();
        assert!(
            codex_launch
                .argv
                .first()
                .is_some_and(|b| b.contains("codex")),
            "non-Claude respawn must launch fresh vendor command: {:?}",
            codex_launch
        );

        let guard = state.open_store().unwrap();
        let after_claude = find_hosted_session(&*guard, &["claude:ada"])
            .unwrap()
            .unwrap();
        assert_eq!(after_claude.status, STATUS_RUNNING);
        assert_eq!(after_claude.stop_reason, None);
        assert_eq!(after_claude.vendor_session_id.as_deref(), Some(claude_sid));
        assert_eq!(after_claude.restart_count, 1);

        let after_codex = find_hosted_session(&*guard, &["codex:bob"])
            .unwrap()
            .unwrap();
        assert_eq!(after_codex.status, STATUS_RUNNING);
        assert_eq!(after_codex.stop_reason, None);
        assert_eq!(after_codex.restart_count, 1);
    }

    #[test]
    fn clean_exit_and_legacy_stopped_or_decommissioned_sessions_are_not_respawned() {
        let tmp = TempDir::new().unwrap();
        let hub_home = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: cwd.clone(),
            home_override: Some(hub_home.path().to_path_buf()),
            ..crate::HubServerConfig::default()
        });

        // 4. Clean exit (exit_code = Some(0)) -> marked STATUS_STOPPED with stop_reason = Some("clean_exit"), no respawn.
        let mut clean_rec = hosted_record("claude", "clean", STATUS_RUNNING, true);
        clean_rec.cwd = Some(cwd.to_string_lossy().into_owned());

        // 5a. Legacy stopped record without `stop_reason = "runtime_gone"` (`status = STATUS_STOPPED, stop_reason = None`) -> no respawn.
        // Note: an external signal killing a running process transitions through `reconcile_dead_hosted_sessions`
        // (`stop_reason = Some("runtime_gone")`) and is respawned up to the 3-in-10m crash-loop guard; deliberate
        // operator retirement uses decommission (`STATUS_DECOMMISSIONED`).
        let mut legacy_stopped_rec = hosted_record("claude", "legacystop", STATUS_STOPPED, true);
        legacy_stopped_rec.cwd = Some(cwd.to_string_lossy().into_owned());
        legacy_stopped_rec.stop_reason = None;

        // 5b. Decommissioned record (`status = STATUS_DECOMMISSIONED`) -> never respawned.
        let mut decomm_rec = hosted_record("claude", "decomm", STATUS_DECOMMISSIONED, true);
        decomm_rec.cwd = Some(cwd.to_string_lossy().into_owned());

        {
            let guard = state.open_store().unwrap();
            persist_hosted_session(&*guard, &clean_rec, "owner").unwrap();
            persist_hosted_session(&*guard, &legacy_stopped_rec, "owner").unwrap();
            persist_hosted_session(&*guard, &decomm_rec, "owner").unwrap();
        }

        let fake = FakeRuntime::default();
        fake.presence_of
            .borrow_mut()
            .insert("Clean:claude".into(), RuntimePresence::Gone);
        fake.exit_codes
            .borrow_mut()
            .insert("Clean:claude".into(), 0);
        let env = fake.env(hub_home.path());

        // Two reconcile passes:
        reconcile_and_respawn_hosted_sessions_with_store(
            state.open_store().unwrap(),
            || state.open_store().map_err(LifecycleError::from),
            &env,
            |_, _| {},
        )
        .unwrap();
        let respawned = reconcile_and_respawn_hosted_sessions_with_store(
            state.open_store().unwrap(),
            || state.open_store().map_err(LifecycleError::from),
            &env,
            |_, _| {},
        )
        .unwrap();
        assert!(
            respawned.is_empty(),
            "neither clean_exit, legacy stopped, nor decommissioned sessions may respawn"
        );
        assert!(fake.launches.borrow().is_empty());

        let guard = state.open_store().unwrap();
        let after_clean = find_hosted_session(&*guard, &["claude:clean"])
            .unwrap()
            .unwrap();
        assert_eq!(after_clean.status, STATUS_STOPPED);
        assert_eq!(
            after_clean.stop_reason.as_deref(),
            Some(STOP_REASON_CLEAN_EXIT)
        );

        let after_legacy = find_hosted_session(&*guard, &["claude:legacystop"])
            .unwrap()
            .unwrap();
        assert_eq!(after_legacy.status, STATUS_STOPPED);
        assert_eq!(after_legacy.stop_reason, None);

        let after_decomm = find_hosted_session(&*guard, &["claude:decomm"])
            .unwrap()
            .unwrap();
        assert_eq!(after_decomm.status, STATUS_DECOMMISSIONED);
    }

    #[test]
    fn crash_loop_guard_enforces_backoff_trips_on_fourth_stop_notifies_ferris_and_resets_on_manual_restart()
     {
        let tmp = TempDir::new().unwrap();
        let hub_home = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: cwd.clone(),
            home_override: Some(hub_home.path().to_path_buf()),
            ..crate::HubServerConfig::default()
        });

        let mut rec = hosted_record("codex", "crashy", STATUS_RUNNING, true);
        rec.cwd = Some(cwd.to_string_lossy().into_owned());
        {
            let guard = state.open_store().unwrap();
            persist_hosted_session(&*guard, &rec, "owner").unwrap();
        }

        let _trusted_codex = trusted_codex_home(hub_home.path(), &cwd);
        let fake = FakeRuntime::default();
        let env = fake.env(hub_home.path());
        let base_ms: u64 = 1_000_000;

        let mark_gone_and_step = |clock_ms: u64, events: &mut Vec<(String, serde_json::Value)>| {
            TEST_RESPAWN_NOW_MS.with(|c| c.set(Some(clock_ms)));
            let res = reconcile_and_respawn_hosted_sessions_with_store(
                state.open_store().unwrap(),
                || state.open_store().map_err(LifecycleError::from),
                &env,
                |ev, payload| events.push((ev.to_string(), payload)),
            )
            .unwrap();
            TEST_RESPAWN_NOW_MS.with(|c| c.set(None));
            res
        };

        let simulate_crash = || {
            fake.live.borrow_mut().remove("Crashy");
            fake.presence_of
                .borrow_mut()
                .insert("Crashy:codex".into(), RuntimePresence::Gone);
        };

        let mut events = Vec::new();

        // Crash #1 at t = base_ms -> Respawn #1 immediately, sets 30s backoff (next_allowed = base_ms + 30_000).
        simulate_crash();
        assert!(mark_gone_and_step(base_ms, &mut events).is_empty());
        assert_eq!(mark_gone_and_step(base_ms, &mut events).len(), 1);
        assert_eq!(fake.launches.borrow().len(), 1);

        // Crash #2 at t = base_ms + 10_000 (before 30s backoff expires) -> stops, but blocked by 30s backoff.
        simulate_crash();
        assert!(mark_gone_and_step(base_ms + 5_000, &mut events).is_empty());
        assert!(
            mark_gone_and_step(base_ms + 10_000, &mut events).is_empty(),
            "must wait for 30s backoff before respawn #2"
        );
        // At t = base_ms + 30_000 -> Respawn #2 allowed, sets 60s backoff (next_allowed = base_ms + 90_000).
        assert_eq!(mark_gone_and_step(base_ms + 30_000, &mut events).len(), 1);
        assert_eq!(fake.launches.borrow().len(), 2);

        // Crash #3 at t = base_ms + 50_000 (before 60s backoff expires at +90_000) -> blocked until +90_000.
        simulate_crash();
        assert!(mark_gone_and_step(base_ms + 40_000, &mut events).is_empty());
        assert!(
            mark_gone_and_step(base_ms + 50_000, &mut events).is_empty(),
            "must wait for 60s backoff before respawn #3"
        );
        // At t = base_ms + 90_000 -> Respawn #3 allowed, sets 120s backoff (next_allowed = base_ms + 210_000).
        assert_eq!(mark_gone_and_step(base_ms + 90_000, &mut events).len(), 1);
        assert_eq!(fake.launches.borrow().len(), 3);

        // Crash #4 within 10 minutes (at t = base_ms + 100_000) -> trips crash-loop guard!
        // Because no Lead is registered yet on `ferroloop`, `stop_reason` becomes `crash_loop` and
        // `session.crash_loop` is broadcast, while `crash_loop_notified` stays `false` so the wire
        // notice will be retried on a subsequent pass once a Lead exists.
        simulate_crash();
        assert!(mark_gone_and_step(base_ms + 95_000, &mut events).is_empty());
        assert!(
            mark_gone_and_step(base_ms + 100_000, &mut events).is_empty(),
            "4th stop within 10m must trip crash-loop guard and not respawn"
        );
        assert_eq!(fake.launches.borrow().len(), 3);
        assert_eq!(
            events
                .iter()
                .filter(|(ev, _)| ev == "session.crash_loop")
                .count(),
            1,
            "must broadcast session.crash_loop once on transition to crash_loop"
        );
        {
            let guard = state.open_store().unwrap();
            let stopped = find_hosted_session(&*guard, &["codex:crashy"])
                .unwrap()
                .unwrap();
            assert_eq!(stopped.status, STATUS_STOPPED);
            assert_eq!(stopped.stop_reason.as_deref(), Some(STOP_REASON_CRASH_LOOP));
            let rstate = load_hosted_respawn_state(&*guard, "codex:crashy");
            assert!(
                !rstate.crash_loop_notified,
                "crash_loop_notified must stay false when no project Lead was registered yet"
            );

            // Register Ferris as Lead on `ferroloop`; the next reconcile pass must retry and deliver
            // the deferred wire notice even though `stop_reason` is already `crash_loop`.
            guard
                .register_agent(RegisterAgentArgs {
                    display_name: "Ferris".into(),
                    email: None,
                    project: "ferroloop".into(),
                    role: AgentRole::Lead,
                    extra_areas: vec![],
                    create_area: true,
                    replace: true,
                    at: stamp::now(),
                    by: "owner".into(),
                })
                .unwrap();
        }

        assert!(mark_gone_and_step(base_ms + 101_000, &mut events).is_empty());
        // A second pass after notification must not send a duplicate wire notice or event.
        assert!(mark_gone_and_step(base_ms + 102_000, &mut events).is_empty());

        {
            let guard = state.open_store().unwrap();
            let rstate = load_hosted_respawn_state(&*guard, "codex:crashy");
            assert!(
                rstate.crash_loop_notified,
                "crash_loop_notified must be saved as true after retry delivers wire notice"
            );

            let ferris_inbox = guard
                .list_wire_inbox(&fl_core::ferrowire::WireFilter {
                    recipient: Some("ferris".into()),
                    ..fl_core::ferrowire::WireFilter::default()
                })
                .unwrap();
            assert_eq!(
                ferris_inbox.len(),
                1,
                "must send exactly one crash-loop wire notice to project lead ferris on retry"
            );
            assert!(
                ferris_inbox[0]
                    .message
                    .subject
                    .contains("Crash loop: codex:crashy"),
                "unexpected subject: {}",
                ferris_inbox[0].message.subject
            );
        }
        assert_eq!(
            events
                .iter()
                .filter(|(ev, _)| ev == "session.crash_loop")
                .count(),
            1,
            "session.crash_loop must not be re-emitted on retry passes"
        );

        // Manual restart resets the crash-loop state and allows automatic respawn again.
        {
            let guard = state.open_store().unwrap();
            let manual = restart_hosted_session_with_store(
                guard,
                || state.open_store().map_err(LifecycleError::from),
                &env,
                "codex:crashy",
                "owner",
            )
            .unwrap();
            assert_eq!(manual.record.status, STATUS_RUNNING);
            assert_eq!(manual.record.stop_reason, None);
        }
        assert_eq!(fake.launches.borrow().len(), 4);

        // Subsequent crash after manual restart respawns cleanly without tripping crash_loop.
        simulate_crash();
        assert!(mark_gone_and_step(base_ms + 110_000, &mut events).is_empty());
        assert_eq!(
            mark_gone_and_step(base_ms + 115_000, &mut events).len(),
            1,
            "automatic respawn must work again after manual restart reset"
        );
        assert_eq!(fake.launches.borrow().len(), 5);
    }

    #[test]
    fn concurrent_reconcile_respawn_and_manual_restart_produce_only_one_spawn() {
        let tmp = TempDir::new().unwrap();
        let hub_home = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let state = crate::HubState::new(crate::HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: cwd.clone(),
            home_override: Some(hub_home.path().to_path_buf()),
            ..crate::HubServerConfig::default()
        });

        let sid = "019d9000-0000-7000-8000-000000000777";
        write_claude_transcript(
            hub_home.path(),
            false,
            sid,
            "claude-opus-5-5",
            10_000,
            false,
        );
        let mut rec = hosted_record("claude", "race", STATUS_STOPPED, true);
        rec.cwd = Some(cwd.to_string_lossy().into_owned());
        rec.vendor_session_id = Some(sid.into());
        rec.hydration_status = HydrationStatus::Confirmed;
        rec.hydration_generation = Some(next_hydration_generation());
        rec.stop_reason = Some(STOP_REASON_RUNTIME_GONE.into());
        {
            let guard = state.open_store().unwrap();
            persist_hosted_session(&*guard, &rec, "owner").unwrap();
        }

        // Case A: A manual `restart_hosted_session_with_store` runs deterministically between
        // reconcile's initial candidate collection (`list_hosted_sessions`, reopen call #1) and its
        // per-candidate re-read (`reopen_store()` inside the candidate loop, reopen call #2).
        // Reconcile must observe that the session is no longer `STATUS_STOPPED` +
        // `STOP_REASON_RUNTIME_GONE` and skip it without spawning a second runtime unit.
        TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(Some(Duration::from_millis(10))));
        let fake = FakeRuntime::default();
        let env = fake.env(hub_home.path());
        let reopen_calls = std::cell::Cell::new(0usize);
        let respawned = reconcile_and_respawn_hosted_sessions_with_store(
            state.open_store().unwrap(),
            || {
                let call = reopen_calls.get() + 1;
                reopen_calls.set(call);
                if call == 2 {
                    let manual_guard = state.open_store().map_err(LifecycleError::from)?;
                    let manual = restart_hosted_session_with_store(
                        manual_guard,
                        || state.open_store().map_err(LifecycleError::from),
                        &env,
                        "claude:race",
                        "owner",
                    )?;
                    assert_eq!(manual.record.status, STATUS_RUNNING);
                }
                state.open_store().map_err(LifecycleError::from)
            },
            &env,
            |_, _| {},
        )
        .unwrap();
        TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(None));

        assert_eq!(
            reopen_calls.get(),
            2,
            "reopen_store must run once for candidate listing and once for per-candidate re-read"
        );
        assert!(
            respawned.is_empty(),
            "reconcile must skip a candidate that manual restart already transitioned to running"
        );
        assert_eq!(
            fake.launches.borrow().len(),
            1,
            "manual restart + interleaved reconcile must spawn the session only once"
        );
        {
            let guard = state.open_store().unwrap();
            let after = get_hosted_session_by_id(&*guard, "claude:race")
                .unwrap()
                .unwrap();
            assert_eq!(after.status, STATUS_RUNNING);
            assert_eq!(after.restart_count, 1);
        }

        // Case B: When the per-session lifecycle mutex is held by an in-flight manual lifecycle
        // call, `reconcile_and_respawn_hosted_sessions_with_store` skips the locked candidate via
        // `try_lock()` without blocking or spawning. Hold the lock on a helper thread for a short
        // bounded duration so that if `try_lock()` were replaced by a blocking lock, reconcile
        // would wait and respawn the session (failing the assertions below rather than hanging).
        {
            let guard = state.open_store().unwrap();
            let mut stopped = get_hosted_session_by_id(&*guard, "claude:race")
                .unwrap()
                .unwrap();
            stopped.status = STATUS_STOPPED.into();
            stopped.stop_reason = Some(STOP_REASON_RUNTIME_GONE.into());
            stopped.restart_count = 0;
            persist_hosted_session(&*guard, &stopped, "owner").unwrap();
        }
        let fake_locked = FakeRuntime::default();
        let env_locked = fake_locked.env(hub_home.path());
        let mutex = hosted_session_lifecycle_mutex(&env_locked, "claude:race");
        let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _held_guard = mutex.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
        locked_rx.recv().unwrap();
        TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(Some(Duration::from_millis(10))));
        let skipped = reconcile_and_respawn_hosted_sessions_with_store(
            state.open_store().unwrap(),
            || state.open_store().map_err(LifecycleError::from),
            &env_locked,
            |_, _| {},
        )
        .unwrap();
        TEST_CLAUDE_RESUME_SURVIVAL_WINDOW.with(|c| c.set(None));
        let _ = release_tx.send(());
        holder.join().unwrap();
        assert!(
            skipped.is_empty(),
            "reconcile must skip a candidate whose per-session lifecycle mutex is held"
        );
        assert!(
            fake_locked.launches.borrow().is_empty(),
            "locked candidate must not be spawned by reconcile"
        );
        {
            let guard = state.open_store().unwrap();
            let after = get_hosted_session_by_id(&*guard, "claude:race")
                .unwrap()
                .unwrap();
            assert_eq!(after.status, STATUS_STOPPED);
            assert_eq!(after.restart_count, 0);
        }
    }

    #[test]
    fn claude_resume_survival_window_prevents_reconcile_dead_hosted_sessions_from_striking() {
        let store = MemStore::default();
        let hub_home = TempDir::new().unwrap();

        let rec = hosted_record("claude", "survive", STATUS_RUNNING, true);
        persist_hosted_session(&store, &rec, "owner").unwrap();

        // Mark session as in its Claude `--resume` survival window.
        save_claude_resume_state(
            &store,
            "claude:survive",
            &ClaudeResumeState {
                resume_unconfirmed_session_id: Some("019d9000-0000-7000-8000-000000000888".into()),
                resume_launched_at_ms: Some(now_ms()),
                in_survival_window: true,
                resume_failed: false,
            },
            "owner",
        )
        .unwrap();

        let fake = FakeRuntime::default();
        fake.presence_of
            .borrow_mut()
            .insert("Survive:claude".into(), RuntimePresence::Gone);
        let env = fake.env(hub_home.path());

        // Even across multiple reconcile passes while RuntimePresence::Gone, no strike is recorded.
        for _ in 0..3 {
            reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
            let mid = find_hosted_session(&store, &["claude:survive"])
                .unwrap()
                .unwrap();
            assert_eq!(
                mid.status, STATUS_RUNNING,
                "Claude session in survival window must not be struck by reconcile_dead_hosted_sessions"
            );
        }

        // Once survival window completes (`in_survival_window = false` and `resume_launched_at_ms = None`),
        // normal 2-strike detection applies.
        delete_claude_resume_state(&store, "claude:survive");
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(
            find_hosted_session(&store, &["claude:survive"])
                .unwrap()
                .unwrap()
                .status,
            STATUS_RUNNING
        );
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(
            find_hosted_session(&store, &["claude:survive"])
                .unwrap()
                .unwrap()
                .status,
            STATUS_STOPPED
        );

        // Regression (S1): A stale `in_survival_window: true` with `resume_launched_at_ms` 1 hour
        // in the past (or `None`) must NOT shield a Gone Claude session forever.
        for (persona, launched_at) in [
            ("stale-old", Some(now_ms().saturating_sub(3_600_000))),
            ("stale-none", None),
        ] {
            let sid = format!("claude:{persona}");
            let unit = hosted_tmux_target("claude", persona);
            let rec = hosted_record("claude", persona, STATUS_RUNNING, true);
            persist_hosted_session(&store, &rec, "owner").unwrap();
            save_claude_resume_state(
                &store,
                &sid,
                &ClaudeResumeState {
                    resume_unconfirmed_session_id: Some(
                        "019d9000-0000-7000-8000-000000000889".into(),
                    ),
                    resume_launched_at_ms: launched_at,
                    in_survival_window: true,
                    resume_failed: false,
                },
                "owner",
            )
            .unwrap();
            fake.presence_of
                .borrow_mut()
                .insert(unit, RuntimePresence::Gone);

            reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
            assert_eq!(
                get_hosted_session_by_id(&store, &sid)
                    .unwrap()
                    .unwrap()
                    .status,
                STATUS_RUNNING,
                "first Gone pass on stale survival window records strike 1 for {sid}"
            );
            assert!(
                !load_claude_resume_state(&store, &sid).in_survival_window,
                "stale in_survival_window must be cleared on disk for {sid}"
            );

            reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
            let stopped = get_hosted_session_by_id(&store, &sid).unwrap().unwrap();
            assert_eq!(
                stopped.status, STATUS_STOPPED,
                "second Gone pass on stale survival window must stop {sid}"
            );
            assert_eq!(
                stopped.stop_reason.as_deref(),
                Some(STOP_REASON_RUNTIME_GONE)
            );
        }
    }

    #[test]
    fn select_host_recorded_exit_code_and_pid_pick_latest_epoch_and_verify_expected_pid() {
        let wanted = fl_host_proto::SessionId::new("Ada--claude").unwrap();
        let mk_summary = |epoch: u64, pid: u32, code: Option<i32>, running: bool| {
            let rec = fl_host_proto::ChildRecord {
                session: wanted.clone(),
                session_epoch: fl_host_proto::SessionEpoch(epoch),
                kind: fl_host_proto::SpawnKind::Pty,
                pid,
                input: fl_host_proto::InputState::Writable,
                started_at_ms: 1_000,
                exit: (!running).then_some(fl_host_proto::ChildExit { code, signal: None }),
                consumer_stalled: false,
            };
            let state = if running {
                fl_host_proto::ChildState::Running(rec)
            } else {
                fl_host_proto::ChildState::LeaderExited(rec)
            };
            fl_host_proto::SessionSummary {
                stream: fl_host_proto::StreamId {
                    host: fl_host_proto::HostInstance::new([0u8; 16]),
                    session: wanted.clone(),
                    epoch: fl_host_proto::SessionEpoch(epoch),
                },
                mode: fl_host_proto::SessionMode::Pty,
                state,
                acked: 0,
                latest: 0,
                input: fl_host_proto::InputState::Writable,
            }
        };

        let e1_fail = mk_summary(1, 1111, Some(1), false);
        let e2_clean = mk_summary(2, 2222, Some(0), false);
        // Both slice orders must pick epoch 2 (exit code 0).
        assert_eq!(
            select_host_recorded_exit_code(&[e1_fail.clone(), e2_clean.clone()], &wanted, None),
            Some(0)
        );
        assert_eq!(
            select_host_recorded_exit_code(&[e2_clean.clone(), e1_fail], &wanted, None),
            Some(0)
        );

        let e1_clean = mk_summary(1, 1111, Some(0), false);
        let e2_fail = mk_summary(2, 2222, Some(1), false);
        // Reverse codes: both slice orders must pick epoch 2 (exit code 1).
        assert_eq!(
            select_host_recorded_exit_code(&[e1_clean.clone(), e2_fail.clone()], &wanted, None),
            Some(1)
        );
        assert_eq!(
            select_host_recorded_exit_code(&[e2_fail.clone(), e1_clean.clone()], &wanted, None),
            Some(1)
        );

        // PID mismatch on the latest epoch (`r.pid = 2222`, `expected_pid = Some(1111)`) returns None.
        assert_eq!(
            select_host_recorded_exit_code(std::slice::from_ref(&e2_clean), &wanted, Some(1111)),
            None
        );
        assert_eq!(
            select_host_recorded_exit_code(std::slice::from_ref(&e2_clean), &wanted, Some(2222)),
            Some(0)
        );

        // When the highest epoch is `Running`, `select_host_recorded_exit_code` returns `None`
        // even if an older epoch exited cleanly.
        let e1_run = mk_summary(1, 1111, None, true);
        let e2_run = mk_summary(2, 2222, None, true);
        assert_eq!(
            select_host_recorded_exit_code(&[e1_clean.clone(), e2_run.clone()], &wanted, None),
            None
        );
        assert_eq!(
            select_host_recorded_exit_code(&[e2_run.clone(), e1_clean], &wanted, None),
            None
        );

        // `select_host_recorded_pid` prefers Running and highest epoch regardless of slice order.
        assert_eq!(
            select_host_recorded_pid(&[e1_run.clone(), e2_run.clone()], &wanted),
            Some(2222)
        );
        assert_eq!(
            select_host_recorded_pid(&[e2_run, e1_run], &wanted),
            Some(2222)
        );

        // A wrapper leader and its vendor child are distinct but belong to the same launch.
        let wrapper_store = MemStore::default();
        let wrapper_home = TempDir::new().unwrap();
        let wrapper_runtime = FakeRuntime::default();
        let mut wrapper = hosted_record("claude", "wrapper", STATUS_RUNNING, true);
        wrapper.pid = Some(1111);
        wrapper.runtime_leader_pid = Some(2222);
        persist_hosted_session(&wrapper_store, &wrapper, "owner").unwrap();
        wrapper_runtime
            .presence_of
            .borrow_mut()
            .insert("Wrapper:claude".into(), RuntimePresence::Gone);
        wrapper_runtime
            .exit_codes
            .borrow_mut()
            .insert("Wrapper:claude".into(), 0);
        wrapper_runtime.set_unit_pid("Wrapper:claude", 2222);
        let wrapper_env = wrapper_runtime.env(wrapper_home.path());
        reconcile_dead_hosted_sessions(&wrapper_store, &wrapper_env, "owner").unwrap();
        reconcile_dead_hosted_sessions(&wrapper_store, &wrapper_env, "owner").unwrap();
        let stopped = get_hosted_session_by_id(&wrapper_store, "claude:wrapper")
            .unwrap()
            .unwrap();
        assert_eq!(stopped.stop_reason.as_deref(), Some(STOP_REASON_CLEAN_EXIT));

        // Reconcile-level check (N5): when `recorded_exit_code` sees a PID mismatch on a `Gone`
        // session (`record.pid = Some(1111)`, runtime exit PID = `2222`, exit code = `0`),
        // `reconcile_dead_hosted_sessions` classifies the stop as `runtime_gone`, never `clean_exit`.
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let mut rec = hosted_record("claude", "pidmis", STATUS_RUNNING, true);
        rec.pid = Some(1111);
        rec.runtime_leader_pid = Some(1111);
        persist_hosted_session(&store, &rec, "owner").unwrap();
        fake.presence_of
            .borrow_mut()
            .insert("Pidmis:claude".into(), RuntimePresence::Gone);
        fake.exit_codes
            .borrow_mut()
            .insert("Pidmis:claude".into(), 0);
        fake.set_unit_pid("Pidmis:claude", 2222);
        let env = fake.env(home.path());
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        let stopped = get_hosted_session_by_id(&store, "claude:pidmis")
            .unwrap()
            .unwrap();
        assert_eq!(stopped.status, STATUS_STOPPED);
        assert_eq!(
            stopped.stop_reason.as_deref(),
            Some(STOP_REASON_RUNTIME_GONE),
            "PID mismatch on clean exit code 0 must classify stop as runtime_gone, not clean_exit"
        );
    }

    #[test]
    fn resolve_project_lead_prefers_own_project_lead_over_extra_areas_lead() {
        let store = MemStore::default();
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Alpha".into(),
                email: None,
                project: "other".into(),
                role: AgentRole::Lead,
                extra_areas: vec!["ferroloop".into()],
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap();
        // When no primary Lead exists on `ferroloop`, fallback selects the extra_areas Lead.
        assert_eq!(
            resolve_project_lead(&store, "ferroloop")
                .unwrap()
                .unwrap()
                .name_key,
            "alpha"
        );

        // Once `ferris` is registered with primary `project = "ferroloop"`, `resolve_project_lead`
        // selects `ferris` even though `alpha` sorts earlier by name.
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Ferris".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap();
        assert_eq!(
            resolve_project_lead(&store, "ferroloop")
                .unwrap()
                .unwrap()
                .name_key,
            "ferris"
        );
    }

    #[test]
    fn dead_streak_enforces_ten_second_minimum_spacing() {
        struct ResetSpacingGuard;
        impl Drop for ResetSpacingGuard {
            fn drop(&mut self) {
                TEST_DEAD_STREAK_MIN_SPACING_MS.with(|c| c.set(None));
                TEST_RESPAWN_NOW_MS.with(|c| c.set(None));
            }
        }
        let _guard = ResetSpacingGuard;
        TEST_DEAD_STREAK_MIN_SPACING_MS.with(|c| c.set(Some(DEAD_STREAK_MIN_SPACING_MS)));

        let (store, fake, home) = script_presence("spacedgone", "grok", RuntimePresence::Gone);
        let env = fake.env(home.path());

        // Pass 1 at t = 1_000_000 -> strike 1, stays running.
        TEST_RESPAWN_NOW_MS.with(|c| c.set(Some(1_000_000)));
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(hosted_status(&store, "grok:spacedgone"), STATUS_RUNNING);

        // Pass 2 at t = 1_002_000 (2s later < 10s minimum spacing) -> still strike 1, stays running.
        TEST_RESPAWN_NOW_MS.with(|c| c.set(Some(1_002_000)));
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        assert_eq!(
            hosted_status(&store, "grok:spacedgone"),
            STATUS_RUNNING,
            "second Gone observation within 10s must not count as strike 2"
        );

        // Pass 3 at t = 1_010_500 (10.5s after strike 1) -> strike 2, transitions to stopped.
        TEST_RESPAWN_NOW_MS.with(|c| c.set(Some(1_010_500)));
        reconcile_dead_hosted_sessions(&store, &env, "owner").unwrap();
        let after = get_hosted_session_by_id(&store, "grok:spacedgone")
            .unwrap()
            .unwrap();
        assert_eq!(after.status, STATUS_STOPPED);
        assert_eq!(after.stop_reason.as_deref(), Some(STOP_REASON_RUNTIME_GONE));

        // Expiry check (N4): a first strike older than `DEAD_STREAK_MAX_AGE_MS` (2 minutes) resets
        // to strike 1 instead of stopping the session on a single new `Gone` observation.
        let (store2, fake2, home2) = script_presence("expiredgone", "grok", RuntimePresence::Gone);
        let env2 = fake2.env(home2.path());
        TEST_RESPAWN_NOW_MS.with(|c| c.set(Some(2_000_000)));
        reconcile_dead_hosted_sessions(&store2, &env2, "owner").unwrap();
        assert_eq!(hosted_status(&store2, "grok:expiredgone"), STATUS_RUNNING);

        // 2 minutes + 1ms later: stale strike 1 expires and resets to a fresh strike 1.
        TEST_RESPAWN_NOW_MS.with(|c| c.set(Some(2_000_000 + DEAD_STREAK_MAX_AGE_MS + 1)));
        reconcile_dead_hosted_sessions(&store2, &env2, "owner").unwrap();
        assert_eq!(
            hosted_status(&store2, "grok:expiredgone"),
            STATUS_RUNNING,
            "Gone observation after >2m must reset expired strike 1 rather than stopping the session"
        );

        // 15s after the fresh strike 1: strike 2 stops the session.
        TEST_RESPAWN_NOW_MS.with(|c| c.set(Some(2_000_000 + DEAD_STREAK_MAX_AGE_MS + 15_001)));
        reconcile_dead_hosted_sessions(&store2, &env2, "owner").unwrap();
        assert_eq!(hosted_status(&store2, "grok:expiredgone"), STATUS_STOPPED);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn spin_up_restart_resume_and_hydration_ack_record_and_upgrade_vendor_cli_pid() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;

        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();

        let vendor_bin = home.path().join("grok");
        std::os::unix::fs::symlink("/bin/sh", &vendor_bin).unwrap();
        let child_cli_script = r#"
            echo "CHILD_CLI_PID=$$"
            sh -c 'echo "CHILD_SH_PID=$$"; sleep 60; :' &
            wait
        "#;
        let vendor_script = r#"
            echo "VENDOR_PID=$$"
            sh -c 'echo "HOOK_SH_PID=$$"; sleep 60; :' &
            "$1" -c "$2" _ &
            wait
        "#;
        let pane_script = r#"
            sh -c '"$1" -c "$2" _ "$1" "$3"; :' _ "$1" "$2" "$3" &
            wait
        "#;
        let mut pane_shell = std::process::Command::new("sh")
            .args([
                "-c",
                pane_script,
                "_",
                vendor_bin.to_str().unwrap(),
                vendor_script,
                child_cli_script,
            ])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let pane_pid = pane_shell.id();
        let stdout = pane_shell.stdout.take().unwrap();
        let mut vendor_pid = None;
        let mut hook_sh_pid = None;
        let mut child_cli_pid = None;
        let mut child_sh_pid = None;
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some(v) = line.trim().strip_prefix("VENDOR_PID=") {
                vendor_pid = v.parse::<u32>().ok();
            } else if let Some(v) = line.trim().strip_prefix("HOOK_SH_PID=") {
                hook_sh_pid = v.parse::<u32>().ok();
            } else if let Some(v) = line.trim().strip_prefix("CHILD_CLI_PID=") {
                child_cli_pid = v.parse::<u32>().ok();
            } else if let Some(v) = line.trim().strip_prefix("CHILD_SH_PID=") {
                child_sh_pid = v.parse::<u32>().ok();
            }
            if vendor_pid.is_some()
                && hook_sh_pid.is_some()
                && child_cli_pid.is_some()
                && child_sh_pid.is_some()
            {
                break;
            }
        }
        let vendor_pid = vendor_pid.unwrap();
        let hook_sh_pid = hook_sh_pid.unwrap();
        let child_cli_pid = child_cli_pid.unwrap();
        let child_sh_pid = child_sh_pid.unwrap();
        let vendor_st = fl_wire::identity::proc_start_time_ticks(vendor_pid);
        assert!(vendor_st.is_some());

        // 1. `spin_up` with `FakeRuntime` returning `pane_pid` (wrapper shell) resolves to `vendor_pid`
        //    and populates `record.pid` and `record.pid_start_time`.
        fake.set_unit_pid("Ada", pane_pid);
        let env = fake.env(home.path());
        let spun = spin_up(
            &store,
            &env,
            &req("ada", "assistant", "grok"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let g1 = spun.hydration_generation.unwrap();
        let rec1 = get_hosted_session_by_id(&store, "grok:ada")
            .unwrap()
            .unwrap();
        assert_eq!(rec1.pid, Some(vendor_pid));
        assert_eq!(rec1.pid_start_time, vendor_st);

        // A caller cannot establish the trusted PID for a pid-less record.
        let mut rec_none = rec1.clone();
        rec_none.pid = None;
        rec_none.pid_start_time = None;
        rec_none.hydration_status = HydrationStatus::Unconfirmed;
        persist_hosted_session(&store, &rec_none, "owner").unwrap();
        assert!(
            record_hydration_ack_with_pid(
                &store,
                "ada",
                true,
                Some(&g1),
                Some("grok:ada"),
                Some(child_sh_pid),
            )
            .unwrap()
        );
        let after_none = get_hosted_session_by_id(&store, "grok:ada")
            .unwrap()
            .unwrap();
        assert_eq!(after_none.pid, None);
        assert_eq!(after_none.pid_start_time, None);
        assert_eq!(after_none.hydration_status, HydrationStatus::Confirmed);

        // The runtime's independent PID rejects a child and accepts the hosted caller.
        let _ = store.delete_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            &hydration_ack_slug(&g1),
        );
        persist_hosted_session(&store, &rec_none, "owner").unwrap();
        assert!(
            !record_hydration_ack_with_runtime(
                &store,
                "ada",
                true,
                Some(&g1),
                Some("grok:ada"),
                Some(child_sh_pid),
                Some(&fake)
            )
            .unwrap()
        );
        assert_eq!(
            get_hosted_session_by_id(&store, "grok:ada")
                .unwrap()
                .unwrap()
                .pid,
            None
        );
        assert!(
            record_hydration_ack_with_runtime(
                &store,
                "ada",
                true,
                Some(&g1),
                Some("grok:ada"),
                Some(hook_sh_pid),
                Some(&fake)
            )
            .unwrap()
        );
        assert_eq!(
            get_hosted_session_by_id(&store, "grok:ada")
                .unwrap()
                .unwrap()
                .pid,
            Some(vendor_pid)
        );

        // 3. When `rec.pid` was recorded as the wrapper shell (`pane_pid`), a child CLI (`child_cli_pid`
        //    or `child_sh_pid`) is rejected, while the genuine `hook_sh_pid` upgrades `rec.pid` to `vendor_pid`!
        let _ = store.delete_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            &hydration_ack_slug(&g1),
        );
        let mut rec_shell = rec1.clone();
        rec_shell.runtime_leader_pid = None;
        rec_shell.pid = Some(pane_pid);
        rec_shell.pid_start_time = fl_wire::identity::proc_start_time_ticks(pane_pid);
        rec_shell.hydration_status = HydrationStatus::Unconfirmed;
        persist_hosted_session(&store, &rec_shell, "owner").unwrap();

        for bad_pid in [child_cli_pid, child_sh_pid] {
            assert!(
                !record_hydration_ack_with_pid(
                    &store,
                    "ada",
                    true,
                    Some(&g1),
                    Some("grok:ada"),
                    Some(bad_pid),
                )
                .unwrap(),
                "child CLI PID {bad_pid} must be rejected when rec.pid is wrapper shell"
            );
        }
        assert_eq!(
            get_hosted_session_by_id(&store, "grok:ada")
                .unwrap()
                .unwrap()
                .pid,
            Some(pane_pid)
        );

        assert!(
            record_hydration_ack_with_pid(
                &store,
                "ada",
                true,
                Some(&g1),
                Some("grok:ada"),
                Some(hook_sh_pid),
            )
            .unwrap(),
            "genuine hook_sh_pid must upgrade wrapper shell rec.pid to vendor_pid"
        );
        let upgraded = get_hosted_session_by_id(&store, "grok:ada")
            .unwrap()
            .unwrap();
        assert_eq!(upgraded.pid, Some(vendor_pid));
        assert_eq!(upgraded.pid_start_time, vendor_st);
        assert_eq!(upgraded.runtime_leader_pid, Some(pane_pid));
        assert_eq!(upgraded.hydration_status, HydrationStatus::Confirmed);

        // 4. Once `rec.pid` is `Some(vendor_pid)`, a child CLI (`child_cli_pid` / `child_sh_pid`) is rejected.
        let _ = store.delete_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            &hydration_ack_slug(&g1),
        );
        for bad_pid in [child_cli_pid, child_sh_pid] {
            assert!(
                !record_hydration_ack_with_pid(
                    &store,
                    "ada",
                    true,
                    Some(&g1),
                    Some("grok:ada"),
                    Some(bad_pid),
                )
                .unwrap()
            );
        }

        // A replacement runtime with the same hosted id cannot overwrite a live
        // reference belonging to a different process instance.
        let mut live_other = upgraded.clone();
        live_other.pid = Some(child_cli_pid);
        live_other.pid_start_time = fl_wire::identity::proc_start_time_ticks(child_cli_pid);
        live_other.runtime_leader_pid = Some(child_cli_pid);
        persist_hosted_session(&store, &live_other, "owner").unwrap();
        reconcile_running_session_pids_with_store(&env, &store).unwrap();
        let retained = get_hosted_session_by_id(&store, "grok:ada")
            .unwrap()
            .unwrap();
        assert_eq!(retained.pid, Some(child_cli_pid));
        assert_eq!(retained.runtime_leader_pid, Some(child_cli_pid));
        // An unproven reference (no start time) yields to the owned runtime PID.
        live_other.pid_start_time = None;
        persist_hosted_session(&store, &live_other, "owner").unwrap();
        reconcile_running_session_pids_with_store(&env, &store).unwrap();
        let refreshed = get_hosted_session_by_id(&store, "grok:ada")
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.pid, Some(vendor_pid));
        assert_eq!(refreshed.runtime_leader_pid, Some(pane_pid));

        // 5. `reconcile_running_session_pids_with_store` backfills `pid` and `pid_start_time` when
        //    `record.pid` was `None` or a wrapper shell (`pane_pid`).
        let mut rec_clear = upgraded;
        rec_clear.pid = None;
        rec_clear.pid_start_time = None;
        persist_hosted_session(&store, &rec_clear, "owner").unwrap();
        assert_eq!(
            reconcile_running_session_pids_with_store(&env, &store).unwrap(),
            1
        );
        let reconciled = get_hosted_session_by_id(&store, "grok:ada")
            .unwrap()
            .unwrap();
        assert_eq!(reconciled.pid, Some(vendor_pid));
        assert_eq!(reconciled.pid_start_time, vendor_st);

        // 6. Claude `--resume` fallback to fresh launch when survival watch fails still populates
        //    `record.pid` and `record.pid_start_time` from the fresh spawn.
        fake.set_unit_pid("Bob", pane_pid);
        let spun_claude = spin_up(
            &store,
            &env,
            &req("bob", "assistant", "claude"),
            "owner",
            Caller::Owner,
        )
        .unwrap();
        let g_bob = spun_claude.hydration_generation.unwrap();
        let sid_bob = get_hosted_session_by_id(&store, "claude:bob")
            .unwrap()
            .unwrap()
            .vendor_session_id
            .unwrap();
        assert!(
            record_hydration_ack_with_pid(
                &store,
                "bob",
                true,
                Some(&g_bob),
                Some("claude:bob"),
                Some(vendor_pid),
            )
            .unwrap()
        );
        write_claude_transcript(
            home.path(),
            false,
            &sid_bob,
            "claude-opus-5-5",
            10_000,
            false,
        );
        fake.kill("Bob", "claude:bob");
        fake.set_unit_pid("Bob", pane_pid);
        fake.vanish_once_windows.borrow_mut().push("claude".into());
        let restarted_bob = restart_hosted_session(&store, &env, "claude:bob", "owner").unwrap();
        assert_ne!(
            restarted_bob.record.vendor_session_id.as_deref(),
            Some(sid_bob.as_str()),
            "fallback fresh launch must mint a new vendor_session_id"
        );
        assert_eq!(restarted_bob.record.pid, Some(vendor_pid));
        assert_eq!(restarted_bob.record.pid_start_time, vendor_st);

        unsafe {
            libc::kill(-(pane_pid as i32), libc::SIGKILL);
        }
        let _ = pane_shell.wait();
    }
}
