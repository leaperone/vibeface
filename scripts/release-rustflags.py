#!/usr/bin/env python3
"""Keep build-machine paths out of released Rust binaries."""

import os
from pathlib import Path
import subprocess


root = Path(__file__).resolve().parent.parent
encoded = os.environ.get("CARGO_ENCODED_RUSTFLAGS")
flags = encoded.split("\x1f") if encoded is not None else os.environ.get("RUSTFLAGS", "").split()
flags = [flag for flag in flags if flag]
paths = [
    (Path.home(), "/build/home"),
    (os.environ.get("CARGO_HOME", Path.home() / ".cargo"), "/cargo"),
    (os.environ.get("RUSTUP_HOME", Path.home() / ".rustup"), "/rustup"),
    (subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip(), "/rust"),
    (root, "/vibeface"),
    (os.environ.get("CARGO_TARGET_DIR", root / "target"), "/target"),
]
flags.extend(f"--remap-path-prefix={Path(path).resolve()}={mapped}" for path, mapped in paths)
print("\x1f".join(flags))
