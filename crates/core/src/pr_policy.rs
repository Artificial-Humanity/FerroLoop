//! Pure pull-request governance policy engine (spec §1).
//!
//! Deterministic evaluation of commit `Co-authored-by:` trailers, changed file
//! paths, `AgentRegistry` records, and `<!-- fl-review: ... -->` structured
//! review markers to enforce the tiered agent rights hierarchy:
//! `assistant` / `custom:<slug>` -> `lead(<project>)` -> `lab_manager` -> `owner`.

use crate::agent::{AgentRecord, AgentRole, AgentStatus};
use crate::at::At;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Default GitHub login of the repository Owner (`lmcfarlin`).
pub const DEFAULT_OWNER_LOGIN: &str = "lmcfarlin";

/// Default protected path patterns that always require `Owner` review and merge.
pub const DEFAULT_PROTECTED_PATTERNS: &[&str] = &[".github/**", ".fl/manifest.json", ".agents/**"];

const REVIEW_MARKER_PREFIX: &str = "<!-- fl-review:";
const REVIEW_MARKER_SUFFIX: &str = "-->";

/// Required authority tier for reviewing and merging a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum RequiredTier {
    /// Project Lead for `project` (or `LabManager` / `Owner`).
    Lead { project: String },
    /// Workspace Lab Manager (or `Owner`).
    LabManager,
    /// Repository Owner (`lmcfarlin`).
    Owner,
}

impl fmt::Display for RequiredTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lead { project } => write!(f, "lead({project})"),
            Self::LabManager => f.write_str("lab_manager"),
            Self::Owner => f.write_str("owner"),
        }
    }
}

/// Parsed `Co-authored-by: <Name> <<email>>` trailer from a commit message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitTrailerAuthor {
    pub display_name: String,
    pub name_key: String,
    pub email: String,
}

/// Minimal commit metadata extracted from a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrCommitSummary {
    pub sha: String,
    pub author_login: Option<String>,
    pub message: String,
}

/// Minimal review metadata extracted from a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrReviewSummary {
    pub id: u64,
    pub author_login: String,
    pub state: String,
    pub body: String,
    pub commit_id: Option<String>,
}

/// Complete deterministic policy evaluation for a pull request.
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

/// Verdict recorded inside a structured `FlReviewMarker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approved,
    ChangesRequested,
    Commented,
}

crate::wire::wire_names!(ReviewVerdict as review_verdict_wire {
    Approved => "approved",
    ChangesRequested => "changes_requested",
    Commented => "commented",
});
crate::wire::wire_parse!(ReviewVerdict as review_verdict_parse);

impl fmt::Display for ReviewVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// Structured machine-verifiable review marker embedded in a GitHub PR review body.
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

/// Structured refusal reasons when an agent review, merge, or CI check violates policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum PolicyRefusal {
    UnknownOrInactiveAgent {
        agent: String,
    },
    SelfReviewForbidden {
        agent: String,
    },
    ProtectedPathRequiresOwner {
        agent: String,
        files: Vec<String>,
    },
    UnattributedCommitsRequireOwner {
        commits: Vec<String>,
    },
    UnknownCoAuthorsRequireOwner {
        co_authors: Vec<String>,
    },
    InsufficientRole {
        agent: String,
        role: AgentRole,
        required: RequiredTier,
    },
    MissingValidApproval {
        head_sha: String,
    },
}

impl fmt::Display for PolicyRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOrInactiveAgent { agent } => {
                write!(
                    f,
                    "agent `{agent}` is not registered as an active agent in the workspace registry"
                )
            }
            Self::SelfReviewForbidden { agent } => {
                write!(
                    f,
                    "self-review forbidden: agent `{agent}` is a co-author on this pull request"
                )
            }
            Self::ProtectedPathRequiresOwner { agent, files } => {
                write!(
                    f,
                    "agent `{agent}` cannot approve or merge protected paths ({}); Owner review required",
                    files.join(", ")
                )
            }
            Self::UnattributedCommitsRequireOwner { commits } => {
                write!(
                    f,
                    "commits lacking `Co-authored-by:` trailers ({}) require Owner review",
                    commits.join(", ")
                )
            }
            Self::UnknownCoAuthorsRequireOwner { co_authors } => {
                write!(
                    f,
                    "unregistered or inactive co-authors ({}) require Owner review",
                    co_authors.join(", ")
                )
            }
            Self::InsufficientRole {
                agent,
                role,
                required,
            } => {
                write!(
                    f,
                    "agent `{agent}` (role `{role}`) lacks authority for required tier `{required}`"
                )
            }
            Self::MissingValidApproval { head_sha } => {
                write!(
                    f,
                    "pull request head `{head_sha}` does not have a valid approval from the required reviewer tier"
                )
            }
        }
    }
}

