# FerroStore v2: Engine, Security Floor and Production Cutover (`crates/store`)

* **Date:** 2026-11-01
* **Author:** Felicity (`felicity@artificialhumanity.io`)
* **Status:** The owner ratified the decisions in §1. The engine (§3–§6) is for implementation on `felicity/ferrostore-v2-*`. The seam and wiring (§8) are on `feira/ferrostore-cutover`.
* **Target crates:** `crates/store` (`fl-store`: `ferrostore/`, `migrate.rs`, a new `ferrostore` binary). The `fl store` subcommands go in `crates/cli`.
* **Builds on:** the FerroStore v1 container: two superblock slots, copy-on-write snapshots and a CRC32-framed WAL (`ferrostore/layout.rs`). It also builds on the 13 store traits that `RedbStore` and `FerroStore` both implement.

---

## 1. Ratified decisions

| # | Decision |
|---|---|
| 1 | **Engine v2 now.** An epoch-cached state handle with incremental WAL tail replay and copy-on-write in-memory maps. Commits use `fdatasync` and group commit. Compaction writes a sibling inode and detects the swap by `stat`/`fstat` inode. The `.fls` superblock carries a version and an engine field, so a disk-paged B-tree can come later. |
| 2 | **Multi-writer.** Concurrent network writes run on the server and stay conflict-safe. FerroStore's `SyncAdapter` is the primary replication between nodes. Git (`refs/heads/fl/state`) is the secondary path. |
| 3 | **Cutover is one verified switch for all stores.** The `.redb` stays on disk, read-only, behind a config switch for instant rollback. Migration is checked per table, by row count and content hash. |
| 4 | **Network security.** PSK/HMAC challenge-response, Bearer tokens and native `rustls` `wss://` on `FerroStoreServer`. A plain `ws://` bind to a non-loopback address is refused unless it is explicitly allowed on a trusted Tailscale interface. Files are 0600, directories 0700, and the socket follows O-13. |
| 5 | **Embeddings.** A lightweight CPU embedder runs in-process, so FerroState hybrid search (HNSW plus BM25 with RRF fusion) never needs an external daemon. This is Feira's track (§8). |
| 6 | **Standalone.** A `ferrostore` binary and `fl store` provide `serve`, `query`, `put`, `get`, `compact`, `checkpoint`, `migrate`, `verify`, `bench` and `inspect`. |

**Non-goals for v2:**
- No paged B-tree and no mmap. The whole state stays in RAM in each process that opens the store embedded.
- No query language beyond the current trait methods and `query_by_field` / BM25 / vector search.
- No change to the 13 trait signatures.

---

## 2. Current engine and why it is not enough

These references are to `origin/main` `f0e2756`.

**Reads.**
- `read_consistent` decodes the whole snapshot and replays the whole WAL on every `with_read` (`ferrostore/mod.rs:2081`).
- The trait impls make 52 `with_read` calls.

**Writes.**
- `with_write_disk` and `commit_disk` decode the whole file under `LOCK_EX` (`mod.rs:1876-1913`).
- Each write then clones the full maps into a `Txn` (`mod.rs:1158`).
- `append_frame` makes 2 `sync_all` calls per commit (`mod.rs:2007`).

**Served mode.**
- `read_state` sends the whole `DocState` to the client (`server.rs:673`, `:1260`) inside a 16 MiB frame cap (`server.rs:31`).
- The hub's `fl.redb` is 18 MB today, so served mode cannot carry the main store.

**Security.**
- `FerroStore::open` creates the file with the umask's mode (`mod.rs:223`).
- `bind_tcp` and `ws://` have no authentication, and `wss://` is refused.

**Missing parity.**
- `export_manifest`, `import_manifest` and `format_version` are missing.
- There is no schema ladder.

**Disk growth.** `allocate_extent` only appends (`mod.rs:1993`), so every checkpoint leaves a dead extent behind. Because of the same append-only rule, lock-free readers are safe today. §5 keeps them safe once space is reclaimed.

