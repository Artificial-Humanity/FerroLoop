//! Recovery-only domain and query validation over freshly decoded immutable state.
use super::*;
use crate::ferrostore::{columnar, doc, vector};
use crate::recovery::profiles::{PROFILE_VERSION, QueryProbe};
use crate::recovery::verify::{Canonical, reference_digest, value_digest};
use crate::recovery::{ValidationExpectations, ValidationProfile};
use fl_core::state::{StateEntry, StatePayload, StateRevision};
use fl_core::store::{MessageStore, StateStore};
use serde::de::DeserializeOwned;
use serde_json::json;

pub(super) fn descriptor(
    state: &DocState,
    published: Publication,
    bytes: u64,
    c: &Check<'_>,
) -> Result<ImageDescriptor, RecoveryError> {
    let schema = doc::schema_version_in(|collection, id| state.docs.get(collection, id).cloned())
        .map_err(|_| c.verify())?;
    // Absence is supported for old generic images, explicitly preserved. A
    // present application version must be on the compatible reader's ladder.
    if schema.is_some() {
        crate::check_format(schema).map_err(|_| RecoveryError::Unsupported {
            operation_id: c.id,
            reason: "application schema unsupported",
        })?;
    }
    Ok(ImageDescriptor {
        bytes,
        sequence: state.last_seq,
        container_format: published.format,
        snapshot_format: if !state.vectors.is_empty() {
            layout::SNAPSHOT_VERSION_VECTOR
        } else if !state.tables.is_empty() {
            layout::SNAPSHOT_VERSION_COLUMNAR
        } else {
            layout::SNAPSHOT_VERSION_DOCS
        },
        engine: published.engine,
        incompatible_features: published.features,
        schema,
        lineage: None,
        external_publish_status: ExternalPublishStatus::ExclusionUnprovenBeforeSingleOwnership,
        exclusions: vec![
            CaptureExclusion::EphemeralMq,
            CaptureExclusion::PendingAdapterDeliveries,
        ],
    })
}
fn typed<T: DeserializeOwned>(value: &Value, l: &OperationLimits) -> Result<T, RecoveryError> {
    serde_json::from_value(value.clone()).map_err(|_| l.verification())
}
fn serialized<T: serde::Serialize>(v: &T, l: &OperationLimits) -> Result<Value, RecoveryError> {
    l.check()?;
    serde_json::to_value(v).map_err(|_| l.verification())
}

