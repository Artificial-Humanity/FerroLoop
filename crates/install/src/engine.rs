//! The install transaction (#205 A4a): lock, journal, stage, verify, publish, switch `current`,
//! write the launcher and aliases, write the receipt LAST, commit; and the recovery that
//! finishes or undoes an interrupted one.
//!
//! The rules the code keeps:
//!
//! * A journal line naming a step is written and flushed BEFORE the step runs.
//! * Every step is idempotent given the intent in the journal's first line, so a step that
//!   crashed half-way can simply run again, and its undo only removes what the journal says
//!   this transaction created and that is still exactly what it created.
//! * Nothing is activated before every artifact hash has been checked; the receipt is replaced
//!   atomically and last; a crash before the receipt leaves the previous receipt, and the next
//!   run finishes or undoes the transaction before it does anything else.
//! * Mutable data (stores, config, keys, logs) is never inside a release and never touched.

use crate::error::{Ctx, InstallError, Result};
use crate::fault::{FaultPoint, Faults};
use crate::fsprobe::{FsProbe, read_bounded};
use crate::journal::{self, Found, Journal, Pending, PendingState, sync_dir};
use crate::lock::{self, LockGuard};
use crate::safe::Safe;
use crate::services::{self, ServiceManager};
use crate::tree::{self, ScannedFile};
use fl_core::install::{
    Artifact, BackupSpec, Disposition, FileKind, HostInfo, HostProbe, InstallPlan, InstallReceipt,
    InstallRequest, IntentFile, JournalOp, Layout, LinkSpec, MAX_RECEIPT_BYTES, Origin,
    OwnedArtifact, OwnedPathKind, PathRole, PathState, PlannedPath, RELEASE_JSON, ReceiptLayout,
    ReceiptRelease, ReleaseKind, ReleaseManifest, Retention, Step, TxnIntent, build_receipt,
    has_committed, next_txn, plan, release_json, retention, sha256_hex,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long an installer waits for `install.lock` by default.
pub const DEFAULT_LOCK_TIMEOUT_SECS: u64 = 30;

/// What the installer runs against.
pub struct Env {
    /// What a leading `~/` in the layout expands to.
    pub home: Option<PathBuf>,
    /// Where this install may write: `--prefix`, `$FL_INSTALL_ROOT`, or `$HOME`.
    pub declared_root: PathBuf,
    pub lock_timeout: Duration,
    pub faults: Faults,
    /// Used to PLAN service actions only; nothing is ever executed through it.
    pub services: Option<Box<dyn ServiceManager>>,
}

impl Env {
    pub fn new(declared_root: PathBuf, home: Option<PathBuf>) -> Self {
        Self {
            home,
            declared_root,
            lock_timeout: Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECS),
            faults: Faults::none(),
            services: None,
        }
    }
}

/// The user's per-path decisions on conflicts.
#[derive(Debug, Clone, Default)]
pub struct Decisions {
    /// Move the existing thing to `backups/<n>/` and install over the empty slot.
    pub replace: Vec<String>,
    /// Leave the existing thing alone; the install does not own it and skips that slot.
    pub keep: Vec<String>,
}

