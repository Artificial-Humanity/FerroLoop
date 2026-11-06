//! Disabled preparatory redb persistence. Private model transactions are not permits.
//!
//! ```compile_fail
//! use fl_store::wire_delivery::CapacityOnlyObservation;
//! ```
use crate::META;
use crate::ferrowire::WIRE_RCPT_LOG;
use crate::ferrowire::{WIRE_INBOX, WIRE_MESSAGES};
use crate::state::AGENTS;
use crate::{RedbStore, backend, decode};
use fl_core::StoreError;
use fl_core::ferrowire::DeliveryReceipt;
use fl_core::wire_delivery::*;
use redb::{ReadableTable, TableDefinition, TableHandle};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

type Def = TableDefinition<'static, &'static str, &'static str>;
const LEDGER: Def = TableDefinition::new("__wire_delivery_v1");
const RESERVATIONS: Def = TableDefinition::new("__wire_delivery_ids_v1");
const CONTROLS: Def = TableDefinition::new("__wire_delivery_control_v1");

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
impl Reader for redb::ReadTransaction {
    fn raw(&self, def: Def, key: &str) -> Result<Option<String>, StoreError> {
        note_io(0);
        let table = match self.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        Ok(table
            .get(key)
            .map_err(backend)?
            .map(|v| v.value().to_owned()))
    }
    fn receipt(&self, key: &DeliveryKey) -> Result<Option<String>, StoreError> {
        note_io(0);
        let table = match self.open_table(WIRE_INBOX) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        Ok(table
            .get((key.recipient_id.as_str(), key.original_id.as_str()))
            .map_err(backend)?
            .map(|v| v.value().to_owned()))
    }
}
impl Reader for redb::WriteTransaction {
    fn raw(&self, def: Def, key: &str) -> Result<Option<String>, StoreError> {
        note_io(0);
        if !self
            .list_tables()
            .map_err(backend)?
            .any(|t| t.name() == def.name())
        {
            return Ok(None);
        }
        Ok(self
            .open_table(def)
            .map_err(backend)?
            .get(key)
            .map_err(backend)?
            .map(|v| v.value().to_owned()))
    }
    fn receipt(&self, key: &DeliveryKey) -> Result<Option<String>, StoreError> {
        note_io(0);
        if !self
            .list_tables()
            .map_err(backend)?
            .any(|t| t.name() == WIRE_INBOX.name())
        {
            return Ok(None);
        }
        Ok(self
            .open_table(WIRE_INBOX)
            .map_err(backend)?
            .get((key.recipient_id.as_str(), key.original_id.as_str()))
            .map_err(backend)?
            .map(|v| v.value().to_owned()))
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
            || r.annotation.len() > 4096
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
fn has_tables(tx: &redb::ReadTransaction) -> Result<bool, StoreError> {
    let mut count = 0;
    for def in [LEDGER, RESERVATIONS, CONTROLS] {
        match tx.open_table(def) {
            Ok(_) => count += 1,
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(backend(e)),
        }
    }
    match count {
        0 => Ok(false),
        3 => Ok(true),
        _ => Err(decode("partial managed tables")),
    }
}
impl WireDeliveryStore for RedbStore {
    fn apply_delivery(&self, command: DeliveryCommand) -> Result<DeliveryResult, StoreError> {
        Ok(DeliveryResult::Refused {
            reason: if matches!(
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
        let tx = self.db.begin_read().map_err(backend)?;
        if !has_tables(&tx)? {
            return Ok(None);
        }
        let g = controls(&tx)?;
        Ok(checked_record(&tx, key, &g)?.map(|(record, recipient)| record.view(&recipient)))
    }
    fn active_delivery_owners(&self) -> Result<Vec<CapacityRetirementTarget>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        if !has_tables(&tx)? {
            return Ok(Vec::new());
        }
        let global = controls(&tx)?;
        global
            .active_index
            .iter()
            .map(|owner| {
                let (record, recipient) = required(checked_record(&tx, &owner.key, &global)?)?;
                Ok(CapacityRetirementTarget {
                    owner: owner.clone(),
                    record_revision: record.revision,
                    recipient_revision: recipient.revision,
                    global_revision: global.revision,
                })
            })
            .collect()
    }
    fn retire_orphan_capacity(
        &self,
        _target: CapacityRetirementTarget,
    ) -> Result<CapacityRetirementResult, StoreError> {
        Ok(CapacityRetirementResult::Unproven)
    }
}

// Prepared kernels deliberately have no production caller until real scope producers
// and the separately reviewed mutation/enrollment fences exist.
#[allow(dead_code)]
fn context(
    tx: &redb::WriteTransaction,
    command: &DeliveryCommand,
) -> Result<TransitionContext, StoreError> {
    let names = tx
        .list_tables()
        .map_err(backend)?
        .map(|t| t.name().to_owned())
        .collect::<Vec<_>>();
    if [
        LEDGER.name(),
        RESERVATIONS.name(),
        CONTROLS.name(),
        META.name(),
        WIRE_INBOX.name(),
        WIRE_RCPT_LOG.name(),
        WIRE_MESSAGES.name(),
        AGENTS.name(),
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
fn write<T: Serialize>(
    tx: &redb::WriteTransaction,
    table: Def,
    key: &str,
    value: &T,
) -> Result<(), StoreError> {
    let raw = serde_json::to_string(value).map_err(backend)?;
    tx.open_table(table)
        .map_err(backend)?
        .insert(key, raw.as_str())
        .map_err(backend)?;
    note_io(2);
    Ok(())
}
fn write_row<T: Serialize>(
    tx: &redb::WriteTransaction,
    table: Def,
    key: &str,
    value: T,
) -> Result<(), StoreError> {
    write(tx, table, key, &Row { schema: 1, value })
}
// This private transaction entry point is executable only by owning-module tests.
#[allow(dead_code)]
fn apply_delivery_in(
    tx: &redb::WriteTransaction,
    command: DeliveryCommand,
) -> Result<DeliveryResult, StoreError> {
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
        let raw = serde_json::to_string(&receipt).map_err(backend)?;
        tx.open_table(WIRE_INBOX)
            .map_err(backend)?
            .insert(
                (receipt.recipient_id.as_str(), receipt.message_id.as_str()),
                raw.as_str(),
            )
            .map_err(backend)?;
        crate::ferrowire::append_wire_receipt_in(tx, &receipt)?;
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
    tx: &redb::WriteTransaction,
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
    use crate::{FORMAT_KEY, observe_preparatory_writes};
    use fl_core::agent::{AgentRecord, AgentRole, AgentStatus};
    use fl_core::ferrowire::{DeliveryStatus, WireAddress, WireMessage};
    use fl_core::wire_delivery::conformance::{fixture_commands, fixture_message};
    use fl_core::{AgentId, At, MessageId};
    use std::sync::{Arc, Barrier};

    fn prepare() -> PrepareInput {
        let DeliveryCommand::Prepare(input) = fixture_commands().remove(0) else {
            unreachable!()
        };
        input
    }
    fn put<T: Serialize>(tx: &redb::WriteTransaction, def: Def, key: &str, value: &T) {
        let raw = serde_json::to_string(value).unwrap();
        tx.open_table(def)
            .unwrap()
            .insert(key, raw.as_str())
            .unwrap();
    }
    fn put_row<T: Serialize>(tx: &redb::WriteTransaction, def: Def, key: &str, value: T) {
        put(tx, def, key, &Row { schema: 1, value })
    }
    fn install(store: &RedbStore, input: &PrepareInput, message: &WireMessage) {
        let tx = store.db.begin_write().unwrap();
        tx.open_table(LEDGER).unwrap();
        tx.open_table(RESERVATIONS).unwrap();
        tx.open_table(CONTROLS).unwrap();
        tx.open_table(WIRE_RCPT_LOG).unwrap();
        if row::<GlobalControl>(&tx, CONTROLS, "global")
            .unwrap()
            .is_none()
        {
            put_row(&tx, CONTROLS, "global", GlobalControl::default());
        }
        if row::<Continuity>(&tx, CONTROLS, "continuity")
            .unwrap()
            .is_none()
        {
            put_row(
                &tx,
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
        if row::<RecipientControl>(&tx, CONTROLS, &recipient_key(&input.key))
            .unwrap()
            .is_none()
        {
            put_row(
                &tx,
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
        let mut admissions: Vec<Digest> = row(&tx, CONTROLS, "live_admissions")
            .unwrap()
            .unwrap_or_default();
        if !admissions.contains(&input.attempt.lifetime.id) {
            admissions.push(input.attempt.lifetime.id);
        }
        put_row(&tx, CONTROLS, "live_admissions", admissions);
        put_row(
            &tx,
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
        put(&tx, AGENTS, agent.id.as_str(), &agent);
        put(&tx, WIRE_MESSAGES, message.id.as_str(), message);
        let receipt = DeliveryReceipt {
            message_id: message.id.clone(),
            recipient_id: input.key.recipient_id.iri().clone(),
            recipient_name: name.clone(),
            status: DeliveryStatus::Pending,
            attempts: 0,
            updated_at: message.created_at.clone(),
            acked_at: None,
        };
        let raw = serde_json::to_string(&receipt).unwrap();
        tx.open_table(WIRE_INBOX)
            .unwrap()
            .insert(
                (input.key.recipient_id.as_str(), message.id.as_str()),
                raw.as_str(),
            )
            .unwrap();
        for key in [
            serde_json::to_string(&message.to).unwrap(),
            format!("name:{name}"),
        ] {
            put_row(
                &tx,
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
        tx.commit().unwrap();
    }
    fn snapshot(store: &RedbStore) -> serde_json::Value {
        let tx = store.db.begin_read().unwrap();
        let names: Vec<_> = tx
            .list_tables()
            .unwrap()
            .map(|t| t.name().to_owned())
            .collect();
        let mut rows = serde_json::Map::new();
        for def in [LEDGER, RESERVATIONS, CONTROLS, WIRE_MESSAGES, AGENTS] {
            if let Ok(t) = tx.open_table(def) {
                rows.insert(
                    def.name().into(),
                    serde_json::json!(
                        t.iter()
                            .unwrap()
                            .map(|r| {
                                let (k, v) = r.unwrap();
                                (k.value().to_owned(), v.value().to_owned())
                            })
                            .collect::<Vec<_>>()
                    ),
                );
            }
        }
        let receipts = tx.open_table(WIRE_INBOX).ok().map(|t| {
            t.iter()
                .unwrap()
                .map(|r| {
                    let (k, v) = r.unwrap();
                    let (a, b) = k.value();
                    (a.to_owned(), b.to_owned(), v.value().to_owned())
                })
                .collect::<Vec<_>>()
        });
        let log = tx.open_table(WIRE_RCPT_LOG).ok().map(|t| {
            t.iter()
                .unwrap()
                .map(|r| {
                    let (k, v) = r.unwrap();
                    (k.value(), v.value().to_owned())
                })
                .collect::<Vec<_>>()
        });
        let meta = tx
            .open_table(META)
            .unwrap()
            .iter()
            .unwrap()
            .map(|r| {
                let (k, v) = r.unwrap();
                (k.value().to_owned(), v.value())
            })
            .collect::<Vec<_>>();
        serde_json::json!({"tables":names,"rows":rows,"receipts":receipts,"log":log,"meta":meta})
    }
    fn zero<T>(call: impl FnOnce() -> T) -> T {
        let (result, writes) = observe_preparatory_writes(call);
        assert_eq!(writes, 0);
        result
    }
    fn public_refusals(store: &RedbStore) {
        let p = prepare();
        let mut commands = fixture_commands();
        commands.extend([
            DeliveryCommand::Release(ReleaseInput {
                key: p.key.clone(),
                revision: Revision(1),
                attempt: p.attempt.clone(),
                proof: DeliveryEvidence::PreMutationRefused,
                now: p.now.clone(),
            }),
            DeliveryCommand::InvalidatePrepared {
                key: p.key.clone(),
                revision: Revision(1),
                now: p.now.clone(),
            },
            DeliveryCommand::Annotate {
                key: p.key.clone(),
                text: "not an authority".into(),
            },
            DeliveryCommand::Close { key: p.key.clone() },
        ]);
        let before = snapshot(store);
        let _ = zero(|| store.inspect_delivery(&p.key));
        let _ = zero(|| store.active_delivery_owners());
        for command in commands {
            let forged: DeliveryCommand =
                serde_json::from_value(serde_json::to_value(&command).unwrap()).unwrap();
            let expected = if matches!(
                command,
                DeliveryCommand::Prepare(_) | DeliveryCommand::Arm(_)
            ) {
                HoldReason::UnsupportedBackend
            } else {
                HoldReason::Disabled
            };
            assert_eq!(
                zero(|| store.apply_delivery(forged)).unwrap(),
                DeliveryResult::Refused { reason: expected }
            );
        }
        let target = CapacityRetirementTarget {
            owner: ActiveOwner {
                key: p.key.clone(),
                attempt: p.attempt.clone(),
            },
            record_revision: Revision(1),
            recipient_revision: Revision(2),
            global_revision: Revision(1),
        };
        assert_eq!(
            zero(|| store.retire_orphan_capacity(target)).unwrap(),
            CapacityRetirementResult::Unproven
        );
        #[cfg(target_os = "linux")]
        {
            use crate::wire_delivery_lifetime::{
                LocalAdmissionError, LocalLifetimeAdmissionStore, assert_preparatory_admit_refusal,
            };
            assert_preparatory_admit_refusal(store, false);
            assert!(matches!(
                zero(|| store.close_local_lifetime(&p.attempt.lifetime)),
                Err(LocalAdmissionError::Unproven)
            ));
        }
        assert_eq!(snapshot(store), before);
    }
    #[test]
    fn managed_redb_preparatory_public_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("public.redb");
        let store = RedbStore::open(&path).unwrap();
        public_refusals(&store);
        drop(store);
        let store = RedbStore::open(&path).unwrap();
        public_refusals(&store);
        install(&store, &prepare(), &fixture_message());
        public_refusals(&store);
        assert!(matches!(
            model(&store, fixture_commands().remove(0)),
            DeliveryResult::Prepared { .. }
        ));
        public_refusals(&store);
        drop(store);
        let store = RedbStore::open(&path).unwrap();
        public_refusals(&store);
        let ro = store.inspect_delivery(&prepare().key).unwrap();
        assert_eq!(zero(|| store.inspect_delivery(&prepare().key)).unwrap(), ro);
    }
    #[test]
    fn managed_preparatory_fixture_boundary() {
        managed_redb_preparatory_public_refusal();
    }
    #[test]
    fn managed_redb_preparatory_inspection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inspect.redb");
        let store = RedbStore::open(&path).unwrap();
        let key = prepare().key;
        let before = snapshot(&store);
        assert_eq!(zero(|| store.inspect_delivery(&key)).unwrap(), None);
        assert!(zero(|| store.active_delivery_owners()).unwrap().is_empty());
        assert_eq!(snapshot(&store), before);
        let tx = store.db.begin_write().unwrap();
        tx.open_table(LEDGER).unwrap();
        tx.commit().unwrap();
        let before = snapshot(&store);
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
        let store = RedbStore::open(&path).unwrap();
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
        install(&store, &prepare(), &fixture_message());
        let tx = store.db.begin_write().unwrap();
        tx.open_table(CONTROLS)
            .unwrap()
            .insert("global", "not json")
            .unwrap();
        tx.commit().unwrap();
        let before = snapshot(&store);
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
        let store = RedbStore::open(&path).unwrap();
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
        let tx = store.db.begin_write().unwrap();
        put_row(&tx, CONTROLS, "global", GlobalControl::default());
        tx.commit().unwrap();
        model(&store, fixture_commands().remove(0));
        model(&store, fixture_commands().remove(1));
        let before = snapshot(&store);
        let view = zero(|| store.inspect_delivery(&key)).unwrap().unwrap();
        let raw = serde_json::to_string(&view).unwrap();
        for forbidden in [
            "\"token\"",
            "\"frozen\"",
            "\"bytes\"",
            "\"worker\"",
            "\"connection\"",
            "\"lifetime\"",
        ] {
            assert!(!raw.contains(forbidden));
        }
        assert_eq!(zero(|| store.active_delivery_owners()).unwrap().len(), 1);
        assert_eq!(snapshot(&store), before);
        drop(store);
        let mut store = RedbStore::open(&path).unwrap();
        assert_eq!(zero(|| store.inspect_delivery(&key)).unwrap(), Some(view));
        public_refusals(&store);
        // A missing reservation, unknown strict row schema, and inconsistent active count all fail closed.
        let tx = store.db.begin_read().unwrap();
        let good_global: GlobalControl = global(&tx).unwrap();
        let good_continuity: Continuity = row(&tx, CONTROLS, "continuity").unwrap().unwrap();
        let good_reservation: IdReservation =
            row(&tx, RESERVATIONS, &hex(&lp(key.original_id.as_str())))
                .unwrap()
                .unwrap();
        drop(tx);
        for bad in ["reservation", "schema", "index"] {
            let tx = store.db.begin_write().unwrap();
            put_row(&tx, CONTROLS, "global", good_global.clone());
            put_row(&tx, CONTROLS, "continuity", good_continuity.clone());
            put_row(
                &tx,
                RESERVATIONS,
                &hex(&lp(key.original_id.as_str())),
                good_reservation.clone(),
            );
            match bad {
                "reservation" => {
                    tx.open_table(RESERVATIONS)
                        .unwrap()
                        .remove(hex(&lp(key.original_id.as_str())).as_str())
                        .unwrap();
                }
                "schema" => {
                    put(
                        &tx,
                        CONTROLS,
                        "continuity",
                        &serde_json::json!({"schema":2,"value":{}}),
                    );
                }
                _ => {
                    put_row(
                        &tx,
                        CONTROLS,
                        "global",
                        GlobalControl {
                            active_attempts: 17,
                            ..Default::default()
                        },
                    );
                }
            }
            tx.commit().unwrap();
            let before = snapshot(&store);
            assert!(matches!(
                zero(|| store.inspect_delivery(&key)),
                Err(StoreError::Decode(_))
            ));
            assert!(matches!(
                zero(|| store.active_delivery_owners()),
                Err(StoreError::Decode(_))
            ));
            assert_eq!(snapshot(&store), before);
            drop(store);
            store = RedbStore::open(&path).unwrap();
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
        }
        let tx = store.db.begin_write().unwrap();
        tx.open_table(META)
            .unwrap()
            .insert(FORMAT_KEY, 999)
            .unwrap();
        tx.commit().unwrap();
        drop(store);
        assert!(matches!(
            RedbStore::open(&path),
            Err(StoreError::FormatVersion { .. })
        ));
    }
    #[derive(Clone)]
    struct ModelHandle(Arc<RedbStore>);
    impl WireDeliveryStore for ModelHandle {
        fn apply_delivery(&self, c: DeliveryCommand) -> Result<DeliveryResult, StoreError> {
            let tx = self.0.db.begin_write().map_err(backend)?;
            let result = apply_delivery_in(&tx, c)?;
            tx.commit().map_err(backend)?;
            Ok(result)
        }
        fn inspect_delivery(&self, k: &DeliveryKey) -> Result<Option<DeliveryView>, StoreError> {
            self.0.inspect_delivery(k)
        }
        fn active_delivery_owners(&self) -> Result<Vec<CapacityRetirementTarget>, StoreError> {
            self.0.active_delivery_owners()
        }
        fn retire_orphan_capacity(
            &self,
            _: CapacityRetirementTarget,
        ) -> Result<CapacityRetirementResult, StoreError> {
            panic!("model retirement needs owning observation")
        }
    }
    fn model(store: &RedbStore, c: DeliveryCommand) -> DeliveryResult {
        let tx = store.db.begin_write().unwrap();
        let result = apply_delivery_in(&tx, c).unwrap();
        tx.commit().unwrap();
        result
    }
    fn armed(store: &RedbStore) -> CapacityRetirementTarget {
        model(store, fixture_commands().remove(0));
        model(store, fixture_commands().remove(1));
        store.active_delivery_owners().unwrap().remove(0)
    }
    fn retire(store: &RedbStore, target: &CapacityRetirementTarget) -> CapacityRetirementResult {
        let tx = store.db.begin_write().unwrap();
        let registry: LifetimeRegistryRow = row(
            &tx,
            CONTROLS,
            &lifetime_key(target.owner.attempt.lifetime.id),
        )
        .unwrap()
        .unwrap();
        let observation = CapacityOnlyObservation {
            target: target.clone(),
            registry_generation: registry.generation,
        };
        let result = retire_capacity_in(&tx, target, observation).unwrap();
        tx.commit().unwrap();
        result
    }
    fn close_fixture(store: &RedbStore, target: &CapacityRetirementTarget) {
        let tx = store.db.begin_write().unwrap();
        let mut registry: LifetimeRegistryRow = row(
            &tx,
            CONTROLS,
            &lifetime_key(target.owner.attempt.lifetime.id),
        )
        .unwrap()
        .unwrap();
        registry.state = LifetimeAdmissionState::Closed;
        put_row(
            &tx,
            CONTROLS,
            &lifetime_key(registry.registration.id),
            registry,
        );
        tx.commit().unwrap();
    }
    #[test]
    fn managed_redb_preparatory_write_probe() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("probe.redb")).unwrap();
        let before = snapshot(&store);
        let (_, attempts) = observe_preparatory_writes(|| drop(store.db.begin_write().unwrap()));
        assert!(attempts > 0);
        assert_eq!(snapshot(&store), before);
        install(&store, &prepare(), &fixture_message());
        let (result, attempts) =
            observe_preparatory_writes(|| model(&store, fixture_commands().remove(0)));
        assert!(attempts > 0);
        assert!(matches!(result, DeliveryResult::Prepared { .. }));
        // Unwinding always removes the observation, and refused read-only writes count too.
        assert!(
            std::panic::catch_unwind(|| observe_preparatory_writes(|| panic!("probe"))).is_err()
        );
        assert_eq!(observe_preparatory_writes(|| ()).1, 0);
    }
    fn next_input(n: u64, recipient: u64) -> (PrepareInput, WireMessage) {
        let mut p = prepare();
        let mut message = fixture_message();
        message.id = MessageId(fl_core::ids::seq_iri(10000 + n));
        message.thread_id = message.id.clone();
        message.to = WireAddress::AgentName(format!("bob-{recipient}"));
        p.key.original_id = message.id.clone();
        p.key.recipient_id = AgentId(fl_core::ids::seq_iri(20000 + recipient));
        p.binding.recipient_id = p.key.recipient_id.clone();
        p.generation = canonical_generation(&message).unwrap();
        p.attempt.token.0[..8].copy_from_slice(&n.to_be_bytes());
        (p, message)
    }
    fn arm_for(p: &PrepareInput, revision: Revision) -> DeliveryCommand {
        let mut arm = match fixture_commands().remove(1) {
            DeliveryCommand::Arm(a) => a,
            _ => unreachable!(),
        };
        arm.key = p.key.clone();
        arm.attempt = p.attempt.clone();
        arm.route = p.route.clone();
        arm.revision = revision;
        arm.frozen.digest =
            canonical_payload(p.generation, &arm.frozen.renderer, &arm.frozen.bytes).unwrap();
        DeliveryCommand::Arm(arm)
    }
    fn race(store: Arc<RedbStore>, commands: [DeliveryCommand; 2]) -> Vec<DeliveryResult> {
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = commands
            .into_iter()
            .map(|command| {
                let handle = ModelHandle(store.clone());
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    handle.apply_delivery(command).unwrap()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    }
    #[test]
    fn managed_redb_preparatory_cas_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cas.redb");
        let store = Arc::new(RedbStore::open(&path).unwrap());
        install(&store, &prepare(), &fixture_message());
        let results = race(
            store.clone(),
            [fixture_commands().remove(0), fixture_commands().remove(0)],
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, DeliveryResult::Prepared { .. }))
                .count(),
            1
        );
        assert_eq!(
            store
                .inspect_delivery(&prepare().key)
                .unwrap()
                .unwrap()
                .prepares,
            1
        );
        let before = snapshot(&store);
        let tx = store.db.begin_write().unwrap();
        let result = apply_delivery_in(&tx, fixture_commands().remove(1)).unwrap();
        assert!(matches!(result, DeliveryResult::Armed { .. }));
        drop(tx);
        assert_eq!(snapshot(&store), before); // injected pre-commit failure
        model(&store, fixture_commands().remove(1));
        let armed_snapshot = snapshot(&store);
        let target = store.active_delivery_owners().unwrap().remove(0);
        assert_eq!(target.record_revision, Revision(2));
        let tx = store.db.begin_write().unwrap();
        let result = apply_delivery_in(&tx, fixture_commands().remove(2)).unwrap();
        assert!(matches!(result, DeliveryResult::Applied { .. }));
        drop(tx);
        assert_eq!(snapshot(&store), armed_snapshot);
        assert_eq!(
            store
                .inspect_delivery(&prepare().key)
                .unwrap()
                .unwrap()
                .eligibility,
            Eligibility::MutationArmed
        );
        model(&store, fixture_commands().remove(2)); // committed before a hypothetical lost reply
        let committed = snapshot(&store);
        drop(store);
        let store = Arc::new(RedbStore::open(&path).unwrap());
        assert_eq!(snapshot(&store), committed);
        model(&store, fixture_commands().remove(2));
        assert_eq!(snapshot(&store), committed); // duplicate settlement, zero delta
        model(&store, fixture_commands().remove(3));
        let view = store.inspect_delivery(&prepare().key).unwrap().unwrap();
        assert!(view.acknowledgment.manual_at.is_some());
        assert_eq!(
            view.attempts[0].evidence,
            Some(DeliveryEvidence::VendorQueued)
        );
        // Actual point IO remains fixed as unrelated retained history grows; zero delta writes on refusal.
        let refused = fixture_commands().remove(0);
        IO.set((0, 0, 0));
        model(&store, refused.clone());
        let baseline = IO.get();
        assert!(baseline.0 > 0 && baseline.1 > 0);
        assert_eq!(baseline.2, 0);
        let annotate = DeliveryCommand::Annotate {
            key: prepare().key,
            text: String::new(),
        };
        IO.set((0, 0, 0));
        model(&store, annotate.clone());
        let changed_baseline = IO.get();
        assert_eq!(changed_baseline.2, 1);
        let tx = store.db.begin_write().unwrap();
        for n in 0..200 {
            let (p, m) = next_input(n, n);
            put(&tx, WIRE_MESSAGES, m.id.as_str(), &m);
            put(
                &tx,
                LEDGER,
                &ledger_key(&p.key),
                &serde_json::json!({"unrelated":"retained history"}),
            );
        }
        tx.commit().unwrap();
        IO.set((0, 0, 0));
        model(&store, refused);
        assert_eq!(IO.get(), baseline);
        IO.set((0, 0, 0));
        model(&store, annotate);
        assert_eq!(IO.get(), changed_baseline);
        public_refusals(&store);
        // Global final-slot race: fifteen occupied recipients, two distinct next recipients, one winner.
        let cap = Arc::new(RedbStore::open(&dir.path().join("capacity.redb")).unwrap());
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
            cap.clone(),
            [
                DeliveryCommand::Prepare(a.clone()),
                DeliveryCommand::Prepare(b.clone()),
            ],
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, DeliveryResult::Prepared { .. }))
                .count(),
            1
        );
        assert_eq!(cap.active_delivery_owners().unwrap().len(), 16);
        for (p, result) in [a, b].iter().zip(results) {
            if matches!(result, DeliveryResult::Refused { .. }) {
                assert!(cap.inspect_delivery(&p.key).unwrap().is_none());
            }
        }
        // Different original IDs competing for the same recipient slot.
        let single = Arc::new(RedbStore::open(&dir.path().join("recipient.redb")).unwrap());
        let (a, ma) = next_input(200, 7);
        let (b, mb) = next_input(201, 7);
        install(&single, &a, &ma);
        install(&single, &b, &mb);
        let results = race(
            single.clone(),
            [DeliveryCommand::Prepare(a), DeliveryCommand::Prepare(b)],
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, DeliveryResult::Prepared { .. }))
                .count(),
            1
        );
        assert_eq!(single.active_delivery_owners().unwrap().len(), 1);
        // Three lifetime prepares and escaped row budget survive native serialization.
        let bounded = RedbStore::open(&dir.path().join("bounded.redb")).unwrap();
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
            model(&bounded, DeliveryCommand::Prepare(p.clone())),
            DeliveryResult::Refused {
                reason: HoldReason::BudgetExhausted
            }
        );
        model(
            &bounded,
            DeliveryCommand::Annotate {
                key: p.key.clone(),
                text: "\0".repeat(4096),
            },
        );
        let view = bounded.inspect_delivery(&p.key).unwrap().unwrap();
        assert_eq!(view.prepares, 3);
        assert!(serde_json::to_vec(&view).unwrap().len() < 32768);
    }
    #[test]
    fn managed_redb_preparatory_conformance_and_ack_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RedbStore::open(&dir.path().join("conformance.redb")).unwrap());
        install(&store, &prepare(), &fixture_message());
        fl_core::wire_delivery::conformance::delivery_conformance(
            &ModelHandle(store.clone()),
            |message| assert_eq!(message, &fixture_message()),
        );
        for legacy in [false, true] {
            let path = dir.path().join(format!("ack-{legacy}.redb"));
            let store = RedbStore::open(&path).unwrap();
            install(&store, &prepare(), &fixture_message());
            armed(&store);
            if legacy {
                // Model a legacy writer's receipt and explicit unknown-provenance suppression row.
                let tx = store.db.begin_write().unwrap();
                let mut record = read_record(&tx, &prepare().key).unwrap().unwrap();
                record.acknowledgment.legacy_unknown = true;
                put(&tx, LEDGER, &ledger_key(&record.key), &record);
                let mut receipt: DeliveryReceipt =
                    json(&tx.receipt(&prepare().key).unwrap().unwrap()).unwrap();
                receipt.status = DeliveryStatus::Acked;
                let raw = serde_json::to_string(&receipt).unwrap();
                tx.open_table(WIRE_INBOX)
                    .unwrap()
                    .insert(
                        (
                            prepare().key.recipient_id.as_str(),
                            prepare().key.original_id.as_str(),
                        ),
                        raw.as_str(),
                    )
                    .unwrap();
                tx.commit().unwrap();
            } else {
                model(&store, fixture_commands().remove(3));
            }
            let before = store.inspect_delivery(&prepare().key).unwrap().unwrap();
            assert_eq!(before.eligibility, Eligibility::MutationArmed);
            assert!(before.barrier.is_some());
            assert_eq!(before.attempts[0].capacity, CapacityState::Held);
            // Both manual and legacy ACK suppress output eligibility without inventing evidence or draining Armed capacity.
            assert_eq!(before.acknowledgment.legacy_unknown, legacy);
            assert_eq!(before.acknowledgment.manual_at.is_some(), !legacy);
            let result = model(&store, DeliveryCommand::Prepare(prepare()));
            assert!(matches!(result, DeliveryResult::Refused { .. }));
            assert_eq!(
                store.inspect_delivery(&prepare().key).unwrap(),
                Some(before.clone())
            );
            public_refusals(&store);
            drop(store);
            let store = RedbStore::open(&path).unwrap();
            assert_eq!(
                store.inspect_delivery(&prepare().key).unwrap(),
                Some(before)
            );
            public_refusals(&store);
        }
    }
    #[test]
    fn managed_redb_preparatory_kernel_never_enrolls_tables() {
        let dir = tempfile::tempdir().unwrap();
        for missing in [
            "ledger",
            "reservation",
            "controls",
            "meta",
            "inbox",
            "log",
            "message",
            "agent",
        ] {
            let store =
                RedbStore::open(&dir.path().join(format!("no-enroll-{missing}.redb"))).unwrap();
            install(&store, &prepare(), &fixture_message());
            let tx = store.db.begin_write().unwrap();
            match missing {
                "ledger" => tx.delete_table(LEDGER),
                "reservation" => tx.delete_table(RESERVATIONS),
                "controls" => tx.delete_table(CONTROLS),
                "meta" => tx.delete_table(META),
                "inbox" => tx.delete_table(WIRE_INBOX),
                "log" => tx.delete_table(WIRE_RCPT_LOG),
                "message" => tx.delete_table(WIRE_MESSAGES),
                _ => tx.delete_table(AGENTS),
            }
            .unwrap();
            tx.commit().unwrap();
            let tx = store.db.begin_read().unwrap();
            let before = tx
                .list_tables()
                .unwrap()
                .map(|t| t.name().to_owned())
                .collect::<Vec<_>>();
            let record_before = tx.open_table(LEDGER).ok().and_then(|t| {
                t.get(ledger_key(&prepare().key).as_str())
                    .unwrap()
                    .map(|r| r.value().to_owned())
            });
            drop(tx);
            let tx = store.db.begin_write().unwrap();
            assert!(matches!(
                apply_delivery_in(&tx, fixture_commands().remove(0)),
                Err(StoreError::Decode(_))
            ));
            // Committing the failed call still creates no tables or partial record/control delta.
            tx.commit().unwrap();
            let tx = store.db.begin_read().unwrap();
            assert_eq!(
                tx.list_tables()
                    .unwrap()
                    .map(|t| t.name().to_owned())
                    .collect::<Vec<_>>(),
                before
            );
            assert_eq!(
                tx.open_table(LEDGER).ok().and_then(|t| t
                    .get(ledger_key(&prepare().key).as_str())
                    .unwrap()
                    .map(|r| r.value().to_owned())),
                record_before
            );
        }
    }

