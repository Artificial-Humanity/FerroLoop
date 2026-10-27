//! `fl github pr` subcommands, Reviewer App brokering, and FerroWire routing (GitHub hierarchy spec §3).

use crate::config;
use crate::ctx::Ctx;
use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Subcommand};
use fl_core::agent::AgentStatus;
use fl_core::ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress};
use fl_core::ids::AgentId;
use fl_core::pr_policy::{
    DEFAULT_OWNER_LOGIN, FlReviewMarker, PolicyRefusal, RequiredTier, ReviewVerdict,
    authorize_agent_action, authorize_agent_merge, evaluate_pr_policy, extract_review_marker,
    format_review_body, verify_merge_approvals,
};
use fl_core::store::{AgentRegistry, MessageStore};
use fl_github::{
    AppCredentials, Client, GithubTracker, MergeMethod, create_pr, get_pr, list_pr_commits,
    list_pr_files, list_pr_reviews, merge_pr, request_pr_reviewers, submit_pr_review,
};
use fl_store::RedbStore;
use serde_json::json;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum PrCmd {
    /// Open a pull request, evaluate its required reviewer tier, and route review requests.
    Create {
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: String,
        #[arg(long)]
        head: String,
        #[arg(long, default_value = "main")]
        base: String,
        #[arg(long)]
        by: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Evaluate pull-request governance policy and merge-approval status.
    Policy {
        number: u64,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Submit a pull-request review via the Reviewer GitHub App with an embedded `fl-review` marker.
    #[command(group(
        ArgGroup::new("verdict")
            .required(true)
            .args(["approve", "request_changes", "comment"])
    ))]
    Review {
        number: u64,
        #[arg(long)]
        approve: bool,
        #[arg(long = "request-changes")]
        request_changes: bool,
        #[arg(long)]
        comment: bool,
        #[arg(long)]
        body: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Verify policy and merge approvals, then merge a pull request via the Reviewer GitHub App.
    Merge {
        number: u64,
        #[arg(long, default_value = "merge")]
        method: MergeMethod,
        #[arg(long)]
        by: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Verify commit attribution, review marker integrity, and optional merge approvals in CI.
    VerifyCi {
        number: u64,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        require_approval: bool,
        #[arg(long)]
        json: bool,
    },
}

fn resolve_api_url() -> Result<String> {
    match std::env::var("FL_GITHUB_API_URL") {
        Err(_) => Ok(fl_github::DEFAULT_API.to_string()),
        Ok(url) => {
            let _ = crate::api_override_host(&url)?;
            Ok(url)
        }
    }
}

/// Construct a dedicated Reviewer App `Client` (`<slug>[bot]`), or return `Ok(None)` when the
/// tracker binding already uses `Credential::App` (so `gh.client()` can be reused directly).
fn reviewer_client(
    gh: &GithubTracker,
    binding: Option<&config::TrackerBinding>,
    github_app: Option<&config::GithubApp>,
) -> Result<Option<Client>> {
    if binding.is_some_and(|b| b.credential == config::Credential::App) {
        return Ok(None);
    }

    let env_app_id = std::env::var("FL_GITHUB_APP_ID")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let env_key_path = std::env::var("FL_GITHUB_PRIVATE_KEY_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let (app_id, private_key) = match (env_app_id, env_key_path) {
        (Some(raw_id), Some(raw_path)) => {
            let id: u64 = raw_id
                .trim()
                .parse()
                .with_context(|| format!("invalid $FL_GITHUB_APP_ID `{raw_id}`"))?;
            (id, PathBuf::from(raw_path.trim()))
        }
        _ => match github_app {
            Some(app) => (app.app_id, app.private_key.clone()),
            None => bail!(
                "agent pull-request review and merge require a GitHub App (`[github]` with \
                 `app_id` and `private_key` in config.toml, or `$FL_GITHUB_APP_ID` + \
                 `$FL_GITHUB_PRIVATE_KEY_PATH`)"
            ),
        },
    };

    let api = resolve_api_url()?;
    let creds = AppCredentials::from_file(&api, app_id, &private_key, &gh.repo().full_name)?;
    Ok(Some(Client::new(&api, Box::new(creds))))
}

fn resolve_project_slug(
    explicit: Option<&str>,
    default_project: Option<&str>,
    repo_full_name: &str,
) -> String {
    if let Some(p) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return p.to_ascii_lowercase();
    }
    if let Some(p) = default_project.map(str::trim).filter(|s| !s.is_empty()) {
        return p.to_ascii_lowercase();
    }
    repo_full_name
        .rsplit('/')
        .next()
        .unwrap_or(repo_full_name)
        .to_ascii_lowercase()
}

