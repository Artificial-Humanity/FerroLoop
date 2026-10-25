# Sub-Project 3: Adapters and Telemetry — Design Specification

**Date:** 2026-10-25  
**Status:** Draft (reviewed, awaiting owner approval)  
**Author:** Ferris (`ferris@artificialhumanity.io`)  
**Scope:** Five-vendor execution adapters and MCP catalog writers (`claude`, `codex`, `antigravity`, `muse`, `grok`), shared `ProcessHarness` with streaming telemetry and rate-card cost fallback, vendor-agnostic `paths_touched` git snapshot delta, wrapper-driven `RuntimeEntry` liveness heartbeats (`live` / `unverifiable` / `exited`), and `fl stats` telemetry breakdown with Decision 21 regret declaration auditing.

---

## 1. Context, Goals, and Non-Goals

### 1.1 Context

In the ratified FerroLoop build order (`Notes/FerroLoop/north-star.md` §3 and §6):

1. **Sub-project 1 — State Database and Agent Registry (`FORMAT_WITH_STATE = 6`):** shipped on `main` (`6ea814e`, PR #43).
2. **Sub-project 2 — FerroWire (`FORMAT_WITH_WIRE = 7`):** shipped on `main` (`5551761`, PR #44), including session discovery, hooks, and push delivery for **Claude Code (`claude`)**, **OpenAI Codex (`codex`)**, **Google Antigravity (`antigravity` / `agy`)**, **Meta Muse (`muse`)**, and **xAI Grok (`grok`)**.
3. **Sub-project 3 — Adapters and Telemetry (this specification):** brings `crates/mcp`, `crates/exec`, `crates/core`, and `crates/cli` to full five-vendor parity and real usage/liveness telemetry.

Today on `main`:
* `crates/mcp` (`fl mcp`) writes MCP server definitions only for three vendors (`Claude`, `Codex`, `Antigravity`).
* `crates/exec/src/adapters/` ships only `ClaudeAdapter`, which runs `claude -p <instruction> --max-budget-usd <usd>` in plain-text mode and hardcodes `tokens_in = 0`, `tokens_out = 0`, `cost_usd_micros = 0`, and `paths_touched = vec![]`.
* `crates/cli/src/cmd/attempt.rs` rejects any `--adapter` other than `"claude"` and defaults `--binary` to `"claude"`.
* No wrapper liveness heartbeats are written to `StateStore` (`StatePayload::Runtime`) during an attempt.
* `crates/cli/src/cmd/stats.rs` (`fl stats`) prints only project-wide attempt counts, total wall-clock milliseconds, and total micro-USD, without token totals, per-adapter breakdowns, partial-cost floor notes, `--json` output, or Decision 21's transition regret audit.

### 1.2 Goals

1. **Five-vendor parity across `fl-mcp` and `fl-exec`:**
   * Extend `fl-mcp` (`VendorName`) with `Muse` (`.muse/settings.json`) and `Grok` (`.grok/config.toml`).
   * Ship five execution adapters in `fl-exec` (`ClaudeAdapter`, `CodexAdapter`, `AntigravityAdapter`, `MuseAdapter`, `GrokAdapter`) plus a unified `AnyAdapter` enum implementing `Runner`.
2. **Real token and cost telemetry (`tokens_in`, `tokens_out`, `cost_usd_micros`):**
   * Run each vendor CLI in its structured JSON or JSONL non-interactive mode (with graceful fallback to plain text when output is non-JSON, so existing tests and custom wrapper scripts continue to work).
   * Drain `stdout` and `stderr` incrementally via async tasks so that even when an attempt ends in `Timeout` or `Crashed`, any JSONL token or cost usage emitted before termination is preserved on `AttemptOutcome`.
   * Support optional per-adapter rate cards (`input_usd_micros_per_mtok`, `output_usd_micros_per_mtok`) in `config.toml` (`[adapters.<name>]`) to compute `cost_usd_micros` when a subscription CLI reports token counts without dollar cost, while keeping `cost_usd_micros = 0` when neither vendor cost nor rate card is present.
3. **Vendor-agnostic `paths_touched` capture:**
   * Capture a pre-attempt git snapshot (`HEAD` + `git status --porcelain=v1 -z -uall` with in-memory SHA-256 content hashes for dirty/untracked non-ignored files) and compute the post-attempt delta across both new commits (`git diff --name-only <head_before> <head_after>`) and working-tree/index changes, returning sorted project-relative paths on `AttemptOutcome::paths_touched`.
4. **Wrapper-driven liveness reporting (`live` / `unverifiable` / `exited`):**
   * Have `ProcessHarness` emit `AttemptHeartbeat` records into `StateStore` (`StatePayload::Runtime` with `MergeMode::LastWriter`) on spawn, periodic tick / stream progress, and exit — without asking the LLM to send heartbeats.
   * Provide deterministic liveness evaluation (`evaluate_liveness`) using the three-state vocabulary `live`, `unverifiable` (`missing_status`, `stale_status`, `host_unavailable`), and `exited`, combining writer-clock timestamp freshness (`At::unix_millis()`) with local OS PID probing (`kill(pid, 0)` on Unix).
   * Expose `fl agent liveness [<persona_or_record>] [--json]` in `fl-cli`.
5. **Rich `fl stats` reporting and Decision 21 regret audit:**
   * Extend `fl stats --project <project> [--json]` with `tokens in`, `tokens out`, a partial-cost floor note when some non-refused attempts report zero cost, a deterministic `by adapter:` breakdown, and Decision 21's advisory transition regret audit (`audit_transition_regret`).
6. **Zero store format bump (`FORMAT_WITH_WIRE = 7` unchanged):**
   * Reuse the existing `Attempt`, `GateRun`, and `RuntimeEntry` schemas in `fl-core` and `fl-store`.

### 1.3 Non-Goals

* Adding `--dangerously-skip-permissions` or `--dangerously-bypass-approvals-and-sandbox` to any adapter (`north-star.md` §4: permission prompts stay on by default).
* Sending any telemetry outside the owner's machine (`north-star.md` §4: nothing is sent home).
* Multi-step workflow graph scheduling and automatic escalation ladder routing (Sub-project 4: Workflow Graph).
* Web UI / PWA dashboard rendering of telemetry and liveness (Sub-project 5: Hub and Command-Center PWA).

---

## 2. Five-Vendor MCP Catalog (`crates/mcp`)

### 2.1 `VendorName` Expansion (`crates/mcp/src/catalog.rs`)

Expand `VendorName` from three variants to five, preserving `#[serde(rename_all = "lowercase")]`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VendorName {
    Claude,
    Codex,
    Antigravity,
    Muse,
    Grok,
}

impl VendorName {
    pub const ALL: [VendorName; 5] = [
        VendorName::Claude,
        VendorName::Codex,
        VendorName::Antigravity,
        VendorName::Muse,
        VendorName::Grok,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            VendorName::Claude => "claude",
            VendorName::Codex => "codex",
            VendorName::Antigravity => "antigravity",
            VendorName::Muse => "muse",
            VendorName::Grok => "grok",
        }
    }
}
```

### 2.2 Parameterized `JsonFile` and `TomlFile` (`crates/mcp/src/vendor/`)

1. **`crates/mcp/src/vendor/json.rs`:**
   * Store `servers_key: &'static str` on `JsonFile` (`"mcpServers"` for `Claude` and `Antigravity`; `"mcp_servers"` for `Muse`) and replace all uses of the hardcoded `SERVERS` constant in `open_with`, `servers_mut`, and `to_bytes` with `self.servers_key`.
   * Add `pub(super) fn open_with(path: &Path, bytes: Option<&[u8]>, servers_key: &'static str, default_top: &[(&'static str, &'static str)]) -> Result<JsonFile, FileRefusal>` (while keeping `open(path, bytes)` as `open_with(path, bytes, "mcpServers", &[])`).
   * When `bytes == None`, `open_with` inserts each `(k, raw_json)` from `default_top` into `file.top` *before* calling `file.servers_mut()`, so `"schema_version": 1` appears above `"mcp_servers"` in newly created `.muse/settings.json` files. When `file.is_empty()` is checked after removing all servers from an unbacked file, `is_empty()` continues to check `self.servers.as_ref().is_none_or(IndexMap::is_empty)` so an empty unbacked file is not left as a stub.
   * When `bytes == Some(_)`, `open_with` preserves all existing top-level keys and ordering byte-for-byte via `IndexMap<String, Box<RawValue>>`, validating that `servers_key` (when present) is a strict JSON object without duplicate keys.
2. **`crates/mcp/src/vendor/muse.rs` (`Muse`):**
   * `name() -> VendorName::Muse`, `title() -> "Muse"`, `target() -> ".muse/settings.json"`.
   * `render(&self, _name: &str, server: &Server) -> Result<Rendered, VendorRefusal>`:
     * `Transport::Stdio`: writes `"command"`, `"args"` (when non-empty), and `"env"` (when non-empty), expanding `EnvValue::Secret` to `"${VAR}"` (using `value.secret_var(key)`).
     * `Transport::Http | Transport::Sse`: writes `"type"` (`"http"` or `"sse"`), `"url"`, and `"headers"` (when non-empty), expanding `HeaderValue::Secret { env, scheme: None }` to `"${env}"` and `HeaderValue::Secret { env, scheme: Some(scheme) }` to `"{scheme} ${env}"`.
   * `open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal>`: delegates to `json::open_with(path, bytes, "mcp_servers", &[("schema_version", "1")])`.
3. **`crates/mcp/src/vendor/grok.rs` (`Grok`):**
   * `name() -> VendorName::Grok`, `title() -> "Grok"`, `target() -> ".grok/config.toml"`.
   * Reuses `TomlFile`, `open`, and `alone(name, &Item::Table(table))` from `codex.rs` (`[mcp_servers.<name>]`).
   * `render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal>`:
     * `Transport::Stdio`: writes `command`, `args`, `env` (literals), and `env_vars` (forwarded secret environment variable names). If a secret variable is renamed (`var != key`), returns `VendorRefusal` with reason: `` `env.<key>` reads the variable `<var>`, and Grok forwards a secret variable under its own name ``.
     * `Transport::Sse`: returns `VendorRefusal` with reason: `"Grok connects to streamable HTTP servers only, not SSE"`.
     * `Transport::Http`: writes `url`, `bearer_token_env_var` (for `Authorization: Bearer` secret header), `http_headers` (literal headers), and `env_http_headers` (whole-value secret headers). Refuses non-Bearer secret header schemes with: `` the secret header `<header>` has the scheme `<scheme>`, and Grok sends a secret header either whole or as `Authorization: Bearer` ``.
4. **`crates/mcp/src/vendor/mod.rs` and `crates/mcp/src/sync.rs`:**
   * Register `Muse` and `Grok` in `vendor(name: VendorName)` in `crates/mcp/src/vendor/mod.rs`.
   * Update `value(vendor, name, bytes)` in `crates/mcp/src/sync.rs` so `VendorName::Codex | VendorName::Grok` parses TOML `[mcp_servers.<name>]` and `VendorName::Claude | VendorName::Antigravity | VendorName::Muse` parses JSON `serde_json::from_slice(bytes)`.
   * Note: `codex_trust` in `crates/cli/src/cmd/mcp.rs` remains specific to `VendorName::Codex` because Grok does not gate `.grok/config.toml` on `~/.codex/config.toml` project trust.
   * Update the vendor test suite in `crates/mcp/src/vendor/mod.rs` (`each_vendor_writes_its_own_project_file`, `an_entry_fl_writes_reads_back_as_the_bytes_it_rendered`, `removing_what_was_added_leaves_the_file_as_it_was`, and `a_secret_value_in_the_environment_reaches_no_rendered_entry`) and the black-box test fixtures in `crates/cli/tests/mcp.rs` (`IGNORED`, `MUSE = ".muse/settings.json"`, `GROK = ".grok/config.toml"`, `Secrets::texts()`, and record-count assertions) for the 5-element `VendorName::ALL`.
5. **`docs/mcp.md`:**
   * Document `.muse/settings.json` and `.grok/config.toml` alongside `.mcp.json`, `.codex/config.toml`, and `.agents/mcp_config.json`, including the five `.gitignore` lines (`/.mcp.json`, `/.codex/config.toml`, `/.agents/mcp_config.json`, `/.muse/settings.json`, `/.grok/config.toml`).

---

## 3. Pure Telemetry, Rate Card, Liveness, and Regret Audit (`crates/core`)

### 3.1 `At::unix_millis()` (`crates/core/src/at.rs`)

Extract the RFC 3339 millisecond computation in `At::parse` (`crates/core/src/at.rs` lines 66–74) into a shared helper and add:

```rust
impl At {
    /// Milliseconds after the Unix epoch for this timestamp.
    pub fn unix_millis(&self) -> u64 { ... }
}
```

Include a round-trip unit test in `crates/core/src/at.rs` verifying `At::from_unix_millis(ms).unix_millis() == ms`.

### 3.2 Adapter Names and Rate Card (`crates/core/src/telemetry.rs`)

Create `crates/core/src/telemetry.rs` (re-exported from `crates/core/src/lib.rs`) and add `AttemptHeartbeat` (which holds 3 IRI fields: `attempt_id: Iri`, `project: ProjectId`, `record: RecordId`) to `every_reference_field_on_the_wire_is_a_full_iri` in `crates/core/tests/wire_refs.rs` with `floor = 3`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Claude,
    Codex,
    Antigravity,
    Muse,
    Grok,
}

