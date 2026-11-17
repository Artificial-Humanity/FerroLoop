# Sandboxed local test runner

`scripts/test-sandbox-canary.sh` exercises the runner using freshly created,
test-owned sockets, a file and a process. It checks both that each host-side
probe works and that the same access fails inside the sandbox. It prints
namespace identifiers, the mount table and open descriptors. It verifies that
the network namespace differs before attempting to connect to sandbox-local
`127.0.0.1:8420`; it never connects to that host port. The socket path checks
are existence checks only. Any failure means containment is unproven.

Run the canary from a Linux checkout with bubblewrap, Python 3, a Rust toolchain
and an offline cargo registry installed:

```sh
scripts/test-sandbox-canary.sh
```

The default command of `scripts/test-sandboxed.sh` is
`cargo test --workspace --offline`; a command may instead follow `--`.
**The current #539 hold still prohibits running product tests in this runner.**
The owner must separately authorize a bounded full-suite experiment.

The runner builds a new mount namespace from explicit binds, uses separate
user, PID, network, IPC and UTS namespaces, drops capabilities, clears the
environment and closes inherited descriptors. It gives the command private
home, XDG, runtime and temporary directories, proc and devices. System tools,
libraries, the selected Rust sysroot, cargo registry and a source snapshot are
read-only; the only writable host-backed mount is an ephemeral target directory
under `/var/tmp`. The snapshot includes only Git-tracked files (including
tracked edits), omits Git metadata, untracked files and submodules, and refuses
nested mounts in the source tree or any host bind;
the sandbox and target are removed on exit. The runner rejects unsafe bind
inputs or missing bubblewrap flags rather than falling back to host execution.
It refuses to launch when non-ignored untracked files or ignored files in
source/test/build locations would be omitted. Stage intended inputs first;
review ignored files individually before using `git add -f`. Move unrelated
files out of those locations rather than staging runtime data or secrets. Known runtime
and build directories such as `target/` are excluded from this check and the
snapshot. The source-input policy is conservative, so it can require cleanup
of unrelated files before launch.

This protects against accidental access to live endpoints, files and
processes by tests and their children. The canary demonstrates the selected
controls on this host, not every possible test behavior. It is not a defense
against a malicious process running as the same host user or a kernel escape.
Read-only toolchain and registry mounts may become stale during a run; run the
canary again after changing the host or runner. The snapshot has no `.git`, so
tests requiring repository metadata may fail; offline dependencies must
already be present. A passing canary alone does not prove the full workspace
suite can build or pass unchanged, and it does not lift the local-test hold.
The canary checks configured project and state store sockets, configured and
database-derived Wire paths, and relevant environment overrides. It cannot
discover arbitrary per-command socket and store paths in other sessions.