pub struct ApplyArgs<'a> {
    pub request: &'a InstallRequest,
    pub manifest: &'a ReleaseManifest,
    pub host: HostInfo,
    /// The directory holding the extracted artifacts the manifest names.
    pub artifact_dir: Option<&'a Path>,
    pub decisions: &'a Decisions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Installed,
    AlreadyInstalled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoverAction {
    /// There was no journal.
    NothingToDo,
    /// The interrupted transaction was finished.
    Completed,
    /// The interrupted transaction was undone.
    RolledBack,
    /// Only a leftover journal of a finished transaction was cleaned up.
    Cleaned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoverReport {
    pub action: RecoverAction,
    pub txn: Option<u64>,
    pub release: Option<String>,
    pub detail: String,
}

impl RecoverReport {
    fn nothing() -> Self {
        Self {
            action: RecoverAction::NothingToDo,
            txn: None,
            release: None,
            detail: "no interrupted install found".into(),
        }
    }

    pub fn render_text(&self) -> String {
        match self.action {
            RecoverAction::NothingToDo => {
                "nothing to resume: no interrupted install found\n".into()
            }
            _ => format!(
                "interrupted install {}: {}\n",
                self.txn
                    .map(|t| format!("(transaction {t})"))
                    .unwrap_or_default(),
                self.detail
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub outcome: Outcome,
    pub release: String,
    pub txn: Option<u64>,
    pub steps: Vec<Step>,
    pub recovered: Option<RecoverReport>,
    /// `(from, to)` of every path `--replace` moved aside.
    pub backups: Vec<(String, String)>,
    pub kept: Vec<String>,
    pub not_executed: Vec<String>,
    pub retention: Option<Retention>,
    pub notes: Vec<String>,
}

impl ApplyReport {
    pub fn render_text(&self) -> String {
        let mut o = String::new();
        let mut line = |s: String| {
            o.push_str(&s);
            o.push('\n');
        };
        if let Some(r) = &self.recovered {
            line(r.render_text().trim_end().to_string());
        }
        match self.outcome {
            Outcome::AlreadyInstalled => line(format!(
                "already installed: release {} matches the receipt; nothing was changed",
                self.release
            )),
            Outcome::Installed => {
                line(format!(
                    "installed release {} (transaction {})",
                    self.release,
                    self.txn.unwrap_or(0)
                ));
                line(format!(
                    "steps: {}",
                    self.steps
                        .iter()
                        .map(|s| s.as_wire())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        for (from, to) in &self.backups {
            line(format!("backed up {from} -> {to}"));
        }
        for k in &self.kept {
            line(format!("kept {k} (not owned, not changed)"));
        }
        if let Some(r) = &self.retention {
            line(format!("retention: keeps {}", r.keep.join(", ")));
            if !r.gc_eligible.is_empty() {
                line(format!(
                    "older releases a later --gc could clear (nothing is cleared in this \
                     release): {}",
                    r.gc_eligible.join(", ")
                ));
            }
        }
        for n in &self.notes {
            line(format!("note: {n}"));
        }
        if !self.not_executed.is_empty() {
            line("a plain install performs none of these separate actions:".into());
            for n in &self.not_executed {
                line(format!("  {n}"));
            }
        }
        o
    }

    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("report serializes");
        s.push('\n');
        s
    }
}

// ------------------------------------------------------------------ paths

struct Paths {
    prefix: PathBuf,
    releases: PathBuf,
    current: PathBuf,
    receipt: PathBuf,
    journal: PathBuf,
    lock: PathBuf,
    backups: PathBuf,
    bin_dir: PathBuf,
}

impl Paths {
    fn new(safe: &Safe, layout: &Layout) -> Result<Self> {
        let prefix = safe.resolve(&layout.prefix)?;
        let bin_dir = safe.resolve(&layout.bin_dir)?;
        Ok(Self {
            releases: prefix.join("releases"),
            current: prefix.join("current"),
            receipt: prefix.join("receipt.json"),
            journal: prefix.join("journal.jsonl"),
            lock: prefix.join("install.lock"),
            backups: prefix.join("backups"),
            prefix,
            bin_dir,
        })
    }

    fn release(&self, name: &str) -> PathBuf {
        self.releases.join(name)
    }

    fn stage(&self, txn: u64) -> PathBuf {
        self.releases.join(format!(".stage-{txn}"))
    }

    fn tmp(&self, what: &str, txn: u64) -> PathBuf {
        self.prefix.join(format!(".{what}.tmp-{txn}"))
    }
}

fn shown_join(base: &str, rel: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), rel)
}

fn lstat(p: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(p) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(InstallError::io(
            format!("cannot inspect {}", p.display()),
            e,
        )),
    }
}

fn read_link_string(p: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => Ok(Some(
            fs::read_link(p)
                .ctx(|| format!("cannot read link {}", p.display()))?
                .to_string_lossy()
                .into_owned(),
        )),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(InstallError::io(
            format!("cannot inspect {}", p.display()),
            e,
        )),
    }
}

enum Expect<'a> {
    /// The link may be absent, already `target`, or still point where it did at planning.
    Prev(Option<&'a str>),
    /// An undo: any symlink is replaced.
    Any,
}

/// Point the symlink `path` at `target` with a temp symlink and `rename(2)`. Refuses to
/// replace anything that is not the symlink it expected. `Ok(false)`: already right.
fn set_symlink(path: &Path, target: &str, tmp: &Path, expect: Expect<'_>) -> Result<bool> {
    match lstat(path)? {
        Some(m) if m.file_type().is_symlink() => {
            let t = read_link_string(path)?.unwrap_or_default();
            if t == target {
                return Ok(false);
            }
            let ok = match expect {
                Expect::Any => true,
                Expect::Prev(Some(p)) => p == t,
                Expect::Prev(None) => false,
            };
            if !ok {
                return Err(InstallError::refused(format!(
                    "{} changed since the plan (it now points to {t}); it is not replaced",
                    path.display()
                )));
            }
        }
        Some(_) => {
            return Err(InstallError::refused(format!(
                "{} is not a symlink any more; it is not replaced",
                path.display()
            )));
        }
        None => {}
    }
    if lstat(tmp)?.is_some() {
        fs::remove_file(tmp).ctx(|| format!("cannot remove {}", tmp.display()))?;
    }
    symlink(target, tmp).ctx(|| format!("cannot create {}", tmp.display()))?;
    fs::rename(tmp, path).ctx(|| format!("cannot switch {}", path.display()))?;
    if let Some(d) = path.parent() {
        sync_dir(d)?;
    }
    Ok(true)
}

/// `rename(2)`, for a path that may be a sealed (read-only) directory. Moving a directory to
/// another parent needs write permission on the directory itself, so its mode is opened for the
/// move and put back on the destination.
fn rename_keeping_mode(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(from)?;
    if !meta.is_dir() || meta.permissions().mode() & 0o200 != 0 {
        return fs::rename(from, to);
    }
    let mode = meta.permissions().mode();
    fs::set_permissions(from, fs::Permissions::from_mode(mode | 0o200))?;
    if let Err(e) = fs::rename(from, to) {
        let _ = fs::set_permissions(from, fs::Permissions::from_mode(mode));
        return Err(e);
    }
    fs::set_permissions(to, fs::Permissions::from_mode(mode))
}

/// Write `bytes` to `path` atomically: temp file, flush, `rename(2)`, flush the directory.
fn write_atomic(path: &Path, tmp: &Path, bytes: &[u8]) -> Result<()> {
    let _ = fs::remove_file(tmp);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(tmp)
        .ctx(|| format!("cannot create {}", tmp.display()))?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .ctx(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(tmp, path).ctx(|| format!("cannot replace {}", path.display()))?;
    if let Some(d) = path.parent() {
        sync_dir(d)?;
    }
    Ok(())
}

fn read_receipt_text(path: &Path) -> Result<Option<String>> {
    read_bounded(path, MAX_RECEIPT_BYTES).map_err(|e| {
        InstallError::refused(format!(
            "install receipt {} cannot be read ({e}); it is not treated as unmanaged",
            path.display()
        ))
    })
}

fn parse_receipt(text: &str, shown: &str) -> Result<InstallReceipt> {
    InstallReceipt::parse(text).map_err(|e| {
        InstallError::refused(format!(
            "install receipt {shown} is invalid ({e}); it is not treated as unmanaged"
        ))
    })
}

// ------------------------------------------------------------ source tree

struct SrcFile {
    abs: PathBuf,
    sha256: String,
    size: u64,
    exec: bool,
}

/// The verified files the release will hold, by path inside the release.
struct Source {
    files: BTreeMap<String, SrcFile>,
}

fn artifact_for(
    manifest: &ReleaseManifest,
    host: HostInfo,
    product: fl_core::install::Product,
) -> Option<&Artifact> {
    manifest
        .artifacts
        .iter()
        .find(|a| a.target == host.target() && a.provides.iter().any(|s| s == product.as_wire()))
}

/// `base/rel`, refusing a symlink among the components below `base`.
fn join_no_symlinks(base: &Path, rel: &str) -> Result<PathBuf> {
    let mut cur = base.to_path_buf();
    for seg in rel.split('/') {
        cur.push(seg);
        if let Some(m) = lstat(&cur)?
            && m.file_type().is_symlink()
        {
            return Err(InstallError::refused(format!(
                "artifact path {} is a symlink; the manifest declares none",
                cur.display()
            )));
        }
    }
    Ok(cur)
}

/// Check every artifact the plan needs against the manifest's size and hash, and collect the
/// files of the selected products. Reads only. Nothing is activated before this passes.
fn prepare_source(args: &ApplyArgs<'_>, plan: &InstallPlan) -> Result<Source> {
    let dir = args.artifact_dir.ok_or_else(|| {
        InstallError::Usage(
            "installing needs --artifact-dir (a local directory of extracted artifacts)".into(),
        )
    })?;
    let dir = fs::canonicalize(dir)
        .ctx(|| format!("artifact directory {} cannot be resolved", dir.display()))?;
    let mut scanned: BTreeMap<String, (PathBuf, tree::Scanned)> = BTreeMap::new();
    for pa in &plan.artifacts {
        let art = args
            .manifest
            .artifacts
            .iter()
            .find(|a| a.id == pa.id)
            .ok_or_else(|| {
                InstallError::refused(format!("artifact `{}` is not in the manifest", pa.id))
            })?;
        let root = join_no_symlinks(&dir, &art.file)?;
        let s = tree::scan_tree(&root, art.size)?;
        if s.total_bytes != art.size {
            return Err(InstallError::refused(format!(
                "artifact `{}` holds {} bytes; the manifest says {}",
                art.id, s.total_bytes, art.size
            )));
        }
        if s.digest != art.sha256 {
            return Err(InstallError::refused(format!(
                "artifact `{}` hash mismatch: its tree digest is {}, the manifest says {}; \
                 nothing was activated",
                art.id, s.digest, art.sha256
            )));
        }
        scanned.insert(art.id.clone(), (root, s));
    }
    let mut files: BTreeMap<String, SrcFile> = BTreeMap::new();
    for sp in &plan.products {
        let entry = args
            .manifest
            .product(sp.id)
            .ok_or_else(|| InstallError::refused(format!("manifest lacks product {}", sp.id)))?;
        let art = artifact_for(args.manifest, args.host, sp.id).ok_or_else(|| {
            InstallError::refused(format!("the manifest has no artifact for {}", sp.id))
        })?;
        let (root, s) = &scanned[&art.id];
        for f in &entry.files {
            if f.path == RELEASE_JSON || f.path.starts_with(&format!("{RELEASE_JSON}/")) {
                return Err(InstallError::refused(format!(
                    "the manifest declares `{}`, which the installer writes itself",
                    f.path
                )));
            }
            let wanted: Vec<&ScannedFile> = match f.kind {
                FileKind::Binary | FileKind::UnitTemplate => {
                    s.files.get(&f.path).into_iter().collect()
                }
                FileKind::AssetDir => {
                    let pre = format!("{}/", f.path);
                    s.files
                        .values()
                        .filter(|x| x.rel.starts_with(&pre))
                        .collect()
                }
            };
            if wanted.is_empty() {
                return Err(InstallError::refused(format!(
                    "artifact `{}` lacks `{}` ({})",
                    art.id, f.path, sp.id
                )));
            }
            for sf in wanted {
                let new = SrcFile {
                    abs: root.join(&sf.rel),
                    sha256: sf.sha256.clone(),
                    size: sf.size,
                    exec: f.kind == FileKind::Binary,
                };
                if let Some(old) = files.get(&sf.rel) {
                    if old.sha256 != new.sha256 {
                        return Err(InstallError::refused(format!(
                            "two artifacts carry different `{}`",
                            sf.rel
                        )));
                    }
                    continue;
                }
                files.insert(sf.rel.clone(), new);
            }
        }
    }
    Ok(Source { files })
}

// -------------------------------------------------------------- the plan

struct Planned {
    plan: InstallPlan,
    receipt: Option<InstallReceipt>,
    receipt_text: Option<String>,
}

fn make_plan(env: &Env, paths: &Paths, args: &ApplyArgs<'_>, probe: &FsProbe) -> Result<Planned> {
    let receipt_text = read_receipt_text(&paths.receipt)?;
    let receipt = receipt_text
        .as_deref()
        .map(|t| parse_receipt(t, &paths.receipt.display().to_string()))
        .transpose()?;
    let _ = env;
    let p = plan(
        args.request,
        args.manifest,
        receipt.as_ref(),
        args.host,
        probe,
    )?;
    Ok(Planned {
        plan: p,
        receipt,
        receipt_text,
    })
}

/// A conflict the user must decide, and the path the decision names.
struct Conflict {
    /// The path the conflict is at, as the plan shows it.
    shown: String,
    /// The path a decision must name (the release directory for a file inside one).
    decide: String,
    role: PathRole,
    text: String,
}

fn conflicts_needing_decision(plan: &InstallPlan) -> Vec<Conflict> {
    let unit_paths: BTreeSet<&str> = plan
        .actions
        .iter()
        .filter_map(|a| a.path.as_deref())
        .collect();
    let release_dir = plan
        .paths
        .iter()
        .find(|p| p.role == PathRole::Release)
        .map(|p| p.path.clone())
        .unwrap_or_default();
    plan.conflicts
        .iter()
        // A unit file is never written in this release: its conflict is information, and
        // the planned action is listed as not executed.
        .filter(|c| !unit_paths.contains(c.path.as_str()))
        .map(|c| {
            let role = plan
                .paths
                .iter()
                .find(|p| p.path == c.path)
                .map(|p| p.role)
                .unwrap_or(PathRole::ReleaseFile);
            let decide = if role == PathRole::ReleaseFile {
                release_dir.clone()
            } else {
                c.path.clone()
            };
            Conflict {
                shown: c.path.clone(),
                decide,
                role,
                text: format!("{}: {}", c.kind.as_wire(), c.detail),
            }
        })
        .collect()
}

struct Resolved {
    /// Shown paths to move aside.
    replace: Vec<String>,
    /// Shown paths left alone.
    keep: Vec<String>,
}

fn resolve_decisions(plan: &InstallPlan, d: &Decisions, safe: &Safe) -> Result<Resolved> {
    let conflicts = conflicts_needing_decision(plan);
    let key = |s: &str| safe.lexical(s).map(|p| p.to_string_lossy().into_owned());
    let mut replace: Vec<String> = Vec::new();
    let mut keep: Vec<String> = Vec::new();
    let mut named: BTreeSet<String> = BTreeSet::new();
    for r in &d.replace {
        if d.keep.iter().any(|k| key(k).ok() == key(r).ok()) {
            return Err(InstallError::Usage(format!(
                "`{r}` is named by both --replace and --keep"
            )));
        }
    }
    let mut unresolved: Vec<&Conflict> = Vec::new();
    for c in &conflicts {
        let want = key(&c.decide)?;
        let replace_hit = d
            .replace
            .iter()
            .any(|r| key(r).ok().as_deref() == Some(&want));
        let keep_hit = d.keep.iter().any(|k| key(k).ok().as_deref() == Some(&want));
        if replace_hit {
            named.insert(want.clone());
            match c.role {
                PathRole::Launcher
                | PathRole::Alias
                | PathRole::Current
                | PathRole::Release
                | PathRole::ReleaseFile => {
                    if !replace.contains(&c.decide) {
                        replace.push(c.decide.clone());
                    }
                }
                _ => {
                    return Err(InstallError::refused(format!(
                        "{} cannot be replaced: the install cannot work without it as a \
                         directory; choose another --prefix",
                        c.shown
                    )));
                }
            }
        } else if keep_hit {
            named.insert(want);
            match c.role {
                PathRole::Launcher | PathRole::Alias => keep.push(c.decide.clone()),
                _ => {
                    return Err(InstallError::refused(format!(
                        "{} cannot be kept: the install needs it; use --replace to move it \
                         aside",
                        c.shown
                    )));
                }
            }
        } else {
            unresolved.push(c);
        }
    }
    if !unresolved.is_empty() {
        let mut msg = format!(
            "{} conflict(s) need an explicit decision; nothing was changed:\n",
            unresolved.len()
        );
        for c in unresolved {
            msg.push_str(&format!(
                "  {}\n    --replace {} (moves it to backups/ under the prefix)  or  --keep {}\n",
                c.text, c.decide, c.decide
            ));
        }
        return Err(InstallError::Refused(msg.trim_end().to_string()));
    }
    for given in d.replace.iter().chain(d.keep.iter()) {
        let k = key(given)?;
        if !named.contains(&k) {
            return Err(InstallError::Usage(format!(
                "`{given}` is not a conflict of this plan, so there is nothing to decide"
            )));
        }
    }
    Ok(Resolved { replace, keep })
}

// -------------------------------------------------------------- the run

struct Run<'a> {
    env: &'a Env,
    safe: &'a Safe,
    paths: &'a Paths,
    intent: &'a TxnIntent,
    txn: u64,
}

impl Run<'_> {
    fn release_path(&self) -> PathBuf {
        self.paths.release(&self.intent.release.name)
    }

    fn do_backup(&self, i: usize) -> Result<()> {
        let b = &self.intent.backups[i];
        let from = self.safe.resolve(&b.path)?;
        let to = self.safe.resolve(&b.to)?;
        if lstat(&to)?.is_some() {
            if lstat(&from)?.is_none() {
                return Ok(()); // done already
            }
            return Err(InstallError::refused(format!(
                "backup target {} already exists",
                to.display()
            )));
        }
        if lstat(&from)?.is_none() {
            return Err(InstallError::refused(format!(
                "{} disappeared before it was backed up",
                from.display()
            )));
        }
        self.safe
            .ensure_dir(to.parent().expect("backup has a parent"))?;
        rename_keeping_mode(&from, &to).map_err(|e| {
            InstallError::io(
                format!(
                    "cannot move {} to {} (a backup stays on one file system)",
                    from.display(),
                    to.display()
                ),
                e,
            )
        })?;
        sync_dir(to.parent().expect("backup has a parent"))?;
        if let Some(p) = from.parent() {
            sync_dir(p)?;
        }
        Ok(())
    }

    fn do_stage(&self, source: &Source) -> Result<()> {
        let stage = self.paths.stage(self.txn);
        fs::create_dir(&stage).ctx(|| format!("cannot create {}", stage.display()))?;
        let mut made: BTreeSet<PathBuf> = BTreeSet::new();
        made.insert(stage.clone());
        for (i, f) in self.intent.files.iter().enumerate() {
            self.env
                .faults
                .stage_file(i)
                .ctx(|| format!("cannot write {}", stage.join(&f.path).display()))?;
            let dest = stage.join(&f.path);
            let mut ancestors: Vec<PathBuf> = Vec::new();
            let mut cur = dest.parent().map(Path::to_path_buf);
            while let Some(d) = cur {
                if d == stage {
                    break;
                }
                ancestors.push(d.clone());
                cur = d.parent().map(Path::to_path_buf);
            }
            for d in ancestors.into_iter().rev() {
                if made.insert(d.clone()) {
                    fs::create_dir(&d).ctx(|| format!("cannot create {}", d.display()))?;
                }
            }
            if f.path == RELEASE_JSON {
                let text = release_json(&self.intent.release, &self.intent.artifacts);
                tree::write_new(&dest, text.as_bytes())?;
            } else {
                let src = source.files.get(&f.path).ok_or_else(|| {
                    InstallError::refused(format!("`{}` has no verified source", f.path))
                })?;
                tree::copy_hashed(&src.abs, &dest, &f.sha256, f.size)?;
            }
        }
        for d in &made {
            sync_dir(d)?;
        }
        sync_dir(&self.paths.releases)
    }

    fn do_verify(&self) -> Result<()> {
        let stage = self.paths.stage(self.txn);
        let release = self.release_path();
        let (dir, sealed) = if lstat(&stage)?.is_some() {
            (stage, false)
        } else if lstat(&release)?.is_some() {
            (release, true)
        } else {
            return Err(InstallError::refused(
                "neither the staged nor the published release exists",
            ));
        };
        let found = tree::read_staged(&dir)?;
        let want: BTreeMap<&str, (&str, u64)> = self
            .intent
            .files
            .iter()
            .map(|f| (f.path.as_str(), (f.sha256.as_str(), f.size)))
            .collect();
        for (p, (sha, size)) in &found {
            match want.get(p.as_str()) {
                None => {
                    return Err(InstallError::refused(format!(
                        "staged tree holds `{p}`, which the release does not declare"
                    )));
                }
                Some((w, s)) if *w != sha || *s != *size => {
                    return Err(InstallError::refused(format!(
                        "staged `{p}` does not match its verified hash"
                    )));
                }
                Some(_) => {}
            }
        }
        for p in want.keys() {
            if !found.contains_key(*p) {
                return Err(InstallError::refused(format!("staged tree lacks `{p}`")));
            }
        }
        if !sealed {
            let exec: BTreeSet<&str> = self
                .intent
                .files
                .iter()
                .filter(|f| f.exec)
                .map(|f| f.path.as_str())
                .collect();
            tree::seal(&dir, &|rel| exec.contains(rel))?;
        }
        Ok(())
    }

    fn do_publish(&self) -> Result<()> {
        let stage = self.paths.stage(self.txn);
        let release = self.release_path();
        match (lstat(&stage)?.is_some(), lstat(&release)?.is_some()) {
            (true, false) => {
                fs::rename(&stage, &release)
                    .ctx(|| format!("cannot publish {}", release.display()))?;
                sync_dir(&self.paths.releases)
            }
            (false, true) => Ok(()),
            (true, true) => Err(InstallError::refused(format!(
                "{} already exists; the staged release is not published over it",
                release.display()
            ))),
            (false, false) => Err(InstallError::refused("the staged release is gone")),
        }
    }

    fn do_link_current(&self) -> Result<()> {
        let target = format!("releases/{}", self.intent.release.name);
        set_symlink(
            &self.paths.current,
            &target,
            &self.paths.tmp("current", self.txn),
            Expect::Prev(self.intent.prev_current.as_deref()),
        )
        .map(|_| ())
    }

    fn do_link(&self, spec: &LinkSpec) -> Result<()> {
        if !spec.write {
            return Ok(());
        }
        let real = self.safe.resolve(&spec.path)?;
        let dir = real.parent().expect("link has a parent");
        set_symlink(
            &real,
            &spec.target,
            &dir.join(format!(".fl-tmp-{}-{}", self.txn, spec.name)),
            Expect::Prev(spec.prev.as_deref()),
        )
        .map(|_| ())
    }

    fn do_write_receipt(&self) -> Result<()> {
        if let Some(cur) = read_receipt_text(&self.paths.receipt)?
            && let Ok(r) = InstallReceipt::parse(&cur)
            && has_committed(&r, self.txn)
        {
            return Ok(());
        }
        let prev = self
            .intent
            .prev_receipt
            .as_deref()
            .map(|t| parse_receipt(t, "(previous)"))
            .transpose()?;
        let new = build_receipt(prev.as_ref(), self.intent, self.txn)
            .map_err(|e| InstallError::refused(format!("the receipt would not be valid: {e}")))?;
        write_atomic(
            &self.paths.receipt,
            &self.paths.tmp("receipt", self.txn),
            new.to_json().as_bytes(),
        )
    }

    // ------------------------------------------------------------- undo

    fn undo_receipt(&self) -> Result<()> {
        let tmp = self.paths.tmp("receipt", self.txn);
        let _ = fs::remove_file(&tmp);
        let Some(cur) = read_receipt_text(&self.paths.receipt)? else {
            return Ok(());
        };
        let ours = InstallReceipt::parse(&cur)
            .map(|r| has_committed(&r, self.txn))
            .unwrap_or(false);
        if !ours {
            return Ok(());
        }
        match &self.intent.prev_receipt {
            Some(prev) => write_atomic(&self.paths.receipt, &tmp, prev.as_bytes()),
            None => {
                fs::remove_file(&self.paths.receipt)
                    .ctx(|| format!("cannot remove {}", self.paths.receipt.display()))?;
                sync_dir(&self.paths.prefix)
            }
        }
    }

    fn undo_link(&self, spec: &LinkSpec) -> Result<()> {
        if !spec.write {
            return Ok(());
        }
        let real = self.safe.resolve(&spec.path)?;
        let dir = real.parent().expect("link has a parent");
        let _ = fs::remove_file(dir.join(format!(".fl-tmp-{}-{}", self.txn, spec.name)));
        if read_link_string(&real)?.as_deref() != Some(spec.target.as_str()) {
            return Ok(()); // not what we placed: not ours to remove
        }
        match &spec.prev {
            Some(prev) => {
                set_symlink(
                    &real,
                    prev,
                    &dir.join(format!(".fl-tmp-{}-{}", self.txn, spec.name)),
                    Expect::Any,
                )?;
            }
            None => {
                fs::remove_file(&real).ctx(|| format!("cannot remove {}", real.display()))?;
                sync_dir(dir)?;
            }
        }
        Ok(())
    }

    fn undo_current(&self) -> Result<()> {
        let tmp = self.paths.tmp("current", self.txn);
        let _ = fs::remove_file(&tmp);
        let ours = format!("releases/{}", self.intent.release.name);
        if read_link_string(&self.paths.current)?.as_deref() != Some(ours.as_str()) {
            return Ok(());
        }
        match &self.intent.prev_current {
            Some(prev) if *prev == ours => Ok(()),
            Some(prev) => set_symlink(&self.paths.current, prev, &tmp, Expect::Any).map(|_| ()),
            None => {
                fs::remove_file(&self.paths.current)
                    .ctx(|| format!("cannot remove {}", self.paths.current.display()))?;
                sync_dir(&self.paths.prefix)
            }
        }
    }

    fn undo_release(&self, p: &Pending) -> Result<()> {
        let announced = |s: Step| p.announced(|o| o.step() == Some(s));
        if announced(Step::Stage) || announced(Step::Publish) {
            tree::remove_tree(&self.paths.stage(self.txn))?;
        }
        if announced(Step::Publish) && !self.intent.release_existed {
            tree::remove_tree(&self.release_path())?;
        }
        Ok(())
    }

    fn undo_backups(&self, p: &Pending) -> Result<()> {
        let mut idx: Vec<u32> = p
            .ops
            .iter()
            .filter_map(|o| match o {
                JournalOp::Backup { index } => Some(*index),
                _ => None,
            })
            .collect();
        idx.sort_unstable();
        idx.dedup();
        for i in idx.into_iter().rev() {
            let Some(b) = self.intent.backups.get(i as usize) else {
                continue;
            };
            let from = self.safe.resolve(&b.path)?;
            let to = self.safe.resolve(&b.to)?;
            if lstat(&to)?.is_some() && lstat(&from)?.is_none() {
                rename_keeping_mode(&to, &from)
                    .ctx(|| format!("cannot restore {} from {}", from.display(), to.display()))?;
                if let Some(d) = from.parent() {
                    sync_dir(d)?;
                }
            }
            if let Some(d) = to.parent() {
                let _ = fs::remove_dir(d); // only if empty
            }
        }
        let _ = fs::remove_dir(&self.paths.backups);
        Ok(())
    }

    /// Undo everything the journal says this transaction may have done, newest first, then
    /// the tail. `final_tail`: also remove the lock file and the directories this transaction
    /// created (the owner rolling back its own failure, or a stand-alone `--resume`).
    fn undo(
        &self,
        p: &Pending,
        guard: &LockGuard,
        journal: &mut Journal,
        final_tail: bool,
    ) -> Result<()> {
        let f = &self.env.faults;
        if p.state != PendingState::RolledBack {
            journal.append(JournalOp::RollbackBegin)?;
            self.undo_receipt()?;
            f.rollback_action(0)?;
            for a in self.intent.aliases.iter().rev() {
                self.undo_link(a)?;
            }
            f.rollback_action(1)?;
            if let Some(l) = &self.intent.launcher {
                self.undo_link(l)?;
            }
            f.rollback_action(2)?;
            self.undo_current()?;
            f.rollback_action(3)?;
            self.undo_release(p)?;
            f.rollback_action(4)?;
            self.undo_backups(p)?;
            f.rollback_action(5)?;
            journal.append(JournalOp::RollbackDone)?;
        }
        self.tail(guard, final_tail)
    }

    fn tail(&self, guard: &LockGuard, final_tail: bool) -> Result<()> {
        if !final_tail {
            return fs::remove_file(&self.paths.journal)
                .or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })
                .ctx(|| "cannot remove the journal".to_string())
                .and_then(|()| sync_dir(&self.paths.prefix));
        }
        if self.intent.lock_created {
            guard
                .unlink()
                .ctx(|| format!("cannot remove {}", guard.path().display()))?;
        }
        match fs::remove_file(&self.paths.journal) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(InstallError::io("cannot remove the journal", e)),
        }
        let mut dirs: Vec<PathBuf> = self
            .intent
            .created_dirs
            .iter()
            .map(PathBuf::from)
            .filter(|d| d.starts_with(self.safe.canonical_root()))
            .collect();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in dirs {
            // Only an empty directory this transaction created goes; anything else stays.
            if fs::remove_dir(&d).is_ok()
                && let Some(parent) = d.parent()
            {
                let _ = sync_dir(parent);
            }
        }
        Ok(())
    }
}

