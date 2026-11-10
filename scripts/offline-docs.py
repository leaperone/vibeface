#!/usr/bin/env python3
"""Assemble the same tracked documentation bundle for every runtime packager."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
from urllib.parse import quote, unquote, urlsplit

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / 'scripts/offline-docs.json'
LINK = re.compile(r'(\]\()([^\s)]+)([^)]*\))')


def sources():
    manifest = json.loads(MANIFEST.read_text())
    tracked = set(subprocess.check_output(['git', 'ls-files', '-z'], cwd=ROOT).decode().split('\0'))
    result = {name: destination for name, destination in manifest['files'].items()}
    for prefix, destination in manifest['trees'].items():
        for name in sorted(tracked):
            if name.startswith(prefix + '/'):
                result[name] = destination + name[len(prefix):]
    for name in result:
        source = ROOT / name
        if name not in tracked or source.is_symlink() or not source.is_file():
            raise ValueError(f'Documentation must be a regular tracked file: {name}')
    return result


def assemble(destination):
    mapping = sources()
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    for name, output in mapping.items():
        source, target = ROOT / name, destination / output
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.suffix != '.md':
            shutil.copyfile(source, target)
            continue
        def rewrite(match):
            link = match[2]
            parts = urlsplit(link)
            if parts.scheme or parts.netloc or not parts.path or link.startswith('#'):
                return match[0]
            resolved = Path(os.path.normpath(str(Path(name).parent / unquote(parts.path)))).as_posix()
            if resolved in mapping:
                rewritten = quote(os.path.relpath(mapping[resolved], Path(output).parent), safe='/.-_')
            else:
                # Source and agent guidance are online references, pinned to this exact source.
                rewritten = f'https://github.com/leaperone/vibeface/blob/{commit}/{quote(resolved, safe="/.-_")}'
            if parts.query:
                rewritten += '?' + parts.query
            if parts.fragment:
                rewritten += '#' + parts.fragment
            return match[1] + rewritten + match[3]
        target.write_text(LINK.sub(rewrite, source.read_text(encoding='utf-8')), encoding='utf-8', newline='\n')
    verify(destination)
    return mapping


def verify(destination):
    if not destination.is_dir():
        raise ValueError('Assembled documentation directory does not exist')
    required = sources().values()
    absent = [name for name in required if not (destination / name).is_file()]
    if absent:
        raise ValueError('Missing assembled documentation files: ' + ', '.join(absent))
    missing = []
    for source in destination.rglob('*.md'):
        for match in LINK.finditer(source.read_text(encoding='utf-8')):
            parts = urlsplit(match[2])
            if parts.scheme or parts.netloc or not parts.path:
                continue
            target = (source.parent / unquote(parts.path)).resolve()
            if not target.is_relative_to(destination.resolve()) or not target.exists():
                missing.append(f'{source.relative_to(destination)}: {match[2]}')
    if missing:
        raise ValueError('Broken assembled offline documentation links:\n' + '\n'.join(missing))


def self_test():
    with tempfile.TemporaryDirectory(prefix='vibeface-docs-') as temporary:
        destination = Path(temporary)
        try:
            verify(destination)
        except ValueError as error:
            assert 'Missing assembled documentation files' in str(error)
        else:
            raise AssertionError('empty documentation bundle accepted')
        mapping = assemble(destination)
        assert len(mapping) > 25 and (destination / 'docs/node.md').is_file()
        assert 'packages/docs/' not in (destination / 'README.md').read_text()
        (destination / 'docs/broken.md').write_text('[missing](missing.md)\n')
        try:
            verify(destination)
        except ValueError as error:
            assert 'missing.md' in str(error)
        else:
            raise AssertionError('missing assembled link accepted')
        (destination / 'docs/broken.md').unlink()
        (destination / 'docs/quick-start.md').unlink()
        try:
            verify(destination)
        except ValueError as error:
            assert 'quick-start.md' in str(error)
        else:
            raise AssertionError('incomplete documentation bundle accepted')
    print('Offline docs self-test passed: all packagers share one manifest and assembled link closure')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('destination', nargs='?', type=Path)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--verify', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    elif args.destination:
        verify(args.destination) if args.verify else assemble(args.destination)
    else:
        parser.error('destination or --self-test is required')
