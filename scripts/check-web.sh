#!/usr/bin/env bash
set -euo pipefail

# The caller installs workspace dependencies and verifies the crypto inputs.
# Keep this entry check-only: it must not install or package browser assets.
repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo/apps/app"
pnpm run generate-routes
pnpm run test
pnpm run typecheck
pnpm --filter @vibeface/client run test
pnpm --filter @vibeface/client run typecheck
