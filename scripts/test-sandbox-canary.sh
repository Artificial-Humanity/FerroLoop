#!/usr/bin/env bash
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
exec /usr/bin/python3 - "$here" "$@" <<'PY'
import errno, os, secrets, signal, socket, subprocess, sys, tempfile, time

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
nonce = secrets.token_hex(16)
home = os.path.expanduser('~')
uid = os.getuid()
host_net = os.readlink('/proc/self/ns/net')
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
