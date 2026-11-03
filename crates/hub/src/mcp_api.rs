//! `GET /api/v1/mcp/catalog` and `POST /api/v1/mcp/sync` (Record #81, Gate G5): the hub's view of
//! the committed MCP catalog (`.fl/mcp.toml`, docs/mcp.md) over the `fl-mcp` library. Both answer
//! the Owner only: the answer names servers and paths, so a Peer gets a 403 and an
//! unauthenticated caller a 401 (before this module is reached).
//!
//! **A pulled catalog is executable config.** A sync turns the catalog's commands into the agent
//! CLIs' files, which those CLIs run. So a sync is a compare-and-swap: a row carries a
//! `catalog_digest`, and `POST` must send it back as `expect_digest`; if the catalog, the machine's
//! switches or the files a sync would write changed since the caller looked, nothing is written.
//!
//! **Known repos.** The hub knows one repo: its `repo_root`. `HubServerConfig` carries no
//! `[[project]]` list, so the configured projects' roots are not listed here. A known repo's
//! project directory is found as `fl mcp` finds it: the nearest directory, from the canonical
//! root up, that holds `.fl/mcp.toml`, else the nearest that holds `.git`. A repo with neither,
//! or with no catalog, is `missing`. The project directory must be the root itself or the root's
//! git root, and must not be the hub's home directory or one above it (a `~/.fl/mcp.toml` or a
//! dotfiles repository would write `~/.mcp.json`), and the home must be known and exist:
//! otherwise the row is an `error`.
//!
//! **`GET /api/v1/mcp/catalog`** answers `{ "repos": [ row, ... ] }`, one row per known repo:
//!
//! - `repo` (the root's directory name), `root`, `catalog_path`, and `status`:
//!   `synced` (`fl mcp check` exits 0), `drift` (a sync would change a vendor file), `missing`
//!   (no catalog) or `error` (the catalog is malformed, a vendor file is refused, git would
//!   commit a vendor file, ...). The FerroDeck type knows only the first three; for `error` the
//!   row carries an `error` text, and `refusals` (one line per refused entry) when entries were
//!   refused.
//! - Additive fields: `enabled_servers` (server names after this machine's switches, or `null`
//!   when the plan failed before the switches were read), `vendors` (per vendor `synced`,
//!   `drift` or `refused`, or `null` when no plan was made), `changes` (what a sync would do, as
//!   `{ vendor, server, action: add|update|remove }`: names only, or `null` with no plan) and
//!   `catalog_digest` (the hex SHA-256 over the catalog's bytes, the machine's switches and the
//!   bytes a sync would write to each vendor file; `null` unless the status is `synced` or
//!   `drift`). The digest is a hash: it shows nothing of what it covers.
//! - `deduplicated_daemons`, `active_vendor_sessions` and `spawned_mcp_processes` are always
//!   `null`: nothing in the hub measures them. They are not invented and not `0`.
//!
//! Only server names and statuses leave the hub: never an `env` or header value, a command or
//! its arguments, or a secret's variable name. A refusal or an error text comes from the
//! library, which never quotes a catalog or vendor-file value.
//!
//! **`POST /api/v1/mcp/sync`** takes `{ "repo": "<name or absolute path>", "expect_digest":
//! "<catalog_digest>" }`, both required: a body without either (`{}` and an empty body too) is a
//! 400 naming the field, so no click syncs anything the caller did not look at. A name is a
//! known repo's `repo`; a path must be a known root after canonicalisation. Anything else is a
//! 404 (a name that two roots share is a 400): the hub never syncs a path it was not configured
//! with. Any other key is a 400. A digest that no longer matches is that repo's `error`
//! ("catalog changed since you looked; reload"), and nothing is written.
//!
//! The answer is `{ "ok": bool, "repos": [ { "repo", "status_before", "status_after",
//! "written": [ path relative to the project dir ], "error"? } ] }`. **Partial failure:** a
//! request the hub can read answers 200 whatever the repos did; `ok` is `false` when any repo
//! has an `error`, and that repo's `error` says why (`written` is then empty: a sync writes all of a
//! repo's files or none). The 400, 403 and 404 answers are `{ "error" }`. A repo with no catalog
//! is a no-op with a `note`, not an error. A sync that finds another one running for longer than
//! [`BUSY_WAIT`] answers 200 `{ "ok": false, "error": "busy: ...", "repos": [] }`.
//!
//! **What a sync writes.** The vendor files `fl mcp sync` writes under the project directory,
//! each a plain file beside a temporary one, and nothing else under the project; plus the
//! library's ownership records and lock under fl's state directory
//! (`$XDG_STATE_HOME/fl/mcp`, default `~/.local/state/fl/mcp`), the same records `fl mcp` keeps,
//! so the hub and the CLI agree on which entries are fl's. Every vendor target is a path
//! relative to the project (the five built-ins' and a registered vendor's, which the library
//! refuses unless it is inside the project); a plan with any other target is refused before
//! anything is written ([`project_relative`]). The hub never reads or writes a vendor's user-level
//! file (`~/.claude.json`, `~/.codex/config.toml`, ...), and unlike the CLI it prints no Codex
//! trust warning.
//!
//! **Same gates as the CLI.** The plan uses the library's `plan`/`apply`; a vendor file that git
//! tracks or would not ignore is refused, as `fl mcp sync` refuses it. The two git questions run
//! hardened (`git_question`): an empty environment but `PATH`, the installer's `-c` overrides, so a
//! repository's `core.fsmonitor` or hooks never run, and a deadline with a process-group kill.
//! The machine's `[[mcp]]` switches come from the resolver the CLI sets with
//! [`crate::HubState::set_mcp_switches`], once, when the hub starts; with none set, the team
//! defaults apply.
//!
//! **Concurrency.** One mutex serialises every catalog read and sync in this hub; a request that
//! cannot take it within [`BUSY_WAIT`] is answered `busy` (a 503 for the GET). The library's
//! file lock serialises against `fl mcp sync` processes; the library waits on it with no
//! deadline, so a wedged `fl mcp sync` holds one hub request and the mutex until it ends.
//! Handlers are blocking functions: the server runs each request in `spawn_blocking`, so no
//! filesystem or git work runs on the async runtime.

