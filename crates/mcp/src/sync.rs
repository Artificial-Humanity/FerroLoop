//! `fl mcp sync` and `fl mcp check` (MCP spec §4.3, §4.5): the entries the
//! catalog wants in each vendor file, a plan for every target, and its
//! application. fl changes and removes only the entries it wrote, and knows
//! them by an ownership record per target, kept on this machine and never in
//! a repository. Every target is planned before any is written, and one
//! refusal writes nothing. Nothing here runs git or reads the network.

use crate::McpError;
use crate::catalog::{Catalog, VendorName};
use crate::vendor::{self, Rendered, VendorRefusal};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// The lock file in the records directory, held for the whole of a write.
pub const LOCK: &str = "sync.lock";

/// This machine's switches for the project, from the `[[mcp]]` entry in the
/// user's config (MCP spec §2.2). A name the catalog does not have is
/// ignored, with a warning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Switches {
    /// On here, though the team default is off.
    pub enable: Vec<String>,
    /// Off here, though the team default is on. The config refuses a name
    /// in both lists; were one in both, `disable` would win.
    pub disable: Vec<String>,
}

/// The ownership record of one target: every entry fl wrote there, with the
/// project root it came from and the SHA-256 of its bytes as written. It
/// holds names, paths and hashes only; a secret's value is never in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The target's canonical path.
    pub target: PathBuf,
    pub entries: BTreeMap<String, Owned>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owned {
    /// The project root, canonical.
    pub root: PathBuf,
    /// Hex SHA-256 of the entry's bytes as fl wrote them.
    pub sha256: String,
}

/// What `sync` does to every target, worked out before anything is written.
#[derive(Debug, Clone)]
pub struct Plan {
    records: PathBuf,
    targets: Vec<Target>,
    skipped: Vec<VendorRefusal>,
    warnings: Vec<String>,
}

/// One vendor file and what happens to each name in it.
#[derive(Debug, Clone)]
pub struct Target {
    pub vendor: VendorName,
    /// The project root joined with the vendor's file.
    pub path: PathBuf,
    /// Every name in the file, the record or the catalog: the file's own
    /// order first, then the rest by name.
    pub entries: Vec<Entry>,
    canonical: PathBuf,
    /// The file as planned; `None` when it did not exist.
    before: Option<Vec<u8>>,
    /// The file to write; `None` when no entry in it changes.
    after: Option<Vec<u8>>,
    record_file: PathBuf,
    /// The record to write; `None` when it does not change.
    record: Option<Record>,
}

impl Target {
    /// Whether `sync` would change the file.
    pub fn changes(&self) -> bool {
        self.after.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub action: Action,
}

/// The classes of MCP spec §4.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Wanted; in neither the file nor the record, or the whole file is
    /// gone (a fresh clone, `git clean -X`) and fl writes it anew.
    Add,
    /// fl's, as fl wrote it, and the catalog wants it different.
    Update,
    /// fl's, as fl wrote it, and the catalog no longer wants it here.
    Remove,
    /// Exactly what fl would write, but not in the record: recovers a crash
    /// between a write and its record. Or fl's, laid out anew (by a
    /// formatter, or agy's panel) but meaning what fl would write. The
    /// record changes, the file does not.
    Adopt,
    /// fl's, removed by hand, and no longer wanted: only the record changes.
    Forget,
    /// A refusal `--replace` named: overwritten (or removed, when no longer
    /// wanted).
    Replace {
        difference: String,
    },
    /// fl's, and already what the catalog wants.
    Unchanged,
    /// Not fl's and not wanted: never touched.
    Untouched,
    Refused(Refusal),
}

/// An entry fl will not change, and why (MCP spec §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub path: PathBuf,
    pub name: String,
    pub kind: RefusalKind,
    /// Which fields differ, never their values; empty for `Removed`.
    pub difference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// fl wrote it and it has changed since.
    HandEdited,
    /// fl wrote it and it is gone, though the catalog still wants it.
    Removed,
    /// fl did not write it, and it is not what fl would write.
    Foreign,
}

/// The difference of an entry the catalog no longer wants in this file.
const NO_LONGER_WANTED: &str = "the catalog no longer has it here, so fl would remove it";

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (path, name, difference) = (self.path.display(), &self.name, &self.difference);
        let replace = format!("run `fl mcp sync --replace {name}`");
        match self.kind {
            // The catalog no longer wants it here: `--replace` removes it.
            RefusalKind::HandEdited if difference == NO_LONGER_WANTED => write!(
                f,
                "{path}: `{name}` was changed by hand since fl wrote it ({difference}). Add the \
                 server back to the catalog to keep the entry, or {replace}, which removes it"
            ),
            RefusalKind::HandEdited => write!(
                f,
                "{path}: `{name}` was changed by hand since fl wrote it ({difference}). Restore \
                 the entry, or {replace}, which shows the difference and overwrites it"
            ),
            RefusalKind::Removed => write!(
                f,
                "{path}: `{name}` was removed by hand since fl wrote it. Restore the entry, or \
                 {replace}, which writes it again"
            ),
            RefusalKind::Foreign => write!(
                f,
                "{path}: `{name}` is an entry fl did not write, and it differs from what fl \
                 would write ({difference}). Rename or remove it, or {replace}, which shows \
                 the difference and overwrites it"
            ),
        }
    }
}

/// What `fl mcp check` reports (MCP spec §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// Every target matches the catalog.
    Matches,
    /// A `sync` would change a file.
    Changes,
    /// A `sync` would refuse.
    Refused,
}

impl Check {
    /// 0, 1 or 2; an error is 2 as well.
    pub fn exit_code(self) -> u8 {
        match self {
            Check::Matches => 0,
            Check::Changes => 1,
            Check::Refused => 2,
        }
    }
}

impl Plan {
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// The servers a vendor cannot run, each left out of that vendor's file
    /// only (MCP spec §4.2). Reported; not a refusal.
    pub fn skipped(&self) -> &[VendorRefusal] {
        &self.skipped
    }

