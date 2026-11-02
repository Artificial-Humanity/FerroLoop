//! Where subscription usage balancing keeps its state: workspace runtime entries, written
//! last-writer-wins like every other runtime entry.
//!
//! - `watch-balance-policy`: the [`BalancePolicy`]. Absent means the defaults.
//! - `watch-balance-history`: the [`BalanceHistory`] the planner reads (the last apply that ran a
//!   switch, and when each persona was last moved). Absent means none.
//! - `watch-balance-audit-<ms>-<n>`: one [`BalanceAudit`] per apply whose plan had a switch to
//!   run: the plan, who applied it and on what authority, and each switch's result. `<n>` tells
//!   apart two audits written in the same millisecond.
//!
//! **Audit growth.** Audits are never pruned: [`StateStore`] has no delete (a state entry can
//! only be revised, and every revision is kept for sync), so each apply that had a switch adds
//! one entry for good. Applies are rare (one per `min_interval_ms` at most, and only when an
//! agent runs hot), so the growth is small; [`list_audits`] reads them all.
//!
//! These are the only entries fl-watch writes. The caller serializes writers (the hub holds its
//! store guard across a whole apply).

use fl_core::at::At;
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs,
};
use fl_core::store::StateStore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::balance::{BalanceError, BalanceHistory, BalancePlan, BalancePolicy};

/// Slug of the stored balance policy.
pub const POLICY_SLUG: &str = "watch-balance-policy";
/// Slug of the stored balance history.
pub const HISTORY_SLUG: &str = "watch-balance-history";
/// Slug prefix of every balance audit entry.
pub const AUDIT_SLUG_PREFIX: &str = "watch-balance-audit-";

/// How one planned switch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchStatus {
    /// The agent runs on the new vendor.
    Ok,
    /// The remodel failed after it touched the session; `outcome` says whether the old session
    /// was restored or the persona is down.
    Failed,
    /// The remodel was refused before it touched anything.
    Refused,
    /// Not run: an earlier switch left its persona down, so the apply stopped.
    NotRun,
}

/// The result of one planned switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchResult {
    pub persona: String,
    pub session_id: String,
    pub from_vendor: String,
    pub to_vendor: String,
    pub status: SwitchStatus,
    /// The session the persona runs as after an `ok` switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_session_id: Option<String>,
    /// `remodeled`, `restored` or `down`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One balance apply, as its audit entry records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BalanceAudit {
    pub applied_at_ms: u64,
    /// The name the apply ran as (the Owner's login, or the peer's agent name).
    pub by: String,
    /// `owner`, or `warrant` for a peer holding an Owner warrant.
    pub authority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warrant_grant_id: Option<String>,
    pub plan: BalancePlan,
    pub results: Vec<SwitchResult>,
}

fn read_entry<S: StateStore + ?Sized, T: DeserializeOwned>(
    store: &S,
    slug: &str,
) -> Result<Option<T>, BalanceError> {
    let Some(entry) =
        store.get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, slug)?
    else {
        return Ok(None);
    };
    let StatePayload::Runtime(rt) = entry.payload else {
        return Err(BalanceError::Undecodable(format!(
            "`{slug}` is not a runtime entry"
        )));
    };
    serde_json::from_value(rt.value)
        .map(Some)
        .map_err(|e| BalanceError::Undecodable(format!("`{slug}` does not decode: {e}")))
}

fn write_entry<S: StateStore + ?Sized>(
    store: &S,
    slug: &str,
    value: Value,
    at: At,
    by: &str,
) -> Result<(), BalanceError> {
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: slug.to_string(),
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Runtime(RuntimeEntry {
            value,
            merge_mode: MergeMode::LastWriter,
        }),
        at,
        by: by.to_string(),
    })?;
    Ok(())
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, BalanceError> {
    serde_json::to_value(v).map_err(|e| BalanceError::Undecodable(e.to_string()))
}

/// The stored policy, or the defaults when none is stored. A stored policy that does not decode
/// or does not validate is an error, not the defaults: it would silently drop pins.
pub fn read_policy<S: StateStore + ?Sized>(store: &S) -> Result<BalancePolicy, BalanceError> {
    let policy: BalancePolicy = read_entry(store, POLICY_SLUG)?.unwrap_or_default();
    policy.validate()?;
    Ok(policy)
}

/// Store `policy`, validated first.
pub fn write_policy<S: StateStore + ?Sized>(
    store: &S,
    policy: &BalancePolicy,
    at: At,
    by: &str,
) -> Result<(), BalanceError> {
    policy.validate()?;
    write_entry(store, POLICY_SLUG, to_value(policy)?, at, by)
}

/// The stored history, or an empty one.
pub fn read_history<S: StateStore + ?Sized>(store: &S) -> Result<BalanceHistory, BalanceError> {
    Ok(read_entry(store, HISTORY_SLUG)?.unwrap_or_default())
}

pub fn write_history<S: StateStore + ?Sized>(
    store: &S,
    history: &BalanceHistory,
    at: At,
    by: &str,
) -> Result<(), BalanceError> {
    write_entry(store, HISTORY_SLUG, to_value(history)?, at, by)
}

/// Write `audit` under the first free `watch-balance-audit-<applied_at_ms>-<n>`, and return
/// that slug.
pub fn write_audit<S: StateStore + ?Sized>(
    store: &S,
    audit: &BalanceAudit,
    at: At,
) -> Result<String, BalanceError> {
    let value = to_value(audit)?;
    let mut n = 0u32;
    let slug = loop {
        let slug = format!("{AUDIT_SLUG_PREFIX}{}-{n}", audit.applied_at_ms);
        if store
            .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)?
            .is_none()
        {
            break slug;
        }
        n += 1;
    };
    write_entry(store, &slug, value, at, &audit.by)?;
    Ok(slug)
}

