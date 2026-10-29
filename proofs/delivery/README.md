# Delivery proof build

This independent workspace uses RISC Zero 3.0.6 with a local client-only host. It does not add a prover to the Node dependency graph. Guest execution, genuine proof generation, EVM acceptance and OS resource enforcement are separate checks.

## Fixed tools

| Tool | Fixed version or source |
| --- | --- |
| Host Rust | `1.98.1`, declared in this workspace's rust-toolchain.toml. |
| Domain MSRV | Rust `1.91.0`. The domain has separate native checks and no RISC Zero dependency. |
| Guest Rust | Official `risc0/rust` release `r0.1.97.0`. |
| zkVM client and build library | `risc0-zkvm = 3.0.6`, `risc0-build = 3.0.6`. |
| Local executor and prover | Official `r0vm 3.0.6`, supplied by the `risc0/risc0` release `v3.0.6`. |
| SHA2 patch | `8631fabdea7bdffa97b11868e04e73491d8e5bcf`. |
| Curve25519 patch | `385adda1fa3b66d9aaa8b8fcc99ddc324d33ba32`. |
| crypto-bigint patch | `3ab63a6f1048833f7047d5a50532e4a4cc789384`. |

The three official crypto patches are isolated to this workspace and the guest's independent workspace. Both lockfiles record their exact Git revisions. Never add these patches to the root Node workspace.

The official Linux x86_64 Rust archive is `https://github.com/risc0/rust/releases/download/r0.1.97.0/rust-toolchain-x86_64-unknown-linux-gnu.tar.gz`. Its release-asset SHA256 is `100b597d605b706bec94e1d32bfaaf5851df8d75173f986d4b56a3e90916be53`. The Mac arm64 archive at the same release is `rust-toolchain-aarch64-apple-darwin.tar.gz`, with SHA256 `85070d82aebbe8e1027e75f2cd0c781f5c82c0ae7e06697218be9d8b71dec15d`.

Install verified archives only inside the task's ignored artifact directory. Do not change the user's default rustup toolchain or Docker VM settings. The CI installer owns download and archive validation. The layout consumed by risc0-build is exact.

The repository installer entry is `scripts/install-delivery-toolchain.py`. Its default RISC0_HOME is `.artifacts/vdp/risc0`. The full verification entry is `scripts/check-delivery.py`. Its `--check-fixtures` mode is structural preflight only and does not satisfy the proof gate.

```text
$RISC0_HOME/settings.toml
$RISC0_HOME/toolchains/v1.97.0-rust-x86_64-unknown-linux-gnu/bin/rustc
```

`settings.toml` contains `[default_versions]` followed by `rust = "1.97.0"`. On Mac use the platform suffix `aarch64-apple-darwin`. The version directory must be a real directory. Rzup ignores symlinked version directories. No rustup toolchain link is needed.

The Linux runtime archive is `https://github.com/risc0/risc0/releases/download/v3.0.6/cargo-risczero-x86_64-unknown-linux-gnu.tgz`, with official release-asset SHA256 `615d961bfb81d318db5071d7548389c850e324ac7f421c075176daf26082a60a`. It contains r0vm as well as cargo-risczero. Extract the verified r0vm into the task artifact directory and require `r0vm --version` to report 3.0.6. Rebuilding and verifying existing receipts in CI does not require an executor binary, so the installer does not install r0vm by default.

## Compile a new guest

The canonical build is Linux x86_64. Bind-mount the repository at `/work`, its isolated Cargo cache at `/cargo` and the same cache at `/home/runner/.cargo`, its verified RISC0_HOME at `/risc0`, and its ignored artifact directory at `/artifacts`. Set `HOME=/home/runner` and use real bind mounts rather than symlinks. Both the proof producer and CI use this layout.

```sh
cd /work
export HOME=/home/runner
export CARGO_HOME=/cargo
export RISC0_HOME=/risc0
bash proofs/delivery/build.sh
/artifacts/proof-target/release/vibeface-delivery-prover info
```

The methods build always compiles the guest and embeds its actual ELF and image ID. It does not use RISC0_SKIP_BUILD or a zero image. The guest compiler is selected from RISC0_HOME, independently of the host Rust toolchain. `risc0-build` strips `CARGO_*` variables before starting the nested guest build, so that build resolves its dependency cache from `HOME/.cargo`; the local sandbox deliberately reproduces CI's `/home/runner` path while sharing the isolated cache at both `/cargo` and `/home/runner/.cargo`. The sandbox keeps the guest source, compiler flags, and registered image configuration unchanged. A Linux build needs the Linux host compiler and Linux guest toolchain archive. A Mac r0vm or Mac Rust toolchain cannot run inside a Linux container.

`build.sh` rejects another OS, architecture, source path, cache path, dev mode or skipped guest build. It fixes host Rust 1.98.1 and output at `/artifacts/proof-target`, and sets RISC0_BUILD_LOCKED=1 so the nested guest build also respects its lockfile. Guest metadata remaps `/work` to `/src`, `/cargo` to `/deps`, `/risc0` to `/toolchain`, and `/artifacts` to `/build`. The Linux standard library comes from the fixed, checksum-verified Linux archive. Mac execution is development evidence only because the Mac prebuilt standard library embeds different source paths. Generate accepted fixtures with the canonical Linux build, then have CI independently rebuild and compare image and ELF hashes.

