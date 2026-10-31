use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use fl_core::agent::{AgentRecord, AgentStatus, AreaRecord, reports_to};
use fl_core::code_index::{
    CodeIndex, CodeSearchKind, CodeSearchOutcome, CodeSymbolHit, TokenGainSummary, load_token_gain,
    record_token_gain_index, record_token_gain_query, scan_code_index,
};
use fl_core::ferrostate::{
    WorkspaceImportReport, apply_workspace_import, create_state_checkpoint, resolve_workspace_root,
    scan_workspace_import,
};
use fl_core::ids::{Kind, RevisionId, StateId};
use fl_core::iri::Iri;
use fl_core::state::{
    HandoffEntry, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, MergeMode,
    RuntimeEntry, SensitivityPatch, StateConcern, StateEntry, StatePayload, StateScope,
    StateSearchHit, UpsertStateArgs, search_state_entries, validate_slug,
};
use fl_core::store::{AgentRegistry, Handles, StateStore};
use fl_exec::state_sync::{sync_state, verify_state};
use fl_store::RedbStore;

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
/// Stores are local, so fetching the full ranked list is cheap.
pub fn collect_search_hits(
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> (Vec<StateSearchHit>, usize) {
    let all = search_state_entries(entries, query, scope, concern, usize::MAX);
    let total_hits = all.len();
    let hits: Vec<StateSearchHit> = all.into_iter().take(limit).collect();
    (hits, total_hits)
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
/// (`CodeSearchResponse` in `ui/src/types/api.ts`).
pub fn code_search_response_json(
    query: &str,
    kind: CodeSearchKind,
    symbols: &[CodeSymbolHit],
    tokens_saved_this_query: u64,
    token_gain: TokenGainSummary,
) -> serde_json::Value {
    serde_json::json!({
        "query": query,
        "kind": kind.as_wire(),
        "symbols": symbols,
        "tokens_saved_this_query": tokens_saved_this_query,
        "token_gain": token_gain,
    })
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
/// gain in `store`, returning the outcome and the updated ledger.
///
/// A `limit` of `0` returns no symbols and records nothing; the returned
/// ledger is then the currently stored one ([`load_token_gain`]). Ledger
/// write failures (malformed/sensitive ledger, store errors) propagate.
pub fn search_and_record(
    store: &impl StateStore,
    index: &CodeIndex,
    kind: CodeSearchKind,
    query: &str,
    limit: usize,
) -> Result<(CodeSearchOutcome, TokenGainSummary), String> {
    let outcome = index.search(kind, query, limit);
    if limit == 0 {
        return Ok((outcome, load_token_gain(store)));
    }
    let token_gain = record_token_gain_query(
        store,
        outcome.tokens_saved_this_query,
        outcome.files_indexed,
        outcome.symbols_indexed,
        now_at(),
        "fl",
    )
    .map_err(|e| e.to_string())?;
    Ok((outcome, token_gain))
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
    /// Search state entries with BM25 ranking.
    Search {
        /// Search query text.
        query: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        concern: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
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
    /// Create or update a `Handoff` snapshot (`--key` defaults to `"current"`).
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
    if let Ok(r) = id_or_key.parse::<Ref>() {
        let iri = match r {
            Ref::Iri(iri) => iri,
            Ref::Handle(h) => store
                .resolve_handle(Kind::State, h)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no state entry with handle {h} in this store"))?,
            Ref::Issue(_) => {
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

    pub fn open_for_scope(&self, scope: Option<&StateScope>) -> Result<RedbStore, String> {
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
        RedbStore::open(&path).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn open_for_id_or_key(
        &self,
        id_or_key: &str,
        scope: Option<&StateScope>,
    ) -> Result<RedbStore, String> {
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
        let primary = RedbStore::open(&primary_path)
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
        if ws_path != primary_path && ws_path.exists() {
            let other = RedbStore::open(&ws_path).map_err(|e| e.to_string())?;
            if other.owns(&iri).map_err(|e| e.to_string())? {
                return Ok(other);
            }
        }
        for entry in &self.cfg.projects {
            let path = &entry.store;
            if path == &primary_path || !path.exists() {
                continue;
            }
            let other = RedbStore::open(path).map_err(|e| e.to_string())?;
            if other.owns(&iri).map_err(|e| e.to_string())? {
                return Ok(other);
            }
        }
        Ok(primary)
    }
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
            let (hits, total_hits) = collect_search_hits(
                &entries,
                &query,
                parsed_scope.as_ref(),
                parsed_concern,
                effective_limit,
            );
            if json {
                let out = search_result_json(&query, &hits, total_hits);
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else if hits.is_empty() {
                println!("no hits for `{query}`");
            } else {
                for hit in &hits {
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
    let (outcome, token_gain) = search_and_record(&store, &index, kind, query, effective_limit)?;
    if json {
        let out = code_search_response_json(
            query,
            kind,
            &outcome.symbols,
            outcome.tokens_saved_this_query,
            token_gain,
        );
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else if outcome.symbols.is_empty() {
        println!("no hits for `{query}`");
    } else {
        for hit in &outcome.symbols {
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