---

## 3. Track 0: the gate before any cutover

Each item ships with tests and comes before the cutover switch.

### 3.1 O-13 permissions

**New store file.**
- `FerroStore::open` creates a missing file through `crate::private::create_private_file`, which gives mode 0600 from the first byte.
- It creates a missing parent directory through `create_private_dir_all`, which gives mode 0700.
- After the file is created and its first superblock is written, it calls `fsync` on the parent directory.

**Existing store file.** An existing file is not chmodded. It gets `private::warn_if_loose`, the same as `RedbStore`.

**Server socket.**
- The Unix socket is created inside a 0700 directory, and set to 0600 before `listen`.
- A stale socket is removed only after a refused `connect`, never before.

**Tests.**
- The new file and directory have the modes above when the umask is 022.
- An existing loose file gets a warning and is not changed.
- The socket mode is 0600.

### 3.2 Format and schema versions

There are two separate numbers.

**Container format.** This is `FORMAT_VERSION` in the superblock, bytes 8..12.

| Version | Layout | Status |
|---|---|---|
| v1 | Today's layout, in which bytes 64.. must be zero | Still readable |
| v2 | Adds the `engine: u32` field at bytes 64..68. `0` means snapshot plus WAL. It also adds the `incompat: u64` feature mask at bytes 68..76. | Written from v2 on |

- `parse_superblock` returns a three-way result: valid, invalid, or **newer than this binary**.
- A newer version, or an unknown `incompat` bit, makes `open` fail with a named error that gives the version found. Today's "no valid superblock" error does not do this.
- A v1 file is upgraded to v2 at its next slot write. Both slots stay CRC-valid throughout.

**Schema version.** This is the doc `("__meta", "schema_version")`. It uses the **same ladder as `RedbStore`**: `FORMAT_VERSION` … `FORMAT_WITH_WORKFLOW` in `lib.rs`.
- Because both backends share one ladder, `format_version()` means the same thing on both.
- A new FerroStore starts at the highest constant.
- `migrate_redb_to_ferrostore` copies the source's version.
- `open` refuses a schema that is newer than the binary knows.
- A future schema change adds one constant plus an upgrade step for **both** backends, in the same PR.

`FerroStore::format_version(&self) -> Result<Option<u64>, StoreError>` mirrors `RedbStore::format_version`.

### 3.3 Manifest parity

`FerroStore::export_manifest` and `FerroStore::import_manifest` have the signatures, errors and `ImportReport` of `RedbStore` (`lib.rs:907`, `:1012`).
- `import_manifest` is one write transaction, so a failure leaves nothing applied.
- The existing redb manifest tests become a conformance suite that runs against both stores.

### 3.4 Migration verifier

```rust
pub struct TableParity {
    pub table: String,
    pub redb_rows: u64,
    pub ferro_rows: u64,
    pub redb_hash: [u8; 32],
    pub ferro_hash: [u8; 32],
}
pub struct ParityReport { pub tables: Vec<TableParity>, pub trait_views: Vec<TableParity> }
impl ParityReport {
    pub fn is_clean(&self) -> bool;
    pub fn mismatches(&self) -> Vec<&TableParity>;
}
pub fn verify_redb_vs_ferrostore(src: &RedbStore, dst: &FerroStore)
    -> Result<ParityReport, StoreError>;
```

**How the hashes are made.**
- `tables` holds one entry for each redb table that `migrate.rs` copies.
- Each hash is SHA-256 over the rows sorted by key, encoded as `len(key) ‖ key ‖ len(value) ‖ canonical-JSON(value)`.
- Canonical JSON has sorted object keys and no whitespace.
- A raw `&str` value is hashed as the JSON string that the migration stores, so the heuristic in `put_raw` is checked, not assumed.

**Trait views.** `trait_views` runs every list-style trait method on both stores and hashes the results the same way. This catches a table-to-collection mapping that row counts alone would pass.

**Return value.**
- The result is `Ok(report)` even when the stores differ.
- It is `Err` only for an I/O or decode failure.

