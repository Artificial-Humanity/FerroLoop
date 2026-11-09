# Antigravity CLI lifecycle follow-up

## Scope

Ferris's #376 scope decision limits this change to missing-route/environment cascade cleanup
and `annotations/<cid>.pbtxt` display titles on the existing `AntigravityCliAdapter` path.
Records 179/181 (trust prompts and remote-control launch flags) are already implemented.
`AntigravityHostAdapter`, remote-control surfacing, and startup trajectory RPC work belong to
the Record 186 adapter series and are excluded here.

## Approach

Extend the existing CLI adapter with a small shared Antigravity session resolver. Retain the
existing cached-route cleanup path and two-second total deletion budget. When an ID or endpoint
is missing, use a verified exact host session and its process tree. A new HostAdapter migration
would exceed the assignment; broad route discovery or newest-trajectory selection cannot prove
that a destructive operation targets the requested session.

Prefer a native UUID already held by the hosted record. A missing UUID can be recovered only
from a positively bound descendant's conversation environment. Keep language-server address and
CSRF token paired to one source. For an idle session with a known UUID, an identified language-
server child can supply its token argument and listener: correlate its socket file descriptors
with LISTEN socket inodes, because `/proc/<pid>/net/tcp` alone covers the network namespace.
Reject ambiguous listeners, conflicting identities, foreign ownership, replaced processes, and
host-name placeholders. Recheck process birth before using recovered evidence. Unsupported
platforms or process shapes produce a bounded diagnostic, never a guessed target.

All new process probing lives beside `AntigravityCliAdapter`, with fixture-injected process and
host sources for tests. Real host probing is opt-in from runtime-enabled lifecycle paths; a
temporary home does not by itself authorize probing the live machine. The complete discovery
and deletion operation shares the existing two-second budget. Bound process, thread, descriptor,
file, and response reads. Keep existing loopback-only, no-proxy, no-redirect RPC behavior.

## Titles and delayed identity

After a successful hosted CLI launch, an optional CLI-adapter finalization hook resolves the
native identity and writes `title: "<DisplayName>"` to the exact UUID annotation. Use the registry
display name, with the existing capitalized persona fallback. Persist a newly verified native
ID in the hosted record. If identity is not available yet, the existing running-session refresh
retries finalization; this also repairs a title after registration supplies the native route.
The operation is idempotent and makes no vendor inference calls. Do not write an annotation for
an unresolved, ambiguous, or persona-derived ID.

Preserve unrelated annotation fields, escape protobuf strings, bound file size, reject symlink
traversal, and write atomically. Report failure without turning an otherwise successful launch
into a rollback. Do not repeatedly write a correct title or persist an unchanged record.

## Validation

Use isolated process fixtures and mock host/RPC endpoints. Cover missing routes and brain,
saved/missing/conflicting UUIDs, idle LS recovery, namespace-only listeners, endpoint/token
pairing, owner/PID/epoch changes, scan/deadline limits, and sibling isolation. Title tests cover
delayed identity, exact filename, registry display name, escaping, field preservation,
idempotence, and symlink refusal. A lifecycle regression must demonstrate that the hook is
actually called and that recovered native identity reaches durable state. No live vendor
sessions, language-server RPCs, or service restarts are needed for development or validation.
