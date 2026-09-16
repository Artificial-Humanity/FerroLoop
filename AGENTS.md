# AGENTS — FerroLoop

This is the entry point for any agent or developer working on FerroLoop (agentic orchestration
and cross-vendor coordination platform). This is an independent GitHub repository.
Public documentation lives in [docs/](docs/). The current-state snapshot and working notes live
in this project's **private** working notes, reachable in a checkout of the umbrella workspace
at `notes/` (a gitignored symlink to `Notes/FerroLoop`) and deliberately not published. Read
`notes/` before starting major architecture work.

---

## Core Stack Matrix

* **Language Ecosystem:** Rust, one static binary, zero runtime dependencies. The single-binary,
  self-hosted posture is deliberate: no cloud accounts, no external runtime services, and no
  mandatory external databases.
* **Orchestration & Protocols:** Supports standard agent communication protocols including
  Model Context Protocol (MCP) and Agent Communication Protocol (ACP), alongside native adapters
  for coding agent CLIs (Claude Code, Antigravity CLI, OpenAI/Codex, Ollama).
* **Storage & Telemetry:** Embedded Rust-native database systems for persistent session state,
  task ledgers, role transition histories, and token/cost telemetry.
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
