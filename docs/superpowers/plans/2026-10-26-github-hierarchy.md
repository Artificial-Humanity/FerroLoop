# GitHub-Level Agent Rights Hierarchy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enforce a tiered agent rights hierarchy (`assistant`/`custom` -> `lead` -> `lab_manager` -> `owner`) for GitHub pull request creation, review, and merge using a pure policy engine in `fl-core`, dual-credential (`EnvToken` author + `AppCredentials` `<slug>[bot]` reviewer) PR operations in `fl-github`, `fl github pr` CLI subcommands with FerroWire notifications, and a tier-aware `.github/workflows/request-admin-review.yml` workflow.

**Architecture:**
1. `crates/core/src/pr_policy.rs` implements pure, deterministic evaluation of commit `Co-authored-by:` trailers, protected file patterns, `AgentRegistry` roles, chronological review folding, and `<!-- fl-review: ... -->` markers.
2. `crates/github/src/pr.rs` and `crates/github/src/client.rs` (`Method::Put`) implement the 8 GitHub Pull Request REST endpoints, and `crates/github/src/fake.rs` models pull requests, reviews, requested reviewers, merges, and GitHub's `422` self-approval refusal.
3. `crates/cli/src/cmd/pr.rs`, `crates/cli/src/cmd/github.rs`, and `crates/cli/src/main.rs` expose `fl github pr {create,policy,review,merge,verify-ci}`, reuse `ctx.store` when `same_store(&path, &ws_store_path)` holds (avoiding `redb` double-lock), authenticate reviews/merges via the Reviewer GitHub App, and dispatch FerroWire notifications.
4. `.github/workflows/request-admin-review.yml` inspects PR files and commit trailers so `lmcfarlin` is requested on GitHub only when `Owner` review is required.

**Tech Stack:** Rust (`fl-core`, `fl-github`, `fl-cli`), `serde`, `serde_json`, `ureq`, `clap`, GitHub Actions (`gh api`).

---

## File Structure

- **Create:**
  - `crates/core/src/pr_policy.rs` — Pure PR governance policy types, trailer parser, protected path matcher, tier evaluator, review marker formatter/parser, and authorization/merge-approval verifiers.
  - `crates/github/src/pr.rs` — Typed GitHub Pull Request REST client functions (`create_pr`, `get_pr`, `list_pr_commits`, `list_pr_files`, `list_pr_reviews`, `request_pr_reviewers`, `submit_pr_review`, `merge_pr`).
  - `crates/cli/src/cmd/pr.rs` — `fl github pr` subcommands (`create`, `policy`, `review`, `merge`, `verify-ci`), Reviewer App client construction, and FerroWire notifications.
  - `crates/cli/tests/github_pr.rs` — Black-box CLI integration tests over `FakeGithub` covering the full hierarchy (`assistant` -> `lead` -> `lab_manager` -> `owner`), self-review refusal, and protected paths.
- **Modify:**
  - `crates/core/src/lib.rs` — Export `pub mod pr_policy;` and re-export core policy types.
  - `crates/core/tests/wire_refs.rs` — Add wire round-trip tests for `pr_policy` types.
  - `crates/github/src/client.rs` — Add `Method::Put` and handle it in `exchange`.
  - `crates/github/src/lib.rs` — Export `pub mod pr;`.
  - `crates/github/src/fake.rs` — Add `FakePull`, `pulls: BTreeMap<u64, FakePull>`, seeding helpers, and the 8 PR routes in `route(...)`.
  - `crates/cli/src/cmd/mod.rs` — Export `pub mod pr;`.
  - `crates/cli/src/cmd/github.rs` — Add `Cmd::Pr(crate::cmd::pr::PrCmd)` and pass workspace state store + GitHub App config.
  - `crates/cli/src/main.rs` — Open `ws_store` safely (reusing `store` when `same_store(&path, &ws_store_path)`) and pass `ws_store`, `binding`, `cfg.github`, and `default_project` to `cmd::github::run`.
  - `.github/workflows/request-admin-review.yml` — Tier-aware Owner review request logic.

---

### Task 1: Pure PR Governance Policy Engine (`crates/core/src/pr_policy.rs`)

**Files:**
- Create: `crates/core/src/pr_policy.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/core/tests/wire_refs.rs`

- [ ] **Step 1: Implement `crates/core/src/pr_policy.rs` with full unit tests**

