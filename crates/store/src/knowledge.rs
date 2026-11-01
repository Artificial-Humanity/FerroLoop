//! State-entry search over either store engine.
//!
//! Two legs are fused by reciprocal rank:
//!
//! * the **lexical leg** is the exact field-weighted BM25 of
//!   `fl_core::state::search_state_entries`, run over *every* entry the caller
//!   passes, on both engines. It never drops a match, applies the pinned and
//!   lifecycle weights, and is the whole answer on redb;
//! * the **vector leg** exists on FerroStore only. It queries FerroStore's
//!   HNSW index of `fl_core::embed` vectors, so a typo or a different word
//!   form can still find an entry.
//!
//! Fused score = `1/(60 + rank_lex) + 1/(60 + rank_vec)` (ranks are 1-based;
//! an id missing from a leg adds nothing for it), multiplied by
//! `fl_core::rank_weight`. Every lexical hit is kept. A vector-only hit is
//! kept only if the entry sits close to the query in the embedding space
//! (the embedder is a hashed lexical/subword model, not a learned semantic
//! one, so a bare nearest neighbour is noise).
//!
//! No document collection of entry text is mirrored: lexical search needs
//! none, so no text of an entry is copied into the store. What is written is
//! one vector per entry in a per-embedding-space collection (metadata `rev`,
//! the entry's head revision, and `space`, both as numbers), plus one small
//! index document per collection mapping entry id to `rev`. Every read of the
//! engine costs time proportional to the store, so staleness is decided from
//! that one document in one read, never by probing vectors one by one.
//!
//! **Sensitive entries are never indexed** (`sensitive || ever_sensitive`):
//! a vector is a lossy but real copy of the text, so those entries are found
//! by the lexical leg only, and a vector left over from before an entry
//! became sensitive is deleted on the next [`IndexPolicy::Sync`] search.
//! Vector hits for them are also dropped at read time.
//!
//! The caller decides which entries may be shown: only ids in the `entries`
//! slice are ever returned, however much the store holds. A failure on the
//! FerroStore path never fails the search; it falls back to BM25 and sets a
//! fixed [`RankedHits::note`].
//!
//! Limits of the vector leg: it asks the index for the `max(200, entries)`
//! nearest vectors of the whole collection, so a vector-only match can be
//! missed in a store holding many more indexed entries than were passed, and
//! an entry not yet indexed (see [`IndexPolicy`]) is found lexically only.

use crate::{AnyStore, FerroStore, VectorMetric, VectorStorage};
use fl_core::{
    Embedder, HashingEmbedder, StateConcern, StateEntry, StatePayload, StateScope, StateSearchHit,
    StoreError, cosine, estimate_tokens, rank_weight, search_state_entries,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Smallest number of nearest vectors asked of the index.
const MIN_CANDIDATES: usize = 200;
/// Reciprocal-rank-fusion constant.
const RRF_K: f64 = 60.0;
/// A vector-only hit needs at least this cosine to the query, measured
/// against the entry's summary or its whole text.
const MIN_VECTOR_COSINE: f32 = 0.25;
/// Longest fallback snippet, in characters.
const SNIPPET_CHARS: usize = 160;
/// Vectors one [`rank_state_entries`] call may write.
pub const DEFAULT_SYNC_BUDGET: usize = 256;
/// `score_millis` is the fused score x this. A hit ranked first in one leg
/// scores about 16, first in both about 33, and the floor is 1. It is a
/// different scale from the raw-BM25 `score_millis` of the redb ranker.
const SCORE_SCALE: f64 = 1000.0;
/// Shown when the knowledge path failed. Fixed text: engine errors are not
/// echoed to callers.
const FALLBACK_NOTE: &str = "knowledge index unavailable; ranked with BM25";
/// Document collection holding one id -> revision index per vector collection.
/// It stores hashes only, never entry text.
const INDEX_DOC_COLLECTION: &str = "state_vec_index";
/// Start of the engine's error for a missing vector collection.
const MISSING_COLLECTION: &str = "no vector collection";

/// Which ranker produced a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Field-weighted BM25 over the given entries.
    Bm25,
    /// BM25 fused with FerroStore vector search.
    Knowledge,
}

impl SearchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bm25 => "bm25",
            Self::Knowledge => "knowledge",
        }
    }
}

/// Whether a search may write to the vector index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPolicy {
    /// Bring the index up to date first. At most `budget` new or changed
    /// vectors are written per call; the rest are found by the lexical leg
    /// and indexed by later searches. Leftover vectors of entries that became
    /// sensitive are deleted (not counted against the budget).
    Sync { budget: usize },
    /// Never write. The vector leg runs only if its collection exists.
    ReadOnly,
}

/// A ranked search result.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedHits {
    /// At most `limit` hits, best first.
    pub hits: Vec<StateSearchHit>,
    /// Hits kept by the ranker before truncation to `limit`. It is not
    /// capped: every lexical match is counted.
    pub total_hits: usize,
    pub mode: SearchMode,
    /// Set when the knowledge path failed and BM25 answered instead.
    pub note: Option<String>,
}

/// What an indexing pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct IndexStats {
    /// Vectors upserted.
    pub written: usize,
    /// Vectors deleted because their entry is now sensitive.
    pub deleted: usize,
    /// Changed or new entries left for a later search by the budget.
    pub pending: usize,
}

/// Rank `entries` with BM25 only (the redb behaviour).
pub fn rank_bm25(
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> RankedHits {
    bm25_ranked(
        search_state_entries(entries, query, scope, concern, usize::MAX),
        limit,
    )
}

fn bm25_ranked(all: Vec<StateSearchHit>, limit: usize) -> RankedHits {
    let total_hits = all.len();
    RankedHits {
        hits: all.into_iter().take(limit).collect(),
        total_hits,
        mode: SearchMode::Bm25,
        note: None,
    }
}

fn default_embedder() -> &'static HashingEmbedder {
    static EMBEDDER: OnceLock<HashingEmbedder> = OnceLock::new();
    EMBEDDER.get_or_init(HashingEmbedder::default)
}

