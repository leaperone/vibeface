#!/usr/bin/env python3
"""Run local Node process acceptance without making paid provider calls.

The default run uses the installed Codex app-server only for native thread
creation and reads. Delayed execution, restart and notification checks use an
explicit stream-json fixture, never a model. All state and logs stay beneath
--artifacts with private permissions. The script prints no credentials.
"""

import argparse
import copy
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


def require(condition, description):
    if not condition:
        raise AssertionError(description)


def wait_for(description, predicate, timeout=35):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, ValueError, AssertionError, http.client.HTTPException) as error:
            last_error = error
        time.sleep(0.15)
    suffix = f" ({type(last_error).__name__})" if last_error else ""
    raise AssertionError(f"timed out waiting for {description}{suffix}")


def private_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")
    path.chmod(0o600)


def available_port():
    with socket.socket() as stream:
        stream.bind(("127.0.0.1", 0))
        return stream.getsockname()[1]


class Client:
    def __init__(self, url):
        self.url = url
        self.cookie = None
        self.csrf = None
        self.account = None

    def request(self, path, payload=None, *, status=200, csrf=True, origin=True,
                authenticated=True, headers=None, timeout=20):
        request_headers = dict(headers or {})
        if origin:
            request_headers.setdefault("Origin", self.url)
        if authenticated and self.cookie:
            request_headers["Cookie"] = self.cookie
        if csrf and self.csrf:
            request_headers["X-CSRF-Token"] = self.csrf
        body = None
        if payload is not None:
            body = json.dumps(payload).encode()
            request_headers["Content-Type"] = "application/json"
        request = urllib.request.Request(self.url + path, body, request_headers)
        try:
            response = urllib.request.urlopen(request, timeout=timeout)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            observed = response.status
            encoded = response.read(4 * 1024 * 1024 + 1)
            require(len(encoded) <= 4 * 1024 * 1024, "HTTP response is bounded")
            require(observed == status,
                    f"{path}: expected HTTP {status}, received {observed}")
            value = json.loads(encoded) if encoded else None
            cookie = response.headers.get("Set-Cookie")
            if cookie and observed == 200 and path == "/api/login":
                require("HttpOnly" in cookie and "SameSite=Strict" in cookie,
                        "login cookie has HttpOnly and SameSite")
                self.cookie = cookie.split(";", 1)[0]
                self.csrf = value["csrf"]
                self.account = value["account"]
            return value

    def login(self, username, password):
        return self.request("/api/login", {"username": username, "password": password})

    def rpc(self, method, *, status=200, **arguments):
        return self.request("/api/rpc", {"method": method, **arguments}, status=status)


class Node:
    def __init__(self, run, name, binary, workspace, fixture):
        self.name = name
        self.binary = binary
        self.directory = run / name
        self.port = available_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.client = Client(self.url)
        self.process = None
        self.log = None
        result = subprocess.run(
            [str(binary), "init", "--data-dir", str(self.directory),
             "--listen", f"127.0.0.1:{self.port}", "--root", str(workspace)],
            capture_output=True, timeout=20)
        require(result.returncode == 0, f"{name}: CLI initialization succeeds")
        self.configure(claude_bin=str(fixture), timeout_secs=60)

    def configure(self, **values):
        path = self.directory / "config.json"
        configuration = json.loads(path.read_text())
        configuration.update(values)
        private_json(path, configuration)

    def start(self):
        require(self.process is None, f"{self.name}: process is stopped before start")
        self.log = (self.directory / "process.log").open("ab")
        self.process = subprocess.Popen(
            [str(self.binary), "serve", "--data-dir", str(self.directory)],
            stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
            start_new_session=True)
        info = wait_for(f"{self.name} HTTP listener", lambda: self.client.request("/api/info"))
        self.node_id = info["node_id"]
        require(self.process.poll() is None, f"{self.name}: independent process remains running")

    def stop(self):
        if self.process is not None:
            # Stop only this script's process group, including its private app-server.
            try:
                os.killpg(self.process.pid, signal.SIGTERM)
                self.process.wait(timeout=5)
            except ProcessLookupError:
                pass
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=5)
            self.process = None
            self.log.close()
            self.log = None

    def pair(self, owner, network, username, password):
        invitation = owner.client.rpc("invite", roles=["gateway", "relay", "replica", "scheduler", "executor"])
        result = subprocess.run(
            [str(self.binary), "pair", "--data-dir", str(self.directory),
             "--url", owner.url, "--network-id", network, "--code", invitation["code"],
             "--username", username, "--password-stdin"],
            input=(password + "\n").encode(), capture_output=True, timeout=40)
        require(result.returncode == 0, f"{self.name}: pinned CLI pairing succeeds")


