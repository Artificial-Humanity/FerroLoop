# GitHub tracker, plan B — the tracker in GitHub Issues

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make GitHub Issues a `Tracker` that `fl` can bind a project to — records and findings live in issues, with every divergence, conflict, disclosure and identity rule the spec requires — and wire it into the CLI.

**Architecture:** A new crate, `fl-github`, holds a blocking REST/GraphQL `Client`, two `Credentials` sources (an environment token and a GitHub App), the pure issue encoding (`meta`), and `GithubTracker`, which implements `Tracker` and `Handles`. `fl-core` gains the pieces a split binding needs: `Catalog::kind_of`, a `CatalogChecked` adapter that checks cross-store references against the catalog, a `KindRouted` handle router, a `Bindings` memory for the repository's `node_id`, and new `StoreError` variants. An in-process fake GitHub (feature `fake`) drives every test; one ignored live test checks the real thing. The CLI builds a `Ctx` that routes tracker work to GitHub when the project's config entry binds it.

**Tech Stack:** Rust 2024 (`rust-version = "1.98"`); new: `ureq` 3.4 (feature `json`), `ring` 0.17, `base64` 0.22, `tiny_http` 0.12 (optional, `fake` feature and tests only).

**Spec:** `docs/superpowers/specs/2026-09-26-github-tracker-design.md` — all of it; especially §1.3 (role binding), §2 (identity), §3 (encoding, writes, reads), §5 (credentials), §6 (disclosure), §7 (errors), §8 (testing). **Prerequisite:** plan A (`2026-09-26-github-tracker-a-manifest.md`) merged — this plan uses its `Bound`/`Fixture` harness, the manifest, `ensure_publishable`, and `RedbStore`'s additive-table pattern.

## Global Constraints

- Verification trio, all green before every commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`.
- Unit tests live in `#[cfg(test)] mod tests` inside the module; black-box tests live in `crates/<crate>/tests/`.
- **No test contacts GitHub**, except the live tests of Task 10, which are `#[ignore]`d and refuse to run without `FL_GITHUB_LIVE_REPO`.
- Every failure path distinguishes "nothing" from "didn't look" (spec §7). No catch-all arm on a result that decides evidence. Every refusal names a remedy.
- **The response is the postcondition** (spec §3.3): a write is done only when GitHub's answer shows it.
- **The credential goes only to the configured API origin.** `Client` refuses to follow a `Link` or `Location` elsewhere. No secret in argv, in an error message, or in a log line.
- `snake_case` on the wire and in the issue's metadata block (decision 33).
- New dependencies only: `ureq = { version = "3.4", features = ["json"] }`, `ring = "0.17"`, `base64 = "0.22"`, `tiny_http = "0.12"` (optional). Record every new crate's licence in the commit that adds it (`cargo metadata --format-version 1 | jq` or the crate's `Cargo.toml`). Name, in the pull request, any licence that is not MIT, Apache-2.0, ISC, BSD or Zlib — `webpki-roots` (CA data, `CDLA-Permissive-2.0`) is expected to be one; the owner decides on it at review.
- Each guard is mutation-tested: revert it, watch its test go red, restore. Say so in the commit message.
- Commits keep the owner's author identity and end with `Co-authored-by: Ferris <Ferris@artificialhumanity.io>`.
- The repository is public: no machine paths, hostnames or lab-internal names. Test repositories are `acme/widgets` and similar.
- The fake proves structure, not integration (spec §8.4). Where the plan models a GitHub behaviour it has not measured, the code carries a comment saying so, and Task 10's live test is the check.

## Review Focus

1. **An issue edited in the web interface between fl's read and its write** must give a `Conflict`, never a quiet overwrite — Task 6 tests it with the fake's `foreign_label_on_next_patch` and `foreign_edit_on_next_patch` knobs.
2. **A repository with more fl issues than one page holds** must be read in full — Task 4 caps the fake's page size at 2 and lists 5 issues.
3. **A claim or withdrawal reason containing `-->` or `<!-- fl:meta`** must not end or duplicate the metadata block — Task 3 tests both.
4. **A record title over 256 characters** is refused before anything is sent; **a claim whose first line is over 256 characters** gets a truncated title and keeps the full claim in the body — Tasks 3 and 4.
5. **A command that does not need the tracker** (`gate list`, `manifest check`, …) must not contact GitHub at all — Task 8 asserts the fake saw no request.

## Owner decisions this plan carries (2026-09-27)

- No downgrade import: an older checkout's manifest is refused (plan A's behaviour stands).
- Anyone may run `fl github repair`; it requires `--by <name>`, which it records in a comment.
- The live test runs against a private throwaway repository in the organization; the owner registers the App and installs it there when the live test is ready (Task 10). Until then the live test runs with an environment token.
- FerroLoop's own repository does not use the GitHub tracker in this sub-project.

---

### Task 1: The split-binding pieces in `fl-core`

**Files:**
- Modify: `crates/core/src/store.rs` (new `StoreError` variants, `Catalog::kind_of`, `CatalogChecked`, `KindRouted`, `Bindings`, tests)
- Modify: `crates/core/src/lib.rs` (exports)
- Modify: `crates/core/src/finding.rs` (`security` field, test)
- Modify: `crates/core/src/mem.rs` (`kind_of`, `Bindings`)
- Modify: `crates/core/src/conformance.rs` (`Fixture` becomes a callback)
- Modify: `crates/store/src/lib.rs` (`kind_of`, `Bindings` over an additive `github_bindings` table, tests)
- Modify: `crates/exec/src/evaluate.rs` (the two test doubles gain `kind_of`)

**Interfaces:**
- Produces:

```rust
// StoreError, new variants
Deleted(Iri)
Diverged { id: Iri, detail: String }
Conflict { id: Iri, detail: String }
NotAnFlItem { id: Iri, what: String }
Moved { id: Iri, to: String }
RateLimited { reset: String }
RepositoryReplaced { name: String, bound: String, found: String }
Credential(String)
SecurityNotPrivate { repo: String, visibility: String }
// Catalog
fn kind_of(&self, id: &Iri) -> Result<Kind, StoreError>;
// adapters
pub struct CatalogChecked<'a> { pub catalog: &'a dyn Catalog, pub tracker: &'a dyn Tracker } // impl Tracker
pub struct KindRouted<'a> { pub catalog: &'a dyn Handles, pub tracker: &'a dyn Handles }       // impl Handles
pub trait Bindings {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError>;
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError>;
}
// Finding
pub security: bool   // #[serde(default)]
// conformance
pub trait Fixture { fn with(&self, f: &mut dyn FnMut(&Bound<'_>)); }
```

- [ ] **Step 1: Write the failing tests**

Append to `crates/core/src/finding.rs`'s `mod tests`:

```rust
    #[test]
    fn a_finding_stored_before_the_security_mark_reads_as_not_security() {
        let f = Finding::raise(ProjectId(seq_iri(1)), RecordId(seq_iri(2)), "a", "c");
        let mut v = serde_json::to_value(&f).unwrap();
        v.as_object_mut().unwrap().remove("security");
        let back: Finding = serde_json::from_value(v).unwrap();
        assert!(!back.security);
        assert!(!f.security, "raise never marks a finding security");
    }
```

Add a test module at the end of `crates/core/src/store.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use crate::ids::seq_iri;
    use crate::model::{CommandSpec, GateKind, PopulationDelivery};

    /// A tracker that must never be asked: every refusal below has to come
    /// from the binding's own check, before the tracker is reached.
    struct NeverAsked;
    impl Tracker for NeverAsked {
        fn add_record(&self, _: &ProjectId, _: &str) -> Result<RecordId, StoreError> {
            unreachable!("the binding must refuse before the tracker is asked")
        }
        fn get_record(&self, _: &RecordId) -> Result<Option<Record>, StoreError> {
            unreachable!("not used")
        }
        fn list_records(&self, _: &ProjectId) -> Result<Vec<Record>, StoreError> {
            Ok(vec![])
        }
        fn set_record_state(&self, _: &RecordId, _: State) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn add_finding(&self, _: Finding) -> Result<FindingId, StoreError> {
            unreachable!("the binding must refuse before the tracker is asked")
        }
        fn get_finding(&self, _: &FindingId) -> Result<Option<Finding>, StoreError> {
            unreachable!("not used")
        }
        fn update_finding(&self, _: &Finding) -> Result<(), StoreError> {
            unreachable!("not used")
        }
        fn list_findings(&self, _: &ProjectId) -> Result<Vec<Finding>, StoreError> {
            unreachable!("the binding must refuse before the tracker is asked")
        }
        fn withdrawals_by(&self, _: &str) -> Result<u64, StoreError> {
            unreachable!("not used")
        }
        fn add_alias(&self, _: &Iri, _: Iri) -> Result<(), StoreError> {
            unreachable!("not used")
        }
    }

    fn gate(catalog: &MemStore, p: &ProjectId) -> GateId {
        let kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        });
        let sel = Selector::Glob {
            pattern: "**/*".into(),
        };
        catalog.add_gate(p, "g", kind, sel, 1, "c", "o").unwrap()
    }

    #[test]
    fn a_split_binding_refuses_a_project_the_catalog_never_held() {
        let catalog = MemStore::default();
        let t = CatalogChecked {
            catalog: &catalog,
            tracker: &NeverAsked,
        };
        let stranger = ProjectId(seq_iri(99));
        let err = t.add_record(&stranger, "t").unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        let err = t.list_findings(&stranger).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
    }

    #[test]
    fn a_split_binding_refuses_another_kind_where_a_project_or_record_is_needed() {
        let catalog = MemStore::default();
        let p = catalog.add_project("/p").unwrap();
        let g = gate(&catalog, &p);
        let t = CatalogChecked {
            catalog: &catalog,
            tracker: &NeverAsked,
        };
        let err = t.add_record(&ProjectId(g.0.clone()), "t").unwrap_err();
        assert!(
            matches!(err, StoreError::WrongKind { expected: Kind::Project, found: Kind::Gate, .. }),
            "{err:?}"
        );
        let err = t
            .add_finding(Finding::raise(p, RecordId(g.0.clone()), "a", "c"))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::WrongKind { expected: Kind::Record, found: Kind::Gate, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_split_binding_passes_a_held_project_through_to_the_tracker() {
        let catalog = MemStore::default();
        let p = catalog.add_project("/p").unwrap();
        let t = CatalogChecked {
            catalog: &catalog,
            tracker: &NeverAsked,
        };
        assert_eq!(t.list_records(&p).unwrap(), vec![]);
    }

    #[test]
    fn kind_routed_asks_the_store_that_holds_each_kind() {
        let catalog = MemStore::default();
        let tracker = MemStore::default();
        let p = catalog.add_project("/p").unwrap();
        // MemStore ids are sequential, so both stores would mint the same
        // first id. Burn one in the tracker's store, so `tp` is an id the
        // catalog does not hold.
        tracker.add_project("/burned").unwrap();
        let tp = tracker.add_project("/q").unwrap();
        let r = tracker.add_record(&tp, "t").unwrap();
        let h = KindRouted {
            catalog: &catalog,
            tracker: &tracker,
        };
        assert_eq!(h.handle_of(Kind::Project, p.iri()).unwrap(), Some(1));
        assert_eq!(h.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
        assert_ne!(p, tp, "the routing check below needs two different ids");
        // A project the TRACKER's store holds is not asked of the tracker.
        assert_eq!(h.handle_of(Kind::Project, tp.iri()).unwrap(), None);
        assert_eq!(h.resolve_handle(Kind::Record, 1).unwrap().as_ref(), Some(r.iri()));
    }

    #[test]
    fn a_memory_store_remembers_a_binding_by_case_insensitive_name() {
        let s = MemStore::default();
        assert_eq!(s.bound_node_id("Acme/Widgets").unwrap(), None);
        s.bind_node_id("Acme/Widgets", "R_1").unwrap();
        assert_eq!(s.bound_node_id("ACME/widgets").unwrap().as_deref(), Some("R_1"));
    }
}
```

`Record`, `State` and `Selector` are already imported at the top of `store.rs`; add any that the compiler names.

Append to `crates/store/src/lib.rs`'s `mod tests`:

```rust
    #[test]
    fn a_binding_survives_a_reopen_and_a_fresh_store_has_none() {
        use fl_core::store::Bindings;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.redb");
        {
            let s = RedbStore::open(&path).unwrap();
            assert_eq!(s.bound_node_id("acme/widgets").unwrap(), None);
            s.bind_node_id("Acme/Widgets", "R_1").unwrap();
        }
        let s = RedbStore::open(&path).unwrap();
        assert_eq!(s.bound_node_id("ACME/widgets").unwrap().as_deref(), Some("R_1"));
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p fl-core -p fl-store`
Expected: FAIL to compile — `security`, `CatalogChecked`, `KindRouted`, `Bindings` do not exist.

- [ ] **Step 3: The new error variants**

In `crates/core/src/store.rs`, add to `StoreError` (after `Dangling`):

```rust
    /// ⚠ The owner deleted the item (a GitHub issue answered 410). "It was
    /// here and is gone" is not "no such item".
    #[error("{0} was deleted where it was held, so nothing can be read or written through it.")]
    Deleted(Iri),
    /// ⚠ fl's own record of an item disagrees with the item's visible state
    /// (GitHub tracker spec §3.4). fl adopts neither side silently.
    #[error(
        "{id} is diverged: {detail}. Run `fl github repair {id} --by <name>` to rewrite its \
         labels and status from fl's record — or, if the issue was never fl's, remove its fl \
         labels."
    )]
    Diverged { id: Iri, detail: String },
    /// ⚠ Another actor wrote the item while fl wrote it (spec §3.3). That
    /// write has landed, and fl's may have overwritten part of it.
    #[error(
        "{id} was changed by another actor while fl wrote it: {detail}. Read it again, check \
         it, and retry."
    )]
    Conflict { id: Iri, detail: String },
    /// The id names something that exists but that fl did not create.
    #[error(
        "{id} is {what}, not an item fl created, so fl neither reads nor changes it. Name an \
         fl record or finding instead."
    )]
    NotAnFlItem { id: Iri, what: String },
    /// ⚠ The item moved out of the store that owned it (a transferred issue).
    #[error(
        "{id} was moved to {to}, outside the repository this tracker binds. Bind that \
         repository, or raise the item again here."
    )]
    Moved { id: Iri, to: String },
    /// ⚠ Reported, never waited out in silence (spec §7).
    #[error("GitHub's rate limit is spent until {reset}. Retry after that time.")]
    RateLimited { reset: String },
    /// ⚠ The configured name now reaches another repository (spec §2.4).
    #[error(
        "`{name}` now reaches a different repository (node {found}) from the one this \
         tracker is bound to (node {bound}). Correct the `github` binding in config.toml; \
         fl refuses every read and write until then."
    )]
    RepositoryReplaced { name: String, bound: String, found: String },
    /// A credential is missing or refused. There is no fallback source.
    #[error("no usable GitHub credential: {0}")]
    Credential(String),
    /// ⚠ Spec §6: a security finding is written only where the public
    /// cannot read it.
    #[error(
        "refused: a security finding cannot be written to {repo}, whose visibility is \
         `{visibility}`. Use a local tracker for it, or bind a private repository."
    )]
    SecurityNotPrivate { repo: String, visibility: String },
```

- [ ] **Step 4: `kind_of`, the adapters and `Bindings`**

Add to `trait Catalog`:

```rust
    /// The kind this store holds `id` under, following an alias; `NotOwned`
    /// if it never held it. A split binding asks this to check a reference
    /// that crosses into another store (GitHub tracker spec §1.3).
    fn kind_of(&self, id: &Iri) -> Result<Kind, StoreError>;
```

Implement it: in `crates/core/src/mem.rs` `impl Catalog for MemStore` — `self.inner.borrow().check(id)`; in `crates/store/src/lib.rs` `impl Catalog for RedbStore` — `self.check(id)`; in `crates/exec/src/evaluate.rs` — `StampRefused` delegates (`self.0.kind_of(id)`) and `BrokenStore` returns `Err(broken())`. The evaluate test module needs `fl_core::{Iri, Kind}` in scope for those two impls.

Add after `impl Roles`:

```rust
/// A tracker whose project and record references are checked against a
/// catalog held by ANOTHER store (GitHub tracker spec §1.3, §8.1 item 2).
///
/// ⚠ For a split binding only. It reads "the catalog holds this id" as "it
/// is not a record", which is true only when the catalog's store backs no
/// tracker. A store that backs both roles checks its own references.
pub struct CatalogChecked<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
}

impl CatalogChecked<'_> {
    fn project(&self, p: &ProjectId) -> Result<(), StoreError> {
        match self.catalog.kind_of(p.iri())? {
            Kind::Project => Ok(()),
            found => Err(StoreError::WrongKind {
                id: p.iri().clone(),
                expected: Kind::Project,
                found,
            }),
        }
    }

    fn record(&self, r: &RecordId) -> Result<(), StoreError> {
        match self.catalog.kind_of(r.iri()) {
            Ok(found) => Err(StoreError::WrongKind {
                id: r.iri().clone(),
                expected: Kind::Record,
                found,
            }),
            // Not the catalog's: the tracker decides whether it is a record.
            Err(StoreError::NotOwned { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Tracker for CatalogChecked<'_> {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        self.project(project)?;
        self.tracker.add_record(project, title)
    }
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        self.tracker.get_record(id)
    }
    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        self.project(project)?;
        self.tracker.list_records(project)
    }
    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        self.tracker.set_record_state(id, state)
    }
    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        self.project(&finding.project)?;
        self.record(&finding.record)?;
        self.tracker.add_finding(finding)
    }
    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        self.tracker.get_finding(id)
    }
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        self.tracker.update_finding(finding)
    }
    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        self.project(project)?;
        self.tracker.list_findings(project)
    }
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        self.tracker.withdrawals_by(actor)
    }
    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        self.tracker.add_alias(primary, alias)
    }
}

/// Handles for a split binding: projects and gates are the catalog's to
/// number, records and findings the tracker's.
pub struct KindRouted<'a> {
    pub catalog: &'a dyn Handles,
    pub tracker: &'a dyn Handles,
}

impl KindRouted<'_> {
    fn owner(&self, kind: Kind) -> &dyn Handles {
        // Every kind by name: a new kind must be routed on purpose.
        match kind {
            Kind::Project | Kind::Gate => self.catalog,
            Kind::Record | Kind::Finding => self.tracker,
        }
    }
}

impl Handles for KindRouted<'_> {
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError> {
        self.owner(kind).handle_of(kind, id)
    }
    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError> {
        self.owner(kind).resolve_handle(kind, handle)
    }
}

/// What a local store remembers about the GitHub repositories a tracker is
/// bound to (GitHub tracker spec §2.4): the repository's `node_id`, keyed by
/// the configured `owner/repo`, compared without regard to case.
pub trait Bindings {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError>;
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError>;
}
```

`MemStore`: add `bindings: BTreeMap<String, String>` to `Inner` and

```rust
impl crate::store::Bindings for MemStore {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError> {
        Ok(self.inner.borrow().bindings.get(&repo.to_ascii_lowercase()).cloned())
    }
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError> {
        self.inner
            .borrow_mut()
            .bindings
            .insert(repo.to_ascii_lowercase(), node_id.to_string());
        Ok(())
    }
}
```

`RedbStore`: add the table and the impl.

```rust
/// configured `owner/repo` (lowercase) → the repository's `node_id`.
/// Additive like `imports`: created by the first bind, and a store without
/// it has bound nothing. An older fl ignores it, and cannot use GitHub mode.
const GITHUB_BINDINGS: TableDefinition<&str, &str> = TableDefinition::new("github_bindings");

impl fl_core::store::Bindings for RedbStore {
    fn bound_node_id(&self, repo: &str) -> Result<Option<String>, StoreError> {
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(GITHUB_BINDINGS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let found = table
            .get(repo.to_ascii_lowercase().as_str())
            .map_err(backend)?
            .map(|v| v.value().to_string());
        Ok(found)
    }
    fn bind_node_id(&self, repo: &str, node_id: &str) -> Result<(), StoreError> {
        let tx = self.db.begin_write().map_err(backend)?;
        tx.open_table(GITHUB_BINDINGS)
            .map_err(backend)?
            .insert(repo.to_ascii_lowercase().as_str(), node_id)
            .map_err(backend)?;
        tx.commit().map_err(backend)
    }
}
```

Export from `crates/core/src/lib.rs`: add `Bindings, CatalogChecked, KindRouted` to the `pub use store::{…}` line.

- [ ] **Step 5: The `security` field**

In `Finding` (after `also_known_as`):

```rust
    /// Marked when the finding is raised, and never changed after (GitHub
    /// tracker spec §6). A finding stored before the mark existed reads as
    /// not security.
    #[serde(default)]
    pub security: bool,
```

and `security: false,` in `Finding::raise`. Fix any other struct literal the compiler names.

- [ ] **Step 6: `Fixture` becomes a callback**

A split binding's `CatalogChecked` borrows two stores the fixture owns, so a fixture cannot return a `Bound` that holds it. In `crates/core/src/conformance.rs` replace the `Fixture` trait, its `Single` impl, and the loop in `run_bound`:

```rust
/// Hands one case its `Bound`. A callback rather than a return value, so a
/// fixture can build adapters that live only for the call — a split
/// binding's `CatalogChecked` borrows two stores the fixture owns.
pub trait Fixture {
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>));
}

impl<S: Catalog + Tracker + Ledger + Handles, G> Fixture for Single<S, G> {
    fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
        f(&Bound {
            catalog: &self.0,
            tracker: &self.0,
            ledger: &self.0,
            handles: &self.0,
        });
    }
}
```

and in `run_bound`:

```rust
    for case in cases {
        let fixture = make();
        fixture.with(&mut |b| case(b));
    }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --workspace`
Expected: PASS, including the 7 new tests.

- [ ] **Step 8: Mutation checks**

One at a time, confirm red, restore:
- `CatalogChecked::project` returns `Ok(())` → both `a_split_binding_refuses…` tests FAIL (the `NeverAsked` tracker panics).
- `CatalogChecked::record` returns `Ok(())` → `…another_kind…` FAILS.
- Swap the two arms of `KindRouted::owner` → `kind_routed_asks…` FAILS.
- Drop `.to_ascii_lowercase()` in `MemStore::bound_node_id` → `a_memory_store_remembers…` FAILS.

- [ ] **Step 9: Trio and commit**

```bash
git add crates/core crates/store/src/lib.rs crates/exec/src/evaluate.rs
git commit -m "feat(core): the split-binding pieces a GitHub tracker needs

Catalog::kind_of; CatalogChecked checks project and record references
against a catalog in another store; KindRouted numbers each kind in the
store that holds it; Bindings remembers a repository's node id (an
additive table in RedbStore); Finding.security; nine StoreError variants
for the GitHub tracker's outcomes. The conformance Fixture becomes a
callback so a split binding can build its adapter per case. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 2: `fl-github` — the client, the credentials, and the fake server

**Files:**
- Modify: `Cargo.toml` (workspace member and dependencies)
- Create: `crates/github/Cargo.toml`, `crates/github/src/lib.rs`, `crates/github/src/client.rs`, `crates/github/src/creds.rs`, `crates/github/src/fake.rs`

**Interfaces:**
- Consumes: Task 1's `StoreError` variants (`Credential`, `RateLimited`, `Unreachable`, `Backend`).
- Produces:

```rust
// client
pub const DEFAULT_API: &str = "https://api.github.com";
pub enum Method { Get, Post, Patch }
pub struct Reply { pub status: u16, pub body: serde_json::Value, pub location: Option<String> }
pub struct Client;
impl Client {
    pub fn new(api: &str, creds: Box<dyn Credentials>) -> Client;
    pub fn describe(&self) -> String;
    pub fn send(&self, m: Method, path_or_url: &str, body: Option<&Value>) -> Result<Reply, StoreError>;
    pub fn get_all(&self, path: &str) -> Result<Vec<Value>, StoreError>;
    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value, StoreError>;
    pub fn identity(&self) -> Result<String, StoreError>;
}
// creds
pub trait Credentials {
    fn token(&self) -> Result<String, StoreError>;
    fn describe(&self) -> String;
    fn identity(&self, api: &str) -> Result<String, StoreError>;   // login, or `<slug>[bot]`
}
pub struct EnvToken; impl EnvToken { pub fn from_env() -> Result<Self, StoreError>; pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, StoreError>; }
pub struct AppCredentials; impl AppCredentials {
    pub fn new(api: &str, app_id: u64, pem: &str, repo: &str) -> Result<Self, StoreError>;
    pub fn from_file(api: &str, app_id: u64, path: &Path, repo: &str) -> Result<Self, StoreError>;
    pub fn jwt(&self, now_unix: u64) -> Result<String, StoreError>;
}
// fake (feature "fake", and cfg(test))
pub struct FakeGithub; impl FakeGithub {
    pub fn start(full_name: &str) -> FakeGithub; pub fn url(&self) -> String;
    pub fn state(&self) -> std::sync::MutexGuard<'_, State>;
}
```

`Client` classifies every answer once: 2xx, 3xx, 404, 410 and 5xx come back as a `Reply` for the caller to judge; 401 is `Credential`; a rate-limit answer is `RateLimited`; any other 4xx is `Backend`; a transport failure is `Unreachable`.

- [ ] **Step 1: The crate and its dependencies**

Root `Cargo.toml`: add `"crates/github"` to `members`, and to `[workspace.dependencies]`:

```toml
ureq = { version = "3.4", features = ["json"] }
ring = "0.17"
base64 = "0.22"
tiny_http = "0.12"
```

`crates/github/Cargo.toml`:

```toml
[package]
name = "fl-github"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
version.workspace = true

