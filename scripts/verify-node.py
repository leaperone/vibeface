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
--network-reliability records the NET-1 process baseline without Codex or models.
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
import shlex
import socket
import struct
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


class RelayCapture:
    def __init__(self, upstream_port, *, retain_payloads=True):
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
        self.retain_payloads = retain_payloads
        self.payload_bytes = 0
        self.frame_count = 0
        self.binary_payload_bytes = 0
        self.binary_frame_count = 0
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
                    self.payload_bytes += len(payload)
                    self.frame_count += 1
                    if buffer[0] & 15 == 2:
                        self.binary_payload_bytes += len(payload)
                        self.binary_frame_count += 1
                    if self.retain_payloads or buffer[0] & 15 == 1:
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
async function networkProbe(input) {
  const sockets = [], identities = [];
  async function control() {
    const socket = new WebSocket(new URL('/api/network/ws', input.url.replace(/^http/, 'ws')));
    const queue = [], waiting = [];
    let closed = false;
    socket.addEventListener('message', event => {
      const waiter = waiting.shift();
      if (waiter) waiter(JSON.parse(event.data)); else queue.push(JSON.parse(event.data));
    });
    const close = () => { closed = true; for (const waiter of waiting.splice(0)) waiter(null); };
    socket.addEventListener('close', close); socket.addEventListener('error', close);
    const next = () => queue.length ? Promise.resolve(queue.shift()) : closed ? Promise.resolve(null) : new Promise(resolve => {
      const done = value => { clearTimeout(timer); resolve(value); };
      const timer = setTimeout(() => { const index = waiting.indexOf(done); if (index >= 0) waiting.splice(index, 1); resolve(null); }, 1500);
      waiting.push(done);
    });
    const value = { socket, next, get closed() { return closed; } };
    sockets.push(value);
    const welcome = await next();
    if (welcome?.type !== 'welcome') throw new Error('probe did not receive relay welcome');
    value.nonce = welcome.nonce;
    return value;
  }
  function key() { const value = Identity.generate(); identities.push(value); return value; }
  async function reserve(owner) {
    const connection = await control();
    const proof = JSON.stringify({nonce:connection.nonce,node_id:owner.nodeId()});
    connection.socket.send(JSON.stringify({type:'reserve',node_id:owner.nodeId(),public_key:owner.publicKeyHex(),signature:owner.sign('relay-reserve',encoder.encode(proof))}));
    const response = await connection.next();
    return { connection, accepted:response?.type === 'reserved',outcome:response?.type || (connection.closed ? 'closed' : 'timeout') };
  }
  async function exchange(message) {
    const connection = await control(); connection.socket.send(JSON.stringify(message));
    return await connection.next();
  }
  try {
    if (input.kind === 'directory') {
      const now = Math.floor(Date.now() / 1000), short = key(), healthy = key();
      async function publish(owner, expires) {
        const record = {node_id:owner.nodeId(),public_key:owner.publicKeyHex(),direct_urls:[],relay_urls:[],issued_at:now,expires_at:expires,signature:''};
        record.signature = owner.sign('reachability',encoder.encode(JSON.stringify(record)));
        if ((await exchange({type:'publish',record}))?.type !== 'published') throw new Error('signed fixture record was rejected');
      }
      await publish(short, now + 2); await publish(healthy, now + 120);
      const before = await exchange({type:'lookup',target:short.nodeId()});
      await new Promise(resolve => setTimeout(resolve, Math.max(0, (now + 2) * 1000 - Date.now()) + 100));
      const expired = await exchange({type:'lookup',target:short.nodeId()});
      const good = await exchange({type:'lookup',target:healthy.nodeId()});
      return {short_record_present_before_expiry:before?.record?.node_id === short.nodeId(),expired_record_removed:expired?.type === 'record' && expired.record === null,healthy_record_retained:good?.record?.node_id === healthy.nodeId()};
    }
    if (input.kind === 'reservations') {
      const owner = key(), first = await reserve(owner), duplicate = await reserve(owner);
      if (!first.accepted) throw new Error(`first isolated reservation failed ${first.outcome}`);
      let accepted = 1, rejected = 0, timeouts = Number(duplicate.outcome === 'timeout');
      for (let index = 1; index < 65; index++) {
        const result = await reserve(key());
        if (result.accepted) accepted++;
        else if (result.outcome === 'closed') rejected++;
        else if (result.outcome === 'timeout') timeouts++;
        else throw new Error(`unexpected reservation reply ${result.outcome}`);
      }
      const now = Math.floor(Date.now() / 1000);
      const record = {node_id:owner.nodeId(),public_key:owner.publicKeyHex(),direct_urls:[],relay_urls:[],issued_at:now,expires_at:now+120,signature:''};
      record.signature = owner.sign('reachability',encoder.encode(JSON.stringify(record)));
      first.connection.socket.send(JSON.stringify({type:'publish',record}));
      await new Promise(resolve => setTimeout(resolve, 100));
      const lookup = await exchange({type:'lookup',target:owner.nodeId()});
      return {unique_identities:65,reserve_requests:66,accepted,rejected,timeouts,duplicate_identity_outcome:duplicate.outcome,duplicate_identity_accepted:duplicate.accepted,first_reservation_still_open:!first.connection.closed,first_reservation_published_after_capacity:lookup?.record?.node_id === owner.nodeId(),directory_query_after_capacity:lookup?.type === 'record'};
    }
    throw new Error('unknown network probe');
  } finally {
    for (const connection of sockets) connection.socket.close();
    for (const value of identities) value.free();
  }
}
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
    if (input.op === 'network_probe') result = await networkProbe(input);
    else if (input.op === 'identity') result = exposed(identity(input.identity || 'client'));
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
        original_config = json.loads((self.directory / "config.json").read_text())
        network_config = original_config["network"]
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
        paired_config = json.loads((self.directory / "config.json").read_text())
        require(paired_config["codex_socket"] == original_config["codex_socket"],
                "pairing preserves managed or explicit Codex socket configuration")
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


