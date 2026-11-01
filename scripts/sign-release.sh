#!/usr/bin/env bash
set -euo pipefail
for name in MACOS_CERTIFICATE_P12_BASE64 MACOS_CERTIFICATE_PASSWORD APPLE_API_KEY_P8_BASE64 APPLE_API_KEY_ID APPLE_API_ISSUER_ID; do
  [[ -n "${!name}" ]] || { echo "::error::Repository secret $name is required to sign and notarize macOS packages."; exit 1; }
done
echo "::add-mask::$APPLE_API_KEY_ID"
echo "::add-mask::$APPLE_API_ISSUER_ID"
keychain="$RUNNER_TEMP/vibeface-signing.keychain-db"
keychain_password="$(openssl rand -hex 32)"
echo "::add-mask::$keychain_password"
umask 077
printf '%s' "$MACOS_CERTIFICATE_P12_BASE64" | base64 --decode > "$RUNNER_TEMP/vibeface-signing.p12"
printf '%s' "$APPLE_API_KEY_P8_BASE64" | base64 --decode > "$RUNNER_TEMP/vibeface-notary.p8"
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$RUNNER_TEMP/vibeface-signing.p12" -k "$keychain" -P "$MACOS_CERTIFICATE_PASSWORD" -f pkcs12 -T /usr/bin/codesign
rm -f "$RUNNER_TEMP/vibeface-signing.p12"
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain" > /dev/null
existing=()
while IFS= read -r line; do
  line="${line#"${line%%[![:space:]]*}"}"
  existing+=("${line//\"/}")
done < <(security list-keychains -d user)
security list-keychains -d user -s "$keychain" "${existing[@]}"
identities="$(security find-identity -v -p codesigning "$keychain" | grep '"Developer ID Application: ' || true)"
[[ -n "$identities" && "$(wc -l <<< "$identities")" -eq 1 ]] || { echo '::error::Expected exactly one Developer ID Application identity in the certificate.'; exit 1; }
identity="$(awk '{print $2}' <<< "$identities")"
team="$(sed -n 's/.*(\([A-Z0-9]\{10\}\))"$/\1/p' <<< "$identities")"
[[ "$identity" =~ ^[0-9A-F]{40}$ && -n "$team" ]] || { echo '::error::Could not resolve the signing identity or team.'; exit 1; }
if [[ "$1" == core ]]; then binary="$2/vibeface-core"; else binary="$2/vibeface"; fi
trap 'security delete-keychain "$keychain"; rm -f "$RUNNER_TEMP/vibeface-notary.p8"' EXIT
codesign --force --options runtime --timestamp --identifier cn.leaperone.vibeface --sign "$identity" "$binary"
codesign --verify --strict "$binary"
ditto -c -k --keepParent "$binary" "$RUNNER_TEMP/native.zip"
xcrun notarytool submit "$RUNNER_TEMP/native.zip" --key "$RUNNER_TEMP/vibeface-notary.p8" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" --wait --timeout 30m --output-format json > "$RUNNER_TEMP/notary-result.json"
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["status"] == "Accepted"' "$RUNNER_TEMP/notary-result.json"
