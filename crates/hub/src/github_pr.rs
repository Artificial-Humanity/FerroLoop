//! Server-verified pull-request review and merge (`POST /api/v1/prs/:n/review`,
//! `POST /api/v1/prs/:n/merge`, and the project-scoped merge aliases).
//!
//! The Hub never trusts a client for the facts a policy decision rests on. A caller names a
//! pull request (and, for a peer, the agent it acts as); the Hub reads the commits, changed
//! files, reviews and head SHA from GitHub itself, evaluates the PR policy over *those*, and for
//! a merge asks GitHub to merge. A `commits`, `files` or `reviews` array in the request body is
//! ignored, because anyone holding the peer token could forge an `APPROVED` review in one.
//!
//! ⚠ A peer still chooses its own agent name (`X-Fl-Agent`, one peer token for all agents), so
//! [`bind_caller`] only stops a peer from naming a *different* agent in the body than the one it
//! announced. It is not authentication of an individual agent; the approval check against
//! GitHub's own Reviewer App reviews is what a merge ultimately rests on.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use fl_core::agent::{AgentRecord, AgentStatus};
use fl_core::pr_policy::{
    FlReviewMarker, PrCommitSummary, PrReviewSummary, ReviewVerdict, authorize_agent_action,
    authorize_agent_merge, evaluate_pr_policy, format_review_body, verify_merge_approvals,
};
use fl_core::store::{AgentRegistry, StoreError};
use fl_exec::stamp;
use fl_github::{
    AppCredentials, Client, EnvToken, MergeMethod, PullRequestView, get_pr, list_pr_commits,
    list_pr_files, list_pr_reviews, merge_pr, submit_pr_review,
};
use fl_store::AnyStore;
use serde::Deserialize;
use serde_json::json;

use crate::HubState;
use crate::api::{HttpRequest, HttpResponse};
use crate::auth::HubAuthority;

/// Where the Hub reads pull requests from and merges them through.
///
/// Built by the caller (`fl hub serve` reads `config.toml` and `$FL_GITHUB_*`); the library
/// reads neither, so a test's Hub never depends on the process environment.
#[derive(Clone, Default)]
pub struct HubGithub {
    /// The REST API base; `None` is `https://api.github.com`.
    pub api_url: Option<String>,
    /// Lowercase project slug → `owner/repo`.
    pub repos: BTreeMap<String, String>,
    /// A user token. Reads pull requests, and merges when there is no App.
    pub token: Option<String>,
    /// The Reviewer App: reviews and merges are made as `<slug>[bot]`.
    pub app_id: Option<u64>,
    pub private_key: Option<PathBuf>,
}

impl fmt::Debug for HubGithub {
    /// ⚠ Never prints the token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubGithub")
            .field("api_url", &self.api_url)
            .field("repos", &self.repos)
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .field("app_id", &self.app_id)
            .field("private_key", &self.private_key)
            .finish()
    }
}

/// Why the Hub could not get a GitHub client.
enum ClientError {
    /// No credential of the kind this action needs is configured.
    Missing(&'static str),
    /// A credential is configured but unusable (unreadable key, …). Detail stays in the log.
    Unusable,
}

impl HubGithub {
    /// The `owner/repo` a project's pull requests live in.
    pub fn repo_for(&self, project: &str) -> Option<&str> {
        self.repos
            .get(&project.to_ascii_lowercase())
            .map(String::as_str)
    }

    fn api(&self) -> &str {
        self.api_url.as_deref().unwrap_or(fl_github::DEFAULT_API)
    }

    fn token_client(&self) -> Option<Client> {
        let token = self.token.as_deref().filter(|t| !t.trim().is_empty())?;
        Some(Client::new(
            self.api(),
            Box::new(EnvToken::from_token("the Hub's GitHub token", token)),
        ))
    }

    fn app_client(&self, repo: &str) -> Result<Option<Client>, ClientError> {
        let (Some(app_id), Some(key)) = (self.app_id, self.private_key.as_deref()) else {
            return Ok(None);
        };
        match AppCredentials::from_file(self.api(), app_id, key, repo) {
            Ok(creds) => Ok(Some(Client::new(self.api(), Box::new(creds)))),
            Err(e) => {
                eprintln!("hub: the GitHub App credential is unusable: {e}");
                Err(ClientError::Unusable)
            }
        }
    }

    /// The client that reads a pull request: the token, else the App.
    fn reader(&self, repo: &str) -> Result<Client, ClientError> {
        if let Some(c) = self.token_client() {
            return Ok(c);
        }
        self.app_client(repo)?.ok_or(ClientError::Missing(
            "a GitHub token or a GitHub App is needed to read pull requests",
        ))
    }