crate::wire::wire_names!(AdapterKind as adapter_kind_wire {
    Claude => "claude",
    Codex => "codex",
    Antigravity => "antigravity",
    Muse => "muse",
    Grok => "grok",
});
crate::wire::wire_parse!(AdapterKind as adapter_kind_parse);

impl AdapterKind {
    pub fn default_binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "agy",
            Self::Muse => "muse",
            Self::Grok => "grok",
        }
    }
}

/// Optional per-adapter token pricing in micro-USD per 1,000,000 tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateCard {
    #[serde(default)]
    pub input_usd_micros_per_mtok: u64,
    #[serde(default)]
    pub output_usd_micros_per_mtok: u64,
}

impl RateCard {
    /// Compute cost in micro-USD when vendor-reported cost is absent/zero.
    /// Uses u128 intermediate math to prevent overflow.
    pub fn resolve_cost(
        self,
        tokens_in: u64,
        tokens_out: u64,
        vendor_cost_usd_micros: Option<u64>,
    ) -> u64 {
        if let Some(c) = vendor_cost_usd_micros
            && c > 0
        {
            return c;
        }
        if self.input_usd_micros_per_mtok == 0 && self.output_usd_micros_per_mtok == 0 {
            return 0;
        }
        let total = (tokens_in as u128)
            .saturating_mul(self.input_usd_micros_per_mtok as u128)
            .saturating_add(
                (tokens_out as u128).saturating_mul(self.output_usd_micros_per_mtok as u128),
            )
            / 1_000_000;
        u64::try_from(total).unwrap_or(u64::MAX)
    }
}
```

### 3.3 Liveness Vocabulary, State Slugs, and `AttemptHeartbeat` (`crates/core/src/telemetry.rs`)

```rust
pub const DEFAULT_STALE_AFTER_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LivenessStatus {
    Live,
    Unverifiable,
    Exited,
}

