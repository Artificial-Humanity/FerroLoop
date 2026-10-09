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
4. Push the branch and open a pull request against `main`. A pull request opened ready
   for review (not a draft) is put in the owner's review queue and assigned to them by
   [.github/workflows/request-admin-review.yml](.github/workflows/request-admin-review.yml).
5. The owner reviews, approves, and merges. Merging happens through the pull request. A
   direct push to `main` is not the route, and the ruleset refuses one.

## Commit identity and safeguards

* Commits are authored by the org machine account `artificially-human`, which is git's
  configured identity and `gh`'s active account. Add the agent as a co-author with the
  trailer in [PERSONA.md](PERSONA.md).
* Never author a commit as the owner, and never switch `gh` to the owner's account.
  GitHub refuses a push that carries the owner's private email (error GH007).
* `main` is protected by the repository ruleset "Main Protection". A pull request is
  required, with one approving review; a new push dismisses an earlier approval. The
  `test, clippy, fmt` check must pass, the branch must be current with `main`, and review
  conversations must be resolved. The machine account cannot approve its own pull
  request, so the owner approves every agent pull request. Force-pushes and deletions
  are refused. There is no pre-push gate on the local side.
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
