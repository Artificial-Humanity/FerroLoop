//! Workflow runs share one stored `WorkflowRun` between every step that executes, so the
//! commit of one step must not clobber another's (audit 2026-10-30, finding X-1).

use fl_core::agent::{AgentRole, RegisterAgentArgs};
use fl_core::at::At;
use fl_core::ids::{ProjectId, WorkflowId};
use fl_core::model::State;
use fl_core::store::{AgentRegistry, Catalog, WorkflowStore};
use fl_core::workflow::{
    AssigneeRule, END_STEP, EdgeCondition, EdgeDef, JoinPolicy, StepCompletion, StepDef,
    WorkflowDef, WorkflowRunStatus,
};
use fl_exec::{
    Git, StepExecutionOptions, WorkflowEnv, WorkflowExecError, execute_workflow_step, stamp,
    start_workflow_run,
};
use fl_store::RedbStore;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git_repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(d.path())
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    fs::create_dir_all(d.path().join("src")).unwrap();
    fs::write(d.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    d
}

/// A fake `claude` that sleeps, then prints a result line with 150 tokens.
fn slow_stub(dir: &Path, secs: &str) -> PathBuf {
    let path = dir.join("slow-claude.sh");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nsleep {secs}\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\",\"usage\":{{\"input_tokens\":120,\"output_tokens\":30}},\"total_cost_usd\":0.0015}}'\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn step(id: &str) -> StepDef {
    StepDef {
        step_id: id.into(),
        title: id.into(),
        prompt: format!("Do {id}."),
        assignee: AssigneeRule::Agent {
            name: "felicia".into(),
        },
        reviews_step: None,
        require_different_vendor: false,
        completion: StepCompletion::Ungated,
        join_policy: JoinPolicy::All,
        max_attempts: 1,
        max_visits: 3,
        budget_usd_micros: None,
        max_tokens: None,
        isolate_worktree: false,
    }
}

fn pass(from: &str, to: &str) -> EdgeDef {
    EdgeDef {
        from_step: from.into(),
        to_step: to.into(),
        on: EdgeCondition::Pass,
    }
}

struct Fixture {
    _repo: tempfile::TempDir,
    _db: tempfile::TempDir,
    store: RedbStore,
    project: ProjectId,
    head: String,
    stub_dir: PathBuf,
}

fn fixture() -> Fixture {
    let repo = git_repo();
    let db = tempfile::tempdir().unwrap();
    let store = RedbStore::open(&db.path().join("fl.redb")).unwrap();
    let project = store
        .add_project(&repo.path().display().to_string())
        .unwrap();
    store
        .register_agent(RegisterAgentArgs {
            display_name: "Felicia".into(),
            email: None,
            project: "ferroloop".into(),
            role: AgentRole::Assistant,
            extra_areas: vec![],
            create_area: true,
            replace: false,
            at: At::from_unix_millis(11),
            by: "owner".into(),
        })
        .unwrap();
    let head = Git::head(repo.path()).unwrap();
    let stub_dir = repo.path().to_path_buf();
    Fixture {
        _repo: repo,
        _db: db,
        store,
        project,
        head,
        stub_dir,
    }
}

fn define(fx: &Fixture, steps: Vec<StepDef>, edges: Vec<EdgeDef>, max_total: u32) -> WorkflowDef {
    fx.store
        .upsert_workflow_def(WorkflowDef {
            id: WorkflowId(stamp::entry_id()),
            project: fx.project.clone(),
            slug: "audit-flow".into(),
            title: "Audit flow".into(),
            start_step: steps[0].step_id.clone(),
            steps,
            edges,
            max_total_steps: max_total,
            budget_usd_micros: None,
            max_tokens: None,
            declared_keys: BTreeMap::new(),
            authored_at_commit: fx.head.clone(),
            authored_by: "ferris".into(),
        })
        .unwrap()
}

fn opts(stub: &Path, step: Option<&str>) -> StepExecutionOptions {
    StepExecutionOptions {
        step_id: step.map(str::to_string),
        adapter_override: Some("claude".into()),
        binary_override: Some(stub.to_string_lossy().into_owned()),
        by: "felicia".into(),
        ..Default::default()
    }
}

