//! The user-level binding of projects to stores (spec §2.6).
//!
//! User-level, not in the repository: a store path belongs to a machine, and
//! a public repository would publish it.
//!
//! ⚠ A config file that exists but cannot be read is an ERROR, never a
//! silent fall-through to the default store. Falling through would put a
//! project's records in a store nobody chose.

use anyhow::{Context, Result, bail};
use fl_core::pr_policy::{DEFAULT_REVIEWER_APP_IDS, ReviewerApps};
use fl_core::state::{StateScope, validate_slug};
use fl_core::telemetry::{AdapterKind, RateCard};
use fl_core::vendor::{
    ApiDialect, VendorDescriptor, VendorRegistry, VendorTransport, validate_api_base_url,
};
use fl_exec::state_sync::StateSyncTarget;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Default branch name for git-backed state synchronization.
pub const DEFAULT_STATE_BRANCH: &str = "fl/state";

/// Workspace-level `[state]` configuration (spec §4.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    /// Optional workspace state store path; defaults to `$XDG_DATA_HOME/fl/fl.redb`.
    #[serde(default)]
    pub store: Option<PathBuf>,
    /// Default git repository for workspace/group/agent/workflow state sync.
    #[serde(default)]
    pub repo: Option<PathBuf>,
    /// Git remote name (defaults to `"origin"`).
    #[serde(default)]
    pub remote: Option<String>,
    /// Git branch name (defaults to `"fl/state"`).
    #[serde(default)]
    pub branch: Option<String>,
    /// Whether the target repository is private and permitted to receive sensitive entries.
    #[serde(default)]
    pub private: bool,
}

/// Per-project `[[project]].state` override (spec §4.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectStateConfig {
    /// Optional override repository for this project's `fl/state` branch.
    #[serde(default)]
    pub repo: Option<PathBuf>,
    /// Optional override remote name (defaults to `[state].remote` or `"origin"`).
    #[serde(default)]
    pub remote: Option<String>,
    /// Optional override branch name (defaults to `"fl/state"`).
    #[serde(default)]
    pub branch: Option<String>,
    /// Whether this project's state target is private.
    #[serde(default)]
    pub private: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    #[serde(default)]
    pub name: Option<String>,
    pub root: PathBuf,
    pub store: PathBuf,
    /// `None`: the tracker is the local store.
    #[serde(default)]
    pub tracker: Option<TrackerBinding>,
    #[serde(default)]
    pub state: Option<ProjectStateConfig>,
}

impl Entry {
    /// Matches `<slug>` by explicit `name` or by lowercase basename of `root` (spec §4.3).
    pub fn matches_slug(&self, slug: &str) -> bool {
        if let Some(ref name) = self.name {
            return name.eq_ignore_ascii_case(slug);
        }
        self.root
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|base| base.eq_ignore_ascii_case(slug))
    }

    /// Derive the normalized project `<slug>` from explicit `name` or lowercase basename of `root`.
    pub fn inferred_slug(&self) -> Option<String> {
        let raw = match &self.name {
            Some(n) => n.clone(),
            None => self
                .root
                .file_name()
                .and_then(|s| s.to_str())?
                .to_ascii_lowercase(),
        };
        validate_slug(&raw).ok()
    }
}

/// A project's tracker when it is not the local store (GitHub tracker spec
/// §1.4): `tracker = { github = "owner/repo", credential = "env" }`, and,
/// for mode B, `ledger = "github"` (GitHub ledger spec §1.5).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerBinding {
    pub github: String,
    pub credential: Credential,
    /// `Some(Github)`: decisions publish their evidence to the repository's
    /// `fl/ledger` branch. `None`: the ledger is the local store.
    ///
    /// ⚠ Part of the binding's identity: two entries for one store that
    /// differ here are two trackers, and refused.
    #[serde(default)]
    pub ledger: Option<LedgerChoice>,
}

impl TrackerBinding {
    /// Whether this binding names the GitHub ledger.
    pub fn github_ledger(&self) -> bool {
        self.ledger == Some(LedgerChoice::Github)
    }
}

/// The ledger a tracker binding names. One value: the GitHub ledger, which
/// always lives in the tracker's repository (spec §1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum LedgerChoice {
    Github,
}

impl TryFrom<String> for LedgerChoice {
    type Error = String;

    /// ⚠ Exactly `github`: any other value is refused by name, never read
    /// as the local ledger — a person who wrote it meant something.
    fn try_from(value: String) -> Result<Self, String> {
        match value.as_str() {
            "github" => Ok(LedgerChoice::Github),
            other => Err(format!(
                "`ledger = \"{other}\"` is not a ledger fl knows. Its only value is \"github\", \
                 the GitHub ledger in the tracker's repository; leave `ledger` out to keep every \
                 run and decision in the local store"
            )),
        }
    }
}

/// Where the GitHub credential comes from. One source, and no fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    App,
    Env,
}

/// `[github]`: the App fl writes as when a binding says `credential = "app"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubApp {
    pub app_id: u64,
    pub private_key: PathBuf,
    /// GitHub user ids whose reviews may carry an `fl-review` agent marker, normally the
    /// Reviewer App's bot user. A marker from any other account counts for nothing, so an
    /// outsider cannot claim to be an agent. Read it through [`reviewer_apps`].
    #[serde(default)]
    pub reviewer_ids: Option<Vec<u64>>,
    /// GitHub logins whose reviews may carry an `fl-review` agent marker, matched without
    /// case. A login is not stable identity: GitHub frees an App's old slug when the App is
    /// renamed, so prefer `reviewer_ids`. Read it through [`reviewer_apps`].
    #[serde(default)]
    pub reviewer_logins: Option<Vec<String>>,
}

/// The accounts whose reviews may carry an agent marker (`[github] reviewer_ids` and
/// `reviewer_logins`). When neither key is set, or there is no `[github]` table, it is
/// [`ReviewerApps::default`]: the ids in [`DEFAULT_REVIEWER_APP_IDS`] and no logins. When
/// either key is set, the two keys are the whole set and an absent key is empty, so
/// `reviewer_logins = []` or `reviewer_ids = []` alone means no agent review counts.
/// `verify-ci` can run on a machine with no App credentials, so a missing `[github]` is not an
/// error.
pub fn reviewer_apps(app: Option<&GithubApp>) -> ReviewerApps {
    match app {
        Some(GithubApp {
            reviewer_ids,
            reviewer_logins,
            ..
        }) if reviewer_ids.is_some() || reviewer_logins.is_some() => ReviewerApps {
            ids: reviewer_ids.clone().unwrap_or_default(),
            logins: reviewer_logins.clone().unwrap_or_default(),
        },
        _ => ReviewerApps::default(),
    }
}

/// The warning `load` prints once when `[github] reviewer_logins` names any login.
fn reviewer_logins_warning(path: &Path) -> String {
    format!(
        "warning: {}: `github.reviewer_logins` matches reviews by login, which can be spoofed \
         after a GitHub App is renamed: GitHub frees the old slug for anyone to register. \
         Prefer `github.reviewer_ids`, the App bot's numeric user id (the default is {:?})",
        path.display(),
        DEFAULT_REVIEWER_APP_IDS
    )
}

/// Whether `login` is a GitHub login, or an App's `<slug>[bot]`: letters, digits and `-`, then an
/// optional trailing `[bot]`, at most 64 bytes in all.
fn is_github_login(login: &str) -> bool {
    let name = match login.len().checked_sub(5) {
        Some(at) if login.is_char_boundary(at) && login[at..].eq_ignore_ascii_case("[bot]") => {
            &login[..at]
        }
        _ => login,
    };
    login.len() <= 64
        && !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `[[mcp]]`: this machine's switches for one project's MCP catalog (MCP
/// spec §2.2), found by the same longest-ancestor rule as `[[project]]` and
/// independent of it, so a project needs no store binding to use the
/// catalog. `disable` turns off a server the team default turns on, and
/// `enable` the reverse. Whether each name is in the catalog is checked
/// against the catalog, not here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEntry {
    pub root: PathBuf,
    #[serde(default)]
    pub disable: Vec<String>,
    #[serde(default)]
    pub enable: Vec<String>,
}

/// Workspace-level `[wire]` configuration (FerroWire spec §5.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireConfig {
    /// Optional Unix domain socket path for local daemon IPC.
    #[serde(default)]
    pub socket: Option<PathBuf>,
    /// Optional TCP address for accepting incoming WebSocket peer connections.
    #[serde(default)]
    pub listen: Option<String>,
    /// Optional outbound WebSocket peer/hub URLs (`ws://` or `wss://`).
    #[serde(default)]
    pub peers: Vec<String>,
    /// Optional path to file containing the pre-shared bearer token (falls back to `$FL_WIRE_TOKEN`).
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// Retention window in days for acknowledged transient messages (default: 30).
    #[serde(default)]
    pub retention_days: Option<u64>,
    /// Whether `fl state sync` exports unacknowledged and durable wire messages (default: true).
    #[serde(default)]
    pub sync_wire: Option<bool>,
}

/// Workspace-level `[hub]` configuration (Hub spec §2.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    /// Listen address for `fl hub serve` (defaults to `"127.0.0.1:8420"`).
    #[serde(default)]
    pub listen: Option<String>,
    /// Static `FerroDeck` UI directory (defaults to `<repo_root>/ui/dist` when present).
    #[serde(default)]
    pub ui_dir: Option<PathBuf>,
    /// The repository the hub's repository-bound views read (worktrees, diffs, architecture,
    /// commands and skills, code search) and where an Owner terminal session starts. An absolute
    /// path to an existing directory; `fl hub serve --repo-root` wins over it. Defaults to the
    /// working directory of `fl hub serve`, which is the home directory for a user service.
    #[serde(default)]
    pub repo_root: Option<PathBuf>,
    /// Optional path to file containing the Hub owner token (falls back to `$FL_HUB_OWNER_TOKEN`).
    #[serde(default)]
    pub owner_token_file: Option<PathBuf>,
    /// Whether to trust `Tailscale-User-Login` headers from `tailscale serve`.
    #[serde(default)]
    pub trust_tailscale_headers: bool,
    /// Whether interactive Owner terminal streams on `/ws/v1` are enabled (defaults to `true`).
    #[serde(default)]
    pub enable_terminal: Option<bool>,
    /// Browser session TTL in hours (defaults to `24`).
    #[serde(default)]
    pub session_ttl_hours: Option<u64>,
}

