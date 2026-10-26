# FerroLoop — GitHub-Level Agent Rights Hierarchy Specification

**Date:** 2026-10-26  
**Author:** Ferris (`ferris`, FerroLoop Lead)  
**Status:** Draft (Pending Spec Review & Owner Approval)

---

## 0. Purpose and Scope

In the Artificial Humanity workspace, multiple resident agents (`assistant`, `custom:<slug>`, `lead`, `lab_manager`) share a single GitHub machine user account (`artificially-human`) for Git pushes and pull request authoring, distinguishing their identities via `Co-authored-by: <Name> <<email>>` commit trailers and the `AgentRegistry` (`state.sqlite` / `fl.redb`).

At the GitHub platform level:
1. GitHub forbids `artificially-human` from approving a pull request opened by `artificially-human`.
2. `.github/workflows/request-admin-review.yml` previously requested review from `lmcfarlin` (`Owner`) on every pull request indiscriminately.

This specification defines **GitHub-Level Agent Rights Hierarchy**:
1. **Pure PR Governance Policy Engine (`fl_core::pr_policy`):** Deterministic evaluation of commit `Co-authored-by:` trailers, changed file paths, and `AgentRegistry` records to compute a pull request's required reviewer tier (`Lead(<project>)`, `LabManager`, or `Owner`), enforce self-review prohibition, and validate structured review trailers (`<!-- fl-review: ... -->`).
2. **GitHub PR Broker (`fl_github::pr`):** Pull request creation, commit/file/review inspection, review submission, and merge execution in `fl-github`, supporting dual credentials (standard tracker/author credential for opening PRs, and `AppCredentials` (`<slug>[bot]`) for submitting reviews and merging PRs).
3. **CLI & FerroWire Integration (`fl github pr`):** Subcommands `create`, `policy`, `review`, `merge`, and `verify-ci` under `fl github pr`, with automatic FerroWire notifications (`MessageKind::Status` / `MessageKind::Alert`) routed to the responsible reviewer tier or PR authors.
4. **GitHub Actions Workflow Update (`.github/workflows/request-admin-review.yml`):** Tier-aware review routing and CI verification so `lmcfarlin` is requested on GitHub only when a pull request touches protected governance/CI paths, is co-authored by `lab_manager` (`jarvis`), or contains unattributed commits.

No `fl.redb` schema version bump is required (`FORMAT_version` remains at `FORMAT_WITH_WIRE = 7`).

---

## 1. Pure PR Governance Policy Engine (`crates/core/src/pr_policy.rs`)

### 1.1 Authority Tiers and Rank Ordering

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum RequiredTier {
    /// Project Lead for `project` (or LabManager / Owner).
    Lead { project: String },
    /// Workspace Lab Manager (or Owner).
    LabManager,
    /// Repository Owner (`lmcfarlin`).
    Owner,
}
```

Authority rank ordering for pull request governance:
1. `AgentRole::Assistant` and `AgentRole::Custom(_)` — Contributor rank (`0`). Cannot approve or merge pull requests.
2. `AgentRole::Lead` — Project Lead rank (`1`) for the agent's covered project/areas (`agent.covers_area(project)`). Can approve and merge pull requests for `project` when `RequiredTier` is `Lead { project }` and the agent is not a commit co-author.
3. `AgentRole::LabManager` — Lab Manager rank (`2`). Can approve and merge pull requests for any project when `RequiredTier` is `Lead { .. }` or `LabManager` and the agent is not a commit co-author.
4. `Owner` (`lmcfarlin`) — Owner rank (`3`). Can approve and merge any pull request.

### 1.2 Co-Author Trailer Parsing and Attribution

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitTrailerAuthor {
    pub display_name: String,
    pub name_key: String,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrCommitSummary {
    pub sha: String,
    pub author_login: Option<String>,
    pub message: String,
}
```

