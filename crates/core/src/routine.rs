//! Pure routine models and bounded schedule calculations. No function here grants authority,
//! persists a claim, or dispatches work. A store transaction must enforce claim uniqueness.
use chrono::{
    DateTime, Datelike, Duration, LocalResult, NaiveDateTime, SecondsFormat, TimeZone, Timelike,
    Utc,
};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// An evaluation span is bounded to the maximum routine lifetime, plus transition padding.
const MAX_SPAN_SECS: i64 = 90 * 86_400;
const GAP_SEARCH_MINUTES: i64 = 2 * 24 * 60;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoutineError {
    #[error("invalid timestamp")]
    Timestamp,
    #[error("invalid schedule: {0}")]
    Schedule(String),
    #[error("schedule scan exceeds bounded range")]
    ScanLimit,
    #[error("routine validation failed: {0}")]
    Validation(String),
    #[error("revision overflow")]
    RevisionOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    Interval { every_secs: u64, anchor_utc: String },
    Calendar { cron: String, tz: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoutineTarget {
    FreshSession { template_id: String },
    BoundIncarnation { incarnation_id: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoutineWork {
    ExecutableOnly {
        gate_or_hook_id: String,
    },
    ModelTurn {
        prompt_template_digest: String,
        workflow_step: Option<String>,
        target: RoutineTarget,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    Day,
    Week,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allocation {
    pub per_firing_tokens: u64,
    pub per_firing_cost_micros: u64,
    pub window: Window,
    pub window_tokens: u64,
    pub window_cost_micros: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    Forbid,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatchUp {
    #[default]
    Skip,
    LatestOnce,
    Bounded {
        n: u8,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoutineState {
    Proposed,
    Approved,
    Paused { reason: String },
    Expired,
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub approved_by: String,
    pub approved_at: String,
    pub revision: u64,
    pub digest: String,
}
fn default_grace() -> u64 {
    300
}
fn default_firing_limit() -> u64 {
    21_600
}
fn default_daily_limit() -> u32 {
    24
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineSpec {
    pub routine_id: String,
    pub revision: u64,
    pub project: String,
    pub schedule: Schedule,
    pub work: RoutineWork,
    pub allocation: Allocation,
    pub concurrency: Concurrency,
    #[serde(default)]
    pub catch_up: CatchUp,
    #[serde(default = "default_grace")]
    pub misfire_grace_secs: u64,
    /// Owner-approved per-firing wall-time ceiling (default 6 h, maximum 24 h).
    #[serde(default = "default_firing_limit")]
    pub max_firing_secs: u64,
    #[serde(default = "default_daily_limit")]
    pub max_firings_per_day: u32,
    pub expires_at: String,
    pub state: RoutineState,
    pub approval: Option<Approval>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledInstant {
    pub scheduled_instant_utc: String,
    pub nominal_slots: Vec<String>,
    pub coalesced: bool,
}
/// Firing lifecycle evidence; transitions must be enforced transactionally by a future store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FiringState {
    Planned,
    Queued,
    EffectAccepted,
    SpawnRequested,
    Spawned,
    Dispatching,
    Sent,
    Accepted,
    Completed,
    Failed,
    Draining,
    Settled,
    Refused { reason: String },
    SkippedMissed,
    SkippedOverlap,
    Cancelled,
    Ambiguous,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiringRecord {
    pub firing_key: String,
    pub routine_id: String,
    pub revision: u64,
    pub scheduled_instant_utc: String,
    pub nominal_slots: Vec<String>,
    pub coalesced: bool,
    pub state: FiringState,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUpEvaluation {
    pub records: Vec<FiringRecord>,
    pub clock_step: bool,
}

fn parse_utc(s: &str) -> Result<DateTime<Utc>, RoutineError> {
    DateTime::parse_from_rfc3339(s)
        .map(|x| x.with_timezone(&Utc))
        .map_err(|_| RoutineError::Timestamp)
}
fn utc_string(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}
/// Hash canonical UTC instant and routine ID as u64-be length-prefixed UTF-8 fields.
pub fn firing_key(routine_id: &str, scheduled_instant_utc: &str) -> Result<String, RoutineError> {
    let instant = utc_string(parse_utc(scheduled_instant_utc)?);
    let mut hash = Sha256::new();
    for field in [routine_id.as_bytes(), instant.as_bytes()] {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field);
    }
    Ok(format!("{:x}", hash.finalize()))
}

// POSIX five-field numeric cron; each field supports lists, ranges, and steps.
// Unsupported extensions fail closed. Sunday is 0 or 7.
struct CronField {
    bits: Vec<bool>,
    wildcard: bool,
}
struct Cron {
    minute: CronField,
    hour: CronField,
    day: CronField,
    month: CronField,
    weekday: CronField,
}
impl CronField {
    fn parse(s: &str, low: usize, high: usize) -> Result<Self, RoutineError> {
        let invalid = || RoutineError::Schedule(format!("invalid cron field {s}"));
        let mut bits = vec![false; high + 1];
        for part in s.split(',') {
            let (range, step) = match part.split_once('/') {
                Some((r, n)) => (r, n.parse::<usize>().map_err(|_| invalid())?),
                None => (part, 1),
            };
            if step == 0 {
                return Err(invalid());
            }
            let (start, end) = if range == "*" {
                (low, high)
            } else if let Some((a, b)) = range.split_once('-') {
                (
                    a.parse().map_err(|_| invalid())?,
                    b.parse().map_err(|_| invalid())?,
                )
            } else {
                let n = range.parse().map_err(|_| invalid())?;
                (n, n)
            };
            if start < low || end > high || start > end {
                return Err(invalid());
            }
            for n in (start..=end).step_by(step) {
                bits[n] = true;
            }
        }
        Ok(Self {
            bits,
            wildcard: s.starts_with('*'),
        })
    }
    fn has(&self, n: usize) -> bool {
        self.bits.get(n).copied().unwrap_or(false)
    }
}
impl Cron {
    fn parse(input: &str) -> Result<Self, RoutineError> {
        let f: Vec<_> = input.split_whitespace().collect();
        if f.len() != 5 || input.len() > 256 {
            return Err(RoutineError::Schedule(
                "cron must have five numeric fields".into(),
            ));
        }
        Ok(Self {
            minute: CronField::parse(f[0], 0, 59)?,
            hour: CronField::parse(f[1], 0, 23)?,
            day: CronField::parse(f[2], 1, 31)?,
            month: CronField::parse(f[3], 1, 12)?,
            weekday: CronField::parse(f[4], 0, 7)?,
        })
    }
    fn matches(&self, local: NaiveDateTime) -> bool {
        let day = self.day.has(local.day() as usize);
        let wd = local.weekday().num_days_from_sunday() as usize;
        let weekday = self.weekday.has(wd) || (wd == 0 && self.weekday.has(7));
        let day_match = match (self.day.wildcard, self.weekday.wildcard) {
            (true, true) => true,
            (true, false) => weekday,
            (false, true) => day,
            (false, false) => day || weekday,
        };
        self.minute.has(local.minute() as usize)
            && self.hour.has(local.hour() as usize)
            && self.month.has(local.month() as usize)
            && day_match
    }
    fn validate_time_fields(&self, cap: u32) -> Result<(), RoutineError> {
        let times: Vec<usize> = (0..1440)
            .filter(|m| self.hour.has(m / 60) && self.minute.has(m % 60))
            .collect();
        if times.len() > cap as usize {
            return Err(RoutineError::Validation("cron can exceed daily cap".into()));
        }
        if times.windows(2).any(|pair| pair[1] - pair[0] < 5) {
            return Err(RoutineError::Validation(
                "cron period is below 300 seconds".into(),
            ));
        }
        Ok(())
    }
}
fn resolve_local(tz: Tz, local: NaiveDateTime) -> Result<DateTime<Utc>, RoutineError> {
    // First occurrence in an overlap. In a gap, walk to the first valid minute.
    for n in 0..=GAP_SEARCH_MINUTES {
        let time = local
            .checked_add_signed(Duration::minutes(n))
            .ok_or(RoutineError::Timestamp)?;
        match tz.from_local_datetime(&time) {
            LocalResult::Single(t) if n == 0 => return Ok(t.with_timezone(&Utc)),
            LocalResult::Ambiguous(a, b) if n == 0 => return Ok(a.min(b).with_timezone(&Utc)),
            LocalResult::Single(_) | LocalResult::Ambiguous(_, _) => {
                // A historical transition can end at a non-minute boundary.
                let minute = time
                    .checked_sub_signed(Duration::minutes(1))
                    .ok_or(RoutineError::Timestamp)?;
                for second in 1..=60 {
                    let candidate = minute
                        .checked_add_signed(Duration::seconds(second))
                        .ok_or(RoutineError::Timestamp)?;
                    match tz.from_local_datetime(&candidate) {
                        LocalResult::Single(t) => return Ok(t.with_timezone(&Utc)),
                        LocalResult::Ambiguous(a, b) => return Ok(a.min(b).with_timezone(&Utc)),
                        LocalResult::None => {}
                    }
                }
                return Err(RoutineError::Timestamp);
            }
            LocalResult::None => {}
        }
    }
    Err(RoutineError::Schedule("timezone gap exceeds bound".into()))
}
impl Schedule {
    /// Window and daily-cap timezone. Interval schedules use UTC.
    pub fn timezone(&self) -> Result<Tz, RoutineError> {
        match self {
            Self::Interval { .. } => Ok(chrono_tz::UTC),
            Self::Calendar { tz, .. } => tz
                .parse()
                .map_err(|_| RoutineError::Schedule("invalid IANA timezone".into())),
        }
    }
    /// Scan (from, to] and enforce the durable claim watermark, if supplied. This does not claim.
    pub fn instants(
        &self,
        from_utc: &str,
        to_utc: &str,
        watermark: Option<&str>,
    ) -> Result<Vec<ScheduledInstant>, RoutineError> {
        let from = parse_utc(from_utc)?;
        let to = parse_utc(to_utc)?;
        let mark = watermark.map(parse_utc).transpose()?;
        if to < from {
            return Ok(Vec::new());
        }
        if (to - from).num_seconds() > MAX_SPAN_SECS {
            return Err(RoutineError::ScanLimit);
        }
        let lower = mark.map_or(from, |m| from.max(m));
        if lower >= to {
            return Ok(Vec::new());
        }
        match self {
            Self::Interval {
                every_secs,
                anchor_utc,
            } => {
                let anchor = parse_utc(anchor_utc)?;
                if *every_secs < 300 || *every_secs > i64::MAX as u64 {
                    return Err(RoutineError::Schedule(
                        "interval outside allowed bounds".into(),
                    ));
                }
                let period = *every_secs as i64;
                let delta = lower
                    .timestamp()
                    .checked_sub(anchor.timestamp())
                    .ok_or(RoutineError::Timestamp)?;
                let mut n = delta.div_euclid(period).max(0);
                let mut out = Vec::new();
                loop {
                    let step = n.checked_mul(period).ok_or(RoutineError::Timestamp)?;
                    let duration = Duration::try_seconds(step).ok_or(RoutineError::Timestamp)?;
                    let next = anchor
                        .checked_add_signed(duration)
                        .ok_or(RoutineError::Timestamp)?;
                    if next <= lower {
                        n = n.checked_add(1).ok_or(RoutineError::Timestamp)?;
                        continue;
                    }
                    if next > to {
                        break;
                    }
                    out.push(ScheduledInstant {
                        scheduled_instant_utc: utc_string(next),
                        nominal_slots: vec![utc_string(next)],
                        coalesced: false,
                    });
                    n = n.checked_add(1).ok_or(RoutineError::Timestamp)?;
                }
                Ok(out)
            }
            Self::Calendar { cron, .. } => {
                let cron = Cron::parse(cron)?;
                let tz = self.timezone()?;
                let local_from = lower.with_timezone(&tz).naive_local();
                let local_to = to.with_timezone(&tz).naive_local();
                // Transition padding covers the largest supported local gap and unusual offset changes.
                let start = local_from
                    .min(local_to)
                    .checked_sub_signed(Duration::days(2))
                    .ok_or(RoutineError::Timestamp)?;
                let end = local_from
                    .max(local_to)
                    .checked_add_signed(Duration::days(2))
                    .ok_or(RoutineError::Timestamp)?;
                let mut local = start
                    .date()
                    .and_hms_opt(0, 0, 0)
                    .ok_or(RoutineError::Timestamp)?;
                let mut slots: BTreeMap<DateTime<Utc>, Vec<String>> = BTreeMap::new();
                while local <= end {
                    if cron.matches(local) {
                        let instant = resolve_local(tz, local)?;
                        if instant > lower && instant <= to {
                            slots
                                .entry(instant)
                                .or_default()
                                .push(local.format("%Y-%m-%dT%H:%M:%S").to_string());
                        }
                    }
                    local = local
                        .checked_add_signed(Duration::minutes(1))
                        .ok_or(RoutineError::Timestamp)?;
                }
                Ok(slots
                    .into_iter()
                    .map(|(instant, nominal_slots)| ScheduledInstant {
                        scheduled_instant_utc: utc_string(instant),
                        coalesced: nominal_slots.len() > 1,
                        nominal_slots,
                    })
                    .collect())
            }
        }
    }
}
impl RoutineSpec {
    /// Validate a proposed approval interval; authorization and durable state changes are external.
    pub fn validate_for_approval(&self, at: &str) -> Result<(), RoutineError> {
        let now = parse_utc(at)?;
        let end = parse_utc(&self.expires_at)?;
        if end <= now || end - now > Duration::days(90) {
            return Err(RoutineError::Validation(
                "expiry must be in the next 90 days".into(),
            ));
        }
        if self.routine_id.trim().is_empty()
            || self.project.trim().is_empty()
            || self.max_firings_per_day == 0
            || self.max_firings_per_day > 288
            || self.misfire_grace_secs > 3600
            || !(60..=86_400).contains(&self.max_firing_secs)
        {
            return Err(RoutineError::Validation("invalid routine bounds".into()));
        }
        if self.allocation.per_firing_tokens == 0
            || self.allocation.per_firing_cost_micros == 0
            || self.allocation.window_tokens < self.allocation.per_firing_tokens
            || self.allocation.window_cost_micros < self.allocation.per_firing_cost_micros
        {
            return Err(RoutineError::Validation("invalid allocation".into()));
        }
        if matches!(self.catch_up, CatchUp::Bounded { n: 4..=u8::MAX }) {
            return Err(RoutineError::Validation(
                "catch-up bound must be 0..=3".into(),
            ));
        }
        let tz = self.schedule.timezone()?;
        match &self.schedule {
            Schedule::Interval {
                every_secs,
                anchor_utc,
            } => {
                parse_utc(anchor_utc)?;
                if *every_secs < 300
                    || *every_secs > i64::MAX as u64
                    || 86_400u64.div_ceil(*every_secs) > u64::from(self.max_firings_per_day)
                {
                    return Err(RoutineError::Validation(
                        "interval exceeds firing limit or period".into(),
                    ));
                }
            }
            Schedule::Calendar { cron, .. } => {
                let parsed = Cron::parse(cron)?;
                parsed.validate_time_fields(self.max_firings_per_day)?;
                // Include all DST coalescence and reject any actual short period across dates.
                // Include boundary days so a short interval or daily overflow crossing an
                // approval/expiry boundary cannot escape validation.
                let before = now
                    .checked_sub_signed(Duration::days(2))
                    .ok_or(RoutineError::Timestamp)?;
                let after = end
                    .checked_add_signed(Duration::days(2))
                    .ok_or(RoutineError::Timestamp)?;
                let mut slots = self.schedule.instants(&utc_string(before), at, None)?;
                slots.extend(self.schedule.instants(at, &self.expires_at, None)?);
                slots.extend(
                    self.schedule
                        .instants(&self.expires_at, &utc_string(after), None)?,
                );
                let mut days: BTreeMap<_, u32> = BTreeMap::new();
                let mut previous = None;
                for slot in slots {
                    let instant = parse_utc(&slot.scheduled_instant_utc)?;
                    if previous.is_some_and(|p| instant - p < Duration::seconds(300)) {
                        return Err(RoutineError::Validation(
                            "cron period is below 300 seconds".into(),
                        ));
                    }
                    previous = Some(instant);
                    let count = days
                        .entry(instant.with_timezone(&tz).date_naive())
                        .or_default();
                    *count += 1;
                    if *count > self.max_firings_per_day {
                        return Err(RoutineError::Validation("cron exceeds daily cap".into()));
                    }
                }
            }
        }
        Ok(())
    }
    /// Calculate the bounded firing deadline. This does not perform or authorize admission.
    pub fn firing_deadline(
        &self,
        scheduled: &str,
        next: &str,
        committed: &str,
    ) -> Result<String, RoutineError> {
        let scheduled = parse_utc(scheduled)?;
        let next = parse_utc(next)?;
        let committed = parse_utc(committed)?;
        let expiry = parse_utc(&self.expires_at)?;
        if committed < scheduled
            || next <= scheduled
            || committed >= expiry
            || !(60..=86_400).contains(&self.max_firing_secs)
            || self.misfire_grace_secs > 3600
        {
            return Err(RoutineError::Validation(
                "invalid firing deadline input".into(),
            ));
        }
        let cap = Duration::seconds(self.max_firing_secs as i64);
        let elapsed = committed - scheduled;
        let deadline = if elapsed <= Duration::seconds(60) {
            next.min(
                scheduled
                    .checked_add_signed(cap)
                    .ok_or(RoutineError::Timestamp)?,
            )
        } else {
            if elapsed > Duration::seconds(self.misfire_grace_secs as i64) {
                return Err(RoutineError::Validation("expired candidate".into()));
            }
            committed
                .checked_add_signed((next - scheduled).min(cap))
                .ok_or(RoutineError::Timestamp)?
        }
        .min(expiry);
        if deadline - committed < Duration::seconds(60) {
            return Err(RoutineError::Validation("expired candidate".into()));
        }
        Ok(utc_string(deadline))
    }

    /// Return paused current revision and a new proposed revision. Caller atomically persists both.
    pub fn edited(&self) -> Result<(Self, Self), RoutineError> {
        let mut old = self.clone();
        if matches!(old.state, RoutineState::Approved) {
            old.state = RoutineState::Paused {
                reason: "edit_pending".into(),
            };
        }
        let mut next = self.clone();
        next.revision = self
            .revision
            .checked_add(1)
            .ok_or(RoutineError::RevisionOverflow)?;
        next.state = RoutineState::Proposed;
        next.approval = None;
        Ok((old, next))
    }
    /// Pure recovery calculation. Candidate means eligible for future admission, never a claim.
    pub fn evaluate_catch_up(
        &self,
        from: &str,
        now: &str,
        watermark: Option<&str>,
        wall_step_secs: i64,
    ) -> Result<CatchUpEvaluation, RoutineError> {
        let now_time = parse_utc(now)?;
        let slots = self.schedule.instants(from, now, watermark)?;
        let eligible = slots
            .iter()
            .filter(|slot| {
                parse_utc(&slot.scheduled_instant_utc).is_ok_and(|t| {
                    now_time - t <= Duration::seconds(self.misfire_grace_secs.min(3600) as i64)
                })
            })
            .count();
        let limit = match self.catch_up {
            CatchUp::Skip => 0,
            CatchUp::LatestOnce => 1,
            CatchUp::Bounded { n: 0..=3 } => {
                if let CatchUp::Bounded { n } = self.catch_up {
                    n as usize
                } else {
                    unreachable!()
                }
            }
            _ => {
                return Err(RoutineError::Validation(
                    "catch-up bound must be 0..=3".into(),
                ));
            }
        };
        let first_candidate = slots.len().saturating_sub(eligible.min(limit));
        let mut records = Vec::with_capacity(slots.len());
        for (i, slot) in slots.into_iter().enumerate() {
            records.push(FiringRecord {
                firing_key: firing_key(&self.routine_id, &slot.scheduled_instant_utc)?,
                routine_id: self.routine_id.clone(),
                revision: self.revision,
                scheduled_instant_utc: slot.scheduled_instant_utc,
                nominal_slots: slot.nominal_slots,
                coalesced: slot.coalesced,
                state: if i >= first_candidate {
                    FiringState::Planned
                } else {
                    FiringState::SkippedMissed
                },
            });
        }
        Ok(CatchUpEvaluation {
            records,
            clock_step: wall_step_secs.unsigned_abs() > 120,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firing_key_unique_and_stable_across_restart() {
        assert_eq!(
            firing_key("r", "2026-01-01T00:00:00Z").unwrap(),
            firing_key("r", "2025-12-31T19:00:00-05:00").unwrap()
        );
        assert_ne!(
            firing_key("r", "2026-01-01T00:00:00Z").unwrap(),
            firing_key("r", "2026-01-01T00:05:00Z").unwrap()
        );
    }

    #[test]
    fn dst_gap_collision_coalesces_one_firing_with_nominal_slots() {
        let schedule = Schedule::Calendar {
            cron: "0,30 2,3 * * *".into(),
            tz: "America/New_York".into(),
        };
        let slots = schedule
            .instants("2026-03-08T06:59:00Z", "2026-03-08T07:00:00Z", None)
            .unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].scheduled_instant_utc, "2026-03-08T07:00:00Z");
        assert_eq!(slots[0].nominal_slots.len(), 3);
        assert!(slots[0].coalesced);
    }
    fn spec(schedule: Schedule) -> RoutineSpec {
        RoutineSpec {
            routine_id: "r1".into(),
            revision: 7,
            project: "p".into(),
            schedule,
            work: RoutineWork::ExecutableOnly {
                gate_or_hook_id: "g".into(),
            },
            allocation: Allocation {
                per_firing_tokens: 100,
                per_firing_cost_micros: 10,
                window: Window::Day,
                window_tokens: 1000,
                window_cost_micros: 100,
            },
            concurrency: Concurrency::Forbid,
            catch_up: CatchUp::Skip,
            misfire_grace_secs: 3600,
            max_firing_secs: 21_600,
            max_firings_per_day: 288,
            expires_at: "2026-03-09T00:00:00Z".into(),
            state: RoutineState::Approved,
            approval: Some(Approval {
                approved_by: "owner".into(),
                approved_at: "2026-03-01T00:00:00Z".into(),
                revision: 7,
                digest: "abc".into(),
            }),
        }
    }
    fn interval() -> Schedule {
        Schedule::Interval {
            every_secs: 300,
            anchor_utc: "2026-03-08T00:00:00Z".into(),
        }
    }
    fn calendar(cron: &str) -> Schedule {
        Schedule::Calendar {
            cron: cron.into(),
            tz: "America/New_York".into(),
        }
    }

    #[test]
    fn dst_gap_fires_once_after_gap() {
        let events = calendar("30 2 * * *")
            .instants("2026-03-08T06:00:00Z", "2026-03-08T08:00:00Z", None)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scheduled_instant_utc, "2026-03-08T07:00:00Z");
    }
    #[test]
    fn dst_overlap_fires_first_occurrence() {
        let events = calendar("30 1 * * *")
            .instants("2026-11-01T04:00:00Z", "2026-11-01T08:00:00Z", None)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scheduled_instant_utc, "2026-11-01T05:30:00Z");
    }
    #[test]
    fn backward_clock_jump_never_refires() {
        let s = spec(interval());
        let eval = s
            .evaluate_catch_up(
                "2026-03-08T00:00:00Z",
                "2026-03-07T23:55:00Z",
                Some("2026-03-08T00:00:00Z"),
                -300,
            )
            .unwrap();
        assert!(eval.records.is_empty());
        assert!(eval.clock_step);
    }
    #[test]
    fn forward_jump_applies_catch_up_policy() {
        let mut s = spec(interval());
        s.catch_up = CatchUp::LatestOnce;
        let eval = s
            .evaluate_catch_up("2026-03-08T00:00:00Z", "2026-03-08T00:20:00Z", None, 1200)
            .unwrap();
        assert!(eval.clock_step);
        assert_eq!(eval.records.len(), 4);
        assert_eq!(
            eval.records
                .iter()
                .filter(|r| r.state == FiringState::Planned)
                .count(),
            1
        );
        assert_eq!(
            eval.records[3].scheduled_instant_utc,
            "2026-03-08T00:20:00Z"
        );
    }
    #[test]
    fn catch_up_bounded_never_exceeds_n() {
        let mut s = spec(interval());
        s.catch_up = CatchUp::Bounded { n: 2 };
        let e = s
            .evaluate_catch_up("2026-03-08T00:00:00Z", "2026-03-08T00:20:00Z", None, 120)
            .unwrap();
        assert!(!e.clock_step);
        assert_eq!(
            e.records
                .iter()
                .filter(|r| r.state == FiringState::Planned)
                .map(|r| r.scheduled_instant_utc.as_str())
                .collect::<Vec<_>>(),
            ["2026-03-08T00:15:00Z", "2026-03-08T00:20:00Z"]
        );
    }
    #[test]
    fn missed_instants_always_recorded() {
        let mut s = spec(interval());
        s.catch_up = CatchUp::LatestOnce;
        s.misfire_grace_secs = 300;
        let e = s
            .evaluate_catch_up("2026-03-08T00:00:00Z", "2026-03-08T00:20:00Z", None, 121)
            .unwrap();
        assert_eq!(e.records.len(), 4);
        assert!(
            e.records[..3]
                .iter()
                .all(|r| r.state == FiringState::SkippedMissed)
        );
        s.catch_up = CatchUp::Skip;
        assert!(
            s.evaluate_catch_up("2026-03-08T00:00:00Z", "2026-03-08T00:20:00Z", None, 0)
                .unwrap()
                .records
                .iter()
                .all(|r| r.state == FiringState::SkippedMissed)
        );
    }
    #[test]
    fn cron_exceeding_daily_cap_refused_at_approval() {
        let mut s = spec(calendar("0 * * * *"));
        s.max_firings_per_day = 23;
        assert!(s.validate_for_approval("2026-03-01T00:00:00Z").is_err());
        s.max_firings_per_day = 24;
        assert!(s.validate_for_approval("2026-03-01T00:00:00Z").is_ok());
        s.schedule = calendar("* * * * *");
        s.max_firings_per_day = 288;
        assert!(s.validate_for_approval("2026-03-01T00:00:00Z").is_err());
    }
    #[test]
    fn revision_change_never_refires_claimed_instant() {
        let mut s = spec(interval());
        let first = s
            .evaluate_catch_up("2026-03-08T00:00:00Z", "2026-03-08T00:10:00Z", None, 0)
            .unwrap();
        s.revision += 1;
        let later = s
            .evaluate_catch_up(
                "2026-03-08T00:00:00Z",
                "2026-03-08T00:15:00Z",
                Some("2026-03-08T00:10:00Z"),
                0,
            )
            .unwrap();
        assert_eq!(later.records.len(), 1);
        assert!(
            first
                .records
                .iter()
                .all(|a| a.firing_key != later.records[0].firing_key)
        );
        assert_eq!(
            firing_key("r1", "2026-03-08T00:10:00Z").unwrap(),
            first.records[1].firing_key
        );
    }
    #[test]
    fn watermark_blocks_reevaluation_after_restart_and_clock_jump() {
        let s = interval();
        let events = s
            .instants(
                "2026-03-08T00:00:00Z",
                "2026-03-08T00:20:00Z",
                Some("2026-03-08T00:15:00Z"),
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scheduled_instant_utc, "2026-03-08T00:20:00Z");
        assert!(
            s.instants(
                "2026-03-08T00:00:00Z",
                "2026-03-08T00:10:00Z",
                Some("2026-03-08T00:15:00Z")
            )
            .unwrap()
            .is_empty()
        );
    }
    #[test]
    fn edit_pauses_current_revision_until_new_approval() {
        let s = spec(interval());
        let (old, new) = s.edited().unwrap();
        assert!(matches!(old.state, RoutineState::Paused { .. }));
        assert_eq!(old.revision, 7);
        assert_eq!(new.revision, 8);
        assert_eq!(new.state, RoutineState::Proposed);
        assert!(new.approval.is_none());
        assert_eq!(s.state, RoutineState::Approved);
    }
    #[test]
    fn interval_windows_use_utc() {
        assert_eq!(interval().timezone().unwrap(), chrono_tz::UTC);
    }
    #[test]
    fn invalid_scans_and_overflow_fail_closed() {
        assert!(
            interval()
                .instants("2026-01-01T00:00:00Z", "2026-06-01T00:00:00Z", None)
                .is_err()
        );
        assert!(
            calendar("@daily")
                .instants("2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z", None)
                .is_err()
        );
        assert!(firing_key("r", "not-a-date").is_err());
        let mut s = spec(interval());
        s.catch_up = CatchUp::Bounded { n: 4 };
        assert!(s.validate_for_approval("2026-03-01T00:00:00Z").is_err());
        s.revision = u64::MAX;
        assert!(s.edited().is_err());
    }
    #[test]
    fn fractional_anchor_is_not_skipped() {
        let s = Schedule::Interval {
            every_secs: 300,
            anchor_utc: "2026-03-08T00:00:00.500Z".into(),
        };
        let slots = s
            .instants("2026-03-08T00:00:00.400Z", "2026-03-08T00:00:01Z", None)
            .unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].scheduled_instant_utc, "2026-03-08T00:00:00.500Z");
    }
    #[test]
    fn catch_up_deadline_uses_fresh_bounded_window_and_rejects_unviable() {
        let s = spec(interval());
        assert_eq!(
            s.firing_deadline(
                "2026-03-08T00:00:00Z",
                "2026-03-08T00:05:00Z",
                "2026-03-08T00:02:00Z"
            )
            .unwrap(),
            "2026-03-08T00:07:00Z"
        );
        assert_eq!(
            s.firing_deadline(
                "2026-03-08T00:00:00Z",
                "2026-03-08T00:05:00Z",
                "2026-03-08T00:00:30Z"
            )
            .unwrap(),
            "2026-03-08T00:05:00Z"
        );
        assert!(
            s.firing_deadline(
                "2026-03-08T00:00:00Z",
                "2026-03-08T00:05:00Z",
                "2026-03-08T02:00:00Z"
            )
            .is_err()
        );
        assert!(
            s.firing_deadline(
                "2026-03-08T23:55:00Z",
                "2026-03-09T00:00:00Z",
                "2026-03-08T23:59:30Z"
            )
            .is_err()
        );
    }
    #[test]
    fn omitted_policy_values_use_conservative_defaults() {
        let mut value = serde_json::to_value(spec(interval())).unwrap();
        let obj = value.as_object_mut().unwrap();
        for field in [
            "catch_up",
            "misfire_grace_secs",
            "max_firing_secs",
            "max_firings_per_day",
        ] {
            obj.remove(field);
        }
        let decoded: RoutineSpec = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.catch_up, CatchUp::Skip);
        assert_eq!(decoded.misfire_grace_secs, 300);
        assert_eq!(decoded.max_firing_secs, 21_600);
        assert_eq!(decoded.max_firings_per_day, 24);
    }
}
