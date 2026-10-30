#!/usr/bin/env python3
"""Rebuild the delivery guest, verify its public receipts, and exercise the EVM adapter."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import runpy
import shutil
import signal
import subprocess
import tempfile
import uuid


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


def read_fixture(name, limit, directory=None):
    file = (directory or FIXTURES) / name
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


def load_fixtures(directory=None):
    manifest = json.loads(read_fixture("manifest.json", 16 * 1024, directory))
    proofs = [json.loads(read_fixture(f"relation-{relation}.json", 32 * 1024, directory)) for relation in range(1, 5)]
    checked_metadata(manifest, proofs)
    schema = (ROOT / "crates/delivery/abi/Statement.sol").read_bytes()
    if manifest["schemaHash"] != hashlib.sha256(schema).hexdigest():
        raise ValueError("Delivery fixtures use an outdated canonical ABI schema")
    load_profile(manifest)
    for relation in range(1, 5):
        if not read_fixture(f"relation-{relation}.receipt", 16 * 1024 * 1024, directory):
            raise ValueError("An empty public receipt is not a proof")
    return manifest, proofs


def optional_fixtures():
    if not FIXTURES.exists() and not FIXTURES.is_symlink():
        print("skipped: no public proof fixture; receipt verification needs fixtures generated on an x86_64 machine with enough memory", flush=True)
        return None
    return load_fixtures()


def run(command, **kwargs):
    print("RUN delivery " + " ".join(str(part) for part in command[:2]), flush=True)
    return subprocess.run(command, cwd=ROOT, check=True, **kwargs)


def checked_verification(result, compiled, proof):
    expected = {"verified": True, "kind": "groth16", "seal": proof["seal"], "imageId": compiled["imageId"], "journalHash": proof["journalHash"]}
    if result != expected or result["verified"] is not True:
        raise ValueError("Receipt verification result or exported seal differs from its public fixture")


def sandbox_commands(context, small=False):
    image = "vibeface-vdp-proof:check-" + hashlib.sha256(str(ROOT).encode()).hexdigest()[:12]
    build = ["docker", "build", "--platform", "linux/amd64", "--file", str(context / "proofs/delivery/Sandbox.Dockerfile"), "--tag", image, str(context)]
    container = ["docker", "run", "--rm", "--platform", "linux/amd64", "--memory", "11g", "--memory-swap", "11g", "--pids-limit", "64", "--env", "HOME=/home/runner"]
    for source, destination in ((ROOT, "/work"), (ARTIFACTS / "cargo", "/cargo"), (ARTIFACTS / "cargo", "/home/runner/.cargo"), (ARTIFACTS / "risc0", "/risc0"), (ARTIFACTS / "canonical", "/artifacts")):
        container.extend(["--mount", f"type=bind,source={source},target={destination}"])
    if small:
        commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        hex_bytes(commit, 20)
        container.extend(["--env", "TOKIO_WORKER_THREADS=2", "--env", "VIBEFACE_DELIVERY_SOURCE_COMMIT=" + commit])
    container.extend([image, "bash", "-c", "python3 scripts/install-delivery-toolchain.py && python3 scripts/check-delivery.py" + (" --prove-small" if small else "")])
    return build, container


def sandbox(small=False):
    optional_fixtures()
    if small and subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=ROOT).strip():
        raise ValueError("Small experiments require a clean committed source checkout")
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
        build, container = sandbox_commands(context, small=small)
        run(build)
    task = "".join(character if character.isascii() and (character.isalnum() or character in "_.-") else "-" for character in ROOT.name)[:64]
    name = "vdp-" + task + "-" + uuid.uuid4().hex
    container[2:2] = ["--name", name, "--label", "vibeface.task-root=" + str(ROOT)]
    print("Delivery sandbox owned by this task: " + name, flush=True)
    previous_term = signal.getsignal(signal.SIGTERM)

    def interrupted(*_):
        raise InterruptedError("Delivery sandbox interrupted")

    try:
        signal.signal(signal.SIGTERM, interrupted)
        run(container)
    finally:
        try:
            result = subprocess.run(["docker", "ps", "--all", "--no-trunc", "--filter", "name=^/" + re.escape(name) + "$",
                "--filter", "label=vibeface.task-root=" + str(ROOT), "--format", "{{.ID}}"], capture_output=True, text=True, check=True, timeout=20)
            container_id = result.stdout.strip()
            if container_id:
                if len(container_id) != 64 or any(character not in "0123456789abcdef" for character in container_id):
                    raise ValueError("Delivery sandbox cleanup requires one full owned container ID")
                subprocess.run(["docker", "stop", "--time", "10", container_id], capture_output=True, check=True, timeout=30)
        finally:
            signal.signal(signal.SIGTERM, previous_term)


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


def publish_small_results(public, verified_bundle=None):
    directories = [public.parent.parent, public.parent, public]
    for directory in directories:
        if directory.is_symlink() or not directory.is_dir():
            raise ValueError("Public experiment directories must not be symlinks")
    files = []
    for name, limit in (("summary.json", 2 * 1024 * 1024), ("resources.json", 64 * 1024)):
        file = public / name
        if file.exists() or file.is_symlink():
            read_fixture(name, limit, public)
            if file.stat().st_nlink != 1:
                raise ValueError("Public experiment files must not have other hard links")
            files.append(file)
    for directory in directories:
        directory.chmod(0o755)
    for file in files:
        file.chmod(0o644)
    if verified_bundle is not None:
        proofs = public / "proofs"
        proofs.mkdir(mode=0o755)
        proofs.chmod(0o755)
        for name in ("manifest.json", *[f"relation-{relation}.{suffix}" for relation in range(1, 5) for suffix in ("json", "receipt")]):
            shutil.copyfile(verified_bundle / name, proofs / name)
            (proofs / name).chmod(0o644)


def prove_small():
    live = runpy.run_path(str(ROOT / "scripts/run-delivery-live.py"))
    _, initial = live["resources"]()
    destination = Path("/artifacts/small") / uuid.uuid4().hex
    destination.parent.mkdir(parents=True, exist_ok=True)
    public = destination / "public"
    status, failure, verified_bundle = "FAIL", None, None
    try:
        check()
        run(["node", "contracts/delivery/experiment-delivery.mjs", str(destination)])
        compiled = json.loads((ARTIFACTS / "host-info.json").read_bytes())
        directory = destination / "private/proofs"
        manifest, proofs = load_fixtures(directory)
        expected = {"manifest.json", *[f"relation-{relation}.{suffix}" for relation in range(1, 5) for suffix in ("json", "receipt")]}
        if {file.name for file in directory.iterdir()} != expected:
            raise ValueError("Small experiment export must contain exactly nine public proof files")
        checked_metadata(manifest, proofs, compiled)
        checked_profile_vectors(json.loads((ARTIFACTS / "public-vectors.json").read_bytes()), load_profile(compiled), compiled, proofs)
        prover = "/artifacts/proof-target/release/vibeface-delivery-prover"
        for relation, proof in enumerate(proofs, 1):
            value = json.loads(run([prover, "verify", proof["journal"]], input=read_fixture(f"relation-{relation}.receipt", 16 * 1024 * 1024, directory), capture_output=True).stdout)
            checked_verification(value, compiled, proof)
        summary = json.loads((public / "summary.json").read_bytes())
        if summary["status"] != "PASS" or len(summary["proofs"]) != 4 or len(summary["branches"]) != 3 or summary["corruptERejectedWithoutOutput"] is not True:
            raise ValueError("Small experiment did not complete proofs, settlement and recovery")
        verified_bundle = directory
        status = "PASS"
    except Exception as error:
        failure = error
    finally:
        public.mkdir(parents=True, exist_ok=True)
        try:
            _, final = live["resources"]()
            delta = live["oom_delta"](initial, final)
            if any(delta.values()):
                status = "FAIL"
                failure = failure or ValueError("Small experiment incurred kernel OOM events")
            resources = {"status": status, "scope": "entire bounded sandbox including build, EVM, executor, prover and compression", "initial": initial, "final": final, "oomDelta": delta}
        except Exception as error:
            status = "FAIL"
            failure = failure or error
            resources = {"status": "UNKNOWN", "initial": initial, "final": None, "oomDelta": None}
        with (public / "resources.json").open("x") as output:
            json.dump(resources, output, indent=2)
            output.write("\n")
        publish_small_results(public, verified_bundle)
        print(f"Small experiment {status}: {public}", flush=True)
    if failure:
        raise failure


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
        (ROOT, "/work"), (ARTIFACTS / "cargo", "/cargo"), (ARTIFACTS / "cargo", "/home/runner/.cargo"), (ARTIFACTS / "risc0", "/risc0"), (ARTIFACTS / "canonical", "/artifacts"))]
    assert build[-2] == container[-4]
    assert container[-3:] == ["bash", "-c", "python3 scripts/install-delivery-toolchain.py && python3 scripts/check-delivery.py"]
    env_pairs = [container[index + 1] for index, value in enumerate(container) if value == "--env"]
    assert env_pairs == ["HOME=/home/runner"]
    assert not {"--env-file", "-e", "--privileged"}.intersection(container)
    _, small = sandbox_commands(Path("/context"), small=True)
    assert small[-1].endswith("scripts/check-delivery.py --prove-small")
    assert small[small.index("--memory") + 1] == small[small.index("--memory-swap") + 1] == "11g"
    assert "TOKIO_WORKER_THREADS=2" in small
    assert "run-delivery-live.py --inside" not in small[-1]
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
    with patch(f"{__name__}.optional_fixtures", return_value=None), patch(f"{__name__}.subprocess.check_output", return_value=b" M uncommitted-source"), patch(f"{__name__}.run") as operation:
        try:
            sandbox(small=True)
        except ValueError as error:
            assert "clean committed source" in str(error)
        else:
            raise AssertionError("Uncommitted sources must not be attributed to HEAD in a proof experiment")
        operation.assert_not_called()
    owned_id = "a" * 64
    failed = ValueError("sandbox failed")
    interrupted = KeyboardInterrupt()
    lookup_failed = subprocess.TimeoutExpired(["docker", "ps"], 20)
    stop_failed = subprocess.CalledProcessError(1, ["docker", "stop"])
    previous_term = signal.getsignal(signal.SIGTERM)

    def prior_term(*_):
        raise AssertionError("The prior SIGTERM handler must remain inactive during sandbox dispatch")

    signal.signal(signal.SIGTERM, prior_term)
    try:
        for failure, lookup, stop_error, expected in (
            ("SIGTERM", owned_id, None, InterruptedError),
            (interrupted, owned_id, None, interrupted),
            (failed, owned_id, None, failed),
            (failed, "", None, failed),
            (None, "", None, None),
            (None, owned_id, stop_failed, stop_failed),
            ("SIGTERM", lookup_failed, None, lookup_failed),
            (None, "invalid-owned-id", None, ValueError),
        ):
            def execute(command):
                if command[:2] == ["docker", "run"]:
                    if failure == "SIGTERM":
                        handler = signal.getsignal(signal.SIGTERM)
                        assert callable(handler) and handler is not prior_term
                        handler(signal.SIGTERM, None)
                    elif failure is not None:
                        raise failure

            result = lookup if isinstance(lookup, BaseException) else subprocess.CompletedProcess([], 0, stdout=lookup + "\n")
            with patch(f"{__name__}.optional_fixtures", return_value=None), patch(f"{__name__}.run", side_effect=execute) as operation, \
                    patch("subprocess.run", side_effect=[result, stop_error or subprocess.CompletedProcess([], 0)]) as cleanup:
                caught = None
                try:
                    sandbox()
                except BaseException as error:
                    caught = error
                if isinstance(expected, type):
                    assert isinstance(caught, expected), "Sandbox interruption or cleanup must propagate the expected failure"
                else:
                    assert caught is expected
                assert signal.getsignal(signal.SIGTERM) is prior_term
                assert operation.call_count == 2
                owned = operation.call_args.args[0]
                name = owned[owned.index("--name") + 1]
                assert name.startswith("vdp-") and len(name.rsplit("-", 1)[1]) == 32
                assert owned[owned.index("--label") + 1] == "vibeface.task-root=" + str(ROOT)
                assert owned.count("--name") == 1 and "--rm" in owned
                assert cleanup.call_args_list[0].args[0] == ["docker", "ps", "--all", "--no-trunc", "--filter", "name=^/" + re.escape(name) + "$",
                    "--filter", "label=vibeface.task-root=" + str(ROOT), "--format", "{{.ID}}"]
                assert cleanup.call_args_list[0].kwargs == {"capture_output": True, "text": True, "check": True, "timeout": 20}
                if lookup == owned_id:
                    assert cleanup.call_count == 2
                    assert cleanup.call_args_list[1].args[0] == ["docker", "stop", "--time", "10", owned_id]
                    assert cleanup.call_args_list[1].kwargs == {"capture_output": True, "check": True, "timeout": 30}
                else:
                    assert cleanup.call_count == 1
    finally:
        signal.signal(signal.SIGTERM, previous_term)
    with tempfile.TemporaryDirectory(prefix="public-export-self-test-", dir=ARTIFACTS) as directory:
        base = Path(directory) / "run"
        public = base / "public"
        private = base / "private"
        for path in (base, public, private):
            path.mkdir(mode=0o700)
        for path in (public / "summary.json", private / "witness.json", public / "not-public.json"):
            path.write_text("{}\n")
            path.chmod(0o600)
        publish_small_results(public)
        assert all(path.stat().st_mode & 0o777 == 0o755 for path in (base, public))
        assert (public / "summary.json").stat().st_mode & 0o777 == 0o644
        assert all(path.stat().st_mode & 0o777 == 0o600 for path in (private / "witness.json", public / "not-public.json"))
        assert private.stat().st_mode & 0o777 == 0o700
        bundle = private / "verified-bundle-layout"
        bundle.mkdir(mode=0o700)
        names = {"manifest.json", *[f"relation-{relation}.{suffix}" for relation in range(1, 5) for suffix in ("json", "receipt")]}
        for name in names:
            (bundle / name).write_bytes(b"public-file-layout-only")
            (bundle / name).chmod(0o600)
        previous_umask = os.umask(0o077)
        try:
            publish_small_results(public, bundle)
        finally:
            os.umask(previous_umask)
        assert all(path.stat().st_mode & 0o777 == 0o755 for path in (base.parent, base, public, public / "proofs"))
        assert {file.name for file in (public / "proofs").iterdir()} == names
        for name in names:
            assert (public / "proofs" / name).read_bytes() == b"public-file-layout-only"
            assert (public / "proofs" / name).stat().st_mode & 0o777 == 0o644
            assert (bundle / name).stat().st_mode & 0o777 == 0o600
        (public / "summary.json").unlink()
        for hard_link in (False, True):
            if hard_link:
                os.link(private / "witness.json", public / "summary.json")
            else:
                (public / "summary.json").symlink_to(private / "witness.json")
            try:
                publish_small_results(public)
            except ValueError:
                pass
            else:
                raise AssertionError("Private data linked into the public allowlist must be rejected")
            assert (private / "witness.json").stat().st_mode & 0o777 == 0o600
            (public / "summary.json").unlink()
    print("Delivery fixture, profile, receipt export, public permissions and sandbox self-tests passed; no Docker job or cryptographic proof ran")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--check-fixtures", action="store_true")
    mode.add_argument("--prove-small", action="store_true")
    parser.add_argument("--sandbox", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
    elif args.check_fixtures:
        if optional_fixtures():
            print("Public fixture structure and schema checked; proof verification has not run")
    elif args.sandbox:
        sandbox(small=args.prove_small)
    elif args.prove_small:
        prove_small()
    else:
        check()
