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

`fl state import-workspace` (§6.1) and the owner-only Hub endpoint (§6.3) share one implementation in `fl-core` (`crates/core/src/ferrostate.rs`: `scan_workspace_import` reads the tree once, `apply_workspace_import` writes it; `resolve_workspace_root` finds the first ancestor holding `.agents/memory/` or `Notes/`). It is a one-way, read-only import; source files are never modified or moved.

* **`.agents/memory/*.md` → `Memory`** (top level only, sorted; nested files are ignored). `MEMORY.md` is never imported itself: a memory is `pinned = true` iff its exact filename occurs in `MEMORY.md`'s text. Slug is derived deterministically from the filename stem: lowercase, fold runs of invalid characters to `-`, keep `[a-z0-9_-]`, trim leading/trailing `-_`, prefix `m-` when the result would not start with a letter, truncate to 64 bytes and re-trim; a stem with nothing usable is skipped. YAML frontmatter is hand-parsed (first line `---`, closed by `---`/`...`; `name`/`description`/`title`/`type` at any indentation, e.g. `type` nested under `metadata:`, surrounding quotes stripped — no YAML dependency). `summary` is frontmatter `description`, else `title`, else the first Markdown heading, else `name`, else the slug — collapsed to one line, truncated to 240 characters, never empty. `body` is the text after frontmatter (the whole file when there is none). Frontmatter `type` maps per §8; `lifecycle = active`, `superseded_by = None`, scope `workspace`.
* **`Notes/*/STATE.md` → `Handoff`** (immediate subdirectories only, sorted). One entry per `Notes/<dir>/` at slug `project-state`, scope `project:<dir-slug>` with the same slug derivation as filenames (e.g. `ArtificialHumanity.io` → `artificialhumanity-io`). Markdown bullets (`-`, `*`, `+` at any indent; indented continuation lines join the previous bullet with spaces) are routed by their heading, matched lowercase: contains `shipped`/`completed`/`done`/`accepted` → `shipped`; `open work`/`next`/`todo`/`what to do next` → `next_steps`; `blocker`/`blocked` → `blockers`; `current`/`resume`/`handoff`/`in flight`/`in progress` → `in_flight`. Bullets and paragraphs under any other heading are preserved into `notes` with their heading context, so no prose is dropped (`notes` is truncated char-safely to `MAX_NOTES_BYTES`). `summary` is the first substantive bullet or paragraph (collapsed, ≤ 240 characters), else the cleaned H1, else `Imported project state for <project-slug>`.
* **Scope/idempotency/sensitivity.** Memories always land at scope `workspace`; handoffs at their `project:<dir-slug>`. Writes go through `upsert_state` at the deterministic `(scope, concern, slug)` key, so re-import updates the same entry instead of duplicating it. Imported entries take `SensitivityPatch::Inherit` (§7) — import never sets or clears sensitivity.
* **Reporting.** The shared report is `{ "imported_memories": n, "imported_handoffs": n, "scopes": ["<wire scope>", ...], "skipped": ["<path>: <reason>", ...] }` (`scopes` sorted and unique). The CLI prints it in full with `--json` and as counts/scopes (plus any `skipped` lines) otherwise; the Hub response carries exactly the first three fields (§6.3). A per-file read/UTF-8/oversize failure, or a per-upsert store failure, lands in `skipped` and never aborts the run.

---

## 4. Session Checkpoints

A checkpoint is how a session ends so the next one can resume without rereading the workspace. It is captured **automatically from git** and introduces no new type. The shared core lives in `crates/core/src/ferrostate.rs` (`probe_git_repo`, `create_state_checkpoint`), so the CLI, MCP, and Hub surfaces (§6) never duplicate the git probing:

* A checkpoint **is** a `Handoff` entry at `(scope, handoff, <slug>)`, distinct from the canonical `current`, so checkpointing never overwrites the published handoff. An explicit slug must pass `validate_slug`; the default slug is `checkpoint-<by>` derived to a valid slug.
* The probe of a repo path resolves the git top-level (`git rev-parse --show-toplevel`), branch (`git branch --show-current`; empty/detached → `detached`), HEAD SHA (`rev-parse HEAD`), latest commit subject (`log -1 --format=%s`), and dirty files (`git status --porcelain`). A non-repository, a repository with no commits, or any git failure is a clear error — never a bogus checkpoint.
* The write is one `Handoff` upsert through `StateStore::upsert_state` with `SensitivityPatch::Inherit`. `shipped` gets the HEAD commit line (`<head_sha> <subject>`), `in_flight` gets the dirty `XY path` entries (capped at 200, with the full count preserved in the notes and the report), and `notes` records `branch`/`head_sha`/`dirty_files`/latest subject/repo top-level lines. An explicit summary is sanitized to one line of at most 240 characters; the default is `Checkpoint on <branch> @ <short-head> (<n> dirty): <subject>`. `updated_at`/`by` stamp who and when.
* Every surface returns the same serializable report: `{ "entry": <StateEntry>, "branch": string, "head_sha": string, "dirty_files": n }`. There is **no** checkpoint show/get command: resume reads the checkpoint like any other `Handoff` (`fl state get`, `state_get`, `GET /api/v1/state`), then §2.5 recall for the task terms. Promotion to the canonical handoff is an ordinary `fl state handoff set`; no promotion command is added. Checkpoints revise, sync, conflict, and redact like any other `Handoff` (Phase 1 §3.3, §7).

