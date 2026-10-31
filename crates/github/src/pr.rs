//! Pull-request REST operations over [`Client`] (GitHub hierarchy spec §2).
//!
//! Supports dual-credential routing: standard tracker credentials for PR
//! creation and metadata inspection, and Reviewer App (`<slug>[bot]`)
//! credentials for submitting formal reviews and merging pull requests.

use crate::client::{Client, Method};
use fl_core::StoreError;
pub use fl_core::pr_policy::{PrCommitSummary, PrReviewSummary};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::str::FromStr;

/// Pull request summary returned by GitHub's REST API.
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

impl PullRequestView {
    /// Parse a GitHub REST pull-request JSON payload.
    pub fn from_github_json(v: &Value) -> Result<Self, StoreError> {
        let number = v.get("number").and_then(Value::as_u64).ok_or_else(|| {
            StoreError::Backend("GitHub sent a pull request without `number`".into())
        })?;
        let html_url = v
            .get("html_url")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent pull request #{number} without `html_url`"
                ))
            })?
            .to_string();
        let state = v
            .get("state")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent pull request #{number} without `state`"
                ))
            })?
            .to_string();
        let title = v
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent pull request #{number} without `title`"
                ))
            })?
            .to_string();
        let body = v
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let head_ref = v
            .pointer("/head/ref")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent pull request #{number} without `head.ref`"
                ))
            })?
            .to_string();
        let head_sha = v
            .pointer("/head/sha")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent pull request #{number} without `head.sha`"
                ))
            })?
            .to_string();
        let base_ref = v
            .pointer("/base/ref")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent pull request #{number} without `base.ref`"
                ))
            })?
            .to_string();
        let user_login = v
            .pointer("/user/login")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent pull request #{number} without `user.login`"
                ))
            })?
            .to_string();
        let merged = v.get("merged").and_then(Value::as_bool).unwrap_or(false);
        let requested_reviewers = v
            .get("requested_reviewers")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        item.get("login")
                            .and_then(Value::as_str)
                            .or_else(|| item.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            number,
            html_url,
            state,
            title,
            body,
            head_ref,
            head_sha,
            base_ref,
            user_login,
            merged,
            requested_reviewers,
        })
    }
}

/// Strategy used when merging a pull request via `PUT /repos/{o}/{r}/pulls/{n}/merge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub const ALL: &'static [Self] = &[Self::Merge, Self::Squash, Self::Rebase];

    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "merge" => Some(Self::Merge),
            "squash" => Some(Self::Squash),
            "rebase" => Some(Self::Rebase),
            _ => None,
        }
    }
}

impl fmt::Display for MergeMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

impl FromStr for MergeMethod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_wire(s).ok_or_else(|| {
            format!("invalid merge method `{s}` (expected `merge`, `squash`, or `rebase`)")
        })
    }
}

fn get_paginated(client: &Client, initial_path: &str) -> Result<Vec<Value>, StoreError> {
    client.get_all(initial_path)
}

/// The login GitHub shows for a deleted account.
const GHOST_LOGIN: &str = "ghost";

fn parse_review_summary(v: &Value) -> Result<PrReviewSummary, StoreError> {
    let id = v.get("id").and_then(Value::as_u64).ok_or_else(|| {
        StoreError::Backend("GitHub sent a pull-request review without `id`".into())
    })?;
    // GitHub sends `"user": null` for a review whose author's account was
    // deleted. That review must not make the whole listing fail (and with it
    // policy, status and merge for the PR); it is attributed to GitHub's own
    // `ghost` placeholder, which no registered agent or owner can be.
    let author_login = match v.get("user") {
        Some(Value::Null) => GHOST_LOGIN.to_string(),
        _ => v
            .pointer("/user/login")
            .or_else(|| v.get("author_login"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent pull-request review {id} without `user.login`"
                ))
            })?
            .to_string(),
    };
    let state = v
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            StoreError::Backend(format!(
                "GitHub sent pull-request review {id} without `state`"
            ))
        })?
        .to_string();
    let body = v
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let commit_id = v
        .get("commit_id")
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok(PrReviewSummary {
        id,
        author_login,
        state,
        body,
        commit_id,
    })
}

/// Create a pull request (`POST /repos/{owner}/{repo}/pulls`).
pub fn create_pr(
    client: &Client,
    repo: &str,
    title: &str,
    body: &str,
    head: &str,
    base: &str,
) -> Result<PullRequestView, StoreError> {
    let payload = json!({
        "title": title,
        "body": body,
        "head": head,
        "base": base,
    });
    let reply = client.send(
        Method::Post,
        &format!("/repos/{repo}/pulls"),
        Some(&payload),
    )?;
    if !matches!(reply.status, 200 | 201) {
        return Err(StoreError::Backend(format!(
            "GitHub answered {} when creating a pull request in `{repo}`",
            reply.status
        )));
    }
    PullRequestView::from_github_json(&reply.body)
}

