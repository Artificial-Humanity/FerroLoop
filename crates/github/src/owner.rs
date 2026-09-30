//! Which records a repository owns, for the GitHub ledger (GitHub ledger
//! spec §2.1). Local: the record's IRI and what the local store remembers,
//! never a network call.

use crate::meta::parse_issue_url;
use fl_core::iri::Iri;
use fl_core::store::{Bindings, StoreError};

/// Whether `record` is an issue of the repository whose current full name
/// is `current` and whose node is `node_id`.
///
/// ⚠ The issue URL names its repository by `owner/repo`. That is this
/// repository when the name is its current one, without regard to case, or
/// a name this store has bound to the same node — the configured name kept
/// after a rename (GitHub tracker spec §2.4). Anything else is not owned:
/// another repository's URL, a URL under a name this store never bound (fl
/// claims no history it does not know), and any IRI that is not an issue
/// URL.
pub fn issue_of_repository(
    record: &Iri,
    current: &str,
    node_id: &str,
    memory: &dyn Bindings,
) -> Result<bool, StoreError> {
    let Some((name, _)) = parse_issue_url(record) else {
        return Ok(false);
    };
    if name.eq_ignore_ascii_case(current) {
        return Ok(true);
    }
    Ok(memory.bound_node_id(&name)?.as_deref() == Some(node_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;

    fn url(s: &str) -> Iri {
        Iri::parse(s).unwrap()
    }

    #[test]
    fn an_issue_under_the_current_name_is_owned_whatever_its_case() {
        let memory = MemStore::default();
        for record in [
            "https://github.com/acme/gadgets/issues/1",
            "https://github.com/Acme/Gadgets/issues/2",
        ] {
            assert!(
                issue_of_repository(&url(record), "acme/gadgets", "R_1", &memory).unwrap(),
                "{record}"
            );
        }
    }

    // A record raised before a rename keeps the old name in its URL; the
    // store bound that name to this repository's node when it was
    // configured.
    #[test]
    fn an_issue_under_a_name_this_store_bound_to_the_same_node_is_owned() {
        let memory = MemStore::default();
        memory.bind_node_id("acme/widgets", "R_1").unwrap();
        let old = url("https://github.com/acme/widgets/issues/1");
        assert!(issue_of_repository(&old, "acme/gadgets", "R_1", &memory).unwrap());
    }

    #[test]
    fn anything_else_is_not_owned() {
        let memory = MemStore::default();
        memory.bind_node_id("acme/other", "R_2").unwrap();
        for record in [
            // Another repository this store knows, under its own node.
            "https://github.com/acme/other/issues/1",
            // A name this store never bound: fl claims no history.
            "https://github.com/acme/unknown/issues/1",
            // Not an issue URL at all.
            "https://github.com/acme/gadgets/pull/1",
            "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b",
        ] {
            assert!(
                !issue_of_repository(&url(record), "acme/gadgets", "R_1", &memory).unwrap(),
                "{record}"
            );
        }
    }
}
