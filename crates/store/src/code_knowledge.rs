//! Code-index search over either store engine.
//!
//! [`search_code`] answers the three `fl_core::code_index` searches. `Def` and
//! `Outline`, and every search on redb, are exactly
//! [`CodeIndex::search`](fl_core::CodeIndex::search): same hits, same order,
//! same `tokens_saved_this_query`. Their [`SearchMode`] is
//! [`SearchMode::Bm25`], the enum's "no vector leg" value (code search ranks
//! by name tiers, not BM25; the enum is shared with the state search).
//!
//! `Search` on FerroStore fuses two legs by reciprocal rank (`k = 60`, ranks
//! 1-based, as in [`crate::knowledge`]):
//!
//! * the **lexical leg** is the exact `CodeIndex::search(Search, ..)` ranking
//!   over every symbol and line, capped at 10,000 matches;
//! * the **vector leg** is FerroStore's HNSW index of `fl_core::embed` vectors
//!   of the declarations, so a typo, a word fragment or a different identifier
//!   form (`tokenise`, `chekpoint`) still finds a symbol.
//!
//! A vector-only hit is kept only if its declaration sits close to the query
//! (cosine of at least 0.25 to the declaration text or to the name alone; the
//! embedder is hashed and lexical, so a bare nearest neighbour is noise). The
//! final hits are built from the [`CodeIndex`]'s own symbol data, so snippets
//! and token estimates are exact, and `tokens_saved_this_query` is recomputed
//! from them by [`fl_core::outcome_from_hits`], the rule the exact path uses.
//! Order: fused score, then path, then start line (then kind and name, so the
//! order is total).
//!
//! **What is embedded: name, signature and path, never the snippet.** A body
//! can hold a secret (a token in a `const`, a key in a fixture), and it is
//! large; a declaration line is short and is what a person searches by. A
//! signature is cut at its first `=`, because a `const` or `static` initialiser
//! sits on the declaration line.
//!
//! **Where the vectors live: a sidecar file.** A scan of this repository finds
//! about 21,500 symbols, and every read of a FerroStore decodes the whole
//! store, so putting them in the primary store would slow every read of it.
//! They go to `<primary>.code.fls` instead (`fl.fls` -> `fl.code.fls`), made
//! mode 0600 beside the primary by [`FerroStore::open`]. It holds derived
//! data only: delete it and the next [`IndexPolicy::Sync`] search rebuilds it.
//! Nothing treats it as a project store: the Hub lists projects by a name
//! `<slug>.fls` with a slug of letters, digits, `-` and `_`, and `foo.code`
//! is not one.
//!
//! **What is stored**, in the sidecar, per scanned root: one vector per symbol
//! in the collection `code_vec_<8 hex of sha256(space_id)>_<16 hex of
//! sha256(root)>` (metadata `{path}`, the id a hash of path, kind, name and
//! start line), and one index document `code_vec_index/<collection>` mapping
//! each file path to `{digest, ids}`: the SHA-256 of the file's lines and the
//! ids of the vectors it owns. A file whose digest is unchanged is left alone;
//! a changed file has its stale vectors deleted and its new ones written; a
//! file gone from the tree has its vectors deleted.
//!
//! **The root keys everything.** Paths in the document are relative to the
//! scanned root, so two roots hold `src/lib.rs` each, and a search with
//! another root (or a subdirectory) must not read the first tree as deleted.
//! The root is the [`CodeIndex::root`] (canonical when the filesystem can say
//! so), and its hash is part of the collection name: each root has its own
//! vectors and its own document, symbol ids are scoped by that collection, and
//! roots coexist in one sidecar without touching each other. A root that is
//! not searched again leaves its collection behind; the sidecar is derived
//! data, and deleting it clears them all.
//!
//! The document is written *ahead* of the vectors, as in
//! [`crate::knowledge`]: before a file is touched, ids that might come to
//! exist are recorded under a `pending` digest; afterwards the real digest
//! replaces it, and that final write happens on the error path too. A crash
//! or an error part-way leaves a committed vector listed, never orphaned, and
//! the next sync repairs it (a `pending` digest never matches).
//!
//! Every write to the document is a merge into the freshest copy, done inside
//! one [`FerroStore::transaction`]: the read and the write run under the
//! store's write lock, so a concurrent writer's files cannot be lost between
//! them. The placeholder remains for what a transaction cannot cover, a crash
//! or an error between the document write and the vector writes.
//!
//! Vectors are written in batches of [`UPSERT_BATCH`] through
//! [`FerroStore::upsert_vectors`] (a batch is one commit and one copy of the
//! collection, and is all or nothing), a changed file's stale vectors are
//! deleted in one transaction per batch, the vectors of files gone from the
//! tree in groups of about a batch, and a sync that wrote at least one batch
//! ends with a [`FerroStore::checkpoint`]: the engine replays every logged
//! vector insert through HNSW on open, so an unfolded bulk build would make
//! the next open slow.
//!
//! [`IndexPolicy::Sync`] writes at most `budget` new vectors per call (a whole
//! file at a time; a file larger than the budget goes alone) and starts no new
//! group of deletes or file once `SYNC_DEADLINE` has passed (the first unit of
//! work is always done), so one search never waits for a whole cold build
//! (about 21,500 symbols take about 50 seconds in all, 5 to 6 seconds per
//! call). Deletes draw no budget: it caps embedding work. What is left is found
//! lexically and indexed by the next searches, each of which makes progress.
//! [`IndexPolicy::ReadOnly`] writes nothing and never creates the sidecar:
//! without one the vector leg is empty.
//!
//! Any failure on the FerroStore path (sidecar open, index, search) falls back
//! to the exact answer with a fixed [`RankedCode::note`] and the engine's
//! error in [`RankedCode::cause`], never in JSON. A corrupt or unwritable
//! sidecar never fails a code search.

use crate::knowledge::{
    MIN_VECTOR_COSINE, UPSERT_BATCH, checkpoint_after_bulk, default_embedder, delete_vectors,
    ensure_vector_collection, is_zero, missing_as_none, rrf, stamp,
};
use crate::{AnyStore, FerroStore, IndexPolicy, SearchMode};
use fl_core::{
    CodeIndex, CodeSearchKind, CodeSearchOutcome, CodeSymbolHit, CodeSymbolRef, Embedder,
    StoreError, cosine, outcome_from_hits,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Vectors one CLI or MCP call may plan (and embed). A cap on the work per
/// call; the deadline below is what normally ends a call.
pub const DEFAULT_CODE_SYNC_BUDGET: usize = 4096;
/// Longest a sync spends writing, checked before each file or group of deletes
/// after the first. A batched upsert costs about 2.4 ms per vector (embedding
/// included), so this admits about 2,000 vectors per call, and a cold build of
/// this repository (about 21,500 symbols, about 50 seconds in all) takes a
/// handful of searches of 5 to 6 seconds each, none of which waits for the
/// whole build. Work not reached is left to a later search, like files over
/// the budget.
const SYNC_DEADLINE: Duration = Duration::from_secs(5);
/// Most matches the lexical leg keeps before fusion.
const LEXICAL_CAP: usize = 10_000;
/// Smallest number of nearest vectors asked of the index.
const MIN_CANDIDATES: usize = 200;
/// Shown when the vector path failed. Fixed text: engine errors are not
/// echoed to callers.
const FALLBACK_NOTE: &str = "code index unavailable; searched without vectors";
/// Document collection holding one path -> state index per vector collection.
/// It stores digests and ids only, never code.
const INDEX_DOC_COLLECTION: &str = "code_vec_index";
/// Digest meaning "vectors of this file may exist and are unfinished".
const PENDING_DIGEST: &str = "pending";
/// What replaces the primary store's extension to name the sidecar.
const SIDECAR_EXTENSION: &str = "code.fls";

/// A ranked code search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedCode {
    /// At most `limit` hits, best first, with the counts and the token gain
    /// computed from exactly those hits.
    pub outcome: CodeSearchOutcome,
    /// [`SearchMode::Knowledge`] when the FerroStore hybrid answered,
    /// otherwise [`SearchMode::Bm25`] (the exact `CodeIndex` answer).
    pub mode: SearchMode,
    /// Set when the vector path failed and the exact answer was given.
    pub note: Option<String>,
    /// The engine's own error behind `note`, for diagnosis. It is never part
    /// of any JSON output: callers show it on stderr at most.
    pub cause: Option<String>,
}

impl RankedCode {
    fn exact(outcome: CodeSearchOutcome) -> Self {
        Self {
            outcome,
            mode: SearchMode::Bm25,
            note: None,
            cause: None,
        }
    }

    /// Add `"mode":"knowledge"` when the hybrid answered and `"note"` on a
    /// fallback to a JSON object. A plain exact answer adds nothing, so its
    /// JSON is byte-identical to what it always was.
    pub fn annotate_json(&self, out: &mut Value) {
        let Some(map) = out.as_object_mut() else {
            return;
        };
        if self.mode == SearchMode::Knowledge {
            map.insert("mode".into(), self.mode.as_str().into());
        }
        if let Some(note) = &self.note {
            map.insert("note".into(), note.clone().into());
        }
    }

    /// The stderr line that tells an operator why a search was answered
    /// without vectors, or `None` when it was not. It carries the engine's
    /// own error, which is kept out of JSON on purpose. One line.
    pub fn unavailable_line(&self) -> Option<String> {
        let cause = self.cause.as_deref()?;
        let one_line: Vec<&str> = cause.split_whitespace().collect();
        Some(format!(
            "fl: code index unavailable: {}",
            one_line.join(" ")
        ))
    }
}

/// What a sync pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CodeSyncStats {
    /// Vectors upserted.
    pub written: usize,
    /// Vectors deleted (stale, or their file is gone).
    pub deleted: usize,
    /// Symbols left to a later search by the budget or the deadline: those of
    /// changed or new files not written, and the ids of gone files not yet
    /// deleted.
    pub pending: usize,
}

