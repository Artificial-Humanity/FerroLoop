//! Route a failing CI check or a `CHANGES_REQUESTED` review to the agent who
//! authored the pull request.
//!
//! This module does no I/O. A caller supplies the pull-request facts and the
//! names of active agents, and keeps the [`PrFeedbackPoller`] so one failing
//! check or one review is announced once per commit.

use fl_core::pr_policy::{PrCommitSummary, PrReviewSummary, parse_co_authors};
use serde::Serialize;
use std::collections::HashSet;

/// One completed check run on a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrCheckFact {
    pub id: u64,
    pub name: String,
    /// GitHub's `conclusion`, such as `failure` or `timed_out`.
    pub conclusion: String,
    pub details_url: String,
    /// The commit the check ran against.
    pub head_sha: String,
}

/// The facts one poll saw for one open pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFeedbackSnapshot {
    pub number: u64,
    pub branch: String,
    pub head_sha: String,
    pub commits: Vec<PrCommitSummary>,
    pub checks: Vec<PrCheckFact>,
    pub reviews: Vec<PrReviewSummary>,
}

/// What the author is told to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrFeedbackEvent {
    CiFailure {
        check_id: u64,
        check_name: String,
        conclusion: String,
        details_url: String,
    },
    ChangesRequested {
        review_id: u64,
        reviewer: String,
        excerpt: String,
    },
}

/// A typed FerroWire body. The body is data. It is not a command to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrFeedbackIntent {
    pub intent: &'static str,
    pub pr_number: u64,
    pub branch: String,
    pub head_sha: String,
    pub remediation: String,
    #[serde(flatten)]
    pub event: PrFeedbackEvent,
}

/// One message to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackNotice {
    /// Registered agent `name_key`.
    pub persona: String,
    /// `(pr_number, head_sha, check_or_review_id)`.
    pub dedup_key: String,
    pub subject: String,
    pub body: String,
}

const INTENT: &str = "github.pr_feedback";
const CI_REMEDIATION: &str = "Read the failing check, fix the cause, and push a new commit on this branch. A later green run of an older commit does not clear this failure.";
const REVIEW_REMEDIATION: &str = "Read the review, apply the accepted findings, push a new commit on this branch, and ask the reviewer for a scoped re-review.";

/// Remembers which `(pull request, commit, check or review)` triples were already sent.
#[derive(Debug, Default, Clone)]
pub struct PrFeedbackPoller {
    seen: HashSet<String>,
}

impl PrFeedbackPoller {
    /// Notices for facts that have not been announced yet. Does not record them.
    /// The caller records a key with [`remember`](Self::remember) after the send succeeds.
    pub fn poll(
        &self,
        prs: &[PrFeedbackSnapshot],
        active_agents: &HashSet<String>,
    ) -> Vec<FeedbackNotice> {
        let mut notices = Vec::new();
        for pr in prs {
            notices.extend(route_one(pr, active_agents, &self.seen));
        }
        notices.sort_by(|a, b| a.dedup_key.cmp(&b.dedup_key));
        notices
    }

    /// Record a notice that was delivered.
    pub fn remember(&mut self, dedup_key: &str) {
        self.seen.insert(dedup_key.to_string());
    }
}

fn route_one(
    pr: &PrFeedbackSnapshot,
    active_agents: &HashSet<String>,
    seen: &HashSet<String>,
) -> Vec<FeedbackNotice> {
    let Some(persona) = author_persona(pr, active_agents) else {
        return Vec::new();
    };
    let head = pr.head_sha.to_ascii_lowercase();
    let mut notices = Vec::new();
    for check in &pr.checks {
        if !failing_on_head(check, &head) {
            continue;
        }
        let key = dedup_key(pr.number, &head, &format!("check:{}", check.id));
        if seen.contains(&key) {
            continue;
        }
        let event = PrFeedbackEvent::CiFailure {
            check_id: check.id,
            check_name: check.name.clone(),
            conclusion: check.conclusion.to_ascii_lowercase(),
            details_url: check.details_url.clone(),
        };
        notices.push(notice(
            pr,
            &persona,
            key,
            &format!("PR #{} CI failed: {}", pr.number, check.name),
            event,
            CI_REMEDIATION,
        ));
    }
    for review in &pr.reviews {
        if !changes_on_head(review, &head) {
            continue;
        }
        let key = dedup_key(pr.number, &head, &format!("review:{}", review.id));
        if seen.contains(&key) {
            continue;
        }
        let event = PrFeedbackEvent::ChangesRequested {
            review_id: review.id,
            reviewer: review.author_login.clone(),
            excerpt: excerpt(&review.body),
        };
        notices.push(notice(
            pr,
            &persona,
            key,
            &format!(
                "PR #{} changes requested by {}",
                pr.number, review.author_login
            ),
            event,
            REVIEW_REMEDIATION,
        ));
    }
    notices
}