Create `crates/core/src/pr_policy.rs` implementing Spec §1.1–§1.5:
- `DEFAULT_OWNER_LOGIN = "lmcfarlin"`
- `DEFAULT_PROTECTED_PATTERNS`:
  - `".github/**"`, `".fl/manifest.json"`, `".agents/**"`, `"AGENTS.md"`, `"**/AGENTS.md"`, `"CLAUDE.md"`, `"**/CLAUDE.md"`, `"WORKFLOW.md"`, `"**/WORKFLOW.md"`, `"PERSONA.md"`, `"**/PERSONA.md"`, `"CONTRIBUTING.md"`, `"**/CONTRIBUTING.md"`
- Types:
  - `RequiredTier` (`Lead { project: String }`, `LabManager`, `Owner`)
  - `CommitTrailerAuthor { display_name, name_key, email }`
  - `PrCommitSummary { sha, author_login, message }`
  - `PrReviewSummary { id, author_login, state, body, commit_id }`
  - `PrPolicyEvaluation { project, co_authors, unattributed_commits, unknown_co_authors, protected_files, required_tier, target_reviewer, escalation_reasons }`
  - `ReviewVerdict` (`Approved`, `ChangesRequested`, `Commented`)
  - `FlReviewMarker { reviewer, role, project, head_sha, verdict, at }`
  - `PolicyRefusal` (`UnknownOrInactiveAgent`, `SelfReviewForbidden`, `ProtectedPathRequiresOwner`, `UnattributedCommitsRequireOwner`, `UnknownCoAuthorsRequireOwner`, `InsufficientRole`, `MissingValidApproval`) with ` fmt::Display`
- Functions:
  - `parse_co_authors(message: &str) -> Vec<CommitTrailerAuthor>`
  - `is_protected_path(path: &str, extra_patterns: &[String]) -> bool`
  - `evaluate_pr_policy(project: &str, commits: &[PrCommitSummary], changed_files: &[String], extra_protected_patterns: &[String], agents: &[AgentRecord], owner_login: &str) -> PrPolicyEvaluation`
  - `format_review_body(prose: &str, marker: &FlReviewMarker) -> String`
  - `extract_review_marker(body: &str) -> Option<FlReviewMarker>`
  - `authorize_agent_action(eval: &PrPolicyEvaluation, caller: &AgentRecord) -> Result<(), PolicyRefusal>`
  - `verify_merge_approvals(head_sha: &str, eval: &PrPolicyEvaluation, reviews: &[PrReviewSummary], agents: &[AgentRecord], owner_login: &str) -> Result<String, PolicyRefusal>` (folding `reviews` chronologically per logical reviewer so a later `CHANGES_REQUESTED` or `DISMISSED` overrides an earlier `APPROVED`, while `COMMENTED` does not).

- [ ] **Step 2: Export `pr_policy` in `crates/core/src/lib.rs` and add wire round-trip tests in `crates/core/tests/wire_refs.rs`**

Export `pub mod pr_policy;` and re-export key types in `crates/core/src/lib.rs`. Add JSON round-trip assertions for `RequiredTier`, `ReviewVerdict`, `FlReviewMarker`, `PrPolicyEvaluation`, and `PolicyRefusal` in `crates/core/tests/wire_refs.rs`.

- [ ] **Step 3: Run `cargo test -p fl-core` and commit Task 1**

Run:
```bash
cargo test -p fl-core
```
Commit with message `feat(core): add pure PR governance policy engine and review marker verifier` and `Co-authored-by: Ferris <ferris@artificialhumanity.io>`.

---

### Task 2: GitHub PR REST Broker & `FakeGithub` PR Support (`crates/github`)

**Files:**
- Modify: `crates/github/src/client.rs`
- Create: `crates/github/src/pr.rs`
- Modify: `crates/github/src/lib.rs`
- Modify: `crates/github/src/fake.rs`

- [ ] **Step 1: Add `Method::Put` to `crates/github/src/client.rs`**

In `crates/github/src/client.rs`:
- Add `Put` to `pub enum Method`.
- In `exchange(...)`, add:
  ```rust
  (Method::Put, Some(b)) => headers(agent.put(url), &auth).send_json(b),
  (Method::Put, None) => headers(agent.put(url), &auth).send_empty(),
  ```

- [ ] **Step 2: Create `crates/github/src/pr.rs` and export in `crates/github/src/lib.rs`**