---

## 5. Structural Code Indexer and Token-Savings Ledger (retiring XERJ)

XERJ answered "what does this file declare?" without the file's full token cost. Phase 2 absorbs that job and XERJ is retired: no XERJ binary, config, or output files are read or required.

### 5.1 Live structural code scan (`crates/core/src/code_index.rs`)

* `scan_code_index(root)` walks a workspace tree (default: current directory or repository root), skipping `.git/`, `target/`, `node_modules/`, `dist/`, `.next/`, files larger than `1 MiB` (`MAX_CODE_FILE_BYTES`), and non-UTF-8 files, indexing `.rs`, `.ts`, `.tsx`, `.js`, `.py`, `.toml`, and `.md` files in deterministic relative-path order.
* Extraction is line-oriented with no external parser crate: declarations (functions, types, traits, modules, headings, and TOML sections) become `CodeSymbolHit` items (`name`, `kind`, `path`, `start_line`, `end_line`, `signature`, `snippet`, `estimated_file_tokens`, `snippet_tokens`, `tokens_saved`), with raw line matches as the fallback tier for `CodeSearchKind::Search`.
* Three query kinds (`CodeSearchKind`: `def` | `search` | `outline`) run directly against `CodeIndex::search(kind, q, limit)`.

### 5.2 Token-savings ledger (`ferrostate-token-gain`)

* Cumulative savings are persisted in a single workspace `Runtime` entry at `(workspace, runtime, ferrostate-token-gain)` (`TOKEN_GAIN_SLUG`, `merge_mode = last_writer`) holding `TokenGainSummary`: `{ "queries_count": n, "tokens_saved": n, "files_indexed": n, "symbols_indexed": n }`.
* For each query, `tokens_saved_this_query` is computed per distinct file as `max(0, file_tokens - sum(snippet_tokens of returned hits in that file))`, summed across files so multiple hits in the same file never double-count that file's tokens. `record_token_gain_query` increments `queries_count` and adds `tokens_saved_this_query`; `record_token_gain_index` updates `files_indexed` and `symbols_indexed`. Hub exposure: §6.3.

---

## 6. Surfaces

### 6.1 CLI (`fl state`, `crates/cli/src/cmd/state.rs`)

Existing subcommands are unchanged. Phase 2 adds:

* `fl state search <query> [--scope <scope>] [--concern <concern>] [--limit <n>=10] [--json]` — `list_state` → `search_state_entries` (default limit 10, capped at 100). `--json`: `{ "query": ..., "hits": [<StateSearchHit>, ...], "total_hits": n }`; text: one tab-separated `<score_millis>\t<scope>\t<concern>\t<slug>\ttokens=<n>\t<snippet>` line per hit.
* `fl state import-workspace [--workspace-root <path>] [--by <author>=fl] [--json]` — §3; the root is the explicit `--workspace-root`, else the first ancestor of the cwd holding `.agents/memory/` or `Notes/`. The scan is grouped by scope and each scope is written through the multi-store routing (`open_for_scope`). `--json` prints the full §3 report including `skipped`; text prints counts and scopes plus any skipped lines.
* `fl state checkpoint [--scope <scope>] [--slug <slug>] [--summary <text>] [--by <author>=fl] [--json]` — §4: probes the git repository at the cwd and records it as a `Handoff` checkpoint. The default scope is the bound project scope for the cwd when configured, else `workspace`. `--json` prints the §4 report `{ "entry": <StateEntry>, "branch": ..., "head_sha": ..., "dirty_files": n }`; text prints one concise line naming scope/slug/branch/head/dirty. There is no checkpoint show command.
* `fl state index [--root <path>] [--json]` — scans the workspace tree and records `files_indexed`/`symbols_indexed` in `ferrostate-token-gain`.
* `fl state code-def <symbol> [--root <path>] [--limit <n>=50] [--json]`, `fl state code-search <query> [--root <path>] [--limit <n>=50] [--json]`, and `fl state code-outline <file_path> [--root <path>] [--limit <n>=500] [--json]` — §5 structural code queries; `--json`: `{ "query": ..., "kind": "def"|"search"|"outline", "symbols": [<CodeSymbolHit>, ...], "tokens_saved_this_query": n, "token_gain": <TokenGainSummary> }`.
* `fl state install [--user] [--json]` — registers the `ferrostate` MCP server (`fl state mcp`) across vendor CLI configurations.

