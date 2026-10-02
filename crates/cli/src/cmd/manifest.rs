//! `fl manifest` (GitHub tracker spec §4), and the checks other commands
//! make before they run an imported gate or publish a gate's IRI.

use crate::refs::{self, Ref};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fl_core::ids::{GateId, ProjectId};
use fl_core::model::{GateKind, Selector};
use fl_core::store::{Bindings, Catalog};
use fl_core::{Iri, Kind};
use fl_exec::git::Git;
use fl_store::RedbStore;
use fl_store::manifest::{Currency, MANIFEST_PATH, Manifest};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum Cmd {
    /// Write `.fl/manifest.json` for a project this store authors.
    Export {
        #[arg(long)]
        project: Ref,
    },
    /// Import `.fl/manifest.json` from a project root into this store.
    Import {
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Check that the manifest is intact, matches this store, and is committed.
    Check {
        #[arg(long)]
        project: Ref,
    },
}

impl Cmd {
    fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Export { project } | Cmd::Check { project } => vec![project],
            Cmd::Import { .. } => vec![],
        }
    }
    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }
    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }
    /// `Import` works on the store bound to the root it imports into.
    pub fn root(&self) -> Option<&Path> {
        match self {
            Cmd::Import { root } => Some(root),
            _ => None,
        }
    }
}

/// The manifest at `root`, parsed and verified. A missing file is refused by
/// its path: "no manifest" is never read as "nothing to check".
pub fn read(root: &Path) -> Result<Manifest> {
    let path = root.join(MANIFEST_PATH);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read the manifest at {}", path.display()))?;
    Manifest::parse(&text).with_context(|| format!("{} was refused", path.display()))
}

fn root_of(store: &RedbStore, project: &ProjectId) -> Result<PathBuf> {
    let Some(p) = store.get_project(project)? else {
        bail!("{project} is held by this store, but it is not a project");
    };
    Ok(PathBuf::from(p.root))
}

/// Spec §4.5: on an importing machine, the working tree's manifest must be
/// the one this store imported. A project this store authors passes.
pub fn ensure_import_current(store: &RedbStore, project: &ProjectId) -> Result<()> {
    let Some(recorded) = store.imported_hash(project)? else {
        return Ok(());
    };
    let root = root_of(store, project)?;
    let m = read(&root)?;
    if m.content_sha256 != recorded {
        bail!(
            "the manifest at {} changed since this store imported it (imported {recorded}, \
             now {}). Run `fl manifest import` before running its gates.",
            root.join(MANIFEST_PATH).display(),
            m.content_sha256
        );
    }
    Ok(())
}

/// Spec §4.3–§4.5: before a gate's IRI is written where another machine
/// reads it, the committed manifest must carry that gate as this store has
/// it. `gate: None` checks every gate of the project.
pub fn ensure_publishable(
    store: &RedbStore,
    project: &ProjectId,
    gate: Option<&GateId>,
) -> Result<()> {
    // Both branches below: an importing store's check covers the manifest,
    // not which project the named gate belongs to.
    if let Some(g) = gate {
        let def = store
            .get_gate(g)?
            .with_context(|| format!("{g} is held by this store, but it is not a gate"))?;
        if def.project != *project {
            bail!(
                "gate `{}` belongs to project {}, not to {project}. Name a gate of the finding's \
                 project",
                def.name,
                def.project
            );
        }
    }
    let root = root_of(store, project)?;
    if store.imported_hash(project)?.is_some() {
        ensure_import_current(store, project)?;
    } else {
        let m = read(&root)?;
        // ⚠ A manifest for another project would otherwise pass vacuously
        // for a project with no gates.
        if m.body.project != *project {
            bail!(
                "{} is the manifest of project {}, not of {project}. Run `fl manifest export`, \
                 then commit.",
                root.join(MANIFEST_PATH).display(),
                m.body.project
            );
        }
        let gates = match gate {
            Some(g) => vec![
                store
                    .get_gate(g)?
                    .with_context(|| format!("{g} is held by this store, but it is not a gate"))?,
            ],
            None => store.list_gates(project)?,
        };
        for g in &gates {
            match m.currency_of(g) {
                Currency::Current => {}
                Currency::Differs => bail!(
                    "gate `{}` changed since the manifest was exported. Run \
                     `fl manifest export`, then commit.",
                    g.name
                ),
                Currency::Absent => bail!(
                    "gate `{}` is not in the manifest. Run `fl manifest export`, then commit.",
                    g.name
                ),
            }
        }
        // The whole project, not only its gates: a transition added or
        // changed since export would otherwise never reach another machine.
        if gate.is_none() {
            let now = store.export_manifest(project, "", 0, None)?;
            if now.body.gates.len() != m.body.gates.len() {
                bail!(
                    "the manifest lists a gate this store no longer holds. Run \
                     `fl manifest export`, then commit."
                );
            }
            if now.body.transitions != m.body.transitions {
                bail!(
                    "the project's transitions changed since the manifest was exported. Run \
                     `fl manifest export`, then commit."
                );
            }
        }
    }
    if !Git::is_committed(&root, MANIFEST_PATH).map_err(|e| anyhow::anyhow!("{e}"))? {
        if Git::is_ignored(&root, MANIFEST_PATH).map_err(|e| anyhow::anyhow!("{e}"))? {
            bail!(
                "{} is ignored by git, so it can never be committed. Another machine can only \
                 resolve a gate through a committed manifest: remove the pattern that ignores \
                 it from .gitignore.",
                root.join(MANIFEST_PATH).display()
            );
        }
        bail!(
            "{} is not committed. Another machine can only resolve a gate through a \
             committed manifest: commit it. (fl cannot tell whether the commit was pushed.)",
            root.join(MANIFEST_PATH).display()
        );
    }
    Ok(())
}