/// The steps with their journal lines and fault points.
struct Tx<'a> {
    run: Run<'a>,
    journal: Journal,
    source: Option<&'a Source>,
    announced: Vec<JournalOp>,
}

impl Tx<'_> {
    fn announce(&mut self, step: Step, op: JournalOp) -> Result<()> {
        self.journal.append(op.clone())?;
        self.announced.push(op);
        let f = &self.run.env.faults;
        f.hit(FaultPoint::before(step))?;
        f.fail_io(step)
            .ctx(|| format!("step {} could not write", step.as_wire()))?;
        Ok(())
    }

    fn finished(&self, step: Step) -> Result<()> {
        self.run.env.faults.hit(FaultPoint::after(step))
    }

    fn run_steps(&mut self, steps: &[Step]) -> Result<()> {
        for &step in steps {
            match step {
                Step::Begin => {}
                Step::Backup => {
                    for i in 0..self.run.intent.backups.len() {
                        self.announce(Step::Backup, JournalOp::Backup { index: i as u32 })?;
                        self.run.do_backup(i)?;
                    }
                    if !self.run.intent.backups.is_empty() {
                        self.finished(Step::Backup)?;
                    }
                }
                Step::Stage => {
                    self.announce(step, JournalOp::Stage)?;
                    let src = self.source.ok_or_else(|| {
                        InstallError::refused("the verified artifacts are not available to stage")
                    })?;
                    self.run.do_stage(src)?;
                    self.finished(step)?;
                }
                Step::Verify => {
                    self.announce(step, JournalOp::Verify)?;
                    self.run.do_verify()?;
                    self.finished(step)?;
                }
                Step::Publish => {
                    self.announce(step, JournalOp::Publish)?;
                    self.run.do_publish()?;
                    self.finished(step)?;
                }
                Step::LinkCurrent => {
                    self.announce(step, JournalOp::LinkCurrent)?;
                    self.run.do_link_current()?;
                    self.finished(step)?;
                }
                Step::WriteLauncher => {
                    self.announce(step, JournalOp::WriteLauncher)?;
                    if let Some(l) = &self.run.intent.launcher {
                        self.run.do_link(l)?;
                    }
                    self.finished(step)?;
                }
                Step::WriteAliases => {
                    self.announce(step, JournalOp::WriteAliases)?;
                    for a in &self.run.intent.aliases {
                        self.run.do_link(a)?;
                    }
                    self.finished(step)?;
                }
                Step::WriteReceipt => {
                    self.announce(step, JournalOp::WriteReceipt)?;
                    self.run.do_write_receipt()?;
                    self.finished(step)?;
                }
                Step::Commit => {
                    self.announce(step, JournalOp::Commit)?;
                    match fs::remove_file(&self.run.paths.journal) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(InstallError::io("cannot close the journal", e)),
                    }
                    sync_dir(&self.run.paths.prefix)?;
                    self.finished(step)?;
                }
            }
        }
        Ok(())
    }

    fn pending(&self, state: PendingState) -> Pending {
        Pending {
            state,
            txn: self.run.txn,
            intent: self.run.intent.clone(),
            ops: self.announced.clone(),
            last_seq: 0,
        }
    }
}

