//! State-entry search over either store engine.
//!
//! On redb, [`rank_state_entries`] is exactly the BM25 ranker of
//! `fl_core::state::search_state_entries`. On FerroStore it mirrors the
//! entries it is given into a document collection and a vector collection and
//! ranks them with FerroStore's hybrid search (BM25 over the documents fused
//! with HNSW search over `fl_core::embed` vectors), so a typo or a different
//! word form can still find an entry.
//!
//! The embedder is a hashed lexical/subword model, not a learned semantic one.
//! A vector-only hit is therefore kept only when the entry also matches a
//! query term lexically, or sits close to the query in the embedding space.
//!
//! The caller decides which entries may be shown: only ids in the `entries`
//! slice are ever returned, however much the store holds. A failure on the
//! FerroStore path never fails the search; it falls back to BM25 and reports
//! why in [`RankedHits::note`].

use crate::{AnyStore, FerroStore, Fusion, VectorMetric, VectorStorage};
use fl_core::{
    Embedder, HashingEmbedder, StateConcern, StateEntry, StatePayload, StateScope, StateSearchHit,
    StoreError, cosine, estimate_tokens, search_state_entries,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Document collection that mirrors the searchable text of state entries.
pub const KNOWLEDGE_DOC_COLLECTION: &str = "state_knowledge";
/// Candidates asked of the hybrid search before filtering.
const CANDIDATES: usize = 200;
/// Reciprocal-rank-fusion constant.
const RRF_K: u32 = 60;
/// A vector-only hit needs at least this cosine to the query, measured
/// against the entry's summary or its whole text.
const MIN_VECTOR_COSINE: f32 = 0.25;
/// Longest fallback snippet, in characters.
const SNIPPET_CHARS: usize = 160;

/// Which ranker produced a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Field-weighted BM25 over the given entries.
    Bm25,
    /// FerroStore BM25 fused with vector search.
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

/// A ranked search result.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedHits {
    /// At most `limit` hits, best first.
    pub hits: Vec<StateSearchHit>,
    /// Hits before truncation to `limit`.
    pub total_hits: usize,
    pub mode: SearchMode,
    /// Set when the knowledge path failed and BM25 answered instead.
    pub note: Option<String>,
}

/// Rank `entries` with BM25 only (the redb behaviour).
pub fn rank_bm25(
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> RankedHits {
    let all = search_state_entries(entries, query, scope, concern, usize::MAX);
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

/// Rank `entries` against `query` on `store` with the default embedder.
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
        entries,
        query,
        scope,
        concern,
        limit,
    )
}

/// [`rank_state_entries`] with a chosen embedder.
pub fn rank_state_entries_with(
    store: &AnyStore,
    embedder: &dyn Embedder,
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
) -> RankedHits {
    let Some(ferro) = store.as_ferro() else {
        return rank_bm25(entries, query, scope, concern, limit);
    };
    match rank_knowledge(ferro, embedder, entries, query, scope, concern, limit) {
        Ok(Some(ranked)) => ranked,
        Ok(None) => rank_bm25(entries, query, scope, concern, limit),
        Err(err) => {
            let mut ranked = rank_bm25(entries, query, scope, concern, limit);
            ranked.note = Some(format!("knowledge search unavailable: {err}"));
            ranked
        }
    }
}

