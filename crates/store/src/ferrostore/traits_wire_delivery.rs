//! Disabled preparatory native peer persistence; no production mutation caller.
use super::doc::{AGENTS, META, WIRE_INBOX, WIRE_MESSAGES, WIRE_RCPT_LOG};
use super::{FerroStore, Inner, Txn, doc};
use crate::{backend, decode};
use fl_core::StoreError;
use fl_core::ferrowire::DeliveryReceipt;
use fl_core::wire_delivery::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
type Def = &'static str;
const LEDGER: Def = "__wire_delivery_v1";
const RESERVATIONS: Def = "__wire_delivery_ids_v1";
const CONTROLS: Def = "__wire_delivery_control_v1";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row<T> {
    schema: u16,
    value: T,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LifetimeAdmissionState {
    Open,
    Closing,
    Closed,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LifetimeRegistryRow {
    registration: LifetimeRegistration,
    state: LifetimeAdmissionState,
    generation: Revision,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn lp(value: &str) -> Vec<u8> {
    let mut out = (value.len() as u64).to_be_bytes().to_vec();
    out.extend_from_slice(value.as_bytes());
    out
}
fn ledger_key(key: &DeliveryKey) -> String {
    let mut bytes = lp(key.original_id.as_str());
    bytes.extend(lp(key.recipient_id.as_str()));
    hex(&bytes)
}
fn recipient_key(key: &DeliveryKey) -> String {
    format!("recipient/{}", hex(&lp(key.recipient_id.as_str())))
}
fn identity_key(key: &str) -> String {
    format!("identity/{}", hex(&lp(key)))
}
fn lifetime_key(id: Digest) -> String {
    format!("lifetime/{}", hex(&id.0))
}

// These test-only observations count actual point reads/decodes/writes, not model claims.
#[cfg(test)]
thread_local! { static IO: std::cell::Cell<(u64,u64,u64)> = const { std::cell::Cell::new((0,0,0)) }; }
fn note_io(_kind: usize) {
    #[cfg(test)]
    IO.with(|cell| {
        let (mut r, mut d, mut w) = cell.get();
        match _kind {
            0 => r += 1,
            1 => d += 1,
            _ => w += 1,
        };
        cell.set((r, d, w));
    });
}
trait Reader {
    fn raw(&self, table: Def, key: &str) -> Result<Option<String>, StoreError>;
    fn receipt(&self, key: &DeliveryKey) -> Result<Option<String>, StoreError>;
}
impl Reader for Txn {
    fn raw(&self, table: Def, key: &str) -> Result<Option<String>, StoreError> {
        note_io(0);
        self.get(table, key)
            .map(|value| serde_json::to_string(&value).map_err(backend))
            .transpose()
    }
    fn receipt(&self, key: &DeliveryKey) -> Result<Option<String>, StoreError> {
        self.raw(
            WIRE_INBOX,
            &doc::pair(key.recipient_id.as_str(), key.original_id.as_str()),
        )
    }
}
fn json<T: DeserializeOwned>(raw: &str) -> Result<T, StoreError> {
    note_io(1);
    serde_json::from_str(raw).map_err(decode)
}
fn get<T: DeserializeOwned>(
    reader: &impl Reader,
    table: Def,
    key: &str,
) -> Result<Option<T>, StoreError> {
    reader.raw(table, key)?.map(|raw| json(&raw)).transpose()
}
fn row<T: DeserializeOwned>(
    reader: &impl Reader,
    table: Def,
    key: &str,
) -> Result<Option<T>, StoreError> {
    let row: Option<Row<T>> = get(reader, table, key)?;
    row.map(|r| {
        if r.schema == 1 {
            Ok(r.value)
        } else {
            Err(decode("unsupported managed row schema"))
        }
    })
    .transpose()
}
fn required<T>(value: Option<T>) -> Result<T, StoreError> {
    value.ok_or_else(|| decode("missing managed control"))
}
fn global(reader: &impl Reader) -> Result<GlobalControl, StoreError> {
    let value: GlobalControl = required(row(reader, CONTROLS, "global")?)?;
    if value.active_index.len() > 16
        || usize::from(value.active_attempts) != value.active_index.len()
        || value.active_index.iter().enumerate().any(|(i, o)| {
            value.active_index[..i]
                .iter()
                .any(|p| p.key.recipient_id == o.key.recipient_id)
        })
    {
        return Err(decode("inconsistent managed active index"));
    }
    Ok(value)
}
fn controls(reader: &impl Reader) -> Result<GlobalControl, StoreError> {
    let continuity: Continuity = required(row(reader, CONTROLS, "continuity")?)?;
    if continuity.schema != 1 {
        return Err(decode("unknown managed continuity schema"));
    }
    let admissions: Vec<Digest> = required(row(reader, CONTROLS, "live_admissions")?)?;
    if admissions.len() > 64
        || admissions
            .iter()
            .enumerate()
            .any(|(i, id)| admissions[..i].contains(id))
    {
        return Err(decode("invalid live admissions index"));
    }
    global(reader)
}
// Validate persisted sizes, not the writer's larger reservation for future evidence.
// Frozen bytes have their own limit; their metadata still counts in compact JSON.
fn validate_record_bounds(record: &DeliveryRecord) -> Result<(), StoreError> {
    if record.annotation.len() > 4096
        || record.key.original_id.as_str().len() > 4096
        || record.key.recipient_id.as_str().len() > 4096
        || record.binding.authority_id.len() > 4096
    {
        return Err(decode("oversized managed record field"));
    }
    let mut compact = record.clone();
    for attempt in &mut compact.attempts {
        if attempt.detail.chars().count() > 300
            || attempt.route.thread_id.len() > 4096
            || attempt
                .route
                .executable
                .as_ref()
                .is_some_and(|s| s.len() > 4096)
            || matches!(&attempt.evidence, Some(DeliveryEvidence::AppServerAccepted { thread, turn })
                if thread.len() > 4096 || turn.len() > 4096)
        {
            return Err(decode("oversized managed evidence field"));
        }
        if let Some(frozen) = &mut attempt.frozen {
            if frozen.bytes.len() > 1_048_576 + 14
                || frozen.renderer.len() > 4096
                || frozen
                    .expected_turn
                    .as_ref()
                    .is_some_and(|s| s.len() > 4096)
            {
                return Err(decode("oversized managed frozen payload"));
            }
            frozen.bytes.clear();
        }
        if serde_json::to_vec(attempt).map_err(decode)?.len() > 8192 {
            return Err(decode("oversized managed evidence summary"));
        }
    }
    if serde_json::to_vec(&compact).map_err(decode)?.len() > 32_768 {
        return Err(decode("oversized managed compact record"));
    }
    Ok(())
}
fn read_record(
    reader: &impl Reader,
    key: &DeliveryKey,
) -> Result<Option<DeliveryRecord>, StoreError> {
    let record: Option<DeliveryRecord> = get(reader, LEDGER, &ledger_key(key))?;
    if record.as_ref().is_some_and(|r| {
        r.schema != 1
            || r.key != *key
            || r.binding.recipient_id != key.recipient_id
            || r.attempts.len() > 3
            || usize::from(r.prepares) != r.attempts.len()
            || r.attempts
                .iter()
                .filter(|a| a.capacity == CapacityState::Held)
                .count()
                > 1
            || r.attempts.iter().filter(|a| a.frozen.is_some()).count() > 1
            || (r.eligibility == Eligibility::Prepared
                && r.attempts.last().is_none_or(|a| {
                    a.armed_at.is_some()
                        || a.frozen.is_some()
                        || a.evidence.is_some()
                        || a.capacity != CapacityState::Held
                }))
    }) {
        return Err(decode("invalid managed record"));
    }
    if let Some(record) = &record {
        validate_record_bounds(record)?;
    }
    Ok(record)
}
fn recipient(
    reader: &impl Reader,
    key: &DeliveryKey,
    g: &GlobalControl,
) -> Result<RecipientControl, StoreError> {
    let recipient: RecipientControl = required(row(reader, CONTROLS, &recipient_key(key))?)?;
    let owners: Vec<_> = g
        .active_index
        .iter()
        .filter(|o| o.key.recipient_id == key.recipient_id)
        .collect();
    let consistent = match &recipient.active {
        Some(owner) => owners.as_slice() == [owner],
        None => owners.is_empty(),
    };
    if recipient.binding.recipient_id != key.recipient_id
        || !consistent
        || recipient
            .barrier
            .as_ref()
            .is_some_and(|k| k.recipient_id != key.recipient_id)
    {
        return Err(decode("inconsistent managed recipient"));
    }
    Ok(recipient)
}
fn checked_record(
    reader: &impl Reader,
    key: &DeliveryKey,
    g: &GlobalControl,
) -> Result<Option<(DeliveryRecord, RecipientControl)>, StoreError> {
    let Some(record) = read_record(reader, key)? else {
        if g.active_index.iter().any(|o| o.key == *key) {
            return Err(decode("missing active record"));
        }
        return Ok(None);
    };
    let recipient = recipient(reader, key, g)?;
    let reservation: IdReservation = required(row(
        reader,
        RESERVATIONS,
        &hex(&lp(key.original_id.as_str())),
    )?)?;
    if reservation.original_id != key.original_id
        || reservation.generation != record.generation
        || record.binding != recipient.binding
    {
        return Err(decode("inconsistent managed reservation/binding"));
    }
    let held = record
        .attempts
        .iter()
        .find(|a| a.capacity == CapacityState::Held);
    if let Some(held) = held {
        let owner = ActiveOwner {
            key: key.clone(),
            attempt: held.binding.clone(),
        };
        if recipient.active.as_ref() != Some(&owner) || !g.active_index.contains(&owner) {
            return Err(decode("missing managed owner index"));
        }
    } else if recipient.active.as_ref().is_some_and(|o| o.key == *key) {
        return Err(decode("inactive managed record indexed active"));
    }
    Ok(Some((record, recipient)))
}
fn has_tables(tx: &Txn) -> Result<bool, StoreError> {
    let names = tx.collections();
    let count = [LEDGER, RESERVATIONS, CONTROLS]
        .iter()
        .filter(|name| names.iter().any(|n| n == **name))
        .count();
    match count {
        0 => Ok(false),
        3 => Ok(true),
        _ => Err(decode("partial managed collections")),
    }
}
fn unsupported() -> StoreError {
    backend("managed wire delivery: UnsupportedBackend")
}
impl WireDeliveryStore for FerroStore {
    fn apply_delivery(&self, command: DeliveryCommand) -> Result<DeliveryResult, StoreError> {
        Ok(DeliveryResult::Refused {
            reason: if matches!(self.inner, Inner::Remote(_))
                || matches!(
                    command,
                    DeliveryCommand::Prepare(_) | DeliveryCommand::Arm(_)
                ) {
                HoldReason::UnsupportedBackend
            } else {
                HoldReason::Disabled
            },
        })
    }
    fn inspect_delivery(&self, key: &DeliveryKey) -> Result<Option<DeliveryView>, StoreError> {
        if matches!(self.inner, Inner::Remote(_)) {
            return Err(unsupported());
        }
        self.with_read(|tx| {
            if !has_tables(tx)? {
                return Ok(None);
            }
            let g = controls(tx)?;
            Ok(checked_record(tx, key, &g)?.map(|(record, recipient)| record.view(&recipient)))
        })
    }
    fn active_delivery_owners(&self) -> Result<Vec<CapacityRetirementTarget>, StoreError> {
        if matches!(self.inner, Inner::Remote(_)) {
            return Err(unsupported());
        }
        self.with_read(|tx| {
            if !has_tables(tx)? {
                return Ok(Vec::new());
            }
            let global = controls(tx)?;
            global
                .active_index
                .iter()
                .map(|owner| {
                    let (record, recipient) = required(checked_record(tx, &owner.key, &global)?)?;
                    Ok(CapacityRetirementTarget {
                        owner: owner.clone(),
                        record_revision: record.revision,
                        recipient_revision: recipient.revision,
                        global_revision: global.revision,
                    })
                })
                .collect()
        })
    }
    fn retire_orphan_capacity(
        &self,
        _target: CapacityRetirementTarget,
    ) -> Result<CapacityRetirementResult, StoreError> {
        if matches!(self.inner, Inner::Remote(_)) {
            return Err(unsupported());
        }
        Ok(CapacityRetirementResult::Unproven)
    }
}
#[cfg(target_os = "linux")]
impl crate::wire_delivery_lifetime::LocalLifetimeAdmissionStore for FerroStore {
    fn admit_local_lifetime(
        &self,
        _input: crate::wire_delivery_lifetime::LocalAdmissionInput,
    ) -> Result<LifetimeRegistration, crate::wire_delivery_lifetime::LocalAdmissionError> {
        Err(if matches!(self.inner, Inner::Remote(_)) {
            crate::wire_delivery_lifetime::LocalAdmissionError::UnsupportedBackend
        } else {
            crate::wire_delivery_lifetime::LocalAdmissionError::Unproven
        })
    }
    fn close_local_lifetime(
        &self,
        _registration: &LifetimeRegistration,
    ) -> Result<(), crate::wire_delivery_lifetime::LocalAdmissionError> {
        Err(if matches!(self.inner, Inner::Remote(_)) {
            crate::wire_delivery_lifetime::LocalAdmissionError::UnsupportedBackend
        } else {
            crate::wire_delivery_lifetime::LocalAdmissionError::Unproven
        })
    }
}

// Prepared kernels deliberately have no production caller until real scope producers
// and the separately reviewed mutation/enrollment fences exist.
#[allow(dead_code)]
fn context(tx: &mut Txn, command: &DeliveryCommand) -> Result<TransitionContext, StoreError> {
    let names = tx.collections();
    if [
        LEDGER,
        RESERVATIONS,
        CONTROLS,
        META,
        WIRE_INBOX,
        WIRE_RCPT_LOG,
        WIRE_MESSAGES,
        AGENTS,
    ]
    .iter()
    .any(|required| !names.iter().any(|name| name == required))
    {
        return Err(decode("uninitialized managed tables"));
    }
    let key = command.key();
    let global = controls(tx)?;
    let recipient = recipient(tx, key, &global)?;
    let record = read_record(tx, key)?;
    let reservation = row(tx, RESERVATIONS, &hex(&lp(key.original_id.as_str())))?;
    let continuity = required(row(tx, CONTROLS, "continuity")?)?;
    let message: Option<fl_core::ferrowire::WireMessage> =
        get(tx, WIRE_MESSAGES, key.original_id.as_str())?;
    let receipt: Option<DeliveryReceipt> = tx.receipt(key)?.map(|raw| json(&raw)).transpose()?;
    let registry = get(tx, AGENTS, key.recipient_id.as_str())?;
    let selector = match &message {
        Some(message) => {
            indexed_identity(tx, &serde_json::to_string(&message.to).map_err(backend)?)?
        }
        None => IdentityResolution::Unresolved,
    };
    let receipt_alias = match &receipt {
        Some(receipt) => indexed_identity(tx, &format!("name:{}", receipt.recipient_name))?,
        None => IdentityResolution::Unresolved,
    };
    let lifetime = match command {
        DeliveryCommand::Prepare(input) => registered(tx, &input.attempt.lifetime)?,
        DeliveryCommand::Arm(input) => registered(tx, &input.attempt.lifetime)?,
        _ => None,
    };
    Ok(TransitionContext {
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
    })
}
fn indexed_identity(reader: &impl Reader, key: &str) -> Result<IdentityResolution, StoreError> {
    let index: IdentityIndexRow = required(row(reader, CONTROLS, &identity_key(key))?)?;
    if index.key != key {
        return Err(decode("mismatched managed identity index"));
    }
    Ok(index.resolution)
}
fn registered(
    reader: &impl Reader,
    requested: &LifetimeRegistration,
) -> Result<Option<LifetimeRegistration>, StoreError> {
    let registry: Option<LifetimeRegistryRow> = row(reader, CONTROLS, &lifetime_key(requested.id))?;
    let live: Vec<Digest> = required(row(reader, CONTROLS, "live_admissions")?)?;
    Ok(registry
        .filter(|r| {
            r.registration == *requested
                && r.generation == requested.revision
                && matches!(r.state, LifetimeAdmissionState::Open)
                && live.contains(&requested.id)
        })
        .map(|r| r.registration))
}
fn write<T: Serialize>(tx: &mut Txn, table: Def, key: &str, value: &T) -> Result<(), StoreError> {
    doc::put_json(tx, table, key, value)?;
    note_io(2);
    Ok(())
}
fn write_row<T: Serialize>(
    tx: &mut Txn,
    table: Def,
    key: &str,
    value: T,
) -> Result<(), StoreError> {
    write(tx, table, key, &Row { schema: 1, value })
}
// This private transaction entry point is executable only by owning-module tests.
#[allow(dead_code)]
fn apply_delivery_in(tx: &mut Txn, command: DeliveryCommand) -> Result<DeliveryResult, StoreError> {
    let ctx = context(tx, &command)?;
    let delta = transition(ctx, command)?;
    if let Some(record) = delta.record {
        write(tx, LEDGER, &ledger_key(&record.key), &record)?;
    }
    if let Some(reservation) = delta.reservation {
        write_row(
            tx,
            RESERVATIONS,
            &hex(&lp(reservation.original_id.as_str())),
            reservation,
        )?;
    }
    if let Some(recipient) = delta.recipient {
        let key = format!(
            "recipient/{}",
            hex(&lp(recipient.binding.recipient_id.as_str()))
        );
        write_row(tx, CONTROLS, &key, recipient)?;
    }
    if let Some(global) = delta.global {
        write_row(tx, CONTROLS, "global", global)?;
    }
    if let Some(receipt) = delta.receipt {
        let next = doc::get_json::<u64>(tx, META, doc::NEXT_WIRE_RCPT_SEQ)?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| backend("receipt sequence overflow"))?;
        // All fallible serialization precedes staging; no partial delta on serialization failure.
        doc::put_json(
            tx,
            WIRE_INBOX,
            &doc::pair(receipt.recipient_id.as_str(), receipt.message_id.as_str()),
            &receipt,
        )?;
        doc::put_json(tx, META, doc::NEXT_WIRE_RCPT_SEQ, &next)?;
        doc::put_json(tx, WIRE_RCPT_LOG, &doc::key_u64(next), &receipt)?;
        note_io(2);
        note_io(2);
        note_io(2);
    }
    Ok(delta.result)
}
// Not serializable, transferable, or constructible outside this owning module.
#[allow(dead_code)]
struct CapacityOnlyObservation {
    target: CapacityRetirementTarget,
    registry_generation: Revision,
}
// Model-only closure premises; this is not a physical scope verifier.
#[allow(dead_code)]
fn retire_capacity_in(
    tx: &mut Txn,
    target: &CapacityRetirementTarget,
    observation: CapacityOnlyObservation,
) -> Result<CapacityRetirementResult, StoreError> {
    if observation.target != *target {
        return Ok(CapacityRetirementResult::Stale);
    }
    let registry: Option<LifetimeRegistryRow> = row(
        tx,
        CONTROLS,
        &lifetime_key(target.owner.attempt.lifetime.id),
    )?;
    let Some(registry) = registry else {
        return Ok(CapacityRetirementResult::Unproven);
    };
    if registry.registration != target.owner.attempt.lifetime
        || registry.generation != observation.registry_generation
    {
        return Ok(CapacityRetirementResult::Stale);
    }
    if !matches!(registry.state, LifetimeAdmissionState::Closed) {
        return Ok(CapacityRetirementResult::Unproven);
    }
    let ctx = context(
        tx,
        &DeliveryCommand::Close {
            key: target.owner.key.clone(),
        },
    )?;
    let delta = transition_capacity_retirement(ctx, target.clone())?;
    if let Some(record) = delta.record {
        write(tx, LEDGER, &ledger_key(&record.key), &record)?;
    }
    if let Some(recipient) = delta.recipient {
        write_row(tx, CONTROLS, &recipient_key(&target.owner.key), recipient)?;
    }
    if let Some(global) = delta.global {
        write_row(tx, CONTROLS, "global", global)?;
    }
    Ok(delta.result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnyStore, observe_preparatory_rpc, observe_preparatory_writes};
    use fl_core::agent::{AgentRecord, AgentRole, AgentStatus};
    use fl_core::ferrowire::{DeliveryStatus, WireAddress, WireMessage};
    use fl_core::wire_delivery::conformance::{fixture_commands, fixture_message};
    use fl_core::{At, MessageId};
    use serde_json::{Value, json};
    use std::sync::{Arc, Barrier};
    fn prepare() -> PrepareInput {
        let DeliveryCommand::Prepare(p) = fixture_commands().remove(0) else {
            unreachable!()
        };
        p
    }
    fn put<T: Serialize>(tx: &mut Txn, table: Def, key: &str, value: &T) {
        doc::put_json(tx, table, key, value).unwrap();
    }
    fn put_row<T: Serialize>(tx: &mut Txn, table: Def, key: &str, value: T) {
        put(tx, table, key, &Row { schema: 1, value });
    }
    fn install(store: &FerroStore, input: &PrepareInput, message: &WireMessage) {
        store
            .with_write(|tx| {
                // Ferro only retains nonempty collections. These ordinary strict unrelated
                // rows create fixture presence, never enrollment or an admission grant.
                let anchor = DeliveryKey {
                    original_id: MessageId(fl_core::ids::seq_iri(9990)),
                    recipient_id: input.key.recipient_id.clone(),
                };
                if tx.get(LEDGER, &ledger_key(&anchor)).is_none() {
                    put(
                        tx,
                        LEDGER,
                        &ledger_key(&anchor),
                        &DeliveryRecord {
                            schema: 1,
                            key: anchor.clone(),
                            generation: Digest([42; 32]),
                            binding: input.binding.clone(),
                            revision: Revision(1),
                            eligibility: Eligibility::Eligible,
                            prepares: 0,
                            attempts: vec![],
                            acknowledgment: AcknowledgmentState {
                                manual_at: None,
                                legacy_unknown: false,
                            },
                            held: None,
                            annotation: String::new(),
                        },
                    );
                    put_row(
                        tx,
                        RESERVATIONS,
                        &hex(&lp(anchor.original_id.as_str())),
                        IdReservation {
                            original_id: anchor.original_id,
                            generation: Digest([42; 32]),
                        },
                    );
                }
                if row::<GlobalControl>(tx, CONTROLS, "global")
                    .unwrap()
                    .is_none()
                {
                    put_row(tx, CONTROLS, "global", GlobalControl::default());
                }
                if row::<Continuity>(tx, CONTROLS, "continuity")
                    .unwrap()
                    .is_none()
                {
                    put_row(
                        tx,
                        CONTROLS,
                        "continuity",
                        Continuity {
                            schema: 1,
                            cutover: At::from_unix_millis(0),
                            policy_hash: input.binding.policy_hash,
                            local_epoch: input.binding.local_epoch,
                            enabled: true,
                            intact: true,
                        },
                    );
                }
                if row::<RecipientControl>(tx, CONTROLS, &recipient_key(&input.key))
                    .unwrap()
                    .is_none()
                {
                    put_row(
                        tx,
                        CONTROLS,
                        &recipient_key(&input.key),
                        RecipientControl {
                            binding: input.binding.clone(),
                            revision: Revision(1),
                            active: None,
                            barrier: None,
                            identity_conflict: false,
                        },
                    );
                }
                let mut admissions: Vec<Digest> = row(tx, CONTROLS, "live_admissions")
                    .unwrap()
                    .unwrap_or_default();
                if !admissions.contains(&input.attempt.lifetime.id) {
                    admissions.push(input.attempt.lifetime.id);
                }
                put_row(tx, CONTROLS, "live_admissions", admissions);
                put_row(
                    tx,
                    CONTROLS,
                    &lifetime_key(input.attempt.lifetime.id),
                    LifetimeRegistryRow {
                        registration: input.attempt.lifetime.clone(),
                        state: LifetimeAdmissionState::Open,
                        generation: input.attempt.lifetime.revision,
                    },
                );
                let name = match &message.to {
                    WireAddress::AgentName(n) => n.clone(),
                    _ => "bob".into(),
                };
                let agent = AgentRecord {
                    id: input.key.recipient_id.clone(),
                    name_key: name.clone(),
                    display_name: name.clone(),
                    email: "fixture@example.test".into(),
                    project: "ferroloop".into(),
                    role: AgentRole::Assistant,
                    extra_areas: vec![],
                    status: AgentStatus::Active,
                    updated_at: At::from_unix_millis(1),
                    by: "fixture".into(),
                };
                put(tx, AGENTS, agent.id.as_str(), &agent);
                put(tx, WIRE_MESSAGES, message.id.as_str(), message);
                let receipt = DeliveryReceipt {
                    message_id: message.id.clone(),
                    recipient_id: input.key.recipient_id.iri().clone(),
                    recipient_name: name.clone(),
                    status: DeliveryStatus::Pending,
                    attempts: 0,
                    updated_at: message.created_at.clone(),
                    acked_at: None,
                };
                put(
                    tx,
                    WIRE_INBOX,
                    &doc::pair(input.key.recipient_id.as_str(), message.id.as_str()),
                    &receipt,
                );
                if tx.get(WIRE_RCPT_LOG, "0").is_none() {
                    put(tx, WIRE_RCPT_LOG, "0", &receipt);
                }
                if tx.get(META, doc::NEXT_WIRE_RCPT_SEQ).is_none() {
                    put(tx, META, doc::NEXT_WIRE_RCPT_SEQ, &0u64);
                }
                for key in [
                    serde_json::to_string(&message.to).unwrap(),
                    format!("name:{name}"),
                ] {
                    put_row(
                        tx,
                        CONTROLS,
                        &identity_key(&key),
                        IdentityIndexRow {
                            key,
                            resolution: IdentityResolution::Unique {
                                recipient_id: input.key.recipient_id.clone(),
                                binding_revision: input.binding.binding_revision,
                            },
                        },
                    );
                }
                Ok(())
            })
            .unwrap();
    }

    fn snapshot(store: &FerroStore) -> Value {
        store.with_read(|tx|Ok(json!({"seq":tx.local_base()?.last_seq,"collections":tx.collections().into_iter().map(|name|{let rows=tx.scan(&name);(name,rows)}).collect::<std::collections::BTreeMap<_,_>>()}))).unwrap()
    }
    fn zero<T>(f: impl FnOnce() -> T) -> T {
        let ((result, rpc), writes) = observe_preparatory_writes(|| observe_preparatory_rpc(f));
        assert_eq!(writes, 0);
        assert_eq!(rpc, 0);
        result
    }
    fn model(store: &FerroStore, command: DeliveryCommand) -> DeliveryResult {
        store
            .with_write(|tx| apply_delivery_in(tx, command.clone()))
            .unwrap()
    }
    fn record(store: &FerroStore, key: &DeliveryKey) -> DeliveryRecord {
        store
            .with_read(|tx| Ok(read_record(tx, key)?.unwrap()))
            .unwrap()
    }
    fn target(store: &FerroStore) -> CapacityRetirementTarget {
        store.active_delivery_owners().unwrap().remove(0)
    }
    fn commands() -> Vec<DeliveryCommand> {
        let p = prepare();
        let mut c = fixture_commands();
        c.extend([
            DeliveryCommand::Release(ReleaseInput {
                key: p.key.clone(),
                revision: Revision(1),
                attempt: p.attempt,
                proof: DeliveryEvidence::PreMutationRefused,
                now: p.now.clone(),
            }),
            DeliveryCommand::InvalidatePrepared {
                key: p.key.clone(),
                revision: Revision(1),
                now: p.now,
            },
            DeliveryCommand::Annotate {
                key: p.key.clone(),
                text: "not authority".into(),
            },
            DeliveryCommand::Close { key: p.key },
        ]);
        c
    }
    fn refusals<S: WireDeliveryStore>(store: &S, remote: bool) {
        for command in commands() {
            let expected = if remote
                || matches!(
                    command,
                    DeliveryCommand::Prepare(_) | DeliveryCommand::Arm(_)
                ) {
                HoldReason::UnsupportedBackend
            } else {
                HoldReason::Disabled
            };
            let forged = serde_json::from_value(serde_json::to_value(command).unwrap()).unwrap();
            assert_eq!(
                zero(|| store.apply_delivery(forged)).unwrap(),
                DeliveryResult::Refused { reason: expected }
            );
        }
        let p = prepare();
        let t = CapacityRetirementTarget {
            owner: ActiveOwner {
                key: p.key.clone(),
                attempt: p.attempt,
            },
            record_revision: Revision(1),
            recipient_revision: Revision(1),
            global_revision: Revision(1),
        };
        if remote {
            assert!(
                matches!(zero(||store.inspect_delivery(&p.key)),Err(StoreError::Backend(s)) if s=="managed wire delivery: UnsupportedBackend")
            );
            assert!(
                matches!(zero(||store.active_delivery_owners()),Err(StoreError::Backend(s))if s=="managed wire delivery: UnsupportedBackend")
            );
            assert!(
                matches!(zero(||store.retire_orphan_capacity(t)),Err(StoreError::Backend(s))if s=="managed wire delivery: UnsupportedBackend")
            );
        } else {
            let _ = zero(|| store.inspect_delivery(&p.key));
            let _ = zero(|| store.active_delivery_owners());
            assert_eq!(
                zero(|| store.retire_orphan_capacity(t)).unwrap(),
                CapacityRetirementResult::Unproven
            );
        }
    }
    #[cfg(target_os = "linux")]
    fn admissions<S: crate::wire_delivery_lifetime::LocalLifetimeAdmissionStore>(
        store: &S,
        remote: bool,
    ) {
        use crate::wire_delivery_lifetime::{
            LocalAdmissionError, assert_preparatory_admit_refusal,
        };
        assert_preparatory_admit_refusal(store, remote);
        let result = zero(|| store.close_local_lifetime(&prepare().attempt.lifetime));
        assert!(if remote {
            matches!(result, Err(LocalAdmissionError::UnsupportedBackend))
        } else {
            matches!(result, Err(LocalAdmissionError::Unproven))
        });
    }
    fn public_refusals(store: &FerroStore) {
        let before = snapshot(store);
        refusals(store, false);
        #[cfg(target_os = "linux")]
        admissions(store, false);
        assert_eq!(snapshot(store), before);
    }
    #[test]
    fn managed_ferro_preparatory_public_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("public.fls");
        for store in [FerroStore::in_memory(), FerroStore::open(&path).unwrap()] {
            public_refusals(&store);
            install(&store, &prepare(), &fixture_message());
            public_refusals(&store);
            assert!(matches!(
                model(&store, fixture_commands().remove(0)),
                DeliveryResult::Prepared { .. }
            ));
            public_refusals(&store);
        }
        public_refusals(&FerroStore::open(&path).unwrap());
    }
    #[test]
    fn managed_any_store_preparatory_native_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("any.fls");
        for store in [FerroStore::in_memory(), FerroStore::open(&path).unwrap()] {
            let any = AnyStore::from(store);
            refusals(&any, false);
            #[cfg(target_os = "linux")]
            admissions(&any, false);
        }
        // Owning backend constructs its fixture before passing this very store through AnyStore.
        let store = FerroStore::open(&path).unwrap();
        install(&store, &prepare(), &fixture_message());
        model(&store, fixture_commands().remove(0));
        let view = store.inspect_delivery(&prepare().key).unwrap();
        let owners = store.active_delivery_owners().unwrap();
        let before = snapshot(&store);
        let any = AnyStore::from(store);
        refusals(&any, false);
        #[cfg(target_os = "linux")]
        admissions(&any, false);
        assert_eq!(zero(|| any.inspect_delivery(&prepare().key)).unwrap(), view);
        assert_eq!(zero(|| any.active_delivery_owners()).unwrap(), owners);
        drop(any);
        let store = FerroStore::open(&path).unwrap();
        assert_eq!(snapshot(&store), before);
        let any = AnyStore::from(store);
        refusals(&any, false);
        #[cfg(target_os = "linux")]
        admissions(&any, false);
        assert_eq!(zero(|| any.inspect_delivery(&prepare().key)).unwrap(), view);
        let redb = crate::RedbStore::open(&dir.path().join("any.redb")).unwrap();
        let any = AnyStore::from(redb);
        refusals(&any, false);
        #[cfg(target_os = "linux")]
        admissions(&any, false);
    }
    #[test]
    fn managed_ferro_preparatory_cas_reopen() {
        let store = FerroStore::in_memory();
        install(&store, &prepare(), &fixture_message());
        assert!(matches!(
            model(&store, fixture_commands().remove(0)),
            DeliveryResult::Prepared { .. }
        ));
    }
    fn next_input(n: u64, recipient: u64) -> (PrepareInput, WireMessage) {
        let mut p = prepare();
        let mut m = fixture_message();
        m.id = MessageId(fl_core::ids::seq_iri(10000 + n));
        m.thread_id = m.id.clone();
        m.to = WireAddress::AgentName(format!("bob-{recipient}"));
        p.key.original_id = m.id.clone();
        p.key.recipient_id = fl_core::AgentId(fl_core::ids::seq_iri(20000 + recipient));
        p.binding.recipient_id = p.key.recipient_id.clone();
        p.generation = canonical_generation(&m).unwrap();
        p.attempt.token.0[..8].copy_from_slice(&n.to_be_bytes());
        (p, m)
    }
    fn arm_for(p: &PrepareInput, revision: Revision) -> DeliveryCommand {
        let DeliveryCommand::Arm(mut a) = fixture_commands().remove(1) else {
            unreachable!()
        };
        a.key = p.key.clone();
        a.attempt = p.attempt.clone();
        a.route = p.route.clone();
        a.revision = revision;
        a.frozen.digest =
            canonical_payload(p.generation, &a.frozen.renderer, &a.frozen.bytes).unwrap();
        DeliveryCommand::Arm(a)
    }
    fn race(stores: [Arc<FerroStore>; 2], commands: [DeliveryCommand; 2]) -> Vec<DeliveryResult> {
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = stores
            .into_iter()
            .zip(commands)
            .map(|(s, c)| {
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    model(&s, c)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    }
    fn one_winner(results: &[DeliveryResult]) {
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, DeliveryResult::Prepared { .. }))
                .count(),
            1
        );
    }
    fn close_fixture(store: &FerroStore, t: &CapacityRetirementTarget) {
        store
            .with_write(|tx| {
                let mut r: LifetimeRegistryRow =
                    row(tx, CONTROLS, &lifetime_key(t.owner.attempt.lifetime.id))?.unwrap();
                r.state = LifetimeAdmissionState::Closed;
                put_row(tx, CONTROLS, &lifetime_key(r.registration.id), r);
                Ok(())
            })
            .unwrap();
    }
    fn retire(store: &FerroStore, t: &CapacityRetirementTarget) -> CapacityRetirementResult {
        store
            .with_write(|tx| {
                let r: LifetimeRegistryRow =
                    row(tx, CONTROLS, &lifetime_key(t.owner.attempt.lifetime.id))?.unwrap();
                retire_capacity_in(
                    tx,
                    t,
                    CapacityOnlyObservation {
                        target: t.clone(),
                        registry_generation: r.generation,
                    },
                )
            })
            .unwrap()
    }
    #[test]
    fn managed_ferro_preparatory_write_probe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.fls");
        for store in [FerroStore::in_memory(), FerroStore::open(&path).unwrap()] {
            let before = snapshot(&store);
            let (_, count) = observe_preparatory_writes(|| store.with_write(|_| Ok(())).unwrap());
            assert!(count > 0);
            assert_eq!(snapshot(&store), before);
            install(&store, &prepare(), &fixture_message());
            let (r, count) =
                observe_preparatory_writes(|| model(&store, fixture_commands().remove(0)));
            assert!(count > 0);
            assert!(matches!(r, DeliveryResult::Prepared { .. }));
            public_refusals(&store);
            let seq = store.last_seq().unwrap();
            let (_, count) = observe_preparatory_writes(|| store.commit_if_seq(seq, &[]).unwrap());
            assert!(count > 0);
            let (_, count) = observe_preparatory_writes(|| {
                store.put_doc("probe", "ordinary", &json!(1)).unwrap()
            });
            assert!(count > 0);
        }
        public_refusals(&FerroStore::open(&path).unwrap());
        assert!(
            std::panic::catch_unwind(|| observe_preparatory_rpc(|| panic!("observer unwind")))
                .is_err()
        );
        assert_eq!(observe_preparatory_rpc(|| ()).1, 0);
    }
    #[test]
    fn managed_ferro_preparatory_cas_reopen_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cas.fls");
        let store = Arc::new(FerroStore::open(&path).unwrap());
        let second = Arc::new(FerroStore::open(&path).unwrap());
        install(&store, &prepare(), &fixture_message());
        one_winner(&race(
            [store.clone(), second.clone()],
            [fixture_commands().remove(0), fixture_commands().remove(0)],
        ));
        assert_eq!(record(&store, &prepare().key).prepares, 1);
        let before = snapshot(&store);
        assert!(
            store
                .with_write(|tx| {
                    assert!(matches!(
                        apply_delivery_in(tx, fixture_commands().remove(1))?,
                        DeliveryResult::Armed { .. }
                    ));
                    Err::<(), _>(backend("injected before commit"))
                })
                .is_err()
        );
        assert_eq!(snapshot(&store), before);
        model(&store, fixture_commands().remove(1));
        let armed = snapshot(&store);
        assert_eq!(record(&store, &prepare().key).attempts[0].evidence, None);
        assert!(
            store
                .with_write(|tx| {
                    apply_delivery_in(tx, fixture_commands().remove(2))?;
                    Err::<(), _>(backend("abort settlement"))
                })
                .is_err()
        );
        assert_eq!(snapshot(&store), armed);
        model(&store, fixture_commands().remove(2));
        let committed = snapshot(&store);
        drop(second);
        drop(store);
        let store = Arc::new(FerroStore::open(&path).unwrap());
        assert_eq!(snapshot(&store), committed);
        model(&store, fixture_commands().remove(2));
        assert_eq!(snapshot(&store), committed);
        model(&store, fixture_commands().remove(3));
        let view = store.inspect_delivery(&prepare().key).unwrap().unwrap();
        assert!(view.acknowledgment.manual_at.is_some());
        assert_eq!(
            view.attempts[0].evidence,
            Some(DeliveryEvidence::VendorQueued)
        );
        let refused = fixture_commands().remove(0);
        let annotation = DeliveryCommand::Annotate {
            key: prepare().key,
            text: String::new(),
        };
        IO.set((0, 0, 0));
        model(&store, refused.clone());
        let base = IO.get();
        assert!(base.0 > 0 && base.1 > 0 && base.2 == 0);
        IO.set((0, 0, 0));
        model(&store, annotation.clone());
        let changed = IO.get();
        assert_eq!(changed.2, 1);
        store
            .with_write(|tx| {
                for n in 0..200 {
                    let (p, m) = next_input(n, n);
                    put(tx, WIRE_MESSAGES, m.id.as_str(), &m);
                    put(
                        tx,
                        LEDGER,
                        &ledger_key(&p.key),
                        &json!({"unrelated":"retained"}),
                    );
                }
                Ok(())
            })
            .unwrap();
        IO.set((0, 0, 0));
        model(&store, refused);
        assert_eq!(IO.get(), base);
        IO.set((0, 0, 0));
        model(&store, annotation);
        assert_eq!(IO.get(), changed);
        let path = dir.path().join("capacity.fls");
        let cap = Arc::new(FerroStore::open(&path).unwrap());
        for n in 0..15 {
            let (p, m) = next_input(n, n);
            install(&cap, &p, &m);
            assert!(matches!(
                model(&cap, DeliveryCommand::Prepare(p)),
                DeliveryResult::Prepared { .. }
            ));
        }
        let (a, ma) = next_input(90, 90);
        let (b, mb) = next_input(91, 91);
        install(&cap, &a, &ma);
        install(&cap, &b, &mb);
        let results = race(
            [cap.clone(), Arc::new(FerroStore::open(&path).unwrap())],
            [
                DeliveryCommand::Prepare(a.clone()),
                DeliveryCommand::Prepare(b.clone()),
            ],
        );
        one_winner(&results);
        assert_eq!(cap.active_delivery_owners().unwrap().len(), 16);
        for (p, r) in [a, b].iter().zip(results) {
            if matches!(r, DeliveryResult::Refused { .. }) {
                assert!(cap.inspect_delivery(&p.key).unwrap().is_none());
            }
        }
        let path = dir.path().join("recipient.fls");
        let single = Arc::new(FerroStore::open(&path).unwrap());
        let (a, ma) = next_input(200, 7);
        let (b, mb) = next_input(201, 7);
        install(&single, &a, &ma);
        install(&single, &b, &mb);
        one_winner(&race(
            [single.clone(), Arc::new(FerroStore::open(&path).unwrap())],
            [DeliveryCommand::Prepare(a), DeliveryCommand::Prepare(b)],
        ));
        assert_eq!(single.active_delivery_owners().unwrap().len(), 1);
        let bounded = FerroStore::in_memory();
        let mut p = prepare();
        install(&bounded, &p, &fixture_message());
        for round in 0..3 {
            p.attempt.token = PhaseToken([round + 10; 32]);
            let DeliveryResult::Prepared { record } =
                model(&bounded, DeliveryCommand::Prepare(p.clone()))
            else {
                panic!("prepare")
            };
            model(
                &bounded,
                DeliveryCommand::Release(ReleaseInput {
                    key: p.key.clone(),
                    revision: record.revision,
                    attempt: p.attempt.clone(),
                    proof: DeliveryEvidence::PreMutationRefused,
                    now: p.now.clone(),
                }),
            );
        }
        assert_eq!(
            model(&bounded, DeliveryCommand::Prepare(p)),
            DeliveryResult::Refused {
                reason: HoldReason::BudgetExhausted
            }
        );
    }
    #[test]
    fn managed_ferro_preparatory_retirement_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retire.fls");
        let store = FerroStore::open(&path).unwrap();
        let mut inputs = vec![];
        for n in 0..16 {
            let (p, m) = next_input(n, n);
            install(&store, &p, &m);
            model(&store, DeliveryCommand::Prepare(p.clone()));
            model(&store, arm_for(&p, Revision(1)));
            inputs.push(p);
        }
        let t = target(&store);
        close_fixture(&store, &t);
        let initial = snapshot(&store);
        let old = record(&store, &t.owner.key);
        let others: Vec<_> = inputs
            .iter()
            .filter(|p| p.key != t.owner.key)
            .map(|p| record(&store, &p.key))
            .collect();
        for field in 0..5 {
            let mut wrong = t.clone();
            match field {
                0 => wrong.owner.attempt.worker.0[0] ^= 1,
                1 => wrong.owner.attempt.lifetime.id.0[0] ^= 1,
                2 => wrong.record_revision.0 += 1,
                3 => wrong.recipient_revision.0 += 1,
                _ => wrong.global_revision.0 += 1,
            };
            let result = store
                .with_write(|tx| {
                    retire_capacity_in(
                        tx,
                        &wrong,
                        CapacityOnlyObservation {
                            target: wrong.clone(),
                            registry_generation: Revision(1),
                        },
                    )
                })
                .unwrap();
            assert!(matches!(
                result,
                CapacityRetirementResult::Stale | CapacityRetirementResult::Unproven
            ));
            assert_eq!(snapshot(&store), initial);
        }
        assert_eq!(
            store
                .with_write(|tx| retire_capacity_in(
                    tx,
                    &t,
                    CapacityOnlyObservation {
                        target: t.clone(),
                        registry_generation: Revision(999)
                    }
                ))
                .unwrap(),
            CapacityRetirementResult::Stale
        );
        assert_eq!(snapshot(&store), initial);
        assert!(
            store
                .with_write(|tx| {
                    retire_capacity_in(
                        tx,
                        &t,
                        CapacityOnlyObservation {
                            target: t.clone(),
                            registry_generation: Revision(1),
                        },
                    )?;
                    Err::<(), _>(backend("abort retirement"))
                })
                .is_err()
        );
        assert_eq!(snapshot(&store), initial);
        assert!(matches!(
            retire(&store, &t),
            CapacityRetirementResult::Retired { .. }
        ));
        assert_eq!(store.active_delivery_owners().unwrap().len(), 15);
        let after = record(&store, &t.owner.key);
        assert_eq!(after.prepares, old.prepares);
        assert_eq!(after.eligibility, old.eligibility);
        assert_eq!(after.acknowledgment, old.acknowledgment);
        assert_eq!(after.attempts[0].evidence, old.attempts[0].evidence);
        assert_eq!(after.attempts[0].capacity, CapacityState::Retired);
        for p in inputs.iter().filter(|p| p.key != t.owner.key).zip(others) {
            assert_eq!(record(&store, &p.0.key), p.1);
        }
        let committed = snapshot(&store);
        assert!(matches!(
            retire(&store, &t),
            CapacityRetirementResult::AlreadyReleased { .. }
        ));
        assert_eq!(snapshot(&store), committed);
        drop(store);
        let store = FerroStore::open(&path).unwrap();
        assert_eq!(snapshot(&store), committed);
        assert_eq!(
            zero(|| store.retire_orphan_capacity(t.clone())).unwrap(),
            CapacityRetirementResult::Unproven
        );
        let r = record(&store, &t.owner.key);
        let late = SettleInput {
            key: t.owner.key.clone(),
            revision: r.revision,
            attempt: t.owner.attempt.clone(),
            evidence: DeliveryEvidence::MutationUnconfirmed,
            detail: String::new(),
            now: At::from_unix_millis(4000),
        };
        model(&store, DeliveryCommand::Settle(late.clone()));
        let settled = snapshot(&store);
        model(&store, DeliveryCommand::Settle(late));
        assert_eq!(snapshot(&store), settled);
        assert_eq!(store.active_delivery_owners().unwrap().len(), 15);
        assert_eq!(
            record(&store, &t.owner.key).attempts[0].capacity,
            CapacityState::Retired
        );
        assert!(
            store
                .inspect_delivery(&t.owner.key)
                .unwrap()
                .unwrap()
                .barrier
                .is_some()
        );
        // Settlement first releases only its own capacity; later retirement never decrements again.
        let store = FerroStore::in_memory();
        install(&store, &prepare(), &fixture_message());
        model(&store, fixture_commands().remove(0));
        model(&store, fixture_commands().remove(1));
        let t = target(&store);
        model(&store, fixture_commands().remove(2));
        close_fixture(&store, &t);
        let before = snapshot(&store);
        assert!(matches!(
            retire(&store, &t),
            CapacityRetirementResult::AlreadyReleased { .. }
        ));
        assert_eq!(snapshot(&store), before);
        // Independent native handles serialize a retirement against settlement without a second decrement.
        let raced_path = dir.path().join("retirement-race.fls");
        let first = FerroStore::open(&raced_path).unwrap();
        install(&first, &prepare(), &fixture_message());
        model(&first, fixture_commands().remove(0));
        model(&first, fixture_commands().remove(1));
        let t = target(&first);
        close_fixture(&first, &t);
        let second = FerroStore::open(&raced_path).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let settle_gate = barrier.clone();
        let settle = std::thread::spawn(move || {
            settle_gate.wait();
            model(&first, fixture_commands().remove(2))
        });
        let retire_target = t.clone();
        let retirement = std::thread::spawn(move || {
            barrier.wait();
            retire(&second, &retire_target)
        });
        settle.join().unwrap();
        retirement.join().unwrap();
        let raced = FerroStore::open(&raced_path).unwrap();
        assert!(raced.active_delivery_owners().unwrap().is_empty());
        let before = snapshot(&raced);
        assert!(matches!(
            retire(&raced, &t),
            CapacityRetirementResult::AlreadyReleased { .. }
        ));
        assert_eq!(snapshot(&raced), before);
    }

    #[test]
    fn managed_ferro_preparatory_inspection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inspect.fls");
        let key = prepare().key;
        let mut store = FerroStore::open(&path).unwrap();
        let before = snapshot(&store);
        assert_eq!(zero(|| store.inspect_delivery(&key)).unwrap(), None);
        assert!(zero(|| store.active_delivery_owners()).unwrap().is_empty());
        assert_eq!(snapshot(&store), before);
        store
            .with_write(|tx| {
                put(tx, LEDGER, "unrelated", &json!({"schema":1}));
                Ok(())
            })
            .unwrap();
        let before = snapshot(&store);
        for _ in 0..2 {
            assert!(matches!(
                zero(|| store.inspect_delivery(&key)),
                Err(StoreError::Decode(_))
            ));
            assert!(matches!(
                zero(|| store.active_delivery_owners()),
                Err(StoreError::Decode(_))
            ));
            public_refusals(&store);
            assert_eq!(snapshot(&store), before);
            drop(store);
            store = FerroStore::open(&path).unwrap();
        }
        install(&store, &prepare(), &fixture_message());
        assert_eq!(zero(|| store.inspect_delivery(&key)).unwrap(), None);
        model(&store, fixture_commands().remove(0));
        model(&store, fixture_commands().remove(1));
        let good = record(&store, &key);
        let view = store.inspect_delivery(&key).unwrap().unwrap();
        let rendered = serde_json::to_string(&view).unwrap();
        for forbidden in [
            "\"token\"",
            "\"frozen\"",
            "\"bytes\"",
            "\"worker\"",
            "\"connection\"",
            "\"lifetime\"",
        ] {
            assert!(!rendered.contains(forbidden));
        }
        for n in 0..12 {
            let mut r = good.clone();
            match n {
                0 => r.schema = 2,
                1 => r.attempts[0].detail = "x".repeat(9000),
                2 => r.attempts[0].detail = "🦀".repeat(301),
                3 => r.annotation = "x".repeat(4097),
                4 => r.attempts[0].frozen.as_mut().unwrap().bytes = vec![0; 1_048_576 + 15],
                5 => r.attempts[0].route.thread_id = "x".repeat(4097),
                6 => r.attempts[0].frozen.as_mut().unwrap().expected_turn = Some("x".repeat(4097)),
                7 => r.attempts[0].route.executable = Some("\0".repeat(2000)),
                8 => {
                    r.binding.authority_id = "\0".repeat(4096);
                    r.annotation = "\0".repeat(4096);
                }
                9 => r.prepares = 2,
                10 => r.attempts[0].frozen.as_mut().unwrap().renderer = "x".repeat(4097),
                _ => {
                    r.attempts[0].evidence = Some(DeliveryEvidence::AppServerAccepted {
                        thread: "x".repeat(4097),
                        turn: "turn".into(),
                    })
                }
            }
            store
                .with_write(|tx| {
                    put(tx, LEDGER, &ledger_key(&key), &r);
                    Ok(())
                })
                .unwrap();
            let before = snapshot(&store);
            for _ in 0..2 {
                assert!(
                    matches!(
                        zero(|| store.inspect_delivery(&key)),
                        Err(StoreError::Decode(_))
                    ),
                    "corrupt{n}"
                );
                assert!(
                    matches!(
                        zero(|| store.active_delivery_owners()),
                        Err(StoreError::Decode(_))
                    ),
                    "corrupt{n}"
                );
                public_refusals(&store);
                assert_eq!(snapshot(&store), before);
                drop(store);
                store = FerroStore::open(&path).unwrap();
            }
        }
        store
            .with_write(|tx| {
                put(tx, LEDGER, &ledger_key(&key), &good);
                Ok(())
            })
            .unwrap();
        assert_eq!(zero(|| store.inspect_delivery(&key)).unwrap(), Some(view));
        // Actual legal model-written maximum payload stays outside compact JSON accounting.
        let positive_path = dir.path().join("positive-bound.fls");
        for store in [
            FerroStore::in_memory(),
            FerroStore::open(&positive_path).unwrap(),
        ] {
            install(&store, &prepare(), &fixture_message());
            model(&store, fixture_commands().remove(0));
            model(
                &store,
                DeliveryCommand::Annotate {
                    key: key.clone(),
                    text: "\0".repeat(4096),
                },
            );
            let revision = record(&store, &key).revision;
            let DeliveryCommand::Arm(mut a) = arm_for(&prepare(), revision) else {
                unreachable!()
            };
            a.frozen.bytes = vec![0; 1_048_576 + 14];
            a.frozen.digest =
                canonical_payload(prepare().generation, &a.frozen.renderer, &a.frozen.bytes)
                    .unwrap();
            assert!(matches!(
                model(&store, DeliveryCommand::Arm(a)),
                DeliveryResult::Armed { .. }
            ));
            assert!(zero(|| store.inspect_delivery(&key)).unwrap().is_some());
            let mut settle = match fixture_commands().remove(2) {
                DeliveryCommand::Settle(s) => s,
                _ => unreachable!(),
            };
            settle.revision = record(&store, &key).revision;
            settle.detail = "🦀".repeat(300);
            model(&store, DeliveryCommand::Settle(settle));
            assert!(zero(|| store.inspect_delivery(&key)).unwrap().is_some());
            public_refusals(&store);
        }
        let reopened = FerroStore::open(&positive_path).unwrap();
        assert!(zero(|| reopened.inspect_delivery(&key)).unwrap().is_some());
        public_refusals(&reopened);
    }
    #[test]
    fn managed_ferro_preparatory_conformance_and_ack_provenance() {
        struct Model(Arc<FerroStore>);
        impl WireDeliveryStore for Model {
            fn apply_delivery(&self, c: DeliveryCommand) -> Result<DeliveryResult, StoreError> {
                Ok(model(&self.0, c))
            }
            fn inspect_delivery(
                &self,
                k: &DeliveryKey,
            ) -> Result<Option<DeliveryView>, StoreError> {
                self.0.inspect_delivery(k)
            }
            fn active_delivery_owners(&self) -> Result<Vec<CapacityRetirementTarget>, StoreError> {
                self.0.active_delivery_owners()
            }
            fn retire_orphan_capacity(
                &self,
                _: CapacityRetirementTarget,
            ) -> Result<CapacityRetirementResult, StoreError> {
                panic!("private retirement observation required")
            }
        }
        let s = Arc::new(FerroStore::in_memory());
        install(&s, &prepare(), &fixture_message());
        fl_core::wire_delivery::conformance::delivery_conformance(&Model(s), |m| {
            assert_eq!(m, &fixture_message())
        });
        let dir = tempfile::tempdir().unwrap();
        for legacy in [false, true] {
            let path = dir.path().join(format!("ack-{legacy}.fls"));
            let s = FerroStore::open(&path).unwrap();
            install(&s, &prepare(), &fixture_message());
            model(&s, fixture_commands().remove(0));
            model(&s, fixture_commands().remove(1));
            if legacy {
                s.with_write(|tx| {
                    let mut r = read_record(tx, &prepare().key)?.unwrap();
                    r.acknowledgment.legacy_unknown = true;
                    put(tx, LEDGER, &ledger_key(&r.key), &r);
                    let mut receipt: DeliveryReceipt = json(&tx.receipt(&prepare().key)?.unwrap())?;
                    receipt.status = DeliveryStatus::Acked;
                    put(
                        tx,
                        WIRE_INBOX,
                        &doc::pair(
                            prepare().key.recipient_id.as_str(),
                            prepare().key.original_id.as_str(),
                        ),
                        &receipt,
                    );
                    Ok(())
                })
                .unwrap();
            } else {
                model(&s, fixture_commands().remove(3));
            }
            let before = s.inspect_delivery(&prepare().key).unwrap().unwrap();
            assert_eq!(before.eligibility, Eligibility::MutationArmed);
            assert!(before.barrier.is_some());
            assert_eq!(before.attempts[0].capacity, CapacityState::Held);
            assert_eq!(before.acknowledgment.legacy_unknown, legacy);
            assert_eq!(before.acknowledgment.manual_at.is_some(), !legacy);
            assert!(matches!(
                model(&s, fixture_commands().remove(0)),
                DeliveryResult::Refused { .. }
            ));
            drop(s);
            let s = FerroStore::open(&path).unwrap();
            assert_eq!(s.inspect_delivery(&prepare().key).unwrap(), Some(before));
            public_refusals(&s);
        }
    }
    #[cfg(unix)]
    fn remote_fixture(
        tcp: bool,
    ) -> (
        tempfile::TempDir,
        super::super::FerroStoreServer,
        FerroStore,
        std::path::PathBuf,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.fls");
        let s = FerroStore::open(&path).unwrap();
        install(&s, &prepare(), &fixture_message());
        model(&s, fixture_commands().remove(0));
        let server = if tcp {
            super::super::FerroStoreServer::bind_tcp(
                "127.0.0.1:0",
                s,
                super::super::ServerAuth::new()
                    .with_psk("fixture", &[71; 32])
                    .unwrap(),
            )
            .unwrap()
        } else {
            super::super::FerroStoreServer::bind_unix(dir.path().join("test.sock"), s).unwrap()
        };
        let client = if tcp {
            FerroStore::connect_with(
                &format!("ws://{}", server.local_addr()),
                &super::super::ConnectOptions::psk("fixture", vec![71; 32]),
            )
            .unwrap()
        } else {
            FerroStore::connect_uri(&format!(
                "unix://{}",
                dir.path().join("test.sock").display()
            ))
            .unwrap()
        };
        (dir, server, client, path)
    }
    #[cfg(unix)]
    #[test]
    fn managed_ferro_preparatory_remote_probe() {
        for tcp in [false, true] {
            let (_dir, mut server, client, _path) = remote_fixture(tcp);
            let (result, rpc) = observe_preparatory_rpc(|| client.last_seq());
            assert!(result.is_ok());
            assert_eq!(rpc, 2);
            client.put_doc("ordinary", "key", &json!(7)).unwrap();
            assert_eq!(client.get_doc("ordinary", "key").unwrap(), Some(json!(7)));
            server.shutdown().unwrap();
        }
    }
    #[cfg(unix)]
    #[test]
    fn managed_any_store_preparatory_remote_refusal() {
        for tcp in [false, true] {
            let (dir, mut server, client, path) = remote_fixture(tcp);
            let reader = FerroStore::open(&path).unwrap();
            let before = snapshot(&reader);
            refusals(&client, true);
            #[cfg(target_os = "linux")]
            admissions(&client, true);
            let any = AnyStore::from(client);
            refusals(&any, true);
            #[cfg(target_os = "linux")]
            admissions(&any, true);
            assert_eq!(snapshot(&reader), before);
            server.shutdown().unwrap();
            refusals(&any, true);
            #[cfg(target_os = "linux")]
            admissions(&any, true);
            assert_eq!(snapshot(&reader), before);
            assert_eq!(
                std::fs::read_dir(dir.path())
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|e| e == "fls"))
                    .count(),
                1
            );
        }
    }
    #[test]
    fn managed_ferro_preparatory_native_io_faults() {
        use super::super::crash_sim::{IoOp, SimFs};
        use std::path::Path;
        for fail_sync in [false, true] {
            let fs = SimFs::default();
            let path = Path::new("/sim/managed.fls");
            let s = FerroStore::open_with(&fs, path).unwrap();
            install(&s, &prepare(), &fixture_message());
            let before = snapshot(&s);
            fs.fail_next(move |op| {
                if fail_sync {
                    matches!(op, IoOp::SyncData { .. })
                } else {
                    matches!(op, IoOp::Write { .. })
                }
            });
            assert!(
                s.with_write(|tx| apply_delivery_in(tx, fixture_commands().remove(0)))
                    .is_err()
            );
            drop(s);
            let s = FerroStore::open_with(&fs, path).unwrap();
            assert_eq!(snapshot(&s), before);
            public_refusals(&s);
        }
    }
    #[test]
    fn managed_ferro_preparatory_prerequisites_and_lifetime_gates() {
        for missing in [
            LEDGER,
            RESERVATIONS,
            CONTROLS,
            META,
            WIRE_INBOX,
            WIRE_RCPT_LOG,
            WIRE_MESSAGES,
            AGENTS,
        ] {
            let store = FerroStore::in_memory();
            install(&store, &prepare(), &fixture_message());
            store
                .with_write(|tx| {
                    for (key, _) in tx.scan(missing) {
                        tx.delete(missing, key);
                    }
                    Ok(())
                })
                .unwrap();
            let before = snapshot(&store);
            store
                .with_write(|tx| {
                    assert!(matches!(
                        apply_delivery_in(tx, fixture_commands().remove(0)),
                        Err(StoreError::Decode(_))
                    ));
                    Ok(())
                })
                .unwrap();
            assert_eq!(snapshot(&store), before);
            public_refusals(&store);
        }
        for variant in 0..3 {
            let store = FerroStore::in_memory();
            install(&store, &prepare(), &fixture_message());
            store
                .with_write(|tx| {
                    let mut r: LifetimeRegistryRow =
                        row(tx, CONTROLS, &lifetime_key(prepare().attempt.lifetime.id))?.unwrap();
                    match variant {
                        0 => r.state = LifetimeAdmissionState::Closing,
                        1 => r.generation.0 += 1,
                        _ => put_row(tx, CONTROLS, "live_admissions", Vec::<Digest>::new()),
                    };
                    put_row(tx, CONTROLS, &lifetime_key(r.registration.id), r);
                    Ok(())
                })
                .unwrap();
            let before = snapshot(&store);
            assert!(matches!(
                model(&store, fixture_commands().remove(0)),
                DeliveryResult::Refused { .. }
            ));
            assert_eq!(snapshot(&store), before);
        }
    }
    #[test]
    fn managed_ferro_preparatory_control_corruption() {
        for variant in 0..6 {
            let store = FerroStore::in_memory();
            install(&store, &prepare(), &fixture_message());
            model(&store, fixture_commands().remove(0));
            store
                .with_write(|tx| {
                    match variant {
                        0 => put(
                            tx,
                            CONTROLS,
                            "global",
                            &json!({"schema":2,"value":GlobalControl::default()}),
                        ),
                        1 => {
                            let mut g = controls(tx)?;
                            g.active_attempts += 1;
                            put_row(tx, CONTROLS, "global", g);
                        }
                        2 => {
                            let mut g = controls(tx)?;
                            g.active_index.push(g.active_index[0].clone());
                            g.active_attempts += 1;
                            put_row(tx, CONTROLS, "global", g);
                        }
                        3 => put_row(
                            tx,
                            CONTROLS,
                            "live_admissions",
                            vec![prepare().attempt.lifetime.id; 65],
                        ),
                        4 => {
                            let mut c: Continuity = required(row(tx, CONTROLS, "continuity")?)?;
                            c.schema = 2;
                            put_row(tx, CONTROLS, "continuity", c);
                        }
                        _ => {
                            let mut r = required(read_record(tx, &prepare().key)?)?;
                            r.revision.0 += 1;
                            r.prepares = 3;
                            put(tx, LEDGER, &ledger_key(&prepare().key), &r);
                        }
                    }
                    Ok(())
                })
                .unwrap();
            let before = snapshot(&store);
            assert!(
                matches!(
                    zero(|| store.inspect_delivery(&prepare().key)),
                    Err(StoreError::Decode(_))
                ),
                "variant {variant}"
            );
            assert!(
                matches!(
                    zero(|| store.active_delivery_owners()),
                    Err(StoreError::Decode(_))
                ),
                "variant {variant}"
            );
            public_refusals(&store);
            assert_eq!(snapshot(&store), before);
        }
    }
}
