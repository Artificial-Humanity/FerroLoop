# Stable Identity and Store Roles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `u64` counter ids with IRIs and split persistence into three roles (catalog, tracker, ledger) that a store backs, while the CLI keeps speaking short handles.

**Architecture:** A validated `Iri` type lives in `fl-core`. The single `Store` trait becomes three role traits with `&self` methods, bound together by a `Roles` value, so the engine names the roles it touches. Local stores mint `urn:uuid:` (UUIDv7), own an id only if they hold it, and keep a per-store, per-kind handle table that the CLI reads and writes through. A user-level config binds a project to a store, and an IRI on the command line selects the store that owns it.

**Tech Stack:** Rust 2024 (MSRV 1.98), redb 4, serde/serde_json, clap 4. New: `uuid` (feature `v7`) in `fl-store`, `toml` in `fl-cli`.

**Spec:** `docs/superpowers/specs/2026-09-23-identity-and-store-roles-design.md`

## Global Constraints

- Edition 2024, `rust-version = "1.98"`, Apache-2.0. No new dependency except `uuid` (features `["v7"]`, `fl-store` only) and `toml` (`fl-cli` only).
- `fl-core` stays pure: no IO, no clock, no randomness, no new dependency. It never mints a UUIDv7.
- Every task ends green on the trio: `cargo test --workspace`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo fmt --all --check`. Commit only when all three pass.
- Every commit ends with `Co-authored-by: Ferris <Ferris@artificialhumanity.io>`.
- No existing test is deleted or weakened without the reason in the commit message.
- Every guard test is verified by mutation: revert the guarded line, watch the test go red, restore. Each task names the reverts.
- snake_case on the wire (decision 33). A new enum that crosses the process boundary uses `wire_names!`.
- *(Invariant)* An id is normalized once, on mint or on entry. No comparison runs on an id that was not normalized.
- *(Invariant)* Parse and resolve are different steps. A store asked for an id it does not hold answers `NotOwned`, never "not found".
- *(Invariant)* Every failure path distinguishes "nothing" from "didn't look". A method that takes an id checks ownership before it answers, including list methods: a list over a project the store does not hold is `NotOwned`, never an empty list.
- *(Invariant)* Evidence before state: the ledger entry is written before the tracker state changes.
- *(Invariant)* Every stored reference is a full IRI. A handle never enters a stored item, the wire, or JSON output.
- *(Invariant)* An insert never overwrites. A minted id that already exists is refused.
- Handles are sequential **per store and per kind** (owner, 2026-09-24; spec §4 amended in the plan's PR).
- Human output names an item by its handle, or by its name or root when it has no handle. It prints an IRI only when nothing else identifies the item.
- A store that backs all three roles (`MemStore`, `RedbStore`) checks every reference it holds. A reference to an id in another store is checked at the engine layer (spec §3.4); no store in this plan backs fewer than three roles.

## Rulings made while planning

- **Deletion.** No delete operation exists. The ownership index is append-only; a test holds that no operation removes an entry. A future delete must leave a tombstone. The tombstone case of spec §6.4a waits for a delete to exist.
- **Minting in `fl-core`.** `MemStore` mints deterministic UUID-shaped ids from a counter (`seq_iri`). Two `MemStore`s mint the same ids; `MemStore` is a test store. Only `RedbStore` mints real UUIDv7.
- **`&self` role methods.** Role methods take `&self`, so one store can be passed as two roles at once (`Roles::single`). `MemStore` uses a `RefCell`. `RedbStore` already only needs `&self`.
- **Ledger rows.** Gate runs and attempts keep internal `u64` row keys. They are not ids, never leave the store, and get no IRI.
- **`Dangling`.** Produced when a stored reference is followed and the owning store holds no item of that kind (today: a reference of the wrong kind). A reference to an id no store holds stays `NotOwned`, with the reference named in the message — "gone" and "never looked" stay distinct.
- **Store selection.** A handle resolves only in the bound store. A full IRI on the command line selects the store that owns it, searched across the bound store and every store in the config. Two owners are refused; none is `NotOwned` listing every store searched.

## Review Focus

1. An IRI typed in uppercase (`URN:UUID:0190…ABC`) names the same item as its lowercase form. Pinned in Task 4.
2. The store file is already open in another process or handle → exit 2 with `Unreachable` naming the path; never a panic or a raw redb error. Pinned in Task 3.
3. The config file exists but is malformed, or names a relative store path → exit 2 naming the file; never a silent fall-through to the default store. Pinned in Task 6.
4. The working directory is a subdirectory of a bound project, or reaches it through a symlink → that project's store is used. Pinned in Task 6.
5. Handle input at the edges — `0`, a number past every handle, `3abc`, `99999999999999999999` → a refusal naming the input and the store; never a silent empty answer. Pinned in Task 4.

---

## File Structure

| file | responsibility | tasks |
|---|---|---|
| `crates/core/src/iri.rs` (new) | `Iri`, `IriError`, normalization, serde | 1 |
| `crates/core/src/ids.rs` | `Kind`, id newtypes over `Iri`, `seq_iri` | 4 |
| `crates/core/src/store.rs` | `StoreError`, `Catalog`, `Tracker`, `Ledger`, `Handles`, `Roles`, `follow` | 2, 3, 4, 5 |
| `crates/core/src/mem.rs` (new) | `MemStore` (moved out of `store.rs`) | 2, 4, 5 |
| `crates/core/src/conformance.rs` (new) | one suite per role, plus the all-roles suite | 2, 4, 5 |
| `crates/core/src/model.rs`, `finding.rs`, `log.rs` | id fields, `also_known_as` | 4, 5 |
| `crates/store/src/lib.rs` | `RedbStore`: format version, schema v2, ownership, handles, aliases | 2–5 |
| `crates/exec/src/evaluate.rs`, `finding.rs` | take roles, not `Store` | 2, 4, 5 |
| `crates/exec/src/record.rs` (new) | `move_record`: evidence before state | 2 |
| `crates/cli/src/refs.rs` (new) | `Ref` (handle or IRI), `resolve`, `show` | 4 |
| `crates/cli/src/config.rs` (new) | user config, store binding, store selection | 6 |
| `crates/cli/src/main.rs`, `cmd/*.rs` | wiring | 2, 4, 6 |
| `docs/getting-started.md`, `README.md` | handles, config | 4, 6 |

---

### Task 1: The `Iri` type

**Files:**
- Create: `crates/core/src/iri.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Produces: `fl_core::iri::{Iri, IriError}`, re-exported as `fl_core::{Iri, IriError}`. `Iri::parse(&str) -> Result<Iri, IriError>`, `Iri::as_str(&self) -> &str`, `Iri::scheme(&self) -> &str`. `Display`, `FromStr`, `Serialize` (a plain string), `Deserialize` (parses and normalizes, so a malformed stored id is a decode error). `Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug`. Not `Copy`.

- [ ] **Step 1: Write the failing tests** at the bottom of `crates/core/src/iri.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_urn_uuid_is_folded_to_lowercase() {
        let a = Iri::parse("URN:UUID:0190A1B2-C3D4-7E5F-8A6B-7C8D9E0F1A2B").unwrap();
        assert_eq!(a.as_str(), "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b");
    }

    #[test]
    fn normalizing_twice_changes_nothing() {
        for raw in [
            "URN:UUID:0190A1B2-C3D4-7E5F-8A6B-7C8D9E0F1A2B",
            "HTTPS://GitHub.COM/Artificial-Humanity/FerroLoop/issues/41",
            "urn:isbn:0451450523",
        ] {
            let once = Iri::parse(raw).unwrap();
            let twice = Iri::parse(once.as_str()).unwrap();
            assert_eq!(once, twice, "{raw}");
        }
    }

    #[test]
    fn an_https_iri_folds_scheme_and_host_but_not_path() {
        let a = Iri::parse("HTTPS://GitHub.COM/Artificial-Humanity/FerroLoop/issues/41").unwrap();
        assert_eq!(
            a.as_str(),
            "https://github.com/Artificial-Humanity/FerroLoop/issues/41"
        );
    }

    #[test]
    fn another_urn_namespace_keeps_its_nss_case() {
        let a = Iri::parse("URN:Example:MixedCase").unwrap();
        assert_eq!(a.as_str(), "urn:example:MixedCase");
    }

    #[test]
    fn what_is_not_an_absolute_iri_is_refused_by_name() {
        for bad in ["", "3", "3abc", "no-colon", ":rest", "1http:x", "urn:", "urn:uuid:a b"] {
            let err = Iri::parse(bad).expect_err(bad);
            if !bad.is_empty() {
                assert!(err.to_string().contains(bad), "{bad}: {err}");
            }
        }
    }

    #[test]
    fn the_wire_form_is_a_plain_string_and_is_normalized_on_the_way_in() {
        let a: Iri = serde_json::from_str("\"URN:UUID:0190A1B2-C3D4-7E5F-8A6B-7C8D9E0F1A2B\"").unwrap();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            "\"urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b\""
        );
        assert!(serde_json::from_str::<Iri>("\"not an iri\"").is_err());
        assert!(serde_json::from_str::<Iri>("3").is_err());
    }
}
```

- [ ] **Step 2: Run to confirm they fail.** Run: `cargo test -p fl-core iri` — Expected: compile error, `Iri` not defined.

- [ ] **Step 3: Implement** above the tests in `crates/core/src/iri.rs`:

```rust
//! The one id type. Every id FerroLoop stores or prints is an absolute IRI.
//!
//! ⚠ Normalization happens here, once, on the way in — `parse` and the serde
//! `Deserialize` both normalize. After that, ids compare as exact strings, so
//! a comparison can never run on an id that was not normalized (spec §2.3).
//!
//! This is deliberately not a full RFC 3987 parser. It accepts what an id
//! needs to be — `scheme ":" rest`, no whitespace — and refuses the rest by
//! name. Resolving an id to a store is a separate step (spec §2.4).

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Iri(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IriError {
    #[error("an id cannot be empty")]
    Empty,
    #[error("`{0}` contains whitespace, and an id cannot")]
    Whitespace(String),
    #[error("`{0}` is not an absolute IRI: it needs a scheme, a colon, and something after it (such as `urn:uuid:…`)")]
    NotAbsolute(String),
}

impl Iri {
    pub fn parse(input: &str) -> Result<Self, IriError> {
        if input.is_empty() {
            return Err(IriError::Empty);
        }
        if input.chars().any(char::is_whitespace) {
            return Err(IriError::Whitespace(input.to_string()));
        }
        let not_absolute = || IriError::NotAbsolute(input.to_string());
        let (scheme, rest) = input.split_once(':').ok_or_else(not_absolute)?;
        let mut chars = scheme.chars();
        let scheme_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if !scheme_ok || rest.is_empty() {
            return Err(not_absolute());
        }
        if scheme.eq_ignore_ascii_case("urn") {
            // `urn:<nid>:<nss>` needs both parts.
            match rest.split_once(':') {
                Some((nid, nss)) if !nid.is_empty() && !nss.is_empty() => {}
                _ => return Err(not_absolute()),
            }
        }
        Ok(Self(normalize(scheme, rest)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn scheme(&self) -> &str {
        self.0.split_once(':').map(|(s, _)| s).unwrap_or("")
    }
}

/// The scheme is case-insensitive everywhere. For `urn:` the namespace id is
/// too (RFC 8141), and for `urn:uuid:` so is the hex. For `http(s)` the host
/// is. Nothing else is touched: a path is case-sensitive.
fn normalize(scheme: &str, rest: &str) -> String {
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "urn" => {
            let (nid, nss) = rest.split_once(':').expect("checked in parse");
            let nid = nid.to_ascii_lowercase();
            let nss = if nid == "uuid" {
                nss.to_ascii_lowercase()
            } else {
                nss.to_string()
            };
            format!("urn:{nid}:{nss}")
        }
        "http" | "https" => match rest.strip_prefix("//") {
            Some(after) => {
                let end = after.find(['/', '?', '#']).unwrap_or(after.len());
                let (authority, tail) = after.split_at(end);
                format!("{scheme}://{}{tail}", authority.to_ascii_lowercase())
            }
            None => format!("{scheme}:{rest}"),
        },
        _ => format!("{scheme}:{rest}"),
    }
}

impl fmt::Display for Iri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for Iri {
    type Err = IriError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Iri {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Iri {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Iri::parse(&raw).map_err(serde::de::Error::custom)
    }
}
```

In `crates/core/src/lib.rs` add `pub mod iri;` and `pub use iri::{Iri, IriError};`.

- [ ] **Step 4: Run.** `cargo test -p fl-core iri` — Expected: 6 passed.
- [ ] **Step 5: Mutation checks.** (a) Replace `nss.to_ascii_lowercase()` with `nss.to_string()` → `a_urn_uuid_is_folded_to_lowercase` goes red. (b) Make `Deserialize` build `Iri(raw)` without parsing → the wire test goes red. Restore both.
- [ ] **Step 6: Trio, then commit.**

```bash
git add crates/core/src/iri.rs crates/core/src/lib.rs
git commit -m "feat(core): add the Iri id type with normalization on entry"
```

---

### Task 2: Split `Store` into roles

Ids stay `u64` in this task; only the trait shape changes. Every existing test must still pass with its assertions intact.

**Files:**
- Modify: `crates/core/src/store.rs` (traits, `Roles`; `MemStore` moves out)
- Create: `crates/core/src/mem.rs`, `crates/core/src/conformance.rs`, `crates/exec/src/record.rs`
- Modify: `crates/core/src/lib.rs`, `crates/core/Cargo.toml`, `crates/store/Cargo.toml`, `crates/store/src/lib.rs`, `crates/exec/src/{lib.rs,evaluate.rs,finding.rs}`, `crates/cli/src/main.rs`, `crates/cli/src/cmd/*.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces (later tasks rely on these exact names):
  - `pub trait Catalog`, `pub trait Tracker`, `pub trait Ledger` in `fl_core::store`, methods listed in Step 3, all `&self`, id arguments by reference.
  - `pub struct Roles<'a> { pub catalog: &'a dyn Catalog, pub tracker: &'a dyn Tracker, pub ledger: &'a dyn Ledger }` with `Roles::single<S: Catalog + Tracker + Ledger>(store: &'a S) -> Roles<'a>`; `Roles` is `Clone + Copy`.
  - `fl_core::mem::MemStore`, re-exported as `fl_core::MemStore`.
  - `fl_core::conformance::{catalog, tracker, ledger, all_roles}`: each `pub fn <name><S, G>(make: impl Fn() -> (S, G))`, behind `#[cfg(any(test, feature = "conformance"))]`.
  - `fl_exec::evaluate::{run_single_gate(catalog: &dyn Catalog, ledger: &dyn Ledger, project: &ProjectId, gate: &GateId), evaluate_transition(catalog: &dyn Catalog, ledger: &dyn Ledger, project: &ProjectId, name: &str, record: Option<&RecordId>)}`.
  - `fl_exec::finding::{attach_reproduction(roles: Roles<'_>, finding: &FindingId, gate: &GateId), verify_finding(roles: Roles<'_>, finding: &FindingId)}`.
  - `fl_exec::record::{move_record(roles: Roles<'_>, record: &Record, to: State) -> Result<MoveReport, ExecError>, MoveReport { pub transitions: Vec<TransitionReport>, pub outcome: MoveOutcome }, MoveOutcome { Ungated, Moved, Refused { code: i32 } }}`.

- [ ] **Step 1: Write the evidence-before-state test first.** Create `crates/exec/src/record.rs` with only the test module and an empty `move_record` stub returning `unimplemented!()`, and add `pub mod record;` to `crates/exec/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::log::{Attempt, GateRun};
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Regret, Selector, Transition};
    use fl_core::store::{Catalog, Ledger, StoreError, Tracker};
    use fl_core::{GateId, MemStore, ProjectId};
    use std::process::Command;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git").args(args).current_dir(d.path()).status().unwrap().success());
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(d.path().join("a.rs"), "fn a() {}").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first"]);
        d
    }

    /// A ledger that refuses every append: the evidence cannot be written.
    struct DownLedger;
    impl Ledger for DownLedger {
        fn append_gate_run(&self, _: GateRun) -> Result<(), StoreError> {
            Err(StoreError::Backend("ledger down".into()))
        }
        fn append_attempt(&self, _: Attempt) -> Result<(), StoreError> {
            Err(StoreError::Backend("ledger down".into()))
        }
        fn gate_runs(&self, _: &GateId) -> Result<Vec<GateRun>, StoreError> {
            Ok(vec![])
        }
        fn attempts(&self, _: &ProjectId) -> Result<Vec<Attempt>, StoreError> {
            Ok(vec![])
        }
    }

    fn gated_record(store: &MemStore, root: &std::path::Path) -> Record {
        let head = crate::git::Git::head(root).unwrap();
        let p = store.add_project(&root.display().to_string()).unwrap();
        let g = store
            .add_gate(
                &p,
                "passes",
                GateKind::Command(CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    delivery: PopulationDelivery::Args,
                    timeout_secs: 10,
                    pass_codes: vec![0],
                }),
                Selector::Glob { pattern: "*.rs".into() },
                1,
                &head,
                "tester",
            )
            .unwrap();
        store
            .add_transition(Transition {
                project: p.clone(),
                name: "launch".into(),
                from: State::Todo,
                to: State::Done,
                regret: Regret::Low,
                gates: vec![g],
            })
            .unwrap();
        let r = store.add_record(&p, "t").unwrap();
        store.get_record(&r).unwrap().unwrap()
    }

    // ⚠⚠ Spec §3.5 (Invariant): evidence before state. If the gate run
    // cannot be recorded, the record must not move — otherwise the state
    // says "verified" with no evidence behind it.
    #[test]
    fn a_ledger_that_cannot_record_the_evidence_leaves_the_state_unchanged() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());
        let roles = Roles { catalog: &store, tracker: &store, ledger: &DownLedger };

        let err = move_record(roles, &record, State::Done);

        assert!(err.is_err(), "a move whose evidence was not written must not succeed");
        assert_eq!(store.get_record(&record.id).unwrap().unwrap().state, State::Todo);
    }

    #[test]
    fn a_passing_move_writes_the_evidence_and_then_the_state() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());

        let report = move_record(Roles::single(&store), &record, State::Done).unwrap();

        assert!(matches!(report.outcome, MoveOutcome::Moved));
        assert_eq!(store.get_record(&record.id).unwrap().unwrap().state, State::Done);
        let gate = &store.list_gates(&record.project).unwrap()[0].id;
        assert_eq!(store.gate_runs(gate).unwrap().len(), 1);
    }

    #[test]
    fn an_undeclared_move_is_ungated_and_says_so() {
        let d = repo();
        let store = MemStore::default();
        let record = gated_record(&store, d.path());

        let report = move_record(Roles::single(&store), &record, State::Doing).unwrap();

        assert!(matches!(report.outcome, MoveOutcome::Ungated));
        assert!(report.transitions.is_empty());
    }
}
```

- [ ] **Step 2: Run to confirm failure.** `cargo test -p fl-exec record` — Expected: compile errors (`Roles`, `Catalog` not defined).

- [ ] **Step 3: Define the role traits** in `crates/core/src/store.rs`, replacing `pub trait Store { … }`. Keep `StoreError` exactly as it is. Keep the doc comment about populations on `Catalog`:

```rust
/// Definitions: projects, gates, transitions. Rarely changed; each belongs
/// to a repository.
///
/// ⚠ There is no method that stores a population, and adding one would break
/// the design. A population is enumerated fresh from the working tree at run
/// time so it cannot go stale in the store.
pub trait Catalog {
    fn add_project(&self, root: &str) -> Result<ProjectId, StoreError>;
    fn get_project(&self, id: &ProjectId) -> Result<Option<Project>, StoreError>;
    fn list_projects(&self) -> Result<Vec<Project>, StoreError>;

    #[allow(clippy::too_many_arguments)]
    fn add_gate(
        &self,
        project: &ProjectId,
        name: &str,
        kind: GateKind,
        selector: Selector,
        min_population: u64,
        authored_at_commit: &str,
        authored_by: &str,
    ) -> Result<GateId, StoreError>;
    fn get_gate(&self, id: &GateId) -> Result<Option<GateDef>, StoreError>;
    fn list_gates(&self, project: &ProjectId) -> Result<Vec<GateDef>, StoreError>;
    fn update_gate(&self, def: &GateDef) -> Result<(), StoreError>;

    /// Stores a transition, keyed by `(project, name)`. Overwrites any existing transition with the same key (upsert semantics).
    fn add_transition(&self, t: Transition) -> Result<(), StoreError>;
    fn get_transition(&self, project: &ProjectId, name: &str)
        -> Result<Option<Transition>, StoreError>;
    /// Every transition a project declares.
    ///
    /// Needed because a transition is addressed by NAME, but a record move is
    /// addressed by the (from, to) pair it performs — so the move has to ask
    /// which declarations cover it.
    fn list_transitions(&self, project: &ProjectId) -> Result<Vec<Transition>, StoreError>;
}

/// Mutable state that people discuss: records and findings.
pub trait Tracker {
    fn add_record(&self, project: &ProjectId, title: &str) -> Result<RecordId, StoreError>;
    fn get_record(&self, id: &RecordId) -> Result<Option<Record>, StoreError>;
    fn list_records(&self, project: &ProjectId) -> Result<Vec<Record>, StoreError>;
    fn set_record_state(&self, id: &RecordId, state: State) -> Result<(), StoreError>;

    fn add_finding(&self, finding: Finding) -> Result<FindingId, StoreError>;
    fn get_finding(&self, id: &FindingId) -> Result<Option<Finding>, StoreError>;
    fn update_finding(&self, finding: &Finding) -> Result<(), StoreError>;
    fn list_findings(&self, project: &ProjectId) -> Result<Vec<Finding>, StoreError>;

    /// How many findings this actor raised and then withdrew.
    ///
    /// ⚠ Decision 27 puts a cost on a claim the reviewer cannot support. A
    /// cost nobody can read is not a cost, so this is part of the trait and
    /// not a report bolted on later.
    fn withdrawals_by(&self, actor: &str) -> Result<u64, StoreError>;
}

/// Append-only evidence: gate runs and attempts.
pub trait Ledger {
    fn append_gate_run(&self, run: GateRun) -> Result<(), StoreError>;
    fn append_attempt(&self, attempt: Attempt) -> Result<(), StoreError>;
    fn gate_runs(&self, gate: &GateId) -> Result<Vec<GateRun>, StoreError>;
    fn attempts(&self, project: &ProjectId) -> Result<Vec<Attempt>, StoreError>;
}

/// Binds each role to the store that backs it (spec §3.3). A store is the
/// backing entity; a role is what it backs. One store can back all three.
#[derive(Clone, Copy)]
pub struct Roles<'a> {
    pub catalog: &'a dyn Catalog,
    pub tracker: &'a dyn Tracker,
    pub ledger: &'a dyn Ledger,
}

impl<'a> Roles<'a> {
    /// One store backing all three roles.
    pub fn single<S: Catalog + Tracker + Ledger>(store: &'a S) -> Self {
        Self { catalog: store, tracker: store, ledger: store }
    }
}
```

- [ ] **Step 4: Move `MemStore` to `crates/core/src/mem.rs`.** Wrap its maps in one `RefCell`:

```rust
use crate::finding::{Finding, FindingState};
use crate::ids::{FindingId, GateId, ProjectId, RecordId};
use crate::log::{Attempt, GateRun};
use crate::model::{GateDef, GateKind, Project, Record, Selector, State, Transition};
use crate::store::{Catalog, Ledger, StoreError, Tracker};
use std::cell::RefCell;
use std::collections::BTreeMap;

/// An in-memory store for tests. Deliberately lives in `fl-core` so the
/// engine's own tests need no backend at all. Backs all three roles.
#[derive(Default)]
pub struct MemStore {
    inner: RefCell<Inner>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    projects: BTreeMap<u64, Project>,
    gates: BTreeMap<u64, GateDef>,
    transitions: BTreeMap<(u64, String), Transition>,
    records: BTreeMap<u64, Record>,
    runs: Vec<GateRun>,
    attempts: Vec<Attempt>,
    findings: BTreeMap<u64, Finding>,
}

impl Inner {
    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}
```

Then split the existing `impl Store for MemStore` body into `impl Catalog for MemStore`, `impl Tracker for MemStore`, `impl Ledger for MemStore`, each method in the trait its name is listed under in Step 3. Mechanical rule for every method body: replace `&mut self` / `&self` with `&self`; begin the body with `let mut s = self.inner.borrow_mut();` (writes) or `let s = self.inner.borrow();` (reads); replace `self.` with `s.`; replace an `id` parameter now taken by reference with `*id` where a value is needed (`GateId` is still `Copy`). Example:

```rust
fn get_gate(&self, id: &GateId) -> Result<Option<GateDef>, StoreError> {
    Ok(self.inner.borrow().gates.get(&id.0).cloned())
}
```

In `lib.rs`: `pub mod mem;`, `pub use mem::MemStore;`, and replace `pub use store::{MemStore, Store, StoreError};` with `pub use store::{Catalog, Ledger, Roles, StoreError, Tracker};`.

- [ ] **Step 5: Build the conformance module.** In `crates/core/Cargo.toml` add:

```toml
[features]
conformance = []
```

In `lib.rs`: `#[cfg(any(test, feature = "conformance"))] #[doc(hidden)] pub mod conformance;`.

Create `crates/core/src/conformance.rs`. Move **every** test currently in the `#[cfg(test)] mod tests` of `store.rs` into it as a plain function taking `&S` (keep the name, the comments, and every assertion), and group them by role. The suites own their helpers (`sample_kind`, `sample_selector`, `sample_run`):

```rust
//! The contract every store must meet, one suite per role (spec §6.1).
//!
//! ⚠ A store gets no suite of its own and no privileged path. `MemStore`,
//! `RedbStore` and, later, a GitHub store all run these same functions. A
//! case that only one store can pass is a defect in that store, not a case to
//! move out of here.
//!
//! `make` returns the store plus a guard to keep alive (a temp directory for
//! a file store, `()` for memory).

use crate::finding::{Finding, FindingState};
use crate::log::GateRun;
use crate::model::{CommandSpec, GateKind, PopulationDelivery, Selector, State};
use crate::store::{Catalog, Ledger, Tracker};
use crate::verdict::Verdict;

pub fn catalog<S: Catalog, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        a_project_round_trips::<S>,
        affirming_a_gate_moves_only_its_stamp::<S>,
        list_transitions_returns_every_transition_of_one_project_and_no_others::<S>,
    ];
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}

pub fn tracker<S: Catalog + Tracker, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[
        list_records_returns_only_the_named_projects_records::<S>,
        a_record_state_change_is_visible_on_the_next_read::<S>,
        a_finding_round_trips_and_gets_a_real_id::<S>,
        withdrawals_are_counted_against_whoever_raised_the_finding::<S>,
        findings_are_listed_per_project::<S>,
    ];
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}

pub fn ledger<S: Catalog + Ledger, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[the_logs_are_append_only_and_read_back_in_order::<S>];
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}

/// Cases that need one store backing all three roles. Empty until Task 4.
pub fn all_roles<S: Catalog + Tracker + Ledger, G>(make: impl Fn() -> (S, G)) {
    let cases: &[fn(&S)] = &[];
    for case in cases {
        let (s, _guard) = make();
        case(&s);
    }
}
```

Each case is a private generic function, for example:

```rust
fn a_project_round_trips<S: Catalog>(s: &S) {
    let id = s.add_project("/tmp/p").unwrap();
    assert_eq!(s.get_project(&id).unwrap().unwrap().root, "/tmp/p");
    assert_eq!(s.list_projects().unwrap().len(), 1);
}
```

Assignment of the moved cases:
- `catalog`: `a_project_round_trips`, `affirming_a_gate_moves_only_its_stamp`, and `list_transitions_returns_every_transition_of_one_project_and_no_others` (moved from `crates/store/src/lib.rs` tests; it registers projects through `add_project` instead of `ProjectId(1)`/`ProjectId(2)` literals, and its "project 3" case uses a third added project with no transitions).
- `tracker`: `list_records_returns_only_the_named_projects_records`, `a_record_state_change_is_visible_on_the_next_read`, `a_finding_round_trips_and_gets_a_real_id`, `withdrawals_are_counted_against_whoever_raised_the_finding` (its `GateId(1)` becomes a gate added through `add_gate`), `findings_are_listed_per_project`.
- `ledger`: `the_logs_are_append_only_and_read_back_in_order`.
- `all_roles`: empty for now; Task 4 fills it.

`ids_are_handed_out_in_sequence_and_never_reused` and `a_missing_project_is_none_and_not_an_error` stay in `mem.rs`'s own test module for this task (Task 4 replaces both, with the reason).

In `mem.rs` tests:

```rust
#[test]
fn mem_store_meets_every_role_contract() {
    crate::conformance::catalog(|| (MemStore::default(), ()));
    crate::conformance::tracker(|| (MemStore::default(), ()));
    crate::conformance::ledger(|| (MemStore::default(), ()));
    crate::conformance::all_roles(|| (MemStore::default(), ()));
}
```

In `crates/store/Cargo.toml` `[dev-dependencies]` add `fl-core = { path = "../core", features = ["conformance"] }`, and in the redb tests:

```rust
fn fresh() -> (RedbStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let s = RedbStore::open(&dir.path().join("t.redb")).unwrap();
    (s, dir)
}

#[test]
fn redb_store_meets_every_role_contract() {
    fl_core::conformance::catalog(fresh);
    fl_core::conformance::tracker(fresh);
    fl_core::conformance::ledger(fresh);
    fl_core::conformance::all_roles(fresh);
}
```

- [ ] **Step 6: Port `RedbStore`.** Split `impl Store for RedbStore` into the three role impls. Every method takes `&self`; id parameters by reference, used as `id.0`. The helper methods already take `&self` — no body changes beyond that.

- [ ] **Step 7: Port the engine.** In `crates/exec/src/evaluate.rs`:
  - `run_gate(catalog: &dyn Catalog, ledger: &dyn Ledger, root, head, def, regret, record: Option<&RecordId>)`: `store.append_gate_run(..)` → `ledger.append_gate_run(..)` with `record: record.copied()`; `store.update_gate(..)` → `catalog.update_gate(..)`.
  - `run_single_gate(catalog, ledger, project: &ProjectId, gate: &GateId)` and `evaluate_transition(catalog, ledger, project: &ProjectId, transition_name, record: Option<&RecordId>)`: reads go to `catalog`, the call to `run_gate` passes both.
  - Test module: `BrokenStore` becomes three impls (`Catalog`, `Tracker`, `Ledger`) with the same `Err(broken())` bodies; call sites become `run_single_gate(&store, &store, &ProjectId(1), &GateId(1))`. The `setup` helper takes `&MemStore`. No assertion changes.

  In `crates/exec/src/finding.rs`: `attach_reproduction(roles: Roles<'_>, finding: &FindingId, gate: &GateId)` and `verify_finding(roles: Roles<'_>, finding: &FindingId)`. Findings go through `roles.tracker`, gates and projects through `roles.catalog`, and `run_single_gate(roles.catalog, roles.ledger, &f.project, gate)`. Error variants that take an id take `*finding` / `*gate` (still `Copy`).

- [ ] **Step 8: Implement `move_record`** in `crates/exec/src/record.rs`, above the tests. The logic is moved out of `crates/cli/src/cmd/record.rs`'s `Move` branch, unchanged:

```rust
use crate::evaluate::{TransitionReport, evaluate_transition};
use crate::population::ExecError;
use fl_core::model::{Record, State, Transition};
use fl_core::store::Roles;

pub struct MoveReport {
    pub transitions: Vec<TransitionReport>,
    pub outcome: MoveOutcome,
}

pub enum MoveOutcome {
    /// No transition covers this move, so nothing was bypassed and nothing
    /// was verified. The caller must say so: "allowed" and "not checked"
    /// must not look alike.
    Ungated,
    Moved,
    /// At least one transition did not pass; the record did not move.
    Refused { code: i32 },
}

fn store_err(e: impl std::fmt::Display) -> ExecError {
    ExecError::Store(e.to_string())
}

/// Move a record, running every transition that covers `(record.state, to)`.
///
/// ⚠⚠ Evidence before state (spec §3.5, Invariant). The gate runs are
/// appended to the ledger inside `evaluate_transition`; only after every one
/// of them is written does the tracker state change. A crash in between
/// leaves evidence and no move, which is safe to retry. The reverse order
/// would leave a move with no evidence.
pub fn move_record(roles: Roles<'_>, record: &Record, to: State) -> Result<MoveReport, ExecError> {
    let declared: Vec<Transition> = roles
        .catalog
        .list_transitions(&record.project)
        .map_err(store_err)?
        .into_iter()
        .filter(|t| t.from == record.state && t.to == to)
        .collect();

    if declared.is_empty() {
        roles.tracker.set_record_state(&record.id, to).map_err(store_err)?;
        return Ok(MoveReport { transitions: vec![], outcome: MoveOutcome::Ungated });
    }

    let mut transitions = Vec::new();
    let mut worst = 0;
    for t in &declared {
        let report = evaluate_transition(
            roles.catalog,
            roles.ledger,
            &record.project,
            &t.name,
            Some(&record.id),
        )?;
        // Same rule `check` applies: a transition that declares no gates
        // verified nothing, so it cannot authorise a move.
        let code = if report.gates.is_empty() { 1 } else { report.exit_code() };
        worst = worst.max(code);
        transitions.push(report);
    }

    if worst != 0 {
        return Ok(MoveReport { transitions, outcome: MoveOutcome::Refused { code: worst } });
    }

    roles.tracker.set_record_state(&record.id, to).map_err(store_err)?;
    Ok(MoveReport { transitions, outcome: MoveOutcome::Moved })
}
```

- [ ] **Step 9: Port the CLI.** `main.rs`: `let store = RedbStore::open(..)`, and each `cmd::X::run(&store, c)`. Each `cmd/*.rs`: `pub fn run(store: &RedbStore, cmd: Cmd)`, imports `Catalog`/`Tracker`/`Ledger` as used, and `Roles::single(store)` where an exec function takes roles. `record.rs`'s `Move` branch keeps its lookups and prints and calls `move_record`, printing from the report exactly what it prints today:

```rust
let report = move_record(Roles::single(store), &record, state)
    .map_err(|e| anyhow::anyhow!("{e}"))?;
match report.outcome {
    MoveOutcome::Ungated => {
        println!(
            "{id}\t{}\tungated: project {} declares no transition from `{}` to `{}`",
            state.as_wire(), record.project, record.state.as_wire(), state.as_wire()
        );
        return Ok(0);
    }
    _ => {}
}
for t in &report.transitions {
    for g in &t.gates {
        let (label, detail) = g.verdict.describe();
        println!(
            "{label}\t{}\t{}\t{detail}\t{}ms{}",
            t.transition,
            g.name,
            g.duration_ms,
            g.staleness.note()
        );
        if !g.verdict.is_pass() && !g.output_excerpt.is_empty() {
            for line in g.output_excerpt.lines().take(20) {
                println!("\t| {line}");
            }
        }
    }
    if t.gates.is_empty() {
        println!(
            "FAIL\t{}\tthe transition declares no gates, so nothing was verified",
            t.transition
        );
    }
}
match report.outcome {
    MoveOutcome::Refused { code } => {
        println!("REFUSED\t{id}\tstays `{}`", record.state.as_wire());
        Ok(code)
    }
    _ => {
        println!("{id}\t{}", state.as_wire());
        Ok(0)
    }
}
```

The existing `⚠` comments in the `Move` branch that explain why the move consults its gates stay, moved to `move_record`'s doc comment where they describe its logic.

- [ ] **Step 10: Run everything.** `cargo test --workspace` — Expected: all pass. No case disappears: every test function moved into `conformance.rs` still runs, inside both runners. The reported count changes because moved cases no longer count one-by-one; list the moved cases in the commit message body.

- [ ] **Step 11: Mutation check.** In `move_record`, move the final `set_record_state` call to before the `for t in &declared` loop → `a_ledger_that_cannot_record_the_evidence_leaves_the_state_unchanged` goes red. Restore.

- [ ] **Step 12: Trio, then commit.**

```bash
git add crates/
git commit -m "refactor: split Store into catalog, tracker and ledger roles"
```

---

### Task 3: Store format version

Still the `u64` schema. This task makes the store say which format it holds, so that Task 4's schema change is refused cleanly on an old file.

**Files:**
- Modify: `crates/core/src/store.rs` (two `StoreError` variants), `crates/store/src/lib.rs`

**Interfaces:**
- Produces: `StoreError::FormatVersion { found: Option<u64>, expected: u64 }`, `StoreError::Unreachable { store: String, cause: String }`; `fl_store::FORMAT_VERSION: u64` (= `1` in this task); `RedbStore::open` refuses a store whose version is not `FORMAT_VERSION`.

- [ ] **Step 1: Write the failing tests** in `crates/store/src/lib.rs` tests:

```rust
/// A store written before format versioning: tables, and no version key.
/// The table definitions are local and frozen: this fixture must keep
/// writing the OLD shape after Task 4 changes the live ones.
fn legacy_store(path: &std::path::Path) {
    const OLD_META: TableDefinition<&str, u64> = TableDefinition::new("meta");
    const OLD_PROJECTS: TableDefinition<u64, &str> = TableDefinition::new("projects");
    let db = redb::Database::create(path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        tx.open_table(OLD_META).unwrap().insert("next_id", 3u64).unwrap();
        tx.open_table(OLD_PROJECTS).unwrap().insert(1u64, "{}").unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn a_store_from_before_format_versioning_is_refused_with_a_remedy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.redb");
    legacy_store(&path);
    let err = RedbStore::open(&path).err().expect("an unversioned store must be refused");
    assert!(
        matches!(err, StoreError::FormatVersion { found: None, expected: FORMAT_VERSION }),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("start a new store"), "no remedy: {msg}");
}

// Separate from the refusal above, so that "refuses everything" cannot pass
// as "refuses the old store".
#[test]
fn a_new_store_is_accepted_and_accepted_again_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.redb");
    RedbStore::open(&path).unwrap();
    RedbStore::open(&path).unwrap();
}

#[test]
fn a_store_already_open_is_unreachable_and_names_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let _held = RedbStore::open(&path).unwrap();
    let err = RedbStore::open(&path).err().expect("a second open of a held store must fail");
    assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    assert!(err.to_string().contains(&path.display().to_string()), "{err}");
}
```

- [ ] **Step 2: Run to confirm failure.** `cargo test -p fl-store` — Expected: compile error, the variants do not exist.

- [ ] **Step 3: Add the variants** to `StoreError`:

```rust
    /// ⚠ The store could not be reached at all. This is "didn't look", and it
    /// must never read as an empty store.
    #[error("the store at {store} could not be opened: {cause}")]
    Unreachable { store: String, cause: String },
    #[error(
        "the store holds format {}, and this version of fl reads format {expected}. \
         There is no migration: start a new store, or keep using the version of fl \
         that wrote this one.",
        match found { Some(v) => v.to_string(), None => "none (written before format versioning)".to_string() }
    )]
    FormatVersion { found: Option<u64>, expected: u64 },
```

- [ ] **Step 4: Implement the check** in `RedbStore::open`. Decide fresh / current / refused **before** creating any table, because creating tables would make an old store look new:

```rust
pub const FORMAT_VERSION: u64 = 1;
const FORMAT_KEY: &str = "format_version";

pub fn open(path: &Path) -> Result<Self, StoreError> {
    let label = path.display().to_string();
    let db = Database::create(path).map_err(|e| StoreError::Unreachable {
        store: label.clone(),
        cause: e.to_string(),
    })?;

    // ⚠ Read before writing. A missing version key must not read as
    // "fresh": that would make an old store look empty — a vacuous pass.
    let found = {
        let tx = db.begin_read().map_err(backend)?;
        match tx.open_table(META) {
            Ok(meta) => Some(meta.get(FORMAT_KEY).map_err(backend)?.map(|v| v.value())),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(backend(e)),
        }
    };
    match found {
        // No META table at all: a brand-new file.
        None => Self::create_tables(&db)?,
        Some(Some(v)) if v == FORMAT_VERSION => {}
        Some(v) => return Err(StoreError::FormatVersion { found: v, expected: FORMAT_VERSION }),
    }
    Ok(Self { db, label })
}

fn create_tables(db: &Database) -> Result<(), StoreError> {
    let tx = db.begin_write().map_err(backend)?;
    {
        let mut meta = tx.open_table(META).map_err(backend)?;
        meta.insert(FORMAT_KEY, FORMAT_VERSION).map_err(backend)?;
    }
    tx.open_table(PROJECTS).map_err(backend)?;
    tx.open_table(GATES).map_err(backend)?;
    tx.open_table(TRANSITIONS).map_err(backend)?;
    tx.open_table(RECORDS).map_err(backend)?;
    tx.open_table(GATE_RUNS).map_err(backend)?;
    tx.open_table(ATTEMPTS).map_err(backend)?;
    tx.open_table(FINDINGS).map_err(backend)?;
    {
    tx.commit().map_err(backend)
}
```

Add `label: String` to `RedbStore` and a `pub fn label(&self) -> &str`. Export `FORMAT_VERSION`.

- [ ] **Step 5: Run.** `cargo test --workspace` — Expected: all pass. The existing `a_record_written_in_an_older_wire_format_is_refused_with_a_remedy` still passes (its store is written through `RedbStore`, so it is versioned).
- [ ] **Step 6: Mutation checks.** (a) Map `Some(None)` to "fresh" (`Some(None) => Self::create_tables(&db)?`) → the legacy test goes red. (b) Map the `Database::create` error with `backend` instead of `Unreachable` → the held-store test goes red. Restore both.
- [ ] **Step 7: Trio, then commit.**

```bash
git add crates/core/src/store.rs crates/store/src/lib.rs
git commit -m "feat(store): stamp and check a store format version"
```

---

### Task 4: Ids become IRIs; handles; ownership

The core of the plan. The whole workspace changes together, because an id type change does not compile halfway. Commit once, at green.

**Files:**
- Modify: `crates/core/src/{ids.rs,store.rs,mem.rs,conformance.rs,model.rs,finding.rs,log.rs,lib.rs}`, `crates/store/{Cargo.toml,src/lib.rs}`, `crates/exec/src/{evaluate.rs,finding.rs,record.rs,runner.rs,adapters/claude.rs}`, `crates/cli/src/{main.rs,cmd/*.rs}`, `crates/cli/tests/*.rs`, `docs/getting-started.md`
- Create: `crates/cli/src/refs.rs`, `crates/core/tests/wire_refs.rs`
- Workspace: `Cargo.toml` (`uuid`)

**Interfaces:**
- Consumes: `Iri` (Task 1), the role traits (Task 2), `FORMAT_VERSION`, `label()` (Task 3).
- Produces:
  - `fl_core::ids::Kind { Project, Gate, Record, Finding }` with `wire_names!` and `wire_parse!`; `Ord`.
  - `ProjectId(pub Iri)`, `GateId(pub Iri)`, `RecordId(pub Iri)`, `FindingId(pub Iri)`: `Clone` (not `Copy`), `#[serde(transparent)]`, `const KIND: Kind`, `fn iri(&self) -> &Iri`, `Display` prints the IRI.
  - `fl_core::ids::seq_iri(n: u64) -> Iri` = `urn:uuid:00000000-0000-7000-8000-{n:012x}`.
  - `StoreError::NotOwned { id: Iri, searched: Vec<String> }`, `StoreError::AlreadyExists(Iri)`.
  - `pub trait Handles { fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError>; fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError>; }`, implemented by `MemStore` and `RedbStore`.
  - `RedbStore::owns(&self, id: &Iri) -> Result<bool, StoreError>`.
  - `fl_cli::refs::{Ref, resolve, show}` (binary-internal).
  - `FORMAT_VERSION = 2`.

- [ ] **Step 1: Write the new failing tests first.**

(a) In `conformance.rs`, `all_roles` cases (these need all three roles in one store):

```rust
/// The original defect, as a regression (spec §6.2): two independent stores
/// each mint a gate, and nothing distinguishes them. Not in the shared suite —
/// `MemStore` mints deterministically — so it lives in the redb tests.

pub fn an_id_this_store_never_held_is_not_owned_rather_than_absent<S: Catalog + Tracker + Ledger>(s: &S) {
    let stranger = GateId(Iri::parse("urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b").unwrap());
    let err = s.get_gate(&stranger).expect_err("an unheld id must not read as `None`");
    assert!(matches!(err, StoreError::NotOwned { .. }), "{err:?}");
    assert!(err.to_string().contains(stranger.iri().as_str()), "{err}");
}

pub fn a_list_over_a_project_this_store_never_held_is_refused_not_empty<S: Catalog + Tracker + Ledger>(s: &S) {
    let stranger = ProjectId(Iri::parse("urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b").unwrap());
    for result in [
        s.list_gates(&stranger).map(|v| v.len()),
        s.list_transitions(&stranger).map(|v| v.len()),
        s.list_records(&stranger).map(|v| v.len()),
        s.list_findings(&stranger).map(|v| v.len()),
        s.attempts(&stranger).map(|v| v.len()),
    ] {
        assert!(matches!(result, Err(StoreError::NotOwned { .. })), "{result:?}");
    }
}

pub fn an_id_of_another_kind_is_owned_but_not_found<S: Catalog + Tracker + Ledger>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let as_gate = GateId(p.0.clone());
    assert_eq!(s.get_gate(&as_gate).unwrap(), None);
}

pub fn handles_are_per_kind_and_start_at_one<S: Catalog + Tracker + Ledger + Handles>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let g = s.add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o").unwrap();
    let r = s.add_record(&p, "t").unwrap();
    assert_eq!(s.handle_of(Kind::Project, p.iri()).unwrap(), Some(1));
    assert_eq!(s.handle_of(Kind::Gate, g.iri()).unwrap(), Some(1));
    assert_eq!(s.handle_of(Kind::Record, r.iri()).unwrap(), Some(1));
    assert_eq!(s.resolve_handle(Kind::Gate, 1).unwrap().as_ref(), Some(g.iri()));
    assert_eq!(s.resolve_handle(Kind::Gate, 2).unwrap(), None);
    assert_eq!(s.resolve_handle(Kind::Finding, 0).unwrap(), None);
}

/// Spec §2.6 / the deletion ruling: ownership is membership, and nothing
/// removes an entry. Every id minted along the way must still be owned at
/// the end.
pub fn no_operation_gives_up_an_owned_id<S: Catalog + Tracker + Ledger>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let g = s.add_gate(&p, "g", sample_kind(), sample_selector(), 1, "abc", "o").unwrap();
    let r = s.add_record(&p, "t").unwrap();
    let f = s.add_finding(Finding::raise(p.clone(), r.clone(), "a", "c")).unwrap();
    let mut def = s.get_gate(&g).unwrap().unwrap();
    def.authored_at_commit = "def".into();
    s.update_gate(&def).unwrap();
    s.set_record_state(&r, State::Doing).unwrap();
    let mut fin = s.get_finding(&f).unwrap().unwrap();
    fin.withdraw("x").unwrap();
    s.update_finding(&fin).unwrap();
    s.append_gate_run(sample_run(g.clone(), "abc", 1)).unwrap();
    assert!(s.get_project(&p).unwrap().is_some());
    assert!(s.get_gate(&g).unwrap().is_some());
    assert!(s.get_record(&r).unwrap().is_some());
    assert!(s.get_finding(&f).unwrap().is_some());
}
```

Register them in `all_roles` (`S: … + Handles` for the handle case: widen the `all_roles` bound to `Catalog + Tracker + Ledger + Handles`).

`mem.rs`'s `ids_are_handed_out_in_sequence_and_never_reused` is replaced by `handles_are_per_kind_and_start_at_one` (reason for the commit message: ids are no longer sequential numbers; the never-reused property moves to handles and is pinned across a reopen in (b)). `a_missing_project_is_none_and_not_an_error` is replaced by `an_id_this_store_never_held_is_not_owned_rather_than_absent` (reason: spec §5 — an unheld id is now `NotOwned`, by design).

(b) In `crates/store/src/lib.rs` tests:

```rust
#[test]
fn two_independent_stores_mint_different_ids() {
    let (a, _da) = fresh();
    let (b, _db) = fresh();
    let pa = a.add_project("/p").unwrap();
    let pb = b.add_project("/p").unwrap();
    let ga = a.add_gate(&pa, "g", kind(), selector(), 1, "abc", "o").unwrap();
    let gb = b.add_gate(&pb, "g", kind(), selector(), 1, "abc", "o").unwrap();
    assert_ne!(ga, gb, "two installs must not both have `gate 1` as their id");
    assert!(ga.iri().as_str().starts_with("urn:uuid:"));
}

#[test]
fn a_handle_is_never_reused_after_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    {
        let s = RedbStore::open(&path).unwrap();
        s.add_project("/a").unwrap();
    }
    let s = RedbStore::open(&path).unwrap();
    let b = s.add_project("/b").unwrap();
    assert_eq!(s.handle_of(Kind::Project, b.iri()).unwrap(), Some(2));
}

#[test]
fn a_minted_id_that_already_exists_is_refused_and_nothing_is_overwritten() {
    let (s, _d) = fresh();
    let id = Iri::parse("urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b").unwrap();
    s.insert_new_with_id(id.clone(), Kind::Project, PROJECTS, |i| Project { id: ProjectId(i), root: "/first".into() }).unwrap();
    let err = s
        .insert_new_with_id(id.clone(), Kind::Project, PROJECTS, |i| Project { id: ProjectId(i), root: "/second".into() })
        .unwrap_err();
    assert!(matches!(err, StoreError::AlreadyExists(ref i) if *i == id), "{err:?}");
    assert_eq!(s.get_project(&ProjectId(id)).unwrap().unwrap().root, "/first");
}

#[test]
fn a_format_1_store_is_refused_by_format_2() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.redb");
    {
        let db = redb::Database::create(&path).unwrap();
        let tx = db.begin_write().unwrap();
        tx.open_table(META).unwrap().insert("format_version", 1u64).unwrap();
        tx.commit().unwrap();
    }
    let err = RedbStore::open(&path).err().unwrap();
    assert!(matches!(err, StoreError::FormatVersion { found: Some(1), expected: 2 }), "{err:?}");
}
```

`the_id_counter_survives_a_reopen_so_ids_are_never_reused` and `a_failed_insert_does_not_advance_the_shared_id_counter` are rewritten against handles (`next_handle:<kind>` in `META`): the first becomes `a_handle_is_never_reused_after_a_reopen` above; the second keeps its shape — a failed insert (serialization of an unserializable value, as the test does today) must not advance `next_handle:project` and must leave no `IDS` entry. Say so in the commit message.

(c) Create `crates/core/tests/wire_refs.rs` — the reference-field scan of spec §6.5:

```rust
//! ⚠⚠ Every stored reference is a full IRI (spec §4, Invariant).
//!
//! The scan is driven by what each type SERIALIZES as an IRI, not by a list
//! of field names: every IRI string in the JSON is replaced, one at a time,
//! with a handle-shaped number, and the type must refuse to read it back. A
//! reference field that would accept a handle is the defect. Each type also
//! carries a floor on how many IRIs it must contain, so a scan that silently
//! found fewer fields fails instead of passing over nothing.

use fl_core::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

fn iri(n: u64) -> Iri {
    fl_core::ids::seq_iri(n)
}

fn iri_paths(v: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match v {
        Value::String(s) if Iri::parse(s).is_ok() && s.starts_with("urn:uuid:") => out.push(path.clone()),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                path.push(i.to_string());
                iri_paths(item, path, out);
                path.pop();
            }
        }
        Value::Object(map) => {
            for (k, item) in map {
                path.push(k.clone());
                iri_paths(item, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

fn set(v: &mut Value, path: &[String], to: Value) {
    let mut cur = v;
    for key in path {
        cur = match cur {
            Value::Array(a) => &mut a[key.parse::<usize>().unwrap()],
            Value::Object(m) => m.get_mut(key).unwrap(),
            _ => unreachable!(),
        };
    }
    *cur = to;
}

fn assert_every_reference_refuses_a_handle<T: Serialize + DeserializeOwned>(name: &str, sample: &T, floor: usize) {
    let json = serde_json::to_value(sample).unwrap();
    let mut paths = Vec::new();
    iri_paths(&json, &mut Vec::new(), &mut paths);
    assert!(paths.len() >= floor, "{name}: found {} IRI fields, expected at least {floor}", paths.len());
    for p in &paths {
        let mut broken = json.clone();
        set(&mut broken, p, Value::from(3u64));
        assert!(
            serde_json::from_value::<T>(broken).is_err(),
            "{name}.{} accepted a handle where a full IRI belongs",
            p.join(".")
        );
    }
}

#[test]
fn every_reference_field_on_the_wire_is_a_full_iri() {
    let (p, g, r, f) = (ProjectId(iri(1)), GateId(iri(2)), RecordId(iri(3)), FindingId(iri(4)));
    // One sample per type that crosses the process boundary, with every
    // Option set to Some and every Vec non-empty, so no reference field is
    // hidden by a None or an empty list.
    let project = Project { id: p.clone(), root: "/p".into() };
    let gate_def = GateDef {
        id: g.clone(),
        project: p.clone(),
        name: "g".into(),
        kind: GateKind::Command(CommandSpec {
            program: "true".into(),
            args: vec!["a".into()],
            delivery: PopulationDelivery::Args,
            timeout_secs: 1,
            pass_codes: vec![0],
        }),
        selector: Selector::Glob { pattern: "*.rs".into() },
        min_population: 1,
        authored_at_commit: "abc".into(),
        authored_by: "o".into(),
        last_pass_commit: Some("abc".into()),
    };
    let transition = Transition {
        project: p.clone(),
        name: "launch".into(),
        from: State::Review,
        to: State::Done,
        regret: Regret::High,
        gates: vec![g.clone()],
    };
    let record = Record { id: r.clone(), project: p.clone(), title: "t".into(), state: State::Todo };
    let mut finding = Finding::raise(p.clone(), r.clone(), "a", "c");
    finding.id = f.clone();
    finding.reproduction = Some(g.clone());
    let gate_run = GateRun {
        gate: g.clone(),
        record: Some(r.clone()),
        commit: "abc".into(),
        verdict: Verdict::from_predicate(true, 1),
        population: 1,
        output_excerpt: String::new(),
        duration_ms: 1,
        cost_usd_micros: 0,
    };
    let attempt = Attempt {
        project: p.clone(),
        record: r.clone(),
        adapter: "claude".into(),
        status: AttemptStatus::Completed,
        duration_ms: 1,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd_micros: 0,
        paths_touched: vec!["a.rs".into()],
        output_excerpt: String::new(),
    };
    assert_every_reference_refuses_a_handle("Project", &project, 1);
    assert_every_reference_refuses_a_handle("GateDef", &gate_def, 2);
    assert_every_reference_refuses_a_handle("Transition", &transition, 2);
    assert_every_reference_refuses_a_handle("Record", &record, 2);
    assert_every_reference_refuses_a_handle("Finding", &finding, 4);
    assert_every_reference_refuses_a_handle("GateRun", &gate_run, 2);
    assert_every_reference_refuses_a_handle("Attempt", &attempt, 2);
}
```

The `…` comment names the seven values the implementer builds; every field of each struct is listed in the files named, and the test must compile against them — there is no guessing involved.

(d) In `crates/cli/tests/cli.rs` — Review Focus 1 and 5:

```rust
#[test]
fn an_iri_typed_in_uppercase_names_the_same_item() {
    let d = tempfile::tempdir().unwrap();
    let _repo = project(&d);
    cli(&d)
        .args(["gate", "add", "--project", "1", "--name", "g", "--glob", "*.rs", "--program", "true"])
        .assert()
        .success();
    // `gate show 1` prints the gate's JSON, whose `id` is the full IRI.
    let shown = cli(&d).args(["gate", "show", "1"]).output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let id = json["id"].as_str().unwrap().to_string();
    cli(&d).args(["gate", "show", &id.to_uppercase()]).assert().success();
}

#[test]
fn handle_input_at_the_edges_is_refused_by_name() {
    let d = tempfile::tempdir().unwrap();
    let _repo = project(&d);
    for bad in ["0", "7", "99999999999999999999", "3abc"] {
        cli(&d)
            .args(["gate", "show", bad])
            .assert()
            .code(2)
            .stderr(contains(bad));
    }
}

#[test]
fn a_list_over_a_project_that_does_not_exist_is_refused_not_empty() {
    let d = tempfile::tempdir().unwrap();
    let _repo = project(&d);
    cli(&d).args(["record", "list", "--project", "9"]).assert().code(2).stderr(contains("9"));
}
```

Integration tests can use `serde_json` directly: it is already a normal dependency of `fl-cli`.

- [ ] **Step 2: Run to confirm failure.** `cargo test --workspace` — Expected: compile errors throughout. This is the signal to start.

- [ ] **Step 3: Rewrite `crates/core/src/ids.rs`:**

```rust
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
```

Check the exact invocation shape of `wire_parse!` in `crates/core/src/wire.rs` and match it. Export `Kind` from `lib.rs`.

- [ ] **Step 4: `StoreError` and `Handles`** in `store.rs`:

```rust
    /// ⚠ This store never held the id. It did not look anywhere else, so this
    /// is never "not found" — `searched` says exactly where it looked.
    #[error("no store holds {id} (searched: {})", searched.join(", "))]
    NotOwned { id: Iri, searched: Vec<String> },
    #[error("{0} already exists; an insert never overwrites")]
    AlreadyExists(Iri),
```

```rust
/// Short names a person types and reads (spec §4). Display only: a handle
/// never enters a stored item or the wire. Per store and per kind.
pub trait Handles {
    fn handle_of(&self, kind: Kind, id: &Iri) -> Result<Option<u64>, StoreError>;
    fn resolve_handle(&self, kind: Kind, handle: u64) -> Result<Option<Iri>, StoreError>;
}
```

Export both from `lib.rs`: `pub use store::{Catalog, Handles, Ledger, Roles, StoreError, Tracker};` and `pub use ids::{FindingId, GateId, Kind, ProjectId, RecordId};`.

- [ ] **Step 5: `MemStore`.** Re-key every map by `Iri`, and add the ownership index and handle tables:

```rust
const LABEL: &str = "memory";

#[derive(Default)]
struct Inner {
    next_id: u64,
    /// Every id this store ever minted, with its kind. Append-only: nothing
    /// removes an entry. Ownership is membership (spec §2.6).
    owned: BTreeMap<Iri, Kind>,
    next_handle: BTreeMap<Kind, u64>,
    handles: BTreeMap<(Kind, u64), Iri>,
    handle_of: BTreeMap<Iri, u64>,
    projects: BTreeMap<Iri, Project>,
    gates: BTreeMap<Iri, GateDef>,
    transitions: BTreeMap<(Iri, String), Transition>,
    records: BTreeMap<Iri, Record>,
    runs: Vec<GateRun>,
    attempts: Vec<Attempt>,
    findings: BTreeMap<Iri, Finding>,
}

impl Inner {
    fn mint(&mut self, kind: Kind) -> Iri {
        self.next_id += 1;
        let id = seq_iri(self.next_id);
        self.owned.insert(id.clone(), kind);
        let n = self.next_handle.entry(kind).or_insert(0);
        *n += 1;
        self.handles.insert((kind, *n), id.clone());
        self.handle_of.insert(id.clone(), *n);
        id
    }

    /// ⚠ Every method that takes an id asks this first — list methods too.
    /// A list over a project this store never held is "didn't look", and an
    /// empty list would say "looked, found nothing".
    fn check(&self, id: &Iri) -> Result<Kind, StoreError> {
        self.owned.get(id).copied().ok_or_else(|| StoreError::NotOwned {
            id: id.clone(),
            searched: vec![LABEL.to_string()],
        })
    }
}
```

Method rules: every `add_*` calls `check` on each id argument (the project; for `add_finding`, the finding's `project` and `record`), then `mint(Kind::…)`. Every `get_*` calls `check(id)` and then reads its map (`Ok(None)` if the id is owned under another kind). Every `list_*` / `attempts` calls `check(project)` first. `update_*` calls `check`, then returns `NoSuch*` if the map lacks it. `gate_runs(gate)` calls `check(gate)`. `append_*` checks nothing (spec §3.4: the engine already resolved the references it is recording). `impl Handles for MemStore` reads `handles` / `handle_of`.

- [ ] **Step 6: `RedbStore` schema v2.** Add to `crates/store/Cargo.toml`: `uuid = { workspace = true }`; in the workspace `Cargo.toml`: `uuid = { version = "1.26", features = ["v7"] }`. Replace the table set:

```rust
pub const FORMAT_VERSION: u64 = 2;

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// Every id this store ever minted → its kind's wire name. Append-only.
const IDS: TableDefinition<&str, &str> = TableDefinition::new("ids");
const HANDLES: TableDefinition<(&str, u64), &str> = TableDefinition::new("handles");
const HANDLE_OF: TableDefinition<&str, u64> = TableDefinition::new("handle_of");
const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");
const GATES: TableDefinition<&str, &str> = TableDefinition::new("gates");
const TRANSITIONS: TableDefinition<(&str, &str), &str> = TableDefinition::new("transitions");
const RECORDS: TableDefinition<&str, &str> = TableDefinition::new("records");
const FINDINGS: TableDefinition<&str, &str> = TableDefinition::new("findings");
/// Ledger rows keep internal sequence keys. They are not ids and never leave
/// the store.
const GATE_RUNS: TableDefinition<u64, &str> = TableDefinition::new("gate_runs");
const ATTEMPTS: TableDefinition<u64, &str> = TableDefinition::new("attempts");
```

`NEXT_ID` goes; `NEXT_RUN` / `NEXT_ATTEMPT` stay. Handle counters are `next_handle:<kind wire>` in `META`. Replace `bump_and_put` with:

```rust
fn insert_new<T: serde::Serialize>(
    &self,
    kind: Kind,
    table: TableDefinition<&str, &str>,
    build: impl FnOnce(Iri) -> T,
) -> Result<Iri, StoreError> {
    let id = Iri::parse(&format!("urn:uuid:{}", uuid::Uuid::now_v7())).map_err(backend)?;
    self.insert_new_with_id(id, kind, table, build)
}

/// Mint, index, hand out a handle, and write the row — in ONE write
/// transaction, so they land together or not at all (the lesson of the old
/// `bump_and_put`). `pub(crate)` so a test can force a collision.
pub(crate) fn insert_new_with_id<T: serde::Serialize>(
    &self,
    id: Iri,
    kind: Kind,
    table: TableDefinition<&str, &str>,
    build: impl FnOnce(Iri) -> T,
) -> Result<Iri, StoreError> {
    let json = serde_json::to_string(&build(id.clone())).map_err(backend)?;
    let tx = self.db.begin_write().map_err(backend)?;
    {
        let mut ids = tx.open_table(IDS).map_err(backend)?;
        if ids.get(id.as_str()).map_err(backend)?.is_some() {
            return Err(StoreError::AlreadyExists(id));
        }
        ids.insert(id.as_str(), kind.as_wire()).map_err(backend)?;

        let key = format!("next_handle:{}", kind.as_wire());
        let mut meta = tx.open_table(META).map_err(backend)?;
        let n = meta.get(key.as_str()).map_err(backend)?.map(|v| v.value()).unwrap_or(0) + 1;
        meta.insert(key.as_str(), n).map_err(backend)?;

        tx.open_table(HANDLES).map_err(backend)?
            .insert((kind.as_wire(), n), id.as_str()).map_err(backend)?;
        tx.open_table(HANDLE_OF).map_err(backend)?
            .insert(id.as_str(), n).map_err(backend)?;
        tx.open_table(table).map_err(backend)?
            .insert(id.as_str(), json.as_str()).map_err(backend)?;
    }
    tx.commit().map_err(backend)?;
    Ok(id)
}

/// The kind this store holds `id` under, or `NotOwned` naming this store.
fn check(&self, id: &Iri) -> Result<Kind, StoreError> {
    let tx = self.db.begin_read().map_err(backend)?;
    let ids = tx.open_table(IDS).map_err(backend)?;
    let Some(v) = ids.get(id.as_str()).map_err(backend)? else {
        return Err(StoreError::NotOwned { id: id.clone(), searched: vec![self.label.clone()] });
    };
    Kind::from_wire(v.value()).ok_or_else(|| decode(format!("unknown kind `{}` for {id}", v.value())))
}

pub fn owns(&self, id: &Iri) -> Result<bool, StoreError> {
    match self.check(id) {
        Ok(_) => Ok(true),
        Err(StoreError::NotOwned { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}
```

`get_json` / `put_json` / `all_json` take `TableDefinition<&str, &str>` and `&Iri`. `get_json` calls `check` first. Transitions key on `(project.iri().as_str(), name)`. Method rules are the same as `MemStore`'s in Step 5. `create_tables` creates every table above and writes `FORMAT_VERSION`. `impl Handles for RedbStore` reads `HANDLE_OF` and `HANDLES`.

- [ ] **Step 7: Port the model, exec and core call sites.** Ids are no longer `Copy`: take by reference where the Task 2 signatures already do, `.clone()` where a struct field needs an owned id, and replace `*id` with `id.clone()`. `Finding::raise` keeps its placeholder, now `FindingId(seq_iri(0))`, with its comment updated ("the store replaces it"). The conformance case `a_finding_round_trips_and_gets_a_real_id` asserts `id != FindingId(seq_iri(0))` in place of `id.get() != 0`. Test code that wrote `ProjectId(1)` / `GateId(9999)` for an id it never stored uses `ProjectId(seq_iri(1))` / `GateId(seq_iri(9999))`, and test code that needs a real id uses the one the store returned. `runner.rs` / `adapters/claude.rs` tests: `RecordId(seq_iri(1))`.

  Messages in `fl-exec` follow the naming rule: `no project with id {project}` becomes a `NotOwned` from the store (propagate it); `project {project} declares no transition named …` becomes `the project at {root} declares no transition named …`; `transition `{name}` names gate {gate_id}, which does not exist` keeps the IRI (a dangling gate has no handle and no name).

- [ ] **Step 8: CLI handles.** Create `crates/cli/src/refs.rs`:

```rust
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
            format!("`{s}` is neither a handle (a number such as `3`) nor an IRI (such as `urn:uuid:…`): {e}")
        })
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
```

  Every clap argument that names an item (`--project`, `--record`, `--gate`, and the positional `id` / `finding` / `record`) changes type from `u64` to `Ref` (transition `--gate` becomes `Vec<Ref>`). Each command resolves with `refs::resolve(store, store.label(), Kind::…, &arg)?` into the typed id, and prints ids with `refs::show`. A message that echoes what the user asked for echoes their input (`{arg}` as typed), not the resolved IRI.

- [ ] **Step 9: The guide.** Run `cargo test -p fl-cli --test getting_started`. Handles are per kind now, so numbers in the guide shift (the first gate is `1`, not `2`). Update `docs/getting-started.md` so every command and every printed output matches, and update the prose that explains ids: the number printed is the item's **handle** — a short name local to this store — and its full id is an IRI, visible in `gate show`. `VERIFIED_COMMANDS` must still equal the number of verified commands; if a command was added or removed, change it and say why in the commit.

- [ ] **Step 10: Run everything.** `cargo test --workspace` — Expected: all pass.

- [ ] **Step 11: Mutation checks.** (a) In `MemStore::check` and `RedbStore::check`, return `Ok(Kind::Project)` for an unknown id → the `NotOwned` conformance cases go red for that store. (b) Delete the `check(project)` call from `list_records` → `a_list_over_a_project_this_store_never_held_is_refused_not_empty` goes red. (c) Remove the `AlreadyExists` early return → the collision test goes red. (d) Make ids lenient the way a well-meant change would: give `Iri`'s `Deserialize` a visitor that also accepts a number (`visit_u64` returning `seq_iri(n)`) → `every_reference_field_on_the_wire_is_a_full_iri` goes red. Restore all. Restore all.

- [ ] **Step 12: Trio, then commit.** List replaced tests and their reasons in the message body.

```bash
git add Cargo.toml Cargo.lock crates/ docs/getting-started.md
git commit -m "feat: ids become IRIs; handles per store and kind; ownership is membership"
```

---

### Task 5: Following references — `Dangling` and aliases

**Files:**
- Modify: `crates/core/src/{store.rs,mem.rs,conformance.rs,model.rs,finding.rs}`, `crates/store/src/lib.rs`, `crates/exec/src/{evaluate.rs,finding.rs}`

**Interfaces:**
- Consumes: Task 4's ownership and `check`.
- Produces:
  - `StoreError::Dangling { from: String, to: Iri }`.
  - `pub fn follow<T>(from: &str, to: &Iri, got: Result<Option<T>, StoreError>) -> Result<T, StoreError>` in `fl_core::store`.
  - `Record.also_known_as: Vec<Iri>` and `Finding.also_known_as: Vec<Iri>`, `#[serde(default)]`.
  - `Tracker::add_alias(&self, primary: &Iri, alias: Iri) -> Result<(), StoreError>`.

- [ ] **Step 1: Failing tests.** In `conformance.rs` `tracker` suite:

```rust
pub fn an_alias_reaches_the_item_it_names<S: Catalog + Tracker>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let r = s.add_record(&p, "t").unwrap();
    let old = Iri::parse("https://github.com/o/r/issues/41").unwrap();
    s.add_alias(r.iri(), old.clone()).unwrap();
    let via_alias = s.get_record(&RecordId(old.clone())).unwrap().unwrap();
    assert_eq!(via_alias.id, r);
    assert!(via_alias.also_known_as.contains(&old));
}

pub fn an_alias_already_in_use_is_refused_and_names_it<S: Catalog + Tracker>(s: &S) {
    let p = s.add_project("/p").unwrap();
    let a = s.add_record(&p, "a").unwrap();
    let b = s.add_record(&p, "b").unwrap();
    let old = Iri::parse("https://github.com/o/r/issues/41").unwrap();
    s.add_alias(a.iri(), old.clone()).unwrap();
    let err = s.add_alias(b.iri(), old.clone()).unwrap_err();
    assert!(matches!(err, StoreError::AlreadyExists(ref i) if *i == old), "{err:?}");
    // An existing primary id cannot become an alias either.
    let err = s.add_alias(b.iri(), a.0.clone()).unwrap_err();
    assert!(matches!(err, StoreError::AlreadyExists(_)), "{err:?}");
}
```

In `crates/exec/src/evaluate.rs` tests, replace the body of `a_transition_naming_a_gate_that_does_not_exist_is_refused` (keep its name and comment) so it covers both outcomes:

```rust
let d = repo_with(&[("src/a.rs", "x")]);
let s = MemStore::default();
let p = s.add_project(&d.path().display().to_string()).unwrap();

// A gate id that no store holds: "never looked" — NotOwned, with the
// transition named so the reader knows where the reference came from.
let stranger = GateId(seq_iri(9999));
s.add_transition(Transition {
    project: p.clone(),
    name: "launch".into(),
    from: State::Review,
    to: State::Done,
    regret: Regret::Low,
    gates: vec![stranger.clone()],
})
.unwrap();
let err = evaluate_transition(&s, &s, &p, "launch", None).unwrap_err();
assert!(err.to_string().contains(stranger.iri().as_str()), "got {err}");
assert!(err.to_string().contains("launch"), "got {err}");

// An id the store holds as another kind: "gone" — Dangling.
let record = s.add_record(&p, "t").unwrap();
s.add_transition(Transition {
    project: p.clone(),
    name: "ship".into(),
    from: State::Review,
    to: State::Done,
    regret: Regret::Low,
    gates: vec![GateId(record.0.clone())],
})
.unwrap();
let err = evaluate_transition(&s, &s, &p, "ship", None).unwrap_err();
assert!(err.to_string().contains("dangling"), "got {err}");
```

- [ ] **Step 2: Run to confirm failure.** `cargo test --workspace` — compile errors.
- [ ] **Step 3: Implement.**

```rust
    #[error("{from} refers to {to}, which is dangling: the store holds that id, but not as this kind of item")]
    Dangling { from: String, to: Iri },
```

```rust
/// Follow a stored reference. `Ok(None)` from the owning store means the
/// reference points at nothing of this kind: dangling. `NotOwned` passes
/// through unchanged — "no store holds it" is not the same as "gone".
pub fn follow<T>(from: &str, to: &Iri, got: Result<Option<T>, StoreError>) -> Result<T, StoreError> {
    match got {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err(StoreError::Dangling { from: from.to_string(), to: to.clone() }),
        Err(e) => Err(e),
    }
}
```

In `evaluate_transition`, the gate loop becomes `follow(&format!("transition `{transition_name}`"), gate_id.iri(), catalog.get_gate(gate_id))`, and a `NotOwned` is wrapped into the existing `BadSelector` message so that it names the transition: `transition `{name}` names gate {gate_id}: {e}`. Apply `follow` the same way in `finding.rs` to `f.reproduction` and in `verify_finding`'s project lookup.

Add `#[serde(default)] pub also_known_as: Vec<Iri>` to `Record` and `Finding`, and `also_known_as: vec![]` to every struct literal that builds one: `add_record` in both stores, `Finding::raise`, and the samples in `crates/core/tests/wire_refs.rs`.

Aliases: `IDS` maps an alias to the kind wire name `"alias"`; a new table `ALIASES: &str → &str` (alias → primary). `add_alias` in one write transaction: refuse `AlreadyExists(alias)` if the alias is in `IDS` at all; `check(primary)` must be `Record` or `Finding`; insert into `IDS` and `ALIASES`; push onto the item's `also_known_as` and rewrite its row. `check` follows an alias to its primary's kind, and `get_record` / `get_finding` look up the primary. `MemStore` mirrors this with an `aliases: BTreeMap<Iri, Iri>`. Kind gets no `Alias` variant — `"alias"` is an index marker, not a kind, and `check` handles it before `Kind::from_wire`.

- [ ] **Step 4: Run.** `cargo test --workspace` — all pass.
- [ ] **Step 5: Mutation checks.** (a) In `follow`, map `Ok(None)` to `NotOwned` → the dangling assertion goes red. (b) Drop the `AlreadyExists` check in `add_alias` → the alias conformance case goes red (for both stores). Restore.
- [ ] **Step 6: Trio, then commit.**

```bash
git add crates/
git commit -m "feat: follow references as dangling or not-owned; tracker aliases"
```

---

### Task 6: Binding a project to a store

**Files:**
- Create: `crates/cli/src/config.rs`, `crates/cli/tests/stores.rs`
- Modify: `crates/cli/src/main.rs`, `crates/cli/src/cmd/*.rs` (an `iris()` method per `Cmd`), `crates/cli/Cargo.toml`, `Cargo.toml`, `README.md`

**Interfaces:**
- Consumes: `RedbStore::{open, owns, label}`, `StoreError::NotOwned`, `refs::Ref`.
- Produces: `config::{Entry { root: PathBuf, store: PathBuf }, path() -> Option<PathBuf>, load(path: Option<&Path>) -> Result<Vec<Entry>>, bound(entries: &[Entry], cwd: &Path) -> Option<PathBuf>}`; `Cmd::iris(&self) -> Vec<Iri>` on each command module.

- [ ] **Step 1: Failing tests** in `crates/cli/tests/stores.rs`. Every test sets `XDG_CONFIG_HOME` and `XDG_DATA_HOME` to temp directories and removes `FL_DB`, so no test reads the developer's own config. Build the helpers from `cli.rs`'s `git_repo` (copy the function in full into this file).

```rust
use assert_cmd::Command;
use predicates::str::contains;
use std::path::{Path, PathBuf};
use std::process::Command as Sys;

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Sys::new("git").args(args).current_dir(dir).output().unwrap().status.success(),
        "git {args:?} failed"
    );
}

fn git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@example.com"]);
    git(repo.path(), &["config", "user.name", "t"]);
    std::fs::write(repo.path().join("a.rs"), "fn a() {}").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    repo
}

/// Private config and data homes, so no test reads the developer's own.
struct Env {
    config: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self { config: tempfile::tempdir().unwrap(), data: tempfile::tempdir().unwrap() }
    }
    fn fl(&self, cwd: &Path) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("XDG_CONFIG_HOME", self.config.path())
            .env("XDG_DATA_HOME", self.data.path())
            .env_remove("FL_DB")
            .current_dir(cwd);
        c
    }
    fn default_store(&self) -> PathBuf {
        self.data.path().join("fl").join("fl.redb")
    }
    fn write_config(&self, body: &str) {
        let dir = self.config.path().join("fl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), body).unwrap();
    }
}

fn bind(repo: &Path, store: &Path) -> String {
    format!("[[project]]\nroot = \"{}\"\nstore = \"{}\"\n", repo.display(), store.display())
}

const STRANGER: &str = "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b";

// Review Focus 4.
#[test]
fn a_project_bound_in_config_uses_its_store_from_a_subdirectory() {
    let env = Env::new();
    let repo = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let a = stores.path().join("a.redb");
    env.write_config(&bind(repo.path(), &a));
    let sub = repo.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    env.fl(&sub).args(["project", "add", repo.path().to_str().unwrap()]).assert().success();
    assert!(a.exists(), "the bound store was not used");
    assert!(!env.default_store().exists(), "the default store was created anyway");
}

// Review Focus 4.
#[cfg(unix)]
#[test]
fn a_symlinked_working_directory_binds_like_the_real_one() {
    let env = Env::new();
    let repo = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let a = stores.path().join("a.redb");
    env.write_config(&bind(repo.path(), &a));
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("via-link");
    std::os::unix::fs::symlink(repo.path(), &link).unwrap();
    env.fl(&link).args(["project", "list"]).assert().success();
    assert!(a.exists(), "a symlinked cwd did not bind");
    assert!(!env.default_store().exists());
}

#[test]
fn precedence_is_db_then_env_then_config_then_default() {
    let env = Env::new();
    let repo = git_repo();
    let stores = tempfile::tempdir().unwrap();
    let flag = stores.path().join("flag.redb");
    let var = stores.path().join("env.redb");
    let cfg = stores.path().join("cfg.redb");
    env.write_config(&bind(repo.path(), &cfg));

    env.fl(repo.path())
        .env("FL_DB", &var)
        .args(["--db", flag.to_str().unwrap(), "project", "list"])
        .assert()
        .success();
    assert!(flag.exists() && !var.exists() && !cfg.exists(), "--db did not win");

    env.fl(repo.path()).env("FL_DB", &var).args(["project", "list"]).assert().success();
    assert!(var.exists() && !cfg.exists(), "$FL_DB did not beat the config");

    env.fl(repo.path()).args(["project", "list"]).assert().success();
    assert!(cfg.exists() && !env.default_store().exists(), "the config did not beat the default");

    let elsewhere = tempfile::tempdir().unwrap();
    env.fl(elsewhere.path()).args(["project", "list"]).assert().success();
    assert!(env.default_store().exists(), "an unbound directory did not fall to the default");
}

// Review Focus 3.
#[test]
fn a_malformed_config_is_an_error_naming_the_file() {
    let env = Env::new();
    let repo = git_repo();
    env.write_config("[[project]\nroot =");
    env.fl(repo.path()).args(["project", "list"]).assert().code(2).stderr(contains("config.toml"));
    assert!(!env.default_store().exists(), "a broken config fell through to the default store");
}

// Review Focus 3.
#[test]
fn a_relative_store_path_in_config_is_refused() {
    let env = Env::new();
    let repo = git_repo();
    env.write_config(&format!(
        "[[project]]\nroot = \"{}\"\nstore = \"stores/a.redb\"\n",
        repo.path().display()
    ));
    env.fl(repo.path()).args(["project", "list"]).assert().code(2).stderr(contains("absolute"));
}

/// Two repos, each bound to its own store, each registered in it.
fn two_bound_stores(env: &Env) -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir, PathBuf, PathBuf) {
    let (ra, rb) = (git_repo(), git_repo());
    let stores = tempfile::tempdir().unwrap();
    let (a, b) = (stores.path().join("a.redb"), stores.path().join("b.redb"));
    env.write_config(&format!("{}{}", bind(ra.path(), &a), bind(rb.path(), &b)));
    env.fl(ra.path()).args(["project", "add", ra.path().to_str().unwrap()]).assert().success();
    env.fl(rb.path()).args(["project", "add", rb.path().to_str().unwrap()]).assert().success();
    (ra, rb, stores, a, b)
}

#[test]
fn an_iri_selects_the_store_that_holds_it() {
    let env = Env::new();
    let (ra, rb, _stores, _a, _b) = two_bound_stores(&env);
    env.fl(ra.path())
        .args(["gate", "add", "--project", "1", "--name", "in-a", "--glob", "*.rs", "--program", "true"])
        .assert()
        .success();
    let shown = env.fl(ra.path()).args(["gate", "show", "1"]).output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let iri = json["id"].as_str().unwrap().to_string();

    env.fl(rb.path()).args(["gate", "show", &iri]).assert().success().stdout(contains("in-a"));
}

// Spec §6.4a.
#[test]
fn an_iri_no_store_holds_is_not_owned_and_names_every_store_searched() {
    let env = Env::new();
    let (_ra, rb, _stores, a, b) = two_bound_stores(&env);
    env.fl(rb.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(contains(a.to_str().unwrap()))
        .stderr(contains(b.to_str().unwrap()));
}

#[test]
fn a_store_that_cannot_be_opened_during_an_iri_search_is_an_error_not_a_skip() {
    let env = Env::new();
    let (ra, rb) = (git_repo(), git_repo());
    let stores = tempfile::tempdir().unwrap();
    let (a, b) = (stores.path().join("a.redb"), stores.path().join("b.redb"));
    std::fs::write(&b, b"this is not a database").unwrap();
    env.write_config(&format!("{}{}", bind(ra.path(), &a), bind(rb.path(), &b)));
    env.fl(ra.path()).args(["project", "add", ra.path().to_str().unwrap()]).assert().success();
    env.fl(ra.path())
        .args(["gate", "show", STRANGER])
        .assert()
        .code(2)
        .stderr(contains("could not open"))
        .stderr(contains(b.to_str().unwrap()));
}
```

- [ ] **Step 2: Run to confirm failure.** `cargo test -p fl-cli --test stores` — Expected: failures (no config support).

- [ ] **Step 3: Implement `config.rs`.** Add `toml = "1.1"` to the workspace and `toml.workspace = true` and `serde.workspace = true` to `fl-cli`.

```rust
//! The user-level binding of projects to stores (spec §2.6).
//!
//! User-level, not in the repository: a store path belongs to a machine, and
//! a public repository would publish it.
//!
//! ⚠ A config file that exists but cannot be read is an ERROR, never a
//! silent fall-through to the default store. Falling through would put a
//! project's records in a store nobody chose.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub root: PathBuf,
    pub store: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    project: Vec<Entry>,
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("fl").join("config.toml"))
}

pub fn load(path: Option<&Path>) -> Result<Vec<Entry>> {
    let Some(path) = path else { return Ok(vec![]) };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    let file: File = toml::from_str(&text)
        .with_context(|| format!("{} is not a valid fl config", path.display()))?;
    for e in &file.project {
        if !e.root.is_absolute() || !e.store.is_absolute() {
            bail!(
                "{}: `root` and `store` must be absolute paths (got root `{}`, store `{}`)",
                path.display(), e.root.display(), e.store.display()
            );
        }
    }
    Ok(file.project)
}

/// The store bound to the project containing `cwd`: the entry whose root is
/// the longest ancestor of `cwd`. Both sides are canonicalized, so a
/// symlinked path binds like the real one.
pub fn bound(entries: &[Entry], cwd: &Path) -> Option<PathBuf> {
    let cwd = cwd.canonicalize().ok()?;
    entries
        .iter()
        .filter_map(|e| e.root.canonicalize().ok().map(|r| (r, e)))
        .filter(|(r, _)| cwd.starts_with(r))
        .max_by_key(|(r, _)| r.components().count())
        .map(|(_, e)| e.store.clone())
}
```

- [ ] **Step 4: Wire it in `main.rs`.** `db_path(explicit, entries, cwd)` precedence: `--db`, `$FL_DB`, `config::bound(entries, cwd)`, the XDG default. Then choose the store:

```rust
/// A full IRI on the command line selects the store that holds it (spec
/// §2.6). Handles resolve only in the bound store, so a command with no IRI
/// uses the bound store.
fn choose_store(bound: &Path, entries: &[config::Entry], iris: &[Iri]) -> Result<PathBuf> {
    if iris.is_empty() {
        return Ok(bound.to_path_buf());
    }
    let mut candidates = vec![bound.to_path_buf()];
    for e in entries {
        if !candidates.contains(&e.store) {
            candidates.push(e.store.clone());
        }
    }
    // Never create a store while searching: only files that exist are stores.
    candidates.retain(|c| c.exists());

    let mut chosen: Option<PathBuf> = None;
    for id in iris {
        let mut owners = Vec::new();
        for c in &candidates {
            // ⚠ A store that cannot be opened is an ERROR here, not a
            // "doesn't have it": skipping it would search less than it says.
            let s = RedbStore::open(c).with_context(|| format!("could not open the store at {}", c.display()))?;
            if s.owns(id)? {
                owners.push(c.clone());
            }
        }
        match owners.as_slice() {
            [] => {
                return Err(StoreError::NotOwned {
                    id: id.clone(),
                    searched: candidates.iter().map(|c| c.display().to_string()).collect(),
                }
                .into())
            }
            [one] => match &chosen {
                Some(prev) if prev != one => bail!(
                    "this command names items in two different stores ({} and {}); name items from one store",
                    prev.display(), one.display()
                ),
                _ => chosen = Some(one.clone()),
            },
            many => bail!(
                "{id} is held by more than one store: {}. Refusing to pick one.",
                many.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            ),
        }
    }
    Ok(chosen.expect("iris is non-empty"))
}
```

Each `cmd` module gets `impl Cmd { pub fn iris(&self) -> Vec<Iri> }` returning every `Ref::Iri` among its item arguments; `Command::iris()` in `main.rs` dispatches to them.

- [ ] **Step 5: README.** Add a short section "Where the store lives" to `README.md` with the four-tier precedence and this example:

```toml
# ~/.config/fl/config.toml
[[project]]
root = "/home/you/code/app"
store = "/home/you/.local/share/fl/app.redb"
```

- [ ] **Step 6: Run.** `cargo test --workspace` — all pass.
- [ ] **Step 7: Mutation checks.** (a) In `load`, return `Ok(vec![])` when parsing fails → `a_malformed_config_is_an_error_naming_the_file` goes red. (b) In `choose_store`, `continue` past a store that fails to open → `a_store_that_cannot_be_opened_during_an_iri_search_is_an_error_not_a_skip` goes red. (c) Remove `canonicalize` from `bound` → the symlink test goes red. Restore all.
- [ ] **Step 8: Trio, then commit.**

```bash
git add Cargo.toml Cargo.lock crates/cli README.md
git commit -m "feat(cli): bind projects to stores in user config; an IRI selects its store"
```

---

## After the last task (controller, not an implementer)

- Open the pull request against `main` for the owner to review and merge.
- After merge: update `Notes/FerroLoop/2026-09-17-ratified-decisions.md` — mark stable identity closed, with the date and a pointer to the spec and this plan. `Notes/` is a shared working tree: stage that one path explicitly.

## Parked observation

`run_gate` stamps `last_pass_commit` with `let _ = store.update_gate(&updated);`, discarding a catalog write failure. That pre-dates this plan and is out of its scope, but it is a swallowed error on the "nothing vs didn't look" line. Raise it as its own finding.
