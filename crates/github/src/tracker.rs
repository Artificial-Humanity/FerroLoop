//! `GithubTracker`: the `Tracker` and `Handles` roles over the Issues of one
//! repository (GitHub tracker spec §2–§3).
//!
//! ⚠ It does not check project references — it cannot see the catalog. It
//! is always used through `CatalogChecked`, which does (spec §1.3).

use crate::client::{Client, Method};
use crate::meta::{self, IssueView, ItemKind, Meta, Read, RecordRef, TITLE_MAX};
use fl_core::at::At;
use fl_core::finding::{Finding, FindingState};
use fl_core::ids::{FindingId, Kind, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::model::{Record, State};
use fl_core::store::{Bindings, Handles, StoreError, Tracker};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub full_name: String,
    pub node_id: String,
}

/// Said once, when the tracker opens, and never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Renamed { from: String, to: String },
}

impl std::fmt::Display for Notice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Notice::Renamed { from, to } => write!(
                f,
                "the repository `{from}` is now `{to}`. fl follows it; update the `github` \
                 binding in config.toml to stop this notice"
            ),
        }
    }
}

/// What fl last read or wrote of an item: its block, prose and title.
type Seen = (Meta, String, String);

pub struct GithubTracker {
    client: Client,
    repo: Repo,
    labels_ready: Cell<bool>,
    /// Kinds seen this process, by issue number, so a handle lookup does not
    /// read the issue again.
    kinds: RefCell<BTreeMap<u64, ItemKind>>,
    /// ⚠ What this process last read of each item. The engine reads an
    /// item, runs gates for minutes, then writes: a write refuses if the
    /// item changed in between, so a finding withdrawn by someone else
    /// during a verify cannot be marked fixed (spec §3.3).
    seen: RefCell<BTreeMap<u64, Seen>>,
    /// How long to wait between searches for an ambiguous create's key.
    settle: std::time::Duration,
    /// How long fl waits for GitHub's timeline and edit history to show its
    /// own write, and how long it pauses between reads while it waits.
    visible_within: std::time::Duration,
    poll: std::time::Duration,
}

/// Timeline events that change what fl reads (spec §3.3). A comment or a
/// mention does not, and is not a conflict.
const STATE_EVENTS: [&str; 5] = ["labeled", "unlabeled", "closed", "reopened", "renamed"];

/// The order a list of issues is read in, by creation.
#[derive(Debug, Clone, Copy)]
enum Order {
    OldestFirst,
    NewestFirst,
}

/// How far before an attempt began the create-key search keeps reading.
/// The search reads issues newest first and stops at the first one GitHub
/// stamped (`createdAt`, by GitHub's clock) more than this before the
/// attempt began (by this machine's clock): the margin covers the skew
/// between the two clocks. Ten minutes is far more than the skew of a
/// machine whose clock is set by the network; an issue the attempt made
/// cannot be older than the attempt by more than the skew.
const CREATE_SEARCH_MARGIN: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Issues per page of a list filtered by an fl label: GitHub's largest.
const LIST_PAGE: u64 = 100;

/// Issues per page of the create-key search, which reads every issue —
/// fl's or not — with its whole body. Small, so a page of large bodies
/// stays under the client's limit on an answer's size (ureq's documented
/// default reads at most 10 MB — not measured against GitHub; GitHub allows a body of 65,536 characters, which JSON escaping
/// can make several times longer). The search usually stops within its
/// first page.
const SEARCH_PAGE: u64 = 25;

/// This machine's clock, in milliseconds after the Unix epoch.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A listed issue's `createdAt`. GitHub spells it to the second
/// (`2026-10-05T12:34:56Z`); it is read as an `At`, whose order is time
/// order. ⚠ One fl cannot read is an error: the search cannot tell
/// whether to stop at it.
fn created_at(node: &Value) -> Result<At, StoreError> {
    let raw = node.get("createdAt").and_then(Value::as_str).unwrap_or("");
    let spelled = match raw.len() {
        20 if raw.ends_with('Z') => format!("{}.000Z", &raw[..19]),
        _ => raw.to_string(),
    };
    At::parse(&spelled).map_err(|_| {
        backend(format!(
            "GitHub listed an issue created at {raw:?}, a time fl cannot read; retry"
        ))
    })
}

/// One state-changing timeline event: its kind and, for a label event,
/// the label.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    id: u64,
    kind: String,
    label: Option<String>,
}

/// The events of a whole timeline (oldest first) that can record a change.
/// ⚠ A `labeled` event for a label already on the issue is dropped as noise.
/// Measured live on 2026-10-05: GitHub sometimes records a `labeled` event a
/// second time, about 0-1 s after the first (4 of 10 calls that added two
/// labels; 2 of 33 issues fl created adding one label per call), and
/// re-adding a label the issue already carries records no event at all. So
/// such an event cannot be anyone's write. Which labels are on the issue is
/// replayed from the timeline itself, from none: never from an issue fl
/// read, which can already hold a label someone added inside the window.
/// An `unlabeled` event, and every other kind, is always kept.
/// Measured live on 2026-10-05 (one sample): deleting a label from the
/// repository records an `unlabeled` event on each issue that carried it,
/// and adding it again after it is re-created records a new `labeled`, so
/// that path keeps the replay right.
/// ⚠ Modelled: the timeline records every label change of the issue, in
/// order, and an event that shows means every earlier one shows too. A
/// label change with no event would leave the replay wrong; a later
/// `labeled` event for that label could then be taken for noise.
fn changes(events: &[Event]) -> Vec<&Event> {
    let mut on = BTreeSet::new();
    events
        .iter()
        .filter(|e| match (e.kind.as_str(), &e.label) {
            ("labeled", Some(l)) => on.insert(l.clone()),
            ("unlabeled", Some(l)) => {
                on.remove(l);
                true
            }
            _ => true,
        })
        .collect()
}

/// The changes `after` shows that `before` did not, in timeline order.
fn new_changes<'a>(before: &Window, after: &'a Window) -> Vec<&'a Event> {
    let old: BTreeSet<u64> = before.events.iter().map(|e| e.id).collect();
    changes(&after.events)
        .into_iter()
        .filter(|e| !old.contains(&e.id))
        .collect()
}

/// What GitHub has recorded about an issue's changes at one moment.
struct Window {
    /// The state-changing events, in timeline order.
    events: Vec<Event>,
    edits: BTreeSet<String>,
    /// The edit history's `totalCount`, which does not depend on the order
    /// GitHub lists the entries in.
    edits_total: u64,
}

/// What `repair` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repaired {
    pub number: u64,
    pub state: String,
    /// `false` when the issue already agreed with its block.
    pub changed: bool,
}

/// What an issue number reached.
#[allow(clippy::large_enum_variant)] // one value per read; boxing buys nothing
enum Fetched {
    Found(IssueView),
    Absent,
    Gone,
    Moved(String),
}

/// What an id reached, for one wanted kind.
#[allow(clippy::large_enum_variant)]
enum Found {
    Item(IssueView, Meta, String),
    OtherKind(ItemKind),
    Absent,
}

/// Whether an id is an issue of this repository.
enum Owner {
    Ours(u64),
    /// Not ours; `Some` carries why, for the `NotOwned` message.
    Elsewhere(Option<String>),
}

fn backend(msg: String) -> StoreError {
    StoreError::Backend(msg)
}

/// ⚠ Every error inside `after_ambiguous_create` — a failed search for the
/// create key, and ANY failure of the resend (a transport failure, a rate
/// limit, a refused credential, a rejected request) — gets the "look before
/// retrying" advice (`look_before_retrying`). The FIRST attempt failed
/// ambiguously (a 5xx, or the connection dropping), so by then fl cannot
/// know whether the issue exists, whatever the later error is about: a plain
/// "retry" would make a duplicate whenever the first attempt had landed.
/// (Earlier rounds wrapped only a transport failure here, reading the other
/// errors as unrelated to that ambiguity; they are not, because the
/// ambiguity comes from the first attempt, not from the error.)
fn after_ambiguous_failure(title: &str, step: &str, e: StoreError) -> StoreError {
    backend(format!(
        "an issue create failed in a way that may still have created the issue, and then \
         {step} ({e}). {}",
        look_before_retrying(title)
    ))
}

/// The advice once a create may have landed: where to look for the issue,
/// and what to do with it. ⚠ Not "list the repository's fl issues": a
/// create sends no labels (`label_created` adds them), so an issue it made
/// may carry none, and then it is in neither `fl record list`, `fl finding
/// list`, nor a GitHub list filtered by fl's labels. Following that advice
/// made the duplicate it was meant to prevent.
fn look_before_retrying(title: &str) -> String {
    format!(
        "Before retrying, look among the repository's newest issues, labelled or not, for one \
         titled {title:?}; if it is there, run `fl github repair <number> --by <name>` on it \
         instead of creating it again, so the retry makes no duplicate. GitHub's issue list can \
         take minutes to show a new issue: if it is not there, wait a few minutes and look again \
         before retrying"
    )
}

fn text(v: &Value, k: &str) -> Result<String, StoreError> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| backend(format!("GitHub sent a repository without `{k}`")))
}

/// `GET /repos/{name}`, following ONE redirect (a renamed or transferred
/// repository answers 301). `Ok(None)` for 404. The `bool` is `has_issues` —
/// carried alongside rather than on `Repo` itself, since only `open` needs
/// it (spec §2.6): a repository with Issues turned off answers a 410 on
/// every issue, which would otherwise be misread as "deleted".
fn read_repo(client: &Client, name: &str) -> Result<Option<(Repo, bool)>, StoreError> {
    let mut reply = client.send(Method::Get, &format!("/repos/{name}"), None)?;
    if matches!(reply.status, 301 | 302 | 307 | 308) {
        let to = reply
            .location
            .clone()
            .ok_or_else(|| backend(format!("GitHub redirected `{name}` without a Location")))?;
        reply = client.send(Method::Get, &to, None)?;
    }
    match reply.status {
        200 => Ok(Some((
            Repo {
                full_name: text(&reply.body, "full_name")?,
                node_id: text(&reply.body, "node_id")?,
            },
            // Absent only from a GitHub answer this fl has never seen;
            // treated as "on" rather than blocking a repository over a
            // field that was never actually withheld.
            reply
                .body
                .get("has_issues")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        ))),
        404 => Ok(None),
        s => Err(backend(format!(
            "GitHub answered {s} when fl read the repository `{name}`; retry"
        ))),
    }
}

/// ⚠ The response is the postcondition (spec §3.3): GitHub silently drops
/// labels a caller may not set, so "no error" is not "written".
fn check_written(
    back: &IssueView,
    title: &str,
    labels: &[String],
    body: &str,
    state: &str,
    reason: Option<&str>,
) -> Result<(), StoreError> {
    let problems = written_problems(back, title, Some(labels), body, state, reason);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(backend(format!(
            "GitHub accepted the write to {} but did not apply it: {}. The credential may \
             lack permission to set labels (Issues: read and write)",
            back.url,
            problems.join("; ")
        )))
    }
}

/// How GitHub's answer differs from what fl sent; empty when it does not.
/// `labels` is `None` for a create, which sends none: they are added, and
/// checked, by their own calls (`label_created`).
fn written_problems(
    back: &IssueView,
    title: &str,
    labels: Option<&[String]>,
    body: &str,
    state: &str,
    reason: Option<&str>,
) -> Vec<String> {
    let mut problems = Vec::new();
    if back.title != title {
        problems.push("the title came back different".to_string());
    }
    if reason.is_some() && back.state_reason.as_deref() != reason {
        problems.push(format!(
            "the issue came back closed as `{:?}`, not `{reason:?}`",
            back.state_reason
        ));
    }
    if let Some(labels) = labels {
        let want: BTreeSet<&str> = labels.iter().map(String::as_str).collect();
        let got: BTreeSet<&str> = back.labels.iter().map(String::as_str).collect();
        if want != got {
            problems.push(format!("the labels came back as {got:?}, not {want:?}"));
        }
    }
    if back.body.replace("\r\n", "\n") != body {
        problems.push("the body came back different".to_string());
    }
    if back.state != state {
        problems.push(format!(
            "the issue came back `{}`, not `{state}`",
            back.state
        ));
    }
    problems
}

/// An error about an issue fl has just created: it exists without fl's
/// labels (`certain`), or perhaps without some or all of them — GitHub
/// can apply one label and drop the other — so fl's lists leave it out, or
/// report it as diverged. ⚠ Never "retry":
/// a retry mints a new create key and makes a DUPLICATE. `fl github
/// repair` restores the labels from the block instead (spec §0.1b, 14).
fn unlabelled_issue(issue: &IssueView, why: &str, certain: bool) -> StoreError {
    let state = if certain {
        "It exists without fl's labels, so fl's lists leave it out"
    } else {
        "It may exist without some or all of fl's labels, so fl's lists leave it out or report \
         it as diverged"
    };
    backend(format!(
        "{} was created, but {why}. {state}. Do not create it again: `fl github repair {} \
         --by <name>` restores its labels from its block",
        issue.url, issue.number
    ))
}