/// Fetch a pull request (`GET /repos/{owner}/{repo}/pulls/{number}`).
pub fn get_pr(client: &Client, repo: &str, number: u64) -> Result<PullRequestView, StoreError> {
    let reply = client.send(Method::Get, &format!("/repos/{repo}/pulls/{number}"), None)?;
    if reply.status != 200 {
        return Err(StoreError::Backend(format!(
            "GitHub answered {} when reading pull request #{number} of `{repo}`",
            reply.status
        )));
    }
    PullRequestView::from_github_json(&reply.body)
}

/// List all commits on a pull request (`GET /repos/{owner}/{repo}/pulls/{number}/commits`).
pub fn list_pr_commits(
    client: &Client,
    repo: &str,
    number: u64,
) -> Result<Vec<PrCommitSummary>, StoreError> {
    let items = get_paginated(
        client,
        &format!("/repos/{repo}/pulls/{number}/commits?per_page=100"),
    )?;
    if items.len() >= PR_COMMITS_CAP {
        return Err(StoreError::Backend(format!(
            "pull request #{number} lists {} commits, the most GitHub returns; the rest cannot \
             be checked for attribution",
            items.len()
        )));
    }
    let mut commits = Vec::with_capacity(items.len());
    for item in items {
        let sha = item
            .get("sha")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent a commit on pull request #{number} without `sha`"
                ))
            })?
            .to_string();
        let author_login = item
            .pointer("/author/login")
            .and_then(Value::as_str)
            .map(str::to_string);
        let message = item
            .pointer("/commit/message")
            .or_else(|| item.get("message"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent commit `{sha}` on pull request #{number} without `commit.message`"
                ))
            })?
            .to_string();
        commits.push(PrCommitSummary {
            sha,
            author_login,
            message,
        });
    }
    Ok(commits)
}

/// GitHub's `GET /pulls/{n}/files` stops at this many files, without saying so.
const PR_FILES_CAP: usize = 3000;
/// GitHub's `GET /pulls/{n}/commits` stops at this many commits, without saying so.
const PR_COMMITS_CAP: usize = 250;

/// List all changed file paths on a pull request (`GET /repos/{owner}/{repo}/pulls/{number}/files`).
///
/// A renamed file is listed under BOTH its new and its old path: the old one
/// may be protected (`.github/…` renamed out of it). A listing GitHub has cut
/// at its cap is an error, not a short list that hides what lies past it.
pub fn list_pr_files(client: &Client, repo: &str, number: u64) -> Result<Vec<String>, StoreError> {
    let items = get_paginated(
        client,
        &format!("/repos/{repo}/pulls/{number}/files?per_page=100"),
    )?;
    files_from_items(&items, number)
}

fn files_from_items(items: &[Value], number: u64) -> Result<Vec<String>, StoreError> {
    if items.len() >= PR_FILES_CAP {
        return Err(StoreError::Backend(format!(
            "pull request #{number} lists {} files, the most GitHub returns; the rest cannot be \
             checked against the protected paths",
            items.len()
        )));
    }
    let mut files = Vec::with_capacity(items.len());
    for item in items {
        let filename = item
            .get("filename")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub sent a file entry on pull request #{number} without `filename`"
                ))
            })?
            .to_string();
        if let Some(old) = item.get("previous_filename").and_then(Value::as_str)
            && old != filename
        {
            files.push(old.to_string());
        }
        files.push(filename);
    }
    Ok(files)
}

/// List all reviews on a pull request (`GET /repos/{owner}/{repo}/pulls/{number}/reviews`).
pub fn list_pr_reviews(
    client: &Client,
    repo: &str,
    number: u64,
) -> Result<Vec<PrReviewSummary>, StoreError> {
    let items = get_paginated(
        client,
        &format!("/repos/{repo}/pulls/{number}/reviews?per_page=100"),
    )?;
    let mut reviews = Vec::with_capacity(items.len());
    for item in items {
        reviews.push(parse_review_summary(&item)?);
    }
    Ok(reviews)
}

