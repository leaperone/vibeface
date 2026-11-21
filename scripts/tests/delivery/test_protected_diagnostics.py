#!/usr/bin/env python3
"""AUTO-23 offline protected delivery cases.

Legacy ``verify-delivery-protected.py`` self-tests remain compatibility shims;
case ownership lives here under stable diagnostic, proof, reconciliation,
review, and runtime compatibility IDs.
"""

from __future__ import annotations

import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import select
import socket
import stat
import subprocess
import sys
import tempfile
import types
import uuid
from types import SimpleNamespace
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[2]
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))
from verification.io import digest_bytes

# Fixture-only projection of the resource phases used by the protected journal.
FIXTURE_RESOURCE_PHASES = {("core", "key_preproof"), ("core", "proof_info"),
                           ("core", "proof_prove"), ("core", "proof_verify"),
                           ("core", "register_tx"), ("host", "host_succinct"),
                           ("host", "host_groth16"), ("shim", "shim_workers")}

def _load_module(path, name):
    """Load one hyphenated script as a real ModuleType for compatibility tests."""
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise ImportError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

def self_test_proof_fixture(node, index, artifacts, profile, mode="prove"):
    peer, order = ("11" if node == "S" else "22") * 32, ("ab" if index == 0 else "cd") * 32
    relation = 2 if node == "S" else 3
    records = [{"v": 2, "kind": "job", "node": peer, "order": order, "relation": relation, "window": 0,
                "job": digest_bytes((node + str(index)).encode()), "profile": profile},
               {"v": 2, "kind": "tools", "prover": artifacts["prover"], "r0vm": artifacts["r0vm"],
                "target_cgroup": [1, 2], "core_same_target": True}]
    def event(writer, command, phase, edge, *, io=None, role=None, code=None, mode=None):
        stamp = len(records) * 1000
        sampled = writer == "core" and edge != "observed" or writer == "host" and phase in ("host_succinct", "host_groth16") or writer == "shim" and phase == "shim_workers"
        records.append({"v": 2, "kind": "event", "w": writer, "c": command, "p": phase, "e": edge, "t": stamp,
                        "x": code, "s": None, "n": None, "f": None, "r": role, "k": False,
                        "g": None if edge == "observed" else [1, 2], "q": [stamp] * 5 if sampled else None, "i": io, "a": mode})
    def stage(writer, phase, *, command="prove", role=None):
        event(writer, command, phase, "begin", role=role); event(writer, command, phase, "ok", role=role)
    def activity(edge):
        records.append({"v": 2, "kind": "source_activity", "e": edge, "t": len(records) * 1000, "g": [1, 2], "q": [len(records) * 1000] * 7})
    def command(name, phase):
        event("core", name, phase, "begin", io=[False, False, False, False, None, False, False])
        if name == "prove" and node == "S": activity("begin")
        event("core", name, "process_spawn", "observed", io=[True, False, False, False, None, False, False])
        event("core", name, "stdin_closed", "observed", io=[True, True, True, False, None, False, False])
        if name == "prove":
            for phase_name in ("host_input", "host_succinct", "host_succinct_verify"): stage("host", phase_name)
            event("host", "prove", "host_groth16", "begin")
            for phase_name in ("shim_validate", "shim_lock", "shim_fifo"): stage("shim", phase_name)
            for role in ("generator", "prover"): stage("shim", role + "_spawn", role=role)
            event("shim", "prove", "shim_workers", "begin")
            for role in ("generator", "prover"): event("shim", "prove", "worker_wait", "ok", role=role, code=0)
            event("shim", "prove", "shim_workers", "ok"); stage("shim", "shim_commit")
            event("host", "prove", "host_groth16", "ok")
            for phase_name in ("host_receipt_verify", "host_receipt_publish"): stage("host", phase_name)
        elif name == "verify": stage("host", "host_receipt_verify", command="verify")
        event("core", name, "stdout_complete", "observed", io=[True, True, True, True, 9, False, False])
        event("core", name, "process_wait", "observed", io=[True, True, True, True, 9, True, False], code=0)
        if name == "prove" and node == "S": activity("end")
        event("core", name, phase, "ok", io=[True, True, True, True, 9, True, False], code=0)
    event("core", "prove", "proof_job", "begin", mode="not_started")
    command("info", "proof_info")
    if mode == "prove": command("prove", "proof_prove")
    command("verify", "proof_verify")
    event("core", "prove", "proof_job", "ok", mode=mode)
    return records