- `parse_co_authors(message: &str) -> Vec<CommitTrailerAuthor>` scans all lines of `message` (case-insensitive prefix `co-authored-by:`) matching `Co-authored-by: <Name> <<email>>`.
- `name_key` is derived by lowercasing `<Name>` trimmed (or the local part of `<email>` before `@` if `<Name>` is not a single slug).
- **Unattributed machine commits:** For every commit in the PR where `author_login` is not the Owner (`owner_login`, default `"lmcfarlin"`):
  - If `parse_co_authors(&commit.message)` is empty, the commit is recorded in `unattributed_commits: Vec<String>`.
  - If a parsed `CommitTrailerAuthor` does not match an `Active` agent in `agents: &[AgentRecord]` (matched by `name_key` or `email` case-insensitively), it is recorded in `unknown_co_authors: Vec<String>`.

### 1.3 Protected Paths

```rust
pub const DEFAULT_PROTECTED_PATTERNS: &[&str] = &[
    ".github/**",
    ".fl/manifest.json",
    ".agents/**",
    "AGENTS.md",
    "**/AGENTS.md",
    "CLAUDE.md",
    "**/CLAUDE.md",
    "WORKFLOW.md",
    "**/WORKFLOW.md",
    "PERSONA.md",
    "**/PERSONA.md",
    "CONTRIBUTING.md",
    "**/CONTRIBUTING.md",
];
```

`is_protected_path(path: &str, extra_patterns: &[String]) -> bool` normalizes leading `./` and checks whether `path` matches any default protected pattern or any `extra_patterns`:
- Prefix directory globs (`".github/**"`, `".agents/**"`) match any path equal to `.github`/`.agents` or starting with `.github/` or `.agents/`.
- Basename governance files (`AGENTS.md`, `CLAUDE.md`, `WORKFLOW.md`, `PERSONA.md`, `CONTRIBUTING.md`) match at the repository root or at any subdirectory depth (`path == name || path.ends_with(&format!("/{name}"))`).
- Exact path `".fl/manifest.json"` matches `path == ".fl/manifest.json"`.

### 1.4 Escalation Ladder & Fallback Resolution

