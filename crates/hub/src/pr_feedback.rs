//! Poll open pull requests and send a typed FerroWire message to the author
//! when CI fails or a reviewer requests changes.
//!
//! The routing rules live in `fl_github::feedback`. This module reads GitHub,
//! sends the message, and remembers a delivery only after the send succeeds.

use std::collections::HashSet;
use std::time::Duration;

use fl_core::agent::AgentStatus;
use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
use fl_core::pr_policy::{PrCommitSummary, PrReviewSummary};
use fl_core::store::{AgentRegistry, MessageStore, StoreError};
use fl_exec::stamp;
use fl_github::{
    Client, FeedbackNotice, PrCheckFact, PrFeedbackPoller, PrFeedbackSnapshot, list_check_runs,
    list_open_prs, list_pr_commits, list_pr_reviews,
};
use fl_store::AnyStore;

use crate::HubState;
use crate::github_pr::{ClientError, HubGithub};

/// How often a running hub looks for new failures. The first look waits one
/// interval, so a short-lived test server does not call GitHub.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Read the configured repositories and deliver any new notices.
///
/// Every repository is read before the store is opened. The poller lock is
/// taken only for delivery, after the store is open, and is skipped when
/// there is nothing to send.
pub fn poll_configured_prs(state: &HubState) -> Result<usize, String> {
    let github = &state.config.github;
    if !github_ready(github) {
        return Ok(0);
    }
    let mut snapshots = Vec::new();
    let mut unusable: Option<String> = None;
    for (project, repo) in &github.repos {
        let client = match github.reader(repo) {
            Ok(client) => client,
            Err(ClientError::Missing(_)) => continue,
            Err(ClientError::Unusable) => {
                if unusable.is_none() {
                    unusable = Some(format!(
                        "the GitHub credential for project `{project}` is unusable"
                    ));
                }
                continue;
            }
        };
        match load_snapshots(github, &client, repo) {
            Ok(loaded) => snapshots.extend(loaded),
            Err(e) => eprintln!("hub: pull-request feedback for `{repo}`: {e}"),
        }
    }
    let sent = if snapshots.is_empty() {
        0
    } else {
        let store = state.open_store().map_err(|e| e.to_string())?;
        let mut poller = state.pr_feedback.lock().unwrap_or_else(|e| e.into_inner());
        let sent =
            deliver_pr_feedback(&store, &mut poller, &snapshots).map_err(|e| e.to_string())?;
        drop(poller);
        sent
    };
    if let Some(err) = unusable {
        return Err(err);
    }
    Ok(sent)
}

fn github_ready(github: &HubGithub) -> bool {
    if github.repos.is_empty() {
        return false;
    }
    let token = github
        .token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty());
    token || github.app_id.is_some()
}

/// Snapshots for the open pull requests. A failed commits, checks, or reviews
/// read becomes an empty list, so the other facts are still delivered. A check
/// read whose error says `answered 401` or `answered 403` is tried once with
/// the Reviewer App when the Hub has one. A check that was not read is not
/// remembered, because it never becomes a notice.
fn load_snapshots(
    github: &HubGithub,
    client: &Client,
    repo: &str,
) -> Result<Vec<PrFeedbackSnapshot>, StoreError> {
    let mut snapshots = Vec::new();
    for pr in list_open_prs(client, repo)? {
        let number = pr.number;
        let head_sha = pr.head_sha.clone();
        snapshots.push(snapshot_from_parts(
            repo,
            number,
            pr.head_ref,
            head_sha.clone(),
            list_pr_commits(client, repo, number),
            load_checks(github, client, repo, number, &head_sha),
            list_pr_reviews(client, repo, number),
        ));
    }
    Ok(snapshots)
}

fn snapshot_from_parts(
    repo: &str,
    number: u64,
    branch: String,
    head_sha: String,
    commits: Result<Vec<PrCommitSummary>, StoreError>,
    checks: Result<Vec<PrCheckFact>, StoreError>,
    reviews: Result<Vec<PrReviewSummary>, StoreError>,
) -> PrFeedbackSnapshot {
    PrFeedbackSnapshot {
        repo: repo.to_string(),
        number,
        branch,
        head_sha,
        commits: empty_on_err(number, "commits", commits),
        checks: empty_on_err(number, "checks", checks),
        reviews: empty_on_err(number, "reviews", reviews),
    }
}

