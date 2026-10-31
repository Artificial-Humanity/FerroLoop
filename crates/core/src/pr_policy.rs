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

/// Normalise a repository path for matching: `\` is a separator, `.` and empty
/// segments vanish, `..` pops, and case is folded (a case-insensitive checkout
/// writes `.GitHub/workflows/ci.yml` to the protected directory).
fn normalize_policy_path(raw: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for seg in raw.trim().replace('\\', "/").split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_lowercase()),
        }
    }
    parts.join("/")
}

fn matches_pattern(normalized_path: &str, raw_pattern: &str) -> bool {
    let pattern = normalize_policy_path(raw_pattern);
    if pattern.is_empty() {
        return false;
    }
    let path = normalize_policy_path(normalized_path);
    if let Some(dir) = pattern.strip_suffix("/**") {
        if !dir.contains(['*', '?', '[', '{']) {
            return path == dir || path.starts_with(&format!("{dir}/"));
        }
    } else if !pattern.contains(['*', '?', '[', '{']) {
        if let Some(suffix) = pattern.strip_prefix("**/") {
            return path == suffix || path.ends_with(&format!("/{suffix}"));
        }
        if !pattern.contains('/') {
            return path == pattern || path.ends_with(&format!("/{pattern}"));
        }
        return path == pattern;
    }
    // Anything else is a real glob. A pattern that does not compile fails
    // closed: it protects its literal text rather than nothing.
    match globset::GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
    {
        Ok(g) => {
            g.compile_matcher().is_match(&path)
                || (!pattern.contains('/')
                    && globset::Glob::new(&format!("**/{pattern}"))
                        .is_ok_and(|g| g.compile_matcher().is_match(&path)))
        }
        Err(_) => path == pattern,
    }
}