impl std::error::Error for PolicyRefusal {}

/// Parse all `Co-authored-by: <Name> <<email>>` trailers from a commit message.
pub fn parse_co_authors(message: &str) -> Vec<CommitTrailerAuthor> {
    let mut out = Vec::new();
    for line in message.lines() {
        let trimmed = line.trim();
        let Some((prefix, rest)) = trimmed.split_once(':') else {
            continue;
        };
        if !prefix.trim().eq_ignore_ascii_case("co-authored-by") {
            continue;
        }
        let rest = rest.trim();
        let Some((raw_name, email_part)) = rest.rsplit_once('<') else {
            continue;
        };
        let Some(raw_email) = email_part.strip_suffix('>') else {
            continue;
        };
        let display_name = raw_name.trim().to_string();
        let email = raw_email.trim().to_ascii_lowercase();
        if display_name.is_empty() || email.is_empty() || !email.contains('@') {
            continue;
        }
        let name_key = if display_name.chars().any(char::is_whitespace) {
            email
                .split_once('@')
                .map(|(local, _)| local.to_ascii_lowercase())
                .unwrap_or_else(|| display_name.to_ascii_lowercase())
        } else {
            display_name.to_ascii_lowercase()
        };
        out.push(CommitTrailerAuthor {
            display_name,
            name_key,
            email,
        });
    }
    out
}

fn matches_pattern(normalized_path: &str, raw_pattern: &str) -> bool {
    let pattern = raw_pattern
        .trim()
        .trim_start_matches("./")
        .trim_matches('/');
    if pattern.is_empty() {
        return false;
    }
    if let Some(dir) = pattern.strip_suffix("/**") {
        let dir = dir.trim_matches('/');
        return normalized_path == dir || normalized_path.starts_with(&format!("{dir}/"));
    }
    if let Some(suffix) = pattern.strip_prefix("**/") {
        let suffix = suffix.trim_matches('/');
        return normalized_path == suffix || normalized_path.ends_with(&format!("/{suffix}"));
    }
    if !pattern.contains('/') {
        return normalized_path == pattern || normalized_path.ends_with(&format!("/{pattern}"));
    }
    normalized_path == pattern
}

/// Check whether `path` matches any default protected pattern or `extra_patterns`.
pub fn is_protected_path(path: &str, extra_patterns: &[String]) -> bool {
    let normalized = path.trim().trim_start_matches("./").trim_matches('/');
    if normalized.is_empty() {
        return false;
    }
    DEFAULT_PROTECTED_PATTERNS
        .iter()
        .any(|pat| matches_pattern(normalized, pat))
        || extra_patterns
            .iter()
            .any(|pat| matches_pattern(normalized, pat))
}

fn find_active_agent<'a>(
    agents: &'a [AgentRecord],
    author: &CommitTrailerAuthor,
) -> Option<&'a AgentRecord> {
    agents.iter().find(|a| {
        a.status == AgentStatus::Active
            && (a.name_key.eq_ignore_ascii_case(&author.name_key)
                || a.email.eq_ignore_ascii_case(&author.email))
    })
}

fn push_unique(vec: &mut Vec<String>, item: String) {
    if !vec.contains(&item) {
        vec.push(item);
    }
}

