use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::{GateId, ProjectId};
use fl_core::model::{CommandSpec, GateDef, GateKind, PopulationDelivery, Selector};
use fl_core::store::Catalog;
use fl_core::{Iri, Kind};
use fl_exec::evaluate::run_single_gate;
use fl_store::RedbStore;

/// Declare a gate: a population to examine, and a program to run over it.
///
/// The population comes from exactly one of `--glob`, `--changed-since`
/// or `--population-from`. There is no default: a gate that does not say
/// what it examines is the empty-population failure waiting to happen.
#[derive(clap::Args)]
#[command(group(
    clap::ArgGroup::new("population")
        .required(true)
        .args(["glob", "changed_since", "population_from"])
))]
pub struct AddArgs {
    #[arg(long)]
    project: Ref,
    #[arg(long)]
    name: String,
    /// `command` or `agent`.
    #[arg(long, default_value = "command")]
    kind: String,
    /// Population: every file matching this glob, walked from the project root.
    #[arg(long)]
    glob: Option<String>,
    /// Population: every file that differs from this git ref.
    #[arg(long, value_name = "REF")]
    changed_since: Option<String>,
    /// Population: the paths this program prints on stdout, one per line.
    /// It runs in the project root, and a non-zero exit is an error — an
    /// unknown population, never an empty one.
    #[arg(long, value_name = "PROGRAM")]
    population_from: Option<String>,
    /// An argument for `--population-from`. Repeatable. Use the `--x=-v`
    /// form for any value that starts with `-`.
    #[arg(long, value_name = "VALUE", num_args = 0.., requires = "population_from")]
    population_arg: Vec<String>,
    /// The program the gate runs over the population it resolved.
    #[arg(long)]
    program: String,
    #[arg(long, num_args = 0..)]
    arg: Vec<String>,
    #[arg(long, default_value_t = 1)]
    min_population: u64,
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
    #[arg(long, default_value = "unknown")]
    authored_by: String,
}

#[derive(Subcommand)]
pub enum Cmd {
    Add(Box<AddArgs>),
    List {
        #[arg(long)]
        project: Ref,
    },
    Show {
        id: Ref,
    },
    /// Re-stamp a gate against HEAD: "I looked, and it still holds."
    Affirm {
        id: Ref,
        #[arg(long, default_value = "unknown")]
        by: String,
    },
    /// Run one gate against the live working tree and print its verdict.
    Run {
        id: Ref,
    },
    /// TESTING AFFORDANCE: rewrite a command gate's program in place, with no
    /// re-authoring and no new commit stamp. This is how a test simulates a
    /// repair without a model in the loop — and, just as directly, how a
    /// person could weaken a reproduction by hand and have it read as a
    /// legitimate fix. Refused outside this repo's own test harness: there
    /// is no production route to this command.
    SetProgram {
        id: Ref,
        #[arg(long)]
        program: String,
    },
}

impl Cmd {
    /// Every item this command names, so a full IRI on the command line can
    /// select the store that holds it (spec §2.6). A transition name or a
    /// program string is not an id and never appears here.
    pub fn iris(&self) -> Vec<Iri> {
        match self {
            Cmd::Add(args) => refs::iris(&[&args.project]),
            Cmd::List { project } => refs::iris(&[project]),
            Cmd::Show { id } => refs::iris(&[id]),
            Cmd::Affirm { id, .. } => refs::iris(&[id]),
            Cmd::Run { id } => refs::iris(&[id]),
            Cmd::SetProgram { id, .. } => refs::iris(&[id]),
        }
    }

    /// Whether this command names any item by handle rather than IRI.
    pub fn has_handle(&self) -> bool {
        match self {
            Cmd::Add(args) => refs::has_handle(&[&args.project]),
            Cmd::List { project } => refs::has_handle(&[project]),
            Cmd::Show { id } => refs::has_handle(&[id]),
            Cmd::Affirm { id, .. } => refs::has_handle(&[id]),
            Cmd::Run { id } => refs::has_handle(&[id]),
            Cmd::SetProgram { id, .. } => refs::has_handle(&[id]),
        }
    }
}

/// The gate `id` names, or a refusal that echoes what was typed.
fn gate(store: &RedbStore, id: &Ref) -> Result<GateDef> {
    let gid = GateId(refs::resolve(store, store.label(), Kind::Gate, id)?);
    let Some(g) = store.get_gate(&gid)? else {
        bail!(
            "`{id}` is not a gate in the store at {}. Use `fl gate list --project <project>` \
             to see gates that exist.",
            store.label()
        );
    };
    Ok(g)
}

