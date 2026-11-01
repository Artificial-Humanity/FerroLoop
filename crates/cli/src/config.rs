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

#[derive(Debug, Default)]
pub struct Config {
    pub state: Option<StateConfig>,
    pub wire: Option<WireConfig>,
    pub hub: Option<HubConfig>,
    pub projects: Vec<Entry>,
    pub github: Option<GithubApp>,
    pub mcp: Vec<McpEntry>,
    pub adapters: BTreeMap<AdapterKind, AdapterConfig>,
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
        adapters.insert(kind, adapter_cfg);
    }
    Ok(Config {
        state: file.state,
        wire: file.wire,
        hub: file.hub,
        projects: file.project,
        github: file.github,
        mcp: file.mcp,
        adapters,
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
}
