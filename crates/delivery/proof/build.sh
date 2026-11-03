#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 || "$(pwd -P)" != /work ]]; then
  echo 'delivery proof builds require Linux x86_64 with the repository bind-mounted at /work' >&2
  exit 2
fi
if [[ "${CARGO_HOME:-}" != /cargo || "${RISC0_HOME:-}" != /risc0 ]]; then
  echo 'set CARGO_HOME=/cargo and RISC0_HOME=/risc0 to isolated task mounts' >&2
  exit 2
fi
if [[ -n "${RISC0_DEV_MODE+x}" || -n "${RISC0_SKIP_BUILD+x}" ]]; then
  echo 'RISC0_DEV_MODE and RISC0_SKIP_BUILD are forbidden' >&2
  exit 2
fi
export CARGO_TARGET_DIR=/artifacts/proof-target
export CARGO_BUILD_JOBS=2
export RISC0_BUILD_LOCKED=1
cargo +1.98.1 build --manifest-path crates/delivery/proof/Cargo.toml --release --locked -p vibeface-delivery-prover
"$CARGO_TARGET_DIR/release/vibeface-delivery-prover" info
