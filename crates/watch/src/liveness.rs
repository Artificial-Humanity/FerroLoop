//! Agent health from attempt heartbeats and hosted-session records.
//!
//! Every verdict is core's [`evaluate_liveness`]; this module only picks the heartbeat each agent
//! is judged by, folds the verdict into one word ([`WatchLiveness`]) and counts the words per
//! node and per project. The clock is an argument (`now_ms`), so a test can stand on either side
//! of the stale edge.
//!
//! An agent is one key: the heartbeat's `agent` (lowercased), or for a heartbeat that names no
//! agent, its record's liveness slug. Its newest heartbeat (by `heartbeat_at`, then attempt id)
//! decides. A hosted session joins the agent of its persona; a hosted session with no heartbeat
//! is listed as `missing` unless the hub has decommissioned it.

use fl_core::at::At;
use fl_core::store::{Catalog, StateStore};
use fl_core::telemetry::{
    AttemptHeartbeat, LivenessStatus, LivenessVerdict, UnverifiableReason, evaluate_liveness,
    liveness_record_slug,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::runtime::{RuntimeSnapshot, WatchHostedSession, read_runtime};
use crate::{WatchError, project_slug};

/// How to judge a heartbeat: core's stale age, the host this process runs on (a heartbeat from
/// this host is checked against its pid), and the pid probe.
pub struct LivenessOptions<'a> {
    pub stale_after_ms: u64,
    pub local_host: &'a str,
    pub pid_is_alive: &'a dyn Fn(u32) -> bool,
}

/// One word for an agent's liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchLiveness {
    /// A fresh heartbeat (and, on this host, a running pid).
    Live,
    /// The heartbeat is older than the stale age, its host is unreachable, or the wrapper itself
    /// wrote `unverifiable`.
    Stale,
    /// The heartbeat says live but its process is gone: the attempt never recorded its end.
    Dead,
    /// The attempt recorded its end.
    Exited,
    /// No heartbeat at all (a hosted session nothing has reported on).
    Missing,
}

impl WatchLiveness {
    pub fn as_str(self) -> &'static str {
        match self {
            WatchLiveness::Live => "live",
            WatchLiveness::Stale => "stale",
            WatchLiveness::Dead => "dead",
            WatchLiveness::Exited => "exited",
            WatchLiveness::Missing => "missing",
        }
    }
}

/// Fold core's verdict into one word. `Exited` is `dead` when the heartbeat still claimed to be
/// live (core found the pid gone), and `exited` when the wrapper wrote the exit itself.
pub fn classify_liveness(
    hb: Option<&AttemptHeartbeat>,
    verdict: &LivenessVerdict,
) -> WatchLiveness {
    match verdict {
        LivenessVerdict::Live { .. } => WatchLiveness::Live,
        LivenessVerdict::Unverifiable {
            reason: UnverifiableReason::MissingStatus,
            ..
        } => WatchLiveness::Missing,
        LivenessVerdict::Unverifiable { .. } => WatchLiveness::Stale,
        LivenessVerdict::Exited { .. } => {
            if hb.is_some_and(|h| h.status != LivenessStatus::Exited) {
                WatchLiveness::Dead
            } else {
                WatchLiveness::Exited
            }
        }
    }
}

/// One agent's health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchAgentHealth {
    /// The agent key (lowercased persona, or a record liveness slug).
    pub agent: String,
    pub liveness: WatchLiveness,
    /// Whether the agent is supposed to be running: its heartbeat has not recorded an exit (or,
    /// with no heartbeat, its hosted session is `running`), and the hub has not decommissioned
    /// its hosted session. Only active agents raise heartbeat alerts.
    pub active: bool,
    /// Core's verdict, verbatim.
    pub verdict: LivenessVerdict,
    /// The heartbeat's host, else the hosted session's node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The project slug (heartbeat project resolved through the catalog, else its IRI; or the
    /// hosted session's project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_age_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted_session: Option<WatchHostedSession>,
}