class Events:
    def __init__(self, client):
        self.client = client
        self.lines = []
        self.ready = threading.Event()
        self.connection = None
        self.thread = threading.Thread(target=self.read, daemon=True)

    def read(self):
        parsed = urllib.parse.urlparse(self.client.url)
        self.connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=3)
        try:
            self.connection.request("GET", "/api/events", headers={"Cookie": self.client.cookie})
            response = self.connection.getresponse()
            if response.status != 200:
                return
            self.ready.set()
            while True:
                line = response.readline()
                if not line:
                    return
                if line.startswith(b"data:"):
                    self.lines.append(json.loads(line[5:].strip()))
        except (OSError, ValueError, http.client.HTTPException):
            pass

    def start(self):
        self.thread.start()
        require(self.ready.wait(8), "authenticated SSE subscription opens")

    def close(self):
        if self.connection:
            self.connection.close()
        self.thread.join(timeout=4)


def fixture_executor(arguments):
    """Implement only the stream-json CLI boundary used by fault acceptance."""
    run = Path(arguments[arguments.index("--fixture-root") + 1])
    native = arguments[arguments.index("--resume") + 1] if "--resume" in arguments else arguments[arguments.index("--session-id") + 1]
    request = json.loads(sys.stdin.read())
    invocation = {"native_session_id": native, "label": request["label"], "pid": os.getpid()}
    with (run / "fixture-invocations.jsonl").open("a") as output:
        output.write(json.dumps(invocation) + "\n")
        output.flush()
    print(json.dumps({"type": "system", "subtype": "init", "session_id": native}), flush=True)
    gate = request.get("gate")
    if gate:
        while not Path(gate).exists():
            time.sleep(0.05)
    result = "x" * request.get("result_size", 0) or "fixture terminal result"
    print(json.dumps({"type": "result", "subtype": "success", "is_error": False,
                      "session_id": native, "result": result}), flush=True)


def invocations(run, label=None):
    path = run / "fixture-invocations.jsonl"
    records = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
    return [record for record in records if label is None or record["label"] == label]


def operation(client, node, operation_id):
    return client.rpc("operation", target_node=node.node_id, operation_id=operation_id)["operation"]


def request(network, conversation, node, operation_id, session_id, action, revision=0, task=None):
    return {"operation_id": operation_id, "network_id": network,
            "conversation_id": conversation, "task_id": task,
            "target_node": node.node_id, "session_id": session_id,
            "expected_revision": revision, "basis_heads": [], "action": action,
            "resources": []}


def dispatch(client, value, status=200):
    return client.rpc("operate", request=value, status=status)


def succeeded(client, node, operation_id):
    record = operation(client, node, operation_id)
    require(record is not None, "accepted operation is queryable")
    return record if record["state"] == "succeeded" else None


def verify_snapshot(snapshot):
    events = {event["hash"]: event for event in snapshot["events"]}
    for event_hash, signed in events.items():
        require(hashlib.sha256(signed["canonical_body"].encode()).hexdigest() == event_hash,
                "exported event content hash matches")
        event = json.loads(signed["canonical_body"])
        require(event["network_id"] == snapshot["network_id"], "snapshot remains in its network")
        dependencies = event["parents"] + ([event["previous"]] if event["previous"] else [])
        require(all(dependency in events for dependency in dependencies), "export includes complete event closure")
    require(all(head in events for head in snapshot["heads"]), "snapshot contains advertised heads")
    return len(events)


def verify_blobs(node, network):
    connection = sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True)
    try:
        rows = connection.execute("SELECT hash,size,content FROM blobs WHERE network_id=?", (network,)).fetchall()
        for digest, size, content in rows:
            require(size == len(content) and hashlib.sha256(content).hexdigest() == digest,
                    "replicated blob length and digest match")
        return len(rows)
    finally:
        connection.close()


