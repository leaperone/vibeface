#!/usr/bin/env bash
set -euo pipefail
umask 077

fail() {
  echo "$1" >&2
  exit "${2:-1}"
}

if [[ $# == 1 && $1 == --version ]]; then
  echo 'Vibeface confined RISC Zero Groth16 runner v2025-04-03.1'
  exit 0
fi

[[ $# == 5 ]] || fail 'unsupported confined Groth16 invocation' 64
[[ $1 == run && $2 == --rm && $3 == -v && $5 == risczero/risc0-groth16-prover:v2025-04-03.1 ]] \
  || fail 'unsupported confined Groth16 invocation' 64
[[ $4 == /*:/mnt ]] || fail 'invalid confined Groth16 work directory' 64
work=${4%:/mnt}
[[ $work != / && $work != *:* && $work != *$'\n'* && $work != *$'\r'* \
  && $(realpath --canonicalize-existing -- "$work") == "$work" ]] \
  || fail 'work directory must be a canonical absolute path' 64
[[ -d $work && $(stat -c '%u' -- "$work") == "$(id -u)" ]] \
  || fail 'work directory must belong to the prover' 64
mode=$(stat -c '%a' -- "$work")
(( (8#$mode & 077) == 0 )) || fail 'work directory must be private' 64
[[ -f $work/input.json && ! -L $work/input.json ]] || fail 'missing regular Groth16 input' 64
[[ ! -e $work/proof.json && ! -L $work/proof.json ]] || fail 'Groth16 output already exists' 64

exec 9</opt/vdp/gnark
flock --nonblock 9 || fail 'confined Groth16 runner is busy' 75
temporary=$(mktemp -d /tmp/vibeface-gnark.XXXXXXXX)
generator_pid=
prover_pid=
cleanup() {
  local status=$?
  trap - EXIT HUP INT TERM
  if [[ -n $generator_pid ]]; then
    kill -KILL "$generator_pid" 2>/dev/null || true
    wait "$generator_pid" 2>/dev/null || true
  fi
  if [[ -n $prover_pid ]]; then
    kill -KILL "$prover_pid" 2>/dev/null || true
    wait "$prover_pid" 2>/dev/null || true
  fi
  rm -rf -- "$temporary"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

ulimit -s 16384
mkfifo "$temporary/witness"
/opt/vdp/bin/stark_verify "$work/input.json" "$temporary/witness" \
  >"$temporary/generator.stdout" 2>"$temporary/generator.stderr" &
generator_pid=$!
/opt/vdp/bin/prover /opt/vdp/gnark/stark_verify.cs /opt/vdp/gnark/stark_verify_final.pk.dmp \
  "$temporary/witness" "$temporary/proof.json" \
  >"$temporary/prover.stdout" 2>"$temporary/prover.stderr" &
prover_pid=$!

if ! wait -n -p finished "$generator_pid" "$prover_pid"; then
  fail 'confined Groth16 worker failed'
fi
if [[ $finished == "$generator_pid" ]]; then
  generator_pid=
  wait "$prover_pid" || fail 'confined Groth16 prover failed'
  prover_pid=
else
  prover_pid=
  wait "$generator_pid" || fail 'confined Groth16 witness generator failed'
  generator_pid=
fi

[[ -f $temporary/proof.json && ! -L $temporary/proof.json ]] \
  || fail 'confined Groth16 prover produced no proof'
python3 - "$temporary/proof.json" "$work/proof.json" <<'PY'
import os
import sys

with open(sys.argv[1], "rb") as source:
    descriptor = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "wb") as output:
        while chunk := source.read(64 * 1024):
            output.write(chunk)
        output.flush()
        os.fsync(output.fileno())
PY
