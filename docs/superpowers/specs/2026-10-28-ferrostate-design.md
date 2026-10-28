# Sub-project 7: `FerroState` Phase 2 — Bulletproof Memory, Context & Structural Code Index (`crates/core` search / `fl state`)

* **Date:** 2026-10-28
* **Author:** Felicia (`felicia@artificialhumanity.io`)
* **Status:** Approved for implementation (Phase 2 build on `felicia/ferrostate-phase2`)
* **Target Crates & Surfaces:**
  * `crates/core` (`fl-core`: BM25 search + token estimate in `crates/core/src/state.rs`)
  * `crates/cli` (`fl state` subcommand family + `fl state mcp` stdio MCP server)
  * `crates/hub` (`/api/v1/state/*` REST + `token_gain` on `/api/v1/stats`)
* **Builds on:** `2026-10-22-state-database-design.md` (Phase 1: scopes, three concerns,
  revision DAG, `fl/state` sync, sensitivity). This phase adds no new concern, no new
  store format, and no new `Kind`.

---

## 1. Purpose and Scope

Phase 1 made state durable. Phase 2 makes it *recallable under a token budget*, imports the legacy Markdown memory that never entered the store, and lets sessions end with a resumable checkpoint. It also retires the external XERJ code-outline tool by folding a structural code index into the same recall path, with a ledger that proves the tokens saved. Phase 2 has exactly four capabilities, exposed through §6:

1. **BM25 ranked search + token-budgeted recall** over `StateEntry`s (§2, `fl-core`).
2. **Workspace importer** for `.agents/memory/` and `Notes/*/STATE.md` (§3) — the bulk importer Phase 1 §0.2 deferred.
3. **Session checkpoints** as named `Handoff` entries (§4). No new concern.
4. **Structural code indexer + token-savings ledger** retiring XERJ (§5).

Sensitivity and owner-only rules are unchanged (§7); one importer deviation is in §8. Non-goals: no embeddings, vector store, or external search service (ranking is lexical BM25 in `fl-core`, deterministic and offline); no new on-disk format (importer, checkpoints, code index, and ledger are ordinary `StateEntry`s written through `StateStore::upsert_state`, so they sync over `fl/state` like any other entry); no full-source indexing (signatures and headings only, §5).

---

## 2. BM25 Ranked Search and Token-Budgeted Recall (`crates/core/src/state.rs`)

### 2.1 Exact core shapes

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSearchHit {
    pub entry: StateEntry,
    pub score_millis: u64,
    pub matched_terms: Vec<String>,
    pub snippet: String,
    pub estimated_tokens: u64,
}

