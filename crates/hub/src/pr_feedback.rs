//! Poll open pull requests and send a typed FerroWire message to the author
//! when CI fails or a reviewer requests changes.
//!
//! The routing rules live in `fl_github::feedback`. This module reads GitHub,
//! sends the message, and remembers a delivery only after the send succeeds.

use std::collections::HashSet;
use std::time::Duration;

use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
use fl_core::store::{AgentRegistry, MessageStore, StoreError};
use fl_exec::stamp;
use fl_github::{
    Client, FeedbackNotice, PrFeedbackPoller, PrFeedbackSnapshot, list_check_runs, list_open_prs,
    list_pr_commits, list_pr_reviews,
};
use fl_store::AnyStore;

use crate::HubState;
use crate::github_pr::{ClientError, HubGithub};

/// How often a running hub looks for new failures. The first look waits one
/// interval, so a short-lived test server does not call GitHub.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Read the configured repositories and deliver any new notices.
pub fn poll_configured_prs(state: &HubState) -> Result<usize, String> {
    let github = &state.config.github;
    if !github_ready(github) {
        return Ok(0);
    }
    let store = state.open_store().map_err(|e| e.to_string())?;
    let mut poller = state.pr_feedback.lock().unwrap_or_else(|e| e.into_inner());
    let mut sent = 0;
    for (project, repo) in &github.repos {
        let client = match github.reader(repo) {
            Ok(client) => client,
            Err(ClientError::Missing(_)) => continue,
            Err(ClientError::Unusable) => {
                return Err(format!(
                    "the GitHub credential for project `{project}` is unusable"
                ));
            }
        };
        let snapshots = match load_snapshots(&client, repo) {
            Ok(snapshots) => snapshots,
            Err(e) => {
                eprintln!("hub: pull-request feedback for `{repo}`: {e}");
                continue;
            }
        };
        sent += deliver_pr_feedback(&store, &mut poller, &snapshots).map_err(|e| e.to_string())?;
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

fn load_snapshots(client: &Client, repo: &str) -> Result<Vec<PrFeedbackSnapshot>, StoreError> {
    let mut snapshots = Vec::new();
    for pr in list_open_prs(client, repo)? {
        let commits = match list_pr_commits(client, repo, pr.number) {
            Ok(commits) => commits,
            Err(e) => {
                eprintln!("hub: pull request #{} commits: {e}", pr.number);
                continue;
            }
        };
        let checks = match list_check_runs(client, repo, &pr.head_sha) {
            Ok(checks) => checks,
            Err(e) => {
                eprintln!("hub: pull request #{} checks: {e}", pr.number);
                continue;
            }
        };
        let reviews = match list_pr_reviews(client, repo, pr.number) {
            Ok(reviews) => reviews,
            Err(e) => {
                eprintln!("hub: pull request #{} reviews: {e}", pr.number);
                continue;
            }
        };
        snapshots.push(PrFeedbackSnapshot {
            number: pr.number,
            branch: pr.head_ref,
            head_sha: pr.head_sha,
            commits,
            checks,
            reviews,
        });
    }
    Ok(snapshots)
}

/// Send each new notice and remember it only after `send_wire` succeeds.
pub fn deliver_pr_feedback(
    store: &AnyStore,
    poller: &mut PrFeedbackPoller,
    prs: &[PrFeedbackSnapshot],
) -> Result<usize, StoreError> {
    let agents = store.list_agents(None, true)?;
    let active = agents
        .iter()
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
    use fl_core::agent::{AgentRole, RegisterAgentArgs};
    use fl_core::ferrowire::WireFilter;
    use fl_core::pr_policy::{PrCommitSummary, PrReviewSummary};
    use fl_github::PrCheckFact;
    use tempfile::TempDir;

    use crate::{HubServerConfig, HubState};

    fn sha(fill: char) -> String {
        fill.to_string().repeat(40)
    }

    fn state(tmp: &TempDir) -> HubState {
        let state = HubState::new(HubServerConfig {
            store_path: tmp.path().join("store.redb"),
            repo_root: tmp.path().to_path_buf(),
            ..HubServerConfig::default()
        });
        let store = state.open_store().unwrap();
        store
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
        state
    }

    fn failing(head: &str) -> PrFeedbackSnapshot {
        PrFeedbackSnapshot {
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
        let state = state(&tmp);
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
        let state = state(&tmp);
        assert_eq!(poll_configured_prs(&state).unwrap(), 0);
    }
}
