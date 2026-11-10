#!/usr/bin/env bash
# Build a generic iOS device archive. Signing is enabled only with a complete
# distribution certificate and provisioning profile supplied via environment.
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
method="${IOS_DISTRIBUTION_METHOD:-app-store-connect}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ "$release_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-(nightly\.[0-9]{8}\.[0-9]+|rc\.[0-9]+))?$ ]] || { echo 'Invalid release version' >&2; exit 2; }
[[ "$source_sha" =~ ^[0-9a-f]{40}$ ]] || { echo 'Invalid source SHA' >&2; exit 2; }
test "$(git -C "$root" rev-parse HEAD)" = "$source_sha"
test "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["expo"]["extra"]["releaseVersion"])' "$root/apps/mobile/app.json")" = "$release_version"

configured=0
for value in "$certificate" "$certificate_password" "$profile"; do
  [[ -z "$value" ]] || ((configured+=1))
done
if (( configured != 0 && configured != 3 )); then
  echo 'iOS signing requires the P12, its password, and a provisioning profile together' >&2
  exit 1
fi
[[ "$method" == app-store-connect || "$method" == ad-hoc ]] || { echo 'Unsupported iOS distribution method' >&2; exit 2; }

archive="$root/.artifacts/ios-build/Vibeface.xcarchive"
temp_root="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
private="$(mktemp -d "$temp_root/vibeface-ios-signing.XXXXXX")"
keychain="$private/signing.keychain-db"
provisioning_dir="$HOME/Library/MobileDevice/Provisioning Profiles"
existing_profile=""
profile_changed=0
keychain_list_changed=0
provisioning_dir_existed=0
mobile_device_dir_existed=0
[[ ! -d "$HOME/Library/MobileDevice" ]] || mobile_device_dir_existed=1
[[ ! -d "$provisioning_dir" ]] || provisioning_dir_existed=1
python3 - "$private/keychains.json" <<'PY'
import json, shlex, subprocess, sys
values = shlex.split(subprocess.check_output(['security', 'list-keychains', '-d', 'user'], text=True))
with open(sys.argv[1], 'w') as output:
    json.dump(values, output)
PY
cleanup() {
  security delete-keychain "$keychain" >/dev/null 2>&1 || true
  if (( keychain_list_changed == 1 )); then
    # Restore the caller's keychain search list without adding signing material to it.
    python3 - "$private/keychains.json" <<'PY' >/dev/null 2>&1 || true
import json, subprocess, sys
with open(sys.argv[1]) as source:
    old = json.load(source)
subprocess.run(['security', 'list-keychains', '-d', 'user', '-s', *old], check=True)
PY
  fi
  if (( profile_changed == 1 )); then
    installed_profile="$provisioning_dir/$profile_uuid.mobileprovision"
    if [[ -n "$existing_profile" ]]; then
      cp -p "$existing_profile" "$installed_profile"
    else
      rm -f "$installed_profile"
    fi
  fi
  if (( provisioning_dir_existed == 0 )); then rmdir "$provisioning_dir" >/dev/null 2>&1 || true; fi
  if (( mobile_device_dir_existed == 0 )); then rmdir "$HOME/Library/MobileDevice" >/dev/null 2>&1 || true; fi
  rm -rf "$private"
}
trap cleanup EXIT

if (( configured == 3 )); then
  umask 077
  printf '%s' "$certificate" | base64 -D > "$private/ios-distribution.p12"
  printf '%s' "$profile" | base64 -D > "$private/profile.mobileprovision"
  security cms -D -i "$private/profile.mobileprovision" > "$private/profile.plist"
  profile_data="$(python3 - "$private/profile.plist" "$method" <<'PY'
import datetime, hashlib, json, plistlib, re, sys
with open(sys.argv[1], 'rb') as source:
    profile = plistlib.load(source)
entitlements = profile.get('Entitlements', {})
app_id = entitlements.get('application-identifier', '')
teams = profile.get('TeamIdentifier', [])
if not profile.get('UUID') or not profile.get('Name') or not teams:
    raise SystemExit('Provisioning profile is missing its identity fields')
if not re.fullmatch(r'[0-9A-Fa-f]{8}(?:-[0-9A-Fa-f]{4}){3}-[0-9A-Fa-f]{12}', str(profile['UUID'])) or not re.fullmatch(r'[A-Z0-9]{10}', str(teams[0])):
    raise SystemExit('Provisioning profile UUID or team is invalid')
if app_id != teams[0] + '.one.leaper.vibeface':
    raise SystemExit('Provisioning profile application identifier does not exactly match the app bundle')
if entitlements.get('get-task-allow') is not False:
    raise SystemExit('Provisioning profile is not a distribution profile')
if profile.get('ExpirationDate', datetime.datetime.min) <= datetime.datetime.now(datetime.timezone.utc).replace(tzinfo=None):
    raise SystemExit('Provisioning profile is expired')
devices = profile.get('ProvisionedDevices')
if sys.argv[2] == 'ad-hoc' and not devices:
    raise SystemExit('Ad Hoc export requires a profile with registered devices')
