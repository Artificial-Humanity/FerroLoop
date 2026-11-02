//! Automated subscription usage balancing (Local Record 60): the pure planner.
//!
//! Hub agents run on vendor subscriptions whose allowance has a short (`primary`, 5 hours) and a
//! long (`secondary`, 7 days) window. When the vendor an agent runs on runs hot, the hub moves
//! the agent to the healthiest allowed vendor with a remodel. This module decides which moves to
//! make; it reads no store and calls no vendor. [`plan_balance`] takes a [`BalancePolicy`], the
//! clock, the agents, the vendor quota readings, the candidate vendors and the
//! [`BalanceHistory`], and returns a [`BalancePlan`]. The same inputs always give the same plan.
//!
//! **Vendor health.** Every reading whose `resets_at_ms` is still ahead of `now_ms` (or that has
//! no reset time) is live; a reading that has reset is ignored. Per vendor (the trimmed,
//! lowercased name) each window reads the HIGHEST live finite `used_percent`, and any live
//! reading with an `exhausted_reason` makes the vendor exhausted, whatever its windows read.
//! A vendor with no live reading is `unknown`.
//!
//! **Triggers.** An agent triggers when its vendor is exhausted, its primary window is at or
//! above `primary_trigger_pct`, or its secondary window is at or above `secondary_trigger_pct`
//! (checked in that order; the first one that fires is the plan's `trigger`).
//!
//! **Targets.** A candidate vendor is eligible when the policy allows it (in
//! `allowed_target_vendors`, or that list is empty; and not in `excluded_target_vendors`), it is
//! not exhausted, it is not the agent's own vendor, and its primary and secondary windows are at
//! or below `target_max_primary_pct` and `target_max_secondary_pct`. A window a vendor does not
//! report counts as 0% when the vendor has some other live reading. A fully unknown vendor is
//! eligible only with `allow_unknown_quota_targets`, ranks after every known vendor, and takes at
//! most one agent per plan. Known vendors rank by the lowest `max(primary, secondary)` (the
//! score), then the lowest primary, then the lowest secondary, then the vendor id. The candidate
//! vendors are the caller's (the hub passes the subscription vendors only, never a metered API
//! vendor); the policy's lists only narrow them.
//!
//! **No load projection.** A plan does not add its own switches' load to a known target: every
//! agent sees the readings as they were. With `max_switches_per_run` above 1, several agents can
//! land on the same coolest vendor in one run (an unknown vendor takes one at most).
//!
//! **Order and skips.** Every agent appears in the plan: as a switch, or as a skip with its
//! reason (pinned, excluded, healthy, no reading, not remodelable, cooldown, persona already
//! switched, no eligible target, run limit). Triggered agents are taken worst first (an exhausted
//! vendor, then the highest score, then persona and session id), so `max_switches_per_run` keeps
//! the hottest. A persona is switched once per run: a second session of it is skipped. With
//! [`plan_balance_checked`] the caller also judges each chosen switch before it is planned (the
//! hub runs the remodel's own pre-checks); a refused one is a `not_remodelable` skip and leaves
//! its run slot to the next triggered agent. A plan made before `min_interval_ms` has passed since
//! the last apply carries a `blocked` reason: its switches show what an apply would do once it
//! may run, and an apply refuses it.
//!
//! **`enabled`.** The planner and an explicit apply (by the Owner, or a peer holding an Owner
//! warrant) ignore it. It is the switch a future scheduler reads before it applies on its own.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::alerts::{QuotaWindowKind, WatchQuotaWindow};

/// The Owner-warrant scope that lets a peer run a balance apply.
pub const APPLY_SCOPE: &str = "watch.balance.apply";

pub const DEFAULT_PRIMARY_TRIGGER_PCT: f64 = 90.0;
pub const DEFAULT_SECONDARY_TRIGGER_PCT: f64 = 85.0;
pub const DEFAULT_TARGET_MAX_PRIMARY_PCT: f64 = 70.0;
pub const DEFAULT_TARGET_MAX_SECONDARY_PCT: f64 = 70.0;
/// 30 minutes.
pub const DEFAULT_COOLDOWN_MS: u64 = 30 * 60 * 1000;
/// 5 minutes.
pub const DEFAULT_MIN_INTERVAL_MS: u64 = 5 * 60 * 1000;
pub const DEFAULT_MAX_SWITCHES_PER_RUN: u32 = 1;

/// Why a balance policy, plan or store entry was refused.
#[derive(Debug, thiserror::Error)]
pub enum BalanceError {
    /// A policy field is out of range: the message names the field.
    #[error("invalid balance policy: {0}")]
    InvalidPolicy(String),
    #[error(transparent)]
    Store(#[from] fl_core::store::StoreError),
    /// A stored balance entry that does not decode: the message names the slug.
    #[error("{0}")]
    Undecodable(String),
}

/// When and how far the hub moves agents off a hot vendor. Every field has a default, so a
/// partial JSON object is a whole policy; an unknown field is refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BalancePolicy {
    /// Read by a future scheduler only: an explicit apply runs whatever this says.
    pub enabled: bool,
    /// An agent whose vendor's primary window is at or above this is moved.
    pub primary_trigger_pct: f64,
    /// An agent whose vendor's secondary window is at or above this is moved.
    pub secondary_trigger_pct: f64,
    /// A target's primary window must be at or below this.
    pub target_max_primary_pct: f64,
    /// A target's secondary window must be at or below this.
    pub target_max_secondary_pct: f64,
    /// The least time between two balance moves of one persona.
    pub cooldown_ms: u64,
    /// The least time between two balance applies.
    pub min_interval_ms: u64,
    /// The most switches one plan makes (at least 1).
    pub max_switches_per_run: u32,
    /// Personas never moved.
    pub pinned_personas: Vec<String>,
    /// Personas balancing leaves alone.
    pub excluded_personas: Vec<String>,
    /// The vendors an agent may move to; empty is every candidate vendor.
    pub allowed_target_vendors: Vec<String>,
    /// Vendors an agent never moves to.
    pub excluded_target_vendors: Vec<String>,
    /// Whether a vendor with no live quota reading may be a target.
    pub allow_unknown_quota_targets: bool,
}

impl Default for BalancePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            primary_trigger_pct: DEFAULT_PRIMARY_TRIGGER_PCT,
            secondary_trigger_pct: DEFAULT_SECONDARY_TRIGGER_PCT,
            target_max_primary_pct: DEFAULT_TARGET_MAX_PRIMARY_PCT,
            target_max_secondary_pct: DEFAULT_TARGET_MAX_SECONDARY_PCT,
            cooldown_ms: DEFAULT_COOLDOWN_MS,
            min_interval_ms: DEFAULT_MIN_INTERVAL_MS,
            max_switches_per_run: DEFAULT_MAX_SWITCHES_PER_RUN,
            pinned_personas: Vec::new(),
            excluded_personas: Vec::new(),
            allowed_target_vendors: Vec::new(),
            excluded_target_vendors: Vec::new(),
            allow_unknown_quota_targets: false,
        }
    }
}

