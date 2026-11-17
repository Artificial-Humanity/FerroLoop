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
mounts=$(findmnt -rn -o TARGET) || die 'cannot enumerate host mounts'
for path in /usr/bin /usr/lib /usr/lib64 /usr/include "$toolchain" "$registry"; do
    [[ -d $path && ! -L $path ]] || die "invalid bind directory: $path"
    sockets=$(find "$path" -xdev -type s -print -quit) || die "cannot inspect bind directory: $path"
    [[ -z $sockets ]] || die "socket in bind directory: $path"
    if printf '%s\n' "$mounts" | awk -v p="$path/" 'index($0,p)==1 {found=1} END {exit !found}'; then die "nested mount in bind directory: $path"; fi
done

scratch=$(mktemp -d /var/tmp/ferroloop-test-sandbox.XXXXXXXX)
trap 'rm -rf -- "$scratch"' EXIT
mkdir "$scratch/source" "$scratch/target"
# Snapshot without VCS metadata, sockets, or build outputs. Never follow links.
python3 - "$root" "$scratch/source" <<'PY'
import os, shutil, stat, sys
src, dst = sys.argv[1:]
def ignore(directory, names):
    omitted = []
    for name in names:
        path = os.path.join(directory, name)
        mode = os.lstat(path).st_mode
        if name in ('.git', 'target') or stat.S_ISSOCK(mode):
            omitted.append(name)
        elif not (stat.S_ISDIR(mode) or stat.S_ISREG(mode) or stat.S_ISLNK(mode)):
            raise RuntimeError('unsupported source entry: ' + path)
    return omitted
shutil.copytree(src, dst, symlinks=True, ignore=ignore, dirs_exist_ok=True)
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