/// Rank `entries` against `query` on `store` with the default embedder,
/// indexing up to [`DEFAULT_SYNC_BUDGET`] changed entries first.
pub fn rank_state_entries(
    store: &AnyStore,
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> RankedHits {
    rank_state_entries_with(
        store,
        default_embedder(),
        IndexPolicy::Sync {
            budget: DEFAULT_SYNC_BUDGET,
        },
        entries,
        query,
        scope,
        concern,
        limit,
    )
}

/// [`rank_state_entries`] that never writes to the store: for a read request.
pub fn rank_state_entries_read_only(
    store: &AnyStore,
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> RankedHits {
    rank_state_entries_with(
        store,
        default_embedder(),
        IndexPolicy::ReadOnly,
        entries,
        query,
        scope,
        concern,
        limit,
    )
}

/// [`rank_state_entries`] with a chosen embedder and index policy.
#[allow(clippy::too_many_arguments)]
pub fn rank_state_entries_with(
    store: &AnyStore,
    embedder: &dyn Embedder,
    policy: IndexPolicy,
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> RankedHits {
    rank_with_stats(
        store, embedder, policy, entries, query, scope, concern, limit,
    )
    .0
}

/// The vector collection for an embedding space: another space or dimension
/// never shares one.
pub fn vector_collection_name(embedder: &dyn Embedder) -> String {
    let digest = Sha256::digest(embedder.space_id().as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("state_vec_{hex}")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rank_with_stats(
    store: &AnyStore,
    embedder: &dyn Embedder,
    policy: IndexPolicy,
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> (RankedHits, IndexStats) {
    let lexical = search_state_entries(entries, query, scope, concern, usize::MAX);
    let Some(ferro) = store.as_ferro() else {
        return (bm25_ranked(lexical, limit), IndexStats::default());
    };
    let mut stats = IndexStats::default();
    match rank_knowledge(
        ferro, embedder, policy, entries, query, scope, concern, limit, &lexical, &mut stats,
    ) {
        Ok(Some(ranked)) => (ranked, stats),
        Ok(None) => (bm25_ranked(lexical, limit), stats),
        Err(_) => {
            let mut ranked = bm25_ranked(lexical, limit);
            ranked.note = Some(FALLBACK_NOTE.to_string());
            (ranked, stats)
        }
    }
}

/// `Ok(None)` when the query has nothing to embed and the BM25 answer is the
/// right one.
#[allow(clippy::too_many_arguments)]
fn rank_knowledge(
    ferro: &FerroStore,
    embedder: &dyn Embedder,
    policy: IndexPolicy,
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
    lexical: &[StateSearchHit],
    stats: &mut IndexStats,
) -> Result<Option<RankedHits>, StoreError> {
    if query.trim().is_empty() {
        return Ok(None);
    }
    let query_vec = embedder.embed(query);
    if is_zero(&query_vec) {
        return Ok(None);
    }
    let eligible: Vec<&StateEntry> = entries
        .iter()
        .filter(|e| scope.is_none_or(|s| &e.scope == s))
        .filter(|e| concern.is_none_or(|c| e.concern == c))
        .collect();
    if eligible.is_empty() {
        return Ok(Some(RankedHits {
            hits: Vec::new(),
            total_hits: 0,
            mode: SearchMode::Knowledge,
            note: None,
        }));
    }
    let collection = vector_collection_name(embedder);
    if let IndexPolicy::Sync { budget } = policy {
        *stats = sync_index(ferro, embedder, &collection, &eligible, budget)?;
    }
    // A collection that does not exist yet (read-only before any sync, or
    // nothing indexable) is an empty vector leg, not a failure.
    let vector_order = nearest(ferro, &collection, &query_vec, &eligible, entries.len())?;

    // Rank both legs by id.
    let vec_rank: HashMap<&str, usize> = vector_order
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i + 1))
        .collect();
    let lex_ids: std::collections::HashSet<&str> =
        lexical.iter().map(|h| h.entry.id.as_str()).collect();

    let mut kept: Vec<(f64, StateSearchHit)> = Vec::new();
    for (i, hit) in lexical.iter().enumerate() {
        let mut fused = rrf(i + 1);
        if let Some(rv) = vec_rank.get(hit.entry.id.as_str()) {
            fused += rrf(*rv);
        }
        fused *= rank_weight(&hit.entry);
        let mut hit = hit.clone();
        hit.score_millis = score_millis(fused);
        kept.push((fused, hit));
    }
    for (i, entry) in vector_order.iter().enumerate() {
        if lex_ids.contains(entry.id.as_str()) {
            continue;
        }
        let text = searchable_text(entry);
        // A short query is diluted by a long body, so the title (the
        // summary, else the slug) counts on its own as well as the whole text.
        let near = cosine(&query_vec, &embedder.embed(&text))
            .max(cosine(&query_vec, &embedder.embed(&title_text(entry))));
        if near < MIN_VECTOR_COSINE {
            continue;
        }
        let fused = rrf(i + 1) * rank_weight(entry);
        kept.push((
            fused,
            StateSearchHit {
                snippet: fallback_snippet(entry),
                estimated_tokens: estimate_tokens(&text),
                entry: (*entry).clone(),
                score_millis: score_millis(fused),
                matched_terms: Vec::new(),
            },
        ));
    }
    kept.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| {
                b.1.entry
                    .updated_at
                    .unix_millis()
                    .cmp(&a.1.entry.updated_at.unix_millis())
            })
            .then_with(|| a.1.entry.slug.cmp(&b.1.entry.slug))
            .then_with(|| a.1.entry.id.as_str().cmp(b.1.entry.id.as_str()))
    });
    let total_hits = kept.len();
    Ok(Some(RankedHits {
        hits: kept.into_iter().take(limit).map(|(_, h)| h).collect(),
        total_hits,
        mode: SearchMode::Knowledge,
        note: None,
    }))
}

fn rrf(rank: usize) -> f64 {
    1.0 / (RRF_K + rank as f64)
}

fn score_millis(fused: f64) -> u64 {
    ((fused * SCORE_SCALE).round() as u64).max(1)
}

fn is_zero(v: &[f32]) -> bool {
    v.iter().all(|x| *x == 0.0)
}

/// Whether an entry may have a vector in the index.
fn indexable(entry: &StateEntry) -> bool {
    !(entry.sensitive || entry.ever_sensitive)
}

/// The indexable eligible entries nearest to the query, best first. The index
/// is asked for `max(200, passed)` ids of the whole collection; ids that are
/// not eligible (other scopes, hidden or sensitive entries) are dropped. A
/// missing collection gives an empty list; any other error is returned.
fn nearest<'a>(
    ferro: &FerroStore,
    collection: &str,
    query_vec: &[f32],
    eligible: &[&'a StateEntry],
    passed: usize,
) -> Result<Vec<&'a StateEntry>, StoreError> {
    let k = passed.max(MIN_CANDIDATES);
    let Some(mut found) = missing_as_none(ferro.search_vectors(collection, query_vec, k, None))?
    else {
        return Ok(Vec::new());
    };
    found.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut by_id: HashMap<&str, &'a StateEntry> = HashMap::new();
    for entry in eligible.iter().filter(|e| indexable(e)) {
        by_id.entry(entry.id.as_str()).or_insert(entry);
    }
    let mut seen = std::collections::HashSet::new();
    Ok(found
        .iter()
        .filter_map(|(id, _)| by_id.get(id.as_str()).copied())
        .filter(|e| seen.insert(e.id.as_str()))
        .collect())
}