use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use fl_mcp::catalog::Catalog;
use fl_mcp::sync::{self, Action, Check, Plan, Switches};
use fl_mcp::vendor;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::auth::HubAuthority;
use crate::{HttpRequest, HttpResponse, HubState};

const CATALOG: &str = "/api/v1/mcp/catalog";
const SYNC: &str = "/api/v1/mcp/sync";

/// How long a request waits for another one to finish before it answers `busy`.
const BUSY_WAIT: Duration = Duration::from_secs(5);
/// The most time one git question may take.
const GIT_DEADLINE: Duration = Duration::from_secs(10);

/// The machine's switches for a project directory (docs/mcp.md, "Each machine's switches").
type Resolver = Arc<dyn Fn(&Path) -> Result<Switches, String> + Send + Sync>;

/// What the hub keeps for its MCP endpoints.
#[derive(Default)]
pub(crate) struct McpHub {
    /// Held across every catalog read and every sync.
    gate: Mutex<()>,
    settings: Mutex<Settings>,
}

#[derive(Default)]
struct Settings {
    /// Where the ownership records live; `None`: fl's state directory.
    records: Option<PathBuf>,
    switches: Option<Resolver>,
    /// How long to wait for the gate; `None`: [`BUSY_WAIT`].
    wait: Option<Duration>,
}

impl fmt::Debug for McpHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpHub").finish_non_exhaustive()
    }
}

impl McpHub {
    pub(crate) fn set_switches(&self, resolver: Resolver) {
        self.settings().switches = Some(resolver);
    }

    #[cfg(test)]
    pub(crate) fn set_records_dir(&self, dir: PathBuf) {
        self.settings().records = Some(dir);
    }

    #[cfg(test)]
    pub(crate) fn set_wait(&self, wait: Duration) {
        self.settings().wait = Some(wait);
    }

