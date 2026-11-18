#!/usr/bin/env python3
"""Validate persistent target paths and reject unsafe target entries."""
import os
import socket
import stat
import sys
import tempfile

VAR_TMP = '/var/tmp'
DATA_ROOT = '/data/toolchain/ferroloop-build'


def check(directory):
    with os.scandir(directory) as entries:
        for entry in entries:
            info = entry.stat(follow_symlinks=False)
            if stat.S_ISDIR(info.st_mode): check(entry.path)
            elif not stat.S_ISREG(info.st_mode):
                raise RuntimeError('unsafe non-regular target entry: ' + entry.path)


def verify_root(root):
    info = os.lstat(root)
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise RuntimeError('target must be a 0700 directory owned by the current user: ' + root)
    if os.path.ismount(root):
        raise RuntimeError('target may not itself be a mount point: ' + root)


def verify(root):
    verify_root(root)
    check(root)


def persistent_path(path, create=False, var_tmp=VAR_TMP, data_root=DATA_ROOT):
    # Require canonical spelling; never normalize away traversal or empty components.
    if not path.startswith('/') or any(p in ('', '.', '..') for p in path[1:].split('/')):
        raise RuntimeError('target must be an absolute path without empty, dot or dot-dot components: ' + path)
    if path.startswith(var_tmp + '/') and len(path[len(var_tmp) + 1:].split('/')) == 1:
        root = var_tmp
    elif path.startswith(data_root + '/') and len(path[len(data_root) + 1:].split('/')) == 2:
        root = data_root
    else:
        raise RuntimeError('target must be /var/tmp/<task> or /data/toolchain/ferroloop-build/<persona>/<task>: ' + path)

    # Check every component by lstat, including the allowlisted root. The common
    # directory must already exist; only a persona directory and target may be created.
    current = '/'
    root_parts = root[1:].split('/')
    parts = path[1:].split('/')
    for index, part in enumerate(parts):
        current = os.path.join(current, part)
        try:
            info = os.lstat(current)
        except FileNotFoundError:
            if not create or index < len(root_parts):
                raise RuntimeError('target parent does not exist: ' + current)
            os.mkdir(current, 0o700)
            info = os.lstat(current)
        if not stat.S_ISDIR(info.st_mode):
            raise RuntimeError('target path component must be a directory, not a symlink: ' + current)
    return path


def self_test():
    def rejects(fn, description):
        try: fn()
        except (RuntimeError, OSError): return
        raise AssertionError(description + ' was not rejected')

    with tempfile.TemporaryDirectory(prefix='sandbox-target-fixture-') as fixture:
        var_tmp = os.path.join(fixture, 'var', 'tmp')
        data_root = os.path.join(fixture, 'data', 'toolchain', 'ferroloop-build')
        os.makedirs(var_tmp)
        os.makedirs(data_root)
        def path(value, create=False): return persistent_path(value, create, var_tmp, data_root)
        legacy = os.path.join(var_tmp, 'task')
        data = os.path.join(data_root, 'felix', 'task')
        for candidate in (legacy, data):
            path(candidate, True)
            path(candidate)
            verify(candidate)
        for candidate in (var_tmp, os.path.join(var_tmp, 'a', 'b'), data_root,
                          os.path.join(data_root, 'felix'), os.path.join(data, 'extra'),
                          os.path.join(data_root, '..', 'elsewhere', 'task'),
                          data_root + '//task', data_root + '/felix/./task',
                          data_root + '/felix/task/', 'relative/path'):
            rejects(lambda: path(candidate, True), 'unsafe depth or spelling ' + candidate)
        os.symlink(os.path.join(data_root, 'felix'), os.path.join(data_root, 'link'))
        rejects(lambda: path(os.path.join(data_root, 'link', 'task')), 'symlink persona')
        os.symlink(data, os.path.join(data_root, 'felix', 'link'))
        rejects(lambda: path(os.path.join(data_root, 'felix', 'link')), 'symlink target')
        os.symlink(os.path.join(fixture, 'data'), os.path.join(fixture, 'alias'))
        rejects(lambda: persistent_path(os.path.join(fixture, 'alias', 'toolchain', 'ferroloop-build', 'felix', 'task'),
                                      False, var_tmp, os.path.join(fixture, 'alias', 'toolchain', 'ferroloop-build')),
                'symlink root component')
        link = os.path.join(data, 'link')
        os.symlink('/not-used', link)
        rejects(lambda: verify(data), 'target entry symlink')
        os.unlink(link)
        with socket.socket(socket.AF_UNIX) as sock:
            sock.bind(os.path.join(data, 'socket'))
            rejects(lambda: verify(data), 'target entry socket')
        os.unlink(os.path.join(data, 'socket'))
        os.chmod(data, 0o755)
        rejects(lambda: verify(data), 'non-private target')
    print('PASS: test-owned target path and unsafe-entry fixtures')


if sys.argv[1:] == ['--self-test']:
    self_test()
elif len(sys.argv) == 3 and sys.argv[1] in ('--prepare', '--check-path', '--verify-persistent'):
    target = persistent_path(sys.argv[2], create=sys.argv[1] == '--prepare')
    if sys.argv[1] == '--prepare': verify_root(target)
    elif sys.argv[1] == '--verify-persistent': verify(target)
elif len(sys.argv) == 2:
    verify(sys.argv[1])
else:
    raise SystemExit('usage: test-sandbox-target.py DIRECTORY | --prepare DIRECTORY | --verify-persistent DIRECTORY | --self-test')