[dependencies]
fl-core = { path = "../core" }
thiserror.workspace = true
serde.workspace = true
serde_json.workspace = true
uuid.workspace = true
ureq.workspace = true
ring.workspace = true
base64.workspace = true
tiny_http = { workspace = true, optional = true }

[features]
# The in-process fake GitHub, for this crate's tests and the CLI's.
fake = ["dep:tiny_http"]

[dev-dependencies]
fl-core = { path = "../core", features = ["conformance"] }
tiny_http.workspace = true
```

`crates/github/src/lib.rs`:

```rust
//! GitHub Issues as an fl tracker (GitHub tracker spec). The only crate
//! that talks to GitHub.

pub mod client;
pub mod creds;
#[cfg(any(test, feature = "fake"))]
pub mod fake;

pub use client::{Client, DEFAULT_API, Method, Reply};
pub use creds::{AppCredentials, Credentials, EnvToken};
```

- [ ] **Step 2: `client.rs`**

```rust
//! One blocking client for the REST and GraphQL endpoints fl uses. It
//! classifies every answer once, so no caller can read a failure as data.

use crate::creds::Credentials;
use fl_core::StoreError;
use serde_json::Value;
use std::time::Duration;

pub const DEFAULT_API: &str = "https://api.github.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
}

/// An answer that is not a transport failure and not already an error:
/// 2xx, 3xx, 404, 410 and 5xx. The caller decides what each means.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub location: Option<String>,
    link_next: Option<String>,
}

pub struct Client {
    agent: ureq::Agent,
    api: String,
    creds: Box<dyn Credentials>,
}

impl Client {
    pub fn new(api: &str, creds: Box<dyn Credentials>) -> Self {
        Self {
            agent: agent(),
            api: api.trim_end_matches('/').to_string(),
            creds,
        }
    }

    /// Who fl writes as, from the credential.
    pub fn describe(&self) -> String {
        self.creds.describe()
    }

    /// `path_or_url` is a path under the API (`/repos/…`) or a full URL
    /// GitHub handed back (`Link`, `Location`) — which must be on the API's
    /// own origin, because the credential goes wherever it points.
    pub fn send(
        &self,
        method: Method,
        path_or_url: &str,
        body: Option<&Value>,
    ) -> Result<Reply, StoreError> {
        let url = self.url(path_or_url)?;
        let token = self.creds.token()?;
        send(&self.agent, method, &url, &token, body, &self.api)
    }

    /// Every page of a list. ⚠ A failure on ANY page is an error, never a
    /// short list (spec §3.7).
    pub fn get_all(&self, path: &str) -> Result<Vec<Value>, StoreError> {
        let mut out = Vec::new();
        let mut next = Some(path.to_string());
        while let Some(page) = next {
            let reply = self.send(Method::Get, &page, None)?;
            if reply.status != 200 {
                return Err(StoreError::Backend(format!(
                    "GitHub answered {} to a page of {path}. A list with a missing page is \
                     not a list; retry",
                    reply.status
                )));
            }
            let Value::Array(items) = reply.body else {
                return Err(StoreError::Backend(format!(
                    "GitHub answered a page of {path} with something that is not a list"
                )));
            };
            out.extend(items);
            next = reply.link_next;
        }
        Ok(out)
    }

    /// One GraphQL query. An `errors` member is an error, never partial data.
    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value, StoreError> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let reply = self.send(Method::Post, "/graphql", Some(&body))?;
        if reply.status != 200 {
            return Err(StoreError::Backend(format!(
                "GitHub answered {} to a GraphQL query; retry",
                reply.status
            )));
        }
        // GitHub answers a lookup of a missing node with `null` data AND a
        // NOT_FOUND error: that is an answer, not a failure. A rate limit
        // arrives the same way, as a 200 with an error of type RATE_LIMITED.
        if let Some(errors) = reply
            .body
            .get("errors")
            .and_then(Value::as_array)
            .filter(|e| !e.is_empty())
        {
            let kinds: Vec<&str> = errors
                .iter()
                .map(|e| e.get("type").and_then(Value::as_str).unwrap_or(""))
                .collect();
            if kinds.contains(&"RATE_LIMITED") {
                return Err(StoreError::RateLimited {
                    reset: "GitHub's GraphQL limit resets (it did not say when)".into(),
                });
            }
            if !kinds.iter().all(|k| *k == "NOT_FOUND") {
                return Err(StoreError::Backend(format!(
                    "GitHub refused a GraphQL query: {}",
                    Value::Array(errors.clone())
                )));
            }
        }
        reply.body.get("data").cloned().ok_or_else(|| {
            StoreError::Backend("GitHub answered a GraphQL query with no `data`".into())
        })
    }

    /// Who GitHub says fl writes as (spec §5.4).
    pub fn identity(&self) -> Result<String, StoreError> {
        self.creds.identity(&self.api)
    }

    fn url(&self, path_or_url: &str) -> Result<String, StoreError> {
        if path_or_url.starts_with('/') {
            return Ok(format!("{}{path_or_url}", self.api));
        }
        if path_or_url.starts_with(&format!("{}/", self.api)) {
            return Ok(path_or_url.to_string());
        }
        Err(StoreError::Backend(format!(
            "refused to send the GitHub credential to {path_or_url}, which is not under {}",
            self.api
        )))
    }
}

/// No redirects are followed: a 301 is information (a renamed repository,
/// a transferred issue), and following one would carry the token along.
pub(crate) fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(30)))
        .build();
    ureq::Agent::new_with_config(config)
}

fn headers<B>(rb: ureq::RequestBuilder<B>, auth: &str) -> ureq::RequestBuilder<B> {
    rb.header("Authorization", auth)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", concat!("fl/", env!("CARGO_PKG_VERSION")))
}

/// One request with a bearer token. Shared by `Client` and the App's token
/// exchange, which signs with a JWT rather than a stored token.
pub(crate) fn send(
    agent: &ureq::Agent,
    method: Method,
    url: &str,
    token: &str,
    body: Option<&Value>,
    origin: &str,
) -> Result<Reply, StoreError> {
    let unreachable = |e: ureq::Error| StoreError::Unreachable {
        store: origin.to_string(),
        cause: e.to_string(),
    };
    let auth = format!("Bearer {token}");
    let result = match (method, body) {
        (Method::Get, _) => headers(agent.get(url), &auth).call(),
        (Method::Post, Some(b)) => headers(agent.post(url), &auth).send_json(b),
        (Method::Post, None) => headers(agent.post(url), &auth).send_empty(),
        (Method::Patch, Some(b)) => headers(agent.patch(url), &auth).send_json(b),
        (Method::Patch, None) => headers(agent.patch(url), &auth).send_empty(),
    };
    let mut resp = result.map_err(unreachable)?;
    let status = resp.status().as_u16();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let location = header("location");
    let link_next = header("link").and_then(|l| next_link(&l));
    let remaining = header("x-ratelimit-remaining");
    let reset = header("x-ratelimit-reset");
    let retry_after = header("retry-after");
    let text = resp.body_mut().read_to_string().map_err(unreachable)?;
    let body = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).map_err(|e| {
            StoreError::Backend(format!(
                "GitHub answered {method:?} {url} with a body that is not JSON ({e})"
            ))
        })?
    };
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let rate_limited =
        matches!(status, 403 | 429) && (remaining.as_deref() == Some("0") || retry_after.is_some());
    match status {
        _ if rate_limited => Err(StoreError::RateLimited {
            reset: match (reset, retry_after) {
                (Some(r), _) => format!("{r} (unix seconds)"),
                (None, Some(s)) => format!("{s} seconds from now"),
                (None, None) => "an unknown time".into(),
            },
        }),
        200..=399 | 404 | 410 | 500..=599 => Ok(Reply {
            status,
            body,
            location,
            link_next,
        }),
        401 => Err(StoreError::Credential(format!(
            "GitHub refused the credential ({message}). Check the credential the binding names"
        ))),
        _ => Err(StoreError::Backend(format!(
            "GitHub answered {status} to {method:?} {url}: {message}"
        ))),
    }
}

/// The `rel="next"` URL of a `Link` header, if any.
fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    #[test]
    fn next_link_reads_the_next_url_and_nothing_else() {
        let h = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#;
        assert_eq!(next_link(h).as_deref(), Some("https://api.github.com/x?page=2"));
        assert_eq!(next_link(r#"<https://api.github.com/x?page=1>; rel="prev""#), None);
    }

    #[test]
    fn the_credential_is_never_sent_off_the_api_origin() {
        let fake = FakeGithub::start("acme/widgets");
        let err = client(&fake)
            .send(Method::Get, "https://elsewhere.example/steal", None)
            .unwrap_err();
        assert!(err.to_string().contains("refused to send"), "{err}");
    }

    #[test]
    fn a_rate_limit_is_an_error_naming_the_reset() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().rate_limited = true;
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(
            matches!(err, StoreError::RateLimited { ref reset } if reset.contains("1700000000")),
            "{err:?}"
        );
    }

    #[test]
    fn a_server_error_is_a_reply_the_caller_must_judge() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().fail_repo_read = true;
        let r = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap();
        assert_eq!(r.status, 500);
    }

    #[test]
    fn a_server_that_is_not_there_is_unreachable() {
        let c = Client::new(
            "http://127.0.0.1:9",
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        );
        let err = c.send(Method::Get, "/repos/acme/widgets", None).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    #[test]
    fn a_graphql_not_found_is_an_answer_and_a_graphql_rate_limit_is_an_error() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let data = c
            .graphql("query($id: ID!) { node(id: $id) { ... on Issue { url } } }", serde_json::json!({"id": "I_404"}))
            .unwrap();
        assert!(data["node"].is_null());
        fake.state().graphql_rate_limited = true;
        let err = c
            .graphql("query($id: ID!) { node(id: $id) { ... on Issue { url } } }", serde_json::json!({"id": "I_404"}))
            .unwrap_err();
        assert!(matches!(err, StoreError::RateLimited { .. }), "{err:?}");
    }

    #[test]
    fn a_missing_page_fails_the_whole_list() {
        let fake = FakeGithub::start("acme/widgets");
        {
            let mut s = fake.state();
            s.max_per_page = 1;
            for name in ["a", "b", "c"] {
                s.labels.insert(name.into());
            }
            s.fail_page = Some(("/repos/acme/widgets/labels".into(), 2));
        }
        let err = client(&fake)
            .get_all("/repos/acme/widgets/labels?per_page=100")
            .unwrap_err();
        assert!(err.to_string().contains("missing page"), "{err}");
        fake.state().fail_page = None;
        let all = client(&fake)
            .get_all("/repos/acme/widgets/labels?per_page=100")
            .unwrap();
        assert_eq!(all.len(), 3, "every page, followed");
    }
}
```

- [ ] **Step 3: `creds.rs`**

```rust
//! Where a bearer token comes from (spec §5). One named source per binding,
//! and no fallback from one to the other.

use crate::client::{Method, agent, send};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use fl_core::StoreError;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::Value;
use std::cell::RefCell;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub trait Credentials {
    /// A bearer token for the next request.
    fn token(&self) -> Result<String, StoreError>;
    /// Where the credential comes from, for `fl github whoami`. Never the
    /// secret itself.
    fn describe(&self) -> String;
    /// Who GitHub says fl writes as: a user's login, or an App's
    /// `<slug>[bot]` (spec §5.4).
    fn identity(&self, api: &str) -> Result<String, StoreError>;
}

fn text_field(v: &Value, k: &str, what: &str) -> Result<String, StoreError> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| StoreError::Credential(format!("GitHub's answer about {what} has no `{k}`")))
}

/// The environment variables `credential = "env"` reads, in order.
pub const ENV_VARS: [&str; 2] = ["FL_GITHUB_TOKEN", "GITHUB_TOKEN"];

/// A token from the environment (spec §5.3).
pub struct EnvToken {
    var: &'static str,
    token: String,
}

impl EnvToken {
    pub fn from_env() -> Result<Self, StoreError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, StoreError> {
        for var in ENV_VARS {
            if let Some(t) = get(var).filter(|t| !t.trim().is_empty()) {
                return Ok(Self {
                    var,
                    token: t.trim().to_string(),
                });
            }
        }
        Err(StoreError::Credential(format!(
            "`credential = \"env\"` reads {} from the environment, and neither is set",
            ENV_VARS.join(" or ")
        )))
    }
}

impl Credentials for EnvToken {
    fn token(&self) -> Result<String, StoreError> {
        Ok(self.token.clone())
    }
    fn describe(&self) -> String {
        format!("the token in ${}", self.var)
    }
    fn identity(&self, api: &str) -> Result<String, StoreError> {
        let r = send(&agent(), Method::Get, &format!("{api}/user"), &self.token, None, api)?;
        if r.status != 200 {
            return Err(StoreError::Credential(format!(
                "GitHub answered {} when fl asked whose token ${} is",
                r.status, self.var
            )));
        }
        text_field(&r.body, "login", "the token's user")
    }
}

/// An installation token lives an hour; renew with ten minutes to spare.
const TOKEN_LIFE: Duration = Duration::from_secs(50 * 60);

/// The GitHub App's installation token for one repository (spec §5.2).
pub struct AppCredentials {
    app_id: u64,
    key: RsaKeyPair,
    repo: String,
    api: String,
    agent: ureq::Agent,
    cached: RefCell<Option<(String, Instant)>>,
}

impl AppCredentials {
    pub fn new(api: &str, app_id: u64, pem: &str, repo: &str) -> Result<Self, StoreError> {
        Ok(Self {
            app_id,
            key: parse_pem(pem)?,
            repo: repo.to_string(),
            api: api.trim_end_matches('/').to_string(),
            agent: agent(),
            cached: RefCell::new(None),
        })
    }

    /// The key is read from a file the config names. ⚠ The path, never the
    /// key, may appear in a message.
    pub fn from_file(api: &str, app_id: u64, path: &Path, repo: &str) -> Result<Self, StoreError> {
        let pem = std::fs::read_to_string(path).map_err(|e| {
            StoreError::Credential(format!(
                "could not read the App private key at {}: {e}",
                path.display()
            ))
        })?;
        Self::new(api, app_id, &pem, repo)
    }

    /// A JWT signed with the App's key, valid for nine minutes (GitHub
    /// allows ten) and back-dated a minute for clock drift.
    pub fn jwt(&self, now_unix: u64) -> Result<String, StoreError> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = serde_json::json!({
            "iat": now_unix.saturating_sub(60),
            "exp": now_unix + 540,
            "iss": self.app_id.to_string(),
        });
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let input = format!("{header}.{payload}");
        let mut sig = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig)
            .map_err(|_| StoreError::Credential("signing the App's JWT failed".into()))?;
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
    }

    fn exchange(&self) -> Result<String, StoreError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StoreError::Credential("the system clock is before 1970".into()))?
            .as_secs();
        let jwt = self.jwt(now)?;
        let url = format!("{}/repos/{}/installation", self.api, self.repo);
        let mut found = send(&self.agent, Method::Get, &url, &jwt, None, &self.api)?;
        // After a rename GitHub redirects the old name. Follow ONE redirect,
        // and only on this API's own origin: the JWT goes where it points.
        if matches!(found.status, 301 | 302 | 307 | 308) {
            let to = found
                .location
                .clone()
                .filter(|l| l.starts_with(&format!("{}/", self.api)))
                .ok_or_else(|| {
                    StoreError::Credential(
                        "GitHub redirected the App's installation lookup off its own origin".into(),
                    )
                })?;
            found = send(&self.agent, Method::Get, &to, &jwt, None, &self.api)?;
        }
        if found.status == 404 {
            return Err(StoreError::Credential(format!(
                "the App {} is not installed on {}. Install it on that repository",
                self.app_id, self.repo
            )));
        }
        let id = (found.status == 200)
            .then(|| found.body.get("id").and_then(Value::as_u64))
            .flatten()
            .ok_or_else(|| {
                StoreError::Credential(format!(
                    "GitHub answered {} when fl looked for the App's installation on {}",
                    found.status, self.repo
                ))
            })?;
        let url = format!("{}/app/installations/{id}/access_tokens", self.api);
        let made = send(&self.agent, Method::Post, &url, &jwt, None, &self.api)?;
        let token = (made.status == 201)
            .then(|| made.body.get("token").and_then(Value::as_str))
            .flatten()
            .ok_or_else(|| {
                StoreError::Credential(format!(
                    "GitHub answered {} when fl asked for the App's installation token",
                    made.status
                ))
            })?;
        Ok(token.to_string())
    }
}

impl Credentials for AppCredentials {
    fn token(&self) -> Result<String, StoreError> {
        if let Some((t, at)) = self.cached.borrow().as_ref()
            && at.elapsed() < TOKEN_LIFE
        {
            return Ok(t.clone());
        }
        let t = self.exchange()?;
        *self.cached.borrow_mut() = Some((t.clone(), Instant::now()));
        Ok(t)
    }
    fn describe(&self) -> String {
        format!("GitHub App {} (installation token)", self.app_id)
    }
    fn identity(&self, api: &str) -> Result<String, StoreError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StoreError::Credential("the system clock is before 1970".into()))?
            .as_secs();
        let r = send(&self.agent, Method::Get, &format!("{api}/app"), &self.jwt(now)?, None, api)?;
        if r.status != 200 {
            return Err(StoreError::Credential(format!(
                "GitHub answered {} when fl asked which App {} is",
                r.status, self.app_id
            )));
        }
        Ok(format!("{}[bot]", text_field(&r.body, "slug", "the App")?))
    }
}

