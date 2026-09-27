# GitHub tracker, plan A — the manifest and the catalog side

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a project's gates travel through a committed manifest, so a machine that does not author the project can import them and run them — with every stamp, import and staleness refusal the spec requires.

**Architecture:** A `manifest` module in `fl-store` defines the file format, its hash, export and parse. `RedbStore` gains an import that writes the manifest's items under their existing IRIs and marks the project imported, plus guards that refuse local edits of imported definitions. The CLI gains `fl manifest export|import|check` and refuses to run an imported project's gates when the working-tree manifest changed since import. Two prerequisites land first: a lost pass mark becomes an error, and the conformance harness binds each role to its own store (plan B's GitHub tracker needs that).

**Tech Stack:** Rust 2024 (`rust-version = "1.98"`), redb 4, serde/serde_json, `sha2` 0.10 (new, MIT OR Apache-2.0), clap 4, assert_cmd.

**Spec:** `docs/superpowers/specs/2026-09-26-github-tracker-design.md` — §4 (manifest), §4.2 (import), §1.3 and §8.1 (conformance), §7 (errors). Plan B (`2026-09-26-github-tracker-b-issues.md`) builds the GitHub tracker on top of this plan and must not start until this plan is merged.

## Global Constraints

- Verification trio, all green before every commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`.
- Unit tests live in `#[cfg(test)] mod tests` inside the module; CLI/subprocess tests live in `crates/<crate>/tests/`.
- Every refusal names what was wrong and the remedy. A refusal is never a panic.
- Every failure path distinguishes "nothing" from "didn't look" (spec §7). A catch-all `_ =>` arm on a result that decides evidence is forbidden.
- Wire and file formats are `snake_case` (decision 33).
- No new dependency except `sha2 = "0.10"` in `fl-store`. Record its licence (MIT OR Apache-2.0) in the commit message.
- A store that never imports stays at format 2 and still opens in older builds. The first import raises it to format 3 (`FORMAT_WITH_IMPORTS`) in the same transaction, so an older fl refuses it rather than ignoring the import mark. This build opens both (Task 4). This is the plan's ruling, not the spec's; the owner may reverse it.
- Each fix is mutation-tested: revert the guard, watch its test go red, restore. Say so in the commit message.
- Commits keep the owner's author identity and end with `Co-authored-by: Ferris <Ferris@artificialhumanity.io>`.
- The repository is public. No machine paths, hostnames or lab-internal names in code, tests, docs or commit messages.

## Review Focus

1. **A manifest exported from a store that also holds other projects** must carry only the named project's gates and transitions — Task 3 pins this with a two-project store.
2. **A gate's command names a local path** (for example `/home/x/bin/lint`). Export must print every gate it writes, so the person sees what is about to be committed — Task 5 asserts the program appears in export's output.
3. **The manifest file is missing, not merely edited**, on an importing machine when a gate is about to run. That must be a refusal naming the path, never a silent skip of the currency check — Task 5 tests it.
4. **Re-import after the authoring side added a gate** must add it (and give it a handle) without disturbing the pass marks of the others — Task 4 tests it.
5. **`fl manifest check` on a clean, current, committed manifest** must exit 0 and say so; the refusals are only meaningful beside a pass — Task 5 tests the pass first.

---

### Task 1: A pass mark that cannot be written is an error

**Files:**
- Modify: `crates/exec/src/evaluate.rs:196-200` (the `if verdict.is_pass()` block in `run_gate`) and its `mod tests`

**Interfaces:**
- Consumes: `run_single_gate(catalog: &dyn Catalog, ledger: &dyn Ledger, project: &ProjectId, gate: &GateId) -> Result<GateReport, ExecError>` (unchanged signature).
- Produces: `run_gate` now returns `Err(ExecError::Store(msg))` when the pass mark write fails; `msg` contains `pass mark`.

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `crates/exec/src/evaluate.rs`, after `a_passing_gate_over_a_real_population_passes_the_transition`:

```rust
    /// Delegates to a `MemStore` in every method except `update_gate`, which
    /// refuses — the one write a passing gate makes to the catalog.
    struct StampRefused<'a>(&'a MemStore);

    impl Catalog for StampRefused<'_> {
        fn add_project(&self, root: &str) -> Result<ProjectId, StoreError> {
            self.0.add_project(root)
        }
        fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError> {
            self.0.get_project(id)
        }
        fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
            self.0.list_projects()
        }
        #[allow(clippy::too_many_arguments)]
        fn add_gate(
            &self,
            p: &ProjectId,
            n: &str,
            k: GateKind,
            s: Selector,
            m: u64,
            c: &str,
            b: &str,
        ) -> Result<GateId, StoreError> {
            self.0.add_gate(p, n, k, s, m, c, b)
        }
        fn get_gate(&self, id: &GateId) -> Result<Option<GateDef>, StoreError> {
            self.0.get_gate(id)
        }
        fn list_gates(&self, p: &ProjectId) -> Result<Vec<GateDef>, StoreError> {
            self.0.list_gates(p)
        }
        fn update_gate(&self, _: &GateDef) -> Result<(), StoreError> {
            Err(StoreError::Backend("the catalog refused the write".into()))
        }
        fn add_transition(&self, t: Transition) -> Result<(), StoreError> {
            self.0.add_transition(t)
        }
        fn get_transition(&self, p: &ProjectId, n: &str) -> Result<Option<Transition>, StoreError> {
            self.0.get_transition(p, n)
        }
        fn list_transitions(&self, p: &ProjectId) -> Result<Vec<Transition>, StoreError> {
            self.0.list_transitions(p)
        }
    }

    // ⚠ Spec §4.2 (Invariant): the pass mark is what makes a gate a
    // neighbour in every later `verify`. A discarded write dropped it from
    // that set with nothing said — a verify examining less than it claims.
    #[test]
    fn a_pass_mark_that_cannot_be_written_is_an_error_and_the_run_is_still_recorded() {
        let d = repo_with(&[("src/a.rs", "fn a() {}")]);
        let s = MemStore::default();
        let p = setup(&s, d.path(), "true", "src/**/*.rs", Regret::Low);
        let g = s.list_gates(&p).unwrap()[0].id.clone();

        let err = run_single_gate(&StampRefused(&s), &s, &p, &g)
            .expect_err("a pass whose mark was lost must not read as a clean pass");

        assert!(matches!(err, ExecError::Store(_)), "{err:?}");
        assert!(err.to_string().contains("pass mark"), "{err}");
        assert_eq!(
            s.gate_runs(&g).unwrap().len(),
            1,
            "the evidence is written before the mark, and it is kept"
        );
        assert_eq!(s.get_gate(&g).unwrap().unwrap().last_pass_commit, None);
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p fl-exec a_pass_mark_that_cannot_be_written`
Expected: FAIL — `a pass whose mark was lost must not read as a clean pass` (the call returns `Ok`).

- [ ] **Step 3: Stop discarding the error**

In `run_gate`, replace:

```rust
    if verdict.is_pass() {
        let mut updated = def.clone();
        updated.last_pass_commit = Some(head.to_string());
        let _ = catalog.update_gate(&updated);
    }
```

with:

```rust
    // ⚠ The pass mark is what makes this gate a neighbour in every later
    // `verify` (spec §4.2). Discarding a failed write here dropped the gate
    // from that set with nothing said. The ledger row above is already
    // written, so the evidence survives the refusal.
    if verdict.is_pass() {
        let mut updated = def.clone();
        updated.last_pass_commit = Some(head.to_string());
        catalog.update_gate(&updated).map_err(|e| {
            ExecError::Store(format!(
                "gate `{}` passed, but its pass mark could not be written: {e}",
                def.name
            ))
        })?;
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p fl-exec`
Expected: PASS, every test.

- [ ] **Step 5: Mutation check**

Temporarily put back `let _ = catalog.update_gate(&updated);`, run `cargo test -p fl-exec a_pass_mark_that_cannot_be_written`, confirm FAIL, restore the fix, confirm PASS.

- [ ] **Step 6: Trio and commit**

Run the three trio commands. Then:

```bash
git add crates/exec/src/evaluate.rs
git commit -m "fix(exec): a pass mark that cannot be written is an error

run_gate discarded the result of the catalog write that stamps
last_pass_commit, so a lost mark silently removed the gate from every
later verify's neighbours. The ledger row is still written first.
Mutation-tested: restoring the discard turns the new test red.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 2: The conformance harness binds each role to its own store

**Files:**
- Modify: `crates/core/src/conformance.rs` (harness, the `tracker` and `all_roles` suites and their cases)
- Modify: `crates/core/src/mem.rs:415-422` (the conformance test)
- Modify: `crates/store/src/lib.rs:1190-1196` (the conformance test)

**Interfaces:**
- Produces (used by plan B to run the suites against a split binding):

```rust
pub struct Bound<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
    pub ledger: &'a dyn Ledger,
    pub handles: &'a dyn Handles,
}
pub trait Fixture {
    fn bound(&self) -> Bound<'_>;
}
pub struct Single<S, G>(pub S, pub G);
impl<S: Catalog + Tracker + Ledger + Handles, G> Fixture for Single<S, G>;
pub fn tracker<F: Fixture>(make: impl Fn() -> F);
pub fn all_roles<F: Fixture>(make: impl Fn() -> F);
pub fn local_handles<S: Catalog + Tracker + Ledger + Handles, G>(make: impl Fn() -> (S, G));
```

- `catalog` and `ledger` suites keep their current signatures.

This task changes no behaviour of any store. It is a refactor under green tests, with one case moved (spec §8.1 item 3) and one case's assertion generalised.

- [ ] **Step 1: Add the binding types and the bound runner**

In `crates/core/src/conformance.rs`, after `run_suite`, add:

```rust
/// The roles a case uses, each bound to the store that backs it (spec
/// §1.3 of the GitHub tracker design). A local store binds itself to every
/// role; a split binding — a GitHub tracker over a local catalog and ledger
/// — binds each to its own. `handles` answers for every kind, routed to
/// whichever store holds that kind.
pub struct Bound<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
    pub ledger: &'a dyn Ledger,
    pub handles: &'a dyn Handles,
}

/// Something that can hand out a [`Bound`] for one case. Owns its stores
/// and any guard (a temp directory, a fake server) for the case's lifetime.
pub trait Fixture {
    fn bound(&self) -> Bound<'_>;
}

/// One store backing every role, plus a guard to keep alive.
pub struct Single<S, G>(pub S, pub G);

impl<S: Catalog + Tracker + Ledger + Handles, G> Fixture for Single<S, G> {
    fn bound(&self) -> Bound<'_> {
        Bound {
            catalog: &self.0,
            tracker: &self.0,
            ledger: &self.0,
            handles: &self.0,
        }
    }
}