/// The vector collection for an embedding space: another space or dimension
/// never shares one.
pub fn vector_collection_name(embedder: &dyn Embedder) -> String {
    let digest = Sha256::digest(embedder.space_id().as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("state_vec_{hex}")
}

/// `Ok(None)` when the query has nothing to embed or tokenize and the BM25
/// answer (empty) is the right one.
fn rank_knowledge(
    ferro: &FerroStore,
    embedder: &dyn Embedder,
    entries: &[StateEntry],
    query: &str,
    scope: Option<&StateScope>,
    concern: Option<StateConcern>,
    limit: usize,
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
    let vec_collection = vector_collection_name(embedder);
    sync_index(ferro, embedder, &vec_collection, &eligible)?;

    let found = ferro.search_knowledge(
        KNOWLEDGE_DOC_COLLECTION,
        &vec_collection,
        query,
        &query_vec,
        CANDIDATES,
        Fusion::Rrf { k: RRF_K },
    )?;

    let mut by_id: HashMap<&str, &StateEntry> = HashMap::new();
    for entry in &eligible {
        by_id.entry(entry.id.as_str()).or_insert(entry);
    }
    let mut kept: Vec<(f64, StateSearchHit)> = Vec::new();
    for hit in found {
        let Some(entry) = by_id.get(hit.id.as_str()).copied() else {
            continue;
        };
        let lexical = search_state_entries(std::slice::from_ref(entry), query, None, None, 1)
            .into_iter()
            .next();
        let built = match lexical {
            Some(mut lex) => {
                lex.score_millis = score_millis(hit.score);
                lex
            }
            None => {
                let text = searchable_text(entry);
                // A short query is diluted by a long body, so the title (the
                // summary, else the slug) counts on its own as well as the whole text.
                let near = cosine(&query_vec, &embedder.embed(&text))
                    .max(cosine(&query_vec, &embedder.embed(&title_text(entry))));
                if near < MIN_VECTOR_COSINE {
                    continue;
                }
                StateSearchHit {
                    snippet: fallback_snippet(entry),
                    estimated_tokens: estimate_tokens(&text),
                    entry: entry.clone(),
                    score_millis: score_millis(hit.score),
                    matched_terms: Vec::new(),
                }
            }
        };
        kept.push((hit.score, built));
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

fn score_millis(score: f64) -> u64 {
    ((score * 1000.0).round() as u64).max(1)
}

fn is_zero(v: &[f32]) -> bool {
    v.iter().all(|x| *x == 0.0)
}

/// Mirror `entries` into the document and vector collections, writing only
/// what changed. Returns the number of entries written.
///
/// The vector is written before the document, and the document carries the
/// `rev`/`space` stamp, so an interrupted write is redone on the next search.
/// `rev` and `space` are stored as numbers: FerroStore's BM25 tokenizes every
/// string in a document, and a revision IRI or a hash must not become a
/// searchable term.
pub(crate) fn sync_index(
    ferro: &FerroStore,
    embedder: &dyn Embedder,
    vec_collection: &str,
    entries: &[&StateEntry],
) -> Result<usize, StoreError> {
    ensure_vector_collection(ferro, embedder, vec_collection)?;
    let space = stamp(&embedder.space_id());
    let mut written = 0;
    for entry in entries {
        let id = entry.id.as_str();
        let rev = stamp(entry.head_revision.as_str());
        if let Some(doc) = ferro.get_doc(KNOWLEDGE_DOC_COLLECTION, id)?
            && doc.get("rev").and_then(Value::as_u64) == Some(rev)
            && doc.get("space").and_then(Value::as_u64) == Some(space)
        {
            continue;
        }
        let (summary, body) = summary_and_body(&entry.payload);
        let text = searchable_text(entry);
        let values = embedder.embed(&text);
        if !is_zero(&values) {
            let meta = json!({
                "scope": entry.scope.as_wire(),
                "concern": entry.concern.as_wire(),
            });
            ferro.upsert_vector(vec_collection, id, &values, Some(&meta))?;
        }
        ferro.put_doc(
            KNOWLEDGE_DOC_COLLECTION,
            id,
            &json!({
                "slug": entry.slug,
                "summary": summary,
                "body": body,
                "rev": rev,
                "space": space,
            }),
        )?;
        written += 1;
    }
    Ok(written)
}

fn ensure_vector_collection(
    ferro: &FerroStore,
    embedder: &dyn Embedder,
    name: &str,
) -> Result<(), StoreError> {
    // Creating an existing collection is an error, so probe first: a missing
    // id in an existing collection is `Ok(None)`; a missing collection errs.
    if matches!(ferro.get_vector(name, "probe"), Ok(None)) {
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
        Err(err) => match ferro.get_vector(name, "probe") {
            Ok(None) => Ok(()),
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

    fn put(store: &AnyStore, slug: &str, summary: &str, body: &str, at_ms: u64) -> StateEntry {
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: slug.into(),
                sensitivity: SensitivityPatch::Inherit,
                payload: StatePayload::Memory(MemoryEntry {
                    category: MemoryCategory::Note,
                    basis: MemoryBasis::Inference,
                    lifecycle: MemoryLifecycle::Active,
                    superseded_by: None,
                    pinned: false,
                    summary: summary.into(),
                    body: body.into(),
                }),
                at: At::from_unix_millis(at_ms),
                by: "tester".into(),
            })
            .unwrap()
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
        let (_fd, ferro) = {
            let d = TempDir::new().unwrap();
            let s = open_ferro(&d);
            (d, s)
        };
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
    fn limit_truncates_hits_but_keeps_total() {
        let d = TempDir::new().unwrap();
        let ferro = open_ferro(&d);
        for i in 0..4u64 {
            put(
                &ferro,
                &format!("cache-note-{i}"),
                &format!("cache tuning note {i}"),
                "cache sizes and eviction",
                2_000 + i,
            );
        }
        put(&ferro, "other", "lunch plans", "sandwiches", 3_000);
        let entries = ferro.list_state(None, None).unwrap();
        let all = rank_state_entries(&ferro, &entries, "cache", None, None, 10);
        assert_eq!(all.total_hits, 4, "{:?}", slugs(&all));
        let one = rank_state_entries(&ferro, &entries, "cache", None, None, 1);
        assert_eq!(one.hits.len(), 1);
        assert_eq!(one.total_hits, 4);
        assert_eq!(one.hits[0], all.hits[0]);
        let zero = rank_state_entries(&ferro, &entries, "cache", None, None, 0);
        assert!(zero.hits.is_empty());
        assert_eq!(zero.total_hits, 4);
    }

    #[test]
    fn scope_and_concern_filters_apply_on_ferro() {
        let d = TempDir::new().unwrap();
        let ferro = open_ferro(&d);
        put(&ferro, "ws-cache", "workspace cache note", "cache", 1_000);
        ferro
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
        let entries = ferro.list_state(None, None).unwrap();
        let scope = StateScope::Project("alpha".into());
        let r = rank_state_entries(&ferro, &entries, "cache", Some(&scope), None, 5);
        assert_eq!(slugs(&r), vec!["proj-cache"]);
        let r = rank_state_entries(
            &ferro,
            &entries,
            "cache",
            None,
            Some(StateConcern::Handoff),
            5,
        );
        assert!(r.hits.is_empty());
    }

    #[test]
    fn unchanged_entries_are_not_rewritten_and_updates_are_reindexed() {
        let d = TempDir::new().unwrap();
        let store = open_ferro(&d);
        let entries = seed(&store);
        let ferro = store.as_ferro().unwrap();
        let embedder = HashingEmbedder::default();
        let col = vector_collection_name(&embedder);
        let refs: Vec<&StateEntry> = entries.iter().collect();

        assert_eq!(sync_index(ferro, &embedder, &col, &refs).unwrap(), 4);
        assert_eq!(sync_index(ferro, &embedder, &col, &refs).unwrap(), 0);
        // Searching does not rewrite either.
        let r = rank_state_entries(&store, &entries, "build cache", None, None, 5);
        assert_eq!(slugs(&r).first(), Some(&"build-cache"));
        assert_eq!(sync_index(ferro, &embedder, &col, &refs).unwrap(), 0);

        // An update gets a new head revision: one write, found by its new text.
        let before = entries.iter().find(|e| e.slug == "lunch-notes").unwrap();
        let updated = put(
            &store,
            "lunch-notes",
            "meeting notes about tokenizer",
            "Swapped the tokenizer vocabulary for the offsite.",
            9_000,
        );
        assert_ne!(updated.head_revision, before.head_revision);
        let entries = store.list_state(None, None).unwrap();
        let refs: Vec<&StateEntry> = entries.iter().collect();
        assert_eq!(sync_index(ferro, &embedder, &col, &refs).unwrap(), 1);
        assert_eq!(sync_index(ferro, &embedder, &col, &refs).unwrap(), 0);

        let new = rank_state_entries(&store, &entries, "tokenizer", None, None, 5);
        assert_eq!(slugs(&new), vec!["lunch-notes"]);
        let old = rank_state_entries(&store, &entries, "sandwiches", None, None, 5);
        assert!(old.hits.is_empty(), "stale text found {:?}", slugs(&old));
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
            let r = rank_state_entries(&store, &shown, q, None, None, 5);
            assert!(
                r.hits.iter().all(|h| h.entry.slug != "build-cache"),
                "`{q}` leaked {:?}",
                slugs(&r)
            );
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
    fn a_failing_ferro_path_falls_back_to_bm25_with_a_note() {
        let d = TempDir::new().unwrap();
        let store = open_ferro(&d);
        let entries = seed(&store);
        let ranked = rank_state_entries_with(
            &store,
            &BrokenEmbedder,
            &entries,
            "build cache",
            None,
            None,
            5,
        );
        assert_eq!(ranked.mode, SearchMode::Bm25);
        let note = ranked
            .note
            .as_deref()
            .expect("a note explains the fallback");
        assert!(note.contains("knowledge search unavailable"), "{note}");
        let bm25 = rank_bm25(&entries, "build cache", None, None, 5);
        assert_eq!(ranked.hits, bm25.hits);
        assert_eq!(ranked.total_hits, bm25.total_hits);
        assert!(!ranked.hits.is_empty());
    }

    #[test]
    fn embedding_spaces_do_not_share_a_vector_collection() {
        let a = HashingEmbedder::default();
        let b = HashingEmbedder::new(fl_core::EmbedConfig {
            dim: 64,
            ..fl_core::EmbedConfig::default()
        })
        .unwrap();
        let (na, nb) = (vector_collection_name(&a), vector_collection_name(&b));
        assert_ne!(na, nb);
        assert!(na.starts_with("state_vec_") && na.len() == "state_vec_".len() + 8);

        // Both spaces work against one store, each in its own collection.
        let d = TempDir::new().unwrap();
        let store = open_ferro(&d);
        let entries = seed(&store);
        for e in [&a as &dyn Embedder, &b] {
            let r = rank_state_entries_with(&store, e, &entries, "build cache", None, None, 5);
            assert_eq!(r.mode, SearchMode::Knowledge, "{:?}", r.note);
            assert_eq!(slugs(&r).first(), Some(&"build-cache"));
        }
    }
}