    /// A machine switch naming a server the catalog does not have: it is
    /// ignored, and said so (MCP spec §2.2). Not a refusal.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn refusals(&self) -> Vec<&Refusal> {
        let entries = self.targets.iter().flat_map(|t| &t.entries);
        entries
            .filter_map(|e| match &e.action {
                Action::Refused(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    pub fn check(&self) -> Check {
        if !self.refusals().is_empty() {
            Check::Refused
        } else if self.targets.iter().any(Target::changes) {
            Check::Changes
        } else {
            Check::Matches
        }
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines = Vec::new();
        for target in &self.targets {
            let title = vendor::vendor(target.vendor).title();
            let head = format!("{title}, {}:", target.path.display());
            let mut body = Vec::new();
            for Entry { name, action } in &target.entries {
                let line = match action {
                    Action::Add => format!("add {name}"),
                    Action::Update => format!("update {name}"),
                    Action::Remove => format!("remove {name}"),
                    Action::Adopt => format!("adopt {name} (means what fl would write)"),
                    Action::Forget => format!("forget {name} (removed by hand, no longer wanted)"),
                    Action::Replace { difference } => format!("replace {name} ({difference})"),
                    Action::Refused(_) => format!("refused {name}"),
                    Action::Unchanged | Action::Untouched => continue,
                };
                body.push(format!("  {line}"));
            }
            if body.is_empty() {
                lines.push(format!("{head} no change"));
            } else {
                lines.push(head);
                lines.extend(body);
            }
        }
        lines.extend(self.skipped.iter().map(ToString::to_string));
        lines.extend(self.refusals().iter().map(ToString::to_string));
        write!(f, "{}", lines.join("\n"))
    }
}

/// Plans every vendor file under `root` against the catalog and this
/// machine's switches. `records` is the directory of ownership records;
/// `replace` names the refused entries to overwrite. Reads, never writes.
pub fn plan(
    root: &Path,
    catalog: &Catalog,
    switches: &Switches,
    records: &Path,
    replace: &[String],
) -> Result<Plan, McpError> {
    let (desired, skipped, warnings) = desired(root, catalog, switches);
    let owner = canonical(root)?;
    let targets: Vec<Target> = VendorName::ALL
        .into_iter()
        .map(|v| plan_target(root, &owner, v, &desired[&v], records, replace))
        .collect::<Result<_, _>>()?;
    unmatched_replaces(&targets, replace)?;
    Ok(Plan {
        records: records.to_path_buf(),
        targets,
        skipped,
        warnings,
    })
}

/// MCP spec §4.3: a `--replace` name that overwrote an entry in no target
/// matched nothing, and is refused. A name is repeated only when it is a
/// server name; any other may be a secret someone pasted (MCP spec §6).
fn unmatched_replaces(targets: &[Target], replace: &[String]) -> Result<(), McpError> {
    let replaced: BTreeSet<&str> = targets
        .iter()
        .flat_map(|t| &t.entries)
        .filter(|e| matches!(e.action, Action::Replace { .. }))
        .map(|e| e.name.as_str())
        .collect();
    let unmatched: BTreeSet<&String> = replace
        .iter()
        .filter(|n| !replaced.contains(n.as_str()))
        .collect();
    if unmatched.is_empty() {
        return Ok(());
    }
    let (names, others): (Vec<&String>, Vec<&String>) = unmatched
        .into_iter()
        .partition(|n| crate::catalog::is_server_name(n));
    Err(McpError::UnmatchedReplace {
        names: names.into_iter().cloned().collect(),
        others: others.len(),
    })
}

/// One vendor's file: every name in it, in its record or in `want`,
/// classified, and the file and the record as they would become.
fn plan_target(
    root: &Path,
    owner: &Path,
    name: VendorName,
    want: &BTreeMap<String, Rendered>,
    records: &Path,
    replace: &[String],
) -> Result<Target, McpError> {
    let v = vendor::vendor(name);
    let path = root.join(v.target());
    // A link would carry fl's write into a file it cannot see whole: one git
    // tracks (the gitignore guard asks about the link's own path), or one
    // outside the project.
    if let Some(link) = link_in(owner, Path::new(v.target()))? {
        let problem = if link == Path::new(v.target()) {
            "it is a symbolic link, and fl writes only plain files it can see whole".to_string()
        } else {
            format!(
                "{} is a symbolic link, and fl writes only plain files it can see whole",
                root.join(&link).display()
            )
        };
        return Err(McpError::VendorFile(vendor::FileRefusal {
            path,
            problem,
            next: "Replace the link with a plain file or directory, then run `fl mcp sync` again"
                .into(),
        }));
    }
    let canonical = canonical(&path)?;
    let before = read(&canonical, &path)?;
    let mut file = v.open(&path, before.as_deref())?;
    let record_file = record_file(records, &canonical);
    let old = read_record(&record_file)?.unwrap_or_else(|| Record {
        target: canonical.clone(),
        entries: BTreeMap::new(),
    });
    let mut record = old.clone();
    let mut names = file.names();
    let rest: BTreeSet<&String> = old.entries.keys().chain(want.keys()).collect();
    for n in rest {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    let mut changed = false;
    let mut entries = Vec::new();
    for n in names {
        let current = file.entry(&n);
        let fl = want.get(&n);
        let owned = old.entries.get(&n).map(|o| o.sha256.as_str());
        let replacing = replace.contains(&n);
        let gone = before.is_none();
        let action = classify(
            name,
            &path,
            &n,
            current.as_deref(),
            fl,
            owned,
            gone,
            replacing,
        );
        // The hash of what fl owns once this entry is done: `None` when
        // nothing.
        let wrote = match &action {
            Action::Add | Action::Update | Action::Remove | Action::Replace { .. } => {
                match fl {
                    Some(fl) => file.set(&n, fl),
                    None => _ = file.remove(&n),
                }
                changed = true;
                fl.map(|fl| sha256(fl.bytes()))
            }
            // The entry stays as it stands in the file.
            Action::Adopt => current.as_deref().map(sha256),
            Action::Forget => None,
            Action::Unchanged | Action::Untouched | Action::Refused(_) => {
                entries.push(Entry { name: n, action });
                continue;
            }
        };
        match wrote {
            Some(sha256) => {
                let owned = Owned {
                    root: owner.to_path_buf(),
                    sha256,
                };
                record.entries.insert(n.clone(), owned);
            }
            None => _ = record.entries.remove(&n),
        }
        entries.push(Entry { name: n, action });
    }
    Ok(Target {
        vendor: name,
        path,
        entries,
        canonical,
        after: changed.then(|| file.to_bytes()),
        before,
        record_file,
        record: (record != old).then_some(record),
    })
}

/// One name in one target, by MCP spec §4.3's table. `current` is the entry
/// in the file, `fl` what fl would write, `owned` the hash in the record;
/// `gone`, that the whole file is absent.
#[allow(clippy::too_many_arguments)]
fn classify(
    vendor: VendorName,
    path: &Path,
    name: &str,
    current: Option<&[u8]>,
    fl: Option<&Rendered>,
    owned: Option<&str>,
    gone: bool,
    replace: bool,
) -> Action {
    match (current, fl, owned) {
        (Some(cur), Some(fl), owned) if cur == fl.bytes() => {
            if owned == Some(sha256(cur).as_str()) {
                Action::Unchanged
            } else {
                Action::Adopt
            }
        }
        // fl's entry, laid out anew but meaning what fl would write.
        (Some(cur), Some(fl), Some(hash)) if same_meaning(vendor, name, cur, fl.bytes()) => {
            if sha256(cur) == hash {
                Action::Unchanged
            } else {
                Action::Adopt
            }
        }
        (Some(cur), fl, Some(hash)) if sha256(cur) == hash => {
            if fl.is_some() {
                Action::Update
            } else {
                Action::Remove
            }
        }
        (None, None, Some(_)) => Action::Forget,
        (None, Some(_), None) => Action::Add,
        // The whole file is gone (a fresh clone, `git clean -X`): fl's own
        // generated file is written anew. One that lost only this entry was
        // edited by hand, and is refused below.
        (None, Some(_), Some(_)) if gone => Action::Add,
        (Some(_), None, None) => Action::Untouched,
        // Changed or removed since fl wrote it, or someone else's in the way.
        (current, fl, owned) => {
            let difference = difference(vendor, name, current, fl);
            let kind = if owned.is_none() {
                RefusalKind::Foreign
            } else if current.is_none() {
                RefusalKind::Removed
            } else {
                RefusalKind::HandEdited
            };
            if replace {
                Action::Replace { difference }
            } else {
                Action::Refused(Refusal {
                    path: path.to_path_buf(),
                    name: name.to_string(),
                    kind,
                    difference,
                })
            }
        }
    }
}

/// Writes the plan: nothing at all when it holds a refusal or when a target
/// changed since it was planned. Holds the lock in the records directory
/// throughout, waiting for another `sync` to finish first.
pub fn apply(plan: &Plan) -> Result<(), McpError> {
    let refusals = plan.refusals();
    if !refusals.is_empty() {
        let refusals = refusals.into_iter().cloned().collect();
        return Err(McpError::Refused { refusals });
    }
    let work: Vec<&Target> = plan
        .targets
        .iter()
        .filter(|t| t.after.is_some() || t.record.is_some())
        .collect();
    if work.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(&plan.records).map_err(|e| io_error("write", &plan.records, e))?;
    let lock_path = plan.records.join(LOCK);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| io_error("write", &lock_path, e))?;
    lock.lock().map_err(|e| io_error("lock", &lock_path, e))?;
    // Every target read again before any is written: `agy mcp add`, or a
    // person, may have written one since the plan (MCP spec §4.3).
    for target in &work {
        if read(&target.canonical, &target.path)? != target.before {
            return Err(McpError::Changed {
                path: target.path.clone(),
            });
        }
    }
    let mut staged = Vec::new();
    for target in &work {
        let temp = match &target.after {
            Some(bytes) => Some(stage(target, bytes)?),
            None => None,
        };
        staged.push((target, temp));
    }
    for (target, temp) in staged {
        if let Some(temp) = temp {
            temp.persist(&target.canonical)
                .map_err(|e| io_error("write", &target.path, e.error))?;
        }
        // After the rename: a crash between the two leaves an entry exactly
        // as fl writes it and no record, which the next plan adopts.
        if let Some(record) = &target.record {
            write_record(&plan.records, &target.record_file, record)?;
        }
    }
    drop(lock);
    Ok(())
}

/// The new content in a temporary file beside the target, with the
/// original's permission bits, or a new file's default ones.
fn stage(target: &Target, bytes: &[u8]) -> Result<NamedTempFile, McpError> {
    let io = |e: io::Error| io_error("write", &target.path, e);
    let dir = target
        .canonical
        .parent()
        .expect("a target is in the project root");
    fs::create_dir_all(dir).map_err(io)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(".fl-mcp-");
    if target.before.is_none()
        && let Some(mode) = new_file_mode()
    {
        builder.permissions(mode);
    }
    let mut temp = builder.tempfile_in(dir).map_err(io)?;
    temp.write_all(bytes).map_err(io)?;
    if target.before.is_some() {
        let mode = fs::metadata(&target.canonical).map_err(io)?.permissions();
        fs::set_permissions(temp.path(), mode).map_err(io)?;
    }
    Ok(temp)
}

/// What `File::create` gives: the umask applies to it.
#[cfg(unix)]
fn new_file_mode() -> Option<fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    Some(fs::Permissions::from_mode(0o666))
}

#[cfg(not(unix))]
fn new_file_mode() -> Option<fs::Permissions> {
    None
}

/// The entries the catalog wants in each vendor's file, as fl writes them.
type Desired = BTreeMap<VendorName, BTreeMap<String, Rendered>>;

/// The entries the catalog wants, the servers a vendor cannot run, and a
/// warning for each machine switch naming a server the catalog does not
/// have, which is ignored (MCP spec §2.2).
fn desired(
    root: &Path,
    catalog: &Catalog,
    switches: &Switches,
) -> (Desired, Vec<VendorRefusal>, Vec<String>) {
    let mut warnings = Vec::new();
    let lists = [("enable", &switches.enable), ("disable", &switches.disable)];
    for (list, names) in lists {
        for name in names.iter().filter(|n| !catalog.servers.contains_key(*n)) {
            warnings.push(format!(
                "the `[[mcp]]` entry for this project in your fl config has `{name}` in \
                 `{list}`, and {} has no such server; fl ignores it. Remove it from that entry",
                Catalog::path(root).display()
            ));
        }
    }
    let mut desired: Desired = VendorName::ALL.map(|v| (v, BTreeMap::new())).into();
    let mut skipped = Vec::new();
    for (name, server) in &catalog.servers {
        let on = if switches.disable.contains(name) {
            false
        } else if switches.enable.contains(name) {
            true
        } else {
            server.enabled
        };
        if !on {
            continue;
        }
        for v in VendorName::ALL {
            if !server.is_for(v) {
                continue;
            }
            match vendor::vendor(v).render(name, server) {
                Ok(entry) => {
                    desired
                        .get_mut(&v)
                        .expect("every vendor")
                        .insert(name.clone(), entry);
                }
                Err(refusal) => skipped.push(refusal),
            }
        }
    }
    (desired, skipped, warnings)
}

/// Which fields of the entry differ from what fl would write: names only,
/// never a value, which could be a secret someone typed in.
fn difference(
    vendor: VendorName,
    name: &str,
    current: Option<&[u8]>,
    fl: Option<&Rendered>,
) -> String {
    let (current, fl) = match (current, fl) {
        (None, _) => return "it is not in the file".to_string(),
        (_, None) => return NO_LONGER_WANTED.to_string(),
        (Some(current), Some(fl)) => (current, fl.bytes()),
    };
    let (Some(theirs), Some(ours)) = (fields(vendor, name, current), fields(vendor, name, fl))
    else {
        return "its text differs".to_string();
    };
    let keys: BTreeSet<&String> = theirs.keys().chain(ours.keys()).collect();
    let mut parts = Vec::new();
    for key in keys {
        match (theirs.get(key), ours.get(key)) {
            (Some(a), Some(b)) if a != b => parts.push(format!("`{key}` differs")),
            (Some(_), None) => parts.push(format!("`{key}` is only in the file")),
            (None, Some(_)) => parts.push(format!("`{key}` is only in fl's")),
            _ => {}
        }
    }
    if parts.is_empty() {
        "only its layout or comments differ".to_string()
    } else {
        parts.join(", ")
    }
}

/// Whether two entries mean the same, whatever their layout: the same JSON
/// value, or the same TOML table, keys in any order.
fn same_meaning(vendor: VendorName, name: &str, a: &[u8], b: &[u8]) -> bool {
    match (value(vendor, name, a), value(vendor, name, b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// An entry as a value; `None` when it does not parse.
fn value(vendor: VendorName, name: &str, bytes: &[u8]) -> Option<serde_json::Value> {
    match vendor {
        VendorName::Codex => {
            let doc: toml::Table = toml::from_str(std::str::from_utf8(bytes).ok()?).ok()?;
            let entry = doc.get("mcp_servers")?.get(name)?;
            serde_json::to_value(entry).ok()
        }
        VendorName::Claude | VendorName::Antigravity => serde_json::from_slice(bytes).ok(),
    }
}

/// An entry's fields by dotted name, each value in a form fit only for
/// comparing; `None` when the entry is not a table.
fn fields(vendor: VendorName, name: &str, bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    let value = value(vendor, name, bytes)?;
    fn flatten(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    let key = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    flatten(&key, value, out);
                }
            }
            other => {
                out.insert(prefix.to_string(), other.to_string());
            }
        }
    }
    if !value.is_object() {
        return None;
    }
    let mut out = BTreeMap::new();
    flatten("", &value, &mut out);
    Some(out)
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The record of the target at `canonical`, named by the hash of its path.
fn record_file(records: &Path, canonical: &Path) -> PathBuf {
    records.join(format!(
        "{}.json",
        sha256(canonical.as_os_str().as_encoded_bytes())
    ))
}

fn read_record(file: &Path) -> Result<Option<Record>, McpError> {
    let Some(bytes) = read(file, file)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| McpError::Record {
            path: file.to_path_buf(),
            cause: e.to_string(),
        })
}

fn write_record(records: &Path, file: &Path, record: &Record) -> Result<(), McpError> {
    let io = |e: io::Error| io_error("write", file, e);
    let mut bytes = serde_json::to_vec_pretty(record).map_err(|e| io(e.into()))?;
    bytes.push(b'\n');
    let mut temp = NamedTempFile::new_in(records).map_err(io)?;
    temp.write_all(&bytes).map_err(io)?;
    temp.persist(file).map_err(|e| io(e.error))?;
    Ok(())
}

/// The path with every link resolved; for a file that does not exist yet,
/// its nearest existing ancestor's, joined with the rest.
fn canonical(path: &Path) -> Result<PathBuf, McpError> {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        match fs::canonicalize(at) {
            Ok(real) => return Ok(rest.iter().rev().fold(real, |p, c| p.join(c))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => match (at.parent(), at.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_owned());
                    at = parent;
                }
                _ => return Err(io_error("read", path, e)),
            },
            Err(e) => return Err(io_error("read", path, e)),
        }
    }
}