def self_test_proof_diagnostics(subject):
    from unittest.mock import Mock, patch
    from types import SimpleNamespace
    import tempfile
    artifacts = {"prover": "b" * 64, "r0vm": "c" * 64, "profile": "d" * 64}
    source, profile = "1" * 40, "f" * 64
    orders = [{"order_id": value * 32, "sha256": "e" * 64} for value in ("ab", "cd")]
    principals = {node: {"peer": value * 32} for node, value in (("S", "11"), ("R", "22"))}
    encode = lambda records: b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode() for record in records)
    def summarize(records, node="S", index=0, facts=()):
        return subject.summarize_proof_job(encode(records), node, index, principals[node]["peer"], orders[index]["order_id"], profile,
                                   artifacts, [1, 2], subject.digest((node + str(index)).encode()), facts)
    fixtures = {(node, index): self_test_proof_fixture(node, index, artifacts, profile) for node in ("S", "R") for index in range(2)}
    jobs = [summarize(records, node, index) for (node, index), records in fixtures.items()]
    value = {"schema_version": 1, "source_commit": source, "artifact_sha256": artifacts, "profile_id": profile,
             "counter_fields": list(subject.DIAGNOSTIC_COUNTERS), "attempt_policy": "first_admitted", "availability": "complete", "jobs": jobs}
    subject.need(subject.project_proof_diagnostics(value, source, artifacts, profile, principals, orders, []) == value
         and len(jobs) == 4 and len({job["job_digest"] for job in jobs}) == 4
         and all(job["availability"] == "complete" and job["binding"] == "reserved_job" for job in jobs), "four_concrete_bound_proof_jobs")
    subject.need(all((job["source_activity"] is not None) == (job["node"] == "S") for job in jobs), "source_only_activity")
    subject.need(subject.registration_diagnostics(encode(fixtures["S", 0]), source, artifacts,
         {"exit_code": 0, "signal": None, "spawn_errno": None})["availability"] == "unavailable", "order_v2_never_becomes_key_v1")
    facts = [{"relation": job["relation"], "orderId": "0x" + orders[job["order_index"]]["order_id"], "windowIndex": 0,
              "journalHash": job["job_digest"], "official_host_verified": True, "wrong_journal_receipt_comparison_rejected": True} for job in jobs]
    verified = [summarize(records, node, index, facts) for (node, index), records in fixtures.items()]
    subject.need(all(job["binding"] == "verified_receipt" for job in verified), "only_existing_verified_receipts_upgrade_binding")
    records = fixtures["S", 0]
    cut = next(i for i, event in enumerate(records) if event.get("p") == "host_succinct" and event["e"] == "begin")
    timeout = copy.deepcopy(records[:cut + 1]); stamp = timeout[-1]["t"]
    activity = next(copy.deepcopy(event) for event in records if event["kind"] == "source_activity" and event["e"] == "end")
    activity.update(t=stamp + 1000, q=[stamp + 1000] * 7); timeout.append(activity)
    terminal = next(copy.deepcopy(event) for event in records if event.get("p") == "proof_prove" and event["e"] == "ok")
    terminal.update(e="error", t=stamp + 2000, x=None, f="timeout", q=[stamp + 2000] * 5, i=[True, True, True, False, None, False, True]); timeout.append(terminal)
    outer = copy.deepcopy(records[-1]); outer.update(e="error", t=stamp + 3000, f="stage_error", q=[stamp + 3000] * 5); timeout.append(outer)
    partial = summarize(timeout, facts=facts)
    subject.need(partial["availability"] == "partial" and partial["binding"] == "verified_receipt"
         and partial["last_open_phase"]["phase"] == "host_succinct" and partial["command"]["timed_out"]
         and partial["command"]["stdout_bytes"] is None and partial["command"]["exit_code"] is None, "timeout_prefix_stays_partial_after_later_receipt")
    cut = next(i for i, event in enumerate(records) if event.get("p") == "worker_wait")
    cleanup = copy.deepcopy(records[:cut + 2]); cleanup[-2].update(e="error", x=137, f="worker_exit")
    cleanup[-1].update(e="error", x=None, s=9, f="worker_signal", k=True)
    failed = summarize(cleanup)
    subject.need(failed["first_observed_failure"]["exit_code"] == 137 and failed["first_observed_failure"]["signal"] is None
         and failed["availability"] == "partial", "cleanup_never_overwrites_first_noncleanup_failure")
    for errno in (11, 12):
        spawn = copy.deepcopy(records[:4]); terminal = copy.deepcopy(spawn[-1])
        terminal.update(e="error", n=errno, f="spawn", t=4000, q=[4000] * 5); spawn.append(terminal)
        outer = copy.deepcopy(records[-1]); outer.update(e="error", a="not_started", t=5000, q=[5000] * 5, f="stage_error"); spawn.append(outer)
        observed = summarize(spawn)
        subject.need(observed["command"]["spawn_errno"] == errno and not observed["command"]["spawned"]
             and observed["command"]["exit_code"] is None, "actual_spawn_errno_has_no_wait0")
    negative_count = 0
    changes = [lambda data: data[0].update(node="ff" * 32), lambda data: data[0].update(order=orders[1]["order_id"]),
               lambda data: data[0].update(relation=3), lambda data: data[0].update(window=1), lambda data: data[0].update(job="e" * 64),
               lambda data: data[0].update(profile="e" * 64), lambda data: data[1].update(prover="e" * 64),
               lambda data: data[1].update(r0vm="e" * 64), lambda data: data[0].update(private="PRIVATE_PROOF_SENTINEL"),
               lambda data: data.insert(2, copy.deepcopy(data[0])), lambda data: data[2].update(v=1),
               lambda data: data[3].update(p="proof_preflight"), lambda data: data[3].update(t=0),
               lambda data: data[4].update(i=[True, False, False, True, 16385, False, False]),
               lambda data: data[5].update(i=[True, True, True, False, None, False, False], x=0),
               lambda data: data[-1].update(q=[0] * 5), lambda data: data[-2].update(x=True)]
    for change in changes:
        invalid = copy.deepcopy(records); change(invalid)
        try: summarize(invalid)
        except (subject.CheckFailure, ValueError, TypeError, KeyError): negative_count += 1
        else: raise AssertionError("invalid order diagnostic accepted")
    for bad in (b"", encode(records)[:-1], b"x" * 32769, b"x" * 512 + b"\n", encode(records + [records[-1]] * (65 - len(records))),
                encode(records).replace(b'"v":2', b'"v":2,"v":2', 1)):
        try: subject.summarize_proof_job(bad, "S", 0, principals["S"]["peer"], orders[0]["order_id"], profile, artifacts, [1, 2], records[0]["job"], [])
        except (subject.CheckFailure, ValueError, TypeError, KeyError): negative_count += 1
        else: raise AssertionError("invalid order diagnostic byte stream accepted")
    for change in (lambda data: data[1].update(core_same_target=False), lambda data: data[1].update(target_cgroup=[3, 4]),
                   lambda data: next(event for event in data if event.get("w") == "host").update(g=None),
                   lambda data: next(event for event in data if event.get("w") == "shim").update(g=[3, 4]),
                   lambda data: next(event for event in data if event["kind"] == "source_activity" and event["e"] == "end").update(q=[0] * 7)):
        unknown = copy.deepcopy(records); change(unknown); subject.need(summarize(unknown)["availability"] == "partial", "unknown_group_or_activity_stays_partial")
    for change in (lambda data: data.update(source_commit="0" * 40), lambda data: data["artifact_sha256"].update(prover="0" * 64),
                   lambda data: data.update(profile_id="0" * 64), lambda data: data.update(private="PRIVATE_PROOF_SENTINEL"),
                   lambda data: data["jobs"][0].update(window=True), lambda data: data["jobs"][0]["command"].update(exit_code=137, signal=9),
                   lambda data: data["jobs"][0]["command"].update(failure_class="worker_exit", exit_code=0),
                   lambda data: data["jobs"][0]["command"].update(failure_class="worker_signal", exit_code=137, signal=None),
                   lambda data: data["jobs"][1].update(job_digest=data["jobs"][0]["job_digest"]),
                   lambda data: data["jobs"][0].update(source_activity={"start": {"private": "PRIVATE_PROOF_SENTINEL"}, "end": None})):
        invalid = copy.deepcopy(value); change(invalid)
        try: subject.project_proof_diagnostics(invalid, source, artifacts, profile, principals, orders, facts)
        except (subject.CheckFailure, ValueError, TypeError, KeyError): negative_count += 1
        else: raise AssertionError("invalid public proof diagnostic accepted")
    source_text = (subject.REPOSITORY / "apps/core/src/delivery/proof.rs").read_text()
    admitted = source_text.split("impl ProofRunner {", 1)[1].split("async fn verified_seal", 1)[0]
    admission = [admitted.index(part) for part in (".try_acquire()", "process_lock(receipt)", "Diagnostics::claim_order", "config.validate()?", "config.validate_executables()?", "config.check_memory_limit()?", "config.oom_events()?", "/var/run/docker.sock", "verify_relation(", "input.profile == config.profile", "sink.activate_order")]
    subject.need(admission == sorted(admission) and "proof_preflight" not in subject.PROOF_PHASES["core"], "original_admission_precedes_order_activation")
    with tempfile.TemporaryDirectory(prefix="proof-collector-selftest-", dir=subject.ARTIFACTS) as temporary:
        run = Path(temporary)
        experiment = object.__new__(subject.Experiment)
        experiment.nodes = [SimpleNamespace(name=node, device_id=principals[node]["peer"], directory=run / node) for node in ("S", "R")]
        for node in experiment.nodes: node.directory.mkdir(mode=0o700)
        experiment.config = {"source_commit": source, "sha256": artifacts}; experiment.profile_id = profile
        experiment.proof_channels = {}; experiment.proof_diagnostic_result = None
        experiment.orders = {order["order_id"]: order for order in orders}; experiment.summary = {"genuine_receipts": facts}
        experiment.prover_settings = {"cgroup": str(run)}
        target_info = run.stat(); synthetic_target = [target_info.st_dev, target_info.st_ino]
        for node in experiment.nodes:
            experiment.prepare_proof_diagnostics(node)
            channel = experiment.proof_channels[node.name]
            subject.need(channel is not None and not os.get_inheritable(channel["fd"]), "collector_directory_is_pinned_private")
            for index in range(2):
                fixture = copy.deepcopy(fixtures[node.name, index])
                fixture[1]["target_cgroup"] = synthetic_target
                for event in fixture[2:]:
                    if event["g"] is not None: event["g"] = synthetic_target
                path = channel["path"] / (fixture[0]["job"] + ".jsonl")
                descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                os.write(descriptor, encode(fixture)); os.close(descriptor)
        experiment.rows = Mock(side_effect=lambda node, _sql: [(fixtures[node.name, index][0]["job"], str(node.directory / "delivery/private" / ("proof-" + fixtures[node.name, index][0]["job"] + ".receipt"))) for index in range(2)])
        with patch.object(subject, "resources", lambda: (run, {})):
            collected = experiment.collect_proof_diagnostics()
        subject.need(collected["availability"] == "complete" and len(collected["jobs"]) == 4
             and all(job["resources"]["cgroup"] == synthetic_target for job in collected["jobs"]), "four_synthetic_streams_bound_to_the_pinned_directory_fixture")
        for node in experiment.nodes: subject.need(experiment.proof_channels[node.name] is None, "collector_closes_pinned_descriptors")
        subject.need(experiment.collect_proof_diagnostics() == collected, "collector_never_upgrades_first_attempt")
    for kind in ("valid", "missing", "fifo", "directory", "symlink", "hardlink", "mode", "owner", "changed", "replaced-directory", "overflow", "half-line", "extra"):
        with tempfile.TemporaryDirectory(prefix="proof-collector-negative-", dir=subject.ARTIFACTS) as temporary:
            run = Path(temporary); node = SimpleNamespace(name="S", device_id=principals["S"]["peer"], directory=run / "S"); node.directory.mkdir(mode=0o700)
            experiment = object.__new__(subject.Experiment); experiment.nodes = [node, SimpleNamespace(name="R", device_id=principals["R"]["peer"])]; experiment.proof_channels = {}; experiment.proof_diagnostic_result = None
            experiment.config = {"source_commit": source, "sha256": artifacts}; experiment.profile_id = profile
            experiment.orders = {orders[0]["order_id"]: orders[0]}; experiment.summary = {"genuine_receipts": []}
            experiment.rows = Mock(return_value=[(records[0]["job"], str(node.directory / "delivery/private" / ("proof-" + records[0]["job"] + ".receipt")))])
            experiment.prepare_proof_diagnostics(node); channel = experiment.proof_channels["S"]; path = channel["path"] / (records[0]["job"] + ".jsonl")
            if kind == "fifo": os.mkfifo(path, 0o600)
            elif kind == "directory": path.mkdir(mode=0o700)
            elif kind == "symlink": path.symlink_to(run / "missing")
            elif kind != "missing":
                path.write_bytes(encode(records)); path.chmod(0o600)
                if kind == "hardlink": os.link(path, run / "linked")
                if kind == "mode": path.chmod(0o644)
                if kind == "overflow": path.write_bytes(b"x" * 32769)
                if kind == "half-line": path.write_bytes(encode(records)[:-1])
                if kind == "extra": (channel["path"] / "unexpected").touch(mode=0o600)
                if kind == "replaced-directory":
                    channel["path"].rename(channel["path"].with_name("old-proof-diagnostics")); channel["path"].mkdir(mode=0o700)
            actual_fstat = os.fstat
            regular_stats = 0
            def checked_fstat(descriptor):
                nonlocal regular_stats
                info = actual_fstat(descriptor)
                if stat.S_ISREG(info.st_mode):
                    regular_stats += 1
                    if kind == "owner" or kind == "changed" and regular_stats == 2:
                        fields = {name: getattr(info, name) for name in ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns", "st_mode", "st_uid", "st_nlink")}
                        fields["st_uid" if kind == "owner" else "st_mtime_ns"] += 1
                        return SimpleNamespace(**fields)
                return info
            with patch.object(subject, "resources", lambda: (_ for _ in ()).throw(OSError())), patch.object(os, "fstat", side_effect=checked_fstat):
                unavailable = experiment.collect_proof_diagnostics()
            if kind == "valid":
                subject.need(unavailable["availability"] != "unavailable", "safe_stream_with_reservation_is_collected")
                continue
            subject.need(unavailable["availability"] == "unavailable" and all(job == subject.unavailable_proof_job(job["node"], job["order_index"]) for job in unavailable["jobs"]), "unsafe_stream_is_nonblocking_unavailable")
    maximum, maximum_group = 2**63-1, [2**64-1] * 2
    conservative_records = copy.deepcopy(records)
    for record in conservative_records:
        if record["kind"] == "job": record["window"] = 65535
        elif record["kind"] == "tools": record.update(target_cgroup=maximum_group, core_same_target=False)
        else:
            record["t"] = maximum; record["g"] = maximum_group
            record["q"] = [maximum] * (7 if record["kind"] == "source_activity" else 5)
            if record["kind"] == "event":
                record.update(x=None, s=None, n=4095, f="result_decode", k=False)
                if record["r"] is not None: record["r"] = "generator"
                if record["i"] is not None: record["i"] = [False, False, False, False, 16385, False, False]
                if record["a"] is not None: record["a"] = "receipt_reconcile"
    worst = copy.deepcopy(value)
    phase = {"writer": "host", "command": "prove", "phase": max(subject.PROOF_PHASES["host"], key=len), "t_ns": maximum, "event_index": 63}
    for job in worst["jobs"]:
        job.update(availability="partial", binding="verified_receipt", mode="receipt_reconcile", event_count=62,
                   first_observed_failure={**phase, "failure_class": "result_decode", "worker": "generator", "exit_code": None,
                                           "signal": None, "errno": 4095, "cleanup_requested": False}, last_open_phase=dict(phase))
        job["command"].update(command="verify", spawned=False, stdin_write_complete=False, stdin_closed=False, stdout_eof=False,
            stdout_bytes=16385, wait_observed=False, timed_out=False, exit_code=None, signal=None, spawn_errno=4095,
            failure_class="result_decode", start_ns=maximum, end_ns=maximum)
        job["resources"] = {"cgroup": maximum_group, "same_cgroup_verified": False, **{name: {"t_ns": maximum, "event_index": 63, "values": [maximum] * 5} for name in ("start", "last_open", "end")}}
        if job["source_activity"] is not None:
            job["source_activity"] = {name: {"t_ns": maximum, "cgroup": maximum_group, "values": [maximum] * 7} for name in ("start", "end")}
    worst["availability"] = "unavailable"
    # Optional summaries and mutually exclusive maxima are all included in this size-only bound.
    private_max = max(len((json.dumps(record, separators=(",", ":")) + "\n").encode()) for record in conservative_records)
    stream_bound = sum(len((json.dumps(record, separators=(",", ":")) + "\n").encode()) for record in conservative_records[:2]) + 54 * private_max
    public_bound = len((json.dumps({"proof_diagnostics": worst}, indent=2, sort_keys=True) + "\n").encode())
    subject.need(private_max <= 512 and len(records) <= 56 and stream_bound <= 32768 and public_bound <= 16384, "actual_amended_serialization_bounds")
    for status in ("PASS", "FAIL"):
        base = {"status": status, "genuine_receipts": [], "payload": ""}
        minimal = {**value, "availability": "unavailable", "jobs": [subject.unavailable_proof_job(job["node"], job["order_index"]) for job in jobs]}
        overhead = len((json.dumps({**base, "proof_diagnostics": minimal}, indent=2, sort_keys=True) + "\n").encode())
        base["payload"] = "x" * (131072 - overhead)
        result = subject.final_proof_diagnostics(base, value)
        subject.need(result["status"] == status and result["genuine_receipts"] == [] and result["payload"] == base["payload"]
             and result["proof_diagnostics"]["availability"] == "unavailable" and len((json.dumps(result, indent=2, sort_keys=True) + "\n").encode()) <= 131072, "combined_cap_uses_minimal_unavailable_without_business_changes")
        base["payload"] = "x" * (131072 - len((json.dumps({**base, "payload": ""}, indent=2, sort_keys=True) + "\n").encode()))
        subject.need(subject.final_proof_diagnostics(base, value) == base, "combined_cap_omits_only_diagnostics")
    oversized = copy.deepcopy(value); oversized["jobs"][0]["command"]["failure_class"] = "x" * 16385
    subject.need(subject.final_proof_diagnostics({"status": "PASS"}, oversized)["proof_diagnostics"]["availability"] == "unavailable", "public_field_cap_does_not_expand")
    print(json.dumps({"proof_diagnostic_selftest": "PASS", "concrete_jobs": 4, "negative_controls": negative_count,
                      "group_unknown_controls": 5, "collector_negative_controls": 12,
                      "source_records": len(records), "max_record_bound_bytes": private_max, "stream_bound_bytes": stream_bound,
                      "public_field_bound_bytes": public_bound, "combined_cap_bytes": 131072, "external_processes": 0, "network_sockets": 0}))


def self_test_proof_review_regressions(subject):
    artifacts = {"prover": "b" * 64, "r0vm": "c" * 64, "profile": "d" * 64}
    source, profile = "1" * 40, "f" * 64
    principals = {node: {"peer": value * 32} for node, value in (("S", "11"), ("R", "22"))}
    orders = [{"order_id": value * 32} for value in ("ab", "cd")]
    fixtures = {(node, index): self_test_proof_fixture(node, index, artifacts, profile) for node in ("S", "R") for index in range(2)}
    def summarize(records, node="S", index=0, target=(1, 2)):
        data = b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode() for record in records)
        return subject.summarize_proof_job(data, node, index, principals[node]["peer"], orders[index]["order_id"], profile,
            artifacts, list(target) if target is not None else None, fixtures[node, index][0]["job"], [])
    baseline = fixtures["S", 0]
    jobs = [summarize(records, node, index) for (node, index), records in fixtures.items()]
    failures, observed = [], {}
    def check(label, records, expected_complete):
        try: row = summarize(records); status = row["availability"]
        except subject.CheckFailure: row, status = None, "rejected"
        observed[label] = status
        if (status == "complete") != expected_complete: failures.append(label)
        return row
    def project(row):
        rows = [row, *jobs[1:]]
        value = {"schema_version": 1, "source_commit": source, "artifact_sha256": artifacts, "profile_id": profile,
            "counter_fields": list(subject.DIAGNOSTIC_COUNTERS), "attempt_policy": "first_admitted", "availability": subject.proof_availability(rows), "jobs": rows}
        return subject.project_proof_diagnostics(value, source, artifacts, profile, principals, orders, [])
    check("complete_source", baseline, True)
    check("missing_info", [record for record in baseline if not (record.get("w") == "core" and record.get("c") == "info")], False)
    check("missing_verify", [record for record in baseline if not (record.get("w") in ("core", "host") and record.get("c") == "verify")], False)
    reconcile = self_test_proof_fixture("S", 0, artifacts, profile, "receipt_reconcile")
    check("complete_reconcile", reconcile, True)
    check("reconcile_missing_info", [record for record in reconcile if not (record.get("w") == "core" and record.get("c") == "info")], False)
    info = copy.deepcopy(baseline[:4]); terminal = copy.deepcopy(info[-1])
    terminal.update(e="error", n=11, f="spawn", t=4000, q=[4000] * 5); info.append(terminal)
    outer = copy.deepcopy(baseline[-1]); outer.update(e="error", a="not_started", t=5000, q=[5000] * 5, f="stage_error"); info.append(outer)
    check("fully_observed_info_spawn_failure", info, True)
    failed = [copy.deepcopy(record) for record in baseline if not (record.get("c") == "verify"
        or record.get("w") == "host" and record.get("p") == "host_receipt_publish")]
    for record in failed:
        if record.get("w") == "host" and record.get("p") == "host_receipt_verify" and record["e"] == "ok": record.update(e="error", f="stage_error")
        if record.get("w") == "core" and record.get("c") == "prove" and record.get("p") == "process_wait": record.update(x=1)
        if record.get("w") == "core" and record.get("p") == "proof_prove" and record["e"] == "ok": record.update(e="error", x=1, f="worker_exit")
        if record.get("p") == "proof_job" and record["e"] == "ok": record.update(e="error", f="stage_error")
    check("fully_observed_host_failure", failed, True)
    check("failure_missing_host_stage", [record for record in failed if not (record.get("w") == "host" and record.get("p") == "host_succinct_verify")], False)
    signaled = [copy.deepcopy(record) for record in failed if record.get("w") != "shim"
        and not (record.get("w") == "host" and record.get("p") != "host_input")]
    for record in signaled:
        if record.get("w") == "core" and record.get("c") == "prove" and record.get("p") in ("process_wait", "proof_prove") and record["e"] != "begin":
            record.update(x=None, s=9)
            if record["e"] == "error": record["f"] = "worker_signal"
    check("unexplained_hardkill", signaled, False)
    for edge in ("begin", "end"):
        for group in (None, [3, 4]):
            wrong = copy.deepcopy(baseline)
            raw = next(record for record in wrong if record["kind"] == "source_activity" and record["e"] == edge)
            raw["g"] = group
            row = summarize(wrong); point = row["source_activity"]["start" if edge == "begin" else "end"]
            label = "activity_" + edge + ("_missing_group" if group is None else "_wrong_group")
            normalized = point["values"] == [None] * 7 and point["t_ns"] == raw["t"] and point["cgroup"] == group
            observed[label] = normalized
            if not normalized: failures.append(label)
            untrusted = copy.deepcopy(jobs[0]); untrusted["availability"] = "partial"
            point = untrusted["source_activity"]["start" if edge == "begin" else "end"]; point["cgroup"] = group
            try: project(untrusted)
            except subject.CheckFailure: rejected = True
            else: rejected = False
            observed["public_" + label] = rejected
            if not rejected: failures.append("public_" + label)
    unknown = summarize(baseline, target=None)
    if any(value is not None for point in unknown["source_activity"].values() for value in point["values"]): failures.append("activity_unknown_target")
    partial = copy.deepcopy(baseline)
    next(record for record in partial if record.get("w") == "host")["g"] = [3, 4]
    row = summarize(partial)
    subject.need(row["availability"] == "partial" and row["source_activity"] == jobs[0]["source_activity"]
         and project(row)["jobs"][0]["source_activity"] == jobs[0]["source_activity"], "valid_same_target_partial_keeps_activity")
    print(json.dumps({"proof_review_regressions": observed, "failures": failures}, sort_keys=True))
    subject.need(not failures, "proof_review_regression_failures")


