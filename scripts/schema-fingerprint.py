#!/usr/bin/env python3
"""Check or refresh the authoritative schema digest and Atlas migration checksums."""
import argparse
import base64
import hashlib
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
CORE = ROOT / "apps/core"


def fingerprints():
    migrations = sorted((CORE / "migrations").glob("*.sql"))
    digest = hashlib.sha256()
    for path in sorted([*migrations, CORE / "src/delivery/schema.sql"]):
        body = path.read_bytes()
        name = path.relative_to(CORE).as_posix()
        digest.update(f"{name}\0{len(body)}\0".encode())
        digest.update(body)
    # Atlas sql/migrate.NewHashFile accumulates filename and contents, then
    # HashFile.Sum hashes each filename with its cumulative base64 checksum.
    cumulative = hashlib.sha256()
    entries = []
    for path in migrations:
        body = path.read_bytes()
        if b"atlas:sum" in body:
            raise ValueError("Atlas checksum directives require Atlas itself")
        cumulative.update(path.name.encode())
        cumulative.update(body)
        entries.append((path.name, base64.b64encode(cumulative.digest()).decode()))
    summary = hashlib.sha256()
    for name, checksum in entries:
        summary.update(name.encode())
        summary.update(checksum.encode())
    atlas = "h1:" + base64.b64encode(summary.digest()).decode() + "\n"
    atlas += "".join(f"{name} h1:{checksum}\n" for name, checksum in entries)
    return digest.hexdigest(), atlas


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    schema, atlas = fingerprints()
    config = CORE / "src/config.rs"
    body = config.read_text()
    updated, count = re.subn(
        r'(pub const DATA_EPOCH_SCHEMA_SHA256: &str =\s*\")[0-9a-f]{64}(\";)',
        lambda match: match[1] + schema + match[2],
        body,
    )
    if count != 1:
        raise SystemExit("Expected exactly one schema fingerprint declaration")
    checksums = CORE / "migrations/atlas.sum"
    if args.write:
        config.write_text(updated)
        checksums.write_text(atlas)
    elif body != updated or checksums.read_text() != atlas:
        raise SystemExit("Schema fingerprints differ; run scripts/schema-fingerprint.py --write")
    print(f"Schema fingerprint and Atlas checksum match: {schema}")


if __name__ == "__main__":
    main()
