#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: package-node.sh [--target RUST_TARGET] [--output DIRECTORY]

Builds and packages the CLI, Core binary, web client, docs, and user service installer.
The default output is .artifacts/packages. Provider runtimes and credentials are not bundled.
USAGE
}

repo="$(cd "$(dirname "$0")/.." && pwd)"
output="$repo/.artifacts/packages"
target=''
while (($#)); do
  case "$1" in
    --target|--output)
      (($# >= 2)) || { usage >&2; exit 2; }
      if [[ "$1" == '--target' ]]; then target="$2"; else output="$2"; fi
      shift 2
      ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

notary_vars=0
for var in VIBEFACE_NOTARY_KEY VIBEFACE_NOTARY_KEY_ID VIBEFACE_NOTARY_ISSUER; do
  if [[ -n "${!var:-}" ]]; then notary_vars=$((notary_vars + 1)); fi
done
if ((notary_vars != 0 && notary_vars != 3)); then
  echo 'Set all of VIBEFACE_NOTARY_KEY, VIBEFACE_NOTARY_KEY_ID and VIBEFACE_NOTARY_ISSUER, or none.' >&2
  exit 1
fi
if ((notary_vars == 3)) && [[ -z "${VIBEFACE_CODESIGN_IDENTITY:-}" ]]; then
  echo 'Notarization needs VIBEFACE_CODESIGN_IDENTITY.' >&2
  exit 1
fi

cd "$repo"
python3 "$repo/scripts/build-inputs.py" check crypto
python3 "$repo/scripts/build-inputs.py" check web
build_args=(build --release -p vibeface -p vibeface-core --locked)
target_dir="${CARGO_TARGET_DIR:-$repo/target}"
if [[ -n "$target" ]]; then
  build_args+=(--target "$target")
  cli_binary="$target_dir/$target/release/vibeface"
  core_binary="$target_dir/$target/release/vibeface-core"
else
  target="$(rustc -vV | sed -n 's/^host: //p')"
  cli_binary="$target_dir/release/vibeface"
  core_binary="$target_dir/release/vibeface-core"
fi
web="$repo/apps/app/dist/client"
[[ -s "$web/vibeface_secure.js" && -s "$web/vibeface_secure_bg.wasm" ]] || {
  echo 'Browser cryptography is missing from the static output. Run scripts/build-web-crypto.sh and scripts/build-web.sh before building a package.' >&2
  exit 1
}
[[ -s "$web/_shell.html" && -s "$web/index.html" && -s "$web/_redirects" && -s "$web/_headers" && -s "$web/.vibeface-build.json" ]] || {
  echo 'Web client is missing. Run scripts/build-web.sh before building a package.' >&2
  exit 1
}
release_rustflags="$(python3 "$repo/scripts/release-rustflags.py")"
CARGO_ENCODED_RUSTFLAGS="$release_rustflags" cargo "${build_args[@]}"
[[ -x "$cli_binary" && -x "$core_binary" ]] || { printf 'Release binaries are missing: %s and %s\n' "$cli_binary" "$core_binary" >&2; exit 1; }
version="$(PYTHONPATH="$repo/scripts" python3 -B - <<'PY2'
import release_context
print(release_context.read_version('cli'))
PY2
)"
[[ -n "$version" && -n "$target" ]] || { echo 'Could not determine package version or target.' >&2; exit 1; }
case "$version$target" in *[!a-zA-Z0-9._-]*) echo 'Invalid package version or target.' >&2; exit 1 ;; esac

mkdir -p "$output"
output="$(cd "$output" && pwd)"
staging="$(mktemp -d "$output/.vibeface-package.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
name="vibeface-$version-$target"
package="$staging/$name"
mkdir -p "$package/scripts" "$package/web"
cp "$cli_binary" "$package/vibeface"
cp "$core_binary" "$package/vibeface-core"
if [[ "$target" == *-apple-darwin && -n "${VIBEFACE_CODESIGN_IDENTITY:-}" ]]; then
  for binary in "$package/vibeface" "$package/vibeface-core"; do
    codesign --force --options runtime --timestamp --identifier cn.leaperone.vibeface --sign "$VIBEFACE_CODESIGN_IDENTITY" "$binary"
    codesign --verify --strict "$binary"
  done
  if ((notary_vars == 3)); then
    notary="$(mktemp -d "$staging/.notary.XXXXXX")"
    trap 'rm -rf "$staging" "$notary"' EXIT
    ditto -c -k --keepParent "$package" "$notary/vibeface-native.zip"
    xcrun notarytool submit "$notary/vibeface-native.zip" --key "$VIBEFACE_NOTARY_KEY" --key-id "$VIBEFACE_NOTARY_KEY_ID" --issuer "$VIBEFACE_NOTARY_ISSUER" --wait --timeout 30m --output-format json > "$notary/result.json" || true
    submission='' status=''
    read -r submission status < <(python3 -c 'import json, sys; r = json.load(open(sys.argv[1])); print(r.get("id", ""), r.get("status", ""))' "$notary/result.json" 2> /dev/null) || true
    if [[ "$status" != 'Accepted' ]]; then
      printf 'Notarization %s: %s\n' "${submission:-failed}" "${status:-unknown}" >&2
      cat "$notary/result.json" >&2
      if [[ -n "$submission" ]]; then
        xcrun notarytool log "$submission" --key "$VIBEFACE_NOTARY_KEY" --key-id "$VIBEFACE_NOTARY_KEY_ID" --issuer "$VIBEFACE_NOTARY_ISSUER" >&2 || true
      fi
      exit 1
    fi
    xcrun notarytool log "$submission" --key "$VIBEFACE_NOTARY_KEY" --key-id "$VIBEFACE_NOTARY_KEY_ID" --issuer "$VIBEFACE_NOTARY_ISSUER"
  fi
fi
cp -a "$web/." "$package/web/"
cp "$repo/scripts/install-node-service.sh" "$package/scripts/"
python3 "$repo/scripts/offline-docs.py" "$package"
if [[ -f "$repo/LICENSE" ]]; then cp "$repo/LICENSE" "$package/"; fi
PACKAGE_TARGET="$target" PYTHONPATH="$repo/scripts" python3 -B - "$package" "$output/$name.tar.gz" <<'PY'
import os
from pathlib import Path
import sys

import release_archive

package, archive = map(Path, sys.argv[1:])
release_archive.write_runtime_requirements(package, os.environ['PACKAGE_TARGET'])
release_archive.seal_bundle(package)
release_archive.public_archive(package, archive)
PY
printf 'Package: %s\nChecksum: %s\n' "$output/$name.tar.gz" "$output/$name.tar.gz.sha256"
