//! What a person types to name an item, and what they read back (spec §4).
//! A handle is display only: it is resolved here, at the edge, and never
//! travels further in.

use anyhow::{Result, bail};
use fl_core::{Handles, Iri, Kind};

#[derive(Debug, Clone)]
pub enum Ref {
    Handle(u64),
    Iri(Iri),
}

impl std::str::FromStr for Ref {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            return s
                .parse::<u64>()
                .map(Ref::Handle)
                .map_err(|_| format!("`{s}` is too large to be a handle"));
        }
        Iri::parse(s).map(Ref::Iri).map_err(|e| {
            format!(
                "`{s}` is neither a handle (a number such as `3`) nor an IRI (such as `urn:uuid:…`): {e}"
            )
        })
    }
}

/// What the person typed, for a message that echoes it back. An IRI prints
/// in its normalized form, which names the same item.
impl std::fmt::Display for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ref::Handle(n) => write!(f, "{n}"),
            Ref::Iri(i) => write!(f, "{i}"),
        }
    }
}

/// The id `r` names. A handle resolves only in `store`; an IRI is returned
/// as given, and the store checks ownership when it is asked for the item.
pub fn resolve(store: &impl Handles, label: &str, kind: Kind, r: &Ref) -> Result<Iri> {
    match r {
        Ref::Iri(i) => Ok(i.clone()),
        Ref::Handle(n) => match store.resolve_handle(kind, *n)? {
            Some(i) => Ok(i),
            None => bail!(
                "there is no {} {n} in the store at {label}. List them to see the ones that exist.",
                kind.as_wire()
            ),
        },
    }
}

/// How a person reads an id: its handle, or the full IRI if it has none here.
pub fn show(store: &impl Handles, kind: Kind, id: &Iri) -> Result<String> {
    Ok(match store.handle_of(kind, id)? {
        Some(n) => n.to_string(),
        None => id.to_string(),
    })
}

/// Every `Ref::Iri` among `refs`, in order. A handle is store-local and
/// carries no ownership information, so it plays no part in choosing which
/// store a command's items are searched for in (spec §2.6).
pub fn iris(refs: &[&Ref]) -> Vec<Iri> {
    refs.iter()
        .filter_map(|r| match r {
            Ref::Iri(i) => Some(i.clone()),
            Ref::Handle(_) => None,
        })
        .collect()
}

/// Whether `refs` includes a handle. A handle resolves only in the store it
/// was read from — if an IRI elsewhere among the same command's items sends
/// the search to a different store, a handle alongside it must not be
/// silently resolved against that other store's numbering (Fix round 1,
/// item 1).
pub fn has_handle(refs: &[&Ref]) -> bool {
    refs.iter().any(|r| matches!(r, Ref::Handle(_)))
}
