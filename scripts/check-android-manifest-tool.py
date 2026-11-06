#!/usr/bin/env python3
"""Require an existing Android manifest reader; never install an SDK implicitly."""
import os
from pathlib import Path
import shutil
import subprocess

candidates = sorted((Path(os.environ.get('ANDROID_HOME', '/nonexistent')) / 'build-tools').glob('*/aapt'))
tool = shutil.which('aapt') or (str(candidates[-1]) if candidates else None)
if not tool or not os.access(tool, os.X_OK):
    raise SystemExit('An existing Android SDK build-tools aapt is required to validate product APKs')
subprocess.run([tool, 'version'], check=True)