    #[test]
    fn managed_redb_preparatory_registry_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("registry.redb")).unwrap();
        install(&store, &prepare(), &fixture_message());
        let id = prepare().attempt.lifetime.id;
        for case in ["closing", "closed", "generation", "binding", "live"] {
            let tx = store.db.begin_write().unwrap();
            let mut registry = LifetimeRegistryRow {
                registration: prepare().attempt.lifetime,
                state: LifetimeAdmissionState::Open,
                generation: Revision(1),
            };
            let mut admissions = vec![id];
            match case {
                "closing" => registry.state = LifetimeAdmissionState::Closing,
                "closed" => registry.state = LifetimeAdmissionState::Closed,
                "generation" => registry.generation = Revision(2),
                "binding" => registry.registration.worker = WorkerId([44; 32]),
                _ => admissions.clear(),
            };
            put_row(&tx, CONTROLS, &lifetime_key(id), registry);
            put_row(&tx, CONTROLS, "live_admissions", admissions);
            tx.commit().unwrap();
            let before = snapshot(&store);
            assert_eq!(
                model(&store, DeliveryCommand::Prepare(prepare())),
                DeliveryResult::Refused {
                    reason: HoldReason::UnsupportedBackend
                }
            );
            assert_eq!(snapshot(&store), before);
            public_refusals(&store);
        }
    }
    #[test]
    fn managed_redb_preparatory_retirement_model() {
        let dir = tempfile::tempdir().unwrap();
        for retirement_first in [true, false] {
            let path = dir.path().join(format!("retire-{retirement_first}.redb"));
            let store = RedbStore::open(&path).unwrap();
            install(&store, &prepare(), &fixture_message());
            let target = armed(&store);
            let before = store.inspect_delivery(&prepare().key).unwrap().unwrap();
            assert_eq!(retire(&store, &target), CapacityRetirementResult::Unproven); // scope still Open
            close_fixture(&store, &target);
            for mismatch in 0..6 {
                let tx = store.db.begin_write().unwrap();
                let mut wrong = target.clone();
                match mismatch {
                    0 => wrong.record_revision.0 += 1,
                    1 => wrong.recipient_revision.0 += 1,
                    2 => wrong.global_revision.0 += 1,
                    3 => wrong.owner.attempt.worker = WorkerId([77; 32]),
                    4 => wrong.owner.attempt.lifetime.id = Digest([88; 32]),
                    _ => {}
                };
                let observation = CapacityOnlyObservation {
                    target: wrong.clone(),
                    registry_generation: Revision(if mismatch == 5 { 99 } else { 1 }),
                };
                let result = retire_capacity_in(&tx, &wrong, observation).unwrap();
                assert!(matches!(
                    result,
                    CapacityRetirementResult::Stale | CapacityRetirementResult::Unproven
                ));
                drop(tx);
            }
            let baseline = snapshot(&store);
            let tx = store.db.begin_write().unwrap();
            let result = retire_capacity_in(
                &tx,
                &target,
                CapacityOnlyObservation {
                    target: target.clone(),
                    registry_generation: Revision(1),
                },
            )
            .unwrap();
            assert!(matches!(result, CapacityRetirementResult::Retired { .. }));
            drop(tx);
            assert_eq!(snapshot(&store), baseline);
            if retirement_first {
                assert!(matches!(
                    retire(&store, &target),
                    CapacityRetirementResult::Retired { .. }
                ));
                let retired = store.inspect_delivery(&prepare().key).unwrap().unwrap();
                assert_eq!(retired.attempts[0].capacity, CapacityState::Retired);
                assert_eq!(retired.barrier, before.barrier);
                assert_eq!(retired.prepares, before.prepares);
                assert_eq!(retired.acknowledgment, before.acknowledgment);
                assert_eq!(retired.attempts[0].evidence, None);
                let snap = snapshot(&store);
                assert!(matches!(
                    retire(&store, &target),
                    CapacityRetirementResult::AlreadyReleased { .. }
                ));
                assert_eq!(snapshot(&store), snap);
                let mut settle = fixture_commands().remove(2);
                if let DeliveryCommand::Settle(s) = &mut settle {
                    s.revision = retired.revision;
                }
                model(&store, settle);
                let late = store.inspect_delivery(&prepare().key).unwrap().unwrap();
                assert_eq!(late.attempts[0].capacity, CapacityState::Retired);
                assert_eq!(late.barrier, before.barrier);
                assert_eq!(late.eligibility, before.eligibility);
            } else {
                model(&store, fixture_commands().remove(2));
                let baseline = snapshot(&store);
                assert!(matches!(
                    retire(&store, &target),
                    CapacityRetirementResult::AlreadyReleased { .. }
                ));
                assert_eq!(snapshot(&store), baseline);
            }
            assert!(store.active_delivery_owners().unwrap().is_empty());
            public_refusals(&store);
            let committed = snapshot(&store);
            drop(store);
            let store = RedbStore::open(&path).unwrap();
            assert_eq!(snapshot(&store), committed);
            public_refusals(&store);
        }
        // Native sixteen-slot orphan model: only the exact target is removed once.
        let store = RedbStore::open(&dir.path().join("sixteen.redb")).unwrap();
        for n in 0..16 {
            let (p, m) = next_input(n, n);
            install(&store, &p, &m);
            let DeliveryResult::Prepared { record } =
                model(&store, DeliveryCommand::Prepare(p.clone()))
            else {
                panic!("prepare")
            };
            model(&store, arm_for(&p, record.revision));
        }
        let owners = store.active_delivery_owners().unwrap();
        let target = &owners[0];
        let tx = store.db.begin_read().unwrap();
        let reservation: IdReservation = row(
            &tx,
            RESERVATIONS,
            &hex(&lp(target.owner.key.original_id.as_str())),
        )
        .unwrap()
        .unwrap();
        drop(tx);
        close_fixture(&store, target);
        assert!(matches!(
            retire(&store, target),
            CapacityRetirementResult::Retired { .. }
        ));
        let remaining = store.active_delivery_owners().unwrap();
        assert_eq!(remaining.len(), 15);
        assert_eq!(
            remaining.iter().map(|t| &t.owner).collect::<Vec<_>>(),
            owners[1..].iter().map(|t| &t.owner).collect::<Vec<_>>()
        );
        let tx = store.db.begin_read().unwrap();
        assert_eq!(
            row::<IdReservation>(
                &tx,
                RESERVATIONS,
                &hex(&lp(target.owner.key.original_id.as_str()))
            )
            .unwrap(),
            Some(reservation)
        );
        drop(tx);
        // A concurrent settlement and capacity retirement serialize on the native owner;
        // the loser may be stale, but neither can decrement twice or release the barrier of a retired attempt.
        let raced = Arc::new(RedbStore::open(&dir.path().join("retirement-race.redb")).unwrap());
        install(&raced, &prepare(), &fixture_message());
        let target = armed(&raced);
        close_fixture(&raced, &target);
        let barrier = Arc::new(Barrier::new(2));
        let settle = {
            let store = raced.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                model(&store, fixture_commands().remove(2))
            })
        };
        let retirement = {
            let store = raced.clone();
            let barrier = barrier.clone();
            let target = target.clone();
            std::thread::spawn(move || {
                barrier.wait();
                retire(&store, &target)
            })
        };
        let _ = settle.join().unwrap();
        let _ = retirement.join().unwrap();
        assert!(raced.active_delivery_owners().unwrap().is_empty());
        let view = raced.inspect_delivery(&prepare().key).unwrap().unwrap();
        assert!(matches!(
            view.attempts[0].capacity,
            CapacityState::Retired | CapacityState::Released
        ));
        let snapshot_before = snapshot(&raced);
        assert!(matches!(
            retire(&raced, &target),
            CapacityRetirementResult::AlreadyReleased { .. }
        ));
        assert_eq!(snapshot(&raced), snapshot_before);
    }
}
