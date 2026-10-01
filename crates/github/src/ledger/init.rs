//! Setting up the GitHub ledger (GitHub ledger spec §6.1, §6.2): `init`,
//! recovering from a run that stopped partway; the mode in force; and what
//! `init` tells the person.

use super::GithubLedger;
use super::git::Object;
use super::layout::{BRANCH, FORMAT_FILE, README, README_FILE};
use crate::client::Method;
use fl_core::iri::Iri;
use fl_core::{LedgerFault, StoreError, node_id_shape};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// What `init` found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitOutcome {
    /// The branch was created at `root`; this machine recorded it and its
    /// cut-over.
    Created { root: String },
    /// This machine already knew the ledger's root. `cutover_recorded` says
    /// whether it had no cut-over and now has one (ruling 16).
    AlreadySetUp {
        root: String,
        cutover_recorded: bool,
    },
    /// The branch exists and this machine knows no root: the person confirms
    /// that `root` is the first commit they made, by running `init` again
    /// with it (spec §6.1 step 6). Nothing was recorded.
    Confirm { root: String },
    /// The person confirmed `root`; this machine recorded it and its
    /// cut-over.
    Adopted { root: String },
}

/// The guarantee in force on `fl/ledger` (spec §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// A ruleset refuses a rewrite and a deletion; fl detects an edit.
    Protected,
    /// Nothing refuses them; fl detects all three. `why` names what is
    /// missing.
    DetectionOnly { why: String },
}

impl Mode {
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Protected => "protected",
            Mode::DetectionOnly { .. } => "detection-only",
        }
    }
}

const RULES: [&str; 2] = ["non_fast_forward", "deletion"];

/// How many commits `first_commit` walks back before refusing — far more
/// than any real ledger grows, so hitting it means `head` is not really
/// descended from a ledger's first commit at all (an unrelated history the
/// branch name `fl/ledger` was reused for), and walking it one commit at a
/// time would be the wrong way to find out.
const MAX_WALK: u32 = 1_000;

