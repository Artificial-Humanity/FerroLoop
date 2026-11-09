# Passive Antigravity identity validation (#360)

Ferris approved this scope on 2026-11-09 after the design audit of main 78df6ed8.

Reject GENERIC/generic output (including nested response_item output) before reading persona fields. Remove the Showing-lines exception. Preserve user bootstrap and structured tool-call input, the existing initial SYSTEM_MESSAGE subagent guard, and explicit cross-vendor/alias protection plus same-vendor refresh policy.

Read-only local shape survey: 534 transcripts; row counts CHECKPOINT 124, ERROR_MESSAGE 20, GENERIC 24119, PLANNER_RESPONSE 19188, SYSTEM_MESSAGE 2830, USER_INPUT 174. GENERIC had no command-input fields. Registration-pattern counts: GENERIC /content 197; PLANNER_RESPONSE /tool_calls[]/args/CommandLine 101 (9742 total CommandLine fields). A follow-up transcript count found 27 with GENERIC registration patterns, 16 with neither structured CommandLine nor USER_INPUT registration patterns. This is a pattern count, not proof of active identity loss or archive status. No contents, identifiers, or secrets were printed or saved. Existing PLANNER_RESPONSE scanning invokes tool_call_argument_text (identity.rs:3257); that function recursively reads args (3116–3118), including CommandLine.

Revalidate cached passive Antigravity identities, including concrete conversation IDs, with the existing bounded scan. Drop/refuse a passive claim whose source is missing, unreadable, or no longer validates its persona. Unknown/zero mtime must not become scan-time identity evidence. Preserve explicit registrations. Exercise persistence loading and reconciliation so invalid claims do not survive ordinary reload/dispatch, and concurrent explicit registrations survive pruning.

Do not add archive detection without a validated schema. Parent-reference/archive classification is deferred. Separating identity evidence timestamps from endpoint freshness, and changing merge_newer freshness comparison/ties, are deferred to a Lead-filed follow-up. Existing endpoint refresh behavior is outside this PR. No new discovery machinery or live route/host mutations. All test fixtures must be synthetic and isolated.