pub fn search_state_entries(
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> Vec<StateSearchHit>;

/// ceil(chars / 4); empty text is 0. Reused by recall budgets and the
/// §5 savings ledger.
pub fn estimate_tokens(text: &str) -> u64;
```

All three are re-exported from `crates/core/src/lib.rs`. On the wire a hit is the serde form of the struct: `{ "entry": <StateEntry>, "score_millis": 1234, "matched_terms": ["bm25"], "snippet": "...", "estimated_tokens": 42 }`.

### 2.2 Tokenization and searchable fields

* **Tokenizer:** lowercase ASCII alphanumeric runs (everything outside `[A-Za-z0-9]` separates). Query terms are deduplicated, so a repeated term never double-counts.
* **Fields and weights** per entry (`slug` is `StateEntry.slug`):

  | field | weight | `Memory` | `Handoff` | `Runtime` |
  |---|---|---|---|---|
  | slug | 3 | slug | slug | slug |
  | summary | 2 | `summary` | `summary` | — |
  | body | 1 | `body` | `notes` + `shipped` + `in_flight` + `blockers` + `next_steps` | serialized JSON `value` |

* The *unweighted searchable text* is slug, summary, and body joined with `\n` (empty parts skipped). It drives `snippet` and `estimated_tokens` only.

### 2.3 Scoring

* Real BM25 with `k1 = 1.2`, `b = 0.75`, field-weighted term frequencies (`f = 3·slug + 2·summary + 1·body` counts) and weighted document length. `IDF(t) = ln(1 + (N − df + 0.5) / (df + 0.5))` over the provided entry set (after the §2.4 filters), so IDF is always positive.
* **Boosts (multiplicative):** pinned `Memory` entries ×1.5; non-active `Memory` entries (`retired` or `superseded`) ×0.25.
* `score_millis = (score * 1000.0) as u64` (saturating cast). `matched_terms` is the sorted unique query terms present in the entry. `snippet` is a ~200-char window of the unweighted searchable text around the first (earliest) matched-term occurrence, falling back to the summary (or the start of the searchable text for `Runtime`). `estimated_tokens = estimate_tokens(searchable_text)`.

### 2.4 Filtering, ordering, and degenerate inputs

* When `scope` or `concern` is `Some`, only matching entries are searched. Empty/blank query (no ASCII alphanumeric tokens) or `limit == 0` returns an empty `Vec`; only entries with `score > 0` are returned.
* Sort: `score_millis` desc, then `updated_at` desc (via `At::unix_millis`), then `slug` asc, capped at `limit`. The function is pure (no IO/clock/store); callers fetch entries (e.g. `StateStore::list_state`) and pass the slice in.

### 2.5 Token-budgeted recall

Core search is budget-agnostic (`limit` only). A *recall* is the surface rule (§6) that keeps the longest prefix of hits, in rank order, whose cumulative `estimated_tokens` fits the caller's budget. Entries are never split and rank order is never changed to fit a smaller entry.

---

## 3. Workspace Importer (`.agents/memory/` + `Notes/*/STATE.md`)

`fl state import` (§6.1) is a one-way, read-only import; source files are never modified or moved.

* **`.agents/memory/**/*.md` → `Memory`.** Slug is the file stem via `validate_slug` (un-normalizable stems are skipped, not fatal). `summary` is frontmatter `description` (else `title`, else first heading, else slug), via `validate_summary` (single line, ≤ 240 chars; over-long summaries truncate to the limit and are noted in the report). `body` is the content after frontmatter. Frontmatter `type` maps per §8; `lifecycle = active`, `pinned = false`, `superseded_by = None`.
* **`Notes/*/STATE.md` → `Handoff`.** One entry per `Notes/<dir>/` at slug `current`, scope `project:<dir-slug>` unless `--scope` overrides. `summary` is the first heading (else `Imported STATE.md for <dir>`); full file text goes to `notes`; list fields start empty. No Markdown-section parsing: import preserves text, the next `handoff set`/checkpoint structures it.
* **Scope/idempotency/sensitivity.** `--scope` defaults to `workspace` for `.agents/memory/` and, when given, applies to both sources. Writes go through `upsert_state` at the deterministic `(scope, concern, slug)` key, so re-import updates the same entry. Entries are never sensitive unless `--sensitive` applies `SensitivityPatch::SetSensitive` to the whole run (§7).
* **Reporting.** `{ "imported": n, "updated": n, "skipped": [{ "path": "...", "reason": "..." }] }` (with `--json`; text otherwise). A per-file failure lands in `skipped` and never aborts the run. `--dry-run` reports without writing.

---

## 4. Session Checkpoints

A checkpoint is how a session ends so the next one can resume without rereading the workspace. It introduces no new type:

* A checkpoint **is** a `Handoff` entry at `(scope, handoff, <key>)`, `<key>` defaulting to `checkpoint` — distinct from the canonical `current`, so mid-task checkpointing never overwrites the published handoff. `--key <slug>` allows named checkpoints (e.g. `checkpoint-api`).
* `fl state checkpoint save` (§6.1) wraps the existing handoff write path (`HandoffEntry` via `UpsertStateArgs`): required `--summary`, repeatable `--shipped`/`--in-flight`/`--blockers`/`--next-steps`, `--notes`/`--notes-file`. Record branch, worktree, and in-flight subagents in `in_flight`/`notes`; `updated_at`/`by` stamp who and when.
* Resume is `checkpoint show`, then §2.5 recall for the task terms. Promotion to the canonical handoff is an ordinary `fl state handoff set`; no promotion command is added. Checkpoints revise, sync, conflict, and redact like any other `Handoff` (Phase 1 §3.3, §7).

---

## 5. Structural Code Indexer and Token-Savings Ledger (retiring XERJ)

XERJ answered "what does this file declare?" without the file's full token cost. Phase 2 absorbs that job and XERJ is retired: no XERJ binary, config, or output files are read or required.

### 5.1 Index entries

* `fl state code-index` (§6.1) walks a root (default: repository root), skipping `.git/`, `target/`, and non-UTF-8 files. Each indexed file becomes one `Runtime` entry at scope `project:<root-slug>` (or `--scope`), slug `code-<path-slug>-<sha256(path)[:8]>` (path-slug: relative path ASCII-lowercased, non-`[a-z0-9]` runs folded to `-`, truncated to pass `validate_slug`), `merge_mode = last_writer`, value `{ "path": "crates/core/src/state.rs", "language": "rust", "symbols": [{ "kind": "fn", "name": "search_state_entries", "line": 640, "signature": "pub fn search_state_entries(...)" }], "full_tokens": n, "outline_tokens": n }`.
* Extraction is line-oriented, no parser crate: Rust contributes `fn`/`struct`/`enum`/`trait`/`impl`/`mod` declaration lines (never bodies); Markdown contributes `heading` symbols; other UTF-8 files get a path-only stub. Rebuild is idempotent upsert at the deterministic slug; entries for deleted files are left in place (pruning is out of scope).
* Index entries are `Runtime`, so §2 search already covers them (serialized JSON is the weight-1 body). Code recall is `fl state search --concern runtime`; no separate code search exists.

### 5.2 Token-savings ledger

* Each `code-index` run appends one record per file to a `Runtime` entry at the same scope, slug `token-savings-ledger`, `merge_mode = append`: `{ "path": "...", "at": "<At>", "full_tokens": n, "outline_tokens": n, "saved_tokens": n }`, where `full_tokens = estimate_tokens(file_text)`, `outline_tokens = estimate_tokens(serialized_index_value)`, `saved_tokens = full_tokens.saturating_sub(outline_tokens)`. The ledger measures *avoided* tokens with the one shared estimator, so §2 budgets and §5 savings never disagree. Hub exposure: §6.3.

---

## 6. Surfaces

### 6.1 CLI (`fl state`, `crates/cli/src/cmd/state.rs`)

Existing subcommands are unchanged. Phase 2 adds:

* `fl state search <query> [--scope <scope>] [--concern <concern>] [--limit <n>=20] [--budget-tokens <n>] [--json]` — `list_state` → `search_state_entries` → §2.5 prefix rule when budgeted. `--json`: `{ "hits": [<StateSearchHit>, ...] }`; text: one `<slug>  <score_millis>  <snippet>` line per hit.
* `fl state import [--scope <scope>] [--sensitive] [--dry-run] [--json] [<path>...]` — §3; no paths means `.agents/memory/` + `Notes/*/STATE.md` under the workspace root.
* `fl state checkpoint save --scope <scope> --summary <text> --by <author> [--key <slug>=checkpoint] [--shipped <text>...] [--in-flight <text>...] [--blockers <text>...] [--next-steps <text>...] [--notes <text> | --notes-file <path>] [--sensitive | --not-sensitive] [--json]` — §4; `--json`: `{ "entry": <StateEntry> }`.
* `fl state checkpoint show --scope <scope> [--key <slug>=checkpoint] [--json]` — the checkpoint entry (`{ "entry": <StateEntry> }` with `--json`).
* `fl state code-index [--root <path>] [--scope <scope>] [--by <author>=fl] [--json]` — §5; `--json`: `{ "indexed": n, "skipped": n, "full_tokens": n, "outline_tokens": n, "saved_tokens": n }`.

### 6.2 MCP tools (`fl state mcp`, `crates/cli/src/cmd/state_mcp.rs`)

Three tools join the existing stdio JSON-RPC 2.0 server (results keep the single JSON text block convention). Filesystem work (`import`, `code-index` rebuild) stays CLI-only; MCP callers recall the built index via `state_search`.

* `state_search` — `{ "query": string, "scope"?: string, "concern"?: "memory"|"handoff"|"runtime", "limit"?: number = 20, "budget_tokens"?: number }` → `{ "hits": [<StateSearchHit>, ...] }` (§2.5 rule when budgeted).
* `state_checkpoint_save` — `{ "scope": string, "by": string, "summary": string, "key"?: string = "checkpoint", "shipped"?, "in_flight"?, "blockers"?, "next_steps"?: string[], "notes"?: string, "sensitive"?: bool, "clear_sensitive"?: bool }` → `{ "entry": <StateEntry> }`.
* `state_checkpoint_get` — `{ "scope": string, "key"?: string }` → `{ "entry": <StateEntry> }`, or an `isError` result when absent.

### 6.3 Hub REST (`crates/hub/src/api.rs`)

* `GET /api/v1/state/search?q=<query>[&scope=<scope>][&concern=<concern>][&limit=<n>][&budget_tokens=<n>]` → `200 { "hits": [<StateSearchHit>, ...] }`. `q` required; bad `scope`/`concern` → `400 { "error": ... }`, as in `handle_get_state`.
* `POST /api/v1/state/checkpoint`, body `{ "scope": string, "key"?: string, "summary": string, "shipped"?, "in_flight"?, "blockers"?, "next_steps"?: string[], "notes"?: string, "sensitive"?: bool, "by"?: string }` → `200 { "entry": <StateEntry> }` (§4).
* `GET /api/v1/state/checkpoint?scope=<scope>[&key=<slug>]` → `200 { "entry": <StateEntry> }`, else `404 { "error": ... }`.
* `GET /api/v1/stats` gains one additive field from all `token-savings-ledger` entries (§5.2), existing fields unchanged: `{ ..., "token_gain": { "full_tokens": n, "outline_tokens": n, "saved_tokens": n, "records": n } }` (saturating sums; present and all-zero with no ledger). Existing `GET`/`POST /api/v1/state` are unchanged.

---

## 7. Sensitivity, Redaction, and Owner-Only Rules

No Phase 1 semantics change: `sensitive` stays sticky (`SensitivityPatch`), `ever_sensitive` keeps whole revision streams off non-private remotes, clearing never erases history. New Hub endpoints mirror `handle_get_state` / `handle_post_state` / `redact_sensitive_entry` (`crates/hub/src/api.rs`) exactly:

* **Reads** (search, checkpoint get): workspace `hub-pairing-`/`hub-session-` slugs are filtered out; owners see full entries; non-owners get sensitive entries through `redact_sensitive_entry` (`Memory` summary+body, `Handoff` summary+notes — list fields as the handler leaves them — and `Runtime` value → `[REDACTED]`). Redaction also covers the search `snippet` (`[REDACTED]`), since it derives from the body; `score_millis`/`matched_terms`/`estimated_tokens` remain, as redacted entries themselves do today.
* **Writes** (checkpoint post): reserved slug prefixes → `403`; `sensitive: true` requires owner (`403`); non-owners cannot modify an entry with `sensitive`/`ever_sensitive` set (`403`).
* **CLI/MCP are local-trust** and return full entries; `fl state render` still excludes sensitive entries without `--include-sensitive`. Code-index entries hold only paths/signatures/headings, are written non-sensitive, and the indexer never reads state entries.

---

## 8. Known Deviation: Importer Category Mapping

Frontmatter `type` (`feedback`, `user`, `project`, `reference`) has no one-to-one core counterpart, and this phase does **not** extend the closed enums: `MemoryCategory` is `Warning | Rule | Decision | Note`, `MemoryBasis` is `Ruling | Measured | Inference` — no `Convention`/`Architecture`/`OwnerStated` variants exist or are added. The importer (§3) maps onto the existing variants:

| frontmatter `type` | `MemoryCategory` | `MemoryBasis` |
|---|---|---|
| `feedback` | `Rule` | `Ruling` |
| `user` | `Rule` | `Ruling` |
| `project` | `Decision` | `Ruling` |
| `reference` | `Note` | `Ruling` |
| missing or any other value | `Note` | `Ruling` |

Accepted consequences: the importer never produces `Warning`, `Measured`, or `Inference`; `feedback` guidance and `user` preferences both land as owner `Rule`s (`Ruling`), since both record owner-stated instructions. Tightening this needs a future enum change, out of scope here.

---

## 9. Acceptance

* `cargo test -p fl-core` covers §2: body relevance over irrelevant entries, pinned ×1.5, slug over body-only match, scope+concern filters, empty query/`limit == 0`, `matched_terms`/`snippet`/`estimated_tokens`, retired deprioritization vs identical active, and §2.4 tie-breaks. Ordering tests place the winner last in the input slice, so an unscored input-order implementation cannot pass.
* Later Phase 2 tasks land each §6 surface against these shapes with tests, `cargo fmt --all --check`, and `cargo clippy -- -D warnings` green.