/// `execute_workflow_step` wrote back the run it loaded before a minutes-long attempt, so two
/// fan-out branches executing at once overwrote each other: one completion vanished, the fan-in
/// never saw both branches, and the lost branch's tokens and step never counted toward the
/// run's ceilings.
#[test]
fn concurrent_fan_out_branches_both_land_and_their_spend_is_summed() {
    let fx = fixture();
    let def = define(
        &fx,
        vec![
            step("start"),
            step("branch-a"),
            step("branch-b"),
            step("join"),
        ],
        vec![
            pass("start", "branch-a"),
            pass("start", "branch-b"),
            pass("branch-a", "join"),
            pass("branch-b", "join"),
            pass("join", END_STEP),
        ],
        16,
    );
    let stub = slow_stub(&fx.stub_dir, "0.6");
    let run = start_workflow_run(
        &fx.store,
        &def,
        "fan out",
        At::from_unix_millis(20),
        "ferris",
    )
    .unwrap();
    execute_workflow_step(WorkflowEnv::single(&fx.store), &run.id, &opts(&stub, None)).unwrap();
    let mid = fx.store.get_workflow_run(&run.id).unwrap().unwrap();
    assert_eq!(mid.active_steps.len(), 2, "{:?}", mid.active_steps);

    std::thread::scope(|s| {
        for branch in ["branch-a", "branch-b"] {
            let (stub, run_id, store) = (&stub, &run.id, &fx.store);
            s.spawn(move || {
                // `WorkflowEnv` holds `dyn` stores and is not `Send`: each thread builds its own.
                execute_workflow_step(
                    WorkflowEnv::single(store),
                    run_id,
                    &opts(stub, Some(branch)),
                )
                .unwrap();
            });
        }
    });

    let end = fx.store.get_workflow_run(&run.id).unwrap().unwrap();
    assert_eq!(end.steps["branch-a"].state, State::Done, "{end:#?}");
    assert_eq!(end.steps["branch-b"].state, State::Done, "{end:#?}");
    assert_eq!(end.active_steps, vec!["join".to_string()], "{end:#?}");
    assert_eq!(end.total_steps_executed, 3, "a branch's step was lost");
    assert_eq!(end.total_tokens, 3 * 150, "a branch's tokens were lost");
}

/// `max_total_steps` is a budget: once the run has used it up, the next step must be refused
/// before its attempt spends anything, not run first and be flagged afterwards.
#[test]
fn a_step_is_refused_before_it_runs_once_max_total_steps_is_used_up() {
    let fx = fixture();
    let def = define(
        &fx,
        vec![step("one"), step("two")],
        vec![pass("one", "two"), pass("two", END_STEP)],
        1,
    );
    let stub = slow_stub(&fx.stub_dir, "0");
    let ran = fx.stub_dir.join("ran.log");
    // The stub appends to a log, so a refused step leaves no trace.
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\necho ran >> '{}'\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}},\"total_cost_usd\":0.0}}'\n",
            ran.display()
        ),
    )
    .unwrap();
    let run = start_workflow_run(
        &fx.store,
        &def,
        "budget",
        At::from_unix_millis(20),
        "ferris",
    )
    .unwrap();
    let env = WorkflowEnv::single(&fx.store);
    execute_workflow_step(env, &run.id, &opts(&stub, None)).unwrap();
    assert_eq!(fs::read_to_string(&ran).unwrap().lines().count(), 1);

    let err = execute_workflow_step(env, &run.id, &opts(&stub, None)).unwrap_err();
    assert!(
        matches!(&err, WorkflowExecError::Workflow(e) if e.is_policy_refusal()),
        "{err:?}"
    );
    assert_eq!(
        fs::read_to_string(&ran).unwrap().lines().count(),
        1,
        "the second attempt ran although the step budget was spent"
    );
    let after = fx.store.get_workflow_run(&run.id).unwrap().unwrap();
    assert_eq!(after.status, WorkflowRunStatus::NeedsHuman, "{after:#?}");
}
