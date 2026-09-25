use crate::iri::Iri;
use serde::{Deserialize, Serialize};

/// What kind of item an id names. Crosses the boundary in the store's
/// ownership index and in handle tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Project,
    Gate,
    Record,
    Finding,
}

crate::wire::wire_names!(Kind as kind_wire {
    Project => "project",
    Gate => "gate",
    Record => "record",
    Finding => "finding",
});
crate::wire::wire_parse!(Kind as kind_parse);

/// A deterministic, UUID-shaped id for stores that must not use a clock or
/// randomness (`MemStore`, and tests). Never used by a store that persists.
pub fn seq_iri(n: u64) -> Iri {
    Iri::parse(&format!("urn:uuid:00000000-0000-7000-8000-{n:012x}"))
        .expect("a formatted urn:uuid is a valid IRI")
}

macro_rules! id_type {
    ($name:ident, $kind:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Iri);

        impl $name {
            pub const KIND: Kind = $kind;
            pub fn iri(&self) -> &Iri {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(ProjectId, Kind::Project);
id_type!(GateId, Kind::Gate);
id_type!(RecordId, Kind::Record);
id_type!(FindingId, Kind::Finding);
