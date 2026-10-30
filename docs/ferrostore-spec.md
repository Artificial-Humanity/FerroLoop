# FerroStore

FerroStore is the phase 1 storage engine. It keeps an object/document collection and a FerroMQ log in one `.fls` file, or entirely in memory. Later phases add network serving, columnar storage, and vector search; this version does not implement them.

Integers on disk are little-endian. The CRC-32 is ISO-HDLC: polynomial `0xEDB88320`, init `0xFFFFFFFF`, xorout `0xFFFFFFFF` (the reflected zlib polynomial). A checksum covers the bytes named below and nothing else.

## File layout

A `.fls` file begins with two 4096-byte superblocks.

| Range | Role |
| --- | --- |
| `0..4096` | Superblock slot 0 |
| `4096..8192` | Superblock slot 1 |
| `8192..` | Snapshot and WAL bytes |

Creating a missing file writes slot 0 and leaves slot 1 as zeros, then `fsync`s:

| Field | Initial value |
| --- | --- |
| `epoch` | `1` |
| `snapshot_offset` | `8192` |
| `snapshot_length` | `0` |
| `wal_offset` | `8192` |
| `wal_length` | `0` |
| `snapshot_seq` | `0` |

The create path holds the same short exclusive `flock` as a commit, only while it publishes that first superblock.

### Superblock

Each slot is 4096 bytes.

| Offset | Type | Contents |
| --- | --- | --- |
| `0..8` | bytes | Magic `FERRSTR1` |
| `8..12` | `u32` | Format version `1` |
| `12..20` | `u64` | `epoch`, monotonic, starting at `1` |
| `20..24` | `u32` | CRC-32 of this 4096-byte block with bytes `20..24` set to zero |
| `24..32` | `u64` | `snapshot_offset` (absolute) |
| `32..40` | `u64` | `snapshot_length` |
| `40..48` | `u64` | `wal_offset` (absolute) |
| `48..56` | `u64` | `wal_length` |
| `56..64` | `u64` | `snapshot_seq` |
| `64..4096` | bytes | Zeros |

`snapshot_seq` is the highest committed transaction sequence included in the snapshot, or `0` when no snapshot holds a commit.

A slot is valid only when magic, version, padding, and CRC all match. Any other bytes are ignored. The current superblock is the valid slot with the highest `epoch`. A torn write fails the CRC, so the other slot stays current. Equal epochs on two valid slots are corruption.

Every committed WAL append and every `checkpoint` publishes a new generation: write the inactive slot (the slot that does not hold the highest valid epoch) with `epoch + 1`, then `fsync`. The live slot is never rewritten in place.

### Snapshot

A snapshot is a copy-on-write blob. A live snapshot's bytes are never overwritten.

| Offset | Type | Contents |
| --- | --- | --- |
| `0..8` | bytes | Magic `FERRSNAP` |
| `8..12` | `u32` | Version `1` |
| `12..20` | `u64` | `seq`, the last committed sequence in the blob |
| `20..24` | `u32` | `count` |
| `24..` | records | `count` documents |
| tail | `u32` | CRC-32 of every preceding snapshot byte |

Each record is `u32` length + bytes for the collection, then the same for the id, then the same for the raw JSON document.

`checkpoint` writes a new blob at an offset at or past the end of the file, and past both the live snapshot and the live WAL, so it does not overlap either. It `fsync`s the blob, then publishes a superblock that points `snapshot_offset` / `snapshot_length` at the new blob, sets `snapshot_seq` to the last committed sequence, sets `wal_offset` to the end of the new blob, and sets `wal_length` to `0`. A crash after the blob is written and before that publish leaves readers on the previous superblock.

An empty snapshot (`snapshot_length == 0`) contributes no documents. Its sequence is the superblock's `snapshot_seq`. On `mem://`, `checkpoint` is a successful no-op.

### WAL frame

The committed WAL is the byte range `wal_offset .. wal_offset+wal_length`. Frames are packed with no padding.

