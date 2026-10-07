//! Escalation (routing spec §3): the local store's side of moving a record
//! or a finding to GitHub — the mark that blocks local writes while the
//! issue is found or made, the tombstone that replaces the item once it
//! exists — and the refusals the escalation names.

use crate::finding::Finding;
use crate::ids::Kind;
use crate::iri::Iri;
use crate::model::Record;
use crate::store::StoreError;
use crate::tiered::RecordSeen;
use serde::{Deserialize, Serialize};

/// Step 1 of an escalation (routing spec §3.3): who escalated the item, why,
/// and when, in unix milliseconds. While it stands the local store refuses
/// every write to the item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mark {
    pub by: String,
    pub reason: String,
    pub at_ms: u64,
}

/// Step 3 (routing spec §3.3): what replaces the local item once its issue
/// exists — the old IRI, the new one, and the mark's who, why and time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub from: Iri,
    pub to: Iri,
    pub by: String,
    pub reason: String,
    pub at_ms: u64,
}

/// The local store's side of an escalation (routing spec §3.3, §3.6). Every
/// id is resolved to its primary first; an id the store does not hold is
/// `NotOwned`, except in `mark_of` and `tombstone_of`, which answer `None`.
pub trait Escalations {
    /// Step 1. Refuses an item already marked (`AlreadyMarked`) or tombstoned
    /// (`Escalated`), and any kind but a record or a finding (`WrongKind`).
    fn mark(&self, id: &Iri, mark: &Mark) -> Result<(), StoreError>;
    fn mark_of(&self, id: &Iri) -> Result<Option<Mark>, StoreError>;
    /// `--abandon`. Refuses an item with no mark (`NotMarked`).
    fn unmark(&self, id: &Iri) -> Result<(), StoreError>;
    /// Step 3, in one write: the tombstone from the mark's who, why and time,
    /// and the mark removed. Refuses an item with no mark (`NotMarked`).
    fn tombstone(&self, id: &Iri, to: &Iri) -> Result<Tombstone, StoreError>;
    fn tombstone_of(&self, id: &Iri) -> Result<Option<Tombstone>, StoreError>;
}

/// Who escalated an item, why, and the IRI it had in the local tier — what
/// the issue says of where it came from (routing spec §3.3 step 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub from: Iri,
    pub by: String,
    pub reason: String,
}

/// What an escalation writes to GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// A record and its open findings in both tiers that are neither
    /// security findings nor in a sensitive or undeclared area (decisions
    /// 18, 21, 22).
    Record {
        record: Record,
        findings: Vec<Finding>,
    },
    /// A finding, and its record as the router read it (where it lives now).
    Finding {
        finding: Finding,
        record: RecordSeen,
    },
}

impl Outgoing {
    /// The item's local IRI.
    pub fn id(&self) -> &Iri {
        match self {
            Outgoing::Record { record, .. } => record.id.iri(),
            Outgoing::Finding { finding, .. } => finding.id.iri(),
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Outgoing::Record { .. } => Kind::Record,
            Outgoing::Finding { .. } => Kind::Finding,
        }
    }

    /// A record's title, or a finding's claim: the text the issue's title
    /// is made from.
    pub fn title(&self) -> &str {
        match self {
            Outgoing::Record { record, .. } => &record.title,
            Outgoing::Finding { finding, .. } => &finding.claim,
        }
    }
}

/// The command that finishes the escalation of `id`, a `kind` (routing spec
/// §3.3 step 1): what every refusal of a marked item names.
pub fn escalate_command(kind: Kind, id: &Iri) -> String {
    format!("fl {} escalate {id}", kind.as_wire())
}

