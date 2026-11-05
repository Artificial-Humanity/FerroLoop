# Context window display, Phase 1: implementation plan (Local Record #134)

Spec: [2026-11-05-context-window-display-design.md](../specs/2026-11-05-context-window-display-design.md).

## Task 1: audit and hub fields (Rust)

1. Audit, per vendor, with the real files read-only: Grok (`~/.grok/models_cache.json`,
   `signals.json`, `summary.json`), Codex (`~/.codex/models_cache.json`, `config.toml`,
   rollout `token_count`), Claude (a transcript row that records the window or a `[1m]` tag,
   `~/.claude/settings.json`, `message.model`), Muse (`~/.local/share/muse/model-catalog`,
   `session.jsonl`), Antigravity (`GetCascadeTrajectory` `modelUsage`/steps, `agy models`).
   Write the evidence table into the spec.
2. Add `source` and `offered_windows` to `SessionContextUsage` (serde default, skipped when
   empty). Update every construction site.
3. Each vendor branch sets `max_tokens`, `source` and `offered_windows` from the audit. Remove the
   unconditional 1,000,000 for Claude and Muse where evidence exists; Grok falls back to the
   model's cache value before the registry default.
4. Tests per vendor and per source (`reported`, `catalog`, `assumed`) with synthetic files in temp
   dirs; the JSON shape (new fields present, absent when empty); existing expectations updated only
   where they encoded a guess.

## Task 2: UI

1. `ui/src/types/api.ts`: the new optional fields on the context usage types.
2. Remove `estimateSessionContextUsage`'s table in `ChatPane.tsx`; the meter uses the server value
   only. Roster and Agent Settings consume it; an `assumed` window is marked; offered sizes are
   listed read-only in Agent Settings.
3. FerroWatch shows used / max and the source per agent.
4. Tests for each component and for the no-data case.

## Final review

Whole-branch `code-review` on `origin/main..HEAD`, the full verification trio plus
`cd ui && npx tsc --noEmit && npm test && npm run build`, then the pull request and a review
request to Ferris.
