#!/usr/bin/env python3
"""Verify one same-directory SHA256 sidecar on every supported build host."""
import argparse
import hashlib
from pathlib import Path
import re
import tempfile


def verify(checksum):
    checksum = Path(checksum)
    if checksum.is_symlink() or not checksum.name.endswith('.sha256'):
        raise ValueError('Expected a regular .sha256 sidecar')
    text = checksum.read_bytes()
    match = re.fullmatch(rb'([0-9a-f]{64})  ([A-Za-z0-9._-]+)\n', text)
    if match is None or match[2].decode() != checksum.name.removesuffix('.sha256'):
        raise ValueError('Checksum must name exactly its sibling payload')
    payload = checksum.with_name(match[2].decode())
    if payload.is_symlink() or not payload.is_file():
        raise ValueError('Checksum payload must be a regular file')
    with payload.open('rb') as source:
        actual = hashlib.file_digest(source, 'sha256').hexdigest()
    if actual != match[1].decode():
        raise ValueError('Payload SHA256 differs from its sidecar')


def self_test():
    with tempfile.TemporaryDirectory(prefix='vibeface-checksum-') as directory:
        payload = Path(directory) / 'fixture.tar.gz'
        checksum = payload.with_name(payload.name + '.sha256')
        payload.write_bytes(b'abc')
        digest = 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'
        checksum.write_text(f'{digest}  {payload.name}\n')
        verify(checksum)
        for body in (f'{digest}  ../escape\n', f'{digest}  other.tar.gz\n',
                     f'{"0" * 64}  {payload.name}\n', f'{digest}  {payload.name}\nextra\n'):
            checksum.write_text(body)
            try:
                verify(checksum)
            except ValueError:
                continue
            raise AssertionError('Accepted an invalid or mismatched checksum')
    print('Portable checksum verification passed')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('checksum', type=Path, nargs='?')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    elif args.checksum:
        verify(args.checksum)
        print(f'{args.checksum.name}: verified')
    else:
        parser.error('name one .sha256 sidecar')