fn empty_on_err<T>(number: u64, part: &str, result: Result<Vec<T>, StoreError>) -> Vec<T> {
    match result {
        Ok(items) => items,
        Err(e) => {
            eprintln!("hub: pull request #{number} {part}: {e}");
            Vec::new()
        }
    }
}

/// The token often cannot read check runs. A 401 or 403 is retried once as the
/// Reviewer App; any other error, a missing App, or a failed retry stays an
/// error so the caller records no check it did not see.
fn load_checks(
    github: &HubGithub,
    client: &Client,
    repo: &str,
    number: u64,
    head_sha: &str,
) -> Result<Vec<PrCheckFact>, StoreError> {
    let primary = list_check_runs(client, repo, head_sha);
    let denied = primary.as_ref().is_err_and(check_listing_denied);
    if !denied {
        return primary;
    }
    let retry = match github.reviewer_app_client(repo) {
        Ok(Some(app)) => Some(move || list_check_runs(&app, repo, head_sha)),
        Ok(None) | Err(_) => None,
    };
    checks_after_reader(number, primary, retry)
}

fn checks_after_reader(
    number: u64,
    primary: Result<Vec<PrCheckFact>, StoreError>,
    retry: Option<impl FnOnce() -> Result<Vec<PrCheckFact>, StoreError>>,
) -> Result<Vec<PrCheckFact>, StoreError> {
    match primary {
        Ok(checks) => Ok(checks),
        Err(e) if check_listing_denied(&e) => match retry {
            Some(retry) => {
                eprintln!(
                    "hub: pull request #{number} checks were refused; retrying with the Reviewer App: {e}"
                );
                retry()
            }
            None => Err(e),
        },
        Err(e) => Err(e),
    }
}

fn check_listing_denied(err: &StoreError) -> bool {
    let text = err.to_string();
    text.contains("answered 401") || text.contains("answered 403")
}

/// Send each new notice and remember it only after `send_wire` succeeds.
pub fn deliver_pr_feedback(
    store: &AnyStore,
    poller: &mut PrFeedbackPoller,
    prs: &[PrFeedbackSnapshot],
) -> Result<usize, StoreError> {
    let agents = store.list_agents(None, false)?;
    let active = agents
        .iter()
        .filter(|agent| agent.status == AgentStatus::Active)
        .map(|agent| agent.name_key.clone())
        .collect::<HashSet<_>>();
    let notices = poller.poll(prs, &active);
    let mut sent = 0;
    for notice in notices {
        if send_notice(store, &notice).is_err() {
            eprintln!(
                "hub: pull-request feedback to `{}` was not sent ({})",
                notice.persona, notice.dedup_key
            );
            continue;
        }
        poller.remember(&notice.dedup_key);
        sent += 1;
    }
    Ok(sent)
}

fn send_notice(store: &AnyStore, notice: &FeedbackNotice) -> Result<(), StoreError> {
    let args = SendWireArgs {
        id: None,
        sender_id: None,
        sender_name: "system".into(),
        sender_host: None,
        to: WireAddress::AgentName(notice.persona.clone()),
        kind: MessageKind::Mail,
        urgency: MessageUrgency::Normal,
        durable: true,
        subject: notice.subject.clone(),
        body: notice.body.clone(),
        reply_to: None,
        thread_id: None,
        deadline_at: None,
        at: stamp::now(),
    };
    store.send_wire(args)?;
    Ok(())
}

/// One poll from the accept loop. A failure is logged and does not stop the hub.
pub(crate) fn run_poll(state: &HubState) {
    if state
        .pr_feedback_running
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    let _guard = PollGuard(&state.pr_feedback_running);
    if let Err(e) = poll_configured_prs(state) {
        eprintln!("hub: pull-request feedback poll failed: {e}");
    }
}

