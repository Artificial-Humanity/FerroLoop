//! What a person types to name an item, and what they read back (spec §4).
//! A handle is display only: it is resolved here, at the edge, and never
//! travels further in.

use anyhow::{Result, bail};
use fl_core::{Handles, Iri, Kind};

#[derive(Debug, Clone)]
pub enum Ref {
    /// A bare number: a local item in a routed store (routing spec §2.3),
    /// the tracker's own handle elsewhere.
    Handle(u64),
    /// `#41`: a GitHub issue in a routed store; elsewhere the same as a
    /// bare number, as GitHub writes one.
    Issue(u64),
    Iri(Iri),
}

impl std::str::FromStr for Ref {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        // `owner/repo#41` names that repository's issue (GitHub tracker spec
        // §2.1). The tracker answers `NotOwned` for another repository.
        if let Some((repo, n)) = s.split_once('#')
            && repo.contains('/')
            && !repo.contains(':')
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
        {
            // Shaped like `owner/repo#41` and not an IRI (no scheme): the
            // repository must pass the same check as the config's
            // `github = "owner/repo"`.
            if !crate::config::is_owner_repo(repo) {
                return Err(format!(
                    "`{s}` names an issue, but `{repo}` is not a repository: write it as \
                     `owner/repo#{n}`, with both parts named"
                ));
            }
            return Iri::parse(&format!("https://github.com/{repo}/issues/{n}"))
                .map(Ref::Iri)
                .map_err(|e| format!("`{s}` names an issue fl cannot address: {e}"));
        }
        if let Some(digits) = s.strip_prefix('#')
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            return digits
                .parse::<u64>()
                .map(Ref::Issue)
                .map_err(|_| format!("`{s}` is too large to be an issue number"));
        }
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
            Ref::Issue(n) => write!(f, "#{n}"),
            Ref::Iri(i) => write!(f, "{i}"),
        }
    }
}

/// The id `r` names. A handle resolves only in `store`; an IRI is returned
/// as given, and the store checks ownership when it is asked for the item.
pub fn resolve(store: &dyn Handles, label: &str, kind: Kind, r: &Ref) -> Result<Iri> {
    match r {
        Ref::Iri(i) => Ok(i.clone()),
        Ref::Handle(n) | Ref::Issue(n) => match store.resolve_handle(kind, *n)? {
            Some(i) => Ok(i),
            None => bail!(
                "there is no {} {n} in the store at {label}. List them to see the ones that exist.",
                kind.as_wire()
            ),
        },
    }
}

/// How a person reads an id: its handle, or the full IRI if it has none here.
pub fn show(store: &dyn Handles, kind: Kind, id: &Iri) -> Result<String> {
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
            Ref::Handle(_) | Ref::Issue(_) => None,
        })
        .collect()
}

/// Whether `refs` includes a handle. A handle resolves only in the store it
/// was read from — if an IRI elsewhere among the same command's items sends
/// the search to a different store, a handle alongside it must not be
/// silently resolved against that other store's numbering.
pub fn has_handle(refs: &[&Ref]) -> bool {
    refs.iter()
        .any(|r| matches!(r, Ref::Handle(_) | Ref::Issue(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Ref, String> {
        s.parse()
    }

    #[test]
    fn owner_repo_hash_n_is_that_issue_url() {
        match parse("acme/widgets#41") {
            Ok(Ref::Iri(i)) => assert_eq!(i.as_str(), "https://github.com/acme/widgets/issues/41"),
            other => panic!("expected the issue URL, got {other:?}"),
        }
    }

    // Routing spec §2.3: `#41` is GitHub's spelling, kept apart from a bare
    // number, and each prints back as it was typed.
    #[test]
    fn a_hash_number_is_an_issue_and_a_bare_one_a_handle() {
        assert!(matches!(parse("#41"), Ok(Ref::Issue(41))));
        assert!(matches!(parse("41"), Ok(Ref::Handle(41))));
        assert_eq!(parse("#41").unwrap().to_string(), "#41");
        assert_eq!(parse("41").unwrap().to_string(), "41");
        let (issue, handle) = (parse("#41").unwrap(), parse("41").unwrap());
        assert!(has_handle(&[&issue]) && has_handle(&[&handle]));
        assert!(iris(&[&issue, &handle]).is_empty());
        assert!(parse("#").is_err() && parse("#4a").is_err());
    }

    // Outside a routed store, nothing changes: `#1` and `1` name one item.
    #[test]
    fn outside_a_routed_store_a_hash_number_and_a_bare_one_resolve_alike() {
        use fl_core::store::{Catalog, Tracker};
        let s = fl_core::MemStore::default();
        let p = s.add_project("/p").unwrap();
        let r = s.add_record(&p, "t").unwrap();
        for typed in [Ref::Handle(1), Ref::Issue(1)] {
            assert_eq!(resolve(&s, "memory", Kind::Record, &typed).unwrap(), r.0);
        }
    }

    #[test]
    fn an_issue_ref_with_an_empty_owner_or_repo_is_refused() {
        for s in ["/#1", "a/#1", "/b#1", "a/b/c#1", "a b/c#1"] {
            let err = parse(s).expect_err(s);
            assert!(err.contains("owner/repo#1"), "{s}: {err}");
        }
    }

    #[test]
    fn an_iri_with_a_fragment_is_still_an_iri() {
        // A scheme makes it an IRI, even when its tail looks like `a/b#1`.
        match parse("tag:acme/widgets#1") {
            Ok(Ref::Iri(i)) => assert_eq!(i.as_str(), "tag:acme/widgets#1"),
            other => panic!("expected the IRI as typed, got {other:?}"),
        }
    }
}
