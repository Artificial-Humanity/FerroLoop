//! How an fl item lives in a GitHub issue (GitHub tracker spec §3): one kind
//! label, one state label, and a metadata block at the end of the body.
//! Open or closed is a projection fl writes and never reads state from.
//!
//! Pure: no IO. `GithubTracker` does the talking.

use fl_core::finding::FindingState;
use fl_core::ids::{GateId, Kind, ProjectId};
use fl_core::iri::Iri;
use fl_core::model::State;
use fl_core::store::StoreError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FL_FORMAT: u64 = 1;
/// The format of a block that carries an area or a reference to a local
/// record (routing spec decision 14). An older fl reads format 1 only, so
/// it refuses such an issue as a newer format instead of reading half of it.
pub const FL_FORMAT_ROUTED: u64 = 2;
/// The format of a block that carries its escalation (routing spec §3.3):
/// `Meta` refuses a field it does not know, so an fl that reads formats 1
/// and 2 would read the field as damage; raised, it says "upgrade fl".
pub const FL_FORMAT_ESCALATED: u64 = 3;
/// The start of an area's label, `fl:area/<name>` (routing spec §1.1).
pub const AREA_LABEL_PREFIX: &str = "fl:area/";
pub const META_OPEN: &str = "<!-- fl:meta";
pub const META_CLOSE: &str = "-->";
/// GitHub limits an issue title to 256 characters. The router's check
/// before an escalation's mark reads the same constant.
pub const TITLE_MAX: usize = fl_core::escalate::ISSUE_TITLE_MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Record,
    Finding,
}

impl ItemKind {
    pub const ALL: [ItemKind; 2] = [ItemKind::Record, ItemKind::Finding];

    pub fn as_kind(self) -> Kind {
        match self {
            ItemKind::Record => Kind::Record,
            ItemKind::Finding => Kind::Finding,
        }
    }

    pub fn from_kind(kind: Kind) -> Option<Self> {
        match kind {
            Kind::Record => Some(ItemKind::Record),
            Kind::Finding => Some(ItemKind::Finding),
            Kind::Project | Kind::Gate => None,
        }
    }

    pub fn as_wire(self) -> &'static str {
        self.as_kind().as_wire()
    }

    /// Every state of this kind, from the core enums — never a hand list.
    pub fn states(self) -> Vec<&'static str> {
        match self {
            ItemKind::Record => State::ALL.iter().map(|s| s.as_wire()).collect(),
            ItemKind::Finding => FindingState::ALL.iter().map(|s| s.as_wire()).collect(),
        }
    }

    pub fn valid_state(self, state: &str) -> bool {
        self.states().contains(&state)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRef {
    pub id: Iri,
    /// The record's issue node id (spec §2.3). `None` for a record in the
    /// project's local tier (routing spec §2.5), which no issue holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// A local record's title, which the issue shows beside its IRI: a
    /// reader on GitHub cannot open a local item. `None` for a record on
    /// GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl RecordRef {
    /// Whether this names a record in the local tier.
    pub fn is_local(&self) -> bool {
        self.node_id.is_none()
    }
}

/// Where an escalated issue came from (routing spec §3.3 step 2): the
/// item's IRI in the local tier, who escalated it, and why. The issue shows
/// it as a line written from the block, never as prose, so a finding's claim
/// stays its claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EscalatedFrom {
    pub from: Iri,
    pub by: String,
    pub reason: String,
}

/// The fields a label cannot hold (spec §3.1). ⚠ `deny_unknown_fields`: a
/// block with a field this fl does not know is damaged, not half-read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub fl_format: u64,
    pub kind: ItemKind,
    pub state: String,
    pub project: ProjectId,
    /// The item's area (routing spec §1.1), fixed for its life; its
    /// `fl:area/<name>` label is rewritten from this. Skipped when absent,
    /// so a block without one is written byte for byte as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<RecordRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reproduction: Option<GateId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raised_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_to: Option<String>,
    #[serde(default)]
    pub security: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawn_reason: Option<String>,
    #[serde(default)]
    pub also_known_as: Vec<Iri>,
    /// Set once, when the item is escalated from the local tier; skipped
    /// when absent, so a block without one is written byte for byte as
    /// before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalated: Option<EscalatedFrom>,
    /// Minted by fl for each create, so a retry can find what an ambiguous
    /// failure may already have made (spec §3.3).
    pub create_key: String,
}

impl Meta {
    pub fn new(kind: ItemKind, state: &str, project: ProjectId) -> Self {
        Self {
            fl_format: FL_FORMAT,
            kind,
            state: state.to_string(),
            project,
            area: None,
            record: None,
            reproduction: None,
            raised_by: None,
            assigned_to: None,
            security: false,
            withdrawn_reason: None,
            also_known_as: vec![],
            escalated: None,
            create_key: format!("urn:uuid:{}", uuid::Uuid::now_v7()),
        }
    }

    /// The format this block is written in: [`FL_FORMAT_ESCALATED`] when it
    /// carries its escalation, whatever else it carries; else
    /// [`FL_FORMAT_ROUTED`] when it carries an area or a reference to a
    /// local record; else [`FL_FORMAT`], which every older fl reads.
    pub fn required_format(&self) -> u64 {
        let local_record = self.record.as_ref().is_some_and(RecordRef::is_local);
        if self.escalated.is_some() {
            FL_FORMAT_ESCALATED
        } else if self.area.is_some() || local_record {
            FL_FORMAT_ROUTED
        } else {
            FL_FORMAT
        }
    }

    /// This block with `fl_format` set from its fields. ⚠ Never set the
    /// field by hand: a block whose format disagrees with its fields reads
    /// as damaged, and a remembered copy that disagrees with the issue reads
    /// as a conflict.
    pub fn sealed(mut self) -> Self {
        self.fl_format = self.required_format();
        self
    }
}

/// The parts of a GitHub issue fl reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueView {
    pub number: u64,
    pub url: Iri,
    pub node_id: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub state: String,
    pub state_reason: Option<String>,
    pub is_pull_request: bool,
}