Given:
- `project: &str` (the repository's project slug, e.g. `"ferroloop"`),
- `commits: &[PrCommitSummary]`,
- `changed_files: &[String]`,
- `agents: &[AgentRecord]` (all registered agents),
- `owner_login: &str` (default `"lmcfarlin"`),

`evaluate_pr_policy` computes `PrPolicyEvaluation`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrPolicyEvaluation {
    pub project: String,
    pub co_authors: Vec<String>,
    pub unattributed_commits: Vec<String>,
    pub unknown_co_authors: Vec<String>,
    pub protected_files: Vec<String>,
    pub required_tier: RequiredTier,
    pub target_reviewer: String,
    pub escalation_reasons: Vec<String>,
}
```

Escalation rules (evaluated in order):
1. **Protected paths:** If `protected_files` is non-empty, `required_tier = RequiredTier::Owner`, `target_reviewer = owner_login`.
2. **Unattributed or unknown co-authors:** If `unattributed_commits` or `unknown_co_authors` is non-empty (or `commits` is empty), `required_tier = RequiredTier::Owner`, `target_reviewer = owner_login`.
3. **LabManager co-author:** If any commit co-author has `role == AgentRole::LabManager`, `required_tier = RequiredTier::Owner`, `target_reviewer = owner_login`.
4. **Lead co-author (or cross-project Lead):** If any commit co-author has `role == AgentRole::Lead`, `required_tier = RequiredTier::LabManager`.
   - Look up an active `AgentRecord` with `role == AgentRole::LabManager` who is not in `co_authors`.
   - If found, `target_reviewer = lab_manager.name_key`; otherwise escalate `required_tier = RequiredTier::Owner`, `target_reviewer = owner_login`.
5. **Assistant / Custom co-authors only:**
   - Look up an active `AgentRecord` with `role == AgentRole::Lead` who covers `project` (`agent.covers_area(project)`) and is not in `co_authors`.
   - If found, `required_tier = RequiredTier::Lead { project }`, `target_reviewer = lead.name_key`.
   - If no active non-co-author `Lead` covers `project`, escalate to `RequiredTier::LabManager` (selecting active non-co-author `LabManager`, or `RequiredTier::Owner` if none exists).

### 1.5 Structured Review Marker (`FlReviewMarker`) and Authorization Check

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approved,
    ChangesRequested,
    Commented,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlReviewMarker {
    pub reviewer: String,
    pub role: AgentRole,
    pub project: String,
    pub head_sha: String,
    pub verdict: ReviewVerdict,
    pub at: At,
}
```

- **Formatting:** `format_review_body(prose: &str, marker: &FlReviewMarker) -> String` formats:
  ```markdown
  <prose>

  <!-- fl-review: {"reviewer":"ferris","role":"lead","project":"ferroloop","head_sha":"...","verdict":"approved","at":"..."} -->
  ```
- **Parsing:** `extract_review_marker(body: &str) -> Option<FlReviewMarker>` extracts and deserializes the `<!-- fl-review: ... -->` HTML comment block.
- **Reviewer Authorization (`authorize_agent_action`):**
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(tag = "code", rename_all = "snake_case")]
  pub enum PolicyRefusal {
      UnknownOrInactiveAgent { agent: String },
      SelfReviewForbidden { agent: String },
      ProtectedPathRequiresOwner { agent: String, files: Vec<String> },
      UnattributedCommitsRequireOwner { commits: Vec<String> },
      UnknownCoAuthorsRequireOwner { co_authors: Vec<String> },
      InsufficientRole {
          agent: String,
          role: AgentRole,
          required: RequiredTier,
      },
      MissingValidApproval { head_sha: String },
  }
  ```
  `authorize_agent_action(eval: &PrPolicyEvaluation, caller: &AgentRecord) -> Result<(), PolicyRefusal>` verifies:
  1. `caller.status == AgentStatus::Active` (else `UnknownOrInactiveAgent`).
  2. `!eval.co_authors.iter().any(|c| c == &caller.name_key)` (else `SelfReviewForbidden`).
  3. `eval.protected_files.is_empty()` (else `ProtectedPathRequiresOwner`).
  4. `eval.unattributed_commits.is_empty()` (else `UnattributedCommitsRequireOwner`).
  5. `eval.unknown_co_authors.is_empty()` (else `UnknownCoAuthorsRequireOwner`).
  6. Role check against `eval.required_tier`:
     - `RequiredTier::Lead { project }`: `caller` must be `(AgentRole::Lead && caller.covers_area(project))` or `AgentRole::LabManager`.
     - `RequiredTier::LabManager`: `caller` must be `AgentRole::LabManager`.
     - `RequiredTier::Owner`: always refuses agent callers with `InsufficientRole` (only `Owner` can approve/merge on GitHub).
- **Merge Approval Check (`verify_merge_approvals`):**
  Given `head_sha: &str`, `eval: &PrPolicyEvaluation`, `reviews: &[PrReviewSummary]`, `agents: &[AgentRecord]`, and `owner_login: &str`:
  - A review satisfies merge requirements if its GitHub state is `"APPROVED"` and either:
    a. `review.author_login.eq_ignore_ascii_case(owner_login)` and `review.commit_id.as_deref() == Some(head_sha)`, OR
    b. `extract_review_marker(&review.body)` yields a marker with `marker.verdict == ReviewVerdict::Approved`, `marker.head_sha == head_sha`, and the `AgentRecord` matching `marker.reviewer` has `agent.role == marker.role` and passes `authorize_agent_action(eval, agent)`.
  - If no review satisfies this, returns `Err(PolicyRefusal::MissingValidApproval { head_sha })`.

---

## 2. GitHub PR Broker (`crates/github/src/pr.rs` & `crates/github/src/fake.rs`)

### 2.1 GitHub REST Endpoints for Pull Requests

`fl-github` adds a `pr` module exposing typed operations over `Client`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestView {
    pub number: u64,
    pub html_url: String,
    pub state: String,
    pub title: String,
    pub body: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub user_login: String,
    pub merged: bool,
    pub requested_reviewers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrReviewSummary {
    pub id: u64,
    pub author_login: String,
    pub state: String,
    pub body: String,
    pub commit_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}
```