/// An RSA private key in PEM form: PKCS#1 (`BEGIN RSA PRIVATE KEY`, what
/// GitHub hands out) or PKCS#8 (`BEGIN PRIVATE KEY`).
fn parse_pem(pem: &str) -> Result<RsaKeyPair, StoreError> {
    let refuse = |why: &str| {
        StoreError::Credential(format!(
            "the App private key is not an RSA key in PEM form ({why}). Download a new key \
             from the App's settings"
        ))
    };
    let pkcs1 = pem.contains("-----BEGIN RSA PRIVATE KEY-----");
    if !pkcs1 && !pem.contains("-----BEGIN PRIVATE KEY-----") {
        return Err(refuse("it has no BEGIN line"));
    }
    let b64: String = pem
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let der = STANDARD.decode(b64).map_err(|e| refuse(&e.to_string()))?;
    let key = if pkcs1 {
        RsaKeyPair::from_der(&der)
    } else {
        RsaKeyPair::from_pkcs8(&der)
    };
    key.map_err(|e| refuse(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeGithub;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    /// A throwaway RSA key, made by the `openssl` CLI so no private key is
    /// ever committed. `traditional` selects PKCS#1 over PKCS#8.
    fn throwaway_key(traditional: bool) -> String {
        let mut args = vec!["genrsa"];
        if traditional {
            args.push("-traditional");
        }
        args.push("2048");
        let out = std::process::Command::new("openssl")
            .args(&args)
            .output()
            .expect("these tests need the `openssl` CLI to make a throwaway key");
        assert!(out.status.success(), "openssl genrsa failed");
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn the_environment_token_is_read_in_order_and_its_absence_is_refused() {
        let t = EnvToken::from_lookup(|k| match k {
            "FL_GITHUB_TOKEN" => Some("first".into()),
            "GITHUB_TOKEN" => Some("second".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(t.token().unwrap(), "first");
        assert!(t.describe().contains("FL_GITHUB_TOKEN"));
        let t = EnvToken::from_lookup(|k| (k == "GITHUB_TOKEN").then(|| "second".into())).unwrap();
        assert_eq!(t.token().unwrap(), "second");
        let err = EnvToken::from_lookup(|_| Some("  ".into())).err().unwrap();
        assert!(matches!(err, StoreError::Credential(ref m) if m.contains("GITHUB_TOKEN")), "{err:?}");
    }

    #[test]
    fn the_jwt_is_signed_with_the_apps_key_and_names_the_app() {
        for traditional in [true, false] {
            let pem = throwaway_key(traditional);
            let app = AppCredentials::new("http://127.0.0.1:9", 42, &pem, "acme/widgets").unwrap();
            let jwt = app.jwt(1_000_000).unwrap();
            let parts: Vec<&str> = jwt.split('.').collect();
            assert_eq!(parts.len(), 3);
            let claims: Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
            assert_eq!(claims["iss"], "42");
            assert_eq!(claims["iat"], 1_000_000 - 60);
            assert_eq!(claims["exp"], 1_000_000 + 540);
            let key = parse_pem(&pem).unwrap();
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref())
                .verify(
                    format!("{}.{}", parts[0], parts[1]).as_bytes(),
                    &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
                )
                .expect("the signature verifies against the App's public key");
        }
    }

    #[test]
    fn a_key_that_is_not_pem_is_refused_without_echoing_it() {
        let err = AppCredentials::new("http://x", 1, "not a key at all", "a/b").err().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("not an RSA key"), "{msg}");
        assert!(!msg.contains("not a key at all"), "the input must not be echoed: {msg}");
    }

    #[test]
    fn the_installation_token_is_fetched_once_and_reused() {
        let fake = FakeGithub::start("acme/widgets");
        let app = AppCredentials::new(&fake.url(), 42, &throwaway_key(true), "acme/widgets").unwrap();
        assert_eq!(app.token().unwrap(), crate::fake::INSTALLATION_TOKEN);
        assert_eq!(app.token().unwrap(), crate::fake::INSTALLATION_TOKEN);
        assert_eq!(fake.state().token_requests, 1);
    }

    #[test]
    fn identity_names_the_user_or_the_apps_bot() {
        let fake = FakeGithub::start("acme/widgets");
        let env = EnvToken::from_lookup(|_| Some("t".into())).unwrap();
        assert_eq!(env.identity(&fake.url()).unwrap(), crate::fake::USER_LOGIN);
        let app = AppCredentials::new(&fake.url(), 42, &throwaway_key(true), "acme/widgets").unwrap();
        assert_eq!(app.identity(&fake.url()).unwrap(), format!("{}[bot]", crate::fake::APP_SLUG));
    }

    #[test]
    fn the_app_still_finds_its_installation_after_a_rename() {
        let fake = FakeGithub::start("acme/widgets");
        fake.rename("acme/gadgets");
        let app = AppCredentials::new(&fake.url(), 42, &throwaway_key(true), "acme/widgets").unwrap();
        assert_eq!(app.token().unwrap(), crate::fake::INSTALLATION_TOKEN);
    }

    #[test]
    fn an_app_not_installed_on_the_repository_is_named_as_such() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().installations.clear();
        let app = AppCredentials::new(&fake.url(), 42, &throwaway_key(true), "acme/widgets").unwrap();
        let err = app.token().unwrap_err();
        assert!(err.to_string().contains("not installed on acme/widgets"), "{err}");
    }
}
```

`if let … && …` is a let-chain (edition 2024). If the compiler refuses it on this toolchain, write the equivalent nested `if`.

- [ ] **Step 4: `fake.rs` — the base**

This task builds the fake's server, its state, and the repository, label, App and rate-limit routes. Tasks 4–7 add the issue, timeline, GraphQL and comment routes to the same `route` function.

```rust
//! An in-process fake of the GitHub endpoints fl uses.
//!
//! ⚠ It proves structure, not integration (spec §8.4): it agrees with fl
//! because both were written from the same reading of GitHub's docs. The
//! live tests are the check against GitHub itself.

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

pub const INSTALLATION_TOKEN: &str = "fake-installation-token";
pub const USER_LOGIN: &str = "fake-user";
pub const APP_SLUG: &str = "fake-app";

#[derive(Debug, Clone)]
pub struct Repo {
    pub id: u64,
    pub node_id: String,
    pub full_name: String,
    pub visibility: String,
}

#[derive(Debug, Clone, Default)]
pub struct Issue {
    pub number: u64,
    pub node_id: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub state: String,
    pub state_reason: Option<String>,
    pub pull_request: bool,
    pub gone: bool,
    pub moved_to: Option<String>,
    /// (id, kind) of every timeline event, oldest first.
    pub events: Vec<(u64, String)>,
    /// Ids of the body's edit history, oldest first.
    pub edits: Vec<String>,
    pub comments: Vec<String>,
}

/// Everything the fake holds, and the knobs a test turns. Every knob is
/// one-shot or explicit; none is on by default.
#[derive(Debug, Default)]
pub struct State {
    pub base: String,
    pub repos: Vec<Repo>,
    /// Lowercase old name → repository id: GitHub's redirect after a rename.
    pub redirects: BTreeMap<String, u64>,
    pub labels: BTreeSet<String>,
    pub issues: BTreeMap<u64, Issue>,
    pub next_number: u64,
    pub next_event: u64,
    pub installations: BTreeMap<String, u64>,
    pub token_requests: u64,
    /// "METHOD /path?query" of every request, in order.
    pub requests: Vec<String>,
    /// Largest page the fake serves, whatever `per_page` asks. 0 = 100.
    pub max_per_page: usize,
    /// (path prefix, page number): that page answers 500.
    pub fail_page: Option<(String, u32)>,
    pub rate_limited: bool,
    pub graphql_rate_limited: bool,
    pub fail_repo_read: bool,
    pub drop_labels: bool,
    pub fail_after_create: bool,
    /// The create lands, then the connection breaks mid-answer.
    pub hang_up_after_create: bool,
    /// The create answers 502 and nothing lands.
    pub fail_before_create: bool,
    pub foreign_label_on_next_patch: bool,
    pub foreign_edit_on_next_patch: bool,
}

pub struct FakeGithub {
    server: Arc<tiny_http::Server>,
    thread: Option<JoinHandle<()>>,
    state: Arc<Mutex<State>>,
    url: String,
}

impl FakeGithub {
    /// A fake whose one repository is `full_name`, private, with the App
    /// installed on it.
    pub fn start(full_name: &str) -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind a local port"));
        let port = server.server_addr().to_ip().expect("an IP listener").port();
        let url = format!("http://127.0.0.1:{port}");
        let state = Arc::new(Mutex::new(State {
            base: url.clone(),
            repos: vec![Repo {
                id: 1,
                node_id: "R_1".into(),
                full_name: full_name.into(),
                visibility: "private".into(),
            }],
            installations: BTreeMap::from([(full_name.to_ascii_lowercase(), 7)]),
            next_number: 1,
            next_event: 1,
            ..State::default()
        }));
        let (srv, st) = (Arc::clone(&server), Arc::clone(&state));
        let thread = std::thread::spawn(move || {
            while let Ok(mut req) = srv.recv() {
                let method = req.method().to_string();
                let url = req.url().to_string();
                let auth = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .map(|h| h.value.to_string())
                    .unwrap_or_default();
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
                let answer = route(&mut st.lock().unwrap(), &method, &url, &auth, &body);
                if answer.hang_up {
                    // A broken answer after the server acted: the client
                    // fails at once with a transport error. (Dropping the
                    // request unanswered would make tiny_http answer 500;
                    // a short body would hang the client until its timeout.)
                    let mut w = req.into_writer();
                    let _ = std::io::Write::write_all(&mut w, b"HTTP/9 broken\r\n\r\n");
                    continue;
                }
                let mut resp = tiny_http::Response::from_string(answer.body.to_string())
                    .with_status_code(answer.status)
                    .with_header(header("Content-Type", "application/json"));
                for (k, v) in answer.headers {
                    resp = resp.with_header(header(&k, &v));
                }
                let _ = req.respond(resp);
            }
        });
        Self {
            server,
            thread: Some(thread),
            state,
            url,
        }
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
}

impl Drop for FakeGithub {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn header(k: &str, v: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("an ASCII header")
}

pub(crate) struct Answer {
    status: u16,
    body: Value,
    headers: Vec<(String, String)>,
    hang_up: bool,
}

fn answer(status: u16, body: Value) -> Answer {
    Answer {
        status,
        body,
        headers: vec![],
        hang_up: false,
    }
}

/// `path?query` → (path, query map).
fn split(url: &str) -> (String, BTreeMap<String, String>) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let q = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    (path.to_string(), q)
}

impl State {
    fn repo_named(&self, name: &str) -> Option<&Repo> {
        self.repos
            .iter()
            .find(|r| r.full_name.eq_ignore_ascii_case(name))
    }

    fn repo_json(&self, r: &Repo) -> Value {
        json!({
            "id": r.id, "node_id": r.node_id, "full_name": r.full_name,
            "visibility": r.visibility, "private": r.visibility == "private",
        })
    }

    /// One page of `items`, with a `Link` header when more remain.
    fn page(&self, path: &str, q: &BTreeMap<String, String>, items: Vec<Value>) -> Answer {
        let asked: usize = q.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(30);
        let cap = if self.max_per_page == 0 { 100 } else { self.max_per_page };
        let size = asked.min(cap).max(1);
        let page: u32 = q.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
        if let Some((prefix, bad)) = &self.fail_page
            && path.starts_with(prefix.as_str())
            && *bad == page
        {
            return answer(500, json!({"message": "fake page failure"}));
        }
        let start = (page as usize - 1) * size;
        let chunk: Vec<Value> = items.iter().skip(start).take(size).cloned().collect();
        let mut a = answer(200, Value::Array(chunk));
        if start + size < items.len() {
            let mut next = q.clone();
            next.insert("page".into(), (page + 1).to_string());
            let query: Vec<String> = next.iter().map(|(k, v)| format!("{k}={v}")).collect();
            a.headers.push((
                "Link".into(),
                format!("<{}{path}?{}>; rel=\"next\"", self.base, query.join("&")),
            ));
        }
        a
    }
}

/// The installation of the repository with id `repo`, keyed by the name it
/// was installed under — which a rename does not change.
fn installation(s: &State, repo: Option<u64>) -> Answer {
    let installed = repo.and_then(|id| {
        s.installations
            .iter()
            .find(|(name, _)| {
                s.repos.iter().any(|r| r.id == id && r.full_name.eq_ignore_ascii_case(name))
                    || s.redirects.get(*name) == Some(&id)
            })
            .map(|(_, inst)| *inst)
    });
    match installed {
        Some(inst) => answer(200, json!({"id": inst})),
        None => answer(404, json!({"message": "Not Found"})),
    }
}

/// Every route the fake serves. Later tasks add arms above the final `_`.
pub(crate) fn route(s: &mut State, method: &str, url: &str, auth: &str, body: &str) -> Answer {
    s.requests.push(format!("{method} {url}"));
    if method == "POST" && url == "/graphql" && std::mem::take(&mut s.graphql_rate_limited) {
        return answer(200, json!({"data": null, "errors": [{"type": "RATE_LIMITED"}]}));
    }
    if s.rate_limited {
        s.rate_limited = false;
        let mut a = answer(403, json!({"message": "API rate limit exceeded"}));
        a.headers.push(("x-ratelimit-remaining".into(), "0".into()));
        a.headers.push(("x-ratelimit-reset".into(), "1700000000".into()));
        return a;
    }
    let (path, q) = split(url);
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, parts.as_slice()) {
        ("GET", ["repos", o, r]) => {
            if s.fail_repo_read {
                s.fail_repo_read = false;
                return answer(500, json!({"message": "fake repository failure"}));
            }
            let name = format!("{o}/{r}");
            if let Some(repo) = s.repo_named(&name) {
                return answer(200, s.repo_json(repo));
            }
            match s.redirects.get(&name.to_ascii_lowercase()) {
                Some(id) => {
                    let mut a = answer(301, json!({"message": "Moved Permanently"}));
                    a.headers.push(("Location".into(), format!("{}/repositories/{id}", s.base)));
                    a
                }
                None => answer(404, json!({"message": "Not Found"})),
            }
        }
        ("GET", ["repositories", id]) => match s.repos.iter().find(|r| r.id.to_string() == *id) {
            Some(repo) => answer(200, s.repo_json(repo)),
            None => answer(404, json!({"message": "Not Found"})),
        },
        ("GET", ["repos", o, r, "installation"]) => {
            // A JWT is three dot-separated parts; the signature is checked
            // by the unit test, not here.
            if auth.trim_start_matches("Bearer ").split('.').count() != 3 {
                return answer(401, json!({"message": "A JSON web token could not be decoded"}));
            }
            let name = format!("{o}/{r}").to_ascii_lowercase();
            if s.repo_named(&name).is_none()
                && let Some(id) = s.redirects.get(&name)
            {
                let mut a = answer(301, json!({"message": "Moved Permanently"}));
                a.headers.push(("Location".into(), format!("{}/repositories/{id}/installation", s.base)));
                return a;
            }
            let current = s.repo_named(&name).map(|r| r.id);
            installation(s, current)
        }
        ("GET", ["repositories", id, "installation"]) => {
            let current = s.repos.iter().find(|r| r.id.to_string() == *id).map(|r| r.id);
            installation(s, current)
        }
        ("GET", ["user"]) => answer(200, json!({"login": USER_LOGIN})),
        ("GET", ["app"]) => {
            if auth.trim_start_matches("Bearer ").split('.').count() != 3 {
                return answer(401, json!({"message": "A JSON web token could not be decoded"}));
            }
            answer(200, json!({"slug": APP_SLUG}))
        }
        ("POST", ["app", "installations", _, "access_tokens"]) => {
            s.token_requests += 1;
            answer(201, json!({"token": INSTALLATION_TOKEN, "expires_at": "2099-01-01T00:00:00Z"}))
        }
        ("GET", ["repos", _, _, "labels"]) => {
            let items = s.labels.iter().map(|l| json!({"name": l})).collect();
            s.page(&path, &q, items)
        }
        ("POST", ["repos", _, _, "labels"]) => {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            match v.get("name").and_then(Value::as_str) {
                Some(n) => {
                    s.labels.insert(n.to_string());
                    answer(201, json!({"name": n}))
                }
                None => answer(422, json!({"message": "name is missing"})),
            }
        }
        _ => answer(404, json!({"message": format!("the fake does not serve {method} {path}")})),
    }
}
```

`tiny_http`'s `HeaderField::equiv` compares a header name without regard to case. If the compiler names a different method, use the one it offers and say so in the report.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p fl-github`
Expected: PASS — 7 client tests and 7 credential tests. (`the_app_still_finds_its_installation_after_a_rename` and the GraphQL test need Task 5's `rename` and graphql route: write both fake pieces now, in this task — `rename` from Task 5 Step 1, and a `("POST", ["graphql"])` arm that answers `{"data": {"node": null}, "errors": [{"type": "NOT_FOUND"}]}` for an unknown node id — and let Tasks 5 and 6 extend them.) There is no red step for the new crate as a whole; Step 6 supplies it for each guard.

- [ ] **Step 6: Mutation checks**

One at a time, confirm red, restore:
- In `Client::url`, accept any URL → `the_credential_is_never_sent…` FAILS.
- In `get_all`, `break` on a non-200 page instead of returning the error → `a_missing_page_fails…` FAILS.
- Remove the `rate_limited` arm → `a_rate_limit_is_an_error…` FAILS.
- Make `token()` skip the cache → `the_installation_token_is_fetched_once…` FAILS.
- Remove the redirect-following block in `exchange` → `the_app_still_finds_its_installation…` FAILS.
- Treat NOT_FOUND as an error in `graphql` → `a_graphql_not_found_is_an_answer…` FAILS.

- [ ] **Step 7: Licences, trio, commit**

List the licence of every crate this task adds to `Cargo.lock` and name any that is not MIT, Apache-2.0, ISC, BSD or Zlib in the report.

```bash
git add Cargo.toml Cargo.lock crates/github
git commit -m "feat(github): the client, the credentials, and an in-process fake

A blocking client that classifies every answer once, follows every page
or fails, and never sends the credential off the API origin; an
environment token and a GitHub App (RS256 JWT via ring, installation
token cached below its lifetime); and a fake GitHub for tests. New
dependencies: ureq, ring, base64, tiny_http (licences in the PR).
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 3: `meta` — how an item lives in an issue

**Files:**
- Create: `crates/github/src/meta.rs`
- Modify: `crates/github/src/lib.rs` (`pub mod meta;`)

**Interfaces:**
- Produces (pure; no IO):

```rust
pub const FL_FORMAT: u64 = 1;
pub const META_OPEN: &str = "<!-- fl:meta";
pub const META_CLOSE: &str = "-->";
pub const TITLE_MAX: usize = 256;
pub enum ItemKind { Record, Finding }            // ALL, as_kind, from_kind, as_wire, states, valid_state
pub struct RecordRef { pub id: Iri, pub node_id: String }
pub struct Meta { fl_format, kind, state, project, record, reproduction, raised_by, assigned_to,
                  security, withdrawn_reason, also_known_as, create_key }   // Meta::new(kind, state, project)
pub struct IssueView { pub number, pub url: Iri, pub node_id, pub title, pub body, pub labels: Vec<String>,
                       pub state: String, pub state_reason: Option<String>, pub is_pull_request: bool }
pub enum BodyError { Missing, Damaged(String), UnknownFormat(u64) }
pub enum Read { NotFl(String), Item { kind: ItemKind, meta: Meta, prose: String } }
pub fn kind_label(ItemKind) -> String;          pub fn state_label(ItemKind, &str) -> String;
pub fn all_labels() -> Vec<String>;              pub fn labels_after(&[String], ItemKind, &str) -> Vec<String>;
pub fn projection(ItemKind, &str) -> (&'static str, Option<&'static str>);
pub fn render_body(prose: &str, meta: &Meta) -> String;
pub fn parse_body(body: &str) -> Result<(String, Meta), BodyError>;
pub fn read_item(issue: &IssueView) -> Result<Read, StoreError>;
pub fn title_of(claim: &str) -> String;
pub fn parse_issue_url(id: &Iri) -> Option<(String, u64)>;   pub fn is_issue_url(id: &Iri) -> bool;
```

- [ ] **Step 1: Write the module with its tests**

Create `crates/github/src/meta.rs`:

```rust
//! How an fl item lives in a GitHub issue (GitHub tracker spec §3): one kind
//! label, one state label, and a metadata block at the end of the body.
//! Open or closed is a projection fl writes and never reads state from.
//!
//! Pure: no IO. `GithubTracker` does the talking.

use fl_core::finding::FindingState;
use fl_core::ids::{GateId, Kind, ProjectId};
use fl_core::iri::Iri;
use fl_core::model::State;
use fl_core::store::StoreError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FL_FORMAT: u64 = 1;
pub const META_OPEN: &str = "<!-- fl:meta";
pub const META_CLOSE: &str = "-->";
/// GitHub limits an issue title to 256 characters.
pub const TITLE_MAX: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Record,
    Finding,
}

impl ItemKind {
    pub const ALL: [ItemKind; 2] = [ItemKind::Record, ItemKind::Finding];

    pub fn as_kind(self) -> Kind {
        match self {
            ItemKind::Record => Kind::Record,
            ItemKind::Finding => Kind::Finding,
        }
    }

    pub fn from_kind(kind: Kind) -> Option<Self> {
        match kind {
            Kind::Record => Some(ItemKind::Record),
            Kind::Finding => Some(ItemKind::Finding),
            Kind::Project | Kind::Gate => None,
        }
    }

    pub fn as_wire(self) -> &'static str {
        self.as_kind().as_wire()
    }

    /// Every state of this kind, from the core enums — never a hand list.
    pub fn states(self) -> Vec<&'static str> {
        match self {
            ItemKind::Record => State::ALL.iter().map(|s| s.as_wire()).collect(),
            ItemKind::Finding => FindingState::ALL.iter().map(|s| s.as_wire()).collect(),
        }
    }

    pub fn valid_state(self, state: &str) -> bool {
        self.states().contains(&state)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRef {
    pub id: Iri,
    pub node_id: String,
}

/// The fields a label cannot hold (spec §3.1). ⚠ `deny_unknown_fields`: a
/// block with a field this fl does not know is damaged, not half-read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub fl_format: u64,
    pub kind: ItemKind,
    pub state: String,
    pub project: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<RecordRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reproduction: Option<GateId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raised_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_to: Option<String>,
    #[serde(default)]
    pub security: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawn_reason: Option<String>,
    #[serde(default)]
    pub also_known_as: Vec<Iri>,
    /// Minted by fl for each create, so a retry can find what an ambiguous
    /// failure may already have made (spec §3.3).
    pub create_key: String,
}

impl Meta {
    pub fn new(kind: ItemKind, state: &str, project: ProjectId) -> Self {
        Self {
            fl_format: FL_FORMAT,
            kind,
            state: state.to_string(),
            project,
            record: None,
            reproduction: None,
            raised_by: None,
            assigned_to: None,
            security: false,
            withdrawn_reason: None,
            also_known_as: vec![],
            create_key: format!("urn:uuid:{}", uuid::Uuid::now_v7()),
        }
    }
}

/// The parts of a GitHub issue fl reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueView {
    pub number: u64,
    pub url: Iri,
    pub node_id: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub state: String,
    pub state_reason: Option<String>,
    pub is_pull_request: bool,
}

impl IssueView {
    pub fn from_json(v: &Value) -> Result<Self, StoreError> {
        let text = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| StoreError::Backend(format!("GitHub sent an issue without `{k}`")))
        };
        let url = Iri::parse(&text("html_url")?)
            .map_err(|e| StoreError::Backend(format!("GitHub sent an issue URL fl cannot use: {e}")))?;
        Ok(Self {
            number: v
                .get("number")
                .and_then(Value::as_u64)
                .ok_or_else(|| StoreError::Backend("GitHub sent an issue without `number`".into()))?,
            url,
            node_id: text("node_id")?,
            title: text("title")?,
            // GitHub sends `null` for an empty body.
            body: v.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
            labels: v
                .get("labels")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            state: text("state")?,
            state_reason: v.get("state_reason").and_then(Value::as_str).map(str::to_string),
            is_pull_request: v.get("pull_request").is_some_and(|p| !p.is_null()),
        })
    }
}

pub fn kind_label(kind: ItemKind) -> String {
    format!("fl:{}", kind.as_wire())
}

pub fn state_label(kind: ItemKind, state: &str) -> String {
    format!("fl:{}/{state}", kind.as_wire())
}

/// Every label fl may set, enumerated from the kinds and their states.
pub fn all_labels() -> Vec<String> {
    let mut out = Vec::new();
    for kind in ItemKind::ALL {
        out.push(kind_label(kind));
        for s in kind.states() {
            out.push(state_label(kind, s));
        }
    }
    out
}

/// An issue's labels after fl writes it: every label that is not fl's, then
/// this kind's two (spec §3.3 — fl replaces only its own).
pub fn labels_after(current: &[String], kind: ItemKind, state: &str) -> Vec<String> {
    let mut out: Vec<String> = current
        .iter()
        .filter(|l| !l.starts_with("fl:"))
        .cloned()
        .collect();
    out.push(kind_label(kind));
    out.push(state_label(kind, state));
    out
}

/// Open or closed, and why (spec §3.2).
pub fn projection(kind: ItemKind, state: &str) -> (&'static str, Option<&'static str>) {
    let completed = match kind {
        ItemKind::Record => state == State::Done.as_wire(),
        ItemKind::Finding => state == FindingState::Fixed.as_wire(),
    };
    if completed {
        return ("closed", Some("completed"));
    }
    if kind == ItemKind::Finding && state == FindingState::Withdrawn.as_wire() {
        return ("closed", Some("not_planned"));
    }
    ("open", None)
}

/// The prose, then the block. ⚠ `<` and `>` are escaped inside the JSON so
/// no field value can end the HTML comment or open a second block. They
/// occur only inside JSON strings, where `<`/`>` are the same text.
pub fn render_body(prose: &str, meta: &Meta) -> String {
    let json = serde_json::to_string(meta)
        .expect("a Meta always serializes")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    let block = format!("{META_OPEN}\n{json}\n{META_CLOSE}");
    if prose.is_empty() {
        format!("{block}\n")
    } else {
        format!("{prose}\n\n{block}\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyError {
    Missing,
    Damaged(String),
    UnknownFormat(u64),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyError::Missing => write!(f, "has no fl block"),
            BodyError::Damaged(why) => write!(f, "has a damaged fl block ({why})"),
            BodyError::UnknownFormat(n) => write!(
                f,
                "has an fl block of format {n}, and this fl reads format {FL_FORMAT}"
            ),
        }
    }
}

/// The prose and the block. The block is the LAST opener in the body: fl
/// always writes it last, and anything before it — including a claim that
/// quotes the opener — is prose. ⚠ Searching from the front would let one
/// claim make its issue, and every list over the repository, unreadable.
pub fn parse_body(body: &str) -> Result<(String, Meta), BodyError> {
    let body = body.replace("\r\n", "\n");
    let Some(at) = body.rfind(META_OPEN) else {
        return Err(BodyError::Missing);
    };
    let rest = &body[at + META_OPEN.len()..];
    let Some(end) = rest.find(META_CLOSE) else {
        return Err(BodyError::Damaged("the comment is never closed".into()));
    };
    let loose: Value = serde_json::from_str(rest[..end].trim())
        .map_err(|e| BodyError::Damaged(e.to_string()))?;
    match loose.get("fl_format").and_then(Value::as_u64) {
        Some(FL_FORMAT) => {}
        Some(n) => return Err(BodyError::UnknownFormat(n)),
        None => return Err(BodyError::Damaged("it has no `fl_format`".into())),
    }
    let meta: Meta =
        serde_json::from_value(loose).map_err(|e| BodyError::Damaged(e.to_string()))?;
    if !rest[end + META_CLOSE.len()..].trim().is_empty() {
        return Err(BodyError::Damaged("text follows the block".into()));
    }
    Ok((body[..at].trim_end().to_string(), meta))
}

/// What an issue is to fl.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // one value per read; boxing buys nothing
pub enum Read {
    /// Not fl's: a pull request, or an issue with no `fl:` label (spec §3.5).
    NotFl(String),
    Item {
        kind: ItemKind,
        meta: Meta,
        prose: String,
    },
}

/// Read an issue, comparing the labels, the block and the open/closed
/// status. ⚠ Any disagreement is `Diverged`, naming every value found
/// (spec §3.4); fl adopts neither side.
pub fn read_item(issue: &IssueView) -> Result<Read, StoreError> {
    if issue.is_pull_request {
        return Ok(Read::NotFl("a pull request".into()));
    }
    let fl: Vec<&str> = issue
        .labels
        .iter()
        .map(String::as_str)
        .filter(|l| l.starts_with("fl:"))
        .collect();
    if fl.is_empty() {
        return Ok(Read::NotFl("a GitHub issue with no fl label".into()));
    }
    let diverged = |detail: String| StoreError::Diverged {
        id: issue.url.clone(),
        detail,
    };
    let kinds: Vec<ItemKind> = ItemKind::ALL
        .into_iter()
        .filter(|k| fl.contains(&kind_label(*k).as_str()))
        .collect();
    let [kind] = kinds.as_slice() else {
        return Err(diverged(format!(
            "it carries {} fl kind labels ({fl:?}), not one",
            kinds.len()
        )));
    };
    let kind = *kind;
    let prefix = format!("{}/", kind_label(kind));
    let states: Vec<&str> = fl.iter().filter_map(|l| l.strip_prefix(prefix.as_str())).collect();
    let [label_state] = states.as_slice() else {
        return Err(diverged(format!(
            "it carries {} {} state labels ({fl:?}), not one",
            states.len(),
            kind.as_wire()
        )));
    };
    let (prose, meta) = parse_body(&issue.body).map_err(|e| diverged(format!("its body {e}")))?;
    let mut problems = Vec::new();
    if meta.kind != kind {
        problems.push(format!(
            "the label says {} but the block says {}",
            kind.as_wire(),
            meta.kind.as_wire()
        ));
    }
    if meta.state != *label_state {
        problems.push(format!(
            "the label says `{label_state}` but the block says `{}`",
            meta.state
        ));
    }
    if !kind.valid_state(&meta.state) {
        problems.push(format!("`{}` is not a {} state", meta.state, kind.as_wire()));
    }
    let (want, _) = projection(kind, &meta.state);
    if issue.state != want {
        problems.push(format!(
            "the issue is {} but the block's state `{}` means {want}",
            issue.state, meta.state
        ));
    }
    let own = kind_label(kind);
    let stray: Vec<&&str> = fl
        .iter()
        .filter(|l| **l != own && !l.starts_with(prefix.as_str()))
        .collect();
    if !stray.is_empty() {
        problems.push(format!("it also carries {stray:?}"));
    }
    if problems.is_empty() {
        Ok(Read::Item { kind, meta, prose })
    } else {
        Err(diverged(problems.join("; ")))
    }
}