crate::wire::wire_names!(LivenessStatus as liveness_status_wire {
    Live => "live",
    Unverifiable => "unverifiable",
    Exited => "exited",
});
crate::wire::wire_parse!(LivenessStatus as liveness_status_parse);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnverifiableReason {
    MissingStatus,
    StaleStatus,
    HostUnavailable,
}

crate::wire::wire_names!(UnverifiableReason as unverifiable_reason_wire {
    MissingStatus => "missing_status",
    StaleStatus => "stale_status",
    HostUnavailable => "host_unavailable",
});
crate::wire::wire_parse!(UnverifiableReason as unverifiable_reason_parse);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptHeartbeat {
    pub attempt_id: Iri,
    pub project: ProjectId,
    pub record: RecordId,
    pub adapter: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub host: String,
    pub pid: u32,
    pub started_at: At,
    pub heartbeat_at: At,
    pub status: LivenessStatus,
    #[serde(default)]
    pub outcome: Option<AttemptStatus>,
    #[serde(default)]
    pub exit_at: Option<At>,
    pub duration_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LivenessVerdict {
    Live {
        pid: u32,
        host: String,
        heartbeat_at: At,
        duration_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd_micros: u64,
    },
    Unverifiable {
        reason: UnverifiableReason,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        heartbeat_at: Option<At>,
    },
    Exited {
        outcome: AttemptStatus,
        #[serde(default)]
        exit_at: Option<At>,
        duration_ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd_micros: u64,
    },
}
```

#### Deterministic State Slug Helpers (`liveness_record_slug`, `liveness_agent_slug`)

Because `StateStore` validates that every `StateEntry.key` matches `^[a-z][a-z0-9_-]{0,63}$` (`validate_slug` in `crates/core/src/state.rs`), while `RecordId` is an `Iri` (`urn:uuid:...` or `https://github.com/...`), `crates/core/src/telemetry.rs` provides two deterministic helpers guaranteed to pass `validate_slug`:

* `pub fn liveness_record_slug(record: &RecordId) -> String`:
  * Strips a leading `"urn:uuid:"` prefix if present, lowercases ASCII, replaces any byte outside `[a-z0-9_-]` with `'-'`, and collapses consecutive `'-'`. If the resulting suffix fits in `48` bytes, returns `format!("liveness-record-{suffix}")`; otherwise returns `format!("liveness-record-{}", &sha256_hex(record.iri().as_str())[..36])` (always `<= 52` bytes and matching `^[a-z][a-z0-9_-]{0,63}$`).
* `pub fn liveness_agent_slug(persona: &str) -> String`:
  * Normalizes `persona` (lowercase ASCII, replacing bytes outside `[a-z0-9_-]` with `'-'`, truncated to `48` bytes) and returns `format!("liveness-agent-{sanitized}")`.

#### Deterministic `evaluate_liveness` Rules

`pub fn evaluate_liveness<F>(hb: Option<&AttemptHeartbeat>, now: &At, stale_after_ms: u64, local_host: &str, pid_is_alive: F) -> LivenessVerdict where F: Fn(u32) -> bool`:

1. **`hb == None`:** returns `LivenessVerdict::Unverifiable { reason: UnverifiableReason::MissingStatus, host: None, heartbeat_at: None }`.
2. **`hb.status == LivenessStatus::Exited`:** returns `LivenessVerdict::Exited { outcome: hb.outcome.unwrap_or(AttemptStatus::Crashed), exit_at: hb.exit_at.clone(), duration_ms: hb.duration_ms, tokens_in: hb.tokens_in, tokens_out: hb.tokens_out, cost_usd_micros: hb.cost_usd_micros }`.
3. **`hb.status == LivenessStatus::Unverifiable`:** returns `LivenessVerdict::Unverifiable { reason: UnverifiableReason::StaleStatus, host: Some(hb.host.clone()), heartbeat_at: Some(hb.heartbeat_at.clone()) }`.
4. **`hb.status == LivenessStatus::Live` on `local_host` (`hb.host == local_host`):**
   * If `hb.pid == 0` or `!pid_is_alive(hb.pid)` (the OS process is gone without writing an exit record), returns `LivenessVerdict::Exited { outcome: hb.outcome.unwrap_or(AttemptStatus::Crashed), exit_at: None, duration_ms: hb.duration_ms, tokens_in: hb.tokens_in, tokens_out: hb.tokens_out, cost_usd_micros: hb.cost_usd_micros }`.
   * Otherwise, compute `age_ms = now.unix_millis().saturating_sub(hb.heartbeat_at.unix_millis())`. If `age_ms <= stale_after_ms`, returns `LivenessVerdict::Live { ... }`; else returns `LivenessVerdict::Unverifiable { reason: UnverifiableReason::StaleStatus, host: Some(hb.host.clone()), heartbeat_at: Some(hb.heartbeat_at.clone()) }`.
5. **`hb.status == LivenessStatus::Live` on a remote host (`hb.host != local_host`):**
   * Compute `age_ms = now.unix_millis().saturating_sub(hb.heartbeat_at.unix_millis())`. If `age_ms <= stale_after_ms`, returns `LivenessVerdict::Live { ... }`; else returns `LivenessVerdict::Unverifiable { reason: UnverifiableReason::HostUnavailable, host: Some(hb.host.clone()), heartbeat_at: Some(hb.heartbeat_at.clone()) }`.

### 3.4 Decision 21 Regret Declaration Audit (`audit_transition_regret`)

In `crates/core/src/telemetry.rs`, define:

```rust
pub const REGRET_AUDIT_MULTIPLIER: u64 = 10;
pub const REGRET_AUDIT_MIN_COST_MICROS: u64 = 1_000;
pub const REGRET_AUDIT_MIN_DURATION_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegretMetric {
    CostUsdMicros,
    DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegretChallenge {
    pub name: String,
    pub from: State,
    pub to: State,
    pub metric: RegretMetric,
    pub measured_mean: u64,
    pub project_median: u64,
    pub ratio: u64,
}
```

`pub fn audit_transition_regret(transitions: &[Transition], gate_runs: &[GateRun]) -> Vec<RegretChallenge>`:

1. For each `Transition` in `transitions` that declares at least one gate in `t.gates`:
   * Collect all `GateRun` entries in `gate_runs` whose `run.gate` is in `t.gates`.
   * If no runs exist for `t.gates`, skip `t`.
   * Compute `mean_cost = sum(run.cost_usd_micros) / count` and `mean_duration = sum(run.duration_ms) / count`.
2. Need at least 2 transitions with measured runs to establish a meaningful baseline.
3. Choose the audit metric:
   * If at least 2 measured transitions have `mean_cost > 0`, audit on `RegretMetric::CostUsdMicros` (with floor `REGRET_AUDIT_MIN_COST_MICROS = 1_000`);
   * Otherwise audit on `RegretMetric::DurationMs` (with floor `REGRET_AUDIT_MIN_DURATION_MS = 1_000`).
4. Compute `project_median`: sort the measured transition means ascending; for odd `N`, take `sorted[N / 2]`; for even `N`, take `(sorted[N / 2 - 1] + sorted[N / 2]) / 2`. If `project_median == 0`, return `vec![]`.
5. For each transition with `t.regret == Regret::Low`:
   * If `measured_mean >= floor` and `measured_mean >= project_median.saturating_mul(REGRET_AUDIT_MULTIPLIER)`:
     * Record a `RegretChallenge` with `name: t.name.clone()`, `from: t.from`, `to: t.to`, `ratio = measured_mean / project_median`.
6. Sort challenges deterministically by `(c.from.as_wire(), c.to.as_wire(), c.name.as_str())`.

---

## 4. Shared `ProcessHarness`, `GitSnapshot`, and Five Adapter Dialects (`crates/exec/src/adapters/`)

### 4.1 Module Layout

```text
crates/exec/src/adapters/
├── mod.rs          # CliDialect trait, ParsedOutput, AnyAdapter, re-exports
├── harness.rs      # ProcessHarness, LivenessSink, GitSnapshot (paths_touched delta)
├── claude.rs       # ClaudeAdapter + Claude JSON/JSONL/text parser
├── codex.rs        # CodexAdapter + Codex JSONL/text parser
├── antigravity.rs  # AntigravityAdapter + Antigravity JSON/JSONL/text parser
├── muse.rs         # MuseAdapter + Muse JSON/JSONL/text parser
└── grok.rs         # GrokAdapter + Grok JSON/JSONL/text parser
```

### 4.2 `CliDialect` and `ParsedOutput` (`crates/exec/src/adapters/mod.rs`)

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedOutput {
    /// Human-readable assistant response text (or raw stdout+stderr fallback).
    pub excerpt_text: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Vendor-reported cost in micro-USD when present in output.
    pub vendor_cost_usd_micros: Option<u64>,
}

pub trait CliDialect: Send + Sync {
    fn kind(&self) -> AdapterKind;
    fn id(&self) -> &'static str {
        self.kind().as_wire()
    }
    /// Build the CLI arguments for non-interactive execution of `spec`.
    fn build_args(&self, spec: &AttemptSpec) -> Vec<String>;
    /// Parse complete or partial stdout and stderr into `ParsedOutput`.
    fn parse_output(&self, stdout: &str, stderr: &str) -> ParsedOutput;
}
```

### 4.3 Per-Vendor `CliDialect` Rules

1. **`ClaudeAdapter` (`claude.rs`):**
   * `build_args`: `vec!["-p".into(), spec.instruction.clone(), "--output-format".into(), "json".into(), "--max-budget-usd".into(), format!("{budget_usd:.6}")]` where `budget_usd = spec.budget_usd_micros as f64 / 1_000_000.0`.
   * `parse_output`:
     * Inspects JSON objects in `stdout` (either a single top-level JSON object `{"type":"result","result":"...","total_cost_usd":...,"usage":{...}}` or JSONL lines).
     * Sums `usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens` into `tokens_in`, and `usage.output_tokens` into `tokens_out`.
     * Extracts `total_cost_usd` or `cost_usd` (converted via `(usd * 1_000_000.0).round() as u64`) into `vendor_cost_usd_micros`.
     * Extracts human-readable `result` (or `message.content` text blocks) into `excerpt_text` (appending `stderr` if `excerpt_text` is empty or on non-JSON fallback).
     * If `stdout` contains no valid JSON object with expected fields, falls back to raw `format!("{stdout}{stderr}")` with `0` tokens and `None` cost.
2. **`CodexAdapter` (`codex.rs`):**
   * `build_args`: `vec!["exec".into(), "--json".into(), "--skip-git-repo-check".into(), "-C".into(), spec.project_root.to_string_lossy().into_owned(), spec.instruction.clone()]`.
   * `parse_output`:
     * Parses JSONL lines in `stdout`.
     * Collects assistant message text from `item.completed` / `agent_message` events (`item.text` or `message.content` or `result`).
     * Sums `usage.input_tokens` (including `cached_input_tokens` if reported separately, or `input_tokens` directly when inclusive) and `usage.output_tokens` from `turn.completed` or `response.completed` events, plus `total_cost_usd` / `cost_usd` when present.
     * Falls back to raw `format!("{stdout}{stderr}")` when no JSONL events parse.
3. **`AntigravityAdapter` (`antigravity.rs`):**
   * `build_args`: `vec!["-p".into(), spec.instruction.clone(), "--output-format".into(), "json".into()]`.
   * `parse_output`:
     * Parses single JSON or JSONL lines in `stdout`.
     * Extracts response text (`result`, `response`, `text`, or `content`).
     * Extracts `usage.input_tokens` (or `usageMetadata.promptTokenCount` / `input_tokens`) and `usage.output_tokens` (or `usageMetadata.candidatesTokenCount` / `output_tokens`), plus `total_cost_usd` / `cost_usd` if present.
     * Falls back to raw `format!("{stdout}{stderr}")` when non-JSON.
4. **`MuseAdapter` (`muse.rs`):**
   * `build_args`: `vec!["exec".into(), "--json".into(), spec.instruction.clone()]`.
   * `parse_output`:
     * Parses single JSON or JSONL lines in `stdout`.
     * Extracts response text (`result`, `text`, `message.content`, or `output`), `usage.input_tokens` (`prompt_tokens`), `usage.output_tokens` (`completion_tokens`), and `total_cost_usd` / `cost_usd`.
     * Falls back to raw `format!("{stdout}{stderr}")` when non-JSON.
5. **`GrokAdapter` (`grok.rs`):**
   * `build_args`: `vec!["-p".into(), spec.instruction.clone(), "--output-format".into(), "json".into()]`.
   * `parse_output`:
     * Parses single JSON or JSONL lines in `stdout`.
     * Extracts response text (`result`, `text`, `message.content`, or `choices[0].message.content`), `usage.input_tokens` (`prompt_tokens`), `usage.output_tokens` (`completion_tokens`), and `total_cost_usd` / `cost_usd`.
     * Falls back to raw `format!("{stdout}{stderr}")` when non-JSON.

### 4.4 `GitSnapshot` (`paths_touched` Delta in `harness.rs`)

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathState {
    status: String,
    content_hash: Option<[u8; 32]>,
}

pub struct GitSnapshot {
    head: Option<String>,
    dirty: BTreeMap<String, PathState>,
}
```