Endpoints used by `fl_github::pr`:
1. `create_pr(client, repo, title, body, head, base) -> Result<PullRequestView, StoreError>`
   - `POST /repos/{owner}/{repo}/pulls` with `{ title, body, head, base }`.
2. `get_pr(client, repo, number) -> Result<PullRequestView, StoreError>`
   - `GET /repos/{owner}/{repo}/pulls/{number}`.
3. `list_pr_commits(client, repo, number) -> Result<Vec<PrCommitSummary>, StoreError>`
   - `GET /repos/{owner}/{repo}/pulls/{number}/commits?per_page=100` (paginated).
4. `list_pr_files(client, repo, number) -> Result<Vec<String>, StoreError>`
   - `GET /repos/{owner}/{repo}/pulls/{number}/files?per_page=100` (paginated).
5. `list_pr_reviews(client, repo, number) -> Result<Vec<PrReviewSummary>, StoreError>`
   - `GET /repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100` (paginated).
6. `request_pr_reviewers(client, repo, number, reviewers: &[String]) -> Result<(), StoreError>`
   - `POST /repos/{owner}/{repo}/pulls/{number}/requested_reviewers` with `{ "reviewers": reviewers }`.
7. `submit_pr_review(client, repo, number, commit_id, event, body) -> Result<PrReviewSummary, StoreError>`
   - `POST /repos/{owner}/{repo}/pulls/{number}/reviews` with `{ "commit_id": commit_id, "event": "APPROVE" | "REQUEST_CHANGES" | "COMMENT", "body": body }`.
8. `merge_pr(client, repo, number, head_sha, method, commit_title) -> Result<String, StoreError>`
   - `PUT /repos/{owner}/{repo}/pulls/{number}/merge` with `{ "sha": head_sha, "merge_method": "merge" | "squash" | "rebase", ... }`.

### 2.2 Dual-Credential Routing (`author` vs. `reviewer_app`)

- Opening a pull request (`create_pr`, `request_pr_reviewers`) and reading PR metadata (`get_pr`, `list_pr_commits`, `list_pr_files`, `list_pr_reviews`) use the project's bound `GithubTracker::client()`.
- Submitting a formal review (`submit_pr_review`) and merging a pull request (`merge_pr`) use the **Reviewer App Client**:
  - Resolved from `[github]` (`GithubApp { app_id, private_key }`) in `config.toml`, or environment overrides `$FL_GITHUB_APP_ID` and `$FL_GITHUB_PRIVATE_KEY_PATH` (constructing `AppCredentials::from_file`).
  - If the project's tracker already uses `credential = "app"`, that same `AppCredentials` client is used.
  - If neither `[github]` nor `$FL_GITHUB_APP_ID` + `$FL_GITHUB_PRIVATE_KEY_PATH` is configured when `fl github pr review` or `fl github pr merge` is called, `fl` returns a clear configuration error explaining that agent PR review/merge requires a GitHub App (`[github]` in `config.toml` or `$FL_GITHUB_APP_ID` / `$FL_GITHUB_PRIVATE_KEY_PATH`) so the reviewer identity (`<slug>[bot]`) is distinct from `artificially-human`.

### 2.3 In-Process Fake Support (`crates/github/src/fake.rs`)

`FakeGithub` (`fake::State`) is extended with:
- `pulls: BTreeMap<u64, FakePull>` storing `PullRequestView`, `commits: Vec<PrCommitSummary>`, `files: Vec<String>`, `reviews: Vec<PrReviewSummary>`, `requested_reviewers: Vec<String>`, and `merged: bool`.
- Helper methods on `Fake` for seeding PRs, commits, and changed files in tests (`fake.add_pull(...)`, `fake.set_pull_commits(...)`, `fake.set_pull_files(...)`).
- Support for the 8 PR REST endpoints above, distinguishing requests authenticated with `INSTALLATION_TOKEN` (`fake-app[bot]`) vs. `USER_LOGIN` (`fake-user`) so `FakeGithub` enforces GitHub's platform rule: a user cannot `APPROVE` or `REQUEST_CHANGES` on their own pull request (`422 Unprocessable Entity: Cannot approve your own pull request`), whereas `fake-app[bot]` succeeds.