/// Bring the vector index up to date for `entries`, writing at most `budget`
/// vectors. One read of the index document says which entries are current; an
/// entry whose recorded `rev` and `space` match is left alone. An entry that
/// is now sensitive loses its vector. A zero vector (text with no features)
/// is not stored. The vectors are written before the index document, so an
/// interrupted pass only repeats work.
pub(crate) fn sync_index(
    ferro: &FerroStore,
    embedder: &dyn Embedder,
    collection: &str,
    entries: &[&StateEntry],
    budget: usize,
) -> Result<IndexStats, StoreError> {
    let space = stamp(&embedder.space_id());
    let mut index = IndexDoc::load(ferro, collection, space)?;
    let mut stats = IndexStats::default();
    let mut ready = false;
    let mut changed = false;
    for entry in entries {
        let id = entry.id.as_str();
        if !indexable(entry) {
            if index.revs.remove(id).is_some() {
                ferro.delete_vector(collection, id)?;
                stats.deleted += 1;
                changed = true;
            }
            continue;
        }
        let rev = stamp(entry.head_revision.as_str());
        if index.revs.get(id) == Some(&rev) {
            continue;
        }
        if stats.written >= budget {
            stats.pending += 1;
            continue;
        }
        let values = embedder.embed(&searchable_text(entry));
        if is_zero(&values) {
            continue;
        }
        if !ready {
            ensure_vector_collection(ferro, embedder, collection)?;
            ready = true;
        }
        let meta = json!({ "rev": rev, "space": space });
        ferro.upsert_vector(collection, id, &values, Some(&meta))?;
        index.revs.insert(id.to_string(), rev);
        stats.written += 1;
        changed = true;
    }
    if changed {
        index.store(ferro, collection, space)?;
    }
    Ok(stats)
}

/// Entry id -> `rev` of the vector indexed for it, in one document per
/// collection. Holds hashes only.
struct IndexDoc {
    revs: HashMap<String, u64>,
}

impl IndexDoc {
    fn load(ferro: &FerroStore, collection: &str, space: u64) -> Result<Self, StoreError> {
        let mut revs = HashMap::new();
        if let Some(doc) = ferro.get_doc(INDEX_DOC_COLLECTION, collection)?
            && doc.get("space").and_then(Value::as_u64) == Some(space)
            && let Some(map) = doc.get("revs").and_then(Value::as_object)
        {
            for (id, rev) in map {
                if let Some(rev) = rev.as_u64() {
                    revs.insert(id.clone(), rev);
                }
            }
        }
        Ok(Self { revs })
    }

    fn store(&self, ferro: &FerroStore, collection: &str, space: u64) -> Result<(), StoreError> {
        let revs: serde_json::Map<String, Value> = self
            .revs
            .iter()
            .map(|(id, rev)| (id.clone(), json!(rev)))
            .collect();
        ferro.put_doc(
            INDEX_DOC_COLLECTION,
            collection,
            &json!({ "space": space, "revs": revs }),
        )
    }
}

