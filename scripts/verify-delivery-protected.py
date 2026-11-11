#!/usr/bin/env python3
"""Fresh protected paid Node acceptance, called by verify-node with final artifacts."""

import argparse
import base64
import copy
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import platform
import runpy
import re
import select
import signal
import sqlite3
import stat
import subprocess
import sys
import time
import traceback
import urllib.parse
import urllib.request
import uuid


REPOSITORY = Path(__file__).resolve().parents[1]
ARTIFACTS = REPOSITORY / ".artifacts"
CAP = "100000000000000000"
POLICY = {"watchdog_seconds": 60, "inclusion_seconds": 60, "finality_seconds": 10,
          "clock_skew_seconds": 5, "max_fee_per_gas": "3000000000", "priority_fee_per_gas": "1000000000"}
PROFILE_FIELDS = ("crypto_suite", "max_windows", "max_window_bytes", "max_total_bytes", "max_guest_cycles", "max_prover_memory_bytes")
OBSERVER_READY_FD = "VIBEFACE_PROTECTED_OBSERVER_READY_FD"
NOT_CHECKED = ["old fee5 failure remains its original baseline", "same bytes or maximum size as old fee5 case",
               "strict or Open relations", "Web or Android rendered UI", "public network or real assets",
               "absolute finality or real-world economics", "cross-container chain/order resume",
               "ordinary free discovery and charity transfer regression",
               "forced reorg, nonce-consuming revert, near-cutoff, keeper or token-failure matrix"]


class CheckFailure(RuntimeError):
    pass


def need(value, code):
    if not value:
        raise CheckFailure(code)


DIAGNOSTIC_PHASES = {
    "core": ("owner_guard", "key_preproof", "proof_preflight", "proof_info", "proof_prove", "proof_verify", "key_proof_store", "register_tx", "registry_after", "owner_response"),
    "host": ("host_input", "host_succinct", "host_succinct_verify", "host_groth16", "host_receipt_verify", "host_receipt_publish"),
    "shim": ("shim_validate", "shim_lock", "shim_fifo", "generator_spawn", "prover_spawn", "shim_workers", "worker_wait", "shim_commit"),
}
DIAGNOSTIC_COUNTERS = ("pids_max", "memory_max", "oom", "oom_kill", "oom_group_kill")
DIAGNOSTIC_RESOURCES = {("core", "key_preproof"), ("core", "proof_info"), ("core", "proof_prove"), ("core", "proof_verify"),
                        ("core", "register_tx"), ("host", "host_succinct"), ("host", "host_groth16"), ("shim", "shim_workers")}
DIAGNOSTIC_EVENT = {"writer", "command", "phase", "edge", "exit_code", "signal", "errno", "failure_class", "worker", "cleanup_requested"}
DIAGNOSTIC_CLASSES = {"spawn", "io", "timeout", "output_limit", "result_decode", "receipt_check", "worker_exit", "worker_signal", "stage_error", "unknown"}


def diagnostic_integer(value, maximum, *, minimum=0):
    need(value is None or type(value) is int and minimum <= value <= maximum, "invalid_diagnostic_integer")


def diagnostic_json(data):
    def unique(pairs):
        value = {}
        for key, item in pairs:
            need(key not in value, "duplicate_diagnostic_json_key")
            value[key] = item
        return value
    return json.loads(data, object_pairs_hook=unique)


def diagnostic_events(events):
    need(type(events) is list and len(events) <= 64, "invalid_diagnostic_event_count")
    begun, ended, ranks, failed = {}, set(), {}, set()
    for event in events:
        exact(event, DIAGNOSTIC_EVENT, "invalid_diagnostic_event_fields")
        writer, command, phase, edge = (event[key] for key in ("writer", "command", "phase", "edge"))
        need(isinstance(writer, str) and writer in DIAGNOSTIC_PHASES and isinstance(phase, str)
             and phase in DIAGNOSTIC_PHASES[writer] and command in ("key", "info", "prove", "verify")
             and edge in ("begin", "ok", "error"), "invalid_diagnostic_stage")
        expected = {"proof_info": "info", "proof_prove": "prove", "proof_verify": "verify"}.get(phase, "key")
        need((writer == "core" and command == expected) or (writer == "host" and command == "prove")
             or (writer == "host" and command == "verify" and phase == "host_receipt_verify")
             or (writer == "shim" and command == "prove"), "invalid_diagnostic_command")
        for key, maximum in (("exit_code", 255), ("signal", 64), ("errno", 4095)):
            diagnostic_integer(event[key], maximum, minimum=1 if key in ("signal", "errno") else 0)
        need(not (event["exit_code"] is not None and event["signal"] is not None)
             and type(event["cleanup_requested"]) is bool, "invalid_diagnostic_termination")
        need(event["failure_class"] is None or isinstance(event["failure_class"], str)
             and event["failure_class"] in DIAGNOSTIC_CLASSES, "invalid_diagnostic_failure_class")
        role = event["worker"]
        need(role is None or role in ("generator", "prover"), "invalid_diagnostic_worker")
        if phase in ("generator_spawn", "prover_spawn"):
            need(role == phase.removesuffix("_spawn"), "invalid_diagnostic_spawn_worker")
        elif phase == "worker_wait":
            need(role in ("generator", "prover") and edge != "begin", "invalid_diagnostic_wait_worker")
            need(event["errno"] is None and (
                edge == "ok" and event["exit_code"] == 0 and event["signal"] is None and event["failure_class"] is None
                or edge == "error" and (
                    event["exit_code"] is not None and event["exit_code"] != 0 and event["signal"] is None
                    and event["failure_class"] == "worker_exit"
                    or event["exit_code"] is None and event["signal"] is not None and event["failure_class"] == "worker_signal")),
                "invalid_diagnostic_wait_termination")
        else:
            need(role is None and not event["cleanup_requested"], "unexpected_diagnostic_worker")
        need(edge == "error" or event["failure_class"] is None and event["signal"] is None and event["errno"] is None
             and event["exit_code"] in (None, 0), "invalid_diagnostic_success")
        need(edge != "error" or event["failure_class"] is not None, "missing_diagnostic_failure_class")
        key = (writer, command, phase, role)
        if edge == "begin":
            need((writer, command) not in failed or writer == "core" and phase == "owner_response", "diagnostics_continue_after_failure")
            rank = DIAGNOSTIC_PHASES[writer].index(phase)
            need(key not in begun and key not in ended and rank >= ranks.get((writer, command), -1), "duplicate_or_reordered_diagnostic_begin")
            begun[key] = event
            ranks[writer, command] = rank
        else:
            need(key not in ended and (key in begun or phase == "worker_wait"), "duplicate_or_unbegun_diagnostic_end")
            ended.add(key)
            if edge == "error": failed.add((writer, command))
    return begun, ended


def diagnostic_first_failure(events):
    return next(({key: event[key] for key in ("phase", "worker", "exit_code", "signal", "errno")}
                 for event in events if event["edge"] == "error" and event["worker"] is not None and not event["cleanup_requested"]), None)


def diagnostic_phase(event):
    return {key: event[key] for key in ("writer", "command", "phase")} if event is not None else None


def diagnostic_coverage(events):
    observed = {(event["writer"], event["command"], event["phase"], event["worker"], event["edge"]) for event in events}
    required = set()
    if ("core", "prove", "proof_prove", None, "ok") in observed:
        required.update(("host", "prove", phase, None, "ok") for phase in DIAGNOSTIC_PHASES["host"])
        required.update(("shim", "prove", phase, worker, "ok") for phase, worker in
            [("shim_validate", None), ("shim_lock", None), ("shim_fifo", None), ("generator_spawn", "generator"),
             ("prover_spawn", "prover"), ("shim_workers", None), ("worker_wait", "generator"), ("worker_wait", "prover"), ("shim_commit", None)])
    if ("core", "verify", "proof_verify", None, "ok") in observed:
        required.add(("host", "verify", "host_receipt_verify", None, "ok"))
    need(required <= observed, "successful_proof_missing_diagnostic_coverage")


def project_registration_diagnostics(value, source_commit, artifacts):
    exact(value, {"schema_version", "source_commit", "operation", "artifact_sha256", "availability", "cli", "events", "resource_deltas", "first_observed_failure", "last_observed_phase", "first_observed_failed_phase"}, "invalid_registration_diagnostic_fields")
    need(type(value["schema_version"]) is int and value["schema_version"] == 1 and value["source_commit"] == source_commit
         and re.fullmatch(r"[0-9a-f]{40}", source_commit) and value["operation"] == "register_key", "invalid_registration_diagnostic_binding")
    expected = {name: artifacts[name] for name in ("prover", "r0vm", "profile")}
    need(value["artifact_sha256"] == expected and all(isinstance(item, str) and re.fullmatch(r"[0-9a-f]{64}", item)
         for item in expected.values()), "invalid_registration_diagnostic_artifacts")
    need(value["availability"] in ("complete", "partial", "unavailable"), "invalid_registration_diagnostic_availability")
    exact(value["cli"], {"exit_code", "signal", "spawn_errno"}, "invalid_registration_cli_fields")
    for key, maximum in (("exit_code", 255), ("signal", 64), ("spawn_errno", 4095)):
        diagnostic_integer(value["cli"][key], maximum, minimum=0 if key == "exit_code" else 1)
    need(sum(value["cli"][key] is not None for key in value["cli"]) <= 1, "contradictory_registration_cli_termination")
    begun, ended = diagnostic_events(value["events"])
    diagnostic_coverage(value["events"])
    need(value["last_observed_phase"] == diagnostic_phase(value["events"][-1] if value["events"] else None)
         and value["first_observed_failed_phase"] == diagnostic_phase(next((event for event in value["events"] if event["edge"] == "error"), None)),
         "incorrect_diagnostic_observed_phase")
    rows = value["resource_deltas"]
    need(type(rows) is list and len(rows) <= 8, "invalid_diagnostic_resource_count")
    seen = set()
    for row in rows:
        exact(row, {"writer", "command", "phase", *DIAGNOSTIC_COUNTERS}, "invalid_diagnostic_resource_fields")
        key = (row["writer"], row["command"], row["phase"], None)
        need(key in begun and (row["writer"], row["phase"]) in DIAGNOSTIC_RESOURCES and key not in seen, "invalid_diagnostic_resource_phase")
        seen.add(key)
        for name in DIAGNOSTIC_COUNTERS: diagnostic_integer(row[name], 2**63-1)
        need(all(row[name] is None for name in DIAGNOSTIC_COUNTERS) or all(row[name] is not None for name in DIAGNOSTIC_COUNTERS), "partial_diagnostic_counter_set")
    expected_rows = {key for key in begun if (key[0], key[2]) in DIAGNOSTIC_RESOURCES}
    need(seen == expected_rows and value["first_observed_failure"] == diagnostic_first_failure(value["events"]), "incomplete_diagnostic_projection")
    if value["availability"] == "unavailable":
        need(not value["events"] and not rows and value["first_observed_failure"] is None, "unavailable_diagnostics_claim_events")
    elif value["availability"] == "complete":
        need(set(begun) <= ended and ("core", "key", "owner_response", None) in ended
             and value["cli"]["exit_code"] is not None, "incomplete_diagnostics_claim_complete")
    if value["cli"]["exit_code"] == 0:
        need(not any(event["edge"] == "error" and event["writer"] == "core" for event in value["events"]), "successful_cli_claims_core_failure")
    need(len(json.dumps(value, separators=(",", ":")).encode()) <= 65536, "oversized_registration_diagnostic_projection")
    return copy.deepcopy(value)


def registration_diagnostics(data, source_commit, artifacts, cli):
    value = {"schema_version": 1, "source_commit": source_commit, "operation": "register_key",
        "artifact_sha256": {name: artifacts[name] for name in ("prover", "r0vm", "profile")}, "availability": "unavailable",
        "cli": cli, "events": [], "resource_deltas": [], "first_observed_failure": None, "last_observed_phase": None, "first_observed_failed_phase": None}
    try:
        need(isinstance(data, bytes) and 0 < len(data) <= 32768 and data.endswith(b"\n"), "invalid_diagnostic_bytes")
        raw = []
        for line in data.splitlines():
            need(0 < len(line) + 1 <= 512, "invalid_diagnostic_line")
            record = diagnostic_json(line)
            exact(record, {"v", "counters", *DIAGNOSTIC_EVENT}, "invalid_private_diagnostic_fields")
            need(type(record["v"]) is int and record["v"] == 1, "invalid_private_diagnostic_version")
            counters = record["counters"]
            if counters is not None:
                exact(counters, DIAGNOSTIC_COUNTERS, "invalid_private_diagnostic_counters")
                for item in counters.values():
                    need(type(item) is int and 0 <= item <= 2**63-1, "invalid_private_diagnostic_counter")
                need((record["writer"], record["phase"]) in DIAGNOSTIC_RESOURCES, "unexpected_private_diagnostic_counters")
            raw.append(record)
        events = [{key: record[key] for key in DIAGNOSTIC_EVENT} for record in raw]
        begun, ended = diagnostic_events(events)
        before, deltas = {}, []
        for record in raw:
            key = (record["writer"], record["command"], record["phase"], record["worker"])
            if (record["writer"], record["phase"]) not in DIAGNOSTIC_RESOURCES: continue
            if record["edge"] == "begin":
                before[key] = record["counters"]
                deltas.append({"writer": key[0], "command": key[1], "phase": key[2], **dict.fromkeys(DIAGNOSTIC_COUNTERS)})
            elif key in before and before[key] is not None and record["counters"] is not None:
                delta = {name: record["counters"][name] - before[key][name] for name in DIAGNOSTIC_COUNTERS}
                need(all(item >= 0 for item in delta.values()), "decreased_diagnostic_counter")
                next(row for row in deltas if (row["writer"], row["command"], row["phase"]) == key[:3]).update(delta)
        value.update(events=events, resource_deltas=deltas, first_observed_failure=diagnostic_first_failure(events),
            last_observed_phase=diagnostic_phase(events[-1] if events else None),
            first_observed_failed_phase=diagnostic_phase(next((event for event in events if event["edge"] == "error"), None)),
            availability="complete" if set(begun) <= ended and ("core", "key", "owner_response", None) in ended and cli["exit_code"] is not None else "partial")
        return project_registration_diagnostics(value, source_commit, artifacts)
    except (CheckFailure, ValueError, TypeError, KeyError, UnicodeError, RecursionError):
        value.update(availability="unavailable", events=[], resource_deltas=[], first_observed_failure=None, last_observed_phase=None, first_observed_failed_phase=None)
        return project_registration_diagnostics(value, source_commit, artifacts)


PROOF_PHASES = {"core": ("proof_job", "proof_info", "proof_prove", "proof_verify"),
                "host": DIAGNOSTIC_PHASES["host"], "shim": DIAGNOSTIC_PHASES["shim"]}
PROOF_IO_PHASES = ("process_spawn", "stdin_closed", "stdout_complete", "process_wait")
PROOF_EVENT = {"v", "kind", "w", "c", "p", "e", "t", "x", "s", "n", "f", "r", "k", "g", "q", "i", "a"}
PROOF_JOB_FIELDS = {"node", "order_index", "relation", "window", "job_digest", "availability", "binding", "mode",
                    "event_count", "first_observed_failure", "last_open_phase", "command", "resources", "source_activity"}
PROOF_COMMAND_FIELDS = {"command", "spawned", "stdin_write_complete", "stdin_closed", "stdout_eof", "stdout_bytes",
                        "wait_observed", "exit_code", "signal", "spawn_errno", "timed_out", "failure_class", "start_ns", "end_ns"}
SOURCE_ACTIVITY_FIELDS = ("usage_usec", "user_usec", "system_usec", "cpu_some_total", "memory_some_total", "memory_full_total", "io_some_total")