/// Why `p` cannot be a hub `repo_root`, judged by its text alone (the same rule for
/// `[hub] repo_root` and `fl hub serve --repo-root`): it must be absolute, like `hub.ui_dir`, and
/// no path holds a NUL byte. Whether it is an existing directory is checked at startup.
#[must_use]
pub fn repo_root_shape_error(p: &Path) -> Option<&'static str> {
    if p.to_string_lossy().contains('\0') {
        Some("must not contain a NUL byte")
    } else if !p.is_absolute() {
        Some("must be an absolute path")
    } else {
        None
    }
}

/// Per-adapter `[adapters.<name>]` configuration (Adapters & Telemetry spec §1.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterConfig {
    #[serde(default)]
    pub binary: Option<String>,
    #[serde(default)]
    pub input_usd_micros_per_mtok: Option<u64>,
    #[serde(default)]
    pub output_usd_micros_per_mtok: Option<u64>,
    /// Names of variables copied from fl's own environment to this CLI's child, besides the
    /// allow-list (for example `HTTPS_PROXY` or `AWS_PROFILE`). Names only: the values are read
    /// when the CLI starts. `FL_*` and `FERROGATE_*` are refused. See `docs/vendor-child-env.md`.
    #[serde(default)]
    pub env_pass: Vec<String>,
}

impl AdapterConfig {
    pub fn rate_card(&self) -> Option<RateCard> {
        if self.input_usd_micros_per_mtok.is_none() && self.output_usd_micros_per_mtok.is_none() {
            None
        } else {
            Some(RateCard {
                input_usd_micros_per_mtok: self.input_usd_micros_per_mtok.unwrap_or(0),
                output_usd_micros_per_mtok: self.output_usd_micros_per_mtok.unwrap_or(0),
            })
        }
    }
}

/// How a `[vendors.<id>]` entry says fl reaches its vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VendorTransportKind {
    /// An HTTP API (hosted with a key, or a local engine without one).
    HttpApi,
    /// A subscription CLI. Configuration cannot add one: it needs a code adapter.
    SubscriptionCli,
}

/// Per-vendor `[vendors.<id>]` configuration.
///
/// For one of the 13 built-in vendors it is a set of OVERRIDES (`base_url`, `default_model`,
/// the rate card) that fl applies when it builds that vendor's adapter; the registry's built-in
/// is never changed. For any other id it DEFINES a new HTTP API vendor, so a vLLM, a proxy or
/// a new hosted API needs no recompile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorConfig {
    /// Required (`http_api`) for a new vendor; for a built-in it may only repeat its own.
    #[serde(default)]
    pub transport: Option<VendorTransportKind>,
    /// The wire protocol of a new vendor: `openai_chat_completions`, `anthropic_messages` or
    /// `gemini_generate_content`.
    #[serde(default)]
    pub dialect: Option<ApiDialect>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    /// The environment variable holding a new vendor's API key.
    #[serde(default)]
    pub env_key: Option<String>,
    /// The FerroGate Vault secret holding a new vendor's API key (default: `env_key`).
    #[serde(default)]
    pub vault_secret: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default, alias = "input_usd_micros_per_m")]
    pub input_usd_micros_per_mtok: Option<u64>,
    #[serde(default, alias = "output_usd_micros_per_m")]
    pub output_usd_micros_per_mtok: Option<u64>,
    #[serde(default)]
    pub mcp_config_rel_path: Option<String>,
}

impl VendorConfig {
    pub fn rate_card(&self) -> Option<RateCard> {
        if self.input_usd_micros_per_mtok.is_none() && self.output_usd_micros_per_mtok.is_none() {
            None
        } else {
            Some(RateCard {
                input_usd_micros_per_mtok: self.input_usd_micros_per_mtok.unwrap_or(0),
                output_usd_micros_per_mtok: self.output_usd_micros_per_mtok.unwrap_or(0),
            })
        }
    }

    /// The keys this entry sets, for the message that refuses them.
    fn set_keys(&self, only: &[&str]) -> Vec<&'static str> {
        let all = [
            ("transport", self.transport.is_some()),
            ("dialect", self.dialect.is_some()),
            ("base_url", self.base_url.is_some()),
            ("default_model", self.default_model.is_some()),
            ("display_name", self.display_name.is_some()),
            ("env_key", self.env_key.is_some()),
            ("vault_secret", self.vault_secret.is_some()),
            ("context_window", self.context_window.is_some()),
            (
                "input_usd_micros_per_mtok",
                self.input_usd_micros_per_mtok.is_some(),
            ),
            (
                "output_usd_micros_per_mtok",
                self.output_usd_micros_per_mtok.is_some(),
            ),
            ("mcp_config_rel_path", self.mcp_config_rel_path.is_some()),
        ];
        all.into_iter()
            .filter(|(k, set)| *set && only.contains(k))
            .map(|(k, _)| k)
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct Config {
    pub state: Option<StateConfig>,
    pub wire: Option<WireConfig>,
    pub hub: Option<HubConfig>,
    pub projects: Vec<Entry>,
    pub github: Option<GithubApp>,
    pub mcp: Vec<McpEntry>,
    pub adapters: BTreeMap<AdapterKind, AdapterConfig>,
    /// `[vendors.<id>]`, by lowercase id. A new vendor in it is already registered.
    pub vendors: BTreeMap<String, VendorConfig>,
}

impl Config {
    /// Workspace state store path (`[state].store` or `default_store`).
    pub fn workspace_state_store(&self, default_store: &Path) -> PathBuf {
        self.state
            .as_ref()
            .and_then(|s| s.store.clone())
            .unwrap_or_else(|| default_store.to_path_buf())
    }

    /// Find a `[[project]]` entry matching `slug` by `name` or lowercase basename of `root`.
    pub fn find_project_entry(&self, slug: &str) -> Option<&Entry> {
        self.projects.iter().find(|e| e.matches_slug(slug))
    }

    /// Resolve which `.redb` store holds state for `scope` (spec §4.3):
    /// - `project:<slug>` -> matching `[[project]].store` if that entry exists,
    ///   otherwise the workspace state store.
    /// - All other scopes (`workspace`, `group:*`, `agent:*`, `workflow:*`) -> workspace state store.
    pub fn state_store_for_scope(&self, scope: &StateScope, default_store: &Path) -> PathBuf {
        if let StateScope::Project(slug) = scope
            && let Some(entry) = self.find_project_entry(slug)
        {
            return entry.store.clone();
        }
        self.workspace_state_store(default_store)
    }

