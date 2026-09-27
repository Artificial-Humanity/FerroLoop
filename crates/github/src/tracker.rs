//! `GithubTracker`: the `Tracker` and `Handles` roles over the Issues of one
//! repository (GitHub tracker spec §2–§3).
//!
//! ⚠ It does not check project references — it cannot see the catalog. It
//! is always used through `CatalogChecked`, which does (spec §1.3).

use crate::client::{Client, Method};
use crate::meta::{self, IssueView, ItemKind, Meta, Read, RecordRef, TITLE_MAX};
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
}

/// Timeline events that change what fl reads (spec §3.3). A comment or a
/// mention does not, and is not a conflict.
const STATE_EVENTS: [&str; 5] = ["labeled", "unlabeled", "closed", "reopened", "renamed"];

/// What GitHub has recorded about an issue's changes at one moment.
struct Window {
    events: BTreeMap<u64, String>,
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

/// The resend inside `after_ambiguous_create` gets the "list before
/// retrying" advice ONLY for a transport failure — the one case where fl
/// genuinely cannot tell whether this second attempt is about to duplicate
/// the first (fix round 1, item 5b). Any other error — a credential
/// problem, say — is not about that ambiguity, and naming it "GitHub could
/// not be reached" would misreport it; it passes through with its own
/// variant untouched (fix round 2, item 3).
fn wrap_resend_error(e: StoreError) -> StoreError {
    match e {
        e @ StoreError::Unreachable { .. } => backend(format!(
            "GitHub could not be reached to retry an issue create a second time ({e}). List \
             the repository's fl issues before retrying, so the retry makes no duplicate"
        )),
        other => other,
    }
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
    let want: BTreeSet<&str> = labels.iter().map(String::as_str).collect();
    let got: BTreeSet<&str> = back.labels.iter().map(String::as_str).collect();
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
    if want != got {
        problems.push(format!("the labels came back as {got:?}, not {want:?}"));
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
        };
        Ok((tracker, notice))
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
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

    /// Oldest first: an issue created while a list is read lands on its last
    /// page, so it cannot shift an earlier page's issues onto the next one.
    /// ⚠ The opposite hazard remains, because GitHub's paging is by offset,
    /// not a cursor: an issue LEAVING the filtered set mid-read (a label
    /// removed, closed out from under a state filter) shifts every later
    /// issue one position earlier, which can drop a live item from a page
    /// already served — silently, with no error (spec §3.7). `list` catches
    /// this itself, by re-reading and comparing issue numbers whenever more
    /// than one page was needed.
    fn list_path(&self, labels: &[String]) -> String {
        self.path(&format!(
            "/issues?state=all&sort=created&direction=asc&per_page=100&labels={}",
            labels.join(",")
        ))
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
            Some(_) => Owner::Elsewhere(Some(format!("`{name}` now names a different repository"))),
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
    /// `remember`: whether this read updates `seen` (fix round 2, item 1 —
    /// the same rule `list` already follows, fix round 1 item 1). Only a
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
    /// `remember`: whether a returned item updates `seen` (fix round 1,
    /// item 1). Only a read the CALLER directly asked for and directly
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
        let mut labels = vec![meta::kind_label(kind)];
        if let Some(s) = state {
            labels.push(meta::state_label(kind, s));
        }
        let path = self.list_path(&labels);
        let (raw, pages) = self.client.get_all_paged(&path)?;
        if pages > 1 {
            // ⚠ GitHub pages by offset, not a cursor (spec §3.7): an issue
            // leaving the filtered set mid-read shifts every later issue
            // back by one, which can drop a live item with no error. A
            // second, independent read is compared by issue number; any
            // difference means the set changed while fl was reading it, and
            // the whole list is refused rather than returned short. A single
            // page cannot have shifted anything onto or off of itself, so it
            // costs nothing here.
            let (again, _) = self.client.get_all_paged(&path)?;
            let first: BTreeSet<u64> = raw
                .iter()
                .filter_map(|v| v.get("number").and_then(Value::as_u64))
                .collect();
            let second: BTreeSet<u64> = again
                .iter()
                .filter_map(|v| v.get("number").and_then(Value::as_u64))
                .collect();
            if first != second {
                return Err(backend(format!(
                    "the list of {} issues changed while fl read it; retry",
                    kind.as_wire()
                )));
            }
        }
        let mut out = Vec::new();
        let mut numbers = BTreeSet::new();
        for v in raw {
            let issue = IssueView::from_json(&v)?;
            // Pages are read one by one; an issue seen twice is counted once.
            if !numbers.insert(issue.number) {
                continue;
            }
            match meta::read_item(&issue)? {
                Read::Item {
                    kind: k,
                    meta,
                    prose,
                } if k == kind => {
                    // The label filter is taken to mean AND; the block is
                    // checked too, so a looser filter cannot widen the list.
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
        let sent = json!({"title": title, "body": body, "labels": labels});
        let path = self.path("/issues");
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
            // ⚠ ANY 2xx PROVES the create landed (fix round 2, item 2; fix
            // round 4: not only 201, and a body that broke off counts as
            // unreadable): an unreadable body is never followed by a
            // resend, only a search — resending here risks making exactly
            // the duplicate this whole mechanism exists to avoid.
            Ok(r) if (200..300).contains(&r.status) => match IssueView::from_json(&r.body) {
                Ok(issue) => issue,
                Err(_) => self.after_unreadable_create(kind, meta, r.status)?,
            },
            // ⚠ An ambiguous failure may already have created the issue.
            // Look for the create key before sending again (spec §3.3).
            Ok(r) if r.status >= 500 => self.after_ambiguous_create(kind, meta, &path, &sent)?,
            Err(StoreError::Unreachable { .. }) => {
                self.after_ambiguous_create(kind, meta, &path, &sent)?
            }
            Ok(r) => {
                return Err(backend(format!(
                    "GitHub answered {} to an issue create",
                    r.status
                )));
            }
            Err(e) => return Err(e),
        };
        check_written(&issue, title, &labels, &body, "open", None)?;
        self.kinds.borrow_mut().insert(issue.number, kind);
        self.remember(issue.number, meta, prose, title);
        Ok(issue)
    }

    /// Search for a create by its key, `settle` apart, up to three times.
    /// `Ok(None)` when none of the three searches found it.
    fn search_by_create_key(
        &self,
        kind: ItemKind,
        key: &str,
    ) -> Result<Option<IssueView>, StoreError> {
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(self.settle);
            }
            if let Some(found) = self.find_by_create_key(kind, key)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// ⚠ The list GitHub serves may lag a create that just landed, so the
    /// key is searched for first; only when EVERY search misses is the
    /// create sent again. Only for a FIRST-attempt failure where the create
    /// may not have happened at all — a 5xx answer, or the connection
    /// dropping before an answer arrived. See `after_unreadable_create` for
    /// the case where it certainly did (fix round 2, item 2).
    fn after_ambiguous_create(
        &self,
        kind: ItemKind,
        meta: &Meta,
        path: &str,
        sent: &Value,
    ) -> Result<IssueView, StoreError> {
        if let Some(found) = self.search_by_create_key(kind, &meta.create_key)? {
            return Ok(found);
        }
        // ⚠ `send_unchecked_json`, not `send` (fix round 3, item 1): the
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
                Err(_) => self.after_unreadable_create(kind, meta, r.status),
            },
            Ok(r) => Err(backend(format!(
                "GitHub failed an issue create twice (the second answer was {}). List the \
                 repository's fl issues before retrying, so the retry makes no duplicate",
                r.status
            ))),
            // ⚠ A transport failure here gets the SAME advice as a bad
            // status (fix round 1, item 5b): the search already came up
            // empty, so fl cannot tell whether THIS attempt is about to
            // duplicate an issue the first attempt actually made — only a
            // fresh list can settle that, same as a plain failed retry.
            // Any OTHER kind of error (a credential problem, say) is not
            // about that ambiguity at all — GitHub never answered anything
            // here, 2xx or otherwise — and passes through unchanged (fix
            // round 2, item 3) — `wrap_resend_error` draws exactly that
            // line, and only for a failure BEFORE any 2xx.
            Err(e) => Err(wrap_resend_error(e)),
        }
    }

    /// ⚠ A 2xx proves the create landed (fix round 2, item 2): unlike a 5xx
    /// answer or a dropped connection, there is no "may not have happened"
    /// here. A miss on every search is never followed by a resend — that
    /// would risk making exactly the duplicate this whole path exists to
    /// avoid. The caller is told to list the repository's fl issues itself.
    fn after_unreadable_create(
        &self,
        kind: ItemKind,
        meta: &Meta,
        status: u16,
    ) -> Result<IssueView, StoreError> {
        // ⚠ The create is certain here — GitHub already answered 2xx — so
        // the search's own failure (not just a miss) carries the same
        // advice too (fix round 3, item 2): the client's generic "…;
        // retry" on a failed page read would otherwise reach the caller
        // with no hint that a resend is exactly what must NOT happen.
        let found = self
            .search_by_create_key(kind, &meta.create_key)
            .map_err(|e| {
                backend(format!(
                    "GitHub answered {status} to an issue create, but its own body could not \
                     be read, and searching for it afterward by its create key failed too ({e}). \
                     List the repository's fl issues before retrying, so the retry makes no \
                     duplicate"
                ))
            })?;
        found.ok_or_else(|| {
            backend(format!(
                "GitHub answered {status} to an issue create, but its own body could not be \
                 read, and the issue could not be found afterward by its create key \
                 either. List the repository's fl issues before retrying, so the retry \
                 makes no duplicate"
            ))
        })
    }

    fn find_by_create_key(
        &self,
        kind: ItemKind,
        key: &str,
    ) -> Result<Option<IssueView>, StoreError> {
        for v in self
            .client
            .get_all(&self.list_path(&[meta::kind_label(kind)]))?
        {
            let issue = IssueView::from_json(&v)?;
            if let Ok((_, m)) = meta::parse_body(&issue.body)
                && m.create_key == key
            {
                return Ok(Some(issue));
            }
        }
        Ok(None)
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
        // ⚠ Modelled: the timeline and the edit history are taken to show
        // fl's own write as soon as GitHub answers the PATCH. If they lag,
        // fl's own events can fall after `after` — a foreign write landing
        // late is then missed here, and fl's late events land inside the
        // NEXT write's window as foreign. Checked by the live test
        // `the_edit_history_and_timeline_counts_match_fls_model` (tests/live.rs), which reads
        // at once after each fl write and again two seconds later.
        let after = self.window(n)?;
        // ⚠ Before `remember`: a write that crossed someone else's must not
        // become the baseline the next write is compared with.
        self.check_window(&id, &before, &after, &issue, &back)?;
        self.remember(n, &meta, &prose, &title);
        Ok(())
    }

    /// The issue's state-changing timeline events and its body edit history,
    /// now. ⚠ Never `remember`s: it reads no item.
    fn window(&self, n: u64) -> Result<Window, StoreError> {
        let mut events = BTreeMap::new();
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
            events.insert(id, kind.to_string());
        }
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
        Ok(Window {
            events,
            edits,
            edits_total,
        })
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
        let mut expected: BTreeMap<&str, usize> = BTreeMap::new();
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
        let mut foreign = Vec::new();
        for (eid, kind) in &after.events {
            if before.events.contains_key(eid) {
                continue;
            }
            match expected.get_mut(kind.as_str()) {
                Some(left) if *left > 0 => *left -= 1,
                _ => foreign.push(format!("a `{kind}` event")),
            }
        }
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
        let Some(by_total) = after.edits_total.checked_sub(before.edits_total) else {
            return Err(StoreError::Conflict {
                id: id.clone(),
                detail: "GitHub shows changes fl did not make: an entry was deleted from the \
                         body's edit history"
                    .into(),
            });
        };
        let new_edits = usize::try_from(by_total).unwrap_or(usize::MAX).max(by_ids);
        // ⚠ Modelled: a FIRST body edit is taken to add two entries (the
        // original, then the edit), and any later edit one. Under that model
        // every foreign edit is seen. If GitHub adds ONE entry on a first
        // edit, a foreign edit landing with fl's first edit would be hidden.
        // Checked by exact count in the live test
        // `the_edit_history_and_timeline_counts_match_fls_model`.
        // ⚠ Modelled: the raw bodies are compared, so a rewrite that only
        // turns CRLF into LF is taken to record an edit. If GitHub records
        // none for it, `own_edits` is one too high and one foreign edit in
        // the same window would be hidden. Partly measured: the live test
        // `the_edit_history_and_timeline_counts_match_fls_model` prints whether GitHub
        // records an entry for a CRLF-only rewrite (it does not assert it),
        // and checks that an fl write after one is not a conflict.
        let own_edits = match (old.body != new.body, before.edits_total == 0) {
            (false, _) => 0,
            (true, true) => 2,
            (true, false) => 1,
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
        let r = self.client.send(
            Method::Patch,
            &self.path(&format!("/issues/{n}")),
            Some(&sent),
        )?;
        if r.status != 200 {
            return Err(backend(format!(
                "GitHub answered {} to the repair of {id}; read it again before retrying",
                r.status
            )));
        }
        let back = IssueView::from_json(&r.body)?;
        check_written(
            &back,
            &issue.title,
            &labels,
            &issue.body.replace("\r\n", "\n"),
            state,
            reason,
        )?;
        let after = self.window(n)?;
        // ⚠ The repair has landed either way, so the comment naming who ran
        // it is posted BEFORE a conflict is returned: a rerun would find the
        // issue consistent and post nothing, losing the record.
        let crossed = self.check_window(id, &before, &after, &issue, &back);
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
        match (crossed, comment_failed) {
            (Err(StoreError::Conflict { id, detail }), Some(why)) => {
                return Err(StoreError::Conflict {
                    id,
                    detail: format!(
                        "{detail}; and the comment recording that {by} ran the repair was not \
                         posted ({why}) — add that comment by hand"
                    ),
                });
            }
            (Err(e), _) => return Err(e),
            (Ok(()), Some(why)) => {
                return Err(backend(format!(
                    "the repair of {id} was written, but the comment that records who ran it \
                     was not posted ({why}); add that comment by hand"
                )));
            }
            (Ok(()), None) => {}
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
        // ⚠ `remember: false` (fix round 2, item 1): this is a validity
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use fl_core::MemStore;
    use fl_core::ids::seq_iri;

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

    /// Fix round 1, item 5a: a 201 whose own body cannot be read as an issue
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

    /// Fix round 1, item 5b: a transport failure on the SECOND create
    /// attempt (after the create-key search already came up empty) must
    /// carry the same "list the repository's fl issues before retrying"
    /// remedy as a bad status there — losing that advice on this one path
    /// would leave a caller no wiser about the risk of a duplicate.
    #[test]
    fn a_transport_failure_on_the_ambiguous_resend_still_names_the_remedy() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().fail_before_create = true;
        fake.state().hang_up_after_create = true;
        let err = t.add_record(&p(), "t").unwrap_err();
        assert!(
            err.to_string()
                .contains("List the repository's fl issues before retrying"),
            "{err}"
        );
        assert_eq!(
            fake.issue_count(),
            1,
            "the resend's own create landed even though its answer did not"
        );
    }

    /// Fix round 2, item 2: a 201 PROVES the create landed, even when its
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
        assert!(
            err.to_string()
                .contains("List the repository's fl issues before retrying"),
            "{err}"
        );
        assert_eq!(
            fake.issue_count(),
            1,
            "the create landed even though it could not be found again — a resend would \
             duplicate it"
        );
    }

    /// Fix round 2, item 3: unit-level, no fake — `wrap_resend_error`'s own
    /// contract. A `Credential` error is not about whether the resend's
    /// write is ambiguous; it must keep its own variant, unlike a transport
    /// failure (fix round 1, item 5b), which gets the retry advice. This
    /// case is not provoked through the fake because the fake's create
    /// route has no path that answers 401 specifically on a resend without
    /// also changing what the first attempt saw — a knob built only to
    /// force one match arm would test the knob, not the guard.
    #[test]
    fn the_resends_own_error_is_wrapped_only_when_it_is_a_transport_failure() {
        let credential = StoreError::Credential("bad token".into());
        match wrap_resend_error(credential) {
            StoreError::Credential(msg) => assert_eq!(msg, "bad token"),
            other => panic!("a non-transport error must pass through unchanged: {other:?}"),
        }
        let unreachable = StoreError::Unreachable {
            store: "http://127.0.0.1:1".into(),
            cause: "boom".into(),
        };
        match wrap_resend_error(unreachable) {
            StoreError::Backend(msg) => assert!(
                msg.contains("List the repository's fl issues before retrying"),
                "{msg}"
            ),
            other => panic!("a transport failure must get the retry advice: {other:?}"),
        }
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

    /// Fix round 1, item 1: the SAME race as the test above, but the stale
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

    /// Fix round 2, item 1: the reviewer's probe. `add_finding` reads the
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
        fake.state().fail_page = Some(("/repos/acme/widgets/issues".into(), 2));
        assert!(t.list_records(&p()).is_err(), "never a short list");
    }

    /// Fix round 2, item 4: the flip side of the test below — a list that
    /// fits on a single page must not pay for the second-pass stability
    /// check at all. Nothing can have shifted a page onto or off of itself.
    #[test]
    fn a_single_page_list_is_read_once() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        t.add_record(&p(), "a").unwrap();
        t.add_record(&p(), "b").unwrap();
        let before = fake.state().requests.len();
        assert_eq!(t.list_records(&p()).unwrap().len(), 2);
        let issue_list_requests = fake.state().requests[before..]
            .iter()
            .filter(|r| r.starts_with("GET /repos/acme/widgets/issues?"))
            .count();
        assert_eq!(
            issue_list_requests, 1,
            "a single page must not be read twice"
        );
    }