// --------------------------------------------------------------- recovery

/// How `--resume` treats an interrupted transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverMode {
    /// Finish it when the staged data verifies; otherwise undo it.
    Auto,
    /// Undo it.
    Rollback,
}

fn recover_locked(
    env: &Env,
    safe: &Safe,
    paths: &Paths,
    mode: RecoverMode,
    guard: &LockGuard,
    final_tail: bool,
) -> Result<RecoverReport> {
    let found = match journal::read(&paths.journal)? {
        None => return Ok(RecoverReport::nothing()),
        Some(f) => f,
    };
    let remove_journal = |what: &str| -> Result<()> {
        fs::remove_file(&paths.journal).ctx(|| format!("cannot remove the journal ({what})"))?;
        sync_dir(&paths.prefix)
    };
    let p = match found {
        Found::Empty => {
            remove_journal("empty")?;
            return Ok(RecoverReport {
                action: RecoverAction::Cleaned,
                txn: None,
                release: None,
                detail: "an empty journal (the install had not begun) was removed".into(),
            });
        }
        Found::Committed => {
            remove_journal("committed")?;
            return Ok(RecoverReport {
                action: RecoverAction::Cleaned,
                txn: None,
                release: None,
                detail: "the transaction had committed; its leftover journal was removed".into(),
            });
        }
        Found::Pending(p) => p,
    };
    let run = Run {
        env,
        safe,
        paths,
        intent: &p.intent,
        txn: p.txn,
    };
    let release = p.intent.release.name.clone();
    let mut journal = Journal::reopen(&paths.journal, p.txn, p.last_seq)?;
    let can_complete = mode == RecoverMode::Auto
        && p.state == PendingState::InProgress
        && (!p.intent.steps.contains(&Step::Stage) || p.announced(|o| *o == JournalOp::Verify));
    if can_complete {
        let after_stage: Vec<Step> = p
            .intent
            .steps
            .iter()
            .copied()
            .filter(|s| !matches!(s, Step::Begin | Step::Backup | Step::Stage))
            .collect();
        let mut tx = Tx {
            run,
            journal,
            source: None,
            announced: p.ops.clone(),
        };
        match tx.run_steps(&after_stage) {
            Ok(()) => {
                return Ok(RecoverReport {
                    action: RecoverAction::Completed,
                    txn: Some(p.txn),
                    release: Some(release.clone()),
                    detail: format!("completed; release {release} is installed"),
                });
            }
            Err(e @ InstallError::Crashed(_)) => return Err(e),
            Err(why) => {
                // The staged data does not verify (or a step failed): undo instead.
                let pend = tx.pending(PendingState::InProgress);
                let mut pend = pend;
                pend.last_seq = p.last_seq;
                tx.run.undo(&pend, guard, &mut tx.journal, final_tail)?;
                return Ok(RecoverReport {
                    action: RecoverAction::RolledBack,
                    txn: Some(p.txn),
                    release: Some(release),
                    detail: format!("could not be completed ({why}); it was rolled back"),
                });
            }
        }
    }
    run.undo(&p, guard, &mut journal, final_tail)?;
    Ok(RecoverReport {
        action: RecoverAction::RolledBack,
        txn: Some(p.txn),
        release: Some(release.clone()),
        detail: format!("rolled back; the install is as it was before release {release}"),
    })
}

