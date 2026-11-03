//! Side-effect-free workspace policy and planning models.
//!
//! Inputs describing ownership, isolation, content and approvals must be supplied by a trusted
//! coordinator. This module does not authenticate those inputs or execute any operation.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    #[error("invalid workspace policy: {0}")]
    Policy(String),
    #[error("invalid workspace path: {0}")]
    Path(String),
    #[error("workspace content evidence missing or invalid: {0}")]
    Content(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeMode {
    Create,
    Reuse,
    Shared,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeardownPolicy {
    Keep,
    RemoveCleanOwned,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookExecution {
    Automatic,
    OwnerManual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeConfig {
    pub mode: WorktreeMode,
    pub base_ref: String,
    pub on_teardown: TeardownPolicy,
    #[serde(default)]
    pub root: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSpec {
    pub id: String,
    pub argv: Vec<String>,
    pub execution: HookExecution,
    #[serde(default)]
    pub needs_network: bool,
    #[serde(default)]
    pub required: bool,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_hook_output_limit")]
    pub output_limit_bytes: u64,
    #[serde(default)]
    pub environment_names: Vec<String>,
    #[serde(default)]
    pub credential_bundle: Option<String>,
    /// Additional scripts or executables used by an interpreter or wrapper.
    #[serde(default)]
    pub content_paths: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    pub name: String,
    pub argv: Vec<String>,
    pub adapter: String,
    pub ready_protocol: String,
    pub ready_timeout_secs: u64,
    #[serde(default)]
    pub environment_names: Vec<String>,
    #[serde(default)]
    pub credential_bundle: Option<String>,
    #[serde(default)]
    pub content_paths: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortRange {
    pub first: u16,
    pub last: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePolicy {
    pub schema_version: u32,
    pub worktree: WorktreeConfig,
    #[serde(default)]
    pub post_create: Vec<HookSpec>,
    #[serde(default)]
    pub pre_teardown: Vec<HookSpec>,
    #[serde(default)]
    pub services: Vec<ServiceSpec>,
    #[serde(default)]
    pub ports: Option<PortRange>,
}
fn valid_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}
fn valid_id(s: &str) -> bool {
    s.len() <= 128
        && !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && s != "."
        && s != ".."
}
fn valid_argv(a: &[String]) -> bool {
    !a.is_empty()
        && a.len() <= 128
        && a.iter().all(|s| valid_text(s, 4096))
        && !a[0].starts_with('-')
}
fn default_hook_timeout() -> u64 {
    120
}
fn default_hook_output_limit() -> u64 {
    65_536
}

fn valid_env(a: &[String]) -> bool {
    a.len() <= 64
        && a.iter().all(|s| {
            matches!(
                s.as_str(),
                "PATH"
                    | "TMPDIR"
                    | "TMP"
                    | "TEMP"
                    | "LANG"
                    | "LC_ALL"
                    | "TERM"
                    | "FL_PROJECT_ID"
                    | "FL_WORKTREE_ID"
                    | "FL_OPERATION_ID"
                    | "FL_SESSION_INCARNATION"
            )
        })
        && a.iter().collect::<BTreeSet<_>>().len() == a.len()
}
impl WorkspacePolicy {
    pub fn parse(input: &str) -> Result<Self, WorkspaceError> {
        if input.len() > 256 * 1024 {
            return Err(WorkspaceError::Policy("document too large".into()));
        }
        let policy: Self =
            toml::from_str(input).map_err(|e| WorkspaceError::Policy(e.to_string()))?;
        policy.validate()?;
        Ok(policy)
    }
    pub fn validate(&self) -> Result<(), WorkspaceError> {
        let invalid = || {
            WorkspaceError::Policy("invalid version, identifier, ref, argument, or limit".into())
        };
        if self.schema_version != 1
            || !valid_text(&self.worktree.base_ref, 1024)
            || self.worktree.base_ref.starts_with('-')
            || self.worktree.base_ref.contains(' ')
            || self.worktree.base_ref.contains("..")
            || self.worktree.base_ref.contains("@{")
            || self.worktree.base_ref.contains('\\')
            || self
                .worktree
                .base_ref
                .bytes()
                .any(|c| matches!(c, b'~' | b'^' | b':' | b'?' | b'*' | b'['))
        {
            return Err(invalid());
        }
        if self
            .worktree
            .root
            .as_ref()
            .is_some_and(|r| !valid_text(r, 4096))
            || self.post_create.len() + self.pre_teardown.len() > 128
            || self.services.len() > 64
        {
            return Err(invalid());
        }
        let mut ids = BTreeSet::new();
        for h in self.post_create.iter().chain(&self.pre_teardown) {
            if !valid_id(&h.id)
                || !ids.insert(&h.id)
                || !valid_argv(&h.argv)
                || !valid_env(&h.environment_names)
                || h.timeout_secs == 0
                || h.timeout_secs > 600
                || h.output_limit_bytes == 0
                || h.output_limit_bytes > 65_536
                || h.content_paths.len() > 128
                || h.content_paths.iter().any(|p| !valid_text(p, 4096))
                || h.credential_bundle.as_ref().is_some_and(|b| !valid_id(b))
            {
                return Err(invalid());
            }
        }
        let mut services = BTreeSet::new();
        for s in &self.services {
            if !valid_id(&s.name)
                || !services.insert(&s.name)
                || !valid_argv(&s.argv)
                || !valid_text(&s.adapter, 128)
                || !valid_text(&s.ready_protocol, 128)
                || s.ready_timeout_secs == 0
                || s.ready_timeout_secs > 86400
                || !valid_env(&s.environment_names)
                || s.credential_bundle.as_ref().is_some_and(|b| !valid_id(b))
                || s.content_paths.len() > 128
                || s.content_paths.iter().any(|p| !valid_text(p, 4096))
            {
                return Err(invalid());
            }
        }
        if self
            .ports
            .as_ref()
            .is_some_and(|p| p.first == 0 || p.first > p.last)
        {
            return Err(invalid());
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = r#"
schema_version = 1
[worktree]
mode = "create"
base_ref = "refs/heads/main"
on_teardown = "keep"
[[post_create]]
id = "deps"
argv = ["./bootstrap"]
execution = "owner_manual"
needs_network = true
required = true
timeout_secs = 30
output_limit_bytes = 65536
[ports]
first = 20000
last = 29999
"#;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        let common = repo.join(".git");
        std::fs::create_dir_all(&common).unwrap();
        (dir, repo, common)
    }

    fn paths(
        dir: &std::path::Path,
        repo: &std::path::Path,
        common: &std::path::Path,
    ) -> ResolvedWorkspacePaths {
        resolve_worktree_paths(
            "repo123",
            "inc-1",
            repo,
            common,
            &[repo.into()],
            None,
            Some(&dir.join("data")),
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn external_parent_fallback_and_overlap_are_safe() {
        let (dir, repo, common) = fixture();
        let urn = resolve_worktree_paths(
            "urn:repo:1",
            "urn:uuid:1",
            &repo,
            &common,
            std::slice::from_ref(&repo),
            None,
            None,
            Some(dir.path()),
            None,
        )
        .unwrap();
        assert_eq!(urn.parent.file_name().unwrap(), "urn%3Arepo%3A1");
        assert_eq!(urn.child.file_name().unwrap(), "urn%3Auuid%3A1");
        let out = paths(dir.path(), &repo, &common);
        assert_eq!(out.parent, dir.path().join("data/fl/worktrees/repo123"));
        assert_eq!(out.child, out.parent.join("inc-1"));
        let fallback = resolve_worktree_paths(
            "r",
            "i",
            &repo,
            &common,
            std::slice::from_ref(&repo),
            None,
            Some(std::path::Path::new("relative")),
            Some(dir.path()),
            None,
        )
        .unwrap();
        assert_eq!(
            fallback.parent,
            dir.path().join(".local/share/fl/worktrees/r")
        );
        assert!(
            resolve_worktree_paths(
                "r",
                "i",
                &repo,
                &common,
                std::slice::from_ref(&repo),
                None,
                Some(&repo),
                None,
                None
            )
            .is_err()
        );
        assert!(
            resolve_worktree_paths(
                "../r",
                "i",
                &repo,
                &common,
                std::slice::from_ref(&repo),
                None,
                None,
                Some(dir.path()),
                None
            )
            .is_err()
        );
        assert!(
            resolve_worktree_paths(
                "r",
                "../i",
                &repo,
                &common,
                std::slice::from_ref(&repo),
                None,
                None,
                Some(dir.path()),
                None
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_default_parent_is_refused() {
        use std::os::unix::fs::symlink;
        let (dir, repo, common) = fixture();
        let link = dir.path().join("link");
        symlink(&repo, &link).unwrap();
        assert!(
            resolve_worktree_paths(
                "r",
                "i",
                &repo,
                &common,
                std::slice::from_ref(&repo),
                None,
                Some(&link),
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn nested_root_needs_explicit_owner_isolation_and_exclusion_plan() {
        let (_dir, repo, common) = fixture();
        let isolated = InRepoIsolation {
            owner_opt_in: true,
            exclude_from_parent_discovery: true,
            exclude_from_watchers: true,
            exclude_from_indexers: true,
            exclude_from_builds: true,
        };
        assert!(
            resolve_worktree_paths(
                "r",
                "i",
                &repo,
                &common,
                std::slice::from_ref(&repo),
                Some(std::path::Path::new(".fl/worktrees")),
                None,
                None,
                None
            )
            .is_err()
        );
        let mut incomplete = isolated.clone();
        incomplete.exclude_from_builds = false;
        assert!(
            resolve_worktree_paths(
                "r",
                "i",
                &repo,
                &common,
                std::slice::from_ref(&repo),
                Some(std::path::Path::new(".fl/worktrees")),
                None,
                None,
                Some(incomplete)
            )
            .is_err()
        );
        let out = resolve_worktree_paths(
            "r",
            "i",
            &repo,
            &common,
            std::slice::from_ref(&repo),
            Some(std::path::Path::new(".fl/worktrees")),
            None,
            None,
            Some(isolated),
        )
        .unwrap();
        let entry = out.exclusion.unwrap();
        assert_eq!(entry.info_exclude, common.join("info/exclude"));
        assert_eq!(entry.exact_line, "/.fl/worktrees/");
        assert!(entry.requires_ownership_receipt);
        assert!(!out.warnings.is_empty());
    }

    #[test]
    fn digest_covers_content_policy_paths_and_base() {
        let (dir, repo, common) = fixture();
        let mut policy = WorkspacePolicy::parse(POLICY).unwrap();
        let paths = paths(dir.path(), &repo, &common);
        let base = "a".repeat(40);
        let mut content = vec![ContentEvidence {
            path: "./bootstrap".into(),
            sha256: executable_digest(b"original"),
        }];
        let digest = workspace_policy_digest(&policy, &paths, &base, &content).unwrap();
        assert_eq!(
            digest,
            workspace_policy_digest(&policy, &paths, &base, &content).unwrap()
        );
        content[0].sha256 = executable_digest(b"changed");
        assert_ne!(
            digest,
            workspace_policy_digest(&policy, &paths, &base, &content).unwrap()
        );
        content.clear();
        assert!(workspace_policy_digest(&policy, &paths, &base, &content).is_err());
        content.push(ContentEvidence {
            path: "./bootstrap".into(),
            sha256: executable_digest(b"original"),
        });
        policy.post_create[0].argv = vec!["python3".into(), "tools/task.py".into()];
        let interpreter = vec![ContentEvidence {
            path: "python3".into(),
            sha256: executable_digest(b"interpreter"),
        }];
        assert!(workspace_policy_digest(&policy, &paths, &base, &interpreter).is_err());
        let mut interpreter_and_script = interpreter;
        interpreter_and_script.push(ContentEvidence {
            path: "tools/task.py".into(),
            sha256: executable_digest(b"script"),
        });
        assert!(workspace_policy_digest(&policy, &paths, &base, &interpreter_and_script).is_ok());
        policy.post_create[0].argv = vec![
            "python3".into(),
            "-W".into(),
            "ignore".into(),
            "tools/task.py".into(),
        ];
        assert!(workspace_policy_digest(&policy, &paths, &base, &interpreter_and_script).is_err());
        policy.post_create[0].argv = vec!["./bootstrap".into()];
        policy.post_create[0].timeout_secs += 1;
        assert_ne!(
            digest,
            workspace_policy_digest(&policy, &paths, &base, &content).unwrap()
        );
    }

    #[test]
    fn distinct_hook_refusals_and_required_manual_blocking() {
        let (dir, repo, common) = fixture();
        let mut policy = WorkspacePolicy::parse(POLICY).unwrap();
        let path = paths(dir.path(), &repo, &common);
        let content = vec![ContentEvidence {
            path: "./bootstrap".into(),
            sha256: executable_digest(b"script"),
        }];
        let base = "a".repeat(40);
        let digest = workspace_policy_digest(&policy, &path, &base, &content).unwrap();
        let approval = WorkspaceApproval {
            policy_digest: digest,
            base_oid: base.clone(),
        };
        let success = Vec::new();
        let input = |approval, owner_invoked, automatic_profile_proven| PlanInputs {
            approval,
            owner_invoked,
            automatic_profile_proven,
            operation_id: "op-1",
            successful_manual_hooks: &success,
        };
        let unapproved = plan_workspace(
            &policy,
            path.clone(),
            &base,
            &content,
            input(None, true, true),
        )
        .unwrap();
        assert_eq!(
            unapproved.hooks[0].refusal,
            Some(PlanRefusal::UnapprovedHook)
        );
        let multiple = plan_workspace(
            &policy,
            path.clone(),
            &base,
            &content,
            input(None, false, false),
        )
        .unwrap();
        assert!(
            multiple.hooks[0]
                .refusals
                .contains(&PlanRefusal::HookNetworkRequiresOwner)
        );
        assert!(
            multiple.hooks[0]
                .refusals
                .contains(&PlanRefusal::UnapprovedHook)
        );
        let automatic = plan_workspace(
            &policy,
            path.clone(),
            &base,
            &content,
            input(Some(&approval), false, true),
        )
        .unwrap();
        assert_eq!(
            automatic.hooks[0].refusal,
            Some(PlanRefusal::HookNetworkRequiresOwner)
        );
        assert!(automatic.preparation_blocked);
        let manual = plan_workspace(
            &policy,
            path.clone(),
            &base,
            &content,
            input(Some(&approval), true, true),
        )
        .unwrap();
        assert!(manual.hooks[0].refusal.is_none());
        assert!(
            manual.preparation_blocked,
            "an invocation plan is not a successful receipt"
        );
        policy.post_create[0].execution = HookExecution::Automatic;
        policy.post_create[0].needs_network = false;
        let digest = workspace_policy_digest(&policy, &path, &base, &content).unwrap();
        let approval = WorkspaceApproval {
            policy_digest: digest,
            base_oid: base.clone(),
        };
        let unsupported = plan_workspace(
            &policy,
            path,
            &base,
            &content,
            input(Some(&approval), false, false),
        )
        .unwrap();
        assert_eq!(
            unsupported.hooks[0].refusal,
            Some(PlanRefusal::HookProfileUnsupported)
        );
    }

    #[test]
    fn required_manual_receipt_unblocks_later_automatic_plan() {
        let (dir, repo, common) = fixture();
        let policy = WorkspacePolicy::parse(POLICY).unwrap();
        let path = paths(dir.path(), &repo, &common);
        let base = "a".repeat(40);
        let contents = vec![ContentEvidence {
            path: "./bootstrap".into(),
            sha256: executable_digest(b"script"),
        }];
        let approval = WorkspaceApproval {
            policy_digest: workspace_policy_digest(&policy, &path, &base, &contents).unwrap(),
            base_oid: base.clone(),
        };
        let success = vec![HookRun {
            id: "run-1".into(),
            operation_id: "op-1".into(),
            phase: "post_create".into(),
            hook_id: "deps".into(),
            approved_digest: approval.policy_digest.clone(),
            invocation_mode: HookExecution::OwnerManual,
            execution_profile: "manual".into(),
            owner_invocation_receipt: Some("owner-receipt".into()),
            execution_id: "exec-1".into(),
            state: HookRunState::Succeeded,
            exit_status: Some(0),
            started_at: None,
            ended_at: None,
            output_excerpt_ref: None,
            timeout_reason: None,
        }];
        let plan = plan_workspace(
            &policy,
            path,
            &base,
            &contents,
            PlanInputs {
                approval: Some(&approval),
                owner_invoked: false,
                automatic_profile_proven: true,
                operation_id: "op-1",
                successful_manual_hooks: &success,
            },
        )
        .unwrap();
        assert!(!plan.preparation_blocked);
    }

    #[test]
    fn manual_receipt_must_match_operation_phase_hook_digest_and_authority() {
        let (dir, repo, common) = fixture();
        let policy = WorkspacePolicy::parse(POLICY).unwrap();
        let path = paths(dir.path(), &repo, &common);
        let base = "a".repeat(40);
        let contents = vec![ContentEvidence {
            path: "./bootstrap".into(),
            sha256: executable_digest(b"script"),
        }];
        let approval = WorkspaceApproval {
            policy_digest: workspace_policy_digest(&policy, &path, &base, &contents).unwrap(),
            base_oid: base.clone(),
        };
        let valid = HookRun {
            id: "run".into(),
            operation_id: "op".into(),
            phase: "post_create".into(),
            hook_id: "deps".into(),
            approved_digest: approval.policy_digest.clone(),
            invocation_mode: HookExecution::OwnerManual,
            execution_profile: "manual".into(),
            owner_invocation_receipt: Some("receipt".into()),
            execution_id: "e".into(),
            state: HookRunState::Succeeded,
            exit_status: Some(0),
            started_at: None,
            ended_at: None,
            output_excerpt_ref: None,
            timeout_reason: None,
        };
        let check = |run: HookRun| {
            plan_workspace(
                &policy,
                path.clone(),
                &base,
                &contents,
                PlanInputs {
                    approval: Some(&approval),
                    owner_invoked: false,
                    automatic_profile_proven: true,
                    operation_id: "op",
                    successful_manual_hooks: &[run],
                },
            )
            .unwrap()
        };
        let plan = check(valid.clone());
        assert!(!plan.preparation_blocked);
        assert!(
            plan.hooks[0]
                .refusals
                .contains(&PlanRefusal::OwnerManualRequiresOwner)
        );
        assert!(
            !plan.hooks[0]
                .refusals
                .contains(&PlanRefusal::PreparationBlocked)
        );
        let mut invalid = Vec::new();
        let mut r = valid.clone();
        r.operation_id = "other".into();
        invalid.push(r);
        let mut r = valid.clone();
        r.phase = "pre_teardown".into();
        invalid.push(r);
        let mut r = valid.clone();
        r.hook_id = "other".into();
        invalid.push(r);
        let mut r = valid.clone();
        r.approved_digest = "stale".into();
        invalid.push(r);
        let mut r = valid.clone();
        r.owner_invocation_receipt = None;
        invalid.push(r);
        let mut r = valid.clone();
        r.owner_invocation_receipt = Some("".into());
        invalid.push(r);
        let mut r = valid.clone();
        r.invocation_mode = HookExecution::Automatic;
        invalid.push(r);
        let mut r = valid.clone();
        r.state = HookRunState::Failed;
        invalid.push(r);
        let mut r = valid.clone();
        r.exit_status = None;
        invalid.push(r);
        for r in invalid {
            assert!(check(r).preparation_blocked);
        }
    }

    #[test]
    fn default_and_maximum_hook_limits_are_validated() {
        let without = POLICY
            .replace("timeout_secs = 30\n", "")
            .replace("output_limit_bytes = 65536\n", "");
        let mut policy = WorkspacePolicy::parse(&without).unwrap();
        assert_eq!(policy.post_create[0].timeout_secs, 120);
        assert_eq!(policy.post_create[0].output_limit_bytes, 65_536);
        policy.post_create[0].timeout_secs = 600;
        assert!(policy.validate().is_ok());
        policy.post_create[0].timeout_secs = 601;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn opaque_env_wrapper_cannot_be_approved_without_its_chain() {
        let (dir, repo, common) = fixture();
        let mut policy = WorkspacePolicy::parse(POLICY).unwrap();
        policy.post_create[0].argv =
            vec!["/usr/bin/env".into(), "python3".into(), "./hook.py".into()];
        let contents = vec![ContentEvidence {
            path: "/usr/bin/env".into(),
            sha256: executable_digest(b"env"),
        }];
        assert!(
            workspace_policy_digest(
                &policy,
                &paths(dir.path(), &repo, &common),
                &"a".repeat(40),
                &contents
            )
            .is_err()
        );
    }

    #[test]
    fn hook_limits_and_credential_environment_are_refused() {
        let policy = WorkspacePolicy::parse(POLICY).unwrap();
        let mut bad = policy.clone();
        bad.post_create[0].timeout_secs = 601;
        assert!(bad.validate().is_err());
        bad = policy.clone();
        bad.post_create[0].output_limit_bytes = 65537;
        assert!(bad.validate().is_err());
        for secret in ["AWS_SECRET_ACCESS_KEY", "ANTHROPIC_API_KEY"] {
            bad = policy.clone();
            bad.post_create[0].environment_names = vec![secret.into()];
            assert!(bad.validate().is_err(), "{secret}");
        }
    }

    #[test]
    fn service_only_policy_requires_approval() {
        let (dir, repo, common) = fixture();
        let mut policy = WorkspacePolicy::parse(POLICY).unwrap();
        policy.post_create.clear();
        policy.services.push(ServiceSpec {
            name: "web".into(),
            argv: vec!["./server".into()],
            adapter: "vite-node-v1".into(),
            ready_protocol: "fl-dev-ready-v1".into(),
            ready_timeout_secs: 15,
            environment_names: vec![],
            credential_bundle: None,
            content_paths: vec![],
        });
        let contents = vec![ContentEvidence {
            path: "./server".into(),
            sha256: executable_digest(b"server"),
        }];
        let success = Vec::new();
        let plan = plan_workspace(
            &policy,
            paths(dir.path(), &repo, &common),
            &"a".repeat(40),
            &contents,
            PlanInputs {
                approval: None,
                owner_invoked: false,
                automatic_profile_proven: true,
                operation_id: "op-1",
                successful_manual_hooks: &success,
            },
        )
        .unwrap();
        assert!(plan.preparation_blocked);
        assert_eq!(plan.policy_refusal, Some(PlanRefusal::UnapprovedPolicy));
    }

    #[test]
    fn parses_v1_and_rejects_unknown_fields() {
        let policy = WorkspacePolicy::parse(POLICY).unwrap();
        assert_eq!(policy.post_create[0].id, "deps");
        assert!(
            WorkspacePolicy::parse(&POLICY.replace("schema_version = 1", "schema_version = 2"))
                .is_err()
        );
        assert!(
            WorkspacePolicy::parse(
                &POLICY.replace("schema_version = 1", "schema_version = 1\nunknown = true")
            )
            .is_err()
        );
    }
}

/// The coordinator confirms these isolation mechanisms before an in-repository root is approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InRepoIsolation {
    pub owner_opt_in: bool,
    pub exclude_from_parent_discovery: bool,
    pub exclude_from_watchers: bool,
    pub exclude_from_indexers: bool,
    pub exclude_from_builds: bool,
}
/// Proposed `info/exclude` line and its common-directory destination. This is not a receipt of a write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusionPlan {
    pub info_exclude: PathBuf,
    pub exact_line: String,
    pub requires_ownership_receipt: bool,
}
/// Durable evidence of a later Owner-approved exclusion write; planning never creates one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusionReceipt {
    pub receipt_id: String,
    pub info_exclude: PathBuf,
    pub exact_line: String,
    pub installed_content_digest: String,
    pub owner_approval_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedWorkspacePaths {
    pub parent: PathBuf,
    pub child: PathBuf,
    pub isolation: Option<InRepoIsolation>,
    pub exclusion: Option<ExclusionPlan>,
    pub warnings: Vec<String>,
}
/// Resolve an absolute path through existing ancestors, rejecting symbolic links and traversal.
fn resolve_path(path: &Path) -> Result<PathBuf, WorkspaceError> {
    let invalid = || WorkspaceError::Path(format!("unsafe path: {}", path.display()));
    if !path.is_absolute() || path.to_str().is_none_or(|s| !valid_text(s, 4096)) {
        return Err(invalid());
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => out.push(component.as_os_str()),
            Component::Normal(c) => {
                out.push(c);
                match std::fs::symlink_metadata(&out) {
                    Ok(metadata) if metadata.file_type().is_symlink() => return Err(invalid()),
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(invalid()),
                }
            }
            _ => return Err(invalid()),
        }
    }
    Ok(out)
}
fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
fn exclude_line(relative: &Path) -> Result<String, WorkspaceError> {
    let mut result = String::from("/");
    for (i, c) in relative.components().enumerate() {
        let Component::Normal(c) = c else {
            return Err(WorkspaceError::Path("invalid exclusion".into()));
        };
        let part = c
            .to_str()
            .ok_or_else(|| WorkspaceError::Path("non-UTF8 exclusion".into()))?;
        if i > 0 {
            result.push('/');
        }
        for ch in part.chars() {
            if matches!(ch, '*' | '?' | '[' | ']' | '\\' | '#' | '!' | ' ') {
                result.push('\\');
            }
            result.push(ch);
        }
    }
    result.push('/');
    Ok(result)
}
/// Percent-encode opaque identities as single portable path components without collisions.
fn identity_component(id: &str) -> Result<String, WorkspaceError> {
    if !valid_text(id, 128) || id == "." || id == ".." || id.contains(['/', '\\']) {
        return Err(WorkspaceError::Path(
            "invalid repository or incarnation id".into(),
        ));
    }
    let mut out = String::new();
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(char::from(b));
        } else {
            use std::fmt::Write;
            write!(&mut out, "%{b:02X}").expect("String formatting is infallible");
        }
    }
    if out.len() > 255 {
        return Err(WorkspaceError::Path("encoded identity too long".into()));
    }
    Ok(out)
}
/// Resolve a planned parent/child. All repository paths and the isolation attestation are trusted
/// inputs; callers must enumerate all checkouts, verify the ownership of any existing path and
/// recheck paths at the eventual side-effect boundary.
#[allow(clippy::too_many_arguments)]
pub fn resolve_worktree_paths(
    repo_id: &str,
    incarnation_id: &str,
    project_root: &Path,
    git_common_dir: &Path,
    checkouts: &[PathBuf],
    configured_root: Option<&Path>,
    xdg_data_home: Option<&Path>,
    home: Option<&Path>,
    isolation: Option<InRepoIsolation>,
) -> Result<ResolvedWorkspacePaths, WorkspaceError> {
    let err = |s: &str| WorkspaceError::Path(s.into());
    let repo_component = identity_component(repo_id)?;
    let incarnation_component = identity_component(incarnation_id)?;
    let project = resolve_path(project_root)?;
    let common = resolve_path(git_common_dir)?;
    let checkouts = checkouts
        .iter()
        .map(|p| resolve_path(p))
        .collect::<Result<Vec<_>, _>>()?;
    if !checkouts.iter().any(|p| p == &project) {
        return Err(err("registered project root not in checkout list"));
    }
    let explicit = configured_root.is_some();
    let parent = if let Some(root) = configured_root {
        resolve_path(&if root.is_absolute() {
            root.to_path_buf()
        } else {
            project.join(root)
        })?
    } else {
        let base = match xdg_data_home
            .filter(|p| p.is_absolute() && p.to_str().is_some_and(|s| valid_text(s, 4096)))
        {
            Some(xdg) => xdg.to_path_buf(),
            None => home
                .filter(|p| p.is_absolute())
                .ok_or_else(|| err("no absolute HOME"))?
                .join(".local/share"),
        };
        resolve_path(&base.join("fl/worktrees").join(repo_component))?
    };
    let nested = parent.starts_with(&project) && parent != project;
    let (isolation, exclusion, warnings) = if nested {
        if !explicit
            || overlap(&parent, &common)
            || checkouts
                .iter()
                .any(|p| p != &project && overlap(&parent, p))
        {
            return Err(err("in-repository root overlaps another protected path"));
        }
        let iso = isolation
            .ok_or_else(|| err("in-repository root requires Owner opt-in and isolation"))?;
        if !(iso.owner_opt_in
            && iso.exclude_from_parent_discovery
            && iso.exclude_from_watchers
            && iso.exclude_from_indexers
            && iso.exclude_from_builds)
        {
            return Err(err("in-repository isolation is incomplete"));
        }
        let rel = parent
            .strip_prefix(&project)
            .map_err(|_| err("invalid in-repository root"))?;
        let exclude = ExclusionPlan {
            info_exclude: common.join("info/exclude"),
            exact_line: exclude_line(rel)?,
            requires_ownership_receipt: true,
        };
        (Some(iso), Some(exclude), vec!["Nested worktrees require enforced discovery, watcher, indexer and build isolation.".into()])
    } else {
        if overlap(&parent, &common) || checkouts.iter().any(|p| overlap(&parent, p)) {
            return Err(err(
                "worktree parent overlaps a checkout or Git common directory",
            ));
        }
        (None, None, Vec::new())
    };
    let child = resolve_path(&parent.join(incarnation_component))?;
    Ok(ResolvedWorkspacePaths {
        parent,
        child,
        isolation,
        exclusion,
        warnings,
    })
}

fn hash_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// SHA-256 of bytes actually obtained and verified by the coordinator; no filesystem I/O occurs.
pub fn executable_digest(bytes: &[u8]) -> String {
    hash_bytes(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentEvidence {
    pub path: String,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceApproval {
    pub policy_digest: String,
    pub base_oid: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanRefusal {
    HookNetworkRequiresOwner,
    HookProfileUnsupported,
    UnapprovedHook,
    UnapprovedPolicy,
    OwnerManualRequiresOwner,
    PreparationBlocked,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookPlan {
    pub id: String,
    pub phase: String,
    pub refusal: Option<PlanRefusal>,
    /// All independent blockers, including a missing approval alongside a mode refusal.
    pub refusals: Vec<PlanRefusal>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePlan {
    pub paths: ResolvedWorkspacePaths,
    pub policy_digest: String,
    pub base_oid: String,
    pub hooks: Vec<HookPlan>,
    /// Policy-level refusal covers service-only policies as well as hooks.
    pub policy_refusal: Option<PlanRefusal>,
    pub preparation_blocked: bool,
}
/// Create the canonical digest for the full policy, resolved paths, base and all content evidence.
/// Evidence identities must include each command executable and every explicitly declared script.
pub fn workspace_policy_digest(
    policy: &WorkspacePolicy,
    paths: &ResolvedWorkspacePaths,
    base_oid: &str,
    contents: &[ContentEvidence],
) -> Result<String, WorkspaceError> {
    policy.validate()?;
    if !(base_oid.len() == 40 || base_oid.len() == 64)
        || !base_oid.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(WorkspaceError::Policy(
            "base oid must be a full Git object id".into(),
        ));
    }
    let mut artifacts = BTreeMap::new();
    for c in contents {
        if !valid_text(&c.path, 4096)
            || c.sha256.len() != 64
            || !c
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || artifacts.insert(c.path.clone(), c.sha256.clone()).is_some()
        {
            return Err(WorkspaceError::Content(c.path.clone()));
        }
    }
    for (argv, additional) in policy
        .post_create
        .iter()
        .chain(&policy.pre_teardown)
        .map(|h| (&h.argv, &h.content_paths))
        .chain(policy.services.iter().map(|s| (&s.argv, &s.content_paths)))
    {
        for name in std::iter::once(&argv[0]).chain(additional) {
            if !artifacts.contains_key(name) {
                return Err(WorkspaceError::Content(name.clone()));
            }
        }
        // An interpreter's script is a separate artifact from the interpreter itself. Refuse
        // inline code and module execution here: their transitive content is not identified.
        let executable = Path::new(&argv[0])
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        // These executables dispatch another command, so binding only their own bytes would
        // misrepresent what was approved. A future explicit wrapper resolver may support them.
        if matches!(
            executable,
            "env"
                | "busybox"
                | "sudo"
                | "doas"
                | "nohup"
                | "nice"
                | "timeout"
                | "xargs"
                | "setsid"
                | "command"
        ) {
            return Err(WorkspaceError::Content("unresolved command wrapper".into()));
        }
        let interpreter = matches!(
            executable,
            "sh" | "bash" | "dash" | "zsh" | "python" | "python3" | "node" | "ruby" | "perl"
        );
        if interpreter {
            if argv
                .iter()
                .skip(1)
                .any(|arg| matches!(arg.as_str(), "-c" | "-m" | "--command" | "-e" | "--eval"))
            {
                return Err(WorkspaceError::Content(
                    "inline or module interpreter command".into(),
                ));
            }
            // Interpreter switches can consume additional arguments or change what runs.
            // Accept only an explicit first script; opaque interpreter options fail closed.
            let script = argv
                .get(1)
                .filter(|arg| !arg.starts_with('-'))
                .ok_or_else(|| WorkspaceError::Content("interpreter script".into()))?;
            if !artifacts.contains_key(script) {
                return Err(WorkspaceError::Content(script.clone()));
            }
        }
    }
    let value = serde_json::to_vec(&(policy, paths, base_oid.to_ascii_lowercase(), artifacts))
        .map_err(|e| WorkspaceError::Policy(e.to_string()))?;
    Ok(hash_bytes(&value))
}
/// Request context is authenticated and evidence is checked by the caller; this is not authorization.
#[derive(Debug, Clone)]
pub struct PlanInputs<'a> {
    pub approval: Option<&'a WorkspaceApproval>,
    pub owner_invoked: bool,
    pub automatic_profile_proven: bool,
    /// Identity of the operation whose prerequisites are being evaluated.
    pub operation_id: &'a str,
    /// Trusted, durable hook-run evidence; matching is performed here.
    pub successful_manual_hooks: &'a [HookRun],
}
pub fn plan_workspace(
    policy: &WorkspacePolicy,
    paths: ResolvedWorkspacePaths,
    base_oid: &str,
    contents: &[ContentEvidence],
    input: PlanInputs<'_>,
) -> Result<WorkspacePlan, WorkspaceError> {
    let digest = workspace_policy_digest(policy, &paths, base_oid, contents)?;
    let approved = input
        .approval
        .is_some_and(|a| a.policy_digest == digest && a.base_oid.eq_ignore_ascii_case(base_oid));
    let mut hooks = Vec::new();
    if !valid_text(input.operation_id, 256) || input.operation_id.trim().is_empty() {
        return Err(WorkspaceError::Policy("invalid operation id".into()));
    }
    let policy_refusal = (!approved).then_some(PlanRefusal::UnapprovedPolicy);
    let mut preparation_blocked = !approved;
    for (phase, hook) in policy
        .post_create
        .iter()
        .map(|h| ("post_create", h))
        .chain(policy.pre_teardown.iter().map(|h| ("pre_teardown", h)))
    {
        let completed_manual = approved
            && hook.execution == HookExecution::OwnerManual
            && input.successful_manual_hooks.iter().any(|run| {
                run.operation_id == input.operation_id
                    && run.phase == phase
                    && run.hook_id == hook.id
                    && run.approved_digest == digest
                    && run.invocation_mode == HookExecution::OwnerManual
                    && run.state == HookRunState::Succeeded
                    && run.exit_status == Some(0)
                    && run
                        .owner_invocation_receipt
                        .as_deref()
                        .is_some_and(|r| valid_text(r, 4096) && !r.trim().is_empty())
            });
        let mut refusals = Vec::new();
        if hook.needs_network && !input.owner_invoked {
            refusals.push(PlanRefusal::HookNetworkRequiresOwner);
        }
        if !approved {
            refusals.push(PlanRefusal::UnapprovedHook);
        }
        if hook.execution == HookExecution::OwnerManual && !input.owner_invoked {
            refusals.push(PlanRefusal::OwnerManualRequiresOwner);
            if hook.required && !completed_manual {
                refusals.push(PlanRefusal::PreparationBlocked);
            }
        }
        if hook.execution == HookExecution::Automatic && !input.automatic_profile_proven {
            refusals.push(PlanRefusal::HookProfileUnsupported);
        }
        let refusal = refusals.first().copied();
        if phase == "post_create" && hook.required {
            let blocked = if hook.execution == HookExecution::OwnerManual {
                !completed_manual
            } else {
                refusal.is_some()
            };
            preparation_blocked |= blocked;
        }
        hooks.push(HookPlan {
            id: hook.id.clone(),
            phase: phase.into(),
            refusal,
            refusals,
        });
    }
    Ok(WorkspacePlan {
        paths,
        policy_digest: digest,
        base_oid: base_oid.to_ascii_lowercase(),
        hooks,
        policy_refusal,
        preparation_blocked,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookRunState {
    Queued,
    Dispatching,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Ambiguous,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookRun {
    pub id: String,
    pub operation_id: String,
    pub phase: String,
    pub hook_id: String,
    pub approved_digest: String,
    pub invocation_mode: HookExecution,
    pub execution_profile: String,
    pub owner_invocation_receipt: Option<String>,
    pub execution_id: String,
    pub state: HookRunState,
    pub exit_status: Option<i32>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub output_excerpt_ref: Option<String>,
    pub timeout_reason: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub id: String,
    pub node_id: String,
    pub repo_id: String,
    pub git_common_dir: PathBuf,
    pub project_id: String,
    pub canonical_path: PathBuf,
    pub branch_ref: String,
    pub base_oid: String,
    pub mode: WorktreeMode,
    pub owner_session_incarnation: String,
    pub attached_sessions: Vec<String>,
    pub policy_digest: String,
    pub revision: u64,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceOperation {
    pub operation_id: String,
    pub idempotency_key: String,
    pub worktree_id: String,
    pub caller_authority: String,
    pub coordinator_lease: String,
    pub expected_revision: u64,
    pub step: String,
    pub step_state: String,
    pub hook_run_ids: Vec<String>,
    pub host_resource_ids: Vec<String>,
    pub outcome: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortLease {
    pub lease_id: String,
    pub node_id: String,
    pub worktree_id: String,
    pub service: String,
    pub owner_session_incarnation: String,
    pub host_generation: u64,
    pub port: u16,
    pub protocol: String,
    pub listener_resource_id: String,
    pub backend_resource_id: Option<String>,
    pub state: String,
    pub revision: u64,
}