/// What an import did, for a person: the project, its gates and
/// transitions, and each gate's handle in this store.
fn print_import(store: &RedbStore, m: &Manifest, report: &fl_store::ImportReport) -> Result<()> {
    if let Some((old, new)) = &report.root_moved {
        println!("moved\tthe project's gates now run over {new}, not {old}");
    }
    println!(
        "imported\t{}\tgates: {} added, {} changed, {} unchanged\ttransitions: {}",
        refs::show(store, Kind::Project, report.project.iri())?,
        report.gates_added,
        report.gates_changed,
        report.gates_unchanged,
        report.transitions
    );
    // ⚠ This store numbers handles on its own: they can differ from the
    // authoring machine's, so the person needs to see them here.
    for g in &m.body.gates {
        println!(
            "gate\t{}\t{}",
            refs::show(store, Kind::Gate, g.id.iri())?,
            g.name
        );
    }
    for name in &report.transitions_removed {
        println!("removed\ttransition\t{name}");
    }
    Ok(())
}

/// What `fl github ledger init` learns from the project's committed
/// manifest before it touches GitHub (GitHub ledger spec §6.1 step 4): on a
/// machine that does not author the project, the manifest is imported as
/// `fl manifest import` imports it — which records the ledger's first
/// commit when the manifest carries one. The authoring store wrote that
/// root into the manifest itself; a project with no manifest yet has
/// nothing to teach.
///
/// ⚠ Without it, a machine that never imported the manifest knows no first
/// commit: where the ledger was deleted it would create a second one and
/// hide the deletion, and it would offer an existing ledger for
/// confirmation as if no machine knew it.
pub fn import_before_ledger_init(store: &RedbStore, root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("`{}` could not be resolved", root.display()))?;
    let path = root.join(MANIFEST_PATH);
    if !path
        .try_exists()
        .with_context(|| format!("could not look for {}", path.display()))?
    {
        return Ok(());
    }
    let m = read(&root)?;
    let project = &m.body.project;
    if store.owns(project.iri())? && store.imported_hash(project)?.is_none() {
        return Ok(());
    }
    Git::head(&root)
        .map_err(|e| anyhow::anyhow!("`{}` is not a git working tree: {e}", root.display()))?;
    let report = store.import_manifest(&m, &root.display().to_string())?;
    print_import(store, &m, &report)
}

/// What `manifest export` knows of the repository its project's ledger is
/// in (GitHub ledger spec §6.1 step 4) — or that it cannot know.
pub enum Binding {
    /// The config entry was not read (`--db`/`$FL_DB`), or the command runs
    /// on another project's store.
    Unread,
    /// The project's tracker is the local store: no GitHub ledger.
    Local,
    /// The project is bound to this `owner/repo`.
    Github(String),
}

/// The `node_id` whose ledger root an export writes.
///
/// ⚠ Every export writes the root from the store, so it cannot be dropped
/// (spec §6.1 step 4). When the store records a root and the export cannot
/// tell which repository is this project's — the entry was not read, or it
/// names a repository this store has no node for — it is refused rather
/// than written without one.
fn ledger_node(store: &RedbStore, binding: &Binding) -> Result<Option<String>> {
    match binding {
        Binding::Github(repo) => match store.bound_node_id(repo)? {
            Some(node) => Ok(Some(node)),
            None if store.holds_a_ledger_root()? => bail!(
                "this store records a GitHub ledger root, but no repository node for `{repo}`, \
                 the repository this project's config entry names, so fl cannot tell which \
                 root belongs in the manifest. Run a command that opens the GitHub tracker from \
                 the project's root — `fl github whoami` — which records the binding, then \
                 export again"
            ),
            None => Ok(None),
        },
        Binding::Local => Ok(None),
        Binding::Unread => {
            if store.holds_a_ledger_root()? {
                bail!(
                    "this store records a GitHub ledger root, and fl cannot tell which \
                     repository this project's ledger is in: with --db (or $FL_DB), or an IRI \
                     held by another project's store, it does not read the project's config \
                     entry. Run `fl manifest export` from the project's root, without --db and \
                     with $FL_DB unset, so the manifest keeps the root every other machine \
                     checks the ledger against"
                );
            }
            Ok(None)
        }
    }
}

