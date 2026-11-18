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
The owner permits local product tests only through this runner. CI remains the
merge authority; report local results as sandboxed, including failure counts.
Cargo build scripts and procedural macros can execute during compilation.

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
libraries, the selected Rust sysroot and cargo registry are read-only; `/usr/libexec` supplies compiler internals and the resolved host C
compiler is selected explicitly for C builds and the Rust host linker. The disposable source snapshot and recovery fixture are writable host-backed
mounts, alongside the selected target directory. The snapshot includes only Git-tracked files (including
tracked edits), omits Git metadata, untracked files and submodules, and refuses
nested mounts in the source tree or any host bind;
the snapshot and default target are removed on exit unless an unsafe recovery fixture must be preserved. The runner rejects unsafe bind
inputs or missing bubblewrap flags rather than falling back to host execution.
It refuses to launch when non-ignored untracked files or ignored files in
source/test/build locations would be omitted. Stage intended inputs first;
review ignored files individually before using `git add -f`. Move unrelated
files out of those locations rather than staging runtime data or secrets. Known runtime
and build directories such as `target/` are excluded from this check and the
snapshot. The source-input policy is conservative, so it can require cleanup
of unrelated files before launch.

Set `FL_SANDBOX_TARGET_DIR` to `/data/toolchain/ferroloop-build/<persona>/<task>`
for builds on the lab host, for example
`/data/toolchain/ferroloop-build/felix/sandbox-proof`. The shared build root
must already exist; the persona and task directories are created as needed.
The target must be exactly two components below the build root. A direct child
of `/var/tmp` remains supported for compatibility; the default ephemeral
target is also under `/var/tmp`. Lab policy requires `/data` for real builds:
remove a task target after its PR merges. No component of either path may be a
symlink. New persona and task directories are normalized to mode 0700 even under a
setgid parent. Existing directories are never chmodded automatically; an
existing target must be owned by the current user and already have mode 0700.
Inspect and fix it yourself or choose a new target if the runner refuses it. The runner rejects symlinks, sockets, other special entries,
and nested mounts before launch and checks again after the sandbox exits, even
if the command fails. A failed post-run check makes the runner fail; inspect
or remove the persistent directory yourself before reusing it. The runner does
not remove a persistent target.

Each run creates a private 0700 recovery fixture on the checked target filesystem,
requires ext4 or XFS, and mounts it at `/recovery` as `FL_RECOVERY_TEST_DIR`.
The runner scans it for sockets, special files and nested mounts before removal;
unsafe fixtures are reported and preserved for inspection. The existing disk
watchdog covers its filesystem. `/etc` and `/var/tmp` are private tmpfs mounts;
localhost entries are synthesized and `/sandbox-bin/cc` points to the resolved
compiler. Synthetic `/usr` is mode 0755 and sandbox commands start with
the default SIGINT disposition; neither changes host files or supervisor signals.
The canary writes nonce markers in the writable areas, confirms the
checkout status and tracked tree hash remain unchanged, and prints mounts.

Before creating the source snapshot, the runner checks available space on `/`
and on the target filesystem; it refuses to proceed when either is below the
reserve or cannot be measured. It keeps checking while the sandbox runs and
stops the sandbox process group if space falls below the reserve or measurement
fails. `FL_SANDBOX_MIN_FREE_GIB` sets the reserve as an integer from 1 to 999999999 GiB;
it defaults to 120 GiB. A stopped run reports failure and requires cleanup or
more free space before retrying.

The sandbox receives `/dev/null` on standard input by default. To pass the
launcher's input through deliberately, use `scripts/test-sandboxed.sh --stdin --
COMMAND ...`. A sandbox-local Git template containing only empty `hooks` and `info`
directories gives `git init` its expected structure without exposing host Git
templates. Git’s default template path is a sandbox-only symlink to that same
private directory, so this also works when a test clears the environment. For an authorized full-suite run, pass
`--no-fail-fast` to Cargo so later test binaries run after failures.
Bubblewrap attempts a private cgroup namespace when supported;
the other listed namespaces are mandatory.

This protects against accidental access to live endpoints, files and
processes by tests and their children. The canary demonstrates the selected
controls on this host, not every possible test behavior. It is not a defense
against a malicious process running as the same host user or a kernel escape.
Read-only toolchain and registry mounts may become stale during a run; run the
canary again after changing the host or runner. The snapshot has no `.git`, so
tests requiring repository metadata may fail; offline dependencies must
already be present. The compile-only result above does not prove the suite
passes, and neither it nor a passing canary proves the full suite passes.
The canary checks configured project and state store sockets, configured and
database-derived Wire paths, and relevant environment overrides. It cannot
discover arbitrary per-command socket and store paths in other sessions.