pub(crate) fn expectations_from_reader(
    store: &FerroStore,
    descriptor: ImageDescriptor,
    profile: &ValidationProfile,
    l: &OperationLimits,
) -> Result<ValidationExpectations, RecoveryError> {
    l.check()?;
    if profile.version != PROFILE_VERSION || profile.catalog_identity.trim().is_empty() {
        return Err(RecoveryError::RefusedInput {
            operation_id: l.operation_id,
            reason: "compatible versioned profile and catalog identity required",
        });
    }
    // Charge profile serialization before allocating its JSON representation.
    let (profile_bytes, _) = crate::recovery::verify::serialized_checksum(profile, l)?;
    l.quota(
        profile_bytes.saturating_mul(32),
        l.memory_byte_limit / 4,
        "profile memory",
    )?;
    let profile_sha256 = value_digest(&serialized(profile, l)?, l)?;
    let state = store.read_state().map_err(|_| l.verification())?;
    let check = Check {
        id: l.operation_id,
        deadline: l.deadline,
        cancel: &l.cancel,
        clock: &RealClock,
    };
    // Covers clones made by typed domain reads, query result sets, JSON
    // canonicalization and simultaneous expectation/report arrays. Compressed
    // column expansion has a separate bounded per-group reservation below.
    let retained = retained_bytes(&state, l.memory_byte_limit / 64, &check)?;
    l.quota(
        retained
            .saturating_mul(32)
            .saturating_add(profile_bytes.saturating_mul(32)),
        l.memory_byte_limit / 2,
        "verification memory",
    )?;
    let mut out = ValidationExpectations {
        version: 1,
        source_file_identity: None,
        captured_at_unix_ms: None,
        descriptor,
        catalog_identity: profile.catalog_identity.clone(),
        profile_sha256,
        logical_sha256: String::new(),
        collection_digests: vec![],
        query_digests: vec![],
        vector_results: vec![],
        known: 0,
        unknown: 0,
        opaque_runtime_references: false,
        exceptions: 0,
        dependencies: profile.dependencies.len() as u64,
    };
    let mut logical = Canonical::new(l);
    logical.count(30, state.docs.collections().count() as u64)?;
    let mut used_exceptions = std::collections::BTreeSet::new();
    for collection in state.docs.collections() {
        l.check()?;
        let mut content = Canonical::new(l);
        content.bytes(6, collection.as_bytes())?;
        content.count(31, state.docs.in_collection(collection).count() as u64)?;
        let known = known_collection(collection);
        if known {
            out.known += 1;
        } else {
            out.unknown += 1;
        }
        for (id, value) in state.docs.in_collection(collection) {
            content.bytes(6, id.as_bytes())?;
            content.value(Some(value))?;
            validate_domain(
                &state,
                collection,
                id,
                value,
                profile,
                &mut used_exceptions,
                l,
            )?;
            for rule in &profile.reference_rules {
                if rule.source_collection == *collection
                    && let Some(value) = value.pointer(&rule.pointer)
                {
                    references(
                        &state,
                        collection,
                        id,
                        &rule.pointer,
                        &rule.target_collection,
                        value,
                        profile,
                        &mut used_exceptions,
                        l,
                    )?;
                }
            }
        }
        let digest = content.finish();
        logical.bytes(32, digest.as_bytes())?;
        out.collection_digests.push(digest);
        automatic_doc_probes(store, &state, collection, &mut out, l)?;
    }
    out.exceptions = used_exceptions.len() as u64;
    // An unused exception can conceal a typo or a later repair: require exact
    // coverage of existing missing references, not a blanket wildcard waiver.
    if out.exceptions != profile.preexisting_exceptions.len() as u64 {
        return Err(l.verification());
    }
    logical.count(40, state.tables.len() as u64)?;
    for (name, table) in &state.tables {
        logical.bytes(6, name.as_bytes())?;
        logical.count(41, table.columns.len() as u64)?;
        for column in &table.columns {
            logical.bytes(6, column.name.as_bytes())?;
            logical.bytes(42, &[columnar::type_tag(column.ty)])?;
        }
        let rows = table.groups.iter().try_fold(0u64, |n, g| {
            columnar::group_row_count(g)
                .ok()
                .and_then(|m| n.checked_add(m))
                .ok_or_else(|| l.verification())
        })?;
        logical.count(43, rows)?;
        // Includes persisted zone maps and codec metadata as well as typed rows.
        logical.count(44, table.groups.len() as u64)?;
        for group in &table.groups {
            logical.bytes(45, group)?;
        }
        let row_limits = columnar::Limits {
            max_rows: columnar::MAX_ROWS_PER_GROUP,
            max_bytes: usize::try_from(l.memory_byte_limit / 8).map_err(|_| l.verification())?,
        };
        let mut failure = None;
        columnar::visit_rows_for_recovery(&table.columns, &table.groups, &row_limits, &mut |row| {
            let result = (|| {
                l.check()?;
                logical.count(46, row.len() as u64)?;
                for cell in row {
                    hash_cell(&mut logical, cell)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                failure = Some(error);
                return Err(crate::backend("recovery row traversal stopped"));
            }
            Ok(())
        })
        .map_err(|error| failure.unwrap_or_else(|| decode_error(error, &check)))?;
    }
    logical.count(50, state.vectors.len() as u64)?;
    for (name, col) in &state.vectors {
        logical.bytes(6, name.as_bytes())?;
        logical.value(Some(&vector::VectorParams::of(col).to_json()))?;
        match &col.entry {
            Some(id) => logical.bytes(6, id.as_bytes())?,
            None => logical.bytes(0, &[])?,
        };
        logical.count(51, col.vectors.len() as u64)?;
        for (id, record) in &col.vectors {
            logical.bytes(6, id.as_bytes())?;
            logical.bytes(52, &record.bytes)?;
            logical.value(record.metadata.as_ref())?;
            logical.bytes(53, &[record.level])?;
            logical.count(54, record.neighbors.len() as u64)?;
            for layer in &record.neighbors {
                logical.count(55, layer.len() as u64)?;
                for neighbor in layer {
                    logical.bytes(6, neighbor.as_bytes())?;
                }
            }
            // Sensitive and ever-sensitive state is excluded from state indexes,
            // not from authoritative recovery. Never drop the source documents.
            if name.starts_with("state_vec_")
                && let Some(entry) = state.docs.get(doc::STATE_ENTRIES, id)
            {
                let entry: StateEntry = typed(entry, l)?;
                if entry.sensitive || entry.ever_sensitive {
                    return Err(l.verification());
                }
            }
        }
        if let Some((_, record)) = col.vectors.first_key_value() {
            let query =
                vector::dequantize(col.storage, &record.bytes).map_err(|_| l.verification())?;
            vector_probe(store, name, &query, col.vectors.len(), &mut out, l)?;
        }
    }
    for probe in &profile.probes {
        run_probe(store, probe, &mut out, l)?;
    }
    state_domain_probes(store, &mut out, l)?;
    // The first key is deterministic in the immutable document BTreeMap.
    // All rows were already hashed and domain-validated above.
    if let Some((_, value)) = state.docs.in_collection(doc::WIRE_MESSAGES).next() {
        l.check()?;
        let msg: fl_core::ferrowire::WireMessage = typed(value, l)?;
        admit_doc_probe(store, &[doc::WIRE_MESSAGES], 0, l)?;
        add_result(
            &mut out,
            &serialized(
                &store
                    .get_wire_message(&msg.id)
                    .map_err(|_| l.verification())?,
                l,
            )?,
            l,
        )?;
        admit_doc_probe(store, &[doc::WIRE_INBOX, doc::WIRE_MESSAGES], 0, l)?;
        add_result(
            &mut out,
            &serialized(
                &store
                    .list_wire_receipts(&msg.id)
                    .map_err(|_| l.verification())?,
                l,
            )?,
            l,
        )?;
        admit_doc_probe(store, &[doc::WIRE_THREADS, doc::WIRE_MESSAGES], 0, l)?;
        add_result(
            &mut out,
            &serialized(
                &store
                    .list_wire_thread(&msg.thread_id)
                    .map_err(|_| l.verification())?,
                l,
            )?,
            l,
        )?;
    }
    out.logical_sha256 = logical.finish();
    l.check()?;
    Ok(out)
}
fn hash_cell(h: &mut Canonical<'_>, v: &columnar::ColumnValue) -> Result<(), RecoveryError> {
    use columnar::ColumnValue::*;
    match v {
        Null => h.bytes(60, &[]),
        Int64(n) => h.bytes(61, &n.to_be_bytes()),
        Float64(n) => h.bytes(62, &n.to_bits().to_be_bytes()),
        Bool(b) => h.bytes(63, &[u8::from(*b)]),
        Utf8(s) => h.bytes(64, s.as_bytes()),
        Bytes(b) => h.bytes(65, b),
        TimestampMs(n) => h.bytes(66, &n.to_be_bytes()),
    }
}
fn add_result(
    out: &mut ValidationExpectations,
    value: &Value,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    out.query_digests.push(value_digest(value, l)?);
    Ok(())
}
// Existing query APIs are synchronous. Admit their complete possible input
// before entering them, with cancellation throughout admission and again after
// the call. An admitted call cannot be hard-preempted. Fixed representative
// probes keep these scans linear in stored rows; the operation-wide work quota
// also bounds caller-supplied probes across both verification opens.
fn admit_doc_probe(
    store: &FerroStore,
    collections: &[&str],
    query_bytes_per_row: u64,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    l.check()?;
    let state = store.read_state().map_err(|_| l.verification())?;
    l.charge_query_work(1)?;
    for collection in collections {
        for (id, value) in state.docs.in_collection(collection) {
            #[cfg(test)]
            tests::admission_row(l);
            l.check()?;
            let (bytes, _) = crate::recovery::verify::serialized_checksum(value, l)?;
            l.charge_query_work(
                bytes
                    .saturating_add(id.len() as u64)
                    .saturating_add(query_bytes_per_row)
                    .saturating_add(1),
            )?;
        }
    }
    l.check()
}

fn automatic_doc_probes(
    store: &FerroStore,
    state: &DocState,
    c: &str,
    out: &mut ValidationExpectations,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    // Every collection: representative point, provably absent point, full scan,
    // one exact top-level field query, and exact lexical inclusion/ranking.
    if let Some((id, value)) = state.docs.in_collection(c).next() {
        run_probe(
            store,
            &QueryProbe::Point {
                collection: c.into(),
                id: id.clone(),
            },
            out,
            l,
        )?;
        let mut absent = id.clone();
        absent.push('\0');
        while state.docs.get(c, &absent).is_some() {
            l.check()?;
            absent.push('\0');
        }
        run_probe(
            store,
            &QueryProbe::Absent {
                collection: c.into(),
                id: absent,
            },
            out,
            l,
        )?;
        run_probe(
            store,
            &QueryProbe::Scan {
                collection: c.into(),
            },
            out,
            l,
        )?;
        let field = value.as_object().and_then(|o| {
            o.iter()
                .find_map(|(k, v)| v.as_str().map(|s| (k.as_str(), s)))
        });
        let (key, text) = field.unwrap_or(("__recovery_absent_field__", "recovery"));
        run_probe(
            store,
            &QueryProbe::Query {
                collection: c.into(),
                field: key.into(),
                value: text.into(),
            },
            out,
            l,
        )?;
        run_probe(
            store,
            &QueryProbe::Lexical {
                collection: c.into(),
                query: text.into(),
                limit: state.docs.in_collection(c).count(),
            },
            out,
            l,
        )?;
    }
    Ok(())
}
fn run_probe(
    store: &FerroStore,
    probe: &QueryProbe,
    out: &mut ValidationExpectations,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    l.check()?;
    match probe {
        QueryProbe::Point { collection, .. }
        | QueryProbe::Absent { collection, .. }
        | QueryProbe::Scan { collection } => admit_doc_probe(store, &[collection], 0, l)?,
        QueryProbe::Query {
            collection,
            field,
            value,
        } => {
            l.charge_query_work(field.len() as u64 + value.len() as u64)?;
            admit_doc_probe(store, &[collection], 0, l)?;
        }
        QueryProbe::Lexical {
            collection, query, ..
        } => {
            l.charge_query_work(query.len() as u64)?;
            admit_doc_probe(store, &[collection], query.len() as u64, l)?;
        }
        QueryProbe::Vector { .. } => {}
    }
    match probe {
        QueryProbe::Point { collection, id } | QueryProbe::Absent { collection, id } => {
            let value = store
                .get_doc(collection, id)
                .map_err(|_| l.verification())?;
            if matches!(probe, QueryProbe::Absent { .. }) && value.is_some() {
                return Err(l.verification());
            }
            let mut h = Canonical::new(l);
            h.value(value.as_ref())?;
            out.query_digests.push(h.finish());
        }
        QueryProbe::Scan { collection } => {
            // Transaction Scan uses the reader's actual snapshot and read set.
            let rows = store
                .with_read(|txn| Ok(txn.scan(collection)))
                .map_err(|_| l.verification())?;
            add_result(out, &serialized(&rows, l)?, l)?;
        }
        QueryProbe::Query {
            collection,
            field,
            value,
        } => {
            let rows = store
                .query_by_field(collection, field, value)
                .map_err(|_| l.verification())?;
            add_result(out, &serialized(&rows, l)?, l)?;
        }
        QueryProbe::Lexical {
            collection,
            query,
            limit,
        } => {
            let n = store
                .read_state()
                .map_err(|_| l.verification())?
                .docs
                .in_collection(collection)
                .count();
            let rows = store
                .search_bm25(collection, query, (*limit).min(n))
                .map_err(|_| l.verification())?;
            add_result(out, &serialized(&rows, l)?, l)?;
        }
        QueryProbe::Vector {
            collection,
            vector,
            limit,
        } => vector_probe(store, collection, vector, *limit, out, l)?,
    }
    l.check()
}
fn vector_probe(
    store: &FerroStore,
    c: &str,
    query: &[f32],
    limit: usize,
    out: &mut ValidationExpectations,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    l.check()?;
    let state = store.read_state().map_err(|_| l.verification())?;
    let col = state.vectors.get(c).ok_or_else(|| l.verification())?;
    let mut layers = 1;
    for record in col.vectors.values() {
        l.check()?;
        layers = layers.max(u64::from(record.level) + 1);
    }
    l.charge_query_work((query.len() as u64).saturating_mul(4))?;
    for (id, record) in &col.vectors {
        l.check()?;
        l.charge_query_work(
            (record.bytes.len() as u64)
                .saturating_mul(layers)
                .saturating_add(id.len() as u64)
                .saturating_add(1),
        )?;
        for neighbors in &record.neighbors {
            for neighbor in neighbors {
                l.charge_query_work(neighbor.len() as u64 + 1)?;
            }
        }
    }
    let retained_results = out
        .vector_results
        .iter()
        .map(|rows| rows.len() as u64)
        .sum::<u64>();
    l.quota(
        retained_results
            .saturating_add(limit.min(col.vectors.len()) as u64)
            .saturating_mul(256),
        l.memory_byte_limit / 8,
        "vector result memory",
    )?;
    let hits = store
        .search_vectors(c, query, limit.min(col.vectors.len()), None)
        .map_err(|_| l.verification())?;
    let mut private_hits = Vec::new();
    for (id, score) in hits {
        l.check()?;
        let mut h = Canonical::new(l);
        h.bytes(6, id.as_bytes())?;
        private_hits.push((h.finish(), score));
    }
    out.vector_results.push(private_hits);
    Ok(())
}
// StateStore's point reads inspect at most one alias (doc::locate), plus the
// key/ID indexes and resolved entry. Admit those exact documents, without a scan.
fn admit_state_point(
    state: &DocState,
    id: &str,
    key: Option<&str>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    let point = |collection: &str, id: &str| -> Result<(), RecoveryError> {
        l.charge_query_work(id.len() as u64 + 1)?;
        if let Some(value) = state.docs.get(collection, id) {
            let (bytes, _) = crate::recovery::verify::serialized_checksum(value, l)?;
            l.charge_query_work(bytes)?;
        }
        Ok(())
    };
    let id = if let Some(key) = key {
        point(doc::STATE_KEYS, key)?;
        state
            .docs
            .get(doc::STATE_KEYS, key)
            .and_then(Value::as_str)
            .unwrap_or(id)
    } else {
        id
    };
    point(doc::IDS, id)?;
    let resolved = if state.docs.get(doc::IDS, id).and_then(Value::as_str) == Some("alias") {
        point(doc::ALIASES, id)?;
        let primary = state
            .docs
            .get(doc::ALIASES, id)
            .and_then(Value::as_str)
            .unwrap_or(id);
        point(doc::IDS, primary)?;
        primary
    } else {
        id
    };
    point(doc::STATE_ENTRIES, resolved)
}

fn state_domain_probes(
    store: &FerroStore,
    out: &mut ValidationExpectations,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    admit_doc_probe(store, &[doc::STATE_ENTRIES], 0, l)?;
    let entries = store.list_state(None, None).map_err(|_| l.verification())?;
    add_result(out, &serialized(&entries, l)?, l)?;
    let mut exportable = Vec::new();
    let state = store.read_state().map_err(|_| l.verification())?;
    for entry in &entries {
        l.check()?;
        // Keep point/index consistency checks on every row; no full scan here.
        admit_state_point(&state, entry.id.as_str(), None, l)?;
        let point = store.get_state(&entry.id).map_err(|_| l.verification())?;
        let key = doc::triple(&entry.scope.as_wire(), entry.concern.as_wire(), &entry.slug);
        admit_state_point(&state, entry.id.as_str(), Some(&key), l)?;
        let keyed = store
            .get_state_by_key(&entry.scope, entry.concern, &entry.slug)
            .map_err(|_| l.verification())?;
        if point.as_ref() != Some(entry) || keyed.as_ref() != Some(entry) {
            return Err(l.verification());
        }
        if matches!(entry.payload, StatePayload::Runtime(_)) {
            out.opaque_runtime_references = true;
        }
        if !entry.sensitive && !entry.ever_sensitive {
            exportable.push(entry.clone());
        }
    }
    if let Some(entry) = entries.first() {
        admit_doc_probe(store, &[doc::STATE_REVISIONS], 0, l)?;
        admit_state_point(&state, entry.id.as_str(), None, l)?;
        add_result(
            out,
            &serialized(
                &store
                    .state_revisions(&entry.id)
                    .map_err(|_| l.verification())?,
                l,
            )?,
            l,
        )?;
    }
    // Handoff, runtime and privacy-filtered values, alongside exact lexical
    // state ranking. All operate on this reader; no sidecar or embedder writes.
    add_result(out, &serialized(&exportable, l)?, l)?;
    for concern in [
        fl_core::state::StateConcern::Handoff,
        fl_core::state::StateConcern::Runtime,
    ] {
        admit_doc_probe(store, &[doc::STATE_ENTRIES], 0, l)?;
        add_result(
            out,
            &serialized(
                &store
                    .list_state(None, Some(concern))
                    .map_err(|_| l.verification())?,
                l,
            )?,
            l,
        )?;
    }
    if let Some(entry) = entries.first() {
        admit_doc_probe(store, &[doc::STATE_ENTRIES], entry.slug.len() as u64, l)?;
        let hits =
            fl_core::state::search_state_entries(&entries, &entry.slug, None, None, entries.len());
        add_result(out, &serialized(&hits, l)?, l)?;
    }
    Ok(())
}

fn known_collection(c: &str) -> bool {
    matches!(
        c,
        doc::SCHEMA_COLLECTION
            | doc::META
            | doc::PROJECTS
            | doc::GATES
            | doc::TRANSITIONS
            | doc::RECORDS
            | doc::FINDINGS
            | doc::ATTEMPTS
            | doc::GATE_RUNS
            | doc::CANDIDATE_RUNS
            | doc::CANDIDATE_ATTEMPTS
            | doc::LEDGER_ROOTS
            | doc::LEDGER_HEADS
            | doc::LEDGER_SEGMENTS
            | doc::LEDGER_CUTOVERS
            | doc::LEDGER_PUBLISHED
            | doc::STATE_ENTRIES
            | doc::STATE_REVISIONS
            | doc::STATE_KEYS
            | doc::STATE_REVISION_IDS
            | doc::STATE_CONFLICTS
            | doc::STATE_SYNC_CURSORS
            | doc::AGENTS
            | doc::AGENT_BY_NAME
            | doc::AGENT_EVENTS
            | doc::AGENT_EVENT_IDS
            | doc::AREAS
            | doc::WIRE_MESSAGES
            | doc::WIRE_INBOX
            | doc::WIRE_RCPT_LOG
            | doc::WIRE_MSG_LOG
            | doc::WIRE_MSG_SEQ
            | doc::WIRE_THREADS
            | doc::WIRE_REPLIES
            | doc::IDS
            | doc::ALIASES
            | doc::HANDLES
            | doc::HANDLE_OF
    )
}
#[allow(clippy::too_many_arguments)]
fn references(
    state: &DocState,
    c: &str,
    id: &str,
    pointer: &str,
    target_c: &str,
    value: &Value,
    p: &ValidationProfile,
    used: &mut std::collections::BTreeSet<String>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    l.check()?;
    if value.is_null() {
        return Ok(());
    }
    if let Some(values) = value.as_array() {
        for v in values {
            references(state, c, id, pointer, target_c, v, p, used, l)?;
        }
        return Ok(());
    }
    let target = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_u64() => n.to_string(),
        _ => return Err(l.verification()),
    };
    if state.docs.get(target_c, &target).is_some() {
        return Ok(());
    }
    // Resolve catalog aliases explicitly, with a traversal bound and no cycles.
    let mut alias = target.as_str();
    for _ in 0..state.docs.in_collection(doc::ALIASES).count() {
        l.check()?;
        let Some(next) = state.docs.get(doc::ALIASES, alias).and_then(Value::as_str) else {
            break;
        };
        if state.docs.get(target_c, next).is_some() {
            return Ok(());
        }
        alias = next;
    }
    let digest = reference_digest(c, id, pointer, target_c, &target, l)?;
    if p.preexisting_exceptions
        .iter()
        .any(|e| e.reference_sha256 == digest && !e.reason.trim().is_empty())
    {
        used.insert(digest);
        return Ok(());
    }
    if p.dependencies
        .iter()
        .any(|d| d.reference_sha256 == digest && !d.catalog_identity.trim().is_empty())
    {
        return Ok(());
    }
    Err(l.verification())
}
fn validate_domain(
    state: &DocState,
    c: &str,
    id: &str,
    value: &Value,
    p: &ValidationProfile,
    used: &mut std::collections::BTreeSet<String>,
    l: &OperationLimits,
) -> Result<(), RecoveryError> {
    use fl_core::{
        agent::{AgentRecord, AreaRecord, RegistryEvent},
        ferrowire::{DeliveryReceipt, WireMessage},
        finding::Finding,
        log::{Attempt, GateRun},
        model::{GateDef, Project, Record, Transition},
    };
    let mut reference = |pointer: &str, target: &str, v: &Value| {
        references(state, c, id, pointer, target, v, p, used, l)
    };
    macro_rules! parse {
        ($t:ty) => {
            typed::<$t>(value, l)?
        };
    }
    macro_rules! field {
        ($pointer:expr,$target:expr) => {
            if let Some(v) = value.pointer($pointer) {
                reference($pointer, $target, v)?;
            }
        };
    }
    match c {
        doc::PROJECTS => {
            let row = parse!(Project);
            if row.id.as_str() != id {
                return Err(l.verification());
            }
        }
        doc::GATES => {
            let row = parse!(GateDef);
            if row.id.as_str() != id {
                return Err(l.verification());
            }
            field!("/project", doc::PROJECTS);
        }
        doc::TRANSITIONS => {
            let _ = parse!(Transition);
            field!("/project", doc::PROJECTS);
            field!("/gates", doc::GATES);
        }
        doc::RECORDS => {
            let row = parse!(Record);
            if row.id.as_str() != id {
                return Err(l.verification());
            }
            field!("/project", doc::PROJECTS);
        }
        doc::FINDINGS => {
            let row = parse!(Finding);
            if row.id.as_str() != id {
                return Err(l.verification());
            }
            field!("/project", doc::PROJECTS);
            field!("/record", doc::RECORDS);
            field!("/reproduction", doc::GATES);
        }
        doc::ATTEMPTS => {
            let _ = parse!(Attempt);
            field!("/project", doc::PROJECTS);
            field!("/record", doc::RECORDS);
        }
        doc::GATE_RUNS => {
            let _ = parse!(GateRun);
            field!("/gate", doc::GATES);
            field!("/record", doc::RECORDS);
        }
        doc::LEDGER_ROOTS | doc::LEDGER_HEADS => {
            let target = value.as_str().ok_or_else(|| l.verification())?;
            let digest = reference_digest(c, id, "", "external_repository_revision", target, l)?;
            if !p
                .dependencies
                .iter()
                .any(|d| d.reference_sha256 == digest && !d.catalog_identity.trim().is_empty())
            {
                return Err(l.verification());
            }
        }
        doc::LEDGER_SEGMENTS => {
            let _ = parse!(fl_core::split::CachedSegment);
        }
        doc::LEDGER_CUTOVERS => {
            fl_core::Iri::parse(value.as_str().ok_or_else(|| l.verification())?)
                .map_err(|_| l.verification())?;
        }
        doc::LEDGER_PUBLISHED => {
            if value.as_bool() != Some(true) || doc::split_pair(id).is_none() {
                return Err(l.verification());
            }
        }
        doc::CANDIDATE_RUNS => reference("", doc::GATE_RUNS, value)?,
        doc::CANDIDATE_ATTEMPTS => reference("", doc::ATTEMPTS, value)?,
        doc::AGENTS => {
            let row = parse!(AgentRecord);
            if row.id.as_str() != id {
                return Err(l.verification());
            }
            reference("/name_key", doc::AGENT_BY_NAME, &json!(row.name_key))?;
            if state
                .docs
                .get(doc::AGENT_BY_NAME, &row.name_key)
                .and_then(Value::as_str)
                != Some(id)
            {
                return Err(l.verification());
            }
        }
        doc::AREAS => {
            let row = parse!(AreaRecord);
            if row.area_key != id {
                return Err(l.verification());
            }
        }
        doc::AGENT_BY_NAME => reference("", doc::AGENTS, value)?,
        doc::AGENT_EVENTS => {
            let _ = parse!(RegistryEvent);
        }
        doc::AGENT_EVENT_IDS => reference("", doc::AGENT_EVENTS, value)?,
        doc::STATE_ENTRIES => {
            let row = parse!(StateEntry);
            if row.id.as_str() != id
                || row.payload.concern() != row.concern
                || (row.sensitive && !row.ever_sensitive)
            {
                return Err(l.verification());
            }
            row.payload
                .validate(&row.id)
                .map_err(|_| l.verification())?;
            reference(
                "/head_revision",
                doc::STATE_REVISION_IDS,
                &json!(row.head_revision.as_str()),
            )?;
            if let Some(seq) = state
                .docs
                .get(doc::STATE_REVISION_IDS, row.head_revision.as_str())
                .and_then(Value::as_u64)
            {
                let rev: StateRevision = typed(
                    state
                        .docs
                        .get(doc::STATE_REVISIONS, &seq.to_string())
                        .ok_or_else(|| l.verification())?,
                    l,
                )?;
                if rev.entry_id != row.id
                    || rev.revision_id != row.head_revision
                    || rev.scope != row.scope
                    || rev.concern != row.concern
                    || rev.slug != row.slug
                    || rev.payload != row.payload
                    || rev.sensitive != row.sensitive
                    || rev.ever_sensitive != row.ever_sensitive
                {
                    return Err(l.verification());
                }
            }
            if let StatePayload::Memory(memory) = &row.payload
                && let Some(target) = &memory.superseded_by
            {
                reference(
                    "/payload/memory/superseded_by",
                    doc::STATE_ENTRIES,
                    &json!(target.as_str()),
                )?;
            }
        }
        doc::STATE_REVISIONS => {
            let row = parse!(StateRevision);
            row.payload
                .validate(&row.entry_id)
                .map_err(|_| l.verification())?;
            if row.payload.concern() != row.concern || (row.sensitive && !row.ever_sensitive) {
                return Err(l.verification());
            }
            let seq = state
                .docs
                .get(doc::STATE_REVISION_IDS, row.revision_id.as_str())
                .and_then(Value::as_u64)
                .ok_or_else(|| l.verification())?;
            if seq.to_string() != id {
                return Err(l.verification());
            }
            field!("/parent_revisions", doc::STATE_REVISION_IDS);
            for parent in &row.parent_revisions {
                if let Some(seq) = state
                    .docs
                    .get(doc::STATE_REVISION_IDS, parent.as_str())
                    .and_then(Value::as_u64)
                {
                    if seq >= id.parse::<u64>().map_err(|_| l.verification())? {
                        return Err(l.verification());
                    }
                    let prior: StateRevision = typed(
                        state
                            .docs
                            .get(doc::STATE_REVISIONS, &seq.to_string())
                            .ok_or_else(|| l.verification())?,
                        l,
                    )?;
                    if prior.entry_id != row.entry_id
                        || prior.scope != row.scope
                        || prior.concern != row.concern
                        || prior.slug != row.slug
                        || ((prior.ever_sensitive || prior.sensitive) && !row.ever_sensitive)
                    {
                        return Err(l.verification());
                    }
                }
            }
        }
        doc::STATE_KEYS => reference("", doc::STATE_ENTRIES, value)?,
        doc::STATE_REVISION_IDS => reference("", doc::STATE_REVISIONS, value)?,
        doc::STATE_CONFLICTS => {
            let _ = parse!(fl_core::state::StateConflict);
            field!("/entry_id", doc::STATE_ENTRIES);
            field!("/local_head", doc::STATE_REVISION_IDS);
            field!("/remote_head", doc::STATE_REVISION_IDS);
        }
        doc::STATE_SYNC_CURSORS => {
            let row = parse!(fl_core::state::StateSyncCursor);
            if row.remote != id {
                return Err(l.verification());
            }
            // JSON tuple framing distinguishes remote/branch boundaries and
            // absent versus present commit, then the versioned reference digest
            // binds the collection, key and target kind. No horizon provenance
            // is inferred: even an unpushed cursor needs external reconciliation.
            let target =
                serde_json::to_string(&(&row.remote, &row.branch, &row.last_pushed_commit))
                    .map_err(|_| l.verification())?;
            let digest = reference_digest(c, id, "", "external_state_sync_cursor", &target, l)?;
            if !p
                .dependencies
                .iter()
                .any(|d| d.reference_sha256 == digest && !d.catalog_identity.trim().is_empty())
            {
                return Err(l.verification());
            }
        }
        doc::WIRE_MESSAGES => {
            let row = parse!(WireMessage);
            row.validate().map_err(|_| l.verification())?;
            if row.id.as_str() != id {
                return Err(l.verification());
            }
            field!("/reply_to", doc::WIRE_MESSAGES);
            field!("/correlation_id", doc::WIRE_MESSAGES);
            field!("/sender_id", doc::AGENTS);
        }
        doc::WIRE_RCPT_LOG | doc::WIRE_INBOX => {
            let _ = parse!(DeliveryReceipt);
            field!("/message_id", doc::WIRE_MESSAGES);
            field!("/recipient_id", doc::AGENTS);
        }
        doc::WIRE_MSG_LOG => reference("", doc::WIRE_MESSAGES, value)?,
        doc::WIRE_MSG_SEQ => reference("", doc::WIRE_MSG_LOG, value)?,
        doc::WIRE_THREADS | doc::WIRE_REPLIES => {
            let (_, message) = doc::split_pair(id).ok_or_else(|| l.verification())?;
            reference("/key", doc::WIRE_MESSAGES, &json!(message))?;
        }
        doc::ALIASES => reference("", doc::IDS, value)?,
        doc::HANDLES => reference("", doc::IDS, value)?,
        doc::HANDLE_OF => {
            reference("/key", doc::IDS, &json!(id))?;
            if !value.is_u64() {
                return Err(l.verification());
            }
        }
        doc::IDS
            if fl_core::ids::Kind::from_wire(value.as_str().ok_or_else(|| l.verification())?)
                .is_none()
                && value != &json!("alias") =>
        {
            return Err(l.verification());
        }

        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::{
        At,
        agent::{AgentRole, RegisterAgentArgs},
        ferrowire::{MessageKind, MessageUrgency, SendWireArgs, WireAddress},
        state::{
            HandoffEntry, MergeMode, RuntimeEntry, SensitivityPatch, StateScope, UpsertStateArgs,
        },
        store::{AgentRegistry, Catalog, Tracker},
    };
    fn limits() -> OperationLimits {
        OperationLimits::new(
            Instant::now() + Duration::from_secs(30),
            32 << 20,
            256 << 20,
            CancelToken::new(),
        )
    }
    fn capture(s: &FerroStore) -> CapturedStore {
        s.capture_recovery(
            &CaptureBudget::new(
                Instant::now() + Duration::from_secs(30),
                64 << 20,
                128 << 20,
                Duration::from_secs(1),
            ),
            &CancelToken::new(),
        )
        .unwrap()
    }
    fn profile() -> ValidationProfile {
        ValidationProfile::new(crate::recovery::ProfileKind::Workspace, "fixture")
    }
    fn runtime(s: &FerroStore, sensitive: SensitivityPatch) -> StateEntry {
        s.upsert_state(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: "private-fixture".into(),
            sensitivity: sensitive,
            payload: StatePayload::Runtime(RuntimeEntry {
                value: json!({"secret":"do-not-emit"}),
                merge_mode: MergeMode::LastWriter,
            }),
            at: At::from_unix_millis(1),
            by: "fixture".into(),
        })
        .unwrap()
    }
    thread_local! {
        static ADMISSION: std::cell::Cell<(u64, Option<u64>)> = const { std::cell::Cell::new((0, None)) };
    }
    pub(super) fn admission_row(l: &OperationLimits) {
        ADMISSION.with(|stats| {
            let (n, cancel_at) = stats.get();
            stats.set((n + 1, cancel_at));
            if cancel_at == Some(n + 1) {
                l.cancel.cancel();
            }
        });
    }
    #[test]
    fn r1_cancellation_during_query_admission_stops_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("admission.fls")).unwrap();
        for n in 0..64 {
            store
                .put_doc("fixture", &n.to_string(), &json!({"n":n}))
                .unwrap();
        }
        let captured = capture(&store);
        ADMISSION.with(|s| s.set((0, Some(10))));
        let result = captured.expectations(&profile(), &limits());
        let visits = ADMISSION.with(|s| {
            let n = s.get().0;
            s.set((0, None));
            n
        });
        assert!(matches!(result, Err(RecoveryError::Cancelled { .. })));
        assert_eq!(
            visits, 10,
            "admission must stop at the cancellation boundary"
        );
    }

    // Regression: adding rows must not add full-collection domain API probes.
    #[test]
    fn r1_domain_probe_count_is_independent_of_state_and_wire_row_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("scaling.fls")).unwrap();
        store
            .register_agent(RegisterAgentArgs {
                display_name: "Fixture".into(),
                email: None,
                project: "fixture".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: At::from_unix_millis(1),
                by: "test".into(),
            })
            .unwrap();
        let mut counts = Vec::new();
        let mut visits = Vec::new();
        for n in 0..16 {
            store
                .upsert_state(UpsertStateArgs {
                    scope: StateScope::Workspace,
                    slug: format!("entry-{n}"),
                    sensitivity: SensitivityPatch::Inherit,
                    payload: StatePayload::Runtime(RuntimeEntry {
                        value: json!({"n": n}),
                        merge_mode: MergeMode::LastWriter,
                    }),
                    at: At::from_unix_millis(n + 1),
                    by: "fixture".into(),
                })
                .unwrap();
            store
                .send_wire(SendWireArgs {
                    id: None,
                    sender_id: None,
                    sender_name: "fixture".into(),
                    sender_host: None,
                    to: WireAddress::AgentName("fixture".into()),
                    kind: MessageKind::Mail,
                    urgency: MessageUrgency::Normal,
                    durable: true,
                    subject: "fixture".into(),
                    body: "fixture".into(),
                    reply_to: None,
                    thread_id: None,
                    deadline_at: None,
                    at: At::from_unix_millis(n + 1),
                })
                .unwrap();
            if [0, 3, 15].contains(&n) {
                ADMISSION.with(|s| s.set((0, None)));
                counts.push(
                    capture(&store)
                        .expectations(&profile(), &limits())
                        .unwrap()
                        .query_digests
                        .len(),
                );
                visits.push(ADMISSION.with(|s| s.get().0));
            }
        }
        eprintln!("fixed full-scan probe digests {counts:?}; admitted scan rows {visits:?}");
        // Point/index consistency checks still run for every state entry. These
        // counts specifically measure the collection-scanning query admissions.
        assert!(visits[0] > 0);
        assert!(visits[1] <= visits[0] * 4);
        assert!(visits[2] <= visits[0] * 16);
        assert_eq!(counts[0], counts[1], "four rows must use fixed probe count");
        assert_eq!(
            counts[0], counts[2],
            "sixteen rows must use fixed probe count"
        );
    }

    #[test]
    fn r1_work_quota_is_shared_by_clones_and_refuses_before_decode() {
        let mut l = limits();
        l.query_work_limit = 10;
        let other = l.clone();
        l.charge_query_work(4).unwrap();
        assert!(matches!(
            other.charge_query_work(7),
            Err(RecoveryError::Quota {
                resource: "query work",
                ..
            })
        ));
        other.charge_query_work(6).unwrap();
        assert!(matches!(
            l.charge_query_work(1),
            Err(RecoveryError::Quota { .. })
        ));
        let extreme = OperationLimits::new(
            Instant::now() + Duration::from_secs(30),
            u64::MAX,
            256 << 20,
            CancelToken::new(),
        );
        extreme.charge_query_work(u64::MAX).unwrap();
        assert!(matches!(
            extreme.charge_query_work(1),
            Err(RecoveryError::Quota { .. })
        ));
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("quota.fls")).unwrap();
        let mut expected = capture(&store).expectations(&profile(), &limits()).unwrap();
        store
            .put_doc(doc::STATE_ENTRIES, "invalid", &json!({}))
            .unwrap();
        assert!(matches!(
            state_domain_probes(&store, &mut expected, &l),
            Err(RecoveryError::Quota {
                resource: "query work",
                ..
            })
        ));
    }

    #[test]
    fn r1_cancelled_domain_probe_does_not_enter_synchronous_decode() {
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("cancel.fls")).unwrap();
        let mut expected = capture(&store).expectations(&profile(), &limits()).unwrap();
        // A domain API entered before checking cancellation would decode this
        // row and return Verification instead of the requested Cancelled.
        store
            .put_doc(doc::STATE_ENTRIES, "invalid", &json!({}))
            .unwrap();
        let l = limits();
        l.cancel.cancel();
        assert!(matches!(
            state_domain_probes(&store, &mut expected, &l),
            Err(RecoveryError::Cancelled { .. })
        ));
    }

    #[test]
    fn state_heads_history_handoff_and_privacy_are_validated_from_pin() {
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("state.fls")).unwrap();
        runtime(&store, SensitivityPatch::SetSensitive);
        let entry = runtime(&store, SensitivityPatch::ClearSensitive);
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Project("council".into()),
                slug: "handoff".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "fixture handoff".into(),
                    shipped: vec!["one".into()],
                    in_flight: vec![],
                    blockers: vec![],
                    next_steps: vec!["two".into()],
                    notes: None,
                }),
                at: At::from_unix_millis(2),
                by: "fixture".into(),
            })
            .unwrap();
        let captured = capture(&store);
        let council = ValidationProfile::new(
            crate::recovery::ProfileKind::CouncilPrivacyRevisions,
            "fixture-council",
        );
        let expected = captured.expectations(&council, &limits()).unwrap();
        assert!(expected.known >= 5);
        assert_eq!(expected.unknown, 0);
        store
            .put_doc(
                doc::STATE_ENTRIES,
                entry.id.as_str(),
                &json!({"invalid":"later write"}),
            )
            .unwrap();
        assert_eq!(
            expected.logical_sha256,
            captured
                .expectations(&profile(), &limits())
                .unwrap()
                .logical_sha256
        );
        let mut invalid = Arc::unwrap_or_clone(captured.state);
        let mut value = invalid
            .docs
            .get(doc::STATE_ENTRIES, entry.id.as_str())
            .unwrap()
            .clone();
        value["ever_sensitive"] = json!(false);
        invalid.docs.insert(
            doc::STATE_ENTRIES.into(),
            entry.id.as_str().into(),
            Arc::new(value),
        );
        let read = FerroStore::assemble(
            Inner::Memory(Mutex::new(Arc::new(invalid))),
            "fixture".into(),
        );
        assert!(
            expectations_from_reader(&read, expected.descriptor, &profile(), &limits()).is_err()
        );
    }
    #[test]
    fn missing_or_changed_revision_head_and_cycle_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("state.fls")).unwrap();
        let entry = runtime(&store, SensitivityPatch::Inherit);
        let captured = capture(&store);
        let expected = captured.expectations(&profile(), &limits()).unwrap();
        for mode in 0..3 {
            let mut state = (*captured.state).clone();
            let mut row = state.docs.get(doc::STATE_REVISIONS, "1").unwrap().clone();
            match mode {
                0 => state.docs.remove(doc::STATE_REVISIONS, "1"),
                1 => {
                    row["payload"]["runtime"]["value"] = json!({"changed":"same count"});
                    state
                        .docs
                        .insert(doc::STATE_REVISIONS.into(), "1".into(), Arc::new(row));
                }
                _ => {
                    row["parent_revisions"] = json!([entry.head_revision.as_str()]);
                    state
                        .docs
                        .insert(doc::STATE_REVISIONS.into(), "1".into(), Arc::new(row));
                }
            }
            let read =
                FerroStore::assemble(Inner::Memory(Mutex::new(Arc::new(state))), "fixture".into());
            assert!(
                expectations_from_reader(&read, expected.descriptor.clone(), &profile(), &limits())
                    .is_err(),
                "mode {mode}"
            );
        }
    }
    #[test]
    fn registry_and_persisted_wire_receipts_have_typed_status_and_references() {
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("wire.fls")).unwrap();
        let agent = store
            .register_agent(RegisterAgentArgs {
                display_name: "Fixture".into(),
                email: None,
                project: "fixture".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: true,
                replace: false,
                at: At::from_unix_millis(1),
                by: "test".into(),
            })
            .unwrap();
        let (msg, _) = store
            .send_wire(SendWireArgs {
                id: None,
                sender_id: Some(agent.id.clone()),
                sender_name: "fixture".into(),
                sender_host: None,
                to: WireAddress::AgentName("fixture".into()),
                kind: MessageKind::Mail,
                urgency: MessageUrgency::Normal,
                durable: true,
                subject: "fixture mail".into(),
                body: "private-fixture-body".into(),
                reply_to: None,
                thread_id: None,
                deadline_at: None,
                at: At::from_unix_millis(2),
            })
            .unwrap();
        let captured = capture(&store);
        let expected = captured.expectations(&profile(), &limits()).unwrap();
        assert!(expected.known > 5);
        let mut changed = (*captured.state).clone();
        let (key, receipt) = changed
            .docs
            .in_collection(doc::WIRE_RCPT_LOG)
            .next()
            .unwrap();
        let key = key.clone();
        let mut receipt = receipt.clone();
        receipt["status"] = json!("acked");
        receipt["acked_at"] = json!(At::from_unix_millis(3));
        changed
            .docs
            .insert(doc::WIRE_RCPT_LOG.into(), key, Arc::new(receipt));
        let reader = FerroStore::assemble(
            Inner::Memory(Mutex::new(Arc::new(changed))),
            "fixture".into(),
        );
        let wrong =
            expectations_from_reader(&reader, expected.descriptor.clone(), &profile(), &limits())
                .unwrap();
        assert!(crate::recovery::verify::compare(&expected, &wrong, &limits()).is_err());
        let mut state = (*captured.state).clone();
        let (key, receipt) = state.docs.in_collection(doc::WIRE_RCPT_LOG).next().unwrap();
        let key = key.clone();
        let mut receipt = receipt.clone();
        receipt["status"] = json!("invalid");
        state
            .docs
            .insert(doc::WIRE_RCPT_LOG.into(), key, Arc::new(receipt));
        let read =
            FerroStore::assemble(Inner::Memory(Mutex::new(Arc::new(state))), "fixture".into());
        assert!(
            expectations_from_reader(&read, expected.descriptor.clone(), &profile(), &limits())
                .is_err()
        );
        let mut state = (*captured.state).clone();
        state.docs.remove(doc::WIRE_MESSAGES, msg.id.as_str());
        let read =
            FerroStore::assemble(Inner::Memory(Mutex::new(Arc::new(state))), "fixture".into());
        assert!(
            expectations_from_reader(&read, expected.descriptor, &profile(), &limits()).is_err()
        );
    }
    #[test]
    fn project_references_require_resolution_exception_or_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("project.fls")).unwrap();
        let project = store.add_project("fixture-root").unwrap();
        let record = store.add_record(&project, "fixture record").unwrap();
        let p = ValidationProfile::new(
            crate::recovery::ProfileKind::ProjectLedgerRecords,
            "fixture",
        );
        assert!(capture(&store).expectations(&p, &limits()).is_ok());
        store.delete_doc(doc::PROJECTS, project.as_str()).unwrap();
        let captured = capture(&store);
        assert!(captured.expectations(&p, &limits()).is_err());
        let digest = reference_digest(
            doc::RECORDS,
            record.as_str(),
            "/project",
            doc::PROJECTS,
            project.as_str(),
            &limits(),
        )
        .unwrap();
        let mut except = p.clone();
        except
            .preexisting_exceptions
            .push(crate::recovery::profiles::ReferenceException {
                reference_sha256: digest.clone(),
                reason: "fixture pre-existing imported reference".into(),
            });
        assert_eq!(
            captured
                .expectations(&except, &limits())
                .unwrap()
                .exceptions,
            1
        );
        let mut external = p;
        external
            .dependencies
            .push(crate::recovery::profiles::ReferenceDependency {
                reference_sha256: digest,
                catalog_identity: "other-fixture".into(),
                required_sequence: Some(1),
                policy: crate::recovery::profiles::DependencyPolicy::ReconcileBeforeAdmission,
            });
        assert_eq!(
            captured
                .expectations(&external, &limits())
                .unwrap()
                .dependencies,
            1
        );
    }
    #[test]
    fn incompatible_schema_and_feature_bits_refuse_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("schema.fls")).unwrap();
        store
            .put_doc_unchecked(doc::SCHEMA_COLLECTION, doc::SCHEMA_KEY, &json!(999), false)
            .unwrap();
        assert!(matches!(
            capture(&store).expectations(&profile(), &limits()),
            Err(RecoveryError::Unsupported { .. })
        ));
        let path = dir.path().join("features.fls");
        let file = std::fs::File::create(&path).unwrap();
        let sb = Superblock {
            epoch: 1,
            snapshot_offset: HEADER_BYTES,
            snapshot_length: 0,
            snapshot_seq: 0,
            wal_offset: HEADER_BYTES,
            wal_length: 0,
        };
        file.write_all_at(
            &layout::encode_superblock_as(&sb, 2, layout::ENGINE_SNAPSHOT_WAL, 1 << 63),
            0,
        )
        .unwrap();
        file.set_len(HEADER_BYTES).unwrap();
        drop(file);
        let before = std::fs::read(&path).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert!(decode_existing(&file, &limits()).is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
    }
    #[test]
    fn valid_column_and_vector_changes_at_equal_sequence_fail() {
        use crate::ferrostore::{ColumnDef, ColumnType, ColumnValue, VectorMetric, VectorStorage};
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("mixed.fls")).unwrap();
        let columns = [ColumnDef {
            name: "n".into(),
            ty: ColumnType::Int64,
        }];
        store.create_column_table("table", &columns).unwrap();
        store
            .append_column_rows("table", &[vec![ColumnValue::Int64(7)]])
            .unwrap();
        store
            .create_vector_collection("vectors", 2, VectorStorage::F32, VectorMetric::L2, 4, 8, 8)
            .unwrap();
        store
            .upsert_vector("vectors", "v", &[1.0, 0.0], None)
            .unwrap();
        let captured = capture(&store);
        let expected = captured.expectations(&profile(), &limits()).unwrap();
        for column in [true, false] {
            let mut state = (*captured.state).clone();
            if column {
                Arc::make_mut(state.tables.get_mut("table").unwrap()).groups[0] =
                    columnar::encode_row_group(&columns, &[vec![ColumnValue::Int64(8)]]).unwrap();
            } else {
                Arc::make_mut(state.vectors.get_mut("vectors").unwrap())
                    .vectors
                    .get_mut("v")
                    .unwrap()
                    .bytes = [0.0f32.to_le_bytes(), 1.0f32.to_le_bytes()].concat();
            }
            let reader =
                FerroStore::assemble(Inner::Memory(Mutex::new(Arc::new(state))), "fixture".into());
            let changed = expectations_from_reader(
                &reader,
                expected.descriptor.clone(),
                &profile(),
                &limits(),
            )
            .unwrap();
            assert!(crate::recovery::verify::compare(&expected, &changed, &limits()).is_err());
        }
    }
    #[test]
    fn project_gates_attempts_and_registry_indexes_are_domain_checked() {
        use fl_core::{
            log::{Attempt, AttemptStatus, PathsTouched},
            model::{AgentSpec, GateKind, Selector},
            store::Ledger,
        };
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("ledger.fls")).unwrap();
        let project = store.add_project("fixture-root").unwrap();
        let record = store.add_record(&project, "fixture-record").unwrap();
        let gate = store
            .add_gate(
                &project,
                "fixture-gate",
                GateKind::Agent(AgentSpec {
                    adapter: "fixture-inert".into(),
                    question: "fixture-question".into(),
                    timeout_secs: 1,
                }),
                Selector::Glob {
                    pattern: "*".into(),
                },
                1,
                "fixture-commit",
                "fixture",
            )
            .unwrap();
        store
            .append_attempt(Attempt {
                id: None,
                at: None,
                project: project.clone(),
                record: record.clone(),
                adapter: "fixture".into(),
                status: AttemptStatus::Completed,
                duration_ms: 1,
                tokens_in: 1,
                tokens_out: 1,
                cost_usd_micros: 0,
                paths_touched: PathsTouched::Listed(vec![]),
                output_excerpt: None,
            })
            .unwrap();
        let captured = capture(&store);
        assert!(captured.expectations(&profile(), &limits()).is_ok());
        store.delete_doc(doc::RECORDS, record.as_str()).unwrap();
        assert!(capture(&store).expectations(&profile(), &limits()).is_err());
        let mut state = (*captured.state).clone();
        let mut row = state.docs.get(doc::GATES, gate.as_str()).unwrap().clone();
        row["project"] = json!(record.as_str());
        state
            .docs
            .insert(doc::GATES.into(), gate.as_str().into(), Arc::new(row));
        let read =
            FerroStore::assemble(Inner::Memory(Mutex::new(Arc::new(state))), "fixture".into());
        assert!(
            expectations_from_reader(
                &read,
                captured
                    .expectations(&profile(), &limits())
                    .unwrap()
                    .descriptor,
                &profile(),
                &limits()
            )
            .is_err()
        );
    }
    #[test]
    fn actual_persisted_ledger_publication_and_external_dependency_are_retained() {
        use fl_core::split::Outbox;
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("outbox.fls")).unwrap();
        let id = fl_core::Iri::parse("urn:uuid:00000000-0000-4000-8000-000000000099").unwrap();
        store.mark_published("fixture-repository", &[id]).unwrap();
        store
            .put_doc(
                doc::LEDGER_ROOTS,
                "fixture-repository",
                &json!("fixture-root-commit"),
            )
            .unwrap();
        let mut p = ValidationProfile::new(
            crate::recovery::ProfileKind::ProjectLedgerRecords,
            "fixture",
        );
        assert!(capture(&store).expectations(&p, &limits()).is_err());
        p.dependencies
            .push(crate::recovery::profiles::ReferenceDependency {
                reference_sha256: reference_digest(
                    doc::LEDGER_ROOTS,
                    "fixture-repository",
                    "",
                    "external_repository_revision",
                    "fixture-root-commit",
                    &limits(),
                )
                .unwrap(),
                catalog_identity: "fixture-repository".into(),
                required_sequence: None,
                policy: crate::recovery::profiles::DependencyPolicy::ReconcileBeforeAdmission,
            });
        let expected = capture(&store).expectations(&p, &limits()).unwrap();
        assert_eq!(expected.dependencies, 1);
        assert_eq!(expected.unknown, 0);
    }
}