impl GithubLedger<'_> {
    /// `fl github ledger init` (spec §6.1 steps 2–6). Run once per
    /// repository by a person, and safe to run again: it recovers from a
    /// run that stopped after any step.
    ///
    /// `cutover` is this machine's cut-over, minted by the caller; it is
    /// recorded only when the machine has none. `confirmed` is the first
    /// commit the person confirmed after an earlier `Confirm`.
    pub fn init(&self, cutover: &Iri, confirmed: Option<&str>) -> Result<InitOutcome, StoreError> {
        let repo = self.repo.full_name.clone();
        // ⚠ Ruling 20: a node id that cannot be one is refused before
        // anything is created, so a malformed binding leaves no branch.
        node_id_shape(&self.repo.node_id).map_err(|why| {
            StoreError::Backend(format!(
                "fl will not set up a ledger for {repo}: its node id {why}"
            ))
        })?;
        // ⚠ Step 2: git cannot hold both `fl` and `fl/ledger`.
        if self.branch_head("fl")?.is_some() {
            return Err(StoreError::Backend(format!(
                "the repository {repo} has a branch named `fl`, and git cannot hold both `fl` \
                 and `fl/ledger`. Rename that branch, then run `fl github ledger init` again"
            )));
        }
        match (
            self.branch_head(BRANCH)?,
            self.local.ledger_root(&self.repo.node_id)?,
        ) {
            // ⚠ Step 6: the ledger was deleted. A new one would hide that.
            (None, Some(root)) => Err(LedgerFault::Deleted { repo, root }.into()),
            (None, None) => {
                let root = self.create_branch()?;
                self.record(cutover, &root)?;
                Ok(InitOutcome::Created { root })
            }
            (Some(_), Some(root)) => {
                self.check_head()?;
                let cutover_recorded = self.record_cutover(cutover)?;
                Ok(InitOutcome::AlreadySetUp {
                    root,
                    cutover_recorded,
                })
            }
            // ⚠ Step 6: the branch exists but no root was recorded — `init`
            // stopped after creating it. Ask the person to confirm its first
            // commit is the one they created.
            (Some(head), None) => {
                let first = self.first_commit(&head)?;
                match confirmed {
                    None => Ok(InitOutcome::Confirm { root: first }),
                    Some(c) if c == first => {
                        self.record(cutover, &first)?;
                        Ok(InitOutcome::Adopted { root: first })
                    }
                    Some(c) => Err(StoreError::Backend(format!(
                        "`{c}` is not the first commit of {repo}'s `fl/ledger` branch; {first} \
                         is. Confirm that commit, or find out who started the ledger"
                    ))),
                }
            }
        }
    }

    /// Step 3: `format` and `README.md` in a commit with no parent — through
    /// the REST Git Data API, because `createCommitOnBranch` needs a branch
    /// that exists — and the branch at it.
    fn create_branch(&self) -> Result<String, StoreError> {
        let tree = self.created(
            "/git/trees",
            json!({"tree": [
                {"path": FORMAT_FILE, "mode": "100644", "type": "blob", "content": "1\n"},
                {"path": README_FILE, "mode": "100644", "type": "blob", "content": README},
            ]}),
            "the ledger's first tree",
        )?;
        let commit = self.created(
            "/git/commits",
            json!({"message": "fl: start the ledger", "tree": tree, "parents": []}),
            "the ledger's first commit",
        )?;
        let made = self.client.send(
            Method::Post,
            &self.path("/git/refs"),
            Some(&json!({"ref": format!("refs/heads/{BRANCH}"), "sha": commit})),
        );
        match made {
            Ok(r) if r.status == 201 => Ok(commit),
            Ok(r) => Err(StoreError::Backend(format!(
                "GitHub answered {} when fl created the branch `fl/ledger`; nothing was \
                 recorded. Run `fl github ledger init` again",
                r.status
            ))),
            // ⚠ Modelled: a ref that exists is refused with 422 "Reference
            // already exists". Confirmed by live test
            // `init_sets_up_a_ledger_on_a_private_repository`.
            Err(StoreError::Backend(m)) if m.contains("already exists") => {
                Err(StoreError::Backend(format!(
                    "someone created `fl/ledger` in {} while init ran, so this machine \
                     recorded nothing. Run `fl github ledger init` again: it asks you to \
                     confirm that branch's first commit",
                    self.repo.full_name
                )))
            }
            Err(e) => Err(e),
        }
    }

    /// The id of what a POST to `rest` created.
    fn created(&self, rest: &str, body: Value, what: &str) -> Result<String, StoreError> {
        let r = self
            .client
            .send(Method::Post, &self.path(rest), Some(&body))?;
        if r.status != 201 {
            return Err(StoreError::Backend(format!(
                "GitHub answered {} when fl created {what}; nothing was recorded. Run `fl \
                 github ledger init` again",
                r.status
            )));
        }
        r.body
            .get("sha")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| StoreError::Backend(format!("GitHub created {what} but named no id")))
    }

    /// Steps 4 and 5: this machine's cut-over if it has none, then the
    /// root. The root is GitHub's own commit id; the manifest's consistency
    /// check refuses a malformed one at export.
    ///
    /// ⚠ In that order (ruling 16): a root on record means the cut-over is
    /// too, and a run that stopped between the two keeps its cut-over.
    fn record(&self, cutover: &Iri, root: &str) -> Result<(), StoreError> {
        self.record_cutover(cutover)?;
        self.local.set_ledger_root(&self.repo.node_id, root)
    }

    /// Whether this machine had no cut-over and now has `cutover` (spec §6.1
    /// step 5). ⚠ An existing cut-over never moves.
    fn record_cutover(&self, cutover: &Iri) -> Result<bool, StoreError> {
        if self.local.cutover(&self.repo.node_id)?.is_some() {
            return Ok(false);
        }
        self.local.set_cutover(&self.repo.node_id, cutover)?;
        Ok(true)
    }

    /// The branch's first commit: first parents from `head` back to the
    /// commit with none, bounded by [`MAX_WALK`], and checked to look like
    /// a ledger root before it is offered for confirmation.
    fn first_commit(&self, head: &str) -> Result<String, StoreError> {
        let mut at = head.to_string();
        for _ in 0..MAX_WALK {
            match self.parents_of(&at)?.into_iter().next() {
                Some(p) => at = p,
                None => {
                    self.check_looks_like_a_root(&at)?;
                    return Ok(at);
                }
            }
        }
        Err(StoreError::Backend(format!(
            "fl walked {MAX_WALK} commits back from {}'s `fl/ledger` branch without finding one \
             with no parent, so this does not look like a ledger fl started; import the \
             project's manifest, which carries the ledger's first commit, instead of confirming \
             one found this way",
            self.repo.full_name
        )))
    }

    /// ⚠ A commit with no parent is not, on its own, proof it is a ledger's
    /// first commit — any orphan would pass that test. Before `init` offers
    /// it for confirmation (spec §6.1 step 6), it must also hold the
    /// `format` file every ledger's first commit carries, so a branch named
    /// `fl/ledger` for an unrelated reason is refused rather than adopted.
    fn check_looks_like_a_root(&self, commit: &str) -> Result<(), StoreError> {
        let found = self.objects(commit, &[FORMAT_FILE.to_string()])?;
        if !matches!(found[0], Some(Object::Blob { .. })) {
            return Err(StoreError::Backend(format!(
                "the first commit of {}'s `fl/ledger` branch ({commit}) has no `format` file, so \
                 it does not look like a ledger fl started. Find out who created this branch \
                 before confirming it",
                self.repo.full_name
            )));
        }
        Ok(())
    }

    /// The mode in force (spec §6.2), from the rules GitHub applies to
    /// `fl/ledger`.
    ///
    /// ⚠ Modelled: `rules/branches` lists only the rules in force, so a
    /// disabled or evaluate-only ruleset shows as the rules missing; a plan
    /// without rulesets answers 403 with an upgrade message. Confirmed by
    /// live tests `rules_on_the_ledger_branch_are_readable` and
    /// `a_private_repository_without_a_ruleset_is_detection_only`.
    pub fn mode(&self) -> Result<Mode, StoreError> {
        let r = match self.client.send(
            Method::Get,
            &self.path(&format!("/rules/branches/{BRANCH}")),
            None,
        ) {
            Ok(r) => r,
            Err(StoreError::Backend(m)) if m.contains("Upgrade to GitHub") => {
                return Ok(Mode::DetectionOnly {
                    why: "rulesets are not available on this repository's plan, so nothing \
                          stops a rewrite or a deletion; fl detects them"
                        .into(),
                });
            }
            Err(e) => return Err(e),
        };
        if r.status != 200 {
            return Err(StoreError::Backend(format!(
                "GitHub answered {} when fl read the rules on `fl/ledger`; retry",
                r.status
            )));
        }
        // ⚠ Like the sibling readers (`objects`'s trees, `get_all_paged`'s
        // pages): something that is not a list is an error, never read as
        // an empty one — an empty list and an unreadable answer are
        // different facts, and conflating them would turn "fl cannot tell
        // what rules apply" into "no rules apply".
        let Some(rules) = r.body.as_array() else {
            return Err(StoreError::Backend(
                "GitHub answered the rules on `fl/ledger` with something that is not a list; \
                 retry"
                    .into(),
            ));
        };
        let in_force: BTreeSet<&str> = rules
            .iter()
            .filter_map(|x| x.get("type").and_then(Value::as_str))
            .collect();
        let missing: Vec<String> = RULES
            .iter()
            .filter(|t| !in_force.contains(**t))
            .map(|t| format!("`{t}`"))
            .collect();
        if missing.is_empty() {
            return Ok(Mode::Protected);
        }
        Ok(Mode::DetectionOnly {
            why: format!(
                "no active ruleset on `fl/ledger` has {} (a ruleset that is disabled or only \
                 evaluating applies none), so nothing stops a rewrite or a deletion; fl \
                 detects them",
                missing.join(" or ")
            ),
        })
    }
}

