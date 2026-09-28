# Workflow — Project FerroLoop

Follow [AGENTS.md](AGENTS.md) for repository rules and git configuration.

## Branch, review, merge

1. Branch off local `main`. All work happens on a branch.
2. When the work is complete, use `superpowers:requesting-code-review` to dispatch
   a review.
3. Use `superpowers:receiving-code-review` to evaluate the findings. Address them,
   then commit the fixes.
4. Push the branch and open a pull request against `main`. CI runs the verification
   trio on the pull request; the `test, clippy, fmt` check must pass before the merge
   button is available. A pull request opened ready for review (not a draft) is put in
   the owner's review queue and assigned to them by
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
  are refused. There is no pre-push gate on the local side — observe the git safeguards
  in `AGENTS.md`.
* **The rules bind everyone, the owner included.** The ruleset has no bypass list, so
  no account can push to `main` directly or merge with a red check. Changes to the
  ruleset belong to the owner.
* CI runs the verification trio on every pull request and on every push to `main`:
  [.github/workflows/verify.yml](.github/workflows/verify.yml). On a pull request it
  **blocks the merge**. A branch push with no pull request does not run CI. Run the trio
  locally before pushing — CI is the second reader, not the first.
* The machine account's token cannot read check runs or commit statuses, so
  `gh pr checks` can fail. Read the check's result on the pull request page instead.
* Use the workflow stated here. Do not reconstruct additional rules from retired
  workflows or git history; changes to the workflow belong to the owner.