def proof_hex(value, length=64):
    need(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{" + str(length) + r"}", value), "invalid_proof_diagnostic_hex")


def proof_group(value):
    if value is not None:
        need(type(value) is list and len(value) == 2, "invalid_proof_diagnostic_group")
        for item in value:
            need(type(item) is int and 0 <= item <= 2**64-1, "invalid_proof_diagnostic_group_integer")


def proof_values(value, count, *, nullable=False):
    need(type(value) is list and len(value) == count, "invalid_proof_diagnostic_values")
    for item in value:
        need(nullable and item is None or type(item) is int and 0 <= item <= 2**63-1, "invalid_proof_diagnostic_counter")


def proof_io(value, command, exit_code, signal_number, failure):
    need(type(value) is list and len(value) == 7, "invalid_proof_diagnostic_io")
    spawned, written, closed, eof, count, waited, timed_out = value
    need(all(type(item) is bool for item in (spawned, written, closed, eof, waited, timed_out)), "invalid_proof_diagnostic_io_boolean")
    limit = 16384 if command == "prove" else 4096
    diagnostic_integer(count, limit + 1)
    need(not written or spawned, "proof_write_before_spawn")
    need(not closed or written, "proof_close_before_write")
    need(count is None or spawned, "proof_output_before_spawn")
    need(not eof or count is not None and count <= limit, "proof_output_limit_is_not_eof")
    need(not waited or spawned and closed and eof, "proof_wait_without_completed_io")
    need(not timed_out or not waited, "proof_timeout_is_not_observed_wait")
    need(waited or exit_code is None and signal_number is None, "proof_unobserved_wait_status")
    need(not waited or (exit_code is None) != (signal_number is None), "proof_wait_requires_actual_status")
    need(exit_code is None or signal_number is None, "proof_exit_and_signal_are_distinct")
    if failure == "worker_exit": need(waited and exit_code is not None and exit_code > 0 and signal_number is None, "proof_worker_exit_needs_nonzero_wait")
    if failure == "worker_signal": need(waited and exit_code is None and signal_number is not None, "proof_worker_signal_needs_signal_wait")
    if failure == "spawn": need(not spawned and not waited, "proof_spawn_failure_has_no_process")
    if failure == "timeout": need(timed_out and not waited, "proof_timeout_has_no_wait_status")
    if failure == "output_limit": need(count == limit + 1 and not eof and not waited, "proof_output_limit_requires_actual_excess")
    if failure in ("result_decode", "receipt_check"): need(waited and exit_code == 0 and signal_number is None, "proof_validation_failure_preserves_successful_wait")


def unavailable_proof_job(node, index):
    return {"node": node, "order_index": index, "relation": 2 if node == "S" else 3, "window": 0,
            "job_digest": None, "availability": "unavailable", "binding": "unavailable", "mode": "unavailable", "event_count": 0,
            "first_observed_failure": None, "last_open_phase": None, "command": None, "resources": None, "source_activity": None}


def proof_availability(jobs):
    return "complete" if len(jobs) == 4 and all(job["availability"] == "complete" for job in jobs) else (
        "partial" if any(job["availability"] != "unavailable" for job in jobs) else "unavailable")


def project_proof_diagnostics(value, source_commit, artifacts, profile_id, principals, orders, genuine_receipts):
    if value is None:
        return None
    exact(value, {"schema_version", "source_commit", "artifact_sha256", "profile_id", "counter_fields", "attempt_policy", "availability", "jobs"}, "invalid_proof_diagnostic_fields")
    need(type(value["schema_version"]) is int and value["schema_version"] == 1 and value["source_commit"] == source_commit, "invalid_proof_diagnostic_source")
    proof_hex(source_commit, 40)
    need(value["artifact_sha256"] == {name: artifacts[name] for name in ("prover", "r0vm", "profile")}, "invalid_proof_diagnostic_artifacts")
    for item in value["artifact_sha256"].values(): proof_hex(item)
    need(value["profile_id"] == profile_id, "invalid_proof_diagnostic_profile")
    if profile_id is not None: proof_hex(profile_id)
    need(value["counter_fields"] == list(DIAGNOSTIC_COUNTERS) and value["attempt_policy"] == "first_admitted", "invalid_proof_diagnostic_policy")
    jobs = value["jobs"]
    need(type(jobs) is list and len(jobs) <= 4 and type(orders) is list and len(orders) <= 2, "invalid_proof_diagnostic_job_count")
    need(len({order["order_id"] for order in orders}) == len(orders), "duplicate_proof_diagnostic_order")
    seen, job_digests = set(), set()
    for job in jobs:
        exact(job, PROOF_JOB_FIELDS, "invalid_proof_diagnostic_job_fields")
        node, index = job["node"], job["order_index"]
        need(node in ("S", "R") and type(index) is int and 0 <= index < len(orders)
             and type(job["relation"]) is int and job["relation"] == (2 if node == "S" else 3)
             and type(job["window"]) is int and job["window"] == 0 and (node, index) not in seen, "invalid_proof_diagnostic_slot")
        seen.add((node, index)); proof_hex(principals[node]["peer"]); proof_hex(orders[index]["order_id"])
        need(job["availability"] in ("complete", "partial", "unavailable"), "invalid_proof_diagnostic_availability")
        if job["availability"] == "unavailable":
            need(job == unavailable_proof_job(node, index), "unavailable_proof_diagnostic_claims_observations")
            continue
        proof_hex(job["job_digest"])
        need(job["job_digest"] not in job_digests, "duplicate_proof_diagnostic_digest")
        job_digests.add(job["job_digest"])
        need(profile_id is not None and job["binding"] in ("reserved_job", "verified_receipt")
             and job["mode"] in ("not_started", "prove", "receipt_reconcile")
             and type(job["event_count"]) is int and 0 <= job["event_count"] <= 62, "invalid_proof_diagnostic_job_identity")
        if job["binding"] == "verified_receipt":
            need(any(fact.get("relation") == job["relation"] and fact.get("orderId") == "0x" + orders[index]["order_id"]
                 and type(fact.get("windowIndex")) is int and fact["windowIndex"] == 0 and fact.get("journalHash") == job["job_digest"]
                 and fact.get("official_host_verified") is True and fact.get("wrong_journal_receipt_comparison_rejected") is True
                 for fact in genuine_receipts), "proof_diagnostic_receipt_binding")
        for name in ("first_observed_failure", "last_open_phase"):
            point = job[name]
            if point is None: continue
            fields = {"writer", "command", "phase", "t_ns", "event_index"}
            if name == "first_observed_failure": fields |= {"failure_class", "worker", "exit_code", "signal", "errno", "cleanup_requested"}
            exact(point, fields, "invalid_proof_diagnostic_phase_ref")
            need(point["writer"] in PROOF_PHASES and point["phase"] in PROOF_PHASES[point["writer"]]
                 and point["command"] in ("info", "prove", "verify") and type(point["event_index"]) is int
                 and 2 <= point["event_index"] < job["event_count"] + 2, "invalid_proof_diagnostic_phase")
            diagnostic_integer(point["t_ns"], 2**63-1)
            writer, command, phase = (point[key] for key in ("writer", "command", "phase"))
            expected = {"proof_info": "info", "proof_verify": "verify"}.get(phase, "prove")
            need(writer == "core" and command == expected or writer == "shim" and command == "prove"
                 or writer == "host" and (command == "prove" or command == "verify" and phase == "host_receipt_verify"), "invalid_proof_diagnostic_phase_command")
            if name == "first_observed_failure":
                need(point["failure_class"] in DIAGNOSTIC_CLASSES and point["worker"] in (None, "generator", "prover")
                     and point["cleanup_requested"] is False, "invalid_proof_diagnostic_first_failure")
                for key, maximum in (("exit_code", 255), ("signal", 64), ("errno", 4095)):
                    diagnostic_integer(point[key], maximum, minimum=0 if key == "exit_code" else 1)
                need(point["exit_code"] is None or point["signal"] is None, "invalid_proof_diagnostic_failure_wait")
                if point["failure_class"] == "worker_exit": need(point["exit_code"] is not None and point["exit_code"] > 0, "proof_failure_exit_is_not_wait0")
                if point["failure_class"] == "worker_signal": need(point["signal"] is not None, "proof_failure_signal_is_not_exit137")
                if writer == "shim" and phase in ("generator_spawn", "prover_spawn", "worker_wait"):
                    need(point["worker"] in ("generator", "prover") and (phase == "worker_wait" or point["worker"] == phase.removesuffix("_spawn")), "invalid_proof_failure_worker")
                else: need(point["worker"] is None, "unexpected_proof_failure_worker")
                if phase == "worker_wait":
                    need(point["errno"] is None and (point["failure_class"] == "worker_exit" and point["exit_code"] is not None and point["exit_code"] > 0
                         or point["failure_class"] == "worker_signal" and point["signal"] is not None), "invalid_proof_failure_worker_status")
        command = job["command"]
        if command is not None:
            exact(command, PROOF_COMMAND_FIELDS, "invalid_proof_diagnostic_command_fields")
            need(command["command"] in ("info", "prove", "verify"), "invalid_proof_diagnostic_command")
            for key, maximum in (("exit_code", 255), ("signal", 64), ("spawn_errno", 4095), ("start_ns", 2**63-1), ("end_ns", 2**63-1)):
                diagnostic_integer(command[key], maximum, minimum=1 if key in ("signal", "spawn_errno") else 0)
            proof_io([command[key] for key in ("spawned", "stdin_write_complete", "stdin_closed", "stdout_eof", "stdout_bytes", "wait_observed", "timed_out")],
                     command["command"], command["exit_code"], command["signal"], command["failure_class"])
            need(command["failure_class"] is None or command["failure_class"] in DIAGNOSTIC_CLASSES, "invalid_proof_diagnostic_command_failure")
            need(command["spawn_errno"] is None or command["failure_class"] == "spawn" and not command["spawned"], "invalid_proof_spawn_errno")
            need(not command["timed_out"] or command["failure_class"] == "timeout", "invalid_proof_timeout_class")
            need(command["start_ns"] is None or command["end_ns"] is None or command["start_ns"] <= command["end_ns"], "proof_command_clock_reversed")
        resource = job["resources"]
        if resource is not None:
            exact(resource, {"cgroup", "same_cgroup_verified", "start", "last_open", "end"}, "invalid_proof_diagnostic_resources")
            proof_group(resource["cgroup"])
            need(type(resource["same_cgroup_verified"]) is bool and (not resource["same_cgroup_verified"] or resource["cgroup"] is not None), "invalid_proof_same_group")
            points = []
            for name in ("start", "last_open", "end"):
                point = resource[name]
                if point is None: continue
                exact(point, {"t_ns", "event_index", "values"}, "invalid_proof_counter_point")
                diagnostic_integer(point["t_ns"], 2**63-1); proof_values(point["values"], 5)
                need(resource["cgroup"] is not None and type(point["event_index"]) is int and 2 <= point["event_index"] < job["event_count"] + 2, "invalid_proof_counter_ref")
                points.append(point)
            for before, after in zip(sorted(points, key=lambda item: item["event_index"]), sorted(points, key=lambda item: item["event_index"])[1:]):
                need(all(a <= b for a, b in zip(before["values"], after["values"])), "proof_counters_reversed")
        activity = job["source_activity"]
        if activity is not None:
            need(job["relation"] == 2 and job["mode"] == "prove", "hold_or_reconcile_has_source_activity")
            exact(activity, {"start", "end"}, "invalid_source_activity_fields")
            for point in activity.values():
                if point is None: continue
                exact(point, {"t_ns", "cgroup", "values"}, "invalid_source_activity_point")
                diagnostic_integer(point["t_ns"], 2**63-1); proof_group(point["cgroup"]); proof_values(point["values"], len(SOURCE_ACTIVITY_FIELDS), nullable=True)
                need(not any(item is not None for item in point["values"]) or resource is not None
                     and resource["cgroup"] is not None and point["cgroup"] == resource["cgroup"], "source_activity_values_require_target_group")
            before, after = activity["start"], activity["end"]
            if before and after:
                need(before["t_ns"] is None or after["t_ns"] is None or before["t_ns"] <= after["t_ns"], "source_activity_clock_reversed")
                need(all(a is None or b is None or a <= b for a, b in zip(before["values"], after["values"])), "source_activity_counters_reversed")
        if job["availability"] == "complete":
            need(job["last_open_phase"] is None and resource is not None and resource["same_cgroup_verified"]
                 and resource["start"] is not None and resource["end"] is not None
                 and command is not None and not command["timed_out"], "incomplete_proof_diagnostic_claim")
            if job["relation"] == 2 and job["mode"] == "prove":
                need(activity is not None and all(activity.values()) and all(point["t_ns"] is not None
                     and point["cgroup"] == resource["cgroup"] and all(item is not None for item in point["values"])
                     for point in activity.values()), "incomplete_source_activity_claim")
    groups = {tuple(job["resources"]["cgroup"]) for job in jobs if job["resources"] is not None and job["resources"]["same_cgroup_verified"]}
    need(len(groups) <= 1, "proof_jobs_disagree_about_the_native_cgroup")
    need(value["availability"] == proof_availability(jobs), "invalid_proof_diagnostic_aggregate")
    need(len((json.dumps({"proof_diagnostics": value}, indent=2, sort_keys=True) + "\n").encode()) <= 16384, "proof_diagnostic_public_field_overflow")
    return copy.deepcopy(value)


def summarize_proof_job(data, node, index, peer, order, profile, artifacts, target, reserved_digest, genuine_receipts):
    need(isinstance(data, bytes) and 0 < len(data) <= 32768 and data.endswith(b"\n"), "invalid_order_diagnostic_bytes")
    lines = data.splitlines()
    need(2 <= len(lines) <= 64 and all(0 < len(line) + 1 <= 512 for line in lines), "invalid_order_diagnostic_record_bound")
    records = [diagnostic_json(line) for line in lines]
    header, tools = records[:2]
    exact(header, {"v", "kind", "node", "order", "relation", "window", "job", "profile"}, "invalid_order_diagnostic_header")
    relation = 2 if node == "S" else 3
    need(type(header["v"]) is int and header["v"] == 2 and header["kind"] == "job" and header["node"] == peer
         and header["order"] == order and type(header["relation"]) is int and header["relation"] == relation
         and type(header["window"]) is int and header["window"] == 0 and header["job"] == reserved_digest
         and header["profile"] == profile, "order_diagnostic_input_binding")
    for name in ("node", "order", "job", "profile"): proof_hex(header[name])
    exact(tools, {"v", "kind", "prover", "r0vm", "target_cgroup", "core_same_target"}, "invalid_order_diagnostic_tools")
    need(type(tools["v"]) is int and tools["v"] == 2 and tools["kind"] == "tools"
         and all(tools[name] == artifacts[name] for name in ("prover", "r0vm"))
         and type(tools["core_same_target"]) is bool, "order_diagnostic_tool_binding")
    proof_group(tools["target_cgroup"]); proof_group(target)
    need(not tools["core_same_target"] or tools["target_cgroup"] is not None, "order_diagnostic_unverified_group")
    group = tools["target_cgroup"]
    same = group is not None and group == target and tools["core_same_target"]
    begins, ends, ranks, failed, clocks, counters, milestones, commands = {}, {}, {}, set(), {}, {}, set(), {}
    events, activity = [], {}
    complete = True
    first = None
    def phase_ref(event, position):
        return {"writer": event["w"], "command": event["c"], "phase": event["p"], "t_ns": event["t"], "event_index": position}
    for position, record in enumerate(records[2:], 2):
        need(type(record) is dict, "invalid_order_diagnostic_record")
        if record.get("kind") == "source_activity":
            exact(record, {"v", "kind", "e", "t", "g", "q"}, "invalid_source_activity_record")
            need(type(record["v"]) is int and record["v"] == 2 and relation == 2 and record["e"] in ("begin", "end")
                 and record["e"] not in activity and (record["e"] != "end" or "begin" in activity)
                 and ("core", "prove", "proof_prove", None) in begins
                 and ("core", "prove", "proof_prove", None) not in ends, "invalid_source_activity_order")
            diagnostic_integer(record["t"], 2**63-1); proof_group(record["g"]); proof_values(record["q"], len(SOURCE_ACTIVITY_FIELDS), nullable=True)
            activity[record["e"]] = {"t_ns": record["t"], "cgroup": record["g"], "values": list(record["q"])}
            writer, stamp = "core", record["t"]
        else:
            exact(record, PROOF_EVENT, "invalid_order_diagnostic_event_fields")
            need(type(record["v"]) is int and record["v"] == 2 and record["kind"] == "event", "invalid_order_diagnostic_version")
            writer, command, phase, edge = (record[key] for key in ("w", "c", "p", "e"))
            need(writer in PROOF_PHASES and command in ("info", "prove", "verify") and edge in ("begin", "ok", "error", "observed"), "invalid_order_diagnostic_stage")
            observed = writer == "core" and phase in PROOF_IO_PHASES
            need((observed and edge == "observed") or (phase in PROOF_PHASES[writer] and edge != "observed"), "invalid_order_diagnostic_phase")
            expected = {"proof_info": "info", "proof_verify": "verify"}.get(phase, "prove")
            need((writer == "core" and (observed or command == expected)) or writer == "shim" and command == "prove"
                 or writer == "host" and (command == "prove" or command == "verify" and phase == "host_receipt_verify"), "invalid_order_diagnostic_command")
            diagnostic_integer(record["t"], 2**63-1); proof_group(record["g"])
            if record["q"] is not None: proof_values(record["q"], 5)
            for name, maximum in (("x", 255), ("s", 64), ("n", 4095)):
                diagnostic_integer(record[name], maximum, minimum=0 if name == "x" else 1)
            need(record["x"] is None or record["s"] is None, "order_diagnostic_exit_signal_conflict")
            need(type(record["k"]) is bool and record["r"] in (None, "generator", "prover")
                 and (not record["k"] or writer == "shim" and phase == "worker_wait")
                 and (record["f"] is None or record["f"] in DIAGNOSTIC_CLASSES), "invalid_order_diagnostic_failure")
            need(edge == "error" or record["f"] is None and record["n"] is None, "order_success_claims_failure")
            need(edge != "error" or record["f"] is not None, "order_error_missing_failure")
            role = record["r"]
            if writer == "shim" and phase in ("generator_spawn", "prover_spawn", "worker_wait"):
                need(role in ("generator", "prover") and (phase == "worker_wait" or role == phase.removesuffix("_spawn")), "invalid_order_worker")
            else: need(role is None, "unexpected_order_worker")
            key = (writer, command, phase, role)
            if observed:
                need(record["g"] is None and record["q"] is None and record["a"] is None
                     and (command, phase) not in milestones, "invalid_order_io_milestone")
                milestones.add((command, phase))
            else:
                if edge == "begin":
                    need(key not in begins and key not in ends and (writer, command) not in failed, "duplicate_or_failed_order_begin")
                    rank = PROOF_PHASES[writer].index(phase)
                    need(rank >= ranks.get((writer, command), -1), "reordered_order_diagnostic_begin")
                    ranks[writer, command] = rank; begins[key] = (record, position)
                else:
                    need(key not in ends and (key in begins or writer == "shim" and phase == "worker_wait"), "unbegun_or_duplicate_order_end")
                    if phase == "worker_wait":
                        spawned = ends.get(("shim", "prove", role + "_spawn", role))
                        need(spawned is not None and spawned[0]["e"] == "ok" and record["n"] is None
                             and ((edge == "ok" and record["x"] == 0 and record["s"] is None)
                                  or (edge == "error" and record["x"] is not None and record["x"] != 0 and record["s"] is None and record["f"] == "worker_exit")
                                  or (edge == "error" and record["x"] is None and record["s"] is not None and record["f"] == "worker_signal")), "invalid_order_worker_wait")
                    ends[key] = (record, position)
                    if edge == "error": failed.add((writer, command))
                if record["g"] != group or record["g"] is None: same = False
                if writer == "core" or writer == "host" and phase in ("host_succinct", "host_groth16") or writer == "shim" and phase == "shim_workers":
                    complete = complete and record["q"] is not None
                if record["q"] is not None:
                    need(record["g"] is not None, "order_counters_missing_group")
                    prior = counters.get((writer, tuple(record["g"])))
                    need(prior is None or all(a <= b for a, b in zip(prior, record["q"])), "order_counters_reversed")
                    counters[writer, tuple(record["g"])] = record["q"]
            if writer == "core" and phase != "proof_job":
                core_phase = {"info": "proof_info", "prove": "proof_prove", "verify": "proof_verify"}[command]
                need(observed is False or ("core", command, core_phase, None) in begins, "order_io_without_command")
                proof_io(record["i"], command, record["x"], record["s"], record["f"])
                previous = commands.get(command)
                if previous:
                    old_io = previous[0]["i"]
                    need(all(not old_io[i] or record["i"][i] for i in (0, 1, 2, 3, 5, 6))
                         and (old_io[4] is None or old_io[4] == record["i"][4]), "order_io_flags_reversed")
                commands[command] = (record, position)
                if observed:
                    flag = {"process_spawn": 0, "stdin_closed": 2, "stdout_complete": 4, "process_wait": 5}[phase]
                    need(record["i"][flag] is not None if flag == 4 else record["i"][flag], "order_milestone_missing_observation")
                if edge == "ok":
                    need(all(record["i"][i] for i in (0, 1, 2, 3, 5)) and not record["i"][6]
                         and record["x"] == 0 and record["s"] is None, "order_command_success_without_wait")
                if record["f"] == "spawn": need(not record["i"][0], "order_spawn_error_claims_process")
                if record["f"] == "timeout": need(record["i"][6], "order_timeout_missing_flag")
            else: need(record["i"] is None, "non_command_has_io")
            if writer == "core" and phase == "proof_job":
                need(record["a"] in ("not_started", "prove", "receipt_reconcile") and (edge != "begin" or record["a"] == "not_started"), "invalid_order_job_mode")
            else: need(record["a"] is None, "unexpected_order_job_mode")
            if edge == "error" and not record["k"] and first is None:
                first = {**phase_ref(record, position), "failure_class": record["f"], "worker": role,
                         "exit_code": record["x"], "signal": record["s"], "errno": record["n"], "cleanup_requested": False}
            events.append((record, position)); stamp = record["t"]
        if stamp is None: complete = False
        else:
            need(stamp >= clocks.get(writer, -1), "order_writer_clock_reversed")
            clocks[writer] = stamp
    outer = ("core", "prove", "proof_job", None)
    if events: need(events[0][0]["w"] == "core" and events[0][0]["p"] == "proof_job" and events[0][0]["e"] == "begin", "order_job_must_begin_first")
    mode = "prove" if ("core", "prove", "proof_prove", None) in begins else (
        "receipt_reconcile" if ("core", "verify", "proof_verify", None) in begins else "not_started")
    if outer in ends: need(ends[outer][0]["a"] == mode, "order_terminal_mode_mismatch")
    opened = [(event, position) for key, (event, position) in begins.items() if key not in ends]
    last_open = max(opened, key=lambda item: item[1]) if opened else None
    complete = complete and outer in ends and not opened
    required_writers = {"core"}
    if commands.get("prove", ({"i": [False]},))[0]["i"][0]: required_writers.add("host")
    if ("host", "prove", "host_groth16", None) in begins: required_writers.add("shim")
    if commands.get("verify", ({"i": [False]},))[0]["i"][0]: required_writers.add("host")
    same = same and required_writers <= {event["w"] for event, _ in events if event["g"] == group and event["g"] is not None}
    for command, (event, _) in commands.items():
        flags = event["i"]
        for flag, milestone in ((0, "process_spawn"), (2, "stdin_closed"), (4, "stdout_complete"), (5, "process_wait")):
            if (flags[flag] is not None if flag == 4 else flags[flag]):
                need((command, milestone) in milestones, "order_missing_io_milestone")
        if flags[6]: complete = False
    core = {command: ends.get(("core", command, phase, None)) for command, phase in (("info", "proof_info"), ("prove", "proof_prove"), ("verify", "proof_verify"))}
    succeeded = lambda terminal: terminal is not None and terminal[0]["e"] == "ok"
    outer_success = succeeded(ends.get(outer))
    complete = complete and core["info"] is not None
    if succeeded(core["info"]):
        if mode == "prove":
            complete = complete and core["prove"] is not None and (core["verify"] is not None if succeeded(core["prove"]) else "verify" not in commands)
        elif mode == "receipt_reconcile": complete = complete and core["verify"] is not None and "prove" not in commands
        else: complete = complete and not outer_success and "prove" not in commands and "verify" not in commands
    else: complete = complete and not outer_success and "prove" not in commands and "verify" not in commands
    if outer_success:
        complete = complete and mode in ("prove", "receipt_reconcile") and succeeded(core["verify"]) and (mode != "prove" or succeeded(core["prove"]))
    if any(event["w"] == "core" and event["s"] is not None for event, _ in events): complete = False
    def observed_path(writer, command, phases, success):
        for index, (phase, role) in enumerate(phases):
            key = (writer, command, phase, role)
            terminal = ends.get(key)
            if key not in begins or terminal is None: return False
            if terminal[0]["e"] == "error":
                return not success and not any((writer, command, later, worker) in begins for later, worker in phases[index + 1:])
        return True
    shim_path = [(phase, None) for phase in ("shim_validate", "shim_lock", "shim_fifo")] + [(role + "_spawn", role) for role in ("generator", "prover")] + [(phase, None) for phase in ("shim_workers", "shim_commit")]
    for command in ("prove", "verify"):
        terminal = core[command]
        if terminal is None: continue
        if terminal[0]["i"][0]:
            phases = PROOF_PHASES["host"] if command == "prove" else ("host_receipt_verify",)
            complete = complete and observed_path("host", command, [(phase, None) for phase in phases], succeeded(terminal))
            if command == "prove" and ("host", "prove", "host_groth16", None) in begins:
                complete = complete and observed_path("shim", "prove", shim_path, succeeded(terminal))
                for role in ("generator", "prover"):
                    if succeeded(ends.get(("shim", "prove", role + "_spawn", role))):
                        waited = ends.get(("shim", "prove", "worker_wait", role))
                        complete = complete and waited is not None and (not succeeded(terminal) or succeeded(waited))
        else:
            complete = complete and not any(event["w"] in ("host", "shim") and event["c"] == command for event, _ in events)
    def point(value):
        if value is None or value[0]["g"] != group or value[0]["q"] is None: return None
        return {"t_ns": value[0]["t"], "event_index": value[1], "values": value[0]["q"]}
    start, end = point(begins.get(outer)), point(ends.get(outer))
    source_activity = {"start": activity.get("begin"), "end": activity.get("end")} if activity else None
    if source_activity is not None:
        need(mode == "prove", "reconcile_has_source_activity")
        target_verified = group is not None and group == target and tools["core_same_target"]
        for activity_point in source_activity.values():
            if activity_point is not None and (not target_verified or activity_point["cgroup"] != group): activity_point["values"] = [None] * len(SOURCE_ACTIVITY_FIELDS)
        before, after = source_activity["start"], source_activity["end"]
        if before and after:
            for i, (a, b) in enumerate(zip(before["values"], after["values"])):
                if a is not None and b is not None and b < a: after["values"][i] = None
        if not before or not after or any(item["t_ns"] is None or item["cgroup"] != group or any(value is None for value in item["values"]) for item in source_activity.values() if item):
            complete = False
    if relation == 2 and mode == "prove" and source_activity is None: complete = False
    selected = max(commands.items(), key=lambda item: item[1][1]) if commands else None
    command_summary = None
    if selected:
        command, (event, _) = selected
        begin = begins["core", command, {"info": "proof_info", "prove": "proof_prove", "verify": "proof_verify"}[command], None][0]
        terminal = ends.get(("core", command, begin["p"], None))
        command_summary = dict(zip(("spawned", "stdin_write_complete", "stdin_closed", "stdout_eof", "stdout_bytes", "wait_observed", "timed_out"), event["i"]))
        command_summary.update(command=command, exit_code=event["x"], signal=event["s"], spawn_errno=event["n"] if event["f"] == "spawn" else None,
                               failure_class=event["f"], start_ns=begin["t"], end_ns=terminal[0]["t"] if terminal else None)
    binding = "verified_receipt" if any(fact.get("journalHash") == header["job"] and fact.get("relation") == relation
        and fact.get("orderId") == "0x" + order and type(fact.get("windowIndex")) is int and fact["windowIndex"] == 0 and fact.get("official_host_verified") is True
        and fact.get("wrong_journal_receipt_comparison_rejected") is True for fact in genuine_receipts) else "reserved_job"
    return {**unavailable_proof_job(node, index), "job_digest": header["job"], "binding": binding, "mode": mode,
            "availability": "complete" if complete and same and start is not None and end is not None and command_summary is not None else "partial",
            "event_count": len(records) - 2, "first_observed_failure": first,
            "last_open_phase": phase_ref(*last_open) if last_open else None, "command": command_summary,
            "resources": {"cgroup": group, "same_cgroup_verified": same, "start": start, "last_open": point(last_open), "end": end},
            "source_activity": source_activity}


def final_proof_diagnostics(summary, diagnostics):
    if diagnostics is None: return dict(summary)
    minimal = {**diagnostics, "availability": "unavailable", "jobs": [unavailable_proof_job(job["node"], job["order_index"]) for job in diagnostics["jobs"]]}
    for value in (diagnostics, minimal):
        result = {**summary, "proof_diagnostics": value}
        if len((json.dumps({"proof_diagnostics": value}, indent=2, sort_keys=True) + "\n").encode()) <= 16384 \
                and len((json.dumps(result, indent=2, sort_keys=True) + "\n").encode()) <= 131072:
            return result
    return dict(summary)


def wait_observer_ready():
    descriptor = os.environ.pop(OBSERVER_READY_FD, None)
    if descriptor is None:
        return
    need(descriptor.isdecimal() and int(descriptor) >= 3, "invalid_observer_ready_descriptor")
    descriptor = int(descriptor)
    try:
        os.set_inheritable(descriptor, False)
        ready, _, _ = select.select([descriptor], [], [], 120)
        need(ready and os.read(descriptor, 1) == b"R", "restart_observer_not_ready_before_business")
    finally:
        os.close(descriptor)


def exact(value, fields, code):
    need(isinstance(value, dict) and set(value) == set(fields), code)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_regular(path, limit, *, private=False):
    info = path.lstat()
    need(stat.S_ISREG(info.st_mode) and 0 < info.st_size <= limit, "bounded_regular_file_required")
    if private:
        need(info.st_uid == os.getuid() and not info.st_mode & 0o077, "owner_only_file_required")
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), "rb") as stream:
        data = stream.read(limit + 1)
    need(0 < len(data) <= limit, "bounded_file_read_required")
    return data