---

## 3. CLI & FerroWire Integration (`crates/cli/src/cmd/github.rs` & `pr.rs`)

### 3.1 `fl github pr` Subcommands

`crates/cli/src/cmd/github.rs` adds `Pr(crate::cmd::pr::PrCmd)`:

1. `fl github pr create`
   ```text
   fl github pr create --title <TITLE> --body <BODY> --head <BRANCH> [--base <BRANCH>] [--by <AGENT>] [--project <SLUG>] [--json]
   ```
   - Calls `create_pr` (default `--base main`).
   - Fetches the PR's commits and changed files and runs `evaluate_pr_policy`.
   - If `eval.required_tier == RequiredTier::Owner`, calls `request_pr_reviewers` for `["lmcfarlin"]` (or configured `owner_login`).
   - Otherwise, sends a FerroWire message (`MessageKind::Status`, `MessageUrgency::Normal`, subject `"PR #<number> ready for <tier> review: <title>"`) to `WireAddress::Agent(eval.target_reviewer)` in the workspace state store.
   - Prints tab-separated summary (or `--json`).

2. `fl github pr policy`
   ```text
   fl github pr policy <NUMBER> [--project <SLUG>] [--json]
   ```
   - Fetches PR metadata, commits, files, and reviews; loads agents from the workspace state store; computes `PrPolicyEvaluation` and whether the current `head_sha` has a valid approval.
   - Prints human-readable policy report or `--json`.

3. `fl github pr review`
   ```text
   fl github pr review <NUMBER> (--approve | --request-changes | --comment) --body <BODY> --by <AGENT> [--project <SLUG>] [--json]
   ```
   - Resolves `--by <AGENT>` in the workspace `AgentRegistry`.
   - Fetches PR metadata, commits, and files; computes `PrPolicyEvaluation`.
   - For `--approve` and `--request-changes`, runs `authorize_agent_action(&eval, &caller)`. On `Err(refusal)`, prints the refusal reason to `stderr` (or JSON if `--json`) and exits with code `2`.
   - Formats the review body with `FlReviewMarker` (`head_sha = pr.head_sha`) and submits via the Reviewer App Client (`submit_pr_review`).
   - Sends a FerroWire notification to each co-author in `eval.co_authors`.

4. `fl github pr merge`
   ```text
   fl github pr merge <NUMBER> [--method merge|squash|rebase] --by <AGENT> [--project <SLUG>] [--json]
   ```
   - Resolves `--by <AGENT>` in the workspace `AgentRegistry`.
   - Fetches PR metadata, commits, files, and reviews; computes `PrPolicyEvaluation`.
   - Runs `authorize_agent_action(&eval, &caller)` and `verify_merge_approvals(&pr.head_sha, &eval, &reviews, &agents, owner_login)`. On `Err(refusal)`, prints the refusal and exits with code `2`.
   - Calls `merge_pr` via the Reviewer App Client with `head_sha = pr.head_sha` and `method` (default ` MergeMethod::Merge`).
   - Sends a FerroWire notification to each co-author in `eval.co_authors`.

5. `fl github pr verify-ci`
   ```text
   fl github pr verify-ci <NUMBER> [--project <SLUG>] [--require-approval] [--json]
   ```
   - Evaluates the PR's commits, files, and existing reviews:
     - Refuses with exit code `2` if any commit from a non-Owner author lacks a `Co-authored-by:` trailer (`UnattributedCommitsRequireOwner`).
     - Refuses with exit code `2` if any `<slug>[bot]` `APPROVED` review on `head_sha` carries an invalid/forged `<!-- fl-review: ... -->` marker or violates `authorize_agent_action` (e.g., self-review or protected paths).
     - When `--require-approval` is passed, also verifies `verify_merge_approvals`.

