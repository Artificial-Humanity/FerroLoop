//! Route a failing CI check or a `CHANGES_REQUESTED` review to the agent who
//! authored the pull request.
//!
//! This module does no I/O. A caller supplies the pull-request facts and the
//! names of active agents, and keeps the [`PrFeedbackPoller`] so the latest
//! failing check of each name, and each reviewer's open change request, is
//! announced once per commit.

use fl_core::pr_policy::{
    FlReviewMarker, PrCommitSummary, PrReviewSummary, extract_review_marker, parse_co_authors,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};

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
    /// `owner/repo` this pull request belongs to.
    pub repo: String,
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
    /// `{repo}:{number}:{head_sha}:check:{id}` or `:review:{id}`.
    pub dedup_key: String,
    pub subject: String,
    pub body: String,
}

const INTENT: &str = "github.pr_feedback";
const CI_REMEDIATION: &str = "Read the failing check, fix the cause, and push a new commit on this branch. A later green run of an older commit does not clear this failure.";
const REVIEW_REMEDIATION: &str = "Read the review, apply the accepted findings, push a new commit on this branch, and ask the reviewer for a scoped re-review.";

/// How many delivered keys are kept. A later notice pushes the oldest out.
const MAX_SEEN_KEYS: usize = 4096;

/// Remembers which `(repo, pull request, commit, check or review)` notices were sent.
/// At most 4096 keys are kept; the oldest is forgotten first.
#[derive(Debug, Default, Clone)]
pub struct PrFeedbackPoller {
    seen: HashSet<String>,
    order: VecDeque<String>,
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

    /// Record a notice that was delivered. A key already recorded is left where it is.
    pub fn remember(&mut self, dedup_key: &str) {
        let key = dedup_key.to_string();
        if !self.seen.insert(key.clone()) {
            return;
        }
        self.order.push_back(key);
        if self.order.len() > MAX_SEEN_KEYS
            && let Some(oldest) = self.order.pop_front()
        {
            self.seen.remove(&oldest);
        }
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
    notices.extend(ci_notices(pr, &persona, &head, seen));
    notices.extend(review_notices(pr, &persona, &head, seen));
    notices
}

/// The highest id of each check name on `head`. A later success suppresses an earlier failure.
fn ci_notices(
    pr: &PrFeedbackSnapshot,
    persona: &str,
    head: &str,
    seen: &HashSet<String>,
) -> Vec<FeedbackNotice> {
    let mut latest: HashMap<&str, &PrCheckFact> = HashMap::new();
    for check in &pr.checks {
        if !check_on_head(check, head) {
            continue;
        }
        latest
            .entry(check.name.as_str())
            .and_modify(|current| {
                if check.id > current.id {
                    *current = check;
                }
            })
            .or_insert(check);
    }
    let mut notices = Vec::new();
    for check in latest.into_values() {
        if !failing_conclusion(&check.conclusion) {
            continue;
        }
        let key = dedup_key(&pr.repo, pr.number, head, "check", check.id);
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
            persona,
            key,
            &format!("PR #{} CI failed: {}", pr.number, check.name),
            event,
            CI_REMEDIATION,
        ));
    }
    notices
}

struct ReviewPick<'a> {
    review: &'a PrReviewSummary,
    marker: Option<FlReviewMarker>,
}

