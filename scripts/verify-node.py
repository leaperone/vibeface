#!/usr/bin/env python3
"""Run local Node process acceptance without making paid provider calls.

The default run uses the installed Codex app-server only for native thread
creation and reads. Delayed execution, restart and notification checks use an
explicit stream-json fixture, never a model. All state and logs stay beneath
--artifacts with private permissions. The script prints no credentials.
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
            and "capability" not in access and "claim_nonce" not in access,
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
    static = rb"(?:^|[;}\n])\s*(?:import|export)\s*(?:[^;\"'`]*?\bfrom\s*)?[\"'`]([^\"'`]+)[\"'`]"
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
            "additional_business_grants": 0}


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


def register_execution_service(node):
    client = node.client
    client.command("open_recovery", envelope=node.backup, password=node.backup_password)
    prepared = client.command("prepare_execution_service", handle="execution-service", trust_ref="execution",
                              source_project={"space": client.scope["space_id"], "resource": client.resource_id})
    registered = finish_native_change(client, prepared, "execution_service_register", password=node.backup_password)
    client.command("connect", url=node.url, node_id=node.node_id, node_public_key=node.node_public_key,
                   connection="execution", trust_ref="execution")
    client.authenticate(resource_id=registered["resource_id"], host_public_key=node.node_public_key,
                        connection="execution", trust_ref="execution", resource_kind="execution_service",
                        grant_leaf=registered["grant_leaf"], activate=False)
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
    fixture = run / "fixture-executor"
    fixture.write_text(f"#!{sys.executable}\nimport os\nos.execv({sys.executable!r}, [{sys.executable!r}, {str(Path(__file__).resolve())!r}, '--fixture-executor', '--fixture-root', {str(run)!r}] + __import__('sys').argv[1:])\n")
    fixture.chmod(0o700)
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


def unknown_record(client, node, operation_id):
    record = operation(client, node, operation_id)
    return record if record and record["state"] == "unknown" else None


class PortableCli:
    def __init__(self, arguments):
        values = [str(value) for value in arguments]
        command = (["script", "-q", "/dev/null", *values] if sys.platform == "darwin"
                   else ["script", "-qefc", shlex.join(values), "/dev/null"])
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.STDOUT, bufsize=0, start_new_session=True)
        self.fd = self.process.stdout.fileno()
        self.output = ""
        self.exit_code = None

    def read(self):
        if select.select([self.fd], [], [], 0.05)[0]:
            try:
                self.output += os.read(self.fd, 65536).decode(errors="replace")
            except OSError:
                pass
        self.exit_code = self.process.poll()
        return self.output

    def until(self, text):
        wait_for("CLI prompt or completion", lambda: text in self.read())

    def send(self, value):
        self.process.stdin.write((value + "\n").encode())
        self.process.stdin.flush()

    def unlock(self, password, new=False):
        self.until("New CLI profile passphrase: " if new else "CLI profile passphrase: ")
        self.send(password)
        if new:
            self.until("Repeat passphrase: ")
            self.send(password)

    def finish(self, expected=0):
        wait_for("CLI exit", lambda: self.read() is not None and self.exit_code is not None)
        require((self.exit_code == 0) == (expected == 0), "portable CLI command exit matches expectation: " + self.output[-2000:])
        return self.output

    def close(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=5)
        self.process.stdin.close()
        self.process.stdout.close()


def portable_access_acceptance(options):
    binary = options.binary.resolve(strict=True)
    module = options.crypto_module.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("portable-access-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    fixture = run / "unused-executor"
    fixture.write_text("#!/bin/sh\nexit 1\n")
    fixture.chmod(0o700)
    helper = Path(__file__).resolve().with_name("node-client.mjs")
    nodes, commands = [], []
    checks = []
    password = "portable-access-fixture-passphrase"

    def passed(description):
        checks.append(description)
        print("PASS " + description, flush=True)

    def cli(arguments, profile, new=False):
        command = PortableCli([binary, *arguments, "--profile", profile, "--profiles-dir", run / "profiles"])
        commands.append(command)
        command.unlock(password, new)
        return command

    def pending(node, reference):
        return node.client.identity_call("enrollment_pending", {"account_id": reference["account_id"], "offer_hash": reference["offer_hash"]})["requests"]

    try:
        relay = Node(run, "relay", binary, workspace, fixture, helper, module)
        target = Node(run, "target", binary, workspace, fixture, helper, module)
        nodes.extend([target, relay])
        relay.configure(roles=["relay"], network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False, "relay_enabled": True}, codex_bin=str(fixture))
        target.configure(network={"bootstrap_urls": [relay.url.replace("http://", "ws://") + "/api/network/ws"], "advertise_urls": [], "mdns": False, "relay_enabled": False}, codex_bin=str(fixture))
        relay.start()
        target.start()
        target.bootstrap()
        card = run / "node.vibeface.json"
        result = subprocess.run([binary, "access", "--data-dir", target.directory, "--via", relay.url, "--out", card], capture_output=True, timeout=20)
        require(result.returncode == 0, "CLI exports a verified relay connection: " + result.stderr.decode(errors="replace"))
        connection = json.loads(card.read_text())
        require(connection["relay"] is True and connection["node_id"] == target.node_id and connection["node_public_key"] == target.node_public_key, "connection pins the private target behind the relay")
        require("capability" not in connection and "claim_nonce" not in connection, "claimed node exports no bootstrap material")
        require(card.stat().st_mode & 0o077 == 0, "connection export uses private file permissions")
        previous_card = card.read_bytes()
        repeated = subprocess.run([binary, "access", "--data-dir", target.directory, "--via", relay.url, "--out", card], capture_output=True, timeout=20)
        require(repeated.returncode != 0 and card.read_bytes() == previous_card, "access refuses to overwrite a connection file")
        wrong_kind = subprocess.run([binary, "client", "enroll", "--invitation-file", card, "--profile", "invalid", "--profiles-dir", run / "profiles"], capture_output=True, timeout=10)
        require(wrong_kind.returncode != 0 and not (run / "profiles" / "invalid").exists(), "public cards cannot enroll devices or create a CLI profile")
        passed("access file pins the private node through a separate relay")

        browser_reference = target.client.command("create_device_invitation", endpoint=relay.url, relay=True, rights=[])
        lost = target.client.command("browser_enrollment", reference=browser_reference, drop_response=True)
        require(lost["status"] == "response_lost" and lost["writes"] == 1, "browser request was committed before its response was dropped")
        resumed = target.client.command("browser_enrollment", reference=browser_reference)
        require(resumed["status"] == "requested" and resumed["request_id"] == lost["request_id"] and resumed["writes"] == 1,
                "production browser enrollment restores its original request without another write")
        require(len(pending(target, browser_reference)) == 1, "browser response loss creates exactly one enrollment request")
        target.client.command("approve_device", reference=browser_reference, request_id=resumed["request_id"])
        target.client.command("browser_possession")
        target.client.command("complete_device", reference=browser_reference, request_id=resumed["request_id"])
        completed = target.client.command("browser_enrollment", reference=browser_reference)
        require(completed["status"] == "completed" and completed["writes"] == 1, "browser completion resumes without publishing another request")
        target.client.authenticate()
        passed("production browser protocol resumes lost responses and completed enrollment")

        reference = target.client.command("create_device_invitation", endpoint=relay.url, relay=True, rights=["enroll_device", "revoke_device"])
        owner_invite = run / "owner-invitation.vibeface.json"
        private_json(owner_invite, reference)
        owner = cli(["client", "enroll", "--connection-file", owner_invite], "owner", new=True)
        requests = wait_for("CLI enrollment request", lambda: pending(target, reference) or None)
        request_id = requests[0]["request_id"]
        owner.close()
        commands.remove(owner)
        owner = cli(["client", "enroll", "--connection-file", owner_invite], "owner")
        requests = wait_for("resumed CLI request", lambda: pending(target, reference) or None)
        require(len(requests) == 1 and requests[0]["request_id"] == request_id, "restarted CLI keeps the original enrollment request")
        target.client.command("approve_device", reference=reference, request_id=request_id)
        wait_for("CLI possession", lambda: any(item["status"] == "possessed" for item in pending(target, reference)))
        target.client.command("complete_device", reference=reference, request_id=request_id)
        owner.finish()
        require(password not in owner.output, "CLI passphrase never appears in terminal output")
        passed("CLI enrollment survives restart and uses its own manager key")

        target.client.close()
        phone_invite = run / "phone-invitation.vibeface.json"
        unpublished_path = run / "missing" / "phone-invitation.vibeface.json"
        failed_output = cli(["invite", "create", "--via", relay.url, "--out", unpublished_path], "owner")
        failed_output.finish(expected=1)
        with sqlite3.connect(f"file:{target.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
            offers_before_retry = database.execute("SELECT count(*) FROM local_values WHERE key GLOB 'enrollment-offer:*'").fetchone()[0]
        created = cli(["invite", "create", "--data-dir", target.directory, "--via", relay.url, "--out", phone_invite], "owner")
        created.finish()
        with sqlite3.connect(f"file:{target.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
            require(database.execute("SELECT count(*) FROM local_values WHERE key GLOB 'enrollment-offer:*'").fetchone()[0] == offers_before_retry,
                    "retry after output failure reuses the published invitation")
        phone_reference = json.loads(phone_invite.read_text())
        require(phone_reference["relay"] is True and phone_reference["node_public_key"] == target.node_public_key, "CLI invitation binds the same relay target")
        passed("enrolled CLI creates invitations after original manager closes")

        phone = cli(["client", "enroll", "--connection-file", phone_invite], "phone", new=True)
        phone.until("Confirm this device in your existing management client.")
        request_line = next(line for line in phone.read().splitlines() if line.startswith("Request: "))
        phone_request = request_line.removeprefix("Request: ").strip()
        listing = cli(["invite", "pending"], "owner")
        require(phone_request in listing.finish(), "CLI lists the real device request")
        denied = cli(["invite", "approve", phone_request], "owner")
        denied.until("Type yes: ")
        denied.send("no")
        require("approval cancelled" in denied.finish(expected=1), "declining approval is explicit")
        unchanged = cli(["invite", "pending"], "owner")
        require(phone_request + " requested " in unchanged.finish(), "cancelled approval leaves the request unauthorized")
        approving = cli(["invite", "approve", phone_request], "owner")
        approving.until("Type yes: ")
        approving.send("yes")
        approving.until("Approved. Waiting for the new device")
        approving.close()
        commands.remove(approving)
        approving = cli(["invite", "approve", phone_request], "owner")
        approving.finish()
        require("Type yes:" not in approving.output, "resumed approval uses the saved exact request")
        phone.finish()
        for command in commands:
            require(password not in command.output, "CLI output does not expose its unlock password")
        passed("CLI explicitly approves an independent device through the relay")
        passed("cancelled approval grants nothing and an interrupted approved request resumes")

        profile_path = run / "profiles" / "owner" / "profile.json"
        require(profile_path.parent.stat().st_mode & 0o077 == 0, "CLI profile directory is private")
        profile_path.chmod(0o644)
        insecure = subprocess.run([binary, "invite", "pending", "--profile", "owner", "--profiles-dir", run / "profiles"], capture_output=True, timeout=10)
        profile_path.chmod(0o600)
        require(insecure.returncode != 0 and b"private regular file" in insecure.stderr, "CLI refuses an exposed profile before requesting its password")
        wrong_password = PortableCli([binary, "invite", "pending", "--profile", "owner", "--profiles-dir", run / "profiles"])
        commands.append(wrong_password)
        wrong_password.unlock("incorrect-fixture-passphrase")
        wrong_password.finish(expected=1)
        require("incorrect-fixture-passphrase" not in wrong_password.output, "incorrect passwords are hidden too")
        passed("CLI rejects exposed profiles and incorrect passwords")

        raw = json.loads((run / "profiles" / "phone" / "profile.json").read_text())
        require(raw["public_key"] != target.node_public_key, "phone profile does not copy the node identity")
        require(raw["public_key"] != json.loads((run / "profiles" / "owner" / "profile.json").read_text())["public_key"], "device profiles have separate keys")
        report = {"ok": True, "checks": checks, "provider_calls": 0, "binary_sha256": file_sha256(binary), "wasm_sha256": file_sha256(module.with_name("vibeface_secure_bg.wasm")), "not_checked": ["rendered browser", "real phone browser", "public HTTPS relay"], "run": str(run)}
        private_json(run / "report.json", report)
        print(json.dumps(report), flush=True)
    finally:
        for command in commands:
            command.close()
        for node in nodes:
            node.client.close()
            node.stop()


if __name__ == "__main__":
    os.umask(0o077)
    if "--fixture-executor" in sys.argv:
        fixture_executor(sys.argv[1:])
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--binary", type=Path, required=True)
        parser.add_argument("--artifacts", type=Path, required=True)
        parser.add_argument("--crypto-module", type=Path, default=Path(__file__).resolve().parents[1] / "crates/node/web/generated/vibeface_secure.js")
        parser.add_argument("--codex-bin", default="codex")
        parser.add_argument("--skip-codex", action="store_true", help="omit real Codex transport checks and report that omission")
        parser.add_argument("--gateway-topology", action="store_true", help="run owner-approved independent A Gateway to B Project to C Executor checks")
        parser.add_argument("--remote-topology", action="store_true", help="also run independent-account B/C execution and Team isolation checks")
        parser.add_argument("--portable-access", action="store_true", help="verify relay files and independent CLI enrollment without provider calls")
        arguments = parser.parse_args()
        try:
            if arguments.portable_access:
                portable_access_acceptance(arguments)
            else:
                acceptance(arguments)
        except Exception as error:
            print(f"FAIL {type(error).__name__}: {error}", file=sys.stderr)
            sys.exit(1)