/// [`run_suite`] for cases written against a [`Bound`].
fn run_bound<F: Fixture>(
    suite: &str,
    expected: usize,
    cases: &[fn(&Bound<'_>)],
    make: impl Fn() -> F,
) {
    assert_eq!(
        cases.len(),
        expected,
        "the {suite} suite lists {} cases but declares {expected}. A case was added or \
         removed: if that was deliberate, update the count beside the list; if not, \
         restore the case",
        cases.len()
    );
    for case in cases {
        let fixture = make();
        case(&fixture.bound());
    }
}
```

- [ ] **Step 2: Rewrite the `tracker` and `all_roles` suites over `Bound`**

Replace the two suite functions and the case-count constants with:

```rust
/// How many cases [`tracker`] runs. Update deliberately — see [`run_suite`].
const TRACKER_CASES: usize = 12;
/// How many cases [`all_roles`] runs. Update deliberately — see [`run_suite`].
const ALL_ROLES_CASES: usize = 7;
/// How many cases [`local_handles`] runs. Update deliberately — see [`run_suite`].
const LOCAL_HANDLES_CASES: usize = 1;

pub fn tracker<F: Fixture>(make: impl Fn() -> F) {
    let cases: &[fn(&Bound<'_>)] = &[
        list_records_returns_only_the_named_projects_records,
        a_record_state_change_is_visible_on_the_next_read,
        a_finding_round_trips_and_gets_a_real_id,
        withdrawals_are_counted_against_whoever_raised_the_finding,
        findings_are_listed_per_project,
        an_alias_reaches_the_item_it_names,
        an_alias_already_in_use_is_refused_and_names_it,
        an_alias_on_a_finding_reaches_it,
        add_alias_resolves_through_an_existing_alias_to_the_true_primary,
        set_record_state_and_update_finding_through_an_alias_touch_the_primary_once,
        a_finding_raised_against_a_record_alias_stores_the_primary,
        update_finding_keeps_the_stored_aliases_whatever_the_caller_holds,
    ];
    run_bound("tracker", TRACKER_CASES, cases, make);
}

/// Cases over every role at once, including ownership and handles.
pub fn all_roles<F: Fixture>(make: impl Fn() -> F) {
    let cases: &[fn(&Bound<'_>)] = &[
        an_id_this_store_never_held_is_not_owned_rather_than_absent,
        a_list_over_a_project_this_store_never_held_is_refused_not_empty,
        a_finding_on_a_record_this_store_never_held_is_refused,
        an_id_of_another_kind_is_owned_but_not_found,
        an_id_of_another_kind_where_a_project_or_record_is_needed_is_refused_as_the_wrong_kind,
        an_id_has_no_handle_under_any_kind_but_its_own,
        no_operation_gives_up_an_owned_id,
    ];
    run_bound("all-roles", ALL_ROLES_CASES, cases, make);
}

/// A property of the LOCAL stores, not of the `Handles` contract: handles
/// are numbered per kind from one. A GitHub tracker's handles are issue
/// numbers shared by records and findings (spec §2.1), so this case is not
/// part of the shared contract (spec §8.1 item 3).
pub fn local_handles<S: Catalog + Tracker + Ledger + Handles, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[handles_are_per_kind_and_start_at_one::<S>];
    run_suite("local-handles", LOCAL_HANDLES_CASES, cases, make);
}
```

Also update the module doc comment's first paragraph to say that a case which only a local store can pass lives in `local_handles`, and why.

- [ ] **Step 3: Convert every case in the two suites**

Apply this rule to each of the 19 case functions listed in Step 2 (not to `handles_are_per_kind_and_start_at_one`, and not to the `catalog` or `ledger` cases):

1. Signature: `fn NAME<S: …>(s: &S)` becomes `fn NAME(roles: &Bound<'_>)`. Keep `pub` where it was `pub`. The parameter is `roles`, not `b`: two cases already have locals named `b` (`findings_are_listed_per_project`, `an_alias_already_in_use_is_refused_and_names_it`).
2. Route each call on `s` to the role that declares the method:

| methods | becomes |
|---|---|
| `add_project`, `get_project`, `list_projects`, `add_gate`, `get_gate`, `list_gates`, `update_gate`, `add_transition`, `get_transition`, `list_transitions` | `roles.catalog.…` |
| `add_record`, `get_record`, `list_records`, `set_record_state`, `add_finding`, `get_finding`, `update_finding`, `list_findings`, `withdrawals_by`, `add_alias` | `roles.tracker.…` |
| `append_gate_run`, `append_attempt`, `gate_runs`, `attempts` | `roles.ledger.…` |
| `handle_of`, `resolve_handle` | `roles.handles.…` |

Example — before:

```rust
fn a_record_state_change_is_visible_on_the_next_read<S: Catalog + Tracker>(s: &S) {
    let p = s.add_project("/tmp/p").unwrap();
    let r = s.add_record(&p, "fix the thing").unwrap();
    s.set_record_state(&r, State::Doing).unwrap();
    assert_eq!(s.get_record(&r).unwrap().unwrap().state, State::Doing);
}
```

after:

```rust
fn a_record_state_change_is_visible_on_the_next_read(roles: &Bound<'_>) {
    let p = roles.catalog.add_project("/tmp/p").unwrap();
    let r = roles.tracker.add_record(&p, "fix the thing").unwrap();
    roles.tracker.set_record_state(&r, State::Doing).unwrap();
    assert_eq!(roles.tracker.get_record(&r).unwrap().unwrap().state, State::Doing);
}
```

- [ ] **Step 4: Generalise the one handle assertion that assumed local numbering**

In `an_id_has_no_handle_under_any_kind_but_its_own`, replace the loop body's first assertion:

```rust
        assert_eq!(
            s.handle_of(*own, id).unwrap(),
            Some(1),
            "{id} as its own kind"
        );
```

with:

```rust
        let h = roles
            .handles
            .handle_of(*own, id)
            .unwrap()
            .unwrap_or_else(|| panic!("{id} has no handle as its own kind"));
        assert_eq!(
            roles.handles.resolve_handle(*own, h).unwrap().as_ref(),
            Some(id),
            "{id}'s handle resolves back to it"
        );
```

and replace the comment above the function with:

```rust
// Final review, item 11: `refs::show` prints a handle only when
// `handle_of(kind, id)` answers `Some`, so an id held under another kind must
// answer `None` — never the handle it has under its OWN kind. A store that
// ignored `kind` would answer `Some` in every row that must be `None`.
```

Generalising that assertion removes the only check that a local store's first finding gets handle 1. Put it back in the local case: in `handles_are_per_kind_and_start_at_one` (which keeps its generic `<S>` form and its `s.` calls), after `let r = s.add_record(&p, "t").unwrap();` add

```rust
    let f = s
        .add_finding(Finding::raise(p.clone(), r.clone(), "a", "c"))
        .unwrap();
    assert_eq!(s.handle_of(Kind::Finding, f.iri()).unwrap(), Some(1));
```

- [ ] **Step 5: Update the two callers**

`crates/core/src/mem.rs`, the test body becomes:

```rust
    #[test]
    fn mem_store_meets_every_role_contract() {
        use crate::conformance::Single;
        crate::conformance::catalog(|| (MemStore::default(), ()));
        crate::conformance::tracker(|| Single(MemStore::default(), ()));
        crate::conformance::ledger(|| (MemStore::default(), ()));
        crate::conformance::all_roles(|| Single(MemStore::default(), ()));
        crate::conformance::local_handles(|| (MemStore::default(), ()));
    }
```

`crates/store/src/lib.rs`, the test body becomes:

```rust
    #[test]
    fn redb_store_meets_every_role_contract() {
        use fl_core::conformance::Single;
        let single = || {
            let (s, g) = fresh();
            Single(s, g)
        };
        fl_core::conformance::catalog(fresh);
        fl_core::conformance::tracker(single);
        fl_core::conformance::ledger(fresh);
        fl_core::conformance::all_roles(single);
        fl_core::conformance::local_handles(fresh);
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p fl-core -p fl-store`
Expected: PASS. If a case fails, the routing table in Step 3 was misapplied in that case.

- [ ] **Step 7: Check the population did not shrink**

Sum the declared case counts. Before: `CATALOG_CASES` 3 + `TRACKER_CASES` 12 + `LEDGER_CASES` 1 + `ALL_ROLES_CASES` 8 = 24. After: 3 + 12 + 1 + 7 + `LOCAL_HANDLES_CASES` 1 = 24. The totals must be equal: this task moves one case and deletes none. (A `grep -c '^fn '` count rises by two, because `run_bound` and `local_handles` are new top-level functions; that is expected.) Then run `cargo test -p fl-core mem_store_meets 2>&1 | tail -3` and confirm the test ran.

**Notes for plan B (not work for this task).** `Bound` is only a set of references. A split binding passes `a_list_over_a_project_this_store_never_held_is_refused_not_empty` and the wrong-kind case only if plan B wraps the GitHub tracker in an adapter that checks project references against the catalog (spec §1.3). The wrong-kind case's closing assertions (`resolve_handle(Kind::Gate, 2)`, `(Kind::Record, 2)`, `(Kind::Finding, 1)` are `None`) hold under GitHub numbering as well — issue 2 does not exist and issue 1 is a record — but plan B must confirm that against its fake.

- [ ] **Step 8: Trio and commit**

```bash
git add crates/core/src/conformance.rs crates/core/src/mem.rs crates/store/src/lib.rs
git commit -m "test(core): bind each role to its own store in the conformance harness

The tracker and all-roles suites required one store backing every role,
which a GitHub tracker over a local catalog cannot be. Cases now take a
Bound with one reference per role. \"Handles start at one per kind\" is a
local-store property, not the Handles contract, and moves to its own
local_handles suite; all-roles declares 7 cases (was 8), and no case is
deleted.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 3: The manifest format, its hash, export — and the manifest never makes a gate stale

**Files:**
- Create: `crates/store/src/manifest.rs`
- Modify: `crates/store/src/lib.rs` (add `pub mod manifest;`)
- Modify: `crates/core/src/lib.rs` (the `MANIFEST_PATH` constant, so `fl-exec` can use it without depending on `fl-store`)
- Modify: `crates/exec/src/evaluate.rs` (`staleness_for` ignores the manifest file) and its `mod tests`
- Modify: `Cargo.toml` (workspace dependency) and `crates/store/Cargo.toml`

**Interfaces:**
- Produces:

```rust
pub const MANIFEST_FORMAT: u64 = 1;
pub use fl_core::MANIFEST_PATH; // ".fl/manifest.json", defined in fl-core
pub struct Provenance { pub commit: String, pub exported_at_unix: u64 }
pub struct Body { pub format_version: u64, pub provenance: Provenance, pub project: ProjectId,
                  pub gates: Vec<GateDef>, pub transitions: Vec<Transition> }
pub struct Manifest { pub body: Body, pub content_sha256: String }
pub enum ManifestError { Parse(String), Format { found: u64 }, HandEdited { recorded: String, found: String },
                         Inconsistent(String), AuthoringStore(ProjectId), NotAuthoring(ProjectId),
                         WouldRemoveGate(GateId), RootTaken { root: String, other: ProjectId }, Store(StoreError) }
pub enum Currency { Current, Differs, Absent }
pub fn content_sha256(body: &Body) -> Result<String, ManifestError>;
pub fn export(catalog: &dyn Catalog, project: &ProjectId, commit: &str, exported_at_unix: u64) -> Result<Manifest, ManifestError>;
impl Manifest {
    pub fn to_json(&self) -> String;
    pub fn parse(text: &str) -> Result<Manifest, ManifestError>;
    pub fn verify(&self) -> Result<(), ManifestError>; // hash + internal consistency
    pub fn currency_of(&self, def: &GateDef) -> Currency;
}
// fl-core
pub const MANIFEST_PATH: &str = ".fl/manifest.json";
```

- [ ] **Step 0: Put `MANIFEST_PATH` in `fl-core`**

In `crates/core/src/lib.rs`, after the `pub use` lines, add:

```rust
/// Where a project's committed manifest lives, relative to the project root
/// (GitHub tracker spec §4.1). Here rather than in `fl-store` because the
/// engine must recognise the file too: it never makes a gate stale.
pub const MANIFEST_PATH: &str = ".fl/manifest.json";
```

- [ ] **Step 1: Add the dependency**

In the root `Cargo.toml` `[workspace.dependencies]`, add `sha2 = "0.10"`. In `crates/store/Cargo.toml` `[dependencies]`, add `sha2.workspace = true`.

- [ ] **Step 2: Write the module with its failing tests**

Create `crates/store/src/manifest.rs`:

```rust
//! The committed manifest (GitHub tracker spec §4): a project's gates and
//! transitions, exported from the store that authors them, so another
//! machine can import them and resolve the gate an issue names.
//!
//! ⚠ The hash covers everything but itself. A manifest whose content does
//! not hash to the value it records was edited by hand, and is refused:
//! the store that authors the project is the only place a gate is authored.

use fl_core::ids::{GateId, ProjectId};
use fl_core::model::{GateDef, Transition};
use fl_core::store::{Catalog, StoreError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const MANIFEST_FORMAT: u64 = 1;
pub use fl_core::MANIFEST_PATH;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub commit: String,
    pub exported_at_unix: u64,
}

/// Everything the hash covers. ⚠ No store path and no project root: both
/// are specific to a machine, and the repository can be public (spec §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    pub format_version: u64,
    pub provenance: Provenance,
    pub project: ProjectId,
    /// Sorted by id. Never carries a `last_pass_commit`: a pass mark is
    /// earned on one machine and means nothing on another.
    pub gates: Vec<GateDef>,
    /// Sorted by name.
    pub transitions: Vec<Transition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub body: Body,
    pub content_sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error(
        "the manifest is not valid: {0}. Export it again from the store that authors the project"
    )]
    Parse(String),
    #[error(
        "the manifest is format {found}, and this version of fl reads format \
         {MANIFEST_FORMAT}. Use a version of fl that reads format {found}"
    )]
    Format { found: u64 },
    #[error(
        "the manifest was edited by hand: its content hashes to {found}, but it records \
         {recorded}. Gates are authored in the store that owns the project; export again \
         from there"
    )]
    HandEdited { recorded: String, found: String },
    #[error(
        "the manifest is inconsistent: {0}. Export it again from the store that authors the \
         project"
    )]
    Inconsistent(String),
    #[error(
        "this store authors project {0}, so it cannot import it: it already holds the source"
    )]
    AuthoringStore(ProjectId),
    #[error(
        "this store imported project {0} from a manifest, so it cannot export it. Export \
         from the store that authors it"
    )]
    NotAuthoring(ProjectId),
    #[error(
        "the manifest no longer lists gate {0}, which this store holds. A re-import would \
         remove a neighbour from every later verify, so it is refused. Restore the gate in \
         the store that authors the project and export again"
    )]
    WouldRemoveGate(GateId),
    #[error(
        "project {other} in this store already uses the root {root}. Import into a store \
         bound to another checkout, or use that project"
    )]
    RootTaken { root: String, other: ProjectId },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// How a store's gate compares with the manifest's copy of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Currency {
    Current,
    Differs,
    Absent,
}

