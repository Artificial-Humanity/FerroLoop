#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == --inside && $(readlink /proc/self/fd/0) != /dev/null ]]; then
    printf 'FAIL: sandbox standard input is not /dev/null\n' >&2
    exit 1
fi
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
exec /usr/bin/python3 - "$here" "$@" <<'PY'
import errno, os, secrets, signal, socket, subprocess, sys, tempfile, time, tomllib

here, *args = sys.argv[1:]

def connect(family, address):
    with socket.socket(family) as sock:
        sock.settimeout(1)
        return sock.connect_ex(address)

def require(condition, message):
    if not condition:
        raise SystemExit('FAIL: ' + message)

if args and args[0] == '--inside':
    _, host_net, unix_path, abstract_name, port, filename, pid, *absent = args
    port, pid = int(port), int(pid)
    inside_net = os.readlink('/proc/self/ns/net')
    print('host net namespace:', host_net, 'sandbox net namespace:', inside_net, flush=True)
    require(inside_net != host_net, 'network namespace matches host; no connections attempted')
    for name in ('user', 'pid', 'net', 'ipc', 'uts', 'mnt'):
        print('namespace', name, os.readlink('/proc/self/ns/' + name))
    print('mount table:')
    print(open('/proc/self/mountinfo').read())
    print('open descriptors:')
    for fd in sorted(os.listdir('/proc/self/fd'), key=int):
        try: print(fd, os.readlink('/proc/self/fd/' + fd))
        except FileNotFoundError: pass
    require(connect(socket.AF_UNIX, unix_path) != 0, 'host filesystem socket reachable')
    require(connect(socket.AF_UNIX, '\0' + abstract_name) != 0, 'host abstract socket reachable')
    require(connect(socket.AF_INET, ('127.0.0.1', port)) != 0, 'host test listener reachable')
    result = connect(socket.AF_INET, ('127.0.0.1', 8420))
    require(result == errno.ECONNREFUSED, 'sandbox loopback :8420 was not refused: ' + str(result))
    try:
        with open(filename, 'a') as f: f.write('sandbox write\n')
    except OSError: pass
    else: raise SystemExit('FAIL: host sentinel writable')
    require(not os.path.exists('/proc/' + str(pid)), 'sentinel PID visible in sandbox; signal skipped')
    try: os.kill(pid, signal.SIGUSR1)
    except ProcessLookupError: pass
    except PermissionError: pass
    else: raise SystemExit('FAIL: host sentinel signal accepted')
    for path in absent:
        require(not os.path.lexists(path), 'live path exists inside: ' + path)
    print('PASS: sandbox negative controls and absent paths', flush=True)
    raise SystemExit(0)

require(not args, 'unexpected arguments')
subprocess.run(['/usr/bin/python3', os.path.join(here, 'test-sandbox-mounts.py'), '--self-test'], check=True)
subprocess.run(['/usr/bin/python3', os.path.join(here, 'test-sandbox-source.py'), '--self-test'], check=True)
subprocess.run(['/usr/bin/python3', os.path.join(here, 'test-sandbox-target.py'), '--self-test'], check=True)
nonce = secrets.token_hex(16)
home = os.path.expanduser('~')
uid = os.getuid()
host_net = os.readlink('/proc/self/ns/net')

# Read configuration without opening a store or connecting to a socket. Cover
# each configured project and state store, FL_DB, and Wire's configured and
# database-derived socket paths. Explicit command-line paths in other sessions
# cannot be discovered here; this is a bounded assertion, not an inventory.
config_home = os.environ.get('XDG_CONFIG_HOME', '')
if not os.path.isabs(config_home): config_home = os.path.join(home, '.config')
config_path = os.path.join(config_home, 'fl/config.toml')
try:
    with open(config_path, 'rb') as f: config = tomllib.load(f)
except FileNotFoundError:
    config = {}
except (OSError, tomllib.TOMLDecodeError) as e:
    raise SystemExit('FAIL: cannot inspect FerroLoop config: ' + str(e))
require(isinstance(config.get('project', []), list), 'invalid project config')
wire = config.get('wire', {})
state = config.get('state', {})
require(isinstance(wire, dict) and isinstance(state, dict), 'invalid wire/state config')
configured = []
def add_path(value):
    require(isinstance(value, str) and value != '', 'invalid configured path')
    configured.append(os.path.abspath(value))
if wire.get('socket') is not None: add_path(wire['socket'])
stores = []
for entry in config.get('project', []):
    require(isinstance(entry, dict) and isinstance(entry.get('store'), str), 'invalid project store')
    stores.append(entry['store'])
