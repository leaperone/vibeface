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
  compare_schema "sqlite://$database?mode=ro&immutable=1&_fk=1" 'Rust-initialized database'
  printf 'Rust-initialized database matches schema.hcl.\n'
fi
printf 'Atlas initial migration replays from an empty SQLite database and matches schema.hcl.\n'