/// Check whether `path` matches any default protected pattern or `extra_patterns`.
pub fn is_protected_path(path: &str, extra_patterns: &[String]) -> bool {
    let normalized = normalize_policy_path(path);
    if normalized.is_empty() {
        return false;
    }
    DEFAULT_PROTECTED_PATTERNS
        .iter()
        .any(|pat| matches_pattern(&normalized, pat))
        || extra_patterns
            .iter()
            .any(|pat| matches_pattern(&normalized, pat))
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

        let mut attributed = false;
        for trailer in trailers {
            if trailer.name_key.eq_ignore_ascii_case(owner) {
                // A trailer is text anyone can write: it names the Owner only on
                // a commit the Owner's login authored. Otherwise it attributes
                // nothing (so it cannot launder an unattributed commit).
                if is_owner_commit {
                    owner_authored_commits = true;
                    attributed = true;
                }
                continue;
            }
            match find_active_agent(agents, &trailer) {
                Some(agent) => {
                    attributed = true;
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
        if !attributed && !is_owner_commit {
            push_unique(&mut unattributed_commits, commit.sha.clone());
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
    // `>` only occurs inside JSON strings, where `\u003e` is the same character;
    // it keeps a `-->` in a field from ending the HTML comment early.
    let json = serde_json::to_string(marker)
        .expect("FlReviewMarker serializes to JSON")
        .replace('>', "\\u003e");
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

/// Check every approving review that GitHub reports on `head_sha` for integrity: the review
/// must carry a valid `fl-review` marker for an approving, active, authorized agent.
///
/// A review whose marker records an earlier head (`marker.head_sha != head_sha`) is accepted
/// without further action, but only after every other check passed. GitHub rewrites `commit_id` on older reviews when
/// `origin/main` is merged into the pull request branch, so such a review looks like it is on
/// `head_sha` while it was written for an earlier commit. Skipping it grants nothing:
/// [`verify_merge_approvals`] counts an approval only when its marker names `head_sha`, and a
/// pull request still needs one for `--require-approval`. The head comparison is the last step (it is
/// the only refusal a superseded marker escapes):
/// a marker with a non-approving verdict, or from an unknown, inactive, wrong-role, co-author
/// (self-review) or under-tier agent, is refused whichever head it names. A review with no
/// readable marker is also refused. The owner's own reviews are not checked.
pub fn verify_review_integrity(
    head_sha: &str,
    eval: &PrPolicyEvaluation,
    reviews: &[PrReviewSummary],
    agents: &[AgentRecord],
    owner_login: &str,
) -> Result<(), PolicyRefusal> {
    let owner = if owner_login.trim().is_empty() {
        DEFAULT_OWNER_LOGIN
    } else {
        owner_login.trim()
    };
    let missing = || PolicyRefusal::MissingValidApproval {
        head_sha: head_sha.to_string(),
    };
    for review in reviews {
        let on_head = review
            .commit_id
            .as_deref()
            .is_none_or(|cid| cid == head_sha);
        if !review.state.trim().eq_ignore_ascii_case("APPROVED")
            || !on_head
            || review.author_login.eq_ignore_ascii_case(owner)
        {
            continue;
        }
        let marker = extract_review_marker(&review.body).ok_or_else(missing)?;
        if marker.verdict != ReviewVerdict::Approved {
            return Err(missing());
        }
        let Some(agent) = agents.iter().find(|a| {
            a.status == AgentStatus::Active
                && a.name_key.eq_ignore_ascii_case(&marker.reviewer)
                && a.role == marker.role
        }) else {
            return Err(PolicyRefusal::UnknownOrInactiveAgent {
                agent: marker.reviewer,
            });
        };
        authorize_agent_action(eval, agent)?;
        // The head comparison is last, so it is the only refusal a superseded marker escapes.
        // Nothing follows it in this loop, so a marker that names an earlier head and passed
        // every check above needs no `continue`: it is accepted and skipped by falling through.
    }
    Ok(())
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

    // Fold oldest first whatever order the caller listed them in: review ids
    // only grow, and a stale review must not overwrite a later one.
    let mut ordered: Vec<&PrReviewSummary> = reviews.iter().collect();
    ordered.sort_by_key(|r| r.id);

    for review in ordered {
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

    fn commit(sha: &str, login: Option<&str>, message: &str) -> PrCommitSummary {
        PrCommitSummary {
            sha: sha.into(),
            author_login: login.map(str::to_string),
            message: message.into(),
        }
    }

    #[test]
    fn owner_trailer_on_a_foreign_commit_does_not_launder_it() {
        let roster = sample_roster();
        let commits = vec![
            commit(
                "good",
                Some("machine"),
                "x\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>",
            ),
            commit(
                "forged",
                Some("mallory"),
                "y\n\nCo-authored-by: Some Body <lmcfarlin@evil.example>",
            ),
        ];
        let eval = evaluate_pr_policy(
            "ferroloop",
            &commits,
            &["src/a.rs".into()],
            &[],
            &roster,
            "lmcfarlin",
        );
        assert_eq!(eval.unattributed_commits, vec!["forged".to_string()]);
        assert_eq!(eval.required_tier, RequiredTier::Owner);
    }

    #[test]
    fn protected_patterns_cover_globs_case_and_dot_segments() {
        assert!(is_protected_path("keys/server.pem", &["*.pem".into()]));
        assert!(is_protected_path("docs/a.md", &["docs/*.md".into()]));
        assert!(!is_protected_path("docs/sub/a.md", &["docs/*.md".into()]));
        assert!(is_protected_path(
            "ci/x/y.yml",
            &[".GITHUB/**".into(), "ci/**/*.yml".into()]
        ));
        assert!(is_protected_path(".GitHub/workflows/ci.yml", &[]));
        assert!(is_protected_path("./a/../.github/x.yml", &[]));
        assert!(is_protected_path(".github\\workflows\\ci.yml", &[]));
        assert!(!is_protected_path("src/main.rs", &["*.pem".into()]));
    }

    #[test]
    fn review_marker_survives_an_arrow_in_a_field() {
        let marker = FlReviewMarker {
            project: "a-->b".into(),
            reviewer: "ferris".into(),
            role: AgentRole::Lead,
            verdict: ReviewVerdict::Approved,
            head_sha: "abc".into(),
            at: At::from_unix_millis(5),
        };
        let body = format_review_body("ok", &marker);
        assert_eq!(extract_review_marker(&body), Some(marker));
    }

    #[test]
    fn approvals_fold_by_review_id_not_by_list_order() {
        let roster = sample_roster();
        let commits = vec![commit(
            "c",
            Some("machine"),
            "x\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>",
        )];
        let eval = evaluate_pr_policy(
            "ferroloop",
            &commits,
            &["src/a.rs".into()],
            &[],
            &roster,
            "lmcfarlin",
        );
        let mk = |id: u64, state: &str| PrReviewSummary {
            id,
            author_login: "lmcfarlin".into(),
            state: state.into(),
            body: String::new(),
            commit_id: Some("head".into()),
        };
        // Owner approved (id 2) after requesting changes (id 1); newest listed first.
        assert!(
            verify_merge_approvals(
                "head",
                &eval,
                &[mk(2, "APPROVED"), mk(1, "CHANGES_REQUESTED")],
                &roster,
                "lmcfarlin"
            )
            .is_ok()
        );
        // Owner changed their mind (id 2) after approving (id 1); newest listed first.
        assert!(
            verify_merge_approvals(
                "head",
                &eval,
                &[mk(2, "CHANGES_REQUESTED"), mk(1, "APPROVED")],
                &roster,
                "lmcfarlin"
            )
            .is_err()
        );
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

    // ---- `verify_review_integrity`: superseded reviews ----------------------------------

    const HEAD: &str = "head-new";
    const OLD: &str = "head-old";

    fn integrity_eval(agents: &[AgentRecord]) -> PrPolicyEvaluation {
        evaluate_pr_policy(
            "ferroloop",
            &[PrCommitSummary {
                sha: HEAD.into(),
                author_login: Some("artificially-human".into()),
                message: "feat: item\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
            }],
            &["crates/core/src/lib.rs".into()],
            &[],
            agents,
            DEFAULT_OWNER_LOGIN,
        )
    }

    /// A review by `reviewer` whose marker records `marker_head` and `verdict`, which GitHub
    /// reports as `state` on `commit_id`.
    fn marked_review(
        id: u64,
        reviewer: &AgentRecord,
        state: &str,
        verdict: ReviewVerdict,
        marker_head: &str,
        commit_id: Option<&str>,
    ) -> PrReviewSummary {
        let marker = FlReviewMarker {
            reviewer: reviewer.name_key.clone(),
            role: reviewer.role.clone(),
            project: "ferroloop".into(),
            head_sha: marker_head.into(),
            verdict,
            at: At::from_unix_millis(id * 1_000),
        };
        PrReviewSummary {
            id,
            author_login: "ferroloop-reviewer[bot]".into(),
            state: state.into(),
            body: format_review_body("review", &marker),
            commit_id: commit_id.map(str::to_string),
        }
    }

    fn missing() -> PolicyRefusal {
        PolicyRefusal::MissingValidApproval {
            head_sha: HEAD.into(),
        }
    }

    /// What `fl github pr verify-ci --require-approval` decides: the integrity check, then a
    /// valid approval on the head.
    fn verify_ci_requiring_approval(
        eval: &PrPolicyEvaluation,
        reviews: &[PrReviewSummary],
        agents: &[AgentRecord],
    ) -> Result<String, PolicyRefusal> {
        verify_review_integrity(HEAD, eval, reviews, agents, DEFAULT_OWNER_LOGIN)?;
        verify_merge_approvals(HEAD, eval, reviews, agents, DEFAULT_OWNER_LOGIN)
    }

    #[test]
    fn integrity_skips_a_superseded_review_but_a_head_approval_is_still_required() {
        let agents = sample_roster();
        let (ferris, jarvis) = (&agents[1], &agents[0]);
        let eval = integrity_eval(&agents);

        // Merging `origin/main` made GitHub report the round-1 reviews on the new head, while
        // their markers still name the old one. Round 2 re-approves on the new head.
        let round1_approval = marked_review(
            1,
            ferris,
            "APPROVED",
            ReviewVerdict::Approved,
            OLD,
            Some(HEAD),
        );
        let round1_changes = marked_review(
            2,
            ferris,
            "CHANGES_REQUESTED",
            ReviewVerdict::ChangesRequested,
            OLD,
            Some(HEAD),
        );
        let round2_approval = marked_review(
            3,
            ferris,
            "APPROVED",
            ReviewVerdict::Approved,
            HEAD,
            Some(HEAD),
        );
        let all = [round1_approval.clone(), round1_changes, round2_approval];
        assert_eq!(
            verify_review_integrity(HEAD, &eval, &all, &agents, DEFAULT_OWNER_LOGIN),
            Ok(())
        );
        assert_eq!(
            verify_ci_requiring_approval(&eval, &all, &agents),
            Ok("ferris".to_string())
        );

        // A superseded approval alone is skipped by the integrity check, and it is not an
        // approval of the head: `--require-approval` still fails.
        let stale_only = [round1_approval];
        assert_eq!(
            verify_review_integrity(HEAD, &eval, &stale_only, &agents, DEFAULT_OWNER_LOGIN),
            Ok(())
        );
        assert_eq!(
            verify_ci_requiring_approval(&eval, &stale_only, &agents),
            Err(missing())
        );

        // Another reviewer's approval of the head does not rescue a stale approval's author:
        // it is the head approval that counts.
        let stale_plus_other = [
            stale_only[0].clone(),
            marked_review(
                4,
                jarvis,
                "APPROVED",
                ReviewVerdict::Approved,
                HEAD,
                Some(HEAD),
            ),
        ];
        assert_eq!(
            verify_ci_requiring_approval(&eval, &stale_plus_other, &agents),
            Ok("jarvis".to_string())
        );
    }

    #[test]
    fn integrity_still_fails_on_a_changes_requested_review_for_the_head() {
        let agents = sample_roster();
        let ferris = &agents[1];
        let eval = integrity_eval(&agents);

        // A head approval followed by a head-matching CHANGES_REQUESTED blocks the pull
        // request, with or without a skipped superseded review before them.
        let old_approval = marked_review(
            1,
            ferris,
            "APPROVED",
            ReviewVerdict::Approved,
            OLD,
            Some(HEAD),
        );
        let head_approval = marked_review(
            2,
            ferris,
            "APPROVED",
            ReviewVerdict::Approved,
            HEAD,
            Some(HEAD),
        );
        let head_changes = marked_review(
            3,
            ferris,
            "CHANGES_REQUESTED",
            ReviewVerdict::ChangesRequested,
            HEAD,
            Some(HEAD),
        );
        assert_eq!(
            verify_ci_requiring_approval(
                &eval,
                &[old_approval, head_approval.clone(), head_changes],
                &agents
            ),
            Err(missing())
        );

        // GitHub's `APPROVED` with a marker for the head that says `changes_requested` is a
        // contradiction, and is refused by the integrity check itself.
        let contradictory = marked_review(
            4,
            ferris,
            "APPROVED",
            ReviewVerdict::ChangesRequested,
            HEAD,
            Some(HEAD),
        );
        assert_eq!(
            verify_review_integrity(HEAD, &eval, &[contradictory], &agents, DEFAULT_OWNER_LOGIN),
            Err(missing())
        );
    }

    #[test]
    fn integrity_handles_missing_forged_and_unauthorized_markers_as_before() {
        let agents = sample_roster();
        let (jarvis, ferris, felicia) = (&agents[0], &agents[1], &agents[2]);
        let eval = integrity_eval(&agents);
        let check = |reviews: &[PrReviewSummary]| {
            verify_review_integrity(HEAD, &eval, reviews, &agents, DEFAULT_OWNER_LOGIN)
        };
        let approval = |body: &str, author: &str, commit_id: Option<&str>| PrReviewSummary {
            id: 9,
            author_login: author.into(),
            state: "APPROVED".into(),
            body: body.into(),
            commit_id: commit_id.map(str::to_string),
        };

        // No marker, and a marker that does not parse: refused, including after a skipped one.
        let skipped = marked_review(
            1,
            ferris,
            "APPROVED",
            ReviewVerdict::Approved,
            OLD,
            Some(HEAD),
        );
        for body in [
            "approved, trust me",
            "<!-- fl-review: {not json} -->",
            "<!-- fl-review: -->",
        ] {
            let forged = approval(body, "someone", Some(HEAD));
            assert_eq!(
                check(std::slice::from_ref(&forged)),
                Err(missing()),
                "{body}"
            );
            assert_eq!(check(&[skipped.clone(), forged]), Err(missing()), "{body}");
        }
        // A forged approval with no commit id is on the head too.
        assert_eq!(
            check(&[approval("no marker", "someone", None)]),
            Err(missing())
        );

        // A marker for the head from an agent that is not on the roster, or has the wrong role.
        let mut ghost = marked_review(
            2,
            ferris,
            "APPROVED",
            ReviewVerdict::Approved,
            HEAD,
            Some(HEAD),
        );
        ghost.body = ghost.body.replace("\"ferris\"", "\"ghost\"");
        assert_eq!(
            check(&[ghost]),
            Err(PolicyRefusal::UnknownOrInactiveAgent {
                agent: "ghost".into()
            })
        );
        let mut wrong_role = marked_review(
            3,
            ferris,
            "APPROVED",
            ReviewVerdict::Approved,
            HEAD,
            Some(HEAD),
        );
        wrong_role.body = wrong_role.body.replace("\"lead\"", "\"lab_manager\"");
        assert_eq!(
            check(&[wrong_role]),
            Err(PolicyRefusal::UnknownOrInactiveAgent {
                agent: "ferris".into()
            })
        );

        // A co-author approving the pull request, and an assistant below the required tier.
        assert_eq!(
            check(&[marked_review(
                4,
                felicia,
                "APPROVED",
                ReviewVerdict::Approved,
                HEAD,
                Some(HEAD)
            )]),
            Err(PolicyRefusal::SelfReviewForbidden {
                agent: "felicia".into()
            })
        );

        // The owner's own approval is not checked, a review on another commit is not on the
        // head, and a valid approval passes.
        assert_eq!(
            check(&[approval("no marker", DEFAULT_OWNER_LOGIN, Some(HEAD))]),
            Ok(())
        );
        assert_eq!(
            check(&[approval("no marker", "someone", Some("other-sha"))]),
            Ok(())
        );
        assert_eq!(
            check(&[marked_review(
                5,
                jarvis,
                "APPROVED",
                ReviewVerdict::Approved,
                HEAD,
                Some(HEAD)
            )]),
            Ok(())
        );
    }

    #[test]
    fn integrity_refuses_a_superseded_marker_that_fails_any_other_check() {
        let mut agents = sample_roster();
        let (ferris, felicia, kacey) = (agents[1].clone(), agents[2].clone(), agents[3].clone());
        let eval = integrity_eval(&agents);
        // A marker naming the old head, which GitHub reports on the new one.
        let stale = |reviewer: &AgentRecord, verdict: ReviewVerdict| {
            let state = if verdict == ReviewVerdict::Approved {
                "APPROVED"
            } else {
                "CHANGES_REQUESTED"
            };
            marked_review(1, reviewer, state, verdict, OLD, Some(HEAD))
        };
        let check = |reviews: &[PrReviewSummary], agents: &[AgentRecord]| {
            verify_review_integrity(HEAD, &eval, reviews, agents, DEFAULT_OWNER_LOGIN)
        };

        // An agent that is not on the roster.
        let mut ghost = stale(&ferris, ReviewVerdict::Approved);
        ghost.body = ghost.body.replace("\"ferris\"", "\"ghost\"");
        assert_eq!(
            check(&[ghost], &agents),
            Err(PolicyRefusal::UnknownOrInactiveAgent {
                agent: "ghost".into()
            })
        );

        // The wrong role for the named agent.
        let mut wrong_role = stale(&ferris, ReviewVerdict::Approved);
        wrong_role.body = wrong_role.body.replace("\"lead\"", "\"lab_manager\"");
        assert_eq!(
            check(&[wrong_role], &agents),
            Err(PolicyRefusal::UnknownOrInactiveAgent {
                agent: "ferris".into()
            })
        );

        // A co-author reviewing their own pull request.
        assert_eq!(
            check(&[stale(&felicia, ReviewVerdict::Approved)], &agents),
            Err(PolicyRefusal::SelfReviewForbidden {
                agent: "felicia".into()
            })
        );

        // An agent below the required tier.
        assert!(
            matches!(
                check(&[stale(&kacey, ReviewVerdict::Approved)], &agents),
                Err(PolicyRefusal::InsufficientRole { .. })
            ),
            "{:?}",
            check(&[stale(&kacey, ReviewVerdict::Approved)], &agents)
        );

        // A non-approving verdict, even though GitHub reports `APPROVED`.
        let contradictory = marked_review(
            1,
            &ferris,
            "APPROVED",
            ReviewVerdict::ChangesRequested,
            OLD,
            Some(HEAD),
        );
        assert_eq!(check(&[contradictory], &agents), Err(missing()));

        // A deactivated agent.
        agents[1].status = AgentStatus::Retired;
        assert_eq!(
            check(&[stale(&ferris, ReviewVerdict::Approved)], &agents),
            Err(PolicyRefusal::UnknownOrInactiveAgent {
                agent: "ferris".into()
            })
        );
    }
}