/// A finding's title: the claim's first line, cut to GitHub's limit. The
/// whole claim stays in the body.
pub fn title_of(claim: &str) -> String {
    let first = claim.lines().next().unwrap_or("").trim();
    let first = if first.is_empty() { "(finding)" } else { first };
    if first.chars().count() <= TITLE_MAX {
        first.to_string()
    } else {
        let mut t: String = first.chars().take(TITLE_MAX - 1).collect();
        t.push('…');
        t
    }
}

/// `https://github.com/{owner}/{repo}/issues/{n}` → (`owner/repo`, n).
pub fn parse_issue_url(id: &Iri) -> Option<(String, u64)> {
    let rest = id.as_str().strip_prefix("https://github.com/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let [owner, repo, "issues", n] = parts.as_slice() else {
        return None;
    };
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((format!("{owner}/{repo}"), n.parse().ok()?))
}

pub fn is_issue_url(id: &Iri) -> bool {
    parse_issue_url(id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ids::seq_iri;

    fn meta(kind: ItemKind, state: &str) -> Meta {
        Meta::new(kind, state, ProjectId(seq_iri(1)))
    }

    fn issue(labels: &[&str], state: &str, body: &str) -> IssueView {
        IssueView {
            number: 1,
            url: Iri::parse("https://github.com/acme/widgets/issues/1").unwrap(),
            node_id: "I_1".into(),
            title: "t".into(),
            body: body.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            state: state.into(),
            state_reason: None,
            is_pull_request: false,
        }
    }

    #[test]
    fn a_body_round_trips_its_prose_and_its_block() {
        let mut m = meta(ItemKind::Finding, "raised");
        m.raised_by = Some("rev".into());
        let body = render_body("the claim\nsecond line", &m);
        assert_eq!(parse_body(&body).unwrap(), ("the claim\nsecond line".into(), m));
    }

    #[test]
    fn a_value_containing_comment_markers_cannot_break_the_block() {
        let mut m = meta(ItemKind::Finding, "withdrawn");
        m.withdrawn_reason = Some("ends early --> and <!-- fl:meta again".into());
        let body = render_body("prose with --> in it", &m);
        let (prose, back) = parse_body(&body).unwrap();
        assert_eq!(prose, "prose with --> in it");
        assert_eq!(back, m);
    }

    /// Review Focus 3: a claim that quotes the block's opener is prose.
    #[test]
    fn a_claim_quoting_the_opener_is_prose_and_the_block_still_reads() {
        let m = meta(ItemKind::Finding, "raised");
        let claim = "the parser breaks on <!-- fl:meta\n{\"x\":1}\n--> in a claim";
        let (prose, back) = parse_body(&render_body(claim, &m)).unwrap();
        assert_eq!((prose.as_str(), back), (claim, m));
    }

    #[test]
    fn a_body_that_is_missing_damaged_or_newer_is_named() {
        let good = render_body("p", &meta(ItemKind::Record, "todo"));
        assert_eq!(parse_body("just prose"), Err(BodyError::Missing));
        assert!(matches!(parse_body(&good.replace("\"kind\"", "\"kin\"")), Err(BodyError::Damaged(_))));
        assert!(matches!(parse_body(&format!("{good}\nmore")), Err(BodyError::Damaged(_))));
        assert_eq!(
            parse_body(&good.replace("\"fl_format\":1", "\"fl_format\":2")),
            Err(BodyError::UnknownFormat(2))
        );
    }

    #[test]
    fn a_consistent_issue_reads_as_its_item() {
        let m = meta(ItemKind::Record, "done");
        let i = issue(&["bug", "fl:record", "fl:record/done"], "closed", &render_body("", &m));
        assert!(matches!(read_item(&i).unwrap(), Read::Item { kind: ItemKind::Record, .. }));
    }

    #[test]
    fn a_label_that_disagrees_with_the_block_is_diverged_naming_both() {
        let m = meta(ItemKind::Record, "doing");
        let i = issue(&["fl:record", "fl:record/review"], "open", &render_body("", &m));
        let err = read_item(&i).unwrap_err().to_string();
        assert!(err.contains("`review`") && err.contains("`doing`"), "{err}");
    }

    #[test]
    fn two_state_labels_or_a_status_that_disagrees_are_diverged() {
        let m = meta(ItemKind::Record, "doing");
        let two = issue(&["fl:record", "fl:record/doing", "fl:record/done"], "open", &render_body("", &m));
        assert!(matches!(read_item(&two), Err(StoreError::Diverged { .. })));
        let closed = issue(&["fl:record", "fl:record/doing"], "closed", &render_body("", &m));
        let err = read_item(&closed).unwrap_err().to_string();
        assert!(err.contains("closed") && err.contains("open"), "{err}");
    }

    #[test]
    fn a_pull_request_or_an_unlabelled_issue_is_not_fls() {
        let mut pr = issue(&["fl:record", "fl:record/todo"], "open", "");
        pr.is_pull_request = true;
        assert_eq!(read_item(&pr).unwrap(), Read::NotFl("a pull request".into()));
        assert!(matches!(read_item(&issue(&["bug"], "open", "")).unwrap(), Read::NotFl(_)));
    }

    #[test]
    fn the_projection_closes_exactly_the_terminal_states() {
        assert_eq!(projection(ItemKind::Record, "done"), ("closed", Some("completed")));
        assert_eq!(projection(ItemKind::Finding, "fixed"), ("closed", Some("completed")));
        assert_eq!(projection(ItemKind::Finding, "withdrawn"), ("closed", Some("not_planned")));
        for s in ItemKind::Record.states().into_iter().filter(|s| *s != "done") {
            assert_eq!(projection(ItemKind::Record, s), ("open", None), "{s}");
        }
    }

    #[test]
    fn every_state_has_a_label_and_fl_replaces_only_its_own() {
        assert_eq!(
            all_labels().len(),
            2 + State::ALL.len() + FindingState::ALL.len(),
            "one kind label per kind and one state label per state"
        );
        let after = labels_after(&["bug".into(), "fl:record/todo".into()], ItemKind::Record, "doing");
        assert_eq!(after, vec!["bug", "fl:record", "fl:record/doing"]);
    }

    #[test]
    fn a_long_first_line_is_cut_to_the_title_limit() {
        let long = "x".repeat(300);
        let t = title_of(&format!("{long}\nrest"));
        assert_eq!(t.chars().count(), TITLE_MAX);
        assert!(t.ends_with('…'));
        assert_eq!(title_of("short\nrest"), "short");
    }

    #[test]
    fn only_a_github_issue_url_parses() {
        let ok = Iri::parse("https://github.com/Acme/Widgets/issues/41").unwrap();
        assert_eq!(parse_issue_url(&ok), Some(("Acme/Widgets".into(), 41)));
        for bad in [
            "https://github.com/acme/widgets/pull/41",
            "https://github.com/acme/widgets/issues/41/x",
            "https://example.com/acme/widgets/issues/41",
            "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b",
        ] {
            assert_eq!(parse_issue_url(&Iri::parse(bad).unwrap()), None, "{bad}");
        }
    }
}
```

Add `pub mod meta;` to `crates/github/src/lib.rs`.

- [ ] **Step 2: Run the tests**

Run: `cargo test -p fl-github meta`
Expected: PASS, 12 tests.

- [ ] **Step 3: Mutation checks**

One at a time, confirm red, restore:
- Remove the `<`/`>` escaping in `render_body` → `a_value_containing_comment_markers…` FAILS.
- Remove the `issue.state != want` check → `two_state_labels_or_a_status…` FAILS.
- Search with `find` instead of `rfind` → `a_claim_quoting_the_opener…` FAILS.
- Make `labels_after` keep `fl:` labels → `every_state_has_a_label…` FAILS.

- [ ] **Step 4: Trio and commit**

```bash
git add crates/github/src/meta.rs crates/github/src/lib.rs
git commit -m "feat(github): the issue encoding — labels, block, projection, divergence

One kind label, one state label, and a metadata block the body ends
with; open/closed is a projection. A read compares all three and names
every disagreement. Comment markers inside values cannot break the
block. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 4: `GithubTracker` — records, findings, lists, aliases, handles

**Files:**
- Create: `crates/github/src/tracker.rs`
- Modify: `crates/github/src/lib.rs` (`pub mod tracker;` and `pub use tracker::{GithubTracker, Notice, Repo};`)
- Modify: `crates/github/src/fake.rs` (issue routes and test helpers)

**Interfaces:**
- Consumes: Task 1 (`Bindings`, the `StoreError` variants, `Finding.security`), Task 2 (`Client`, `Method`, `Reply`), Task 3 (all of `meta`).
- Produces:

```rust
pub struct Repo { pub full_name: String, pub node_id: String }
pub enum Notice { Renamed { from: String, to: String } }   // Display
pub struct GithubTracker;
impl GithubTracker {
    pub fn open(client: Client, configured: &str, memory: &dyn Bindings) -> Result<(Self, Option<Notice>), StoreError>;
    pub fn repo(&self) -> &Repo;
    pub fn describe(&self) -> String;
    pub fn issue_url(&self, number: u64) -> Iri;
}
impl Tracker for GithubTracker;   impl Handles for GithubTracker;
```

⚠ `GithubTracker` does not check a project reference: it cannot see the catalog. It must always be used through `CatalogChecked` (Task 1), which the CLI (Task 8) and the conformance fixture (Task 5) do.

- [ ] **Step 1: The fake's issue routes**

In `crates/github/src/fake.rs`, add to `impl State`:

```rust
    /// The repository every issue belongs to: the fake's first.
    fn bound(&self) -> &Repo {
        &self.repos[0]
    }

    /// `{o}/{r}` names the bound repository under its current name.
    fn is_bound(&self, o: &str, r: &str) -> bool {
        self.bound().full_name.eq_ignore_ascii_case(&format!("{o}/{r}"))
    }

    fn tick(&mut self) -> u64 {
        let e = self.next_event;
        self.next_event += 1;
        e
    }

    fn issue_json(&self, i: &Issue) -> Value {
        let mut v = json!({
            "number": i.number,
            "node_id": i.node_id,
            "html_url": format!("https://github.com/{}/issues/{}", self.bound().full_name, i.number),
            "title": i.title,
            "body": i.body,
            "labels": i.labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
            "state": i.state,
            "state_reason": i.state_reason,
        });
        if i.pull_request {
            v["pull_request"] = json!({"url": "pull"});
        }
        v
    }
```

a helper beside `split`:

```rust
fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}
```

and these arms in `route`, above the final `_`:

```rust
        ("POST", ["repos", o, r, "issues"]) if s.is_bound(o, r) => {
            if std::mem::take(&mut s.fail_before_create) {
                return answer(502, json!({"message": "fake failure before the create"}));
            }
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let n = s.next_number;
            s.next_number += 1;
            let mut labels = str_list(&v["labels"]);
            if s.drop_labels {
                labels.clear();
            }
            let mut events = Vec::new();
            for l in &labels {
                s.labels.insert(l.clone());
                events.push((s.tick(), "labeled".to_string()));
            }
            let issue = Issue {
                number: n,
                node_id: format!("I_{n}"),
                title: v["title"].as_str().unwrap_or("").into(),
                body: v["body"].as_str().unwrap_or("").into(),
                labels,
                state: "open".into(),
                events,
                ..Issue::default()
            };
            s.issues.insert(n, issue);
            if std::mem::take(&mut s.fail_after_create) {
                return answer(502, json!({"message": "fake failure after the create landed"}));
            }
            if std::mem::take(&mut s.hang_up_after_create) {
                let mut a = answer(201, Value::Null);
                a.hang_up = true;
                return a;
            }
            answer(201, s.issue_json(&s.issues[&n]))
        }
        ("GET", ["repos", o, r, "issues"]) if s.is_bound(o, r) => {
            let want: Vec<String> = q
                .get("labels")
                .map(|l| l.split(',').map(str::to_string).collect())
                .unwrap_or_default();
            let items = s
                .issues
                .values()
                .filter(|i| !i.gone && i.moved_to.is_none())
                .filter(|i| want.iter().all(|w| i.labels.contains(w)))
                .map(|i| s.issue_json(i))
                .collect();
            s.page(&path, &q, items)
        }
        ("GET", ["repos", o, r, "issues", n]) if s.is_bound(o, r) => {
            match n.parse::<u64>().ok().and_then(|n| s.issues.get(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                Some(i) if i.gone => answer(410, json!({"message": "This issue was deleted"})),
                Some(i) if i.moved_to.is_some() => {
                    let mut a = answer(301, json!({"message": "Moved Permanently"}));
                    a.headers.push(("Location".into(), i.moved_to.clone().unwrap_or_default()));
                    a
                }
                Some(i) => answer(200, s.issue_json(i)),
            }
        }
        ("PATCH", ["repos", o, r, "issues", n]) if s.is_bound(o, r) => {
            let Some(n) = n.parse::<u64>().ok().filter(|n| s.issues.contains_key(n)) else {
                return answer(404, json!({"message": "Not Found"}));
            };
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            // A write by someone else that lands inside fl's window.
            if std::mem::take(&mut s.foreign_label_on_next_patch) {
                let e = s.tick();
                s.issues.get_mut(&n).unwrap().events.push((e, "labeled".into()));
            }
            if std::mem::take(&mut s.foreign_edit_on_next_patch) {
                // The fake's own rule: a FIRST edit also records the original.
                let first = s.issues[&n].edits.is_empty();
                for _ in 0..if first { 2 } else { 1 } {
                    let e = format!("E_{}", s.tick());
                    s.issues.get_mut(&n).unwrap().edits.push(e);
                }
            }
            let old = s.issues[&n].clone();
            let mut new = old.clone();
            if let Some(t) = v.get("title").and_then(Value::as_str) {
                new.title = t.into();
            }
            if let Some(b) = v.get("body").and_then(Value::as_str) {
                new.body = b.into();
            }
            if let Some(ls) = v.get("labels") {
                // GitHub silently keeps the old labels when the caller may
                // not set them.
                if !s.drop_labels {
                    new.labels = str_list(ls);
                }
            }
            if let Some(st) = v.get("state").and_then(Value::as_str) {
                new.state = st.into();
            }
            new.state_reason = v.get("state_reason").and_then(Value::as_str).map(str::to_string);
            let mut kinds: Vec<&str> = Vec::new();
            kinds.extend(new.labels.iter().filter(|l| !old.labels.contains(l)).map(|_| "labeled"));
            kinds.extend(old.labels.iter().filter(|l| !new.labels.contains(l)).map(|_| "unlabeled"));
            if old.state != new.state {
                kinds.push(if new.state == "closed" { "closed" } else { "reopened" });
            }
            if old.title != new.title {
                kinds.push("renamed");
            }
            for k in kinds {
                let e = s.tick();
                new.events.push((e, k.into()));
            }
            if old.body != new.body {
                // ⚠ Modelled, not measured: GitHub is taken to record the
                // original body as an entry at the FIRST edit, so a first
                // edit adds two entries. The live test (Task 10) checks it.
                if new.edits.is_empty() {
                    let e = format!("E_{}", s.tick());
                    new.edits.push(e);
                }
                let e = format!("E_{}", s.tick());
                new.edits.push(e);
            }
            for l in &new.labels {
                s.labels.insert(l.clone());
            }
            s.issues.insert(n, new);
            answer(200, s.issue_json(&s.issues[&n]))
        }
```

and to `impl FakeGithub`:

```rust
    pub fn issue(&self, n: u64) -> Issue {
        self.state().issues[&n].clone()
    }

    pub fn issue_count(&self) -> usize {
        self.state().issues.len()
    }

    /// A person changing an issue in the web interface, outside fl.
    pub fn web_edit(&self, n: u64, f: impl FnOnce(&mut Issue)) {
        let mut s = self.state();
        let e = s.tick();
        let issue = s.issues.get_mut(&n).expect("an issue to edit");
        f(issue);
        issue.events.push((e, "labeled".into()));
    }

    /// An issue fl did not make: `labels` as given, no block.
    pub fn plain_issue(&self, labels: &[&str], pull_request: bool) -> u64 {
        let mut s = self.state();
        let n = s.next_number;
        s.next_number += 1;
        let issue = Issue {
            number: n,
            node_id: format!("I_{n}"),
            title: "someone else's".into(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            state: "open".into(),
            pull_request,
            ..Issue::default()
        };
        s.issues.insert(n, issue);
        n
    }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/github/src/tracker.rs` with only its test module for now (the module body comes in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use fl_core::MemStore;
    use fl_core::ids::seq_iri;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    fn open(fake: &FakeGithub) -> GithubTracker {
        GithubTracker::open(client(fake), "acme/widgets", &MemStore::default())
            .unwrap()
            .0
    }

    fn p() -> ProjectId {
        ProjectId(seq_iri(1))
    }

    #[test]
    fn a_record_is_an_issue_with_its_two_labels_and_its_block() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "fix it").unwrap();
        assert_eq!(r.iri().as_str(), "https://github.com/acme/widgets/issues/1");
        let issue = fake.issue(1);
        assert_eq!(issue.labels, vec!["fl:record", "fl:record/todo"]);
        assert_eq!(issue.state, "open");
        let back = t.get_record(&r).unwrap().unwrap();
        assert_eq!((back.title.as_str(), back.state), ("fix it", State::Todo));
        for l in meta::all_labels() {
            assert!(fake.state().labels.contains(&l), "label {l} was not created");
        }
    }

    #[test]
    fn done_closes_the_record_as_completed_and_withdrawn_closes_a_finding_as_not_planned() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        t.set_record_state(&r, State::Done).unwrap();
        let issue = fake.issue(1);
        assert_eq!((issue.state.as_str(), issue.state_reason.as_deref()), ("closed", Some("completed")));
        assert_eq!(t.get_record(&r).unwrap().unwrap().state, State::Done);

        let f = t.add_finding(Finding::raise(p(), r.clone(), "hasty", "a claim")).unwrap();
        let mut fin = t.get_finding(&f).unwrap().unwrap();
        fin.withdraw("not concrete").unwrap();
        t.update_finding(&fin).unwrap();
        let issue = fake.issue(2);
        assert_eq!((issue.state.as_str(), issue.state_reason.as_deref()), ("closed", Some("not_planned")));
        assert_eq!(t.withdrawals_by("hasty").unwrap(), 1);
        assert_eq!(t.withdrawals_by("careful").unwrap(), 0);
    }

    #[test]
    fn a_label_github_dropped_is_an_error_not_a_success() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().drop_labels = true;
        let err = t.add_record(&p(), "t").unwrap_err();
        assert!(err.to_string().contains("did not apply"), "{err}");
    }

    #[test]
    fn a_create_that_failed_after_landing_is_found_not_duplicated() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().fail_after_create = true;
        let r = t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
        assert_eq!(r.iri().as_str(), "https://github.com/acme/widgets/issues/1");
    }

    #[test]
    fn a_create_whose_answer_was_lost_is_found_not_duplicated() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().hang_up_after_create = true;
        t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
    }

    #[test]
    fn a_create_that_failed_before_landing_is_sent_once_more() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake).without_settle();
        fake.state().fail_before_create = true;
        t.add_record(&p(), "t").unwrap();
        assert_eq!(fake.issue_count(), 1, "exactly one issue");
    }

    /// ⚠ The engine reads, runs gates, then writes. A finding withdrawn by
    /// someone else in between must not be marked fixed.
    #[test]
    fn a_write_over_an_item_that_changed_since_fl_read_it_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let f = t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
        let mut stale = t.get_finding(&f).unwrap().unwrap();
        fake.web_edit(2, |i| {
            let (prose, mut m) = meta::parse_body(&i.body).unwrap();
            m.state = "withdrawn".into();
            m.withdrawn_reason = Some("someone else".into());
            i.body = meta::render_body(&prose, &m);
            i.labels = vec!["fl:finding".into(), "fl:finding/withdrawn".into()];
            i.state = "closed".into();
            i.state_reason = Some("not_planned".into());
        });
        stale.assigned_to = Some("fixer".into());
        let err = t.update_finding(&stale).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
        assert!(fake.issue(2).labels.contains(&"fl:finding/withdrawn".to_string()), "untouched");
    }

    #[test]
    fn an_update_that_changes_nothing_sends_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        t.set_record_state(&r, State::Todo).unwrap();
        assert!(!fake.state().requests.iter().any(|q| q.starts_with("PATCH")));
    }

    #[test]
    fn a_list_reads_every_page_and_a_failed_page_fails_it() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        for i in 0..5 {
            t.add_record(&p(), &format!("r{i}")).unwrap();
        }
        fake.state().max_per_page = 2;
        assert_eq!(t.list_records(&p()).unwrap().len(), 5);
        assert!(t.list_records(&ProjectId(seq_iri(2))).unwrap().is_empty());
        fake.state().fail_page = Some(("/repos/acme/widgets/issues".into(), 2));
        assert!(t.list_records(&p()).is_err(), "never a short list");
    }

    /// The row of spec §8.2 that makes the others meaningful: a store that
    /// errors unconditionally would pass every refusal test, and fails this.
    #[test]
    fn an_empty_repository_lists_nothing_cleanly() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        assert_eq!(t.list_records(&p()).unwrap(), vec![]);
        assert_eq!(t.list_findings(&p()).unwrap(), vec![]);
        assert_eq!(t.withdrawals_by("anyone").unwrap(), 0);
    }

    #[test]
    fn deleted_moved_absent_and_foreign_issues_are_told_apart() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let gone = t.add_record(&p(), "gone").unwrap();
        let moved = t.add_record(&p(), "moved").unwrap();
        fake.state().issues.get_mut(&1).unwrap().gone = true;
        fake.state().issues.get_mut(&2).unwrap().moved_to =
            Some(format!("{}/repositories/9/issues/1", fake.url()));
        assert!(matches!(t.get_record(&gone), Err(StoreError::Deleted(_))));
        assert!(matches!(t.get_record(&moved), Err(StoreError::Moved { .. })));
        assert_eq!(t.get_record(&RecordId(t.issue_url(99))).unwrap(), None);
        let plain = fake.plain_issue(&["bug"], false);
        let pr = fake.plain_issue(&["fl:record", "fl:record/todo"], true);
        for n in [plain, pr] {
            let err = t.get_record(&RecordId(t.issue_url(n))).unwrap_err();
            assert!(matches!(err, StoreError::NotAnFlItem { .. }), "{err:?}");
        }
    }

    #[test]
    fn a_title_over_the_limit_is_refused_before_anything_is_sent() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let err = t.add_record(&p(), &"x".repeat(257)).unwrap_err();
        assert!(err.to_string().contains("256"), "{err}");
        assert!(
            !fake.state().requests.iter().any(|r| r.starts_with("POST /repos/acme/widgets/issues")),
            "nothing may be sent"
        );
    }

    #[test]
    fn a_web_edit_that_disagrees_with_the_block_is_reported_and_never_dropped_from_a_list() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| i.labels = vec!["fl:record".into(), "fl:record/done".into()]);
        assert!(matches!(t.get_record(&r), Err(StoreError::Diverged { .. })));
        assert!(matches!(t.list_records(&p()), Err(StoreError::Diverged { .. })));
    }

    #[test]
    fn handles_are_issue_numbers_of_the_right_kind() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let f = t.add_finding(Finding::raise(p(), r.clone(), "a", "c")).unwrap();
        assert_eq!(t.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
        assert_eq!(t.handle_of(Kind::Finding, r.iri()).unwrap(), None);
        assert_eq!(t.handle_of(Kind::Project, r.iri()).unwrap(), None);
        assert_eq!(t.resolve_handle(Kind::Finding, 2).unwrap().as_ref(), Some(f.iri()));
        assert_eq!(t.resolve_handle(Kind::Record, 2).unwrap(), None);
        assert_eq!(t.resolve_handle(Kind::Record, 99).unwrap(), None);
    }

    #[test]
    fn an_alias_is_found_by_a_full_scan_and_a_second_use_of_it_is_refused() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let a = t.add_record(&p(), "a").unwrap();
        let b = t.add_record(&p(), "b").unwrap();
        let old = Iri::parse("https://github.com/elsewhere/old/issues/7").unwrap();
        t.add_alias(a.iri(), old.clone()).unwrap();
        assert_eq!(t.get_record(&RecordId(old.clone())).unwrap().unwrap().id, a);
        assert!(matches!(t.add_alias(b.iri(), old), Err(StoreError::AlreadyExists(_))));
        assert!(matches!(t.add_alias(b.iri(), a.0.clone()), Err(StoreError::AlreadyExists(_))));
    }

    #[test]
    fn open_remembers_the_repository_and_refuses_one_that_replaced_it() {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        assert_eq!(memory.bound_node_id("acme/widgets").unwrap().as_deref(), Some("R_1"));
        memory.bind_node_id("acme/widgets", "R_other").unwrap();
        let err = GithubTracker::open(client(&fake), "acme/widgets", &memory).err().unwrap();
        assert!(matches!(err, StoreError::RepositoryReplaced { .. }), "{err:?}");
    }

    #[test]
    fn a_finding_on_a_finding_is_the_wrong_kind_and_on_nothing_is_no_such_record() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let f = t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
        let err = t.add_finding(Finding::raise(p(), RecordId(f.0.clone()), "a", "c")).unwrap_err();
        assert!(matches!(err, StoreError::WrongKind { expected: Kind::Record, found: Kind::Finding, .. }), "{err:?}");
        let err = t.add_finding(Finding::raise(p(), RecordId(t.issue_url(99)), "a", "c")).unwrap_err();
        assert!(matches!(err, StoreError::NoSuchRecord(_)), "{err:?}");
    }
}
```

Run: `cargo test -p fl-github tracker` — FAIL to compile (`GithubTracker` does not exist).

- [ ] **Step 3: Write the tracker**

Put this ABOVE the test module in `crates/github/src/tracker.rs`:

```rust
//! `GithubTracker`: the `Tracker` and `Handles` roles over the Issues of one
//! repository (GitHub tracker spec §2–§3).
//!
//! ⚠ It does not check project references — it cannot see the catalog. It
//! is always used through `CatalogChecked`, which does (spec §1.3).

