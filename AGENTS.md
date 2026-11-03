# AGENTS — FerroLoop

This is the entry point for any agent or developer working on FerroLoop (agentic orchestration
and cross-vendor coordination platform). This is an independent GitHub repository.
Public documentation lives in [docs/](docs/). The current-state snapshot and working notes live
in this project's **private** working notes, reachable in a checkout of the umbrella workspace
at `notes/` (a gitignored symlink to `Notes/FerroLoop`) and deliberately not published. Read
`notes/` before starting major architecture work.

---

## Core Stack Matrix

* **Language Ecosystem:** Rust. Installation is simple: install, uninstall, or install component
  by component (cherry-pick). That goal does not imply how many binaries or other parts FerroLoop
  has. FerroLoop is self-hosted: no cloud accounts, no mandatory external databases, and nothing
  depends on a vendor-hosted service.
* **Orchestration & Protocols:** Five execution adapters run attempts (`fl attempt`): `claude`,
  `codex`, `antigravity`, `muse` and `grok`. `fl mcp` keeps a project's catalog of MCP servers and
  writes it into the MCP config files of those same five CLIs; it only writes their configuration
  and starts neither the CLIs nor the MCP servers. FerroWire carries agent-to-agent messaging,
  receipts, vendor push and peer relay (`fl wire`). FerroGate holds the zero-knowledge secrets
  vault, the credential proxy and the declarative MCP gateway (`fl vault`). FerroDeck is the
  command-center web app that the hub daemon serves (`fl hub`).
* **Storage & Telemetry:** FerroStore (`fl-store`) is the embedded store. Its default engine is
  the `.fls` file (the `ferro` backend); a lone `.redb` is migrated to it on first open, verified
  table by table and view by view, and kept read-only. `FL_STORE_BACKEND=redb` selects redb.
  The standalone `ferrostore` binary and `fl store` share one command module (`fl_store::cli`).
  The store holds a project's gates, transitions, gate runs and attempts (each with its outcome,
  duration, tokens and cost), and its records and findings unless the project keeps those in
  GitHub Issues. FerroState searches state entries (memories, handoffs and runtime state) with
  exact BM25, and the code index by name tiers and line matches; on the ferro engine both fuse in
  an HNSW vector leg.
* **Capability truth lives in code, not prose:** Enums, adapter registries, and protocol
  schemas generate capability lists wherever possible. Avoid duplicating hard-coded provider or
  feature lists in documentation that easily rots.
* **Verification trio:** `cargo test`, `cargo clippy --all-targets` (kept warning-free so new
  warnings remain visible), and integration smoke checks. All must be green before landing work.
* **Two test layers, one place each:** Unit tests live in `#[cfg(test)] mod tests` inside the
  module they test. Subprocess, CLI framing, and multi-agent coordination scenarios belong in
  `tests/` driven as black-box tests with isolated environments.

---

## Integration Dependencies

* FerroLoop coordinates external coding-agent CLIs and MCP servers. Changes to protocol schemas,
  CLI invocation flags, or message-passing boundaries affect agent workflows across the workspace.
* Inter-agent messaging and task handoffs must maintain strict boundary isolation, timeout
  controls, and token-spend ceilings.

---

## File Naming Conventions

Names must be predictable so links resolve on case-sensitive systems (Linux/CI) as well as
case-insensitive macOS/Windows.

* **Canonical root marker files → `UPPERCASE`** (`SCREAMING_SNAKE_CASE` if multi-word):
  `README.md`, `LICENSE`, `CONTRIBUTING.md`, `AGENTS.md`, `WORKFLOW.md`, `PERSONA.md`.
* **Anchor docs in `docs/` → `UPPERCASE`, single word preferred:** `ROADMAP.md`, `ARCHITECTURE.md`.
* **All other documents → `lowercase-kebab-case.md`:** e.g. `open-decisions.md`.
* **Source code → the language's own convention:** Rust `snake_case.rs`.
* **Never** let case be the only difference between two paths, and always reference files with their exact case.

---

## System Operational Mandates

⚠ **How work gets done — [WORKFLOW.md](WORKFLOW.md).** Branching, review, and landing on `main`
are defined there; read it before your first commit.

### Token & Spend Discipline

* Agent execution loops and automated multi-turn handoffs must enforce hard token ceilings and
  cost caps.
* Pre-flight checks should fail fast on budget exhaustion rather than allowing runaway agent loops.
