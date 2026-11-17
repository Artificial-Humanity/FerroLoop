#!/usr/bin/env bash
set -euo pipefail

# This is a fail-closed launcher, not a security boundary against hostile code.
die() { printf 'test-sandboxed: %s\n' "$*" >&2; exit 1; }
[[ $(uname -s) == Linux ]] || die 'Linux is required'
command -v bwrap >/dev/null || die 'bubblewrap is required'
command -v python3 >/dev/null || die 'python3 is required'
for flag in --unshare-user --unshare-pid --unshare-net --unshare-ipc --unshare-uts --die-with-parent --new-session --clearenv --cap-drop; do
    bwrap --help | grep -q -- "$flag" || die "bubblewrap lacks $flag"
done
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
toolchain=$(rustc --print sysroot)
toolchain=$(realpath -e -- "$toolchain")
registry=$(realpath -e -- "${CARGO_HOME:-$HOME/.cargo}/registry")
[[ -x $toolchain/bin/cargo && -x $toolchain/bin/rustc ]] || die 'Rust toolchain missing cargo or rustc'

# No bind may expose a socket, even through a read-only mount. Refuse nested
# mounts too: their contents can change independently of the checked tree.
python3 "$root/scripts/test-sandbox-mounts.py" --check /usr/bin /usr/lib /usr/lib64 /usr/include "$toolchain" "$registry" "$root" || die 'mount inspection failed'
for path in /usr/bin /usr/lib /usr/lib64 /usr/include "$toolchain" "$registry"; do
    [[ -d $path && ! -L $path ]] || die "invalid bind directory: $path"
    sockets=$(find "$path" -xdev -type s -print -quit) || die "cannot inspect bind directory: $path"
    [[ -z $sockets ]] || die "socket in bind directory: $path"
done

scratch=$(mktemp -d /var/tmp/ferroloop-test-sandbox.XXXXXXXX)
trap 'rm -rf -- "$scratch"' EXIT
mkdir "$scratch/source" "$scratch/target"
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

args=(--unshare-user --unshare-pid --unshare-net --unshare-ipc --unshare-uts
      --die-with-parent --new-session --clearenv --cap-drop ALL
      --ro-bind /usr/bin /usr/bin --ro-bind /usr/lib /usr/lib --ro-bind /usr/lib64 /usr/lib64 --ro-bind /usr/include /usr/include
      --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64
      --ro-bind "$toolchain" /toolchain --ro-bind "$registry" /cargo/registry
      --ro-bind "$scratch/source" /source --bind "$scratch/target" /target
      --tmpfs /home --dir /home/sandbox --tmpfs /tmp --tmpfs /run --dir /run/user
      --proc /proc --dev /dev
      --setenv HOME /home/sandbox --setenv XDG_RUNTIME_DIR /run/user
      --setenv XDG_CONFIG_HOME /home/sandbox/config --setenv XDG_DATA_HOME /home/sandbox/data
      --setenv XDG_CACHE_HOME /home/sandbox/cache --setenv XDG_STATE_HOME /home/sandbox/state
      --setenv CARGO_HOME /cargo --setenv CARGO_TARGET_DIR /target --setenv CARGO_NET_OFFLINE true
      --setenv PATH /toolchain/bin:/usr/bin --setenv TMPDIR /tmp --setenv LANG C.UTF-8
      --chdir /source)
if [[ ${1:-} == -- ]]; then shift; fi
if (($# == 0)); then set -- /toolchain/bin/cargo test --workspace --offline; fi

# Close all descriptors inherited from the launching shell before bwrap starts.
python3 - "$(command -v bwrap)" "${args[@]}" -- "$@" <<'PY'
import os, sys
for name in os.listdir('/proc/self/fd'):
    fd = int(name)
    if fd > 2:
        try: os.close(fd)
        except OSError: pass
os.execv(sys.argv[1], sys.argv[1:])
PY