    fn settings(&self) -> MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The gate, or `None` when another request holds it for longer than the wait.
    pub(crate) fn gate(&self) -> Option<MutexGuard<'_, ()>> {
        let end = Instant::now() + self.settings().wait.unwrap_or(BUSY_WAIT);
        loop {
            match self.gate.try_lock() {
                Ok(held) => return Some(held),
                Err(TryLockError::Poisoned(e)) => return Some(e.into_inner()),
                Err(TryLockError::WouldBlock) if Instant::now() >= end => return None,
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// The ownership-record directory, as `fl mcp` resolves it (`<state base>/fl/mcp`).
    fn records_dir(&self) -> Result<PathBuf, String> {
        if let Some(dir) = self.settings().records.clone() {
            return Ok(dir);
        }
        let absolute = |k: &str| {
            std::env::var_os(k)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let base = absolute("XDG_STATE_HOME")
            .or_else(|| absolute("HOME").map(|h| h.join(".local/state")))
            .ok_or("neither an absolute $XDG_STATE_HOME nor $HOME is set, so there is nowhere to keep fl's ownership records")?;
        Ok(base.join("fl").join("mcp"))
    }

    fn switches(&self, project: &Path) -> Result<Switches, String> {
        let resolver = self.settings().switches.clone();
        resolver.map_or_else(|| Ok(Switches::default()), |r| r(project))
    }
}

/// The repos the hub knows: its `repo_root`. See the module note.
fn known_roots(state: &HubState) -> Vec<PathBuf> {
    vec![state.config.repo_root.clone()]
}

/// The name a repo goes by: its (canonical) root's directory name.
fn repo_name(root: &Path) -> String {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    canonical
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_string_lossy().into_owned())
}

/// `fl mcp`'s project rule, from a repo root: the nearest directory, from the canonical root up,
/// that holds `.fl/mcp.toml`, else the nearest that holds `.git`. `None` when the root is absent
/// or there is neither. The canonical root comes back with it.
fn project_dir(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let root = fs::canonicalize(root).ok()?;
    let found = (root.ancestors().find(|d| Catalog::path(d).is_file()))
        .or_else(|| root.ancestors().find(|d| d.join(".git").exists()));
    let found = found.map(Path::to_path_buf)?;
    Some((found, root))
}

/// Why the hub will not write at `project`, for a hub whose root is `root` and whose home is
/// `home`: it is the home directory or above it, or it is neither the root nor the root's git
/// root. A catalog there is not this repository's.
fn project_problem(project: &Path, root: &Path, home: Option<&Path>) -> Option<String> {
    // Fail closed: a home the hub cannot name is a home it cannot keep out of.
    let Some(home) = home.and_then(|h| fs::canonicalize(h).ok()) else {
        return Some("cannot determine the home directory; refusing to sync".into());
    };
    if home.starts_with(project) {
        return Some(
            "the catalog's project is the home directory or a directory above it, and the hub \
             would write the agent CLIs' files there; run `fl mcp sync` yourself if you mean it"
                .into(),
        );
    }
    let git_root = root.ancestors().find(|d| d.join(".git").exists());
    if project != root && Some(project) != git_root {
        return Some(
            "the catalog is not in this repository: it sits above the repository's root, and the \
             hub writes only inside the repository"
                .into(),
        );
    }
    None
}

/// `path` as a path inside `project`: `Some(relative)` only when it is `project` joined with
/// plain components (no `..`, no root, no prefix). The hub writes nothing else.
fn project_relative(project: &Path, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(project).ok()?;
    let plain = rel.components().all(|c| matches!(c, Component::Normal(_)));
    (plain && rel.components().next().is_some()).then(|| rel.to_path_buf())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Synced,
    Drift,
    Missing,
    Error,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Synced => "synced",
            Status::Drift => "drift",
            Status::Missing => "missing",
            Status::Error => "error",
        }
    }
}

/// What looking at one repo found.
struct Probe {
    status: Status,
    catalog_path: PathBuf,
    /// The project directory, when there is a catalog to plan from.
    project: Option<PathBuf>,
    plan: Option<Plan>,
    enabled: Option<Vec<String>>,
    error: Option<String>,
    refusals: Vec<String>,
    /// Set for `synced` and `drift` only.
    digest: Option<String>,
}

