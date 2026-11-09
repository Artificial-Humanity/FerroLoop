# Antigravity follow-up deferred to Record 186

Ferris's final ruling on 2026-11-09, recorded in FerroState
`project:ferroloop` / `decision-376-scope`, supersedes this branch's earlier
implementation plan and scope decisions.

All of Issue #376 is deferred to the Record 186 adapter series. This branch is
preserved as design input only: **unreviewed for landing and deferred**, with no
pull request. Local task review and tests do not replace the required full
design review or Lead approval. The title task (Task 2) was not started.

The cleanup resolver and its tests remain at source commit
`e75e431693ebccc5ccde00ecddf3b235a4e1a443`. They explore bound descendant identity,
paired language-server endpoints, owned listener evidence, and teardown
tombstones. Destructive trajectory deletion requires a full design review in
Record 186 before any adoption.

Until that work is approved, manual `DeleteCascadeTrajectory` remains the
procedure. Surfacing means native remote UI/title; there is no startup
`GetCascadeTrajectorySteps` effect. Do not resume this branch's old plan without
a new assignment.