    /// The client that merges: the Reviewer App, else the token.
    fn merger(&self, repo: &str) -> Result<Client, ClientError> {
        if let Some(c) = self.app_client(repo)? {
            return Ok(c);
        }
        self.token_client().ok_or(ClientError::Missing(
            "a GitHub App or a GitHub token is needed to merge",
        ))
    }

    /// The client that submits a review. A review that carries a verdict must come from the
    /// Reviewer App: only its reviews count as an agent's (see `ReviewerApps`), and GitHub
    /// refuses an approval from the pull request's own author. A plain comment may fall back
    /// to the token.
    fn reviewer(&self, repo: &str, needs_app: bool) -> Result<Client, ClientError> {
        if let Some(c) = self.app_client(repo)? {
            return Ok(c);
        }
        if needs_app {
            return Err(ClientError::Missing(
                "a GitHub App is needed to submit an approving or change-requesting review",
            ));
        }
        self.token_client().ok_or(ClientError::Missing(
            "a GitHub App or a GitHub token is needed to review",
        ))
    }
}

fn client_failure(e: ClientError) -> HttpResponse {
    match e {
        ClientError::Missing(what) => HttpResponse::json(
            503,
            json!({ "error": format!("GitHub is not configured on this Hub: {what}") }),
        ),
        ClientError::Unusable => HttpResponse::json(
            503,
            json!({ "error": "the Hub's GitHub credential is unusable; see the Hub log" }),
        ),
    }
}

fn github_failure(e: &StoreError) -> HttpResponse {
    HttpResponse::json(
        502,
        json!({ "error": format!("GitHub request failed: {e}") }),
    )
}

fn unconfigured_repo(project: &str) -> HttpResponse {
    HttpResponse::json(
        503,
        json!({
            "error": format!(
                "GitHub is not configured on this Hub: no repository is bound to project \
                 `{project}`"
            )
        }),
    )
}

/// The agent a request acts as, from the authority and the body's `by`.
///
/// An owner acts as itself; `by` is ignored. A peer that announced an agent name may not name a
/// different one in the body. A peer that announced none (`peer`) names its agent in `by`,
/// which the caller then looks up.
#[allow(clippy::result_large_err)]
fn bind_caller(authority: &HubAuthority, by: Option<&str>) -> Result<String, HttpResponse> {
    let actor = authority.actor_name().trim().to_ascii_lowercase();
    if authority.is_owner() {
        return Ok(actor);
    }
    let by = by
        .map(|b| b.trim().to_ascii_lowercase())
        .filter(|b| !b.is_empty());
    if actor == "peer" {
        return Ok(by.unwrap_or(actor));
    }
    match by {
        Some(b) if b != actor => Err(HttpResponse::json(
            403,
            json!({
                "error": format!("this request acts as `{actor}` and cannot name `{b}` in `by`")
            }),
        )),
        _ => Ok(actor),
    }
}

fn active_agent<'a>(agents: &'a [AgentRecord], name_key: &str) -> Option<&'a AgentRecord> {
    agents
        .iter()
        .find(|a| a.status == AgentStatus::Active && a.name_key == name_key)
}

/// The pull request's facts, read from GitHub.
struct PrFacts {
    pr: PullRequestView,
    commits: Vec<PrCommitSummary>,
    files: Vec<String>,
    reviews: Vec<PrReviewSummary>,
}

fn read_facts(
    client: &Client,
    repo: &str,
    number: u64,
    with_reviews: bool,
) -> Result<PrFacts, StoreError> {
    Ok(PrFacts {
        pr: get_pr(client, repo, number)?,
        commits: list_pr_commits(client, repo, number)?,
        files: list_pr_files(client, repo, number)?,
        reviews: if with_reviews {
            list_pr_reviews(client, repo, number)?
        } else {
            Vec::new()
        },
    })
}

/// A client-supplied head SHA that is set must be the one GitHub reports. `HEAD` and the empty
/// string mean "whatever it is now".
fn head_mismatch(supplied: Option<&str>, actual: &str) -> Option<HttpResponse> {
    let supplied = supplied
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("HEAD"))?;
    (supplied != actual).then(|| {
        HttpResponse::json(
            409,
            json!({
                "error": "head_sha does not match the pull request's current head",
                "head_sha": actual,
            }),
        )
    })
}

fn default_project() -> String {
    "ferroloop".to_string()
}

#[derive(Deserialize, Default)]
struct MergePayload {
    method: Option<String>,
    by: Option<String>,
    project: Option<String>,
    head_sha: Option<String>,
    number: Option<u64>,
}

