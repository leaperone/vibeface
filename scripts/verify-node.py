#!/usr/bin/env python3
"""Run local Node process acceptance without making paid provider calls.

The default run uses the installed Codex app-server only for native thread
creation and reads. Delayed execution, restart and notification checks use an
explicit stream-json fixture, never a model. All state and logs stay beneath
--artifacts with private permissions. The script prints no credentials.
--phase1 checks durable Manager queues, scoped tools and cross-entry progress
using a controlled sidecar. It does not verify real provider model behavior.
--chat-agent checks taskless tools, sandboxed commands and startup event recovery
with real Node processes and controlled provider protocol fixtures.
"""

import argparse
import base64
import copy
import hashlib
import http.client
import json
import os
import re
from html.parser import HTMLParser
from pathlib import Path
import shutil
import signal
import select
import shlex
import socket
import struct
import sqlite3
import subprocess
import sys
import tempfile
import time
import traceback
import threading
import urllib.error
import urllib.parse
import urllib.request
import uuid


DATA_FORMAT = {"format": "vibeface-node", "data_epoch": 3, "layout_version": 1}


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
    suffix = (f" ({last_error})" if isinstance(last_error, TimeoutError)
              else f" ({type(last_error).__name__})" if last_error else "")
    raise AssertionError(f"timed out waiting for {description}{suffix}")


def private_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")
    path.chmod(0o600)


def file_sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def data_directory_snapshot(directory):
    snapshot = {}
    for path in (directory, *sorted(directory.rglob("*"))):
        content = (os.readlink(path) if path.is_symlink()
                   else file_sha256(path) if path.is_file() else None)
        snapshot[str(path.relative_to(directory))] = (path.lstat().st_mode, content)
    return snapshot


def sqlite_files_snapshot(directory):
    snapshot = {}
    for name in ("node.sqlite3", "node.sqlite3-wal", "node.sqlite3-shm", "node.sqlite3-journal"):
        path = directory / name
        snapshot[name] = None
        if path.exists():
            require(path.is_file() and not path.is_symlink(), "SQLite artifact is a regular file")
            metadata = path.stat()
            snapshot[name] = {"mode": metadata.st_mode, "size": metadata.st_size, "sha256": file_sha256(path)}
    return snapshot


def verify_current_data_format(directory, *, stopped=True):
    require(json.loads((directory / "data-epoch.json").read_text()) == DATA_FORMAT,
            "fresh node declares exactly the current format, data epoch and layout")
    require(sorted(path.name for path in directory.glob("*.sqlite3")) == ["node.sqlite3"],
            "identity and business state use one SQLite database")
    temporary = tempfile.TemporaryDirectory(prefix="sqlite-format-", dir=directory.parent) if stopped else None
    original = sqlite_files_snapshot(directory) if stopped else None
    database = directory / "node.sqlite3"
    if stopped:
        copied = Path(temporary.name)
        for name, metadata in original.items():
            if metadata is not None:
                shutil.copy2(directory / name, copied / name)
        require(sqlite_files_snapshot(directory) == original, "copying stopped SQLite files preserves all source files")
        database = copied / "node.sqlite3"
    try:
        mode = "rw" if stopped else "ro"
        with sqlite3.connect(f"file:{database}?mode={mode}", uri=True) as connection:
            connection.execute("PRAGMA query_only=ON")
            verify_database_format(connection)
    finally:
        if temporary is not None:
            temporary.cleanup()
            require(sqlite_files_snapshot(directory) == original, "offline format inspection preserves main, WAL, SHM and journal")


def verify_database_format(connection):
    metadata = connection.execute("SELECT format_id,data_epoch,layout_version,initialization_state,schema_fingerprint FROM schema_metadata").fetchall()
    require(len(metadata) == 1 and metadata[0][:4] == ("vibeface-node", 3, 1, "ready"),
            "database initialization is ready and matches the current directory format")
    require(re.fullmatch(r"[a-f0-9]{64}", metadata[0][4]) is not None,
            "database declares its initialized schema fingerprint")
    for table in ("events", "conversations"):
        columns = {row[1]: row for row in connection.execute(f"PRAGMA table_info({table})")}
        resource = columns.get("resource_id")
        require(resource is not None and resource[3] == 1 and resource[4] is None,
                f"{table} requires an explicit resource without a compatibility default")
        references = connection.execute(f"PRAGMA foreign_key_list({table})").fetchall()
        candidates = {row[0] for row in references
                      if row[2:5] == ("resource_records", "resource_id", "resource_id")}
        require(any({(row[3], row[4]) for row in references if row[0] == key}
                    == {("space_id", "space_id"), ("resource_id", "resource_id")}
                    for key in candidates),
                f"{table} ties its space and resource to one signed resource catalog row")
    require(connection.execute("PRAGMA foreign_key_check").fetchall() == [],
            "current database has no broken foreign keys")


def maintenance(node, command):
    stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stream.settimeout(5)
    try:
        wait_for("private maintenance listener",
                 lambda: stream.connect(f"/tmp/vibeface-{node.node_id[:32]}/maintenance.sock") is None,
                 timeout=5)
        stream.sendall(json.dumps({"protocol": 1, "command": command}).encode() + b"\n")
        response = b""
        while not response.endswith(b"\n"):
            part = stream.recv(1024)
            require(part and len(response) < 8192, "bounded maintenance response")
            response += part
        value = json.loads(response)
        require(value["protocol"] == 1, "maintenance protocol matches")
        return stream, value
    except BaseException:
        stream.close()
        raise


def verify_cli_status(node, running):
    result = subprocess.run([str(node.binary), "status", "--json", "--data-dir", str(node.directory)],
                            capture_output=True, text=True, timeout=10)
    require((result.returncode == 0) == running, "CLI status exit code reflects node reachability")
    status = json.loads(result.stdout)
    require(set(status) == {"status", "node_id", "data_dir", "listen", "roles", "version", "cli_version", "error"},
            "CLI status returns only public node health and local paths")
    require(status["status"] == ("running" if running else "stopped") and status["node_id"] == node.node_id,
            "CLI status pins the initialized local node in both running and stopped states")
    require(node.access["capability"] not in result.stdout, "CLI status never exposes bootstrap capability")


def verify_live_access(node, operation_id):
    before = operation(node.client, node, operation_id)
    require(before["state"] == "running", "access probe begins with a real running execution")
    result = subprocess.run([str(node.binary), "access", "--json", "--data-dir", str(node.directory)],
                            capture_output=True, text=True, timeout=10)
    require(result.returncode == 0, "access CLI reads the live node without taking its directory lock")
    access = json.loads(result.stdout)
    require(access["url"] == node.url and access["node_id"] == node.node_id
            and access["node_public_key"] == node.node_public_key
            and access["capability"] is None and access["claim_nonce"] is None,
            "initialized access returns the pinned entry and no bootstrap claim secrets")
    require(operation(node.client, node, operation_id) == before,
            "access CLI leaves the running operation and its deduplication record unchanged")


def verify_maintenance(node):
    stream, value = maintenance(node, "acquire")
    try:
        require(value["status"] == "ready", "idle node enters maintenance")
        require(value["health"]["node_id"] == node.node_id, "lease pins the running node")
        node.client.rpc("nodes", status=503)
        probe, health = maintenance(node, "probe")
        probe.close()
        require(health["status"] == "healthy", "health is readable while writes are closed")
        require(node.client.request("/api/info")["node_id"] == node.node_id, "HTTP reads survive maintenance")
    finally:
        stream.close()
    wait_for("lease disconnect reopens node", lambda: node.client.rpc("nodes"))
    node.stop()
    marker = node.directory / "update-pending.json"
    private_json(marker, {"protocol": 1})
    node.start()
    opened = node.client.command("connect", url=node.url, node_id=node.node_id,
                                 node_public_key=node.node_public_key, check=False)
    if opened["ok"]:
        denied = node.client.command("request", method="rpc",
                                     params={"method": "nodes"}, check=False)
        require(not denied["ok"] or (denied["result"].get("error") and not denied["result"].get("result")),
                "pending startup cannot admit business RPC")
    commit, value = maintenance(node, "commit")
    commit.close()
    require(value["status"] == "error", "pending marker prevents premature commit")
    marker.unlink()
    commit, value = maintenance(node, "commit")
    commit.close()
    require(value["status"] == "committed", "verified startup opens only after explicit commit")
    node.client.connect()
    node.client.authenticate()
    node.client.rpc("nodes")
    result = subprocess.run([str(node.binary), "update", "status", "--data-dir", str(node.directory)],
                            capture_output=True, timeout=10)
    require(result.returncode == 0, "update status works without a managed installation")
    result = subprocess.run([str(node.binary), "update", "apply", "--automatic", "--data-dir", str(node.directory)],
                            capture_output=True, timeout=10)
    require(result.returncode == 0, "unconfigured automatic updates do nothing")


def available_port():
    with socket.socket() as stream:
        stream.bind(("127.0.0.1", 0))
        return stream.getsockname()[1]