/// `fl install --resume` / `--rollback`: finish or undo an interrupted transaction, and do
/// nothing else.
pub fn recover(env: &Env, layout: &Layout, mode: RecoverMode) -> Result<RecoverReport> {
    let safe = Safe::new(&env.declared_root, env.home.as_deref())?;
    let paths = Paths::new(&safe, layout)?;
    safe.check_chain(&paths.journal)?;
    match lstat(&paths.prefix)? {
        None => return Ok(RecoverReport::nothing()),
        Some(m) if m.is_dir() => {}
        Some(_) => {
            return Err(InstallError::refused(format!(
                "{} is not a directory",
                paths.prefix.display()
            )));
        }
    }
    if lstat(&paths.journal)?.is_none() {
        return Ok(RecoverReport::nothing());
    }
    let guard = lock::acquire(&paths.lock, env.lock_timeout, || Ok(()))?;
    recover_locked(env, &safe, &paths, mode, &guard, true)
}

// ------------------------------------------------------------------ apply

fn probe_backup(probe: &FsProbe, shown: &str, to: String) -> Result<BackupSpec> {
    match probe.probe(shown) {
        PathState::File(fp) => Ok(BackupSpec {
            path: shown.to_string(),
            to,
            kind: OwnedPathKind::File,
            sha256: Some(fp.sha256),
            link_target: None,
        }),
        PathState::Symlink { target } => Ok(BackupSpec {
            path: shown.to_string(),
            to,
            kind: OwnedPathKind::Symlink,
            sha256: None,
            link_target: Some(target),
        }),
        PathState::Dir => Ok(BackupSpec {
            path: shown.to_string(),
            to,
            kind: OwnedPathKind::Dir,
            sha256: None,
            link_target: None,
        }),
        other => Err(InstallError::refused(format!(
            "{shown} cannot be backed up ({other:?})"
        ))),
    }
}