/// Evaluate pull-request governance policy deterministically from commits, files, and registry state.
pub fn evaluate_pr_policy(
    project: &str,
    commits: &[PrCommitSummary],
    changed_files: &[String],
    extra_protected_patterns: &[String],
    agents: &[AgentRecord],
    owner_login: &str,
) -> PrPolicyEvaluation {
    let owner = if owner_login.trim().is_empty() {
        DEFAULT_OWNER_LOGIN
    } else {
        owner_login.trim()
    };
    let project_slug = project.trim().to_ascii_lowercase();

    let mut co_authors = Vec::new();
    let mut co_author_roles = Vec::new();
    let mut unattributed_commits = Vec::new();
    let mut unknown_co_authors = Vec::new();
    let mut owner_authored_commits = false;

    for commit in commits {
        let trailers = parse_co_authors(&commit.message);
        let is_owner_commit = commit
            .author_login
            .as_deref()
            .is_some_and(|login| login.eq_ignore_ascii_case(owner));

        if trailers.is_empty() {
            if is_owner_commit {
                owner_authored_commits = true;
            } else {
                push_unique(&mut unattributed_commits, commit.sha.clone());
            }
            continue;
        }

        for trailer in trailers {
            if trailer.name_key.eq_ignore_ascii_case(owner) {
                owner_authored_commits = true;
                continue;
            }
            match find_active_agent(agents, &trailer) {
                Some(agent) => {
                    push_unique(&mut co_authors, agent.name_key.clone());
                    if !co_author_roles.contains(&agent.role) {
                        co_author_roles.push(agent.role.clone());
                    }
                }
                None => {
                    push_unique(&mut unknown_co_authors, trailer.name_key);
                }
            }
        }
    }

    let mut protected_files = Vec::new();
    for file in changed_files {
        if is_protected_path(file, extra_protected_patterns) {
            push_unique(&mut protected_files, file.clone());
        }
    }

    let mut escalation_reasons = Vec::new();
    if !protected_files.is_empty() {
        escalation_reasons.push(format!(
            "touches protected path(s): {}",
            protected_files.join(", ")
        ));
    }
    if commits.is_empty() {
        escalation_reasons.push("pull request has no commits".to_string());
    }
    if !unattributed_commits.is_empty() {
        escalation_reasons.push(format!(
            "contains unattributed commit(s) without Co-authored-by trailer: {}",
            unattributed_commits.join(", ")
        ));
    }
    if !unknown_co_authors.is_empty() {
        escalation_reasons.push(format!(
            "contains unregistered or inactive co-author(s): {}",
            unknown_co_authors.join(", ")
        ));
    }
    if owner_authored_commits && co_authors.is_empty() {
        escalation_reasons.push("authored directly by repository Owner".to_string());
    }

    let has_lab_manager_co_author = co_author_roles.contains(&AgentRole::LabManager);
    let has_lead_co_author = co_author_roles.contains(&AgentRole::Lead);

    if has_lab_manager_co_author {
        escalation_reasons.push("co-authored by lab_manager".to_string());
    } else if has_lead_co_author {
        escalation_reasons.push("co-authored by project lead".to_string());
    }

    let active_lab_manager = agents.iter().find(|a| {
        a.status == AgentStatus::Active
            && a.role == AgentRole::LabManager
            && !co_authors.contains(&a.name_key)
    });

    let active_project_lead = agents.iter().find(|a| {
        a.status == AgentStatus::Active
            && a.role == AgentRole::Lead
            && a.covers_area(&project_slug)
            && !co_authors.contains(&a.name_key)
    });

    let (required_tier, target_reviewer) = if !protected_files.is_empty()
        || commits.is_empty()
        || !unattributed_commits.is_empty()
        || !unknown_co_authors.is_empty()
        || co_authors.is_empty()
        || has_lab_manager_co_author
    {
        (RequiredTier::Owner, owner.to_string())
    } else if has_lead_co_author {
        if let Some(lm) = active_lab_manager {
            (RequiredTier::LabManager, lm.name_key.clone())
        } else {
            escalation_reasons.push(
                "no active non-co-author lab_manager available; escalating to owner".to_string(),
            );
            (RequiredTier::Owner, owner.to_string())
        }
    } else if let Some(lead) = active_project_lead {
        (
            RequiredTier::Lead {
                project: project_slug.clone(),
            },
            lead.name_key.clone(),
        )
    } else if let Some(lm) = active_lab_manager {
        escalation_reasons.push(format!(
            "no active non-co-author lead for `{project_slug}`; escalating to lab_manager"
        ));
        (RequiredTier::LabManager, lm.name_key.clone())
    } else {
        escalation_reasons.push(format!(
            "no active lead or lab_manager available for `{project_slug}`; escalating to owner"
        ));
        (RequiredTier::Owner, owner.to_string())
    };

    PrPolicyEvaluation {
        project: project_slug,
        co_authors,
        unattributed_commits,
        unknown_co_authors,
        protected_files,
        required_tier,
        target_reviewer,
        escalation_reasons,
    }
}

/// Format a GitHub pull-request review body with an embedded `FlReviewMarker` HTML comment.
pub fn format_review_body(prose: &str, marker: &FlReviewMarker) -> String {
    let json = serde_json::to_string(marker).expect("FlReviewMarker serializes to JSON");
    let trimmed = prose.trim();
    if trimmed.is_empty() {
        format!("{REVIEW_MARKER_PREFIX} {json} {REVIEW_MARKER_SUFFIX}")
    } else {
        format!("{trimmed}\n\n{REVIEW_MARKER_PREFIX} {json} {REVIEW_MARKER_SUFFIX}")
    }
}

/// Extract and deserialize an `FlReviewMarker` from a GitHub pull-request review body.
pub fn extract_review_marker(body: &str) -> Option<FlReviewMarker> {
    let start = body.rfind(REVIEW_MARKER_PREFIX)?;
    let after_prefix = &body[start + REVIEW_MARKER_PREFIX.len()..];
    let end = after_prefix.find(REVIEW_MARKER_SUFFIX)?;
    let json_slice = after_prefix[..end].trim();
    serde_json::from_str(json_slice).ok()
}

