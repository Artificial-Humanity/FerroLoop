# Workflow — Project FerroLoop

Follow [AGENTS.md](AGENTS.md) for repository rules and git configuration.

## Branch, review, merge

1. Branch off local `main`. All work happens on a branch.
2. When the work is complete, use `superpowers:requesting-code-review` to dispatch
   a review.
3. Use `superpowers:receiving-code-review` to evaluate the findings. Address them,
   then commit the fixes.
4. Merge to `main` after the review cycle. A direct push to `main` is allowed;
   a pull request is not required.

## Commit identity and safeguards

* Keep the owner's configured git author identity. Use [PERSONA.md](PERSONA.md)
  for the agent's co-author identity.
* Follow the review cycle even without mechanical enforcement. `main` has no branch
  protection and no pre-push gate. Observe the git safeguards in `AGENTS.md`.
* CI runs the verification trio on every push and pull request:
  [.github/workflows/verify.yml](.github/workflows/verify.yml). It **reports; it does not
  block** — nothing stops a red branch from being merged, so read the run rather than
  assuming it. Run the trio locally before pushing; CI is the second reader, not the first.
* Use the workflow stated here. Do not reconstruct additional rules from retired
  workflows or git history; changes to the workflow belong to the owner.
