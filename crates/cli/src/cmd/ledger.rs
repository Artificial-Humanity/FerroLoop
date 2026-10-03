//! `fl github ledger` (GitHub ledger spec §3.5, §3.6, §6.1): set up the
//! GitHub ledger, walk its history, and quarantine a line. Each is run by a
//! person, by hand.

use crate::ctx::Ctx;
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_exec::stamp;
use fl_github::GithubLedger;
use fl_github::ledger::{InitOutcome, Mode, VERIFY_LIMIT, VerifyPhase, Visibility, guidance};
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
    /// Walk every commit of `fl/ledger` from its first and check that each
    /// only adds lines or segments; then check that no id is on two
    /// different lines. About one request per commit, plus one per segment.
    Verify {
        /// Stop after walking back this many commits.
        #[arg(long, default_value_t = VERIFY_LIMIT)]
        max_commits: usize,
    },
    /// Mark one line of the ledger for readers to skip. Nothing is removed.
    Quarantine {
        /// The segment, as a path on the branch (`runs/<key>/<n>.jsonl`).
        file: String,
        /// The line, from 1.
        line: u64,
        /// Who decided. Written to the ledger permanently.
        #[arg(long)]
        by: String,
        /// Why. Written to the ledger permanently.
        #[arg(long)]
        reason: String,
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
        Cmd::Verify { max_commits } => verify(gl, max_commits),
        Cmd::Quarantine {
            file,
            line,
            by,
            reason,
        } => quarantine(gl, &file, line, &by, &reason),
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
    let outcome = gl.init(&stamp::entry_id(), confirm)?;
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

/// How often `verify` says how far it has gotten, in each phase.
const PROGRESS_EVERY: usize = 100;

/// What `verify` says after `n` steps of `phase`: a line every
/// [`PROGRESS_EVERY`], in every phase, so a long history never looks like
/// a hang — not during the walk, nor the compare or the segment reads
/// after it.
fn progress_line(phase: VerifyPhase, n: usize) -> Option<String> {
    (n > 0 && n.is_multiple_of(PROGRESS_EVERY)).then(|| match phase {
        VerifyPhase::Walk => format!("verify: walked back {n} commits so far"),
        VerifyPhase::Compare => format!("verify: compared {n} pairs of commits so far"),
        VerifyPhase::Segments => format!("verify: read {n} segments so far"),
    })
}

/// `fl github ledger verify` (spec §3.5). Exit 1 when it found anything.
fn verify(gl: &GithubLedger<'_>, max_commits: usize) -> Result<i32> {
    // A limit of 0 could never verify anything.
    if max_commits == 0 {
        bail!("`--max-commits` must be at least 1: a walk of no commits verifies nothing");
    }
    let v = gl.verify_with(max_commits, &mut |phase, n| {
        if let Some(line) = progress_line(phase, n) {
            eprintln!("{line}");
        }
    })?;
    if let Some(bad) = &v.first_bad {
        println!("BAD\t{}\t{}", bad.commit, bad.what);
    }
    if let Some(same) = &v.same_id {
        println!(
            "SAME ID\t{}\t`{}` line {}\t`{}` line {}",
            same.id, same.first.0, same.first.1, same.second.0, same.second.1
        );
    }
    if v.first_bad.is_none() && v.same_id.is_none() {
        println!(
            "verified\t{} commits\teach only adds, and no id is on two lines",
            v.commits
        );
        return Ok(0);
    }
    println!("walked\t{} commits", v.commits);
    Ok(1)
}

/// `fl github ledger quarantine` (spec §3.6).
///
/// ⚠ Decision 16: decision 2's projection covers the entries, not the
/// text a person writes to publish. Said every time; warned before the
/// append where the repository is not private.
fn quarantine(gl: &GithubLedger<'_>, file: &str, line: u64, by: &str, reason: &str) -> Result<i32> {
    let repo = &gl.repo().full_name;
    match gl.visibility()? {
        // Said before the library checks the arguments, so worded for the
        // append that may not happen. ⚠ Its own prefix: a read's notes are
        // `note:`.
        Visibility::Private => eprintln!(
            "permanent: once appended, `--by` and `--reason` are written to the ledger of \
             {repo} permanently; nothing is ever removed from it"
        ),
        Visibility::NotPrivate => eprintln!(
            "warning: {repo} is not private: once appended, `--by` and `--reason` are \
             published permanently — anyone who can read {repo} can read them, and nothing \
             is ever removed from its ledger"
        ),
    }
    let commit = gl.quarantine(&stamp::entry_id(), &stamp::now(), file, line, by, reason)?;
    // A fresh id always adds a line, so `quarantine` names a commit; `None`
    // would mean nothing was added, which the library allows only for an id
    // already present.
    let commit = commit.unwrap_or_else(|| "no commit: the line was already there".into());
    println!("quarantined\t{file}\tline {line}\t{commit}");
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

    // A long walk reports every hundred steps, in every phase, and only
    // then.
    #[test]
    fn progress_is_reported_every_hundred_commits() {
        // 10 and 110: a multiple of a smaller step is not a hundredth step.
        for n in [0, 1, 10, 99, 101, 110, 199] {
            assert_eq!(progress_line(VerifyPhase::Walk, n), None, "{n}");
        }
        assert_eq!(
            progress_line(VerifyPhase::Walk, 100).as_deref(),
            Some("verify: walked back 100 commits so far")
        );
        assert!(progress_line(VerifyPhase::Walk, 200).is_some());
        assert_eq!(
            progress_line(VerifyPhase::Compare, 100).as_deref(),
            Some("verify: compared 100 pairs of commits so far")
        );
        assert_eq!(
            progress_line(VerifyPhase::Segments, 100).as_deref(),
            Some("verify: read 100 segments so far")
        );
        for phase in [VerifyPhase::Compare, VerifyPhase::Segments] {
            assert_eq!(progress_line(phase, 99), None, "{phase:?}");
        }
    }
}
