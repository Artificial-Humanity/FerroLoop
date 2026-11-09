# Passive Antigravity Identity Implementation Plan

> **For agentic workers:** Execute with the repository subagent-dev skill: fresh implementer and independent review per task, then whole-branch review.

**Goal:** Prevent command output from claiming a persona and remove cached passive Antigravity claims lacking current source evidence.
**Architecture:** Keep the existing bounded transcript scanner and discovery/reconciliation paths. No route schema or explicit priority policy changes.
**Tech Stack:** Rust, serde_json, existing isolated fixture helpers.
**Spec:** ../specs/2026-11-09-passive-agy-identity-360-design.md

## Global constraints

Only synthetic fixtures. No live brain reads by implementers, no host or route mutations. No new dependencies. Fiona coauthor on every commit. Identity-age versus endpoint-freshness changes are deferred.

## Review focus

Output-level persona fields and nested output must not bypass filtering (task 1). Legitimate structured CommandLine registration must survive (task 1). Missing sources and unknown timestamps must fail closed for passive claims (task 2). Invalid cached identities must stay removed through persistence and dispatch reconciliation (task 2). Concurrent explicit registration and existing same-vendor refresh must survive (task 2).

## Task 1: Reject tool output before identity extraction

Modify/test crates/wire/src/identity.rs.
- [ ] Write failing synthetic tests: reverse GENERIC Wesley positive; GENERIC/generic output quoting Kayla with/without Showing lines, multiline, output persona fields, nested response_item output. Existing initial SYSTEM_MESSAGE subagent fixture remains rejected.
- [ ] Add positive PLANNER_RESPONSE tool_calls args CommandLine registration and USER_INPUT bootstrap tests, including subsequent rejected output unable to override them.
- [ ] Run focused tests to demonstrate red, then classify GENERIC/generic as output before identity fields and remove the exceptional branch. Keep structured tool input behavior.
- [ ] Run fl-wire tests and fmt check, commit with Fiona trailer. Cite shape survey in commit message.

## Task 2: Revalidate cached passive Antigravity claims

Modify/test crates/wire/src/identity.rs; caller tests only if required by reconciliation evidence.
Consumes scan_transcript_for_persona(path, 50) from task 1. Keep its byte and line bounds.
- [ ] Write failing tests for cached concrete and placeholder passive claims: invalid GENERIC-only, persona mismatch, absent/unreadable source, absent brain directory, and zero mtime. Valid source remains discoverable. Explicit routes survive unavailable/invalid passive evidence.
- [ ] Test loading saved primary and legacy fallback fixtures followed by discovery, merge_newer of validated snapshots, both dispatch reconciliation paths, and a concurrent explicit registration surviving removal. Preserve explicit cross-vendor and alias protections and same-vendor refresh tests.
- [ ] Implement minimal revalidation and removal/refusal in existing discovery; ensure early returns do not bypass invalidation. Unknown/zero mtime must never fall back to now for passive Antigravity identity evidence. Do not split route timestamps or alter freshness/tie policy.
- [ ] Run focused tests red/green, full fl-wire tests and fmt check; commit with Fiona trailer.

## Finish

- [ ] Independent review each task; remediate through fresh fixer/reviewer.
- [ ] Full cargo test --workspace, cargo clippy --all-targets --workspace -- -D warnings, cargo fmt --all -- --check, git diff --check. Unit and integration suites included.
- [ ] Whole-branch read-only review. Post archive classifier deferral on #360 and send timestamp follow-up draft to Ferris.
- [ ] Push and open via fl github pr create, cite surveys and any unresolved live/archive classification, request Ferris review. Merge only after current-head approval and green checks.