use crate::client::{Client, Method};
use crate::meta::{self, IssueView, ItemKind, Meta, Read, RecordRef, TITLE_MAX};
use fl_core::finding::{Finding, FindingState};
use fl_core::ids::{FindingId, Kind, ProjectId, RecordId};
use fl_core::iri::Iri;
use fl_core::model::{Record, State};
use fl_core::store::{Bindings, Handles, StoreError, Tracker};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub full_name: String,
    pub node_id: String,
}

/// Said once, when the tracker opens, and never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Renamed { from: String, to: String },
}

impl std::fmt::Display for Notice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Notice::Renamed { from, to } => write!(
                f,
                "the repository `{from}` is now `{to}`. fl follows it; update the `github` \
                 binding in config.toml to stop this notice"
            ),
        }
    }
}

/// What fl last read or wrote of an item: its block, prose and title.
type Seen = (Meta, String, String);

pub struct GithubTracker {
    client: Client,
    repo: Repo,
    labels_ready: Cell<bool>,
    /// Kinds seen this process, by issue number, so a handle lookup does not
    /// read the issue again.
    kinds: RefCell<BTreeMap<u64, ItemKind>>,
    /// ⚠ What this process last read of each item. The engine reads an
    /// item, runs gates for minutes, then writes: a write refuses if the
    /// item changed in between, so a finding withdrawn by someone else
    /// during a verify cannot be marked fixed (spec §3.3).
    seen: RefCell<BTreeMap<u64, Seen>>,
    /// How long to wait between searches for an ambiguous create's key.
    settle: std::time::Duration,
}

/// What an issue number reached.
#[allow(clippy::large_enum_variant)] // one value per read; boxing buys nothing
enum Fetched {
    Found(IssueView),
    Absent,
    Gone,
    Moved(String),
}

/// What an id reached, for one wanted kind.
#[allow(clippy::large_enum_variant)]
enum Found {
    Item(IssueView, Meta, String),
    OtherKind(ItemKind),
    Absent,
}

/// Whether an id is an issue of this repository.
enum Owner {
    Ours(u64),
    /// Not ours; `Some` carries why, for the `NotOwned` message.
    Elsewhere(Option<String>),
}

fn backend(msg: String) -> StoreError {
    StoreError::Backend(msg)
}

fn text(v: &Value, k: &str) -> Result<String, StoreError> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| backend(format!("GitHub sent a repository without `{k}`")))
}

/// `GET /repos/{name}`, following ONE redirect (a renamed or transferred
/// repository answers 301). `Ok(None)` for 404.
fn read_repo(client: &Client, name: &str) -> Result<Option<Repo>, StoreError> {
    let mut reply = client.send(Method::Get, &format!("/repos/{name}"), None)?;
    if matches!(reply.status, 301 | 302 | 307 | 308) {
        let to = reply
            .location
            .clone()
            .ok_or_else(|| backend(format!("GitHub redirected `{name}` without a Location")))?;
        reply = client.send(Method::Get, &to, None)?;
    }
    match reply.status {
        200 => Ok(Some(Repo {
            full_name: text(&reply.body, "full_name")?,
            node_id: text(&reply.body, "node_id")?,
        })),
        404 => Ok(None),
        s => Err(backend(format!(
            "GitHub answered {s} when fl read the repository `{name}`; retry"
        ))),
    }
}

/// ⚠ The response is the postcondition (spec §3.3): GitHub silently drops
/// labels a caller may not set, so "no error" is not "written".
fn check_written(
    back: &IssueView,
    title: &str,
    labels: &[String],
    body: &str,
    state: &str,
    reason: Option<&str>,
) -> Result<(), StoreError> {
    let want: BTreeSet<&str> = labels.iter().map(String::as_str).collect();
    let got: BTreeSet<&str> = back.labels.iter().map(String::as_str).collect();
    let mut problems = Vec::new();
    if back.title != title {
        problems.push("the title came back different".to_string());
    }
    if reason.is_some() && back.state_reason.as_deref() != reason {
        problems.push(format!(
            "the issue came back closed as `{:?}`, not `{reason:?}`",
            back.state_reason
        ));
    }
    if want != got {
        problems.push(format!("the labels came back as {got:?}, not {want:?}"));
    }
    if back.body.replace("\r\n", "\n") != body {
        problems.push("the body came back different".to_string());
    }
    if back.state != state {
        problems.push(format!("the issue came back `{}`, not `{state}`", back.state));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(backend(format!(
            "GitHub accepted the write to {} but did not apply it: {}. The credential may \
             lack permission to set labels (Issues: read and write)",
            back.url,
            problems.join("; ")
        )))
    }
}