**The verified cutover sequence.** `open_store` (§8) and `ferrostore migrate` both use this sequence:
1. Migrate into a sibling temp file with a name unique to this call: process id, a counter and a nonce. It is created with `create_new`, mode 0600. A call removes only a temp it created.
2. Verify. If the report is not clean, delete the temp file and keep running on redb.
3. `fsync` the file. Check that the temp path still names the inode that was verified (by `dev` and `ino`).
4. `link` it to `<name>.fls`, then `unlink` the temp. Unlike `rename`, `link` refuses a `.fls` that another process created in the meantime.
5. `fsync` the directory.
6. `chmod 0400` the `.redb`. It is kept, not deleted.

After step 4 the `.fls` is the store. A later failure in cleanup, `fsync` or `chmod` is a warning, not an error.

The `.redb` stays open, and so keeps redb's exclusive lock, from before step 1 until after step 6. No other process can write it after it is copied. Before step 1, the call removes this target's own temp files that a crash left: regular files, owned by the current user, not modified for 24 hours.

**Acceptance.** A dry run against a copy of the real `~/.local/share/fl/fl.redb` gives `is_clean()`. Record the row counts and the time taken in the PR.

**Reserved collections.** Collections whose names start with `__` belong to the store, and `__meta` is one of them. The public document API refuses them.
- Sync sends every committed op, so the stream of sequence numbers has no gaps.
- A receiver applies `__meta/schema_version` only as a raise to a value on the ladder that is higher than its own. It drops any other reserved op, but the batch's sequence number still advances.
- A version newer than the binary knows is refused.

### 3.5 Crash-consistency harness

**The I/O layer.** All container I/O goes through one internal trait: `write_at`, `read_at`, `sync_data`, `sync_all`, `len`, `set_len` and `rename`. The real implementation wraps `File`.

**The fault model.** The test implementation records every operation and can "cut power" after operation *N*:
- A write that is not followed by a sync is either kept, dropped, or torn at a 512-byte boundary.
- Each of the three choices is enumerated.

**The workload.** For a scripted workload, the harness enumerates every cut point. The workload is:
1. Puts and deletes.
2. A column-table append.
3. A vector upsert.
4. A checkpoint.
5. A group commit of three writers.
6. Once §5 lands, a compaction.

**What is checked after each cut.** The harness reopens the store and checks:
- `open` succeeds;
- every commit acknowledged before the cut is present;
- no commit that was not acknowledged is partly applied;
- no two slots share an epoch;
- `last_seq` is monotonic.

The harness runs in `cargo test`. It must stay under 30 s, so it uses a small workload and no real fsync.

---

## 4. Track 1: the engine

### 4.1 Epoch-cached handle and tail replay

Each `Disk` keeps a cache:

```text
Cache {
  dev_ino:     (dev, ino),
  sb:          Superblock,
  state:       Arc<DocState>,
  wal_applied: u64,
}
```

`wal_applied` is the number of WAL bytes already folded into `state`.

**The refresh.** `refresh()` reads only the two 4 KiB slots, then takes one of these branches:
- **Same `dev_ino` and the same epoch:** return the cached `Arc`. No other I/O.
- **Same snapshot extent and WAL offset, and a longer `wal_length`:** read only `[wal_offset + wal_applied, wal_offset + wal_length)`, decode those frames, and apply them through COW (§4.2).
- **Anything else** (a checkpoint, a compaction, or a different inode): do a full load, as today.

**Which paths use it.**
- **Reads** (`with_read`) refresh without a lock.
- **Writes** refresh under `flock(LOCK_EX)`, so the CPU and I/O cost of a write is O(new frames + ops), not O(DB).

**Correctness rule.** A lock-free reader only ever reads bytes inside the extents that one published superblock names. §5 keeps those bytes valid for any reader that still holds that superblock's inode.

### 4.2 Copy-on-write state with no per-write clone

`DocState` becomes two levels of `Arc`:

```text
docs:    BTreeMap<String, Arc<BTreeMap<String, Arc<Value>>>>   // collection -> id -> doc
tables:  BTreeMap<String, Arc<ColumnTable>>
vectors: BTreeMap<String, Arc<VectorCollection>>
```

**`Txn` is an overlay** on a base `Arc<DocState>`.
- It holds a `BTreeMap<(String, String), Option<Arc<Value>>>` of pending puts and deletes, plus per-collection staged columnar and vector changes.
- `get` and `scan` read the overlay first and merge it over the base.

**Commit** applies the overlay with `Arc::make_mut`, first on the outer map and then on each touched collection. The cost depends on whether anyone else holds the state:
- **When no reader holds the old state:** O(ops).
- **When a reader does:** O(collections) plus O(size of the touched collections). It is never O(DB).

No new dependency. The `im`/`imbl` persistent maps are MPL-2.0. Per-collection `Arc` gets the bound we need without them.

**Known limit (vectors).** An HNSW upsert or delete can fail partway, so a vector op stages a copy of the touched `VectorCollection`. A vector write therefore costs O(that collection) even when no reader holds the state. It is never O(DB). Docs and column tables do meet the O(ops) bound.

The fix is a follow-up: split the upsert into a search phase that only reads and an apply phase that cannot fail. The apply phase then runs in place under `make_mut`.

### 4.3 Durability and group commit

**Sync.** Frame bytes are synced with `fdatasync`, which still flushes the file size. The slot write is also followed by `fdatasync`. `sync_all` stays only where the inode's metadata must also be durable: create, set_len and rename.

**What one group does.** Writers in one process join a group:
1. A leader takes the process's write mutex and the `flock`.
2. It refreshes.
3. Each queued writer then runs its own body in turn, on its own thread, against the group's running state.
4. A body that fails discards only its own overlay. The other writers in the group are not affected.
5. The leader appends every frame.
6. It calls `fdatasync` once.
7. It writes one slot, calls `fdatasync`, and releases the `flock`.
8. Then it wakes every writer with its own result.

**Rules.**
- A commit is acknowledged, and `on_commit`, the sync registration and MQ notification run, only after the slot's `fdatasync`.
- The `flock` is held from the first frame write until the slot is published. Without that, a writer in another process could overwrite frames that are written but not yet published.
- Group size is capped by bytes (4 MiB) and by count (64). There is no added latency: a group is whatever is queued when the leader takes the lock.

### 4.4 Auto-checkpoint

After a commit, the writer checkpoints when the WAL is longer than `max(snapshot_length, 4 MiB)`. It does so in a lock window of its own, after the commit has released its locks and every writer in its group has its result, so no other commit waits for the checkpoint. `FerroStore::checkpoint` stays public.

### 4.5 Remote write retry

A remote CAS retry is bounded to 8 attempts, with jittered exponential backoff from 5 ms to 500 ms. When the attempts run out, it returns the existing `StoreError::Conflict { id, detail }`. Write bodies must be free of side effects, because they may run more than once. A test runs every trait write body twice against the same state.

---

## 5. Track 1: compaction by sibling inode

**When it runs.** `FerroStore::compact()` runs on demand (and through `ferrostore compact`, §7). Auto-compaction follows a checkpoint whose file has more dead bytes than live ones (more than half the file) and more than 64 MiB (`AUTO_COMPACT_BYTES`). *Dead bytes* are the file length less the live extents (the header, the live snapshot and the live WAL); `FerroStore::stats()` reports them. Auto-compaction takes a lock window of its own after the checkpoint's, as §4.4 does, never inside a group's. A failed fsync poisons the handle; any other failure leaves the old file, backs off by another threshold of dead bytes and warns once per run.