impl GithubTracker {
    /// Read the repository and compare it with the node id the local store
    /// remembers for `configured` (spec §2.4). The first open records it.
    pub fn open(
        client: Client,
        configured: &str,
        memory: &dyn Bindings,
    ) -> Result<(Self, Option<Notice>), StoreError> {
        let (repo, has_issues) = read_repo(&client, configured)?.ok_or_else(|| {
            backend(format!(
                "the repository `{configured}` does not exist, or the credential cannot read \
                 it. Check the `github` binding and the credential"
            ))
        })?;
        // ⚠ A repository with Issues turned off answers 410 on every issue —
        // indistinguishable, at that point, from an issue GitHub deleted.
        // Caught here, once, so `fetch` never has to guess which one it saw.
        if !has_issues {
            return Err(backend(format!(
                "the repository `{}` has Issues turned off, so fl has nowhere to keep records \
                 and findings. Turn Issues on for `{}`, or bind another repository",
                repo.full_name, repo.full_name
            )));
        }
        match memory.bound_node_id(configured)? {
            None => memory.bind_node_id(configured, &repo.node_id)?,
            Some(bound) if bound != repo.node_id => {
                return Err(StoreError::RepositoryReplaced {
                    name: configured.to_string(),
                    bound,
                    found: repo.node_id,
                });
            }
            Some(_) => {}
        }
        let notice = (!repo.full_name.eq_ignore_ascii_case(configured)).then(|| Notice::Renamed {
            from: configured.to_string(),
            to: repo.full_name.clone(),
        });
        let tracker = Self {
            client,
            repo,
            labels_ready: Cell::new(false),
            kinds: RefCell::new(BTreeMap::new()),
            seen: RefCell::new(BTreeMap::new()),
            settle: std::time::Duration::from_secs(2),
            visible_within: std::time::Duration::from_secs(10),
            poll: std::time::Duration::from_millis(250),
        };
        Ok((tracker, notice))
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// The client this tracker writes through, so the GitHub ledger can
    /// share its credential and origin guard (GitHub ledger spec §1.1).
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn describe(&self) -> String {
        self.client.describe()
    }

    /// Who GitHub says fl writes as (spec §5.4).
    pub fn identity(&self) -> Result<String, StoreError> {
        self.client.identity()
    }

    /// Tests only: no pause between create-key searches.
    #[doc(hidden)]
    pub fn without_settle(mut self) -> Self {
        self.settle = std::time::Duration::ZERO;
        self
    }

    /// Tests only: how long to wait for GitHub to show fl's own write, and
    /// the pause between reads.
    #[doc(hidden)]
    pub fn with_visibility(
        mut self,
        within: std::time::Duration,
        poll: std::time::Duration,
    ) -> Self {
        self.visible_within = within;
        self.poll = poll;
        self
    }

    fn remember(&self, n: u64, meta: &Meta, prose: &str, title: &str) {
        self.seen
            .borrow_mut()
            .insert(n, (meta.clone(), prose.to_string(), title.to_string()));
    }

    pub fn issue_url(&self, number: u64) -> Iri {
        Iri::parse(&format!(
            "https://github.com/{}/issues/{number}",
            self.repo.full_name
        ))
        .expect("an issue URL is an IRI")
    }

    fn label(&self) -> String {
        format!("github:{}", self.repo.full_name)
    }

    fn path(&self, rest: &str) -> String {
        format!("/repos/{}{rest}", self.repo.full_name)
    }

    /// Every issue carrying `label` (every issue, for `None`), read from
    /// GraphQL's `issues` connection a page at a time in `order`, each node
    /// handed to `each` until it answers `false` (spec §3.7), `page` issues
    /// to a page. Each node is one `IssueView::from_graphql` reads, plus
    /// `createdAt`.
    ///
    /// ⚠ GraphQL, never the REST issue list. Measured live on 2026-10-05:
    /// the REST list left a new issue out for 31-93 s (once more than
    /// 180 s), and showed a label change 30-100 s late; this connection
    /// showed a new issue within 1 s (5 of 5) and a label change within
    /// 2-3 s (5 of 5). Read through REST, a list was silently short, an
    /// alias scan could miss an owner, and a create-key search could miss
    /// a create that landed and send it again.
    /// ⚠ A failed page, or a page with no readable `issues` connection, is
    /// an ERROR, never a short list.
    fn each_issue(
        &self,
        label: Option<&str>,
        order: Order,
        page: u64,
        mut each: impl FnMut(&Value) -> Result<bool, StoreError>,
    ) -> Result<(), StoreError> {
        let (owner, name) = self
            .repo
            .full_name
            .split_once('/')
            .expect("a full name is owner/name");
        // Two spellings rather than `labels: null` for "no filter": what
        // GitHub makes of an explicit null filter is unmeasured.
        let (declared, filter) = match label {
            Some(_) => (", $labels: [String!]!", "labels: $labels, "),
            None => ("", ""),
        };
        let query = format!(
            "query($owner: String!, $name: String!, $first: Int!, $after: String, $direction: \
             OrderDirection!{declared}) {{ repository(owner: $owner, name: $name) {{ \
             issues({filter}states: [OPEN, CLOSED], first: $first, after: $after, orderBy: \
             {{field: CREATED_AT, direction: $direction}}) {{ pageInfo {{ hasNextPage \
             endCursor }} nodes {{ number id url title body state stateReason createdAt \
             labels(first: 100) {{ totalCount nodes {{ name }} }} }} }} }} }}"
        );
        let direction = match order {
            Order::OldestFirst => "ASC",
            Order::NewestFirst => "DESC",
        };
        let what = label.map_or_else(|| "the full".to_string(), |l| format!("the `{l}`"));
        let mut after: Option<String> = None;
        loop {
            let mut vars = json!({"owner": owner, "name": name, "first": page,
                                  "after": after, "direction": direction});
            if let Some(l) = label {
                vars["labels"] = json!([l]);
            }
            let data = self.client.graphql(&query, vars)?;
            let issues = data.pointer("/repository/issues");
            let (Some(nodes), Some(more)) = (
                issues
                    .and_then(|i| i.get("nodes"))
                    .and_then(Value::as_array),
                issues
                    .and_then(|i| i.pointer("/pageInfo/hasNextPage"))
                    .and_then(Value::as_bool),
            ) else {
                return Err(backend(format!(
                    "GitHub answered a page of {what} issue list of {} without its `issues` \
                     connection. A list with a missing page is not a list; retry",
                    self.repo.full_name
                )));
            };
            for node in nodes {
                if !each(node)? {
                    return Ok(());
                }
            }
            if !more {
                return Ok(());
            }
            // ⚠ Progress, checked: a page that says another follows but
            // holds no issue, or hands back the cursor it was asked for,
            // would be read again forever.
            if nodes.is_empty() {
                return Err(backend(format!(
                    "GitHub answered an empty page of {what} issue list of {} and said another \
                     follows; retry",
                    self.repo.full_name
                )));
            }
            let cursor = issues
                .and_then(|i| i.pointer("/pageInfo/endCursor"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    backend(format!(
                        "GitHub said {what} issue list of {} has another page but gave no \
                         cursor to it; retry",
                        self.repo.full_name
                    ))
                })?;
            if after.as_deref() == Some(cursor) {
                return Err(backend(format!(
                    "GitHub said {what} issue list of {} has another page but gave the same \
                     cursor again; retry",
                    self.repo.full_name
                )));
            }
            after = Some(cursor.to_string());
        }
    }

    /// Whether `id` is an issue of THIS repository (spec §2.2, §2.4). A URL
    /// under another name costs one read: after a rename or a transfer, the
    /// old name can still reach this repository — or, once reused, another.
    fn owner(&self, id: &Iri) -> Result<Owner, StoreError> {
        let Some((name, n)) = meta::parse_issue_url(id) else {
            return Ok(Owner::Elsewhere(None));
        };
        if name.eq_ignore_ascii_case(&self.repo.full_name) {
            return Ok(Owner::Ours(n));
        }
        Ok(match read_repo(&self.client, &name)? {
            Some((r, _)) if r.node_id == self.repo.node_id => Owner::Ours(n),
            // ⚠ "Now" only when GitHub led the name somewhere under another
            // name — a redirect after a rename or a transfer. A
            // repository answering under the name itself is
            // just another repository: fl cannot tell a reused old name of
            // this one from a name it never had, so it claims neither.
            Some((r, _)) if !r.full_name.eq_ignore_ascii_case(&name) => {
                Owner::Elsewhere(Some(format!(
                    "`{name}` now leads to `{}`, a different repository from the bound one",
                    r.full_name
                )))
            }
            Some(_) => Owner::Elsewhere(Some(format!(
                "`{name}` is another repository, not the bound `{}`",
                self.repo.full_name
            ))),
            None => Owner::Elsewhere(None),
        })
    }

    /// The issue number `id` names here, directly or as an alias.
    fn locate(&self, id: &Iri) -> Result<u64, StoreError> {
        let why = match self.owner(id)? {
            Owner::Ours(n) => return Ok(n),
            Owner::Elsewhere(why) => why,
        };
        if let Some(n) = self.alias_owner(id)? {
            return Ok(n);
        }
        let searched = match why {
            Some(why) => format!("{} ({why})", self.label()),
            None => self.label(),
        };
        Err(StoreError::NotOwned {
            id: id.clone(),
            searched: vec![searched],
        })
    }

    /// ⚠ A full scan of every fl issue (spec §2.5): correct, and costly — one
    /// list per lookup. The search API is not used: its index lags and it
    /// promises no complete result.
    fn alias_owner(&self, alias: &Iri) -> Result<Option<u64>, StoreError> {
        let mut found = Vec::new();
        for kind in ItemKind::ALL {
            // ⚠ `remember: false` — see `list`'s doc comment. This scan is
            // not the read the caller asked for; it must not overwrite what
            // `seen` holds for an item it merely passes over.
            for (issue, meta, _) in self.list(kind, None, false)? {
                if meta.also_known_as.contains(alias) {
                    found.push(issue.number);
                }
            }
        }
        match found.as_slice() {
            [] => Ok(None),
            [n] => Ok(Some(*n)),
            many => Err(backend(format!(
                "{alias} is an alias of more than one issue ({many:?}); refusing to pick one. \
                 Name the issue by its own URL"
            ))),
        }
    }

    fn fetch(&self, n: u64) -> Result<Fetched, StoreError> {
        let reply = self
            .client
            .send(Method::Get, &self.path(&format!("/issues/{n}")), None)?;
        match reply.status {
            200 => Ok(Fetched::Found(IssueView::from_json(&reply.body)?)),
            404 => Ok(Fetched::Absent),
            410 => Ok(Fetched::Gone),
            301 | 302 | 307 | 308 => Ok(Fetched::Moved(
                reply
                    .location
                    .unwrap_or_else(|| "an unknown location".into()),
            )),
            s => Err(backend(format!(
                "GitHub answered {s} when fl read issue {n} of {}; retry",
                self.repo.full_name
            ))),
        }
    }

    /// The fl item `id` names, when it is of kind `want`.
    ///
    /// `remember`: whether this read updates `seen` (the same rule `list`
    /// already follows). Only a
    /// read the CALLER directly asked for and directly receives —
    /// `get_record`, `get_finding` — may do that. `add_finding`'s own read
    /// of the record it points at is a validity check, not a hand-off: the
    /// caller never sees that record, so remembering it here would let
    /// `add_finding` silently move the record's conflict baseline to
    /// whatever GitHub says right now, without the caller ever having asked
    /// to look at it — exactly what let `set_record_state`, called right
    /// after, overwrite a retitle made by someone else in between.
    fn item(&self, id: &Iri, want: ItemKind, remember: bool) -> Result<Found, StoreError> {
        let n = self.locate(id)?;
        match self.fetch(n)? {
            Fetched::Absent => Ok(Found::Absent),
            Fetched::Gone => Err(StoreError::Deleted(id.clone())),
            Fetched::Moved(to) => Err(StoreError::Moved { id: id.clone(), to }),
            Fetched::Found(issue) => match meta::read_item(&issue)? {
                Read::NotFl(what) => Err(StoreError::NotAnFlItem {
                    id: id.clone(),
                    what,
                }),
                Read::Item { kind, meta, prose } => {
                    self.kinds.borrow_mut().insert(n, kind);
                    if remember {
                        self.remember(n, &meta, &prose, &issue.title);
                    }
                    Ok(if kind == want {
                        Found::Item(issue, meta, prose)
                    } else {
                        Found::OtherKind(kind)
                    })
                }
            },
        }
    }

    /// Every fl item of `kind`, optionally in one state. ⚠ An issue carrying
    /// the kind's label that does not read as that kind is diverged, and the
    /// list fails — it is never dropped (spec §3.4, §5).
    ///
    /// `remember`: whether a returned item updates `seen`. Only a read the
    /// CALLER directly asked for and directly
    /// receives — `list_records`, `list_findings` — may do that. An
    /// internal scan made in service of resolving something else (an alias,
    /// a withdrawal count) must not: it would silently refresh `seen` for
    /// items the caller never saw, which is exactly what let a stale write
    /// made *through an alias* sail past the conflict check and overwrite
    /// another actor's withdrawal.
    fn list(
        &self,
        kind: ItemKind,
        state: Option<&str>,
        remember: bool,
    ) -> Result<Vec<(IssueView, Meta, String)>, StoreError> {
        // ⚠ One label, never two. Measured live on 2026-10-05: GraphQL's
        // `labels` filter is OR, the union (`fl:record` 73 issues,
        // `fl:finding` 70, both 143), so two labels would widen the list,
        // never narrow it. A state label names its kind; the kind label is
        // checked from the read.
        let label = match state {
            Some(s) => meta::state_label(kind, s),
            None => meta::kind_label(kind),
        };
        // Read once. ⚠ Modelled: GitHub's cursor names the last issue
        // served, not an offset, so an issue leaving the filtered set
        // mid-read (a label removed) cannot shift a live item across a page
        // boundary, and the REST list's second read and comparison are not
        // needed. A reading of the cursor model GitHub's GraphQL follows,
        // not of GitHub's own docs; unmeasured; no live test checks it yet.
        // Oldest first, so an issue created mid-read lands on the last page.
        let mut raw = Vec::new();
        self.each_issue(Some(&label), Order::OldestFirst, LIST_PAGE, |node| {
            raw.push(IssueView::from_graphql(node)?);
            Ok(true)
        })?;
        let mut out = Vec::new();
        let mut numbers = BTreeSet::new();
        for issue in raw {
            // An issue served twice is counted once.
            if !numbers.insert(issue.number) {
                continue;
            }
            match meta::read_item(&issue)? {
                Read::Item {
                    kind: k,
                    meta,
                    prose,
                } if k == kind => {
                    // The block is checked too, so a looser filter cannot
                    // widen the list.
                    if state.is_some_and(|st| meta.state != st) {
                        continue;
                    }
                    self.kinds.borrow_mut().insert(issue.number, k);
                    if remember {
                        self.remember(issue.number, &meta, &prose, &issue.title);
                    }
                    out.push((issue, meta, prose));
                }
                Read::Item { .. } => {
                    return Err(StoreError::Diverged {
                        id: issue.url.clone(),
                        detail: "its kind label and its block disagree".into(),
                    });
                }
                Read::NotFl(what) => {
                    return Err(StoreError::Diverged {
                        id: issue.url.clone(),
                        detail: format!("it carries an fl label but is {what}"),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Create every fl label that is missing, explicitly — never as a side
    /// effect of an issue write (spec §3.3). Once per process.
    fn ensure_labels(&self) -> Result<(), StoreError> {
        if self.labels_ready.get() {
            return Ok(());
        }
        let have: BTreeSet<String> = self
            .client
            .get_all(&self.path("/labels?per_page=100"))?
            .iter()
            .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
            .collect();
        for name in meta::all_labels() {
            if have.contains(&name) {
                continue;
            }
            let body = json!({"name": name, "color": "5319e7", "description": "managed by fl"});
            let r = self
                .client
                .send(Method::Post, &self.path("/labels"), Some(&body))?;
            if r.status != 201 {
                return Err(backend(format!(
                    "GitHub answered {} when fl created the label `{name}`; retry",
                    r.status
                )));
            }
        }
        self.labels_ready.set(true);
        Ok(())
    }

    fn create(
        &self,
        kind: ItemKind,
        title: &str,
        prose: &str,
        meta: &Meta,
    ) -> Result<IssueView, StoreError> {
        if title.chars().count() > TITLE_MAX {
            return Err(backend(format!(
                "a title of {} characters is longer than GitHub's limit of {TITLE_MAX}; \
                 shorten it",
                title.chars().count()
            )));
        }
        // ⚠ Before anything is sent: GitHub may trim
        // a title, and the check that the create came back as sent would
        // then fail AFTER the issue exists — a landed create reported as an
        // error. (GitHub's trimming is unmeasured; refusing costs nothing.)
        if title.trim() != title {
            return Err(backend(format!(
                "the title {title:?} starts or ends with whitespace, which GitHub may trim, so \
                 fl could not confirm the issue it creates. Remove the leading and trailing \
                 whitespace"
            )));
        }
        let (state, _) = meta::projection(kind, &meta.state);
        if state != "open" {
            return Err(backend(format!(
                "fl creates items open, and `{}` is a closed state",
                meta.state
            )));
        }
        self.ensure_labels()?;
        let labels = vec![meta::kind_label(kind), meta::state_label(kind, &meta.state)];
        let body = meta::render_body(prose, meta);
        // ⚠ No labels in the create: they are added afterward, by their own
        // call (`label_created`). Measured live on 2026-10-05: labels set in the
        // create showed their `labeled` events 28-88 s late (once more than
        // 180 s), past `await_create_events`, so they landed in the next
        // update's window as a spurious conflict; labels added by their own
        // call showed in 1-2 s (3 of 3), with nothing more after 180 s.
        let sent = json!({"title": title, "body": body});
        let path = self.path("/issues");
        // Before the first send: the create-key search reads back to here.
        let started = now_millis();
        // ⚠ `send_unchecked_json`, not `send`: a 201 whose own body cannot be
        // read is exactly as ambiguous as a 5xx or a dropped connection
        // (spec §3.3) — the write may have landed regardless of whether fl
        // could read GitHub's answer to it. The strict `send` would report
        // that case as a plain `Backend` error, ending the call as though
        // nothing happened, when a duplicate may be one retry away.
        let issue = match self
            .client
            .send_unchecked_json(Method::Post, &path, Some(&sent))
        {
            // ⚠ ANY 2xx PROVES the create landed (not only 201, and a body
            // that broke off counts as unreadable): an unreadable body is
            // never followed by a
            // resend, only a search — resending here risks making exactly
            // the duplicate this whole mechanism exists to avoid.
            Ok(r) if (200..300).contains(&r.status) => match IssueView::from_json(&r.body) {
                Ok(issue) => issue,
                Err(_) => self.after_unreadable_create(title, meta, started, r.status)?,
            },
            // ⚠ An ambiguous failure may already have created the issue.
            // Look for the create key before sending again (spec §3.3).
            Ok(r) if r.status >= 500 => {
                self.after_ambiguous_create(title, meta, started, &path, &sent)?
            }
            Err(StoreError::Unreachable { .. }) => {
                self.after_ambiguous_create(title, meta, started, &path, &sent)?
            }
            Ok(r) => {
                return Err(backend(format!(
                    "GitHub answered {} to an issue create",
                    r.status
                )));
            }
            Err(e) => return Err(e),
        };
        // ⚠ Not `check_written`: no labels were sent, so a mismatch here is
        // not a label permission, and the issue exists without labels.
        let problems = written_problems(&issue, title, None, &body, "open", None);
        if !problems.is_empty() {
            return Err(unlabelled_issue(
                &issue,
                &format!("GitHub did not create it as sent: {}", problems.join("; ")),
                true,
            ));
        }
        let issue = self.label_created(issue, &labels)?;
        self.await_create_events(issue.number, &labels);
        self.kinds.borrow_mut().insert(issue.number, kind);
        self.remember(issue.number, meta, prose, title);
        Ok(issue)
    }

    /// Add fl's `labels` to an issue fl just created without them, and check
    /// GitHub's answer. An issue that already carries exactly them (found
    /// by its create key, labelled before) is left as it is.
    /// ⚠ Every error is `unlabelled_issue`: it names the issue, which
    /// exists by now, and the repair — never a retry.
    fn label_created(&self, issue: IssueView, labels: &[String]) -> Result<IssueView, StoreError> {
        let fl_labels = |all: &[String]| -> BTreeSet<String> {
            all.iter()
                .filter(|l| l.starts_with("fl:"))
                .cloned()
                .collect()
        };
        let want: BTreeSet<String> = labels.iter().cloned().collect();
        if fl_labels(&issue.labels) == want {
            return Ok(issue);
        }
        let unlabelled = |why: String| unlabelled_issue(&issue, &why, false);
        // Both labels in one call. (Measured live on 2026-10-05: GitHub
        // sometimes records a `labeled` event twice, one label per call or
        // two; `changes` drops the copy wherever it lands.)
        let r = self
            .client
            .send(
                Method::Post,
                &self.path(&format!("/issues/{}/labels", issue.number)),
                Some(&json!({ "labels": labels })),
            )
            .map_err(|e| unlabelled(format!("adding its labels failed ({e})")))?;
        if r.status != 200 {
            return Err(unlabelled(format!(
                "GitHub answered {} when fl added its labels",
                r.status
            )));
        }
        let got: Vec<String> = r
            .body
            .as_array()
            .and_then(|a| {
                a.iter()
                    .map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .ok_or_else(|| {
                unlabelled("GitHub's answer to adding its labels could not be read".into())
            })?;
        let applied = fl_labels(&got);
        let missing: Vec<&String> = want.difference(&applied).collect();
        if !missing.is_empty() {
            return Err(unlabelled(format!(
                "GitHub accepted its labels but did not apply {missing:?}. The credential may \
                 lack permission to set labels (Issues: read and write)"
            )));
        }
        // ⚠ Every label fl sent was applied, so not a permission: adding
        // labels never removes one, and the issue already carried another
        // fl label (an automation, or a person, labelled it first).
        let extra: Vec<&String> = applied.difference(&want).collect();
        if !extra.is_empty() {
            return Err(backend(format!(
                "{} was created and given fl's labels, but it also carries {extra:?}, which fl \
                 did not set and adding labels does not remove, so it reads as diverged. Do not \
                 create it again: `fl github repair {} --by <name>` rewrites its fl labels from \
                 its block",
                issue.url, issue.number
            )));
        }
        Ok(IssueView {
            labels: got,
            ..issue
        })
    }

    /// Search for a create by its key, `settle` apart, up to three times.
    /// `Ok(None)` when none of the three searches found it. `started`: when
    /// the attempt began, by this machine's clock, in milliseconds.
    fn search_by_create_key(
        &self,
        key: &str,
        started: u64,
    ) -> Result<Option<IssueView>, StoreError> {
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(self.settle);
            }
            if let Some(found) = self.find_by_create_key(key, started)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// ⚠ The list GitHub serves may lag a create that just landed (GraphQL's
    /// showed one within 1 s, 5 of 5, measured live on 2026-10-05; the REST
    /// list, which fl does not use, took 31-93 s), so the key is searched
    /// for `settle` apart; only when EVERY search misses is the create sent
    /// again. Only for a FIRST-attempt failure where the create
    /// may not have happened at all — a 5xx answer, or the connection
    /// dropping before an answer arrived. See `after_unreadable_create` for
    /// the case where it certainly did.
    fn after_ambiguous_create(
        &self,
        title: &str,
        meta: &Meta,
        started: u64,
        path: &str,
        sent: &Value,
    ) -> Result<IssueView, StoreError> {
        let searched = self
            .search_by_create_key(&meta.create_key, started)
            .map_err(|e| {
                after_ambiguous_failure(title, "searching for it by its create key failed", e)
            })?;
        if let Some(found) = searched {
            return Ok(found);
        }
        // ⚠ `send_unchecked_json`, not `send`: the
        // RULE applies to this resend exactly as it does to the first
        // attempt in `create` — once GitHub answers 2xx here, the create
        // has landed for certain, and an unreadable or non-issue body must
        // route to `after_unreadable_create` (search only, never a third
        // send), not surface as a plain parse error with no retry advice.
        match self
            .client
            .send_unchecked_json(Method::Post, path, Some(sent))
        {
            Ok(r) if (200..300).contains(&r.status) => match IssueView::from_json(&r.body) {
                Ok(issue) => Ok(issue),
                Err(_) => self.after_unreadable_create(title, meta, started, r.status),
            },
            Ok(r) => Err(backend(format!(
                "GitHub failed an issue create twice (the second answer was {}). {}",
                r.status,
                look_before_retrying(title)
            ))),
            // ⚠ Every failure of the resend gets the SAME advice as a bad
            // status: the first attempt's fate is
            // unknown, so whatever stopped this one — a dropped connection,
            // a rate limit, a refused credential — fl cannot say whether the
            // issue exists. Only a fresh look can settle that.
            Err(e) => Err(after_ambiguous_failure(
                title,
                "sending it a second time failed",
                e,
            )),
        }
    }

    /// ⚠ A 2xx proves the create landed: unlike a 5xx
    /// answer or a dropped connection, there is no "may not have happened"
    /// here. A miss on every search is never followed by a resend — that
    /// would risk making exactly the duplicate this whole path exists to
    /// avoid. The caller is told where to look for the issue itself.
    fn after_unreadable_create(
        &self,
        title: &str,
        meta: &Meta,
        started: u64,
        status: u16,
    ) -> Result<IssueView, StoreError> {
        // ⚠ The create is certain here — GitHub already answered 2xx — so
        // the search's own failure (not just a miss) carries the same
        // advice too: the client's generic "…;
        // retry" on a failed page read would otherwise reach the caller
        // with no hint that a resend is exactly what must NOT happen.
        let found = self
            .search_by_create_key(&meta.create_key, started)
            .map_err(|e| {
                backend(format!(
                    "GitHub answered {status} to an issue create, but its own body could not \
                     be read, and searching for it afterward by its create key failed too ({e}). \
                     {}",
                    look_before_retrying(title)
                ))
            })?;
        found.ok_or_else(|| {
            backend(format!(
                "GitHub answered {status} to an issue create, but its own body could not be \
                 read, and the issue could not be found afterward by its create key \
                 either. {}",
                look_before_retrying(title)
            ))
        })
    }

    /// The issue whose block carries create key `key`, among the issues
    /// created since `CREATE_SEARCH_MARGIN` before `started`.
    /// ⚠ Every issue, not only fl's labelled ones: a create sends no labels
    /// and adds them afterward (`label_created`), so an issue this attempt
    /// made may carry none — after a stop between the two calls, or a
    /// create whose answer was lost. Newest first, and it
    /// stops at the first issue older than the margin, so its cost does not
    /// grow with the repository's history.
    fn find_by_create_key(&self, key: &str, started: u64) -> Result<Option<IssueView>, StoreError> {
        let since =
            At::from_unix_millis(started.saturating_sub(CREATE_SEARCH_MARGIN.as_millis() as u64));
        let mut found = None;
        self.each_issue(None, Order::NewestFirst, SEARCH_PAGE, |node| {
            if created_at(node)? < since {
                return Ok(false);
            }
            let issue = IssueView::from_graphql(node)?;
            if let Ok((_, m)) = meta::parse_body(&issue.body)
                && m.create_key == key
            {
                found = Some(issue);
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(found)
    }

    /// Read, change, write, and check the answer. `missing` is the error for
    /// an issue that does not exist — `Fn`, not `FnOnce`: the issue is read
    /// twice, and one that vanishes between the reads is missing too.
    fn update(
        &self,
        n: u64,
        kind: ItemKind,
        missing: impl Fn() -> StoreError,
        change: impl FnOnce(&mut Meta, &mut String, &mut String) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        self.ensure_labels()?;
        let id = self.issue_url(n);
        // Classify first, so a missing, deleted or moved issue keeps its
        // outcome — its timeline would answer 404, 410 or 301 instead.
        match self.fetch(n)? {
            // Before the window: GitHub's edit history does not answer for
            // a pull request's number.
            Fetched::Found(i) if i.is_pull_request => {
                return Err(StoreError::NotAnFlItem {
                    id,
                    what: "a pull request".into(),
                });
            }
            Fetched::Found(_) => {}
            Fetched::Absent => return Err(missing()),
            Fetched::Gone => return Err(StoreError::Deleted(id)),
            Fetched::Moved(to) => return Err(StoreError::Moved { id, to }),
        }
        let before = self.window(n)?;
        // Read again inside the window: the state fl changes is the state
        // the window starts from, so a write landing between the first read
        // and the window is inside it.
        let issue = match self.fetch(n)? {
            Fetched::Found(i) => i,
            Fetched::Absent => return Err(missing()),
            Fetched::Gone => return Err(StoreError::Deleted(id)),
            Fetched::Moved(to) => return Err(StoreError::Moved { id, to }),
        };
        let Read::Item {
            kind: found,
            mut meta,
            mut prose,
        } = meta::read_item(&issue)?
        else {
            return Err(StoreError::NotAnFlItem {
                id,
                what: "not an fl item".into(),
            });
        };
        if found != kind {
            return Err(StoreError::WrongKind {
                id,
                expected: kind.as_kind(),
                found: found.as_kind(),
            });
        }
        // ⚠ The caller changed what it READ. If the item moved on since
        // then, writing the caller's fields over it would lose the other
        // change silently (spec §3.3).
        if let Some((m, p, t)) = self.seen.borrow().get(&n)
            && (m != &meta || p != &prose || t != &issue.title)
        {
            return Err(StoreError::Conflict {
                id,
                detail: "it changed after fl read it and before fl wrote it".into(),
            });
        }
        // A reference written under an old name is rewritten on the next
        // write (spec §2.4).
        if let Some(r) = meta.record.as_mut() {
            let current = self.current_ref(r)?;
            r.id = current;
        }
        let mut title = issue.title.clone();
        change(&mut meta, &mut prose, &mut title)?;
        let labels = meta::labels_after(&issue.labels, kind, &meta.state);
        let (state, reason) = meta::projection(kind, &meta.state);
        let body = meta::render_body(&prose, &meta);
        let same_labels =
            labels.iter().collect::<BTreeSet<_>>() == issue.labels.iter().collect::<BTreeSet<_>>();
        if same_labels
            && body == issue.body.replace("\r\n", "\n")
            && title == issue.title
            && state == issue.state
        {
            return Ok(()); // nothing to write, and nothing to record as an edit
        }
        let mut sent = json!({"title": title, "body": body, "labels": labels, "state": state});
        if let Some(r) = reason {
            sent["state_reason"] = json!(r);
        }
        let r = self.client.send(
            Method::Patch,
            &self.path(&format!("/issues/{n}")),
            Some(&sent),
        )?;
        if r.status != 200 {
            return Err(backend(format!(
                "GitHub answered {} when fl wrote {id}; read it again before retrying",
                r.status
            )));
        }
        let back = IssueView::from_json(&r.body)?;
        check_written(&back, &title, &labels, &body, state, reason)?;
        // ⚠ Measured live: the timeline and the edit history lag a write, so
        // `after` is read until it shows fl's own write (`window_after`).
        let after = self.window_after(&id, n, &before, &issue, &back)?;
        // ⚠ Before `remember`: a write that crossed someone else's must not
        // become the baseline the next write is compared with.
        self.check_window(&id, &before, &after, &issue, &back)?;
        self.remember(n, &meta, &prose, &title);
        Ok(())
    }

    /// The issue's state-changing timeline events and its body edit history,
    /// now. ⚠ Never `remember`s: it reads no item.
    fn window(&self, n: u64) -> Result<Window, StoreError> {
        let events = self.state_events(n)?;
        let (edits, edits_total) = self.edit_history(n)?;
        Ok(Window {
            events,
            edits,
            edits_total,
        })
    }

    /// The issue's state-changing timeline events, by id. ⚠ Never
    /// `remember`s: it reads no item.
    fn state_events(&self, n: u64) -> Result<Vec<Event>, StoreError> {
        let mut events = Vec::new();
        for e in self
            .client
            .get_all(&self.path(&format!("/issues/{n}/timeline?per_page=100")))?
        {
            // ⚠ No `event` field is not "not a state event": fl cannot tell
            // what it was, so it cannot rule it out.
            let kind = e.get("event").and_then(Value::as_str).ok_or_else(|| {
                backend(format!(
                    "GitHub sent a timeline item on issue {n} without an `event` kind, so fl \
                     cannot tell whether someone else changed the issue; retry"
                ))
            })?;
            if !STATE_EVENTS.contains(&kind) {
                continue;
            }
            let id = e.get("id").and_then(Value::as_u64).ok_or_else(|| {
                backend(format!(
                    "a `{kind}` event on issue {n} has no id, so fl cannot tell it from its own \
                     write; retry"
                ))
            })?;
            // ⚠ A label event without its label cannot be told from noise.
            let label = match kind {
                "labeled" | "unlabeled" => Some(
                    e.pointer("/label/name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| {
                            backend(format!(
                                "a `{kind}` event on issue {n} names no label, so fl cannot \
                                 tell whether it changed the issue; retry"
                            ))
                        })?,
                ),
                _ => None,
            };
            events.push(Event {
                id,
                kind: kind.to_string(),
                label,
            });
        }
        Ok(events)
    }

    /// The body's edit history: its entries' ids and its `totalCount`.
    /// ⚠ Never `remember`s: it reads no item.
    fn edit_history(&self, n: u64) -> Result<(BTreeSet<String>, u64), StoreError> {
        let (owner, name) = self
            .repo
            .full_name
            .split_once('/')
            .expect("a full name is owner/name");
        let data = self.client.graphql(
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, \
             name: $name) { issue(number: $number) { userContentEdits(last: 100) { totalCount \
             nodes { id } } } } }",
            json!({"owner": owner, "name": name, "number": n}),
        )?;
        // `issue: null` (with a NOT_FOUND error) is an answer: GitHub's
        // GraphQL finds no issue at that number. Say what it is instead.
        if data
            .pointer("/repository/issue")
            .is_some_and(Value::is_null)
        {
            return Err(self.not_in_graphql(n)?);
        }
        let history = data
            .pointer("/repository/issue/userContentEdits")
            .ok_or_else(|| {
                backend(format!(
                    "GitHub's edit history for issue {n} came back without \
                     `userContentEdits`; retry"
                ))
            })?;
        let (Some(nodes), Some(edits_total)) = (
            history.get("nodes").and_then(Value::as_array),
            history.get("totalCount").and_then(Value::as_u64),
        ) else {
            return Err(backend(format!(
                "GitHub's edit history for issue {n} came back without `nodes` or \
                 `totalCount`; retry"
            )));
        };
        // ⚠ A node without an id is not "no edit": dropping it would count
        // one edit short, and could hide someone else's.
        let edits =
            nodes
                .iter()
                .map(|x| {
                    x.get("id").and_then(Value::as_str).map(str::to_string).ok_or_else(|| {
                    backend(format!(
                        "GitHub's edit history for issue {n} has an entry without an id, so \
                         fl cannot count the edits; retry"
                    ))
                })
                })
                .collect::<Result<_, _>>()?;
        Ok((edits, edits_total))
    }

    /// Why GitHub's GraphQL finds no issue `n`, which its REST API served a
    /// moment ago: read it again and name what it is now.
    fn not_in_graphql(&self, n: u64) -> Result<StoreError, StoreError> {
        let id = self.issue_url(n);
        Ok(match self.fetch(n)? {
            Fetched::Found(i) if i.is_pull_request => StoreError::NotAnFlItem {
                id,
                what: "a pull request".into(),
            },
            Fetched::Gone => StoreError::Deleted(id),
            Fetched::Moved(to) => StoreError::Moved { id, to },
            Fetched::Absent => backend(format!(
                "issue {n} was there a moment ago, and GitHub now answers that it does not \
                 exist; read it again before retrying"
            )),
            Fetched::Found(_) => backend(format!(
                "GitHub's edit history does not find issue {n}, which its REST API still \
                 serves; retry"
            )),
        })
    }

    /// ⚠ Detection, not prevention (spec §3.3): GitHub has no conditional
    /// update. Every state-changing event or body edit between `before` and
    /// `after` that fl's own write does not explain is someone else's.
    fn check_window(
        &self,
        id: &Iri,
        before: &Window,
        after: &Window,
        old: &IssueView,
        new: &IssueView,
    ) -> Result<(), StoreError> {
        let Own {
            events: mut expected,
            edits: own_edits,
        } = own_write(before, old, new);
        let mut foreign = Vec::new();
        for e in new_changes(before, after) {
            let kind = &e.kind;
            match expected.get_mut(kind.as_str()) {
                Some(left) if *left > 0 => *left -= 1,
                _ => foreign.push(format!("a `{kind}` event")),
            }
        }
        let Some(new_edits) = new_edits(before, after) else {
            return Err(StoreError::Conflict {
                id: id.clone(),
                detail: "GitHub shows changes fl did not make: an entry was deleted from the \
                         body's edit history"
                    .into(),
            });
        };
        if new_edits > own_edits {
            foreign.push(format!("{} body edit(s)", new_edits - own_edits));
        }
        if foreign.is_empty() {
            Ok(())
        } else {
            Err(StoreError::Conflict {
                id: id.clone(),
                detail: format!(
                    "GitHub shows changes fl did not make: {}",
                    foreign.join(", ")
                ),
            })
        }
    }

    /// The window after fl's own write, read again until it SHOWS that
    /// write. ⚠ Measured live: GitHub's timeline and edit history lag a
    /// write. Taken too early, `after` lacks fl's own events, and they land
    /// in the NEXT write's window, where they read as someone else's.
    /// ⚠ Modelled: once fl's own events show, anything written before them
    /// shows too — unmeasured; no live test checks it yet.
    fn window_after(
        &self,
        id: &Iri,
        n: u64,
        before: &Window,
        old: &IssueView,
        new: &IssueView,
    ) -> Result<Window, StoreError> {
        let own = own_write(before, old, new);
        let start = std::time::Instant::now();
        loop {
            let after = self.window(n)?;
            if shows(before, &after, &own) {
                return Ok(after);
            }
            if start.elapsed() >= self.visible_within {
                return Err(backend(format!(
                    "GitHub accepted fl's write to {id} but has not shown it in the issue's \
                     timeline and edit history after {} s, so fl cannot tell whether someone \
                     else wrote at the same time; read it again before writing",
                    self.visible_within.as_secs_f32()
                )));
            }
            std::thread::sleep(self.poll);
        }
    }

    /// After a create: wait until the timeline shows the `labeled` events
    /// of the labels `label_created` added, so they do not land in the
    /// next write's window. ⚠ Measured live on 2026-10-05: they showed
    /// 1-2 s after the call that added them (3 of 3). A create whose events
    /// never show still succeeds — the create landed, and the next write
    /// refuses as a conflict, which is the safe side.
    /// ⚠ Never an error: the issue exists by now, and `add_record` mints a
    /// new create key on every call, so an error here would invite a retry
    /// that makes a DUPLICATE. A failed read ends the wait like a timeout.
    /// ⚠ Waits for the labels themselves, as the timeline's changes add
    /// them (`changes`): a `labeled` event that is noise never ends the wait.
    fn await_create_events(&self, n: u64, labels: &[String]) {
        let start = std::time::Instant::now();
        loop {
            let Ok(events) = self.state_events(n) else {
                return;
            };
            let mut on = BTreeSet::new();
            for e in changes(&events) {
                if let Some(l) = &e.label {
                    if e.kind == "labeled" {
                        on.insert(l.as_str());
                    } else {
                        on.remove(l.as_str());
                    }
                }
            }
            let shown = labels.iter().all(|l| on.contains(l.as_str()));
            if shown || start.elapsed() >= self.visible_within {
                return;
            }
            std::thread::sleep(self.poll);
        }
    }

    /// `fl github repair` (spec §3.4): rewrite the fl labels and the
    /// open/closed status FROM the block, and leave a comment naming who ran
    /// it. The block is fl's record of the protocol, so a repair never moves
    /// an item to a state the protocol did not reach. ⚠ Never `remember`s:
    /// the caller receives no item from it.
    pub fn repair(&self, id: &Iri, by: &str) -> Result<Repaired, StoreError> {
        // ⚠ Explicitly, as every write does: a deleted fl label is a common
        // reason to repair, and the PATCH must not recreate it as a side
        // effect (spec §3.3).
        self.ensure_labels()?;
        let n = self.locate(id)?;
        let classify = |fetched: Fetched| match fetched {
            // Before the window: GitHub's edit history does not answer for
            // a pull request's number.
            Fetched::Found(i) if i.is_pull_request => Err(StoreError::NotAnFlItem {
                id: id.clone(),
                what: "a pull request".into(),
            }),
            Fetched::Found(i) => Ok(i),
            Fetched::Absent => Err(StoreError::NotAnFlItem {
                id: id.clone(),
                what: "an issue that does not exist".into(),
            }),
            Fetched::Gone => Err(StoreError::Deleted(id.clone())),
            Fetched::Moved(to) => Err(StoreError::Moved { id: id.clone(), to }),
        };
        // Classify first, so a missing, deleted or moved issue keeps its
        // outcome; then open the window and read again inside it.
        classify(self.fetch(n)?)?;
        let before = self.window(n)?;
        let issue = classify(self.fetch(n)?)?;
        // ⚠ Not `Diverged`: its message says to run `fl github repair`,
        // which is the command refusing here.
        let restore = |detail: String| {
            backend(format!(
                "{id} cannot be repaired: {detail}. A repair rewrites from the block, so \
                 restore the block from the issue's edit history first — or, if the issue was \
                 never fl's, remove its fl labels instead of repairing it"
            ))
        };
        let (_, meta) =
            meta::parse_body(&issue.body).map_err(|e| restore(format!("its body {e}")))?;
        if !meta.kind.valid_state(&meta.state) {
            return Err(restore(format!(
                "the block's state `{}` is not valid",
                meta.state
            )));
        }
        if matches!(meta::read_item(&issue), Ok(Read::Item { .. })) {
            return Ok(Repaired {
                number: n,
                state: meta.state,
                changed: false,
            });
        }
        let labels = meta::labels_after(&issue.labels, meta.kind, &meta.state);
        let (state, reason) = meta::projection(meta.kind, &meta.state);
        let mut sent = json!({"labels": labels, "state": state});
        if let Some(r) = reason {
            sent["state_reason"] = json!(r);
        }
        // ⚠ `send_unchecked_json`, not `send`: a 2xx whose body cannot be
        // read still proves the repair landed, so it must reach the comment
        // below rather than end the call first.
        let r = self.client.send_unchecked_json(
            Method::Patch,
            &self.path(&format!("/issues/{n}")),
            Some(&sent),
        )?;
        if !(200..300).contains(&r.status) {
            return Err(backend(format!(
                "GitHub answered {} to the repair of {id}; read it again before retrying",
                r.status
            )));
        }
        // ⚠ The PATCH answered 2xx, so the repair has landed, at least in
        // part: the comment naming who ran it is posted NOW, before anything
        // that could fail — reading the answer, checking it, the window. A
        // rerun after such a failure would find the issue consistent and
        // post nothing, losing the record.
        let note = json!({"body": format!(
            "`fl github repair`: the fl labels and the open/closed status were rewritten from \
             fl's record (state `{}`) by {by}.",
            meta.state
        )});
        let comment_failed = match self.client.send(
            Method::Post,
            &self.path(&format!("/issues/{n}/comments")),
            Some(&note),
        ) {
            Ok(c) if c.status == 201 => None,
            Ok(c) => Some(format!("GitHub answered {}", c.status)),
            Err(e) => Some(e.to_string()),
        };
        let checked = IssueView::from_json(&r.body).and_then(|back| {
            check_written(
                &back,
                &issue.title,
                &labels,
                &issue.body.replace("\r\n", "\n"),
                state,
                reason,
            )?;
            let after = self.window_after(id, n, &before, &issue, &back)?;
            self.check_window(id, &before, &after, &issue, &back)
        });
        let comment = match &comment_failed {
            None => format!("the comment recording that {by} ran the repair was posted"),
            Some(why) => format!(
                "the comment recording that {by} ran the repair was not posted ({why}) — add \
                 that comment by hand"
            ),
        };
        match (checked, comment_failed) {
            (Ok(()), None) => {}
            (Ok(()), Some(_)) => {
                return Err(backend(format!(
                    "the repair of {id} was written, but {comment}"
                )));
            }
            (Err(StoreError::Conflict { id, detail }), _) => {
                return Err(StoreError::Conflict {
                    id,
                    detail: format!("{detail}; {comment}"),
                });
            }
            (Err(e), _) => {
                return Err(backend(format!(
                    "GitHub answered {} to the repair of {id}, but then: {e}; {comment}",
                    r.status
                )));
            }
        }
        Ok(Repaired {
            number: n,
            state: meta.state,
            changed: true,
        })
    }

    fn record_from(&self, issue: &IssueView, meta: &Meta) -> Result<Record, StoreError> {
        let state = State::from_wire(&meta.state).ok_or_else(|| StoreError::Diverged {
            id: issue.url.clone(),
            detail: format!("the block's state `{}` is not a record state", meta.state),
        })?;
        Ok(Record {
            id: RecordId(issue.url.clone()),
            project: meta.project.clone(),
            title: issue.title.clone(),
            state,
            also_known_as: meta.also_known_as.clone(),
        })
    }

    fn finding_from(
        &self,
        issue: &IssueView,
        meta: &Meta,
        prose: &str,
    ) -> Result<Finding, StoreError> {
        let diverged = |detail: &str| StoreError::Diverged {
            id: issue.url.clone(),
            detail: detail.to_string(),
        };
        let state = FindingState::from_wire(&meta.state)
            .ok_or_else(|| diverged("the block's state is not a finding state"))?;
        let record = meta
            .record
            .as_ref()
            .ok_or_else(|| diverged("the block names no record"))?;
        let raised_by = meta
            .raised_by
            .clone()
            .ok_or_else(|| diverged("the block names no raiser"))?;
        Ok(Finding {
            id: FindingId(issue.url.clone()),
            project: meta.project.clone(),
            record: RecordId(self.current_ref(record)?),
            raised_by,
            claim: prose.to_string(),
            reproduction: meta.reproduction.clone(),
            state,
            assigned_to: meta.assigned_to.clone(),
            withdrawn_reason: meta.withdrawn_reason.clone(),
            also_known_as: meta.also_known_as.clone(),
            security: meta.security,
        })
    }

    /// A finding's record reference (spec §2.3). A URL under this
    /// repository's CURRENT name is trusted: the repository itself is bound
    /// by node id at open. Any other URL is resolved by the reference's node
    /// id — never by the URL, because an old name may now reach another
    /// repository.
    fn current_ref(&self, r: &RecordRef) -> Result<Iri, StoreError> {
        if let Some((name, _)) = meta::parse_issue_url(&r.id)
            && name.eq_ignore_ascii_case(&self.repo.full_name)
        {
            return Ok(r.id.clone());
        }
        let data = self.client.graphql(
            "query($id: ID!) { node(id: $id) { ... on Issue { url repository { id } } } }",
            json!({ "id": r.node_id }),
        )?;
        let node = data
            .get("node")
            .filter(|n| !n.is_null())
            .ok_or_else(|| StoreError::Deleted(r.id.clone()))?;
        let url = node
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| backend("GitHub answered a node lookup without `url`".into()))?;
        if node.pointer("/repository/id").and_then(Value::as_str)
            != Some(self.repo.node_id.as_str())
        {
            return Err(StoreError::Moved {
                id: r.id.clone(),
                to: url.to_string(),
            });
        }
        Iri::parse(url).map_err(|e| backend(format!("GitHub sent an issue URL fl cannot use: {e}")))
    }

    /// Which kind issue `n` holds. `None` only when no issue `n` exists; a
    /// deleted, moved or foreign issue is an error naming what it is, never
    /// "not found" (spec §3.5, §3.6).
    fn kind_at(&self, n: u64) -> Result<Option<ItemKind>, StoreError> {
        if let Some(k) = self.kinds.borrow().get(&n) {
            return Ok(Some(*k));
        }
        let id = self.issue_url(n);
        match self.fetch(n)? {
            Fetched::Found(issue) => match meta::read_item(&issue)? {
                Read::Item { kind, .. } => {
                    self.kinds.borrow_mut().insert(n, kind);
                    Ok(Some(kind))
                }
                Read::NotFl(what) => Err(StoreError::NotAnFlItem { id, what }),
            },
            Fetched::Absent => Ok(None),
            Fetched::Gone => Err(StoreError::Deleted(id)),
            Fetched::Moved(to) => Err(StoreError::Moved { id, to }),
        }
    }

    /// Spec §6: only a `private` repository may hold a security finding.
    /// Read live, every time — visibility can change — and a failed read is
    /// an ERROR: an unknown visibility is not a pass.
    fn require_private(&self) -> Result<(), StoreError> {
        let r = self.client.send(
            Method::Get,
            &format!("/repos/{}", self.repo.full_name),
            None,
        )?;
        let refuse = |why: String| {
            backend(format!(
                "fl could not read the visibility of {} ({why}), so it will not write a \
                 security finding there. Retry, or use a local tracker",
                self.repo.full_name
            ))
        };
        if r.status != 200 {
            return Err(refuse(format!("GitHub answered {}", r.status)));
        }
        let visibility = r
            .body
            .get("visibility")
            .and_then(Value::as_str)
            .ok_or_else(|| refuse("the answer names no visibility".into()))?;
        if visibility == "private" {
            Ok(())
        } else {
            Err(StoreError::SecurityNotPrivate {
                repo: self.repo.full_name.clone(),
                visibility: visibility.to_string(),
            })
        }
    }
}

impl Tracker for GithubTracker {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        let meta = Meta::new(ItemKind::Record, State::Todo.as_wire(), project.clone());
        Ok(RecordId(
            self.create(ItemKind::Record, title, "", &meta)?.url,
        ))
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        match self.item(id.iri(), ItemKind::Record, true)? {
            Found::Item(issue, meta, _) => self.record_from(&issue, &meta).map(Some),
            Found::OtherKind(_) | Found::Absent => Ok(None),
        }
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        let mut out = Vec::new();
        for (issue, meta, _) in self.list(ItemKind::Record, None, true)? {
            if meta.project == *project {
                out.push(self.record_from(&issue, &meta)?);
            }
        }
        Ok(out)
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        let n = self.locate(id.iri())?;
        self.update(
            n,
            ItemKind::Record,
            || StoreError::NoSuchRecord(id.clone()),
            |meta, _, _| {
                meta.state = state.as_wire().to_string();
                Ok(())
            },
        )
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        // The record must be an fl record of this repository (spec §3.1).
        // ⚠ `remember: false`: this is a validity
        // check, not a read the caller receives the record from.
        let record = match self.item(finding.record.iri(), ItemKind::Record, false)? {
            Found::Item(issue, _, _) => issue,
            Found::OtherKind(k) => {
                return Err(StoreError::WrongKind {
                    id: finding.record.iri().clone(),
                    expected: Kind::Record,
                    found: k.as_kind(),
                });
            }
            Found::Absent => return Err(StoreError::NoSuchRecord(finding.record.clone())),
        };
        if finding.security {
            self.require_private()?;
        }
        let mut meta = Meta::new(
            ItemKind::Finding,
            finding.state.as_wire(),
            finding.project.clone(),
        );
        meta.record = Some(RecordRef {
            id: record.url.clone(),
            node_id: record.node_id.clone(),
        });
        meta.reproduction = finding.reproduction.clone();
        meta.raised_by = Some(finding.raised_by.clone());
        meta.assigned_to = finding.assigned_to.clone();
        meta.withdrawn_reason = finding.withdrawn_reason.clone();
        meta.security = finding.security;
        let title = meta::title_of(&finding.claim);
        Ok(FindingId(
            self.create(ItemKind::Finding, &title, &finding.claim, &meta)?
                .url,
        ))
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        match self.item(id.iri(), ItemKind::Finding, true)? {
            Found::Item(issue, meta, prose) => self.finding_from(&issue, &meta, &prose).map(Some),
            Found::OtherKind(_) | Found::Absent => Ok(None),
        }
    }

    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        let n = self.locate(finding.id.iri())?;
        self.update(
            n,
            ItemKind::Finding,
            || StoreError::NoSuchFinding(finding.id.clone()),
            |meta, prose, title| {
                meta.state = finding.state.as_wire().to_string();
                meta.reproduction = finding.reproduction.clone();
                meta.assigned_to = finding.assigned_to.clone();
                meta.withdrawn_reason = finding.withdrawn_reason.clone();
                // ⚠ Not taken from the caller: `also_known_as` (the trait's
                // contract), `security` (set at raise only, spec §6), the raiser
                // and the record.
                if *prose != finding.claim {
                    *prose = finding.claim.clone();
                    *title = meta::title_of(&finding.claim);
                }
                Ok(())
            },
        )
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        let mut out = Vec::new();
        for (issue, meta, prose) in self.list(ItemKind::Finding, None, true)? {
            if meta.project == *project {
                out.push(self.finding_from(&issue, &meta, &prose)?);
            }
        }
        Ok(out)
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        // ⚠ `remember: false` — this is a count, not a read the caller
        // receives items from; see `list`'s doc comment.
        let withdrawn = self.list(
            ItemKind::Finding,
            Some(FindingState::Withdrawn.as_wire()),
            false,
        )?;
        Ok(withdrawn
            .iter()
            .filter(|(_, m, _)| m.raised_by.as_deref() == Some(actor))
            .count() as u64)
    }

    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        // One id namespace: the alias may not name an issue here, nor be
        // another item's alias.
        if let Owner::Ours(_) = self.owner(&alias)? {
            return Err(StoreError::AlreadyExists(alias));
        }
        if self.alias_owner(&alias)?.is_some() {
            return Err(StoreError::AlreadyExists(alias));
        }
        let n = self.locate(primary)?;
        let missing = || StoreError::NotOwned {
            id: primary.clone(),
            searched: vec![format!("{} (no issue {n})", self.label())],
        };
        let kind = self.kind_at(n)?.ok_or_else(missing)?;
        self.update(n, kind, missing, |meta, _, _| {
            meta.also_known_as.push(alias.clone());
            Ok(())
        })
    }
}

impl Handles for GithubTracker {
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError> {
        let Some(want) = ItemKind::from_kind(kind) else {
            return Ok(None);
        };
        // An alias has no handle: only an issue's own URL does.
        let Owner::Ours(n) = self.owner(id)? else {
            return Ok(None);
        };
        Ok((self.kind_at(n)? == Some(want)).then_some(n))
    }

    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError> {
        let Some(want) = ItemKind::from_kind(kind) else {
            return Ok(None);
        };
        Ok((self.kind_at(handle)? == Some(want)).then(|| self.issue_url(handle)))
    }
}

/// The events and body edits fl's own write accounts for.
struct Own {
    events: BTreeMap<&'static str, usize>,
    edits: usize,
}

fn own_write(before: &Window, old: &IssueView, new: &IssueView) -> Own {
    let mut expected: BTreeMap<&'static str, usize> = BTreeMap::new();
    *expected.entry("labeled").or_default() += new
        .labels
        .iter()
        .filter(|l| !old.labels.contains(l))
        .count();
    *expected.entry("unlabeled").or_default() += old
        .labels
        .iter()
        .filter(|l| !new.labels.contains(l))
        .count();
    if old.state != new.state {
        let k = if new.state == "closed" {
            "closed"
        } else {
            "reopened"
        };
        *expected.entry(k).or_default() += 1;
    }
    if old.title != new.title {
        *expected.entry("renamed").or_default() += 1;
    }
    // ⚠ Modelled: a FIRST body edit is taken to add two entries (the
    // original, then the edit), and any later edit one. Under that model
    // every foreign edit is seen. If GitHub adds ONE entry on a first
    // edit, a foreign edit landing with fl's first edit would be hidden.
    // Checked by exact count in the live test
    // `the_edit_history_and_timeline_counts_match_fls_model`.
    // ⚠ The raw bodies are compared, so a rewrite that only turns CRLF into
    // LF is taken to record an edit. Measured live (2026-09-29): GitHub
    // recorded one entry for a CRLF-only rewrite, as modelled; the live
    // test prints it but does not assert it. If that ever changes, `edits`
    // is one too high: `window_after` then waits its full time for an entry
    // that never comes, and fails the write with "has not shown".
    // (`update` cannot currently send a line-endings-only change: its
    // no-op check normalises CRLF, and every change rewrites the block.)
    let edits = match (old.body != new.body, before.edits_total == 0) {
        (false, _) => 0,
        (true, true) => 2,
        (true, false) => 1,
    };
    Own {
        events: expected,
        edits,
    }
}

/// Body edits between two windows; `None` when the history SHRANK, which is
/// someone else deleting an entry.
fn new_edits(before: &Window, after: &Window) -> Option<usize> {
    // ⚠ Modelled: `totalCount` is taken to count every entry — checked
    // on every read by the live test
    // `the_edit_history_and_timeline_counts_match_fls_model`.
    // `last: 100` is taken to list the NEWEST hundred (oldest first) —
    // unmeasured; no live test checks it yet (it needs more than a
    // hundred entries). Counting by `totalCount` does not depend on that
    // order; the ids are a second count that cannot over-count under
    // either order, so the larger of the two is taken. A history that
    // SHRANK (someone deleted an entry) is someone else's change; a
    // deletion offset by an edit in the same window is not seen —
    // unmeasured; no live test checks it yet.
    let by_ids = after.edits.difference(&before.edits).count();
    let by_total = after.edits_total.checked_sub(before.edits_total)?;
    Some(usize::try_from(by_total).unwrap_or(usize::MAX).max(by_ids))
}

/// Whether `after` shows every event and edit fl's own write made. A
/// shrunken history counts as shown, so `check_window` reports it.
fn shows(before: &Window, after: &Window, own: &Own) -> bool {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for e in new_changes(before, after) {
        *seen.entry(e.kind.as_str()).or_default() += 1;
    }
    own.events
        .iter()
        .all(|(k, want)| seen.get(k).copied().unwrap_or(0) >= *want)
        && new_edits(before, after).is_none_or(|n| n >= own.edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::EnvToken;
    use crate::fake::{Copies, FakeGithub};
    use fl_core::MemStore;
    use fl_core::ids::seq_iri;
    use std::time::Duration;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    fn open(fake: &FakeGithub) -> GithubTracker {
        GithubTracker::open(client(fake), "acme/widgets", &MemStore::default())
            .unwrap()
            .0
            .with_visibility(Duration::from_secs(10), Duration::ZERO)
    }

    fn p() -> ProjectId {
        ProjectId(seq_iri(1))
    }

    #[test]
    fn a_record_is_an_issue_with_its_two_labels_and_its_block() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "fix it").unwrap();
        assert_eq!(r.iri().as_str(), "https://github.com/acme/widgets/issues/1");
        let issue = fake.issue(1);
        assert_eq!(issue.labels, vec!["fl:record", "fl:record/todo"]);
        assert_eq!(issue.state, "open");
        let back = t.get_record(&r).unwrap().unwrap();
        assert_eq!((back.title.as_str(), back.state), ("fix it", State::Todo));
        for l in meta::all_labels() {
            assert!(
                fake.state().labels.contains(&l),
                "label {l} was not created"
            );
        }
    }

    #[test]
    fn done_closes_the_record_as_completed_and_withdrawn_closes_a_finding_as_not_planned() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        t.set_record_state(&r, State::Done).unwrap();
        let issue = fake.issue(1);
        assert_eq!(
            (issue.state.as_str(), issue.state_reason.as_deref()),
            ("closed", Some("completed"))
        );
        assert_eq!(t.get_record(&r).unwrap().unwrap().state, State::Done);

        let f = t
            .add_finding(Finding::raise(p(), r.clone(), "hasty", "a claim"))
            .unwrap();
        let mut fin = t.get_finding(&f).unwrap().unwrap();
        fin.withdraw("not concrete").unwrap();
        t.update_finding(&fin).unwrap();
        let issue = fake.issue(2);
        assert_eq!(
            (issue.state.as_str(), issue.state_reason.as_deref()),
            ("closed", Some("not_planned"))
        );
        assert_eq!(t.withdrawals_by("hasty").unwrap(), 1);
        assert_eq!(t.withdrawals_by("careful").unwrap(), 0);
    }

    #[test]
    fn a_label_github_dropped_is_an_error_not_a_success() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().drop_labels = true;
        let err = t.add_record(&p(), "t").unwrap_err();
        assert!(err.to_string().contains("did not apply"), "{err}");
    }

    #[test]
    fn a_create_that_failed_after_landing_is_found_not_duplicated() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().fail_after_create = true;
        let r = t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
        assert_eq!(r.iri().as_str(), "https://github.com/acme/widgets/issues/1");
    }

    #[test]
    fn a_create_whose_answer_was_lost_is_found_not_duplicated() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().hang_up_after_create = true;
        t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
    }

