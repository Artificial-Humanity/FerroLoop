//! `fl host` — Native `fl-host` daemon and session inspection/control subcommands.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fl_host_proto::{
    BlockingHostClient, ChildState, HostConfig, HostEndpoint, SendOutcome, SessionSummary,
};
use fl_wire::vendor_adapter::{
    host_session_id_to_unit, matching_live_host_sessions, unit_to_host_session_id,
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
        /// Maximum trailing bytes to capture.
        #[arg(long, default_value_t = 16384)]
        max_bytes: usize,
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

fn resolve_target_session<'a>(
    sessions: &'a [SessionSummary],
    target: &str,
) -> Result<&'a SessionSummary> {
    let live_ids = matching_live_host_sessions(sessions, target);
    if let Some(id) = live_ids.first()
        && let Some(found) = sessions
            .iter()
            .find(|s| &s.stream.session == id && matches!(s.state, ChildState::Running(_)))
    {
        return Ok(found);
    }
    if let Some(id) = unit_to_host_session_id(target)
        && let Some(found) = sessions.iter().find(|s| s.stream.session == id)
    {
        return Ok(found);
    }
    sessions
        .iter()
        .find(|s| s.stream.session.as_str() == target)
        .ok_or_else(|| anyhow::anyhow!("no fl-host session matching `{target}`"))
}

pub fn execute(cmd: HostCmd) -> Result<()> {
    match cmd {
        HostCmd::Serve { socket } => {
            let endpoint = match socket {
                Some(path) => fl_host::socket_endpoint(&path.to_string_lossy())
                    .context("invalid --socket path for fl-host")?,
                None => HostEndpoint::discover().context("failed to discover fl-host endpoint")?,
            };
            fl_host::serve(HostConfig::new(endpoint)).context("fl-host daemon exited with error")
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
            let sessions = client
                .list_sessions()
                .context("failed to list fl-host sessions")?;
            let active = sessions
                .iter()
                .filter(|s| matches!(s.state, ChildState::Running(_)))
                .count();
            let total = sessions.len();
            if json {
                let payload = serde_json::json!({
                    "socket": endpoint.socket_path().display().to_string(),
                    "active_sessions": active,
                    "total_sessions": total,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("fl-host: {}", endpoint.socket_path().display());
                println!("sessions: {active} active / {total} total");
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
            let sessions = client
                .list_sessions()
                .context("failed to list fl-host sessions")?;
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
        HostCmd::Capture { session, max_bytes } => {
            let endpoint =
                HostEndpoint::discover().context("failed to discover fl-host endpoint")?;
            let mut client = BlockingHostClient::connect(&endpoint).with_context(|| {
                format!(
                    "failed to connect to fl-host at {}",
                    endpoint.socket_path().display()
                )
            })?;
            let sessions = client
                .list_sessions()
                .context("failed to list fl-host sessions")?;
            let summary = resolve_target_session(&sessions, &session)?;
            let limit = u32::try_from(max_bytes)
                .unwrap_or(u32::MAX)
                .min(fl_host_proto::MAX_RAW_INPUT as u32);
            let output = client
                .capture_output(summary.stream.session.clone(), limit)
                .with_context(|| format!("failed to capture output from `{session}`"))?;
            print!("{output}");
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
            let sessions = client
                .list_sessions()
                .context("failed to list fl-host sessions")?;
            let summary = resolve_target_session(&sessions, &session)?;
            let stream = summary.stream.clone();
            let grant = client
                .claim_controller(stream.clone())
                .with_context(|| format!("failed to claim controller for `{session}`"))?;
            let outcome = if raw {
                client
                    .write_raw(stream, grant.lease, text.into_bytes())
                    .with_context(|| format!("failed to write raw bytes to `{session}`"))?
            } else {
                client
                    .send_line(stream, grant.lease, text)
                    .with_context(|| format!("failed to send line to `{session}`"))?
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
            let sessions = client
                .list_sessions()
                .context("failed to list fl-host sessions")?;
            let summary = resolve_target_session(&sessions, &session)?;
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
}