/// The sidecar file of the primary store `store`: `<primary>.code.fls`. `None`
/// for a store with no file of its own (in memory, or served over a socket).
pub fn sidecar_path(store: &AnyStore) -> Option<PathBuf> {
    // The path the store was opened with, never its label: the label is
    // `Path::display`, which is lossy for a name that is not UTF-8.
    let file = store.ferro_file()?;
    (file.extension().and_then(|e| e.to_str()) == Some(crate::any::FERRO_EXTENSION)
        && file.file_name().is_some())
    .then(|| file.with_extension(SIDECAR_EXTENSION))
}

/// Search `index` on `store` with the default embedder. See the module
/// documentation.
pub fn search_code(
    store: &AnyStore,
    index: &CodeIndex,
    kind: CodeSearchKind,
    query: &str,
    limit: usize,
    policy: IndexPolicy,
) -> RankedCode {
    search_code_with(store, default_embedder(), index, kind, query, limit, policy)
}

/// [`search_code`] with a chosen embedder.
pub fn search_code_with(
    store: &AnyStore,
    embedder: &dyn Embedder,
    index: &CodeIndex,
    kind: CodeSearchKind,
    query: &str,
    limit: usize,
    policy: IndexPolicy,
) -> RankedCode {
    search_code_stats(store, embedder, index, kind, query, limit, policy).0
}

pub(crate) fn search_code_stats(
    store: &AnyStore,
    embedder: &dyn Embedder,
    index: &CodeIndex,
    kind: CodeSearchKind,
    query: &str,
    limit: usize,
    policy: IndexPolicy,
) -> (RankedCode, CodeSyncStats) {
    let mut stats = CodeSyncStats::default();
    let exact = || RankedCode::exact(index.search(kind, query, limit));
    if kind != CodeSearchKind::Search
        || limit == 0
        || query.trim().is_empty()
        || store.as_ferro().is_none()
    {
        return (exact(), stats);
    }
    match hybrid(store, embedder, index, query, limit, policy, &mut stats) {
        Ok(Some(ranked)) => (ranked, stats),
        Ok(None) => (exact(), stats),
        Err(err) => {
            let mut ranked = exact();
            ranked.note = Some(FALLBACK_NOTE.to_string());
            ranked.cause = Some(err.to_string());
            (ranked, stats)
        }
    }
}

fn hex_prefix(bytes: &[u8], take: usize) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(take)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The vector collection for an embedding space and a scanned root: another
/// space, dimension or root never shares one. The root is the index's own
/// ([`CodeIndex::root`]; an index not made by a scan has none and shares the
/// root-less collection).
pub fn code_vector_collection_name(embedder: &dyn Embedder, index: &CodeIndex) -> String {
    let space = hex_prefix(embedder.space_id().as_bytes(), 4);
    let root = index
        .root()
        .map_or(&[][..], |r| r.as_os_str().as_encoded_bytes());
    format!("code_vec_{space}_{}", hex_prefix(root, 8))
}

/// An id for a symbol (or a matched line): a hash of its path, kind, name and
/// start line. It is unique within one collection, and the collection is per
/// root, so the same relative path in two roots never shares an id.
fn symbol_id(path: &str, kind: &str, name: &str, start_line: u64) -> String {
    let mut hasher = Sha256::new();
    for part in [path, kind, name] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher.update(start_line.to_be_bytes());
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn id_of_symbol(sym: &CodeSymbolRef<'_>) -> String {
    symbol_id(sym.path(), sym.kind(), sym.name(), sym.start_line())
}

fn id_of_hit(hit: &CodeSymbolHit) -> String {
    symbol_id(&hit.path, &hit.kind, &hit.name, hit.start_line)
}

/// What is embedded for a declaration: the name, the signature up to its
/// first `=`, and the path. Never the snippet, and not an initialiser either:
/// in `const KEY: &str = "..."` the value is a body, and on one line with the
/// declaration.
fn embed_text(name: &str, signature: &str, path: &str) -> String {
    let declared = signature.split('=').next().unwrap_or(signature).trim_end();
    format!("{name}\n{declared}\n{path}")
}

fn backend(msg: impl Into<String>) -> StoreError {
    StoreError::Backend(msg.into())
}

/// `Ok(None)` when the query has nothing to embed and the exact answer is the
/// right one.
fn hybrid(
    store: &AnyStore,
    embedder: &dyn Embedder,
    index: &CodeIndex,
    query: &str,
    limit: usize,
    policy: IndexPolicy,
    stats: &mut CodeSyncStats,
) -> Result<Option<RankedCode>, StoreError> {
    let query_vec = embedder.embed(query);
    if is_zero(&query_vec) {
        return Ok(None);
    }
    let path = sidecar_path(store).ok_or_else(|| {
        backend("the code vector index needs a store with a file of its own; this one has none")
    })?;
    let collection = code_vector_collection_name(embedder, index);
    let sidecar = match policy {
        IndexPolicy::Sync { .. } => Some(open_sidecar_for_write(&path)?),
        IndexPolicy::ReadOnly => open_sidecar_if_present(&path)?,
    };
    if let (IndexPolicy::Sync { budget }, Some(ferro)) = (policy, &sidecar) {
        *stats = sync_files(ferro, embedder, &collection, index, budget, SYNC_DEADLINE)?;
    }
    let by_id: HashMap<String, CodeSymbolRef<'_>> = index
        .files()
        .flat_map(|file| file.symbols())
        .map(|sym| (id_of_symbol(&sym), sym))
        .collect();
    let mut vector_order: Vec<(&str, &CodeSymbolRef<'_>)> = Vec::new();
    if let Some(ferro) = &sidecar {
        let k = MIN_CANDIDATES.max(limit.saturating_mul(4));
        if let Some(mut found) =
            missing_as_none(ferro.search_vectors(&collection, &query_vec, k, None))?
        {
            found.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let mut seen = HashSet::new();
            for (id, _) in &found {
                if let Some((key, sym)) = by_id.get_key_value(id.as_str())
                    && seen.insert(key.as_str())
                {
                    vector_order.push((key.as_str(), sym));
                }
            }
        }
    }

    let lexical = index
        .search(CodeSearchKind::Search, query, LEXICAL_CAP)
        .symbols;
    let vec_rank: HashMap<&str, usize> = vector_order
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (*id, i + 1))
        .collect();
    let lex_ids: Vec<String> = lexical.iter().map(id_of_hit).collect();
    let lex_set: HashSet<&str> = lex_ids.iter().map(String::as_str).collect();

    let mut kept: Vec<(f64, CodeSymbolHit)> = Vec::with_capacity(lexical.len());
    for (i, (hit, id)) in lexical.iter().zip(&lex_ids).enumerate() {
        let mut fused = rrf(i + 1);
        if let Some(rank) = vec_rank.get(id.as_str()) {
            fused += rrf(*rank);
        }
        kept.push((fused, hit.clone()));
    }
    for (i, (id, sym)) in vector_order.iter().enumerate() {
        if lex_set.contains(id) {
            continue;
        }
        // A short query is diluted by a long signature, so the name counts on
        // its own as well as the whole declaration.
        let near = cosine(
            &query_vec,
            &embedder.embed(&embed_text(sym.name(), sym.signature(), sym.path())),
        )
        .max(cosine(&query_vec, &embedder.embed(sym.name())));
        if near >= MIN_VECTOR_COSINE {
            kept.push((rrf(i + 1), sym.hit()));
        }
    }
    kept.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.path.cmp(&b.1.path))
            .then_with(|| a.1.start_line.cmp(&b.1.start_line))
            .then_with(|| a.1.kind.cmp(&b.1.kind))
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    let hits: Vec<CodeSymbolHit> = kept.into_iter().take(limit).map(|(_, h)| h).collect();
    Ok(Some(RankedCode {
        outcome: outcome_from_hits(hits, index.files_indexed(), index.symbols_indexed()),
        mode: SearchMode::Knowledge,
        note: None,
        cause: None,
    }))
}

/// Open or create the sidecar. A symlink is refused: the file is private
/// derived data, never an alias for another path.
///
/// Known gap: the check and the open are two steps, and `FerroStore::open`
/// takes a path, not a file descriptor, so there is no `O_NOFOLLOW` open. A
/// process that can write the store's directory can swap the file for a
/// symlink between them. The sidecar holds derived data only (no code bodies),
/// and a process that can write that directory can already replace the primary
/// store, so the check stops a stale or stray link, not an attacker.
fn open_sidecar_for_write(path: &Path) -> Result<FerroStore, StoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(backend(format!("{} is a symlink", path.display())));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(backend(format!("{} is not a regular file", path.display())));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(backend(format!("{}: {err}", path.display()))),
    }
    FerroStore::open(path)
}

/// Open the sidecar only if it exists and has content. An absent or empty
/// file is `None` (an empty vector leg), and nothing is created or written.
///
/// The same gap as [`open_sidecar_for_write`] applies, and one more: the file
/// can be deleted, or filled by a concurrent writer, between the check and
/// the open. A file deleted in between is created and given its first
/// superblock by `FerroStore::open` (an empty, valid sidecar, which the next
/// sync fills); a file filled in between is simply read.
fn open_sidecar_if_present(path: &Path) -> Result<Option<FerroStore>, StoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(backend(format!("{} is a symlink", path.display())))
        }
        Ok(meta) if !meta.is_file() => {
            Err(backend(format!("{} is not a regular file", path.display())))
        }
        // `FerroStore::open` would write the first superblock of an empty file.
        Ok(meta) if meta.len() == 0 => Ok(None),
        Ok(_) => FerroStore::open(path).map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(backend(format!("{}: {err}", path.display()))),
    }
}

/// What the index document records for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileRecord {
    /// A file content digest, or [`PENDING_DIGEST`].
    digest: String,
    /// Ids of the vectors the file owns, or may own while pending.
    ids: Vec<String>,
}

/// The index document of one collection.
struct IndexDoc {
    files: BTreeMap<String, FileRecord>,
}