/// Verify whether `caller` is authorized to approve or merge a pull request with policy `eval`.
pub fn authorize_agent_action(
    eval: &PrPolicyEvaluation,
    caller: &AgentRecord,
) -> Result<(), PolicyRefusal> {
    if caller.status != AgentStatus::Active {
        return Err(PolicyRefusal::UnknownOrInactiveAgent {
            agent: caller.name_key.clone(),
        });
    }
    if eval
        .co_authors
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&caller.name_key))
    {
        return Err(PolicyRefusal::SelfReviewForbidden {
            agent: caller.name_key.clone(),
        });
    }
    if !eval.protected_files.is_empty() {
        return Err(PolicyRefusal::ProtectedPathRequiresOwner {
            agent: caller.name_key.clone(),
            files: eval.protected_files.clone(),
        });
    }
    if !eval.unattributed_commits.is_empty() {
        return Err(PolicyRefusal::UnattributedCommitsRequireOwner {
            commits: eval.unattributed_commits.clone(),
        });
    }
    if !eval.unknown_co_authors.is_empty() {
        return Err(PolicyRefusal::UnknownCoAuthorsRequireOwner {
            co_authors: eval.unknown_co_authors.clone(),
        });
    }

    match &eval.required_tier {
        RequiredTier::Lead { project } => {
            let authorized = (caller.role == AgentRole::Lead && caller.covers_area(project))
                || caller.role == AgentRole::LabManager;
            if !authorized {
                return Err(PolicyRefusal::InsufficientRole {
                    agent: caller.name_key.clone(),
                    role: caller.role.clone(),
                    required: eval.required_tier.clone(),
                });
            }
        }
        RequiredTier::LabManager => {
            if caller.role != AgentRole::LabManager {
                return Err(PolicyRefusal::InsufficientRole {
                    agent: caller.name_key.clone(),
                    role: caller.role.clone(),
                    required: eval.required_tier.clone(),
                });
            }
        }
        RequiredTier::Owner => {
            return Err(PolicyRefusal::InsufficientRole {
                agent: caller.name_key.clone(),
                role: caller.role.clone(),
                required: RequiredTier::Owner,
            });
        }
    }

    Ok(())
}

/// Verify whether `caller` is authorized to merge a pull request with policy `eval`
/// once required approvals (`verify_merge_approvals`) are satisfied.
///
/// Under the Author-Owned Merge rule (spec §4.3), a registered active co-author in
/// `eval.co_authors` may merge their own pull request across all tiers (`Lead`,
/// `LabManager`, and `Owner`-reviewed PRs) once the required reviewer tier has
/// approved `head_sha`. Non-co-author reviewers meeting `authorize_agent_action`
/// are also accepted.
pub fn authorize_agent_merge(
    eval: &PrPolicyEvaluation,
    caller: &AgentRecord,
) -> Result<(), PolicyRefusal> {
    if caller.status != AgentStatus::Active {
        return Err(PolicyRefusal::UnknownOrInactiveAgent {
            agent: caller.name_key.clone(),
        });
    }
    if !eval.unattributed_commits.is_empty() {
        return Err(PolicyRefusal::UnattributedCommitsRequireOwner {
            commits: eval.unattributed_commits.clone(),
        });
    }
    if !eval.unknown_co_authors.is_empty() {
        return Err(PolicyRefusal::UnknownCoAuthorsRequireOwner {
            co_authors: eval.unknown_co_authors.clone(),
        });
    }
    if eval
        .co_authors
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&caller.name_key))
    {
        return Ok(());
    }
    authorize_agent_action(eval, caller)
}

#[derive(Debug, Clone)]
struct FoldedReviewState {
    label: String,
    state: String,
    valid_for_head: bool,
    authorized_reviewer: bool,
}

