//! `fl github ledger` (GitHub ledger spec §3.5, §3.6, §4.3, §6.1): set up
//! the GitHub ledger, walk its history, quarantine a line, and post the
//! decision comments an item's issue is missing. Each is run by a person,
//! by hand.

use crate::ctx::Ctx;
use crate::refs::Ref;
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::decision::Outcome;
use fl_exec::stamp;
use fl_github::GithubLedger;
use fl_github::ledger::{InitOutcome, Mode, VERIFY_LIMIT, VerifyPhase, Visibility, guidance};
use fl_github::ledger::{Published, render};
use std::collections::BTreeSet;
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
    /// Post every decision comment missing from a record's or a finding's
    /// issue, rendered from the ledger. Safe to run again: a decision whose
    /// comment is there is skipped.
    Comment {
        /// The record or finding: its issue number (`41` or `#41`),
        /// `owner/repo#41`, or its URL.
        item: Ref,
    },
}

impl Cmd {
    /// Every item this command names, by `Ref`.
    pub fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Comment { item } => vec![item],
            Cmd::Init { .. } | Cmd::Verify { .. } | Cmd::Quarantine { .. } => vec![],
        }
    }
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
        Cmd::Comment { item } => comment(ctx, gl, &item),
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

/// `fl github ledger comment <item>` (spec §4.3): every decision filed
/// under the item whose comment no comment on its issue marks — all pages
/// read — rendered from the ledger and posted where the issue is now,
/// oldest first.
fn comment(ctx: &Ctx<'_>, gl: &GithubLedger<'_>, item: &Ref) -> Result<i32> {
    let Some(gh) = ctx.github else {
        bail!("`fl github ledger comment` needs the project bound to a GitHub repository");
    };
    let iri = match item {
        Ref::Handle(n) => gh.issue_url(*n),
        Ref::Iri(i) => i.clone(),
    };
    let at = gl.issue_at(&iri)?;
    // ⚠ GitHub redirects a transferred issue, and also one whose repository
    // was renamed; the address says where it is, not why it moved.
    if let Some(to) = &at.moved_to {
        println!("moved\t{to}");
    }
    let (head, published) = gl.published_decisions(&iri)?;
    // Whose comments may mark a decision here: fl's own login, which
    // `posted` adds, and everyone who wrote a decision under the item (spec
    // §4.3; a colleague's machine posts under its own login).
    let writers: BTreeSet<String> = published.iter().map(|p| p.by.clone()).collect();
    let posted = gl.posted(&at, &writers)?;
    let missing: Vec<&Published> = published
        .iter()
        .filter(|p| !posted.contains(&p.decision.id))
        .collect();
    let already = published.len() - missing.len();
    // ⚠ A decision whose id fl does not write gets no comment: its marker
    // could close the HTML comment early (`render::markable`). Said, and
    // never posted.
    let (missing, unmarkable): (Vec<&Published>, Vec<&Published>) = missing
        .into_iter()
        .partition(|p| render::markable(&p.decision.id));
    for p in &unmarkable {
        eprintln!(
            "skipped\t{}\tits id is not one fl writes, so it gets no comment; quarantine its \
             line (`fl github ledger quarantine {} {} --by <name> --reason <text>`)",
            p.decision.id.as_str().escape_debug(),
            p.file,
            p.line
        );
    }
    // Nothing missing: nothing more to read.
    if !missing.is_empty() {
        let cat = crate::comment::catalogued(ctx.store, &at.project)?;
        let mut gates = Vec::new();
        for p in &missing {
            for g in render::candidate_gates(&p.decision.outcome, &cat) {
                if !gates.contains(&g) {
                    gates.push(g);
                }
            }
        }
        let runs = gl.runs_of(&gates)?;
        // The project's attempts are read only when an attempt's comment is
        // missing.
        let attempts = if missing
            .iter()
            .any(|p| matches!(p.decision.outcome, Outcome::Attempt { .. }))
        {
            gl.attempts_of(&at.project)?
        } else {
            Vec::new()
        };
        let visibility = gl.visibility()?;
        let repo = &gl.repo().full_name;
        for p in &missing {
            let view = render::view(
                p.decision.clone(),
                p.by.clone(),
                gl.commit_of(&head, p),
                &runs,
                &attempts,
                &cat,
            );
            // No state line: the ledger does not record whether the state
            // change completed.
            gl.post_at(&at, &render::render(&view, repo, visibility, None))?;
            println!("posted\t{}", p.decision.id);
        }
    }
    println!(
        "comments\t{} posted, {already} already there",
        missing.len()
    );
    Ok(if unmarkable.is_empty() { 0 } else { 1 })
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
            witness: None,
            tracker_label: String::new(),
        };
        let err = run(&ctx, Cmd::Init { confirm: None }, None).unwrap_err();
        assert!(
            err.to_string().contains("no config entry names one here"),
            "{err:#}"
        );
        assert_eq!(fake.ledger_head(), None, "nothing was created");
    }

    // The item `comment` names reaches the store's choice and the handle
    // check, as every command's items do.
    #[test]
    fn the_item_comment_names_is_one_of_its_refs() {
        use crate::cmd::github::Cmd as Github;
        assert!(
            Github::Ledger(Cmd::Comment {
                item: Ref::Handle(3)
            })
            .has_handle()
        );
        let iri = fl_core::Iri::parse("https://github.com/acme/widgets/issues/3").unwrap();
        assert_eq!(
            Github::Ledger(Cmd::Comment {
                item: Ref::Iri(iri.clone())
            })
            .iris(),
            vec![iri]
        );
        assert!(
            Github::Ledger(Cmd::Verify { max_commits: 1 })
                .iris()
                .is_empty()
        );
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
