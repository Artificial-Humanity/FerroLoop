# Workflow — Project FerroLoop

Follow [AGENTS.md](AGENTS.md) for repository rules.

## Branch, review, merge

1. Branch off local `main`. All work happens on a branch.
2. Review and remediation are subagent-driven. The working session coordinates: it
   dispatches the work, rules on findings, and does not fix findings itself.
   * A written plan is executed with `superpowers:subagent-driven-development`: a fresh
     implementer and a fresh reviewer for each task, then a whole-branch review.
   * Other work is reviewed by a fresh reviewer subagent, dispatched with
     `superpowers:requesting-code-review`.
3. Evaluate the findings with `superpowers:receiving-code-review`. A subagent fixes
   them and commits the fixes; a fresh reviewer subagent then re-reviews the fix diff.
   Repeat until no Critical or Important finding is open.
4. Push the branch and open a ready-for-review pull request against `main` with
   `fl github pr create`, which works out the required reviewer tier. Then ask that
   reviewer for a review over `agent-msg`:
   * A pull request from an assisting agent is reviewed by the project Lead.
   * A pull request from the Lead is reviewed by the Lab Manager (`jarvis`).
   * The owner reviews a pull request that touches a protected path (the
     `DEFAULT_PROTECTED_PATTERNS` in [crates/core/src/pr_policy.rs](crates/core/src/pr_policy.rs):
     instruction files, `.github/`, `.agents/`, `.fl/manifest.json`), carries a commit
     without a `Co-authored-by:` trailer, or is co-authored by the Lab Manager.
     [.github/workflows/request-admin-review.yml](.github/workflows/request-admin-review.yml)
     requests the owner's review on GitHub in those cases.
5. An agent reviewer records its verdict with `fl github pr review`. The review is posted
   by the Reviewer GitHub App (`<slug>[bot]`) with an `fl-review` marker, because the
   machine account cannot approve a pull request it opened.
6. Once the required approval is in and the checks pass, the agent who opened the pull
   request merges it with `fl github pr merge`. A reviewer never merges another agent's
   pull request: the author resolves conflicts and keeps the branch current. Merging
   happens through the pull request. A direct push to `main` is not the route, and the
   ruleset refuses one.

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
  changes to the workflow belong to the owner.

## Design records

Design records under `docs/superpowers/` (specs and plans) are not instruction files.
They keep their history and dated decisions.