def self_test_diagnostic_fixture():
    records = []
    def event(writer, command, phase, edge, *, worker=None, code=None):
        counters = None
        if (writer, phase) in FIXTURE_RESOURCE_PHASES:
            counters = {"pids_max": 0, "memory_max": 0 if edge == "begin" else 2, "oom": 0, "oom_kill": 0, "oom_group_kill": 0}
        records.append({"v": 1, "writer": writer, "command": command, "phase": phase, "edge": edge,
            "exit_code": code, "signal": None, "errno": None, "failure_class": None,
            "worker": worker, "cleanup_requested": False, "counters": counters})
    def pair(writer, command, phase, **kwargs):
        event(writer, command, phase, "begin", worker=kwargs.get("worker"))
        event(writer, command, phase, "ok", **kwargs)
    for phase in ("owner_guard", "key_preproof", "proof_preflight"): pair("core", "key", phase)
    pair("core", "info", "proof_info", code=0)
    event("core", "prove", "proof_prove", "begin")
    for phase in ("host_input", "host_succinct", "host_succinct_verify"): pair("host", "prove", phase)
    event("host", "prove", "host_groth16", "begin")
    for phase in ("shim_validate", "shim_lock", "shim_fifo"): pair("shim", "prove", phase)
    for worker in ("generator", "prover"): pair("shim", "prove", worker + "_spawn", worker=worker)
    event("shim", "prove", "shim_workers", "begin")
    for worker in ("generator", "prover"): event("shim", "prove", "worker_wait", "ok", worker=worker, code=0)
    event("shim", "prove", "shim_workers", "ok")
    pair("shim", "prove", "shim_commit")
    event("host", "prove", "host_groth16", "ok")
    for phase in ("host_receipt_verify", "host_receipt_publish"): pair("host", "prove", phase)
    event("core", "prove", "proof_prove", "ok", code=0)
    event("core", "verify", "proof_verify", "begin")
    pair("host", "verify", "host_receipt_verify")
    event("core", "verify", "proof_verify", "ok", code=0)
    for phase in ("key_proof_store", "register_tx", "registry_after", "owner_response"): pair("core", "key", phase)
    return records


