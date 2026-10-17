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
from concurrent.futures import ThreadPoolExecutor
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
import socket
import sqlite3
import subprocess
import sys
import time
import traceback
import threading
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
    opened = node.client.command("connect", url=node.url, node_id=node.node_id, check=False)
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


CLIENT_SOURCE = r"""
import init, { Identity, Handshake, recoverySeal, recoveryOpen, verifyMembership } from "MODULE_URL";
import { readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
await init({ module_or_path: await readFile(new URL('./vibeface_secure_bg.wasm', "MODULE_URL")) });
const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', { fatal: true });
const keys = new Map([['client', Identity.generate()], ['root', Identity.generate()]]);
const connections = new Map();
const identity = name => { const key = keys.get(name); if (!key) throw new Error('unknown local identity'); return key; };
const exposed = key => ({ public_key: key.publicKeyHex(), subject_id: key.nodeId() });
async function connect(input) {
  const key = identity(input.identity || 'client');
  const socket = new WebSocket(new URL('/api/network/ws', input.url.replace(/^http/, 'ws')));
  socket.binaryType = 'arraybuffer';
  const queue = [], waiting = [], pending = new Map(), events = [], frames = [];
  let channel, closed = false, sequence = 0;
  function close(error = new Error('channel closed')) {
    if (closed) return;
    closed = true;
    for (const waiter of waiting.splice(0)) waiter.reject(error);
    for (const request of pending.values()) { clearTimeout(request.timer); request.reject(error); }
    pending.clear(); socket.close();
  }
  const receive = () => queue.length ? Promise.resolve(queue.shift()) : closed ? Promise.reject(new Error('channel closed')) : new Promise((resolve, reject) => waiting.push({resolve, reject}));
  socket.addEventListener('message', event => { const waiter = waiting.shift(); if (waiter) waiter.resolve(event.data); else if (queue.length < 512) queue.push(event.data); else close(new Error('bounded receive queue')); });
  socket.addEventListener('close', () => close());
  socket.addEventListener('error', () => close(new Error('WebSocket transport failed')));
  async function control(type) { const frame = await receive(); if (typeof frame !== 'string') throw new Error('control frame is not text'); const value = JSON.parse(frame); if (value.type !== type) throw new Error(`expected ${type}`); }
  const timer = setTimeout(() => close(new Error('handshake timeout')), 15000);
  try {
    await control('welcome');
    socket.send(JSON.stringify(input.relay ? {type:'dial', target:input.node_id, service:'client'} : {type:'direct', service:'client'}));
    await control('ready');
    const handshake = new Handshake(true, key.secretHex(), input.node_id, encoder.encode('vibeface:client:v1'));
    socket.send(handshake.write(new Uint8Array()));
    const response = await receive();
    if (!(response instanceof ArrayBuffer)) throw new Error('Noise response must be binary');
    handshake.read(new Uint8Array(response));
    socket.send(handshake.write(new Uint8Array()));
    if (!handshake.isFinished() || handshake.remoteId() !== input.node_id) throw new Error('target identity mismatch');
    channel = handshake.finish(); handshake.free();
  } catch (error) { close(error); throw error; }
  finally { clearTimeout(timer); }
  (async () => {
    try {
      while (!closed) {
        const frame = await receive();
        if (!(frame instanceof ArrayBuffer)) throw new Error('plaintext application frame rejected');
        const plaintext = channel.openRecord(new Uint8Array(frame));
        if (plaintext === null) continue;
        const value = JSON.parse(decoder.decode(plaintext));
        const request = pending.get(value.id);
        if (request) { pending.delete(value.id); clearTimeout(request.timer); request.resolve(value); }
        else events.push(value);
      }
    } catch (error) { close(error); }
    finally { channel.free(); }
  })();
  return {
    events, frames,
    request(method, params) {
      if (closed) throw new Error('channel closed');
      const id = String(++sequence);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => { pending.delete(id); reject(new Error('request timeout')); }, 20000);
        pending.set(id, {resolve, reject, timer});
        for (const record of channel.sealMessage(encoder.encode(JSON.stringify({id,method,params})))) { frames.push(Buffer.from(record)); socket.send(record); }
      });
    },
    async attack(kind) {
      if (!frames.length) throw new Error('no protected request to attack');
      const record = Buffer.from(frames[frames.length - 1]);
      if (kind === 'tamper') record[record.length - 1] ^= 1;
      socket.send(record);
      for (let attempt = 0; attempt < 50 && !closed; attempt++) await new Promise(resolve => setTimeout(resolve, 100));
      if (!closed) throw new Error('invalid protected frame did not close the channel');
      return {rejected:true};
    },
    close,
  };
}
for await (const line of createInterface({input:process.stdin})) {
  try {
    const input = JSON.parse(line);
    let result;
    if (input.op === 'identity') result = exposed(identity(input.identity || 'client'));
    else if (input.op === 'sign') result = identity(input.identity || 'client').sign(input.domain, encoder.encode(typeof input.payload === 'string' ? input.payload : JSON.stringify(input.payload)));
    else if (input.op === 'backup') {
      const root = identity(input.identity || 'root');
      const epoch = input.recovery_epoch || 1;
      const payload = encoder.encode(JSON.stringify({version:1,network_id:root.nodeId(),root_secret_key:root.secretHex(),genesis:input.state.genesis,recovery_epoch:epoch,decryption_keys:[]}));
      try { result = {version:1,network_id:root.nodeId(),recovery_epoch:epoch,envelope:JSON.parse(decoder.decode(recoverySeal(payload,input.password)))}; }
      finally { payload.fill(0); }
    }
    else if (input.op === 'recover_material') {
      const plaintext = recoveryOpen(encoder.encode(JSON.stringify(input.backup.envelope)),input.password);
      try {
        const payload = JSON.parse(decoder.decode(plaintext));
        const root = Identity.fromSecretHex(payload.root_secret_key);
        keys.get('recovered')?.free(); keys.set('recovered',root);
        result = {network_id:root.nodeId(),recovery_epoch:payload.recovery_epoch,genesis:payload.genesis};
      } finally { plaintext.fill(0); }
    }
    else if (input.op === 'recovery_checkpoint') {
      const root = identity('recovered');
      const verified = verifyMembership(JSON.stringify(input.state), root.nodeId(), root.publicKeyHex());
      try { result = JSON.parse(verified.createRecoveryCheckpoint(root, JSON.stringify(input.members), input.backup_epoch, JSON.stringify(input.prepared))); }
      finally { verified.free(); }
    }
    else if (input.op === 'forget_root') { identity('root').free(); keys.delete('root'); result = {forgotten:true}; }
    else if (input.op === 'connect') { const old = connections.get(input.connection || 'main'); old?.close(); const value = await connect(input); connections.set(input.connection || 'main', value); result = {connected:true}; }
    else { const connection = connections.get(input.connection || 'main'); if (!connection) throw new Error('no connection');
      if (input.op === 'request') result = await connection.request(input.method,input.params || {});
      else if (input.op === 'events') result = connection.events;
      else if (input.op === 'wire') result = connection.frames.map(frame => frame.toString('hex'));
      else if (input.op === 'attack') result = await connection.attack(input.kind);
      else if (input.op === 'close') { connection.close(); result = {closed:true}; }
      else throw new Error('unknown client command');
    }
    process.stdout.write(`${JSON.stringify({ok:true,result})}\n`);
  } catch (error) { process.stdout.write(`${JSON.stringify({ok:false,error:String(error)})}\n`); }
}
for (const connection of connections.values()) connection.close();
for (const key of keys.values()) key.free();
"""


