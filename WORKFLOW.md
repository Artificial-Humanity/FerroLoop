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
   button is available.
5. The owner reviews and merges. Merging happens through the pull request. A direct
   push to `main` is not the route, and branch protection refuses one.

## Commit identity and safeguards

* Keep the owner's configured git author identity. Use [PERSONA.md](PERSONA.md)
  for the agent's co-author identity.
* `main` is protected. A pull request is required, the `test, clippy, fmt` check must
  pass, the branch must be current with `main`, and review conversations must be
  resolved. Force-pushes and deletions are refused. There is no pre-push gate on the
  local side — observe the git safeguards in `AGENTS.md`.
* **A repository admin can bypass every rule above.** `enforce_admins` is off, so
  protection is a guard rail for the normal path, not a wall. Treat the pull request as
  the route because it is the workflow, not because git will stop you.
* CI runs the verification trio on every push and pull request:
  [.github/workflows/verify.yml](.github/workflows/verify.yml). On a pull request it now
  **blocks the merge**; on a plain branch push it only reports. Run the trio locally
  before pushing — CI is the second reader, not the first.
* Use the workflow stated here. Do not reconstruct additional rules from retired
  workflows or git history; changes to the workflow belong to the owner.
