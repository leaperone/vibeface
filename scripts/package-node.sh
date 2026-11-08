#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: package-node.sh [--target RUST_TARGET] [--output DIRECTORY]
       package-node.sh --target RUST_TARGET --native-output ARCHIVE
       package-node.sh --target RUST_TARGET --native-input VERIFIED_DIRECTORY

Builds and packages the CLI, Core binary, web client, docs, and user service installer.
The default output is .artifacts/packages. Provider runtimes and credentials are not bundled.
USAGE
}

repo="$(cd "$(dirname "$0")/.." && pwd)"
output="$repo/.artifacts/packages"
target=''
native_output=''
native_input=''
while (($#)); do
  case "$1" in
    --target|--output)
      (($# >= 2)) || { usage >&2; exit 2; }
      if [[ "$1" == '--target' ]]; then target="$2"; else output="$2"; fi
      shift 2
      ;;
    --native-output|--native-input)
      (($# >= 2)) || { usage >&2; exit 2; }
      if [[ "$1" == '--native-output' ]]; then native_output="$2"; else native_input="$2"; fi
      shift 2
      ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done
[[ -z "$native_output" || -z "$native_input" ]] || { usage >&2; exit 2; }

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
if [[ -z "$native_output" ]]; then
  python3 "$repo/scripts/build-inputs.py" check crypto
  python3 "$repo/scripts/build-inputs.py" check web
fi
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
if [[ -n "$native_input" ]]; then
  input_args=(check native --input "$native_input" --target "$target")
  if [[ -n "${VIBEFACE_INPUT_ATTEMPT:-}" ]]; then input_args+=(--producer-attempt "$VIBEFACE_INPUT_ATTEMPT"); fi
  python3 "$repo/scripts/build-inputs.py" "${input_args[@]}"
  cli_binary="$native_input/vibeface"
  core_binary="$native_input/vibeface-core"
else
  release_rustflags="$(python3 "$repo/scripts/release-rustflags.py")"
  CARGO_ENCODED_RUSTFLAGS="$release_rustflags" cargo "${build_args[@]}"
fi
[[ -x "$cli_binary" && -x "$core_binary" ]] || { printf 'Release binaries are missing: %s and %s\n' "$cli_binary" "$core_binary" >&2; exit 1; }
if [[ -n "$native_output" ]]; then
  python3 "$repo/scripts/build-inputs.py" seal native --source "$(dirname "$cli_binary")" --target "$target" --output "$native_output"
  exit 0
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
version="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$repo/apps/cli/Cargo.toml" | head -n 1)"
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
cp "$repo/README.md" "$package/"
python3 - "$repo" "$package" <<'PY'
from pathlib import Path
import shutil
import subprocess
import sys

repo, package = map(Path, sys.argv[1:])
for name in subprocess.check_output(["git", "ls-files", "-z", "--", "packages/docs"], cwd=repo).decode().split("\0"):
    if not name:
        continue
    source = repo / name
    if source.is_symlink() or any((repo / part).is_symlink() for part in Path(name).parents) or not source.is_file():
        raise SystemExit("Package documentation must be a regular tracked file")
    destination = package / "docs" / Path(name).relative_to("packages/docs")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
PY
if [[ -f "$repo/LICENSE" ]]; then cp "$repo/LICENSE" "$package/"; fi
cat > "$package/RUNTIME-REQUIREMENTS.md" <<'REQUIREMENTS'
# Runtime requirements

The archive contains the CLI, Core, and static browser application. Install the provider tools separately before enabling the bot.

- Codex bots and native App Server access need the `codex` CLI and its authenticated account.
- Cursor uses the official local Agent CLI (`agent` or `cursor-agent`) and its CLI subscription login; an explicitly configured API key/token is also supported.

The device page's automatic connection finds common program installations and completes missing declared authentication variables from the Node environment or a bounded interactive login shell. Existing private settings and enabled uses are preserved. You can still choose an absolute program path in advanced settings.

Codex and Claude keep private session roots and can reuse official CLI access-token snapshots without consuming the source refresh token. Private logins take priority; expired snapshots need a fresh source login or the private login command in settings. Cursor uses private ACP roots with the official CLI authentication store. GUI tokens are not extracted. No credentials or provider runtimes are included.
REQUIREMENTS
PACKAGE_TARGET="$target" python3 "$repo/scripts/release.py" seal-bundle "$package"
python3 - "$output/$name.tar.gz" "$package" "$name" <<'PY'
import sys
import tarfile


def public_metadata(info):
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    info.pax_headers = {}
    return info


with tarfile.open(sys.argv[1], "w:gz") as archive:
    archive.add(sys.argv[2], arcname=sys.argv[3], filter=public_metadata)
PY
cd "$output"
if command -v sha256sum >/dev/null; then
  sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
else
  shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256"
fi
printf 'Package: %s\nChecksum: %s\n' "$output/$name.tar.gz" "$output/$name.tar.gz.sha256"