/// Counts of each liveness word.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchLivenessCounts {
    pub agents: u64,
    pub active: u64,
    pub live: u64,
    pub stale: u64,
    pub dead: u64,
    pub exited: u64,
    pub missing: u64,
}

impl WatchLivenessCounts {
    fn add(&mut self, a: &WatchAgentHealth) {
        self.agents += 1;
        if a.active {
            self.active += 1;
        }
        match a.liveness {
            WatchLiveness::Live => self.live += 1,
            WatchLiveness::Stale => self.stale += 1,
            WatchLiveness::Dead => self.dead += 1,
            WatchLiveness::Exited => self.exited += 1,
            WatchLiveness::Missing => self.missing += 1,
        }
    }
}

/// Counts for one node or one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchGroupHealth {
    pub key: String,
    #[serde(flatten)]
    pub counts: WatchLivenessCounts,
}

/// Health of every agent FerroWatch can see.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchHealthReport {
    pub now_ms: u64,
    pub stale_after_ms: u64,
    pub local_host: String,
    /// Sorted by agent key.
    pub agents: Vec<WatchAgentHealth>,
    pub totals: WatchLivenessCounts,
    /// Sorted by node; an agent with no node counts under `unknown`.
    pub by_node: Vec<WatchGroupHealth>,
    /// Sorted by project; an agent with no project counts under `unknown`.
    pub by_project: Vec<WatchGroupHealth>,
    pub hosted_sessions: u64,
    pub hosted_sessions_undecodable: u64,
    pub heartbeats_undecodable: u64,
}

/// Project IRI → slug, for every project the catalog lists.
pub fn project_labels<S: Catalog + ?Sized>(
    store: &S,
) -> Result<BTreeMap<String, String>, WatchError> {
    Ok(store
        .list_projects()?
        .into_iter()
        .map(|p| (p.id.as_str().to_string(), project_slug(&p.root)))
        .collect())
}

/// Read the runtime entries and the project list from `store` and judge every agent at `now_ms`.
pub fn build_health<S: StateStore + Catalog + ?Sized>(
    store: &S,
    now_ms: u64,
    opts: &LivenessOptions<'_>,
) -> Result<WatchHealthReport, WatchError> {
    let runtime = read_runtime(store)?;
    let labels = project_labels(store)?;
    Ok(evaluate_health(&runtime, &labels, now_ms, opts))
}

fn agent_key(hb: &AttemptHeartbeat) -> String {
    match hb.agent.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        Some(agent) => agent.to_ascii_lowercase(),
        None => liveness_record_slug(&hb.record),
    }
}

/// The hosted session a persona's agent row shows: a running one first, then the newest.
fn pick_hosted<'a>(sessions: &[&'a WatchHostedSession]) -> Option<&'a WatchHostedSession> {
    sessions.iter().copied().max_by(|a, b| {
        (a.is_running(), a.updated_at_ms, b.session_id.as_str()).cmp(&(
            b.is_running(),
            b.updated_at_ms,
            a.session_id.as_str(),
        ))
    })
}

