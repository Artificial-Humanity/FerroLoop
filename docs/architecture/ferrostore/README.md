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
| `likec4.config.json` | Marks this folder as the LikeC4 project `ferroloop-ferrostore`, so a LikeC4 server that loads `docs/architecture/` shows it as its own project |

Validate and render with the LikeC4 CLI (1.60.0 used):

```sh
likec4 validate docs/architecture/ferrostore
likec4 export png -o <out-dir> docs/architecture/ferrostore
```

## Evidence

Paths are relative to `crates/`, except `docs/ferrostore-spec.md`, which is relative to the repository root.

| Claim | Evidence |
| --- | --- |
| A `.fls` path always selects the ferro engine; other paths use `FL_STORE_BACKEND`, default ferro; `x.redb` maps to `x.fls` | `store/src/any.rs` `choose_backend`, `physical_path` |
| An open connects to `<file>.fls.sock` when it accepts, otherwise opens the file embedded; a failed handshake is an error | `store/src/ferrostore/mod.rs` `FerroStore::auto` |
| A second embedded open of the same file succeeds; a short exclusive `flock` is held only around commit, checkpoint, compaction, file create and publishing a sync batch | `store/src/ferrostore/mod.rs` (`open` doc comment; sync-batch publish at `:4626`), `docs/ferrostore-spec.md:28` (create) |
| The RPC server exposes 34 generic primitives | `store/src/ferrostore/auth.rs:159-194` `METHODS` |
| The server is documented as "the only opener" but takes no exclusive lock | `store/src/ferrostore/server.rs:3`; no lock in `server.rs` or `cli/serve.rs` |
| `ferrostore get/put/query` open the file embedded even while it is served | `store/src/cli/data.rs` |
| The wire daemon opens one workspace handle at startup, remote or embedded, and keeps it for the process lifetime; there is no runtime fallback or reconnect. `ferrowire.service` has `Wants=` and `PartOf=ferrostore.service`, so it restarts with the store daemon | `cli/src/cmd/wire.rs:1287-1295`, `wire/src/daemon.rs`; `systemctl --user cat ferrowire.service` |
| The hub opens the workspace store per request under a hub-wide mutex, and project stores under a per-path lock | `hub/src/lib.rs` `open_store_at`, `open_project_store_at` |
| The hub finds a project store by slug as `~/.local/share/fl/{slug}.redb` (mapped to `.fls`), not through the `config.toml` store binding | `hub/src/records.rs:117-154` `project_redb_with`, `:202` |
| The hub gate governor opens `config.store_path` once on first use and keeps that handle for the hub lifetime, outside the hub-wide mutex; remote or embedded is fixed at that first open | `hub/src/api.rs:8489-8491` `hub_gate_governor`, `hub/src/lib.rs` `GateGovernorStoreCache` (~359-391) |
| `ferroloop-host.service` runs `fl host serve`, which opens no store; the `fl-host` crate has no `fl-store` dependency; the `fl host` client subcommands open the workspace store like any `fl` invocation | `host/Cargo.toml`; `cli/src/cmd/host.rs:244-269` `load_registry_agents`, `execute_with_db` (`Serve`) |
| The ferrostate MCP opens the store on every tool call and the code index per search batch | `cli/src/cmd/state_mcp.rs` `open_for_scope` |
| The ferrostate MCP IRI lookup scans every configured project store, including the unserved ones, embedded | `cli/src/cmd/state_mcp.rs:484-507` `open_for_id_or_key` |
| The code index is always embedded, in a `<primary>.code.fls` sidecar | `store/src/code_knowledge.rs` |
| 47 `Inner::Remote` sites in `ferrostore/mod.rs` (44 branches; 3 are constructors), plus 6 in `traits_wire_delivery.rs`; remote refuses `stats`, `configure_mq`, single publish, if-absent put, local sync publish | `store/src/ferrostore/mod.rs`, `store/src/ferrostore/traits_wire_delivery.rs` |
| `WireDeliveryStore` and F1 records are disabled on every backend and have no production caller | `store/src/ferrostore/traits_wire_delivery.rs`, `store/src/f1_preparation.rs`; only `core/src/wire_delivery/conformance.rs` calls them |
| FerroMQ has no production publisher and no topic consumer; one caller reads the `mq_telemetry()` API | `watch/src/lib.rs:120` |
| Served stores: `ferrostore.service` (fl.fls) and `ferrostore@{ferroloop,kaggle-gemma4,lucida,prosodia,sonora,website}`; `the-council.fls`, `ferroloop-website.fls` and `fl.code.fls` have no unit | `systemctl --user` on ai-lab-0, 2026-11-09 |
| No backup system. The read-only legacy `*.redb` files are stale pre-migration snapshots from 2026-11-01, not current data; `the-council` and `ferroloop-website` have no `.redb` at all. A manifest export exists but is not a backup system | `store/src/health.rs` `backup_beside`; `core/src/doctor/store.rs:31` ("not current data"); `store/src/any.rs` `export_manifest` (~1106); `ls ~/.local/share/fl/*.redb` on ai-lab-0, 2026-11-09 |

## Differences from the 2026-11-08 analysis notes

The private notes `2026-11-08-ferris-architectural-analysis.md` and
`2026-11-08-host-and-store-instability-review.md` predate this inventory. The code differs
from them here:

- The ferro engine is a snapshot plus WAL over a JSON document map, not a B+tree.
- There is no `Inner::Local`; the variants are `Memory`, `Disk` and `Remote`.
- `fl-wire` and `fl-hub` do not reach the `unsupported()` wire-delivery stubs; those
  traits have no production caller.
- `ferroloop-host.service` (`fl host serve`) does not open any store, and the `fl-host` crate has
  no `fl-store` dependency. The `fl host` client subcommands do open the workspace store, as
  any `fl` invocation does.

## Open questions for the target

1. Does every `.fls` get a daemon, or do small or rarely used stores (`ferroloop-website.fls`)
   get retired or merged instead?
2. Who owns `fl.code.fls`: the workspace daemon, or a single indexer process, given it is
   derived data that is rebuilt when deleted?
3. With the daemon down, should callers fail with a clear error, or keep today's embedded
   fallback for read-only use only?
4. How does the hub reuse connections without the hub-wide mutex, given remote commits
   are validated by read-set hash and retried?
5. What backup and restore guarantee should exist? Today there is none: the read-only legacy
   `.redb` files are stale 2026-11-01 snapshots, not current data, and two stores have none.
