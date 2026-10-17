#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: package-node.sh [--skip-build] [--target RUST_TARGET] [--output DIRECTORY]

Builds and packages the node binary, SDK sidecar sources, execution image recipe, docs, and user service installer.
The default output is .artifacts/packages. Provider runtimes and credentials are not bundled.
USAGE
}

repo="$(cd "$(dirname "$0")/.." && pwd)"
output="$repo/.artifacts/packages"
build=true
target=''
while (($#)); do
  case "$1" in
    --skip-build) build=false; shift ;;
    --target|--output)
      (($# >= 2)) || { usage >&2; exit 2; }
      if [[ "$1" == '--target' ]]; then target="$2"; else output="$2"; fi
      shift 2
      ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

cd "$repo"
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
if "$build"; then cargo "${build_args[@]}"; fi
[[ -x "$binary" ]] || { printf 'Release binary is missing: %s\n' "$binary" >&2; exit 1; }
[[ -d "$repo/crates/node/sdk" ]] || { echo 'Node SDK sidecar sources are missing.' >&2; exit 1; }
version="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$repo/Cargo.toml" | head -n 1)"
[[ -n "$version" && -n "$target" ]] || { echo 'Could not determine package version or target.' >&2; exit 1; }
case "$version$target" in *[!a-zA-Z0-9._-]*) echo 'Invalid package version or target.' >&2; exit 1 ;; esac
source_commit='null'
if "$build" && [[ -z "$(git status --porcelain --untracked-files=normal)" ]]; then
  source_commit="\"$(git rev-parse HEAD)\""
fi

mkdir -p "$output"
output="$(cd "$output" && pwd)"
staging="$(mktemp -d "$output/.vibeface-package.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
name="vibeface-$version-$target"
package="$staging/$name"
mkdir -p "$package/sidecars" "$package/scripts/execution" "$package/web"
cp "$binary" "$package/vibeface"
printf '{"version":"%s","target":"%s","source_commit":%s,"update_protocol":2,"data_epoch":3}\n' "$version" "$target" "$source_commit" > "$package/BUILD-INFO.json"
cp "$crypto/vibeface_secure.js" "$crypto/vibeface_secure_bg.wasm" "$package/web/"
cp "$repo/scripts/install-node-service.sh" "$package/scripts/"
cp "$repo/scripts/build-execution-image.sh" "$package/scripts/"
cp "$repo/scripts/execution/Dockerfile" "$package/scripts/execution/"
cp "$repo/README.md" "$package/"
cp -R "$repo/docs" "$package/docs"
if [[ -f "$repo/LICENSE" ]]; then cp "$repo/LICENSE" "$package/"; fi
while IFS= read -r -d '' source; do
  cp "$source" "$package/sidecars/"
done < <(find "$repo/crates/node/sdk" -maxdepth 1 -type f \( -name '*.mjs' -o -name 'package.json' -o -name 'package-lock.json' -o -name 'README.md' \) -print0)
[[ -f "$package/sidecars/package.json" && -f "$package/sidecars/package-lock.json" && -f "$package/sidecars/codex-manager.mjs" ]] || { echo 'SDK sidecar entry, lockfile, or package manifest is missing.' >&2; exit 1; }
cat > "$package/RUNTIME-REQUIREMENTS.md" <<'REQUIREMENTS'
# Runtime requirements

The archive contains the Rust node binary and SDK sidecar sources. Install the provider tools separately before enabling the corresponding node roles.

- Codex execution needs the `codex` CLI and its authenticated account.
- The Codex SDK sidecar needs Node.js 18 or newer. Run `npm ci --omit=dev` in `sidecars/` before using it.
- Cursor scheduling needs the official Cursor SDK bridge executable and CURSOR_API_KEY in the service environment.
- Claude and Cursor CLI execution need their respective authenticated executables.
- Shared execution needs Docker and an immutable Linux image built with `bash scripts/build-execution-image.sh`. The included recipe installs real Codex 0.163.0, Claude Code 2.1.296, Node.js and Python 3. It contains no provider credentials.
- Set `execution_isolation.image` to the emitted `sha256:` image ID and `docker_bin` to the absolute Docker CLI path. Model egress uses the separately configured scoped gateway, never a host HOME or upstream key in the execution container. Shared Cursor model egress is currently rejected.

Use absolute provider and sidecar paths in `config.json` when running a background service. No credentials, provider runtimes, or npm dependencies are included.
REQUIREMENTS
COPYFILE_DISABLE=1 tar -czf "$output/$name.tar.gz" -C "$staging" "$name"
cd "$output"
if command -v sha256sum >/dev/null; then
  sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
else
  shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256"
fi
printf 'Package: %s\nChecksum: %s\n' "$output/$name.tar.gz" "$output/$name.tar.gz.sha256"