    #[test]
    fn a_create_that_failed_before_landing_is_sent_once_more() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().fail_before_create = true;
        t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
    }

    /// A 201 whose own body cannot be read as an issue
    /// is exactly as ambiguous as a 5xx or a dropped connection (spec §3.3)
    /// — the create may have landed regardless of whether fl could read
    /// GitHub's answer to it.
    #[test]
    fn a_create_whose_201_body_is_unreadable_is_found_not_duplicated() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().unreadable_create_body_next = true;
        let r = t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
        assert_eq!(r.iri().as_str(), "https://github.com/acme/widgets/issues/1");
    }

    /// A transport failure on the SECOND create
    /// attempt (after the create-key search already came up empty) must
    /// carry the same "look before retrying" advice as a bad status
    /// there — losing that advice on this one path
    /// would leave a caller no wiser about the risk of a duplicate.
    #[test]
    fn a_transport_failure_on_the_ambiguous_resend_still_names_the_remedy() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().fail_before_create = true;
        fake.state().hang_up_after_create = true;
        let err = t.add_record(&p(), "t").unwrap_err();
        assert!(says_where_to_look(&err.to_string(), "t"), "{err}");
        assert_eq!(
            fake.issue_count(),
            1,
            "the resend's own create landed even though its answer did not"
        );
    }

    /// A 201 PROVES the create landed, even when its
    /// own body cannot be read. If the create-key search then misses too
    /// (GitHub's list index lagging indefinitely, say), the right answer is
    /// to refuse and say so — never to send the create again, which would
    /// make the very duplicate this whole mechanism exists to prevent.
    #[test]
    fn an_unreadable_201_whose_create_key_search_misses_is_refused_never_resent() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().unreadable_create_body_next = true;
        fake.state().omit_from_list = Some(1);
        let err = t.add_record(&p(), "t").unwrap_err();
        assert!(says_where_to_look(&err.to_string(), "t"), "{err}");
        assert_eq!(
            fake.issue_count(),
            1,
            "the create landed even though it could not be found again — a resend would \
             duplicate it"
        );
    }

    /// The create-key search reads newest first and stops at the first
    /// issue created more than `CREATE_SEARCH_MARGIN` before the attempt
    /// began: one page per search here, however long the repository's
    /// history.
    #[test]
    fn a_create_key_search_stops_at_issues_older_than_the_attempt() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        for i in 0..3 {
            t.add_record(&p(), &format!("old {i}")).unwrap();
        }
        let eleven_minutes = 11 * 60 * 1000;
        for i in fake.state().issues.values_mut() {
            i.created_ms -= eleven_minutes;
        }
        fake.state().max_per_page = 1;
        fake.state().unreadable_create_body_next = true;
        fake.state().omit_from_list = Some(4);
        let before = fake.state().list_issue_requests;
        t.add_record(&p(), "t").unwrap_err();
        assert_eq!(
            fake.state().list_issue_requests - before,
            3,
            "three searches of one page each"
        );
    }

    /// The create-key search reads every issue, fl's or not, with whole
    /// bodies: small pages, so a page of large bodies stays under the
    /// client's answer-size limit (it usually needs one page). A list,
    /// filtered to fl's issues, reads a hundred to a page.
    #[test]
    fn the_create_key_search_reads_small_pages_and_a_list_full_ones() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().hang_up_after_create = true;
        t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.state().issues_firsts, vec![Some(25)]);
        fake.state().issues_firsts.clear();
        t.list_records(&p()).unwrap();
        assert_eq!(fake.state().issues_firsts, vec![Some(100)]);
    }

    /// GitHub's clock may run behind this machine's, so an issue the
    /// attempt made can carry a `createdAt` before the attempt began. The
    /// margin covers it: the search still finds it, and nothing is sent
    /// again.
    #[test]
    fn a_create_key_search_finds_a_create_stamped_by_a_clock_running_behind() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().clock_behind_ms = 5 * 60 * 1000;
        fake.state().hang_up_after_create = true;
        t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
    }

    /// ⚠ Where a create that may have landed is to be looked for: among the
    /// newest issues, labelled or not, by its title — a create sends no
    /// labels, so the issue is in no list filtered by fl's labels.
    fn says_where_to_look(msg: &str, title: &str) -> bool {
        msg.contains("newest issues, labelled or not")
            && msg.contains(&format!("titled {title:?}"))
            && msg.contains("fl github repair <number> --by <name>")
            && !msg.contains("List the repository's fl issues")
    }

    /// Unit-level — once the first attempt was
    /// ambiguous, EVERY later error carries the advice, whatever its kind:
    /// a credential error too, since the first attempt's fate is unknown
    /// regardless of what failed next.
    #[test]
    fn every_error_after_an_ambiguous_create_carries_the_advice() {
        for e in [
            StoreError::Credential("bad token".into()),
            StoreError::RateLimited {
                reset: "1700000000 (unix seconds)".into(),
            },
            StoreError::Unreachable {
                store: "http://127.0.0.1:1".into(),
                cause: "boom".into(),
            },
            StoreError::Backend("GitHub answered 422".into()),
        ] {
            let shown = e.to_string();
            let msg = after_ambiguous_failure("a title", "sending it a second time failed", e)
                .to_string();
            assert!(
                says_where_to_look(&msg, "a title") && msg.contains(&shown),
                "{msg}"
            );
        }
    }

    /// The first create lands
    /// but its answer is lost, and the search for its key then fails. The
    /// error must not say a bare "retry": a retry would make issue #2.
    #[test]
    fn a_failed_key_search_after_an_ambiguous_create_carries_the_advice() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().hang_up_after_create = true;
        fake.state().fail_issues_query_after = Some(0);
        let err = t.add_record(&p(), "t").unwrap_err();
        let msg = err.to_string();
        assert!(
            says_where_to_look(&msg, "t")
                && msg.contains("searching for it by its create key failed"),
            "{msg}"
        );
        assert_eq!(fake.issue_count(), 1, "the first create landed; no resend");
    }

    /// A resend refused by the rate limit after an
    /// ambiguous first attempt carries the advice too — the first attempt
    /// may have landed.
    #[test]
    fn a_rate_limited_resend_after_an_ambiguous_create_carries_the_advice() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().fail_before_create = true;
        fake.state().rate_limited_next_create = true;
        let err = t.add_record(&p(), "t").unwrap_err();
        let msg = err.to_string();
        assert!(
            says_where_to_look(&msg, "t") && msg.contains("rate limit"),
            "{msg}"
        );
        assert_eq!(fake.issue_count(), 0);
    }

    /// ⚠ The engine reads, runs gates, then writes. A finding withdrawn by
    /// someone else in between must not be marked fixed.
    #[test]
    fn a_write_over_an_item_that_changed_since_fl_read_it_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let f = t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
        let mut stale = t.get_finding(&f).unwrap().unwrap();
        fake.web_edit(2, |i| {
            let (prose, mut m) = meta::parse_body(&i.body).unwrap();
            m.state = "withdrawn".into();
            m.withdrawn_reason = Some("someone else".into());
            i.body = meta::render_body(&prose, &m);
            i.labels = vec!["fl:finding".into(), "fl:finding/withdrawn".into()];
            i.state = "closed".into();
            i.state_reason = Some("not_planned".into());
        });
        stale.assigned_to = Some("fixer".into());
        let err = t.update_finding(&stale).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
        assert!(
            fake.issue(2)
                .labels
                .contains(&"fl:finding/withdrawn".to_string()),
            "untouched"
        );
    }

    /// The SAME race as the test above, but the stale
    /// write is addressed by an ALIAS rather than the finding's own URL.
    /// Resolving an alias scans every fl issue (`locate` → `alias_owner` →
    /// `list`); that scan must not itself update `seen` for the issue it
    /// finds, or it silently refreshes `seen` to the CURRENT (already
    /// withdrawn) state right before the conflict check reads it — making
    /// the check compare the current state against itself and pass, so the
    /// stale write sails through and overwrites the withdrawal.
    #[test]
    fn a_stale_write_made_through_an_alias_is_still_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let f = t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
        let alias = Iri::parse("https://github.com/elsewhere/old/issues/9").unwrap();
        t.add_alias(f.iri(), alias.clone()).unwrap();
        let mut stale = t.get_finding(&f).unwrap().unwrap();
        fake.web_edit(2, |i| {
            let (prose, mut m) = meta::parse_body(&i.body).unwrap();
            m.state = "withdrawn".into();
            m.withdrawn_reason = Some("someone else".into());
            i.body = meta::render_body(&prose, &m);
            i.labels = vec!["fl:finding".into(), "fl:finding/withdrawn".into()];
            i.state = "closed".into();
            i.state_reason = Some("not_planned".into());
        });
        stale.assigned_to = Some("fixer".into());
        stale.id = FindingId(alias);
        let err = t.update_finding(&stale).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
        assert!(
            fake.issue(2)
                .labels
                .contains(&"fl:finding/withdrawn".to_string()),
            "untouched"
        );
    }

    /// Control for the test below: with no `add_finding` in between, the
    /// ordinary conflict check on the record alone already works. Isolates
    /// that the next test's failure (without the fix) comes specifically
    /// from `add_finding`'s own read of the record, not from anything else.
    #[test]
    fn a_web_retitle_alone_is_still_caught_as_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let _ = t.get_record(&r).unwrap().unwrap();
        fake.web_edit(1, |i| i.title = "renamed by someone else".into());
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    /// `add_finding` reads the
    /// record it points at (to check it exists and is a record) via the
    /// same `item` a direct `get_record` uses. That read must not move the
    /// record's `seen` baseline — it is a validity check, not something the
    /// caller receives the record from. Without the fix, `add_finding`'s
    /// read silently re-baselines `seen` to GitHub's CURRENT state, so the
    /// `set_record_state` right after compares the current state against
    /// itself, finds no difference, and overwrites the retitle instead of
    /// refusing.
    #[test]
    fn add_finding_does_not_move_the_records_seen_baseline() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let _ = t.get_record(&r).unwrap().unwrap();
        fake.web_edit(1, |i| i.title = "renamed by someone else".into());
        t.add_finding(Finding::raise(p(), r.clone(), "a", "c"))
            .unwrap();
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    #[test]
    fn an_update_that_changes_nothing_sends_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        t.set_record_state(&r, State::Todo).unwrap();
        assert!(!fake.state().requests.iter().any(|q| q.starts_with("PATCH")));
    }

    #[test]
    fn a_list_reads_every_page_and_a_failed_page_fails_it() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        for i in 0..5 {
            t.add_record(&p(), &format!("r{i}")).unwrap();
        }
        fake.state().max_per_page = 2;
        assert_eq!(t.list_records(&p()).unwrap().len(), 5);
        assert!(t.list_records(&ProjectId(seq_iri(2))).unwrap().is_empty());
        fake.state().fail_issues_query_after = Some(1);
        assert!(t.list_records(&p()).is_err(), "never a short list");
    }

    /// A page that answers with no `issues` connection is not an empty
    /// page: the list fails (spec §3.7).
    #[test]
    fn a_list_page_without_an_issues_connection_is_an_error_not_an_empty_list() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        t.add_record(&p(), "a").unwrap();
        fake.state().issues_query_without_connection_next = true;
        let e = t.list_records(&p()).unwrap_err().to_string();
        assert!(
            e.contains("without its `issues`") && e.contains("retry"),
            "{e}"
        );
    }

    /// Measured live on 2026-10-05: GitHub's REST issue list left a new
    /// issue out for 31-93 s, once for more than 180 s; GraphQL's `issues`
    /// connection showed it within 1 s. Every list fl makes — records,
    /// findings, a withdrawal count, an alias scan — reads GraphQL, so none
    /// is short while the REST list lags.
    #[test]
    fn every_list_sees_a_just_created_issue_the_rest_list_leaves_out() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().rest_list_lags = true;
        let r = t.add_record(&p(), "t").unwrap();
        let f = t
            .add_finding(Finding::raise(p(), r.clone(), "hasty", "c"))
            .unwrap();
        let alias = Iri::parse("https://github.com/elsewhere/old/issues/3").unwrap();
        t.add_alias(f.iri(), alias.clone()).unwrap();
        let mut fin = t.get_finding(&f).unwrap().unwrap();
        fin.withdraw("w").unwrap();
        t.update_finding(&fin).unwrap();
        assert_eq!(t.list_records(&p()).unwrap().len(), 1);
        assert_eq!(t.list_findings(&p()).unwrap().len(), 1);
        assert_eq!(t.withdrawals_by("hasty").unwrap(), 1);
        assert_eq!(
            t.get_finding(&FindingId(alias.clone()))
                .unwrap()
                .unwrap()
                .id,
            f
        );
        let other = t.add_record(&p(), "u").unwrap();
        assert!(matches!(
            t.add_alias(other.iri(), alias),
            Err(StoreError::AlreadyExists(_))
        ));
    }

    /// A page that says another follows must move the cursor and hold
    /// issues; one that does neither would be read forever, and is an
    /// error.
    #[test]
    fn a_list_whose_cursor_does_not_move_is_an_error_not_an_endless_read() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        t.add_record(&p(), "a").unwrap();
        fake.state().issues_cursor_stuck = true;
        let before = fake.state().list_issue_requests;
        let e = t.list_records(&p()).unwrap_err().to_string();
        assert!(e.contains("cursor") && e.contains("retry"), "{e}");
        assert_eq!(fake.state().list_issue_requests - before, 2);
    }

    #[test]
    fn an_empty_list_page_that_says_more_follow_is_an_error() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        t.add_record(&p(), "a").unwrap();
        fake.state().issues_empty_page_with_more_next = true;
        let e = t.list_records(&p()).unwrap_err().to_string();
        assert!(e.contains("empty page") && e.contains("retry"), "{e}");
    }

    /// GitHub's cursor names the last issue served, not an offset, so a
    /// list is read once, a page at a time.
    #[test]
    fn a_multi_page_list_is_read_once_by_cursor() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        for i in 0..5 {
            t.add_record(&p(), &format!("r{i}")).unwrap();
        }
        fake.state().max_per_page = 2;
        let before = fake.state().list_issue_requests;
        assert_eq!(t.list_records(&p()).unwrap().len(), 5);
        assert_eq!(
            fake.state().list_issue_requests - before,
            3,
            "3 pages for 5 records at 2 per page, read once"
        );
    }

    /// An issue leaving the filtered set between two pages of one read
    /// moves no other issue across a page boundary: the cursor names a
    /// position, not an offset (spec §3.7). Under offset paging this drops
    /// a live record with no error.
    #[test]
    fn an_issue_leaving_the_list_mid_read_drops_no_other() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        for i in 0..5 {
            t.add_record(&p(), &format!("r{i}")).unwrap();
        }
        fake.state().max_per_page = 2;
        // Issue 1 vanishes right as the second page is served; the first
        // page already went out with it.
        let next = fake.state().list_issue_requests + 2;
        fake.state().vanish_after_list_request = Some((next, 1));
        let titles: Vec<String> = t
            .list_records(&p())
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert_eq!(titles, ["r0", "r1", "r2", "r3", "r4"]);
    }

    /// The row of spec §8.2 that makes the others meaningful: a store that
    /// errors unconditionally would pass every refusal test, and fails this.
    #[test]
    fn an_empty_repository_lists_nothing_cleanly() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        assert_eq!(t.list_records(&p()).unwrap(), vec![]);
        assert_eq!(t.list_findings(&p()).unwrap(), vec![]);
        assert_eq!(t.withdrawals_by("anyone").unwrap(), 0);
    }

    #[test]
    fn deleted_moved_absent_and_foreign_issues_are_told_apart() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let gone = t.add_record(&p(), "gone").unwrap();
        let moved = t.add_record(&p(), "moved").unwrap();
        fake.state().issues.get_mut(&1).unwrap().gone = true;
        fake.state().issues.get_mut(&2).unwrap().moved_to =
            Some(format!("{}/repositories/9/issues/1", fake.url()));
        assert!(matches!(t.get_record(&gone), Err(StoreError::Deleted(_))));
        assert!(matches!(
            t.get_record(&moved),
            Err(StoreError::Moved { .. })
        ));
        assert_eq!(t.get_record(&RecordId(t.issue_url(99))).unwrap(), None);
        let plain = fake.plain_issue(&["bug"], false);
        let pr = fake.plain_issue(&["fl:record", "fl:record/todo"], true);
        for n in [plain, pr] {
            let err = t.get_record(&RecordId(t.issue_url(n))).unwrap_err();
            assert!(matches!(err, StoreError::NotAnFlItem { .. }), "{err:?}");
        }
    }

    /// GraphQL's `issues` connection lists issues only, so a pull request
    /// carrying fl labels is not in a list; named directly, it is refused
    /// as not an fl item (`deleted_moved_absent_and_foreign_issues_are_told_apart`).
    #[test]
    fn a_pull_request_with_fl_labels_is_not_in_a_list() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        t.add_record(&p(), "t").unwrap();
        fake.plain_issue(&["fl:record", "fl:record/todo"], true);
        assert_eq!(t.list_records(&p()).unwrap().len(), 1);
    }

    #[test]
    fn a_title_with_leading_or_trailing_whitespace_is_refused_before_anything_is_sent() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        for title in [" t", "t ", "\tt", "t\n", " "] {
            let err = t.add_record(&p(), title).unwrap_err();
            assert!(
                err.to_string().contains("starts or ends with whitespace"),
                "{title:?}: {err}"
            );
        }
        assert!(
            !fake
                .state()
                .requests
                .iter()
                .any(|r| r.starts_with("POST /repos/acme/widgets/issues")),
            "nothing may be sent"
        );
        assert_eq!(fake.issue_count(), 0);
        t.add_record(&p(), "a title with inner  spaces").unwrap();
    }

    #[test]
    fn a_title_over_the_limit_is_refused_before_anything_is_sent() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let err = t.add_record(&p(), &"x".repeat(257)).unwrap_err();
        assert!(err.to_string().contains("256"), "{err}");
        assert!(
            !fake
                .state()
                .requests
                .iter()
                .any(|r| r.starts_with("POST /repos/acme/widgets/issues")),
            "nothing may be sent"
        );
    }

    #[test]
    fn a_web_edit_that_disagrees_with_the_block_is_reported_and_never_dropped_from_a_list() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| {
            i.labels = vec!["fl:record".into(), "fl:record/done".into()]
        });
        assert!(matches!(t.get_record(&r), Err(StoreError::Diverged { .. })));
        assert!(matches!(
            t.list_records(&p()),
            Err(StoreError::Diverged { .. })
        ));
    }

    #[test]
    fn handles_are_issue_numbers_of_the_right_kind() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let f = t
            .add_finding(Finding::raise(p(), r.clone(), "a", "c"))
            .unwrap();
        assert_eq!(t.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
        assert_eq!(t.handle_of(Kind::Finding, r.iri()).unwrap(), None);
        assert_eq!(t.handle_of(Kind::Project, r.iri()).unwrap(), None);
        assert_eq!(
            t.resolve_handle(Kind::Finding, 2).unwrap().as_ref(),
            Some(f.iri())
        );
        assert_eq!(t.resolve_handle(Kind::Record, 2).unwrap(), None);
        assert_eq!(t.resolve_handle(Kind::Record, 99).unwrap(), None);
    }

    /// Measured live on 2026-10-05: the `labeled` event of a label a create
    /// adds by its own call reaches the timeline 1-2 s after it.
    /// Unless the create waits for them, they land in the NEXT write's
    /// window and read as someone else's.
    #[test]
    fn a_create_waits_until_its_labels_show_in_the_timeline() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().timeline_lag_reads = 1;
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().timeline_lag_reads = 0;
        t.set_record_state(&r, State::Doing).unwrap();
    }

    /// Measured live on 2026-10-05: labels set IN a create show their
    /// `labeled` events 28-88 s late, past fl's wait, so they land in the
    /// next update's window and read as someone else's. fl creates the
    /// issue without labels and then adds them, whose events show in 1-2 s.
    #[test]
    fn a_create_followed_at_once_by_an_update_is_not_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).with_visibility(Duration::from_millis(50), Duration::from_millis(5));
        fake.state().creation_labels_late = true;
        let r = t.add_record(&p(), "t").unwrap();
        t.set_record_state(&r, State::Doing).unwrap();
        let requests = fake.state().requests.clone();
        let created = requests
            .iter()
            .position(|q| q == "POST /repos/acme/widgets/issues");
        let labelled = requests
            .iter()
            .position(|q| q == "POST /repos/acme/widgets/issues/1/labels");
        assert!(
            matches!((created, labelled), (Some(c), Some(l)) if c < l),
            "{requests:?}"
        );
    }

    /// Adding labels never removes one. An issue that already carries
    /// another fl label (an automation labelled it on open) keeps it, and
    /// the error says what it found and names the repair — not a missing
    /// permission, since every label fl sent was applied.
    #[test]
    fn a_created_issue_carrying_another_fl_label_is_named_without_blaming_permission() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().labels_on_open = vec!["fl:record/done".into()];
        let e = t.add_record(&p(), "t").unwrap_err().to_string();
        assert!(
            e.contains("\"fl:record/done\"")
                && e.contains("fl github repair 1 --by <name>")
                && !e.contains("permission"),
            "{e}"
        );
        assert_eq!(issue_posts(&fake), 1);
    }

    /// An issue that already carries exactly fl's labels is not labelled
    /// again.
    #[test]
    fn a_created_issue_already_carrying_fls_labels_gets_no_label_call() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().labels_on_open = vec!["fl:record".into(), "fl:record/todo".into()];
        let r = t.add_record(&p(), "t").unwrap();
        assert_eq!(t.get_record(&r).unwrap().unwrap().state, State::Todo);
        assert!(
            !fake
                .state()
                .requests
                .iter()
                .any(|q| q == "POST /repos/acme/widgets/issues/1/labels"),
            "no label call"
        );
    }

    /// A create answered with a body other than the one sent is not
    /// written as sent, but it exists, without fl's labels (none were
    /// sent): the error says so, names the remedy, and does not blame label
    /// permission.
    #[test]
    fn a_create_answered_with_another_body_names_the_issue_and_the_repair() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().create_body_appended_next = Some("\nadded by someone".into());
        let e = t.add_record(&p(), "t").unwrap_err().to_string();
        assert!(
            e.contains("https://github.com/acme/widgets/issues/1")
                && e.contains("the body came back different")
                && e.contains("It exists without fl's labels")
                && e.contains("Do not create it again")
                && e.contains("fl github repair 1 --by <name>")
                && !e.contains("permission"),
            "{e}"
        );
        assert_eq!(issue_posts(&fake), 1);
        assert!(fake.issue(1).labels.is_empty());
    }

    /// A create whose answer was lost landed without labels — a create sends
    /// none. The search finds it by its key though it carries no fl label
    /// and the REST list lags, adds fl's labels, and never sends the create
    /// again.
    #[test]
    fn a_lost_create_that_landed_without_labels_is_found_labelled_and_not_sent_again() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().rest_list_lags = true;
        fake.state().hang_up_after_create = true;
        let r = t.add_record(&p(), "t").unwrap();
        assert_eq!(r.iri().as_str(), "https://github.com/acme/widgets/issues/1");
        assert_eq!(issue_posts(&fake), 1, "sent once, never again");
        assert_eq!(fake.issue_count(), 1);
        assert_eq!(fake.issue(1).labels, vec!["fl:record", "fl:record/todo"]);
        assert_eq!(t.list_records(&p()).unwrap().len(), 1);
    }

    /// The issue exists once the create is answered. When adding its labels
    /// then fails, the error names it and the remedy — `fl github repair`,
    /// which restores the labels from the block (spec §0.1b, 14) — and
    /// nothing is sent again.
    #[test]
    fn a_create_whose_labels_fail_names_the_issue_and_repair_restores_them() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().fail_label_add_next = true;
        let e = t.add_record(&p(), "t").unwrap_err().to_string();
        assert!(
            e.contains("https://github.com/acme/widgets/issues/1")
                && e.contains("without some or all of fl's labels")
                && e.contains("Do not create it again")
                && e.contains("fl github repair 1 --by <name>"),
            "{e}"
        );
        assert_eq!(fake.issue_count(), 1);
        assert_eq!(issue_posts(&fake), 1, "never created again");
        assert!(fake.issue(1).labels.is_empty());
        let url = t.issue_url(1);
        assert!(t.repair(&url, "owner").unwrap().changed);
        assert_eq!(fake.issue(1).labels, vec!["fl:record", "fl:record/todo"]);
        assert_eq!(
            t.get_record(&RecordId(url)).unwrap().unwrap().state,
            State::Todo
        );
    }

    /// A create is two calls: the issue, then its two labels in ONE label
    /// call. (Adding them one per call did not stop GitHub recording a
    /// `labeled` event twice; the conflict window drops such copies.)
    #[test]
    fn a_create_adds_its_two_labels_in_one_call() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        t.add_record(&p(), "t").unwrap();
        let label_calls = fake
            .state()
            .requests
            .iter()
            .filter(|q| *q == "POST /repos/acme/widgets/issues/1/labels")
            .count();
        assert_eq!(label_calls, 1);
        assert_eq!(fake.issue(1).labels, vec!["fl:record", "fl:record/todo"]);
    }

    /// Measured live on 2026-10-05: GitHub sometimes records a `labeled`
    /// event twice, about 0-1 s apart (4 of 10 two-label calls; 2 of 33
    /// issues fl created, one label per call). Re-adding a label the issue
    /// already carries makes no event, so a `labeled` event for a label
    /// already on the issue cannot be anyone's write: it is not counted.
    /// Here the copies of a create's two events land in the next update's
    /// window.
    #[test]
    fn a_create_whose_label_events_github_records_twice_is_not_followed_by_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).with_visibility(Duration::from_millis(50), Duration::from_millis(5));
        fake.state().labeled_copies = Some(Copies::Held);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().labeled_copies = None;
        t.set_record_state(&r, State::Doing).unwrap();
        t.set_record_state(&r, State::Review).unwrap();
    }

    /// The same for an update's own `labeled` event, recorded again in the
    /// next update's window.
    #[test]
    fn an_updates_label_event_recorded_twice_is_not_a_conflict_in_the_next_window() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().labeled_copies = Some(Copies::Held);
        t.set_record_state(&r, State::Doing).unwrap();
        fake.state().labeled_copies = None;
        t.set_record_state(&r, State::Review).unwrap();
    }

    /// A copy of fl's own `labeled` event in the same window, before or
    /// after it, is one change, not two.
    #[test]
    fn a_copy_of_fls_own_label_event_in_its_window_is_not_a_conflict() {
        for copies in [Copies::Before, Copies::After] {
            let fake = FakeGithub::start("acme/widgets");
            let t = open(&fake);
            let r = t.add_record(&p(), "t").unwrap();
            fake.state().labeled_copies = Some(copies);
            t.set_record_state(&r, State::Doing)
                .unwrap_or_else(|e| panic!("{copies:?}: {e}"));
            fake.state().labeled_copies = None;
            t.set_record_state(&r, State::Review)
                .unwrap_or_else(|e| panic!("{copies:?}: {e}"));
        }
    }

    /// A repair that adds two labels at once, whose events GitHub records
    /// twice, followed at once by an update: the copies land in the
    /// update's window and are not counted.
    #[test]
    fn an_update_right_after_a_two_label_repair_is_not_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| i.labels = vec!["bug".into()]);
        fake.state().labeled_copies = Some(Copies::Held);
        assert!(t.repair(r.iri(), "owner").unwrap().changed);
        fake.state().labeled_copies = None;
        t.set_record_state(&r, State::Doing).unwrap();
    }

    /// A copy of an earlier `labeled` event must not stand in for fl's own,
    /// lagging one: if the wait ended on the copy, fl's own events would
    /// land in the next write's window as someone else's.
    #[test]
    fn a_copy_of_an_earlier_label_event_does_not_end_the_wait_for_fls_own() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().labeled_copies = Some(Copies::Held);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().labeled_copies = None;
        fake.web_edit(1, |i| i.labels = vec![]);
        // The repair's own two `labeled` events lag two timeline reads; the
        // create's held copies show at once.
        fake.state().timeline_lag_reads = 2;
        assert!(t.repair(r.iri(), "owner").unwrap().changed);
        fake.state().timeline_lag_reads = 0;
        t.set_record_state(&r, State::Doing).unwrap();
    }

    /// Removing a label is never noise: someone else's removal inside fl's
    /// window is a conflict.
    #[test]
    fn a_foreign_label_removal_inside_fls_write_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| i.labels.push("bug".into()));
        fake.state().foreign_label_changes_on_next_patch = vec![("unlabeled".into(), "bug".into())];
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    /// A label removed and added back inside fl's window leaves the labels
    /// as they were, but the removal is someone else's write.
    #[test]
    fn a_foreign_label_removed_and_added_back_inside_fls_write_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| i.labels.push("bug".into()));
        fake.state().foreign_label_changes_on_next_patch = vec![
            ("unlabeled".into(), "bug".into()),
            ("labeled".into(), "bug".into()),
        ];
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    /// A label someone adds after fl's window opens, before fl reads the
    /// issue inside it, is in the issue fl writes from. Its event is still
    /// a change: whether a `labeled` event is noise is read from the
    /// timeline, never from the issue fl read.
    #[test]
    fn a_label_added_just_after_fls_window_opens_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().foreign_label_after_next_timeline = true;
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    /// An update's own events can lag too: unless it waits for them, they
    /// land in the next write's window and read as someone else's.
    #[test]
    fn an_update_waits_until_its_own_events_show_in_the_timeline() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().timeline_lag_reads = 2;
        t.set_record_state(&r, State::Doing).unwrap();
        // A body-only write: fl expects no events, so a late one is foreign.
        let alias = Iri::parse("https://github.com/elsewhere/old/issues/7").unwrap();
        t.add_alias(r.iri(), alias).unwrap();
    }

    /// The body's edit history lags too, on its own clock: waiting for the
    /// timeline alone is not enough.
    #[test]
    fn an_update_waits_until_its_own_body_edits_show_in_the_edit_history() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        t.set_record_state(&r, State::Doing).unwrap();
        fake.state().edit_lag_reads = 2;
        t.set_record_state(&r, State::Review).unwrap();
        // A repair edits no body, so a late entry from the write before it
        // would read as someone else's.
        fake.web_edit(1, |i| i.labels = vec!["bug".into()]);
        assert!(t.repair(r.iri(), "owner").unwrap().changed);
    }

    /// The issue exists once GitHub answers the create. An error after that
    /// would invite a retry, and a retry mints a new create key: a duplicate.
    #[test]
    fn a_create_whose_timeline_read_fails_still_succeeds() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().fail_next_timeline = true;
        t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.issue_count(), 1);
    }

    /// Waiting for fl's own events must not hide someone else's that land
    /// with them.
    #[test]
    fn a_foreign_write_is_still_caught_while_fl_waits_for_its_own() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().timeline_lag_reads = 2;
        fake.state().foreign_label_on_next_patch = true;
        assert!(matches!(
            t.set_record_state(&r, State::Doing),
            Err(StoreError::Conflict { .. })
        ));
    }

    #[test]
    fn an_update_github_never_shows_is_an_error_that_says_to_read_again() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).with_visibility(Duration::from_millis(50), Duration::from_millis(5));
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().timeline_lag_reads = u32::MAX;
        let e = t
            .set_record_state(&r, State::Doing)
            .unwrap_err()
            .to_string();
        assert!(e.contains("has not shown"), "{e}");
        assert!(e.contains("read it again"), "{e}");
        assert_eq!(
            fake.issue(1).labels,
            vec!["fl:record", "fl:record/doing"],
            "the write itself landed"
        );
        // Not remembered: the next write, without a fresh read, is refused.
        fake.state().timeline_lag_reads = 0;
        assert!(matches!(
            t.set_record_state(&r, State::Review),
            Err(StoreError::Conflict { .. })
        ));
    }

    #[test]
    fn a_create_github_never_shows_still_succeeds() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).with_visibility(Duration::from_millis(50), Duration::from_millis(5));
        fake.state().timeline_lag_reads = u32::MAX;
        t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.issue_count(), 1);
    }

    #[test]
    fn an_alias_is_found_by_a_full_scan_and_a_second_use_of_it_is_refused() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let a = t.add_record(&p(), "a").unwrap();
        let b = t.add_record(&p(), "b").unwrap();
        let old = Iri::parse("https://github.com/elsewhere/old/issues/7").unwrap();
        t.add_alias(a.iri(), old.clone()).unwrap();
        assert_eq!(t.get_record(&RecordId(old.clone())).unwrap().unwrap().id, a);
        assert!(matches!(
            t.add_alias(b.iri(), old),
            Err(StoreError::AlreadyExists(_))
        ));
        assert!(matches!(
            t.add_alias(b.iri(), a.0.clone()),
            Err(StoreError::AlreadyExists(_))
        ));
    }

    /// ⚠ Two issues cannot legitimately end up sharing an alias through
    /// `add_alias` (it refuses the second use), but a full scan must still
    /// refuse to guess if it ever finds one anyway — naming both, never
    /// picking one silently.
    #[test]
    fn an_alias_shared_by_two_issues_is_refused_naming_both() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        t.add_record(&p(), "a").unwrap();
        t.add_record(&p(), "b").unwrap();
        let shared = Iri::parse("https://github.com/elsewhere/shared/issues/1").unwrap();
        for n in [1, 2] {
            fake.web_edit(n, |i| {
                let (prose, mut m) = meta::parse_body(&i.body).unwrap();
                m.also_known_as.push(shared.clone());
                i.body = meta::render_body(&prose, &m);
            });
        }
        let err = t.get_record(&RecordId(shared)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains('1') && msg.contains('2'), "{msg}");
    }

    #[test]
    fn open_remembers_the_repository_and_refuses_one_that_replaced_it() {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        assert_eq!(
            memory.bound_node_id("acme/widgets").unwrap().as_deref(),
            Some("R_1")
        );
        memory.bind_node_id("acme/widgets", "R_other").unwrap();
        let err = GithubTracker::open(client(&fake), "acme/widgets", &memory)
            .err()
            .unwrap();
        assert!(
            matches!(err, StoreError::RepositoryReplaced { .. }),
            "{err:?}"
        );
    }

    /// GitHub also answers 410 on every issue of a
    /// repository that has Issues turned off — indistinguishable, at that
    /// point, from an issue GitHub deleted. Caught once, at `open`, so
    /// `fetch` never has to guess which one it saw.
    #[test]
    fn a_repository_with_issues_turned_off_is_refused_naming_the_remedy() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().repos[0].has_issues = false;
        let memory = MemStore::default();
        let err = GithubTracker::open(client(&fake), "acme/widgets", &memory)
            .err()
            .unwrap();
        assert!(matches!(err, StoreError::Backend(_)), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("Issues") && msg.contains("acme/widgets"),
            "{msg}"
        );
    }

    /// Once the
    /// RESEND itself lands with an unreadable 201, a fresh search finds the
    /// issue it actually created (nothing here makes the fake's list lag),
    /// so the call succeeds — exactly one issue, no third send. This is the
    /// good outcome the RULE exists to reach; the test after this one
    /// covers what happens when the search cannot help.
    #[test]
    fn a_resend_that_lands_with_an_unreadable_201_is_found_by_a_fresh_search() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().fail_before_create = true;
        fake.state().unreadable_create_body_next = true;
        let r = t.add_record(&p(), "t").unwrap();
        assert_eq!(r.iri().as_str(), "https://github.com/acme/widgets/issues/1");
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
        assert_eq!(
            issue_posts(&fake),
            2,
            "the first attempt and the resend, never a third"
        );
    }

    /// The RULE — once GitHub has answered
    /// 2xx to a create, first send OR resend, every LATER failure is
    /// refused with the advice and never leads to another send. Before the
    /// fix, the resend used the strict `send`, so a 201 with an unreadable
    /// body made `client.send` itself fail with a plain "…not JSON…"
    /// `Backend` error that the resend's error wrapper's `other => other` arm
    /// let straight through — no advice, even though the create had already
    /// landed (`fail_before_create` + `unreadable_create_body_next` → that
    /// error, `issue_count == 1`).
    /// Combined with `omit_from_list` here so the follow-up search also
    /// cannot find it, reaching the refusal this test checks for.
    #[test]
    fn a_resend_that_lands_with_an_unreadable_201_and_cannot_be_found_is_refused_never_a_third_send()
     {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().fail_before_create = true;
        fake.state().unreadable_create_body_next = true;
        fake.state().omit_from_list = Some(1);
        let err = t.add_record(&p(), "t").unwrap_err();
        assert!(says_where_to_look(&err.to_string(), "t"), "{err}");
        assert_eq!(
            fake.issue_count(),
            1,
            "the resend's own create landed even though it could not be confirmed"
        );
        assert_eq!(issue_posts(&fake), 2, "no third send");
    }

    /// Once a 201 proves the
    /// create landed, a failure of the FOLLOW-UP SEARCH itself (not just a
    /// miss) must also carry the advice — before the fix it passed through
    /// with the client's generic "…; retry" text instead.
    #[test]
    fn a_search_that_itself_fails_after_an_unreadable_201_still_carries_the_advice() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().unreadable_create_body_next = true;
        fake.state().fail_issues_query_after = Some(0);
        let err = t.add_record(&p(), "t").unwrap_err();
        assert!(says_where_to_look(&err.to_string(), "t"), "{err}");
        assert_eq!(
            fake.issue_count(),
            1,
            "the create landed even though confirming it failed"
        );
    }

    /// Creates sent: `POST /issues` itself, not a post to one issue's
    /// labels or comments.
    fn issue_posts(fake: &FakeGithub) -> usize {
        fake.state()
            .requests
            .iter()
            .filter(|r| *r == "POST /repos/acme/widgets/issues")
            .count()
    }

    /// A create answered 201 whose body
    /// then breaks off is a create that landed — the status was read. It
    /// used to surface as `Unreachable`, which took the resend path; with
    /// the search missing too, that resend made a silent duplicate.
    #[test]
    fn a_201_whose_body_breaks_off_and_cannot_be_found_is_refused_never_resent() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().broken_create_body_next = true;
        fake.state().omit_from_list = Some(1);
        let err = t.add_record(&p(), "t").unwrap_err();
        assert!(says_where_to_look(&err.to_string(), "t"), "{err}");
        assert_eq!(fake.issue_count(), 1, "a resend would duplicate it");
        assert_eq!(issue_posts(&fake), 1, "exactly one send");
    }

    /// The same broken 201, with nothing making the list lag —
    /// the search finds the issue the one send made.
    #[test]
    fn a_201_whose_body_breaks_off_is_found_by_its_create_key() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().broken_create_body_next = true;
        let r = t.add_record(&p(), "t").unwrap();
        assert_eq!(r.iri().as_str(), "https://github.com/acme/widgets/issues/1");
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
        assert_eq!(issue_posts(&fake), 1, "exactly one send");
    }

    /// Any 2xx answer to a create is a create that landed, not
    /// only 201 — a 200 carrying the issue is the created issue.
    #[test]
    fn a_create_answered_200_with_the_issue_is_the_created_issue() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().create_answers_200_next = true;
        let r = t.add_record(&p(), "t").unwrap();
        assert_eq!(r.iri().as_str(), "https://github.com/acme/widgets/issues/1");
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
        assert_eq!(issue_posts(&fake), 1, "exactly one send");
    }

    #[test]
    fn a_finding_on_a_finding_is_the_wrong_kind_and_on_nothing_is_no_such_record() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let f = t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
        let err = t
            .add_finding(Finding::raise(p(), RecordId(f.0.clone()), "a", "c"))
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::WrongKind {
                    expected: Kind::Record,
                    found: Kind::Finding,
                    ..
                }
            ),
            "{err:?}"
        );
        let err = t
            .add_finding(Finding::raise(p(), RecordId(t.issue_url(99)), "a", "c"))
            .unwrap_err();
        assert!(matches!(err, StoreError::NoSuchRecord(_)), "{err:?}");
    }

    #[test]
    fn a_renamed_repository_is_followed_with_a_notice_and_its_old_urls_still_resolve() {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        let r = t.add_record(&p(), "t").unwrap();
        fake.rename("acme/gadgets");
        let (t, notice) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        assert_eq!(
            notice,
            Some(Notice::Renamed {
                from: "acme/widgets".into(),
                to: "acme/gadgets".into()
            })
        );
        let back = t.get_record(&r).unwrap().unwrap();
        assert_eq!(
            back.id.iri().as_str(),
            "https://github.com/acme/gadgets/issues/1"
        );
    }

    #[test]
    fn a_reused_old_name_is_refused_at_open_and_an_old_url_says_why_it_is_not_owned() {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        let r = t.add_record(&p(), "t").unwrap();
        fake.rename("acme/gadgets");
        fake.reuse_name("acme/widgets");
        let err = GithubTracker::open(client(&fake), "acme/widgets", &memory)
            .err()
            .unwrap();
        assert!(
            matches!(err, StoreError::RepositoryReplaced { .. }),
            "{err:?}"
        );
        let (t, _) = GithubTracker::open(client(&fake), "acme/gadgets", &memory).unwrap();
        let err = t.get_record(&r).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        assert!(
            err.to_string()
                .contains("`acme/widgets` is another repository, not the bound `acme/gadgets`"),
            "{err}"
        );
    }

    /// An unrelated repository is "not the bound
    /// one" — never "now names a different repository", which claims a
    /// history fl does not know. "Now" is kept for a name GitHub redirects
    /// to a repository under another name.
    #[test]
    fn a_url_of_another_repository_says_what_it_is_without_claiming_a_rename() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.reuse_name("other/repo");
        let err = t
            .get_record(&RecordId(
                Iri::parse("https://github.com/other/repo/issues/1").unwrap(),
            ))
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        assert!(
            msg.contains("`other/repo` is another repository, not the bound `acme/widgets`")
                && !msg.contains("now"),
            "{msg}"
        );

        fake.reuse_name("other/new");
        let id = fake.state().repos.last().unwrap().id;
        fake.state().redirects.insert("other/old".into(), id);
        let msg = t
            .get_record(&RecordId(
                Iri::parse("https://github.com/other/old/issues/1").unwrap(),
            ))
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("`other/old` now leads to `other/new`, a different repository"),
            "{msg}"
        );
    }

    #[test]
    fn a_findings_record_reference_follows_its_node_id_and_the_next_write_rewrites_it() {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        let r = t.add_record(&p(), "t").unwrap();
        t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
        fake.rename("acme/gadgets");
        fake.reuse_name("acme/widgets");
        let (t, _) = GithubTracker::open(client(&fake), "acme/gadgets", &memory).unwrap();
        let mut f = t.get_finding(&FindingId(t.issue_url(2))).unwrap().unwrap();
        assert_eq!(
            f.record.iri().as_str(),
            "https://github.com/acme/gadgets/issues/1"
        );
        assert!(
            fake.issue(2).body.contains("acme/widgets/issues/1"),
            "not yet rewritten"
        );
        f.withdraw("x").unwrap();
        t.update_finding(&f).unwrap();
        let body = fake.issue(2).body;
        assert!(
            body.contains("acme/gadgets/issues/1") && !body.contains("acme/widgets/issues/1"),
            "{body}"
        );
    }

    /// `current_ref`'s `Moved` branch (a node lookup answers a
    /// repository id that is not the one this tracker is bound to — the
    /// issue was transferred elsewhere) was unreachable, because the fake's
    /// node lookup always answered under the bound repository. The fake's
    /// `transferred_nodes` knob makes it answer as GitHub does for a
    /// transferred issue: a different `repository.id`, and a URL under a
    /// name this tracker does not own.
    #[test]
    fn a_transferred_records_reference_is_a_moved_error_naming_the_new_url() {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        let r = t.add_record(&p(), "t").unwrap();
        t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
        fake.rename("acme/gadgets");
        fake.reuse_name("acme/widgets");
        let record_node_id = fake.issue(1).node_id;
        fake.state().transferred_nodes.insert(record_node_id);
        let (t, _) = GithubTracker::open(client(&fake), "acme/gadgets", &memory).unwrap();
        let err = t.get_finding(&FindingId(t.issue_url(2))).unwrap_err();
        match err {
            StoreError::Moved { id, to } => {
                assert_eq!(id.as_str(), "https://github.com/acme/widgets/issues/1");
                assert_eq!(to, "https://github.com/elsewhere/transferred/issues/1");
            }
            other => panic!("expected Moved, got {other:?}"),
        }
    }

    #[test]
    fn a_foreign_label_inside_fls_write_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().foreign_label_on_next_patch = true;
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    /// A write that crossed someone else's is not a baseline: the next
    /// write refuses until the caller reads the item again.
    #[test]
    fn a_conflicted_write_does_not_move_the_baseline() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().foreign_label_on_next_patch = true;
        t.set_record_state(&r, State::Doing).unwrap_err();
        let err = t.set_record_state(&r, State::Review).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
        assert_eq!(t.get_record(&r).unwrap().unwrap().state, State::Doing);
        t.set_record_state(&r, State::Review).unwrap();
    }

    #[test]
    fn a_foreign_body_edit_inside_fls_write_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        // The first edit is fl's; the blind spot is the first edit only.
        t.set_record_state(&r, State::Doing).unwrap();
        fake.state().foreign_edit_on_next_patch = true;
        let err = t.set_record_state(&r, State::Review).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    /// Under the model, a foreign FIRST edit adds two entries and fl's edit
    /// one more: three is more than fl's two, so it is seen.
    #[test]
    fn a_foreign_first_edit_inside_fls_first_edit_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().foreign_edit_on_next_patch = true;
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    #[test]
    fn a_deleted_or_moved_issue_keeps_its_outcome_through_a_write() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let gone = t.add_record(&p(), "gone").unwrap();
        let moved = t.add_record(&p(), "moved").unwrap();
        fake.state().issues.get_mut(&1).unwrap().gone = true;
        fake.state().issues.get_mut(&2).unwrap().moved_to =
            Some(format!("{}/repositories/9/issues/1", fake.url()));
        assert!(matches!(
            t.set_record_state(&gone, State::Doing),
            Err(StoreError::Deleted(_))
        ));
        assert!(matches!(
            t.set_record_state(&moved, State::Doing),
            Err(StoreError::Moved { .. })
        ));
        let absent = RecordId(t.issue_url(99));
        assert!(matches!(
            t.set_record_state(&absent, State::Doing),
            Err(StoreError::NoSuchRecord(_))
        ));
    }

    #[test]
    fn fls_own_writes_are_never_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        for s in [
            State::Doing,
            State::Review,
            State::Done,
            State::Doing,
            State::Done,
        ] {
            t.set_record_state(&r, s).unwrap();
        }
        let f = t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
        let mut fin = t.get_finding(&f).unwrap().unwrap();
        fin.withdraw("w").unwrap();
        t.update_finding(&fin).unwrap();
    }

    #[test]
    fn repair_rewrites_the_labels_and_status_from_the_block_and_says_who() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| {
            i.labels = vec!["bug".into()];
            i.state = "closed".into();
        });
        assert!(t.get_record(&r).is_err());
        let done = t.repair(r.iri(), "owner").unwrap();
        assert_eq!(
            (done.number, done.state.as_str(), done.changed),
            (1, "todo", true)
        );
        let issue = fake.issue(1);
        assert_eq!(issue.labels, vec!["bug", "fl:record", "fl:record/todo"]);
        assert_eq!(
            issue.state, "open",
            "the block wins: a repair never closes what fl left open"
        );
        assert!(
            issue.comments.iter().any(|c| c.contains("by owner")),
            "{:?}",
            issue.comments
        );
        assert_eq!(t.get_record(&r).unwrap().unwrap().state, State::Todo);
    }

    /// A 200 proves the repair landed, so an unreadable body must not skip
    /// the comment naming who ran it: a rerun would find the issue
    /// consistent and post nothing.
    #[test]
    fn repair_answered_with_an_unreadable_200_still_says_who() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| i.labels = vec!["bug".into()]);
        fake.state().unreadable_patch_body_next = true;
        let e = t.repair(r.iri(), "owner").unwrap_err().to_string();
        assert!(e.contains("GitHub answered 200 to the repair"), "{e}");
        assert!(e.contains("was posted"), "{e}");
        let issue = fake.issue(1);
        assert_eq!(issue.labels, vec!["bug", "fl:record", "fl:record/todo"]);
        assert!(
            issue.comments.iter().any(|c| c.contains("by owner")),
            "{:?}",
            issue.comments
        );
    }

    #[test]
    fn repair_trusts_the_block_over_a_state_label() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| {
            i.labels = vec!["fl:record".into(), "fl:record/done".into()];
            i.state = "closed".into();
        });
        let done = t.repair(r.iri(), "owner").unwrap();
        assert_eq!(done.state, "todo");
        assert_eq!(fake.issue(1).labels, vec!["fl:record", "fl:record/todo"]);
        assert_eq!(fake.issue(1).state, "open");
    }

    #[test]
    fn repair_of_a_consistent_issue_changes_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let done = t.repair(r.iri(), "owner").unwrap();
        assert!(!done.changed);
        assert!(fake.issue(1).comments.is_empty());
    }

    /// A label deleted from the repository leaves every fl issue without
    /// it. Repair creates it explicitly, before its write — never as a side
    /// effect of the write.
    #[test]
    fn repair_creates_a_deleted_label_before_it_writes() {
        let fake = FakeGithub::start("acme/widgets");
        let r = open(&fake).add_record(&p(), "t").unwrap();
        fake.state().labels.remove("fl:record/todo");
        fake.web_edit(1, |i| i.labels = vec!["fl:record".into()]);
        let t = open(&fake);
        fake.state().requests.clear();
        assert!(t.repair(r.iri(), "owner").unwrap().changed);
        let requests = fake.state().requests.clone();
        let created = requests
            .iter()
            .position(|q| q == "POST /repos/acme/widgets/labels");
        let written = requests
            .iter()
            .position(|q| q == "PATCH /repos/acme/widgets/issues/1");
        assert!(
            matches!((created, written), (Some(c), Some(w)) if c < w),
            "{requests:?}"
        );
    }

    #[test]
    fn repair_of_a_deleted_or_moved_issue_keeps_its_outcome() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let gone = t.add_record(&p(), "gone").unwrap();
        let moved = t.add_record(&p(), "moved").unwrap();
        fake.state().issues.get_mut(&1).unwrap().gone = true;
        fake.state().issues.get_mut(&2).unwrap().moved_to =
            Some(format!("{}/repositories/9/issues/1", fake.url()));
        assert!(matches!(
            t.repair(gone.iri(), "owner"),
            Err(StoreError::Deleted(_))
        ));
        assert!(matches!(
            t.repair(moved.iri(), "owner"),
            Err(StoreError::Moved { .. })
        ));
    }

    #[test]
    fn repair_refuses_a_damaged_block_and_says_to_restore_it() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| i.body = "someone rewrote it".into());
        let err = t.repair(r.iri(), "owner").unwrap_err();
        assert!(err.to_string().contains("restore the block"), "{err}");
        assert!(
            !err.to_string().contains("fl github repair"),
            "the refusal must not say to run the command that refused: {err}"
        );
    }

    /// GitHub's edit history answers `issue: null` for
    /// a pull request's number, so a pull request is refused BEFORE the
    /// window is opened.
    #[test]
    fn a_pull_request_is_refused_before_the_window() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let n = fake.plain_issue(&["fl:record"], true);
        fake.state().requests.clear();
        let is_pr = |e: &StoreError| matches!(e, StoreError::NotAnFlItem { what, .. } if what == "a pull request");
        let err = t
            .set_record_state(&RecordId(t.issue_url(n)), State::Doing)
            .unwrap_err();
        assert!(is_pr(&err), "{err:?}");
        let err = t.repair(&t.issue_url(n), "owner").unwrap_err();
        assert!(is_pr(&err), "{err:?}");
        let requests = fake.state().requests.clone();
        assert!(
            !requests.iter().any(|q| q.contains("/timeline")),
            "{requests:?}"
        );
    }

    /// When GitHub's GraphQL finds no issue at a
    /// number, the window says what the number is now — never "came back
    /// without `nodes`".
    #[test]
    fn a_window_graphql_cannot_find_names_what_the_number_is() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let n = fake.plain_issue(&["fl:record"], true);
        let err = t
            .window(n)
            .err()
            .expect("a pull request has no edit history");
        assert!(
            matches!(&err, StoreError::NotAnFlItem { what, .. } if what == "a pull request"),
            "{err:?}"
        );
    }

    /// An entry without an id is not "no edit".
    #[test]
    fn an_edit_history_entry_without_an_id_is_an_error_not_a_short_count() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().null_edit_node_next = true;
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("without an id") && msg.contains("retry"),
            "{msg}"
        );
    }

    /// If `last: 100` lists the OLDEST entries, the
    /// ids never show a new edit; `totalCount` still counts it.
    #[test]
    fn a_foreign_edit_the_listed_ids_do_not_show_is_counted_by_the_total() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        t.set_record_state(&r, State::Doing).unwrap();
        fake.state().edit_nodes_cap = Some(2);
        t.set_record_state(&r, State::Review).unwrap();
        fake.state().foreign_edit_on_next_patch = true;
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    /// A history that shrank inside fl's window was changed by someone
    /// else: fl never deletes an entry. The ids are hidden, so only the
    /// total can see it.
    #[test]
    fn an_edit_history_that_shrank_inside_fls_write_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().edit_nodes_cap = Some(0);
        t.set_record_state(&r, State::Doing).unwrap();
        t.set_record_state(&r, State::Review).unwrap();
        fake.state().delete_edits_on_next_patch = 3;
        let err = t.set_record_state(&r, State::Done).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    /// The SECOND read is the one fl changes. A label
    /// someone adds between the first read and the window is in that read,
    /// so fl's write keeps it and does not count it as foreign.
    #[test]
    fn a_write_landing_before_the_window_is_kept_not_overwritten() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().foreign_label_on_next_timeline = true;
        t.set_record_state(&r, State::Doing).unwrap();
        assert_eq!(
            fake.issue(1).labels,
            vec!["bug", "fl:record", "fl:record/doing"]
        );
    }

    /// The repair landed even though it crossed
    /// another write, so the record of who ran it is still posted.
    #[test]
    fn a_repair_that_crossed_another_write_still_records_who_ran_it() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| i.labels = vec!["bug".into()]);
        fake.state().foreign_label_on_next_patch = true;
        let err = t.repair(r.iri(), "owner").unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
        let comments = fake.issue(1).comments;
        assert!(
            comments.iter().any(|c| c.contains("by owner")),
            "{comments:?}"
        );
    }

    /// The repair PATCH answered 200, then its check
    /// failed (GitHub kept the old labels). The repair landed at least in
    /// part, so the comment is posted anyway, and the error says it was — a
    /// rerun could otherwise find nothing to do and never post it.
    #[test]
    fn a_repair_whose_check_fails_after_the_patch_still_records_who_ran_it() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| {
            i.labels = vec!["fl:record".into(), "fl:record/done".into()]
        });
        fake.state().drop_labels = true;
        let msg = t.repair(r.iri(), "owner").unwrap_err().to_string();
        assert!(
            msg.contains("did not apply") && msg.contains("ran the repair was posted"),
            "{msg}"
        );
        let comments = fake.issue(1).comments;
        assert_eq!(comments.len(), 1, "{comments:?}");
        assert!(comments[0].contains("by owner"), "{comments:?}");
    }

    /// When the comment fails as well as the check,
    /// the error says to add the comment by hand.
    #[test]
    fn a_repair_whose_check_and_comment_both_fail_says_to_add_the_comment_by_hand() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| {
            i.labels = vec!["fl:record".into(), "fl:record/done".into()]
        });
        fake.state().drop_labels = true;
        fake.state().fail_comment_next = true;
        let msg = t.repair(r.iri(), "owner").unwrap_err().to_string();
        assert!(
            msg.contains("did not apply")
                && msg.contains("was not posted (GitHub answered 500)")
                && msg.contains("add that comment by hand"),
            "{msg}"
        );
        assert!(fake.issue(1).comments.is_empty());
    }

    /// A timeline item fl cannot classify is an
    /// error naming a remedy, never skipped.
    #[test]
    fn a_timeline_item_without_a_kind_or_an_id_is_an_error() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().odd_timeline_item_next = Some(json!({"id": 99}));
        let msg = t
            .set_record_state(&r, State::Doing)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("without an `event`") && msg.contains("retry"),
            "{msg}"
        );
        fake.state().odd_timeline_item_next = Some(json!({"event": "labeled"}));
        let msg = t
            .set_record_state(&r, State::Doing)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("has no id") && msg.contains("retry"), "{msg}");
        // A label event that names no label cannot be told from noise.
        fake.state().odd_timeline_item_next = Some(json!({"id": 98, "event": "unlabeled"}));
        let msg = t
            .set_record_state(&r, State::Doing)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("names no label") && msg.contains("retry"),
            "{msg}"
        );
    }

    /// The replay behind the conflict window: a `labeled` event for a
    /// label already on the issue is dropped, by the labels the timeline
    /// itself has added and removed; every other event is kept.
    #[test]
    fn a_labeled_event_for_a_label_already_on_is_dropped_and_nothing_else_is() {
        let ev = |id: u64, kind: &str, label: Option<&str>| Event {
            id,
            kind: kind.into(),
            label: label.map(str::to_string),
        };
        let events = vec![
            ev(1, "labeled", Some("a")),
            ev(2, "labeled", Some("a")),
            ev(3, "labeled", Some("b")),
            ev(4, "unlabeled", Some("a")),
            ev(5, "unlabeled", Some("a")),
            ev(6, "labeled", Some("a")),
            ev(7, "closed", None),
            ev(8, "labeled", Some("b")),
        ];
        let kept: Vec<u64> = changes(&events).iter().map(|e| e.id).collect();
        assert_eq!(kept, [1, 3, 4, 5, 6, 7]);
    }

    fn raise_security(t: &GithubTracker) -> Result<FindingId, StoreError> {
        let r = t.add_record(&p(), "t").unwrap();
        let mut f = Finding::raise(p(), r, "a", "a secret-leaking defect");
        f.security = true;
        t.add_finding(f)
    }

    #[test]
    fn a_security_finding_is_written_only_to_a_private_repository() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        raise_security(&t).unwrap();
        assert!(fake.issue(2).body.contains("\"security\":true"));
    }

    #[test]
    fn a_security_finding_on_a_public_or_internal_repository_is_refused_and_nothing_is_written() {
        for visibility in ["public", "internal"] {
            let fake = FakeGithub::start("acme/widgets");
            let t = open(&fake);
            fake.state().repos[0].visibility = visibility.into();
            let err = raise_security(&t).unwrap_err();
            assert!(
                matches!(err, StoreError::SecurityNotPrivate { visibility: ref v, .. } if v == visibility),
                "{err:?}"
            );
            assert_eq!(fake.issue_count(), 1, "only the record exists");
        }
    }

    #[test]
    fn an_unreadable_visibility_refuses_a_security_finding() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().fail_repo_read = true;
        let mut f = Finding::raise(p(), r, "a", "c");
        f.security = true;
        let err = t.add_finding(f).unwrap_err();
        assert!(err.to_string().contains("visibility"), "{err}");
        assert_eq!(fake.issue_count(), 1);
    }

    #[test]
    fn a_finding_not_marked_security_may_go_to_a_public_repository() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().repos[0].visibility = "public".into();
        let r = t.add_record(&p(), "t").unwrap();
        t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
    }

    /// The same suites the local stores pass (spec §8.1): the GitHub
    /// tracker over a `MemStore` catalog and ledger, checked by
    /// `CatalogChecked`, numbered by `KindRouted`.
    mod contract {
        use super::*;
        use fl_core::conformance::{self, Bound, Fixture};
        use fl_core::store::{CatalogChecked, KindRouted};

        struct Split {
            catalog: MemStore,
            tracker: GithubTracker,
            _fake: FakeGithub,
        }

        impl Fixture for Split {
            fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
                let checked = CatalogChecked {
                    catalog: &self.catalog,
                    tracker: &self.tracker,
                };
                let handles = KindRouted {
                    catalog: &self.catalog,
                    tracker: &self.tracker,
                };
                f(&Bound {
                    catalog: &self.catalog,
                    tracker: &checked,
                    ledger: &self.catalog,
                    handles: &handles,
                });
            }
        }

        fn split() -> Split {
            let fake = FakeGithub::start("acme/widgets");
            let tracker = open(&fake);
            Split {
                catalog: MemStore::default(),
                tracker,
                _fake: fake,
            }
        }

        #[test]
        fn the_github_tracker_meets_the_tracker_contract() {
            conformance::tracker(split);
        }

        #[test]
        fn the_github_tracker_meets_the_all_roles_contract() {
            conformance::all_roles(split);
        }
    }
}
