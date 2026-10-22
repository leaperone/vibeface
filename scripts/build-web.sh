#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo/crates/node/web"
for asset in vibeface_secure.js vibeface_secure_bg.wasm vibeface_secure.d.ts; do
  [[ -s "generated/$asset" ]] || { echo 'Run scripts/build-web-crypto.sh before building the web client.' >&2; exit 1; }
done
if ! command -v pnpm >/dev/null; then
  echo 'pnpm 11.9.0 is required to build the web client.' >&2
  exit 1
fi
pnpm install --frozen-lockfile --filter "vibeface-web..." --filter "vibeface-mobile..."
pnpm run generate-routes
pnpm run test
pnpm run typecheck
pnpm --filter @vibeface/client run test
pnpm --filter @vibeface/client run typecheck
pnpm --filter @vibeface/secure-native run typecheck
pnpm --filter vibeface-mobile run typecheck
pnpm run build
[[ -s dist/client/_shell.html ]] || { echo 'SPA shell was not written to dist/client/_shell.html' >&2; exit 1; }
