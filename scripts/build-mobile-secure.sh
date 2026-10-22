#!/usr/bin/env bash
# Builds crates/secure-ffi for React Native and regenerates the bindings in packages/secure-native.
#
# ios:     Xcode, `rustup target add aarch64-apple-ios aarch64-apple-ios-sim`.
#          Writes packages/secure-native/build/VibefaceSecureFramework.xcframework.
# android: the Android NDK (ANDROID_NDK_HOME set to the app's `ndkVersion`), `cargo install cargo-ndk`,
#          `rustup target add aarch64-linux-android x86_64-linux-android`.
#          Writes packages/secure-native/android/src/main/jniLibs/<abi>/libvibeface_secure_ffi.a.
#
# Run `pnpm install` at the repository root first; the generated sources are committed, the
# compiled libraries are not.
set -euo pipefail

platform="${1:-}"
if [[ "$platform" != ios && "$platform" != android ]]; then
  printf 'Usage: %s ios|android\n' "$0" >&2
  exit 2
fi
if [[ "$platform" == android ]] && ! command -v cargo-ndk >/dev/null 2>&1; then
  printf 'Install cargo-ndk and the Android NDK first.\n' >&2
  exit 1
fi

cd "$(dirname "${BASH_SOURCE[0]}")/../packages/secure-native"
pnpm exec ubrn build "$platform" --and-generate --release --config ubrn.config.yaml
