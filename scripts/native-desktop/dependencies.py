#!/usr/bin/env python3
"""Inspect the actual linked dependencies of the native Linux desktop executable."""
import argparse
import json
from pathlib import Path
import re
import subprocess


def inspect(executable, target):
    linked = subprocess.check_output(['ldd', str(executable)], text=True)
    if 'not found' in linked:
        raise ValueError('Native GPUI executable has unresolved system libraries')
    libraries = sorted(set(re.findall(r'^\s*(\S+\.so(?:\.\d+)*)\s+=>', linked, re.M)))
    if not libraries:
        raise ValueError('No native shared-library dependencies were inspected')
    versions = subprocess.check_output(['readelf', '--version-info', str(executable)], text=True)
    glibc = sorted(set(re.findall(r'\bGLIBC_(\d+(?:\.\d+)+)', versions)), key=lambda value: tuple(map(int, value.split('.'))))
    if not glibc:
        raise ValueError('No glibc symbol requirements were inspected')
    return {'target': target, 'libraries': libraries, 'glibc_symbols': glibc, 'minimum_glibc': glibc[-1]}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--executable', required=True, type=Path)
    parser.add_argument('--target', required=True, choices=('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu'))
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    args.output.write_text(json.dumps(inspect(args.executable.resolve(), args.target), indent=2) + '\n')
