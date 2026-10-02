//! `fl github ledger` (GitHub ledger spec §3.5, §3.6, §6.1): set up the
//! GitHub ledger, walk its history, and quarantine a line. Each is run by a
//! person, by hand.

use crate::ctx::Ctx;
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_github::GithubLedger;
use fl_github::ledger::{InitOutcome, Mode, guidance};
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

/// The mode in force as one line (spec §6.2): `mode`, its name, and what
/// is missing when it is not protected.
pub fn mode_line(mode: &Mode) -> String {
    match mode {
        Mode::Protected => "mode\tprotected".to_string(),
        Mode::DetectionOnly { why } => format!("mode\tdetection-only\t{why}"),
    }
}

pub fn run(ctx: &Ctx<'_>, cmd: Cmd, root: Option<&Path>) -> Result<i32> {
    let gl = bound(ctx)?;
    match cmd {
        Cmd::Init { confirm } => {
            // ⚠ The manifest is read first (spec §6.1 step 4), and it is
            // read at the project's root: without one, refuse rather than
            // pass over that step.
            let Some(root) = root else {
                bail!(
                    "`fl github ledger init` reads the project's committed manifest at the \
                     project's root, and no config entry names one here. Run it inside the \
                     project's checkout"
                );
            };
            init(ctx, gl, root, confirm.as_deref())
        }
    }
}

/// `fl github ledger init` (spec §6.1).
fn init(ctx: &Ctx<'_>, gl: &GithubLedger<'_>, root: &Path, confirm: Option<&str>) -> Result<i32> {
    // ⚠ The mode before anything is written, here or on GitHub — the
    // manifest's import included: a rules read that fails leaves this
    // machine's store and the repository as they were, so it never leaves a
    // ledger whose "export and commit the manifest" was not said.
    let mode = gl.mode()?;
    // ⚠ The manifest first: a machine that lacks the ledger's first commit
    // learns it there, before anything is created on GitHub.
    crate::cmd::manifest::import_before_ledger_init(ctx.store, root)?;
    let repo = gl.repo().full_name.clone();
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
            println!("{}", mode_line(&mode));
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

#[cfg(test)]
mod tests {
    use super::*;
    use fl_github::fake::FakeGithub;
    use fl_github::{Client, EnvToken, Repo};
    use fl_store::RedbStore;

    // Spec §6.1 step 4: `init` without the project's root refuses; it never
    // passes over the manifest and goes on to GitHub.
    #[test]
    fn init_without_the_projects_root_refuses_and_creates_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        let client = Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        );
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("s.redb")).unwrap();
        let repo = Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        };
        let gl = GithubLedger::new(&client, repo, &store);
        let ctx = Ctx {
            store: &store,
            tracker: &store,
            ledger: &store,
            handles: &store,
            github: None,
            github_ledger: Some(&gl),
            tracker_label: String::new(),
        };
        let err = run(&ctx, Cmd::Init { confirm: None }, None).unwrap_err();
        assert!(
            err.to_string().contains("no config entry names one here"),
            "{err:#}"
        );
        assert_eq!(fake.ledger_head(), None, "nothing was created");
    }
}
