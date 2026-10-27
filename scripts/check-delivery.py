#!/usr/bin/env python3
"""Rebuild the delivery guest, verify its public receipts, and exercise the EVM adapter."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / ".artifacts/vdp"
FIXTURES = ROOT / "proofs/delivery/fixtures"
PROFILE = ROOT / "proofs/delivery/profile.json"
STATEMENT_BYTES = 47 * 32
PUBLIC_FIELDS = {"mode", "kind", "imageId", "profileId", "relation", "journal", "journalHash", "seal"}
PROFILE_CAPS = {"crypto_suite": 1, "max_windows": 2, "max_window_bytes": 65_536, "max_total_bytes": 131_072,
                "max_guest_cycles": 64_000_000, "max_prover_memory_bytes": 16 * 1024 * 1024 * 1024}


def hex_bytes(value, length=None):
    if not isinstance(value, str) or any(character not in "0123456789abcdef" for character in value):
        raise ValueError("Public proof values must use lowercase hexadecimal without a prefix")
    if not value or len(value) % 2 or length is not None and len(value) != length * 2:
        raise ValueError("Public proof hexadecimal has the wrong length")
    return bytes.fromhex(value)


def checked_metadata(manifest, proofs, compiled=None):
    if set(manifest) != {"schemaVersion", "imageId", "schemaHash", "elfSha256", "proofs"} or type(manifest["schemaVersion"]) is not int or manifest["schemaVersion"] != 1:
        raise ValueError("Unsupported delivery fixture manifest")
    for field in ("imageId", "schemaHash", "elfSha256"):
        hex_bytes(manifest[field], 32)
    if len(manifest["proofs"]) != 4 or len(proofs) != 4:
        raise ValueError("Delivery fixtures must cover all four proof relations")
    if compiled is not None:
        for field in ("imageId", "schemaHash", "elfSha256"):
            if manifest[field] != compiled[field]:
                raise ValueError(f"Delivery fixture {field} differs from the rebuilt guest")
        if compiled["statementBytes"] != STATEMENT_BYTES:
            raise ValueError("Rebuilt guest uses a different public statement schema")
    for relation, (entry, proof) in enumerate(zip(manifest["proofs"], proofs), 1):
        if entry != {"receipt": f"relation-{relation}.receipt", "public": f"relation-{relation}.json"}:
            raise ValueError("Unexpected delivery fixture file or order")
        if set(proof) != PUBLIC_FIELDS or proof["mode"] != "prove" or proof["kind"] != "groth16" or type(proof["relation"]) is not int or proof["relation"] != relation:
            raise ValueError("Fixture must be the public output of a genuine Groth16 proof")
        if proof["imageId"] != manifest["imageId"]:
            raise ValueError("Fixture metadata binds another guest image")
        image = hex_bytes(proof["imageId"], 32)
        profile = hex_bytes(proof["profileId"], 32)
        journal = hex_bytes(proof["journal"], STATEMENT_BYTES)
        digest = hex_bytes(proof["journalHash"], 32)
        hex_bytes(proof["seal"], 4 + 8 * 32)
        if hashlib.sha256(journal).digest() != digest:
            raise ValueError("Fixture public journal digest mismatch")
        if int.from_bytes(journal[32:64], "big") != relation or journal[4 * 32:5 * 32] != profile or journal[5 * 32:6 * 32] != image:
            raise ValueError("Fixture relation, profile or image differs from its public journal")


def read_fixture(name, limit):
    file = FIXTURES / name
    if file.is_symlink() or not file.is_file() or file.stat().st_size > limit:
        raise ValueError(f"Missing, oversized or nonregular public fixture: {name}")
    return file.read_bytes()


def checked_profile(profile, compiled):
    if set(profile) != {"guest_image_id", "statement_schema_hash", *PROFILE_CAPS}:
        raise ValueError("Product delivery profile has unexpected fields")
    for field, expected in PROFILE_CAPS.items():
        if type(profile[field]) is not int or profile[field] != expected:
            raise ValueError(f"Product delivery profile has an unsupported {field}")
    for field, metadata in (("guest_image_id", "imageId"), ("statement_schema_hash", "schemaHash")):
        word = profile[field]
        if not isinstance(word, list) or len(word) != 32 or any(type(byte) is not int or not 0 <= byte <= 255 for byte in word):
            raise ValueError("Product delivery profile requires canonical 32-byte words")
        if bytes(word) != hex_bytes(compiled[metadata], 32):
            raise ValueError(f"Product delivery profile {field} differs from the rebuilt guest")


def load_profile(compiled):
    if PROFILE.is_symlink() or not PROFILE.is_file() or PROFILE.stat().st_size > 16 * 1024:
        raise ValueError("Missing, oversized or nonregular product delivery profile")
    profile = json.loads(PROFILE.read_bytes())
    checked_profile(profile, compiled)
    return profile


def checked_profile_vectors(vectors, profile, compiled, proofs):
    checked_profile(profile, compiled)
    if vectors["profile"] != profile or vectors["schemaHash"] != compiled["schemaHash"]:
        raise ValueError("Rust public vectors differ from the product delivery profile")
    expected = hex_bytes(vectors["profileId"], 32)
    if hashlib.sha256(hex_bytes(vectors["profileAbi"], 9 * 32)).digest() != expected:
        raise ValueError("Rust profile ID differs from its canonical ABI")
    for proof in proofs:
        if hex_bytes(proof["profileId"], 32) != expected:
            raise ValueError("Public receipt uses a profile outside the product trust root")


def load_fixtures():
    manifest = json.loads(read_fixture("manifest.json", 16 * 1024))
    proofs = [json.loads(read_fixture(f"relation-{relation}.json", 32 * 1024)) for relation in range(1, 5)]
    checked_metadata(manifest, proofs)
    schema = (ROOT / "crates/delivery/abi/Statement.sol").read_bytes()
    if manifest["schemaHash"] != hashlib.sha256(schema).hexdigest():
        raise ValueError("Delivery fixtures use an outdated canonical ABI schema")
    load_profile(manifest)
    for relation in range(1, 5):
        if not read_fixture(f"relation-{relation}.receipt", 16 * 1024 * 1024):
            raise ValueError("An empty public receipt is not a proof")
    return manifest, proofs


def optional_fixtures():
    if not FIXTURES.exists() and not FIXTURES.is_symlink():
        print("skipped: no public proof fixture; receipt verification needs fixtures generated on an x86_64 machine with enough memory", flush=True)
        return None
    return load_fixtures()


def run(command, **kwargs):
    return subprocess.run(command, cwd=ROOT, check=True, **kwargs)


def checked_verification(result, compiled, proof):
    expected = {"verified": True, "kind": "groth16", "seal": proof["seal"], "imageId": compiled["imageId"], "journalHash": proof["journalHash"]}
    if result != expected or result["verified"] is not True:
        raise ValueError("Receipt verification result or exported seal differs from its public fixture")


def sandbox_commands(context):
    image = "vibeface-vdp-proof:check-" + hashlib.sha256(str(ROOT).encode()).hexdigest()[:12]
    build = ["docker", "build", "--platform", "linux/amd64", "--file", str(context / "proofs/delivery/Sandbox.Dockerfile"), "--tag", image, str(context)]
    container = ["docker", "run", "--rm", "--platform", "linux/amd64", "--memory", "11g", "--memory-swap", "11g", "--pids-limit", "64"]
    for source, destination in ((ROOT, "/work"), (ARTIFACTS / "cargo", "/cargo"), (ARTIFACTS / "risc0", "/risc0"), (ARTIFACTS / "canonical", "/artifacts")):
        container.extend(["--mount", f"type=bind,source={source},target={destination}"])
    container.extend([image, "bash", "-c", "python3 scripts/install-delivery-toolchain.py && python3 scripts/check-delivery.py"])
    return build, container


def sandbox():
    optional_fixtures()
    for name in ("cargo", "risc0", "canonical"):
        (ARTIFACTS / name).mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="sandbox-build-", dir=ARTIFACTS) as directory:
        context = Path(directory)
        for name in ("proofs/delivery/Sandbox.Dockerfile", "proofs/delivery/confined-gnark.sh"):
            source = ROOT / name
            if source.is_symlink() or not source.is_file():
                raise ValueError(f"Sandbox build requires a regular source file: {name}")
            destination = context / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
        build, container = sandbox_commands(context)
        run(build)
    run(container)


def check():
    if any(name in os.environ for name in ("RISC0_DEV_MODE", "RISC0_SKIP_BUILD")):
        raise ValueError("Delivery verification forbids dev mode and skipped guest builds")
    if (platform.system(), platform.machine(), str(ROOT)) != ("Linux", "x86_64", "/work") or os.environ.get("CARGO_HOME") != "/cargo" or os.environ.get("RISC0_HOME") != "/risc0":
        raise ValueError("Run full delivery verification in the canonical Linux sandbox at /work with CARGO_HOME=/cargo and RISC0_HOME=/risc0")
    fixtures = optional_fixtures()
    manifest, proofs = fixtures or (None, [])
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    environment = {**os.environ, "RISC0_BUILD_LOCKED": "1"}
    target = Path("/artifacts/proof-target")
    run(["cargo", "fmt", "--manifest-path", "proofs/delivery/Cargo.toml", "--all", "--", "--check"], env=environment)
    run(["cargo", "fmt", "--manifest-path", "proofs/delivery/guest/Cargo.toml", "--", "--check"], env=environment)
    run(["bash", "proofs/delivery/build.sh"], env=environment)
    prover = target / "release/vibeface-delivery-prover"
    compiled = json.loads(run([str(prover), "info"], capture_output=True, text=True, env=environment).stdout)
    if fixtures:
        checked_metadata(manifest, proofs, compiled)
    (ARTIFACTS / "host-info.json").write_text(json.dumps(compiled, indent=2) + "\n")
    profile = load_profile(compiled)
    vectors = ARTIFACTS / "public-vectors.json"
    with vectors.open("wb") as output:
        run(["cargo", "run", "--locked", "-p", "vibeface-delivery", "--example", "fixture", "--target-dir", "/artifacts/domain-target", "--", "public", "35", "2", compiled["imageId"], str(PROFILE)], stdout=output, env=environment)
    checked_profile_vectors(json.loads(vectors.read_bytes()), profile, compiled, proofs)
    verified = []
    for relation, proof in enumerate(proofs, 1):
        receipt = read_fixture(f"relation-{relation}.receipt", 16 * 1024 * 1024)
        command = [str(prover), "verify", proof["journal"]]
        result = json.loads(run(command, input=receipt, capture_output=True, env=environment).stdout)
        checked_verification(result, compiled, proof)
        journal = bytearray(hex_bytes(proof["journal"]))
        journal[5 * 32] ^= 1
        rejected = subprocess.run([str(prover), "verify", journal.hex()], input=receipt, capture_output=True, env=environment, cwd=ROOT)
        if rejected.returncode == 0:
            raise ValueError("A receipt accepted a journal with another guest image")
        verified.append({"relation": relation, "journalHash": proof["journalHash"], "verified": True, "wrongJournalRejected": True})
        print(f"PASS current guest relation {relation} public receipt and wrong-journal rejection", flush=True)
    scripts = [("install.mjs", []), ("compile.mjs", ["--check-trust"]), ("exercise-settlement.mjs", ["--self-test"]), ("run.mjs", []), ("verify-risc0.mjs", []), ("compare-vectors.mjs", [str(vectors)])]
    if fixtures:
        scripts.append(("verify-delivery.mjs", [str(FIXTURES)]))
    for script, arguments in scripts:
        run(["node", f"contracts/delivery/{script}", *arguments])
    (ARTIFACTS / "proof-results.json").write_text(json.dumps({"imageId": compiled["imageId"], "schemaHash": compiled["schemaHash"], "receipts": verified}, indent=2) + "\n")
    print("Delivery receipts, cross-language bytes and production EVM adapter checks passed" if fixtures else "Guest rebuild, cross-language bytes and production EVM adapter checks passed; skipped: no public proof fixture")


def self_test():
    from copy import deepcopy
    from unittest.mock import patch
    manifest = {"schemaVersion": 1, "imageId": "11" * 32, "schemaHash": "22" * 32, "elfSha256": "33" * 32,
                "proofs": [{"receipt": f"relation-{relation}.receipt", "public": f"relation-{relation}.json"} for relation in range(1, 5)]}
    compiled = {key: manifest[key] for key in ("imageId", "schemaHash", "elfSha256")}
    compiled["statementBytes"] = STATEMENT_BYTES
    proofs = []
    for relation in range(1, 5):
        journal = bytearray(STATEMENT_BYTES)
        journal[32:64] = relation.to_bytes(32, "big")
        journal[4 * 32:5 * 32] = b"\x44" * 32
        journal[5 * 32:6 * 32] = b"\x11" * 32
        proofs.append({"mode": "prove", "kind": "groth16", "relation": relation, "imageId": manifest["imageId"], "profileId": "44" * 32,
                       "journal": journal.hex(), "journalHash": hashlib.sha256(journal).hexdigest(), "seal": "55" * 260})
    checked_metadata(manifest, proofs, compiled)

    def reject(value, data, image=compiled):
        try:
            checked_metadata(value, data, image)
        except (ValueError, TypeError, KeyError):
            return
        raise AssertionError("Invalid fixture metadata must be rejected")

    for field in ("imageId", "schemaHash", "elfSha256"):
        changed = dict(compiled, **{field: "66" * 32})
        reject(manifest, proofs, changed)
    reject(dict(manifest, proofs=manifest["proofs"][:-1]), proofs)
    changed = deepcopy(manifest)
    changed["proofs"][0]["receipt"] = "../outside"
    reject(changed, proofs)
    for field, value in (("relation", 4), ("kind", "fake"), ("imageId", "66" * 32), ("profileId", "66" * 32), ("journalHash", "66" * 32), ("seal", "55"), ("secret", "must-not-be-public")):
        changed = deepcopy(proofs)
        changed[0][field] = value
        reject(manifest, changed)
    profile = {**PROFILE_CAPS, "guest_image_id": list(hex_bytes(compiled["imageId"])), "statement_schema_hash": list(hex_bytes(compiled["schemaHash"]))}
    profile_abi = bytes(9 * 32)
    expected_profile = hashlib.sha256(profile_abi).hexdigest()
    vectors = {"profile": profile, "schemaHash": compiled["schemaHash"], "profileAbi": profile_abi.hex(), "profileId": expected_profile}
    profile_proofs = deepcopy(proofs)
    for proof in profile_proofs:
        proof["profileId"] = expected_profile
        journal = bytearray(hex_bytes(proof["journal"]))
        journal[4 * 32:5 * 32] = hex_bytes(expected_profile)
        proof["journal"], proof["journalHash"] = journal.hex(), hashlib.sha256(journal).hexdigest()
    checked_metadata(manifest, profile_proofs, compiled)
    checked_profile_vectors(vectors, profile, compiled, profile_proofs)
    for field, value in (("max_windows", 16), ("max_total_bytes", 1_048_576), ("crypto_suite", True), ("guest_image_id", [102] * 32), ("statement_schema_hash", [102] * 32), ("extra", 1)):
        changed = deepcopy(profile)
        changed[field] = value
        try:
            checked_profile_vectors(vectors, changed, compiled, profile_proofs)
        except ValueError:
            pass
        else:
            raise AssertionError("Unsupported product profiles must fail")
    changed = deepcopy(profile_proofs)
    for proof in changed:
        proof["profileId"] = "66" * 32
        journal = bytearray(hex_bytes(proof["journal"]))
        journal[4 * 32:5 * 32] = b"\x66" * 32
        proof["journal"], proof["journalHash"] = journal.hex(), hashlib.sha256(journal).hexdigest()
    checked_metadata(manifest, changed, compiled)
    try:
        checked_profile_vectors(vectors, profile, compiled, changed)
    except ValueError:
        pass
    else:
        raise AssertionError("Self-consistent receipt profiles outside the product trust root must fail")
    verified = {"verified": True, "kind": "groth16", "seal": proofs[0]["seal"], "imageId": compiled["imageId"], "journalHash": proofs[0]["journalHash"]}
    checked_verification(verified, compiled, proofs[0])
    for field, value in (("kind", "succinct"), ("seal", "66" * 260), ("imageId", "66" * 32), ("journalHash", "66" * 32), ("verified", 1), ("extra", "unexpected")):
        changed = dict(verified, **{field: value})
        try:
            checked_verification(changed, compiled, proofs[0])
        except ValueError:
            pass
        else:
            raise AssertionError("A mismatched receipt export must fail")
    build, container = sandbox_commands(Path("/context"))
    assert build[:5] == ["docker", "build", "--platform", "linux/amd64", "--file"]
    assert build[-1] == "/context" and build[5] == "/context/proofs/delivery/Sandbox.Dockerfile"
    assert container[:5] == ["docker", "run", "--rm", "--platform", "linux/amd64"]
    for flag, value in (("--memory", "11g"), ("--memory-swap", "11g"), ("--pids-limit", "64")):
        assert container[container.index(flag) + 1] == value
    mounts = [container[index + 1] for index, value in enumerate(container) if value == "--mount"]
    assert mounts == [f"type=bind,source={source},target={destination}" for source, destination in (
        (ROOT, "/work"), (ARTIFACTS / "cargo", "/cargo"), (ARTIFACTS / "risc0", "/risc0"), (ARTIFACTS / "canonical", "/artifacts"))]
    assert build[-2] == container[-4]
    assert container[-3:] == ["bash", "-c", "python3 scripts/install-delivery-toolchain.py && python3 scripts/check-delivery.py"]
    assert not {"--env", "--env-file", "-e", "--privileged"}.intersection(container)
    with patch(f"{__name__}.FIXTURES", ROOT / ".artifacts/no-public-proof-fixture"), patch(f"{__name__}.load_fixtures") as loader:
        assert optional_fixtures() is None
        loader.assert_not_called()
    with patch(f"{__name__}.FIXTURES", ROOT), patch(f"{__name__}.load_fixtures", side_effect=ValueError("missing fixture")), patch(f"{__name__}.run") as operation:
        try:
            sandbox()
        except ValueError:
            pass
        else:
            raise AssertionError("A broken fixture must stop sandbox dispatch")
        operation.assert_not_called()
    print("Delivery fixture, product profile, receipt export and sandbox command self-tests passed; no Docker job or cryptographic proof ran")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--check-fixtures", action="store_true")
    mode.add_argument("--sandbox", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
    elif args.check_fixtures:
        if optional_fixtures():
            print("Public fixture structure and schema checked; proof verification has not run")
    elif args.sandbox:
        sandbox()
    else:
        check()