impl IndexDoc {
    fn load(ferro: &FerroStore, collection: &str, space: u64) -> Result<Self, StoreError> {
        Ok(Self::parse(
            ferro.get_doc(INDEX_DOC_COLLECTION, collection)?,
            space,
        ))
    }

    /// The document as read; one written for another embedding space, or
    /// absent, is empty.
    fn parse(doc: Option<Value>, space: u64) -> Self {
        let mut files = BTreeMap::new();
        if let Some(doc) = doc
            && doc.get("space").and_then(Value::as_u64) == Some(space)
            && let Some(map) = doc.get("files").and_then(Value::as_object)
        {
            for (path, rec) in map {
                let digest = rec.get("digest").and_then(Value::as_str);
                let ids = rec.get("ids").and_then(Value::as_array);
                if let (Some(digest), Some(ids)) = (digest, ids) {
                    files.insert(
                        path.clone(),
                        FileRecord {
                            digest: digest.to_string(),
                            ids: ids
                                .iter()
                                .filter_map(|id| id.as_str().map(str::to_string))
                                .collect(),
                        },
                    );
                }
            }
        }
        Self { files }
    }

    fn to_json(&self, space: u64) -> Value {
        let files: serde_json::Map<String, Value> = self
            .files
            .iter()
            .map(|(path, rec)| {
                (
                    path.clone(),
                    json!({ "digest": rec.digest, "ids": rec.ids }),
                )
            })
            .collect();
        json!({ "space": space, "files": files })
    }

    /// Read the freshest document, apply `changes` and write it back, in one
    /// transaction: no other writer commits between the read and the write.
    /// Paths this call did not touch keep whatever another writer recorded.
    ///
    /// A path whose freshest record lists an id the change does not account
    /// for (see [`Change::known`]) was written by a concurrent sync after this
    /// one planned its change. Its ids are never dropped: the record then
    /// carries them under [`PENDING_DIGEST`], and the next sync repairs it,
    /// deleting those that no longer belong. A removal is then kept as well.
    fn update(
        ferro: &FerroStore,
        collection: &str,
        space: u64,
        changes: &Changes,
    ) -> Result<(), StoreError> {
        ferro.transaction(|txn| {
            let mut fresh = Self::parse(txn.get_doc(INDEX_DOC_COLLECTION, collection), space);
            for (path, change) in changes {
                let known: HashSet<&str> = change
                    .known
                    .iter()
                    .chain(change.record.iter().flat_map(|rec| &rec.ids))
                    .map(String::as_str)
                    .collect();
                let unknown: Vec<String> = fresh
                    .files
                    .get(path)
                    .map(|cur| {
                        cur.ids
                            .iter()
                            .filter(|id| !known.contains(id.as_str()))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                match (&change.record, unknown.is_empty()) {
                    (Some(rec), true) => {
                        fresh.files.insert(path.clone(), rec.clone());
                    }
                    (Some(rec), false) => {
                        let mut ids = rec.ids.clone();
                        ids.extend(unknown);
                        fresh.files.insert(
                            path.clone(),
                            FileRecord {
                                digest: PENDING_DIGEST.to_string(),
                                ids,
                            },
                        );
                    }
                    (None, true) => {
                        fresh.files.remove(path);
                    }
                    (None, false) => {
                        fresh.files.insert(
                            path.clone(),
                            FileRecord {
                                digest: PENDING_DIGEST.to_string(),
                                ids: unknown,
                            },
                        );
                    }
                }
            }
            txn.put_doc(INDEX_DOC_COLLECTION, collection, &fresh.to_json(space))
        })
    }
}

/// A changed or new file to bring up to date.
struct FileWork {
    path: String,
    digest: String,
    /// The file has no declarations: nothing is listed for it.
    no_symbols: bool,
    /// How many declarations the file has, counted the way `pending` counts.
    symbols: usize,
    /// `(id, vector)` for every symbol with a non-zero vector.
    vectors: Vec<(String, Vec<f32>)>,
    /// Ids the document lists for the file now.
    old_ids: Vec<String>,
}

impl FileWork {
    fn new_ids(&self) -> Vec<String> {
        self.vectors.iter().map(|(id, _)| id.clone()).collect()
    }

    /// Every id this writer accounts for: the ones listed before and the new.
    fn known_ids(&self) -> Vec<String> {
        let mut ids = self.old_ids.clone();
        ids.extend(self.new_ids());
        ids
    }
}

/// A change to one path of the index document.
struct Change {
    /// The record to write; `None` removes the path.
    record: Option<FileRecord>,
    /// Ids this writer accounts for: listed in `record`, or deleted by it. An
    /// id the freshest document lists for the path that is in neither was put
    /// there by a concurrent writer, and is kept (see [`IndexDoc::update`]).
    known: Vec<String>,
}

impl Change {
    fn set(record: FileRecord, known: Vec<String>) -> Self {
        Self {
            record: Some(record),
            known,
        }
    }
}

type Changes = BTreeMap<String, Change>;

/// Bring the sidecar up to date with `index`, writing at most `budget` new
/// vectors (a whole file at a time; the first file is taken even if it alone
/// is larger) and for at most `deadline` (checked between files; the first
/// file is always done). A budget of 0 writes none. One read of the index document says
/// which files are current; embedding happens only for files that are not.
///
/// The document is written before the vectors it describes (see the module
/// documentation) and finalised on the error path too, then the first error is
/// returned.
pub(crate) fn sync_files(
    ferro: &FerroStore,
    embedder: &dyn Embedder,
    collection: &str,
    index: &CodeIndex,
    budget: usize,
    deadline: Duration,
) -> Result<CodeSyncStats, StoreError> {
    sync_files_batched(
        ferro,
        embedder,
        collection,
        index,
        budget,
        deadline,
        UPSERT_BATCH,
    )
}

/// [`sync_files`] with a chosen batch size: the vectors of whole files are
/// grouped until a group holds `batch` of them, and a group is written, then
/// finalised, together.
pub(crate) fn sync_files_batched(
    ferro: &FerroStore,
    embedder: &dyn Embedder,
    collection: &str,
    index: &CodeIndex,
    budget: usize,
    deadline: Duration,
    batch: usize,
) -> Result<CodeSyncStats, StoreError> {
    let batch = batch.max(1);
    let started = Instant::now();
    let space = stamp(&embedder.space_id());
    let doc = IndexDoc::load(ferro, collection, space)?;
    let mut stats = CodeSyncStats::default();

    // Plan: nothing is written here.
    let mut present: HashSet<&str> = HashSet::new();
    let mut work: Vec<FileWork> = Vec::new();
    let mut used = 0usize;
    for file in index.files() {
        present.insert(file.path());
        let digest = file.content_digest();
        let known = doc.files.get(file.path());
        if known.is_some_and(|rec| rec.digest == digest) {
            continue;
        }
        let symbols: Vec<CodeSymbolRef<'_>> = file.symbols().collect();
        if symbols.is_empty() && known.is_none() {
            continue;
        }
        let fits =
            symbols.is_empty() || (budget > 0 && (used == 0 || used + symbols.len() <= budget));
        if !fits {
            stats.pending += symbols.len();
            continue;
        }
        used += symbols.len();
        let mut vectors = Vec::with_capacity(symbols.len());
        let mut seen = HashSet::new();
        for sym in &symbols {
            let id = id_of_symbol(sym);
            if !seen.insert(id.clone()) {
                continue;
            }
            let values = embedder.embed(&embed_text(sym.name(), sym.signature(), sym.path()));
            if !is_zero(&values) {
                vectors.push((id, values));
            }
        }
        work.push(FileWork {
            path: file.path().to_string(),
            digest,
            no_symbols: symbols.is_empty(),
            symbols: symbols.len(),
            vectors,
            old_ids: known.map(|rec| rec.ids.clone()).unwrap_or_default(),
        });
    }
    let gone: Vec<(&String, &FileRecord)> = doc
        .files
        .iter()
        .filter(|(path, _)| !present.contains(path.as_str()))
        .collect();
    if work.is_empty() && gone.is_empty() {
        return Ok(stats);
    }

    // Record intent first. An id the document already lists stays listed.
    let mut changes: Changes = BTreeMap::new();
    let mut intent = false;
    for (path, rec) in &gone {
        if rec.digest != PENDING_DIGEST {
            intent = true;
            changes.insert(
                (*path).clone(),
                Change::set(
                    FileRecord {
                        digest: PENDING_DIGEST.to_string(),
                        ids: rec.ids.clone(),
                    },
                    rec.ids.clone(),
                ),
            );
        }
    }
    for item in &work {
        let listed: HashSet<&str> = item.old_ids.iter().map(String::as_str).collect();
        let fresh: Vec<String> = item
            .new_ids()
            .into_iter()
            .filter(|id| !listed.contains(id.as_str()))
            .collect();
        if !fresh.is_empty() {
            intent = true;
            let mut ids = item.old_ids.clone();
            ids.extend(fresh);
            changes.insert(
                item.path.clone(),
                Change::set(
                    FileRecord {
                        digest: PENDING_DIGEST.to_string(),
                        ids,
                    },
                    item.known_ids(),
                ),
            );
        }
    }
    if work.iter().any(|w| !w.vectors.is_empty()) {
        ensure_vector_collection(ferro, embedder, collection)?;
    }
    if intent {
        IndexDoc::update(ferro, collection, space, &changes)?;
    }
    changes.clear();

    // Write. Stop at the first error but still finalise what succeeded. Files
    // gone from the tree go first, in groups of about `batch` ids; the files
    // to write follow, in groups of about `batch` vectors, each group written,
    // its stale vectors deleted, and only then its files finalised. Both stop
    // at the deadline, after the first unit of work of the whole sync.
    let mut failure: Option<StoreError> = None;
    'files: {
        let mut did_work = false;
        let mut cut_off = false;
        let groups = gone_groups(&gone, batch);
        for (n, group) in groups.iter().enumerate() {
            if n > 0 && started.elapsed() >= deadline {
                // Still listed as pending: a later sync takes it up again.
                stats.pending += group_ids(&groups[n..]);
                cut_off = true;
                break;
            }
            if let Err(err) = delete_gone_group(ferro, collection, group, batch, &mut stats) {
                failure = Some(err);
                break 'files;
            }
            for (path, rec) in group {
                changes.insert(
                    (*path).clone(),
                    Change {
                        record: None,
                        known: rec.ids.clone(),
                    },
                );
            }
            did_work = true;
        }
        if cut_off {
            stats.pending += work.iter().map(|w| w.symbols).sum::<usize>();
            break 'files;
        }
        let mut group: Vec<&FileWork> = Vec::new();
        let mut buffered = 0usize;
        for (done, item) in work.iter().enumerate() {
            if (did_work || done > 0) && started.elapsed() >= deadline {
                // Still listed as pending: a later sync takes it up again.
                stats.pending += work[done..].iter().map(|w| w.symbols).sum::<usize>();
                break;
            }
            buffered += item.vectors.len();
            group.push(item);
            if buffered >= batch {
                if let Err(err) =
                    write_group(ferro, collection, &group, batch, &mut stats, &mut changes)
                {
                    failure = Some(err);
                    break 'files;
                }
                group.clear();
                buffered = 0;
            }
        }
        if !group.is_empty()
            && let Err(err) =
                write_group(ferro, collection, &group, batch, &mut stats, &mut changes)
        {
            failure = Some(err);
        }
    }
    let finalised = if changes.is_empty() {
        Ok(())
    } else {
        IndexDoc::update(ferro, collection, space, &changes)
    };
    checkpoint_after_bulk(ferro, stats.written);
    match (failure, finalised) {
        (Some(err), _) | (None, Err(err)) => Err(err),
        (None, Ok(())) => Ok(stats),
    }
}

/// The files gone from the tree, in groups that hold about `batch` ids (a
/// file is never split, so a file with more ids than that is a group alone).
fn gone_groups<'a>(
    gone: &[(&'a String, &'a FileRecord)],
    batch: usize,
) -> Vec<Vec<(&'a String, &'a FileRecord)>> {
    let mut groups = Vec::new();
    let mut group = Vec::new();
    let mut ids = 0usize;
    for &(path, rec) in gone {
        ids += rec.ids.len();
        group.push((path, rec));
        if ids >= batch {
            groups.push(std::mem::take(&mut group));
            ids = 0;
        }
    }
    if !group.is_empty() {
        groups.push(group);
    }
    groups
}

