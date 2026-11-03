//! Pure intent, finite allocation and advisory committee calculations.
//!
//! These models do not grant authority, persist a transaction or dispatch work. A caller must
//! authenticate results, enforce deadlines and atomically persist every allocation transition.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntentError {
    #[error("invalid intent ancestry or cycle")]
    Ancestry,
    #[error("allocation is invalid or cannot fund the requested carve")]
    Budget,
    #[error("invalid committee or quorum")]
    Committee,
    #[error("result is stale, duplicate, mismatched, or not a verdict")]
    Result,
    #[error("committee decision is already final")]
    Final,
}

/// Identifies a particular incarnation and its controller, never a reusable persona.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub incarnation_id: String,
    pub thread_epoch: u64,
    pub controller_lease_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegateTarget {
    FreshSession { template_id: String },
    BoundIncarnation { incarnation_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitteeVerdict {
    Approve,
    Reject,
    Abstain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultSchema {
    Verdict,
    Artifact { kind: String },
    Text { max_bytes: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypedResult {
    Verdict {
        verdict: CommitteeVerdict,
        rationale: String,
    },
    Artifact {
        kind: String,
        reference: String,
    },
    Text {
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuorumRule {
    All,
    Majority,
    AtLeast(u8),
}

impl QuorumRule {
    pub fn threshold(self, members: usize) -> Result<usize, IntentError> {
        if !(2..=5).contains(&members) {
            return Err(IntentError::Committee);
        }
        match self {
            Self::All => Ok(members),
            Self::Majority => Ok(members / 2 + 1),
            Self::AtLeast(k) if k > 0 && usize::from(k) <= members => Ok(usize::from(k)),
            _ => Err(IntentError::Committee),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateIntent {
    pub intent_id: String,
    pub parent_intent: Option<String>,
    pub delegator: Binding,
    pub delegate: DelegateTarget,
    pub task_template_digest: String,
    pub sub_allocation: AllocationCarve,
    pub deadline_utc: String,
    pub result_schema: ResultSchema,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitteeIntent {
    pub intent_id: String,
    pub delegator: Binding,
    pub members: Vec<DelegateTarget>,
    pub per_member: AllocationAmount,
    pub quorum: QuorumRule,
    pub deadline_utc: String,
    pub result_schema: ResultSchema,
}

impl CommitteeIntent {
    /// Validate the committee's size, threshold, and v1 verdict-only schema.
    pub fn validate(&self) -> Result<(), IntentError> {
        self.quorum.threshold(self.members.len())?;
        if self.result_schema != ResultSchema::Verdict {
            return Err(IntentError::Committee);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentResult {
    pub intent_id: String,
    pub member: Binding,
    pub attempt: u32,
    pub result: TypedResult,
    pub usage_ref: Option<String>,
}

/// Validate a proposed link. `ancestors` are prior delegator incarnation IDs, oldest first.
/// The new delegation is level one when this list is empty and level two when it has one entry.
pub fn validate_ancestry(
    delegator: &str,
    ancestors: &[String],
    target: Option<&str>,
) -> Result<(), IntentError> {
    if delegator.trim().is_empty() || ancestors.len() > 1 {
        return Err(IntentError::Ancestry);
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for id in ancestors
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(delegator))
        .chain(target)
    {
        if id.trim().is_empty() || !seen.insert(id) {
            return Err(IntentError::Ancestry);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationAmount {
    pub tokens: u64,
    pub cost_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarveRequest {
    pub allocation_id: String,
    pub settlement_id: String,
    pub tokens: u64,
    pub cost_micros: u64,
}

/// A child transfer tied to its immediate parent, original root, and source window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationCarve {
    pub allocation_id: String,
    pub parent_allocation_id: String,
    pub root_envelope_id: String,
    pub window_id: String,
    pub settlement_id: String,
    pub tokens: u64,
    pub cost_micros: u64,
}

/// A caller-owned snapshot; applying and persisting a carve is a store transaction in later slices.
/// Unreserved funds exclude all in-flight reservations and prior carves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationPool {
    allocation_id: String,
    root_envelope_id: String,
    window_id: String,
    total_tokens: u64,
    total_cost_micros: u64,
    unreserved_tokens: u64,
    unreserved_cost_micros: u64,
    issued_allocations: Vec<String>,
    issued_settlements: Vec<String>,
}

impl AllocationCarve {
    /// Construct a child pool after deducting any already reserved funds.
    pub fn child_pool(
        &self,
        unreserved_tokens: u64,
        unreserved_cost_micros: u64,
    ) -> Result<AllocationPool, IntentError> {
        AllocationPool::new(
            self.allocation_id.clone(),
            self.root_envelope_id.clone(),
            self.window_id.clone(),
            self.tokens,
            self.cost_micros,
            unreserved_tokens,
            unreserved_cost_micros,
        )
    }
}

impl AllocationPool {
    pub fn new(
        allocation_id: String,
        root_envelope_id: String,
        window_id: String,
        total_tokens: u64,
        total_cost_micros: u64,
        unreserved_tokens: u64,
        unreserved_cost_micros: u64,
    ) -> Result<Self, IntentError> {
        let pool = Self {
            allocation_id,
            root_envelope_id,
            window_id,
            total_tokens,
            total_cost_micros,
            unreserved_tokens,
            unreserved_cost_micros,
            issued_allocations: Vec::new(),
            issued_settlements: Vec::new(),
        };
        pool.validate()?;
        Ok(pool)
    }
    fn validate(&self) -> Result<(), IntentError> {
        if self.allocation_id.trim().is_empty()
            || self.root_envelope_id.trim().is_empty()
            || self.window_id.trim().is_empty()
            || self.total_tokens == 0
            || self.total_cost_micros == 0
            || self.unreserved_tokens > self.total_tokens
            || self.unreserved_cost_micros > self.total_cost_micros
        {
            return Err(IntentError::Budget);
        }
        Ok(())
    }
    pub fn unreserved_tokens(&self) -> u64 {
        self.unreserved_tokens
    }
    pub fn unreserved_cost_micros(&self) -> u64 {
        self.unreserved_cost_micros
    }
    pub fn carve(&mut self, request: CarveRequest) -> Result<AllocationCarve, IntentError> {
        Ok(self.carve_many(vec![request])?.remove(0))
    }
    pub fn carve_committee(
        &mut self,
        requests: Vec<CarveRequest>,
    ) -> Result<Vec<AllocationCarve>, IntentError> {
        if !(2..=5).contains(&requests.len()) {
            return Err(IntentError::Committee);
        }
        // Every committee member receives the same approved budget.
        if requests
            .iter()
            .any(|r| r.tokens != requests[0].tokens || r.cost_micros != requests[0].cost_micros)
        {
            return Err(IntentError::Committee);
        }
        self.carve_many(requests)
    }
    fn carve_many(
        &mut self,
        requests: Vec<CarveRequest>,
    ) -> Result<Vec<AllocationCarve>, IntentError> {
        self.validate()?;
        let mut ids: HashSet<&str> = self.issued_allocations.iter().map(String::as_str).collect();
        let mut settlements: HashSet<&str> =
            self.issued_settlements.iter().map(String::as_str).collect();
        let mut tokens = 0u64;
        let mut cost = 0u64;
        for r in &requests {
            if r.allocation_id.trim().is_empty()
                || r.settlement_id.trim().is_empty()
                || r.allocation_id == self.allocation_id
                || !ids.insert(&r.allocation_id)
                || !settlements.insert(&r.settlement_id)
                || r.tokens == 0
                || r.cost_micros == 0
                || r.tokens >= self.total_tokens
                || r.cost_micros >= self.total_cost_micros
            {
                return Err(IntentError::Budget);
            }
            tokens = tokens.checked_add(r.tokens).ok_or(IntentError::Budget)?;
            cost = cost.checked_add(r.cost_micros).ok_or(IntentError::Budget)?;
        }
        let remaining_tokens = self
            .unreserved_tokens
            .checked_sub(tokens)
            .ok_or(IntentError::Budget)?;
        let remaining_cost = self
            .unreserved_cost_micros
            .checked_sub(cost)
            .ok_or(IntentError::Budget)?;
        let carves: Vec<_> = requests
            .into_iter()
            .map(|r| AllocationCarve {
                allocation_id: r.allocation_id,
                parent_allocation_id: self.allocation_id.clone(),
                root_envelope_id: self.root_envelope_id.clone(),
                window_id: self.window_id.clone(),
                settlement_id: r.settlement_id,
                tokens: r.tokens,
                cost_micros: r.cost_micros,
            })
            .collect();
        self.unreserved_tokens = remaining_tokens;
        self.unreserved_cost_micros = remaining_cost;
        self.issued_allocations
            .extend(carves.iter().map(|c| c.allocation_id.clone()));
        self.issued_settlements
            .extend(carves.iter().map(|c| c.settlement_id.clone()));
        Ok(carves)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitteeOutcome {
    Approved,
    NotApproved,
    NoQuorum,
    Cancelled,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitteeCounts {
    pub approvals: usize,
    pub rejections: usize,
    pub abstentions: usize,
    pub outstanding: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CommitteeSlot {
    binding: Binding,
    attempt: u32,
    verdict: Option<CommitteeVerdict>,
}

/// Advisory tally only. `record` assumes the caller has already authenticated the result and
/// checked the deadline. Persist each result and tally atomically in commit order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitteeTally {
    intent_id: String,
    rule: QuorumRule,
    slots: Vec<CommitteeSlot>,
    outcome: Option<CommitteeOutcome>,
}

impl CommitteeTally {
    pub fn new(
        intent_id: String,
        rule: QuorumRule,
        members: Vec<(Binding, u32)>,
    ) -> Result<Self, IntentError> {
        rule.threshold(members.len())?;
        let mut ids = HashSet::new();
        if intent_id.trim().is_empty()
            || members.iter().any(|(b, _)| {
                b.incarnation_id.trim().is_empty()
                    || b.controller_lease_id.trim().is_empty()
                    || !ids.insert(&b.incarnation_id)
            })
        {
            return Err(IntentError::Committee);
        }
        Ok(Self {
            intent_id,
            rule,
            slots: members
                .into_iter()
                .map(|(binding, attempt)| CommitteeSlot {
                    binding,
                    attempt,
                    verdict: None,
                })
                .collect(),
            outcome: None,
        })
    }
    pub fn outcome(&self) -> Option<CommitteeOutcome> {
        self.outcome
    }
    pub fn counts(&self) -> CommitteeCounts {
        let mut result = CommitteeCounts::default();
        for slot in &self.slots {
            match slot.verdict {
                Some(CommitteeVerdict::Approve) => result.approvals += 1,
                Some(CommitteeVerdict::Reject) => result.rejections += 1,
                Some(CommitteeVerdict::Abstain) => result.abstentions += 1,
                None => result.outstanding += 1,
            }
        }
        result
    }
    pub fn record(
        &mut self,
        result: &IntentResult,
    ) -> Result<Option<CommitteeOutcome>, IntentError> {
        if self.outcome.is_some() {
            return Err(IntentError::Final);
        }
        let threshold = self.rule.threshold(self.slots.len())?;
        if result.intent_id != self.intent_id {
            return Err(IntentError::Result);
        }
        let TypedResult::Verdict { verdict, .. } = result.result else {
            return Err(IntentError::Result);
        };
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.binding == result.member && slot.attempt == result.attempt)
            .ok_or(IntentError::Result)?;
        if slot.verdict.is_some() {
            return Err(IntentError::Result);
        }
        slot.verdict = Some(verdict);
        let count = self.counts();
        if count.approvals >= threshold {
            self.outcome = Some(CommitteeOutcome::Approved);
        } else if count.approvals + count.outstanding < threshold {
            self.outcome = Some(CommitteeOutcome::NotApproved);
        }
        Ok(self.outcome)
    }
    /// The caller invokes this only once the applicable deadline has passed.
    pub fn expire(&mut self) {
        if self.outcome.is_none() {
            self.outcome = Some(CommitteeOutcome::NoQuorum);
        }
    }
    pub fn cancel(&mut self) {
        if self.outcome.is_none() {
            self.outcome = Some(CommitteeOutcome::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(id: &str) -> Binding {
        Binding {
            incarnation_id: id.into(),
            thread_epoch: 1,
            controller_lease_id: "lease".into(),
        }
    }
    fn parent() -> AllocationPool {
        AllocationPool::new(
            "parent".into(),
            "root".into(),
            "window".into(),
            100,
            50,
            80,
            40,
        )
        .unwrap()
    }
    fn child(id: &str) -> CarveRequest {
        CarveRequest {
            allocation_id: id.into(),
            settlement_id: format!("settle-{id}"),
            tokens: 20,
            cost_micros: 10,
        }
    }
    fn result(id: &str, attempt: u32, verdict: CommitteeVerdict, rationale: &str) -> IntentResult {
        IntentResult {
            intent_id: "i".into(),
            member: binding(id),
            attempt,
            result: TypedResult::Verdict {
                verdict,
                rationale: rationale.into(),
            },
            usage_ref: None,
        }
    }
    fn tally() -> CommitteeTally {
        CommitteeTally::new(
            "i".into(),
            QuorumRule::Majority,
            vec![(binding("a"), 1), (binding("b"), 3), (binding("c"), 1)],
        )
        .unwrap()
    }

    #[test]
    fn sub_allocation_carve_is_atomic_and_strictly_decreasing() {
        let mut p = parent();
        let c = p.carve(child("c")).unwrap();
        assert_eq!(
            (p.unreserved_tokens(), p.unreserved_cost_micros()),
            (60, 30)
        );
        assert_eq!(
            (
                c.parent_allocation_id.as_str(),
                c.root_envelope_id.as_str(),
                c.window_id.as_str()
            ),
            ("parent", "root", "window")
        );
        let before = p.clone();
        let mut r = child("bad");
        r.tokens = 61;
        assert!(p.carve(r).is_err());
        assert_eq!(p, before);
        let mut r = child("zero");
        r.cost_micros = 0;
        assert!(p.carve(r).is_err());
        assert_eq!(p, before);
    }
    #[test]
    fn committee_carves_all_or_nothing() {
        let mut p = parent();
        let before = p.clone();
        assert!(
            p.carve_committee(vec![
                child("1"),
                child("2"),
                child("3"),
                child("4"),
                child("5")
            ])
            .is_err()
        );
        assert_eq!(p, before);
        assert_eq!(
            p.carve_committee(vec![child("1"), child("2")])
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            (p.unreserved_tokens(), p.unreserved_cost_micros()),
            (40, 20)
        );
    }
    #[test]
    fn depth_and_cycle_refused() {
        assert!(validate_ancestry("b", &["a".into()], Some("c")).is_ok());
        assert!(validate_ancestry("c", &["a".into(), "b".into()], Some("d")).is_err());
        assert!(validate_ancestry("b", &["a".into()], Some("a")).is_err());
        assert!(validate_ancestry("a", &["a".into()], Some("b")).is_err());
    }
    #[test]
    fn quorum_from_typed_verdicts_only() {
        let mut a = tally();
        let mut b = tally();
        for (id, attempt) in [("a", 1), ("b", 3)] {
            a.record(&result(
                id,
                attempt,
                CommitteeVerdict::Approve,
                "reject everything",
            ))
            .unwrap();
            b.record(&result(id, attempt, CommitteeVerdict::Approve, "approve"))
                .unwrap();
        }
        assert_eq!(a.outcome(), Some(CommitteeOutcome::Approved));
        assert_eq!(a.outcome(), b.outcome());
    }
    #[test]
    fn one_verdict_per_member_slot_current_attempt_only() {
        let mut t = tally();
        assert!(
            t.record(&result("b", 2, CommitteeVerdict::Approve, ""))
                .is_err()
        );
        assert!(
            t.record(&result("x", 1, CommitteeVerdict::Approve, ""))
                .is_err()
        );
        t.record(&result("b", 3, CommitteeVerdict::Reject, ""))
            .unwrap();
        assert!(
            t.record(&result("b", 3, CommitteeVerdict::Approve, ""))
                .is_err()
        );
        assert_eq!(t.counts().rejections, 1);
    }
    #[test]
    fn committee_mixed_votes_abstentions_deterministic_outcome() {
        let mut t = tally();
        t.record(&result("a", 1, CommitteeVerdict::Approve, ""))
            .unwrap();
        t.record(&result("b", 3, CommitteeVerdict::Abstain, ""))
            .unwrap();
        assert_eq!(t.outcome(), None);
        t.record(&result("c", 1, CommitteeVerdict::Approve, ""))
            .unwrap();
        assert_eq!(t.outcome(), Some(CommitteeOutcome::Approved));
        assert_eq!(t.counts().abstentions, 1);
    }
    #[test]
    fn committee_early_impossibility_not_approved() {
        let mut t = tally();
        t.record(&result("a", 1, CommitteeVerdict::Reject, ""))
            .unwrap();
        t.record(&result("b", 3, CommitteeVerdict::Abstain, ""))
            .unwrap();
        assert_eq!(t.outcome(), Some(CommitteeOutcome::NotApproved));
        assert!(
            t.record(&result("c", 1, CommitteeVerdict::Approve, ""))
                .is_err()
        );
        t.expire();
        assert_eq!(t.outcome(), Some(CommitteeOutcome::NotApproved));
    }
    #[test]
    fn nested_carves_draw_only_from_immediate_parent_remainder() {
        let mut root = parent();
        let child = root.carve(child("child")).unwrap();
        let mut nested = child.child_pool(15, 8).unwrap();
        let first = CarveRequest {
            allocation_id: "grandchild".into(),
            settlement_id: "g-settlement".into(),
            tokens: 10,
            cost_micros: 4,
        };
        let carved = nested.carve(first.clone()).unwrap();
        assert_eq!(carved.parent_allocation_id, "child");
        assert_eq!(carved.root_envelope_id, "root");
        assert_eq!(carved.window_id, "window");
        let before = nested.clone();
        let mut second = first;
        second.allocation_id = "grandchild2".into();
        second.settlement_id = "other".into();
        assert!(nested.carve(second).is_err());
        assert_eq!(nested, before);
        assert_eq!(root.unreserved_tokens(), 60);
    }

    #[test]
    fn carve_rejects_overflow_duplicate_ids_and_reserved_funds_without_mutation() {
        let mut pool = AllocationPool::new(
            "p".into(),
            "root".into(),
            "w".into(),
            u64::MAX,
            u64::MAX,
            u64::MAX - 1,
            u64::MAX - 1,
        )
        .unwrap();
        let req = CarveRequest {
            allocation_id: "a".into(),
            settlement_id: "sa".into(),
            tokens: u64::MAX - 1,
            cost_micros: u64::MAX - 1,
        };
        let mut other = req.clone();
        other.allocation_id = "b".into();
        other.settlement_id = "sb".into();
        let before = pool.clone();
        assert!(pool.carve_committee(vec![req.clone(), other]).is_err());
        assert_eq!(pool, before);
        let mut p = parent();
        p.carve(child("same")).unwrap();
        let before = p.clone();
        assert!(p.carve(child("same")).is_err());
        assert_eq!(p, before);
        let mut bad = child("more");
        bad.cost_micros = 31;
        assert!(p.carve(bad).is_err());
        assert_eq!(p, before);
        let mut malformed = serde_json::to_value(&p).unwrap();
        malformed["unreserved_tokens"] = serde_json::json!(u64::MAX);
        let mut untrusted: AllocationPool = serde_json::from_value(malformed).unwrap();
        assert!(untrusted.carve(child("x")).is_err());
    }

    #[test]
    fn thresholds_validate_member_count_and_v1_result_schema() {
        assert_eq!(QuorumRule::All.threshold(5).unwrap(), 5);
        assert_eq!(QuorumRule::Majority.threshold(4).unwrap(), 3);
        assert_eq!(QuorumRule::AtLeast(2).threshold(4).unwrap(), 2);
        assert!(QuorumRule::AtLeast(0).threshold(3).is_err());
        assert!(QuorumRule::AtLeast(4).threshold(3).is_err());
        assert!(QuorumRule::All.threshold(1).is_err());
        assert!(QuorumRule::All.threshold(6).is_err());
        let mut committee = CommitteeIntent {
            intent_id: "i".into(),
            delegator: binding("d"),
            members: vec![
                DelegateTarget::BoundIncarnation {
                    incarnation_id: "a".into(),
                },
                DelegateTarget::BoundIncarnation {
                    incarnation_id: "b".into(),
                },
            ],
            per_member: AllocationAmount {
                tokens: 10,
                cost_micros: 1,
            },
            quorum: QuorumRule::All,
            deadline_utc: "2026-01-01T00:00:00Z".into(),
            result_schema: ResultSchema::Verdict,
        };
        assert!(committee.validate().is_ok());
        committee.result_schema = ResultSchema::Text { max_bytes: 10 };
        assert!(committee.validate().is_err());
    }

    #[test]
    fn quorum_rejects_stale_binding_wrong_intent_and_non_verdict() {
        let mut t = tally();
        let original = t.clone();
        let mut r = result("a", 1, CommitteeVerdict::Approve, "");
        r.member.thread_epoch += 1;
        assert!(t.record(&r).is_err());
        r.member = binding("a");
        r.intent_id = "other".into();
        assert!(t.record(&r).is_err());
        r.intent_id = "i".into();
        r.result = TypedResult::Text {
            text: "approve".into(),
        };
        assert!(t.record(&r).is_err());
        assert_eq!(t, original);
        assert!(
            CommitteeTally::new(
                "i".into(),
                QuorumRule::All,
                vec![(binding("a"), 1), (binding("a"), 2)]
            )
            .is_err()
        );
    }

    #[test]
    fn deadline_and_cancellation_never_overwrite_first_terminal_result() {
        let mut t = tally();
        t.expire();
        t.cancel();
        assert_eq!(t.outcome(), Some(CommitteeOutcome::NoQuorum));
        assert!(
            t.record(&result("a", 1, CommitteeVerdict::Approve, ""))
                .is_err()
        );
        let mut t = tally();
        t.cancel();
        t.expire();
        assert_eq!(t.outcome(), Some(CommitteeOutcome::Cancelled));
    }

    #[test]
    fn typed_models_and_tally_round_trip_without_changing_decision() {
        let mut t = tally();
        t.record(&result("a", 1, CommitteeVerdict::Approve, "untrusted"))
            .unwrap();
        let json = serde_json::to_string(&t).unwrap();
        let mut restored: CommitteeTally = serde_json::from_str(&json).unwrap();
        restored
            .record(&result("b", 3, CommitteeVerdict::Approve, ""))
            .unwrap();
        assert_eq!(restored.outcome(), Some(CommitteeOutcome::Approved));
    }
}
