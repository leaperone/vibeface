#!/usr/bin/env python3
"""Read the wasm-bindgen CLI version required by this checkout's secure crate."""

import re
import tomllib
from pathlib import Path


with Path("crates/secure/Cargo.toml").open("rb") as manifest:
    wasm_target = tomllib.load(manifest)["target"]['cfg(target_arch = "wasm32")']
dependency = wasm_target["dependencies"]["wasm-bindgen"]
match = re.fullmatch(r"=([0-9]+\.[0-9]+\.[0-9]+)", dependency)
if not match:
    raise SystemExit("vibeface-secure must pin wasm-bindgen to an exact version")
print(match[1])
