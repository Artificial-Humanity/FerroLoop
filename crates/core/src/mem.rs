use crate::agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent,
    RegistryEventKind, validate_agent_identity,
};
use crate::at::At;
use crate::escalation::{EscalationFault, Escalations, Mark, Tombstone, escalate_command};
use crate::ferrowire::{
    DeliveryReceipt, DeliveryStatus, MessageKind, SendWireArgs, ThreadId, WireFilter,
    WireImportRejectReason, WireImportReport, WireInboxItem, WireMessage, check_group_address,
    resolve_recipients, validate_sender_name, validate_subject, validate_wire_body,
};
use crate::finding::{Finding, FindingState};
use crate::ids::{
    AgentId, FindingId, GateId, Kind, MessageId, ProjectId, RecordId, RevisionId, StateId,
    WorkflowId, WorkflowRunId, seq_iri,
};
use crate::iri::Iri;
use crate::log::{Attempt, GateRun};
use crate::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
use crate::routing::{ForeignRecord, Routes, RoutingMap, check_foreign_for_local};
use crate::split::{CachedSegment, LedgerCache, Outbox, Pending};
use crate::state::{
    MemoryLifecycle, MergeMode, SensitivityPatch, StateConcern, StateConflict, StateEntry,
    StateError, StatePayload, StateRevision, StateScope, StateSyncCursor, UpsertStateArgs,
    compute_append_merge, compute_last_writer_merge, is_ancestor_in_dag,
    lowest_common_ancestor_in_dag, sorted_parents, validate_slug,
};
use crate::store::{
    AgentRegistry, Catalog, Handles, Ledger, MessageStore, StateStore, StoreError, Tracker,
    WorkflowStore,
};
use crate::workflow::{WorkflowDef, WorkflowRun, validate_workflow_def};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

/// An in-memory store for tests. Deliberately lives in `fl-core` so the
/// engine's own tests need no backend at all. Backs all three roles.
///
/// Ids are minted with [`seq_iri`], not UUIDv7: this store must not use a
/// clock or randomness, so its ids are deterministic.
#[derive(Default)]
pub struct MemStore {
    inner: RefCell<Inner>,
}

const LABEL: &str = "memory";

#[derive(Default)]
struct Inner {
    next_id: u64,
    /// Every id this store ever minted, with its kind. Append-only: nothing
    /// removes an entry. Ownership is membership (spec §2.6).
    owned: BTreeMap<Iri, Kind>,
    next_handle: BTreeMap<Kind, u64>,
    handles: BTreeMap<(Kind, u64), Iri>,
    handle_of: BTreeMap<Iri, u64>,
    projects: BTreeMap<Iri, Project>,
    gates: BTreeMap<Iri, GateDef>,
    transitions: BTreeMap<(Iri, String), Transition>,
    records: BTreeMap<Iri, Record>,
    runs: Vec<GateRun>,
    attempts: Vec<Attempt>,
    findings: BTreeMap<Iri, Finding>,
    /// project → its routing map (routing spec §1.2).
    routing: BTreeMap<Iri, RoutingMap>,

    /// alias → primary. `"alias"` is not a `Kind`: it is an index marker, so
    /// an alias never enters `owned`. `check` follows it before deciding.
    aliases: BTreeMap<Iri, Iri>,
    /// configured `owner/repo` (lowercase) → the repository's `node_id`.
    bindings: BTreeMap<String, String>,
    /// (repository `node_id`, entry id) for every entry marked published.
    published: BTreeSet<(String, Iri)>,
    /// Every entry published to any repository, or set aside: it waits no
    /// more (spec §2.1).
    settled: BTreeSet<Iri>,
    /// repository `node_id` → the id after which entries are publishable.
    cutovers: BTreeMap<String, Iri>,
    /// repository `node_id` → its GitHub ledger's first commit.
    ledger_roots: BTreeMap<String, String>,
    /// repository `node_id` → the last head of its ledger this machine checked.
    heads: BTreeMap<String, String>,
    /// (repository `node_id`, path on the branch) → that file as last read.
    segments: BTreeMap<(String, String), CachedSegment>,
    /// primary → the mark of an escalation under way (routing spec §3.3).
    escalating: BTreeMap<Iri, Mark>,
    /// primary → the tombstone of an escalated item. The item's row, handle
    /// and aliases stay: its id and handle still resolve here (§2.3).
    tombstones: BTreeMap<Iri, Tombstone>,

    areas: BTreeMap<String, AreaRecord>,
    agents: BTreeMap<AgentId, AgentRecord>,
    agent_by_name: BTreeMap<String, AgentId>,
    registry_events: BTreeMap<u64, RegistryEvent>,
    registry_event_ids: BTreeMap<Iri, u64>,
    next_registry_seq: u64,

    state_entries: BTreeMap<StateId, StateEntry>,
    state_by_key: BTreeMap<(String, String, String), StateId>,
    state_revisions: BTreeMap<u64, StateRevision>,
    state_revision_ids: BTreeMap<RevisionId, u64>,
    next_state_rev_seq: u64,
    state_conflicts: BTreeMap<StateId, StateConflict>,
    sync_cursors: BTreeMap<String, StateSyncCursor>,

    wire_messages: BTreeMap<MessageId, WireMessage>,
    wire_msg_log: BTreeMap<u64, MessageId>,
    wire_msg_seq: BTreeMap<MessageId, u64>,
    next_wire_msg_seq: u64,
    wire_inbox: BTreeMap<(Iri, MessageId), DeliveryReceipt>,
    wire_rcpt_log: BTreeMap<u64, DeliveryReceipt>,
    next_wire_rcpt_seq: u64,
    wire_threads: BTreeSet<(ThreadId, MessageId)>,
    wire_replies: BTreeSet<(MessageId, MessageId)>,

    delivery: ManagedMemory,

    workflow_defs: BTreeMap<WorkflowId, WorkflowDef>,
    workflow_by_slug: BTreeMap<(ProjectId, String), WorkflowId>,
    workflow_runs: BTreeMap<WorkflowRunId, WorkflowRun>,
}

impl Inner {
    fn mint(&mut self, kind: Kind) -> Iri {
        self.next_id += 1;
        let id = seq_iri(self.next_id);
        self.owned.insert(id.clone(), kind);
        let n = self.next_handle.entry(kind).or_insert(0);
        *n += 1;
        self.handles.insert((kind, *n), id.clone());
        self.handle_of.insert(id.clone(), *n);
        id
    }

    fn mint_unhanded(&mut self, kind: Kind) -> Iri {
        self.next_id += 1;
        let id = seq_iri(self.next_id);
        self.owned.insert(id.clone(), kind);
        id
    }

    fn mint_unindexed(&mut self) -> Iri {
        self.next_id += 1;
        seq_iri(self.next_id)
    }

    /// Register an imported entity IRI in `owned`, `handles`, and `handle_of` if not already known.
    fn index_imported(&mut self, kind: Kind, id: &Iri) -> Result<(), StoreError> {
        let canonical = self.resolve(id);
        if let Some(&existing) = self.owned.get(&canonical) {
            if existing == kind {
                return Ok(());
            }
            return Err(StoreError::WrongKind {
                id: canonical,
                expected: kind,
                found: existing,
            });
        }
        self.owned.insert(canonical.clone(), kind);
        let n = self.next_handle.entry(kind).or_insert(0);
        *n += 1;
        self.handles.insert((kind, *n), canonical.clone());
        self.handle_of.insert(canonical, *n);
        Ok(())
    }

    /// Register an imported `Kind::Message` IRI in `owned` without allocating a numeric handle.
    fn index_imported_unhanded(&mut self, kind: Kind, id: &Iri) -> Result<(), StoreError> {
        let canonical = self.resolve(id);
        if let Some(&existing) = self.owned.get(&canonical) {
            if existing == kind {
                return Ok(());
            }
            return Err(StoreError::WrongKind {
                id: canonical,
                expected: kind,
                found: existing,
            });
        }
        self.owned.insert(canonical, kind);
        Ok(())
    }

    fn append_wire_message(&mut self, msg: WireMessage) {
        if !self.wire_msg_seq.contains_key(&msg.id) {
            self.next_wire_msg_seq += 1;
            let seq = self.next_wire_msg_seq;
            self.wire_msg_seq.insert(msg.id.clone(), seq);
            self.wire_msg_log.insert(seq, msg.id.clone());
        }
        self.wire_threads
            .insert((msg.thread_id.clone(), msg.id.clone()));
        if let Some(ref corr) = msg.correlation_id {
            self.wire_replies.insert((corr.clone(), msg.id.clone()));
        }
        self.wire_messages.insert(msg.id.clone(), msg);
    }

    fn append_wire_receipt(&mut self, rcpt: DeliveryReceipt) {
        self.next_wire_rcpt_seq += 1;
        let seq = self.next_wire_rcpt_seq;
        self.wire_rcpt_log.insert(seq, rcpt);
    }

    fn recipient_matches(&self, rcpt: &DeliveryReceipt, query: &str) -> bool {
        let trimmed = query.trim();
        let (agent_id, name_key) = if let Ok(iri) = Iri::parse(trimmed) {
            let resolved_iri = self.resolve(&iri);
            if let Some(agent) = self.agents.get(&AgentId(resolved_iri.clone())) {
                (Some(resolved_iri), Some(agent.name_key.clone()))
            } else if let Some(rest) = trimmed.strip_prefix("urn:fl:agent:")
                && let Ok(k) = validate_slug(rest)
            {
                let aid = self.agent_by_name.get(&k).map(|id| self.resolve(id.iri()));
                (aid, Some(k))
            } else {
                (Some(resolved_iri), None)
            }
        } else if let Ok(k) = validate_slug(trimmed) {
            let aid = self.agent_by_name.get(&k).map(|id| self.resolve(id.iri()));
            (aid, Some(k))
        } else {
            (None, None)
        };

        let rcpt_resolved = self.resolve(&rcpt.recipient_id);
        if let Some(ref aid) = agent_id
            && &rcpt_resolved == aid
        {
            return true;
        }
        if let Some(ref nk) = name_key
            && (&rcpt.recipient_name == nk
                || rcpt.recipient_id.as_str() == format!("urn:fl:agent:{nk}"))
        {
            return true;
        }
        rcpt.recipient_id.as_str() == trimmed || rcpt.recipient_name.eq_ignore_ascii_case(trimmed)
    }

    /// Re-point an unpublished local `old_id` to `new_id` of the same `kind`,
    /// preserving any existing numeric handle on `old_id` (spec §3.2, §4.3).
    fn alias_unpublished_id(
        &mut self,
        kind: Kind,
        old_id: &Iri,
        new_id: &Iri,
    ) -> Result<(), StoreError> {
        if old_id == new_id {
            return Ok(());
        }
        if let Some(&existing) = self.owned.get(old_id)
            && existing != kind
        {
            return Err(StoreError::WrongKind {
                id: old_id.clone(),
                expected: kind,
                found: existing,
            });
        }
        self.owned.remove(old_id);
        self.aliases.insert(old_id.clone(), new_id.clone());
        for target in self.aliases.values_mut() {
            if target == old_id {
                *target = new_id.clone();
            }
        }
        self.owned.insert(new_id.clone(), kind);
        if let Some(h) = self.handle_of.remove(old_id) {
            self.handles.insert((kind, h), new_id.clone());
            self.handle_of.insert(new_id.clone(), h);
        } else if !self.handle_of.contains_key(new_id) {
            let n = self.next_handle.entry(kind).or_insert(0);
            *n += 1;
            self.handles.insert((kind, *n), new_id.clone());
            self.handle_of.insert(new_id.clone(), *n);
        }
        Ok(())
    }

    fn append_registry_event(&mut self, ev: RegistryEvent) {
        self.next_registry_seq += 1;
        let seq = self.next_registry_seq;
        self.registry_event_ids.insert(ev.event_id.clone(), seq);
        self.registry_events.insert(seq, ev);
    }

    fn append_state_revision(&mut self, rev: StateRevision) {
        self.next_state_rev_seq += 1;
        let seq = self.next_state_rev_seq;
        self.state_revision_ids.insert(rev.revision_id.clone(), seq);
        self.state_revisions.insert(seq, rev);
    }

    fn revisions_for_entry(&self, entry_id: &StateId) -> BTreeMap<RevisionId, StateRevision> {
        self.state_revisions
            .values()
            .filter(|r| &r.entry_id == entry_id)
            .map(|r| (r.revision_id.clone(), r.clone()))
            .collect()
    }

    fn apply_imported_state_revision(
        &mut self,
        rev: StateRevision,
        merge_at: At,
    ) -> Result<(), StoreError> {
        rev.payload
            .validate(&rev.entry_id)
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;
        for parent in &rev.parent_revisions {
            if !self.state_revision_ids.contains_key(parent) {
                return Err(StoreError::InvalidState(format!(
                    "revision `{}` references unknown parent `{}`",
                    rev.revision_id, parent
                )));
            }
        }
        if let StatePayload::Memory(m) = &rev.payload
            && let Some(target_id) = &m.superseded_by
        {
            let resolved_target = StateId(self.resolve(target_id.iri()));
            match self.state_entries.get(&resolved_target) {
                Some(target) if target.concern == StateConcern::Memory => {}
                _ => {
                    return Err(StoreError::InvalidState(format!(
                        "superseded_by target `{target_id}` does not exist as a Memory entry"
                    )));
                }
            }
        }

        let key = (
            rev.scope.as_wire(),
            rev.concern.as_wire().to_string(),
            rev.slug.clone(),
        );

        // Check offline same-key collision (`S_local != S_remote`).
        if let Some(existing_id) = self.state_by_key.get(&key).cloned()
            && existing_id != rev.entry_id
        {
            self.alias_unpublished_id(Kind::State, existing_id.iri(), rev.entry_id.iri())?;
            if let Some(mut local_entry) = self.state_entries.remove(&existing_id) {
                local_entry.id = rev.entry_id.clone();
                self.state_entries.insert(rev.entry_id.clone(), local_entry);
            }
            if let Some(mut conflict) = self.state_conflicts.remove(&existing_id) {
                conflict.entry_id = rev.entry_id.clone();
                self.state_conflicts.insert(rev.entry_id.clone(), conflict);
            }
            for stored_rev in self.state_revisions.values_mut() {
                if stored_rev.entry_id == existing_id {
                    stored_rev.entry_id = rev.entry_id.clone();
                }
            }
            self.state_by_key.insert(key.clone(), rev.entry_id.clone());
        }

        let resolved_entry_id = StateId(self.resolve(rev.entry_id.iri()));
        let mut rev = rev;
        rev.entry_id = resolved_entry_id.clone();
        self.index_imported(Kind::State, rev.entry_id.iri())?;

        // Check immutability if entry already exists.
        if let Some(existing) = self.state_entries.get(&resolved_entry_id)
            && (existing.scope != rev.scope
                || existing.concern != rev.concern
                || existing.slug != rev.slug)
        {
            return Err(StoreError::Tampered {
                id: resolved_entry_id.0,
                detail: "revision changed immutable (scope, concern, slug) of existing state entry"
                    .into(),
            });
        }

        self.append_state_revision(rev.clone());

        let Some(existing) = self.state_entries.get(&resolved_entry_id).cloned() else {
            let entry = StateEntry {
                id: resolved_entry_id.clone(),
                scope: rev.scope,
                concern: rev.concern,
                slug: rev.slug,
                sensitive: rev.sensitive,
                ever_sensitive: rev.ever_sensitive || rev.sensitive,
                payload: rev.payload,
                head_revision: rev.revision_id,
                updated_at: rev.at,
                by: rev.by,
            };
            self.state_by_key.insert(key, resolved_entry_id.clone());
            self.state_entries.insert(resolved_entry_id, entry);
            return Ok(());
        };

        let dag = self.revisions_for_entry(&resolved_entry_id);
        let r_local = existing.head_revision.clone();
        let r_remote = rev.revision_id.clone();

        // Case 1: Remote is descendant of local -> fast-forward.
        if is_ancestor_in_dag(&r_local, &r_remote, &dag) {
            let updated = StateEntry {
                id: resolved_entry_id.clone(),
                scope: existing.scope,
                concern: existing.concern,
                slug: existing.slug,
                sensitive: rev.sensitive,
                ever_sensitive: existing.ever_sensitive || rev.ever_sensitive || rev.sensitive,
                payload: rev.payload,
                head_revision: r_remote.clone(),
                updated_at: rev.at,
                by: rev.by,
            };
            self.state_entries
                .insert(resolved_entry_id.clone(), updated);
            if let Some(c) = self.state_conflicts.get(&resolved_entry_id)
                && is_ancestor_in_dag(&c.local_head, &r_remote, &dag)
                && is_ancestor_in_dag(&c.remote_head, &r_remote, &dag)
            {
                self.state_conflicts.remove(&resolved_entry_id);
            }
            return Ok(());
        }

        // Case 2: Local is already descendant of remote -> no-op.
        if is_ancestor_in_dag(&r_remote, &r_local, &dag) {
            return Ok(());
        }

        // Case 3: True divergence between r_local and r_remote.
        let local_rev = dag.get(&r_local).cloned().ok_or_else(|| {
            StoreError::InvalidState(format!("missing local head revision `{r_local}`"))
        })?;
        let base_rev = lowest_common_ancestor_in_dag(&r_local, &r_remote, &dag)
            .and_then(|id| dag.get(&id).cloned());

        let auto_merged: Option<StatePayload> = match (&local_rev.payload, &rev.payload) {
            (StatePayload::Runtime(l_rt), StatePayload::Runtime(r_rt))
                if l_rt.merge_mode == r_rt.merge_mode =>
            {
                match l_rt.merge_mode {
                    MergeMode::Conflict => None,
                    MergeMode::Append => {
                        let base_arr = match base_rev.as_ref().map(|b| &b.payload) {
                            Some(StatePayload::Runtime(b_rt)) => b_rt.value.as_array().cloned(),
                            _ => Some(Vec::new()),
                        };
                        match (base_arr, l_rt.value.as_array(), r_rt.value.as_array()) {
                            (Some(b), Some(l), Some(r)) => {
                                Some(StatePayload::Runtime(crate::state::RuntimeEntry {
                                    value: serde_json::Value::Array(compute_append_merge(b, l, r)),
                                    merge_mode: MergeMode::Append,
                                }))
                            }
                            _ => None,
                        }
                    }
                    MergeMode::LastWriter => {
                        let win_payload =
                            compute_last_writer_merge(&local_rev, &rev).payload.clone();
                        Some(win_payload)
                    }
                }
            }
            _ => None,
        };

        if let Some(merged_payload) = auto_merged {
            let merge_rev_id = RevisionId(self.mint_unindexed());
            let sensitive = local_rev.sensitive || rev.sensitive;
            let ever_sensitive = existing.ever_sensitive
                || local_rev.ever_sensitive
                || rev.ever_sensitive
                || sensitive;
            let merge_rev = StateRevision {
                revision_id: merge_rev_id.clone(),
                entry_id: resolved_entry_id.clone(),
                parent_revisions: sorted_parents(r_local, r_remote),
                scope: existing.scope.clone(),
                concern: existing.concern,
                slug: existing.slug.clone(),
                sensitive,
                ever_sensitive,
                payload: merged_payload.clone(),
                at: merge_at.clone(),
                by: "fl".to_string(),
            };
            self.append_state_revision(merge_rev);
            let updated = StateEntry {
                id: resolved_entry_id.clone(),
                scope: existing.scope,
                concern: existing.concern,
                slug: existing.slug,
                sensitive,
                ever_sensitive,
                payload: merged_payload,
                head_revision: merge_rev_id,
                updated_at: merge_at,
                by: "fl".to_string(),
            };
            self.state_entries.insert(resolved_entry_id, updated);
        } else {
            self.state_conflicts.insert(
                resolved_entry_id.clone(),
                StateConflict {
                    entry_id: resolved_entry_id,
                    local_head: r_local,
                    remote_head: r_remote,
                    detected_at: merge_at,
                },
            );
        }
        Ok(())
    }