def fixture_manager_settings(run, workspace):
    return {"provider": "codex_sdk", "workspace": str(workspace), "model": "controlled-fixture",
            "cursor_sdk_bin": "unused-fixture", "node_bin": sys.executable,
            "codex_sdk_script": str(fixture_wrapper(run, "fixture-manager", "--fixture-manager"))}


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
    read_only = "This management call is read-only." in prompt
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
              "node_url": initial["tool_env"]["VIBEFACE_NODE_URL"]}
    fixture_record(run, "manager-invocations.jsonl", record)
    schemas = initial.get("custom_tools", {})
    expected_tools = {"nodes", "context", "execution_status", "call_lookup"}
    if not read_only:
        expected_tools.update({"command", "delegate", "continue_execution", "stop_execution"})
    require(set(schemas) == expected_tools, "sidecar receives the authoritative scoped high-level tool catalog")
    fixture_record(run, "manager-tool-catalog.jsonl", {"trigger": trigger, "tools": schemas})

    def tool(name, arguments, expected=True, rejection="read-only", call_ref=None, discard=False):
        environment = initial["tool_env"]
        arguments = dict(arguments)
        if call_ref and name in {"command", "delegate", "continue_execution", "stop_execution"}:
            arguments["call_ref"] = call_ref
        request = urllib.request.Request(
            environment["VIBEFACE_NODE_URL"] + "/api/tools",
            json.dumps({"name": name, "arguments": arguments, "call_id": call_ref or uuid.uuid4().hex}).encode(),
            {"Content-Type": "application/json", "Authorization": "Bearer " + environment["VIBEFACE_TOOL_TOKEN"]})
        started = time.monotonic()
        fixture_record(run, "manager-tool-starts.jsonl", {"conversation_id": context["conversation_id"],
                       "trigger": trigger, "name": name, "started_at_ms": int(time.time() * 1000)})
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                value = None if discard else json.load(response)
            accepted = True
        except urllib.error.HTTPError as error:
            raw_error = error.read(16385)
            constraint = ""
            diagnostic = "HTTP error response truncated: exceeds 16384 bytes"
            if len(raw_error) <= 16384:
                try:
                    payload = json.loads(raw_error)
                    constraint = payload.get("error", "") if isinstance(payload, dict) else ""
                    if not isinstance(constraint, str):
                        constraint = ""
                    diagnostic = constraint or "HTTP error response has no string error field"
                except (ValueError, UnicodeDecodeError):
                    diagnostic = "HTTP error response is not valid JSON"
            fixture_record(run, "manager-tool-errors.jsonl", {
                "conversation_id": context["conversation_id"], "trigger": trigger, "name": name,
                "http_status": error.code,
                "error": diagnostic.replace(environment["VIBEFACE_TOOL_TOKEN"], "[redacted]")[:2048],
                "elapsed_ms": round((time.monotonic() - started) * 1000)})
            require(error.code == 400 and rejection in constraint,
                    "negative tool check rejects specifically for its expected authority constraint")
            value, accepted = None, False
        except (OSError, http.client.HTTPException) as error:
            elapsed_ms = round((time.monotonic() - started) * 1000)
            fixture_record(run, "manager-tool-errors.jsonl", {
                "conversation_id": context["conversation_id"], "trigger": trigger, "name": name,
                "error_type": type(error).__name__, "elapsed_ms": elapsed_ms})
            raise RuntimeError("fixture tool transport failed with " + type(error).__name__
                               + " after " + str(elapsed_ms) + " ms") from None
        fixture_record(run, "manager-tools.jsonl", {"conversation_id": context["conversation_id"],
                       "trigger": trigger, "name": name, "accepted": accepted, "expected": expected,
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
                if not directive.get("cancel_unknown"):
                    emit({"type": "cancelled", "session_id": native,
                          "confirmation": "managed_processes_exited"})
                return
    emit({"type": "result", "session_id": native, "final_response": label + ":complete"})


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


def scheduler_records(node, network, kind="job"):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        rows = connection.execute("SELECT value FROM local_values WHERE network_id=? AND key LIKE ?",
                                  (network, "scheduler:v1:" + kind + ":%"))
        return [json.loads(row[0]) for row in rows]


def scheduler_record(node, network, trigger, kind="job"):
    return next((row for row in scheduler_records(node, network, kind) if row["trigger"] == trigger), None)


def require_quiet(description, predicate):
    # Cover two complete three-second durable scheduler reconciliation intervals.
    deadline = time.monotonic() + 7
    while time.monotonic() < deadline:
        require(predicate(), description)
        time.sleep(0.2)


def local_operation(node, network, operation_id):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        row = connection.execute("SELECT request,state,result FROM operations WHERE network_id=? AND operation_id=?",
                                 (network, operation_id)).fetchone()
    return {"request": json.loads(row[0]), "state": json.loads(row[1]), "result": json.loads(row[2])} if row else None


def local_events(node, network, conversation):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        rows = connection.execute("SELECT envelope FROM events WHERE network_id=? AND scope_id=?",
                                  (network, conversation)).fetchall()
    return [json.loads(json.loads(row[0])["canonical_body"]) for row in rows]


def chat_agent_checks(run, workspace, a, b, c, network, passed):
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
        job = scheduler_record(a, network, receipt["trigger"])
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
            for job in scheduler_records(a, network) if job["conversation_id"] == name))
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
        (lambda row: row if row and row["state"] == "unknown" else None)(local_operation(b, network, operation_id)))
    require_quiet("restart never reruns the unknown command", lambda: marker.read_text() == "once")
    require(local_operation(b, network, operation_id)["request"]["session_id"] is None,
            "unknown command recovery does not create an SDK session")
    wait_for("restarted executor reconnects before subsequent work", lambda:
        any(row["node_id"] == b.node_id and row["online"] and row.get("capabilities")
            for row in a.client.rpc("nodes")["nodes"]))
    passed("running command restart preserves Unknown and its original file effect without replay", operation_id=operation_id)