pub fn run(store: &RedbStore, cmd: Cmd, binding: &Binding) -> Result<i32> {
    match cmd {
        Cmd::Export { project } => {
            let p = ProjectId(refs::resolve(
                store,
                store.label(),
                Kind::Project,
                &project,
            )?);
            let root = root_of(store, &p)?;
            let head = Git::head(&root).map_err(|e| anyhow::anyhow!("{e}"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("the system clock is before 1970")?
                .as_secs();
            let node = ledger_node(store, binding)?;
            let m = store.export_manifest(&p, &head, now, node.as_deref())?;
            // ⚠ Every gate is printed with what it runs, and what it
            // examines: either can name a local path, and this file is
            // about to be committed.
            for g in &m.body.gates {
                let runs = match &g.kind {
                    GateKind::Command(c) => format!("{} {}", c.program, c.args.join(" ")),
                    GateKind::Agent(a) => format!("agent {}", a.adapter),
                };
                println!(
                    "gate\t{}\t{}\t{}",
                    refs::show(store, Kind::Gate, g.id.iri())?,
                    g.name,
                    runs.trim_end()
                );
                let population = match &g.selector {
                    Selector::Glob { pattern } => format!("glob {pattern}"),
                    Selector::Changed { base } => format!("changed since {base}"),
                    Selector::Command { program, args } => format!("{program} {}", args.join(" ")),
                };
                println!("\tpopulation\t{}", population.trim_end());
            }
            let path = root.join(MANIFEST_PATH);
            std::fs::create_dir_all(path.parent().expect("MANIFEST_PATH has a parent"))
                .with_context(|| format!("could not create {}", path.display()))?;
            std::fs::write(&path, m.to_json())
                .with_context(|| format!("could not write {}", path.display()))?;
            println!("wrote\t{}\tsha256:{}", path.display(), m.content_sha256);
            if let Some(ledger_root) = &m.body.ledger_root {
                println!("ledger_root\t{}", ledger_root.commit);
            }
            println!(
                "commit it: another machine resolves these gates only through a committed manifest"
            );
        }
        Cmd::Import { root } => {
            let root = root
                .canonicalize()
                .with_context(|| format!("`{}` could not be resolved", root.display()))?;
            Git::head(&root).map_err(|e| {
                anyhow::anyhow!("`{}` is not a git working tree: {e}", root.display())
            })?;
            let m = read(&root)?;
            let report = store.import_manifest(&m, &root.display().to_string())?;
            print_import(store, &m, &report)?;
        }
        Cmd::Check { project } => {
            let p = ProjectId(refs::resolve(
                store,
                store.label(),
                Kind::Project,
                &project,
            )?);
            ensure_publishable(store, &p, None)?;
            println!(
                "current\t{}",
                root_of(store, &p)?.join(MANIFEST_PATH).display()
            );
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};

    #[test]
    fn a_gate_of_another_project_is_refused_before_any_manifest_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("s.redb")).unwrap();
        let p1 = store.add_project("/one").unwrap();
        let p2 = store.add_project("/two").unwrap();
        let kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        });
        let sel = Selector::Glob {
            pattern: "**/*".into(),
        };
        let g2 = store.add_gate(&p2, "g", kind, sel, 1, "c", "o").unwrap();
        let err = ensure_publishable(&store, &p1, Some(&g2)).unwrap_err();
        assert!(err.to_string().contains("belongs to project"), "{err}");
    }

    // A manifest outside a git working tree is refused before it is
    // imported: the gates it carries run over a working tree.
    #[test]
    fn init_imports_no_manifest_outside_a_git_working_tree() {
        let dir = tempfile::tempdir().unwrap();
        let author = RedbStore::open(&dir.path().join("author.redb")).unwrap();
        let p = author.add_project("/one").unwrap();
        let m = author
            .export_manifest(&p, "0123456789abcdef0123456789abcdef01234567", 0, None)
            .unwrap();
        let plain = tempfile::tempdir().unwrap();
        let path = plain.path().join(MANIFEST_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, m.to_json()).unwrap();
        let other = RedbStore::open(&dir.path().join("other.redb")).unwrap();
        let err = import_before_ledger_init(&other, plain.path()).unwrap_err();
        assert!(
            err.to_string().contains("is not a git working tree"),
            "{err:#}"
        );
        assert!(!other.owns(p.iri()).unwrap(), "nothing was imported");
    }
}
