# FerroStore architecture model

A [LikeC4](https://likec4.dev) model of how FerroStore runs on one host: which processes
open which `.fls` files, embedded or through a `ferrostore` socket, and the modules of the
`fl-store` library behind each handle. It records the code as of `main@386cb6a7`
(2026-11-09) and a proposed single-owner end state for Milestone 6 (Record 188).

The repository-wide crate model is generated separately by `fl arch likec4` from
`.fl/architecture.toml`. This model goes one level lower, to runtime topology, which the
crate model cannot express.

## Files

| File | Content |
| --- | --- |
| `specification.c4` | Element kinds (`service`, `caller`, `storefile`, `module`), relationship kinds (`embedded`, `socket`, `uses`) and tags |
| `model.c4` | Elements and relationships. `#today` relationships describe the code now; `#target` relationships are proposed and not built |
| `views.c4` | `index` (who opens which file today), `gaps` (opens that break the single-owner rule), `library` (inside `fl-store`), `target` (proposed end state) |

Validate and render with the LikeC4 CLI (1.60.0 used):

```sh
likec4 validate docs/architecture/ferrostore
likec4 export png -o <out-dir> docs/architecture/ferrostore
```

## Evidence

Paths are relative to `crates/`.

| Claim | Evidence |
| --- | --- |
| A `.fls` path always selects the ferro engine; other paths use `FL_STORE_BACKEND`, default ferro; `x.redb` maps to `x.fls` | `store/src/any.rs` `choose_backend`, `physical_path` |
| An open connects to `<file>.fls.sock` when it accepts, otherwise opens the file embedded; a failed handshake is an error | `store/src/ferrostore/mod.rs` `FerroStore::auto` |
| A second embedded open of the same file succeeds; `flock` is held only around commit, checkpoint and compaction | `store/src/ferrostore/mod.rs` (`open` doc comment), `docs/ferrostore-spec.md` |
| The server is documented as "the only opener" but takes no exclusive lock | `store/src/ferrostore/server.rs:3`; no lock in `server.rs` or `cli/serve.rs` |
| `ferrostore get/put/query` open the file embedded even while it is served | `store/src/cli/data.rs` |
| The wire daemon keeps one long-lived workspace handle | `cli/src/cmd/wire.rs` (daemon start), `wire/src/daemon.rs` |
| The hub opens the workspace store per request under a hub-wide mutex, and project stores under a per-path lock | `hub/src/lib.rs` `open_store_at`, `open_project_store_at` |
| `fl-host` has no `fl-store` dependency | `host/Cargo.toml` |
| The ferrostate MCP opens the store on every tool call and the code index per search batch | `cli/src/cmd/state_mcp.rs` `open_for_scope` |
| The code index is always embedded, in a `<primary>.code.fls` sidecar | `store/src/code_knowledge.rs` |
| 47 `Inner::Remote` branches; remote refuses `stats`, `configure_mq`, single publish, if-absent put, local sync publish | `store/src/ferrostore/mod.rs` |
| `WireDeliveryStore` and F1 records are disabled on every backend and have no production caller | `store/src/ferrostore/traits_wire_delivery.rs`, `store/src/f1_preparation.rs`; only `core/src/wire_delivery/conformance.rs` calls them |
| FerroMQ has no production publisher; one consumer reads `mq_telemetry` | `watch/src/lib.rs` |
| Served stores: `ferrostore.service` (fl.fls) and `ferrostore@{ferroloop,kaggle-gemma4,lucida,prosodia,sonora,website}`; `the-council.fls`, `ferroloop-website.fls` and `fl.code.fls` have no unit | `systemctl --user` on ai-lab-0, 2026-11-09 |
| No backup subsystem; the read-only legacy `.redb` is the only copy | `store/src/health.rs` `backup_beside` |

## Differences from the 2026-11-08 analysis notes

The private notes `2026-11-08-ferris-architectural-analysis.md` and
`2026-11-08-host-and-store-instability-review.md` predate this inventory. The code differs
from them here:

- The ferro engine is a snapshot plus WAL over a JSON document map, not a B+tree.
- There is no `Inner::Local`; the variants are `Memory`, `Disk` and `Remote`.
- `fl-wire` and `fl-hub` do not reach the `unsupported()` wire-delivery stubs; those
  traits have no production caller.
- `fl-host` does not open any store.

## Open questions for the target

1. Does every `.fls` get a daemon, or do small or rarely used stores (`ferroloop-website.fls`)
   get retired or merged instead?
2. Who owns `fl.code.fls`: the workspace daemon, or a single indexer process, given it is
   derived data that is rebuilt when deleted?
3. With the daemon down, should callers fail with a clear error, or keep today's embedded
   fallback for read-only use only?
4. How does the hub reuse connections without the hub-wide mutex, given remote commits
   are validated by read-set hash and retried?
5. What backup and restore guarantee replaces "the read-only legacy `.redb` beside the file"?
