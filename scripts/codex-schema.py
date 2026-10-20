#!/usr/bin/env python3
"""Regenerate crates/bridge/schema/app-server-methods.json from the installed Codex CLI.

The Rust coverage test compares this file with the bridge method table and
docs/codex-app-server.md, so a Codex upgrade that adds a method fails CI until
the method is classified.
"""
import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "crates/bridge/schema/app-server-methods.json"
UNIONS = {
    "client_requests": "ClientRequest.json",
    "client_notifications": "ClientNotification.json",
    "server_requests": "ServerRequest.json",
    "server_notifications": "ServerNotification.json",
}


def methods(path):
    names = []
    for variant in json.loads(path.read_text())["oneOf"]:
        method = variant["properties"]["method"]
        names.extend(method.get("enum") or [method["const"]])
    return names


def generate(codex):
    version = subprocess.run([codex, "--version"], capture_output=True, text=True, check=True).stdout.split()[-1]
    with tempfile.TemporaryDirectory() as directory:
        subprocess.run([codex, "app-server", "generate-json-schema", "--out", directory],
                       check=True, capture_output=True)
        table = {key: methods(Path(directory) / name) for key, name in UNIONS.items()}
    return {"codex_version": version, **table}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", default="codex", help="Codex CLI executable")
    parser.add_argument("--check", action="store_true", help="fail if the committed file differs")
    options = parser.parse_args()
    generated = json.dumps(generate(options.codex), indent=2) + "\n"
    if options.check:
        if OUTPUT.read_text() != generated:
            sys.exit("app-server-methods.json is stale; rerun scripts/codex-schema.py")
        return
    OUTPUT.write_text(generated)
    print(f"wrote {OUTPUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
