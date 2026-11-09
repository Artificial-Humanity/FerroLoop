//! `fl host` — Native `fl-host` daemon and session inspection/control subcommands.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fl_host_proto::{
    BlockingHostClient, ChildState, HostConfig, HostEndpoint, SendOutcome, SessionSummary,
};
use fl_wire::vendor_adapter::{
    host_session_id_to_unit, lifecycle_id_to_host_session_id, matching_live_host_sessions,
    split_lifecycle_session_id, unit_to_host_session_id,
};

#[derive(Subcommand)]
pub enum HostCmd {
    /// Run the `fl-host` daemon on `--socket` or the discovered runtime socket.
    Serve {
        /// Optional Unix domain socket path override.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Show `fl-host` daemon status and session counts.
    Status {
        /// Emit status as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List sessions managed by `fl-host`.
    List {
        /// Emit sessions as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Capture recent terminal output from a session.
    Capture {
        /// Session id or `session:window` unit target.
        session: String,
        /// Maximum trailing bytes to capture (raw mode only).
        #[arg(long, default_value_t = 16384, conflicts_with = "rendered")]
        max_bytes: usize,
        /// Capture the recent raw terminal bytes (the default).
        #[arg(long, conflicts_with = "rendered")]
        raw: bool,
        /// Capture the coherent visible PTY screen.
        #[arg(long)]
        rendered: bool,
        /// Emit the full rendered snapshot and metadata as JSON.
        #[arg(long, requires = "rendered")]
        json: bool,
    },
    /// Send a line or raw bytes to a live session.
    Send {
        /// Session id or `session:window` unit target.
        session: String,
        /// Text to send.
        text: String,
        /// Send raw bytes without appending a newline.
        #[arg(long)]
        raw: bool,
    },
    /// Terminate a live session.
    Kill {
        /// Session id or `session:window` unit target.
        session: String,
        /// Force-kill the session.
        #[arg(long)]
        force: bool,
    },
}

fn session_vendor_and_state(summary: &SessionSummary) -> (String, &'static str, Option<u32>) {
    let unit = host_session_id_to_unit(&summary.stream.session);
    let vendor = match unit.split_once(':') {
        Some((s, _)) if s.eq_ignore_ascii_case("muse") => "muse".to_string(),
        Some((_, w)) => w.trim().to_ascii_lowercase(),
        None => "unknown".to_string(),
    };
    let (state, pid) = match &summary.state {
        ChildState::Running(rec) => ("running", Some(rec.pid)),
        ChildState::LeaderExited(rec) => ("leader_exited", Some(rec.pid)),
        ChildState::CleanupComplete(rec) => ("cleanup_complete", Some(rec.pid)),
    };
    (vendor, state, pid)
}

fn state_rank(state: &ChildState) -> u8 {
    match state {
        ChildState::Running(_) => 2,
        ChildState::LeaderExited(_) => 1,
        ChildState::CleanupComplete(_) => 0,
    }
}

/// Deduplicate `fl-host` session summaries by session ID, keeping the running or newest epoch
/// so superseded `cleanup_complete` epochs from prior restarts do not appear alongside a live
/// session of the same name.
pub fn dedup_session_summaries(sessions: Vec<SessionSummary>) -> Vec<SessionSummary> {
    let mut by_session: std::collections::BTreeMap<String, SessionSummary> =
        std::collections::BTreeMap::new();
    for s in sessions {
        let key = s.stream.session.as_str().to_string();
        match by_session.get(&key) {
            Some(prev)
                if (state_rank(&prev.state), prev.stream.epoch.0)
                    >= (state_rank(&s.state), s.stream.epoch.0) => {}
            _ => {
                by_session.insert(key, s);
            }
        }
    }
    by_session.into_values().collect()
}

#[cfg(test)]
fn resolve_target_session<'a>(
    sessions: &'a [SessionSummary],
    target: &str,
) -> Result<&'a SessionSummary> {
    resolve_target_session_with_agents(sessions, target, &[])
}

fn resolve_target_session_with_agents<'a>(
    sessions: &'a [SessionSummary],
    target: &str,
    agents: &[fl_core::AgentRecord],
) -> Result<&'a SessionSummary> {
    let trimmed = target.trim();
    if let Some((vendor, persona)) = split_lifecycle_session_id(trimmed) {
        if vendor == "muse" {
            let id = lifecycle_id_to_host_session_id(trimmed, &[])
                .map_err(anyhow::Error::msg)?
                .ok_or_else(|| anyhow::anyhow!("no fl-host session matching `{target}`"))?;
            return sessions
                .iter()
                .find(|s| s.stream.session == id && matches!(s.state, ChildState::Running(_)))
                .or_else(|| sessions.iter().find(|s| s.stream.session == id))
                .ok_or_else(|| anyhow::anyhow!("no fl-host session matching `{target}`"));
        }
        let lifecycle_id =
            lifecycle_id_to_host_session_id(trimmed, agents).map_err(anyhow::Error::msg)?;
        let direct_id = unit_to_host_session_id(trimmed);

        let mut case_matched_ids: Vec<&str> = sessions
            .iter()
            .filter_map(|s| {
                let (sess_persona, sess_vendor) = s.stream.session.as_str().split_once('.')?;
                (sess_vendor == vendor && sess_persona.eq_ignore_ascii_case(persona))
                    .then_some(s.stream.session.as_str())
            })
            .collect();
        case_matched_ids.sort_unstable();
        case_matched_ids.dedup();
        if case_matched_ids.len() > 1 {
            bail!(
                "ambiguous fl-host session target `{target}`: multiple sessions match persona `{persona}` on vendor `{vendor}` ({})",
                case_matched_ids.join(", ")
            );
        }

        let direct_exists = direct_id
            .as_ref()
            .is_some_and(|did| sessions.iter().any(|s| &s.stream.session == did));

        if let Some(id) = lifecycle_id {
            if direct_exists && direct_id.as_ref() != Some(&id) {
                let did = direct_id.as_ref().unwrap().as_str();
                let id_str = id.as_str();
                bail!(
                    "ambiguous fl-host session target `{target}`: matches both `{did}` and `{id_str}`"
                );
            }
            if let [only] = case_matched_ids.as_slice()
                && *only != id.as_str()
            {
                let id_str = id.as_str();
                bail!(
                    "ambiguous fl-host session target `{target}`: registry display casing `{id_str}` disagrees with session `{only}`"
                );
            }
            if let Some(found) = sessions
                .iter()
                .find(|s| s.stream.session == id && matches!(s.state, ChildState::Running(_)))
                .or_else(|| sessions.iter().find(|s| s.stream.session == id))
            {
                return Ok(found);
            }
            let id_str = id.as_str();
            bail!("no fl-host session matching `{target}` (expected `{id_str}`)");
        } else if let [only] = case_matched_ids.as_slice() {
            if direct_exists {
                let did = direct_id.as_ref().unwrap().as_str();
                if *only != did {
                    bail!(
                        "ambiguous fl-host session target `{target}`: matches both `{did}` and `{only}`"
                    );
                }
            }
            if let Some(found) = sessions
                .iter()
                .find(|s| {
                    s.stream.session.as_str() == *only && matches!(s.state, ChildState::Running(_))
                })
                .or_else(|| sessions.iter().find(|s| s.stream.session.as_str() == *only))
            {
                return Ok(found);
            }
        }
    }

    let live_ids = matching_live_host_sessions(sessions, trimmed);
    if live_ids.len() > 1 {
        let mut names: Vec<&str> = live_ids
            .iter()
            .map(fl_host_proto::SessionId::as_str)
            .collect();
        names.sort_unstable();
        names.dedup();
        if names.len() > 1 {
            bail!(
                "ambiguous fl-host session target `{target}`: matches multiple live sessions ({})",
                names.join(", ")
            );
        }
    }
    if let Some(id) = live_ids.first()
        && let Some(found) = sessions
            .iter()
            .find(|s| &s.stream.session == id && matches!(s.state, ChildState::Running(_)))
    {
        return Ok(found);
    }
    if let Some(id) = unit_to_host_session_id(trimmed)
        && let Some(found) = sessions.iter().find(|s| s.stream.session == id)
    {
        return Ok(found);
    }
    sessions
        .iter()
        .find(|s| s.stream.session.as_str() == trimmed)
        .ok_or_else(|| anyhow::anyhow!("no fl-host session matching `{target}`"))
}

