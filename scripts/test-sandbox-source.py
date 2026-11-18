#!/usr/bin/env python3
"""Fail closed when a source snapshot would silently omit new inputs."""
import os
import subprocess
import sys
import tempfile

SOURCE_ROOTS = ('crates/', 'scripts/', 'tests/', 'src/', 'benches/', 'examples/', 'ui/', 'contrib/', 'docs/')
BUILD_INPUTS = {'Cargo.toml', 'Cargo.lock', 'build.rs', '.cargo/config', '.cargo/config.toml'}
RUNTIME_DIRS = {'target', 'node_modules', 'dist', '.vite', '.git', '.fl', '.cache', '.codex', '.venv'}

def git_files(root, *args):
    raw = subprocess.check_output(['git', '-C', root, 'ls-files', '-z', *args])
    return [os.fsdecode(p) for p in raw.split(b'\0') if p]

def missing_inputs(root):
    # All non-ignored untracked files could be test fixtures or build inputs.
    untracked = git_files(root, '--others', '--exclude-standard')
    # Ignored files in source/test trees may also be intentional inputs. Skip
    # known runtime/build directories without descending into their contents.
    ignored = git_files(root, '--others', '--ignored', '--exclude-standard', '--',
        'crates', 'scripts', 'tests', 'src', 'benches', 'examples', 'ui', 'contrib', 'docs', '.cargo',
        'Cargo.toml', 'Cargo.lock', 'build.rs',
        ':(exclude,glob)**/target/**', ':(exclude,glob)**/node_modules/**',
        ':(exclude,glob)**/dist/**', ':(exclude,glob)**/.vite/**',
        ':(exclude,glob)**/.git/**', ':(exclude,glob)**/.fl/**',
        ':(exclude,glob)**/.cache/**', ':(exclude,glob)**/.codex/**',
        ':(exclude,glob)**/.venv/**')
    relevant_ignored = [p for p in ignored
        if not (set(p.split('/')) & RUNTIME_DIRS)
        and (p in BUILD_INPUTS or p.startswith(SOURCE_ROOTS))]
    return sorted(set(untracked + relevant_ignored))

def self_test():
    with tempfile.TemporaryDirectory(prefix='sandbox-source-fixture-') as root:
        def git(*args):
            subprocess.run(['git', '-C', root, *args], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        def put(path, data):
            full = os.path.join(root, path)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, 'w') as f: f.write(data)
        git('init', '-q')
        put('.gitignore', 'target/\n*.generated.rs\n*.fls\n')
        git('add', '.gitignore')
        put('crates/example/src/new.rs', 'source')
        put('crates/example/target/runtime.fls', 'runtime')
        assert missing_inputs(root) == ['crates/example/src/new.rs']
        git('add', 'crates/example/src/new.rs')
        assert not missing_inputs(root)
        put('crates/example/src/hidden.generated.rs', 'source')
        assert missing_inputs(root) == ['crates/example/src/hidden.generated.rs']
        git('add', '-f', 'crates/example/src/hidden.generated.rs')
        assert not missing_inputs(root)
    print('PASS: test-owned Git source-input fixture')

if __name__ == '__main__':
    if sys.argv[1:] == ['--self-test']:
        self_test()
    elif len(sys.argv) == 3 and sys.argv[1] == '--check':
        missing = missing_inputs(sys.argv[2])
        if missing:
            print('test-sandboxed: source snapshot would omit untracked inputs:', file=sys.stderr)
            for path in missing: print('  ' + repr(path), file=sys.stderr)
            raise SystemExit('Stage intended source/test/build files (git add; use -f only after reviewing ignored files), or move unrelated files outside the checkout. Never stage secrets or runtime data. Ignored runtime data is never copied.')
    else:
        raise SystemExit('usage: test-sandbox-source.py --check ROOT | --self-test')