/// Every audit that decodes, oldest first, with its slug.
pub fn list_audits<S: StateStore + ?Sized>(
    store: &S,
) -> Result<Vec<(String, BalanceAudit)>, BalanceError> {
    let mut out: Vec<(String, BalanceAudit)> = store
        .list_state(Some(&StateScope::Workspace), Some(StateConcern::Runtime))?
        .into_iter()
        .filter(|e| e.slug.starts_with(AUDIT_SLUG_PREFIX))
        .filter_map(|e| match e.payload {
            StatePayload::Runtime(rt) => serde_json::from_value(rt.value).ok().map(|a| (e.slug, a)),
            _ => None,
        })
        .collect();
    let seq = |slug: &str| {
        let rest = slug.strip_prefix(AUDIT_SLUG_PREFIX).unwrap_or_default();
        let (ms, n) = rest.split_once('-').unwrap_or((rest, "0"));
        (
            ms.parse::<u64>().unwrap_or(0),
            n.parse::<u32>().unwrap_or(0),
        )
    };
    out.sort_by_key(|(slug, _)| seq(slug));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balance::{BalanceInputs, plan_balance};
    use crate::testkit::put_runtime;
    use fl_core::MemStore;

    fn at() -> At {
        At::from_unix_millis(5)
    }

    #[test]
    fn the_policy_defaults_when_absent_and_round_trips() {
        let store = MemStore::default();
        assert_eq!(read_policy(&store).unwrap(), BalancePolicy::default());
        let policy = BalancePolicy {
            enabled: true,
            pinned_personas: vec!["ferris".into()],
            cooldown_ms: 7,
            ..BalancePolicy::default()
        };
        write_policy(&store, &policy, at(), "owner").unwrap();
        assert_eq!(read_policy(&store).unwrap(), policy);
        // An invalid policy is not stored.
        let bad = BalancePolicy {
            max_switches_per_run: 0,
            ..BalancePolicy::default()
        };
        assert!(matches!(
            write_policy(&store, &bad, at(), "owner"),
            Err(BalanceError::InvalidPolicy(_))
        ));
        assert_eq!(read_policy(&store).unwrap(), policy);
    }

    #[test]
    fn a_stored_policy_that_does_not_decode_or_validate_is_an_error() {
        let store = MemStore::default();
        put_runtime(&store, POLICY_SLUG, serde_json::json!({"cooldown": 1}));
        let err = read_policy(&store).unwrap_err().to_string();
        assert!(err.contains(POLICY_SLUG), "{err}");
        put_runtime(
            &store,
            POLICY_SLUG,
            serde_json::json!({"max_switches_per_run": 0}),
        );
        assert!(matches!(
            read_policy(&store),
            Err(BalanceError::InvalidPolicy(_))
        ));
    }

    #[test]
    fn history_round_trips_and_defaults_empty() {
        let store = MemStore::default();
        assert_eq!(read_history(&store).unwrap(), BalanceHistory::default());
        let mut h = BalanceHistory::default();
        h.record(42, ["Ada"]);
        write_history(&store, &h, at(), "owner").unwrap();
        assert_eq!(read_history(&store).unwrap(), h);
    }

    #[test]
    fn audits_take_the_first_free_slug_and_list_in_order() {
        let store = MemStore::default();
        let policy = BalancePolicy::default();
        let plan = plan_balance(&BalanceInputs {
            policy: &policy,
            now_ms: 1_000,
            agents: &[],
            readings: &[],
            candidate_vendors: &[],
            history: &BalanceHistory::default(),
        })
        .unwrap();
        let audit = |ms: u64| BalanceAudit {
            applied_at_ms: ms,
            by: "lmcfarlin".into(),
            authority: "owner".into(),
            warrant_grant_id: None,
            plan: plan.clone(),
            results: vec![SwitchResult {
                persona: "ada".into(),
                session_id: "claude:ada".into(),
                from_vendor: "claude".into(),
                to_vendor: "codex".into(),
                status: SwitchStatus::Ok,
                new_session_id: Some("codex:ada".into()),
                outcome: Some("remodeled".into()),
                restored_session_id: None,
                error: None,
            }],
        };
        let a = write_audit(&store, &audit(10_000), at()).unwrap();
        let b = write_audit(&store, &audit(10_000), at()).unwrap();
        let c = write_audit(&store, &audit(9_000), at()).unwrap();
        assert_eq!(a, "watch-balance-audit-10000-0");
        assert_eq!(b, "watch-balance-audit-10000-1");
        assert_eq!(c, "watch-balance-audit-9000-0");
        // Junk under the prefix is skipped, not fatal.
        put_runtime(&store, "watch-balance-audit-1-0", serde_json::json!("junk"));
        let listed = list_audits(&store).unwrap();
        let slugs: Vec<&str> = listed.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(slugs, vec![c.as_str(), a.as_str(), b.as_str()]);
        assert_eq!(listed[1].1, audit(10_000));
        // None of these entries reads as a stored quota window or a hosted session.
        let snap = crate::read_runtime(&store).unwrap();
        assert!(snap.quotas.is_empty() && snap.quotas_undecodable == 0);
        assert!(snap.hosted_sessions.is_empty());
    }
}
