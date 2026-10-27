#!/usr/bin/env bash
set -euo pipefail
umask 077
export LC_ALL=C

usage() {
  cat <<'USAGE'
Usage: install.sh [--channel stable|nightly] --install-root DIRECTORY --bin-dir DIRECTORY
       install.sh --self-test

Install the latest published Vibeface release for this platform (default: stable).
No account, provider, node data, background service, or automatic update is configured.
Existing installations must use vibeface update apply instead.
USAGE
}

fail() { printf 'Vibeface installer: %s\n' "$*" >&2; exit 1; }

# Parse only named fields; remote JSON is never evaluated as shell code.
json_fields() {
  awk -v mode="$2" -v page="${3:-0}" '
    function error() { invalid=1; exit 1 }
    function field(value, k) {
      k=key; key=""
      if (!(k ~ /^(tag_name|draft|prerelease|sha|version|target|source_commit|update_protocol|data_epoch)$/)) return
      if (seen[record SUBSEP k]++) error()
      if (k ~ /^(draft|prerelease)$/ && value !~ /^(true|false)$/) error()
      if (k ~ /^(update_protocol|data_epoch)$/ && value !~ /^(0|[1-9][0-9]*)$/) error()
      if (k !~ /^(draft|prerelease|update_protocol|data_epoch)$/ && value !~ /^"/) error()
      if (value ~ /^"/) {
        value=substr(value,2,length(value)-2)
        if (value ~ /[^a-zA-Z0-9._-]/) error()
      }
      print page "." record "\t" k "\t" value
    }
    { json=json $0 "\n" }
    END {
      if (invalid) exit 1
      depth=0; record=0; expected=(mode=="releases" ? "[" : "{")
      while (length(json)) {
        sub(/^[ \t\r\n]+/,"",json)
        if (!length(json)) break
        if (!match(json,/^("([^"\\]|\\.)*"|-?[0-9]+([.][0-9]+)?([eE][+-]?[0-9]+)?|true|false|null|[][{}:,])/)) error()
        token=substr(json,1,RLENGTH); json=substr(json,RLENGTH+1)
        if (closed) error()
        if (!started++) { if (token!=expected) error() }
        at=(mode=="releases" ? 2 : 1)
        if (key!="" && depth==at && token!=":") field(token)
        if (token=="{" || token=="[") {
          if (depth==0 && closed) error()
          depth++; stack[depth]=token
          if (mode=="releases" && depth==2) { if (token!="{") error(); record++ }
          if (depth>64) error()
        } else if (token=="}" || token=="]") {
          if (depth<1 || (token=="}" ? stack[depth]!="{" : stack[depth]!="[")) error()
          depth--; if (!depth) closed=1
        } else if (token==":" && depth==at) {
          if (previous !~ /^"/) error()
          key=substr(previous,2,length(previous)-2)
        }
        previous=token
      }
      if (!started || depth || !closed) error()
      if (mode=="releases") print record "\tcount\t" record
    }
  ' "$1"
}

select_release() {
  awk -F '\t' -v channel="$2" '
    function number(s) { return s ~ /^(0|[1-9][0-9]*)$/ && (length(s)<20 || (length(s)==20 && "x" s <= "x18446744073709551615")) }
    function date(s, y,m,d,days,leap) {
      if (s !~ /^[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]$/) return 0
      y=substr(s,1,4)+0; m=substr(s,5,2)+0; d=substr(s,7,2)+0
      leap=(y%4==0 && (y%100!=0 || y%400==0)); days=31
      if (m==4 || m==6 || m==9 || m==11) days=30
      if (m==2) days=28+leap
      return y>0 && m>=1 && m<=12 && d>=1 && d<=days
    }
    $2=="tag_name" { tags[$1]=$3 }
    $2=="draft" { draft[$1]=$3 }
    $2=="prerelease" { pre[$1]=$3 }
    END {
      for (i in tags) {
        tag=tags[i]; if (draft[i]!="false" || pre[i]!=(channel=="nightly" ? "true" : "false")) continue
        if (substr(tag,1,1)!="v") continue
        n=split(substr(tag,2),part,"-"); if (n!=(channel=="nightly" ? 2 : 1)) continue
        if (split(part[1],base,".")!=3 || !number(base[1]) || !number(base[2]) || !number(base[3])) continue
        day=0; build=0
        if (channel=="nightly") {
          if (split(part[2],night,".")!=3 || night[1]!="nightly" || !date(night[2]) || !number(night[3]) || night[3]=="0") continue
          day=night[2]; build=night[3]
        }
        print base[1],base[2],base[3],day,build,tag
      }
    }
  ' "$1" | sort -k1,1n -k2,2n -k3,3n -k4,4n -k5,5n | tail -n 1 | awk '{print $6}'
}

fetch() {
  local url="$1" destination="$2" limit="$3"
  curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
    --tlsv1.2 --connect-timeout 15 --max-time 180 --max-filesize "$limit" \
    --header 'Accept: application/vnd.github+json' --user-agent 'vibeface-installer' \
    --output "$destination" "$url" || fail "download failed (HTTP/network or GitHub API limit): $url"
  [[ $(wc -c < "$destination") -le $limit ]] || fail "download exceeds size limit: $url"
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

check_archive() {
  local archive="$1" package="$2" scratch="$3" length
  # Bound decompression before tar reads any member, then reject links and special files.
  length=$(gzip -dc "$archive" | head -c 536870913 | wc -c) || fail 'invalid or oversized compressed archive'
  ((length <= 536870912)) || fail 'expanded archive exceeds size limit'
  tar -tzf "$archive" > "$scratch/entries" || fail 'invalid tar archive'
  awk -v root="$package" '
    {
      path=$0; sub(/\/$/,"",path)
      if (path !~ /^[a-zA-Z0-9._\/-]+$/ || (path!=root && index(path,root "/")!=1)) exit 1
      n=split(path,parts,"/")
      for (i=1;i<=n;i++) if (parts[i]=="" || parts[i]=="." || parts[i]=="..") exit 1
      if (seen[path]++ || NR>4096) exit 1
    }
    END { if (NR==0) exit 1 }
  ' "$scratch/entries" || fail 'archive contains unsafe or duplicate paths'
  tar -tvzf "$archive" > "$scratch/types" || fail 'cannot inspect archive members'
  awk 'substr($0,1,1)!="-" && substr($0,1,1)!="d" {exit 1}' "$scratch/types" || fail 'archive contains links or special files'
}

verify_package() {
  local package="$1" version="$2" target="$3" commit="$4" scratch="$5" entry
  for entry in vibeface BUILD-INFO.json README.md RUNTIME-REQUIREMENTS.md \
    scripts/install-node-service.sh docs/node.md docs/releasing.md \
    web/vibeface_secure.js web/vibeface_secure_bg.wasm; do
    [[ -f "$package/$entry" && -s "$package/$entry" ]] || fail "package is missing $entry"
  done
  json_fields "$package/BUILD-INFO.json" object > "$scratch/build-fields" || fail 'invalid BUILD-INFO.json'
  awk -F '\t' -v version="$version" -v target="$target" -v commit="$commit" '
    { info[$2]=$3 }
    END { exit !(info["version"]==version && info["target"]==target && info["source_commit"]==commit && info["update_protocol"]=="1" && info["data_epoch"]=="5") }
  ' "$scratch/build-fields" || fail 'BUILD-INFO does not match the release, platform, source commit, or data epoch'
  [[ -x "$package/vibeface" ]] || fail 'packaged CLI is not executable'
  [[ $("$package/vibeface" --version) == "vibeface $version" ]] || fail 'packaged CLI version does not match the release'
}

install_release() (
  local channel=stable install_root='' bin_dir=''
  local target tag version package archive scratch='' lock='' destination='' complete=false
  local made_current=false made_bin=false
  while (($#)); do
    case "$1" in
      --channel|--install-root|--bin-dir)
        (($# >= 2)) || fail "missing value for $1"
        case "$1" in
          --channel) channel="$2" ;;
          --install-root) install_root="$2" ;;
          --bin-dir) bin_dir="$2" ;;
        esac
        shift 2 ;;
      --help|-h) usage; exit 0 ;;
      *) fail "unknown argument: $1" ;;
    esac
  done
  [[ $channel == stable || $channel == nightly ]] || fail 'channel must be stable or nightly'
  [[ -n "$install_root" && -n "$bin_dir" ]] || fail 'Specify --install-root and --bin-dir explicitly.'
  [[ $install_root == /* && $bin_dir == /* && $install_root != *$'\n'* && $bin_dir != *$'\n'* ]] || fail 'installation directories must be absolute paths without newlines'
  [[ ! -e "$install_root/current" && ! -L "$install_root/current" ]] || fail 'current already exists; use vibeface update apply'
  [[ ! -e "$bin_dir/vibeface" && ! -L "$bin_dir/vibeface" ]] || fail 'vibeface already exists in the bin directory; use update apply for a managed installation'
  case "$(uname -s)/$(uname -m)" in
    Darwin/arm64|Darwin/aarch64) target=aarch64-apple-darwin ;;
    Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-gnu ;;
    Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-gnu ;;
    *) fail 'supported platforms: macOS Apple Silicon, Linux x64 and Linux ARM64 (glibc)' ;;
  esac
  local tool
  for tool in curl awk sort tail head tar gzip mktemp wc uname; do
    command -v "$tool" >/dev/null 2>&1 || fail "required installation tool not found: $tool"
  done
  command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 || fail 'sha256sum or shasum is required'
  scratch=$(mktemp -d "${TMPDIR:-/tmp}/vibeface-install.XXXXXX")
  trap '
    if ! "$complete"; then
      if "$made_bin" && [[ -L "$bin_dir/vibeface" && $(readlink "$bin_dir/vibeface") == "$install_root/current/vibeface" ]]; then rm "$bin_dir/vibeface"; fi
      if "$made_current" && [[ -L "$install_root/current" && $(readlink "$install_root/current") == "releases/$version" ]]; then rm "$install_root/current"; fi
      if [[ -n $destination ]]; then rm -rf "$destination"; fi
    fi
    [[ -z $lock ]] || rmdir "$lock" || true
    rm -rf "$scratch"
  ' EXIT
  trap 'exit 130' INT TERM
  local page count found=false
  : > "$scratch/releases"
  for ((page=1; page<=20; page++)); do
    fetch "https://api.github.com/repos/leaperone/vibeface/releases?per_page=100&page=$page" "$scratch/page.json" 4194304
    json_fields "$scratch/page.json" releases "$page" > "$scratch/page-fields" || fail 'invalid GitHub release response'
    cat "$scratch/page-fields" >> "$scratch/releases"
    count=$(awk -F '\t' '$2=="count" {print $3}' "$scratch/page-fields")
    if ((count < 100)); then found=true; break; fi
  done
  "$found" || fail 'release list exceeds 20 pages; cannot select the latest version safely'
  tag=$(select_release "$scratch/releases" "$channel")
  if [[ -z $tag ]]; then
    if [[ $channel == stable ]]; then fail 'no published Stable release; use --channel nightly explicitly to install a preview'; fi
    fail 'no published Nightly release'
  fi
  version=${tag#v}
  package="vibeface-$version-$target"
  archive="$package.tar.gz"
  fetch "https://api.github.com/repos/leaperone/vibeface/commits/$tag" "$scratch/commit.json" 4194304
  json_fields "$scratch/commit.json" object > "$scratch/commit-fields" || fail 'invalid release commit response'
  local commit
  commit=$(awk -F '\t' '$2=="sha" {print $3}' "$scratch/commit-fields")
  [[ $commit =~ ^[0-9a-f]{40}$ ]] || fail 'release tag has no valid source commit'
  printf 'Downloading %s (%s)...\n' "$tag" "$target"
  fetch "https://github.com/leaperone/vibeface/releases/download/$tag/$archive" "$scratch/$archive" 134217728
  fetch "https://github.com/leaperone/vibeface/releases/download/$tag/$archive.sha256" "$scratch/checksum" 4096
  local expected actual
  expected=$(awk -v name="$archive" '
    NR==1 && NF==2 { file=$2; sub(/^\*/,"",file); if (file!=name || length($1)!=64 || $1 ~ /[^0-9a-fA-F]/) exit 1; hash=tolower($1); next }
    { exit 1 }
    END { if (NR!=1 || hash=="") exit 1; print hash }
  ' "$scratch/checksum") || fail 'checksum must name exactly the downloaded archive'
  actual=$(sha256 "$scratch/$archive")
  [[ $actual == "$expected" ]] || fail 'SHA256 mismatch; nothing was installed'
  check_archive "$scratch/$archive" "$package" "$scratch"
  mkdir "$scratch/unpack"
  COPYFILE_DISABLE=1 tar -xzf "$scratch/$archive" --no-same-owner --no-same-permissions -C "$scratch/unpack" || fail 'cannot unpack release'
  verify_package "$scratch/unpack/$package" "$version" "$target" "$commit" "$scratch"
  [[ ! -L "$install_root" && ! -L "$install_root/releases" && ! -L "$bin_dir" ]] || fail 'installation directories must not be symbolic links'
  mkdir -p "$install_root/releases" "$bin_dir"
  install_root=$(cd "$install_root" && pwd -P)
  bin_dir=$(cd "$bin_dir" && pwd -P)
  mkdir "$install_root/.install-lock" || fail 'another installer is active; no changes made'
  lock="$install_root/.install-lock"
  [[ ! -e "$install_root/current" && ! -L "$install_root/current" && ! -e "$bin_dir/vibeface" && ! -L "$bin_dir/vibeface" ]] || fail 'installation appeared during download; use update apply'
  [[ ! -e "$install_root/releases/$version" && ! -L "$install_root/releases/$version" ]] || fail 'release directory already exists; it was preserved'
  destination="$install_root/releases/$version"
  mv "$scratch/unpack/$package" "$destination" || fail 'cannot stage the verified release'
  ln -s "releases/$version" "$install_root/current" || fail 'cannot create the current release link'
  made_current=true
  ln -s "$install_root/current/vibeface" "$bin_dir/vibeface" || fail 'cannot create the CLI link'
  made_bin=true
  complete=true
  printf 'Installed %s at %s\n' "$tag" "$bin_dir/vibeface"
  printf 'No node data or services were created. Start your local node:\n  %q\n' "$bin_dir/vibeface"
  printf 'After initialization, select its update channel:\n  %q update configure --install-root %q --channel %s --automatic false\n' "$bin_dir/vibeface" "$install_root" "$channel"
  # shellcheck disable=SC2016 # Print the PATH expression for the user to evaluate.
  case ":$PATH:" in *":$bin_dir:"*) ;; *) printf 'Add your bin directory to PATH, or use the absolute CLI path above:\n  export PATH=%q:"$PATH"\n' "$bin_dir" ;; esac
)

