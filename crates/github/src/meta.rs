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
pub const META_OPEN: &str = "<!-- fl:meta";
pub const META_CLOSE: &str = "-->";
/// GitHub limits an issue title to 256 characters.
pub const TITLE_MAX: usize = 256;

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
    pub node_id: String,
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
            record: None,
            reproduction: None,
            raised_by: None,
            assigned_to: None,
            security: false,
            withdrawn_reason: None,
            also_known_as: vec![],
            create_key: format!("urn:uuid:{}", uuid::Uuid::now_v7()),
        }
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
            body: v
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
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
/// this kind's two (spec §3.3 — fl replaces only its own).
pub fn labels_after(current: &[String], kind: ItemKind, state: &str) -> Vec<String> {
    let mut out: Vec<String> = current
        .iter()
        .filter(|l| !l.starts_with("fl:"))
        .cloned()
        .collect();
    out.push(kind_label(kind));
    out.push(state_label(kind, state));
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

/// The prose, then the block. ⚠ `<` and `>` are escaped inside the JSON so
/// no field value can end the HTML comment or open a second block. They
/// occur only inside JSON strings, where `<`/`>` are the same text.
pub fn render_body(prose: &str, meta: &Meta) -> String {
    let json = serde_json::to_string(meta)
        .expect("a Meta always serializes")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    let block = format!("{META_OPEN}\n{json}\n{META_CLOSE}");
    if prose.is_empty() {
        format!("{block}\n")
    } else {
        format!("{prose}\n\n{block}\n")
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
                "has an fl block of format {n}, and this fl reads format {FL_FORMAT}"
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
        Some(FL_FORMAT) => {}
        Some(n) => return Err(BodyError::UnknownFormat(n)),
        None => return Err(BodyError::Damaged("it has no `fl_format`".into())),
    }
    let meta: Meta =
        serde_json::from_value(loose).map_err(|e| BodyError::Damaged(e.to_string()))?;
    if !rest[end + META_CLOSE.len()..].trim().is_empty() {
        return Err(BodyError::Damaged("text follows the block".into()));
    }
    Ok((body[..at].trim_end().to_string(), meta))
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
        .filter(|l| **l != own && !l.starts_with(prefix.as_str()))
        .collect();
    if !stray.is_empty() {
        problems.push(format!("it also carries {stray:?}"));
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
            parse_body(&good.replace("\"fl_format\":1", "\"fl_format\":2")),
            Err(BodyError::UnknownFormat(2))
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
            node_id: "I_9".into(),
        });
        let body = render_body("", &m);
        assert!(body.contains("\"record\":{"), "{body}");
        let bad = body.replacen("\"record\":{", "\"record\":{\"extra\":1,", 1);
        assert!(matches!(parse_body(&bad), Err(BodyError::Damaged(_))));
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
        let mut odd = node.clone();
        odd["labels"] = serde_json::json!({"totalCount": 0, "nodes": []});
        odd["state"] = serde_json::json!("MERGED");
        let e = IssueView::from_graphql(&odd).unwrap_err().to_string();
        assert!(e.contains("MERGED"), "{e}");
    }
}