if state.get('store') is not None: stores.append(state['store'])
if os.environ.get('FL_DB'): stores.append(os.environ['FL_DB'])
data_home = os.environ.get('XDG_DATA_HOME', '')
if not os.path.isabs(data_home): data_home = os.path.join(home, '.local/share')
stores.append(os.path.join(data_home, 'fl/fl.redb'))
for store in stores:
    require(isinstance(store, str) and store != '', 'invalid store path')
    store = os.path.abspath(store)
    stem, extension = os.path.splitext(store)
    ferro = stem + '.fls' if extension else store + '.fls'
    for path in (store + '.sock', ferro + '.sock', os.path.join(os.path.dirname(store), 'fl-wire.sock')):
        add_path(path)
with tempfile.TemporaryDirectory(prefix='ferroloop-canary-', dir='/var/tmp') as directory:
    unix_path = os.path.join(directory, 's-' + nonce)
    abstract_name = 'ferroloop-canary-' + nonce
    filename = os.path.join(directory, 'file-' + nonce)
    marker = os.path.join(directory, 'signal-' + nonce)
    with open(filename, 'w') as f: f.write(nonce + '\n')
    sockets = [socket.socket(socket.AF_UNIX), socket.socket(socket.AF_UNIX), socket.socket(socket.AF_INET)]
    try:
        sockets[0].bind(unix_path)
        sockets[1].bind('\0' + abstract_name)
        sockets[2].bind(('127.0.0.1', 0))
        # Never connect to host port 8420, even if the ephemeral allocator
        # happens to choose it for this test-owned listener.
        if sockets[2].getsockname()[1] == 8420:
            sockets[2].close()
            sockets[2] = socket.socket(socket.AF_INET)
            sockets[2].bind(('127.0.0.1', 0))
            require(sockets[2].getsockname()[1] != 8420, 'ephemeral listener selected reserved port 8420')
        for s in sockets: s.listen(8)
        port = sockets[2].getsockname()[1]
        child_code = ('import signal,time,sys\n'
                      'signal.signal(signal.SIGUSR1, lambda *_: open(sys.argv[1], "w").write("received"))\n'
                      'print("ready", flush=True)\n'
                      'while True: time.sleep(1)\n')
        child = subprocess.Popen(['/usr/bin/python3', '-c', child_code, marker], stdout=subprocess.PIPE, text=True, close_fds=True)
        try:
            require(child.stdout.readline().strip() == 'ready', 'sentinel process did not start')
            require(connect(socket.AF_UNIX, unix_path) == 0, 'host filesystem socket control')
            require(connect(socket.AF_UNIX, '\0' + abstract_name) == 0, 'host abstract socket control')
            require(connect(socket.AF_INET, ('127.0.0.1', port)) == 0, 'host loopback control')
            with open(filename, 'a') as f: f.write('host write\n')
            os.kill(child.pid, signal.SIGUSR1)
            for _ in range(100):
                if os.path.exists(marker): break
                time.sleep(.01)
            require(os.path.exists(marker), 'host signal control')
            expected = open(filename).read()
            absent = [f'/run/user/{uid}/ferroloop/fl-host.sock', f'/tmp/ferroloop-{uid}/fl-host.sock',
                      f'/tmp/tmux-{uid}', os.path.join(home, '.codex/app-server-control/app-server-control.sock'),
                      os.path.join(home, '.local/share/fl/ferroloop.fls.sock'),
                      f'/run/user/{uid}/fl-wire.sock', '/tmp/fl-wire.sock',
                      os.path.join(home, '.cache/fl/wire.sock'),
                      os.path.join(home, '.local/share/fl/fl-wire.sock')]
            runtime = os.environ.get('XDG_RUNTIME_DIR')
            if runtime:
                absent += [os.path.join(runtime, 'fl-wire.sock'), os.path.join(runtime, 'ferroloop/fl-host.sock')]
            codex_home = os.environ.get('CODEX_HOME')
            if codex_home:
                absent.append(os.path.join(codex_home, 'app-server-control/app-server-control.sock'))
            absent += [os.environ[k] for k in ('FL_HOST_SOCK', 'FL_WIRE_SOCKET', 'CODEX_APP_SERVER_CONTROL_SOCKET') if os.environ.get(k)]
            absent += configured
            absent = sorted({p for name in absent for p in (os.path.abspath(name), os.path.realpath(name))})
            print('Checking', len(absent), 'configured, default and resolved socket paths by existence only', flush=True)
            print('PASS: host positive controls; sentinel nonce', nonce, flush=True)
            command = [os.path.join(here, 'test-sandboxed.sh'), '--', '/usr/bin/bash',
                       '/source/scripts/test-sandbox-canary.sh', '--inside', host_net, unix_path,
                       abstract_name, str(port), filename, str(child.pid), *absent]
            result = subprocess.run(command, close_fds=True)
            require(result.returncode == 0, 'sandbox canary failed')
            require(open(filename).read() == expected, 'sentinel changed inside sandbox')
            require(child.poll() is None, 'sentinel process exited')
            print('PASS: all owned-sentinel controls')
        finally:
            child.terminate()
            child.wait(timeout=5)
    finally:
        for s in sockets: s.close()
PY