def bot_checks(run, workspace, a, b, c, network, passed, outsider):
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
    require(outsider.client.rpc("bots")["bots"] == [], "another personal network cannot list these Bots")
    independent = outsider.client.rpc("create_bot", **{**definitions[0], "name": "Other network", "project": None})["bot"]
    require(outsider.client.rpc("bots")["bots"] == [independent]
            and sorted(a.client.rpc("bots")["bots"], key=lambda bot: bot["bot_id"]) == stored,
            "the same Bot identity in another network cannot replace or reveal this network's definition")
    require_quiet("creating Bot definitions does not launch a model", lambda: len(fixture_records(run)) == before_calls)

    def send(definition, label):
        receipt = a.client.rpc("message", conversation_id=definition["conversation_id"], message_id=label,
                               text=json.dumps({"chat_agent": True, "label": label}))
        wait_for("Bot message gets one final reply", lambda:
            (scheduler_record(a, network, receipt["trigger"]) or {}).get("state") == "completed")
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
    c.stop()
    c.configure(manager=fixture_manager_settings(run, workspace), codex_bin=sys.executable)
    c.start()
    c.client.connect()
    c.client.authenticate()
    a.stop()
    a.start()
    a.client.connect()
    a.client.authenticate()
    require_quiet("restart and activating a Manager on imported Bot history never run a model", lambda:
        len(fixture_records(run)) == before_calls and not scheduler_records(c, network))
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


def startup_subscription_checks(run, workspace, a, b, c, network, passed):
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
        return [event["body"] for event in local_events(b, network, name)
                if event["body"].get("type") == "notification_raised" and event["body"].get("kind") == kind]

    wait_for("controlled Codex turn starts", lambda: len(rpc("turn/start")) == 1)
    wait_for("management dispatch commits before disconnect", lambda:
        scheduler_record(a, network, receipt["trigger"])["state"] == "completed")
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
        (local_operation(b, network, operation_id) or {}).get("state") == "succeeded")
    wait_for("duplicate completion has one result notification", lambda:
        sum(event["body"].get("notification_id") == "result:" + operation_id + ":succeeded"
            for event in local_events(b, network, name)) == 1)
    wait_for("native completion returns once to original Scheduler without Web", lambda:
        len([row for row in fixture_records(run) if row["conversation_id"] == name and row["autonomous"]]) == 1
        and any(job["conversation_id"] == name and job["kind"] == "autonomous" and job["state"] == "completed"
                for job in scheduler_records(a, network)))
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
        and (scheduler_record(a, network, receipt["trigger"]) or {}).get("state") == "completed")
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
        (local_operation(b, network, operation_id) or {}).get("state") == "succeeded")
    require(len(rpc("turn/start")) == 2, "missed completion recovery never sends another native turn")
    for node in (a, b, c):
        node.client.connect()
        node.client.authenticate()
    passed("startup reconciles a completion missed while offline through native read RPCs without replay",
           operation_id=operation_id, native_thread=native)