impl GithubTracker {
    /// Read the repository and compare it with the node id the local store
    /// remembers for `configured` (spec §2.4). The first open records it.
    pub fn open(
        client: Client,
        configured: &str,
        memory: &dyn Bindings,
    ) -> Result<(Self, Option<Notice>), StoreError> {
        let repo = read_repo(&client, configured)?.ok_or_else(|| {
            backend(format!(
                "the repository `{configured}` does not exist, or the credential cannot read \
                 it. Check the `github` binding and the credential"
            ))
        })?;
        match memory.bound_node_id(configured)? {
            None => memory.bind_node_id(configured, &repo.node_id)?,
            Some(bound) if bound != repo.node_id => {
                return Err(StoreError::RepositoryReplaced {
                    name: configured.to_string(),
                    bound,
                    found: repo.node_id,
                });
            }
            Some(_) => {}
        }
        let notice = (!repo.full_name.eq_ignore_ascii_case(configured)).then(|| Notice::Renamed {
            from: configured.to_string(),
            to: repo.full_name.clone(),
        });
        let tracker = Self {
            client,
            repo,
            labels_ready: Cell::new(false),
            kinds: RefCell::new(BTreeMap::new()),
            seen: RefCell::new(BTreeMap::new()),
            settle: std::time::Duration::from_secs(2),
        };
        Ok((tracker, notice))
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    pub fn describe(&self) -> String {
        self.client.describe()
    }

    /// Who GitHub says fl writes as (spec §5.4).
    pub fn identity(&self) -> Result<String, StoreError> {
        self.client.identity()
    }

    /// Tests only: no pause between create-key searches.
    #[doc(hidden)]
    pub fn without_settle(mut self) -> Self {
        self.settle = std::time::Duration::ZERO;
        self
    }

    fn remember(&self, n: u64, meta: &Meta, prose: &str, title: &str) {
        self.seen
            .borrow_mut()
            .insert(n, (meta.clone(), prose.to_string(), title.to_string()));
    }

    pub fn issue_url(&self, number: u64) -> Iri {
        Iri::parse(&format!(
            "https://github.com/{}/issues/{number}",
            self.repo.full_name
        ))
        .expect("an issue URL is an IRI")
    }

    fn label(&self) -> String {
        format!("github:{}", self.repo.full_name)
    }

    fn path(&self, rest: &str) -> String {
        format!("/repos/{}{rest}", self.repo.full_name)
    }

    /// Oldest first: an issue created while a list is read lands on its last
    /// page, and cannot shift an earlier page's issues onto the next one.
    fn list_path(&self, labels: &[String]) -> String {
        self.path(&format!(
            "/issues?state=all&sort=created&direction=asc&per_page=100&labels={}",
            labels.join(",")
        ))
    }

    /// Whether `id` is an issue of THIS repository (spec §2.2, §2.4). A URL
    /// under another name costs one read: after a rename or a transfer, the
    /// old name can still reach this repository — or, once reused, another.
    fn owner(&self, id: &Iri) -> Result<Owner, StoreError> {
        let Some((name, n)) = meta::parse_issue_url(id) else {
            return Ok(Owner::Elsewhere(None));
        };
        if name.eq_ignore_ascii_case(&self.repo.full_name) {
            return Ok(Owner::Ours(n));
        }
        Ok(match read_repo(&self.client, &name)? {
            Some(r) if r.node_id == self.repo.node_id => Owner::Ours(n),
            Some(_) => Owner::Elsewhere(Some(format!("`{name}` now names a different repository"))),
            None => Owner::Elsewhere(None),
        })
    }

    /// The issue number `id` names here, directly or as an alias.
    fn locate(&self, id: &Iri) -> Result<u64, StoreError> {
        let why = match self.owner(id)? {
            Owner::Ours(n) => return Ok(n),
            Owner::Elsewhere(why) => why,
        };
        if let Some(n) = self.alias_owner(id)? {
            return Ok(n);
        }
        let searched = match why {
            Some(why) => format!("{} ({why})", self.label()),
            None => self.label(),
        };
        Err(StoreError::NotOwned {
            id: id.clone(),
            searched: vec![searched],
        })
    }

    /// ⚠ A full scan of every fl issue (spec §2.5): correct, and costly — one
    /// list per lookup. The search API is not used: its index lags and it
    /// promises no complete result.
    fn alias_owner(&self, alias: &Iri) -> Result<Option<u64>, StoreError> {
        let mut found = Vec::new();
        for kind in ItemKind::ALL {
            for (issue, meta, _) in self.list(kind, None)? {
                if meta.also_known_as.contains(alias) {
                    found.push(issue.number);
                }
            }
        }
        match found.as_slice() {
            [] => Ok(None),
            [n] => Ok(Some(*n)),
            many => Err(backend(format!(
                "{alias} is an alias of more than one issue ({many:?}); refusing to pick one. \
                 Name the issue by its own URL"
            ))),
        }
    }

    fn fetch(&self, n: u64) -> Result<Fetched, StoreError> {
        let reply = self.client.send(Method::Get, &self.path(&format!("/issues/{n}")), None)?;
        match reply.status {
            200 => Ok(Fetched::Found(IssueView::from_json(&reply.body)?)),
            404 => Ok(Fetched::Absent),
            410 => Ok(Fetched::Gone),
            301 | 302 | 307 | 308 => Ok(Fetched::Moved(
                reply.location.unwrap_or_else(|| "an unknown location".into()),
            )),
            s => Err(backend(format!(
                "GitHub answered {s} when fl read issue {n} of {}; retry",
                self.repo.full_name
            ))),
        }
    }

    /// The fl item `id` names, when it is of kind `want`.
    fn item(&self, id: &Iri, want: ItemKind) -> Result<Found, StoreError> {
        let n = self.locate(id)?;
        match self.fetch(n)? {
            Fetched::Absent => Ok(Found::Absent),
            Fetched::Gone => Err(StoreError::Deleted(id.clone())),
            Fetched::Moved(to) => Err(StoreError::Moved { id: id.clone(), to }),
            Fetched::Found(issue) => match meta::read_item(&issue)? {
                Read::NotFl(what) => Err(StoreError::NotAnFlItem { id: id.clone(), what }),
                Read::Item { kind, meta, prose } => {
                    self.kinds.borrow_mut().insert(n, kind);
                    self.remember(n, &meta, &prose, &issue.title);
                    Ok(if kind == want {
                        Found::Item(issue, meta, prose)
                    } else {
                        Found::OtherKind(kind)
                    })
                }
            },
        }
    }

    /// Every fl item of `kind`, optionally in one state. ⚠ An issue carrying
    /// the kind's label that does not read as that kind is diverged, and the
    /// list fails — it is never dropped (spec §3.4, §5).
    fn list(&self, kind: ItemKind, state: Option<&str>) -> Result<Vec<(IssueView, Meta, String)>, StoreError> {
        let mut labels = vec![meta::kind_label(kind)];
        if let Some(s) = state {
            labels.push(meta::state_label(kind, s));
        }
        let mut out = Vec::new();
        let mut numbers = BTreeSet::new();
        for v in self.client.get_all(&self.list_path(&labels))? {
            let issue = IssueView::from_json(&v)?;
            // Pages are read one by one; an issue seen twice is counted once.
            if !numbers.insert(issue.number) {
                continue;
            }
            match meta::read_item(&issue)? {
                Read::Item { kind: k, meta, prose } if k == kind => {
                    // The label filter is taken to mean AND; the block is
                    // checked too, so a looser filter cannot widen the list.
                    if state.is_some_and(|st| meta.state != st) {
                        continue;
                    }
                    self.kinds.borrow_mut().insert(issue.number, k);
                    self.remember(issue.number, &meta, &prose, &issue.title);
                    out.push((issue, meta, prose));
                }
                Read::Item { .. } => {
                    return Err(StoreError::Diverged {
                        id: issue.url.clone(),
                        detail: "its kind label and its block disagree".into(),
                    });
                }
                Read::NotFl(what) => {
                    return Err(StoreError::Diverged {
                        id: issue.url.clone(),
                        detail: format!("it carries an fl label but is {what}"),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Create every fl label that is missing, explicitly — never as a side
    /// effect of an issue write (spec §3.3). Once per process.
    fn ensure_labels(&self) -> Result<(), StoreError> {
        if self.labels_ready.get() {
            return Ok(());
        }
        let have: BTreeSet<String> = self
            .client
            .get_all(&self.path("/labels?per_page=100"))?
            .iter()
            .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
            .collect();
        for name in meta::all_labels() {
            if have.contains(&name) {
                continue;
            }
            let body = json!({"name": name, "color": "5319e7", "description": "managed by fl"});
            let r = self.client.send(Method::Post, &self.path("/labels"), Some(&body))?;
            if r.status != 201 {
                return Err(backend(format!(
                    "GitHub answered {} when fl created the label `{name}`; retry",
                    r.status
                )));
            }
        }
        self.labels_ready.set(true);
        Ok(())
    }

    fn create(&self, kind: ItemKind, title: &str, prose: &str, meta: &Meta) -> Result<IssueView, StoreError> {
        if title.chars().count() > TITLE_MAX {
            return Err(backend(format!(
                "a title of {} characters is longer than GitHub's limit of {TITLE_MAX}; \
                 shorten it",
                title.chars().count()
            )));
        }
        let (state, _) = meta::projection(kind, &meta.state);
        if state != "open" {
            return Err(backend(format!(
                "fl creates items open, and `{}` is a closed state",
                meta.state
            )));
        }
        self.ensure_labels()?;
        let labels = vec![meta::kind_label(kind), meta::state_label(kind, &meta.state)];
        let body = meta::render_body(prose, meta);
        let sent = json!({"title": title, "body": body, "labels": labels});
        let path = self.path("/issues");
        let issue = match self.client.send(Method::Post, &path, Some(&sent)) {
            Ok(r) if r.status == 201 => IssueView::from_json(&r.body)?,
            // ⚠ An ambiguous failure may already have created the issue.
            // Look for the create key before sending again (spec §3.3).
            Ok(r) if r.status >= 500 => self.after_ambiguous_create(kind, meta, &path, &sent)?,
            Err(StoreError::Unreachable { .. }) => {
                self.after_ambiguous_create(kind, meta, &path, &sent)?
            }
            Ok(r) => {
                return Err(backend(format!(
                    "GitHub answered {} to an issue create",
                    r.status
                )));
            }
            Err(e) => return Err(e),
        };
        check_written(&issue, title, &labels, &body, "open", None)?;
        self.kinds.borrow_mut().insert(issue.number, kind);
        self.remember(issue.number, meta, prose, title);
        Ok(issue)
    }

    /// ⚠ The list GitHub serves may lag a create that just landed, so the
    /// key is searched for three times, `settle` apart, before one resend.
    fn after_ambiguous_create(&self, kind: ItemKind, meta: &Meta, path: &str, sent: &Value) -> Result<IssueView, StoreError> {
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(self.settle);
            }
            if let Some(found) = self.find_by_create_key(kind, &meta.create_key)? {
                return Ok(found);
            }
        }
        let r = self.client.send(Method::Post, path, Some(sent))?;
        if r.status != 201 {
            return Err(backend(format!(
                "GitHub failed an issue create twice (the second answer was {}). List the \
                 repository's fl issues before retrying, so the retry makes no duplicate",
                r.status
            )));
        }
        IssueView::from_json(&r.body)
    }

    fn find_by_create_key(&self, kind: ItemKind, key: &str) -> Result<Option<IssueView>, StoreError> {
        for v in self.client.get_all(&self.list_path(&[meta::kind_label(kind)]))? {
            let issue = IssueView::from_json(&v)?;
            if let Ok((_, m)) = meta::parse_body(&issue.body)
                && m.create_key == key
            {
                return Ok(Some(issue));
            }
        }
        Ok(None)
    }

    /// Read, change, write, and check the answer. `missing` is the error for
    /// an issue that does not exist.
    fn update(
        &self,
        n: u64,
        kind: ItemKind,
        missing: impl FnOnce() -> StoreError,
        change: impl FnOnce(&mut Meta, &mut String, &mut String) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        self.ensure_labels()?;
        let id = self.issue_url(n);
        let issue = match self.fetch(n)? {
            Fetched::Found(i) => i,
            Fetched::Absent => return Err(missing()),
            Fetched::Gone => return Err(StoreError::Deleted(id)),
            Fetched::Moved(to) => return Err(StoreError::Moved { id, to }),
        };
        let Read::Item { kind: found, mut meta, mut prose } = meta::read_item(&issue)? else {
            return Err(StoreError::NotAnFlItem {
                id,
                what: "not an fl item".into(),
            });
        };
        if found != kind {
            return Err(StoreError::WrongKind {
                id,
                expected: kind.as_kind(),
                found: found.as_kind(),
            });
        }
        // ⚠ The caller changed what it READ. If the item moved on since
        // then, writing the caller's fields over it would lose the other
        // change silently (spec §3.3).
        if let Some((m, p, t)) = self.seen.borrow().get(&n)
            && (m != &meta || p != &prose || t != &issue.title)
        {
            return Err(StoreError::Conflict {
                id,
                detail: "it changed after fl read it and before fl wrote it".into(),
            });
        }
        let mut title = issue.title.clone();
        change(&mut meta, &mut prose, &mut title)?;
        let labels = meta::labels_after(&issue.labels, kind, &meta.state);
        let (state, reason) = meta::projection(kind, &meta.state);
        let body = meta::render_body(&prose, &meta);
        let same_labels = labels.iter().collect::<BTreeSet<_>>() == issue.labels.iter().collect::<BTreeSet<_>>();
        if same_labels && body == issue.body.replace("\r\n", "\n") && title == issue.title && state == issue.state {
            return Ok(()); // nothing to write, and nothing to record as an edit
        }
        let mut sent = json!({"title": title, "body": body, "labels": labels, "state": state});
        if let Some(r) = reason {
            sent["state_reason"] = json!(r);
        }
        let r = self.client.send(Method::Patch, &self.path(&format!("/issues/{n}")), Some(&sent))?;
        if r.status != 200 {
            return Err(backend(format!(
                "GitHub answered {} when fl wrote {id}; read it again before retrying",
                r.status
            )));
        }
        let back = IssueView::from_json(&r.body)?;
        check_written(&back, &title, &labels, &body, state, reason)?;
        self.remember(n, &meta, &prose, &title);
        Ok(())
    }

    fn record_from(&self, issue: &IssueView, meta: &Meta) -> Result<Record, StoreError> {
        let state = State::from_wire(&meta.state).ok_or_else(|| StoreError::Diverged {
            id: issue.url.clone(),
            detail: format!("the block's state `{}` is not a record state", meta.state),
        })?;
        Ok(Record {
            id: RecordId(issue.url.clone()),
            project: meta.project.clone(),
            title: issue.title.clone(),
            state,
            also_known_as: meta.also_known_as.clone(),
        })
    }

    fn finding_from(&self, issue: &IssueView, meta: &Meta, prose: &str) -> Result<Finding, StoreError> {
        let diverged = |detail: &str| StoreError::Diverged {
            id: issue.url.clone(),
            detail: detail.to_string(),
        };
        let state = FindingState::from_wire(&meta.state)
            .ok_or_else(|| diverged("the block's state is not a finding state"))?;
        let record = meta
            .record
            .as_ref()
            .ok_or_else(|| diverged("the block names no record"))?;
        let raised_by = meta
            .raised_by
            .clone()
            .ok_or_else(|| diverged("the block names no raiser"))?;
        Ok(Finding {
            id: FindingId(issue.url.clone()),
            project: meta.project.clone(),
            record: RecordId(self.current_ref(record)?),
            raised_by,
            claim: prose.to_string(),
            reproduction: meta.reproduction.clone(),
            state,
            assigned_to: meta.assigned_to.clone(),
            withdrawn_reason: meta.withdrawn_reason.clone(),
            also_known_as: meta.also_known_as.clone(),
            security: meta.security,
        })
    }

    /// A finding's record reference. Task 5 resolves a URL that is not under
    /// this repository's current name by its node id (spec §2.3).
    fn current_ref(&self, r: &RecordRef) -> Result<Iri, StoreError> {
        Ok(r.id.clone())
    }

    /// Which kind issue `n` holds. `None` only when no issue `n` exists; a
    /// deleted, moved or foreign issue is an error naming what it is, never
    /// "not found" (spec §3.5, §3.6).
    fn kind_at(&self, n: u64) -> Result<Option<ItemKind>, StoreError> {
        if let Some(k) = self.kinds.borrow().get(&n) {
            return Ok(Some(*k));
        }
        let id = self.issue_url(n);
        match self.fetch(n)? {
            Fetched::Found(issue) => match meta::read_item(&issue)? {
                Read::Item { kind, .. } => {
                    self.kinds.borrow_mut().insert(n, kind);
                    Ok(Some(kind))
                }
                Read::NotFl(what) => Err(StoreError::NotAnFlItem { id, what }),
            },
            Fetched::Absent => Ok(None),
            Fetched::Gone => Err(StoreError::Deleted(id)),
            Fetched::Moved(to) => Err(StoreError::Moved { id, to }),
        }
    }
}

impl Tracker for GithubTracker {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError> {
        let meta = Meta::new(ItemKind::Record, State::Todo.as_wire(), project.clone());
        Ok(RecordId(self.create(ItemKind::Record, title, "", &meta)?.url))
    }

    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError> {
        match self.item(id.iri(), ItemKind::Record)? {
            Found::Item(issue, meta, _) => self.record_from(&issue, &meta).map(Some),
            Found::OtherKind(_) | Found::Absent => Ok(None),
        }
    }

    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError> {
        let mut out = Vec::new();
        for (issue, meta, _) in self.list(ItemKind::Record, None)? {
            if meta.project == *project {
                out.push(self.record_from(&issue, &meta)?);
            }
        }
        Ok(out)
    }

    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError> {
        let n = self.locate(id.iri())?;
        self.update(n, ItemKind::Record, || StoreError::NoSuchRecord(id.clone()), |meta, _, _| {
            meta.state = state.as_wire().to_string();
            Ok(())
        })
    }

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError> {
        // The record must be an fl record of this repository (spec §3.1).
        let record = match self.item(finding.record.iri(), ItemKind::Record)? {
            Found::Item(issue, _, _) => issue,
            Found::OtherKind(k) => {
                return Err(StoreError::WrongKind {
                    id: finding.record.iri().clone(),
                    expected: Kind::Record,
                    found: k.as_kind(),
                });
            }
            Found::Absent => return Err(StoreError::NoSuchRecord(finding.record.clone())),
        };
        let mut meta = Meta::new(ItemKind::Finding, finding.state.as_wire(), finding.project.clone());
        meta.record = Some(RecordRef {
            id: record.url.clone(),
            node_id: record.node_id.clone(),
        });
        meta.reproduction = finding.reproduction.clone();
        meta.raised_by = Some(finding.raised_by.clone());
        meta.assigned_to = finding.assigned_to.clone();
        meta.withdrawn_reason = finding.withdrawn_reason.clone();
        meta.security = finding.security;
        let title = meta::title_of(&finding.claim);
        Ok(FindingId(
            self.create(ItemKind::Finding, &title, &finding.claim, &meta)?.url,
        ))
    }

    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError> {
        match self.item(id.iri(), ItemKind::Finding)? {
            Found::Item(issue, meta, prose) => self.finding_from(&issue, &meta, &prose).map(Some),
            Found::OtherKind(_) | Found::Absent => Ok(None),
        }
    }

    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError> {
        let n = self.locate(finding.id.iri())?;
        self.update(n, ItemKind::Finding, || StoreError::NoSuchFinding(finding.id.clone()), |meta, prose, title| {
            meta.state = finding.state.as_wire().to_string();
            meta.reproduction = finding.reproduction.clone();
            meta.assigned_to = finding.assigned_to.clone();
            meta.withdrawn_reason = finding.withdrawn_reason.clone();
            // ⚠ Not taken from the caller: `also_known_as` (the trait's
            // contract), `security` (set at raise only, spec §6), the raiser
            // and the record.
            if *prose != finding.claim {
                *prose = finding.claim.clone();
                *title = meta::title_of(&finding.claim);
            }
            Ok(())
        })
    }

    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError> {
        let mut out = Vec::new();
        for (issue, meta, prose) in self.list(ItemKind::Finding, None)? {
            if meta.project == *project {
                out.push(self.finding_from(&issue, &meta, &prose)?);
            }
        }
        Ok(out)
    }

    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError> {
        let withdrawn = self.list(ItemKind::Finding, Some(FindingState::Withdrawn.as_wire()))?;
        Ok(withdrawn
            .iter()
            .filter(|(_, m, _)| m.raised_by.as_deref() == Some(actor))
            .count() as u64)
    }

    fn add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError> {
        // One id namespace: the alias may not name an issue here, nor be
        // another item's alias.
        if let Owner::Ours(_) = self.owner(&alias)? {
            return Err(StoreError::AlreadyExists(alias));
        }
        if self.alias_owner(&alias)?.is_some() {
            return Err(StoreError::AlreadyExists(alias));
        }
        let n = self.locate(primary)?;
        let missing = || StoreError::NotOwned {
            id: primary.clone(),
            searched: vec![format!("{} (no issue {n})", self.label())],
        };
        let kind = self.kind_at(n)?.ok_or_else(missing)?;
        self.update(n, kind, missing, |meta, _, _| {
            meta.also_known_as.push(alias.clone());
            Ok(())
        })
    }
}

impl Handles for GithubTracker {
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError> {
        let Some(want) = ItemKind::from_kind(kind) else {
            return Ok(None);
        };
        // An alias has no handle: only an issue's own URL does.
        let Owner::Ours(n) = self.owner(id)? else {
            return Ok(None);
        };
        Ok((self.kind_at(n)? == Some(want)).then_some(n))
    }

    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError> {
        let Some(want) = ItemKind::from_kind(kind) else {
            return Ok(None);
        };
        Ok((self.kind_at(handle)? == Some(want)).then(|| self.issue_url(handle)))
    }
}
```

Add to `crates/github/src/lib.rs`: `pub mod tracker;` and `pub use tracker::{GithubTracker, Notice, Repo};`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github`
Expected: PASS, including the 17 new tracker tests.

- [ ] **Step 5: Mutation checks**

One at a time, confirm red, restore:
- Make `check_written` return `Ok(())` → `a_label_github_dropped…` FAILS.
- In `create`, treat a 5xx as an error without `after_ambiguous_create` → `a_create_that_failed_after_landing…` FAILS (the retry needs the search, and the plain error ends the call).
- Make `find_by_create_key` return `Ok(None)` → the same test FAILS with two issues.
- Remove the `Err(StoreError::Unreachable { .. })` arm in `create` → `a_create_whose_answer_was_lost…` FAILS.
- Remove the `seen` comparison in `update` → `a_write_over_an_item_that_changed…` FAILS.
- Remove the no-op early return → `an_update_that_changes_nothing…` FAILS.
- In `list`, `continue` past a diverged issue → `a_web_edit_that_disagrees…` FAILS.
- Move the title check after the POST → `a_title_over_the_limit…` FAILS.
- Make `alias_owner` return the first of several matches → add a test first: give two issues the same alias by editing their bodies with `fake.web_edit` (re-render each block with `meta::render_body` and the alias pushed into `also_known_as`), assert `get_record` of the alias is refused naming both numbers, then run the mutation.

- [ ] **Step 6: Trio and commit**

```bash
git add crates/github
git commit -m "feat(github): GithubTracker — records, findings, lists, aliases, handles

Every write is checked against GitHub's answer; an ambiguous create is
found by its key before a retry; a list reads every page and fails on a
diverged issue rather than dropping it; deleted, moved, absent, foreign
and pull-request issues are told apart; aliases resolve by a full scan;
handles are issue numbers of the right kind. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 5: Identity across renames, references by node id, and the shared contract

**Files:**
- Modify: `crates/github/src/tracker.rs` (`current_ref`, the rewrite of stale references in `update`, tests, the conformance run)
- Modify: `crates/github/src/fake.rs` (`rename`, `reuse_name`, the GraphQL node route)

**Interfaces:**
- Consumes: Task 4's `GithubTracker`; Task 1's `CatalogChecked`, `KindRouted`, `Fixture`; plan A's `conformance::{tracker, all_roles, Bound}`.
- Produces: no new public API. After this task `GithubTracker` passes the same `tracker` and `all_roles` suites as the local stores (spec §8.1).

**Spec §2.3, as implemented here:** a reference's URL is trusted when it is under the bound repository's CURRENT name — the repository itself is bound by node id at open. Any other URL is resolved by the reference's `node_id` through one GraphQL lookup, never by the URL. This is the plan's reading of "never by URL"; it avoids a lookup per reference in the common case.

- [ ] **Step 1: The fake reuses names and answers node lookups**

`rename` and a `("POST", ["graphql"])` arm already exist (Task 2). Add to `impl FakeGithub`:

```rust
    /// Someone creates a new repository at `name`, which ends its redirect.
    pub fn reuse_name(&self, name: &str) {
        let mut s = self.state();
        s.redirects.remove(&name.to_ascii_lowercase());
        let id = s.repos.len() as u64 + 1;
        s.repos.push(Repo {
            id,
            node_id: format!("R_{id}"),
            full_name: name.into(),
            visibility: "public".into(),
        });
    }
```

and make the `("POST", ["graphql"])` arm answer a node lookup — `null` with a NOT_FOUND error, as GitHub does, for an unknown or deleted issue:

```rust
        ("POST", ["graphql"]) => {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let id = v.pointer("/variables/id").and_then(Value::as_str).unwrap_or("");
            let node = s
                .issues
                .values()
                .find(|i| i.node_id == id && !i.gone)
                .map(|i| {
                    json!({
                        "url": format!("https://github.com/{}/issues/{}", s.bound().full_name, i.number),
                        "repository": {"id": s.bound().node_id},
                    })
                });
            match node {
                Some(n) => answer(200, json!({"data": {"node": n}})),
                None => answer(200, json!({"data": {"node": null}, "errors": [{"type": "NOT_FOUND"}]})),
            }
        }
```

- [ ] **Step 2: Write the failing tests**

Add to `mod tests` in `tracker.rs`:

```rust
    #[test]
    fn a_renamed_repository_is_followed_with_a_notice_and_its_old_urls_still_resolve() {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        let r = t.add_record(&p(), "t").unwrap();
        fake.rename("acme/gadgets");
        let (t, notice) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        assert_eq!(
            notice,
            Some(Notice::Renamed {
                from: "acme/widgets".into(),
                to: "acme/gadgets".into()
            })
        );
        let back = t.get_record(&r).unwrap().unwrap();
        assert_eq!(back.id.iri().as_str(), "https://github.com/acme/gadgets/issues/1");
    }

    #[test]
    fn a_reused_old_name_is_refused_at_open_and_an_old_url_says_why_it_is_not_owned() {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        let r = t.add_record(&p(), "t").unwrap();
        fake.rename("acme/gadgets");
        fake.reuse_name("acme/widgets");
        let err = GithubTracker::open(client(&fake), "acme/widgets", &memory).err().unwrap();
        assert!(matches!(err, StoreError::RepositoryReplaced { .. }), "{err:?}");
        let (t, _) = GithubTracker::open(client(&fake), "acme/gadgets", &memory).unwrap();
        let err = t.get_record(&r).unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
        assert!(err.to_string().contains("now names a different repository"), "{err}");
    }

    #[test]
    fn a_findings_record_reference_follows_its_node_id_and_the_next_write_rewrites_it() {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        let r = t.add_record(&p(), "t").unwrap();
        t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
        fake.rename("acme/gadgets");
        fake.reuse_name("acme/widgets");
        let (t, _) = GithubTracker::open(client(&fake), "acme/gadgets", &memory).unwrap();
        let mut f = t.get_finding(&FindingId(t.issue_url(2))).unwrap().unwrap();
        assert_eq!(f.record.iri().as_str(), "https://github.com/acme/gadgets/issues/1");
        assert!(fake.issue(2).body.contains("acme/widgets/issues/1"), "not yet rewritten");
        f.withdraw("x").unwrap();
        t.update_finding(&f).unwrap();
        let body = fake.issue(2).body;
        assert!(body.contains("acme/gadgets/issues/1") && !body.contains("acme/widgets/issues/1"), "{body}");
    }

    /// The same suites the local stores pass (spec §8.1): the GitHub
    /// tracker over a `MemStore` catalog and ledger, checked by
    /// `CatalogChecked`, numbered by `KindRouted`.
    mod contract {
        use super::*;
        use fl_core::conformance::{self, Bound, Fixture};
        use fl_core::store::{CatalogChecked, KindRouted};

        struct Split {
            catalog: MemStore,
            tracker: GithubTracker,
            _fake: FakeGithub,
        }

        impl Fixture for Split {
            fn with(&self, f: &mut dyn FnMut(&Bound<'_>)) {
                let checked = CatalogChecked {
                    catalog: &self.catalog,
                    tracker: &self.tracker,
                };
                let handles = KindRouted {
                    catalog: &self.catalog,
                    tracker: &self.tracker,
                };
                f(&Bound {
                    catalog: &self.catalog,
                    tracker: &checked,
                    ledger: &self.catalog,
                    handles: &handles,
                });
            }
        }

        fn split() -> Split {
            let fake = FakeGithub::start("acme/widgets");
            let tracker = open(&fake);
            Split {
                catalog: MemStore::default(),
                tracker,
                _fake: fake,
            }
        }

        #[test]
        fn the_github_tracker_meets_the_tracker_contract() {
            conformance::tracker(split);
        }

        #[test]
        fn the_github_tracker_meets_the_all_roles_contract() {
            conformance::all_roles(split);
        }
    }
```

Run: `cargo test -p fl-github tracker`. `a_findings_record_reference_follows…` FAILS (the reference is not followed). The rename and reuse tests exercise Task 4's `owner` and `open` and are expected to PASS already: they pin that behaviour, and Step 5's mutations are their red evidence. The contract tests show which cases, if any, fail. Record the output.

- [ ] **Step 3: Follow references by node id, and rewrite them on the next write**

Replace `current_ref` in `impl GithubTracker`:

```rust
    /// A finding's record reference (spec §2.3). A URL under this
    /// repository's CURRENT name is trusted: the repository itself is bound
    /// by node id at open. Any other URL is resolved by the reference's node
    /// id — never by the URL, because an old name may now reach another
    /// repository.
    fn current_ref(&self, r: &RecordRef) -> Result<Iri, StoreError> {
        if let Some((name, _)) = meta::parse_issue_url(&r.id)
            && name.eq_ignore_ascii_case(&self.repo.full_name)
        {
            return Ok(r.id.clone());
        }
        let data = self.client.graphql(
            "query($id: ID!) { node(id: $id) { ... on Issue { url repository { id } } } }",
            json!({ "id": r.node_id }),
        )?;
        let node = data
            .get("node")
            .filter(|n| !n.is_null())
            .ok_or_else(|| StoreError::Deleted(r.id.clone()))?;
        let url = node
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| backend("GitHub answered a node lookup without `url`".into()))?;
        if node.pointer("/repository/id").and_then(Value::as_str) != Some(self.repo.node_id.as_str()) {
            return Err(StoreError::Moved {
                id: r.id.clone(),
                to: url.to_string(),
            });
        }
        Iri::parse(url).map_err(|e| backend(format!("GitHub sent an issue URL fl cannot use: {e}")))
    }
```

In `update`, immediately AFTER the `seen` comparison (Task 4) and before `let mut title`, add — after it, because the comparison is against what fl read, and this rewrite is fl's own change:

```rust
        // A reference written under an old name is rewritten on the next
        // write (spec §2.4).
        if let Some(r) = meta.record.as_mut() {
            let current = self.current_ref(r)?;
            r.id = current;
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github`
Expected: PASS, including both contract tests. If a contract case fails, fix `GithubTracker` — never the case; the suite is the contract (spec §8.1). Name any such fix in the report.

- [ ] **Step 5: Mutation checks**

One at a time, confirm red, restore:
- Make `current_ref` return `r.id.clone()` always → `a_findings_record_reference…` FAILS.
- In `owner`, treat any repository answer as ours (drop the node id comparison) → `a_reused_old_name…` FAILS.
- In `open`, skip the `RepositoryReplaced` check → the same test FAILS.
- Remove `CatalogChecked`'s record check (Task 1) → the all-roles contract FAILS.

- [ ] **Step 6: Trio and commit**

```bash
git add crates/github
git commit -m "feat(github): follow renames by node id; pass the shared tracker contract

A renamed repository is followed with a notice and its old URLs still
resolve; a reused old name is refused at open and explained on lookup; a
finding's record reference under another name is resolved by node id
and rewritten on the next write. GithubTracker, checked by
CatalogChecked and numbered by KindRouted, passes the same tracker and
all-roles suites as the local stores. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 6: Conflict detection, and `repair`

**Files:**
- Modify: `crates/github/src/tracker.rs` (`Window`, `window`, `check_window`, `update` uses them, `repair`, tests)
- Modify: `crates/github/src/fake.rs` (timeline route, edit-history GraphQL branch, comments route)
- Modify: `crates/github/src/lib.rs` (export `Repaired`)

**Interfaces:**
- Produces:

```rust
pub struct Repaired { pub number: u64, pub state: String, pub changed: bool }
impl GithubTracker { pub fn repair(&self, id: &Iri, by: &str) -> Result<Repaired, StoreError>; }
```

- [ ] **Step 1: The fake's timeline, edit history and comments**

Arms for `route`. The edit-history branch goes FIRST inside the existing `("POST", ["graphql"])` arm, before the node lookup:

```rust
            let query = v["query"].as_str().unwrap_or("");
            if query.contains("userContentEdits") {
                let n = v.pointer("/variables/number").and_then(Value::as_u64).unwrap_or(0);
                let nodes: Vec<Value> = s
                    .issues
                    .get(&n)
                    .map(|i| i.edits.iter().map(|e| json!({"id": e})).collect())
                    .unwrap_or_default();
                return answer(
                    200,
                    json!({"data": {"repository": {"issue": {"userContentEdits": {"nodes": nodes}}}}}),
                );
            }
```

New arms:

```rust
        ("GET", ["repos", o, r, "issues", n, "timeline"]) if s.is_bound(o, r) => {
            match n.parse::<u64>().ok().and_then(|n| s.issues.get(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                Some(i) if i.gone => answer(410, json!({"message": "This issue was deleted"})),
                Some(i) if i.moved_to.is_some() => {
                    let mut a = answer(301, json!({"message": "Moved Permanently"}));
                    a.headers.push(("Location".into(), i.moved_to.clone().unwrap_or_default()));
                    a
                }
                Some(i) => {
                    let items = i
                        .events
                        .iter()
                        .map(|(id, kind)| json!({"id": id, "event": kind}))
                        .collect();
                    s.page(&path, &q, items)
                }
            }
        }
        ("POST", ["repos", o, r, "issues", n, "comments"]) if s.is_bound(o, r) => {
            let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            match n.parse::<u64>().ok().and_then(|n| s.issues.get_mut(&n)) {
                None => answer(404, json!({"message": "Not Found"})),
                Some(i) => {
                    i.comments.push(v["body"].as_str().unwrap_or("").to_string());
                    answer(201, json!({"id": i.comments.len()}))
                }
            }
        }
```

- [ ] **Step 2: Write the failing tests**

Add to `mod tests` in `tracker.rs`:

```rust
    #[test]
    fn a_foreign_label_inside_fls_write_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().foreign_label_on_next_patch = true;
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    #[test]
    fn a_foreign_body_edit_inside_fls_write_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        // The first edit is fl's; the blind spot is the first edit only.
        t.set_record_state(&r, State::Doing).unwrap();
        fake.state().foreign_edit_on_next_patch = true;
        let err = t.set_record_state(&r, State::Review).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    /// Under the model, a foreign FIRST edit adds two entries and fl's edit
    /// one more: three is more than fl's two, so it is seen.
    #[test]
    fn a_foreign_first_edit_inside_fls_first_edit_is_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().foreign_edit_on_next_patch = true;
        let err = t.set_record_state(&r, State::Doing).unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
    }

    #[test]
    fn a_deleted_or_moved_issue_keeps_its_outcome_through_a_write() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let gone = t.add_record(&p(), "gone").unwrap();
        let moved = t.add_record(&p(), "moved").unwrap();
        fake.state().issues.get_mut(&1).unwrap().gone = true;
        fake.state().issues.get_mut(&2).unwrap().moved_to =
            Some(format!("{}/repositories/9/issues/1", fake.url()));
        assert!(matches!(t.set_record_state(&gone, State::Doing), Err(StoreError::Deleted(_))));
        assert!(matches!(t.set_record_state(&moved, State::Doing), Err(StoreError::Moved { .. })));
        let absent = RecordId(t.issue_url(99));
        assert!(matches!(t.set_record_state(&absent, State::Doing), Err(StoreError::NoSuchRecord(_))));
    }

    #[test]
    fn fls_own_writes_are_never_a_conflict() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        for s in [State::Doing, State::Review, State::Done, State::Doing, State::Done] {
            t.set_record_state(&r, s).unwrap();
        }
        let f = t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
        let mut fin = t.get_finding(&f).unwrap().unwrap();
        fin.withdraw("w").unwrap();
        t.update_finding(&fin).unwrap();
    }

    #[test]
    fn repair_rewrites_the_labels_and_status_from_the_block_and_says_who() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| {
            i.labels = vec!["bug".into()];
            i.state = "closed".into();
        });
        assert!(t.get_record(&r).is_err());
        let done = t.repair(r.iri(), "owner").unwrap();
        assert_eq!((done.number, done.state.as_str(), done.changed), (1, "todo", true));
        let issue = fake.issue(1);
        assert_eq!(issue.labels, vec!["bug", "fl:record", "fl:record/todo"]);
        assert_eq!(issue.state, "open", "the block wins: a repair never closes what fl left open");
        assert!(issue.comments.iter().any(|c| c.contains("by owner")), "{:?}", issue.comments);
        assert_eq!(t.get_record(&r).unwrap().unwrap().state, State::Todo);
    }

    #[test]
    fn repair_trusts_the_block_over_a_state_label() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| {
            i.labels = vec!["fl:record".into(), "fl:record/done".into()];
            i.state = "closed".into();
        });
        let done = t.repair(r.iri(), "owner").unwrap();
        assert_eq!(done.state, "todo");
        assert_eq!(fake.issue(1).labels, vec!["fl:record", "fl:record/todo"]);
        assert_eq!(fake.issue(1).state, "open");
    }

    #[test]
    fn repair_of_a_consistent_issue_changes_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        let done = t.repair(r.iri(), "owner").unwrap();
        assert!(!done.changed);
        assert!(fake.issue(1).comments.is_empty());
    }

    #[test]
    fn repair_refuses_a_damaged_block_and_says_to_restore_it() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.web_edit(1, |i| i.body = "someone rewrote it".into());
        let err = t.repair(r.iri(), "owner").unwrap_err();
        assert!(err.to_string().contains("restore the block"), "{err}");
    }
```

Run: `cargo test -p fl-github tracker` — the conflict tests FAIL (no detection yet) and `repair` does not exist.

- [ ] **Step 3: The window, the check, and `repair`**

Add to `tracker.rs` (module level):

```rust
/// Timeline events that change what fl reads (spec §3.3). A comment or a
/// mention does not, and is not a conflict.
const STATE_EVENTS: [&str; 5] = ["labeled", "unlabeled", "closed", "reopened", "renamed"];

/// What GitHub has recorded about an issue's changes at one moment.
struct Window {
    events: BTreeMap<u64, String>,
    edits: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repaired {
    pub number: u64,
    pub state: String,
    /// `false` when the issue already agreed with its block.
    pub changed: bool,
}
```

Add to `impl GithubTracker`:

```rust
    fn window(&self, n: u64) -> Result<Window, StoreError> {
        let mut events = BTreeMap::new();
        for e in self.client.get_all(&self.path(&format!("/issues/{n}/timeline?per_page=100")))? {
            let kind = e.get("event").and_then(Value::as_str).unwrap_or("");
            if !STATE_EVENTS.contains(&kind) {
                continue;
            }
            let id = e.get("id").and_then(Value::as_u64).ok_or_else(|| {
                backend(format!(
                    "a `{kind}` event on issue {n} has no id, so fl cannot tell it from its own write"
                ))
            })?;
            events.insert(id, kind.to_string());
        }
        let (owner, name) = self
            .repo
            .full_name
            .split_once('/')
            .expect("a full name is owner/name");
        let data = self.client.graphql(
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { userContentEdits(last: 100) { nodes { id } } } } }",
            json!({"owner": owner, "name": name, "number": n}),
        )?;
        let nodes = data
            .pointer("/repository/issue/userContentEdits/nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| backend(format!("GitHub's edit history for issue {n} came back without `nodes`")))?;
        let edits = nodes
            .iter()
            .filter_map(|x| x.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        Ok(Window { events, edits })
    }

    /// ⚠ Detection, not prevention (spec §3.3): GitHub has no conditional
    /// update. Every state-changing event or body edit between `before` and
    /// `after` that fl's own write does not explain is someone else's.
    fn check_window(
        &self,
        id: &Iri,
        before: &Window,
        after: &Window,
        old: &IssueView,
        new: &IssueView,
    ) -> Result<(), StoreError> {
        let mut expected: BTreeMap<&str, usize> = BTreeMap::new();
        *expected.entry("labeled").or_default() +=
            new.labels.iter().filter(|l| !old.labels.contains(l)).count();
        *expected.entry("unlabeled").or_default() +=
            old.labels.iter().filter(|l| !new.labels.contains(l)).count();
        if old.state != new.state {
            let k = if new.state == "closed" { "closed" } else { "reopened" };
            *expected.entry(k).or_default() += 1;
        }
        if old.title != new.title {
            *expected.entry("renamed").or_default() += 1;
        }
        let mut foreign = Vec::new();
        for (eid, kind) in &after.events {
            if before.events.contains_key(eid) {
                continue;
            }
            match expected.get_mut(kind.as_str()) {
                Some(left) if *left > 0 => *left -= 1,
                _ => foreign.push(format!("a `{kind}` event")),
            }
        }
        let new_edits = after.edits.difference(&before.edits).count();
        // ⚠ Modelled, not measured: a FIRST body edit is taken to add two
        // entries (the original, then the edit), and any later edit one.
        // Under that model every foreign edit is seen. If GitHub adds ONE
        // entry on a first edit, a foreign edit landing with fl's first edit
        // would be hidden — the live test measures exactly this.
        let own_edits = match (old.body != new.body, before.edits.is_empty()) {
            (false, _) => 0,
            (true, true) => 2,
            (true, false) => 1,
        };
        if new_edits > own_edits {
            foreign.push(format!("{} body edit(s)", new_edits - own_edits));
        }
        if foreign.is_empty() {
            Ok(())
        } else {
            Err(StoreError::Conflict {
                id: id.clone(),
                detail: format!("GitHub shows changes fl did not make: {}", foreign.join(", ")),
            })
        }
    }

    /// `fl github repair` (spec §3.4): rewrite the fl labels and the
    /// open/closed status FROM the block, and leave a comment naming who ran
    /// it. The block is fl's record of the protocol, so a repair never moves
    /// an item to a state the protocol did not reach.
    pub fn repair(&self, id: &Iri, by: &str) -> Result<Repaired, StoreError> {
        let n = self.locate(id)?;
        // Classify first, so a missing, deleted or moved issue keeps its
        // outcome; then open the window and read again inside it.
        if let Fetched::Absent | Fetched::Gone | Fetched::Moved(_) = self.fetch(n)? {
            return Err(StoreError::NotAnFlItem {
                id: id.clone(),
                what: "an issue that is missing, deleted or moved".into(),
            });
        }
        let before = self.window(n)?;
        let issue = match self.fetch(n)? {
            Fetched::Found(i) => i,
            Fetched::Absent => {
                return Err(StoreError::NotAnFlItem {
                    id: id.clone(),
                    what: "an issue that does not exist".into(),
                });
            }
            Fetched::Gone => return Err(StoreError::Deleted(id.clone())),
            Fetched::Moved(to) => return Err(StoreError::Moved { id: id.clone(), to }),
        };
        if issue.is_pull_request {
            return Err(StoreError::NotAnFlItem {
                id: id.clone(),
                what: "a pull request".into(),
            });
        }
        let restore = |detail: String| StoreError::Diverged {
            id: id.clone(),
            detail: format!(
                "{detail}. A repair rewrites from the block, so restore the block from the \
                 issue's edit history first — or, if the issue was never fl's, remove its fl \
                 labels instead of repairing it"
            ),
        };
        let (_, meta) = meta::parse_body(&issue.body).map_err(|e| restore(format!("its body {e}")))?;
        if !meta.kind.valid_state(&meta.state) {
            return Err(restore(format!("the block's state `{}` is not valid", meta.state)));
        }
        if matches!(meta::read_item(&issue), Ok(Read::Item { .. })) {
            return Ok(Repaired {
                number: n,
                state: meta.state,
                changed: false,
            });
        }
        let labels = meta::labels_after(&issue.labels, meta.kind, &meta.state);
        let (state, reason) = meta::projection(meta.kind, &meta.state);
        let mut sent = json!({"labels": labels, "state": state});
        if let Some(r) = reason {
            sent["state_reason"] = json!(r);
        }
        let r = self.client.send(Method::Patch, &self.path(&format!("/issues/{n}")), Some(&sent))?;
        if r.status != 200 {
            return Err(backend(format!("GitHub answered {} to the repair of {id}", r.status)));
        }
        let back = IssueView::from_json(&r.body)?;
        check_written(&back, &issue.title, &labels, &issue.body.replace("\r\n", "\n"), state, reason)?;
        let after = self.window(n)?;
        self.check_window(id, &before, &after, &issue, &back)?;
        let note = json!({"body": format!(
            "`fl github repair`: the fl labels and the open/closed status were rewritten from \
             fl's record (state `{}`) by {by}.",
            meta.state
        )});
        let c = self.client.send(Method::Post, &self.path(&format!("/issues/{n}/comments")), Some(&note))?;
        if c.status != 201 {
            return Err(backend(format!(
                "the repair of {id} was written, but GitHub answered {} to the comment that \
                 records who ran it; add that comment by hand",
                c.status
            )));
        }
        Ok(Repaired {
            number: n,
            state: meta.state,
            changed: true,
        })
    }
```

In `update`, the order becomes: fetch and classify (so an absent, deleted or moved issue keeps its outcome), THEN take the window, THEN read the issue again and use that second read as the state fl changes and compares — a write landing between the first read and the window is then inside the window. Concretely, right after the existing `let issue = match self.fetch(n)? { … };`:

```rust
        let before = self.window(n)?;
        // Read again inside the window: the state fl changes is the state
        // the window starts from.
        let issue = match self.fetch(n)? {
            Fetched::Found(i) => i,
            Fetched::Absent => return Err(missing()),
            Fetched::Gone => return Err(StoreError::Deleted(id)),
            Fetched::Moved(to) => return Err(StoreError::Moved { id, to }),
        };
```

(`missing` is `FnOnce`; call it in only one of the two matches — make the first match map `Absent` to `StoreError::NotOwned` for this repository's label, or change `missing` to `Fn`.) Then replace the final `check_written(…)?; self.remember(…); Ok(())` with:

```rust
        check_written(&back, &title, &labels, &body, state, reason)?;
        let after = self.window(n)?;
        self.check_window(&id, &before, &after, &issue, &back)?;
        self.remember(n, &meta, &prose, &title);
        Ok(())
```

Export `Repaired` from `lib.rs` (`pub use tracker::{GithubTracker, Notice, Repaired, Repo};`).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-github`
Expected: PASS, including the contract tests, which now run with conflict detection on, and the 9 new tests.

- [ ] **Step 5: Mutation checks**

One at a time, confirm red, restore:
- Make `check_window` return `Ok(())` → both conflict tests FAIL.
- Set `own_edits` to `usize::from(old.body != new.body)` → `fls_own_writes_are_never_a_conflict` FAILS (the fake's first edit adds two entries).
- Take the window BEFORE the first fetch again → `a_deleted_or_moved_issue_keeps_its_outcome…` FAILS.
- In `repair`, write labels and status from the LABELS instead of the block (use the label's state) → `repair_rewrites…` FAILS.
- Skip the comment → the same test FAILS.

- [ ] **Step 6: Trio and commit**

```bash
git add crates/github
git commit -m "feat(github): detect a write that crosses someone else's, and repair divergence

Before and after each write fl reads the issue's state-changing
timeline events and its body edit history; anything its own write does
not explain is a Conflict. The first-edit accounting is modelled, not
measured, and the live test checks it. repair rewrites fl's labels and
status from the block, never from the labels, and leaves a comment
naming who ran it. Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 7: The disclosure rule

**Files:**
- Modify: `crates/github/src/tracker.rs` (`require_private`, its call in `add_finding`, tests)

**Interfaces:**
- Consumes: Task 1's `StoreError::SecurityNotPrivate`, `Finding.security`.

- [ ] **Step 1: Write the failing tests**

```rust
    fn raise_security(t: &GithubTracker) -> Result<FindingId, StoreError> {
        let r = t.add_record(&p(), "t").unwrap();
        let mut f = Finding::raise(p(), r, "a", "a secret-leaking defect");
        f.security = true;
        t.add_finding(f)
    }

    #[test]
    fn a_security_finding_is_written_only_to_a_private_repository() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        raise_security(&t).unwrap();
        assert!(fake.issue(2).body.contains("\"security\":true"));
    }

    #[test]
    fn a_security_finding_on_a_public_or_internal_repository_is_refused_and_nothing_is_written() {
        for visibility in ["public", "internal"] {
            let fake = FakeGithub::start("acme/widgets");
            let t = open(&fake);
            fake.state().repos[0].visibility = visibility.into();
            let err = raise_security(&t).unwrap_err();
            assert!(
                matches!(err, StoreError::SecurityNotPrivate { visibility: ref v, .. } if v == visibility),
                "{err:?}"
            );
            assert_eq!(fake.issue_count(), 1, "only the record exists");
        }
    }

    #[test]
    fn an_unreadable_visibility_refuses_a_security_finding() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        let r = t.add_record(&p(), "t").unwrap();
        fake.state().fail_repo_read = true;
        let mut f = Finding::raise(p(), r, "a", "c");
        f.security = true;
        let err = t.add_finding(f).unwrap_err();
        assert!(err.to_string().contains("visibility"), "{err}");
        assert_eq!(fake.issue_count(), 1);
    }

    #[test]
    fn a_finding_not_marked_security_may_go_to_a_public_repository() {
        let fake = FakeGithub::start("acme/widgets");
        let t = open(&fake);
        fake.state().repos[0].visibility = "public".into();
        let r = t.add_record(&p(), "t").unwrap();
        t.add_finding(Finding::raise(p(), r, "a", "c")).unwrap();
    }
```

Run: `cargo test -p fl-github security` — the refusal tests FAIL.

- [ ] **Step 2: The check**

Add to `impl GithubTracker`:

```rust
    /// Spec §6: only a `private` repository may hold a security finding.
    /// Read live, every time — visibility can change — and a failed read is
    /// an ERROR: an unknown visibility is not a pass.
    fn require_private(&self) -> Result<(), StoreError> {
        let r = self
            .client
            .send(Method::Get, &format!("/repos/{}", self.repo.full_name), None)?;
        let refuse = |why: String| {
            backend(format!(
                "fl could not read the visibility of {} ({why}), so it will not write a \
                 security finding there. Retry, or use a local tracker",
                self.repo.full_name
            ))
        };
        if r.status != 200 {
            return Err(refuse(format!("GitHub answered {}", r.status)));
        }
        let visibility = r
            .body
            .get("visibility")
            .and_then(Value::as_str)
            .ok_or_else(|| refuse("the answer names no visibility".into()))?;
        if visibility == "private" {
            Ok(())
        } else {
            Err(StoreError::SecurityNotPrivate {
                repo: self.repo.full_name.clone(),
                visibility: visibility.to_string(),
            })
        }
    }
```

In `add_finding`, immediately after the record is found and before the `Meta` is built:

```rust
        if finding.security {
            self.require_private()?;
        }
```

- [ ] **Step 3: Run the tests, mutation checks**

Run: `cargo test -p fl-github` — PASS. Then, one at a time: remove the `if finding.security` call → both refusal tests FAIL; accept `internal` → the `internal` iteration FAILS; treat a non-200 as `Ok(())` → `an_unreadable_visibility…` FAILS.

- [ ] **Step 4: Trio and commit**

```bash
git add crates/github/src/tracker.rs
git commit -m "feat(github): a security finding is written only to a private repository

Visibility is read live for every security finding; public and internal
are refused, and an unreadable visibility is an error, never a pass.
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 8: The CLI routes tracker work to the bound tracker

**Files:**
- Modify: `crates/cli/Cargo.toml` (`fl-github`; dev: `fl-github` with `fake`)
- Modify: `crates/cli/src/config.rs` (`TrackerBinding`, `Credential`, `GithubApp`, `Config`, `bound_entry`)
- Create: `crates/cli/src/ctx.rs`
- Modify: `crates/cli/src/main.rs` (open the tracker, build the `Ctx`, filter issue URLs from store choice)
- Modify: `crates/cli/src/refs.rs` (`&dyn Handles`, `#` handles)
- Modify: `crates/cli/src/cmd/{record,finding,attempt,check}.rs` (take `&Ctx`)
- Create: `crates/cli/tests/github.rs`

**Interfaces:**
- Consumes: `GithubTracker::{open, repo, describe, issue_url}`, `Notice`, `Client`, `EnvToken`, `AppCredentials`, `DEFAULT_API`, `meta::is_issue_url`; `CatalogChecked`, `KindRouted`.
- Produces:

```rust
// config
pub struct TrackerBinding { pub github: String, pub credential: Credential }
pub enum Credential { App, Env }
pub struct GithubApp { pub app_id: u64, pub private_key: PathBuf }
pub struct Config { pub projects: Vec<Entry>, pub github: Option<GithubApp> }
pub fn load(path: Option<&Path>) -> Result<Config>;
pub fn bound_entry(entries: &[Entry], cwd: &Path) -> Result<Option<Entry>>;
// ctx
pub struct Ctx<'a> { pub store: &'a RedbStore, pub tracker: &'a dyn Tracker, pub handles: &'a dyn Handles,
                     pub tracker_label: String }          // Task 9 adds `github: Option<&'a GithubTracker>`
impl Ctx<'_> { pub fn roles(&self) -> Roles<'_>; }
```

`FL_GITHUB_API_URL` overrides the API origin (default `https://api.github.com`). It exists for tests. ⚠ The credential goes wherever it points, so it must be `https://`, or `http://` to a loopback address (`127.0.0.1`, `localhost`, `[::1]`); anything else is refused, and a notice is printed on stderr whenever it is set.

- [ ] **Step 1: Write the failing tests**

`crates/cli/Cargo.toml`: add `fl-github = { path = "../github" }` to `[dependencies]` and `fl-github = { path = "../github", features = ["fake"] }` to `[dev-dependencies]`.

Create `crates/cli/tests/github.rs`:

```rust
//! The CLI with a project bound to GitHub Issues, against the in-process
//! fake. The `fl` binary reaches the fake through `FL_GITHUB_API_URL`.

use assert_cmd::Command;
use fl_github::fake::FakeGithub;
use predicates::str::contains;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command as Sys;

fn git(dir: &Path, args: &[&str]) {
    let out = Sys::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

struct G {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    fake: FakeGithub,
}

/// A git repository whose `check.sh` fails while a file named `bug` exists,
/// bound to the fake's `acme/widgets` (or to nothing, when `bound` is false).
fn fixture_with(bound: bool) -> G {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/a.rs"), "fn a() {}").unwrap();
    let check = repo.path().join("check.sh");
    fs::write(&check, "#!/bin/sh\n[ ! -e bug ]\n").unwrap();
    fs::set_permissions(&check, fs::Permissions::from_mode(0o755)).unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    let fake = FakeGithub::start("acme/widgets");
    let tracker = if bound {
        "tracker = { github = \"acme/widgets\", credential = \"env\" }\n"
    } else {
        ""
    };
    let cfg = format!(
        "[[project]]\nroot = \"{}\"\nstore = \"{}\"\n{tracker}",
        repo.path().canonicalize().unwrap().display(),
        home.path().join("fl.redb").display()
    );
    fs::create_dir_all(home.path().join("config/fl")).unwrap();
    fs::write(home.path().join("config/fl/config.toml"), cfg).unwrap();
    G { home, repo, fake }
}

fn fixture() -> G {
    fixture_with(true)
}

impl G {
    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("FL_GITHUB_TOKEN", "t")
            .env("FL_GITHUB_API_URL", self.fake.url())
            .env_remove("GITHUB_TOKEN")
            .env_remove("FL_DB")
            .current_dir(self.repo.path());
        c
    }

    fn project(&self) {
        self.fl().args(["project", "add", "."]).assert().success();
    }
}

#[test]
fn records_live_in_github_issues() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "fix it"])
        .assert()
        .success()
        .stdout(contains("1\tfix it"));
    assert_eq!(g.fake.issue(1).labels, vec!["fl:record", "fl:record/todo"]);
    g.fl()
        .args(["record", "list", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("todo\tfix it"));
    g.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
    assert!(g.fake.issue(1).labels.contains(&"fl:record/doing".to_string()));
}

#[test]
fn a_handle_may_carry_a_hash_and_an_issue_url_skips_the_local_stores() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    g.fl()
        .args(["record", "move", "#1", "--to", "doing"])
        .assert()
        .success();
    g.fl()
        .args(["record", "move", "https://github.com/acme/widgets/issues/1", "--to", "review"])
        .assert()
        .success();
    assert!(g.fake.issue(1).labels.contains(&"fl:record/review".to_string()));
}

#[test]
fn a_command_that_needs_no_tracker_never_contacts_github() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["gate", "add", "--project", "1", "--name", "g", "--glob", "src/**/*.rs", "--program", "true"])
        .assert()
        .success();
    g.fl()
        .args(["transition", "add", "--project", "1", "--name", "ship", "--from", "review", "--to", "done", "--regret", "low", "--gate", "1"])
        .assert()
        .success();
    g.fl().args(["gate", "list", "--project", "1"]).assert().success();
    g.fl().args(["project", "list"]).assert().success();
    g.fl().args(["check", "ship", "--project", "1"]).assert().success();
    g.fl().args(["manifest", "export", "--project", "1"]).assert().success();
    git(g.repo.path(), &["add", ".fl"]);
    git(g.repo.path(), &["commit", "-qm", "manifest"]);
    g.fl().args(["manifest", "check", "--project", "1"]).assert().success();
    assert!(g.fake.state().requests.is_empty(), "{:?}", g.fake.state().requests);
}

#[test]
fn an_api_override_off_this_machine_is_refused() {
    let g = fixture();
    g.project();
    g.fl()
        .env("FL_GITHUB_API_URL", "http://example.com")
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("https://"));
}

#[test]
fn db_cannot_be_combined_with_a_github_binding() {
    let g = fixture();
    g.project();
    g.fl()
        .arg("--db")
        .arg(g.home.path().join("other.redb"))
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("Drop --db"));
}

#[test]
fn a_renamed_repository_is_announced_on_stderr() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    g.fake.rename("acme/gadgets");
    g.fl()
        .args(["record", "list", "--project", "1"])
        .assert()
        .success()
        .stderr(contains("now `acme/gadgets`"));
}

#[test]
fn a_missing_credential_is_refused_naming_where_fl_looked() {
    let g = fixture();
    g.project();
    g.fl()
        .env_remove("FL_GITHUB_TOKEN")
        .args(["record", "list", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("FL_GITHUB_TOKEN or GITHUB_TOKEN"));
}

#[test]
fn an_unknown_tracker_key_is_refused_not_ignored() {
    let g = fixture();
    let path = g.home.path().join("config/fl/config.toml");
    let cfg = fs::read_to_string(&path)
        .unwrap()
        .replace("credential = \"env\" }", "credential = \"env\", extra = 1 }");
    assert!(cfg.contains("extra = 1"), "the edit must land");
    fs::write(&path, cfg).unwrap();
    g.fl().args(["project", "list"]).assert().failure();
}

#[test]
fn an_unbound_project_keeps_its_local_tracker() {
    let g = fixture_with(false);
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    assert!(g.fake.state().requests.is_empty());
}
```

Run: `cargo test -p fl-cli --test github` — FAIL (records are written to the local store; the config rejects `tracker`).

- [ ] **Step 2: The config**

In `crates/cli/src/config.rs`, add the types and change `Entry`, `File` and `load`:

```rust
/// A project's tracker when it is not the local store (GitHub tracker spec
/// §1.4): `tracker = { github = "owner/repo", credential = "env" }`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerBinding {
    pub github: String,
    pub credential: Credential,
}

/// Where the GitHub credential comes from. One source, and no fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    App,
    Env,
}

/// `[github]`: the App fl writes as when a binding says `credential = "app"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubApp {
    pub app_id: u64,
    pub private_key: PathBuf,
}

#[derive(Debug, Default)]
pub struct Config {
    pub projects: Vec<Entry>,
    pub github: Option<GithubApp>,
}
```

`Entry` gains:

```rust
    /// `None`: the tracker is the local store.
    #[serde(default)]
    pub tracker: Option<TrackerBinding>,
```

`File` gains `#[serde(default)] github: Option<GithubApp>,`. `load` returns `Result<Config>` (an absent file is `Config::default()`), and after the existing root/store check validates:

```rust
        if let Some(t) = &e.tracker {
            let ok = t.github.split('/').count() == 2
                && t.github.split('/').all(|p| !p.is_empty())
                && !t.github.contains(char::is_whitespace);
            if !ok {
                bail!(
                    "{}: `github = \"{}\"` must name a repository as `owner/repo`",
                    path.display(),
                    t.github
                );
            }
        }
```

and, for `file.github`, that `private_key` is absolute (same message shape as `root`/`store`).

Rename the body of `bound` to `bound_entry`, returning the winning `Entry` (clone it), and compare `(store, tracker)` pairs rather than stores alone in the "bound to more than one" check (message: "…is bound to more than one store or tracker in the config: …"). Then:

```rust
pub fn bound(entries: &[Entry], cwd: &Path) -> Result<Option<PathBuf>> {
    Ok(bound_entry(entries, cwd)?.map(|e| e.store))
}
```

Update this module's tests for `load` returning `Config` (`.projects`), and give every `Entry` literal in them `tracker: None`. Every existing assertion keeps its meaning. Remove imports the compiler reports unused after the routing change (for example `Roles` and `Tracker` in `record.rs`), and run `cargo fmt`.

- [ ] **Step 3: `Ctx`, `refs`, and the commands**

Create `crates/cli/src/ctx.rs`:

```rust
//! What a command works with (GitHub tracker spec §1.3): the local store
//! for the catalog and the ledger, and whichever tracker the project is
//! bound to.

use fl_core::store::{Handles, Roles, Tracker};
use fl_store::RedbStore;

pub struct Ctx<'a> {
    pub store: &'a RedbStore,
    /// The local store, or the GitHub tracker behind `CatalogChecked`.
    pub tracker: &'a dyn Tracker,
    /// The local store's handles, or `KindRouted` over the store and GitHub.
    pub handles: &'a dyn Handles,
    /// Where records and findings live, for messages.
    pub tracker_label: String,
}

impl Ctx<'_> {
    pub fn roles(&self) -> Roles<'_> {
        Roles {
            catalog: self.store,
            tracker: self.tracker,
            ledger: self.store,
        }
    }
}
```

`crates/cli/src/refs.rs`: `resolve` and `show` take `store: &dyn Handles` instead of `&impl Handles` (every caller passing `store` still compiles). `Ref::from_str` reads `owner/repo#41` as that issue's URL (spec §2.1 — the tracker then answers `NotOwned` for another repository), before the other forms:

```rust
        if let Some((repo, n)) = s.split_once('#')
            && repo.split('/').count() == 2
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
        {
            return Iri::parse(&format!("https://github.com/{repo}/issues/{n}"))
                .map(Ref::Iri)
                .map_err(|e| format!("`{s}` names an issue fl cannot address: {e}"));
        }
```

and accepts a leading `#` on a handle:

```rust
        let digits = s.strip_prefix('#').unwrap_or(s);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return digits
                .parse::<u64>()
                .map(Ref::Handle)
                .map_err(|_| format!("`{s}` is too large to be a handle"));
        }
```

Change `record`, `finding`, `attempt` and `check` to `pub fn run(ctx: &Ctx<'_>, cmd: Cmd) -> Result<i32>`, with `let store = ctx.store;` as the first line, and route by this table (the helpers `finding_id` and `finding` in `finding.rs` take `ctx: &Ctx<'_>` the same way):

| today | becomes |
|---|---|
| `store.add_record`, `get_record`, `list_records`, `set_record_state`, `add_finding`, `get_finding`, `update_finding`, `list_findings`, `withdrawals_by` | `ctx.tracker.…` |
| `refs::resolve(store, store.label(), Kind::Record \| Kind::Finding, …)` | `refs::resolve(ctx.handles, &ctx.tracker_label, …)` |
| `refs::resolve(store, store.label(), Kind::Project \| Kind::Gate, …)` | `refs::resolve(ctx.handles, store.label(), …)` |
| `refs::show(store, …)` | `refs::show(ctx.handles, …)` |
| `Roles::single(store)` | `ctx.roles()` |
| "is not a record/finding in the store at {}", `store.label()` | the same sentence with `ctx.tracker_label` |

Catalog and ledger calls (`get_project`, `list_transitions`, `get_gate`, `append_attempt`, `attempts`, `evaluate_transition(store, store, …)`, the `manifest` guards) stay on `store`.

- [ ] **Step 4: `main.rs`**

Add `mod ctx;` and, in `run`:

```rust
    let cfg = config::load(config::path().as_deref())?;
    let entries = &cfg.projects;
```

(replacing the old `entries` line; pass `entries` where `&entries` was passed). After `locus` is known:

```rust
    let binding = config::bound_entry(entries, &locus)?.and_then(|e| e.tracker);
    let mut iris = cli.command.iris();
    // A GitHub issue URL is the tracker's to resolve: no local store holds
    // one, and searching them would refuse it as NotOwned (spec §2.2).
    if binding.is_some() {
        iris.retain(|i| !fl_github::meta::is_issue_url(i));
    }
```

(replacing `let iris = cli.command.iris();`). After the store is opened:

```rust
    // A bound project's node binding and catalog live in the store its
    // config entry names; `--db` would pair GitHub with another catalog.
    if binding.is_some() && confined && cli.command.needs_tracker() {
        bail!(
            "this project's tracker is bound to GitHub in the config, so it uses the store its \
             config entry names. Drop --db (and unset $FL_DB) for this command"
        );
    }
    let github = match (&binding, cli.command.needs_tracker()) {
        (Some(b), true) => Some(open_github(b, cfg.github.as_ref(), &store)?),
        _ => None,
    };
    let (checked, routed);
    let ctx = match &github {
        Some(gh) => {
            checked = CatalogChecked { catalog: &store, tracker: gh };
            routed = KindRouted { catalog: &store, tracker: gh };
            Ctx {
                store: &store,
                tracker: &checked,
                handles: &routed,
                tracker_label: format!("github:{}", gh.repo().full_name),
            }
        }
        None => Ctx {
            store: &store,
            tracker: &store,
            handles: &store,
            tracker_label: store.label().to_string(),
        },
    };
```

and dispatch `Record`, `Finding`, `Attempt` and `Check` with `&ctx`. Add to `impl Command`:

```rust
    /// Whether the command reads or writes records or findings. Only these
    /// open the tracker, so a catalog command never contacts GitHub.
    fn needs_tracker(&self) -> bool {
        match self {
            Command::Record(_) | Command::Finding(_) | Command::Attempt(_) => true,
            // `check` is the CI gate: it touches the tracker only to resolve
            // `--record`, and must not need GitHub otherwise.
            Command::Check(c) => c.record.is_some(),
            Command::Project(_) | Command::Gate(_) | Command::Transition(_) | Command::Stats(_)
            | Command::Manifest(_) => false,
        }
    }
```

and the opener:

```rust
fn open_github(
    b: &config::TrackerBinding,
    app: Option<&config::GithubApp>,
    store: &RedbStore,
) -> Result<fl_github::GithubTracker> {
    let api = match std::env::var("FL_GITHUB_API_URL") {
        Err(_) => fl_github::DEFAULT_API.to_string(),
        // ⚠ For tests. The credential goes wherever this points, so only
        // https, or plain http to this machine, is accepted — and said.
        Ok(url) => {
            let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
                .iter()
                .any(|p| url == *p || url.starts_with(&format!("{p}:")) || url.starts_with(&format!("{p}/")));
            if !url.starts_with("https://") && !loopback {
                bail!(
                    "$FL_GITHUB_API_URL is `{url}`; fl sends the GitHub credential there, so it \
                     must be https://, or http:// to this machine. Unset it to use GitHub"
                );
            }
            eprintln!("notice: $FL_GITHUB_API_URL is set; talking to {url}, not GitHub");
            url
        }
    };
    let creds: Box<dyn fl_github::Credentials> = match b.credential {
        config::Credential::Env => Box::new(fl_github::EnvToken::from_env()?),
        config::Credential::App => {
            let Some(app) = app else {
                bail!(
                    "`credential = \"app\"` needs a `[github]` section with `app_id` and \
                     `private_key` in the config"
                );
            };
            Box::new(fl_github::AppCredentials::from_file(&api, app.app_id, &app.private_key, &b.github)?)
        }
    };
    let (tracker, notice) =
        fl_github::GithubTracker::open(fl_github::Client::new(&api, creds), &b.github, store)?;
    if let Some(n) = notice {
        eprintln!("notice: {n}");
    }
    Ok(tracker)
}
```

`let (checked, routed);` declares both and assigns them only in the `Some` arm; if the compiler refuses the pattern form, declare them on two lines.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p fl-cli` — PASS, every existing CLI suite included (the local path is unchanged).

- [ ] **Step 6: Mutation checks**

One at a time, confirm red, restore:
- Make `needs_tracker` return `true` for every command → `a_command_that_needs_no_tracker…` FAILS; make `Check(_) => true` → the same test FAILS on `check`.
- Remove the loopback/https guard → `an_api_override_off_this_machine…` FAILS.
- Remove the `--db` refusal → `db_cannot_be_combined…` FAILS.
- Remove the `iris.retain(…)` line → `a_handle_may_carry_a_hash_and_an_issue_url…` FAILS (NotOwned).
- Remove `deny_unknown_fields` from `TrackerBinding` → `an_unknown_tracker_key…` FAILS.
- Accept only digits in `Ref::from_str` → the same `#1` test FAILS.

- [ ] **Step 7: Trio and commit**

```bash
git add crates/cli
git commit -m "feat(cli): route records and findings to the tracker the project is bound to

A config entry may bind a project to a GitHub repository with a named
credential source. Commands that read or write records and findings get
a Ctx: the local store for the catalog and ledger, and the GitHub
tracker behind CatalogChecked and KindRouted; no other command contacts
GitHub. A GitHub issue URL is left to the tracker, a handle may be
written #41, and a rename is announced on stderr. Guards
mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 9: Findings in GitHub mode, `--security`, and `fl github`

**Files:**
- Create: `crates/cli/src/cmd/github.rs`
- Modify: `crates/cli/src/cmd/mod.rs`, `crates/cli/src/main.rs` (`Github` subcommand, last in `Command`)
- Modify: `crates/cli/src/cmd/finding.rs` (`--security`; the publish check before a reproduction)
- Modify: `crates/cli/src/cmd/manifest.rs` (`ensure_publishable(Some(gate))` checks the gate belongs to the project in BOTH branches — plan A's deferred item)
- Modify: `docs/getting-started.md` (the `fl --help` transcript gains `github`)
- Modify: `crates/cli/tests/github.rs`

**Interfaces:**
- Consumes: Task 8's `Ctx`; `GithubTracker::{repair, describe, repo, issue_url}`, `Repaired`; plan A's `ensure_publishable`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/cli/tests/github.rs`:

```rust
#[test]
fn a_finding_walks_raise_reproduce_assign_verify_through_github_issues() {
    let g = fixture();
    g.project();
    fs::write(g.repo.path().join("bug"), "").unwrap();
    g.fl()
        .args(["gate", "add", "--project", "1", "--name", "no-bug", "--glob", "src/**/*.rs", "--program", "./check.sh"])
        .assert()
        .success();
    g.fl().args(["manifest", "export", "--project", "1"]).assert().success();
    git(g.repo.path(), &["add", ".fl"]);
    git(g.repo.path(), &["commit", "-qm", "manifest"]);
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "work"])
        .assert()
        .success();
    g.fl()
        .args(["finding", "raise", "--record", "1", "--claim", "a bug exists", "--by", "rev"])
        .assert()
        .success()
        .stdout(contains("2\traised"));
    g.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .success();
    g.fl()
        .args(["finding", "assign", "2", "--to", "fixer"])
        .assert()
        .success();
    fs::remove_file(g.repo.path().join("bug")).unwrap();
    g.fl()
        .args(["finding", "verify", "2"])
        .assert()
        .success()
        .stdout(contains("CLOSED"));
    let issue = g.fake.issue(2);
    assert_eq!((issue.state.as_str(), issue.state_reason.as_deref()), ("closed", Some("completed")));
    assert!(issue.labels.contains(&"fl:finding/fixed".to_string()));
}

#[test]
fn a_reproduction_is_refused_until_the_manifest_carries_the_gate() {
    let g = fixture();
    g.project();
    fs::write(g.repo.path().join("bug"), "").unwrap();
    g.fl()
        .args(["gate", "add", "--project", "1", "--name", "no-bug", "--glob", "src/**/*.rs", "--program", "./check.sh"])
        .assert()
        .success();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "work"])
        .assert()
        .success();
    g.fl()
        .args(["finding", "raise", "--record", "1", "--claim", "c", "--by", "rev"])
        .assert()
        .success();
    g.fl()
        .args(["finding", "reproduce", "2", "--gate", "1"])
        .assert()
        .failure()
        .stderr(contains("manifest"));
    assert!(g.fake.issue(2).labels.contains(&"fl:finding/raised".to_string()), "unchanged");
}

#[test]
fn a_security_finding_is_refused_on_a_public_repository() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    g.fake.state().repos[0].visibility = "public".into();
    g.fl()
        .args(["finding", "raise", "--record", "1", "--claim", "c", "--by", "rev", "--security"])
        .assert()
        .failure()
        .stderr(contains("security finding"));
    assert_eq!(g.fake.issue_count(), 1);
}

#[test]
fn whoami_names_the_credential_and_the_repository() {
    let g = fixture();
    g.fl()
        .args(["github", "whoami"])
        .assert()
        .success()
        .stdout(
            contains(fl_github::fake::USER_LOGIN)
                .and(contains("$FL_GITHUB_TOKEN"))
                .and(contains("acme/widgets")),
        );
}

#[test]
fn a_web_edit_is_diverged_until_repaired() {
    let g = fixture();
    g.project();
    g.fl()
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    g.fake.web_edit(1, |i| {
        i.labels = vec!["fl:record".into(), "fl:record/done".into()];
        i.state = "closed".into();
    });
    g.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .failure()
        .stderr(contains("fl github repair"));
    g.fl()
        .args(["github", "repair", "1", "--by", "owner"])
        .assert()
        .success()
        .stdout(contains("repaired"));
    g.fl()
        .args(["record", "move", "1", "--to", "doing"])
        .assert()
        .success();
}

#[test]
fn fl_github_without_a_binding_is_refused_naming_the_config() {
    let g = fixture_with(false);
    g.fl()
        .args(["github", "whoami"])
        .assert()
        .failure()
        .stderr(contains("tracker"));
}
```

Add `use predicates::prelude::PredicateBooleanExt;` at the top of the file.

Run: `cargo test -p fl-cli --test github` — the new tests FAIL (`--security` and `github` do not exist; `reproduce` does not check the manifest).

- [ ] **Step 2: `fl github`**

Create `crates/cli/src/cmd/github.rs`:

```rust
//! `fl github` (GitHub tracker spec §3.4, §5.4).

use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::Iri;

#[derive(Subcommand)]
pub enum Cmd {
    /// Print who fl writes to GitHub as, and the repository it binds.
    Whoami,
    /// Rewrite a diverged issue's fl labels and open/closed status from
    /// fl's own record in its body, and leave a comment naming who ran it.
    Repair {
        /// The issue: a handle (`41` or `#41`) or its URL.
        id: Ref,
        /// Who is repairing it; recorded in the comment.
        #[arg(long)]
        by: String,
    },
}

impl Cmd {
    fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Whoami => vec![],
            Cmd::Repair { id, .. } => vec![id],
        }
    }
    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }
    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }
}

pub fn run(ctx: &Ctx<'_>, cmd: Cmd) -> Result<i32> {
    let Some(gh) = ctx.github else {
        bail!("`fl github` needs the project to be bound to a GitHub repository");
    };
    match cmd {
        Cmd::Whoami => {
            println!("writes as\t{}", gh.identity()?);
            println!("credential\t{}", gh.describe());
            println!("repository\t{}", gh.repo().full_name);
        }
        Cmd::Repair { id, by } => {
            let iri = match &id {
                Ref::Handle(n) => gh.issue_url(*n),
                Ref::Iri(i) => i.clone(),
            };
            let done = gh.repair(&iri, &by)?;
            let word = if done.changed { "repaired" } else { "consistent" };
            println!("{word}\t{}\t{}", done.number, done.state);
        }
    }
    Ok(0)
}
```

`ctx.rs`: add the field `pub github: Option<&'a fl_github::GithubTracker>,` (the GitHub tracker, for `fl github` and the publish check), and set it in `main.rs`'s two `Ctx` constructions: `github: Some(gh)` and `github: None`.

`mod.rs`: `pub mod github;`. `main.rs`: a `Github` variant LAST in `Command`, with the doc comment `/// GitHub tracker: who fl writes as, and repair of a diverged issue.` and `#[command(subcommand)]`; its arms in `iris`, `has_handle` and dispatch (`cmd::github::run(&ctx, c)`); `Command::Github(_) => true` in `needs_tracker`; and, before the tracker is opened:

```rust
    if matches!(cli.command, Command::Github(_)) && binding.is_none() {
        bail!(
            "`fl github` needs a tracker binding: add `tracker = {{ github = \"owner/repo\", \
             credential = \"env\" }}` to this project's entry in {}",
            config::path().map(|p| p.display().to_string()).unwrap_or_else(|| "the config".into())
        );
    }
```

- [ ] **Step 3: `--security`, and the publish check before a reproduction**

In `finding.rs`, `Raise` gains:

```rust
        /// Mark the finding as a security finding. A GitHub tracker writes
        /// one only to a private repository (GitHub tracker spec §6).
        #[arg(long)]
        security: bool,
```

and the raise builds the finding, sets `f.security = security;`, then calls `ctx.tracker.add_finding(f)`.

In `Reproduce`, after the gate id is resolved and before `attach_reproduction`:

```rust
            // A gate IRI about to be written where another machine reads it
            // must be in the committed manifest (spec §4.3–§4.5).
            if ctx.github.is_some() {
                crate::cmd::manifest::ensure_publishable(store, &f.project, Some(&gid))?;
            }
```

In `manifest.rs`, `ensure_publishable` with `Some(gate)` first checks, in both branches, that the store holds the gate and that it belongs to `project`:

```rust
    if let Some(g) = gate {
        let def = store
            .get_gate(g)?
            .with_context(|| format!("{g} is held by this store, but it is not a gate"))?;
        if def.project != *project {
            bail!(
                "gate `{}` belongs to project {}, not to {project}. Name a gate of the finding's \
                 project",
                def.name,
                def.project
            );
        }
    }
```

- [ ] **Step 4: The help transcript**

Run `cargo test -p fl-cli --test getting_started`; it fails on the `fl --help` transcript. Add the `github` line after `manifest` in `docs/getting-started.md`, padded like its neighbours, with the text clap prints. Re-run; PASS with `VERIFIED_COMMANDS` still 65.

- [ ] **Step 5: Run the tests, mutation checks**

Run: `cargo test --workspace` — PASS. Then, one at a time: remove the `ensure_publishable` call → `a_reproduction_is_refused_until…` FAILS; drop `f.security = security` → `a_security_finding_is_refused…` FAILS; remove the binding refusal → `fl_github_without_a_binding…` FAILS (a panic or a different message); remove the `def.project != *project` check → the unit test below FAILS. Add it to `crates/cli/src/cmd/manifest.rs` first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};

    #[test]
    fn a_gate_of_another_project_is_refused_before_any_manifest_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("s.redb")).unwrap();
        let p1 = store.add_project("/one").unwrap();
        let p2 = store.add_project("/two").unwrap();
        let kind = GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        });
        let sel = Selector::Glob { pattern: "**/*".into() };
        let g2 = store.add_gate(&p2, "g", kind, sel, 1, "c", "o").unwrap();
        let err = ensure_publishable(&store, &p1, Some(&g2)).unwrap_err();
        assert!(err.to_string().contains("belongs to project"), "{err}");
    }
}
```

(`tempfile` is already a dev-dependency of `fl-cli`.)

- [ ] **Step 6: Trio and commit**

```bash
git add crates/cli docs/getting-started.md
git commit -m "feat(cli): findings in GitHub mode, --security, and fl github whoami|repair

A reproduction in GitHub mode needs the gate in the committed manifest;
raise takes --security, which a GitHub tracker writes only to a private
repository; fl github whoami names the credential and repository, and
fl github repair rewrites a diverged issue from its block. The
single-gate publish check now confirms the gate belongs to the project.
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 10: The live tests, and the documentation

**Files:**
- Create: `crates/github/tests/live.rs`
- Create: `docs/github-tracker.md`
- Modify: `README.md` (one line pointing at the new page, beside the existing docs links)

- [ ] **Step 1: The live tests**

Create `crates/github/tests/live.rs`:

```rust
//! Against GitHub itself (GitHub tracker spec §8.3). Ignored by default.
//!
//! Run against a PRIVATE THROWAWAY repository — these tests create issues
//! and never delete them:
//!
//!   FL_GITHUB_LIVE_REPO=owner/repo FL_GITHUB_TOKEN=… \
//!     cargo test -p fl-github --test live -- --ignored --nocapture --test-threads=1
//!
//! For the App instead of a token, set FL_GITHUB_APP_ID and FL_GITHUB_APP_KEY
//! (the path of its private key file).

use fl_core::finding::Finding;
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::model::State;
use fl_core::store::{StoreError, Tracker};
use fl_core::MemStore;
use fl_github::{AppCredentials, Client, Credentials, DEFAULT_API, EnvToken, GithubTracker};

fn repo() -> String {
    std::env::var("FL_GITHUB_LIVE_REPO")
        .expect("set FL_GITHUB_LIVE_REPO=owner/repo (a private throwaway repository) to run the live tests")
}

fn client() -> Client {
    let creds: Box<dyn Credentials> = match (std::env::var("FL_GITHUB_APP_ID"), std::env::var("FL_GITHUB_APP_KEY")) {
        (Ok(id), Ok(key)) => Box::new(
            AppCredentials::from_file(DEFAULT_API, id.parse().expect("a numeric App id"), key.as_ref(), &repo())
                .expect("the App credential"),
        ),
        _ => Box::new(EnvToken::from_env().expect("FL_GITHUB_TOKEN or GITHUB_TOKEN")),
    };
    Client::new(DEFAULT_API, creds)
}

fn tracker() -> GithubTracker {
    // The repository first: its absence is the message a person needs.
    let repo = repo();
    let client = client();
    let visibility = client
        .send(fl_github::Method::Get, &format!("/repos/{repo}"), None)
        .expect("read the live repository")
        .body["visibility"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(visibility, "private", "the live tests run only against a PRIVATE repository");
    GithubTracker::open(client, &repo, &MemStore::default()).expect("open the live repository").0
}

fn project() -> ProjectId {
    ProjectId(Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7())).unwrap())
}

#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn a_record_and_a_finding_round_trip_on_github() {
    let t = tracker();
    let p = project();
    let r = t.add_record(&p, "fl live test: a record").unwrap();
    for s in [State::Doing, State::Review, State::Done] {
        t.set_record_state(&r, s).unwrap();
        assert_eq!(t.get_record(&r).unwrap().unwrap().state, s);
    }
    let f = t.add_finding(Finding::raise(p.clone(), r.clone(), "live", "a live claim")).unwrap();
    let mut fin = t.get_finding(&f).unwrap().unwrap();
    fin.withdraw("live test").unwrap();
    t.update_finding(&fin).unwrap();
    assert_eq!(t.list_records(&p).unwrap().len(), 1);
    assert_eq!(t.list_findings(&p).unwrap().len(), 1);
}

/// ⚠ One clean round is not evidence: this runs rounds and counts. Two
/// writers each add an alias to the same finding — a read-modify-write of
/// one list. A round in which both succeed and one alias is missing is a
/// SILENT lost update, and must never happen.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn concurrent_writers_are_detected_never_silently_lost() {
    const ROUNDS: usize = 10;
    let (mut clean, mut detected, mut lost, mut other) = (0, 0, 0, 0);
    for round in 0..ROUNDS {
        let t = tracker();
        let p = project();
        let r = t.add_record(&p, &format!("fl live race {round}")).unwrap();
        let f = t.add_finding(Finding::raise(p, r, "live", "race")).unwrap();
        let aliases: Vec<Iri> = (0..2)
            .map(|_| Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7())).unwrap())
            .collect();
        // Both writers open, then wait at the barrier, so their writes overlap.
        let gate = std::sync::Barrier::new(2);
        let results: Vec<Result<(), StoreError>> = std::thread::scope(|s| {
            let handles: Vec<_> = aliases
                .iter()
                .map(|a| {
                    let (f, a, gate) = (f.clone(), a.clone(), &gate);
                    s.spawn(move || {
                        let t = tracker();
                        gate.wait();
                        t.add_alias(f.iri(), a)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        // Let GitHub's reads catch up before judging what landed.
        std::thread::sleep(std::time::Duration::from_secs(2));
        let back = t.get_finding(&f).unwrap().unwrap();
        let both_ok = results.iter().all(Result::is_ok);
        let any_conflict = results.iter().any(|r| matches!(r, Err(StoreError::Conflict { .. })));
        let all_present = aliases.iter().all(|a| back.also_known_as.contains(a));
        match (both_ok, any_conflict, all_present) {
            (true, _, true) => clean += 1,
            (true, _, false) => lost += 1,
            (false, true, _) => detected += 1,
            (false, false, _) => other += 1,
        }
        println!("round {round}: {results:?}");
    }
    println!("clean {clean}, conflict detected {detected}, silently lost {lost}, other errors {other}");
    assert_eq!(lost, 0, "a lost update went undetected");
    assert_eq!(other, 0, "a round failed for a reason other than a detected conflict");
}

/// Measures the model `check_window` rests on, EXACTLY: a first body edit
/// adds two edit-history entries and a later one adds one, and each label
/// change adds one timeline event. `check_window` only tolerates up to its
/// model, so only a direct count can show the model is wrong. If this
/// fails, fix `check_window` and the fake together.
#[test]
#[ignore = "live: needs FL_GITHUB_LIVE_REPO and a credential"]
fn the_edit_history_and_timeline_counts_match_fls_model() {
    let t = tracker();
    let raw = client();
    let repo = repo();
    let (owner, name) = repo.split_once('/').unwrap();
    let r = t.add_record(&project(), "fl live test: edit history").unwrap();
    let n: u64 = r.iri().as_str().rsplit('/').next().unwrap().parse().unwrap();
    let edits = || {
        raw.graphql(
            "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { issue(number: $number) { userContentEdits(last: 100) { nodes { id } } } } }",
            serde_json::json!({"owner": owner, "name": name, "number": n}),
        )
        .unwrap()["repository"]["issue"]["userContentEdits"]["nodes"]
            .as_array()
            .unwrap()
            .len()
    };
    let labelled = || {
        raw.get_all(&format!("/repos/{repo}/issues/{n}/timeline?per_page=100"))
            .unwrap()
            .iter()
            .filter(|e| matches!(e["event"].as_str(), Some("labeled" | "unlabeled")))
            .count()
    };
    let (e0, l0) = (edits(), labelled());
    t.set_record_state(&r, State::Doing).unwrap(); // first edit; one label off, one on
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (e1, l1) = (edits(), labelled());
    t.set_record_state(&r, State::Review).unwrap(); // a later edit
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (e2, l2) = (edits(), labelled());
    println!("edits {e0} -> {e1} -> {e2}; label events {l0} -> {l1} -> {l2}");
    assert_eq!(e1 - e0, 2, "a first body edit adds two entries (the model)");
    assert_eq!(e2 - e1, 1, "a later body edit adds one entry (the model)");
    assert_eq!((l1 - l0, l2 - l1), (2, 2), "each label change is one timeline event");
}
```

Then run `cargo test -p fl-github --test live` — the three tests are listed as ignored and nothing contacts GitHub. Run `cargo test -p fl-github --test live -- --ignored` with `FL_GITHUB_LIVE_REPO` unset (and no token set) and confirm each test FAILS with the message naming `FL_GITHUB_LIVE_REPO` (a live test that passes without running would be a vacuous pass). `serde_json` is already a dependency of `fl-github`, so the integration test can use it.

- [ ] **Step 2: `docs/github-tracker.md`**

Write the page in the style of the existing `docs/` pages: prose, commands in inline code, and no `$ ` transcripts. It must cover, accurately against the code:

1. **What it is.** A project's records and findings can live in one repository's Issues; the catalog and the ledger stay local; other machines import the manifest (`docs/sharing-gates.md`).
2. **Binding.** The `tracker = { github = "owner/repo", credential = "env" | "app" }` entry, the `[github]` section (`app_id`, `private_key`), and that an older fl refuses a config with a `tracker` key.
3. **Credentials.** `FL_GITHUB_TOKEN`, then `GITHUB_TOKEN`; the App: registering it (Issues: read and write; Metadata: read), installing it on the bound repository only, the key file; `fl github whoami`; no fallback between sources.
4. **What an issue looks like.** The two labels, the block at the end of the body, open/closed as a projection, and that fl creates its labels.
5. **Divergence and repair.** What a web edit that disagrees does; `fl github repair <id> --by <name>`, what it rewrites (from the block, never from the labels), the comment it leaves, and that a damaged block must be restored from the edit history first.
6. **Conflicts.** Detection, not prevention; what counts; the first-edit blind spot, stated plainly.
7. **Security findings.** `--security`; only a `private` repository; visibility read every time; the limits (a repository made public later; an unmarked finding).
8. **Identity.** Issue URLs, `#41` handles, a renamed repository (notice), a reused old name (refused), aliases by full scan (cost).
9. **Limits.** Rate limits are reported, not waited out; `FL_GITHUB_API_URL` is for tests and accepts only https or this machine. A finding whose record reference predates a rename costs one lookup each time it is read, until it is next written (a terminal finding is never written again). Install the App on the bound repository only: its token is not narrowed further. An issue whose body holds fl's block is fl's, even with its labels removed — `repair` restores it; fl never adopts an issue without a block. A deliberately recreated repository at the bound name is refused, and there is no command yet to accept it.
10. **The live tests.** How to run them, against a private throwaway repository.

Add one line to `README.md` beside the other documentation links, pointing at the page.

- [ ] **Step 3: Trio and commit**

```bash
git add crates/github/tests/live.rs docs/github-tracker.md README.md
git commit -m "test(github): live tests against a real repository; docs: the GitHub tracker

Three ignored tests: a round trip, a concurrency test that counts silent
lost updates over ten rounds and requires zero, and a check of the
edit-history model conflict detection rests on. They refuse to run
without FL_GITHUB_LIVE_REPO. docs/github-tracker.md covers binding,
credentials, the issue encoding, divergence and repair, conflicts,
security findings, identity and limits.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

## Finish

- [ ] Run the trio once more on the branch head.
- [ ] Dispatch the whole-branch review (`superpowers:requesting-code-review`), address the findings with `superpowers:receiving-code-review`, push, and open the pull request. Name every new crate licence that is not MIT, Apache-2.0, ISC, BSD or Zlib in its description. The owner merges.
- [ ] When the owner has registered the App and made the live-test repository, run the live tests and record their counts in the pull request or a follow-up.