    /// Fix round 1, item 2: a list that needed more than one page is read a
    /// SECOND time to check the set of issue numbers is stable — a stable
    /// list still succeeds, just at the cost of the extra read.
    #[test]
    fn a_stable_multi_page_list_is_read_twice_and_still_succeeds() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        for i in 0..5 {
            t.add_record(&p(), &format!("r{i}")).unwrap();
        }
        fake.state().max_per_page = 2;
        let before = fake.state().requests.len();
        assert_eq!(t.list_records(&p()).unwrap().len(), 5);
        let issue_list_requests = fake.state().requests[before..]
            .iter()
            .filter(|r| r.starts_with("GET /repos/acme/widgets/issues?"))
            .count();
        assert_eq!(
            issue_list_requests, 6,
            "3 pages needed for 5 records at 2 per page, read twice"
        );
    }

    /// Fix round 1, item 2: GitHub pages by offset. An issue leaving the
    /// filtered set between two page reads of the SAME pass shifts every
    /// later issue back by one — which can drop a live item with no error
    /// (spec §3.7). The second, independent read this fake's fix adds must
    /// catch the mismatch rather than returning what looks like a complete
    /// but short list.
    #[test]
    fn a_list_that_changes_shape_between_the_two_passes_is_an_error_not_a_short_list() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        for i in 0..5 {
            t.add_record(&p(), &format!("r{i}")).unwrap();
        }
        fake.state().max_per_page = 2;
        // Issue 1 vanishes right as the first pass's second page is served
        // (its first page already went out with issue 1 still in it).
        fake.state().vanish_after_list_request = Some((2, 1));
        let err = t.list_records(&p()).unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref m) if m.contains("changed while fl read it")),
            "{err:?}"
        );
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

    /// Fix round 1, item 4: GitHub also answers 410 on every issue of a
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

    /// Fix round 3, item 1: the reviewer's probe, positive case. Once the
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
        let posts = fake
            .state()
            .requests
            .iter()
            .filter(|r| r.starts_with("POST /repos/acme/widgets/issues"))
            .count();
        assert_eq!(posts, 2, "the first attempt and the resend, never a third");
    }

    /// Fix round 3, item 1 (Important): the RULE — once GitHub has answered
    /// 2xx to a create, first send OR resend, every LATER failure is
    /// refused with the advice and never leads to another send. Before the
    /// fix, the resend used the strict `send`, so a 201 with an unreadable
    /// body made `client.send` itself fail with a plain "…not JSON…"
    /// `Backend` error that `wrap_resend_error`'s `other => other` arm let
    /// straight through — no advice, even though the create had already
    /// landed (the reviewer's probe: `fail_before_create` +
    /// `unreadable_create_body_next` → that error, `issue_count == 1`).
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
        assert!(
            err.to_string()
                .contains("List the repository's fl issues before retrying"),
            "{err}"
        );
        assert_eq!(
            fake.issue_count(),
            1,
            "the resend's own create landed even though it could not be confirmed"
        );
        let posts = fake
            .state()
            .requests
            .iter()
            .filter(|r| r.starts_with("POST /repos/acme/widgets/issues"))
            .count();
        assert_eq!(posts, 2, "no third send");
    }

    /// Fix round 3, item 2 (Minor, same class): once a 201 proves the
    /// create landed, a failure of the FOLLOW-UP SEARCH itself (not just a
    /// miss) must also carry the advice — before the fix it passed through
    /// with the client's generic "…; retry" text instead.
    #[test]
    fn a_search_that_itself_fails_after_an_unreadable_201_still_carries_the_advice() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().unreadable_create_body_next = true;
        fake.state().fail_page = Some(("/repos/acme/widgets/issues".into(), 1));
        let err = t.add_record(&p(), "t").unwrap_err();
        assert!(
            err.to_string()
                .contains("List the repository's fl issues before retrying"),
            "{err}"
        );
        assert_eq!(
            fake.issue_count(),
            1,
            "the create landed even though confirming it failed"
        );
    }

    fn issue_posts(fake: &FakeGithub) -> usize {
        fake.state()
            .requests
            .iter()
            .filter(|r| r.starts_with("POST /repos/acme/widgets/issues"))
            .count()
    }

    /// Fix round 4 (the reviewer's probe): a create answered 201 whose body
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
        assert!(
            err.to_string()
                .contains("List the repository's fl issues before retrying"),
            "{err}"
        );
        assert_eq!(fake.issue_count(), 1, "a resend would duplicate it");
        assert_eq!(issue_posts(&fake), 1, "exactly one send");
    }

    /// Fix round 4: the same broken 201, with nothing making the list lag —
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

    /// Fix round 4: any 2xx answer to a create is a create that landed, not
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
            err.to_string().contains("now names a different repository"),
            "{err}"
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

    /// Fix round 1: `current_ref`'s `Moved` branch (a node lookup answers a
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

    /// Fix round 1, item 1: GitHub's edit history answers `issue: null` for
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

    /// Fix round 1, item 1: when GitHub's GraphQL finds no issue at a
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

    /// Fix round 1, item 2a: an entry without an id is not "no edit".
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

    /// Fix round 1, item 2b: if `last: 100` lists the OLDEST entries, the
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

    /// Fix round 1, item 3: the SECOND read is the one fl changes. A label
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

    /// Fix round 1, item 4: the repair landed even though it crossed
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

    /// Fix round 1, items 6 and 7: a timeline item fl cannot classify is an
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