fn emit_refusal(refusal: &PolicyRefusal, json_out: bool) -> Result<i32> {
    if json_out {
        println!("{}", serde_json::to_string_pretty(refusal)?);
    } else {
        eprintln!("error: {refusal}");
    }
    Ok(2)
}

fn send_wire_notification(
    ws_store: &RedbStore,
    sender_id: Option<AgentId>,
    sender_name: &str,
    recipient_name: &str,
    subject: String,
    body: String,
) -> Result<()> {
    let norm_sender = fl_core::ferrowire::validate_sender_name(sender_name)
        .unwrap_or_else(|_| "system".to_string());
    let args = SendWireArgs {
        id: None,
        sender_id,
        sender_name: norm_sender,
        sender_host: None,
        to: WireAddress::AgentName(recipient_name.to_ascii_lowercase()),
        kind: MessageKind::Mail,
        urgency: MessageUrgency::Normal,
        durable: true,
        subject,
        body,
        reply_to: None,
        thread_id: None,
        deadline_at: None,
        at: crate::cmd::agent::now_at(),
    };
    let (_msg, _receipts) = ws_store.send_wire(args)?;
    Ok(())
}

pub fn run(
    _ctx: &Ctx<'_>,
    gh: &GithubTracker,
    ws_store: &RedbStore,
    binding: Option<&config::TrackerBinding>,
    github_app: Option<&config::GithubApp>,
    default_project: Option<&str>,
    cmd: PrCmd,
) -> Result<i32> {
    let repo = &gh.repo().full_name;
    match cmd {
        PrCmd::Create {
            title,
            body,
            head,
            base,
            by,
            project,
            json,
        } => {
            let project_slug = resolve_project_slug(project.as_deref(), default_project, repo);
            let mut pr = create_pr(gh.client(), repo, &title, &body, &head, &base)?;
            let commits = list_pr_commits(gh.client(), repo, pr.number)?;
            let files = list_pr_files(gh.client(), repo, pr.number)?;
            let agents = ws_store.list_agents(None, true)?;
            let eval = evaluate_pr_policy(
                &project_slug,
                &commits,
                &files,
                &[],
                &agents,
                DEFAULT_OWNER_LOGIN,
            );

            if eval.required_tier == RequiredTier::Owner {
                let reviewers = vec![eval.target_reviewer.clone()];
                request_pr_reviewers(gh.client(), repo, pr.number, &reviewers)?;
                if !pr.requested_reviewers.contains(&eval.target_reviewer) {
                    pr.requested_reviewers.push(eval.target_reviewer.clone());
                }
            } else {
                let sender_name = by
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_ascii_lowercase)
                    .or_else(|| eval.co_authors.first().cloned())
                    .unwrap_or_else(|| "system".to_string());
                let sender_id = ws_store.agent_by_name(&sender_name)?.map(|a| a.id);
                let subject = format!(
                    "PR #{} ready for {} review: {}",
                    pr.number, eval.required_tier, pr.title
                );
                let wire_body = format!(
                    "Pull request #{} ({}) in `{}` is ready for `{}` review.\n\nTitle: {}\nBranch: {} -> {}\nHead SHA: {}\nCo-authors: {}",
                    pr.number,
                    pr.html_url,
                    repo,
                    eval.required_tier,
                    pr.title,
                    pr.head_ref,
                    pr.base_ref,
                    pr.head_sha,
                    if eval.co_authors.is_empty() {
                        "none".to_string()
                    } else {
                        eval.co_authors.join(", ")
                    }
                );
                send_wire_notification(
                    ws_store,
                    sender_id,
                    &sender_name,
                    &eval.target_reviewer,
                    subject,
                    wire_body,
                )?;
            }

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "number": pr.number,
                        "html_url": pr.html_url,
                        "state": pr.state,
                        "title": pr.title,
                        "head_ref": pr.head_ref,
                        "head_sha": pr.head_sha,
                        "base_ref": pr.base_ref,
                        "requested_reviewers": pr.requested_reviewers,
                        "project": eval.project,
                        "co_authors": eval.co_authors,
                        "unattributed_commits": eval.unattributed_commits,
                        "unknown_co_authors": eval.unknown_co_authors,
                        "protected_files": eval.protected_files,
                        "required_tier": eval.required_tier,
                        "target_reviewer": eval.target_reviewer,
                        "escalation_reasons": eval.escalation_reasons,
                        "policy": eval,
                    }))?
                );
            } else {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    pr.number, pr.state, eval.required_tier, eval.target_reviewer, pr.html_url
                );
            }
            Ok(0)
        }

        PrCmd::Policy {
            number,
            project,
            json,
        } => {
            let project_slug = resolve_project_slug(project.as_deref(), default_project, repo);
            let pr = get_pr(gh.client(), repo, number)?;
            let commits = list_pr_commits(gh.client(), repo, number)?;
            let files = list_pr_files(gh.client(), repo, number)?;
            let reviews = list_pr_reviews(gh.client(), repo, number)?;
            let agents = ws_store.list_agents(None, true)?;
            let eval = evaluate_pr_policy(
                &project_slug,
                &commits,
                &files,
                &[],
                &agents,
                DEFAULT_OWNER_LOGIN,
            );
            let approved_by =
                verify_merge_approvals(&pr.head_sha, &eval, &reviews, &agents, DEFAULT_OWNER_LOGIN)
                    .ok();

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "number": pr.number,
                        "head_sha": pr.head_sha,
                        "project": eval.project,
                        "co_authors": eval.co_authors,
                        "unattributed_commits": eval.unattributed_commits,
                        "unknown_co_authors": eval.unknown_co_authors,
                        "protected_files": eval.protected_files,
                        "required_tier": eval.required_tier,
                        "target_reviewer": eval.target_reviewer,
                        "escalation_reasons": eval.escalation_reasons,
                        "approved": approved_by.is_some(),
                        "approved_by": approved_by,
                    }))?
                );
            } else {
                println!("pr\t#{}", pr.number);
                println!("project\t{}", eval.project);
                println!("head_sha\t{}", pr.head_sha);
                println!(
                    "co_authors\t{}",
                    if eval.co_authors.is_empty() {
                        "none".to_string()
                    } else {
                        eval.co_authors.join(",")
                    }
                );
                println!("required_tier\t{}", eval.required_tier);
                println!("target_reviewer\t{}", eval.target_reviewer);
                if !eval.protected_files.is_empty() {
                    println!("protected_files\t{}", eval.protected_files.join(","));
                }
                println!("approved\t{}", approved_by.as_deref().unwrap_or("false"));
            }
            Ok(0)
        }

        PrCmd::Review {
            number,
            approve,
            request_changes,
            comment: _,
            body,
            by,
            project,
            json,
        } => {
            let (verdict, gh_event) = if approve {
                (ReviewVerdict::Approved, "APPROVE")
            } else if request_changes {
                (ReviewVerdict::ChangesRequested, "REQUEST_CHANGES")
            } else {
                (ReviewVerdict::Commented, "COMMENT")
            };

            let project_slug = resolve_project_slug(project.as_deref(), default_project, repo);
            let pr = get_pr(gh.client(), repo, number)?;
            let commits = list_pr_commits(gh.client(), repo, number)?;
            let files = list_pr_files(gh.client(), repo, number)?;
            let agents = ws_store.list_agents(None, true)?;
            let eval = evaluate_pr_policy(
                &project_slug,
                &commits,
                &files,
                &[],
                &agents,
                DEFAULT_OWNER_LOGIN,
            );

            let caller_key = by.trim().to_ascii_lowercase();
            let Some(caller) = ws_store.agent_by_name(&caller_key)? else {
                return emit_refusal(
                    &PolicyRefusal::UnknownOrInactiveAgent { agent: caller_key },
                    json,
                );
            };
            if caller.status != AgentStatus::Active {
                return emit_refusal(
                    &PolicyRefusal::UnknownOrInactiveAgent { agent: caller_key },
                    json,
                );
            }

            if matches!(
                verdict,
                ReviewVerdict::Approved | ReviewVerdict::ChangesRequested
            ) && let Err(refusal) = authorize_agent_action(&eval, &caller)
            {
                return emit_refusal(&refusal, json);
            }

            let marker = FlReviewMarker {
                reviewer: caller.name_key.clone(),
                role: caller.role.clone(),
                project: eval.project.clone(),
                head_sha: pr.head_sha.clone(),
                verdict,
                at: crate::cmd::agent::now_at(),
            };
            let formatted_body = format_review_body(&body, &marker);

            let review = if matches!(
                verdict,
                ReviewVerdict::Approved | ReviewVerdict::ChangesRequested
            ) {
                let owned_app = reviewer_client(gh, binding, github_app)?;
                let client = owned_app.as_ref().unwrap_or_else(|| gh.client());
                submit_pr_review(
                    client,
                    repo,
                    number,
                    &pr.head_sha,
                    gh_event,
                    &formatted_body,
                )?
            } else {
                let owned_app = reviewer_client(gh, binding, github_app).ok().flatten();
                let client = owned_app.as_ref().unwrap_or_else(|| gh.client());
                submit_pr_review(
                    client,
                    repo,
                    number,
                    &pr.head_sha,
                    gh_event,
                    &formatted_body,
                )?
            };

            for co_author in &eval.co_authors {
                if !co_author.eq_ignore_ascii_case(&caller.name_key) {
                    let subject = format!(
                        "PR #{} reviewed by {} ({}): {}",
                        pr.number, caller.name_key, verdict, pr.title
                    );
                    let wire_body = format!(
                        "Agent `{}` ({}) submitted a `{}` review on PR #{} ({}) at head `{}`:\n\n{}",
                        caller.name_key,
                        caller.role,
                        verdict,
                        pr.number,
                        pr.html_url,
                        pr.head_sha,
                        body.trim()
                    );
                    send_wire_notification(
                        ws_store,
                        Some(caller.id.clone()),
                        &caller.name_key,
                        co_author,
                        subject,
                        wire_body,
                    )?;
                }
            }

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "number": pr.number,
                        "review_id": review.id,
                        "state": review.state,
                        "verdict": verdict,
                        "reviewer": caller.name_key,
                        "role": caller.role,
                        "head_sha": pr.head_sha,
                        "author_login": review.author_login,
                    }))?
                );
            } else {
                println!(
                    "reviewed\t{}\t{}\t{}\t{}",
                    pr.number, verdict, caller.name_key, pr.head_sha
                );
            }
            Ok(0)
        }

        PrCmd::Merge {
            number,
            method,
            by,
            project,
            json,
        } => {
            let project_slug = resolve_project_slug(project.as_deref(), default_project, repo);
            let pr = get_pr(gh.client(), repo, number)?;
            let commits = list_pr_commits(gh.client(), repo, number)?;
            let files = list_pr_files(gh.client(), repo, number)?;
            let reviews = list_pr_reviews(gh.client(), repo, number)?;
            let agents = ws_store.list_agents(None, true)?;
            let eval = evaluate_pr_policy(
                &project_slug,
                &commits,
                &files,
                &[],
                &agents,
                DEFAULT_OWNER_LOGIN,
            );

            let caller_key = by.trim().to_ascii_lowercase();
            let Some(caller) = ws_store.agent_by_name(&caller_key)? else {
                return emit_refusal(
                    &PolicyRefusal::UnknownOrInactiveAgent { agent: caller_key },
                    json,
                );
            };
            if let Err(refusal) = authorize_agent_merge(&eval, &caller) {
                return emit_refusal(&refusal, json);
            }
            let approved_by = match verify_merge_approvals(
                &pr.head_sha,
                &eval,
                &reviews,
                &agents,
                DEFAULT_OWNER_LOGIN,
            ) {
                Ok(who) => who,
                Err(refusal) => return emit_refusal(&refusal, json),
            };

            let owned_app = reviewer_client(gh, binding, github_app)?;
            let client = owned_app.as_ref().unwrap_or_else(|| gh.client());
            let merge_sha = merge_pr(client, repo, number, &pr.head_sha, method, None)?;

            for co_author in &eval.co_authors {
                if !co_author.eq_ignore_ascii_case(&caller.name_key) {
                    let subject = format!(
                        "PR #{} merged by {}: {}",
                        pr.number, caller.name_key, pr.title
                    );
                    let wire_body = format!(
                        "Pull request #{} ({}) in `{}` was merged by `{}` ({}) via `{}` (merge SHA `{}`).",
                        pr.number,
                        pr.html_url,
                        repo,
                        caller.name_key,
                        caller.role,
                        method,
                        merge_sha
                    );
                    send_wire_notification(
                        ws_store,
                        Some(caller.id.clone()),
                        &caller.name_key,
                        co_author,
                        subject,
                        wire_body,
                    )?;
                }
            }

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "number": pr.number,
                        "merged": true,
                        "merge_sha": merge_sha,
                        "method": method,
                        "merged_by": caller.name_key,
                        "approved_by": approved_by,
                        "head_sha": pr.head_sha,
                    }))?
                );
            } else {
                println!(
                    "merged\t{}\t{}\t{}\t{}",
                    pr.number, method, caller.name_key, merge_sha
                );
            }
            Ok(0)
        }

        PrCmd::VerifyCi {
            number,
            project,
            require_approval,
            json,
        } => {
            let project_slug = resolve_project_slug(project.as_deref(), default_project, repo);
            let pr = get_pr(gh.client(), repo, number)?;
            let commits = list_pr_commits(gh.client(), repo, number)?;
            let files = list_pr_files(gh.client(), repo, number)?;
            let reviews = list_pr_reviews(gh.client(), repo, number)?;
            let agents = ws_store.list_agents(None, true)?;
            let eval = evaluate_pr_policy(
                &project_slug,
                &commits,
                &files,
                &[],
                &agents,
                DEFAULT_OWNER_LOGIN,
            );

            if !eval.unattributed_commits.is_empty() {
                return emit_refusal(
                    &PolicyRefusal::UnattributedCommitsRequireOwner {
                        commits: eval.unattributed_commits.clone(),
                    },
                    json,
                );
            }
            if !agents.is_empty() && !eval.unknown_co_authors.is_empty() {
                return emit_refusal(
                    &PolicyRefusal::UnknownCoAuthorsRequireOwner {
                        co_authors: eval.unknown_co_authors.clone(),
                    },
                    json,
                );
            }

            for review in &reviews {
                let state = review.state.trim().to_ascii_uppercase();
                let on_head = review
                    .commit_id
                    .as_deref()
                    .is_none_or(|cid| cid == pr.head_sha);
                if state == "APPROVED"
                    && on_head
                    && !review
                        .author_login
                        .eq_ignore_ascii_case(DEFAULT_OWNER_LOGIN)
                {
                    let Some(marker) = extract_review_marker(&review.body) else {
                        return emit_refusal(
                            &PolicyRefusal::MissingValidApproval {
                                head_sha: pr.head_sha.clone(),
                            },
                            json,
                        );
                    };
                    if marker.verdict != ReviewVerdict::Approved || marker.head_sha != pr.head_sha {
                        return emit_refusal(
                            &PolicyRefusal::MissingValidApproval {
                                head_sha: pr.head_sha.clone(),
                            },
                            json,
                        );
                    }
                    let Some(agent) = agents.iter().find(|a| {
                        a.status == AgentStatus::Active
                            && a.name_key.eq_ignore_ascii_case(&marker.reviewer)
                            && a.role == marker.role
                    }) else {
                        return emit_refusal(
                            &PolicyRefusal::UnknownOrInactiveAgent {
                                agent: marker.reviewer,
                            },
                            json,
                        );
                    };
                    if let Err(refusal) = authorize_agent_action(&eval, agent) {
                        return emit_refusal(&refusal, json);
                    }
                }
            }

            let approved_by = if require_approval {
                match verify_merge_approvals(
                    &pr.head_sha,
                    &eval,
                    &reviews,
                    &agents,
                    DEFAULT_OWNER_LOGIN,
                ) {
                    Ok(who) => Some(who),
                    Err(refusal) => return emit_refusal(&refusal, json),
                }
            } else {
                verify_merge_approvals(&pr.head_sha, &eval, &reviews, &agents, DEFAULT_OWNER_LOGIN)
                    .ok()
            };

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "number": pr.number,
                        "head_sha": pr.head_sha,
                        "verified": true,
                        "required_tier": eval.required_tier,
                        "target_reviewer": eval.target_reviewer,
                        "approved_by": approved_by,
                    }))?
                );
            } else {
                println!(
                    "verified\t{}\t{}\t{}",
                    pr.number, eval.required_tier, pr.head_sha
                );
            }
            Ok(0)
        }
    }
}
