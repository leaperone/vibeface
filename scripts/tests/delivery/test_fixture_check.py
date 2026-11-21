#!/usr/bin/env python3
"""AUTO-23 offline delivery fixture and sandbox contract cases.

Migration map: ``check-delivery.py:self_test`` → ``fixture_check`` and
``check-delivery.py:continuation_self_test`` → the continuation case.
"""

from __future__ import annotations

import argparse
import contextlib
import errno
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import platform
import re
import runpy
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import types
import uuid
import zipfile
from unittest import TestCase
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[2]
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

def _load_subject():
    spec = importlib.util.spec_from_file_location("check_delivery_selftest_target", SCRIPTS / "check-delivery.py")
    if spec is None or spec.loader is None:
        raise ImportError("check-delivery self-test target cannot be loaded")
    subject = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(subject)
    return subject

from tests.delivery.test_fixture_continuation import run_continuation

def self_test_fixture(subject):
    from copy import deepcopy
    from unittest.mock import patch
    manifest = {"schemaVersion": 1, "imageId": "11" * 32, "schemaHash": "22" * 32, "elfSha256": "33" * 32,
                "proofs": [{"receipt": f"relation-{relation}.receipt", "public": f"relation-{relation}.json"} for relation in range(1, 5)]}
    compiled = {key: manifest[key] for key in ("imageId", "schemaHash", "elfSha256")}
    compiled["statementBytes"] = subject.STATEMENT_BYTES
    proofs = []
    for relation in range(1, 5):
        journal = bytearray(subject.STATEMENT_BYTES)
        journal[32:64] = relation.to_bytes(32, "big")
        journal[4 * 32:5 * 32] = b"\x44" * 32
        journal[5 * 32:6 * 32] = b"\x11" * 32
        proofs.append({"mode": "prove", "kind": "groth16", "relation": relation, "imageId": manifest["imageId"], "profileId": "44" * 32,
                       "journal": journal.hex(), "journalHash": hashlib.sha256(journal).hexdigest(), "seal": "55" * 260})
    subject.checked_metadata(manifest, proofs, compiled)

    def reject(value, data, image=compiled):
        try:
            subject.checked_metadata(value, data, image)
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
    profile = {**subject.PROFILE_CAPS, "guest_image_id": list(subject.hex_bytes(compiled["imageId"])), "statement_schema_hash": list(subject.hex_bytes(compiled["schemaHash"]))}
    profile_abi = bytes(9 * 32)
    expected_profile = hashlib.sha256(profile_abi).hexdigest()
    vectors = {"profile": profile, "schemaHash": compiled["schemaHash"], "profileAbi": profile_abi.hex(), "profileId": expected_profile}
    profile_proofs = deepcopy(proofs)
    for proof in profile_proofs:
        proof["profileId"] = expected_profile
        journal = bytearray(subject.hex_bytes(proof["journal"]))
        journal[4 * 32:5 * 32] = subject.hex_bytes(expected_profile)
        proof["journal"], proof["journalHash"] = journal.hex(), hashlib.sha256(journal).hexdigest()
    subject.checked_metadata(manifest, profile_proofs, compiled)
    subject.checked_profile_vectors(vectors, profile, compiled, profile_proofs)
    for field, value in (("max_windows", 16), ("max_total_bytes", 1_048_576), ("crypto_suite", True), ("guest_image_id", [102] * 32), ("statement_schema_hash", [102] * 32), ("extra", 1)):
        changed = deepcopy(profile)
        changed[field] = value
        with TestCase().assertRaises(ValueError, msg='Unsupported product profiles must fail'):
            subject.checked_profile_vectors(vectors, changed, compiled, profile_proofs)
    changed = deepcopy(profile_proofs)
    for proof in changed:
        proof["profileId"] = "66" * 32
        journal = bytearray(subject.hex_bytes(proof["journal"]))
        journal[4 * 32:5 * 32] = b"\x66" * 32
        proof["journal"], proof["journalHash"] = journal.hex(), hashlib.sha256(journal).hexdigest()
    subject.checked_metadata(manifest, changed, compiled)
    with TestCase().assertRaises(ValueError, msg='Self-consistent receipt profiles outside the product trust root must fail'):
        subject.checked_profile_vectors(vectors, profile, compiled, changed)
    verified = {"verified": True, "kind": "groth16", "seal": proofs[0]["seal"], "imageId": compiled["imageId"], "journalHash": proofs[0]["journalHash"]}
    subject.checked_verification(verified, compiled, proofs[0])
    for field, value in (("kind", "succinct"), ("seal", "66" * 260), ("imageId", "66" * 32), ("journalHash", "66" * 32), ("verified", 1), ("extra", "unexpected")):
        changed = dict(verified, **{field: value})
        with TestCase().assertRaises(ValueError, msg='A mismatched receipt export must fail'):
            subject.checked_verification(changed, compiled, proofs[0])
    build, container = subject.sandbox_commands(Path("/context"))
    assert build[:5] == ["docker", "build", "--platform", "linux/amd64", "--file"]
    assert build[-1] == "/context" and build[5] == "/context/crates/delivery/proof/Sandbox.Dockerfile"
    assert container[:5] == ["docker", "run", "--rm", "--platform", "linux/amd64"]
    for flag, value in (("--memory", "11g"), ("--memory-swap", "11g"), ("--pids-limit", "64")):
        assert container[container.index(flag) + 1] == value
    mounts = [container[index + 1] for index, value in enumerate(container) if value == "--mount"]
    assert mounts == [f"type=bind,source={source},target={destination}" for source, destination in (
        (subject.ROOT, "/work"), (subject.ARTIFACTS / "cargo", "/cargo"), (subject.ARTIFACTS / "cargo", "/home/runner/.cargo"), (subject.ARTIFACTS / "risc0", "/risc0"), (subject.ARTIFACTS / "canonical", "/artifacts"))]
    assert build[-2] == container[-4]
    _, shared = subject.sandbox_commands(Path("/context"), cache=Path("/shared"))
    assert [shared[index + 1] for index, value in enumerate(shared) if value == "--mount"][1:] == [f"type=bind,source=/shared/{source},target={destination}" for source, destination in (
        ("cargo", "/cargo"), ("cargo", "/home/runner/.cargo"), ("risc0", "/risc0"))] + [f"type=bind,source={subject.ARTIFACTS / 'canonical'},target=/artifacts",
        "type=bind,source=/shared/risc0,target=/work/.artifacts/vdp/risc0"]
    with tempfile.TemporaryDirectory(prefix="target-share-", dir=subject.ARTIFACTS) as directory:
        base = Path(directory)
        built, shared_target, worktree = base / "built", base / "shared", base / "worktree"
        (built / "release").mkdir(parents=True)
        (built / "release/prover").write_text("first")
        os.utime(built / "release/prover", (1_000_000, 1_000_000))
        subject.publish_target(built, shared_target)
        subject.seed_target(worktree, shared_target)
        assert (worktree / "release/prover").read_text() == "first"
        assert all(path.stat().st_mtime == 0 for path in (worktree, worktree / "release", worktree / "release/prover"))
        assert (shared_target / "release/prover").stat().st_mtime == 1_000_000
        (built / "release/prover").write_text("second")
        subject.publish_target(built, shared_target)
        subject.seed_target(worktree, shared_target)
        assert (shared_target / "release/prover").read_text() == "second" and (worktree / "release/prover").read_text() == "first"
        with patch.object(subject, "clone_tree", side_effect=subprocess.CalledProcessError(1, ["cp"])):
            subject.publish_target(worktree, shared_target)
            subject.seed_target(base / "cold", shared_target)
        assert (shared_target / "release/prover").read_text() == "second" and not (base / "cold").exists()
        real_clone = subject.clone_tree

        def clone_during_publish(source, destination):
            real_clone(source, destination)
            if source == shared_target:
                subject.publish_target(built, shared_target)

        with patch.object(subject, "clone_tree", side_effect=clone_during_publish):
            subject.seed_target(base / "raced", shared_target)
        assert not (base / "raced").exists()
        with patch.object(subject, "clone_tree", side_effect=InterruptedError("Delivery sandbox interrupted")):
            for share in (subject.publish_target, lambda target, shared: subject.seed_target(base / "cold", shared)):
                with TestCase().assertRaises(InterruptedError, msg="Sharing must not swallow a SIGTERM interruption"):
                    share(worktree, shared_target)
        assert sorted(path.name for path in base.iterdir()) == ["built", "shared", "worktree"]
    assert container[-3:] == ["bash", "-c", "python3 scripts/install-delivery-toolchain.py && python3 scripts/check-delivery.py"]
    env_pairs = [container[index + 1] for index, value in enumerate(container) if value == "--env"]
    assert env_pairs == ["HOME=/home/runner", "PYTHONDONTWRITEBYTECODE=1"]
    assert not {"--env-file", "-e", "--privileged"}.intersection(container)
    deadline = str(int(time.time() * 1000) + 100000)
    with patch.dict(os.environ, {subject.DEADLINE_ENV: deadline}), patch.object(subject, "source_commit", return_value="a" * 40):
        _, small = subject.sandbox_commands(Path("/context"), small="relation-2", work_id="b" * 32)
    assert small[-1].endswith("scripts/check-delivery.py --prove-small relation-2")
    assert subject.DEADLINE_ENV + "=" + deadline in small
    assert not any("TOKEN" in value or "SECRET" in value for value in small)
    assert "--resume-run" not in small[-1]
    assert small[small.index("--memory") + 1] == small[small.index("--memory-swap") + 1] == "11g"
    assert "TOKIO_WORKER_THREADS=2" in small
    assert "run-delivery-live.py --inside" not in small[-1]
    with patch.object(subject, "FIXTURES", subject.ROOT / ".artifacts/no-public-proof-fixture"), patch.object(subject, "load_fixtures") as loader:
        assert subject.optional_fixtures() is None
        loader.assert_not_called()
    with patch.object(subject, "FIXTURES", subject.ROOT), patch.object(subject, "load_fixtures", side_effect=ValueError("missing fixture")), patch.object(subject, "run") as operation:
        with TestCase().assertRaises(ValueError, msg='A broken fixture must stop sandbox dispatch'):
            subject.sandbox()
        operation.assert_not_called()
    with patch.object(subject, "optional_fixtures", return_value=None), patch.object(subject.subprocess, "check_output", return_value=b" M uncommitted-source"), patch.object(subject, "run") as operation:
        try:
            subject.sandbox(small="prepare")
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
            with patch.dict(os.environ, {"GITHUB_ACTIONS": "false"}), patch.object(subject, "optional_fixtures", return_value=None), patch.object(subject, "run", side_effect=execute) as operation, \
                    patch.object(subject, "seed_target") as seeded, patch.object(subject, "publish_target") as published, \
                    patch("subprocess.run", side_effect=[result, stop_error or subprocess.CompletedProcess([], 0)]) as cleanup:
                caught = None
                try:
                    subject.sandbox()
                except BaseException as error:
                    caught = error
                if isinstance(expected, type):
                    assert isinstance(caught, expected), "Sandbox interruption or cleanup must propagate the expected failure"
                else:
                    assert caught is expected
                assert seeded.call_count == 2 and published.call_count == (2 if failure is None else 0)
                assert signal.getsignal(signal.SIGTERM) is prior_term
                assert operation.call_count == 2
                owned = operation.call_args.args[0]
                name = owned[owned.index("--name") + 1]
                assert name.startswith("vdp-") and len(name.rsplit("-", 1)[1]) == 32
                assert owned[owned.index("--label") + 1] == "vibeface.task-root=" + str(subject.ROOT)
                assert owned.count("--name") == 1 and "--rm" in owned
                assert cleanup.call_args_list[0].args[0] == ["docker", "ps", "--all", "--no-trunc", "--filter", "name=^/" + re.escape(name) + "$",
                    "--filter", "label=vibeface.task-root=" + str(subject.ROOT), "--format", "{{.ID}}"]
                assert cleanup.call_args_list[0].kwargs == {"capture_output": True, "text": True, "check": True, "timeout": 20}
                if lookup == owned_id:
                    assert cleanup.call_count == 2
                    assert cleanup.call_args_list[1].args[0] == ["docker", "stop", "--time", "10", owned_id]
                    assert cleanup.call_args_list[1].kwargs == {"capture_output": True, "check": True, "timeout": 30}
                else:
                    assert cleanup.call_count == 1
        with patch.dict(os.environ, {"GITHUB_ACTIONS": "true"}), patch.object(subject, "optional_fixtures", return_value=None), patch.object(subject, "run"), \
                patch.object(subject, "seed_target") as seeded, patch.object(subject, "publish_target") as published, \
                patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0, stdout="")):
            subject.sandbox()
        assert seeded.call_count == published.call_count == 0, "CI runners have no sibling worktree to share targets with"
    finally:
        signal.signal(signal.SIGTERM, previous_term)
    return manifest, compiled, profile, vectors, profile_proofs




CASE_REGISTRY = (("fixture_check", self_test_fixture), ("continuation", run_continuation))

def run(subject=None):
    if subject is None:
        subject = _load_subject()
    if not isinstance(subject, types.ModuleType):
        raise TypeError("check-delivery self-tests require a ModuleType subject")
    subject.ARTIFACTS.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="shared-cache-self-test-", dir=subject.ARTIFACTS) as cache, patch.object(subject, "shared_cache", return_value=Path(cache)):
        manifest, compiled, profile, vectors, proofs = CASE_REGISTRY[0][1](subject)
        CASE_REGISTRY[1][1](subject, manifest, compiled, profile, vectors, proofs)
    print("Delivery fixture, profile, continuation provenance, public export, resources and sandbox self-tests passed; no network, Docker or cryptographic proof ran")

def self_test(subject=None):
    return run(subject)

if __name__ == "__main__":
    run()