---

## 4. GitHub Actions Workflow Update (`.github/workflows/request-admin-review.yml`)

Update `.github/workflows/request-admin-review.yml` on `pull_request_target` (`opened`, ` synchronize`, `ready_for_review`, `reopened`):
1. Fetch the PR's changed files (`gh api "repos/$REPO/pulls/$PR/files" --paginate --jq '.[].filename'`) and commits (`gh api "repos/$REPO/pulls/$PR/commits" --paginate`).
2. Check whether the PR requires `Owner` (`lmcfarlin`) review:
   - Any changed file matches `.github/*`, `.fl/manifest.json`, `.agents/*`, or `*/AGENTS.md`, `AGENTS.md`, `*/CLAUDE.md`, `CLAUDE.md`, `*/WORKFLOW.md`, `WORKFLOW.md`, `*/PERSONA.md`, `PERSONA.md`, `*/CONTRIBUTING.md`, `CONTRIBUTING.md`.
   - Any commit by a non-admin lacks a `Co-authored-by:` trailer.
   - Any commit carries a `Co-authored-by:` trailer for `jarvis@artificialhumanity.io` (`lab_manager`).
3. If `Owner` review is required, request review from `lmcfarlin`. Otherwise, skip requesting `lmcfarlin` and log the computed agent review tier (`Lead` or `LabManager`).

---

## 5. Testing Requirements

1. **Unit Tests (`crates/core/src/pr_policy.rs`):**
   - `parse_co_authors` with single, multiple, case-varied trailers, and commits without trailers.
   - `is_protected_path` across `.github/workflows/ci.yml`, `.fl/manifest.json`, root and nested `AGENTS.md` / `WORKFLOW.md` / `PERSONA.md` / `CLAUDE.md` / `CONTRIBUTING.md`, `.agents/AGENTS.md`, and normal source paths.
   - `evaluate_pr_policy` escalation ladder:
     - `Assistant` PR -> `RequiredTier::Lead { project }` targeting project `Lead`.
     - `Assistant` PR when project has no active `Lead` -> `RequiredTier::LabManager` targeting `jarvis`.
     - `Lead` PR (or multi-commit PR with both `Assistant` and `Lead` co-authors) -> `RequiredTier::LabManager` targeting `jarvis`.
     - `LabManager` PR -> `RequiredTier::Owner` targeting `lmcfarlin`.
     - Protected path touched by any role -> `RequiredTier::Owner` targeting `lmcfarlin`.
     - Unattributed commit -> `RequiredTier::Owner`.
   - `authorize_agent_action` & `verify_merge_approvals`:
     - Self-review refusal (`SelfReviewForbidden`) even when caller is `Lead` or `LabManager`.
     - `Assistant` attempting review (`InsufficientRole`).
     - `Lead` of another project attempting review (`InsufficientRole`).
     - `Lead` attempting review of a `Lead` PR (`InsufficientRole`).
     - Valid `FlReviewMarker` approval on matching `head_sha` passes `verify_merge_approvals`; stale `head_sha` fails with `MissingValidApproval`.
2. **Fake GitHub & CLI Integration Tests (`crates/cli/tests/github_pr.rs`):**
   - End-to-end flow over `FakeGithub`:
     - Register `ferris` (`lead`), `felicia` (`assistant`), and `jarvis` (`lab_manager`).
     - `felicia` creates PR #1 -> routes FerroWire review request to `ferris`.
     - `felicia` tries to approve PR #1 -> exits with code `2`.
     - `ferris` approves PR #1 via Reviewer App -> posts `<!-- fl-review: ... -->` as `fake-app[bot]` and merges PR #1 -> exits `0`.
     - `ferris` creates PR #2 -> `ferris` self-approval exits `2`; `jarvis` (`lab_manager`) approves and merges PR #2 -> exits `0`.
     - PR #3 touching `.github/workflows/ci.yml` -> requests `lmcfarlin` on GitHub; both `ferris` and `jarvis` approvals exit `2`.
