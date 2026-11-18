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


def child_exited(proc):
    # Observe without reaping: the child PID must remain reserved until every
    # signal to its process group has been sent.
    return os.waitid(os.P_PID, proc.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is not None


def terminate_group(proc):
    try: os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError: pass
    deadline = time.monotonic() + 5
    while not child_exited(proc) and time.monotonic() < deadline:
        time.sleep(.05)
    # Keep the group leader waitable and unreaped even if it already exited.
    # An ignoring descendant can remain after the leader receives SIGTERM.
    try: os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError: pass
    proc.wait()


def supervise(paths, reserve, command, measure=available, interval=1):
    # The first check runs before starting even the bubblewrap process.
    check_space(paths, reserve, measure)
    proc = subprocess.Popen(command, close_fds=True, start_new_session=True)
    completed = False
    try:
        while True:
            check_space(paths, reserve, measure)
            if child_exited(proc):
                # Sample again before reaping, including for very short commands.
                check_space(paths, reserve, measure)
                # A signal between wait() and setting completed would otherwise
                # enter cleanup after the leader PID was already released.
                mask = {signal.SIGINT, signal.SIGTERM, signal.SIGHUP}
                previous = signal.pthread_sigmask(signal.SIG_BLOCK, mask)
                try:
                    status = proc.wait()
                    completed = True
                finally:
                    signal.pthread_sigmask(signal.SIG_SETMASK, previous)
                return status if status >= 0 else 128 - status
            time.sleep(interval)
    finally:
        # --die-with-parent ends the bwrap sandbox if the leader is terminated.
        if not completed:
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
        stubborn_ready = os.path.join(root, 'stubborn-ready')
        survived = os.path.join(root, 'stubborn-survived')
        stubborn = ('import pathlib,signal,sys,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); '
                    'pathlib.Path(sys.argv[1]).touch(); time.sleep(2); pathlib.Path(sys.argv[2]).touch()')
        stubborn_cmd = [sys.executable, '-c', parent, stubborn, stubborn_ready, survived]
        def stop_stubborn(_path):
            return reserve - 1 if os.path.exists(stubborn_ready) else reserve + 1
        try: supervise(('/', '/target'), reserve, stubborn_cmd, stop_stubborn, .01)
        except RuntimeError: pass
        else: raise AssertionError('stubborn descendant group was not stopped')
        time.sleep(2.2)
        assert not os.path.exists(survived), 'SIGTERM-ignoring descendant survived low-space abort'
        for bad_path in ('/', '/target'):
            def failing(path):
                if path == bad_path: raise OSError('fixture measurement failure')
                return reserve + 1
            try: supervise(('/', '/target'), reserve, cmd, failing, .01)
            except OSError: pass
            else: raise AssertionError('measurement failure accepted')
        assert supervise(('/', '/target'), reserve, [sys.executable, '-c', 'pass'], lambda _: reserve + 1, .01) == 0
        # Exercise the exit-before-cleanup race. At each signal the already
        # exited leader must still be our waitable child; only then may wait()
        # reap it. The wrapper detects any additional signal after reaping.
        exited = subprocess.Popen([sys.executable, '-c', 'pass'], start_new_session=True)
        deadline = time.monotonic() + 5
        while not child_exited(exited):
            assert time.monotonic() < deadline, 'owned child did not exit'
            time.sleep(.01)
        signals = []
        actual_killpg = os.killpg
        def checked_killpg(pgid, sig):
            assert exited.returncode is None, 'group signal attempted after leader was reaped'
            assert child_exited(exited), 'owned leader unexpectedly not waitable'
            signals.append(sig)
            return actual_killpg(pgid, sig)
        os.killpg = checked_killpg
        try: terminate_group(exited)
        finally: os.killpg = actual_killpg
        assert signals == [signal.SIGTERM, signal.SIGKILL]
        assert exited.returncode == 0, 'leader not reaped after group signals'
    print('PASS: test-owned free-space preflight and running-process fixtures')


def bwrap_self_test():
    # An owned delayed write proves whether the real bwrap sandbox survived.
    # The unsupervised control must write it; on abort the same command must not.
    with tempfile.TemporaryDirectory(prefix='sandbox-space-bwrap-fixture-') as root:
        ready = os.path.join(root, 'ready')
        survived = os.path.join(root, 'survived')
        child = ('import pathlib,signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); '
                 'pathlib.Path("/fixture/ready").touch(); time.sleep(2); '
                 'pathlib.Path("/fixture/survived").touch()')
        parent = ('import subprocess,sys; '
                  'sys.exit(subprocess.Popen([sys.executable, "-c", sys.argv[1]]).wait())')
        cmd = ['bwrap', '--unshare-user', '--unshare-pid', '--unshare-net',
               '--die-with-parent', '--new-session',
               '--ro-bind', '/usr/bin', '/usr/bin', '--ro-bind', '/usr/lib', '/usr/lib',
               '--ro-bind', '/usr/lib64', '/usr/lib64', '--symlink', 'usr/lib', '/lib',
               '--symlink', 'usr/lib64', '/lib64', '--proc', '/proc', '--dev', '/dev',
               '--bind', root, '/fixture', '--', '/usr/bin/python3', '-c', parent, child]
        subprocess.run(cmd, check=True, timeout=15, close_fds=True)
        assert os.path.exists(ready) and os.path.exists(survived), 'owned bwrap control did not complete'
        os.unlink(ready)
        os.unlink(survived)
        reserve = reserve_bytes('120')
        def measured(_path):
            return reserve - 1 if os.path.exists(ready) else reserve + 1
        try: supervise(('/', '/target'), reserve, cmd, measured, .01)
        except RuntimeError: pass
        else: raise AssertionError('bwrap low-space abort did not happen')
        assert os.path.exists(ready), 'bwrap fixture did not start'
        time.sleep(2.2)
        assert not os.path.exists(survived), 'bwrap child survived low-space abort'
    print('PASS: owned bwrap low-space abort and positive control')


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    if sys.argv[1:] == ['--self-test']:
        self_test()
        return 0
    if sys.argv[1:] == ['--bwrap-self-test']:
        bwrap_self_test()
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