def verify_direct_start(run, binary):
    directory = run / "direct-start"
    provider_free = run / "provider-free-bin"
    provider_free.mkdir()
    environment = {**os.environ, "PATH": str(provider_free)}
    missing = run / "explicit-serve-missing"
    result = subprocess.run([str(binary), "serve", "--data-dir", str(missing)],
                            env=environment, capture_output=True, timeout=10)
    require(result.returncode != 0 and not missing.exists(),
            "explicit serve requires initialized data and does not create an identity")
    configuration = None
    protected = None
    log_path = run / "direct-start.log"
    for attempt in range(3):
        with log_path.open("ab") as log:
            process = subprocess.Popen([str(binary), "--data-dir", str(directory)],
                                       env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            try:
                if attempt == 0:
                    wait_for("direct startup initialization", lambda: (directory / "data-epoch.json").exists())
                    configuration = json.loads((directory / "config.json").read_text())
                    require(configuration["listen"] == "127.0.0.1:4790"
                            and configuration["roots"] == [] and configuration["manager"] is None
                            and not configuration["allow_insecure_lan"]
                            and configuration["network"] == {"bootstrap_urls": [], "advertise_urls": [],
                                                              "mdns": False, "relay_enabled": False},
                            "direct first startup grants no execution roots, manager or remote ingress")
                else:
                    url = configuration["public_url"]

                    def own_listener():
                        with urllib.request.urlopen(url + "/api/info", timeout=1) as response:
                            return json.load(response)

                    info = wait_for("direct startup listener", own_listener)
                    require(process.poll() is None, "direct startup remains running")
                    status = subprocess.run([str(binary), "status", "--json", "--data-dir", str(directory)],
                                            env=environment, capture_output=True, timeout=10)
                    require(status.returncode == 0 and json.loads(status.stdout)["node_id"] == info["node_id"],
                            "direct startup serves the same identity on HTTP and its private socket")
                    with urllib.request.urlopen(url, timeout=5) as response:
                        require(response.status == 200 and b"<html" in response.read(),
                                "direct startup serves the embedded web client")
            finally:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        if attempt == 0:
            configuration["listen"] = f"127.0.0.1:{available_port()}"
            configuration["public_url"] = "http://" + configuration["listen"]
            configuration["local_access"]["listen"] = f"127.0.0.1:{available_port()}"
            private_json(directory / "config.json", configuration)
            protected = {path: file_sha256(directory / path)
                         for path in ("identity.json", "config.json", "bootstrap.json", "data-epoch.json")}
        else:
            require(all(file_sha256(directory / path) == digest for path, digest in protected.items()),
                    "direct startup and restart preserve the exact identity, config, bootstrap and epoch")
    capability = json.loads((directory / "bootstrap.json").read_text())["capability"]
    require(capability.encode() not in log_path.read_bytes(), "startup never prints the bootstrap capability")
    verify_current_data_format(directory)


class RelayCapture:
    def __init__(self, upstream_port):
        self.port = available_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.upstream_port = upstream_port
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", self.port))
        self.listener.listen(32)
        self.listener.settimeout(0.2)
        self.stopped = threading.Event()
        self.connections = []
        self.payloads = []
        self.errors = []
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        while not self.stopped.is_set():
            try:
                client, _ = self.listener.accept()
                server = socket.create_connection(("127.0.0.1", self.upstream_port), timeout=5)
                client.settimeout(None)
                server.settimeout(None)
            except socket.timeout:
                continue
            except OSError:
                if not self.stopped.is_set():
                    self.errors.append("relay proxy failed to accept")
                return
            self.connections.extend([client, server])
            for source, target in ((client, server), (server, client)):
                threading.Thread(target=self.forward, args=(source, target), daemon=True).start()

    def forward(self, source, target):
        buffer = bytearray()
        headers = True
        try:
            while not self.stopped.is_set():
                part = source.recv(65536)
                if not part:
                    return
                target.sendall(part)
                buffer.extend(part)
                if headers:
                    end = buffer.find(b"\r\n\r\n")
                    if end < 0:
                        require(len(buffer) < 16384, "bounded relay upgrade headers")
                        continue
                    del buffer[:end + 4]
                    headers = False
                while len(buffer) >= 2:
                    masked = bool(buffer[1] & 128)
                    length = buffer[1] & 127
                    offset = 2
                    if length in (126, 127):
                        width = 2 if length == 126 else 8
                        if len(buffer) < offset + width:
                            break
                        length = int.from_bytes(buffer[offset:offset + width], "big")
                        offset += width
                    require(length <= 16 * 1024 * 1024, "bounded captured WebSocket frame")
                    key = buffer[offset:offset + 4] if masked else None
                    offset += 4 if masked else 0
                    if len(buffer) < offset + length:
                        break
                    payload = bytes(buffer[offset:offset + length])
                    if masked:
                        payload = bytes(value ^ key[index % 4] for index, value in enumerate(payload))
                    self.payloads.append(payload)
                    del buffer[:offset + length]
        except (OSError, AssertionError) as error:
            if isinstance(error, AssertionError):
                self.errors.append(str(error))
        finally:
            for stream in (source, target):
                try:
                    stream.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def close(self):
        self.stopped.set()
        self.listener.close()
        for stream in self.connections:
            stream.close()
        self.thread.join(timeout=2)


class Client:
    def __init__(self, url, helper, module):
        self.url = url
        self.helper = helper
        self.module = module
        self.scope = None
        self.policy = None
        self.connection_policies = {}
        self.connection_scopes = {}
        self.connection_resources = {}
        self.connection_authentication = {}
        self.gateways = {}
        self.resource_id = None
        self.process = None
        self.node_id = None
        self.node_public_key = None
        self.host_public_key = None
        self.audience = None
        self.tasks = {}
        self.bindings = {}
        self.target_public_keys = {}
        self.operations = {}
        self.signed_requests = {}
        self.execution_sources = {}
        self.business_entry = None
        self.manager_fixture_run = None
        self.manager_originals = {}

    def command(self, op, *, check=True, **arguments):
        if op in {"connect", "close", "authenticate"}:
            connection = arguments.get("connection", "main")
            self.connection_policies.pop(connection, None)
            if op in {"connect", "close"}:
                self.gateways.pop(connection, None)
        if self.process is None:
            self.process = subprocess.Popen(
                ["node", str(self.helper), str(self.module)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.process.stdin.write(json.dumps({"op": op, **arguments}) + "\n")
        self.process.stdin.flush()
        require(select.select([self.process.stdout], [], [], 30)[0], "WASM client command completed before timeout")
        line = self.process.stdout.readline()
        require(line, "WASM client returned a bounded JSON response")
        value = json.loads(line)
        if not check:
            return value
        require(value.get("ok"), f"client {op} failed: {value.get('error', 'unknown error')}")
        return value.get("result")

    def connect(self, node_id=None, *, node_public_key=None, relay=False, connection="main"):
        if self.business_entry and node_id is None and node_public_key is None and connection == "main":
            return self.command("connect", **self.business_entry, relay=relay, connection=connection)
        self.node_id = node_id or self.node_id
        self.node_public_key = node_public_key or self.node_public_key
        return self.command("connect", url=self.url, node_id=self.node_id,
                            node_public_key=self.node_public_key, relay=relay, connection=connection)

    def call(self, method, params=None, *, status=200, connection="main", invocation=None):
        if method in {"identity", "authenticate"}:
            require(len(json.dumps(params or {}, separators=(",", ":"), ensure_ascii=False).encode()) <= 65536,
                    "identity request stays within 64 KiB")
        response = self.command("request", method=method, params=params or {},
                                invocation=invocation, connection=connection)
        if method in {"identity", "authenticate"}:
            require(sum(len(json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode())
                        for value in (params or {}, response)) <= 65536,
                    "complete identity carrier stays within 64 KiB")
        error = response.get("error")
        if status == 200:
            require(not error, f"encrypted {method} rejected: {error}")
            return response.get("result")
        require(bool(error), f"encrypted {method} expected rejection")
        if isinstance(error, dict) and error.get("status"):
            require(error["status"] == status, f"encrypted {method} rejection status")
        return response

    def identity_call(self, method, params=None, *, status=200, connection="main"):
        return self.call("identity", {"method": method, "params": params or {}},
                         status=status, connection=connection)

    def authenticate(self, *, resource_id=None, audience=None, host_public_key=None, connection="main",
                     trust_ref="primary", resource_kind="project", grant_leaf=None, activate=True):
        require(connection not in self.gateways, "Gateway authentication must refresh the separate B source")
        selected_resource = resource_id or self.resource_id
        expected_host = host_public_key or self.host_public_key or self.node_public_key
        selected_audience = audience or (self.audience if expected_host == self.host_public_key else None)
        verified = self.command("authenticate", resource_id=selected_resource, audience=selected_audience,
                                expected_host_public_key=expected_host, connection=connection,
                                trust_ref=trust_ref, resource_kind=resource_kind, grant_leaf=grant_leaf)
        expected_scope = self.command("get_trust", trust_ref=trust_ref)["scope"]
        require(verified["scope"] == expected_scope, "client authenticates the pinned account, space and device")
        policy = verified["policy"]
        require(policy["audience_public_key"] == expected_host,
                "verified authorization targets the independently pinned resource host")
        self.target_public_keys[hashlib.sha256(bytes.fromhex(expected_host)).hexdigest()] = expected_host
        resources = policy["resources"]
        require(resources and all(item["payload"]["kind"] == resource_kind for item in resources),
                "signed authorization exposes the selected resource kind")
        selected_resource = selected_resource or resources[0]["payload"]["resource"]
        require(selected_resource in {item["payload"]["resource"] for item in resources},
                "selected resource belongs to the verified projection")
        self.connection_policies[connection] = policy
        self.connection_scopes[connection] = verified["scope"]
        self.connection_resources[connection] = selected_resource
        self.connection_authentication[connection] = {"resource_id": selected_resource, "audience": selected_audience,
            "host_public_key": expected_host, "trust_ref": trust_ref, "resource_kind": resource_kind,
            "grant_leaf": grant_leaf, "activate": activate}
        if activate:
            self.scope, self.policy = verified["scope"], policy
            self.host_public_key, self.audience, self.resource_id = expected_host, policy["audience"], selected_resource
        return verified

    def attach_gateway(self, card, *, source_connection="main", connection="project-gateway", refresh=True):
        require(connection != source_connection and source_connection not in self.gateways,
                "Gateway route retains a separate direct B policy connection")
        require(source_connection in self.connection_authentication,
                "Gateway route requires a confirmed B authorization")
        if refresh:
            self.authenticate(connection=source_connection, **self.connection_authentication[source_connection])
        endpoint = card["gateway"]
        self.command("connect", connection=connection, **endpoint)
        self.gateways[connection] = {"card": copy.deepcopy(card), "source_connection": source_connection}
        attached = self.command("attach_gateway", connection=connection, source_connection=source_connection, card=card)
        require(attached["scope"] == self.connection_scopes[source_connection]
                and attached["policy"] == self.connection_policies[source_connection],
                "Gateway uses the exact confirmed B policy and scope")
        self.connection_scopes[connection] = copy.deepcopy(attached["scope"])
        self.connection_policies[connection] = copy.deepcopy(attached["policy"])
        self.connection_resources[connection] = self.connection_resources[source_connection]
        self.gateways[connection] = {"card": copy.deepcopy(card), "source_connection": source_connection,
                                    "approval_hash": attached["gateway"]["approval_hash"]}
        return attached

    def rpc(self, method, *, status=200, connection="main", remote=None, **arguments):
        scope = self.scope if connection == "main" else self.connection_scopes.get(connection, self.scope)
        body = {"method": method, "space_id": scope["space_id"],
                "resource_id": self.resource_id if connection == "main" else self.connection_resources.get(connection, self.resource_id),
                **arguments}
        connection_policy = self.connection_policies.get(connection)
        if connection_policy is None or body["resource_id"] not in {
                item["payload"]["resource"] for item in connection_policy["resources"]}:
            require(connection not in self.gateways, "Gateway has no matching B authorization; refresh and attach its direct source")
            default_resource = self.resource_id
            self.authenticate(resource_id=body["resource_id"], connection=connection)
            self.resource_id = default_resource
        body.setdefault("authorization_id", body["request"]["operation_id"]
                        if method == "operate" else uuid.uuid4().hex)
        original = None
        if method in {"operation", "reconcile", "resolve_unknown"}:
            original = self.operations.get((body["space_id"], body["resource_id"], body["operation_id"]))
        if method == "session":
            matching = [request for (space, resource, _), request in self.operations.items()
                        if (space, resource, request["target_node"], request["session_id"])
                        == (body["space_id"], body["resource_id"], body["target_node"], body["session_id"])]
            require(matching and all((request["conversation_id"], request["task_id"], request["binding"])
                                     == (matching[0]["conversation_id"], matching[0]["task_id"], matching[0]["binding"])
                                     for request in matching),
                    "session read uses independently saved matching task and binding intent")
            original = matching[-1] if remote else matching[0]
            if remote:
                body.setdefault("operation_id", original["operation_id"])
        if original:
            for field in ("conversation_id", "task_id", "target_node", "session_id", "binding"):
                body.setdefault(field, copy.deepcopy(original[field]))
        request = body["request"] if method == "operate" else body
        conversation = request.get("conversation_id")
        task_id = request.get("task_id")
        task = (self.tasks.get((body["space_id"], body["resource_id"], conversation, task_id))
                if task_id and method != "create_task" else None)
        require(not task_id or method == "create_task" or task is not None,
                "task context comes from an independently verified TaskCreated")
        if task:
            body.setdefault("task_ref", task["task_ref"])
        context = {"conversation": conversation, "task": task["task_ref"] if task else None,
                   "sdk_session": None, "execution_session": None, "executor_kind": None,
                   "workspace_resource": body["resource_id"]}
        raw_context = {"context": context, "binding_generation": None, "canonical_cwd": None,
                       "execution_target_public_key": None}
        if method == "update_task":
            body.setdefault("detail", "")
        if method == "message":
            body.setdefault("scheduler_node", hashlib.sha256(bytes.fromhex(self.host_public_key)).hexdigest())
            body.setdefault("report_results", False)
        if method in {"operate", "bind_execution", "rebind_execution"} or original:
            target = request["target_node"]
            target_key = self.target_public_keys.get(target)
            require(target_key is not None, "execution target has an independently verified public key")
            binding = request.get("binding")
            executor = (request.get("action", {}).get("executor") or body.get("executor")
                        or (binding and binding["binding"]["open_spec"]["executor"]))
            require(executor is not None, "execution kind comes from the intended open or verified binding")
            context["execution_session"] = {"target": self.command("derive_authorization_id", kind="device", value=target_key),
                                            "session_id": request["session_id"]}
            context["executor_kind"] = executor
            raw_context["binding_generation"] = (body["binding_generation"] if method == "bind_execution"
                                                  else binding["binding"]["generation"] if binding else None)
            raw_context["execution_target_public_key"] = target_key
            if binding:
                raw_context["canonical_cwd"] = binding["binding"]["open_spec"]["cwd"]
            if method in {"bind_execution", "rebind_execution"}:
                raw_context["canonical_cwd"] = body["cwd"]
            elif method == "operate" and request["action"]["type"] == "open":
                raw_context["canonical_cwd"] = request["action"]["cwd"]
        if remote:
            remote_metadata = method in {"operation", "session", "reconcile"}
            if method in {"bind_execution", "rebind_execution"}:
                raw_context["binding_generation"] = remote["generation"]
            remote_context = copy.deepcopy(raw_context)
            if remote_metadata:
                source_proof = self.execution_sources.get((body["space_id"], body["resource_id"], body["operation_id"]))
                require(source_proof is not None, "remote metadata uses the original verified Project source")
                body["execution_source"] = copy.deepcopy(source_proof)
                raw_context = self.command("verify_project_metadata", source=source_proof,
                                           binding=original["binding"], connection=connection)
            if not remote_metadata:
                body.update(remote["share"]["selection"])
            if method == "operate" and request["action"]["type"] != "open":
                body["execution_session_receipt"] = remote["receipt"]
            handle = "remote-" + uuid.uuid4().hex
            self.command("read_remote_policy", handle=handle, connection=connection,
                         source_connection=remote["source_connection"],
                         execution_connection=remote["execution_connection"],
                         execution_trust_ref=remote["trust_ref"], selection=remote["share"]["selection"],
                         action={"bind_execution": "execution_open", "rebind_execution": "execution_rebind"}.get(method)
                                or ({"open": "execution_open", "send": "execution_send"}.get(request.get("action", {}).get("type"))
                                    if method == "operate" else "operation_status_read"),
                         raw_context=remote_context, approve=remote.get("approve", False))
            remote["approve"] = False
        signing = {"raw_context": raw_context}
        if remote and not remote_metadata:
            signing["remote_handle"] = handle
        try:
            proof = self.command("prepare_signed", body=body, connection=connection, **signing)
            if "error" in proof:
                response = proof
            else:
                saved = self.signed_requests.setdefault(body["authorization_id"], copy.deepcopy(proof))
                require(connection not in self.gateways or saved == proof,
                        "one Gateway authorization ID retains its original signed request")
                if method == "operate":
                    self.operations[(body["space_id"], body["resource_id"], body["request"]["operation_id"])] = copy.deepcopy(body["request"])
                response = self.command("request", method="rpc", params=proof["body"],
                                        invocation=proof["invocation"], connection=connection)
            result = response.get("result", {})
            if remote and method == "operate" and result.get("execution_source"):
                source_proof = result["execution_source"]
                self.command("verify_project_metadata", source=source_proof, binding=request["binding"], connection=connection)
                require(source_proof["payload"]["operation_id"] == request["operation_id"],
                        "signed Project source identifies this exact submitted operation")
                source_key = (body["space_id"], body["resource_id"], request["operation_id"])
                previous = self.execution_sources.setdefault(source_key, copy.deepcopy(source_proof))
                require(previous == source_proof, "the original signed Project source never changes")
            if remote and result.get("actual"):
                actual = result["actual"]
                original_proof = self.signed_requests[actual["receipt"]["payload"]["operation_id"]]
                self.command("verify_remote_result", handle=handle, actual=actual,
                             original=original_proof, binding=original["binding"] if original else request["binding"])
            if remote and method == "session" and result.get("receipt"):
                result["session"] = self.command("verify_remote_session", handle=handle,
                                                 receipt=result["receipt"], binding=original["binding"])
                remote["receipt"] = result["receipt"]
        finally:
            if remote:
                self.command("release_remote", handle=handle)
        error = response.get("error")
        require(not error if status == 200 else bool(error),
                f"encrypted {method}: expected {'success' if status == 200 else 'rejection'}, received {error}")
        if status == 200 and method == "operate":
            self.operations[(body["space_id"], body["resource_id"], body["request"]["operation_id"])] = copy.deepcopy(body["request"])
        return response.get("result") if status == 200 else response

    def add_project_fixture(self, authority):
        resource_id = "res2_" + os.urandom(32).hex()
        self.governance("resource", {"space": self.scope["space_id"], "resource": resource_id,
                                     "kind": "project", "host_node_public_key": authority.node_public_key})
        default_resource = self.resource_id
        self.authenticate(resource_id=resource_id)
        require(resource_id in {item["payload"]["resource"] for item in self.policy["resources"]},
                "root-signed Project is visible only after its signed space policy is installed")
        self.authenticate(resource_id=default_resource)
        return resource_id

    def governance(self, kind, intent, *, connection="main"):
        handle = "governance-" + uuid.uuid4().hex
        prepared = self.command("prepare_governance", handle=handle, kind=kind, intent=intent, connection=connection)
        committed = self.identity_call("space_policy_commit", {"request": prepared["request"]}, connection=connection)
        return self.command("finish_preparation", handle=handle, receipt=committed["committed"])

    def create_task(self, authority, conversation, task_id, *, title, goal, acceptance):
        intended = {"type": "task_created", "task_id": task_id, "title": title,
                    "goal": goal, "acceptance": acceptance}
        response = self.rpc("create_task", conversation_id=conversation, task_id=task_id,
                            title=title, goal=goal, acceptance=acceptance)
        envelope = response["creation"]
        require(response["authority_public_key"] == authority.node_public_key
                and response["task"]["task_creation"] == envelope,
                "task response matches the independently pinned author and signed creation")
        verified = self.command("verify_event", envelope=envelope,
                                space_id=self.scope["space_id"], resource_id=self.resource_id,
                                author_public_key=authority.node_public_key)
        require(verified["body"]["scope_id"] == conversation and verified["body"]["body"] == intended,
                "signed task creation matches the independently specified conversation and task intent")
        self.tasks[(self.scope["space_id"], self.resource_id, conversation, task_id)] = {
            "task_ref": verified["hash"],
            "authority_node": authority.node_id, "authority_public_key": authority.node_public_key}
        return response

    def bind_execution(self, conversation, task_id, target, session_id, open_spec, *, remote=None, connection="main"):
        task = self.tasks[(self.scope["space_id"], self.resource_id, conversation, task_id)]
        self.target_public_keys[target.node_id] = target.node_public_key
        require(open_spec["type"] == "open" and Path(open_spec["cwd"]).resolve() == Path(open_spec["cwd"]),
                "binding intent uses an explicit canonical project directory")
        response = self.rpc("bind_execution", conversation_id=conversation, task_id=task_id,
                            task_ref=task["task_ref"],
                            target_node=target.node_id, session_id=session_id,
                            executor=open_spec["executor"], cwd=open_spec["cwd"], binding_generation=1,
                            remote=remote, connection=connection)
        expected = {"space_id": self.scope["space_id"], "conversation_id": conversation,
                    "task_ref": task["task_ref"], "task_id": task_id,
                    "authority_node": task["authority_node"], "generation": 1,
                    "target_node": target.node_id, "session_id": session_id,
                    "open_spec": open_spec, "previous_binding_hash": None, "previous_seal": None}
        binding = self.command("verify_task_binding", binding=response["binding"],
                               authority_public_key=task["authority_public_key"],
                               expected={"binding": expected})
        self.bindings[(self.scope["space_id"], self.resource_id, conversation, task_id, session_id)] = binding
        return binding

    def subscribe(self):
        body = {"method": "conversations", "space_id": self.scope["space_id"],
                "resource_id": self.resource_id, "authorization_id": uuid.uuid4().hex}
        response = self.command("signed_request", method="subscribe", body=body)
        require(not response.get("error") and response.get("result") == {"subscribed": True},
                "subscription is signed for the selected account, space and resource")

    def request(self, path, payload=None, *, status=200, origin=True, headers=None, timeout=20, **unused):
        request_headers = dict(headers or {})
        if origin:
            request_headers.setdefault("Origin", self.url)
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
            require(observed == status, f"{path}: expected HTTP {status}, received {observed}")
            return json.loads(encoded) if encoded and "application/json" in response.headers.get("Content-Type", "") else encoded or None

    def close(self):
        if self.process is not None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
            self.process = None


class Node:
    def __init__(self, run, name, binary, workspace, fixture, helper, module):
        self.name = name
        self.binary = binary
        self.workspace = workspace.resolve()
        self.directory = run / name
        self.port = available_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.client = Client(self.url, helper, module)
        self.process = None
        self.log = None
        initialized_at = time.monotonic()
        try:
            result = subprocess.run(
                [str(binary), "init", "--data-dir", str(self.directory),
                 "--listen", f"127.0.0.1:{self.port}", "--root", str(workspace)],
                env={**os.environ, "RUST_BACKTRACE": "1"}, capture_output=True, timeout=45)
        except subprocess.TimeoutExpired as error:
            (run / f"{name}-init.log").write_bytes((error.stdout or b"") + (error.stderr or b""))
            raise RuntimeError(f"{name}: CLI initialization timed out") from None
        self.init_seconds = time.monotonic() - initialized_at
        (run / f"{name}-init.log").write_bytes(result.stdout + result.stderr)
        require(result.returncode == 0, f"{name}: CLI initialization succeeds")
        verify_current_data_format(self.directory)
        self.configure(claude_bin=str(fixture), timeout_secs=60)
        result = subprocess.run([str(binary), "access", "--json", "--data-dir", str(self.directory)],
                                capture_output=True, timeout=15)
        require(result.returncode == 0, f"{name}: local access material is available")
        self.access = json.loads(result.stdout)
        require(self.access["node_id"], "local access pins the Node identity")
        self.node_id = self.access["node_id"]
        self.node_public_key = self.access["node_public_key"]
        require(hashlib.sha256(bytes.fromhex(self.node_public_key)).hexdigest() == self.node_id,
                "trusted local access pins the exact Node public key")
        require(isinstance(self.access["claim_nonce"], str)
                and re.fullmatch(r"[a-f0-9]{64}", self.access["claim_nonce"]) is not None
                and self.access["claim_nonce"] == self.access["capability"],
                "fresh local access binds the installation capability to the Node claim nonce")
        self.client.node_id = self.node_id
        self.client.node_public_key = self.node_public_key

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
        require(self.node_id == info["node_id"], "HTTP listener serves the independently pinned local Node")
        require(self.process.poll() is None, f"{self.name}: independent process remains running")

    def stop(self):
        if self.process is not None:
            # Stop only this script's process group, including its private app-server.
            try:
                if self.process.poll() is None:
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

    def bootstrap(self):
        self.client.connect(self.node_id)
        self.client.identity_call("node_claim", {"capability": "invalid-capability"}, status=400)
        claimed = self.client.identity_call("node_claim", {"capability": self.access["capability"]})
        prepared = self.client.command("prepare_creation", claim=claimed["claim"],
                                       node_public_key=self.node_public_key,
                                       claim_nonce=self.access["claim_nonce"])
        possession = self.client.identity_call("node_possession", prepared)
        require(self.client.identity_call("node_possession", prepared) == possession,
                "repeated Node possession request returns the persisted proof")
        self.backup_password = "acceptance-only-encrypted-recovery-passphrase"
        created = self.client.command("finish_creation", proof=possession["proof"],
                                       password=self.backup_password, endpoint=self.url)
        self.backup = created["recovery_envelope"]
        backup_path = self.directory / "account-recovery.json"
        backup_path.write_bytes(bytes.fromhex(self.backup))
        backup_path.chmod(0o600)
        self.client.scope = created["scope"]
        payload = {"pins": created["pins"], "authorization": created["authorization"]}
        invalid = copy.deepcopy(payload)
        invalid["authorization"]["journal"][0]["value"]["signature"] = "00" * 64
        self.client.identity_call("bootstrap", invalid, status=400)
        require(not self.client.identity_call("status")["initialized"],
                "invalid account creation leaves the Node unclaimed")
        result = self.client.identity_call("bootstrap", payload)
        require(result["scope"] == self.client.scope, "bootstrap installs the locally constructed account scope")
        require(self.client.identity_call("bootstrap", payload) == result,
                "identical bootstrap request is idempotent after capability consumption")
        self.client.authenticate()
        return self.client.scope["space_id"]


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
            print(json.dumps({"type": "stream_event", "session_id": native,
                              "event": {"type": "content_block_delta", "index": 0, "delta": {
                                  "type": "text_delta", "text": request["label"] + ":working"}}}), flush=True)
            time.sleep(0.2)
    result = "x" * request.get("result_size", 0) or "fixture terminal result"
    print(json.dumps({"type": "result", "subtype": "success", "is_error": False,
                      "session_id": native, "result": result}), flush=True)


def invocations(run, label=None):
    path = run / "fixture-invocations.jsonl"
    records = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
    return [record for record in records if label is None or record["label"] == label]


def fixture_wrapper(run, name, mode):
    path = run / name
    path.write_text(f"#!{sys.executable}\nimport os\nos.execv({sys.executable!r}, "
                    f"[{sys.executable!r}, {str(Path(__file__).resolve())!r}, "
                    f"{mode!r}, '--fixture-root', {str(run)!r}] + __import__('sys').argv[1:])\n")
    path.chmod(0o700)
    return path


def fixture_manager_settings():
    return {"provider": "codex_sdk", "model": "controlled-fixture"}


def fixture_record(run, filename, record):
    encoded = (json.dumps(record) + "\n").encode()
    descriptor = os.open(run / filename, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    try:
        os.write(descriptor, encoded)
    finally:
        os.close(descriptor)


def fixture_records(run, filename="manager-invocations.jsonl"):
    path = run / filename
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def stop_fixture_processes(run):
    records = fixture_records(run) + fixture_records(run, "fixture-invocations.jsonl") + fixture_records(run, "codex-rpc.jsonl")
    for pid in {record["pid"] for record in records}:
        result = subprocess.run(["ps", "-p", str(pid), "-o", "command="], capture_output=True, text=True)
        if str(run) in result.stdout and "--fixture-" in result.stdout:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def websocket_receive(stream):
    def exact(length):
        value = b""
        while len(value) < length:
            block = stream.recv(length - len(value))
            if not block:
                raise EOFError("fixture WebSocket closed")
            value += block
        return value

    first, second = exact(2)
    length = second & 127
    if length == 126:
        length = struct.unpack("!H", exact(2))[0]
    elif length == 127:
        length = struct.unpack("!Q", exact(8))[0]
    require(length <= 1024 * 1024, "fixture bounds WebSocket messages")
    mask = exact(4) if second & 128 else None
    payload = exact(length)
    if mask:
        payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
    if first & 15 == 8:
        raise EOFError("fixture WebSocket close frame")
    require(first & 15 == 1, "fixture receives text RPC frames")
    return json.loads(payload)


def websocket_send(stream, value):
    encoded = json.dumps(value).encode()
    prefix = (bytes([0x81, len(encoded)]) if len(encoded) < 126 else
              bytes([0x81, 126]) + struct.pack("!H", len(encoded)))
    stream.sendall(prefix + encoded)


def fixture_codex(arguments):
    """Implement an App Server protocol fixture without loading any provider."""
    run = Path(arguments[arguments.index("--fixture-root") + 1])
    address = arguments[arguments.index("--listen") + 1]
    require(address.startswith("unix://"), "fixture only binds its private Unix socket")
    socket_path = Path(address.removeprefix("unix://"))
    state_path = run / "fixture-codex-state.json"
    state = json.loads(state_path.read_text()) if state_path.exists() else {"threads": {}}
    if socket_path.exists():
        socket_path.unlink()
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(socket_path))
    listener.listen(1)
    disconnect_seen = None
    while True:
        stream, _ = listener.accept()
        stream.settimeout(5)
        header = b""
        try:
            while not header.endswith(b"\r\n\r\n"):
                part = stream.recv(1)
                if not part:
                    raise EOFError("fixture socket probe closed before WebSocket handshake")
                header += part
                require(len(header) <= 16384, "fixture bounds WebSocket handshake")
        except (EOFError, OSError):
            stream.close()
            continue
        key = next(line.split(":", 1)[1].strip() for line in header.decode().splitlines()
                   if line.lower().startswith("sec-websocket-key:"))
        accept = base64.b64encode(hashlib.sha1(
            (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        stream.sendall(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                        "Connection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\n\r\n").encode())
        connection = uuid.uuid4().hex
        subscribed = set()

        def emit_approval(thread_id, thread):
            if not thread.get("directive", {}).get("approval") or not thread.get("turns"):
                return
            event = {"id": "approval-" + thread_id, "method": "item/commandExecution/requestApproval",
                     "params": {"threadId": thread_id, "turnId": thread["turns"][-1]["id"],
                                "itemId": "fixture-command", "command": "printf fixture",
                                "cwd": thread["cwd"], "reason": "controlled approval acceptance"}}
            for _ in range(2):
                websocket_send(stream, event)
            fixture_record(run, "codex-events.jsonl", {"connection": connection,
                           "thread_id": thread_id, "event": "approval", "copies": 2})

        try:
            while True:
                disconnect = run / "fixture-codex-disconnect"
                generation = disconnect.read_text() if disconnect.exists() else None
                if generation != disconnect_seen:
                    disconnect_seen = generation
                    if generation is not None:
                        break
                for thread_id in list(subscribed):
                    thread = state["threads"][thread_id]
                    gate = thread.get("directive", {}).get("gate")
                    if thread["status"]["type"] == "active" and gate and Path(gate).exists():
                        thread["status"] = {"type": "idle"}
                        turn = thread["turns"][-1]
                        turn.update(status="completed", items=[{"id": "fixture-answer", "type": "agentMessage",
                                                               "text": "fixture Codex terminal result"}])
                        private_json(state_path, state)
                        event = {"method": "turn/completed", "params": {"threadId": thread_id, "turn": turn}}
                        websocket_send(stream, event)
                        websocket_send(stream, event)
                        if thread.get("directive", {}).get("approval"):
                            websocket_send(stream, {"method": "serverRequest/resolved",
                                                   "params": {"requestId": "approval-" + thread_id}})
                if not select.select([stream], [], [], 0.1)[0]:
                    continue
                message = websocket_receive(stream)
                fixture_record(run, "codex-rpc.jsonl", {"connection": connection, "pid": os.getpid(),
                               "message": message})
                method = message.get("method")
                params = message.get("params", {})
                if method is None or "id" not in message:
                    continue
                if method == "initialize":
                    result = {"userAgent": "controlled-fixture", "platformFamily": "unix", "platformOs": sys.platform}
                elif method == "thread/start":
                    thread_id = "fixture-thread-" + uuid.uuid4().hex
                    thread = {"id": thread_id, "cwd": params["cwd"], "status": {"type": "idle"}, "turns": []}
                    state["threads"][thread_id] = thread
                    result = {"thread": thread}
                    subscribed.add(thread_id)
                elif method in {"thread/read", "thread/resume", "thread/turns/list", "turn/start"}:
                    thread_id = params["threadId"]
                    thread = state["threads"][thread_id]
                    if method == "turn/start":
                        directive = json.loads(params["input"][0]["text"])
                        thread["directive"] = directive
                        thread["status"] = {"type": "active", "activeFlags": []}
                        turn = {"id": "fixture-turn-" + uuid.uuid4().hex, "status": "inProgress", "items": []}
                        thread["turns"].append(turn)
                        result = {"turn": turn}
                    elif method == "thread/turns/list":
                        result = {"data": thread["turns"], "nextCursor": None}
                    else:
                        result = {"thread": thread}
                    subscribed.add(thread_id)
                else:
                    websocket_send(stream, {"id": message["id"], "error": {"code": -32601,
                                            "message": "fixture does not implement " + method}})
                    continue
                private_json(state_path, state)
                websocket_send(stream, {"id": message["id"], "result": result})
                if method in {"thread/resume", "turn/start"}:
                    emit_approval(thread_id, thread)
        except (EOFError, OSError):
            pass
        finally:
            stream.close()


def fixture_manager(arguments):
    """Exercise only the sidecar protocol and real scoped Node tools, never a model."""
    run = Path(arguments[arguments.index("--fixture-root") + 1])
    initial = json.loads(sys.stdin.readline())
    require(initial["type"] == "init", "fixture receives sidecar initialization")
    prompt = initial["prompt"]
    start = prompt.rfind("\n\n{")
    require(start >= 0, "fixture receives explicit management context")
    context, _ = json.JSONDecoder().raw_decode(prompt[start + 2:])
    trigger = context["trigger_event_hash"]
    body = (context.get("trigger_event") or {}).get("body", {})
    directive = {}
    try:
        candidate = json.loads(body.get("text", ""))
        if isinstance(candidate, dict) and (candidate.get("phase1") or candidate.get("chat_agent")):
            directive = candidate
    except ValueError:
        pass
    autonomous = body.get("role") != "user"
    label = directive.get("label", body.get("message_id", "autonomous"))
    native = initial.get("session_id") or "fixture-native-" + uuid.uuid4().hex
    require("tool_env" not in initial and "custom_tools" not in initial,
            "fixture receives one private-tool contract without callback credentials")
    schemas = initial["tools"]
    require(isinstance(schemas, dict) and all(isinstance(schema, dict)
            and isinstance(schema.get("description"), str)
            and isinstance(schema.get("inputSchema"), dict) for schema in schemas.values()),
            "tool catalog includes the authoritative description and JSON input schema")
    read_only = not {"command", "delegate", "continue_execution", "stop_execution"}.intersection(schemas)
    emit = lambda value: print(json.dumps(value), flush=True)
    emit({"type": "ready", "session_id": initial.get("session_id")})
    require(json.loads(sys.stdin.readline())["type"] == "ack", "fixture waits for durable start ACK")
    if not initial.get("session_id"):
        emit({"type": "session", "session_id": native})
        require(json.loads(sys.stdin.readline())["type"] == "session_ack",
                "fixture waits for durable native session ACK")
    record = {"conversation_id": context["conversation_id"], "trigger": trigger, "label": label,
              "native_session_id": native, "resumed": bool(initial.get("session_id")),
              "autonomous": autonomous, "read_only": read_only, "pid": os.getpid(),
              "bot": context.get("bot"), "bot_prompt_markers": [marker for marker in
                  ("bot-alpha-instructions-sentinel", "bot-beta-instructions-sentinel") if marker in prompt],
              "source_node": context.get("source_node"), "resource_id": context.get("resource_id")}
    fixture_record(run, "manager-invocations.jsonl", record)
    expected_tools = {"nodes", "context", "execution_status", "call_lookup"}
    if not read_only:
        expected_tools.update({"command", "delegate", "continue_execution", "stop_execution"})
    require(set(schemas) == expected_tools, "sidecar receives the authoritative scoped high-level tool catalog")
    fixture_record(run, "manager-tool-catalog.jsonl", {"trigger": trigger, "tools": schemas})

    def tool(name, arguments, expected=True, rejection="read-only", call_ref=None, discard=False):
        arguments = dict(arguments)
        if call_ref and name in {"command", "delegate", "continue_execution", "stop_execution"}:
            arguments["call_ref"] = call_ref
        # Transport correlation is fresh even when the stable intent is retried.
        call_id = uuid.uuid4().hex
        started = time.monotonic()
        fixture_record(run, "manager-tool-starts.jsonl", {"conversation_id": context["conversation_id"],
                       "trigger": trigger, "name": name, "call_id": call_id,
                       "started_at_ms": int(time.time() * 1000)})
        emit({"type": "tool_call", "call_id": call_id, "name": name, "arguments": arguments})
        require(select.select([sys.stdin], [], [], 30)[0], "private tool result arrives before timeout")
        encoded = sys.stdin.readline(1024 * 1024 + 1)
        require(encoded and len(encoded.encode()) <= 1024 * 1024,
                "private tool result is present and bounded")
        frame = json.loads(encoded)
        require(frame.get("type") == "tool_result" and frame.get("call_id") == call_id,
                "private tool result matches this transport attempt")
        value = frame.get("result")
        require(isinstance(value, dict), "private tool result is an object")
        error = value.get("error")
        accepted = error is None
        if error is not None:
            require(isinstance(error, str), "private tool rejection has a bounded diagnostic")
            fixture_record(run, "manager-tool-errors.jsonl", {
                "conversation_id": context["conversation_id"], "trigger": trigger, "name": name,
                "error": error[:2048], "elapsed_ms": round((time.monotonic() - started) * 1000)})
            if not expected and name not in schemas:
                require(error == "tool_unavailable_in_scope", "missing-catalog tool has the exact scope rejection")
            else:
                require(rejection in error, "negative tool check rejects specifically for its expected authority constraint")
        fixture_record(run, "manager-tools.jsonl", {"conversation_id": context["conversation_id"],
                       "trigger": trigger, "name": name, "call_id": call_id, "arguments": arguments,
                       "accepted": accepted, "expected": expected,
                       "call_ref": call_ref, "response_discarded": discard,
                       "result": None if discard else value,
                       "elapsed_ms": round((time.monotonic() - started) * 1000)})
        require(accepted == expected, "scoped tool " + name + " has its expected authority")
        if not expected:
            tool("context", {"limit": 1})
        return None if discard else value

    dispatch_spec = directive.get("dispatch")
    if dispatch_spec:
        label = dispatch_spec["label"]
        arguments = {"target_node": dispatch_spec["target_node"], "cwd": dispatch_spec["cwd"],
                     "executor": dispatch_spec.get("executor", "claude_code"),
                     "text": json.dumps({"label": label, "gate": dispatch_spec.get("gate"),
                                         "result_size": dispatch_spec.get("result_size", 0),
                                         "approval": dispatch_spec.get("approval", False)})}
        wait_for("delegate dispatches one durable open and send", lambda:
                 tool("delegate", arguments, call_ref="delegate-" + label).get("all_dispatched"))
    if autonomous:
        require(read_only, "ordinary result callbacks have server-enforced read-only authority")
        tool("command", {"command": "printf forbidden > forbidden-followup", "cwd": str(run / "workspace")},
             expected=False)
    for action in directive.get("tools", []):
        call_ref = action.get("call_ref")
        for index in range(action.get("repeat", 1)):
            value = tool(action["name"], action["arguments"], expected=action.get("accepted", True),
                         rejection=action.get("rejection", "read-only"), call_ref=call_ref,
                         discard=action.get("discard_first", False) and index == 0)
        if action.get("until_dispatched"):
            wait_for("repeated call keeps its prepared request until dispatched", lambda:
                     tool(action["name"], action["arguments"], call_ref=call_ref).get("all_dispatched"))
        if action.get("wait_terminal"):
            wait_for("tool operation reaches its expected terminal state", lambda:
                (tool("execution_status", {"work_ref": value["work_ref"]}).get("operation") or {}).get("state")
                in {"succeeded", "failed", "rejected"})
    sequence = 0
    gate = directive.get("gate")
    while True:
        sequence += 1
        emit({"type": "event", "event": {"type": "item.updated", "item": {
            "id": "fixture-text", "type": "agent_message", "text": label + ":" + str(sequence)}}})
        if not gate or Path(gate).exists():
            break
        if select.select([sys.stdin], [], [], 0.2)[0]:
            control = sys.stdin.readline()
            if not control:
                return
            if json.loads(control)["type"] == "cancel":
                require(json.loads(control).get("session_id") == native,
                        "cancel targets the current native manager session")
                if not directive.get("cancel_unknown"):
                    emit({"type": "cancelled", "session_id": native,
                          "confirmation": "managed_processes_exited"})
                return
    emit({"type": "result", "session_id": native, "final_response": label + ":complete"})


def operation(client, node, operation_id):
    if client.manager_fixture_run is None:
        return client.rpc("operation", target_node=node.node_id, operation_id=operation_id)["operation"]
    records = [row for row in fixture_records(client.manager_fixture_run, "manager-tools.jsonl")
               if row.get("accepted") and row.get("name") in {"command", "delegate"}
               and row.get("result") and any(item["operation_id"] == operation_id
                   for item in row["result"].get("operations", []))]
    if not records:
        return None
    record = records[-1]
    result = client.rpc("execution_status", conversation_id=record["conversation_id"],
                        expected_operation_id=operation_id)
    original = result["operation"]["request"]
    require(original["operation_id"] == operation_id and original["target_node"] == node.node_id
            and original["space_id"] == client.scope["space_id"]
            and original["conversation_id"] == record["conversation_id"],
            "read-only execution status returns the intended operation and exact conversation")
    intended = record["arguments"]
    require(all(original.get(field) is None for field in ("task_id", "binding")),
            "ordinary SDK actions do not manufacture Task bindings")
    action = original["action"]
    if record["name"] == "command":
        require(action["type"] == "command" and action["command"] == intended["command"]
                and action["cwd"] == str(Path(intended["cwd"]).resolve())
                and original.get("session_id") is None and original["expected_revision"] == 0,
                "read-only command result is bound to the complete original sessionless intent")
        if "timeout_secs" in intended:
            require(action["timeout_secs"] == intended["timeout_secs"],
                    "command result preserves the explicitly requested timeout")
    elif record["name"] == "delegate":
        pair = record["result"]["operations"]
        require(len(pair) == 2 and pair[0]["operation_id"] != pair[1]["operation_id"]
                and action["type"] in {"open", "send"} and original.get("session_id"),
                "ordinary delegation retains exactly one real Open and Send")
        if action["type"] == "open":
            require(operation_id == pair[0]["operation_id"] and original["expected_revision"] == 0
                    and action["cwd"] == str(Path(intended["cwd"]).resolve())
                    and action["executor"] == intended.get("executor", "codex"),
                    "ordinary Open preserves the initial revision, executor and canonical directory")
        else:
            opened = operation(client, node, pair[0]["operation_id"])
            require(opened and operation_id == pair[1]["operation_id"]
                    and original["expected_revision"] == 1 and action["text"] == intended["text"]
                    and original["session_id"] == opened["request"]["session_id"]
                    and original["target_node"] == opened["request"]["target_node"],
                    "ordinary Send uses its own admitted Open session, target and next revision")
    if node.node_id != hashlib.sha256(bytes.fromhex(client.host_public_key)).hexdigest():
        verified = client.command("verify_manager_execution", status=result, operation_id=operation_id,
                                  target_public_key=node.node_public_key)
        require(verified["request"] == original, "native observation verifies the exact foreign request")
    previous = client.manager_originals.setdefault(operation_id, copy.deepcopy(original))
    require(previous == original, "repeated read-only status never replaces the original operation")
    return result["operation"]


def admitted_scope(node, client, operation_id):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        connection.row_factory = sqlite3.Row
        rows = connection.execute(
            "SELECT * FROM accepted_authorizations WHERE space_id=? AND authorization_id=?",
            (client.scope["space_id"], operation_id)).fetchall()
    require(len(rows) == 1, "execution has exactly one durable authorization admission")
    record = dict(rows[0])
    signed = json.loads(record["signed_invocation"])
    body = json.loads(record["source_body"])
    require(record["actor_kind"] == "account" and record["actor_id"] == client.scope["account_id"]
            and record["device_id"] == client.scope["device_id"]
            and record["resource_id"] == client.resource_id
            and record["operation_id"] == operation_id
            and signed["payload"]["operation_id"] == operation_id
            and body["space_id"] == client.scope["space_id"]
            and body["resource_id"] == client.resource_id
            and body["authorization_id"] == operation_id,
            "durable admission keeps the original account, space, device, resource and operation identity")
    return record


def resolution_audit(node, client, operation_id):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        connection.row_factory = sqlite3.Row
        rows = connection.execute(
            "SELECT * FROM accepted_authorizations WHERE space_id=? AND json_extract(source_body,'$.method')='resolve_unknown' AND json_extract(source_body,'$.operation_id')=?",
            (client.scope["space_id"], operation_id)).fetchall()
    require(len(rows) == 1, "Unknown resolution has one independent durable authorization")
    record = dict(rows[0])
    body = json.loads(record["source_body"])
    invocation = json.loads(record["signed_invocation"])["payload"]
    require(record["actor_kind"] == "account" and record["actor_id"] == client.scope["account_id"]
            and record["device_id"] == client.scope["device_id"]
            and record["resource_id"] == client.resource_id
            and body["operation_id"] == operation_id
            and invocation["operation_id"] == body["authorization_id"] != operation_id,
            "resolution keeps the original actor and Project and uses an independent signed call identity")
    return record


def request(space, conversation, node, operation_id, session_id, action, revision=0, task=None):
    return {"operation_id": operation_id, "space_id": space,
            "conversation_id": conversation, "task_id": task,
            "target_node": node.node_id, "session_id": session_id,
            "expected_revision": revision, "basis_heads": [], "action": action,
            "resources": [action["cwd"]] if action["type"] == "open" else []}


def dispatch(client, value, status=200):
    key = (client.scope["space_id"], client.resource_id, value["conversation_id"], value["task_id"], value["session_id"])
    binding = client.bindings.get(key)
    if binding is not None:
        value["binding"] = binding
        if value["action"]["type"] == "send":
            value["resources"] = [binding["binding"]["open_spec"]["cwd"]]
    return client.rpc("operate", request=value, status=status)


def succeeded(client, node, operation_id):
    record = operation(client, node, operation_id)
    require(record is not None, "accepted operation is queryable")
    if record["state"] in {"failed", "rejected", "unknown"}:
        raise RuntimeError(f"operation {operation_id} reached {record['state']}")
    return record if record["state"] == "succeeded" else None


def verify_snapshot(client, snapshot, resource_id, author_keys):
    require(snapshot["version"] == 2 and snapshot["space_id"] == client.scope["space_id"],
            "snapshot declares the current wire version and expected space")
    events = {event["hash"]: event for event in snapshot["events"]}
    require(len(events) == len(snapshot["events"]), "snapshot contains no duplicate event hashes")
    for event_hash, signed in events.items():
        claimed_author = json.loads(signed["canonical_body"])["author"]
        require(claimed_author in author_keys, "event author has an independently verified Node key")
        verified = client.command("verify_event", envelope=signed,
                                  space_id=client.scope["space_id"], resource_id=resource_id,
                                  author_public_key=author_keys[claimed_author])
        require(verified["hash"] == event_hash, "shared verifier preserves the exported event hash")
        event = verified["body"]
        dependencies = event["parents"] + ([event["previous"]] if event["previous"] else [])
        require(all(dependency in events for dependency in dependencies), "export includes complete event closure")
    require(all(head in events for head in snapshot["heads"]), "snapshot contains advertised heads")
    return len(events)


def verify_blobs(node, network):
    connection = sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True)
    try:
        rows = connection.execute("SELECT hash,size,content FROM blobs WHERE space_id=?", (network,)).fetchall()
        for digest, size, content in rows:
            require(size == len(content) and hashlib.sha256(content).hexdigest() == digest,
                    "replicated blob length and digest match")
        return len(rows)
    finally:
        connection.close()


def verify_no_model_budget(nodes):
    for node in nodes:
        connection = sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True)
        try:
            used = connection.execute("SELECT COALESCE(SUM(used),0) FROM budget_days").fetchone()[0]
            require(used == 0, "ordinary progress and replay never reserve management model budget")
        finally:
            connection.close()


def javascript_dependencies(content):
    # Static module specifiers cannot use templates; minifiers use them for ordinary strings.
    static = rb"\b(?:import|export)\s*(?:[^;\"'`]*?\bfrom\s*)?[\"']([^\"']+)[\"']"
    dynamic = rb"\bimport\s*\(\s*[\"'`]([^\"'`]+)[\"'`]"
    return [address.decode() for pattern in (static, dynamic) for address in re.findall(pattern, content)]


def verify_web_assets(node, module):
    def fetch(path, *, method="GET", status=200, max_bytes=8 * 1024 * 1024):
        request = urllib.request.Request(node.url + path, headers={"Origin": node.url}, method=method)
        try:
            response = urllib.request.urlopen(request, timeout=10)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            content = response.read(max_bytes + 1)
            require(response.status == status and len(content) <= max_bytes,
                    f"{method} {path}: expected bounded HTTP {status}")
            return content, response.headers

    shell, headers = fetch("/")
    require(headers.get_content_type() == "text/html" and headers["Cache-Control"] == "no-store",
            "SPA shell is HTML and cannot be stored")
    policy = headers["Content-Security-Policy"]
    script_policy = next(part.strip() for part in policy.split(";") if part.strip().startswith("script-src "))
    require("'unsafe-inline'" not in script_policy and "'wasm-unsafe-eval'" in script_policy,
            "scripts allow WASM compilation without allowing arbitrary inline JavaScript")
    require("ws:" in policy and "wss:" in policy and "data:" in policy and "blob:" in policy,
            "CSP permits encrypted remote transport and local image previews")
    inline = re.findall(rb"<script\b([^>]*)>(.*?)</script\s*>", shell, re.DOTALL)
    require(inline, "Start shell contains its generated bootstrap scripts")
    for attributes, body in inline:
        if not re.search(rb"\bsrc\s*=", attributes):
            digest = base64.b64encode(hashlib.sha256(body).digest()).decode()
            require(f"'sha256-{digest}'" in script_policy, "every inline bootstrap script has its exact CSP hash")
    for path in ("/", "/conversations", "/tasks/example"):
        page, page_headers = fetch(path)
        require(page == shell and page_headers["Cache-Control"] == "no-store", "SPA routes serve the same uncached shell")
        body, head_headers = fetch(path, method="HEAD")
        require(not body and head_headers.get_content_type() == "text/html"
                and int(head_headers["Content-Length"]) == len(shell), "HEAD preserves HTML metadata without a body")

    class References(HTMLParser):
        def __init__(self):
            super().__init__()
            self.paths = set()

        def handle_starttag(self, tag, attrs):
            values = dict(attrs)
            address = values.get("src") if tag == "script" else values.get("href") if tag == "link" else None
            if address:
                require(address.startswith("/") and not address.startswith("//"), "shell resources come from the trusted Web origin")
                self.paths.add(address)

    references = References()
    references.feed(shell.decode())
    pending, checked, content_types = list(references.paths), set(), set()
    uses_external_crypto = False
    while pending:
        path = pending.pop()
        if path in checked:
            continue
        checked.add(path)
        if path == "/generated/vibeface_secure.js":
            continue
        require(len(checked) < 256, "static dependency traversal is bounded")
        content, asset_headers = fetch(path)
        mime = asset_headers.get_content_type()
        content_types.add(mime)
        require(path.startswith("/assets/") and asset_headers["Cache-Control"] == "public, max-age=31536000, immutable",
                f"{path}: hashed client assets are immutable")
        body, head_headers = fetch(path, method="HEAD")
        require(not body and int(head_headers["Content-Length"]) == len(content)
                and head_headers.get_content_type() == mime and head_headers["Cache-Control"] == asset_headers["Cache-Control"],
                "HEAD preserves asset MIME, length and cache policy without a body")
        if mime == "text/javascript":
            require(b"__wbindgen_" not in content and b"vibeface_secure_bg" not in content,
                    "client chunks do not duplicate generated cryptography or embed a second WASM instance")
            for address in javascript_dependencies(content):
                if address == "/generated/vibeface_secure.js":
                    uses_external_crypto = True
                else:
                    require(address.startswith(("./", "../", "/assets/")), "client imports only trusted local modules")
                    pending.append(urllib.parse.urljoin(path, address))
        elif mime == "text/css":
            for reference in re.findall(rb"url\([\"']?([^\"')]+)", content):
                address = reference.decode().strip()
                if not address.startswith("data:"):
                    pending.append(urllib.parse.urljoin(path, address))
    require(uses_external_crypto, "client chunks import the single same-origin generated cryptography module")
    require({"text/css", "text/javascript", "font/woff2"} <= content_types,
            "real JavaScript, CSS and bundled font resources are reachable")
    for path in ("/api", "/api/not-a-route", "/peer/not-a-route", "/sdk.v1.SdkCustomToolCallbackService/CallCustomTool",
                 "/assets", "/assets/not-a-file", "/assets/missing.js", "/generated", "/generated/not-a-file", "/missing.css"):
        body, missing_headers = fetch(path, status=404)
        require(body != shell and missing_headers["Cache-Control"] == "no-store", "unknown API and assets return uncached 404, never the SPA shell")
    for path in (module, module.with_name("vibeface_secure_bg.wasm")):
        content, crypto_headers = fetch("/generated/" + path.name, max_bytes=path.stat().st_size)
        require(hashlib.sha256(content).hexdigest() == file_sha256(path),
                "served browser cryptography matches the verified package resource")
        require(crypto_headers["Cache-Control"] == "no-store", "mutable browser cryptography cannot be cached across updates")
        if path.suffix == ".wasm":
            require(crypto_headers.get_content_type() == "application/wasm", "browser WASM has the correct MIME type")


def verify_incompatible_data(run, binary, workspace):
    directory = run / "INCOMPATIBLE"
    try:
        result = subprocess.run([str(binary), "init", "--data-dir", str(directory), "--root", str(workspace)],
                                env={**os.environ, "RUST_BACKTRACE": "1"}, capture_output=True, timeout=45)
    except subprocess.TimeoutExpired as error:
        (run / "incompatible-init.log").write_bytes((error.stdout or b"") + (error.stderr or b""))
        raise RuntimeError("incompatible fixture: CLI initialization timed out") from None
    (run / "incompatible-init.log").write_bytes(result.stdout + result.stderr)
    require(result.returncode == 0, "format boundary fixture initializes through the real CLI")
    verify_current_data_format(directory)
    (directory / "node.lock").unlink(missing_ok=True)
    provider = directory / "provider-probe"
    provider.write_text(f"#!{sys.executable}\nfrom pathlib import Path\nPath({str(directory / 'provider-started')!r}).touch()\n")
    provider.chmod(0o700)
    configuration = json.loads((directory / "config.json").read_text())
    for key in ("codex_bin", "claude_bin", "cursor_bin"):
        configuration[key] = str(provider)
    private_json(directory / "config.json", configuration)
    marker = directory / "data-epoch.json"

    def rejected_startup():
        for command in ([str(binary), "serve", "--data-dir", str(directory)],
                        [str(binary), "--data-dir", str(directory)]):
            before = data_directory_snapshot(directory)
            process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                       start_new_session=True)
            try:
                stdout, stderr = process.communicate(timeout=10)
                require(process.returncode != 0 and any(
                    word in (stdout + stderr).lower() for word in (b"format", b"epoch", b"layout", b"legacy")),
                        "incompatible data rejects startup before opening stores or providers")
                require(data_directory_snapshot(directory) == before,
                        "rejected startup preserves every directory entry, mode and file byte, including SQLite sidecars")
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=5)

    for value in ({"data_epoch": 2}, None, {**DATA_FORMAT, "format": "other-node"},
                  {**DATA_FORMAT, "layout_version": 2}):
        if value is None:
            marker.unlink()
        else:
            private_json(marker, value)
        rejected_startup()
    private_json(marker, DATA_FORMAT)
    database = directory / "node.sqlite3"
    current_database = run / "current-format.sqlite3"
    database.rename(current_database)
    rejected_startup()
    database.touch()
    rejected_startup()
    database.unlink()
    old_layout = sqlite3.connect(database)
    try:
        old_layout.execute("PRAGMA journal_mode=WAL")
        old_layout.execute("PRAGMA user_version=2")
        old_layout.execute("CREATE TABLE networks(network_id TEXT PRIMARY KEY, bundle TEXT)")
        old_layout.execute("INSERT INTO networks VALUES('legacy-personal-network','preserved legacy bundle')")
        old_layout.commit()
        old_layout.execute("PRAGMA wal_checkpoint(PASSIVE)")
        require((directory / "node.sqlite3-wal").exists(), "old-layout fixture includes a live WAL")
        rejected_startup()
    finally:
        old_layout.close()
    database.unlink()
    current_database.rename(database)
    legacy_path = directory / "identity.sqlite3"
    legacy = sqlite3.connect(legacy_path)
    try:
        legacy.execute("PRAGMA journal_mode=WAL")
        legacy.execute("PRAGMA user_version=2")
        legacy.execute("CREATE TABLE preserved(value TEXT)")
        legacy.execute("INSERT INTO preserved VALUES('keep this old identity database')")
        legacy.commit()
        require((directory / "identity.sqlite3-wal").exists(), "legacy fixture includes a live WAL")
        rejected_startup()
        private_json(marker, {"data_epoch": 2})
        rejected_startup()
    finally:
        legacy.close()
    private_json(marker, DATA_FORMAT)
    legacy_path.unlink()
    legacy_path.symlink_to(run / "missing-identity-database")
    rejected_startup()


def recover_same_node(node):
    previous = node.client
    restored = Client(node.url, previous.helper, previous.module)
    try:
        wrong = restored.command("open_recovery", envelope=node.backup,
                                 password="incorrect-acceptance-password", check=False)
        require(not wrong["ok"], "wrong recovery password is rejected locally")
        uninitialized = restored.command("recovery_context_request", node_public_key=node.node_public_key, check=False)
        require(not uninitialized["ok"], "a rejected recovery open leaves no usable material")
        context = restored.command("open_recovery", envelope=node.backup, password=node.backup_password)
        require(context["scope"] == previous.scope and context["entry"]["node_public_key"] == node.node_public_key,
                "encrypted recovery material independently pins the original account, space and Node")
        restored.connect(node.node_id, node_public_key=node.node_public_key)
        restored.command("recovery_context_request", node_public_key=node.node_public_key)
        prepared = restored.command("prepare_recovery")
        proof = restored.identity_call("recovery_node_possession", prepared)
        created = restored.command("finish_recovery", proof=proof["proof"], endpoint=node.url,
                                   password=node.backup_password)
        result = restored.identity_call("recover", created["commit"])
        require(result["scope"] == created["scope"]
                and result["scope"]["account_id"] == previous.scope["account_id"]
                and result["scope"]["space_id"] == previous.scope["space_id"]
                and result["scope"]["device_id"] != previous.scope["device_id"],
                "typed recovery preserves the account and space while installing a new device")
        restored.scope = created["scope"]
        restored.resource_id = previous.resource_id
        restored.authenticate()
        rejected = previous.command("authenticate", resource_id=previous.resource_id, audience=previous.audience,
                                    expected_host_public_key=node.node_public_key, check=False)
        require(not rejected["ok"] and rejected.get("rpc_method") in {
                    "begin_client_policy_read", "confirm_client_policy_auth", "authenticate"},
                "the pre-recovery device is rejected by the current bounded authentication protocol")
        inspected = restored.command("open_recovery", envelope=created["recovery_envelope"], password=node.backup_password)
        require(inspected["scope"] == restored.scope,
                "the new encrypted recovery material represents the recovered account and device")
        node.backup = created["recovery_envelope"]
        path = node.directory / "account-recovery.json"
        path.write_bytes(bytes.fromhex(node.backup))
        path.chmod(0o600)
        previous.close()
        node.client = restored
        return restored
    except Exception:
        restored.close()
        raise


def business_policy_snapshot(node):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        grants = {digest: json.loads(entry)["value"]["payload"]["grantee"] for digest, entry in connection.execute(
            "SELECT entry_hash,entry FROM policy_journal WHERE json_extract(entry,'$.kind')='grant' ORDER BY entry_hash")}
        spaces = connection.execute(
            "SELECT domain_id,authority_epoch,sequence,head_hash FROM current_policy_heads WHERE domain_kind='space' ORDER BY domain_id"
        ).fetchall()
    require(spaces, "Node policy contains an independently established space head")
    return {"grants": grants, "space_heads": spaces}


def enroll_account_node(authority, candidate):
    client = authority.client
    before = business_policy_snapshot(authority)
    scope = copy.deepcopy(client.scope)
    context = client.command("open_recovery", envelope=authority.backup, password=authority.backup_password)
    require(context["scope"] == scope,
            "Node enrollment root material belongs to the existing independently pinned account")
    connection = "candidate-" + candidate.name
    client.command("connect", url=candidate.url, node_id=candidate.node_id,
                   node_public_key=candidate.node_public_key, connection=connection)
    require(not client.identity_call("status", connection=connection)["initialized"],
            "additional Node starts with an unclaimed installation")
    claim = client.identity_call("node_claim", {"capability": candidate.access["capability"]}, connection=connection)
    prepared = client.command("prepare_node_enrollment", claim=claim["claim"],
                              node_public_key=candidate.node_public_key,
                              claim_nonce=candidate.access["claim_nonce"])
    snapshot = client.identity_call("enrollment_policy_read_begin", prepared)
    begin = {**prepared, "snapshot": snapshot["snapshot"]}
    progress = client.identity_call("account_node_import_begin", begin, connection=connection)
    require(client.identity_call("account_node_import_begin", begin, connection=connection) == progress,
            "the original additional-Node import begin is idempotent")
    import_context = progress["context"]
    while not progress.get("proof"):
        page = client.identity_call("enrollment_policy_page", progress["page_request"])
        progress = client.identity_call("account_node_import_page", {
            "context": import_context, "page": page["page"]}, connection=connection)
        require(progress["context"] == import_context, "additional-Node import preserves its original purpose context")
        if page["page"]["payload"]["complete"]:
            break
    witness = client.identity_call("enrollment_policy_witness", progress["page_request"])
    finish = {"context": import_context, "witness": witness["witness"]}
    possession = client.identity_call("account_node_import_finish", finish, connection=connection)
    require(client.identity_call("account_node_import_finish", finish, connection=connection) == possession,
            "the original additional-Node import returns its persisted possession proof")
    commit = client.command("prepare_node_enrollment_commit", possession_signature=possession["proof"])
    committed = client.identity_call("account_node_commit", commit)
    require(client.identity_call("account_node_commit", commit) == committed,
            "the existing account authority returns the same enrollment receipt")
    verified = client.command("finish_node_enrollment", journal=committed["journal_delta"])
    installation = {"context": import_context, "installation": verified["installation"]}
    installed = client.identity_call("account_node_install", installation, connection=connection)
    require(installed["installed"] is True and installed["account_id"] == scope["account_id"]
            and installed["node_public_key"] == candidate.node_public_key
            and installed["pins"] == verified["pins"],
            "additional Node installs the verified existing account and the committed after-pins")
    require(client.identity_call("account_node_install", installation, connection=connection) == installed,
            "the same account Node installation is idempotent")
    client.authenticate()
    after = business_policy_snapshot(authority)
    projected = business_policy_snapshot(candidate)
    account_grantee = {"kind": "actor", "value": {"kind": "account", "id": scope["account_id"]}}
    expected_grants = {digest: grantee for digest, grantee in before["grants"].items() if grantee == account_grantee}
    require(client.scope == scope and after == before,
            "Node enrollment preserves the original account and the authority's complete business policy")
    require(expected_grants and projected["grants"] == expected_grants
            and projected["space_heads"] == before["space_heads"],
            "additional Node installs only the existing Account Grant projection and unchanged space heads")
    return {"node_id": candidate.node_id, "account_id": scope["account_id"],
            "business_policy_sha256": hashlib.sha256(json.dumps(before, sort_keys=True).encode()).hexdigest(),
            "authority_before": before, "authority_after": after, "candidate_projection": projected,
            "additional_business_grants": 0, "node_journal_delta": committed["journal_delta"]}


def enroll_account_client(authority, candidate, peers):
    owner, client = authority.client, candidate.client
    handle = "client-enrollment-" + candidate.name
    public_key = client.command("identity")["public_key"]
    owner_key = owner.command("identity")["public_key"]
    require(public_key != owner_key and public_key != candidate.node_public_key,
            "new browser client, existing owner and Node hold distinct private keys")
    owner.authenticate()
    offer = owner.command("create_client_enrollment_offer", handle=handle)
    require(offer["reference"]["account_id"] == owner.scope["account_id"]
            and offer["reference"]["node_public_key"] == authority.node_public_key,
            "locally created enrollment offer pins the expected account and authority Node")
    client.command("connect", url=authority.url, node_id=authority.node_id,
                   node_public_key=authority.node_public_key)
    begun = client.command("begin_client_enrollment", handle=handle, **offer,
                           expected_space_id=owner.scope["space_id"])
    owner.command("approve_client_enrollment", handle=handle, pending=begun["pending"],
                  expected_public_key=public_key)
    client.command("possess_client_enrollment", handle=handle)
    committed = owner.command("commit_client_enrollment", handle=handle)
    finished = client.command("finish_client_enrollment", handle=handle, committed=committed["committed"],
                              expected_scope=committed["scope"])
    require(finished["scope"] == committed["scope"] == begun["scope"]
            and finished["request_id"] == committed["request_id"],
            "new device completes its original owner-approved enrollment with exact scope")
    client.scope = finished["scope"]
    client.resource_id = owner.resource_id
    client.host_public_key = authority.node_public_key
    owner.authenticate()
    for peer in peers:
        connection = "client-policy-" + candidate.name + "-" + peer.name
        owner.command("connect", url=peer.url, node_id=peer.node_id,
                      node_public_key=peer.node_public_key, connection=connection)
        installed = owner.identity_call("policy_commit", {"journal": committed["journal_delta"]}, connection=connection)
        require(installed["accepted"], "enrolled Node accepts only the signed Account device delta")
    client.business_entry = {"url": authority.url, "node_id": authority.node_id,
                             "node_public_key": authority.node_public_key}
    client.connect()
    client.authenticate(host_public_key=authority.node_public_key)
    require(client.scope["account_id"] == owner.scope["account_id"]
            and client.scope["space_id"] == owner.scope["space_id"]
            and client.scope["device_id"] != owner.scope["device_id"],
            "same-Account browser clients authenticate with separate enrolled keys")
    return {"node_id": candidate.node_id, "client_public_key": public_key,
            "device_id": client.scope["device_id"], "request_id": committed["request_id"]}


def install_project_replica_grant(authority, replica, peers):
    owner = authority.client
    owner.authenticate()
    service = owner.command("derive_authorization_id", kind="service", value=replica.node_public_key)
    intent = {"space": owner.scope["space_id"], "grantee": {"kind": "actor", "value": {"kind": "service", "id": service}},
              "scope": {"kind": "exact", "value": {"space": owner.scope["space_id"], "resource": owner.resource_id}},
              "actions": ["replica_read"], "audience_node_public_key": authority.node_public_key,
              "constraints": {"context": {"conversation": None, "task": None, "sdk_session": None,
                  "execution_session": None, "executor_kind": None, "workspace_resource": None},
                  "require_account_actor": False, "require_webauthn_uv": False},
              "remaining_delegations": 0, "expires_at_ms": min(int(time.time() * 1000) + 3_600_000,
                  owner.policy["account_enrollment_expires_at_ms"])}
    handle = "replica-grant-" + replica.name
    prepared = owner.command("prepare_governance", handle=handle, kind="grant", intent=intent)
    committed = owner.identity_call("space_policy_commit", {"request": prepared["request"]})
    owner.command("finish_preparation", handle=handle, receipt=committed["committed"])
    require(committed["journal_delta"] == [*prepared["request"]["new_objects"],
            {"kind": "space_commit", "value": committed["committed"]}],
            "replica policy distribution contains exactly the owner-approved grant and its commit")
    for peer in peers:
        connection = "replica-policy-" + replica.name + "-" + peer.name
        owner.command("connect", url=peer.url, node_id=peer.node_id,
                      node_public_key=peer.node_public_key, connection=connection)
        require(owner.identity_call("policy_commit", {"journal": committed["journal_delta"]},
                                    connection=connection)["accepted"],
                "enrolled Node accepts the signed replica policy delta")
    owner.authenticate()
    return {"service_id": service, "resource_id": owner.resource_id, "actions": ["replica_read"]}


def setup_manager_topology(authority, executor, replica):
    peers = [executor, replica]
    authority.stop()
    authority.configure(network={"bootstrap_urls": [],
        "advertise_urls": [authority.url.replace("http:", "ws:") + "/api/network/ws"],
        "mdns": False, "relay_enabled": True})
    authority.start()
    authority.client.connect()
    authority.client.authenticate()
    enrollment = []
    for node in peers:
        node.configure(network={"bootstrap_urls": [authority.url.replace("http:", "ws:") + "/api/network/ws"],
            "advertise_urls": [node.url.replace("http:", "ws:") + "/api/network/ws"],
            "mdns": False, "relay_enabled": True})
        node.start()
        result = enroll_account_node(authority, node)
        for previous in peers[:peers.index(node)]:
            connection = "node-policy-" + node.name + "-" + previous.name
            authority.client.command("connect", url=previous.url, node_id=previous.node_id,
                node_public_key=previous.node_public_key, connection=connection)
            require(authority.client.identity_call("policy_commit", {"journal": result["node_journal_delta"]},
                    connection=connection)["accepted"], "previous Node receives the signed account-node enrollment delta")
        enrollment.append({key: value for key, value in result.items() if key != "node_journal_delta"})
    clients = [enroll_account_client(authority, node, peers) for node in peers]
    grants = [install_project_replica_grant(authority, node, peers) for node in peers]
    execution = register_execution_service(executor, authority=authority, policy_peers=peers)
    for node in (authority, executor, replica):
        node.client.authenticate()

        node.client.manager_fixture_run = authority.directory.parent
        node.client.target_public_keys.update({peer.node_id: peer.node_public_key
                                              for peer in (authority, executor, replica)})
    node_keys = {authority.node_public_key, executor.node_public_key, replica.node_public_key}
    client_keys = {node.client.command("identity")["public_key"] for node in (authority, executor, replica)}
    require(len(node_keys) == len(client_keys) == 3 and not node_keys & client_keys,
            "three enrolled Nodes and three browser clients retain six separate keys")
    return {"account_id": authority.client.scope["account_id"], "space_id": authority.client.scope["space_id"],
            "project_resource": authority.client.resource_id, "node_enrollments": enrollment,
            "client_enrollments": clients, "replica_grants": grants,
            "executor": {"node_id": executor.node_id, "resource_id": execution["resource_id"],
                         "root": str(executor.workspace)}}


def verify_account_gateway(authority, gateway, conversation, other_resource):
    client = authority.client
    connection = "gateway-" + gateway.name
    client.command("connect", url=gateway.url, node_id=gateway.node_id,
                   node_public_key=gateway.node_public_key, connection=connection)
    wait_for("Gateway authenticates the original Project host", lambda: client.authenticate(
        connection=connection, host_public_key=authority.node_public_key))
    wrong_host = client.command("check_authorization_host",
                                expected_host_public_key=gateway.node_public_key,
                                connection=connection, check=False)
    require(not wrong_host["ok"], "Gateway transport pin cannot replace the independently pinned Project host")
    wrong_transport = client.command("connect", url=gateway.url, node_id=authority.node_id,
                                     node_public_key=authority.node_public_key,
                                     connection="gateway-wrong-transport", check=False)
    require(not wrong_transport["ok"], "Project host pin cannot replace the actual Gateway Noise endpoint")
    direct = client.rpc("conversation", conversation_id=conversation)
    forwarded = client.rpc("conversation", conversation_id=conversation, connection=connection)
    require(forwarded == direct, "separately pinned Gateway returns the exact original Project-host history")
    client.rpc("conversation", resource_id=other_resource, conversation_id=conversation,
               connection=connection, status=400)
    client.rpc("message", conversation_id=conversation, message_id="gateway-message",
               text="A Gateway preserves the original signed account intent.", connection=connection)
    messages = client.rpc("conversation", conversation_id=conversation)["messages"]
    require(len([message for message in messages if message["message_id"] == "gateway-message"]) == 1,
            "Gateway message writes exactly once at the original Project host")
    with sqlite3.connect(f"file:{gateway.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
        require(all(database.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0] == 0
                    for table in ("events", "conversations", "messages", "tasks", "operations")),
                "Gateway without business Grants stores no private history or execution records")
    return {"gateway_node_id": gateway.node_id, "resource_host_node_id": authority.node_id,
            "account_id": client.scope["account_id"], "resource_id": client.resource_id,
            "distinct_transport_and_host_pins": True, "gateway_business_rows": 0}


def verify_channel_security(node):
    substituted = node.client.command("identity", identity="root")
    wrong = node.client.command("connect", url=node.url, node_id=substituted["node_id"],
                                node_public_key=substituted["public_key"],
                                connection="wrong-identity", check=False)
    require(wrong["ok"] is False, "Noise refuses a substituted target identity")
    legacy = node.client.command("connect", url=node.url, node_id=node.node_id,
                                 node_public_key=node.node_public_key, context="vibeface:client:v1",
                                 connection="legacy-protocol", check=False)
    require(legacy["ok"] is False, "Node refuses the old client handshake context")
    for kind in ("tamper", "replay"):
        connection = "negative-" + kind
        node.client.connect(connection=connection)
        node.client.authenticate(connection=connection)
        result = node.client.command("attack", kind=kind, connection=connection)
        require(result == {"rejected": True}, "invalid protected record closes the real Node channel")
    require(node.client.rpc("nodes")["nodes"], "valid channel remains usable after separate attack connections")


def verify_browser_origins(node, origins):
    for origin in origins:
        headers = {"Host": urllib.parse.urlsplit(origin).netloc, "Origin": origin}
        node.client.request("/api/info", headers=headers)
        node.client.request("/api/info", headers={**headers, "Origin": "https://hostile.invalid"}, status=403)
        wrong_scheme = ("https:" if origin.startswith("http:") else "http:") + origin.split(":", 1)[1]
        node.client.request("/api/info", headers={**headers, "Origin": wrong_scheme}, status=403)
    node.client.request("/api/info", headers={"Host": "unconfigured.invalid"}, origin=False, status=403)


def finish_native_change(client, prepared, method, *, connection="main", team=False, password=None):
    params = {"request": prepared["request"]} if method != "execution_service_register" else prepared["request"]
    response = client.identity_call(method, params, connection=connection)
    return client.command("finish_preparation", handle=prepared["handle"],
                          receipt=response["acceptance" if team else "committed"], password=password)


def register_execution_service(node, *, authority=None, policy_peers=()):
    authority = authority or node
    client = authority.client
    client.authenticate()
    client.command("open_recovery", envelope=authority.backup, password=authority.backup_password)
    handle = "execution-service-" + node.name
    trust_ref = "execution" if authority is node else "execution-" + node.name
    prepared = client.command("prepare_execution_service", handle=handle, trust_ref=trust_ref,
                              target_node_public_key=node.node_public_key,
                              source_project={"space": client.scope["space_id"], "resource": client.resource_id})
    committed = client.identity_call("execution_service_register", prepared["request"])
    registered = client.command("finish_preparation", handle=handle, receipt=committed["committed"],
                                password=authority.backup_password, retain_registration=True)
    require(committed["journal_delta"] == [*prepared["request"]["request"]["new_objects"],
            {"kind": "space_commit", "value": committed["committed"]}],
            "execution registration installs exactly its owner-approved resource, grant and commit")
    authority.backup = registered.pop("recovery_envelope")
    backup_path = authority.directory / "account-recovery.json"
    backup_path.write_bytes(bytes.fromhex(authority.backup))
    backup_path.chmod(0o600)
    for peer in policy_peers:
        connection = "execution-policy-" + node.name + "-" + peer.name
        client.command("connect", url=peer.url, node_id=peer.node_id, node_public_key=peer.node_public_key,
                       connection=connection)
        require(client.identity_call("policy_commit", {"journal": committed["journal_delta"]},
                    connection=connection)["accepted"], "target imports the verified execution registration delta")
    connection = "execution-install-" + node.name
    client.command("connect", url=node.url, node_id=node.node_id, node_public_key=node.node_public_key,
                   connection=connection)
    installation = client.command("prepare_execution_service_install", handle=handle, connection=connection,
                                  canonical_root=str(node.workspace))
    require(installation["request"]["registration"] == prepared["request"]
            and installation["request"]["committed"] == committed["committed"]
            and installation["request"]["intent"]["payload"]["target_node_public_key"] == node.node_public_key
            and installation["request"]["intent"]["payload"]["canonical_root"] == str(node.workspace),
            "native installation binds the registered target and its configured canonical root")
    tampered = copy.deepcopy(installation["request"])
    tampered["intent"]["payload"]["canonical_root"] = str(node.workspace.parent)
    client.identity_call("execution_service_install", tampered, status=400, connection=connection)
    installed = client.identity_call("execution_service_install", installation["request"], connection=connection)
    require(installed["installed"] is True and installed["request_hash"] == installation["request_hash"]
            and installed["execution_resource"] == {
                "space": client.scope["space_id"], "resource": registered["resource_id"]},
            "physical target installs the exact signed resource and root after rejecting a changed root")
    require(client.identity_call("execution_service_install", installation["request"], connection=connection) == installed,
            "repeating the same signed installation returns the original receipt")
    client.command("release_execution_registration", handle=handle)
    execution_connection = "execution" if authority is node else "shared-execution-" + node.name
    client.command("connect", url=node.url, node_id=node.node_id, node_public_key=node.node_public_key,
                   connection=execution_connection, trust_ref=trust_ref)
    client.authenticate(resource_id=registered["resource_id"], host_public_key=node.node_public_key,
                        connection=execution_connection, trust_ref=trust_ref, resource_kind="execution_service",
                        grant_leaf=registered["grant_leaf"], activate=False)
    client.authenticate()
    return registered


def create_execution_share(project, executor, registered, conversation, task, session, *, purpose="execution", previous=None):
    source, owner = project.client, executor.client
    params = {"session_id": session, "executor_kind": "claude_code",
              "expires_at_ms": int(time.time() * 1000) + 120_000, "purpose": purpose}
    if previous:
        params.update(previous)
    produced = source.rpc("execution_share_request", conversation_id=conversation, task_id=task,
                          task_ref=source.tasks[(source.scope["space_id"], source.resource_id, conversation, task)]["task_ref"],
                          share_request=params)
    require(produced["source_node_public_key"] == project.node_public_key
            and produced["session_id"] == session and produced["purpose"] == purpose,
            "compact share request retains the independently selected Project, session and purpose")
    intent = source.command("build_share_intent", intent=produced)
    source_ref = "source-" + uuid.uuid4().hex
    owner.command("save_trust", trust_ref=source_ref, trust=source.command("get_trust"))
    owner.command("connect", url=project.url, node_id=project.node_id, node_public_key=project.node_public_key,
                  connection=source_ref, trust_ref=source_ref)
    source_handle = "source-policy-" + uuid.uuid4().hex
    owner.command("read_share_policy", handle=source_handle, connection=source_ref,
                  source_trust_ref=source_ref, intent=intent)
    actions = {"execution": ["execution_open", "execution_send", "operation_status_read"],
               "resolution": ["execution_resolve", "operation_resolve"],
               "rebind": ["execution_rebind", "operation_status_read"]}[purpose]
    prepared = owner.command("prepare_share", handle="share-" + uuid.uuid4().hex,
                             connection="execution", source_handle=source_handle,
                             intent={"executionResourceId": registered["resource_id"], "actions": actions,
                                     "expiresAt": int(time.time() * 1000) + 3_600_000})
    finished = finish_native_change(owner, prepared, "space_policy_commit", connection="execution")
    owner.authenticate(resource_id=registered["resource_id"], host_public_key=executor.node_public_key,
                       connection="execution", trust_ref="execution", resource_kind="execution_service",
                       grant_leaf=registered["grant_leaf"], activate=False)
    return finished["share"]


def primary_policy_domains(node):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
        return set(database.execute("SELECT domain_kind,domain_id FROM current_policy_heads"))


def seal_remote_execution(project, executor, registered, remote, conversation, task, session):
    source, owner = project.client, executor.client
    current = source.rpc("session", target_node=executor.node_id, session_id=session, remote=remote)
    receipt = current["receipt"]
    binding = source.bindings[(source.scope["space_id"], source.resource_id, conversation, task, session)]
    share = create_execution_share(project, executor, registered, conversation, task, session, purpose="rebind",
                                   previous={"execution_resource": remote["share"]["selection"]["execution_resource"],
                                             "target_public_key": executor.node_public_key, "old_binding": binding,
                                             "old_execution_receipt": receipt})
    execution_trust = owner.command("get_trust", trust_ref="execution")
    account_pin = next(pin for pin in source.command("get_trust")["pins"]
                       if pin["domain"] == {"kind": "account", "id": source.scope["account_id"]})
    require(not any(pin["domain"] == account_pin["domain"] for pin in execution_trust["pins"]),
            "independent executor account differs from the Project Account")
    seal_trust = {"scope": {**source.scope, "space_id": share["selection"]["execution_resource"]["space"]},
                  "pins": sorted(execution_trust["pins"] + [account_pin],
                                 key=lambda pin: (pin["domain"]["kind"], pin["domain"]["id"]))}
    source.command("save_trust", trust_ref="remote-seal", trust=seal_trust)
    source.command("connect", connection="remote-seal", url=executor.url, node_id=executor.node_id,
                   node_public_key=executor.node_public_key, trust_ref="remote-seal")
    source.authenticate(connection="remote-seal", trust_ref="remote-seal", resource_kind="execution_service",
                        resource_id=share["selection"]["execution_resource"]["resource"],
                        grant_leaf=share["selection"]["execution_grant"], host_public_key=executor.node_public_key, activate=False)
    body = {"method": "seal_remote_execution", "space_id": share["selection"]["execution_resource"]["space"],
            "resource_id": share["selection"]["execution_resource"]["resource"], "authorization_id": "remote-seal",
            "original_space_id": source.scope["space_id"], "operation_id": receipt["payload"]["operation_id"],
            "binding": binding, "execution_receipt": receipt}
    raw = {"context": {"conversation": conversation, "task": binding["binding"]["task_ref"], "sdk_session": None,
                       "execution_session": {"target": share["context"]["execution_session"]["target"], "session_id": session},
                       "executor_kind": binding["binding"]["open_spec"]["executor"], "workspace_resource": body["resource_id"]},
           "binding_generation": binding["binding"]["generation"], "canonical_cwd": None,
           "execution_target_public_key": executor.node_public_key}
    proof = source.command("prepare_signed", connection="remote-seal", body=body, raw_context=raw)
    private_json(project.directory.parent / "remote-original-seal-request.json", proof)
    sealed = source.call("rpc", proof["body"], invocation=proof["invocation"], connection="remote-seal")
    seal = source.command("verify_remote_seal", binding=binding, seal=sealed["seal"],
                          project_public_key=project.node_public_key, execution_public_key=executor.node_public_key,
                          revision=current["session"]["revision"])
    status_body = {**body, "method": "remote_execution_seal_status", "authorization_id": "remote-seal-status",
                   "seal_invocation_hash": source.command("signed_invocation_hash", invocation=proof["invocation"])}
    status = source.command("signed_request", connection="remote-seal", body=status_body, raw_context=raw)
    require(status.get("result", {}).get("seal") == seal, "seal status identifies the exact saved signed seal request")
    return seal


def prepare_independent_gateway(gateway, project, conversation):
    source = project.client
    source.authenticate()
    now = int(time.time() * 1000)
    route = {"version": 2, "route_id": os.urandom(32).hex(), "generation": 1,
             "gateway": gateway.node_public_key, "client_account": source.scope["account_id"],
             "client_key": source.command("identity")["public_key"], "target_key": project.node_public_key,
             "resource": {"space": source.scope["space_id"], "resource": source.resource_id},
             "allowed_actions": ["conversation_read", "message_append", "execution_open", "execution_send",
                                 "execution_rebind", "operation_status_read"],
             "source_pins": source.command("get_trust")["pins"], "state": "active",
             "issued_at_ms": now, "expires_at_ms": now + 3_600_000}
    approval = gateway.client.command("sign_gateway_route", payload=route)
    card = {"version": 2, "kind": "vibeface-gateway",
            "gateway": {"url": gateway.url, "node_id": gateway.node_id,
                        "node_public_key": gateway.node_public_key, "relay": False}, "approval": approval}
    original_domains = primary_policy_domains(gateway)
    attached = source.attach_gateway(card, refresh=False)
    expected_hash = source.command("gateway_route_hash", approval=approval)
    require(attached["gateway"]["approval_hash"] == expected_hash,
            "Gateway installation matches the locally verified owner-signed approval hash")
    direct = source.rpc("conversation", conversation_id=conversation)
    forwarded = source.rpc("conversation", conversation_id=conversation, connection="project-gateway")
    require(forwarded == direct, "independent A returns the exact B conversation through a verified reply")
    source.attach_gateway(card, refresh=False)
    for field, replacement in (("gateway", project.node_public_key), ("client_key", gateway.node_public_key),
                               ("client_account", gateway.client.scope["account_id"]),
                               ("target_key", gateway.node_public_key)):
        changed = copy.deepcopy(card)
        changed["approval"]["payload"][field] = replacement
        rejected = source.command("attach_gateway", connection="project-gateway", source_connection="main",
                                  card=changed, check=False)
        require(not rejected["ok"], f"Gateway card rejects substituted {field} before forwarding")
    source.rpc("message", connection="project-gateway", conversation_id=conversation,
               message_id="independent-gateway-message", text="U selects A and retains B policy authority.")
    require(sum(item["message_id"] == "independent-gateway-message"
                for item in source.rpc("conversation", conversation_id=conversation)["messages"]) == 1,
            "independent Gateway writes exactly once at B")
    require(primary_policy_domains(gateway) == original_domains,
            "A route and foreign source import do not install B primary policy")
    return card, original_domains


def finish_independent_gateway(gateway, project, executor, card, original_domains, conversation):
    source = project.client
    require(primary_policy_domains(gateway) == original_domains,
            "A kept its own primary domains after B and C execution")
    attached = source.attach_gateway(card)
    gateway.stop()
    gateway.start()
    reconnected = source.command("reconnect_gateway", connection="project-gateway")
    require(reconnected["source_ref"] == attached["gateway"]["source_ref"],
            "A restart reuses the original source reference without reupload")
    source.rpc("conversation", conversation_id=conversation, connection="project-gateway")
    require(primary_policy_domains(gateway) == original_domains,
            "A restores foreign route originals without seeding B or C primary policy")
    revoked = copy.deepcopy(card["approval"]["payload"])
    revoked.update(generation=revoked["generation"] + 1, state="revoked",
                   issued_at_ms=int(time.time() * 1000))
    approval = gateway.client.command("sign_gateway_route", payload=revoked)
    committed = source.call("gateway_source", {"stage": "route", "approval": approval}, connection="project-gateway")
    require(committed["approval"] == source.command("gateway_route_hash", approval=approval),
            "A persists the exact owner-signed route revocation")
    result = source.command("signed_request", connection="project-gateway",
                            body={"method": "conversation", "space_id": source.scope["space_id"],
                                  "resource_id": source.resource_id, "conversation_id": conversation,
                                  "authorization_id": uuid.uuid4().hex}, check=False)
    require(not result["ok"], "revoked A route does not silently fall back to direct B")
    with sqlite3.connect(f"file:{gateway.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
        rows = {table: database.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
                for table in ("events", "conversations", "messages", "tasks", "operations")}
    require(not any(rows.values()), "A has no B business history or C operation outbox")
    require(len({source.scope["account_id"], gateway.client.scope["account_id"], executor.client.scope["account_id"]}) == 3,
            "A, B and C retain independent account owners")
    return {"gateway": gateway.node_id, "project": project.node_id, "executor": executor.node_id,
            "approval_hash": source.command("gateway_route_hash", approval=card["approval"]),
            "primary_domains": sorted(original_domains), "gateway_business_rows": rows,
            "checks": ["owner-signed route and local hash", "B original source upload and exact retry",
                       "A to B signed read and write", "A to B to C execution and receipts",
                       "A restart restores source", "owner revocation refuses forwarding without fallback"]}


def readonly_context(source, binding, target_key, *, resolver=False):
    value = binding["binding"]
    return {"context": {"conversation": value["conversation_id"], "task": value["task_ref"],
                        "sdk_session": None,
                        "execution_session": {"target": source.command("derive_authorization_id", kind="device", value=target_key),
                                              "session_id": value["session_id"]},
                        "executor_kind": value["open_spec"]["executor"], "workspace_resource": source.resource_id},
            "binding_generation": value["generation"],
            "canonical_cwd": value["open_spec"]["cwd"] if resolver else None,
            "execution_target_public_key": target_key}


def read_resolution_policy(source, remote, binding, *, resolver=False, check=True):
    context = readonly_context(source, binding, remote["share"]["selection"]["target_public_key"], resolver=resolver)
    handle = "readonly-" + uuid.uuid4().hex
    result = source.command("read_remote_policy", handle=handle, connection="main", source_connection="main",
                            execution_connection=remote["execution_connection"],
                            execution_trust_ref=remote["trust_ref"], selection=remote["share"]["selection"],
                            purpose="resolver" if resolver else "observation_read", action="operation_resolve",
                            raw_context=context, check=check)
    return handle, result


def executor_original(node, space, operation_id):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
        row = database.execute(
            "SELECT original_request,handoff,handoff_hash,invocation_hash,body_hash,admission_transport,"
            "admission_challenge,admission_receipts,admission_receipt,admitted_at_ms "
            "FROM execution_handoffs WHERE original_space_id=? AND operation_id=?", (space, operation_id)).fetchone()
    require(row is not None, "C retained the original signed handoff and admission")
    return row


def persisted_operation_state(node, space, operation_id, *, executor=False):
    table, column = ("execution_handoffs", "original_space_id") if executor else ("operations", "space_id")
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
        row = database.execute(f"SELECT state FROM {table} WHERE {column}=? AND operation_id=?",
                               (space, operation_id)).fetchone()
    raw = row[0] if row else None
    return json.loads(raw) if raw is not None and not executor else raw


def revoke_execution_share(executor, registered, share):
    owner = executor.client
    prepared = owner.command("prepare_revoke_grant", handle="revoke-" + uuid.uuid4().hex,
                             connection="execution", grant=share["selection"]["execution_grant"])
    finish_native_change(owner, prepared, "space_policy_commit", connection="execution")
    owner.authenticate(resource_id=registered["resource_id"], host_public_key=executor.node_public_key,
                       connection="execution", trust_ref="execution", resource_kind="execution_service",
                       grant_leaf=registered["grant_leaf"], activate=False)


def observation_protected_state(run, project, executor, space, operation_id, label):
    state = {}
    for node, tables in ((project, (("accepted_authorizations", "space_id", False),
                                    ("operations", "space_id", True), ("resource_reservations", "space_id", True))),
                         (executor, (("execution_handoffs", "original_space_id", True),
                                     ("execution_handoff_reservations", "original_space_id", True)))):
        with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
            for table, scope_column, scoped_operation in tables:
                where = f"{scope_column}=?" + (" AND operation_id=?" if scoped_operation else "")
                parameters = (space, operation_id) if scoped_operation else (space,)
                state[(node.name, table)] = database.execute(
                    f"SELECT * FROM {table} WHERE {where} ORDER BY rowid", parameters).fetchall()
    state["provider_invocations"] = len(invocations(run, label))
    return state


def verify_observation_rejections(run, project, executor, outsider, label, operation_id, observation_body):
    source = project.client
    altered_target = copy.deepcopy(observation_body)
    altered_target["observation_request"]["payload"]["intent"]["target"]["source"]["operation_id"] += "-tampered"
    altered_signature = copy.deepcopy(observation_body)
    signature = altered_signature["observation_request"]["signature"]
    altered_signature["observation_request"]["signature"] = ("0" if signature[0] != "0" else "1") + signature[1:]
    outsider.client.command("connect", connection="readonly-other-reader", url=project.url,
                            node_id=project.node_id, node_public_key=project.node_public_key)
    attempts = (("tampered_original_operation", source, "main", altered_target),
                ("tampered_signature", source, "main", altered_signature),
                ("independent_account_transport", outsider.client, "readonly-other-reader", observation_body))
    for name, client, connection, body in attempts:
        before = observation_protected_state(run, project, executor, source.scope["space_id"], operation_id, label)
        client.call("rpc", body, connection=connection, status=400)
        require(observation_protected_state(run, project, executor, source.scope["space_id"], operation_id, label) == before,
                name + " rejects without changing admissions, operations, reservations or provider invocations")
    return [name for name, _, _, _ in attempts]


def verify_remote_readonly(run, project, executor, outsider, registered, *, connection="main"):
    source, owner = project.client, executor.client
    records = []
    for terminal in (False, True):
        label = "readonly-terminal" if terminal else "readonly-unknown"
        conversation, task, session = label, label + "-task", label + "-session"
        source.rpc("create_conversation", conversation_id=conversation, title=label)
        source.create_task(project, conversation, task, title=label, goal="Read and reconcile the original remote operation",
                           acceptance="Revocation and reconciliation never repeat the provider")
        share = create_execution_share(project, executor, registered, conversation, task, session)
        source.command("save_trust", trust_ref="remote-execution", trust=owner.command("get_trust", trust_ref="execution"))
        writer = {"share": share, "trust_ref": "remote-execution", "source_connection": "main",
                  "execution_connection": "remote-execution", "generation": 1, "approve": True}
        open_spec = {"type": "open", "executor": "claude_code", "cwd": str(project.directory.parent / "workspace")}
        binding = source.bind_execution(conversation, task, executor, session, open_spec, remote=writer, connection=connection)
        opened = request(source.scope["space_id"], conversation, executor, label + "-open", session, open_spec, task=task)
        opened["binding"] = binding
        source.rpc("operate", request=opened, remote=writer, connection=connection)
        wait_for("readonly fixture session opens", lambda: source.rpc("operation", target_node=executor.node_id,
                 operation_id=opened["operation_id"], remote=writer, connection=connection)["operation"]["state"] == "succeeded")
        current = source.rpc("session", target_node=executor.node_id, session_id=session, remote=writer,
                             connection=connection)["session"]
        reader_share = create_execution_share(project, executor, registered, conversation, task, session, purpose="resolution")
        reader = {**writer, "share": reader_share, "approve": False}
        source.command("save_trust", trust_ref="remote-execution", trust=owner.command("get_trust", trust_ref="execution"))
        gate = run / (label + "-complete")
        sent = request(source.scope["space_id"], conversation, executor, label + "-send", session,
                       {"type": "send", "text": json.dumps({"label": label, "gate": str(gate)})},
                       revision=current["revision"], task=task)
        sent.update(binding=binding, resources=[open_spec["cwd"]])
        source.rpc("operate", request=sent, remote=writer, connection=connection)
        wait_for("readonly fixture starts exactly once", lambda: len(invocations(run, label)) == 1)
        initial = wait_for("original C receipt is available", lambda: source.rpc("operation", target_node=executor.node_id,
                           operation_id=sent["operation_id"], remote=writer, connection=connection).get("actual"))
        original = initial["handoff"]
        original_request = copy.deepcopy(source.signed_requests[sent["operation_id"]])
        admission = admitted_scope(project, source, sent["operation_id"])
        accepted = executor_original(executor, source.scope["space_id"], sent["operation_id"])
        project.stop()
        if terminal:
            gate.touch(mode=0o600)
            wait_for("C commits terminal while B is stopped", lambda: persisted_operation_state(
                executor, source.scope["space_id"], sent["operation_id"], executor=True) == "succeeded")
        else:
            executor.stop()
            executor.start()
            owner.connect()
            owner.authenticate()
            owner.command("connect", url=executor.url, node_id=executor.node_id, node_public_key=executor.node_public_key,
                          connection="execution", trust_ref="execution")
            owner.authenticate(resource_id=registered["resource_id"], host_public_key=executor.node_public_key,
                               connection="execution", trust_ref="execution", resource_kind="execution_service",
                               grant_leaf=registered["grant_leaf"], activate=False)
        project.start()
        source.connect()
        source.authenticate()
        if connection != "main":
            source.attach_gateway(source.gateways[connection]["card"], connection=connection, refresh=False)
        source.command("connect", url=executor.url, node_id=executor.node_id, node_public_key=executor.node_public_key,
                       connection="remote-execution", trust_ref="remote-execution")
        require(persisted_operation_state(project, source.scope["space_id"], sent["operation_id"]) == "unknown",
                "B restart retains Unknown before an explicit account decision")
        revoke_execution_share(executor, registered, share)
        source.command("save_trust", trust_ref="remote-execution", trust=owner.command("get_trust", trust_ref="execution"))
        denied_writer = source.command("read_remote_policy", handle=label + "-revoked-writer", connection="main",
                                       source_connection="main", execution_connection="remote-execution",
                                       execution_trust_ref="remote-execution", selection=share["selection"],
                                       action="execution_send", raw_context=readonly_context(
                                           source, binding, executor.node_public_key, resolver=True), check=False)
        require(not denied_writer["ok"], "revoked C writer no longer yields a current execution policy")
        observer, _ = read_resolution_policy(source, reader, binding)
        try:
            signed = source.command("sign_observation_request", handle=observer, original=original,
                                    authorization_id=label + "-observe")
            observation_body = {"method": "execution_observation", "observation_request": signed}
            observed = source.call("rpc", observation_body)
            require(observed.get("remote_verified") and observed["original_handoff"] == original,
                    "direct B observation retains the exact original handoff after writer revocation")
            actual = source.command("verify_observation", handle=observer, actual=observed["actual"], binding=binding)
            expected_state = "succeeded" if terminal else "unknown"
            require(actual["handoff"] == original and actual["receipt"]["payload"]["state"] == expected_state,
                    "WASM verifies the C receipt for the exact original operation")
            repeated = source.call("rpc", observation_body)
            repeated_actual = source.command("verify_observation", handle=observer, actual=repeated["actual"], binding=binding)
            require(repeated_actual["handoff"] == original and repeated_actual["receipt"]["payload"]["state"] == expected_state,
                    "retrying the same signed observation only reads the original operation")
            rejected = verify_observation_rejections(run, project, executor, outsider, label,
                                                     sent["operation_id"], observation_body)
        finally:
            source.command("release_remote", handle=observer)
        require(persisted_operation_state(project, source.scope["space_id"], sent["operation_id"]) == "unknown",
                "Observation does not resolve B or release its original reservation")
        resolver, _ = read_resolution_policy(source, reader, binding, resolver=True)
        try:
            resolution = source.command("resolution_body", handle=resolver, actual=actual,
                                        authorization_id=label + "-resolve", outcome="succeeded" if terminal else "failed",
                                        evidence="The isolated executor receipt and process state were independently verified.")
            expected_purpose = "reconcile_terminal" if terminal else "resolve_unknown"
            require(resolution["body"]["resolution_purpose"] == expected_purpose,
                    "native resolution chooses the purpose from the signed C state")
            proof = source.command("prepare_signed", body=resolution["body"], raw_context=resolution["context"],
                                   remote_handle=resolver)
            require("error" not in proof, "B prepares the current independent resolver authorization")
            final = source.call("rpc", proof["body"], invocation=proof["invocation"])
            verified = source.command("verify_resolution_result", handle=resolver, actual=final["operation"]["result"], original=proof)
            require(verified["purpose"] == expected_purpose and verified["state"] == ("succeeded" if terminal else "failed"),
                    "WASM verifies C's signed final resolution against the original account decision")
            duplicate = source.call("rpc", proof["body"], invocation=proof["invocation"])
            require(duplicate == final, "the exact signed resolution retry returns the same terminal operation")
        finally:
            source.command("release_remote", handle=resolver)
        source.call("rpc", original_request["body"], invocation=original_request["invocation"], connection=connection, status=400)
        require(admitted_scope(project, source, sent["operation_id"]) == admission
                and executor_original(executor, source.scope["space_id"], sent["operation_id"]) == accepted
                and len(invocations(run, label)) == 1,
                "Observation, resolution and the rejected revoked-write retry preserve the original admission and one execution")
        for node, table, column in ((project, "resource_reservations", "space_id"),
                                    (executor, "execution_handoff_reservations", "original_space_id")):
            with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
                require(database.execute(f"SELECT COUNT(*) FROM {table} WHERE {column}=? AND operation_id=?",
                                         (source.scope["space_id"], sent["operation_id"])).fetchone()[0] == 0,
                        "confirmed resolution releases the original B and C reservations")
        audit = resolution_audit(project, source, sent["operation_id"])
        observer, _ = read_resolution_policy(source, reader, binding)
        try:
            retained = {"method": "execution_observation", "observation_request": source.command(
                "sign_observation_request", handle=observer, original=original, authorization_id=label + "-revoked-reader")}
        finally:
            source.command("release_remote", handle=observer)
        revoke_execution_share(executor, registered, reader_share)
        source.call("rpc", retained, status=400)
        source.command("save_trust", trust_ref="remote-execution", trust=owner.command("get_trust", trust_ref="execution"))
        _, denied_reader = read_resolution_policy(source, reader, binding, check=False)
        require(not denied_reader["ok"] and len(invocations(run, label)) == 1,
                "revoked C reader rejects both its retained signature and a fresh observation policy")
        private_json(run / (label + "-resolution.json"), {"original": original_request, "observation": observation_body,
                     "resolution": proof, "final": final, "resolution_admission": audit})
        records.append({"operation_id": sent["operation_id"], "purpose": expected_purpose,
                        "writer_grant": share["selection"]["execution_grant"],
                        "reader_grant": reader_share["selection"]["execution_grant"],
                        "executor_state": verified["state"], "provider_invocations": len(invocations(run, label)),
                        "rejections": rejected,
                        "original_write_retry": "rejected_after_writer_revocation",
                        "evidence": label + "-resolution.json"})
    return records


def verify_remote_topology(run, binary, workspace, fixture, helper, module, nodes, *, gateway=False):
    group = [Node(run, name, binary, workspace, fixture, helper, module)
             for name in ("REMOTE_A", "REMOTE_B", "REMOTE_C")]
    nodes.extend(group)
    for index in (1, 2, 0):
        node = group[index]
        configuration = {"network": {"bootstrap_urls": [group[1].url.replace("http://", "ws://") + "/api/network/ws"]
                                      if gateway and index == 0 else [],
                                      "advertise_urls": [node.url.replace("http://", "ws://") + "/api/network/ws"],
                                      "mdns": False, "relay_enabled": True}}
        if gateway and index == 0:
            configuration["roles"] = ["gateway", "replica", "relay"]
        node.configure(**configuration)
        node.start()
        node.bootstrap()
    outsider, project, executor = group
    require(len({node.client.scope["account_id"] for node in group}) == 3,
            "remote acceptance starts three independently claimed empty databases")
    source, owner = project.client, executor.client
    outsider.client.command("connect", connection="foreign-project", url=project.url, node_id=project.node_id,
                            node_public_key=project.node_public_key)
    denied = outsider.client.command("authenticate", connection="foreign-project", check=False,
                                     expected_host_public_key=project.node_public_key, resource_id=source.resource_id)
    require(denied.get("ok") is False and denied.get("rpc_method") == "begin_client_policy_read",
            "B refuses to authenticate an independent Account absent from its Project policy")
    source_domains, executor_domains = primary_policy_domains(project), primary_policy_domains(executor)
    registered = register_execution_service(executor)
    conversation, task, session = "remote-project", "remote-task", "remote-session"
    source.rpc("create_conversation", conversation_id=conversation, title="Remote task")
    source.create_task(project, conversation, task, title="Remote execution", goal="Execute on an independent Node",
                       acceptance="The signed result survives restart without replay")
    share = create_execution_share(project, executor, registered, conversation, task, session)
    require(primary_policy_domains(project) == source_domains,
            "B received no C primary policy while C issued the precise Task share")
    require(not {("account", source.scope["account_id"]), ("space", source.scope["space_id"])}
            & primary_policy_domains(executor), "C share creation did not seed B business policy")
    source.command("save_trust", trust_ref="remote-execution", trust=owner.command("get_trust", trust_ref="execution"))
    source.command("connect", url=executor.url, node_id=executor.node_id, node_public_key=executor.node_public_key,
                   connection="remote-execution", trust_ref="remote-execution")
    remote = {"share": share, "trust_ref": "remote-execution", "source_connection": "main",
              "execution_connection": "remote-execution", "generation": 1, "approve": True}
    gateway_card, gateway_domains = prepare_independent_gateway(outsider, project, conversation) if gateway else (None, None)
    connection = "project-gateway" if gateway else "main"
    open_spec = {"type": "open", "executor": "claude_code", "cwd": str(workspace)}
    source.bind_execution(conversation, task, executor, session, open_spec, remote=remote, connection=connection)
    opened = request(source.scope["space_id"], conversation, executor, "remote-open", session, open_spec, task=task)
    opened["binding"] = source.bindings[(source.scope["space_id"], source.resource_id, conversation, task, session)]
    source.rpc("operate", request=opened, remote=remote, connection=connection)

    def remote_operation(operation_id):
        return source.rpc("operation", target_node=executor.node_id, operation_id=operation_id, remote=remote, connection=connection)["operation"]

    wait_for("independent C completes remote open", lambda: remote_operation("remote-open")["state"] == "succeeded")
    current = source.rpc("session", target_node=executor.node_id, session_id=session, remote=remote, connection=connection)["session"]
    require(current["target_node"] == executor.node_id and current["binding_generation"] == 1,
            "native WASM verifies the C session receipt against the exact B binding")
    outsider.client.rpc("conversation", conversation_id=conversation, status=400)
    gate = run / "remote-complete"
    sent = request(source.scope["space_id"], conversation, executor, "remote-send", session,
                   {"type": "send", "text": json.dumps({"label": "remote-result", "gate": str(gate), "result_size": 128})},
                   revision=current["revision"], task=task)
    sent.update(binding=opened["binding"], resources=[str(workspace)])
    source.rpc("operate", request=sent, remote=remote, connection=connection)
    original = copy.deepcopy(source.signed_requests["remote-send"])
    private_json(run / "remote-original-request.json", original)
    wait_for("C starts one accepted remote send", lambda: len(invocations(run, "remote-result")) == 1)
    gate.touch(mode=0o600)
    wait_for("C commits the remote result", lambda: remote_operation("remote-send")["state"] == "succeeded")
    for node in (project, executor):
        node.stop()
        node.start()
    source.connect()
    source.authenticate()
    if gateway:
        source.attach_gateway(gateway_card, refresh=False)
    source.command("connect", url=executor.url, node_id=executor.node_id, node_public_key=executor.node_public_key,
                   connection="remote-execution", trust_ref="remote-execution")
    require(remote_operation("remote-send")["state"] == "succeeded" and len(invocations(run, "remote-result")) == 1,
            "B and C restart preserve the admitted remote result without replay")
    wait_for("restarted Project history retains the verified original C result", lambda:
             shared_history(source, conversation, task, sent, remote=remote, connection=connection))
    source.call("rpc", original["body"], invocation=original["invocation"], connection=connection)
    require(len(invocations(run, "remote-result")) == 1, "exact signed remote retry does not rerun C")
    changed = copy.deepcopy(original["body"])
    changed["request"]["session_id"] = "unapproved-session"
    if gateway:
        changed_result = source.command("request", method="rpc", params=changed, invocation=original["invocation"],
                                        connection=connection, check=False)
        require(not changed_result["ok"] or changed_result["result"].get("error"),
                "Gateway refuses an original signed request with a substituted session")
    else:
        source.call("rpc", changed, invocation=original["invocation"], status=400, connection=connection)
    require(len(invocations(run, "remote-result")) == 1, "a changed signed remote session cannot execute C")
    require(primary_policy_domains(project) == source_domains,
            "completed foreign import remains outside B primary policy")

    seal = seal_remote_execution(project, executor, registered, remote, conversation, task, session)
    next_session = "remote-session-2"
    next_share = create_execution_share(project, executor, registered, conversation, task, next_session)
    source.command("save_trust", trust_ref="remote-execution", trust=owner.command("get_trust", trust_ref="execution"))
    replacement = {**remote, "share": next_share, "generation": 2, "approve": True}
    rebound = source.rpc("rebind_execution", conversation_id=conversation, task_id=task,
                         task_ref=opened["binding"]["binding"]["task_ref"], binding=opened["binding"],
                         previous_seal=seal, previous_target_public_key=executor.node_public_key,
                         target_node=executor.node_id, session_id=next_session, executor="claude_code", cwd=str(workspace),
                         remote=replacement, connection=connection)
    expected = {**opened["binding"]["binding"], "generation": 2, "session_id": next_session,
                "previous_binding_hash": seal["seal"]["binding_hash"], "previous_seal": seal}
    next_binding = source.command("verify_task_binding", binding=rebound["binding"],
                                  authority_public_key=project.node_public_key, expected={"binding": expected})
    source.bindings[(source.scope["space_id"], source.resource_id, conversation, task, next_session)] = next_binding
    next_open = request(source.scope["space_id"], conversation, executor, "remote-rebound-open", next_session, open_spec, task=task)
    next_open["binding"] = next_binding
    source.rpc("operate", request=next_open, remote=replacement, connection=connection)
    wait_for("sealed Task opens only its next binding", lambda: source.rpc("operation", target_node=executor.node_id,
              operation_id="remote-rebound-open", remote=replacement, connection=connection)["operation"]["state"] == "succeeded")

    readonly = verify_remote_readonly(run, project, executor, outsider, registered, connection=connection)

    team_prepared = source.command("prepare_team", handle="remote-team", trust_ref="remote-team",
                                   source_project=source.resource_id)
    team = finish_native_change(source, team_prepared, "team_create", team=True, password=project.backup_password)
    source.command("connect", url=project.url, node_id=project.node_id, node_public_key=project.node_public_key,
                   connection="team", trust_ref="remote-team")
    source.authenticate(resource_id=team["resource_id"], connection="team", trust_ref="remote-team",
                        host_public_key=project.node_public_key, activate=False)
    source.rpc("create_conversation", connection="team", conversation_id="team-only", title="Team isolation")
    source.rpc("conversation", conversation_id="team-only", status=400)
    gateway_report = finish_independent_gateway(outsider, project, executor, gateway_card, gateway_domains, conversation) if gateway else None
    return {"gateway": gateway_report, "accounts": [node.client.scope["account_id"] for node in group],
            "nodes": [node.node_id for node in group], "execution_resource": registered["resource_id"],
            "source_domains_before": sorted(source_domains), "executor_domains_before": sorted(executor_domains),
            "team_space": team["scope"]["space_id"], "original_request": "remote-original-request.json", "readonly": readonly,
            "checks": ["B has no C primary policy", "C has no B account seed", "native bounded source and foreign policy imports",
                       "remote open/send receipts", "exact duplicate after B/C restart", "native seal and exact seal status",
                       "generation-2 rebind and new session open", "independent account rejection", "Team isolation",
                       "independent ObservationRead and Resolver grants", "writer and reader revocation",
                       "Unknown resolution and C-signed terminal reconciliation without replay"],
            "not_checked": ([] if gateway else ["cross-account authenticated A Gateway"]) + ["replacement on a different C node",
                            "SDK continuation without browser"]}

def scheduler_records(node, space, kind="job", *, resource_id=None):
    resource_id = resource_id or node.client.resource_id
    require(resource_id, "scheduler inspection requires a verified Project resource")
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        rows = connection.execute(
            "SELECT value FROM local_values WHERE space_id=? AND key LIKE ? "
            "AND json_extract(value,'$.resource_id')=?", (space, "scheduler:v1:" + kind + ":%", resource_id))
        return [json.loads(row[0]) for row in rows]


def scheduler_record(node, space, trigger, kind="job"):
    return next((row for row in scheduler_records(node, space, kind) if row["trigger"] == trigger), None)


def require_quiet(description, predicate):
    # Cover two complete three-second durable scheduler reconciliation intervals.
    deadline = time.monotonic() + 7
    while time.monotonic() < deadline:
        require(predicate(), description)
        time.sleep(0.2)


def local_operation(node, space, operation_id, *, resource_id=None):
    resource_id = resource_id or node.client.resource_id
    require(resource_id, "operation inspection requires a verified Project resource")
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        row = connection.execute(
            "SELECT o.request,o.state,o.result FROM operations o "
            "JOIN accepted_authorizations a ON a.space_id=o.space_id AND a.authorization_id=o.operation_id "
            "WHERE o.space_id=? AND o.operation_id=? AND a.resource_id=?",
            (space, operation_id, resource_id)).fetchone()
        if row:
            return {"request": json.loads(row[0]), "state": json.loads(row[1]), "result": json.loads(row[2])}
        row = connection.execute(
            "SELECT original_request,state,result,handoff,result_receipt FROM execution_handoffs "
            "WHERE execution_space_id=? AND original_space_id=? AND operation_id=? "
            "AND json_extract(handoff,'$.handoff.source_attestation.payload.project.resource')=?",
            (node.client.scope["space_id"], space, operation_id, resource_id)).fetchone()
    if not row:
        return None
    request, result, wire = json.loads(row[0]), json.loads(row[2]), json.loads(row[3])
    source = wire["handoff"]["source_attestation"]["payload"]
    event = json.loads(wire["handoff"]["project_dispatch"]["canonical_body"])
    require(source["project"] == {"space": space, "resource": resource_id}
            and source["operation_id"] == operation_id and request["operation_id"] == operation_id
            and request["target_node"] == node.node_id
            and wire["handoff"]["target_public_key"] == node.node_public_key
            and wire["handoff"]["execution_resource"]["space"] == node.client.scope["space_id"]
            and event["space_id"] == space and event["resource_id"] == resource_id
            and event["cause"]["admission"]["source_body"]["request"] == request,
            "foreign executor row retains the exact admitted Project request and intended local target")
    work = source["work"]
    if work["kind"] == "command":
        require(wire["handoff"]["origin"]["kind"] == "command"
                and request["action"]["type"] == "command" and request["expected_revision"] == 0
                and all(request.get(field) is None for field in ("task_id", "session_id", "binding")),
                "sessionless Command has no fabricated Task, binding or SDK session")
    elif work["kind"] == "conversation_session":
        require(wire["handoff"]["origin"]["kind"] in {"conversation_open", "conversation_session"}
                and request["session_id"] == work["execution_session"]["session_id"]
                and request.get("task_id") is None and request.get("binding") is None,
                "ordinary conversation session preserves its original session without a Task")
    operation_record = {"request": request, "state": row[1], "result": result}
    if row[1] in {"succeeded", "failed", "rejected"}:
        require(row[4] is not None, "terminal foreign operation has its C-signed result receipt")
        signed = json.loads(row[4])
        receipt = signed["payload"]
        require(signed["issuer"] == node.node_public_key and receipt["work"] == work
                and receipt["operation_id"] == operation_id and receipt["state"] == row[1],
                "terminal foreign receipt keeps the intended signer, Work, operation and state")
        status = {"operation": operation_record, "actual": {"handoff": wire, "receipt": signed, "result": result},
                  "execution_source": wire["handoff"]["source_attestation"]}
        verified = node.client.command("verify_manager_execution", status=status,
            target_public_key=node.node_public_key, operation_id=operation_id)
        require(verified["request"] == request, "offline native verification binds the stored terminal result hash")
    return operation_record


def local_events(node, space, conversation, *, resource_id=None):
    resource_id = resource_id or node.client.resource_id
    require(resource_id, "event inspection requires a verified Project resource")
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        rows = connection.execute(
            "SELECT envelope FROM events WHERE space_id=? AND resource_id=? AND scope_id=? AND status='ready'",
            (space, resource_id, conversation)).fetchall()
    events = [json.loads(json.loads(row[0])["canonical_body"]) for row in rows]
    require(all(event["space_id"] == space and event["resource_id"] == resource_id
                and event["scope_id"] == conversation for event in events),
            "durable ready events retain their exact Space, Project and conversation")
    return events


def chat_agent_checks(run, workspace, a, b, c, space, passed):
    def calls(name, autonomous=None):
        return [record for record in fixture_records(run) if record["conversation_id"] == name
                and (autonomous is None or record["autonomous"] == autonomous)]

    def send(name, label, **directive):
        text = json.dumps({"chat_agent": True, "label": label, **directive})
        receipt = a.client.rpc("message", conversation_id=name, message_id=label, text=text,
                               report_results=directive.get("report_results", False))
        require(receipt["scheduler_node"] == a.node_id and receipt["accepted"],
                "automatic routing fixes the authorized Scheduler before accepting the message")
        return receipt

    def completed(receipt):
        job = scheduler_record(a, space, receipt["trigger"])
        if job and job["state"] in {"failed", "unknown", "cancelled"}:
            raise RuntimeError("chat fixture job reached " + job["state"] + ": " + job.get("detail", ""))
        return job if job and job["state"] == "completed" else None

    def prepare(name):
        a.client.rpc("create_conversation", conversation_id=name, title=name)
        directory = workspace / name
        directory.mkdir()
        return directory

    def result(name, reference):
        records = [row["result"] for row in fixture_records(run, "manager-tools.jsonl")
                   if row["conversation_id"] == name and row.get("call_ref") == reference and row.get("result")]
        return records[-1] if records else None

    def assert_taskless(name):
        snapshot = c.client.rpc("conversation", conversation_id=name)
        require(snapshot["tasks"] == [], "ordinary actions create no business Task")
        require(all(item["request"].get("task_id") is None and item["request"].get("binding") is None
                    for item in snapshot["operations"]), "ordinary signed dispatches have no hidden Task binding")
        return snapshot

    name = "chat-command-dedup"
    directory = prepare(name)
    marker = directory / "count.txt"
    with sqlite3.connect(f"file:{b.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        before_sessions = connection.execute("SELECT COUNT(*) FROM execution_sessions").fetchone()[0]
    before_sdk = len(invocations(run))
    receipt = send(name, "command-once", tools=[
        {"name": "command", "call_ref": "command-once", "repeat": 2, "discard_first": True,
         "arguments": {"command": "printf once >> count.txt; printf command-output", "cwd": str(directory),
                       "target_node": b.node_id}},
        {"name": "call_lookup", "arguments": {"call_ref": "command-once"}},
        {"name": "command", "call_ref": "command-once", "accepted": False,
         "rejection": "call reference reused with different tool arguments",
         "arguments": {"command": "printf changed >> count.txt", "cwd": str(directory), "target_node": b.node_id}}])
    wait_for("command management round commits", lambda: completed(receipt))
    prepared = result(name, "command-once")
    require(prepared and len(prepared["operations"]) == 1, "command prepares one durable operation")
    operation_id = prepared["operations"][0]["operation_id"]
    record = wait_for("sessionless command completes", lambda: succeeded(a.client, b, operation_id))
    require(record["request"]["session_id"] is None and record["request"]["expected_revision"] == 0,
            "command stores an absent session and revision zero")
    require(record["result"]["exit_code"] == 0 and record["result"]["stdout"] == "command-output"
            and marker.read_text() == "once", "lost command response retry has one exact file effect and output")
    with sqlite3.connect(f"file:{b.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        require(connection.execute("SELECT COUNT(*) FROM execution_sessions").fetchone()[0] == before_sessions,
                "command creates no ExecutionSession")
    require(len(invocations(run)) == before_sdk, "command never launches the SDK fixture")
    wait_for("third entry imports the command", lambda:
             any(item["operation_id"] == operation_id for item in c.client.rpc("conversation", conversation_id=name)["executions"]))
    assert_taskless(name)
    passed("sessionless command survives discarded response retry with one side effect, no Task and no SDK session",
           operation_id=operation_id)

    name = "chat-sandbox"
    directory = prepare(name)
    outside = run / "outside-command-root"
    outside.mkdir()
    secret = outside / "private.txt"
    secret.write_text("outside-must-not-leak")
    escaped = outside / "escaped.txt"
    receipt = send(name, "sandbox-denials", tools=[
        {"name": "command", "call_ref": "deny-read", "wait_terminal": True, "arguments": {
            "command": "cat " + shlex.quote(str(secret)), "cwd": str(directory), "target_node": b.node_id}},
        {"name": "command", "call_ref": "deny-write", "arguments": {
            "command": "printf escaped > " + shlex.quote(str(escaped)), "cwd": str(directory), "target_node": b.node_id}}])
    wait_for("sandbox management round commits", lambda: completed(receipt))
    for reference in ("deny-read", "deny-write"):
        operation_id = result(name, reference)["operations"][0]["operation_id"]
        record = wait_for("sandbox rejects an actual outside-root effect", lambda:
            (lambda row: row if row and row["state"] == "failed" else None)(operation(a.client, b, operation_id)))
        require(record["result"].get("exit_code") != 0 and
                "outside-must-not-leak" not in json.dumps(record["result"]),
                "sandbox refuses the outside file and does not reveal its content")
    require(secret.read_text() == "outside-must-not-leak" and not escaped.exists(),
            "sandbox leaves both outside-root files unchanged")
    passed("real command child cannot read or write outside the allowed root")

    name = "chat-delegate-report"
    directory = prepare(name)
    gate = directory / "release-executor"
    manager_gate = directory / "release-manager"
    receipt = send(name, "delegate-report", report_results=True, gate=str(manager_gate), dispatch={
        "label": "taskless-report", "target_node": b.node_id, "cwd": str(directory), "gate": str(gate)})
    wait_for("taskless SDK turn starts", lambda: len(invocations(run, "taskless-report")) == 1)
    prepared = wait_for("delegate records its durable dispatch response", lambda:
        (lambda value: value if value and value.get("all_dispatched") else None)(
            result(name, "delegate-taskless-report")))
    require(prepared["all_dispatched"] and len(prepared["operations"]) == 2,
            "delegate freezes exactly one open and one send")
    send_id = prepared["operations"][-1]["operation_id"]
    gate.touch()
    wait_for("ordinary delegation completes", lambda: succeeded(a.client, b, send_id))
    manager_gate.touch()
    wait_for("originating management round commits", lambda: completed(receipt))
    wait_for("taskless result returns once to the original conversation", lambda:
        len(calls(name, True)) == 1 and any(job["kind"] == "autonomous" and job["state"] == "completed"
            for job in scheduler_records(a, space) if job["conversation_id"] == name))
    require(calls(name, True)[0]["read_only"] and len(invocations(run, "taskless-report")) == 1,
            "ordinary completion cannot gain write authority or rerun the executor")
    require_quiet("completion replay does not create a second callback", lambda: len(calls(name, True)) == 1)
    require(not (workspace / "forbidden-followup").exists(), "read-only result callback cannot execute a command")
    wait_for("third entry projects ordinary execution", lambda:
        any(item["operation_id"] == send_id for item in c.client.rpc("conversation", conversation_id=name)["executions"]))
    assert_taskless(name)
    passed("ordinary delegation reports once in its original conversation and callback writes are denied",
           operation_id=send_id, trigger=receipt["trigger"])

    name = "chat-command-restart"
    directory = prepare(name)
    marker = directory / "count.txt"
    receipt = send(name, "command-running", tools=[{"name": "command", "call_ref": "running-command", "arguments": {
        "command": "printf once >> count.txt; while [ ! -e release ]; do sleep 0.1; done", "cwd": str(directory),
        "target_node": b.node_id, "timeout_secs": 600}}])
    wait_for("long command has crossed its side-effect boundary", lambda: marker.exists() and marker.read_text() == "once")
    wait_for("long command management round commits", lambda: completed(receipt))
    operation_id = result(name, "running-command")["operations"][0]["operation_id"]
    b.stop()
    (directory / "release").touch()
    b.start()
    b.client.connect()
    b.client.authenticate()
    wait_for("interrupted command remains unknown", lambda:
        (lambda row: row if row and row["state"] == "unknown" else None)(local_operation(b, space, operation_id, resource_id=a.client.resource_id)))
    require_quiet("restart never reruns the unknown command", lambda: marker.read_text() == "once")
    require(local_operation(b, space, operation_id, resource_id=a.client.resource_id)["request"]["session_id"] is None,
            "unknown command recovery does not create an SDK session")
    wait_for("restarted executor reconnects before subsequent work", lambda:
        any(row["node_id"] == b.node_id and row["online"] and row.get("capabilities")
            for row in a.client.rpc("nodes")["nodes"]))
    passed("running command restart preserves Unknown and its original file effect without replay", operation_id=operation_id)


def bot_checks(run, workspace, a, b, c, space, passed, outsider):
    definitions = [{"bot_id": "bot-" + label, "conversation_id": "bot-history-" + label,
                    "name": label.capitalize(), "instructions": "bot-" + label + "-instructions-sentinel",
                    "project": {"target_node": b.node_id, "cwd": str(workspace)}} for label in ("alpha", "beta")]
    stored = []
    before_calls = len(fixture_records(run))
    for definition in definitions:
        original = a.client.rpc("create_bot", **definition)["bot"]
        retry = a.client.rpc("create_bot", **definition)["bot"]
        require(original == retry and original["event_hash"], "identical Bot create retry returns the original signed definition")
        for key, replacement in (("name", "changed"), ("instructions", "changed"), ("conversation_id", "changed-history")):
            a.client.rpc("create_bot", **{**definition, key: replacement}, status=400)
        a.client.rpc("create_bot", **{**definition, "bot_id": definition["bot_id"] + "-duplicate"}, status=400)
        stored.append(original)
    require(sorted(a.client.rpc("bots")["bots"], key=lambda bot: bot["bot_id"]) == stored,
            "Bot listing contains the two exact persisted definitions")
    require(outsider.client.rpc("bots")["bots"] == [], "another personal space cannot list these Bots")
    independent = outsider.client.rpc("create_bot", **{**definitions[0], "name": "Other space", "project": None})["bot"]
    require(outsider.client.rpc("bots")["bots"] == [independent]
            and sorted(a.client.rpc("bots")["bots"], key=lambda bot: bot["bot_id"]) == stored,
            "the same Bot identity in another space cannot replace or reveal this space's definition")
    require_quiet("creating Bot definitions does not launch a model", lambda: len(fixture_records(run)) == before_calls)

    def send(definition, label):
        receipt = a.client.rpc("message", conversation_id=definition["conversation_id"], message_id=label,
                               text=json.dumps({"chat_agent": True, "label": label}))
        wait_for("Bot message gets one final reply", lambda:
            (scheduler_record(a, space, receipt["trigger"]) or {}).get("state") == "completed")
        return next(row for row in fixture_records(run) if row["trigger"] == receipt["trigger"])

    first = [send(definition, "bot-message-" + definition["bot_id"]) for definition in definitions]
    for definition, invocation in zip(definitions, first):
        require(invocation["bot_prompt_markers"] == [definition["instructions"]],
                "each management prompt receives only its own Bot instructions")
        snapshot = a.client.rpc("conversation", conversation_id=definition["conversation_id"])
        require(snapshot["bot"] == next(bot for bot in stored if bot["bot_id"] == definition["bot_id"])
                and [message["message_id"] for message in snapshot["messages"] if message["role"] == "user"]
                    == ["bot-message-" + definition["bot_id"]], "two Bots retain separate definitions and message histories")
    require(first[0]["native_session_id"] != first[1]["native_session_id"],
            "two Bots never share the Scheduler SDK cache")
    before_calls = len(fixture_records(run))
    wait_for("third entry imports both Bot definitions", lambda: len(c.client.rpc("bots")["bots"]) == 2)
    wait_for("C durably replicates both signed Bot definitions", lambda:
        all(any(event["body"].get("type") == "bot_created" and event["body"].get("bot_id") == definition["bot_id"]
                for event in local_events(c, space, definition["conversation_id"], resource_id=a.client.resource_id))
            for definition in definitions))
    c.stop()
    c.configure(manager=fixture_manager_settings(), codex_bin=sys.executable)
    c.start()
    c.client.connect()
    c.client.authenticate()
    a.stop()
    a.start()
    a.client.connect()
    a.client.authenticate()
    require_quiet("restart and activating a Manager on imported Bot history never run a model", lambda:
        len(fixture_records(run)) == before_calls and not scheduler_records(c, space))
    require(sorted(a.client.rpc("bots")["bots"], key=lambda bot: bot["bot_id"]) == stored,
            "Bot definitions and their signed identity survive restart")
    resumed = send(definitions[0], "bot-alpha-after-restart")
    require(resumed["resumed"] and resumed["native_session_id"] == first[0]["native_session_id"]
            and resumed["bot_prompt_markers"] == [definitions[0]["instructions"]],
            "restarted Bot resumes only its own native cache with its original instructions")
    c.stop()
    c.configure(manager=None)
    c.start()
    c.client.connect()
    c.client.authenticate()
    passed("explicit Bot creation retries safely, isolates instructions/history/native caches and imports or restarts without model replay",
           bot_ids=[bot["bot_id"] for bot in stored])


def startup_subscription_checks(run, workspace, a, b, c, space, passed):
    name = "chat-startup-subscription"
    directory = workspace / name
    directory.mkdir()
    gate = directory / "release"
    a.client.rpc("create_conversation", conversation_id=name, title=name)
    text = json.dumps({"chat_agent": True, "label": "startup-subscription", "dispatch": {
        "label": "startup-subscription", "target_node": b.node_id, "cwd": str(directory),
        "executor": "codex", "gate": str(gate), "approval": True}})
    receipt = a.client.rpc("message", conversation_id=name, message_id="startup-subscription",
                           text=text, report_results=True)

    def rpc(method):
        return [row for row in fixture_records(run, "codex-rpc.jsonl") if row["message"].get("method") == method]

    def notifications(kind):
        return [event["body"] for event in local_events(b, space, name, resource_id=a.client.resource_id)
                if event["body"].get("type") == "notification_raised" and event["body"].get("kind") == kind]

    wait_for("controlled Codex turn starts", lambda: len(rpc("turn/start")) == 1)
    wait_for("management dispatch commits before disconnect", lambda:
        scheduler_record(a, space, receipt["trigger"])["state"] == "completed")
    records = [row["result"] for row in fixture_records(run, "manager-tools.jsonl")
               if row.get("call_ref") == "delegate-startup-subscription" and row.get("result", {}).get("all_dispatched")]
    require(records and len(records[-1]["operations"]) == 2, "Codex delegation has one frozen open and send")
    operation_id = records[-1]["operations"][-1]["operation_id"]
    native = rpc("turn/start")[0]["message"]["params"]["threadId"]
    wait_for("duplicate native approval produces one persisted notification", lambda: len(notifications("needs_input")) == 1)
    approval_id = notifications("needs_input")[0]["notification_id"]
    for node in (a, b, c):
        node.client.command("close")
    previous_pids = {row["pid"] for row in rpc("initialize")}
    b.stop()
    b.start()
    restored = wait_for("Node startup subscribes known native work without any client connection", lambda:
        next((row for row in rpc("thread/resume") if row["pid"] not in previous_pids
              and row["message"]["params"].get("threadId") == native), None))
    require(restored["message"]["params"].get("excludeTurns") is True,
            "startup restores a native subscription without starting a model turn")
    private_json(run / "fixture-codex-disconnect", "disconnect-once")
    wait_for("adapter reconnect restores the same native subscription", lambda:
        len({row["connection"] for row in rpc("thread/resume") if row["pid"] == restored["pid"]
             and row["message"]["params"].get("threadId") == native}) >= 2)
    wait_for("reconnected native pending approval is replayed", lambda:
        len({row["connection"] for row in fixture_records(run, "codex-events.jsonl")
             if row["thread_id"] == native and row["event"] == "approval"}) >= 3)
    require_quiet("native approval replay keeps one durable notification without auto-approval", lambda:
        [row["notification_id"] for row in notifications("needs_input")] == [approval_id]
        and len(rpc("turn/start")) == 1
        and not any(row["message"].get("id") == "approval-" + native and "method" not in row["message"]
                    for row in fixture_records(run, "codex-rpc.jsonl")))
    gate.touch()
    wait_for("native completion updates original operation while all clients remain disconnected", lambda:
        (local_operation(b, space, operation_id, resource_id=a.client.resource_id) or {}).get("state") == "succeeded")
    wait_for("duplicate completion has one result notification", lambda:
        sum(event["body"].get("notification_id") == "result:" + operation_id + ":succeeded"
            for event in local_events(b, space, name, resource_id=a.client.resource_id)) == 1)
    wait_for("native completion returns once to original Scheduler without Web", lambda:
        len([row for row in fixture_records(run) if row["conversation_id"] == name and row["autonomous"]]) == 1
        and any(job["conversation_id"] == name and job["kind"] == "autonomous" and job["state"] == "completed"
                for job in scheduler_records(a, space)))
    for node in (a, b, c):
        node.client.connect()
        node.client.authenticate()
    snapshot = c.client.rpc("conversation", conversation_id=name)
    require(snapshot["tasks"] == [] and len(rpc("turn/start")) == 1,
            "startup recovery preserves taskless history and never replays the native turn")
    passed("startup and reconnect restore Codex events without Web, duplicate approvals notify once and no approval is automatic",
           operation_id=operation_id, native_thread=native, fixture="controlled App Server JSON-RPC")

    name = "chat-restart-reconcile"
    directory = workspace / name
    directory.mkdir()
    a.client.rpc("create_conversation", conversation_id=name, title=name)
    text = json.dumps({"chat_agent": True, "label": "restart-reconcile", "dispatch": {
        "label": "restart-reconcile", "target_node": b.node_id, "cwd": str(directory),
        "executor": "codex", "gate": str(directory / "never-release")}})
    receipt = a.client.rpc("message", conversation_id=name, message_id="restart-reconcile", text=text)
    wait_for("second native turn is durably dispatched", lambda: len(rpc("turn/start")) == 2
        and (scheduler_record(a, space, receipt["trigger"]) or {}).get("state") == "completed")
    records = [row["result"] for row in fixture_records(run, "manager-tools.jsonl")
               if row.get("call_ref") == "delegate-restart-reconcile" and row.get("result", {}).get("all_dispatched")]
    operation_id = records[-1]["operations"][-1]["operation_id"]
    native = rpc("turn/start")[-1]["message"]["params"]["threadId"]
    for node in (a, b, c):
        node.client.command("close")
    previous_pids = {row["pid"] for row in rpc("initialize")}
    b.stop()
    state_path = run / "fixture-codex-state.json"
    state = json.loads(state_path.read_text())
    thread = state["threads"][native]
    thread["status"] = {"type": "idle"}
    thread["turns"][-1].update(status="completed", items=[{
        "id": "offline-answer", "type": "agentMessage", "text": "fixture completed while Node was offline"}])
    private_json(state_path, state)
    b.start()
    wait_for("startup reads the missed native terminal turn without any Web", lambda:
        any(row["pid"] not in previous_pids and row["message"]["params"].get("threadId") == native
            for row in rpc("thread/turns/list")))
    wait_for("read-only native reconciliation confirms the original operation", lambda:
        (local_operation(b, space, operation_id, resource_id=a.client.resource_id) or {}).get("state") == "succeeded")
    require(len(rpc("turn/start")) == 2, "missed completion recovery never sends another native turn")
    for node in (a, b, c):
        node.client.connect()
        node.client.authenticate()
    passed("startup reconciles a completion missed while offline through native read RPCs without replay",
           operation_id=operation_id, native_thread=native)


def chat_agent_acceptance(options):
    binary = options.binary.resolve(strict=True)
    module = options.crypto_module.resolve(strict=True)
    require(module.with_name("vibeface_secure_bg.wasm").read_bytes().startswith(b"\x00asm"),
            "real shared cryptography WASM is available")
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("chat-agent-process-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir()
    fixture = fixture_wrapper(run, "fixture-executor", "--fixture-executor")
    codex = fixture_wrapper(run, "fixture-codex", "--fixture-codex")
    helper = Path(__file__).resolve().with_name("node-client.mjs")
    nodes = []
    report = {"status": "running", "source_binary": str(binary), "binary_sha256": file_sha256(binary),
              "crypto_module_sha256": file_sha256(module),
              "wasm_sha256": file_sha256(module.with_name("vibeface_secure_bg.wasm")),
              "provider_calls": 0, "evidence": [], "not_checked": ["real provider model behavior",
                  "rendered browser UI", "real LAN/public ingress", "Linux command sandbox on a macOS host"]}

    def passed(description, **evidence):
        report["evidence"].append({"check": description, **evidence})
        print("PASS " + description, flush=True)

    try:
        for name in ("A", "B", "C", "BOT-OTHER-NETWORK"):
            node = Node(run, name, binary, workspace, fixture, helper, module)
            node.configure(timeout_secs=600, codex_bin=str(codex) if name == "B" else sys.executable)
            nodes.append(node)
        a, b, c, outsider = nodes
        a.configure(manager=fixture_manager_settings(), daily_decision_limit=100)
        a.start()
        verify_web_assets(a, module)
        space = a.bootstrap()
        outsider.start()
        outsider.bootstrap()
        report["topology"] = setup_manager_topology(a, b, c)
        wait_for("private Nodes can reach each other", lambda:
            all(any(row["node_id"] == target.node_id and row["online"] for row in a.client.rpc("nodes")["nodes"])
                for target in (b, c)))
        bot_checks(run, workspace, a, b, c, space, passed, outsider)
        chat_agent_checks(run, workspace, a, b, c, space, passed)
        startup_subscription_checks(run, workspace, a, b, c, space, passed)
        require(file_sha256(binary) == report["binary_sha256"]
                and file_sha256(module) == report["crypto_module_sha256"]
                and file_sha256(module.with_name("vibeface_secure_bg.wasm")) == report["wasm_sha256"],
                "process acceptance uses unchanged binary, JS and WASM artifacts")
        for node in nodes:
            verify_current_data_format(node.directory, stopped=False)
        report["status"] = "passed"
        report["manager_invocations"] = len(fixture_records(run))
        report["executor_invocations"] = len(invocations(run))
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        for directory_name in ("chat-command-restart", "chat-startup-subscription"):
            directory = workspace / directory_name
            if directory.is_dir():
                (directory / "release").touch()
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        stop_fixture_processes(run)
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


def phase1_acceptance(options):
    source_binary = options.binary.resolve(strict=True)
    module = options.crypto_module.resolve(strict=True)
    require(module.with_name("vibeface_secure_bg.wasm").read_bytes().startswith(b"\x00asm"),
            "real shared cryptography WASM is available")
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("phase1-process-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    binary = run / "vibeface"
    shutil.copy2(source_binary, binary)
    workspace = run / "workspace"
    workspace.mkdir()
    fixture = fixture_wrapper(run, "fixture-executor", "--fixture-executor")
    manager = fixture_manager_settings()
    helper = Path(__file__).resolve().with_name("node-client.mjs")
    nodes = []
    report = {"status": "running", "source_binary": str(source_binary),
              "binary_sha256": file_sha256(binary), "provider_calls": 0,
              "crypto_module_sha256": file_sha256(module),
              "wasm_sha256": file_sha256(module.with_name("vibeface_secure_bg.wasm")),
              "evidence": [], "not_checked": ["real Cursor/Codex model success and native controls",
                  "rendered browser UI", "wire ACK loss", "real LAN/public ingress",
                  "instruction-boundary claim/final-commit crash injection (verify separately in Rust tests)"]}

    def passed(description, **evidence):
        report["evidence"].append({"check": description, **evidence})
        print("PASS " + description, flush=True)

    def calls(conversation=None, *, label=None, autonomous=None):
        return [record for record in fixture_records(run)
                if (conversation is None or record["conversation_id"] == conversation)
                and (label is None or record["label"] == label)
                and (autonomous is None or record["autonomous"] == autonomous)]

    def conversation(client, name):
        return client.rpc("conversation", conversation_id=name)

    def new_conversation(name):
        a.client.rpc("create_conversation", conversation_id=name, title=name)
        return name

    def send(client, name, label, **directive):
        text = json.dumps({"phase1": True, "label": label, **directive})
        receipt = client.rpc("message", conversation_id=name, message_id=label, text=text,
                             scheduler_node=a.node_id, report_results=directive.get("report_results", False))
        require(receipt["accepted"] is True and receipt["message_id"] == label
                and receipt["scheduler_node"] == a.node_id, "acceptance pins message and Scheduler")
        proof = receipt["receipt"]
        require(proof["version"] == 1 and proof["space_id"] == space
                and proof["resource_id"] == a.client.resource_id
                and proof["conversation_id"] == name and proof["trigger"] == receipt["trigger"]
                and proof["target_node"] == a.node_id, "durable receipt has the exact accepted identity")
        return receipt, text

    def state(receipt, expected):
        record = scheduler_record(a, space, receipt["trigger"])
        return record if record and record["state"] == expected else None

    def completed(receipt):
        record = scheduler_record(a, space, receipt["trigger"])
        if record and record["state"] in {"failed", "unknown", "cancelled"}:
            raise RuntimeError("management job reached " + record["state"] + ": " + record.get("detail", ""))
        return record if record and record["state"] == "completed" else None

    def reconnect(node):
        node.client.connect()
        wait_for(node.name + " authenticates its enrolled independent client", lambda: node.client.authenticate())

    def stop_managers():
        # Only fixture PIDs still belonging to this run may be stopped.
        for record in fixture_records(run):
            pid = record["pid"]
            result = subprocess.run(["ps", "-p", str(pid), "-o", "command="], capture_output=True, text=True)
            if "--fixture-manager" in result.stdout and str(run) in result.stdout:
                try:
                    os.killpg(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    try:
        for name in ("A", "B", "C"):
            node = Node(run, name, binary, workspace, fixture, helper, module)
            node.configure(timeout_secs=600)
            nodes.append(node)
        a, b, c = nodes
        a.configure(manager=manager, codex_bin=sys.executable, daily_decision_limit=100)
        a.start()
        verify_web_assets(a, module)
        space = a.bootstrap()
        report["topology"] = setup_manager_topology(a, b, c)
        wait_for("three private Nodes become mutually reachable", lambda:
                 all(any(row["node_id"] == target.node_id and row["online"]
                         for row in a.client.rpc("nodes")["nodes"]) for target in (b, c)))
        c.client.subscribe()
        passed("three real Nodes enroll independent clients and explicit replica grants; Manager uses a controlled fixture")

        name = new_conversation("phase1-queue")
        gate = run / "release-first"
        first, original = send(a.client, name, "queue-first", gate=str(gate))
        require(first["queued"] is True, "local receipt confirms durable queue admission")
        wait_for("first management invocation starts", lambda: len(calls(name, label="queue-first")) == 1)
        second, _ = send(a.client, name, "queue-second")
        wait_for("busy second message remains durably queued", lambda: state(second, "queued"))
        cancelled_queued, _ = send(a.client, name, "queue-cancelled")
        queued_before = wait_for("cancellation target remains queued behind the active reply",
                                lambda: state(cancelled_queued, "queued"))
        retry = a.client.rpc("message", conversation_id=name, message_id="queue-first", text=original,
                             scheduler_node=a.node_id, report_results=False)
        require(retry["trigger"] == first["trigger"], "message retry retains its exact trigger")
        a.client.rpc("message", conversation_id=name, message_id="queue-first", text="changed", status=400)
        wait_for("third entry imports this conversation", lambda: conversation(c.client, name))
        wait_for("C durably replicates the original signed message", lambda:
            any(event["body"].get("message_id") == "queue-first"
                for event in local_events(c, space, name, resource_id=a.client.resource_id)))
        wait_for("third entry restores original Manager active and queued durable states", lambda:
                 (lambda jobs: any(job["trigger"] == first["trigger"] and job["state"] == "running" for job in jobs)
                  and any(job["trigger"] == second["trigger"] and job["state"] == "queued" for job in jobs)
                  and any(job["trigger"] == cancelled_queued["trigger"] and job["state"] == "queued" for job in jobs))(
                      conversation(c.client, name)["scheduler"]["jobs"]))
        progress = wait_for("third entry observes actual remote management text", lambda:
            [event["payload"] for event in c.client.command("events") if event.get("payload", {}).get("type") == "manager_progress"
             and event["payload"].get("conversation_id") == name and event["payload"].get("trigger") == first["trigger"]])
        require(all(row["source_node"] == a.node_id and row["invocation_id"]
                    and row["sequence"] > 0 and row["progress"]["type"] == "text_snapshot" for row in progress),
                "remote progress carries the original invocation identity and normalized text snapshots")
        seen = {}
        for row in progress:
            require(seen.setdefault((row["invocation_id"], row["sequence"]), row["progress"]) == row["progress"],
                    "replayed management progress keeps the same content for its invocation sequence")
        active_before = state(first, "running")
        require(active_before, "original reply remains running before queued cancellation")
        cancelled_reply = c.client.rpc("cancel_manager", conversation_id=name, trigger=cancelled_queued["trigger"], attempt_id=None)
        require(cancelled_reply.get("cancel_requested") is True
                and cancelled_reply["job"]["state"] == "cancelled"
                and "attempt" not in cancelled_reply["job"],
                "third entry cancels queued work before any SDK attempt")
        cancelled_job = wait_for("queued cancellation persists at the original Scheduler",
                                 lambda: state(cancelled_queued, "cancelled"))
        require(all(cancelled_job[key] == queued_before[key] for key in ("message_id", "basis_heads", "sequence")),
                "queued cancellation preserves the original accepted message and basis")
        repeated_cancel = a.client.rpc("cancel_manager", conversation_id=name, trigger=cancelled_queued["trigger"], attempt_id=None)
        require(repeated_cancel.get("cancel_requested") is True and repeated_cancel["job"] == cancelled_reply["job"],
                "repeated queued cancellation confirms the same terminal job")
        active_after = state(first, "running")
        require(active_after and active_after["attempt"] == active_before["attempt"] and state(second, "queued")
                and not calls(name, label="queue-cancelled"),
                "queued cancellation leaves the active attempt and other queued message intact")
        lease, maintenance_reply = maintenance(a, "acquire")
        lease.close()
        require(maintenance_reply["status"] != "ready", "running and queued jobs prevent managed update drain")
        gate.touch()
        wait_for("first reply commits with terminal state", lambda: completed(first))
        wait_for("busy second message is automatically consumed", lambda: completed(second))
        for number in range(4):
            race, _ = send(a.client, name, "consumer-rewake-" + str(number))
            wait_for("consumer exit/new enqueue is not lost", lambda: completed(race))
        snapshot = conversation(a.client, name)
        jobs = snapshot["scheduler"]["jobs"]
        completed_jobs = [job for job in jobs if job["state"] == "completed"]
        cancelled_jobs = [job for job in jobs if job["state"] == "cancelled"]
        require(len(jobs) == 7 and len(completed_jobs) == len(calls(name)) == 6
                and {job["trigger"] for job in completed_jobs} == {call["trigger"] for call in calls(name)}
                and [job["trigger"] for job in cancelled_jobs] == [cancelled_queued["trigger"]],
                "each uncancelled trigger invokes once and queued cancellation remains terminal")
        require(not cancelled_jobs[0].get("reply_message_id") and not calls(name, label="queue-cancelled")
                and not any(message["message_id"] == cancelled_job["message_id"] for message in snapshot["messages"])
                and sum(message["message_id"] == "queue-cancelled" for message in snapshot["messages"]) == 1,
                "cancelled queued work retains its user message without an SDK call or assistant reply")
        require(all(sum(message["message_id"] == job["reply_message_id"] for message in snapshot["messages"]) == 1
                    for job in completed_jobs), "each completed job has exactly one persisted final reply")
        require(calls(name)[1]["resumed"] and calls(name)[0]["native_session_id"] == calls(name)[1]["native_session_id"],
                "successful consecutive calls preserve the local native session")
        passed("busy messages persist, retry deduplicates, queued cancellation is final and remaining replies match",
               triggers=[first["trigger"], second["trigger"]], cancelled_trigger=cancelled_queued["trigger"],
               invocations=len(calls(name)))

        for unconfirmed in (False, True):
            name = new_conversation("phase1-cancel-" + str(unconfirmed).lower())
            cancelled, _ = send(a.client, name, "cancel-" + str(unconfirmed).lower(),
                                gate=str(run / "never-release-cancel"), cancel_unknown=unconfirmed)
            wait_for("controlled management call starts before cancel", lambda: calls(name))
            active = wait_for("cancellation pins the current running attempt", lambda: state(cancelled, "running"))
            reply = a.client.rpc("cancel_manager", conversation_id=name, trigger=cancelled["trigger"],
                                 attempt_id=active["attempt"]["id"])
            require(reply.get("cancel_requested") is True, "cancel request has an explicit pending state")
            terminal = "unknown" if unconfirmed else "cancelled"
            wait_for("cancel reflects actual confirmation evidence", lambda: state(cancelled, terminal))
            require(len(calls(name)) == 1, "cancel does not restart the same management trigger")
            passed("management cancellation becomes " + terminal + " according to controlled evidence",
                   trigger=cancelled["trigger"], evidence_kind="controlled sidecar protocol only")

        name = new_conversation("phase1-restart")
        interrupted, _ = send(a.client, name, "restart-running", gate=str(run / "never-release"))
        wait_for("old attempt crosses running boundary", lambda: state(interrupted, "running") and calls(name))
        queued, _ = send(a.client, name, "restart-queued")
        wait_for("second trigger is persisted before crash", lambda: state(queued, "queued"))
        old_attempt = state(interrupted, "running")["attempt"]["id"]
        a.stop()
        stop_managers()
        a.start()
        reconnect(a)
        unknown = wait_for("restarted running attempt stays unknown", lambda: state(interrupted, "unknown"))
        wait_for("queued trigger performs its first call after restart", lambda: completed(queued))
        require(unknown["attempt"]["id"] == old_attempt and len(calls(name, label="restart-running")) == 1
                and len(calls(name, label="restart-queued")) == 1, "recovery preserves original attempt and never replays running work")
        require(calls(name, label="restart-queued")[0]["read_only"]
                and conversation(a.client, name)["scheduler"]["write_frozen"], "unknown recovery stays server-enforced read-only")
        readonly, _ = send(a.client, name, "unknown-readonly", tools=[{
            "name": "command", "arguments": {"command": "printf forbidden > forbidden-restart",
                "cwd": str(workspace)}, "accepted": False}])
        wait_for("new read-only recovery conversation remains usable", lambda: completed(readonly))
        require(not (workspace / "forbidden-restart").exists(), "read-only recovery cannot create business work")
        a.client.rpc("resolve_manager_unknown", conversation_id=name, trigger=interrupted["trigger"],
                     attempt_id=old_attempt, evidence="Owned fixture process terminated; original attempt admitted no business operations.")
        writable, _ = send(a.client, name, "after-human-reconcile", tools=[{
            "name": "command", "arguments": {"command": "printf allowed > after-reconcile",
                "cwd": str(workspace)}, "wait_terminal": True}])
        wait_for("new human round receives normal authority only after reconciliation", lambda: completed(writable))
        require((workspace / "after-reconcile").read_text() == "allowed" and not conversation(a.client, name)["scheduler"]["write_frozen"]
                and state(interrupted, "unknown") and len(calls(name, label="restart-running")) == 1,
                "reconciliation restores future writes without changing or replaying the original Unknown")
        passed("restart preserves Unknown, enforces read-only recovery and restores only future writes after human reconciliation",
               attempt_id=old_attempt)

        name = new_conversation("phase1-remote-ack")
        wait_for("remote source knows conversation", lambda: conversation(c.client, name))
        with sqlite3.connect(c.directory / "node.sqlite3") as connection:
            require(c.directory.parent == run and c.directory.is_dir(),
                    "receipt fault injection affects only this run's private Node database")
            space_sql = "'" + space.replace("'", "''") + "'"
            resource_sql = "'" + a.client.resource_id.replace("'", "''") + "'"
            connection.execute("CREATE TRIGGER phase1_drop_receipt BEFORE UPDATE OF value ON local_values "
                "WHEN NEW.space_id=" + space_sql + " AND json_extract(NEW.value,'$.resource_id')=" + resource_sql +
                " AND NEW.key LIKE 'scheduler:v1:outbox:%' AND json_extract(NEW.value,'$.delivered')=1 "
                "BEGIN SELECT RAISE(ABORT,'phase1 injected lost durable receipt'); END")
        remote, remote_text = send(c.client, name, "remote-receipt")
        require(remote["queued"] is False and remote["state"] == "pending_delivery",
                "source acceptance never masquerades as a remote durable queue ACK")
        wait_for("remote target commits its only reply despite source receipt loss", lambda: completed(remote))
        pending = scheduler_record(c, space, remote["trigger"], "outbox")
        require(pending and not pending["delivered"], "injected receipt failure leaves durable source outbox pending")
        c.stop()
        with sqlite3.connect(c.directory / "node.sqlite3") as connection:
            connection.execute("DROP TRIGGER phase1_drop_receipt")
        c.start()
        reconnect(c)
        wait_for("restarted source reconciles original target receipt", lambda:
                 scheduler_record(c, space, remote["trigger"], "outbox")["delivered"])
        duplicate = c.client.rpc("message", conversation_id=name, message_id="remote-receipt", text=remote_text,
                                 scheduler_node=a.node_id, report_results=False)
        require(duplicate["trigger"] == remote["trigger"] and len(calls(name)) == 1,
                "same source trigger and target deduplicate after missing durable ACK")
        passed("durable ACK loss and source restart retry only the original target without duplicate invocation",
               fault="source ACK persistence rejected after target admission", trigger=remote["trigger"])

        chat_agent_checks(run, workspace, a, b, c, space, passed)

        name = new_conversation("phase1-budget")
        with sqlite3.connect(a.directory / "node.sqlite3") as connection:
            require(a.directory.parent == run and a.directory.is_dir(),
                    "budget fault injection affects only this run's private Node database")
            connection.execute("INSERT INTO budget_days(space_id,day,used) VALUES(?,?,?) "
                "ON CONFLICT(space_id,day) DO UPDATE SET used=excluded.used",
                (space, time.strftime("%Y-%m-%d", time.gmtime()), 100))
        human, _ = send(a.client, name, "human-after-autonomous-budget")
        wait_for("human chat ignores exhausted autonomous budget", lambda: completed(human))
        wait_for("remote entry knows the budget-test conversation", lambda: conversation(c.client, name))
        remote_human, _ = send(c.client, name, "remote-human-after-autonomous-budget")
        wait_for("verified remote human chat also ignores exhausted autonomous budget", lambda: completed(remote_human))
        with sqlite3.connect(f"file:{a.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
            used = connection.execute("SELECT used FROM budget_days WHERE space_id=? AND day=?",
                                      (space, time.strftime("%Y-%m-%d", time.gmtime()))).fetchone()[0]
        require(used == 100, "local and remote human chat do not consume or reset existing autonomous usage")
        passed("local and remote human chat remain available without consuming or resetting exhausted autonomous budget")
        before = len(calls())
        wait_for("third entry has imported the last human message", lambda:
                 any(message["message_id"] == "human-after-autonomous-budget" for message in conversation(c.client, name)["messages"]))
        c.stop()
        c.configure(manager=manager, codex_bin=sys.executable)
        c.start()
        reconnect(c)
        require_quiet("imported shared DAG never manufactures local execution jobs after Manager activation", lambda:
                      len(calls()) == before and not scheduler_records(c, space))
        passed("activating a Manager on imported history does not replay shared business events")
        require(file_sha256(binary) == report["binary_sha256"]
                and file_sha256(module) == report["crypto_module_sha256"]
                and file_sha256(module.with_name("vibeface_secure_bg.wasm")) == report["wasm_sha256"],
                "process acceptance uses unchanged binary, JS and WASM artifacts")
        for node in nodes:
            verify_current_data_format(node.directory, stopped=False)
        report["status"] = "passed"
        report["manager_invocations"] = len(calls())
        report["executor_invocations"] = len(invocations(run))
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        stop_managers()
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


def acceptance(options):
    binary = options.binary.resolve(strict=True)
    module = options.crypto_module.resolve(strict=True)
    require(module.with_name("vibeface_secure_bg.wasm").read_bytes().startswith(b"\x00asm"),
            "real shared cryptography WASM is available")
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("node-acceptance-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    fixture = fixture_wrapper(run, "fixture-executor", "--fixture-executor")
    helper = Path(__file__).resolve().with_name("node-client.mjs")
    nodes = []
    report = {"status": "running", "evidence": [], "provider_calls": 0,
              "binary_sha256": file_sha256(binary), "crypto_module_sha256": file_sha256(module),
              "wasm_sha256": file_sha256(module.with_name("vibeface_secure_bg.wasm")),
              "not_checked": ["Gateway to registered Project host", "additional same-Account Node enrollment", "Node Service revocation",
                              "recovery on real Node wire", "paid model output and SDK reasoning",
                              "real LAN/public ingress", "rendered browser UI", "phone and platform Passkey"]}

    def passed(description, kind="real_node_processes"):
        report["evidence"].append({"check": description, "kind": kind})
        print("PASS " + description, flush=True)

    try:
        verify_direct_start(run, binary)
        verify_incompatible_data(run, binary, workspace)
        passed("fresh SQLite startup and restart; incompatible directories refuse before all writes and providers")
        for name in ("A", "OTHER"):
            node = Node(run, name, binary, workspace, fixture, helper, module)
            nodes.append(node)
            node.configure(codex_bin=options.codex_bin)
            if name == "A":
                node.configure(network={"bootstrap_urls": [], "advertise_urls": [node.url.replace("http://", "ws://") + "/api/network/ws"],
                                        "mdns": False, "relay_enabled": True})
            node.start()
            node.bootstrap()
        a, other = nodes
        client, foreign = a.client, other.client
        require(client.scope["account_id"] != foreign.scope["account_id"]
                and client.scope["space_id"] != foreign.scope["space_id"],
                "two real independent accounts own distinct personal spaces")
        report["topology"] = {"accounts": [client.scope["account_id"], foreign.scope["account_id"]],
                              "spaces": [client.scope["space_id"], foreign.scope["space_id"]],
                              "nodes": [a.node_id, other.node_id]}
        verify_cli_status(a, True)
        verify_web_assets(a, module)
        verify_channel_security(a)
        verify_maintenance(a)
        verify_browser_origins(a, [f"http://localhost:{a.port}", f"http://0.0.0.0:{a.port}"])
        passed("two-stage account creation, pinned Noise v2, channel attacks, maintenance and packaged web assets")

        first = client.resource_id
        second = client.add_project_fixture(a)
        require(first != second, "one space contains two independently signed Projects")
        report["topology"]["resources"] = [first, second]
        for resource, conversation in ((first, "scope-first"), (second, "scope-second")):
            client.rpc("create_conversation", resource_id=resource, conversation_id=conversation, title=conversation)
            listed = client.rpc("conversations", resource_id=resource)["conversations"]
            require([entry["conversation_id"] for entry in listed] == [conversation],
                    "resource list contains only its own conversation")
        conversation, task = "scope-first", "fixture-task"
        for method in ("conversation", "context", "export"):
            client.rpc(method, resource_id=second, conversation_id=conversation, status=400)
            foreign.rpc(method, conversation_id=conversation, status=400)
        client.authenticate(resource_id=first)
        body = {"method": "conversations", "space_id": client.scope["space_id"],
                "resource_id": first, "authorization_id": uuid.uuid4().hex}
        prepared = client.command("prepare_signed", body=body)
        client.call("rpc", {**body, "space_id": foreign.scope["space_id"]}, invocation=prepared["invocation"], status=400)
        require(client.call("rpc", body, invocation=prepared["invocation"])["conversations"],
                "a rejected signed scope replacement does not consume the valid original")
        passed("account, space and signed Project scope isolate conversation lists, context and exports")
        client.create_task(a, conversation, task, title="Explicit acceptance", goal="Keep an uncertain operation durable",
                           acceptance="An explicit account decision is required")
        client.rpc("message", conversation_id=conversation, message_id="message-original", text="Keep resource-scoped history")
        client.rpc("message", conversation_id=conversation, message_id="message-original", text="Keep resource-scoped history")
        client.rpc("message", conversation_id=conversation, message_id="message-original", text="changed", status=400)
        history = client.rpc("conversation", conversation_id=conversation)
        require(len([message for message in history["messages"] if message["message_id"] == "message-original"]) == 1,
                "stable message retry preserves one signed event")
        if options.skip_codex:
            report["not_checked"].append("real Codex native transport (--skip-codex)")
        else:
            require(shutil.which(options.codex_bin) is not None or Path(options.codex_bin).is_file(),
                    "Codex executable is available; use --skip-codex for fixture-only acceptance")
            client.create_task(a, conversation, "native-transport-task", title="Native transport",
                               goal="Open one native thread without a model", acceptance="Native thread ID persists")
            native_spec = {"type": "open", "executor": "codex", "cwd": str(workspace)}
            client.bind_execution(conversation, "native-transport-task", a, "native-codex", native_spec)
            native_open = request(client.scope["space_id"], conversation, a, "codex-native-open", "native-codex",
                                  native_spec, task="native-transport-task")
            dispatch(client, native_open)
            wait_for("real Codex native thread opens", lambda: succeeded(client, a, "codex-native-open"))
            native = client.rpc("session", target_node=a.node_id, session_id="native-codex")
            require(native["session"]["native_session_id"] and "error" not in native["actual"],
                    "real app-server native thread is persisted and readable")
            passed("installed Codex opens and reads a native thread without a model turn", "real_codex_transport")
        open_spec = {"type": "open", "executor": "claude_code", "cwd": str(workspace)}
        client.bind_execution(conversation, task, a, "fixture-main", open_spec)
        opened = request(client.scope["space_id"], conversation, a, "fixture-open", "fixture-main", open_spec, task=task)
        dispatch(client, opened)
        wait_for("confirmed task session opens", lambda: succeeded(client, a, "fixture-open"))
        dispatch(client, opened)
        changed = copy.deepcopy(opened)
        changed["action"]["cwd"] = str(run)
        dispatch(client, changed, status=400)
        passed("verified TaskCreated and explicit TaskBinding govern execution; changed duplicate intent rejects")

        device = client.scope["device_id"]
        session = client.rpc("session", target_node=a.node_id, session_id="fixture-main")["session"]
        gate = run / "complete-result"
        sent = request(client.scope["space_id"], conversation, a, "fixture-result-send", "fixture-main",
                       {"type": "send", "text": json.dumps({"label": "result", "gate": str(gate), "result_size": 280000})},
                       revision=session["revision"], task=task)
        client.subscribe()
        dispatch(client, sent)
        wait_for("one fixture executes", lambda: len(invocations(run, "result")) == 1)
        dispatch(client, sent)
        gate.touch(mode=0o600)
        wait_for("fixture result is durable", lambda: succeeded(client, a, sent["operation_id"]))
        notification = "result:fixture-result-send:succeeded"
        wait_for("resource result notification", lambda: any(item["notification_id"] == notification
                 for item in client.rpc("notifications", client_id=device)["notifications"]))
        client.rpc("notifications", client_id=foreign.scope["device_id"], status=400)
        client.rpc("ack", conversation_id=conversation, notification_id=notification, client_id=foreign.scope["device_id"], status=400)
        client.rpc("ack", resource_id=second, conversation_id=conversation, notification_id=notification, client_id=device, status=400)
        client.rpc("ack", conversation_id=conversation, notification_id=notification, client_id=device)
        require(all(item["notification_id"] != notification
                    for item in client.rpc("notifications", client_id=device)["notifications"]),
                "the explicit device acknowledgement removes its notification")
        snapshot = client.rpc("export", conversation_id=conversation)
        report["event_count"] = verify_snapshot(client, snapshot, first, {a.node_id: a.node_public_key})
        require(verify_blobs(a, client.scope["space_id"]) > 0, "large result has verified persisted attachment bytes")
        references = {blob["hash"]: blob["size"] for envelope in snapshot["events"]
                      for blob in json.loads(envelope["canonical_body"])["blobs"]}
        require(references, "a validated result event refers to a nonempty set of attachments")
        foreign.rpc("create_conversation", conversation_id="foreign-readable", title="Foreign private context")
        for digest, size in references.items():
            content, offset = bytearray(), 0
            while True:
                chunk = client.rpc("context", conversation_id=conversation, blob_hash=digest, offset=offset, limit=100)
                require(chunk["hash"] == digest and chunk["size"] == size and chunk["offset"] == offset,
                        "attachment chunks retain their verified event reference and offset")
                content.extend(bytes.fromhex(chunk["data_hex"]))
                if chunk["next_offset"] is None:
                    break
                require(chunk["next_offset"] > offset, "attachment pagination makes progress")
                offset = chunk["next_offset"]
            require(len(content) == size and hashlib.sha256(content).hexdigest() == digest,
                    "the real authorized attachment API returns the exact verified bytes")
            client.rpc("context", resource_id=second, conversation_id="scope-second", blob_hash=digest, status=400)
            foreign.rpc("context", conversation_id="foreign-readable", blob_hash=digest, status=400)
        require(len(invocations(run, "result")) == 1, "reads and duplicate dispatch never repeat the fixture")
        passed("large result, signed DAG closure and durable device notifications stay within one resource", "real_nodes_with_stream_json_fixture")

        session = client.rpc("session", target_node=a.node_id, session_id="fixture-main")["session"]
        uncertain = request(client.scope["space_id"], conversation, a, "fixture-unknown-send", "fixture-main",
                            {"type": "send", "text": json.dumps({"label": "unknown", "gate": str(run / "never-complete")})},
                            revision=session["revision"], task=task)
        dispatch(client, uncertain)
        wait_for("interrupted fixture starts once", lambda: len(invocations(run, "unknown")) == 1)
        admission = admitted_scope(a, client, uncertain["operation_id"])
        verify_live_access(a, uncertain["operation_id"])
        before = None
        report["unknown_restarts"] = {"initial_pid": a.process.pid, "original_admission": admission, "restarts": []}
        for index in range(2):
            a.stop()
            a.start()
            client.connect()
            client.authenticate()
            current = wait_for("restart preserves Unknown", lambda: unknown_record(client, a, uncertain["operation_id"]))
            require(current == before if before else current["state"] == "unknown", "the second restart preserves the exact Unknown record")
            require(admitted_scope(a, client, uncertain["operation_id"]) == admission,
                    "both restarts preserve the complete original account admission and stable operation ID")
            report["unknown_restarts"]["restarts"].append({"pid": a.process.pid, "operation": current})
            before = current
            verify_current_data_format(a.directory, stopped=False)
        client.rpc("reconcile", target_node=a.node_id, operation_id=uncertain["operation_id"])
        require(operation(client, a, uncertain["operation_id"])["state"] == "unknown", "inconclusive reconciliation keeps Unknown")
        dispatch(client, uncertain, status=400)
        client.call("rpc", json.loads(admission["source_body"]), invocation=json.loads(admission["signed_invocation"]))
        require(len(invocations(run, "unknown")) == 1, "neither a rejected new signature nor the exact signed duplicate repeats execution")
        require(admitted_scope(a, client, uncertain["operation_id"]) == admission,
                "the exact signed duplicate preserves the original admission")
        blocked = copy.deepcopy(uncertain)
        blocked["operation_id"] = "unknown-resource-conflict"
        dispatch(client, blocked, status=400)
        client.rpc("resolve_unknown", conversation_id=conversation, task_id=task,
                   task_ref=client.tasks[(client.scope["space_id"], first, conversation, task)]["task_ref"],
                   target_node=a.node_id, session_id="fixture-main", operation_id=uncertain["operation_id"],
                   outcome="failed", evidence="The acceptance process group was stopped and one invocation was independently verified.")
        report["unknown_resolution"] = {"original_admission": admission,
                                        "resolution_admission": resolution_audit(a, client, uncertain["operation_id"]),
                                        "final_operation": operation(client, a, uncertain["operation_id"])}
        require(report["unknown_resolution"]["final_operation"]["state"] == "failed"
                and admitted_scope(a, client, uncertain["operation_id"]) == admission
                and len(invocations(run, "unknown")) == 1,
                "the independent Account resolution commits Failed and preserves the original admission without replay")
        with sqlite3.connect(f"file:{a.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
            require(database.execute("SELECT COUNT(*) FROM resource_reservations WHERE space_id=? AND operation_id=?",
                                     (client.scope["space_id"], uncertain["operation_id"])).fetchone()[0] == 0,
                    "explicit Account resolution releases only the resolved operation's resource reservations")
        passed("Unknown, complete original admission and resource locks survive two restarts without replay", "real_nodes_with_stream_json_fixture")
        require(all(item["notification_id"] != notification
                    for item in client.rpc("notifications", client_id=device)["notifications"]),
                "device acknowledgement remains durable after both restarts")
        gateway = Node(run, "GATEWAY", binary, workspace, fixture, helper, module)
        nodes.append(gateway)
        gateway.configure(roles=["gateway", "replica", "relay"],
                          network={"bootstrap_urls": [a.url.replace("http://", "ws://") + "/api/network/ws"],
                                   "advertise_urls": [gateway.url.replace("http://", "ws://") + "/api/network/ws"],
                                   "mdns": False, "relay_enabled": False})
        gateway.start()
        report["node_enrollment"] = enroll_account_node(a, gateway)
        report["not_checked"].remove("additional same-Account Node enrollment")
        passed("an additional Node installs the same Account Service and only the original Account Grant projection")
        report["gateway"] = verify_account_gateway(a, gateway, conversation, second)
        report["not_checked"].remove("Gateway to registered Project host")
        passed("independent Gateway and host pins protect forwarded Account requests without Gateway private history")
        restored = recover_same_node(a)
        recovered = restored.rpc("conversation", conversation_id=conversation)
        require(any(item["task_id"] == task for item in recovered["tasks"]),
                "account recovery preserves the original signed task history")
        verify_snapshot(restored, restored.rpc("export", conversation_id=conversation), first, {a.node_id: a.node_public_key})
        require(len(invocations(run, "unknown")) == 1 and len(invocations(run, "result")) == 1,
                "recovery does not replay accepted execution")
        report["not_checked"].remove("recovery on real Node wire")
        passed("encrypted typed recovery preserves account, resources and history and rejects the old device")
        if options.remote_topology or options.gateway_topology:
            report["remote_topology"] = verify_remote_topology(run, binary, workspace, fixture, helper, module, nodes,
                                                              gateway=options.gateway_topology)
            report["not_checked"].extend(report["remote_topology"]["not_checked"])
            passed("three independent accounts, native foreign policy import, B/C execution restart and Team isolation",
                   "real_nodes_with_stream_json_fixture")
        verify_no_model_budget(nodes)
        require(file_sha256(binary) == report["binary_sha256"], "binary stays unchanged throughout acceptance")
        require(file_sha256(module) == report["crypto_module_sha256"]
                and file_sha256(module.with_name("vibeface_secure_bg.wasm")) == report["wasm_sha256"],
                "shared cryptography module and WASM stay unchanged throughout acceptance")
        report["fixture_invocations"] = len(invocations(run))
        report["status"] = "passed_incremental"
        print(f"PASS incremental process report: {run / 'report.json'}", flush=True)
    except Exception as error:
        report["status"] = "failed"
        report["failure"] = f"{type(error).__name__}: {error}"
        report["traceback"] = traceback.format_exc()
        raise
    finally:
        for node in reversed(nodes):
            node.stop()
            node.client.close()
        private_json(run / "report.json", report)


def shared_history(client, conversation, task, dispatched, *, remote=None, connection="main"):
    key = (client.scope["space_id"], client.resource_id, dispatched["operation_id"])
    original = client.operations[key]
    signed = client.signed_requests[dispatched["operation_id"]]
    require(original == signed["body"]["request"],
            "history lookup retains the exact originally signed operation")
    status = client.rpc("operation", conversation_id=conversation, task_id=task,
                        target_node=original["target_node"], operation_id=original["operation_id"],
                        remote=remote, connection=connection)
    actual = status["operation"]["request"]
    require(actual == original, "explicit read-only status returns the complete original signed request")
    binding = original["binding"]
    task_ref = client.tasks[(client.scope["space_id"], client.resource_id, conversation, task)]["task_ref"]
    require(binding["binding"]["task_ref"] == task_ref
            and binding["binding"]["space_id"] == client.scope["space_id"]
            and binding["binding"]["conversation_id"] == conversation
            and binding["binding"]["target_node"] == original["target_node"]
            and binding["binding"]["session_id"] == original["session_id"],
            "read-only history stays on the verified original Task creation and execution binding")
    if remote:
        source = client.execution_sources[key]
        require(source["payload"]["operation_id"] == original["operation_id"],
                "foreign history uses the retained signed Project source of the original operation")
        require(status.get("actual"), "foreign history includes the original C-signed execution result")
    result = client.rpc("conversation", conversation_id=conversation, connection=connection)
    record = next((item for item in result["operations"]
                   if item["request"]["operation_id"] == original["operation_id"]), None)
    if not record:
        return None
    require(record["request"] == actual,
            "conversation snapshot retains exactly the original request returned by read-only status")
    return result if any(item["task_id"] == task for item in result["tasks"]) and not any(result["missing"].values()) else None


def unknown_record(client, node, operation_id):
    record = operation(client, node, operation_id)
    return record if record and record["state"] == "unknown" else None


if __name__ == "__main__":
    os.umask(0o077)
    if "--fixture-executor" in sys.argv:
        fixture_executor(sys.argv[1:])
    elif "--fixture-manager" in sys.argv:
        fixture_manager(sys.argv[1:])
    elif "--fixture-codex" in sys.argv:
        fixture_codex(sys.argv[1:])
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--binary", type=Path, required=True)
        parser.add_argument("--artifacts", type=Path, required=True)
        parser.add_argument("--crypto-module", type=Path, default=Path(__file__).resolve().parents[1] / "crates/node/web/generated/vibeface_secure.js")
        parser.add_argument("--codex-bin", default="codex")
        parser.add_argument("--skip-codex", action="store_true", help="omit real Codex transport checks and report that omission")
        parser.add_argument("--gateway-topology", action="store_true", help="run owner-approved independent A Gateway to B Project to C Executor checks")
        parser.add_argument("--remote-topology", action="store_true", help="also run independent-account B/C execution and Team isolation checks")
        modes = parser.add_mutually_exclusive_group()
        modes.add_argument("--phase1", action="store_true", help="run focused durable Manager queue/follow-up process checks with controlled fixtures")
        modes.add_argument("--chat-agent", action="store_true", help="run taskless tools, command isolation and startup subscription process checks")
        arguments = parser.parse_args()
        if (arguments.phase1 or arguments.chat_agent) and (arguments.gateway_topology or arguments.remote_topology):
            parser.error("--phase1/--chat-agent cannot be combined with topology modes; run each acceptance separately")
        if (arguments.phase1 or arguments.chat_agent) and arguments.skip_codex:
            parser.error("--skip-codex only applies to the default topology acceptance")
        try:
            if arguments.chat_agent:
                chat_agent_acceptance(arguments)
            elif arguments.phase1:
                phase1_acceptance(arguments)
            else:
                acceptance(arguments)
        except Exception as error:
            print(f"FAIL {type(error).__name__}: {error}", file=sys.stderr)
            sys.exit(1)
