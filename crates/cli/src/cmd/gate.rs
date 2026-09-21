use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::ids::{GateId, ProjectId};
use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
use fl_core::store::Store;
use fl_exec::evaluate::run_single_gate;

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
    project: u64,
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
        project: u64,
    },
    Show {
        id: u64,
    },
    /// Re-stamp a gate against HEAD: "I looked, and it still holds."
    Affirm {
        id: u64,
        #[arg(long, default_value = "unknown")]
        by: String,
    },
    /// Run one gate against the live working tree and print its verdict.
    Run {
        id: u64,
    },
    /// TESTING AFFORDANCE: rewrite a command gate's program in place, with no
    /// re-authoring and no new commit stamp. This is how a test simulates a
    /// repair without a model in the loop — and, just as directly, how a
    /// person could weaken a reproduction by hand and have it read as a
    /// legitimate fix. Refused outside this repo's own test harness: there
    /// is no production route to this command.
    SetProgram {
        id: u64,
        #[arg(long)]
        program: String,
    },
}

pub fn run(store: &mut impl Store, cmd: Cmd) -> Result<i32> {
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
            let p = ProjectId(project);
            let Some(proj) = store.get_project(p)? else {
                bail!(
                    "no project with id {project}. Run `flctl project list` to see the ids that exist."
                );
            };
            let head = fl_exec::git::Git::head(std::path::Path::new(&proj.root)).map_err(|e| {
                anyhow::anyhow!(
                    "{e}. A project must be a git working tree, because a gate's provenance is a commit."
                )
            })?;
            let id = store.add_gate(
                p,
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
            println!("{id}\t{name}\t{head}");
        }
        Cmd::List { project } => {
            for g in store.list_gates(ProjectId(project))? {
                println!("{}\t{}\t{}", g.id, g.name, g.authored_at_commit);
            }
        }
        Cmd::Show { id } => {
            let Some(g) = store.get_gate(GateId(id))? else {
                bail!(
                    "no gate with id {id}. Use `flctl gate list --project <id>` to see gates that exist."
                );
            };
            println!("{}", serde_json::to_string_pretty(&g)?);
        }
        Cmd::Affirm { id, by } => {
            let Some(mut g) = store.get_gate(GateId(id))? else {
                bail!(
                    "no gate with id {id}. Use `flctl gate list --project <id>` to see gates that exist."
                );
            };
            let Some(proj) = store.get_project(g.project)? else {
                bail!(
                    "gate {id} belongs to project {}, which no longer exists.",
                    g.project
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
            let Some(g) = store.get_gate(GateId(id))? else {
                bail!(
                    "no gate with id {id}. Use `flctl gate list --project <id>` to see gates that exist."
                );
            };
            let report = run_single_gate(store, g.project, GateId(id))
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
            // itself launches (`cargo build`/`run`/`test`); a `flctl`
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
            let Some(mut g) = store.get_gate(GateId(id))? else {
                bail!(
                    "no gate with id {id}. Use `flctl gate list --project <id>` to see gates that exist."
                );
            };
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
