#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
out="${1:-$root/crates/node/web/generated}"
bindgen="${WASM_BINDGEN:-wasm-bindgen}"
version=0.2.129
if ! command -v "$bindgen" >/dev/null 2>&1; then
  printf 'Install wasm-bindgen-cli %s or set WASM_BINDGEN.\n' "$version" >&2
  exit 1
fi
if [[ "$("$bindgen" --version)" != "wasm-bindgen $version" ]]; then
  printf 'Expected wasm-bindgen-cli %s.\n' "$version" >&2
  exit 1
fi
cargo build --package vibeface-secure --lib --locked --release --target wasm32-unknown-unknown
metadata="$(cargo metadata --no-deps --format-version 1 --locked)"
target="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' <<< "$metadata")"
mkdir -p "$out"
"$bindgen" "$target/wasm32-unknown-unknown/release/vibeface_secure.wasm" \
  --target web --out-dir "$out" --no-typescript