/// `POST /api/v1/prs/:number/merge`, `POST /api/v1/projects/:project/prs/:number/merge` and
/// `POST /api/v1/projects/:project/pr/merge` (number in the body).
pub(crate) fn handle_post_pr_merge(
    state: &HubState,
    store: &AnyStore,
    authority: &HubAuthority,
    path_number: Option<u64>,
    path_project: Option<&str>,
    req: &HttpRequest,
) -> HttpResponse {
    let payload: MergePayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let Some(number) = path_number.or(payload.number) else {
        return HttpResponse::json(400, json!({ "error": "a pull request number is required" }));
    };
    let method = match payload.method.as_deref() {
        None => MergeMethod::Merge,
        Some(raw) => match MergeMethod::from_wire(raw) {
            Some(m) => m,
            None => {
                return HttpResponse::json(
                    400,
                    json!({ "error": format!("unknown merge method `{raw}`; use merge, squash or rebase") }),
                );
            }
        },
    };
    let project_slug = path_project
        .map(str::to_string)
        .or(payload.project.clone())
        .unwrap_or_else(default_project)
        .to_ascii_lowercase();

    let caller_name = match bind_caller(authority, payload.by.as_deref()) {
        Ok(n) => n,
        Err(resp) => return resp,
    };
    let agents = store.list_agents(None, true).unwrap_or_default();
    let caller = if authority.is_owner() {
        None
    } else {
        match active_agent(&agents, &caller_name) {
            Some(a) => Some(a),
            None => {
                return HttpResponse::json(
                    403,
                    json!({ "error": format!("unknown or inactive agent `{caller_name}`") }),
                );
            }
        }
    };

    let github = &state.config.github;
    let Some(repo) = github.repo_for(&project_slug) else {
        return unconfigured_repo(&project_slug);
    };
    let reader = match github.reader(repo) {
        Ok(c) => c,
        Err(e) => return client_failure(e),
    };
    let facts = match read_facts(&reader, repo, number, true) {
        Ok(f) => f,
        Err(e) => return github_failure(&e),
    };
    if facts.pr.merged || facts.pr.state != "open" {
        return HttpResponse::json(
            409,
            json!({ "error": format!("pull request #{number} is not open"), "state": facts.pr.state, "merged": facts.pr.merged }),
        );
    }
    if let Some(resp) = head_mismatch(payload.head_sha.as_deref(), &facts.pr.head_sha) {
        return resp;
    }

    let eval = evaluate_pr_policy(
        &project_slug,
        &facts.commits,
        &facts.files,
        &[],
        &agents,
        &state.config.owner_login,
    );
    let mut approved_by = None;
    if let Some(caller) = caller {
        if let Err(refusal) = authorize_agent_merge(&eval, caller) {
            return HttpResponse::json(
                403,
                json!({ "error": refusal.to_string(), "refusal": refusal }),
            );
        }
        match verify_merge_approvals(
            &facts.pr.head_sha,
            &eval,
            &facts.reviews,
            &agents,
            &state.config.owner_login,
            &state.config.reviewer_apps,
        ) {
            Ok(who) => approved_by = Some(who),
            Err(refusal) => {
                return HttpResponse::json(
                    403,
                    json!({ "error": refusal.to_string(), "refusal": refusal }),
                );
            }
        }
    }

    let merger = match github.merger(repo) {
        Ok(c) => c,
        Err(e) => return client_failure(e),
    };
    let merge_sha = match merge_pr(&merger, repo, number, &facts.pr.head_sha, method, None) {
        Ok(sha) => sha,
        Err(e) => return github_failure(&e),
    };
    HttpResponse::json(
        200,
        json!({
            "number": number,
            "merged": true,
            "merge_sha": merge_sha,
            "method": method,
            "merged_by": caller_name,
            "approved_by": approved_by,
            "head_sha": facts.pr.head_sha,
            "policy": eval,
        }),
    )
}

#[derive(Deserialize, Default)]
struct ReviewPayload {
    verdict: Option<String>,
    #[serde(default)]
    approve: bool,
    #[serde(default)]
    request_changes: bool,
    #[serde(default)]
    body: String,
    by: Option<String>,
    project: Option<String>,
    head_sha: Option<String>,
    // Read only when GitHub is not configured (the dry-run below); never when it is.
    #[serde(default)]
    commits: Vec<PrCommitSummary>,
    #[serde(default)]
    files: Vec<String>,
}