/// What `init` tells the person once the ledger is set up (spec §6.1 steps
/// 7–9, §6.2, §6.3), one paragraph each, for the command to print.
pub fn guidance(repo: &str, mode: &Mode) -> Vec<String> {
    let mut out = vec![match mode {
        Mode::Protected => "mode: protected. A ruleset on `fl/ledger` refuses a rewrite or a \
                            deletion, and fl detects an edit."
            .to_string(),
        Mode::DetectionOnly { why } => format!("mode: detection-only. {why}."),
    }];
    if *mode != Mode::Protected {
        out.push(format!(
            "Optional, for an administrator: protect `fl/ledger` with a ruleset. It is \
             recommended where the plan offers rulesets; on GitHub Free, for a private \
             repository, it is not available, and the ledger runs detection-only. fl's \
             credential must not hold Administration permission, so fl cannot add it \
             itself:\n\n{}",
            ruleset_command(repo)
        ));
    }
    out.push(format!(
        "Protect the default branch of {repo} as well: Contents: write lets fl's credential \
         push to any branch."
    ));
    out.push(
        "fl's credential needs Contents: read and write, Issues: read and write, and Metadata: \
         read. A missing write permission shows at the first flush or comment, whose error \
         names it; until it is granted, every move, check and finding decision is refused, and \
         each attempt is kept locally with a repeated error."
            .to_string(),
    );
    out.push(format!(
        "Disclosure: on a private repository the ledger keeps output excerpts. If {repo} is \
         ever made public, every excerpt in the ledger's history becomes public, and removing \
         one would take the history rewrite the ledger exists to forbid."
    ));
    out
}