impl IssueView {
    pub fn from_json(v: &Value) -> Result<Self, StoreError> {
        let text = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| StoreError::Backend(format!("GitHub sent an issue without `{k}`")))
        };
        let url = Iri::parse(&text("html_url")?).map_err(|e| {
            StoreError::Backend(format!("GitHub sent an issue URL fl cannot use: {e}"))
        })?;
        Ok(Self {
            number: v.get("number").and_then(Value::as_u64).ok_or_else(|| {
                StoreError::Backend("GitHub sent an issue without `number`".into())
            })?,
            url,
            node_id: text("node_id")?,
            title: text("title")?,
            // GitHub sends `null` for an empty body.
            body: v
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            labels: v
                .get("labels")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            state: text("state")?,
            state_reason: v
                .get("state_reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            is_pull_request: v.get("pull_request").is_some_and(|p| !p.is_null()),
        })
    }

    /// The same view from a node of GitHub's GraphQL `issues` connection,
    /// which every list reads (spec §3.7). Its node holds `number id url
    /// title body state stateReason labels(first: 100) { totalCount nodes {
    /// name } }`. GraphQL spells the state and its reason in capitals; they
    /// are lowered to REST's spelling. ⚠ Never a pull request: the
    /// connection lists issues only.
    pub fn from_graphql(v: &Value) -> Result<Self, StoreError> {
        let text = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| StoreError::Backend(format!("GitHub listed an issue without `{k}`")))
        };
        let number = v
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| StoreError::Backend("GitHub listed an issue without `number`".into()))?;
        let url = Iri::parse(&text("url")?).map_err(|e| {
            StoreError::Backend(format!("GitHub listed an issue URL fl cannot use: {e}"))
        })?;
        let state = match text("state")?.as_str() {
            "OPEN" => "open".to_string(),
            "CLOSED" => "closed".to_string(),
            other => {
                return Err(StoreError::Backend(format!(
                    "GitHub listed issue {number} in the state `{other}`, which fl does not know"
                )));
            }
        };
        // ⚠ A label left out would make the issue read as another item, or
        // as diverged: the labels are read in full or not at all.
        let (Some(total), Some(nodes)) = (
            v.pointer("/labels/totalCount").and_then(Value::as_u64),
            v.pointer("/labels/nodes").and_then(Value::as_array),
        ) else {
            return Err(StoreError::Backend(format!(
                "GitHub listed issue {number} without its `labels`; retry"
            )));
        };
        let labels = nodes
            .iter()
            .map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .filter(|l| l.len() as u64 == total)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "GitHub listed {} of its {total} labels of issue {number}, so fl cannot \
                     tell what it is; retry",
                    nodes.len()
                ))
            })?;
        Ok(Self {
            number,
            url,
            node_id: text("id")?,
            title: text("title")?,
            // ⚠ Required, unlike REST's `null` for an empty body: GraphQL's
            // `body` is never null, and a body read as empty would make an
            // issue's create key "not found" — and a create-key search send
            // the create again.
            body: text("body")?,
            labels,
            state,
            state_reason: v
                .get("stateReason")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase),
            is_pull_request: false,
        })
    }
}

pub fn kind_label(kind: ItemKind) -> String {
    format!("fl:{}", kind.as_wire())
}

pub fn state_label(kind: ItemKind, state: &str) -> String {
    format!("fl:{}/{state}", kind.as_wire())
}

/// An area's label (routing spec §1.1).
pub fn area_label(area: &str) -> String {
    format!("{AREA_LABEL_PREFIX}{area}")
}

/// Every label fl may set, enumerated from the kinds and their states.
pub fn all_labels() -> Vec<String> {
    let mut out = Vec::new();
    for kind in ItemKind::ALL {
        out.push(kind_label(kind));
        for s in kind.states() {
            out.push(state_label(kind, s));
        }
    }
    out
}

/// An issue's labels after fl writes it: every label that is not fl's, then
/// this kind's two, then the area's — all from the block (spec §3.3; routing
/// spec §1.1: fl replaces only its own labels, and the area label is one).
pub fn labels_after(
    current: &[String],
    kind: ItemKind,
    state: &str,
    area: Option<&str>,
) -> Vec<String> {
    let mut out: Vec<String> = current
        .iter()
        .filter(|l| !l.starts_with("fl:"))
        .cloned()
        .collect();
    out.push(kind_label(kind));
    out.push(state_label(kind, state));
    if let Some(a) = area {
        out.push(area_label(a));
    }
    out
}

/// Open or closed, and why (spec §3.2).
pub fn projection(kind: ItemKind, state: &str) -> (&'static str, Option<&'static str>) {
    let completed = match kind {
        ItemKind::Record => state == State::Done.as_wire(),
        ItemKind::Finding => state == FindingState::Fixed.as_wire(),
    };
    if completed {
        return ("closed", Some("completed"));
    }
    if kind == ItemKind::Finding && state == FindingState::Withdrawn.as_wire() {
        return ("closed", Some("not_planned"));
    }
    ("open", None)
}

/// The line an issue shows for a finding whose record is in the local tier
/// (routing spec §2.5): the record's title and IRI as text, escaped as a
/// ledger comment escapes a name, so neither mentions anyone, links an issue
/// or opens a tag. `None` when the block names no local record.
pub fn record_line(meta: &Meta) -> Option<String> {
    let r = meta.record.as_ref().filter(|r| r.is_local())?;
    Some(format!(
        "Record: {} — {}, held in the local tier, not on GitHub.",
        crate::ledger::render::escape(r.title.as_deref().unwrap_or("")),
        crate::ledger::render::escape(r.id.as_str())
    ))
}

/// The line an escalated issue shows (routing spec §3.3 step 2): who
/// escalated it, why, and its IRI in the local tier, each escaped as
/// [`record_line`] escapes a title. `None` when the block carries no
/// escalation.
pub fn escalation_line(meta: &Meta) -> Option<String> {
    let e = meta.escalated.as_ref()?;
    Some(format!(
        "Escalated from the local tier by {}: {}. Its local IRI was {}.",
        crate::ledger::render::escape(&e.by),
        crate::ledger::render::escape(&e.reason),
        crate::ledger::render::escape(e.from.as_str())
    ))
}