/// `POST /api/v1/prs/:number/review`.
///
/// With a repository bound to the project, the Hub reads the pull request from GitHub, checks
/// the caller may review it, and submits the review as the Reviewer App with the `fl-review`
/// marker on the real head SHA. With none bound it only formats the review (a dry run that
/// posts nothing, from the facts the caller sent).
pub(crate) fn handle_post_pr_review(
    state: &HubState,
    store: &AnyStore,
    authority: &HubAuthority,
    number: u64,
    req: &HttpRequest,
) -> HttpResponse {
    let payload: ReviewPayload = match req.json() {
        Ok(p) => p,
        Err(e) => return HttpResponse::json(400, json!({ "error": e })),
    };
    let verdict = verdict_of(&payload);
    let project_slug = payload
        .project
        .clone()
        .unwrap_or_else(default_project)
        .to_ascii_lowercase();
    let caller_name = match bind_caller(authority, payload.by.as_deref()) {
        Ok(n) => n,
        Err(resp) => return resp,
    };
    let agents = store.list_agents(None, true).unwrap_or_default();
    let caller = if authority.is_owner() {
        None
    } else {
        match active_agent(&agents, &caller_name) {
            Some(a) => Some(a),
            None => {
                return HttpResponse::json(
                    403,
                    json!({ "error": format!("unknown or inactive agent `{caller_name}`") }),
                );
            }
        }
    };

    let github = &state.config.github;
    let Some(repo) = github.repo_for(&project_slug) else {
        return dry_run_review(
            state,
            authority,
            number,
            verdict,
            &project_slug,
            &caller_name,
            caller,
            &agents,
            &payload,
        );
    };
    let reader = match github.reader(repo) {
        Ok(c) => c,
        Err(e) => return client_failure(e),
    };
    let facts = match read_facts(&reader, repo, number, false) {
        Ok(f) => f,
        Err(e) => return github_failure(&e),
    };
    if facts.pr.merged || facts.pr.state != "open" {
        return HttpResponse::json(
            409,
            json!({ "error": format!("pull request #{number} is not open") }),
        );
    }
    if let Some(resp) = head_mismatch(payload.head_sha.as_deref(), &facts.pr.head_sha) {
        return resp;
    }
    let eval = evaluate_pr_policy(
        &project_slug,
        &facts.commits,
        &facts.files,
        &[],
        &agents,
        &state.config.owner_login,
    );

    let Some(caller) = caller else {
        // The owner reviews on GitHub as itself; the Hub posts nothing for it.
        return HttpResponse::json(
            200,
            json!({
                "number": number,
                "verdict": verdict,
                "reviewer": caller_name,
                "head_sha": facts.pr.head_sha,
                "body": payload.body,
                "submitted": false,
                "policy": eval,
            }),
        );
    };
    let needs_app = matches!(
        verdict,
        ReviewVerdict::Approved | ReviewVerdict::ChangesRequested
    );
    if needs_app && let Err(refusal) = authorize_agent_action(&eval, caller) {
        return HttpResponse::json(
            403,
            json!({ "error": refusal.to_string(), "refusal": refusal }),
        );
    }
    let marker = FlReviewMarker {
        reviewer: caller.name_key.clone(),
        role: caller.role.clone(),
        project: eval.project.clone(),
        head_sha: facts.pr.head_sha.clone(),
        verdict,
        at: stamp::now(),
    };
    let formatted_body = format_review_body(&payload.body, &marker);
    let event = match verdict {
        ReviewVerdict::Approved => "APPROVE",
        ReviewVerdict::ChangesRequested => "REQUEST_CHANGES",
        ReviewVerdict::Commented => "COMMENT",
    };
    let submitter = match github.reviewer(repo, needs_app) {
        Ok(c) => c,
        Err(e) => return client_failure(e),
    };
    let review = match submit_pr_review(
        &submitter,
        repo,
        number,
        &facts.pr.head_sha,
        event,
        &formatted_body,
    ) {
        Ok(r) => r,
        Err(e) => return github_failure(&e),
    };
    HttpResponse::json(
        200,
        json!({
            "number": number,
            "verdict": verdict,
            "reviewer": caller.name_key,
            "role": caller.role,
            "head_sha": facts.pr.head_sha,
            "submitted": true,
            "review_id": review.id,
            "state": review.state,
            "author_login": review.author_login,
            "author_id": review.author_id,
            "policy": eval,
        }),
    )
}

fn verdict_of(payload: &ReviewPayload) -> ReviewVerdict {
    let says = |a: &str, b: &str| {
        payload
            .verdict
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case(a) || v.eq_ignore_ascii_case(b))
    };
    if payload.approve || says("approved", "approve") {
        ReviewVerdict::Approved
    } else if payload.request_changes || says("changes_requested", "request_changes") {
        ReviewVerdict::ChangesRequested
    } else {
        ReviewVerdict::Commented
    }
}

/// Said in every dry-run response: the policy there is the caller's claim, not GitHub's record.
const DRY_RUN_NOTE: &str = "nothing was posted to GitHub; the policy was computed from the commits and files the caller sent, not read from GitHub";