/// The last state-changing review per reviewer. `COMMENTED` and `PENDING` do not count.
/// A newer review that is not on `head` still supersedes an older one that is.
fn review_notices(
    pr: &PrFeedbackSnapshot,
    persona: &str,
    head: &str,
    seen: &HashSet<String>,
) -> Vec<FeedbackNotice> {
    let mut ordered: Vec<&PrReviewSummary> = pr.reviews.iter().collect();
    ordered.sort_by_key(|review| review.id);
    let mut latest: HashMap<String, ReviewPick<'_>> = HashMap::new();
    for review in ordered {
        if ignored_review_state(&review.state) {
            continue;
        }
        let marker = extract_review_marker(&review.body);
        let key = match &marker {
            Some(marker) => format!("agent:{}", marker.reviewer.to_ascii_lowercase()),
            None => format!("login:{}", review.author_login.trim().to_ascii_lowercase()),
        };
        latest.insert(key, ReviewPick { review, marker });
    }
    let mut notices = Vec::new();
    for pick in latest.into_values() {
        if !pick.review.state.eq_ignore_ascii_case("CHANGES_REQUESTED") {
            continue;
        }
        if !commit_on_head(pick.review, head) {
            continue;
        }
        if let Some(marker) = &pick.marker
            && !marker.head_sha.eq_ignore_ascii_case(head)
        {
            continue;
        }
        let key = dedup_key(&pr.repo, pr.number, head, "review", pick.review.id);
        if seen.contains(&key) {
            continue;
        }
        let reviewer = pick
            .marker
            .as_ref()
            .map(|marker| marker.reviewer.as_str())
            .unwrap_or(pick.review.author_login.as_str());
        let event = PrFeedbackEvent::ChangesRequested {
            review_id: pick.review.id,
            reviewer: reviewer.to_string(),
            excerpt: excerpt(&pick.review.body),
        };
        notices.push(notice(
            pr,
            persona,
            key,
            &format!("PR #{} changes requested by {reviewer}", pr.number),
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

fn dedup_key(repo: &str, number: u64, head_sha: &str, kind: &str, id: u64) -> String {
    format!("{repo}:{number}:{head_sha}:{kind}:{id}")
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

fn check_on_head(check: &PrCheckFact, head: &str) -> bool {
    let check_head = check.head_sha.to_ascii_lowercase();
    !check_head.is_empty() && check_head == head
}

fn failing_conclusion(conclusion: &str) -> bool {
    matches!(
        conclusion.to_ascii_lowercase().as_str(),
        "failure" | "timed_out"
    )
}

fn ignored_review_state(state: &str) -> bool {
    state.eq_ignore_ascii_case("COMMENTED") || state.eq_ignore_ascii_case("PENDING")
}

fn commit_on_head(review: &PrReviewSummary, head: &str) -> bool {
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

const REVIEW_MARKER_PREFIX: &str = "<!-- fl-review:";

fn excerpt(body: &str) -> String {
    let prose = match body.rfind(REVIEW_MARKER_PREFIX) {
        Some(at) => &body[..at],
        None => body,
    };
    let flat = prose.split_whitespace().collect::<Vec<_>>().join(" ");
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
    use fl_core::agent::AgentRole;
    use fl_core::at::At;
    use fl_core::pr_policy::{FlReviewMarker, ReviewVerdict, format_review_body};

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
            repo: "acme/widgets".into(),
            number: 327,
            branch: branch.into(),
            head_sha: head.into(),
            commits: vec![commit(message)],
            checks: vec![],
            reviews: vec![],
        }
    }

    fn review_by(id: u64, author: &str, state: &str, commit: &str, body: &str) -> PrReviewSummary {
        PrReviewSummary {
            id,
            author_login: author.into(),
            author_id: Some(7),
            state: state.into(),
            body: body.into(),
            commit_id: Some(commit.into()),
        }
    }

    fn marked(prose: &str, reviewer: &str, verdict: ReviewVerdict, head: &str) -> String {
        format_review_body(
            prose,
            &FlReviewMarker {
                reviewer: reviewer.into(),
                role: AgentRole::Lead,
                project: "ferroloop".into(),
                head_sha: head.into(),
                verdict,
                at: At::from_unix_millis(1),
            },
        )
    }

    fn with_author(branch: &str, head: &str) -> PrFeedbackSnapshot {
        pr(
            branch,
            head,
            "feat: x\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>\n",
        )
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

    #[test]
    fn a_newer_approval_from_the_same_login_supersedes_changes_requested() {
        let head = sha('a');
        let mut snap = with_author("felicia/topic", &head);
        snap.reviews.push(review(2, "APPROVED", &head, "ship it"));
        snap.reviews
            .push(review(1, "CHANGES_REQUESTED", &head, "fix it"));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia"]));
        assert!(notices.is_empty());
    }

    #[test]
    fn a_newer_bot_approval_supersedes_changes_from_the_same_marker_reviewer() {
        let head = sha('a');
        let mut snap = with_author("felicia/topic", &head);
        snap.reviews.push(review_by(
            11,
            "ferroloop[bot]",
            "APPROVED",
            &head,
            &marked("looks good", "ferris", ReviewVerdict::Approved, &head),
        ));
        snap.reviews.push(review_by(
            10,
            "ferroloop[bot]",
            "CHANGES_REQUESTED",
            &head,
            &marked(
                "fix the router",
                "ferris",
                ReviewVerdict::ChangesRequested,
                &head,
            ),
        ));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia"]));
        assert!(notices.is_empty());
    }

    #[test]
    fn a_bot_review_reports_the_marker_reviewer_and_strips_the_marker() {
        let head = sha('b');
        let mut snap = pr("felicia/topic", &head, "feat: x\n");
        // The marker verdict is not required to match the GitHub review state.
        snap.reviews.push(review_by(
            7,
            "ferroloop[bot]",
            "CHANGES_REQUESTED",
            &head,
            &marked(
                "Please fix the cycle.",
                "ferris",
                ReviewVerdict::Approved,
                &head.to_ascii_uppercase(),
            ),
        ));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia"]));
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].persona, "felicia");
        assert!(notices[0].subject.contains("ferris"));
        assert!(!notices[0].subject.contains("ferroloop"));
        let body: serde_json::Value = serde_json::from_str(&notices[0].body).unwrap();
        assert_eq!(body["kind"], "changes_requested");
        assert_eq!(body["reviewer"], "ferris");
        assert_eq!(body["review_id"], 7);
        assert_eq!(body["excerpt"], "Please fix the cycle.");
        assert!(!notices[0].body.contains("fl-review"));
        assert_eq!(
            excerpt(&marked(
                "",
                "ferris",
                ReviewVerdict::ChangesRequested,
                &head
            )),
            "(no review text)"
        );
    }

    #[test]
    fn a_comment_after_changes_requested_does_not_clear_the_notice() {
        let head = sha('a');
        let mut snap = with_author("felicia/topic", &head);
        snap.reviews
            .push(review(1, "CHANGES_REQUESTED", &head, "fix it"));
        snap.reviews
            .push(review(2, "COMMENTED", &head, "one more note"));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia"]));
        assert_eq!(notices.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&notices[0].body).unwrap();
        assert_eq!(body["review_id"], 1);
        assert_eq!(body["reviewer"], "ferris");
    }

    #[test]
    fn a_marker_for_another_commit_produces_no_notice() {
        let head = sha('a');
        let other = sha('b');
        let mut snap = with_author("felicia/topic", &head);
        snap.reviews.push(review_by(
            3,
            "ferroloop[bot]",
            "CHANGES_REQUESTED",
            &head,
            &marked("fix it", "ferris", ReviewVerdict::ChangesRequested, &other),
        ));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia"]));
        assert!(notices.is_empty());
    }

    #[test]
    fn a_newer_review_off_the_head_supersedes_an_older_change_request() {
        let head = sha('a');
        let old = sha('0');
        let mut snap = with_author("felicia/topic", &head);
        snap.reviews
            .push(review(1, "CHANGES_REQUESTED", &head, "fix it"));
        snap.reviews
            .push(review(2, "APPROVED", &old, "approved an older commit"));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia"]));
        assert!(notices.is_empty());
    }

    #[test]
    fn the_same_pull_request_in_two_repos_notifies_twice() {
        let head = sha('a');
        let mut left = with_author("felicia/topic", &head);
        left.repo = "acme/widgets".into();
        left.checks.push(check(1, "failure", &head));
        let mut right = left.clone();
        right.repo = "acme/other".into();
        let notices = PrFeedbackPoller::default().poll(&[left, right], &agents(&["felicia"]));
        assert_eq!(notices.len(), 2);
        assert_ne!(notices[0].dedup_key, notices[1].dedup_key);
        assert!(
            notices
                .iter()
                .any(|n| n.dedup_key == format!("acme/widgets:327:{head}:check:1"))
        );
        assert!(
            notices
                .iter()
                .any(|n| n.dedup_key == format!("acme/other:327:{head}:check:1"))
        );
    }

    #[test]
    fn remember_forgets_the_oldest_key_past_the_cap() {
        let head = sha('a');
        let mut oldest = with_author("felicia/topic", &head);
        oldest.repo = "acme/old".into();
        oldest.number = 1;
        oldest.checks.push(check(1, "failure", &head));
        let mut newest = oldest.clone();
        newest.repo = "acme/new".into();
        newest.number = 2;
        newest.checks[0].id = 2;

        let registered = agents(&["felicia"]);
        let mut poller = PrFeedbackPoller::default();
        let found = poller.poll(&[oldest.clone(), newest.clone()], &registered);
        assert_eq!(found.len(), 2);
        let oldest_key = found
            .iter()
            .find(|n| n.dedup_key.starts_with("acme/old:"))
            .unwrap()
            .dedup_key
            .clone();
        let newest_key = found
            .iter()
            .find(|n| n.dedup_key.starts_with("acme/new:"))
            .unwrap()
            .dedup_key
            .clone();

        poller.remember(&oldest_key);
        for n in 1..MAX_SEEN_KEYS {
            poller.remember(&format!("pad:{n}"));
        }
        poller.remember(&oldest_key);
        assert!(
            poller
                .poll(std::slice::from_ref(&oldest), &registered)
                .is_empty()
        );

        poller.remember(&newest_key);
        let again = poller.poll(&[oldest, newest], &registered);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].dedup_key, oldest_key);
        assert_ne!(again[0].dedup_key, newest_key);
    }

    #[test]
    fn an_empty_check_head_is_not_on_the_pull_request_head() {
        let head = sha('a');
        let mut snap = with_author("felicia/topic", &head);
        snap.checks.push(check(1, "failure", ""));
        let notices = PrFeedbackPoller::default().poll(&[snap], &agents(&["felicia"]));
        assert!(notices.is_empty());
    }

    #[test]
    fn the_latest_check_of_a_name_suppresses_an_earlier_failure() {
        let head = sha('a');
        let registered = agents(&["felicia"]);

        let mut suppressed = with_author("felicia/topic", &head);
        suppressed.checks.push(check(2, "success", &head));
        suppressed.checks.push(check(1, "failure", &head));
        assert!(
            PrFeedbackPoller::default()
                .poll(&[suppressed], &registered)
                .is_empty()
        );

        let mut failed = with_author("felicia/topic", &head);
        failed.checks.push(check(2, "failure", &head));
        failed.checks.push(check(1, "success", &head));
        let notices = PrFeedbackPoller::default().poll(&[failed], &registered);
        assert_eq!(notices.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&notices[0].body).unwrap();
        assert_eq!(body["check_id"], 2);
        assert_eq!(
            notices[0].dedup_key,
            format!("acme/widgets:327:{head}:check:2")
        );

        let mut both = with_author("felicia/topic", &head);
        both.checks.push(check(1, "failure", &head));
        let mut lint = check(4, "timed_out", &head);
        lint.name = "lint".into();
        both.checks.push(lint);
        let notices = PrFeedbackPoller::default().poll(&[both], &registered);
        assert_eq!(notices.len(), 2);
        assert!(notices.iter().any(|n| n.dedup_key.ends_with(":check:1")));
        assert!(notices.iter().any(|n| n.dedup_key.ends_with(":check:4")));
    }
}