fn load_registry_agents(db: Option<&Path>) -> Result<Vec<fl_core::AgentRecord>> {
    let store_path = match crate::explicit_db(db.map(Path::to_path_buf)) {
        Some(p) => Some(p),
        None => {
            let default = crate::default_state_store_path().ok();
            match crate::config::load(crate::config::path().as_deref()) {
                Ok(cfg) => match default {
                    Some(d) => Some(cfg.workspace_state_store(&d)),
                    None => cfg.state.as_ref().and_then(|s| s.store.clone()),
                },
                Err(_) => default,
            }
        }
    };
    let Some(path) = store_path else {
        return Ok(Vec::new());
    };
    if !fl_store::store_exists(&path) {
        return Ok(Vec::new());
    }
    let store = fl_store::open_store(&path)
        .with_context(|| format!("could not open the store at {}", path.display()))?;
    use fl_core::AgentRegistry;
    store
        .list_agents(None, true)
        .with_context(|| format!("could not list agents from {}", path.display()))
}

fn resolve_target_session_from_store<'a>(
    sessions: &'a [SessionSummary],
    target: &str,
    db: Option<&Path>,
) -> Result<&'a SessionSummary> {
    let agents = if split_lifecycle_session_id(target).is_some_and(|(vendor, _)| vendor != "muse") {
        load_registry_agents(db)?
    } else {
        Vec::new()
    };
    resolve_target_session_with_agents(sessions, target, &agents)
}