/// Verify that `reviews` contain an active, unrevoked approval for `head_sha` from a reviewer
/// meeting `eval.required_tier` (folding reviews chronologically per logical reviewer).
pub fn verify_merge_approvals(
    head_sha: &str,
    eval: &PrPolicyEvaluation,
    reviews: &[PrReviewSummary],
    agents: &[AgentRecord],
    owner_login: &str,
) -> Result<String, PolicyRefusal> {
    let owner = if owner_login.trim().is_empty() {
        DEFAULT_OWNER_LOGIN
    } else {
        owner_login.trim()
    };

    let mut latest_by_reviewer: BTreeMap<String, FoldedReviewState> = BTreeMap::new();

    for review in reviews {
        let state = review.state.trim().to_ascii_uppercase();
        if !matches!(
            state.as_str(),
            "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
        ) {
            // `COMMENTED` or `PENDING` reviews do not overwrite an earlier state-changing review.
            continue;
        }

        if review.author_login.eq_ignore_ascii_case(owner) {
            let key = format!("owner:{}", owner.to_ascii_lowercase());
            let valid_for_head =
                state == "APPROVED" && review.commit_id.as_deref() == Some(head_sha);
            latest_by_reviewer.insert(
                key,
                FoldedReviewState {
                    label: owner.to_string(),
                    state,
                    valid_for_head,
                    authorized_reviewer: true,
                },
            );
            continue;
        }

        if let Some(marker) = extract_review_marker(&review.body) {
            let reviewer_key = marker.reviewer.to_ascii_lowercase();
            let key = format!("agent:{reviewer_key}");
            let matched_agent = agents.iter().find(|a| {
                a.status == AgentStatus::Active
                    && a.name_key.eq_ignore_ascii_case(&marker.reviewer)
                    && a.role == marker.role
            });
            let authorized = matched_agent
                .map(|agent| authorize_agent_action(eval, agent).is_ok())
                .unwrap_or(false);
            let commit_matches = review
                .commit_id
                .as_deref()
                .is_none_or(|cid| cid == head_sha);
            let valid_for_head = state == "APPROVED"
                && marker.verdict == ReviewVerdict::Approved
                && marker.head_sha == head_sha
                && commit_matches
                && authorized;

            latest_by_reviewer.insert(
                key,
                FoldedReviewState {
                    label: marker.reviewer,
                    state,
                    valid_for_head,
                    authorized_reviewer: authorized,
                },
            );
        }
    }

    // If any authorized reviewer's latest state-changing review is CHANGES_REQUESTED, block merge.
    if latest_by_reviewer
        .values()
        .any(|r| r.authorized_reviewer && r.state == "CHANGES_REQUESTED")
    {
        return Err(PolicyRefusal::MissingValidApproval {
            head_sha: head_sha.to_string(),
        });
    }

    if let Some(approved) = latest_by_reviewer
        .values()
        .find(|r| r.state == "APPROVED" && r.valid_for_head)
    {
        return Ok(approved.label.clone());
    }

    Err(PolicyRefusal::MissingValidApproval {
        head_sha: head_sha.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AgentId, seq_iri};

    fn make_agent(
        seq: u64,
        name: &str,
        project: &str,
        role: AgentRole,
        extra_areas: &[&str],
    ) -> AgentRecord {
        let name_key = name.to_ascii_lowercase();
        AgentRecord {
            id: AgentId(seq_iri(seq)),
            email: format!("{name_key}@artificialhumanity.io"),
            name_key,
            display_name: name.to_string(),
            project: project.to_string(),
            role,
            extra_areas: extra_areas.iter().map(|s| (*s).to_string()).collect(),
            status: AgentStatus::Active,
            updated_at: At::from_unix_millis(1_000),
            by: "owner".to_string(),
        }
    }

    fn sample_roster() -> Vec<AgentRecord> {
        vec![
            make_agent(1, "Jarvis", "ai-lab-amd", AgentRole::LabManager, &[]),
            make_agent(2, "Ferris", "ferroloop", AgentRole::Lead, &[]),
            make_agent(3, "Felicia", "ferroloop", AgentRole::Assistant, &[]),
            make_agent(
                4,
                "Kacey",
                "ferroloop",
                AgentRole::Custom("scribe".into()),
                &[],
            ),
            make_agent(5, "Sonya", "sonora", AgentRole::Lead, &[]),
        ]
    }

    #[test]
    fn parse_co_authors_handles_single_multiple_and_case_variations() {
        let msg = "\
feat(cli): add github pr subcommands

Implemented policy engine and broker.

Co-authored-by: Felicia <felicia@artificialhumanity.io>
co-authored-by: Ferris <ferris@artificialhumanity.io>
CO-AUTHORED-BY: Special Agent <special@artificialhumanity.io>
Signed-off-by: Someone <someone@example.com>
";
        let authors = parse_co_authors(msg);
        assert_eq!(
            authors,
            vec![
                CommitTrailerAuthor {
                    display_name: "Felicia".into(),
                    name_key: "felicia".into(),
                    email: "felicia@artificialhumanity.io".into(),
                },
                CommitTrailerAuthor {
                    display_name: "Ferris".into(),
                    name_key: "ferris".into(),
                    email: "ferris@artificialhumanity.io".into(),
                },
                CommitTrailerAuthor {
                    display_name: "Special Agent".into(),
                    name_key: "special".into(),
                    email: "special@artificialhumanity.io".into(),
                },
            ]
        );
        assert!(parse_co_authors("chore: commit without trailers").is_empty());
    }

    #[test]
    fn is_protected_path_matches_governance_ci_and_manifest_files() {
        let extra = vec!["secrets/**".to_string(), "config/prod.toml".to_string()];
        assert!(is_protected_path(".github/workflows/ci.yml", &extra));
        assert!(is_protected_path("./.github/CODEOWNERS", &extra));
        assert!(is_protected_path(".fl/manifest.json", &extra));
        assert!(is_protected_path(".agents/AGENTS.md", &extra));
        assert!(is_protected_path(".agents/roles/lead.md", &extra));
        assert!(is_protected_path("secrets/token.txt", &extra));
        assert!(is_protected_path("config/prod.toml", &extra));

        // Repo-level instruction files follow the normal tiered hierarchy (Assistant -> Lead -> LabManager).
        assert!(!is_protected_path("AGENTS.md", &extra));
        assert!(!is_protected_path("crates/core/AGENTS.md", &extra));
        assert!(!is_protected_path("CLAUDE.md", &extra));
        assert!(!is_protected_path("WORKFLOW.md", &extra));
        assert!(!is_protected_path("PERSONA.md", &extra));
        assert!(!is_protected_path("CONTRIBUTING.md", &extra));
        assert!(!is_protected_path("crates/core/src/pr_policy.rs", &extra));
        assert!(!is_protected_path("README.md", &extra));
        assert!(!is_protected_path("docs/AGENTS_GUIDE.md", &extra));
    }

    #[test]
    fn escalation_ladder_routes_assistant_to_lead_to_lab_manager_to_owner() {
        let agents = sample_roster();

        // 1. Assistant PR -> Project Lead (ferris)
        let eval_assistant = evaluate_pr_policy(
            "ferroloop",
            &[PrCommitSummary {
                sha: "sha1".into(),
                author_login: Some("artificially-human".into()),
                message: "feat: task\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
            }],
            &["crates/core/src/lib.rs".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );
        assert_eq!(
            eval_assistant.required_tier,
            RequiredTier::Lead {
                project: "ferroloop".into()
            }
        );
        assert_eq!(eval_assistant.target_reviewer, "ferris");
        assert_eq!(eval_assistant.co_authors, vec!["felicia"]);

        // 2. Assistant PR in a project with no active Lead -> LabManager (jarvis)
        let eval_no_lead = evaluate_pr_policy(
            "prosodia",
            &[PrCommitSummary {
                sha: "sha1".into(),
                author_login: Some("artificially-human".into()),
                message: "feat: task\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
            }],
            &["src/main.rs".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );
        assert_eq!(eval_no_lead.required_tier, RequiredTier::LabManager);
        assert_eq!(eval_no_lead.target_reviewer, "jarvis");

        // 3. Lead PR (or mixed Assistant + Lead) -> LabManager (jarvis)
        let eval_lead = evaluate_pr_policy(
            "ferroloop",
            &[
                PrCommitSummary {
                    sha: "sha1".into(),
                    author_login: Some("artificially-human".into()),
                    message:
                        "feat: part 1\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                            .into(),
                },
                PrCommitSummary {
                    sha: "sha2".into(),
                    author_login: Some("artificially-human".into()),
                    message:
                        "feat: part 2\n\nCo-authored-by: Ferris <ferris@artificialhumanity.io>"
                            .into(),
                },
            ],
            &["crates/core/src/lib.rs".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );
        assert_eq!(eval_lead.required_tier, RequiredTier::LabManager);
        assert_eq!(eval_lead.target_reviewer, "jarvis");

        // 4. LabManager PR -> Owner (lmcfarlin)
        let eval_lm = evaluate_pr_policy(
            "ai-lab-amd",
            &[PrCommitSummary {
                sha: "sha1".into(),
                author_login: Some("artificially-human".into()),
                message: "feat: infra\n\nCo-authored-by: Jarvis <jarvis@artificialhumanity.io>"
                    .into(),
            }],
            &["scripts/setup.sh".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );
        assert_eq!(eval_lm.required_tier, RequiredTier::Owner);
        assert_eq!(eval_lm.target_reviewer, "lmcfarlin");

        // 5. Protected path touched by Assistant -> Owner (lmcfarlin)
        let eval_protected = evaluate_pr_policy(
            "ferroloop",
            &[PrCommitSummary {
                sha: "sha1".into(),
                author_login: Some("artificially-human".into()),
                message: "ci: tweak\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
            }],
            &[".github/workflows/ci.yml".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );
        assert_eq!(eval_protected.required_tier, RequiredTier::Owner);
        assert_eq!(eval_protected.target_reviewer, "lmcfarlin");
        assert_eq!(
            eval_protected.protected_files,
            vec![".github/workflows/ci.yml"]
        );

        // 6. Unattributed commit -> Owner (lmcfarlin)
        let eval_unattributed = evaluate_pr_policy(
            "ferroloop",
            &[PrCommitSummary {
                sha: "deadbeef".into(),
                author_login: Some("artificially-human".into()),
                message: "chore: mystery commit".into(),
            }],
            &["crates/core/src/lib.rs".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );
        assert_eq!(eval_unattributed.required_tier, RequiredTier::Owner);
        assert_eq!(eval_unattributed.unattributed_commits, vec!["deadbeef"]);
    }

    #[test]
    fn authorize_agent_action_enforces_self_review_and_role_boundaries() {
        let agents = sample_roster();
        let jarvis = &agents[0];
        let ferris = &agents[1];
        let felicia = &agents[2];
        let sonya = &agents[4];

        // Assistant PR on ferroloop
        let eval_assistant = evaluate_pr_policy(
            "ferroloop",
            &[PrCommitSummary {
                sha: "sha1".into(),
                author_login: Some("artificially-human".into()),
                message: "feat: work\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
            }],
            &["crates/core/src/lib.rs".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );

        // Self-review by felicia -> SelfReviewForbidden
        assert_eq!(
            authorize_agent_action(&eval_assistant, felicia),
            Err(PolicyRefusal::SelfReviewForbidden {
                agent: "felicia".into()
            })
        );

        // Lead of another project (sonya) -> InsufficientRole
        assert_eq!(
            authorize_agent_action(&eval_assistant, sonya),
            Err(PolicyRefusal::InsufficientRole {
                agent: "sonya".into(),
                role: AgentRole::Lead,
                required: RequiredTier::Lead {
                    project: "ferroloop".into()
                },
            })
        );

        // Project Lead (ferris) and LabManager (jarvis) are both authorized
        assert_eq!(authorize_agent_action(&eval_assistant, ferris), Ok(()));
        assert_eq!(authorize_agent_action(&eval_assistant, jarvis), Ok(()));

        // Lead PR by ferris -> ferris cannot self-review; sonya lacks role; jarvis succeeds
        let eval_lead = evaluate_pr_policy(
            "ferroloop",
            &[PrCommitSummary {
                sha: "sha2".into(),
                author_login: Some("artificially-human".into()),
                message: "feat: lead work\n\nCo-authored-by: Ferris <ferris@artificialhumanity.io>"
                    .into(),
            }],
            &["crates/core/src/lib.rs".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );
        assert_eq!(
            authorize_agent_action(&eval_lead, ferris),
            Err(PolicyRefusal::SelfReviewForbidden {
                agent: "ferris".into()
            })
        );
        assert_eq!(
            authorize_agent_action(&eval_lead, sonya),
            Err(PolicyRefusal::InsufficientRole {
                agent: "sonya".into(),
                role: AgentRole::Lead,
                required: RequiredTier::LabManager,
            })
        );
        assert_eq!(authorize_agent_action(&eval_lead, jarvis), Ok(()));
    }

    #[test]
    fn review_marker_round_trip_and_chronological_merge_approval_folding() {
        let agents = sample_roster();
        let ferris = &agents[1];

        let eval = evaluate_pr_policy(
            "ferroloop",
            &[PrCommitSummary {
                sha: "head-1".into(),
                author_login: Some("artificially-human".into()),
                message: "feat: item\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
            }],
            &["crates/core/src/lib.rs".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );

        let marker = FlReviewMarker {
            reviewer: ferris.name_key.clone(),
            role: ferris.role.clone(),
            project: "ferroloop".into(),
            head_sha: "head-1".into(),
            verdict: ReviewVerdict::Approved,
            at: At::from_unix_millis(5_000),
        };
        let body = format_review_body("Looks good to me!", &marker);
        assert_eq!(extract_review_marker(&body), Some(marker.clone()));

        let approve_review = PrReviewSummary {
            id: 10,
            author_login: "ferroloop-reviewer[bot]".into(),
            state: "APPROVED".into(),
            body: body.clone(),
            commit_id: Some("head-1".into()),
        };

        // 1. Valid approval on matching head_sha passes
        assert_eq!(
            verify_merge_approvals(
                "head-1",
                &eval,
                std::slice::from_ref(&approve_review),
                &agents,
                DEFAULT_OWNER_LOGIN
            ),
            Ok("ferris".to_string())
        );

        // 2. Stale head_sha fails
        assert_eq!(
            verify_merge_approvals(
                "head-2",
                &eval,
                std::slice::from_ref(&approve_review),
                &agents,
                DEFAULT_OWNER_LOGIN
            ),
            Err(PolicyRefusal::MissingValidApproval {
                head_sha: "head-2".into()
            })
        );

        // 3. Approval followed by COMMENTED still passes
        let comment_marker = FlReviewMarker {
            verdict: ReviewVerdict::Commented,
            ..marker.clone()
        };
        let comment_review = PrReviewSummary {
            id: 11,
            author_login: "ferroloop-reviewer[bot]".into(),
            state: "COMMENTED".into(),
            body: format_review_body("One minor note", &comment_marker),
            commit_id: Some("head-1".into()),
        };
        assert_eq!(
            verify_merge_approvals(
                "head-1",
                &eval,
                &[approve_review.clone(), comment_review],
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Ok("ferris".to_string())
        );

        // 4. Approval followed by CHANGES_REQUESTED fails
        let changes_marker = FlReviewMarker {
            verdict: ReviewVerdict::ChangesRequested,
            ..marker.clone()
        };
        let changes_review = PrReviewSummary {
            id: 12,
            author_login: "ferroloop-reviewer[bot]".into(),
            state: "CHANGES_REQUESTED".into(),
            body: format_review_body("Wait, found a bug", &changes_marker),
            commit_id: Some("head-1".into()),
        };
        assert_eq!(
            verify_merge_approvals(
                "head-1",
                &eval,
                &[approve_review.clone(), changes_review],
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Err(PolicyRefusal::MissingValidApproval {
                head_sha: "head-1".into()
            })
        );

        // 5. Approval followed by DISMISSED fails
        let dismissed_review = PrReviewSummary {
            id: 13,
            author_login: "ferroloop-reviewer[bot]".into(),
            state: "DISMISSED".into(),
            body,
            commit_id: Some("head-1".into()),
        };
        assert_eq!(
            verify_merge_approvals(
                "head-1",
                &eval,
                &[approve_review, dismissed_review],
                &agents,
                DEFAULT_OWNER_LOGIN,
            ),
            Err(PolicyRefusal::MissingValidApproval {
                head_sha: "head-1".into()
            })
        );
    }

    #[test]
    fn author_owned_merge_allows_co_author_across_tiers_while_forbidding_self_review() {
        let agents = sample_roster();
        let jarvis = &agents[0];
        let ferris = &agents[1];
        let felicia = &agents[2];
        let kacey = &agents[3];
        let sonya = &agents[4];

        // 1. Assistant PR (`felicia` co-author, required_tier = Lead(ferroloop))
        let eval_assistant = evaluate_pr_policy(
            "ferroloop",
            &[PrCommitSummary {
                sha: "sha1".into(),
                author_login: Some("artificially-human".into()),
                message: "feat: task\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
            }],
            &["crates/core/src/lib.rs".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );
        // `felicia` cannot self-review, but IS authorized to merge her own PR once approved.
        assert_eq!(
            authorize_agent_action(&eval_assistant, felicia),
            Err(PolicyRefusal::SelfReviewForbidden {
                agent: "felicia".into()
            })
        );
        assert_eq!(authorize_agent_merge(&eval_assistant, felicia), Ok(()));
        // Non-co-author subordinate `kacey` or foreign lead `sonya` cannot merge.
        assert!(matches!(
            authorize_agent_merge(&eval_assistant, kacey),
            Err(PolicyRefusal::InsufficientRole { .. })
        ));
        assert!(matches!(
            authorize_agent_merge(&eval_assistant, sonya),
            Err(PolicyRefusal::InsufficientRole { .. })
        ));
        // Authorized reviewers (`ferris`, `jarvis`) can also merge.
        assert_eq!(authorize_agent_merge(&eval_assistant, ferris), Ok(()));
        assert_eq!(authorize_agent_merge(&eval_assistant, jarvis), Ok(()));

        // 2. Lead PR (`ferris` co-author, required_tier = LabManager)
        let eval_lead = evaluate_pr_policy(
            "ferroloop",
            &[PrCommitSummary {
                sha: "sha2".into(),
                author_login: Some("artificially-human".into()),
                message: "feat: lead work\n\nCo-authored-by: Ferris <ferris@artificialhumanity.io>"
                    .into(),
            }],
            &["crates/core/src/lib.rs".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );
        assert_eq!(
            authorize_agent_action(&eval_lead, ferris),
            Err(PolicyRefusal::SelfReviewForbidden {
                agent: "ferris".into()
            })
        );
        assert_eq!(authorize_agent_merge(&eval_lead, ferris), Ok(()));

        // 3. Protected-path / Owner-tier PR (`felicia` co-author, required_tier = Owner)
        // Once `lmcfarlin` approves on GitHub (`verify_merge_approvals`), co-author `felicia` can merge.
        let eval_protected = evaluate_pr_policy(
            "ferroloop",
            &[PrCommitSummary {
                sha: "sha3".into(),
                author_login: Some("artificially-human".into()),
                message: "ci: update\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
            }],
            &[".github/workflows/ci.yml".into()],
            &[],
            &agents,
            DEFAULT_OWNER_LOGIN,
        );
        assert_eq!(authorize_agent_merge(&eval_protected, felicia), Ok(()));
        assert_eq!(
            authorize_agent_merge(&eval_protected, ferris),
            Err(PolicyRefusal::ProtectedPathRequiresOwner {
                agent: "ferris".into(),
                files: vec![".github/workflows/ci.yml".into()],
            })
        );
    }
}