def chat_agent_acceptance(options):
    binary = options.binary.resolve(strict=True)
    module = options.crypto_module.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("chat-agent-process-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir()
    fixture = fixture_wrapper(run, "fixture-executor", "--fixture-executor")
    codex = fixture_wrapper(run, "fixture-codex", "--fixture-codex")
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    nodes = []
    report = {"status": "running", "source_binary": str(binary), "binary_sha256": file_sha256(binary),
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
        a.configure(manager=fixture_manager_settings(run, workspace), daily_decision_limit=100)
        a.start()
        network = a.bootstrap()
        outsider.start()
        outsider.bootstrap()
        for node in (b, c):
            node.configure(network={"bootstrap_urls": [a.url.replace("http:", "ws:") + "/api/network/ws"],
                                    "advertise_urls": [node.url.replace("http:", "ws:") + "/api/network/ws"],
                                    "mdns": False, "relay_enabled": True})
            node.pair(a, network)
            node.start()
            node.client.connect()
            wait_for(node.name + " receives its paired client grant", lambda: node.client.authenticate())
        wait_for("private Nodes can reach each other", lambda:
            all(any(row["node_id"] == target.node_id and row["online"] for row in a.client.rpc("nodes")["nodes"])
                for target in (b, c)))
        bot_checks(run, workspace, a, b, c, network, passed, outsider)
        chat_agent_checks(run, workspace, a, b, c, network, passed)
        startup_subscription_checks(run, workspace, a, b, c, network, passed)
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
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("phase1-process-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    binary = run / "vibeface"
    shutil.copy2(source_binary, binary)
    workspace = run / "workspace"
    workspace.mkdir()
    fixture = fixture_wrapper(run, "fixture-executor", "--fixture-executor")
    manager = fixture_manager_settings(run, workspace)
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    nodes = []
    report = {"status": "running", "source_binary": str(source_binary),
              "binary_sha256": file_sha256(binary), "provider_calls": 0,
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
        require(proof["version"] == 1 and proof["network_id"] == network
                and proof["conversation_id"] == name and proof["trigger"] == receipt["trigger"]
                and proof["target_node"] == a.node_id, "durable receipt has the exact accepted identity")
        return receipt, text

    def state(receipt, expected):
        record = scheduler_record(a, network, receipt["trigger"])
        return record if record and record["state"] == expected else None

    def completed(receipt):
        record = scheduler_record(a, network, receipt["trigger"])
        if record and record["state"] in {"failed", "unknown", "cancelled"}:
            raise RuntimeError("management job reached " + record["state"] + ": " + record.get("detail", ""))
        return record if record and record["state"] == "completed" else None

    def reconnect(node):
        node.client.connect()
        wait_for(node.name + " receives its paired client grant", lambda: node.client.authenticate())

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
        network = a.bootstrap()
        for node in (b, c):
            node.configure(network={"bootstrap_urls": [a.url.replace("http:", "ws:") + "/api/network/ws"],
                                    "advertise_urls": [node.url.replace("http:", "ws:") + "/api/network/ws"],
                                    "mdns": False, "relay_enabled": True})
            node.pair(a, network)
            node.start()
            reconnect(node)
        wait_for("three private Nodes become mutually reachable", lambda:
                 all(any(row["node_id"] == target.node_id and row["online"]
                         for row in a.client.rpc("nodes")["nodes"]) for target in (b, c)))
        c.client.call("subscribe")
        passed("three real Nodes pair with independent clients; Manager uses a controlled protocol fixture")

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
        cancelled_reply = c.client.rpc("cancel_manager", conversation_id=name, trigger=cancelled_queued["trigger"])
        require(cancelled_reply.get("cancel_requested") is True
                and cancelled_reply["job"]["state"] == "cancelled"
                and "attempt" not in cancelled_reply["job"],
                "third entry cancels queued work before any SDK attempt")
        cancelled_job = wait_for("queued cancellation persists at the original Scheduler",
                                 lambda: state(cancelled_queued, "cancelled"))
        require(all(cancelled_job[key] == queued_before[key] for key in ("message_id", "basis_heads", "sequence")),
                "queued cancellation preserves the original accepted message and basis")
        repeated_cancel = a.client.rpc("cancel_manager", conversation_id=name, trigger=cancelled_queued["trigger"])
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
            reply = a.client.rpc("cancel_manager", conversation_id=name, trigger=cancelled["trigger"])
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
                     evidence="Owned fixture process terminated; original attempt admitted no business operations.")
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
            connection.execute("CREATE TRIGGER phase1_drop_receipt BEFORE UPDATE OF value ON local_values "
                "WHEN NEW.key LIKE 'scheduler:v1:outbox:%' AND json_extract(NEW.value,'$.delivered')=1 "
                "BEGIN SELECT RAISE(ABORT,'phase1 injected lost durable receipt'); END")
        remote, remote_text = send(c.client, name, "remote-receipt")
        require(remote["queued"] is False and remote["state"] == "pending_delivery",
                "source acceptance never masquerades as a remote durable queue ACK")
        wait_for("remote target commits its only reply despite source receipt loss", lambda: completed(remote))
        pending = scheduler_record(c, network, remote["trigger"], "outbox")
        require(pending and not pending["delivered"], "injected receipt failure leaves durable source outbox pending")
        c.stop()
        with sqlite3.connect(c.directory / "node.sqlite3") as connection:
            connection.execute("DROP TRIGGER phase1_drop_receipt")
        c.start()
        reconnect(c)
        wait_for("restarted source reconciles original target receipt", lambda:
                 scheduler_record(c, network, remote["trigger"], "outbox")["delivered"])
        duplicate = c.client.rpc("message", conversation_id=name, message_id="remote-receipt", text=remote_text,
                                 scheduler_node=a.node_id, report_results=False)
        require(duplicate["trigger"] == remote["trigger"] and len(calls(name)) == 1,
                "same source trigger and target deduplicate after missing durable ACK")
        passed("durable ACK loss and source restart retry only the original target without duplicate invocation",
               fault="source ACK persistence rejected after target admission", trigger=remote["trigger"])

        chat_agent_checks(run, workspace, a, b, c, network, passed)

        name = new_conversation("phase1-budget")
        with sqlite3.connect(a.directory / "node.sqlite3") as connection:
            connection.execute("INSERT INTO budget_days(network_id,day,used) VALUES(?,?,?) "
                "ON CONFLICT(network_id,day) DO UPDATE SET used=excluded.used",
                (network, time.strftime("%Y-%m-%d", time.gmtime()), 100))
        human, _ = send(a.client, name, "human-after-autonomous-budget")
        wait_for("human chat ignores exhausted autonomous budget", lambda: completed(human))
        wait_for("remote entry knows the budget-test conversation", lambda: conversation(c.client, name))
        remote_human, _ = send(c.client, name, "remote-human-after-autonomous-budget")
        wait_for("verified remote human chat also ignores exhausted autonomous budget", lambda: completed(remote_human))
        with sqlite3.connect(f"file:{a.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
            used = connection.execute("SELECT used FROM budget_days WHERE network_id=? AND day=?",
                                      (network, time.strftime("%Y-%m-%d", time.gmtime()))).fetchone()[0]
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
                      len(calls()) == before and not scheduler_records(c, network))
        passed("activating a Manager on imported history does not replay shared business events")
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
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("node-acceptance-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    child_workspace = workspace / "child"
    child_workspace.mkdir()
    fixture = fixture_wrapper(run, "fixture-executor", "--fixture-executor")
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
        verify_direct_start(run, binary)
        passed("direct CLI startup initializes safely and restarts the same node without provider calls")
        for name in ("A", "B", "C", "R"):
            nodes.append(Node(run, name, binary, workspace, fixture, helper, module))
        a, b, c, relay = nodes
        verify_incompatible_data(run, binary, workspace, fixture, helper, module)
        passed("incompatible data epoch refuses startup without changing existing files")
        for node in nodes:
            node.configure(codex_bin=options.codex_bin)
        nodes[0].configure(manager=fixture_manager_settings(run, workspace), codex_bin=sys.executable)
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
        after = wait_for("fresh C reads the completed original binding",
                         lambda: shared_history(c.client, conversation, task, send))
        require(next(item for item in after["tasks"] if item["task_id"] == task)["status"] != "accepted", "turn completion never accepts a task")
        require(any(s["session_id"] == "fixture-main" for s in after["sessions"]), "C reads actual B execution state after A exits")
        require(next(item for item in after["operations"] if item["request"]["operation_id"] == send["operation_id"])["state"] == "succeeded",
                "fresh C caches the original executor's known successful result")
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


def network_reliability_acceptance(options):
    binary = options.binary.resolve(strict=True)
    repository = Path(__file__).resolve().parents[1]
    artifacts = options.artifacts.resolve()
    require(artifacts.is_relative_to(repository / ".artifacts"),
            "NET-1 artifacts remain inside this worktree's .artifacts")
    ignored = subprocess.run(["git", "check-ignore", "--quiet", str(artifacts / "probe")],
                             cwd=repository, capture_output=True)
    require(ignored.returncode == 0, "NET-1 artifacts are ignored before fixture creation")
    artifacts.mkdir(parents=True, exist_ok=True)
    run = artifacts / ("m0-baseline-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    fixture = fixture_wrapper(run, "fixture-executor", "--fixture-executor")
    module = options.crypto_module.resolve(strict=True)
    require(module.with_name("vibeface_secure_bg.wasm").read_bytes().startswith(b"\x00asm"),
            "NET-1 uses the real browser cryptography WASM")
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    helper.chmod(0o600)
    report = {"status": "running", "mode": "network-reliability-m0",
              "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repository, text=True).strip(),
              "binary_sha256": file_sha256(binary), "artifacts": str(run),
              "command": [sys.executable, *sys.argv], "provider_calls": 0, "cases": {},
              "not_checked": ["mixed resolver replies and full resolver cache", "slow consumers and source quotas",
                              "attachment crash recovery and 64 MiB completion", "rendered browser UI", "production networks"]}
    nodes, capture = [], None
    try:
        for name in ("R", "A", "B"):
            nodes.append(Node(run, name, binary, workspace, fixture, helper, module))
        relay, source, receiver = nodes
        relay.configure(roles=["relay"], network={"bootstrap_urls": [], "advertise_urls": [],
                                               "mdns": False, "relay_enabled": True})
        capture = RelayCapture(relay.port, retain_payloads=False)
        relay.configure(allowed_origins=[capture.url])
        relay.start()
        probe = relay.client
        directory = probe.command("network_probe", kind="directory", url=relay.url)
        require(all(directory.values()), "short directory lease expires without losing the healthy record")
        report["cases"]["directory_expiry"] = {"status": "observed", **directory,
                                                 "full_resolver_cache": "not_exercised"}
        print("OBSERVED NET-1 signed directory expiry", flush=True)
        reservations = probe.command("network_probe", kind="reservations", url=relay.url)
        require(reservations["accepted"] == 64 and reservations["rejected"] == 1,
                "isolated baseline admits exactly 64 unique reservations and rejects the 65th")
        require(reservations["timeouts"] == 0 and reservations["first_reservation_published_after_capacity"],
                "reservation capacity has no probe timeouts and retains the first control connection")
        require(not reservations["duplicate_identity_accepted"], "baseline rejects a duplicate identity reservation")
        report["cases"]["reservation_capacity"] = {"status": "observed", **reservations}
        print("OBSERVED NET-1 reservation capacity", flush=True)
        relay.stop()
        relay.start()
        for node in (source, receiver):
            node.configure(manager=None, network={"bootstrap_urls": [capture.url.replace("http:", "ws:") + "/api/network/ws"],
                                                  "advertise_urls": [], "mdns": False, "relay_enabled": False})
        source.start()
        network = source.bootstrap()
        wait_for("source reservation through the sole relay", lambda: b'{"type":"reserved"}' in capture.payloads)
        receiver.pair(source, network, relay_url=capture.url)
        receiver.start()
        receiver.client.connect(receiver.node_id)
        wait_for("receiver client authorization", lambda: receiver.client.authenticate())
        conversation = "net1-baseline-history"
        source.client.rpc("create_conversation", conversation_id=conversation, title="NET-1 baseline")
        baseline = wait_for("receiver has the complete signed history",
                            lambda: (value if len(value["events"]) == 1 else None)
                            if (value := receiver.client.rpc("export", conversation_id=conversation)) else None)
        database = receiver.directory / "node.sqlite3"
        with sqlite3.connect(database, timeout=10) as connection:
            connection.execute("CREATE TABLE net1_projection_audit(table_name TEXT, operation TEXT)")
            for table in ("conversations", "messages", "tasks", "events"):
                for operation in ("INSERT", "UPDATE", "DELETE"):
                    connection.execute(f"CREATE TRIGGER net1_{table}_{operation.lower()} AFTER {operation} ON {table} "
                                       f"BEGIN INSERT INTO net1_projection_audit VALUES('{table}','{operation}'); END")
        source.client.rpc("create_task", conversation_id=conversation, task_id="net1-baseline-task",
                          title="NET-1 baseline", goal="Preserve signed history", acceptance="Verified fixture")
        baseline = wait_for("audit positive control imports a real new event",
                            lambda: (value if len(value["events"]) == 2 else None)
                            if (value := receiver.client.rpc("export", conversation_id=conversation)) else None)
        require(any(item["node_id"] == source.node_id and item["online"]
                    for item in receiver.client.rpc("nodes")["nodes"]), "receiver has a real authenticated source peer")
        with sqlite3.connect(database, timeout=10) as connection:
            positive_writes = connection.execute("SELECT COUNT(*) FROM net1_projection_audit WHERE table_name!='events'").fetchone()[0]
            require(positive_writes > 0, "database audit observes actual projection writes before the no-op check")
            connection.execute("DELETE FROM net1_projection_audit")
        started = time.monotonic()
        time.sleep(7)
        after = receiver.client.rpc("export", conversation_id=conversation)
        require(after == baseline, "unchanged sync preserves the exact complete signed snapshot")
        with sqlite3.connect(database, timeout=10) as connection:
            writes = [{"table": table, "operation": operation, "rows": count}
                      for table, operation, count in connection.execute(
                          "SELECT table_name,operation,COUNT(*) FROM net1_projection_audit GROUP BY table_name,operation ORDER BY table_name,operation")]
        business_writes = sum(value["rows"] for value in writes if value["table"] != "events")
        rebuilds = sum(value["rows"] for value in writes if value["table"] == "conversations" and value["operation"] == "DELETE")
        report["cases"]["noop_sync"] = {"status": "observed", "history_events": 2,
                                           "observation_seconds": time.monotonic() - started,
                                           "business_projection_writes": business_writes,
                                           "projection_rebuilds": rebuilds,
                                           "positive_control_business_writes": positive_writes,
                                           "rebuild_counter": "one conversation DELETE row per rebuild in this fixture",
                                           "writes": writes, "snapshot_identical": True}
        print(f"OBSERVED NET-1 no-op sync business writes={business_writes}", flush=True)
        receiver.stop()
        source.stop()
        def seed_attachment(size):
            payload = b"n" * size
            blob = {"hash": hashlib.sha256(payload).hexdigest(), "size": len(payload)}
            with sqlite3.connect(source.directory / "node.sqlite3", timeout=10) as connection:
                previous, sequence = connection.execute(
                    "SELECT hash,sequence FROM events WHERE network_id=? AND scope_id=? AND author=? ORDER BY sequence DESC LIMIT 1",
                    (network, conversation, source.node_id)).fetchone()
                event = {"version": 1, "network_id": network, "scope_id": conversation, "author": source.node_id,
                         "sequence": sequence + 1, "previous": previous, "parents": [previous],
                         "created_at_ms": int(time.time() * 1000),
                         "body": {"type": "message_posted", "message_id": f"net1-attachment-{size}",
                                  "role": "assistant", "text": "NET-1 signed attachment fixture"}, "blobs": [blob]}
                signed = subprocess.run(["node", str(repository / "scripts/crypto-client.mjs"), "--module", str(module)],
                                        input=json.dumps({"op": "fixture_event", "identity_file": str(source.directory / "identity.json"),
                                                          "event": event}) + "\n", capture_output=True, text=True, timeout=15)
                require(signed.returncode == 0, "temporary source identity signs the fixture without exposing its key")
                result = json.loads(signed.stdout)
                require(result.get("ok"), "fixture event signing succeeds")
                envelope = result["result"]
                connection.execute("INSERT INTO blobs(network_id,hash,size,content) VALUES(?,?,?,?)",
                                   (network, blob["hash"], blob["size"], payload))
                connection.execute("INSERT INTO events(network_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,'ready')",
                                   (network, envelope["hash"], conversation, source.node_id, sequence + 1,
                                    json.dumps(envelope, separators=(",", ":"))))
            del payload
            return blob, envelope

        def blob_complete(reference):
            with sqlite3.connect(database, timeout=10) as connection:
                return connection.execute("SELECT EXISTS(SELECT 1 FROM blobs WHERE network_id=? AND hash=?)",
                                          (network, reference["hash"])).fetchone()[0] == 1

        small, _ = seed_attachment(4096)
        source.start()
        receiver.start()
        wait_for("small signed attachment validates the transfer fixture", lambda: blob_complete(small), timeout=25)
        require(verify_blobs(receiver, network) == 1, "small attachment fixture passes real peer signature, size and hash checks")
        receiver.stop()
        source.stop()
        blob, envelope = seed_attachment(32 * 1024 * 1024)
        source.start()
        receiver.start()
        receiver.client.connect(receiver.node_id)
        wait_for("restarted receiver client authorization", lambda: receiver.client.authenticate())
        wait_for("restarted attachment peer is authenticated online", lambda: any(
            item["node_id"] == source.node_id and item["online"]
            for item in receiver.client.rpc("nodes")["nodes"]))
        started = time.monotonic()
        starting_bytes, starting_frames = capture.payload_bytes, capture.frame_count
        starting_binary_bytes = capture.binary_payload_bytes
        starting_binary_frames = capture.binary_frame_count
        observations, complete = [], False
        while time.monotonic() - started < 90:
            with sqlite3.connect(database, timeout=10) as connection:
                complete = connection.execute("SELECT EXISTS(SELECT 1 FROM blobs WHERE network_id=? AND hash=?)",
                                              (network, blob["hash"])).fetchone()[0] == 1
                pending = connection.execute("SELECT COUNT(*) FROM events WHERE network_id=? AND hash=? AND status='pending'",
                                             (network, envelope["hash"])).fetchone()[0]
            observation = {"elapsed_seconds": time.monotonic() - started,
                           "wire_payload_bytes": capture.payload_bytes - starting_bytes,
                           "complete_blob": complete, "pending_envelopes": pending}
            observations.append(observation)
            if complete or observation["wire_payload_bytes"] > 130 * 1024 * 1024:
                break
            time.sleep(1)
        private_json(run / "blob-progress.json", observations)
        if complete:
            require(verify_blobs(receiver, network) >= 1, "complete receiver blob passes final size and hash")
        report["cases"]["single_relay_attachment"] = {"status": "observed", "blob": blob,
            "small_fixture_verified": small, "complete": complete, "verified_complete_bytes": blob["size"] if complete else 0,
            "wire_payload_bytes": capture.payload_bytes - starting_bytes,
            "wire_frames": capture.frame_count - starting_frames,
            "wire_counter": "all captured directions and both relay legs, including control and protocol overhead",
            "binary_payload_bytes": capture.binary_payload_bytes - starting_binary_bytes,
            "binary_frames": capture.binary_frame_count - starting_binary_frames,
            "peer_online_after_restart": True,
            "observation_seconds": time.monotonic() - started,
            "termination": "completed" if complete else "wire observation bound" if observations[-1]["wire_payload_bytes"] > 130 * 1024 * 1024 else "observation deadline",
            "received_body_offset": None, "offset_note": "encrypted relay capture cannot expose uncommitted receiver body offsets",
            "direct_urls": [], "relay_count": 1, "capture_errors": capture.errors,
            "progress_artifact": str(run / "blob-progress.json")}
        require(not capture.errors, "relay capture parses all observed frames")
        require(capture.binary_payload_bytes - starting_binary_bytes > 65536,
                "large attachment observation includes actual binary traffic after the authenticated peer reconnects")
        require(not invocations(run), "NET-1 synchronization fixtures never invoke an executor")
        verify_no_model_budget(nodes)
        require(file_sha256(binary) == report["binary_sha256"], "baseline binary remains unchanged")
        report["status"] = "observed"
        report["fixture_invocations"] = 0
        print(f"OBSERVED NET-1 baseline report: {run / 'report.json'}", flush=True)
    except Exception as error:
        report["status"] = "failed"
        report["failure"] = f"{type(error).__name__}: {error}"
        report["traceback"] = traceback.format_exc()
        raise
    finally:
        for node in reversed(nodes):
            node.stop()
            node.client.close()
        if capture:
            capture.close()
        private_json(run / "report.json", report)
        if options.evidence_dir:
            options.evidence_dir.mkdir(parents=True, exist_ok=True)
            private_json(options.evidence_dir / "m0-baseline.json", report)


def shared_history(client, conversation, task, dispatched=None):
    actual = None
    if dispatched:
        status = client.rpc("task_control", conversation_id=conversation, task_id=task, action="status",
                            target_node=dispatched["target_node"], session_id=dispatched["session_id"])
        actual = status["operation"]["request"]
        require(all(actual.get(key) == value for key, value in dispatched.items() if key != "resources"),
                "read-only task status returns the exact original dispatch body")
        proof = actual.get("binding") or {}
        binding = proof.get("binding", {})
        require(bool(proof.get("signature")) and all(binding.get(key) == dispatched[key] for key in
                    ("network_id", "conversation_id", "task_id", "target_node", "session_id")),
                "read-only task status stays on the original task binding")
    result = client.rpc("conversation", conversation_id=conversation)
    if dispatched:
        record = next((item for item in result["operations"] if item["request"]["operation_id"] == dispatched["operation_id"]), None)
        if not record or not record["request"].get("binding"):
            return None
        require(record["request"] == actual, "snapshot retains the original dispatch returned by explicit read-only status")
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
        parser.add_argument("--evidence-dir", type=Path, help="save a compact NET-1 baseline report in the task evidence directory")
        modes = parser.add_mutually_exclusive_group()
        modes.add_argument("--phase1", action="store_true", help="run focused durable Manager queue/follow-up process checks with controlled fixtures")
        modes.add_argument("--chat-agent", action="store_true", help="run taskless tools, command isolation and startup subscription process checks")
        modes.add_argument("--network-reliability", action="store_true", help="record NET-1 isolated process baselines without provider calls")
        arguments = parser.parse_args()
        try:
            if arguments.network_reliability:
                network_reliability_acceptance(arguments)
            elif arguments.chat_agent:
                chat_agent_acceptance(arguments)
            elif arguments.phase1:
                phase1_acceptance(arguments)
            else:
                acceptance(arguments)
        except Exception as error:
            print(f"FAIL {type(error).__name__}: {error}", file=sys.stderr)
            sys.exit(1)
