#!/usr/bin/env bash
# Build a device archive; distribution signing requires both target profiles.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  printf 'Usage: %s <release-version> <source-sha>\n' "$0" >&2
  exit 2
fi

release_version="$1"
source_sha="$2"
certificate="${IOS_CERTIFICATE_P12_BASE64:-}"
certificate_password="${IOS_CERTIFICATE_PASSWORD:-}"
profile="${IOS_PROVISIONING_PROFILE_BASE64:-}"
share_profile="${IOS_SHARE_PROVISIONING_PROFILE_BASE64:-}"
method="${IOS_DISTRIBUTION_METHOD:-app-store-connect}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ "$release_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-(hourly\.[0-9]{10}|rc\.[0-9]+))?$ ]] || { echo 'Invalid release version' >&2; exit 2; }
[[ "$source_sha" =~ ^[0-9a-f]{40}$ ]] || { echo 'Invalid source SHA' >&2; exit 2; }
test "$(git -C "$root" rev-parse HEAD)" = "$source_sha"
test "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["expo"]["extra"]["releaseVersion"])' "$root/apps/mobile/app.json")" = "$release_version"

configured=0
for value in "$certificate" "$certificate_password" "$profile" "$share_profile"; do
  [[ -z "$value" ]] || ((configured+=1))
done
if (( configured != 0 && configured != 4 )); then
  echo 'iOS signing requires the P12, its password, and separate app and sharing extension profiles together' >&2
  exit 1
fi
[[ "$method" == app-store-connect || "$method" == ad-hoc ]] || { echo 'Unsupported iOS distribution method' >&2; exit 2; }

archive="$root/.artifacts/ios-build/Vibeface.xcarchive"
private=""
keychain=""
keychain_created=0
provisioning_dir="$HOME/Library/MobileDevice/Provisioning Profiles"
installed_count=0
installed_profiles=("")
existing_profiles=("")
keychain_list_changed=0
provisioning_dir_existed=1
mobile_device_dir_existed=1
cleanup() {
  cleanup_status=0
  if (( keychain_created == 1 )); then security delete-keychain "$keychain" >/dev/null 2>&1 || cleanup_status=1; fi
  if (( keychain_list_changed == 1 )); then
    # Restore the caller's keychain search list without adding signing material to it.
    python3 - "$private/keychains.json" <<'PY' >/dev/null 2>&1 || cleanup_status=1
import json, subprocess, sys
with open(sys.argv[1]) as source:
    old = json.load(source)
subprocess.run(['security', 'list-keychains', '-d', 'user', '-s', *old], check=True)
PY
  fi
  for ((index=0; index<installed_count; index++)); do
    if [[ -n "${existing_profiles[$index]}" ]]; then
      cp -p "${existing_profiles[$index]}" "${installed_profiles[$index]}" || cleanup_status=1
    else
      rm -f "${installed_profiles[$index]}" || cleanup_status=1
    fi
  done
  if (( provisioning_dir_existed == 0 )); then rmdir "$provisioning_dir" >/dev/null 2>&1 || true; fi
  if (( mobile_device_dir_existed == 0 )); then rmdir "$HOME/Library/MobileDevice" >/dev/null 2>&1 || true; fi
  if (( cleanup_status != 0 )); then
    echo 'iOS signing cleanup failed; private backups were retained for recovery' >&2
    return 1
  fi
  if [[ -n "$private" ]]; then rm -rf "$private"; fi
}
trap cleanup EXIT

unset IOS_SIGNING_TEAM IOS_SIGNING_PROFILE_UUID IOS_SHARE_SIGNING_PROFILE_UUID IOS_SIGNING_IDENTITY
if (( configured == 4 )); then
  umask 077
  git -C "$root" check-ignore -q .artifacts/ios-build/
  mkdir -p "$root/.artifacts/ios-build"
  private="$(mktemp -d "$root/.artifacts/ios-build/signing.XXXXXX")"
  keychain="$private/signing.keychain-db"
  [[ -d "$HOME/Library/MobileDevice" ]] || mobile_device_dir_existed=0
  [[ -d "$provisioning_dir" ]] || provisioning_dir_existed=0
  printf '%s' "$certificate" | base64 -D > "$private/ios-distribution.p12"
  printf '%s' "$profile" | base64 -D > "$private/app.mobileprovision"
  printf '%s' "$share_profile" | base64 -D > "$private/share.mobileprovision"
  security cms -D -i "$private/app.mobileprovision" > "$private/app.plist"
  security cms -D -i "$private/share.mobileprovision" > "$private/share.plist"
  profile_data="$(python3 -B "$root/scripts/ios_signing.py" "$private/app.plist" "$private/share.plist" "$method")"
  team="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["team"])' "$profile_data")"
  app_uuid="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["app_uuid"])' "$profile_data")"
  share_uuid="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["share_uuid"])' "$profile_data")"
  python3 - "$private/keychains.json" <<'PY'
