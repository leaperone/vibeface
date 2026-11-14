#!/usr/bin/env bash
set -euo pipefail
[[ $# == 2 ]] || { echo 'Usage: sign-release.sh core|cli NATIVE_OUTPUT_DIRECTORY' >&2; exit 2; }
keychain="$RUNNER_TEMP/vibeface-signing.keychain-db"
trap 'security delete-keychain "$keychain" >/dev/null 2>&1 || true; rm -f "$RUNNER_TEMP/vibeface-signing.p12" "$RUNNER_TEMP/vibeface-notary.p8"' EXIT
app="$1"
directory="$2"
[[ "$app" == core || "$app" == cli ]] || { echo 'Expected core or cli.' >&2; exit 1; }
[[ -d "$directory" ]] || { echo "Native output directory does not exist: $directory" >&2; exit 1; }
GITHUB_ENV="${GITHUB_ENV:-/dev/null}" source "$(dirname "${BASH_SOURCE[0]}")/import-mac-identity.sh"
if [[ "$app" == core ]]; then
  binaries=("$directory/vibeface-core")
else
  binaries=("$directory/vibeface")
fi
notary_inputs=$(mktemp -d "$RUNNER_TEMP/vibeface-notary-$app.XXXXXX")
for binary in "${binaries[@]}"; do
  codesign --force --options runtime --timestamp --identifier cn.leaperone.vibeface --sign "$identity" "$binary"
  codesign --verify --strict "$binary"
  cp "$binary" "$notary_inputs/"
done
ditto -c -k --keepParent "$notary_inputs" "$RUNNER_TEMP/native-$app.zip"
xcrun notarytool submit "$RUNNER_TEMP/native-$app.zip" --key "$RUNNER_TEMP/vibeface-notary.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" --wait --timeout 30m --output-format json > "$RUNNER_TEMP/notary-result-$app.json"
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["status"] == "Accepted"' "$RUNNER_TEMP/notary-result-$app.json"