fn notice(
    pr: &PrFeedbackSnapshot,
    persona: &str,
    key: String,
    subject: &str,
    event: PrFeedbackEvent,
    remediation: &str,
) -> FeedbackNotice {
    let intent = PrFeedbackIntent {
        intent: INTENT,
        pr_number: pr.number,
        branch: pr.branch.clone(),
        head_sha: pr.head_sha.clone(),
        remediation: remediation.to_string(),
        event,
    };
    FeedbackNotice {
        persona: persona.to_string(),
        dedup_key: key,
        subject: clip_subject(subject),
        body: serde_json::to_string(&intent).unwrap_or_else(|_| "{}".into()),
    }
}

fn dedup_key(number: u64, head_sha: &str, id: &str) -> String {
    format!("{number}:{head_sha}:{id}")
}

fn clip_subject(subject: &str) -> String {
    let mut out = String::new();
    for c in subject.chars() {
        if c == '\n' || c == '\r' {
            continue;
        }
        if out.len() + c.len_utf8() > 200 {
            break;
        }
        out.push(c);
    }
    out
}

fn failing_on_head(check: &PrCheckFact, head: &str) -> bool {
    let check_head = check.head_sha.to_ascii_lowercase();
    if !check_head.is_empty() && check_head != head {
        return false;
    }
    matches!(
        check.conclusion.to_ascii_lowercase().as_str(),
        "failure" | "timed_out"
    )
}

fn changes_on_head(review: &PrReviewSummary, head: &str) -> bool {
    if !review.state.eq_ignore_ascii_case("CHANGES_REQUESTED") {
        return false;
    }
    review
        .commit_id
        .as_deref()
        .is_some_and(|sha| sha.eq_ignore_ascii_case(head))
}

/// The registered agent who authored the pull request.
///
/// A `Co-authored-by` trailer on the newest commit that names an active agent
/// wins. Otherwise the branch prefix before the first `/` wins when that name
/// is an active agent.
pub fn author_persona(pr: &PrFeedbackSnapshot, active_agents: &HashSet<String>) -> Option<String> {
    for commit in pr.commits.iter().rev() {
        for author in parse_co_authors(&commit.message) {
            if active_agents.contains(&author.name_key) {
                return Some(author.name_key);
            }
        }
    }
    let prefix = pr.branch.split('/').next().unwrap_or("").trim();
    if pr.branch.contains('/') && active_agents.contains(&prefix.to_ascii_lowercase()) {
        return Some(prefix.to_ascii_lowercase());
    }
    None
}

