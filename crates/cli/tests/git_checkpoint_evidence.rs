//! Dormant, process-free contract for a future CI-only Git fixture trace.
//!
//! These synthetic tests do not acquire a kit, launch a process, verify real
//! ownership, or establish that any runner is supported. No production or
//! fixture-context path consumes this contract. Every test is explicitly
//! ignored by the normal workspace run; an eventual opt-in harness must first
//! supply reviewed pins and independently establish ownership and trace capture.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Resource {
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Runner {
    os: String,
    arch: String,
    image: String,
    image_version: String,
    abi: String,
}

/// Expected values must come from a reviewed, pinned kit, not ambient discovery.
#[derive(Clone, Debug)]
struct Manifest {
    version: u32,
    fixture_root: PathBuf,
    runner: Runner,
    artifact_sha256: String,
    git: Resource,
    tracer: Resource,
    helpers: Vec<Resource>,
}

#[derive(Clone, Debug)]
struct Observation {
    runner: Runner,
    artifact_sha256: String,
    git: Resource,
    tracer: Resource,
    helpers: Vec<Resource>,
    trace: Trace,
}

#[derive(Clone, Debug)]
struct Trace {
    /// The trace producer must affirm it captured the complete process tree and
    /// both exec and connect streams; an incomplete trace is never evidence.
    complete_process_tree: bool,
    complete_exec_stream: bool,
    complete_connect_stream: bool,
    events: Vec<Event>,
}

#[derive(Clone, Debug)]
enum Event {
    Exec {
        pid: u32,
        executable: PathBuf,
        args: Vec<String>,
    },
    Exit {
        pid: u32,
        success: bool,
    },
    Connect,
}

fn sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn absolute_file(path: &Path) -> bool {
    path.is_absolute()
        && path.file_name().is_some()
        && !path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
}

fn valid_resource(resource: &Resource) -> bool {
    absolute_file(&resource.path) && sha256(&resource.sha256)
}

fn expected_git_commands(repo: &Path) -> Vec<Vec<String>> {
    let cwd = repo.to_string_lossy().into_owned();
    let command = |args: &[&str]| {
        [
            vec!["-C".to_string(), cwd.clone()],
            args.iter().map(|s| (*s).to_string()).collect(),
        ]
        .concat()
    };
    vec![
        command(&["init", "-b", "main"]),
        command(&["config", "user.name", "Checkpoint Tester"]),
        command(&["config", "user.email", "tester@example.invalid"]),
        command(&["add", "tracked.txt"]),
        command(&["commit", "-m", "cli checkpoint subject"]),
        command(&["rev-parse", "--show-toplevel"]),
        command(&["branch", "--show-current"]),
        command(&["rev-parse", "HEAD"]),
        command(&["log", "-1", "--format=%s"]),
        command(&["status", "--porcelain=v1", "-z", "--untracked-files=all"]),
    ]
}