/// SHA-256 of the body's compact JSON, as lowercase hex. serde_json writes
/// struct fields in declaration order and the body holds no maps, so equal
/// bodies always produce equal bytes.
pub fn content_sha256(body: &Body) -> Result<String, ManifestError> {
    let bytes = serde_json::to_vec(body).map_err(|e| ManifestError::Parse(e.to_string()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Every gate and transition of `project`, with pass marks cleared.
pub fn export(
    catalog: &dyn Catalog,
    project: &ProjectId,
    commit: &str,
    exported_at_unix: u64,
) -> Result<Manifest, ManifestError> {
    if catalog.get_project(project)?.is_none() {
        return Err(ManifestError::Inconsistent(format!(
            "{project} is held by this store, but it is not a project"
        )));
    }
    let mut gates = catalog.list_gates(project)?;
    for g in &mut gates {
        g.last_pass_commit = None;
    }
    gates.sort_by(|a, b| a.id.cmp(&b.id));
    let mut transitions = catalog.list_transitions(project)?;
    transitions.sort_by(|a, b| a.name.cmp(&b.name));
    let body = Body {
        format_version: MANIFEST_FORMAT,
        provenance: Provenance {
            commit: commit.to_string(),
            exported_at_unix,
        },
        project: project.clone(),
        gates,
        transitions,
    };
    let content_sha256 = content_sha256(&body)?;
    let m = Manifest {
        body,
        content_sha256,
    };
    // An export never writes a file its own parse would refuse — for
    // example a transition naming a gate of another project.
    m.check_consistent()?;
    Ok(m)
}

impl Manifest {
    /// Pretty JSON with a trailing newline, for a file a person reviews in a
    /// diff. The hash is over the compact form of the body, so layout is free.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("a manifest always serializes");
        s.push('\n');
        s
    }

    /// Parse, then refuse anything that is not exactly what an export wrote.
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        // The format is read first, loosely, so a future format is named as
        // one rather than reported as whatever field it renamed.
        let loose: serde_json::Value =
            serde_json::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))?;
        match loose.pointer("/body/format_version").and_then(|v| v.as_u64()) {
            Some(MANIFEST_FORMAT) => {}
            Some(found) => return Err(ManifestError::Format { found }),
            None => {
                return Err(ManifestError::Parse(
                    "it has no `body.format_version`".into(),
                ));
            }
        }
        let m: Manifest =
            serde_json::from_value(loose).map_err(|e| ManifestError::Parse(e.to_string()))?;
        m.verify()?;
        Ok(m)
    }

    /// The hash and the internal consistency. `parse` calls it, and so does
    /// the store's import: a `Manifest` can be built by hand (its fields are
    /// public), and the store must not trust one it did not check.
    pub fn verify(&self) -> Result<(), ManifestError> {
        let found = content_sha256(&self.body)?;
        if found != self.content_sha256 {
            return Err(ManifestError::HandEdited {
                recorded: self.content_sha256.clone(),
                found,
            });
        }
        self.check_consistent()
    }

    fn check_consistent(&self) -> Result<(), ManifestError> {
        let p = &self.body.project;
        let mut ids = BTreeSet::new();
        for g in &self.body.gates {
            if g.project != *p {
                return Err(ManifestError::Inconsistent(format!(
                    "gate {} belongs to project {}, not {p}",
                    g.id, g.project
                )));
            }
            if g.last_pass_commit.is_some() {
                return Err(ManifestError::Inconsistent(format!(
                    "gate {} carries a pass mark; pass marks are local to each machine and \
                     are never exported",
                    g.id
                )));
            }
            if !ids.insert(g.id.clone()) {
                return Err(ManifestError::Inconsistent(format!(
                    "gate {} is listed twice",
                    g.id
                )));
            }
        }
        let mut names = BTreeSet::new();
        for t in &self.body.transitions {
            if t.project != *p {
                return Err(ManifestError::Inconsistent(format!(
                    "transition `{}` belongs to project {}, not {p}",
                    t.name, t.project
                )));
            }
            if !names.insert(t.name.clone()) {
                return Err(ManifestError::Inconsistent(format!(
                    "transition `{}` is listed twice",
                    t.name
                )));
            }
            for g in &t.gates {
                if !ids.contains(g) {
                    return Err(ManifestError::Inconsistent(format!(
                        "transition `{}` names gate {g}, which the manifest does not list",
                        t.name
                    )));
                }
            }
        }
        Ok(())
    }

    /// Compare a store's gate with the manifest's copy, ignoring the pass
    /// mark (spec §4.3).
    pub fn currency_of(&self, def: &GateDef) -> Currency {
        match self.body.gates.iter().find(|g| g.id == def.id) {
            None => Currency::Absent,
            Some(g) => {
                let mut bare = def.clone();
                bare.last_pass_commit = None;
                if *g == bare {
                    Currency::Current
                } else {
                    Currency::Differs
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Regret, Selector, State};

    fn kind(program: &str) -> GateKind {
        GateKind::Command(CommandSpec {
            program: program.into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        })
    }

    fn glob() -> Selector {
        Selector::Glob {
            pattern: "**/*.rs".into(),
        }
    }

    /// Two projects in one store; `p` has two gates and a transition.
    fn store() -> (MemStore, ProjectId, GateId, GateId) {
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let other = s.add_project("/q").unwrap();
        s.add_gate(&other, "other", kind("true"), glob(), 1, "c0", "o")
            .unwrap();
        let g1 = s.add_gate(&p, "fmt", kind("true"), glob(), 1, "c1", "o").unwrap();
        let g2 = s
            .add_gate(&p, "lint", kind("/opt/tools/lint"), glob(), 1, "c1", "o")
            .unwrap();
        s.add_transition(Transition {
            project: p.clone(),
            name: "ship".into(),
            from: State::Review,
            to: State::Done,
            regret: Regret::High,
            gates: vec![g1.clone(), g2.clone()],
        })
        .unwrap();
        (s, p, g1, g2)
    }

    #[test]
    fn an_export_round_trips_through_its_file_form() {
        let (s, p, _, _) = store();
        let m = export(&s, &p, "abc", 7).unwrap();
        assert_eq!(Manifest::parse(&m.to_json()).unwrap(), m);
    }

    #[test]
    fn an_export_carries_only_the_named_project() {
        let (s, p, g1, g2) = store();
        let m = export(&s, &p, "abc", 7).unwrap();
        let ids: Vec<_> = m.body.gates.iter().map(|g| g.id.clone()).collect();
        assert_eq!(ids, vec![g1, g2]);
        assert_eq!(m.body.transitions.len(), 1);
    }

    #[test]
    fn a_pass_mark_is_never_exported() {
        let (s, p, g1, _) = store();
        let mut def = s.get_gate(&g1).unwrap().unwrap();
        def.last_pass_commit = Some("abc".into());
        s.update_gate(&def).unwrap();
        let m = export(&s, &p, "abc", 7).unwrap();
        assert!(m.body.gates.iter().all(|g| g.last_pass_commit.is_none()));
    }

    #[test]
    fn a_hand_edit_is_refused() {
        let (s, p, _, _) = store();
        let text = export(&s, &p, "abc", 7)
            .unwrap()
            .to_json()
            .replace("\"name\": \"fmt\"", "\"name\": \"fmt2\"");
        assert!(text.contains("fmt2"), "the edit must have landed");
        let err = Manifest::parse(&text).unwrap_err();
        assert!(matches!(err, ManifestError::HandEdited { .. }), "{err}");
    }

    #[test]
    fn a_future_format_is_named_as_one() {
        let (s, p, _, _) = store();
        let text = export(&s, &p, "abc", 7)
            .unwrap()
            .to_json()
            .replace("\"format_version\": 1", "\"format_version\": 9");
        let err = Manifest::parse(&text).unwrap_err();
        assert!(matches!(err, ManifestError::Format { found: 9 }), "{err}");
    }

    #[test]
    fn a_transition_naming_an_unlisted_gate_is_refused_even_with_a_correct_hash() {
        let (s, p, _, g2) = store();
        let mut m = export(&s, &p, "abc", 7).unwrap();
        m.body.gates.retain(|g| g.id != g2);
        m.content_sha256 = content_sha256(&m.body).unwrap();
        let err = Manifest::parse(&m.to_json()).unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
    }

    /// Re-hash after a hand change, so only `check_consistent` can refuse.
    fn rehashed(mut m: Manifest) -> Manifest {
        m.content_sha256 = content_sha256(&m.body).unwrap();
        m
    }

    #[test]
    fn a_gate_of_another_project_is_refused() {
        let (s, p, g1, _) = store();
        let mut m = export(&s, &p, "abc", 7).unwrap();
        let g = m.body.gates.iter_mut().find(|g| g.id == g1).unwrap();
        g.project = ProjectId(fl_core::ids::seq_iri(999));
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
    }

    #[test]
    fn a_pass_mark_in_the_file_is_refused() {
        let (s, p, _, _) = store();
        let mut m = export(&s, &p, "abc", 7).unwrap();
        m.body.gates[0].last_pass_commit = Some("abc".into());
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
    }

    #[test]
    fn a_gate_or_transition_listed_twice_is_refused() {
        let (s, p, _, _) = store();
        let mut m = export(&s, &p, "abc", 7).unwrap();
        let dup = m.body.gates[0].clone();
        m.body.gates.push(dup);
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");

        let mut m = export(&s, &p, "abc", 7).unwrap();
        let dup = m.body.transitions[0].clone();
        m.body.transitions.push(dup);
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
    }

    #[test]
    fn currency_ignores_the_pass_mark_and_sees_every_other_change() {
        let (s, p, g1, _) = store();
        let m = export(&s, &p, "abc", 7).unwrap();
        let mut def = s.get_gate(&g1).unwrap().unwrap();
        def.last_pass_commit = Some("zzz".into());
        assert_eq!(m.currency_of(&def), Currency::Current);
        def.authored_at_commit = "c2".into();
        assert_eq!(m.currency_of(&def), Currency::Differs);
        def.id = GateId(fl_core::ids::seq_iri(999));
        assert_eq!(m.currency_of(&def), Currency::Absent);
    }
}
```

Add `pub mod manifest;` near the top of `crates/store/src/lib.rs` (after the `use` lines).

- [ ] **Step 3: Run the tests**

Run: `cargo test -p fl-store manifest`
Expected: PASS, 10 tests. (The module is new, so there is no red step for the file as a whole; Step 4 supplies the red step for each refusal.)

- [ ] **Step 4: Mutation checks**

One at a time, confirm each test goes red, then restore:
- Delete the `if found != m.content_sha256 { … }` block → `a_hand_edit_is_refused` FAILS.
- Delete `g.last_pass_commit = None;` in `export` → `a_pass_mark_is_never_exported` FAILS.
- Delete the `for g in &t.gates { … }` loop in `check_consistent` → `a_transition_naming_an_unlisted_gate…` FAILS.
- Delete `bare.last_pass_commit = None;` in `currency_of` → `currency_ignores_the_pass_mark…` FAILS.
- Delete the `g.project != *p` check → `a_gate_of_another_project_is_refused` FAILS.

- [ ] **Step 4b: The manifest file never makes a gate stale**

Found in review: a gate whose selector covers `.fl/manifest.json` (`**/*.json`, `**/*`, a `changed-since` gate) went stale at every export, and the prescribed remedy — affirm, export, commit — staled it again, because the manifest commit always lands after the new stamp. The manifest's content is governed by the currency checks (spec §4.3, §4.5), so its changes are not evidence that a gate's population moved.

Append to `mod tests` in `crates/exec/src/evaluate.rs`:

```rust
    #[test]
    fn committing_the_manifest_never_makes_a_gate_stale() {
        let d = repo_with(&[("cfg/a.json", "{}")]);
        let s = MemStore::default();
        let p = setup(&s, d.path(), "true", "**/*.json", Regret::High);
        let manifest = d.path().join(fl_core::MANIFEST_PATH);
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, "{}").unwrap();
        for args in [&["add", "-A"][..], &["commit", "-qm", "manifest"][..]] {
            assert!(Command::new("git").args(args).current_dir(d.path()).status().unwrap().success());
        }
        let r = evaluate_transition(&s, &s, &p, "launch", None).unwrap();
        assert!(r.passed(), "{:?}", r.gates[0].verdict);
        assert_eq!(r.gates[0].staleness, Staleness::Fresh);
    }
```

Run `cargo test -p fl-exec committing_the_manifest` — FAIL (`Staleness::StaleFail`, and the high-regret transition fails).

In `staleness_for`, replace

```rust
    let Ok(changed) = Git::changed_between(root, &def.authored_at_commit, head) else {
        // A stamp we cannot resolve is treated as stale. An unreadable
        // provenance is not evidence of freshness.
        return true;
    };
```

with

```rust
    let Ok(mut changed) = Git::changed_between(root, &def.authored_at_commit, head) else {
        // A stamp we cannot resolve is treated as stale. An unreadable
        // provenance is not evidence of freshness.
        return true;
    };
    // ⚠ The committed manifest changes on every export, and its content is
    // governed by the currency checks, not by staleness (GitHub tracker spec
    // §4.3, §4.5). Counting it here staled every gate whose selector covers
    // it, and the remedy — affirm, export, commit — staled it again.
    let manifest = root.join(fl_core::MANIFEST_PATH);
    changed.retain(|p| *p != manifest);
```

Run it again — PASS. Mutation: delete the `retain` line → FAIL. Import `Staleness` in the test module if it is not already in scope (`use fl_core::stale::Staleness;`). The population itself is unchanged: a JSON linter over `**/*.json` still examines the manifest.

- [ ] **Step 5: Trio and commit**

```bash
git add Cargo.toml Cargo.lock crates/core/src/lib.rs crates/store/Cargo.toml \
        crates/store/src/lib.rs crates/store/src/manifest.rs crates/exec/src/evaluate.rs
git commit -m "feat(store): the manifest format, its hash, and export

A project's gates and transitions, pass marks cleared, sorted, and hashed
with SHA-256 over the body's compact JSON. parse and verify refuse a hand
edit, a future format, and an internally inconsistent file; export
refuses to write one. The manifest file never makes a gate stale: its
content is governed by the currency checks. New dependency: sha2 0.10
(MIT OR Apache-2.0). Each refusal mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 4: Import into the local store, and guards on imported definitions

**Files:**
- Modify: `crates/core/src/store.rs` (new `StoreError::Imported`)
- Modify: `crates/store/src/lib.rs` (`IMPORTS` table, `index_new`, `import_manifest`, `imported_hash`, `export_manifest`, guards in `add_gate`, `update_gate`, `add_transition`, tests)

**Interfaces:**
- Consumes: Task 3's `Manifest`, `ManifestError`, `export`.
- Produces:

```rust
// fl-core
StoreError::Imported { id: Iri, action: &'static str }
// fl-store
pub struct ImportReport { pub project: ProjectId, pub gates_added: usize,
                          pub gates_changed: usize, pub gates_unchanged: usize, pub transitions: usize,
                          pub root_moved: Option<(String, String)> } // (old root, new root)
pub const FORMAT_WITH_IMPORTS: u64 = 3;
impl RedbStore {
    pub fn import_manifest(&self, m: &Manifest, root: &str) -> Result<ImportReport, ManifestError>;
    pub fn imported_hash(&self, project: &ProjectId) -> Result<Option<String>, StoreError>;
    pub fn export_manifest(&self, project: &ProjectId, commit: &str, exported_at_unix: u64) -> Result<Manifest, ManifestError>;
}
```

- [ ] **Step 1: Add the error variant**

In `crates/core/src/store.rs`, add to `StoreError` after `AlreadyExists`:

```rust
    /// ⚠ The item belongs to a project this store imported from a manifest
    /// (GitHub tracker spec §4.2). Its definition is authored elsewhere, and
    /// a local edit would make this copy disagree with the manifest every
    /// other reader resolves.
    #[error(
        "this store imported {id}'s project from a manifest, so it cannot {action} it. \
         Change it in the store that authors the project, run `fl manifest export` there, \
         commit, then run `fl manifest import` here."
    )]
    Imported { id: Iri, action: &'static str },
```

- [ ] **Step 2: Write the failing tests**

Append to `mod tests` in `crates/store/src/lib.rs`:

```rust
    use crate::manifest::{Manifest, ManifestError, content_sha256};
    use fl_core::model::{Regret, Transition};

    /// An authoring store with one project: gates `fmt` and `lint`, and a
    /// transition over both. Returns the store's guard too.
    fn authoring() -> (RedbStore, tempfile::TempDir, ProjectId, GateId, GateId) {
        let (s, dir) = fresh();
        let p = s.add_project("/author").unwrap();
        let g1 = s.add_gate(&p, "fmt", kind(), selector(), 1, "c1", "o").unwrap();
        let g2 = s.add_gate(&p, "lint", kind(), selector(), 1, "c1", "o").unwrap();
        s.add_transition(Transition {
            project: p.clone(),
            name: "ship".into(),
            from: State::Review,
            to: State::Done,
            regret: Regret::High,
            gates: vec![g1.clone(), g2.clone()],
        })
        .unwrap();
        (s, dir, p, g1, g2)
    }

    fn stamp(s: &RedbStore, g: &GateId, commit: &str) {
        let mut def = s.get_gate(g).unwrap().unwrap();
        def.last_pass_commit = Some(commit.into());
        s.update_gate(&def).unwrap();
    }

    #[test]
    fn an_import_writes_every_item_under_its_own_iri_and_marks_the_project() {
        let (a, _ga, p, g1, g2) = authoring();
        let m = a.export_manifest(&p, "c1", 7).unwrap();
        let (b, _gb) = fresh();
        let report = b.import_manifest(&m, "/elsewhere").unwrap();
        assert_eq!((report.gates_added, report.transitions), (2, 1));
        assert_eq!(b.get_project(&p).unwrap().unwrap().root, "/elsewhere");
        assert_eq!(b.get_gate(&g1).unwrap(), a.get_gate(&g1).unwrap());
        assert!(b.owns(g2.iri()).unwrap());
        assert!(b.get_transition(&p, "ship").unwrap().is_some());
        assert_eq!(b.imported_hash(&p).unwrap(), Some(m.content_sha256.clone()));
        assert!(b.handle_of(Kind::Gate, g1.iri()).unwrap().is_some());
    }

    #[test]
    fn a_store_that_never_imported_reads_as_nothing_imported() {
        let (s, _g) = fresh();
        let p = s.add_project("/p").unwrap();
        assert_eq!(s.imported_hash(&p).unwrap(), None);
    }

    #[test]
    fn the_authoring_store_refuses_to_import_its_own_project() {
        let (a, _g, p, _, _) = authoring();
        let m = a.export_manifest(&p, "c1", 7).unwrap();
        let err = a.import_manifest(&m, "/author").unwrap_err();
        assert!(matches!(err, ManifestError::AuthoringStore(_)), "{err}");
    }

    #[test]
    fn an_importing_store_refuses_to_export() {
        let (a, _ga, p, _, _) = authoring();
        let (b, _gb) = fresh();
        b.import_manifest(&a.export_manifest(&p, "c1", 7).unwrap(), "/x")
            .unwrap();
        let err = b.export_manifest(&p, "c1", 8).unwrap_err();
        assert!(matches!(err, ManifestError::NotAuthoring(_)), "{err}");
    }

    #[test]
    fn an_imported_gate_can_earn_a_pass_mark_but_cannot_be_edited() {
        let (a, _ga, p, g1, _) = authoring();
        let (b, _gb) = fresh();
        b.import_manifest(&a.export_manifest(&p, "c1", 7).unwrap(), "/x")
            .unwrap();
        stamp(&b, &g1, "c1");
        assert_eq!(
            b.get_gate(&g1).unwrap().unwrap().last_pass_commit.as_deref(),
            Some("c1")
        );

        let mut edited = b.get_gate(&g1).unwrap().unwrap();
        edited.name = "renamed".into();
        let err = b.update_gate(&edited).unwrap_err();
        assert!(matches!(err, StoreError::Imported { .. }), "{err}");

        let err = b.add_gate(&p, "new", kind(), selector(), 1, "c1", "o").unwrap_err();
        assert!(matches!(err, StoreError::Imported { .. }), "{err}");

        let err = b
            .add_transition(Transition {
                project: p.clone(),
                name: "other".into(),
                from: State::Todo,
                to: State::Doing,
                regret: Regret::Low,
                gates: vec![],
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::Imported { .. }), "{err}");
    }

    #[test]
    fn a_reimport_keeps_the_mark_of_an_unchanged_gate_and_drops_a_changed_ones() {
        let (a, _ga, p, g1, g2) = authoring();
        let (b, _gb) = fresh();
        b.import_manifest(&a.export_manifest(&p, "c1", 7).unwrap(), "/x")
            .unwrap();
        stamp(&b, &g1, "c1");
        stamp(&b, &g2, "c1");

        let mut changed = a.get_gate(&g2).unwrap().unwrap();
        changed.authored_at_commit = "c2".into();
        a.update_gate(&changed).unwrap();
        let g3 = a.add_gate(&p, "new", kind(), selector(), 1, "c2", "o").unwrap();

        let report = b
            .import_manifest(&a.export_manifest(&p, "c2", 8).unwrap(), "/x")
            .unwrap();
        assert_eq!(
            (report.gates_added, report.gates_changed, report.gates_unchanged),
            (1, 1, 1)
        );
        assert_eq!(
            b.get_gate(&g1).unwrap().unwrap().last_pass_commit.as_deref(),
            Some("c1")
        );
        assert_eq!(b.get_gate(&g2).unwrap().unwrap().last_pass_commit, None);
        assert_eq!(b.get_gate(&g2).unwrap().unwrap().authored_at_commit, "c2");
        assert!(b.handle_of(Kind::Gate, g3.iri()).unwrap().is_some());
    }

    #[test]
    fn an_import_refuses_a_manifest_it_did_not_verify() {
        let (a, _ga, p, _, _) = authoring();
        let mut m = a.export_manifest(&p, "c1", 7).unwrap();
        m.body.gates[0].name = "edited".into();
        let (b, _gb) = fresh();
        let err = b.import_manifest(&m, "/x").unwrap_err();
        assert!(matches!(err, ManifestError::HandEdited { .. }), "{err}");
        assert!(!b.owns(p.iri()).unwrap(), "nothing may be written");
    }

    #[test]
    fn a_fresh_import_onto_a_root_another_project_uses_is_refused() {
        let (a, _ga, p, _, _) = authoring();
        let (b, _gb) = fresh();
        let local = b.add_project("/x").unwrap();
        let err = b
            .import_manifest(&a.export_manifest(&p, "c1", 7).unwrap(), "/x")
            .unwrap_err();
        assert!(
            matches!(err, ManifestError::RootTaken { ref other, .. } if *other == local),
            "{err}"
        );
    }

    #[test]
    fn a_reimport_from_another_checkout_moves_the_root_and_says_so() {
        let (a, _ga, p, _, _) = authoring();
        let (b, _gb) = fresh();
        let m = a.export_manifest(&p, "c1", 7).unwrap();
        assert_eq!(b.import_manifest(&m, "/one").unwrap().root_moved, None);
        let report = b.import_manifest(&m, "/two").unwrap();
        assert_eq!(report.root_moved, Some(("/one".into(), "/two".into())));
        assert_eq!(b.get_project(&p).unwrap().unwrap().root, "/two");
    }

    #[test]
    fn a_reimport_mirrors_the_manifests_transitions() {
        let (a, _ga, p, _, _) = authoring();
        let (b, _gb) = fresh();
        let first = a.export_manifest(&p, "c1", 7).unwrap();
        b.import_manifest(&first, "/x").unwrap();
        let mut body = first.body.clone();
        body.transitions.clear();
        let without = Manifest {
            content_sha256: content_sha256(&body).unwrap(),
            body,
        };
        b.import_manifest(&without, "/x").unwrap();
        assert!(b.list_transitions(&p).unwrap().is_empty());
    }

    #[test]
    fn imported_hash_of_a_project_this_store_never_held_is_not_owned() {
        let (s, _g) = fresh();
        let err = s
            .imported_hash(&ProjectId(seq_iri(42)))
            .unwrap_err();
        assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
    }

    #[test]
    fn a_store_that_imported_is_format_3_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.redb");
        let (a, _ga, p, _, _) = authoring();
        {
            let b = RedbStore::open(&path).unwrap();
            b.import_manifest(&a.export_manifest(&p, "c1", 7).unwrap(), "/x")
                .unwrap();
        }
        let db = redb::Database::open(&path).unwrap();
        let tx = db.begin_read().unwrap();
        let meta = tx.open_table(META).unwrap();
        assert_eq!(
            meta.get(FORMAT_KEY).unwrap().map(|v| v.value()),
            Some(FORMAT_WITH_IMPORTS),
            "an older fl must refuse this store rather than ignore its imports"
        );
        drop(meta);
        drop(tx);
        drop(db);
        let b = RedbStore::open(&path).unwrap();
        assert!(b.imported_hash(&p).unwrap().is_some());
    }

    #[test]
    fn a_reimport_that_removes_a_gate_is_refused_and_changes_nothing() {
        let (a, _ga, p, g1, g2) = authoring();
        let (b, _gb) = fresh();
        let first = a.export_manifest(&p, "c1", 7).unwrap();
        b.import_manifest(&first, "/x").unwrap();

        let mut body = first.body.clone();
        body.gates.retain(|g| g.id != g2);
        for t in &mut body.transitions {
            t.gates.retain(|g| *g != g2);
        }
        let shrunk = Manifest {
            content_sha256: content_sha256(&body).unwrap(),
            body,
        };
        let err = b.import_manifest(&shrunk, "/x").unwrap_err();
        assert!(
            matches!(err, ManifestError::WouldRemoveGate(ref g) if *g == g2),
            "{err}"
        );
        assert_eq!(b.imported_hash(&p).unwrap(), Some(first.content_sha256));
        assert!(b.get_gate(&g1).unwrap().is_some());
    }
```

`kind()`, `selector()`, `fresh()` and the `State` import already exist in this test module (lines ~717-1140); reuse them.

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p fl-store`
Expected: FAIL to compile — `import_manifest`, `imported_hash` and `export_manifest` do not exist.

- [ ] **Step 4: Extract `index_new` from `insert_new_with_id`**

In `crates/store/src/lib.rs`, add a free function after `alias_primary`:

```rust
/// Index `id` as a new item of `kind` and give it the next handle, inside a
/// write transaction the caller already holds. Refuses an id already
/// indexed: an insert never overwrites. Shared by `insert_new_with_id` and
/// `import_manifest`, so a minted id and an imported id are indexed the same
/// way.
fn index_new(tx: &redb::WriteTransaction, id: &Iri, kind: Kind) -> Result<(), StoreError> {
    let mut ids = tx.open_table(IDS).map_err(backend)?;
    if ids.get(id.as_str()).map_err(backend)?.is_some() {
        return Err(StoreError::AlreadyExists(id.clone()));
    }
    ids.insert(id.as_str(), kind.as_wire()).map_err(backend)?;

    let key = format!("next_handle:{}", kind.as_wire());
    let mut meta = tx.open_table(META).map_err(backend)?;
    let n = meta
        .get(key.as_str())
        .map_err(backend)?
        .map(|v| v.value())
        .unwrap_or(0)
        + 1;
    meta.insert(key.as_str(), n).map_err(backend)?;

    tx.open_table(HANDLES)
        .map_err(backend)?
        .insert((kind.as_wire(), n), id.as_str())
        .map_err(backend)?;
    tx.open_table(HANDLE_OF)
        .map_err(backend)?
        .insert(id.as_str(), n)
        .map_err(backend)?;
    Ok(())
}
```

and reduce the body of `insert_new_with_id` (keep its doc comment) to:

```rust
        let json = serde_json::to_string(&build(id.clone())).map_err(backend)?;
        let tx = self.db.begin_write().map_err(backend)?;
        index_new(&tx, &id, kind)?;
        tx.open_table(table)
            .map_err(backend)?
            .insert(id.as_str(), json.as_str())
            .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(id)
```

Do not run the tests here: the Step 2 tests still do not compile. Step 6 runs everything, including `a_failed_insert_does_not_advance_the_shared_id_counter`, which covers this refactor.

- [ ] **Step 5: Add the table, `imported_hash`, the guards, export and import**

Add the table and the format beside the others:

```rust
/// project → the `content_sha256` of the manifest it was imported from.
/// Created by the first import; a store without it has imported nothing
/// (GitHub tracker spec §4.2).
const IMPORTS: TableDefinition<&str, &str> = TableDefinition::new("imports");

/// ⚠ The format of a store that holds an import. The first import raises
/// the store from 2 to 3 in the same transaction, so an older fl — which
/// knows nothing of imports and would let a person edit an imported gate —
/// REFUSES the store with `FormatVersion` instead of ignoring the mark.
/// This build opens both. A store that never imports stays 2 and still
/// opens in older builds.
pub const FORMAT_WITH_IMPORTS: u64 = 3;
```

In `RedbStore::open`, change the accepting arm to:

```rust
            Some(Some(v)) if v == FORMAT_VERSION || v == FORMAT_WITH_IMPORTS => {}
```

Leave `create_tables` writing `FORMAT_VERSION`, and leave the refusal's `expected: FORMAT_VERSION` as it is (the existing test `a_store_from_before_format_versioning_is_refused_with_a_remedy` pins it).

Add to `impl RedbStore`:

```rust
    /// The hash of the manifest `project` was imported from, or `None` if
    /// this store authors it. A project this store never held is `NotOwned`:
    /// `None` would read as "authored here".
    pub fn imported_hash(&self, project: &ProjectId) -> Result<Option<String>, StoreError> {
        self.check_kind(project.iri(), Kind::Project)?;
        let tx = self.db.begin_read().map_err(backend)?;
        let table = match tx.open_table(IMPORTS) {
            Ok(t) => t,
            // The first import creates the table. Its absence is "nothing
            // imported", read from a table that does not exist yet — not a
            // failure to look.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let hash = table
            .get(project.iri().as_str())
            .map_err(backend)?
            .map(|v| v.value().to_string());
        Ok(hash)
    }

    fn refuse_if_imported(
        &self,
        project: &ProjectId,
        action: &'static str,
    ) -> Result<(), StoreError> {
        if self.imported_hash(project)?.is_some() {
            return Err(StoreError::Imported {
                id: project.iri().clone(),
                action,
            });
        }
        Ok(())
    }

    /// Export `project`, which this store must author.
    pub fn export_manifest(
        &self,
        project: &ProjectId,
        commit: &str,
        exported_at_unix: u64,
    ) -> Result<Manifest, ManifestError> {
        self.check_kind(project.iri(), Kind::Project)?;
        if self.imported_hash(project)?.is_some() {
            return Err(ManifestError::NotAuthoring(project.clone()));
        }
        manifest::export(self, project, commit, exported_at_unix)
    }

    /// Write the manifest's project, gates and transitions under their own
    /// IRIs, and mark the project imported — in ONE write transaction.
    /// Everything that can refuse is decided before anything is written.
    pub fn import_manifest(&self, m: &Manifest, root: &str) -> Result<ImportReport, ManifestError> {
        // A `Manifest` can be built by hand; the store checks it itself.
        m.verify()?;
        let body = &m.body;
        let project = &body.project;

        let held_project = match self.locate(project.iri()) {
            Ok((_, Kind::Project)) => true,
            Ok((_, found)) => {
                return Err(StoreError::WrongKind {
                    id: project.iri().clone(),
                    expected: Kind::Project,
                    found,
                }
                .into());
            }
            Err(StoreError::NotOwned { .. }) => false,
            Err(e) => return Err(e.into()),
        };
        if held_project && self.imported_hash(project)?.is_none() {
            return Err(ManifestError::AuthoringStore(project.clone()));
        }
        // Another project on the same root would give one checkout two
        // sets of gates and two handles, with nothing said.
        for other in self.list_projects()? {
            if other.id != *project && other.root == root {
                return Err(ManifestError::RootTaken {
                    root: root.to_string(),
                    other: other.id,
                });
            }
        }
        let root_moved = match held_project {
            true => {
                let held = self
                    .get_project(project)?
                    .ok_or_else(|| ManifestError::Inconsistent(format!("{project} vanished")))?;
                (held.root != root).then(|| (held.root, root.to_string()))
            }
            false => None,
        };

        let held_gates = if held_project {
            self.list_gates(project)?
        } else {
            vec![]
        };
        for g in &held_gates {
            if !body.gates.iter().any(|m| m.id == g.id) {
                return Err(ManifestError::WouldRemoveGate(g.id.clone()));
            }
        }
        // Transitions mirror the manifest: one it no longer lists is
        // removed. (Neither side has a command that removes one today.)
        let stale_transitions: Vec<String> = if held_project {
            self.list_transitions(project)?
                .into_iter()
                .filter(|t| !body.transitions.iter().any(|m| m.name == t.name))
                .map(|t| t.name)
                .collect()
        } else {
            vec![]
        };

        let mut report = ImportReport {
            project: project.clone(),
            gates_added: 0,
            gates_changed: 0,
            gates_unchanged: 0,
            transitions: body.transitions.len(),
            root_moved,
        };
        // (definition to write, whether it is new to this store)
        let mut writes: Vec<(&GateDef, bool)> = Vec::new();
        for g in &body.gates {
            match self.locate(g.id.iri()) {
                Err(StoreError::NotOwned { .. }) => {
                    writes.push((g, true));
                    report.gates_added += 1;
                }
                Ok((_, Kind::Gate)) => {
                    let Some(held) = held_gates.iter().find(|h| h.id == g.id) else {
                        return Err(ManifestError::Inconsistent(format!(
                            "gate {} is held by this store under another project",
                            g.id
                        )));
                    };
                    let mut bare = held.clone();
                    bare.last_pass_commit = None;
                    if bare == *g {
                        report.gates_unchanged += 1;
                    } else {
                        writes.push((g, false));
                        report.gates_changed += 1;
                    }
                }
                Ok((_, found)) => {
                    return Err(StoreError::WrongKind {
                        id: g.id.iri().clone(),
                        expected: Kind::Gate,
                        found,
                    }
                    .into());
                }
                Err(e) => return Err(e.into()),
            }
        }

        let tx = self.db.begin_write().map_err(backend)?;
        if !held_project || report.root_moved.is_some() {
            if !held_project {
                index_new(&tx, project.iri(), Kind::Project)?;
            }
            let json = serde_json::to_string(&Project {
                id: project.clone(),
                root: root.to_string(),
            })
            .map_err(backend)?;
            tx.open_table(PROJECTS)
                .map_err(backend)?
                .insert(project.iri().as_str(), json.as_str())
                .map_err(backend)?;
        }
        for (g, is_new) in &writes {
            if *is_new {
                index_new(&tx, g.id.iri(), Kind::Gate)?;
            }
            let json = serde_json::to_string(g).map_err(backend)?;
            tx.open_table(GATES)
                .map_err(backend)?
                .insert(g.id.iri().as_str(), json.as_str())
                .map_err(backend)?;
        }
        {
            let mut table = tx.open_table(TRANSITIONS).map_err(backend)?;
            for name in &stale_transitions {
                table
                    .remove((project.iri().as_str(), name.as_str()))
                    .map_err(backend)?;
            }
            for t in &body.transitions {
                let json = serde_json::to_string(t).map_err(backend)?;
                table
                    .insert((t.project.iri().as_str(), t.name.as_str()), json.as_str())
                    .map_err(backend)?;
            }
        }
        tx.open_table(IMPORTS)
            .map_err(backend)?
            .insert(project.iri().as_str(), m.content_sha256.as_str())
            .map_err(backend)?;
        tx.open_table(META)
            .map_err(backend)?
            .insert(FORMAT_KEY, FORMAT_WITH_IMPORTS)
            .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(report)
    }
```

Add, next to `RedbStore`:

```rust
/// What an import did, for the person who ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub project: ProjectId,
    pub gates_added: usize,
    pub gates_changed: usize,
    pub gates_unchanged: usize,
    pub transitions: usize,
    /// `(old, new)` when a re-import came from another checkout. The
    /// project's gates now run over the new root, and the CLI says so.
    pub root_moved: Option<(String, String)>,
}
```

and `use crate::manifest::{Manifest, ManifestError};` at the top.

Guards, in `impl Catalog for RedbStore`:
- `add_gate`: after `self.check_kind(project.iri(), Kind::Project)?;` add `self.refuse_if_imported(project, "add a gate to")?;`
- `add_transition`: after its `check_kind` add `self.refuse_if_imported(&t.project, "add a transition to")?;`
- `update_gate` becomes:

```rust
    fn update_gate(&self, def: &GateDef) -> Result<(), StoreError> {
        let Some(held) = self.get_gate(&def.id)? else {
            return Err(StoreError::NoSuchGate(def.id.clone()));
        };
        // An imported gate may earn a local pass mark and nothing else
        // (GitHub tracker spec §4.2).
        if self.imported_hash(&held.project)?.is_some() {
            let (mut before, mut after) = (held, def.clone());
            before.last_pass_commit = None;
            after.last_pass_commit = None;
            if before != after {
                return Err(StoreError::Imported {
                    id: def.id.iri().clone(),
                    action: "change the definition of",
                });
            }
        }
        self.put_json(GATES, def.id.iri(), def)
    }
```

`Project` must be imported in `lib.rs` (it already is via `fl_core::model::{…, Project, …}`).

- [ ] **Step 6: Run the tests**

Run: `cargo test -p fl-store`
Expected: PASS, including the 13 new tests and the conformance test.

- [ ] **Step 7: Mutation checks**

One at a time, confirm red, then restore:
- Remove `self.refuse_if_imported(project, "add a gate to")?;` → `an_imported_gate_can_earn…` FAILS.
- Make `update_gate` skip the imported check → same test FAILS.
- Remove the `WouldRemoveGate` loop → `a_reimport_that_removes_a_gate…` FAILS.
- Remove `bare.last_pass_commit = None;` in the import → `a_reimport_keeps_the_mark…` FAILS (the unchanged gate is counted as changed and loses its mark).
- Remove the `AuthoringStore` check → `the_authoring_store_refuses…` FAILS.
- Remove `m.verify()?;` from the import → `an_import_refuses_a_manifest_it_did_not_verify` FAILS.
- Remove the `RootTaken` loop → `a_fresh_import_onto_a_root…` FAILS.
- Remove the `META` write in the import → `a_store_that_imported_is_format_3…` FAILS.
- Remove the `stale_transitions` removal → `a_reimport_mirrors…` FAILS.
- Remove `self.check_kind(…)` from `imported_hash` → `imported_hash_of_a_project…` FAILS.

- [ ] **Step 8: Trio and commit**

```bash
git add crates/core/src/store.rs crates/store/src/lib.rs
git commit -m "feat(store): import a manifest under its own IRIs, and guard imported definitions

One write transaction writes the project, gates, transitions and the
import mark, and raises the store to format 3 so an older fl refuses it
rather than ignoring the mark. Every refusal is decided first, and the
store re-verifies the manifest itself. An imported gate may earn a local
pass mark and nothing else; a re-import keeps the mark of an unchanged
gate, drops a changed one's, refuses to remove a gate, mirrors the
transitions, and moves the root when it came from another checkout.
Each guard mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

### Task 5: `fl manifest export | import | check`, and the currency guard on gate runs

**Files:**
- Modify: `crates/exec/src/git.rs` (`Git::is_committed` and tests)
- Create: `crates/cli/src/cmd/manifest.rs`
- Modify: `crates/cli/src/cmd/mod.rs`, `crates/cli/src/main.rs` (new `Manifest` subcommand in `Command`, `iris`, `has_handle`, `project_root`, dispatch)
- Modify: `crates/cli/src/cmd/gate.rs` (`Run`), `crates/cli/src/cmd/check.rs`, `crates/cli/src/cmd/record.rs` (`Move`), `crates/cli/src/cmd/finding.rs` (`Reproduce`, `Verify`) — one guard call each
- Create: `crates/cli/tests/manifest.rs`, `docs/sharing-gates.md`
- Modify: `docs/getting-started.md` (the `fl --help` transcript, and one linking sentence)

**Interfaces:**
- Consumes: Task 4's `RedbStore::{export_manifest, import_manifest, imported_hash}`, Task 3's `Manifest::{parse, to_json, currency_of}`, `MANIFEST_PATH`.
- Produces (plan B calls `ensure_publishable` before it writes a gate IRI into an issue):

```rust
// fl-exec
impl Git { pub fn is_committed(root: &Path, rel: &str) -> Result<bool, ExecError>; }
// fl-cli, crates/cli/src/cmd/manifest.rs
pub fn read(root: &Path) -> anyhow::Result<Manifest>;
pub fn ensure_import_current(store: &RedbStore, project: &ProjectId) -> anyhow::Result<()>;
pub fn ensure_publishable(store: &RedbStore, project: &ProjectId, gate: Option<&GateId>) -> anyhow::Result<()>;
```

- [ ] **Step 1: `Git::is_committed`, test first**

Append to `mod tests` in `crates/exec/src/git.rs`:

```rust
    #[test]
    fn is_committed_tells_untracked_modified_and_committed_apart() {
        let d = repo();
        fs::create_dir_all(d.path().join(".fl")).unwrap();
        fs::write(d.path().join(".fl/manifest.json"), "{}").unwrap();
        assert!(!Git::is_committed(d.path(), ".fl/manifest.json").unwrap(), "untracked");
        commit(d.path(), "manifest");
        assert!(Git::is_committed(d.path(), ".fl/manifest.json").unwrap(), "committed");
        fs::write(d.path().join(".fl/manifest.json"), "{ }").unwrap();
        assert!(!Git::is_committed(d.path(), ".fl/manifest.json").unwrap(), "modified");
    }

    #[test]
    fn is_committed_outside_a_repository_is_an_error_not_false() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            Git::is_committed(d.path(), ".fl/manifest.json"),
            Err(ExecError::Git(_))
        ));
    }
```

Run `cargo test -p fl-exec is_committed` — FAIL (no such function). Then add to `impl Git`:

```rust
    /// Whether `rel` (relative to `root`) is tracked and has no uncommitted
    /// change. Untracked is `false`, not an error; a git that cannot answer
    /// is an error, never `false`.
    pub fn is_committed(root: &Path, rel: &str) -> Result<bool, ExecError> {
        let tracked = !git(root, &["ls-files", "--", rel])?.is_empty();
        let clean = git(root, &["status", "--porcelain", "--", rel])?.is_empty();
        Ok(tracked && clean)
    }
```

Run again — PASS.

- [ ] **Step 2: Write the CLI tests**

Create `crates/cli/tests/manifest.rs`:

```rust
//! `fl manifest` and the currency guard, driven as a black box across two
//! "machines": two private config/data homes, one authoring clone and one
//! importing clone of the same repository.

use assert_cmd::Command;
use predicates::str::contains;
use std::path::Path;
use std::process::Command as Sys;

fn git(dir: &Path, args: &[&str]) {
    let out = Sys::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["init", "-q"]);
    git(d.path(), &["config", "user.email", "t@example.com"]);
    git(d.path(), &["config", "user.name", "t"]);
    std::fs::write(d.path().join("a.rs"), "fn a() {}").unwrap();
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-qm", "first"]);
    d
}

/// One machine: private config and data homes.
struct Machine {
    config: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        Self {
            config: tempfile::tempdir().unwrap(),
            data: tempfile::tempdir().unwrap(),
        }
    }
    fn fl(&self, cwd: &Path) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", self.config.path())
            .env("XDG_DATA_HOME", self.data.path())
            .env_remove("FL_DB")
            .current_dir(cwd);
        c
    }
}

/// The authoring machine registers the repo, adds a gate and a transition,
/// exports, and commits the manifest.
fn authored() -> (Machine, tempfile::TempDir) {
    let m = Machine::new();
    let r = repo();
    m.fl(r.path()).args(["project", "add", "."]).assert().success();
    m.fl(r.path())
        .args(["gate", "add", "--project", "1", "--name", "rs", "--glob", "*.rs", "--program", "true"])
        .assert()
        .success();
    m.fl(r.path())
        .args(["transition", "add", "--project", "1", "--name", "ship", "--from", "review",
               "--to", "done", "--regret", "high", "--gate", "1"])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "export", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("\trs\ttrue"));
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "manifest"]);
    (m, r)
}

fn clone_of(src: &Path) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["clone", "-q", src.to_str().unwrap(), "."]);
    d
}

#[test]
fn a_clean_committed_manifest_checks_as_current() {
    let (m, r) = authored();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .success()
        .stdout(contains("current"));
}

#[test]
fn an_uncommitted_manifest_is_refused_by_check() {
    let m = Machine::new();
    let r = repo();
    m.fl(r.path()).args(["project", "add", "."]).assert().success();
    m.fl(r.path())
        .args(["gate", "add", "--project", "1", "--name", "rs", "--glob", "*.rs", "--program", "true"])
        .assert()
        .success();
    m.fl(r.path()).args(["manifest", "export", "--project", "1"]).assert().success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("commit"));
}

#[test]
fn a_gate_changed_since_export_is_refused_by_check() {
    let (m, r) = authored();
    std::fs::write(r.path().join("b.rs"), "fn b() {}").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "more"]);
    m.fl(r.path()).args(["gate", "affirm", "1"]).assert().success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest export"));
}

#[test]
fn another_machine_imports_and_runs_the_gate() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stdout(contains("1 added"));
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .success()
        .stdout(contains("PASS"));
}

#[test]
fn an_imported_gate_cannot_be_affirmed_locally() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other.fl(c.path()).args(["manifest", "import"]).assert().success();
    other
        .fl(c.path())
        .args(["gate", "affirm", "1"])
        .assert()
        .failure()
        .stderr(contains("imported from a manifest"));
}

#[test]
fn a_manifest_edited_after_import_stops_the_gate_from_running() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other.fl(c.path()).args(["manifest", "import"]).assert().success();
    let path = c.path().join(".fl/manifest.json");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("\"name\": \"rs\"", "\"name\": \"rs2\"")).unwrap();
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .failure()
        .stderr(contains("edited by hand"));
}

#[test]
fn a_manifest_that_moved_on_since_import_stops_the_gate_until_reimport() {
    let (author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other.fl(c.path()).args(["manifest", "import"]).assert().success();

    std::fs::write(r.path().join("b.rs"), "fn b() {}").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "more"]);
    author.fl(r.path()).args(["gate", "affirm", "1"]).assert().success();
    author.fl(r.path()).args(["manifest", "export", "--project", "1"]).assert().success();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "re-export"]);
    git(c.path(), &["pull", "-q"]);

    other
        .fl(c.path())
        .args(["check", "ship", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
    other
        .fl(c.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stdout(contains("1 changed"));
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .success();
}

/// An importing machine whose working-tree manifest moved on since import.
fn stale_import() -> (Machine, tempfile::TempDir, Machine, tempfile::TempDir) {
    let (author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other.fl(c.path()).args(["manifest", "import"]).assert().success();
    std::fs::write(r.path().join("b.rs"), "fn b() {}").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "more"]);
    author.fl(r.path()).args(["gate", "affirm", "1"]).assert().success();
    author.fl(r.path()).args(["manifest", "export", "--project", "1"]).assert().success();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-qm", "re-export"]);
    git(c.path(), &["pull", "-q"]);
    (author, r, other, c)
}

#[test]
fn a_gated_move_is_refused_on_a_stale_import_and_an_ungated_one_is_not() {
    let (_a, _r, other, c) = stale_import();
    other
        .fl(c.path())
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    // todo → review: no transition covers it, so no gate runs.
    other
        .fl(c.path())
        .args(["record", "move", "1", "--to", "review"])
        .assert()
        .success();
    // review → done: `ship` covers it.
    other
        .fl(c.path())
        .args(["record", "move", "1", "--to", "done"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
}

#[test]
fn reproduce_and_verify_are_refused_on_a_stale_import() {
    let (_a, _r, other, c) = stale_import();
    other
        .fl(c.path())
        .args(["record", "add", "--project", "1", "--title", "t"])
        .assert()
        .success();
    other
        .fl(c.path())
        .args(["finding", "raise", "--record", "1", "--claim", "c", "--by", "r"])
        .assert()
        .success();
    other
        .fl(c.path())
        .args(["finding", "reproduce", "1", "--gate", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
    other
        .fl(c.path())
        .args(["finding", "verify", "1"])
        .assert()
        .failure()
        .stderr(contains("fl manifest import"));
}

#[test]
fn check_refuses_a_gate_or_transition_added_since_export() {
    let (m, r) = authored();
    m.fl(r.path())
        .args(["gate", "add", "--project", "1", "--name", "late", "--glob", "*.rs", "--program", "true"])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("not in the manifest"));

    let (m, r) = authored();
    m.fl(r.path())
        .args(["transition", "add", "--project", "1", "--name", "late", "--from", "todo",
               "--to", "doing", "--regret", "low", "--gate", "1"])
        .assert()
        .success();
    m.fl(r.path())
        .args(["manifest", "check", "--project", "1"])
        .assert()
        .failure()
        .stderr(contains("transitions changed"));
}

#[test]
fn a_reimport_from_another_checkout_says_the_root_moved() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c1 = clone_of(r.path());
    let c2 = clone_of(r.path());
    other.fl(c1.path()).args(["manifest", "import"]).assert().success();
    other
        .fl(c2.path())
        .args(["manifest", "import"])
        .assert()
        .success()
        .stdout(contains("moved"));
}

#[test]
fn a_missing_manifest_on_an_importing_machine_is_refused_by_path() {
    let (_author, r) = authored();
    let other = Machine::new();
    let c = clone_of(r.path());
    other.fl(c.path()).args(["manifest", "import"]).assert().success();
    std::fs::remove_file(c.path().join(".fl/manifest.json")).unwrap();
    other
        .fl(c.path())
        .args(["gate", "run", "1"])
        .assert()
        .failure()
        .stderr(contains(".fl/manifest.json"));
}
```


Run: `cargo test -p fl-cli --test manifest`
Expected: FAIL — `manifest` is not a subcommand.

- [ ] **Step 3: Write `cmd/manifest.rs`**

Create `crates/cli/src/cmd/manifest.rs`:

```rust
//! `fl manifest` (GitHub tracker spec §4), and the checks other commands
//! make before they run an imported gate or publish a gate's IRI.

use crate::refs::{self, Ref};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fl_core::ids::{GateId, ProjectId};
use fl_core::model::GateKind;
use fl_core::store::Catalog;
use fl_core::{Iri, Kind};
use fl_exec::git::Git;
use fl_store::RedbStore;
use fl_store::manifest::{Currency, MANIFEST_PATH, Manifest};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum Cmd {
    /// Write `.fl/manifest.json` for a project this store authors.
    Export {
        #[arg(long)]
        project: Ref,
    },
    /// Import `.fl/manifest.json` from a project root into this store.
    Import {
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Check that the manifest is intact, matches this store, and is committed.
    Check {
        #[arg(long)]
        project: Ref,
    },
}

impl Cmd {
    fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Export { project } | Cmd::Check { project } => vec![project],
            Cmd::Import { .. } => vec![],
        }
    }
    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }
    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }
    /// `Import` works on the store bound to the root it imports into.
    pub fn root(&self) -> Option<&Path> {
        match self {
            Cmd::Import { root } => Some(root),
            _ => None,
        }
    }
}

/// The manifest at `root`, parsed and verified. A missing file is refused by
/// its path: "no manifest" is never read as "nothing to check".
pub fn read(root: &Path) -> Result<Manifest> {
    let path = root.join(MANIFEST_PATH);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read the manifest at {}", path.display()))?;
    Manifest::parse(&text).with_context(|| format!("{} was refused", path.display()))
}

fn root_of(store: &RedbStore, project: &ProjectId) -> Result<PathBuf> {
    let Some(p) = store.get_project(project)? else {
        bail!("{project} is held by this store, but it is not a project");
    };
    Ok(PathBuf::from(p.root))
}

/// Spec §4.5: on an importing machine, the working tree's manifest must be
/// the one this store imported. A project this store authors passes.
pub fn ensure_import_current(store: &RedbStore, project: &ProjectId) -> Result<()> {
    let Some(recorded) = store.imported_hash(project)? else {
        return Ok(());
    };
    let root = root_of(store, project)?;
    let m = read(&root)?;
    if m.content_sha256 != recorded {
        bail!(
            "the manifest at {} changed since this store imported it (imported {recorded}, \
             now {}). Run `fl manifest import` before running its gates.",
            root.join(MANIFEST_PATH).display(),
            m.content_sha256
        );
    }
    Ok(())
}

/// Spec §4.3–§4.5: before a gate's IRI is written where another machine
/// reads it, the committed manifest must carry that gate as this store has
/// it. `gate: None` checks every gate of the project.
pub fn ensure_publishable(
    store: &RedbStore,
    project: &ProjectId,
    gate: Option<&GateId>,
) -> Result<()> {
    let root = root_of(store, project)?;
    if store.imported_hash(project)?.is_some() {
        ensure_import_current(store, project)?;
    } else {
        let m = read(&root)?;
        // ⚠ A manifest for another project would otherwise pass vacuously
        // for a project with no gates.
        if m.body.project != *project {
            bail!(
                "{} is the manifest of project {}, not of {project}. Run `fl manifest export`, \
                 then commit.",
                root.join(MANIFEST_PATH).display(),
                m.body.project
            );
        }
        let gates = match gate {
            Some(g) => vec![
                store
                    .get_gate(g)?
                    .with_context(|| format!("{g} is held by this store, but it is not a gate"))?,
            ],
            None => store.list_gates(project)?,
        };
        for g in &gates {
            match m.currency_of(g) {
                Currency::Current => {}
                Currency::Differs => bail!(
                    "gate `{}` changed since the manifest was exported. Run \
                     `fl manifest export`, then commit.",
                    g.name
                ),
                Currency::Absent => bail!(
                    "gate `{}` is not in the manifest. Run `fl manifest export`, then commit.",
                    g.name
                ),
            }
        }
        // The whole project, not only its gates: a transition added or
        // changed since export would otherwise never reach another machine.
        if gate.is_none() {
            let now = store.export_manifest(project, "", 0)?;
            if now.body.gates.len() != m.body.gates.len() {
                bail!(
                    "the manifest lists a gate this store no longer holds. Run \
                     `fl manifest export`, then commit."
                );
            }
            if now.body.transitions != m.body.transitions {
                bail!(
                    "the project's transitions changed since the manifest was exported. Run \
                     `fl manifest export`, then commit."
                );
            }
        }
    }
    if !Git::is_committed(&root, MANIFEST_PATH).map_err(|e| anyhow::anyhow!("{e}"))? {
        bail!(
            "{} is not committed. Another machine can only resolve a gate through a \
             committed manifest: commit it. (fl cannot tell whether the commit was pushed.)",
            root.join(MANIFEST_PATH).display()
        );
    }
    Ok(())
}

pub fn run(store: &RedbStore, cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Export { project } => {
            let p = ProjectId(refs::resolve(store, store.label(), Kind::Project, &project)?);
            let root = root_of(store, &p)?;
            let head = Git::head(&root).map_err(|e| anyhow::anyhow!("{e}"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .context("the system clock is before 1970")?
                .as_secs();
            let m = store.export_manifest(&p, &head, now)?;
            // ⚠ Every gate is printed with what it runs: a gate command can
            // name a local path, and this file is about to be committed.
            for g in &m.body.gates {
                let runs = match &g.kind {
                    GateKind::Command(c) => format!("{} {}", c.program, c.args.join(" ")),
                    GateKind::Agent(a) => format!("agent {}", a.adapter),
                };
                println!(
                    "gate\t{}\t{}\t{}",
                    refs::show(store, Kind::Gate, g.id.iri())?,
                    g.name,
                    runs.trim_end()
                );
            }
            let path = root.join(MANIFEST_PATH);
            std::fs::create_dir_all(path.parent().expect("MANIFEST_PATH has a parent"))
                .with_context(|| format!("could not create {}", path.display()))?;
            std::fs::write(&path, m.to_json())
                .with_context(|| format!("could not write {}", path.display()))?;
            println!("wrote\t{}\tsha256:{}", path.display(), m.content_sha256);
            println!("commit it: another machine resolves these gates only through a committed manifest");
        }
        Cmd::Import { root } => {
            let root = root
                .canonicalize()
                .with_context(|| format!("`{}` could not be resolved", root.display()))?;
            Git::head(&root).map_err(|e| {
                anyhow::anyhow!("`{}` is not a git working tree: {e}", root.display())
            })?;
            let m = read(&root)?;
            let report = store.import_manifest(&m, &root.display().to_string())?;
            if let Some((old, new)) = &report.root_moved {
                println!("moved\tthe project's gates now run over {new}, not {old}");
            }
            println!(
                "imported\t{}\tgates: {} added, {} changed, {} unchanged\ttransitions: {}",
                refs::show(store, Kind::Project, report.project.iri())?,
                report.gates_added,
                report.gates_changed,
                report.gates_unchanged,
                report.transitions
            );
        }
        Cmd::Check { project } => {
            let p = ProjectId(refs::resolve(store, store.label(), Kind::Project, &project)?);
            ensure_publishable(store, &p, None)?;
            println!("current\t{}", root_of(store, &p)?.join(MANIFEST_PATH).display());
        }
    }
    Ok(0)
}
```

Add `pub mod manifest;` to `crates/cli/src/cmd/mod.rs`.

In `crates/cli/src/main.rs`:
- add the variant `#[command(subcommand)] Manifest(cmd::manifest::Cmd),` to `Command`;
- add `Command::Manifest(c) => c.iris(),` to `iris`, `Command::Manifest(c) => c.has_handle(),` to `has_handle`;
- in `project_root`, add `Command::Manifest(c) => c.root(),` before `_ => None`;
- in the dispatch `match`, add `Command::Manifest(c) => cmd::manifest::run(&store, c),`.

- [ ] **Step 4: The currency guard on every command that runs a gate**

Add one call, `crate::cmd::manifest::ensure_import_current(store, &<project>)?;`, immediately before the gate runs:

| file | where | `<project>` |
|---|---|---|
| `cmd/gate.rs` | `Cmd::Run`, after `let g = gate(store, &id)?;` | `g.project` |
| `cmd/check.rs` | `run`, before `evaluate_transition` | `project` |
| `cmd/record.rs` | `Cmd::Move`, after the record is read, before `move_record` — only when gated (below) | `record.project` |
| `cmd/finding.rs` | `Cmd::Reproduce`, after `finding_id` | `finding(store, &finding)?.project` |
| `cmd/finding.rs` | `Cmd::Verify`, after `finding_id` | `finding(store, &finding)?.project` |

For the two `finding.rs` sites, read the finding once into a local and pass `&f.project`. Both arms bind a field named `finding`, which shadows the module's `fn finding`, so call it by path: `let f = self::finding(store, &finding)?;`. A finding that does not exist now gets that function's "is not a finding in the store" refusal before `attach_reproduction`/`verify_finding` run; `explain`'s `NoSuchFinding` arm stays as a guard for a finding that disappears in between.

For `record move`, guard only a move that a transition covers — an ungated move runs no gate, so a stale manifest must not refuse it (spec §4.5 is about running gates):

```rust
            let gated = store
                .list_transitions(&record.project)?
                .iter()
                .any(|t| t.from == record.state && t.to == state);
            if gated {
                crate::cmd::manifest::ensure_import_current(store, &record.project)?;
            }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p fl-cli --test manifest`
Expected: PASS, 12 tests.

Then `cargo test --workspace`. ⚠ `getting_started` FAILS here, and that is expected: the guide's first command is `fl --help`, whose transcript lists every subcommand, and `manifest` is new. Give the variant a doc comment in `main.rs` — `/// Share a project's gates through a committed manifest.` — and add its line to the `fl --help` transcript in `docs/getting-started.md`, after `stats` and before `help`, padded like its neighbours: `  manifest    Share a project's gates through a committed manifest`. Re-run; PASS, with `VERIFIED_COMMANDS` still 65.

- [ ] **Step 6: Mutation checks**

One at a time, confirm red, then restore:
- In `ensure_import_current`, return `Ok(())` early → `a_manifest_that_moved_on…` and `a_missing_manifest…` FAIL.
- Remove the `is_committed` check → `an_uncommitted_manifest_is_refused_by_check` FAILS.
- Remove the guard call in `cmd/gate.rs` → `a_manifest_edited_after_import…` FAILS.
- Remove the guard call in `cmd/check.rs` → `a_manifest_that_moved_on…` FAILS.
- Remove the guard call in `cmd/record.rs` → `a_gated_move_is_refused…` FAILS; make it unconditional → the same test FAILS on the ungated move.
- Remove either guard call in `cmd/finding.rs` → `reproduce_and_verify_are_refused…` FAILS.
- Remove the transitions comparison in `ensure_publishable` → `check_refuses_a_gate_or_transition…` FAILS.
- Remove the `m.body.project != *project` check → no CLI test catches it; confirm with a unit probe or accept, and say which in the commit message.

- [ ] **Step 7: Document the commands**

Do NOT add this to `docs/getting-started.md`. That guide promises that every command in it was run to produce the output shown, and its test (`crates/cli/tests/getting_started.rs`) runs every block whose first line starts with `$ `; it recognises only bare fences, so a ```` ```text ```` block would open a phantom block rather than be skipped.

Create `docs/sharing-gates.md` instead: prose, with commands in inline code, and no `$ ` transcripts. Cover: why a manifest exists (another machine resolves the gate an issue names); `fl manifest export`, then commit; `fl manifest import` on the other machine; `fl manifest check`; what is refused and the remedy for each (edited by hand; changed since import; not committed; a gate changed since export; an imported gate edited locally); that pass marks stay on each machine; that a store which imports becomes format 3; and that `crates/cli/tests/manifest.rs` is the executed version of this page. Add one sentence at the end of `docs/getting-started.md`'s last section linking to it, in prose (no `$ ` line). Re-run `cargo test -p fl-cli --test getting_started`.

- [ ] **Step 8: Trio and commit**

```bash
git add crates/exec/src/git.rs crates/cli/src/cmd/manifest.rs crates/cli/src/cmd/mod.rs \
        crates/cli/src/main.rs crates/cli/src/cmd/gate.rs crates/cli/src/cmd/check.rs \
        crates/cli/src/cmd/record.rs crates/cli/src/cmd/finding.rs crates/cli/tests/manifest.rs \
        docs/getting-started.md docs/sharing-gates.md
git commit -m "feat(cli): fl manifest export, import and check; imported gates run only when current

Export prints every gate with what it runs before writing the file.
Import writes into the store bound to the root. Every command that runs
a gate refuses when the working tree's manifest is missing, hand-edited,
or changed since import. check refuses an uncommitted or stale manifest.
Guards mutation-tested.

Co-authored-by: Ferris <Ferris@artificialhumanity.io>"
```

---

## Finish

- [ ] Run the trio once more on the branch head.
- [ ] Dispatch the whole-branch review (`superpowers:requesting-code-review`), address findings with `superpowers:receiving-code-review`, push, and open the pull request. The owner merges. Plan B starts from the merged `main`.
