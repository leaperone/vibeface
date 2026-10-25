#!/usr/bin/env python3
"""Compare two pinned release packages on the same private relay and resume workload."""

import argparse
import base64
from dataclasses import dataclass
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import re
import resource
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import uuid


ROOT = Path(__file__).resolve().parents[1]
MESSAGE_BYTES = 4096
BLOB_BYTES = 64 * 1024 * 1024
CHUNK_BYTES = 1024 * 1024
SAMPLES_PER_SIDE = 5
OBSERVE_PAUSE = 0.0005
RSS_PAUSE = 0.1
IDLE_SECONDS = 1.0


def require(condition, description):
    if not condition:
        raise AssertionError(description)


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(CHUNK_BYTES), b""):
            digest.update(block)
    return digest.hexdigest()


def checked_path(value):
    require(isinstance(value, str) and Path(value).is_absolute() and "trash" not in value.lower(),
            "Artifact paths must be explicit absolute paths outside excluded directories")
    path = Path(value)
    require(not path.is_symlink() and path.is_file(), "Artifact input must be a regular file")
    return path.resolve(strict=True)


def read_json(path):
    require(not path.is_symlink() and path.is_file() and path.stat().st_size <= 16 * 1024,
            "Configuration and build metadata must be bounded regular files")
    return json.loads(path.read_text())


@dataclass(frozen=True)
class Artifact:
    side: str
    source_commit: str
    binary: Path
    module: Path
    hashes: dict
    build_info: dict

    @classmethod
    def read(cls, side, path):
        value = read_json(path)
        require(set(value) == {"source_commit", "binary", "binary_sha256", "crypto_module",
                               "crypto_module_sha256", "wasm_sha256", "build_info"},
                "Artifact specification has unexpected fields")
        require(re.fullmatch(r"[0-9a-f]{40}", value["source_commit"] or ""), "Pin the full source commit")
        binary, module = checked_path(value["binary"]), checked_path(value["crypto_module"])
        wasm = checked_path(str(module.with_name("vibeface_secure_bg.wasm")))
        hashes = {"binary": value["binary_sha256"], "module": value["crypto_module_sha256"], "wasm": value["wasm_sha256"]}
        for name, file in (("binary", binary), ("module", module), ("wasm", wasm)):
            require(re.fullmatch(r"[0-9a-f]{64}", hashes[name] or "") and sha256_file(file) == hashes[name],
                    "Artifact content differs from its pinned SHA-256")
        require(os.access(binary, os.X_OK) and wasm.read_bytes().startswith(b"\x00asm"),
                "The release binary is executable and its matching WASM is present")
        info = read_json(checked_path(value["build_info"]))
        require(set(info) == {"version", "target", "source_commit", "update_protocol", "data_epoch"}
                and info["source_commit"] == value["source_commit"],
                "BUILD-INFO must identify the pinned release package source")
        return cls(side, value["source_commit"], binary, module, hashes, info)

    def verify(self):
        for name, file in (("binary", self.binary), ("module", self.module),
                           ("wasm", self.module.with_name("vibeface_secure_bg.wasm"))):
            require(sha256_file(file) == self.hashes[name], "One immutable artifact must complete every sample")

    def public(self):
        return {"side": self.side, "source_commit": self.source_commit, "sha256": self.hashes,
                "build_info": self.build_info}


class Coverage:
    def __init__(self, size):
        self.size, self.total, self.intervals = size, 0, []

    def add(self, offset, length):
        require(type(offset) is int and type(length) is int and 0 <= offset <= self.size
                and 0 <= length <= self.size - offset, "Observed payload range is within the original file")
        self.total += length
        if length:
            merged = []
            for start, end in sorted([*self.intervals, (offset, offset + length)]):
                if merged and start <= merged[-1][1]:
                    merged[-1] = (merged[-1][0], max(end, merged[-1][1]))
                else:
                    merged.append((start, end))
            self.intervals = merged

    def result(self):
        unique = sum(end - start for start, end in self.intervals)
        return {"payload_bytes": self.total, "unique_offset_bytes": unique,
                "repeated_offset_bytes": self.total - unique}


def cpu_seconds(value):
    days, separator, clock = value.partition("-")
    if not separator:
        days, clock = "0", days
    parts = clock.split(":")
    require(days.isdigit() and 2 <= len(parts) <= 3, "Unsupported ps CPU time format")
    numbers = [float(part) for part in parts]
    require(all(math.isfinite(number) and number >= 0 for number in numbers), "Invalid ps CPU counter")
    return int(days) * 86400 + sum(number * 60 ** index for index, number in enumerate(reversed(numbers)))


