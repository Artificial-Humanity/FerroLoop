//! Versioned canonical bytes for immutable messages and single-message payloads.

use super::Digest;
use crate::StoreError;
use crate::ferrowire::{WireAddress, WireMessage};
use sha2::{Digest as _, Sha256};

const GENERATION_DOMAIN: &[u8] = b"FerroLoop/WireDelivery/generation/v1";
const PAYLOAD_DOMAIN: &[u8] = b"FerroLoop/WireDelivery/payload/v1";

/// Bind all immutable fields, including the selector kind, without normalizing
/// stored strings. This does not validate delivery eligibility or confer proof.
pub fn canonical_generation(msg: &WireMessage) -> Result<Digest, StoreError> {
    Ok(Digest(Sha256::digest(generation_bytes(msg)?).into()))
}

/// Bind one exact payload to its immutable generation and renderer version.
pub fn canonical_payload(
    generation: Digest,
    renderer: &str,
    bytes: &[u8],
) -> Result<Digest, StoreError> {
    Ok(Digest(
        Sha256::digest(payload_bytes(generation, renderer, bytes)?).into(),
    ))
}

fn encode_lp(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), StoreError> {
    let length = u64::try_from(bytes.len())
        .map_err(|_| StoreError::Backend("canonical field length exceeds u64".into()))?;
    out.extend(length.to_be_bytes());
    out.extend(bytes);
    Ok(())
}

fn encode_option(out: &mut Vec<u8>, value: Option<&str>) -> Result<(), StoreError> {
    match value {
        None => out.push(0),
        Some(value) => {
            out.push(1);
            encode_lp(out, value.as_bytes())?;
        }
    }
    Ok(())
}

fn encode_selector(out: &mut Vec<u8>, selector: &WireAddress) -> Result<(), StoreError> {
    let role_wire;
    let parts: Vec<&str> = match selector {
        WireAddress::AgentName(name) => vec!["agent_name", name],
        WireAddress::AgentId(id) => vec!["agent_id", id.as_str()],
        WireAddress::All => vec!["all"],
        WireAddress::Area(area) => vec!["area", area],
        WireAddress::Role(role) => {
            role_wire = role.as_wire();
            vec!["role", &role_wire]
        }
        WireAddress::AreaRole { area, role } => {
            role_wire = role.as_wire();
            vec!["area_role", area, &role_wire]
        }
        WireAddress::Council => vec!["council"],
        WireAddress::Group(group) => vec!["group", group],
    };
    for part in parts {
        encode_lp(out, part.as_bytes())?;
    }
    Ok(())
}

fn generation_bytes(msg: &WireMessage) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::new();
    encode_lp(&mut out, GENERATION_DOMAIN)?;
    encode_lp(&mut out, msg.id.as_str().as_bytes())?;
    encode_lp(&mut out, msg.thread_id.as_str().as_bytes())?;
    encode_option(&mut out, msg.reply_to.as_ref().map(|id| id.as_str()))?;
    encode_option(&mut out, msg.correlation_id.as_ref().map(|id| id.as_str()))?;
    encode_option(&mut out, msg.sender_id.as_ref().map(|id| id.as_str()))?;
    encode_lp(&mut out, msg.sender_name.as_bytes())?;
    encode_option(&mut out, msg.sender_host.as_deref())?;
    encode_selector(&mut out, &msg.to)?;
    encode_lp(&mut out, msg.kind.as_wire().as_bytes())?;
    encode_lp(&mut out, msg.urgency.as_wire().as_bytes())?;
    out.push(u8::from(msg.durable));
    encode_lp(&mut out, msg.subject.as_bytes())?;
    encode_lp(&mut out, msg.body.as_bytes())?;
    encode_lp(&mut out, msg.created_at.as_str().as_bytes())?;
    encode_option(&mut out, msg.deadline_at.as_ref().map(|at| at.as_str()))?;
    Ok(out)
}

