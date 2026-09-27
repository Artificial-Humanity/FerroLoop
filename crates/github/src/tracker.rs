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

fn text(v: &Value, k: &str) -> Result<String, StoreError> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| backend(format!("GitHub sent a repository without `{k}`")))
}

/// `GET /repos/{name}`, following ONE redirect (a renamed or transferred
/// repository answers 301). `Ok(None)` for 404.
fn read_repo(client: &Client, name: &str) -> Result<Option<Repo>, StoreError> {
    let mut reply = client.send(Method::Get, &format!("/repos/{name}"), None)?;
    if matches!(reply.status, 301 | 302 | 307 | 308) {
        let to = reply
            .location
            .clone()
            .ok_or_else(|| backend(format!("GitHub redirected `{name}` without a Location")))?;
        reply = client.send(Method::Get, &to, None)?;
    }
    match reply.status {
        200 => Ok(Some(Repo {
            full_name: text(&reply.body, "full_name")?,
            node_id: text(&reply.body, "node_id")?,
        })),
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
        let repo = read_repo(&client, configured)?.ok_or_else(|| {
            backend(format!(
                "the repository `{configured}` does not exist, or the credential cannot read \
                 it. Check the `github` binding and the credential"
            ))
        })?;
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
    /// page, and cannot shift an earlier page's issues onto the next one.
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
            Some(r) if r.node_id == self.repo.node_id => Owner::Ours(n),
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
            for (issue, meta, _) in self.list(kind, None)? {
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
    fn item(&self, id: &Iri, want: ItemKind) -> Result<Found, StoreError> {
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
                    self.remember(n, &meta, &prose, &issue.title);
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
    fn list(
        &self,
        kind: ItemKind,
        state: Option<&str>,
    ) -> Result<Vec<(IssueView, Meta, String)>, StoreError> {
        let mut labels = vec![meta::kind_label(kind)];
        if let Some(s) = state {
            labels.push(meta::state_label(kind, s));
        }
        let mut out = Vec::new();
        let mut numbers = BTreeSet::new();
        for v in self.client.get_all(&self.list_path(&labels))? {
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
                    self.remember(issue.number, &meta, &prose, &issue.title);
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
        let issue = match self.client.send(Method::Post, &path, Some(&sent)) {
            Ok(r) if r.status == 201 => IssueView::from_json(&r.body)?,
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

    /// ⚠ The list GitHub serves may lag a create that just landed, so the
    /// key is searched for three times, `settle` apart, before one resend.
    fn after_ambiguous_create(
        &self,
        kind: ItemKind,
        meta: &Meta,
        path: &str,
        sent: &Value,
    ) -> Result<IssueView, StoreError> {
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(self.settle);
            }
            if let Some(found) = self.find_by_create_key(kind, &meta.create_key)? {
                return Ok(found);
            }
        }
        let r = self.client.send(Method::Post, path, Some(sent))?;
        if r.status != 201 {
            return Err(backend(format!(
                "GitHub failed an issue create twice (the second answer was {}). List the \
                 repository's fl issues before retrying, so the retry makes no duplicate",
                r.status
            )));
        }
        IssueView::from_json(&r.body)
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
    /// an issue that does not exist.
    fn update(
        &self,
        n: u64,
        kind: ItemKind,
        missing: impl FnOnce() -> StoreError,
        change: impl FnOnce(&mut Meta, &mut String, &mut String) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        self.ensure_labels()?;
        let id = self.issue_url(n);
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
        self.remember(n, &meta, &prose, &title);
        Ok(())
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

    /// A finding's record reference. Task 5 resolves a URL that is not under
    /// this repository's current name by its node id (spec §2.3).
    fn current_ref(&self, r: &RecordRef) -> Result<Iri, StoreError> {
        Ok(r.id.clone())
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
}

impl Tracker for GithubTracker {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        let meta = Meta::new(ItemKind::Record, State::Todo.as_wire(), project.clone());
        Ok(RecordId(
            self.create(ItemKind::Record, title, "", &meta)?.url,
        ))
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        match self.item(id.iri(), ItemKind::Record)? {
            Found::Item(issue, meta, _) => self.record_from(&issue, &meta).map(Some),
            Found::OtherKind(_) | Found::Absent => Ok(None),
        }
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        let mut out = Vec::new();
        for (issue, meta, _) in self.list(ItemKind::Record, None)? {
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
        let record = match self.item(finding.record.iri(), ItemKind::Record)? {
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
        match self.item(id.iri(), ItemKind::Finding)? {
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
        for (issue, meta, prose) in self.list(ItemKind::Finding, None)? {
            if meta.project == *project {
                out.push(self.finding_from(&issue, &meta, &prose)?);
            }
        }
        Ok(out)
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        let withdrawn = self.list(ItemKind::Finding, Some(FindingState::Withdrawn.as_wire()))?;
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
}