def process_stats(processes):
    if not processes:
        return {}
    result = subprocess.run(["ps", "-p", ",".join(str(pid) for pid in processes), "-o", "pid=,rss=,time="],
                            capture_output=True, text=True, timeout=5)
    require(result.returncode in (0, 1), "Read only the task's process counters")
    rows = {}
    for line in result.stdout.splitlines():
        pid, rss, cpu = line.split()
        pid = int(pid)
        require(pid in processes and int(rss) >= 0, "Process counter belongs to this fixture")
        rows[pid] = {"role": processes[pid], "pid": pid, "rss_bytes": int(rss) * 1024,
                     "cpu_seconds": cpu_seconds(cpu)}
    return rows


class Meter:
    def __init__(self, nodes, clients):
        self.nodes, self.clients, self.samples = nodes, clients, []
        self.lock, self.done, self.thread = threading.Lock(), threading.Event(), None
        self.phase = "idle"
        self.error = None

    def capture(self):
        processes = {}
        for name, owner in [(node.name, node) for node in self.nodes] + list(self.clients.items()):
            process = owner.process
            if process is not None and process.poll() is None:
                processes[process.pid] = name
        rows = process_stats(processes)
        with self.lock:
            self.samples.append({"at": time.monotonic(), "phase": self.phase, "processes": list(rows.values())})

    def start(self):
        self.capture()

        def collect():
            try:
                while not self.done.wait(RSS_PAUSE):
                    self.capture()
            except Exception as error:
                self.error = error
        self.thread = threading.Thread(target=collect, daemon=True)
        self.thread.start()

    def set_phase(self, phase):
        self.capture()
        with self.lock:
            self.phase = phase

    def stop(self):
        self.done.set()
        if self.thread:
            self.thread.join(timeout=6)
            require(not self.thread.is_alive(), "Task process sampler stops")
        self.capture()
        if self.error:
            raise self.error

    def result(self):
        roles = {node.name for node in self.nodes}
        idle = {role: [] for role in roles}
        peaks, cpu = {}, {}
        for sample in self.samples:
            for row in sample["processes"]:
                role = row["role"]
                peaks[role] = max(peaks.get(role, 0), row["rss_bytes"])
                if sample["phase"] == "idle" and role in idle:
                    idle[role].append(row["rss_bytes"])
                cpu.setdefault(row["pid"], {"role": role, "first": row["cpu_seconds"], "last": row["cpu_seconds"]})["last"] = row["cpu_seconds"]
        require(all(idle.values()), "Every Node has actual steady-state RSS samples")
        return {"sample_interval_seconds": RSS_PAUSE,
                "idle_rss_bytes": {role: statistics.median(values) for role, values in idle.items()},
                "sampled_peak_rss_bytes": peaks,
                "observed_cpu_seconds": [{"role": row["role"], "pid": pid, "seconds": row["last"] - row["first"]}
                                         for pid, row in cpu.items()],
                "cpu_scope": "first-to-last observed counter per PID; short intervals and restart startup can be under-sampled",
                "samples": self.samples}