def self_test_diagnostics(subject):
    source, artifacts = "1" * 40, {name: "a" * 64 for name in ("prover", "r0vm", "profile")}
    cli = {"exit_code": 0, "signal": None, "spawn_errno": None}
    records = self_test_diagnostic_fixture()
    encode = lambda items: b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode() for record in items)
    full = encode(records)
    positive = subject.registration_diagnostics(full, source, artifacts, cli)
    subject.need(positive["availability"] == "complete" and len(positive["events"]) == len(records) <= 64
         and len(positive["resource_deltas"]) == 8 and len(full) <= 32768, "complete_registration_diagnostic_path_fits_bounds")
    subject.need(all(row["memory_max"] == 2 and row["pids_max"] == 0 and row["oom"] == 0 for row in positive["resource_deltas"]),
         "diagnostic_pressure_not_invented_oom_or_pid_failure")
    bad = []
    for key, item in (("exit_code", True), ("signal", 65), ("errno", 4096), ("phase", "UNAPPROVED_PRIVATE_SENTINEL"), ("private", "UNAPPROVED_PRIVATE_SENTINEL")):
        changed = copy.deepcopy(records); changed[0][key] = item; bad.append(encode(changed))
    changed = copy.deepcopy(records); changed[2]["counters"]["memory_max"] = 3; bad.append(encode(changed))
    bad += [full + b"x" * 32769, encode(records + records), encode([records[1], records[0], *records[2:]]),
            encode([records[0], records[0], *records[1:]]), b" " * 513 + b"\n", full.replace(b'"v":1', b'"v":1,"v":1', 1)]
    bad.append(encode([record for record in records if record["writer"] != "host"]))
    for data in bad:
        value = subject.registration_diagnostics(data, source, artifacts, cli)
        subject.need(value["availability"] == "unavailable" and not value["events"]
             and "UNAPPROVED_PRIVATE_SENTINEL" not in json.dumps(value), "invalid_diagnostics_are_unavailable_without_private_text")
    missing = copy.deepcopy(records)
    for record in missing: record["counters"] = None
    projected = subject.registration_diagnostics(encode(missing), source, artifacts, cli)
    subject.need(all(all(row[name] is None for name in subject.DIAGNOSTIC_COUNTERS) for row in projected["resource_deltas"]), "unobserved_counters_are_null")
    boundary = next(index for index, record in enumerate(records) if record["phase"] == "worker_wait")
    for signal_number, exit_code in ((9, None), (None, 137)):
        failed = copy.deepcopy(records[:boundary + 1])
        failed[-1].update(edge="error", signal=signal_number, exit_code=exit_code,
                          failure_class="worker_signal" if signal_number else "worker_exit")
        value = subject.registration_diagnostics(encode(failed), source, artifacts, {**cli, "exit_code": 1})
        subject.need(value["availability"] == "partial" and value["first_observed_failure"]["signal"] == signal_number
             and value["first_observed_failure"]["exit_code"] == exit_code, "worker_signal_not_inferred_from_137")
    partial = value
    cleanup = copy.deepcopy(failed)
    cleanup.append({**records[boundary + 1], "edge": "error", "exit_code": None, "signal": 15,
                    "failure_class": "worker_signal", "cleanup_requested": True})
    value = subject.registration_diagnostics(encode(cleanup), source, artifacts, partial["cli"])
    subject.need(value["availability"] == "partial" and value["first_observed_failure"] == partial["first_observed_failure"]
         and value["events"][-1]["cleanup_requested"], "cleanup_failure_preserves_first_worker_failure")
    invalid_waits = []
    for change in ({"exit_code": 0}, {"failure_class": "worker_signal"},
                   {"exit_code": None, "signal": 9, "failure_class": "worker_exit"}, {"errno": 5},
                   {"signal": 9}, {"exit_code": None},
                   {"edge": "ok", "exit_code": 1, "failure_class": None},
                   {"edge": "ok", "exit_code": None, "failure_class": None}):
        raw = copy.deepcopy(failed); raw[-1].update(change)
        public = copy.deepcopy(partial); public["events"][-1].update(change)
        public["first_observed_failure"] = subject.diagnostic_first_failure(public["events"])
        public["first_observed_failed_phase"] = subject.diagnostic_phase(next((event for event in public["events"] if event["edge"] == "error"), None))
        invalid_waits.append((raw, public))
    raw, public = copy.deepcopy(records), copy.deepcopy(positive)
    for event in [*raw, *public["events"]]:
        if event["phase"] == "worker_wait": event["exit_code"] = None
    invalid_waits.append((raw, public))
    for raw, public in invalid_waits:
        value = subject.registration_diagnostics(encode(raw), source, artifacts, public["cli"])
        subject.need(value["availability"] == "unavailable" and not value["events"] and not value["resource_deltas"]
             and all(value[key] is None for key in ("first_observed_failure", "last_observed_phase", "first_observed_failed_phase")),
             "contradictory_worker_wait_raw_is_unavailable")
        try: subject.project_registration_diagnostics(public, source, artifacts)
        except subject.CheckFailure: pass
        else: raise AssertionError("contradictory worker_wait public projection accepted")
    core_end = next(index for index, event in enumerate(records) if event["writer"] == "core" and event["phase"] == "proof_prove" and event["edge"] == "ok")
    for failure_class in ("result_decode", "receipt_check"):
        core_failed = copy.deepcopy(records[:core_end + 1])
        core_failed[-1].update(edge="error", failure_class=failure_class)
        value = subject.registration_diagnostics(encode(core_failed), source, artifacts, {**cli, "exit_code": 1})
        subject.need(value["availability"] == "partial" and value["events"][-1]["exit_code"] == 0
             and value["events"][-1]["failure_class"] == failure_class, "core_validation_can_fail_after_successful_subprocess")
    print(f"Registration diagnostic self-tests passed; full synthetic path {len(records)} events/8 resource groups; no processes or proofs")


