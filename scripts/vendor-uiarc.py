#!/usr/bin/env python3
"""Copy a pinned Arc source dependency closure and record every file's origin."""

import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
COMMIT = "859606819c68787b2dccb54adcb9b08bd3272d5d"
REPOSITORY = "https://github.com/WheatXie/uiarc-pro-reverse"
IMPORT = re.compile(r"(?:from\s*|import\s*)[\"'](\.[^\"']+)[\"']")


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="Local checkout of the pinned upstream")
    parser.add_argument("items", nargs="+", help="Arc catalogue ids to acquire")
    args = parser.parse_args()
    source = args.source.resolve()
    actual = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if actual != COMMIT:
        parser.error(f"Expected upstream {COMMIT}, found {actual}")
    library = source / "src/components/arc"
    destination = ROOT / "packages/uiarc/src"
    manifest_file = ROOT / "packages/uiarc/source.json"
    manifest = json.loads(manifest_file.read_text()) if manifest_file.exists() else {
        "repository": REPOSITORY, "commit": COMMIT, "items": [], "files": {},
    }
    if manifest["commit"] != COMMIT:
        parser.error("Existing vendored files are from a different upstream revision")
    pending = [library / "foundation.css", library / "motion-tokens.ts"]
    for item in args.items:
        if not re.fullmatch(r"[a-z][a-z0-9-]*", item):
            parser.error(f"Invalid catalogue id: {item}")
        entry = library / item / (item + ".tsx")
        if not entry.is_file():
            parser.error(f"Missing Arc item: {entry}")
        pending.append(entry)
    seen = set()
    while pending:
        path = pending.pop().resolve()
        if not path.is_relative_to(library) or "demo" in path.name:
            parser.error(f"Import escapes the component library: {path}")
        if path in seen:
            continue
        seen.add(path)
        data = path.read_bytes()
        relative = path.relative_to(library)
        target = destination / relative
        if target.exists() and relative.as_posix() in manifest["files"]:
            expected = manifest["files"][relative.as_posix()]["sha256"]
            if sha256(target.read_bytes()) != expected:
                parser.error(f"Refusing to replace locally modified source: {target}")
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        manifest["files"][relative.as_posix()] = {
            "path": "src/components/arc/" + relative.as_posix(), "sha256": sha256(data),
        }
        if path.suffix in {".ts", ".tsx"}:
            for specifier in IMPORT.findall(data.decode()):
                dependency = path.parent / specifier
                candidates = [dependency, Path(str(dependency) + ".ts"), Path(str(dependency) + ".tsx"), dependency / "index.ts"]
                resolved = next((candidate for candidate in candidates if candidate.is_file()), None)
                if resolved is None:
                    parser.error(f"Unresolved upstream import {specifier} in {path}")
                pending.append(resolved)
    manifest["items"] = sorted(set(manifest["items"]) | set(args.items))
    manifest["files"] = dict(sorted(manifest["files"].items()))
    manifest_file.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Acquired {len(args.items)} Arc items, {len(seen)} source files at {COMMIT}")


if __name__ == "__main__":
    main()
