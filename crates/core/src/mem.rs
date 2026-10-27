use crate::agent::{
    AgentRecord, AgentRole, AgentStatus, AreaRecord, RegisterAgentArgs, RegistryEvent,
    RegistryEventKind, validate_agent_identity,
};
use crate::at::At;
use crate::escalation::{EscalationFault, Escalations, Mark, Tombstone, escalate_command};
use crate::ferrowire::{
    DeliveryReceipt, DeliveryStatus, MessageKind, SendWireArgs, ThreadId, WireFilter,
    WireInboxItem, WireMessage, resolve_recipients, validate_sender_name, validate_subject,
    validate_wire_body,
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
            .range((seq + 1)..)
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
            .range((seq + 1)..)
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

        let recipients = resolve_recipients(
            &msg.to,
            msg.sender_id.as_ref(),
            &msg.sender_name,
            &all_agents,
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
        for (&seq, mid) in s.wire_msg_log.range((after_seq + 1)..) {
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
            .range((after_seq + 1)..)
            .filter(|(_, rcpt)| s.wire_messages.contains_key(&rcpt.message_id))
            .map(|(&seq, rcpt)| (seq, rcpt.clone()))
            .collect())
    }

    fn import_wire_batch(
        &self,
        messages: &[WireMessage],
        receipts: &[DeliveryReceipt],
    ) -> Result<(), StoreError> {
        let mut s = self.inner.borrow_mut();
        let all_agents: Vec<AgentRecord> = s.agents.values().cloned().collect();

        for msg in messages {
            msg.validate()
                .map_err(|e| StoreError::InvalidState(e.to_string()))?;
            if let Some(existing) = s.wire_messages.get(&msg.id) {
                if existing != msg {
                    return Err(StoreError::WireMessageTampered {
                        id: msg.id.clone(),
                        detail: "imported wire message differs from stored copy".into(),
                    });
                }
                continue;
            }
            s.index_imported_unhanded(Kind::Message, msg.id.iri())?;
            s.append_wire_message(msg.clone());

            let resolved = resolve_recipients(
                &msg.to,
                msg.sender_id.as_ref(),
                &msg.sender_name,
                &all_agents,
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
                return Err(StoreError::MessageNotFound(rcpt.message_id.clone()));
            }
            let key = (rcpt.recipient_id.clone(), rcpt.message_id.clone());
            match s.wire_inbox.get(&key).cloned() {
                Some(existing) => {
                    let merged = existing.merge(rcpt);
                    if merged != existing {
                        s.wire_inbox.insert(key, merged.clone());
                        s.append_wire_receipt(merged);
                    }
                }
                None => {
                    s.wire_inbox.insert(key, rcpt.clone());
                    s.append_wire_receipt(rcpt.clone());
                }
            }
        }
        Ok(())
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
