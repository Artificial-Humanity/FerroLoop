//! `fl github ledger` (GitHub ledger spec §3.5, §3.6, §6.1): set up the
//! GitHub ledger, walk its history, and quarantine a line. Each is run by a
//! person, by hand.

use crate::ctx::Ctx;
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_github::GithubLedger;
use fl_github::ledger::{InitOutcome, guidance};
use std::path::Path;

#[derive(Subcommand)]
pub enum Cmd {
    /// Set up the GitHub ledger on the bound repository, or record this
    /// machine's cut-over on one already set up. Safe to run again.
    Init {
        /// The ledger's first commit, as an earlier `init` asked you to
        /// confirm.
        #[arg(long)]
        confirm: Option<String>,
    },
}

/// The GitHub ledger the binding names, or the refusal that says how to
/// name it (spec §6.1 step 1).
fn bound<'a>(ctx: &Ctx<'a>) -> Result<&'a GithubLedger<'a>> {
    match ctx.github_ledger {
        Some(gl) => Ok(gl),
        None => bail!(
            "this project's tracker binding does not name the GitHub ledger. Add \
             `ledger = \"github\"` to its `tracker = {{ … }}` in the config, then run this again"
        ),
    }
}

pub fn run(ctx: &Ctx<'_>, cmd: Cmd, root: Option<&Path>) -> Result<i32> {
    let gl = bound(ctx)?;
    match cmd {
        Cmd::Init { confirm } => init(ctx, gl, root, confirm.as_deref()),
    }
}

/// `fl github ledger init` (spec §6.1).
fn init(
    ctx: &Ctx<'_>,
    gl: &GithubLedger<'_>,
    root: Option<&Path>,
    confirm: Option<&str>,
) -> Result<i32> {
    // ⚠ The manifest first: a machine that lacks the ledger's first commit
    // learns it there, before anything is created on GitHub.
    if let Some(root) = root {
        crate::cmd::manifest::import_before_ledger_init(ctx.store, root)?;
    }
    let repo = gl.repo().full_name.clone();
    // ⚠ The mode before anything is created or recorded: once the root is
    // recorded nothing below can fail, so a rules read that fails never
    // leaves a ledger whose "export and commit the manifest" was not said.
    let mode = gl.mode()?;
    let outcome = gl.init(&fl_exec::stamp::entry_id(), confirm)?;
    match &outcome {
        InitOutcome::Created { root } => println!("created\tfl/ledger\t{root}"),
        InitOutcome::Adopted { root } => println!("adopted\tfl/ledger\t{root}"),
        // ⚠ Spec §6.1 step 6: the ledger is set up, so `init` says so and
        // stops — after this machine's cut-over (step 5) and the mode in
        // force, a statement of state; the guidance is not printed again.
        InitOutcome::AlreadySetUp {
            root,
            cutover_recorded,
        } => {
            println!("set up\tfl/ledger\t{root}");
            if *cutover_recorded {
                println!(
                    "this machine's cut-over is recorded: from now on, its decisions publish \
                     their evidence"
                );
            }
            println!("mode\t{}", mode.name());
            return Ok(0);
        }
        // ⚠ Nothing was recorded, and nothing was refused: exit 1.
        InitOutcome::Confirm { root } => {
            println!("confirm\tfl/ledger\t{root}");
            println!(
                "`fl/ledger` exists, but neither this machine nor the project's manifest records \
                 its first commit. If {root} is the commit you created with `fl github ledger \
                 init`, run `fl github ledger init --confirm {root}`. Nothing was recorded."
            );
            return Ok(1);
        }
    }
    // Created or adopted: this run recorded the root (steps 7-9).
    for paragraph in guidance(&repo, &mode) {
        println!("\n{paragraph}");
    }
    println!(
        "\nNext: run `fl manifest export --project <project>` and commit \
         `.fl/manifest.json`. Every other machine learns the ledger's first commit from it, \
         and refuses to publish until it has."
    );
    Ok(0)
}