/// No repository is bound to the project: format the review from what the caller sent, post
/// nothing. A formatted body counts for nothing on GitHub unless the Reviewer App posts it.
#[allow(clippy::too_many_arguments)]
fn dry_run_review(
    state: &HubState,
    authority: &HubAuthority,
    number: u64,
    verdict: ReviewVerdict,
    project_slug: &str,
    caller_name: &str,
    caller: Option<&AgentRecord>,
    agents: &[AgentRecord],
    payload: &ReviewPayload,
) -> HttpResponse {
    let head_sha = payload
        .head_sha
        .clone()
        .unwrap_or_else(|| "HEAD".to_string());
    let eval = evaluate_pr_policy(
        project_slug,
        &payload.commits,
        &payload.files,
        &[],
        agents,
        &state.config.owner_login,
    );
    let Some(caller) = caller else {
        debug_assert!(authority.is_owner());
        return HttpResponse::json(
            200,
            json!({
                "number": number,
                "verdict": verdict,
                "reviewer": caller_name,
                "head_sha": head_sha,
                "body": payload.body,
                "submitted": false,
                "dry_run": true,
                "note": DRY_RUN_NOTE,
                "policy": eval,
            }),
        );
    };
    if matches!(
        verdict,
        ReviewVerdict::Approved | ReviewVerdict::ChangesRequested
    ) && let Err(refusal) = authorize_agent_action(&eval, caller)
    {
        return HttpResponse::json(
            403,
            json!({ "error": refusal.to_string(), "refusal": refusal }),
        );
    }
    let marker = FlReviewMarker {
        reviewer: caller.name_key.clone(),
        role: caller.role.clone(),
        project: eval.project.clone(),
        head_sha: head_sha.clone(),
        verdict,
        at: stamp::now(),
    };
    HttpResponse::json(
        200,
        json!({
            "number": number,
            "verdict": verdict,
            "reviewer": caller.name_key,
            "role": caller.role,
            "head_sha": head_sha,
            "body": format_review_body(&payload.body, &marker),
            "submitted": false,
            "dry_run": true,
            "note": DRY_RUN_NOTE,
            "policy": eval,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HubServerConfig, HubState};
    use fl_core::agent::{AgentRole, RegisterAgentArgs};
    use fl_core::pr_policy::{ReviewerApps, format_review_body};
    use fl_github::fake::{APP_BOT_ID, FakeGithub, FakePull, LOOKALIKE_ID};
    use serde_json::Value;
    use std::process::Command;
    use tempfile::TempDir;

    const PEER: &str = "peer-tok-secret";
    const OWNER: &str = "owner-tok-secret";
    const REPO: &str = "Artificial-Humanity/FerroLoop";

    fn marker(reviewer: &str, role: AgentRole, head: &str) -> FlReviewMarker {
        FlReviewMarker {
            reviewer: reviewer.into(),
            role,
            project: "ferroloop".into(),
            head_sha: head.into(),
            verdict: ReviewVerdict::Approved,
            at: fl_core::at::At::from_unix_millis(1),
        }
    }

    fn co_authored(sha: &str) -> PrCommitSummary {
        PrCommitSummary {
            sha: sha.into(),
            author_login: Some("artificially-human".into()),
            message: "feat: x\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>".into(),
        }
    }

    /// A Hub bound to a `FakeGithub` with pull request #1 (Lead tier, co-authored by `felicia`,
    /// head `sha1`) and agents `felicia` (assistant) and `ferris` (lead).
    struct Fixture {
        _tmp: TempDir,
        state: HubState,
        fake: FakeGithub,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let fake = FakeGithub::start(REPO);
            let state = HubState::new(HubServerConfig {
                store_path: tmp.path().join("store.redb"),
                repo_root: tmp.path().to_path_buf(),
                owner_login: "lmcfarlin".into(),
                reviewer_apps: ReviewerApps {
                    ids: vec![APP_BOT_ID],
                    logins: vec![],
                },
                github: HubGithub {
                    api_url: Some(fake.url()),
                    repos: BTreeMap::from([("ferroloop".to_string(), REPO.to_string())]),
                    token: Some("user-token".into()),
                    ..HubGithub::default()
                },
                owner_token: Some(OWNER.into()),
                peer_token: Some(PEER.into()),
                home_override: Some(tmp.path().to_path_buf()),
                ..HubServerConfig::default()
            });
            {
                let store = state.open_store().unwrap();
                for (name, role) in [
                    ("Felicia", AgentRole::Assistant),
                    ("Ferris", AgentRole::Lead),
                    ("Alex", AgentRole::Assistant),
                ] {
                    store
                        .register_agent(RegisterAgentArgs {
                            display_name: name.into(),
                            email: None,
                            project: "ferroloop".into(),
                            role,
                            extra_areas: vec![],
                            create_area: true,
                            replace: true,
                            at: stamp::now(),
                            by: "owner".into(),
                        })
                        .unwrap();
                }
            }
            let fixture = Self {
                _tmp: tmp,
                state,
                fake,
            };
            fixture.reset();
            fixture
        }

        /// Pull request #1, open and unreviewed.
        fn reset(&self) {
            self.fake.state().pulls.insert(
                1,
                FakePull {
                    number: 1,
                    title: "x".into(),
                    body: String::new(),
                    state: "open".into(),
                    head_ref: "feature".into(),
                    head_sha: "sha1".into(),
                    base_ref: "main".into(),
                    user_login: "artificially-human".into(),
                    merged: false,
                    merge_sha: None,
                    merge_method: None,
                    requested_reviewers: vec![],
                    commits: vec![co_authored("sha1")],
                    files: vec!["crates/core/src/lib.rs".into()],
                    reviews: vec![],
                },
            );
        }

        fn approve_as_app(&self, login: &str, id: Option<u64>) {
            self.fake.add_pull_review_as(
                1,
                login,
                id,
                "APPROVED",
                &format_review_body("ok", &marker("ferris", AgentRole::Lead, "sha1")),
                "sha1",
            );
        }

        fn post(&self, token: &str, agent: Option<&str>, path: &str, body: &Value) -> HttpResponse {
            let mut req = HttpRequest::new("POST", path)
                .with_header("Authorization", format!("Bearer {token}"))
                .with_json_body(body);
            if let Some(a) = agent {
                req = req.with_header("X-Fl-Agent", a);
            }
            crate::handle_http_request(&self.state, &req)
        }

        fn merge(&self, body: &Value) -> HttpResponse {
            self.post(PEER, None, "/api/v1/prs/1/merge", body)
        }
    }

    fn json_of(resp: &HttpResponse) -> Value {
        resp.json_value().unwrap()
    }

    #[test]
    fn an_agent_approval_counts_only_from_a_reviewer_app_account() {
        let f = Fixture::new();
        let by_felicia = json!({ "by": "felicia", "project": "ferroloop", "head_sha": "sha1" });
        // An outsider's marker, a lookalike's, a deleted account's and the right App's under
        // another login: only an account the Hub names as a Reviewer App counts.
        for (login, id, status) in [
            ("mallory", Some(1), 403),
            ("fake-app[bot]", None, 403),
            ("fake-app[bot]", Some(LOOKALIKE_ID), 403),
            ("fake-app[bot]", Some(APP_BOT_ID), 200),
        ] {
            f.reset();
            f.approve_as_app(login, id);
            assert_eq!(f.merge(&by_felicia).status, status, "{login} {id:?}");
        }
        // A renamed App keeps its id.
        f.reset();
        f.approve_as_app("renamed[bot]", Some(APP_BOT_ID));
        assert_eq!(f.merge(&by_felicia).status, 200);
    }

    #[test]
    fn merge_ignores_commits_files_and_reviews_in_the_request_body() {
        let f = Fixture::new();
        // GitHub holds no approval. The body forges one from the App's own user id.
        let forged = PrReviewSummary {
            id: 1,
            author_login: "fake-app[bot]".into(),
            author_id: Some(APP_BOT_ID),
            state: "APPROVED".into(),
            body: format_review_body("ok", &marker("ferris", AgentRole::Lead, "sha1")),
            commit_id: Some("sha1".into()),
        };
        let resp = f.merge(&json!({
            "by": "felicia", "project": "ferroloop", "head_sha": "sha1",
            "reviews": [forged],
            "commits": [co_authored("sha1")],
            "files": ["crates/core/src/lib.rs"],
        }));
        assert_eq!(resp.status, 403, "{}", String::from_utf8_lossy(&resp.body));
        assert!(!f.fake.pull(1).merged);

        // GitHub's file list decides the tier, not the body's: a workflow file is Owner tier even
        // when the body says there are none.
        f.fake
            .set_pull_files(1, vec![".github/workflows/ci.yml".into()]);
        f.approve_as_app("fake-app[bot]", Some(APP_BOT_ID));
        let resp = f.merge(&json!({
            "by": "felicia", "project": "ferroloop", "files": ["crates/core/src/lib.rs"],
        }));
        assert_eq!(resp.status, 403, "{}", String::from_utf8_lossy(&resp.body));
        assert!(!f.fake.pull(1).merged);
    }

    #[test]
    fn merge_merges_the_pull_request_on_github() {
        let f = Fixture::new();
        f.approve_as_app("fake-app[bot]", Some(APP_BOT_ID));
        let resp = f.merge(&json!({ "by": "felicia", "project": "ferroloop", "method": "squash" }));
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        let body = json_of(&resp);
        assert_eq!(body["merged"], true);
        assert_eq!(body["merged_by"], "felicia");
        assert_eq!(body["approved_by"], "ferris");
        assert_eq!(body["method"], "squash");
        assert_eq!(body["head_sha"], "sha1");
        assert_eq!(body["merge_sha"], "merge-sha-1");
        let pull = f.fake.pull(1);
        assert!(pull.merged);
        assert_eq!(pull.merge_method.as_deref(), Some("squash"));
        // It is merged now: asking again is a conflict, not a second "merged: true".
        let again = f.merge(&json!({ "by": "felicia", "project": "ferroloop" }));
        assert_eq!(again.status, 409);
    }

    #[test]
    fn an_agent_who_did_not_author_the_pull_request_cannot_merge_it() {
        let f = Fixture::new();
        f.approve_as_app("fake-app[bot]", Some(APP_BOT_ID));
        // `alex` is an active assistant, but not a co-author of pull request #1.
        let resp = f.merge(&json!({ "by": "alex", "project": "ferroloop" }));
        assert_eq!(resp.status, 403, "{}", String::from_utf8_lossy(&resp.body));
        assert!(!f.fake.pull(1).merged);
        // Nor can a name that is no agent at all.
        assert_eq!(
            f.merge(&json!({ "by": "nobody", "project": "ferroloop" }))
                .status,
            403
        );
    }

    #[test]
    fn owner_merges_without_approvals_but_still_on_github() {
        let f = Fixture::new();
        let resp = f.post(
            OWNER,
            None,
            "/api/v1/prs/1/merge",
            &json!({ "by": "someone-else" }),
        );
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        assert_eq!(json_of(&resp)["merged_by"], "lmcfarlin");
        assert!(f.fake.pull(1).merged);
    }

    #[test]
    fn merge_refuses_when_github_is_not_configured() {
        let mut f = Fixture::new();
        f.state.config.github = HubGithub::default();
        let resp = f.merge(&json!({ "by": "felicia", "project": "ferroloop" }));
        assert_eq!(resp.status, 503);
        assert!(
            json_of(&resp)["error"]
                .as_str()
                .unwrap()
                .contains("not configured")
        );
        // A project with no bound repository is the same refusal, and so is a Hub with no
        // credential.
        let mut f = Fixture::new();
        f.state.config.github.token = None;
        assert_eq!(
            f.merge(&json!({ "by": "felicia", "project": "ferroloop" }))
                .status,
            503
        );
        assert_eq!(
            f.merge(&json!({ "by": "felicia", "project": "elsewhere" }))
                .status,
            503
        );
        assert!(!f.fake.pull(1).merged);
    }

    #[test]
    fn merge_checks_head_sha_method_and_state() {
        let f = Fixture::new();
        f.approve_as_app("fake-app[bot]", Some(APP_BOT_ID));
        let base = |extra: Value| {
            let mut b = json!({ "by": "felicia", "project": "ferroloop" });
            for (k, v) in extra.as_object().unwrap() {
                b[k] = v.clone();
            }
            b
        };
        assert_eq!(f.merge(&base(json!({ "head_sha": "stale" }))).status, 409);
        assert_eq!(
            f.merge(&base(json!({ "method": "fast-forward" }))).status,
            400
        );
        assert_eq!(
            f.merge(&json!({ "by": "felicia" , "number": "x" })).status,
            400
        );
        assert!(!f.fake.pull(1).merged);
        // `HEAD` and a blank mean "the current head".
        assert_eq!(f.merge(&base(json!({ "head_sha": "HEAD" }))).status, 200);
        // A closed pull request is not merged, whoever asks.
        f.reset();
        f.fake.state().pulls.get_mut(&1).unwrap().state = "closed".into();
        assert_eq!(
            f.post(OWNER, None, "/api/v1/prs/1/merge", &json!({}))
                .status,
            409
        );
    }

    #[test]
    fn a_peer_cannot_name_another_agent_in_by() {
        let f = Fixture::new();
        f.approve_as_app("fake-app[bot]", Some(APP_BOT_ID));
        let body = json!({ "by": "felicia", "project": "ferroloop" });
        // Announced as ferris, naming felicia: refused, and nothing merged.
        let resp = f.post(PEER, Some("ferris"), "/api/v1/prs/1/merge", &body);
        assert_eq!(resp.status, 403, "{}", String::from_utf8_lossy(&resp.body));
        assert!(!f.fake.pull(1).merged);
        // Announced as felicia, with or without `by`: she merges as herself.
        let resp = f.post(
            PEER,
            Some("felicia"),
            "/api/v1/prs/1/merge",
            &json!({ "project": "ferroloop" }),
        );
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        assert_eq!(json_of(&resp)["merged_by"], "felicia");
    }

    #[test]
    fn project_scoped_merge_aliases_reach_the_same_handler() {
        let f = Fixture::new();
        f.approve_as_app("fake-app[bot]", Some(APP_BOT_ID));
        let resp = f.post(
            PEER,
            Some("felicia"),
            "/api/v1/projects/ferroloop/prs/1/merge",
            &json!({}),
        );
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        f.reset();
        f.approve_as_app("fake-app[bot]", Some(APP_BOT_ID));
        let resp = f.post(
            PEER,
            Some("felicia"),
            "/api/v1/projects/ferroloop/pr/merge",
            &json!({ "number": 1 }),
        );
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        // No number anywhere is a bad request.
        let resp = f.post(
            PEER,
            Some("felicia"),
            "/api/v1/projects/ferroloop/pr/merge",
            &json!({}),
        );
        assert_eq!(resp.status, 400);
    }

    fn rsa_pem() -> String {
        let out = Command::new("openssl")
            .args(["genrsa", "-traditional", "2048"])
            .output()
            .expect("tests require the `openssl` CLI");
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn review_is_submitted_by_the_app_on_the_head_github_reports() {
        let mut f = Fixture::new();
        let key = f._tmp.path().join("app.pem");
        std::fs::write(&key, rsa_pem()).unwrap();
        f.state.config.github.app_id = Some(42);
        f.state.config.github.private_key = Some(key);
        // The body's commits and files are not what the review is judged on, and a stale
        // head_sha is refused.
        let body = json!({
            "by": "ferris", "project": "ferroloop", "approve": true, "body": "ok",
            "files": [".github/workflows/ci.yml"], "commits": [],
        });
        let resp = f.post(PEER, None, "/api/v1/prs/1/review", &body);
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        let out = json_of(&resp);
        assert_eq!(out["submitted"], true);
        assert_eq!(out["head_sha"], "sha1");
        let review = f.fake.pull(1).reviews.pop().expect("a review on GitHub");
        assert_eq!(review.state, "APPROVED");
        assert_eq!(review.author_id, Some(APP_BOT_ID));
        let marker = fl_core::pr_policy::extract_review_marker(&review.body).expect("a marker");
        assert_eq!(marker.reviewer, "ferris");
        assert_eq!(marker.head_sha, "sha1");
        // The approval it left is one the merge accepts.
        let merged = f.merge(&json!({ "by": "felicia", "project": "ferroloop" }));
        assert_eq!(
            merged.status,
            200,
            "{}",
            String::from_utf8_lossy(&merged.body)
        );
        // Another agent's name in `by` is refused here too, and a stale head is a conflict.
        f.reset();
        let resp = f.post(PEER, Some("felicia"), "/api/v1/prs/1/review", &body);
        assert_eq!(resp.status, 403);
        let mut stale = body.clone();
        stale["head_sha"] = json!("old");
        assert_eq!(
            f.post(PEER, None, "/api/v1/prs/1/review", &stale).status,
            409
        );
    }

    #[test]
    fn an_approving_review_needs_the_app_and_a_qualified_reviewer() {
        let f = Fixture::new();
        // The token alone cannot make a review that counts as an agent's.
        let approve = json!({ "by": "ferris", "project": "ferroloop", "approve": true });
        assert_eq!(
            f.post(PEER, None, "/api/v1/prs/1/review", &approve).status,
            503
        );
        // A plain comment may use the token.
        let comment = json!({ "by": "ferris", "project": "ferroloop", "body": "note" });
        assert_eq!(
            f.post(PEER, None, "/api/v1/prs/1/review", &comment).status,
            200
        );
        // An assistant who co-authored the pull request cannot approve it.
        let own = json!({ "by": "felicia", "project": "ferroloop", "approve": true });
        assert_eq!(f.post(PEER, None, "/api/v1/prs/1/review", &own).status, 403);
    }

    #[test]
    fn review_without_a_bound_repository_posts_nothing() {
        let mut f = Fixture::new();
        f.state.config.github = HubGithub::default();
        let resp = f.post(
            PEER,
            None,
            "/api/v1/prs/1/review",
            &json!({
                "by": "ferris", "project": "ferroloop", "approve": true, "head_sha": "abc",
                "files": ["crates/core/src/lib.rs"], "commits": [co_authored("abc")],
            }),
        );
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        let body = json_of(&resp);
        assert_eq!(body["submitted"], false);
        assert_eq!(body["dry_run"], true);
        assert_eq!(body["note"], DRY_RUN_NOTE);
        assert!(
            body["note"]
                .as_str()
                .unwrap()
                .contains("not read from GitHub")
        );
        assert!(f.fake.pull(1).reviews.is_empty());

        // The owner's dry-run response says the same.
        let resp = f.post(
            OWNER,
            None,
            "/api/v1/prs/1/review",
            &json!({
                "project": "ferroloop", "approve": true, "head_sha": "abc",
                "files": ["crates/core/src/lib.rs"], "commits": [co_authored("abc")],
            }),
        );
        assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
        let body = json_of(&resp);
        assert_eq!(body["dry_run"], true);
        assert!(
            body["note"]
                .as_str()
                .unwrap()
                .contains("nothing was posted")
        );
    }

    #[test]
    fn the_debug_output_never_shows_the_token() {
        let g = HubGithub {
            token: Some("s3cret-token".into()),
            ..HubGithub::default()
        };
        assert!(!format!("{g:?}").contains("s3cret"));
    }
}