fn next_backup_number(dir: &Path) -> u32 {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
        + 1
}

fn backup_name(role: PathRole, shown: &str) -> String {
    let base = shown.rsplit('/').next().unwrap_or("item");
    let tag = match role {
        PathRole::Launcher => "launcher",
        PathRole::Alias => "alias",
        PathRole::Current => "current",
        _ => "release",
    };
    format!("{tag}-{base}")
}

/// Everything the transaction needs, decided under the lock.
struct Built {
    intent: TxnIntent,
    steps: Vec<Step>,
}

#[allow(clippy::too_many_arguments)]
fn build_intent(
    args: &ApplyArgs<'_>,
    planned: &Planned,
    resolved: &Resolved,
    safe: &Safe,
    paths: &Paths,
    probe: &FsProbe,
    source: Option<&Source>,
    created: &[PathBuf],
    guard: &LockGuard,
) -> Result<Built> {
    let plan = &planned.plan;
    let replaced = |shown: &str| resolved.replace.iter().any(|r| r == shown);
    let kept = |shown: &str| resolved.keep.iter().any(|r| r == shown);
    let by_role = |role: PathRole| -> Vec<&PlannedPath> {
        plan.paths.iter().filter(|p| p.role == role).collect()
    };

    let release_dir = by_role(PathRole::Release)
        .into_iter()
        .next()
        .ok_or_else(|| InstallError::refused("the plan names no release directory"))?;
    let release_replaced = replaced(&release_dir.path);
    let new_release = release_replaced || release_dir.disposition == Disposition::Create;
    if !new_release {
        let adds = plan
            .paths
            .iter()
            .any(|p| p.role == PathRole::ReleaseFile && p.disposition == Disposition::Create);
        if adds {
            return Err(InstallError::refused(format!(
                "release {} is already installed with other products; changing the selection \
                 inside an installed release is not implemented in this release",
                plan.release.name
            )));
        }
    }
    if release_replaced {
        let target = format!("releases/{}", plan.release.name);
        if read_link_string(&paths.current)?.as_deref() == Some(target.as_str()) {
            return Err(InstallError::refused(format!(
                "release {} is the selected release and was modified; it cannot be replaced \
                 while selected in this release (install another version, or restore the files)",
                plan.release.name
            )));
        }
    }

    // ---- backups
    let mut backups: Vec<BackupSpec> = Vec::new();
    let prefix_shown = &plan.layout.prefix;
    let n = next_backup_number(&paths.backups);
    let mut created_dirs: Vec<String> = created
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    for r in &resolved.replace {
        let role = plan
            .paths
            .iter()
            .find(|p| &p.path == r)
            .map(|p| p.role)
            .unwrap_or(PathRole::Release);
        let to = shown_join(
            prefix_shown,
            &format!("backups/{n}/{}", backup_name(role, r)),
        );
        backups.push(probe_backup(probe, r, to)?);
    }
    if !backups.is_empty() {
        for d in [paths.backups.join(n.to_string()), paths.backups.clone()] {
            if lstat(&d)?.is_none() {
                created_dirs.push(d.to_string_lossy().into_owned());
            }
        }
    }

    // ---- links
    let mk_link = |p: &PlannedPath, name: &str| -> Result<Option<LinkSpec>> {
        if kept(&p.path) {
            return Ok(None);
        }
        let target = probe.expand(p.link_target.as_deref().unwrap_or(""));
        let (write, prev) = if replaced(&p.path) {
            (true, None)
        } else {
            match p.disposition {
                Disposition::Keep => (false, None),
                Disposition::Create => (true, None),
                // An owned link being moved remembers where it pointed.
                Disposition::Repoint => (true, read_link_string(&safe.resolve(&p.path)?)?),
                other => {
                    return Err(InstallError::refused(format!(
                        "{} is {} and cannot be installed",
                        p.path,
                        other.as_wire()
                    )));
                }
            }
        };
        Ok(Some(LinkSpec {
            name: name.to_string(),
            path: p.path.clone(),
            target,
            prev,
            write,
        }))
    };
    let mut launcher = None;
    let mut launcher_kept = false;
    for p in by_role(PathRole::Launcher) {
        launcher = mk_link(p, "fl")?;
        launcher_kept = launcher.is_none();
    }
    let mut aliases: Vec<LinkSpec> = Vec::new();
    // Aliases are aliases of THE launcher: without ours they are not installed.
    if !launcher_kept {
        for p in by_role(PathRole::Alias) {
            let name = p.path.rsplit('/').next().unwrap_or("alias").to_string();
            if let Some(l) = mk_link(p, &name)? {
                aliases.push(l);
            }
        }
    }

    let current = by_role(PathRole::Current)
        .into_iter()
        .next()
        .ok_or_else(|| InstallError::refused("the plan names no `current`"))?;
    let current_replaced = replaced(&current.path);
    let current_disp = if current_replaced {
        Disposition::Create
    } else {
        current.disposition
    };
    let prev_current = if current_replaced {
        None
    } else {
        read_link_string(&paths.current)?
    };

    // ---- files
    let mut files: Vec<IntentFile> = Vec::new();
    let artifacts: Vec<OwnedArtifact> = plan
        .artifacts
        .iter()
        .map(|a| OwnedArtifact {
            id: a.id.clone(),
            sha256: a.sha256.clone(),
        })
        .collect();
    let release = ReceiptRelease {
        version: plan.release.version.clone(),
        name: plan.release.name.clone(),
        kind: plan.release.kind,
        commit: args.manifest.release.commit.clone(),
    };
    if new_release {
        let src = source.ok_or_else(|| {
            InstallError::Usage("installing a new release needs --artifact-dir".into())
        })?;
        for (rel, f) in &src.files {
            files.push(IntentFile {
                path: rel.clone(),
                sha256: f.sha256.clone(),
                size: f.size,
                exec: f.exec,
            });
        }
        let rj = release_json(&release, &artifacts);
        files.push(IntentFile {
            path: RELEASE_JSON.to_string(),
            sha256: sha256_hex(rj.as_bytes()),
            size: rj.len() as u64,
            exec: false,
        });
        files.sort_by(|a, b| a.path.cmp(&b.path));
    }

    // ---- steps
    let mut steps = vec![Step::Begin];
    if !backups.is_empty() {
        steps.push(Step::Backup);
    }
    if new_release {
        steps.extend([Step::Stage, Step::Verify, Step::Publish]);
    }
    if matches!(current_disp, Disposition::Create | Disposition::Repoint)
        || new_release && prev_current.as_deref() != Some(&format!("releases/{}", release.name))
    {
        steps.push(Step::LinkCurrent);
    }
    if launcher.as_ref().is_some_and(|l| l.write) {
        steps.push(Step::WriteLauncher);
    }
    if aliases.iter().any(|a| a.write) {
        steps.push(Step::WriteAliases);
    }
    steps.extend([Step::WriteReceipt, Step::Commit]);

    let products = plan
        .products
        .iter()
        .filter(|p| p.origin != Origin::Dependency)
        .map(|p| p.id)
        .collect();
    let intent = TxnIntent {
        release,
        profile: plan.profile,
        products,
        layout: ReceiptLayout {
            prefix: plan.layout.prefix.clone(),
            bin_dir: plan.layout.bin_dir.clone(),
            unit_dir: plan.layout.unit_dir.clone(),
        },
        artifacts,
        files,
        steps: steps.clone(),
        release_existed: !new_release,
        prev_current,
        launcher,
        aliases,
        backups,
        created_dirs,
        lock_created: guard.created,
        prev_receipt: planned.receipt_text.clone(),
    };
    Ok(Built { intent, steps })
}