/// `Ok(None)` only for the engine's own "no vector collection" error; any
/// other failure is returned.
fn missing_as_none<T>(result: Result<T, StoreError>) -> Result<Option<T>, StoreError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if is_missing_collection(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

fn is_missing_collection(err: &StoreError) -> bool {
    matches!(err, StoreError::Backend(msg) if msg.starts_with(MISSING_COLLECTION))
}

fn ensure_vector_collection(
    ferro: &FerroStore,
    embedder: &dyn Embedder,
    name: &str,
) -> Result<(), StoreError> {
    if missing_as_none(ferro.get_vector(name, "probe"))?.is_some() {
        return Ok(());
    }
    let created = ferro.create_vector_collection(
        name,
        embedder.dim() as u32,
        VectorStorage::F32,
        VectorMetric::Cosine,
        16,
        200,
        64,
    );
    match created {
        Ok(()) => Ok(()),
        // Another process may have created it between the probe and now.
        Err(err) => match missing_as_none(ferro.get_vector(name, "probe")) {
            Ok(Some(_)) => Ok(()),
            _ => Err(err),
        },
    }
}

fn stamp(text: &str) -> u64 {
    let digest = Sha256::digest(text.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    // Keep within the range every JSON consumer reads exactly.
    u64::from_be_bytes(bytes) & ((1 << 53) - 1)
}

/// The same fields BM25 searches: `(summary, body)` of a payload.
fn summary_and_body(payload: &StatePayload) -> (String, String) {
    match payload {
        StatePayload::Memory(m) => (m.summary.clone(), m.body.clone()),
        StatePayload::Handoff(h) => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(notes) = &h.notes {
                parts.push(notes.clone());
            }
            parts.extend(h.shipped.iter().cloned());
            parts.extend(h.in_flight.iter().cloned());
            parts.extend(h.blockers.iter().cloned());
            parts.extend(h.next_steps.iter().cloned());
            (h.summary.clone(), parts.join("\n"))
        }
        StatePayload::Runtime(r) => (
            String::new(),
            serde_json::to_string(&r.value).unwrap_or_default(),
        ),
    }
}

fn searchable_text(entry: &StateEntry) -> String {
    let (summary, body) = summary_and_body(&entry.payload);
    [entry.slug.as_str(), summary.as_str(), body.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn title_text(entry: &StateEntry) -> String {
    let (summary, _) = summary_and_body(&entry.payload);
    if summary.is_empty() {
        entry.slug.clone()
    } else {
        summary
    }
}

fn fallback_snippet(entry: &StateEntry) -> String {
    let (summary, body) = summary_and_body(&entry.payload);
    let source = if summary.is_empty() { &body } else { &summary };
    let one_line = source.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(SNIPPET_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendKind, StoreOptions, open_redb, open_store_with};
    use fl_core::{
        At, MemoryBasis, MemoryCategory, MemoryEntry, MemoryLifecycle, SensitivityPatch,
        StateStore, UpsertStateArgs,
    };
    use tempfile::TempDir;

    fn open_ferro(dir: &TempDir) -> AnyStore {
        open_store_with(
            &dir.path().join("k.redb"),
            &StoreOptions {
                backend: Some(BackendKind::Ferro),
                ..StoreOptions::default()
            },
        )
        .unwrap()
    }

    struct Spec<'a> {
        slug: &'a str,
        summary: &'a str,
        body: &'a str,
        at_ms: u64,
        pinned: bool,
        lifecycle: MemoryLifecycle,
        sensitive: bool,
    }

    fn spec<'a>(slug: &'a str, summary: &'a str, body: &'a str, at_ms: u64) -> Spec<'a> {
        Spec {
            slug,
            summary,
            body,
            at_ms,
            pinned: false,
            lifecycle: MemoryLifecycle::Active,
            sensitive: false,
        }
    }

    fn put_spec(store: &AnyStore, s: Spec<'_>) -> StateEntry {
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: s.slug.into(),
                sensitivity: if s.sensitive {
                    SensitivityPatch::SetSensitive
                } else {
                    SensitivityPatch::Inherit
                },
                payload: StatePayload::Memory(MemoryEntry {
                    category: MemoryCategory::Note,
                    basis: MemoryBasis::Inference,
                    lifecycle: s.lifecycle,
                    superseded_by: None,
                    pinned: s.pinned,
                    summary: s.summary.into(),
                    body: s.body.into(),
                }),
                at: At::from_unix_millis(s.at_ms),
                by: "tester".into(),
            })
            .unwrap()
    }

    fn put(store: &AnyStore, slug: &str, summary: &str, body: &str, at_ms: u64) -> StateEntry {
        put_spec(store, spec(slug, summary, body, at_ms))
    }

    const FIXTURE: [(&str, &str, &str); 4] = [
        (
            "build-cache",
            "checkpoint the build cache",
            "Nightly job writes a checkpoint of the build cache to disk.",
        ),
        (
            "hub-runbook",
            "deploy runbook for the hub",
            "Steps to roll out the hub service and verify health afterwards.",
        ),
        (
            "lunch-notes",
            "meeting notes about lunch",
            "The team agreed on sandwiches for the offsite.",
        ),
        (
            "gpu-queue",
            "gpu queue arbitration",
            "Shorter jobs first, then priority by project.",
        ),
    ];

    fn seed(store: &AnyStore) -> Vec<StateEntry> {
        for (i, (slug, summary, body)) in FIXTURE.iter().enumerate() {
            put(store, slug, summary, body, 1_000 + i as u64);
        }
        store.list_state(None, None).unwrap()
    }

    fn slugs(r: &RankedHits) -> Vec<&str> {
        r.hits.iter().map(|h| h.entry.slug.as_str()).collect()
    }

    fn pair() -> (TempDir, AnyStore, TempDir, AnyStore) {
        let rd = TempDir::new().unwrap();
        let redb = open_redb(&rd.path().join("k.redb")).unwrap();
        let fd = TempDir::new().unwrap();
        let ferro = open_ferro(&fd);
        (rd, redb, fd, ferro)
    }

    fn has_collection(ferro: &FerroStore, name: &str) -> bool {
        missing_as_none(ferro.get_vector(name, "probe"))
            .unwrap()
            .is_some()
    }

    const SYNC: IndexPolicy = IndexPolicy::Sync {
        budget: DEFAULT_SYNC_BUDGET,
    };

    fn run(
        store: &AnyStore,
        policy: IndexPolicy,
        entries: &[StateEntry],
        query: &str,
    ) -> (RankedHits, IndexStats) {
        rank_with_stats(
            store,
            default_embedder(),
            policy,
            entries,
            query,
            None,
            None,
            10,
        )
    }

    #[test]
    fn exact_keyword_ranks_the_same_top_entry_on_both_engines() {
        let (_rd, redb, _fd, ferro) = pair();
        let redb_entries = seed(&redb);
        let ferro_entries = seed(&ferro);
        for q in ["build cache", "hub runbook", "sandwiches", "gpu queue"] {
            let a = rank_state_entries(&redb, &redb_entries, q, None, None, 5);
            let b = rank_state_entries(&ferro, &ferro_entries, q, None, None, 5);
            assert_eq!(a.mode, SearchMode::Bm25);
            assert_eq!(b.mode, SearchMode::Knowledge, "{q}: {:?}", b.note);
            assert!(!a.hits.is_empty(), "{q}");
            assert_eq!(
                a.hits[0].entry.slug, b.hits[0].entry.slug,
                "top entry differs for `{q}`"
            );
            assert_eq!(a.note, None);
            assert_eq!(b.note, None);
        }
    }

    #[test]
    fn redb_result_is_exactly_the_bm25_ranking() {
        let (_rd, redb, _fd, _ferro) = pair();
        let entries = seed(&redb);
        let ranked = rank_state_entries(&redb, &entries, "cache", None, None, 1);
        let all = search_state_entries(&entries, "cache", None, None, usize::MAX);
        assert_eq!(ranked.total_hits, all.len());
        assert_eq!(ranked.hits, all.into_iter().take(1).collect::<Vec<_>>());
        // limit 0 keeps the full ranked count, as before.
        let none = rank_state_entries(&redb, &entries, "cache", None, None, 0);
        assert!(none.hits.is_empty());
        assert_eq!(none.total_hits, ranked.total_hits);
        // The read-only entry point is the same on redb.
        let ro = rank_state_entries_read_only(&redb, &entries, "cache", None, None, 1);
        assert_eq!(ro, ranked);
    }

    #[test]
    fn typo_and_stem_queries_find_the_entry_on_ferro_only() {
        let (_rd, redb, _fd, ferro) = pair();
        let redb_entries = seed(&redb);
        let ferro_entries = seed(&ferro);
        // A lone misspelled word scores about 0.26 against the summary: just
        // over the 0.25 bar.
        for q in ["checkpoints", "checkpointing", "chekpoint"] {
            let r = rank_state_entries(&redb, &redb_entries, q, None, None, 5);
            assert!(r.hits.is_empty(), "redb found `{q}`: {:?}", slugs(&r));
            let f = rank_state_entries(&ferro, &ferro_entries, q, None, None, 5);
            assert_eq!(f.mode, SearchMode::Knowledge, "{:?}", f.note);
            assert_eq!(slugs(&f).first(), Some(&"build-cache"), "query `{q}`");
            let hit = &f.hits[0];
            assert!(hit.matched_terms.is_empty());
            assert!(!hit.snippet.is_empty() && !hit.snippet.contains('\n'));
            assert!(hit.snippet.chars().count() <= SNIPPET_CHARS);
            assert!(hit.score_millis >= 1);
            assert!(hit.estimated_tokens > 0);
        }
    }

    #[test]
    fn unrelated_query_returns_nothing_on_ferro() {
        let d = TempDir::new().unwrap();
        let ferro = open_ferro(&d);
        let entries = seed(&ferro);
        for q in ["zebra quantum xylophone", "kubernetes"] {
            let r = rank_state_entries(&ferro, &entries, q, None, None, 5);
            assert_eq!(r.mode, SearchMode::Knowledge, "{:?}", r.note);
            assert!(r.hits.is_empty(), "`{q}` found {:?}", slugs(&r));
            assert_eq!(r.total_hits, 0);
        }
        // Empty and symbol-only queries are empty, like BM25.
        for q in ["", "   ", "!!!"] {
            let r = rank_state_entries(&ferro, &entries, q, None, None, 5);
            assert!(r.hits.is_empty() && r.total_hits == 0, "`{q}`");
        }
    }

    #[test]
    fn limit_truncates_hits_but_keeps_total_on_both_engines() {
        let (_rd, redb, _fd, ferro) = pair();
        for store in [&redb, &ferro] {
            for i in 0..4u64 {
                put(
                    store,
                    &format!("cache-note-{i}"),
                    &format!("cache tuning note {i}"),
                    "cache sizes and eviction",
                    2_000 + i,
                );
            }
            put(store, "other", "lunch plans", "sandwiches", 3_000);
            let entries = store.list_state(None, None).unwrap();
            let all = rank_state_entries(store, &entries, "cache", None, None, 10);
            assert_eq!(all.total_hits, 4, "{:?}", slugs(&all));
            let one = rank_state_entries(store, &entries, "cache", None, None, 1);
            assert_eq!(one.hits.len(), 1);
            assert_eq!(one.total_hits, 4);
            assert_eq!(one.hits[0], all.hits[0]);
            let zero = rank_state_entries(store, &entries, "cache", None, None, 0);
            assert!(zero.hits.is_empty());
            assert_eq!(zero.total_hits, 4);
        }
    }

    #[test]
    fn scope_and_concern_filters_apply_on_both_engines() {
        let (_rd, redb, _fd, ferro) = pair();
        for store in [&redb, &ferro] {
            put(store, "ws-cache", "workspace cache note", "cache", 1_000);
            store
                .upsert_state(UpsertStateArgs {
                    scope: StateScope::Project("alpha".into()),
                    slug: "proj-cache".into(),
                    sensitivity: SensitivityPatch::Inherit,
                    payload: StatePayload::Memory(MemoryEntry {
                        category: MemoryCategory::Note,
                        basis: MemoryBasis::Inference,
                        lifecycle: MemoryLifecycle::Active,
                        superseded_by: None,
                        pinned: false,
                        summary: "project cache note".into(),
                        body: "cache".into(),
                    }),
                    at: At::from_unix_millis(1_001),
                    by: "tester".into(),
                })
                .unwrap();
            let entries = store.list_state(None, None).unwrap();
            let scope = StateScope::Project("alpha".into());
            let r = rank_state_entries(store, &entries, "cache", Some(&scope), None, 5);
            assert_eq!(slugs(&r), vec!["proj-cache"]);
            let r = rank_state_entries(
                store,
                &entries,
                "cache",
                None,
                Some(StateConcern::Handoff),
                5,
            );
            assert!(r.hits.is_empty());
            // The vector leg honours the filters too: a typo finds only the
            // project entry under the project scope.
            let r = rank_state_entries(store, &entries, "chache", Some(&scope), None, 5);
            assert!(r.hits.iter().all(|h| h.entry.slug == "proj-cache"));
        }
    }

    #[test]
    fn indexing_is_idempotent_and_updates_and_sensitivity_are_counted() {
        let d = TempDir::new().unwrap();
        let store = open_ferro(&d);
        let mut entries = seed(&store);
        let ferro = store.as_ferro().unwrap();
        let col = vector_collection_name(default_embedder());

        // A sensitive entry is found lexically but never indexed.
        let mut s = spec("vault-notes", "vault rotation notes", "vaultsecret", 1_500);
        s.sensitive = true;
        let secret = put_spec(&store, s);
        entries.push(secret.clone());

        let (_, first) = run(&store, SYNC, &entries, "build cache");
        assert_eq!(first.written, 4);
        assert_eq!(first.deleted, 0);
        assert!(
            ferro
                .get_vector(&col, secret.id.as_str())
                .unwrap()
                .is_none()
        );
        let (_, second) = run(&store, SYNC, &entries, "build cache");
        assert_eq!(second, IndexStats::default());

        let lexical = rank_state_entries(&store, &entries, "vaultsecret", None, None, 5);
        assert_eq!(slugs(&lexical), vec!["vault-notes"]);
        // ...but not by a typo, which only the vector leg could find.
        let typo = rank_state_entries(&store, &entries, "vaultsecrt", None, None, 5);
        assert!(typo.hits.is_empty(), "{:?}", slugs(&typo));

        // An update gets a new head revision: exactly one write, found by its new text.
        let before = entries.iter().find(|e| e.slug == "lunch-notes").unwrap();
        let updated = put(
            &store,
            "lunch-notes",
            "meeting notes about tokenizer",
            "Swapped the tokenizer vocabulary for the offsite.",
            9_000,
        );
        assert_ne!(updated.head_revision, before.head_revision);
        let mut entries = store.list_state(None, None).unwrap();
        let (new, stats) = run(&store, SYNC, &entries, "tokenizer");
        assert_eq!(stats.written, 1);
        assert_eq!(slugs(&new), vec!["lunch-notes"]);
        let (_, again) = run(&store, SYNC, &entries, "tokenizer");
        assert_eq!(again.written, 0);
        let (old, _) = run(&store, SYNC, &entries, "sandwiches");
        assert!(old.hits.is_empty(), "stale text found {:?}", slugs(&old));

        // An entry that becomes sensitive loses its vector; nothing is written.
        let mut s = spec(
            "lunch-notes",
            "meeting notes about tokenizer",
            "Swapped the tokenizer vocabulary for the offsite.",
            9_100,
        );
        s.sensitive = true;
        let flipped = put_spec(&store, s);
        assert!(
            ferro
                .get_vector(&col, flipped.id.as_str())
                .unwrap()
                .is_some()
        );
        entries = store.list_state(None, None).unwrap();
        let (_, stats) = run(&store, SYNC, &entries, "tokenizer");
        assert_eq!((stats.written, stats.deleted), (0, 1));
        assert!(
            ferro
                .get_vector(&col, flipped.id.as_str())
                .unwrap()
                .is_none()
        );
        // Even a read-only search cannot find it through a typo.
        let (typo, _) = run(&store, IndexPolicy::ReadOnly, &entries, "tokenzer");
        assert!(typo.hits.is_empty(), "{:?}", slugs(&typo));
    }

    #[test]
    fn the_sync_budget_bounds_writes_per_call() {
        let d = TempDir::new().unwrap();
        let store = open_ferro(&d);
        let entries = seed(&store);
        let policy = IndexPolicy::Sync { budget: 3 };
        let (r, first) = run(&store, policy, &entries, "build cache");
        assert_eq!((first.written, first.pending), (3, 1));
        // Lexical matches never wait for the index.
        assert_eq!(slugs(&r).first(), Some(&"build-cache"));
        let (_, second) = run(&store, policy, &entries, "build cache");
        assert_eq!((second.written, second.pending), (1, 0));
        let (_, third) = run(&store, policy, &entries, "build cache");
        assert_eq!(third, IndexStats::default());
    }

    #[test]
    fn read_only_policy_never_writes_and_uses_only_what_sync_indexed() {
        let d = TempDir::new().unwrap();
        let store = open_ferro(&d);
        let entries = seed(&store);
        let ferro = store.as_ferro().unwrap();
        let col = vector_collection_name(default_embedder());

        // Before any sync there is no collection, and read-only does not create one.
        let (lex, stats) = run(&store, IndexPolicy::ReadOnly, &entries, "cache");
        assert_eq!(stats, IndexStats::default());
        assert_eq!(slugs(&lex), vec!["build-cache"]);
        assert_eq!(lex.mode, SearchMode::Knowledge);
        assert!(!has_collection(ferro, &col));
        let (stem, _) = run(&store, IndexPolicy::ReadOnly, &entries, "checkpointing");
        assert!(stem.hits.is_empty(), "{:?}", slugs(&stem));
        assert!(!has_collection(ferro, &col));

        // A sync search indexes; then read-only finds the vector-only match
        // and still writes nothing.
        let (_, synced) = run(&store, SYNC, &entries, "cache");
        assert_eq!(synced.written, 4);
        let (stem, stats) = run(&store, IndexPolicy::ReadOnly, &entries, "checkpointing");
        assert_eq!(stats, IndexStats::default());
        assert_eq!(slugs(&stem).first(), Some(&"build-cache"));

        // A changed entry is not re-indexed by read-only; its new words are
        // still found lexically.
        put(
            &store,
            "gpu-queue",
            "gpu queue arbitration",
            "Added a zanzibar rule.",
            9_000,
        );
        let entries = store.list_state(None, None).unwrap();
        let (r, stats) = run(&store, IndexPolicy::ReadOnly, &entries, "zanzibar");
        assert_eq!(stats, IndexStats::default());
        assert_eq!(slugs(&r), vec!["gpu-queue"]);
        let (_, resync) = run(&store, SYNC, &entries, "zanzibar");
        assert_eq!(resync.written, 1);
    }

    #[test]
    fn only_passed_entries_are_ever_returned() {
        let d = TempDir::new().unwrap();
        let store = open_ferro(&d);
        let all = seed(&store);
        // The caller drops `build-cache` (say it is sensitive for this viewer).
        let shown: Vec<StateEntry> = all
            .iter()
            .filter(|e| e.slug != "build-cache")
            .cloned()
            .collect();
        // Index everything first so the store holds the hidden entry.
        let r = rank_state_entries(&store, &all, "build cache", None, None, 5);
        assert_eq!(slugs(&r).first(), Some(&"build-cache"));
        for q in ["build cache", "checkpoint", "chekpoint"] {
            for policy in [SYNC, IndexPolicy::ReadOnly] {
                let (r, _) = run(&store, policy, &shown, q);
                assert!(
                    r.hits.iter().all(|h| h.entry.slug != "build-cache"),
                    "`{q}` leaked {:?}",
                    slugs(&r)
                );
            }
        }
    }

    /// An embedder that breaks its own dimension contract.
    struct BrokenEmbedder;

    impl Embedder for BrokenEmbedder {
        fn dim(&self) -> usize {
            16
        }
        fn embed(&self, _text: &str) -> Vec<f32> {
            vec![0.5; 8]
        }
        fn space_id(&self) -> String {
            "broken-test-space".into()
        }
    }

    #[test]
    fn a_failing_ferro_path_falls_back_to_bm25_with_a_fixed_note() {
        let d = TempDir::new().unwrap();
        let store = open_ferro(&d);
        let entries = seed(&store);
        let ranked = rank_state_entries_with(
            &store,
            &BrokenEmbedder,
            SYNC,
            &entries,
            "build cache",
            None,
            None,
            5,
        );
        assert_eq!(ranked.mode, SearchMode::Bm25);
        // No engine text in the note.
        assert_eq!(
            ranked.note.as_deref(),
            Some("knowledge index unavailable; ranked with BM25")
        );
        let bm25 = rank_bm25(&entries, "build cache", None, None, 5);
        assert_eq!(ranked.hits, bm25.hits);
        assert_eq!(ranked.total_hits, bm25.total_hits);
        assert!(!ranked.hits.is_empty());
    }

    #[test]
    fn the_engines_missing_collection_message_is_pinned() {
        let d = TempDir::new().unwrap();
        let store = open_ferro(&d);
        let ferro = store.as_ferro().unwrap();
        // If the engine words this error differently the probe would treat a
        // missing collection as a failure: fail here, loudly.
        let err = ferro.get_vector("state_vec_00000000", "probe").unwrap_err();
        assert!(is_missing_collection(&err), "engine message changed: {err}");
        assert!(
            err.to_string().contains("no vector collection"),
            "engine message changed: {err}"
        );
        assert!(!has_collection(ferro, "state_vec_00000000"));
        for call in [
            ferro
                .upsert_vector("state_vec_00000000", "x", &[0.0; 16], None)
                .unwrap_err(),
            ferro
                .search_vectors("state_vec_00000000", &[0.0; 16], 1, None)
                .map(|_| ())
                .unwrap_err(),
        ] {
            assert!(is_missing_collection(&call), "{call}");
        }
    }

    #[test]
    fn a_probe_error_that_is_not_a_missing_collection_is_not_swallowed() {
        let d = TempDir::new().unwrap();
        let store = open_ferro(&d);
        let ferro = store.as_ferro().unwrap();
        let embedder = HashingEmbedder::default();
        let col = vector_collection_name(&embedder);
        ensure_vector_collection(ferro, &embedder, &col).unwrap();
        assert!(has_collection(ferro, &col));

        // An existing collection probed with an invalid id is an error, and
        // not the "missing" one.
        let bad = ferro.get_vector(&col, "").unwrap_err();
        assert!(!is_missing_collection(&bad), "{bad}");
        assert!(missing_as_none(Err::<(), _>(bad)).is_err());
        assert!(missing_as_none(Err::<(), _>(StoreError::Backend("disk on fire".into()))).is_err());
        assert_eq!(
            missing_as_none(Err::<(), _>(StoreError::Backend(
                "no vector collection `x`".into()
            )))
            .unwrap(),
            None
        );

        // A collection of the wrong dimension surfaces as a search that falls
        // back to BM25 (the collection exists, so it is not recreated).
        let wrong = vector_collection_name(&BrokenEmbedder);
        ferro
            .create_vector_collection(
                &wrong,
                32,
                VectorStorage::F32,
                VectorMetric::Cosine,
                16,
                200,
                64,
            )
            .unwrap();
        let entries = seed(&store);
        for policy in [SYNC, IndexPolicy::ReadOnly] {
            let r = rank_state_entries_with(
                &store,
                &BrokenEmbedder,
                policy,
                &entries,
                "build cache",
                None,
                None,
                5,
            );
            assert_eq!(r.mode, SearchMode::Bm25, "{policy:?}");
            assert!(r.note.is_some(), "{policy:?}");
            assert_eq!(slugs(&r).first(), Some(&"build-cache"));
        }
    }

    /// Pinned memory, retired memory, and normal ones with the same text.
    fn weight_fixture(store: &AnyStore) -> Vec<StateEntry> {
        let body = "Rotation of the ledgerprobe keys happens every quarter.";
        let summary = "ledgerprobe key rotation";
        put(store, "note-a", summary, body, 1_000);
        put(store, "note-b", summary, body, 1_001);
        let mut p = spec("note-p", summary, body, 1_002);
        p.pinned = true;
        put_spec(store, p);
        let mut r = spec("note-r", summary, body, 1_003);
        r.lifecycle = MemoryLifecycle::Retired;
        put_spec(store, r);
        store.list_state(None, None).unwrap()
    }

    #[test]
    fn pinned_and_retired_weights_hold_on_both_engines() {
        let (_rd, redb, _fd, ferro) = pair();
        let re = weight_fixture(&redb);
        let fe = weight_fixture(&ferro);
        for q in ["ledgerprobe", "ledgerprobe key rotation", "key rotation"] {
            let a = rank_state_entries(&redb, &re, q, None, None, 10);
            let b = rank_state_entries(&ferro, &fe, q, None, None, 10);
            let order = slugs(&b);
            assert_eq!(b.mode, SearchMode::Knowledge, "{:?}", b.note);
            assert_eq!(
                order.first(),
                Some(&"note-p"),
                "`{q}` pinned first: {order:?}"
            );
            assert_eq!(
                order.last(),
                Some(&"note-r"),
                "`{q}` retired last: {order:?}"
            );
            // The relative order of the weighted entries is redb's. (The two
            // identical normal entries may swap: they tie lexically and the
            // vector leg breaks that tie arbitrarily.)
            let tiers = |r: &RankedHits| -> Vec<usize> {
                slugs(r)
                    .into_iter()
                    .map(|s| match s {
                        "note-p" => 0,
                        "note-r" => 2,
                        _ => 1,
                    })
                    .collect()
            };
            assert_eq!(tiers(&b), tiers(&a), "`{q}` weight order differs from redb");
            assert_eq!(tiers(&b), vec![0, 1, 1, 2]);
        }
    }

    #[test]
    fn every_lexical_match_is_kept_beyond_the_old_candidate_cap() {
        let (_rd, redb, _fd, ferro) = pair();
        for store in [&redb, &ferro] {
            for i in 0..260u64 {
                put(
                    store,
                    &format!("bulk-{i:03}"),
                    &format!("bulk note {i}"),
                    "This note mentions the zirconium probe.",
                    1_000 + i,
                );
            }
            put(store, "other", "lunch", "sandwiches", 5_000);
        }
        let re = redb.list_state(None, None).unwrap();
        let fe = ferro.list_state(None, None).unwrap();
        let a = rank_state_entries(&redb, &re, "zirconium", None, None, 5);
        // A small budget keeps the test fast: the lexical leg does not wait
        // for the index.
        let few = IndexPolicy::Sync { budget: 3 };
        let (b, stats) = rank_with_stats(
            &ferro,
            default_embedder(),
            few,
            &fe,
            "zirconium",
            None,
            None,
            5,
        );
        assert_eq!((stats.written, stats.pending), (3, 258));
        assert_eq!(b.mode, SearchMode::Knowledge, "{:?}", b.note);
        assert_eq!(a.total_hits, 260);
        assert_eq!(b.total_hits, 260);
        assert_eq!(b.hits.len(), 5);
        assert_eq!(a.hits.len(), 5);
        let all = rank_with_stats(
            &ferro,
            default_embedder(),
            IndexPolicy::ReadOnly,
            &fe,
            "zirconium",
            None,
            None,
            1_000,
        )
        .0;
        assert_eq!(all.hits.len(), 260);
        assert_eq!(all.total_hits, 260);
    }

    #[test]
    fn ferro_returns_every_hit_redb_returns_for_keyword_queries() {
        let (_rd, redb, _fd, ferro) = pair();
        let mut re = seed(&redb);
        let mut fe = seed(&ferro);
        re.extend(weight_fixture(&redb));
        fe.extend(weight_fixture(&ferro));
        let queries = [
            "cache",
            "build cache",
            "checkpoint",
            "hub",
            "queue",
            "priority",
            "notes",
            "ledgerprobe",
            "rotation keys",
            "the",
            "zzz nothing",
        ];
        for q in queries {
            let a = rank_state_entries(&redb, &re, q, None, None, 1_000);
            let b = rank_state_entries(&ferro, &fe, q, None, None, 1_000);
            let have: Vec<&str> = slugs(&b);
            for slug in slugs(&a) {
                assert!(have.contains(&slug), "`{q}`: ferro lost {slug}");
            }
            assert!(b.total_hits >= a.total_hits, "`{q}`");
        }
    }
}
