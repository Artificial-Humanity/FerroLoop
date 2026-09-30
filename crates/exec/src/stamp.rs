//! The id and the time a new ledger entry is stamped with (GitHub ledger
//! spec §1.3). Here rather than in `fl-core`, which has no clock and no
//! randomness.

use fl_core::{At, Iri};
use std::time::{SystemTime, UNIX_EPOCH};

/// A fresh `urn:uuid:` of version 7: ordered by time, and unique without
/// asking anyone.
pub fn entry_id() -> Iri {
    Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7()))
        .expect("a formatted urn:uuid is a valid IRI")
}

/// Now. A clock set before 1970 stamps the epoch itself: a visibly wrong
/// time on an entry, never a refusal of evidence already gathered. `at` only
/// orders entries; the id alone identifies one.
pub fn now() -> At {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    At::from_unix_millis(ms)
}

#[cfg(test)]
mod tests {
    #[test]
    fn two_ids_are_never_the_same_and_both_are_version_7() {
        let (a, b) = (super::entry_id(), super::entry_id());
        assert_ne!(a, b);
        for id in [&a, &b] {
            let hex = id.as_str().strip_prefix("urn:uuid:").expect("a urn:uuid");
            assert_eq!(hex.as_bytes()[14], b'7', "not version 7: {id}");
        }
    }

    #[test]
    fn now_is_after_this_code_was_written() {
        assert!(super::now().as_str() > "2026-09-30T00:00:00.000Z");
    }
}