/// Whether the installed state already is what the plan would produce.
fn is_noop(plan: &InstallPlan, receipt: Option<&InstallReceipt>, resolved: &Resolved) -> bool {
    let Some(r) = receipt else { return false };
    let ignored = |p: &PlannedPath| {
        matches!(p.role, PathRole::Receipt | PathRole::Lock) || resolved.keep.contains(&p.path)
    };
    let paths_ok = plan
        .paths
        .iter()
        .filter(|p| !ignored(p))
        .all(|p| matches!(p.disposition, Disposition::Keep | Disposition::Exists));
    let mut explicit: Vec<_> = plan
        .products
        .iter()
        .filter(|p| p.origin != Origin::Dependency)
        .map(|p| p.id)
        .collect();
    explicit.sort();
    let mut have = r.products.clone();
    have.sort();
    let mut arts: Vec<_> = plan
        .artifacts
        .iter()
        .map(|a| (a.id.clone(), a.sha256.clone()))
        .collect();
    arts.sort();
    let mut have_arts: Vec<_> = r
        .artifacts
        .iter()
        .map(|a| (a.id.clone(), a.sha256.clone()))
        .collect();
    have_arts.sort();
    paths_ok
        && resolved.replace.is_empty()
        && r.release.name == plan.release.name
        && r.profile == plan.profile
        && explicit == have
        && arts == have_arts
        && r.layout.prefix == plan.layout.prefix
        && r.layout.bin_dir == plan.layout.bin_dir
        && r.layout.unit_dir == plan.layout.unit_dir
}