/// The first symbolic link on the way from the canonical project root to
/// `target`, relative to it: the root itself may be reached through a link,
/// nothing below it may.
fn link_in(owner: &Path, target: &Path) -> Result<Option<PathBuf>, McpError> {
    let mut at = PathBuf::new();
    for part in target.components() {
        at.push(part);
        match fs::symlink_metadata(owner.join(&at)) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(Some(at)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error("read", &owner.join(&at), e)),
        }
    }
    Ok(None)
}

/// The file's bytes; `None` when it does not exist. `path` names it.
fn read(file: &Path, path: &Path) -> Result<Option<Vec<u8>>, McpError> {
    match fs::read(file) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("read", path, e)),
    }
}

fn io_error(op: &'static str, path: &Path, e: io::Error) -> McpError {
    McpError::Io {
        op,
        path: path.to_path_buf(),
        cause: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Two servers every vendor can run: a stdio server with a secret and an
    /// http remote.
    const CATALOG: &str = r#"
[server.docs]
transport = "http"
url = "https://docs.example.com/mcp"

[server.notes]
transport = "stdio"
command = "npx"
args = ["-y", "@example/notes-mcp@1.2.0"]
env.NOTES_TOKEN = { secret = true }
"#;

    const PINNED: &str = "@example/notes-mcp@1.2.0";
    const UPGRADED: &str = "@example/notes-mcp@1.3.0";

    /// A project root and a records directory, both in one temporary
    /// directory, never the real home.
    struct Fixture {
        dir: TempDir,
        root: PathBuf,
        records: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        fs::create_dir(&root).unwrap();
        let records = dir.path().join("state").join("fl").join("mcp");
        Fixture { dir, root, records }
    }

    fn catalog(text: &str) -> Catalog {
        Catalog::parse(text, Path::new(".fl/mcp.toml")).unwrap()
    }

    fn off(names: &[&str]) -> Switches {
        Switches {
            disable: names.iter().map(|n| n.to_string()).collect(),
            ..Switches::default()
        }
    }

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    impl Fixture {
        fn plan(&self, text: &str) -> Plan {
            self.plan_with(text, &Switches::default(), &[])
        }

        fn plan_with(&self, text: &str, switches: &Switches, replace: &[&str]) -> Plan {
            let replace: Vec<String> = replace.iter().map(|n| n.to_string()).collect();
            plan(
                &self.root,
                &catalog(text),
                switches,
                &self.records,
                &replace,
            )
            .unwrap()
        }

        fn sync(&self, text: &str) {
            apply(&self.plan(text)).unwrap();
        }

        fn path(&self, v: VendorName) -> PathBuf {
            self.root.join(vendor::vendor(v).target())
        }

        fn read(&self, v: VendorName) -> Option<String> {
            fs::read_to_string(self.path(v)).ok()
        }

        fn write(&self, v: VendorName, text: &str) {
            let path = self.path(v);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        /// A person's edit of the file: `from` must be in it.
        fn edit(&self, v: VendorName, from: &str, to: &str) {
            let text = self.read(v).unwrap();
            assert!(text.contains(from), "{from} not in {text}");
            self.write(v, &text.replacen(from, to, 1));
        }

        /// The entry as it stands in the file.
        fn entry(&self, v: VendorName, name: &str) -> Option<Vec<u8>> {
            let bytes = fs::read(self.path(v)).ok();
            let file = vendor::vendor(v).open(&self.path(v), bytes.as_deref());
            file.unwrap().entry(name)
        }

        /// Every file in the fixture but the lock, with its bytes.
        fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
            fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
                for entry in fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else if path.file_name().unwrap() != LOCK {
                        out.push((path.clone(), fs::read(&path).unwrap()));
                    }
                }
            }
            let mut out = Vec::new();
            walk(self.dir.path(), &mut out);
            out.sort();
            out
        }

        fn record(&self, v: VendorName) -> Option<serde_json::Value> {
            let canonical = fs::canonicalize(self.path(v)).unwrap();
            let name = format!("{}.json", sha(canonical.as_os_str().as_encoded_bytes()));
            let text = fs::read_to_string(self.records.join(name)).ok()?;
            Some(serde_json::from_str(&text).unwrap())
        }
    }

    /// What fl writes for `name` from `text`'s catalog.
    fn rendered(v: VendorName, text: &str, name: &str) -> Vec<u8> {
        let catalog = catalog(text);
        let entry = vendor::vendor(v).render(name, &catalog.servers[name]);
        entry.unwrap().bytes().to_vec()
    }

    /// The target's actions by entry name.
    fn actions(plan: &Plan, v: VendorName) -> Vec<(String, Action)> {
        let target = plan.targets().iter().find(|t| t.vendor == v).unwrap();
        let entries = target.entries.iter();
        entries
            .map(|e| (e.name.clone(), e.action.clone()))
            .collect()
    }

    fn each(action: Action) -> Vec<(String, Action)> {
        vec![("docs".into(), action.clone()), ("notes".into(), action)]
    }

    fn refusal(action: &Action) -> String {
        match action {
            Action::Refused(r) => r.to_string(),
            other => panic!("not a refusal: {other:?}"),
        }
    }

    #[test]
    fn a_desired_entry_absent_from_the_file_and_the_record_is_added() {
        let f = fixture();
        let plan = f.plan(CATALOG);
        for v in VendorName::ALL {
            assert_eq!(actions(&plan, v), each(Action::Add), "{v:?}");
        }
        apply(&plan).unwrap();
        for v in VendorName::ALL {
            for name in ["docs", "notes"] {
                let expected = rendered(v, CATALOG, name);
                assert_eq!(f.entry(v, name), Some(expected.clone()), "{v:?} {name}");
                let record = f.record(v).unwrap();
                assert_eq!(record["entries"][name]["sha256"], sha(&expected));
            }
        }
    }

    #[test]
    fn an_entry_fl_wrote_is_updated_when_the_catalog_changes() {
        let f = fixture();
        f.sync(CATALOG);
        let upgraded = CATALOG.replace(PINNED, UPGRADED);
        let plan = f.plan(&upgraded);
        for v in VendorName::ALL {
            let expected = vec![
                ("docs".into(), Action::Unchanged),
                ("notes".into(), Action::Update),
            ];
            assert_eq!(actions(&plan, v), expected, "{v:?}");
        }
        apply(&plan).unwrap();
        for v in VendorName::ALL {
            let expected = rendered(v, &upgraded, "notes");
            assert_eq!(f.entry(v, "notes"), Some(expected.clone()), "{v:?}");
            assert_eq!(
                f.record(v).unwrap()["entries"]["notes"]["sha256"],
                sha(&expected)
            );
        }
    }

    #[test]
    fn an_entry_fl_wrote_is_removed_when_the_catalog_no_longer_has_it() {
        let f = fixture();
        f.sync(CATALOG);
        let docs_only = CATALOG.split("[server.notes]").next().unwrap();
        let plan = f.plan(docs_only);
        for v in VendorName::ALL {
            let expected = vec![
                ("docs".into(), Action::Unchanged),
                ("notes".into(), Action::Remove),
            ];
            assert_eq!(actions(&plan, v), expected, "{v:?}");
        }
        apply(&plan).unwrap();
        for v in VendorName::ALL {
            assert_eq!(f.entry(v, "notes"), None, "{v:?}");
            assert!(f.entry(v, "docs").is_some(), "{v:?}");
            let record = f.record(v).unwrap();
            assert!(record["entries"].get("notes").is_none(), "{record}");
        }
    }

    #[test]
    fn a_hand_edited_entry_is_refused_naming_the_remedy() {
        let f = fixture();
        f.sync(CATALOG);
        f.edit(VendorName::Claude, PINNED, "@example/notes-mcp@1.2.1");
        let before = f.snapshot();
        let plan = f.plan(CATALOG);
        let claude = actions(&plan, VendorName::Claude);
        assert_eq!(claude[0], ("docs".into(), Action::Unchanged));
        assert_eq!(
            refusal(&claude[1].1),
            format!(
                "{}: `notes` was changed by hand since fl wrote it (`args` differs). Restore \
                 the entry, or run `fl mcp sync --replace notes`, which shows the difference \
                 and overwrites it",
                f.path(VendorName::Claude).display()
            )
        );
        assert_eq!(plan.check(), Check::Refused);
        let err = apply(&plan).unwrap_err();
        assert!(matches!(err, McpError::Refused { .. }), "{err:?}");
        assert!(err.to_string().contains("nothing was written"), "{err}");
        assert!(err.to_string().contains("was changed by hand"), "{err}");
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn an_entry_removed_by_hand_is_refused_unless_the_catalog_no_longer_wants_it() {
        let f = fixture();
        f.sync(CATALOG);
        let codex = vendor::vendor(VendorName::Codex);
        let path = f.path(VendorName::Codex);
        let mut file = codex.open(&path, Some(&fs::read(&path).unwrap())).unwrap();
        assert!(file.remove("notes"));
        fs::write(&path, file.to_bytes()).unwrap();

        let plan = f.plan(CATALOG);
        assert_eq!(
            refusal(&actions(&plan, VendorName::Codex)[1].1),
            format!(
                "{}: `notes` was removed by hand since fl wrote it. Restore the entry, or run \
                 `fl mcp sync --replace notes`, which writes it again",
                path.display()
            )
        );

        let docs_only = CATALOG.split("[server.notes]").next().unwrap();
        let plan = f.plan(docs_only);
        assert_eq!(
            actions(&plan, VendorName::Codex)[1],
            ("notes".into(), Action::Forget)
        );
        assert_eq!(
            actions(&plan, VendorName::Claude)[1],
            ("notes".into(), Action::Remove)
        );
        apply(&plan).unwrap();
        let record = f.record(VendorName::Codex).unwrap();
        assert!(record["entries"].get("notes").is_none(), "{record}");
        assert!(f.plan(docs_only).targets().iter().all(|t| !t.changes()));
    }

    #[test]
    fn a_hand_edited_entry_the_catalog_no_longer_wants_is_refused_not_removed() {
        let f = fixture();
        f.sync(CATALOG);
        f.edit(VendorName::Claude, PINNED, "@example/notes-mcp@1.2.1");
        let docs_only = CATALOG.split("[server.notes]").next().unwrap();
        let before = f.snapshot();
        let plan = f.plan(docs_only);
        let claude = actions(&plan, VendorName::Claude);
        assert_eq!(claude[0], ("docs".into(), Action::Unchanged));
        assert_eq!(
            refusal(&claude[1].1),
            format!(
                "{}: `notes` was changed by hand since fl wrote it (the catalog no longer has it \
                 here, so fl would remove it). Add the server back to the catalog to keep the \
                 entry, or run `fl mcp sync --replace notes`, which removes it",
                f.path(VendorName::Claude).display()
            )
        );
        assert_eq!(plan.check(), Check::Refused);
        let err = apply(&plan).unwrap_err();
        assert!(matches!(err, McpError::Refused { .. }), "{err:?}");
        assert!(err.to_string().contains("was changed by hand"), "{err}");
        assert!(err.to_string().contains("which removes it"), "{err}");
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn replace_removes_a_hand_edited_entry_the_catalog_no_longer_wants() {
        let f = fixture();
        f.sync(CATALOG);
        f.edit(VendorName::Claude, PINNED, "@example/notes-mcp@1.2.1");
        let docs_only = CATALOG.split("[server.notes]").next().unwrap();
        let plan = f.plan_with(docs_only, &Switches::default(), &["notes"]);
        let difference = "the catalog no longer has it here, so fl would remove it".to_string();
        assert_eq!(
            actions(&plan, VendorName::Claude)[1],
            ("notes".into(), Action::Replace { difference })
        );
        apply(&plan).unwrap();
        assert_eq!(f.entry(VendorName::Claude, "notes"), None);
        assert!(f.entry(VendorName::Claude, "docs").is_some());
        let record = f.record(VendorName::Claude).unwrap();
        assert!(record["entries"].get("notes").is_none(), "{record}");
        assert!(record["entries"].get("docs").is_some(), "{record}");
        for v in [VendorName::Codex, VendorName::Antigravity] {
            assert_eq!(f.entry(v, "notes"), None, "{v:?}");
        }
        assert!(f.plan(docs_only).targets().iter().all(|t| !t.changes()));
    }

    #[test]
    fn someone_elses_entry_under_a_desired_name_is_refused() {
        let f = fixture();
        let theirs = "{\n  \"mcpServers\": {\n    \"notes\": {\"command\": \"notes-mcp\", \
                      \"timeout\": 30}\n  }\n}\n";
        f.write(VendorName::Claude, theirs);
        let plan = f.plan(CATALOG);
        let claude = actions(&plan, VendorName::Claude);
        assert_eq!(claude[0].0, "notes");
        assert_eq!(claude[1], ("docs".into(), Action::Add));
        assert_eq!(
            refusal(&claude[0].1),
            format!(
                "{}: `notes` is an entry fl did not write, and it differs from what fl would \
                 write (`args` is only in fl's, `command` differs, `env.NOTES_TOKEN` is only \
                 in fl's, `timeout` is only in the file). Rename or remove it, or run `fl mcp \
                 sync --replace notes`, which shows the difference and overwrites it",
                f.path(VendorName::Claude).display()
            )
        );
        assert_eq!(actions(&plan, VendorName::Codex), each(Action::Add));
    }

    #[test]
    fn an_exact_match_not_in_the_record_is_adopted() {
        let f = fixture();
        f.sync(CATALOG);
        let synced = f.snapshot();
        fs::remove_dir_all(&f.records).unwrap();
        let plan = f.plan(CATALOG);
        for v in VendorName::ALL {
            assert_eq!(actions(&plan, v), each(Action::Adopt), "{v:?}");
        }
        assert_eq!(plan.check(), Check::Matches);
        apply(&plan).unwrap();
        assert_eq!(f.snapshot(), synced);
    }

    #[test]
    fn a_crash_between_the_write_and_its_record_is_recovered_by_adopting() {
        let f = fixture();
        let plan = f.plan(CATALOG);
        // A directory where the Claude Code record goes: the record cannot be
        // written, as if fl stopped right after the rename.
        let canonical = fs::canonicalize(&f.root).unwrap().join(".mcp.json");
        let blocked = record_file(&f.records, &canonical);
        fs::create_dir_all(&blocked).unwrap();
        let err = apply(&plan).unwrap_err();
        assert!(err.to_string().contains("could not write"), "{err}");
        assert_eq!(
            f.entry(VendorName::Claude, "notes"),
            Some(rendered(VendorName::Claude, CATALOG, "notes"))
        );

        fs::remove_dir(&blocked).unwrap();
        let plan = f.plan(CATALOG);
        assert_eq!(actions(&plan, VendorName::Claude), each(Action::Adopt));
        assert_eq!(actions(&plan, VendorName::Codex), each(Action::Add));
        apply(&plan).unwrap();
        let plan = f.plan(CATALOG);
        for v in VendorName::ALL {
            assert_eq!(actions(&plan, v), each(Action::Unchanged), "{v:?}");
        }
    }

    #[test]
    fn an_entry_fl_did_not_write_and_does_not_want_is_left_untouched() {
        let f = fixture();
        let theirs = "{\n  \"mcpServers\": {\n    \"theirs\": {\"command\": \"theirs-mcp\"}\n  \
                      },\n  \"other\": 1\n}\n";
        f.write(VendorName::Claude, theirs);
        let foreign = "# Mine.\n[mcp_servers.theirs]\ncommand = \"theirs-mcp\"  # keep\n";
        f.write(VendorName::Codex, foreign);
        let kept = |f: &Fixture| {
            (
                f.entry(VendorName::Claude, "theirs"),
                f.entry(VendorName::Codex, "theirs"),
            )
        };
        let before = kept(&f);

        let plan = f.plan(CATALOG);
        assert_eq!(
            actions(&plan, VendorName::Claude)[0],
            ("theirs".into(), Action::Untouched)
        );
        apply(&plan).unwrap();
        assert_eq!(kept(&f), before);
        assert!(f.read(VendorName::Claude).unwrap().contains("\"other\": 1"));

        let plan = f.plan("");
        let expected = vec![
            ("theirs".into(), Action::Untouched),
            ("docs".into(), Action::Remove),
            ("notes".into(), Action::Remove),
        ];
        assert_eq!(actions(&plan, VendorName::Codex), expected);
        apply(&plan).unwrap();
        assert_eq!(kept(&f), before);
        assert_eq!(f.read(VendorName::Codex).unwrap(), foreign);
    }

    #[test]
    fn a_file_changed_between_plan_and_write_is_refused_and_nothing_is_written() {
        let f = fixture();
        f.sync(CATALOG);
        let plan = f.plan(&CATALOG.replace(PINNED, UPGRADED));
        assert!(plan.targets().iter().all(|t| t.changes()));
        // `agy mcp add`, or a person, writes the last target after the plan.
        f.edit(
            VendorName::Antigravity,
            "\"mcpServers\": {",
            "\"mcpServers\": {\"x\": {},",
        );
        let before = f.snapshot();
        let err = apply(&plan).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "{} changed while fl was planning its write: another program or a person \
                 wrote it. Nothing was written; run `fl mcp sync` again",
                f.path(VendorName::Antigravity).display()
            )
        );
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn one_refusal_writes_nothing_in_any_of_the_three_files() {
        let f = fixture();
        let theirs = "[mcp_servers.notes]\ncommand = \"notes-mcp\"\n";
        f.write(VendorName::Codex, theirs);
        let before = f.snapshot();
        let plan = f.plan(CATALOG);
        assert_eq!(actions(&plan, VendorName::Claude), each(Action::Add));
        assert_eq!(actions(&plan, VendorName::Antigravity), each(Action::Add));
        assert!(matches!(
            actions(&plan, VendorName::Codex)[0].1,
            Action::Refused(_)
        ));
        assert!(apply(&plan).is_err());
        assert_eq!(f.snapshot(), before);
        assert_eq!(f.read(VendorName::Claude), None);
        assert_eq!(f.read(VendorName::Antigravity), None);
    }

    #[test]
    fn replace_overwrites_only_the_named_entry() {
        let f = fixture();
        f.sync(CATALOG);
        let docs = "https://docs.example.com/mcp";
        f.edit(VendorName::Claude, PINNED, "@example/notes-mcp@1.2.1");
        f.edit(VendorName::Claude, docs, "https://docs.example.com/v2");
        f.edit(
            VendorName::Claude,
            "{\n    \"docs\"",
            "{\n    \"theirs\": {},\n    \"docs\"",
        );
        let theirs = f.entry(VendorName::Claude, "theirs");

        let plan = f.plan_with(CATALOG, &Switches::default(), &["notes"]);
        let claude = actions(&plan, VendorName::Claude);
        assert_eq!(claude[0], ("theirs".into(), Action::Untouched));
        assert!(refusal(&claude[1].1).contains("`docs` was changed by hand"));
        let difference = "`args` differs".to_string();
        assert_eq!(claude[2], ("notes".into(), Action::Replace { difference }));
        assert_eq!(plan.check(), Check::Refused);

        f.edit(VendorName::Claude, "https://docs.example.com/v2", docs);
        let edited_docs = f.entry(VendorName::Claude, "docs");
        let plan = f.plan_with(CATALOG, &Switches::default(), &["notes"]);
        apply(&plan).unwrap();
        let notes = rendered(VendorName::Claude, CATALOG, "notes");
        assert_eq!(f.entry(VendorName::Claude, "notes"), Some(notes.clone()));
        assert_eq!(f.entry(VendorName::Claude, "docs"), edited_docs);
        assert_eq!(f.entry(VendorName::Claude, "theirs"), theirs);
        let record = f.record(VendorName::Claude).unwrap();
        assert_eq!(record["entries"]["notes"]["sha256"], sha(&notes));
    }

    /// The error of planning `text` with these `--replace` names; the plan
    /// is not printed when there is none.
    fn refused_replace(f: &Fixture, text: &str, names: &[&str]) -> McpError {
        let replace: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        match plan(
            &f.root,
            &catalog(text),
            &Switches::default(),
            &f.records,
            &replace,
        ) {
            Ok(_) => panic!("a `--replace` that matches no refused entry was planned"),
            Err(e) => e,
        }
    }

    // MCP spec §4.3: a `--replace` that names no refused entry would be
    // ignored, and the person would think the entry was overwritten. It is
    // refused before anything is written.
    #[test]
    fn a_replace_that_matches_no_refused_entry_is_refused_and_nothing_is_written() {
        let f = fixture();
        f.sync(CATALOG);
        let docs = "https://docs.example.com/mcp";
        f.edit(VendorName::Claude, docs, "https://docs.example.com/v2");
        let before = f.snapshot();

        // A name nothing has, and the name of an entry that is not refused.
        for name in ["nosuch", "notes"] {
            let err = refused_replace(&f, CATALOG, &[name]);
            assert!(matches!(err, McpError::UnmatchedReplace { .. }), "{err:?}");
            let msg = err.to_string();
            assert!(
                msg.contains(&format!("`--replace` names `{name}`")),
                "{msg}"
            );
            assert!(msg.contains("nothing was written"), "{msg}");
            assert!(msg.contains("Run `fl mcp sync` without it"), "{msg}");
        }
        // A name that matches does not excuse one that does not; only the
        // one that does not is named.
        let msg = refused_replace(&f, CATALOG, &["docs", "nosuch"]).to_string();
        assert!(msg.contains("`nosuch`") && !msg.contains("`docs`"), "{msg}");
        assert_eq!(f.snapshot(), before);
        // The one that matches is replaced.
        let replace = vec!["docs".to_string()];
        let plan = plan(
            &f.root,
            &catalog(CATALOG),
            &Switches::default(),
            &f.records,
            &replace,
        );
        apply(&plan.unwrap()).unwrap();
        assert_eq!(
            f.entry(VendorName::Claude, "docs"),
            Some(rendered(VendorName::Claude, CATALOG, "docs"))
        );
    }

    // A name that is not a server name may be a secret someone pasted: it is
    // counted, never repeated (MCP spec §6).
    #[test]
    fn a_replace_name_that_is_not_a_server_name_is_not_repeated() {
        let f = fixture();
        f.sync(CATALOG);
        let long = "a".repeat(33);
        for token in [
            "ghp_example0token",
            "Sk-Example0Token",
            "a b",
            "",
            long.as_str(),
        ] {
            let msg = refused_replace(&f, CATALOG, &[token, "nosuch"]).to_string();
            assert!(msg.contains("`nosuch`"), "{msg}");
            assert!(msg.contains("a name that is not a server name"), "{msg}");
            assert!(!msg.to_lowercase().contains("example0token"), "{msg}");
            assert!(!msg.contains("a b") && !msg.contains(&long), "{msg}");
        }
        let msg = refused_replace(&f, CATALOG, &["ghp_example0token", "Other_Token"]).to_string();
        assert!(
            msg.contains("`--replace` names 2 names that are not server names"),
            "{msg}"
        );
        assert!(
            !msg.contains("Other_Token") && !msg.contains("ghp_"),
            "{msg}"
        );
    }

    #[test]
    fn a_disabled_server_is_removed_from_every_vendor_file_it_was_in() {
        let f = fixture();
        f.sync(CATALOG);
        let plan = f.plan_with(CATALOG, &off(&["notes"]), &[]);
        for v in VendorName::ALL {
            assert_eq!(
                actions(&plan, v)[1],
                ("notes".into(), Action::Remove),
                "{v:?}"
            );
        }
        apply(&plan).unwrap();
        for v in VendorName::ALL {
            assert_eq!(f.entry(v, "notes"), None, "{v:?}");
            assert!(f.entry(v, "docs").is_some(), "{v:?}");
        }
    }

    #[test]
    fn the_machine_switches_turn_a_server_on_or_off_here() {
        let f = fixture();
        let docs_off = CATALOG.replace(
            "transport = \"http\"",
            "enabled = false\ntransport = \"http\"",
        );
        let names = |plan: &Plan| -> Vec<String> {
            actions(plan, VendorName::Claude)
                .into_iter()
                .map(|(n, _)| n)
                .collect()
        };
        assert_eq!(names(&f.plan(&docs_off)), ["notes"]);
        let on = Switches {
            enable: vec!["docs".into()],
            ..Switches::default()
        };
        assert_eq!(names(&f.plan_with(&docs_off, &on, &[])), ["docs", "notes"]);
        assert_eq!(
            names(&f.plan_with(CATALOG, &off(&["notes"]), &[])),
            ["docs"]
        );
        assert_eq!(names(&f.plan_with(CATALOG, &on, &[])), ["docs", "notes"]);
    }

    // MCP spec §0.1 decision 15: a switch naming a server the catalog no longer has
    // is ignored, with a warning; the rest of the plan goes on.
    #[test]
    fn a_machine_switch_naming_an_unknown_server_is_a_warning() {
        let f = fixture();
        let switches = Switches {
            enable: vec!["sentry".into()],
            disable: vec!["notes".into(), "old".into()],
        };
        let plan = f.plan_with(CATALOG, &switches, &[]);
        assert_eq!(
            plan.warnings(),
            [
                format!(
                    "the `[[mcp]]` entry for this project in your fl config has `sentry` in \
                     `enable`, and {} has no such server; fl ignores it. Remove it from that \
                     entry",
                    f.root.join(".fl/mcp.toml").display()
                ),
                format!(
                    "the `[[mcp]]` entry for this project in your fl config has `old` in \
                     `disable`, and {} has no such server; fl ignores it. Remove it from that \
                     entry",
                    f.root.join(".fl/mcp.toml").display()
                ),
            ]
        );
        let docs_only = vec![("docs".to_string(), Action::Add)];
        assert_eq!(actions(&plan, VendorName::Claude), docs_only);
        assert_eq!(plan.check(), Check::Changes);
        assert!(f.plan(CATALOG).warnings().is_empty());
    }

    // MCP spec §0.1 decision 14: after a fresh clone or `git clean -X`, fl's own
    // files are gone as a whole; `sync` writes them anew from the catalog.
    #[test]
    fn a_vendor_file_gone_as_a_whole_is_written_anew() {
        let f = fixture();
        f.sync(CATALOG);
        for v in VendorName::ALL {
            fs::remove_file(f.path(v)).unwrap();
        }
        let docs_only = CATALOG.split("[server.notes]").next().unwrap();
        let plan = f.plan(docs_only);
        for v in VendorName::ALL {
            let expected = vec![
                ("docs".to_string(), Action::Add),
                ("notes".to_string(), Action::Forget),
            ];
            assert_eq!(actions(&plan, v), expected, "{v:?}");
        }
        assert_eq!(plan.check(), Check::Changes);
        apply(&plan).unwrap();
        for v in VendorName::ALL {
            assert_eq!(f.entry(v, "docs").unwrap(), rendered(v, CATALOG, "docs"));
            let record = f.record(v).unwrap();
            let names: Vec<&String> = record["entries"].as_object().unwrap().keys().collect();
            assert_eq!(names, ["docs"], "{v:?}");
        }
        assert_eq!(f.plan(docs_only).check(), Check::Matches);
    }

    // A formatter, or agy's own panel, may lay fl's entry out anew: the same
    // meaning is adopted, not refused (a changed value still is).
    #[test]
    fn fl_s_entry_laid_out_anew_with_the_same_meaning_is_adopted() {
        let f = fixture();
        f.sync(CATALOG);
        let claude = f.read(VendorName::Claude).unwrap();
        let compact =
            serde_json::to_string(&serde_json::from_str::<serde_json::Value>(&claude).unwrap());
        f.write(VendorName::Claude, &compact.unwrap());
        let codex = f.read(VendorName::Codex).unwrap();
        let args = "args = [\"-y\", \"@example/notes-mcp@1.2.0\"]\n";
        let reordered = codex.replacen(args, "", 1).replacen(
            "[mcp_servers.notes]\n",
            &format!("[mcp_servers.notes]\n{args}"),
            1,
        );
        assert_ne!(reordered, codex);
        f.write(VendorName::Codex, &reordered);
        let plan = f.plan(CATALOG);
        assert_eq!(actions(&plan, VendorName::Claude), each(Action::Adopt));
        assert_eq!(
            actions(&plan, VendorName::Codex),
            vec![
                ("docs".to_string(), Action::Unchanged),
                ("notes".to_string(), Action::Adopt)
            ]
        );
        assert_eq!(plan.check(), Check::Matches);
        assert!(
            plan.to_string()
                .contains("\n  adopt notes (means what fl would write)\n"),
            "{plan}"
        );
        apply(&plan).unwrap();
        assert_eq!(
            f.read(VendorName::Codex).unwrap(),
            reordered,
            "the file is kept"
        );
        let again = f.plan(CATALOG);
        assert_eq!(actions(&again, VendorName::Claude), each(Action::Unchanged));
        assert!(
            again
                .targets()
                .iter()
                .all(|t| t.record.is_none() && !t.changes())
        );
        // A change of meaning in the new layout is still refused.
        f.edit(VendorName::Codex, "@example/notes-mcp@1.2.0", UPGRADED);
        let plan = f.plan(CATALOG);
        assert!(refusal(&actions(&plan, VendorName::Codex)[1].1).contains("`args` differs"));
    }

    // A vendor file that is a link, or lies under a linked directory, could
    // carry fl's write into a file it cannot see whole — one git tracks, or
    // one outside the project: refused, naming the path, and nothing is
    // written. The project root itself may be reached through a link.
    #[test]
    fn a_vendor_file_reached_through_a_link_is_refused() {
        let f = fixture();
        let refused = |root: &Path| {
            let plan = plan(
                root,
                &catalog(CATALOG),
                &Switches::default(),
                &f.records,
                &[],
            );
            plan.unwrap_err().to_string()
        };
        let next = "Replace the link with a plain file or directory, then run `fl mcp sync` again";
        // In the project: a file someone shares, through `.mcp.json`.
        fs::create_dir(f.root.join("docs")).unwrap();
        let shared = f.root.join("docs/shared.json");
        fs::write(&shared, "{\"mcpServers\": {}}\n").unwrap();
        std::os::unix::fs::symlink("docs/shared.json", f.path(VendorName::Claude)).unwrap();
        assert_eq!(
            refused(&f.root),
            format!(
                "{}: it is a symbolic link, and fl writes only plain files it can see whole. \
                 {next}",
                f.path(VendorName::Claude).display()
            )
        );
        fs::remove_file(f.path(VendorName::Claude)).unwrap();
        // A linked directory in the project.
        fs::create_dir_all(f.root.join("gen/agents")).unwrap();
        std::os::unix::fs::symlink("gen/agents", f.root.join(".agents")).unwrap();
        assert_eq!(
            refused(&f.root),
            format!(
                "{}: {} is a symbolic link, and fl writes only plain files it can see whole. \
                 {next}",
                f.path(VendorName::Antigravity).display(),
                f.root.join(".agents").display()
            )
        );
        fs::remove_file(f.root.join(".agents")).unwrap();
        // A linked directory that leads outside.
        let elsewhere = f.dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, f.root.join(".codex")).unwrap();
        assert!(refused(&f.root).contains(&format!(
            "{} is a symbolic link, and fl writes only plain files it can see whole",
            f.root.join(".codex").display()
        )));
        fs::remove_file(f.root.join(".codex")).unwrap();
        assert_eq!(
            fs::read_to_string(&shared).unwrap(),
            "{\"mcpServers\": {}}\n"
        );
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert!(!f.records.exists());
        // The project root itself, through a link: fine.
        let link = f.dir.path().join("app-link");
        std::os::unix::fs::symlink(&f.root, &link).unwrap();
        let plan = plan(
            &link,
            &catalog(CATALOG),
            &Switches::default(),
            &f.records,
            &[],
        );
        apply(&plan.unwrap()).unwrap();
        assert!(f.read(VendorName::Claude).unwrap().contains("notes"));
    }

    #[test]
    fn check_is_0_when_everything_matches_1_with_changes_2_on_a_refusal() {
        let f = fixture();
        let empty = f.plan("");
        assert_eq!(empty.check(), Check::Matches);
        // Nothing to do writes nothing, not even the lock.
        apply(&empty).unwrap();
        assert!(!f.records.exists());
        let plan = f.plan(CATALOG);
        assert_eq!(plan.check(), Check::Changes);
        assert_eq!(plan.check().exit_code(), 1);
        apply(&plan).unwrap();
        let plan = f.plan(CATALOG);
        assert_eq!(plan.check(), Check::Matches);
        assert_eq!(plan.check().exit_code(), 0);
        assert_eq!(
            f.plan(&CATALOG.replace(PINNED, UPGRADED)).check(),
            Check::Changes
        );
        f.edit(VendorName::Codex, PINNED, "@example/notes-mcp@1.2.1");
        let plan = f.plan(CATALOG);
        assert_eq!(plan.check(), Check::Refused);
        assert_eq!(plan.check().exit_code(), 2);
    }

    #[test]
    fn the_plan_prints_each_target_its_actions_and_each_refusal() {
        let f = fixture();
        f.sync(CATALOG);
        f.edit(VendorName::Codex, PINNED, "@example/notes-mcp@1.2.1");
        let next = CATALOG
            .replace(PINNED, UPGRADED)
            .replace("[server.docs]", "[server.gone]")
            + "\n[server.events]\ntransport = \"sse\"\nurl = \"https://events.example.com/sse\"\n";
        let plan = f.plan(&next);
        let path = |v| f.path(v).display().to_string();
        let expected = format!(
            "Claude Code, {claude}:\n  remove docs\n  update notes\n  add events\n  add gone\n\
             Codex, {codex}:\n  remove docs\n  refused notes\n  add gone\n\
             Antigravity, {agy}:\n  remove docs\n  update notes\n  add gone\n\
             Codex cannot run server `events`: Codex connects to streamable HTTP servers only, \
             not SSE. It is left out of .codex/config.toml; the other vendors still get it. To \
             say so in the catalog, give the server a `vendors` list without `codex`\n\
             Antigravity cannot run server `events`: Antigravity does not support the legacy \
             SSE transport; only a streamable HTTP endpoint or a stdio server. It is left out \
             of .agents/mcp_config.json; the other vendors still get it. To say so in the \
             catalog, give the server a `vendors` list without `antigravity`\n\
             {codex}: `notes` was changed by hand since fl wrote it (`args` differs). Restore \
             the entry, or run `fl mcp sync --replace notes`, which shows the difference and \
             overwrites it",
            claude = path(VendorName::Claude),
            codex = path(VendorName::Codex),
            agy = path(VendorName::Antigravity),
        );
        assert_eq!(plan.to_string(), expected);
        assert_eq!(plan.check(), Check::Refused);
        f.edit(VendorName::Codex, "@example/notes-mcp@1.2.1", PINNED);
        let plan = f.plan(CATALOG);
        let expected = format!(
            "Claude Code, {}: no change\nCodex, {}: no change\nAntigravity, {}: no change",
            path(VendorName::Claude),
            path(VendorName::Codex),
            path(VendorName::Antigravity)
        );
        assert_eq!(plan.to_string(), expected);
    }

    #[test]
    fn a_vendor_that_cannot_run_a_server_skips_it_and_the_others_still_get_it() {
        let f = fixture();
        let text = r#"
[server.events]
transport = "sse"
url = "https://events.example.com/sse"

[server.local]
vendors = ["codex"]
transport = "stdio"
command = "local-mcp"
"#;
        let plan = f.plan(text);
        let add = |name: &str| vec![(name.to_string(), Action::Add)];
        assert_eq!(actions(&plan, VendorName::Claude), add("events"));
        assert_eq!(actions(&plan, VendorName::Codex), add("local"));
        assert_eq!(actions(&plan, VendorName::Antigravity), vec![]);
        let skipped: Vec<_> = plan
            .skipped()
            .iter()
            .map(|r| (r.vendor, r.server.as_str()))
            .collect();
        assert_eq!(
            skipped,
            [
                (VendorName::Codex, "events"),
                (VendorName::Antigravity, "events")
            ]
        );
        assert_eq!(plan.check(), Check::Changes);
        apply(&plan).unwrap();
        assert_eq!(f.read(VendorName::Antigravity), None);
        // A record for Claude Code and Codex, none for an untouched target.
        assert_eq!(fs::read_dir(&f.records).unwrap().count(), 3);
        assert_eq!(f.plan(text).check(), Check::Matches);
    }

    #[test]
    fn a_record_holds_each_entry_s_name_root_and_hash_only() {
        let f = fixture();
        f.sync(CATALOG);
        let root = fs::canonicalize(&f.root).unwrap();
        let target = root.join(".mcp.json");
        let hash = |name| sha(&rendered(VendorName::Claude, CATALOG, name));
        assert_eq!(
            f.record(VendorName::Claude).unwrap(),
            serde_json::json!({
                "target": target,
                "entries": {
                    "docs": { "root": root, "sha256": hash("docs") },
                    "notes": { "root": root, "sha256": hash("notes") },
                }
            })
        );
        let mut names: Vec<_> = fs::read_dir(&f.records)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names.len(), 4, "{names:?}");
        assert_eq!(names.last().unwrap(), LOCK);
    }

    #[cfg(unix)]
    #[test]
    fn the_record_is_keyed_by_the_canonical_path() {
        let f = fixture();
        f.sync(CATALOG);
        let link = f.dir.path().join("link");
        std::os::unix::fs::symlink(&f.root, &link).unwrap();
        let through = |text: &str| {
            plan(&link, &catalog(text), &Switches::default(), &f.records, &[]).unwrap()
        };
        let same = through(CATALOG);
        for v in VendorName::ALL {
            assert_eq!(actions(&same, v), each(Action::Unchanged), "{v:?}");
        }
        apply(&through(&CATALOG.replace(PINNED, UPGRADED))).unwrap();
        let root = fs::canonicalize(&f.root).unwrap();
        let record = f.record(VendorName::Claude).unwrap();
        assert_eq!(record["entries"]["notes"]["root"], root.to_str().unwrap());
    }

    #[test]
    fn a_record_fl_cannot_read_is_refused_naming_the_remedy() {
        let f = fixture();
        f.sync(CATALOG);
        let canonical = fs::canonicalize(f.path(VendorName::Codex)).unwrap();
        let file = record_file(&f.records, &canonical);
        let unknown = r#"{"target": "x", "entries": {}, "project": "app"}"#;
        let inner = r#"{"target": "x", "entries": {"a": {"root": "r", "sha256": "0", "v": 1}}}"#;
        for text in ["{", unknown, inner] {
            fs::write(&file, text).unwrap();
            let err = plan(
                &f.root,
                &catalog(CATALOG),
                &Switches::default(),
                &f.records,
                &[],
            );
            let err = err.unwrap_err().to_string();
            let head = format!(
                "{} is not an ownership record fl can read: ",
                file.display()
            );
            assert!(err.starts_with(&head), "{err}");
            let remedy = "Delete it; the next `fl mcp sync` adopts every entry that still \
                          matches the catalog";
            assert!(err.ends_with(remedy), "{err}");
        }
    }

    #[test]
    fn a_vendor_file_fl_cannot_read_stops_the_plan() {
        let f = fixture();
        f.write(VendorName::Claude, "{ // mine\n}\n");
        let err = plan(
            &f.root,
            &catalog(CATALOG),
            &Switches::default(),
            &f.records,
            &[],
        );
        let err = err.unwrap_err();
        assert!(matches!(err, McpError::VendorFile(_)), "{err:?}");
        assert!(err.to_string().contains("it is not strict JSON ("), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_original_file_mode_is_kept_and_a_new_file_gets_the_default_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let f = fixture();
        f.sync(CATALOG);
        let plain = f.dir.path().join("plain");
        fs::File::create(&plain).unwrap();
        for v in VendorName::ALL {
            assert_eq!(mode(&f.path(v)), mode(&plain), "{v:?}");
        }
        // A mode the umask would change, so only a copy keeps it.
        let claude = f.path(VendorName::Claude);
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o646)).unwrap();
        f.sync(&CATALOG.replace(PINNED, UPGRADED));
        assert_eq!(mode(&claude), 0o646);
    }

    #[test]
    fn a_second_sync_waits_while_another_holds_the_lock() {
        let f = fixture();
        let plan = f.plan(CATALOG);
        fs::create_dir_all(&f.records).unwrap();
        let held = fs::File::create(f.records.join(LOCK)).unwrap();
        held.lock().unwrap();
        let waiting = std::thread::spawn(move || apply(&plan));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!waiting.is_finished());
        assert_eq!(f.read(VendorName::Claude), None);
        drop(held);
        waiting.join().unwrap().unwrap();
        assert!(f.read(VendorName::Claude).is_some());
    }
}