fn key(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn key_set(list: &[String]) -> BTreeSet<String> {
    list.iter().map(|s| key(s)).collect()
}

impl BalancePolicy {
    /// Refuse a policy that cannot be meant: a percentage that is not a number or is outside
    /// 0..=100, a trigger at or below its target maximum (an agent moved off a vendor would be
    /// a target for it again), no switch per run, an empty name, or a vendor both allowed and
    /// excluded.
    pub fn validate(&self) -> Result<(), BalanceError> {
        let bad = |m: String| Err(BalanceError::InvalidPolicy(m));
        for (name, v) in [
            ("primary_trigger_pct", self.primary_trigger_pct),
            ("secondary_trigger_pct", self.secondary_trigger_pct),
            ("target_max_primary_pct", self.target_max_primary_pct),
            ("target_max_secondary_pct", self.target_max_secondary_pct),
        ] {
            if !v.is_finite() {
                return bad(format!("`{name}` must be a number, not {v}"));
            }
            if !(0.0..=100.0).contains(&v) {
                return bad(format!("`{name}` must be between 0 and 100, not {v}"));
            }
        }
        for (trigger, max, t, m) in [
            (
                "primary_trigger_pct",
                "target_max_primary_pct",
                self.primary_trigger_pct,
                self.target_max_primary_pct,
            ),
            (
                "secondary_trigger_pct",
                "target_max_secondary_pct",
                self.secondary_trigger_pct,
                self.target_max_secondary_pct,
            ),
        ] {
            if t <= m {
                return bad(format!("`{trigger}` ({t}) must be above `{max}` ({m})"));
            }
        }
        if self.max_switches_per_run == 0 {
            return bad("`max_switches_per_run` must be at least 1".into());
        }
        for (name, list) in [
            ("pinned_personas", &self.pinned_personas),
            ("excluded_personas", &self.excluded_personas),
            ("allowed_target_vendors", &self.allowed_target_vendors),
            ("excluded_target_vendors", &self.excluded_target_vendors),
        ] {
            if list.iter().any(|s| s.trim().is_empty()) {
                return bad(format!("`{name}` has an empty entry"));
            }
        }
        let excluded = key_set(&self.excluded_target_vendors);
        if let Some(both) = key_set(&self.allowed_target_vendors)
            .intersection(&excluded)
            .next()
        {
            return bad(format!(
                "vendor `{both}` is in both `allowed_target_vendors` and `excluded_target_vendors`"
            ));
        }
        Ok(())
    }
}

/// One agent the planner may move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalanceAgent {
    pub persona: String,
    pub session_id: String,
    /// The vendor it runs on now.
    pub vendor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub project: String,
    /// Why a remodel would be refused, so the plan does not recommend it; `None` when a remodel
    /// can move it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remodel_blocker: Option<String>,
}

/// What earlier applies did, for cooldowns and the minimum interval.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BalanceHistory {
    /// When the last apply that ran a switch ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_apply_ms: Option<u64>,
    /// Lowercased persona → when an apply last ran a remodel of it (whether it succeeded or
    /// failed; a refused remodel touched nothing and does not count).
    pub persona_last_switch_ms: BTreeMap<String, u64>,
}

impl BalanceHistory {
    /// Note an apply at `now_ms` that ran a remodel of each of `personas`. With no persona the
    /// apply ran no switch, and nothing changes. Returns whether anything was noted.
    pub fn record<'a>(&mut self, now_ms: u64, personas: impl IntoIterator<Item = &'a str>) -> bool {
        let mut any = false;
        for p in personas {
            self.persona_last_switch_ms.insert(key(p), now_ms);
            any = true;
        }
        if any {
            self.last_apply_ms = Some(now_ms);
        }
        any
    }
}

/// Whether the planner knows how much of a vendor's allowance is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VendorStatus {
    /// A live reading says the allowance is exhausted (or rate limited).
    Exhausted,
    /// At least one live reading.
    Known,
    /// No live reading.
    Unknown,
}

/// The worst live reading of one window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VendorWindow {
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    /// How many live readings of this window there were.
    pub readings: u32,
}

/// One vendor as the planner sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VendorHealth {
    pub vendor: String,
    pub status: VendorStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<VendorWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<VendorWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhausted_reason: Option<String>,
    /// `max(primary, secondary)`, a missing window read as 0; `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Whether the vendor is one an agent may be moved to at all (in the candidate list).
    pub candidate: bool,
    /// Whether it may be a target (for an agent on another vendor).
    pub eligible: bool,
    /// Why it may not be a target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ineligible_reason: Option<String>,
}

impl VendorHealth {
    fn primary_pct(&self) -> f64 {
        self.primary.as_ref().map_or(0.0, |w| w.used_percent)
    }

    fn secondary_pct(&self) -> f64 {
        self.secondary.as_ref().map_or(0.0, |w| w.used_percent)
    }
}

/// Which rule moved an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BalanceTrigger {
    Exhausted,
    Primary,
    Secondary,
}

/// One move the plan makes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedSwitch {
    pub persona: String,
    pub session_id: String,
    #[serde(default)]
    pub project: String,
    pub from_vendor: String,
    pub to_vendor: String,
    pub trigger: BalanceTrigger,
    /// Why, in words: the rule and the reading that fired it.
    pub reason: String,
    /// The target's score; `None` for an unknown target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_score: Option<f64>,
    pub target_status: VendorStatus,
}

/// Why an agent is not moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Pinned,
    Excluded,
    /// Its vendor is under every trigger.
    Healthy,
    /// Its vendor has no live reading, so nothing can trigger.
    NoReading,
    NotRemodelable,
    Cooldown,
    /// Another session of the persona is switched in this run.
    PersonaSwitched,
    NoEligibleTarget,
    RunLimit,
}

/// An agent the plan leaves where it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedAgent {
    pub persona: String,
    pub session_id: String,
    pub vendor: String,
    pub reason: SkipReason,
    pub detail: String,
}

/// What a balance run would do (or did) at `evaluated_at_ms`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BalancePlan {
    pub evaluated_at_ms: u64,
    pub policy: BalancePolicy,
    /// Set when no apply may run now; the switches are still what one would do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    pub switches: Vec<PlannedSwitch>,
    pub skipped: Vec<SkippedAgent>,
    /// Every vendor the plan considered, by vendor id.
    pub vendors: Vec<VendorHealth>,
}

/// Everything [`plan_balance`] reads.
#[derive(Debug, Clone, Copy)]
pub struct BalanceInputs<'a> {
    pub policy: &'a BalancePolicy,
    pub now_ms: u64,
    pub agents: &'a [BalanceAgent],
    pub readings: &'a [WatchQuotaWindow],
    /// The vendors an agent may move to before the policy narrows them (the hub passes the
    /// subscription vendors).
    pub candidate_vendors: &'a [String],
    pub history: &'a BalanceHistory,
}

fn minutes(ms: u64) -> u64 {
    ms.div_ceil(60_000)
}

