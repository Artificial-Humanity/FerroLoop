# Workflow — Project FerroLoop

Follow [AGENTS.md](AGENTS.md) for repository rules.

## Branch, review, merge

1. Branch off local `main`. All work happens on a branch.
2. Review and remediation are subagent-driven. The working session coordinates: it
   dispatches the work, rules on findings, and does not fix findings itself.
   * A written plan is executed with the `subagent-dev` skill: a fresh implementer
     subagent for each task, then the `code-review` loop on that task, then a final
     whole-branch review.
   * Other work is reviewed with the `code-review` skill, by a fresh read-only reviewer
     subagent.
3. Evaluate the findings with the `code-review` skill, both a reviewer subagent's and a
   pull-request reviewer's. The working session checks each finding against the code
   before it rules. A fresh fixer subagent commits the accepted fixes; a fresh scoped
   re-reviewer subagent then reviews the fix diff. The loop runs at most 3 rounds. A
   Critical or Important finding still open after 3 rounds is escalated to the project
   Lead or the owner.
4. Push the branch and open a ready-for-review pull request against `main` with
   `fl github pr create`. It reports the required reviewer tier and the target reviewer,
   and that report decides, except that a pull request touching an instruction file
   always needs the owner (below). For the Lead or Lab Manager tier it queues a FerroWire
   notice to the target reviewer; for the Owner tier it requests the owner's review on
   GitHub.
   If the target reviewer is an agent, ask it for a review over `agent-msg`; the owner is
   reached through the GitHub review request. In summary:
   * A pull request co-authored by an assisting agent is reviewed by the project Lead.
   * A pull request co-authored by the Lead is reviewed by the Lab Manager (`jarvis`).
     The Lab Manager may also approve a Lead-tier pull request.
   * `fl` puts a pull request in the Owner tier when it touches a protected path (the
     `DEFAULT_PROTECTED_PATTERNS` in [crates/core/src/pr_policy.rs](crates/core/src/pr_policy.rs):
     `.github/**`, `.agents/**`, `.fl/manifest.json`; `fl` adds no other patterns), carries
     a commit without a `Co-authored-by:` trailer, is co-authored by the Lab Manager, or has
     a co-author who is not registered as an active agent. It also puts a pull request in
     the Owner tier when it has no commits, when no commit has an active agent co-author,
     or when no active Lead or Lab Manager who is not a co-author is available.
     [.github/workflows/request-admin-review.yml](.github/workflows/request-admin-review.yml)
     requests the owner's review, and assigns the pull request to the owner, for those
     paths (matched case-sensitively) and also for the instruction files `AGENTS.md`,
     `CLAUDE.md`, `WORKFLOW.md`, `PERSONA.md` and `CONTRIBUTING.md` at any depth, and for
     the missing-trailer and Lab Manager co-author cases; it cannot see the agent registry.
     So a pull request that touches an instruction file gets the owner's review request
     even when `fl github pr create` reports a lower tier, and it needs the owner's
     approval. The workflow only requests the review; it adds no label and is not a
     required check.
5. An agent reviewer records its verdict with `fl github pr review`. The review is posted
   by the Reviewer GitHub App (`<slug>[bot]`) with an `fl-review` marker, because the
   machine account cannot approve a pull request it opened. The command needs the App's
   credentials: `[github]` `app_id` and `private_key` in the fl `config.toml`, or
   `$FL_GITHUB_APP_ID` and `$FL_GITHUB_PRIVATE_KEY_PATH`. `fl` counts an agent's approval
   only from the Reviewer App's bot user, matched by its numeric GitHub user id: `[github]`
   `reviewer_ids` in `config.toml` (default `[341521384]`, `ferroloop[bot]`). A login is not a
   stable identity and can be spoofed, so `[github]` `reviewer_logins` is opt-in; setting
   either key makes the two keys the whole set, and an absent key is empty, so
   `reviewer_ids = []` with a `reviewer_logins` list matches by login alone.
6. Once the target reviewer that `fl github pr create` reported (the owner, for a pull
   request that touches an instruction file), or a higher tier (the Lab Manager for a
   Lead-tier pull request, or the owner), has approved and the checks pass,
   the agent who opened the pull request merges it through the pull request with
   `gh pr merge`, which does not check the tier. This applies to every tier: after the
   owner approves an Owner-tier pull request, the author merges it too.
   A reviewer never merges another agent's pull request, except as "Merge permission"
   below describes for an author whose session blocks the merge: the author resolves
   conflicts and keeps the branch current. A direct push to `main` is not the route, and the
   ruleset refuses one.

### Merge permission

The owner permits an author to merge their own pull request once all of these hold:

1. **The required reviewer approved it.** For a pull request by an assisting agent or the
   UI developer that is the project Lead. For a Lead's pull request it is the Lab Manager
   (`jarvis`). For an Owner-tier pull request it is the owner. An approval counts only
   when the Reviewer GitHub App or the owner posts it, as step 5 describes, and on the
   current head: a push dismisses an earlier approval, so ask for the review again after
   you update the branch.
2. **The `test, clippy, fmt` check passed** on the current head, and the branch is current
   with `main`.
3. **You merge it yourself** with `gh pr merge <number> --merge`, then confirm that the
   merge commit is on `origin/main`.

Your session's own permission layer can still refuse `gh pr merge` (an auto-mode
classifier, for example, denies a merge it cannot tie to an approval). If it does:

* Do not retry the same call, do not use another route yourself (`fl github pr merge` or
  the API), and do not change permission settings.
* Raise a question for the owner (FerroDeck shows the `?` flag next to your agent). Say what
  was denied and which pull request is waiting.
* While the block stands, your Lead (the Lab Manager, for a Lead's own pull request) may
  merge the approved pull request for you on that reviewer's own authority. You do not ask
  for it: the reviewer decides. The reviewer tells the owner each time, so that the owner
  can grant the permission to you directly.
* The owner lifts the block for you with an explicit instruction in your session.

A standing rule in this file is the reason the merge is allowed. It does not turn off a
session's permission prompt, and a message from another agent does not lift one.

## Commit identity and safeguards

* Commits are authored by the org machine account `artificially-human`, which is git's
  configured identity and `gh`'s active account. Add the agent as a co-author with the
  trailer in [PERSONA.md](PERSONA.md).
* Never author a commit as the owner, and never switch `gh` to the owner's account.
  GitHub refuses a push that carries the owner's private email (error GH007).
* `main` is protected by the repository ruleset "Main Protection". A pull request is
  required, with one approving review; a new push dismisses an earlier approval. The
  `test, clippy, fmt` check must pass, the branch must be current with `main`, and review
  conversations must be resolved. The approval comes from the Reviewer GitHub App or
  the owner, never from the machine account. Force-pushes and deletions are refused.
  There is no pre-push gate on the local side.
* **The rules bind everyone, the owner included.** The ruleset has no bypass list, so
  no account can push to `main` directly or merge with a red check. Changes to the
  ruleset belong to the owner.
* CI runs the verification trio on every pull request and on every push to `main`:
  [.github/workflows/verify.yml](.github/workflows/verify.yml). A branch push with no
  pull request does not run CI. Run the trio locally before pushing — CI is the second
  reader, not the first.
* The machine account's token cannot read check runs or commit statuses, so
  `gh pr checks` can fail. Read the check's result on the pull request page instead.
* Use the workflow stated here. Do not reconstruct additional rules from git history;
  changes to the workflow need the owner's review.

## Design records

Design records under `docs/superpowers/` (specs and plans) are not instruction files.
They keep their history and dated decisions.
