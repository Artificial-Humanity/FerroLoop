# Nested Projects Implementation Plan

> **For agentic workers:** Use the repository's subagent-dev and code-review workflow. Each task uses focused red/green checks before its review.

**Goal:** Create real or virtual projects with hierarchical display names and use the hierarchy in the FerroDeck roster.

**Architecture:** Keep the flat slug as the project's identity. Persist the parent slug with the workspace area. Build the roster from that relationship and keep existing flat registries readable.

**Tech stack:** Rust, FerroStore and redb, Clap, React, TypeScript, and Vitest.

**Spec:** [Issue #548 and its owner clarification](https://github.com/Artificial-Humanity/FerroLoop/issues/548).

## Constraints

- A name can have one to four trimmed, nonempty segments separated by `/`. Keep the existing display-name length limit and allow Unicode names. When no slug is supplied, use ASCII kebab-case segments and strip Latin combining accents after canonical decomposition. Refuse a name with untranslatable letters and explain how to supply `--slug`. Keep legacy slug derivation for unrelated paths.
- Slugs, store paths, wire addresses, routing, permissions, and agent membership still use the existing project identity. No inheritance. Old areas default to top level even if their names already contain a slash. The examples in #548 are test fixtures. Production project names and links require separate confirmation.
- Rename and reparent operations for linked nodes belong to #310. Keep the existing update operation for flat areas and allow idempotent repeats and non-name updates. Refuse a project delete if it has a child, before any reassignment, decommissioning, or filesystem deletion.
- All local product checks run through `scripts/test-sandboxed.sh`. Use task targets beneath `/data/toolchain/ferroloop-build`. Use an isolated workspace with synthetic data to demonstrate the owner workflow.

## Task 1 — Area records and compatibility

**Files:** `crates/core/src/agent.rs` and existing area fixtures; schema/version consumers in `crates/store/src`, including `state.rs`, `ferrostore/traits_state.rs`, `formats.rs`, `health.rs` and existing store tests.

**Contract:** `AreaRecord.parent: Option<String>` defaults to `None` on deserialization and is omitted on serialization when absent. Normalize a present parent through the existing slug validator and reject self-parenting. Keep flat registry event encoding byte-compatible. Only a write or import of a record carrying a parent raises the shared logical schema to the next format; raise it inside the same write transaction that stores the row and event. No eager rewrite. Existing import and event identity semantics remain in place. The next format protects old embedded clients that strictly decode area rows and events. Verify the version ladder and advertised ranges throughout both engines and distinguish unsupported-format fixtures from the existing area-delete fixtures.

- [ ] Add a fixture that decodes a legacy area and re-encodes its original fields, with no added null. Exercise local write and registry import into both stores, reopen and check the parent, schema and event. Distinguish a flat write from the first nested write. Exercise invalid parent slugs and self-parenting. Run the focused tests before and after the change.
- [ ] Implement the record and schema support and update existing struct literals explicitly. Keep parent-aware project mutation ordering out of the storage format change. Run the focused store and registry suites and commit.

## Task 2 — Project lifecycle and API

**Files:** `crates/hub/src/project_init.rs`, `project_list.rs`, `project_delete.rs`, `projects_api.rs`, `api.rs` and existing project fixtures; `crates/cli/src/cmd/project.rs` and CLI project tests.

**Contract:** Both request types use `create_parent: bool`, default false. CLI accepts the existing positional forms and the explicit #548 `--name` form for `--path` or `--virtual`. The owner-only POST body accepts optional `create_parent`. Match a canonical name prefix to an existing workspace area unambiguously, verifying its own parent chain. If missing and explicitly requested, preflight all missing prefixes and create virtual parents shallowest first. Reuse virtual-group checks for reserved, assigned, and store/config slugs. No auto-parent creation on ambiguity or a conflict discovered in preflight. If a write fails after a parent is saved, it is a valid empty group and retry can reuse it. Preserve existing bindings, partial-initialization recovery, store isolation and owner checks.

Serialize competing CLI and Hub project mutations using the existing in-process configuration lock and one shared on-disk advisory lock derived from the configuration location. Hold it from relationship validation through publication. Derive it consistently for create, update, and both delete paths. Acquire it after persona fences in fenced delete, return a retryable conflict on contention, and keep the descriptor private. Reuse existing safe open primitives where practical. Keep reads free of lock-file writes. Low-level sync can transiently deliver out-of-order ancestors: projections must keep the project visible as a root if its chain is unavailable or invalid, and never walk a cycle.

`ProjectSummary` and roster area payloads return `parent` and `ancestors` (ordered root-to-parent, empty for roots). The roster payload uses the same parent validation as the project list. Registry parent links are the authority for display. Provide full names in human and JSON project listings while keeping identifying fields stable.

- [ ] Write CLI/Hub tests for `Kaggle/Pokémon` → `kaggle-pokemon`, an explicit slug, four levels, invalid segment and excessive depth, a name needing an explicit slug, missing/ambiguous parent, optional ancestors, repeated creation, and an existing legacy slash name. Ensure a refusal leaves target data untouched. Add parent-delete and linked-rename refusal, old API compatibility, and contention coverage across CLI and Hub. Run the focused tests before implementing.
- [ ] Implement shared resolution and lifecycle handling. Check that a nested name keeps its flat slug in CLI, summary and roster, and that malformed topology cannot hang readers or hide agents. Run the project-init, project-delete and CLI project suites before committing.

## Task 3 — FerroDeck create and browse

**Files:** `ui/src/types/api.ts`, `ui/src/api/client.ts`, `ui/src/components/RosterSidebar.tsx`, `ui/src/components/settings/NewProjectModal.tsx`, scoped layout CSS if necessary, and the existing project, modal and roster tests.

**Contract:** Build the tree from the explicit API links, with compatible behavior for older hubs that omit them. Render direct agents before children, use the last segment on linked rows and full names for accessible labels and tooltips. Each node has its own collapse control and remembered state. Collapsing a parent puts badge totals and highest severity for hidden subtree agents on its row. A persona projected through extra areas counts once per rolled-up badge. Parent pins keep the subtree together and child pins sort within their siblings. Keep virtual markers and existing saved flat pins/collapse preferences. Keep hierarchy visible for matched descendants even when an ancestor has no matching agents. Missing or cyclic metadata never hides groups. Use a small indent at phone width and prevent horizontal overflow. The existing creation dialog accepts hierarchical names, previews the correct slug, allows explicitly creating missing parents, and still surfaces server conflicts without mutating scope.

- [ ] Add red tests for ordered nesting, parent and child toggles independently, persistence after remount, unread/question/important roll-up, deduplication, parent/child pin behavior, search, older payloads, and the creation request. Include a browser check at phone width.
- [ ] Implement using existing project, pin and roster state ownership. Run the focused UI suite, typecheck and build in the sandbox; commit.

## Release verification

- [ ] Review each task and finally the entire branch. Run the repository verification trio and UI checks. Exercise create/list/roster with a disposable lab-style workspace and verify that a flat workspace does not acquire hierarchy automatically.
- [ ] Document the supported rollout and rollback boundary. Upgrade store readers and registry synchronizers before the first nested write. A topology-bearing store is not readable by an older embedded build. The owner confirms actual project names and parent relationships before any live creation or migration.