1. **`GitSnapshot::capture(root: &Path) -> Option<Self>`:**
   * Runs `git status --porcelain=v1 -z -uall` in `root`. If `git` fails (e.g., `root` is not a git repository), returns `None`.
   * Records `head = Git::head(root).ok()`.
   * Parses NUL-delimited porcelain entries (`XY PATH`, skipping the source path of `R`/`C` rename/copy pairs). Ignores paths starting with `.fl/`.
   * For each path `rel`, inspects `fs::symlink_metadata(root.join(&rel))` in memory using `sha2::Sha256` (avoiding subprocess spawns and never writing loose objects to `.git/objects`):
     * If symlink: hashes the link target bytes from `fs::read_link`.
     * If regular file: hashes the file bytes from `fs::read`.
     * If deleted, directory, or unreadable: `content_hash = None`.
   * Inserts `PathState { status, content_hash }` into `dirty`.
2. **`GitSnapshot::diff_after(&self, root: &Path) -> Vec<String>`:**
   * Captures `after = GitSnapshot::capture(root)`. If `None`, returns `vec![]`.
   * Initializes `touched = BTreeSet::new()`.
   * If `let (Some(h1), Some(h2)) = (&self.head, &after.head)` and `h1 != h2`:
     * Runs `git diff --name-only <h1> <h2>` in `root` and inserts every non-`.fl/` relative path into `touched`.
   * For every `(path, after_state)` in `&after.dirty`:
     * If `self.dirty.get(path) != Some(after_state)`, inserts `path.clone()` into `touched`.
   * For every `(path, _before_state)` in `&self.dirty`:
     * If `!after.dirty.contains_key(path)` and `!root.join(path).exists()`, inserts `path.clone()` into `touched` (an untracked or dirty file that existed before the attempt was deleted during the attempt).
   * Returns `touched.into_iter().collect()`.

