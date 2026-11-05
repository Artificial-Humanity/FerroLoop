//! GitHub Issues as an fl tracker (GitHub tracker spec). The only crate
//! that talks to GitHub.

pub mod client;
pub mod creds;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
#[cfg(any(test, feature = "fake"))]
pub mod fake_git;
pub mod feedback;
pub mod ledger;
pub mod meta;
pub mod owner;
pub mod pr;
pub mod tracker;

pub use client::{Client, DEFAULT_API, GraphqlAnswer, Method, Reply};
pub use creds::{AppCredentials, Credentials, EnvToken, FileKeySigner, JwtSigner};
pub use feedback::{
    FeedbackNotice, PrCheckFact, PrFeedbackEvent, PrFeedbackPoller, PrFeedbackSnapshot,
    author_persona,
};
pub use ledger::GithubLedger;
pub use pr::{
    MergeMethod, PrCommitSummary, PrReviewSummary, PullRequestView, create_pr, get_pr,
    list_check_runs, list_open_prs, list_pr_commits, list_pr_files, list_pr_reviews, merge_pr,
    request_pr_reviewers, submit_pr_review,
};
pub use tracker::{GithubTracker, Notice, Repaired, Repo};

/// Whether `s` names a repository as `owner/repo`: exactly two non-empty
/// parts of `[A-Za-z0-9._-]`, neither `.` nor `..`. The config's `github = …` and a typed
/// `owner/repo#41` follow the same rule.
pub fn is_owner_repo(s: &str) -> bool {
    // Each part is spliced into API URLs and IRIs, so it is limited to the
    // characters GitHub names use; `.`/`..` would traverse and `?#%` would
    // rewrite the request.
    let part = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    s.split('/').count() == 2 && s.split('/').all(part)
}
