use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use fl_core::agent::{AgentRecord, AgentStatus, AreaRecord, reports_to};
use fl_core::code_index::{
    CodeIndex, CodeSearchKind, CodeSymbolHit, TokenGainSummary, load_token_gain,
    record_token_gain_index, record_token_gain_query, scan_code_index,
};
use fl_core::ferrostate::{
    WorkspaceImportReport, apply_workspace_import, create_state_checkpoint, resolve_workspace_root,
    scan_workspace_import,
};
use fl_core::ids::{Kind, RevisionId, StateId};
use fl_core::iri::Iri;
use fl_core::ref_index::{
    RefIndexPlan, RefIndexReport, RefIndexStatus, RefKind, RefSource, index_local_source,
};
use fl_core::state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateEntry, StatePayload, StateScope,
    StateSearchHit, UpsertStateArgs, validate_slug,
};
use fl_core::store::{AgentRegistry, Handles, StateStore};
use fl_exec::state_sync::{sync_state, verify_state};
use fl_store::{
    AnyStore, CodeSession, DEFAULT_CODE_SYNC_BUDGET, IndexPolicy, RankMode, RankedCode, RankedHits,
    SearchMode,
};

use crate::cmd::agent::{now_at, write_atomic_non_symlink};
use crate::config::Config;
use crate::refs::Ref;

/// Default byte ceiling for `fl state render` (24.4 KB, matching workspace index cap).
pub const DEFAULT_RENDER_MAX_BYTES: usize = 24_400;

/// Default hit count for state search surfaces (CLI, MCP, Hub).
pub const DEFAULT_SEARCH_LIMIT: usize = 10;

/// Maximum hit count for state search surfaces (CLI, MCP, Hub).
pub const MAX_SEARCH_LIMIT: usize = 100;

/// Resolve a caller-supplied search limit: default 10, capped at 100.
pub fn effective_search_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_SEARCH_LIMIT).min(MAX_SEARCH_LIMIT)
}

