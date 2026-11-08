#!/usr/bin/env python3
"""Reject standalone Windows programs that need an unbundled VC++ runtime."""
import argparse
from pathlib import Path
import re
import shutil
import subprocess


def verify(binary):
    binary = Path(binary)
    if not binary.is_file() or binary.is_symlink():
        raise ValueError('Expected a regular packaged Windows executable')
    dumpbin = shutil.which('dumpbin.exe')
    if dumpbin:
        output = subprocess.check_output([dumpbin, '/DEPENDENTS', str(binary)], encoding='utf-8', errors='replace')
        libraries = re.findall(r'^\s+([A-Za-z0-9_.-]+\.dll)\s*$', output, re.M | re.I)
    else:
        objdump = shutil.which('objdump')
        if not objdump:
            raise ValueError('The existing MSVC dumpbin or objdump tool is required to inspect PE imports')
        output = subprocess.check_output([objdump, '-p', str(binary)], encoding='utf-8', errors='replace')
        libraries = re.findall(r'DLL Name:\s*([A-Za-z0-9_.-]+\.dll)', output, re.I)
    if not libraries:
        raise ValueError('The packaged program has no readable PE import table')
    forbidden = sorted({name for name in libraries if name.lower().startswith(('vcruntime', 'msvcp', 'concrt'))})
    if forbidden:
        raise ValueError(f'{binary.name} needs an unbundled Visual C++ runtime: {", ".join(forbidden)}')
    print(f'{binary.name}: no dynamic Visual C++ redistributable dependency')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binaries', type=Path, nargs='+')
    args = parser.parse_args()
    try:
        for binary in args.binaries:
            verify(binary)
    except (ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, str(error) + '\n')
