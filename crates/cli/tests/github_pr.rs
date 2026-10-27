//! End-to-end CLI tests for `fl github pr` (`create`, `policy`, `review`, `merge`, `verify-ci`),
//! dual-credential Reviewer App brokering (`fake-user` author + `fake-app[bot]` reviewer),
//! and FerroWire review/merge notifications over `FakeGithub`.

use assert_cmd::Command;
use fl_core::pr_policy::{FlReviewMarker, PrCommitSummary, ReviewVerdict, extract_review_marker};
use fl_github::fake::{APP_SLUG, FakeGithub, USER_LOGIN};
use predicates::str::contains;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command as Sys;

fn throwaway_rsa_pem() -> String {
    let out = Sys::new("openssl")
        .args(["genrsa", "-traditional", "2048"])
        .output()
        .expect("tests require the `openssl` CLI");
    assert!(out.status.success(), "openssl genrsa failed");
    String::from_utf8(out.stdout).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Sys::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct PrFixture {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    fake: FakeGithub,
}

impl PrFixture {
    fn new(shared_store: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.email", "t@example.com"]);
        git(repo.path(), &["config", "user.name", "t"]);
        fs::create_dir_all(repo.path().join("src")).unwrap();
        fs::write(repo.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-qm", "initial"]);

        let fake = FakeGithub::start("Artificial-Humanity/FerroLoop");
        let key_path = home.path().join("reviewer-app.pem");
        fs::write(&key_path, throwaway_rsa_pem()).unwrap();

        let project_store = home.path().join("fl.redb");
        let state_store = if shared_store {
            project_store.clone()
        } else {
            home.path().join("ws-state.redb")
        };

        let cfg = format!(
            "[state]\nstore = \"{}\"\n\n\
             [github]\napp_id = 42\nprivate_key = \"{}\"\n\n\
             [[project]]\nname = \"ferroloop\"\nroot = \"{}\"\nstore = \"{}\"\n\
             tracker = {{ github = \"Artificial-Humanity/FerroLoop\", credential = \"env\" }}\n",
            state_store.display(),
            key_path.display(),
            repo.path().canonicalize().unwrap().display(),
            project_store.display(),
        );
        fs::create_dir_all(home.path().join("config/fl")).unwrap();
        fs::write(home.path().join("config/fl/config.toml"), cfg).unwrap();

        Self { home, repo, fake }
    }

    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("XDG_STATE_HOME", self.home.path().join("state"))
            .env("XDG_RUNTIME_DIR", self.home.path().join("run"))
            .env("FL_WIRE_SOCKET", self.home.path().join("run/wire.sock"))
            .env("FL_WIRE_ROUTES", self.home.path().join("state/routes.json"))
            .env("FL_WIRE_DISABLE_HOST_DISCOVERY", "1")
            .env("FL_GITHUB_TOKEN", "user-token")
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env_remove("GITHUB_TOKEN")
            .env_remove("FL_DB")
            .env_remove("FL_GITHUB_APP_ID")
            .env_remove("FL_GITHUB_PRIVATE_KEY_PATH")
            .env_remove("AGENT_PERSONA")
            .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("CODEX_THREAD_ID")
            .current_dir(self.repo.path());
        c
    }

    fn seed_roster(&self) {
        for (name, project, role) in [
            ("Jarvis", "ai-lab-amd", "lab_manager"),
            ("Ferris", "ferroloop", "lead"),
            ("Felicia", "ferroloop", "assistant"),
            ("Alex", "ferroloop", "assistant"),
        ] {
            self.fl()
                .args([
                    "agent",
                    "register",
                    name,
                    "--project",
                    project,
                    "--role",
                    role,
                    "--create-area",
                    "--by",
                    "owner",
                ])
                .assert()
                .success();
        }
    }

    fn read_inbox_json(&self, agent: &str) -> Vec<Value> {
        let out = self
            .fl()
            .args(["wire", "read", "--for", agent, "--json"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "wire read failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn full_pr_hierarchy_assistant_to_lead_to_lab_manager_to_owner() {
    let f = PrFixture::new(true);
    f.seed_roster();

    // -------------------------------------------------------------------------
    // 1. Assistant (`felicia`) opens PR #1 -> routes to Project Lead (`ferris`)
    // -------------------------------------------------------------------------
    f.fake.seed_branch(
        "felicia/feat-1",
        "sha-pr1",
        vec![PrCommitSummary {
            sha: "sha-pr1".into(),
            author_login: Some(USER_LOGIN.into()),
            message:
                "feat(cli): add helper\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
        }],
        vec!["crates/cli/src/cmd/pr.rs".into()],
    );

    let create1_out = f
        .fl()
        .args([
            "github",
            "pr",
            "create",
            "--title",
            "Add PR helper",
            "--body",
            "Implements helper.",
            "--head",
            "felicia/feat-1",
            "--by",
            "felicia",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        create1_out.status.success(),
        "create1 stderr: {}",
        String::from_utf8_lossy(&create1_out.stderr)
    );
    let create1_json: Value = serde_json::from_slice(&create1_out.stdout).unwrap();
    assert_eq!(create1_json["number"], 1);
    assert_eq!(create1_json["required_tier"]["tier"], "lead");
    assert_eq!(create1_json["required_tier"]["project"], "ferroloop");
    assert_eq!(create1_json["target_reviewer"], "ferris");
    assert!(
        f.fake.pull(1).requested_reviewers.is_empty(),
        "Owner must not be requested on GitHub for an Assistant PR"
    );

    // Verify FerroWire review request arrived in `ferris`'s inbox
    let ferris_inbox = f.read_inbox_json("ferris");
    assert_eq!(ferris_inbox.len(), 1);
    assert_eq!(ferris_inbox[0]["sender"], "felicia");
    assert!(
        ferris_inbox[0]["subject"]
            .as_str()
            .unwrap()
            .contains("PR #1 ready for lead(ferroloop) review")
    );

    // Check `fl github pr policy 1 --json` before approval
    let policy1_out = f
        .fl()
        .args(["github", "pr", "policy", "1", "--json"])
        .output()
        .unwrap();
    assert!(policy1_out.status.success());
    let policy1_json: Value = serde_json::from_slice(&policy1_out.stdout).unwrap();
    assert_eq!(policy1_json["required_tier"]["tier"], "lead");
    assert_eq!(policy1_json["target_reviewer"], "ferris");
    assert_eq!(policy1_json["approved"], false);

    // 2. `felicia` attempts self-review -> exits 2 (`self_review_forbidden`)
    let self_rev = f
        .fl()
        .args([
            "github",
            "pr",
            "review",
            "1",
            "--approve",
            "--body",
            "self",
            "--by",
            "felicia",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(self_rev.status.code(), Some(2));
    let self_rev_json: Value = serde_json::from_slice(&self_rev.stdout).unwrap();
    assert_eq!(self_rev_json["code"], "self_review_forbidden");
    assert_eq!(self_rev_json["agent"], "felicia");

    // 3. Peer assistant `alex` attempts review -> exits 2 (`insufficient_role`)
    let peer_rev = f
        .fl()
        .args([
            "github",
            "pr",
            "review",
            "1",
            "--approve",
            "--body",
            "peer",
            "--by",
            "alex",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(peer_rev.status.code(), Some(2));
    let peer_rev_json: Value = serde_json::from_slice(&peer_rev.stdout).unwrap();
    assert_eq!(peer_rev_json["code"], "insufficient_role");
    assert_eq!(peer_rev_json["agent"], "alex");

    // Before approval, `felicia` cannot merge her own PR #1 -> exits 2 (`missing_valid_approval`)
    let unapproved_merge = f
        .fl()
        .args([
            "github", "pr", "merge", "1", "--method", "squash", "--by", "felicia", "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(unapproved_merge.status.code(), Some(2));
    let unapproved_merge_json: Value = serde_json::from_slice(&unapproved_merge.stdout).unwrap();
    assert_eq!(unapproved_merge_json["code"], "missing_valid_approval");

    // Before approval, `verify-ci --require-approval` exits 2 (`missing_valid_approval`)
    let unapproved_ci = f
        .fl()
        .args([
            "github",
            "pr",
            "verify-ci",
            "1",
            "--require-approval",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(unapproved_ci.status.code(), Some(2));
    let unapproved_ci_json: Value = serde_json::from_slice(&unapproved_ci.stdout).unwrap();
    assert_eq!(unapproved_ci_json["code"], "missing_valid_approval");

    // 4. Project Lead `ferris` approves PR #1 via Reviewer App (`fake-app[bot]`)
    f.fl()
        .args([
            "github",
            "pr",
            "review",
            "1",
            "--approve",
            "--body",
            "LGTM, clean implementation.",
            "--by",
            "ferris",
        ])
        .assert()
        .success()
        .stdout(contains("reviewed\t1\tapproved\tferris\tsha-pr1"));

    let pr1_state = f.fake.pull(1);
    assert_eq!(pr1_state.reviews.len(), 1);
    assert_eq!(
        pr1_state.reviews[0].author_login,
        format!("{APP_SLUG}[bot]")
    );
    assert_eq!(pr1_state.reviews[0].state, "APPROVED");
    let marker: FlReviewMarker =
        extract_review_marker(&pr1_state.reviews[0].body).expect("embedded fl-review marker");
    assert_eq!(marker.reviewer, "ferris");
    assert_eq!(marker.head_sha, "sha-pr1");
    assert_eq!(marker.verdict, ReviewVerdict::Approved);

    // 5. `verify-ci --require-approval` now exits 0
    f.fl()
        .args(["github", "pr", "verify-ci", "1", "--require-approval"])
        .assert()
        .success()
        .stdout(contains("verified\t1\tlead(ferroloop)\tsha-pr1"));

    // Non-co-author peer assistant `alex` is still refused when attempting to merge PR #1 (`insufficient_role`)
    let peer_merge = f
        .fl()
        .args([
            "github", "pr", "merge", "1", "--method", "squash", "--by", "alex", "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(peer_merge.status.code(), Some(2));
    let peer_merge_json: Value = serde_json::from_slice(&peer_merge.stdout).unwrap();
    assert_eq!(peer_merge_json["code"], "insufficient_role");
    assert_eq!(peer_merge_json["agent"], "alex");

    // 6. Author-owned merge: `felicia` (co-author) merges her own PR #1 once `ferris` has approved
    f.fl()
        .args([
            "github", "pr", "merge", "1", "--method", "squash", "--by", "felicia",
        ])
        .assert()
        .success()
        .stdout(contains("merged\t1\tsquash\tfelicia"));
    assert!(f.fake.pull(1).merged);

    let felicia_inbox = f.read_inbox_json("felicia");
    assert!(
        felicia_inbox.iter().any(|item| {
            item["subject"]
                .as_str()
                .unwrap_or_default()
                .contains("PR #1 reviewed by ferris")
        }),
        "expected review notification in felicia's inbox, got: {felicia_inbox:?}"
    );

    // -------------------------------------------------------------------------
    // 2. Project Lead (`ferris`) opens PR #2 -> escalates to LabManager (`jarvis`)
    // -------------------------------------------------------------------------
    f.fake.seed_branch(
        "ferris/lead-change",
        "sha-pr2",
        vec![PrCommitSummary {
            sha: "sha-pr2".into(),
            author_login: Some(USER_LOGIN.into()),
            message:
                "feat(core): lead refactor\n\nCo-authored-by: Ferris <ferris@artificialhumanity.io>"
                    .into(),
        }],
        vec!["crates/core/src/pr_policy.rs".into()],
    );

    let create2_out = f
        .fl()
        .args([
            "github",
            "pr",
            "create",
            "--title",
            "Lead refactor",
            "--body",
            "Refactors core.",
            "--head",
            "ferris/lead-change",
            "--by",
            "ferris",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(create2_out.status.success());
    let create2_json: Value = serde_json::from_slice(&create2_out.stdout).unwrap();
    assert_eq!(create2_json["number"], 2);
    assert_eq!(create2_json["required_tier"]["tier"], "lab_manager");
    assert_eq!(create2_json["target_reviewer"], "jarvis");

    let jarvis_inbox = f.read_inbox_json("jarvis");
    assert_eq!(jarvis_inbox.len(), 1);
    assert!(
        jarvis_inbox[0]["subject"]
            .as_str()
            .unwrap()
            .contains("PR #2 ready for lab_manager review")
    );

    // `ferris` self-review on PR #2 is forbidden (`self_review_forbidden`)
    let ferris_self = f
        .fl()
        .args([
            "github",
            "pr",
            "review",
            "2",
            "--approve",
            "--body",
            "self",
            "--by",
            "ferris",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(ferris_self.status.code(), Some(2));
    let ferris_self_json: Value = serde_json::from_slice(&ferris_self.stdout).unwrap();
    assert_eq!(ferris_self_json["code"], "self_review_forbidden");

    // `jarvis` (`lab_manager`) approves and merges PR #2 -> exits 0
    f.fl()
        .args([
            "github",
            "pr",
            "review",
            "2",
            "--approve",
            "--body",
            "Approved by Lab Manager.",
            "--by",
            "jarvis",
        ])
        .assert()
        .success();

    f.fl()
        .args(["github", "pr", "merge", "2", "--by", "jarvis"])
        .assert()
        .success();
    assert!(f.fake.pull(2).merged);

    // -------------------------------------------------------------------------
    // 3. PR #3 touching `.github/workflows/ci.yml` -> escalates to Owner (`lmcfarlin`)
    // -------------------------------------------------------------------------
    f.fake.seed_branch(
        "felicia/ci-update",
        "sha-pr3",
        vec![PrCommitSummary {
            sha: "sha-pr3".into(),
            author_login: Some(USER_LOGIN.into()),
            message:
                "ci: update workflow\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
        }],
        vec![".github/workflows/ci.yml".into()],
    );

    let create3_out = f
        .fl()
        .args([
            "github",
            "pr",
            "create",
            "--title",
            "Update CI workflow",
            "--body",
            "Touches protected workflow.",
            "--head",
            "felicia/ci-update",
            "--by",
            "felicia",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(create3_out.status.success());
    let create3_json: Value = serde_json::from_slice(&create3_out.stdout).unwrap();
    assert_eq!(create3_json["number"], 3);
    assert_eq!(create3_json["required_tier"]["tier"], "owner");
    assert_eq!(create3_json["target_reviewer"], "lmcfarlin");
    assert_eq!(
        f.fake.pull(3).requested_reviewers,
        vec!["lmcfarlin".to_string()]
    );

    // Both `ferris` (`lead`) and `jarvis` (`lab_manager`) are refused with `protected_path_requires_owner`
    for caller in ["ferris", "jarvis"] {
        let out = f
            .fl()
            .args([
                "github",
                "pr",
                "review",
                "3",
                "--approve",
                "--body",
                "try",
                "--by",
                caller,
                "--json",
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "caller {caller} should fail");
        let refusal: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(refusal["code"], "protected_path_requires_owner");
        assert_eq!(refusal["agent"], caller);
        assert_eq!(
            refusal["files"],
            serde_json::json!([".github/workflows/ci.yml"])
        );
    }
}

#[test]
fn separate_workspace_state_store_and_env_app_credentials_work() {
    let f = PrFixture::new(false);
    f.seed_roster();

    // Remove `[github]` from `config.toml` and pass `$FL_GITHUB_APP_ID` + `$FL_GITHUB_PRIVATE_KEY_PATH` instead
    let cfg_path = f.home.path().join("config/fl/config.toml");
    let raw_cfg = fs::read_to_string(&cfg_path).unwrap();
    let stripped: String = raw_cfg
        .lines()
        .filter(|l| {
            !l.starts_with("[github]") && !l.starts_with("app_id") && !l.starts_with("private_key")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&cfg_path, format!("{stripped}\n")).unwrap();

    f.fake.seed_branch(
        "felicia/separate-store",
        "sha-sep",
        vec![PrCommitSummary {
            sha: "sha-sep".into(),
            author_login: Some(USER_LOGIN.into()),
            message:
                "feat: separate store test\n\nCo-authored-by: Felicia <felicia@artificialhumanity.io>"
                    .into(),
        }],
        vec!["src/lib.rs".into()],
    );

    f.fl()
        .args([
            "github",
            "pr",
            "create",
            "--title",
            "Separate store PR",
            "--body",
            "Testing separate ws_store.",
            "--head",
            "felicia/separate-store",
        ])
        .assert()
        .success();

    // Without `[github]` or `$FL_GITHUB_APP_ID`, review fails with a clear configuration message
    f.fl()
        .args([
            "github",
            "pr",
            "review",
            "1",
            "--approve",
            "--body",
            "LGTM",
            "--by",
            "ferris",
        ])
        .assert()
        .failure()
        .stderr(contains("FL_GITHUB_APP_ID"));

    // Providing `$FL_GITHUB_APP_ID` and `$FL_GITHUB_PRIVATE_KEY_PATH` succeeds
    let key_path = f.home.path().join("reviewer-app.pem");
    f.fl()
        .env("FL_GITHUB_APP_ID", "42")
        .env("FL_GITHUB_PRIVATE_KEY_PATH", &key_path)
        .args([
            "github",
            "pr",
            "review",
            "1",
            "--approve",
            "--body",
            "LGTM",
            "--by",
            "ferris",
        ])
        .assert()
        .success();

    f.fl()
        .env("FL_GITHUB_APP_ID", "42")
        .env("FL_GITHUB_PRIVATE_KEY_PATH", &key_path)
        .args(["github", "pr", "merge", "1", "--by", "ferris"])
        .assert()
        .success();
    assert!(f.fake.pull(1).merged);
}