self_test() (
  local test_dir script fixture version target package case_dir
  script=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")
  test_dir=$(mktemp -d "${TMPDIR:-/tmp}/vibeface-installer-test.XXXXXX")
  trap 'rm -rf "$test_dir"' EXIT
  test_dir=$(cd "$test_dir" && pwd -P)
  fixture="$test_dir/fixture"
  mkdir -p "$fixture/bin"
  export VIBEFACE_INSTALL_FIXTURE="$fixture"
  export VIBEFACE_INSTALL_REAL_LN
  VIBEFACE_INSTALL_REAL_LN=$(command -v ln)
  cat > "$fixture/bin/curl" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
output=''
[[ ! -f "$VIBEFACE_INSTALL_FIXTURE/http-failure" ]] || exit 22
while (($#)); do
  if [[ $1 == --output ]]; then output=$2; shift 2; else url=$1; shift; fi
 done
case "$url" in
  *'/releases?per_page=100&page=1') cp "$VIBEFACE_INSTALL_FIXTURE/releases.json" "$output" ;;
  *'/releases?per_page=100&page=2') cp "$VIBEFACE_INSTALL_FIXTURE/page2.json" "$output" ;;
  *'/commits/'*) printf '{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","nested":{"sha":"ignored"}}' > "$output" ;;
  *.tar.gz.sha256) cp "$VIBEFACE_INSTALL_FIXTURE/checksum" "$output" ;;
  *.tar.gz) cp "$VIBEFACE_INSTALL_FIXTURE/package.tar.gz" "$output" ;;
  *) exit 22 ;;