/// The health of every vendor `readings` and `extra` name, keyed by vendor id.
pub fn vendor_health(
    readings: &[WatchQuotaWindow],
    now_ms: u64,
    extra: impl IntoIterator<Item = String>,
) -> BTreeMap<String, VendorHealth> {
    let mut out: BTreeMap<String, VendorHealth> = BTreeMap::new();
    let blank = |vendor: &str| VendorHealth {
        vendor: vendor.to_string(),
        status: VendorStatus::Unknown,
        primary: None,
        secondary: None,
        exhausted_reason: None,
        score: None,
        candidate: false,
        eligible: false,
        ineligible_reason: None,
    };
    for v in extra {
        let k = key(&v);
        if !k.is_empty() {
            out.entry(k.clone()).or_insert_with(|| blank(&k));
        }
    }
    for q in readings {
        if q.resets_at_ms.is_some_and(|r| r <= now_ms) {
            continue;
        }
        let k = key(&q.vendor);
        if k.is_empty() {
            continue;
        }
        let h = out.entry(k.clone()).or_insert_with(|| blank(&k));
        if let Some(reason) = &q.exhausted_reason {
            h.status = VendorStatus::Exhausted;
            // The first reason in reading order is kept, so the plan does not depend on which
            // of several sessions said it.
            if h.exhausted_reason.is_none() {
                h.exhausted_reason = Some(reason.clone());
            }
        } else if h.status == VendorStatus::Unknown && q.used_percent.is_finite() {
            h.status = VendorStatus::Known;
        }
        if !q.used_percent.is_finite() {
            continue;
        }
        let slot = match q.window {
            QuotaWindowKind::Primary => &mut h.primary,
            QuotaWindowKind::Secondary => &mut h.secondary,
        };
        match slot {
            Some(w) => {
                w.readings += 1;
                // The highest reading wins; on a tie, the later reset (the longer wait).
                let higher = q.used_percent > w.used_percent
                    || (q.used_percent == w.used_percent
                        && q.resets_at_ms.unwrap_or(u64::MAX) > w.resets_at_ms.unwrap_or(u64::MAX));
                if higher {
                    w.used_percent = q.used_percent;
                    w.resets_at_ms = q.resets_at_ms;
                    w.window_minutes = q.window_minutes;
                }
            }
            None => {
                *slot = Some(VendorWindow {
                    used_percent: q.used_percent,
                    resets_at_ms: q.resets_at_ms,
                    window_minutes: q.window_minutes,
                    readings: 1,
                });
            }
        }
    }
    for h in out.values_mut() {
        if h.status != VendorStatus::Unknown {
            h.score = Some(h.primary_pct().max(h.secondary_pct()));
        }
    }
    out
}

/// The rule that fires for a vendor, and the words for it.
fn trigger_of(h: &VendorHealth, p: &BalancePolicy) -> Option<(BalanceTrigger, String)> {
    if h.status == VendorStatus::Exhausted {
        let why = h.exhausted_reason.as_deref().unwrap_or("exhausted");
        return Some((
            BalanceTrigger::Exhausted,
            format!("{} reports its allowance exhausted (`{why}`)", h.vendor),
        ));
    }
    for (kind, window, limit) in [
        (BalanceTrigger::Primary, &h.primary, p.primary_trigger_pct),
        (
            BalanceTrigger::Secondary,
            &h.secondary,
            p.secondary_trigger_pct,
        ),
    ] {
        if let Some(w) = window
            && w.used_percent >= limit
        {
            let name = if kind == BalanceTrigger::Primary {
                "primary"
            } else {
                "secondary"
            };
            let resets = w
                .resets_at_ms
                .map(|r| format!(", resets at {r}"))
                .unwrap_or_default();
            return Some((
                kind,
                format!(
                    "{} {name} window at {:.1}% >= {limit}% trigger{resets}",
                    h.vendor, w.used_percent
                ),
            ));
        }
    }
    None
}

/// Whether a vendor may be a target, ignoring whose vendor it is: `Err` says why not.
fn target_check(
    h: &VendorHealth,
    p: &BalancePolicy,
    allowed: &BTreeSet<String>,
) -> Result<(), String> {
    let excluded = key_set(&p.excluded_target_vendors);
    if !h.candidate {
        return Err("not a candidate vendor (not a subscription vendor)".into());
    }
    if !allowed.is_empty() && !allowed.contains(&h.vendor) {
        return Err("not in `allowed_target_vendors`".into());
    }
    if excluded.contains(&h.vendor) {
        return Err("in `excluded_target_vendors`".into());
    }
    match h.status {
        VendorStatus::Exhausted => Err(format!(
            "exhausted (`{}`)",
            h.exhausted_reason.as_deref().unwrap_or("exhausted")
        )),
        VendorStatus::Unknown if !p.allow_unknown_quota_targets => {
            Err("no live quota reading, and `allow_unknown_quota_targets` is off".into())
        }
        VendorStatus::Unknown => Ok(()),
        VendorStatus::Known => {
            if h.primary_pct() > p.target_max_primary_pct {
                return Err(format!(
                    "primary window at {:.1}% > {}% target maximum",
                    h.primary_pct(),
                    p.target_max_primary_pct
                ));
            }
            if h.secondary_pct() > p.target_max_secondary_pct {
                return Err(format!(
                    "secondary window at {:.1}% > {}% target maximum",
                    h.secondary_pct(),
                    p.target_max_secondary_pct
                ));
            }
            Ok(())
        }
    }
}

/// Target order: known before unknown; then score, primary, secondary, vendor id.
fn target_order(a: &VendorHealth, b: &VendorHealth) -> Ordering {
    let unknown = |h: &VendorHealth| h.status == VendorStatus::Unknown;
    unknown(a)
        .cmp(&unknown(b))
        .then_with(|| a.score.unwrap_or(0.0).total_cmp(&b.score.unwrap_or(0.0)))
        .then_with(|| a.primary_pct().total_cmp(&b.primary_pct()))
        .then_with(|| a.secondary_pct().total_cmp(&b.secondary_pct()))
        .then_with(|| a.vendor.cmp(&b.vendor))
}

/// The caller's verdict on one chosen switch: the agent, and the vendor it would move to.
/// `Err` says why the move would be refused.
pub type Remodelable<'f> = dyn FnMut(&BalanceAgent, &str) -> Result<(), String> + 'f;

/// Plan a balance run. The policy is validated first. Every chosen switch is planned as it is;
/// see [`plan_balance_checked`] to judge each one first.
pub fn plan_balance(inputs: &BalanceInputs<'_>) -> Result<BalancePlan, BalanceError> {
    plan_balance_checked(inputs, &mut |_, _| Ok(()))
}