### 6.2 MCP tools (`fl state mcp`, `crates/cli/src/cmd/state_mcp.rs`)

Tools join the existing stdio JSON-RPC 2.0 server (results keep the single JSON text block convention).

* `fl_memory_search` — `{ "query": string, "scope"?: string, "concern"?: "memory"|"handoff"|"runtime", "limit"?: number }` → `{ "query": ..., "hits": [<StateSearchHit>, ...], "total_hits": n }`.
* `fl_state_checkpoint` — `{ "scope"?: string, "slug"?: string, "summary"?: string, "by"?: string = "fl" }` → the §4 report `{ "entry": <StateEntry>, "branch": ..., "head_sha": ..., "dirty_files": n }` as pretty JSON. The repository is the server's cwd; the default scope matches the CLI (bound project scope for the cwd when configured, else `workspace`). Wrong argument types or an invalid scope/slug are `isError` results. There is no checkpoint get tool; use `state_get`.
* `fl_code_def`, `fl_code_search`, `fl_code_outline` — `{ "q": string, "root"?: string, "limit"?: number }` → `{ "query": ..., "kind": "def"|"search"|"outline", "symbols": [<CodeSymbolHit>, ...], "tokens_saved_this_query": n, "token_gain": <TokenGainSummary> }`.
* `fl_batch_search` — `{ "queries": [{ "kind": "def"|"search"|"outline", "q": string, "limit"?: number }, ...], "root"?: string }` (up to 20 subqueries) → `{ "results": [...], "token_gain": <TokenGainSummary> }`.

### 6.3 Hub REST (`crates/hub/src/api.rs`)

* `GET /api/v1/state/search?q=<query>[&scope=<scope>][&concern=<concern>][&limit=<n>]` → `200 { "query": ..., "hits": [<StateSearchHit>, ...], "total_hits": n }`. Bad `scope`/`concern`/`limit` → `400 { "error": ... }`, as in `handle_get_state`.
* `GET /api/v1/state/code-search?q=<query>[&kind=def|search|outline][&limit=<n>]` → `200 { "query": ..., "kind": ..., "symbols": [<CodeSymbolHit>, ...], "tokens_saved_this_query": n, "token_gain": <TokenGainSummary> }`.
* `POST /api/v1/state/checkpoint`, body empty or `{ "scope"?: string, "slug"?: string, "summary"?: string, "by"?: string }` (non-object body or wrong-typed fields → `400`) → `200` with exactly the §4 report `{ "entry": <StateEntry>, "branch": ..., "head_sha": ..., "dirty_files": n }`. The repository is the Hub's configured repo root (git errors → `500`); the default scope is `project:<git-top-level basename slug>` when derivable, else `workspace`; `by` defaults to the caller's actor name. There is no `GET` checkpoint route; use `GET /api/v1/state`.
* `GET /api/v1/stats` gains one additive field from `ferrostate-token-gain` (§5.2), existing fields unchanged: `{ ..., "token_gain": { "queries_count": n, "tokens_saved": n, "files_indexed": n, "symbols_indexed": n } }` (present and all-zero when no ledger entry exists). Existing `GET`/`POST /api/v1/state` are unchanged.
* `POST /api/v1/state/import-workspace`, **owner-only** (non-owner → `403`), body empty or `{ "workspace_root"?: string, "by"?: string }` (non-object body or wrong-typed fields → `400`) → `200 { "imported_memories": n, "imported_handoffs": n, "scopes": ["<wire scope>", ...] }` exactly (no `skipped` field). The root is the body's `workspace_root`, else resolved from the Hub's configured repo root; `by` defaults to the caller's actor name. All entries go to the Hub store (§3).

---

## 7. Sensitivity, Redaction, and Owner-Only Rules

No Phase 1 semantics change: `sensitive` stays sticky (`SensitivityPatch`), `ever_sensitive` keeps whole revision streams off non-private remotes, clearing never erases history. New Hub endpoints mirror `handle_get_state` / `handle_post_state` / `redact_sensitive_entry` (`crates/hub/src/api.rs`) exactly:

* **Reads** (search; checkpoints are read through the ordinary state reads): workspace `hub-pairing-`/`hub-session-` slugs are filtered out; owners see full entries; non-owners get sensitive entries through `redact_sensitive_entry` (`Memory` summary+body, `Handoff` summary+notes → `[REDACTED]` with `shipped`/`in_flight`/`blockers`/`next_steps` cleared, and `Runtime` value → `[REDACTED]`). Redaction also covers the search `snippet` (`[REDACTED]`), since it derives from the body; `score_millis`/`matched_terms`/`estimated_tokens` remain, as redacted entries themselves do today.
* **Writes** (checkpoint post): reserved `hub-pairing-`/`hub-session-` slug prefixes → `403`; non-owners cannot modify an existing checkpoint entry with `sensitive`/`ever_sensitive` set (`403`). Checkpoint writes always use `SensitivityPatch::Inherit`, so they never set or clear sensitivity.
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

