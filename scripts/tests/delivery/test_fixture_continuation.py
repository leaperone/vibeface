#!/usr/bin/env python3
"""AUTO-23 continuation provenance and public publication cases."""

from __future__ import annotations

import argparse
import contextlib
import errno
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import uuid
import zipfile
from unittest import TestCase

def run_continuation(subject, manifest, compiled, profile, vectors, proofs):
    if subject is None:
        from tests.delivery.test_fixture_check import _load_subject
        subject = _load_subject()
    from copy import deepcopy
    from unittest.mock import patch
    commit = "a" * 40
    producer = {"repository": "test/repository", "workflowPath": subject.WORKFLOW_PATH, "event": "workflow_dispatch", "runId": 101, "runAttempt": 2, "headSha": commit}
    snapshot = {"memory.max": str(11 * 1024 ** 3), "memory.swap.max": "0", "memory.current": "100", "memory.peak": "200", "memory.events": "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0", "pids.max": "64", "pids.current": "1"}
    resources = {"schemaVersion": 1, "status": "PASS", "scope": subject.RESOURCE_SCOPE, "initial": dict(snapshot), "final": dict(snapshot), "oomDelta": {"oom": 0, "oom_kill": 0, "oom_group_kill": 0}}
    process = {"label": "prove-1", "exitCode": 0, "elapsedMs": 1.5, "sandboxCpuUsec": 1, "before": {"cpu.stat": "usage_usec 1"}, "after": {"cpu.stat": "usage_usec 2"}, "stderrSha256": "0" * 64, "diagnosticMarkers": [], "succinctCheckpointBytes": 1}
    addresses = ["0x" + str(number) * 40 for number in range(1, 9)]
    block = lambda number, timestamp: {"number": number, "hash": "0x" + "1" * 64, "timestamp": timestamp}
    transaction = lambda number, nonce: {"hash": "0x" + "2" * 64, "blockNumber": number, "blockHash": "0x" + "1" * 64, "gasUsed": "1", "nonce": nonce}
    context = {"schemaVersion": 1, "sourceCommit": commit, "recipe": "delivery-small-public-test-v1", "guest": compiled,
        "profileId": vectors["profileId"], "profileSha256": hashlib.sha256(subject.PROFILE.read_bytes()).hexdigest(),
        "file": {"bytes": 4096, "sha256": hashlib.sha256(bytes((index * 37 + 19) % 251 for index in range(4096))).hexdigest()},
        "deployment": {"chainId": 31337, "confirmations": 2, "accounts": addresses[:4], "escrow": addresses[4], "asset": addresses[5], "adapter": addresses[6], "verifier": addresses[7], "payer": addresses[0], "provider": addresses[1], "tokenDecimals": 6, "funderNonce": 2,
            "mining": {"genesisTimestamp": 1700000000, "timestampIncrement": 1, "blockTime": 0, "gasPriceWei": "2000000000", "coinbase": "0x" + "0" * 40},
            "genesisBlock": block(0, 1700000000), "runtimeCodeHashes": {field: "0x" + "3" * 64 for field in ("escrow", "asset", "adapter", "verifier")},
            "baselineBlock": block(4, 1700000004), "baselineBalances": {"nativeWei": {"payer": "1", "provider": "1", "escrow": "0"}, "tokenAtoms": {"payer": "1000000000000", "provider": "0", "escrow": "0"}},
            "deploymentTransactions": {"asset": transaction(1, 0), "escrow": transaction(3, 1)}},
        "orderId": "4" * 64, "expectedJournals": [{field: proof[field] for field in ("relation", "journal", "journalHash")} for proof in proofs]}
    context_data = subject.json_bytes(context)
    context_hash = hashlib.sha256(context_data).hexdigest()
    wrongs = [{"relation": proof["relation"], "journal": ("00" if proof["journal"][:2] != "00" else "01") + proof["journal"][2:]} for proof in proofs]
    controls = [{"relation": proof["relation"], "cycles": 10, "segments": 1, "journalHash": proof["journalHash"], "executed": True, "proved": False, "originalJournalDecoderEof": True, "wrongJournalDecoderEof": True} for proof in proofs]
    node_summary = {"schemaVersion": 1, "sourceCommit": commit, "workItem": "prepare", "status": "PREPARED", "stage": "complete", "guest": compiled, "file": context["file"], "contextSha256": context_hash,
        "executionControls": controls, "legacyImagePreTermsRejected": True, "wrongJournals": wrongs, "reused": False, "replayChecks": 2, "proofs": [], "branches": [], "transactions": [], "processes": [],
        "limits": {"memoryBytes": 11 * 1024 ** 3, "swapBytes": 0, "pids": 64}, "resourceScope": "CPU deltas cover each entire sandbox interval; memory.peak is cumulative sandbox lifetime, not per-process RSS",
        "coverage": "synthetic raw 4096-byte file, deterministic public test keys and controlled Ganache branches; no product Node or durable-publication-policy coverage; raw late/reorg controls intentionally exceed its gate; no UI, mobile, real asset, or public-chain finality", "deployment": None, "corruptERejectedWithoutOutput": False, "failure": None}

    def must_reject(call):
        try:
            call()
        except (ValueError, TypeError, KeyError, OSError, zipfile.BadZipFile):
            return
        raise AssertionError("Invalid continuation input must be rejected")

    subject.checked_context(context_data, compiled, vectors["profileId"], commit)
    subject.checked_node_summary(node_summary, "prepare", context, commit)
    producer_coverage = re.search(r"^  coverage: '([^'\n]*)',$", (subject.ROOT / "contracts/delivery/experiment-delivery.mjs").read_text(), re.M)[1]
    subject.checked_node_summary({**node_summary, "coverage": producer_coverage}, "prepare", context, commit)
    subject.checked_resources(resources, require_pass=True)
    subject.checked_counters({"cpu.stat": "usage_usec 1\ncore_sched.force_idle_usec 0"})
    must_reject(lambda: subject.checked_counters({"cpu.stat": "usage_usec 1\n.core_sched.force_idle_usec 0"}))
    must_reject(lambda: subject.checked_counters({"cpu.stat": "usage_usec 1\ncore_sched.force_idle_usec -1"}))
    subject.checked_transaction({"label": "fund", "hash": "0x" + "1" * 64, "blockHash": "0x" + "2" * 64, "blockNumber": 5, "status": 1,
        "gasUsed": "1", "effectiveGasPrice": "2", "nativeCostWei": "2", "events": [{"name": "Funded", "args": ["0x" + "4" * 64, "0x" + "Ab" * 20, "17"]}]})
    budget, order = "0x" + "5" * 64, "0x" + context["orderId"]
    accepted_events = (("open-budget", [("BudgetOpened", [budget, addresses[0], addresses[1], "17"])]),
        ("reserve-batch", [("Funded", [order, addresses[0], "17"]), ("BudgetReserved", [budget, order, "17"])]),
        ("publish", [("WindowUpdated", [order, "0", "3"]), ("BudgetEarned", [budget, order, "0", "17"])]),
        ("withdraw", [("BudgetWithdrawn", [budget, addresses[1], "17"])]),
        ("close", [("BudgetClosed", [budget])]),
        ("refund", [("Refunded", [order, addresses[0], "17"]), ("BudgetRefunded", [budget, addresses[0], "17"])]))
    accepted = {**node_summary, "workItem": "accept", "status": "PASS-accept-candidate",
        "transactions": [{"label": label, "hash": "0x" + "1" * 64, "blockHash": "0x" + "2" * 64, "blockNumber": 5, "status": 1, "gasUsed": "1", "effectiveGasPrice": "2", "nativeCostWei": "2",
            "events": [{"name": name, "args": arguments} for name, arguments in events]} for label, events in accepted_events],
        "deployment": {**{field: context["deployment"][field] for field in ("chainId", "escrow", "asset", "payer", "provider", "adapter", "verifier", "tokenDecimals", "runtimeCodeHashes")},
            "profileId": "0x" + context["profileId"], "orderId": order, "budgetId": budget, "allocationTransaction": "0x" + "6" * 64}}
    subject.checked_node_summary(accepted, "accept", context, commit)
    must_reject(lambda: subject.checked_node_summary({**accepted, "deployment": {**{field: value for field, value in accepted["deployment"].items() if field not in ("budgetId", "allocationTransaction")}, "fundTransaction": "0x" + "6" * 64}}, "accept", context, commit))
    must_reject(lambda: subject.checked_node_summary({**accepted, "deployment": {**accepted["deployment"], "budgetId": "5" * 64}}, "accept", context, commit))
    must_reject(lambda: subject.checked_transaction({**accepted["transactions"][0], "events": [{"name": "Withdrawn", "args": [order, addresses[1], "17"]}]}))
    must_reject(lambda: subject.checked_transaction({**accepted["transactions"][0], "events": [{"name": "BudgetClosed", "args": [budget, "17"]}]}))
    for field, value in (("sourceCommit", "b" * 40), ("profileId", "f" * 64), ("profileSha256", "f" * 64), ("orderId", "0x" + "4" * 64), ("secret", "private")):
        changed = {**context, field: value}
        must_reject(lambda: subject.checked_context(subject.json_bytes(changed), compiled, vectors["profileId"], commit))
    for path, value in ((["deployment", "mining", "blockTime"], True), (["file", "bytes"], 4097), (["deployment", "payer"], addresses[1]), (["expectedJournals", 0, "journalHash"], "f" * 64)):
        changed = deepcopy(context)
        target = changed
        for key in path[:-1]:
            target = target[key]
        target[path[-1]] = value
        must_reject(lambda: subject.checked_context(subject.json_bytes(changed), compiled, vectors["profileId"], commit))
    must_reject(lambda: subject.read_json_bytes(b'{"a":1,"a":2}'))
    must_reject(lambda: subject.checked_resources(subject.unknown_resources(), require_pass=True))
    oom = deepcopy(resources)
    oom["final"]["memory.events"] = oom["final"]["memory.events"].replace("oom 0", "oom 1")
    must_reject(lambda: subject.checked_resources(oom, require_pass=True))
    oom["status"], oom["oomDelta"]["oom"] = "FAIL", 1
    subject.checked_resources(oom)
    for field, value in (("extra", "secret"),
                         ("coverage", "synthetic raw 4096-byte file, deterministic public test keys and controlled Ganache branches; no production confidentiality, product Node, UI, mobile, real asset, or public-chain finality"),
                         ("coverage", "raw witness"), ("coverage", producer_coverage + " "), ("coverage", None),
                         ("processes", [{**process, "privateLog": "secret"}])):
        must_reject(lambda: subject.checked_node_summary({**node_summary, field: value}, "prepare", context, commit))

    def origin_for(item, artifact_id, attempt=2):
        return {**producer, "runAttempt": attempt, "artifactId": artifact_id,
            "artifactName": f"delivery-small-{item}-{commit}-101-{attempt}", "artifactDigest": "sha256:" + "0" * 64}

    assert subject.WORKFLOW_PATH == ".github/workflows/delivery-small.yml"
    subject.checked_producer(producer)
    for path in (".github/workflows/ci.yml", ".github/workflows/delivery-live.yml", ".github/workflows/delivery-small.yml/extra"):
        must_reject(lambda: subject.checked_producer({**producer, "workflowPath": path}))
    run_api = {"id": 101, "path": subject.WORKFLOW_PATH, "event": "workflow_dispatch", "head_sha": commit, "repository": {"full_name": producer["repository"]}, "head_repository": {"full_name": producer["repository"]}, "run_attempt": 2, "run_started_at": "2026-10-30T00:00:00Z"}
    subject.checked_run_api(run_api, producer, 101, 2)
    for field, value in (("path", "other.yml"), ("path", ".github/workflows/ci.yml"), ("path", ".github/workflows/delivery-live.yml"), ("event", "push"), ("head_sha", "b" * 40), ("run_attempt", 3)):
        must_reject(lambda: subject.checked_run_api({**run_api, field: value}, producer, 101, 2))
    artifact_api = {"id": 1, "name": origin_for("prepare", 1)["artifactName"], "digest": "sha256:" + "0" * 64, "size_in_bytes": 100, "expired": False,
        "expires_at": "2999-01-01T00:00:00Z", "created_at": "2026-10-30T00:01:00Z", "workflow_run": {"id": 101, "head_sha": commit}}
    with patch.object(subject, "gh_api", return_value=run_api):
        subject.artifact_origin(artifact_api, producer, 101)
        for field, value in (("expired", True), ("expires_at", "2000-01-01T00:00:00Z"), ("name", "delivery-small-latest"), ("created_at", "2000-01-01T00:00:00Z")):
            must_reject(lambda: subject.artifact_origin({**artifact_api, field: value}, producer, 101))
    with tempfile.TemporaryDirectory(prefix="continuation-self-test-", dir=subject.ARTIFACTS) as temporary:
        base = Path(temporary)
        pair = base / "private-pair"
        pair.mkdir(mode=0o700)
        for relation, proof in enumerate(proofs, 1):
            (pair / f"relation-{relation}.json").write_bytes(subject.json_bytes(proof))
            (pair / f"relation-{relation}.receipt").write_bytes(b"self-test-only-not-a-genuine-receipt")
        for file in pair.iterdir():
            file.chmod(0o600)
        def part_for(relation, original=None):
            name = f"relation-{relation}"
            return {"schemaVersion": 1, "status": "VERIFIED_PARTIAL", "sourceCommit": commit, "contextSha256": context_hash, "relation": relation, "journalHash": proofs[relation - 1]["journalHash"],
                "publicSha256": hashlib.sha256((pair / (name + ".json")).read_bytes()).hexdigest(), "receiptSha256": hashlib.sha256((pair / (name + ".receipt")).read_bytes()).hexdigest(),
                "wrongJournal": wrongs[relation - 1]["journal"], "producer": producer, "producerArtifact": original,
                "provingResources": {"sandbox": resources, "process": {**process, "label": f"prove-{relation}"}}, "verificationResources": resources}
        def verifier(command, **kwargs):
            proof = next(proof for proof in proofs if proof["journal"] == command[-1])
            return subprocess.CompletedProcess(command, 0, stdout=subject.json_bytes({"verified": True, "kind": "groth16", "seal": proof["seal"], "imageId": compiled["imageId"], "journalHash": proof["journalHash"]}))
        with patch.object(subject, "run", side_effect=verifier), patch("subprocess.run", return_value=subprocess.CompletedProcess([], 1, stderr=b"Error: receipt journal mismatch\n")):
            subject.verify_pair(pair, 1, context, compiled, wrongs[0]["journal"])
            with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 1, stderr=b"Error: io error: unexpected end of file\n")):
                must_reject(lambda: subject.verify_pair(pair, 1, context, compiled, wrongs[0]["journal"]))
        bad_proof = deepcopy(proofs[0])
        bad_proof["seal"] = "00"
        (pair / "relation-1.json").write_bytes(subject.json_bytes(bad_proof))
        must_reject(lambda: subject.verify_pair(pair, 1, context, compiled))
        (pair / "relation-1.json").write_bytes(subject.json_bytes(proofs[0]))
        origin = origin_for("relation-1", 2)
        part = part_for(1)
        subject.checked_part(part, pair, context, origin)
        for field, value in (("receiptSha256", "f" * 64), ("contextSha256", "f" * 64), ("extra", 1), ("verificationResources", subject.unknown_resources())):
            must_reject(lambda: subject.checked_part({**part, field: value}, pair, context, origin))
        (pair / "linked.json").symlink_to(pair / "relation-1.json")
        must_reject(lambda: subject.read_fixture("linked.json", 32 * 1024, pair))
        (pair / "linked.json").unlink()
        os.link(pair / "relation-1.json", pair / "linked.json")
        must_reject(lambda: subject.read_fixture("relation-1.json", 32 * 1024, pair))
        (pair / "linked.json").unlink()
        must_reject(lambda: subject.read_fixture("../private-pair/relation-1.json", 32 * 1024, pair))
        must_reject(lambda: subject.read_fixture("relation-1.receipt", 1, pair))
        summary = subject.base_summary("relation-1", commit, producer)
        summary.update(status="VERIFIED_PARTIAL", publication="VERIFIED_PARTIAL", contextSha256=context_hash)
        public = base / "run/public"
        previous_mask = os.umask(0o077)
        try:
            subject.publish_small_results(public, summary, resources, context_data, pair, part)
        finally:
            os.umask(previous_mask)
        subject.exact_tree(public, subject.public_layout("relation-1", "VERIFIED_PARTIAL"))
        assert all(file.stat().st_mode & 0o777 == 0o600 for file in pair.iterdir())
        assert all(file.stat().st_mode & 0o777 == (0o755 if file.is_dir() else 0o644) for file in [public, *public.rglob("*")])
        must_reject(lambda: subject.publish_small_results(base / "bad/public", summary, subject.unknown_resources(), context_data, pair, part))
        diagnostic = {**summary, "status": "FAIL", "publication": "UNQUALIFIED"}
        subject.publish_small_results(public, diagnostic, subject.unknown_resources(), context_data, pair)
        assert not (public / "partials/relation-1/part.json").exists()
        assert (public / "partials/relation-1/relation-1.receipt").read_bytes() == b"self-test-only-not-a-genuine-receipt"
        must_reject(lambda: subject.publish_small_results(base / "bad2/public", diagnostic, resources, context_data, pair, part))
        subject.publish_small_results(public, summary, resources, context_data, pair, part)
        with patch.object(subject, "load_profile", return_value=profile):
            revoked_public = subject.finish_host_public(public, "relation-1", commit, ValueError("private cleanup error"))
        revoked = subject.read_json_bytes((revoked_public / "summary.json").read_bytes())
        assert revoked["status"] == "FAIL" and revoked["publication"] == "UNQUALIFIED"
        assert not (revoked_public / "partials/relation-1/part.json").exists()
        assert b"private cleanup error" not in (revoked_public / "summary.json").read_bytes()
        prepared = subject.base_summary("prepare", commit, producer)
        prepared.update(status="PREPARED", contextSha256=context_hash, node=node_summary)
        prepared_public = base / "prepared/public"
        subject.publish_small_results(prepared_public, prepared, resources, context_data)
        output = base / "github-output"
        with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}), patch.object(subject, "load_profile", return_value=profile):
            subject.finish_host_public(prepared_public, "prepare", commit, None)
        assert 'matrix={"item":["relation-1","relation-2","relation-3","relation-4"]}' in output.read_text()

        def archive_bytes(files, symlink=False):
            buffer = io.BytesIO()
            with zipfile.ZipFile(buffer, "w") as archive:
                for name, data in files.items():
                    if symlink:
                        entry = zipfile.ZipInfo(name)
                        entry.external_attr = (stat.S_IFLNK | 0o777) << 16
                        archive.writestr(entry, data)
                    else:
                        archive.writestr(name, data)
            return buffer.getvalue()
        files = {file.relative_to(public).as_posix(): file.read_bytes() for file in public.rglob("*") if file.is_file()}
        data = archive_bytes(files)
        zip_origin = {**origin, "artifactDigest": "sha256:" + hashlib.sha256(data).hexdigest()}
        subject.extract_artifact(data, zip_origin, base / "downloaded")
        for name in ("../outside", "/absolute", "private/witness.json"):
            bad = archive_bytes({**files, name: b"private"})
            must_reject(lambda: subject.extract_artifact(bad, {**origin, "artifactDigest": "sha256:" + hashlib.sha256(bad).hexdigest()}, base / "invalid-zip"))
        bad = archive_bytes(files, symlink=True)
        must_reject(lambda: subject.extract_artifact(bad, {**origin, "artifactDigest": "sha256:" + hashlib.sha256(bad).hexdigest()}, base / "invalid-link-zip"))
        must_reject(lambda: subject.extract_artifact(data + b"corrupt", zip_origin, base / "invalid-digest"))
        download_api = {**artifact_api, "id": origin["artifactId"], "name": origin["artifactName"], "digest": zip_origin["artifactDigest"]}
        requests = []
        def fake_api(repository, suffix, binary=False):
            assert repository == producer["repository"]
            requests.append(suffix)
            if suffix in ("runs/101", "runs/101/attempts/2"):
                return run_api
            if suffix == "runs/101/artifacts?per_page=100&page=1":
                return {"artifacts": [download_api, download_api]}
            if suffix == f"artifacts/{origin['artifactId']}/zip":
                assert binary
                return data
            raise AssertionError("Unexpected self-test Actions request")
        with patch.object(subject, "ARTIFACTS", base / "downloads"), patch.object(subject, "gh_api", side_effect=fake_api):
            downloaded = subject.download_inputs(producer, None)
            assert len(subject.read_json_bytes((downloaded / "index.json").read_bytes())["artifacts"]) == 1
            assert requests.count(f"artifacts/{origin['artifactId']}/zip") == 1
        with patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "GITHUB_REPOSITORY": producer["repository"], "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_RUN_ID": "101", "GITHUB_RUN_ATTEMPT": "2", "GITHUB_SHA": commit, "GITHUB_WORKFLOW_REF": producer["repository"] + "/" + subject.WORKFLOW_PATH + "@refs/heads/test"}):
            assert subject.current_producer(commit) == producer
            must_reject(lambda: subject.current_producer("f" * 40))
            for path in (".github/workflows/ci.yml", ".github/workflows/delivery-live.yml", ".github/workflows/delivery-small.yml/extra"):
                with patch.dict(os.environ, {"GITHUB_WORKFLOW_REF": producer["repository"] + "/" + path + "@refs/heads/test"}):
                    must_reject(lambda: subject.current_producer(commit))

        incoming = base / "incoming"
        incoming.mkdir(mode=0o700)
        origins = []
        for relation in range(1, 5):
            item = f"relation-{relation}"
            item_origin = origin_for(item, 10 + relation, attempt=1 if relation % 2 else 2)
            item_producer = {key: item_origin[key] for key in producer}
            item_part = part_for(relation)
            item_part["producer"] = item_producer
            item_summary = subject.base_summary(item, commit, item_producer)
            item_summary.update(status="VERIFIED_PARTIAL", publication="VERIFIED_PARTIAL", contextSha256=context_hash)
            subject.publish_small_results(incoming / ("stage-" + str(relation)) / "public", item_summary, resources, context_data, pair, item_part)
            (incoming / ("stage-" + str(relation)) / "public").rename(incoming / str(item_origin["artifactId"]))
            shutil.rmtree(incoming / ("stage-" + str(relation)))
            origins.append(item_origin)
        index = {"schemaVersion": 1, "sourceCommit": commit, "repository": producer["repository"], "artifacts": origins}
        (incoming / "index.json").write_bytes(subject.json_bytes(index))
        with patch.object(subject, "run", side_effect=verifier), patch("subprocess.run", return_value=subprocess.CompletedProcess([], 1, stderr=b"Error: receipt journal mismatch\n")), patch.object(subject, "load_fixtures", return_value=(manifest, proofs)):
            _, selected, _ = subject.normalize_inputs(incoming, base / "normalized", compiled, vectors, commit, "accept", producer["repository"])
            assert set(selected) == {1, 2, 3, 4} and {value[0]["runAttempt"] for value in selected.values()} == {1, 2}
            assert {file.name for file in (base / "normalized/proofs").iterdir()} == subject.PROOF_NAMES
            competitor = origin_for("relation-1", 50)
            competitor_summary = subject.base_summary("relation-1", commit, producer)
            competitor_summary.update(status="VERIFIED_PARTIAL", publication="VERIFIED_PARTIAL", contextSha256=context_hash)
            subject.publish_small_results(incoming / "competitor/public", competitor_summary, resources, context_data, pair, part_for(1))
            (incoming / "competitor/public").rename(incoming / "50")
            shutil.rmtree(incoming / "competitor")
            (incoming / "index.json").write_bytes(subject.json_bytes({**index, "artifacts": origins + [competitor]}))
            _, competing, _ = subject.normalize_inputs(incoming, base / "competing", compiled, vectors, commit, "accept", producer["repository"])
            assert competing[1][0]["artifactId"] == origins[0]["artifactId"]
            receipt_file = incoming / "50/partials/relation-1/relation-1.receipt"
            receipt_file.write_bytes(b"corrupt claimed-qualified receipt")
            must_reject(lambda: subject.normalize_inputs(incoming, base / "corrupt-competitor", compiled, vectors, commit, "accept", producer["repository"]))
            shutil.rmtree(incoming / "50")
            (incoming / "index.json").write_bytes(subject.json_bytes({**index, "artifacts": origins + [origins[0]]}))
            must_reject(lambda: subject.normalize_inputs(incoming, base / "duplicate", compiled, vectors, commit, "accept", producer["repository"]))
            (incoming / "index.json").write_bytes(subject.json_bytes({**index, "artifacts": origins[:-1]}))
            shutil.rmtree(incoming / str(origins[-1]["artifactId"]))
            must_reject(lambda: subject.normalize_inputs(incoming, base / "missing", compiled, vectors, commit, "accept", producer["repository"]))
            diagnostic_origin = origin_for("relation-4", 60)
            diagnostic_summary = subject.base_summary("relation-4", commit, producer)
            diagnostic_summary.update(publication="UNQUALIFIED", contextSha256=context_hash)
            subject.publish_small_results(incoming / "diagnostic/public", diagnostic_summary, subject.unknown_resources(), context_data, pair)
            (incoming / "diagnostic/public").rename(incoming / "60")
            shutil.rmtree(incoming / "diagnostic")
            (incoming / "index.json").write_bytes(subject.json_bytes({**index, "artifacts": origins[:-1] + [diagnostic_origin]}))
            must_reject(lambda: subject.normalize_inputs(incoming, base / "unqualified-missing", compiled, vectors, commit, "accept", producer["repository"]))
        full = base / "normalized/proofs"
        final_summary = subject.base_summary("accept", commit, producer)
        final_summary.update(status="FAIL", publication="VERIFIED_BUNDLE", contextSha256=context_hash)
        subject.publish_small_results(base / "final/public", final_summary, subject.unknown_resources(), context_data, verified_bundle=full)
        assert {file.name for file in (base / "final/public/proofs").iterdir()} == subject.PROOF_NAMES
        (full / "extra.json").write_bytes(b"{}")
        must_reject(lambda: subject.publish_small_results(base / "extra/public", final_summary, resources, context_data, verified_bundle=full))
        (full / "extra.json").unlink()

        env = {subject.DEADLINE_ENV: str(int(time.time() * 1000) + 100000), "VIBEFACE_DELIVERY_SOURCE_COMMIT": commit, "VIBEFACE_DELIVERY_PRODUCER": json.dumps(producer)}
        def run_item(reuse=False, node_fails=False, bad_proves=False, final_resources="PASS", binary_fails=False, negative_fails=False):
            run_id = uuid.uuid4().hex
            old = part_for(1)
            original_origin = origin_for("relation-1", 500, attempt=1)
            if reuse:
                old["producer"] = {**producer, "runAttempt": 1}
                old["provingResources"]["process"]["elapsedMs"] = 999
            selected = {1: (original_origin, pair, old)} if reuse else {}
            calls = []
            candidate = deepcopy(node_summary)
            candidate.update(workItem="relation-1", status="FAIL" if node_fails else "PROOF_VERIFIED", replayChecks=0, reused=reuse,
                processes=[] if reuse else [process], proofs=[] if node_fails else [{"relation": 1, "cycles": 10, "segments": 1, "receiptBytes": 39, "sealBytes": 260, "journalHash": proofs[0]["journalHash"], "independentlyVerified": True, "wrongJournalRejected": True, "reused": reuse}])
            if bad_proves:
                candidate["processes"] = [process, {**process, "label": "prove-2"}]
            def execute(command, **kwargs):
                calls.append(command)
                if command[:2] == ["node", "contracts/delivery/experiment-delivery.mjs"]:
                    dest = Path(command[3])
                    (dest / "public").mkdir(mode=0o700, parents=True)
                    (dest / "private/proofs").mkdir(mode=0o700, parents=True)
                    (dest / "public/context.json").write_bytes(context_data)
                    (dest / "public/summary.json").write_bytes(subject.json_bytes(candidate))
                    for suffix in ("json", "receipt"):
                        shutil.copyfile(pair / ("relation-1." + suffix), dest / ("private/proofs/relation-1." + suffix))
                    if node_fails:
                        raise ValueError("private assertion text must not be exported")
                    return subprocess.CompletedProcess(command, 0)
                if command[0].endswith("vibeface-delivery-prover"):
                    if binary_fails:
                        raise ValueError("self-test binary corruption")
                    return verifier(command, **kwargs)
                return subprocess.CompletedProcess(command, 0)
            observations = [snapshot, snapshot if final_resources == "PASS" else (oom["final"] if final_resources == "OOM" else ValueError("unknown"))]
            def observe():
                value = observations.pop(0)
                if isinstance(value, Exception):
                    raise value
                return None, value
            negative_error = b"Error: io error: unexpected end of file\n" if negative_fails else b"Error: receipt journal mismatch\n"
            counters = {"oom": int(final_resources == "OOM"), "oom_kill": 0, "oom_group_kill": 0}
            with (
                patch.dict(os.environ, {**env, "VIBEFACE_DELIVERY_WORK_ID": run_id}),
                patch.object(subject, "CANONICAL", base / "canonical"),
                patch.object(subject, "source_commit", return_value=commit),
                patch.object(subject, "prepare_canonical", return_value=(compiled, profile, vectors, {})),
                patch.object(subject, "normalize_inputs", return_value=(context, selected, [])),
                patch.object(subject, "run", side_effect=execute),
                patch.object(subject, "cgroup_resources", side_effect=observe),
                patch.object(subject, "oom_delta", side_effect=lambda *_: dict(counters)),
                patch("subprocess.run", return_value=subprocess.CompletedProcess([], 1, stderr=negative_error)),
                patch("builtins.print"),
            ):
                if node_fails or bad_proves or final_resources != "PASS" or binary_fails or negative_fails:
                    must_reject(lambda: subject.prove_small("relation-1"))
                else:
                    subject.prove_small("relation-1")
            exported = base / "canonical/small" / run_id / "public"
            result = subject.read_json_bytes((exported / "summary.json").read_bytes())
            certificate = exported / "partials/relation-1/part.json"
            assert (exported / "partials/relation-1/relation-1.receipt").exists() is not binary_fails
            assert sum(command[:2] == ["node", "contracts/delivery/experiment-delivery.mjs"] for command in calls) == 1
            if node_fails or bad_proves or final_resources != "PASS" or binary_fails or negative_fails:
                assert result["status"] == "FAIL" and result["publication"] == ("NO_PROOF" if binary_fails else "UNQUALIFIED") and not certificate.exists()
            else:
                saved = subject.read_json_bytes(certificate.read_bytes())
                assert result["status"] == "VERIFIED_PARTIAL"
                if reuse:
                    assert saved["producer"] == old["producer"] and saved["producerArtifact"] == original_origin and saved["provingResources"] == old["provingResources"]
        run_item()
        run_item(reuse=True)
        run_item(node_fails=True)
        run_item(bad_proves=True)
        run_item(final_resources="UNKNOWN")
        run_item(final_resources="OOM")
        run_item(binary_fails=True)
        run_item(negative_fails=True)
        for has_published_pair in (False, True):
            outer_output = base / ("outer-output-" + str(has_published_pair))
            generated = []
            stop_error = subprocess.CalledProcessError(1, ["docker", "stop"])
            def outer_run(command, **kwargs):
                if command[:2] != ["docker", "run"]:
                    return subprocess.CompletedProcess(command, 0)
                work_id = next(value.split("=", 1)[1] for value in command if value.startswith("VIBEFACE_DELIVERY_WORK_ID="))
                original_public = base / "host-artifacts/canonical/small" / work_id / "public"
                generated.append(original_public)
                if has_published_pair:
                    subject.publish_small_results(original_public, summary, resources, context_data, pair, part)
                    original_public.parent.chmod(0o555)
                raise subprocess.CalledProcessError(1, ["docker", "run"])
            previous = signal.getsignal(signal.SIGTERM)
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(outer_output)}), patch.object(subject, "ARTIFACTS", base / "host-artifacts"),                 patch.object(subject, "optional_fixtures", return_value=None), patch.object(subject, "source_commit", return_value=commit), patch.object(subject, "current_producer", return_value=producer),                 patch.object(subject, "download_inputs", return_value=None), patch.object(subject, "load_profile", return_value=profile), patch.object(subject, "run", side_effect=outer_run),                 patch("subprocess.run", side_effect=[subprocess.CompletedProcess([], 0, stdout="a" * 64), stop_error]), patch("builtins.print"):
                try:
                    subject.sandbox("relation-1")
                except subprocess.CalledProcessError as error:
                    assert error is stop_error
                else:
                    raise AssertionError("Failed CLI and owned cleanup must remain a failed work item")
            assert signal.getsignal(signal.SIGTERM) is previous
            output_path = outer_output.read_text().strip().split("=", 1)[1]
            safe_public = subject.ROOT / output_path
            assert safe_public != generated[0] and "*" not in output_path
            safe_summary = subject.read_json_bytes((safe_public / "summary.json").read_bytes())
            assert safe_summary["status"] == "FAIL" and safe_summary["publication"] == ("UNQUALIFIED" if has_published_pair else "NO_PROOF")
            assert not (safe_public / "partials/relation-1/part.json").exists()
            assert (safe_public / "partials/relation-1/relation-1.receipt").exists() is has_published_pair
            if has_published_pair:
                assert (generated[0] / "partials/relation-1/part.json").exists()
                assert generated[0].parent.stat().st_mode & 0o777 == 0o555
                generated[0].parent.chmod(0o755)
        calls = []
        with (
            patch.dict(os.environ, {**env, "VIBEFACE_DELIVERY_WORK_ID": uuid.uuid4().hex}),
            patch.object(subject, "CANONICAL", base / "canonical"),
            patch.object(subject, "source_commit", return_value=commit),
            patch.object(subject, "prepare_canonical", return_value=(compiled, profile, vectors, {})),
            patch.object(subject, "normalize_inputs", side_effect=ValueError("missing part")),
            patch.object(subject, "run", side_effect=lambda command, **kwargs: calls.append(command)),
            patch.object(subject, "cgroup_resources", return_value=(None, snapshot)),
            patch.object(subject, "oom_delta", return_value=resources["oomDelta"]),
            patch("builtins.print"),
        ):
            must_reject(lambda: subject.prove_small("accept"))
        assert not any(command[:2] == ["node", "contracts/delivery/experiment-delivery.mjs"] for command in calls)