if sys.argv[2] == 'app-store-connect' and (devices or profile.get('ProvisionsAllDevices')):
    raise SystemExit('App Store Connect export requires an App Store distribution profile')
certificates = [hashlib.sha1(certificate).hexdigest().upper() for certificate in profile.get('DeveloperCertificates', [])]
print(json.dumps({'team': teams[0], 'uuid': profile['UUID'], 'name': profile['Name'], 'certificates': certificates}))
PY
)"
  team="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["team"])' "$profile_data")"
  profile_uuid="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["uuid"])' "$profile_data")"
  profile_certificates="$(python3 -c 'import json,sys; print(" ".join(json.loads(sys.argv[1])["certificates"]))' "$profile_data")"
  keychain_password="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
  security create-keychain -p "$keychain_password" "$keychain" >/dev/null
  security set-keychain-settings -lut 21600 "$keychain"
  security unlock-keychain -p "$keychain_password" "$keychain"
  security import "$private/ios-distribution.p12" -k "$keychain" -P "$certificate_password" -T /usr/bin/codesign >/dev/null
  security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" >/dev/null
  python3 - "$private/keychains.json" "$keychain" <<'PY'
import json, subprocess, sys
with open(sys.argv[1]) as source:
    old = json.load(source)
subprocess.run(['security', 'list-keychains', '-d', 'user', '-s', sys.argv[2], *old], check=True)
PY
  keychain_list_changed=1
  identity_line="$(security find-identity -v -p codesigning "$keychain" | sed -nE "s/^[[:space:]]*[0-9]+\) ([A-Fa-f0-9]{40}) \"(Apple Distribution:.* \\($team\\))\"$/\\1|\\2/p" | head -n 1)"
  [[ -n "$identity_line" ]] || { echo 'P12 has no Apple Distribution identity matching the profile team' >&2; exit 1; }
  certificate_fingerprint="${identity_line%%|*}"
  identity="${identity_line#*|}"
  profile_certificates="$(printf '%s' "$profile_certificates" | tr '[:lower:]' '[:upper:]')"
  certificate_fingerprint="$(printf '%s' "$certificate_fingerprint" | tr '[:lower:]' '[:upper:]')"
  [[ " $profile_certificates " == *" $certificate_fingerprint "* ]] || { echo 'P12 signing certificate is not listed in the provisioning profile' >&2; exit 1; }
  mkdir -p "$provisioning_dir"
  installed_profile="$provisioning_dir/$profile_uuid.mobileprovision"
  if [[ -e "$installed_profile" ]]; then
    existing_profile="$private/existing-profile.mobileprovision"
    cp -p "$installed_profile" "$existing_profile"
  fi
  profile_changed=1
  install -m 600 "$private/profile.mobileprovision" "$installed_profile"
  python3 - "$private/export-options.plist" "$method" "$team" "$profile_uuid" <<'PY'
import plistlib, sys
path, method, team, uuid = sys.argv[1:]
options = {
    'destination': 'export', 'method': method, 'signingStyle': 'manual',
    'teamID': team, 'provisioningProfiles': {'one.leaper.vibeface': uuid},
    'compileBitcode': False, 'stripSwiftSymbols': True,
}
with open(path, 'wb') as output:
    plistlib.dump(options, output)
PY
  export IOS_SIGNING_TEAM="$team" IOS_SIGNING_PROFILE_UUID="$profile_uuid" IOS_SIGNING_IDENTITY="$identity"
fi

"$root/scripts/build-mobile-secure.sh" ios
cd "$root/apps/mobile"
npx expo prebuild --platform ios --no-install
(cd ios && pod install)
mkdir -p "$(dirname "$archive")"
rm -rf "$archive"
signing_args=(CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY='')
if (( configured == 3 )); then
  signing_args=(CODE_SIGNING_ALLOWED=YES CODE_SIGNING_REQUIRED=YES CODE_SIGN_STYLE=Manual "DEVELOPMENT_TEAM=$IOS_SIGNING_TEAM" "PROVISIONING_PROFILE=$IOS_SIGNING_PROFILE_UUID" "PROVISIONING_PROFILE_SPECIFIER=$IOS_SIGNING_PROFILE_UUID" "CODE_SIGN_IDENTITY=$IOS_SIGNING_IDENTITY")
fi
xcodebuild \
  -workspace ios/Vibeface.xcworkspace \
  -scheme Vibeface \
  -configuration Release \
  -destination 'generic/platform=iOS' \
  -archivePath "$archive" \
  ARCHS=arm64 \
  "${signing_args[@]}" \
  archive

test -d "$archive/Products/Applications/Vibeface.app"
if (( configured == 3 )); then
  app="$archive/Products/Applications/Vibeface.app"
  test -e "$app/embedded.mobileprovision"
  xcodebuild -exportArchive -archivePath "$archive" -exportPath "$root/.artifacts/ios-build/export" -exportOptionsPlist "$private/export-options.plist"
  test -f "$root/.artifacts/ios-build/export/Vibeface.ipa"
else
  ! test -e "$archive/Products/Applications/Vibeface.app/embedded.mobileprovision"
fi
