use super::{
    Attribution, IdentityMappingDocument, SourceUsageDocument, UsageBucket, resolve_persona,
};
use fl_store::AnyStore;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const COLLECTION: &str = "watch_usage_v1";
const HOUR: u64 = 3_600_000;

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("invalid usage query: {0}")]
    Invalid(String),
    #[error("usage unsupported on redb backend")]
    Unsupported,
    #[error("usage store error: {0}")]
    Store(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageGroupBy {
    Persona,
    Vendor,
    Model,
}
impl std::str::FromStr for UsageGroupBy {
    type Err = UsageError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "persona" => Ok(Self::Persona),
            "vendor" => Ok(Self::Vendor),
            "model" => Ok(Self::Model),
            _ => Err(UsageError::Invalid(
                "by must be persona, vendor, or model".into(),
            )),
        }
    }
}
#[derive(Debug, Clone)]
pub struct UsageReportQuery {
    pub since_ms: u64,
    pub by: UsageGroupBy,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageRow {
    pub key: String,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub turns: Option<u64>,
    pub subagents: u64,
    pub unavailable: super::CategoryUnavailable,
    pub turns_unavailable: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReportCoverage {
    pub incomplete: bool,
    pub pending: bool,
    pub unavailable_sources: u64,
    pub malformed_lines: u64,
    pub oversized_lines: u64,
    pub last_success_ms: Option<u64>,
    pub unattributed_sources: u64,
    pub ambiguous_sources: u64,
    pub unknown_launch_times: u64,
    pub undated_observations: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    pub requested_start_ms: u64,
    pub requested_end_ms: u64,
    pub effective_start_ms: u64,
    pub effective_end_ms: u64,
    pub by: UsageGroupBy,
    pub unit: String,
    pub rows: Vec<UsageRow>,
    pub undated: UsageRow,
    pub coverage: ReportCoverage,
}
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CollectorStatus {
    pub last_success_ms: Option<u64>,
    pub pending: bool,
    pub incomplete: bool,
}

/// Parse a positive relative duration or an absolute RFC3339 timestamp, relative to `now_ms`.
pub fn parse_since(value: &str, now_ms: u64) -> Result<u64, UsageError> {
    if let Some(split) = value.find(|c: char| !c.is_ascii_digit()) {
        let (number, unit) = value.split_at(split);
        if !number.is_empty() && ["s", "m", "h", "d", "w"].contains(&unit) {
            let n = number
                .parse::<u64>()
                .map_err(|_| UsageError::Invalid("duration is too large".into()))?;
            let multiplier = match unit {
                "s" => 1000,
                "m" => 60_000,
                "h" => HOUR,
                "d" => 24 * HOUR,
                _ => 168 * HOUR,
            };
            let delta = n
                .checked_mul(multiplier)
                .filter(|n| *n > 0)
                .ok_or_else(|| {
                    UsageError::Invalid("duration must be positive and in range".into())
                })?;
            return now_ms
                .checked_sub(delta)
                .ok_or_else(|| UsageError::Invalid("duration precedes the Unix epoch".into()));
        }
    }
    let invalid =
        || UsageError::Invalid("since must be a duration (e.g. 48h) or UTC timestamp".into());
    let utc = value
        .strip_suffix('Z')
        .or_else(|| value.strip_suffix("+00:00"))
        .ok_or_else(invalid)?;
    let (whole, fraction) = utc.split_once('.').unwrap_or((utc, ""));
    if whole.len() != 19
        || (!fraction.is_empty()
            && (!fraction.bytes().all(|b| b.is_ascii_digit()) || fraction.len() > 9))
        || (utc.contains('.') && fraction.is_empty())
    {
        return Err(invalid());
    }
    let fraction = format!("{fraction:0<3}");
    let canonical = format!("{whole}.{}Z", &fraction[..3]);
    let millis = fl_core::at::At::parse(&canonical)
        .map_err(|_| invalid())?
        .unix_millis();
    if millis >= now_ms {
        return Err(UsageError::Invalid("since must be before now".into()));
    }
    Ok(millis)
}

#[derive(Default)]
struct Accum {
    row: UsageRow,
    unavailable: [bool; 5],
}
impl Accum {
    fn add(&mut self, b: &UsageBucket, incomplete: &mut bool) {
        fn add_value(
            total: &mut Option<u64>,
            v: Option<u64>,
            missing: bool,
            flag: &mut bool,
            incomplete: &mut bool,
        ) {
            *flag |= missing || v.is_none();
            if let Some(n) = v {
                *total = Some(match total.unwrap_or(0).checked_add(n) {
                    Some(sum) => sum,
                    None => {
                        *incomplete = true;
                        u64::MAX
                    }
                });
            }
        }
        add_value(
            &mut self.row.input,
            b.counts.input,
            b.unavailable.input,
            &mut self.unavailable[0],
            incomplete,
        );
        add_value(
            &mut self.row.output,
            b.counts.output,
            b.unavailable.output,
            &mut self.unavailable[1],
            incomplete,
        );
        add_value(
            &mut self.row.cache_read,
            b.counts.cache_read,
            b.unavailable.cache_read,
            &mut self.unavailable[2],
            incomplete,
        );
        add_value(
            &mut self.row.cache_write,
            b.counts.cache_write,
            b.unavailable.cache_write,
            &mut self.unavailable[3],
            incomplete,
        );
        add_value(
            &mut self.row.turns,
            Some(b.turns),
            b.turns_unavailable,
            &mut self.unavailable[4],
            incomplete,
        );
    }
    fn finish(mut self) -> UsageRow {
        self.row.unavailable = super::CategoryUnavailable {
            input: self.unavailable[0],
            output: self.unavailable[1],
            cache_read: self.unavailable[2],
            cache_write: self.unavailable[3],
        };
        self.row.turns_unavailable = self.unavailable[4];
        self.row
    }
}

pub fn aggregate_documents(
    sources: &[SourceUsageDocument],
    identities: &[IdentityMappingDocument],
    status: Option<CollectorStatus>,
    query: &UsageReportQuery,
    now_ms: u64,
) -> Result<UsageReport, UsageError> {
    if query.since_ms >= now_ms {
        return Err(UsageError::Invalid("since must be before now".into()));
    }
    let start = query.since_ms / HOUR * HOUR;
    let end = now_ms
        .checked_add((HOUR - now_ms % HOUR) % HOUR)
        .ok_or_else(|| UsageError::Invalid("end is outside supported range".into()))?;
    let mut coverage = ReportCoverage::default();
    if let Some(s) = status {
        coverage.last_success_ms = s.last_success_ms;
        coverage.pending = s.pending;
        coverage.incomplete = s.incomplete;
    } else {
        coverage.incomplete = true;
    }
    let mut rows: BTreeMap<String, Accum> = BTreeMap::new();
    let mut undated = Accum::default();
    undated.row.key = "undated".into();
    for source in sources {
        coverage.incomplete |= source.coverage.incomplete
            || source.coverage.ambiguous_generation
            || source.coverage.unverified_prefix;
        coverage.pending |= source.coverage.pending;
        coverage.unavailable_sources = coverage
            .unavailable_sources
            .saturating_add(u64::from(source.coverage.unavailable));
        coverage.malformed_lines = coverage
            .malformed_lines
            .saturating_add(source.coverage.malformed_lines);
        coverage.oversized_lines = coverage
            .oversized_lines
            .saturating_add(source.coverage.oversized_lines);
        let attribution = resolve_persona(&source.session_id, identities);
        match attribution {
            Attribution::Unattributed => coverage.unattributed_sources += 1,
            Attribution::Ambiguous => coverage.ambiguous_sources += 1,
            _ => (),
        }
        let persona = match attribution {
            Attribution::Persona(p) => p,
            _ => "unattributed".into(),
        };
        let vendor = match source.vendor {
            super::UsageVendor::Antigravity => "antigravity",
            super::UsageVendor::Codex => "codex",
            super::UsageVendor::Claude => "claude",
            super::UsageVendor::Muse => "muse",
            super::UsageVendor::Grok => "grok",
        };
        if source.coverage.unavailable && source.buckets.is_empty() {
            let key = match query.by {
                UsageGroupBy::Persona => persona.clone(),
                UsageGroupBy::Vendor => vendor.into(),
                UsageGroupBy::Model => "unknown".into(),
            };
            let row = rows.entry(key.clone()).or_default();
            row.row.key = key;
            row.unavailable = [true; 5];
        }
        for b in &source.buckets {
            let Some(hour) = b.hour_ms else {
                coverage.undated_observations =
                    coverage.undated_observations.saturating_add(b.observations);
                undated.add(b, &mut coverage.incomplete);
                continue;
            };
            if hour < start || hour >= end {
                continue;
            }
            let key = match query.by {
                UsageGroupBy::Persona => persona.clone(),
                UsageGroupBy::Vendor => vendor.into(),
                UsageGroupBy::Model => b.model.clone().unwrap_or_else(|| "unknown".into()),
            };
            let row = rows.entry(key.clone()).or_default();
            row.row.key = key;
            row.add(b, &mut coverage.incomplete);
        }
    }
    // Each session with a unique parent is one subagent; provenance duplicates do not multiply it.
    let children: BTreeSet<&str> = identities
        .iter()
        .filter(|m| m.parent_id.is_some())
        .map(|m| m.session_id.as_str())
        .collect();
    for child in children {
        let mappings: Vec<_> = identities
            .iter()
            .filter(|m| m.session_id == child)
            .collect();
        let parents: BTreeSet<_> = mappings
            .iter()
            .filter_map(|m| m.parent_id.as_deref())
            .collect();
        if parents.len() != 1 {
            coverage.incomplete = true;
            continue;
        }
        let launches: BTreeSet<_> = mappings.iter().filter_map(|m| m.launch_time_ms).collect();
        if launches.len() != 1 {
            coverage.unknown_launch_times += 1;
            if launches.len() > 1 {
                coverage.incomplete = true;
            }
            continue;
        }
        let time = *launches.first().unwrap();
        if time < start || time >= end {
            continue;
        }
        let key = match query.by {
            UsageGroupBy::Persona => match resolve_persona(child, identities) {
                Attribution::Persona(p) => p,
                _ => "unattributed".into(),
            },
            UsageGroupBy::Vendor => sources
                .iter()
                .find(|s| s.session_id == child)
                .map(|s| match s.vendor {
                    super::UsageVendor::Antigravity => "antigravity",
                    super::UsageVendor::Codex => "codex",
                    super::UsageVendor::Claude => "claude",
                    super::UsageVendor::Muse => "muse",
                    super::UsageVendor::Grok => "grok",
                })
                .unwrap_or("unknown")
                .into(),
            UsageGroupBy::Model => "unknown".into(),
        };
        let row = rows.entry(key.clone()).or_default();
        row.row.key = key;
        row.row.subagents = row.row.subagents.saturating_add(1);
    }
    Ok(UsageReport {
        requested_start_ms: query.since_ms,
        requested_end_ms: now_ms,
        effective_start_ms: start,
        effective_end_ms: end,
        by: query.by,
        unit: "tokens".into(),
        rows: rows.into_values().map(Accum::finish).collect(),
        undated: undated.finish(),
        coverage,
    })
}

pub fn format_time(ms: u64) -> String {
    fl_core::at::At::from_unix_millis(ms).as_str().to_owned()
}

pub fn aggregate_usage(
    store: &AnyStore,
    query: &UsageReportQuery,
    now_ms: u64,
) -> Result<UsageReport, UsageError> {
    let ferro = store.as_ferro().ok_or(UsageError::Unsupported)?;
    let read = |kind: &str| -> Result<Vec<serde_json::Value>, UsageError> {
        let ids = ferro
            .query_by_field(COLLECTION, "kind", kind)
            .map_err(|e| UsageError::Store(e.to_string()))?;
        let mut values = Vec::new();
        for id in ids {
            if let Some(value) = ferro
                .get_doc(COLLECTION, &id)
                .map_err(|e| UsageError::Store(e.to_string()))?
            {
                values.push(value["document"].clone());
            }
        }
        Ok(values)
    };
    let sources = read("source")?
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<SourceUsageDocument>, _>>()
        .map_err(|e| UsageError::Store(e.to_string()))?;
    let identities = read("identity")?
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<IdentityMappingDocument>, _>>()
        .map_err(|e| UsageError::Store(e.to_string()))?;
    let status = ferro
        .get_doc(COLLECTION, "collector-status")
        .map_err(|e| UsageError::Store(e.to_string()))?
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| UsageError::Store(e.to_string()))?;
    aggregate_documents(&sources, &identities, status, query, now_ms)
}
