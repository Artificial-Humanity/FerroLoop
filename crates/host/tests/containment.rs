//! Pipe-child containment. Readiness is a pipe message. Sleeps are not a barrier.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fl_host::{
    CLEANUP_DEADLINE, CleanupReport, ContainmentRefusal, CrashPoint, Host, LeaderAnchor,
    OpenOptions, ProcessIdentity, STARTUP_CLEANUP_DEADLINE, SessionHandle, SessionPhase,
    SurvivorRecord, check_pidfd_support, process_identity, read_registry, refusal,
    signal_anchored_group, write_raw_state, write_registry_fixture,
};
use fl_host_proto::{
    BlockingHostClient, ChildState, HostConfig, HostEndpoint, SessionEpoch, SessionId, SpawnKind,
    SpawnSpec,
};

fn child_bin() -> String {
    env!("CARGO_BIN_EXE_fl-host-testchild").to_owned()
}

fn host_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fl-host"))
}

fn harness() -> (tempfile::TempDir, HostEndpoint) {
    let dir = tempfile::tempdir().expect("tempdir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("mode");
    }
    let endpoint = HostEndpoint::unix(dir.path().join("h.sock")).expect("endpoint");
    (dir, endpoint)
}

fn base_opts(endpoint: &HostEndpoint) -> OpenOptions {
    let mut opts = OpenOptions::new(HostConfig::new(endpoint.clone()));
    opts.supervisor_bin = host_bin();
    opts
}

fn open_host(endpoint: &HostEndpoint) -> Host {
    Host::open(base_opts(endpoint)).expect("open")
}

fn spec(session: &str, args: &[&str]) -> SpawnSpec {
    SpawnSpec {
        session: SessionId::new(session).expect("session"),
        kind: SpawnKind::Pipe,
        argv: std::iter::once(child_bin())
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .collect(),
        cwd: std::env::temp_dir(),
        env: BTreeMap::new(),
        env_allow: BTreeSet::new(),
    }
}