    /// ⚠ Every method that takes an id asks this first — list methods too.
    /// A list over a project this store never held is "didn't look", and an
    /// empty list would say "looked, found nothing".
    ///
    /// Follows one alias hop: an alias is never in `owned`, so this looks it
    /// up in `aliases` and reports the PRIMARY's kind. This is what makes an
    /// alias "owned" for every other check in this module.
    fn check(&self, id: &Iri) -> Result<Kind, StoreError> {
        if let Some(kind) = self.owned.get(id) {
            return Ok(*kind);
        }
        if let Some(kind) = self
            .aliases
            .get(id)
            .and_then(|primary| self.owned.get(primary))
        {
            return Ok(*kind);
        }
        Err(StoreError::NotOwned {
            id: id.clone(),
            searched: vec![LABEL.to_string()],
        })
    }

    /// `check`, and then refuse an id held under any kind but `expected`
    /// with `WrongKind`. Every method that takes a project (and
    /// `add_finding`'s record) asks this rather than `check`: answering an
    /// empty list for a gate's IRI would say "looked, found nothing" about
    /// an item that was never a project.
    fn check_kind(&self, id: &Iri, expected: Kind) -> Result<(), StoreError> {
        let found = self.check(id)?;
        if found != expected {
            return Err(StoreError::WrongKind {
                id: id.clone(),
                expected,
                found,
            });
        }
        Ok(())
    }

    /// `id` itself, or the primary it aliases. Used wherever a lookup needs
    /// the key a row is actually stored under — `check` alone answers
    /// ownership, not which key to read.
    fn resolve(&self, id: &Iri) -> Iri {
        self.aliases.get(id).cloned().unwrap_or_else(|| id.clone())
    }

    /// Resolve `id` to its primary IRI, returning `WrongKind` if `id` is owned
    /// under any kind other than `expected`, or `Ok(id.clone())` if not owned.
    fn resolve_if_kind(&self, id: &Iri, expected: Kind) -> Result<Iri, StoreError> {
        match self.check(id) {
            Ok(found) if found != expected => Err(StoreError::WrongKind {
                id: id.clone(),
                expected,
                found,
            }),
            Ok(_) => Ok(self.resolve(id)),
            Err(StoreError::NotOwned { .. }) => Ok(id.clone()),
            Err(e) => Err(e),
        }
    }

    /// `check`, then the primary `id` names and its kind — unless the item
    /// was escalated: a tombstoned item reads as `Escalated`, under every
    /// name it has (routing spec §3.6).
    fn live(&self, id: &Iri) -> Result<(Iri, Kind), StoreError> {
        let kind = self.check(id)?;
        let primary = self.resolve(id);
        if let Some(t) = self.tombstones.get(&primary) {
            return Err(StoreError::Escalated {
                from: primary,
                to: t.to.clone(),
            });
        }
        Ok((primary, kind))
    }

    /// `live`, and then refuse an item marked escalating: a write would
    /// change the copy the escalation is moving (routing spec §3.3 step 1).
    fn writable(&self, id: &Iri) -> Result<Iri, StoreError> {
        let (primary, kind) = self.live(id)?;
        if self.escalating.contains_key(&primary) {
            return Err(StoreError::Escalating {
                to_finish: escalate_command(kind, &primary),
                id: primary,
            });
        }
        Ok(primary)
    }
}

impl Catalog for MemStore {
    fn add_project(&self, root: &str) -> Result<ProjectId, StoreError> {
        let mut s = self.inner.borrow_mut();
        let id = ProjectId(s.mint(Kind::Project));
        s.projects.insert(
            id.0.clone(),
            Project {
                id: id.clone(),
                root: root.to_string(),
            },
        );
        Ok(id)
    }

    fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError> {
        let s = self.inner.borrow();
        s.check(&id.0)?;
        Ok(s.projects.get(&id.0).cloned())
    }

    fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        Ok(self.inner.borrow().projects.values().cloned().collect())
    }

    fn set_project_root(&self, id: &ProjectId, root: &str) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&id.0, Kind::Project)?;
        match s.projects.get_mut(&id.0) {
            Some(p) => {
                p.root = root.to_string();
                Ok(())
            }
            None => Err(StoreError::InvalidState(format!("no project {}", id.0))),
        }
    }

    fn remove_project(&self, id: &ProjectId) -> Result<bool, StoreError> {
        {
            let s = self.inner.borrow();
            s.check_kind(&id.0, Kind::Project)?;
            if !s.projects.contains_key(&id.0) {
                return Ok(false);
            }
        }
        crate::store::check_project_removable(self, id)?;
        Ok(self.inner.borrow_mut().projects.remove(&id.0).is_some())
    }

    fn add_gate(
        &self,
        project: &ProjectId,
        name: &str,
        kind: GateKind,
        selector: Selector,
        min_population: u64,
        authored_at_commit: &str,
        authored_by: &str,
    ) -> Result<GateId, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&project.0, Kind::Project)?;
        let id = GateId(s.mint(Kind::Gate));
        s.gates.insert(
            id.0.clone(),
            GateDef {
                id: id.clone(),
                project: project.clone(),
                name: name.to_string(),
                kind,
                selector,
                min_population,
                authored_at_commit: authored_at_commit.to_string(),
                authored_by: authored_by.to_string(),
                last_pass_commit: None,
            },
        );
        Ok(id)
    }

    fn get_gate(&self, id: &GateId) -> Result<Option<GateDef>, StoreError> {
        let s = self.inner.borrow();
        s.check(&id.0)?;
        Ok(s.gates.get(&id.0).cloned())
    }

    fn list_gates(&self, project: &ProjectId) -> Result<Vec<GateDef>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.gates
            .values()
            .filter(|g| g.project == *project)
            .cloned()
            .collect())
    }

    fn update_gate(&self, def: &GateDef) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check(&def.id.0)?;
        if !s.gates.contains_key(&def.id.0) {
            return Err(StoreError::NoSuchGate(def.id.clone()));
        }
        s.gates.insert(def.id.0.clone(), def.clone());
        Ok(())
    }

    fn add_transition(&self, t: Transition) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&t.project.0, Kind::Project)?;
        s.transitions
            .insert((t.project.0.clone(), t.name.clone()), t);
        Ok(())
    }

    fn get_transition(
        &self,
        project: &ProjectId,
        name: &str,
    ) -> Result<Option<Transition>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.transitions
            .get(&(project.0.clone(), name.to_string()))
            .cloned())
    }

    fn list_transitions(&self, project: &ProjectId) -> Result<Vec<Transition>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.transitions
            .values()
            .filter(|t| t.project == *project)
            .cloned()
            .collect())
    }

    fn kind_of(&self, id: &Iri) -> Result<Kind, StoreError> {
        self.inner.borrow().check(id)
    }
}

impl MemStore {
    /// A store whose first minted id is `seq_iri(first)`, the next
    /// `seq_iri(first + 1)`, and so on: still deterministic for a given
    /// `first`. For a test against a real service that keeps what earlier
    /// runs wrote, so each run's ids are its own. Handles still start at 1.
    /// Not in the binary: the `conformance` feature is a dev-dependency only.
    #[cfg(any(test, feature = "conformance"))]
    #[doc(hidden)]
    pub fn starting_at(first: u64) -> Self {
        let store = Self::default();
        store.inner.borrow_mut().next_id = first.saturating_sub(1);
        store
    }

    /// Write `project`'s routing map. A map that is not one fl writes is
    /// refused (`RoutingMap::check`).
    pub fn set_routes(&self, project: &ProjectId, map: &RoutingMap) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&project.0, Kind::Project)?;
        map.check()
            .map_err(|why| StoreError::Backend(format!("the routing map is not valid: {why}")))?;
        s.routing.insert(project.0.clone(), map.clone());
        Ok(())
    }
}

impl Routes for MemStore {
    fn routes(&self, project: &ProjectId) -> Result<Option<RoutingMap>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.routing.get(&project.0).cloned())
    }
}

impl Tracker for MemStore {
    fn add_record_with_area(
        &self,
        project: &ProjectId,
        title: &str,
        area: Option<&str>,
    ) -> Result<RecordId, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&project.0, Kind::Project)?;
        let id = RecordId(s.mint(Kind::Record));
        s.records.insert(
            id.0.clone(),
            Record {
                id: id.clone(),
                project: project.clone(),
                title: title.to_string(),
                state: State::Todo,
                also_known_as: vec![],
                area: area.map(str::to_string),
            },
        );
        Ok(id)
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        let s = self.inner.borrow();
        let (target, _) = s.live(&id.0)?;
        Ok(s.records.get(&target).cloned())
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.records
            .values()
            .filter(|r| r.project == *project && !s.tombstones.contains_key(r.id.iri()))
            .cloned()
            .collect())
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        // `id` may be an alias: resolve to the primary key `records` is
        // actually keyed by.
        let target = s.writable(&id.0)?;
        let rec = s
            .records
            .get_mut(&target)
            .ok_or_else(|| StoreError::NoSuchRecord(id.clone()))?;
        rec.state = state;
        Ok(())
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&finding.project.0, Kind::Project)?;
        s.check_kind(&finding.record.0, Kind::Record)?;
        // `record` may have been given as an alias: resolve to the primary,
        // so two findings raised against the same record always agree on
        // which IRI names it. A marked record takes findings (routing spec
        // §3.3 step 1); an escalated one is `Escalated`.
        let (record_primary, _) = s.live(&finding.record.0)?;
        let id = FindingId(s.mint(Kind::Finding));
        let mut finding = finding;
        finding.id = id.clone();
        finding.record = RecordId(record_primary);
        s.findings.insert(id.0.clone(), finding);
        Ok(id)
    }

    fn add_finding_checked(
        &self,
        finding: Finding,
        record: ForeignRecord,
    ) -> Result<FindingId, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(&finding.project.0, Kind::Project)?;
        check_foreign_for_local(&record, s.check(record.id().iri()).is_ok())?;
        let id = FindingId(s.mint(Kind::Finding));
        let mut finding = finding;
        finding.id = id.clone();
        finding.record = record.id().clone();
        s.findings.insert(id.0.clone(), finding);
        Ok(id)
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        let s = self.inner.borrow();
        let (target, _) = s.live(&id.0)?;
        Ok(s.findings.get(&target).cloned())
    }

    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        // `finding.id` (what the caller passed) may be an alias: resolve to
        // the primary key `findings` is actually keyed by, and pin the
        // written row's own `id` to that primary too — even if the caller's
        // struct still carries the alias — so an update through an alias
        // lands on, and stays keyed by, the primary, never a second row
        // under the alias.
        let target = s.writable(&finding.id.0)?;
        if !s.findings.contains_key(&target) {
            return Err(StoreError::NoSuchFinding(finding.id.clone()));
        }
        // The stored `also_known_as` is kept and the caller's ignored (see
        // the trait): only `add_alias` adds a name. The record, the raiser,
        // the security mark and the area are fixed when the finding is
        // raised (routing spec §1.1; GitHub tracker spec §6).
        let kept = s.findings[&target].clone();
        let mut stored = finding.clone();
        stored.id = FindingId(target.clone());
        stored.also_known_as = kept.also_known_as;
        stored.record = kept.record;
        stored.raised_by = kept.raised_by;
        stored.security = kept.security;
        stored.area = kept.area;
        s.findings.insert(target, stored);
        Ok(())
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.findings
            .values()
            .filter(|f| f.project == *project && !s.tombstones.contains_key(f.id.iri()))
            .cloned()
            .collect())
    }

    /// An escalated finding is counted by the tier it lives in now.
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        let s = self.inner.borrow();
        Ok(s.findings
            .values()
            .filter(|f| f.raised_by == actor && f.state == FindingState::Withdrawn)
            .filter(|f| !s.tombstones.contains_key(f.id.iri()))
            .count() as u64)
    }

    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        // `alias` must be unused, whether as a primary id or as another
        // alias: this store has exactly one id namespace.
        if s.owned.contains_key(&alias) || s.aliases.contains_key(&alias) {
            return Err(StoreError::AlreadyExists(alias));
        }
        // `primary` may itself be an alias; resolve to the true primary so
        // `aliases` never chains and the row update below finds the row.
        let resolved = s.writable(primary)?;
        let kind = s.check(&resolved)?;
        match kind {
            Kind::Record => {
                s.records
                    .get_mut(&resolved)
                    .expect("a Record kind means records holds this row")
                    .also_known_as
                    .push(alias.clone());
            }
            Kind::Finding => {
                s.findings
                    .get_mut(&resolved)
                    .expect("a Finding kind means findings holds this row")
                    .also_known_as
                    .push(alias.clone());
            }
            other => {
                return Err(StoreError::Backend(format!(
                    "{resolved} is a {}, and only a record or finding can carry an alias",
                    other.as_wire()
                )));
            }
        }
        s.aliases.insert(alias, resolved);
        Ok(())
    }
}

impl Escalations for MemStore {
    fn mark(&self, id: &Iri, mark: &Mark) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        let (primary, kind) = s.live(id)?;
        if !matches!(kind, Kind::Record | Kind::Finding) {
            return Err(StoreError::WrongKind {
                id: id.clone(),
                expected: Kind::Record,
                found: kind,
            });
        }
        if s.escalating.contains_key(&primary) {
            return Err(EscalationFault::AlreadyMarked { id: primary, kind }.into());
        }
        s.escalating.insert(primary, mark.clone());
        Ok(())
    }

    fn mark_of(&self, id: &Iri) -> Result<Option<Mark>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.escalating.get(&s.resolve(id)).cloned())
    }

    fn unmark(&self, id: &Iri) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check(id)?;
        let primary = s.resolve(id);
        if s.escalating.remove(&primary).is_none() {
            return Err(EscalationFault::NotMarked { id: primary }.into());
        }
        Ok(())
    }

    fn tombstone(&self, id: &Iri, to: &Iri) -> Result<Tombstone, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check(id)?;
        let primary = s.resolve(id);
        let Some(mark) = s.escalating.remove(&primary) else {
            return Err(EscalationFault::NotMarked { id: primary }.into());
        };
        let tomb = Tombstone {
            from: primary.clone(),
            to: to.clone(),
            by: mark.by,
            reason: mark.reason,
            at_ms: mark.at_ms,
        };
        s.tombstones.insert(primary, tomb.clone());
        Ok(tomb)
    }

    fn tombstone_of(&self, id: &Iri) -> Result<Option<Tombstone>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.tombstones.get(&s.resolve(id)).cloned())
    }
}

impl Ledger for MemStore {
    // `append_*` checks nothing (spec §3.4): the engine already resolved the
    // references it is recording.
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError> {
        self.inner.borrow_mut().runs.push(run);
        Ok(())
    }

    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError> {
        self.inner.borrow_mut().attempts.push(attempt);
        Ok(())
    }

    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError> {
        let s = self.inner.borrow();
        s.check(&gate.0)?;
        Ok(s.runs.iter().filter(|r| r.gate == *gate).cloned().collect())
    }

    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(&project.0, Kind::Project)?;
        Ok(s.attempts
            .iter()
            .filter(|a| a.project == *project)
            .cloned()
            .collect())
    }
}

impl Outbox for MemStore {
    fn unpublished(&self, _repo: &str, after: &Iri) -> Result<Pending, StoreError> {
        let s = self.inner.borrow();
        let waiting = |id: &Option<Iri>| {
            id.as_ref()
                .is_some_and(|id| id > after && !s.settled.contains(id))
        };
        let mut runs: Vec<GateRun> = s
            .runs
            .iter()
            .filter(|r| r.record.is_some() && waiting(&r.id))
            .cloned()
            .collect();
        let mut attempts: Vec<Attempt> = s
            .attempts
            .iter()
            .filter(|a| waiting(&a.id))
            .cloned()
            .collect();
        runs.sort_by(|a, b| a.id.cmp(&b.id));
        attempts.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Pending { runs, attempts })
    }

    fn is_published(&self, repo: &str, id: &Iri) -> Result<bool, StoreError> {
        Ok(self
            .inner
            .borrow()
            .published
            .contains(&(repo.to_string(), id.clone())))
    }

    fn mark_published(&self, repo: &str, ids: &[Iri]) -> Result<(), StoreError> {
        self.settle(repo, ids, &[])
    }

    fn settle(&self, repo: &str, published: &[Iri], set_aside: &[Iri]) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        for id in published {
            s.published.insert((repo.to_string(), id.clone()));
        }
        s.settled.extend(published.iter().chain(set_aside).cloned());
        Ok(())
    }

    fn cutover(&self, repo: &str) -> Result<Option<Iri>, StoreError> {
        Ok(self.inner.borrow().cutovers.get(repo).cloned())
    }

    fn set_cutover(&self, repo: &str, id: &Iri) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        match s.cutovers.get(repo) {
            Some(held) if held == id => Ok(()),
            Some(held) => Err(StoreError::CutoverChanged {
                node_id: repo.to_string(),
                held: held.clone(),
                found: id.clone(),
            }),
            None => {
                s.cutovers.insert(repo.to_string(), id.clone());
                Ok(())
            }
        }
    }
}

impl LedgerCache for MemStore {
    fn last_head(&self, repo: &str) -> Result<Option<String>, StoreError> {
        Ok(self.inner.borrow().heads.get(repo).cloned())
    }

    fn cached(&self, repo: &str, path: &str) -> Result<Option<CachedSegment>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .segments
            .get(&(repo.to_string(), path.to_string()))
            .cloned())
    }

    fn cached_under(
        &self,
        repo: &str,
        dir: &str,
    ) -> Result<Vec<(String, CachedSegment)>, StoreError> {
        let prefix = format!("{dir}/");
        Ok(self
            .inner
            .borrow()
            .segments
            .iter()
            .filter(|((r, p), _)| r == repo && p.starts_with(&prefix))
            .map(|((_, p), c)| (p.clone(), c.clone()))
            .collect())
    }

    fn remember(
        &self,
        repo: &str,
        head: &str,
        segments: &[(String, CachedSegment)],
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.borrow_mut();
        inner.heads.insert(repo.to_string(), head.to_string());
        for (path, segment) in segments {
            inner
                .segments
                .insert((repo.to_string(), path.clone()), segment.clone());
        }
        Ok(())
    }
}

impl Handles for MemStore {
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError> {
        let s = self.inner.borrow();
        Ok(match s.owned.get(id) {
            Some(k) if *k == kind => s.handle_of.get(id).copied(),
            _ => None,
        })
    }

    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError> {
        Ok(self.inner.borrow().handles.get(&(kind, handle)).cloned())
    }
}