impl Probe {
    fn new(status: Status, catalog_path: PathBuf, project: Option<PathBuf>) -> Probe {
        Probe {
            status,
            catalog_path,
            project,
            plan: None,
            enabled: None,
            error: None,
            refusals: Vec::new(),
            digest: None,
        }
    }

    fn failed(catalog_path: PathBuf, project: Option<PathBuf>, error: String) -> Probe {
        let mut probe = Probe::new(Status::Error, catalog_path, project);
        probe.error = Some(error);
        probe
    }
}

/// Adds one length-prefixed field, so no two inputs hash alike by running together.
fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// The digest of what a sync would do: the catalog's bytes as read, the machine's switches, and
/// the bytes it would write to each vendor file. A hash only; nothing of the input shows.
fn digest(catalog: &[u8], switches: &Switches, project: &Path, plan: &Plan) -> String {
    let mut hash = Sha256::new();
    field(&mut hash, b"fl-hub-mcp-digest-v1");
    field(&mut hash, catalog);
    for list in [&switches.enable, &switches.disable] {
        let mut names = list.clone();
        names.sort();
        field(&mut hash, &(names.len() as u64).to_le_bytes());
        for name in names {
            field(&mut hash, name.as_bytes());
        }
    }
    for target in plan.targets() {
        field(&mut hash, target.vendor.as_str().as_bytes());
        let rel = project_relative(project, &target.path).unwrap_or_default();
        field(&mut hash, rel.as_os_str().as_encoded_bytes());
        match target.written_bytes() {
            Some(bytes) => {
                field(&mut hash, b"write");
                field(&mut hash, bytes);
            }
            None => field(&mut hash, b"keep"),
        }
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Plans one repo exactly as `fl mcp check` does (no write, no registry, no network).
fn probe(hub: &McpHub, root: &Path, home: Option<&Path>) -> Probe {
    let Some((project, canonical_root)) = project_dir(root) else {
        return Probe::new(Status::Missing, Catalog::path(root), None);
    };
    let catalog_path = Catalog::path(&project);
    let fail = |error: String| Probe::failed(catalog_path.clone(), Some(project.clone()), error);
    if let Some(problem) = project_problem(&project, &canonical_root, home) {
        return fail(problem);
    }
    // The bytes are read once: the digest covers exactly what the plan is made from.
    let bytes = match fs::read(&catalog_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Probe::new(Status::Missing, catalog_path, Some(project));
        }
        Err(e) => return fail(format!("could not read {}: {e}", catalog_path.display())),
    };
    let catalog = match String::from_utf8(bytes.clone())
        .map_err(|_| format!("{} is not valid UTF-8", catalog_path.display()))
        .and_then(|text| Catalog::parse(&text, &catalog_path).map_err(|e| e.to_string()))
    {
        Ok(catalog) => catalog,
        Err(e) => return fail(e),
    };
    let switches = match hub.switches(&project) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let records = match hub.records_dir() {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let plan = match sync::plan(&project, &catalog, &switches, &records, &[]) {
        Ok(plan) => plan,
        Err(e) => return fail(e.to_string()),
    };
    let enabled: Vec<String> = (catalog.servers.iter())
        .filter(|(name, server)| {
            let on = server.enabled || switches.enable.contains(name);
            on && !switches.disable.contains(name)
        })
        .map(|(name, _)| name.clone())
        .collect();
    let mut probe = Probe::new(Status::Synced, catalog_path, Some(project.clone()));
    probe.enabled = Some(enabled);
    if let Err(e) = confine(&project, &plan).and_then(|()| git_gate(&project, &plan)) {
        probe.status = Status::Error;
        probe.error = Some(e);
        probe.plan = Some(plan);
        return probe;
    }
    match plan.check() {
        Check::Matches => {}
        Check::Changes => probe.status = Status::Drift,
        Check::Refused => {
            probe.status = Status::Error;
            probe.refusals = plan.refusals().iter().map(|r| r.to_string()).collect();
            probe.error = Some(format!(
                "{} entries were refused; nothing would be written",
                probe.refusals.len()
            ));
        }
    }
    if matches!(probe.status, Status::Synced | Status::Drift) {
        probe.digest = Some(digest(&bytes, &switches, &project, &plan));
    }
    probe.plan = Some(plan);
    probe
}

/// Every vendor target is a plain path inside the project; anything else is refused.
fn confine(project: &Path, plan: &Plan) -> Result<(), String> {
    for target in plan.targets() {
        if project_relative(project, &target.path).is_none() {
            return Err(format!(
                "the {} target is not a path inside the project, and the hub writes only inside it",
                target.vendor.as_str()
            ));
        }
    }
    Ok(())
}

/// Asks git one question in `dir`, hardened as the installer runs git: nothing inherited but
/// `PATH`, the installer's `-c` overrides (no `core.fsmonitor`, no hooks, no attributes file),
/// no optional locks, and a deadline that kills git's whole process group. `Ok` is git's exit
/// code and standard output.
fn git_question(dir: &Path, args: &[&str]) -> Result<(i32, Vec<u8>), String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut command = Command::new("git");
    command.env_clear().envs(fl_install::hardened_env(&path));
    command.arg("-C").arg(dir);
    command.args(["--no-pager", "--no-optional-locks", "--no-replace-objects"]);
    for kv in fl_install::HARDENING {
        command.arg("-c").arg(kv);
    }
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not run git: {e}"))?;
    let pid = child.id() as i32;
    // Both pipes are drained as git runs, bounded: a question has a short answer.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(pipe) = pipe {
                let _ = pipe.take(1 << 20).read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let end = Instant::now() + GIT_DEADLINE;
    // The leader is watched without being reaped, so its pid is ours until the group is killed.
    let status = loop {
        match exited(pid) {
            true => {
                // Anything git started in its group goes with it. SAFETY: the leader is a
                // zombie, not reaped, so the group number cannot belong to anyone else.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                match child.wait() {
                    Ok(status) => break status,
                    Err(e) => return Err(format!("could not wait for git: {e}")),
                }
            }
            false if Instant::now() >= end => {
                // SAFETY: the child is this process's own and is not yet reaped, so the
                // process group number is still its own.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                let _ = child.wait();
                return Err(format!(
                    "git did not answer within {} seconds",
                    GIT_DEADLINE.as_secs()
                ));
            }
            false => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    match status.code() {
        Some(code) if code == 0 || code == 1 => Ok((code, stdout)),
        _ => Err(format!(
            "git {} failed in {}: {}",
            args.first().copied().unwrap_or(""),
            dir.display(),
            String::from_utf8_lossy(&stderr).trim()
        )),
    }
}

/// Whether the child `pid` has exited, without reaping it.
fn exited(pid: i32) -> bool {
    // SAFETY: `info` is zeroed and written by `waitid`; WNOWAIT leaves the child waitable.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        r != 0 || info.si_pid() != 0
    }
}

/// docs/mcp.md ("The `.gitignore` lines"): a vendor file fl writes must be ignored and not
/// tracked. Asked of git, which fails rather than answer `no`.
fn git_gate(project: &Path, plan: &Plan) -> Result<(), String> {
    let mut problems = Vec::new();
    let writes = |t: &sync::Target| {
        (t.entries.iter()).any(|e| !matches!(e.action, Action::Untouched | Action::Forget))
    };
    for target in plan.targets().iter().filter(|t| writes(t)) {
        let rel = vendor::vendor(target.vendor).target();
        let (_, listed) = git_question(project, &["ls-files", "--", rel])?;
        if !listed.is_empty() {
            problems.push(format!("{rel} is tracked by git"));
        } else if git_question(project, &["check-ignore", "-q", "--", rel])?.0 != 0 {
            problems.push(format!("{rel} is not ignored by git"));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "fl writes these files from the catalog, so git must ignore them ({}): add them to \
         .gitignore; nothing was written",
        problems.join("; ")
    ))
}

/// Per vendor: `refused`, `drift` or `synced`.
fn vendors(plan: &Plan) -> Value {
    let mut map = serde_json::Map::new();
    for target in plan.targets() {
        let refused = (target.entries.iter()).any(|e| matches!(e.action, Action::Refused(_)));
        let state = if refused {
            "refused"
        } else if target.changes() {
            "drift"
        } else {
            "synced"
        };
        map.insert(target.vendor.as_str().to_string(), json!(state));
    }
    Value::Object(map)
}

/// What a sync would do, by name: `{ vendor, server, action }` for each entry it adds, updates
/// or removes. No command, argument or value.
fn changes(plan: &Plan) -> Value {
    let mut out = Vec::new();
    for target in plan.targets() {
        for entry in &target.entries {
            let action = match entry.action {
                Action::Add => "add",
                Action::Update | Action::Replace { .. } => "update",
                Action::Remove => "remove",
                _ => continue,
            };
            out.push(json!({
                "vendor": target.vendor.as_str(),
                "server": entry.name,
                "action": action,
            }));
        }
    }
    json!(out)
}

fn row(root: &Path, probe: &Probe) -> Value {
    let mut row = json!({
        "repo": repo_name(root),
        "root": fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()).display().to_string(),
        "catalog_path": probe.catalog_path.display().to_string(),
        "status": probe.status.as_str(),
        "deduplicated_daemons": Value::Null,
        "active_vendor_sessions": Value::Null,
        "spawned_mcp_processes": Value::Null,
        "enabled_servers": probe.enabled,
        "vendors": probe.plan.as_ref().map(vendors),
        "changes": probe.plan.as_ref().map(changes),
        "catalog_digest": probe.digest,
    });
    if let Some(error) = &probe.error {
        row["error"] = json!(error);
    }
    if !probe.refusals.is_empty() {
        row["refusals"] = json!(probe.refusals);
    }
    row
}