/// The prose, the line naming a local record when there is one, the line
/// naming where an escalated item came from when there is one, then the
/// block, sealed — a blank line between each. ⚠ `<` and `>` are escaped inside the JSON so no field
/// value can end the HTML comment or open a second block. They occur only
/// inside JSON strings, where `<`/`>` are the same text.
pub fn render_body(prose: &str, meta: &Meta) -> String {
    let meta = meta.clone().sealed();
    let json = serde_json::to_string(&meta)
        .expect("a Meta always serializes")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    let block = format!("{META_OPEN}\n{json}\n{META_CLOSE}");
    let shown = [
        Some(prose.to_string()),
        record_line(&meta),
        escalation_line(&meta),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    if shown.is_empty() {
        format!("{block}\n")
    } else {
        format!("{shown}\n\n{block}\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyError {
    Missing,
    Damaged(String),
    UnknownFormat(u64),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyError::Missing => write!(f, "has no fl block"),
            BodyError::Damaged(why) => write!(f, "has a damaged fl block ({why})"),
            BodyError::UnknownFormat(n) => write!(
                f,
                "has an fl block of format {n}, and this fl reads formats {FL_FORMAT} to \
                 {FL_FORMAT_ESCALATED}: upgrade fl to read it"
            ),
        }
    }
}

/// The prose and the block. The block is the LAST opener in the body: fl
/// always writes it last, and anything before it — including a claim that
/// quotes the opener — is prose. ⚠ Searching from the front would let one
/// claim make its issue, and every list over the repository, unreadable.
pub fn parse_body(body: &str) -> Result<(String, Meta), BodyError> {
    let body = body.replace("\r\n", "\n");
    let Some(at) = body.rfind(META_OPEN) else {
        return Err(BodyError::Missing);
    };
    let rest = &body[at + META_OPEN.len()..];
    let Some(end) = rest.find(META_CLOSE) else {
        return Err(BodyError::Damaged("the comment is never closed".into()));
    };
    let loose: Value =
        serde_json::from_str(rest[..end].trim()).map_err(|e| BodyError::Damaged(e.to_string()))?;
    match loose.get("fl_format").and_then(Value::as_u64) {
        Some(FL_FORMAT | FL_FORMAT_ROUTED | FL_FORMAT_ESCALATED) => {}
        Some(n) => return Err(BodyError::UnknownFormat(n)),
        None => return Err(BodyError::Damaged("it has no `fl_format`".into())),
    }
    let meta: Meta =
        serde_json::from_value(loose).map_err(|e| BodyError::Damaged(e.to_string()))?;
    if meta.fl_format != meta.required_format() {
        return Err(BodyError::Damaged(format!(
            "its `fl_format` is {}, but its fields are format {}",
            meta.fl_format,
            meta.required_format()
        )));
    }
    if !rest[end + META_CLOSE.len()..].trim().is_empty() {
        return Err(BodyError::Damaged("text follows the block".into()));
    }
    // The lines fl writes from the block are not part of the prose: the
    // escalation's line last, then the local record's before it.
    let mut prose = body[..at].trim_end();
    for line in [escalation_line(&meta), record_line(&meta)]
        .into_iter()
        .flatten()
    {
        prose = prose
            .strip_suffix(line.as_str())
            .map_or(prose, str::trim_end);
    }
    Ok((prose.to_string(), meta))
}

/// What an issue is to fl.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // one value per read; boxing buys nothing
pub enum Read {
    /// Not fl's: a pull request, or an issue with no `fl:` label (spec §3.5).
    NotFl(String),
    Item {
        kind: ItemKind,
        meta: Meta,
        prose: String,
    },
}

/// Read an issue, comparing the labels, the block and the open/closed
/// status. ⚠ Any disagreement is `Diverged`, naming every value found
/// (spec §3.4); fl adopts neither side.
pub fn read_item(issue: &IssueView) -> Result<Read, StoreError> {
    if issue.is_pull_request {
        return Ok(Read::NotFl("a pull request".into()));
    }
    let fl: Vec<&str> = issue
        .labels
        .iter()
        .map(String::as_str)
        .filter(|l| l.starts_with("fl:"))
        .collect();
    if fl.is_empty() {
        return Ok(Read::NotFl("a GitHub issue with no fl label".into()));
    }
    let diverged = |detail: String| StoreError::Diverged {
        id: issue.url.clone(),
        detail,
    };
    let kinds: Vec<ItemKind> = ItemKind::ALL
        .into_iter()
        .filter(|k| fl.contains(&kind_label(*k).as_str()))
        .collect();
    let [kind] = kinds.as_slice() else {
        return Err(diverged(format!(
            "it carries {} fl kind labels ({fl:?}), not one",
            kinds.len()
        )));
    };
    let kind = *kind;
    let prefix = format!("{}/", kind_label(kind));
    let states: Vec<&str> = fl
        .iter()
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .collect();
    let [label_state] = states.as_slice() else {
        return Err(diverged(format!(
            "it carries {} {} state labels ({fl:?}), not one",
            states.len(),
            kind.as_wire()
        )));
    };
    let (prose, meta) = parse_body(&issue.body).map_err(|e| diverged(format!("its body {e}")))?;
    let mut problems = Vec::new();
    if meta.kind != kind {
        problems.push(format!(
            "the label says {} but the block says {}",
            kind.as_wire(),
            meta.kind.as_wire()
        ));
    }
    if meta.state != *label_state {
        problems.push(format!(
            "the label says `{label_state}` but the block says `{}`",
            meta.state
        ));
    }
    if !kind.valid_state(&meta.state) {
        problems.push(format!(
            "`{}` is not a {} state",
            meta.state,
            kind.as_wire()
        ));
    }
    let (want, _) = projection(kind, &meta.state);
    if issue.state != want {
        problems.push(format!(
            "the issue is {} but the block's state `{}` means {want}",
            issue.state, meta.state
        ));
    }
    let own = kind_label(kind);
    let stray: Vec<&&str> = fl
        .iter()
        .filter(|l| {
            **l != own && !l.starts_with(prefix.as_str()) && !l.starts_with(AREA_LABEL_PREFIX)
        })
        .collect();
    if !stray.is_empty() {
        problems.push(format!("it also carries {stray:?}"));
    }
    // Routing spec §1.1: the block is the truth; an area label that is
    // missing, extra or another area's is diverged.
    let areas: Vec<&str> = fl
        .iter()
        .copied()
        .filter(|l| l.starts_with(AREA_LABEL_PREFIX))
        .collect();
    let want = meta.area.as_deref().map(area_label);
    match (want.as_deref(), areas.as_slice()) {
        (None, []) => {}
        (Some(w), [one]) if *one == w => {}
        (Some(w), []) => problems.push(format!(
            "it has no `{w}` label, which its block's area needs"
        )),
        (w, found) => problems.push(format!(
            "its area labels {found:?} do not match its block's area ({})",
            w.unwrap_or("none")
        )),
    }
    if problems.is_empty() {
        Ok(Read::Item { kind, meta, prose })
    } else {
        Err(diverged(problems.join("; ")))
    }
}

/// A finding's title: the claim's first line, cut to GitHub's limit. The
/// whole claim stays in the body.
pub fn title_of(claim: &str) -> String {
    let first = claim.lines().next().unwrap_or("").trim();
    let first = if first.is_empty() { "(finding)" } else { first };
    if first.chars().count() <= TITLE_MAX {
        first.to_string()
    } else {
        let mut t: String = first.chars().take(TITLE_MAX - 1).collect();
        t.push('…');
        t
    }
}

/// `https://github.com/{owner}/{repo}/issues/{n}` → (`owner/repo`, n).
pub fn parse_issue_url(id: &Iri) -> Option<(String, u64)> {
    let rest = id.as_str().strip_prefix("https://github.com/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let [owner, repo, "issues", n] = parts.as_slice() else {
        return None;
    };
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((format!("{owner}/{repo}"), n.parse().ok()?))
}

pub fn is_issue_url(id: &Iri) -> bool {
    parse_issue_url(id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ids::seq_iri;

    fn meta(kind: ItemKind, state: &str) -> Meta {
        Meta::new(kind, state, ProjectId(seq_iri(1)))
    }

    fn issue(labels: &[&str], state: &str, body: &str) -> IssueView {
        IssueView {
            number: 1,
            url: Iri::parse("https://github.com/acme/widgets/issues/1").unwrap(),
            node_id: "I_1".into(),
            title: "t".into(),
            body: body.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            state: state.into(),
            state_reason: None,
            is_pull_request: false,
        }
    }

    #[test]
    fn a_body_round_trips_its_prose_and_its_block() {
        let mut m = meta(ItemKind::Finding, "raised");
        m.raised_by = Some("rev".into());
        let body = render_body("the claim\nsecond line", &m);
        assert_eq!(
            parse_body(&body).unwrap(),
            ("the claim\nsecond line".into(), m)
        );
    }

    #[test]
    fn a_value_containing_comment_markers_cannot_break_the_block() {
        let mut m = meta(ItemKind::Finding, "withdrawn");
        m.withdrawn_reason = Some("ends early --> and <!-- fl:meta again".into());
        let body = render_body("prose with --> in it", &m);
        let (prose, back) = parse_body(&body).unwrap();
        assert_eq!(prose, "prose with --> in it");
        assert_eq!(back, m);
    }

    /// Review Focus 3: a claim that quotes the block's opener is prose.
    #[test]
    fn a_claim_quoting_the_opener_is_prose_and_the_block_still_reads() {
        let m = meta(ItemKind::Finding, "raised");
        let claim = "the parser breaks on <!-- fl:meta\n{\"x\":1}\n--> in a claim";
        let (prose, back) = parse_body(&render_body(claim, &m)).unwrap();
        assert_eq!((prose.as_str(), back), (claim, m));
    }

    #[test]
    fn a_body_that_is_missing_damaged_or_newer_is_named() {
        let good = render_body("p", &meta(ItemKind::Record, "todo"));
        assert_eq!(parse_body("just prose"), Err(BodyError::Missing));
        assert!(matches!(
            parse_body(&good.replace("\"kind\"", "\"kin\"")),
            Err(BodyError::Damaged(_))
        ));
        assert!(matches!(
            parse_body(&format!("{good}\nmore")),
            Err(BodyError::Damaged(_))
        ));
        assert_eq!(
            parse_body(&good.replace("\"fl_format\":1", "\"fl_format\":4")),
            Err(BodyError::UnknownFormat(4))
        );
        let said = BodyError::UnknownFormat(4).to_string();
        assert!(
            said.contains("formats 1 to 3") && said.contains("upgrade fl"),
            "{said}"
        );
    }

    #[test]
    fn a_consistent_issue_reads_as_its_item() {
        let m = meta(ItemKind::Record, "done");
        let i = issue(
            &["bug", "fl:record", "fl:record/done"],
            "closed",
            &render_body("", &m),
        );
        assert!(matches!(
            read_item(&i).unwrap(),
            Read::Item {
                kind: ItemKind::Record,
                ..
            }
        ));
    }

    #[test]
    fn a_label_that_disagrees_with_the_block_is_diverged_naming_both() {
        let m = meta(ItemKind::Record, "doing");
        let i = issue(
            &["fl:record", "fl:record/review"],
            "open",
            &render_body("", &m),
        );
        let err = read_item(&i).unwrap_err().to_string();
        assert!(err.contains("`review`") && err.contains("`doing`"), "{err}");
    }

    #[test]
    fn two_state_labels_or_a_status_that_disagrees_are_diverged() {
        let m = meta(ItemKind::Record, "doing");
        let two = issue(
            &["fl:record", "fl:record/doing", "fl:record/done"],
            "open",
            &render_body("", &m),
        );
        assert!(matches!(read_item(&two), Err(StoreError::Diverged { .. })));
        let closed = issue(
            &["fl:record", "fl:record/doing"],
            "closed",
            &render_body("", &m),
        );
        let err = read_item(&closed).unwrap_err().to_string();
        assert!(err.contains("closed") && err.contains("open"), "{err}");
    }

    #[test]
    fn a_pull_request_or_an_unlabelled_issue_is_not_fls() {
        let mut pr = issue(&["fl:record", "fl:record/todo"], "open", "");
        pr.is_pull_request = true;
        assert_eq!(
            read_item(&pr).unwrap(),
            Read::NotFl("a pull request".into())
        );
        assert!(matches!(
            read_item(&issue(&["bug"], "open", "")).unwrap(),
            Read::NotFl(_)
        ));
    }

    #[test]
    fn the_projection_closes_exactly_the_terminal_states() {
        assert_eq!(
            projection(ItemKind::Record, "done"),
            ("closed", Some("completed"))
        );
        assert_eq!(
            projection(ItemKind::Finding, "fixed"),
            ("closed", Some("completed"))
        );
        assert_eq!(
            projection(ItemKind::Finding, "withdrawn"),
            ("closed", Some("not_planned"))
        );
        for s in ItemKind::Record
            .states()
            .into_iter()
            .filter(|s| *s != "done")
        {
            assert_eq!(projection(ItemKind::Record, s), ("open", None), "{s}");
        }
    }

    #[test]
    fn every_state_has_a_label_and_fl_replaces_only_its_own() {
        assert_eq!(
            all_labels().len(),
            2 + State::ALL.len() + FindingState::ALL.len(),
            "one kind label per kind and one state label per state"
        );
        let after = labels_after(
            &["bug".into(), "fl:record/todo".into()],
            ItemKind::Record,
            "doing",
            None,
        );
        assert_eq!(after, vec!["bug", "fl:record", "fl:record/doing"]);
    }

    #[test]
    fn a_long_first_line_is_cut_to_the_title_limit() {
        let long = "x".repeat(300);
        let t = title_of(&format!("{long}\nrest"));
        assert_eq!(t.chars().count(), TITLE_MAX);
        assert!(t.ends_with('…'));
        assert_eq!(title_of("short\nrest"), "short");
    }

    /// `read_item`'s "exactly one kind label" check
    /// (`let [kind] = kinds.as_slice() else …`).
    #[test]
    fn zero_or_two_kind_labels_are_diverged_naming_the_kind_labels() {
        let m = meta(ItemKind::Record, "todo");
        let two_kinds = issue(
            &["fl:record", "fl:finding", "fl:record/todo"],
            "open",
            &render_body("", &m),
        );
        let err = read_item(&two_kinds).unwrap_err().to_string();
        assert!(
            err.contains("fl:record") && err.contains("fl:finding") && err.contains('2'),
            "{err}"
        );

        let no_kind = issue(&["fl:record/todo"], "open", &render_body("", &m));
        assert!(
            matches!(read_item(&no_kind), Err(StoreError::Diverged { .. })),
            "a state label with no kind label must still diverge"
        );
    }

    /// `read_item`'s stray-label check
    /// (`if !stray.is_empty() { … }`).
    #[test]
    fn a_stray_label_is_diverged_naming_it() {
        let m = meta(ItemKind::Record, "todo");
        let i = issue(
            &["fl:record", "fl:record/todo", "fl:finding/raised"],
            "open",
            &render_body("", &m),
        );
        let err = read_item(&i).unwrap_err().to_string();
        assert!(err.contains("fl:finding/raised"), "{err}");
    }

    /// `#[serde(deny_unknown_fields)]` on `Meta`
    /// itself — an unknown top-level field in the block is damaged.
    #[test]
    fn an_unknown_top_level_field_in_the_block_is_damaged() {
        let m = meta(ItemKind::Record, "todo");
        let body = render_body("", &m);
        assert!(body.contains('{'), "{body}");
        let bad = body.replacen('{', "{\"extra\":1,", 1);
        assert!(matches!(parse_body(&bad), Err(BodyError::Damaged(_))));
    }

    /// `#[serde(deny_unknown_fields)]` on
    /// `RecordRef` — an unknown field nested inside `record` is damaged
    /// too, independently of `Meta`'s own guard.
    #[test]
    fn an_unknown_field_inside_record_is_damaged() {
        let mut m = meta(ItemKind::Finding, "raised");
        m.record = Some(RecordRef {
            id: Iri::parse("urn:uuid:00000000-0000-7000-8000-000000000099").unwrap(),
            node_id: Some("I_9".into()),
            title: None,
        });
        let body = render_body("", &m);
        assert!(body.contains("\"record\":{"), "{body}");
        let bad = body.replacen("\"record\":{", "\"record\":{\"extra\":1,", 1);
        assert!(matches!(parse_body(&bad), Err(BodyError::Damaged(_))));
    }

    fn local_ref(title: &str) -> RecordRef {
        RecordRef {
            id: Iri::parse("urn:uuid:00000000-0000-7000-8000-000000000042").unwrap(),
            node_id: None,
            title: Some(title.into()),
        }
    }

    /// Everything before the block: what GitHub renders.
    fn shown(body: &str) -> &str {
        &body[..body.rfind(META_OPEN).unwrap()]
    }

    // Routing spec decision 14: format 2 exactly when the block carries an
    // area or a reference to a local record — computed, whatever the field
    // held.
    #[test]
    fn the_block_is_format_2_exactly_when_it_carries_an_area_or_a_local_record() {
        let format = |m: &Meta| parse_body(&render_body("p", m)).unwrap().1.fl_format;
        let mut plain = meta(ItemKind::Finding, "raised");
        plain.fl_format = 7;
        assert_eq!(format(&plain), 1, "computed, never taken from the field");
        let mut with_area = meta(ItemKind::Record, "todo");
        with_area.area = Some("code".into());
        assert_eq!(format(&with_area), 2);
        let mut on_github = meta(ItemKind::Finding, "raised");
        on_github.record = Some(RecordRef {
            id: Iri::parse("https://github.com/acme/widgets/issues/3").unwrap(),
            node_id: Some("I_3".into()),
            title: None,
        });
        assert_eq!(format(&on_github), 1);
        assert!(!shown(&render_body("p", &on_github)).contains("Record: "));
        let mut on_local = meta(ItemKind::Finding, "raised");
        on_local.record = Some(local_ref("t"));
        assert_eq!(format(&on_local), 2);
        assert!(render_body("p", &plain).contains("\"fl_format\":1"));
        assert!(render_body("p", &with_area).contains("\"fl_format\":2"));
        assert!(
            !render_body("p", &plain).contains("\"area\""),
            "skipped when absent"
        );
    }

    #[test]
    fn a_block_whose_format_disagrees_with_its_fields_is_damaged() {
        let plain = render_body("p", &meta(ItemKind::Record, "todo"));
        let lying = plain.replace("\"fl_format\":1", "\"fl_format\":2");
        assert_ne!(lying, plain, "the edit must have landed");
        match parse_body(&lying) {
            Err(BodyError::Damaged(why)) => {
                assert!(why.contains("but its fields are format 1"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        let mut m = meta(ItemKind::Record, "todo");
        m.area = Some("code".into());
        let lying = render_body("p", &m).replace("\"fl_format\":2", "\"fl_format\":1");
        assert!(matches!(parse_body(&lying), Err(BodyError::Damaged(_))));
    }

    // Routing spec §2.5: a reader on GitHub cannot open a local record, so
    // the issue names it as text — text that mentions nobody and links
    // nothing — and the claim reads back without it.
    #[test]
    fn a_local_record_reference_shows_as_plain_text_and_reads_back_as_the_claim() {
        let mut m = meta(ItemKind::Finding, "raised");
        m.record = Some(local_ref("@alice: fix #3 <b>"));
        let body = render_body("the claim", &m);
        let text = shown(&body);
        assert!(text.contains("Record: "), "{text}");
        assert!(
            text.contains("held in the local tier, not on GitHub"),
            "{text}"
        );
        assert!(
            text.contains("00000000-0000-7000-8000-000000000042"),
            "{text}"
        );
        assert!(
            !text.contains("@alice"),
            "a title must not mention anyone: {text}"
        );
        assert!(
            !text.contains("#3"),
            "a title must not link an issue: {text}"
        );
        assert!(!text.contains("<b>"), "{text}");
        let (prose, back) = parse_body(&body).unwrap();
        assert_eq!(prose, "the claim");
        assert_eq!(back, m.clone().sealed());
        let empty = render_body("", &m);
        assert!(
            empty.starts_with("Record: "),
            "an empty claim leaves no blank lines before the line: {empty:?}"
        );
        let (prose, _) = parse_body(&empty).unwrap();
        assert_eq!(prose, "", "an empty claim reads back empty");
    }

    // Routing spec §1.1: the area label is one of fl's, rewritten from the
    // block like the other two.
    #[test]
    fn every_rewrite_keeps_the_area_label_from_the_block() {
        let after = labels_after(
            &["bug".into(), "fl:record/todo".into(), "fl:area/old".into()],
            ItemKind::Record,
            "doing",
            Some("code"),
        );
        assert_eq!(
            after,
            vec!["bug", "fl:record", "fl:record/doing", "fl:area/code"]
        );
        let none = labels_after(&["fl:area/code".into()], ItemKind::Record, "todo", None);
        assert_eq!(none, vec!["fl:record", "fl:record/todo"]);
    }

    // Routing spec §1.1: "an issue whose area label is missing or differs
    // from its block reads as diverged, as a wrong state label does today".
    #[test]
    fn an_area_label_that_is_missing_or_differs_from_the_block_is_diverged() {
        let mut m = meta(ItemKind::Record, "todo");
        m.area = Some("code".into());
        let body = render_body("", &m);
        let read = |labels: &[&str]| read_item(&issue(labels, "open", &body));
        assert!(matches!(
            read(&["fl:record", "fl:record/todo", "fl:area/code"]).unwrap(),
            Read::Item { .. }
        ));
        let err = read(&["fl:record", "fl:record/todo"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("which its block's area needs") && err.contains("fl:area/code"),
            "{err}"
        );
        let err = read(&["fl:record", "fl:record/todo", "fl:area/design"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("do not match its block's area") && err.contains("fl:area/design"),
            "{err}"
        );
        let err = read(&["fl:record", "fl:record/todo", "fl:area/code", "fl:area/x"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("do not match its block's area"), "{err}");
        let plain = render_body("", &meta(ItemKind::Record, "todo"));
        let err = read_item(&issue(
            &["fl:record", "fl:record/todo", "fl:area/code"],
            "open",
            &plain,
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("do not match its block's area (none)"),
            "{err}"
        );
    }

    fn escalated_from(by: &str, reason: &str) -> EscalatedFrom {
        EscalatedFrom {
            from: Iri::parse("urn:uuid:00000000-0000-7000-8000-000000000007").unwrap(),
            by: by.into(),
            reason: reason.into(),
        }
    }

    const ESCALATED_LINE: &str = "Escalated from the local tier by alice: it needs a person. \
                                  Its local IRI was urn:uuid:00000000-0000-7000-8000-000000000007.";

    // Routing spec §3.3: a block that carries its escalation is format 3,
    // whatever else it carries — an fl that reads formats 1 and 2 refuses it
    // as newer rather than reading it as damaged.
    #[test]
    fn the_block_is_format_3_exactly_when_it_carries_its_escalation() {
        let format = |m: &Meta| parse_body(&render_body("p", m)).unwrap().1.fl_format;
        for area in [None, Some("code")] {
            for record in [None, Some(local_ref("t"))] {
                let mut m = meta(ItemKind::Finding, "raised");
                m.area = area.map(str::to_string);
                m.record = record.clone();
                let before = if area.is_some() || record.is_some() {
                    2
                } else {
                    1
                };
                assert_eq!(format(&m), before, "{area:?} {record:?}");
                m.escalated = Some(escalated_from("alice", "it needs a person"));
                m.fl_format = 2;
                assert_eq!(format(&m), 3, "{area:?} {record:?}");
            }
        }
        let mut m = meta(ItemKind::Record, "needs_human");
        m.escalated = Some(escalated_from("alice", "it needs a person"));
        let body = render_body("", &m);
        assert!(
            body.contains(
                "\"escalated\":{\"from\":\"urn:uuid:00000000-0000-7000-8000-000000000007\",\
                 \"by\":\"alice\",\"reason\":\"it needs a person\"}"
            ),
            "{body}"
        );
        assert!(body.contains("\"fl_format\":3"), "{body}");
        assert!(
            !render_body("", &meta(ItemKind::Record, "todo")).contains("escalated"),
            "skipped when absent"
        );
    }

    // Routing spec §3.3 step 2: an escalated record's issue names who
    // escalated it, why, and its old IRI — after its prose, as a line fl
    // writes from the block and reads back out of the prose.
    #[test]
    fn an_escalated_record_shows_its_provenance_after_its_prose_and_reads_back_unchanged() {
        let mut m = meta(ItemKind::Record, "needs_human");
        m.escalated = Some(escalated_from("alice", "it needs a person"));
        assert_eq!(escalation_line(&m).as_deref(), Some(ESCALATED_LINE));
        assert_eq!(escalation_line(&meta(ItemKind::Record, "todo")), None);
        let body = render_body("the prose\nsecond line", &m);
        assert_eq!(
            shown(&body),
            format!("the prose\nsecond line\n\n{ESCALATED_LINE}\n\n")
        );
        assert_eq!(
            parse_body(&body).unwrap(),
            ("the prose\nsecond line".into(), m.clone().sealed())
        );
        let empty = render_body("", &m);
        assert_eq!(shown(&empty), format!("{ESCALATED_LINE}\n\n"));
        assert_eq!(
            parse_body(&empty).unwrap().0,
            "",
            "no prose reads back empty"
        );
    }

    // A finding's text is its claim: the record line and the provenance
    // line follow it, in that order, and the claim reads back unchanged.
    #[test]
    fn an_escalated_findings_claim_reads_back_as_the_claim() {
        let mut m = meta(ItemKind::Finding, "reproduced");
        m.record = Some(local_ref("t"));
        m.escalated = Some(escalated_from("alice", "it needs a person"));
        let record = record_line(&m).unwrap();
        let body = render_body("the claim", &m);
        assert_eq!(
            shown(&body),
            format!("the claim\n\n{record}\n\n{ESCALATED_LINE}\n\n")
        );
        assert_eq!(
            parse_body(&body).unwrap(),
            ("the claim".into(), m.clone().sealed())
        );
        m.record = Some(RecordRef {
            id: Iri::parse("https://github.com/acme/widgets/issues/3").unwrap(),
            node_id: Some("I_3".into()),
            title: None,
        });
        let body = render_body("the claim", &m);
        assert_eq!(shown(&body), format!("the claim\n\n{ESCALATED_LINE}\n\n"));
        assert_eq!(parse_body(&body).unwrap().0, "the claim");
    }

    // The line is text on GitHub: who and why mention nobody, link nothing,
    // open no tag and break no line — and the block keeps them as written.
    #[test]
    fn an_escalations_who_and_why_are_escaped_and_still_read_back() {
        let mut m = meta(ItemKind::Finding, "raised");
        m.escalated = Some(EscalatedFrom {
            from: Iri::parse("urn:x-local:item_7").unwrap(),
            by: "@someone <b>#12".into(),
            reason: "see #12 <b>now</b>\nask @someone".into(),
        });
        let body = render_body("the claim", &m);
        let line = escalation_line(&m).unwrap();
        assert_eq!(
            shown(&body),
            format!("the claim\n\n{line}\n\n"),
            "one line, after the claim"
        );
        assert!(
            line.starts_with(
                "Escalated from the local tier by @&#8203;someone &lt;b&gt;#&#8203;12: "
            ),
            "{line}"
        );
        assert!(
            line.contains("see #&#8203;12 &lt;b&gt;now&lt;/b&gt;<br>ask @&#8203;someone. "),
            "{line}"
        );
        assert!(
            line.ends_with("Its local IRI was urn:x-local:item\\_7."),
            "{line}"
        );
        for raw in ["@someone", "#12", "<b>", "item_7", "\n"] {
            assert!(!line.contains(raw), "{raw:?} in {line}");
        }
        assert_eq!(
            parse_body(&body).unwrap(),
            ("the claim".into(), m.clone().sealed())
        );
    }

    #[test]
    fn a_block_whose_format_disagrees_with_its_escalation_is_damaged() {
        let plain = render_body("p", &meta(ItemKind::Record, "todo"));
        let lying = plain.replace("\"fl_format\":1", "\"fl_format\":3");
        assert_ne!(lying, plain, "the edit must have landed");
        match parse_body(&lying) {
            Err(BodyError::Damaged(why)) => {
                assert!(why.contains("is 3, but its fields are format 1"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        let mut m = meta(ItemKind::Record, "todo");
        m.area = Some("code".into());
        m.escalated = Some(escalated_from("alice", "it needs a person"));
        let good = render_body("p", &m);
        let lying = good.replace("\"fl_format\":3", "\"fl_format\":2");
        assert_ne!(lying, good, "the edit must have landed");
        match parse_body(&lying) {
            Err(BodyError::Damaged(why)) => {
                assert!(why.contains("is 2, but its fields are format 3"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        let extra = good.replacen("\"escalated\":{", "\"escalated\":{\"extra\":1,", 1);
        assert_ne!(extra, good, "the edit must have landed");
        assert!(matches!(parse_body(&extra), Err(BodyError::Damaged(_))));
    }

    // An issue an fl that knows no escalation wrote reads exactly as it did,
    // and is written back byte for byte.
    #[test]
    fn an_older_format_block_reads_and_writes_exactly_as_before() {
        let one = "the prose\n\n<!-- fl:meta\n{\"fl_format\":1,\"kind\":\"record\",\
                   \"state\":\"todo\",\
                   \"project\":\"urn:uuid:00000000-0000-7000-8000-000000000001\",\
                   \"security\":false,\"also_known_as\":[],\"create_key\":\"urn:uuid:k\"}\n-->\n";
        let (prose, m) = parse_body(one).unwrap();
        let mut want = meta(ItemKind::Record, "todo");
        want.create_key = "urn:uuid:k".into();
        assert_eq!((prose.as_str(), &m), ("the prose", &want));
        assert_eq!(render_body(&prose, &m), one);
        let two = "the claim\n\nRecord: t — urn:uuid:00000000-0000-7000-8000-000000000042, \
                   held in the local tier, not on GitHub.\n\n<!-- fl:meta\n{\"fl_format\":2,\
                   \"kind\":\"finding\",\"state\":\"raised\",\
                   \"project\":\"urn:uuid:00000000-0000-7000-8000-000000000001\",\
                   \"area\":\"code\",\"record\":{\"id\":\
                   \"urn:uuid:00000000-0000-7000-8000-000000000042\",\"title\":\"t\"},\
                   \"raised_by\":\"rev\",\"security\":false,\"also_known_as\":[],\
                   \"create_key\":\"urn:uuid:k\"}\n-->\n";
        let (prose, m) = parse_body(two).unwrap();
        let mut want = meta(ItemKind::Finding, "raised");
        want.fl_format = 2;
        want.area = Some("code".into());
        want.record = Some(local_ref("t"));
        want.raised_by = Some("rev".into());
        want.create_key = "urn:uuid:k".into();
        assert_eq!((prose.as_str(), &m), ("the claim", &want));
        assert_eq!(render_body(&prose, &m), two);
    }

    #[test]
    fn only_a_github_issue_url_parses() {
        let ok = Iri::parse("https://github.com/Acme/Widgets/issues/41").unwrap();
        assert_eq!(parse_issue_url(&ok), Some(("Acme/Widgets".into(), 41)));
        for bad in [
            "https://github.com/acme/widgets/pull/41",
            "https://github.com/acme/widgets/issues/41/x",
            "https://example.com/acme/widgets/issues/41",
            "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b",
        ] {
            assert_eq!(parse_issue_url(&Iri::parse(bad).unwrap()), None, "{bad}");
        }
    }

    /// GitHub's GraphQL `Issue` node (the lists, spec §3.7) and its REST
    /// issue read as the same view: the lists and a single read must agree
    /// on what an item is.
    #[test]
    fn a_graphql_issue_node_reads_as_the_same_view_as_its_rest_issue() {
        let rest = serde_json::json!({
            "number": 7, "node_id": "I_7",
            "html_url": "https://github.com/acme/widgets/issues/7",
            "title": "t", "body": "b",
            "labels": [{"name": "fl:finding"}, {"name": "fl:finding/withdrawn"}],
            "state": "closed", "state_reason": "not_planned",
        });
        let node = serde_json::json!({
            "number": 7, "id": "I_7",
            "url": "https://github.com/acme/widgets/issues/7",
            "title": "t", "body": "b",
            "labels": {"totalCount": 2,
                       "nodes": [{"name": "fl:finding"}, {"name": "fl:finding/withdrawn"}]},
            "state": "CLOSED", "stateReason": "NOT_PLANNED",
            "createdAt": "2026-10-05T12:00:00Z",
        });
        assert_eq!(
            IssueView::from_graphql(&node).unwrap(),
            IssueView::from_json(&rest).unwrap()
        );
        let mut open = node.clone();
        open["state"] = serde_json::json!("OPEN");
        open["stateReason"] = Value::Null;
        let v = IssueView::from_graphql(&open).unwrap();
        assert_eq!((v.state.as_str(), v.state_reason), ("open", None));
    }

    /// A node fl cannot read in full is an error, never a view with a
    /// field guessed: a label left out would read as a different item.
    #[test]
    fn a_graphql_issue_node_with_labels_left_out_or_an_unknown_state_is_an_error() {
        let node = serde_json::json!({
            "number": 7, "id": "I_7",
            "url": "https://github.com/acme/widgets/issues/7",
            "title": "t", "body": "b",
            "labels": {"totalCount": 3, "nodes": [{"name": "fl:record"}]},
            "state": "OPEN", "stateReason": null,
        });
        let e = IssueView::from_graphql(&node).unwrap_err().to_string();
        assert!(e.contains("1 of its 3 labels"), "{e}");
        let mut no_labels = node.clone();
        no_labels["labels"] = Value::Null;
        let e = IssueView::from_graphql(&no_labels).unwrap_err().to_string();
        assert!(e.contains("`labels`"), "{e}");
        // ⚠ Not an empty body: read as one, an issue's create key would
        // be "not found", and a create-key search would send it again.
        for body in [Value::Null, Value::from(7)] {
            let mut no_body = node.clone();
            no_body["labels"] = serde_json::json!({"totalCount": 0, "nodes": []});
            no_body["body"] = body;
            let e = IssueView::from_graphql(&no_body).unwrap_err().to_string();
            assert!(e.contains("`body`"), "{e}");
        }
        let mut no_body = node.clone();
        no_body["labels"] = serde_json::json!({"totalCount": 0, "nodes": []});
        no_body.as_object_mut().unwrap().remove("body");
        assert!(IssueView::from_graphql(&no_body).is_err());
        let mut odd = node.clone();
        odd["labels"] = serde_json::json!({"totalCount": 0, "nodes": []});
        odd["state"] = serde_json::json!("MERGED");
        let e = IssueView::from_graphql(&odd).unwrap_err().to_string();
        assert!(e.contains("MERGED"), "{e}");
    }
}