Implement `PullRequestView`, `MergeMethod`, and the 8 PR functions in `crates/github/src/pr.rs`:
- `create_pr(client: &Client, repo: &str, title: &str, body: &str, head: &str, base: &str) -> Result<PullRequestView, StoreError>`
- `get_pr(client: &Client, repo: &str, number: u64) -> Result<PullRequestView, StoreError>`
- `list_pr_commits(client: &Client, repo: &str, number: u64) -> Result<Vec<PrCommitSummary>, StoreError>` (follows pagination via `reply.next_link()`)
- `list_pr_files(client: &Client, repo: &str, number: u64) -> Result<Vec<String>, StoreError>` (follows pagination)
- `list_pr_reviews(client: &Client, repo: &str, number: u64) -> Result<Vec<PrReviewSummary>, StoreError>` (follows pagination)
- `request_pr_reviewers(client: &Client, repo: &str, number: u64, reviewers: &[String]) -> Result<(), StoreError>`
- `submit_pr_review(client: &Client, repo: &str, number: u64, commit_id: &str, event: &str, body: &str) -> Result<PrReviewSummary, StoreError>`
- `merge_pr(client: &Client, repo: &str, number: u64, head_sha: &str, method: MergeMethod, commit_title: Option<&str>) -> Result<String, StoreError>`

Note: If `Reply::link_next` in `crates/github/src/client.rs` is private, add `pub fn next(&self) -> Option<&str> { self.link_next.as_deref() }` on `Reply`.

- [ ] **Step 3: Extend `crates/github/src/fake.rs` with PR state, seeding helpers, and REST routes**

In `crates/github/src/fake.rs`:
- Add `FakePull` to `fake` with `number`, `title`, `body`, `state`, `head_ref`, `head_sha`, `base_ref`, `user_login`, `merged`, `merge_sha`, `requested_reviewers`, `commits: Vec<PrCommitSummary>`, `files: Vec<String>`, `reviews: Vec<PrReviewSummary>`.
- Add `pub pulls: BTreeMap<u64, FakePull>` to `State` (and `pub default_pr_commits: Vec<PrCommitSummary>`, `pub default_pr_files: Vec<String>` so a newly created PR via `POST /repos/{o}/{r}/pulls` automatically inherits seeded branch commits/files when present).
- Add helper methods on `FakeGithub`:
  - `pub fn seed_branch(&self, head_sha: &str, commits: Vec<PrCommitSummary>, files: Vec<String>)`
  - `pub fn set_pull_commits(&self, number: u64, head_sha: &str, commits: Vec<PrCommitSummary>)`
  - `pub fn set_pull_files(&self, number: u64, files: Vec<String>)`
  - `pub fn add_pull_review(&self, number: u64, author_login: &str, state: &str, body: &str, commit_id: &str)`
  - `pub fn pull(&self, number: u64) -> FakePull`
- Handle the 8 PR routes in `route(...)`:
  - `("POST", ["repos", o, r, "pulls"])`
  - `("GET", ["repos", o, r, "pulls", n])`
  - `("GET", ["repos", o, r, "pulls", n, "commits"])`
  - `("GET", ["repos", o, r, "pulls", n, "files"])`
  - `("GET", ["repos", o, r, "pulls", n, "reviews"])`
  - `("POST", ["repos", o, r, "pulls", n, "requested_reviewers"])`
  - `("POST", ["repos", o, r, "pulls", n, "reviews"])` — enforce `422` if `matches!(event, "APPROVE" | "REQUEST_CHANGES") && by == pull.user_login`.
  - `("PUT", ["repos", o, r, "pulls", n, "merge"])` — enforce `409` if `sha != pull.head_sha`.

- [ ] **Step 4: Run `cargo test -p fl-github` (including unit tests in `pr.rs`) and commit Task 2**

Run:
```bash
cargo test -p fl-github
```
Commit with message `feat(github): add pull request REST broker, Method::Put, and FakeGithub PR routes` and `Co-authored-by: Ferris <ferris@artificialhumanity.io>`.

---

### Task 3: CLI `fl github pr` Subcommands & FerroWire Dispatch (`crates/cli`)

**Files:**
- Create: `crates/cli/src/cmd/pr.rs`
- Modify: `crates/cli/src/cmd/mod.rs`
- Modify: `crates/cli/src/cmd/github.rs`
- Modify: `crates/cli/src/main.rs`
- Create: `crates/cli/tests/github_pr.rs`

- [ ] **Step 1: Create `crates/cli/src/cmd/pr.rs` and wire into `cmd/mod.rs`, `cmd/github.rs`, and `main.rs`**

In `crates/cli/src/cmd/pr.rs`:
- Implement `PrCmd` (`Create`, `Policy`, `Review`, `Merge`, `VerifyCi`).
- Implement `reviewer_client(gh: &fl_github::GithubTracker, binding: Option<&config::TrackerBinding>, app: Option<&config::GithubApp>) -> anyhow::Result<Option<fl_github::Client>>`:
  - If `binding.is_some_and(|b| b.credential == config::Credential::App)`, return `Ok(None)` (caller reuses `gh.client()`).
  - Else check `app` (`[github]` in `config.toml`) or `$FL_GITHUB_APP_ID` + `$FL_GITHUB_PRIVATE_KEY_PATH` and construct `fl_github::Client::new(&api, Box::new(fl_github::AppCredentials::from_file(...)?))`.