/// The administrator's ruleset for `fl/ledger`, as a ready `gh api`
/// command (spec §6.1 step 7).
pub fn ruleset_command(repo: &str) -> String {
    let body = json!({
        "name": "fl ledger",
        "target": "branch",
        "enforcement": "active",
        "conditions": {"ref_name": {"include": [format!("refs/heads/{BRANCH}")], "exclude": []}},
        "rules": RULES.iter().map(|r| json!({"type": r})).collect::<Vec<_>>(),
    });
    format!("gh api --method POST repos/{repo}/rulesets --input - <<'JSON'\n{body}\nJSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use crate::fake_git::Ruleset;
    use crate::tracker::Repo;
    use fl_core::MemStore;
    use fl_core::conformance::entry_iri;
    use fl_core::ids::{GateId, seq_iri};
    use fl_core::split::{CachedSegment, LedgerCache, Outbox, Pending};
    use fl_core::store::Bindings;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    fn repo() -> Repo {
        Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        }
    }

    fn open<'a>(c: &'a Client, local: &'a dyn fl_core::split::LedgerMemory) -> GithubLedger<'a> {
        GithubLedger::new(c, repo(), local).with_lag(0, Duration::ZERO)
    }

    /// The next request whose URL contains `frag` answers `status`/`body`
    /// instead, once.
    fn body_next(fake: &FakeGithub, frag: &str, status: u16, body: Value) {
        fake.state().body_next = Some((frag.into(), status, body));
    }

    // Spec §6.1 steps 3 and 4.
    #[test]
    fn init_creates_the_branch_and_records_its_root_and_this_machines_cut_over() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        let l = open(&c, &local);
        let InitOutcome::Created { root } = l.init(&entry_iri(0), None).unwrap() else {
            panic!("a fresh repository gets a new ledger");
        };
        let files = fake.ledger_files();
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![README_FILE, FORMAT_FILE],
            "README.md and format (in path order), and no `.github/`"
        );
        assert_eq!(files[FORMAT_FILE], "1\n");
        assert!(
            fake.state().git.commits[&root].parents.is_empty(),
            "an orphan"
        );
        assert_eq!(fake.ledger_head(), Some(root.clone()));
        assert_eq!(local.ledger_root("R_1").unwrap(), Some(root));
        assert_eq!(local.cutover("R_1").unwrap(), Some(entry_iri(0)));
        assert!(
            l.runs(&GateId(seq_iri(1))).unwrap().is_empty(),
            "and it reads"
        );
    }

    #[test]
    fn init_run_again_says_the_ledger_is_set_up_and_moves_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        let InitOutcome::Created { root } = open(&c, &local).init(&entry_iri(0), None).unwrap()
        else {
            panic!("created");
        };
        assert_eq!(
            open(&c, &local).init(&entry_iri(7), None).unwrap(),
            InitOutcome::AlreadySetUp {
                root,
                cutover_recorded: false,
            }
        );
        assert_eq!(
            local.cutover("R_1").unwrap(),
            Some(entry_iri(0)),
            "never moved"
        );
        assert_eq!(fake.ledger_commits(), 1);
    }

    // ⚠ Spec §6.1 step 5 (ruling 16): a machine that imported the root has no
    // cut-over; init records one, or its flushes would publish nothing.
    #[test]
    fn a_machine_that_imported_the_root_records_its_own_cut_over() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).init(&entry_iri(5), None).unwrap(),
            InitOutcome::AlreadySetUp {
                root,
                cutover_recorded: true,
            }
        );
        assert_eq!(local.cutover("R_1").unwrap(), Some(entry_iri(5)));
    }

    // Spec §6.1 step 2.
    #[test]
    fn init_refuses_when_a_branch_named_fl_exists() {
        let fake = FakeGithub::start("acme/widgets");
        let commit = fake.seed_ledger();
        fake.delete_ledger();
        fake.state().git.refs.insert("heads/fl".into(), commit);
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(err.to_string().contains("named `fl`"), "{err}");
        assert_eq!(fake.ledger_head(), None, "nothing created");
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
    }

    // Spec §6.1 step 6: init stopped after creating the branch.
    #[test]
    fn init_that_stopped_after_the_branch_asks_to_confirm_its_first_commit() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.hand_commit(&[("runs/k/1.jsonl", Some("x\n"))]);
        let local = MemStore::default();
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).init(&entry_iri(0), None).unwrap(),
            InitOutcome::Confirm { root: root.clone() }
        );
        assert_eq!(
            local.ledger_root("R_1").unwrap(),
            None,
            "nothing until confirmed"
        );
        let err = open(&c, &local)
            .init(
                &entry_iri(0),
                Some("0123456789abcdef0123456789abcdef01234567"),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains(&root),
            "names the real first commit: {err}"
        );
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
        assert_eq!(
            open(&c, &local).init(&entry_iri(0), Some(&root)).unwrap(),
            InitOutcome::Adopted { root: root.clone() }
        );
        assert_eq!(local.ledger_root("R_1").unwrap(), Some(root));
        assert_eq!(local.cutover("R_1").unwrap(), Some(entry_iri(0)));
    }

    // Ruling 16: a cut-over recorded by a run that stopped before the root
    // is kept, never replaced.
    #[test]
    fn init_that_stopped_between_the_cut_over_and_the_root_keeps_the_cut_over() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        local.set_cutover("R_1", &entry_iri(3)).unwrap();
        let c = client(&fake);
        assert_eq!(
            open(&c, &local).init(&entry_iri(9), Some(&root)).unwrap(),
            InitOutcome::Adopted { root }
        );
        assert_eq!(local.cutover("R_1").unwrap(), Some(entry_iri(3)));
    }

    // Ruling: a commit with no parent is not, on its own, proof it is a
    // ledger's first commit. A branch named `fl/ledger` for an unrelated
    // reason — no `format` file in its root — must be refused rather than
    // offered for confirmation.
    #[test]
    fn init_refuses_to_confirm_a_first_commit_that_does_not_look_like_a_ledger_root() {
        let fake = FakeGithub::start("acme/widgets");
        let bogus = fake.seed_ledger_with(&[("README.md", "not a ledger")]);
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(err.to_string().contains(&bogus), "{err}");
        assert!(
            err.to_string().contains("does not look like a ledger"),
            "{err}"
        );
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
    }

    // `first_commit` must not walk forever looking for a commit with no
    // parent: a history this long is not really a ledger whose root fl
    // should ever confirm by walking to it one commit at a time.
    #[test]
    fn first_commit_refuses_a_history_longer_than_the_walk_limit() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        {
            let mut s = fake.state();
            let mut at = root.clone();
            for _ in 0..=MAX_WALK {
                let tree = s.git.put_tree(&BTreeMap::new());
                at = s.git.put_commit(&tree, vec![at], "filler");
            }
            s.git.refs.insert(format!("heads/{BRANCH}"), at);
        }
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(
            err.to_string().contains("import the project's manifest"),
            "{err}"
        );
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
    }

    #[test]
    fn init_whose_branch_could_not_be_created_records_nothing_and_a_rerun_creates_it() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().fail_next_ref_create = true;
        let local = MemStore::default();
        let c = client(&fake);
        assert!(open(&c, &local).init(&entry_iri(0), None).is_err());
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
        assert_eq!(local.cutover("R_1").unwrap(), None);
        assert!(matches!(
            open(&c, &local).init(&entry_iri(0), None).unwrap(),
            InitOutcome::Created { .. }
        ));
    }

    #[test]
    fn someone_creating_the_branch_while_init_runs_leaves_this_machine_unrecorded() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().race_next_ref_create = true;
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(err.to_string().contains("while init ran"), "{err}");
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
        let theirs = fake.ledger_head().unwrap();
        assert_eq!(
            open(&c, &local).init(&entry_iri(0), None).unwrap(),
            InitOutcome::Confirm { root: theirs }
        );
    }

    // Spec §6.1 step 6 and §7: a deleted ledger is refused, never replaced.
    #[test]
    fn init_refuses_a_ledger_that_was_deleted() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.delete_ledger();
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Deleted { .. })),
            "{err:?}"
        );
        assert_eq!(fake.ledger_head(), None, "no new ledger hides the deletion");
    }

    #[test]
    fn init_refuses_a_rewritten_ledger() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.rewrite_ledger(&[("format", "1\n")]);
        let local = MemStore::default();
        local.set_ledger_root("R_1", &root).unwrap();
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(
            matches!(err, StoreError::Ledger(LedgerFault::Rewritten { .. })),
            "{err:?}"
        );
    }

    // Ruling 20: init checks the repository's node id before it creates
    // anything, so a malformed binding leaves no branch behind.
    #[test]
    fn init_refuses_a_node_id_that_cannot_be_one_before_creating_anything() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        let bad = GithubLedger::new(
            &c,
            Repo {
                full_name: "acme/widgets".into(),
                node_id: "1 not a node".into(),
            },
            &local,
        );
        let err = bad.init(&entry_iri(0), None).unwrap_err();
        assert!(err.to_string().contains("is not a GitHub node id"), "{err}");
        assert_eq!(fake.ledger_head(), None, "no branch left behind");
        assert!(fake.state().requests.is_empty(), "not one request");
        assert_eq!(local.cutover("1 not a node").unwrap(), None);
    }

    #[test]
    fn init_whose_first_tree_or_commit_was_not_created_records_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        fake.state().fail_next_git_create = true;
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(err.to_string().contains("answered 500"), "{err}");
        assert_eq!(fake.ledger_head(), None);
        assert_eq!(local.cutover("R_1").unwrap(), None);
    }

    // ⚠ A 201 that creates something but names no id must be refused, never
    // read as if the id were simply absent (empty) — a garbled answer is a
    // different fact from "GitHub created nothing".
    #[test]
    fn init_whose_first_tree_was_created_with_no_id_records_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        body_next(&fake, "/git/trees", 201, json!({}));
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(err.to_string().contains("named no id"), "{err}");
        assert_eq!(fake.ledger_head(), None, "no branch left behind");
        assert_eq!(local.ledger_root("R_1").unwrap(), None);
        assert_eq!(local.cutover("R_1").unwrap(), None);
    }

    #[test]
    fn a_mode_that_cannot_be_read_is_an_error_not_detection_only() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        fake.state().fail_rules_next = true;
        let local = MemStore::default();
        let c = client(&fake);
        let err = open(&c, &local).mode().unwrap_err();
        assert!(err.to_string().contains("answered 500"), "{err}");
    }

    // ⚠ A 200 that is not a list must be an error, like the sibling readers
    // (`objects`'s trees, `get_all_paged`'s pages) — never read as "no
    // active ruleset", which would hide that fl could not read the rules at
    // all behind the detection-only answer a real absence of rules gives.
    #[test]
    fn a_mode_whose_rules_are_not_a_list_is_an_error_not_detection_only() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        body_next(&fake, "/rules/branches/fl/ledger", 200, json!({}));
        let err = open(&c, &local).mode().unwrap_err();
        assert!(err.to_string().contains("not a list"), "{err}");
    }

    /// A local store whose cut-over cannot be written.
    struct NoCutover<'a>(&'a MemStore);
    impl Bindings for NoCutover<'_> {
        fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError> {
            self.0.bound_node_id(repo)
        }
        fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError> {
            self.0.bind_node_id(repo, node_id)
        }
        fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError> {
            self.0.ledger_root(node_id)
        }
        fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError> {
            self.0.set_ledger_root(node_id, commit)
        }
    }
    impl LedgerCache for NoCutover<'_> {
        fn last_head(&self, repo: &str) -> Result<Option<String>, StoreError> {
            self.0.last_head(repo)
        }
        fn set_last_head(&self, repo: &str, head: &str) -> Result<(), StoreError> {
            self.0.set_last_head(repo, head)
        }
        fn cached(&self, repo: &str, path: &str) -> Result<Option<CachedSegment>, StoreError> {
            self.0.cached(repo, path)
        }
        fn cached_under(
            &self,
            repo: &str,
            dir: &str,
        ) -> Result<Vec<(String, CachedSegment)>, StoreError> {
            self.0.cached_under(repo, dir)
        }
        fn cache(&self, repo: &str, path: &str, s: &CachedSegment) -> Result<(), StoreError> {
            self.0.cache(repo, path, s)
        }
        fn remember(
            &self,
            repo: &str,
            head: &str,
            segments: &[(String, CachedSegment)],
        ) -> Result<(), StoreError> {
            self.0.remember(repo, head, segments)
        }
    }
    impl Outbox for NoCutover<'_> {
        fn unpublished(&self, repo: &str, after: &Iri) -> Result<Pending, StoreError> {
            self.0.unpublished(repo, after)
        }
        fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError> {
            self.0.is_published(repo, id)
        }
        fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError> {
            self.0.mark_published(repo, ids)
        }
        fn settle(&self, repo: &str, done: &[Iri], aside: &[Iri]) -> Result<(), StoreError> {
            self.0.settle(repo, done, aside)
        }
        fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError> {
            self.0.cutover(repo)
        }
        fn set_cutover(&self, _: &str, _: &Iri) -> Result<(), StoreError> {
            Err(StoreError::Backend("the disk is full".into()))
        }
    }

    // ⚠ Ruling 16: the cut-over is recorded before the root, so a root on
    // record means a cut-over is too — a failed cut-over leaves no root.
    #[test]
    fn a_cut_over_that_cannot_be_recorded_leaves_no_root() {
        let fake = FakeGithub::start("acme/widgets");
        let store = MemStore::default();
        let local = NoCutover(&store);
        let c = client(&fake);
        let err = open(&c, &local).init(&entry_iri(0), None).unwrap_err();
        assert!(err.to_string().contains("the disk is full"), "{err}");
        assert_eq!(store.ledger_root("R_1").unwrap(), None);
        assert!(
            fake.ledger_head().is_some(),
            "the branch was created on GitHub before the local write failed"
        );
    }

    // Spec §6.2: protected only with both rules in force.
    #[test]
    fn the_mode_is_protected_only_when_both_rules_are_in_force() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        let mode = |rulesets: Vec<Ruleset>| {
            fake.state().rulesets = rulesets;
            open(&c, &local).mode().unwrap()
        };
        let both = ["non_fast_forward", "deletion"];
        assert_eq!(
            mode(vec![Ruleset::on_ledger("active", &both)]),
            Mode::Protected
        );
        assert_eq!(
            mode(vec![
                Ruleset::on_ledger("active", &["non_fast_forward"]),
                Ruleset::on_ledger("active", &["deletion"]),
            ]),
            Mode::Protected,
            "two rulesets may share the two rules"
        );
        for (rulesets, names) in [
            (vec![], vec!["`non_fast_forward`", "`deletion`"]),
            (
                vec![Ruleset::on_ledger("active", &["non_fast_forward"])],
                vec!["`deletion`"],
            ),
            (
                vec![Ruleset::on_ledger("disabled", &both)],
                vec!["`non_fast_forward`", "`deletion`"],
            ),
            (
                vec![Ruleset::on_ledger("evaluate", &both)],
                vec!["`non_fast_forward`", "`deletion`"],
            ),
        ] {
            match mode(rulesets) {
                Mode::DetectionOnly { why } => {
                    for n in &names {
                        assert!(why.contains(n), "{why}");
                    }
                    if names.len() == 1 {
                        assert!(
                            !why.contains("`non_fast_forward`"),
                            "names only what is missing: {why}"
                        );
                    }
                }
                Mode::Protected => panic!("{names:?} missing, yet protected"),
            }
        }
        fake.state().rulesets = vec![];
        fake.state().rules_need_upgrade = true;
        match open(&c, &local).mode().unwrap() {
            Mode::DetectionOnly { why } => assert!(why.contains("plan"), "{why}"),
            Mode::Protected => panic!("no rulesets on the plan, yet protected"),
        }
        assert_eq!(Mode::Protected.name(), "protected");
    }

    // Spec §6.1 steps 7-9 and §6.3.
    #[test]
    fn the_guidance_names_the_mode_the_ruleset_the_default_branch_the_permissions_and_the_limit() {
        let detection = Mode::DetectionOnly {
            why: "no active ruleset".into(),
        };
        let all = guidance("acme/widgets", &detection).join("\n");
        for part in [
            "detection-only",
            "gh api --method POST repos/acme/widgets/rulesets",
            "default branch",
            "Contents: read and write",
            "Issues: read and write",
            "Metadata: read",
            "made public",
        ] {
            assert!(all.contains(part), "{part}: {all}");
        }
        let protected = guidance("acme/widgets", &Mode::Protected).join("\n");
        assert!(protected.contains("protected"), "{protected}");
        assert!(
            !protected.contains("gh api"),
            "no ruleset to add: {protected}"
        );

        let command = ruleset_command("acme/widgets");
        let body = command
            .split_once('\n')
            .and_then(|(_, rest)| rest.rsplit_once("\nJSON"))
            .map(|(json, _)| json)
            .expect("a heredoc body");
        let v: Value = serde_json::from_str(body).expect("the ruleset is JSON");
        // ⚠ Pinned exactly: a broadened `include`, or a `target` other than
        // `branch`, would hand an administrator a command that blocks a
        // force-push or a deletion on more than `fl/ledger` — or on no
        // branch at all.
        assert_eq!(v["target"], "branch");
        assert_eq!(
            v["conditions"],
            json!({"ref_name": {"include": ["refs/heads/fl/ledger"], "exclude": []}})
        );
        assert_eq!(v["enforcement"], "active");
        let rules: Vec<&str> = v["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["type"].as_str().unwrap())
            .collect();
        assert_eq!(rules, vec!["non_fast_forward", "deletion"]);
    }
}
