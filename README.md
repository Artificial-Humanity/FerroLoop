# FerroLoop

**FerroLoop** is an open-source, self-hosted agent orchestration and coordination platform built for small teams and solo developers. It enables heterogeneous coding agents across different vendors (Claude Code, Antigravity CLI, Codex, Gemini, local models) to communicate, hand off tasks, enforce role-gated review loops, and track telemetry safely without vendor lock-in.

---

## Overview

Modern software development with agentic AI frequently requires coordinating multiple specialized tools and models. A typical workflow might involve an architecture pass in Claude, an implementation sprint run via Antigravity CLI using Gemini, and automated review or testing driven by another model family.

FerroLoop acts as the coordination fabric:

* **Cross-Vendor Interoperability:** Connects distinct agent sessions into cohesive, auditable workflows.
* **Agent-to-Agent Communication:** Implements standard protocols (ACP, MCP) for structured message passing, task delegation, and status updates.
* **Role-Gated Reviews & Verification:** Enforces development discipline (plan → build → review → verify → land) with automated gates and human escalation points.
* **Telemetry & Budget Enforcement:** Tracks token consumption, session duration, and API expenditure across providers with strict retry and spending ceilings.
* **Single-Binary Rust Architecture:** Self-hosted daemon with an embedded Rust database engine, requiring zero cloud dependencies or complex deployment infrastructure.

---

## Core Architecture

FerroLoop is designed as a standalone Rust daemon with an accompanying CLI interface:

* **Coordinator Daemon:** Manages session state, routes messages between agent sessions, referees role gates, and persists execution history.
* **Agent Adapters:** Bridges into native coding-agent harnesses (Claude Code, Antigravity CLI, OpenAI/Codex, Ollama) via standard stdio, subprocess, or network protocols.
* **Protocol Engines:** Speaks Model Context Protocol (MCP) and Agent Communication Protocol (ACP) for capabilities exchange and inter-agent coordination.
* **Storage & Telemetry:** Embedded Rust database engine maintaining an append-only audit trail of actions, state transitions, and token/cost metrics.

---

## Getting Started

### Prerequisites

* Rust 1.80+ and Cargo

### Building from Source

```console
# Clone the repository
git clone https://github.com/Artificial-Humanity/FerroLoop.git
cd FerroLoop

# Build release binary
cargo build --release
```

The compiled binary will be located at `target/release/ferroloop`.

---

## Documentation

* [AGENTS.md](AGENTS.md) — Working rules, core stack matrix, and environment conventions for agents and contributors.
* [WORKFLOW.md](WORKFLOW.md) — Development workflow, review requirements, and commit conventions.
* [PERSONA.md](PERSONA.md) — Developer persona and domain expertise (Ferris).

---

## License

Licensed under [Apache-2.0](LICENSE).