pub fn execute_with_db(cmd: HostCmd, db: Option<&Path>) -> Result<()> {
    match cmd {
        HostCmd::Serve { socket } => {
            let endpoint = match socket {
                Some(path) => fl_host::socket_endpoint(&path.to_string_lossy())
                    .context("invalid --socket path for fl-host")?,
                None => HostEndpoint::discover().context("failed to discover fl-host endpoint")?,
            };
            let build = crate::version::build_info();
            fl_host::serve_with_build_info(
                HostConfig::new(endpoint),
                fl_host_proto::HostBuildInfo {
                    version: build.version,
                    commit: build.commit,
                    dirty: build.dirty,
                    build_time_unix: build.build_time_unix,
                },
            )
            .context("fl-host daemon exited with error")
        }
        HostCmd::Status { json } => {
            let endpoint =
                HostEndpoint::discover().context("failed to discover fl-host endpoint")?;
            let mut client = BlockingHostClient::connect(&endpoint).with_context(|| {
                format!(
                    "failed to connect to fl-host at {}",
                    endpoint.socket_path().display()
                )
            })?;
            let build = client
                .host_status()
                .context("failed to query fl-host build identity")?;
            let (active, total) = if client.negotiated().1 == 0 {
                // host_status sends no request at minor zero, so this remains the
                // first request. These hosts cannot decode ListSessions either.
                let response = client
                    .request(fl_host_proto::HostRequest::List {
                        id: fl_host_proto::RequestId(1),
                    })
                    .context("failed to list fl-host sessions")?;
                let fl_host_proto::HostResponse::Listed { children, .. } = response else {
                    bail!("unexpected fl-host session response");
                };
                (
                    children
                        .iter()
                        .filter(|s| matches!(s.state, ChildState::Running(_)))
                        .count(),
                    children.len(),
                )
            } else {
                let sessions = dedup_session_summaries(
                    client
                        .list_sessions()
                        .context("failed to list fl-host sessions")?,
                );
                (
                    sessions
                        .iter()
                        .filter(|s| matches!(s.state, ChildState::Running(_)))
                        .count(),
                    sessions.len(),
                )
            };
            if json {
                let payload = serde_json::json!({
                    "socket": endpoint.socket_path().display().to_string(),
                    "active_sessions": active,
                    "total_sessions": total,
                    "build_identity": if build.is_some() { "available" } else { "unavailable" },
                    "build": build,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("fl-host: {}", endpoint.socket_path().display());
                println!("sessions: {active} active / {total} total");
                match build {
                    Some(build) => {
                        println!("build: {} ({})", build.version, build.commit);
                        println!(
                            "dirty: {}",
                            build
                                .dirty
                                .map_or_else(|| "unknown".into(), |v| v.to_string())
                        );
                        println!(
                            "build time (unix): {}",
                            build
                                .build_time_unix
                                .map_or_else(|| "unknown".into(), |v| v.to_string())
                        );
                    }
                    None => println!("build identity: unavailable (older daemon)"),
                }
            }
            Ok(())
        }
        HostCmd::List { json } => {
            let endpoint =
                HostEndpoint::discover().context("failed to discover fl-host endpoint")?;
            let mut client = BlockingHostClient::connect(&endpoint).with_context(|| {
                format!(
                    "failed to connect to fl-host at {}",
                    endpoint.socket_path().display()
                )
            })?;
            let sessions = dedup_session_summaries(
                client
                    .list_sessions()
                    .context("failed to list fl-host sessions")?,
            );
            if json {
                let rows: Vec<_> = sessions
                    .iter()
                    .map(|s| {
                        let (vendor, state, pid) = session_vendor_and_state(s);
                        serde_json::json!({
                            "id": s.stream.session.as_str(),
                            "unit": host_session_id_to_unit(&s.stream.session),
                            "vendor": vendor,
                            "state": state,
                            "pid": pid,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if sessions.is_empty() {
                println!("No sessions in fl-host.");
            } else {
                println!("{:<24} {:<14} {:<18} PID", "ID", "VENDOR", "STATE");
                for s in &sessions {
                    let (vendor, state, pid) = session_vendor_and_state(s);
                    let pid_str = pid.map_or_else(|| "-".to_string(), |p| p.to_string());
                    println!(
                        "{:<24} {:<14} {:<18} {}",
                        s.stream.session.as_str(),
                        vendor,
                        state,
                        pid_str
                    );
                }
            }
            Ok(())
        }
        HostCmd::Capture {
            session,
            max_bytes,
            raw: _,
            rendered,
            json,
        } => {
            let endpoint =
                HostEndpoint::discover().context("failed to discover fl-host endpoint")?;
            let mut client = BlockingHostClient::connect(&endpoint).with_context(|| {
                format!(
                    "failed to connect to fl-host at {}",
                    endpoint.socket_path().display()
                )
            })?;
            let sessions = dedup_session_summaries(
                client
                    .list_sessions()
                    .context("failed to list fl-host sessions")?,
            );
            let summary = resolve_target_session_from_store(&sessions, &session, db)?;
            if rendered {
                let screen = client
                    .capture_rendered(summary.stream.clone())
                    .with_context(|| {
                        format!("failed to capture rendered screen from `{session}`")
                    })?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&screen)?);
                } else {
                    print!("{}", screen.contents);
                }
            } else {
                let limit = u32::try_from(max_bytes)
                    .unwrap_or(u32::MAX)
                    .min(fl_host_proto::MAX_RAW_INPUT as u32);
                let output = client
                    .capture_output(summary.stream.session.clone(), limit)
                    .with_context(|| format!("failed to capture output from `{session}`"))?;
                print!("{output}");
            }
            Ok(())
        }
        HostCmd::Send { session, text, raw } => {
            let endpoint =
                HostEndpoint::discover().context("failed to discover fl-host endpoint")?;
            let mut client = BlockingHostClient::connect(&endpoint).with_context(|| {
                format!(
                    "failed to connect to fl-host at {}",
                    endpoint.socket_path().display()
                )
            })?;
            let sessions = dedup_session_summaries(
                client
                    .list_sessions()
                    .context("failed to list fl-host sessions")?,
            );
            let summary = resolve_target_session_from_store(&sessions, &session, db)?;
            let stream = summary.stream.clone();
            let grant = client
                .claim_controller(stream.clone())
                .with_context(|| format!("failed to claim controller for `{session}`"))?;
            let outcome = if raw {
                client
                    .write_raw(stream, grant.lease, text.into_bytes())
                    .with_context(|| format!("failed to write raw bytes to `{session}`"))?
            } else {
                if !text.is_empty() {
                    let outcome = client
                        .write_raw(stream.clone(), grant.lease, text.into_bytes())
                        .with_context(|| format!("failed to write text to `{session}`"))?;
                    if !matches!(outcome, SendOutcome::Written) {
                        bail!("send to `{session}` returned {outcome:?}");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                client
                    .write_raw(stream, grant.lease, b"\r".to_vec())
                    .with_context(|| format!("failed to send Enter to `{session}`"))?
            };
            if !matches!(outcome, SendOutcome::Written) {
                bail!("send to `{session}` returned {outcome:?}");
            }
            Ok(())
        }
        HostCmd::Kill {
            session,
            force: _force,
        } => {
            let endpoint =
                HostEndpoint::discover().context("failed to discover fl-host endpoint")?;
            let mut client = BlockingHostClient::connect(&endpoint).with_context(|| {
                format!(
                    "failed to connect to fl-host at {}",
                    endpoint.socket_path().display()
                )
            })?;
            let sessions = dedup_session_summaries(
                client
                    .list_sessions()
                    .context("failed to list fl-host sessions")?,
            );
            let summary = resolve_target_session_from_store(&sessions, &session, db)?;
            let stream = summary.stream.clone();
            let grant = client
                .claim_controller(stream.clone())
                .with_context(|| format!("failed to claim controller for `{session}`"))?;
            client
                .kill_session(stream, grant.lease)
                .with_context(|| format!("failed to kill `{session}`"))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_host_proto::{
        ChildRecord, HostInstance, InputState, SessionEpoch, SessionId, SessionMode, SpawnKind,
        StreamId,
    };

    fn make_summary(id: &str, pid: u32) -> SessionSummary {
        let sid = SessionId::new(id).unwrap();
        SessionSummary {
            stream: StreamId {
                host: HostInstance::new([7u8; 16]),
                session: sid.clone(),
                epoch: SessionEpoch(1),
            },
            mode: SessionMode::ReliablePipe,
            state: ChildState::Running(ChildRecord {
                pid,
                session: sid,
                session_epoch: SessionEpoch(1),
                kind: SpawnKind::Pipe,
                input: InputState::Writable,
                started_at_ms: 1000,
                exit: None,
                consumer_stalled: false,
            }),
            acked: 0,
            latest: 0,
            input: InputState::Writable,
        }
    }

    #[test]
    fn resolves_and_classifies_all_seven_vendor_sessions() {
        let sessions = vec![
            make_summary("Ada.claude", 101),
            make_summary("Bea.codex", 102),
            make_summary("Cy.antigravity", 103),
            make_summary("Dee.grok", 104),
            make_summary("muse.eve", 105),
            make_summary("Fay.lemonade", 106),
            make_summary("Gus.ollama", 107),
        ];

        assert_eq!(
            session_vendor_and_state(&sessions[0]),
            ("claude".to_string(), "running", Some(101))
        );
        assert_eq!(
            session_vendor_and_state(&sessions[4]),
            ("muse".to_string(), "running", Some(105))
        );
        assert_eq!(
            session_vendor_and_state(&sessions[5]),
            ("lemonade".to_string(), "running", Some(106))
        );
        assert_eq!(
            session_vendor_and_state(&sessions[6]),
            ("ollama".to_string(), "running", Some(107))
        );

        let resolved_unit = resolve_target_session(&sessions, "Fay:lemonade").unwrap();
        assert_eq!(resolved_unit.stream.session.as_str(), "Fay.lemonade");

        let resolved_id = resolve_target_session(&sessions, "Gus.ollama").unwrap();
        assert_eq!(resolved_id.stream.session.as_str(), "Gus.ollama");

        let resolved_muse = resolve_target_session(&sessions, "muse:eve").unwrap();
        assert_eq!(resolved_muse.stream.session.as_str(), "muse.eve");
    }

    #[test]
    fn dedup_session_summaries_drops_superseded_cleanup_complete_epochs() {
        use fl_host_proto::ChildExit;
        let sid = SessionId::new("Feira.antigravity").unwrap();
        let old_cleanup = SessionSummary {
            stream: StreamId {
                host: HostInstance::new([7u8; 16]),
                session: sid.clone(),
                epoch: SessionEpoch(1),
            },
            mode: SessionMode::Pty,
            state: ChildState::CleanupComplete(ChildRecord {
                session: sid.clone(),
                session_epoch: SessionEpoch(1),
                kind: SpawnKind::Pty,
                pid: 1111,
                input: InputState::Fenced,
                started_at_ms: 100,
                exit: Some(ChildExit {
                    code: Some(0),
                    signal: None,
                }),
                consumer_stalled: false,
            }),
            acked: 0,
            latest: 0,
            input: InputState::Fenced,
        };
        let mut new_running = make_summary("Feira.antigravity", 2270049);
        new_running.stream.epoch = SessionEpoch(2);
        if let ChildState::Running(ref mut rec) = new_running.state {
            rec.session_epoch = SessionEpoch(2);
        }

        let deduped = dedup_session_summaries(vec![old_cleanup.clone(), new_running.clone()]);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].stream.epoch, SessionEpoch(2));
        assert!(matches!(deduped[0].state, ChildState::Running(_)));

        // Reversed input order still keeps the running epoch.
        let deduped_rev = dedup_session_summaries(vec![new_running, old_cleanup]);
        assert_eq!(deduped_rev.len(), 1);
        assert_eq!(deduped_rev[0].stream.epoch, SessionEpoch(2));
        assert!(matches!(deduped_rev[0].state, ChildState::Running(_)));
    }

    #[test]
    fn resolves_lifecycle_vendor_persona_id_via_registry_display_casing_and_refuses_ambiguity() {
        use fl_core::{AgentId, AgentRecord, AgentRole, AgentStatus, At, Iri};

        let make_agent = |id: u64, name_key: &str, display_name: &str| AgentRecord {
            id: AgentId(Iri::parse(&format!("urn:test:agent:{id}")).unwrap()),
            name_key: name_key.to_string(),
            display_name: display_name.to_string(),
            email: format!("{name_key}@artificialhumanity.io"),
            project: "kaggle-gemma4".to_string(),
            role: AgentRole::Journalist,
            extra_areas: Vec::new(),
            status: AgentStatus::Active,
            updated_at: At::from_unix_millis(1),
            by: "owner".to_string(),
        };

        let agents = vec![
            make_agent(1, "kage", "Kage"),
            make_agent(2, "jarvis", "JARVIS"),
        ];
        let sessions = vec![
            make_summary("Kage.codex", 201),
            make_summary("JARVIS.claude", 202),
            make_summary("muse.eve", 203),
        ];

        let kage = resolve_target_session_with_agents(&sessions, "codex:kage", &agents).unwrap();
        assert_eq!(kage.stream.session.as_str(), "Kage.codex");
        let kage_mixed =
            resolve_target_session_with_agents(&sessions, "CODEX:Kage", &agents).unwrap();
        assert_eq!(kage_mixed.stream.session.as_str(), "Kage.codex");

        let jarvis =
            resolve_target_session_with_agents(&sessions, "claude:jarvis", &agents).unwrap();
        assert_eq!(jarvis.stream.session.as_str(), "JARVIS.claude");

        let muse = resolve_target_session_with_agents(&sessions, "muse:eve", &agents).unwrap();
        assert_eq!(muse.stream.session.as_str(), "muse.eve");
        let muse_mixed = resolve_target_session_with_agents(&sessions, "Muse:Eve", &[]).unwrap();
        assert_eq!(muse_mixed.stream.session.as_str(), "muse.eve");

        // Unregistered persona with a single unambiguous matching session resolves via fallback.
        let kage_unregistered =
            resolve_target_session_with_agents(&sessions, "codex:kage", &[]).unwrap();
        assert_eq!(kage_unregistered.stream.session.as_str(), "Kage.codex");
        let kage_upper_unregistered =
            resolve_target_session_with_agents(&sessions, "codex:Kage", &[]).unwrap();
        assert_eq!(
            kage_upper_unregistered.stream.session.as_str(),
            "Kage.codex"
        );

        // Unregistered persona with no matching session errors.
        let err = resolve_target_session_with_agents(&sessions, "codex:nobody", &[]).unwrap_err();
        assert!(
            err.to_string()
                .contains("no fl-host session matching `codex:nobody`"),
            "{err}"
        );

        // Multiple case-colliding host sessions refuse on ambiguity (with or without registry).
        let colliding_sessions = vec![
            make_summary("Kage.codex", 201),
            make_summary("kage.codex", 202),
        ];
        let err = resolve_target_session_with_agents(&colliding_sessions, "codex:kage", &agents)
            .unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
        let err =
            resolve_target_session_with_agents(&colliding_sessions, "codex:kage", &[]).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");

        // Direct `<vendor>.<persona>` alongside `<Persona>.<vendor>` refuses on ambiguity.
        let direct_and_lifecycle = vec![
            make_summary("Kage.codex", 201),
            make_summary("codex.kage", 202),
        ];
        let err = resolve_target_session_with_agents(&direct_and_lifecycle, "codex:kage", &agents)
            .unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
        let err = resolve_target_session_with_agents(&direct_and_lifecycle, "codex:kage", &[])
            .unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");

        // Session casing disagreeing with registry display casing refuses on ambiguity.
        let wrong_case = vec![make_summary("Jarvis.claude", 201)];
        let err =
            resolve_target_session_with_agents(&wrong_case, "claude:jarvis", &agents).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");

        // Bare persona matching multiple live vendor sessions refuses on ambiguity.
        let multi_live = vec![
            make_summary("Kage.codex", 201),
            make_summary("Kage.claude", 202),
        ];
        let err = resolve_target_session_with_agents(&multi_live, "Kage", &agents).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
    }

    #[test]
    fn resolve_target_session_from_store_reads_agent_registry() {
        use fl_core::{AgentRegistry, AgentRole, At, RegisterAgentArgs};

        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("fl.fls");
        let store = fl_store::open_store(&db_path).unwrap();
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Kage".to_string(),
                email: None,
                project: "kaggle-gemma4".to_string(),
                role: AgentRole::Journalist,
                extra_areas: Vec::new(),
                create_area: true,
                replace: false,
                at: At::from_unix_millis(100),
                by: "owner".to_string(),
            })
            .unwrap();
        drop(store);

        let sessions = vec![make_summary("Kage.codex", 301)];
        let resolved =
            resolve_target_session_from_store(&sessions, "codex:kage", Some(&db_path)).unwrap();
        assert_eq!(resolved.stream.session.as_str(), "Kage.codex");
    }
}