def forbidden(*_args, **_kwargs):
    raise AssertionError("no-process self-test attempted an external process or network")


def self_test_runtime(subject):
    node_helpers = _load_module(subject.REPOSITORY / "scripts/verify-node.py", "verify_node_selftest")
    Client = node_helpers.Client
    for identity in (None, Path("/artifact/client-identity.json")):
        stdin = io.StringIO()
        stdout = io.StringIO('{"ok":true,"result":{"device_id":"synthetic-client"}}\n'
                             '{"ok":true,"result":{"status":"ready"}}\n')
        process = SimpleNamespace(stdin=stdin, stdout=stdout, wait=Mock(return_value=0), kill=Mock())
        with patch.dict(os.environ, {"UV_THREADPOOL_SIZE": "7", "VIBEFACE_HELPER_SELFTEST_SENTINEL": "unchanged"}):
            parent_environment = dict(os.environ)
            with patch.object(subprocess, "Popen", return_value=process) as spawn, \
                 patch.object(select, "select", return_value=([stdout], [], [])) as ready:
                client = Client("http://127.0.0.1:1", Path("/artifact/client.mjs"), identity_file=identity)
                for timeout in (0, 31):
                    try: client.command("identity", _wait_timeout=timeout)
                    except AssertionError as error:
                        subject.need(str(error) == "client command wait stays within its original deadline", "client_wait_bound_rejects_before_spawn")
                    else: raise AssertionError("client command accepted an invalid wait bound")
                subject.need(spawn.call_count == 0, "client_invalid_wait_never_spawns")
                subject.need(client.command("identity") == {"device_id": "synthetic-client"}, "client_first_json_response")
                subject.need(client.command("request", method="status", params={}, _wait_timeout=7) == {"status": "ready"}, "client_second_json_response")
                subject.need([json.loads(line) for line in stdin.getvalue().splitlines()] == [
                    {"op": "identity"}, {"op": "request", "method": "status", "params": {}}], "client_original_json_requests")
                client.close()
                subject.need(spawn.call_count == 1, "client_reuses_its_helper_process")
                subject.need(spawn.call_args.args == (["node", "--v8-pool-size=1", "/artifact/client.mjs", *([str(identity)] if identity else [])],),
                     "client_helper_v8_pool_is_fixed")
                subject.need(spawn.call_args.kwargs == {"stdin": subprocess.PIPE, "stdout": subprocess.PIPE, "stderr": subprocess.DEVNULL,
                     "text": True, "bufsize": 1, "env": {**parent_environment, "UV_THREADPOOL_SIZE": "1"}}, "client_helper_child_pool_and_stdio_preserved")
                subject.need(dict(os.environ) == parent_environment, "client_helper_does_not_mutate_parent_environment")
                subject.need([call.args for call in ready.call_args_list] == [([stdout], [], [], 30), ([stdout], [], [], 7)], "client_original_wait_bounds")
                subject.need(stdin.closed and client.process is None, "client_close_sends_eof_and_releases_process")
                subject.need(process.wait.call_count == 1 and process.wait.call_args.args == ()
                     and process.wait.call_args.kwargs == {"timeout": 5}, "client_close_original_wait_bound")
                subject.need(process.kill.call_count == 0, "client_normal_close_does_not_kill")
        stdout.close()
    subject.need(subprocess.Popen is forbidden and subprocess.run is forbidden and socket.socket is forbidden, "client_mock_preserves_outer_process_and_network_gate")
    sample = {"schema_version": 1, "source_commit": "1" * 40,
        "paths": {"repository": str(subject.REPOSITORY), "binary": "/artifact/vibeface", "core_binary": "/artifact/vibeface-core", "crypto_module": "/artifact/vibeface_secure.js", "prover": "/artifact/vibeface-delivery-prover", "r0vm": "/artifact/r0vm"},
        "sha256": {name: "a" * 64 for name in ("binary", "core_binary", "crypto_module", "crypto_wasm", "prover", "r0vm", "profile")},
        "elf_sha256": "b" * 64, "caps": {"deposit": "10", "max_service_amount": "5", "source_gas_cap_wei": subject.CAP, "provider_gas_cap_wei": subject.CAP, "register_key_gas_cap_wei": subject.CAP, "native_funding_wei_each": str(10 ** 19)}, "publication_policy": subject.POLICY}
    subject.validate_config(sample)
    for invalid_channel in (None, "mode", "inode", "overflow", "closed"):
        with tempfile.TemporaryDirectory(prefix="protected-diagnostic-channel-selftest-", dir=subject.ARTIFACTS) as temporary:
            experiment = subject.Experiment(sample, {}, Path(temporary) / "native")
            receiver = SimpleNamespace(directory=experiment.run / "R")
            receiver.directory.mkdir(mode=0o700)
            intent = str(uuid.uuid4())
            experiment.prepare_registration_diagnostics(receiver, intent)
            channel = experiment.registration_channel
            subject.need(channel is not None and not os.get_inheritable(channel["fd"]), "native_diagnostic_descriptor_is_not_inherited")
            data = b"".join((json.dumps(item, separators=(",", ":")) + "\n").encode() for item in self_test_diagnostic_fixture())
            os.write(channel["fd"], data)
            experiment.registration_cli["exit_code"] = 0
            if invalid_channel == "mode": os.fchmod(channel["fd"], 0o644)
            if invalid_channel == "inode": channel["identity"] = (0, 0)
            if invalid_channel == "overflow": os.ftruncate(channel["fd"], 32769)
            if invalid_channel == "closed": os.close(channel["fd"])
            experiment.retire_registration_diagnostics()
            experiment.publish_registration_diagnostics()
            value = subject.diagnostic_json((experiment.public / "registration-diagnostics.json").read_bytes())
            subject.need(value["availability"] == ("complete" if invalid_channel is None else "unavailable"), "bound_diagnostic_channel_validation")
            subject.need(experiment.summary["status"] == "RUNNING" and not experiment.summary["genuine_receipts"], "diagnostics_cannot_complete_business")
            try: os.fstat(channel["fd"])
            except OSError: pass
            else: raise AssertionError("native diagnostic descriptor leaked")
    for change in (lambda item: item.update(source_commit="0" * 40), lambda item: item.update(private_key="forbidden"),
                   lambda item: item["paths"].update(binary="/unrelated/trash/vibeface"), lambda item: item["sha256"].update(prover="0" * 64),
                   lambda item: item["publication_policy"].update(clock_skew_seconds=0), lambda item: item["caps"].update(deposit="11")):
        invalid = copy.deepcopy(sample); change(invalid)
        try: subject.validate_config(invalid)
        except subject.CheckFailure: pass
        else: raise AssertionError("invalid protected config accepted")
    journal = bytes(range(256)) * 5 + bytes(range(224))
    for receipt in (b"prefix" + journal + b"suffix", b"prefix" + journal + b"middle" + journal + b"suffix"):
        subject.need(subject.extract_journal(receipt, subject.digest(journal)) == journal, "unique_public_journal_content")
    for invalid in (b"missing", journal[:-1], b"x" + journal[1:], b"x" * 16385):
        try: subject.extract_journal(invalid, subject.digest(journal))
        except subject.CheckFailure: pass
        else: raise AssertionError("missing, altered or oversized public journal accepted")
    for relation in (1, 2, 3):
        fields = [bytes([7]) * 32 for _ in range(47)]; fields[1] = relation.to_bytes(32, "big")
        subject.need(len(subject.wrong_journal(b"".join(fields))) == 1504, "wrong_journal_shape")
    for url in ("https://127.0.0.1:8545", "http://example.com:8545", "http://user@127.0.0.1:8545", "http://127.0.0.1:8545/path"):
        try: subject.loopback(url)
        except (subject.CheckFailure, ValueError): pass
        else: raise AssertionError("non-loopback origin accepted")
    subject.need(subject.oom_delta({"memory.events": "oom 0\noom_kill 0\n"}, {"memory.events": "oom 1\noom_kill 0\n"})["oom"] == 1, "actual_oom_delta")
    for preconfigured in (False, True):
        with tempfile.TemporaryDirectory(prefix="protected-fresh-settings-selftest-", dir=subject.ARTIFACTS) as temporary:
            run = Path(temporary)
            nodes = []
            for index, name in enumerate(("S", "R", "P")):
                node = object.__new__(node_helpers.Node)
                node.name = name; node.directory = run / name; node.directory.mkdir()
                node.device_id = str(index + 1) * 64; node.quic_port = 27001 + index
                node.start = Mock(side_effect=subject.CheckFailure("synthetic_stop_before_real_node_start"))
                subject.save_json(node.directory / "config.json", {"network": {}})
                nodes.append(node)
            if preconfigured: subject.save_json(nodes[0].directory / "delivery-settings.json", {"enabled": False, "paid_enabled": False})
            startup = object.__new__(subject.Experiment)
            startup.run = run; startup.paths = {name: Path(value) for name, value in sample["paths"].items()}
            startup.nodes = []; startup.progress = Mock(); startup.deployment = {"funder":"0x" + "11" * 20}
            startup.prepare_proof_diagnostics = Mock()
            startup.rpc = Mock(side_effect=[["0x" + value * 20 for value in ("11","22","33","44")],"0x7a69"])
            startup.helpers = {"CLIENT_SOURCE":"MODULE_URL", "mesh_network":Mock(return_value="t-123456789abc"),
                "public_node_args":Mock(return_value=["--relay-open"]), "Node":Mock(side_effect=nodes), "isolate_delivery_node":Mock()}
            try: startup.setup_nodes()
            except subject.CheckFailure as error:
                subject.need(str(error) == ("fresh_delivery_settings_must_be_absent" if preconfigured else "synthetic_stop_before_real_node_start"),
                     "fresh_settings_use_independent_file_contract")
            else: raise AssertionError("the settings selfcheck must stop before real Node startup")
            startup.helpers["mesh_network"].assert_called_once_with()
            subject.need([node.start.call_count for node in nodes] == [0, 0, 0 if preconfigured else 1],
                 "only_absent_delivery_settings_start_the_independent_provider_first")
            for node in nodes:
                config = json.loads((node.directory / "config.json").read_bytes())
                subject.need(set(config) == {"network"}, "main_config_must_not_gain_delivery_fields")
                subject.need(config["network"] == ({} if preconfigured else {"mesh": {"enabled": True, "network": "t-123456789abc",
                    "bootstrap": [] if node.name == "P" else [nodes[2].mesh_address], "allow_private": True}}),
                    "private_mesh_has_only_s_r_p_loopback_links_and_no_public_bootstrap")
    for response in (b"R", b"X", b"", None):
        reader, writer = os.pipe()
        if response is not None:
            os.write(writer, response)
        os.close(writer)
        with patch.dict(os.environ, {subject.OBSERVER_READY_FD: str(reader)}), \
             patch("select.select", return_value=([] if response is None else [reader], [], [])):
            try: subject.wait_observer_ready()
            except subject.CheckFailure as error:
                subject.need(response != b"R" and str(error) == "restart_observer_not_ready_before_business", "observer_not_ready_is_fail_closed")
            else: subject.need(response == b"R", "only_actual_ready_byte_releases_business")
            subject.need(subject.OBSERVER_READY_FD not in os.environ, "ready_descriptor_not_inherited_by_business")
        try: os.fstat(reader)
        except OSError: pass
        else: raise AssertionError("the native readiness reader remained open")
    with tempfile.TemporaryDirectory(prefix="protected-ready-selftest-", dir=subject.ARTIFACTS) as temporary:
        startup = subject.Experiment(sample, {}, Path(temporary) / "native")
        def ready_before_processes():
            subject.need(startup.public.is_dir() and not startup.nodes and startup.deployment is None,
                 "fresh_directories_exist_before_observer_gate")
            raise subject.CheckFailure("synthetic_stop_at_readiness_gate")
        with patch.object(subject, "wait_observer_ready", ready_before_processes):
            try: startup.start()
            except subject.CheckFailure as error: subject.need(str(error) == "synthetic_stop_at_readiness_gate", "ready_precedes_original_start")
            else: raise AssertionError("startup bypassed the readiness gate")
    class PredicateObserved(Exception):
        pass
    complete_objects = [(0, "ciphertext", 1, True), (0, "capsule", 1, True)]
    cases = [("a-pending", "a", None, 2, ["a", "b"], None),
             ("b-pending", "b", None, 2, ["a", "b"], None),
             ("one-source", None, None, 1, ["a", "b"], None),
             ("missing-offer", None, None, 2, ["a"], None),
             ("ready", None, None, 2, ["a", "b"], True)]
    for message in ("proof task exceeded its execution deadline", "proof timeout", "prover timed out",
                    "executor refused work", "RPC exceeded its execution deadline"):
        cases.append((message, "a", message, 2, ["a", "b"], None))
    with tempfile.TemporaryDirectory(prefix="protected-source-predicate-selftest-", dir=subject.ARTIFACTS) as temporary:
        for name, missing, error_message, count, offers, expected_ready in cases:
            source, receiver, provider = object(), object(), object()
            experiment = object.__new__(subject.Experiment)
            experiment.nodes = [source, receiver, provider]
            experiment.orders = {key: {"deadlines": {"key_deadline": 123}} for key in ("a", "b")}
            experiment.summary = {}; experiment.run = Path(temporary)
            experiment.proofs = {key: {"entry": {"relation": 2}} for key in ("a", "b")[:count]}
            def result(node, operation, **params):
                if operation == "list":
                    subject.need(node is receiver, "offers_come_from_receiver")
                    return {"orders": [{"order_id": key} for key in offers]}
                subject.need(node is source and operation == "get", "source_state_checked_before_readiness")
                if params["order_id"] == "b" and error_message:
                    return {"error": error_message}
                return {"deadlines": {"key_deadline": 123}}
            experiment.call = Mock(side_effect=result)
            experiment.rows = Mock(side_effect=lambda _node, _query, params: [] if params[0] == missing else complete_objects)
            observed = []
            def wait(label, predicate, *, timeout):
                subject.need(timeout == 16000, "original_source_stage_bound_unchanged")
                observed.append(predicate())
                raise PredicateObserved()
            experiment.wait = wait
            try: experiment.wait_allocations_and_stop_source()
            except PredicateObserved: subject.need(not error_message and observed == [expected_ready], "actual_source_predicate_result")
            except subject.CheckFailure as error:
                code = "actual_product_proof_execution_deadline_exceeded" if error_message and error_message.startswith(("proof", "prover")) else "actual_paid_order_executor_failed_private_log_retained"
                subject.need(error_message and str(error) == code and experiment.summary["failure_order_id"] == "b", "actual_get_failure_classification")
                subject.need(json.loads((experiment.run / "order-failure-b.json").read_bytes()) == {"error": error_message}, "actual_order_error_retained_privately")
                experiment.rows.assert_not_called()
            else: raise AssertionError("the predicate selfcheck reached business operations")
            subject.need([call.kwargs["order_id"] for call in experiment.call.call_args_list if call.args[1] == "get"] == ["a", "b"],
                 "all_source_errors_checked_before_pending_objects")
    experiment = object.__new__(subject.Experiment)
    experiment.progress = Mock(); experiment.scan_receipts = Mock(); experiment.checkpoint = Mock()
    experiment.processes = SimpleNamespace(capture=Mock())
    busy = {"error": "another delivery operation is in progress"}
    requests = {
        "prepare": {"intent_id": "00000000-0000-4000-8000-000000000001", "budget_id": "b" * 64},
        "confirm_batch": {"intent_ids": ["00000000-0000-4000-8000-000000000001", "00000000-0000-4000-8000-000000000002"]},
        "accept_batch": {"order_ids": ["a" * 64, "b" * 64]},
    }
    for op, params in requests.items():
        client = Mock(command=Mock(side_effect=[busy, {"result": {"accepted": True}}]))
        with patch("time.monotonic", return_value=0), patch("time.sleep"):
            subject.need(experiment.call(SimpleNamespace(client=client), op, **params) == {"accepted": True}, "explicit_busy_then_success")
        subject.need(client.command.call_count == 2 and client.command.call_args_list[0] == client.command.call_args_list[1], "busy_retry_keeps_exact_request")
        subject.need(client.command.call_args.kwargs["params"] == {"op": op, **params}, "busy_retry_keeps_original_ids")
        client.call.assert_not_called()
        for error in (TimeoutError("network outcome unknown"), RuntimeError("another delivery operation is in progress"),
                      {"error": "outcome unknown"}, {"error": "another delivery operation is in progress; uncertain"}, {"error": "other refusal"}):
            client = Mock(command=Mock(side_effect=[error] if isinstance(error, Exception) else None, return_value=error))
            with patch("time.monotonic", return_value=0), patch("time.sleep") as pause:
                try: experiment.call(SimpleNamespace(client=client), op, **params)
                except (subject.CheckFailure, TimeoutError, RuntimeError): pass
                else: raise AssertionError("a non-busy failure was retried or accepted")
            subject.need(client.command.call_count == 1, "non_busy_failure_is_not_retried"); pause.assert_not_called()
        client = Mock(command=Mock(return_value=busy))
        with patch("time.monotonic", side_effect=[0, 0, 0, 0, 121]), patch("time.sleep"):
            try: experiment.call(SimpleNamespace(client=client), op, **params)
            except subject.CheckFailure as error: subject.need("stage_observer_timeout_" in str(error), "busy_keeps_existing_stage_bound")
            else: raise AssertionError("busy exceeded the original stage bound")
        subject.need(client.command.call_count == 1, "expired_busy_stage_sends_no_new_request")
    client = Mock(call=Mock(side_effect=AssertionError("another delivery operation is in progress")))
    try: experiment.call(SimpleNamespace(client=client), "get", order_id="a" * 64)
    except AssertionError: pass
    else: raise AssertionError("read operations must retain their original Client.call behavior")
    client.command.assert_not_called()
    print(json.dumps({"status": "SELF_TEST_PASS", "external_processes": 0, "network_sockets": 0, "genuine_receipts_generated": 0}))