impl crate::store::Bindings for MemStore {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .bindings
            .get(&repo.to_ascii_lowercase())
            .cloned())
    }
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError> {
        self.inner
            .borrow_mut()
            .bindings
            .insert(repo.to_ascii_lowercase(), node_id.to_string());
        Ok(())
    }
    fn ledger_root(&self, node_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self.inner.borrow().ledger_roots.get(node_id).cloned())
    }
    fn set_ledger_root(&self, node_id: &str, commit: &str) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        match s.ledger_roots.get(node_id) {
            Some(held) if held == commit => Ok(()),
            Some(held) => Err(StoreError::LedgerRootChanged {
                node_id: node_id.to_string(),
                held: held.clone(),
                found: commit.to_string(),
            }),
            None => {
                s.ledger_roots
                    .insert(node_id.to_string(), commit.to_string());
                Ok(())
            }
        }
    }
}

impl AgentRegistry for MemStore {
    fn upsert_area(&self, area: AreaRecord) -> Result<AreaRecord, StoreError> {
        let normalized = area
            .normalized()
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let mut s = self.inner.borrow_mut();
        let ev = RegistryEvent {
            event_id: s.mint_unindexed(),
            at: normalized.updated_at.clone(),
            by: normalized.by.clone(),
            kind: RegistryEventKind::AreaUpsert(normalized.clone()),
        };
        s.areas
            .insert(normalized.area_key.clone(), normalized.clone());
        s.append_registry_event(ev);
        Ok(normalized)
    }

    fn get_area(&self, area_key: &str) -> Result<Option<AreaRecord>, StoreError> {
        let key = validate_slug(area_key).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let s = self.inner.borrow();
        Ok(s.areas.get(&key).cloned())
    }

    fn list_areas(&self) -> Result<Vec<AreaRecord>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.areas.values().cloned().collect())
    }

    fn delete_area(&self, area_key: &str, at: At, by: &str) -> Result<bool, StoreError> {
        let key = validate_slug(area_key).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let mut s = self.inner.borrow_mut();
        if s.areas.remove(&key).is_none() {
            return Ok(false);
        }
        let ev = RegistryEvent {
            event_id: s.mint_unindexed(),
            at,
            by: by.to_string(),
            kind: RegistryEventKind::AreaDelete { area_key: key },
        };
        s.append_registry_event(ev);
        Ok(true)
    }

    fn register_agent(&self, args: RegisterAgentArgs) -> Result<AgentRecord, StoreError> {
        let (name_key, display_name, email) =
            validate_agent_identity(&args.display_name, args.email.as_deref())
                .map_err(StoreError::InvalidState)?;
        let project =
            validate_slug(&args.project).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let mut extra_areas = Vec::with_capacity(args.extra_areas.len());
        for a in &args.extra_areas {
            extra_areas
                .push(validate_slug(a).map_err(|e| StoreError::InvalidState(e.to_string()))?);
        }
        extra_areas.sort();
        extra_areas.dedup();

        let mut s = self.inner.borrow_mut();

        // Area validation / creation.
        for area_key in std::iter::once(&project).chain(extra_areas.iter()) {
            if !s.areas.contains_key(area_key) {
                if args.create_area {
                    let area = AreaRecord {
                        area_key: area_key.clone(),
                        name: area_key.clone(),
                        path: None,
                        purpose: format!("Registered area {area_key}"),
                        updated_at: args.at.clone(),
                        by: args.by.clone(),
                    };
                    let ev = RegistryEvent {
                        event_id: s.mint_unindexed(),
                        at: args.at.clone(),
                        by: args.by.clone(),
                        kind: RegistryEventKind::AreaUpsert(area.clone()),
                    };
                    s.areas.insert(area_key.clone(), area);
                    s.append_registry_event(ev);
                } else {
                    return Err(StoreError::NoSuchArea(area_key.clone()));
                }
            }
        }

        // Check name collision (active or retired).
        let existing_by_name = s
            .agent_by_name
            .get(&name_key)
            .cloned()
            .and_then(|id| s.agents.get(&id).cloned());
        if let Some(ref existing) = existing_by_name
            && !args.replace
        {
            return Err(StoreError::AgentCollision {
                name_key,
                existing_id: existing.id.clone(),
                status: existing.status,
            });
        }

        // Check one-lead-per-project invariant.
        if args.role == AgentRole::Lead {
            let conflicting_lead = s
                .agents
                .values()
                .find(|a| {
                    a.status == AgentStatus::Active
                        && a.project == project
                        && a.role == AgentRole::Lead
                        && a.name_key != name_key
                })
                .cloned();
            if let Some(lead) = conflicting_lead {
                if !args.replace {
                    return Err(StoreError::LeadTaken {
                        project,
                        lead_name: lead.display_name,
                        lead_id: lead.id,
                    });
                }
                let mut retired = lead.clone();
                retired.status = AgentStatus::Retired;
                retired.updated_at = args.at.clone();
                retired.by = args.by.clone();
                s.agents.insert(retired.id.clone(), retired.clone());
                let retire_ev = RegistryEvent {
                    event_id: s.mint_unindexed(),
                    at: args.at.clone(),
                    by: args.by.clone(),
                    kind: RegistryEventKind::AgentRetire { id: retired.id },
                };
                s.append_registry_event(retire_ev);
            }
        }

        let agent_id = match existing_by_name {
            Some(existing) => existing.id,
            None => AgentId(s.mint(Kind::Agent)),
        };

        let record = AgentRecord {
            id: agent_id.clone(),
            name_key: name_key.clone(),
            display_name,
            email,
            project,
            role: args.role,
            extra_areas,
            status: AgentStatus::Active,
            updated_at: args.at.clone(),
            by: args.by.clone(),
        };
        s.agent_by_name.insert(name_key, agent_id.clone());
        s.agents.insert(agent_id, record.clone());
        let ev = RegistryEvent {
            event_id: s.mint_unindexed(),
            at: args.at,
            by: args.by,
            kind: RegistryEventKind::AgentUpsert(record.clone()),
        };
        s.append_registry_event(ev);
        Ok(record)
    }

    fn retire_agent(&self, id: &AgentId, at: At, by: &str) -> Result<AgentRecord, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(id.iri(), Kind::Agent)?;
        let resolved = AgentId(s.resolve(id.iri()));
        let Some(mut rec) = s.agents.get(&resolved).cloned() else {
            return Err(StoreError::NoSuchAgent(resolved.to_string()));
        };
        rec.status = AgentStatus::Retired;
        rec.updated_at = at.clone();
        rec.by = by.trim().to_string();
        s.agents.insert(resolved.clone(), rec.clone());
        let ev = RegistryEvent {
            event_id: s.mint_unindexed(),
            at,
            by: rec.by.clone(),
            kind: RegistryEventKind::AgentRetire { id: resolved },
        };
        s.append_registry_event(ev);
        Ok(rec)
    }

    fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>, StoreError> {
        let s = self.inner.borrow();
        s.check(id.iri())?;
        let resolved = AgentId(s.resolve(id.iri()));
        Ok(s.agents.get(&resolved).cloned())
    }

    fn agent_by_name(&self, name: &str) -> Result<Option<AgentRecord>, StoreError> {
        let key = validate_slug(name).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let s = self.inner.borrow();
        let Some(id) = s.agent_by_name.get(&key).cloned() else {
            return Ok(None);
        };
        let resolved = AgentId(s.resolve(id.iri()));
        Ok(s.agents.get(&resolved).cloned())
    }

    fn list_agents(
        &self,
        project: Option<&str>,
        include_retired: bool,
    ) -> Result<Vec<AgentRecord>, StoreError> {
        let project_filter = match project {
            Some(p) => Some(validate_slug(p).map_err(|e| StoreError::InvalidState(e.to_string()))?),
            None => None,
        };
        let s = self.inner.borrow();
        let mut out: Vec<AgentRecord> = s
            .agents
            .values()
            .filter(|a| include_retired || a.status == AgentStatus::Active)
            .filter(|a| {
                project_filter
                    .as_ref()
                    .is_none_or(|p| &a.project == p || a.extra_areas.contains(p))
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name_key.cmp(&b.name_key));
        Ok(out)
    }

    fn registry_events_after(&self, seq: u64) -> Result<Vec<(u64, RegistryEvent)>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.registry_events
            .range((std::ops::Bound::Excluded(seq), std::ops::Bound::Unbounded))
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }

    fn import_registry_events(&self, events: &[RegistryEvent]) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        for ev in events {
            if let Some(&seq) = s.registry_event_ids.get(&ev.event_id) {
                let existing = s.registry_events.get(&seq).unwrap();
                if existing != ev {
                    return Err(StoreError::Tampered {
                        id: ev.event_id.clone(),
                        detail: "registry event payload differs from stored copy".into(),
                    });
                }
                continue;
            }
            match &ev.kind {
                RegistryEventKind::AreaUpsert(area) => {
                    let norm = area
                        .normalized()
                        .map_err(|e| StoreError::InvalidState(e.to_string()))?;
                    s.areas.insert(norm.area_key.clone(), norm);
                }
                RegistryEventKind::AgentUpsert(rec) => {
                    if let Some(existing_id) = s.agent_by_name.get(&rec.name_key).cloned()
                        && existing_id != rec.id
                    {
                        s.alias_unpublished_id(Kind::Agent, existing_id.iri(), rec.id.iri())?;
                        s.agents.remove(&existing_id);
                    }
                    if rec.role == AgentRole::Lead && rec.status == AgentStatus::Active {
                        for other in s.agents.values_mut() {
                            if other.project == rec.project
                                && other.role == AgentRole::Lead
                                && other.status == AgentStatus::Active
                                && other.id != rec.id
                            {
                                other.status = AgentStatus::Retired;
                            }
                        }
                    }
                    s.index_imported(Kind::Agent, rec.id.iri())?;
                    s.agent_by_name.insert(rec.name_key.clone(), rec.id.clone());
                    s.agents.insert(rec.id.clone(), rec.clone());
                }
                RegistryEventKind::AreaDelete { area_key } => {
                    s.areas.remove(area_key);
                }
                RegistryEventKind::AgentRetire { id } => {
                    let resolved = AgentId(s.resolve(id.iri()));
                    if let Some(mut rec) = s.agents.get(&resolved).cloned() {
                        rec.status = AgentStatus::Retired;
                        rec.updated_at = ev.at.clone();
                        rec.by = ev.by.clone();
                        s.agents.insert(resolved, rec);
                    }
                }
            }
            s.append_registry_event(ev.clone());
        }
        Ok(())
    }
}

impl StateStore for MemStore {
    fn upsert_state(&self, args: UpsertStateArgs) -> Result<StateEntry, StoreError> {
        let slug =
            validate_slug(&args.slug).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let concern = args.payload.concern();
        let key = (
            args.scope.as_wire(),
            concern.as_wire().to_string(),
            slug.clone(),
        );

        let mut s = self.inner.borrow_mut();
        let existing = s
            .state_by_key
            .get(&key)
            .cloned()
            .and_then(|id| s.state_entries.get(&id).cloned());

        if let Some(ref ex) = existing
            && let Some(conflict) = s.state_conflicts.get(&ex.id)
        {
            return Err(StoreError::StateConflict {
                entry_id: ex.id.clone(),
                local_head: conflict.local_head.clone(),
                remote_head: conflict.remote_head.clone(),
            });
        }

        let entry_id = match existing {
            Some(ref ex) => ex.id.clone(),
            None => StateId(s.mint(Kind::State)),
        };

        args.payload
            .validate(&entry_id)
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;

        if let StatePayload::Memory(ref m) = args.payload
            && let Some(ref target_id) = m.superseded_by
        {
            let resolved_target = StateId(s.resolve(target_id.iri()));
            match s.state_entries.get(&resolved_target) {
                Some(target) if target.concern == StateConcern::Memory => {}
                _ => {
                    return Err(StoreError::InvalidState(format!(
                        "superseded_by target `{target_id}` does not exist as a Memory entry"
                    )));
                }
            }
        }

        let (sensitive, ever_sensitive, parent_revisions) = match existing {
            Some(ref ex) => {
                let sens = match args.sensitivity {
                    SensitivityPatch::Inherit => ex.sensitive,
                    SensitivityPatch::SetSensitive => true,
                    SensitivityPatch::ClearSensitive => false,
                };
                let ever = ex.ever_sensitive || sens;
                (sens, ever, vec![ex.head_revision.clone()])
            }
            None => {
                let sens = matches!(args.sensitivity, SensitivityPatch::SetSensitive);
                (sens, sens, Vec::new())
            }
        };

        let rev_id = RevisionId(s.mint_unindexed());
        let rev = StateRevision {
            revision_id: rev_id.clone(),
            entry_id: entry_id.clone(),
            parent_revisions,
            scope: args.scope.clone(),
            concern,
            slug: slug.clone(),
            sensitive,
            ever_sensitive,
            payload: args.payload.clone(),
            at: args.at.clone(),
            by: args.by.trim().to_string(),
        };
        let entry = StateEntry {
            id: entry_id.clone(),
            scope: args.scope,
            concern,
            slug,
            sensitive,
            ever_sensitive,
            payload: args.payload,
            head_revision: rev_id,
            updated_at: args.at,
            by: args.by.trim().to_string(),
        };
        s.append_state_revision(rev);
        s.state_by_key.insert(key, entry_id.clone());
        s.state_entries.insert(entry_id, entry.clone());
        Ok(entry)
    }

    fn retire_memory(
        &self,
        scope: &StateScope,
        slug: &str,
        lifecycle: MemoryLifecycle,
        superseded_by: Option<StateId>,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError> {
        let slug = validate_slug(slug).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        if matches!(lifecycle, MemoryLifecycle::Active) {
            return Err(StoreError::InvalidState(
                "retire_memory requires `retired` or `superseded` lifecycle".into(),
            ));
        }
        let current = self
            .get_state_by_key(scope, StateConcern::Memory, &slug)?
            .ok_or_else(|| StoreError::NoSuchState(format!("{}:memory:{slug}", scope.as_wire())))?;
        let StatePayload::Memory(mut mem) = current.payload else {
            return Err(StoreError::InvalidState(format!(
                "state entry `{}` is not a Memory entry",
                current.id
            )));
        };
        mem.lifecycle = lifecycle;
        mem.superseded_by = superseded_by;
        self.upsert_state(UpsertStateArgs {
            scope: scope.clone(),
            slug,
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Memory(mem),
            at,
            by: by.to_string(),
        })
    }

    fn get_state(&self, id: &StateId) -> Result<Option<StateEntry>, StoreError> {
        let s = self.inner.borrow();
        s.check(id.iri())?;
        let resolved = StateId(s.resolve(id.iri()));
        Ok(s.state_entries.get(&resolved).cloned())
    }

    fn get_state_by_key(
        &self,
        scope: &StateScope,
        concern: StateConcern,
        slug: &str,
    ) -> Result<Option<StateEntry>, StoreError> {
        let slug = validate_slug(slug).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let key = (scope.as_wire(), concern.as_wire().to_string(), slug);
        let s = self.inner.borrow();
        let Some(id) = s.state_by_key.get(&key).cloned() else {
            return Ok(None);
        };
        let resolved = StateId(s.resolve(id.iri()));
        Ok(s.state_entries.get(&resolved).cloned())
    }

    fn list_state(
        &self,
        scope: Option<&StateScope>,
        concern: Option<StateConcern>,
    ) -> Result<Vec<StateEntry>, StoreError> {
        let s = self.inner.borrow();
        let mut out: Vec<StateEntry> = s
            .state_entries
            .values()
            .filter(|e| scope.is_none_or(|sc| &e.scope == sc))
            .filter(|e| concern.is_none_or(|c| e.concern == c))
            .cloned()
            .collect();
        out.sort_by(|a, b| (&a.scope, a.concern, &a.slug).cmp(&(&b.scope, b.concern, &b.slug)));
        Ok(out)
    }

    fn state_revisions(&self, id: &StateId) -> Result<Vec<StateRevision>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(id.iri(), Kind::State)?;
        let resolved = StateId(s.resolve(id.iri()));
        Ok(s.state_revisions
            .values()
            .filter(|r| r.entry_id == resolved)
            .cloned()
            .collect())
    }

    fn list_conflicts(&self) -> Result<Vec<StateConflict>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.state_conflicts.values().cloned().collect())
    }

    fn resolve_conflict(
        &self,
        id: &StateId,
        keep: &RevisionId,
        at: At,
        by: &str,
    ) -> Result<StateEntry, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(id.iri(), Kind::State)?;
        let resolved = StateId(s.resolve(id.iri()));
        let Some(conflict) = s.state_conflicts.get(&resolved).cloned() else {
            return Err(StoreError::InvalidState(format!(
                "state entry `{resolved}` has no active conflict"
            )));
        };
        if keep != &conflict.local_head && keep != &conflict.remote_head {
            return Err(StoreError::InvalidState(format!(
                "`--keep {keep}` must match either local head `{}` or remote head `{}`",
                conflict.local_head, conflict.remote_head
            )));
        }
        let Some(&seq) = s.state_revision_ids.get(keep) else {
            return Err(StoreError::InvalidState(format!(
                "unknown revision `{keep}`"
            )));
        };
        let chosen_rev = s.state_revisions.get(&seq).cloned().unwrap();
        let local_rev = s
            .state_revision_ids
            .get(&conflict.local_head)
            .and_then(|seq| s.state_revisions.get(seq))
            .cloned()
            .ok_or_else(|| StoreError::InvalidState("missing local_head revision".into()))?;
        let remote_rev = s
            .state_revision_ids
            .get(&conflict.remote_head)
            .and_then(|seq| s.state_revisions.get(seq))
            .cloned()
            .ok_or_else(|| StoreError::InvalidState("missing remote_head revision".into()))?;
        let existing = s
            .state_entries
            .get(&resolved)
            .cloned()
            .ok_or_else(|| StoreError::NoSuchState(resolved.to_string()))?;

        let merge_rev_id = RevisionId(s.mint_unindexed());
        let sensitive = local_rev.sensitive || remote_rev.sensitive;
        let ever_sensitive = existing.ever_sensitive
            || local_rev.ever_sensitive
            || remote_rev.ever_sensitive
            || sensitive;
        let merge_rev = StateRevision {
            revision_id: merge_rev_id.clone(),
            entry_id: resolved.clone(),
            parent_revisions: sorted_parents(conflict.local_head, conflict.remote_head),
            scope: existing.scope.clone(),
            concern: existing.concern,
            slug: existing.slug.clone(),
            sensitive,
            ever_sensitive,
            payload: chosen_rev.payload.clone(),
            at: at.clone(),
            by: by.trim().to_string(),
        };
        s.append_state_revision(merge_rev);
        s.state_conflicts.remove(&resolved);
        let updated = StateEntry {
            id: resolved.clone(),
            scope: existing.scope,
            concern: existing.concern,
            slug: existing.slug,
            sensitive,
            ever_sensitive,
            payload: chosen_rev.payload,
            head_revision: merge_rev_id,
            updated_at: at,
            by: by.trim().to_string(),
        };
        s.state_entries.insert(resolved, updated.clone());
        Ok(updated)
    }

    fn state_revisions_after(&self, seq: u64) -> Result<Vec<(u64, StateRevision)>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.state_revisions
            .range((std::ops::Bound::Excluded(seq), std::ops::Bound::Unbounded))
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }

    fn import_state_revisions(
        &self,
        revs: &[StateRevision],
        merge_at: At,
    ) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        for rev in revs {
            if let Some(&seq) = s.state_revision_ids.get(&rev.revision_id) {
                let existing = s.state_revisions.get(&seq).unwrap();
                let mut normalized_rev = rev.clone();
                normalized_rev.entry_id = StateId(s.resolve(rev.entry_id.iri()));
                if existing != &normalized_rev {
                    return Err(StoreError::Tampered {
                        id: rev.revision_id.0.clone(),
                        detail: "state revision payload differs from stored copy".into(),
                    });
                }
                continue;
            }
            s.apply_imported_state_revision(rev.clone(), merge_at.clone())?;
        }
        Ok(())
    }

    fn sync_cursor(&self, remote: &str) -> Result<Option<StateSyncCursor>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.sync_cursors.get(remote).cloned())
    }

    fn set_sync_cursor(&self, cursor: StateSyncCursor) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        s.sync_cursors.insert(cursor.remote.clone(), cursor);
        Ok(())
    }
}

