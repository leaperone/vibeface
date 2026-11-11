#!/usr/bin/env bash
set -euo pipefail
keychain="$RUNNER_TEMP/vibeface-signing.keychain-db"
trap 'security delete-keychain "$keychain" >/dev/null 2>&1 || true; rm -f "$RUNNER_TEMP/vibeface-signing.p12" "$RUNNER_TEMP/vibeface-notary.p8"' EXIT
GITHUB_ENV="${GITHUB_ENV:-/dev/null}" source "$(dirname "${BASH_SOURCE[0]}")/import-mac-identity.sh"
[[ "$1" == product ]] || { echo 'Expected the product native programs.' >&2; exit 1; }
binaries=("$2/vibeface" "$2/vibeface-core")
notary_inputs=$(mktemp -d "$RUNNER_TEMP/vibeface-notary-product.XXXXXX")
for binary in "${binaries[@]}"; do
  codesign --force --options runtime --timestamp --identifier cn.leaperone.vibeface --sign "$identity" "$binary"
  codesign --verify --strict "$binary"
  cp "$binary" "$notary_inputs/"
done
ditto -c -k --keepParent "$notary_inputs" "$RUNNER_TEMP/native.zip"
xcrun notarytool submit "$RUNNER_TEMP/native.zip" --key "$RUNNER_TEMP/vibeface-notary.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" --wait --timeout 30m --output-format json > "$RUNNER_TEMP/notary-result.json"
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["status"] == "Accepted"' "$RUNNER_TEMP/notary-result.json"