**The compactor's steps.** It first sweeps this store's stale `.compact-` temps (24 hours, the migration's rule). While it holds the write mutex and `flock` on the **current** inode, the compactor:
1. Refreshes.
2. Creates `<name>.fls.compact-<pid>-<n>-<nonce>` with `O_EXCL`, mode 0600, and takes that file's own `flock`. `<name>` is the store's real path: `open` resolves the given path once, and every name operation uses it, so a store opened through a symlink compacts beside its file and the link still names it.
3. Writes a v2 header (slot 0 only), a snapshot of the current state and an empty WAL. The epoch is the old file's current epoch + 1; `last_seq` is kept.
4. Calls `fsync` (`sync_all`) on that file.
5. Renames it over `<name>.fls`.
6. Calls `fsync` on the directory.
7. Installs the state in its own cache and moves its own descriptors to the new file.
8. Only then releases the new file's lock and the old inode's lock.

A failure before the rename removes the temp and leaves the old file untouched. The new file's own lock keeps any writer from committing into it before the compactor's directory sync. A failed directory sync poisons the compactor and releases that lock with the name not durable.

**A handle never commits into a file whose name it has not itself made durable.** Each write descriptor carries `name_durable`: true when this handle created the file at open, or compacted into it, and its directory sync succeeded; false when `open` found the file already there, or a swap check reopened the path. Before the first commit, checkpoint or compaction through a descriptor whose `name_durable` is false, under the `flock` and after the swap check has validated the identity, the writer calls `fsync` on the directory and sets it true. A failure refuses the write before any byte is written and poisons the handle: after a failed `fsync` a later one can report success without the entry on disk, so a retry is not trusted. Reads never sync the directory.

**Swap detection is mandatory on every path.**
- After a writer takes `flock` on its descriptor, it compares `stat(path)` `(dev, ino)` with `fstat(fd)`.
- If they differ, it unlocks, reopens the path for both its write and read descriptors, and tries again.
- A reader refresh makes the same check before it trusts its cache.
- Without this check, a writer on the old inode would commit into an unlinked file and the write would be lost.

**Readers.** A reader already reading the old inode finishes on it. The old inode stays valid until its last descriptor closes. Descriptors are `Arc`s swapped under short mutexes, so reads stay lock-free.

**Epochs across inodes.** The old inode is never published after the rename, and the new one starts one epoch past it, so the epoch orders states along the path's history. The cache orders two states of one inode by epoch. Between two inodes, the one the path names (as the last swap check found it) wins whatever the epochs, so a file put in place at a lower epoch (a restore by rename) is read at once, and an install from an inode the path no longer names never replaces a state of the current one. Reads stay monotonic along the path's history across the swap.

**Tests.**
- Compaction runs while another process (the test binary re-run as a child writer) keeps writing. Every acknowledged write is present afterwards.
- A writer blocked on the old inode's lock during the rename lands its write in the new file.
- A reader part way through a tail read of the old inode finishes on it, then moves to the new inode and never goes back.
- The crash harness covers each step of the compaction, and checks that exactly one file is the store after recovery.
- After a compactor's failed directory sync, another handle syncs the directory before its first write into the new file (or, when that sync fails too, refuses the write), and a crash then loses no acknowledged commit.
- A file renamed in at a lower epoch is read at the next refresh; a slow read of the old inode never replaces its state.
- A store opened through a symlink compacts, and the link still names the store.

---

## 6. Track 1: the server, authentication and TLS

### 6.1 Operations run on the server

`read_state` and `commit_if_seq` are removed from the client protocol. A remote client never receives the whole state.

**Why not one RPC per trait method.** Most of the 13 traits' roughly 110 methods are bodies over one small set of `Txn` read primitives:
- `get`, `visit`/`scan` of a collection, `query_by_field`;
- `table_columns`, a column scan;
- `vector` (a collection's parameters, `get_vector`, search);
- `next_mq_seq`;
- the schema version.

So making those primitives remote covers every trait method, and also public `transaction` bodies, which are closures and cannot run on the server. One RPC per method would cover only the trait methods. Typed per-method RPCs stay possible later as fast paths for hot read-only methods.

**Remote `Txn`: lazy reads at a pinned snapshot.**
- **Opening a session.** A remote `with_read`, `with_write_outcome` or `transaction` opens a *read session*. The server pins its current `Arc<DocState>` under a session id and returns it with `last_seq` as `base_seq`. Holding the `Arc` is cheap because of copy-on-write (§4.2).
- **Lifetime.** A session ends when it commits, aborts or closes. Otherwise it expires after 30 s idle. At most 64 sessions are open per connection, and 1024 per server; the oldest is refused first.
- **Reads.** Each read primitive is one RPC against the pinned snapshot. So every read in one body sees one consistent state, the same guarantee an embedded `Txn` gives.
- **Writes** are staged in the client's overlay, as on disk today.
- **The read set.** The client records each read: the key, plus a 32-byte hash of the canonical result. For `get`, the key is the collection and id, and the hash covers the value or its absence. For a scan or a query, the key is the collection or the query, and the hash covers the whole ordered result.

**Commit: optimistic concurrency, validated by value.**
- **The request.** `commit_txn { session, base_seq, reads: [(key, hash)], ops, req }`.
- **Validation.** The server runs this as an ordinary write body through group commit (§4.3). Inside the write lock it recomputes every read against the current committed state and compares the hashes:
  - If all are equal, it commits `ops`.
  - If any differs, nothing is written and the reply is `lost`.
- **Why this is serializable.** The transaction's reads return the same values at the commit point, and validation and commit are atomic under the lock. A scan or query is validated on its whole result, so phantoms are caught.
- **Retry.** The client retries a lost commit with a new session, up to the §4.5 bound (8 attempts, then `Conflict`).
- **Read-only bodies** close their session without a commit.

**Vectors in a remote transaction.**
- Vector writes (`upsert_vector`, `delete_vector`, create) are staged as ops. The server applies HNSW, and the client never holds a collection.
- A body that reads a vector it wrote in the same transaction is refused remotely with a named error. That case is rare, and supporting it would mean shipping the collection.
- Vector search and `get_vector` are read primitives validated by result hash.

**Exactly-once writes.** `commit_txn` and every public write RPC carry `req`. The server keeps a bounded table of recent `req` ids and their results, holding 10 000 entries for up to 10 minutes. A retried `req` returns the first result and is not applied twice. That covers a transport retry after a lost reply. A CAS retry (`lost`) gets a fresh `req`.

**Result size.** The 16 MiB frame cap stays. A read primitive whose result would exceed it is an error that names the primitive and the collection. Paginated scans are a follow-up and a known limit.

**Cost.** A body makes one round trip per distinct read, and repeated reads of the same key in one session are served from the client's cache. On a Unix socket that costs tens of microseconds per read. A body that scans a large collection still moves that collection, but nothing else.

### 6.2 Authentication

**Unix socket.**
- The socket mode is 0600 (§3.1).
- The server checks `SO_PEERCRED`: the peer's uid must equal the server's.
- A PSK is optional.

**TCP, `ws://` or `wss://`.** Every connection authenticates before any operation, by one of two methods.

- **PSK challenge-response.** Each side proves itself to the other, with direction-separated labels, the same pattern as the FerroWire W-1 reflection fix:
  1. The server sends `ns`.
  2. The client sends `nc` and `HMAC-SHA256(psk, "fl-store/1/client" ‖ server_id ‖ ns ‖ nc)`.
  3. The server replies with `HMAC-SHA256(psk, "fl-store/1/server" ‖ server_id ‖ nc ‖ ns)`.
  4. Each side compares in constant time.

- **Bearer token** in the `Authorization` header of the `ws`/`wss` upgrade.
  - The server stores only SHA-256 hashes of tokens, in a 0600 token file.
  - Each token has a scope: `read`, `write` or `admin`. `admin` covers compact, checkpoint and migrate.

A failed authentication closes the connection after a fixed delay. It says nothing about which check failed.

### 6.3 TLS and bind policy

`wss://` uses `rustls` with a certificate and key from config.
- By default the client verifies against the system roots.
- It can instead pin a CA file or an SPKI SHA-256.

**Bind policy for `ws://`.**
- Plain `ws://` is refused on any non-loopback address, unless the config's `allow_plaintext_on = ["tailscale0"]` names the interface **and** the bound address is on that interface.
- Authentication is still required on Tailscale.

**Dependencies.** `rustls` 0.23 (with `ring`) and `rustls-pki-types` are already workspace dependencies. HMAC uses `ring::hmac`, the same as `crates/wire/src/peer.rs`. If roots are needed, add `webpki-roots` or `rustls-native-certs`, which are Apache-2.0/MIT/ISC.

---

## 7. The standalone `ferrostore` binary

**Where it lives.** `crates/store/src/bin/ferrostore.rs` is a thin argument parser over a library module, `fl_store::ferrostore::cli`. `fl store <cmd>` in `crates/cli` calls the same module, so the two never drift.

| Command | What it does |
|---|---|
| `serve` | Unix, `ws` or `wss` server under the auth and bind policy in §6 |
| `get` / `put` / `query` | Doc get and put, and `query_by_field` / BM25 / vector query, as JSON on stdout |
| `checkpoint` / `compact` | §4.4 and §5 |
| `migrate` / `verify` | §3.4 |
| `inspect` | Superblocks, epochs, format, engine and schema versions, extents, dead bytes and per-collection counts |
| `bench` | Put, get, scan and vector-search workloads against FerroStore and redb. Reports p50, p99 and throughput. SQLite and Turso come in Track 4. |

**Syncing the standalone repo.** Syncing this into `Artificial-Humanity/FerroStore` is a push to a public repo. Ferris or the owner does that push.

---

## 8. Ownership split

| Area | Owner |
|---|---|
| §3–§7, everything inside `crates/store/src/ferrostore/`, plus `migrate.rs` and the binary | Felicity |
| `src/any.rs` (`AnyStore`, `open_store`, backend config switch, the auto-migration trigger that calls §3.4) | Feira |
| `src/ferrostore/compat.rs` (`label`, `holds_a_ledger_root`, `owns`, `imported_hash`, `refuse_if_imported`, `set_routes`, `holds_routing`) | Feira |
| Migrating the 91 `RedbStore::open` call sites | Feira |
| The CPU embedder | Feira |
| FerroState, FerroWire and `fl stats` native wiring | Feira |

**Rules between the two tracks.**
- `compat.rs` uses only the trait methods and the public `FerroStore` API. Track 1 rewrites `Txn`, `DocState` and the `pub(super)` helpers.
- `open_with_retries` passes straight through to `open` for FerroStore.
- **Before `HubStoreGuard` is removed**, audit the callers that rely on two trait calls being atomic together. Any such pair needs `FerroStore::transaction(|txn| …)`, a public multi-op transaction, which Track 1 exposes.

---

## 9. Order and gates

| PR | Content | Gates |
|---|---|---|
| 1 | §3 (Track 0) and this spec | The embedded cutover |
| 2 | §4.1–§4.4 | The embedded cutover |
| 3 | §5 compaction | The embedded cutover. It can be shipped later only if `fl.fls` growth is measured and bounded. |
| 4 | §6 | Served or networked mode, and multi-writer over the network |
| 5 | §7 | Nothing |

- **Feira's cutover switch** waits for PRs 1–3.
- **Hub-hosted served mode** waits for PR 4.

Each PR goes through the standard loop: implementer, independent review, fix, re-review.

## 10. Risks and open items

- **Memory.** Every process that opens the store embedded holds the whole state. Before the cutover, measure the state's size in RAM against the 18 MB on disk.
- **The 16 MiB result cap.** The list RPCs keep it until pagination lands.
- **Drift in the standalone repos.** The `Artificial-Humanity/FerroStore` and FerroTrack copies drift from `crates/store` until each sync.
- **Scale target.** The owner set no scale target. The design limit is RAM per process. A paged engine slots in behind the v2 `engine` field.