### 4.5 `ProcessHarness` Execution and Heartbeat Loop (`harness.rs`)

Each adapter holds:
* `binary: String`
* `rate_card: RateCard`
* `liveness_sink: Option<LivenessSink>`
* `heartbeat_interval: Duration` (default `Duration::from_secs(5)`)

Constructors on each adapter (`ClaudeAdapter`, `CodexAdapter`, `AntigravityAdapter`, `MuseAdapter`, `GrokAdapter`) and `AnyAdapter`:
* `pub fn new(binary: String) -> Self` (keeps full backward compatibility with existing `ClaudeAdapter::new` callers)
* `pub fn with_rate_card(mut self, rate_card: RateCard) -> Self`
* `pub fn with_liveness_sink(mut self, sink: LivenessSink) -> Self`
* `pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self`

During `attempt(spec: AttemptSpec)`:
1. If `spec.budget_usd_micros == 0`, emits a terminal `Exited` (`outcome: Some(AttemptStatus::Refused)`) heartbeat (if `liveness_sink` is attached) and returns `Ok(AttemptOutcome::refused("the attempt's spend ceiling is zero, so nothing was started"))`.
2. Captures `pre_git = GitSnapshot::capture(&spec.project_root)`.
3. Builds `tokio::process::Command::new(&self.binary)` with `dialect.build_args(&spec)`, `current_dir(&spec.project_root)`, `stdout(Stdio::piped())`, `stderr(Stdio::piped())`, and `kill_on_drop(true)`.
4. Spawns the child with `SPAWN_RETRY_LIMIT` (`20` retries × `2ms`) on `ErrorKind::ExecutableFileBusy`. On other spawn error, emits a terminal `Exited` (`outcome: Some(AttemptStatus::Refused)`) heartbeat and returns `Ok(AttemptOutcome::refused(format!("could not start `{}`: {e}", self.binary)))`.
5. Records `pid = child.id().unwrap_or(0)` and emits the initial `Live` heartbeat to `liveness_sink`.
6. Spawns two reader tasks (`tokio::spawn`) that read `child.stdout` and `child.stderr` chunk-by-chunk into `Arc<Mutex<Vec<u8>>>` buffers (`stdout_buf`, `stderr_buf`).
7. Runs a `tokio::select!` loop awaiting `child.wait()`, the timeout timer (`Duration::from_secs(spec.timeout_secs.max(1))`), and a periodic `tokio::time::interval(self.heartbeat_interval)` tick:
   * On each heartbeat tick, parses a snapshot of `stdout_buf` and `stderr_buf` via `dialect.parse_output`, resolves cost via `rate_card.resolve_cost(...)`, and invokes `liveness_sink` with `status = LivenessStatus::Live` and updated `heartbeat_at`, `duration_ms`, `tokens_in`, `tokens_out`, and `cost_usd_micros`.
