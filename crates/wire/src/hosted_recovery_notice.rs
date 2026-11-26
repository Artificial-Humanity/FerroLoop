//! Private attachments for local hosted recovery notices.

use fl_core::agent::{AgentRole, AgentStatus};
use fl_core::at::At;
use fl_core::ferrowire::{
    MAX_WIRE_BODY_BYTES, MessageKind, WireAddress, WireFilter, WireInboxItem,
};
use fl_core::ids::{AgentId, MessageId};
use fl_core::iri::Iri;
use fl_core::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs, validate_slug,
};
use fl_core::store::{AgentRegistry, StateStore, StoreError};
use fl_host_proto::{MAX_HOSTED_EXIT_TAIL_BYTES, MAX_HOSTED_EXIT_TAIL_LINES};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Workspace runtime keys for the sensitive, local-only notice excerpts.
pub const NOTICE_STATE_SLUG_PREFIX: &str = "hosted-recovery-notice-";

// Mark the low random bits to screen ordinary mail before querying private state.
// The v7 timestamp, variant and version are left intact.
const NOTICE_UUID_MARKER: &[u8; 2] = b"hr";
const TAIL_HEADING: &str = "\n\nSanitized terminal excerpt from the captured exit\n";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attachment {
    recipient_id: AgentId,
    project: String,
    terminal_tail: String,
}

fn bounded_tail(tail: &str) -> bool {
    tail.len() <= MAX_HOSTED_EXIT_TAIL_BYTES
        && tail.lines().count() <= MAX_HOSTED_EXIT_TAIL_LINES
        && !tail.trim().is_empty()
        && tail
            .chars()
            .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
}

/// Save a sanitized terminal excerpt in sensitive workspace state. Send the
/// returned ID as a direct durable system mail with a safe body.
pub fn prepare(
    store: &impl StateStore,
    recipient: &AgentId,
    project: &str,
    terminal_tail: &str,
    at: At,
    by: &str,
) -> Result<MessageId, StoreError> {
    if !bounded_tail(terminal_tail) {
        return Err(StoreError::InvalidState(
            "invalid hosted recovery terminal excerpt".into(),
        ));
    }
    let project =
        validate_slug(project).map_err(|err| StoreError::InvalidState(err.to_string()))?;
    let mut bytes = *Uuid::now_v7().as_bytes();
    bytes[14..].copy_from_slice(NOTICE_UUID_MARKER);
    let uuid = Uuid::from_bytes(bytes);
    let id = MessageId(Iri::parse(&format!("urn:uuid:{uuid}")).expect("a UUIDv7 is a valid URN"));

    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: format!("{NOTICE_STATE_SLUG_PREFIX}{uuid}"),
        sensitivity: SensitivityPatch::SetSensitive,
        payload: StatePayload::Runtime(RuntimeEntry {
            value: serde_json::json!(Attachment {
                recipient_id: recipient.clone(),
                project,
                terminal_tail: terminal_tail.into(),
            }),
            merge_mode: MergeMode::Conflict,
        }),
        at,
        by: by.into(),
    })?;
    Ok(id)
}

/// Append private excerpts only to the results of a local named inbox read.
/// Call after ACK and publication so only a response to that read holds a tail.
pub fn attach_to_local_inbox(
    store: &(impl StateStore + AgentRegistry),
    filter: &WireFilter,
    items: &mut [WireInboxItem],
) {
    let Some(name) = filter
        .recipient
        .as_deref()
        .and_then(crate::identity::normalize_persona)
    else {
        return;
    };
    for item in items {
        if let Some(tail) = load_tail(store, item, &name)
            && item.message.body.len() + TAIL_HEADING.len() + tail.len() <= MAX_WIRE_BODY_BYTES
        {
            item.message.body.push_str(TAIL_HEADING);
            item.message.body.push_str(&tail);
        }
    }
}

fn load_tail(
    store: &(impl StateStore + AgentRegistry),
    item: &WireInboxItem,
    name: &str,
) -> Option<String> {
    let message = &item.message;
    let WireAddress::AgentId(recipient) = &message.to else {
        return None;
    };
    if message.sender_name != "system"
        || message.sender_id.is_some()
        || message.kind != MessageKind::Mail
        || !message.durable
        || item.receipt.message_id != message.id
        || &item.receipt.recipient_id != recipient.iri()
        || item.receipt.recipient_name != name
    {
        return None;
    }
    let id = message.id.as_str().strip_prefix("urn:uuid:")?;
    let uuid = Uuid::parse_str(id).ok()?;
    if id.len() != 36
        || uuid.get_version_num() != 7
        || uuid.get_variant() != uuid::Variant::RFC4122
        || !uuid.as_bytes().ends_with(NOTICE_UUID_MARKER)
    {
        return None;
    }
    let entry = store
        .get_state_by_key(
            &StateScope::Workspace,
            StateConcern::Runtime,
            &format!("{NOTICE_STATE_SLUG_PREFIX}{uuid}"),
        )
        .ok()??;
    if !entry.sensitive {
        return None;
    }
    let StatePayload::Runtime(runtime) = entry.payload else {
        return None;
    };
    let attachment: Attachment = serde_json::from_value(runtime.value).ok()?;
    if &attachment.recipient_id != recipient || !bounded_tail(&attachment.terminal_tail) {
        return None;
    }
    let agent = store.get_agent(recipient).ok()??;
    if &agent.id != recipient
        || agent.name_key != name
        || agent.status != AgentStatus::Active
        || !(agent.role == AgentRole::LabManager
            || (agent.role == AgentRole::Lead && agent.covers_area(&attachment.project)))
    {
        return None;
    }
    Some(attachment.terminal_tail)
}