def file_digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for part in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(part)
    return value.hexdigest()


def durable(path, data):
    need(path.parent.resolve().is_relative_to(ARTIFACTS.resolve()), "write_outside_task_artifacts")
    temporary = path.with_name("." + path.name + "." + uuid.uuid4().hex)
    with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def save_json(path, value):
    durable(path, (json.dumps(value, indent=2, sort_keys=True) + "\n").encode())


def validate_config(value):
    exact(value, {"schema_version", "source_commit", "paths", "sha256", "elf_sha256", "caps", "publication_policy"}, "exact_config_schema_required")
    need(type(value["schema_version"]) is int and value["schema_version"] == 1, "config_schema_version")
    need(isinstance(value["source_commit"], str) and re.fullmatch(r"[0-9a-f]{40}", value["source_commit"])
         and value["source_commit"] != "0" * 40, "final_product_commit_required")
    exact(value["paths"], {"repository", "binary", "core_binary", "crypto_module", "prover", "r0vm"}, "exact_path_schema_required")
    for item in value["paths"].values():
        need(isinstance(item, str) and Path(item).is_absolute() and "trash" not in item.lower()
             and ".." not in Path(item).parts, "absolute_safe_paths_required")
    paths = {name: Path(item) for name, item in value["paths"].items()}
    need(paths["repository"].resolve() == REPOSITORY, "current_source_repository_required")
    need(paths["binary"].name == "vibeface" and paths["core_binary"] == paths["binary"].with_name("vibeface-core"), "same_directory_cli_core_required")
    need(paths["prover"].name == "vibeface-delivery-prover" and paths["crypto_module"].name == "vibeface_secure.js", "explicit_host_and_crypto_required")
    exact(value["sha256"], {"binary", "core_binary", "crypto_module", "crypto_wasm", "prover", "r0vm", "profile"}, "exact_hash_schema_required")
    for item in [*value["sha256"].values(), value["elf_sha256"]]:
        need(isinstance(item, str) and re.fullmatch(r"[0-9a-f]{64}", item) and item != "0" * 64, "explicit_final_artifact_hash_required")
    need(value["caps"] == {"deposit": "10", "max_service_amount": "5", "source_gas_cap_wei": CAP,
                          "provider_gas_cap_wei": CAP, "register_key_gas_cap_wei": CAP,
                          "native_funding_wei_each": str(10 ** 19)}, "explicit_test_caps_required")
    need(value["publication_policy"] == POLICY, "explicit_local_publication_policy_required")
    return value


def loopback(url):
    value = urllib.parse.urlsplit(url)
    need(value.scheme == "http" and value.hostname and ipaddress.ip_address(value.hostname).is_loopback
         and value.port and not value.username and not value.password and value.path in ("", "/")
         and not value.query and not value.fragment, "literal_loopback_rpc_required")
    return url


def extract_journal(receipt, expected_digest):
    need(0 < len(receipt) <= 16384 and re.fullmatch(r"[0-9a-f]{64}", expected_digest), "bounded_receipt_and_statement_digest_required")
    candidates = {receipt[index:index + 1504] for index in range(max(0, len(receipt) - 1504 + 1))
                  if digest(receipt[index:index + 1504]) == expected_digest}
    need(len(candidates) == 1, "unique_47_word_journal_required")
    return next(iter(candidates))


def words(journal):
    need(len(journal) == 1504, "exact_47_word_statement_required")
    return [journal[index:index + 32] for index in range(0, 1504, 32)]


def wrong_journal(journal):
    fields = words(journal)
    relation = int.from_bytes(fields[1], "big")
    if relation in (2, 3):
        fields[1] = (5 - relation).to_bytes(32, "big")
    else:
        need(relation == 1, "expected_key_source_or_hold_relation")
        changed = bytearray(fields[4])
        changed[0] ^= 1
        need(any(changed), "alternate_profile_remains_nonzero")
        fields[4] = bytes(changed)
    return b"".join(fields)


def profile_id(profile):
    encoded = hashlib.sha256(b"vibeface-delivery-profile-v1").digest() + bytes(profile["guest_image_id"]) + bytes(profile["statement_schema_hash"])
    return digest(encoded + b"".join(profile[name].to_bytes(32, "big") for name in PROFILE_FIELDS))


_live_helpers = runpy.run_path(str(REPOSITORY / "scripts/run-delivery-live.py"))
resources = _live_helpers["resources"]
oom_delta = _live_helpers["oom_delta"]


