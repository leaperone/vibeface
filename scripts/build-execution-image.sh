#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
output="$repo/.artifacts/execution-image"
if (($#)); then
  if [[ $# != 2 || "$1" != --output ]]; then
    echo 'Usage: build-execution-image.sh [--output DIRECTORY]' >&2
    exit 2
  fi
  output="$2"
fi
umask 077
mkdir -p "$output"
output="$(cd "$output" && pwd)"
scratch="$(mktemp -d "$output/.build.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/context"
sdk="$repo/crates/node/sdk"
if [[ ! -d "$sdk" ]]; then sdk="$repo/sidecars"; fi
mkdir "$scratch/context/sidecars"
for source in package.json package-lock.json codex-manager.mjs; do
  [[ -f "$sdk/$source" ]] || { printf 'SDK build source is missing: %s\n' "$source" >&2; exit 1; }
  cp "$sdk/$source" "$scratch/context/sidecars/"
done
source_commit=unknown
if [[ -f "$repo/BUILD-INFO.json" ]]; then
  recorded="$(sed -n 's/.*"source_commit":"\([a-f0-9]\{40\}\)".*/\1/p' "$repo/BUILD-INFO.json")"
  if [[ -n "$recorded" ]]; then source_commit="$recorded"; fi
elif command -v git >/dev/null && [[ "$(git -C "$repo" rev-parse --show-toplevel 2>/dev/null || true)" == "$repo" ]]; then
  if [[ -z "$(git -C "$repo" status --porcelain --untracked-files=normal)" ]]; then
    source_commit="$(git -C "$repo" rev-parse HEAD)"
  fi
fi
docker build --file "$repo/scripts/execution/Dockerfile" \
  --label "org.opencontainers.image.revision=$source_commit" \
  --iidfile "$scratch/image.id" "$scratch/context"
image="$(cat "$scratch/image.id")"
[[ "$image" =~ ^sha256:[a-f0-9]{64}$ ]] || { echo 'Docker did not return an immutable image ID.' >&2; exit 1; }
mv "$scratch/image.id" "$output/image.id"
printf '{"image_id":"%s","source_commit":"%s","codex_version":"0.163.0","claude_version":"2.1.296","codex_sdk_version":"0.163.0"}\n' \
  "$image" "$source_commit" > "$output/BUILD-INFO.json"
printf 'Execution image: %s\nBuild information: %s\n' "$image" "$output/BUILD-INFO.json"
