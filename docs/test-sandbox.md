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
The owner must separately authorize a bounded full-suite experiment. A
compile-only `cargo test --workspace --offline --no-run` was separately
authorized; it compiles test executables but does not run them. Cargo build
scripts and procedural macros can execute during compilation.

On 2026-11-17, this compile-only command completed successfully on ai-lab-0
in 1 minute 40 seconds (exit status 0):

```sh
FL_SANDBOX_TARGET_DIR=/var/tmp/felix-sandbox-target-539 scripts/test-sandboxed.sh -- /toolchain/bin/cargo test --workspace --offline --no-run
```

The local command log was `/tmp/felix-sandbox-build.log` (a temporary file).
This proves that the workspace test executables compiled in that environment;
it does not prove that the tests pass or authorize executing them.

The runner builds a new mount namespace from explicit binds, uses separate
user, PID, network, IPC and UTS namespaces, drops capabilities, clears the
environment and closes inherited descriptors. It gives the command private
home, XDG, runtime and temporary directories, proc and devices. System tools,
libraries, the selected Rust sysroot, cargo registry and a source snapshot are
read-only; `/usr/libexec` supplies compiler internals and the resolved host C
compiler is selected explicitly for C builds and the Rust host linker. The only
writable host-backed mount is a target directory under `/var/tmp`. The snapshot includes only Git-tracked files (including
tracked edits), omits Git metadata, untracked files and submodules, and refuses
nested mounts in the source tree or any host bind;
the snapshot and default target are removed on exit. The runner rejects unsafe bind
inputs or missing bubblewrap flags rather than falling back to host execution.
It refuses to launch when non-ignored untracked files or ignored files in
source/test/build locations would be omitted. Stage intended inputs first;
review ignored files individually before using `git add -f`. Move unrelated
files out of those locations rather than staging runtime data or secrets. Known runtime
and build directories such as `target/` are excluded from this check and the
snapshot. The source-input policy is conservative, so it can require cleanup
of unrelated files before launch.

To retain build outputs across invocations, set `FL_SANDBOX_TARGET_DIR` to a
direct child of `/var/tmp`, for example `/var/tmp/my-sandbox-target`. The
directory is created with mode 0700 and must remain owned by the current user
with that mode. The runner rejects symlinks, sockets, other special entries,
and nested mounts before launch and checks again after the sandbox exits, even
if the command fails. A failed post-run check makes the runner fail; inspect
or remove the persistent directory yourself before reusing it. The runner does
not remove a persistent target.

The sandbox receives `/dev/null` on standard input by default. To pass the
launcher's input through deliberately, use `scripts/test-sandboxed.sh --stdin --
COMMAND ...`. Bubblewrap attempts a private cgroup namespace when supported;
the other listed namespaces are mandatory.

This protects against accidental access to live endpoints, files and
processes by tests and their children. The canary demonstrates the selected
controls on this host, not every possible test behavior. It is not a defense
against a malicious process running as the same host user or a kernel escape.
Read-only toolchain and registry mounts may become stale during a run; run the
canary again after changing the host or runner. The snapshot has no `.git`, so
tests requiring repository metadata may fail; offline dependencies must
already be present. The compile-only result above does not prove the suite
passes, and neither it nor a passing canary lifts the local-test hold.
The canary checks configured project and state store sockets, configured and
database-derived Wire paths, and relevant environment overrides. It cannot
discover arbitrary per-command socket and store paths in other sessions.
