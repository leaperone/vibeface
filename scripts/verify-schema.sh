#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
database=''
if (($#)); then
  if [[ $# != 2 || "$1" != --database || ! -f "$2" || -L "$2" ]]; then
    echo 'Usage: verify-schema.sh [--database EXISTING_SQLITE_DATABASE]' >&2
    exit 2
  fi
  database="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
fi
cd "$repo"
shopt -s nullglob
migrations=(crates/node/migrations/*.sql)
if [[ ${#migrations[@]} != 1 || "${migrations[0]:-}" != crates/node/migrations/202610180001_initial.sql ]]; then
  echo 'The fresh format requires exactly one initial Atlas SQL file.' >&2
  exit 1
fi
umask 077
mkdir -p "$repo/.artifacts"
scratch="$(mktemp -d "$repo/.artifacts/schema-check.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
atlas migrate validate --dir file://crates/node/migrations
atlas migrate apply --dir file://crates/node/migrations --url "sqlite://$scratch/node.sqlite3?_fk=1"
compare_schema() {
  local diff
  diff="$(atlas schema diff \
    --from "$1" \
    --to file://crates/node/schema.hcl \
    --dev-url 'sqlite://schema-check?mode=memory&_fk=1' \
    --exclude atlas_schema_revisions \
    --format '{{ sql . }}')"
  if [[ -n "$diff" ]]; then
    printf '%s differs from schema.hcl:\n%s\n' "$2" "$diff" >&2
    return 1
  fi
}
compare_schema "sqlite://$scratch/node.sqlite3?mode=ro&immutable=1&_fk=1" 'Atlas initial migration'
if [[ -n "$database" ]]; then
  python3 - "$database" "$scratch/existing.sqlite3" <<'PY'
import os
import shutil
import sqlite3
import stat
import sys
from pathlib import Path

source, snapshot = map(Path, sys.argv[1:])

def files():
    found = {}
    for suffix in ('', '-wal', '-journal'):
        path = Path(str(source) + suffix)
        try:
            info = path.lstat()
        except FileNotFoundError:
            if not suffix:
                raise
            continue
        if not stat.S_ISREG(info.st_mode):
            raise RuntimeError('database and sidecars must be regular files')
        found[suffix] = (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)
    return found

before = files()
for suffix in before:
    with os.fdopen(os.open(str(source) + suffix, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as original:
        if not stat.S_ISREG(os.fstat(original.fileno()).st_mode):
            raise RuntimeError('database and sidecars must be regular files')
        with Path(str(snapshot) + suffix).open('xb') as copied:
            shutil.copyfileobj(original, copied)
if files() != before:
    raise RuntimeError('database changed while copying; stop its writer before verification')
connection = sqlite3.connect(str(snapshot))
try:
    if connection.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone()[0] != 0:
        raise RuntimeError('could not read the complete database snapshot')
finally:
    connection.close()
PY
  compare_schema "sqlite://$scratch/existing.sqlite3?mode=ro&immutable=1&_fk=1" 'Rust-initialized database'
  printf 'Rust-initialized database matches schema.hcl.\n'
fi
printf 'Atlas initial migration replays from an empty SQLite database and matches schema.hcl.\n'