`info` returns `imageId`, `schemaHash`, `elfSha256`, and `statementBytes`. Only the actual built image belongs in the runtime ProofProfile. The guest embeds the fixed schema bytes and algorithms, not its final image ID or profile ID. A guest-source or ABI change requires rebuilding before generating new inputs and proofs.

## Execute and prove locally

Generate a bounded private ProofInput with the domain fixture example using this build's image ID. The example's `input` command creates a mode-0600 file and refuses overwrite. Feed that file on stdin. Never pass a witness, receiver secret or source plaintext on argv.

```sh
export RISC0_SERVER_PATH=/absolute/path/to/verified/r0vm
"$CARGO_TARGET_DIR/release/vibeface-delivery-prover" execute < .artifacts/delivery-proof/source.private.json
"$CARGO_TARGET_DIR/release/vibeface-delivery-prover" prove .artifacts/delivery-proof/source.receipt < .artifacts/delivery-proof/source.private.json
```

The host directly constructs ExternalProver with this trusted local binary. It never selects Bonsai or an automatic remote backend. It rejects RISC0_DEV_MODE, Fake receipts, unsuccessful execution, unresolved assumptions, wrong image and wrong journal. `execute` reports cycles and `proved=false`. `prove` verifies a real succinct receipt, compresses it to Groth16, verifies again and exports a public seal with the verifier selector.

At startup the host sets its process umask to `077`, before starting any child process. The local r0vm and its compression command inherit this mask, so Rust tempfile directories are created with mode `0700` and their files remain private. This does not depend on the calling shell's umask or relax the compression sandbox's permission checks.

### Explicit compression diagnostics

The normal two-argument `prove <new-receipt-path>` command and its eight public output fields are unchanged. For a diagnostic run, an explicit optional flag saves the verified Succinct receipt before Groth16 compression:

```sh
"$CARGO_TARGET_DIR/release/vibeface-delivery-prover" prove .artifacts/delivery-proof/source.receipt --succinct-receipt .artifacts/delivery-proof/source.succinct.receipt < .artifacts/delivery-proof/source.private.json
```

Both output paths must be new and distinct. The checkpoint is atomically published with mode `0600`, after verification against this build's image and the exact statement journal. It remains available if compression fails. The Node's default call does not request this checkpoint or allocate additional intermediate storage.

Retry only compression from an existing checkpoint with its expected public statement journal:

```sh
"$CARGO_TARGET_DIR/release/vibeface-delivery-prover" compress .artifacts/delivery-proof/source.retry.receipt "<expected-public-journal-hex>" < .artifacts/delivery-proof/source.succinct.receipt
```

`compress` reads a bincode Receipt on stdin, limited to 16 MiB with trailing bytes rejected. It accepts only a genuine Succinct receipt for this compiled image, successful execution without unresolved assumptions, and the specified canonical statement journal. It rejects Fake, Groth16, corrupted or mismatched input before invoking the local compression prover. The new Groth16 receipt is verified and atomically published with mode `0600`; an existing destination, including a symlink, is never overwritten.

Diagnostic success emits exactly seven public fields: `mode="compress"`, `kind="groth16"`, `imageId`, `relation`, `journal`, `journalHash`, and `seal`. It does not rerun guest execution or generate a new Succinct proof. This output omits `profileId` and is not automatically accepted as the eight-field `prove` public fixture. Neither checkpoint creation nor a failed compression counts as a Groth16 proof.

Do not call a source native test or guest execution a genuine proof. Run incorrect witnesses before expensive proof generation. Check a complete 64 KiB window and both windows of a two-window manifest against the actual compiled image.

The profile's maxProverMemoryBytes is a signed limit. It is not OS enforcement. A Linux cgroup must include the local r0vm and the Groth16 prover. A Docker wrapper launched through a host Docker socket becomes a sibling container and needs its own explicit bound. Putting only the host process inside a container does not prove the whole job stayed within the profile. macOS RLIMIT_RSS is not a substitute.

## Public proof fixtures

Only add genuinely generated receipts under `fixtures/`. When no fixtures are present, `scripts/check-delivery.py` skips receipt verification; a passing guest rebuild and contract or cross-language check does not fill that missing genuine-proof coverage. Missing fixtures are an incomplete proof gate.

`manifest.json` has this shape.

```json
{
  "schemaVersion": 1,
  "imageId": "lowercase hex64 without 0x",
  "schemaHash": "lowercase hex64 without 0x",
  "elfSha256": "lowercase hex64 without 0x",
  "proofs": [
    {"receipt": "relation-1.receipt", "public": "relation-1.json"}
  ]
}
```

Require all four relations. Additional window fixtures use unique basenames. Public JSON is the host prove output with mode, kind, imageId, profileId, relation, journal, journalHash and seal. CI rebuilds the guest, compares manifest metadata to host info, and passes each expected public journal to the rebuilt host's verify command with its receipt on stdin. It must reject stale image, schema, ELF, journal and changed proof. Keep private inputs outside this directory.