/// Request reviewers on a pull request (`POST /repos/{owner}/{repo}/pulls/{number}/requested_reviewers`).
pub fn request_pr_reviewers(
    client: &Client,
    repo: &str,
    number: u64,
    reviewers: &[String],
) -> Result<(), StoreError> {
    if reviewers.is_empty() {
        return Ok(());
    }
    let payload = json!({
        "reviewers": reviewers,
    });
    let reply = client.send(
        Method::Post,
        &format!("/repos/{repo}/pulls/{number}/requested_reviewers"),
        Some(&payload),
    )?;
    if !matches!(reply.status, 200 | 201) {
        return Err(StoreError::Backend(format!(
            "GitHub answered {} when requesting reviewers on pull request #{number} of `{repo}`",
            reply.status
        )));
    }
    Ok(())
}

/// Submit a pull-request review (`POST /repos/{owner}/{repo}/pulls/{number}/reviews`).
pub fn submit_pr_review(
    client: &Client,
    repo: &str,
    number: u64,
    commit_id: &str,
    event: &str,
    body: &str,
) -> Result<PrReviewSummary, StoreError> {
    let payload = json!({
        "commit_id": commit_id,
        "event": event,
        "body": body,
    });
    let reply = client.send(
        Method::Post,
        &format!("/repos/{repo}/pulls/{number}/reviews"),
        Some(&payload),
    )?;
    if !matches!(reply.status, 200 | 201) {
        return Err(StoreError::Backend(format!(
            "GitHub answered {} when submitting a review on pull request #{number} of `{repo}`",
            reply.status
        )));
    }
    parse_review_summary(&reply.body)
}

