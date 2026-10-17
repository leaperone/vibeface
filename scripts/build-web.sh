#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo/crates/node/web"
if ! command -v pnpm >/dev/null; then
  echo 'pnpm is required to build the web client. Install pnpm 11.9.0.' >&2
  exit 1
fi
pnpm install --frozen-lockfile
pnpm run build
[[ -f dist/client/_shell.html ]] || { echo 'SPA shell was not written to dist/client/_shell.html' >&2; exit 1; }