    /// Resolve the `StateSyncTarget` for `fl state sync` / `fl state verify` (spec §4.3, FerroWire §5.1).
    pub fn sync_target_for_scope(
        &self,
        scope: Option<&StateScope>,
        cwd: &Path,
        default_store: &Path,
        by: &str,
    ) -> StateSyncTarget {
        let ws = self.state.as_ref();
        let ws_store = self.workspace_state_store(default_store);
        let cfg_sync_wire = self.wire.as_ref().and_then(|w| w.sync_wire).unwrap_or(true);
        if let Some(StateScope::Project(slug)) = scope
            && let Some(entry) = self.find_project_entry(slug)
        {
            let proj_state = entry.state.as_ref();
            let repo_dir = proj_state
                .and_then(|s| s.repo.clone())
                .or_else(|| ws.and_then(|s| s.repo.clone()))
                .unwrap_or_else(|| entry.root.clone());
            let remote = proj_state
                .and_then(|s| s.remote.clone())
                .or_else(|| ws.and_then(|s| s.remote.clone()))
                .unwrap_or_else(|| "origin".to_string());
            let branch = proj_state
                .and_then(|s| s.branch.clone())
                .or_else(|| ws.and_then(|s| s.branch.clone()))
                .unwrap_or_else(|| DEFAULT_STATE_BRANCH.to_string());
            let private = proj_state
                .and_then(|s| s.private)
                .or_else(|| ws.map(|s| s.private))
                .unwrap_or(false);
            let shares_ws_store = entry.store == ws_store;
            return StateSyncTarget {
                repo_dir,
                remote,
                branch,
                private,
                sync_registry: shares_ws_store,
                sync_wire: shares_ws_store && cfg_sync_wire,
                scope_filter: Some(StateScope::Project(slug.clone())),
                by: by.to_string(),
            };
        }

        let repo_dir = ws
            .and_then(|s| s.repo.clone())
            .unwrap_or_else(|| cwd.to_path_buf());
        let remote = ws
            .and_then(|s| s.remote.clone())
            .unwrap_or_else(|| "origin".to_string());
        let branch = ws
            .and_then(|s| s.branch.clone())
            .unwrap_or_else(|| DEFAULT_STATE_BRANCH.to_string());
        let private = ws.map(|s| s.private).unwrap_or(false);
        let scope_filter = match scope {
            Some(StateScope::Project(slug)) => Some(StateScope::Project(slug.clone())),
            _ => None,
        };
        StateSyncTarget {
            repo_dir,
            remote,
            branch,
            private,
            sync_registry: true,
            sync_wire: cfg_sync_wire,
            scope_filter,
            by: by.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    state: Option<StateConfig>,
    #[serde(default)]
    wire: Option<WireConfig>,
    #[serde(default)]
    hub: Option<HubConfig>,
    #[serde(default)]
    project: Vec<Entry>,
    #[serde(default)]
    github: Option<GithubApp>,
    /// ⚠ The top level refuses a key it does not know, so an fl older than
    /// this table refuses a config that has one — for every command, since
    /// every command reads the config first. Upgrading fl on that machine is
    /// the remedy (MCP spec §2.2, release scope).
    #[serde(default)]
    mcp: Vec<McpEntry>,
    #[serde(default)]
    adapters: BTreeMap<String, AdapterConfig>,
    #[serde(default)]
    vendors: BTreeMap<String, VendorConfig>,
}

/// The XDG config base directory: `xdg_config_home` if it is a non-empty,
/// ABSOLUTE path, else `home/.config`. Per the XDG base directory spec, a
/// relative `$XDG_CONFIG_HOME` (including empty, which is not absolute)
/// must be treated as unset rather than used as-is.
/// Pure and dependency-free so it can be unit-tested without touching this
/// process's own environment.
fn base_dir(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_base(xdg_config_home, home, ".config")
}

/// The XDG data base directory, by the same rule as [`base_dir`]:
/// `xdg_data_home` if it is an ABSOLUTE path, else `home/.local/share`. An
/// empty or relative `$XDG_DATA_HOME` is treated as unset — used as-is it
/// would put the store under the current directory.
pub fn data_dir(xdg_data_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_base(xdg_data_home, home, ".local/share")
}

/// The XDG state base directory, by the same rule as [`data_dir`]:
/// `xdg_state_home` if it is an ABSOLUTE path, else `home/.local/state`. An
/// empty or relative `$XDG_STATE_HOME` is treated as unset.
pub fn state_dir(xdg_state_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_base(xdg_state_home, home, ".local/state")
}

/// fl's own state directory, `<state base>/fl`, from this process's
/// `$XDG_STATE_HOME` and `$HOME` by [`state_dir`]'s rule. `None` when
/// neither gives an absolute base.
pub fn fl_state_dir() -> Option<PathBuf> {
    let base = state_dir(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )?;
    Some(base.join("fl"))
}

/// The one rule every XDG base follows: the variable if it is absolute, else
/// `home` joined with the spec's default for that base.
fn xdg_base(var: Option<PathBuf>, home: Option<PathBuf>, default: &str) -> Option<PathBuf> {
    var.filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(default)))
}

pub fn path() -> Option<PathBuf> {
    let base = base_dir(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )?;
    Some(base.join("fl").join("config.toml"))
}

pub fn load(path: Option<&Path>) -> Result<Config> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    let mut file: File = toml::from_str(&text)
        .with_context(|| format!("{} is not a valid fl config", path.display()))?;
    if let Some(st) = &file.state {
        if let Some(s) = &st.store
            && !s.is_absolute()
        {
            bail!(
                "{}: `state.store` must be an absolute path (got `{}`)",
                path.display(),
                s.display()
            );
        }
        if let Some(r) = &st.repo
            && !r.is_absolute()
        {
            bail!(
                "{}: `state.repo` must be an absolute path (got `{}`)",
                path.display(),
                r.display()
            );
        }
    }
    let mut seen_names = std::collections::BTreeSet::new();
    for e in &mut file.project {
        if let Some(raw_name) = &e.name {
            let norm = validate_slug(raw_name).map_err(|err| {
                anyhow::anyhow!("{}: invalid project `name`: {err}", path.display())
            })?;
            if !seen_names.insert(norm.clone()) {
                bail!("{}: duplicate project name `{norm}`", path.display());
            }
            e.name = Some(norm);
        }
        if !e.root.is_absolute() || !e.store.is_absolute() {
            bail!(
                "{}: `root` and `store` must be absolute paths (got root `{}`, store `{}`)",
                path.display(),
                e.root.display(),
                e.store.display()
            );
        }
        if let Some(t) = &e.tracker
            && !is_owner_repo(&t.github)
        {
            bail!(
                "{}: `github = \"{}\"` must name a repository as `owner/repo`",
                path.display(),
                t.github
            );
        }
        if let Some(pst) = &e.state
            && let Some(r) = &pst.repo
            && !r.is_absolute()
        {
            bail!(
                "{}: `state.repo` in `[[project]]` must be an absolute path (got `{}`)",
                path.display(),
                r.display()
            );
        }
    }
    if let Some(app) = &mut file.github {
        if !app.private_key.is_absolute() {
            bail!(
                "{}: `private_key` must be an absolute path (got `{}`)",
                path.display(),
                app.private_key.display()
            );
        }
        for login in app.reviewer_logins.iter_mut().flatten() {
            *login = login.trim().to_string();
            if !is_github_login(login) {
                bail!(
                    "{}: `github.reviewer_logins` entry `{login}` is not a GitHub login; use \
                     letters, digits and `-` (at most 64 characters), with an optional trailing \
                     `[bot]`, for example `ferroloop[bot]`",
                    path.display()
                );
            }
        }
        if app.reviewer_ids.iter().flatten().any(|&id| id == 0) {
            bail!(
                "{}: `github.reviewer_ids` entry `0` is not a GitHub user id; use the App \
                 bot's id from `gh api 'users/<slug>[bot]' --jq .id`",
                path.display()
            );
        }
        if app.reviewer_logins.as_ref().is_some_and(|l| !l.is_empty()) {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| eprintln!("{}", reviewer_logins_warning(path)));
        }
    }
    for m in &file.mcp {
        if !m.root.is_absolute() {
            bail!(
                "{}: `root` in `[[mcp]]` must be an absolute path (got `{}`)",
                path.display(),
                m.root.display()
            );
        }
        // MCP spec §2.2: one name switched both ways says nothing.
        if let Some(name) = m.enable.iter().find(|n| m.disable.contains(n)) {
            bail!(
                "{}: in the `[[mcp]]` entry for {}, `{name}` is in both `enable` and \
                 `disable`; keep it in one",
                path.display(),
                m.root.display()
            );
        }
    }
    if let Some(w) = &file.wire {
        if let Some(s) = &w.socket
            && !s.is_absolute()
        {
            bail!(
                "{}: `wire.socket` must be an absolute path (got `{}`)",
                path.display(),
                s.display()
            );
        }
        if let Some(tf) = &w.token_file
            && !tf.is_absolute()
        {
            bail!(
                "{}: `wire.token_file` must be an absolute path (got `{}`)",
                path.display(),
                tf.display()
            );
        }
    }
    if let Some(h) = &file.hub {
        if let Some(u) = &h.ui_dir
            && !u.is_absolute()
        {
            bail!(
                "{}: `hub.ui_dir` must be an absolute path (got `{}`)",
                path.display(),
                u.display()
            );
        }
        if let Some(r) = &h.repo_root
            && let Some(why) = repo_root_shape_error(r)
        {
            bail!(
                "{}: `hub.repo_root` {why} (got `{}`)",
                path.display(),
                r.display()
            );
        }
        if let Some(otf) = &h.owner_token_file
            && !otf.is_absolute()
        {
            bail!(
                "{}: `hub.owner_token_file` must be an absolute path (got `{}`)",
                path.display(),
                otf.display()
            );
        }
    }
    let mut adapters = BTreeMap::new();
    for (raw_key, adapter_cfg) in file.adapters {
        let kind = AdapterKind::from_wire(&raw_key).ok_or_else(|| {
            anyhow::anyhow!(
                "{}: unknown adapter `{raw_key}` in `[adapters]`; known adapters are: {}",
                path.display(),
                AdapterKind::ALL
                    .iter()
                    .map(|k| k.as_wire())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        if let Some(bin) = &adapter_cfg.binary
            && bin.trim().is_empty()
        {
            bail!(
                "{}: `adapters.{raw_key}.binary` must not be empty",
                path.display()
            );
        }
        fl_exec::child_env::check_pass_list(&adapter_cfg.env_pass).map_err(|why| {
            anyhow::anyhow!("{}: `adapters.{raw_key}.env_pass` {why}", path.display())
        })?;
        adapters.insert(kind, adapter_cfg);
    }
    let vendors = load_vendors(path, file.vendors)?;
    Ok(Config {
        state: file.state,
        wire: file.wire,
        hub: file.hub,
        projects: file.project,
        github: file.github,
        mcp: file.mcp,
        adapters,
        vendors,
    })
}

/// Checks every `[vendors.<id>]` entry, then registers each new vendor in the process-wide
/// registry. Returns the entries by lowercase id.
///
/// ⚠ Nothing is registered unless every entry is valid, and registering is idempotent
/// ([`fl_core::ensure_vendor_registered`]): a process may load the config more than once. A
/// different definition of an id already registered is an error naming the id, never a panic;
/// it is found while the entries are checked, so no vendor of the file is registered before it.
/// (Only a vendor another thread registers between that check and the registration can still
/// fail after earlier vendors of the file are in.)
fn load_vendors(
    path: &Path,
    raw: BTreeMap<String, VendorConfig>,
) -> Result<BTreeMap<String, VendorConfig>> {
    let mut vendors = BTreeMap::new();
    let mut new_vendors = Vec::new();
    for (raw_id, cfg) in raw {
        let id = raw_id.trim().to_ascii_lowercase();
        let at = |msg: String| anyhow::anyhow!("{}: `[vendors.{raw_id}]` {msg}", path.display());
        if vendors.contains_key(&id) {
            return Err(at(format!(
                "repeats the vendor `{id}`: ids are compared without case"
            )));
        }
        if let Some(builtin) = VendorRegistry::global()
            .get(&id)
            .filter(|_| VendorRegistry::is_builtin(&id))
        {
            check_builtin_override(&cfg, &builtin).map_err(at)?;
        } else {
            let descriptor = new_vendor_descriptor(&id, &cfg).map_err(at)?;
            // A conflict with a vendor registered earlier in this process is found here, before
            // the first vendor of this file is registered.
            if VendorRegistry::global()
                .get(&id)
                .is_some_and(|existing| existing != descriptor)
            {
                return Err(at(
                    "is already registered in this process with a different definition; an id \
                     cannot be redefined"
                        .into(),
                ));
            }
            new_vendors.push(descriptor);
        }
        vendors.insert(id, cfg);
    }
    for descriptor in new_vendors {
        let id = descriptor.id.clone();
        fl_core::ensure_vendor_registered(descriptor).map_err(|e| {
            anyhow::anyhow!(
                "{}: could not register the vendor `{id}` from `[vendors.{id}]`: {e}",
                path.display()
            )
        })?;
    }
    Ok(vendors)
}

/// What a built-in vendor's entry may hold: overrides only.
fn check_builtin_override(cfg: &VendorConfig, builtin: &VendorDescriptor) -> Result<(), String> {
    if cfg.mcp_config_rel_path.is_some() {
        return Err(
            "sets `mcp_config_rel_path`, but a built-in vendor's MCP file is fixed in \
             fl-mcp (the five CLI files) or absent (API vendors); only a new vendor can set it"
                .into(),
        );
    }
    match &builtin.transport {
        VendorTransport::SubscriptionCli { .. } => {
            let set = cfg.set_keys(&[
                "dialect",
                "base_url",
                "default_model",
                "display_name",
                "env_key",
                "vault_secret",
                "context_window",
                "input_usd_micros_per_mtok",
                "output_usd_micros_per_mtok",
            ]);
            if !set.is_empty() {
                return Err(format!(
                    "is a subscription CLI, which takes no API settings ({}); its binary and rate \
                     card go in `[adapters.{}]`",
                    set.join(", "),
                    builtin.id
                ));
            }
            if cfg.transport == Some(VendorTransportKind::HttpApi) {
                return Err(
                    "says `transport = \"http_api\"`, but this built-in vendor is a \
                     subscription CLI"
                        .into(),
                );
            }
        }
        VendorTransport::HttpApi { .. } => {
            let set = cfg.set_keys(&[
                "dialect",
                "display_name",
                "env_key",
                "vault_secret",
                "context_window",
            ]);
            if !set.is_empty() {
                return Err(format!(
                    "is a built-in vendor; only `base_url`, `default_model` and the rate card can \
                     be overridden, not {}",
                    set.join(", ")
                ));
            }
            if cfg.transport == Some(VendorTransportKind::SubscriptionCli) {
                return Err(
                    "says `transport = \"subscription_cli\"`, but this built-in vendor \
                     is an HTTP API"
                        .into(),
                );
            }
            if let Some(url) = &cfg.base_url {
                validate_api_base_url(url).map_err(|e| format!("`base_url`: {e}"))?;
            }
            if cfg
                .default_model
                .as_deref()
                .is_some_and(|m| m.trim().is_empty())
            {
                return Err("`default_model` must not be empty".into());
            }
        }
    }
    Ok(())
}

/// The descriptor of a new, config-defined HTTP API vendor.
fn new_vendor_descriptor(id: &str, cfg: &VendorConfig) -> Result<VendorDescriptor, String> {
    match cfg.transport {
        Some(VendorTransportKind::HttpApi) => {}
        Some(VendorTransportKind::SubscriptionCli) => {
            return Err(
                "sets `transport = \"subscription_cli\"`, which configuration cannot \
                 add: a subscription CLI needs a code adapter (an `ExecVendorAdapter`). Only \
                 `transport = \"http_api\"` can define a new vendor"
                    .into(),
            );
        }
        None => {
            return Err(format!(
                "is not a built-in vendor, so it defines a new one and needs \
                 `transport = \"http_api\"`. Built-in vendors are: {}",
                VendorRegistry::global()
                    .list()
                    .iter()
                    .filter(|v| VendorRegistry::is_builtin(&v.id))
                    .map(|v| v.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let need = |key: &str, v: &Option<String>| -> Result<String, String> {
        match v.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => Ok(s.to_string()),
            Some(_) => Err(format!("`{key}` must not be empty")),
            None => Err(format!("is a new vendor and needs `{key}`")),
        }
    };
    let Some(api_dialect) = cfg.dialect else {
        return Err(
            "is a new vendor and needs `dialect` (`openai_chat_completions`, \
             `anthropic_messages` or `gemini_generate_content`)"
                .into(),
        );
    };
    let base_url = need("base_url", &cfg.base_url)?;
    validate_api_base_url(&base_url).map_err(|e| format!("`base_url`: {e}"))?;
    let default_model = need("default_model", &cfg.default_model)?;
    let key_name = |key: &str, v: &Option<String>| -> Result<Option<String>, String> {
        match v.as_deref().map(str::trim) {
            Some("") => Err(format!(
                "`{key}` must not be empty; leave it out for no key"
            )),
            other => Ok(other.map(str::to_string)),
        }
    };
    let env_key_name = key_name("env_key", &cfg.env_key)?;
    let vault_secret_name = key_name("vault_secret", &cfg.vault_secret)?.or(env_key_name.clone());
    Ok(VendorDescriptor {
        id: id.to_string(),
        display_name: match cfg.display_name.as_deref().map(str::trim) {
            Some("") => return Err("`display_name` must not be empty".into()),
            Some(n) => n.to_string(),
            None => id.to_string(),
        },
        transport: VendorTransport::HttpApi {
            default_base_url: base_url,
            api_dialect,
            vault_secret_name,
            env_key_name,
        },
        default_model,
        default_context_window: cfg.context_window.unwrap_or(0),
        default_rate_card: cfg.rate_card().unwrap_or_default(),
        mcp_config_rel_path: cfg.mcp_config_rel_path.clone(),
    })
}

/// Whether `s` names a repository as `owner/repo`: exactly two non-empty
/// parts of `[A-Za-z0-9._-]`, neither `.` nor `..`. The config's `github = …` and a typed
/// `owner/repo#41` follow the same rule.
pub fn is_owner_repo(s: &str) -> bool {
    // Each part is spliced into API URLs and IRIs, so it is limited to the
    // characters GitHub names use; `.`/`..` would traverse and `?#%` would
    // rewrite the request.
    let part = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    s.split('/').count() == 2 && s.split('/').all(part)
}

/// Canonicalize `path`. A path that does not exist is not an error here —
/// `Ok(None)` — that is simply a project root (or a `cwd`) not yet created.
/// Any OTHER failure, such as a permission error partway down the tree, IS
/// an error naming the path: silently treating it the same as "does not
/// exist" would fall through to the default store, putting a project's
/// records in a store nobody chose.
fn canonicalize(path: &Path) -> Result<Option<PathBuf>> {
    match path.canonicalize() {
        Ok(p) => Ok(Some(p)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("could not resolve {}", path.display())),
    }
}

/// The items whose root is the longest ancestor of `cwd`, in their order in
/// `items`, with `cwd` canonicalized; `None` when `cwd` does not exist or no
/// item's root is an ancestor of it. Both sides are canonicalized, so a
/// symlinked path binds like the real one, and a root that does not exist
/// is skipped. More than one item comes back only when their roots are the
/// same directory: the caller decides whether that tie is a conflict.
fn longest_ancestor<'a, T>(
    items: &'a [T],
    root_of: impl Fn(&T) -> &Path,
    cwd: &Path,
) -> Result<Option<(PathBuf, Vec<&'a T>)>> {
    let Some(cwd) = canonicalize(cwd)? else {
        return Ok(None);
    };
    let mut matches: Vec<(PathBuf, &T)> = Vec::new();
    for item in items {
        let Some(root) = canonicalize(root_of(item))? else {
            continue;
        };
        if cwd.starts_with(&root) {
            matches.push((root, item));
        }
    }
    let Some(longest) = matches.iter().map(|(r, _)| r.components().count()).max() else {
        return Ok(None);
    };
    let winners = matches
        .into_iter()
        .filter(|(r, _)| r.components().count() == longest)
        .map(|(_, item)| item)
        .collect();
    Ok(Some((cwd, winners)))
}

/// The config entry for the project containing `cwd`: the entry whose root
/// is the longest ancestor of `cwd`. Both sides are canonicalized, so a
/// symlinked path binds like the real one.
///
/// §2.6 binds a project to exactly one store, and a project has one
/// tracker. Two (or more) entries whose canonical `root` is identical — the
/// longest match is therefore tied — but whose `(store, tracker)` differs
/// are refused rather than silently picking one: nothing chose between them.
pub fn bound_entry(entries: &[Entry], cwd: &Path) -> Result<Option<Entry>> {
    let Some((cwd, winners)) = longest_ancestor(entries, |e| &e.root, cwd)? else {
        return Ok(None);
    };
    let mut distinct: Vec<(&PathBuf, &Option<TrackerBinding>)> = Vec::new();
    for e in &winners {
        if !distinct.contains(&(&e.store, &e.tracker)) {
            distinct.push((&e.store, &e.tracker));
        }
    }
    if distinct.len() > 1 {
        let names = winners
            .iter()
            .map(|e| {
                let tracker = match &e.tracker {
                    Some(t) if t.github_ledger() => format!("github:{} (ledger github)", t.github),
                    Some(t) => format!("github:{}", t.github),
                    None => "the store's own tracker".to_string(),
                };
                format!("{} -> {} -> {tracker}", e.root.display(), e.store.display())
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "the project at {} is bound to more than one store or tracker in the config: \
             {names}. Remove all but one of these entries",
            cwd.display()
        );
    }
    Ok(Some(winners[0].clone()))
}

/// The `[[mcp]]` entry for the project containing `cwd`, by
/// [`bound_entry`]'s rule: the entry whose root is the longest ancestor of
/// `cwd` (MCP spec §2.2). Two entries on one root that switch differently
/// are refused, naming both: nothing chose between them. Two that switch
/// alike are one choice, however their roots are spelled.
pub fn mcp_entry(entries: &[McpEntry], cwd: &Path) -> Result<Option<McpEntry>> {
    let Some((cwd, winners)) = longest_ancestor(entries, |e| &e.root, cwd)? else {
        return Ok(None);
    };
    let first = winners[0];
    if winners
        .iter()
        .any(|e| (&e.disable, &e.enable) != (&first.disable, &first.enable))
    {
        let names = winners
            .iter()
            .map(|e| {
                format!(
                    "{} (disable {:?}, enable {:?})",
                    e.root.display(),
                    e.disable,
                    e.enable
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "the project at {} has more than one `[[mcp]]` entry in the config: {names}. \
             Keep one of these entries",
            cwd.display()
        );
    }
    Ok(Some(first.clone()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn owner_repo_rejects_traversal_and_url_metacharacters() {
        for bad in [
            "../..", "a/..", "./b", "a/b?x=1", "a/b#c", "a/b%2f", "a\u{0}/b", "own er/b", "a/b/c",
            "/b", "a/",
        ] {
            assert!(!is_owner_repo(bad), "{bad:?} must be rejected");
        }
        for ok in ["acme/widgets.rs", "a-b/c_d", "Org1/repo.name"] {
            assert!(is_owner_repo(ok), "{ok:?} must be accepted");
        }
    }

    use super::*;

    /// `load` over a config file holding `text`.
    fn load_text(text: &str) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        load(Some(&path))
    }

    fn entry_with(github: &str) -> String {
        format!(
            "[[project]]\nroot = \"/r\"\nstore = \"/s.redb\"\n\
             tracker = {{ github = \"{github}\", credential = \"env\" }}\n"
        )
    }

    #[test]
    fn a_tracker_must_name_a_repository_as_owner_slash_repo() {
        for bad in [
            "acme",
            "acme/",
            "/widgets",
            "acme/widgets/x",
            "ac me/widgets",
            "",
        ] {
            let err = load_text(&entry_with(bad)).expect_err(bad);
            assert!(
                format!("{err:#}").contains("must name a repository as `owner/repo`"),
                "{bad}: {err:#}"
            );
        }
        let cfg = load_text(&entry_with("acme/widgets")).unwrap();
        assert_eq!(
            cfg.projects[0].tracker,
            Some(TrackerBinding {
                github: "acme/widgets".into(),
                credential: Credential::Env,
                ledger: None,
            })
        );
    }

    #[test]
    fn a_relative_private_key_is_refused() {
        let err = load_text("[github]\napp_id = 1\nprivate_key = \"key.pem\"\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("`private_key` must be an absolute path"),
            "{err:#}"
        );
        let cfg = load_text("[github]\napp_id = 1\nprivate_key = \"/k/key.pem\"\n").unwrap();
        assert_eq!(cfg.github.unwrap().app_id, 1);
    }

    const GITHUB_TABLE: &str = "[github]\napp_id = 1\nprivate_key = \"/k/key.pem\"\n";

    fn apps_of(text: &str) -> ReviewerApps {
        reviewer_apps(load_text(text).unwrap().github.as_ref())
    }

    fn logins_of(text: &str) -> Vec<String> {
        apps_of(text).logins
    }

    #[test]
    fn reviewer_apps_default_to_the_reviewer_app_id_and_no_logins() {
        let default = ReviewerApps {
            ids: DEFAULT_REVIEWER_APP_IDS.to_vec(),
            logins: Vec::new(),
        };
        assert_eq!(default.ids, [341_521_384]);
        // No `[github]` table at all (verify-ci on a machine with no App credentials).
        assert_eq!(apps_of(""), default);
        // A `[github]` table with neither key keeps working and takes the default.
        assert_eq!(apps_of(GITHUB_TABLE), default);
        assert_eq!(reviewer_apps(None), default);
    }

    #[test]
    fn reviewer_ids_alone_replace_the_default_with_no_logins() {
        let with = |list: &str| format!("{GITHUB_TABLE}reviewer_ids = {list}\n");
        assert_eq!(
            apps_of(&with("[7, 341521384]")),
            ReviewerApps {
                ids: vec![7, 341_521_384],
                logins: Vec::new(),
            }
        );
        // An empty list is deliberate: no agent review counts.
        assert_eq!(apps_of(&with("[]")), ReviewerApps::none());
    }

    #[test]
    fn reviewer_logins_alone_replace_the_default_with_no_ids() {
        let with = |list: &str| format!("{GITHUB_TABLE}reviewer_logins = {list}\n");
        assert_eq!(
            apps_of(&with(
                "[\"other-app[bot]\", \" Second[BOT] \", \"human-1\"]"
            )),
            ReviewerApps {
                ids: Vec::new(),
                logins: vec![
                    "other-app[bot]".into(),
                    "Second[BOT]".into(),
                    "human-1".into()
                ],
            }
        );
        // An empty list is deliberate: no agent review counts, not even the default id.
        assert_eq!(apps_of(&with("[]")), ReviewerApps::none());
    }

    #[test]
    fn reviewer_ids_and_logins_together_are_the_whole_set() {
        let text =
            format!("{GITHUB_TABLE}reviewer_ids = [5]\nreviewer_logins = [\"other-app[bot]\"]\n");
        assert_eq!(
            apps_of(&text),
            ReviewerApps {
                ids: vec![5],
                logins: vec!["other-app[bot]".into()],
            }
        );
    }

    #[test]
    fn a_bad_reviewer_id_is_refused() {
        let err = load_text(&format!("{GITHUB_TABLE}reviewer_ids = [5, 0]\n")).unwrap_err();
        assert!(
            format!("{err:#}").contains("`github.reviewer_ids` entry `0`"),
            "{err:#}"
        );
        for bad in ["5", "[\"5\"]", "[-1]", "[1.5]", "[\"ferroloop[bot]\"]"] {
            assert!(
                load_text(&format!("{GITHUB_TABLE}reviewer_ids = {bad}\n")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_reviewer_logins_warning_names_the_spoofing_risk_and_reviewer_ids() {
        let msg = reviewer_logins_warning(Path::new("/c/config.toml"));
        for part in [
            "/c/config.toml",
            "spoofed",
            "renamed",
            "`github.reviewer_ids`",
        ] {
            assert!(msg.contains(part), "{part}: {msg}");
        }
    }

    #[test]
    fn an_invalid_reviewer_login_is_refused_naming_the_field() {
        let long = format!("{}[bot]", "a".repeat(60));
        for bad in [
            "",
            "   ",
            "[bot]",
            "has space",
            "under_score",
            "dot.ted",
            "slash/y",
            "name[bot",
            "name[bot]x",
            "name[bot][bot]",
            "na[me]",
            "caf\u{e9}",
            long.as_str(),
        ] {
            let text = format!("{GITHUB_TABLE}reviewer_logins = [\"ok[bot]\", {bad:?}]\n");
            let err = load_text(&text).expect_err(bad);
            let msg = format!("{err:#}");
            assert!(
                msg.contains("`github.reviewer_logins` entry"),
                "{bad}: {msg}"
            );
        }
        // A 64-byte login is the longest allowed.
        let max = format!("{}[bot]", "a".repeat(59));
        assert_eq!(max.len(), 64);
        let text = format!("{GITHUB_TABLE}reviewer_logins = [{max:?}]\n");
        assert_eq!(logins_of(&text), [max]);
        // Not a list of strings.
        assert!(load_text(&format!("{GITHUB_TABLE}reviewer_logins = \"x[bot]\"\n")).is_err());
    }

    #[test]
    fn the_github_table_still_refuses_an_unknown_key() {
        let err = load_text(&format!("{GITHUB_TABLE}reviewer_login = [\"x\"]\n")).unwrap_err();
        assert!(format!("{err:#}").contains("reviewer_login"), "{err:#}");
        // `app_id` and `private_key` stay required.
        assert!(load_text("[github]\nreviewer_logins = [\"x[bot]\"]\n").is_err());
    }

    #[test]
    fn two_entries_differing_only_by_tracker_are_refused_naming_each_tracker() {
        let root = tempfile::tempdir().unwrap();
        let store = PathBuf::from("/tmp/fl-config-test-same.redb");
        let entries = vec![
            Entry {
                name: None,
                root: root.path().to_path_buf(),
                store: store.clone(),
                tracker: None,
                state: None,
            },
            Entry {
                name: None,
                root: root.path().to_path_buf(),
                store,
                tracker: Some(TrackerBinding {
                    github: "acme/widgets".into(),
                    credential: Credential::Env,
                    ledger: None,
                }),
                state: None,
            },
        ];
        let msg = format!("{:#}", bound_entry(&entries, root.path()).unwrap_err());
        assert!(msg.contains("-> github:acme/widgets"), "{msg}");
        assert!(msg.contains("-> the store's own tracker"), "{msg}");
        assert!(msg.contains("Remove all but one of these entries"), "{msg}");
    }

    #[test]
    fn identical_entries_are_not_a_conflict() {
        let root = tempfile::tempdir().unwrap();
        let e = Entry {
            name: None,
            root: root.path().to_path_buf(),
            store: PathBuf::from("/tmp/fl-config-test-same.redb"),
            tracker: Some(TrackerBinding {
                github: "acme/widgets".into(),
                credential: Credential::Env,
                ledger: None,
            }),
            state: None,
        };
        let got = bound_entry(&[e.clone(), e], root.path()).unwrap().unwrap();
        assert_eq!(got.tracker.unwrap().github, "acme/widgets");
    }

    #[test]
    fn an_empty_or_relative_xdg_config_home_falls_back_to_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            base_dir(Some(PathBuf::from("")), home.clone()),
            Some(PathBuf::from("/home/u/.config")),
            "an empty XDG_CONFIG_HOME must be treated as unset"
        );
        assert_eq!(
            base_dir(Some(PathBuf::from("relative/config")), home.clone()),
            Some(PathBuf::from("/home/u/.config")),
            "a relative XDG_CONFIG_HOME must be treated as unset"
        );
        assert_eq!(
            base_dir(Some(PathBuf::from("/abs/config")), home),
            Some(PathBuf::from("/abs/config")),
            "an absolute XDG_CONFIG_HOME must still win"
        );
    }

    #[test]
    fn an_empty_or_relative_xdg_data_home_falls_back_to_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            data_dir(Some(PathBuf::from("")), home.clone()),
            Some(PathBuf::from("/home/u/.local/share")),
            "an empty XDG_DATA_HOME must be treated as unset"
        );
        assert_eq!(
            data_dir(Some(PathBuf::from("relative/data")), home.clone()),
            Some(PathBuf::from("/home/u/.local/share")),
            "a relative XDG_DATA_HOME must be treated as unset"
        );
        assert_eq!(
            data_dir(Some(PathBuf::from("/abs/data")), home.clone()),
            Some(PathBuf::from("/abs/data")),
            "an absolute XDG_DATA_HOME must still win"
        );
        assert_eq!(
            data_dir(None, home),
            Some(PathBuf::from("/home/u/.local/share"))
        );
        assert_eq!(data_dir(Some(PathBuf::from("rel")), None), None);
    }

    #[cfg(unix)]
    #[test]
    fn bound_resolves_a_symlinked_cwd_passed_directly() {
        // Task 6's own "symlinked working directory" CLI test drives `fl`
        // as a subprocess; `std::env::current_dir()` (`getcwd(3)`) already
        // resolves a symlinked process cwd before `bound` ever sees it on
        // Linux, so that test cannot tell `bound`'s own `cwd.canonicalize()`
        // apart from a naive string comparison. Calling `bound` directly
        // here, with a raw symlink path nothing else has resolved first,
        // closes that gap.
        let real = tempfile::tempdir().unwrap();
        let stores = tempfile::tempdir().unwrap();
        let store = stores.path().join("a.redb");
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("via-link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        let entries = vec![Entry {
            name: None,
            root: real.path().to_path_buf(),
            store: store.clone(),
            tracker: None,
            state: None,
        }];
        let got = bound_entry(&entries, &link).unwrap().map(|e| e.store);
        assert_eq!(got, Some(store));
    }

    #[test]
    fn two_entries_with_the_same_root_but_different_stores_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let entries = vec![
            Entry {
                name: None,
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-one.redb"),
                tracker: None,
                state: None,
            },
            Entry {
                name: None,
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-two.redb"),
                tracker: None,
                state: None,
            },
        ];
        let err = bound_entry(&entries, root.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fl-config-test-one.redb") && msg.contains("fl-config-test-two.redb"),
            "the refusal must name both entries: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_permission_error_while_canonicalizing_is_an_error_not_a_silent_skip() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let blocked = parent.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        let inner = blocked.join("root");
        std::fs::create_dir(&inner).unwrap();
        // No execute permission on `blocked`: the kernel refuses to resolve
        // anything under it — for anyone but root.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Ask the standard library directly, first, to find out whether
        // this process (root, or a filesystem that does not enforce the
        // bit) can even be made to see the failure this test exists to
        // pin — a soft `Err(_) => {}` / `Ok(_) => eprintln!(...)` on OUR
        // wrapper's result would never fail no matter what `canonicalize`
        // does with the error, so it could not catch a regression either.
        let raw = inner.canonicalize();
        let result = canonicalize(&inner);

        // Restore before the TempDir's own cleanup has to walk it again.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

        match raw {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                assert!(
                    result.is_err(),
                    "a permission failure must be reported as an error, not treated the \
                     same as \"does not exist\""
                );
            }
            _ => eprintln!(
                "skipping the assertion: this process could still resolve a 0o000 \
                 directory, so it is likely running as root"
            ),
        }
    }

    // Spec §1.5: `ledger` is optional, and its only value is "github"; any
    // other is a config error naming it.
    #[test]
    fn the_only_ledger_a_binding_names_is_github() {
        let with = |ledger: &str| {
            format!(
                "[[project]]\nroot = \"/r\"\nstore = \"/s.redb\"\n\
                 tracker = {{ github = \"acme/widgets\", credential = \"env\"{ledger} }}\n"
            )
        };
        let binding = |text: &str| {
            load_text(text).unwrap().projects[0]
                .tracker
                .clone()
                .unwrap()
        };
        assert!(!binding(&with("")).github_ledger());
        assert!(binding(&with(", ledger = \"github\"")).github_ledger());
        for bad in ["local", "GitHub", ""] {
            let err = load_text(&with(&format!(", ledger = \"{bad}\""))).expect_err(bad);
            assert!(
                format!("{err:#}")
                    .contains(&format!("`ledger = \"{bad}\"` is not a ledger fl knows")),
                "{bad}: {err:#}"
            );
        }
    }

    // `ledger = true` is not the string "github": refused when the config
    // is read, never taken as the local ledger.
    #[test]
    fn a_ledger_key_that_is_not_a_string_is_refused() {
        let text = "[[project]]\nroot = \"/r\"\nstore = \"/s.redb\"\n\
                    tracker = { github = \"acme/widgets\", credential = \"env\", ledger = true }\n";
        let msg = format!(
            "{:#}",
            load_text(text).expect_err("`ledger = true` is refused")
        );
        assert!(msg.contains("invalid type: boolean `true`"), "{msg}");
        assert!(msg.contains("expected a string"), "{msg}");
    }

    // Two entries on one root and one store that differ only in `ledger`
    // are two trackers: refused, naming which has the GitHub ledger.
    #[test]
    fn two_entries_differing_only_by_ledger_are_refused_naming_which_has_it() {
        let root = tempfile::tempdir().unwrap();
        let binding = |ledger| TrackerBinding {
            github: "acme/widgets".into(),
            credential: Credential::Env,
            ledger,
        };
        let entries = vec![
            Entry {
                name: None,
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-same.redb"),
                tracker: Some(binding(None)),
                state: None,
            },
            Entry {
                name: None,
                root: root.path().to_path_buf(),
                store: PathBuf::from("/tmp/fl-config-test-same.redb"),
                tracker: Some(binding(Some(LedgerChoice::Github))),
                state: None,
            },
        ];
        let msg = format!("{:#}", bound_entry(&entries, root.path()).unwrap_err());
        assert!(
            msg.contains("-> github:acme/widgets (ledger github)"),
            "{msg}"
        );
        assert!(msg.contains("-> github:acme/widgets,"), "{msg}");
    }

    fn mcp(root: &Path, disable: &[&str], enable: &[&str]) -> McpEntry {
        McpEntry {
            root: root.to_path_buf(),
            disable: disable.iter().map(|s| s.to_string()).collect(),
            enable: enable.iter().map(|s| s.to_string()).collect(),
        }
    }

    // MCP spec §2.2: a project's machine switches, independent of any
    // `[[project]]` binding; both lists default to empty.
    #[test]
    fn an_mcp_entry_loads_with_its_switches() {
        let cfg = load_text(
            "[[mcp]]\nroot = \"/code/app\"\ndisable = [\"github\"]\nenable = [\"sentry\"]\n\
             [[mcp]]\nroot = \"/code/other\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.mcp,
            vec![
                mcp(Path::new("/code/app"), &["github"], &["sentry"]),
                mcp(Path::new("/code/other"), &[], &[]),
            ]
        );
        assert!(cfg.projects.is_empty());
        let cfg = load_text("[[project]]\nroot = \"/r\"\nstore = \"/s.redb\"\n").unwrap();
        assert!(
            cfg.mcp.is_empty(),
            "a config with no [[mcp]] has no switches"
        );
    }

    // A typo in a switch is refused, not ignored; `antigravity` was a key
    // of an earlier design and is not one now (MCP spec §2.2).
    #[test]
    fn an_unknown_key_in_an_mcp_entry_is_refused() {
        for key in ["antigravity", "disabled", "store"] {
            let text = format!("[[mcp]]\nroot = \"/code/app\"\n{key} = \"x\"\n");
            let msg = format!("{:#}", load_text(&text).expect_err(key));
            assert!(
                msg.contains(&format!("unknown field `{key}`")),
                "{key}: {msg}"
            );
        }
    }

    #[test]
    fn a_relative_mcp_root_is_refused() {
        for bad in ["code/app", ""] {
            let text = format!("[[mcp]]\nroot = \"{bad}\"\n");
            let msg = format!("{:#}", load_text(&text).expect_err(bad));
            assert!(
                msg.contains("`root` in `[[mcp]]` must be an absolute path"),
                "{bad:?}: {msg}"
            );
        }
        assert_eq!(
            load_text("[[mcp]]\nroot = \"/code/app\"\n")
                .unwrap()
                .mcp
                .len(),
            1
        );
    }

    // MCP spec §2.2: "The same name in `enable` and `disable` is an error"
    // — within one entry; two projects may switch one name each way.
    #[test]
    fn a_server_both_enabled_and_disabled_is_refused_naming_it() {
        let text = "[[mcp]]\nroot = \"/code/app\"\n\
                    enable = [\"sentry\", \"github\"]\ndisable = [\"docs\", \"github\"]\n";
        let msg = format!("{:#}", load_text(text).unwrap_err());
        assert!(
            msg.contains("`github` is in both `enable` and `disable`"),
            "{msg}"
        );
        assert!(!msg.contains("`sentry` is in both"), "{msg}");
        let cfg = load_text(
            "[[mcp]]\nroot = \"/code/app\"\nenable = [\"github\"]\n\
             [[mcp]]\nroot = \"/code/other\"\ndisable = [\"github\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.mcp.len(), 2);
    }

    // MCP spec §2.2: the `[[project]]` rule — the entry whose root is the
    // longest ancestor of the working directory — and no other.
    #[test]
    fn mcp_entry_picks_the_entry_with_the_longest_root() {
        let outer = tempfile::tempdir().unwrap();
        let inner = outer.path().join("app");
        std::fs::create_dir_all(inner.join("src")).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let entries = vec![
            mcp(outer.path(), &["outer"], &[]),
            mcp(&inner, &["inner"], &[]),
            mcp(outer.path(), &["outer-too"], &[]),
            mcp(elsewhere.path(), &["elsewhere"], &[]),
        ];
        let got = mcp_entry(&entries, &inner.join("src")).unwrap();
        assert_eq!(got, Some(entries[1].clone()), "the longest root wins");
        assert_eq!(
            mcp_entry(&entries, &inner).unwrap(),
            Some(entries[1].clone())
        );
        let none = tempfile::tempdir().unwrap();
        assert_eq!(mcp_entry(&entries, none.path()).unwrap(), None);
        assert_eq!(
            mcp_entry(&entries, &none.path().join("not-there")).unwrap(),
            None,
            "a working directory that does not exist binds nothing"
        );
        assert_eq!(mcp_entry(&[], &inner).unwrap(), None);
    }

    #[test]
    fn two_different_mcp_entries_on_one_root_are_refused_naming_both() {
        let root = tempfile::tempdir().unwrap();
        for (a, b) in [
            (
                mcp(root.path(), &["github"], &[]),
                mcp(root.path(), &["sentry"], &[]),
            ),
            (
                mcp(root.path(), &["github"], &["docs"]),
                mcp(root.path(), &["github"], &[]),
            ),
        ] {
            let msg = format!("{:#}", mcp_entry(&[a, b], root.path()).unwrap_err());
            assert!(
                msg.contains("has more than one `[[mcp]]` entry in the config"),
                "{msg}"
            );
            assert!(msg.contains("Keep one of these entries"), "{msg}");
            assert_eq!(msg.matches(" (disable [").count(), 2, "{msg}");
        }
        let msg = format!(
            "{:#}",
            mcp_entry(
                &[
                    mcp(root.path(), &["github"], &[]),
                    mcp(root.path(), &["sentry"], &["docs"]),
                ],
                root.path()
            )
            .unwrap_err()
        );
        assert!(
            msg.contains("(disable [\"github\"], enable [])")
                && msg.contains("(disable [\"sentry\"], enable [\"docs\"])"),
            "the refusal must name both entries: {msg}"
        );
    }

    // Two entries that say the same thing about one root — however the root
    // is spelled — are one choice, not a conflict.
    #[test]
    fn identical_mcp_entries_on_one_root_are_not_a_conflict() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        let e = mcp(root.path(), &["github"], &["sentry"]);
        let respelled = mcp(&root.path().join("sub/.."), &["github"], &["sentry"]);
        assert_eq!(
            mcp_entry(&[e.clone(), respelled], root.path()).unwrap(),
            Some(e.clone())
        );
        assert_eq!(
            mcp_entry(&[e.clone(), e.clone()], root.path()).unwrap(),
            Some(e)
        );
    }

    #[test]
    fn an_empty_or_relative_xdg_state_home_falls_back_to_home() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            state_dir(Some(PathBuf::from("")), home.clone()),
            Some(PathBuf::from("/home/u/.local/state")),
            "an empty XDG_STATE_HOME must be treated as unset"
        );
        assert_eq!(
            state_dir(Some(PathBuf::from("relative/state")), home.clone()),
            Some(PathBuf::from("/home/u/.local/state")),
            "a relative XDG_STATE_HOME must be treated as unset"
        );
        assert_eq!(
            state_dir(Some(PathBuf::from("/abs/state")), home.clone()),
            Some(PathBuf::from("/abs/state")),
            "an absolute XDG_STATE_HOME must still win"
        );
        assert_eq!(
            state_dir(None, home),
            Some(PathBuf::from("/home/u/.local/state"))
        );
        assert_eq!(state_dir(Some(PathBuf::from("rel")), None), None);
        assert_eq!(
            state_dir(Some(PathBuf::from("/abs/state")), None),
            Some(PathBuf::from("/abs/state"))
        );
    }

    #[test]
    fn parses_workspace_and_project_state_config_and_routes_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let ws_store = dir.path().join("ws-state.redb");
        let ws_repo = dir.path().join("notes");
        let proj_root = dir.path().join("FerroLoop");
        let proj_store = dir.path().join("ferroloop.redb");
        let proj_state_repo = dir.path().join("private-state");
        let cfg = load_text(&format!(
            "[state]\nstore = \"{ws}\"\nrepo = \"{wr}\"\nremote = \"origin\"\nprivate = true\n\n\
             [[project]]\nname = \"ferroloop\"\nroot = \"{pr}\"\nstore = \"{ps}\"\n\
             state = {{ repo = \"{psr}\", remote = \"upstream\", private = false }}\n",
            ws = ws_store.display(),
            wr = ws_repo.display(),
            pr = proj_root.display(),
            ps = proj_store.display(),
            psr = proj_state_repo.display(),
        ))
        .unwrap();
        let default_store = dir.path().join("default.redb");

        assert_eq!(cfg.workspace_state_store(&default_store), ws_store);
        assert_eq!(
            cfg.state_store_for_scope(&StateScope::Workspace, &default_store),
            ws_store
        );
        assert_eq!(
            cfg.state_store_for_scope(&StateScope::Project("ferroloop".into()), &default_store),
            proj_store
        );
        assert_eq!(
            cfg.state_store_for_scope(&StateScope::Project("other".into()), &default_store),
            ws_store
        );

        let proj_target = cfg.sync_target_for_scope(
            Some(&StateScope::Project("ferroloop".into())),
            dir.path(),
            &default_store,
            "ferris",
        );
        assert_eq!(proj_target.repo_dir, proj_state_repo);
        assert_eq!(proj_target.remote, "upstream");
        assert!(!proj_target.private);

        let ferro_entry = cfg.find_project_entry("ferroloop").unwrap();
        assert!(ferro_entry.matches_slug("FerroLoop"));
        assert_eq!(ferro_entry.inferred_slug(), Some("ferroloop".into()));

        let fallback_entry = Entry {
            name: None,
            root: PathBuf::from("/path/to/My-Project"),
            store: PathBuf::from("/tmp/my-project.redb"),
            tracker: None,
            state: None,
        };
        assert!(fallback_entry.matches_slug("my-project"));
        assert_eq!(fallback_entry.inferred_slug(), Some("my-project".into()));
    }

    #[test]
    fn rejects_relative_state_store_or_repo_path() {
        let err = load_text("[state]\nstore = \"relative/state.redb\"\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("must be an absolute path"),
            "got: {err:#}"
        );
    }

    #[test]
    fn parses_wire_config_and_rejects_relative_paths() {
        let err = load_text("[wire]\nsocket = \"relative/wire.sock\"\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("`wire.socket` must be an absolute path"),
            "got: {err:#}"
        );
        let err2 = load_text("[wire]\ntoken_file = \"relative/token\"\n").unwrap_err();
        assert!(
            format!("{err2:#}").contains("`wire.token_file` must be an absolute path"),
            "got: {err2:#}"
        );

        let cfg = load_text(
            "[wire]\n\
             socket = \"/run/fl/wire.sock\"\n\
             listen = \"127.0.0.1:4222\"\n\
             peers = [\"ws://10.0.0.2:4222\"]\n\
             token_file = \"/etc/fl/token\"\n\
             retention_days = 14\n\
             sync_wire = false\n",
        )
        .unwrap();
        let w = cfg.wire.as_ref().unwrap();
        assert_eq!(w.socket, Some(PathBuf::from("/run/fl/wire.sock")));
        assert_eq!(w.listen.as_deref(), Some("127.0.0.1:4222"));
        assert_eq!(w.peers, vec!["ws://10.0.0.2:4222".to_string()]);
        assert_eq!(w.token_file, Some(PathBuf::from("/etc/fl/token")));
        assert_eq!(w.retention_days, Some(14));
        assert_eq!(w.sync_wire, Some(false));

        let target = cfg.sync_target_for_scope(
            Some(&StateScope::Workspace),
            Path::new("/tmp"),
            Path::new("/tmp/default.redb"),
            "ferris",
        );
        assert!(!target.sync_wire);
    }

    #[test]
    fn parses_adapter_rate_cards_and_refuses_unknown_adapters_or_empty_binary() {
        let cfg = load_text(
            "[adapters.codex]\n\
             binary = \"/opt/bin/codex\"\n\
             input_usd_micros_per_mtok = 2500000\n\
             output_usd_micros_per_mtok = 10000000\n\n\
             [adapters.claude]\n\
             binary = \"claude-custom\"\n",
        )
        .unwrap();
        let codex = cfg.adapters.get(&AdapterKind::Codex).unwrap();
        assert_eq!(codex.binary.as_deref(), Some("/opt/bin/codex"));
        assert_eq!(
            codex.rate_card(),
            Some(RateCard {
                input_usd_micros_per_mtok: 2_500_000,
                output_usd_micros_per_mtok: 10_000_000,
            })
        );
        let claude = cfg.adapters.get(&AdapterKind::Claude).unwrap();
        assert_eq!(claude.binary.as_deref(), Some("claude-custom"));
        assert_eq!(claude.rate_card(), None);

        let err = load_text("[adapters.telepathy]\nbinary = \"mind\"\n").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown adapter `telepathy`"), "{msg}");
        assert!(
            msg.contains("claude, codex, antigravity, muse, grok"),
            "{msg}"
        );

        let empty_bin = load_text("[adapters.muse]\nbinary = \"   \"\n").unwrap_err();
        assert!(
            format!("{empty_bin:#}").contains("`adapters.muse.binary` must not be empty"),
            "{empty_bin:#}"
        );
    }

    #[test]
    fn adapter_env_pass_is_parsed_and_validated_at_load() {
        let cfg = load_text(
            "[adapters.claude]\nenv_pass = [\"HTTPS_PROXY\", \"AWS_PROFILE\", \"HTTPS_PROXY\", \"GH_TOKEN\"]\n",
        )
        .unwrap();
        assert_eq!(
            cfg.adapters.get(&AdapterKind::Claude).unwrap().env_pass,
            ["HTTPS_PROXY", "AWS_PROFILE", "HTTPS_PROXY", "GH_TOKEN"]
        );
        assert!(
            load_text("[adapters.codex]\nbinary = \"codex\"\n")
                .unwrap()
                .adapters[&AdapterKind::Codex]
                .env_pass
                .is_empty()
        );

        let many = (0..=fl_exec::child_env::MAX_ENV_PASS)
            .map(|i| format!("\"V{i}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let long = "A".repeat(fl_exec::child_env::MAX_ENV_NAME_LEN + 1);
        for (list, why) in [
            ("\"FL_VAULT_MASTER_KEY\"", "can never reach"),
            ("\"FERROGATE_TOKEN\"", "can never reach"),
            ("\"FL_\"", "can never reach"),
            ("\"1BAD\"", "not a variable name"),
            ("\"A=B\"", "not a variable name"),
            ("\"\"", "not a variable name"),
            ("\"HTTPS PROXY\"", "not a variable name"),
            (&format!("\"{long}\""), "longer than"),
            (&many, "at most"),
        ] {
            let err = load_text(&format!("[adapters.muse]\nenv_pass = [{list}]\n")).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("`adapters.muse.env_pass`"), "{msg}");
            assert!(msg.contains(why), "{list}: {msg}");
        }
        // Names, not values: a table or a string is not a list of names, and a key it does not
        // know is still refused.
        assert!(load_text("[adapters.muse]\nenv_pass = \"HTTPS_PROXY\"\n").is_err());
        assert!(load_text("[adapters.muse]\nenv_passs = [\"X\"]\n").is_err());
    }

    #[test]
    fn parses_hub_config_and_rejects_relative_paths() {
        let err = load_text("[hub]\nui_dir = \"relative/ui/dist\"\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("`hub.ui_dir` must be an absolute path"),
            "got: {err:#}"
        );
        let err2 = load_text("[hub]\nowner_token_file = \"relative/owner.token\"\n").unwrap_err();
        assert!(
            format!("{err2:#}").contains("`hub.owner_token_file` must be an absolute path"),
            "got: {err2:#}"
        );

        let cfg = load_text(
            "[hub]\n\
             listen = \"127.0.0.1:8420\"\n\
             ui_dir = \"/srv/ferrodeck/dist\"\n\
             owner_token_file = \"/etc/fl/owner.token\"\n\
             trust_tailscale_headers = true\n\
             enable_terminal = false\n\
             session_ttl_hours = 48\n",
        )
        .unwrap();
        let h = cfg.hub.as_ref().unwrap();
        assert_eq!(h.listen.as_deref(), Some("127.0.0.1:8420"));
        assert_eq!(h.ui_dir, Some(PathBuf::from("/srv/ferrodeck/dist")));
        assert_eq!(
            h.owner_token_file,
            Some(PathBuf::from("/etc/fl/owner.token"))
        );
        assert!(h.trust_tailscale_headers);
        assert_eq!(h.enable_terminal, Some(false));
        assert_eq!(h.session_ttl_hours, Some(48));
    }

    #[test]
    fn hub_repo_root_is_validated_like_ui_dir() {
        // (toml value, expected error fragment or None when valid)
        let table: [(&str, Option<&str>); 6] = [
            ("\"/srv/repo\"", None),
            ("\"/srv/repo/\"", None),
            (
                "\"relative/repo\"",
                Some("`hub.repo_root` must be an absolute path"),
            ),
            ("\".\"", Some("`hub.repo_root` must be an absolute path")),
            ("\"\"", Some("`hub.repo_root` must be an absolute path")),
            (
                "\"/srv/re\\u0000po\"",
                Some("`hub.repo_root` must not contain a NUL byte"),
            ),
        ];
        for (value, want) in table {
            let got = load_text(&format!("[hub]\nrepo_root = {value}\n"));
            match want {
                None => {
                    let cfg = got.unwrap_or_else(|e| panic!("{value}: {e:#}"));
                    let root = cfg.hub.unwrap().repo_root.unwrap();
                    assert!(root.starts_with("/srv/repo"), "{value}: {root:?}");
                }
                Some(frag) => {
                    let err = format!("{:#}", got.expect_err(value));
                    assert!(err.contains(frag), "{value}: {err}");
                }
            }
        }
        // unset stays unset
        assert_eq!(
            load_text("[hub]\nlisten = \"127.0.0.1:1\"\n")
                .unwrap()
                .hub
                .unwrap()
                .repo_root,
            None
        );
    }

    fn err_of(text: &str) -> String {
        format!("{:#}", load_text(text).unwrap_err())
    }

    #[test]
    fn a_built_in_vendor_takes_overrides_and_the_registry_is_untouched() {
        let cfg = load_text(
            "[vendors.Kimi-API]\n\
             base_url = \"http://127.0.0.1:9000/v1\"\n\
             default_model = \"kimi-custom\"\n\
             input_usd_micros_per_m = 600000\n\
             output_usd_micros_per_mtok = 2500000\n",
        )
        .unwrap();
        let v = cfg.vendors.get("kimi-api").expect("keyed by lowercase id");
        assert_eq!(v.base_url.as_deref(), Some("http://127.0.0.1:9000/v1"));
        assert_eq!(v.default_model.as_deref(), Some("kimi-custom"));
        assert_eq!(
            v.rate_card(),
            Some(RateCard {
                input_usd_micros_per_mtok: 600_000,
                output_usd_micros_per_mtok: 2_500_000,
            })
        );
        // The built-in descriptor is never mutated.
        let d = fl_core::lookup_vendor("kimi-api").unwrap();
        assert_eq!(d.default_model, "kimi-k2.5");
        assert!(matches!(
            d.transport,
            VendorTransport::HttpApi { default_base_url, .. }
                if default_base_url == "https://api.moonshot.ai/v1"
        ));
        // Either spelling of the rate keys reads.
        let cfg = load_text(
            "[vendors.ollama]\ninput_usd_micros_per_mtok = 1\noutput_usd_micros_per_m = 2\n",
        )
        .unwrap();
        assert_eq!(cfg.vendors["ollama"].output_usd_micros_per_mtok, Some(2));
    }

    #[test]
    fn a_vendor_entry_refuses_what_cannot_be_right() {
        let msg = err_of("[vendors.ollama]\nbase_url = \"http://127.0.0.1:1/v1\"\nbogus = 1\n");
        assert!(
            msg.contains("bogus") && msg.contains("config.toml"),
            "{msg}"
        );

        let msg = err_of("[vendors.openai-api]\nmcp_config_rel_path = \".x/mcp.json\"\n");
        assert!(
            msg.contains("[vendors.openai-api]") && msg.contains("mcp_config_rel_path"),
            "{msg}"
        );
        let msg = err_of("[vendors.claude]\nmcp_config_rel_path = \".x/mcp.json\"\n");
        assert!(msg.contains("mcp_config_rel_path"), "{msg}");

        let msg = err_of("[vendors.openai-api]\nbase_url = \"http://example.com/v1\"\n");
        assert!(msg.contains("plain http"), "{msg}");
        let msg = err_of("[vendors.openai-api]\nbase_url = \"https://user:pw@example.com/v1\"\n");
        assert!(msg.contains("user name or password"), "{msg}");

        // A subscription CLI takes no API settings and points at `[adapters.*]`.
        let msg = err_of("[vendors.claude]\nbase_url = \"https://example.com\"\n");
        assert!(
            msg.contains("subscription CLI") && msg.contains("[adapters.claude]"),
            "{msg}"
        );
        let msg = err_of("[vendors.codex]\ninput_usd_micros_per_mtok = 1\n");
        assert!(msg.contains("[adapters.codex]"), "{msg}");

        // A built-in's identity is not an override.
        let msg = err_of("[vendors.openai-api]\nenv_key = \"OTHER\"\n");
        assert!(msg.contains("env_key"), "{msg}");
        let msg = err_of("[vendors.openai-api]\ndefault_model = \"  \"\n");
        assert!(msg.contains("default_model"), "{msg}");
    }

    #[test]
    fn a_new_vendor_needs_its_required_fields() {
        let full = [
            ("transport", "\"http_api\""),
            ("dialect", "\"openai_chat_completions\""),
            ("base_url", "\"http://127.0.0.1:8001/v1\""),
            ("default_model", "\"m\""),
        ];
        for skip in 0..full.len() {
            let body: String = full
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, (k, v))| format!("{k} = {v}\n"))
                .collect();
            let msg = err_of(&format!("[vendors.cfg-missing-{skip}]\n{body}"));
            assert!(
                msg.contains(&format!("[vendors.cfg-missing-{skip}]"))
                    && msg.contains(full[skip].0),
                "{msg}"
            );
            assert!(
                fl_core::lookup_vendor(&format!("cfg-missing-{skip}")).is_none(),
                "an invalid entry registers nothing"
            );
        }
        let msg = err_of(
            "[vendors.cfg-cli]\ntransport = \"subscription_cli\"\ndialect = \"anthropic_messages\"\n",
        );
        assert!(msg.contains("code adapter"), "{msg}");
        let msg =
            err_of("[vendors.cfg-bad-dialect]\ntransport = \"http_api\"\ndialect = \"soap\"\n");
        assert!(msg.contains("soap"), "{msg}");
        let msg = err_of(
            "[vendors.cfg-bad-url]\ntransport = \"http_api\"\ndialect = \"anthropic_messages\"\n\
             base_url = \"http://example.com\"\ndefault_model = \"m\"\n",
        );
        assert!(
            msg.contains("[vendors.cfg-bad-url]") && msg.contains("plain http"),
            "{msg}"
        );
        let msg = err_of(
            "[vendors.\"Not A Slug\"]\ntransport = \"http_api\"\ndialect = \"anthropic_messages\"\n\
             base_url = \"https://example.com\"\ndefault_model = \"m\"\n",
        );
        assert!(msg.contains("not a slug"), "{msg}");
    }

    #[test]
    fn a_new_vendor_registers_and_the_same_config_loads_twice() {
        let text = "[vendors.cfg-vllm]\n\
                    transport = \"http_api\"\n\
                    dialect = \"openai_chat_completions\"\n\
                    base_url = \"http://localhost:8001/v1\"\n\
                    default_model = \"qwen\"\n\
                    display_name = \"Local vLLM\"\n\
                    env_key = \"CFG_VLLM_KEY\"\n\
                    context_window = 32000\n\
                    input_usd_micros_per_mtok = 10\n\
                    output_usd_micros_per_mtok = 20\n\
                    mcp_config_rel_path = \".vllm/mcp.json\"\n";
        let first = load_text(text).unwrap();
        assert!(first.vendors.contains_key("cfg-vllm"));
        let d = fl_core::lookup_vendor("cfg-vllm").expect("registered");
        assert_eq!(d.display_name, "Local vLLM");
        assert_eq!(d.default_model, "qwen");
        assert_eq!(d.default_context_window, 32_000);
        assert_eq!(d.mcp_config_rel_path.as_deref(), Some(".vllm/mcp.json"));
        assert_eq!(d.default_rate_card.output_usd_micros_per_mtok, 20);
        // `vault_secret` defaults to `env_key`.
        assert_eq!(
            d.transport,
            VendorTransport::HttpApi {
                default_base_url: "http://localhost:8001/v1".into(),
                api_dialect: ApiDialect::OpenAiChatCompletions,
                vault_secret_name: Some("CFG_VLLM_KEY".into()),
                env_key_name: Some("CFG_VLLM_KEY".into()),
            }
        );
        // The same file again is fine; a conflicting definition of the id is an error.
        load_text(text).unwrap();
        let msg = err_of(&text.replace("qwen", "llama"));
        assert!(
            msg.contains("cfg-vllm") && msg.contains("already registered"),
            "{msg}"
        );
        assert_eq!(
            fl_core::lookup_vendor("cfg-vllm").unwrap().default_model,
            "qwen"
        );
    }

    #[test]
    fn a_conflict_with_a_registered_vendor_registers_none_of_the_files_vendors() {
        let first = "[vendors.cfg-pre-first]\ntransport = \"http_api\"\n\
                     dialect = \"openai_chat_completions\"\nbase_url = \"http://127.0.0.1:8003/v1\"\n\
                     default_model = \"m\"\n";
        let taken = "[vendors.cfg-pre-taken]\ntransport = \"http_api\"\n\
                     dialect = \"openai_chat_completions\"\nbase_url = \"http://127.0.0.1:8004/v1\"\n\
                     default_model = \"one\"\n";
        load_text(taken).unwrap();
        // `cfg-pre-first` sorts before `cfg-pre-taken`: it would be registered first.
        let msg = err_of(&format!("{first}{}", taken.replace("one", "two")));
        assert!(
            msg.contains("[vendors.cfg-pre-taken]") && msg.contains("already registered"),
            "{msg}"
        );
        assert!(
            fl_core::lookup_vendor("cfg-pre-first").is_none(),
            "an earlier vendor of the failed file must not be registered"
        );
        assert_eq!(
            fl_core::lookup_vendor("cfg-pre-taken")
                .unwrap()
                .default_model,
            "one"
        );
        // The same two, now consistent, load.
        load_text(&format!("{first}{taken}")).unwrap();
        assert!(fl_core::lookup_vendor("cfg-pre-first").is_some());
    }

    #[test]
    fn a_new_vendor_with_no_key_name_is_keyless_and_a_vault_secret_alone_is_kept() {
        load_text(
            "[vendors.cfg-keyless]\ntransport = \"http_api\"\ndialect = \"openai_chat_completions\"\n\
             base_url = \"http://127.0.0.1:8002/v1\"\ndefault_model = \"m\"\n\
             [vendors.cfg-vaulted]\ntransport = \"http_api\"\ndialect = \"gemini_generate_content\"\n\
             base_url = \"https://gw.example.com\"\ndefault_model = \"m\"\nvault_secret = \"gw_key\"\n",
        )
        .unwrap();
        let keyless = fl_core::lookup_vendor("cfg-keyless").unwrap();
        assert!(!keyless.transport.needs_api_key());
        let vaulted = fl_core::lookup_vendor("cfg-vaulted").unwrap();
        assert!(matches!(
            vaulted.transport,
            VendorTransport::HttpApi {
                vault_secret_name: Some(ref v),
                env_key_name: None,
                ..
            } if v == "gw_key"
        ));
    }

    #[test]
    fn a_new_vendor_cannot_replace_a_built_in_by_another_spelling() {
        // `Claude` is the built-in `claude`: it is an override entry, never a definition.
        let msg = err_of(
            "[vendors.Claude]\ntransport = \"http_api\"\ndialect = \"anthropic_messages\"\n\
             base_url = \"https://example.com\"\ndefault_model = \"m\"\n",
        );
        assert!(msg.contains("subscription CLI"), "{msg}");
        assert!(matches!(
            fl_core::lookup_vendor("claude").unwrap().transport,
            VendorTransport::SubscriptionCli { .. }
        ));
    }
}