| Offset | Type | Contents |
| --- | --- | --- |
| `0..4` | `u32` | Payload length |
| `4..8` | `u32` | CRC-32 of the payload bytes only |
| `8..` | bytes | Payload |

One payload is one transaction:

| Field | Type | Contents |
| --- | --- | --- |
| `seq` | `u64` | File sequence, `1`-based |
| `op_count` | `u32` | Number of operations |
| ops | | Repeated operations |

Each operation starts with a `u8` tag: `1` put, `2` delete. Then `u32` collection length, collection UTF-8, `u32` id length, id UTF-8. A put also stores `u32` document length and the raw JSON bytes. A delete stops after the id.

`seq` increases by exactly 1 for each committed transaction in the file. Puts, deletes, FerroMQ publishes, and FerroMQ acks are each one transaction. The sequence is a property of the file, not of a topic.

Replay walks the committed range:

- Fewer than 8 header bytes at the tail, or a declared payload that is not fully inside the range, is a torn tail. Stop. Ignore that partial frame. Earlier frames stay committed.
- A CRC mismatch on the last frame (its declared end is the end of the WAL range, or the next byte would fall past the range) is a torn tail. Ignore it.
- A CRC mismatch with another complete frame after it — a following header and a payload that fits in the range — is `StoreError::Backend` and the message contains `corrupt`.
- Apply frames with `seq > snapshot_seq` in order onto the snapshot. The first applied sequence must be `snapshot_seq + 1`, and each next frame must be the next integer. A gap or a duplicate is corruption (`StoreError::Backend`, message contains `corrupt`).
- A CRC-valid frame whose payload is not a well-formed transaction is the same corruption error.

Secondary indexes and BM25 statistics are rebuilt from the documents after replay. They are not written to the WAL. The in-process subscriber list is not written either.

## Concurrency

The write descriptor is opened read/write. `flock` is the libc call (`LOCK_EX = 2`, `LOCK_UN = 8`) on that descriptor. It is held only across the critical section, not for the life of the handle.

Commit:

1. Build the operation in memory.
2. Take `LOCK_EX`.
3. Re-read the latest superblock, snapshot, and WAL. Another process may have committed.
4. Apply the operation to that state. Assign `seq = last_seq + 1`.
5. Append one CRC32-framed transaction at `wal_offset + wal_length`.
6. `fsync` the data.
7. Publish the inactive superblock with the new `wal_length` and `epoch + 1`. Snapshot fields stay as they were. `fsync`.
8. Unlock.
9. Then notify in-process FerroMQ subscribers. Notification is outside the lock and is not durable.

`checkpoint` holds the same exclusive lock while it reads, writes the new snapshot, and publishes.

Readers open a separate `O_RDONLY` description and do not `flock`. A read loads both superblocks, selects the highest valid epoch, loads that snapshot, and applies WAL frames with `seq > snapshot_seq`. A torn superblock fails its CRC and is ignored. Two `FerroStore::open` handles on one path both succeed; the second open is not `StoreError::Unreachable`. A reader observes a writer's committed document without taking a file lock.

`mem://` uses the same document map and the same APIs, with no file and no `flock`.

## Storage models

Phase 1 implements two of the four native models.

| Model | Phase | Behavior in this version |
| --- | --- | --- |
| Object / document | 1 | JSON documents addressed by collection and id. Top-level field index and Okapi BM25. |
| FerroMQ | 1 | Durable per-topic log, consumer-group acks, in-process subscribers. |
| Columnar | 3 | Not written. Row groups, RLE/delta encoding, and zone maps are a later phase. |
| Vector | 4 | Not written. `f32` / `f16` / `i8` embeddings, HNSW, and hybrid BM25+vector search are a later phase. |

## Connection URI

`FerroStore::connect_uri` accepts:

| URI | Result |
| --- | --- |
| `mem://`, `mem:///` | `in_memory()` |
| `file:///tmp/x.fls` | `open` of absolute `/tmp/x.fls` |
| `file://localhost/tmp/x.fls` | `open` of absolute `/tmp/x.fls` |
| `file://rel/x.fls` | `open` of relative `rel/x.fls` |
| `unix://...`, `ws://...`, `wss://...` | `StoreError::Backend` whose message contains `Phase 2` and does not say the scheme is unknown |
| Anything else, including `""` and `http://` | `StoreError::Backend` whose message contains `unknown` and does not describe the scheme as deferred |