8. When `child.wait()` finishes or the timeout fires:
   * If timed out, calls `let _ = child.kill().await;` and `let _ = child.wait().await;`.
   * Joins both reader tasks (with a short 100 ms drain timeout so a grandchild holding a pipe open cannot hang the runner).
   * Parses final `stdout` and `stderr` with `dialect.parse_output(&stdout_str, &stderr_str)`.
   * Computes `cost_usd_micros = self.rate_card.resolve_cost(parsed.tokens_in, parsed.tokens_out, parsed.vendor_cost_usd_micros)`.
   * Computes `paths_touched = pre_git.map(|g| g.diff_after(&spec.project_root)).unwrap_or_default()`.
   * Builds `output_excerpt` (truncated on a UTF-8 `char` boundary at `EXCERPT_LIMIT = 8192`). For `Timeout`, `output_excerpt` is `format!("`{}` exceeded its {}s timeout", self.binary, spec.timeout_secs)` (with any non-empty `parsed.excerpt_text` appended after `\n` and truncated to `EXCERPT_LIMIT`).
   * Emits the final `Exited` `AttemptHeartbeat` to `liveness_sink`.
   * Returns `Ok(AttemptOutcome { status, output_excerpt, duration_ms, tokens_in: parsed.tokens_in, tokens_out: parsed.tokens_out, cost_usd_micros, paths_touched })`.