const BUSY: &str = "busy: another MCP catalog request is running; retry in a moment";

fn error(status: u16, message: impl Into<String>) -> HttpResponse {
    HttpResponse::json(status, json!({ "error": message.into() }))
}

/// `GET /api/v1/mcp/catalog`.
fn handle_get(state: &HubState) -> HttpResponse {
    let hub = &state.mcp;
    let Some(_held) = hub.gate() else {
        return error(503, BUSY).with_header("Retry-After", "5");
    };
    let home = crate::api::hub_home_dir(state);
    let repos: Vec<Value> = known_roots(state)
        .iter()
        .map(|root| row(root, &probe(hub, root, home.as_deref())))
        .collect();
    HttpResponse::json(200, json!({ "repos": repos }))
}

/// The known root and the digest a request names. Both fields are required.
fn select(roots: Vec<PathBuf>, body: &Value) -> Result<(PathBuf, String), HttpResponse> {
    let object = body.as_object().expect("json_object_body gave an object");
    if let Some(key) = object
        .keys()
        .find(|k| *k != "repo" && *k != "expect_digest")
    {
        return Err(error(
            400,
            format!("unknown field `{key}`; the body takes only `repo` and `expect_digest`"),
        ));
    }
    let digest = match object.get("expect_digest") {
        None | Some(Value::Null) => {
            return Err(error(
                400,
                "`expect_digest` is required: send the `catalog_digest` of the row you looked at \
                 (GET /api/v1/mcp/catalog); a sync of a catalog you have not seen is refused",
            ));
        }
        Some(Value::String(d)) if d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()) => {
            d.to_ascii_lowercase()
        }
        Some(_) => {
            return Err(error(
                400,
                "`expect_digest` must be a row's `catalog_digest`: 64 hex digits",
            ));
        }
    };
    let given = match object.get("repo") {
        None | Some(Value::Null) => {
            return Err(error(
                400,
                "`repo` is required: name one repo from GET /api/v1/mcp/catalog",
            ));
        }
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim(),
        Some(_) => return Err(error(400, "`repo` must be a non-empty string")),
    };
    let unknown = || {
        error(
            404,
            "no such repo; GET /api/v1/mcp/catalog lists the repos the hub knows",
        )
    };
    if given.contains('/') || given.starts_with('.') {
        if !Path::new(given).is_absolute() {
            return Err(error(400, "a `repo` path must be absolute"));
        }
        let wanted = fs::canonicalize(given).map_err(|_| unknown())?;
        let hit = roots
            .into_iter()
            .find(|r| fs::canonicalize(r).is_ok_and(|c| c == wanted));
        return hit.map(|r| (r, digest)).ok_or_else(unknown);
    }
    let mut named: Vec<PathBuf> = roots
        .into_iter()
        .filter(|r| repo_name(r) == given)
        .collect();
    match named.len() {
        0 => Err(unknown()),
        1 => Ok((named.remove(0), digest)),
        _ => Err(error(
            400,
            "more than one known repo has that name; name it by its absolute path",
        )),
    }
}