/// Why an escalation was refused (routing spec §3.1, §3.2, §4): each names
/// its cause and what to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EscalationFault {
    #[error(
        "{id} is not a local item, so there is nothing to escalate: only an item in the local \
         tier moves to GitHub"
    )]
    NotLocal { id: Iri },
    #[error("{id} was escalated already, to {to}. Read and change it there")]
    AlreadyEscalated { id: Iri, to: Iri },
    #[error(
        "{id} is in a closed state, `{state}`, and GitHub takes open items only, so it cannot be \
         escalated. Nothing was written; it stays in the local tier"
    )]
    Closed { id: Iri, state: String },
    #[error(
        "the title of {id} cannot be an issue's title: {why}. Nothing was written; it stays in \
         the local tier"
    )]
    Title { id: Iri, why: String },
    #[error(
        "the alias {alias} already names {issue} on GitHub, and one id names one item across \
         both tiers, so the escalation would give it two meanings. Nothing was written"
    )]
    AliasTaken { alias: Iri, issue: Iri },
    #[error(
        "{id} is marked escalating already. Run `{}` to finish the escalation, or add \
         `--abandon` to stop it",
        escalate_command(*kind, id)
    )]
    AlreadyMarked { id: Iri, kind: Kind },
    #[error("{id} is not marked escalating, so there is no escalation to finish or abandon")]
    NotMarked { id: Iri },
    #[error(
        "the escalation of {id} cannot be abandoned: its issue exists, {issue}. Finish it with \
         `{}` instead, which replaces the local item with a tombstone",
        escalate_command(*kind, id)
    )]
    IssueExists { id: Iri, kind: Kind, issue: Iri },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{FindingId, ProjectId, RecordId, seq_iri};
    use crate::model::State;
    use crate::routing::Tier;

    fn issue(n: u64) -> Iri {
        Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
    }

    // Routing spec §4: every escalation refusal names its cause and what to
    // do — each in a phrase no other refusal says, so a test that asserts
    // one phrase can tell which refusal it got.
    #[test]
    fn every_escalation_refusal_names_its_remedy() {
        let id = seq_iri(1);
        let said: Vec<(StoreError, &str)> = vec![
            (
                EscalationFault::NotLocal { id: issue(3) }.into(),
                "is not a local item",
            ),
            (
                EscalationFault::AlreadyEscalated {
                    id: id.clone(),
                    to: issue(7),
                }
                .into(),
                "was escalated already",
            ),
            (
                EscalationFault::Closed {
                    id: id.clone(),
                    state: "done".into(),
                }
                .into(),
                "is in a closed state",
            ),
            (
                EscalationFault::Title {
                    id: id.clone(),
                    why: "it is over 256 characters".into(),
                }
                .into(),
                "cannot be an issue's title",
            ),
            (
                EscalationFault::AliasTaken {
                    alias: issue(4),
                    issue: issue(4),
                }
                .into(),
                "already names",
            ),
            (
                EscalationFault::AlreadyMarked {
                    id: id.clone(),
                    kind: Kind::Finding,
                }
                .into(),
                "is marked escalating already",
            ),
            (
                EscalationFault::NotMarked { id: id.clone() }.into(),
                "is not marked escalating",
            ),
            (
                EscalationFault::IssueExists {
                    id: id.clone(),
                    kind: Kind::Record,
                    issue: issue(7),
                }
                .into(),
                "its issue exists",
            ),
            (
                StoreError::Escalating {
                    id: id.clone(),
                    to_finish: escalate_command(Kind::Record, &id),
                },
                "so this store refuses to change it",
            ),
            (
                StoreError::Escalated {
                    from: id.clone(),
                    to: issue(7),
                },
                "was escalated to GitHub and is now",
            ),
        ];
        let messages: Vec<String> = said.iter().map(|(e, _)| e.to_string()).collect();
        for (i, (_, phrase)) in said.iter().enumerate() {
            for (j, msg) in messages.iter().enumerate() {
                assert_eq!(msg.contains(phrase), i == j, "{phrase:?} in {msg}");
            }
        }
        let finish = format!("`fl finding escalate {id}`");
        assert!(messages[5].contains(&finish), "{}", messages[5]);
        assert!(messages[5].contains("`--abandon`"), "{}", messages[5]);
        let finish = format!("`fl record escalate {id}`");
        assert!(messages[7].contains(&finish), "{}", messages[7]);
        assert!(messages[7].contains(issue(7).as_str()), "{}", messages[7]);
        assert!(messages[1].contains(issue(7).as_str()), "{}", messages[1]);
        assert!(messages[8].contains(&finish), "{}", messages[8]);
        let abandon = format!("`fl record escalate {id} --abandon`");
        assert!(messages[8].contains(&abandon), "{}", messages[8]);
        assert!(messages[9].contains(issue(7).as_str()), "{}", messages[9]);
    }

    #[test]
    fn the_command_that_finishes_an_escalation_names_the_kind_and_the_id() {
        assert_eq!(
            escalate_command(Kind::Finding, &seq_iri(4)),
            format!("fl finding escalate {}", seq_iri(4))
        );
        assert_eq!(
            escalate_command(Kind::Record, &seq_iri(4)),
            format!("fl record escalate {}", seq_iri(4))
        );
    }

    #[test]
    fn an_outgoing_item_answers_its_id_kind_and_title() {
        let p = ProjectId(seq_iri(1));
        let record = Record {
            id: RecordId(seq_iri(2)),
            project: p.clone(),
            title: "the title".into(),
            state: State::NeedsHuman,
            also_known_as: vec![],
            area: Some("design".into()),
        };
        let mut finding = Finding::raise(p, record.id.clone(), "rev", "the claim");
        finding.id = FindingId(seq_iri(3));
        let out = Outgoing::Record {
            record: record.clone(),
            findings: vec![finding.clone()],
        };
        assert_eq!(
            (out.id(), out.kind(), out.title()),
            (record.id.iri(), Kind::Record, "the title")
        );
        let out = Outgoing::Finding {
            finding: finding.clone(),
            record: RecordSeen {
                id: record.id.clone(),
                title: "the title".into(),
                tier: Tier::Local,
            },
        };
        assert_eq!(
            (out.id(), out.kind(), out.title()),
            (finding.id.iri(), Kind::Finding, "the claim")
        );
    }

    // The store keeps both as JSON (routing spec §3.6): snake_case, and a
    // field this version does not know is refused rather than dropped.
    #[test]
    fn a_mark_and_a_tombstone_are_kept_as_json_and_refuse_unknown_fields() {
        let mark = Mark {
            by: "alice".into(),
            reason: "needs a design review".into(),
            at_ms: 5,
        };
        let json = serde_json::to_string(&mark).unwrap();
        assert_eq!(
            json,
            r#"{"by":"alice","reason":"needs a design review","at_ms":5}"#
        );
        assert_eq!(serde_json::from_str::<Mark>(&json).unwrap(), mark);
        let extra = r#"{"by":"a","reason":"r","at_ms":5,"to":"x"}"#;
        assert!(serde_json::from_str::<Mark>(extra).is_err());
        let tomb = Tombstone {
            from: seq_iri(2),
            to: issue(7),
            by: "alice".into(),
            reason: "r".into(),
            at_ms: 5,
        };
        let json = serde_json::to_string(&tomb).unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"from":"{}","to":"{}","by":"alice","reason":"r","at_ms":5}}"#,
                seq_iri(2),
                issue(7)
            )
        );
        assert_eq!(serde_json::from_str::<Tombstone>(&json).unwrap(), tomb);
        let extra = json.replace("\"at_ms\":5", "\"at_ms\":5,\"why\":\"x\"");
        assert!(serde_json::from_str::<Tombstone>(&extra).is_err());
    }
}
