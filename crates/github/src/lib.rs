//! GitHub Issues as an fl tracker (GitHub tracker spec). The only crate
//! that talks to GitHub.

pub mod client;
pub mod creds;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
#[cfg(any(test, feature = "fake"))]
pub mod fake_git;
pub mod ledger;
pub mod meta;
pub mod owner;
pub mod pr;
pub mod tracker;

pub use client::{Client, DEFAULT_API, GraphqlAnswer, Method, Reply};
pub use creds::{AppCredentials, Credentials, EnvToken};
pub use ledger::GithubLedger;
pub use pr::{
    MergeMethod, PrCommitSummary, PrReviewSummary, PullRequestView, create_pr, get_pr,
    list_pr_commits, list_pr_files, list_pr_reviews, merge_pr, request_pr_reviewers,
    submit_pr_review,
};
pub use tracker::{GithubTracker, Notice, Repaired, Repo};