/// Syncs one repo; the hub's gate is held.
fn sync_one(hub: &McpHub, root: &Path, home: Option<&Path>, expect: &str) -> Value {
    let before = probe(hub, root, home);
    let mut result = json!({
        "repo": repo_name(root),
        "status_before": before.status.as_str(),
        "status_after": before.status.as_str(),
        "written": [],
    });
    match before.status {
        Status::Missing => {
            result["note"] = json!("no MCP catalog (.fl/mcp.toml); nothing to sync");
            return result;
        }
        Status::Error => {
            result["error"] = json!(before.error.clone().unwrap_or_default());
            if !before.refusals.is_empty() {
                result["refusals"] = json!(before.refusals);
            }
            return result;
        }
        Status::Synced | Status::Drift => {}
    }
    if before.digest.as_deref() != Some(expect) {
        result["error"] = json!("catalog changed since you looked; reload and review it again");
        return result;
    }
    let (Some(project), Some(plan)) = (&before.project, &before.plan) else {
        return result;
    };
    let written: Vec<String> = plan
        .targets()
        .iter()
        .filter(|t| t.changes())
        .filter_map(|t| project_relative(project, &t.path))
        .map(|rel| rel.display().to_string())
        .collect();
    let applied = sync::apply(plan);
    let after = probe(hub, root, home);
    result["status_after"] = json!(after.status.as_str());
    match applied {
        Ok(()) => result["written"] = json!(written),
        Err(e) => result["error"] = json!(e.to_string()),
    }
    result
}