def process_fact(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {"pid": pid, "birth": fields[19], "group": int(fields[2]), "state": fields[0]}
    except (FileNotFoundError, ProcessLookupError):
        return None


class OwnedProcesses:
    def __init__(self):
        self.facts = {}

    def capture(self):
        pending, seen = [os.getpid()], set()
        while pending:
            pid = pending.pop()
            if pid in seen:
                continue
            seen.add(pid)
            fact = process_fact(pid)
            if fact is None:
                continue
            if pid != os.getpid():
                self.facts[pid] = fact
            for task in Path(f"/proc/{pid}/task").glob("*"):
                try:
                    pending.extend(map(int, (task / "children").read_text().split()))
                except (FileNotFoundError, ProcessLookupError):
                    pass

    def alive(self):
        return [fact for fact in self.facts.values() if (current := process_fact(fact["pid"]))
                and current["birth"] == fact["birth"] and current["state"] != "Z"]

    def stop_all(self):
        self.capture()
        for action in (signal.SIGTERM, signal.SIGKILL):
            for fact in reversed(self.alive()):
                try:
                    os.kill(fact["pid"], action)
                except ProcessLookupError:
                    pass
            deadline = time.monotonic() + (8 if action == signal.SIGTERM else 2)
            while self.alive() and time.monotonic() < deadline:
                time.sleep(0.1)
        return [fact["pid"] for fact in self.alive()]


class Experiment:
    def __init__(self, config, helpers, run_directory):
        self.config = validate_config(config)
        self.helpers = helpers
        self.paths = {name: Path(value) for name, value in config["paths"].items()}
        self.repository = self.paths["repository"]
        self.run = Path(run_directory).resolve()
        need(self.run.is_relative_to(ARTIFACTS.resolve()) and not self.run.exists(), "fresh_run_directory_inside_task_artifacts_required")
        self.run.mkdir(mode=0o700, parents=True)
        self.environment = {**os.environ, "DELIVERY_CONTRACT_ARTIFACTS": str(self.run / "contracts")}
        self.public = self.run / "public"
        self.public.mkdir(mode=0o700)
        self.processes = OwnedProcesses()
        self.nodes, self.passwords, self.proofs, self.orders, self.receipt_seen = [], {}, {}, {}, {}
        self.deployment = None
        self.budget = None
        self.initial_resources = None
        self.started = time.monotonic()
        self.registration_channel = None
        self.proof_channels = {}
        self.proof_diagnostic_result = None
        self.registration_cli = dict.fromkeys(("exit_code", "signal", "spawn_errno"))
        self.stage = "initializing"
        self.summary = {"schema_version": 1, "status": "RUNNING", "source_commit": config["source_commit"],
                        "run_id": self.run.name,
                        "scope": "fresh-two-order-protected-native-only", "parent_goal_status": "NOT_DETERMINED_BY_THIS_RUN",
                        "helper_source_sha256": file_digest(self.repository / "scripts/verify-node.py"),
                        "driver_source_sha256": file_digest(Path(__file__)), "artifact_sha256": config["sha256"], "node_proof_timeout_seconds": 7200,
                        "fees_each": {"delivery": "5", "reservation": "0", "bonus": "0"},
                        "deposit": "10", "publication_policy": POLICY, "not_checked": NOT_CHECKED,
                        "genuine_receipts": [], "public_files": {}, "orders": [], "checkpoints": {}}

    def public_json(self, name, value):
        path = self.public / name
        save_json(path, value)
        self.summary["public_files"][name] = file_digest(path)

    def progress(self, stage=None, **facts):
        if stage:
            self.stage = stage
        self.summary.update(facts)
        self.summary.update({"stage": self.stage, "wall_clock_unix_seconds": int(time.time()),
                             "elapsed_seconds": round(time.monotonic() - self.started, 3)})
        encoded = (json.dumps(self.summary, indent=2, sort_keys=True) + "\n").encode()
        need(len(encoded) <= 131072, "bounded_safe_progress_required")
        durable(self.public / "progress.json", encoded)
        print(json.dumps({"event": "protected_native_progress", "status": self.summary["status"], "stage": self.stage,
                          "run_id": self.run.name, "verified_genuine_receipts": len(self.proofs),
                          "source_commit": self.config["source_commit"]}, sort_keys=True), flush=True)

    def command(self, arguments, *, data=None, timeout=30, environment=None, success=True):
        result = subprocess.run(list(map(str, arguments)), input=data, capture_output=True, timeout=timeout,
                                env=self.environment if environment is None else environment, cwd=self.repository)
        self.processes.capture()
        need(len(result.stdout) <= 1024 * 1024 and len(result.stderr) <= 1024 * 1024, "bounded_child_output_required")
        if (result.returncode == 0) != success:
            failure = self.run / ("command-failure-" + uuid.uuid4().hex + ".log")
            durable(failure, result.stdout + b"\n" + result.stderr)
            raise CheckFailure("child_command_failed_private_log_retained")
        return result

    def wait(self, label, predicate, *, timeout=120):
        self.progress(label)
        until, checkpoint = time.monotonic() + timeout, 0
        while time.monotonic() < until:
            self.processes.capture()
            self.scan_receipts()
            value = predicate()
            if value:
                return value
            if time.monotonic() >= checkpoint:
                self.checkpoint(label)
                checkpoint = time.monotonic() + 30
            time.sleep(1)
        raise CheckFailure("stage_observer_timeout_" + label)

    def checkpoint(self, name):
        _, snapshot = resources()
        deltas = oom_delta(self.initial_resources, snapshot)
        need(not any(deltas.values()), "actual_cgroup_oom_during_experiment")
        anchor = self.rpc("eth_getBlockByNumber", ["latest", False]) if self.deployment else None
        value = {"wall_clock_unix_seconds": int(time.time()), "resources": snapshot, "oom_delta": deltas,
                 "original_deadlines": {key: item["deadlines"] for key, item in self.orders.items()}}
        if anchor:
            value["chain_anchor"] = {"number": int(anchor["number"], 16), "hash": anchor["hash"], "timestamp": int(anchor["timestamp"], 16)}
            value["deadline_remaining_seconds"] = {key: {field: deadline - int(anchor["timestamp"], 16)
                 for field, deadline in item["deadlines"].items()} for key, item in self.orders.items()}
        self.summary["checkpoints"][name] = value
        self.progress()

    def rpc(self, method, params):
        request = urllib.request.Request(loopback(self.deployment["rpc_url"]), json.dumps({"jsonrpc": "2.0", "id": 1,
            "method": method, "params": params}).encode(), {"Content-Type": "application/json"})
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(request, timeout=30) as response:
            encoded = response.read(1024 * 1024 + 1)
        need(len(encoded) <= 1024 * 1024, "bounded_loopback_rpc_response")
        result = json.loads(encoded)
        need(result.get("id") == 1 and "error" not in result, "successful_loopback_rpc_required")
        return result["result"]

    def confirmed(self, tx_hash):
        value = self.rpc("eth_getTransactionReceipt", [tx_hash])
        if value is None:
            return None
        need(value["status"] == "0x1", "task_funding_transaction_reverted")
        return value if int(self.rpc("eth_blockNumber", []), 16) >= int(value["blockNumber"], 16) + 1 else None

    def cli(self, node, arguments, *, password=None, timeout=30, success=True):
        command = [str(self.paths["binary"]), "wallet", "--data-dir", str(node.directory), *arguments]
        descriptor = None
        password_path = self.run / ("passphrase-" + uuid.uuid4().hex)
        try:
            if password is not None:
                descriptor = os.open(password_path, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                os.write(descriptor, password.encode() + b"\n")
                os.lseek(descriptor, 0, os.SEEK_SET)
                command += ["--passphrase-fd", str(descriptor)]
            result = subprocess.run(command, capture_output=True, timeout=timeout,
                                    pass_fds=(() if descriptor is None else (descriptor,)))
        finally:
            if descriptor is not None:
                os.close(descriptor)
                password_path.unlink(missing_ok=True)
        if (result.returncode == 0) != success or len(result.stdout) > 65536 or len(result.stderr) > 65536:
            durable(self.run / ("wallet-failure-" + uuid.uuid4().hex + ".log"), result.stdout[:65536] + b"\n" + result.stderr[:65536])
            raise CheckFailure("local_owner_wallet_command_failed_private_log_retained")
        return json.loads(result.stdout) if success and result.stdout.lstrip().startswith(b"{") else None

    def call(self, node, op, **params):
        packet = {"op": op, **params}
        if op not in ("prepare", "confirm_batch", "accept_batch"):
            return node.client.call("delivery", packet)
        def attempt():
            response = node.client.command("request", method="delivery", params=packet, connection="main")
            error = response.get("error")
            if error == "another delivery operation is in progress":
                return None
            need(not error, "delivery_" + op + "_rejected")
            return {"result": response.get("result")}
        return self.wait("delivery-" + op + "-after-explicit-pre-mutation-busy", attempt)["result"]

    def rows(self, node, query, parameters=()):
        path = node.directory / "delivery/service.sqlite3"
        if not path.exists():
            return []
        with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as database:
            database.execute("PRAGMA query_only=ON")
            return database.execute(query, parameters).fetchall()

    def scan_receipts(self):
        for node in self.nodes:
            for statement_digest, filename in self.rows(node, "SELECT digest,path FROM proof_jobs ORDER BY digest"):
                key = (node.name, statement_digest)
                path = Path(filename)
                need(path.is_absolute() and path.resolve().is_relative_to(node.directory.resolve()), "receipt_must_belong_to_this_fresh_node")
                self.receipt_seen.setdefault(key, time.monotonic())
                if key in self.proofs or not path.exists():
                    continue
                receipt = read_regular(path, 16384, private=True)
                journal = extract_journal(receipt, statement_digest)
                fields = words(journal)
                relation = int.from_bytes(fields[1], "big")
                need(relation in (1, 2, 3), "unexpected_open_or_other_proof")
                need(fields[4].hex() == self.profile_id and fields[5] == bytes(self.profile["guest_image_id"])
                     and int.from_bytes(fields[2], "big") == 31337
                     and "0x" + fields[3][-20:].hex() == self.deployment["escrow"].lower(), "exact_public_statement_identity_required")
                order_id = fields[6].hex()
                if relation != 1:
                    need(order_id in self.orders and int.from_bytes(fields[25], "big") == 1
                         and int.from_bytes(fields[26], "big") == 0 and int.from_bytes(fields[16], "big") == 0,
                         "new_single_window_recoverable_order_required")
                    need([int.from_bytes(fields[index], "big") for index in (33, 34, 35, 36)] == [5, 0, 0, 5], "genuine_exact_fee5_terms_required")
                env = {"PATH": "/usr/bin:/bin", "RISC0_SERVER_PATH": str(self.paths["r0vm"])}
                verified = self.command([self.paths["prover"], "verify", journal.hex()], data=receipt, timeout=120, environment=env)
                result = json.loads(verified.stdout)
                need(result.get("verified") is True and result.get("kind") == "groth16"
                     and result.get("imageId") == bytes(self.profile["guest_image_id"]).hex()
                     and result.get("journalHash") == statement_digest
                     and isinstance(result.get("seal"), str) and re.fullmatch(r"[0-9a-f]+", result["seal"]), "official_host_groth16_identity_required")
                negative = self.command([self.paths["prover"], "verify", wrong_journal(journal).hex()],
                    data=receipt, timeout=120, environment=env, success=False)
                need(b"receipt journal mismatch" in negative.stderr, "wrong_journal_must_fail_at_official_receipt_comparison")
                basename = "key-binding" if relation == 1 else f"order-{order_id}-relation-{relation}-window-0"
                need(not (self.public / (basename + ".receipt")).exists(), "fresh_unique_proof_export_required")
                durable(self.public / (basename + ".receipt"), receipt)
                metadata = {"mode": "prove", "kind": "groth16", "relation": relation,
                            "imageId": result["imageId"], "profileId": self.profile_id, "journal": journal.hex(),
                            "journalHash": statement_digest, "seal": result["seal"]}
                save_json(self.public / (basename + ".json"), metadata)
                entry = {"relation": relation, "public": basename + ".json", "receipt": basename + ".receipt"}
                if relation != 1:
                    entry.update({"orderId": "0x" + order_id, "windowIndex": 0})
                fact = {**entry, "node": node.name, "journalHash": statement_digest, "receiptSha256": digest(receipt),
                        "publicSha256": file_digest(self.public / (basename + ".json")),
                        "official_host_verified": True, "wrong_journal_receipt_comparison_rejected": True,
                        "first_reservation_observed_to_export_seconds": round(time.monotonic() - self.receipt_seen[key], 3)}
                self.proofs[key] = {"entry": entry, "journal": journal, "fact": fact}
                self.summary["genuine_receipts"].append(fact)
                self.progress()

    def proof_count(self, relation):
        return sum(item["entry"]["relation"] == relation for item in self.proofs.values())

    def public_chain(self, *, orders=False, capsules=False):
        tx_hashes = []
        for node in self.nodes:
            for _, tx_hash, phase in self.rows(node, "SELECT id,hash,phase FROM transactions ORDER BY id"):
                need(phase not in ("reverted", "failed", "conflicted"), "actual_product_transaction_terminal_failure")
                if phase == "confirmed":
                    need(re.fullmatch(r"0x[0-9a-f]{64}", tx_hash), "actual_confirmed_transaction_hash_required")
                    tx_hashes.append(tx_hash)
        deployment = self.deployment
        packet = {"repository": str(self.repository), "rpc_url": deployment["rpc_url"], "escrow": deployment["escrow"],
                  "asset": deployment["asset"], "transactions": sorted(set(tx_hashes)),
                  "orders": ["0x" + value for value in self.orders] if orders else [],
                  "accounts": [node.wallet_address for node in self.nodes] + [deployment["escrow"]],
                  "budget_id": "0x" + self.budget if self.budget else None, "include_capsules": capsules,
                  "contracts": {name: deployment[name] for name in ("escrow", "asset", "adapter", "verifier")}}
        output = self.command(["node", self.repository / "contracts/delivery/inspect-protected.mjs"], data=json.dumps(packet).encode(), timeout=120)
        return json.loads(output.stdout)

    def get(self, node, order_id):
        value = self.call(node, "get", order_id=order_id)
        if value.get("error"):
            save_json(self.run / ("order-failure-" + order_id + ".json"), value)
            message = str(value["error"]).lower()
            self.summary["failure_order_id"] = order_id
            code = "actual_paid_order_executor_failed_private_log_retained"
            if re.search(r"proof|prover", message) and re.search(r"timeout|timed out|exceeded its execution deadline", message):
                code = "actual_product_proof_execution_deadline_exceeded"
            raise CheckFailure(code)
        need(value["deadlines"] == self.orders[order_id]["deadlines"], "original_order_deadlines_must_not_change")
        return value

    def start(self):
        wait_observer_ready()
        need(sys.platform.startswith("linux") and platform.machine() in ("x86_64", "amd64"), "linux_amd64_required")
        need(not any(name.startswith(("RISC0_DEV", "RISC0_SKIP")) for name in os.environ), "no_dev_or_skip_verifier_environment")
        git_env = {**os.environ, "GIT_OPTIONAL_LOCKS": "0"}
        source = self.command(["git", "rev-parse", "HEAD"], environment=git_env).stdout.decode().strip()
        dirty = self.command(["git", "status", "--porcelain", "--untracked-files=normal"], environment=git_env).stdout
        need(source == self.config["source_commit"] and not dirty, "final_clean_product_head_required")
        profile_path = self.repository / "crates/delivery/proof/profile.json"
        targets = {name: path for name, path in self.paths.items() if name != "repository"}
        targets.update({"crypto_wasm": self.paths["crypto_module"].with_name("vibeface_secure_bg.wasm"), "profile": profile_path})
        for name, path in targets.items():
            need(path.is_file() and not path.is_symlink() and file_digest(path) == self.config["sha256"][name], "final_artifact_hash_mismatch_" + name)
        self.profile = json.loads(read_regular(profile_path, 16384))
        self.profile_id = profile_id(self.profile)
        need(len(bytes(self.profile["guest_image_id"])) == 32 and any(self.profile["guest_image_id"])
             and [self.profile[name] for name in PROFILE_FIELDS] == [1, 2, 65536, 131072, 64000000, 17179869184],
             "exact_official_pinned_profile_required")
        need(digest((self.repository / "crates/delivery/abi/Statement.sol").read_bytes()) == bytes(self.profile["statement_schema_hash"]).hex(), "tracked_47_word_schema_identity_required")
        info_result = self.command([self.paths["prover"], "info"], timeout=30,
                                  environment={"PATH": "/usr/bin:/bin", "RISC0_SERVER_PATH": str(self.paths["r0vm"])})
        info = json.loads(info_result.stdout)
        need(info == {"imageId": bytes(self.profile["guest_image_id"]).hex(), "schemaHash": bytes(self.profile["statement_schema_hash"]).hex(),
                      "elfSha256": self.config["elf_sha256"], "statementBytes": 1504}, "official_guest_host_profile_identity_required")
        self.command(["python3", "scripts/build-inputs.py", "check", "crypto"], timeout=120)
        cgroup, self.initial_resources = resources()
        self.summary.update({"profile_id": self.profile_id, "prover_info": info, "initial_resources": self.initial_resources})
        self.prover_settings = {"program": str(self.paths["prover"]), "r0vm": str(self.paths["r0vm"]), "cgroup": str(cgroup), "profile": self.profile}
        self.progress("fresh-current-clock-deployment")
        deployment_file = self.run / "deployment.json"
        log = (self.run / "deployment.log").open("xb")
        self.deployment_process = subprocess.Popen(["node", str(self.repository / "contracts/delivery/deploy-local.mjs"), "--listen", "127.0.0.1:0", "--output", str(deployment_file)],
                                                  stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True, env=self.environment)
        log.close()
        self.processes.capture()
        def ready():
            need(self.deployment_process.poll() is None, "fresh_deployment_exited_private_log_retained")
            try:
                value = json.loads(deployment_file.read_bytes())
                return value if value.get("rpc_url") else None
            except (FileNotFoundError, json.JSONDecodeError):
                return None
        self.deployment = self.wait("fresh-current-clock-deployment", ready)
        need(self.deployment["chain_id"] == 31337 and self.deployment["confirmations"] == 2
             and self.deployment["profile"] == self.profile and "experiment_fixture" not in self.deployment,
             "fresh_current_clock_official_deployment_required")
        loopback(self.deployment["rpc_url"])
        latest = self.rpc("eth_getBlockByNumber", ["latest", False])
        need(abs(int(latest["timestamp"], 16) - time.time()) < 60, "real_current_chain_clock_required")
        self.setup_nodes()

    def setup_nodes(self):
        self.progress("default-off-owner-configuration-and-fresh-wallets")
        workspace = self.run / "workspace"
        workspace.mkdir(mode=0o700)
        client = self.run / "client.mjs"
        durable(client, self.helpers["CLIENT_SOURCE"].replace("MODULE_URL", self.paths["crypto_module"].as_uri()).encode())
        mesh = self.helpers["mesh_network"]()
        accounts = self.rpc("eth_accounts", [])
        need(len(accounts) == 4 and accounts[0].lower() == self.deployment["funder"].lower(), "fresh_testchain_funder_only")
        need(int(self.rpc("eth_chainId", []), 16) == 31337, "testchain_identity_required")
        self.funder = accounts[0]
        default_checks = []
        for name in ("S", "R", "P"):
            args = self.helpers["public_node_args"]() if name == "P" else []
            node = self.helpers["Node"](self.run, name, self.paths["binary"], workspace, client, init_args=args)
            self.nodes.append(node)
            self.helpers["isolate_delivery_node"](node)
            need(not (node.directory / "delivery-settings.json").exists(), "fresh_delivery_settings_must_be_absent")
            if name in ("S", "R"):
                self.prepare_proof_diagnostics(node)
        source, receiver, provider = self.nodes
        provider.configure_mesh(mesh)
        for node in (source, receiver):
            node.configure_mesh(mesh, [provider.mesh_address])
        for node in self.nodes:
            need(json.loads((node.directory / "config.json").read_bytes())["network"]["mesh"] == {
                "enabled": True, "network": mesh, "bootstrap": [] if node is provider else [provider.mesh_address],
                "allow_private": True}, "private_mesh_uses_only_the_independent_provider_bootstrap")
        provider.start()
        for node in self.nodes:
            name = node.name
            if node is not provider:
                node.start()
                node.bootstrap()
                status = self.call(node, "status")
                need(status["enabled"] is False and status["paid_enabled"] is False and status["wallet"]["state"] == "absent", "default_disabled_api_and_absent_wallet_required")
            need(self.cli(node, ["status"])["state"] == "absent" and not (node.directory / "delivery-wallet.json").exists()
                 and not self.rows(node, "SELECT id FROM transactions"), "default_off_creates_no_wallet_or_payment")
            default_checks.append({"node": name, "lab_enabled": False, "paid_enabled": False, "wallet": "absent", "transactions": 0})
            settings = {"enabled": True, "charity_enabled": False, "paid_enabled": True,
                        "chain": {field: self.deployment[field] for field in ("rpc_url", "chain_id", "escrow", "asset", "profile", "confirmations")},
                        "prover": self.prover_settings, "provider_gas_cap": CAP, "publication_policy": POLICY,
                        "delivery_fee_per_window": "5", "reservation_fee_per_window": "0", "priority_fee": "0"}
            path = node.directory / "acceptance-delivery-settings.json"
            save_json(path, settings)
            self.cli(node, ["configure", str(path)])
            password = base64.urlsafe_b64encode(os.urandom(32)).decode()
            self.passwords[name] = password
            need(self.cli(node, ["create"], password=password)["state"] == "locked", "new_wallet_starts_locked")
            self.cli(node, ["unlock"], password="wrong-task-passphrase", success=False)
            need(self.cli(node, ["status"])["state"] == "locked", "wrong_passphrase_cannot_unlock")
            unlocked = self.cli(node, ["unlock"], password=password)
            need(unlocked["state"] == "unlocked", "owner_fd_wallet_unlock_required")
            node.wallet_address = unlocked["address"].lower()
            funding = self.rpc("eth_sendTransaction", [{"from": self.funder, "to": node.wallet_address, "value": hex(10 ** 19)}])
            self.wait("fresh-test-native-funding-" + name, lambda: self.confirmed(funding), timeout=60)
        source, receiver, provider = self.nodes
        need(source.client.network_id != receiver.client.network_id and len({node.device_id for node in self.nodes}) == 3
             and len({node.wallet_address for node in self.nodes}) == 3, "independent_s_r_p_network_identity_wallets_required")
        transfer = "0xa9059cbb" + source.wallet_address[2:].zfill(64) + format(10, "064x")
        token_tx = self.rpc("eth_sendTransaction", [{"from": self.funder, "to": self.deployment["asset"], "data": transfer}])
        self.wait("fresh-testtoken-funding", lambda: self.confirmed(token_tx), timeout=60)
        initial = self.public_chain()
        need(initial["balances"][source.wallet_address] == "10" and initial["balances"][receiver.wallet_address] == "0"
             and initial["balances"][provider.wallet_address] == "0" and initial["balances"][self.deployment["escrow"].lower()] == "0",
             "fresh_known_testtoken_balances_required")
        self.summary["token_balances_before"] = initial["balances"]
        self.summary.update({"default_off_checks": default_checks, "principals": {node.name: {"peer": node.device_id, "wallet": node.wallet_address,
                             "network_id": node.client.network_id if node.name != "P" else None} for node in self.nodes},
                             "deployment": {field: self.deployment[field] for field in ("rpc_url", "chain_id", "escrow", "asset", "adapter", "verifier", "runtime_code_hashes", "baseline_block")}})
        save_json(self.run / "private-runtime.json", {"passwords": self.passwords, "nodes": {node.name: str(node.directory) for node in self.nodes}})

    def prepare_proof_diagnostics(self, node):
        descriptor = None
        self.proof_channels = getattr(self, "proof_channels", {})
        if node.name in self.proof_channels: return
        try:
            need(node.name in ("S", "R") and node.name not in self.proof_channels, "fresh_proof_diagnostic_node_required")
            flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK
            descriptor = os.open(node.directory, flags)
            chain = []
            path = node.directory
            for component in (None, "delivery", "private", "proof-diagnostics"):
                if component is not None:
                    try: os.mkdir(component, 0o700, dir_fd=descriptor)
                    except FileExistsError:
                        need(component != "proof-diagnostics", "fresh_proof_diagnostic_directory_required")
                    child = os.open(component, flags, dir_fd=descriptor)
                    os.close(descriptor); descriptor = child; path = path / component
                info = os.fstat(descriptor)
                need(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid() and stat.S_IMODE(info.st_mode) == 0o700,
                     "private_proof_diagnostic_directory_required")
                chain.append((path, (info.st_dev, info.st_ino)))
            self.proof_channels[node.name] = {"fd": descriptor, "path": path, "chain": chain}
        except (OSError, CheckFailure, ValueError):
            if descriptor is not None: os.close(descriptor)
            self.proof_channels[node.name] = None

    def collect_proof_diagnostics(self):
        if self.proof_diagnostic_result is not None:
            return copy.deepcopy(self.proof_diagnostic_result)
        orders = list(self.orders.values())[:2]
        jobs = {(name, index): unavailable_proof_job(name, index) for name in ("S", "R") for index in range(len(orders))}
        target = None
        try:
            actual, _ = resources()
            configured = Path(self.prover_settings["cgroup"])
            current, expected = actual.stat(), configured.stat()
            if (current.st_dev, current.st_ino) == (expected.st_dev, expected.st_ino): target = [current.st_dev, current.st_ino]
        except (OSError, ValueError, KeyError, AttributeError, StopIteration): pass
        for node in self.nodes:
            if node.name not in ("S", "R"): continue
            channel = self.proof_channels.get(node.name)
            if channel is None: continue
            touched = set()
            try:
                def directory_current():
                    for path, identity in channel["chain"]:
                        info = path.lstat()
                        need(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid() and stat.S_IMODE(info.st_mode) == 0o700
                             and (info.st_dev, info.st_ino) == identity, "changed_proof_diagnostic_directory")
                    pinned = os.fstat(channel["fd"])
                    need((pinned.st_dev, pinned.st_ino) == channel["chain"][-1][1], "changed_proof_diagnostic_directory_fd")
                directory_current()
                with os.scandir(channel["fd"]) as entries:
                    names = [entry.name for _, entry in zip(range(3), entries)]
                need(len(names) <= 2 and all(re.fullmatch(r"[0-9a-f]{64}\.jsonl", name) for name in names), "unexpected_proof_diagnostic_files")
                reserved = dict(self.rows(node, "SELECT digest,path FROM proof_jobs ORDER BY digest"))
                for name in names:
                    descriptor = None
                    index = None
                    try:
                        descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK, dir_fd=channel["fd"])
                        before = os.fstat(descriptor)
                        need(stat.S_ISREG(before.st_mode) and before.st_uid == os.getuid() and stat.S_IMODE(before.st_mode) == 0o600
                             and before.st_nlink == 1 and 0 < before.st_size <= 32768, "invalid_proof_diagnostic_file")
                        data = os.read(descriptor, 32769)
                        after = os.fstat(descriptor)
                        named = os.stat(name, dir_fd=channel["fd"], follow_symlinks=False)
                        metadata = lambda info: (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns, info.st_mode, info.st_uid, info.st_nlink)
                        need(metadata(before) == metadata(after) == metadata(named) and len(data) == before.st_size, "changed_proof_diagnostic_file")
                        header = diagnostic_json(data.splitlines()[0])
                        index = next((index for index, order in enumerate(orders) if order["order_id"] == header.get("order")), None)
                        need(index is not None and index not in touched, "unknown_or_duplicate_proof_diagnostic_order")
                        touched.add(index)
                        job_digest = name[:-6]
                        need(reserved.get(job_digest) == str(node.directory / "delivery/private" / ("proof-" + job_digest + ".receipt")), "proof_diagnostic_reservation_binding")
                        jobs[node.name, index] = summarize_proof_job(data, node.name, index, node.device_id, orders[index]["order_id"],
                            getattr(self, "profile_id", None), self.config["sha256"], target, job_digest, self.summary["genuine_receipts"])
                    except Exception:
                        if index is None: raise CheckFailure("unbound_proof_diagnostic_file")
                        jobs[node.name, index] = unavailable_proof_job(node.name, index)
                    finally:
                        if descriptor is not None: os.close(descriptor)
                directory_current()
            except Exception:
                for index in range(len(orders)): jobs[node.name, index] = unavailable_proof_job(node.name, index)
            finally:
                os.close(channel["fd"]); self.proof_channels[node.name] = None
        result = {"schema_version": 1, "source_commit": self.config["source_commit"],
                  "artifact_sha256": {name: self.config["sha256"][name] for name in ("prover", "r0vm", "profile")},
                  "profile_id": getattr(self, "profile_id", None), "counter_fields": list(DIAGNOSTIC_COUNTERS),
                  "attempt_policy": "first_admitted", "availability": proof_availability(list(jobs.values())), "jobs": list(jobs.values())}
        principals = {node.name: {"peer": node.device_id} for node in self.nodes if node.name in ("S", "R")}
        try:
            self.proof_diagnostic_result = project_proof_diagnostics(result, self.config["source_commit"], self.config["sha256"],
                getattr(self, "profile_id", None), principals, orders, self.summary["genuine_receipts"])
        except (CheckFailure, ValueError, TypeError, KeyError):
            self.proof_diagnostic_result = {**result, "availability": "unavailable", "jobs": [unavailable_proof_job(job["node"], job["order_index"]) for job in result["jobs"]]}
        return copy.deepcopy(self.proof_diagnostic_result)

    def prepare_registration_diagnostics(self, receiver, intent):
        descriptor = None
        try:
            need(str(uuid.UUID(intent)) == intent, "canonical_diagnostic_intent_required")
            root = receiver.directory
            for path in (root, root / "delivery", root / "delivery/private"):
                if not path.exists(): path.mkdir(mode=0o700)
                info = path.lstat()
                need(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid() and not info.st_mode & 0o077,
                     "owner_only_diagnostic_directory_required")
            path = root / "delivery/private" / ("key-diagnostics-" + intent + ".jsonl")
            descriptor = os.open(path, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
            info = os.fstat(descriptor)
            self.registration_channel = {"fd": descriptor, "path": path, "identity": (info.st_dev, info.st_ino)}
        except (OSError, CheckFailure, ValueError):
            if descriptor is not None: os.close(descriptor)
            self.registration_channel = None

    def retire_registration_diagnostics(self):
        channel = self.registration_channel
        if channel is not None:
            try:
                info = channel["path"].lstat()
                need((info.st_dev, info.st_ino) == channel["identity"], "diagnostic_channel_inode_changed")
                channel["path"].rename(self.run / "registration-diagnostics-private.jsonl")
            except (OSError, CheckFailure):
                pass

    def publish_registration_diagnostics(self):
        data = b""
        channel = self.registration_channel
        if channel is not None:
            descriptor = channel["fd"]
            try:
                info = os.fstat(descriptor)
                need((info.st_dev, info.st_ino) == channel["identity"] and stat.S_ISREG(info.st_mode)
                     and info.st_uid == os.getuid() and stat.S_IMODE(info.st_mode) == 0o600
                     and 0 < info.st_size <= 32768, "invalid_bound_diagnostic_channel")
                os.lseek(descriptor, 0, os.SEEK_SET)
                candidate = os.read(descriptor, 32769)
                after = os.fstat(descriptor)
                need((info.st_size, info.st_mtime_ns) == (after.st_size, after.st_mtime_ns), "diagnostic_channel_still_changing")
                data = candidate
            except (OSError, CheckFailure):
                pass
            finally:
                try: os.close(descriptor)
                except OSError: pass
                self.registration_channel = None
        value = registration_diagnostics(data, self.config["source_commit"], self.config["sha256"], self.registration_cli)
        try:
            self.public_json("registration-diagnostics.json", value)
        except (OSError, CheckFailure):
            pass

    def register(self):
        receiver = self.nodes[1]
        intent = str(uuid.uuid4())
        arguments = ["register-key", "--intent-id", intent, "--gas-cap", CAP]
        self.prepare_registration_diagnostics(receiver, intent)
        output = self.run / "register-key-output.json"
        errors = self.run / "register-key-private.log"
        with output.open("xb") as stdout, errors.open("xb") as stderr:
            try:
                process = subprocess.Popen([str(self.paths["binary"]), "wallet", "--data-dir", str(receiver.directory), *arguments],
                    stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, start_new_session=True)
            except OSError as error:
                self.registration_cli["spawn_errno"] = error.errno
                raise
        def done():
            if process.poll() is None:
                return None
            self.registration_cli.update(exit_code=process.returncode if process.returncode >= 0 else None,
                                         signal=-process.returncode if process.returncode < 0 else None)
            self.retire_registration_diagnostics()
            need(process.returncode == 0, "genuine_register_key_failed_private_log_retained")
            return True
        self.wait("genuine-register-key-before-order-deadlines", done, timeout=7400)
        def confirmed():
            value = self.cli(receiver, arguments)
            need(value.get("phase") not in ("failed", "reverted", "conflicted", "superseded"), "genuine_key_registry_terminal_failure")
            return value if value.get("phase") == "confirmed" and value.get("registry_confirmed") is True else None
        result = self.wait("register-key-confirmed-before-any-prepare", confirmed)
        self.scan_receipts()
        need(self.proof_count(1) == 1 and len(self.proofs) == 1, "exactly_one_new_key_binding_receipt_required")
        self.summary["registration"] = {"intent_id": intent, "serial": result["serial"], "binding_hash": result["binding_hash"], "tx_hash": result["tx_hash"],
                                        "confirmed_before_any_order_prepare": True}

    def open_and_prepare(self):
        source, receiver, provider = self.nodes
        intent = str(uuid.uuid4())
        arguments = ["budget", "open", "--intent-id", intent, "--payee", provider.wallet_address, "--provider-peer", provider.device_id,
                     "--deposit", "10", "--gas-cap", CAP]
        def opened():
            value = self.cli(source, arguments)
            need(re.fullmatch(r"[0-9a-f]{64}", value["budget_id"]), "actual_budget_id_required")
            need(self.budget is None or value["budget_id"] == self.budget, "budget_open_same_uuid_identity")
            self.budget = value["budget_id"]
            need(value["transaction"]["phase"] not in ("reverted", "conflicted", "failed"), "budget_open_terminal_failure")
            return value if value["transaction"]["phase"] == "confirmed" else None
        self.wait("confirmed-prepaid-budget-before-orders", opened)
        original = os.urandom(4096)
        durable(self.run / "synthetic-original.bin", original)
        self.original = original
        for label in ("a", "b"):
            name = "native-genuine-" + label + ".bin"
            intent = str(uuid.uuid4())
            prepared = self.call(source, "prepare", intent_id=intent, receiver_peer=receiver.device_id, provider_peer=provider.device_id,
                mode="paid", acceptance="recoverable", name=name, content_base64=base64.b64encode(original).decode(),
                max_service_amount="5", max_gas_amount=CAP, budget_id=self.budget)
            order_id = prepared["order_id"]
            need(re.fullmatch(r"[0-9a-f]{64}", order_id) and order_id not in self.orders and prepared["intent_id"] == intent,
                 "two_distinct_fresh_paid_order_ids_required")
            need(prepared["mode"] == "paid" and prepared["acceptance"] == "recoverable" and len(prepared["windows"]) == 1
                 and prepared["payment"]["budget_id"] == self.budget and prepared["payment"]["total"] == "5"
                 and prepared["payment"]["delivery_fee"] == "5" and prepared["payment"]["reservation_fee"] == "0"
                 and prepared["payment"]["priority_fee"] == "0", "exact_paid_recoverable_fee5_shared_budget_required")
            self.orders[order_id] = {"order_id": order_id, "intent_id": intent, "label": label, "name": name, "bytes": len(original),
                "sha256": digest(original), "deadlines": prepared["deadlines"], "fees": {"delivery": "5", "reservation": "0", "bonus": "0"}}
        self.summary["budget_id"] = self.budget
        self.summary["orders"] = list(self.orders.values())
        self.progress("two-new-original-deadlines-fixed")
        self.call(source, "confirm_batch", intent_ids=[item["intent_id"] for item in self.orders.values()])

    def wait_allocations_and_stop_source(self):
        source, receiver, provider = self.nodes
        def uploaded():
            for order_id in self.orders:
                self.get(source, order_id)
            for order_id in self.orders:
                records = self.rows(provider, "SELECT idx,kind,committed,complete FROM objects WHERE order_id=? ORDER BY idx,kind", (order_id,))
                if len(records) != 2 or any(index != 0 or not complete or committed <= 0 for index, _, committed, complete in records):
                    return None
                need({kind for _, kind, _, _ in records} == {"ciphertext", "capsule"}, "provider_has_both_real_c_e_objects")
            received = {item["order_id"] for item in self.call(receiver, "list")["orders"]}
            return True if set(self.orders).issubset(received) and self.proof_count(2) == 2 else None
        self.wait("two-genuine-source-allocations-complete-upload-and-offers", uploaded, timeout=16000)
        snapshot = self.public_chain(orders=True)
        slots = {("0x" + value, 0) for value in self.orders}
        reserves = [item for item in snapshot["transactions"] if item["method"] == "reserveBatch"]
        need(any({(slot["orderId"], slot["windowIndex"]) for slot in item["slots"]} == slots and len(item["slots"]) == 2 for item in reserves),
             "one_actual_reserve_batch_must_cover_two_distinct_orders")
        self.compare_public_values(snapshot, relations=(2,))
        need(snapshot["budget"]["locked"] == "10" and snapshot["budget"]["earned"] == "0", "pre_service_irrevocable_exact_allocation_required")
        need(not any(item["method"] in ("holdBatch", "revealBatch", "withdrawBudget") for item in snapshot["transactions"]), "no_early_provider_earned_or_payment")
        need(all(phase == "confirmed" for _, _, phase in self.rows(source, "SELECT id,hash,phase FROM transactions")), "source_has_no_unknown_payment_before_offline")
        source_nonce = int(self.rpc("eth_getTransactionCount", [source.wallet_address, "latest"]), 16)
        self.cli(source, ["lock"])
        need(self.cli(source, ["status"])["state"] == "locked", "payer_wallet_locked_before_offline")
        source.client.close()
        source.stop()
        need(source.process is None, "payer_process_stopped_after_real_upload_and_allocation")
        self.summary["source_offline"] = {"wallet_nonce": source_nonce, "after_two_slot_reserve": True,
            "after_both_c_e_objects_complete": True, "after_both_receiver_offers": True, "before_receiver_accept": True,
            "wall_clock_unix_seconds": int(time.time()), "chain_anchor": snapshot["anchor"]}
        self.public_json("before-source-offline.json", snapshot)
        self.checkpoint("source-offline-before-receiver-accept")

    def compare_public_values(self, snapshot, *, relations=(2, 3)):
        for window in snapshot["windows"]:
            for relation in relations:
                proof = [item for item in self.proofs.values() if item["entry"].get("orderId") == window["orderId"] and item["entry"]["relation"] == relation]
                need(len(proof) == 1 and proof[0]["journal"].hex() == window["public_values"][str(relation)], "exported_receipt_must_equal_product_public_values")

    def holds_and_settle(self):
        source, receiver, provider = self.nodes
        self.call(receiver, "accept_batch", order_ids=list(self.orders))
        def held():
            for order_id in self.orders:
                self.get(receiver, order_id)
            status = self.cli(provider, ["budget", "status", "--budget-id", self.budget])
            expected = {(self.budget, order_id, 0) for order_id in self.orders}
            ready = status["ready_windows"]
            actual = {(item["budget_id"], item["order_id"], item["index"]) for item in ready}
            return True if self.proof_count(3) == 2 and len(ready) == 2 and actual == expected else None
        self.wait("two-genuine-holds-private-before-explicit-flush", held, timeout=16000)
        before = self.public_chain(orders=True)
        need(not any(item["method"] in ("holdBatch", "revealBatch", "withdrawBudget") for item in before["transactions"]), "provider_batch_must_wait_for_explicit_flush")
        self.cli(provider, ["budget", "flush", "--budget-id", self.budget])
        def settled():
            values = [self.get(receiver, order_id) for order_id in self.orders]
            if not all(value["evidence"] == "recoverable" and value["payment"]["earned"] == "5" for value in values):
                return None
            state = self.cli(provider, ["budget", "status", "--budget-id", self.budget])
            budgets = state["budgets"]
            need(len(budgets) == 1 and budgets[0]["terms"]["budget_id"] == self.budget, "single_actual_shared_budget_status")
            budget = budgets[0]
            return budget if budget["earned"] == "10" and budget["withdrawn"] == "10" else None
        budget = self.wait("provider-independent-reveal-earned-and-cumulative-withdraw10", settled, timeout=300)
        need(source.process is None, "payer_stays_offline_through_payment_and_recovery")
        need(int(self.rpc("eth_getTransactionCount", [source.wallet_address, "latest"]), 16) == self.summary["source_offline"]["wallet_nonce"],
             "payer_nonce_must_not_increase_after_offline")
        reads = []
        for order_id, original in self.orders.items():
            value = self.call(receiver, "read", order_id=order_id)
            recovered = base64.b64decode(value["content_base64"], validate=True)
            need(recovered == self.original and value["name"] == original["name"] and value["bytes"] == original["bytes"]
                 and value["sha256"] == original["sha256"], "real_receiver_file_name_length_and_hash_must_match")
            order = self.get(receiver, order_id)
            need(order["payment"]["earned"] == "5" and order["payment"]["withdrawn"] == "0"
                 and not any(item["decryption_proven"] for item in order["windows"]), "per_order_earned_and_budget_withdraw_are_distinct")
            reads.append({"order_id": order_id, "name": value["name"], "bytes": value["bytes"], "sha256": value["sha256"], "payer_offline": True})
        snapshot = self.public_chain(orders=True, capsules=True)
        self.compare_public_values(snapshot)
        need(int(snapshot["balances"][provider.wallet_address]) - int(self.summary["token_balances_before"][provider.wallet_address]) == 10
             and snapshot["balances"][self.deployment["escrow"].lower()] == "0"
             and snapshot["budget"]["earned"] == "10" and snapshot["budget"]["withdrawn"] == "10"
             and snapshot["budget"]["locked"] == "0" and snapshot["budget"]["refunded"] == "0", "real_testtoken_conservation_and_payee_increment10")
        self.summary["recovered_files"] = reads
        self.summary["budget_final"] = {name: budget[name] for name in ("locked", "earned", "withdrawn", "refunded", "free", "state")}
        self.summary["payer_post_service_nonce_delta"] = 0
        self.export_settlement(snapshot)

    def export_settlement(self, snapshot):
        source, receiver, provider = self.nodes
        callers = {"registerKey": receiver.wallet_address, "approve": source.wallet_address, "openBudget": source.wallet_address,
                   "reserveBatch": source.wallet_address, "holdBatch": provider.wallet_address, "revealBatch": provider.wallet_address, "withdrawBudget": provider.wallet_address}
        transactions = {}
        for method, caller in callers.items():
            selected = [item for item in snapshot["transactions"] if item["method"] == method]
            need(selected and all(item["from"] == caller for item in selected), "actual_budget_methods_and_fixed_callers_required")
            if method in ("registerKey", "approve", "openBudget"):
                need(len(selected) == 1, "single_registration_approval_and_budget_open_required")
                transactions[method] = selected[0]["hash"]
            else:
                transactions[method] = [item["hash"] for item in selected]
        slots = {("0x" + value, 0) for value in self.orders}
        for method in ("reserveBatch", "holdBatch", "revealBatch"):
            selected = [item for item in snapshot["transactions"] if item["method"] == method]
            need(any(len(item["slots"]) == 2 and {(slot["orderId"], slot["windowIndex"]) for slot in item["slots"]} == slots for item in selected), "actual_two_distinct_slot_" + method + "_required")
        need(len(transactions["withdrawBudget"]) == 1 and len(self.proofs) == 5 and self.proof_count(1) == 1
             and self.proof_count(2) == 2 and self.proof_count(3) == 2, "five_new_receipts_one_cumulative_withdraw_required")
        bundle = {"schemaVersion": 2, "escrow": self.deployment["escrow"], "asset": self.deployment["asset"], "budgetId": "0x" + self.budget,
                  "orders": [{"orderId": "0x" + value, "windowCount": 1, "amount": "5"} for value in self.orders],
                  "proofs": [item["entry"] for item in self.proofs.values()],
                  "capsules": [{name: item[name] for name in ("orderId", "windowIndex", "capsule")} for item in snapshot["windows"]], "transactions": transactions}
        self.public_json("settlement.json", bundle)
        self.public_json("confirmed-chain.json", snapshot)
        observed = self.command(["node", self.repository / "contracts/delivery/observe-settlement.mjs", "--settlement", self.public / "settlement.json",
                                 "--rpc-url", self.deployment["rpc_url"], "--case", "protected"], timeout=180)
        value = json.loads(observed.stdout)
        need(value.get("status") == "PASS", "official_protected_settlement_observer_required")
        self.public_json("settlement-observation.json", value)
        self.summary["official_settlement_observer"] = "PASS"

    def restart_and_disabled_boundaries(self):
        self.progress("real-s-r-p-restart-locked-disabled-history")
        previous = {node.name: self.rows(node, "SELECT id,hash,phase FROM transactions ORDER BY id") for node in self.nodes}
        previous_receipts = {node.name: self.rows(node, "SELECT digest,path FROM proof_jobs ORDER BY digest") for node in self.nodes}
        for node in self.nodes:
            if node.process is not None:
                self.cli(node, ["lock"])
                node.client.close()
                node.stop()
            node.start()
            need(self.cli(node, ["status"])["state"] == "locked", "restarted_real_wallet_must_be_locked")
            if node.name != "P":
                node.client.connect()
                node.client.authenticate()
            settings_path = node.directory / "acceptance-delivery-settings.json"
            settings = json.loads(settings_path.read_bytes())
            settings.update({"enabled": False, "paid_enabled": False})
            save_json(settings_path, settings)
            self.cli(node, ["configure", str(settings_path)])
            self.cli(node, ["budget", "withdraw", "--budget-id", self.budget, "--intent-id", str(uuid.uuid4())], success=False)
            status = self.cli(node, ["budget", "status", "--budget-id", self.budget])
            need(status["execution_enabled"] is False, "disabled_locked_budget_status_is_read_only")
        for order_id in self.orders:
            value = self.call(self.nodes[1], "read", order_id=order_id)
            need(base64.b64decode(value["content_base64"], validate=True) == self.original, "restart_disabled_history_still_recovers_original")
        need(previous == {node.name: self.rows(node, "SELECT id,hash,phase FROM transactions ORDER BY id") for node in self.nodes}
             and previous_receipts == {node.name: self.rows(node, "SELECT digest,path FROM proof_jobs ORDER BY digest") for node in self.nodes},
             "restart_and_disabled_operations_must_not_repeat_proof_or_business_write")
        self.summary["real_process_restart"] = {"nodes": ["S", "R", "P"], "wallets_locked": True, "lab_disabled": True,
            "original_receipts_and_transactions_unchanged": True, "receiver_history_read": True}

    def execute(self):
        try:
            self.start()
            self.register()
            self.open_and_prepare()
            self.wait_allocations_and_stop_source()
            self.holds_and_settle()
            self.restart_and_disabled_boundaries()
            git_env = {**os.environ, "GIT_OPTIONAL_LOCKS": "0"}
            need(self.command(["git", "rev-parse", "HEAD"], environment=git_env).stdout.decode().strip() == self.config["source_commit"]
                 and not self.command(["git", "status", "--porcelain", "--untracked-files=normal"], environment=git_env).stdout,
                 "product_head_must_remain_clean_and_unchanged_for_entire_run")
            self.checkpoint("final")
            self.summary["status"] = "PASS"
        except BaseException as error:
            self.summary["status"] = "INTERRUPTED" if isinstance(error, (KeyboardInterrupt, SystemExit)) else "FAIL"
            self.summary["failure_code"] = str(error) if isinstance(error, CheckFailure) else type(error).__name__
            durable(self.run / "failure-private.log", traceback.format_exc().encode())
            try:
                self.scan_receipts()
            except BaseException:
                durable(self.run / "partial-export-failure-private.log", traceback.format_exc().encode())
        finally:
            survivors = self.processes.stop_all()
            for node in self.nodes:
                try:
                    node.client.close()
                    node.stop()
                except BaseException:
                    pass
            if getattr(self, "deployment_process", None) is not None:
                try:
                    self.deployment_process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.summary["status"] = "FAIL"
                    self.summary["failure_code"] = "deployment_child_cleanup_incomplete"
            if survivors:
                self.summary["status"] = "FAIL"
                self.summary["failure_code"] = "owned_child_cleanup_incomplete"
                self.summary["owned_child_survivors"] = survivors
            if self.initial_resources:
                try:
                    _, final = resources()
                    self.summary["final_resources"] = final
                    self.summary["oom_delta"] = oom_delta(self.initial_resources, final)
                    if any(self.summary["oom_delta"].values()):
                        self.summary["status"] = "FAIL"
                        self.summary["failure_code"] = "actual_cgroup_oom_during_experiment"
                except BaseException:
                    self.summary["status"] = "FAIL"
                    self.summary["failure_code"] = "final_resource_read_failed"
            self.publish_registration_diagnostics()
            self.progress()
            try: diagnostics = self.collect_proof_diagnostics()
            except Exception: diagnostics = None
            self.public_json("native-result.json", final_proof_diagnostics(self.summary, diagnostics))
            self.progress()
        return 0 if self.summary["status"] == "PASS" else 1


def run_experiment(config, helpers, run_directory):
    os.umask(0o077)
    previous = signal.getsignal(signal.SIGTERM)
    def interrupt(_signum, _frame):
        raise KeyboardInterrupt()
    signal.signal(signal.SIGTERM, interrupt)
    try:
        return Experiment(config, helpers, run_directory).execute()
    finally:
        signal.signal(signal.SIGTERM, previous)



def run_from_options(options, helpers):
    need(options.delivery_config is not None, "owner_only_final_delivery_config_required")
    path = options.delivery_config
    need("trash" not in str(path).lower(), "forbidden_configuration_path")
    config = validate_config(json.loads(read_regular(path.resolve(strict=True), 16384, private=True)))
    for name in ("binary", "core_binary", "crypto_module"):
        need(Path(config["paths"][name]).resolve(strict=True) == getattr(options, name).resolve(strict=True), "cli_option_and_final_config_artifact_mismatch")
    run_directory = options.delivery_run_dir or options.artifacts.resolve() / ("delivery-protected-" + uuid.uuid4().hex)
    return run_experiment(config, helpers, run_directory)


def self_test_proof_fixture(node, index, artifacts, profile, mode="prove"):
    peer, order = ("11" if node == "S" else "22") * 32, ("ab" if index == 0 else "cd") * 32
    relation = 2 if node == "S" else 3
    records = [{"v": 2, "kind": "job", "node": peer, "order": order, "relation": relation, "window": 0,
                "job": digest((node + str(index)).encode()), "profile": profile},
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


def self_test_proof_diagnostics():
    from unittest.mock import Mock, patch
    from types import SimpleNamespace
    import tempfile
    artifacts = {"prover": "b" * 64, "r0vm": "c" * 64, "profile": "d" * 64}
    source, profile = "1" * 40, "f" * 64
    orders = [{"order_id": value * 32, "sha256": "e" * 64} for value in ("ab", "cd")]
    principals = {node: {"peer": value * 32} for node, value in (("S", "11"), ("R", "22"))}
    encode = lambda records: b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode() for record in records)
    def summarize(records, node="S", index=0, facts=()):
        return summarize_proof_job(encode(records), node, index, principals[node]["peer"], orders[index]["order_id"], profile,
                                   artifacts, [1, 2], digest((node + str(index)).encode()), facts)
    fixtures = {(node, index): self_test_proof_fixture(node, index, artifacts, profile) for node in ("S", "R") for index in range(2)}
    jobs = [summarize(records, node, index) for (node, index), records in fixtures.items()]
    value = {"schema_version": 1, "source_commit": source, "artifact_sha256": artifacts, "profile_id": profile,
             "counter_fields": list(DIAGNOSTIC_COUNTERS), "attempt_policy": "first_admitted", "availability": "complete", "jobs": jobs}
    need(project_proof_diagnostics(value, source, artifacts, profile, principals, orders, []) == value
         and len(jobs) == 4 and len({job["job_digest"] for job in jobs}) == 4
         and all(job["availability"] == "complete" and job["binding"] == "reserved_job" for job in jobs), "four_concrete_bound_proof_jobs")
    need(all((job["source_activity"] is not None) == (job["node"] == "S") for job in jobs), "source_only_activity")
    need(registration_diagnostics(encode(fixtures["S", 0]), source, artifacts,
         {"exit_code": 0, "signal": None, "spawn_errno": None})["availability"] == "unavailable", "order_v2_never_becomes_key_v1")
    facts = [{"relation": job["relation"], "orderId": "0x" + orders[job["order_index"]]["order_id"], "windowIndex": 0,
              "journalHash": job["job_digest"], "official_host_verified": True, "wrong_journal_receipt_comparison_rejected": True} for job in jobs]
    verified = [summarize(records, node, index, facts) for (node, index), records in fixtures.items()]
    need(all(job["binding"] == "verified_receipt" for job in verified), "only_existing_verified_receipts_upgrade_binding")
    for node in ("S", "R"):
        reconciled = summarize(self_test_proof_fixture(node, 0, artifacts, profile, "receipt_reconcile"), node)
        need(reconciled["availability"] == "complete" and reconciled["mode"] == "receipt_reconcile"
             and reconciled["source_activity"] is None, "receipt_reconciliation_does_not_claim_new_prove")
    records = fixtures["S", 0]
    cut = next(i for i, event in enumerate(records) if event.get("p") == "host_succinct" and event["e"] == "begin")
    timeout = copy.deepcopy(records[:cut + 1]); stamp = timeout[-1]["t"]
    activity = next(copy.deepcopy(event) for event in records if event["kind"] == "source_activity" and event["e"] == "end")
    activity.update(t=stamp + 1000, q=[stamp + 1000] * 7); timeout.append(activity)
    terminal = next(copy.deepcopy(event) for event in records if event.get("p") == "proof_prove" and event["e"] == "ok")
    terminal.update(e="error", t=stamp + 2000, x=None, f="timeout", q=[stamp + 2000] * 5, i=[True, True, True, False, None, False, True]); timeout.append(terminal)
    outer = copy.deepcopy(records[-1]); outer.update(e="error", t=stamp + 3000, f="stage_error", q=[stamp + 3000] * 5); timeout.append(outer)
    partial = summarize(timeout, facts=facts)
    need(partial["availability"] == "partial" and partial["binding"] == "verified_receipt"
         and partial["last_open_phase"]["phase"] == "host_succinct" and partial["command"]["timed_out"]
         and partial["command"]["stdout_bytes"] is None and partial["command"]["exit_code"] is None, "timeout_prefix_stays_partial_after_later_receipt")
    cut = next(i for i, event in enumerate(records) if event.get("p") == "worker_wait")
    cleanup = copy.deepcopy(records[:cut + 2]); cleanup[-2].update(e="error", x=137, f="worker_exit")
    cleanup[-1].update(e="error", x=None, s=9, f="worker_signal", k=True)
    failed = summarize(cleanup)
    need(failed["first_observed_failure"]["exit_code"] == 137 and failed["first_observed_failure"]["signal"] is None
         and failed["availability"] == "partial", "cleanup_never_overwrites_first_noncleanup_failure")
    for errno in (11, 12):
        spawn = copy.deepcopy(records[:4]); terminal = copy.deepcopy(spawn[-1])
        terminal.update(e="error", n=errno, f="spawn", t=4000, q=[4000] * 5); spawn.append(terminal)
        outer = copy.deepcopy(records[-1]); outer.update(e="error", a="not_started", t=5000, q=[5000] * 5, f="stage_error"); spawn.append(outer)
        observed = summarize(spawn)
        need(observed["command"]["spawn_errno"] == errno and not observed["command"]["spawned"]
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
        except (CheckFailure, ValueError, TypeError, KeyError): negative_count += 1
        else: raise AssertionError("invalid order diagnostic accepted")
    for bad in (b"", encode(records)[:-1], b"x" * 32769, b"x" * 512 + b"\n", encode(records + [records[-1]] * (65 - len(records))),
                encode(records).replace(b'"v":2', b'"v":2,"v":2', 1)):
        try: summarize_proof_job(bad, "S", 0, principals["S"]["peer"], orders[0]["order_id"], profile, artifacts, [1, 2], records[0]["job"], [])
        except (CheckFailure, ValueError, TypeError, KeyError): negative_count += 1
        else: raise AssertionError("invalid order diagnostic byte stream accepted")
    for change in (lambda data: data[1].update(core_same_target=False), lambda data: data[1].update(target_cgroup=[3, 4]),
                   lambda data: next(event for event in data if event.get("w") == "host").update(g=None),
                   lambda data: next(event for event in data if event.get("w") == "shim").update(g=[3, 4]),
                   lambda data: next(event for event in data if event["kind"] == "source_activity" and event["e"] == "end").update(q=[0] * 7)):
        unknown = copy.deepcopy(records); change(unknown); need(summarize(unknown)["availability"] == "partial", "unknown_group_or_activity_stays_partial")
    for change in (lambda data: data.update(source_commit="0" * 40), lambda data: data["artifact_sha256"].update(prover="0" * 64),
                   lambda data: data.update(profile_id="0" * 64), lambda data: data.update(private="PRIVATE_PROOF_SENTINEL"),
                   lambda data: data["jobs"][0].update(window=True), lambda data: data["jobs"][0]["command"].update(exit_code=137, signal=9),
                   lambda data: data["jobs"][0]["command"].update(failure_class="worker_exit", exit_code=0),
                   lambda data: data["jobs"][0]["command"].update(failure_class="worker_signal", exit_code=137, signal=None),
                   lambda data: data["jobs"][1].update(job_digest=data["jobs"][0]["job_digest"]),
                   lambda data: data["jobs"][0].update(source_activity={"start": {"private": "PRIVATE_PROOF_SENTINEL"}, "end": None})):
        invalid = copy.deepcopy(value); change(invalid)
        try: project_proof_diagnostics(invalid, source, artifacts, profile, principals, orders, facts)
        except (CheckFailure, ValueError, TypeError, KeyError): negative_count += 1
        else: raise AssertionError("invalid public proof diagnostic accepted")
    source_text = (REPOSITORY / "apps/core/src/delivery/proof.rs").read_text()
    admitted = source_text.split("impl ProofRunner {", 1)[1].split("async fn verified_seal", 1)[0]
    admission = [admitted.index(part) for part in (".try_acquire()", "process_lock(receipt)", "Diagnostics::claim_order", "config.validate()?", "config.validate_executables()?", "config.check_memory_limit()?", "config.oom_events()?", "/var/run/docker.sock", "verify_relation(", "input.profile == config.profile", "sink.activate_order")]
    need(admission == sorted(admission) and "proof_preflight" not in PROOF_PHASES["core"], "original_admission_precedes_order_activation")
    with tempfile.TemporaryDirectory(prefix="proof-collector-selftest-", dir=ARTIFACTS) as temporary:
        run = Path(temporary)
        experiment = object.__new__(Experiment)
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
            need(channel is not None and not os.get_inheritable(channel["fd"]), "collector_directory_is_pinned_private")
            for index in range(2):
                fixture = copy.deepcopy(fixtures[node.name, index])
                fixture[1]["target_cgroup"] = synthetic_target
                for event in fixture[2:]:
                    if event["g"] is not None: event["g"] = synthetic_target
                path = channel["path"] / (fixture[0]["job"] + ".jsonl")
                descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                os.write(descriptor, encode(fixture)); os.close(descriptor)
        experiment.rows = Mock(side_effect=lambda node, _sql: [(fixtures[node.name, index][0]["job"], str(node.directory / "delivery/private" / ("proof-" + fixtures[node.name, index][0]["job"] + ".receipt"))) for index in range(2)])
        with patch.dict(globals(), resources=lambda: (run, {})):
            collected = experiment.collect_proof_diagnostics()
        need(collected["availability"] == "complete" and len(collected["jobs"]) == 4
             and all(job["resources"]["cgroup"] == synthetic_target for job in collected["jobs"]), "four_synthetic_streams_bound_to_the_pinned_directory_fixture")
        for node in experiment.nodes: need(experiment.proof_channels[node.name] is None, "collector_closes_pinned_descriptors")
        need(experiment.collect_proof_diagnostics() == collected, "collector_never_upgrades_first_attempt")
    for kind in ("valid", "missing", "fifo", "directory", "symlink", "hardlink", "mode", "owner", "changed", "replaced-directory", "overflow", "half-line", "extra"):
        with tempfile.TemporaryDirectory(prefix="proof-collector-negative-", dir=ARTIFACTS) as temporary:
            run = Path(temporary); node = SimpleNamespace(name="S", device_id=principals["S"]["peer"], directory=run / "S"); node.directory.mkdir(mode=0o700)
            experiment = object.__new__(Experiment); experiment.nodes = [node, SimpleNamespace(name="R", device_id=principals["R"]["peer"])]; experiment.proof_channels = {}; experiment.proof_diagnostic_result = None
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
            with patch.dict(globals(), resources=lambda: (_ for _ in ()).throw(OSError())), patch.object(os, "fstat", side_effect=checked_fstat):
                unavailable = experiment.collect_proof_diagnostics()
            if kind == "valid":
                need(unavailable["availability"] != "unavailable", "safe_stream_with_reservation_is_collected")
                continue
            need(unavailable["availability"] == "unavailable" and all(job == unavailable_proof_job(job["node"], job["order_index"]) for job in unavailable["jobs"]), "unsafe_stream_is_nonblocking_unavailable")
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
    phase = {"writer": "host", "command": "prove", "phase": max(PROOF_PHASES["host"], key=len), "t_ns": maximum, "event_index": 63}
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
    need(private_max <= 512 and len(records) <= 56 and stream_bound <= 32768 and public_bound <= 16384, "actual_amended_serialization_bounds")
    for status in ("PASS", "FAIL"):
        base = {"status": status, "genuine_receipts": [], "payload": ""}
        minimal = {**value, "availability": "unavailable", "jobs": [unavailable_proof_job(job["node"], job["order_index"]) for job in jobs]}
        overhead = len((json.dumps({**base, "proof_diagnostics": minimal}, indent=2, sort_keys=True) + "\n").encode())
        base["payload"] = "x" * (131072 - overhead)
        result = final_proof_diagnostics(base, value)
        need(result["status"] == status and result["genuine_receipts"] == [] and result["payload"] == base["payload"]
             and result["proof_diagnostics"]["availability"] == "unavailable" and len((json.dumps(result, indent=2, sort_keys=True) + "\n").encode()) <= 131072, "combined_cap_uses_minimal_unavailable_without_business_changes")
        base["payload"] = "x" * (131072 - len((json.dumps({**base, "payload": ""}, indent=2, sort_keys=True) + "\n").encode()))
        need(final_proof_diagnostics(base, value) == base, "combined_cap_omits_only_diagnostics")
    oversized = copy.deepcopy(value); oversized["jobs"][0]["command"]["failure_class"] = "x" * 16385
    need(final_proof_diagnostics({"status": "PASS"}, oversized)["proof_diagnostics"]["availability"] == "unavailable", "public_field_cap_does_not_expand")
    print(json.dumps({"proof_diagnostic_selftest": "PASS", "concrete_jobs": 4, "negative_controls": negative_count,
                      "group_unknown_controls": 5, "collector_negative_controls": 12,
                      "source_records": len(records), "max_record_bound_bytes": private_max, "stream_bound_bytes": stream_bound,
                      "public_field_bound_bytes": public_bound, "combined_cap_bytes": 131072, "external_processes": 0, "network_sockets": 0}))


def self_test_proof_review_regressions():
    artifacts = {"prover": "b" * 64, "r0vm": "c" * 64, "profile": "d" * 64}
    source, profile = "1" * 40, "f" * 64
    principals = {node: {"peer": value * 32} for node, value in (("S", "11"), ("R", "22"))}
    orders = [{"order_id": value * 32} for value in ("ab", "cd")]
    fixtures = {(node, index): self_test_proof_fixture(node, index, artifacts, profile) for node in ("S", "R") for index in range(2)}
    def summarize(records, node="S", index=0, target=(1, 2)):
        data = b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode() for record in records)
        return summarize_proof_job(data, node, index, principals[node]["peer"], orders[index]["order_id"], profile,
            artifacts, list(target) if target is not None else None, fixtures[node, index][0]["job"], [])
    baseline = fixtures["S", 0]
    jobs = [summarize(records, node, index) for (node, index), records in fixtures.items()]
    failures, observed = [], {}
    def check(label, records, expected_complete):
        try: row = summarize(records); status = row["availability"]
        except CheckFailure: row, status = None, "rejected"
        observed[label] = status
        if (status == "complete") != expected_complete: failures.append(label)
        return row
    def project(row):
        rows = [row, *jobs[1:]]
        value = {"schema_version": 1, "source_commit": source, "artifact_sha256": artifacts, "profile_id": profile,
            "counter_fields": list(DIAGNOSTIC_COUNTERS), "attempt_policy": "first_admitted", "availability": proof_availability(rows), "jobs": rows}
        return project_proof_diagnostics(value, source, artifacts, profile, principals, orders, [])
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
            except CheckFailure: rejected = True
            else: rejected = False
            observed["public_" + label] = rejected
            if not rejected: failures.append("public_" + label)
    unknown = summarize(baseline, target=None)
    if any(value is not None for point in unknown["source_activity"].values() for value in point["values"]): failures.append("activity_unknown_target")
    partial = copy.deepcopy(baseline)
    next(record for record in partial if record.get("w") == "host")["g"] = [3, 4]
    row = summarize(partial)
    need(row["availability"] == "partial" and row["source_activity"] == jobs[0]["source_activity"]
         and project(row)["jobs"][0]["source_activity"] == jobs[0]["source_activity"], "valid_same_target_partial_keeps_activity")
    print(json.dumps({"proof_review_regressions": observed, "failures": failures}, sort_keys=True))
    need(not failures, "proof_review_regression_failures")


def self_test_diagnostic_fixture():
    records = []
    def event(writer, command, phase, edge, *, worker=None, code=None):
        counters = None
        if (writer, phase) in DIAGNOSTIC_RESOURCES:
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


def self_test_diagnostics():
    source, artifacts = "1" * 40, {name: "a" * 64 for name in ("prover", "r0vm", "profile")}
    cli = {"exit_code": 0, "signal": None, "spawn_errno": None}
    records = self_test_diagnostic_fixture()
    encode = lambda items: b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode() for record in items)
    full = encode(records)
    positive = registration_diagnostics(full, source, artifacts, cli)
    need(positive["availability"] == "complete" and len(positive["events"]) == len(records) <= 64
         and len(positive["resource_deltas"]) == 8 and len(full) <= 32768, "complete_registration_diagnostic_path_fits_bounds")
    need(all(row["memory_max"] == 2 and row["pids_max"] == 0 and row["oom"] == 0 for row in positive["resource_deltas"]),
         "diagnostic_pressure_not_invented_oom_or_pid_failure")
    bad = []
    for key, item in (("exit_code", True), ("signal", 65), ("errno", 4096), ("phase", "UNAPPROVED_PRIVATE_SENTINEL"), ("private", "UNAPPROVED_PRIVATE_SENTINEL")):
        changed = copy.deepcopy(records); changed[0][key] = item; bad.append(encode(changed))
    changed = copy.deepcopy(records); changed[2]["counters"]["memory_max"] = 3; bad.append(encode(changed))
    bad += [full + b"x" * 32769, encode(records + records), encode([records[1], records[0], *records[2:]]),
            encode([records[0], records[0], *records[1:]]), b" " * 513 + b"\n", full.replace(b'"v":1', b'"v":1,"v":1', 1)]
    bad.append(encode([record for record in records if record["writer"] != "host"]))
    for data in bad:
        value = registration_diagnostics(data, source, artifacts, cli)
        need(value["availability"] == "unavailable" and not value["events"]
             and "UNAPPROVED_PRIVATE_SENTINEL" not in json.dumps(value), "invalid_diagnostics_are_unavailable_without_private_text")
    missing = copy.deepcopy(records)
    for record in missing: record["counters"] = None
    projected = registration_diagnostics(encode(missing), source, artifacts, cli)
    need(all(all(row[name] is None for name in DIAGNOSTIC_COUNTERS) for row in projected["resource_deltas"]), "unobserved_counters_are_null")
    boundary = next(index for index, record in enumerate(records) if record["phase"] == "worker_wait")
    for signal_number, exit_code in ((9, None), (None, 137)):
        failed = copy.deepcopy(records[:boundary + 1])
        failed[-1].update(edge="error", signal=signal_number, exit_code=exit_code,
                          failure_class="worker_signal" if signal_number else "worker_exit")
        value = registration_diagnostics(encode(failed), source, artifacts, {**cli, "exit_code": 1})
        need(value["availability"] == "partial" and value["first_observed_failure"]["signal"] == signal_number
             and value["first_observed_failure"]["exit_code"] == exit_code, "worker_signal_not_inferred_from_137")
    partial = value
    cleanup = copy.deepcopy(failed)
    cleanup.append({**records[boundary + 1], "edge": "error", "exit_code": None, "signal": 15,
                    "failure_class": "worker_signal", "cleanup_requested": True})
    value = registration_diagnostics(encode(cleanup), source, artifacts, partial["cli"])
    need(value["availability"] == "partial" and value["first_observed_failure"] == partial["first_observed_failure"]
         and value["events"][-1]["cleanup_requested"], "cleanup_failure_preserves_first_worker_failure")
    invalid_waits = []
    for change in ({"exit_code": 0}, {"failure_class": "worker_signal"},
                   {"exit_code": None, "signal": 9, "failure_class": "worker_exit"}, {"errno": 5},
                   {"signal": 9}, {"exit_code": None},
                   {"edge": "ok", "exit_code": 1, "failure_class": None},
                   {"edge": "ok", "exit_code": None, "failure_class": None}):
        raw = copy.deepcopy(failed); raw[-1].update(change)
        public = copy.deepcopy(partial); public["events"][-1].update(change)
        public["first_observed_failure"] = diagnostic_first_failure(public["events"])
        public["first_observed_failed_phase"] = diagnostic_phase(next((event for event in public["events"] if event["edge"] == "error"), None))
        invalid_waits.append((raw, public))
    raw, public = copy.deepcopy(records), copy.deepcopy(positive)
    for event in [*raw, *public["events"]]:
        if event["phase"] == "worker_wait": event["exit_code"] = None
    invalid_waits.append((raw, public))
    for raw, public in invalid_waits:
        value = registration_diagnostics(encode(raw), source, artifacts, public["cli"])
        need(value["availability"] == "unavailable" and not value["events"] and not value["resource_deltas"]
             and all(value[key] is None for key in ("first_observed_failure", "last_observed_phase", "first_observed_failed_phase")),
             "contradictory_worker_wait_raw_is_unavailable")
        try: project_registration_diagnostics(public, source, artifacts)
        except CheckFailure: pass
        else: raise AssertionError("contradictory worker_wait public projection accepted")
    core_end = next(index for index, event in enumerate(records) if event["writer"] == "core" and event["phase"] == "proof_prove" and event["edge"] == "ok")
    for failure_class in ("result_decode", "receipt_check"):
        core_failed = copy.deepcopy(records[:core_end + 1])
        core_failed[-1].update(edge="error", failure_class=failure_class)
        value = registration_diagnostics(encode(core_failed), source, artifacts, {**cli, "exit_code": 1})
        need(value["availability"] == "partial" and value["events"][-1]["exit_code"] == 0
             and value["events"][-1]["failure_class"] == failure_class, "core_validation_can_fail_after_successful_subprocess")
    print(f"Registration diagnostic self-tests passed; full synthetic path {len(records)} events/8 resource groups; no processes or proofs")


def self_test():
    from unittest.mock import Mock, patch
    from types import SimpleNamespace
    import io
    import socket
    import tempfile
    def forbidden(*_args, **_kwargs):
        raise AssertionError("no-process self-test attempted an external process or network")
    with patch.object(subprocess, "Popen", forbidden), patch.object(subprocess, "run", forbidden), patch.object(socket, "socket", forbidden):
        self_test_diagnostics()
        self_test_proof_diagnostics()
        self_test_proof_review_regressions()
        node_helpers = runpy.run_path(str(REPOSITORY / "scripts/verify-node.py"))
        Client = node_helpers["Client"]
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
                            need(str(error) == "client command wait stays within its original deadline", "client_wait_bound_rejects_before_spawn")
                        else: raise AssertionError("client command accepted an invalid wait bound")
                    need(spawn.call_count == 0, "client_invalid_wait_never_spawns")
                    need(client.command("identity") == {"device_id": "synthetic-client"}, "client_first_json_response")
                    need(client.command("request", method="status", params={}, _wait_timeout=7) == {"status": "ready"}, "client_second_json_response")
                    need([json.loads(line) for line in stdin.getvalue().splitlines()] == [
                        {"op": "identity"}, {"op": "request", "method": "status", "params": {}}], "client_original_json_requests")
                    client.close()
                    need(spawn.call_count == 1, "client_reuses_its_helper_process")
                    need(spawn.call_args.args == (["node", "--v8-pool-size=1", "/artifact/client.mjs", *([str(identity)] if identity else [])],),
                         "client_helper_v8_pool_is_fixed")
                    need(spawn.call_args.kwargs == {"stdin": subprocess.PIPE, "stdout": subprocess.PIPE, "stderr": subprocess.DEVNULL,
                         "text": True, "bufsize": 1, "env": {**parent_environment, "UV_THREADPOOL_SIZE": "1"}}, "client_helper_child_pool_and_stdio_preserved")
                    need(dict(os.environ) == parent_environment, "client_helper_does_not_mutate_parent_environment")
                    need([call.args for call in ready.call_args_list] == [([stdout], [], [], 30), ([stdout], [], [], 7)], "client_original_wait_bounds")
                    need(stdin.closed and client.process is None, "client_close_sends_eof_and_releases_process")
                    need(process.wait.call_count == 1 and process.wait.call_args.args == ()
                         and process.wait.call_args.kwargs == {"timeout": 5}, "client_close_original_wait_bound")
                    need(process.kill.call_count == 0, "client_normal_close_does_not_kill")
            stdout.close()
        need(subprocess.Popen is forbidden and subprocess.run is forbidden and socket.socket is forbidden, "client_mock_preserves_outer_process_and_network_gate")
        sample = {"schema_version": 1, "source_commit": "1" * 40,
            "paths": {"repository": str(REPOSITORY), "binary": "/artifact/vibeface", "core_binary": "/artifact/vibeface-core", "crypto_module": "/artifact/vibeface_secure.js", "prover": "/artifact/vibeface-delivery-prover", "r0vm": "/artifact/r0vm"},
            "sha256": {name: "a" * 64 for name in ("binary", "core_binary", "crypto_module", "crypto_wasm", "prover", "r0vm", "profile")},
            "elf_sha256": "b" * 64, "caps": {"deposit": "10", "max_service_amount": "5", "source_gas_cap_wei": CAP, "provider_gas_cap_wei": CAP, "register_key_gas_cap_wei": CAP, "native_funding_wei_each": str(10 ** 19)}, "publication_policy": POLICY}
        validate_config(sample)
        for invalid_channel in (None, "mode", "inode", "overflow", "closed"):
            with tempfile.TemporaryDirectory(prefix="protected-diagnostic-channel-selftest-", dir=ARTIFACTS) as temporary:
                experiment = Experiment(sample, {}, Path(temporary) / "native")
                receiver = SimpleNamespace(directory=experiment.run / "R")
                receiver.directory.mkdir(mode=0o700)
                intent = str(uuid.uuid4())
                experiment.prepare_registration_diagnostics(receiver, intent)
                channel = experiment.registration_channel
                need(channel is not None and not os.get_inheritable(channel["fd"]), "native_diagnostic_descriptor_is_not_inherited")
                data = b"".join((json.dumps(item, separators=(",", ":")) + "\n").encode() for item in self_test_diagnostic_fixture())
                os.write(channel["fd"], data)
                experiment.registration_cli["exit_code"] = 0
                if invalid_channel == "mode": os.fchmod(channel["fd"], 0o644)
                if invalid_channel == "inode": channel["identity"] = (0, 0)
                if invalid_channel == "overflow": os.ftruncate(channel["fd"], 32769)
                if invalid_channel == "closed": os.close(channel["fd"])
                experiment.retire_registration_diagnostics()
                experiment.publish_registration_diagnostics()
                value = diagnostic_json((experiment.public / "registration-diagnostics.json").read_bytes())
                need(value["availability"] == ("complete" if invalid_channel is None else "unavailable"), "bound_diagnostic_channel_validation")
                need(experiment.summary["status"] == "RUNNING" and not experiment.summary["genuine_receipts"], "diagnostics_cannot_complete_business")
                try: os.fstat(channel["fd"])
                except OSError: pass
                else: raise AssertionError("native diagnostic descriptor leaked")
        for change in (lambda item: item.update(source_commit="0" * 40), lambda item: item.update(private_key="forbidden"),
                       lambda item: item["paths"].update(binary="/unrelated/trash/vibeface"), lambda item: item["sha256"].update(prover="0" * 64),
                       lambda item: item["publication_policy"].update(clock_skew_seconds=0), lambda item: item["caps"].update(deposit="11")):
            invalid = copy.deepcopy(sample); change(invalid)
            try: validate_config(invalid)
            except CheckFailure: pass
            else: raise AssertionError("invalid protected config accepted")
        journal = bytes(range(256)) * 5 + bytes(range(224))
        for receipt in (b"prefix" + journal + b"suffix", b"prefix" + journal + b"middle" + journal + b"suffix"):
            need(extract_journal(receipt, digest(journal)) == journal, "unique_public_journal_content")
        for invalid in (b"missing", journal[:-1], b"x" + journal[1:], b"x" * 16385):
            try: extract_journal(invalid, digest(journal))
            except CheckFailure: pass
            else: raise AssertionError("missing, altered or oversized public journal accepted")
        for relation in (1, 2, 3):
            fields = [bytes([7]) * 32 for _ in range(47)]; fields[1] = relation.to_bytes(32, "big")
            need(len(wrong_journal(b"".join(fields))) == 1504, "wrong_journal_shape")
        for url in ("https://127.0.0.1:8545", "http://example.com:8545", "http://user@127.0.0.1:8545", "http://127.0.0.1:8545/path"):
            try: loopback(url)
            except (CheckFailure, ValueError): pass
            else: raise AssertionError("non-loopback origin accepted")
        need(oom_delta({"memory.events": "oom 0\noom_kill 0\n"}, {"memory.events": "oom 1\noom_kill 0\n"})["oom"] == 1, "actual_oom_delta")
        for preconfigured in (False, True):
            with tempfile.TemporaryDirectory(prefix="protected-fresh-settings-selftest-", dir=ARTIFACTS) as temporary:
                run = Path(temporary)
                nodes = []
                for index, name in enumerate(("S", "R", "P")):
                    node = object.__new__(node_helpers["Node"])
                    node.name = name; node.directory = run / name; node.directory.mkdir()
                    node.device_id = str(index + 1) * 64; node.quic_port = 27001 + index
                    node.start = Mock(side_effect=CheckFailure("synthetic_stop_before_real_node_start"))
                    save_json(node.directory / "config.json", {"network": {}})
                    nodes.append(node)
                if preconfigured: save_json(nodes[0].directory / "delivery-settings.json", {"enabled": False, "paid_enabled": False})
                startup = object.__new__(Experiment)
                startup.run = run; startup.paths = {name: Path(value) for name, value in sample["paths"].items()}
                startup.nodes = []; startup.progress = Mock(); startup.deployment = {"funder":"0x" + "11" * 20}
                startup.prepare_proof_diagnostics = Mock()
                startup.rpc = Mock(side_effect=[["0x" + value * 20 for value in ("11","22","33","44")],"0x7a69"])
                startup.helpers = {"CLIENT_SOURCE":"MODULE_URL", "mesh_network":Mock(return_value="t-123456789abc"),
                    "public_node_args":Mock(return_value=["--relay-open"]), "Node":Mock(side_effect=nodes), "isolate_delivery_node":Mock()}
                try: startup.setup_nodes()
                except CheckFailure as error:
                    need(str(error) == ("fresh_delivery_settings_must_be_absent" if preconfigured else "synthetic_stop_before_real_node_start"),
                         "fresh_settings_use_independent_file_contract")
                else: raise AssertionError("the settings selfcheck must stop before real Node startup")
                startup.helpers["mesh_network"].assert_called_once_with()
                need([node.start.call_count for node in nodes] == [0, 0, 0 if preconfigured else 1],
                     "only_absent_delivery_settings_start_the_independent_provider_first")
                for node in nodes:
                    config = json.loads((node.directory / "config.json").read_bytes())
                    need(set(config) == {"network"}, "main_config_must_not_gain_delivery_fields")
                    need(config["network"] == ({} if preconfigured else {"mesh": {"enabled": True, "network": "t-123456789abc",
                        "bootstrap": [] if node.name == "P" else [nodes[2].mesh_address], "allow_private": True}}),
                        "private_mesh_has_only_s_r_p_loopback_links_and_no_public_bootstrap")
        for response in (b"R", b"X", b"", None):
            reader, writer = os.pipe()
            if response is not None:
                os.write(writer, response)
            os.close(writer)
            with patch.dict(os.environ, {OBSERVER_READY_FD: str(reader)}), \
                 patch("select.select", return_value=([] if response is None else [reader], [], [])):
                try: wait_observer_ready()
                except CheckFailure as error:
                    need(response != b"R" and str(error) == "restart_observer_not_ready_before_business", "observer_not_ready_is_fail_closed")
                else: need(response == b"R", "only_actual_ready_byte_releases_business")
                need(OBSERVER_READY_FD not in os.environ, "ready_descriptor_not_inherited_by_business")
            try: os.fstat(reader)
            except OSError: pass
            else: raise AssertionError("the native readiness reader remained open")
        with tempfile.TemporaryDirectory(prefix="protected-ready-selftest-", dir=ARTIFACTS) as temporary:
            startup = Experiment(sample, {}, Path(temporary) / "native")
            def ready_before_processes():
                need(startup.public.is_dir() and not startup.nodes and startup.deployment is None,
                     "fresh_directories_exist_before_observer_gate")
                raise CheckFailure("synthetic_stop_at_readiness_gate")
            with patch.dict(globals(), wait_observer_ready=ready_before_processes):
                try: startup.start()
                except CheckFailure as error: need(str(error) == "synthetic_stop_at_readiness_gate", "ready_precedes_original_start")
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
        with tempfile.TemporaryDirectory(prefix="protected-source-predicate-selftest-", dir=ARTIFACTS) as temporary:
            for name, missing, error_message, count, offers, expected_ready in cases:
                source, receiver, provider = object(), object(), object()
                experiment = object.__new__(Experiment)
                experiment.nodes = [source, receiver, provider]
                experiment.orders = {key: {"deadlines": {"key_deadline": 123}} for key in ("a", "b")}
                experiment.summary = {}; experiment.run = Path(temporary)
                experiment.proofs = {key: {"entry": {"relation": 2}} for key in ("a", "b")[:count]}
                def result(node, operation, **params):
                    if operation == "list":
                        need(node is receiver, "offers_come_from_receiver")
                        return {"orders": [{"order_id": key} for key in offers]}
                    need(node is source and operation == "get", "source_state_checked_before_readiness")
                    if params["order_id"] == "b" and error_message:
                        return {"error": error_message}
                    return {"deadlines": {"key_deadline": 123}}
                experiment.call = Mock(side_effect=result)
                experiment.rows = Mock(side_effect=lambda _node, _query, params: [] if params[0] == missing else complete_objects)
                observed = []
                def wait(label, predicate, *, timeout):
                    need(timeout == 16000, "original_source_stage_bound_unchanged")
                    observed.append(predicate())
                    raise PredicateObserved()
                experiment.wait = wait
                try: experiment.wait_allocations_and_stop_source()
                except PredicateObserved: need(not error_message and observed == [expected_ready], "actual_source_predicate_result")
                except CheckFailure as error:
                    code = "actual_product_proof_execution_deadline_exceeded" if error_message and error_message.startswith(("proof", "prover")) else "actual_paid_order_executor_failed_private_log_retained"
                    need(error_message and str(error) == code and experiment.summary["failure_order_id"] == "b", "actual_get_failure_classification")
                    need(json.loads((experiment.run / "order-failure-b.json").read_bytes()) == {"error": error_message}, "actual_order_error_retained_privately")
                    experiment.rows.assert_not_called()
                else: raise AssertionError("the predicate selfcheck reached business operations")
                need([call.kwargs["order_id"] for call in experiment.call.call_args_list if call.args[1] == "get"] == ["a", "b"],
                     "all_source_errors_checked_before_pending_objects")
        experiment = object.__new__(Experiment)
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
                need(experiment.call(SimpleNamespace(client=client), op, **params) == {"accepted": True}, "explicit_busy_then_success")
            need(client.command.call_count == 2 and client.command.call_args_list[0] == client.command.call_args_list[1], "busy_retry_keeps_exact_request")
            need(client.command.call_args.kwargs["params"] == {"op": op, **params}, "busy_retry_keeps_original_ids")
            client.call.assert_not_called()
            for error in (TimeoutError("network outcome unknown"), RuntimeError("another delivery operation is in progress"),
                          {"error": "outcome unknown"}, {"error": "another delivery operation is in progress; uncertain"}, {"error": "other refusal"}):
                client = Mock(command=Mock(side_effect=[error] if isinstance(error, Exception) else None, return_value=error))
                with patch("time.monotonic", return_value=0), patch("time.sleep") as pause:
                    try: experiment.call(SimpleNamespace(client=client), op, **params)
                    except (CheckFailure, TimeoutError, RuntimeError): pass
                    else: raise AssertionError("a non-busy failure was retried or accepted")
                need(client.command.call_count == 1, "non_busy_failure_is_not_retried"); pause.assert_not_called()
            client = Mock(command=Mock(return_value=busy))
            with patch("time.monotonic", side_effect=[0, 0, 0, 0, 121]), patch("time.sleep"):
                try: experiment.call(SimpleNamespace(client=client), op, **params)
                except CheckFailure as error: need("stage_observer_timeout_" in str(error), "busy_keeps_existing_stage_bound")
                else: raise AssertionError("busy exceeded the original stage bound")
            need(client.command.call_count == 1, "expired_busy_stage_sends_no_new_request")
        client = Mock(call=Mock(side_effect=AssertionError("another delivery operation is in progress")))
        try: experiment.call(SimpleNamespace(client=client), "get", order_id="a" * 64)
        except AssertionError: pass
        else: raise AssertionError("read operations must retain their original Client.call behavior")
        client.command.assert_not_called()
    print(json.dumps({"status": "SELF_TEST_PASS", "external_processes": 0, "network_sockets": 0, "genuine_receipts_generated": 0}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", required=True)
    parser.parse_args()
    self_test()
