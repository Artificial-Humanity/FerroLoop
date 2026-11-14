# CLI test isolation: #539 implementation plan

## Decision

The Lead's ruling scopes #539 to accidental contact with live services by tests and their children. The previously proposed mandatory OS sandbox and authenticated capability broker are not adopted. Same-UID socket replacement, forged capabilities, and complete raw DNS or subprocess mediation are outside this defect and belong to the per-agent isolation work in Record #192.

## Tasks

1. Build a shared command fixture helper that clears inherited environment and provides private HOME, XDG, socket, route and executable overrides. It is only a convention, not a sandbox. Tests may override these settings for explicit fixtures. A caller supplies a fresh, private root and owns its lifetime. Windows host endpoints use a per-root named-pipe suffix; Unix sockets use short paths.
2. Migrate CLI integration-test `fl` subprocess construction and the in-process daemon fixtures to the shared isolation model. The latter need their own treatment: child process environment does not protect environment reads made by a daemon in the test process. Audit all inherited-environment clearing and executable launches, including non-`fl` children.
3. Add a source-lint test for `fl` launches and `env_clear` outside the approved helper, plus the crate guard test for default live runtime paths. Add the test-root endpoint tripwire in the clients that can bypass default discovery, including explicit endpoints, with platform-aware handling.
4. Independently review and verify each slice. Local verification uses pure or audited isolated fixtures; the previously denied live-capable host-runtime traces and live-capable full Rust suites are not run locally. Positive host and full-suite verification run in CI under the Lead's exception. A local strace audit is permitted only within the separately specified private namespace; if its permission is refused, stop and use CI evidence pending a separate CI-audit change.

The existing CLI hardening covers selected paths only. Until every migration, lint, guard and tripwire is reviewed and verified, it does not establish isolation of the complete CLI test suite. Wire, hub explicit endpoints, and other crates also remain in scope of #539; one crate's completion does not close the issue.