def acceptance(options):
    binary = options.binary.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("node-acceptance-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    child_workspace = workspace / "child"
    child_workspace.mkdir()
    fixture = run / "fixture-executor"
    fixture.write_text(f"#!{sys.executable}\nimport os\nos.execv({sys.executable!r}, [{sys.executable!r}, {str(Path(__file__).resolve())!r}, '--fixture-executor', '--fixture-root', {str(run)!r}] + __import__('sys').argv[1:])\n")
    fixture.chmod(0o700)
    password_a, password_b, password_c = [secrets.token_urlsafe(24) for _ in range(3)]
    nodes = []
    report = {"status": "running", "evidence": [], "provider_calls": 0,
              "not_checked": ["paid model output and SDK reasoning", "real LAN/public ingress",
                              "rendered browser UI", "hostile DAG import rejection (covered by store tests)"]}
    stream = None

    def passed(description, kind="real_node_processes"):
        report["evidence"].append({"check": description, "kind": kind})
        print("PASS " + description, flush=True)

    try:
        for name in ("A", "B", "C"):
            nodes.append(Node(run, name, binary, workspace, fixture))
        a, b, c = nodes
        for node in nodes:
            node.configure(codex_bin=options.codex_bin)
        a.start()
        a.client.request("/api/rpc", {"method": "nodes"}, authenticated=False, status=401)
        a.client.request("/api/register", {"username": "owner_a", "password": password_a}, status=201)
        a.client.login("owner_a", password_a)
        network = a.client.account["network_id"]
        a.client.request("/api/rpc", {"method": "nodes"}, csrf=False, status=403)
        a.client.request("/api/rpc", {"method": "nodes"}, origin=False, status=403)
        a.client.request("/api/rpc", {"method": "nodes"}, headers={"Origin": "https://hostile.invalid"}, status=403)
        a.client.request("/api/tools", {"name": "nodes", "arguments": {}, "call_id": "human-call"}, status=401)
        a.client.request("/api/pair", {"code": "invalid", "node_id": "unknown", "public_key": "00", "proof": "00"}, status=400)
        passed("login, CSRF, Origin and scoped-tool boundaries")

        b.pair(a, network, "owner_b", password_b)
        c.pair(a, network, "owner_c", password_c)
        c.configure(peers=[{"network_id": network, "url": b.url.replace("http:", "ws:") + "/peer"}])
        b.start()
        c.start()
        b.client.login("owner_b", password_b)
        c.client.login("owner_c", password_c)
        wait_for("A routes to C through B", lambda: any(n["node_id"] == c.node_id and n["online"] for n in a.client.rpc("nodes")["nodes"]))
        require(len({a.node_id, b.node_id, c.node_id}) == 3, "nodes have distinct stable identities")
        require(c.client.account["network_id"] == b.client.account["network_id"] == network, "all local accounts use paired network")
        passed("three independent nodes pair and route over A-B-C WebSockets")

        other = Client(a.url)
        other_password = secrets.token_urlsafe(24)
        other.request("/api/register", {"username": "separate", "password": other_password}, status=201)
        other.login("separate", other_password)
        require(other.account["network_id"] != network, "separate account gets separate network")
        conversation, task = "acceptance-conversation", "acceptance-task"
        a.client.rpc("create_conversation", conversation_id=conversation, title="Failover acceptance")
        a.client.rpc("create_task", conversation_id=conversation, task_id=task, title="Explicit acceptance", goal="Keep history across node changes", acceptance="User accepts explicitly")
        a.client.rpc("message", conversation_id=conversation, message_id="message-original", text="Keep this context after A disconnects.")
        a.client.rpc("message", conversation_id=conversation, message_id="message-original", text="Keep this context after A disconnects.")
        a.client.rpc("message", conversation_id=conversation, message_id="message-original", text="changed content", status=400)
        other.rpc("conversation", conversation_id=conversation, status=400)
        require(other.rpc("conversations")["conversations"] == [], "other network cannot list conversation")
        foreign = request(other.account["network_id"], conversation, b, "foreign", "foreign", {"type": "open", "executor": "claude_code", "cwd": str(workspace)})
        dispatch(a.client, foreign, status=400)
        history = wait_for("shared message and task at C", lambda: shared_history(c.client, conversation, task))
        require(len([m for m in history["messages"] if m["message_id"] == "message-original"]) == 1, "message retry is idempotent")
        passed("shared conversation sync and hosted-account network isolation")

        if not options.skip_codex:
            require(shutil.which(options.codex_bin) is not None or Path(options.codex_bin).is_file(), "Codex executable is available; use --skip-codex for fixture-only run")
            native_open = request(network, conversation, b, "codex-native-open", "native-codex", {"type": "open", "executor": "codex", "cwd": str(workspace)}, task=task)
            dispatch(a.client, native_open)
            wait_for("real Codex native thread open", lambda: succeeded(a.client, b, "codex-native-open"))
            native = a.client.rpc("session", target_node=b.node_id, session_id="native-codex")
            require(native["session"]["native_session_id"], "real app-server native thread ID is persisted")
            require("error" not in native["actual"], "real app-server thread is readable")
            passed("installed Codex app-server opens and reads a native thread without a model turn", "real_codex_transport")
        else:
            report["not_checked"].append("real Codex native transport (--skip-codex)")

        open_request = request(network, conversation, b, "fixture-open", "fixture-main", {"type": "open", "executor": "claude_code", "cwd": str(workspace)}, task=task)
        dispatch(a.client, open_request)
        wait_for("fixture session ready", lambda: succeeded(a.client, b, "fixture-open"))
        dispatch(a.client, open_request)
        changed = copy.deepcopy(open_request)
        changed["action"]["cwd"] = str(child_workspace)
        dispatch(a.client, changed, status=400)
        bad_path = request(network, conversation, b, "invalid-root", "invalid-root", {"type": "open", "executor": "claude_code", "cwd": str(run)})
        dispatch(a.client, bad_path, status=400)
        missing_head = request(network, conversation, b, "missing-basis", "missing-basis", {"type": "open", "executor": "claude_code", "cwd": str(workspace)})
        missing_head["basis_heads"] = ["00" * 32]
        dispatch(a.client, missing_head, status=400)
        passed("execution admission rejects changed IDs, invalid roots and missing basis")

        session = a.client.rpc("session", target_node=b.node_id, session_id="fixture-main")["session"]
        gate = run / "complete-first"
        send = request(network, conversation, b, "fixture-delayed-send", "fixture-main", {"type": "send", "text": json.dumps({"label": "delayed", "gate": str(gate), "result_size": 280000})}, revision=session["revision"], task=task)
        dispatch(a.client, send)
        wait_for("fixture invocation begins", lambda: len(invocations(run, "delayed")) == 1)
        dispatch(c.client, send)
        stale = copy.deepcopy(send)
        stale["operation_id"] = "stale-concurrent"
        dispatch(c.client, stale, status=400)
        overlap = request(network, conversation, b, "overlapping-path", "overlapping-session", {"type": "open", "executor": "claude_code", "cwd": str(child_workspace)})
        dispatch(c.client, overlap, status=400)
        stream = Events(c.client)
        stream.start()
        wait_for("accepted dispatch and message at C", lambda: shared_history(c.client, conversation, task))
        a.stop()
        gate.touch(mode=0o600)
        wait_for("B finishes while A is offline", lambda: succeeded(c.client, b, "fixture-delayed-send"))
        notification = "result:fixture-delayed-send"
        wait_for("C receives durable B result notification", lambda: any(n["notification_id"] == notification for n in c.client.rpc("notifications", client_id="phone")["notifications"]))
        wait_for("C receives live event stream update", lambda: len(stream.lines) > 0)
        after = c.client.rpc("conversation", conversation_id=conversation)
        require(after["tasks"][0]["status"] != "accepted", "turn completion never accepts a task")
        require(any(s["session_id"] == "fixture-main" for s in after["sessions"]), "C reads actual B execution state after A exits")
        require(len(invocations(run, "delayed")) == 1, "retries and imported history do not repeat execution")
        wait_for("large result blob replicated to C", lambda: verify_blobs(c, network) >= 1)
        count = verify_snapshot(c.client.rpc("export", conversation_id=conversation))
        require(count > 5, "export contains conversation, dispatch and result history")
        passed("A goes offline; C restores context, queries B and receives live/durable completion", "real_nodes_with_stream_json_fixture")
        passed("session CAS, overlapping-directory exclusion and execution deduplication", "real_nodes_with_stream_json_fixture")
        passed("large attachment copies with matching content hash and complete DAG closure")
        stream.close()
        stream = None

        c.client.rpc("ack", notification_id=notification, client_id="phone")
        require(all(n["notification_id"] != notification for n in c.client.rpc("notifications", client_id="phone")["notifications"]), "ACK removes notification for same client")
        require(any(n["notification_id"] == notification for n in c.client.rpc("notifications", client_id="laptop")["notifications"]), "ACK does not consume another client delivery")
        session = c.client.rpc("session", target_node=b.node_id, session_id="fixture-main")["session"]
        offline_gate = run / "complete-offline"
        offline = request(network, conversation, b, "fixture-offline-send", "fixture-main", {"type": "send", "text": json.dumps({"label": "offline", "gate": str(offline_gate)})}, revision=session["revision"], task=task)
        dispatch(c.client, offline)
        wait_for("second fixture invocation", lambda: len(invocations(run, "offline")) == 1)
        c.stop()
        offline_gate.touch(mode=0o600)
        wait_for("B completes without any connected client", lambda: succeeded(b.client, b, "fixture-offline-send"))
        c.start()
        c.client.login("owner_c", password_c)
        wait_for("persistent notification delivered after reconnect", lambda: any(n["notification_id"] == "result:fixture-offline-send" for n in c.client.rpc("notifications", client_id="phone")["notifications"]))
        require(all(n["notification_id"] != notification for n in c.client.rpc("notifications", client_id="phone")["notifications"]), "client ACK remains durable after restart")
        passed("offline notifications replay after reconnect and per-client ACK persists", "real_nodes_with_stream_json_fixture")

        session = c.client.rpc("session", target_node=b.node_id, session_id="fixture-main")["session"]
        unknown = request(network, conversation, b, "fixture-unknown-send", "fixture-main", {"type": "send", "text": json.dumps({"label": "unknown", "gate": str(run / "never-complete")})}, revision=session["revision"], task=task)
        dispatch(c.client, unknown)
        wait_for("interrupted fixture is executing", lambda: len(invocations(run, "unknown")) == 1)
        b.stop()
        b.start()
        b.client.login("owner_b", password_b)
        record = wait_for("running operation recovers as Unknown", lambda: unknown_record(b.client, b, unknown["operation_id"]))
        require(record["state"] == "unknown", "restart does not report success")
        dispatch(b.client, unknown)
        require(len(invocations(run, "unknown")) == 1, "Unknown retry does not invoke executor")
        blocked = copy.deepcopy(unknown)
        blocked["operation_id"] = "unknown-resource-conflict"
        dispatch(b.client, blocked, status=400)
        b.client.rpc("reconcile", target_node=b.node_id, operation_id=unknown["operation_id"])
        require(operation(b.client, b, unknown["operation_id"])["state"] == "unknown", "inconclusive reconciliation keeps Unknown")
        b.client.rpc("resolve_unknown", target_node=b.node_id, operation_id=unknown["operation_id"], outcome="failed", evidence="Acceptance stopped its entire private process group and verified one invocation.")
        passed("execution interruption preserves Unknown and reserves resources until human resolution", "real_nodes_with_stream_json_fixture")

        a.start()
        a.client.login("owner_a", password_a)
        wait_for("owner reconnects to B", lambda: any(n["node_id"] == b.node_id and n["online"] for n in a.client.rpc("nodes")["nodes"]))
        a.client.rpc("revoke", node_id=c.node_id)
        wait_for("revoked node receives updated trust", lambda: revoked(c.client))
        a.client.rpc("operation", target_node=c.node_id, operation_id="anything", status=400)
        require(len(invocations(run)) == 3, "sync/restart/revocation never creates extra executor calls")
        passed("revocation rejects further data or execution access")
        report["status"] = "passed"
        report["fixture_invocations"] = len(invocations(run))
        report["event_count"] = count
        print(f"PASS acceptance report: {run / 'report.json'}", flush=True)
    except Exception as error:
        report["status"] = "failed"
        report["failure"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        if stream:
            stream.close()
        for node in reversed(nodes):
            node.stop()
        private_json(run / "report.json", report)


def shared_history(client, conversation, task):
    result = client.rpc("conversation", conversation_id=conversation)
    return result if (any(m["message_id"] == "message-original" for m in result["messages"])
                      and any(t["task_id"] == task for t in result["tasks"])
                      and not any(result["missing"].values())) else None


def unknown_record(client, node, operation_id):
    record = operation(client, node, operation_id)
    return record if record and record["state"] == "unknown" else None


def revoked(client):
    client.rpc("nodes", status=400)
    return True


if __name__ == "__main__":
    os.umask(0o077)
    if "--fixture-executor" in sys.argv:
        fixture_executor(sys.argv[1:])
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--binary", type=Path, required=True)
        parser.add_argument("--artifacts", type=Path, required=True)
        parser.add_argument("--codex-bin", default="codex")
        parser.add_argument("--skip-codex", action="store_true", help="omit real Codex transport checks and report that omission")
        arguments = parser.parse_args()
        try:
            acceptance(arguments)
        except Exception as error:
            print(f"FAIL {type(error).__name__}: {error}", file=sys.stderr)
            sys.exit(1)