impl MessageStore for MemStore {
    fn send_wire(
        &self,
        args: SendWireArgs,
    ) -> Result<(WireMessage, Vec<DeliveryReceipt>), StoreError> {
        let subject =
            validate_subject(&args.subject).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let body =
            validate_wire_body(&args.body).map_err(|e| StoreError::InvalidState(e.to_string()))?;
        let sender_name = validate_sender_name(&args.sender_name)
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;

        let mut s = self.inner.borrow_mut();

        let (parent_thread, correlation_id) = match &args.reply_to {
            Some(parent_id) => {
                let Some(parent) = s.wire_messages.get(parent_id).cloned() else {
                    return Err(StoreError::MessageNotFound(parent_id.clone()));
                };
                let corr = if parent.kind == MessageKind::Ask {
                    Some(parent.id.clone())
                } else if parent.correlation_id.is_some() {
                    parent.correlation_id.clone()
                } else if args.kind == MessageKind::Reply {
                    Some(parent.id.clone())
                } else {
                    None
                };
                (Some(parent.thread_id), corr)
            }
            None => {
                if args.kind == MessageKind::Reply {
                    return Err(StoreError::InvalidState(
                        StateError::ReplyRequiresParent.to_string(),
                    ));
                }
                (None, None)
            }
        };

        let msg_id = match args.id {
            Some(id) => {
                // A message is immutable once sent: reusing its id must not
                // replace the body under receipts that already point at it.
                if s.wire_messages.contains_key(&id) {
                    return Err(StoreError::AlreadyExists(id.iri().clone()));
                }
                s.index_imported_unhanded(Kind::Message, id.iri())?;
                id
            }
            None => MessageId(s.mint_unhanded(Kind::Message)),
        };

        let thread_id = args
            .thread_id
            .or(parent_thread)
            .unwrap_or_else(|| msg_id.clone());
        let durable = args.durable
            || matches!(
                args.kind,
                MessageKind::Ask
                    | MessageKind::Handoff
                    | MessageKind::TaskOffer
                    | MessageKind::ApprovalRequest
            );

        let all_agents: Vec<AgentRecord> = s.agents.values().cloned().collect();
        let sender_id = args.sender_id.or_else(|| {
            all_agents
                .iter()
                .find(|a| a.status == AgentStatus::Active && a.name_key == sender_name)
                .map(|a| a.id.clone())
        });

        let msg = WireMessage {
            id: msg_id.clone(),
            thread_id,
            reply_to: args.reply_to,
            correlation_id,
            sender_id,
            sender_name,
            sender_host: args.sender_host,
            to: args.to,
            kind: args.kind,
            urgency: args.urgency,
            durable,
            subject,
            body,
            created_at: args.at.clone(),
            deadline_at: args.deadline_at,
        };
        msg.validate()
            .map_err(|e| StoreError::InvalidState(e.to_string()))?;

        let areas: Vec<AreaRecord> = s.areas.values().cloned().collect();
        check_group_address(&msg.to, &areas).map_err(StoreError::InvalidState)?;
        let recipients = resolve_recipients(
            &msg.to,
            msg.sender_id.as_ref(),
            &msg.sender_name,
            &all_agents,
            &areas,
        );

        s.append_wire_message(msg.clone());

        let mut receipts = Vec::with_capacity(recipients.len());
        for (recipient_id, recipient_name) in recipients {
            let rcpt = DeliveryReceipt {
                message_id: msg.id.clone(),
                recipient_id: recipient_id.clone(),
                recipient_name,
                status: DeliveryStatus::Pending,
                attempts: 0,
                updated_at: args.at.clone(),
                acked_at: None,
            };
            s.wire_inbox
                .insert((recipient_id, msg.id.clone()), rcpt.clone());
            s.append_wire_receipt(rcpt.clone());
            receipts.push(rcpt);
        }

        Ok((msg, receipts))
    }