/// Plan a balance run, asking `remodelable` about each switch the planner chooses (in plan
/// order, and only for an agent that triggered, is under the run limit and has a target). A
/// refused switch becomes a `not_remodelable` skip with the verdict's reason, and its run slot
/// goes to the next triggered agent. The plan is as deterministic as the verdicts.
pub fn plan_balance_checked(
    inputs: &BalanceInputs<'_>,
    remodelable: &mut Remodelable<'_>,
) -> Result<BalancePlan, BalanceError> {
    let p = inputs.policy;
    p.validate()?;
    let now = inputs.now_ms;
    let candidates = key_set(inputs.candidate_vendors);
    let allowed = key_set(&p.allowed_target_vendors);
    let pinned = key_set(&p.pinned_personas);
    let excluded = key_set(&p.excluded_personas);

    let mut health = vendor_health(
        inputs.readings,
        now,
        candidates
            .iter()
            .cloned()
            .chain(inputs.agents.iter().map(|a| key(&a.vendor))),
    );
    for h in health.values_mut() {
        h.candidate = candidates.contains(&h.vendor);
        match target_check(h, p, &allowed) {
            Ok(()) => h.eligible = true,
            Err(why) => h.ineligible_reason = Some(why),
        }
    }
    let mut targets: Vec<&VendorHealth> = health.values().filter(|h| h.eligible).collect();
    targets.sort_by(|a, b| target_order(a, b));

    let blocked = inputs.history.last_apply_ms.and_then(|last| {
        let since = now.saturating_sub(last);
        (since < p.min_interval_ms).then(|| {
            format!(
                "the last balance apply ran {} min ago; `min_interval_ms` is {} min, so the next \
                 may run in {} min",
                since / 60_000,
                minutes(p.min_interval_ms),
                minutes(p.min_interval_ms - since)
            )
        })
    });

    let mut agents: Vec<&BalanceAgent> = inputs.agents.iter().collect();
    agents.sort_by(|a, b| {
        key(&a.persona)
            .cmp(&key(&b.persona))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    let mut skipped = Vec::new();
    let skip = |a: &BalanceAgent, reason: SkipReason, detail: String| SkippedAgent {
        persona: a.persona.clone(),
        session_id: a.session_id.clone(),
        vendor: key(&a.vendor),
        reason,
        detail,
    };
    let mut triggered: Vec<(&BalanceAgent, &VendorHealth, BalanceTrigger, String)> = Vec::new();
    for a in agents {
        let persona = key(&a.persona);
        let vendor = key(&a.vendor);
        if pinned.contains(&persona) {
            skipped.push(skip(a, SkipReason::Pinned, "in `pinned_personas`".into()));
            continue;
        }
        if excluded.contains(&persona) {
            skipped.push(skip(
                a,
                SkipReason::Excluded,
                "in `excluded_personas`".into(),
            ));
            continue;
        }
        let Some(h) = health.get(&vendor) else {
            skipped.push(skip(
                a,
                SkipReason::NoReading,
                "the agent names no vendor".into(),
            ));
            continue;
        };
        let Some((trigger, reason)) = trigger_of(h, p) else {
            let (why, detail) = match h.status {
                VendorStatus::Unknown => (
                    SkipReason::NoReading,
                    format!("{vendor} has no live quota reading"),
                ),
                _ => (
                    SkipReason::Healthy,
                    format!(
                        "{vendor} primary {:.1}% < {}%, secondary {:.1}% < {}%",
                        h.primary_pct(),
                        p.primary_trigger_pct,
                        h.secondary_pct(),
                        p.secondary_trigger_pct
                    ),
                ),
            };
            skipped.push(skip(a, why, detail));
            continue;
        };
        if let Some(why) = &a.remodel_blocker {
            skipped.push(skip(
                a,
                SkipReason::NotRemodelable,
                format!("{reason}; {why}"),
            ));
            continue;
        }
        if let Some(last) = inputs.history.persona_last_switch_ms.get(&persona) {
            let since = now.saturating_sub(*last);
            if since < p.cooldown_ms {
                skipped.push(skip(
                    a,
                    SkipReason::Cooldown,
                    format!(
                        "{reason}; moved {} min ago, and `cooldown_ms` is {} min ({} min left)",
                        since / 60_000,
                        minutes(p.cooldown_ms),
                        minutes(p.cooldown_ms - since)
                    ),
                ));
                continue;
            }
        }
        triggered.push((a, h, trigger, reason));
    }

    // Worst first: exhausted, then the highest score; then persona and session (already sorted,
    // and the sort is stable).
    triggered.sort_by(|(_, x, _, _), (_, y, _, _)| {
        let exhausted = |h: &VendorHealth| h.status == VendorStatus::Exhausted;
        exhausted(y)
            .cmp(&exhausted(x))
            .then_with(|| y.score.unwrap_or(0.0).total_cmp(&x.score.unwrap_or(0.0)))
    });

    let max = p.max_switches_per_run as usize;
    let mut switches: Vec<PlannedSwitch> = Vec::new();
    let mut unknown_taken: BTreeSet<String> = BTreeSet::new();
    let mut personas_switched: BTreeSet<String> = BTreeSet::new();
    for (a, from, trigger, reason) in triggered {
        if personas_switched.contains(&key(&a.persona)) {
            skipped.push(skip(
                a,
                SkipReason::PersonaSwitched,
                format!(
                    "{reason}; another session of `{}` is switched in this run",
                    a.persona
                ),
            ));
            continue;
        }
        if switches.len() >= max {
            skipped.push(skip(
                a,
                SkipReason::RunLimit,
                format!("{reason}; `max_switches_per_run` ({max}) is reached by worse-off agents"),
            ));
            continue;
        }
        let target = targets.iter().find(|t| {
            t.vendor != from.vendor
                && !(t.status == VendorStatus::Unknown && unknown_taken.contains(&t.vendor))
        });
        let Some(t) = target else {
            skipped.push(skip(
                a,
                SkipReason::NoEligibleTarget,
                format!("{reason}; no vendor is under the target maximums"),
            ));
            continue;
        };
        if let Err(why) = remodelable(a, &t.vendor) {
            skipped.push(skip(
                a,
                SkipReason::NotRemodelable,
                format!(
                    "{reason}; a remodel to {} would be refused: {why}",
                    t.vendor
                ),
            ));
            continue;
        }
        if t.status == VendorStatus::Unknown {
            unknown_taken.insert(t.vendor.clone());
        }
        personas_switched.insert(key(&a.persona));
        let to = match t.score {
            Some(s) => format!("{} (score {s:.1}%)", t.vendor),
            None => format!("{} (no quota reading)", t.vendor),
        };
        switches.push(PlannedSwitch {
            persona: a.persona.clone(),
            session_id: a.session_id.clone(),
            project: a.project.clone(),
            from_vendor: from.vendor.clone(),
            to_vendor: t.vendor.clone(),
            trigger,
            reason: format!("{reason}; moving to {to}"),
            target_score: t.score,
            target_status: t.status,
        });
    }
    skipped.sort_by(|a, b| {
        key(&a.persona)
            .cmp(&key(&b.persona))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    Ok(BalancePlan {
        evaluated_at_ms: now,
        policy: p.clone(),
        blocked,
        switches,
        skipped,
        vendors: health.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;
    const HOUR: u64 = 3_600_000;

    fn reading(vendor: &str, window: QuotaWindowKind, pct: f64) -> WatchQuotaWindow {
        WatchQuotaWindow {
            vendor: vendor.into(),
            window,
            used_percent: pct,
            resets_at_ms: Some(NOW + HOUR),
            session: None,
            window_minutes: None,
            exhausted_reason: None,
        }
    }

    fn p(vendor: &str, pct: f64) -> WatchQuotaWindow {
        reading(vendor, QuotaWindowKind::Primary, pct)
    }

    fn s(vendor: &str, pct: f64) -> WatchQuotaWindow {
        reading(vendor, QuotaWindowKind::Secondary, pct)
    }

    fn agent(persona: &str, vendor: &str) -> BalanceAgent {
        BalanceAgent {
            persona: persona.into(),
            session_id: format!("{vendor}:{persona}"),
            vendor: vendor.into(),
            model: None,
            project: "ferroloop".into(),
            remodel_blocker: None,
        }
    }

    fn vendors() -> Vec<String> {
        ["antigravity", "claude", "codex", "grok", "muse"]
            .map(String::from)
            .to_vec()
    }

    fn plan_with(
        policy: &BalancePolicy,
        agents: &[BalanceAgent],
        readings: &[WatchQuotaWindow],
        history: &BalanceHistory,
    ) -> BalancePlan {
        plan_balance(&BalanceInputs {
            policy,
            now_ms: NOW,
            agents,
            readings,
            candidate_vendors: &vendors(),
            history,
        })
        .unwrap()
    }

    fn plan(agents: &[BalanceAgent], readings: &[WatchQuotaWindow]) -> BalancePlan {
        plan_with(
            &BalancePolicy::default(),
            agents,
            readings,
            &BalanceHistory::default(),
        )
    }

    fn moves(plan: &BalancePlan) -> Vec<(&str, &str, &str)> {
        plan.switches
            .iter()
            .map(|s| {
                (
                    s.persona.as_str(),
                    s.from_vendor.as_str(),
                    s.to_vendor.as_str(),
                )
            })
            .collect()
    }

    fn skip_of(plan: &BalancePlan, persona: &str) -> SkipReason {
        plan.skipped
            .iter()
            .find(|s| s.persona == persona)
            .unwrap_or_else(|| panic!("{persona} not skipped: {plan:#?}"))
            .reason
    }

    /// Every vendor known and cool except the ones named.
    fn cool_but(hot: &[WatchQuotaWindow]) -> Vec<WatchQuotaWindow> {
        let mut r = vec![
            p("antigravity", 50.0),
            p("claude", 40.0),
            p("codex", 30.0),
            p("grok", 60.0),
            p("muse", 20.0),
        ];
        r.retain(|x| {
            !hot.iter()
                .any(|h| h.vendor == x.vendor && h.window == x.window)
        });
        r.extend(hot.iter().cloned());
        r
    }

    #[test]
    fn defaults_are_the_documented_ones_and_valid() {
        let d = BalancePolicy::default();
        assert!(!d.enabled);
        assert_eq!(d.primary_trigger_pct, 90.0);
        assert_eq!(d.secondary_trigger_pct, 85.0);
        assert_eq!(d.target_max_primary_pct, 70.0);
        assert_eq!(d.target_max_secondary_pct, 70.0);
        assert_eq!(d.cooldown_ms, 30 * 60 * 1000);
        assert_eq!(d.min_interval_ms, 5 * 60 * 1000);
        assert_eq!(d.max_switches_per_run, 1);
        assert!(!d.allow_unknown_quota_targets);
        d.validate().unwrap();
        // A partial object is a whole policy; an unknown field is refused.
        let partial: BalancePolicy =
            serde_json::from_value(serde_json::json!({"cooldown_ms": 5})).unwrap();
        assert_eq!(partial.cooldown_ms, 5);
        assert_eq!(partial.primary_trigger_pct, 90.0);
        assert!(
            serde_json::from_value::<BalancePolicy>(serde_json::json!({"cooldown": 5})).is_err()
        );
    }

    #[test]
    fn validation_names_the_field() {
        let d = BalancePolicy::default;
        for (policy, field) in [
            (
                BalancePolicy {
                    primary_trigger_pct: f64::NAN,
                    ..d()
                },
                "primary_trigger_pct",
            ),
            (
                BalancePolicy {
                    secondary_trigger_pct: 100.5,
                    ..d()
                },
                "secondary_trigger_pct",
            ),
            (
                BalancePolicy {
                    target_max_primary_pct: -1.0,
                    ..d()
                },
                "target_max_primary_pct",
            ),
            (
                BalancePolicy {
                    target_max_secondary_pct: f64::INFINITY,
                    ..d()
                },
                "target_max_secondary_pct",
            ),
            // A trigger at or below its target maximum.
            (
                BalancePolicy {
                    primary_trigger_pct: 70.0,
                    ..d()
                },
                "primary_trigger_pct",
            ),
            (
                BalancePolicy {
                    target_max_secondary_pct: 90.0,
                    ..d()
                },
                "secondary_trigger_pct",
            ),
            (
                BalancePolicy {
                    max_switches_per_run: 0,
                    ..d()
                },
                "max_switches_per_run",
            ),
            (
                BalancePolicy {
                    pinned_personas: vec![" ".into()],
                    ..d()
                },
                "pinned_personas",
            ),
            (
                BalancePolicy {
                    excluded_target_vendors: vec![String::new()],
                    ..d()
                },
                "excluded_target_vendors",
            ),
            (
                BalancePolicy {
                    allowed_target_vendors: vec!["Codex".into()],
                    excluded_target_vendors: vec!["codex ".into()],
                    ..d()
                },
                "codex",
            ),
        ] {
            let err = policy.validate().unwrap_err();
            assert!(matches!(err, BalanceError::InvalidPolicy(_)));
            assert!(err.to_string().contains(field), "{field}: {err}");
            // The planner refuses it too.
            assert!(
                plan_balance(&BalanceInputs {
                    policy: &policy,
                    now_ms: NOW,
                    agents: &[],
                    readings: &[],
                    candidate_vendors: &[],
                    history: &BalanceHistory::default(),
                })
                .is_err()
            );
        }
        // Edges that are fine: 0 and 100, a zero cooldown and interval.
        BalancePolicy {
            primary_trigger_pct: 100.0,
            target_max_primary_pct: 0.0,
            cooldown_ms: 0,
            min_interval_ms: 0,
            ..d()
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn triggers_fire_at_the_threshold_not_below() {
        for (readings, want) in [
            (vec![p("claude", 89.99)], None),
            (vec![p("claude", 90.0)], Some(BalanceTrigger::Primary)),
            (vec![p("claude", 95.0)], Some(BalanceTrigger::Primary)),
            (vec![s("claude", 84.9)], None),
            (vec![s("claude", 85.0)], Some(BalanceTrigger::Secondary)),
            // Primary is checked before secondary.
            (
                vec![s("claude", 99.0), p("claude", 91.0)],
                Some(BalanceTrigger::Primary),
            ),
        ] {
            let plan = plan(&[agent("ada", "claude")], &cool_but(&readings));
            match want {
                None => assert_eq!(skip_of(&plan, "ada"), SkipReason::Healthy, "{readings:?}"),
                Some(t) => {
                    assert_eq!(plan.switches.len(), 1, "{readings:?}");
                    assert_eq!(plan.switches[0].trigger, t, "{readings:?}");
                    assert!(plan.switches[0].reason.contains("claude"));
                }
            }
        }
    }

    #[test]
    fn the_coolest_eligible_vendor_wins_by_score_then_windows_then_id() {
        let plan = plan(&[agent("ada", "claude")], &cool_but(&[p("claude", 95.0)]));
        assert_eq!(moves(&plan), vec![("ada", "claude", "muse")]);
        assert_eq!(plan.switches[0].target_score, Some(20.0));
        assert_eq!(plan.switches[0].target_status, VendorStatus::Known);
        // The score is max(primary, secondary): muse's secondary at 65 puts it behind codex (30).
        let plan = super::tests::plan(
            &[agent("ada", "claude")],
            &cool_but(&[p("claude", 95.0), s("muse", 65.0)]),
        );
        assert_eq!(moves(&plan), vec![("ada", "claude", "codex")]);
        // Equal scores: the lower primary wins, then the lower secondary, then the id.
        let tie = [
            p("claude", 95.0),
            p("codex", 50.0),
            s("codex", 10.0),
            p("muse", 10.0),
            s("muse", 50.0),
            p("grok", 50.0),
            s("grok", 20.0),
            p("antigravity", 50.0),
            s("antigravity", 20.0),
        ];
        let plan = super::tests::plan(&[agent("ada", "claude")], &tie);
        assert_eq!(moves(&plan), vec![("ada", "claude", "muse")]);
        let no_muse: Vec<_> = tie.iter().filter(|r| r.vendor != "muse").cloned().collect();
        let plan = super::tests::plan(&[agent("ada", "claude")], &no_muse);
        assert_eq!(moves(&plan), vec![("ada", "claude", "codex")]);
        let only_ties: Vec<_> = no_muse
            .iter()
            .filter(|r| r.vendor != "codex")
            .cloned()
            .collect();
        let plan = super::tests::plan(&[agent("ada", "claude")], &only_ties);
        assert_eq!(moves(&plan), vec![("ada", "claude", "antigravity")]);
    }

    #[test]
    fn target_maximums_are_inclusive_and_a_missing_window_reads_zero() {
        // At the maximum is fine; above it is not.
        let r = [p("claude", 95.0), p("codex", 70.0), s("codex", 70.0)];
        let plan = plan(&[agent("ada", "claude")], &r);
        assert_eq!(moves(&plan), vec![("ada", "claude", "codex")]);
        let r = [p("claude", 95.0), p("codex", 70.01)];
        let plan = super::tests::plan(&[agent("ada", "claude")], &r);
        assert_eq!(skip_of(&plan, "ada"), SkipReason::NoEligibleTarget);
        let codex = plan.vendors.iter().find(|v| v.vendor == "codex").unwrap();
        assert!(!codex.eligible);
        assert!(
            codex
                .ineligible_reason
                .as_deref()
                .unwrap()
                .contains("primary")
        );
        // codex reports only a secondary window: its primary counts as 0.
        let r = [p("claude", 95.0), s("codex", 10.0)];
        let plan = super::tests::plan(&[agent("ada", "claude")], &r);
        assert_eq!(moves(&plan), vec![("ada", "claude", "codex")]);
        let codex = plan.vendors.iter().find(|v| v.vendor == "codex").unwrap();
        assert_eq!(codex.score, Some(10.0));
        assert!(codex.primary.is_none());
    }

    #[test]
    fn exhausted_wins_over_low_windows_on_both_sides() {
        let mut limited = p("claude", 10.0);
        limited.exhausted_reason = Some("rate limited".into());
        let plan = plan(&[agent("ada", "claude")], &cool_but(&[limited.clone()]));
        assert_eq!(plan.switches[0].trigger, BalanceTrigger::Exhausted);
        assert!(plan.switches[0].reason.contains("rate limited"));
        let claude = plan.vendors.iter().find(|v| v.vendor == "claude").unwrap();
        assert_eq!(claude.status, VendorStatus::Exhausted);
        assert_eq!(claude.exhausted_reason.as_deref(), Some("rate limited"));
        // An exhausted vendor is never a target, however cool its windows read.
        let mut muse = p("muse", 1.0);
        muse.exhausted_reason = Some("out of credits".into());
        let r = [p("codex", 95.0), muse, p("grok", 60.0)];
        let plan = super::tests::plan(&[agent("bo", "codex")], &r);
        assert_eq!(moves(&plan), vec![("bo", "codex", "grok")]);
        let muse = plan.vendors.iter().find(|v| v.vendor == "muse").unwrap();
        assert!(!muse.eligible);
        assert!(
            muse.ineligible_reason
                .as_deref()
                .unwrap()
                .contains("exhausted")
        );
    }

    #[test]
    fn expired_readings_are_ignored() {
        let mut old_hot = p("claude", 99.0);
        old_hot.resets_at_ms = Some(NOW);
        let mut old_limited = s("claude", 1.0);
        old_limited.resets_at_ms = Some(NOW - 1);
        old_limited.exhausted_reason = Some("rate limited".into());
        let plan = plan(
            &[agent("ada", "claude")],
            &[old_hot, old_limited, p("codex", 10.0)],
        );
        assert_eq!(skip_of(&plan, "ada"), SkipReason::NoReading);
        let claude = plan.vendors.iter().find(|v| v.vendor == "claude").unwrap();
        assert_eq!(claude.status, VendorStatus::Unknown);
        assert_eq!(claude.score, None);
        // A reading with no reset time is live.
        let mut open = p("claude", 99.0);
        open.resets_at_ms = None;
        let plan = super::tests::plan(&[agent("ada", "claude")], &[open, p("codex", 10.0)]);
        assert_eq!(moves(&plan), vec![("ada", "claude", "codex")]);
    }

    #[test]
    fn readings_aggregate_per_vendor_case_insensitively_by_their_max() {
        let mut low = p("Claude ", 40.0);
        low.resets_at_ms = Some(NOW + 2 * HOUR);
        let mut high = p("claude", 92.0);
        high.resets_at_ms = Some(NOW + 3 * HOUR);
        let nan = p("CLAUDE", f64::NAN);
        let h = vendor_health(&[low, high, nan], NOW, []);
        assert_eq!(h.len(), 1);
        let claude = &h["claude"];
        assert_eq!(claude.status, VendorStatus::Known);
        let w = claude.primary.as_ref().unwrap();
        assert_eq!(w.used_percent, 92.0);
        assert_eq!(w.resets_at_ms, Some(NOW + 3 * HOUR));
        assert_eq!(
            w.readings, 2,
            "a reading that is not a number is not counted"
        );
        assert_eq!(claude.score, Some(92.0));
    }

    #[test]
    fn pinned_excluded_and_unremodelable_agents_are_skipped_with_reasons() {
        let mut blocked = agent("cy", "claude");
        blocked.remodel_blocker = Some("the hub did not start its runtime".into());
        let policy = BalancePolicy {
            pinned_personas: vec!["ADA".into()],
            excluded_personas: vec!["bo".into()],
            ..BalancePolicy::default()
        };
        let plan = plan_with(
            &policy,
            &[
                agent("ada", "claude"),
                agent("bo", "claude"),
                blocked,
                agent("di", "claude"),
            ],
            &cool_but(&[p("claude", 99.0)]),
            &BalanceHistory::default(),
        );
        assert_eq!(skip_of(&plan, "ada"), SkipReason::Pinned);
        assert_eq!(skip_of(&plan, "bo"), SkipReason::Excluded);
        assert_eq!(skip_of(&plan, "cy"), SkipReason::NotRemodelable);
        let cy = plan.skipped.iter().find(|s| s.persona == "cy").unwrap();
        assert!(cy.detail.contains("did not start"), "{}", cy.detail);
        assert_eq!(moves(&plan), vec![("di", "claude", "muse")]);
        // A pinned agent is pinned even on a healthy vendor.
        let plan = plan_with(
            &policy,
            &[agent("ada", "codex")],
            &cool_but(&[]),
            &BalanceHistory::default(),
        );
        assert_eq!(skip_of(&plan, "ada"), SkipReason::Pinned);
    }

    #[test]
    fn cooldown_is_per_persona_and_ends_on_time() {
        let policy = BalancePolicy::default();
        let r = cool_but(&[p("claude", 99.0)]);
        let agents = [agent("ada", "claude"), agent("bo", "claude")];
        let mut history = BalanceHistory::default();
        history
            .persona_last_switch_ms
            .insert("ada".into(), NOW - policy.cooldown_ms + 1);
        let policy2 = BalancePolicy {
            max_switches_per_run: 5,
            ..policy.clone()
        };
        let plan = plan_with(&policy2, &agents, &r, &history);
        assert_eq!(skip_of(&plan, "ada"), SkipReason::Cooldown);
        assert_eq!(moves(&plan), vec![("bo", "claude", "muse")]);
        assert!(plan.blocked.is_none());
        history
            .persona_last_switch_ms
            .insert("ada".into(), NOW - policy.cooldown_ms);
        let plan = plan_with(&policy2, &agents, &r, &history);
        assert_eq!(plan.switches.len(), 2, "{plan:#?}");
    }

    #[test]
    fn the_min_interval_blocks_the_whole_plan_but_still_shows_its_switches() {
        let policy = BalancePolicy::default();
        let r = cool_but(&[p("claude", 99.0)]);
        let mut history = BalanceHistory {
            last_apply_ms: Some(NOW - policy.min_interval_ms + 60_000),
            ..BalanceHistory::default()
        };
        let plan = plan_with(&policy, &[agent("ada", "claude")], &r, &history);
        let why = plan.blocked.as_deref().unwrap();
        assert!(why.contains("min_interval_ms"), "{why}");
        assert!(why.contains("in 1 min"), "{why}");
        assert_eq!(moves(&plan), vec![("ada", "claude", "muse")]);
        history.last_apply_ms = Some(NOW - policy.min_interval_ms);
        let plan = plan_with(&policy, &[agent("ada", "claude")], &r, &history);
        assert!(plan.blocked.is_none());
        // `record` notes the apply and every persona it ran a remodel of.
        let mut h = BalanceHistory::default();
        assert!(h.record(NOW, ["Ada", "bo"]));
        assert_eq!(h.last_apply_ms, Some(NOW));
        assert_eq!(h.persona_last_switch_ms.get("ada"), Some(&NOW));
        assert_eq!(h.persona_last_switch_ms.len(), 2);
    }

    #[test]
    fn an_apply_that_ran_no_remodel_records_nothing() {
        let mut h = BalanceHistory::default();
        assert!(!h.record(NOW, []));
        assert_eq!(h, BalanceHistory::default());
        // An earlier apply's time is kept, not moved forward.
        h.record(NOW - HOUR, ["ada"]);
        assert!(!h.record(NOW, std::iter::empty()));
        assert_eq!(h.last_apply_ms, Some(NOW - HOUR));
        assert_eq!(h.persona_last_switch_ms.get("ada"), Some(&(NOW - HOUR)));
    }

    #[test]
    fn a_persona_is_switched_once_per_run() {
        // ada has two sessions, both on hot vendors; ada's worse one (grok) moves.
        let mut second = agent("ada", "grok");
        second.session_id = "grok:ada".into();
        let policy = BalancePolicy {
            max_switches_per_run: 5,
            ..BalancePolicy::default()
        };
        let plan = plan_with(
            &policy,
            &[agent("ada", "claude"), second, agent("bo", "claude")],
            &cool_but(&[p("claude", 95.0), p("grok", 99.0)]),
            &BalanceHistory::default(),
        );
        assert_eq!(
            moves(&plan),
            vec![("ada", "grok", "muse"), ("bo", "claude", "muse")]
        );
        let again = plan
            .skipped
            .iter()
            .find(|s| s.session_id == "claude:ada")
            .unwrap();
        assert_eq!(again.reason, SkipReason::PersonaSwitched);
        assert!(again.detail.contains("another session"), "{}", again.detail);
        // Case does not make it another persona.
        let mut upper = agent("ADA", "claude");
        upper.session_id = "claude:ADA".into();
        let plan = plan_with(
            &policy,
            &[agent("ada", "grok"), upper],
            &cool_but(&[p("claude", 95.0), p("grok", 99.0)]),
            &BalanceHistory::default(),
        );
        assert_eq!(plan.switches.len(), 1, "{plan:#?}");
        assert_eq!(skip_of(&plan, "ADA"), SkipReason::PersonaSwitched);
    }

    #[test]
    fn a_switch_the_caller_refuses_is_skipped_and_frees_its_run_slot() {
        let r = cool_but(&[p("claude", 95.0)]);
        let agents = [agent("ada", "claude"), agent("bo", "claude")];
        let history = BalanceHistory::default();
        let mut asked: Vec<(String, String)> = Vec::new();
        let plan = plan_balance_checked(
            &BalanceInputs {
                policy: &BalancePolicy::default(),
                now_ms: NOW,
                agents: &agents,
                readings: &r,
                candidate_vendors: &vendors(),
                history: &history,
            },
            &mut |a, to| {
                asked.push((a.persona.clone(), to.to_string()));
                if a.persona == "ada" {
                    Err("tmux session `Ada` is shared".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        // ada is first (by persona) and refused; bo takes the one run slot.
        assert_eq!(moves(&plan), vec![("bo", "claude", "muse")]);
        assert_eq!(skip_of(&plan, "ada"), SkipReason::NotRemodelable);
        let ada = plan.skipped.iter().find(|s| s.persona == "ada").unwrap();
        assert!(ada.detail.contains("would be refused"), "{}", ada.detail);
        assert!(ada.detail.contains("is shared"), "{}", ada.detail);
        assert_eq!(
            asked,
            vec![
                ("ada".to_string(), "muse".to_string()),
                ("bo".to_string(), "muse".to_string())
            ],
            "asked once per chosen switch, in plan order"
        );
        // An unknown target refused for one agent stays free for the next.
        let only = [p("claude", 99.0)];
        let policy = BalancePolicy {
            allow_unknown_quota_targets: true,
            allowed_target_vendors: vec!["grok".into()],
            ..BalancePolicy::default()
        };
        let plan = plan_balance_checked(
            &BalanceInputs {
                policy: &policy,
                now_ms: NOW,
                agents: &agents,
                readings: &only,
                candidate_vendors: &vendors(),
                history: &history,
            },
            &mut |a, _| {
                if a.persona == "ada" {
                    Err("no".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert_eq!(moves(&plan), vec![("bo", "claude", "grok")]);
    }

    #[test]
    fn the_run_limit_keeps_the_worst_off_agents() {
        let r = cool_but(&[p("claude", 91.0), p("grok", 99.0), s("codex", 90.0)]);
        let agents = [
            agent("ada", "claude"),
            agent("bo", "grok"),
            agent("cy", "codex"),
        ];
        let plan = plan(&agents, &r);
        assert_eq!(moves(&plan), vec![("bo", "grok", "muse")]);
        assert_eq!(skip_of(&plan, "ada"), SkipReason::RunLimit);
        assert_eq!(skip_of(&plan, "cy"), SkipReason::RunLimit);
        let policy = BalancePolicy {
            max_switches_per_run: 2,
            ..BalancePolicy::default()
        };
        let plan = plan_with(&policy, &agents, &r, &BalanceHistory::default());
        // grok 99 first, then ada (claude 91) before cy (codex 90).
        assert_eq!(
            moves(&plan),
            vec![("bo", "grok", "muse"), ("ada", "claude", "muse")]
        );
        assert_eq!(skip_of(&plan, "cy"), SkipReason::RunLimit);
        // An exhausted vendor outranks any percentage.
        let mut limited = p("codex", 5.0);
        limited.exhausted_reason = Some("rate limited".into());
        let r = cool_but(&[p("grok", 99.0), limited]);
        let plan = super::tests::plan(&[agent("bo", "grok"), agent("cy", "codex")], &r);
        assert_eq!(moves(&plan), vec![("cy", "codex", "muse")]);
    }

    #[test]
    fn unknown_vendors_are_targets_only_when_allowed_last_and_once() {
        // Only claude and codex report; codex is too hot to take anyone.
        let r = [p("claude", 99.0), p("codex", 80.0)];
        let agents = [
            agent("ada", "claude"),
            agent("bo", "claude"),
            agent("cy", "claude"),
            agent("di", "claude"),
        ];
        let plan = plan(&agents, &r);
        assert!(plan.switches.is_empty());
        assert_eq!(skip_of(&plan, "ada"), SkipReason::NoEligibleTarget);
        let grok = plan.vendors.iter().find(|v| v.vendor == "grok").unwrap();
        assert_eq!(grok.status, VendorStatus::Unknown);
        assert!(
            grok.ineligible_reason
                .as_deref()
                .unwrap()
                .contains("allow_unknown")
        );

        let policy = BalancePolicy {
            allow_unknown_quota_targets: true,
            max_switches_per_run: 10,
            ..BalancePolicy::default()
        };
        let plan = plan_with(&policy, &agents, &r, &BalanceHistory::default());
        // One agent per unknown vendor, in vendor id order; with three unknown vendors the
        // fourth agent has nowhere to go.
        assert_eq!(
            moves(&plan),
            vec![
                ("ada", "claude", "antigravity"),
                ("bo", "claude", "grok"),
                ("cy", "claude", "muse")
            ]
        );
        assert_eq!(skip_of(&plan, "di"), SkipReason::NoEligibleTarget);
        assert!(plan.switches.iter().all(|s| s.target_score.is_none()));
        assert!(
            plan.switches
                .iter()
                .all(|s| s.target_status == VendorStatus::Unknown)
        );
        // A known cool vendor ranks before every unknown one, and takes more than one agent.
        let r = [p("claude", 99.0), p("codex", 60.0)];
        let plan = plan_with(&policy, &agents, &r, &BalanceHistory::default());
        assert_eq!(
            plan.switches
                .iter()
                .map(|s| s.to_vendor.as_str())
                .collect::<Vec<_>>(),
            vec!["codex", "codex", "codex", "codex"]
        );
    }

    #[test]
    fn allowed_and_excluded_target_vendors_narrow_the_candidates() {
        let r = cool_but(&[p("claude", 99.0)]);
        let policy = BalancePolicy {
            allowed_target_vendors: vec!["Grok".into(), "codex".into()],
            ..BalancePolicy::default()
        };
        let plan = plan_with(
            &policy,
            &[agent("ada", "claude")],
            &r,
            &BalanceHistory::default(),
        );
        assert_eq!(moves(&plan), vec![("ada", "claude", "codex")]);
        let policy = BalancePolicy {
            excluded_target_vendors: vec!["muse".into(), "codex".into()],
            ..BalancePolicy::default()
        };
        let plan = plan_with(
            &policy,
            &[agent("ada", "claude")],
            &r,
            &BalanceHistory::default(),
        );
        assert_eq!(moves(&plan), vec![("ada", "claude", "antigravity")]);
        // A vendor outside the candidate list (the registry) is never a target.
        let mut r = r;
        r.push(p("kimi", 0.0));
        let plan = super::tests::plan(&[agent("ada", "claude")], &r);
        assert_eq!(moves(&plan), vec![("ada", "claude", "muse")]);
        let kimi = plan.vendors.iter().find(|v| v.vendor == "kimi").unwrap();
        assert!(!kimi.candidate && !kimi.eligible);
    }

    #[test]
    fn the_plan_is_deterministic_whatever_the_input_order() {
        let policy = BalancePolicy {
            max_switches_per_run: 3,
            allow_unknown_quota_targets: true,
            ..BalancePolicy::default()
        };
        let mut agents = vec![
            agent("di", "claude"),
            agent("ada", "claude"),
            agent("cy", "grok"),
            agent("bo", "codex"),
            agent("eve", "muse"),
        ];
        let mut readings = vec![
            p("claude", 95.0),
            s("claude", 30.0),
            p("grok", 99.0),
            s("codex", 88.0),
            p("muse", 20.0),
            p("antigravity", 20.0),
        ];
        let mut cands = vendors();
        let history = BalanceHistory::default();
        let first = plan_balance(&BalanceInputs {
            policy: &policy,
            now_ms: NOW,
            agents: &agents,
            readings: &readings,
            candidate_vendors: &cands,
            history: &history,
        })
        .unwrap();
        for _ in 0..4 {
            agents.rotate_left(1);
            readings.rotate_left(2);
            cands.reverse();
            let again = plan_balance(&BalanceInputs {
                policy: &policy,
                now_ms: NOW,
                agents: &agents,
                readings: &readings,
                candidate_vendors: &cands,
                history: &history,
            })
            .unwrap();
            assert_eq!(again, first);
        }
        // grok 99 first; claude 95 (ada, then di); codex 88 hits the run limit.
        assert_eq!(
            moves(&first),
            vec![
                ("cy", "grok", "antigravity"),
                ("ada", "claude", "antigravity"),
                ("di", "claude", "antigravity")
            ]
        );
        assert_eq!(skip_of(&first, "bo"), SkipReason::RunLimit);
        assert_eq!(skip_of(&first, "eve"), SkipReason::Healthy);
        let personas: Vec<&str> = first.skipped.iter().map(|s| s.persona.as_str()).collect();
        assert_eq!(personas, vec!["bo", "eve"]);
        let ids: Vec<&str> = first.vendors.iter().map(|v| v.vendor.as_str()).collect();
        assert_eq!(ids, vec!["antigravity", "claude", "codex", "grok", "muse"]);
        // It round-trips through JSON.
        let json = serde_json::to_value(&first).unwrap();
        let back: BalancePlan = serde_json::from_value(json).unwrap();
        assert_eq!(back, first);
    }
}
