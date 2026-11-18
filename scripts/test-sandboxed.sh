#!/usr/bin/env bash
set -euo pipefail

# This is a fail-closed launcher, not a security boundary against hostile code.
die() { printf 'test-sandboxed: %s\n' "$*" >&2; exit 1; }
[[ $(uname -s) == Linux ]] || die 'Linux is required'
command -v bwrap >/dev/null || die 'bubblewrap is required'
command -v python3 >/dev/null || die 'python3 is required'
help=$(bwrap --help) || die 'cannot inspect bubblewrap flags'
for flag in --unshare-user --unshare-pid --unshare-net --unshare-ipc --unshare-uts --unshare-cgroup-try --die-with-parent --new-session --clearenv --cap-drop --chmod; do
    grep -Eq -- "^[[:space:]]+$flag([[:space:]]|$)" <<< "$help" || die "bubblewrap lacks $flag"
done
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
toolchain=$(rustc --print sysroot)
toolchain=$(realpath -e -- "$toolchain")
registry=$(realpath -e -- "${CARGO_HOME:-$HOME/.cargo}/registry")
[[ -x $toolchain/bin/cargo && -x $toolchain/bin/rustc ]] || die 'Rust toolchain missing cargo or rustc'
cc=$(realpath -e -- "$(command -v cc)") || die 'C compiler unavailable'
[[ $cc == /usr/bin/* && ${cc#/usr/bin/} != */* && -x $cc ]] || die 'C compiler must resolve to an executable directly in /usr/bin'
host=$("$toolchain/bin/rustc" -vV | sed -n 's/^host: //p')
[[ $host =~ ^[A-Za-z0-9_-]+$ ]] || die 'invalid Rust host triple'
linker_var=CARGO_TARGET_$(printf '%s' "$host" | tr '[:lower:]-' '[:upper:]_')_LINKER
python3 "$root/scripts/test-sandbox-source.py" --check "$root" || die 'stage intended source inputs before launching'

# No bind may expose a socket, even through a read-only mount. Refuse nested
# mounts too: their contents can change independently of the checked tree.
python3 "$root/scripts/test-sandbox-mounts.py" --check /usr/bin /usr/lib /usr/lib64 /usr/libexec /usr/include "$toolchain" "$registry" "$root" || die 'mount inspection failed'
for path in /usr/bin /usr/lib /usr/lib64 /usr/libexec /usr/include "$toolchain" "$registry"; do
    [[ -d $path && ! -L $path ]] || die "invalid bind directory: $path"
    sockets=$(find "$path" -xdev -type s -print -quit) || die "cannot inspect bind directory: $path"
    [[ -z $sockets ]] || die "socket in bind directory: $path"
done

scratch=$(mktemp -d /var/tmp/ferroloop-test-sandbox.XXXXXXXX)
target=
target_checked=0
persistent_target=0
recovery=
recovery_checked=0
scan_target() {
    if ((persistent_target)); then
        python3 "$root/scripts/test-sandbox-target.py" --check-path "$target" || return 1
    fi
    python3 "$root/scripts/test-sandbox-mounts.py" --check "$target" || return 1
    if ((persistent_target)); then
        python3 "$root/scripts/test-sandbox-target.py" --verify-persistent "$target"
    else
        python3 "$root/scripts/test-sandbox-target.py" "$target"
    fi
}
cleanup() {
    status=$?
    trap - EXIT
    if ((recovery_checked)); then
        if python3 "$root/scripts/test-sandbox-mounts.py" --check "$recovery" && python3 "$root/scripts/test-sandbox-target.py" "$recovery"; then
            rm -rf -- "$recovery" || status=1
        else
            printf 'test-sandboxed: unsafe recovery directory preserved: %s\n' "$recovery" >&2
            status=1
        fi
    fi
    if ((target_checked)) && ! scan_target; then
        printf 'test-sandboxed: target failed post-run inspection: %s\n' "$target" >&2
        status=1
    fi
    if [[ -n $recovery && -e $recovery && $recovery == "$scratch/"* ]]; then
        printf 'test-sandboxed: scratch preserved for inspection: %s\n' "$scratch" >&2
    else
        rm -rf -- "$scratch" || status=1
    fi
    exit "$status"
}
trap cleanup EXIT
mkdir -m 700 "$scratch/source"
if [[ -n ${FL_SANDBOX_TARGET_DIR:-} ]]; then
    target=$FL_SANDBOX_TARGET_DIR
    persistent_target=1
    python3 "$root/scripts/test-sandbox-target.py" --prepare "$target" || die 'invalid persistent target'
else
    target=$scratch/target
    mkdir -m 700 "$target"
fi
scan_target || die 'target failed pre-run inspection'
target_checked=1
min_free_gib=${FL_SANDBOX_MIN_FREE_GIB:-120}
python3 "$root/scripts/test-sandbox-space.py" --check / "$target" "$min_free_gib" || die 'free-space preflight failed'
# Recovery tests need a native filesystem. The directory is always created by
# this runner under the checked target and is inspected before removal.
recovery=$(mktemp -d "$target/.sandbox-recovery.XXXXXXXX")
chmod 700 "$recovery"
recovery_checked=1
case $(stat -f -c %t -- "$recovery") in
    ef53|58465342) ;;
    *) die 'recovery fixture requires ext4 or XFS' ;;