    fn get_wire_message(&self, id: &MessageId) -> Result<Option<WireMessage>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.wire_messages.get(id).cloned())
    }

    fn list_wire_inbox(&self, filter: &WireFilter) -> Result<Vec<WireInboxItem>, StoreError> {
        let s = self.inner.borrow();
        let mut items = Vec::new();
        for rcpt in s.wire_inbox.values() {
            if let Some(ref rec_query) = filter.recipient
                && !s.recipient_matches(rcpt, rec_query)
            {
                continue;
            }
            if filter.unread_only
                && matches!(rcpt.status, DeliveryStatus::Acked | DeliveryStatus::Expired)
            {
                continue;
            }
            let Some(msg) = s.wire_messages.get(&rcpt.message_id).cloned() else {
                continue;
            };
            if let Some(ref tid) = filter.thread_id
                && &msg.thread_id != tid
            {
                continue;
            }
            if let Some(ref cid) = filter.correlation_id
                && msg.correlation_id.as_ref() != Some(cid)
            {
                continue;
            }
            items.push(WireInboxItem {
                message: msg,
                receipt: rcpt.clone(),
            });
        }
        items.sort_by(|a, b| {
            (&a.message.id, &a.receipt.recipient_id).cmp(&(&b.message.id, &b.receipt.recipient_id))
        });
        if let Some(limit) = filter.limit
            && items.len() > limit
        {
            items.truncate(limit);
        }
        Ok(items)
    }

    fn list_wire_thread(&self, thread_id: &ThreadId) -> Result<Vec<WireMessage>, StoreError> {
        let s = self.inner.borrow();
        let mut out = Vec::new();
        for (tid, mid) in &s.wire_threads {
            if tid == thread_id
                && let Some(msg) = s.wire_messages.get(mid)
            {
                out.push(msg.clone());
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn list_wire_replies(
        &self,
        correlation_id: &MessageId,
    ) -> Result<Vec<WireMessage>, StoreError> {
        let s = self.inner.borrow();
        let mut out = Vec::new();
        for (cid, mid) in &s.wire_replies {
            if cid == correlation_id
                && let Some(msg) = s.wire_messages.get(mid)
            {
                out.push(msg.clone());
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn list_wire_receipts(
        &self,
        message_id: &MessageId,
    ) -> Result<Vec<DeliveryReceipt>, StoreError> {
        let s = self.inner.borrow();
        if !s.wire_messages.contains_key(message_id) {
            return Err(StoreError::MessageNotFound(message_id.clone()));
        }
        let mut out: Vec<DeliveryReceipt> = s
            .wire_inbox
            .values()
            .filter(|r| &r.message_id == message_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.recipient_id.cmp(&b.recipient_id));
        Ok(out)
    }

    fn update_wire_receipt(
        &self,
        message_id: &MessageId,
        recipient: &str,
        status: DeliveryStatus,
        increment_attempts: bool,
        at: At,
    ) -> Result<Option<DeliveryReceipt>, StoreError> {
        let mut s = self.inner.borrow_mut();
        if !s.wire_messages.contains_key(message_id) {
            return Err(StoreError::MessageNotFound(message_id.clone()));
        }
        let matching_key = s
            .wire_inbox
            .iter()
            .find(|((_, mid), rcpt)| mid == message_id && s.recipient_matches(rcpt, recipient))
            .map(|(k, _)| k.clone());
        let Some(key) = matching_key else {
            return Ok(None);
        };
        let existing = s.wire_inbox.get(&key).cloned().unwrap();
        let attempts = if increment_attempts {
            existing.attempts.saturating_add(1)
        } else {
            existing.attempts
        };
        let incoming = DeliveryReceipt {
            message_id: existing.message_id.clone(),
            recipient_id: existing.recipient_id.clone(),
            recipient_name: existing.recipient_name.clone(),
            status,
            attempts,
            updated_at: at.clone(),
            acked_at: if status == DeliveryStatus::Acked {
                Some(at)
            } else {
                None
            },
        };
        let merged = existing.merge(&incoming);
        s.wire_inbox.insert(key, merged.clone());
        s.append_wire_receipt(merged.clone());
        Ok(Some(merged))
    }

    fn wire_messages_after(&self, after_seq: u64) -> Result<Vec<(u64, WireMessage)>, StoreError> {
        let s = self.inner.borrow();
        let mut out = Vec::new();
        for (&seq, mid) in s.wire_msg_log.range((
            std::ops::Bound::Excluded(after_seq),
            std::ops::Bound::Unbounded,
        )) {
            if let Some(msg) = s.wire_messages.get(mid) {
                out.push((seq, msg.clone()));
            }
        }
        Ok(out)
    }

    fn wire_receipts_after(
        &self,
        after_seq: u64,
    ) -> Result<Vec<(u64, DeliveryReceipt)>, StoreError> {
        let s = self.inner.borrow();
        Ok(s.wire_rcpt_log
            .range((
                std::ops::Bound::Excluded(after_seq),
                std::ops::Bound::Unbounded,
            ))
            .filter(|(_, rcpt)| s.wire_messages.contains_key(&rcpt.message_id))
            .map(|(&seq, rcpt)| (seq, rcpt.clone()))
            .collect())
    }

    fn import_wire_batch(
        &self,
        messages: &[WireMessage],
        receipts: &[DeliveryReceipt],
    ) -> Result<WireImportReport, StoreError> {
        let mut report = WireImportReport::default();
        let mut s = self.inner.borrow_mut();
        let all_agents: Vec<AgentRecord> = s.agents.values().cloned().collect();

        for msg in messages {
            // Each message stands or falls alone: a skip writes nothing for it.
            if let Err(e) = msg.validate() {
                report.reject(
                    &msg.id,
                    false,
                    WireImportRejectReason::InvalidMessage,
                    e.to_string(),
                );
                continue;
            }
            if let Some(existing) = s.wire_messages.get(&msg.id) {
                if existing != msg {
                    report.reject(
                        &msg.id,
                        false,
                        WireImportRejectReason::TamperedMessage,
                        "imported wire message differs from stored copy",
                    );
                }
                continue;
            }
            match s.index_imported_unhanded(Kind::Message, msg.id.iri()) {
                Ok(()) => {}
                Err(e @ StoreError::WrongKind { .. }) => {
                    report.reject(
                        &msg.id,
                        false,
                        WireImportRejectReason::IdCollision,
                        e.to_string(),
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
            s.append_wire_message(msg.clone());
            report.imported_messages += 1;

            let resolved = resolve_recipients(
                &msg.to,
                msg.sender_id.as_ref(),
                &msg.sender_name,
                &all_agents,
                &s.areas.values().cloned().collect::<Vec<_>>(),
            );
            for (rec_id, rec_name) in resolved {
                let in_incoming = receipts.iter().any(|r| {
                    r.message_id == msg.id
                        && (r.recipient_id == rec_id || r.recipient_name == rec_name)
                });
                if !in_incoming && !s.wire_inbox.contains_key(&(rec_id.clone(), msg.id.clone())) {
                    let rcpt = DeliveryReceipt {
                        message_id: msg.id.clone(),
                        recipient_id: rec_id.clone(),
                        recipient_name: rec_name,
                        status: DeliveryStatus::Pending,
                        attempts: 0,
                        updated_at: msg.created_at.clone(),
                        acked_at: None,
                    };
                    s.wire_inbox.insert((rec_id, msg.id.clone()), rcpt.clone());
                    s.append_wire_receipt(rcpt);
                }
            }
        }

        for rcpt in receipts {
            if !s.wire_messages.contains_key(&rcpt.message_id) {
                report.reject(
                    &rcpt.message_id,
                    true,
                    WireImportRejectReason::UnknownMessage,
                    "no such wire message",
                );
                continue;
            }
            let key = (rcpt.recipient_id.clone(), rcpt.message_id.clone());
            match s.wire_inbox.get(&key).cloned() {
                Some(existing) => {
                    let merged = existing.merge(rcpt);
                    if merged != existing {
                        s.wire_inbox.insert(key, merged.clone());
                        s.append_wire_receipt(merged);
                        report.imported_receipts += 1;
                    }
                }
                None => {
                    s.wire_inbox.insert(key, rcpt.clone());
                    s.append_wire_receipt(rcpt.clone());
                    report.imported_receipts += 1;
                }
            }
        }
        Ok(report)
    }

    fn prune_wire(&self, before: At) -> Result<usize, StoreError> {
        let mut s = self.inner.borrow_mut();
        let to_prune: Vec<MessageId> = s
            .wire_messages
            .values()
            .filter(|msg| !msg.durable && msg.created_at < before)
            .filter(|msg| {
                s.wire_inbox
                    .values()
                    .filter(|r| r.message_id == msg.id)
                    .all(|r| matches!(r.status, DeliveryStatus::Acked | DeliveryStatus::Expired))
            })
            .map(|msg| msg.id.clone())
            .collect();

        let count = to_prune.len();
        for mid in to_prune {
            if let Some(msg) = s.wire_messages.remove(&mid) {
                s.owned.remove(msg.id.iri());
                s.wire_threads.remove(&(msg.thread_id, msg.id.clone()));
                if let Some(corr) = msg.correlation_id {
                    s.wire_replies.remove(&(corr, msg.id.clone()));
                }
                if let Some(seq) = s.wire_msg_seq.remove(&msg.id) {
                    s.wire_msg_log.remove(&seq);
                }
                s.wire_inbox.retain(|(_, m), _| m != &mid);
                s.wire_rcpt_log.retain(|_, r| r.message_id != mid);
            }
        }
        Ok(count)
    }
}

impl WorkflowStore for MemStore {
    fn upsert_workflow_def(&self, mut def: WorkflowDef) -> Result<WorkflowDef, StoreError> {
        validate_workflow_def(&def).map_err(|e| StoreError::InvalidWorkflow(e.to_string()))?;
        let mut s = self.inner.borrow_mut();
        s.check_kind(def.project.iri(), Kind::Project)?;
        let project = ProjectId(s.resolve(def.project.iri()));
        let key = (project.clone(), def.slug.clone());

        if let Some(existing_id) = s.workflow_by_slug.get(&key).cloned() {
            let canonical_id = WorkflowId(s.resolve(existing_id.iri()));
            def.id = canonical_id.clone();
            def.project = project;
            s.workflow_defs.insert(canonical_id, def.clone());
            return Ok(def);
        }

        let canonical_id = WorkflowId(s.resolve_if_kind(def.id.iri(), Kind::Workflow)?);
        s.index_imported(Kind::Workflow, canonical_id.iri())?;
        if let Some(prev) = s.workflow_defs.get(&canonical_id) {
            let prev_key = (prev.project.clone(), prev.slug.clone());
            if prev_key != key {
                s.workflow_by_slug.remove(&prev_key);
            }
        }
        def.id = canonical_id.clone();
        def.project = project;
        s.workflow_by_slug.insert(key, canonical_id.clone());
        s.workflow_defs.insert(canonical_id, def.clone());
        Ok(def)
    }

    fn get_workflow_def(&self, id: &WorkflowId) -> Result<Option<WorkflowDef>, StoreError> {
        let s = self.inner.borrow();
        let canonical = WorkflowId(s.resolve_if_kind(id.iri(), Kind::Workflow)?);
        Ok(s.workflow_defs.get(&canonical).cloned())
    }

    fn get_workflow_def_by_slug(
        &self,
        project: &ProjectId,
        slug: &str,
    ) -> Result<Option<WorkflowDef>, StoreError> {
        let s = self.inner.borrow();
        s.check_kind(project.iri(), Kind::Project)?;
        let canonical_project = ProjectId(s.resolve(project.iri()));
        let Some(id) = s
            .workflow_by_slug
            .get(&(canonical_project, slug.to_string()))
            .cloned()
        else {
            return Ok(None);
        };
        let canonical_id = WorkflowId(s.resolve(id.iri()));
        Ok(s.workflow_defs.get(&canonical_id).cloned())
    }

    fn list_workflow_defs(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<WorkflowDef>, StoreError> {
        let s = self.inner.borrow();
        let project_filter = match project {
            Some(p) => {
                s.check_kind(p.iri(), Kind::Project)?;
                Some(ProjectId(s.resolve(p.iri())))
            }
            None => None,
        };
        let mut out: Vec<WorkflowDef> = s
            .workflow_defs
            .values()
            .filter(|d| project_filter.as_ref().is_none_or(|p| &d.project == p))
            .cloned()
            .collect();
        out.sort_by(|a, b| (&a.project, &a.slug).cmp(&(&b.project, &b.slug)));
        Ok(out)
    }

    fn create_workflow_run(&self, mut run: WorkflowRun) -> Result<WorkflowRun, StoreError> {
        let mut s = self.inner.borrow_mut();
        s.check_kind(run.project.iri(), Kind::Project)?;
        let canonical_project = ProjectId(s.resolve(run.project.iri()));
        let canonical_wf_id = WorkflowId(s.resolve_if_kind(run.workflow_id.iri(), Kind::Workflow)?);
        if !s.workflow_defs.contains_key(&canonical_wf_id) {
            return Err(StoreError::NoSuchWorkflow(run.workflow_id.to_string()));
        }
        let canonical_run_id = WorkflowRunId(s.resolve_if_kind(run.id.iri(), Kind::WorkflowRun)?);
        if s.workflow_runs.contains_key(&canonical_run_id)
            || s.owned.contains_key(canonical_run_id.iri())
        {
            return Err(StoreError::AlreadyExists(run.id.0));
        }
        s.index_imported(Kind::WorkflowRun, canonical_run_id.iri())?;
        run.id = canonical_run_id.clone();
        run.project = canonical_project;
        run.workflow_id = canonical_wf_id;
        s.workflow_runs.insert(canonical_run_id, run.clone());
        Ok(run)
    }

    fn update_workflow_run(&self, mut run: WorkflowRun) -> Result<WorkflowRun, StoreError> {
        let mut s = self.inner.borrow_mut();
        let canonical_id = WorkflowRunId(s.resolve_if_kind(run.id.iri(), Kind::WorkflowRun)?);
        if !s.workflow_runs.contains_key(&canonical_id) {
            return Err(StoreError::NoSuchWorkflowRun(run.id.to_string()));
        }
        run.id = canonical_id.clone();
        s.workflow_runs.insert(canonical_id, run.clone());
        Ok(run)
    }

    fn get_workflow_run(&self, id: &WorkflowRunId) -> Result<Option<WorkflowRun>, StoreError> {
        let s = self.inner.borrow();
        let canonical = WorkflowRunId(s.resolve_if_kind(id.iri(), Kind::WorkflowRun)?);
        Ok(s.workflow_runs.get(&canonical).cloned())
    }

    fn list_workflow_runs(
        &self,
        project: Option<&ProjectId>,
        workflow_id: Option<&WorkflowId>,
    ) -> Result<Vec<WorkflowRun>, StoreError> {
        let s = self.inner.borrow();
        let project_filter = match project {
            Some(p) => {
                s.check_kind(p.iri(), Kind::Project)?;
                Some(ProjectId(s.resolve(p.iri())))
            }
            None => None,
        };
        let wf_filter = match workflow_id {
            Some(w) => Some(WorkflowId(s.resolve_if_kind(w.iri(), Kind::Workflow)?)),
            None => None,
        };
        let mut out: Vec<WorkflowRun> = s
            .workflow_runs
            .values()
            .filter(|r| project_filter.as_ref().is_none_or(|p| &r.project == p))
            .filter(|r| wf_filter.as_ref().is_none_or(|w| &r.workflow_id == w))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn import_workflow_def(&self, mut def: WorkflowDef) -> Result<(), StoreError> {
        validate_workflow_def(&def).map_err(|e| StoreError::InvalidWorkflow(e.to_string()))?;
        let mut s = self.inner.borrow_mut();
        let canonical_project = ProjectId(s.resolve_if_kind(def.project.iri(), Kind::Project)?);
        let canonical_id = WorkflowId(s.resolve_if_kind(def.id.iri(), Kind::Workflow)?);
        let key = (canonical_project.clone(), def.slug.clone());

        if let Some(existing_id) = s.workflow_by_slug.get(&key).cloned()
            && existing_id != canonical_id
        {
            s.alias_unpublished_id(Kind::Workflow, existing_id.iri(), canonical_id.iri())?;
            s.workflow_defs.remove(&existing_id);
            for r in s.workflow_runs.values_mut() {
                if r.workflow_id == existing_id {
                    r.workflow_id = canonical_id.clone();
                }
            }
        }
        if let Some(prev) = s.workflow_defs.get(&canonical_id) {
            let prev_key = (prev.project.clone(), prev.slug.clone());
            if prev_key != key {
                s.workflow_by_slug.remove(&prev_key);
            }
        }
        s.index_imported(Kind::Workflow, canonical_id.iri())?;
        def.id = canonical_id.clone();
        def.project = canonical_project;
        s.workflow_by_slug.insert(key, canonical_id.clone());
        s.workflow_defs.insert(canonical_id, def);
        Ok(())
    }

    fn import_workflow_run(&self, mut run: WorkflowRun) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        let canonical_project = ProjectId(s.resolve_if_kind(run.project.iri(), Kind::Project)?);
        let canonical_wf_id = WorkflowId(s.resolve_if_kind(run.workflow_id.iri(), Kind::Workflow)?);
        if !s.workflow_defs.contains_key(&canonical_wf_id) {
            return Err(StoreError::NoSuchWorkflow(run.workflow_id.to_string()));
        }
        let canonical_id = WorkflowRunId(s.resolve_if_kind(run.id.iri(), Kind::WorkflowRun)?);
        s.index_imported(Kind::WorkflowRun, canonical_id.iri())?;
        run.id = canonical_id.clone();
        run.project = canonical_project;
        run.workflow_id = canonical_wf_id;
        s.workflow_runs.insert(canonical_id, run);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_args(id: Option<MessageId>, body: &str) -> SendWireArgs {
        use crate::ferrowire::{MessageKind, MessageUrgency, WireAddress};
        SendWireArgs {
            id,
            sender_id: None,
            sender_name: "jarvis".into(),
            sender_host: None,
            to: WireAddress::AgentName("ferris".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "s".into(),
            body: body.into(),
            reply_to: None,
            thread_id: None,
            deadline_at: None,
            at: At::from_unix_millis(10),
        }
    }

    #[test]
    fn a_cursor_at_u64_max_reads_nothing_instead_of_overflowing() {
        let s = MemStore::default();
        s.send_wire(wire_args(None, "hello")).unwrap();
        assert!(s.wire_messages_after(u64::MAX).unwrap().is_empty());
        assert!(s.wire_receipts_after(u64::MAX).unwrap().is_empty());
        assert!(s.state_revisions_after(u64::MAX).unwrap().is_empty());
        assert!(s.registry_events_after(u64::MAX).unwrap().is_empty());
        assert_eq!(s.wire_messages_after(0).unwrap().len(), 1);
    }

    #[test]
    fn sending_with_a_used_message_id_is_refused_and_leaves_the_first_message() {
        let s = MemStore::default();
        let id = MessageId(Iri::parse("urn:uuid:00000000-0000-4000-8000-000000000001").unwrap());
        s.send_wire(wire_args(Some(id.clone()), "first")).unwrap();
        let err = s
            .send_wire(wire_args(Some(id.clone()), "second"))
            .unwrap_err();
        assert!(matches!(err, StoreError::AlreadyExists(_)), "{err:?}");
        assert_eq!(s.get_wire_message(&id).unwrap().unwrap().body, "first");
    }

    #[test]
    fn mem_store_meets_every_role_contract() {
        use crate::conformance::Single;
        crate::conformance::catalog(|| (MemStore::default(), ()));
        crate::conformance::tracker(|| Single(MemStore::default(), ()));
        crate::conformance::ledger(|| Single(MemStore::default(), ()));
        crate::conformance::all_roles(|| Single(MemStore::default(), ()));
        crate::conformance::local_handles(|| (MemStore::default(), ()));
        crate::conformance::ledger_cache(|| (MemStore::default(), ()));
        crate::conformance::agent_registry(|| (MemStore::default(), ()));
        crate::conformance::state_store(|| (MemStore::default(), ()));
        crate::conformance::message_store(|| (MemStore::default(), ()));
        crate::conformance::workflow_store(|| (MemStore::default(), ()));
    }

    #[test]
    fn a_store_started_at_n_mints_n_first() {
        let s = MemStore::starting_at(1_000);
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        assert_eq!((p.0, r.0), (seq_iri(1_000), seq_iri(1_001)));
        assert_eq!(
            MemStore::default().add_project("/p").unwrap().0,
            seq_iri(1),
            "the default store still starts at 1"
        );
    }

    #[test]
    fn mem_store_meets_the_escalation_contract() {
        use crate::conformance::Single;
        crate::conformance::escalations(|| Single(MemStore::default(), ()));
    }

    // Routing spec §3.3 step 1: the shared case, run on its own by name.
    #[test]
    fn a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed() {
        use crate::conformance::{EscalationFixture, Single};
        Single(MemStore::default(), ()).with_escalations(&mut |b| {
            crate::conformance::a_marked_item_refuses_every_write_and_a_finding_about_it_is_allowed(
                b,
            )
        });
    }

    // Routing spec §2.3: a local handle of an escalated item resolves
    // through its tombstone, so the handle and the row stay.
    #[test]
    fn an_escalated_items_handle_still_resolves_to_its_id() {
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        let mark = Mark {
            by: "alice".into(),
            reason: "r".into(),
            at_ms: 1,
        };
        s.mark(r.iri(), &mark).unwrap();
        let to = Iri::parse("https://github.com/acme/widgets/issues/7").unwrap();
        s.tombstone(r.iri(), &to).unwrap();
        assert_eq!(
            s.resolve_handle(Kind::Record, 1).unwrap(),
            Some(r.0.clone())
        );
        assert_eq!(s.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
    }

    #[test]
    fn a_split_ledger_over_a_mem_store_meets_the_ledger_contracts() {
        use crate::conformance::SplitOver;
        crate::conformance::ledger(|| SplitOver(MemStore::default(), ()));
        crate::conformance::split_ledger(|| SplitOver(MemStore::default(), ()));
    }

    /// A project with one gate and one record.
    fn outbox_world() -> (
        MemStore,
        crate::ids::GateId,
        crate::ids::RecordId,
        ProjectId,
    ) {
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let kind = crate::model::GateKind::Command(crate::model::CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: crate::model::PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        });
        let sel = crate::model::Selector::Glob {
            pattern: "**/*".into(),
        };
        let g = s.add_gate(&p, "g", kind, sel, 1, "c", "o").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        (s, g, r, p)
    }

    // Spec §1.3, §2.1, §3.2 step 6: only an entry with an id, tied to a
    // record, after the cut-over and not yet marked can be waiting; a mark
    // is per repository; the list is in id order.
    #[test]
    fn unpublished_lists_only_record_tied_entries_after_the_cut_over_with_no_mark() {
        use crate::conformance::{entry_iri, sample_attempt, sample_record_run};
        use crate::split::Outbox;
        let (s, g, r, p) = outbox_world();
        let before = sample_record_run(2, &g, Some(&r));
        let marked = sample_record_run(4, &g, Some(&r));
        let no_record = sample_record_run(5, &g, None);
        let mut no_id = sample_record_run(6, &g, Some(&r));
        no_id.id = None;
        let late = sample_record_run(8, &g, Some(&r));
        let waiting = sample_record_run(7, &g, Some(&r));
        for run in [&before, &marked, &no_record, &no_id, &late, &waiting] {
            s.append_gate_run(run.clone()).unwrap();
        }
        let attempt = sample_attempt(9, &p, &r);
        s.append_attempt(attempt.clone()).unwrap();
        s.mark_published("R_1", &[marked.id.clone().unwrap()])
            .unwrap();

        let pending = s.unpublished("R_1", &entry_iri(3)).unwrap();
        assert_eq!(
            pending.runs,
            vec![waiting.clone(), late.clone()],
            "id order"
        );
        assert_eq!(pending.attempts, vec![attempt]);
        assert_eq!(
            s.unpublished("R_2", &entry_iri(3)).unwrap().runs,
            vec![waiting, late],
            "the waiting set is the store's: an entry published anywhere waits for no repository"
        );
        assert!(
            !s.is_published("R_2", marked.id.as_ref().unwrap()).unwrap(),
            "a mark is per repository"
        );
    }

    // Spec §2.1: an entry set aside waits no more, and is not published.
    #[test]
    fn an_entry_set_aside_waits_no_more_and_is_not_published() {
        use crate::conformance::{entry_iri, sample_record_run};
        use crate::split::Outbox;
        let (s, g, r, _p) = outbox_world();
        let aside = sample_record_run(4, &g, Some(&r));
        s.append_gate_run(aside.clone()).unwrap();
        s.settle("R_1", &[], &[aside.id.clone().unwrap()]).unwrap();
        s.settle("R_1", &[], &[aside.id.clone().unwrap()]).unwrap();
        assert!(s.unpublished("R_1", &entry_iri(0)).unwrap().runs.is_empty());
        assert!(!s.is_published("R_1", aside.id.as_ref().unwrap()).unwrap());
    }

    // Spec §2.1: the cut-over is recorded once, when the GitHub ledger is
    // switched on; moving it would strand every entry in between.
    #[test]
    fn a_cut_over_is_recorded_once_per_repository() {
        use crate::conformance::entry_iri;
        use crate::split::Outbox;
        let s = MemStore::default();
        assert_eq!(s.cutover("R_1").unwrap(), None);
        s.set_cutover("R_1", &entry_iri(3)).unwrap();
        s.set_cutover("R_1", &entry_iri(3)).unwrap();
        let err = s.set_cutover("R_1", &entry_iri(4)).unwrap_err();
        assert!(
            matches!(err, StoreError::CutoverChanged { ref held, .. } if *held == entry_iri(3)),
            "{err:?}"
        );
        assert_eq!(s.cutover("R_1").unwrap(), Some(entry_iri(3)));
        assert_eq!(s.cutover("R_2").unwrap(), None);
    }

    // Routing spec §2.5: a local store keeps a GitHub record's reference
    // only through the router's proof, and never for a record it holds.
    #[test]
    fn a_memory_store_keeps_a_checked_reference_to_a_github_record() {
        use crate::routing::{ForeignRecord, Tier};
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let url = RecordId(Iri::parse("https://github.com/acme/widgets/issues/7").unwrap());
        // Raised against the placeholder: the stored reference must come
        // from the proof.
        let f = Finding::raise(p.clone(), RecordId(seq_iri(0)), "rev", "claim");
        let id = s
            .add_finding_checked(
                f.clone(),
                ForeignRecord::for_tests(url.clone(), "t", Tier::Github),
            )
            .unwrap();
        assert_eq!(s.get_finding(&id).unwrap().unwrap().record, url);
        let err = s
            .add_finding_checked(f.clone(), ForeignRecord::for_tests(url, "t", Tier::Local))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("is a record in the local tier, so a finding about it"),
            "{err}"
        );
        let held = s.add_record(&p, "t").unwrap();
        let err = s
            .add_finding_checked(f, ForeignRecord::for_tests(held, "t", Tier::Github))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("is held by this store, so a finding about it"),
            "{err}"
        );
    }

    #[test]
    fn a_memory_store_keeps_one_routing_map_per_project() {
        use crate::routing::{Routes, RoutingMap};
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let q = s.add_project("/q").unwrap();
        assert_eq!(s.routes(&p).unwrap(), None);
        s.set_routes(&p, &RoutingMap::starting()).unwrap();
        assert_eq!(s.routes(&p).unwrap(), Some(RoutingMap::starting()));
        assert_eq!(s.routes(&q).unwrap(), None, "per project");
        let err = s.routes(&ProjectId(seq_iri(99))).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
    }
}

#[derive(Default)]
struct ManagedMemory {
    records: BTreeMap<crate::wire_delivery::DeliveryKey, crate::wire_delivery::DeliveryRecord>,
    reservations: BTreeMap<MessageId, crate::wire_delivery::IdReservation>,
    recipients: BTreeMap<AgentId, crate::wire_delivery::RecipientControl>,
    identities: BTreeMap<String, crate::wire_delivery::IdentityResolution>,
    receipt_keys: BTreeMap<crate::wire_delivery::DeliveryKey, Iri>,
    global: Option<crate::wire_delivery::GlobalControl>,
    continuity: Option<crate::wire_delivery::Continuity>,
    access: DeliveryAccess,
    // Deliberately unavailable in production, including feature=conformance.
    #[cfg(test)]
    admissions: BTreeMap<crate::wire_delivery::Digest, crate::wire_delivery::LifetimeRegistration>,
    #[cfg(test)]
    ended_scopes:
        BTreeMap<crate::wire_delivery::Digest, crate::wire_delivery::LifetimeRegistration>,
}

#[derive(Default)]
struct DeliveryAccess {
    #[cfg(test)]
    reads: Vec<(&'static str, bool)>,
    #[cfg(test)]
    writes: Vec<&'static str>,
}
impl DeliveryAccess {
    fn read<T>(&mut self, name: &'static str, value: Option<T>) -> Option<T> {
        #[cfg(test)]
        self.reads.push((name, value.is_some()));
        #[cfg(not(test))]
        let _ = name;
        value
    }
    fn write(&mut self, name: &'static str) {
        #[cfg(test)]
        self.writes.push(name);
        #[cfg(not(test))]
        let _ = name;
    }
}

impl Inner {
    fn delivery_context(
        &mut self,
        command: &crate::wire_delivery::DeliveryCommand,
    ) -> Result<Option<crate::wire_delivery::TransitionContext>, StoreError> {
        use crate::wire_delivery::*;
        let key = command.key();
        let d = &mut self.delivery;
        let record = d.access.read("record", d.records.get(key).cloned());
        let reservation = d
            .access
            .read("reservation", d.reservations.get(&key.original_id).cloned());
        let recipient = d
            .access
            .read("recipient", d.recipients.get(&key.recipient_id).cloned());
        let global = d.access.read("global", d.global.clone());
        let continuity = d.access.read("continuity", d.continuity.clone());
        let message = d
            .access
            .read("message", self.wire_messages.get(&key.original_id).cloned());
        let receipt_key = d
            .access
            .read("receipt_index", d.receipt_keys.get(key).cloned());
        let receipt = d.access.read(
            "receipt",
            receipt_key
                .as_ref()
                .and_then(|id| self.wire_inbox.get(&(id.clone(), key.original_id.clone())))
                .cloned(),
        );
        let registry = d
            .access
            .read("registry", self.agents.get(&key.recipient_id).cloned());
        let selector_key = message
            .as_ref()
            .map(|m| serde_json::to_string(&m.to))
            .transpose()
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let selector = d.access.read(
            "selector_index",
            selector_key
                .as_ref()
                .and_then(|k| d.identities.get(k))
                .cloned(),
        );
        let receipt_alias = d.access.read(
            "receipt_alias_index",
            receipt
                .as_ref()
                .and_then(|r| d.identities.get(&format!("name:{}", r.recipient_name)))
                .cloned(),
        );
        #[cfg(test)]
        let lifetime = match command {
            DeliveryCommand::Prepare(input) => d.access.read(
                "admission",
                d.admissions.get(&input.attempt.lifetime.id).cloned(),
            ),
            DeliveryCommand::Arm(input) => d.access.read(
                "admission",
                d.admissions.get(&input.attempt.lifetime.id).cloned(),
            ),
            _ => None,
        };
        #[cfg(not(test))]
        let lifetime = None;
        let (Some(recipient), Some(global), Some(continuity), Some(selector), Some(receipt_alias)) =
            (recipient, global, continuity, selector, receipt_alias)
        else {
            return Ok(None);
        };
        Ok(Some(TransitionContext {
            key: key.clone(),
            record,
            reservation,
            recipient,
            global,
            continuity,
            message,
            receipt,
            identity: IdentityContext {
                registry,
                selector,
                receipt_alias,
            },
            lifetime,
        }))
    }

    fn commit_delivery(
        &mut self,
        delta: crate::wire_delivery::DeliveryDelta,
    ) -> crate::wire_delivery::DeliveryResult {
        let d = &mut self.delivery;
        if let Some(record) = delta.record {
            d.access.write("record");
            d.records.insert(record.key.clone(), record);
        }
        if let Some(reservation) = delta.reservation {
            d.access.write("reservation");
            d.reservations
                .insert(reservation.original_id.clone(), reservation);
        }
        if let Some(recipient) = delta.recipient {
            d.access.write("recipient");
            d.recipients
                .insert(recipient.binding.recipient_id.clone(), recipient);
        }
        if let Some(global) = delta.global {
            d.access.write("global");
            d.global = Some(global);
        }
        if let Some(receipt) = delta.receipt {
            d.access.write("receipt");
            self.wire_inbox.insert(
                (receipt.recipient_id.clone(), receipt.message_id.clone()),
                receipt.clone(),
            );
            self.append_wire_receipt(receipt);
            self.delivery.access.write("receipt_log");
            self.delivery.access.write("receipt_sequence");
        }
        delta.result
    }
}

impl crate::wire_delivery::WireDeliveryStore for MemStore {
    fn apply_delivery(
        &self,
        command: crate::wire_delivery::DeliveryCommand,
    ) -> Result<crate::wire_delivery::DeliveryResult, StoreError> {
        use crate::wire_delivery::*;
        // Ordinary MemStore has no trusted lifetime-admission producer.
        #[cfg(not(test))]
        if matches!(
            command,
            DeliveryCommand::Prepare(_) | DeliveryCommand::Arm(_)
        ) {
            return Ok(DeliveryResult::Refused {
                reason: HoldReason::UnsupportedBackend,
            });
        }
        let mut inner = self.inner.borrow_mut();
        let Some(context) = inner.delivery_context(&command)? else {
            return Ok(DeliveryResult::Refused {
                reason: HoldReason::ContinuityLost,
            });
        };
        let delta = transition(context, command)?;
        Ok(inner.commit_delivery(delta))
    }

    fn inspect_delivery(
        &self,
        key: &crate::wire_delivery::DeliveryKey,
    ) -> Result<Option<crate::wire_delivery::DeliveryView>, StoreError> {
        let mut inner = self.inner.borrow_mut();
        let d = &mut inner.delivery;
        let record = d.access.read("record", d.records.get(key).cloned());
        let recipient = d
            .access
            .read("recipient", d.recipients.get(&key.recipient_id).cloned());
        match (record, recipient) {
            (Some(record), Some(recipient)) => Ok(Some(record.view(&recipient))),
            (None, _) => Ok(None),
            _ => Err(StoreError::Backend(
                "missing delivery recipient control".into(),
            )),
        }
    }

    fn active_delivery_owners(
        &self,
    ) -> Result<Vec<crate::wire_delivery::CapacityRetirementTarget>, StoreError> {
        use crate::wire_delivery::*;
        let mut inner = self.inner.borrow_mut();
        let d = &mut inner.delivery;
        let Some(global) = d.access.read("global", d.global.clone()) else {
            return Err(StoreError::Backend(
                "missing delivery global control".into(),
            ));
        };
        if !crate::wire_delivery::valid_global(&global) {
            return Err(StoreError::Backend("corrupt delivery active index".into()));
        }
        let mut out = Vec::with_capacity(global.active_index.len());
        for owner in &global.active_index {
            let record = d.access.read("record", d.records.get(&owner.key).cloned());
            let recipient = d.access.read(
                "recipient",
                d.recipients.get(&owner.key.recipient_id).cloned(),
            );
            let (Some(record), Some(recipient)) = (record, recipient) else {
                return Err(StoreError::Backend("missing active delivery owner".into()));
            };
            if recipient.active.as_ref() != Some(owner)
                || !record
                    .attempts
                    .iter()
                    .any(|a| a.binding == owner.attempt && a.capacity == CapacityState::Held)
            {
                return Err(StoreError::Backend("corrupt active delivery owner".into()));
            }
            out.push(CapacityRetirementTarget {
                owner: owner.clone(),
                record_revision: record.revision,
                recipient_revision: recipient.revision,
                global_revision: global.revision,
            });
        }
        Ok(out)
    }

    fn retire_orphan_capacity(
        &self,
        target: crate::wire_delivery::CapacityRetirementTarget,
    ) -> Result<crate::wire_delivery::CapacityRetirementResult, StoreError> {
        #[cfg(not(test))]
        {
            let _ = target;
            Ok(crate::wire_delivery::CapacityRetirementResult::Unproven)
        }
        #[cfg(test)]
        {
            use crate::wire_delivery::*;
            let mut inner = self.inner.borrow_mut();
            // This module-local fixture is not a real producer or an API for proof submission.
            if inner
                .delivery
                .ended_scopes
                .get(&target.owner.attempt.lifetime.id)
                != Some(&target.owner.attempt.lifetime)
            {
                return Ok(CapacityRetirementResult::Unproven);
            }
            let command = DeliveryCommand::Close {
                key: target.owner.key.clone(),
            };
            let Some(context) = inner.delivery_context(&command)? else {
                return Ok(CapacityRetirementResult::Stale);
            };
            let delta = transition_capacity_retirement(context, target)?;
            if let Some(record) = delta.record {
                inner.delivery.access.write("record");
                inner.delivery.records.insert(record.key.clone(), record);
            }
            if let Some(recipient) = delta.recipient {
                inner.delivery.access.write("recipient");
                inner
                    .delivery
                    .recipients
                    .insert(recipient.binding.recipient_id.clone(), recipient);
            }
            if let Some(global) = delta.global {
                inner.delivery.access.write("global");
                inner.delivery.global = Some(global);
            }
            Ok(delta.result)
        }
    }
}

#[cfg(test)]
mod managed_tests {
    use super::*;
    use crate::wire_delivery::conformance::{fixture_commands, fixture_message};
    use crate::wire_delivery::*;

    fn install(s: &MemStore) {
        let DeliveryCommand::Prepare(input) = fixture_commands().remove(0) else {
            unreachable!()
        };
        install_input(s, &input, fixture_message());
    }

    fn install_input(s: &MemStore, input: &PrepareInput, message: WireMessage) {
        let mut inner = s.inner.borrow_mut();
        let recipient_id = input.key.recipient_id.clone();
        let name = match &message.to {
            crate::ferrowire::WireAddress::AgentName(name) => name.clone(),
            _ => "bob".into(),
        };
        let resolution = IdentityResolution::Unique {
            recipient_id: recipient_id.clone(),
            binding_revision: input.binding.binding_revision,
        };
        inner.agents.insert(
            recipient_id.clone(),
            AgentRecord {
                id: recipient_id.clone(),
                name_key: name.clone(),
                display_name: "Bob".into(),
                email: "bob@example.test".into(),
                project: "ferroloop".into(),
                role: AgentRole::Assistant,
                extra_areas: vec![],
                status: AgentStatus::Active,
                updated_at: At::from_unix_millis(1),
                by: "fixture".into(),
            },
        );
        inner.wire_inbox.insert(
            (recipient_id.iri().clone(), message.id.clone()),
            DeliveryReceipt {
                message_id: message.id.clone(),
                recipient_id: recipient_id.iri().clone(),
                recipient_name: name.clone(),
                status: DeliveryStatus::Pending,
                attempts: 0,
                updated_at: message.created_at.clone(),
                acked_at: None,
            },
        );
        inner.delivery.identities.insert(
            serde_json::to_string(&message.to).unwrap(),
            resolution.clone(),
        );
        inner
            .delivery
            .identities
            .insert(format!("name:{name}"), resolution);
        inner
            .delivery
            .receipt_keys
            .insert(input.key.clone(), recipient_id.iri().clone());
        inner.append_wire_message(message);
        inner
            .delivery
            .recipients
            .entry(recipient_id)
            .or_insert(RecipientControl {
                binding: input.binding.clone(),
                revision: Revision(1),
                active: None,
                barrier: None,
                identity_conflict: false,
            });
        inner
            .delivery
            .global
            .get_or_insert_with(GlobalControl::default);
        inner.delivery.continuity.get_or_insert(Continuity {
            schema: DELIVERY_SCHEMA_VERSION,
            cutover: At::from_unix_millis(0),
            policy_hash: input.binding.policy_hash,
            local_epoch: input.binding.local_epoch,
            enabled: true,
            intact: true,
        });
        inner
            .delivery
            .admissions
            .insert(input.attempt.lifetime.id, input.attempt.lifetime.clone());
    }

    #[test]
    fn managed_mem_first_prepare_wins() {
        let s = MemStore::default();
        install(&s);
        let command = fixture_commands().remove(0);
        let mut other = command.clone();
        if let DeliveryCommand::Prepare(input) = &mut other {
            input.attempt.token = PhaseToken([42; 32]);
        }
        let results = [
            s.apply_delivery(command.clone()).unwrap(),
            s.apply_delivery(other).unwrap(),
        ];
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, DeliveryResult::Prepared { .. }))
                .count(),
            1
        );
        assert_eq!(
            s.inspect_delivery(command.key()).unwrap().unwrap().prepares,
            1
        );
    }

    #[test]
    fn managed_transition_crash_matrix() {
        // One fixture per spec §7 crash row; copying committed Inner below models
        // process restart, without generating a new attempt or connection grant.
        for crash in 0..8 {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            match crash {
                0 => {
                    assert!(s.inspect_delivery(&input.key).unwrap().is_none());
                    prepare(&s, &input);
                }
                1 => {
                    let p = prepare(&s, &input);
                    let stale_arm = arm_input(&input, p.revision);
                    s.apply_delivery(DeliveryCommand::InvalidatePrepared {
                        key: input.key.clone(),
                        revision: p.revision,
                        now: At::from_unix_millis(22000),
                    })
                    .unwrap();
                    assert_eq!(view(&s, &input).prepares, 1);
                    assert_eq!(view(&s, &input).eligibility, Eligibility::Eligible);
                    assert!(matches!(
                        s.apply_delivery(DeliveryCommand::Arm(stale_arm)).unwrap(),
                        DeliveryResult::Refused { .. }
                    ));
                }
                2..=4 => {
                    // Unknown arm reply, armed-before-output/partial output, and
                    // vendor acceptance with failed settlement all retain fences.
                    armed(&s, &input);
                    let committed = view(&s, &input);
                    assert_eq!(committed.eligibility, Eligibility::MutationArmed);
                    assert!(matches!(
                        s.apply_delivery(DeliveryCommand::Prepare(input.clone()))
                            .unwrap(),
                        DeliveryResult::Refused { .. }
                    ));
                    s.apply_delivery(DeliveryCommand::Close {
                        key: input.key.clone(),
                    })
                    .unwrap();
                    assert_ne!(view(&s, &input).eligibility, Eligibility::Eligible);
                    assert_eq!(view(&s, &input).barrier, Some(input.key.clone()));
                    settle(&s, &input, DeliveryEvidence::MutationUnconfirmed);
                    assert_eq!(view(&s, &input).eligibility, Eligibility::Held);
                }
                5 => {
                    armed(&s, &input);
                    settle(&s, &input, DeliveryEvidence::VendorQueued);
                    let before = view(&s, &input);
                    settle(&s, &input, DeliveryEvidence::VendorQueued);
                    assert_eq!(view(&s, &input), before);
                    assert!(matches!(
                        s.apply_delivery(DeliveryCommand::Prepare(input.clone()))
                            .unwrap(),
                        DeliveryResult::Refused { .. }
                    ));
                }
                6 => {
                    let p = prepare(&s, &input);
                    ack(&s, &input, 2100);
                    assert!(matches!(
                        s.apply_delivery(DeliveryCommand::Arm(arm_input(&input, p.revision)))
                            .unwrap(),
                        DeliveryResult::Refused { .. }
                    ));
                    assert_ne!(view(&s, &input).eligibility, Eligibility::Eligible);
                }
                7 => {
                    s.inner
                        .borrow_mut()
                        .delivery
                        .continuity
                        .as_mut()
                        .unwrap()
                        .intact = false;
                    assert!(matches!(
                        s.apply_delivery(DeliveryCommand::Prepare(input.clone()))
                            .unwrap(),
                        DeliveryResult::Refused {
                            reason: HoldReason::ContinuityLost
                        }
                    ));
                    assert!(s.inspect_delivery(&input.key).unwrap().is_none());
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn managed_three_prepares_lifetime() {
        let s = MemStore::default();
        install(&s);
        let DeliveryCommand::Prepare(mut input) = fixture_commands().remove(0) else {
            unreachable!()
        };
        for n in 0..3 {
            input.attempt.token = PhaseToken([n; 32]);
            let DeliveryResult::Prepared { record } = s
                .apply_delivery(DeliveryCommand::Prepare(input.clone()))
                .unwrap()
            else {
                panic!("prepare")
            };
            s.apply_delivery(DeliveryCommand::Release(ReleaseInput {
                key: input.key.clone(),
                revision: record.revision,
                attempt: input.attempt.clone(),
                proof: DeliveryEvidence::PreMutationRefused,
                now: input.now.clone(),
            }))
            .unwrap();
        }
        ack(&s, &input, 5000);
        s.apply_delivery(DeliveryCommand::Annotate {
            key: input.key.clone(),
            text: "restart simulation".into(),
        })
        .unwrap();
        let restarted = MemStore::default();
        {
            let mut from = s.inner.borrow_mut();
            let mut to = restarted.inner.borrow_mut();
            to.delivery = std::mem::take(&mut from.delivery);
            to.delivery.admissions.clear(); // Restart cannot reconstruct connection authority.
            to.wire_messages = std::mem::take(&mut from.wire_messages);
            to.wire_inbox = std::mem::take(&mut from.wire_inbox);
            to.agents = std::mem::take(&mut from.agents);
        }
        assert_eq!(
            restarted
                .inspect_delivery(&input.key)
                .unwrap()
                .unwrap()
                .prepares,
            3
        );
        assert!(matches!(
            restarted
                .apply_delivery(DeliveryCommand::Prepare(input))
                .unwrap(),
            DeliveryResult::Refused {
                reason: HoldReason::BudgetExhausted
            }
        ));
    }

    fn base_input() -> PrepareInput {
        let DeliveryCommand::Prepare(input) = fixture_commands().remove(0) else {
            unreachable!()
        };
        input
    }
    fn prepare(s: &MemStore, input: &PrepareInput) -> DeliveryRecord {
        match s
            .apply_delivery(DeliveryCommand::Prepare(input.clone()))
            .unwrap()
        {
            DeliveryResult::Prepared { record } => record,
            result => panic!("expected Prepared, got {result:?}"),
        }
    }
    fn arm_input(input: &PrepareInput, revision: Revision) -> ArmInput {
        let DeliveryCommand::Arm(mut arm) = fixture_commands().remove(1) else {
            unreachable!()
        };
        arm.key = input.key.clone();
        arm.attempt = input.attempt.clone();
        arm.route = input.route.clone();
        arm.revision = revision;
        arm.now = At::from_unix_millis(input.now.unix_millis() + 1);
        arm.frozen.digest =
            canonical_payload(input.generation, &arm.frozen.renderer, &arm.frozen.bytes).unwrap();
        arm
    }
    fn armed(s: &MemStore, input: &PrepareInput) -> DeliveryRecord {
        let prepared = prepare(s, input);
        match s
            .apply_delivery(DeliveryCommand::Arm(arm_input(input, prepared.revision)))
            .unwrap()
        {
            DeliveryResult::Armed { record } => record,
            result => panic!("expected Armed, got {result:?}"),
        }
    }
    fn settle(s: &MemStore, input: &PrepareInput, evidence: DeliveryEvidence) -> DeliveryResult {
        let revision = s.inspect_delivery(&input.key).unwrap().unwrap().revision;
        s.apply_delivery(DeliveryCommand::Settle(SettleInput {
            key: input.key.clone(),
            revision,
            attempt: input.attempt.clone(),
            evidence,
            detail: String::new(),
            now: At::from_unix_millis(3000),
        }))
        .unwrap()
    }
    fn view(s: &MemStore, input: &PrepareInput) -> DeliveryView {
        s.inspect_delivery(&input.key).unwrap().unwrap()
    }
    fn ack(s: &MemStore, input: &PrepareInput, at: u64) {
        s.apply_delivery(DeliveryCommand::ManualAck(ManualAckInput {
            key: input.key.clone(),
            at: At::from_unix_millis(at),
        }))
        .unwrap();
    }
    fn next_input(n: u64) -> (PrepareInput, WireMessage) {
        let mut input = base_input();
        let mut message = fixture_message();
        message.id = MessageId(seq_iri(10_000 + n));
        message.thread_id = message.id.clone();
        message.to = crate::ferrowire::WireAddress::AgentName(format!("bob-{n}"));
        input.key = DeliveryKey {
            original_id: message.id.clone(),
            recipient_id: AgentId(seq_iri(20_000 + n)),
        };
        input.binding.recipient_id = input.key.recipient_id.clone();
        input.attempt.token = PhaseToken([n as u8; 32]);
        input.generation = canonical_generation(&message).unwrap();
        (input, message)
    }

    #[test]
    fn managed_mem_shared_conformance() {
        let s = MemStore::default();
        install(&s);
        let agent = s
            .inner
            .borrow()
            .agents
            .get(&base_input().key.recipient_id)
            .unwrap()
            .clone();
        {
            let mut inner = s.inner.borrow_mut();
            inner.agents.clear();
            inner.wire_messages.clear();
            inner.wire_inbox.clear();
        }
        crate::wire_delivery::conformance::delivery_conformance(&s, |message| {
            s.import_registry_events(&[RegistryEvent {
                event_id: seq_iri(9003),
                at: At::from_unix_millis(1),
                by: "fixture".into(),
                kind: RegistryEventKind::AgentUpsert(agent.clone()),
            }])
            .unwrap();
            let report = s
                .import_wire_batch(std::slice::from_ref(message), &[])
                .unwrap();
            assert_eq!(report.imported_messages, 1);
        });
    }

    #[test]
    fn managed_stale_wrong_worker_and_digest_fail_closed() {
        for case in 0..7 {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            if case == 0 {
                let mut bad = input.clone();
                bad.generation = Digest([0; 32]);
                assert!(matches!(
                    s.apply_delivery(DeliveryCommand::Prepare(bad)).unwrap(),
                    DeliveryResult::Refused {
                        reason: HoldReason::GenerationConflict
                    }
                ));
                assert!(s.inspect_delivery(&input.key).unwrap().is_none());
                continue;
            }
            let prepared = prepare(&s, &input);
            let mut arm = arm_input(&input, prepared.revision);
            match case {
                1 => arm.revision = Revision(0),
                2 => arm.attempt.worker = WorkerId([0; 32]),
                3 => {
                    s.inner
                        .borrow_mut()
                        .wire_messages
                        .get_mut(&input.key.original_id)
                        .unwrap()
                        .body
                        .push('!');
                }
                4 => arm.frozen.digest = Digest([0; 32]),
                5 => {
                    s.inner.borrow_mut().delivery.admissions.clear();
                }
                6 => arm.route.thread_id = "other".into(),
                _ => unreachable!(),
            }
            assert!(matches!(
                s.apply_delivery(DeliveryCommand::Arm(arm)).unwrap(),
                DeliveryResult::Refused { .. }
            ));
            assert_eq!(view(&s, &input).eligibility, Eligibility::Prepared);
            assert_eq!(
                s.inner
                    .borrow()
                    .delivery
                    .global
                    .as_ref()
                    .unwrap()
                    .active_attempts,
                1
            );
        }
    }

    #[test]
    fn managed_expiry_ack_and_invalidation_race_arm() {
        for case in 0..4 {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            let p = prepare(&s, &input);
            let mut arm = arm_input(&input, p.revision);
            match case {
                0 => {
                    arm.now = At::from_unix_millis(input.now.unix_millis() + 20_000);
                }
                1 => ack(&s, &input, 2100),
                2 => {
                    s.apply_delivery(DeliveryCommand::InvalidatePrepared {
                        key: input.key.clone(),
                        revision: p.revision,
                        now: At::from_unix_millis(22_000),
                    })
                    .unwrap();
                }
                3 => {
                    s.inner
                        .borrow_mut()
                        .wire_messages
                        .get_mut(&input.key.original_id)
                        .unwrap()
                        .deadline_at = Some(At::from_unix_millis(2001));
                }
                _ => unreachable!(),
            }
            assert!(matches!(
                s.apply_delivery(DeliveryCommand::Arm(arm)).unwrap(),
                DeliveryResult::Refused { .. }
            ));
            assert_eq!(view(&s, &input).prepares, 1);
        }
        let s = MemStore::default();
        install(&s);
        let input = base_input();
        armed(&s, &input);
        let before = view(&s, &input);
        assert!(matches!(
            s.apply_delivery(DeliveryCommand::InvalidatePrepared {
                key: input.key.clone(),
                revision: before.revision,
                now: At::from_unix_millis(u64::MAX)
            })
            .unwrap(),
            DeliveryResult::Refused { .. }
        ));
        assert_eq!(view(&s, &input), before);
    }

    #[test]
    fn managed_ack_preserves_independent_evidence() {
        for evidence in [
            None,
            Some(DeliveryEvidence::VendorQueued),
            Some(DeliveryEvidence::MutationUnconfirmed),
        ] {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            armed(&s, &input);
            if let Some(evidence) = evidence {
                settle(&s, &input, evidence);
            }
            let before = view(&s, &input);
            ack(&s, &input, 4000);
            let after = view(&s, &input);
            assert_eq!(before.attempts, after.attempts);
            assert_eq!(before.prepares, after.prepares);
            assert_eq!(before.barrier, after.barrier);
            assert_eq!(before.eligibility, after.eligibility);
            assert_eq!(
                after.acknowledgment.manual_at,
                Some(At::from_unix_millis(4000))
            );
            ack(&s, &input, 5000);
            assert_eq!(view(&s, &input), after);
            let inner = s.inner.borrow();
            let receipt = inner
                .wire_inbox
                .get(&(
                    input.key.recipient_id.iri().clone(),
                    input.key.original_id.clone(),
                ))
                .unwrap();
            assert_eq!(receipt.status, DeliveryStatus::Acked);
        }
        // Enrollment's legacy provenance has no invented attempt/evidence.
        let s = MemStore::default();
        install(&s);
        let input = base_input();
        let mut ctx = s
            .inner
            .borrow_mut()
            .delivery_context(&DeliveryCommand::Prepare(input.clone()))
            .unwrap()
            .unwrap();
        ctx.record = Some(DeliveryRecord {
            schema: DELIVERY_SCHEMA_VERSION,
            key: input.key.clone(),
            generation: input.generation,
            binding: input.binding.clone(),
            revision: Revision(1),
            eligibility: Eligibility::Held,
            prepares: 0,
            attempts: vec![],
            acknowledgment: AcknowledgmentState {
                manual_at: None,
                legacy_unknown: true,
            },
            held: Some(HoldReason::LegacyUncertain),
            annotation: String::new(),
        });
        ctx.reservation = Some(IdReservation {
            original_id: input.key.original_id.clone(),
            generation: input.generation,
        });
        {
            let mut inner = s.inner.borrow_mut();
            inner
                .delivery
                .records
                .insert(input.key.clone(), ctx.record.unwrap());
            inner
                .delivery
                .reservations
                .insert(input.key.original_id.clone(), ctx.reservation.unwrap());
        }
        ack(&s, &input, 4000);
        let v = view(&s, &input);
        assert!(v.acknowledgment.legacy_unknown);
        assert!(v.acknowledgment.manual_at.is_some());
        assert!(v.attempts.is_empty());
        assert_eq!(v.prepares, 0);
    }

    #[test]
    fn managed_settlement_projection_contradiction_and_bounds() {
        for transport in [Transport::Queue, Transport::Steer, Transport::Stdout] {
            let s = MemStore::default();
            install(&s);
            let mut input = base_input();
            if transport == Transport::Stdout {
                input.attempt.lane = Lane::Hook;
            }
            let p = prepare(&s, &input);
            let mut arm = arm_input(&input, p.revision);
            arm.frozen.transport = transport;
            arm.frozen.expected_turn = (transport == Transport::Steer).then(|| "turn".into());
            s.apply_delivery(DeliveryCommand::Arm(arm)).unwrap();
            let evidence = match transport {
                Transport::Queue => DeliveryEvidence::VendorQueued,
                Transport::Steer => DeliveryEvidence::AppServerAccepted {
                    thread: "thread".into(),
                    turn: "turn".into(),
                },
                Transport::Stdout => DeliveryEvidence::HookEmissionCompleted,
            };
            settle(&s, &input, evidence.clone());
            let before = view(&s, &input);
            settle(&s, &input, evidence);
            assert_eq!(view(&s, &input), before);
            assert_eq!(
                s.inner
                    .borrow()
                    .wire_inbox
                    .get(&(
                        input.key.recipient_id.iri().clone(),
                        input.key.original_id.clone()
                    ))
                    .unwrap()
                    .status,
                DeliveryStatus::Delivered
            );
            settle(&s, &input, DeliveryEvidence::MutationUnconfirmed);
            let after = view(&s, &input);
            assert_eq!(after.eligibility, Eligibility::Held);
            assert_eq!(after.attempts, before.attempts);
            assert_eq!(after.barrier, Some(input.key.clone()));
        }
        let s = MemStore::default();
        install(&s);
        let input = base_input();
        let a = armed(&s, &input);
        let before = s
            .inner
            .borrow()
            .wire_inbox
            .get(&(
                input.key.recipient_id.iri().clone(),
                input.key.original_id.clone(),
            ))
            .unwrap()
            .clone();
        s.apply_delivery(DeliveryCommand::Settle(SettleInput {
            key: input.key.clone(),
            revision: a.revision,
            attempt: input.attempt.clone(),
            evidence: DeliveryEvidence::MutationUnconfirmed,
            detail: "é".repeat(400),
            now: At::from_unix_millis(3000),
        }))
        .unwrap();
        let v = view(&s, &input);
        assert_eq!(v.held, Some(HoldReason::EvidenceOverflow));
        assert_eq!(v.attempts[0].detail.chars().count(), 300);
        assert_eq!(
            *s.inner
                .borrow()
                .wire_inbox
                .get(&(
                    input.key.recipient_id.iri().clone(),
                    input.key.original_id.clone()
                ))
                .unwrap(),
            before
        );
        s.apply_delivery(DeliveryCommand::Annotate {
            key: input.key.clone(),
            text: "x".repeat(4097),
        })
        .unwrap();
        assert!(view(&s, &input).annotation.is_empty());
    }

    #[test]
    fn managed_uncertainty_barrier_new_ids_and_one_frozen_copy() {
        let s = MemStore::default();
        install(&s);
        let mut input = base_input();
        for n in 0..3 {
            input.attempt.token = PhaseToken([n; 32]);
            armed(&s, &input);
            assert_eq!(
                s.inner
                    .borrow()
                    .delivery
                    .records
                    .get(&input.key)
                    .unwrap()
                    .attempts
                    .iter()
                    .filter(|a| a.frozen.is_some())
                    .count(),
                1
            );
            settle(&s, &input, DeliveryEvidence::DefinitiveNoMutation);
        }
        let s = MemStore::default();
        install(&s);
        let input = base_input();
        armed(&s, &input);
        settle(&s, &input, DeliveryEvidence::MutationUnconfirmed);
        ack(&s, &input, 4000);
        s.apply_delivery(DeliveryCommand::Annotate {
            key: input.key.clone(),
            text: "reviewed".into(),
        })
        .unwrap();
        s.apply_delivery(DeliveryCommand::Close {
            key: input.key.clone(),
        })
        .unwrap();
        let mut new_input = input.clone();
        let mut message = fixture_message();
        message.id = MessageId(seq_iri(9999));
        new_input.key.original_id = message.id.clone();
        new_input.generation = canonical_generation(&message).unwrap();
        install_input(&s, &new_input, message);
        assert!(matches!(
            s.apply_delivery(DeliveryCommand::Prepare(new_input))
                .unwrap(),
            DeliveryResult::Refused {
                reason: HoldReason::Unconfirmed
            }
        ));
        assert_eq!(view(&s, &input).barrier, Some(input.key.clone()));
    }

    #[test]
    fn managed_capacity_predicates_are_atomic() {
        let s = MemStore::default();
        for n in 0..15 {
            let (input, message) = next_input(n);
            install_input(&s, &input, message);
            armed(&s, &input);
        }
        let (a, ma) = next_input(15);
        let (b, mb) = next_input(16);
        install_input(&s, &a, ma);
        install_input(&s, &b, mb);
        prepare(&s, &a);
        assert!(matches!(
            s.apply_delivery(DeliveryCommand::Prepare(b.clone()))
                .unwrap(),
            DeliveryResult::Refused {
                reason: HoldReason::Capacity
            }
        ));
        assert!(s.inspect_delivery(&b.key).unwrap().is_none());
        assert_eq!(s.active_delivery_owners().unwrap().len(), 16);
        let s = MemStore::default();
        install(&s);
        let first = base_input();
        let mut second = first.clone();
        let mut message = fixture_message();
        message.id = MessageId(seq_iri(999));
        second.key.original_id = message.id.clone();
        second.generation = canonical_generation(&message).unwrap();
        install_input(&s, &second, message);
        prepare(&s, &second);
        assert!(matches!(
            s.apply_delivery(DeliveryCommand::Prepare(first.clone()))
                .unwrap(),
            DeliveryResult::Refused {
                reason: HoldReason::Capacity
            }
        ));
        assert!(s.inspect_delivery(&first.key).unwrap().is_none());
    }

    fn end_scope(s: &MemStore, input: &PrepareInput) {
        s.inner
            .borrow_mut()
            .delivery
            .ended_scopes
            .insert(input.attempt.lifetime.id, input.attempt.lifetime.clone());
    }
    fn target(s: &MemStore, input: &PrepareInput) -> CapacityRetirementTarget {
        s.active_delivery_owners()
            .unwrap()
            .into_iter()
            .find(|t| t.owner.key == input.key)
            .unwrap()
    }

    #[test]
    fn managed_capacity_retirement_preserves_uncertainty() {
        let s = MemStore::default();
        let mut inputs = vec![];
        for n in 0..16 {
            let (input, message) = next_input(n);
            install_input(&s, &input, message);
            armed(&s, &input);
            inputs.push(input);
        }
        let input = &inputs[0];
        let before = view(&s, input);
        let t = target(&s, input);
        assert!(matches!(
            s.retire_orphan_capacity(t.clone()).unwrap(),
            CapacityRetirementResult::Unproven
        ));
        end_scope(&s, input);
        assert!(matches!(
            s.retire_orphan_capacity(t.clone()).unwrap(),
            CapacityRetirementResult::Retired { .. }
        ));
        let after = view(&s, input);
        assert_eq!(after.eligibility, before.eligibility);
        assert_eq!(after.acknowledgment, before.acknowledgment);
        assert_eq!(after.prepares, before.prepares);
        assert_eq!(after.attempts[0].evidence, before.attempts[0].evidence);
        assert_eq!(after.barrier, before.barrier);
        assert_eq!(after.attempts[0].capacity, CapacityState::Retired);
        assert!(matches!(
            s.retire_orphan_capacity(t).unwrap(),
            CapacityRetirementResult::AlreadyReleased { .. }
        ));
        let (fresh, message) = next_input(17);
        install_input(&s, &fresh, message);
        prepare(&s, &fresh);
        assert_eq!(s.active_delivery_owners().unwrap().len(), 16);
        assert!(matches!(
            s.apply_delivery(DeliveryCommand::Prepare(input.clone()))
                .unwrap(),
            DeliveryResult::Refused { .. }
        ));
        let mut later = input.clone();
        let mut message = fixture_message();
        message.id = MessageId(seq_iri(999));
        message.to = crate::ferrowire::WireAddress::AgentName("bob-0".into());
        later.key.original_id = message.id.clone();
        later.generation = canonical_generation(&message).unwrap();
        install_input(&s, &later, message);
        assert!(matches!(
            s.apply_delivery(DeliveryCommand::Prepare(later)).unwrap(),
            DeliveryResult::Refused {
                reason: HoldReason::Unconfirmed
            }
        ));
    }

    #[test]
    fn managed_capacity_retirement_race_orders() {
        for retirement_first in [false, true] {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            armed(&s, &input);
            let t = target(&s, &input);
            end_scope(&s, &input);
            if retirement_first {
                s.retire_orphan_capacity(t.clone()).unwrap();
                let before = view(&s, &input);
                settle(&s, &input, DeliveryEvidence::VendorQueued);
                let after = view(&s, &input);
                assert_eq!(after.attempts[0].capacity, CapacityState::Retired);
                assert_eq!(after.barrier, before.barrier);
                assert_ne!(after.eligibility, Eligibility::Eligible);
            } else {
                settle(&s, &input, DeliveryEvidence::VendorQueued);
            }
            assert!(matches!(
                s.retire_orphan_capacity(t).unwrap(),
                CapacityRetirementResult::AlreadyReleased { .. }
            ));
            assert!(s.active_delivery_owners().unwrap().is_empty());
        }
        let s = MemStore::default();
        install(&s);
        let input = base_input();
        armed(&s, &input);
        let stale = target(&s, &input);
        end_scope(&s, &input);
        ack(&s, &input, 4000);
        assert!(matches!(
            s.retire_orphan_capacity(stale).unwrap(),
            CapacityRetirementResult::Stale
        ));
        assert_eq!(s.active_delivery_owners().unwrap().len(), 1);
        let fresh = target(&s, &input);
        s.inner
            .borrow_mut()
            .delivery
            .ended_scopes
            .get_mut(&input.attempt.lifetime.id)
            .unwrap()
            .revision = Revision(2);
        assert!(matches!(
            s.retire_orphan_capacity(fresh.clone()).unwrap(),
            CapacityRetirementResult::Unproven
        ));
        end_scope(&s, &input);
        s.retire_orphan_capacity(fresh).unwrap();
        assert!(s.active_delivery_owners().unwrap().is_empty());
    }

    #[test]
    fn managed_missing_metadata_never_rebuilds_from_history() {
        for case in 0..6 {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            prepare(&s, &input);
            {
                let mut inner = s.inner.borrow_mut();
                match case {
                    0 => inner.delivery.global = None,
                    1 => inner.delivery.continuity = None,
                    2 => {
                        inner.delivery.recipients.clear();
                    }
                    3 => {
                        inner.delivery.reservations.clear();
                    }
                    4 => {
                        inner.delivery.identities.clear();
                    }
                    5 => {
                        inner.delivery.receipt_keys.clear();
                    }
                    _ => unreachable!(),
                }
            }
            assert!(matches!(
                s.apply_delivery(DeliveryCommand::Arm(arm_input(&input, Revision(1))))
                    .unwrap(),
                DeliveryResult::Refused { .. }
            ));
        }
    }

    #[test]
    fn managed_hot_context_independent_of_retained_history() {
        type AccessTrace = (Vec<(&'static str, bool)>, Vec<&'static str>);
        fn run(history: usize) -> Vec<AccessTrace> {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            // Direct module-local historical fixtures, absent from the bounded active index.
            let donor = armed(&s, &input);
            settle(&s, &input, DeliveryEvidence::VendorQueued);
            {
                let mut inner = s.inner.borrow_mut();
                inner.delivery.records.clear();
                inner.delivery.reservations.clear();
                inner
                    .delivery
                    .recipients
                    .get_mut(&input.key.recipient_id)
                    .unwrap()
                    .revision = Revision(1);
                inner
                    .wire_inbox
                    .get_mut(&(
                        input.key.recipient_id.iri().clone(),
                        input.key.original_id.clone(),
                    ))
                    .unwrap()
                    .status = DeliveryStatus::Pending;
                for n in 0..history {
                    let (other, message) = next_input(n as u64);
                    let mut row = donor.clone();
                    row.key = other.key.clone();
                    row.binding = other.binding.clone();
                    row.eligibility = if n % 2 == 0 {
                        Eligibility::Held
                    } else {
                        Eligibility::Settled
                    };
                    row.attempts[0].capacity = CapacityState::Released;
                    inner.delivery.records.insert(other.key.clone(), row);
                    inner.delivery.reservations.insert(
                        message.id.clone(),
                        IdReservation {
                            original_id: message.id,
                            generation: other.generation,
                        },
                    );
                    inner.delivery.recipients.insert(
                        other.key.recipient_id.clone(),
                        RecipientControl {
                            binding: other.binding,
                            revision: Revision(1),
                            active: None,
                            barrier: None,
                            identity_conflict: false,
                        },
                    );
                    inner
                        .delivery
                        .identities
                        .insert(format!("old-alias-{n}"), IdentityResolution::Conflict);
                }
                inner.delivery.access = DeliveryAccess::default();
            }
            let mut out = vec![];
            let capture = |out: &mut Vec<_>| {
                let access = std::mem::take(&mut s.inner.borrow_mut().delivery.access);
                out.push((access.reads, access.writes));
            };
            s.apply_delivery(DeliveryCommand::Prepare(input.clone()))
                .unwrap();
            capture(&mut out);
            s.apply_delivery(DeliveryCommand::Arm(arm_input(&input, Revision(1))))
                .unwrap();
            capture(&mut out);
            s.apply_delivery(DeliveryCommand::Settle(SettleInput {
                key: input.key.clone(),
                revision: Revision(2),
                attempt: input.attempt.clone(),
                evidence: DeliveryEvidence::VendorQueued,
                detail: String::new(),
                now: At::from_unix_millis(3000),
            }))
            .unwrap();
            capture(&mut out);
            ack(&s, &input, 4000);
            capture(&mut out);
            view(&s, &input);
            capture(&mut out);
            out
        }
        let empty = run(0);
        let retained = run(512);
        assert_eq!(empty, retained);
        assert_eq!(
            empty[0].0.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            vec![
                "record",
                "reservation",
                "recipient",
                "global",
                "continuity",
                "message",
                "receipt_index",
                "receipt",
                "registry",
                "selector_index",
                "receipt_alias_index",
                "admission"
            ]
        );
        assert_eq!(empty[4].0.len(), 2);
        assert!(!empty[0].1.is_empty());
    }

    #[test]
    fn managed_retired_late_nonmutation_and_historical_release_cannot_reopen() {
        let s = MemStore::default();
        install(&s);
        let mut input = base_input();
        armed(&s, &input);
        end_scope(&s, &input);
        s.retire_orphan_capacity(target(&s, &input)).unwrap();
        let before = view(&s, &input);
        let result = s
            .apply_delivery(DeliveryCommand::Release(ReleaseInput {
                key: input.key.clone(),
                revision: before.revision,
                attempt: input.attempt.clone(),
                proof: DeliveryEvidence::DefinitiveNoMutation,
                now: At::from_unix_millis(4000),
            }))
            .unwrap();
        assert!(matches!(result, DeliveryResult::Refused { .. }));
        assert_eq!(view(&s, &input), before);
        settle(&s, &input, DeliveryEvidence::DefinitiveNoMutation);
        let after = view(&s, &input);
        assert_eq!(after.eligibility, before.eligibility);
        assert_eq!(after.barrier, before.barrier);
        assert_eq!(after.attempts[0].capacity, CapacityState::Retired);
        // Exact historical retirement must never clear a later prepared owner.
        let s = MemStore::default();
        install(&s);
        armed(&s, &input);
        let old = target(&s, &input);
        end_scope(&s, &input);
        settle(&s, &input, DeliveryEvidence::DefinitiveNoMutation);
        input.attempt.token = PhaseToken([44; 32]);
        prepare(&s, &input);
        let new_owner = target(&s, &input);
        assert!(matches!(
            s.retire_orphan_capacity(old).unwrap(),
            CapacityRetirementResult::AlreadyReleased { .. }
        ));
        assert_eq!(s.active_delivery_owners().unwrap(), vec![new_owner]);
    }

    fn compact_delivery_bytes(record: &DeliveryRecord) -> usize {
        let mut compact = record.clone();
        for attempt in &mut compact.attempts {
            if let Some(frozen) = &mut attempt.frozen {
                frozen.bytes.clear();
            }
        }
        serde_json::to_vec(&compact).unwrap().len()
    }

    #[test]
    fn managed_annotation_accounts_for_json_expansion() {
        for (text, accepted) in [
            ("\0".repeat(4091), false),
            ("\u{1}".repeat(4091), false),
            ("x".repeat(4091), true),
            ("\"".repeat(4091), true),
            ("\\".repeat(4091), true),
            ("😀".repeat(1022), true),
        ] {
            let s = MemStore::default();
            install(&s);
            let mut input = base_input();
            input.route.thread_id = "t".repeat(2000);
            input.route.executable = Some("e".repeat(2000));
            for token in 0..3 {
                input.attempt.token = PhaseToken([token; 32]);
                let p = prepare(&s, &input);
                assert!(matches!(
                    s.apply_delivery(DeliveryCommand::Release(ReleaseInput {
                        key: input.key.clone(),
                        revision: p.revision,
                        attempt: input.attempt.clone(),
                        proof: DeliveryEvidence::PreMutationRefused,
                        now: At::from_unix_millis(2100),
                    }))
                    .unwrap(),
                    DeliveryResult::Applied { .. }
                ));
            }
            s.apply_delivery(DeliveryCommand::Annotate {
                key: input.key.clone(),
                text: "prior".into(),
            })
            .unwrap();
            ack(&s, &input, 4000);
            let before = view(&s, &input);
            s.apply_delivery(DeliveryCommand::Annotate {
                key: input.key.clone(),
                text: text.clone(),
            })
            .unwrap();
            let after = view(&s, &input);
            let row = s.inner.borrow().delivery.records[&input.key].clone();
            assert!(compact_delivery_bytes(&row) <= 32_768);
            assert_eq!(
                after.annotation,
                if accepted {
                    format!("prior{text}")
                } else {
                    "prior".into()
                }
            );
            assert_eq!(after.attempts, before.attempts);
            assert_eq!(after.prepares, before.prepares);
            assert_eq!(after.acknowledgment, before.acknowledgment);
            assert_eq!(after.barrier, before.barrier);
            if !accepted {
                assert_eq!(after.eligibility, Eligibility::Held);
                assert_eq!(after.held, Some(HoldReason::EvidenceOverflow));
            }
            assert!(s.active_delivery_owners().unwrap().is_empty());
        }
    }

    #[test]
    fn managed_escaped_annotation_uses_remaining_row_capacity() {
        let s = MemStore::default();
        install(&s);
        let input = base_input();
        prepare(&s, &input);
        s.apply_delivery(DeliveryCommand::Annotate {
            key: input.key.clone(),
            text: "\0".repeat(4096),
        })
        .unwrap();
        assert_eq!(view(&s, &input).annotation.len(), 4096);
        assert_eq!(view(&s, &input).eligibility, Eligibility::Prepared);
        assert!(compact_delivery_bytes(&s.inner.borrow().delivery.records[&input.key]) <= 32_768);
        // Raw UTF-8 bytes remain independently bounded even with row space left.
        s.apply_delivery(DeliveryCommand::Annotate {
            key: input.key.clone(),
            text: "é".into(),
        })
        .unwrap();
        let v = view(&s, &input);
        assert_eq!(v.annotation, "\0".repeat(4096));
        assert_eq!(v.held, Some(HoldReason::EvidenceOverflow));
        assert_eq!(v.eligibility, Eligibility::Held);
        assert!(s.active_delivery_owners().unwrap().is_empty());
        ack(&s, &input, 4000);
        assert!(view(&s, &input).acknowledgment.manual_at.is_some());
        assert!(s.active_delivery_owners().unwrap().is_empty());
    }

    #[test]
    fn managed_annotation_overflow_preserves_armed_safety_operations() {
        for retire in [false, true] {
            let s = MemStore::default();
            install(&s);
            let mut input = base_input();
            input.route.thread_id = "t".repeat(2000);
            input.route.executable = Some("e".repeat(2000));
            for token in 0..2 {
                input.attempt.token = PhaseToken([token; 32]);
                let p = prepare(&s, &input);
                s.apply_delivery(DeliveryCommand::Release(ReleaseInput {
                    key: input.key.clone(),
                    revision: p.revision,
                    attempt: input.attempt.clone(),
                    proof: DeliveryEvidence::PreMutationRefused,
                    now: At::from_unix_millis(2100),
                }))
                .unwrap();
            }
            input.attempt.token = PhaseToken([2; 32]);
            armed(&s, &input);
            s.apply_delivery(DeliveryCommand::Annotate {
                key: input.key.clone(),
                text: "prior".into(),
            })
            .unwrap();
            let before = view(&s, &input);
            s.apply_delivery(DeliveryCommand::Annotate {
                key: input.key.clone(),
                text: "\0".repeat(4091),
            })
            .unwrap();
            let overflow = view(&s, &input);
            assert_eq!(overflow.held, Some(HoldReason::EvidenceOverflow));
            assert_eq!(overflow.eligibility, Eligibility::Held);
            assert_eq!(overflow.annotation, before.annotation);
            assert_eq!(overflow.attempts, before.attempts);
            assert_eq!(overflow.acknowledgment, before.acknowledgment);
            assert_eq!(overflow.prepares, before.prepares);
            assert_eq!(overflow.barrier, before.barrier);
            assert_eq!(s.active_delivery_owners().unwrap().len(), 1);
            ack(&s, &input, 4000);
            assert_eq!(s.active_delivery_owners().unwrap().len(), 1);
            let retirement = target(&s, &input);
            if retire {
                end_scope(&s, &input);
                assert!(matches!(
                    s.retire_orphan_capacity(retirement.clone()).unwrap(),
                    CapacityRetirementResult::Retired { .. }
                ));
            }
            settle(&s, &input, DeliveryEvidence::VendorQueued);
            let settled = view(&s, &input);
            assert_eq!(
                settled.attempts[2].evidence,
                Some(DeliveryEvidence::VendorQueued)
            );
            assert_eq!(
                settled.attempts[2].capacity,
                if retire {
                    CapacityState::Retired
                } else {
                    CapacityState::Released
                }
            );
            assert!(settled.acknowledgment.manual_at.is_some());
            assert!(s.active_delivery_owners().unwrap().is_empty());
            assert!(
                compact_delivery_bytes(&s.inner.borrow().delivery.records[&input.key]) <= 32_768
            );
            settle(&s, &input, DeliveryEvidence::VendorQueued);
            assert_eq!(view(&s, &input), settled);
            end_scope(&s, &input);
            assert!(matches!(
                s.retire_orphan_capacity(retirement).unwrap(),
                CapacityRetirementResult::AlreadyReleased { .. }
            ));
            assert!(s.active_delivery_owners().unwrap().is_empty());
            if retire {
                assert_eq!(settled.eligibility, overflow.eligibility);
                assert_eq!(settled.held, overflow.held);
                assert_eq!(settled.barrier, overflow.barrier);
            }
            assert!(matches!(
                s.apply_delivery(DeliveryCommand::Prepare(input.clone()))
                    .unwrap(),
                DeliveryResult::Refused { .. }
            ));
        }
    }

    fn malformed_prepared_command(case: usize) {
        for corruption in 0..5 {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            let p = prepare(&s, &input);
            {
                let mut inner = s.inner.borrow_mut();
                let record = inner.delivery.records.get_mut(&input.key).unwrap();
                match corruption {
                    0 => {
                        record.attempts.clear();
                        record.prepares = 0;
                    }
                    1 => record.attempts[0].armed_at = Some(At::from_unix_millis(2001)),
                    2 => record.attempts[0].frozen = Some(arm_input(&input, p.revision).frozen),
                    3 => record.attempts[0].evidence = Some(DeliveryEvidence::PreMutationRefused),
                    4 => record.attempts[0].capacity = CapacityState::Released,
                    _ => unreachable!(),
                }
            }
            let command = match case {
                0 => DeliveryCommand::ManualAck(ManualAckInput {
                    key: input.key.clone(),
                    at: At::from_unix_millis(4000),
                }),
                1 => DeliveryCommand::InvalidatePrepared {
                    key: input.key.clone(),
                    revision: p.revision,
                    now: At::from_unix_millis(4000),
                },
                2 => DeliveryCommand::Annotate {
                    key: input.key.clone(),
                    text: "x".repeat(4097),
                },
                3 => DeliveryCommand::Close {
                    key: input.key.clone(),
                },
                _ => unreachable!(),
            };
            let before = s
                .inner
                .borrow_mut()
                .delivery_context(&command)
                .unwrap()
                .unwrap();
            s.inner.borrow_mut().delivery.access = DeliveryAccess::default();
            assert_eq!(
                s.apply_delivery(command.clone()).unwrap(),
                DeliveryResult::Refused {
                    reason: HoldReason::Corrupt
                }
            );
            assert!(s.inner.borrow().delivery.access.writes.is_empty());
            let after = s
                .inner
                .borrow_mut()
                .delivery_context(&command)
                .unwrap()
                .unwrap();
            assert_eq!(after, before);
        }
    }

    #[test]
    fn managed_malformed_prepared_ack_refuses_without_writes() {
        malformed_prepared_command(0);
    }
    #[test]
    fn managed_malformed_prepared_invalidation_refuses_without_writes() {
        malformed_prepared_command(1);
    }
    #[test]
    fn managed_malformed_prepared_annotation_refuses_without_writes() {
        malformed_prepared_command(2);
    }
    #[test]
    fn managed_malformed_prepared_close_refuses_without_writes() {
        malformed_prepared_command(3);
    }

    #[test]
    fn managed_bounds_and_overflow_do_not_reopen_or_block_safety_flags() {
        for bytes in [1_048_576 + 15, 1_048_576 + 14] {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            let p = prepare(&s, &input);
            let mut arm = arm_input(&input, p.revision);
            arm.frozen.bytes = vec![b'x'; bytes];
            arm.frozen.digest =
                canonical_payload(input.generation, &arm.frozen.renderer, &arm.frozen.bytes)
                    .unwrap();
            let result = s.apply_delivery(DeliveryCommand::Arm(arm)).unwrap();
            assert_eq!(
                matches!(result, DeliveryResult::Armed { .. }),
                bytes == 1_048_576 + 14
            );
        }
        let s = MemStore::default();
        install(&s);
        let input = base_input();
        let p = prepare(&s, &input);
        s.apply_delivery(DeliveryCommand::Annotate {
            key: input.key.clone(),
            text: "x".repeat(4097),
        })
        .unwrap();
        assert!(matches!(
            s.apply_delivery(DeliveryCommand::Arm(arm_input(&input, p.revision)))
                .unwrap(),
            DeliveryResult::Refused { .. }
        ));
        assert_eq!(view(&s, &input).eligibility, Eligibility::Held);
        assert!(s.active_delivery_owners().unwrap().is_empty());
        ack(&s, &input, 5000);
        assert!(view(&s, &input).acknowledgment.manual_at.is_some());
        let s = MemStore::default();
        install(&s);
        let mut input = base_input();
        input.route.thread_id = "x".repeat(4097);
        assert!(matches!(
            s.apply_delivery(DeliveryCommand::Prepare(input)).unwrap(),
            DeliveryResult::Refused {
                reason: HoldReason::EvidenceOverflow
            }
        ));
        let s = MemStore::default();
        install(&s);
        let mut input = base_input();
        input.route.thread_id = "x".repeat(2000);
        let p = prepare(&s, &input);
        let mut arm = arm_input(&input, p.revision);
        arm.frozen.transport = Transport::Steer;
        arm.frozen.expected_turn = Some("y".repeat(2000));
        assert!(matches!(
            s.apply_delivery(DeliveryCommand::Arm(arm)).unwrap(),
            DeliveryResult::Refused {
                reason: HoldReason::EvidenceOverflow
            }
        ));
    }

    #[test]
    fn managed_corrupt_capacity_and_missing_reservation_refuse_prepare() {
        for case in 0..5 {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            let p = prepare(&s, &input);
            s.apply_delivery(DeliveryCommand::Release(ReleaseInput {
                key: input.key.clone(),
                revision: p.revision,
                attempt: input.attempt.clone(),
                proof: DeliveryEvidence::PreMutationRefused,
                now: At::from_unix_millis(2100),
            }))
            .unwrap();
            {
                let mut inner = s.inner.borrow_mut();
                match case {
                    0 => inner.delivery.reservations.clear(),
                    1 => inner.delivery.global.as_mut().unwrap().active_attempts = 1,
                    2 => {
                        let owner = ActiveOwner {
                            key: input.key.clone(),
                            attempt: input.attempt.clone(),
                        };
                        let global = inner.delivery.global.as_mut().unwrap();
                        global.active_index = vec![owner.clone(), owner];
                        global.active_attempts = 2;
                    }
                    3 => {
                        let owner = ActiveOwner {
                            key: input.key.clone(),
                            attempt: input.attempt.clone(),
                        };
                        let global = inner.delivery.global.as_mut().unwrap();
                        global.active_index = vec![owner];
                        global.active_attempts = 1;
                    }
                    4 => {
                        inner
                            .delivery
                            .recipients
                            .get_mut(&input.key.recipient_id)
                            .unwrap()
                            .active = Some(ActiveOwner {
                            key: input.key.clone(),
                            attempt: input.attempt.clone(),
                        });
                    }
                    _ => unreachable!(),
                }
            }
            assert!(matches!(
                s.apply_delivery(DeliveryCommand::Prepare(input.clone()))
                    .unwrap(),
                DeliveryResult::Refused {
                    reason: HoldReason::Corrupt
                }
            ));
            assert_eq!(view(&s, &input).prepares, 1);
        }
    }

    #[test]
    fn managed_corrupt_receipt_and_prepared_phase_cannot_mutate() {
        for case in 0..3 {
            let s = MemStore::default();
            install(&s);
            let input = base_input();
            let p = prepare(&s, &input);
            match case {
                0 => {
                    let mut inner = s.inner.borrow_mut();
                    inner
                        .wire_inbox
                        .get_mut(&(
                            input.key.recipient_id.iri().clone(),
                            input.key.original_id.clone(),
                        ))
                        .unwrap()
                        .recipient_id = seq_iri(42);
                }
                1 => {
                    let mut inner = s.inner.borrow_mut();
                    inner.delivery.records.get_mut(&input.key).unwrap().attempts[0].armed_at =
                        Some(At::from_unix_millis(2001));
                }
                2 => {
                    let mut inner = s.inner.borrow_mut();
                    inner.delivery.records.get_mut(&input.key).unwrap().attempts[0].evidence =
                        Some(DeliveryEvidence::PreMutationRefused);
                }
                _ => unreachable!(),
            }
            let command = if case == 0 {
                DeliveryCommand::ManualAck(ManualAckInput {
                    key: input.key.clone(),
                    at: At::from_unix_millis(4000),
                })
            } else {
                DeliveryCommand::Arm(arm_input(&input, p.revision))
            };
            assert!(matches!(
                s.apply_delivery(command).unwrap(),
                DeliveryResult::Refused {
                    reason: HoldReason::Corrupt
                }
            ));
            assert!(view(&s, &input).acknowledgment.manual_at.is_none());
        }
    }

    #[test]
    fn managed_initial_refusals_preserve_all_counters_and_slots() {
        let cases = [
            HoldReason::Disabled,
            HoldReason::ContinuityLost,
            HoldReason::UnsupportedVersion,
            HoldReason::WrongHome,
            HoldReason::UnknownIdentity,
            HoldReason::AliasConflict,
            HoldReason::GenerationConflict,
            HoldReason::UnsupportedBackend,
            HoldReason::UnsupportedBackend,
            HoldReason::Unconfirmed,
            HoldReason::LegacyUncertain,
            HoldReason::Expired,
        ];
        for (case, expected) in cases.into_iter().enumerate() {
            let s = MemStore::default();
            install(&s);
            let mut input = base_input();
            {
                let mut inner = s.inner.borrow_mut();
                match case {
                    0 => inner.delivery.continuity.as_mut().unwrap().enabled = false,
                    1 => inner.delivery.continuity.as_mut().unwrap().intact = false,
                    2 => inner.delivery.continuity.as_mut().unwrap().schema = 2,
                    3 => inner.delivery.continuity.as_mut().unwrap().local_epoch = 2,
                    4 => {
                        inner
                            .agents
                            .get_mut(&input.key.recipient_id)
                            .unwrap()
                            .status = AgentStatus::Retired
                    }
                    5 => {
                        inner
                            .delivery
                            .identities
                            .insert("name:bob".into(), IdentityResolution::Conflict);
                    }
                    6 => input.binding.binding_revision = Revision(2),
                    7 => inner.delivery.admissions.clear(),
                    8 => input.attempt.lifetime.revision = Revision(2),
                    9 => {
                        inner
                            .delivery
                            .recipients
                            .get_mut(&input.key.recipient_id)
                            .unwrap()
                            .barrier = Some(input.key.clone())
                    }
                    10 => {
                        inner.delivery.continuity.as_mut().unwrap().cutover =
                            At::from_unix_millis(1000)
                    }
                    11 => {
                        inner
                            .wire_messages
                            .get_mut(&input.key.original_id)
                            .unwrap()
                            .deadline_at = Some(input.now.clone())
                    }
                    _ => unreachable!(),
                }
            }
            // The expiry fixture mutates immutable fields, so bind its new digest.
            if case == 11 {
                input.generation = canonical_generation(
                    s.inner
                        .borrow()
                        .wire_messages
                        .get(&input.key.original_id)
                        .unwrap(),
                )
                .unwrap();
            }
            s.inner.borrow_mut().delivery.access = DeliveryAccess::default();
            assert_eq!(
                s.apply_delivery(DeliveryCommand::Prepare(input.clone()))
                    .unwrap(),
                DeliveryResult::Refused { reason: expected }
            );
            let inner = s.inner.borrow();
            assert!(inner.delivery.records.is_empty());
            assert!(inner.delivery.reservations.is_empty());
            assert_eq!(inner.delivery.global.as_ref().unwrap().active_attempts, 0);
            assert!(
                inner
                    .delivery
                    .recipients
                    .get(&input.key.recipient_id)
                    .unwrap()
                    .active
                    .is_none()
            );
            assert!(inner.delivery.access.writes.is_empty());
        }
    }

    #[test]
    fn managed_immutable_identity_field_bounds() {
        for case in 0..7 {
            let s = MemStore::default();
            let mut input = base_input();
            let mut message = fixture_message();
            let large = "x".repeat(4097);
            let large_id = Iri::parse(&format!("urn:fixture:{large}")).unwrap();
            match case {
                0 => message.sender_host = Some(large),
                1 => {
                    message.id = MessageId(large_id);
                    input.key.original_id = message.id.clone();
                }
                2 => message.thread_id = MessageId(large_id),
                3 => message.reply_to = Some(MessageId(large_id)),
                4 => message.correlation_id = Some(MessageId(large_id)),
                5 => message.sender_id = Some(AgentId(large_id)),
                6 => message.to = crate::ferrowire::WireAddress::AgentName(large),
                _ => unreachable!(),
            }
            input.generation = canonical_generation(&message).unwrap();
            install_input(&s, &input, message);
            assert!(
                matches!(
                    s.apply_delivery(DeliveryCommand::Prepare(input)).unwrap(),
                    DeliveryResult::Refused {
                        reason: HoldReason::EvidenceOverflow
                    }
                ),
                "field {case}"
            );
        }
    }
}