/// `POST /api/v1/mcp/sync`.
fn handle_post(state: &HubState, req: &HttpRequest) -> HttpResponse {
    let body = match crate::api::json_object_body(req) {
        Ok(body) => body,
        Err(resp) => return resp,
    };
    let (root, expect) = match select(known_roots(state), &body) {
        Ok(picked) => picked,
        Err(resp) => return resp,
    };
    let hub = &state.mcp;
    let Some(_held) = hub.gate() else {
        return HttpResponse::json(200, json!({ "ok": false, "error": BUSY, "repos": [] }));
    };
    let home = crate::api::hub_home_dir(state);
    let repos = vec![sync_one(hub, &root, home.as_deref(), &expect)];
    let ok = repos.iter().all(|r| r.get("error").is_none());
    HttpResponse::json(200, json!({ "ok": ok, "repos": repos }))
}

/// Routes the two MCP endpoints; `None` for any other path.
pub(crate) fn route(
    state: &HubState,
    authority: &HubAuthority,
    req: &HttpRequest,
) -> Option<HttpResponse> {
    let path = req.path.trim_end_matches('/');
    let method = match path {
        CATALOG => "GET",
        SYNC => "POST",
        _ => return None,
    };
    if !authority.is_owner() {
        return Some(error(403, "owner authority required"));
    }
    if req.method != method {
        return Some(
            error(405, format!("{path} takes only {method}")).with_header("Allow", method),
        );
    }
    Some(match path {
        CATALOG => handle_get(state),
        _ => handle_post(state, req),
    })
}

#[cfg(test)]
#[path = "mcp_api_tests.rs"]
mod tests;