esac
MOCK
  cat > "$fixture/bin/uname" <<'MOCK'
#!/usr/bin/env bash
if [[ $1 == -s ]]; then cat "$VIBEFACE_INSTALL_FIXTURE/os"; else cat "$VIBEFACE_INSTALL_FIXTURE/arch"; fi
MOCK
  cat > "$fixture/bin/ln" <<'MOCK'
#!/usr/bin/env bash
if [[ -f "$VIBEFACE_INSTALL_FIXTURE/fail-link" && ${2:-} == */current/vibeface ]]; then exit 1; fi
exec "$VIBEFACE_INSTALL_REAL_LN" "$@"
MOCK
  chmod +x "$fixture/bin/curl" "$fixture/bin/uname" "$fixture/bin/ln"
  printf 'Linux\n' > "$fixture/os"
  printf 'x86_64\n' > "$fixture/arch"
  export PATH="$fixture/bin:$PATH"
  cat > "$fixture/releases.json" <<'JSON'
[
 {"tag_name":"v0.0.1-nightly.20261017.2","draft":false,"prerelease":true,"body":"quoted \"tag_name\": \"v999.0.0\""},
 {"tag_name":"v1.9.0","draft":false,"prerelease":false},
 {"tag_name":"v0.0.1-nightly.20261017.10","draft":false,"prerelease":true},
 {"tag_name":"v1.10.0","draft":false,"prerelease":false},
 {"tag_name":"v999.0.0-rc.1","draft":false,"prerelease":true},
 {"tag_name":"v999.0.0","draft":true,"prerelease":false},
 {"tag_name":"v0.0.1-nightly.20260229.1","draft":false,"prerelease":true},
 {"tag_name":"v01.2.3","draft":false,"prerelease":false},
 {"tag_name":"v18446744073709551616.0.0","draft":false,"prerelease":false},
 {"tag_name":"v0.0.1-nightly.20261017.18446744073709551616","draft":false,"prerelease":true}
]
JSON
  json_fields "$fixture/releases.json" releases > "$fixture/fields"
  [[ $(select_release "$fixture/fields" stable) == v1.10.0 ]] || fail 'self-test: Stable numeric sorting or channel isolation'
  [[ $(select_release "$fixture/fields" nightly) == v0.0.1-nightly.20261017.10 ]] || fail 'self-test: Nightly numeric sorting or channel isolation'
  version=0.0.1-nightly.20261017.10
  target=x86_64-unknown-linux-gnu
  package="vibeface-$version-$target"
  mkdir -p "$fixture/source/$package"
  local entry
  for entry in README.md RUNTIME-REQUIREMENTS.md scripts/install-node-service.sh docs/node.md docs/releasing.md \
    web/vibeface_secure.js web/vibeface_secure_bg.wasm; do
    mkdir -p "$fixture/source/$package/$(dirname "$entry")"
    printf 'test fixture\n' > "$fixture/source/$package/$entry"
  done
  printf '{"version":"%s","target":"%s","source_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","update_protocol":1,"data_epoch":5}\n' \
    "$version" "$target" > "$fixture/source/$package/BUILD-INFO.json"
  printf '#!/bin/sh\nprintf "vibeface %s\\n"\n' "$version" > "$fixture/source/$package/vibeface"
  chmod +x "$fixture/source/$package/vibeface"
  repack_fixture() {
    tar -czf "$fixture/package.tar.gz" -C "$fixture/source" "$package"
    printf '%s  %s.tar.gz\n' "$(sha256 "$fixture/package.tar.gz")" "$package" > "$fixture/checksum"
  }
  expect_failure() {
    if bash "$script" --channel "$1" --install-root "$case_dir/root" --bin-dir "$case_dir/bin" > "$case_dir/output" 2>&1; then
      fail "self-test: expected failure for $2"
    fi
    awk -v message="$2" 'index($0,message) {found=1} END {exit !found}' "$case_dir/output" || { cat "$case_dir/output" >&2; fail "self-test: missing error: $2"; }
  }
  repack_fixture
  case_dir="$test_dir/success"
  mkdir "$case_dir"
  bash "$script" --channel nightly --install-root "$case_dir/root" --bin-dir "$case_dir/bin" > "$case_dir/output"
  [[ $("$case_dir/bin/vibeface" --version) == "vibeface $version" ]] || fail 'self-test: installed CLI'
  [[ $(readlink "$case_dir/root/current") == "releases/$version" ]] || fail 'self-test: current layout'
  [[ $(readlink "$case_dir/bin/vibeface") == "$case_dir/root/current/vibeface" ]] || fail 'self-test: bin layout'
  [[ -f "$case_dir/root/releases/$version/scripts/install-node-service.sh" ]] || fail 'self-test: incomplete package'
  expect_failure nightly 'current already exists'
  [[ $(readlink "$case_dir/root/current") == "releases/$version" ]] || fail 'self-test: existing installation changed'
  local platform old_package
  for platform in aarch64-unknown-linux-gnu aarch64-apple-darwin x86_64-unknown-linux-gnu; do
    old_package="$package"
    target="$platform"
    package="vibeface-$version-$target"
    if [[ $old_package != "$package" ]]; then mv "$fixture/source/$old_package" "$fixture/source/$package"; fi
    printf '{"version":"%s","target":"%s","source_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","update_protocol":1,"data_epoch":5}\n' \
      "$version" "$target" > "$fixture/source/$package/BUILD-INFO.json"
    case "$target" in
      aarch64-apple-darwin) printf 'Darwin\n' > "$fixture/os"; printf 'arm64\n' > "$fixture/arch" ;;
      aarch64-unknown-linux-gnu) printf 'Linux\n' > "$fixture/os"; printf 'aarch64\n' > "$fixture/arch" ;;
      *) printf 'Linux\n' > "$fixture/os"; printf 'x86_64\n' > "$fixture/arch" ;;
    esac
    repack_fixture
    case_dir="$test_dir/$target"
    mkdir "$case_dir"
    bash "$script" --channel nightly --install-root "$case_dir/root" --bin-dir "$case_dir/bin" > "$case_dir/output"
    [[ -f "$case_dir/root/releases/$version/BUILD-INFO.json" ]] || fail "self-test: platform selection $target"
  done
  case_dir="$test_dir/link-failure"
  mkdir "$case_dir"
  : > "$fixture/fail-link"
  expect_failure nightly 'cannot create the CLI link'
  [[ ! -e "$case_dir/root/current" && ! -L "$case_dir/root/current" && ! -e "$case_dir/root/releases/$version" ]] || fail 'self-test: activation failure left a selected or partial release'
  rm "$fixture/fail-link"
  case_dir="$test_dir/checksum"
  mkdir "$case_dir"
  printf '%064d  %s.tar.gz\n' 0 "$package" > "$fixture/checksum"
  expect_failure nightly 'SHA256 mismatch'
  [[ ! -e "$case_dir/root/current" && ! -L "$case_dir/root/current" ]] || fail 'self-test: failed checksum activated a release'
  repack_fixture
  case_dir="$test_dir/checksum-name"
  mkdir "$case_dir"
  printf '%s  ../%s.tar.gz\n' "$(sha256 "$fixture/package.tar.gz")" "$package" > "$fixture/checksum"
  expect_failure nightly 'checksum must name exactly'
  repack_fixture
  case_dir="$test_dir/symlink"
  mkdir "$case_dir"
  ln -s /tmp/vibeface-outside "$fixture/source/$package/link"
  repack_fixture
  expect_failure nightly 'archive contains links or special files'
  rm "$fixture/source/$package/link"
  case_dir="$test_dir/absolute-path"
  mkdir "$case_dir"
  tar -czPf "$fixture/package.tar.gz" "$fixture/source/$package" 2>/dev/null
  printf '%s  %s.tar.gz\n' "$(sha256 "$fixture/package.tar.gz")" "$package" > "$fixture/checksum"
  expect_failure nightly 'archive contains unsafe or duplicate paths'
  repack_fixture
  case_dir="$test_dir/package-protocol"
  mkdir "$case_dir"
  sed 's/"update_protocol":1/"update_protocol":2/' "$fixture/source/$package/BUILD-INFO.json" > "$fixture/unsupported-build"
  cp "$fixture/unsupported-build" "$fixture/source/$package/BUILD-INFO.json"
  repack_fixture
  expect_failure nightly 'BUILD-INFO does not match'
  sed 's/"update_protocol":2/"update_protocol":1/' "$fixture/source/$package/BUILD-INFO.json" > "$fixture/supported-build"
  cp "$fixture/supported-build" "$fixture/source/$package/BUILD-INFO.json"
  repack_fixture
  local unsupported_epoch
  for unsupported_epoch in 0 4 6; do
    case_dir="$test_dir/data-epoch-$unsupported_epoch"
    mkdir "$case_dir"
    sed "s/\"data_epoch\":5/\"data_epoch\":$unsupported_epoch/" "$fixture/supported-build" > "$fixture/unsupported-build"
    cp "$fixture/unsupported-build" "$fixture/source/$package/BUILD-INFO.json"
    repack_fixture
    expect_failure nightly 'BUILD-INFO does not match'
  done
  cp "$fixture/supported-build" "$fixture/source/$package/BUILD-INFO.json"
  repack_fixture
  case_dir="$test_dir/manifest"
  mkdir "$case_dir"
  printf '{"version":"wrong","target":"%s","source_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","update_protocol":1,"data_epoch":5}\n' "$target" > "$fixture/source/$package/BUILD-INFO.json"
  repack_fixture
  expect_failure nightly 'BUILD-INFO does not match'
  case_dir="$test_dir/bin-conflict"
  mkdir -p "$case_dir/bin"
  printf 'preserve me\n' > "$case_dir/bin/vibeface"
  expect_failure nightly 'already exists in the bin directory'
  [[ $(cat "$case_dir/bin/vibeface") == 'preserve me' ]] || fail 'self-test: bin conflict changed'
  case_dir="$test_dir/no-stable"
  mkdir "$case_dir"
  printf '[{"tag_name":"v%s","draft":false,"prerelease":true}]\n' "$version" > "$fixture/releases.json"
  expect_failure stable 'no published Stable release'
  [[ ! -e "$case_dir/root" ]] || fail 'self-test: no Stable release created installation directories'
  # Page-local indexes must not overwrite earlier candidates.
  json_fields "$fixture/releases.json" releases 1 > "$fixture/fields"
  printf '[{"tag_name":"v0.0.1-nightly.20261017.2","draft":false,"prerelease":true}]\n' > "$fixture/page2.json"
  json_fields "$fixture/page2.json" releases 2 >> "$fixture/fields"
  [[ $(select_release "$fixture/fields" nightly) == "v$version" ]] || fail 'self-test: pagination changed version selection'
  case_dir="$test_dir/http-failure"
  mkdir "$case_dir"
  : > "$fixture/http-failure"
  expect_failure stable 'download failed (HTTP/network or GitHub API limit)'
  rm "$fixture/http-failure"
  printf 'Public installer self-test passed: channels, numeric sorting, layout, SHA256, manifest, links, paths, and existing-install protection.\n'
)

if [[ ${1:-} == --self-test ]]; then
  [[ $# == 1 ]] || fail '--self-test does not accept other arguments'
  self_test
else
  install_release "$@"
fi
