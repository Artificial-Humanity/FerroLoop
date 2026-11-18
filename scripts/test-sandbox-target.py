#!/usr/bin/env python3
"""Reject non-private or unsafe persistent target directory entries."""
import os
import socket
import stat
import sys
import tempfile

def check(directory):
    with os.scandir(directory) as entries:
        for entry in entries:
            info = entry.stat(follow_symlinks=False)
            if stat.S_ISDIR(info.st_mode): check(entry.path)
            elif not stat.S_ISREG(info.st_mode):
                raise RuntimeError('unsafe non-regular target entry: ' + entry.path)

def verify(root):
    info = os.lstat(root)
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise RuntimeError('target must be a 0700 directory owned by the current user: ' + root)
    if os.path.ismount(root):
        raise RuntimeError('target may not itself be a mount point: ' + root)
    check(root)

if sys.argv[1:] == ['--self-test']:
    with tempfile.TemporaryDirectory(prefix='sandbox-target-fixture-') as root:
        verify(root)
        link = os.path.join(root, 'link')
        os.symlink('/not-used', link)
        try: verify(root)
        except RuntimeError: pass
        else: raise SystemExit('symlink was not rejected')
        os.unlink(link)
        with socket.socket(socket.AF_UNIX) as sock:
            sock.bind(os.path.join(root, 'socket'))
            try: verify(root)
            except RuntimeError: pass
            else: raise SystemExit('socket was not rejected')
    print('PASS: test-owned unsafe target fixture')
elif len(sys.argv) == 2:
    verify(sys.argv[1])
else:
    raise SystemExit('usage: test-sandbox-target.py DIRECTORY | --self-test')
