#!/usr/bin/env python3
"""Create a native portable ZIP without requiring a POSIX shell on Windows."""
from pathlib import Path
import sys
import zipfile
source, output = map(Path, sys.argv[1:])
with zipfile.ZipFile(output, 'x', compression=zipfile.ZIP_DEFLATED) as archive:
    for file in sorted(source.rglob('*')):
        if file.is_file():
            archive.write(file, file.relative_to(source).as_posix())