/// Merge a pull request (`PUT /repos/{owner}/{repo}/pulls/{number}/merge`).
pub fn merge_pr(
    client: &Client,
    repo: &str,
    number: u64,
    head_sha: &str,
    method: MergeMethod,
    commit_title: Option<&str>,
) -> Result<String, StoreError> {
    let mut payload = json!({
        "sha": head_sha,
        "merge_method": method.as_wire(),
    });
    if let Some(title) = commit_title {
        payload["commit_title"] = json!(title);
    }
    let reply = client.send(
        Method::Put,
        &format!("/repos/{repo}/pulls/{number}/merge"),
        Some(&payload),
    )?;
    if reply.status != 200 || reply.body.get("merged").and_then(Value::as_bool) == Some(false) {
        return Err(StoreError::Backend(format!(
            "GitHub answered {} when merging pull request #{number} of `{repo}`",
            reply.status
        )));
    }
    reply
        .body
        .get("sha")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            StoreError::Backend(format!(
                "GitHub merged pull request #{number} of `{repo}` without returning `sha`"
            ))
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_renamed_file_lists_its_old_path_too() {
        let items = vec![serde_json::json!({
            "filename": "x.yml",
            "previous_filename": ".github/workflows/ci.yml",
            "status": "renamed"
        })];
        let files = files_from_items(&items, 1).unwrap();
        assert!(files.contains(&".github/workflows/ci.yml".to_string()));
        assert!(files.contains(&"x.yml".to_string()));
    }

    #[test]
    fn a_file_listing_at_githubs_cap_is_an_error() {
        let items: Vec<Value> = (0..PR_FILES_CAP)
            .map(|i| serde_json::json!({ "filename": format!("f{i}") }))
            .collect();
        assert!(files_from_items(&items, 1).is_err());
        assert!(files_from_items(&items[..PR_FILES_CAP - 1], 1).is_ok());
    }

    #[test]
    fn a_review_by_a_deleted_user_parses_as_ghost() {
        let v = serde_json::json!({
            "id": 9, "user": null, "state": "APPROVED", "body": "", "commit_id": "abc"
        });
        assert_eq!(parse_review_summary(&v).unwrap().author_login, "ghost");
    }

    use super::*;
    use crate::creds::{AppCredentials, EnvToken, throwaway_key};
    use crate::fake::{APP_SLUG, FakeGithub, USER_LOGIN};

    fn user_client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("user-token".into())).unwrap()),
        )
    }

    fn app_client(fake: &FakeGithub, repo: &str) -> Client {
        let app = AppCredentials::new(&fake.url(), 42, &throwaway_key(true), repo).unwrap();
        Client::new(&fake.url(), Box::new(app))
    }

    #[test]
    fn merge_method_wire_and_from_str_round_trip() {
        for method in MergeMethod::ALL {
            let wire = method.as_wire();
            assert_eq!(method.to_string(), wire);
            assert_eq!(MergeMethod::from_str(wire).unwrap(), *method);
            assert_eq!(
                serde_json::to_string(method).unwrap(),
                format!("\"{wire}\"")
            );
        }
        assert!(MergeMethod::from_str("fast-forward").is_err());
    }

    #[test]
    fn pr_lifecycle_over_fake_github_enforces_self_review_and_head_sha_guards() {
        let repo = "acme/widgets";
        let fake = FakeGithub::start(repo);
        let author = user_client(&fake);
        let reviewer_app = app_client(&fake, repo);

        fake.seed_branch(
            "felicia/fix-1",
            "sha-111",
            vec![
                PrCommitSummary {
                    sha: "sha-100".into(),
                    author_login: Some(USER_LOGIN.into()),
                    message:
                        "feat: part 1\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                            .into(),
                },
                PrCommitSummary {
                    sha: "sha-111".into(),
                    author_login: Some(USER_LOGIN.into()),
                    message:
                        "feat: part 2\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                            .into(),
                },
            ],
            vec![
                "crates/core/src/lib.rs".into(),
                "crates/github/src/pr.rs".into(),
            ],
        );

        // 1. Create PR as `fake-user`
        let created = create_pr(
            &author,
            repo,
            "Add PR hierarchy support",
            "Implements hierarchy.",
            "felicia/fix-1",
            "main",
        )
        .unwrap();
        assert_eq!(created.number, 1);
        assert_eq!(created.user_login, USER_LOGIN);
        assert_eq!(created.head_ref, "felicia/fix-1");
        assert_eq!(created.head_sha, "sha-111");
        assert_eq!(created.base_ref, "main");
        assert!(!created.merged);

        // 2. Request reviewers
        request_pr_reviewers(&author, repo, 1, &["lmcfarlin".into()]).unwrap();
        let fetched = get_pr(&author, repo, 1).unwrap();
        assert_eq!(fetched.requested_reviewers, vec!["lmcfarlin"]);

        // 3. Verify pagination on commits and files (`max_per_page = 1`)
        fake.state().max_per_page = 1;
        let commits = list_pr_commits(&author, repo, 1).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "sha-100");
        assert_eq!(commits[1].sha, "sha-111");

        let files = list_pr_files(&author, repo, 1).unwrap();
        assert_eq!(
            files,
            vec![
                "crates/core/src/lib.rs".to_string(),
                "crates/github/src/pr.rs".to_string()
            ]
        );

        // 4. Platform self-review refusal (`422`) when `fake-user` tries to APPROVE or REQUEST_CHANGES
        let self_approve_err =
            submit_pr_review(&author, repo, 1, "sha-111", "APPROVE", "Self LGTM").unwrap_err();
        assert!(
            self_approve_err.to_string().contains("422")
                && self_approve_err
                    .to_string()
                    .contains("Cannot approve or request changes on your own pull request"),
            "unexpected error: {self_approve_err}"
        );

        // Commenting on own PR is allowed by GitHub
        let comment_rev =
            submit_pr_review(&author, repo, 1, "sha-111", "COMMENT", "Author note").unwrap();
        assert_eq!(comment_rev.state, "COMMENTED");
        assert_eq!(comment_rev.author_login, USER_LOGIN);

        // 5. Approving via `AppCredentials` (`fake-app[bot]`) succeeds with `200`
        let app_rev = submit_pr_review(
            &reviewer_app,
            repo,
            1,
            "sha-111",
            "APPROVE",
            "Approved by Ferris",
        )
        .unwrap();
        assert_eq!(app_rev.author_login, format!("{APP_SLUG}[bot]"));
        assert_eq!(app_rev.state, "APPROVED");
        assert_eq!(app_rev.commit_id.as_deref(), Some("sha-111"));

        let reviews = list_pr_reviews(&author, repo, 1).unwrap();
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].state, "COMMENTED");
        assert_eq!(reviews[1].state, "APPROVED");

        // 6. Stale-SHA merge refusal (`409 Conflict`)
        let stale_merge_err = merge_pr(
            &reviewer_app,
            repo,
            1,
            "sha-stale",
            MergeMethod::Squash,
            None,
        )
        .unwrap_err();
        assert!(
            stale_merge_err.to_string().contains("409"),
            "unexpected error: {stale_merge_err}"
        );
        assert!(!fake.pull(1).merged);

        // 7. Merge with matching `head_sha` succeeds
        let merge_sha = merge_pr(
            &reviewer_app,
            repo,
            1,
            "sha-111",
            MergeMethod::Squash,
            Some("Add PR hierarchy support (#1)"),
        )
        .unwrap();
        assert!(!merge_sha.is_empty());

        let after_merge = get_pr(&author, repo, 1).unwrap();
        assert!(after_merge.merged);
        assert_eq!(after_merge.state, "closed");
        assert_eq!(fake.pull(1).merge_method.as_deref(), Some("squash"));
    }
}