fn payload_bytes(generation: Digest, renderer: &str, bytes: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::new();
    encode_lp(&mut out, PAYLOAD_DOMAIN)?;
    encode_lp(&mut out, renderer.as_bytes())?;
    out.extend(1_u64.to_be_bytes());
    encode_lp(&mut out, &generation.0)?;
    encode_lp(&mut out, bytes)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRole;
    use crate::ferrowire::{MessageKind, MessageUrgency, WireAddress, WireMessage};
    use crate::wire_delivery::Digest;
    use crate::{At, MessageId, ids::seq_iri};

    fn golden_message() -> WireMessage {
        WireMessage {
            id: MessageId(seq_iri(1)),
            thread_id: MessageId(seq_iri(1)),
            reply_to: None,
            correlation_id: None,
            sender_id: None,
            sender_name: "alice".into(),
            sender_host: None,
            to: WireAddress::AgentName("bob".into()),
            kind: MessageKind::Mail,
            urgency: MessageUrgency::Normal,
            durable: false,
            subject: "Hello".into(),
            body: "Body".into(),
            created_at: At::parse("2026-11-06T00:00:00.000Z").unwrap(),
            deadline_at: None,
        }
    }

    fn unhex(raw: &str) -> Vec<u8> {
        raw.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn digest(raw: &str) -> Digest {
        Digest(unhex(raw).try_into().unwrap())
    }

    #[test]
    fn managed_c14n_golden_fields_and_selectors() {
        let msg = golden_message();
        // Independently encoded planning fixture, not derived with the encoder under test.
        let expected = unhex(concat!(
            "0000000000000024466572726f4c6f6f702f5769726544656c69766572792f67656e65726174696f6e2f7631",
            "000000000000002d75726e3a757569643a30303030303030302d303030302d373030302d383030302d303030303030303030303031",
            "000000000000002d75726e3a757569643a30303030303030302d303030302d373030302d383030302d303030303030303030303031",
            "0000000000000000000005616c69636500000000000000000a6167656e745f6e616d65",
            "0000000000000003626f6200000000000000046d61696c00000000000000066e6f726d616c00",
            "000000000000000548656c6c6f0000000000000004426f6479",
            "0000000000000018323032362d31312d30365430303a30303a30302e3030305a00"
        ));
        assert_eq!(expected.len(), 281);
        assert_eq!(generation_bytes(&msg).unwrap(), expected);
        let generation = canonical_generation(&msg).unwrap();
        assert_eq!(
            generation,
            digest("1f7850e9a8504f1f77793182b2ebca7f12b68665b432ffba1c1df32c50f60413")
        );
        assert_eq!(
            canonical_payload(generation, "wire-peer-envelope-v1", b"example\n").unwrap(),
            digest("cbeaaa916435db1d6e91c7932fa644e7f98c3a12b3632ad00d12770b2d7e030a")
        );
        assert_eq!(msg.created_at.as_str().len(), 24);
        let payload = payload_bytes(generation, "wire-peer-envelope-v1", b"example\n").unwrap();
        assert_eq!(payload.len(), 134);
        assert_eq!(&payload[70..78], &1_u64.to_be_bytes());
        assert_eq!(&payload[78..86], &32_u64.to_be_bytes());
        assert_eq!(&payload[86..118], &generation.0);
    }

    #[test]
    fn managed_c14n_selector_tags_and_custom_roles_are_literal() {
        let id = crate::AgentId(seq_iri(9));
        let cases = [
            (
                WireAddress::AgentName("bob".into()),
                vec!["agent_name", "bob"],
            ),
            (
                WireAddress::AgentId(id.clone()),
                vec!["agent_id", id.as_str()],
            ),
            (WireAddress::All, vec!["all"]),
            (WireAddress::Area("ops".into()), vec!["area", "ops"]),
            (WireAddress::Role(AgentRole::Lead), vec!["role", "lead"]),
            (
                WireAddress::Role(AgentRole::Custom("scribe".into())),
                vec!["role", "custom:scribe"],
            ),
            (
                WireAddress::AreaRole {
                    area: "ops".into(),
                    role: AgentRole::Assistant,
                },
                vec!["area_role", "ops", "assistant"],
            ),
            (WireAddress::Council, vec!["council"]),
            (WireAddress::Group("ops".into()), vec!["group", "ops"]),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (selector, literal_parts) in cases {
            let mut expected = Vec::new();
            for part in literal_parts {
                expected.extend((part.len() as u64).to_be_bytes());
                expected.extend(part.as_bytes());
            }
            let mut actual = Vec::new();
            encode_selector(&mut actual, &selector).unwrap();
            assert_eq!(actual, expected);
            let mut msg = golden_message();
            msg.to = selector;
            assert!(seen.insert(canonical_generation(&msg).unwrap()));
        }
    }

    #[test]
    fn managed_c14n_options_boolean_and_stored_bytes_are_distinct() {
        let original = golden_message();
        let base = canonical_generation(&original).unwrap();
        let mut alternatives = Vec::new();
        let mut msg = original.clone();
        msg.reply_to = Some(MessageId(seq_iri(2)));
        alternatives.push(msg);
        let mut msg = original.clone();
        msg.correlation_id = Some(MessageId(seq_iri(2)));
        alternatives.push(msg);
        let mut msg = original.clone();
        msg.sender_id = Some(crate::AgentId(seq_iri(2)));
        alternatives.push(msg);
        let mut msg = original.clone();
        msg.sender_host = Some("host".into());
        alternatives.push(msg);
        let mut msg = original.clone();
        msg.deadline_at = Some(At::parse("2026-11-07T00:00:00.000Z").unwrap());
        alternatives.push(msg);
        let mut msg = original.clone();
        msg.durable = true;
        alternatives.push(msg);
        let mut msg = original.clone();
        msg.subject = " Hello ".into();
        alternatives.push(msg);
        let mut msg = original.clone();
        msg.body = "Body\n".into();
        alternatives.push(msg);
        let mut seen = std::collections::BTreeSet::new();
        for msg in alternatives {
            let changed = canonical_generation(&msg).unwrap();
            assert_ne!(base, changed);
            assert!(seen.insert(changed));
        }
        let mut composed = original.clone();
        composed.body = "é".into();
        let mut decomposed = original.clone();
        decomposed.body = "e\u{301}".into();
        assert_ne!(
            canonical_generation(&composed).unwrap(),
            canonical_generation(&decomposed).unwrap()
        );
        let mut bytes = Vec::new();
        encode_option(&mut bytes, Some("é")).unwrap();
        assert_eq!(bytes, b"\x01\0\0\0\0\0\0\0\x02\xc3\xa9");
        bytes.clear();
        encode_option(&mut bytes, None).unwrap();
        assert_eq!(bytes, b"\0");
        assert_ne!(
            canonical_payload(base, "renderer-a", b"bytes").unwrap(),
            canonical_payload(base, "renderer-b", b"bytes").unwrap()
        );
        assert_ne!(
            canonical_payload(base, "renderer-a", b"bytes").unwrap(),
            canonical_payload(base, "renderer-a", b"bytes\n").unwrap()
        );
        assert_ne!(
            canonical_payload(base, "renderer-a", b"bytes").unwrap(),
            canonical_payload(Digest([7; 32]), "renderer-a", b"bytes").unwrap()
        );
    }
}
