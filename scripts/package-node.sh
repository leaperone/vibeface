#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: package-node.sh [--target RUST_TARGET] [--output DIRECTORY]

Builds and packages the node binary, docs, and user service installer.
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
build_args=(build --release -p vibeface --locked)
target_dir="${CARGO_TARGET_DIR:-$repo/target}"
if [[ -n "$target" ]]; then
  build_args+=(--target "$target")
  binary="$target_dir/$target/release/vibeface"
else
  target="$(rustc -vV | sed -n 's/^host: //p')"
  binary="$target_dir/release/vibeface"
fi
crypto="$repo/crates/node/web/generated"
[[ -s "$crypto/vibeface_secure.js" && -s "$crypto/vibeface_secure_bg.wasm" ]] || {
  echo 'Browser cryptography is missing. Run scripts/build-web-crypto.sh before building a package.' >&2
  exit 1
}
[[ -s "$repo/crates/node/web/dist/client/_shell.html" ]] || {
  echo 'Web client is missing. Run scripts/build-web.sh before building a package.' >&2
  exit 1
}
release_rustflags="$(python3 "$repo/scripts/release-rustflags.py")"
CARGO_ENCODED_RUSTFLAGS="$release_rustflags" cargo "${build_args[@]}"
[[ -x "$binary" ]] || { printf 'Release binary is missing: %s\n' "$binary" >&2; exit 1; }
version="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$repo/Cargo.toml" | head -n 1)"
[[ -n "$version" && -n "$target" ]] || { echo 'Could not determine package version or target.' >&2; exit 1; }
case "$version$target" in *[!a-zA-Z0-9._-]*) echo 'Invalid package version or target.' >&2; exit 1 ;; esac
source_commit='null'
if [[ -z "$(git status --porcelain --untracked-files=normal)" ]]; then
  source_commit="\"$(git rev-parse HEAD)\""
fi

mkdir -p "$output"
output="$(cd "$output" && pwd)"
staging="$(mktemp -d "$output/.vibeface-package.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
name="vibeface-$version-$target"
package="$staging/$name"
mkdir -p "$package/scripts" "$package/web"
cp "$binary" "$package/vibeface"
if [[ "$target" == *-apple-darwin && -n "${VIBEFACE_CODESIGN_IDENTITY:-}" ]]; then
  codesign --force --options runtime --timestamp --identifier cn.leaperone.vibeface --sign "$VIBEFACE_CODESIGN_IDENTITY" "$package/vibeface"
  codesign --verify --strict "$package/vibeface"
  if ((notary_vars == 3)); then
    notary="$(mktemp -d "$staging/.notary.XXXXXX")"
    trap 'rm -rf "$staging" "$notary"' EXIT
    ditto -c -k --keepParent "$package/vibeface" "$notary/vibeface.zip"
    xcrun notarytool submit "$notary/vibeface.zip" --key "$VIBEFACE_NOTARY_KEY" --key-id "$VIBEFACE_NOTARY_KEY_ID" --issuer "$VIBEFACE_NOTARY_ISSUER" --wait --timeout 30m --output-format json > "$notary/result.json" || true
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
  fi
fi
data_epoch="$(sed -n 's/^pub const DATA_EPOCH: u32 = \([0-9][0-9]*\);$/\1/p' "$repo/crates/node/src/config.rs")"
update_protocol="$(sed -n 's/^pub const UPDATE_PROTOCOL: u32 = \([0-9][0-9]*\);$/\1/p' "$repo/crates/node/src/config.rs")"
[[ -n "$data_epoch" && -n "$update_protocol" ]] || { echo 'DATA_EPOCH or UPDATE_PROTOCOL not found in crates/node/src/config.rs' >&2; exit 1; }
printf '{"version":"%s","target":"%s","source_commit":%s,"update_protocol":%s,"data_epoch":%s}\n' "$version" "$target" "$source_commit" "$update_protocol" "$data_epoch" > "$package/BUILD-INFO.json"
cp "$crypto/vibeface_secure.js" "$crypto/vibeface_secure_bg.wasm" "$package/web/"
cp "$repo/scripts/install-node-service.sh" "$package/scripts/"
cp "$repo/README.md" "$package/"
python3 - "$repo" "$package" <<'PY'
from pathlib import Path
import shutil
import subprocess
import sys

repo, package = map(Path, sys.argv[1:])
for name in subprocess.check_output(["git", "ls-files", "-z", "--", "docs"], cwd=repo).decode().split("\0"):
    if not name:
        continue
    source = repo / name
    if source.is_symlink() or any((repo / part).is_symlink() for part in Path(name).parents) or not source.is_file():
        raise SystemExit("Package documentation must be a regular tracked file")
    destination = package / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
PY
if [[ -f "$repo/LICENSE" ]]; then cp "$repo/LICENSE" "$package/"; fi
cat > "$package/RUNTIME-REQUIREMENTS.md" <<'REQUIREMENTS'
# Runtime requirements

The archive contains the Rust node binary. Install the provider tools separately before enabling the bot.

- Codex bots and native App Server access need the `codex` CLI and its authenticated account.
- Cursor scheduling needs the official Cursor SDK bridge executable and CURSOR_API_KEY in the service environment.

Use absolute provider paths in `config.json` when running a background service. No credentials or provider runtimes are included.
REQUIREMENTS
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