pub fn run(store: &RedbStore, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Add(args) => {
            let AddArgs {
                project,
                name,
                kind,
                glob,
                changed_since,
                population_from,
                population_arg,
                program,
                arg,
                min_population,
                timeout_secs,
                authored_by,
            } = *args;
            if kind != "command" {
                bail!(
                    "`{kind}` is not a gate kind. Milestone 1 implements `command`. \
                     `agent` is defined but not yet built."
                );
            }
            let p = ProjectId(refs::resolve(
                store,
                store.label(),
                Kind::Project,
                &project,
            )?);
            let Some(proj) = store.get_project(&p)? else {
                bail!(
                    "`{project}` is not a project in the store at {}. Run `fl project list` to \
                     see the ones that exist.",
                    store.label()
                );
            };
            let head = fl_exec::git::Git::head(std::path::Path::new(&proj.root)).map_err(|e| {
                anyhow::anyhow!(
                    "{e}. A project must be a git working tree, because a gate's provenance is a commit."
                )
            })?;
            let id = store.add_gate(
                &p,
                &name,
                GateKind::Command(CommandSpec {
                    program,
                    args: arg,
                    delivery: PopulationDelivery::Args,
                    timeout_secs,
                    pass_codes: vec![0],
                }),
                // clap's group guarantees exactly one of the three is set,
                // so the final arm is unreachable rather than a default. A
                // default here would be a gate quietly examining something
                // other than what its author asked for.
                match (glob, changed_since, population_from) {
                    (Some(pattern), _, _) => Selector::Glob { pattern },
                    (_, Some(base), _) => Selector::Changed { base },
                    (_, _, Some(program)) => Selector::Command {
                        program,
                        args: population_arg,
                    },
                    // clap's `population` group already refuses this. Not
                    // `unreachable!()`: a panic here would be a refusal with
                    // no guidance, and its message would name the same three
                    // flags a real refusal does — which is precisely what let
                    // the test for that refusal pass on a crash.
                    (None, None, None) => bail!(
                        "gate `{name}` does not say what it examines. Give it one of \
                         `--glob <PATTERN>`, `--changed-since <REF>` or \
                         `--population-from <PROGRAM>`."
                    ),
                },
                min_population,
                &head,
                &authored_by,
            )?;
            println!(
                "{}\t{name}\t{head}",
                refs::show(store, Kind::Gate, id.iri())?
            );
        }
        Cmd::List { project } => {
            let p = ProjectId(refs::resolve(
                store,
                store.label(),
                Kind::Project,
                &project,
            )?);
            for g in store.list_gates(&p)? {
                println!(
                    "{}\t{}\t{}",
                    refs::show(store, Kind::Gate, g.id.iri())?,
                    g.name,
                    g.authored_at_commit
                );
            }
        }
        Cmd::Show { id } => {
            let g = gate(store, &id)?;
            println!("{}", serde_json::to_string_pretty(&g)?);
        }
        Cmd::Affirm { id, by } => {
            let mut g = gate(store, &id)?;
            let Some(proj) = store.get_project(&g.project)? else {
                bail!(
                    "gate {id} belongs to project {}, which no longer exists.",
                    refs::show(store, Kind::Project, g.project.iri())?
                );
            };
            let head = fl_exec::git::Git::head(std::path::Path::new(&proj.root))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            g.authored_at_commit = head.clone();
            g.authored_by = by;
            store.update_gate(&g)?;
            println!("{id}\t{head}");
        }
        Cmd::Run { id } => {
            let g = gate(store, &id)?;
            let report = run_single_gate(store, store, &g.project, &g.id)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let (label, detail) = report.verdict.describe();
            println!("{label}\t{}\t{detail}", g.name);
            return Ok(report.verdict.exit_code());
        }
        Cmd::SetProgram { id, program } => {
            // ⚠⚠ Fix-wave finding 3: an honest sentence in --help is not a
            // guard. This rewrites a gate's command in place with no
            // re-authoring and no new commit stamp, so accepting it in a
            // production build would hand out an unearned green light — a
            // finding closed, `git log` unchanged, no repair made. The
            // check below is a repo-internal test hook, set only by this
            // checkout's own `.cargo/config.toml` for processes Cargo
            // itself launches (`cargo build`/`run`/`test`); a `fl`
            // binary run any other way — including the only way a
            // production build is ever run, as a standalone artifact
            // outside Cargo's process tree — never has it set. Deliberately
            // not named in --help or in any shipped documentation.
            if std::env::var_os("FL_SET_PROGRAM_TEST_HOOK").is_none() {
                bail!(
                    "`gate set-program` is refused: it is a testing affordance, not a \
                     production command. Rewriting a gate's program in place, with no \
                     re-authoring and no new commit stamp, is exactly how a red gate could be \
                     turned green by hand and read as a legitimate repair. There is no \
                     production route to this command — to change what a gate runs, author a \
                     new gate with `gate add` against the current commit, so the change is \
                     provenanced like any other."
                );
            }
            let mut g = gate(store, &id)?;
            let GateKind::Command(spec) = &mut g.kind else {
                bail!("gate {id} is not a command gate, so it has no `program` field to rewrite.");
            };
            spec.program = program;
            store.update_gate(&g)?;
            println!("{id}\t{}", g.name);
        }
    }
    Ok(0)
}