/// How many ids the files of `groups` list.
fn group_ids(groups: &[Vec<(&String, &FileRecord)>]) -> usize {
    groups.iter().flatten().map(|(_, rec)| rec.ids.len()).sum()
}

/// Delete the vectors of one group of gone files, in transactions of at most
/// `batch` ids.
fn delete_gone_group(
    ferro: &FerroStore,
    collection: &str,
    group: &[(&String, &FileRecord)],
    batch: usize,
    stats: &mut CodeSyncStats,
) -> Result<(), StoreError> {
    let ids: Vec<&str> = group
        .iter()
        .flat_map(|(_, rec)| rec.ids.iter().map(String::as_str))
        .collect();
    for chunk in ids.chunks(batch) {
        delete_vectors(ferro, collection, chunk)?;
        stats.deleted += chunk.len();
    }
    Ok(())
}

/// Write the vectors of `group` in commits of at most `batch`, delete the ids
/// its files no longer own in one transaction, and record each file as done in
/// `changes`. On `Err` nothing is recorded: the group's files stay pending,
/// and a chunk that did commit is listed by the intent record.
fn write_group(
    ferro: &FerroStore,
    collection: &str,
    group: &[&FileWork],
    batch: usize,
    stats: &mut CodeSyncStats,
    changes: &mut Changes,
) -> Result<(), StoreError> {
    let mut items: Vec<(&str, &[f32], Value)> = Vec::new();
    for item in group {
        for (id, values) in &item.vectors {
            items.push((id.as_str(), values.as_slice(), json!({ "path": item.path })));
        }
    }
    for chunk in items.chunks(batch) {
        let refs: Vec<(&str, &[f32], Option<&Value>)> = chunk
            .iter()
            .map(|(id, values, meta)| (*id, *values, Some(meta)))
            .collect();
        ferro.upsert_vectors(collection, &refs)?;
        stats.written += chunk.len();
    }
    let mut stale: Vec<&str> = Vec::new();
    for item in group {
        let current: HashSet<&str> = item.vectors.iter().map(|(id, _)| id.as_str()).collect();
        stale.extend(
            item.old_ids
                .iter()
                .map(String::as_str)
                .filter(|id| !current.contains(id)),
        );
    }
    for chunk in stale.chunks(batch) {
        delete_vectors(ferro, collection, chunk)?;
        stats.deleted += chunk.len();
    }
    for item in group {
        let known = item.known_ids();
        let change = if item.no_symbols {
            Change {
                record: None,
                known,
            }
        } else {
            Change::set(
                FileRecord {
                    digest: item.digest.clone(),
                    ids: item.new_ids(),
                },
                known,
            )
        };
        changes.insert(item.path.clone(), change);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendKind, StoreOptions, open_redb, open_store_with};
    use fl_core::{HashingEmbedder, scan_code_index};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    const LONG: Duration = Duration::from_secs(600);
    const SYNC: IndexPolicy = IndexPolicy::Sync {
        budget: DEFAULT_CODE_SYNC_BUDGET,
    };

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const STORE_RS: &str = "\
pub fn checkpoint_store(path: &str) {
    let _ = path;
}
pub fn tokenize_input(text: &str) -> Vec<String> {
    let _ = text;
    Vec::new()
}
pub struct SearchIndex;
pub fn render_snake_case_name() {}
const API_TOKEN: &str = \"sk-live-0123456789\";
";
    const GPU_RS: &str = "\
pub fn arbitrate_gpu_queue() {}
pub fn lunch_menu_planner() {}
";

    /// A scanned code tree and the directory it lives in.
    fn tree() -> (TempDir, CodeIndex) {
        let root = TempDir::new().unwrap();
        write(root.path(), "src/store.rs", STORE_RS);
        write(root.path(), "src/gpu.rs", GPU_RS);
        let index = scan_code_index(root.path());
        (root, index)
    }

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

    fn pair() -> (TempDir, AnyStore, TempDir, AnyStore) {
        let rd = TempDir::new().unwrap();
        let redb = open_redb(&rd.path().join("k.redb")).unwrap();
        let fd = TempDir::new().unwrap();
        let ferro = open_ferro(&fd);
        (rd, redb, fd, ferro)
    }

    fn sidecar(dir: &TempDir) -> PathBuf {
        dir.path().join("k.code.fls")
    }

    fn names(r: &RankedCode) -> Vec<&str> {
        r.outcome.symbols.iter().map(|h| h.name.as_str()).collect()
    }

    fn run(
        store: &AnyStore,
        index: &CodeIndex,
        policy: IndexPolicy,
        query: &str,
    ) -> (RankedCode, CodeSyncStats) {
        search_code_stats(
            store,
            default_embedder(),
            index,
            CodeSearchKind::Search,
            query,
            10,
            policy,
        )
    }

    fn open_side(dir: &TempDir) -> FerroStore {
        FerroStore::open(sidecar(dir)).unwrap()
    }

    fn col(index: &CodeIndex) -> String {
        code_vector_collection_name(default_embedder(), index)
    }

    fn vector_count(ferro: &FerroStore, index: &CodeIndex) -> usize {
        index
            .files()
            .flat_map(|f| f.symbols())
            .filter(|s| {
                ferro
                    .get_vector(&col(index), &id_of_symbol(s))
                    .ok()
                    .flatten()
                    .is_some()
            })
            .count()
    }

    #[test]
    fn redb_is_exactly_the_index_search() {
        let (_root, index) = tree();
        let (_rd, redb, _fd, _ferro) = pair();
        for kind in [
            CodeSearchKind::Def,
            CodeSearchKind::Search,
            CodeSearchKind::Outline,
        ] {
            for q in [
                "checkpoint_store",
                "tokenize",
                "src/gpu",
                "queue",
                "zzz",
                "",
            ] {
                for limit in [0, 1, 3, 50] {
                    let r = search_code(&redb, &index, kind, q, limit, SYNC);
                    assert_eq!(
                        r.outcome,
                        index.search(kind, q, limit),
                        "{kind:?} {q} {limit}"
                    );
                    assert_eq!(r.mode, SearchMode::Bm25);
                    assert_eq!((r.note, r.cause), (None, None));
                }
            }
        }
    }

    #[test]
    fn def_and_outline_on_ferro_are_exactly_today_and_create_no_sidecar() {
        let (_root, index) = tree();
        let (_rd, _redb, fd, ferro) = pair();
        for kind in [CodeSearchKind::Def, CodeSearchKind::Outline] {
            for q in [
                "checkpoint_store",
                "tokenize",
                "src/store",
                "src/gpu",
                "zzz",
            ] {
                let r = search_code(&ferro, &index, kind, q, 20, SYNC);
                assert_eq!(r.outcome, index.search(kind, q, 20), "{kind:?} {q}");
                assert_eq!(r.mode, SearchMode::Bm25);
                assert_eq!((r.note, r.cause), (None, None));
            }
        }
        assert!(!sidecar(&fd).exists());
    }

    #[test]
    fn an_exact_name_query_has_the_same_top_symbol_on_both_engines() {
        let (_root, index) = tree();
        let (_rd, redb, _fd, ferro) = pair();
        for q in [
            "checkpoint_store",
            "tokenize_input",
            "SearchIndex",
            "arbitrate_gpu_queue",
            "lunch_menu_planner",
        ] {
            let a = search_code(&redb, &index, CodeSearchKind::Search, q, 5, SYNC);
            let b = search_code(&ferro, &index, CodeSearchKind::Search, q, 5, SYNC);
            assert_eq!(b.mode, SearchMode::Knowledge, "{q}: {:?}", b.cause);
            assert_eq!(b.note, None);
            assert_eq!(
                a.outcome.symbols[0], b.outcome.symbols[0],
                "top symbol differs for `{q}`"
            );
            assert_eq!(a.outcome.files_indexed, b.outcome.files_indexed);
            assert_eq!(a.outcome.symbols_indexed, b.outcome.symbols_indexed);
        }
    }

    #[test]
    fn fragments_and_typos_find_a_symbol_on_ferro_that_redb_misses() {
        let (_root, index) = tree();
        let (_rd, redb, _fd, ferro) = pair();
        for (q, want) in [
            ("tokenise", "tokenize_input"),
            ("chekpoint", "checkpoint_store"),
            ("case_name_render", "render_snake_case_name"),
            ("input tokens", "tokenize_input"),
        ] {
            let a = search_code(&redb, &index, CodeSearchKind::Search, q, 10, SYNC);
            assert!(!names(&a).contains(&want), "redb finds `{want}` for `{q}`");
            let b = search_code(&ferro, &index, CodeSearchKind::Search, q, 10, SYNC);
            assert!(
                names(&b).contains(&want),
                "ferro misses `{want}` for `{q}`: {:?} {:?}",
                names(&b),
                b.cause
            );
        }
    }

    #[test]
    fn an_unrelated_query_gets_nothing_from_the_vector_leg() {
        let (_root, index) = tree();
        let (_rd, _redb, _fd, ferro) = pair();
        run(&ferro, &index, SYNC, "checkpoint");
        for q in ["quantum entanglement zebra", "xqzvk", "lorem ipsum dolor"] {
            let r = search_code(&ferro, &index, CodeSearchKind::Search, q, 10, SYNC);
            assert_eq!(r.mode, SearchMode::Knowledge);
            assert!(r.outcome.symbols.is_empty(), "{q}: {:?}", names(&r));
            assert_eq!(r.outcome.tokens_saved_this_query, 0);
        }
    }

    #[test]
    fn limit_and_order_are_total_and_deterministic() {
        let (_root, index) = tree();
        let (_rd, _redb, _fd, ferro) = pair();
        let big = search_code(
            &ferro,
            &index,
            CodeSearchKind::Search,
            "queue store",
            50,
            SYNC,
        );
        assert!(big.outcome.symbols.len() > 2, "{:?}", names(&big));
        for limit in 1..=big.outcome.symbols.len() {
            let r = search_code(
                &ferro,
                &index,
                CodeSearchKind::Search,
                "queue store",
                limit,
                SYNC,
            );
            assert_eq!(
                r.outcome.symbols,
                big.outcome.symbols[..limit],
                "limit {limit}"
            );
        }
        for _ in 0..3 {
            let again = search_code(
                &ferro,
                &index,
                CodeSearchKind::Search,
                "queue store",
                50,
                SYNC,
            );
            assert_eq!(again, big);
        }
        let none = search_code(&ferro, &index, CodeSearchKind::Search, "queue", 0, SYNC);
        assert!(none.outcome.symbols.is_empty());
        assert_eq!(none.outcome.files_indexed, index.files_indexed());
    }

    #[test]
    fn the_sidecar_is_made_beside_the_primary_private_and_only_by_sync() {
        let (_root, index) = tree();
        let (_rd, _redb, fd, ferro) = pair();
        let path = sidecar(&fd);
        assert_eq!(sidecar_path(&ferro), Some(path.clone()));
        assert_eq!(path.file_name().unwrap(), "k.code.fls");
        assert_eq!(path.parent(), fd.path().join("k.fls").parent());

        let (r, _) = run(&ferro, &index, IndexPolicy::ReadOnly, "checkpoint_store");
        assert_eq!(r.mode, SearchMode::Knowledge);
        assert_eq!(names(&r), ["checkpoint_store"]);
        assert!(!path.exists(), "ReadOnly created the sidecar");

        let (_, stats) = run(&ferro, &index, SYNC, "checkpoint_store");
        assert_eq!(stats.written, 7);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
        // The primary store holds none of it.
        let missing = ferro.as_ferro().unwrap().get_vector(&col(&index), "x");
        assert!(
            missing
                .unwrap_err()
                .to_string()
                .contains("no vector collection")
        );
    }

    #[test]
    fn read_only_writes_nothing_even_when_the_sidecar_is_stale() {
        let (root, index) = tree();
        let (_rd, _redb, fd, ferro) = pair();
        run(&ferro, &index, SYNC, "checkpoint");
        write(root.path(), "src/store.rs", "pub fn brand_new() {}\n");
        write(root.path(), "src/extra.rs", "pub fn also_new() {}\n");
        let index2 = scan_code_index(root.path());
        let path = sidecar(&fd);
        let before = (
            std::fs::read(&path).unwrap(),
            std::fs::metadata(&path).unwrap().modified().unwrap(),
        );
        let (r, stats) = run(&ferro, &index2, IndexPolicy::ReadOnly, "brand_new");
        assert_eq!(stats, CodeSyncStats::default());
        assert_eq!(r.mode, SearchMode::Knowledge);
        assert_eq!(names(&r), ["brand_new"]);
        let after = (
            std::fs::read(&path).unwrap(),
            std::fs::metadata(&path).unwrap().modified().unwrap(),
        );
        assert_eq!(before, after, "a ReadOnly search changed the sidecar");
        // An empty file is not opened (opening would write a superblock).
        std::fs::write(&path, b"").unwrap();
        let (r, _) = run(&ferro, &index2, IndexPolicy::ReadOnly, "brand_new");
        assert_eq!(r.note, None);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn deleting_the_sidecar_rebuilds_it() {
        let (_root, index) = tree();
        let (_rd, _redb, fd, ferro) = pair();
        run(&ferro, &index, SYNC, "checkpoint");
        std::fs::remove_file(sidecar(&fd)).unwrap();
        let (r, stats) = run(&ferro, &index, SYNC, "tokenise");
        assert_eq!(stats.written, 7, "{stats:?}");
        assert!(names(&r).contains(&"tokenize_input"));
        let side = open_side(&fd);
        assert_eq!(vector_count(&side, &index), 7);
        // Quiet once whole.
        let (_, quiet) = run(&ferro, &index, SYNC, "tokenise");
        assert_eq!(quiet, CodeSyncStats::default());
    }

    #[test]
    fn a_corrupt_or_unwritable_sidecar_falls_back_to_the_exact_answer() {
        let (_root, index) = tree();
        let (_rd, _redb, fd, ferro) = pair();
        let exact = index.search(CodeSearchKind::Search, "checkpoint", 10);
        run(&ferro, &index, SYNC, "checkpoint");
        let path = sidecar(&fd);

        std::fs::write(&path, vec![0xA5u8; 8192]).unwrap();
        for policy in [SYNC, IndexPolicy::ReadOnly] {
            let r = search_code(
                &ferro,
                &index,
                CodeSearchKind::Search,
                "checkpoint",
                10,
                policy,
            );
            assert_eq!(r.mode, SearchMode::Bm25, "{policy:?}");
            assert_eq!(r.note.as_deref(), Some(FALLBACK_NOTE));
            let cause = r.cause.as_deref().expect("cause kept");
            assert!(!FALLBACK_NOTE.contains(cause));
            assert_eq!(r.outcome, exact);
            let mut json = json!({});
            r.annotate_json(&mut json);
            assert_eq!(json, json!({ "note": FALLBACK_NOTE }));
            assert!(
                r.unavailable_line()
                    .unwrap()
                    .starts_with("fl: code index unavailable: ")
            );
        }

        // A directory, then a symlink, where the file should be.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let r = search_code(
            &ferro,
            &index,
            CodeSearchKind::Search,
            "checkpoint",
            10,
            SYNC,
        );
        assert_eq!(r.outcome, exact);
        assert!(r.note.is_some() && r.cause.is_some());
        std::fs::remove_dir(&path).unwrap();
        std::os::unix::fs::symlink(fd.path().join("elsewhere"), &path).unwrap();
        for policy in [SYNC, IndexPolicy::ReadOnly] {
            let r = search_code(
                &ferro,
                &index,
                CodeSearchKind::Search,
                "checkpoint",
                10,
                policy,
            );
            assert_eq!(r.outcome, exact);
            assert!(r.cause.as_deref().unwrap().contains("symlink"));
        }
        assert!(!fd.path().join("elsewhere").exists());
    }

    #[test]
    fn a_store_with_no_file_falls_back_with_a_note() {
        let (_root, index) = tree();
        let mem = AnyStore::in_memory();
        assert_eq!(sidecar_path(&mem), None);
        let r = search_code(&mem, &index, CodeSearchKind::Search, "checkpoint", 10, SYNC);
        assert_eq!(
            r.outcome,
            index.search(CodeSearchKind::Search, "checkpoint", 10)
        );
        assert!(r.note.is_some() && r.cause.is_some());
    }

    #[test]
    fn a_changed_file_loses_its_stale_vectors_and_a_gone_file_all_of_them() {
        let (root, index) = tree();
        let (_rd, _redb, fd, ferro) = pair();
        run(&ferro, &index, SYNC, "checkpoint");
        let side = open_side(&fd);
        let old_ids: Vec<String> = index
            .files()
            .filter(|f| f.path() == "src/store.rs")
            .flat_map(|f| f.symbols().map(|s| id_of_symbol(&s)).collect::<Vec<_>>())
            .collect();
        assert_eq!(old_ids.len(), 5);

        write(
            root.path(),
            "src/store.rs",
            "pub fn checkpoint_store(path: &str) {}\npub fn fresh_symbol_here() {}\n",
        );
        let changed = scan_code_index(root.path());
        let (r, stats) = run(&ferro, &changed, SYNC, "fresh symbol");
        assert_eq!(stats.written, 2, "{stats:?}");
        // Four of the five old ids are stale; `checkpoint_store` keeps its id.
        assert_eq!(stats.deleted, 4, "{stats:?}");
        assert!(names(&r).contains(&"fresh_symbol_here"));
        for id in &old_ids {
            // checkpoint_store moved no lines, so its id is the same and is back.
            let gone = side.get_vector(&col(&index), id).unwrap().is_none();
            let kept = changed
                .files()
                .flat_map(|f| f.symbols().map(|s| id_of_symbol(&s)).collect::<Vec<_>>())
                .any(|n| &n == id);
            assert_eq!(gone, !kept, "{id}");
        }
        assert_eq!(vector_count(&side, &changed), 4);

        std::fs::remove_file(root.path().join("src/gpu.rs")).unwrap();
        let fewer = scan_code_index(root.path());
        let (_, stats) = run(&ferro, &fewer, SYNC, "fresh symbol");
        assert_eq!((stats.written, stats.deleted), (0, 2), "{stats:?}");
        let doc =
            IndexDoc::load(&side, &col(&index), stamp(&default_embedder().space_id())).unwrap();
        assert_eq!(doc.files.keys().collect::<Vec<_>>(), ["src/store.rs"]);
        // The vector leg no longer returns the deleted file's symbols.
        let (r, _) = run(&ferro, &fewer, SYNC, "arbitrate gpu queue");
        assert!(names(&r).is_empty(), "{:?}", names(&r));
        let (_, quiet) = run(&ferro, &fewer, SYNC, "x");
        assert_eq!(quiet, CodeSyncStats::default());
    }

    #[test]
    fn the_budget_limits_new_vectors_and_reports_the_rest_as_pending() {
        let (_root, index) = tree();
        let (_rd, _redb, fd, ferro) = pair();
        // gpu.rs has 2 symbols, store.rs 5. A budget of 3 takes gpu.rs, then
        // store.rs does not fit.
        let (r, stats) = run(
            &ferro,
            &index,
            IndexPolicy::Sync { budget: 3 },
            "checkpoint_store",
        );
        assert_eq!((stats.written, stats.pending), (2, 5), "{stats:?}");
        // Pending files are still found lexically.
        assert_eq!(names(&r), ["checkpoint_store"]);
        let (_, stats) = run(&ferro, &index, IndexPolicy::Sync { budget: 3 }, "x");
        assert_eq!((stats.written, stats.pending), (5, 0), "{stats:?}");
        assert_eq!(vector_count(&open_side(&fd), &index), 7);

        // A budget of 0 writes nothing; a file larger than the budget goes alone.
        let (_rd2, _redb2, fd2, ferro2) = pair();
        let (_, stats) = run(&ferro2, &index, IndexPolicy::Sync { budget: 0 }, "x");
        assert_eq!((stats.written, stats.pending), (0, 7), "{stats:?}");
        let (_, stats) = run(&ferro2, &index, IndexPolicy::Sync { budget: 1 }, "x");
        assert_eq!((stats.written, stats.pending), (2, 5), "{stats:?}");
        assert_eq!(vector_count(&open_side(&fd2), &index), 2);
    }

    #[test]
    fn the_deadline_stops_between_files_and_the_next_sync_finishes() {
        let (_root, index) = tree();
        let d = TempDir::new().unwrap();
        let side = FerroStore::open(d.path().join("s.code.fls")).unwrap();
        // Zero time: the first file (gpu.rs, 2 symbols) is still done, store.rs waits.
        let stats = sync_files(
            &side,
            default_embedder(),
            &col(&index),
            &index,
            100,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!((stats.written, stats.pending), (2, 5), "{stats:?}");
        let doc =
            IndexDoc::load(&side, &col(&index), stamp(&default_embedder().space_id())).unwrap();
        assert_eq!(doc.files["src/store.rs"].digest, PENDING_DIGEST);
        let stats = sync_files(&side, default_embedder(), &col(&index), &index, 100, LONG).unwrap();
        assert_eq!((stats.written, stats.pending), (5, 0), "{stats:?}");
        assert_eq!(vector_count(&side, &index), 7);
    }

    /// Embeds like the default embedder, in the same space, but returns a
    /// vector of the wrong length for the `fail_at`th call (1-based), so that
    /// one `upsert_vector` is refused by the engine.
    struct FlakyEmbedder {
        inner: HashingEmbedder,
        calls: std::cell::Cell<usize>,
        fail_at: usize,
    }

    impl Embedder for FlakyEmbedder {
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        fn embed(&self, text: &str) -> Vec<f32> {
            self.calls.set(self.calls.get() + 1);
            if self.calls.get() == self.fail_at {
                return vec![0.5; 3];
            }
            self.inner.embed(text)
        }
        fn space_id(&self) -> String {
            self.inner.space_id()
        }
    }

    #[test]
    fn an_error_midway_lists_every_committed_vector_and_the_next_sync_repairs_it() {
        let (root, index) = tree();
        let d = TempDir::new().unwrap();
        let side = FerroStore::open(d.path().join("s.code.fls")).unwrap();
        let space = stamp(&default_embedder().space_id());

        // gpu.rs: 2 symbols, store.rs: 5. The 4th vector (the 2nd of store.rs) is bad.
        let flaky = FlakyEmbedder {
            inner: HashingEmbedder::default(),
            calls: std::cell::Cell::new(0),
            fail_at: 4,
        };
        // A batch of 2: gpu.rs commits alone, then store.rs's batch of 5 is
        // refused whole (a batch is all or nothing).
        let err =
            sync_files_batched(&side, &flaky, &col(&index), &index, 100, LONG, 2).unwrap_err();
        assert!(
            err.to_string().contains("vector values have length"),
            "{err}"
        );
        let doc = IndexDoc::load(&side, &col(&index), space).unwrap();
        // Finalised: gpu.rs. Pending, listing every id that might exist: store.rs.
        assert_ne!(doc.files["src/gpu.rs"].digest, PENDING_DIGEST);
        assert_eq!(doc.files["src/store.rs"].digest, PENDING_DIGEST);
        let listed: HashSet<&String> = doc.files.values().flat_map(|f| &f.ids).collect();
        for sym in index.files().flat_map(|f| f.symbols().collect::<Vec<_>>()) {
            let id = id_of_symbol(&sym);
            let has = side.get_vector(&col(&index), &id).unwrap().is_some();
            assert!(
                !has || listed.contains(&id),
                "{} committed but unlisted",
                sym.name()
            );
        }
        assert_eq!(vector_count(&side, &index), 2);

        // store.rs changes before the repair: its half-written ids are still
        // cleaned up, from the pending record.
        write(root.path(), "src/store.rs", "pub fn only_one() {}\n");
        let changed = scan_code_index(root.path());
        let stats =
            sync_files(&side, default_embedder(), &col(&index), &changed, 100, LONG).unwrap();
        assert_eq!(stats.written, 1);
        // All five ids the pending record lists are deleted (the four that
        // never committed are no-ops), so nothing half-written stays.
        assert_eq!(stats.deleted, 5, "{stats:?}");
        assert_eq!(vector_count(&side, &changed), 3);
        let all: usize = side
            .search_vectors(
                &col(&index),
                &default_embedder().embed("only_one"),
                100,
                None,
            )
            .unwrap()
            .len();
        assert_eq!(all, 3, "no orphan vector remains");
        assert_eq!(
            sync_files(&side, default_embedder(), &col(&index), &changed, 100, LONG).unwrap(),
            CodeSyncStats::default()
        );
    }

    #[test]
    fn a_pending_file_with_unchanged_text_is_repaired() {
        let (_root, index) = tree();
        let d = TempDir::new().unwrap();
        let side = FerroStore::open(d.path().join("s.code.fls")).unwrap();
        let flaky = FlakyEmbedder {
            inner: HashingEmbedder::default(),
            calls: std::cell::Cell::new(0),
            fail_at: 3,
        };
        sync_files(&side, &flaky, &col(&index), &index, 100, LONG).unwrap_err();
        let stats = sync_files(&side, default_embedder(), &col(&index), &index, 100, LONG).unwrap();
        assert!(stats.written >= 5, "{stats:?}");
        assert_eq!(vector_count(&side, &index), 7);
    }

    #[test]
    fn a_batched_sync_indexes_the_same_ids_whatever_the_batch_size() {
        let (_root, index) = tree();
        let want: HashSet<String> = index
            .files()
            .flat_map(|f| f.symbols().map(|s| id_of_symbol(&s)).collect::<Vec<_>>())
            .collect();
        assert_eq!(want.len(), 7);
        let mut docs = Vec::new();
        for batch in [1, 2, 3, 5, 7, 256] {
            let d = TempDir::new().unwrap();
            let side = FerroStore::open(d.path().join("s.code.fls")).unwrap();
            let stats = sync_files_batched(
                &side,
                default_embedder(),
                &col(&index),
                &index,
                100,
                LONG,
                batch,
            )
            .unwrap();
            assert_eq!((stats.written, stats.pending), (7, 0), "batch {batch}");
            let got: HashSet<String> = side
                .search_vectors(&col(&index), &default_embedder().embed("store"), 100, None)
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            assert_eq!(got, want, "batch {batch}");
            let doc =
                IndexDoc::load(&side, &col(&index), stamp(&default_embedder().space_id())).unwrap();
            let listed: HashSet<String> = doc.files.values().flat_map(|f| f.ids.clone()).collect();
            assert_eq!(listed, want, "batch {batch}");
            assert!(doc.files.values().all(|f| f.digest != PENDING_DIGEST));
            docs.push(doc.files);
        }
        assert!(docs.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn a_big_sync_is_written_in_batches_and_reopens_from_a_snapshot() {
        let root = TempDir::new().unwrap();
        for f in 0..4 {
            let body: String = (0..120)
                .map(|n| format!("pub fn symbol_{f}_{n}(value: u32) {{}}\n"))
                .collect();
            write(root.path(), &format!("src/f{f}.rs"), &body);
        }
        let index = scan_code_index(root.path());
        let d = TempDir::new().unwrap();
        let path = d.path().join("s.code.fls");
        {
            let side = FerroStore::open(&path).unwrap();
            let stats =
                sync_files(&side, default_embedder(), &col(&index), &index, 1000, LONG).unwrap();
            assert_eq!(stats.written, 480);
            assert_eq!(vector_count(&side, &index), 480);
        }
        // 480 vectors are well under the engine's own 4 MiB trigger, so only
        // the checkpoint after the bulk build can have emptied the log.
        assert_eq!(
            crate::ferrostore::wal_length_on_disk(&path),
            0,
            "the log was not folded into a snapshot"
        );
        let side = FerroStore::open(&path).unwrap();
        assert_eq!(vector_count(&side, &index), 480);
        let quiet =
            sync_files(&side, default_embedder(), &col(&index), &index, 1000, LONG).unwrap();
        assert_eq!(quiet, CodeSyncStats::default());
    }

    #[test]
    fn a_small_sync_leaves_the_log_alone() {
        // The control for the test above: below one batch nothing checkpoints.
        let (_root, index) = tree();
        let d = TempDir::new().unwrap();
        let path = d.path().join("s.code.fls");
        let side = FerroStore::open(&path).unwrap();
        let stats = sync_files(&side, default_embedder(), &col(&index), &index, 100, LONG).unwrap();
        assert_eq!(stats.written, 7);
        assert!(crate::ferrostore::wal_length_on_disk(&path) > 0);
    }

    #[test]
    fn two_concurrent_index_document_merges_both_survive() {
        let (_root, index) = tree();
        let d = TempDir::new().unwrap();
        let path = d.path().join("s.code.fls");
        // Two handles on one file, as two processes would be.
        let (first, second) = (
            FerroStore::open(&path).unwrap(),
            FerroStore::open(&path).unwrap(),
        );
        let space = stamp(&default_embedder().space_id());
        let c = col(&index);
        let rec = |n: usize| FileRecord {
            digest: format!("d{n}"),
            ids: vec![format!("id{n}")],
        };
        // Many rounds of two writers, each adding its own paths from a stale
        // view; every path must be in the document at the end.
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for (who, side) in [&first, &second].into_iter().enumerate() {
                let (c, barrier, rec) = (&c, &barrier, &rec);
                scope.spawn(move || {
                    for round in 0..40usize {
                        barrier.wait();
                        let n = who * 1000 + round;
                        let r = rec(n);
                        let mut changes = Changes::new();
                        changes.insert(
                            format!("src/w{who}_{round}.rs"),
                            Change::set(r.clone(), r.ids),
                        );
                        IndexDoc::update(side, c, space, &changes).unwrap();
                    }
                });
            }
        });
        let doc = IndexDoc::load(&first, &c, space).unwrap();
        assert_eq!(doc.files.len(), 80, "a concurrent merge lost an update");
    }

    /// A record with digest `digest` and ids `ids`.
    fn record(digest: &str, ids: &[&str]) -> FileRecord {
        FileRecord {
            digest: digest.to_string(),
            ids: ids.iter().map(|id| id.to_string()).collect(),
        }
    }

    #[test]
    fn a_concurrent_syncs_ids_for_the_same_path_are_never_orphaned() {
        let (_root, index) = tree();
        let d = TempDir::new().unwrap();
        let path = d.path().join("s.code.fls");
        let (mine, other) = (
            FerroStore::open(&path).unwrap(),
            FerroStore::open(&path).unwrap(),
        );
        let space = stamp(&default_embedder().space_id());
        let c = col(&index);
        let one = |change: Change| {
            let mut changes = Changes::new();
            changes.insert("src/a.rs".to_string(), change);
            changes
        };
        // I planned from a document that listed `old`; meanwhile another
        // process synced the same path to a different text and listed `theirs`.
        IndexDoc::update(
            &mine,
            &c,
            space,
            &one(Change::set(record("d0", &["old"]), vec![])),
        )
        .unwrap();
        let planned = IndexDoc::load(&mine, &c, space).unwrap();
        IndexDoc::update(
            &other,
            &c,
            space,
            &one(Change::set(
                record("dT", &["old", "theirs"]),
                vec!["old".to_string(), "theirs".to_string()],
            )),
        )
        .unwrap();
        assert_eq!(planned.files["src/a.rs"].ids, ["old"]);

        // My intent record lists `old` and `mine`; it must not drop `theirs`.
        let intent = Change::set(
            record(PENDING_DIGEST, &["old", "mine"]),
            vec!["old".to_string(), "mine".to_string()],
        );
        IndexDoc::update(&mine, &c, space, &one(intent)).unwrap();
        let doc = IndexDoc::load(&other, &c, space).unwrap();
        let got = &doc.files["src/a.rs"];
        assert_eq!(got.digest, PENDING_DIGEST);
        let ids: HashSet<&str> = got.ids.iter().map(String::as_str).collect();
        assert_eq!(ids, HashSet::from(["old", "mine", "theirs"]));

        // My final record names `mine` only (`old` was deleted, so it is
        // accounted for), but `theirs` is not mine to drop: the path stays
        // pending with it, for the next sync to repair.
        let done = Change::set(
            record("dM", &["mine"]),
            vec!["old".to_string(), "mine".to_string()],
        );
        IndexDoc::update(&mine, &c, space, &one(done)).unwrap();
        let doc = IndexDoc::load(&other, &c, space).unwrap();
        let got = &doc.files["src/a.rs"];
        assert_eq!(got.digest, PENDING_DIGEST);
        let ids: HashSet<&str> = got.ids.iter().map(String::as_str).collect();
        assert_eq!(ids, HashSet::from(["mine", "theirs"]));

        // A removal of a path another process just rewrote is kept as pending.
        let gone = Change {
            record: None,
            known: vec!["mine".to_string()],
        };
        IndexDoc::update(&mine, &c, space, &one(gone)).unwrap();
        let doc = IndexDoc::load(&other, &c, space).unwrap();
        assert_eq!(doc.files["src/a.rs"], record(PENDING_DIGEST, &["theirs"]));

        // With nothing unaccounted for, the record is applied and removal removes.
        let clean = Change::set(record("dM", &["theirs"]), vec!["theirs".to_string()]);
        IndexDoc::update(&mine, &c, space, &one(clean)).unwrap();
        assert_eq!(
            IndexDoc::load(&other, &c, space).unwrap().files["src/a.rs"],
            record("dM", &["theirs"])
        );
        let gone = Change {
            record: None,
            known: vec!["theirs".to_string()],
        };
        IndexDoc::update(&other, &c, space, &one(gone)).unwrap();
        assert!(IndexDoc::load(&mine, &c, space).unwrap().files.is_empty());
    }

    /// Two files in two roots with the same relative path and the same text.
    fn twin_roots() -> (TempDir, TempDir) {
        let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        for root in [&a, &b] {
            write(
                root.path(),
                "src/lib.rs",
                "pub fn shared_name() {}\npub fn second() {}\n",
            );
        }
        (a, b)
    }

    #[test]
    fn roots_coexist_in_one_sidecar_and_never_delete_each_other() {
        let a = TempDir::new().unwrap();
        write(a.path(), "src/lib.rs", "pub fn alpha_fn() {}\n");
        write(a.path(), "src/more.rs", "pub fn alpha_more() {}\n");
        let b = TempDir::new().unwrap();
        write(b.path(), "lib/other.rs", "pub fn beta_fn() {}\n");
        let (index_a, index_b) = (scan_code_index(a.path()), scan_code_index(b.path()));
        let (_rd, _redb, fd, ferro) = pair();
        assert_ne!(col(&index_a), col(&index_b));

        let (_, first) = run(&ferro, &index_a, SYNC, "alpha");
        assert_eq!((first.written, first.deleted), (2, 0), "{first:?}");
        let side = open_side(&fd);
        let before = std::fs::read(sidecar(&fd)).unwrap();
        assert_eq!(vector_count(&side, &index_a), 2);

        // B is a different tree: none of A's files is "gone" from it.
        let (r, other) = run(&ferro, &index_b, SYNC, "beta");
        assert_eq!((other.written, other.deleted), (1, 0), "{other:?}");
        assert_eq!(names(&r), ["beta_fn"]);
        let side = open_side(&fd);
        assert_eq!(
            vector_count(&side, &index_a),
            2,
            "B's search deleted A's vectors"
        );
        assert_eq!(vector_count(&side, &index_b), 1);
        assert_ne!(before, std::fs::read(sidecar(&fd)).unwrap());

        // And back to A: nothing to write, nothing to delete, still answers.
        let (r, again) = run(&ferro, &index_a, SYNC, "alpha");
        assert_eq!(again, CodeSyncStats::default());
        let mut found = names(&r);
        found.sort_unstable();
        assert_eq!(found, ["alpha_fn", "alpha_more"]);
        let (_, again) = run(&ferro, &index_b, SYNC, "beta");
        assert_eq!(again, CodeSyncStats::default());
        let side = open_side(&fd);
        assert_eq!(vector_count(&side, &index_a), 2);
        assert_eq!(vector_count(&side, &index_b), 1);

        // A subdirectory of A is a root of its own, too.
        let sub = scan_code_index(&a.path().join("src"));
        assert_ne!(col(&sub), col(&index_a));
        let (_, subsync) = run(&ferro, &sub, SYNC, "alpha");
        assert_eq!(subsync.deleted, 0, "{subsync:?}");
        assert_eq!(vector_count(&open_side(&fd), &index_a), 2);
    }

    #[test]
    fn the_same_relative_path_in_two_roots_does_not_collide() {
        let (a, b) = twin_roots();
        let (index_a, index_b) = (scan_code_index(a.path()), scan_code_index(b.path()));
        let (_rd, _redb, fd, ferro) = pair();
        // Same path, kind, name and line: the same symbol id in both roots.
        let id_a: Vec<String> = index_a
            .files()
            .flat_map(|f| f.symbols().map(|s| id_of_symbol(&s)).collect::<Vec<_>>())
            .collect();
        let id_b: Vec<String> = index_b
            .files()
            .flat_map(|f| f.symbols().map(|s| id_of_symbol(&s)).collect::<Vec<_>>())
            .collect();
        assert_eq!(id_a, id_b);
        run(&ferro, &index_a, SYNC, "shared");
        run(&ferro, &index_b, SYNC, "shared");
        let side = open_side(&fd);
        assert_eq!(vector_count(&side, &index_a), 2);
        assert_eq!(vector_count(&side, &index_b), 2);

        // B edits the file: its stale vector goes, A's copy stays.
        write(b.path(), "src/lib.rs", "pub fn second() {}\n");
        let index_b = scan_code_index(b.path());
        let (_, stats) = run(&ferro, &index_b, SYNC, "second");
        assert_eq!(stats.deleted, 2, "{stats:?}");
        let side = open_side(&fd);
        assert_eq!(vector_count(&side, &index_b), 1);
        assert_eq!(
            vector_count(&side, &index_a),
            2,
            "B's edit reached A's vectors"
        );
        let space = stamp(&default_embedder().space_id());
        let doc_a = IndexDoc::load(&side, &col(&index_a), space).unwrap();
        assert_eq!(doc_a.files["src/lib.rs"].ids.len(), 2);
        // Removing B's file deletes only B's.
        std::fs::remove_file(b.path().join("src/lib.rs")).unwrap();
        let (_, stats) = run(&ferro, &scan_code_index(b.path()), SYNC, "second");
        assert_eq!(stats.deleted, 1, "{stats:?}");
        assert_eq!(vector_count(&open_side(&fd), &index_a), 2);
    }

    #[test]
    fn the_root_is_its_canonical_path() {
        let a = TempDir::new().unwrap();
        write(a.path(), "src/lib.rs", "pub fn one() {}\n");
        let direct = scan_code_index(a.path());
        let dotted = scan_code_index(&a.path().join("src").join(".."));
        assert_eq!(
            col(&direct),
            col(&dotted),
            "an alias of a root is another root"
        );
        // A path that cannot be canonicalized is used as given.
        let missing = scan_code_index(&a.path().join("missing"));
        assert_eq!(missing.root(), Some(a.path().join("missing").as_path()));
        assert_ne!(col(&missing), col(&direct));
    }

    #[test]
    fn files_gone_from_a_root_are_deleted_in_bounded_groups() {
        let root = TempDir::new().unwrap();
        for f in 0..6 {
            write(
                root.path(),
                &format!("src/f{f}.rs"),
                &format!("pub fn a_{f}() {{}}\npub fn b_{f}() {{}}\n"),
            );
        }
        let index = scan_code_index(root.path());
        let d = TempDir::new().unwrap();
        let side = FerroStore::open(d.path().join("s.code.fls")).unwrap();
        let c = col(&index);
        let space = stamp(&default_embedder().space_id());
        let stats = sync_files(&side, default_embedder(), &c, &index, 100, LONG).unwrap();
        assert_eq!(stats.written, 12);

        for f in 0..6 {
            std::fs::remove_file(root.path().join(format!("src/f{f}.rs"))).unwrap();
        }
        let empty = scan_code_index(root.path());
        // Groups of about 2 ids: a file each. No time left: the first group
        // is still deleted, the rest wait and are reported as pending.
        let stats = sync_files_batched(
            &side,
            default_embedder(),
            &c,
            &empty,
            100,
            Duration::ZERO,
            2,
        )
        .unwrap();
        assert_eq!((stats.deleted, stats.pending), (2, 10), "{stats:?}");
        let doc = IndexDoc::load(&side, &c, space).unwrap();
        assert_eq!(
            doc.files.len(),
            5,
            "only the deleted group left the document"
        );
        assert_eq!(
            doc.files
                .values()
                .filter(|r| r.digest == PENDING_DIGEST)
                .count(),
            5
        );
        // The next syncs take them up, one group per call here, then none.
        let mut total = 2;
        for left in (0..5).rev() {
            let stats = sync_files_batched(
                &side,
                default_embedder(),
                &c,
                &empty,
                100,
                Duration::ZERO,
                2,
            )
            .unwrap();
            total += stats.deleted;
            assert_eq!(IndexDoc::load(&side, &c, space).unwrap().files.len(), left);
        }
        assert_eq!(total, 12);
        let leftover = side
            .search_vectors(&c, &default_embedder().embed("a_0"), 100, None)
            .unwrap();
        assert!(leftover.is_empty(), "{leftover:?}");
        // With time, one call deletes everything it can in groups.
        write(root.path(), "src/x.rs", "pub fn x() {}\n");
        let one = scan_code_index(root.path());
        sync_files(&side, default_embedder(), &c, &one, 100, LONG).unwrap();
        let stats = sync_files(&side, default_embedder(), &c, &empty, 100, LONG).unwrap();
        assert_eq!((stats.deleted, stats.pending), (1, 0));
    }

    #[test]
    fn the_sidecar_path_is_made_from_the_real_path_of_the_store() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new().unwrap();
        // A directory name that is not UTF-8: `display()` would turn it to U+FFFD.
        let odd = dir.path().join(OsStr::from_bytes(b"caf\xe9"));
        std::fs::create_dir(&odd).unwrap();
        let store = open_store_with(
            &odd.join("k.redb"),
            &StoreOptions {
                backend: Some(BackendKind::Ferro),
                ..StoreOptions::default()
            },
        )
        .unwrap();
        let want = odd.join("k.code.fls");
        let got = sidecar_path(&store).unwrap();
        assert_eq!(got, want);
        assert_eq!(got.parent().unwrap(), odd);
        // And it works end to end.
        let (_root, index) = tree();
        let r = search_code(
            &store,
            &index,
            CodeSearchKind::Search,
            "checkpoint",
            5,
            SYNC,
        );
        assert_eq!(r.mode, SearchMode::Knowledge, "{:?}", r.cause);
        assert!(want.exists());
    }

    #[test]
    fn token_gain_is_booked_by_the_same_rule_on_both_engines() {
        let (_root, index) = tree();
        let (_rd, redb, _fd, ferro) = pair();
        // Exact kinds: byte-identical outcomes.
        for kind in [CodeSearchKind::Def, CodeSearchKind::Outline] {
            let q = if kind == CodeSearchKind::Def {
                "checkpoint_store"
            } else {
                "src/"
            };
            let a = search_code(&redb, &index, kind, q, 20, SYNC);
            let b = search_code(&ferro, &index, kind, q, 20, SYNC);
            assert_eq!(a.outcome, b.outcome);
        }
        // Search: the same hits give the same gain on either engine.
        let a = search_code(
            &redb,
            &index,
            CodeSearchKind::Search,
            "checkpoint_store",
            20,
            SYNC,
        );
        let b = search_code(
            &ferro,
            &index,
            CodeSearchKind::Search,
            "checkpoint_store",
            20,
            SYNC,
        );
        let from_a = outcome_from_hits(
            a.outcome.symbols.clone(),
            a.outcome.files_indexed,
            a.outcome.symbols_indexed,
        );
        assert_eq!(from_a, a.outcome);
        let again = outcome_from_hits(
            b.outcome.symbols.clone(),
            b.outcome.files_indexed,
            b.outcome.symbols_indexed,
        );
        assert_eq!(again, b.outcome);
        // A ferro result with extra vector hits is still per distinct file.
        let wide = search_code(&ferro, &index, CodeSearchKind::Search, "tokenise", 20, SYNC);
        let mut per_file: std::collections::BTreeMap<&str, (u64, u64)> = Default::default();
        for h in &wide.outcome.symbols {
            let e = per_file
                .entry(&h.path)
                .or_insert((h.estimated_file_tokens, 0));
            e.1 += h.snippet_tokens;
        }
        let want: u64 = per_file.values().map(|(f, s)| f.saturating_sub(*s)).sum();
        assert_eq!(wide.outcome.tokens_saved_this_query, want);
        assert!(want > 0);
    }

    #[test]
    fn json_is_unchanged_except_mode_on_a_hybrid_answer() {
        let (_root, index) = tree();
        let (_rd, redb, _fd, ferro) = pair();
        let a = search_code(&redb, &index, CodeSearchKind::Search, "checkpoint", 5, SYNC);
        let mut plain = json!({ "query": "checkpoint" });
        a.annotate_json(&mut plain);
        assert_eq!(plain, json!({ "query": "checkpoint" }));
        assert_eq!(a.unavailable_line(), None);
        let b = search_code(
            &ferro,
            &index,
            CodeSearchKind::Search,
            "checkpoint",
            5,
            SYNC,
        );
        let mut hybrid = json!({ "query": "checkpoint" });
        b.annotate_json(&mut hybrid);
        assert_eq!(
            hybrid,
            json!({ "query": "checkpoint", "mode": "knowledge" })
        );
    }

    #[test]
    fn only_the_declaration_is_embedded_never_the_body() {
        assert_eq!(
            embed_text(
                "tokenize_input",
                "pub fn tokenize_input(text: &str)",
                "src/store.rs"
            ),
            "tokenize_input\npub fn tokenize_input(text: &str)\nsrc/store.rs"
        );
        // An initialiser is a body: the value never reaches the embedder, so
        // the sidecar holds no lossy copy of it.
        let text = embed_text(
            "API_TOKEN",
            "const API_TOKEN: &str = \"sk-live-0123456789\";",
            "src/store.rs",
        );
        assert_eq!(text, "API_TOKEN\nconst API_TOKEN: &str\nsrc/store.rs");
        let (_root, index) = tree();
        let token = index
            .files()
            .flat_map(|f| f.symbols().collect::<Vec<_>>())
            .find(|s| s.name() == "API_TOKEN")
            .unwrap();
        assert!(
            token.signature().contains("sk-live"),
            "fixture has the secret"
        );
        assert!(!embed_text(token.name(), token.signature(), token.path()).contains("sk-live"));
    }
}