def helpers():
    spec = importlib.util.spec_from_file_location("free_network_verification", ROOT / "scripts/verify-node.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def message_at(connection, network, conversation, message_id):
    rows = connection.execute("SELECT envelope FROM events WHERE network_id=?", (network,)).fetchall()
    for row in rows:
        event = json.loads(json.loads(row[0])["canonical_body"])
        body = event["body"]
        if event["scope_id"] == conversation and body.get("type") == "message_posted" and body.get("message_id") == message_id:
            return body["text"]
    return None


def wait_observed(predicate, timeout):
    deadline, attempts, intervals, previous = time.monotonic() + timeout, 0, [], time.monotonic()
    while time.monotonic() < deadline:
        value = predicate()
        attempts += 1
        now = time.monotonic()
        intervals.append(now - previous)
        previous = now
        if value:
            return value, {"checks": attempts, "requested_pause_seconds": OBSERVE_PAUSE,
                           "maximum_observed_interval_seconds": max(intervals)}
        time.sleep(OBSERVE_PAUSE)
    raise AssertionError("Timed out waiting for actual target data")


def restart(node, network, verify):
    node.stop()
    node.start()
    verify.wait_for("the restarted fixture is writable", lambda: verify.node_status(node).get("write_state") == "writable", timeout=60)
    node.client.connect()
    node.client.authenticate(network)


def run_sample(artifact, iteration, run, payload, digest, verify):
    directory = run / f"{iteration:02d}-{artifact.side}"
    directory.mkdir(mode=0o700)
    workspace = directory / "workspace"
    workspace.mkdir(mode=0o700)
    helper = directory / "client.mjs"
    helper.write_text(verify.CLIENT_SOURCE.replace("MODULE_URL", artifact.module.as_uri()))
    helper.chmod(0o600)
    nodes, clients, meter = [], {}, None
    result = {"side": artifact.side, "iteration": iteration, "status": "running", "errors": 0,
              "artifact": artifact.public(), "load_before": os.getloadavg()}
    try:
        relay_port = verify.available_port()
        relay_url = f"http://127.0.0.1:{relay_port}"
        for name in ("A", "P", "B"):
            node = verify.Node(directory, name, artifact.binary, workspace, helper)
            verify.isolate_delivery_node(node)
            nodes.append(node)
        a, p, b = nodes
        p.configure_network(relay_listen=f"127.0.0.1:{relay_port}")
        a.configure_network(relays=[relay_url])
        b.configure_network(relays=[relay_url], relay_paths_only=True)
        a.start()
        network = a.bootstrap()
        p.pair(a)
        p.start()
        at_p = verify.Client(p.url, helper, identity_file=a.client.identity_file)
        clients["client_p"] = at_p
        at_p.connect(p.device_id)
        verify.wait_for("the management client is known at P", lambda: at_p.authenticate(network))
        b.pair(at_p)
        b.start()
        at_p.close()
        target = verify.Client(b.url, helper, identity_file=a.client.identity_file)
        clients["client_b"] = target
        target.connect(b.device_id)
        verify.wait_for("the management client is known at B", lambda: target.authenticate(network))
        conversation = "free-perf-conversation"
        a.client.rpc("create_conversation", conversation_id=conversation, title="Free performance fixture")
        verify.wait_for("all three members have the same initial history", lambda: verify.converged(nodes, network), timeout=60)
        target.rpc("conversation", conversation_id=conversation)
        clients["client_a"] = a.client
        meter = Meter(nodes, clients)
        meter.start()
        time.sleep(IDLE_SECONDS)
        meter.set_phase("message")
        loader_before = resource.getrusage(resource.RUSAGE_SELF)
        text = "m" * MESSAGE_BYTES
        message_digest = hashlib.sha256(text.encode()).hexdigest()
        with sqlite3.connect((b.directory / "node.sqlite3").as_uri() + "?mode=ro", uri=True) as connection:
            started = time.monotonic()
            a.client.rpc("message", conversation_id=conversation, message_id="free-perf-4k", text=text, report_results=False)
            received, resolution = wait_observed(lambda: message_at(connection, network, conversation, "free-perf-4k"), 60)
            require(len(received.encode()) == MESSAGE_BYTES and hashlib.sha256(received.encode()).hexdigest() == message_digest,
                    "The target committed the exact 4096-byte private message")
            shown = next(row for row in target.rpc("conversation", conversation_id=conversation)["messages"]
                         if row["message_id"] == "free-perf-4k")
            require(shown["text"] == text, "The target API reads back the same complete private message")
            elapsed = time.monotonic() - started
        result["message"] = {"bytes": MESSAGE_BYTES, "sha256": message_digest, "seconds": elapsed,
                             "goodput_bytes_per_second": MESSAGE_BYTES / elapsed, "observer": resolution}
        meter.set_phase("attachment")
        upload = Coverage(BLOB_BYTES)
        offset, restarted_at = 0, None
        started = upload_started = time.monotonic()
        with payload.open("rb") as source:
            while offset < BLOB_BYTES:
                source.seek(offset)
                chunk = source.read(CHUNK_BYTES)
                upload.add(offset, len(chunk))
                reply = a.client.rpc("upload_attachment", conversation_id=conversation, hash=digest, size=BLOB_BYTES,
                                     offset=offset, data=base64.b64encode(chunk).decode())
                if reply["complete"]:
                    offset = BLOB_BYTES
                    break
                next_offset = reply["offset"]
                require(type(next_offset) is int and offset < next_offset <= BLOB_BYTES, "Upload acknowledges a forward durable offset")
                offset = next_offset
                if restarted_at is None and offset >= 24 * CHUNK_BYTES:
                    restart(a, network, verify)
                    restarted_at, offset = offset, 0
        upload_elapsed = time.monotonic() - upload_started
        require(upload.result()["unique_offset_bytes"] == BLOB_BYTES and restarted_at is not None,
                "The upload covers the full original and crosses a real sender restart")
        b.stop()
        a.client.rpc("message", conversation_id=conversation, message_id="free-perf-attachment", text="Attachment",
                     attachments=[{"hash": digest, "size": BLOB_BYTES, "name": "free-perf.bin"}], report_results=False)
        partial = b.directory / "blobs/partial" / (digest + ".part")
        b.start()
        wait_observed(lambda: partial.exists() and partial.stat().st_size >= 4 * CHUNK_BYTES
                      and verify.committed_blob_offset(b, network, digest) > 0, 60)
        meter.capture()
        b.stop(crash=True)
        kept = verify.committed_blob_offset(b, network, digest)
        partial_size = partial.stat().st_size
        require(0 < kept < BLOB_BYTES and kept <= partial_size <= BLOB_BYTES,
                "The receiver was interrupted with a nonempty incomplete durable prefix")
        b.start()
        stored = b.directory / "blobs/sha256" / digest[:2] / digest
        _, download_resolution = wait_observed(lambda: stored.exists() and stored.stat().st_size == BLOB_BYTES, 120)
        require(sha256_file(stored) == digest, "The receiver's completed 64MiB file has the original digest")
        target.connect(b.device_id)
        target.authenticate(network)
        browser_read = Coverage(BLOB_BYTES)
        actual_digest, offset = hashlib.sha256(), 0
        read_started = time.monotonic()
        while offset is not None:
            require(time.monotonic() - read_started <= 120, "The complete browser file read stays within its fixture deadline")
            page = target.rpc("read_artifact", conversation_id=conversation, blob_hash=digest, offset=offset, binary=True)
            chunk = base64.b64decode(page["data"], validate=True)
            require(chunk, "Each browser read returns original bytes")
            browser_read.add(offset, len(chunk))
            actual_digest.update(chunk)
            next_offset = page["next_offset"]
            require(next_offset is None or next_offset == offset + len(chunk), "Browser pages preserve the exact original byte order")
            offset = next_offset
        elapsed = time.monotonic() - started
        require(browser_read.result()["unique_offset_bytes"] == BLOB_BYTES and actual_digest.hexdigest() == digest,
                "The browser receives the full original 64MiB attachment after both restarts")
        result["attachment"] = {"bytes": BLOB_BYTES, "sha256": digest, "seconds": elapsed,
            "goodput_bytes_per_second": BLOB_BYTES / elapsed, "upload_seconds": upload_elapsed,
            "browser_read_seconds": time.monotonic() - read_started, "upload": upload.result(),
            "browser_read": browser_read.result(), "upload_restart_durable_offset": restarted_at,
            "receiver_restart_durable_offset": kept, "receiver_uncommitted_tail_bytes": partial_size - kept,
            "receiver_completion_observer": download_resolution,
            "repeated_bytes_scope": "observed upload_attachment payload and read_artifact page offsets only",
            "quic_wire_repeated_bytes": None}
        loader_after = resource.getrusage(resource.RUSAGE_SELF)
        result["loader_cpu_seconds"] = ((loader_after.ru_utime + loader_after.ru_stime)
                                         - (loader_before.ru_utime + loader_before.ru_stime))
        require(not any((node.directory / "delivery-wallet.json").exists() for node in nodes),
                "The free comparison creates no delivery wallet")
        artifact.verify()
        result["status"] = "passed"
    except Exception as error:
        result.update(status="failed", errors=1, failure={"type": type(error).__name__, "message": str(error)})
        raise
    finally:
        cleanup_errors = []
        if meter:
            try:
                meter.stop()
                result["resources"] = meter.result()
            except Exception as error:
                cleanup_errors.append(type(error).__name__)
        for client in clients.values():
            try:
                client.close()
            except Exception as error:
                cleanup_errors.append(type(error).__name__)
        for node in reversed(nodes):
            try:
                node.client.close()
            except Exception as error:
                cleanup_errors.append(type(error).__name__)
            try:
                node.stop()
            except Exception as error:
                cleanup_errors.append(type(error).__name__)
        if cleanup_errors:
            result.update(status="failed", errors=result["errors"] + len(cleanup_errors), cleanup_errors=cleanup_errors)
        result["load_after"] = os.getloadavg()
        verify.private_json(directory / "sample.json", result)
        require(not cleanup_errors, "All task processes and resource collection must finish successfully")
    return result


def distribution(values):
    require(values and all(math.isfinite(value) and value > 0 for value in values), "Only completed positive measurements are summarized")
    return {"count": len(values), "median": statistics.median(values), "minimum": min(values), "maximum": max(values)}


def summarize(samples):
    require(len(samples) == SAMPLES_PER_SIDE * 2 and all(row["status"] == "passed" and row["errors"] == 0 for row in samples),
            "All five alternating comparisons must complete successfully")
    summary = {"status": "passed", "metrics": {}, "idle_rss_bytes": {}, "observability": {
        "duplicate_bytes": "application request/page interval overlap; not QUIC or physical network duplicates",
        "rss": "ps snapshots; sampled peaks are lower bounds on instantaneous peaks",
        "cpu": "ps counters for only the task's Node and WASM client PIDs, plus Python RUSAGE_SELF"}}
    grouped = {side: [row for row in samples if row["side"] == side] for side in ("baseline", "head")}
    require(all(len(rows) == SAMPLES_PER_SIDE for rows in grouped.values()), "Each pinned artifact has five successful samples")
    for name in ("message", "attachment"):
        baseline, head = (distribution([row[name]["seconds"] for row in grouped[side]]) for side in ("baseline", "head"))
        ratio = head["median"] / baseline["median"]
        resolution = max(row["message"]["observer"]["maximum_observed_interval_seconds"] for row in samples) if name == "message" else 0
        verdict = "passed" if ratio <= 1.2 else "failed"
        if name == "message" and resolution > baseline["median"] * 0.2:
            verdict = "inconclusive"
        if verdict != "passed":
            summary["status"] = "failed" if verdict == "failed" else "inconclusive" if summary["status"] == "passed" else summary["status"]
        spread = max(baseline["maximum"] - baseline["minimum"], head["maximum"] - head["minimum"])
        summary["metrics"][name] = {"baseline_seconds": baseline, "head_seconds": head,
            "median_ratio": ratio, "maximum_allowed_ratio": 1.2, "status": verdict,
            "observer_resolution_seconds": resolution,
            "comparison": "no measurable difference" if abs(head["median"] - baseline["median"]) <= spread + resolution else "measured difference"}
    for side, rows in grouped.items():
        roles = rows[0]["resources"]["idle_rss_bytes"]
        medians = {role: statistics.median(row["resources"]["idle_rss_bytes"][role] for row in rows) for role in roles}
        summary["idle_rss_bytes"][side] = {"roles": medians, "total": sum(medians.values())}
    extra = summary["idle_rss_bytes"]["head"]["total"] - summary["idle_rss_bytes"]["baseline"]["total"]
    summary["idle_rss_extra_bytes"] = extra
    summary["idle_rss_maximum_extra_bytes"] = 64 * 1024 * 1024
    if extra > summary["idle_rss_maximum_extra_bytes"]:
        summary["status"] = "failed"
    return summary


def self_test():
    observed = Coverage(32)
    for offset, length in ((0, 10), (5, 10), (20, 5), (10, 20), (32, 0)):
        observed.add(offset, length)
    require(observed.result() == {"payload_bytes": 45, "unique_offset_bytes": 30, "repeated_offset_bytes": 15},
            "Overlapping and adjacent request ranges have exact repeated-byte counts")
    for offset, length in ((-1, 1), (0, -1), (31, 2), (33, 0), (False, 1)):
        try:
            observed.add(offset, length)
        except AssertionError:
            continue
        raise AssertionError("Invalid payload ranges must fail")
    require(cpu_seconds("02:03.50") == 123.5 and cpu_seconds("1-02:03:04.50") == 93784.5,
            "Linux and macOS CPU counters use actual seconds")
    require(cpu_seconds("00:00:01") == 1, "Short Linux CPU counters parse")
    require(distribution([3, 1, 2, 5, 4]) == {"count": 5, "median": 3, "minimum": 1, "maximum": 5},
            "The summary reports all five samples and their spread")
    fixture = []
    for iteration in range(SAMPLES_PER_SIDE):
        for side in ("baseline", "head"):
            fixture.append({"side": side, "status": "passed", "errors": 0,
                "message": {"seconds": 1, "observer": {"maximum_observed_interval_seconds": 0.001}},
                "attachment": {"seconds": 10}, "resources": {"idle_rss_bytes": {"A": 100, "P": 100, "B": 100}}})
    require(summarize(fixture)["status"] == "passed", "Completed equal work passes measured thresholds")
    fixture[1]["message"]["observer"]["maximum_observed_interval_seconds"] = 0.3
    require(summarize(fixture)["status"] == "inconclusive", "A coarse observer cannot claim the 20 percent bound")
    fixture[1]["status"] = "failed"
    try:
        summarize(fixture)
    except AssertionError:
        pass
    else:
        raise AssertionError("Rejected or failed work cannot become a fast sample")
    require(process_stats({os.getpid(): "self-test"})[os.getpid()]["rss_bytes"] > 0,
            "The task process has a real OS RSS counter")
    print("PASS free network performance parser, range, threshold and OS counter checks; no Node or benchmark ran")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-spec", type=Path)
    parser.add_argument("--head-spec", type=Path)
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    require(args.baseline_spec and args.head_spec and args.artifacts, "Provide both pinned package specifications and a task artifact directory")
    artifacts = args.artifacts.resolve()
    require("trash" not in str(artifacts).lower() and artifacts.is_relative_to(ROOT / ".artifacts"),
            "Performance artifacts stay inside this worktree's ignored .artifacts directory")
    subprocess.run(["git", "check-ignore", "--quiet", str(artifacts)], cwd=ROOT, check=True)
    artifacts.mkdir(parents=True, exist_ok=True, mode=0o700)
    run = artifacts / ("free-network-" + uuid.uuid4().hex)
    run.mkdir(mode=0o700)
    sides = [Artifact.read("baseline", args.baseline_spec), Artifact.read("head", args.head_spec)]
    require(sides[0].source_commit != sides[1].source_commit and sides[0].build_info["target"] == sides[1].build_info["target"],
            "Compare distinct source commits built for the same production target")
    verify = helpers()
    require(all(side.build_info["data_epoch"] == verify.DATA_EPOCH for side in sides), "The shared controlled fixture understands both data epochs")
    payload = run / "attachment.bin"
    with payload.open("xb") as output:
        for index in range(BLOB_BYTES // CHUNK_BYTES):
            output.write(hashlib.shake_256(b"vibeface-free-network-perf-v1" + index.to_bytes(4, "big")).digest(CHUNK_BYTES))
    digest = sha256_file(payload)
    report = {"schema_version": 1, "status": "running", "samples": [], "errors": 0,
        "claim": "same private relay workload: median message and resumable attachment latency at most 20 percent higher; extra idle RSS at most 64MiB",
        "machine": {"platform": platform.platform(), "logical_cores": os.cpu_count(), "load": os.getloadavg(), "python": platform.python_version()},
        "artifacts": [side.public() for side in sides], "message_bytes": MESSAGE_BYTES, "attachment_bytes": BLOB_BYTES,
        "attachment_sha256": digest, "fixture_sha256": sha256_file(ROOT / "scripts/verify-node.py"),
        "driver_sha256": sha256_file(Path(__file__).resolve()),
        "scene": "three fresh member Nodes, B relay_paths_only through member P, execution and manager disabled; warm connections; sender and receiver restarts during attachment",
        "not_measured": ["QUIC wire duplicate bytes", "real NAT or public internet", "AI providers", "paid delivery performance"]}
    try:
        for iteration in range(1, SAMPLES_PER_SIDE + 1):
            for side in sides:
                print(f"RUN free network {iteration} {side.side}", flush=True)
                sample = run_sample(side, iteration, run, payload, digest, verify)
                report["samples"].append(sample)
                verify.private_json(run / "report.json", report)
        report["summary"] = summarize(report["samples"])
        report["status"] = report["summary"]["status"]
    except Exception as error:
        report.update(status="failed", errors=1, failure={"type": type(error).__name__, "message": str(error)})
    finally:
        verify.private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)
    return 0 if report["status"] == "passed" else 2 if report["status"] == "inconclusive" else 1


if __name__ == "__main__":
    os.umask(0o077)
    try:
        raise SystemExit(main())
    except Exception as error:
        print("FAIL free network benchmark: " + type(error).__name__ + ": " + str(error), file=sys.stderr)
        raise SystemExit(1)