/// Judge every agent in `runtime` at `now_ms`. Pure: the same input gives the same report.
pub fn evaluate_health(
    runtime: &RuntimeSnapshot,
    project_labels: &BTreeMap<String, String>,
    now_ms: u64,
    opts: &LivenessOptions<'_>,
) -> WatchHealthReport {
    let now = At::from_unix_millis(now_ms);

    // The newest heartbeat per agent key.
    let mut newest: BTreeMap<String, &AttemptHeartbeat> = BTreeMap::new();
    for hb in runtime.latest_per_attempt().into_values() {
        let key = agent_key(hb);
        match newest.get(&key) {
            Some(prev)
                if (prev.heartbeat_at.as_str(), prev.attempt_id.as_str())
                    >= (hb.heartbeat_at.as_str(), hb.attempt_id.as_str()) => {}
            _ => {
                newest.insert(key, hb);
            }
        }
    }

    let mut hosted_by_persona: BTreeMap<String, Vec<&WatchHostedSession>> = BTreeMap::new();
    for s in &runtime.hosted_sessions {
        let persona = s.persona.trim().to_ascii_lowercase();
        let key = if persona.is_empty() {
            s.session_id.clone()
        } else {
            persona
        };
        hosted_by_persona.entry(key).or_default().push(s);
    }

    let label = |iri: &str| {
        project_labels
            .get(iri)
            .cloned()
            .unwrap_or_else(|| iri.to_string())
    };
    let non_empty = |s: &str| (!s.trim().is_empty()).then(|| s.to_string());

    let mut agents = Vec::new();
    for (key, hb) in &newest {
        let hosted = hosted_by_persona
            .get(key)
            .and_then(|v| pick_hosted(v))
            .cloned();
        let verdict = evaluate_liveness(
            Some(hb),
            &now,
            opts.stale_after_ms,
            opts.local_host,
            opts.pid_is_alive,
        );
        let liveness = classify_liveness(Some(hb), &verdict);
        let active = hb.status != LivenessStatus::Exited
            && !hosted
                .as_ref()
                .is_some_and(WatchHostedSession::is_decommissioned);
        let beat = hb.heartbeat_at.unix_millis();
        agents.push(WatchAgentHealth {
            agent: key.clone(),
            liveness,
            active,
            verdict,
            node: non_empty(&hb.host)
                .or_else(|| hosted.as_ref().and_then(|h| non_empty(&h.node_id))),
            project: Some(label(hb.project.as_str())),
            adapter: non_empty(&hb.adapter),
            attempt_id: Some(hb.attempt_id.as_str().to_string()),
            record: Some(hb.record.as_str().to_string()),
            heartbeat_at_ms: Some(beat),
            heartbeat_age_ms: Some(now_ms.saturating_sub(beat)),
            hosted_session: hosted,
        });
    }
    for (key, sessions) in &hosted_by_persona {
        if newest.contains_key(key) {
            continue;
        }
        let Some(hosted) = pick_hosted(sessions) else {
            continue;
        };
        if hosted.is_decommissioned() {
            continue;
        }
        let verdict = evaluate_liveness(
            None,
            &now,
            opts.stale_after_ms,
            opts.local_host,
            opts.pid_is_alive,
        );
        agents.push(WatchAgentHealth {
            agent: key.clone(),
            liveness: classify_liveness(None, &verdict),
            active: hosted.is_running(),
            verdict,
            node: non_empty(&hosted.node_id),
            project: non_empty(&hosted.project),
            adapter: non_empty(&hosted.vendor),
            attempt_id: None,
            record: None,
            heartbeat_at_ms: None,
            heartbeat_age_ms: None,
            hosted_session: Some(hosted.clone()),
        });
    }
    agents.sort_by(|a, b| a.agent.cmp(&b.agent));

    let mut totals = WatchLivenessCounts::default();
    let mut by_node: BTreeMap<String, WatchLivenessCounts> = BTreeMap::new();
    let mut by_project: BTreeMap<String, WatchLivenessCounts> = BTreeMap::new();
    for a in &agents {
        totals.add(a);
        by_node
            .entry(a.node.clone().unwrap_or_else(|| "unknown".into()))
            .or_default()
            .add(a);
        by_project
            .entry(a.project.clone().unwrap_or_else(|| "unknown".into()))
            .or_default()
            .add(a);
    }
    let groups = |m: BTreeMap<String, WatchLivenessCounts>| {
        m.into_iter()
            .map(|(key, counts)| WatchGroupHealth { key, counts })
            .collect()
    };

    WatchHealthReport {
        now_ms,
        stale_after_ms: opts.stale_after_ms,
        local_host: opts.local_host.to_string(),
        agents,
        totals,
        by_node: groups(by_node),
        by_project: groups(by_project),
        hosted_sessions: runtime.hosted_sessions.len() as u64,
        hosted_sessions_undecodable: runtime.hosted_sessions_undecodable,
        heartbeats_undecodable: runtime.heartbeats_undecodable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::HeartbeatEntry;
    use crate::testkit::{heartbeat, put_runtime};
    use fl_core::MemStore;
    use fl_core::log::AttemptStatus;
    use serde_json::json;

    const STALE: u64 = 30_000;
    const BEAT: u64 = 1_800_000_000_000;

    fn alive(_: u32) -> bool {
        true
    }
    fn gone(_: u32) -> bool {
        false
    }

    fn opts<'a>(host: &'a str, pid: &'a dyn Fn(u32) -> bool) -> LivenessOptions<'a> {
        LivenessOptions {
            stale_after_ms: STALE,
            local_host: host,
            pid_is_alive: pid,
        }
    }

    fn snap(hbs: Vec<AttemptHeartbeat>, hosted: Vec<WatchHostedSession>) -> RuntimeSnapshot {
        RuntimeSnapshot {
            heartbeats: hbs
                .into_iter()
                .enumerate()
                .map(|(i, heartbeat)| HeartbeatEntry {
                    slug: format!("liveness-agent-{i}"),
                    heartbeat,
                })
                .collect(),
            hosted_sessions: hosted,
            ..RuntimeSnapshot::default()
        }
    }

    fn hosted(id: &str, persona: &str, status: &str, updated: u64) -> WatchHostedSession {
        WatchHostedSession {
            session_id: id.into(),
            persona: persona.into(),
            project: "ferroloop".into(),
            vendor: id.split(':').next().unwrap_or("").into(),
            node_id: "ai-lab-0".into(),
            status: status.into(),
            updated_at_ms: updated,
            ..WatchHostedSession::default()
        }
    }

    fn one(r: &WatchHealthReport) -> &WatchAgentHealth {
        assert_eq!(r.agents.len(), 1, "{:?}", r.agents);
        &r.agents[0]
    }

    #[test]
    fn remote_heartbeat_at_the_clock_edges() {
        let s = snap(
            vec![heartbeat("a", "Ada", "remote", BEAT, LivenessStatus::Live)],
            vec![],
        );
        let labels = BTreeMap::new();
        let o = opts("here", &alive);
        // Exactly at the stale age: still live. One millisecond past: stale (host unavailable).
        let at = evaluate_health(&s, &labels, BEAT + STALE, &o);
        assert_eq!(one(&at).liveness, WatchLiveness::Live);
        assert_eq!(one(&at).heartbeat_age_ms, Some(STALE));
        assert_eq!(one(&at).agent, "ada");
        let past = evaluate_health(&s, &labels, BEAT + STALE + 1, &o);
        assert_eq!(one(&past).liveness, WatchLiveness::Stale);
        assert!(matches!(
            one(&past).verdict,
            LivenessVerdict::Unverifiable {
                reason: UnverifiableReason::HostUnavailable,
                ..
            }
        ));
        // A clock behind the heartbeat is age zero, not an underflow.
        let early = evaluate_health(&s, &labels, BEAT - 5, &o);
        assert_eq!(one(&early).liveness, WatchLiveness::Live);
        assert_eq!(one(&early).heartbeat_age_ms, Some(0));
    }

    #[test]
    fn local_heartbeat_checks_the_pid_and_goes_stale_or_dead() {
        let s = snap(
            vec![heartbeat("a", "ada", "here", BEAT, LivenessStatus::Live)],
            vec![],
        );
        let labels = BTreeMap::new();
        let live = evaluate_health(&s, &labels, BEAT + STALE, &opts("here", &alive));
        assert_eq!(one(&live).liveness, WatchLiveness::Live);
        let stale = evaluate_health(&s, &labels, BEAT + STALE + 1, &opts("here", &alive));
        assert_eq!(one(&stale).liveness, WatchLiveness::Stale);
        // The pid is gone: dead at once, fresh or not.
        let dead = evaluate_health(&s, &labels, BEAT + 1, &opts("here", &gone));
        assert_eq!(one(&dead).liveness, WatchLiveness::Dead);
        assert!(one(&dead).active);
        assert!(matches!(
            one(&dead).verdict,
            LivenessVerdict::Exited {
                outcome: AttemptStatus::Crashed,
                ..
            }
        ));
    }

    #[test]
    fn a_recorded_exit_is_exited_and_inactive_and_unverifiable_is_stale() {
        let mut exited = heartbeat("a", "ada", "here", BEAT, LivenessStatus::Exited);
        exited.outcome = Some(AttemptStatus::Completed);
        let unv = heartbeat("b", "bo", "remote", BEAT, LivenessStatus::Unverifiable);
        let r = evaluate_health(
            &snap(vec![exited, unv], vec![]),
            &BTreeMap::new(),
            BEAT,
            &opts("here", &gone),
        );
        assert_eq!(r.agents[0].agent, "ada");
        assert_eq!(r.agents[0].liveness, WatchLiveness::Exited);
        assert!(!r.agents[0].active);
        assert_eq!(r.agents[1].liveness, WatchLiveness::Stale);
        assert!(r.agents[1].active);
        assert_eq!(r.totals.exited, 1);
        assert_eq!(r.totals.stale, 1);
        assert_eq!(r.totals.active, 1);
    }

    #[test]
    fn the_newest_heartbeat_per_agent_decides_and_agentless_heartbeats_key_by_record() {
        let old = heartbeat("a1", "ada", "remote", BEAT - 100_000, LivenessStatus::Live);
        let new = heartbeat("a2", "ADA", "remote", BEAT, LivenessStatus::Live);
        let anon = heartbeat("a3", "", "remote", BEAT, LivenessStatus::Live);
        let r = evaluate_health(
            &snap(vec![old, new, anon.clone()], vec![]),
            &BTreeMap::new(),
            BEAT + 1,
            &opts("here", &alive),
        );
        assert_eq!(r.agents.len(), 2);
        assert_eq!(r.agents[0].agent, "ada");
        assert_eq!(r.agents[0].attempt_id.as_deref(), Some("urn:fl:attempt:a2"));
        assert_eq!(r.agents[0].liveness, WatchLiveness::Live);
        assert_eq!(r.agents[1].agent, liveness_record_slug(&anon.record));
    }

    #[test]
    fn hosted_sessions_join_their_persona_or_stand_alone_as_missing() {
        let hb = heartbeat("a", "ada", "remote", BEAT, LivenessStatus::Live);
        let sessions = vec![
            hosted("claude:ada", "Ada", "running", 5),
            hosted("codex:ada", "ada", "decommissioned", 9),
            hosted("muse:cy", "cy", "running", 1),
            hosted("grok:di", "di", "decommissioned", 1),
            hosted("codex:ed", "ed", "decommissioning", 1),
        ];
        let r = evaluate_health(
            &snap(vec![hb], sessions),
            &BTreeMap::new(),
            BEAT + 1,
            &opts("here", &alive),
        );
        let names: Vec<_> = r.agents.iter().map(|a| a.agent.as_str()).collect();
        assert_eq!(
            names,
            vec!["ada", "cy", "ed"],
            "a decommissioned session is not listed"
        );
        // The running session wins over a newer decommissioned one.
        assert_eq!(
            r.agents[0].hosted_session.as_ref().unwrap().session_id,
            "claude:ada"
        );
        assert!(r.agents[0].active);
        assert_eq!(r.agents[1].liveness, WatchLiveness::Missing);
        assert!(r.agents[1].active);
        assert_eq!(r.agents[1].node.as_deref(), Some("ai-lab-0"));
        assert_eq!(r.agents[1].project.as_deref(), Some("ferroloop"));
        assert!(!r.agents[2].active, "decommissioning is not running");
        assert_eq!(r.hosted_sessions, 5);
    }

    #[test]
    fn a_decommissioned_session_makes_its_stale_heartbeat_inactive() {
        let hb = heartbeat("a", "ada", "remote", BEAT, LivenessStatus::Live);
        let r = evaluate_health(
            &snap(
                vec![hb],
                vec![hosted("claude:ada", "ada", "decommissioned", 1)],
            ),
            &BTreeMap::new(),
            BEAT + STALE + 1,
            &opts("here", &alive),
        );
        assert_eq!(one(&r).liveness, WatchLiveness::Stale);
        assert!(!one(&r).active);
    }

    #[test]
    fn summaries_per_node_and_project_use_catalog_slugs() {
        let a = heartbeat("a", "ada", "node-1", BEAT, LivenessStatus::Live);
        let mut b = heartbeat("b", "bo", "node-2", BEAT - 100_000, LivenessStatus::Live);
        let c = heartbeat("c", "cy", "node-1", BEAT - 100_000, LivenessStatus::Live);
        let p = a.project.as_str().to_string();
        b.project = fl_core::ids::ProjectId(fl_core::ids::seq_iri(77));
        let labels = BTreeMap::from([(p, "ferroloop".to_string())]);
        let r = evaluate_health(
            &snap(vec![a, b, c], vec![]),
            &labels,
            BEAT,
            &opts("here", &alive),
        );
        let node1 = r.by_node.iter().find(|g| g.key == "node-1").unwrap();
        assert_eq!(
            (node1.counts.agents, node1.counts.live, node1.counts.stale),
            (2, 1, 1)
        );
        let node2 = r.by_node.iter().find(|g| g.key == "node-2").unwrap();
        assert_eq!(node2.counts.stale, 1);
        let keys: Vec<_> = r.by_project.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"ferroloop"));
        let fl = r.by_project.iter().find(|g| g.key == "ferroloop").unwrap();
        assert_eq!(fl.counts.agents, 2);
        assert_eq!(r.totals.agents, 3);
    }

    #[test]
    fn build_health_reads_the_store_through_state_and_catalog_traits() {
        let store = MemStore::default();
        let pid = store.add_project("/src/FerroLoop").unwrap();
        let mut hb = heartbeat("a", "ada", "remote", BEAT, LivenessStatus::Live);
        hb.project = pid;
        put_runtime(
            &store,
            "liveness-agent-ada",
            serde_json::to_value(&hb).unwrap(),
        );
        put_runtime(
            &store,
            "liveness-record-x",
            serde_json::to_value(&hb).unwrap(),
        );
        put_runtime(
            &store,
            "hosted-session-claude-bo",
            json!({"session_id": "claude:bo", "persona": "bo", "status": "running",
                   "node_id": "ai-lab-0", "project": "ferroloop"}),
        );
        put_runtime(&store, "hosted-session-junk", json!(42));
        let r = build_health(&store, BEAT + 1, &opts("here", &alive)).unwrap();
        let names: Vec<_> = r.agents.iter().map(|a| a.agent.as_str()).collect();
        assert_eq!(names, vec!["ada", "bo"]);
        assert_eq!(r.agents[0].project.as_deref(), Some("ferroloop"));
        assert_eq!(r.by_project.len(), 1);
        assert_eq!(r.by_project[0].counts.agents, 2);
        assert_eq!(r.hosted_sessions_undecodable, 1);
        // Deterministic: the same clock gives the same report.
        let again = build_health(&store, BEAT + 1, &opts("here", &alive)).unwrap();
        assert_eq!(r, again);
    }
}
