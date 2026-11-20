#!/usr/bin/env python3
"""Reject mount points below a bind root, decoding Linux mountinfo paths."""
import os
import re
import sys
import tempfile

ESCAPE = re.compile(r'\\([0-7]{3})')

def mountpoints(lines):
    result = []
    for line in lines:
        fields = line.split(' - ', 1)[0].split()
        if len(fields) < 6 or ' - ' not in line:
            raise ValueError('malformed mountinfo')
        path = ESCAPE.sub(lambda m: chr(int(m.group(1), 8)), fields[4])
        if not path.startswith('/'):
            raise ValueError('non-absolute mountpoint')
        result.append(os.path.normpath(path))
    return result

def nested(roots, points):
    for root in roots:
        root = os.path.normpath(os.path.abspath(root))
        for point in points:
            if point != root and os.path.commonpath((root, point)) == root:
                return point
    return None

def main():
    if sys.argv[1:] == ['--self-test']:
        with tempfile.TemporaryDirectory(prefix='mount fixture ') as directory:
            root = os.path.join(directory, 'root with spaces')
            child = os.path.join(root, 'child with spaces')
            escape = lambda p: p.replace('\\', r'\134').replace(' ', r'\040').replace('\t', r'\011').replace('\n', r'\012')
            lines = [f'1 0 0:1 / {escape(child)} rw - tmpfs tmpfs rw\n']
            assert nested([root], mountpoints(lines)) == child
            assert nested([root + '-other'], mountpoints(lines)) is None
            print('PASS: escaped test-owned mountinfo fixture')
        return
    if len(sys.argv) < 3 or sys.argv[1] != '--check':
        raise SystemExit('usage: test-sandbox-mounts.py --check ROOT... | --self-test')
    with open('/proc/self/mountinfo', encoding='utf-8', errors='surrogateescape') as f:
        point = nested(sys.argv[2:], mountpoints(f))
    if point:
        raise SystemExit('nested mount in bind or source tree: ' + point)

if __name__ == '__main__':
    main()