- Implement FerroWire notification helper using `ws_store: &RedbStore` (`MessageStore::send_wire` with `MessageKind::Mail`, `MessageUrgency::Normal`, `WireAddress::AgentName(...)`).
- Ensure `PolicyRefusal` prints structured JSON to `stdout` when `--json` is set, or `error: <refusal>` to `stderr` when `--json` is false, and returns `Ok(2)`.
- Wire `Cmd::Pr(crate::cmd::pr::PrCmd)` in `crates/cli/src/cmd/github.rs` and pass `ws_store`, `binding`, `cfg.github.as_ref()`, and `default_project` from `crates/cli/src/main.rs` (opening a separate `RedbStore` at `ws_store_path` only when `!same_store(&path, &ws_store_path)`).

- [ ] **Step 2: Write end-to-end integration tests in `crates/cli/tests/github_pr.rs`**

Write integration tests in `crates/cli/tests/github_pr.rs` covering:
1. Assistant (`felicia`) opens PR #1 -> `RequiredTier::Lead { project: "ferroloop" }`, FerroWire review request delivered to `ferris`'s inbox (`fl wire read --agent ferris`).
2. Assistant (`felicia`) attempts `fl github pr review 1 --approve --by felicia` -> exits with code `2` (`self_review_forbidden`).
3. Another assistant (`alex`) attempts `fl github pr review 1 --approve --by alex` -> exits with code `2` (`insufficient_role`).
4. Project Lead (`ferris`) runs `fl github pr review 1 --approve --body "LGTM" --by ferris` -> succeeds via Reviewer App (`fake-app[bot]`), embeds `<!-- fl-review: ... -->`, and `fl github pr merge 1 --by ferris` merges PR #1 and notifies `felicia`.
5. Project Lead (`ferris`) opens PR #2 -> `RequiredTier::LabManager` targeting `jarvis`. `ferris` attempting self-approval exits `2` (`self_review_forbidden`). `jarvis` (`lab_manager`) approves and merges PR #2 -> exits `0`.
6. Protected-path PR #3 touching `.github/workflows/ci.yml` -> `RequiredTier::Owner` targeting `lmcfarlin`, requests `lmcfarlin` on GitHub, and refuses both `ferris` and `jarvis` approvals with exit code `2` (`protected_path_requires_owner`).

- [ ] **Step 3: Run `cargo test -p fl-cli --test github_pr` and commit Task 3**

Run:
```bash
cargo test -p fl-cli --test github_pr
```
Commit with message `feat(cli): add fl github pr subcommands, App reviewer brokering, and FerroWire routing` and `Co-authored-by: Ferris <ferris@artificialhumanity.io>`.

---

### Task 4: Tier-Aware GitHub Actions Workflow & Full Verification (`.github/workflows/request-admin-review.yml`)

**Files:**
- Modify: `.github/workflows/request-admin-review.yml`

- [ ] **Step 1: Update `.github/workflows/request-admin-review.yml` per Spec §4**

Update `.github/workflows/request-admin-review.yml` so that on `pull_request_target` (`opened`, `synchronize`, `ready_for_review`, `reopened`):
- It inspects changed files and commit trailers via `gh api`.
- It requests review from `lmcfarlin` when:
  - Any changed file matches `.github/*`, `.fl/manifest.json`, `.agents/*`, `AGENTS.md`, `*/AGENTS.md`, `CLAUDE.md`, `*/CLAUDE.md`, `WORKFLOW.md`, `*/WORKFLOW.md`, `PERSONA.md`, `*/PERSONA.md`, `CONTRIBUTING.md`, `*/CONTRIBUTING.md`.
  - Any commit by a non-admin has no `Co-authored-by:` trailer.
  - Any commit carries `Co-authored-by:` for `jarvis@artificialhumanity.io`.
- Otherwise, it logs that the PR is routed to the project `lead` or `lab_manager` and does not spam `lmcfarlin` with a GitHub review request.

- [ ] **Step 2: Run full workspace verification (`fmt`, `clippy`, `test`) and commit Task 4**

Run:
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Commit with message `ci(github): make request-admin-review workflow tier-aware` and `Co-authored-by: Ferris <ferris@artificialhumanity.io>`.