def self_test_receipt_reconcile(subject):
    """Keep receipt reconciliation as its own stable case ID."""
    artifacts = {"prover": "b" * 64, "r0vm": "c" * 64, "profile": "d" * 64}
    profile = "f" * 64
    principals = {node: {"peer": value * 32} for node, value in (("S", "11"), ("R", "22"))}
    orders = [{"order_id": value * 32} for value in ("ab", "cd")]
    for node in ("S", "R"):
        records = self_test_proof_fixture(node, 0, artifacts, profile, "receipt_reconcile")
        encoded = b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode() for record in records)
        job = subject.summarize_proof_job(encoded, node, 0, principals[node]["peer"], orders[0]["order_id"], profile, artifacts, [1, 2], records[0]["job"], [])
        subject.need(job["availability"] == "complete" and job["mode"] == "receipt_reconcile" and job["source_activity"] is None, "receipt_reconciliation_does_not_claim_new_prove")


# Legacy -> stable case inventory: diagnostics, proof jobs/reconciliation, review, runtime.
CASE_REGISTRY = (
    ("diagnostic_schema", self_test_diagnostics),
    ("proof_jobs", self_test_proof_diagnostics),
    ("receipt_reconcile", self_test_receipt_reconcile),
    ("review_regressions", self_test_proof_review_regressions),
    ("runtime_compatibility", self_test_runtime),
)
def run(subject=None):
    """Run the complete protected suite against an explicit ModuleType subject."""
    if subject is None:
        # runpy.run_path does not register its returned globals as sys.modules.
        subject = _load_module(SCRIPTS / "verify-delivery-protected.py", "verify_delivery_protected_selftest_target")
    if not isinstance(subject, types.ModuleType):
        raise TypeError("protected self-tests require a ModuleType subject")
    with patch.object(subprocess, "Popen", forbidden), patch.object(subprocess, "run", forbidden), patch.object(socket, "socket", forbidden):
        for _case_id, case in CASE_REGISTRY:
            case(subject)

if __name__ == "__main__":
    run()