/// Validate supplied evidence only. Successful validation is not proof that an
/// archive was acquired safely or that any real process or fixture was owned.
fn validate(manifest: &Manifest, observed: &Observation, repo: &Path) -> Result<(), &'static str> {
    if manifest.version != 1 {
        return Err("unsupported manifest version");
    }
    if manifest.runner.os != "linux"
        || manifest.runner.arch != "x86_64"
        || manifest.runner.image.trim().is_empty()
        || manifest.runner.image_version.trim().is_empty()
        || manifest.runner.abi.trim().is_empty()
        || observed.runner != manifest.runner
    {
        return Err("unverified or mismatched runner");
    }
    if !sha256(&manifest.artifact_sha256)
        || observed.artifact_sha256 != manifest.artifact_sha256
        || !valid_resource(&manifest.git)
        || !valid_resource(&manifest.tracer)
        || observed.git != manifest.git
        || observed.tracer != manifest.tracer
        || observed.helpers != manifest.helpers
        || !absolute_file(&manifest.fixture_root)
        || !absolute_file(repo)
        || !repo.starts_with(&manifest.fixture_root)
        || repo == manifest.fixture_root
    {
        return Err("missing or mismatched kit, tracer, or repository identity");
    }
    let mut resources = HashSet::new();
    for resource in std::iter::once(&manifest.git)
        .chain(std::iter::once(&manifest.tracer))
        .chain(&manifest.helpers)
    {
        if !valid_resource(resource)
            || !resource.path.starts_with(&manifest.fixture_root)
            || !resources.insert(&resource.path)
        {
            return Err("invalid or conflicting resource");
        }
    }
    let trace = &observed.trace;
    if !trace.complete_process_tree || !trace.complete_exec_stream || !trace.complete_connect_stream
    {
        return Err("incomplete trace");
    }
    let helpers: HashSet<_> = manifest.helpers.iter().map(|r| &r.path).collect();
    let expected = expected_git_commands(repo);
    let mut commands = Vec::new();
    let mut processes = HashMap::new();
    for event in &trace.events {
        match event {
            Event::Connect => return Err("network attempt observed"),
            Event::Exec {
                pid,
                executable,
                args,
            } => {
                if *pid == 0 || processes.insert(*pid, false).is_some() {
                    return Err("invalid or repeated process");
                }
                if executable == &manifest.git.path {
                    commands.push(args.clone());
                } else if !helpers.contains(executable) {
                    return Err("unexpected executable");
                }
            }
            Event::Exit { pid, success } => {
                let exited = processes.get_mut(pid).ok_or("exit without exec")?;
                if *exited || !success {
                    return Err("failed or duplicate process exit");
                }
                *exited = true;
            }
        }
    }
    if processes.values().any(|exited| !exited) || commands != expected {
        return Err("incomplete process exits or unexpected operations");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Manifest, Observation, PathBuf) {
        let runner = Runner {
            os: "linux".into(),
            arch: "x86_64".into(),
            image: "synthetic-image".into(),
            image_version: "synthetic-version".into(),
            abi: "synthetic-abi".into(),
        };
        let resource = |path: &str, digit: char| Resource {
            path: path.into(),
            sha256: std::iter::repeat_n(digit, 64).collect(),
        };
        let manifest = Manifest {
            version: 1,
            fixture_root: "/fixture".into(),
            runner: runner.clone(),
            artifact_sha256: "a".repeat(64),
            git: resource("/fixture/kit/git", 'b'),
            tracer: resource("/fixture/kit/tracer", 'c'),
            helpers: vec![],
        };
        let repo = PathBuf::from("/fixture/repo");
        let mut events = Vec::new();
        for (index, args) in expected_git_commands(&repo).into_iter().enumerate() {
            let pid = index as u32 + 1;
            events.push(Event::Exec {
                pid,
                executable: manifest.git.path.clone(),
                args,
            });
            events.push(Event::Exit { pid, success: true });
        }
        let observed = Observation {
            runner,
            artifact_sha256: manifest.artifact_sha256.clone(),
            git: manifest.git.clone(),
            tracer: manifest.tracer.clone(),
            helpers: vec![],
            trace: Trace {
                complete_process_tree: true,
                complete_exec_stream: true,
                complete_connect_stream: true,
                events,
            },
        };
        (manifest, observed, repo)
    }

    #[test]
    #[ignore = "opt-in synthetic evidence contract; no real CI fixture is provisioned"]
    fn synthetic_complete_trace_satisfies_contract() {
        let (manifest, observed, repo) = sample();
        assert_eq!(validate(&manifest, &observed, &repo), Ok(()));
    }

    #[test]
    #[ignore = "opt-in synthetic evidence contract; no real CI fixture is provisioned"]
    fn refuses_unpinned_or_mismatched_runner_kit_and_tracer() {
        let (manifest, mut observed, repo) = sample();
        observed.runner.image_version.clear();
        assert!(validate(&manifest, &observed, &repo).is_err());
        let (_, mut observed, _) = sample();
        observed.artifact_sha256 = "0".repeat(64);
        assert!(validate(&manifest, &observed, &repo).is_err());
        let (_, mut observed, _) = sample();
        observed.tracer.path = "/usr/bin/strace".into();
        assert!(validate(&manifest, &observed, &repo).is_err());
        let (mut invalid, observed, _) = sample();
        invalid.version = 0;
        assert!(validate(&invalid, &observed, &repo).is_err());
    }

    #[test]
    #[ignore = "opt-in synthetic evidence contract; no real CI fixture is provisioned"]
    fn refuses_incomplete_failed_or_network_trace() {
        let (manifest, mut observed, repo) = sample();
        observed.trace.complete_connect_stream = false;
        assert!(validate(&manifest, &observed, &repo).is_err());
        let (_, mut observed, _) = sample();
        observed.trace.events.push(Event::Connect);
        assert!(validate(&manifest, &observed, &repo).is_err());
        let (_, mut observed, _) = sample();
        observed.trace.events.pop();
        assert!(validate(&manifest, &observed, &repo).is_err());
        let (_, mut observed, _) = sample();
        observed.trace.events[1] = Event::Exit {
            pid: 1,
            success: false,
        };
        assert!(validate(&manifest, &observed, &repo).is_err());
    }

    #[test]
    #[ignore = "opt-in synthetic evidence contract; no real CI fixture is provisioned"]
    fn refuses_unexpected_executable_or_operation() {
        let (manifest, mut observed, repo) = sample();
        observed.trace.events.insert(
            0,
            Event::Exec {
                pid: 99,
                executable: "/usr/bin/git".into(),
                args: vec![],
            },
        );
        assert!(validate(&manifest, &observed, &repo).is_err());
        let (_, mut observed, _) = sample();
        if let Event::Exec { args, .. } = &mut observed.trace.events[0] {
            args.push("--unexpected".into());
        }
        assert!(validate(&manifest, &observed, &repo).is_err());
    }

    #[test]
    #[ignore = "opt-in synthetic evidence contract; no real CI fixture is provisioned"]
    fn refuses_bad_manifest_resources() {
        let (mut manifest, observed, repo) = sample();
        manifest.git.sha256 = String::new();
        assert!(validate(&manifest, &observed, &repo).is_err());
        let (mut manifest, mut observed, repo) = sample();
        manifest.helpers.push(manifest.git.clone());
        observed.helpers = manifest.helpers.clone();
        assert!(validate(&manifest, &observed, &repo).is_err());
        let (manifest, observed, _) = sample();
        assert!(validate(&manifest, &observed, Path::new("../unowned")).is_err());
        assert!(validate(&manifest, &observed, Path::new("/outside/repo")).is_err());
    }
}