struct PollGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for PollGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::agent::{AgentRecord, AgentRole, RegisterAgentArgs};
    use fl_core::ferrowire::WireFilter;
    use fl_core::pr_policy::{PrCommitSummary, PrReviewSummary};
    use tempfile::TempDir;

    use crate::{HubServerConfig, HubState};

    fn sha(fill: char) -> String {
        fill.to_string().repeat(40)
    }

    fn state(tmp: &TempDir) -> (HubState, AgentRecord) {
        let state = HubState::new(HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: tmp.path().to_path_buf(),
            ..HubServerConfig::default()
        });
        let store = state.open_store().unwrap();
        let felicia = store
            .register_agent(RegisterAgentArgs {
                display_name: "Felicia".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Assistant,
                extra_areas: vec![],
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap();
        drop(store);
        (state, felicia)
    }

    fn register(store: &AnyStore, display_name: &str) -> AgentRecord {
        store
            .register_agent(RegisterAgentArgs {
                display_name: display_name.into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Assistant,
                extra_areas: vec![],
                create_area: true,
                replace: true,
                at: stamp::now(),
                by: "owner".into(),
            })
            .unwrap()
    }

    fn failing(head: &str) -> PrFeedbackSnapshot {
        PrFeedbackSnapshot {
            repo: "acme/widgets".into(),
            number: 88,
            branch: "felicia/ci-and-pr-feedback-routing-88".into(),
            head_sha: head.into(),
            commits: vec![PrCommitSummary {
                sha: head.into(),
                author_login: Some("artificially-human".into()),
                message: "feat: x\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>\n"
                    .into(),
                parent_count: 1,
            }],
            checks: vec![PrCheckFact {
                id: 4,
                name: "test, clippy, fmt".into(),
                conclusion: "failure".into(),
                details_url: "https://example.test/checks/4".into(),
                head_sha: head.into(),
            }],
            reviews: vec![PrReviewSummary {
                id: 9,
                author_login: "ferris".into(),
                author_id: Some(1),
                state: "CHANGES_REQUESTED".into(),
                body: "Fix the router.".into(),
                commit_id: Some(head.into()),
            }],
        }
    }

    #[test]
    fn a_failure_and_a_review_reach_the_author_once() {
        let tmp = TempDir::new().unwrap();
        let (state, _felicia) = state(&tmp);
        let head = sha('a');
        let snap = failing(&head);
        let store = state.open_store().unwrap();
        let mut poller = PrFeedbackPoller::default();
        assert_eq!(
            deliver_pr_feedback(&store, &mut poller, std::slice::from_ref(&snap)).unwrap(),
            2
        );
        assert_eq!(
            deliver_pr_feedback(&store, &mut poller, &[snap]).unwrap(),
            0
        );
        let inbox = store
            .list_wire_inbox(&WireFilter {
                recipient: Some("felicia".into()),
                ..WireFilter::default()
            })
            .unwrap();
        assert_eq!(inbox.len(), 2);
        let bodies = inbox
            .iter()
            .map(|item| item.message.body.as_str())
            .collect::<Vec<_>>();
        assert!(bodies.iter().any(|b| b.contains("\"kind\":\"ci_failure\"")));
        assert!(
            bodies
                .iter()
                .any(|b| b.contains("\"kind\":\"changes_requested\""))
        );
        assert!(
            bodies
                .iter()
                .all(|b| b.contains("\"intent\":\"github.pr_feedback\""))
        );
        assert!(bodies.iter().all(|b| b.contains(&head)));
    }

    #[test]
    fn an_unconfigured_hub_polls_nothing() {
        let tmp = TempDir::new().unwrap();
        let (state, _felicia) = state(&tmp);
        assert_eq!(poll_configured_prs(&state).unwrap(), 0);
    }

    #[test]
    fn a_retired_co_author_does_not_take_the_notice_from_the_branch_author() {
        let tmp = TempDir::new().unwrap();
        let (state, felicia) = state(&tmp);
        let store = state.open_store().unwrap();
        register(&store, "Ferris");
        store
            .retire_agent(&felicia.id, stamp::now(), "owner")
            .unwrap();
        let head = sha('b');
        let snap = PrFeedbackSnapshot {
            repo: "acme/widgets".into(),
            number: 88,
            branch: "ferris/topic".into(),
            head_sha: head.clone(),
            commits: vec![PrCommitSummary {
                sha: head.clone(),
                author_login: Some("artificially-human".into()),
                message: "feat: x\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>\n"
                    .into(),
                parent_count: 1,
            }],
            checks: vec![PrCheckFact {
                id: 4,
                name: "test, clippy, fmt".into(),
                conclusion: "failure".into(),
                details_url: "https://example.test/checks/4".into(),
                head_sha: head,
            }],
            reviews: vec![],
        };
        let mut poller = PrFeedbackPoller::default();
        assert_eq!(
            deliver_pr_feedback(&store, &mut poller, std::slice::from_ref(&snap)).unwrap(),
            1
        );
        let ferris = store
            .list_wire_inbox(&WireFilter {
                recipient: Some("ferris".into()),
                ..WireFilter::default()
            })
            .unwrap();
        assert_eq!(ferris.len(), 1);
        assert!(ferris[0].message.body.contains("\"kind\":\"ci_failure\""));
        let retired = store
            .list_wire_inbox(&WireFilter {
                recipient: Some("felicia".into()),
                ..WireFilter::default()
            })
            .unwrap();
        assert!(retired.is_empty());
    }

    #[test]
    fn a_failed_check_listing_keeps_the_review() {
        let head = sha('c');
        let denied = StoreError::Backend(format!(
            "GitHub answered 403 when listing check runs for `{head}` on `acme/widgets`"
        ));
        assert!(check_listing_denied(&denied));
        assert!(check_listing_denied(&StoreError::Backend(
            "GitHub answered 401 when listing check runs".into(),
        )));
        assert!(!check_listing_denied(&StoreError::Backend(
            "GitHub answered 500 when listing check runs".into(),
        )));

        let review = PrReviewSummary {
            id: 9,
            author_login: "reviewer".into(),
            author_id: Some(1),
            state: "CHANGES_REQUESTED".into(),
            body: "Fix the router.".into(),
            commit_id: Some(head.clone()),
        };
        let snap = snapshot_from_parts(
            "acme/widgets",
            88,
            "ferris/topic".into(),
            head.clone(),
            Err(StoreError::Backend(
                "pull request commits could not be listed".into(),
            )),
            Err(denied),
            Ok(vec![review.clone()]),
        );
        assert!(snap.commits.is_empty());
        assert!(snap.checks.is_empty());
        assert_eq!(snap.reviews, vec![review]);
        assert_eq!(snap.head_sha, head);
        assert_eq!(snap.branch, "ferris/topic");

        let active = HashSet::from(["ferris".to_string()]);
        let notices = PrFeedbackPoller::default().poll(std::slice::from_ref(&snap), &active);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].persona, "ferris");
        assert!(notices[0].body.contains("\"kind\":\"changes_requested\""));
        assert!(!notices[0].dedup_key.contains("check:"));
    }

    #[test]
    fn a_403_check_listing_is_retried_once_and_any_other_error_is_not() {
        let head = sha('d');
        let recovered = PrCheckFact {
            id: 4,
            name: "test, clippy, fmt".into(),
            conclusion: "failure".into(),
            details_url: "https://example.test/checks/4".into(),
            head_sha: head,
        };
        let calls = std::cell::Cell::new(0u32);
        let got = checks_after_reader(
            88,
            Err(StoreError::Backend(
                "GitHub answered 403 when listing check runs".into(),
            )),
            Some(|| {
                calls.set(calls.get() + 1);
                Ok(vec![recovered.clone()])
            }),
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(got, vec![recovered]);

        let calls = std::cell::Cell::new(0u32);
        let other = checks_after_reader(
            88,
            Err(StoreError::Backend(
                "GitHub answered 500 when listing check runs".into(),
            )),
            Some(|| {
                calls.set(calls.get() + 1);
                Ok(vec![])
            }),
        );
        assert_eq!(calls.get(), 0);
        assert!(other.is_err());
        assert!(!check_listing_denied(other.as_ref().unwrap_err()));

        let failed = checks_after_reader(
            88,
            Err(StoreError::Backend(
                "GitHub answered 401 when listing check runs".into(),
            )),
            Some(|| {
                Err(StoreError::Backend(
                    "GitHub answered 403 when listing check runs via the Reviewer App".into(),
                ))
            }),
        );
        assert!(failed.is_err());
        let snap = snapshot_from_parts(
            "acme/widgets",
            88,
            "ferris/topic".into(),
            sha('e'),
            Ok(vec![]),
            failed,
            Ok(vec![]),
        );
        assert!(snap.checks.is_empty());
    }
}