/// Rank `entries` with the core BM25 searcher and truncate at the surface,
/// returning the full ranked count before truncation as the second value.
/// Stores are local, so fetching the full ranked list is cheap. The surfaces
/// rank through `fl_store::rank_state_entries`; this BM25-only form needs no
/// store.
#[cfg(test)]
pub fn collect_search_hits(
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> (Vec<StateSearchHit>, usize) {
    let ranked = fl_store::rank_bm25(entries, query, scope, concern, limit);
    (ranked.hits, ranked.total_hits)
}

/// Wire shape shared by the CLI, MCP, and Hub search surfaces.
pub fn search_result_json(
    query: &str,
    hits: &[StateSearchHit],
    total_hits: usize,
) -> serde_json::Value {
    serde_json::json!({
        "query": query,
        "hits": hits,
        "total_hits": total_hits,
    })
}

/// [`search_result_json`] for a ranked result. A BM25 result with no note has
/// the exact shape above; `mode` is added only when the knowledge ranker
/// answered, and `note` only when it failed over to BM25.
pub fn ranked_result_json(query: &str, ranked: &RankedHits) -> serde_json::Value {
    let mut out = search_result_json(query, &ranked.hits, ranked.total_hits);
    if let Some(map) = out.as_object_mut() {
        if ranked.mode == SearchMode::Knowledge {
            map.insert("mode".into(), ranked.mode.as_str().into());
        }
        if let Some(note) = &ranked.note {
            map.insert("note".into(), note.clone().into());
        }
    }
    out
}

/// [`ranked_result_json`] for a search that named its [`RankMode`]. `Auto`
/// keeps the shape above; any other mode adds `"mode": "<mode>"`, the mode
/// that was requested and ran (an explicit `vector` or `hybrid` request never
/// falls back to BM25). A `vector` or `hybrid` result whose index was still
/// being built adds `"partial": true` and `"pending_index": <entries>`.
pub fn ranked_result_json_for(
    query: &str,
    ranked: &RankedHits,
    mode: RankMode,
) -> serde_json::Value {
    let mut out = ranked_result_json(query, ranked);
    if let Some(map) = out.as_object_mut() {
        if mode != RankMode::Auto {
            map.insert("mode".into(), mode.as_str().into());
        }
        if ranked.pending_index > 0 {
            map.insert("partial".into(), true.into());
            map.insert("pending_index".into(), ranked.pending_index.into());
        }
    }
    out
}

/// The stderr line for a partial `vector` or `hybrid` result, or `None`.
pub fn partial_index_line(ranked: &RankedHits) -> Option<String> {
    (ranked.pending_index > 0).then(|| {
        format!(
            "fl: {} entries not yet indexed; results may be partial, re-run",
            ranked.pending_index
        )
    })
}

/// Rank `entries` for `mode` with the deterministic local embedder, indexing
/// up to the default sync budget first. `Vector` and `Hybrid` fail on a store
/// without a vector index (redb) instead of answering with BM25.
pub fn rank_for_mode(
    store: &AnyStore,
    mode: RankMode,
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> Result<RankedHits, String> {
    fl_store::rank_state_entries_mode(
        store,
        fl_store::default_embedder(),
        IndexPolicy::Sync {
            budget: fl_store::DEFAULT_SYNC_BUDGET,
        },
        mode,
        entries,
        query,
        scope,
        concern,
        limit,
    )
    .map_err(|e| e.to_string())
}

/// The stderr line that tells an operator why a search fell back to BM25, or
/// `None` when it did not. It carries the engine's own error, which is kept out
/// of the JSON output on purpose. One line, whatever the error holds.
pub fn index_unavailable_line(ranked: &RankedHits) -> Option<String> {
    if ranked.mode != SearchMode::Bm25 {
        return None;
    }
    let cause = ranked.cause.as_deref()?;
    let one_line: Vec<&str> = cause.split_whitespace().collect();
    Some(format!(
        "fl: knowledge index unavailable: {}",
        one_line.join(" ")
    ))
}

/// One-line plain-text rendering of a search hit for the CLI.
pub fn format_search_hit(hit: &StateSearchHit) -> String {
    let snippet: String = hit
        .snippet
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    format!(
        "{}\t{}\t{}\t{}\ttokens={}\t{}",
        hit.score_millis,
        hit.entry.scope.as_wire(),
        hit.entry.concern.as_wire(),
        hit.entry.slug,
        hit.estimated_tokens,
        snippet.trim()
    )
}

/// Default hit count for `code-def`/`code-search` surfaces (CLI, MCP, Hub).
pub const DEFAULT_CODE_LIMIT: usize = 50;

/// Default hit count for `code-outline` surfaces (CLI, MCP, Hub).
pub const DEFAULT_CODE_OUTLINE_LIMIT: usize = 500;

/// Maximum hit count for code-index surfaces (CLI, MCP, Hub).
pub const MAX_CODE_LIMIT: usize = 1000;

/// The default code-search limit for `kind` when the caller omits one.
pub fn default_code_limit(kind: CodeSearchKind) -> usize {
    match kind {
        CodeSearchKind::Outline => DEFAULT_CODE_OUTLINE_LIMIT,
        CodeSearchKind::Def | CodeSearchKind::Search => DEFAULT_CODE_LIMIT,
    }
}

/// Resolve a caller-supplied code-search limit: per-kind default, capped
/// at 1000. An explicit `0` stays `0` (no symbols, no token-gain record).
pub fn effective_code_limit(limit: Option<usize>, kind: CodeSearchKind) -> usize {
    limit
        .unwrap_or_else(|| default_code_limit(kind))
        .min(MAX_CODE_LIMIT)
}

/// Wire shape shared by the CLI, MCP, and Hub code-search surfaces
/// (`CodeSearchResponse` in `ui/src/types/api.ts`). `"mode":"knowledge"` is
/// added only when the FerroStore hybrid answered and `"note"` only on a
/// fallback; an exact answer has neither, so its JSON is what it always was.
pub fn code_search_response_json(
    query: &str,
    kind: CodeSearchKind,
    ranked: &RankedCode,
    token_gain: TokenGainSummary,
) -> serde_json::Value {
    let mut out = serde_json::json!({
        "query": query,
        "kind": kind.as_wire(),
        "symbols": ranked.outcome.symbols,
        "tokens_saved_this_query": ranked.outcome.tokens_saved_this_query,
        "tokens_saved_this_query_counted": ranked.outcome.tokens_saved_this_query_counted,
        "count_unit": ranked.outcome.count_unit(),
        "token_gain": token_gain,
    });
    ranked.annotate_json(&mut out);
    out
}

/// One-line plain-text rendering of a code hit for the CLI:
/// `path:start kind name — signature`.
pub fn format_code_hit(hit: &CodeSymbolHit) -> String {
    format!(
        "{}:{} {} {} — {}",
        hit.path, hit.start_line, hit.kind, hit.name, hit.signature
    )
}

/// Run one search over an already-scanned `index` and record its token
/// gain in `store`, returning the ranked result and the updated ledger.
///
/// On FerroStore a `Search` also brings the code vector index up to date
/// (at most [`DEFAULT_CODE_SYNC_BUDGET`] new vectors per call, and no new file
/// is started after a few seconds, so a cold build finishes over several
/// searches). If that path
/// failed, the exact answer is returned and the reason goes to stderr, one
/// line, never into the JSON. The ledger is fed from the final outcome.
///
/// A `limit` of `0` returns no symbols and records nothing; the returned
/// ledger is then the currently stored one ([`load_token_gain`]). Ledger
/// write failures (malformed/sensitive ledger, store errors) propagate.
pub fn search_and_record(
    store: &AnyStore,
    index: &CodeIndex,
    kind: CodeSearchKind,
    query: &str,
    limit: usize,
) -> Result<(RankedCode, TokenGainSummary), String> {
    let session = CodeSession::with_default_embedder(
        store,
        index,
        IndexPolicy::Sync {
            budget: DEFAULT_CODE_SYNC_BUDGET,
        },
    );
    search_and_record_in(&session, store, kind, query, limit)
}

/// [`search_and_record`] on a held [`CodeSession`]: a caller with several
/// searches over one index (`fl_batch_search`) makes the session once, so the
/// sidecar is opened and synced once for all of them.
pub fn search_and_record_in(
    session: &CodeSession<'_>,
    store: &AnyStore,
    kind: CodeSearchKind,
    query: &str,
    limit: usize,
) -> Result<(RankedCode, TokenGainSummary), String> {
    let ranked = session.search(kind, query, limit);
    if let Some(line) = ranked.unavailable_line() {
        eprintln!("{line}");
    }
    if limit == 0 {
        return Ok((ranked, load_token_gain(store)));
    }
    let token_gain = record_token_gain_query(
        store,
        ranked.outcome.tokens_saved_this_query,
        ranked.outcome.tokens_saved_this_query_counted,
        ranked.outcome.files_indexed,
        ranked.outcome.symbols_indexed,
        now_at(),
        "fl",
    )
    .map_err(|e| e.to_string())?;
    Ok((ranked, token_gain))
}

#[derive(Subcommand)]
pub enum StateCmd {
    /// Manage durable `Memory` entries.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Manage `Handoff` status snapshots.
    Handoff {
        #[command(subcommand)]
        cmd: HandoffCmd,
    },
    /// Manage structured JSON `Runtime` entries.
    Runtime {
        #[command(subcommand)]
        cmd: RuntimeCmd,
    },
    /// Fetch a state entry (and optional revision history) by IRI, handle, or key.
    Get {
        /// `urn:uuid:...` IRI, numeric handle (`1`, `2`, …), or entry slug (`[a-z]...`).
        id_or_key: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: Option<String>,
        #[arg(long)]
        history: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search state entries. `--mode` picks the legs: `auto` (default) is
    /// BM25 fused with the vector index when the store has one, `lexical` is
    /// BM25 only, `vector` is the local hashing-embedding index only, `hybrid`
    /// is BM25 + vector fused by reciprocal rank (k=60). `vector` and `hybrid`
    /// need the FerroStore engine and make no network call. With a mode other
    /// than `auto`, the JSON `mode` key is the requested mode (under `auto` it
    /// names the ranker that answered).
    Search {
        /// Search query text.
        query: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        /// Ranking legs: auto, lexical, vector, or hybrid.
        #[arg(long, default_value = "auto", value_parser = RankMode::parse)]
        mode: RankMode,
        #[arg(long)]
        json: bool,
    },
    /// List state entries filtered by scope, concern, or memory category.
    List {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: Option<String>,
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        include_retired: bool,
        #[arg(long)]
        json: bool,
    },
    /// List unresolved `StateConflict`s.
    Conflicts {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Resolve an active `StateConflict` by selecting `--keep <revision-id>`.
    Resolve {
        /// `urn:uuid:...` IRI, numeric handle, or entry slug.
        id_or_key: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: Option<String>,
        /// Winning revision IRI (`local_head` or `remote_head`).
        #[arg(long)]
        keep: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Render a deterministic read-only Markdown startup summary.
    Render {
        #[arg(long = "scope")]
        scopes: Vec<String>,
        #[arg(long, default_value_t = DEFAULT_RENDER_MAX_BYTES)]
        max_bytes: usize,
        #[arg(long)]
        include_sensitive: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Synchronize state and registry revisions with the remote `fl/state` orphan branch.
    Sync {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Verify `fl/state` orphan-branch integrity and append-only invariants.
    Verify {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run the newline-delimited JSON-RPC 2.0 MCP server over stdio.
    Mcp,
    /// Record an automatic git session checkpoint as a `Handoff` entry.
    Checkpoint {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// One-shot import of `.agents/memory/*.md` and `Notes/*/STATE.md`.
    ImportWorkspace {
        #[arg(long = "workspace-root")]
        workspace_root: Option<PathBuf>,
        #[arg(long, default_value = "fl")]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Scan a workspace tree into the structural code index and record its size.
    Index {
        /// Workspace root to scan (defaults to the current directory).
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Find symbol definitions by exact/prefix name in the code index.
    #[command(name = "code-def")]
    CodeDef {
        /// Symbol name to look up.
        symbol: String,
        /// Workspace root to scan (defaults to the current directory).
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Ranked text search over declarations and lines in the code index.
    #[command(name = "code-search")]
    CodeSearch {
        /// Search query text.
        query: String,
        /// Workspace root to scan (defaults to the current directory).
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// List the symbols declared in a file (matched by path) in source order.
    #[command(name = "code-outline")]
    CodeOutline {
        /// File path (or path suffix) to outline.
        file_path: String,
        /// Workspace root to scan (defaults to the current directory).
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Index a local reference source, or validate a remote one (no fetch, nothing stored).
    #[command(name = "ref-index")]
    RefIndex {
        #[arg(long)]
        name: String,
        #[arg(long)]
        locator: String,
        #[arg(long)]
        revision: Option<String>,
        #[arg(long, default_value = "code")]
        kind: String,
        #[arg(long)]
        json: bool,
    },
    /// Register the `ferrostate` MCP server (`fl state mcp`) across vendor CLI configs.
    Install {
        /// Write to user-home vendor configurations instead of the project files.
        #[arg(long)]
        user: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum MemoryCmd {
    /// Create or update a `Memory` entry.
    Set {
        #[arg(long)]
        scope: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        category: String,
        #[arg(long)]
        basis: String,
        #[arg(long)]
        summary: String,
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        #[arg(long = "body-file", conflicts_with = "body")]
        body_file: Option<PathBuf>,
        #[arg(long, conflicts_with = "unpinned")]
        pinned: bool,
        #[arg(long, conflicts_with = "pinned")]
        unpinned: bool,
        #[arg(long, conflicts_with = "not_sensitive")]
        sensitive: bool,
        #[arg(long = "not-sensitive", conflicts_with = "sensitive")]
        not_sensitive: bool,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Retire or supersede an existing `Memory` entry.
    Retire {
        /// `urn:uuid:...` IRI, numeric handle, or memory slug.
        id_or_key: String,
        #[arg(long)]
        scope: Option<String>,
        /// Target `Memory` entry (IRI, handle, or slug in the same scope) that supersedes this entry.
        #[arg(long = "superseded-by")]
        superseded_by: Option<String>,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum HandoffCmd {
    /// Create or update a `Handoff` snapshot (`--key` defaults to `"current"`). Agents that share
    /// a scope should each pass their own `--key` (their persona) so they never overwrite one
    /// another's handoff.
    Set {
        #[arg(long)]
        scope: String,
        #[arg(long, default_value = "current")]
        key: String,
        #[arg(long)]
        summary: String,
        #[arg(long = "shipped")]
        shipped: Vec<String>,
        #[arg(long = "in-flight")]
        in_flight: Vec<String>,
        #[arg(long = "blocker")]
        blockers: Vec<String>,
        #[arg(long = "next")]
        next_steps: Vec<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long, conflicts_with = "not_sensitive")]
        sensitive: bool,
        #[arg(long = "not-sensitive", conflicts_with = "sensitive")]
        not_sensitive: bool,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Read one `Handoff` snapshot by key (`--key` defaults to `"current"`). Looks only at the
    /// handoff concern, so a memory or runtime entry with the same key never shadows it.
    Get {
        #[arg(long)]
        scope: String,
        #[arg(long, default_value = "current")]
        key: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum RuntimeCmd {
    /// Create or update a structured JSON `Runtime` entry.
    Set {
        #[arg(long)]
        scope: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        value: String,
        #[arg(long, default_value = "conflict")]
        merge: String,
        #[arg(long, conflicts_with = "not_sensitive")]
        sensitive: bool,
        #[arg(long = "not-sensitive", conflicts_with = "sensitive")]
        not_sensitive: bool,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
}

pub fn sensitivity_from_flags(sensitive: bool, not_sensitive: bool) -> SensitivityPatch {
    if sensitive {
        SensitivityPatch::SetSensitive
    } else if not_sensitive {
        SensitivityPatch::ClearSensitive
    } else {
        SensitivityPatch::Inherit
    }
}

pub fn parse_concern(s: &str) -> Result<StateConcern, String> {
    StateConcern::from_wire(s).ok_or_else(|| {
        format!(
            "unknown state concern `{s}`; expected one of: {}",
            StateConcern::wire_values()
        )
    })
}

pub fn parse_memory_category(s: &str) -> Result<MemoryCategory, String> {
    MemoryCategory::from_wire(s).ok_or_else(|| {
        format!(
            "unknown memory category `{s}`; expected one of: {}",
            MemoryCategory::wire_values()
        )
    })
}

pub fn parse_memory_basis(s: &str) -> Result<MemoryBasis, String> {
    MemoryBasis::from_wire(s).ok_or_else(|| {
        format!(
            "unknown memory basis `{s}`; expected one of: {}",
            MemoryBasis::wire_values()
        )
    })
}

pub fn parse_ref_kind(s: &str) -> Result<RefKind, String> {
    RefKind::from_wire(s).ok_or_else(|| {
        format!(
            "invalid kind `{s}`: expected `code` or `docs` (one of: {})",
            RefKind::wire_values()
        )
    })
}

/// Human line for a ref-index report (same fields as the JSON shape).
pub fn format_ref_index_report(report: &RefIndexReport) -> String {
    let revision = report.revision.as_deref().unwrap_or("none");
    let mut line = format!(
        "ref-index name={} kind={} locator={} revision={} status={} scanned={} files_indexed={} symbols_indexed={} revision_applied={}",
        report.name,
        report.kind.as_wire(),
        report.locator,
        revision,
        report.status.as_wire(),
        report.scanned,
        report.files_indexed,
        report.symbols_indexed,
        report.revision_applied
    );
    match report.status {
        RefIndexStatus::RemoteNotFetched => {
            line.push_str("; remote source validated; fetch not performed; nothing stored");
            if report.revision.is_some() {
                line.push_str("; revision not applied: remote fetch deferred");
            }
        }
        RefIndexStatus::Scanned => {
            if report.revision.is_some() {
                line.push_str("; revision not applied: local scan indexes the tree as-is");
            }
        }
    }
    line
}

/// Pretty-JSON shape for a ref-index report (same fields as the human line).
pub fn ref_index_report_json(report: &RefIndexReport) -> serde_json::Value {
    serde_json::json!({
        "name": report.name,
        "kind": report.kind.as_wire(),
        "locator": report.locator,
        "revision": report.revision,
        "status": report.status.as_wire(),
        "scanned": report.scanned,
        "files_indexed": report.files_indexed,
        "symbols_indexed": report.symbols_indexed,
        "revision_applied": report.revision_applied,
    })
}

/// Build the `ref-index` output (human or JSON) for one source.
pub fn format_ref_index_output(
    name: &str,
    locator: &str,
    revision: Option<&str>,
    kind: &str,
    json: bool,
) -> Result<String, String> {
    let kind = parse_ref_kind(kind)?;
    let source = RefSource::new(
        name.to_string(),
        locator.to_string(),
        revision.map(str::to_string),
        kind,
    )
    .map_err(|e| e.to_string())?;
    let plan = RefIndexPlan::build(vec![source]).map_err(|e| e.to_string())?;
    let source = plan
        .sources()
        .first()
        .ok_or_else(|| "ref-index plan is empty".to_string())?;
    let report = index_local_source(source).map_err(|e| e.to_string())?;
    if json {
        serde_json::to_string_pretty(&ref_index_report_json(&report)).map_err(|e| e.to_string())
    } else {
        Ok(format_ref_index_report(&report))
    }
}

pub fn parse_merge_mode(s: &str) -> Result<MergeMode, String> {
    MergeMode::from_wire(s).ok_or_else(|| {
        format!(
            "unknown merge mode `{s}`; expected one of: {}",
            MergeMode::wire_values()
        )
    })
}

/// Resolve `<id-or-key>` in `store` per Spec §5.2:
/// 1. `urn:uuid:...` -> `get_state`
/// 2. Bare digits (`1`, `2`, …) -> `resolve_handle(Kind::State, n)` -> `get_state`
/// 3. ASCII-letter slug (`[a-z]...`) -> requires `scope`; if `concern` is given, uses
///    `get_state_by_key`, otherwise checks all 3 concerns (defaulting to `Handoff` for
///    `"current"` when present and unambiguous, or failing if ambiguous).
pub fn resolve_state_in_store<S: StateStore + Handles>(
    store: &S,
    id_or_key: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
) -> Result<StateEntry, String> {
    if let Ok(r) = id_or_key.parse::<Ref>()
        && !matches!(r, Ref::Slug(_))
    {
        let iri = match r {
            Ref::Iri(iri) => iri,
            Ref::Handle(h) => store
                .resolve_handle(Kind::State, h)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no state entry with handle {h} in this store"))?,
            Ref::Issue(_) | Ref::Slug(_) => {
                return Err(format!(
                    "`{id_or_key}` is an issue reference, not a state handle, IRI, or key"
                ));
            }
        };
        return store
            .get_state(&StateId(iri))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no state entry matching `{id_or_key}`"));
    }

    let slug = validate_slug(id_or_key).map_err(|e| e.to_string())?;
    let Some(scope) = scope else {
        return Err(format!(
            "resolving state slug `{slug}` requires `--scope <scope>`"
        ));
    };
    if let Some(c) = concern {
        return store
            .get_state_by_key(scope, c, &slug)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "no state entry for `({}, {}, {slug})`",
                    scope.as_wire(),
                    c.as_wire()
                )
            });
    }

    let mut matches = Vec::new();
    for c in [
        StateConcern::Memory,
        StateConcern::Handoff,
        StateConcern::Runtime,
    ] {
        if let Some(e) = store
            .get_state_by_key(scope, c, &slug)
            .map_err(|e| e.to_string())?
        {
            matches.push(e);
        }
    }
    match matches.len() {
        0 => Err(format!(
            "no state entry with slug `{slug}` in scope `{}`",
            scope.as_wire()
        )),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => {
            if slug == "current"
                && let Some(h) = matches
                    .iter()
                    .find(|e| e.concern == StateConcern::Handoff)
                    .cloned()
            {
                return Ok(h);
            }
            Err(format!(
                "slug `{slug}` exists in multiple concerns under `{}`; pass `--concern <memory|handoff|runtime>`",
                scope.as_wire()
            ))
        }
    }
}

fn scope_rank(scope: &StateScope) -> (u8, String) {
    match scope {
        StateScope::Workspace => (0, String::new()),
        StateScope::Group(s) => (1, s.clone()),
        StateScope::Project(s) => (2, s.clone()),
        StateScope::Agent(s) => (3, s.clone()),
        StateScope::Workflow(s) => (4, s.clone()),
    }
}

/// Render the deterministic read-only startup Markdown summary (spec §5.2):
/// 1. Active agent roster summary.
/// 2. Current `Handoff` snapshot(s) for the requested scopes.
/// 3. Active `Memory` index lines ordered by priority tier (`pinned || category == Warning`
///    first, then remaining active memories), sorted by `(scope_rank, slug)` within each tier,
///    capped so total output (including the `_Truncated: ..._` footer line) never exceeds `max_bytes`.
pub fn render_state_markdown(
    areas: &[AreaRecord],
    agents: &[AgentRecord],
    entries: &[StateEntry],
    max_bytes: usize,
    include_sensitive: bool,
) -> String {
    let visible: Vec<&StateEntry> = entries
        .iter()
        .filter(|e| include_sensitive || !e.sensitive)
        .collect();

    let mut out = String::from("# FerroLoop State Summary\n\n");

    // 1. Active Agents section.
    out.push_str("## Active Agents\n\n");
    let mut active_agents: Vec<&AgentRecord> = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Active)
        .collect();
    active_agents.sort_by(|a, b| a.name_key.cmp(&b.name_key));
    if active_agents.is_empty() {
        out.push_str("_No active agents registered._\n\n");
    } else {
        for a in active_agents {
            let mgr = reports_to(a, agents);
            let area_name = areas
                .iter()
                .find(|ar| ar.area_key == a.project)
                .map(|ar| ar.name.as_str())
                .unwrap_or(a.project.as_str());
            out.push_str(&format!(
                "- **{}** (`{}`, `{}`, project `{}` / {}, reports to {})\n",
                a.display_name,
                a.name_key,
                a.role.as_wire(),
                a.project,
                area_name,
                mgr
            ));
        }
        out.push('\n');
    }

    // 2. Current Handoff section.
    let mut handoffs: Vec<(&StateEntry, &HandoffEntry)> = visible
        .iter()
        .filter_map(|e| match &e.payload {
            StatePayload::Handoff(h) => Some((*e, h)),
            _ => None,
        })
        .collect();
    handoffs.sort_by(|(a, _), (b, _)| {
        (scope_rank(&a.scope), &a.slug).cmp(&(scope_rank(&b.scope), &b.slug))
    });
    if !handoffs.is_empty() {
        out.push_str("## Handoff\n\n");
        for (entry, h) in handoffs {
            out.push_str(&format!(
                "### `{}` / `{}` — {}\n",
                entry.scope.as_wire(),
                entry.slug,
                h.summary
            ));
            if !h.in_flight.is_empty() {
                out.push_str(&format!("- **In flight:** {}\n", h.in_flight.join("; ")));
            }
            if !h.blockers.is_empty() {
                out.push_str(&format!("- **Blockers:** {}\n", h.blockers.join("; ")));
            }
            if !h.next_steps.is_empty() {
                out.push_str(&format!("- **Next steps:** {}\n", h.next_steps.join("; ")));
            }
            if !h.shipped.is_empty() {
                out.push_str(&format!("- **Shipped:** {}\n", h.shipped.join("; ")));
            }
            out.push('\n');
        }
    }

    // 3. Active Memory Index section.
    let mut memories: Vec<(&StateEntry, &MemoryEntry)> = visible
        .iter()
        .filter_map(|e| match &e.payload {
            StatePayload::Memory(m) if m.lifecycle == MemoryLifecycle::Active => Some((*e, m)),
            _ => None,
        })
        .collect();
    memories.sort_by(|(ea, ma), (eb, mb)| {
        let tier_a = !(ma.pinned || ma.category == MemoryCategory::Warning);
        let tier_b = !(mb.pinned || mb.category == MemoryCategory::Warning);
        (tier_a, scope_rank(&ea.scope), &ea.slug).cmp(&(tier_b, scope_rank(&eb.scope), &eb.slug))
    });

    out.push_str("## Memory Index\n\n");
    if out.len() > max_bytes {
        out.truncate(max_bytes);
        return out;
    }

    let total_memories = memories.len();
    for (idx, (entry, mem)) in memories.iter().enumerate() {
        let line = format!(
            "- [{}] ({}, {}, {}) — {}\n",
            entry.slug,
            entry.id,
            mem.category.as_wire(),
            mem.basis.as_wire(),
            mem.summary
        );
        let remaining_after_this = total_memories - (idx + 1);
        let reserve_footer = if remaining_after_this > 0 {
            format!(
                "_Truncated: {} additional active memory entries omitted by byte cap._\n",
                remaining_after_this
            )
            .len()
        } else {
            0
        };

        if out.len() + line.len() + reserve_footer > max_bytes {
            let omitted = total_memories - idx;
            let footer = format!(
                "_Truncated: {omitted} additional active memory entries omitted by byte cap._\n"
            );
            if out.len() + footer.len() <= max_bytes {
                out.push_str(&footer);
            }
            break;
        }
        out.push_str(&line);
    }

    out
}

pub struct StateEnv<'a> {
    pub explicit_db: Option<&'a Path>,
    pub default_store: &'a Path,
    pub cfg: &'a Config,
    pub cwd: &'a Path,
}

impl<'a> StateEnv<'a> {
    /// Default scopes for `fl state render` when `--scope` is omitted (spec §5.2):
    /// `workspace`, plus `project:<slug>` when `cwd` is inside a configured `[[project]]`.
    pub fn default_render_scopes(&self) -> Vec<StateScope> {
        let mut scopes = vec![StateScope::Workspace];
        if let Ok(Some(entry)) = crate::config::bound_entry(&self.cfg.projects, self.cwd)
            && let Some(slug) = entry.inferred_slug()
        {
            scopes.push(StateScope::Project(slug));
        }
        scopes
    }

    /// Default checkpoint scope: the bound project scope for `cwd` when
    /// configured, else `workspace`.
    pub fn default_checkpoint_scope(&self) -> StateScope {
        if let Ok(Some(entry)) = crate::config::bound_entry(&self.cfg.projects, self.cwd)
            && let Some(slug) = entry.inferred_slug()
        {
            return StateScope::Project(slug);
        }
        StateScope::Workspace
    }

    pub fn open_for_scope(&self, scope: Option<&StateScope>) -> Result<AnyStore, String> {
        let path = if let Some(db) = self.explicit_db {
            db.to_path_buf()
        } else if let Some(sc) = scope {
            self.cfg.state_store_for_scope(sc, self.default_store)
        } else {
            self.cfg.workspace_state_store(self.default_store)
        };
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            fl_store::private::create_private_dir_all(dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        fl_store::open_store(&path).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn open_for_id_or_key(
        &self,
        id_or_key: &str,
        scope: Option<&StateScope>,
    ) -> Result<AnyStore, String> {
        let primary_path = if let Some(db) = self.explicit_db {
            db.to_path_buf()
        } else if let Some(sc) = scope {
            self.cfg.state_store_for_scope(sc, self.default_store)
        } else {
            self.cfg.workspace_state_store(self.default_store)
        };
        if let Some(dir) = primary_path.parent()
            && !dir.as_os_str().is_empty()
        {
            fl_store::private::create_private_dir_all(dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let primary = fl_store::open_store(&primary_path)
            .map_err(|e| format!("{}: {e}", primary_path.display()))?;
        if self.explicit_db.is_some() {
            return Ok(primary);
        }
        let Ok(Ref::Iri(iri)) = id_or_key.parse::<Ref>() else {
            return Ok(primary);
        };
        if primary.owns(&iri).map_err(|e| e.to_string())? {
            return Ok(primary);
        }
        let ws_path = self.cfg.workspace_state_store(self.default_store);
        if !crate::same_store(&ws_path, &primary_path)
            && fl_store::store_exists(&ws_path)
            && scan_owns(&ws_path, &iri)?
        {
            return fl_store::open_store(&ws_path).map_err(|e| e.to_string());
        }
        for entry in &self.cfg.projects {
            let path = &entry.store;
            if crate::same_store(path, &primary_path) || !fl_store::store_exists(path) {
                continue;
            }
            if scan_owns(path, &iri)? {
                return fl_store::open_store(path).map_err(|e| e.to_string());
            }
        }
        Ok(primary)
    }
}

/// Whether the store at `path` holds `iri`, asked without migrating it: a lookup
/// that only searches must not migrate every configured store. (The store a
/// command goes on to use is opened with `open_store`.)
pub(crate) fn scan_owns(path: &Path, iri: &Iri) -> Result<bool, String> {
    let scan = fl_store::open_store_for_scan(path).map_err(|e| e.to_string())?;
    scan.owns(iri).map_err(|e| e.to_string())
}

pub fn run(env: &StateEnv<'_>, cmd: StateCmd) -> Result<(), String> {
    match cmd {
        StateCmd::Memory { cmd } => match cmd {
            MemoryCmd::Set {
                scope,
                key,
                category,
                basis,
                summary,
                body,
                body_file,
                pinned,
                unpinned,
                sensitive,
                not_sensitive,
                by,
                json,
            } => {
                let parsed_scope = StateScope::from_wire(&scope).map_err(|e| e.to_string())?;
                let parsed_cat = parse_memory_category(&category)?;
                let parsed_basis = parse_memory_basis(&basis)?;
                let resolved_body = match (body, body_file) {
                    (Some(b), None) => b,
                    (None, Some(path)) => std::fs::read_to_string(&path)
                        .map_err(|e| format!("{}: {e}", path.display()))?,
                    _ => {
                        return Err("provide either `--body <text>` or `--body-file <path>`".into());
                    }
                };
                let store = env.open_for_scope(Some(&parsed_scope))?;
                let existing = store
                    .get_state_by_key(&parsed_scope, StateConcern::Memory, &key)
                    .map_err(|e| e.to_string())?;
                let resolved_pinned = if pinned {
                    true
                } else if unpinned {
                    false
                } else if let Some(StateEntry {
                    payload: StatePayload::Memory(ref m),
                    ..
                }) = existing
                {
                    m.pinned
                } else {
                    false
                };
                let entry = store
                    .upsert_state(UpsertStateArgs {
                        scope: parsed_scope,
                        slug: key,
                        sensitivity: sensitivity_from_flags(sensitive, not_sensitive),
                        payload: StatePayload::Memory(MemoryEntry {
                            category: parsed_cat,
                            basis: parsed_basis,
                            lifecycle: MemoryLifecycle::Active,
                            superseded_by: None,
                            pinned: resolved_pinned,
                            summary,
                            body: resolved_body,
                        }),
                        at: now_at(),
                        by,
                    })
                    .map_err(|e| e.to_string())?;
                print_state_entry(&store, &entry, json);
                Ok(())
            }
            MemoryCmd::Retire {
                id_or_key,
                scope,
                superseded_by,
                by,
                json,
            } => {
                let parsed_scope = scope
                    .as_deref()
                    .map(StateScope::from_wire)
                    .transpose()
                    .map_err(|e| e.to_string())?;
                let store = env.open_for_id_or_key(&id_or_key, parsed_scope.as_ref())?;
                let target_entry = resolve_state_in_store(
                    &store,
                    &id_or_key,
                    parsed_scope.as_ref(),
                    Some(StateConcern::Memory),
                )?;
                let (lifecycle, sup_id) = match superseded_by {
                    Some(ref raw_sup) => {
                        let sup_entry = resolve_state_in_store(
                            &store,
                            raw_sup,
                            Some(&target_entry.scope),
                            Some(StateConcern::Memory),
                        )?;
                        (MemoryLifecycle::Superseded, Some(sup_entry.id))
                    }
                    None => (MemoryLifecycle::Retired, None),
                };
                let updated = store
                    .retire_memory(
                        &target_entry.scope,
                        &target_entry.slug,
                        lifecycle,
                        sup_id,
                        now_at(),
                        &by,
                    )
                    .map_err(|e| e.to_string())?;
                print_state_entry(&store, &updated, json);
                Ok(())
            }
        },
        StateCmd::Handoff { cmd } => match cmd {
            HandoffCmd::Set {
                scope,
                key,
                summary,
                shipped,
                in_flight,
                blockers,
                next_steps,
                notes,
                sensitive,
                not_sensitive,
                by,
                json,
            } => {
                let parsed_scope = StateScope::from_wire(&scope).map_err(|e| e.to_string())?;
                let store = env.open_for_scope(Some(&parsed_scope))?;
                let entry = store
                    .upsert_state(UpsertStateArgs {
                        scope: parsed_scope,
                        slug: key,
                        sensitivity: sensitivity_from_flags(sensitive, not_sensitive),
                        payload: StatePayload::Handoff(HandoffEntry {
                            summary,
                            shipped,
                            in_flight,
                            blockers,
                            next_steps,
                            notes,
                        }),
                        at: now_at(),
                        by,
                    })
                    .map_err(|e| e.to_string())?;
                print_state_entry(&store, &entry, json);
                Ok(())
            }
            HandoffCmd::Get { scope, key, json } => {
                let parsed_scope = StateScope::from_wire(&scope).map_err(|e| e.to_string())?;
                let store = env.open_for_scope(Some(&parsed_scope))?;
                let slug = validate_slug(&key).map_err(|e| e.to_string())?;
                let entry = store
                    .get_state_by_key(&parsed_scope, StateConcern::Handoff, &slug)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| {
                        format!("no handoff `{slug}` in scope `{}`", parsed_scope.as_wire())
                    })?;
                print_state_entry(&store, &entry, json);
                Ok(())
            }
        },
        StateCmd::Runtime { cmd } => match cmd {
            RuntimeCmd::Set {
                scope,
                key,
                value,
                merge,
                sensitive,
                not_sensitive,
                by,
                json,
            } => {
                let parsed_scope = StateScope::from_wire(&scope).map_err(|e| e.to_string())?;
                let merge_mode = parse_merge_mode(&merge)?;
                let json_val: serde_json::Value = serde_json::from_str(&value)
                    .map_err(|e| format!("invalid `--value` JSON: {e}"))?;
                let store = env.open_for_scope(Some(&parsed_scope))?;
                let entry = store
                    .upsert_state(UpsertStateArgs {
                        scope: parsed_scope,
                        slug: key,
                        sensitivity: sensitivity_from_flags(sensitive, not_sensitive),
                        payload: StatePayload::Runtime(RuntimeEntry {
                            value: json_val,
                            merge_mode,
                        }),
                        at: now_at(),
                        by,
                    })
                    .map_err(|e| e.to_string())?;
                print_state_entry(&store, &entry, json);
                Ok(())
            }
        },
        StateCmd::Get {
            id_or_key,
            scope,
            concern,
            history,
            json,
        } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> =
                concern.as_deref().map(parse_concern).transpose()?;
            let store = env.open_for_id_or_key(&id_or_key, parsed_scope.as_ref())?;
            let entry =
                resolve_state_in_store(&store, &id_or_key, parsed_scope.as_ref(), parsed_concern)?;
            if history {
                let revs = store
                    .state_revisions(&entry.id)
                    .map_err(|e| e.to_string())?;
                if json {
                    let out = serde_json::json!({
                        "entry": entry,
                        "revisions": revs,
                    });
                    println!("{}", serde_json::to_string_pretty(&out).unwrap());
                } else {
                    print_state_entry(&store, &entry, false);
                    for r in revs {
                        println!(
                            "  rev {}\tparents={}\tat={}\tby={}",
                            r.revision_id,
                            r.parent_revisions
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(","),
                            r.at,
                            r.by
                        );
                    }
                }
            } else {
                print_state_entry(&store, &entry, json);
            }
            Ok(())
        }
        StateCmd::Search {
            query,
            scope,
            concern,
            limit,
            mode,
            json,
        } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> =
                concern.as_deref().map(parse_concern).transpose()?;
            let effective_limit = effective_search_limit(limit);
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let entries: Vec<StateEntry> = store
                .list_state(parsed_scope.as_ref(), parsed_concern)
                .map_err(|e| e.to_string())?;
            let ranked = rank_for_mode(
                &store,
                mode,
                &entries,
                &query,
                parsed_scope.as_ref(),
                parsed_concern,
                effective_limit,
            )?;
            let hits = &ranked.hits;
            if let Some(line) = index_unavailable_line(&ranked) {
                eprintln!("{line}");
            }
            if let Some(line) = partial_index_line(&ranked) {
                eprintln!("{line}");
            }
            if json {
                let out = ranked_result_json_for(&query, &ranked, mode);
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else if hits.is_empty() {
                println!("no hits for `{query}`");
            } else {
                for hit in hits {
                    println!("{}", format_search_hit(hit));
                }
            }
            Ok(())
        }
        StateCmd::List {
            scope,
            concern,
            category,
            include_retired,
            json,
        } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> =
                concern.as_deref().map(parse_concern).transpose()?;
            let parsed_cat: Option<MemoryCategory> =
                category.as_deref().map(parse_memory_category).transpose()?;
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let entries: Vec<StateEntry> = store
                .list_state(parsed_scope.as_ref(), parsed_concern)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|e| match &e.payload {
                    StatePayload::Memory(m) => {
                        (include_retired || m.lifecycle == MemoryLifecycle::Active)
                            && parsed_cat.is_none_or(|c| m.category == c)
                    }
                    _ => parsed_cat.is_none(),
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&entries).unwrap());
            } else {
                for e in &entries {
                    print_state_entry(&store, e, false);
                }
            }
            Ok(())
        }
        StateCmd::Conflicts { scope, json } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let mut conflicts = store.list_conflicts().map_err(|e| e.to_string())?;
            if let Some(ref sc) = parsed_scope {
                conflicts.retain(|c| {
                    store
                        .get_state(&c.entry_id)
                        .ok()
                        .flatten()
                        .is_some_and(|e| &e.scope == sc)
                });
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&conflicts).unwrap());
            } else {
                for c in conflicts {
                    println!(
                        "{}\tlocal={}\tremote={}\tdetected_at={}",
                        c.entry_id, c.local_head, c.remote_head, c.detected_at
                    );
                }
            }
            Ok(())
        }
        StateCmd::Resolve {
            id_or_key,
            scope,
            concern,
            keep,
            by,
            json,
        } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let parsed_concern: Option<StateConcern> =
                concern.as_deref().map(parse_concern).transpose()?;
            let keep_iri = Iri::parse(&keep).map_err(|e| format!("invalid `--keep` IRI: {e}"))?;
            let store = env.open_for_id_or_key(&id_or_key, parsed_scope.as_ref())?;
            let entry =
                resolve_state_in_store(&store, &id_or_key, parsed_scope.as_ref(), parsed_concern)?;
            let resolved = store
                .resolve_conflict(&entry.id, &RevisionId(keep_iri), now_at(), &by)
                .map_err(|e| e.to_string())?;
            print_state_entry(&store, &resolved, json);
            Ok(())
        }
        StateCmd::Render {
            scopes,
            max_bytes,
            include_sensitive,
            out,
        } => {
            let mut parsed_scopes = Vec::new();
            for raw in &scopes {
                parsed_scopes.push(StateScope::from_wire(raw).map_err(|e| e.to_string())?);
            }
            if parsed_scopes.is_empty() {
                parsed_scopes = env.default_render_scopes();
            }

            let (areas, agents) = {
                let ws_store = env.open_for_scope(Some(&StateScope::Workspace))?;
                let areas = ws_store.list_areas().map_err(|e| e.to_string())?;
                let agents = ws_store
                    .list_agents(None, false)
                    .map_err(|e| e.to_string())?;
                (areas, agents)
            };

            let mut entries = Vec::new();
            for sc in &parsed_scopes {
                let st = env.open_for_scope(Some(sc))?;
                entries.extend(st.list_state(Some(sc), None).map_err(|e| e.to_string())?);
            }

            let md = render_state_markdown(&areas, &agents, &entries, max_bytes, include_sensitive);
            if let Some(ref path) = out {
                write_atomic_non_symlink(path, md.as_bytes())?;
            } else {
                print!("{md}");
            }
            Ok(())
        }
        StateCmd::Sync { scope, by, json } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let target = env.cfg.sync_target_for_scope(
                parsed_scope.as_ref(),
                env.cwd,
                env.default_store,
                &by,
            );
            let report = sync_state(&store, &target, now_at()).map_err(|e| e.to_string())?;
            if json {
                let val = serde_json::json!({
                    "remote": report.remote,
                    "branch": report.branch,
                    "previous_commit": report.previous_commit,
                    "committed_oid": report.committed_oid,
                    "imported_registry_events": report.imported_registry_events,
                    "imported_revisions": report.imported_revisions,
                    "imported_wire_messages": report.imported_wire_messages,
                    "imported_wire_receipts": report.imported_wire_receipts,
                    "published_registry_events": report.published_registry_events,
                    "published_revisions": report.published_revisions,
                    "published_wire_messages": report.published_wire_messages,
                    "published_wire_receipts": report.published_wire_receipts,
                    "active_conflicts": report.active_conflicts,
                });
                println!("{}", serde_json::to_string_pretty(&val).unwrap());
            } else {
                println!(
                    "synced {}/{}: imported_revs={} published_revs={} wire_msgs={} conflicts={}",
                    report.remote,
                    report.branch,
                    report.imported_revisions,
                    report.published_revisions,
                    report.published_wire_messages,
                    report.active_conflicts
                );
            }
            Ok(())
        }
        StateCmd::Verify { scope, json } => {
            let parsed_scope = scope
                .as_deref()
                .map(StateScope::from_wire)
                .transpose()
                .map_err(|e| e.to_string())?;
            let store = env.open_for_scope(parsed_scope.as_ref())?;
            let target = env.cfg.sync_target_for_scope(
                parsed_scope.as_ref(),
                env.cwd,
                env.default_store,
                "fl",
            );
            let report = verify_state(&store, &target).map_err(|e| e.to_string())?;
            if json {
                let val = serde_json::json!({
                    "remote": report.remote,
                    "branch": report.branch,
                    "tip_commit": report.tip_commit,
                    "verified_commits": report.verified_commits,
                    "verified_revisions": report.verified_revisions,
                    "verified_registry_events": report.verified_registry_events,
                    "verified_wire_messages": report.verified_wire_messages,
                    "verified_wire_receipts": report.verified_wire_receipts,
                });
                println!("{}", serde_json::to_string_pretty(&val).unwrap());
            } else {
                println!(
                    "verified {}/{}: commits={} revisions={} registry_events={} wire_messages={}",
                    report.remote,
                    report.branch,
                    report.verified_commits,
                    report.verified_revisions,
                    report.verified_registry_events,
                    report.verified_wire_messages
                );
            }
            Ok(())
        }
        StateCmd::Checkpoint {
            scope,
            slug,
            summary,
            by,
            json,
        } => {
            let by = if by.trim().is_empty() {
                "fl".to_string()
            } else {
                by
            };
            let resolved_scope = match scope.as_deref() {
                Some(raw) => StateScope::from_wire(raw).map_err(|e| e.to_string())?,
                None => env.default_checkpoint_scope(),
            };
            let store = env.open_for_scope(Some(&resolved_scope))?;
            let report = create_state_checkpoint(
                &store,
                env.cwd,
                resolved_scope,
                slug.as_deref(),
                summary.as_deref(),
                &by,
                now_at(),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                println!(
                    "checkpoint scope={} slug={} branch={} head={} dirty={}",
                    report.entry.scope.as_wire(),
                    report.entry.slug,
                    report.branch,
                    report.head_sha,
                    report.dirty_files
                );
            }
            Ok(())
        }
        StateCmd::Mcp => crate::cmd::state_mcp::run(env),
        StateCmd::ImportWorkspace {
            workspace_root,
            by,
            json,
        } => {
            let root = workspace_root.unwrap_or_else(|| resolve_workspace_root(env.cwd));
            let scan = scan_workspace_import(&root, &by, now_at());
            let mut grouped: BTreeMap<StateScope, Vec<UpsertStateArgs>> = BTreeMap::new();
            for upsert in scan.upserts {
                grouped
                    .entry(upsert.scope.clone())
                    .or_default()
                    .push(upsert);
            }
            let mut report = WorkspaceImportReport {
                skipped: scan.skipped,
                ..WorkspaceImportReport::default()
            };
            let mut scopes = std::collections::BTreeSet::new();
            for (scope, upserts) in grouped {
                let store = match env.open_for_scope(Some(&scope)) {
                    Ok(store) => store,
                    Err(e) => {
                        report.skipped.push(format!(
                            "{}: failed to open state store: {e}",
                            scope.as_wire()
                        ));
                        continue;
                    }
                };
                let partial = apply_workspace_import(&store, upserts, Vec::new());
                report.imported_memories += partial.imported_memories;
                report.imported_handoffs += partial.imported_handoffs;
                scopes.extend(partial.scopes);
                report.skipped.extend(partial.skipped);
            }
            report.scopes = scopes.into_iter().collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                println!(
                    "imported memories={} handoffs={} scopes={}",
                    report.imported_memories,
                    report.imported_handoffs,
                    report.scopes.join(",")
                );
                for skipped in &report.skipped {
                    println!("skipped: {skipped}");
                }
            }
            Ok(())
        }
        StateCmd::Index { root, json } => {
            let root = root.unwrap_or_else(|| env.cwd.to_path_buf());
            let index = scan_code_index(&root);
            let store = env.open_for_scope(Some(&StateScope::Workspace))?;
            let token_gain = record_token_gain_index(
                &store,
                index.files_indexed(),
                index.symbols_indexed(),
                now_at(),
                "fl",
            )
            .map_err(|e| e.to_string())?;
            if json {
                let out = serde_json::json!({
                    "files_indexed": index.files_indexed(),
                    "symbols_indexed": index.symbols_indexed(),
                    "token_gain": token_gain,
                });
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else {
                println!(
                    "indexed files={} symbols={}",
                    index.files_indexed(),
                    index.symbols_indexed()
                );
            }
            Ok(())
        }
        StateCmd::CodeDef {
            symbol,
            root,
            limit,
            json,
        } => run_code_query(env, CodeSearchKind::Def, &symbol, root, limit, json),
        StateCmd::CodeSearch {
            query,
            root,
            limit,
            json,
        } => run_code_query(env, CodeSearchKind::Search, &query, root, limit, json),
        StateCmd::CodeOutline {
            file_path,
            root,
            limit,
            json,
        } => run_code_query(env, CodeSearchKind::Outline, &file_path, root, limit, json),
        StateCmd::RefIndex {
            name,
            locator,
            revision,
            kind,
            json,
        } => {
            println!(
                "{}",
                format_ref_index_output(&name, &locator, revision.as_deref(), &kind, json)?
            );
            Ok(())
        }
        StateCmd::Install { user, json } => crate::cmd::vault::run_mcp_install(
            env.cwd,
            user,
            json,
            "ferrostate",
            &["state", "mcp"],
            "`ferrostate` MCP server",
        )
        .map(|_| ())
        .map_err(|(_, msg)| msg),
    }
}

/// Shared body of `code-def`/`code-search`/`code-outline`: scan `root`
/// (default `env.cwd`) once, search, record the query's token gain, and
/// print the shared `CodeSearchResponse` JSON or one line per hit.
fn run_code_query(
    env: &StateEnv<'_>,
    kind: CodeSearchKind,
    query: &str,
    root: Option<PathBuf>,
    limit: Option<usize>,
    json: bool,
) -> Result<(), String> {
    if query.trim().is_empty() {
        return Err("query must not be blank".to_string());
    }
    let root = root.unwrap_or_else(|| env.cwd.to_path_buf());
    let effective_limit = effective_code_limit(limit, kind);
    let index = scan_code_index(&root);
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    let (ranked, token_gain) = search_and_record(&store, &index, kind, query, effective_limit)?;
    if json {
        let out = code_search_response_json(query, kind, &ranked, token_gain);
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else if ranked.outcome.symbols.is_empty() {
        println!("no hits for `{query}`");
    } else {
        for hit in &ranked.outcome.symbols {
            println!("{}", format_code_hit(hit));
        }
    }
    Ok(())
}

fn print_state_entry<S: Handles>(store: &S, entry: &StateEntry, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(entry).unwrap());
    } else {
        let h = store
            .handle_of(Kind::State, entry.id.iri())
            .ok()
            .flatten()
            .unwrap_or(0);
        println!(
            "{h}\t{}\t{}\t{}\t{}\trev={}",
            entry.id,
            entry.scope.as_wire(),
            entry.concern.as_wire(),
            entry.slug,
            entry.head_revision
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;
    use fl_core::agent::{AgentRole, RegisterAgentArgs};

    #[test]
    fn ref_index_local_scan_exact_counts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\npub struct Widget;\n",
        )
        .unwrap();
        let locator = dir.path().to_string_lossy().into_owned();
        let out = format_ref_index_output("demo", &locator, None, "code", true).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["name"], "demo");
        assert_eq!(v["kind"], "code");
        assert_eq!(v["locator"], locator);
        assert_eq!(v["revision"], serde_json::Value::Null);
        assert_eq!(v["status"], "scanned");
        assert_eq!(v["scanned"], true);
        assert_eq!(v["files_indexed"], 1);
        assert_eq!(v["symbols_indexed"], 3);
        assert_eq!(v["revision_applied"], false);
        let human = format_ref_index_output("demo", &locator, None, "code", false).unwrap();
        assert!(human.contains("status=scanned"), "{human}");
        assert!(human.contains("files_indexed=1"), "{human}");
        assert!(human.contains("symbols_indexed=3"), "{human}");
    }

    #[test]
    fn ref_index_remote_json_and_human_carry_same_fields() {
        let args = ("demo", "https://example.com/r.git", Some("v1"), "code");
        let out = format_ref_index_output(args.0, args.1, args.2, args.3, true).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["name"], "demo");
        assert_eq!(v["kind"], "code");
        assert_eq!(v["locator"], "https://example.com/r.git");
        assert_eq!(v["revision"], "v1");
        assert_eq!(v["status"], "remote-not-fetched");
        assert_eq!(v["scanned"], false);
        assert_eq!(v["files_indexed"], 0);
        assert_eq!(v["symbols_indexed"], 0);
        assert_eq!(v["revision_applied"], false);
        let human = format_ref_index_output(args.0, args.1, args.2, args.3, false).unwrap();
        for needle in [
            "name=demo",
            "kind=code",
            "locator=https://example.com/r.git",
            "revision=v1",
            "status=remote-not-fetched",
            "scanned=false",
            "files_indexed=0",
            "symbols_indexed=0",
            "revision_applied=false",
            "remote source validated; fetch not performed; nothing stored",
            "revision not applied: remote fetch deferred",
        ] {
            assert!(human.contains(needle), "missing {needle} in {human}");
        }
        assert!(!human.contains("registered"), "{human}");
    }

    #[test]
    fn ref_index_missing_path_error_names_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let locator = missing.to_string_lossy().into_owned();
        let err = format_ref_index_output("demo", &locator, None, "code", false).unwrap_err();
        assert!(err.contains(&locator), "{err}");
    }

    #[test]
    fn ref_index_invalid_kind_and_revision_applied_false() {
        let err =
            format_ref_index_output("demo", "https://example.com/r.git", None, "bogus", false)
                .unwrap_err();
        assert!(
            err.contains("bogus") && err.contains("code") && err.contains("docs"),
            "{err}"
        );
        // Revision given on a local scan: not applied, tree indexed as-is.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn alpha() {}\n").unwrap();
        let locator = dir.path().to_string_lossy().into_owned();
        let out = format_ref_index_output("demo", &locator, Some("v2"), "code", true).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["revision"], "v2");
        assert_eq!(v["revision_applied"], false);
        let human = format_ref_index_output("demo", &locator, Some("v2"), "code", false).unwrap();
        assert!(human.contains("revision_applied=false"), "{human}");
        assert!(
            human.contains("revision not applied: local scan indexes the tree as-is"),
            "{human}"
        );
    }

    /// `scan_owns` answers for a store it can open, and fails (never "not mine") on one it cannot.
    #[test]
    fn scan_owns_answers_for_a_store_and_fails_on_one_it_cannot_open() {
        use fl_core::Catalog;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.redb");
        let project = {
            let store = fl_store::open_redb(&path).unwrap();
            store.add_project("/repo").unwrap()
        };
        assert!(scan_owns(&path, project.iri()).unwrap());
        let stranger = Iri::parse("urn:uuid:00000000-0000-7000-8000-000000000001").unwrap();
        assert!(!scan_owns(&path, &stranger).unwrap());
        // A directory is not a store: an error, never "not mine".
        let not_a_store = dir.path().join("d.redb");
        std::fs::create_dir(&not_a_store).unwrap();
        assert!(scan_owns(&not_a_store, &stranger).is_err());
    }

    /// A store reached through two configured paths (here a symlink to the `.fls` file) is the
    /// same store: `open_for_id_or_key` takes the same-store branch and does not scan the alias
    /// as a second store. The primary is made with the ferro engine explicitly (a `.fls` path),
    /// so the alias resolves to the primary's physical file on any engine selection.
    #[test]
    fn open_for_id_or_key_does_not_reopen_the_primary_through_an_alias() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("proj.fls");
        let alias = dir.path().join("alias.fls");
        fl_store::open_store_with(
            &primary,
            &fl_store::StoreOptions {
                backend: Some(fl_store::BackendKind::Ferro),
                ..fl_store::StoreOptions::default()
            },
        )
        .unwrap();
        std::os::unix::fs::symlink(&primary, &alias).unwrap();
        assert!(
            crate::same_store(&primary, &alias),
            "the alias names the primary's file"
        );

        let cfg = Config {
            projects: vec![
                crate::config::Entry {
                    name: Some("proj".into()),
                    root: dir.path().join("proj"),
                    store: primary.clone(),
                    tracker: None,
                    state: None,
                    cwd: None,
                },
                crate::config::Entry {
                    name: Some("other".into()),
                    root: dir.path().join("other"),
                    store: alias.clone(),
                    tracker: None,
                    state: None,
                    cwd: None,
                },
            ],
            ..Config::default()
        };
        // The workspace store is an alias of the project store too.
        let env = StateEnv {
            explicit_db: None,
            default_store: &alias,
            cfg: &cfg,
            cwd: dir.path(),
        };
        let scope = StateScope::Project("proj".into());
        let store = env
            .open_for_id_or_key("https://github.com/acme/widgets/issues/1", Some(&scope))
            .expect("an alias of the primary is not opened again");
        assert_eq!(store.label(), primary.display().to_string());
    }

    /// The same-store branch, proved: a primary that stays on redb holds its file, so scanning
    /// the same file again through another path would fail (a redb file opens once at a time).
    /// A directory that cannot be written leaves the lone `.redb` unmigrated (no migration lock
    /// can be made), and a symlinked directory is the alias. Without the dedupe in
    /// `open_for_id_or_key` the scan of the alias is refused and the call errors.
    #[test]
    fn open_for_id_or_key_skips_a_scan_of_the_primary_held_on_redb() {
        use std::os::unix::fs::PermissionsExt;
        /// Gives the directory back its write bit so the temp dir can be removed.
        struct Writable(PathBuf);
        impl Drop for Writable {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let primary = real.join("proj.redb");
        drop(fl_store::open_redb(&primary).unwrap());
        let alias_dir = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias_dir).unwrap();
        let alias = alias_dir.join("proj.redb");
        let _writable = Writable(real.clone());
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::File::create(real.join("probe")).is_ok() {
            return; // a user who can write anywhere (root) cannot make the directory read-only
        }
        assert!(crate::same_store(&primary, &alias));

        let entry = |name: &str, store: &Path| crate::config::Entry {
            name: Some(name.into()),
            root: dir.path().join(name),
            store: store.to_path_buf(),
            tracker: None,
            state: None,
            cwd: None,
        };
        let cfg = Config {
            projects: vec![entry("proj", &primary), entry("other", &alias)],
            ..Config::default()
        };
        // The workspace store is the alias too.
        let env = StateEnv {
            explicit_db: None,
            default_store: &alias,
            cfg: &cfg,
            cwd: dir.path(),
        };
        let scope = StateScope::Project("proj".into());
        let store = env
            .open_for_id_or_key("https://github.com/acme/widgets/issues/1", Some(&scope))
            .expect("the alias of the primary is not scanned");
        // Still on redb: no .fls was made.
        assert_eq!(store.label(), primary.display().to_string());
        assert!(!real.join("proj.fls").exists());
    }

    #[test]
    fn test_render_state_markdown_and_truncation() {
        let store = MemStore::default();
        let area = store
            .upsert_area(
                AreaRecord::new(
                    "FerroLoop",
                    Some("FerroLoop/".into()),
                    "agent orchestration",
                    now_at(),
                    "ferris",
                )
                .unwrap(),
            )
            .unwrap();
        let ag1 = store
            .register_agent(RegisterAgentArgs {
                display_name: "Ferris".into(),
                email: None,
                project: "ferroloop".into(),
                role: AgentRole::Lead,
                extra_areas: vec![],
                create_area: false,
                replace: false,
                at: now_at(),
                by: "ferris".into(),
            })
            .unwrap();

        let handoff_entry = store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Project("ferroloop".into()),
                slug: "current".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Handoff(HandoffEntry {
                    summary: "Testing handoff".into(),
                    shipped: vec!["Format 6".into()],
                    in_flight: vec!["Task 6".into()],
                    blockers: vec![],
                    next_steps: vec!["Task 7".into()],
                    notes: None,
                }),
                at: now_at(),
                by: "ferris".into(),
            })
            .unwrap();

        let unpinned_mem = store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "architecture-decision".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Memory(MemoryEntry {
                    category: MemoryCategory::Decision,
                    basis: MemoryBasis::Ruling,
                    lifecycle: MemoryLifecycle::Active,
                    superseded_by: None,
                    pinned: false,
                    summary: "Use redb for local state".into(),
                    body: "Embedded pure Rust database.".into(),
                }),
                at: now_at(),
                by: "ferris".into(),
            })
            .unwrap();

        let pinned_mem = store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "pinned-warning".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Memory(MemoryEntry {
                    category: MemoryCategory::Warning,
                    basis: MemoryBasis::Ruling,
                    lifecycle: MemoryLifecycle::Active,
                    superseded_by: None,
                    pinned: true,
                    summary: "Do not delete root files".into(),
                    body: "Root files are protected.".into(),
                }),
                at: now_at(),
                by: "ferris".into(),
            })
            .unwrap();

        let areas = vec![area];
        let agents = vec![ag1];
        let entries = vec![handoff_entry, unpinned_mem, pinned_mem];

        // Normal render
        let full = render_state_markdown(&areas, &agents, &entries, 24_400, false);
        assert!(full.contains("# FerroLoop State Summary"));
        assert!(full.contains("## Active Agents"));
        assert!(full.contains("## Handoff"));
        assert!(full.contains("## Memory Index"));
        // Pinned/Warning should appear before unpinned Decision
        let pos_pinned = full.find("pinned-warning").unwrap();
        let pos_unpinned = full.find("architecture-decision").unwrap();
        assert!(pos_pinned < pos_unpinned);

        // Truncated render with small max_bytes
        let truncated = render_state_markdown(&areas, &agents, &entries, pos_unpinned + 20, false);
        assert!(truncated.contains("additional active memory entries omitted by byte cap"));
        assert!(truncated.len() <= pos_unpinned + 20);
    }

    #[test]
    fn test_resolve_state_in_store() {
        let store = MemStore::default();
        let e = store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "test-note".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Memory(MemoryEntry {
                    category: MemoryCategory::Note,
                    basis: MemoryBasis::Inference,
                    lifecycle: MemoryLifecycle::Active,
                    superseded_by: None,
                    pinned: false,
                    summary: "A test note".into(),
                    body: "Body text".into(),
                }),
                at: now_at(),
                by: "ferris".into(),
            })
            .unwrap();

        // Resolve by slug
        let found = resolve_state_in_store(
            &store,
            "test-note",
            Some(&StateScope::Workspace),
            Some(StateConcern::Memory),
        )
        .unwrap();
        assert_eq!(found.id, e.id);

        // Resolve by IRI
        let found_iri = resolve_state_in_store(&store, e.id.iri().as_str(), None, None).unwrap();
        assert_eq!(found_iri.id, e.id);

        // Resolve by handle
        let found_handle = resolve_state_in_store(&store, "1", None, None).unwrap();
        assert_eq!(found_handle.id, e.id);
    }

    #[test]
    fn search_surface_ranks_hits_and_reports_total_before_truncation() {
        let store = MemStore::default();
        let weak = store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "weak-note".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Memory(MemoryEntry {
                    category: MemoryCategory::Note,
                    basis: MemoryBasis::Inference,
                    lifecycle: MemoryLifecycle::Active,
                    superseded_by: None,
                    pinned: false,
                    summary: "ranking mention".into(),
                    body: "ranking appears once here".into(),
                }),
                at: now_at(),
                by: "ferris".into(),
            })
            .unwrap();
        let strong = store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: "strong-note".into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Memory(MemoryEntry {
                    category: MemoryCategory::Note,
                    basis: MemoryBasis::Inference,
                    lifecycle: MemoryLifecycle::Active,
                    superseded_by: None,
                    pinned: false,
                    summary: "search design".into(),
                    body: "bm25 ranking search ranking bm25 ranking".into(),
                }),
                at: now_at(),
                by: "ferris".into(),
            })
            .unwrap();
        // Weakest first: only the ranker can put `strong-note` on top.
        let entries = vec![weak, strong];

        let (hits, total) = collect_search_hits(&entries, "bm25 ranking", None, None, 1);
        assert_eq!(total, 2);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.slug, "strong-note");
        assert!(hits[0].score_millis > 0);
        assert!(hits[0].estimated_tokens > 0);
        assert!(!hits[0].snippet.is_empty());

        let value = search_result_json("bm25 ranking", &hits, total);
        assert_eq!(value["query"], "bm25 ranking");
        assert_eq!(value["total_hits"], 2);
        assert_eq!(value["hits"][0]["entry"]["slug"], "strong-note");
        assert!(value["hits"][0]["score_millis"].is_number());
        assert!(value["hits"][0]["snippet"].is_string());

        // Limit 0 returns no hits but keeps the full ranked count.
        let (none, total_zero) = collect_search_hits(&entries, "bm25 ranking", None, None, 0);
        assert!(none.is_empty());
        assert_eq!(total_zero, 2);

        // Human line carries score, scope, concern, slug, tokens, snippet.
        let line = format_search_hit(&hits[0]);
        assert!(line.contains("workspace"), "{line}");
        assert!(line.contains("memory"), "{line}");
        assert!(line.contains("strong-note"), "{line}");
        assert!(line.contains("tokens="), "{line}");
        assert!(!line.contains('\n'), "{line}");

        assert_eq!(effective_search_limit(None), DEFAULT_SEARCH_LIMIT);
        assert_eq!(effective_search_limit(Some(0)), 0);
        assert_eq!(effective_search_limit(Some(250)), MAX_SEARCH_LIMIT);
        assert!(parse_concern("bogus").is_err());
    }

    #[test]
    fn ranked_result_json_adds_mode_and_note_only_when_not_plain_bm25() {
        let plain = RankedHits {
            hits: Vec::new(),
            total_hits: 0,
            mode: SearchMode::Bm25,
            note: None,
            cause: None,
            pending_index: 0,
        };
        assert_eq!(
            ranked_result_json("q", &plain),
            search_result_json("q", &[], 0)
        );
        let knowledge = RankedHits {
            mode: SearchMode::Knowledge,
            ..plain.clone()
        };
        assert_eq!(ranked_result_json("q", &knowledge)["mode"], "knowledge");
        let fell_back = RankedHits {
            note: Some("why".into()),
            ..plain
        };
        let value = ranked_result_json("q", &fell_back);
        assert_eq!(value["note"], "why");
        assert!(value.get("mode").is_none());
    }

    #[test]
    fn explicit_modes_name_the_mode_that_ran_and_auto_keeps_the_old_shape() {
        let plain = RankedHits {
            hits: Vec::new(),
            total_hits: 0,
            mode: SearchMode::Bm25,
            note: None,
            cause: None,
            pending_index: 0,
        };
        assert_eq!(
            ranked_result_json_for("q", &plain, RankMode::Auto),
            ranked_result_json("q", &plain)
        );
        for (mode, name) in [
            (RankMode::Lexical, "lexical"),
            (RankMode::Vector, "vector"),
            (RankMode::Hybrid, "hybrid"),
        ] {
            assert_eq!(ranked_result_json_for("q", &plain, mode)["mode"], name);
            assert_eq!(RankMode::parse(name), Ok(mode));
        }
        assert!(RankMode::parse("bm25").is_err());
    }

    #[test]
    fn a_partly_indexed_vector_result_is_flagged_in_json_and_on_stderr() {
        let mut ranked = RankedHits {
            hits: Vec::new(),
            total_hits: 0,
            mode: SearchMode::Knowledge,
            note: None,
            cause: None,
            pending_index: 0,
        };
        assert_eq!(partial_index_line(&ranked), None);
        let whole = ranked_result_json_for("q", &ranked, RankMode::Vector);
        assert!(whole.get("partial").is_none() && whole.get("pending_index").is_none());
        ranked.pending_index = 7;
        let value = ranked_result_json_for("q", &ranked, RankMode::Hybrid);
        assert_eq!(value["partial"], true);
        assert_eq!(value["pending_index"], 7);
        assert_eq!(value["mode"], "hybrid");
        let line = partial_index_line(&ranked).unwrap();
        assert!(line.contains("7 entries not yet indexed"), "{line}");
    }

    #[test]
    fn the_cause_goes_to_stderr_only_when_bm25_answered_after_a_failure() {
        let plain = RankedHits {
            hits: Vec::new(),
            total_hits: 0,
            mode: SearchMode::Bm25,
            note: None,
            cause: None,
            pending_index: 0,
        };
        assert_eq!(index_unavailable_line(&plain), None);
        let failed = RankedHits {
            note: Some("why".into()),
            cause: Some("backend: disk\non fire".into()),
            pending_index: 0,
            ..plain.clone()
        };
        assert_eq!(
            index_unavailable_line(&failed).as_deref(),
            Some("fl: knowledge index unavailable: backend: disk on fire")
        );
        // The JSON never carries the cause.
        let json = ranked_result_json("q", &failed).to_string();
        assert!(!json.contains("disk"), "{json}");
        // A knowledge answer has nothing to report even if a cause were set.
        let knowledge = RankedHits {
            mode: SearchMode::Knowledge,
            ..failed
        };
        assert_eq!(index_unavailable_line(&knowledge), None);
    }

    #[test]
    fn test_parse_helpers() {
        assert_eq!(
            sensitivity_from_flags(true, false),
            SensitivityPatch::SetSensitive
        );
        assert_eq!(
            sensitivity_from_flags(false, true),
            SensitivityPatch::ClearSensitive
        );
        assert_eq!(
            sensitivity_from_flags(false, false),
            SensitivityPatch::Inherit
        );

        assert_eq!(parse_concern("memory").unwrap(), StateConcern::Memory);
        assert_eq!(parse_concern("handoff").unwrap(), StateConcern::Handoff);
        assert_eq!(parse_concern("runtime").unwrap(), StateConcern::Runtime);
        assert!(parse_concern("invalid").is_err());

        assert_eq!(
            parse_memory_category("decision").unwrap(),
            MemoryCategory::Decision
        );
        assert_eq!(parse_memory_basis("ruling").unwrap(), MemoryBasis::Ruling);
        assert_eq!(parse_merge_mode("append").unwrap(), MergeMode::Append);
    }
}