esac
python3 "$root/scripts/test-sandbox-mounts.py" --check "$recovery" || die 'invalid recovery mount'
python3 "$root/scripts/test-sandbox-target.py" "$recovery" || die 'invalid recovery directory'
# Snapshot only Git-tracked files, without VCS metadata or runtime data. Never
# traverse untracked files or submodules; tracked changes are included.
python3 - "$root" "$scratch/source" <<'PY'
import os, shutil, stat, subprocess, sys
src, dst = sys.argv[1:]
records = subprocess.check_output(['git', '-C', src, 'ls-files', '--stage', '-z']).split(b'\0')
for record in filter(None, records):
    metadata, raw_path = record.split(b'\t', 1)
    mode, _, stage = metadata.split()
    if stage != b'0': raise RuntimeError('unmerged source entry')
    name = os.fsdecode(raw_path)
    parts = name.split('/')
    if any(p in ('', '.', '..', '.git', 'target') for p in parts):
        raise RuntimeError('unsafe tracked source path: ' + name)
    if mode == b'160000': continue  # Submodules are not workspace members.
    source = os.path.join(src, name)
    destination = os.path.join(dst, name)
    for i in range(1, len(parts)):
        if os.path.islink(os.path.join(src, *parts[:i])):
            raise RuntimeError('symlinked parent of tracked source: ' + name)
    try: info = os.lstat(source)
    except FileNotFoundError: continue  # A tracked deletion is intentional.
    os.makedirs(os.path.dirname(destination), exist_ok=True)
    if stat.S_ISLNK(info.st_mode): os.symlink(os.readlink(source), destination)
    elif stat.S_ISREG(info.st_mode): shutil.copy2(source, destination, follow_symlinks=False)
    else: raise RuntimeError('unsupported tracked source entry: ' + name)
PY

args=(--unshare-user --unshare-pid --unshare-net --unshare-ipc --unshare-uts --unshare-cgroup-try
      --die-with-parent --new-session --clearenv --cap-drop ALL
      --dir /usr --ro-bind /usr/bin /usr/bin --ro-bind /usr/lib /usr/lib --ro-bind /usr/lib64 /usr/lib64 --ro-bind /usr/libexec /usr/libexec --ro-bind /usr/include /usr/include
      --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64
      --ro-bind "$toolchain" /toolchain --ro-bind "$registry" /cargo/registry
      --bind "$scratch/source" /source --bind "$target" /target --bind "$recovery" /recovery
      --chmod 0755 /usr --dir /usr/share --dir /usr/share/git-core --symlink /tmp/git-template /usr/share/git-core/templates
      --tmpfs /etc --dir /var --tmpfs /var/tmp --dir /usr/local --dir /sandbox-bin --symlink "$cc" /sandbox-bin/cc
      --tmpfs /home --dir /home/sandbox --tmpfs /tmp --dir /tmp/git-template --dir /tmp/git-template/hooks --dir /tmp/git-template/info --tmpfs /run --dir /run/user
      --proc /proc --dev /dev
      --setenv HOME /home/sandbox --setenv XDG_RUNTIME_DIR /run/user
      --setenv XDG_CONFIG_HOME /home/sandbox/config --setenv XDG_DATA_HOME /home/sandbox/data
      --setenv XDG_CACHE_HOME /home/sandbox/cache --setenv XDG_STATE_HOME /home/sandbox/state
      --setenv CARGO_HOME /cargo --setenv CARGO_TARGET_DIR /target --setenv CARGO_NET_OFFLINE true
      --setenv CC "$cc" --setenv "$linker_var" "$cc" --setenv GIT_TEMPLATE_DIR /tmp/git-template
      --setenv PATH /sandbox-bin:/toolchain/bin:/usr/bin --setenv TMPDIR /tmp --setenv LANG C.UTF-8
      --setenv FL_RECOVERY_TEST_DIR /recovery
      --chdir /source)
inherit_stdin=0
if [[ ${1:-} == --stdin ]]; then inherit_stdin=1; shift; fi
if [[ ${1:-} == -- ]]; then shift; fi
if (($# == 0)); then set -- /toolchain/bin/cargo test --workspace --offline; fi

# Close all descriptors inherited from the launching shell before bwrap starts.
if ((!inherit_stdin)); then exec </dev/null; fi
python3 "$root/scripts/test-sandbox-space.py" / "$target" "$min_free_gib" -- "$(command -v bwrap)" "${args[@]}" -- /usr/bin/python3 -c 'import os,signal,sys; open("/etc/hosts", "w").write("127.0.0.1 localhost\n::1 localhost\n"); signal.signal(signal.SIGINT, signal.SIG_DFL); os.execvp(sys.argv[1], sys.argv[1:])' "$@"