fn excerpt(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for c in flat.chars() {
        if out.len() + c.len_utf8() > 240 {
            out.push_str("...");
            break;
        }
        out.push(c);
    }
    if out.is_empty() {
        "(no review text)".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_ascii_lowercase()).collect()
    }

    fn sha(fill: char) -> String {
        fill.to_string().repeat(40)
    }

    fn commit(message: &str) -> PrCommitSummary {
        PrCommitSummary {
            sha: sha('a'),
            author_login: Some("artificially-human".into()),
            message: message.into(),
            parent_count: 1,
        }
    }

    fn check(id: u64, conclusion: &str, head: &str) -> PrCheckFact {
        PrCheckFact {
            id,
            name: "test, clippy, fmt".into(),
            conclusion: conclusion.into(),
            details_url: format!("https://example.test/checks/{id}"),
            head_sha: head.into(),
        }
    }

    fn review(id: u64, state: &str, commit: &str, body: &str) -> PrReviewSummary {
        PrReviewSummary {
            id,
            author_login: "ferris".into(),
            author_id: Some(7),
            state: state.into(),
            body: body.into(),
            commit_id: Some(commit.into()),
        }
    }

    fn pr(branch: &str, head: &str, message: &str) -> PrFeedbackSnapshot {
        PrFeedbackSnapshot {
            number: 327,
            branch: branch.into(),
            head_sha: head.into(),
            commits: vec![commit(message)],
            checks: vec![],
            reviews: vec![],
        }
    }

    #[test]
    fn a_co_author_trailer_names_the_author_ahead_of_the_branch_prefix() {
        let head = sha('a');
        let mut snap = pr(
            "ferris/other",
            &head,
            "feat: x\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>\n",
        );
        snap.checks.push(check(9, "FAILURE", &head));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia", "ferris"]));
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].persona, "felicia");
    }

    #[test]
    fn a_branch_prefix_names_the_author_when_no_trailer_matches() {
        let head = sha('b');
        let mut snap = pr("Felicia/ci-route", &head, "feat: x\n");
        snap.checks.push(check(3, "timed_out", &head));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia"]));
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].persona, "felicia");
        let body: serde_json::Value = serde_json::from_str(&notices[0].body).unwrap();
        assert_eq!(body["intent"], "github.pr_feedback");
        assert_eq!(body["kind"], "ci_failure");
        assert_eq!(body["conclusion"], "timed_out");
        assert_eq!(body["pr_number"], 327);
        assert_eq!(body["branch"], "Felicia/ci-route");
        assert!(
            body["remediation"]
                .as_str()
                .unwrap()
                .contains("push a new commit")
        );
    }

    #[test]
    fn an_unregistered_author_is_skipped() {
        let head = sha('c');
        let mut snap = pr("nobody/topic", &head, "feat: x\n");
        snap.checks.push(check(1, "failure", &head));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia"]));
        assert!(notices.is_empty());
    }

    #[test]
    fn a_repeated_poll_of_the_same_failure_is_silent_and_a_new_commit_is_not() {
        let head = sha('d');
        let next = sha('e');
        let mut first = pr(
            "felicia/topic",
            &head,
            "feat: x\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>\n",
        );
        first.checks.push(check(11, "failure", &head));
        let registered = agents(&["felicia"]);
        let mut poller = PrFeedbackPoller::default();
        let once = poller.poll(std::slice::from_ref(&first), &registered);
        assert_eq!(once.len(), 1);
        poller.remember(&once[0].dedup_key);
        assert!(poller.poll(&[first.clone()], &registered).is_empty());

        let mut again = first;
        again.head_sha = next.clone();
        again.checks[0].head_sha = next;
        let twice = poller.poll(&[again], &registered);
        assert_eq!(twice.len(), 1);
        assert_ne!(twice[0].dedup_key, once[0].dedup_key);
    }

    #[test]
    fn passing_and_pending_checks_and_an_old_review_are_skipped() {
        let head = sha('f');
        let old = sha('0');
        let mut snap = pr(
            "felicia/topic",
            &head,
            "feat: x\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>\n",
        );
        snap.checks.push(check(1, "success", &head));
        snap.checks.push(check(2, "neutral", &head));
        snap.checks.push(check(3, "failure", &old));
        snap.reviews.push(review(4, "APPROVED", &head, "ship it"));
        snap.reviews
            .push(review(5, "CHANGES_REQUESTED", &old, "stale"));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia"]));
        assert!(notices.is_empty());
    }

    #[test]
    fn a_changes_requested_review_on_the_current_head_is_routed_once() {
        let head = sha('a');
        let mut snap = pr(
            "felicia/topic",
            &head,
            "feat: x\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>\n",
        );
        snap.reviews.push(review(
            42,
            "CHANGES_REQUESTED",
            &head,
            "Please fix the empty After= reset.\n\nIt hides a real cycle.",
        ));
        let registered = agents(&["felicia"]);
        let mut poller = PrFeedbackPoller::default();
        let notices = poller.poll(&[snap.clone()], &registered);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].persona, "felicia");
        assert!(notices[0].subject.contains("ferris"));
        let body: serde_json::Value = serde_json::from_str(&notices[0].body).unwrap();
        assert_eq!(body["kind"], "changes_requested");
        assert_eq!(body["reviewer"], "ferris");
        assert_eq!(body["review_id"], 42);
        assert_eq!(body["head_sha"], head);
        assert!(
            body["excerpt"]
                .as_str()
                .unwrap()
                .contains("empty After= reset")
        );
        assert!(
            body["remediation"]
                .as_str()
                .unwrap()
                .contains("scoped re-review")
        );
        poller.remember(&notices[0].dedup_key);
        assert!(poller.poll(&[snap], &registered).is_empty());
    }
}