class Client:
    def __init__(self, url, helper, module):
        self.url = url
        self.helper = helper
        self.module = module
        self.network_id = None
        self.principal = None
        self.process = None
        self.node_id = None

    def command(self, op, *, check=True, **arguments):
        if self.process is None:
            self.process = subprocess.Popen(
                ["node", str(self.helper)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
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

    def connect(self, node_id=None, *, relay=False, connection="main"):
        self.node_id = node_id or self.node_id
        return self.command("connect", url=self.url, node_id=self.node_id,
                            relay=relay, connection=connection)

    def call(self, method, params=None, *, status=200, connection="main"):
        response = self.command("request", method=method, params=params or {}, connection=connection)
        error = response.get("error")
        if status == 200:
            require(not error, f"encrypted {method} rejected: {error}")
            return response.get("result")
        require(bool(error), f"encrypted {method} expected rejection")
        if isinstance(error, dict) and error.get("status"):
            require(error["status"] == status, f"encrypted {method} rejection status")
        return response

    def authenticate(self, network=None):
        self.network_id = network or self.network_id
        result = self.call("authenticate", {"network_id": self.network_id})
        self.principal = result["principal"]
        require(self.principal["network_id"] == self.network_id, "client authenticates the pinned personal network")
        return result

    def rpc(self, method, *, status=200, **arguments):
        return self.call("rpc", {"method": method, **arguments}, status=status)

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

    def invite(self, kind, roles=None):
        prepared = self.call("prepare_invite", {"kind": kind, "node_roles": sorted(roles or []),
                                                 "request_id": uuid.uuid4().hex})
        prepared["invite"]["inviter_signature"] = self.command("sign", **prepared["signing"])
        tampered = copy.deepcopy(prepared["invite"])
        tampered["payload"]["network_id"] = "ab" * 32
        self.call("commit_invite", {"invite": tampered, "capability": prepared["capability"]}, status=400)
        return self.call("commit_invite", {"invite": prepared["invite"], "capability": prepared["capability"]})

    def approve(self, request_id):
        prepared = self.call("prepare_pair", {"request_id": request_id})
        prepared["event"]["signature"] = self.command("sign", **prepared["signing"])
        review_signature = self.command("sign", **prepared["review_signing"])
        return self.call("pair_confirm", {"request_id": request_id, "event": prepared["event"],
                                           "inviter_signature": review_signature})

    def pair_client(self, owner, network, name):
        self.url = owner.url
        self.connect(owner.node_id)
        invitation = owner.invite("client")
        prepared = self.call("prepare_pair_request", {**invitation, "name": name,
                                                       "request_id": uuid.uuid4().hex})
        prepared["request"]["proof"] = self.command("sign", **prepared["signing"])
        invalid = copy.deepcopy(prepared["request"])
        invalid["proof"] = "00" * 64
        self.call("pair_request", {"request": invalid}, status=400)
        receipt = self.call("pair_request", {"request": prepared["request"]})
        review_signature = self.command("sign", **receipt["signing"])
        self.call("pair_accept", {"request_id": receipt["request_id"], "signature": review_signature})
        owner.approve(receipt["request_id"])
        result = self.call("pair_status", {"request_id": receipt["request_id"]})
        require(result["status"] == "granted", "new client pairing completes after both signatures")
        self.authenticate(network)
        self.call("prepare_revoke", {"subject_id": owner.node_id}, status=403)

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
        try:
            result = subprocess.run(
                [str(binary), "init", "--data-dir", str(self.directory),
                 "--listen", f"127.0.0.1:{self.port}", "--root", str(workspace)],
                capture_output=True, timeout=45)
        except subprocess.TimeoutExpired:
            raise RuntimeError(f"{name}: CLI initialization timed out") from None
        require(result.returncode == 0, f"{name}: CLI initialization succeeds")
        self.configure(claude_bin=str(fixture), timeout_secs=60)
        result = subprocess.run([str(binary), "access", "--json", "--data-dir", str(self.directory)],
                                capture_output=True, timeout=15)
        require(result.returncode == 0, f"{name}: local access material is available")
        self.access = json.loads(result.stdout)
        require(self.access["node_id"], "local access pins the Node identity")
        self.node_id = self.access["node_id"]

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

    def pair(self, owner, network, *, relay_url=None):
        network_config = json.loads((self.directory / "config.json").read_text())["network"]
        invitation = owner.client.invite("node", ["gateway", "relay", "replica", "scheduler", "executor"])
        path = self.directory / "pair-invitation.json"
        private_json(path, {"url": relay_url or owner.url, "node_id": owner.node_id,
                            "relay": relay_url is not None, "invite": invitation})
        process = subprocess.Popen([str(self.binary), "pair", "--data-dir", str(self.directory),
                                    "--invite-file", str(path), "--name", f"Node {self.name}"],
                                   stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            def pending():
                requests = owner.client.call("pair_requests")
                return next((item for item in requests if item["review"]["new_member"]["subject_id"] == self.node_id
                             and item.get("new_subject_signature")), None)
            review = wait_for(f"{self.name} signed CLI pairing review", pending)
            owner.client.approve(review["request_id"])
            stdout, stderr = process.communicate(timeout=40)
            log = self.directory / "pair-cli.log"
            log.write_bytes(stdout + stderr)
            log.chmod(0o600)
            require(process.returncode == 0, f"{self.name}: pinned CLI pairing completes after management approval")
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
        self.client.pair_client(owner.client, network, f"{self.name} independent client")
        self.client.url = self.url
        self.client.node_id = self.node_id
        self.configure(network=network_config)

    def bootstrap(self):
        self.client.connect(self.node_id)
        root = self.client.command("identity", identity="root")
        prepared = self.client.call("prepare_genesis", {"owner_public_key": root["public_key"],
                                                        "client_name": f"{self.name} management client"})
        state = prepared["state"]
        signing = prepared["signing"]
        state["genesis"]["signature"] = self.client.command("sign", identity="root", **signing)
        self.backup_password = "acceptance-only-encrypted-recovery-passphrase"
        self.backup = self.client.command("backup", state=state, password=self.backup_password)
        private_json(self.directory / "recovery-package.json", self.backup)
        self.client.command("forget_root")
        self.client.call("bootstrap", {"capability": "invalid-capability", "state": state}, status=400)
        result = self.client.call("bootstrap", {"capability": self.access["capability"], "state": state})
        self.client.network_id = state["genesis"]["network_id"]
        self.client.principal = result["principal"]
        require(self.client.principal["can_manage"], "bootstrap grants the local management client")
        return self.client.network_id


class Events:
    def __init__(self, client):
        self.client = client

    @property
    def lines(self):
        return self.client.command("events")

    def start(self):
        self.client.call("subscribe")

    def close(self):
        pass


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
    if record["state"] in {"failed", "rejected", "unknown"}:
        raise RuntimeError(f"operation {operation_id} reached {record['state']}")
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


def verify_no_model_budget(nodes):
    for node in nodes:
        connection = sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True)
        try:
            used = connection.execute("SELECT COALESCE(SUM(used),0) FROM budget_days").fetchone()[0]
            require(used == 0, "ordinary progress and replay never reserve management model budget")
        finally:
            connection.close()


def javascript_dependencies(content):
    static = rb"\b(?:import|export)\s*(?:[^;\"'`]*?\bfrom\s*)?[\"'`]([^\"'`]+)[\"'`]"
    dynamic = rb"\bimport\s*\(\s*[\"'`]([^\"'`]+)[\"'`]"
    return [address.decode() for pattern in (static, dynamic) for address in re.findall(pattern, content)]


def verify_web_assets(node, module):
    def fetch(path, *, method="GET", status=200):
        request = urllib.request.Request(node.url + path, headers={"Origin": node.url}, method=method)
        try:
            response = urllib.request.urlopen(request, timeout=10)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            content = response.read(8 * 1024 * 1024 + 1)
            require(response.status == status and len(content) <= 8 * 1024 * 1024,
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
        content, crypto_headers = fetch("/generated/" + path.name)
        require(hashlib.sha256(content).hexdigest() == file_sha256(path),
                "served browser cryptography matches the verified package resource")
        require(crypto_headers["Cache-Control"] == "no-store", "mutable browser cryptography cannot be cached across updates")
        if path.suffix == ".wasm":
            require(crypto_headers.get_content_type() == "application/wasm", "browser WASM has the correct MIME type")


def verify_incompatible_data(run, binary, workspace, fixture, helper, module):
    node = Node(run, "INCOMPATIBLE", binary, workspace, fixture, helper, module)
    marker = node.directory / "data-epoch.json"
    private_json(marker, {"data_epoch": 1})
    database = node.directory / "node.sqlite3"
    with sqlite3.connect(database) as connection:
        connection.execute("PRAGMA user_version=1")
        connection.execute("CREATE TABLE preserved(value TEXT)")
        connection.execute("INSERT INTO preserved VALUES('keep this data')")
    protected = [database, node.directory / "identity.json", node.directory / "config.json",
                 node.directory / "identity.sqlite3", node.directory / "bootstrap.json"]
    for missing_marker in (False, True):
        if missing_marker:
            marker.unlink()
        checked = protected + ([] if missing_marker else [marker])
        before = {path: file_sha256(path) for path in checked}
        process = subprocess.Popen([str(binary), "serve", "--data-dir", str(node.directory)],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        try:
            stdout, stderr = process.communicate(timeout=10)
            require(process.returncode != 0 and (missing_marker or b"epoch" in (stdout + stderr).lower()),
                    "incompatible or missing data epoch rejects startup before opening stores or providers")
            require(all(file_sha256(path) == digest for path, digest in before.items()),
                    "rejected startup preserves data, identity, config and epoch marker byte for byte")
            require(not missing_marker or not marker.exists(), "rejected startup does not invent a data epoch")
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=5)


def clear_private_binding_cache(node, run, network):
    require(node.name == "C" and node.directory.parent == run and node.process is None,
            "binding cache reset is limited to this stopped private acceptance node")
    with sqlite3.connect(node.directory / "node.sqlite3") as connection:
        before = connection.execute("SELECT hash,envelope FROM events WHERE network_id=? ORDER BY hash", (network,)).fetchall()
        deleted = connection.execute("DELETE FROM local_values WHERE network_id=? AND (key LIKE 'task-binding-current:%' OR key LIKE 'task-binding-record:%')", (network,)).rowcount
        require(connection.execute("SELECT hash,envelope FROM events WHERE network_id=? ORDER BY hash", (network,)).fetchall() == before,
                "fresh scheduler simulation retains every signed DAG event byte")
        require(connection.execute("SELECT COUNT(*) FROM local_values WHERE network_id=? AND (key LIKE 'task-binding-current:%' OR key LIKE 'task-binding-record:%')", (network,)).fetchone()[0] == 0,
                "fresh scheduler has no task binding current or record cache")
    return deleted


def recover_fresh_node(node, backup, password):
    node.start()
    node.client.connect(node.node_id)
    material = node.client.command("recover_material", backup=backup, password=password)
    state = {"genesis": material["genesis"], "checkpoint": None, "events": []}
    challenge = node.client.call("recovery_challenge", {"network_id": material["network_id"],
                                 "owner_public_key": material["genesis"]["owner_public_key"],
                                 "genesis": material["genesis"]})
    signature = node.client.command("sign", identity="recovered", **challenge["signing"])
    params = {"challenge_id": challenge["challenge_id"], "signature": signature,
              "recovery_epoch": material["recovery_epoch"], "state": state}
    node.client.call("recovery_context", params, status=400)
    context = node.client.call("recovery_context", {**params, "capability": node.access["capability"]})
    require(context["view"]["network_id"] == backup["network_id"], "fresh recovery proves the original root network")
    identity = node.client.command("identity")
    info = node.client.call("info")
    members = [
        {**identity, "kind": "client", "name": "Fresh recovery manager", "node_roles": [], "manage": True},
        {"subject_id": info["node_id"], "public_key": info["public_key"], "kind": "node", "name": node.name,
         "node_roles": sorted(info["roles"]), "manage": False},
    ]
    members.sort(key=lambda member: member["subject_id"])
    prepared = node.client.call("prepare_recovery", {"challenge_id": challenge["challenge_id"], "members": members})
    require(prepared["checkpoint"]["epoch"] == backup["recovery_epoch"] + 1,
            "fresh recovery advances beyond the backed-up recovery epoch")
    prepared["checkpoint"] = node.client.command("recovery_checkpoint", state=context["state"], members=members,
                                                   backup_epoch=backup["recovery_epoch"], prepared=prepared)
    result = node.client.call("recover", {"challenge_id": challenge["challenge_id"], "checkpoint": prepared["checkpoint"],
                                          "capability": node.access["capability"]})
    node.client.network_id = backup["network_id"]
    require(result["principal"]["can_manage"] and result["view"]["epoch"] == backup["recovery_epoch"] + 1,
            "fresh node restores independent management at the next recovery epoch")
    node.client.authenticate()
    require(node.client.rpc("conversations")["conversations"] == [],
            "root-only recovery does not invent unavailable business history")
    updated = node.client.command("backup", identity="recovered", state=result["state"],
                                  recovery_epoch=result["view"]["epoch"], password=password)
    private_json(node.directory / "recovery-package.json", updated)
    return updated


def verify_recovery(node, other, nodes, network):
    wrong = other.command("recover_material", backup=node.backup,
                          password="wrong-acceptance-passphrase", check=False)
    require(wrong["ok"] is False, "encrypted recovery package rejects a wrong password")
    missing = copy.deepcopy(node.backup)
    missing["envelope"] = {}
    rejected = other.command("recover_material", backup=missing,
                             password=node.backup_password, check=False)
    require(rejected["ok"] is False, "recovery rejects missing encrypted key material")
    material = other.command("recover_material", backup=node.backup, password=node.backup_password)
    require(material["network_id"] == network, "recovered root retains the original personal fingerprint")
    other.url = node.url
    other.connect(node.node_id)
    wrong_challenge = other.call("recovery_challenge", {"network_id": network,
                                 "owner_public_key": material["genesis"]["owner_public_key"],
                                 "genesis": material["genesis"]})
    wrong_signature = other.command("sign", **wrong_challenge["signing"])
    response = other.call("recovery_context", {"challenge_id": wrong_challenge["challenge_id"],
                                              "signature": wrong_signature}, status=403)
    require(not response.get("result") and "state" not in response and "view" not in response,
            "wrong root proof reveals no private membership")
    challenge = other.call("recovery_challenge", {"network_id": network,
                           "owner_public_key": material["genesis"]["owner_public_key"],
                           "genesis": material["genesis"]})
    signature = other.command("sign", identity="recovered", **challenge["signing"])
    context = other.call("recovery_context", {"challenge_id": challenge["challenge_id"], "signature": signature})
    view = context["view"]
    keep = [member for member in view["members"] if member["subject_id"] not in view["revoked"] and not member["manage"]]
    identity = other.command("identity")
    keep.append({**identity, "kind": "client", "name": "Recovered management client", "node_roles": [], "manage": True})
    keep.sort(key=lambda member: member["subject_id"])
    prepared = other.call("prepare_recovery", {"challenge_id": challenge["challenge_id"], "members": keep})
    for field in ("history", "signing"):
        tampered = copy.deepcopy(prepared)
        if field == "history":
            tampered["checkpoint"]["historical_nodes"] = []
        else:
            tampered["signing"]["domain"] = "identity-genesis"
        rejected = other.command("recovery_checkpoint", state=context["state"], members=keep,
                                 backup_epoch=node.backup["recovery_epoch"], prepared=tampered, check=False)
        require(rejected["ok"] is False, "trusted WASM rejects altered recovery history or signing payload")
    prepared["checkpoint"] = other.command("recovery_checkpoint", state=context["state"], members=keep,
                                            backup_epoch=node.backup["recovery_epoch"], prepared=prepared)
    result = other.call("recover", {"challenge_id": challenge["challenge_id"], "checkpoint": prepared["checkpoint"]})
    other.network_id = network
    require(result["principal"]["can_manage"] and result["view"]["epoch"] > view["epoch"],
            "root recovery establishes an independent manager in a higher epoch")
    history = other.rpc("conversation", conversation_id="acceptance-conversation")
    require(any(message["message_id"] == "message-original"
                and message["text"] == "Keep this context after A disconnects."
                for message in history["messages"]),
            "root recovery retains the exact existing private conversation")
    rejected = node.client.command("request", method="directory", check=False)
    if rejected["ok"]:
        require(rejected["result"].get("error") and not rejected["result"].get("result"),
                "recovered epoch rejects the old client's already-open channel")
    else:
        require("closed" in rejected.get("error", "").lower(),
                "old authorization is explicitly rejected or disconnected")
    node.client.connect()
    node.client.call("authenticate", {"network_id": network}, status=403)
    for peer in nodes:
        if peer is node or peer.name == "C":
            continue
        wait_for("authorized peer installs recovered management epoch", lambda: peer.client.call("directory")["view"]["epoch"] == result["view"]["epoch"])
    return other.command("backup", identity="recovered", state=result["state"],
                         recovery_epoch=result["view"]["epoch"], password=node.backup_password)


def verify_binding_race(a, b, c, network, conversation, workspace):
    task_id = "concurrent-binding-task"
    a.client.rpc("create_task", conversation_id=conversation, task_id=task_id,
                 title="Unique execution binding", goal="Accept only one execution location",
                 acceptance="Conflicting concurrent dispatch is rejected")
    wait_for("C receives the exact task creation", lambda: any(item["task_id"] == task_id
             for item in c.client.rpc("conversation", conversation_id=conversation)["tasks"]))
    requests = [request(network, conversation, target, "binding-race-" + label,
                        "binding-session-" + label,
                        {"type": "open", "executor": "claude_code", "cwd": str(workspace)}, task=task_id)
                for label, target in (("B", b), ("C", c))]
    def submit(client, value):
        return client.command("request", method="rpc", params={"method": "operate", "request": value})
    with ThreadPoolExecutor(max_workers=2) as pool:
        futures = [pool.submit(submit, client, value) for client, value in zip((a.client, c.client), requests)]
        responses = [future.result() for future in futures]
    accepted = [index for index, value in enumerate(responses) if not value.get("error")]
    require(len(accepted) == 1, "two schedulers confirm exactly one execution location")
    winner = accepted[0]
    targets = (b, c)
    wait_for("winning binding open succeeds", lambda: succeeded(a.client, targets[winner], requests[winner]["operation_id"]))
    require(operation(a.client, targets[1 - winner], requests[1 - winner]["operation_id"]) is None,
            "losing execution target never accepts a conflicting operation")


def verify_channel_security(node):
    wrong = node.client.command("connect", url=node.url, node_id="ab" * 32,
                                connection="wrong-identity", check=False)
    require(wrong["ok"] is False, "Noise refuses a substituted target identity")
    for kind in ("tamper", "replay"):
        connection = "negative-" + kind
        node.client.connect(connection=connection)
        node.client.call("authenticate", {"network_id": node.client.network_id}, connection=connection)
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
    module = options.crypto_module.resolve(strict=True)
    require(module.with_name("vibeface_secure_bg.wasm").read_bytes().startswith(b"\x00asm"),
            "real browser cryptography WASM is available")
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    helper.chmod(0o600)
    nodes = []
    report = {"status": "running", "evidence": [], "provider_calls": 0,
              "binary_sha256": file_sha256(binary),
              "not_checked": ["paid model output and SDK reasoning", "real LAN/public ingress",
                              "rendered browser UI", "hostile DAG import rejection (run store module tests separately)"]}
    stream = None
    capture = None
    recovery_client = None

    def passed(description, kind="real_node_processes"):
        report["evidence"].append({"check": description, "kind": kind})
        print("PASS " + description, flush=True)

    try:
        for name in ("A", "B", "C", "R"):
            nodes.append(Node(run, name, binary, workspace, fixture, helper, module))
        a, b, c, relay = nodes
        verify_incompatible_data(run, binary, workspace, fixture, helper, module)
        passed("incompatible data epoch refuses startup without changing existing files")
        for node in nodes:
            node.configure(codex_bin=options.codex_bin)
        capture = RelayCapture(relay.port)
        relay.configure(allowed_origins=[capture.url], network={"bootstrap_urls": [], "advertise_urls": [],
                                                "mdns": False, "relay_enabled": True})
        relay.start()
        for node in (a, b, c):
            node.configure(network={"bootstrap_urls": [capture.url.replace("http:", "ws:") + "/api/network/ws"],
                                    "advertise_urls": [], "mdns": False, "relay_enabled": False})
        a.start()
        verify_cli_status(a, True)
        a.stop()
        verify_cli_status(a, False)
        missing = subprocess.run([str(binary), "status", "--json", "--data-dir", str(run / "not-initialized")],
                                 capture_output=True, timeout=10)
        require(missing.returncode != 0 and not missing.stdout, "CLI status rejects uninitialized directories")
        a.start()
        passed("binary CLI status reports running and stopped nodes without exposing pairing secrets")
        a.client.connect(a.node_id)
        a.client.call("rpc", {"method": "nodes"}, status=401)
        a.client.call("directory", status=401)
        for path in ("/api/register", "/api/login", "/api/rpc"):
            a.client.request(path, {}, status=404)
        network = a.bootstrap()
        verify_web_assets(a, module)
        passed("embedded SPA routes, assets, CSP, cache policy, HEAD and packaged JS/WASM resources")
        a.client.request("/api/tools", {"name": "nodes", "arguments": {}, "call_id": "human-call"}, status=404)
        passed("independent client/root identity, local bootstrap capability and encrypted authorization")

        verify_channel_security(a)
        passed("Noise target binding, tamper and replay reject on real Node channels")

        verify_maintenance(a)
        passed("private maintenance lease, write admission, startup health gate and disabled updates")

        verify_browser_origins(a, [f"http://localhost:{a.port}", f"http://0.0.0.0:{a.port}"])
        a.stop()
        extra_origins = [f"http://192.0.2.10:{a.port}", "https://node.example"]
        a.configure(allowed_origins=extra_origins, allow_insecure_lan=True)
        a.start()
        a.client.connect()
        a.client.authenticate()
        verify_browser_origins(a, extra_origins)
        passed("local aliases and explicit entries validate Host/Origin without account cookies", "real_http_origin_boundaries")

        b.pair(a, network)
        wait_for("A reserves its public relay route",
                 lambda: b'{"type":"reserved"}' in capture.payloads)
        c.pair(a, network, relay_url=capture.url)
        b.start()
        c.start()
        b.client.connect()
        wait_for("B receives its independently paired client grant", lambda: b.client.authenticate())
        c.client.connect()
        wait_for("C receives its independently paired client grant", lambda: c.client.authenticate())
        wait_for("A reaches C through the public relay", lambda: any(n["node_id"] == c.node_id and n["online"] for n in a.client.rpc("nodes")["nodes"]))
        require(len({a.node_id, b.node_id, c.node_id}) == 3, "nodes have distinct stable identities")
        require(len({node.client.command("identity")["subject_id"] for node in (a, b, c)}) == 3,
                "each client installation owns an independent identity")
        require(c.client.network_id == b.client.network_id == network, "independent local clients use the same paired network")
        phone = c.client.command("identity")["subject_id"]
        laptop = b.client.command("identity")["subject_id"]
        passed("three independent nodes pair and route through a foreign public relay")

        other = relay.client
        other_network = relay.bootstrap()
        require(other_network != network, "public relay owns an independent personal network")
        report["topology"] = {"network_id": network, "nodes": [node.node_id for node in (a, b, c)],
                              "relay_node_id": relay.node_id, "relay_network_id": other_network}
        other.command("connect", url=a.url, node_id=a.node_id, connection="foreign")
        other.call("authenticate", {"network_id": network}, connection="foreign", status=401)
        other.call("rpc", {"method": "nodes"}, connection="foreign", status=401)
        conversation, task = "acceptance-conversation", "acceptance-task"
        a.client.rpc("create_conversation", conversation_id=conversation, title="Failover acceptance")
        a.client.rpc("create_task", conversation_id=conversation, task_id=task, title="Explicit acceptance", goal="Keep history across node changes", acceptance="User accepts explicitly")
        a.client.rpc("message", conversation_id=conversation, message_id="message-original", text="Keep this context after A disconnects.")
        a.client.rpc("message", conversation_id=conversation, message_id="message-original", text="Keep this context after A disconnects.")
        a.client.rpc("message", conversation_id=conversation, message_id="message-original", text="changed content", status=400)
        other.rpc("conversation", conversation_id=conversation, status=400)
        require(other.rpc("conversations")["conversations"] == [], "other network cannot list conversation")
        foreign = request(other_network, conversation, b, "foreign", "foreign", {"type": "open", "executor": "claude_code", "cwd": str(workspace)})
        dispatch(a.client, foreign, status=400)
        history = wait_for("shared message and task at C", lambda: shared_history(c.client, conversation, task))
        require(len([m for m in history["messages"] if m["message_id"] == "message-original"]) == 1, "message retry is idempotent")
        passed("shared private history stays isolated from a separately owned public relay")
        require(capture.payloads and not capture.errors, "relay path captured valid unmasked WebSocket records")
        require(b"Keep this context after A disconnects." not in b"".join(capture.payloads),
                "public relay cannot see the private message present on C")
        with sqlite3.connect(f"file:{relay.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
            require(connection.execute("SELECT COUNT(*) FROM events WHERE network_id=?", (network,)).fetchone()[0] == 0,
                    "foreign relay stores no private business events")
        report["relay_capture"] = {"frames": len(capture.payloads), "plaintext_canary_found": False}
        passed("foreign relay transports protected history without plaintext context or private replicas")

        verify_binding_race(a, b, c, network, conversation, workspace)
        passed("concurrent schedulers obtain one task binding and the losing target accepts nothing")

        if not options.skip_codex:
            require(shutil.which(options.codex_bin) is not None or Path(options.codex_bin).is_file(), "Codex executable is available; use --skip-codex for fixture-only run")
            a.client.rpc("create_task", conversation_id=conversation, task_id="native-transport-task", title="Native transport", goal="Open one native thread without a model", acceptance="Native thread ID persists")
            native_open = request(network, conversation, b, "codex-native-open", "native-codex", {"type": "open", "executor": "codex", "cwd": str(workspace)}, task="native-transport-task")
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
        wait_for("accepted dispatch and message at C", lambda: shared_history(c.client, conversation, task, send))
        c.stop()
        deleted_bindings = clear_private_binding_cache(c, run, network)
        a.stop()
        c.start()
        c.client.connect()
        c.client.authenticate()
        wait_for("fresh C reconnects to original executor B", lambda: any(item["node_id"] == b.node_id and item["online"] for item in c.client.rpc("nodes")["nodes"]))
        require(a.process is None, "binding authority A remains offline during fresh C continuation")
        stream = Events(c.client)
        stream.start()
        report["fresh_scheduler"] = {"authority_offline": True, "binding_cache_rows_removed": deleted_bindings,
                                     "explicit_task_id": task, "original_target": b.node_id}
        gate.touch(mode=0o600)
        wait_for("B finishes while A is offline", lambda: succeeded(c.client, b, "fixture-delayed-send"))
        notification = "result:fixture-delayed-send:succeeded"
        wait_for("C receives durable B result notification", lambda: any(n["notification_id"] == notification for n in c.client.rpc("notifications", client_id=phone)["notifications"]))
        wait_for("C receives live event stream update", lambda: len(stream.lines) > 0)
        after = c.client.rpc("conversation", conversation_id=conversation)
        require(next(item for item in after["tasks"] if item["task_id"] == task)["status"] != "accepted", "turn completion never accepts a task")
        require(any(s["session_id"] == "fixture-main" for s in after["sessions"]), "C reads actual B execution state after A exits")
        require(len(invocations(run, "delayed")) == 1, "retries and imported history do not repeat execution")
        wait_for("large result blob replicated to C", lambda: verify_blobs(c, network) >= 1)
        count = verify_snapshot(c.client.rpc("export", conversation_id=conversation))
        require(count > 5, "export contains conversation, dispatch and result history")
        passed("A goes offline; fresh C restores context and receives original B completion without replay", "real_nodes_with_stream_json_fixture")
        passed("large attachment copies with matching content hash and complete DAG closure")
        stream.close()
        stream = None

        c.client.rpc("notifications", client_id=laptop, status=403)
        c.client.rpc("ack", notification_id=notification, client_id=laptop, status=403)
        c.client.rpc("ack", notification_id=notification, client_id=phone)
        require(all(n["notification_id"] != notification for n in c.client.rpc("notifications", client_id=phone)["notifications"]), "ACK removes notification for same client")
        require(any(n["notification_id"] == notification for n in b.client.rpc("notifications", client_id=laptop)["notifications"]), "ACK does not consume another client delivery")
        session = c.client.rpc("session", target_node=b.node_id, session_id="fixture-main")["session"]
        offline_gate = run / "complete-offline"
        offline = request(network, conversation, b, "fixture-offline-send", "fixture-main", {"type": "send", "text": json.dumps({"label": "offline", "gate": str(offline_gate)})}, revision=session["revision"], task=task)
        require(a.process is None, "fresh C submits new explicit-task work while authority A stays offline")
        dispatch(c.client, offline)
        wait_for("second fixture invocation", lambda: len(invocations(run, "offline")) == 1)
        dispatch(c.client, offline)
        stale = copy.deepcopy(offline)
        stale["operation_id"] = "stale-concurrent"
        dispatch(c.client, stale, status=400)
        overlap = request(network, conversation, b, "overlapping-path", "overlapping-session", {"type": "open", "executor": "claude_code", "cwd": str(child_workspace)})
        dispatch(c.client, overlap, status=400)
        passed("fresh C derives the binding from DAG and dispatches new work to original B with A offline", "real_nodes_with_stream_json_fixture")
        passed("session CAS, overlapping-directory exclusion and execution deduplication", "real_nodes_with_stream_json_fixture")
        c.stop()
        offline_gate.touch(mode=0o600)
        wait_for("B completes without any connected client", lambda: succeeded(b.client, b, "fixture-offline-send"))
        c.start()
        c.client.connect()
        c.client.authenticate()
        wait_for("persistent notification delivered after reconnect", lambda: any(n["notification_id"] == "result:fixture-offline-send:succeeded" for n in c.client.rpc("notifications", client_id=phone)["notifications"]))
        require(all(n["notification_id"] != notification for n in c.client.rpc("notifications", client_id=phone)["notifications"]), "client ACK remains durable after restart")
        passed("offline notifications replay after reconnect and per-client ACK persists", "real_nodes_with_stream_json_fixture")

        session = c.client.rpc("session", target_node=b.node_id, session_id="fixture-main")["session"]
        unknown = request(network, conversation, b, "fixture-unknown-send", "fixture-main", {"type": "send", "text": json.dumps({"label": "unknown", "gate": str(run / "never-complete")})}, revision=session["revision"], task=task)
        dispatch(c.client, unknown)
        wait_for("interrupted fixture is executing", lambda: len(invocations(run, "unknown")) == 1)
        lease, state = maintenance(b, "acquire")
        lease.close()
        require(state["status"] == "busy", "running execution postpones updates")
        b.stop()
        b.start()
        b.client.connect()
        b.client.authenticate()
        record = wait_for("running operation recovers as Unknown", lambda: unknown_record(b.client, b, unknown["operation_id"]))
        require(record["state"] == "unknown", "restart does not report success")
        lease, state = maintenance(b, "acquire")
        lease.close()
        require(state["status"] == "busy", "Unknown execution postpones updates")
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
        a.client.connect()
        a.client.authenticate()
        wait_for("owner reconnects to B", lambda: any(n["node_id"] == b.node_id and n["online"] for n in a.client.rpc("nodes")["nodes"]))
        proposal = a.client.call("prepare_revoke", {"subject_id": c.node_id})
        proposal["event"]["signature"] = a.client.command("sign", **proposal["signing"])
        a.client.call("revoke", {"event": proposal["event"]})
        wait_for("B installs device revocation", lambda: any(n["node_id"] == c.node_id and n["revoked"] for n in b.client.rpc("nodes")["nodes"]))
        wait_for("known revocation rejects C remote access", lambda: revoked_remote(c.client, b))
        a.client.rpc("operation", target_node=c.node_id, operation_id="anything", status=400)
        require(len(invocations(run)) == 3, "sync/restart/revocation never creates extra executor calls")
        passed("known revocation rejects further cross-node data and execution access")
        recovery_client = Client(a.url, helper, module)
        backup = verify_recovery(a, recovery_client, [a, b, c], network)
        passed("encrypted recovery rejects wrong or missing material and restores independent management without losing history")
        for name in ("FRESH-EPOCH-2", "FRESH-EPOCH-3"):
            fresh = Node(run, name, binary, workspace, fixture, helper, module)
            nodes.append(fresh)
            fresh.configure(roles=["gateway", "replica"], codex_bin=options.codex_bin)
            backup = recover_fresh_node(fresh, backup, a.backup_password)
        require(backup["recovery_epoch"] == 4, "epoch 2 and epoch 3 backups both restore on independent fresh nodes")
        passed("epoch 2 and 3 encrypted backups restore fresh nodes at higher epochs without fabricating history")
        verify_no_model_budget(nodes)
        require(len(invocations(run)) == 3, "recovery never replays previously accepted execution")
        passed("ordinary notifications, history replay and identity recovery use no management-model budget")
        require(file_sha256(binary) == report["binary_sha256"], "binary did not change during acceptance")
        report["status"] = "passed"
        report["fixture_invocations"] = len(invocations(run))
        report["event_count"] = count
        print(f"PASS acceptance report: {run / 'report.json'}", flush=True)
    except Exception as error:
        report["status"] = "failed"
        report["failure"] = f"{type(error).__name__}: {error}"
        report["traceback"] = traceback.format_exc()
        raise
    finally:
        if stream:
            stream.close()
        private_json(run / "report.json", report)
        if recovery_client:
            recovery_client.close()
        if capture:
            capture.close()
        for node in reversed(nodes):
            node.stop()
            node.client.close()
        private_json(run / "report.json", report)


def shared_history(client, conversation, task, dispatched=None):
    result = client.rpc("conversation", conversation_id=conversation)
    if dispatched:
        record = next((item for item in result["operations"] if item["request"]["operation_id"] == dispatched["operation_id"]), None)
        if not record or not record["request"].get("binding"):
            return None
        actual = record["request"]
        if any(actual[key] != dispatched[key] for key in ("target_node", "session_id", "task_id", "action")):
            return None
    return result if (any(m["message_id"] == "message-original" for m in result["messages"])
                      and any(t["task_id"] == task for t in result["tasks"])
                      and not any(result["missing"].values())) else None


def unknown_record(client, node, operation_id):
    record = operation(client, node, operation_id)
    return record if record and record["state"] == "unknown" else None


def revoked_remote(client, target):
    client.rpc("session", target_node=target.node_id, session_id="fixture-main", status=400)
    return True


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
        arguments = parser.parse_args()
        try:
            acceptance(arguments)
        except Exception as error:
            print(f"FAIL {type(error).__name__}: {error}", file=sys.stderr)
            sys.exit(1)