import json, shlex, subprocess, sys
values = shlex.split(subprocess.check_output(['security', 'list-keychains', '-d', 'user'], text=True))
with open(sys.argv[1], 'w') as output:
    json.dump(values, output)
PY
  keychain_password="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
  security create-keychain -p "$keychain_password" "$keychain" >/dev/null
  keychain_created=1
  security set-keychain-settings -lut 21600 "$keychain"
  security unlock-keychain -p "$keychain_password" "$keychain"
  security import "$private/ios-distribution.p12" -k "$keychain" -P "$certificate_password" -T /usr/bin/codesign >/dev/null
  security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" >/dev/null
  keychain_list_changed=1
  python3 - "$private/keychains.json" "$keychain" <<'PY'
import json, subprocess, sys
with open(sys.argv[1]) as source:
    old = json.load(source)
subprocess.run(['security', 'list-keychains', '-d', 'user', '-s', sys.argv[2], *old], check=True)
PY
  security find-identity -v -p codesigning "$keychain" > "$private/identities.txt"
  identity="$(python3 - "$private/identities.txt" "$profile_data" <<'PY'
import json, re, sys
with open(sys.argv[1]) as source:
    identities = source.read()
pair = json.loads(sys.argv[2])
for fingerprint, team in re.findall(r'^\s*\d+\) ([A-Fa-f0-9]{40}) "Apple Distribution:.* \(([A-Z0-9]{10})\)"$', identities, re.M):
    if team == pair['team'] and fingerprint.upper() in pair['certificates']:
        print(fingerprint.upper())
        break
else:
    raise SystemExit('P12 has no Apple Distribution identity allowed by both target profiles')
PY
)"
  mkdir -p "$provisioning_dir"
  for target in app share; do
    if [[ "$target" == app ]]; then profile_uuid="$app_uuid"; else profile_uuid="$share_uuid"; fi
    installed_profile="$provisioning_dir/$profile_uuid.mobileprovision"
    [[ ! -L "$installed_profile" ]] || { echo 'Refusing to replace a symlinked provisioning profile' >&2; exit 1; }
    existing_profile=""
    if [[ -e "$installed_profile" ]]; then
      [[ -f "$installed_profile" ]] || { echo 'Installed provisioning profile is not a regular file' >&2; exit 1; }
      existing_profile="$private/existing-$target.mobileprovision"
      cp -p "$installed_profile" "$existing_profile"
    fi
    installed_profiles[installed_count]="$installed_profile"
    existing_profiles[installed_count]="$existing_profile"
    ((installed_count+=1))
    install -m 600 "$private/$target.mobileprovision" "$installed_profile"
  done
  python3 - "$private/export-options.plist" "$method" "$team" "$app_uuid" "$share_uuid" "$identity" <<'PY'
import plistlib, sys
path, method, team, app_uuid, share_uuid, identity = sys.argv[1:]
options = {
    'destination': 'export', 'method': method, 'signingStyle': 'manual',
    'teamID': team, 'signingCertificate': identity,
    'provisioningProfiles': {'one.leaper.vibeface': app_uuid, 'one.leaper.vibeface.share': share_uuid},
    'compileBitcode': False, 'stripSwiftSymbols': True,
}
with open(path, 'wb') as output:
    plistlib.dump(options, output)
PY
  export IOS_SIGNING_TEAM="$team" IOS_SIGNING_PROFILE_UUID="$app_uuid" IOS_SHARE_SIGNING_PROFILE_UUID="$share_uuid" IOS_SIGNING_IDENTITY="$identity"
fi

"$root/scripts/build-mobile-secure.sh" ios
cd "$root/apps/mobile"
npx expo prebuild --platform ios --no-install
(cd ios && pod install)
mkdir -p "$(dirname "$archive")"
rm -rf "$archive"
signing_args=()
if (( configured == 0 )); then
  signing_args=(CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY='')
fi
xcodebuild \
  -workspace ios/Vibeface.xcworkspace \
  -scheme Vibeface \
  -configuration Release \
  -destination 'generic/platform=iOS' \
  -archivePath "$archive" \
  ARCHS=arm64 \
  ${signing_args[@]+"${signing_args[@]}"} \
  archive

app="$archive/Products/Applications/Vibeface.app"
extension="$app/PlugIns/expo-sharing-extension.appex"
test -d "$app"
test -d "$extension"
if (( configured == 4 )); then
  test -e "$app/embedded.mobileprovision"
  test -e "$extension/embedded.mobileprovision"
  xcodebuild -exportArchive -archivePath "$archive" -exportPath "$root/.artifacts/ios-build/export" -exportOptionsPlist "$private/export-options.plist"
  test -f "$root/.artifacts/ios-build/export/Vibeface.ipa"
else
  [[ ! -e "$app/embedded.mobileprovision" && ! -e "$extension/embedded.mobileprovision" ]] || {
    echo 'Unsigned iOS archive unexpectedly contains a provisioning profile' >&2
    exit 1
  }
fi