---

## 5. CLI, Configuration, and Reporting (`crates/cli`)

### 5.1 `[adapters.<name>]` in `config.toml` (`crates/cli/src/config.rs`)

Add `pub adapters: BTreeMap<AdapterKind, AdapterConfig>` (default empty) to `Config` in `crates/cli/src/config.rs`:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterConfig {
    #[serde(default)]
    pub binary: Option<String>,
    #[serde(default)]
    pub input_usd_micros_per_mtok: u64,
    #[serde(default)]
    pub output_usd_micros_per_mtok: u64,
}
```

* Because the map key is `AdapterKind` (`claude`, `codex`, `antigravity`, `muse`, `grok`), any unknown adapter key in `[adapters.<unknown>]` fails config loading with a clear message naming the valid adapters (`claude, codex, antigravity, muse, grok`).
* If `binary` is `Some("")` (empty or whitespace-only), config validation refuses it with a clear message.

### 5.2 `fl attempt` (`crates/cli/src/cmd/attempt.rs` and `crates/cli/src/main.rs`)

1. **Arguments and adapter validation (first check in `run`):**
   * `--adapter <ADAPTER>` (default `"claude"`): parsed as the **very first check** in `cmd::attempt::run` via `AdapterKind::from_wire(&cmd.adapter)` (before resolving `cmd.record`, preserving `an_unknown_adapter_is_refused_and_names_the_ones_that_exist` in `crates/cli/tests/attempt.rs`). On unknown adapter, bail with:
     `` `{}` is not a known adapter. Known adapters: claude, codex, antigravity, muse, grok. ``
   * `--binary <BINARY>`: `Option<String>` (no longer hardcoded `default_value = "claude"`). Resolved as:
     `cmd.binary.or_else(|| cfg_adapter.and_then(|a| a.binary.clone())).unwrap_or_else(|| kind.default_binary().to_string())`.
2. **Plumbing from `main.rs` and `LivenessSink`:**
   * Update `Command::Attempt(c)` in `crates/cli/src/main.rs` to pass `&ctx`, `&store`, and `&cfg.adapters` into `cmd::attempt::run`.
   * Mint `let entry = stamp::entry_id();` before running the adapter so the `AttemptHeartbeat` carries the exact `attempt_id` that will be appended to the ledger.
   * Construct a `LivenessSink` that writes `StatePayload::Runtime(RuntimeEntry { value: serde_json::to_value(hb), merge_mode: MergeMode::LastWriter })` into `store` (which implements `StateStore`) under `StateScope::Workspace` with key `liveness_record_slug(&record.id)` (and `liveness_agent_slug(&persona)` when `AGENT_PERSONA` or a registered workspace agent resolves).
3. **Ledger append:**
   * Set `attempt.adapter = kind.as_wire().to_string()`.
   * Keep `settle(...)` and `conclude(...)` behavior and exit-code rules (Decisions 14 and 15) unchanged.

### 5.3 `fl agent liveness` (`crates/cli/src/cmd/agent.rs`)

Update `cmd::agent::run`'s trait bound from `S: AgentRegistry + Handles` to `S: AgentRegistry + StateStore + Handles` and add subcommand `Liveness` to `fl agent`:

```console
fl agent liveness [<target>] [--stale-after-ms <ms>] [--json]
```

* Reads `RuntimeEntry` items in `StateScope::Workspace` whose key starts with `liveness-agent-` or `liveness-record-`.
* When `<target>` is provided, matches entries where `<target>` equals `entry.key`, `hb.agent.as_deref()`, `hb.record.iri().as_str()`, `liveness_agent_slug(target)`, `liveness_record_slug(&RecordId(resolved_iri))`, or a record handle resolved via `store.resolve_handle(Kind::Record, ...)`.
* Deserializes each value as `AttemptHeartbeat` and evaluates `evaluate_liveness(Some(&hb), &now_at(), stale_after_ms, &local_hostname(), unix_pid_is_alive)`.
* If `<target>` is specified and no matching heartbeat entry exists, evaluates `evaluate_liveness(None, ...)` and prints `unverifiable (missing_status)`.
* Prints human-readable lines (`<key>\t<verdict>\tadapter=<adapter>\tduration=<ms>ms\ttokens=<in>/<out>\tcost=<cost> micro-USD`) or structured JSON when `--json` is passed.

### 5.4 `fl stats` (`crates/cli/src/cmd/stats.rs`)

Extend `fl stats`:

```console
fl stats --project <project> [--json]
```

1. **Text output (`report(&attempts, &coverage, &challenges)`):**
   * Always prints `attempts: <N>`.
   * When `Coverage::LocalOnly { reason }`, prints `note: this covers the local store only, because <reason>`.
   * When `attempts` is non-empty:
     * Status breakdown in sorted wire-name order: `  <status>: <count>`.
     * `wall clock: <total_ms>ms`
     * `tokens in: <total_tokens_in>`
     * `tokens out: <total_tokens_out>`
     * `cost: <total_cost> micro-USD`
     * **Cost floor notes:**
       * Count `zero_cost_attempts = attempts.iter().filter(|a| a.status != AttemptStatus::Refused && a.cost_usd_micros == 0).count()`.
       * When `total_cost == 0`: `note: no adapter reported a cost, so the figure above is a floor and not a total` (preserving existing tests when all attempts are zero-cost).
       * When `total_cost > 0` and `zero_cost_attempts > 0`: `note: <zero_cost_attempts> of <N> attempts reported no cost, so the figure above is a floor and not a total`.
     * **Per-adapter section (`by adapter:`):**
       * For each adapter in `BTreeMap` order:
         `  <adapter>: <count> attempts (<completed> completed), <ms>ms, <tokens_in> in / <tokens_out> out, <cost> micro-USD`
   * **Regret audit section:**
     * Queries `transitions = store.list_transitions(&project)?`.
     * Only when `transitions` has at least 2 transitions with non-empty `gates` and at least one of them is `Regret::Low` (avoiding unnecessary gate-run queries on projects with fewer than 2 gated transitions), collects unique `GateId`s across `transitions` and reads their `GateRun` logs from `store.gate_runs(&gate_id)?` (or `split.gate_runs(&gate_id)` when `source` is `Source::Split(split)` and `coverage == Coverage::Complete`, falling back to `store.gate_runs(&gate_id)?` on transient error).
     * Runs `audit_transition_regret(&transitions, &gate_runs)`.
     * For each `RegretChallenge`, prints:
       * `regret audit: transition <from> -> <to> (<name>) is declared low regret, but its measured mean (<measured_mean><unit>) is <ratio>x the project median (<project_median><unit>)` (where `<from>` and `<to>` use `as_wire()`, and `<unit>` is ` micro-USD` for `CostUsdMicros` and `ms` for `DurationMs`).
2. **JSON output (`--json`):**
   * Prints a single JSON object containing `attempts`, `coverage`, `by_status`, `wall_clock_ms`, `tokens_in`, `tokens_out`, `cost_usd_micros`, `zero_cost_attempts`, `by_adapter`, and `regret_audit`.

---

## 6. Verification and Testing Plan

1. **Unit tests in `fl-mcp` and black-box `crates/cli/tests/mcp.rs`:**
   * `vendor/muse.rs`: stdio server with secret `${VAR}`, renamed secret variable, HTTP/SSE remote server with Bearer and whole-secret headers, and preservation of `"schema_version": 1` and user keys in `.muse/settings.json`.
   * `vendor/grok.rs`: stdio server with `env` and `env_vars`, refusal of renamed secret variable, streamable HTTP with `bearer_token_env_var` and `env_http_headers`, refusal of SSE and non-Bearer secret header schemes, and preservation of TOML comments in `.grok/config.toml`.
   * Update `vendor/mod.rs`, `sync.rs`, and `crates/cli/tests/mcp.rs` for 5-vendor sync, check, adopt, and `.gitignore` verification.
2. **Unit tests in `fl-core` (`at.rs`, `telemetry.rs`, `tests/wire_refs.rs`):**
   * `At::unix_millis()` round-trip test.
   * `RateCard::resolve_cost`: vendor cost priority over rate card, rate-card calculation without overflow, zero floor when neither is present.
   * `liveness_record_slug` and `liveness_agent_slug`: verify `validate_slug` succeeds for UUID URNs, long GitHub Issue URLs, and mixed-case personas.
   * `evaluate_liveness`: `missing_status`, `live` on local host with live PID, `exited (crashed)` when local PID is dead (`ESRCH`), `unverifiable (stale_status)` when local heartbeat is stale, `live` vs `unverifiable (host_unavailable)` for remote host, and `exited` with recorded outcome.
   * `audit_transition_regret`: flags `low`-regret transition `>= 10x` project median, ignores `high`-regret transitions, ignores sub-threshold noise (`< 1_000`), and never mutates transition definitions.
   * `crates/core/tests/wire_refs.rs`: add `AttemptHeartbeat` (`floor = 3`) to `every_reference_field_on_the_wire_is_a_full_iri`.
3. **Unit and integration tests in `fl-exec` (`adapters/`):**
   * Dialect parser unit tests for `claude`, `codex`, `antigravity`, `muse`, and `grok` over single-JSON, streaming-JSONL, partial stream before timeout/crash, and plain-text fallback.
   * `GitSnapshot` unit tests: modified tracked file, new untracked file, deleted file, committed change during attempt, dangling symlink, and pre-existing dirty file left untouched (excluded from `paths_touched`).
   * `ProcessHarness` tests: zero-budget refusal, missing binary refusal, timeout with partial JSONL token capture, multibyte UTF-8 truncation, and `LivenessSink` heartbeat sequence (`Live` -> tick `Live` -> `Exited`).
4. **Black-box CLI tests in `fl-cli`:**
   * `config.rs`: `[adapters.<name>]` parsing and refusal of unknown adapter names.
   * `fl attempt`: end-to-end execution across `--adapter claude|codex|antigravity|muse|grok` using mock vendor scripts emitting JSON/JSONL, verifying stored `tokens_in`, `tokens_out`, `cost_usd_micros` (both vendor-reported and rate-card derived), `paths_touched`, and `liveness-*` `RuntimeEntry` records.
   * `fl agent liveness` and `fl stats [--json]`: end-to-end verification of text and JSON output, partial-cost floor notes, per-adapter rows, and regret audit lines.
