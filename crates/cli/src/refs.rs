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
    /// A lowercase project slug (such as `ferroloop`), resolved only when
    /// `Kind::Project` is asked for.
    Slug(String),
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
            // Normalise through u64 so `#007` and `#7` are one IRI, and a
            // number no issue can have is refused here.
            let Ok(num) = n.parse::<u64>() else {
                return Err(format!("`{s}` names an issue number that is too large"));
            };
            return Iri::parse(&format!("https://github.com/{repo}/issues/{num}"))
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
        match Iri::parse(s) {
            Ok(iri) => Ok(Ref::Iri(iri)),
            Err(e) => {
                if let Ok(slug) = fl_core::state::validate_slug(s) {
                    return Ok(Ref::Slug(slug));
                }
                Err(format!(
                    "`{s}` is neither a handle (a number such as `3`) nor an IRI (such as `urn:uuid:…`): {e}"
                ))
            }
        }
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
            Ref::Slug(s) => write!(f, "{s}"),
        }
    }
}

impl Ref {
    /// The project slug when `self` is `Ref::Slug`.
    pub fn as_slug(&self) -> Option<&str> {
        match self {
            Ref::Slug(s) => Some(s.as_str()),
            Ref::Handle(_) | Ref::Issue(_) | Ref::Iri(_) => None,
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
        Ref::Slug(slug) => {
            if kind != Kind::Project {
                bail!(
                    "`{slug}` is neither a handle (a number such as `3`) nor an IRI (such as `urn:uuid:…`)"
                );
            }
            resolve_project_slug(store, label, slug)
        }
    }
}

fn resolve_project_slug(store: &dyn Handles, label: &str, slug: &str) -> Result<Iri> {
    #[cfg(not(test))]
    let cfg_entries = match crate::config::path() {
        Some(cfg_path) => crate::config::load(Some(&cfg_path))
            .map(|c| c.projects)
            .unwrap_or_default(),
        None => Vec::new(),
    };
    #[cfg(test)]
    let cfg_entries: Vec<crate::config::Entry> = Vec::new();
    resolve_project_slug_with_entries(store, label, slug, &cfg_entries)
}

fn resolve_project_slug_with_entries(
    store: &dyn Handles,
    label: &str,
    slug: &str,
    entries: &[crate::config::Entry],
) -> Result<Iri> {
    let projects = store.projects_for_slug()?;
    if let Some(entry) = crate::config::entry_by_slug(entries, slug)? {
        let entry_canon = entry.root.canonicalize().ok();
        for p in &projects {
            let p_path = std::path::Path::new(&p.root);
            if p_path == entry.root
                || (entry_canon.is_some() && p_path.canonicalize().ok() == entry_canon)
            {
                return Ok(p.id.0.clone());
            }
        }
        bail!(
            "there is no project `{slug}` in the store at {label}. List them to see the ones that exist."
        );
    }
    let matched: Vec<&fl_core::Project> = projects
        .iter()
        .filter(|p| {
            let trimmed = p.root.trim_end_matches(['/', '\\']);
            std::path::Path::new(trimmed)
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|base| base.eq_ignore_ascii_case(slug))
        })
        .collect();
    match matched.as_slice() {
        [p] => Ok(p.id.0.clone()),
        [] => bail!(
            "there is no project `{slug}` in the store at {label}. List them to see the ones that exist."
        ),
        many => {
            let roots = many
                .iter()
                .map(|p| p.root.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "project slug `{slug}` is ambiguous in the store at {label}: matches {roots}. \
                 Pass the project handle or IRI, or disambiguate with `name` in `[[project]]`"
            )
        }
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
            Ref::Handle(_) | Ref::Issue(_) | Ref::Slug(_) => None,
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
    fn issue_number_is_normalised_and_bounded() {
        match parse("acme/w#007") {
            Ok(Ref::Iri(i)) => assert_eq!(i.as_str(), "https://github.com/acme/w/issues/7"),
            other => panic!("expected the canonical issue URL, got {other:?}"),
        }
        assert!(parse("acme/w#99999999999999999999999").is_err());
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

    #[test]
    fn project_slug_resolves_for_project_and_refuses_other_kinds() {
        use fl_core::store::Catalog;
        let s = fl_core::MemStore::default();
        let p = s.add_project("/workspace/FerroLoop").unwrap();
        let slug_ref = parse("ferroloop").expect("slug parses as Ref");
        assert_eq!(slug_ref.as_slug(), Some("ferroloop"));
        assert_eq!(slug_ref.to_string(), "ferroloop");
        assert!(!has_handle(&[&slug_ref]));
        assert!(iris(&[&slug_ref]).is_empty());

        assert_eq!(
            resolve(&s, "memory", Kind::Project, &slug_ref).unwrap(),
            p.0
        );

        let missing = parse("unknown-proj").unwrap();
        let err = resolve(&s, "memory", Kind::Project, &missing).unwrap_err();
        assert!(
            err.to_string()
                .contains("there is no project `unknown-proj` in the store at memory"),
            "unexpected error: {err}"
        );

        let wrong_kind = resolve(&s, "memory", Kind::Record, &slug_ref).unwrap_err();
        assert!(
            wrong_kind
                .to_string()
                .contains("`ferroloop` is neither a handle"),
            "unexpected error: {wrong_kind}"
        );
    }

    #[test]
    fn resolve_project_slug_prefers_config_entry_over_store_basename_and_refuses_ambiguous_basenames()
     {
        use fl_core::store::Catalog;
        let s = fl_core::MemStore::default();
        let p_first = s.add_project("/workspace/alpha/widgets").unwrap();
        let p_second = s.add_project("/workspace/beta/widgets").unwrap();
        let p_named = s.add_project("/workspace/gamma/custom-root").unwrap();

        // Without config entries, two projects in the store sharing the `widgets` basename are refused.
        let ambig_err =
            resolve_project_slug_with_entries(&s, "memory", "widgets", &[]).unwrap_err();
        let ambig_msg = ambig_err.to_string();
        assert!(
            ambig_msg.contains("/workspace/alpha/widgets"),
            "{ambig_msg}"
        );
        assert!(ambig_msg.contains("/workspace/beta/widgets"), "{ambig_msg}");
        assert_ne!(p_first, p_second);

        // An authoritative config entry with `name = "widgets"` resolves to `p_named` first,
        // before store basename matching.
        let cfg_entries = vec![crate::config::Entry {
            name: Some("widgets".into()),
            root: "/workspace/gamma/custom-root".into(),
            store: "/s/main.redb".into(),
            tracker: None,
            state: None,
        }];
        let resolved =
            resolve_project_slug_with_entries(&s, "memory", "widgets", &cfg_entries).unwrap();
        assert_eq!(resolved, p_named.0);

        // Ambiguous config entries for the same slug propagate the config refusal.
        let conflicting_cfg = vec![
            crate::config::Entry {
                name: None,
                root: "/workspace/alpha/widgets".into(),
                store: "/s/a.redb".into(),
                tracker: None,
                state: None,
            },
            crate::config::Entry {
                name: None,
                root: "/workspace/beta/widgets".into(),
                store: "/s/b.redb".into(),
                tracker: None,
                state: None,
            },
        ];
        let cfg_err = resolve_project_slug_with_entries(&s, "memory", "widgets", &conflicting_cfg)
            .unwrap_err();
        assert!(
            cfg_err
                .to_string()
                .contains("matches more than one entry in the config"),
            "unexpected error: {cfg_err}"
        );
    }
}