`file://` paths are not percent-decoded. Only the host `localhost` is stripped; any other non-empty host is part of a relative path.

## Documents

```text
put_doc(collection, id, doc) -> Result<(), StoreError>
get_doc(collection, id) -> Result<Option<Value>, StoreError>
delete_doc(collection, id) -> Result<(), StoreError>
query_by_field(collection, field, value) -> Result<Vec<String>, StoreError>
search_bm25(collection, query, limit) -> Result<Vec<(String, f64)>, StoreError>
```

`put_doc` replaces the document at `(collection, id)`. `delete_doc` of a missing id is success and still commits a delete. `get_doc` of a missing id is `Ok(None)`.

The field index covers top-level object fields only. Strings use their value. Numbers use their JSON text (`1`, not `1.0`, for integers; other numbers use `serde_json::Number`'s JSON rendering). Bools use `true` and `false`. Null, arrays, and objects are not indexed. `query_by_field` returns matching ids in ascending order. A delete removes that document's index hits. The index is rebuilt from documents after replay.

BM25 is Okapi with `k1 = 1.2` and `b = 0.75`. Tokens are Unicode alphanumeric runs, lowercased. Every string in the document is indexed, recursively through object values and array elements. Object keys are not indexed. For `N` documents in the collection, `df` documents containing the term, term frequency `tf`, document length `dl`, and average document length `avgdl`:

```text
IDF = ln(1 + (N - df + 0.5) / (df + 0.5))
score = sum over query tokens of
    IDF * (tf * (k1 + 1)) / (tf + k1 * (1 - b + b * dl / avgdl))
```

`ln` is the natural logarithm. A document scores only when at least one query token occurs in it. Results are ordered by score descending, then id ascending, and truncated to `limit`. An empty query, a query with no tokens, or `limit == 0` returns an empty vector. Statistics are rebuilt from documents after replay.

## FerroMQ

```text
publish_mq(topic, key, payload) -> Result<u64, StoreError>
poll_mq_group(topic, group, limit) -> Result<Vec<MqRecord>, StoreError>
ack_mq(topic, group, seq) -> Result<(), StoreError>
subscribe_mq(topic) -> Receiver<MqRecord>
```

`MqRecord` is `{ seq, key, payload }`.

Durable messages are documents in collection `mq:{topic}`. The id is the decimal sequence with no padding. The JSON object is `{"seq": N, "key": "...", "payload": ...}`. The sequence starts at 1 for each topic and increases by 1 for each publish on that topic. A publish is one put transaction. Acks do not advance the topic sequence.

Acks are documents in collection `mq_ack:{topic}:{group}`, id the decimal sequence, JSON `{"seq": N}`. `ack_mq` is durable. Acking an unknown sequence still records the ack and is idempotent.

`poll_mq_group` returns the oldest unacked messages for that topic and group, up to `limit`, and does not ack them. A second poll without an ack returns the same messages. `limit == 0` returns an empty vector. Another group has its own ack set.

Subscribers are in-process only: a `Mutex<Vec<Sender<MqRecord>>>` map. `publish_mq` sends a clone after the durable commit and after the file lock is released. A disconnected subscriber is dropped. `subscribe_mq` does not replay history; it receives only later publishes on this handle.

## Phases

Phase 1 (Record 14) is this engine: the `.fls` container, `mem://`, `file://`, the document store, and FerroMQ.

Phase 2 (Record 13) is served mode: Unix-socket and WebSocket RPC, and the auto-broker. `unix://`, `ws://`, and `wss://` are recognized and refused until then.

Phase 3 is the columnar model (row groups, RLE/delta, zone maps).

Phase 4 is the vector model (`f32` / `f16` / `i8` embeddings, HNSW, hybrid BM25+vector).

This crate does not implement phases 2–4.
