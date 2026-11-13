# CLI test isolation: design decision for #539

This is a design checkpoint. The CLI test suite is **not isolated** and must not be run without the existing external restrictions on all `connect` calls and child `exec` calls. The fixture hardening commits cover selected inherited paths only.

## Evidence

The current checkout has 79 references to the normal `fl` executable in CLI integration tests and 27 `env_clear` calls. `wire_e2e.rs` starts a wire daemon in the test process at seven sites. Some integration tests directly bind Unix or TCP listeners; test code and dependencies also spawn programs such as git, shell scripts, and vendor stubs. A test process or its descendants can therefore reach outside the wire/host APIs guarded today.

The wire guard's root and exact socket inode registrations are process-local. Enabling it separately in a parent test and an `fl` child does not share authority to connect to a parent fixture. Its constructor is currently specific to the wire library's unit-test binary: dependency crates do not inherit `cfg(test)`. A marker in an environment variable is insufficient because `env_clear` removes it, and a marker by itself is not proof that a target is a fixture.

## Candidate designs

1. **Private capability broker and test-only executable.** Give every integration test binary an unconditional pre-main guard and use a distinct, explicitly test-only `fl` binary in place of every integration-test invocation of the production binary. Create a private broker per test run. Pass a pre-opened authenticated control channel to descendants using an inherited handle that survives environment clearing; the guarded test binary must refuse startup if its capability is absent. The broker grants narrowly scoped access to exact fixtures after validating ownership, path and socket identity, and revokes grants with fixture lifetime. The design must account for handle inheritance by grandchildren, close-on-exec policy, races involving replacement of a same-UID socket, listener startup, and resource cleanup. Merely checking path prefixes or trusting a child-provided path is insufficient. Use a separate executable target or equivalent build boundary so a normal `fl` cannot enable test semantics through Cargo feature unification.
2. **Operating-system boundary around each test and descendants.** Run the suite in a disposable sandbox with a private filesystem, network and process boundary, exposing only explicit fixture endpoints and executables. The boundary must persist across `env_clear` and all descendants. This is potentially simpler to reason about on systems with suitable kernel support, but requires an independently reviewed implementation on every supported platform; unsupported platforms must fail closed, not silently run unsandboxed. Running in a private HOME alone is not a boundary.

Either approach must cover raw socket and DNS APIs as well as the project's host, store, wire and hub clients; arbitrary executable launches and shell descendants; default and explicitly configured endpoints; and in-process daemons. Vendor and tmux execution must require an exact private fixture or be denied. A broker based only on application-level checks does not confine arbitrary subprocesses or raw socket calls, so option 1 still requires a complete audited mediation mechanism for those paths or an OS boundary. Unregistered or unverifiable endpoints must be refused. Existing fixture tests need explicit, scoped grants rather than ambient access to temporary directories.

## Decision needed before implementation

A reviewer must choose whether the supported test environment can require a platform sandbox, or whether the project will build and maintain equivalent cross-platform process and socket mediation plus authenticated fixture delegation. Neither design is yet proven sufficient for this suite. This is a design decision, not a request to weaken the safety boundary or change production behavior. The reviewer should resolve the same-UID replacement threat and decide which platforms can run the complete suite; unsupported environments must report that limitation.

## Verification sequence

1. Inventory all integration targets, child commands, direct daemon starts, socket/DNS call sites, and fixture listeners. Confirm no invocation still points at production `fl`.
2. Develop negative tests for absent, forged, closed, inherited and revoked capabilities; `env_clear`; renamed binaries; symlink and same-UID replacement attempts; and attempted live host, wire, hub, vendor, tmux, TCP and DNS access. Explicitly test production binaries remain unchanged.
3. Until the boundary is reviewed and proven, run only non-spawning tests or tests under the existing external controls: fault-inject **all** connect syscalls and deny **all** child `exec` calls. Compile with `cargo test --no-run` under `/var/tmp`.
4. After independent review, trace the full process tree and verify every attempted connection and execution against the approved exact fixture set, including paths after `env_clear` and grandchildren. Exercise successful private mocks as well as refusals. Keep the prior automatic-review rejection of the live-capable host-runtime trace in force; do not retry or bypass it without an approved safe method.
5. Run the appropriate suites, clippy and formatting only within that approved boundary. Report unsupported platforms and remaining audit scope separately; do not claim #539 complete from CLI work alone.