fn read_until(mut file: impl Read + Send + 'static, needle: &str, timeout: Duration) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    let needle = needle.to_owned();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match file.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    buf.push(byte[0]);
                    if buf
                        .windows(needle.len())
                        .any(|window| window == needle.as_bytes())
                    {
                        if byte[0] != b'\n' {
                            loop {
                                match file.read(&mut byte) {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {
                                        buf.push(byte[0]);
                                        if byte[0] == b'\n' {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
        let _ = tx.send(buf);
    });
    let buf = rx.recv_timeout(timeout).expect("readiness");
    String::from_utf8_lossy(&buf).into_owned()
}

fn child_pid(text: &str) -> u32 {
    let marker = "CHILD ";
    let start = text.find(marker).expect("child pid");
    let digits: String = text[start + marker.len()..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    digits.parse().expect("pid")
}

fn same_process(pid: u32, start: u64) -> bool {
    matches!(process_identity(pid), Ok(ident) if ident.pid == pid && ident.start == start)
}

fn wait_gone(pid: u32, start: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !same_process(pid, start) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn kill_if_same(pid: u32, start: u64) {
    if same_process(pid, start) {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
}

struct Tracked {
    pid: u32,
    start: u64,
    reaper: Option<std::thread::JoinHandle<()>>,
}

impl Tracked {
    fn spawn(mode: &str) -> (Self, String) {
        let mut cmd = Command::new(child_bin());
        cmd.arg(mode).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().expect("canary");
        let stdout = child.stdout.take().expect("stdout");
        let needle = if mode == "grandchild" {
            " END"
        } else {
            "READY"
        };
        let text = read_until(stdout, needle, Duration::from_secs(5));
        let pid = child.id();
        let start = process_identity(pid).expect("identity").start;
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });
        (
            Self {
                pid,
                start,
                reaper: Some(reaper),
            },
            text,
        )
    }

    fn foreign(pid: u32) -> Self {
        let start = process_identity(pid).expect("identity").start;
        Self {
            pid,
            start,
            reaper: None,
        }
    }

    fn alive(&self) -> bool {
        same_process(self.pid, self.start)
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        kill_if_same(self.pid, self.start);
        if let Some(reaper) = self.reaper.take() {
            let _ = reaper.join();
        }
    }
}

fn survivor(session: &str, epoch: u64, pid: u32, start: u64) -> SurvivorRecord {
    SurvivorRecord {
        session: session.to_owned(),
        epoch,
        phase: SessionPhase::Running,
        supervisor_pid: 0,
        supervisor_start: 0,
        vendor_pid: pid,
        vendor_start: start,
        pgid: pid,
        group_leader_pid: pid,
        group_leader_start: start,
        group_leader_reaped: false,
        leader_code: None,
        leader_signal: None,
    }
}

struct Spawned {
    epoch: u64,
    vendor_pid: u32,
    vendor_start: u64,
    supervisor_pid: u32,
    supervisor_start: u64,
    #[allow(dead_code)]
    pgid: u32,
}

struct Worker {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Worker {
    fn start(endpoint: &HostEndpoint, extra: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(host_bin());
        cmd.arg("--containment-worker")
            .env("FL_HOST_SOCK", endpoint.socket_path())
            .env("FL_HOST_SUPERVISOR", host_bin())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in extra {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().expect("worker");
        let mut worker = Self {
            stdout: BufReader::new(child.stdout.take().expect("stdout")),
            stdin: child.stdin.take().expect("stdin"),
            child,
        };
        let ready = worker.line().expect("worker ready");
        assert_eq!(ready, "ready", "{ready}");
        worker
    }

    fn line(&mut self) -> Option<String> {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).ok()?;
        if n == 0 {
            None
        } else {
            Some(line.trim_end().to_owned())
        }
    }

    fn cmd(&mut self, command: &str) -> String {
        writeln!(self.stdin, "{command}").expect("write");
        self.stdin.flush().expect("flush");
        self.line().unwrap_or_default()
    }

    fn spawn_spec(&mut self, spec: &SpawnSpec) -> Spawned {
        let line = self.cmd(&format!(
            "spawn {}",
            serde_json::to_string(spec).expect("json")
        ));
        assert!(line.starts_with("ok "), "{line}");
        let mut fields = line.split_whitespace();
        fields.next();
        let _session = fields.next().expect("session");
        Spawned {
            epoch: fields.next().expect("epoch").parse().expect("epoch"),
            vendor_pid: fields.next().expect("vendor").parse().expect("pid"),
            vendor_start: fields.next().expect("start").parse().expect("start"),
            supervisor_pid: fields.next().expect("supervisor").parse().expect("pid"),
            supervisor_start: fields
                .next()
                .expect("supervisor start")
                .parse()
                .expect("start"),
            pgid: fields.next().expect("pgid").parse().expect("pgid"),
        }
    }

    fn sigkill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn stdout_text(session: &SessionHandle) -> String {
    let bytes = session
        .read_stdout_until(b"isatty", Duration::from_secs(5))
        .expect("report");
    String::from_utf8_lossy(&bytes).into_owned()
}

fn report_value(text: &str) -> serde_json::Value {
    let line = text
        .lines()
        .find(|line| line.starts_with('{'))
        .expect("report json");
    serde_json::from_str(line).expect("parse report")
}

#[cfg(target_os = "linux")]
fn linux_fds(pid: u32) -> Vec<(String, u32)> {
    let dir = format!("/proc/{pid}/fd");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let target = std::fs::read_link(entry.path())
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let info =
            std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{name}")).unwrap_or_default();
        let mut mode = 0u32;
        for line in info.lines() {
            if let Some(rest) = line.strip_prefix("flags:") {
                mode = u32::from_str_radix(rest.trim(), 8).unwrap_or(0) & 3;
            }
        }
        out.push((target, mode));
    }
    out
}

#[test]
fn argv_is_not_shell_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("keep-me");
    std::fs::write(&marker, b"safe").expect("marker");
    let (_root, endpoint) = harness();
    let host = open_host(&endpoint);
    let shell = format!("rm -rf {}", marker.display());
    let spec = spec(
        "argv",
        &["report", "ok; rm -rf /", "$(id)", "a && b", &shell],
    );
    let session = host.spawn(&spec).expect("spawn");
    let value = report_value(&stdout_text(&session));
    assert_eq!(value["isatty"], false);
    let argv = value["argv"].as_array().expect("argv");
    let args: Vec<&str> = argv.iter().filter_map(|item| item.as_str()).collect();
    assert_eq!(args[1], "report");
    assert_eq!(args[2], "ok; rm -rf /");
    assert_eq!(args[3], "$(id)");
    assert_eq!(args[4], "a && b");
    assert_eq!(args[5], shell);
    assert_eq!(std::fs::read(&marker).expect("kept"), b"safe");
    let _ = CleanupReport { complete: true };
}

#[test]
fn env_is_explicit_and_scrubbed() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let session = host.spawn(&spec("env", &["report"])).expect("spawn");
    let value = report_value(&stdout_text(&session));
    assert_eq!(value["isatty"], false);
    assert!(value["env"]["PATH"].is_string());
    assert!(value["env"]["HOME"].is_string());
    assert!(value["env"].get("CARGO_MANIFEST_DIR").is_none());
    let mut allowed = spec("env-allow", &["report"]);
    allowed.env.insert("PATH".into(), "/only-sentinel".into());
    allowed.env.insert("FL_HOST_SENTINEL".into(), "kept".into());
    allowed.env_allow.insert("PATH".into());
    allowed.env_allow.insert("FL_HOST_SENTINEL".into());
    let session = host.spawn(&allowed).expect("allowed");
    let value = report_value(&stdout_text(&session));
    assert_eq!(value["env"]["PATH"], "/only-sentinel");
    assert_eq!(value["env"]["FL_HOST_SENTINEL"], "kept");
    assert!(value["env"]["HOME"].is_string());
    assert!(value["env"].get("CARGO_MANIFEST_DIR").is_none());
    let mut denied = spec("env-denied", &["stall"]);
    denied.env.insert("FL_HOST_SENTINEL".into(), "nope".into());
    let err = host.spawn(&denied).expect_err("denied");
    assert!(err.to_string().contains("disallowed env override"), "{err}");
    assert!(host.session_phase(&denied.session).is_none());
    assert_eq!(host.epoch_high_water(), 2);
}

#[test]
fn bad_cwd_spawns_nothing() {
    let (dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let mut spec = spec("bad-cwd", &["stall"]);
    spec.cwd = dir.path().join("missing-cwd");
    let err = host.spawn(&spec).expect_err("cwd");
    assert!(host.session_phase(&spec.session).is_none(), "{err}");
    assert!(host.epoch_high_water() >= 1);
    assert!(
        read_registry(host.endpoint())
            .expect("registry")
            .sessions
            .is_empty()
    );
}

#[test]
fn exit_code_is_preserved() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let session = host.spawn(&spec("exit", &["exit", "41"])).expect("spawn");
    let exit = session.wait_leader(Duration::from_secs(5)).expect("leader");
    assert_eq!(exit.code, Some(41));
    assert_eq!(exit.signal, None);
    assert!(matches!(session.state(), ChildState::LeaderExited(_)));
}

#[test]
fn kill_only_one_of_two_sessions() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let first = host.spawn(&spec("one", &["stall"])).expect("one");
    let second = host.spawn(&spec("two", &["stall"])).expect("two");
    first
        .read_stdout_until(b"READY", Duration::from_secs(5))
        .expect("ready");
    second
        .read_stdout_until(b"READY", Duration::from_secs(5))
        .expect("ready");
    let err = host.kill(
        first.session(),
        SessionEpoch(first.epoch().0.wrapping_sub(1)),
    );
    assert!(err.expect_err("stale").to_string().contains("stale epoch"));
    assert!(same_process(first.vendor_pid(), first.vendor_start()));
    host.kill(first.session(), first.epoch()).expect("kill");
    let report = first
        .wait_cleanup(CLEANUP_DEADLINE + Duration::from_secs(2))
        .expect("cleanup");
    assert!(report.complete);
    assert!(wait_gone(
        first.vendor_pid(),
        first.vendor_start(),
        Duration::from_secs(2)
    ));
    assert!(same_process(second.vendor_pid(), second.vendor_start()));
    assert!(matches!(second.state(), ChildState::Running(_)));
}

#[test]
fn immediate_grandchild_is_contained() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let session = host.spawn(&spec("grand", &["grandchild"])).expect("spawn");
    let text = session
        .read_stdout_until(b" END", Duration::from_secs(5))
        .expect("child");
    let text = String::from_utf8_lossy(&text);
    let pid = child_pid(&text);
    let start = process_identity(pid).expect("child identity").start;
    session
        .read_stdout_until(b"READY", Duration::from_secs(1))
        .expect("ready");
    host.kill(session.session(), session.epoch()).expect("kill");
    let report = session
        .wait_cleanup(CLEANUP_DEADLINE + Duration::from_secs(2))
        .expect("cleanup");
    assert!(report.complete);
    assert!(wait_gone(
        session.vendor_pid(),
        session.vendor_start(),
        Duration::from_secs(2)
    ));
    assert!(
        wait_gone(pid, start, Duration::from_secs(2)),
        "grandchild escaped the group"
    );
}

#[test]
fn leader_exit_keeps_cleanup_owner() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let session = host.spawn(&spec("orphan", &["orphan"])).expect("spawn");
    let text = session
        .read_stdout_until(b" END", Duration::from_secs(5))
        .expect("child");
    let pid = child_pid(&String::from_utf8_lossy(&text));
    let start = process_identity(pid).expect("grandchild").start;
    let exit = session.wait_leader(Duration::from_secs(5)).expect("leader");
    assert_eq!(exit.code, Some(0));
    assert!(matches!(session.state(), ChildState::LeaderExited(_)));
    assert_eq!(
        host.session_phase(session.session()),
        Some(SessionPhase::LeaderExited)
    );
    let registry = read_registry(host.endpoint()).expect("registry");
    assert!(registry.sessions.iter().any(|record| {
        record.session == session.session().as_str() && record.phase == SessionPhase::LeaderExited
    }));
    let supervisor_start = process_identity(session.supervisor_pid())
        .expect("supervisor")
        .start;
    assert!(same_process(pid, start));
    session.request_cleanup().expect("cleanup");
    let report = session
        .wait_cleanup(CLEANUP_DEADLINE + Duration::from_secs(2))
        .expect("report");
    assert!(report.complete);
    assert!(wait_gone(
        session.supervisor_pid(),
        supervisor_start,
        Duration::from_secs(2)
    ));
    #[cfg(target_os = "linux")]
    assert!(
        wait_gone(pid, start, Duration::from_secs(2)),
        "adopted orphan was not reaped"
    );
    #[cfg(target_os = "macos")]
    {
        // No subreaper: a reparented grandchild is a residual. Do not claim adoption.
        if same_process(pid, start) {
            kill_if_same(pid, start);
        }
    }
}

#[test]
fn daemon_death_obeys_platform_contract() {
    let (_dir, endpoint) = harness();
    let mut worker = Worker::start(&endpoint, &[]);
    let spawned = worker.spawn_spec(&spec("death", &["survive-eof"]));
    let started = Instant::now();
    worker.sigkill();
    assert!(
        wait_gone(
            spawned.vendor_pid,
            spawned.vendor_start,
            CLEANUP_DEADLINE + Duration::from_secs(2)
        ),
        "stdin-EOF survivor outlived daemon death"
    );
    assert!(started.elapsed() < CLEANUP_DEADLINE + Duration::from_secs(2));
    assert!(wait_gone(
        spawned.supervisor_pid,
        spawned.supervisor_start,
        CLEANUP_DEADLINE + Duration::from_secs(2)
    ));
}

#[cfg(unix)]
#[test]
fn detached_grandchild_obeys_platform_residual() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let session = host.spawn(&spec("detach", &["detach"])).expect("spawn");
    let text = session
        .read_stdout_until(b" END", Duration::from_secs(5))
        .expect("child");
    let pid = child_pid(&String::from_utf8_lossy(&text));
    let ident = process_identity(pid).expect("identity");
    host.kill(session.session(), session.epoch()).expect("kill");
    let _ = session.wait_cleanup(CLEANUP_DEADLINE + Duration::from_secs(2));
    #[cfg(target_os = "linux")]
    {
        assert!(
            wait_gone(pid, ident.start, CLEANUP_DEADLINE + Duration::from_secs(1)),
            "linux subreaper left a detached grandchild"
        );
    }
    #[cfg(target_os = "macos")]
    {
        // proc_pidinfo does not pin the process. launchd adoption is the residual.
        if let Ok(now) = process_identity(pid) {
            assert_eq!(now.start, ident.start);
            assert_ne!(now.ppid, session.supervisor_pid());
            kill_if_same(pid, ident.start);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_adopted_orphans_stay_with_their_session() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let alpha = host.spawn(&spec("alpha", &["orphan"])).expect("alpha");
    let beta = host.spawn(&spec("beta", &["orphan"])).expect("beta");
    let alpha_text = alpha
        .read_stdout_until(b" END", Duration::from_secs(5))
        .expect("alpha child");
    let beta_text = beta
        .read_stdout_until(b" END", Duration::from_secs(5))
        .expect("beta child");
    let alpha_pid = child_pid(&String::from_utf8_lossy(&alpha_text));
    let beta_pid = child_pid(&String::from_utf8_lossy(&beta_text));
    let alpha_start = process_identity(alpha_pid).expect("alpha identity").start;
    let beta_start = process_identity(beta_pid).expect("beta identity").start;
    alpha
        .wait_leader(Duration::from_secs(5))
        .expect("alpha leader");
    beta.wait_leader(Duration::from_secs(5))
        .expect("beta leader");
    host.kill(alpha.session(), alpha.epoch())
        .expect("kill alpha");
    assert!(
        alpha
            .wait_cleanup(CLEANUP_DEADLINE + Duration::from_secs(2))
            .expect("cleanup")
            .complete
    );
    assert!(wait_gone(alpha_pid, alpha_start, Duration::from_secs(2)));
    assert!(same_process(beta_pid, beta_start));
    assert!(matches!(beta.state(), ChildState::LeaderExited(_)));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_known_and_adopted_exits_report_exact_status_once() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let session = host.spawn(&spec("dual", &["dual-exit"])).expect("spawn");
    let leader = session.wait_leader(Duration::from_secs(5)).expect("leader");
    assert_eq!(leader.code, Some(7));
    assert_eq!(leader.signal, None);
    let again = session
        .wait_leader(Duration::from_secs(1))
        .expect("same leader");
    assert_eq!(leader, again);
    let deadline = Instant::now() + Duration::from_secs(2);
    let descendant = loop {
        let found = session
            .descendants()
            .into_iter()
            .find(|exit| exit.code == Some(3));
        if let Some(found) = found {
            break found;
        }
        if Instant::now() >= deadline {
            panic!("missing adopted status {:?}", session.descendants());
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    assert_eq!(descendant.signal, None);
    assert_eq!(
        session
            .descendants()
            .iter()
            .filter(|exit| exit.code == Some(3))
            .count(),
        1
    );
    assert!(
        session
            .descendants()
            .iter()
            .all(|exit| exit.pid != session.vendor_pid())
    );
}

#[cfg(unix)]
#[test]
fn term_ignoring_child_is_killed_within_deadline() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let session = host.spawn(&spec("term", &["term-ignore"])).expect("spawn");
    session
        .read_stdout_until(b"READY", Duration::from_secs(5))
        .expect("ready");
    let started = Instant::now();
    session.request_cleanup().expect("cleanup");
    let exit = session
        .wait_leader(CLEANUP_DEADLINE + Duration::from_secs(2))
        .expect("leader");
    assert!(started.elapsed() < CLEANUP_DEADLINE);
    assert_eq!(exit.signal, Some(9));
    assert_eq!(exit.code, None);
    assert!(
        session
            .wait_cleanup(Duration::from_secs(2))
            .expect("report")
            .complete
    );
}

#[cfg(unix)]
#[test]
fn concurrent_exit_and_kill_preserve_identity() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let session = host.spawn(&spec("race", &["exit", "0"])).expect("spawn");
    let other = session.clone();
    let killer = std::thread::spawn(move || other.request_cleanup());
    let exit = session.wait_leader(Duration::from_secs(5)).expect("leader");
    killer.join().expect("join").expect("kill");
    assert_eq!(
        session.wait_leader(Duration::from_secs(1)).expect("again"),
        exit
    );
    assert!(
        session
            .descendants()
            .iter()
            .all(|item| item.pid != session.vendor_pid())
    );
    let _ = session.wait_cleanup(CLEANUP_DEADLINE + Duration::from_secs(2));
    assert!(matches!(
        session.state(),
        ChildState::LeaderExited(_) | ChildState::CleanupComplete(_)
    ));
}

#[cfg(unix)]
#[test]
fn inherited_control_writer_cannot_delay_daemon_death_cleanup() {
    let (_dir, endpoint) = harness();
    let mut worker = Worker::start(&endpoint, &[]);
    let spawned = worker.spawn_spec(&spec("inherit", &["survive-eof"]));
    #[cfg(target_os = "linux")]
    {
        let control = std::fs::read_link(format!("/proc/{}/fd/3", spawned.supervisor_pid))
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        assert!(control.starts_with("pipe:"), "{control}");
        let vendor_has = linux_fds(spawned.vendor_pid)
            .iter()
            .any(|(target, _)| target == &control);
        assert!(!vendor_has, "vendor inherited the control pipe");
        let daemon_writes = linux_fds(worker.child.id())
            .iter()
            .any(|(target, mode)| target == &control && *mode == 1);
        assert!(daemon_writes, "daemon lost the control writer");
    }
    let started = Instant::now();
    worker.sigkill();
    assert!(wait_gone(
        spawned.vendor_pid,
        spawned.vendor_start,
        CLEANUP_DEADLINE + Duration::from_secs(1)
    ));
    assert!(started.elapsed() < CLEANUP_DEADLINE + Duration::from_secs(1));
}

#[cfg(unix)]
#[test]
fn lock_listener_client_fds_not_inherited_by_child() {
    let (_dir, endpoint) = harness();
    let host = open_host(&endpoint);
    let client = BlockingHostClient::connect(host.endpoint()).expect("client");
    let holder = host.spawn(&spec("holder", &["stall"])).expect("holder");
    holder
        .read_stdout_until(b"READY", Duration::from_secs(5))
        .expect("holder ready");
    let session = host.spawn(&spec("fds", &["fds"])).expect("fds");
    let text = session
        .read_stdout_until(b"END", Duration::from_secs(5))
        .expect("fds");
    let text = String::from_utf8_lossy(&text);
    for path in [
        host.endpoint().socket_path().display().to_string(),
        host.endpoint().lock_path().display().to_string(),
        host.endpoint().token_path().display().to_string(),
        host.endpoint().state_path().display().to_string(),
    ] {
        assert!(!text.contains(&path), "{path}\n{text}");
    }
    #[cfg(target_os = "linux")]
    {
        for (target, _) in linux_fds(std::process::id()) {
            if target.starts_with("socket:") {
                assert!(!text.contains(&target), "inherited {target}");
            }
        }
        let own = format!("pipe:[{}]", session.control_inode());
        let sibling = format!("pipe:[{}]", holder.control_inode());
        assert!(!text.contains(&own), "{text}");
        assert!(!text.contains(&sibling), "{text}");
        assert!(
            linux_fds(std::process::id())
                .iter()
                .any(|(target, mode)| target == &own && *mode == 1)
        );
        assert!(
            !linux_fds(session.vendor_pid())
                .iter()
                .any(|(target, _)| target == &own || target == &sibling)
        );
        assert!(
            !linux_fds(session.supervisor_pid())
                .iter()
                .any(|(target, mode)| target == &own && *mode == 1)
        );
        assert!(
            !linux_fds(session.supervisor_pid())
                .iter()
                .any(|(target, _)| target == &sibling)
        );
    }
    drop(client);
}

#[cfg(target_os = "linux")]
#[test]
fn pidfd_unavailable_refuses_spawn() {
    check_pidfd_support().expect("this kernel has pidfds");
    let (_dir, endpoint) = harness();
    let (canary, _) = Tracked::spawn("survive-eof");
    let mut opts = base_opts(&endpoint);
    opts.assume_no_pidfd = true;
    let host = Host::open(opts).expect("open");
    let err = host.spawn(&spec("refused", &["stall"])).expect_err("pidfd");
    assert_eq!(refusal(&err), Some(ContainmentRefusal::PidfdUnavailable));
    assert!(canary.alive());
    assert_eq!(host.epoch_high_water(), 0);
    assert!(
        host.session_phase(&SessionId::new("refused").unwrap())
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn epoch_high_water_survives_and_refuses_exhaustion() {
    let (dir, endpoint) = harness();
    write_registry_fixture(&endpoint, 7, &[]).expect("fixture");
    let host = open_host(&endpoint);
    let session = host.spawn(&spec("epoch", &["stall"])).expect("spawn");
    assert_eq!(session.epoch(), SessionEpoch(8));
    session
        .read_stdout_until(b"READY", Duration::from_secs(5))
        .expect("ready");
    let err = host
        .kill(session.session(), SessionEpoch(7))
        .expect_err("stale");
    assert!(err.to_string().contains("stale epoch"));
    assert!(same_process(session.vendor_pid(), session.vendor_start()));
    host.kill(session.session(), session.epoch()).expect("kill");
    assert!(
        session
            .wait_cleanup(CLEANUP_DEADLINE + Duration::from_secs(2))
            .expect("cleanup")
            .complete
    );
    let next = host
        .spawn(&spec("epoch-next", &["exit", "0"]))
        .expect("next");
    assert_eq!(next.epoch(), SessionEpoch(9));
    drop(host);
    assert_eq!(read_registry(&endpoint).expect("registry").high_water, 9);

    let exhausted = HostEndpoint::unix(dir.path().join("exhausted.sock")).expect("endpoint");
    write_registry_fixture(&exhausted, u64::MAX, &[]).expect("max");
    let host = open_host(&exhausted);
    let err = host
        .spawn(&spec("never", &["stall"]))
        .expect_err("exhausted");
    assert_eq!(refusal(&err), Some(ContainmentRefusal::EpochExhausted));
    assert_eq!(host.epoch_high_water(), u64::MAX);
    assert_eq!(
        read_registry(&exhausted).expect("registry").high_water,
        u64::MAX
    );
}

#[cfg(unix)]
#[test]
fn daemon_sigkill_restart_cleans_recorded_survivors() {
    let (_dir, endpoint) = harness();
    let (canary, _) = Tracked::spawn("survive-eof");
    let mut worker = Worker::start(&endpoint, &[]);
    let spawned = worker.spawn_spec(&spec("restart", &["survive-eof"]));
    worker.sigkill();
    let host = open_host(&endpoint);
    assert!(wait_gone(
        spawned.vendor_pid,
        spawned.vendor_start,
        Duration::from_secs(2)
    ));
    assert!(wait_gone(
        spawned.supervisor_pid,
        spawned.supervisor_start,
        Duration::from_secs(2)
    ));
    assert!(
        canary.alive(),
        "restart cleanup signalled an unrelated process"
    );
    assert!(
        host.session_phase(&SessionId::new("restart").unwrap())
            .is_none()
    );
    let again = host
        .spawn(&spec("restart-2", &["exit", "0"]))
        .expect("admitted");
    assert!(again.epoch().0 > spawned.epoch);
    assert!(canary.alive());
}

#[cfg(unix)]
#[test]
fn leader_exit_preserves_durable_cleanup_owner() {
    let (_dir, endpoint) = harness();
    let mut worker = Worker::start(&endpoint, &[]);
    let spawned = worker.spawn_spec(&spec("owner", &["orphan"]));
    let text = worker.cmd(&format!("stdout {} 5000 END", "owner"));
    assert!(text.contains("CHILD "), "{text}");
    let grandchild = child_pid(&text);
    let grandchild_start = process_identity(grandchild)
        .map(|ident| ident.start)
        .unwrap_or(0);
    let leader = worker.cmd("leader owner 5000");
    assert!(leader.contains("Some(0)"), "{leader}");
    let phase = worker.cmd("phase owner");
    assert_eq!(phase, "ok leader_exited");
    let before = read_registry(&endpoint).expect("registry");
    assert!(before.sessions.iter().any(|record| {
        record.session == "owner"
            && record.phase == SessionPhase::LeaderExited
            && record.epoch == spawned.epoch
    }));
    assert_eq!(before.high_water, spawned.epoch);
    worker.sigkill();
    let still = read_registry(&endpoint).expect("after death");
    assert!(
        still
            .sessions
            .iter()
            .any(|record| record.session == "owner")
    );
    let host = open_host(&endpoint);
    assert!(
        host.session_phase(&SessionId::new("owner").unwrap())
            .is_none()
    );
    assert_eq!(host.epoch_high_water(), spawned.epoch);
    assert!(wait_gone(
        spawned.supervisor_pid,
        spawned.supervisor_start,
        Duration::from_secs(2)
    ));
    kill_if_same(grandchild, grandchild_start);
}

#[cfg(unix)]
#[test]
fn stale_pid_or_identity_mismatch_never_signals() {
    let (_dir, endpoint) = harness();
    let (leader, text) = Tracked::spawn("grandchild");
    let grandchild = Tracked::foreign(child_pid(&text));
    let (stale, _) = Tracked::spawn("survive-eof");
    let (bystander, _) = Tracked::spawn("survive-eof");
    write_registry_fixture(
        &endpoint,
        2,
        &[
            survivor("proven", 1, leader.pid, leader.start),
            survivor("stale", 2, stale.pid, stale.start.wrapping_add(11)),
        ],
    )
    .expect("fixture");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let extra: Arc<Mutex<Option<Tracked>>> = Arc::new(Mutex::new(None));
    let mut opts = base_opts(&endpoint);
    let seen_hook = Arc::clone(&seen);
    let extra_hook = Arc::clone(&extra);
    opts.before_signal = Some(Arc::new(move |pid| {
        seen_hook.lock().expect("seen").push(pid);
        let mut slot = extra_hook.lock().expect("extra");
        if slot.is_none() {
            let (tracked, _) = Tracked::spawn("survive-eof");
            *slot = Some(tracked);
        }
    }));
    let started = Instant::now();
    let err = Host::open(opts).expect_err("unproven");
    assert!(started.elapsed() < STARTUP_CLEANUP_DEADLINE + Duration::from_secs(2));
    assert_eq!(refusal(&err), Some(ContainmentRefusal::IdentityUnproven));
    assert!(
        !leader.alive(),
        "proven leader was not signalled through its pidfd"
    );
    assert!(grandchild.alive(), "saved process group was signalled");
    assert!(stale.alive(), "start mismatch was signalled");
    assert!(bystander.alive());
    let seen = seen.lock().expect("seen");
    assert!(seen.contains(&leader.pid));
    assert!(!seen.contains(&stale.pid));
    assert!(!seen.contains(&grandchild.pid));
    let spawned = extra.lock().expect("extra");
    assert!(spawned.as_ref().expect("hook spawn").alive());
    assert_eq!(
        signal_anchored_group(None, 15),
        Err(ContainmentRefusal::UnanchoredGroup)
    );
    let reaped = LeaderAnchor {
        pid: bystander.pid,
        start: bystander.start,
        pgid: bystander.pid,
        reaped: true,
    };
    assert_eq!(
        signal_anchored_group(Some(&reaped), 15),
        Err(ContainmentRefusal::UnanchoredGroup)
    );
    assert!(bystander.alive());
    let registry = read_registry(&endpoint).expect("kept");
    assert!(
        registry
            .alerts
            .iter()
            .any(|alert| alert.contains("unproven ownership"))
    );
    assert!(
        registry
            .sessions
            .iter()
            .any(|record| record.session == "stale")
    );
    assert!(
        registry
            .sessions
            .iter()
            .all(|record| record.session != "proven")
    );
}

#[cfg(unix)]
#[test]
fn restart_cleanup_isolates_sibling_sessions() {
    let (dir, endpoint) = harness();
    let (alpha, _) = Tracked::spawn("survive-eof");
    let (beta, _) = Tracked::spawn("survive-eof");
    write_registry_fixture(
        &endpoint,
        2,
        &[
            survivor("alpha", 1, alpha.pid, alpha.start),
            survivor("beta", 2, beta.pid, beta.start),
        ],
    )
    .expect("fixture");
    let hold = dir.path().join("hold");
    std::fs::create_dir(&hold).expect("hold");
    let endpoint_for_open = endpoint.clone();
    let hold_for_open = hold.clone();
    let opened = std::thread::spawn(move || {
        let mut opts = base_opts(&endpoint_for_open);
        opts.recover_hold = Some(hold_for_open);
        Host::open(opts).map(|host| host.epoch_high_water())
    });
    wait_path(&hold.join("hold-alpha"), Duration::from_secs(4));
    assert!(alpha.alive());
    assert!(beta.alive());
    std::fs::write(hold.join("go-alpha"), b"1").expect("go");
    wait_path(&hold.join("hold-beta"), Duration::from_secs(4));
    assert!(!alpha.alive(), "sibling cleanup reached alpha late");
    assert!(
        beta.alive(),
        "beta was cleaned while alpha's hold was released"
    );
    std::fs::write(hold.join("go-beta"), b"1").expect("go");
    let high_water = opened.join().expect("thread").expect("recover");
    assert_eq!(high_water, 2);
    assert!(!beta.alive());
}

fn wait_path(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn spawn_record_publication_survives_crash() {
    let (dir, endpoint) = harness();
    let (canary, _) = Tracked::spawn("survive-eof");
    plant_temp(
        dir.path(),
        &survivor("planted", 1, canary.pid, canary.start),
    );
    crash_spawn(&endpoint, "before_publish");
    assert!(
        read_registry(&endpoint)
            .expect("before")
            .sessions
            .is_empty()
    );
    assert!(canary.alive());
    let host = open_host(&endpoint);
    assert!(canary.alive());
    drop(host);

    crash_spawn(&endpoint, "during_temp");
    assert!(
        read_registry(&endpoint)
            .expect("during")
            .sessions
            .is_empty()
    );
    assert!(dir_has_temp(dir.path()));
    assert!(canary.alive());
    let host = open_host(&endpoint);
    assert!(canary.alive());
    drop(host);

    let mut worker = Worker::start(&endpoint, &[("FL_HOST_CRASH_AT", "after_publish")]);
    let _ = worker.cmd(&format!(
        "spawn {}",
        serde_json::to_string(&spec("published", &["survive-eof"])).unwrap()
    ));
    wait_worker_exit(&mut worker, Duration::from_secs(5));
    let recorded = read_registry(&endpoint).expect("after");
    let record = recorded
        .sessions
        .iter()
        .find(|record| record.session == "published")
        .expect("durable record");
    let vendor_pid = record.vendor_pid;
    let vendor_start = record.vendor_start;
    let epoch = record.epoch;
    drop(recorded);
    let host = open_host(&endpoint);
    assert!(wait_gone(vendor_pid, vendor_start, Duration::from_secs(2)));
    assert!(canary.alive());
    let next = host
        .spawn(&spec("after", &["exit", "0"]))
        .expect("new spawn");
    assert!(next.epoch().0 > epoch);
    assert!(canary.alive());
}

fn plant_temp(dir: &Path, record: &SurvivorRecord) {
    let body = serde_json::json!({
        "magic": 0x464C_4831u32,
        "epoch_high_water": 1,
        "sessions": [serde_json::to_value(record).expect("record")],
        "alerts": []
    });
    std::fs::write(
        dir.join(".flh-planted"),
        serde_json::to_vec(&body).expect("json"),
    )
    .expect("plant");
}

fn dir_has_temp(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .expect("dir")
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().starts_with(".flh-"))
}

fn crash_spawn(endpoint: &HostEndpoint, point: &str) {
    let mut worker = Worker::start(endpoint, &[("FL_HOST_CRASH_AT", point)]);
    let _ = worker.cmd(&format!(
        "spawn {}",
        serde_json::to_string(&spec("crash", &["survive-eof"])).unwrap()
    ));
    wait_worker_exit(&mut worker, Duration::from_secs(5));
    let _ = CrashPoint::parse(point);
}

fn wait_worker_exit(worker: &mut Worker, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if worker.child.try_wait().expect("wait").is_some() {
            return;
        }
        assert!(Instant::now() < deadline, "worker did not crash");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[test]
fn invalid_registry_refuses_startup_within_deadline() {
    let (_dir, endpoint) = harness();
    let (canary, _) = Tracked::spawn("survive-eof");
    write_raw_state(&endpoint, b"{").expect("corrupt");
    let started = Instant::now();
    let err = Host::open(base_opts(&endpoint)).expect_err("corrupt");
    assert!(started.elapsed() < STARTUP_CLEANUP_DEADLINE);
    assert_eq!(refusal(&err), Some(ContainmentRefusal::CorruptRegistry));
    assert!(canary.alive());
}

#[cfg(windows)]
fn win_endpoint(suffix: &str) -> HostEndpoint {
    HostEndpoint::windows(suffix, &fl_host::current_user_sid().expect("sid")).expect("endpoint")
}

#[cfg(windows)]
fn win_worker(suffix: &str, extra: &[(&str, &str)]) -> (HostEndpoint, Worker) {
    let endpoint = win_endpoint(suffix);
    let worker = Worker::start(&endpoint, extra);
    (endpoint, worker)
}

#[cfg(windows)]
#[test]
fn windows_fault_barriers_leave_no_job_member() {
    let root = tempfile::tempdir().expect("temp");
    for point in ["windows_created", "windows_contained", "windows_running"] {
        let pidfile = root.path().join(format!("{point}.pid"));
        let marker = root.path().join(format!("{point}.mark"));
        let suffix = format!("barrier-{point}");
        let (_endpoint, mut worker) = win_worker(
            &suffix,
            &[
                ("FL_HOST_CRASH_AT", point),
                ("FL_HOST_PIDFILE", &pidfile.display().to_string()),
            ],
        );
        let spec = spec(
            &format!("job-{point}"),
            &["marker", &marker.display().to_string()],
        );
        let _ = worker.cmd(&format!("spawn {}", serde_json::to_string(&spec).unwrap()));
        wait_worker_exit(&mut worker, Duration::from_secs(5));
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("pidfile")
            .trim()
            .parse()
            .expect("pid");
        assert!(
            !same_process(pid, process_start_or_zero(pid)),
            "{point} left a job member"
        );
        assert!(wait_gone(pid, 0, Duration::from_secs(2)) || process_identity(pid).is_err());
        if point != "windows_running" {
            assert!(!marker.exists(), "{point} resumed the child");
        }
    }
}

#[cfg(windows)]
fn process_start_or_zero(pid: u32) -> u64 {
    process_identity(pid).map(|ident| ident.start).unwrap_or(0)
}

#[cfg(windows)]
#[test]
fn windows_spawn_failures_kill_and_wait() {
    let root = tempfile::tempdir().expect("temp");
    for inject in ["create", "membership", "resume"] {
        let pidfile = root.path().join(format!("{inject}.pid"));
        let marker = root.path().join(format!("{inject}.mark"));
        let suffix = format!("fail-{inject}");
        let (endpoint, mut worker) = win_worker(
            &suffix,
            &[
                ("FL_HOST_INJECT", inject),
                ("FL_HOST_PIDFILE", &pidfile.display().to_string()),
            ],
        );
        let spec = spec(
            &format!("fail-{inject}"),
            &["marker", &marker.display().to_string()],
        );
        let reply = worker.cmd(&format!("spawn {}", serde_json::to_string(&spec).unwrap()));
        assert!(reply.starts_with("err "), "{inject}: {reply}");
        assert!(!marker.exists(), "{inject} reached user code");
        if inject == "create" {
            assert!(!pidfile.exists());
        } else {
            let pid: u32 = std::fs::read_to_string(&pidfile)
                .expect("pid")
                .trim()
                .parse()
                .expect("pid");
            assert!(process_identity(pid).is_err(), "{inject} left the child");
        }
        if inject == "resume" {
            assert!(
                read_registry(&endpoint)
                    .expect("registry")
                    .sessions
                    .is_empty()
            );
            assert!(read_registry(&endpoint).expect("registry").high_water >= 1);
        }
    }
}

#[cfg(windows)]
#[test]
fn windows_siblings_do_not_share_jobs() {
    let suffix = format!("sib-{}", std::process::id());
    let (endpoint, mut worker) = win_worker(&suffix, &[]);
    let first = worker.spawn_spec(&spec("win-one", &["stall"]));
    let second = worker.spawn_spec(&spec("win-two", &["stall"]));
    let _ = worker.cmd("stdout win-one 5000 READY");
    let _ = worker.cmd("stdout win-two 5000 READY");
    let reply = worker.cmd(&format!("kill win-one {}", first.epoch));
    assert!(reply.starts_with("ok"), "{reply}");
    let cleanup = worker.cmd("cleanup win-one 8000");
    assert_eq!(cleanup, "ok true");
    assert!(process_identity(first.vendor_pid).is_err());
    assert!(same_process(second.vendor_pid, second.vendor_start));
    let _ = endpoint;
}

#[cfg(windows)]
#[test]
fn windows_leader_exit_keeps_job_until_accounting() {
    let suffix = format!("lead-{}", std::process::id());
    let (endpoint, mut worker) = win_worker(&suffix, &[]);
    let spawned = worker.spawn_spec(&spec("win-orphan", &["orphan"]));
    let text = worker.cmd("stdout win-orphan 5000 END");
    let child = child_pid(&text.replace("\\n", "\n"));
    let child_start = process_identity(child).expect("child").start;
    let leader = worker.cmd("leader win-orphan 5000");
    assert!(leader.starts_with("ok "), "{leader}");
    let phase = worker.cmd("phase win-orphan");
    assert_eq!(phase, "ok leader_exited");
    assert!(same_process(child, child_start));
    assert!(
        read_registry(&endpoint)
            .expect("registry")
            .sessions
            .iter()
            .any(|record| {
                record.session == "win-orphan" && record.phase == SessionPhase::LeaderExited
            })
    );
    let reply = worker.cmd(&format!("kill win-orphan {}", spawned.epoch));
    assert!(reply.starts_with("ok"), "{reply}");
    let cleanup = worker.cmd("cleanup win-orphan 8000");
    assert_eq!(cleanup, "ok true", "CleanupComplete before job accounting");
    assert!(process_identity(child).is_err());
    assert_eq!(worker.cmd("phase win-orphan"), "ok absent");
}

#[cfg(windows)]
#[test]
fn windows_grandchild_exits_on_job_kill_and_daemon_death() {
    let suffix = format!("die-{}", std::process::id());
    let (_endpoint, mut worker) = win_worker(&suffix, &[]);
    let spawned = worker.spawn_spec(&spec("win-grand", &["grandchild"]));
    let text = worker.cmd("stdout win-grand 5000 END");
    let child = child_pid(&text.replace("\\n", "\n"));
    let child_start = process_identity(child).expect("child").start;
    let reply = worker.cmd(&format!("kill win-grand {}", spawned.epoch));
    assert!(reply.starts_with("ok"), "{reply}");
    let cleanup = worker.cmd("cleanup win-grand 8000");
    assert_eq!(cleanup, "ok true");
    assert!(wait_gone(
        spawned.vendor_pid,
        spawned.vendor_start,
        Duration::from_secs(2)
    ));
    assert!(wait_gone(child, child_start, Duration::from_secs(2)));

    let suffix = format!("dead-{}", std::process::id());
    let (_endpoint, mut worker) = win_worker(&suffix, &[]);
    let spawned = worker.spawn_spec(&spec("win-death", &["grandchild"]));
    let text = worker.cmd("stdout win-death 5000 END");
    let child = child_pid(&text.replace("\\n", "\n"));
    let child_start = process_identity(child).expect("child").start;
    worker.sigkill();
    assert!(wait_gone(
        spawned.vendor_pid,
        spawned.vendor_start,
        CLEANUP_DEADLINE
    ));
    assert!(wait_gone(child, child_start, CLEANUP_DEADLINE));
}

#[test]
fn containment_types_stay_public() {
    let _ = (
        Duration::from_secs(1),
        CleanupReport { complete: false },
        ContainmentRefusal::PidfdUnavailable,
        Host::open,
        OpenOptions::new,
        SessionHandle::epoch,
        ProcessIdentity {
            pid: 1,
            ppid: 1,
            pgid: 1,
            start: 1,
        },
    );
}
