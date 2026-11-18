#!/usr/bin/env python3
"""Fail closed when free space is low, including while sandbox children run."""
import os
import signal
import subprocess
import sys
import tempfile
import time

GIB = 1024 ** 3


def reserve_bytes(value):
    if not value.isascii() or not value.isdecimal() or len(value) > 9 or int(value) < 1:
        raise ValueError('FL_SANDBOX_MIN_FREE_GIB must be a positive integer GiB')
    return int(value) * GIB


def available(path):
    fs = os.statvfs(path)
    return fs.f_bavail * fs.f_frsize


def check_space(paths, reserve, measure=available):
    for path in paths:
        free = measure(path)
        if free < reserve:
            raise RuntimeError(f'{path} has {free} free bytes; reserve is {reserve} bytes')


def terminate_group(proc):
    try: os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError: pass
    try: proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try: os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError: pass
        proc.wait()


def supervise(paths, reserve, command, measure=available, interval=1):
    # The first check runs before starting even the bubblewrap process.
    check_space(paths, reserve, measure)
    proc = subprocess.Popen(command, close_fds=True, start_new_session=True)
    try:
        while True:
            check_space(paths, reserve, measure)
            status = proc.poll()
            if status is not None:
                # Sample again after exit, including for very short commands.
                check_space(paths, reserve, measure)
                return status if status >= 0 else 128 - status
            time.sleep(interval)
    finally:
        # --die-with-parent ends the bwrap sandbox if the leader is terminated.
        if proc.poll() is None:
            terminate_group(proc)


def self_test():
    reserve = reserve_bytes('120')
    for bad in ('', '0', '-1', '1.5', ' 1', '1000000000'):
        try: reserve_bytes(bad)
        except ValueError: pass
        else: raise AssertionError('invalid reserve accepted: ' + repr(bad))
    with tempfile.TemporaryDirectory(prefix='sandbox-space-fixture-') as root:
        marker = os.path.join(root, 'started')
        cmd = [sys.executable, '-c',
               'import pathlib,sys,time; pathlib.Path(sys.argv[1]).touch(); time.sleep(30)', marker]
        try: supervise(('/', '/target'), reserve, cmd, lambda _: reserve - 1, .01)
        except RuntimeError: pass
        else: raise AssertionError('preflight accepted low free space')
        assert not os.path.exists(marker), 'child started before failed preflight'
        def during(path):
            if os.path.exists(marker) and path == '/target': return reserve - 1
            return reserve + 1
        before = time.monotonic()
        try: supervise(('/', '/target'), reserve, cmd, during, .01)
        except RuntimeError: pass
        else: raise AssertionError('low space during run was accepted')
        assert os.path.exists(marker) and time.monotonic() - before < 10, 'child not stopped promptly'
        os.unlink(marker)
        child_ready = os.path.join(root, 'descendant-ready')
        child_stopped = os.path.join(root, 'descendant-stopped')
        descendant = ('import pathlib,signal,sys,time; '
                      'signal.signal(signal.SIGTERM, lambda *_: (pathlib.Path(sys.argv[2]).touch(), sys.exit(0))); '
                      'pathlib.Path(sys.argv[1]).touch(); time.sleep(30)')
        parent = ('import subprocess,sys,time; '
                  'subprocess.Popen([sys.executable, "-c", sys.argv[1], sys.argv[2], sys.argv[3]]); '
                  'time.sleep(30)')
        group_cmd = [sys.executable, '-c', parent, descendant, child_ready, child_stopped]
        def stop_after_child(_path):
            return reserve - 1 if os.path.exists(child_ready) else reserve + 1
        try: supervise(('/', '/target'), reserve, group_cmd, stop_after_child, .01)
        except RuntimeError: pass
        else: raise AssertionError('descendant group was not stopped')
        for _ in range(100):
            if os.path.exists(child_stopped): break
            time.sleep(.01)
        assert os.path.exists(child_stopped), 'descendant did not receive termination'
        for bad_path in ('/', '/target'):
            def failing(path):
                if path == bad_path: raise OSError('fixture measurement failure')
                return reserve + 1
            try: supervise(('/', '/target'), reserve, cmd, failing, .01)
            except OSError: pass
            else: raise AssertionError('measurement failure accepted')
        assert supervise(('/', '/target'), reserve, [sys.executable, '-c', 'pass'], lambda _: reserve + 1, .01) == 0
    print('PASS: test-owned free-space preflight and running-process fixtures')


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    if sys.argv[1:] == ['--self-test']:
        self_test()
        return 0
    if len(sys.argv) == 5 and sys.argv[1] == '--check':
        check_space((sys.argv[2], sys.argv[3]), reserve_bytes(sys.argv[4]))
        return 0
    if len(sys.argv) < 6 or sys.argv[4] != '--':
        raise ValueError('usage: test-sandbox-space.py ROOT TARGET MIN_FREE_GIB -- COMMAND...')
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, interrupted)
    return supervise((sys.argv[1], sys.argv[2]), reserve_bytes(sys.argv[3]), sys.argv[5:])


if __name__ == '__main__':
    try: sys.exit(main())
    except (OSError, ValueError, RuntimeError) as err:
        print('test-sandboxed: free-space check failed: ' + str(err), file=sys.stderr)
        sys.exit(1)
