# Native host foundation

`fl-host` is a standalone daemon. `fl-host-proto` is the local IPC it speaks.
This slice is authenticated local IPC and pipe-child containment. It does not
replace a live agent session.

A client and the daemon prove a shared token with mutual nonces before any
request. The token does not cross the wire. Unix accepts only the same
effective uid. Windows accepts only the same process-token SID. A failed peer
lookup is a refusal.

Children are pipes. Unix containment is a per-session supervisor. Windows
children are born in a kill-on-close Job. Stdout stays in a bounded per-session
backlog. Each subscriber has its own queue. A slow subscriber is dropped and
does not block the others. Disconnect is unconfirmed: a socket write does not
prove the client consumed the event. Ack and replay are not in this slice.

One writer lease belongs to one session epoch and to the control connection
that claimed it. `Send` reports written, not written, or ambiguous. Abandoned
partial input makes that epoch `Fenced`. `Fenced` stays fenced under every
later lease. Input opens again only after kill and respawn with a strictly
newer epoch.

Not in this slice: ack/replay, PTY, hub, wire, or CLI adapters, and live
session migration. The installer does not ship `fl-host`. The tests do not
log into a vendor, start an external daemon, or call inference.

## What the checks prove

`client_refuses_foreign_uid_server` is a pure uid table. It does not create a
second uid. `windows_peer_identity_lookup_unavailable_refused` is a pure lookup
table, including access denied. It does not run another user or an elevated
process. `windows_pipe_options_are_local_and_non_inheritable`,
`windows_pipe_dacl_single_ace`, and `windows_token_dacl_applies_at_creation`
check options and an SDDL string. They do not connect as another SID or from
another machine.

On Windows, the real listener reads `GetSecurityInfo` on the pipe and token
handles it created, and it accepts a same-user client. That is a same-user
real-handle ACL check. It is not a second-user denial and it is not a
remote-machine denial.

The macOS and Windows jobs run these same crate tests. They do not exercise
another uid, another SID, or a remote client.

## Commands

Linux is where this suite was executed for the commit that adds the jobs:

```
cargo fmt --all --check
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace
```

The `fl-host (macos)` and `fl-host (windows)` jobs each install Rust 1.98 with
upstream rustup, including clippy and rustfmt, then run:

```
cargo fmt --all --check
cargo clippy -p fl-host -p fl-host-proto --all-targets -- -D warnings
cargo test -p fl-host -p fl-host-proto
```

The macOS job sets `TMPDIR=/tmp/flh` (mode `0700`) before `cargo test` so Unix
socket paths fit in `sun_path` (104 bytes including NUL). macOS and Windows
results come from those jobs after the pull request exists. The jobs are not
required checks. The required check remains `test, clippy, fmt` on Ubuntu.
Integration coverage is the real `fl-host` and `fl-host-testchild` subprocesses
inside `cargo test`.
