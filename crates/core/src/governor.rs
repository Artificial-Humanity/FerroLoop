//! Shared gateway admission governor (Record #97 §2).
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

pub const GOVERNANCE_CALLS: &[&str] = &["warrant_revoke_check", "hub_lifecycle_read"];
/// Default per-scope budget in charged units (response bytes on gate paths).
pub const DEFAULT_BUDGET_UNITS: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Principal {
    Owner,
    Warrant(String),
    Bundle(String),
}
impl Principal {
    pub fn scope_string(&self) -> String {
        match self {
            Self::Owner => "principal:owner".into(),
            Self::Warrant(id) => format!("principal:warrant:{id}"),
            Self::Bundle(id) => format!("principal:bundle:{id}"),
        }
    }
    pub fn is_owner(&self) -> bool {
        matches!(self, Self::Owner)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EndpointAlias {
    pub connector_id: String,
    pub tool: String,
    pub host: String,
    pub path_prefix: String,
}

fn normalize_host(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    let h = h.trim_end_matches('.');
    // strip default ports only
    if let Some(stripped) = h.strip_suffix(":443") {
        stripped.to_string()
    } else if let Some(stripped) = h.strip_suffix(":80") {
        stripped.to_string()
    } else {
        h.to_string()
    }
}
fn normalize_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let p = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if p.len() > 1 {
        p.trim_end_matches('/').to_string()
    } else {
        p
    }
}
pub fn canonical_endpoint(
    connector: Option<&str>,
    tool: Option<&str>,
    url_host: &str,
    path: &str,
) -> String {
    let host = normalize_host(url_host);
    if host.is_empty() {
        if let (Some(c), Some(t)) = (connector, tool) {
            return format!("connector:{c}/{t}");
        }
        return normalize_path(path);
    }
    format!("{}{}", host, normalize_path(path))
}
pub fn resolve_canonical_endpoint(
    aliases: &[EndpointAlias],
    connector: Option<&str>,
    tool: Option<&str>,
    url_host: &str,
    path: &str,
) -> String {
    if let (Some(c), Some(t)) = (connector, tool)
        && let Some(a) = aliases.iter().find(|a| a.connector_id == c && a.tool == t)
    {
        return format!(
            "{}{}",
            normalize_host(&a.host),
            normalize_path(&a.path_prefix)
        );
    }
    // Raw path: an alias matches only on a segment boundary (the path is
    // the prefix itself or extends it past a `/`), and the canonical form
    // is the alias prefix, exactly as on the connector path.
    let host = normalize_host(url_host);
    if !host.is_empty() {
        let norm_path = normalize_path(path);
        for a in aliases {
            if normalize_host(&a.host) != host {
                continue;
            }
            let prefix = normalize_path(&a.path_prefix);
            if prefix.is_empty() {
                continue;
            }
            if norm_path == prefix || norm_path.starts_with(&format!("{prefix}/")) {
                return format!("{host}{prefix}");
            }
        }
        return format!("{host}{norm_path}");
    }
    canonical_endpoint(connector, tool, url_host, path)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ScopeKey {
    PrincipalScope(Principal),
    ProjectScope(String),
    EndpointScope(String),
    ProjectEndpointScope { project: String, endpoint: String },
}
impl ScopeKey {
    pub fn scope_string(&self) -> String {
        match self {
            Self::PrincipalScope(p) => p.scope_string(),
            Self::ProjectScope(p) => format!("project:{p}"),
            Self::EndpointScope(e) => format!("endpoint:{e}"),
            Self::ProjectEndpointScope { project, endpoint } => {
                format!("project_endpoint:{project}|{endpoint}")
            }
        }
    }
}
pub fn scopes_for(principal: &Principal, project: &str, endpoint: &str) -> Vec<ScopeKey> {
    vec![
        ScopeKey::PrincipalScope(principal.clone()),
        ScopeKey::ProjectScope(project.to_string()),
        ScopeKey::EndpointScope(endpoint.to_string()),
        ScopeKey::ProjectEndpointScope {
            project: project.to_string(),
            endpoint: endpoint.to_string(),
        },
    ]
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScopePolicy {
    pub concurrency_per_scope: Option<u32>,
    pub rate_max_per_window: Option<u64>,
    pub budget_units_per_scope: Option<u64>,
}
fn default_deadline_window_ms() -> u64 {
    60_000
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GovernorPolicy {
    pub concurrency_per_scope: u32,
    pub rate_window_ms: u64,
    pub rate_max_per_window: u64,
    pub budget_units_per_scope: u64,
    pub freshness_max_age_ms: u64,
    pub replay_guard_window_ms: u64,
    /// Lifetime of a permit: `deadline_mono_ms = admission now + this window`.
    #[serde(default = "default_deadline_window_ms")]
    pub deadline_window_ms: u64,
    pub per_scope: BTreeMap<String, ScopePolicy>,
}
impl Default for GovernorPolicy {
    fn default() -> Self {
        Self {
            concurrency_per_scope: 4,
            rate_window_ms: 60_000,
            rate_max_per_window: 100,
            budget_units_per_scope: DEFAULT_BUDGET_UNITS,
            freshness_max_age_ms: 5_000,
            replay_guard_window_ms: 60_000,
            deadline_window_ms: default_deadline_window_ms(),
            per_scope: BTreeMap::new(),
        }
    }
}
impl GovernorPolicy {
    fn concurrency_for(&self, scope: &str) -> u32 {
        self.per_scope
            .get(scope)
            .and_then(|p| p.concurrency_per_scope)
            .unwrap_or(self.concurrency_per_scope)
    }
    fn rate_for(&self, scope: &str) -> u64 {
        self.per_scope
            .get(scope)
            .and_then(|p| p.rate_max_per_window)
            .unwrap_or(self.rate_max_per_window)
    }
    fn budget_for(&self, scope: &str) -> u64 {
        self.per_scope
            .get(scope)
            .and_then(|p| p.budget_units_per_scope)
            .unwrap_or(self.budget_units_per_scope)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowState {
    pub window_start_ms: u64,
    pub cap_at_first_observation: u64,
    pub used: u64,
}
/// Pure window logic: returns (new_state, admitted). Frozen cap never raised within window.
pub fn window_admit(
    state: Option<WindowState>,
    now_ms: u64,
    window_ms: u64,
    current_cap: u64,
    units: u64,
) -> (WindowState, bool) {
    match state {
        Some(s) if window_ms > 0 && now_ms.saturating_sub(s.window_start_ms) < window_ms => {
            if s.used.saturating_add(units) <= s.cap_at_first_observation {
                (
                    WindowState {
                        used: s.used + units,
                        ..s
                    },
                    true,
                )
            } else {
                (s, false)
            }
        }
        _ => {
            let admitted = units <= current_cap;
            (
                WindowState {
                    window_start_ms: now_ms,
                    cap_at_first_observation: current_cap,
                    used: if admitted { units } else { 0 },
                },
                admitted,
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogicalOperationId {
    pub digest: String,
    pub principal: Principal,
    pub project: String,
    pub endpoint_set_digest: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RecordedOutcome {
    Executed,
    UnknownAfterSend,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SideEffectClass {
    SideEffectful,
    IdempotentWithKey,
}
fn default_side_effect_class() -> SideEffectClass {
    SideEffectClass::SideEffectful
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplayRecord {
    pub operation: LogicalOperationId,
    pub outcome: Option<RecordedOutcome>,
    pub original_key: Option<String>,
    /// Side-effect class recorded at dispatch time (trusted caller), never
    /// re-derived from a retry's own claims.
    #[serde(default = "default_side_effect_class")]
    pub side_effect_class: SideEffectClass,
    pub endpoint: String,
    pub recorded_at_ms: u64,
    pub dispatching: bool,
    /// Distinctly marks a dispatch recorded under the Owner-fresh path.
    #[serde(default)]
    pub owner_fresh_action: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayAttempt {
    pub operation: LogicalOperationId,
    pub endpoint: String,
    pub original_key: Option<String>,
    pub side_effect_class: SideEffectClass,
    /// Only an Owner attempt with this set bypasses the replay guard.
    pub owner_fresh_action: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayDecision {
    Allowed,
    AllowedOwnerFresh,
    AllowedReconciliation,
    Refused { reason: String },
}
pub fn replay_decision(
    record: &ReplayRecord,
    attempt: &ReplayAttempt,
    now_ms: u64,
    guard_window_ms: u64,
) -> ReplayDecision {
    if guard_window_ms > 0 && now_ms.saturating_sub(record.recorded_at_ms) > guard_window_ms {
        return ReplayDecision::Allowed;
    }
    // dispatching without outcome = uncertain
    let uncertain = record.dispatching && record.outcome.is_none();
    if !uncertain && record.outcome.is_none() {
        return ReplayDecision::Allowed;
    }
    // Same logical operation (same endpoint-set id) aimed at a different
    // endpoint than the recorded dispatch is an endpoint switch, never a
    // replay or reconciliation of that dispatch.
    if attempt.endpoint != record.endpoint {
        return ReplayDecision::Refused {
            reason: "endpoint_switch_refused".into(),
        };
    }
    // Owner bypass only for an explicitly fresh action; otherwise an Owner
    // replay is refused like everyone else's. Principal mismatch is not a
    // replay of this record (lookup normally filters it out).
    if attempt.operation.principal.is_owner() && attempt.owner_fresh_action {
        return ReplayDecision::AllowedOwnerFresh;
    }
    if record.operation.principal != attempt.operation.principal {
        return ReplayDecision::Allowed;
    }
    // Reconciliation is only for a dispatch whose outcome is unknown after
    // send, recorded (at dispatch time) as idempotent-with-key, retried
    // with the original key. An `Executed` record never replays.
    if record.outcome == Some(RecordedOutcome::UnknownAfterSend)
        && record.side_effect_class == SideEffectClass::IdempotentWithKey
        && attempt.side_effect_class == SideEffectClass::IdempotentWithKey
        && attempt.original_key.is_some()
        && attempt.original_key == record.original_key
        && !uncertain
    {
        return ReplayDecision::AllowedReconciliation;
    }
    ReplayDecision::Refused {
        reason: if uncertain {
            "uncertain_dispatching_without_outcome".into()
        } else {
            "replay_guard_refused".into()
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionRequest {
    pub principal: Principal,
    pub caller_agent: String,
    pub project: String,
    pub endpoint: String,
    pub requested_units: u64,
    pub quota_observed_at_ms: Option<u64>,
    pub governance_call: Option<String>,
    pub call_name: Option<String>,
    /// Explicit fresh Owner action: the only path that bypasses the replay guard.
    pub owner_fresh_action: bool,
    /// Logical-operation digest for the replay guard; `None` skips the guard.
    pub operation_digest: Option<String>,
    /// Explicit logical endpoint-set id for the replay guard. It is NOT
    /// the per-attempt endpoint: attempts in one set share this id, and a
    /// change of the concrete `endpoint` within the set is refused.
    pub endpoint_set_digest: String,
    pub original_key: Option<String>,
    pub side_effect_class: SideEffectClass,
}
impl AdmissionRequest {
    pub fn new(principal: Principal, project: &str, endpoint: &str, units: u64) -> Self {
        Self {
            principal,
            caller_agent: String::new(),
            project: project.into(),
            endpoint: endpoint.into(),
            requested_units: units,
            quota_observed_at_ms: None,
            governance_call: None,
            call_name: None,
            owner_fresh_action: false,
            operation_digest: None,
            endpoint_set_digest: String::new(),
            original_key: None,
            side_effect_class: SideEffectClass::SideEffectful,
        }
    }
    /// Build the replay-guard attempt for this request, if it carries a digest.
    pub fn replay_attempt(&self) -> Option<ReplayAttempt> {
        self.operation_digest.as_ref().map(|digest| ReplayAttempt {
            operation: LogicalOperationId {
                digest: digest.clone(),
                principal: self.principal.clone(),
                project: self.project.clone(),
                endpoint_set_digest: self.endpoint_set_digest.clone(),
            },
            endpoint: self.endpoint.clone(),
            original_key: self.original_key.clone(),
            side_effect_class: self.side_effect_class,
            owner_fresh_action: self.owner_fresh_action,
        })
    }
    fn is_governance(&self) -> bool {
        let name = self
            .governance_call
            .as_deref()
            .or(self.call_name.as_deref());
        matches!(name, Some(n) if GOVERNANCE_CALLS.contains(&n))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Permit {
    pub id: String,
    pub scopes: Vec<String>,
    pub reserved_units: u64,
    pub deadline_mono_ms: u64,
    pub governance_lane: bool,
    pub confirmed_dispatched: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionOutcome {
    Admitted(Permit),
    Deferred { retry_after_ms: u64, reason: String },
    Denied { reason: String },
    AdmissionUnavailable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettledOutcome {
    Executed,
    UnknownAfterSend,
    NeverDispatched,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleRequest {
    pub permit_id: String,
    pub actual_units: u64,
    pub outcome: SettledOutcome,
    pub complete: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayRecordAction {
    Dispatching,
    Outcome(RecordedOutcome),
    /// Remove the matching dispatching record: the dispatch never happened.
    ClearDispatching,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayRequest {
    pub attempt: ReplayAttempt,
    pub now_ms: u64,
    pub record: Option<ReplayRecordAction>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GovernorError {
    #[error("unknown permit: {0}")]
    UnknownPermit(String),
    #[error("governor unavailable")]
    Unavailable,
    #[error("invalid request: {0}")]
    Invalid(String),
}

pub trait GovernorStore: Send + Sync {
    fn reserve(
        &self,
        req: &AdmissionRequest,
        now_ms: u64,
    ) -> Result<AdmissionOutcome, GovernorError>;
    fn settle(&self, req: &SettleRequest) -> Result<(), GovernorError>;
    fn record_and_check_replay(&self, req: &ReplayRequest)
    -> Result<ReplayDecision, GovernorError>;
    fn unconfirmed_permits(&self) -> Vec<Permit>;
    /// Durable failover journal backend (Runtime slug `failover-journal`
    /// in persistent impls). Default: unavailable.
    fn load_failover_journal(
        &self,
    ) -> Result<Vec<crate::failover::AttemptJournalEntry>, GovernorError> {
        Err(GovernorError::Unavailable)
    }
    fn save_failover_journal(
        &self,
        _entries: &[crate::failover::AttemptJournalEntry],
    ) -> Result<(), GovernorError> {
        Err(GovernorError::Unavailable)
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScopeState {
    pub window: Option<WindowState>,
    pub budget_used: u64,
    pub budget_reserved: u64,
    pub concurrency_live: u32,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PermitRecord {
    pub permit: Permit,
    pub completed: bool,
    pub settled: bool,
}
struct Inner {
    policy: GovernorPolicy,
    scopes: HashMap<String, ScopeState>,
    permits: HashMap<String, PermitRecord>,
    replay: Vec<ReplayRecord>,
    next_id: u64,
}
pub struct MemGovernorStore {
    inner: Mutex<Inner>,
    failover_journal: Mutex<Vec<crate::failover::AttemptJournalEntry>>,
}
impl MemGovernorStore {
    pub fn new(policy: GovernorPolicy) -> Self {
        Self {
            inner: Mutex::new(Inner {
                policy,
                scopes: HashMap::new(),
                permits: HashMap::new(),
                replay: Vec::new(),
                next_id: 1,
            }),
            failover_journal: Mutex::new(Vec::new()),
        }
    }
    pub fn update_policy(&self, f: impl FnOnce(&mut GovernorPolicy)) {
        let mut g = self.inner.lock().unwrap();
        f(&mut g.policy);
    }
    pub fn budget_used_for(&self, scope: &str) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .scopes
            .get(scope)
            .map(|s| s.budget_used)
            .unwrap_or(0)
    }
    // helpers used by tests / journal integration
    pub fn record_dispatching(&self, attempt: ReplayAttempt, now_ms: u64) {
        let _ = self.record_and_check_replay(&ReplayRequest {
            attempt,
            now_ms,
            record: Some(ReplayRecordAction::Dispatching),
        });
    }
    pub fn clear_dispatching(&self, attempt: ReplayAttempt, now_ms: u64) {
        let _ = self.record_and_check_replay(&ReplayRequest {
            attempt,
            now_ms,
            record: Some(ReplayRecordAction::ClearDispatching),
        });
    }
    pub fn record_outcome(&self, attempt: ReplayAttempt, outcome: RecordedOutcome, now_ms: u64) {
        let _ = self.record_and_check_replay(&ReplayRequest {
            attempt,
            now_ms,
            record: Some(ReplayRecordAction::Outcome(outcome)),
        });
    }
    pub fn check_replay(
        &self,
        attempt: ReplayAttempt,
        now_ms: u64,
    ) -> Result<ReplayDecision, GovernorError> {
        self.record_and_check_replay(&ReplayRequest {
            attempt,
            now_ms,
            record: None,
        })
    }
}
/// Serializable snapshot of a [`MemGovernorStore`]'s full state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GovernorSnapshot {
    pub policy: GovernorPolicy,
    pub scopes: HashMap<String, ScopeState>,
    pub permits: HashMap<String, PermitRecord>,
    pub replay: Vec<ReplayRecord>,
    pub next_id: u64,
}

impl MemGovernorStore {
    pub fn snapshot(&self) -> GovernorSnapshot {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        GovernorSnapshot {
            policy: g.policy.clone(),
            scopes: g.scopes.clone(),
            permits: g.permits.clone(),
            replay: g.replay.clone(),
            next_id: g.next_id,
        }
    }
    pub fn from_snapshot(snap: GovernorSnapshot) -> Self {
        Self {
            inner: Mutex::new(Inner {
                policy: snap.policy,
                scopes: snap.scopes,
                permits: snap.permits,
                replay: snap.replay,
                next_id: snap.next_id,
            }),
            failover_journal: Mutex::new(Vec::new()),
        }
    }
    pub fn snapshot_json(&self) -> String {
        serde_json::to_string(&self.snapshot()).unwrap_or_default()
    }
    /// Parse a snapshot; corrupt input is an error, never a silent reset.
    pub fn try_from_snapshot_json(json: &str) -> Result<Self, GovernorError> {
        let snap: GovernorSnapshot =
            serde_json::from_str(json).map_err(|_| GovernorError::Unavailable)?;
        Ok(Self::from_snapshot(snap))
    }
    #[deprecated = "use try_from_snapshot_json; corrupt snapshots must not reset"]
    pub fn from_snapshot_json(json: &str, fallback_policy: GovernorPolicy) -> Self {
        Self::try_from_snapshot_json(json).unwrap_or_else(|_| Self::new(fallback_policy))
    }
}

impl Default for MemGovernorStore {
    fn default() -> Self {
        Self::new(GovernorPolicy::default())
    }
}

impl GovernorStore for MemGovernorStore {
    fn reserve(
        &self,
        req: &AdmissionRequest,
        now_ms: u64,
    ) -> Result<AdmissionOutcome, GovernorError> {
        let mut g = self.inner.lock().map_err(|_| GovernorError::Unavailable)?;
        // freshness
        if let Some(obs) = req.quota_observed_at_ms
            && g.policy.freshness_max_age_ms > 0
            && now_ms.saturating_sub(obs) > g.policy.freshness_max_age_ms
        {
            return Ok(AdmissionOutcome::Deferred {
                retry_after_ms: g.policy.freshness_max_age_ms,
                reason: "stale_quota".into(),
            });
        }
        let governance = req.is_governance();
        let scope_keys = scopes_for(&req.principal, &req.project, &req.endpoint);
        let scope_strs: Vec<String> = scope_keys.iter().map(|s| s.scope_string()).collect();
        // pre-check all scopes atomically (no mutation on failure except window creation handled after)
        // concurrency
        for s in &scope_strs {
            let cap = g.policy.concurrency_for(s);
            let live = g.scopes.get(s).map(|x| x.concurrency_live).unwrap_or(0);
            if cap > 0 && live >= cap {
                return Ok(AdmissionOutcome::Deferred {
                    retry_after_ms: 100,
                    reason: format!("concurrency_exhausted:{s}"),
                });
            }
        }
        if !governance {
            // rate pre-check using frozen cap logic (peek)
            for s in &scope_strs {
                let cap = g.policy.rate_for(s);
                if let Some(w) = g.scopes.get(s).and_then(|x| x.window.clone()) {
                    if g.policy.rate_window_ms > 0
                        && now_ms.saturating_sub(w.window_start_ms) < g.policy.rate_window_ms
                        && w.used.saturating_add(1) > w.cap_at_first_observation
                    {
                        let _ = cap;
                        return Ok(AdmissionOutcome::Deferred {
                            retry_after_ms: g
                                .policy
                                .rate_window_ms
                                .saturating_sub(now_ms.saturating_sub(w.window_start_ms)),
                            reason: format!("rate_exhausted:{s}"),
                        });
                    }
                } else if cap == 0 {
                    return Ok(AdmissionOutcome::Deferred {
                        retry_after_ms: g.policy.rate_window_ms,
                        reason: format!("rate_exhausted:{s}"),
                    });
                }
            }
            // budget
            for s in &scope_strs {
                let cap = g.policy.budget_for(s);
                let st = g.scopes.get(s);
                let used = st.map(|x| x.budget_used).unwrap_or(0);
                let reserved = st.map(|x| x.budget_reserved).unwrap_or(0);
                if used
                    .saturating_add(reserved)
                    .saturating_add(req.requested_units)
                    > cap
                {
                    return Ok(AdmissionOutcome::Denied {
                        reason: format!("budget_exhausted:{s}"),
                    });
                }
            }
        }
        // commit: window consume (1 per admission), budget reserve, concurrency++
        if !governance {
            for s in &scope_strs {
                let cap = g.policy.rate_for(s);
                let window_ms = g.policy.rate_window_ms;
                let entry = g.scopes.entry(s.clone()).or_default();
                let (new_w, admitted) =
                    window_admit(entry.window.take(), now_ms, window_ms, cap, 1);
                entry.window = Some(new_w);
                debug_assert!(admitted);
                entry.budget_reserved = entry.budget_reserved.saturating_add(req.requested_units);
            }
        } else {
            for s in &scope_strs {
                g.scopes.entry(s.clone()).or_default();
            }
        }
        for s in &scope_strs {
            g.scopes.get_mut(s).unwrap().concurrency_live += 1;
        }
        let id = format!("permit-{}", g.next_id);
        g.next_id += 1;
        let permit = Permit {
            id: id.clone(),
            scopes: scope_strs.clone(),
            reserved_units: if governance { 0 } else { req.requested_units },
            deadline_mono_ms: now_ms.saturating_add(g.policy.deadline_window_ms),
            governance_lane: governance,
            confirmed_dispatched: false,
        };
        g.permits.insert(
            id,
            PermitRecord {
                permit: permit.clone(),
                completed: false,
                settled: false,
            },
        );
        Ok(AdmissionOutcome::Admitted(permit))
    }

    fn settle(&self, req: &SettleRequest) -> Result<(), GovernorError> {
        let mut g = self.inner.lock().map_err(|_| GovernorError::Unavailable)?;
        let rec = g
            .permits
            .get_mut(&req.permit_id)
            .ok_or_else(|| GovernorError::UnknownPermit(req.permit_id.clone()))?;
        if rec.settled {
            return Ok(());
        }
        rec.settled = true;
        let reserved = rec.permit.reserved_units;
        let scopes = rec.permit.scopes.clone();
        let governance = rec.permit.governance_lane;
        // Actual charge is capped at the reservation.
        let charged = match req.outcome {
            SettledOutcome::Executed => req.actual_units.min(reserved),
            SettledOutcome::UnknownAfterSend => reserved,
            SettledOutcome::NeverDispatched => 0,
        };
        // UnknownAfterSend never releases concurrency, whatever `complete`
        // says: the permit stays in `unconfirmed_permits`.
        let release_concurrency = req.outcome != SettledOutcome::UnknownAfterSend
            && (req.complete || req.outcome == SettledOutcome::NeverDispatched);
        if release_concurrency {
            rec.completed = true;
        }
        for s in &scopes {
            if let Some(st) = g.scopes.get_mut(s) {
                if !governance {
                    st.budget_reserved = st.budget_reserved.saturating_sub(reserved);
                    st.budget_used = st.budget_used.saturating_add(charged);
                }
                if release_concurrency {
                    st.concurrency_live = st.concurrency_live.saturating_sub(1);
                }
            }
        }
        Ok(())
    }

    fn record_and_check_replay(
        &self,
        req: &ReplayRequest,
    ) -> Result<ReplayDecision, GovernorError> {
        let mut g = self.inner.lock().map_err(|_| GovernorError::Unavailable)?;
        if let Some(action) = &req.record {
            match action {
                ReplayRecordAction::Dispatching => {
                    g.replay.push(ReplayRecord {
                        operation: req.attempt.operation.clone(),
                        outcome: None,
                        original_key: req.attempt.original_key.clone(),
                        side_effect_class: req.attempt.side_effect_class,
                        endpoint: req.attempt.endpoint.clone(),
                        recorded_at_ms: req.now_ms,
                        dispatching: true,
                        owner_fresh_action: req.attempt.owner_fresh_action,
                    });
                    return Ok(
                        if req.attempt.owner_fresh_action
                            && req.attempt.operation.principal.is_owner()
                        {
                            ReplayDecision::AllowedOwnerFresh
                        } else {
                            ReplayDecision::Allowed
                        },
                    );
                }
                ReplayRecordAction::ClearDispatching => {
                    if let Some(pos) = g.replay.iter().rposition(|r| {
                        r.operation.digest == req.attempt.operation.digest
                            && r.operation.principal == req.attempt.operation.principal
                            && r.operation.project == req.attempt.operation.project
                            && r.operation.endpoint_set_digest
                                == req.attempt.operation.endpoint_set_digest
                            && r.outcome.is_none()
                    }) {
                        g.replay.remove(pos);
                    }
                    return Ok(ReplayDecision::Allowed);
                }
                ReplayRecordAction::Outcome(outcome) => {
                    // update latest matching dispatching row or push new
                    let mut found = false;
                    for r in g.replay.iter_mut().rev() {
                        if r.operation.digest == req.attempt.operation.digest
                            && r.operation.principal == req.attempt.operation.principal
                            && r.operation.project == req.attempt.operation.project
                            && r.operation.endpoint_set_digest
                                == req.attempt.operation.endpoint_set_digest
                        {
                            r.outcome = Some(*outcome);
                            r.recorded_at_ms = req.now_ms;
                            r.original_key = req.attempt.original_key.clone();
                            r.endpoint = req.attempt.endpoint.clone();
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        g.replay.push(ReplayRecord {
                            operation: req.attempt.operation.clone(),
                            outcome: Some(*outcome),
                            original_key: req.attempt.original_key.clone(),
                            side_effect_class: req.attempt.side_effect_class,
                            endpoint: req.attempt.endpoint.clone(),
                            recorded_at_ms: req.now_ms,
                            dispatching: true,
                            owner_fresh_action: req.attempt.owner_fresh_action,
                        });
                    }
                    return Ok(ReplayDecision::Allowed);
                }
            }
        }
        // Lookup matches principal too, except the Owner-fresh path, which
        // may bypass a record owned by another principal.
        let owner_fresh =
            req.attempt.owner_fresh_action && req.attempt.operation.principal.is_owner();
        let guard = g.policy.replay_guard_window_ms;
        let record = g
            .replay
            .iter()
            .rev()
            .find(|r| {
                r.operation.digest == req.attempt.operation.digest
                    && r.operation.project == req.attempt.operation.project
                    && r.operation.endpoint_set_digest == req.attempt.operation.endpoint_set_digest
                    && (owner_fresh || r.operation.principal == req.attempt.operation.principal)
            })
            .cloned();
        if let Some(r) = record {
            return Ok(replay_decision(&r, &req.attempt, req.now_ms, guard));
        }
        Ok(ReplayDecision::Allowed)
    }

    fn unconfirmed_permits(&self) -> Vec<Permit> {
        let g = self.inner.lock().unwrap();
        g.permits
            .values()
            .filter(|r| !r.completed)
            .map(|r| r.permit.clone())
            .collect()
    }
    fn load_failover_journal(
        &self,
    ) -> Result<Vec<crate::failover::AttemptJournalEntry>, GovernorError> {
        Ok(self
            .failover_journal
            .lock()
            .map_err(|_| GovernorError::Unavailable)?
            .clone())
    }
    fn save_failover_journal(
        &self,
        entries: &[crate::failover::AttemptJournalEntry],
    ) -> Result<(), GovernorError> {
        let mut g = self
            .failover_journal
            .lock()
            .map_err(|_| GovernorError::Unavailable)?;
        *g = entries.to_vec();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn policy() -> GovernorPolicy {
        GovernorPolicy {
            concurrency_per_scope: 100,
            rate_window_ms: 60_000,
            rate_max_per_window: 1000,
            budget_units_per_scope: 1000,
            freshness_max_age_ms: 5_000,
            replay_guard_window_ms: 60_000,
            deadline_window_ms: 60_000,
            per_scope: BTreeMap::new(),
        }
    }
    fn req(p: Principal, proj: &str, ep: &str, units: u64) -> AdmissionRequest {
        AdmissionRequest::new(p, proj, ep, units)
    }
    fn admitted(o: AdmissionOutcome) -> Permit {
        match o {
            AdmissionOutcome::Admitted(p) => p,
            other => panic!("expected admitted, got {other:?}"),
        }
    }
    fn attempt(
        digest: &str,
        p: Principal,
        endpoint: &str,
        key: Option<&str>,
        class: SideEffectClass,
    ) -> ReplayAttempt {
        ReplayAttempt {
            operation: op(digest, p),
            endpoint: endpoint.into(),
            original_key: key.map(str::to_string),
            side_effect_class: class,
            owner_fresh_action: false,
        }
    }
    fn mk_attempt(
        digest: &str,
        p: Principal,
        endpoint: &str,
        key: Option<&str>,
        class: SideEffectClass,
    ) -> ReplayAttempt {
        attempt(digest, p, endpoint, key, class)
    }
    fn op(digest: &str, p: Principal) -> LogicalOperationId {
        LogicalOperationId {
            digest: digest.into(),
            principal: p,
            project: "proj".into(),
            endpoint_set_digest: "set1".into(),
        }
    }

    #[test]
    fn window_cap_never_replenishes() {
        // pure
        let (s, ok) = window_admit(None, 1000, 60_000, 1, 1);
        assert!(ok);
        let (_, ok2) = window_admit(Some(s.clone()), 2000, 60_000, 100, 1);
        assert!(!ok2);
        // store level
        let mut pol = policy();
        pol.rate_max_per_window = 1;
        let store = MemGovernorStore::new(pol);
        admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 1), 1000)
                .unwrap(),
        );
        store.update_policy(|p| p.rate_max_per_window = 100);
        match store
            .reserve(&req(Principal::Owner, "p", "h/a", 1), 2000)
            .unwrap()
        {
            AdmissionOutcome::Deferred { .. } => {}
            other => panic!("expected deferred frozen cap, got {other:?}"),
        }
    }
    #[test]
    fn caller_string_never_keys_budget() {
        let mut pol = policy();
        pol.per_scope.insert(
            "principal:bundle:b1".into(),
            ScopePolicy {
                budget_units_per_scope: Some(1),
                ..Default::default()
            },
        );
        let store = MemGovernorStore::new(pol);
        let mut r1 = req(Principal::Bundle("b1".into()), "p", "h/a", 1);
        r1.caller_agent = "alice".into();
        admitted(store.reserve(&r1, 1000).unwrap());
        let mut r2 = req(Principal::Bundle("b1".into()), "p", "h/a", 1);
        r2.caller_agent = "bob".into();
        assert!(matches!(
            store.reserve(&r2, 1000).unwrap(),
            AdmissionOutcome::Denied { .. }
        ));
    }
    #[test]
    fn governance_lane_skips_worker_allocation() {
        let mut pol = policy();
        pol.budget_units_per_scope = 1;
        let store = MemGovernorStore::new(pol);
        let mut gr = req(Principal::Owner, "p", "h/a", 1);
        gr.governance_call = Some("warrant_revoke_check".into());
        let p = admitted(store.reserve(&gr, 1000).unwrap());
        assert!(p.governance_lane);
        // worker budget still intact
        admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 1), 1000)
                .unwrap(),
        );
    }
    #[test]
    fn per_endpoint_isolation() {
        let mut pol = policy();
        pol.per_scope.insert(
            "endpoint:h/a".into(),
            ScopePolicy {
                budget_units_per_scope: Some(1),
                ..Default::default()
            },
        );
        let store = MemGovernorStore::new(pol);
        admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 1), 1000)
                .unwrap(),
        );
        assert!(matches!(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 1), 1000)
                .unwrap(),
            AdmissionOutcome::Denied { .. }
        ));
        admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/b", 1), 1000)
                .unwrap(),
        );
    }
    #[test]
    fn cross_endpoint_competition_one_project_remainder() {
        let mut pol = policy();
        pol.per_scope.insert(
            "project:p".into(),
            ScopePolicy {
                budget_units_per_scope: Some(1),
                ..Default::default()
            },
        );
        let store = MemGovernorStore::new(pol);
        admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 1), 1000)
                .unwrap(),
        );
        assert!(matches!(
            store
                .reserve(&req(Principal::Owner, "p", "h/b", 1), 1000)
                .unwrap(),
            AdmissionOutcome::Denied { .. }
        ));
    }
    #[test]
    fn raw_vs_connector_alias_same_budget() {
        let aliases = vec![EndpointAlias {
            connector_id: "c1".into(),
            tool: "t1".into(),
            host: "API.Example.com:443".into(),
            path_prefix: "/v1/chat".into(),
        }];
        let via_conn = resolve_canonical_endpoint(&aliases, Some("c1"), Some("t1"), "", "");
        let via_raw =
            resolve_canonical_endpoint(&aliases, None, None, "api.example.com:443", "/v1/chat");
        assert_eq!(via_conn, via_raw);
        assert_eq!(via_conn, "api.example.com/v1/chat");
        let mut pol = policy();
        pol.per_scope.insert(
            format!("endpoint:{via_conn}"),
            ScopePolicy {
                budget_units_per_scope: Some(1),
                ..Default::default()
            },
        );
        let store = MemGovernorStore::new(pol);
        admitted(
            store
                .reserve(&req(Principal::Owner, "p", &via_conn, 1), 1000)
                .unwrap(),
        );
        assert!(matches!(
            store
                .reserve(&req(Principal::Owner, "p", &via_raw, 1), 1000)
                .unwrap(),
            AdmissionOutcome::Denied { .. }
        ));
    }
    #[test]
    fn multi_principal_one_project_cap() {
        let mut pol = policy();
        pol.per_scope.insert(
            "project:p".into(),
            ScopePolicy {
                budget_units_per_scope: Some(1),
                ..Default::default()
            },
        );
        let store = MemGovernorStore::new(pol);
        admitted(
            store
                .reserve(&req(Principal::Warrant("w1".into()), "p", "h/a", 1), 1000)
                .unwrap(),
        );
        assert!(matches!(
            store
                .reserve(&req(Principal::Bundle("b1".into()), "p", "h/a", 1), 1000)
                .unwrap(),
            AdmissionOutcome::Denied { .. }
        ));
    }
    #[test]
    fn unknown_result_recall_new_key_refused() {
        let store = MemGovernorStore::new(policy());
        let attempt = attempt(
            "d1",
            Principal::Bundle("b1".into()),
            "h/a",
            Some("k1"),
            SideEffectClass::IdempotentWithKey,
        );
        store.record_dispatching(attempt.clone(), 1000);
        store.record_outcome(attempt, RecordedOutcome::UnknownAfterSend, 1000);
        let mut retry = mk_attempt(
            "d1",
            Principal::Bundle("b1".into()),
            "h/a",
            Some("k2"),
            SideEffectClass::IdempotentWithKey,
        );
        assert!(matches!(
            store.check_replay(retry.clone(), 2000).unwrap(),
            ReplayDecision::Refused { .. }
        ));
        retry.side_effect_class = SideEffectClass::SideEffectful;
        retry.original_key = Some("k1".into());
        assert!(matches!(
            store.check_replay(retry, 2000).unwrap(),
            ReplayDecision::Refused { .. }
        ));
    }
    #[test]
    fn unknown_result_endpoint_switch_refused() {
        let store = MemGovernorStore::new(policy());
        let attempt = attempt(
            "d1",
            Principal::Bundle("b1".into()),
            "h/a",
            Some("k1"),
            SideEffectClass::IdempotentWithKey,
        );
        store.record_outcome(attempt, RecordedOutcome::UnknownAfterSend, 1000);
        let retry = mk_attempt(
            "d1",
            Principal::Bundle("b1".into()),
            "h/b",
            Some("k1"),
            SideEffectClass::IdempotentWithKey,
        );
        assert!(matches!(
            store.check_replay(retry, 2000).unwrap(),
            ReplayDecision::Refused { .. }
        ));
    }
    #[test]
    fn executed_never_reconciles_even_with_original_key() {
        let store = MemGovernorStore::new(policy());
        let attempt = attempt(
            "d1",
            Principal::Bundle("b1".into()),
            "h/a",
            Some("k1"),
            SideEffectClass::IdempotentWithKey,
        );
        store.record_outcome(attempt.clone(), RecordedOutcome::Executed, 1000);
        assert!(matches!(
            store.check_replay(attempt, 2000).unwrap(),
            ReplayDecision::Refused { .. }
        ));
    }
    #[test]
    fn recorded_side_effectful_unknown_never_reconciles() {
        // Recorded class (SideEffectful at dispatch) governs, even if the
        // retry claims IdempotentWithKey.
        let store = MemGovernorStore::new(policy());
        let attempt = attempt(
            "d1",
            Principal::Bundle("b1".into()),
            "h/a",
            Some("k1"),
            SideEffectClass::SideEffectful,
        );
        store.record_outcome(attempt, RecordedOutcome::UnknownAfterSend, 1000);
        let retry = mk_attempt(
            "d1",
            Principal::Bundle("b1".into()),
            "h/a",
            Some("k1"),
            SideEffectClass::IdempotentWithKey,
        );
        assert!(matches!(
            store.check_replay(retry, 2000).unwrap(),
            ReplayDecision::Refused { .. }
        ));
    }
    #[test]
    fn admission_attempt_uses_explicit_endpoint_set_not_endpoint() {
        let mut r = req(Principal::Bundle("b1".into()), "proj", "h/a", 1);
        r.operation_digest = Some("op".into());
        r.endpoint_set_digest = "set-logical".into();
        let a = r.replay_attempt().unwrap();
        assert_eq!(a.operation.endpoint_set_digest, "set-logical");
        assert_eq!(a.endpoint, "h/a");
        // Same digest+set, switched concrete endpoint: lookup finds the
        // record (shared set) and replay_decision refuses the switch.
        let store = MemGovernorStore::new(policy());
        store.record_outcome(a, RecordedOutcome::Executed, 1000);
        let mut r2 = req(Principal::Bundle("b1".into()), "proj", "h/b", 1);
        r2.operation_digest = Some("op".into());
        r2.endpoint_set_digest = "set-logical".into();
        assert!(matches!(
            store
                .check_replay(r2.replay_attempt().unwrap(), 2000)
                .unwrap(),
            ReplayDecision::Refused { .. }
        ));
    }
    #[test]
    fn alias_raw_match_requires_segment_boundary() {
        let aliases = vec![EndpointAlias {
            connector_id: "c1".into(),
            tool: "t1".into(),
            host: "api.example.com".into(),
            path_prefix: "/v1/chat".into(),
        }];
        assert_eq!(
            resolve_canonical_endpoint(&aliases, None, None, "api.example.com", "/v1/chat/more"),
            "api.example.com/v1/chat"
        );
        assert_eq!(
            resolve_canonical_endpoint(&aliases, None, None, "api.example.com", "/v1/chatty"),
            "api.example.com/v1/chatty"
        );
        // Connector form ignores the per-call path and uses the prefix.
        assert_eq!(
            resolve_canonical_endpoint(
                &aliases,
                Some("c1"),
                Some("t1"),
                "other.example.com",
                "/elsewhere"
            ),
            "api.example.com/v1/chat"
        );
    }
    #[test]
    fn crash_before_outcome_write_is_uncertain() {
        let store = MemGovernorStore::new(policy());
        let attempt = attempt(
            "d1",
            Principal::Bundle("b1".into()),
            "h/a",
            Some("k1"),
            SideEffectClass::IdempotentWithKey,
        );
        store.record_dispatching(attempt.clone(), 1000);
        assert!(matches!(
            store.check_replay(attempt, 2000).unwrap(),
            ReplayDecision::Refused { .. }
        ));
    }
    #[test]
    fn original_key_reconciliation_only() {
        let store = MemGovernorStore::new(policy());
        let attempt = attempt(
            "d1",
            Principal::Bundle("b1".into()),
            "h/a",
            Some("k1"),
            SideEffectClass::IdempotentWithKey,
        );
        store.record_outcome(attempt.clone(), RecordedOutcome::UnknownAfterSend, 1000);
        assert_eq!(
            store.check_replay(attempt, 2000).unwrap(),
            ReplayDecision::AllowedReconciliation
        );
        // Owner without the fresh flag is not a bypass (principal mismatch
        // means this record is not even its replay); with an Owner-owned
        // record it is refused like everyone else (see settlement test below).
        let mut owner = mk_attempt(
            "d1",
            Principal::Owner,
            "h/a",
            None,
            SideEffectClass::SideEffectful,
        );
        assert_ne!(
            store.check_replay(owner.clone(), 2000).unwrap(),
            ReplayDecision::AllowedOwnerFresh
        );
        owner.owner_fresh_action = true;
        assert_eq!(
            store.check_replay(owner, 2000).unwrap(),
            ReplayDecision::AllowedOwnerFresh
        );
    }
    #[test]
    fn owner_replay_refused_unless_fresh_and_settlement_caps() {
        let store = MemGovernorStore::new(policy());
        let owner_attempt = attempt(
            "d9",
            Principal::Owner,
            "h/a",
            Some("k1"),
            SideEffectClass::SideEffectful,
        );
        store.record_outcome(owner_attempt.clone(), RecordedOutcome::Executed, 1000);
        assert!(matches!(
            store.check_replay(owner_attempt.clone(), 2000).unwrap(),
            ReplayDecision::Refused { .. }
        ));
        // Different principal with same digest is a different operation.
        let other = attempt(
            "d9",
            Principal::Bundle("b1".into()),
            "h/a",
            Some("k1"),
            SideEffectClass::SideEffectful,
        );
        assert_eq!(
            store.check_replay(other, 2000).unwrap(),
            ReplayDecision::Allowed
        );
        // Settlement cap: actual above reserved charges only the reservation,
        // and UnknownAfterSend with complete=true keeps concurrency.
        let mut pol = policy();
        pol.budget_units_per_scope = 100;
        pol.concurrency_per_scope = 1;
        let store = MemGovernorStore::new(pol);
        let p = admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 5), 1000)
                .unwrap(),
        );
        store
            .settle(&SettleRequest {
                permit_id: p.id.clone(),
                actual_units: 999,
                outcome: SettledOutcome::Executed,
                complete: true,
            })
            .unwrap();
        assert_eq!(store.budget_used_for("project:p"), 5);
        let p2 = admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 5), 2000)
                .unwrap(),
        );
        store
            .settle(&SettleRequest {
                permit_id: p2.id.clone(),
                actual_units: 5,
                outcome: SettledOutcome::UnknownAfterSend,
                complete: true,
            })
            .unwrap();
        assert!(store.unconfirmed_permits().iter().any(|x| x.id == p2.id));
        assert!(matches!(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 1), 3000)
                .unwrap(),
            AdmissionOutcome::Deferred { .. }
        ));
    }
    #[test]
    fn stale_quota_defers_not_admits() {
        let store = MemGovernorStore::new(policy());
        let mut r = req(Principal::Owner, "p", "h/a", 1);
        r.quota_observed_at_ms = Some(0);
        assert!(matches!(
            store.reserve(&r, 100_000).unwrap(),
            AdmissionOutcome::Deferred { .. }
        ));
    }
    #[test]
    fn settlement_charges_actuals_releases_remainder() {
        let mut pol = policy();
        pol.budget_units_per_scope = 10;
        let store = MemGovernorStore::new(pol);
        let p = admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 5), 1000)
                .unwrap(),
        );
        store
            .settle(&SettleRequest {
                permit_id: p.id,
                actual_units: 2,
                outcome: SettledOutcome::Executed,
                complete: true,
            })
            .unwrap();
        assert_eq!(store.budget_used_for("project:p"), 2);
        admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 8), 1000)
                .unwrap(),
        );
    }
    #[test]
    fn ambiguous_outcome_counts_spent() {
        let mut pol = policy();
        pol.budget_units_per_scope = 10;
        let store = MemGovernorStore::new(pol);
        let p = admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 5), 1000)
                .unwrap(),
        );
        store
            .settle(&SettleRequest {
                permit_id: p.id,
                actual_units: 1,
                outcome: SettledOutcome::UnknownAfterSend,
                complete: true,
            })
            .unwrap();
        assert_eq!(store.budget_used_for("project:p"), 5);
        assert!(matches!(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 6), 1000)
                .unwrap(),
            AdmissionOutcome::Denied { .. }
        ));
    }
    #[test]
    fn concurrent_admissions_exactly_one_wins_last_unit() {
        let mut pol = policy();
        pol.budget_units_per_scope = 1;
        let store = Arc::new(MemGovernorStore::new(pol));
        let mut hs = vec![];
        for _ in 0..8 {
            let s = store.clone();
            hs.push(std::thread::spawn(move || {
                matches!(
                    s.reserve(&req(Principal::Owner, "p", "h/a", 1), 1000)
                        .unwrap(),
                    AdmissionOutcome::Admitted(_)
                )
            }));
        }
        let wins = hs
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|w| *w)
            .count();
        assert_eq!(wins, 1);
    }
    #[test]
    fn permit_unconfirmed_expiry_stays_counted() {
        let mut pol = policy();
        pol.concurrency_per_scope = 1;
        let store = MemGovernorStore::new(pol);
        let p = admitted(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 1), 1000)
                .unwrap(),
        );
        let pid = p.id.clone();
        store
            .settle(&SettleRequest {
                permit_id: pid.clone(),
                actual_units: 1,
                outcome: SettledOutcome::Executed,
                complete: false,
            })
            .unwrap();
        assert!(store.unconfirmed_permits().iter().any(|x| x.id == pid));
        assert!(matches!(
            store
                .reserve(&req(Principal::Owner, "p", "h/a", 1), 2000)
                .unwrap(),
            AdmissionOutcome::Deferred { .. }
        ));
        // NeverDispatched releases
        let store2 = MemGovernorStore::new({
            let mut p2 = policy();
            p2.concurrency_per_scope = 1;
            p2
        });
        let p2p = admitted(
            store2
                .reserve(&req(Principal::Owner, "p", "h/a", 1), 1000)
                .unwrap(),
        );
        store2
            .settle(&SettleRequest {
                permit_id: p2p.id,
                actual_units: 0,
                outcome: SettledOutcome::NeverDispatched,
                complete: false,
            })
            .unwrap();
        admitted(
            store2
                .reserve(&req(Principal::Owner, "p", "h/a", 1), 1000)
                .unwrap(),
        );
    }
}