fn abandon_fresh(guard: &LockGuard, created: &[PathBuf], safe: &Safe) {
    if guard.created {
        let _ = guard.unlink();
    }
    let mut dirs: Vec<&PathBuf> = created
        .iter()
        .filter(|d| d.starts_with(safe.canonical_root()))
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = fs::remove_dir(d);
    }
}

/// Apply the install the planner printed. Takes `install.lock`, finishes or undoes any
/// interrupted transaction first, re-plans under the lock, and then runs the transaction.
pub fn apply(env: &Env, args: &ApplyArgs<'_>) -> Result<ApplyReport> {
    if args.manifest.release.kind != ReleaseKind::Release {
        return Err(InstallError::refused(
            "this manifest lists no artifacts (a source build): installing from a reviewed \
             commit is not implemented in this release",
        ));
    }
    let safe = Safe::new(&env.declared_root, env.home.as_deref())?;
    let paths = Paths::new(&safe, &args.request.layout)?;
    safe.check_chain(&paths.prefix.join("x"))?;
    safe.check_chain(&paths.bin_dir.join("x"))?;
    let probe = FsProbe::new(env.home.clone());

    // ---- read-only: plan, decide, verify every artifact. Nothing is created yet. (With a
    // journal present the disk shows a half-done transaction: the plan waits for the
    // recovery that runs under the lock.)
    let mut source = None;
    if lstat(&paths.journal)?.is_none() {
        let pre = make_plan(env, &paths, args, &probe)?;
        resolve_decisions(&pre.plan, args.decisions, &safe)?;
        let will_stage = {
            let rd = pre.plan.paths.iter().find(|p| p.role == PathRole::Release);
            rd.is_some_and(|p| {
                p.disposition == Disposition::Create
                    || args
                        .decisions
                        .replace
                        .iter()
                        .any(|r| safe.lexical(r).ok() == safe.lexical(&p.path).ok())
            })
        };
        if will_stage {
            source = Some(prepare_source(args, &pre.plan)?);
        }
    }

    // ---- the lock (makes the directories it lives in)
    let mut created: Vec<PathBuf> = Vec::new();
    let guard = lock::acquire(&paths.lock, env.lock_timeout, || {
        for d in [&paths.prefix, &paths.releases, &paths.bin_dir] {
            created.extend(safe.ensure_dir(d)?);
        }
        Ok(())
    })?;

    let mut began = false;
    let result = apply_locked(
        env,
        args,
        &safe,
        &paths,
        &probe,
        &guard,
        &created,
        &mut source,
        &mut began,
    );
    if !began && result.is_err() {
        abandon_fresh(&guard, &created, &safe);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn apply_locked(
    env: &Env,
    args: &ApplyArgs<'_>,
    safe: &Safe,
    paths: &Paths,
    probe: &FsProbe,
    guard: &LockGuard,
    created: &[PathBuf],
    source: &mut Option<Source>,
    began: &mut bool,
) -> Result<ApplyReport> {
    let mut notes: Vec<String> = Vec::new();
    if let Some(pid) = guard.stale_pid {
        notes.push(format!(
            "took over a stale install lock left by pid {pid}, which is no longer running"
        ));
    }
    // An interrupted transaction is finished or undone before anything else happens.
    let recovered = {
        let r = recover_locked(env, safe, paths, RecoverMode::Auto, guard, false)?;
        (r.action != RecoverAction::NothingToDo).then_some(r)
    };

    let planned = make_plan(env, paths, args, probe)?;
    let resolved = resolve_decisions(&planned.plan, args.decisions, safe)?;
    let not_executed = services::not_executed(&planned.plan, env.services.as_deref());
    let release = planned.plan.release.name.clone();

    if is_noop(&planned.plan, planned.receipt.as_ref(), &resolved) {
        let kept = resolved.keep.clone();
        return Ok(ApplyReport {
            outcome: Outcome::AlreadyInstalled,
            release,
            txn: None,
            steps: Vec::new(),
            recovered,
            backups: Vec::new(),
            kept,
            not_executed,
            retention: planned.receipt.as_ref().map(retention),
            notes,
        });
    }

    if source.is_none()
        && planned
            .plan
            .paths
            .iter()
            .any(|p| p.role == PathRole::Release && p.disposition == Disposition::Create)
    {
        *source = Some(prepare_source(args, &planned.plan)?);
    }

    let built = build_intent(
        args,
        &planned,
        &resolved,
        safe,
        paths,
        probe,
        source.as_ref(),
        created,
        guard,
    )?;
    let txn = next_txn(planned.receipt.as_ref());
    if built.intent.steps.contains(&Step::Stage) && lstat(&paths.stage(txn))?.is_some() {
        return Err(InstallError::refused(format!(
            "{} exists but no journal owns it; remove it by hand after checking it",
            paths.stage(txn).display()
        )));
    }

    let mut journal = Journal::create(&paths.journal, txn)?;
    *began = true;
    journal.append(JournalOp::Begin {
        intent: Box::new(built.intent.clone()),
    })?;
    env.faults.hit(FaultPoint::after(Step::Begin))?;

    let run = Run {
        env,
        safe,
        paths,
        intent: &built.intent,
        txn,
    };
    let mut tx = Tx {
        run,
        journal,
        source: source.as_ref(),
        announced: Vec::new(),
    };
    match tx.run_steps(&built.steps) {
        Ok(()) => {}
        Err(e @ InstallError::Crashed(_)) => return Err(e),
        Err(cause) => {
            let pend = tx.pending(PendingState::InProgress);
            return match tx.run.undo(&pend, guard, &mut tx.journal, true) {
                Ok(()) => Err(InstallError::RolledBack {
                    cause: Box::new(cause),
                }),
                Err(e @ InstallError::Crashed(_)) => Err(e),
                Err(undo) => Err(InstallError::UndoFailed {
                    cause: Box::new(cause),
                    undo: Box::new(undo),
                }),
            };
        }
    }
    let after = read_receipt_text(&paths.receipt)
        .ok()
        .flatten()
        .and_then(|t| InstallReceipt::parse(&t).ok());
    Ok(ApplyReport {
        outcome: Outcome::Installed,
        release,
        txn: Some(txn),
        steps: built.steps.clone(),
        recovered,
        backups: built
            .intent
            .backups
            .iter()
            .map(|b| (b.path.clone(), b.to.clone()))
            .collect(),
        kept: resolved.keep,
        not_executed,
        retention: after.as_ref().map(retention),
        notes,
    })
}
