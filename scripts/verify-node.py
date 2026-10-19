#!/usr/bin/env python3
"""Run local Node process acceptance without making paid provider calls.

The default run uses the installed Codex app-server only for native thread
creation and reads. Host work, restart and notification checks use a
controlled SDK sidecar, never a model. All state and logs stay beneath
--artifacts with private permissions. The script prints no credentials.
--phase1 checks durable Manager queues, scoped tools and cross-entry progress
using a controlled sidecar. It does not verify real provider model behavior.
--chat-agent checks Agent tasks, host commands and native event recovery
with real Node processes and controlled provider protocol fixtures.
--network-reliability records the NET-1 process baseline without Codex or models.
--web-only checks packaged Web resources through one isolated Node process.
Add --net1-lane 1 to check completed no-op syncs against the saved M0 baseline.
Add --net1-lane 2 to check interrupted and crashed 32 MiB and 64 MiB relay downloads.
Add --net1-lane 3 to verify a 100001-event closure across three authorized replicas.
Add --net1-lane 4 to check production discovery through isolated directory sources.
Add --net1-lane 5 or 6 to check reservation generations and real-source quotas.
Add --net1-lane 7 to check actual mDNS sources and intermediate-route recovery.
--auto-route checks target-directed clients through isolated public entries.
"""

import argparse
import base64
import copy
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
import hashlib
import http.client
import ipaddress
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
import stat
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
                 lambda: stream.connect(f"/tmp/vibeface-{node.peer_id[:32]}/maintenance.sock") is None,
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
    require(set(status) == {"status", "peer_id", "data_dir", "listen", "roles", "version", "cli_version", "error"},
            "CLI status returns only public node health and local paths")
    require(status["status"] == ("running" if running else "stopped") and status["peer_id"] == node.peer_id,
            "CLI status pins the initialized local node in both running and stopped states")
    require(node.access["capability"] not in result.stdout, "CLI status never exposes bootstrap capability")


def verify_maintenance(node):
    stream, value = maintenance(node, "acquire")
    try:
        require(value["status"] == "ready", "idle node enters maintenance")
        require(value["health"]["peer_id"] == node.peer_id, "lease pins the running node")
        node.client.rpc("nodes", status=503)
        probe, health = maintenance(node, "probe")
        probe.close()
        require(health["status"] == "healthy", "health is readable while writes are closed")
        require(node.client.request("/api/info")["peer_id"] == node.peer_id, "HTTP reads survive maintenance")
    finally:
        stream.close()
    wait_for("lease disconnect reopens node", lambda: node.client.rpc("nodes"))
    node.stop()
    marker = node.directory / "update-pending.json"
    private_json(marker, {"protocol": 1})
    node.start()
    opened = node.client.command("connect", url=node.url, peer_id=node.peer_id, check=False)
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
    node_runtime = shutil.which("node")
    if node_runtime:
        (provider_free / "node").symlink_to(Path(node_runtime).resolve(strict=True))
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
                    require(json.loads((directory / "data-epoch.json").read_text()) == {"data_epoch": 5},
                            "direct startup writes the current data epoch")
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
                    require(status.returncode == 0 and json.loads(status.stdout)["peer_id"] == info["peer_id"],
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
    def __init__(self, upstream_port, *, retain_payloads=True, listen_host="127.0.0.1", upstream_host="127.0.0.1"):
        self.port = available_port()
        require(ipaddress.ip_address(listen_host).version == ipaddress.ip_address(upstream_host).version == 4,
                "capture fixture endpoints use explicit IPv4 addresses")
        self.url = f"http://{listen_host}:{self.port}"
        self.upstream_port = upstream_port
        self.upstream_host = upstream_host
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind((listen_host, self.port))
        self.listener.listen(32)
        self.listener.settimeout(0.2)
        self.stopped = threading.Event()
        self.paused = threading.Event()
        self.reply_gate = threading.Event()
        self.reply_gate.set()
        self.reply_connections = None
        self.lock = threading.Lock()
        self.generation = 0
        self.next_connection = 0
        self.connections = []
        self.workers = []
        self.controls = []
        self.traffic = {}
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
            client = server = None
            try:
                generation = self.generation
                client, _ = self.listener.accept()
                if self.paused.is_set():
                    client.close()
                    continue
                server = socket.create_connection((self.upstream_host, self.upstream_port), timeout=5)
                client.settimeout(None)
                server.settimeout(None)
            except socket.timeout:
                if client is not None:
                    client.close()
                continue
            except OSError:
                for stream in (client, server):
                    if stream is not None:
                        stream.close()
                if not self.stopped.is_set() and not self.paused.is_set():
                    self.errors.append("relay proxy failed to accept")
                if self.stopped.is_set():
                    return
                continue
            with self.lock:
                if self.stopped.is_set() or self.paused.is_set() or generation != self.generation:
                    client.close()
                    server.close()
                    continue
                self.connections.extend([client, server])
                self.next_connection += 1
                connection = self.next_connection
                self.traffic[connection] = {"generation": generation, "binary_payload_bytes": 0, "binary_frames": 0, "ping_frames": 0, "pong_frames": 0}
                for source, target, direction in ((client, server, "to_relay"), (server, client, "from_relay")):
                    worker = threading.Thread(target=self.forward, args=(source, target, connection, generation, direction), daemon=True)
                    self.workers.append(worker)
                    worker.start()

    def forward(self, source, target, connection, generation, direction):
        buffer = bytearray()
        headers = True
        try:
            while not self.stopped.is_set():
                part = source.recv(65536)
                if not part:
                    return
                received_wall_time, received_monotonic = time.time(), time.monotonic()
                while (direction == "from_relay" and (self.reply_connections is None or connection in self.reply_connections)
                       and not self.reply_gate.wait(.1)):
                    if self.stopped.is_set() or self.paused.is_set():
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
                        with self.lock:
                            self.traffic[connection]["binary_payload_bytes"] += len(payload)
                            self.traffic[connection]["binary_frames"] += 1
                    elif buffer[0] & 15 in (9, 10):
                        with self.lock:
                            self.traffic[connection]["ping_frames" if buffer[0] & 15 == 9 else "pong_frames"] += 1
                    if self.retain_payloads or buffer[0] & 15 == 1:
                        self.payloads.append(payload)
                    if buffer[0] & 15 == 1:
                        control = json.loads(payload)
                        with self.lock:
                            require(len(self.controls) < 4096, "bounded captured relay controls")
                            self.controls.append({"connection": connection, "generation": generation,
                                                  "direction": direction, "monotonic": received_monotonic, "wall_time": received_wall_time,
                                                  "peer_id": control.get("peer_id") or (control.get("record") or {}).get("peer_id"),
                                                  **{key: control[key] for key in ("type", "target", "service") if key in control}})
                    del buffer[:offset + length]
        except (OSError, ValueError, AssertionError) as error:
            if isinstance(error, (ValueError, AssertionError)):
                self.errors.append(str(error))
        finally:
            for stream in (source, target):
                try:
                    stream.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def pause(self):
        with self.lock:
            self.paused.set()
            self.generation += 1
            connections, self.connections = self.connections, []
            workers, self.workers = self.workers, []
            boundary = {"monotonic": time.monotonic(), "wall_time": time.time(), "generation": self.generation,
                        "control_index": len(self.controls), "closed_streams": len(connections)}
        for stream in connections:
            try:
                stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            stream.close()
        for worker in workers:
            worker.join(timeout=2)
        require(all(not worker.is_alive() for worker in workers), "all cut relay forwarding workers exit")
        return boundary

    def resume(self):
        with self.lock:
            boundary = {"monotonic": time.monotonic(), "wall_time": time.time(), "generation": self.generation,
                        "control_index": len(self.controls)}
            self.paused.clear()
            return boundary

    def close(self):
        self.stopped.set()
        self.listener.close()
        self.pause()
        self.thread.join(timeout=6)
        require(not self.thread.is_alive(), "relay accept worker exits")


CLIENT_SOURCE = r"""
import init, { Identity, Handshake, recoverySeal, recoveryOpen, verifyMembership, verify } from "MODULE_URL";
import { lstat, readFile, writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { createHash } from 'node:crypto';
await init({ module_or_path: await readFile(new URL('./vibeface_secure_bg.wasm', "MODULE_URL")) });
const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', { fatal: true });
let clientIdentity;
if (process.argv[2]) {
  try {
    const stat = await lstat(process.argv[2]);
    if (!stat.isFile() || stat.isSymbolicLink() || (stat.mode & 0o077)) throw new Error('client identity file must be private');
    clientIdentity = Identity.fromSecretHex(JSON.parse(await readFile(process.argv[2], 'utf8')).secret_key_hex);
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    clientIdentity = Identity.generate();
    await writeFile(process.argv[2], JSON.stringify({secret_key_hex:clientIdentity.secretHex()})+'\n', {mode:0o600,flag:'wx'});
  }
} else clientIdentity = Identity.generate();
const keys = new Map([['client', clientIdentity], ['root', Identity.generate()]]);
const connections = new Map();
const routeStats = new Map();
const identity = name => { const key = keys.get(name); if (!key) throw new Error('unknown local identity'); return key; };
const exposed = key => ({ public_key: key.publicKeyHex(), subject_id: key.peerId() });
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
    const proof = JSON.stringify({nonce:connection.nonce,peer_id:owner.peerId()});
    connection.socket.send(JSON.stringify({type:'reserve',peer_id:owner.peerId(),public_key:owner.publicKeyHex(),signature:owner.sign('relay-reserve',encoder.encode(proof))}));
    const response = await connection.next();
    return { connection, accepted:response?.type === 'reserved',outcome:response?.type || (connection.closed ? 'closed' : 'timeout') };
  }
  async function exchange(message) {
    const connection = await control(); connection.socket.send(JSON.stringify(message));
    return await connection.next();
  }
  try {
    if (input.kind === 'peer_id_contract') {
      const owner = key(), legacy = await control();
      const proof = JSON.stringify({nonce:legacy.nonce,node_id:owner.peerId()});
      legacy.socket.send(JSON.stringify({type:'reserve',node_id:owner.peerId(),public_key:owner.publicKeyHex(),signature:owner.sign('relay-reserve',encoder.encode(proof))}));
      const rejected = await legacy.next();
      if (rejected !== null || !legacy.closed) throw new Error('removed node_id field was not rejected');
      const valid = await reserve(owner);
      if (!valid.accepted) throw new Error('current peer_id reservation was rejected');
      return {old_node_id_rejected:true,peer_id_accepted:true};
    }
    if (input.kind === 'record') {
      const reply = await exchange({type:'lookup',target:input.target});
      const record = reply?.record;
      if (reply?.type !== 'record' || !record || record.peer_id !== input.target) throw new Error('requested signed directory record is unavailable');
      if (createHash('sha256').update(Buffer.from(record.public_key,'hex')).digest('hex') !== record.peer_id) throw new Error('directory identity mismatch');
      const now = Math.floor(Date.now()/1000);
      if (!Number.isSafeInteger(record.issued_at) || !Number.isSafeInteger(record.expires_at) || record.issued_at > now+30 || record.expires_at <= now || record.expires_at <= record.issued_at || record.expires_at > record.issued_at+180) throw new Error('directory lifetime invalid');
      if (![record.direct_urls,record.relay_urls].every(urls => Array.isArray(urls) && urls.length <= 8 && urls.every(url => typeof url === 'string'))) throw new Error('directory URLs invalid');
      verify(record.public_key,'reachability',encoder.encode(JSON.stringify({...record,signature:''})),record.signature);
      return record;
    }
    if (input.kind === 'directory') {
      const now = Math.floor(Date.now() / 1000), short = key(), healthy = key();
      async function publish(owner, expires) {
        const record = {peer_id:owner.peerId(),public_key:owner.publicKeyHex(),direct_urls:[],relay_urls:[],issued_at:now,expires_at:expires,signature:''};
        record.signature = owner.sign('reachability',encoder.encode(JSON.stringify(record)));
        if ((await exchange({type:'publish',record}))?.type !== 'published') throw new Error('signed fixture record was rejected');
      }
      await publish(short, now + 2); await publish(healthy, now + 120);
      const before = await exchange({type:'lookup',target:short.peerId()});
      await new Promise(resolve => setTimeout(resolve, Math.max(0, (now + 2) * 1000 - Date.now()) + 100));
      const expired = await exchange({type:'lookup',target:short.peerId()});
      const good = await exchange({type:'lookup',target:healthy.peerId()});
      return {short_record_present_before_expiry:before?.record?.peer_id === short.peerId(),expired_record_removed:expired?.type === 'record' && expired.record === null,healthy_record_retained:good?.record?.peer_id === healthy.peerId()};
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
      const record = {peer_id:owner.peerId(),public_key:owner.publicKeyHex(),direct_urls:[],relay_urls:[],issued_at:now,expires_at:now+120,signature:''};
      record.signature = owner.sign('reachability',encoder.encode(JSON.stringify(record)));
      first.connection.socket.send(JSON.stringify({type:'publish',record}));
      await new Promise(resolve => setTimeout(resolve, 100));
      const lookup = await exchange({type:'lookup',target:owner.peerId()});
      return {unique_identities:65,reserve_requests:66,accepted,rejected,timeouts,duplicate_identity_outcome:duplicate.outcome,duplicate_identity_accepted:duplicate.accepted,first_reservation_still_open:!first.connection.closed,first_reservation_published_after_capacity:lookup?.record?.peer_id === owner.peerId(),directory_query_after_capacity:lookup?.type === 'record'};
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
    socket.send(JSON.stringify(input.relay ? {type:'dial', target:input.peer_id, service:'client'} : {type:'direct', service:'client'}));
    await control('ready');
    const handshake = new Handshake(true, key.secretHex(), input.peer_id, encoder.encode('vibeface:client:v1'));
    socket.send(handshake.write(new Uint8Array()));
    const response = await receive();
    if (!(response instanceof ArrayBuffer)) throw new Error('Noise response must be binary');
    handshake.read(new Uint8Array(response));
    socket.send(handshake.write(new Uint8Array()));
    if (!handshake.isFinished() || handshake.remoteId() !== input.peer_id) throw new Error('target identity mismatch');
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
async function automaticConnect(input) {
  const stats = {controls:[],opened:0,closed:0,active_data:0,max_active_data:0,authenticated_sessions:0,calls:[]};
  routeStats.set(input.connection || 'main',stats);
  class ObservedSocket extends WebSocket {
    constructor(url) {
      super(url);
      stats.opened++;
      this.dataPath = false;
      this.addEventListener('close',() => { stats.closed++; if (this.dataPath) stats.active_data--; });
    }
    send(value) {
      if (typeof value === 'string') {
        const control = JSON.parse(value);
        stats.controls.push({url:this.url,type:control.type,target:control.target});
        if (['dial','direct'].includes(control.type) && !this.dataPath) {
          this.dataPath = true;
          stats.active_data++; stats.max_active_data = Math.max(stats.max_active_data,stats.active_data);
        }
      }
      super.send(value);
    }
  }
  const source = (await readFile(input.transport,'utf8')).replace(/^import .*;\n/,'').replace(/^export /gm,'');
  const transport = new Function('init','Identity','Handshake','verify','WebSocket','location',
    source + '\nreturn {connectDevice, setIdentity(value) {identity=value;}};')(
      init,Identity,Handshake,verify,ObservedSocket,{protocol:'http:'});
  transport.setIdentity(identity(input.identity || 'client'));
  const events = [], controller = new AbortController();
  const timer = input.cancel_after_ms === undefined ? undefined : setTimeout(() => controller.abort(),input.cancel_after_ms);
  let selected;
  try {
    selected = await transport.connectDevice({address:input.url,peer_id:input.peer_id},
      {signal:controller.signal,onEvent:event => events.push(event)});
    stats.authenticated_sessions++;
  } finally { clearTimeout(timer); }
  return {events,frames:[],close:() => selected.close(),async request(method,params) {
    stats.calls.push({method,rpc_method:params?.method,operation_id:params?.request?.operation_id});
    try { return {result:await selected.request(method,params)}; }
    catch (error) { return {error:String(error)}; }
  }};
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
      const payload = encoder.encode(JSON.stringify({version:1,network_id:root.peerId(),root_secret_key:root.secretHex(),genesis:input.state.genesis,recovery_epoch:epoch,decryption_keys:[]}));
      try { result = {version:1,network_id:root.peerId(),recovery_epoch:epoch,envelope:JSON.parse(decoder.decode(recoverySeal(payload,input.password)))}; }
      finally { payload.fill(0); }
    }
    else if (input.op === 'recover_material') {
      const plaintext = recoveryOpen(encoder.encode(JSON.stringify(input.backup.envelope)),input.password);
      try {
        const payload = JSON.parse(decoder.decode(plaintext));
        const root = Identity.fromSecretHex(payload.root_secret_key);
        keys.get('recovered')?.free(); keys.set('recovered',root);
        result = {network_id:root.peerId(),recovery_epoch:payload.recovery_epoch,genesis:payload.genesis};
      } finally { plaintext.fill(0); }
    }
    else if (input.op === 'recovery_checkpoint') {
      const root = identity('recovered');
      const verified = verifyMembership(JSON.stringify(input.state), root.peerId(), root.publicKeyHex());
      try { result = JSON.parse(verified.createRecoveryCheckpoint(root, JSON.stringify(input.members), input.backup_epoch, JSON.stringify(input.prepared))); }
      finally { verified.free(); }
    }
    else if (input.op === 'forget_root') { identity('root').free(); keys.delete('root'); result = {forgotten:true}; }
    else if (input.op === 'connect' || input.op === 'connect_auto') { const old = connections.get(input.connection || 'main'); old?.close(); const value = await (input.op === 'connect_auto' ? automaticConnect(input) : connect(input)); connections.set(input.connection || 'main', value); result = {connected:true}; }
    else if (input.op === 'route_stats') result = routeStats.get(input.connection || 'main');
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
    def __init__(self, url, helper, module, *, identity_file=None):
        self.url = url
        self.helper = helper
        self.module = module
        self.identity_file = identity_file
        self.network_id = None
        self.principal = None
        self.process = None
        self.peer_id = None

    def command(self, op, *, check=True, _wait_timeout=30, **arguments):
        require(0 < _wait_timeout <= 30, "client command wait stays within its original deadline")
        if self.process is None:
            self.process = subprocess.Popen(
                ["node", str(self.helper), *([str(self.identity_file)] if self.identity_file else [])], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.process.stdin.write(json.dumps({"op": op, **arguments}) + "\n")
        self.process.stdin.flush()
        require(select.select([self.process.stdout], [], [], _wait_timeout)[0], "WASM client command completed before timeout")
        line = self.process.stdout.readline()
        require(line, "WASM client returned a bounded JSON response")
        value = json.loads(line)
        if not check:
            return value
        require(value.get("ok"), f"client {op} failed: {value.get('error', 'unknown error')}")
        return value.get("result")

    def connect(self, peer_id=None, *, relay=False, connection="main"):
        self.peer_id = peer_id or self.peer_id
        return self.command("connect", url=self.url, peer_id=self.peer_id,
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
        self.connect(owner.peer_id)
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
        self.call("prepare_revoke", {"subject_id": owner.peer_id}, status=403)

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
    def __init__(self, run, name, binary, workspace, fixture, helper, module, *, relay_only=False):
        self.name = name
        self.binary = binary
        self.directory = run / name
        self.port = available_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.client = Client(self.url, helper, module, identity_file=self.directory / "client-identity.json")
        self.process = None
        self.log = None
        try:
            result = subprocess.run(
                [str(binary), "init", "--data-dir", str(self.directory),
                 "--listen", f"127.0.0.1:{self.port}",
                 *(["--relay-only"] if relay_only else ["--root", str(workspace)])],
                capture_output=True, timeout=45)
        except subprocess.TimeoutExpired:
            raise RuntimeError(f"{name}: CLI initialization timed out") from None
        require(result.returncode == 0, f"{name}: CLI initialization succeeds")
        require(json.loads((self.directory / "data-epoch.json").read_text()) == {"data_epoch": 5},
                "new private Node data uses the current data epoch")
        self.configure(timeout_secs=60)
        result = subprocess.run([str(binary), "access", "--json", "--data-dir", str(self.directory)],
                                capture_output=True, timeout=15)
        require(result.returncode == 0, f"{name}: local access material is available")
        self.access = json.loads(result.stdout)
        require(self.access["peer_id"], "local access pins the Node identity")
        self.peer_id = self.access["peer_id"]

    def configure(self, **values):
        path = self.directory / "config.json"
        configuration = json.loads(path.read_text())
        configuration.update(values)
        private_json(path, configuration)

    def set_url(self, host, *, listen=None):
        require(self.process is None, f"{self.name}: listener changes require a stopped process")
        address = ipaddress.ip_address(host)
        authority = f"[{address}]" if address.version == 6 else str(address)
        self.url = self.client.url = f"http://{authority}:{self.port}"
        self.configure(listen=listen or f"{authority}:{self.port}", public_url=self.url,
                       allow_insecure_lan=not address.is_loopback)

    def start(self, *, env=None):
        require(self.process is None, f"{self.name}: process is stopped before start")
        self.log = (self.directory / "process.log").open("ab")
        self.process = subprocess.Popen(
            [str(self.binary), "serve", "--data-dir", str(self.directory)],
            stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
            start_new_session=True, env=env)
        info = wait_for(f"{self.name} HTTP listener", lambda: self.client.request("/api/info"))
        self.peer_id = info["peer_id"]
        require(self.process.poll() is None, f"{self.name}: independent process remains running")

    def stop(self, *, crash=False):
        if self.process is not None:
            # Stop only this script's process group, including its private app-server.
            try:
                if self.process.poll() is None:
                    os.killpg(self.process.pid, signal.SIGKILL if crash else signal.SIGTERM)
                self.process.wait(timeout=5)
            except ProcessLookupError:
                pass
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=5)
            returncode = self.process.returncode
            self.process = None
            self.log.close()
            self.log = None
            return returncode

    def pair(self, owner, network, *, entry_url=None):
        original_config = json.loads((self.directory / "config.json").read_text())
        network_config = original_config["network"]
        invitation = owner.client.invite("node", ["gateway", "relay", "replica", "scheduler"])
        path = self.directory / "pair-invitation.json"
        private_json(path, {"url": entry_url or owner.url, "peer_id": owner.peer_id,
                            "invite": invitation})
        log_path = self.directory / f"pair-cli-{uuid.uuid4().hex[:8]}.log"
        log = os.fdopen(os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb")
        process = None
        try:
            process = subprocess.Popen([str(self.binary), "pair", "--data-dir", str(self.directory),
                                        "--invite-file", str(path), "--name", f"Node {self.name}"],
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
            def pending():
                if process.poll() is not None:
                    raise RuntimeError(f"{self.name}: CLI pairing exited {process.returncode} before signed review; see {log_path}")
                requests = owner.client.call("pair_requests")
                return next((item for item in requests if item["review"]["new_member"]["subject_id"] == self.peer_id
                             and item.get("new_subject_signature")), None)
            review = wait_for(f"{self.name} signed CLI pairing review", pending)
            owner.client.approve(review["request_id"])
            process.wait(timeout=40)
            require(process.returncode == 0, f"{self.name}: pinned CLI pairing completes after management approval")
        finally:
            try:
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
            finally:
                log.close()
        self.client.pair_client(owner.client, network, f"{self.name} independent client")
        self.client.url = self.url
        self.client.peer_id = self.peer_id
        paired_config = json.loads((self.directory / "config.json").read_text())
        require(paired_config["codex_socket"] == original_config["codex_socket"],
                "pairing preserves managed or explicit Codex socket configuration")
        self.configure(network=network_config)

    def bootstrap(self):
        self.client.connect(self.peer_id)
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


def wait_for_relay_target(node, capture, after_connection, *, timeout=35):
    deadline = time.monotonic() + timeout

    def reserved():
        with capture.lock:
            controls = list(capture.controls)
        attempts = {item["connection"] for item in controls
                    if item["connection"] > after_connection and item["direction"] == "to_relay"
                    and item["type"] == "reserve" and item["peer_id"] == node.peer_id}
        return next((item for item in reversed(controls)
                     if item["connection"] in attempts and item["direction"] == "from_relay"
                     and item["type"] == "reserved"), None)

    candidate = wait_for("current fixed target relay reservation", reserved, timeout=timeout)
    probe = Client(capture.url, node.client.helper, node.client.module)
    started = time.monotonic()
    try:
        probe.command("connect", url=capture.url, peer_id=node.peer_id, relay=True,
                      _wait_timeout=min(30, deadline - time.monotonic()))
        response = probe.command("request", method="info", params={},
                                 _wait_timeout=min(30, deadline - time.monotonic()))
        require(not response.get("error") and response.get("result", {}).get("peer_id") == node.peer_id,
                "current relay reaches the original target through pinned Noise and an encrypted read")
        return {"target": node.peer_id, "relay_url": capture.url,
                "after_connection": after_connection, "reservation_connection": candidate["connection"],
                "capture_generation": candidate["generation"], "read_peer_id": response["result"]["peer_id"],
                "probe_seconds": time.monotonic() - started, "kind": "pinned Noise and encrypted read-only info"}
    finally:
        if probe.process is not None and deadline - time.monotonic() <= 5:
            probe.process.kill()
        probe.close()


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


def invocations(run, label=None):
    path = run / "fixture-invocations.jsonl"
    records = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
    return [record for record in records if label is None or record["label"] == label]


def fixture_wrapper(run, name, mode):
    path = run / name
    diagnostic = ""
    if mode == "--fixture-manager":
        diagnostic = ("import tempfile\n"
                      "diagnostic_fd, _ = tempfile.mkstemp(prefix='fixture-manager-' + str(os.getpid()) + '-', "
                      f"suffix='.stderr.log', dir={str(run)!r})\n"
                      "os.dup2(diagnostic_fd, 2)\nos.close(diagnostic_fd)\n")
    path.write_text(f"#!{sys.executable}\nimport os\n" + diagnostic + f"os.execv({sys.executable!r}, "
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


def websocket_receive_frame(stream, maximum=1024 * 1024, *, expected_mask=None, deadline=None):
    def exact(length):
        value = b""
        while len(value) < length:
            if deadline is not None:
                remaining = deadline - time.monotonic()
                require(remaining > 0, "fixture frame shares the operation deadline")
                stream.settimeout(remaining)
            block = stream.recv(length - len(value))
            if not block:
                raise EOFError("fixture WebSocket closed")
            value += block
        return value

    first, second = exact(2)
    opcode = first & 15
    require(first & 128 and not first & 0x70 and opcode in (1, 2, 8, 9, 10), "fixture requires unfragmented WS frames without reserved bits")
    if expected_mask is not None:
        require(bool(second & 128) == expected_mask, "fixture checks the WS masking direction")
    length = second & 127
    if length == 126:
        length = struct.unpack("!H", exact(2))[0]
    elif length == 127:
        length = struct.unpack("!Q", exact(8))[0]
    require(length <= maximum, "fixture bounds WebSocket messages")
    require(opcode < 8 or length <= 125, "fixture bounds WS control frames")
    mask = exact(4) if second & 128 else None
    payload = exact(length)
    if mask:
        payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
    if opcode == 8:
        raise EOFError("fixture WebSocket close frame")
    return opcode, payload


def websocket_receive(stream):
    opcode, payload = websocket_receive_frame(stream)
    require(opcode == 1, "fixture receives text RPC frames")
    return json.loads(payload)


def websocket_send_frame(stream, encoded, *, opcode=1, masked=False):
    require(len(encoded) <= 1024 * 1024, "fixture bounds outgoing WebSocket frames")
    mask_flag = 128 if masked else 0
    prefix = (bytes([0x80 | opcode, mask_flag | len(encoded)]) if len(encoded) < 126 else
              bytes([0x80 | opcode, mask_flag | 126]) + struct.pack("!H", len(encoded)) if len(encoded) <= 65535 else
              bytes([0x80 | opcode, mask_flag | 127]) + struct.pack("!Q", len(encoded)))
    if masked:
        mask = os.urandom(4)
        prefix += mask
        encoded = bytes(value ^ mask[index % 4] for index, value in enumerate(encoded))
    stream.sendall(prefix + encoded)


def websocket_send(stream, value):
    websocket_send_frame(stream, json.dumps(value).encode())


def network_mdns_interfaces():
    require(sys.platform == "darwin" and Path("/usr/bin/dns-sd").is_file(),
            "Lane 7 mDNS driver requires the macOS DNS-SD observer")
    result = subprocess.run(["/sbin/ifconfig", "-a"], capture_output=True, text=True, timeout=5)
    require(result.returncode == 0, "read-only interface enumeration succeeds")
    interfaces = []
    for block in re.split(r"(?=^\S+: flags=)", result.stdout, flags=re.MULTILINE):
        name = re.match(r"(en\d+): flags=.*MULTICAST", block)
        if not name or not re.search(r"^\s*status: active\s*$", block, re.MULTILINE):
            continue
        addresses = [str(ipaddress.ip_address(value)) for value in re.findall(r"^\s*inet (\S+)", block, re.MULTILINE)
                     if not ipaddress.ip_address(value).is_loopback and not ipaddress.ip_address(value).is_link_local]
        if addresses:
            interfaces.append({"name": name[1], "ipv4": addresses})
    require(interfaces, "an already-active multicast interface has an IPv4 address without changing host networking")
    return interfaces


def network_dns_name(value):
    unescaped = re.sub(r"\\([0-9]{3}|.)", lambda match: chr(int(match[1])) if match[1].isdigit() else match[1], value)
    return unescaped.rstrip(".").lower() + "."


class NetworkDnsSd:
    def __init__(self, run, name, interface, arguments, *, timeout=180):
        require(1 <= timeout <= 300 and arguments[0] in ("-B", "-R", "-L", "-Q"),
                "bounded DNS-SD fixture operation")
        self.path = run / f"{name}.log"
        self.log = self.path.open("xb")
        self.path.chmod(0o600)
        self.command = ["/usr/bin/dns-sd", "-i", interface, "-t", str(timeout), *map(str, arguments)]
        self.started = time.monotonic()
        self.stopped = None
        self.returncode = None
        try:
            self.process = subprocess.Popen(self.command, stdin=subprocess.DEVNULL, stdout=self.log,
                                            stderr=subprocess.STDOUT, start_new_session=True)
        except BaseException:
            self.log.close()
            raise

    def text(self):
        require(self.path.stat().st_size <= 1024 * 1024, "DNS-SD fixture output stays bounded")
        return self.path.read_text(errors="replace")

    def browse_records(self):
        records = []
        pattern = r"^\s*(\d\d:\d\d:\d\d\.\d+)\s+(Add|Rmv)\s+([0-9A-Fa-f]+)\s+(\d+)\s+(\S+)\s+(\S+)\s+(.+?)\s*$"
        for index, line in enumerate(self.text().splitlines()):
            match = re.match(pattern, line)
            if match:
                stamp, action, flags, interface, domain, service, instance = match.groups()
                records.append({"line": index + 1, "timestamp": stamp, "action": action, "flags": int(flags, 16),
                                "interface_index": int(interface), "domain": domain, "service": service,
                                "instance": instance, "canonical_fullname": network_dns_name(f"{instance}.{service}{domain}")})
        return records

    def resolutions(self):
        records = []
        pattern = r"^\s*(\d\d:\d\d:\d\d\.\d+)\s+(.+?\._vibeface\._tcp\.local\.) can be reached at (\S+):(\d+) \(interface (\d+)\)"
        lines = self.text().splitlines()
        for index, line in enumerate(lines):
            match = re.match(pattern, line)
            if match:
                stamp, fullname, hostname, port, interface = match.groups()
                records.append({"line": index + 1, "timestamp": stamp, "fullname": fullname,
                                "canonical_fullname": network_dns_name(fullname),
                                "hostname": hostname, "port": int(port), "interface_index": int(interface),
                                "txt_lines": lines[index + 1:index + 3]})
        return records

    def addresses(self, hostname):
        records = []
        for index, line in enumerate(self.text().splitlines()):
            columns = line.split()
            if len(columns) < 7 or columns[1] != "Add" or hostname.casefold().rstrip(".") not in [value.casefold().rstrip(".") for value in columns]:
                continue
            try:
                address = ipaddress.ip_address(columns[-1])
            except ValueError:
                continue
            records.append({"line": index + 1, "address": str(address), "family": address.version,
                            "interface_index": int(columns[3]), "raw": line})
        return records

    def snapshot(self):
        return {"command": self.command, "log": str(self.path), "log_sha256": file_sha256(self.path),
                "log_bytes": self.path.stat().st_size, "started_monotonic": self.started,
                "stopped_monotonic": self.stopped, "returncode": self.returncode,
                "process_running": self.process is not None and self.process.poll() is None}

    def close(self):
        if self.process is not None:
            try:
                if self.process.poll() is None:
                    self.process.send_signal(signal.SIGINT)
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
            self.returncode = self.process.returncode
            self.stopped = time.monotonic()
            self.process = None
            self.log.close()
        self.text()


class NetworkDirectoryFixture:
    """Bounded public-Control fixture; never forwards Publish or Reserve upstream."""
    def __init__(self, name, *, record=None, record_factory=None, retain_publications=False):
        self.name, self.record, self.record_factory = name, record, record_factory
        self.publications = {} if retain_publications else None
        self.port = available_port()
        self.url = f"ws://127.0.0.1:{self.port}/api/network/ws"
        self.lock = threading.Lock()
        self.stopped = threading.Event()
        self.reply_gate = threading.Event()
        self.reply_gate.set()
        self.publish_gate = threading.Event()
        self.publish_gate.set()
        self.events, self.errors, self.streams, self.workers = [], [], {}, []
        self.next_connection = 0
        self.rejected_connections = 0
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", self.port))
        self.listener.listen(16)
        self.listener.settimeout(.2)
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def note(self, kind, connection=None, **fields):
        with self.lock:
            require(len(self.events) < 4096, "bounded directory fixture evidence")
            value = {"kind": kind, "connection": connection, "wall_time": time.time(), "monotonic": time.monotonic(), **fields}
            self.events.append(value)
            return value

    def snapshot(self):
        with self.lock:
            return {"name": self.name, "url": self.url, "events": list(self.events), "errors": list(self.errors),
                    "active_sockets": len(self.streams), "accepted_connections": self.next_connection,
                    "fixture_capacity_rejections": self.rejected_connections,
                    "fixture_scope": "original public Control only; Publish ACK is local, Reserve is refused, no upstream forwarding"}

    def published(self, target):
        with self.lock:
            return copy.deepcopy((self.publications or {}).get(target))

    def accept(self):
        while not self.stopped.is_set():
            try:
                stream, peer = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with self.lock:
                if self.stopped.is_set() or len(self.streams) >= 16 or self.next_connection >= 256:
                    if not self.stopped.is_set():
                        self.rejected_connections += 1
                    stream.close()
                    continue
                self.next_connection += 1
                connection = self.next_connection
                self.streams[connection] = stream
                worker = threading.Thread(target=self.serve, args=(stream, peer, connection), daemon=True)
                self.workers.append(worker)
                worker.start()

    def serve(self, stream, peer, connection):
        try:
            stream.settimeout(15)
            self.note("tcp_accepted", connection, peer_ip=peer[0])
            headers = bytearray()
            while b"\r\n\r\n" not in headers:
                require(len(headers) < 16384, "bounded directory HTTP upgrade")
                part = stream.recv(4096)
                if not part:
                    return
                headers.extend(part)
            head, extra = bytes(headers).split(b"\r\n\r\n", 1)
            require(not extra and head.split(b"\r\n", 1)[0] == b"GET /api/network/ws HTTP/1.1", "directory fixture receives the expected WS endpoint")
            values = dict(line.decode("ascii").split(":", 1) for line in head.split(b"\r\n")[1:])
            values = {key.lower(): value.strip() for key, value in values.items()}
            key = values["sec-websocket-key"]
            require(len(base64.b64decode(key, validate=True)) == 16, "valid fixture WS upgrade key")
            accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            stream.sendall(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\n\r\n").encode())
            websocket_send(stream, {"type": "welcome", "nonce": os.urandom(32).hex()})
            while True:
                opcode, payload = websocket_receive_frame(stream, 16 * 1024, expected_mask=True)
                if opcode == 9:
                    websocket_send_frame(stream, payload, opcode=10)
                    continue
                if opcode == 10:
                    continue
                require(opcode == 1, "directory fixture accepts only public Control text")
                control = json.loads(payload)
                break
            kind = control.get("type")
            self.note("control_received", connection, control=kind, target=control.get("target"), peer_id=control.get("peer_id") or control.get("record", {}).get("peer_id"))
            if kind == "publish":
                while not self.publish_gate.wait(.1):
                    if self.stopped.is_set():
                        return
                if self.publications is not None:
                    record = control["record"]
                    target = record["peer_id"]
                    require(re.fullmatch(r"[0-9a-f]{64}", target), "published fixture record names a complete Node identity")
                    with self.lock:
                        require(len(self.publications) < 16 or target in self.publications, "bounded fixture public record capture")
                        self.publications[target] = copy.deepcopy(record)
                websocket_send(stream, {"type": "published"})
                self.note("publish_acknowledged_locally", connection)
            elif kind == "reserve":
                self.note("reserve_refused", connection)
            elif kind == "lookup":
                deadline = time.monotonic() + 35
                while not self.reply_gate.wait(.1):
                    if self.stopped.is_set() or time.monotonic() >= deadline:
                        return
                if self.stopped.is_set():
                    return
                record = self.record_factory(control["target"]) if self.record_factory else copy.deepcopy(self.record)
                while not self.reply_gate.wait(.1):
                    if self.stopped.is_set() or time.monotonic() >= deadline:
                        return
                if self.stopped.is_set():
                    return
                self.note("record_send_started", connection, target=control["target"],
                          record_peer_id=record.get("peer_id") if record else None,
                          issued_at=record.get("issued_at") if record else None, expires_at=record.get("expires_at") if record else None,
                          signature_sha256=hashlib.sha256(record["signature"].encode()).hexdigest() if record else None)
                websocket_send(stream, {"type": "record", "record": record})
                self.note("record_sent", connection, target=control["target"])
            else:
                self.note("control_refused", connection, control=kind)
        except (OSError, EOFError):
            pass
        except Exception as error:
            with self.lock:
                self.errors.append(f"{type(error).__name__}: {error}")
        finally:
            try:
                stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            stream.close()
            with self.lock:
                self.streams.pop(connection, None)

    def release(self):
        value = self.note("reply_gate_released")
        self.reply_gate.set()
        return value

    def close(self):
        self.stopped.set()
        self.reply_gate.set()
        self.publish_gate.set()
        self.listener.close()
        with self.lock:
            streams = list(self.streams.values())
        for stream in streams:
            try:
                stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            stream.close()
        self.thread.join(timeout=2)
        deadline = time.monotonic() + 2
        for worker in self.workers:
            worker.join(timeout=max(0, deadline - time.monotonic()))
        require(not self.thread.is_alive() and all(not worker.is_alive() for worker in self.workers), "directory fixture workers and sockets are reclaimed")


class NetworkUpgradeRejected(AssertionError):
    def __init__(self, summary):
        self.summary = dict(summary)
        super().__init__(f"isolated network upgrade rejected with HTTP {summary['upgrade_status']}")


class NetworkControlSocket:
    """A bounded raw client for original public Controls and actual TCP read stalls."""
    def __init__(self, url, *, source_ip=None, deadline=None, forwarded_for=None, receive_buffer=None):
        endpoint = urllib.parse.urlparse(url)
        require(endpoint.scheme == "ws" and endpoint.path == "/api/network/ws" and not endpoint.query and not endpoint.fragment,
                "network fixture uses the original local WS endpoint")
        require(ipaddress.ip_address(endpoint.hostname).is_loopback, "network fixture connects only to an isolated loopback Node")
        if source_ip is not None:
            require(ipaddress.ip_address(source_ip).is_loopback, "network fixture binds only an existing loopback source")
        self.stream = None
        self.pending = bytearray()
        self.closed = False
        self.deadline = deadline
        self.write_lock = threading.RLock()
        self.pong_lock = threading.Lock()
        self.pending_pong = None
        self.summary = {"url": url, "requested_source_ip": source_ip, "opened_wall_time": time.time()}
        try:
            deadline = min(time.monotonic() + 10, self.deadline or float("inf"))
            if receive_buffer is None:
                self.stream = socket.create_connection((endpoint.hostname, endpoint.port or 80), timeout=10,
                                                       source_address=(source_ip, 0) if source_ip else None)
            else:
                require(isinstance(receive_buffer, int) and 4096 <= receive_buffer <= 65536, "fixture receive buffer is explicitly bounded")
                family = socket.AF_INET6 if ipaddress.ip_address(endpoint.hostname).version == 6 else socket.AF_INET
                self.stream = socket.socket(family)
                self.stream.settimeout(10)
                self.stream.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, receive_buffer)
                if source_ip:
                    self.stream.bind((source_ip, 0))
                self.stream.connect((endpoint.hostname, endpoint.port or 80))
                self.summary.update({"requested_receive_buffer": receive_buffer,
                                     "actual_receive_buffer": self.stream.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF)})
            self.summary.update({"actual_local_address": list(self.stream.getsockname()), "actual_peer_address": list(self.stream.getpeername())})
            nonce = base64.b64encode(os.urandom(16)).decode()
            host = f"[{endpoint.hostname}]" if ":" in endpoint.hostname else endpoint.hostname
            request = (f"GET {endpoint.path} HTTP/1.1\r\nHost: {host}:{endpoint.port or 80}\r\n"
                       f"Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {nonce}\r\nSec-WebSocket-Version: 13\r\n\r\n")
            if forwarded_for is not None:
                ipaddress.ip_address(forwarded_for)
                request = request[:-2] + f"X-Forwarded-For: {forwarded_for}\r\nForwarded: for={forwarded_for}\r\n\r\n"
            remaining = deadline - time.monotonic()
            require(remaining > 0, "raw client connects within its upgrade deadline")
            self.stream.settimeout(remaining)
            self.stream.sendall(request.encode())
            headers = bytearray()
            while b"\r\n\r\n" not in headers:
                require(len(headers) < 16384, "bounded raw client WS upgrade")
                remaining = deadline - time.monotonic()
                require(remaining > 0, "raw upgrade shares one complete handshake deadline")
                self.stream.settimeout(remaining)
                block = self.stream.recv(4096)
                require(block, "raw client receives a complete upgrade response")
                headers.extend(block)
            head, body = bytes(headers).split(b"\r\n\r\n", 1)
            lines = head.decode("ascii").split("\r\n")
            self.summary["upgrade_status"] = int(lines[0].split()[1])
            values = {key.lower(): value.strip() for key, value in (line.split(":", 1) for line in lines[1:])}
            if self.summary["upgrade_status"] != 101:
                length = int(values.get("content-length", "0"))
                require(0 <= length <= 16384 and len(body) <= length, "raw upgrade rejection body stays bounded")
                while len(body) < length:
                    remaining = deadline - time.monotonic()
                    require(remaining > 0, "HTTP rejection body shares the original upgrade deadline")
                    self.stream.settimeout(remaining)
                    block = self.stream.recv(length - len(body))
                    require(block, "HTTP rejection includes its complete bounded body")
                    body += block
                self.summary["rejection_body"] = body.decode("utf8", errors="replace")
                raise NetworkUpgradeRejected(self.summary)
            expected = base64.b64encode(hashlib.sha1((nonce + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            require(values.get("sec-websocket-accept") == expected, "raw fixture checks the actual WS handshake")
            self.pending.extend(body)
            welcome = self.control(deadline=deadline)
            require(welcome.get("type") == "welcome" and isinstance(welcome.get("nonce"), str), "raw client receives the per-connection challenge")
            self.nonce = welcome["nonce"]
        except BaseException:
            self.close()
            raise

    def recv(self, length):
        if self.pending:
            value = bytes(self.pending[:length])
            del self.pending[:length]
            return value
        return self.stream.recv(length)

    def sendall(self, value):
        with self.write_lock:
            self.stream.sendall(value)
            with self.pong_lock:
                pong, self.pending_pong = self.pending_pong, None
            if pong is not None:
                websocket_send_frame(self, pong, opcode=10, masked=True)

    def pong(self, payload):
        if self.write_lock.acquire(blocking=False):
            try:
                websocket_send_frame(self, payload, opcode=10, masked=True)
            finally:
                self.write_lock.release()
        else:
            with self.pong_lock:
                self.pending_pong = payload

    def settimeout(self, value):
        self.stream.settimeout(value)

    def control(self, *, deadline=None):
        deadline = min(deadline or float("inf"), time.monotonic() + 10, self.deadline or float("inf"))
        for _ in range(64):
            opcode, payload = websocket_receive_frame(self, 16 * 1024, expected_mask=False, deadline=deadline)
            if opcode == 9:
                self.pong(payload)
            elif opcode == 10:
                continue
            else:
                require(opcode == 1, "raw fixture expects a public Control")
                return json.loads(payload)
        raise AssertionError("bounded raw control frames before response")

    def send_control(self, value):
        encoded = json.dumps(value, separators=(",", ":")).encode()
        require(len(encoded) <= 16 * 1024, "raw public Control stays within the wire limit")
        remaining = min(10, (self.deadline or float("inf")) - time.monotonic())
        require(remaining > 0, "public Control stays inside the scenario deadline")
        self.stream.settimeout(remaining)
        websocket_send_frame(self, encoded, masked=True)

    def send_binary(self, value):
        require(len(value) <= 65535, "raw encrypted fixture respects the relay frame limit")
        remaining = min(10, (self.deadline or float("inf")) - time.monotonic())
        require(remaining > 0, "binary send stays inside the scenario deadline")
        self.stream.settimeout(remaining)
        websocket_send_frame(self, value, opcode=2, masked=True)

    def binary(self):
        deadline = min(time.monotonic() + 10, self.deadline or float("inf"))
        for _ in range(64):
            opcode, payload = websocket_receive_frame(self, 65535, expected_mask=False, deadline=deadline)
            if opcode == 9:
                self.pong(payload)
            elif opcode == 10:
                continue
            else:
                require(opcode == 2, "raw fixture receives only protected application frames after Ready")
                return payload
        raise AssertionError("bounded raw binary frames before response")

    def reserve(self, owner):
        proof = json.dumps({"nonce": self.nonce, "peer_id": owner.peer_id}, separators=(",", ":"))
        self.send_control({"type": "reserve", "peer_id": owner.peer_id, "public_key": owner.public_key,
                           "signature": owner.sign("relay-reserve", proof.encode())})
        return self.control()

    def attach(self, owner, token):
        proof = json.dumps({"nonce": self.nonce, "token": token}, separators=(",", ":"))
        self.send_control({"type": "attach", "token": token, "signature": owner.sign("relay-attach", proof.encode())})
        return self.control()

    def close(self):
        if self.closed:
            return
        self.closed = True
        self.summary["closed_wall_time"] = time.time()
        if self.stream is not None:
            try:
                self.stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.stream.close()


class NetworkCryptoFixture:
    """One fixed, private fixture identity and Noise channel; never starts a model."""
    def __init__(self, run, name, module, *, expected_peer_id=None, deadline=None):
        repository = Path(__file__).resolve().parents[1]
        self.identity_file = run / f"{name}-identity.json"
        self.deadline = deadline
        require(self.identity_file.parent.resolve() == run.resolve() and self.identity_file.name == f"{name}-identity.json"
                and "/" not in name and name not in (".", ".."), "fixture identity remains directly inside this private run")
        self.process = subprocess.Popen(["node", str(repository / "scripts/crypto-client.mjs"), "--module", str(module)],
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, bufsize=0)
        try:
            identity = self.command("fixture_identity", identity_file=str(self.identity_file), expected_peer_id=expected_peer_id)
            self.peer_id, self.public_key = identity["peer_id"], identity["public_key"]
        except BaseException:
            self.close()
            raise

    def command(self, operation, **arguments):
        require(self.process is not None and self.process.poll() is None, "fixed-identity crypto fixture remains alive")
        encoded = (json.dumps({"op": operation, **arguments}) + "\n").encode()
        require(len(encoded) <= 2 * 1024 * 1024, "crypto fixture request remains bounded")
        deadline = min(time.monotonic() + 15, self.deadline or float("inf"))
        os.set_blocking(self.process.stdin.fileno(), False)
        pending = memoryview(encoded)
        while pending:
            remaining = deadline - time.monotonic()
            require(remaining > 0 and select.select([], [self.process.stdin], [], remaining)[1], "crypto fixture request shares the command deadline")
            try:
                written = os.write(self.process.stdin.fileno(), pending[:4096])
            except BlockingIOError:
                continue
            require(written > 0, "crypto fixture accepts the bounded request")
            pending = pending[written:]
        response = bytearray()
        while b"\n" not in response:
            remaining = deadline - time.monotonic()
            require(remaining > 0 and select.select([self.process.stdout], [], [], remaining)[0], "crypto fixture complete response shares one bounded deadline")
            block = os.read(self.process.stdout.fileno(), min(65536, 2 * 1024 * 1024 + 1 - len(response)))
            require(block, "crypto fixture returns a complete JSON line before EOF")
            response.extend(block)
            require(len(response) <= 2 * 1024 * 1024, "crypto fixture response remains bounded before its newline")
        require(response.endswith(b"\n") and response.count(b"\n") == 1, "one crypto fixture response corresponds to one command")
        value = json.loads(response)
        require(value.get("ok"), f"crypto fixture {operation} succeeds")
        return value.get("result")

    def sign(self, domain, payload):
        return self.command("sign", domain=domain, data=payload.hex())

    def close(self):
        if self.process is not None:
            try:
                self.process.stdin.close()
            except OSError:
                pass
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
            self.process = None


def network_fixture_noise(caller, target, caller_identity, target_identity):
    context = "vibeface:net1:fixture:v1"
    caller_identity.command("handshake", initiator=True, expected_peer_id=target_identity.peer_id, context=context)
    target_identity.command("handshake", initiator=False, expected_peer_id=caller_identity.peer_id, context=context)
    caller.send_binary(bytes.fromhex(caller_identity.command("write", data="")))
    target_identity.command("read", data=target.binary().hex())
    target.send_binary(bytes.fromhex(target_identity.command("write", data="")))
    caller_identity.command("read", data=caller.binary().hex())
    caller.send_binary(bytes.fromhex(caller_identity.command("write", data="")))
    target_identity.command("read", data=target.binary().hex())
    require(caller_identity.command("remote") == target_identity.peer_id
            and target_identity.command("remote") == caller_identity.peer_id, "fixture Noise authenticates both fixed original identities")
    caller_identity.command("finish")
    target_identity.command("finish")
    return {"caller_peer_id": caller_identity.peer_id, "target_peer_id": target_identity.peer_id,
            "context": context, "scope": "fixture Noise over the original public relay; not personal-network member authorization"}


def network_fixture_canary(sender, receiver, sender_identity, receiver_identity, label):
    payload = ("NET-1 " + label + " " + uuid.uuid4().hex).encode()
    require(len(payload) <= 4096, "canary remains bounded")
    records = sender_identity.command("seal_message", data=payload.hex())
    require(0 < len(records) <= 8, "bounded protected canary records")
    for record in records:
        sender.send_binary(bytes.fromhex(record))
    opened = None
    for _ in range(8):
        opened = receiver_identity.command("open_record", data=receiver.binary().hex())
        if opened is not None:
            break
    require(opened is not None and bytes.fromhex(opened) == payload, "the fixed receiving identity decrypts the exact end-to-end canary")
    return {"label": label, "size": len(payload), "sha256": hashlib.sha256(payload).hexdigest(),
            "sender_peer_id": sender_identity.peer_id, "receiver_peer_id": receiver_identity.peer_id, "decrypted_exactly": True}


class CodexSocket:
    def __init__(self, path):
        self.stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.stream.settimeout(8)
        self.stream.connect(path)
        key = base64.b64encode(os.urandom(16)).decode()
        self.stream.sendall(("GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
                             "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                             "Sec-WebSocket-Key: " + key + "\r\n\r\n").encode())
        header = b""
        while not header.endswith(b"\r\n\r\n"):
            part = self.stream.recv(1)
            require(part and len(header) < 16384, "native Socket handshake remains bounded")
            header += part
        require(header.startswith(b"HTTP/1.1 101"), "native Socket upgrades to WebSocket")
        self.sequence = 0
        self.request("initialize", {"clientInfo": {"name": "vibeface-process-fixture", "version": "1"}})

    def request(self, method, params):
        self.sequence += 1
        request_id = self.sequence
        websocket_send_frame(self.stream, json.dumps({"id": request_id, "method": method, "params": params}).encode(), masked=True)
        while True:
            message = websocket_receive(self.stream)
            if message.get("id") == request_id and "method" not in message:
                require("error" not in message, "native Socket accepts " + method)
                return message["result"]

    def __enter__(self):
        return self

    def __exit__(self, *unused):
        self.stream.close()


def fixture_codex(arguments):
    """Implement the native App Server contract without loading a provider."""
    run = Path(arguments[arguments.index("--fixture-root") + 1])
    address = arguments[arguments.index("--listen") + 1]
    require(address.startswith("unix://"), "fixture only binds its private Unix socket")
    socket_path = Path(address.removeprefix("unix://"))
    state_path = run / "fixture-codex-state.json"
    state = json.loads(state_path.read_text()) if state_path.exists() else {"threads": {}}
    state_lock = threading.Lock()
    if socket_path.exists():
        socket_path.unlink()
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(socket_path))
    listener.listen(16)

    def connection(stream):
        stream.settimeout(5)
        header = b""
        try:
            while not header.endswith(b"\r\n\r\n"):
                part = stream.recv(1)
                if not part:
                    return
                header += part
                require(len(header) <= 16384, "fixture bounds WebSocket handshake")
            key = next(line.split(":", 1)[1].strip() for line in header.decode().splitlines()
                       if line.lower().startswith("sec-websocket-key:"))
            accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            stream.sendall(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                            "Connection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\n\r\n").encode())
            connection_id, subscribed, emitted = uuid.uuid4().hex, set(), set()
            disconnect_seen = (run / "fixture-codex-disconnect").read_text() if (run / "fixture-codex-disconnect").exists() else None
            while True:
                disconnect = (run / "fixture-codex-disconnect").read_text() if (run / "fixture-codex-disconnect").exists() else None
                if disconnect != disconnect_seen:
                    return
                with state_lock:
                    for thread_id in list(subscribed):
                        thread = state["threads"][thread_id]
                        if not thread["turns"]:
                            continue
                        turn = thread["turns"][-1]
                        directive = thread.get("directive", {})
                        gate = directive.get("gate")
                        if turn["status"] == "inProgress" and gate and Path(gate).exists() and (
                                not directive.get("approval") or thread.get("approval_response")):
                            thread["status"] = {"type": "idle"}
                            turn.update(status="completed", items=[{"id": "fixture-answer", "type": "agentMessage",
                                                                   "text": "fixture Codex terminal result"}])
                            private_json(state_path, state)
                        identity = (thread_id, turn["id"])
                        if turn["status"] == "inProgress" and identity not in emitted:
                            emitted.add(identity)
                            websocket_send(stream, {"method": "item/agentMessage/delta", "params": {
                                "threadId": thread_id, "turnId": turn["id"], "itemId": "fixture-answer", "delta": "native working"}})
                            if directive.get("approval") and not thread.get("approval_response"):
                                event = {"id": "approval-" + thread_id, "method": "item/commandExecution/requestApproval",
                                         "params": {"threadId": thread_id, "turnId": turn["id"], "itemId": "fixture-command",
                                                    "command": "printf fixture", "cwd": thread["cwd"],
                                                    "availableDecisions": ["accept", "decline", "cancel"],
                                                    "reason": "controlled approval acceptance"}}
                                for _ in range(2):
                                    websocket_send(stream, event)
                                fixture_record(run, "codex-events.jsonl", {"connection": connection_id,
                                               "thread_id": thread_id, "event": "approval", "copies": 2})
                        terminal = (*identity, turn["status"])
                        if turn["status"] != "inProgress" and terminal not in emitted:
                            emitted.add(terminal)
                            event = {"method": "turn/completed", "params": {"threadId": thread_id, "turn": turn}}
                            websocket_send(stream, event)
                            websocket_send(stream, event)
                if not select.select([stream], [], [], .1)[0]:
                    continue
                message = websocket_receive(stream)
                fixture_record(run, "codex-rpc.jsonl", {"connection": connection_id, "pid": os.getpid(), "message": message})
                method, params = message.get("method"), message.get("params", {})
                with state_lock:
                    if method is None:
                        request_id = message.get("id", "")
                        if isinstance(request_id, str) and request_id.startswith("approval-"):
                            thread_id = request_id.removeprefix("approval-")
                            state["threads"][thread_id]["approval_response"] = message.get("result")
                            private_json(state_path, state)
                            websocket_send(stream, {"method": "serverRequest/resolved", "params": {"requestId": request_id}})
                        continue
                    if "id" not in message:
                        continue
                    if method == "initialize":
                        result = {"userAgent": "controlled-fixture", "platformFamily": "unix", "platformOs": sys.platform}
                    elif method == "thread/loaded/list":
                        result = {"data": list(state["threads"]), "nextCursor": None}
                    elif method == "thread/start":
                        thread_id = "fixture-thread-" + uuid.uuid4().hex
                        thread = {"id": thread_id, "cwd": params["cwd"], "status": {"type": "idle"}, "turns": []}
                        state["threads"][thread_id] = thread
                        result = {"thread": thread}
                        subscribed.add(thread_id)
                    elif method in {"thread/read", "thread/resume", "thread/turns/list", "turn/start", "turn/interrupt"}:
                        thread_id = params["threadId"]
                        thread = state["threads"][thread_id]
                        if method == "turn/start":
                            thread["directive"] = json.loads(params["input"][0]["text"])
                            thread["status"] = {"type": "active", "activeFlags": []}
                            turn = {"id": "fixture-turn-" + uuid.uuid4().hex, "status": "inProgress", "items": []}
                            thread["turns"].append(turn)
                            result = {"turn": turn}
                        elif method == "turn/interrupt":
                            require(thread["turns"][-1]["id"] == params["turnId"], "interrupt selects the original turn")
                            thread["status"] = {"type": "idle"}
                            thread["turns"][-1]["status"] = "interrupted"
                            result = {}
                        elif method == "thread/turns/list":
                            result = {"data": thread["turns"], "nextCursor": None}
                        else:
                            result = {"thread": thread}
                        if method != "thread/read":
                            subscribed.add(thread_id)
                    else:
                        websocket_send(stream, {"id": message["id"], "error": {"code": -32601, "message": "fixture does not implement " + method}})
                        continue
                    private_json(state_path, state)
                    websocket_send(stream, {"id": message["id"], "result": result})
        except (EOFError, OSError):
            pass
        finally:
            stream.close()

    while True:
        stream, _ = listener.accept()
        threading.Thread(target=connection, args=(stream,), daemon=True).start()


def fixture_manager(arguments):
    """Exercise only the sidecar protocol and real scoped Node tools, never a model."""
    run = Path(arguments[arguments.index("--fixture-root") + 1])
    initial = json.loads(sys.stdin.readline())
    require(initial["type"] == "init", "fixture receives sidecar initialization")
    prompt = initial["prompt"]
    start = prompt.rfind("\n\n{")
    finalization = "This is a read-only reporting turn." in prompt
    if finalization:
        previous = next((record for record in reversed(fixture_records(run))
                         if record["native_session_id"] == initial.get("session_id")), None)
        require(previous is not None, "final reporting resumes the original fixture SDK session")
        require(initial.get("read_only") and not initial.get("custom_tools"),
                "final reporting has read-only SDK authority and no write callback")
        print(json.dumps({"type": "ready", "session_id": initial["session_id"]}), flush=True)
        require(json.loads(sys.stdin.readline())["type"] == "ack", "final reporting waits for ACK")
        fixture_record(run, "manager-invocations.jsonl", {**previous, "resumed": True,
                       "read_only": True, "finalization": True, "pid": os.getpid()})
        print(json.dumps({"type": "result", "session_id": initial["session_id"],
                          "final_response": previous["label"] + ":complete"}), flush=True)
        return
    require(start >= 0, "fixture receives explicit management context")
    context, _ = json.JSONDecoder().raw_decode(prompt[start + 2:])
    trigger = context["trigger_event_hash"]
    body = (context.get("trigger_event") or {}).get("body", {})
    task = body.get("message", {}) if body.get("type") == "agent_message" else {}
    directive = {}
    try:
        candidate = json.loads(task.get("text", body.get("text", "")))
        if isinstance(candidate, dict):
            directive = candidate
    except ValueError:
        pass
    autonomous = body.get("role") != "user" and task.get("kind") != "task"
    label = directive.get("label", task.get("message_id", body.get("message_id", "autonomous")))
    native = initial.get("session_id") or "fixture-native-" + uuid.uuid4().hex
    read_only = bool(initial.get("read_only"))
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
              "agent_task": task.get("kind") == "task", "finalization": False,
              "bot": context.get("configured_bot"), "bot_prompt_markers": [marker for marker in
                  ("bot-alpha-instructions-sentinel", "bot-beta-instructions-sentinel") if marker in prompt],
              "node_url": initial["tool_env"]["VIBEFACE_NODE_URL"]}
    fixture_record(run, "manager-invocations.jsonl", record)
    schemas = initial.get("custom_tools", {})
    expected_tools = {"nodes", "context", "task_status", "call_lookup"}
    if not read_only:
        expected_tools.update({"send_task", "cancel_task", "observe_session", "codex"})
    require(set(schemas) == expected_tools, "sidecar receives the authoritative scoped high-level tool catalog")
    fixture_record(run, "manager-tool-catalog.jsonl", {"trigger": trigger, "tools": schemas})

    def tool(name, arguments, expected=True, rejection="read-only", call_ref=None, discard=False):
        environment = initial["tool_env"]
        arguments = dict(arguments)
        if call_ref and name == "send_task":
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

    host_command = directive.get("host_command")
    if host_command:
        require(not read_only, "read-only result reporting cannot execute a host command")
        fixture_record(run, "fixture-invocations.jsonl", {"native_session_id": native,
                       "label": label, "pid": os.getpid(), "trigger": trigger})
        completed = subprocess.run(["/bin/sh", "-c", host_command],
                                   cwd=directive.get("host_cwd", initial["cwd"]),
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=45)
        require(completed.returncode == 0, "fixture host command exits successfully")
    dispatch_spec = directive.get("dispatch")
    if dispatch_spec:
        label = dispatch_spec["label"]
        arguments = {"target_node": dispatch_spec["target_node"],
                     "text": json.dumps({key: value for key, value in dispatch_spec.items()
                                         if key != "target_node"})}
        tool("send_task", arguments, call_ref="task-" + label)
    if autonomous:
        require(read_only, "ordinary result callbacks have server-enforced read-only authority")
        tool("send_task", {"target_node": "forbidden-followup", "text": "forbidden"},
             expected=False)
    if directive.get("native"):
        spec = directive["native"]
        require("VIBEFACE_CODEX_SOCKET" not in initial["tool_env"], "Agents never receive the native Socket")
        tool("codex", {"method": "thread/start", "params": {"cwd": spec["cwd"], "approvalPolicy": "never"}},
             expected=False, rejection="not available to Agents", call_ref="native-bypass-" + label)
        thread = tool("codex", {"method": "thread/start", "params": {"cwd": spec["cwd"]}},
                      call_ref="native-thread-" + label)["thread"]
        turn = tool("codex", {"method": "turn/start", "params": {"threadId": thread["id"], "cwd": spec["cwd"],
                    "input": [{"type": "text", "text": json.dumps(spec)}]}}, call_ref="native-turn-" + label)["turn"]
        work = tool("observe_session", {"thread_id": thread["id"], "turn_id": turn["id"]})
        fixture_record(run, "manager-native-work.jsonl", {"trigger": trigger, "label": label,
                       "work_id": work["work_id"], "thread_id": thread["id"], "turn_id": turn["id"]})
    for action in directive.get("tools", []):
        call_ref = action.get("call_ref")
        for index in range(action.get("repeat", 1)):
            value = tool(action["name"], action["arguments"], expected=action.get("accepted", True),
                         rejection=action.get("rejection", "read-only"), call_ref=call_ref,
                         discard=action.get("discard_first", False) and index == 0)
        if action.get("wait_terminal"):
            wait_for("Agent task reaches its exact terminal state", lambda:
                tool("task_status", {"task_ref": value["task_ref"]}).get("state")
                in {"completed", "failed", "interrupted"})
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
    emit({"type": "result", "session_id": native,
          "final_response": "x" * directive.get("result_size", 0) or label + ":complete"})


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
                 node.directory / "bootstrap.json"]
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


def recover_fresh_node(node, backup, password):
    node.start()
    node.client.connect(node.peer_id)
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
        {"subject_id": info["peer_id"], "public_key": info["public_key"], "kind": "node", "name": node.name,
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


def verify_recovery(node, other, nodes, network, *, entry_url=None):
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
    other.url = entry_url or node.url
    if entry_url:
        other.command("connect_auto", url=entry_url, peer_id=node.peer_id,
                      transport=str(Path(__file__).resolve().parents[1] / "crates/node/web/transport.js"))
    else:
        other.connect(node.peer_id)
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


def verify_channel_security(node):
    wrong = node.client.command("connect", url=node.url, peer_id="ab" * 32,
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


def local_events(node, network, conversation):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        rows = connection.execute("SELECT envelope FROM events WHERE network_id=? AND scope_id=?",
                                  (network, conversation)).fetchall()
    return [json.loads(json.loads(row[0])["canonical_body"]) for row in rows]


def wait_for_agent_target(run, observer, target, cwd, *, reason):
    cwd = Path(cwd)
    def ready():
        row = next((row for row in observer.client.rpc("nodes")["nodes"] if row["peer_id"] == target.peer_id), None)
        capabilities = (row or {}).get("capabilities") or {}
        return row if row and row["online"] and not row["revoked"] and "scheduler" in row["roles"] and capabilities.get("manager") and any(
            cwd == Path(root) or Path(root) in cwd.parents for root in capabilities.get("roots", [])) else None
    return wait_for("authorized Agent and host roots " + reason, ready)


def agent_task_snapshot(client, conversation, task_ref):
    return next((task for task in client.rpc("conversation", conversation_id=conversation)["agent_tasks"]
                 if task["task_ref"] == task_ref), None)


def agent_tool_result(run, conversation, call_ref):
    records = [row["result"] for row in fixture_records(run, "manager-tools.jsonl")
               if row["conversation_id"] == conversation and row.get("call_ref") == call_ref and row.get("result")]
    return records[-1] if records else None

def chat_agent_checks(run, workspace, a, b, c, network, passed):
    def calls(name, label=None):
        return [row for row in fixture_records(run) if row["conversation_id"] == name and not row.get("finalization")
                and (label is None or row["label"] == label)]

    def prepare(name):
        a.client.rpc("create_conversation", conversation_id=name, title=name)
        directory = workspace / name
        directory.mkdir()
        return directory

    def send(name, label, **directive):
        return a.client.rpc("message", conversation_id=name, message_id=label,
                            text=json.dumps({"chat_agent": True, "label": label, **directive}))

    def completed(receipt):
        job = scheduler_record(a, network, receipt["trigger"])
        if job and job["state"] in {"failed", "unknown", "cancelled"}:
            raise RuntimeError("Agent source reached " + job["state"] + ": " + job.get("detail", ""))
        return job if job and job["state"] == "completed" else None

    name = "chat-task-dedup"
    directory = prepare(name)
    wait_for_agent_target(run, a, b, directory, reason="before cross-device task")
    text = json.dumps({"label": "task-once", "host_command": "printf once >> count.txt", "host_cwd": str(directory)})
    receipt = send(name, "source-once", tools=[
        {"name": "send_task", "call_ref": "task-once", "repeat": 2, "discard_first": True,
         "arguments": {"target_node": b.peer_id, "text": text}},
        {"name": "call_lookup", "arguments": {"call_ref": "task-once"}},
        {"name": "send_task", "call_ref": "task-once", "accepted": False,
         "rejection": "agent message ID reused with different source, content or target",
         "arguments": {"target_node": b.peer_id, "text": "different work"}}])
    task = wait_for("discarded send_task response is found through the same call reference", lambda: agent_tool_result(run, name, "task-once"))
    wait_for("source waits for the original target and commits its result", lambda: completed(receipt))
    snapshot = wait_for("third entry projects the completed Agent task", lambda:
        row if (row := agent_task_snapshot(c.client, name, task["task_ref"])) and row["state"] == "completed" else None)
    require(snapshot["source_node"] == a.peer_id and snapshot["target_node"] == b.peer_id,
            "Agent result remains pinned to its original source and target")
    require((directory / "count.txt").read_text() == "once" and len(invocations(run, "task-once")) == 1,
            "retried send_task performs exactly one target-host side effect")
    history = c.client.rpc("conversation", conversation_id=name)
    require("tasks" not in history and history["autonomy"]["version"] == 0 and len(history["agent_tasks"]) == 1,
            "ordinary Agent work creates no hidden business Task and grants no conversation autonomy")
    endings = [event["body"]["message"] for event in local_events(c, network, name)
               if event["body"].get("type") == "agent_message" and event["body"]["message"].get("in_reply_to") == task["task_ref"]
               and event["body"]["message"]["kind"] in {"completed", "failed", "interrupted"}]
    require(len(endings) == 1 and endings[0]["kind"] == "completed", "exactly one signed terminal reply returns from B")
    require(all(row["read_only"] for row in calls(name) if row["autonomous"]),
            "ordinary result callbacks cannot gain write authority")
    passed("A asks B's Agent to run one host command; response loss, retries and C history import preserve one task and one result",
           task_ref=task["task_ref"], source_trigger=receipt["trigger"])

    name = "chat-task-cancel"
    directory = prepare(name)
    receipt = send(name, "source-cancel", dispatch={"label": "target-cancel", "target_node": b.peer_id,
        "host_command": "printf once >> count.txt", "host_cwd": str(directory), "gate": str(directory / "never-release")})
    task = wait_for("cancel target is durably delegated", lambda: agent_tool_result(run, name, "task-target-cancel"))
    wait_for("cancelled task crosses one real host side effect", lambda: (directory / "count.txt").exists())
    a.client.rpc("cancel_agent_task", conversation_id=name, task_ref=task["task_ref"])
    wait_for("confirmed target cancellation returns Interrupted", lambda:
        (row := agent_task_snapshot(a.client, name, task["task_ref"])) and row["state"] == "interrupted")
    require_quiet("cancelled Agent task is never replayed", lambda:
        (directory / "count.txt").read_text() == "once" and len(invocations(run, "target-cancel")) == 1)
    passed("Agent task cancellation fences its original attempt, confirms interruption and never repeats the host effect",
           task_ref=task["task_ref"])

    name = "chat-task-restart"
    directory = prepare(name)
    receipt = send(name, "source-restart", dispatch={"label": "target-restart", "target_node": b.peer_id,
        "host_command": "printf once >> count.txt", "host_cwd": str(directory), "gate": str(directory / "never-release")})
    task = wait_for("restart target task is recorded", lambda: agent_tool_result(run, name, "task-target-restart"))
    wait_for("restart task has one real side effect", lambda: (directory / "count.txt").exists())
    running = wait_for("target task is Running before crash", lambda:
        (row := scheduler_record(b, network, task["task_ref"])) and row["state"] == "running" and row)
    b.stop(crash=True)
    for row in calls(name, "target-restart"):
        try:
            os.kill(row["pid"], signal.SIGKILL)
        except ProcessLookupError:
            pass
    b.start()
    b.client.connect()
    b.client.authenticate()
    unknown = wait_for("running target task recovers as Unknown", lambda:
        row if (row := scheduler_record(b, network, task["task_ref"])) and row["state"] == "unknown" else None)
    require(unknown["attempt"]["id"] == running["attempt"]["id"], "Unknown retains the original attempt identity")
    require_quiet("Unknown recovery never replays or reports success", lambda:
        (directory / "count.txt").read_text() == "once" and len(invocations(run, "target-restart")) == 1
        and (scheduler_record(b, network, task["task_ref"]) or {}).get("state") == "unknown")
    endings = [event for event in local_events(b, network, name) if event["body"].get("type") == "agent_message"
               and event["body"]["message"].get("in_reply_to") == task["task_ref"]
               and event["body"]["message"]["kind"] in {"completed", "failed", "interrupted"}]
    require(not endings, "Unknown work cannot manufacture a terminal Agent result")
    passed("target crash preserves the original Unknown attempt and file effect without replay or a false successful result",
           task_ref=task["task_ref"], attempt_id=unknown["attempt"]["id"])

def bot_checks(run, workspace, a, b, c, network, passed, outsider):
    definitions = [{"bot_id": "bot-" + label, "conversation_id": "bot-history-" + label,
                    "name": label.capitalize(), "instructions": "bot-" + label + "-instructions-sentinel",
                    "project": {"target_node": b.peer_id, "cwd": str(workspace)}} for label in ("alpha", "beta")]
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
    def rpc(method):
        return [row for row in fixture_records(run, "codex-rpc.jsonl") if row["message"].get("method") == method]

    def pending(name, work_id):
        return next((work for work in c.client.rpc("conversation", conversation_id=name)["native_sessions"]
                     if work["work_id"] == work_id and work["pending_requests"]), None)

    def approval_notifications(name):
        with sqlite3.connect(f"file:{b.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
            rows = [json.loads(row[0]) for row in connection.execute(
                "SELECT record FROM notification_outbox WHERE network_id=?", (network,))]
        return [row for row in rows if row["conversation_id"] == name and row["kind"] == "needs_input"]

    def start(name, approval=False):
        directory = workspace / name
        directory.mkdir()
        a.client.rpc("create_conversation", conversation_id=name, title=name)
        a.client.rpc("message", conversation_id=name, message_id=name,
            text=json.dumps({"chat_agent": True, "label": name, "dispatch": {
                "label": name, "target_node": b.peer_id,
                "native": {"cwd": str(directory), "gate": str(directory / "release"), "approval": approval}}}))
        task = wait_for("native task is delegated to B", lambda: agent_tool_result(run, name, "task-" + name))
        native = wait_for("SDK registers the original native thread and turn", lambda: next(
            (row for row in fixture_records(run, "manager-native-work.jsonl") if row["label"] == name), None))
        def sdk_returned():
            job = scheduler_record(b, network, task["task_ref"])
            if not job or job["state"] != "running":
                return None
            with sqlite3.connect(f"file:{b.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
                row = connection.execute("SELECT value FROM local_values WHERE network_id=? AND key=?",
                                         (network, "sdk-returned:v1:" + job["attempt"]["id"])).fetchone()
            return job if row and json.loads(row[0]).get("sdk_returned") else None
        running = wait_for("SDK return leaves the original background native task Running", sdk_returned)
        return directory, task, native, running

    name = "chat-startup-subscription"
    directory, task, native, running = start(name, approval=True)
    work = wait_for("remote client projects the original native approval", lambda: pending(name, native["work_id"]))
    require(len(work["pending_requests"]) == 1 and work["thread_id"] == native["thread_id"]
            and work["turn_id"] == native["turn_id"], "duplicate approvals project once for the exact registered turn")
    request_id = work["pending_requests"][0]["request_id"]
    notifications = wait_for("duplicate native approval persists one notification", lambda:
        rows if len(rows := approval_notifications(name)) == 1 else None)
    notification = notifications[0]
    require(notification["notification_id"] == "native-input-" + hashlib.sha256(json.dumps(
        [native["work_id"], request_id], separators=(",", ":"), ensure_ascii=False).encode()).hexdigest(),
        "persisted native approval identifies the original work and request")
    snapshot = b.client.rpc("export", conversation_id=name)
    verify_snapshot(snapshot)
    events = {signed["hash"]: json.loads(signed["canonical_body"]) for signed in snapshot["events"]}
    raised = events.get(notification["event_hash"])
    require(raised is not None and raised["network_id"] == network and raised["scope_id"] == name
            and raised["author"] == b.peer_id and raised["body"] == {
                "type": "notification_raised", "notification_id": notification["notification_id"],
                "kind": notification["kind"], "text": notification["text"]},
            "persisted native approval points to the target's signature-verified NotificationRaised event")
    ancestors, pending_hashes = set(), raised["parents"] + ([raised["previous"]] if raised["previous"] else [])
    while pending_hashes:
        event_hash = pending_hashes.pop()
        if event_hash in ancestors:
            continue
        ancestors.add(event_hash)
        event = events[event_hash]
        pending_hashes.extend(event["parents"] + ([event["previous"]] if event["previous"] else []))
    require(task["task_ref"] in ancestors, "native approval retains the original Agent task as a causal ancestor")
    require(not any(row["message"].get("id") == request_id and "method" not in row["message"]
                    for row in fixture_records(run, "codex-rpc.jsonl")), "native observation never approves automatically")
    require(all(row["message"]["params"].get("approvalPolicy") == "on-request"
                and row["message"]["params"].get("approvalsReviewer") == "user"
                and (row["message"]["params"].get("sandbox") == "workspace-write"
                     or row["message"]["params"].get("sandboxPolicy", {}).get("type") == "workspaceWrite")
                for row in rpc("thread/start") + rpc("turn/start")),
            "the Node pins every Agent native start to user approval in a workspace sandbox")
    sdk_calls_before_restart = len(fixture_records(run))
    for node in (a, b, c):
        node.client.command("close")
    previous_pids = {row["pid"] for row in rpc("initialize")}
    b.stop()
    b.start()
    restored = wait_for("startup restores native subscription with all Web clients disconnected", lambda: next(
        (row for row in rpc("thread/resume") if row["pid"] not in previous_pids
         and row["message"]["params"].get("threadId") == native["thread_id"]), None))
    require(restored["message"]["params"].get("excludeTurns") is True,
            "startup subscribes through native read/resume calls without a new turn")
    private_json(run / "fixture-codex-disconnect", uuid.uuid4().hex)
    wait_for("native reconnect restores the same subscription", lambda:
        len({row["connection"] for row in rpc("thread/resume") if row["pid"] == restored["pid"]
             and row["message"]["params"].get("threadId") == native["thread_id"]}) >= 2)
    require_quiet("native approval replay preserves one durable notification without Web", lambda:
        approval_notifications(name) == notifications and len(fixture_records(run)) == sdk_calls_before_restart)
    c.client.connect()
    c.client.authenticate()
    wait_for("reconnected third entry sees the same pending approval", lambda: pending(name, native["work_id"]))
    require(len(rpc("turn/start")) == 1 and not any(row["message"].get("id") == request_id and "method" not in row["message"]
            for row in fixture_records(run, "codex-rpc.jsonl")), "restarts do not restart or approve native work")
    c.client.rpc("native_respond", conversation_id=name, work_id=native["work_id"], target_node=b.peer_id,
                 request_id=request_id, response={"decision": "decline"})
    directory.joinpath("release").touch()
    wait_for("exact native completion reconciles the original target Agent task", lambda:
        (scheduler_record(b, network, task["task_ref"]) or {}).get("state") == "completed")
    wait_for("completion reaches C from the original signed Agent reply", lambda:
        (row := agent_task_snapshot(c.client, name, task["task_ref"])) and row["state"] == "completed")
    require(len(rpc("turn/start")) == 1, "startup and reconnect never replay the original native turn")
    summaries = [row for row in fixture_records(run) if row["trigger"] == task["task_ref"] and row.get("finalization")]
    require(len(summaries) == 1 and summaries[0]["read_only"] and summaries[0]["resumed"],
            "verified native terminal work resumes the original SDK once for a read-only final answer")
    responses = [row for row in fixture_records(run, "codex-rpc.jsonl") if row["message"].get("id") == request_id
                 and "method" not in row["message"]]
    require(len(responses) == 1 and responses[0]["message"]["result"] == {"decision": "decline"},
            "only the explicit human rejection is sent to the original native request")
    for node in (a, b):
        node.client.connect()
        node.client.authenticate()
    passed("SDK return keeps native work Running; startup and reconnect restore exact approvals, and only an explicit human reply resolves them",
           task_ref=task["task_ref"], native_thread=native["thread_id"], native_turn=native["turn_id"])

    name = "chat-restart-reconcile"
    directory, task, native, running = start(name)
    for node in (a, b, c):
        node.client.command("close")
    b.stop()
    state_path = run / "fixture-codex-state.json"
    state = json.loads(state_path.read_text())
    thread = state["threads"][native["thread_id"]]
    thread["status"] = {"type": "idle"}
    thread["turns"][-1].update(status="completed", items=[{
        "id": "offline-answer", "type": "agentMessage", "text": "fixture completed while Node was offline"}])
    private_json(state_path, state)
    b.start()
    wait_for("startup reads the precise completion missed while offline", lambda:
        (scheduler_record(b, network, task["task_ref"]) or {}).get("state") == "completed")
    require(len(rpc("turn/start")) == 2, "missed completion recovery never starts another native turn")
    for node in (a, b, c):
        node.client.connect()
        node.client.authenticate()
    passed("a saved SDK return and exact missed native terminal outcome reconcile the original task without replay",
           task_ref=task["task_ref"], native_turn=native["turn_id"])

    name = "chat-native-interrupt"
    directory, task, native, running = start(name)
    wait_for("third entry receives the original task before interrupt", lambda:
        agent_task_snapshot(c.client, name, task["task_ref"]))
    reply = c.client.rpc("native_interrupt", conversation_id=name, work_id=native["work_id"], target_node=b.peer_id)
    require(reply["interrupt_requested"] and not reply["confirmed"], "interrupt admission is distinct from native terminal confirmation")
    wait_for("native interrupted event confirms the exact original turn", lambda:
        (scheduler_record(b, network, task["task_ref"]) or {}).get("state") == "cancelled")
    interrupted = rpc("turn/interrupt")
    require(len(interrupted) == 1 and interrupted[0]["message"]["params"] == {
        "threadId": native["thread_id"], "turnId": native["turn_id"]}, "native interruption cannot select another thread or turn")
    require(len(rpc("turn/start")) == 3, "native interruption never restarts the work")
    passed("native interrupt returns pending, then the original turn confirms cancellation without replay",
           task_ref=task["task_ref"], native_turn=native["turn_id"])

def chat_agent_acceptance(options):
    binary = options.binary.resolve(strict=True)
    module = options.crypto_module.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("chat-agent-process-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir()
    fixture = fixture_wrapper(run, "fixture-manager", "--fixture-manager")
    codex = fixture_wrapper(run, "fixture-codex", "--fixture-codex")
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    nodes = []
    report = {"status": "running", "source_binary": str(binary), "binary_sha256": file_sha256(binary),
              "provider_calls": 0, "coverage": "real Node and Noise; fixture SDK and native App Server", "evidence": [], "not_checked": ["real provider model behavior",
                  "rendered browser UI", "real LAN/public ingress", "OS sandboxing of SDK host tools"]}

    def passed(description, **evidence):
        report["evidence"].append({"check": description, **evidence})
        print("PASS " + description, flush=True)

    try:
        for name in ("A", "B", "C", "BOT-OTHER-NETWORK"):
            node = Node(run, name, binary, workspace, fixture, helper, module)
            node.configure(timeout_secs=600, manager=fixture_manager_settings(run, workspace) if name == "B" else None,
                           codex_bin=str(codex) if name == "B" else sys.executable)
            nodes.append(node)
        a, b, c, outsider = nodes
        a.configure(manager=fixture_manager_settings(run, workspace), daily_decision_limit=100,
                    network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False, "relay_enabled": True})
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
            all(any(row["peer_id"] == target.peer_id and row["online"] for row in a.client.rpc("nodes")["nodes"])
                for target in (b, c)))
        bot_checks(run, workspace, a, b, c, network, passed, outsider)
        chat_agent_checks(run, workspace, a, b, c, network, passed)
        startup_subscription_checks(run, workspace, a, b, c, network, passed)
        report["status"] = "passed"
        report["manager_invocations"] = len(fixture_records(run))
        report["host_command_invocations"] = len(invocations(run))
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        for directory_name in ("chat-task-restart", "chat-startup-subscription"):
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
    fixture = fixture_wrapper(run, "fixture-manager", "--fixture-manager")
    manager = fixture_manager_settings(run, workspace)
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    nodes = []
    report = {"status": "running", "source_binary": str(source_binary),
              "binary_sha256": file_sha256(binary), "provider_calls": 0,
              "coverage": "real Node and durable Scheduler; fixture SDK",
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
                             scheduler_node=a.peer_id, report_results=directive.get("report_results", False))
        require(receipt["accepted"] is True and receipt["message_id"] == label
                and receipt["scheduler_node"] == a.peer_id, "acceptance pins message and Scheduler")
        proof = receipt["receipt"]
        require(proof["version"] == 1 and proof["network_id"] == network
                and proof["conversation_id"] == name and proof["trigger"] == receipt["trigger"]
                and proof["target_node"] == a.peer_id, "durable receipt has the exact accepted identity")
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
        a.configure(manager=manager, codex_bin=sys.executable, daily_decision_limit=100,
                    network={"bootstrap_urls": [], "advertise_urls": [a.url.replace("http:", "ws:") + "/api/network/ws"],
                             "mdns": False, "relay_enabled": True})
        a.start()
        network = a.bootstrap()
        for node in (b, c):
            node.configure(manager=manager if node is b else None, codex_bin=sys.executable,
                           network={"bootstrap_urls": [a.url.replace("http:", "ws:") + "/api/network/ws"],
                                    "advertise_urls": [node.url.replace("http:", "ws:") + "/api/network/ws"],
                                    "mdns": False, "relay_enabled": True})
            node.pair(a, network)
            node.start()
            reconnect(node)
        wait_for("three private Nodes become mutually reachable", lambda:
                 all(any(row["peer_id"] == target.peer_id and row["online"]
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
                             scheduler_node=a.peer_id, report_results=False)
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
        require(all(row["source_node"] == a.peer_id and row["invocation_id"]
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
            "name": "send_task", "arguments": {"target_node": b.peer_id,
                "text": "forbidden restart work"}, "accepted": False}])
        wait_for("new read-only recovery conversation remains usable", lambda: completed(readonly))
        require(not (workspace / "forbidden-restart").exists(), "read-only recovery cannot create business work")

        def cached_remote_session():
            with sqlite3.connect(f"file:{c.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
                sessions = [json.loads(row[0]) for row in connection.execute(
                    "SELECT value FROM local_values WHERE network_id=? AND key LIKE 'scheduler:v1:remote-session:%'", (network,))]
            return next((session for session in sessions if any(job.get("trigger") == interrupted["trigger"]
                         for job in session.get("jobs", []))), None)

        def remote_frozen(expected):
            snapshot = conversation(c.client, name)
            cached = cached_remote_session()
            unknown = next((job for job in snapshot["scheduler"]["jobs"] if job["trigger"] == interrupted["trigger"]), None)
            return cached if (cached and cached["write_frozen"] is expected
                and snapshot["scheduler"]["write_frozen"] is expected and unknown and unknown["state"] == "unknown") else None

        cached_frozen = wait_for("C caches the original remote Unknown as write frozen", lambda: remote_frozen(True))
        c.stop()
        require(cached_remote_session()["write_frozen"], "remote frozen status is persisted before C restarts")
        c.start()
        reconnect(c)
        wait_for("restarted C restores the same remote frozen status", lambda: remote_frozen(True))
        a.client.rpc("resolve_manager_unknown", conversation_id=name, trigger=interrupted["trigger"],
                     evidence="Owned fixture process terminated; original attempt admitted no business operations.")
        cached_resolved = wait_for("remote reconciliation releases C's cached freeze without a new job", lambda: remote_frozen(False))
        require(cached_resolved["status_revision"] > cached_frozen["status_revision"],
                "remote permission changes advance the original target's observed status revision")
        c.stop()
        require(cached_remote_session()["write_frozen"] is False, "remote freeze release persists before C restarts")
        c.start()
        reconnect(c)
        wait_for("restarted C keeps reconciled remote history writable", lambda: remote_frozen(False))
        writable, _ = send(a.client, name, "after-human-reconcile",
                           host_command="printf allowed > after-reconcile", host_cwd=str(workspace))
        wait_for("new human round receives normal authority only after reconciliation", lambda: completed(writable))
        require((workspace / "after-reconcile").read_text() == "allowed" and not conversation(a.client, name)["scheduler"]["write_frozen"]
                and state(interrupted, "unknown") and len(calls(name, label="restart-running")) == 1,
                "reconciliation restores future writes without changing or replaying the original Unknown")
        passed("restart preserves Unknown, enforces read-only recovery and restores only future writes after human reconciliation",
               attempt_id=old_attempt)
        passed("cached remote Unknown survives C restart, then reconciliation releases and persists the freeze without replay",
               attempt_id=old_attempt, frozen_revision=cached_frozen["status_revision"],
               reconciled_revision=cached_resolved["status_revision"])

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
                                 scheduler_node=a.peer_id, report_results=False)
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
        report["host_command_invocations"] = len(invocations(run))
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


def verify_local_access(node):
    launcher = json.loads((node.directory / "local-access.json").read_text())
    browser = Client(launcher["origin"], node.client.helper, node.client.module)
    try:
        require(json.loads((node.directory / "config.json").read_text())["local_auto_access"],
                "local browser access is enabled by default")
        node.client.request("/api/local/info", status=404, headers={"Forwarded": "for=127.0.0.1;host=localhost"})
        browser.request("/api/local/launch", {}, status=401, headers={"X-Forwarded-For": "127.0.0.1"})
        info = browser.request("/api/local/info")
        require(info["enabled"] and not info["has_session"], "bare localhost grants no browser identity")
        opened = subprocess.run([str(node.binary), "access", "--json", "--data-dir", str(node.directory)],
                                capture_output=True, text=True, timeout=15)
        require(opened.returncode == 0, "local CLI opens the trusted browser entry")
        entry = json.loads(opened.stdout)
        parsed = urllib.parse.urlparse(entry["url"])
        fragment = urllib.parse.parse_qs(parsed.fragment)
        require(entry["local"] and parsed._replace(fragment="").geturl().rstrip("/") == launcher["origin"]
                and fragment["node"] == [node.peer_id] and "token" not in parsed.query,
                "one-time local credential appears only in the pinned URL fragment")
        identity = browser.command("identity")
        exchange = {"token": fragment["local"][0], "peer_id": node.peer_id, **identity}
        request = urllib.request.Request(launcher["origin"] + "/api/local/exchange", json.dumps(exchange).encode(),
                                         {"Origin": launcher["origin"], "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=10) as response:
            cookie = response.headers["Set-Cookie"]
            require("HttpOnly" in cookie and "SameSite=Strict" in cookie, "local exchange uses a private browser cookie")
        cookie = cookie.split(";", 1)[0]
        browser.request("/api/local/exchange", exchange, status=401)
        require(browser.request("/api/local/info", headers={"Cookie": cookie})["subject_id"] == identity["subject_id"],
                "local session binds to the browser key that exchanged the one-time credential")
        members = node.client.call("directory")["view"]["members"]
        require(not any(member["subject_id"] == identity["subject_id"] for member in members),
                "local access creates no permanent personal-network member")
        node.client.call("local_access", {"enabled": False})
        require(not json.loads((node.directory / "config.json").read_text())["local_auto_access"]
                and not browser.request("/api/local/info", headers={"Cookie": cookie})["has_session"],
                "closing local access persists and revokes existing browser sessions")
        browser.request("/api/local/launch", {}, status=401, headers={"Authorization": "Bearer " + launcher["capability"]})
        node.client.call("local_access", {"enabled": True})
        require(not browser.request("/api/local/info", headers={"Cookie": cookie})["has_session"],
                "re-enabling local access never revives an old browser session")
    finally:
        browser.close()


def verify_real_codex_transport(run, workspace, executable):
    require(shutil.which(executable) is not None or Path(executable).is_file(), "installed Codex is available")
    socket_dir = Path("/tmp") / ("vf-codex-" + uuid.uuid4().hex)
    socket_dir.mkdir(mode=0o700)
    path = socket_dir / "app.sock"
    log = (run / "real-codex.log").open("wb")
    process = subprocess.Popen([executable, "app-server", "--listen", "unix://" + str(path)],
                               stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
    try:
        wait_for("installed native App Server socket", lambda: path.exists() and process.poll() is None)
        with CodexSocket(str(path)) as codex:
            thread = codex.request("thread/start", {"cwd": str(workspace), "approvalPolicy": "untrusted", "sandbox": "read-only"})["thread"]
            actual = codex.request("thread/read", {"threadId": thread["id"], "includeTurns": True})["thread"]
            require(actual["id"] == thread["id"] and not actual["turns"], "native thread is readable without starting a model turn")
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
        log.close()
        if path.exists():
            path.unlink()
        socket_dir.rmdir()


def web_acceptance(options):
    binary = options.binary.resolve(strict=True)
    module = options.crypto_module.resolve(strict=True)
    require(module.with_name("vibeface_secure_bg.wasm").read_bytes().startswith(b"\x00asm"),
            "packaged browser WASM is available")
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("web-process-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    report = {"status": "running", "binary_sha256": file_sha256(binary), "provider_calls": 0,
              "coverage": "real packaged Node HTTP resources, CSP, SPA routes and browser WASM",
              "not_checked": ["Node business tests", "provider behavior", "rendered browser UI"]}
    node = None
    try:
        node = Node(run, "WEB", binary, workspace, None, None, module, relay_only=True)
        node.configure(codex_socket=str(run / "unavailable.sock"))
        node.start()
        verify_web_assets(node, module)
        require(node.process.poll() is None, "packaged Node remains running after Web checks")
        require(file_sha256(binary) == report["binary_sha256"], "Web acceptance preserves the binary")
        report["status"] = "passed"
        print("PASS packaged Web resources, CSP, SPA routes and browser WASM", flush=True)
    except Exception as error:
        report.update(status="failed", failure=f"{type(error).__name__}: {error}", traceback=traceback.format_exc())
        raise
    finally:
        if node:
            node.stop()
            node.client.close()
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


def acceptance(options):
    binary = options.binary.resolve(strict=True)
    module = options.crypto_module.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("node-acceptance-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    fixture = fixture_wrapper(run, "fixture-manager", "--fixture-manager")
    require(module.with_name("vibeface_secure_bg.wasm").read_bytes().startswith(b"\x00asm"), "packaged browser WASM is available")
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    helper.chmod(0o600)
    nodes = []
    report = {"status": "running", "evidence": [], "provider_calls": 0, "binary_sha256": file_sha256(binary),
              "coverage": "real Node, Noise, isolated Relay and WASM client; fixture SDK",
              "not_checked": ["paid model output and SDK reasoning", "real LAN/public ingress", "rendered browser UI",
                              "physical phone, Mac mini and PVE deployment", "local Noise key enforcement through WebSocket"]}
    capture = recovery_client = phone_client = None

    def passed(description, kind="real_node_processes", **evidence):
        report["evidence"].append({"check": description, "kind": kind, **evidence})
        print("PASS " + description, flush=True)

    try:
        verify_direct_start(run, binary)
        for name in ("A", "B", "C", "R", "OTHER-NETWORK"):
            node = Node(run, name, binary, workspace, fixture, helper, module, relay_only=name == "R")
            node.configure(codex_bin=sys.executable, timeout_secs=600)
            nodes.append(node)
        a, b, c, relay, outsider = nodes
        verify_incompatible_data(run, binary, workspace, fixture, helper, module)
        passed("direct startup and incompatible data rejection preserve the existing identity, config and data")
        manager = fixture_manager_settings(run, workspace)
        for node in (a, b):
            node.configure(manager=manager, daily_decision_limit=100)
        capture = RelayCapture(relay.port)
        relay.configure(allowed_origins=[capture.url], network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False, "relay_enabled": True})
        relay.start()
        require(relay.client.command("network_probe", kind="peer_id_contract", url=relay.url) == {
            "old_node_id_rejected": True, "peer_id_accepted": True}, "native relay protocol accepts Peer IDs and rejects the removed field")
        relay_config = json.loads((relay.directory / "config.json").read_text())
        require(relay_config["roles"] == ["relay"] and relay_config["manager"] is None and relay_config["roots"] == [],
                "relay-only launch grants no Agent, project root or personal membership")
        for node in (a, b, c):
            node.configure(network={"bootstrap_urls": [capture.url.replace("http:", "ws:") + "/api/network/ws"],
                                    "advertise_urls": [], "mdns": False, "relay_enabled": False})
        a.start()
        verify_cli_status(a, True)
        a.stop()
        verify_cli_status(a, False)
        a.start()
        a.client.connect(a.peer_id)
        a.client.call("rpc", {"method": "nodes"}, status=401)
        a.client.call("directory", status=401)
        for path in ("/api/register", "/api/login", "/api/rpc"):
            a.client.request(path, {}, status=404)
        network = a.bootstrap()
        verify_web_assets(a, module)
        verify_web_assets(relay, module)
        verify_channel_security(a)
        verify_maintenance(a)
        verify_local_access(a)
        passed("packaged Web assets, Noise replay protection, maintenance and the revocable independent local browser entry")
        verify_browser_origins(a, [f"http://localhost:{a.port}", f"http://0.0.0.0:{a.port}"])
        a.stop()
        with capture.lock:
            after_connection = capture.next_connection
        origins = [f"http://192.0.2.10:{a.port}", "https://node.example"]
        a.configure(allowed_origins=origins, allow_insecure_lan=True)
        a.start()
        a.client.connect()
        a.client.authenticate()
        verify_browser_origins(a, origins)
        b.pair(a, network)
        report["relay_readiness"] = wait_for_relay_target(a, capture, after_connection)
        c.pair(a, network, entry_url=capture.url)
        for node in (b, c):
            node.start()
            node.client.connect()
            wait_for(node.name + " client authorization", lambda node=node: node.client.authenticate())
        wait_for("three personal Nodes are reachable through the pure relay", lambda: all(
            any(row["peer_id"] == node.peer_id and row["online"] for row in a.client.rpc("nodes")["nodes"]) for node in (b, c)))
        phone_client = Client(capture.url, helper, module)
        phone_client.pair_client(a.client, network, "phone through pure relay")
        phone_client.url = capture.url
        phone_client.connect(a.peer_id, relay=True)
        phone_client.authenticate(network)
        require(phone_client.call("info")["peer_id"] == a.peer_id, "relay client authenticates the pinned personal Node")
        with sqlite3.connect(f"file:{relay.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
            require(connection.execute("SELECT COUNT(*) FROM networks").fetchone()[0] == 0,
                    "public relay remains outside every personal network")
        outsider.start()
        other_network = outsider.bootstrap()
        other = outsider.client
        require(len({a.peer_id, b.peer_id, c.peer_id}) == 3, "Nodes have distinct stable Peer IDs")
        require(len({node.client.command("identity")["subject_id"] for node in (a, b, c)}) == 3,
                "each browser installation owns an independent identity")
        require(c.client.network_id == b.client.network_id == network and other_network != network,
                "paired clients share one network and the outsider retains its independent network")
        report["topology"] = {"network_id": network, "peers": [node.peer_id for node in (a, b, c)],
                              "relay_peer_id": relay.peer_id, "relay_network_id": None}
        other.command("connect", url=a.url, peer_id=a.peer_id, connection="foreign")
        other.call("authenticate", {"network_id": network}, connection="foreign", status=401)
        other.call("rpc", {"method": "nodes"}, connection="foreign", status=401)
        conversation = "acceptance-conversation"
        a.client.rpc("create_conversation", conversation_id=conversation, title="Failover acceptance")
        require(a.client.rpc("set_autonomy", conversation_id=conversation, mode="report", expected_version=0)["autonomy"]["version"] == 1,
                "the user's explicit conversation autonomy grant starts a version chain")
        a.client.rpc("set_autonomy", conversation_id=conversation, mode="continue", expected_version=0, status=400)
        text = "Keep this context after A disconnects."
        initial = a.client.rpc("message", conversation_id=conversation, message_id="message-original", text=text)
        require(a.client.rpc("message", conversation_id=conversation, message_id="message-original", text=text)["trigger"] == initial["trigger"],
                "same user message retries retain their original trigger")
        a.client.rpc("message", conversation_id=conversation, message_id="message-original", text="changed", status=400)
        other.rpc("conversation", conversation_id=conversation, status=400)
        require(other.rpc("conversations")["conversations"] == [], "another network cannot list private conversation history")
        wait_for("C imports the exact private history", lambda: shared_history(c.client, conversation))
        require(text.encode() not in b"".join(capture.payloads) and not capture.errors,
                "public relay never receives the private conversation as plaintext")
        with sqlite3.connect(f"file:{relay.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
            require(connection.execute("SELECT COUNT(*) FROM events").fetchone()[0] == 0, "pure relay stores no private event replica")
        passed("independent WASM client pairs through an isolated Web and ciphertext Relay to its pinned personal Node")

        directory = workspace / "delayed-host-task"
        directory.mkdir()
        gate = directory / "release"
        receipt = a.client.rpc("message", conversation_id=conversation, message_id="delegate-delayed",
            text=json.dumps({"chat_agent": True, "label": "source-delayed", "dispatch": {
                "label": "delayed", "target_node": b.peer_id, "host_cwd": str(directory),
                "host_command": "printf once >> count.txt", "gate": str(gate), "result_size": 50000}}))
        task = wait_for("Agent delegation is durable", lambda: agent_tool_result(run, conversation, "task-delayed"))
        wait_for("B performs the real host effect", lambda: (directory / "count.txt").exists())
        running = wait_for("B keeps its Agent task Running", lambda:
            row if (row := scheduler_record(b, network, task["task_ref"])) and row["state"] == "running" else None)
        a.stop()
        c.stop()
        c.start()
        c.client.connect()
        c.client.authenticate()
        c.client.call("subscribe")
        gate.touch()
        wait_for("B finishes without its source Node", lambda: (scheduler_record(b, network, task["task_ref"]) or {}).get("state") == "completed")
        wait_for("fresh C observes the original target's signed completion", lambda:
            (row := agent_task_snapshot(c.client, conversation, task["task_ref"])) and row["state"] == "completed")
        notification = "manager:" + running["attempt"]["id"] + ":completed"
        phone = c.client.command("identity")["subject_id"]
        laptop = b.client.command("identity")["subject_id"]
        wait_for("C receives B's durable final notification", lambda: any(row["notification_id"] == notification
                 for row in c.client.rpc("notifications", client_id=phone)["notifications"]))
        require((directory / "count.txt").read_text() == "once" and len(invocations(run, "delayed")) == 1,
                "source loss and entry restart never repeat a target-host side effect")
        wait_for("C imports the complete result attachment", lambda: verify_blobs(c, network) >= 1)
        count = verify_snapshot(c.client.rpc("export", conversation_id=conversation))
        autonomy = c.client.rpc("conversation", conversation_id=conversation)["autonomy"]
        require((autonomy["mode"], autonomy["version"]) == ("report", 1), "Agent completion never changes the user's conversation autonomy")
        c.client.rpc("notifications", client_id=laptop, status=403)
        c.client.rpc("ack", notification_id=notification, client_id=laptop, status=403)
        c.client.rpc("ack", notification_id=notification, client_id=phone)
        require(all(row["notification_id"] != notification for row in c.client.rpc("notifications", client_id=phone)["notifications"]),
                "notification ACK affects only the current client")
        require(any(row["notification_id"] == notification for row in b.client.rpc("notifications", client_id=laptop)["notifications"]),
                "another client's notification delivery is retained")
        c.stop()
        c.start()
        c.client.connect()
        c.client.authenticate()
        require(all(row["notification_id"] != notification for row in c.client.rpc("notifications", client_id=phone)["notifications"]),
                "per-client notification ACK survives reconnect")
        a.start()
        a.client.connect()
        a.client.authenticate()
        wait_for("returning source receives its original target result", lambda:
            (row := agent_task_snapshot(a.client, conversation, task["task_ref"])) and row["state"] == "completed")
        passed("source A goes offline; fresh C keeps B's one result, exact attachments and per-client durable notifications",
               task_ref=task["task_ref"], event_count=count)
        chat_agent_checks(run, workspace, a, b, c, network, passed)
        if options.skip_codex:
            report["not_checked"].append("installed Codex native transport (--skip-codex)")
        else:
            verify_real_codex_transport(run, workspace, options.codex_bin)
            passed("installed App Server opens and reads an exact native thread without a model turn", "real_codex_transport")
        before = len(invocations(run))
        proposal = a.client.call("prepare_revoke", {"subject_id": c.peer_id})
        proposal["event"]["signature"] = a.client.command("sign", **proposal["signing"])
        a.client.call("revoke", {"event": proposal["event"]})
        wait_for("B installs C's revocation", lambda: any(row["peer_id"] == c.peer_id and row["revoked"] for row in b.client.rpc("nodes")["nodes"]))
        require(not any(row["peer_id"] == c.peer_id and row["online"] for row in a.client.rpc("nodes")["nodes"]),
                "known revoked Nodes lose usable remote routing")
        recovery_client = Client(a.url, helper, module)
        backup = verify_recovery(a, recovery_client, [a, b, c], network)
        for name in ("FRESH-EPOCH-2", "FRESH-EPOCH-3"):
            fresh = Node(run, name, binary, workspace, fixture, helper, module)
            nodes.append(fresh)
            fresh.configure(roles=["gateway", "replica"], codex_bin=sys.executable)
            backup = recover_fresh_node(fresh, backup, a.backup_password)
        require(backup["recovery_epoch"] == 4 and len(invocations(run)) == before,
                "higher recovery epochs restore independent management without replaying any host work")
        passed("revocation and encrypted identity recovery preserve history and never repeat a host effect")
        require(file_sha256(binary) == report["binary_sha256"], "one immutable binary completes acceptance")
        report.update(status="passed", fixture_invocations=len(invocations(run)), event_count=count)
    except Exception as error:
        report.update(status="failed", failure=f"{type(error).__name__}: {error}", traceback=traceback.format_exc())
        raise
    finally:
        for client in (recovery_client, phone_client):
            if client:
                client.close()
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        stop_fixture_processes(run)
        if capture:
            capture.close()
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)

def history_sync_fields(line):
    fields = re.findall(r'\b(network|target|source|origin|request_id|scope|state|processed|changed|missing_events|missing_blobs|conflicts|hash|size|offset|limit|committed_offset|part_length|returned_bytes|phase|peer|link_id|outbound|route|flight_id|jobs)=("[^"]*"|[^\s]+)', line)
    return {key: value[1:-1] if value.startswith('"') else value for key, value in fields}


def network_reliability_closure_progress(run, binary, workspace, fixture, helper, module, report, nodes,
                                         event_count=100001):
    repository = Path(__file__).resolve().parents[1]
    case = {"status": "running", "history_events": event_count, "batch_budget": 1024,
            "observations": [], "batches": [], "scope_flight_completions": [], "complete": False,
            "source_isolation": "maintenance leases block background sync and allow authenticated peer reads"}
    report["cases"]["closure_progress"] = case
    leases = []
    try:
        for name in ("R", "A", "B", "C", "D"):
            node = Node(run, name, binary, workspace, fixture, helper, module)
            node.configure(manager=None, codex_socket=str(run / "unused-codex.sock"))
            nodes.append(node)
        relay, head_source, parent_source, blob_source, receiver = nodes
        relay.configure(roles=["relay"], network={"bootstrap_urls": [], "advertise_urls": [],
                                               "mdns": False, "relay_enabled": True})
        relay.start()
        for node in nodes[1:]:
            node.configure(network={"bootstrap_urls": [relay.url.replace("http:", "ws:") + "/api/network/ws"],
                                    "advertise_urls": [node.url.replace("http:", "ws:") + "/api/network/ws"],
                                    "mdns": False, "relay_enabled": False})
        head_source.start()
        network = head_source.bootstrap()
        for node in (parent_source, blob_source, receiver):
            node.pair(head_source, network)
            node.configure(roles=["gateway", "replica"] + (["scheduler"] if node is receiver else []),
                           manager=fixture_manager_settings(run, workspace) if node is receiver else None,
                           codex_bin=sys.executable)
            node.start(env={**os.environ, "RUST_LOG": "vibeface::runtime=trace", "NO_COLOR": "1"}
                       if node is receiver else None)
            node.client.connect(node.peer_id)
            wait_for(f"{node.name} client authorization", lambda node=node: node.client.authenticate())
        expected_membership = head_source.client.call("directory")["state"]
        wait_for("all source and receiver memberships converge before maintenance", lambda: all(
            node.client.call("directory")["state"] == expected_membership for node in nodes[1:]), timeout=90)
        for node in nodes[1:]:
            def acquire(node=node):
                stream, response = maintenance(node, "acquire")
                if response["status"] == "ready":
                    return stream
                stream.close()
                require(response["status"] == "busy", f"{node.name} maintenance can be retried safely")
                return None
            leases.append(wait_for(f"{node.name} fixture isolation lease", acquire))
        scope = "net1-continuation-history"
        payload = b"NET-1 closure attachment supplied by replica C\n"
        blob = {"hash": hashlib.sha256(payload).hexdigest(), "size": len(payload)}
        event = {"version": 1, "network_id": network, "scope_id": scope, "author": head_source.peer_id,
                 "sequence": 1, "previous": None, "parents": [], "created_at_ms": 1,
                 "body": {}, "blobs": []}
        chain_path = run / "signed-history.ndjson"
        signed = subprocess.run(["node", str(repository / "scripts/crypto-client.mjs"), "--module", str(module)],
                                input=json.dumps({"op": "fixture_chain", "identity_file": str(head_source.directory / "identity.json"),
                                                  "event": event, "blob": blob, "count": event_count,
                                                  "output_file": str(chain_path)}) + "\n",
                                capture_output=True, text=True, timeout=180)
        require(signed.returncode == 0, "one temporary signer generates the bounded history without exposing its key")
        result = json.loads(signed.stdout)
        require(result.get("ok"), "streamed history signing succeeds")
        chain = result["result"]
        require(chain["count"] == event_count and file_sha256(chain_path) == chain["envelopes_sha256"],
                "private streamed history has the exact requested count and digest")
        case.update({"network_id": network, "scope_id": scope, "head": chain["head"], "blob": blob,
                     "expected_envelopes_sha256": chain["envelopes_sha256"], "fixture_artifact": str(chain_path),
                     "sources": {node.name: node.peer_id for node in nodes[1:-1]},
                     "receiver_log": str(receiver.directory / "process.log")})
        sequences = {}
        with sqlite3.connect(head_source.directory / "node.sqlite3", timeout=10) as heads, \
                sqlite3.connect(parent_source.directory / "node.sqlite3", timeout=10) as parents:
            with chain_path.open() as source:
                for line in source:
                    envelope = json.loads(line)
                    body = json.loads(envelope["canonical_body"])
                    sequence = body["sequence"]
                    sequences[envelope["hash"]] = sequence
                    connection = heads if sequence == event_count else parents
                    connection.execute("INSERT INTO events(network_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,?)",
                                       (network, envelope["hash"], scope, head_source.peer_id, sequence,
                                        json.dumps(envelope, separators=(",", ":")), "pending" if sequence == event_count else "ready"))
        require(len(sequences) == event_count and sequences[chain["head"]] == event_count,
                "seeded history contains every unique signed event exactly once")
        database = receiver.directory / "node.sqlite3"
        source_events = {head_source.peer_id: set(), parent_source.peer_id: set(), blob_source.peer_id: set()}
        source_blobs = {node.peer_id: set() for node in nodes[1:-1]}
        log_path = receiver.directory / "process.log"
        log_offset = log_path.stat().st_size
        log_tail = b""
        scope_flights = set()

        def read_progress():
            nonlocal log_offset, log_tail
            with log_path.open("rb") as log:
                log.seek(log_offset)
                chunk = log.read()
                log_offset = log.tell()
            lines = (log_tail + chunk).split(b"\n")
            log_tail = lines.pop()
            for raw in lines:
                line = re.sub(r"\x1b\[[0-9;]*m", "", raw.decode(errors="replace"))
                fields = history_sync_fields(line)
                if "history sync flight" in line:
                    if fields.get("network") == network and fields.get("scope") == scope:
                        scope_flights.add(fields["flight_id"])
                    if "history sync flight yielded" in line and fields.get("scope") == scope:
                        scope_flights.add(fields["flight_id"])
                        case["batches"].append(fields)
                    if "history sync flight finished" in line and fields.get("flight_id") in scope_flights:
                        case["scope_flight_completions"].append(fields)
                if fields.get("network") != network:
                    continue
                if "history event fetched" in line:
                    target, digest = fields["target"], fields["hash"]
                    require(target in source_events and digest in sequences, "events come from a known authorized fixture source")
                    require((target == head_source.peer_id and digest == chain["head"])
                            or (target == parent_source.peer_id and digest != chain["head"]),
                            "each event is fetched from its actual fixed source distribution")
                    source_events[target].add(digest)
                elif "history blob committed" in line and int(fields.get("committed_offset", "0")) == blob["size"]:
                    require(fields["source"] == blob_source.peer_id and fields["hash"] == blob["hash"],
                            "the complete attachment is fetched from replica C")
                    source_blobs[fields["source"]].add(fields["hash"])
            with sqlite3.connect(database, timeout=10) as connection:
                count, ready, pending, invalid = connection.execute(
                    "SELECT COUNT(*),COALESCE(SUM(status='ready'),0),COALESCE(SUM(status='pending'),0),COALESCE(SUM(status IN ('invalid','conflict')),0) FROM events WHERE network_id=? AND scope_id=?",
                    (network, scope)).fetchone()
                has_head = bool(connection.execute("SELECT 1 FROM events WHERE network_id=? AND hash=?", (network, chain["head"])).fetchone())
                has_blob = bool(connection.execute("SELECT 1 FROM blobs WHERE network_id=? AND hash=?", (network, blob["hash"])).fetchone())
                has_conversation = bool(connection.execute("SELECT 1 FROM conversations WHERE network_id=? AND conversation_id=?", (network, scope)).fetchone())
                continuations = []
                for key, value in connection.execute("SELECT key,value FROM local_values WHERE network_id=? AND key LIKE 'net1:v1:closure:%'", (network,)):
                    progress = json.loads(value)
                    if progress["scope"] == scope:
                        continuations.append({"key": key, "heads": progress["heads"], "next_events": progress["events"],
                                              "next_sequences": [sequences.get(digest) for digest in progress["events"]],
                                              "missing_blobs": len(progress["blobs"]),
                                              "frontier_generation": progress["frontier_generation"]})
            observation = {"elapsed_seconds": time.monotonic() - started, "persisted_events": count,
                           "remaining_history": event_count - count, "ready_events": ready,
                           "pending_events": pending, "invalid_events": invalid, "has_head": has_head,
                           "has_conversation": has_conversation, "complete_blob": has_blob, "continuations": continuations}
            previous = case["observations"][-1] if case["observations"] else None
            require(previous is None or count >= previous["persisted_events"], "remaining history decreases monotonically")
            require(count <= event_count and invalid == 0, "receiver stores only valid fixture history")
            if previous is None or any(observation[key] != previous[key] for key in observation if key != "elapsed_seconds"):
                case["observations"].append(observation)
                private_json(run / "report.json", report)
                print(f"PROGRESS NET-1 Lane 3 persisted={count} remaining={event_count-count} pending={pending}", flush=True)
            return observation

        started = time.monotonic()
        read_progress()
        leases.pop().close()
        blocked_message = None
        while time.monotonic() - started < 3600:
            observation = read_progress()
            if blocked_message is None and observation["has_head"] and observation["has_conversation"] and observation["pending_events"]:
                message_id = "net1-incomplete-history-must-not-run"
                rejection = receiver.client.rpc("message", conversation_id=scope, message_id=message_id,
                    scheduler_node=receiver.peer_id, basis_heads=[chain["head"]], report_results=False,
                    text=json.dumps({"phase1": True, "label": "net1-incomplete",
                                     "host_command": "printf forbidden >> incomplete-effect", "host_cwd": str(workspace)}), status=400)
                require("context is incomplete; wait for synchronization" in str(rejection.get("error")),
                        "pending signed history is rejected before SDK work is admitted")
                accepted = len(scheduler_records(receiver, network))
                require(accepted == 0 and not invocations(run) and not (workspace / "incomplete-effect").exists(),
                        "incomplete history never admits a Scheduler job or starts the host fixture")
                blocked_message = {"message_id": message_id, "target_node": receiver.peer_id,
                                   "basis_heads": [chain["head"]], "rejection": rejection["error"],
                                   "accepted_jobs": accepted, "fixture_invocations": 0}
                case["pending_message"] = blocked_message
                with sqlite3.connect(blob_source.directory / "node.sqlite3", timeout=10) as connection:
                    connection.execute("INSERT INTO blobs(network_id,hash,size,content) VALUES(?,?,?,?)",
                                       (network, blob["hash"], blob["size"], payload))
                case["blob_supply"] = "replica C supplies the missing attachment after the incomplete-message rejection"
            complete = bool(case["scope_flight_completions"]) and any(
                progress["heads"] == [chain["head"]] and not progress["next_events"]
                and progress["missing_blobs"] == 0 and progress["frontier_generation"] is not None
                for progress in observation["continuations"])
            if complete and observation["persisted_events"] == event_count and observation["ready_events"] == event_count and observation["complete_blob"]:
                case["complete"] = True
                break
            require(all(node.process.poll() is None for node in nodes), "all isolated Node processes stay alive")
            time.sleep(0.5)
        require(case["complete"], "the full history reaches a complete signed frontier within the observation bound")
        require(blocked_message is not None, "pending history has an actual rejected message admission")
        require(event_count <= 1024 or (len(case["batches"]) >= 2
                and any(int(batch.get("processed", "0")) > 0 for batch in case["batches"])
                and all(int(batch.get("processed", "0")) <= 1024 for batch in case["batches"])),
                "large history yields across real bounded coordinator steps before completing its signed frontier")
        digest = hashlib.sha256()
        with sqlite3.connect(database, timeout=10) as connection:
            for envelope, in connection.execute("SELECT envelope FROM events WHERE network_id=? AND scope_id=? ORDER BY sequence", (network, scope)):
                parsed = json.loads(envelope)
                require(hashlib.sha256(parsed["canonical_body"].encode()).hexdigest() == parsed["hash"],
                        "every persisted event matches its signed content hash")
                digest.update((json.dumps(parsed, separators=(",", ":")) + "\n").encode())
            stored_blob = connection.execute("SELECT size,content FROM blobs WHERE network_id=? AND hash=?", (network, blob["hash"])).fetchone()
        require(digest.hexdigest() == chain["envelopes_sha256"], "receiver preserves the entire exact signed event history")
        require(stored_blob and stored_blob[0] == len(stored_blob[1]) == blob["size"]
                and hashlib.sha256(stored_blob[1]).hexdigest() == blob["hash"], "receiver attachment size and final hash match")
        for node, expected_count in ((head_source, 1), (parent_source, event_count - 1), (blob_source, 0)):
            with sqlite3.connect(node.directory / "node.sqlite3", timeout=10) as connection:
                count = connection.execute("SELECT COUNT(*) FROM events WHERE network_id=? AND scope_id=?", (network, scope)).fetchone()[0]
                blobs = connection.execute("SELECT COUNT(*) FROM blobs WHERE network_id=?", (network,)).fetchone()[0]
            require(count == expected_count and blobs == int(node is blob_source), "source distribution remains fixed throughout transfer")
        require(source_events[head_source.peer_id] == {chain["head"]}
                and len(source_events[parent_source.peer_id]) == event_count - 1
                and source_blobs[blob_source.peer_id] == {blob["hash"]}, "real object-source logs cover every supplied event and attachment")
        require(not invocations(run), "closure continuation and rejected incomplete message never start host work")
        verify_no_model_budget(nodes)
        require(file_sha256(binary) == report["binary_sha256"], "Lane 3 binary remains unchanged")
        case.update({"status": "passed", "actual_envelopes_sha256": digest.hexdigest(), "attachment_hash_verified": True,
                     "source_events_fetched": {node.name: len(source_events[node.peer_id]) for node in nodes[1:-1]},
                     "source_blobs_fetched": {node.name: len(source_blobs[node.peer_id]) for node in nodes[1:-1]},
                     "source_distribution_preserved": True})
        report["fixture_invocations"] = 0
        report["status"] = "passed"
        print(f"PASS NET-1 Lane 3 report: {run / 'report.json'}", flush=True)
    finally:
        for node in reversed(nodes):
            node.stop()
        for lease in leases:
            lease.close()


def seed_net1_attachment(source, network, conversation, size, module):
    repository = Path(__file__).resolve().parents[1]
    payload = b"n" * size
    blob = {"hash": hashlib.sha256(payload).hexdigest(), "size": len(payload)}
    with sqlite3.connect(source.directory / "node.sqlite3", timeout=10) as connection:
        previous, sequence = connection.execute(
            "SELECT hash,sequence FROM events WHERE network_id=? AND scope_id=? AND author=? ORDER BY sequence DESC LIMIT 1",
            (network, conversation, source.peer_id)).fetchone()
        event = {"version": 1, "network_id": network, "scope_id": conversation, "author": source.peer_id,
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
                           (network, envelope["hash"], conversation, source.peer_id, sequence + 1,
                            json.dumps(envelope, separators=(",", ":"))))
    del payload
    return blob, envelope


def network_reliability_blob_resume(run, binary, workspace, fixture, helper, module, report, nodes):
    """Exercise actual range reads across transport loss and a killed receiver."""
    trace_env = {**os.environ, "RUST_LOG": "vibeface::runtime=trace,vibeface::peer=trace,vibeface::network=debug,vibeface::api=debug", "NO_COLOR": "1"}
    case = {"status": "running", "sizes": [], "source_isolation": "source-only PendingActivation blocks background sync; authorized peer reads remain available",
            "route_evidence": "temporary configuration and verified signed directory records constrain devices to one relay; encrypted capture alone does not exclude every bypass",
            "physical_sqlite_reads": "not measured by this process lane; independent instrumented VFS tests are required"}
    report["cases"]["resumable_relay_attachments"] = case
    capture = None

    def read_only(node):
        return sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True, timeout=10)

    def records(node, start=0, *, digest=None):
        path = node.directory / "process.log"
        if not path.exists():
            return []
        result, position = [], start
        with path.open("rb") as stream:
            stream.seek(start)
            for raw in stream:
                if not raw.endswith(b"\n"):
                    break
                end = position + len(raw)
                line = re.sub(r"\x1b\[[0-9;]*m", "", raw.decode(errors="replace"))
                fields = history_sync_fields(line)
                if digest is None or fields.get("hash") == digest:
                    stamp = re.search(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z", line)
                    result.append({"log_offset": position, "log_end": end,
                                   "wall_time": datetime.fromisoformat(stamp.group().replace("Z", "+00:00")).timestamp() if stamp else None,
                                   "fields": fields, "message": line.strip()})
                position = end
        return result

    def boundary(node):
        path = node.directory / "process.log"
        return path.stat().st_size if path.exists() else 0

    try:
        for name in ("R", "A", "B"):
            node = Node(run, name, binary, workspace, fixture, helper, module)
            node.configure(manager=None, codex_socket=str(run / "unavailable-codex.sock"))
            nodes.append(node)
        relay, source, receiver = nodes
        relay.configure(roles=["relay"], network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False, "relay_enabled": True})
        capture = RelayCapture(relay.port, retain_payloads=False)
        relay.configure(allowed_origins=[capture.url])
        relay.start(env=trace_env)
        relay_url = capture.url.replace("http:", "ws:") + "/api/network/ws"
        device_network = {"bootstrap_urls": [relay_url], "advertise_urls": [], "mdns": False, "relay_enabled": False}
        for node in (source, receiver):
            node.configure(network=device_network)
        source.start(env=trace_env)
        network = source.bootstrap()
        receiver.pair(source, network, entry_url=capture.url)
        for node in (source, receiver):
            node.configure(roles=["gateway", "replica"])
        receiver.start(env=trace_env)
        receiver.client.connect(receiver.peer_id)
        wait_for("Lane 2 receiver authorization", lambda: receiver.client.authenticate())
        expected_membership = source.client.call("directory")["state"]
        wait_for("Lane 2 memberships converge", lambda: receiver.client.call("directory")["state"] == expected_membership)
        scope = "net1-resumable-attachments"
        source.client.rpc("create_conversation", conversation_id=scope, title="NET-1 resumable attachments")
        wait_for("Lane 2 initial signed conversation arrives", lambda: len(receiver.client.rpc("export", conversation_id=scope)["events"]) == 1)
        source.stop()
        private_json(source.directory / "update-pending.json", {"protocol": 1})
        source.start(env=trace_env)
        probe, health = maintenance(source, "probe")
        probe.close()
        require(health["status"] == "healthy", "source PendingActivation preserves its read listener")
        receiver.stop()
        case.update({"network_id": network, "scope_id": scope, "relay_url": relay_url,
                     "nodes": {node.name: node.peer_id for node in nodes},
                     "process_logs": {node.name: str(node.directory / "process.log") for node in nodes},
                     "temporary_configurations": {node.name: {key: json.loads((node.directory / "config.json").read_text())[key] for key in ("roles", "network", "manager", "codex_socket")} for node in nodes}})
        for node in (source, receiver):
            require(json.loads((node.directory / "config.json").read_text())["network"] == device_network,
                    "A/B retain only the isolated relay and no direct discovery")
        with read_only(receiver) as connection:
            case["receiver_sqlite"] = {"journal_mode": connection.execute("PRAGMA journal_mode").fetchone()[0],
                                       "observer_synchronous": connection.execute("PRAGMA synchronous").fetchone()[0],
                                       "synchronous_note": "observer connection pragma; not a measurement of the writer connection"}

        for size in (32 * 1024 * 1024, 64 * 1024 * 1024):
            blob, envelope = seed_net1_attachment(source, network, scope, size, module)
            digest = blob["hash"]
            key = f"net1:v1:blob:{digest}:{size}"
            part_name = hashlib.sha256(json.dumps([network, digest, size], separators=(",", ":")).encode()).hexdigest() + ".part"
            staging = receiver.directory / "net1-blob-staging"
            part = staging / part_name
            item = {"status": "running", "blob": blob, "event_hash": envelope["hash"], "manifest_key": key,
                    "part": str(part), "faults": [], "observations": []}
            case["sizes"].append(item)
            start_b, start_a, start_r = boundary(receiver), boundary(source), boundary(relay)
            started = time.monotonic()
            initial_bytes, initial_frames = capture.payload_bytes, capture.frame_count

            def snapshot():
                with read_only(receiver) as connection:
                    row = connection.execute("SELECT value FROM local_values WHERE network_id=? AND key=?", (network, key)).fetchone()
                    complete = connection.execute("SELECT size,length(content) FROM blobs WHERE network_id=? AND hash=?", (network, digest)).fetchone()
                    status = connection.execute("SELECT status FROM events WHERE network_id=? AND hash=?", (network, envelope["hash"])).fetchone()
                    manifests = connection.execute("SELECT value FROM local_values WHERE key>=? AND key<?", ("net1:v1:blob:", "net1:v1:blob;")).fetchall()
                manifest = json.loads(row[0]) if row else None
                if manifest:
                    require(manifest["version"] == 1 and manifest["blob"] == blob, "durable manifest binds the exact signed object")
                offset = manifest["phase"].get("committed_offset") if manifest and manifest["phase"]["state"] == "receiving" else None
                if offset is not None:
                    require(0 <= offset <= size and manifest["phase"]["source_peer_id"] == source.peer_id, "progress stays within this authorized source object")
                files = list(staging.iterdir()) if staging.exists() else []
                reserved = sum(json.loads(value)["blob"]["size"] for value, in manifests)
                actual_bytes = sum(path.lstat().st_size for path in files)
                require(len(manifests) <= 64 and len(files) <= 64 and reserved <= 4 * 64 * 1024 * 1024,
                        "all-network staging manifest/object reservation stays within the fixed budget")
                require(actual_bytes <= 4 * 64 * 1024 * 1024, "observed staging body bytes remain within budget")
                result = {"elapsed_seconds": time.monotonic() - started, "manifest": manifest, "committed_offset": offset,
                          "part_length": part.lstat().st_size if part.exists() else None,
                          "complete_blob": list(complete) if complete else None, "event_status": status[0] if status else None,
                          "staging_objects": len(files), "manifest_objects": len(manifests), "reserved_bytes": reserved, "staging_bytes": actual_bytes}
                if not complete and offset is not None and offset > 0:
                    require(status and status[0] == "pending", "incomplete attachment envelope remains pending outside the complete blob table")
                previous = item["observations"][-1] if item["observations"] else None
                if previous is None or any(result[k] != previous[k] for k in result if k != "elapsed_seconds"):
                    item["observations"].append(result)
                return result

            def positive_progress(minimum, *, ceiling=size):
                value = snapshot()
                require(not value["complete_blob"], "fault is injected before complete publication")
                offset = value["committed_offset"]
                if offset is not None and offset >= minimum:
                    require(offset < ceiling, "fault occurs below the fixed transfer boundary")
                    return value
                return None

            def resumed_request(known, source_cursor, *, expected=None, receiver_cursor=None):
                b_logs = records(receiver, start_b, digest=digest)
                queued = {entry["fields"]["request_id"]: entry for entry in b_logs if "peer blob request queued" in entry["message"]}
                actual = [entry for entry in records(source, source_cursor, digest=digest)
                          if "peer blob range served" in entry["message"] and entry["fields"].get("request_id") not in known]
                if not actual:
                    return None
                served = actual[0]
                if served["fields"].get("request_id") not in queued:
                    return None
                request_entry = queued[served["fields"]["request_id"]]
                fields = request_entry["fields"]
                require(fields.get("network") == network and fields.get("target") == source.peer_id,
                        "queued request keeps the original network and source target")
                require(served["fields"].get("network") == network and served["fields"].get("origin") == receiver.peer_id,
                        "actual source range read authenticates the original receiver")
                preceding = [entry for entry in b_logs if entry["log_offset"] < request_entry["log_offset"]
                             and ("history blob committed" in entry["message"] or "history blob opened" in entry["message"])]
                require(preceding, "a durable open or commit precedes the resumed request")
                durable = int(preceding[-1]["fields"]["committed_offset"])
                offset = int(fields["offset"])
                require(offset == durable > 0 and offset == int(served["fields"]["offset"]),
                        "the first actual resumed read equals its preceding positive durable offset")
                if expected is not None:
                    require(offset == expected, "restart first actual read equals the stable crash manifest offset")
                if receiver_cursor is not None:
                    opened = [entry for entry in b_logs if entry["log_offset"] >= receiver_cursor and "history blob opened" in entry["message"]]
                    require(opened and int(opened[0]["fields"]["committed_offset"]) == expected
                            and int(opened[0]["fields"]["part_length"]) == expected,
                            "restart opened log proves committed-prefix preservation and uncommitted-tail truncation")
                return {"receiver_queued": request_entry, "source_served": served, "preceding_durable": preceding[-1],
                        "ignored_old_request_ids": sorted(known), "opened_after_restart": opened[0] if receiver_cursor is not None else None}

            receiver.start(env=trace_env)
            item["initial_receiver_pid"] = receiver.process.pid
            wait_for(f"{size} attachment reaches the first positive committed prefix", lambda: positive_progress(2 * 1024 * 1024), timeout=45)
            first = {"kind": "manual_transport_cut", "receiver_log_cursor_before_cut": boundary(receiver),
                     "source_log_cursor_before_cut": boundary(source), "cut": capture.pause()}
            first["after_cut"] = snapshot()
            require(first["after_cut"]["committed_offset"] > 0 and not first["after_cut"]["complete_blob"], "manual cut preserves unfinished positive progress")
            first["known_request_ids"] = [entry["fields"]["request_id"] for entry in records(receiver, start_b, digest=digest) if "peer blob request queued" in entry["message"]]
            first["resume"] = capture.resume()
            item["faults"].append(first)
            first["first_actual_resumed_request"] = wait_for("first same-process actual source range read", lambda: resumed_request(set(first["known_request_ids"]), first["source_log_cursor_before_cut"]), timeout=45)
            resumed_at = int(first["first_actual_resumed_request"]["receiver_queued"]["fields"]["offset"])
            wait_for("receiver commits more data before SIGKILL", lambda: positive_progress(resumed_at + 1024 * 1024, ceiling=8 * 1024 * 1024 if size == 64 * 1024 * 1024 else size), timeout=45)
            crash = {"kind": "receiver_process_group_SIGKILL", "old_pid": receiver.process.pid,
                     "receiver_log_cursor_before_cut": boundary(receiver), "source_log_cursor_before_cut": boundary(source), "cut": capture.pause()}
            crash["returncode"] = receiver.stop(crash=True)
            require(crash["returncode"] == -signal.SIGKILL, "actual receiver process exits from SIGKILL")
            crash["stable_after_exit"] = snapshot()
            p = crash["stable_after_exit"]["committed_offset"]
            require(p and p < size and (size != 64 * 1024 * 1024 or p < 8 * 1024 * 1024), "crash retains an unfinished positive prefix below the required boundary")
            require(not crash["stable_after_exit"]["complete_blob"] and crash["stable_after_exit"]["event_status"] == "pending", "crashed partial object is pending and unpublished")
            directory_stat, before_stat = staging.lstat(), part.lstat()
            require(stat.S_ISDIR(directory_stat.st_mode) and not stat.S_ISLNK(directory_stat.st_mode)
                    and stat.S_IMODE(directory_stat.st_mode) == 0o700 and directory_stat.st_uid == os.getuid(), "staging directory is private and task-owned")
            require(stat.S_ISREG(before_stat.st_mode) and before_stat.st_nlink == 1 and stat.S_IMODE(before_stat.st_mode) == 0o600
                    and before_stat.st_uid == os.getuid() and before_stat.st_size >= p, "stopped receiver part is a private owned regular file preserving the committed prefix")
            before_hash = file_sha256(part)
            descriptor = os.open(part, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW)
            try:
                opened_stat = os.fstat(descriptor)
                require((opened_stat.st_dev, opened_stat.st_ino) == (before_stat.st_dev, before_stat.st_ino), "tail injection opens the exact inspected owned part")
                require(os.write(descriptor, b"NET1" * 1024) == 4096, "explicit synthetic tail is exactly 4096 bytes")
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
            crash["artificial_uncommitted_tail"] = {"classification": "explicit synthetic recovery state after real SIGKILL; not naturally observed crash bytes",
                "committed_offset": p, "before_length": before_stat.st_size, "before_sha256": before_hash,
                "after_length": part.stat().st_size, "after_sha256": file_sha256(part), "injected_bytes": 4096,
                "mode": oct(stat.S_IMODE(before_stat.st_mode)), "uid": before_stat.st_uid}
            require(crash["artificial_uncommitted_tail"]["after_length"] == before_stat.st_size + 4096,
                    "tail injection changes only the owned stopped part by the declared exact byte count")
            crash["receiver_log_cursor_before_restart"] = boundary(receiver)
            receiver.start(env=trace_env)
            crash["new_pid"] = receiver.process.pid
            require(crash["new_pid"] != crash["old_pid"], "restart creates a distinct receiver process")
            require(receiver.peer_id == case["nodes"]["B"], "restart keeps the original receiver identity")
            crash["known_request_ids"] = [entry["fields"]["request_id"] for entry in records(receiver, start_b, digest=digest) if "peer blob request queued" in entry["message"]]
            crash["relay_log_cursor_before_resume"] = boundary(relay)
            crash["resume"] = capture.resume()
            item["faults"].append(crash)
            crash["first_actual_resumed_request"] = wait_for("first restarted actual source range read", lambda: resumed_request(set(crash["known_request_ids"]), crash["source_log_cursor_before_cut"], expected=p, receiver_cursor=crash["receiver_log_cursor_before_restart"]), timeout=45)
            wait_for(f"{size} attachment finishes across relay circuits", lambda: snapshot()["complete_blob"], timeout=240)
            wait_for("owned manifest and staging part are cleaned", lambda: not snapshot()["manifest"] and not part.exists(), timeout=35)
            cleaned = snapshot()
            require(cleaned["staging_objects"] == cleaned["manifest_objects"] == 0,
                    "all parts and manifests created by this isolated lane are cleaned after publication")
            published = [entry for entry in records(receiver, start_b, digest=digest) if "history blob published" in entry["message"]]
            require(published, "receiver logs actual publication of this exact attachment")
            complete_scope = wait_for("completed attachment scope reports Complete", lambda: [entry for entry in records(receiver, start_b)
                if entry["log_offset"] >= published[-1]["log_end"] and "history scope sync finished" in entry["message"] and entry["fields"].get("network") == network
                and entry["fields"].get("target") == source.peer_id
                and entry["fields"].get("scope") == scope and entry["fields"].get("state") == "Complete"
                and all(entry["fields"].get(field) == "0" for field in ("missing_events", "missing_blobs", "conflicts"))], timeout=35)
            with read_only(receiver) as connection:
                actual_size, content = connection.execute("SELECT size,content FROM blobs WHERE network_id=? AND hash=?", (network, digest)).fetchone()
                actual_envelope, event_status = connection.execute("SELECT envelope,status FROM events WHERE network_id=? AND hash=?", (network, envelope["hash"])).fetchone()
            require(actual_size == len(content) == size and hashlib.sha256(content).hexdigest() == digest, "completed receiver body has the exact signed size and hash")
            require(event_status == "ready" and json.loads(actual_envelope) == envelope, "receiver publishes the original exact signed envelope as ready")
            del content
            b_logs = records(receiver, start_b, digest=digest)
            durable, previous_request, requests = 0, 0, []
            for entry in b_logs:
                fields = entry["fields"]
                if "history blob opened" in entry["message"] or "history blob committed" in entry["message"]:
                    durable = int(fields["committed_offset"])
                elif "peer blob request queued" in entry["message"]:
                    offset = int(fields["offset"])
                    require(offset == durable and offset >= previous_request, "all actual queued range requests preserve durable monotonic progress without restarting at zero")
                    previous_request = offset
                    requests.append(entry)
            require(requests and int(requests[0]["fields"]["offset"]) == 0, "initial actual range request is the offset-zero positive control")
            queued_by_id = {entry["fields"]["request_id"]: entry for entry in requests}
            all_served = [entry for entry in records(source, start_a, digest=digest) if "peer blob range served" in entry["message"]]
            require(all_served, "the source performed actual authorized range reads")
            for served_entry in all_served:
                served_fields = served_entry["fields"]
                queued_entry = queued_by_id.get(served_fields.get("request_id"))
                require(queued_entry is not None and served_fields.get("network") == network
                        and served_fields.get("origin") == receiver.peer_id
                        and served_fields["offset"] == queued_entry["fields"]["offset"]
                        and int(served_fields["size"]) == size
                        and 0 < int(served_fields["returned_bytes"]) <= int(queued_entry["fields"]["limit"]),
                        "every actual source range read matches the complete receiver queued-ID sequence and exact offset")
            budget_logs = [entry for entry in records(relay, crash["relay_log_cursor_before_resume"]) if "relay byte budget exceeded" in entry["message"]]
            controls = [dict(control) for control in capture.controls]
            circuits = []
            for control in controls:
                if control.get("type") != "dial" or control.get("direction") != "to_relay" or control.get("service") != "peer" or control.get("target") not in (source.peer_id, receiver.peer_id):
                    continue
                ready = next((entry for entry in controls if entry["connection"] == control["connection"] and entry.get("type") == "ready" and entry["direction"] == "from_relay"), None)
                traffic = capture.traffic[control["connection"]]
                if ready and traffic["binary_payload_bytes"] >= 65536:
                    circuits.append({"dial": control, "ready": ready, **traffic})
            for fault in item["faults"]:
                served_time = fault["first_actual_resumed_request"]["source_served"]["wall_time"]
                new_circuits = [entry for entry in circuits if entry["generation"] == fault["resume"]["generation"]
                                and entry["dial"]["wall_time"] >= fault["resume"]["wall_time"]
                                and served_time is not None and entry["ready"]["wall_time"] <= served_time]
                require(new_circuits, "each artificial fault resumes only after a fresh data-carrying peer Dial/Ready")
                fault["fresh_data_carrying_peer_circuit"] = new_circuits[0]
            natural = []
            for closed in budget_logs:
                require(closed["wall_time"] is not None, "natural relay budget closure has a real timestamp")
                later_circuits = [entry for entry in circuits if entry["dial"]["wall_time"] > closed["wall_time"]]
                if not later_circuits:
                    continue
                ready_time = later_circuits[0]["ready"]["wall_time"]
                later_requests = [entry for entry in requests if entry["wall_time"] is not None and entry["wall_time"] > ready_time and int(entry["fields"]["offset"]) > 0]
                served = {entry["fields"]["request_id"]: entry for entry in records(source, start_a, digest=digest) if "peer blob range served" in entry["message"]}
                later_requests = [entry for entry in later_requests if entry["fields"]["request_id"] in served
                                  and served[entry["fields"]["request_id"]]["wall_time"] is not None
                                  and served[entry["fields"]["request_id"]]["wall_time"] > ready_time]
                if later_requests:
                    natural.append({"budget_closure": closed, "new_peer_circuit": later_circuits[0],
                                    "first_actual_positive_request_after_new_circuit": {"receiver_queued": later_requests[0], "source_served": served[later_requests[0]["fields"]["request_id"]]}})
            require(size != 64 * 1024 * 1024 or natural, "64 MiB completes after natural relay byte-budget closure and further positive-offset circuits")
            with read_only(source) as connection:
                source_size, source_content = connection.execute("SELECT size,content FROM blobs WHERE network_id=? AND hash=?", (network, digest)).fetchone()
                source_envelope = connection.execute("SELECT envelope FROM events WHERE network_id=? AND hash=?", (network, envelope["hash"])).fetchone()[0]
            require(source_size == len(source_content) == size and hashlib.sha256(source_content).hexdigest() == digest and json.loads(source_envelope) == envelope, "source object and signed envelope remain unchanged")
            del source_content
            with read_only(relay) as connection:
                relay_events = connection.execute("SELECT COUNT(*) FROM events WHERE network_id=?", (network,)).fetchone()[0]
                relay_blobs = connection.execute("SELECT COUNT(*) FROM blobs WHERE network_id=?", (network,)).fetchone()[0]
            require(relay_events == relay_blobs == 0, "the sole relay never stores this personal network's private history or blobs")
            receiver.client.connect(receiver.peer_id)
            wait_for("completed receiver client authorization", lambda: receiver.client.authenticate())
            signed_records = {node.name: wait_for(f"{node.name} signed relay-only directory record", lambda node=node: relay.client.command("network_probe", kind="record", url=relay.url, target=node.peer_id)) for node in (source, receiver)}
            require(all(value["direct_urls"] == [] and value["relay_urls"] == [relay_url] for value in signed_records.values()), "signed A/B directory records offer only this sole relay")
            logical_bytes = sum(int(entry["fields"]["returned_bytes"]) for entry in records(source, start_a, digest=digest) if "peer blob range served" in entry["message"])
            item.update({"status": "passed", "final": snapshot(), "actual_hash": digest, "actual_size": actual_size,
                         "complete_scope": complete_scope[-1], "all_queued_requests": requests, "signed_directory_records": signed_records,
                         "all_source_range_reads": all_served,
                         "queued_without_source_read": sorted(set(queued_by_id) - {entry["fields"]["request_id"] for entry in all_served}),
                         "natural_budget_resumptions": natural, "data_carrying_peer_circuits": circuits,
                         "natural_correlation_scope": "fresh peer Dial/Ready carrying at least 64 KiB of binary payload, then same-ID queued/served positive-offset reads; temporal association in this two-device one-blob window, not a decoded request-to-circuit identifier",
                         "relay_budget_closures_after_final_manual_resume": budget_logs,
                         "source_logical_returned_bytes": logical_bytes, "physical_sqlite_vfs_bytes": None,
                         "capture_payload_bytes": capture.payload_bytes - initial_bytes, "capture_frames": capture.frame_count - initial_frames,
                         "capture_counter": "all captured directions and both relay legs, including control and protocol overhead; not committed body progress or per-circuit billed bytes",
                         "source_unchanged": True, "relay_private_events": relay_events, "relay_private_blobs": relay_blobs,
                         "duration_seconds": time.monotonic() - started})
            require(not invocations(run), "read-only transfer and recovery never invoke host work")
            verify_no_model_budget(nodes)
            receiver.stop()
            private_json(run / "report.json", report)
            print(f"PASS NET-1 Lane 2 size={size} report: {run / 'report.json'}", flush=True)
        require(not capture.errors, "relay capture remains parseable across every interruption")
        case["client_identities"] = {}
        for node in nodes:
            path = node.client.identity_file
            metadata = path.lstat()
            require(stat.S_ISREG(metadata.st_mode) and stat.S_IMODE(metadata.st_mode) == 0o600 and metadata.st_uid == os.getuid(),
                    "every real WASM test client identity is persisted privately from generation")
            case["client_identities"][node.name] = {"path": str(path), "mode": oct(stat.S_IMODE(metadata.st_mode)), "uid": metadata.st_uid,
                                                     "persistence": "generated with exclusive create and mode 0600 before first client operation"}
        require(file_sha256(binary) == report["binary_sha256"], "Lane 2 uses one unchanged frozen binary")
        case["status"] = report["status"] = "passed"
        report["fixture_invocations"] = 0
    finally:
        for node in reversed(nodes):
            node.stop()
        if capture:
            capture.close()


def network_trace_records(node, offset=0, *, messages=None):
    result = []
    names = "source|class|target|reason|generation|previous|previous_source|previous_authenticated_secs|current|removed_pending|directory|outcome|cached|error|circuit|event|direction|queued_frames|queued_bytes|reader_held_bytes|writer_held_bytes|accepted_bytes|window_bytes|held_bytes|bytes|active|network|peer|neighbor|next_hop|link_id|flight_id|selection|request_id|origin|method|operation_id|request_type|delivered|fullname|epoch|urls|listen|addr_auto|addresses|original|new_name|interface|state|signed_at_ms|remaining_ms|removed_sources|remaining_sources|removed_routes|outbound|path|kind|trigger|timeout_ms|rr_type|ip|mode|status|deadline_seconds|route|route_kind|cancelled"
    path = node.directory / "process.log"
    if not path.exists():
        return result
    position = offset
    with path.open("rb") as stream:
        stream.seek(offset)
        for raw in stream:
            if not raw.endswith(b"\n"):
                break
            line = re.sub(r"\x1b\[[0-9;]*m", "", raw.decode(errors="replace")).strip()
            end = position + len(raw)
            if messages is None or any(message in line for message in messages):
                fields = {}
                for key, value in re.findall(rf'\b({names})=("(?:\\.|[^"\\])*"|\[(?:"(?:\\.|[^"\\])*"|[^\]"])*\]|\{{[^}}]*\}}|Some\([^)]*\)|[^\s,}}]+)', line):
                    value = value.strip()
                    if key in ("target", "request_type", "operation_id", "trigger") and value.startswith("Some(") and value.endswith(")"):
                        value = value[5:-1]
                    fields[key] = json.loads(value) if value.startswith('"') else value
                stamp = re.search(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z", line)
                result.append({"log_offset": position, "log_end": end, "fields": fields, "message": line,
                               "wall_time": datetime.fromisoformat(stamp.group().replace("Z", "+00:00")).timestamp() if stamp else None})
            position = end
    return result


def network_peer_query(caller, target, label):
    network, scope = caller.client.network_id, "net1-read-" + label
    database = caller.directory / "node.sqlite3"
    with sqlite3.connect(database) as connection:
        seeded = connection.execute("SELECT 1 FROM conversations WHERE network_id=? AND conversation_id=?", (network, scope)).fetchone()
    if not seeded:
        repository = Path(__file__).resolve().parents[1]
        envelopes, previous = [], None
        for sequence, body in enumerate(({"type": "conversation_created", "title": "Read-only native status"},
                {"type": "message_posted", "message_id": label, "role": "user", "text": "Observe existing native work only",
                 "scheduler_node": target.peer_id}), 1):
            event = {"version": 1, "network_id": network, "scope_id": scope, "author": caller.peer_id,
                     "sequence": sequence, "previous": previous, "parents": [previous] if previous else [],
                     "created_at_ms": sequence, "body": body, "blobs": []}
            signed = subprocess.run(["node", str(repository / "scripts/crypto-client.mjs"), "--module", str(caller.client.module)],
                input=json.dumps({"op": "fixture_event", "identity_file": str(caller.directory / "identity.json"), "event": event}) + "\n",
                capture_output=True, text=True, timeout=15)
            require(signed.returncode == 0, "private fixture signs the exact native observation target")
            reply = json.loads(signed.stdout)
            require(reply["ok"], "native observation fixture event is signed")
            envelope = reply["result"]
            envelopes.append((event, envelope))
            previous = envelope["hash"]
        with sqlite3.connect(database) as connection:
            for event, envelope in envelopes:
                connection.execute("INSERT OR IGNORE INTO events(network_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,?)",
                    (network, envelope["hash"], scope, caller.peer_id, event["sequence"], json.dumps(envelope, separators=(",", ":")), "ready"))
            connection.execute("INSERT OR IGNORE INTO conversations(network_id,conversation_id,title) VALUES(?,?,?)", (network, scope, "Read-only native status"))
            connection.execute("INSERT OR IGNORE INTO messages(network_id,conversation_id,event_hash,message_id,role,text,created_at_ms,ordinal) VALUES(?,?,?,?,?,?,?,?)",
                (network, scope, envelopes[-1][1]["hash"], label, "user", "Observe existing native work only", 2, 2))
    target_offset = (target.directory / "process.log").stat().st_size
    result = caller.client.rpc("conversation", conversation_id=scope)
    require(result["native_sessions"] == [], "read-only target has no native work to invent")
    handled = wait_for("the fixed original target handles an authenticated native status read", lambda: [entry
        for entry in network_trace_records(target, target_offset, messages=["peer business request handled"])
        if entry["fields"].get("network") == network and entry["fields"].get("origin") == caller.peer_id
        and entry["fields"].get("request_type") == "native_status" and entry["fields"].get("outcome") == "ok"])
    require(len(handled) == 1, "the read reaches its original authenticated target once")
    return {"native_sessions": result["native_sessions"]}


def network_peer_read_evidence(caller, target, network, label, caller_offset, target_offset,
                               *, next_hop, link_id, route_kind):
    def handled_reads():
        return [entry for entry in network_trace_records(target, target_offset, messages=["peer business request handled"])
                if entry["fields"].get("network") == network and entry["fields"].get("origin") == caller.peer_id
                and entry["fields"].get("request_type") == "native_status"]

    handled = wait_for("the original target handles this exact native status read", handled_reads)
    require(len(handled) == 1 and handled[0]["fields"].get("outcome") == "ok",
            "one actual original-target native status read succeeds")
    request_id = handled[0]["fields"]["request_id"]
    queued = wait_for("the same native status request is queued to the expected current next hop", lambda: [
        entry for entry in network_trace_records(caller, caller_offset, messages=["peer packet queued"])
        if entry["fields"].get("network") == network and entry["fields"].get("request_id") == request_id
        and entry["fields"].get("kind") == "Request"])
    require(len(queued) == 1 and queued[0]["fields"].get("target") == target.peer_id
            and queued[0]["fields"].get("next_hop") == next_hop and queued[0]["fields"].get("link_id") == link_id
            and queued[0]["fields"].get("route_kind") == route_kind,
            "the exact read uses only its expected next-hop link generation")
    response = wait_for("the caller accepts the matching original-target signed response", lambda: next(
        (entry for entry in network_trace_records(caller, caller_offset, messages=["peer response matched"])
         if entry["fields"].get("network") == network and entry["fields"].get("request_id") == request_id
         and entry["fields"].get("origin") == target.peer_id and entry["fields"].get("delivered") == "true"), None))
    require(len(handled_reads()) == 1, "the exact native status read is handled once")
    return {"observation": label, "request_id": request_id, "target_handled": handled[0],
            "caller_queued": queued[0], "caller_response": response}


def network_core_evidence(evidence_dir, slice_name, tests):
    require(evidence_dir is not None, "network lanes require frozen core evidence")
    repository = Path(__file__).resolve().parents[1]
    path = evidence_dir / f"m3-{slice_name}-core-slice.json"
    manifest = json.loads(path.read_text())
    if "checks" in manifest:
        require(all(item.get("exit_code") == 0 for item in manifest["checks"]), "referenced core checks passed")
        sources = manifest.get("source", manifest.get("sources"))
        require(isinstance(sources, list) and sources, "frozen source evidence provides its actual nonempty source list")
        log_names = [item["log"] for item in manifest["checks"] if item.get("log")]
    else:
        require(slice_name == "a2" and manifest.get("status") == "scoped pass"
                and manifest.get("review", {}).get("verdict") == "scoped pass"
                and not manifest["review"].get("unresolved_p1_p2")
                and all(manifest["check_results"].get(key) == "pass" for key in ("clippy", "fmt", "diff_check")),
                "A2 has its original scoped-pass check and independent-review schema")
        sources = list(manifest["sources"].values())
        log_names = list(manifest["final_checks"].values())
    snapshots = []
    for source in sources:
        snapshot = repository / source.get("snapshot", manifest.get("snapshot", ""))
        require(snapshot.is_file() and file_sha256(snapshot) == source["sha256"], "core source snapshot matches the frozen manifest")
        snapshots.append(str(snapshot))
    logs = []
    text = ""
    for name in log_names:
        log = evidence_dir.parent / name
        value = log.read_text()
        text += value
        logs.append({"path": str(log), "sha256": file_sha256(log)})
    require(all(re.search(rf"test [^\n]*::{re.escape(name)} \.\.\. ok", text) for name in tests), "named source boundary cases have real passed logs")
    return {"slice": slice_name, "manifest": str(path), "manifest_sha256": file_sha256(path), "source": sources,
            "snapshots": snapshots, "logs": logs, "passed_cases": tests,
            "scope": "production source boundary tests; exact live versus direct-ledger scope remains as declared by the core manifest",
            "actual_ws": manifest.get("actual_ws"), "ledger_only": manifest.get("ledger_only"), "scope_notes": manifest.get("scope_notes")}


def network_assert_source(entry, summary):
    raw = entry["fields"].get("source", "")
    if raw.startswith("Some(") and raw.endswith(")"):
        raw = raw[5:-1].strip('"')
    observed = ipaddress.ip_address(raw)
    actual = ipaddress.ip_address(summary["actual_local_address"][0])
    observed = observed.ipv4_mapped if isinstance(observed, ipaddress.IPv6Address) and observed.ipv4_mapped else observed
    actual = actual.ipv4_mapped if isinstance(actual, ipaddress.IPv6Address) and actual.ipv4_mapped else actual
    require(observed == actual, "server rejection uses the actual connected TCP source, including mapped IPv4 normalization")


def network_circuit_exit(entry):
    return ("network connection closed" in entry["message"]
            and any(value in entry["message"] for value in ("relay caller disconnected", "relay target disconnected",
                       "relay endpoint disconnected", "Connection reset without closing handshake", "relay accepts only encrypted binary frames")))


def network_fixture_cleanup(sockets, identities, nodes, case):
    errors = []
    for kind, values in (("socket", sockets), ("crypto", identities), ("node", nodes)):
        for value in reversed(values):
            try:
                value.stop() if kind == "node" else value.close()
            except Exception as error:
                errors.append({"kind": kind, "error_type": type(error).__name__})
    ports = []
    for node in nodes:
        listen = json.loads((node.directory / "config.json").read_text())["listen"]
        addresses = [(socket.AF_INET, "127.0.0.1")]
        if listen.startswith("[::]"):
            addresses.append((socket.AF_INET6, "::1"))
        for family, host in addresses:
            try:
                with socket.socket(family) as probe:
                    probe.settimeout(.3)
                    closed = probe.connect_ex((host, node.port)) != 0
                ports.append({"node": node.name, "host": host, "port": node.port, "closed": closed})
            except OSError as error:
                ports.append({"node": node.name, "host": host, "port": node.port, "closed": False, "probe_error": type(error).__name__})
    case["raw_sockets"] = [value.summary for value in sockets]
    case["cleanup"] = {"errors": errors, "ports": ports, "remaining_crypto_processes": sum(value.process is not None for value in identities),
                       "remaining_owned_node_processes": sum(value.process is not None for value in nodes)}
    if errors or not all(item["closed"] for item in ports) or case["cleanup"]["remaining_crypto_processes"] or case["cleanup"]["remaining_owned_node_processes"]:
        case["status"] = "failed"
    require(not errors and all(item["closed"] for item in ports)
            and case["cleanup"]["remaining_crypto_processes"] == case["cleanup"]["remaining_owned_node_processes"] == 0,
            "all owned network sockets, crypto helpers and Node processes are reclaimed")


def network_reliability_reservation(run, binary, workspace, fixture, helper, module, report, nodes, evidence_dir):
    case = {"status": "running", "group_deadline_seconds": 60, "stale_attach_expected": "old token can be removed before Attach; unknown or stale generation both reject it"}
    report["cases"]["reservation_generation"] = case
    sockets, identities = [], []
    deadline = time.monotonic() + 60
    env = {**os.environ, "RUST_LOG": "vibeface::network=debug,vibeface::api=debug", "NO_COLOR": "1"}

    def remaining():
        value = deadline - time.monotonic()
        require(value > 0, "reservation scenario stays within its total deadline")
        return value

    def identity(name):
        owner = NetworkCryptoFixture(run, name, module, deadline=deadline)
        identities.append(owner)
        return owner

    def socket_at():
        value = NetworkControlSocket(url, deadline=deadline)
        sockets.append(value)
        return value

    def trace(message, **expected):
        return next((entry for entry in network_trace_records(relay, messages=[message])
                     if all(entry["fields"].get(key) == value for key, value in expected.items())), None)

    def dial(reservation, owner):
        caller = socket_at()
        caller.send_control({"type": "dial", "target": owner.peer_id, "service": "peer"})
        incoming = reservation.control()
        require(incoming.get("type") == "incoming" and incoming.get("service") == "peer", "current reservation receives the exact peer Dial")
        return caller, incoming["token"]

    try:
        relay = Node(run, "R", binary, workspace, fixture, helper, module)
        nodes.append(relay)
        relay.configure(roles=["relay"], manager=None, codex_socket=str(run / "unused-codex.sock"),
                        network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False, "relay_enabled": True})
        relay.start(env=env)
        url = relay.url.replace("http:", "ws:") + "/api/network/ws"
        owner = identity("target")
        caller_owner = identity("caller")
        old = socket_at()
        require(old.reserve(owner) == {"type": "reserved"}, "old fixed identity completes a real reservation challenge")
        old_ready = wait_for("old generation ready trace", lambda: trace("relay reservation ready", target=owner.peer_id), timeout=remaining())
        old_generation = old_ready["fields"]["generation"]
        first, token = dial(old, owner)
        established_target = socket_at()
        require(established_target.attach(owner, token) == {"type": "ready"} and first.control() == {"type": "ready"}, "old generation establishes both circuit legs")
        noise = network_fixture_noise(first, established_target, caller_owner, owner)
        before = network_fixture_canary(first, established_target, caller_owner, owner, "before-handoff")
        old_pending_started = time.monotonic()
        pending_caller, old_token = dial(old, owner)
        token_tag = hashlib.sha256(old_token.encode()).hexdigest()
        wait_for("minimum generation spacing before deliberate new challenge", lambda: old_ready["wall_time"] is not None and time.time() >= old_ready["wall_time"] + 1.1, timeout=remaining())
        fresh = socket_at()
        require(fresh.reserve(owner) == {"type": "reserved"}, "same fixed identity authenticates a fresh challenge without client-closing the old socket")
        installed = wait_for("new generation installation", lambda: next((entry for entry in network_trace_records(relay, messages=["relay reservation installed"])
            if entry["fields"].get("target") == owner.peer_id and entry["fields"].get("generation") != old_generation), None), timeout=remaining())
        generation = installed["fields"]["generation"]
        require(old_generation in installed["fields"].get("previous", ""), "new generation explicitly replaces the original reservation")
        previous_closed = wait_for("server actively reclaims the previous physical socket", lambda: trace("relay reservation previous socket closed", target=owner.peer_id, generation=generation), timeout=remaining())
        removed = wait_for("old cleanup is scoped to its previous generation", lambda: trace("relay reservation removed", target=owner.peer_id, generation=old_generation, current="false"), timeout=remaining())
        new_ready = wait_for("new reservation remains ready after old cleanup", lambda: trace("relay reservation ready", target=owner.peer_id, generation=generation), timeout=remaining())
        old.close()
        after = network_fixture_canary(established_target, first, owner, caller_owner, "old-ready-circuit-after-handoff")
        stale_cursor = (relay.directory / "process.log").stat().st_size
        stale = socket_at()
        require(time.monotonic() < old_pending_started + 10, "old token refusal precedes its natural expiry")
        try:
            stale.attach(owner, old_token)
            raise AssertionError("old generation token must never attach to the new reservation")
        except (EOFError, ConnectionResetError, BrokenPipeError) as error:
            stale_failure = type(error).__name__
        stale_trace = wait_for("old pending token is rejected by actual relay", lambda: next((entry for entry in network_trace_records(relay, stale_cursor)
            if "unknown relay attachment" in entry["message"] or "stale relay attachment" in entry["message"] or entry["fields"].get("reason") == "stale_generation"), None), timeout=remaining())
        require(time.monotonic() < old_pending_started + 10, "completed old-token refusal precedes natural expiry even including signature and response")
        pending_caller.close()
        new_caller_owner = identity("new-caller")
        new_target_owner = identity("target")
        require(new_target_owner.peer_id == owner.peer_id, "new circuit keeps the exact original target key")
        new_caller, token = dial(fresh, owner)
        new_target = socket_at()
        require(new_target.attach(new_target_owner, token) == {"type": "ready"} and new_caller.control() == {"type": "ready"}, "new generation accepts a fresh attachment after old cleanup")
        new_noise = network_fixture_noise(new_caller, new_target, new_caller_owner, new_target_owner)
        new_canary = network_fixture_canary(new_caller, new_target, new_caller_owner, new_target_owner, "new-generation")
        ready_exits = []
        for caller_socket, target_socket in ((first, established_target), (new_caller, new_target)):
            exit_cursor = (relay.directory / "process.log").stat().st_size
            caller_socket.close()
            target_socket.close()
            ready_exits.append(wait_for("owned completed circuit actually exits before the quota subcase",
                lambda exit_cursor=exit_cursor: next((entry for entry in network_trace_records(relay, exit_cursor) if network_circuit_exit(entry)), None), timeout=remaining()))
        current_generation = generation
        recovered_caller_identity = identity("new-caller")
        recovered_target_identity = identity("target")
        require(recovered_caller_identity.peer_id == new_caller_owner.peer_id and recovered_target_identity.peer_id == owner.peer_id,
                "capacity recovery keeps both original fixture identities")
        held, held_tokens = [], []
        held_started = time.monotonic()
        for _ in range(4):
            value, held_token = dial(fresh, owner)
            held.append(value)
            held_tokens.append(held_token)
        denied_cursor = (relay.directory / "process.log").stat().st_size
        extra = socket_at()
        extra.send_control({"type": "dial", "target": owner.peer_id, "service": "peer"})
        try:
            extra.control()
            raise AssertionError("fifth same-source circuit must be refused")
        except (EOFError, ConnectionResetError, BrokenPipeError):
            pass
        denied = wait_for("actual source circuit refusal", lambda: next((entry for entry in network_trace_records(relay, denied_cursor, messages=["network admission rejected"])
            if entry["fields"].get("reason") == "source_circuit"), None), timeout=remaining())
        held_target = socket_at()
        require(held_target.attach(new_target_owner, held_tokens[0]) == {"type": "ready"} and held[0].control() == {"type": "ready"}, "selected waiting circuit is attached before testing immediate transport cleanup")
        cleanup_cursor = (relay.directory / "process.log").stat().st_size
        held[0].close()
        held_target.close()
        selected_exit = wait_for("selected Ready circuit actually exits after both endpoints close", lambda: next((entry
                 for entry in network_trace_records(relay, cleanup_cursor) if network_circuit_exit(entry)), None), timeout=remaining())
        recovered, token = dial(fresh, owner)
        recovered_target = socket_at()
        require(recovered_target.attach(new_target_owner, token) == {"type": "ready"} and recovered.control() == {"type": "ready"}, "releasing only the current work restores capacity on the same reservation")
        require(time.monotonic() < held_started + 10, "capacity recovers before the other pending circuits naturally expire")
        recovered_noise = network_fixture_noise(recovered, recovered_target, recovered_caller_identity, recovered_target_identity)
        recovered_canary = network_fixture_canary(recovered, recovered_target, recovered_caller_identity, recovered_target_identity, "after-quota-release")
        require(time.monotonic() < held_started + 10, "recovered Noise and canary complete while the other pending requests remain unexpired")
        require(not trace("relay reservation removed", target=owner.peer_id, generation=current_generation), "current reservation was not torn down by rejected Dial")
        core = network_core_evidence(evidence_dir, "a1", ["full_data_capacity_refuses_only_the_incoming_and_recovers_on_the_same_reservation"])
        generations_core = network_core_evidence(evidence_dir, "a2", ["a2_new_challenge_preserves_ready_circuit_during_blocked_directory_publication",
            "a2_outer_abort_drops_an_attached_socket_waiting_in_the_receiver", "a2_real_full_source_and_global_handoff_uses_a_full_logical_table_fixture"])
        case.update({"status": "passed", "target_peer_id": owner.peer_id, "old_generation": old_generation, "new_generation": generation,
                     "old_ready": old_ready, "new_installed": installed, "previous_socket_closed": previous_closed,
                     "old_cleanup": removed, "new_ready": new_ready, "old_token_sha256": token_tag, "stale_failure": stale_failure,
                     "stale_trace": stale_trace, "old_noise": noise, "canary_before": before, "old_circuit_canary_after": after,
                     "new_noise": new_noise, "new_canary": new_canary, "current_dial_refusal": denied,
                     "ready_circuit_exits_before_quota": ready_exits, "selected_circuit_exit": selected_exit,
                     "same_generation_recovered": generation, "recovered_noise": recovered_noise, "recovered_canary": recovered_canary,
                     "core_boundary_evidence": [core, generations_core],
                     "not_claimed": ["packaged target Node Data pool exhausted", "64 live source identities on one packaged Node"],
                     "completed_wall_time": time.time(), "deadline_remaining_seconds": remaining()})
        require(not invocations(run), "reservation acceptance never invokes host work")
        verify_no_model_budget(nodes)
        require(file_sha256(binary) == report["binary_sha256"], "Lane 5 binary is immutable")
        report["status"] = "passed"
        report["fixture_invocations"] = 0
    except Exception:
        case["status"] = "failed"
        raise
    finally:
        network_fixture_cleanup(sockets, identities, nodes, case)


def network_reliability_duplex(run, binary, workspace, fixture, helper, module, report, nodes, evidence_dir):
    case = {"status": "running", "group_deadline_seconds": 120, "fixture_messages": 160, "fixture_message_bytes": 60000,
            "flow_identity": "local circuit digest plus fixed target; no attach token is exported"}
    report["cases"]["budget_and_duplex"]["groups"]["slow_duplex"] = case
    sockets, identities, own_nodes = [], [], []
    flood_thread = None
    flood = {"records_sent": 0, "bytes_sent": 0, "error_type": None}
    deadline = time.monotonic() + 120

    def remaining():
        value = deadline - time.monotonic()
        require(value > 0, "duplex group has one total deadline")
        return value

    def identity(name):
        owner = NetworkCryptoFixture(run, name, module, deadline=deadline)
        identities.append(owner)
        return owner

    def connection(*, receive_buffer=None):
        value = NetworkControlSocket(url, deadline=deadline, receive_buffer=receive_buffer)
        sockets.append(value)
        return value

    def flow_records():
        return network_trace_records(relay, cursor, messages=["relay flow"])

    def establish(reservation, caller_owner, target_owner, *, receive_buffer=None):
        caller = connection()
        caller.send_control({"type": "dial", "target": target_owner.peer_id, "service": "peer"})
        incoming = reservation.control()
        require(incoming.get("type") == "incoming" and incoming.get("service") == "peer", "fixed target receives the original relay request")
        target = connection(receive_buffer=receive_buffer)
        require(target.attach(target_owner, incoming["token"]) == {"type": "ready"} and caller.control() == {"type": "ready"}, "both raw data legs reach Ready")
        noise = network_fixture_noise(caller, target, caller_owner, target_owner)
        return caller, target, noise

    try:
        relay = Node(run, "R-duplex", binary, workspace, fixture, helper, module)
        nodes.append(relay)
        own_nodes.append(relay)
        relay.configure(roles=["relay"], manager=None, codex_socket=str(run / "unused-codex.sock"),
                        network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False, "relay_enabled": True})
        relay.start(env={**os.environ, "RUST_LOG": "vibeface::network=debug,vibeface::api=debug", "NO_COLOR": "1"})
        url = relay.url.replace("http:", "ws:") + "/api/network/ws"
        caller_owner, target_owner = identity("duplex-caller"), identity("duplex-target")
        other_caller_owner, other_target_owner = identity("duplex-other-caller"), identity("duplex-other-target")
        reservation, other_reservation = connection(), connection()
        require(reservation.reserve(target_owner) == {"type": "reserved"}
                and other_reservation.reserve(other_target_owner) == {"type": "reserved"}, "both fixed target reservations are ready")
        cursor = (relay.directory / "process.log").stat().st_size
        caller, stopped_target, noise = establish(reservation, caller_owner, target_owner, receive_buffer=8192)
        records, expected_hashes = [], []
        for sequence in range(160):
            payload = sequence.to_bytes(8, "little") + bytes([sequence % 256]) * (60000 - 8)
            expected_hashes.append(hashlib.sha256(payload).hexdigest())
            sealed = caller_owner.command("seal_message", data=payload.hex())
            records.extend(bytes.fromhex(value) for value in sealed)
            require(len(records) <= 320 and sum(map(len, records)) <= 12 * 1024 * 1024, "encrypted flood fixture is bounded before sending")
        before_flood = (relay.directory / "process.log").stat().st_size
        paused = {"wall_time": time.time(), "monotonic": time.monotonic(), "target_socket": dict(stopped_target.summary),
                  "mechanism": "the raw target performs no TCP recv between final Noise read and explicit resume; send remains independent"}
        stable_pending = None

        def send_flood():
            try:
                for value in records:
                    caller.send_binary(value)
                    flood["records_sent"] += 1
                    flood["bytes_sent"] += len(value)
            except Exception as error:
                flood["error_type"] = type(error).__name__

        flood_thread = threading.Thread(target=send_flood, daemon=True)
        flood_thread.start()

        def continuously_blocked():
            nonlocal stable_pending
            candidates = {}
            for entry in network_trace_records(relay, before_flood, messages=["relay flow"]):
                fields = entry["fields"]
                if fields.get("target") != target_owner.peer_id or fields.get("direction") != "caller_to_target":
                    continue
                circuit = fields.get("circuit")
                require(circuit and re.fullmatch(r"[0-9a-f]{12}", circuit), "slow direction is associated with an actual local circuit tag")
                if fields.get("event") == "write_pending":
                    candidates[circuit] = {"write_pending": entry, "queue_full": None}
                elif fields.get("event") in ("write_resumed", "closed"):
                    candidates.pop(circuit, None)
                elif fields.get("event") == "queue_full" and circuit in candidates:
                    candidates[circuit]["queue_full"] = entry
            latest = next(({"circuit": circuit, **value} for circuit, value in candidates.items() if value["queue_full"]), None)
            if latest is None:
                stable_pending = None
                return None
            key = (latest["circuit"], latest["write_pending"]["log_offset"])
            if stable_pending is None or stable_pending["key"] != key:
                stable_pending = {"key": key, "monotonic": time.monotonic()}
                return None
            elapsed = time.monotonic() - stable_pending["monotonic"]
            return {**latest, "continuous_observed_seconds_before_progress": elapsed} if elapsed >= .5 else None

        blocked = wait_for("actual same-flow pending write plus full queue while TCP reading is stopped", continuously_blocked,
                           timeout=min(10, remaining()))
        circuit = blocked["circuit"]
        reverse = network_fixture_canary(stopped_target, caller, target_owner, caller_owner, "reverse-during-continuous-backpressure")
        lookup_socket = connection()
        lookup_socket.send_control({"type": "lookup", "target": other_target_owner.peer_id})
        lookup_reply = lookup_socket.control()
        require(lookup_reply.get("type") == "record", "directory control progresses while the matched slow circuit remains blocked")
        other_caller, other_target, other_noise = establish(other_reservation, other_caller_owner, other_target_owner)
        other_canary = network_fixture_canary(other_caller, other_target, other_caller_owner, other_target_owner, "other-target-during-backpressure")
        progress_done = {"wall_time": time.time(), "monotonic": time.monotonic()}
        require(progress_done["monotonic"] < paused["monotonic"] + 10, "reverse and other-target progress precede the conservative pending-write deadline")
        same_flow = [entry for entry in flow_records() if entry["fields"].get("circuit") == circuit and entry["fields"].get("target") == target_owner.peer_id]
        require(not any(entry["log_offset"] > blocked["write_pending"]["log_offset"] and entry["fields"].get("direction") == "caller_to_target"
                        and entry["fields"].get("event") in ("write_resumed", "closed") for entry in same_flow),
                "the same pending write stays unresolved through reverse, lookup and other-target completion")
        resumed = {"wall_time": time.time(), "monotonic": time.monotonic()}
        decrypted = []
        for sequence in range(160):
            opened = None
            for _ in range(8):
                opened = target_owner.command("open_record", data=stopped_target.binary().hex())
                if opened is not None:
                    break
            require(opened is not None, "resumed reader reconstructs each bounded protected message")
            payload = bytes.fromhex(opened)
            require(len(payload) == 60000 and int.from_bytes(payload[:8], "little") == sequence
                    and hashlib.sha256(payload).hexdigest() == expected_hashes[sequence], "resumed TCP reader decrypts exact ordered payload bytes")
            decrypted.append(hashlib.sha256(payload).hexdigest())
        flood_thread.join(timeout=min(5, remaining()))
        require(not flood_thread.is_alive() and flood["error_type"] is None and flood["records_sent"] == len(records), "bounded sender finishes and is joined after the actual reader resumes")
        same_flow = [entry for entry in flow_records() if entry["fields"].get("circuit") == circuit and entry["fields"].get("target") == target_owner.peer_id]
        interruptions = [entry for entry in same_flow if entry["log_offset"] > blocked["write_pending"]["log_offset"]
                         and entry["fields"].get("direction") == "caller_to_target" and entry["fields"].get("event") in ("write_resumed", "closed")]
        require(interruptions and interruptions[0]["fields"]["event"] == "write_resumed"
                and interruptions[0]["wall_time"] is not None and interruptions[0]["wall_time"] >= progress_done["wall_time"]
                and interruptions[0]["wall_time"] >= resumed["wall_time"],
                "complete logs confirm the original blocked write resumed only after protected progress and explicit read restart")
        snapshots = []
        for entry in same_flow:
            fields = entry["fields"]
            queued_frames = int(fields["queued_frames"])
            queued_bytes, reader_bytes, writer_bytes = (int(fields[key]) for key in ("queued_bytes", "reader_held_bytes", "writer_held_bytes"))
            require(0 <= queued_frames <= 4 and 0 <= queued_bytes <= queued_frames * 65535
                    and 0 <= reader_bytes <= 65535 and 0 <= writer_bytes <= 65535,
                    "actual same-flow snapshots respect each bounded queue and held frame")
            snapshots.append({"direction": fields["direction"], "event": fields["event"], "queued_frames": queued_frames,
                              "application_held_bytes_this_direction": queued_bytes + reader_bytes + writer_bytes})
        require(snapshots and any(value["direction"] == "caller_to_target" and value["event"] == "queue_full" for value in snapshots),
                "actual same-R span snapshots contain the matched slow full queue")
        caller.close()
        stopped_target.close()
        def closed_both_directions():
            values = [entry for entry in flow_records() if entry["fields"].get("circuit") == circuit and entry["fields"].get("event") == "closed"]
            return values if {entry["fields"].get("direction") for entry in values} == {"caller_to_target", "target_to_caller"} else None
        closed = wait_for("both directions of the matched flow release their held frames", closed_both_directions, timeout=min(10, remaining()))
        require({entry["fields"].get("direction") for entry in closed} == {"caller_to_target", "target_to_caller"}
                and all(all(int(entry["fields"][key]) == 0 for key in ("queued_frames", "queued_bytes", "reader_held_bytes", "writer_held_bytes")) for entry in closed),
                "actual closed snapshots show no queued or held application frames")
        recovery_caller_owner, recovery_target_owner = identity("duplex-caller"), identity("duplex-target")
        recovery_caller, recovery_target, recovery_noise = establish(reservation, recovery_caller_owner, recovery_target_owner)
        recovery_canary = network_fixture_canary(recovery_caller, recovery_target, recovery_caller_owner, recovery_target_owner, "after-slow-circuit-cleanup")
        core = network_core_evidence(evidence_dir, "x1", ["x1_shared_production_byte_rate_frame_and_liveness_boundaries",
            "x1_each_silent_endpoint_expires_despite_continuous_opposite_traffic", "x1_stopped_tcp_reader_keeps_reverse_noise_and_other_target_progress",
            "x1_two_real_writers_share_the_production_rate_and_total_byte_limit",
            "x1_abort_and_timeout_during_rate_or_slow_write_release_io_and_allow_reuse"])
        case.update({"status": "passed", "relay_peer_id": relay.peer_id, "target_peer_id": target_owner.peer_id, "circuit": circuit,
            "noise": noise, "reader_paused": paused, "continuous_backpressure": blocked, "reverse_canary": reverse,
            "directory_control": {"type": lookup_reply["type"], "target_peer_id": other_target_owner.peer_id,
                                  "has_record": lookup_reply.get("record") is not None, "scope": "Control query progress; fixture target has no published directory record"},
            "other_target_noise": other_noise, "other_target_canary": other_canary, "progress_done": progress_done, "reader_resumed": resumed,
            "first_write_resumed_after_progress": interruptions[0], "decrypted_message_sha256": decrypted, "flood": flood,
            "flow_snapshots": snapshots, "closed_same_flow": closed, "recovery_noise": recovery_noise, "recovery_canary": recovery_canary,
            "observed_snapshot_note": "instantaneous application-held bytes per direction, not the whole process memory peak or WS/TCP buffers",
            "structural_two_direction_application_bound": 786420, "core_boundary_evidence": [core],
            "combined_64MiB_budget_scope": "production shared-byte/rate/frame boundary fixture; this packaged slow-flow flood remains below the full circuit budget"})
    except Exception:
        case["status"] = "failed"
        raise
    finally:
        try:
            network_fixture_cleanup(sockets, identities, own_nodes, case)
        finally:
            if flood_thread:
                flood_thread.join(timeout=2)
                case["flood_worker_joined"] = not flood_thread.is_alive()
                if flood_thread.is_alive():
                    case["status"] = "failed"
                require(not flood_thread.is_alive(), "owned flood sender exits after exact socket cleanup")


def network_reliability_budgets(run, binary, workspace, fixture, helper, module, report, nodes, evidence_dir):
    case = {"status": "running", "groups": {}, "core_boundary_evidence": [],
            "not_checked": ["X1 slow duplex and combined-byte budget", "production proxy users separated by IP"],
            "source_boundary": "direct TCP into one temporary dual-stack listener; forwarded headers do not determine source"}
    report["cases"]["budget_and_duplex"] = case
    sockets, identities = [], []
    env = {**os.environ, "RUST_LOG": "vibeface::network=debug,vibeface::api=debug", "NO_COLOR": "1"}
    deadline = None

    def remaining():
        value = deadline - time.monotonic()
        require(value > 0, "network budget group stays inside its total deadline")
        return value

    def start_relay(name):
        node = Node(run, name, binary, workspace, fixture, helper, module)
        nodes.append(node)
        node.configure(roles=["relay"], manager=None, codex_socket=str(run / "unused-codex.sock"),
                       listen=f"[::]:{node.port}", public_url=f"http://127.0.0.1:{node.port}", allow_insecure_lan=True,
                       network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False, "relay_enabled": True})
        node.start(env=env)
        connection = http.client.HTTPConnection("::1", node.port, timeout=min(5, remaining()))
        try:
            connection.request("GET", "/api/info")
            response = connection.getresponse()
            require(response.status == 200 and json.loads(response.read(65536))["peer_id"] == node.peer_id,
                    "both real loopback address families reach the same packaged Node identity")
        finally:
            connection.close()
        return node

    def open_at(node, source, *, forwarded_for=None):
        host = "127.0.0.1" if source == "v4" else "[::1]"
        value = NetworkControlSocket(f"ws://{host}:{node.port}/api/network/ws", source_ip="127.0.0.1" if source == "v4" else "::1",
                                     deadline=deadline, forwarded_for=forwarded_for)
        sockets.append(value)
        return value

    def identity(name):
        owner = NetworkCryptoFixture(run, name, module, deadline=deadline)
        identities.append(owner)
        return owner

    def reason(node, cursor, code):
        return wait_for(f"actual {code} rejection", lambda: next((entry for entry in network_trace_records(node, cursor, messages=["network admission rejected"])
            if entry["fields"].get("reason") == code), None), timeout=remaining())

    def lookup(node, source):
        value = open_at(node, source)
        value.send_control({"type": "lookup", "target": node.peer_id})
        reply = value.control()
        require(reply.get("type") == "record" and reply.get("record", {}).get("peer_id") == node.peer_id,
                "other legal source completes a real target lookup")
        summary = dict(value.summary)
        value.close()
        return {"source_socket": summary, "target_peer_id": node.peer_id, "record_received": True}

    def closed_after_control(value):
        try:
            value.control()
        except (EOFError, ConnectionResetError, BrokenPipeError):
            return {"upgrade_status": 101, "closed_before_success": True, "socket": dict(value.summary)}
        raise AssertionError("rejected WS work must close instead of returning success or merely timing out")

    try:
        deadline = time.monotonic() + 45
        relay = start_relay("R-budget")
        case["listener"] = {"listen": f"[::]:{relay.port}", "public_url": relay.url, "allow_insecure_lan": True,
                            "scope": "temporary wildcard test listener; no production config changed", "peer_id": relay.peer_id}
        initial_group_deadline = deadline
        first_initial_started = time.monotonic()
        deadline = min(deadline, first_initial_started + 10)
        held_initial = [open_at(relay, "v4") for _ in range(4)]
        cursor = (relay.directory / "process.log").stat().st_size
        try:
            open_at(relay, "v4", forwarded_for="198.51.100.61")
            raise AssertionError("fifth real source Initial must fail before upgrade")
        except NetworkUpgradeRejected as error:
            initial_rejection = error.summary
            require(error.summary["upgrade_status"] == 503 and "source_initial_socket" in error.summary["rejection_body"], "initial excess is explicit HTTP503 source rejection")
        initial_reason = reason(relay, cursor, "source_initial_socket")
        network_assert_source(initial_reason, initial_rejection)
        other_initial = lookup(relay, "v6")
        require(time.monotonic() < first_initial_started + 10, "other real source lookup completes while all four Initial slots remain unexpired")
        for value in held_initial:
            value.close()
        deadline = initial_group_deadline
        resumed_lookup = wait_for("source Initial capacity is reusable after exact owned sockets close", lambda: lookup(relay, "v4"), timeout=remaining())
        case["groups"]["source_initial"] = {"status": "passed", "limit": 4, "rejected": initial_rejection,
            "server_rejection": initial_reason, "other_real_source_lookup": other_initial, "released_source_lookup": resumed_lookup,
            "held_sockets": [value.summary for value in held_initial], "forwarded_header_ignored": True}

        deadline = time.monotonic() + 60
        long_reservations, long_owners = [], []
        for index in range(16):
            owner = identity(f"long-source-{index}")
            value = open_at(relay, "v4")
            require(value.reserve(owner) == {"type": "reserved"}, "each distinct signed identity gets one source long socket")
            long_owners.append(owner)
            long_reservations.append(value)
        extra_owner = identity("long-source-extra")
        cursor = (relay.directory / "process.log").stat().st_size
        extra = open_at(relay, "v4", forwarded_for="198.51.100.62")
        proof = json.dumps({"nonce": extra.nonce, "peer_id": extra_owner.peer_id}, separators=(",", ":"))
        extra.send_control({"type": "reserve", "peer_id": extra_owner.peer_id, "public_key": extra_owner.public_key,
                            "signature": extra_owner.sign("relay-reserve", proof.encode())})
        long_closed = closed_after_control(extra)
        long_reason = reason(relay, cursor, "source_established_socket")
        network_assert_source(long_reason, extra.summary)
        other_long = lookup(relay, "v6")
        release_cursor = (relay.directory / "process.log").stat().st_size
        for value in long_reservations:
            value.close()
        wait_for("all sixteen exact source reservations actually clean up", lambda: len({entry["fields"].get("target") for entry in network_trace_records(relay, release_cursor, messages=["relay reservation removed"])
                 if entry["fields"].get("target") in {owner.peer_id for owner in long_owners}}) == 16, timeout=remaining())
        case["groups"]["source_long_identity_isolation"] = {"status": "passed", "limit": 16,
            "fixed_distinct_identities": [owner.peer_id for owner in long_owners], "refused_new_identity": extra_owner.peer_id,
            "ws_close": long_closed, "server_rejection": long_reason, "other_real_source_lookup": other_long,
            "removed_reservations": 16, "forwarded_header_ignored": True}
        for owner in long_owners + [extra_owner]:
            owner.close()

        deadline = time.monotonic() + 45
        t1, t2 = identity("target-one"), identity("target-two")
        r1, r2 = open_at(relay, "v4"), open_at(relay, "v4")
        require(r1.reserve(t1) == {"type": "reserved"} and r2.reserve(t2) == {"type": "reserved"}, "two fixed legal targets reserve independently")
        caller_owner = identity("other-source-caller")
        pending, tokens = [], []
        first_pending = time.monotonic()
        for _ in range(4):
            value = open_at(relay, "v4")
            value.send_control({"type": "dial", "target": t1.peer_id, "service": "peer"})
            incoming = r1.control()
            require(incoming.get("type") == "incoming", "every pending Dial reaches the actual target reservation")
            pending.append(value)
            tokens.append(incoming["token"])
        cursor = (relay.directory / "process.log").stat().st_size
        source_excess = open_at(relay, "v4")
        source_excess.send_control({"type": "dial", "target": t2.peer_id, "service": "peer"})
        source_closed = closed_after_control(source_excess)
        source_reason = reason(relay, cursor, "source_circuit")
        network_assert_source(source_reason, source_excess.summary)
        cursor = (relay.directory / "process.log").stat().st_size
        target_excess = open_at(relay, "v6")
        target_excess.send_control({"type": "dial", "target": t1.peer_id, "service": "peer"})
        target_closed = closed_after_control(target_excess)
        target_reason = reason(relay, cursor, "target_circuit")
        network_assert_source(target_reason, target_excess.summary)
        other_caller = open_at(relay, "v6")
        other_caller.send_control({"type": "dial", "target": t2.peer_id, "service": "peer"})
        token = r2.control()["token"]
        other_target = open_at(relay, "v4")
        require(other_target.attach(t2, token) == {"type": "ready"} and other_caller.control() == {"type": "ready"}, "another real source and legal target get a complete circuit under overload")
        noise = network_fixture_noise(other_caller, other_target, caller_owner, t2)
        canary = network_fixture_canary(other_caller, other_target, caller_owner, t2, "other-target-under-load")
        lookups = {source: lookup(relay, source) for source in ("v4", "v6")}
        require(time.monotonic() < first_pending + 10, "source/target refusals and other-target progress precede pending Attach expiry")
        for value in pending:
            value.close()
        r1.close()
        r2.close()
        other_caller.close()
        other_target.close()
        case["groups"]["source_and_target_circuits"] = {"status": "passed", "source_limit": 4, "target_limit": 4,
            "source_refused_on_other_empty_target": {"close": source_closed, "trace": source_reason, "target": t2.peer_id},
            "other_source_refused_on_full_target": {"close": target_closed, "trace": target_reason, "target": t1.peer_id},
            "other_source_other_target_noise": noise, "canary": canary, "lookups": lookups,
            "pending_tokens_sha256": [hashlib.sha256(token.encode()).hexdigest() for token in tokens], "preceded_pending_expiry": True}
        relay.stop()

        deadline = time.monotonic() + 55
        rate_relay = start_relay("R-rate")
        window_start = time.monotonic()
        rate_results = [lookup(rate_relay, "v4") for _ in range(120)]
        cursor = (rate_relay.directory / "process.log").stat().st_size
        try:
            open_at(rate_relay, "v4", forwarded_for="198.51.100.63")
            raise AssertionError("121st source request must not upgrade")
        except NetworkUpgradeRejected as error:
            rate_rejected = error.summary
            require(error.summary["upgrade_status"] == 429 and "source_rate" in error.summary["rejection_body"], "real same-source request rate uses HTTP429")
        rate_reason = reason(rate_relay, cursor, "source_rate")
        network_assert_source(rate_reason, rate_rejected)
        rate_other = lookup(rate_relay, "v6")
        require(time.monotonic() < window_start + 60, "all 120 requests and denial fit the unchanged source rate window")
        case["groups"]["source_request_rate"] = {"status": "passed", "fresh_relay_peer_id": rate_relay.peer_id,
            "accepted": len(rate_results), "source_limit": 120, "rejected": rate_rejected,
            "server_rejection": rate_reason, "other_real_source_lookup": rate_other,
            "window_seconds_observed": time.monotonic() - window_start, "forwarded_header_ignored": True,
            "accepted_socket_sources": [value["source_socket"]["actual_local_address"] for value in rate_results]}
        case["core_boundary_evidence"] = [network_core_evidence(evidence_dir, "a1", [
            "real_sources_share_initial_limits_despite_forwarded_headers_and_ipv6_is_independent",
            "real_source_request_rate_survives_socket_drop_and_another_source_can_lookup",
            "waiting_attach_circuits_bound_real_sources_and_targets_and_leave_lookup_capacity",
            "waiting_circuits_share_source_target_and_global_limits_and_drop_prunes_keys"])]
        rate_relay.stop()
        network_reliability_duplex(run, binary, workspace, fixture, helper, module, report, nodes, evidence_dir)
        require(not invocations(run), "quota and control acceptance never invokes host work")
        verify_no_model_budget(nodes)
        require(file_sha256(binary) == report["binary_sha256"], "Lane 6 uses one immutable binary")
        case["not_checked"] = ["production proxy users separated by IP", "whole-process memory peak"]
        case["status"] = report["status"] = "passed"
        report["fixture_invocations"] = 0
    except Exception:
        case["status"] = "failed"
        raise
    finally:
        network_fixture_cleanup(sockets, identities, nodes, case)


def network_reliability_directory(run, binary, workspace, fixture, helper, module, report, nodes, evidence_dir):
    repository = Path(__file__).resolve().parents[1]
    trace_env = {**os.environ, "RUST_LOG": "vibeface::network=debug,vibeface::peer=debug,vibeface::runtime=debug,vibeface::api=debug", "NO_COLOR": "1"}
    case = {"status": "running", "packaged_node_cases": {}, "core_boundary_evidence": [],
            "not_checked": ["packaged Node full private-cache workload", "other NET-1 lanes", "distributed source quotas"],
            "trigger": "cold Runtime member reconnect -> PeerHub.connect_target -> NetworkHub.dial/resolve -> Noise pinned to original B",
            "fixture_scope": "controlled public directory protocol, no upstream Publish or Reserve; actual R supplies the healthy signed record"}
    report["cases"]["directory_isolation"] = case
    directories, capture, directory_capture = [], None, None

    def directory_logs(node, offset, message):
        result = []
        with (node.directory / "process.log").open("rb") as stream:
            stream.seek(offset)
            for raw in stream:
                if not raw.endswith(b"\n"):
                    break
                line = re.sub(r"\x1b\[[0-9;]*m", "", raw.decode(errors="replace")).strip()
                if message not in line:
                    continue
                names = r"directory|target|outcome|cached|reason"
                fields = {key: value.strip().strip('"') for key, value in re.findall(
                    rf'\b({names})=(.*?)(?=\s+(?:{names})=|$)', line)}
                stamp = re.search(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z", line)
                result.append({"fields": fields, "wall_time": datetime.fromisoformat(stamp.group().replace("Z", "+00:00")).timestamp() if stamp else None,
                               "message": line})
        return result

    def completed_queries(node, offset):
        return directory_logs(node, offset, "directory query completed")

    def core_boundaries():
        require(evidence_dir is not None, "Lane 4 requires the frozen core WS boundary evidence directory")
        entries = []
        for slice_name, tests in (("d1", ["resolved_cache_is_private_bounded_and_full_cache_still_connects",
                                          "directory_stream_delivers_before_slow_reply_and_drop_releases_sockets",
                                          "records_expiring_while_waiting_for_state_are_not_stored_or_delivered"]),
                                  ("d2", ["publication_preserves_relay_changes_during_an_inflight_publish",
                                          "reservation_rejects_publish_and_public_full_publish_is_not_acknowledged",
                                          "relay_refusal_does_not_republish_and_duplicate_entries_do_not_duplicate_tasks",
                                          "directory_operations_have_one_deadline_across_welcome_and_reply"])):
            manifest_path = evidence_dir / f"m3-{slice_name}-core-slice.json"
            manifest = json.loads(manifest_path.read_text())
            require(all(check.get("exit_code") == 0 for check in manifest["checks"]), "referenced frozen core checks all passed")
            log = evidence_dir / f"m3-{slice_name}-network-final.log"
            text = log.read_text()
            require(all(f"test network::tests::{name} ... ok" in text for name in tests), "required source WS boundary cases have actual passed log lines")
            snapshot_path = repository / manifest["snapshot"]
            require(file_sha256(snapshot_path) == manifest["source"][0]["sha256"], "referenced production NetworkHub source snapshot matches its manifest")
            entries.append({"slice": slice_name, "manifest": str(manifest_path), "manifest_sha256": file_sha256(manifest_path),
                            "log": str(log), "log_sha256": file_sha256(log), "production_source": manifest["source"], "snapshot": str(snapshot_path),
                            "passed_cases": tests, "scope": "actual NetworkHub source and real WS boundary tests, with direct cache/table fixtures; not a full-cache workload in the packaged Node"})
        return entries

    def signed_short_record():
        result = subprocess.run(["node", str(repository / "scripts/crypto-client.mjs"), "--module", str(module)],
                                input=json.dumps({"op": "fixture_reachability", "identity_file": str(b.directory / "identity.json"),
                                                  "expected_peer_id": b.peer_id, "relay_urls": [unavailable.url], "ttl_secs": 2}) + "\n",
                                capture_output=True, text=True, timeout=15)
        require(result.returncode == 0, "temporary original target signs a short-lived reachability fixture")
        response = json.loads(result.stdout)
        require(response.get("ok"), "short-lived fixture signing succeeds without exposing private identity material")
        return response["result"]

    try:
        case["core_boundary_evidence"] = core_boundaries()
        for name in ("R", "A", "B", "D"):
            node = Node(run, name, binary, workspace, fixture, helper, module)
            node.configure(manager=None, codex_socket=str(run / "unavailable-codex.sock"))
            nodes.append(node)
        relay, a, b, directory = nodes
        relay.configure(roles=["relay"], network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False, "relay_enabled": True})
        capture = RelayCapture(relay.port, retain_payloads=False)
        relay.configure(allowed_origins=[capture.url])
        relay.start(env=trace_env)
        relay_url = capture.url.replace("http:", "ws:") + "/api/network/ws"
        directory_capture = RelayCapture(directory.port, retain_payloads=False)
        directory.configure(roles=["relay"], allowed_origins=[directory_capture.url],
                            network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False, "relay_enabled": False})
        directory.start(env=trace_env)
        directory_url = directory_capture.url.replace("http:", "ws:") + "/api/network/ws"
        healthy = NetworkDirectoryFixture("healthy")
        directories.append(healthy)
        healthy.record_factory = lambda target: copy.deepcopy(healthy.record) if target == b.peer_id else None
        a.configure(network={"bootstrap_urls": [healthy.url], "advertise_urls": [], "mdns": False, "relay_enabled": False})
        case["first_start_a_network"] = json.loads((a.directory / "config.json").read_text())["network"]
        b.configure(network={"bootstrap_urls": [relay_url, directory_url], "advertise_urls": [], "mdns": False, "relay_enabled": False})
        a.start(env=trace_env)
        network = a.bootstrap()
        b.pair(a, network)
        b.configure(roles=["gateway", "replica", "scheduler"])
        b.start(env=trace_env)

        def target_record():
            record = directory.client.command("network_probe", kind="record", url=directory.url, target=b.peer_id)
            require(record["direct_urls"] == [] and record["relay_urls"] == [relay_url], "healthy source provides the original B with only the actual relay path")
            return record

        healthy.record = wait_for("B reserves and signs its sole relay path", target_record)
        b.client.connect(b.peer_id)
        wait_for("Lane 4 B independent client authorization", lambda: b.client.authenticate())
        membership = a.client.call("directory")["state"]
        wait_for("Lane 4 source and target memberships converge", lambda: b.client.call("directory")["state"] == membership)

        def authenticated_target():
            return next((node for node in a.client.rpc("nodes")["nodes"]
                         if node["peer_id"] == b.peer_id and node["online"] and node.get("capabilities")), None)

        wait_for("setup discovers B using only the healthy directory", authenticated_target)
        a.stop()
        a.configure(roles=["gateway", "replica"])
        healthy.record = target_record()
        wrong_record = relay.client.command("network_probe", kind="record", url=relay.url, target=relay.peer_id)
        invalid_signature = copy.deepcopy(healthy.record)
        invalid_signature["signature"] = ("0" if invalid_signature["signature"][0] != "0" else "1") + invalid_signature["signature"][1:]
        slow = NetworkDirectoryFixture("slow", record_factory=lambda target: copy.deepcopy(healthy.record) if target == b.peer_id else None)
        wrong = NetworkDirectoryFixture("wrong-identity", record_factory=lambda target: copy.deepcopy(wrong_record) if target == b.peer_id else None)
        bad_signature = NetworkDirectoryFixture("bad-signature", record_factory=lambda target: copy.deepcopy(invalid_signature) if target == b.peer_id else None)
        unavailable = NetworkDirectoryFixture("unavailable-short-path")
        short = NetworkDirectoryFixture("short-expiry")
        short.record_factory = lambda target: copy.deepcopy(short.record) if target == b.peer_id else None
        directories.extend([slow, wrong, bad_signature, unavailable, short])
        for source in (healthy, slow, short):
            source.reply_gate.clear()
        sources = [short, wrong, bad_signature, slow, healthy]
        a.configure(network={"bootstrap_urls": [source.url for source in sources], "advertise_urls": [], "mdns": False, "relay_enabled": False})
        require(relay_url not in json.loads((a.directory / "config.json").read_text())["network"]["bootstrap_urls"], "cold A has no configured R relay URL")
        log_offset = (a.directory / "process.log").stat().st_size
        controls_offset = len(capture.controls)
        cold_start = {"wall_time": time.time(), "monotonic": time.monotonic(), "a_log_offset": log_offset,
                      "query_deadline_seconds": 10, "deadline_basis": "lower bound before spawning cold A; actual production queries start later"}
        case.update({"network_id": network, "target_peer_id": b.peer_id, "source_peer_id": a.peer_id,
                     "healthy_record": healthy.record, "wrong_identity_record_peer_id": wrong_record["peer_id"],
                     "cold_start": cold_start, "a_process_log": str(a.directory / "process.log"),
                     "configurations": {node.name: json.loads((node.directory / "config.json").read_text())["network"] for node in nodes}})
        a.start(env=trace_env)
        a.client.connect(a.peer_id)
        wait_for("cold A original management client authorization", lambda: a.client.authenticate())
        wait_for("slow directory really receives Lookup(B)", lambda: any(event["kind"] == "control_received" and event.get("control") == "lookup" and event.get("target") == b.peer_id for event in slow.snapshot()["events"]))
        wait_for("short directory really receives Lookup(B) before signing", lambda: any(event["kind"] == "control_received" and event.get("control") == "lookup" and event.get("target") == b.peer_id for event in short.snapshot()["events"]))
        short.record = signed_short_record()
        short.release()
        accepted_short = wait_for("production resolver accepts the still-valid short record", lambda: next((entry for entry in completed_queries(a, log_offset)
            if entry["fields"].get("directory") == short.url and entry["fields"].get("target") == b.peer_id and entry["fields"].get("outcome") == "record"), None))
        short_sent = next(event for event in short.snapshot()["events"] if event["kind"] == "record_send_started" and event.get("target") == b.peer_id)
        require(accepted_short["wall_time"] is not None and accepted_short["wall_time"] < short_sent["expires_at"], "short candidate is actually received while valid, not merely generated before expiry")
        wait_for("accepted short candidate really expires before healthy release", lambda: time.time() >= short_sent["expires_at"], timeout=5)
        rejected = {}
        for source, reason in ((wrong, "another node"), (bad_signature, "signature")):
            rejected[source.name] = wait_for(f"production rejects {source.name} independently", lambda source=source, reason=reason: next((entry for entry in completed_queries(a, log_offset)
                if entry["fields"].get("directory") == source.url and entry["fields"].get("target") == b.peer_id
                and entry["fields"].get("outcome") == "rejected" and reason in entry["fields"].get("reason", "").lower()), None))
        release = healthy.release()
        require(time.monotonic() < cold_start["monotonic"] + 10, "healthy response is released before the conservative production query deadline")
        target = wait_for("cold A authenticates original B before the unreleased slow query deadline", authenticated_target,
                          timeout=max(.1, cold_start["monotonic"] + 10 - time.monotonic()))
        query = network_peer_query(a, b, "net1-lane4-read-only")
        finished = {"wall_time": time.time(), "monotonic": time.monotonic()}
        require(query == {"native_sessions": []}, "fixed original B returns its authenticated read-only native status result")
        require(finished["monotonic"] < cold_start["monotonic"] + 10 and not slow.reply_gate.is_set(), "target Noise and query complete before the slow production query can time out or be released")
        require(not any(event["kind"] == "record_sent" for event in slow.snapshot()["events"]), "the slow directory has not supplied a candidate before target authentication")
        healthy_result = next(entry for entry in completed_queries(a, log_offset) if entry["fields"].get("directory") == healthy.url
                              and entry["fields"].get("target") == b.peer_id and entry["fields"].get("outcome") == "record")
        controls = list(capture.controls[controls_offset:])
        circuits = []
        for event in controls:
            if event.get("type") == "dial" and event.get("direction") == "to_relay" and event.get("target") == b.peer_id and event.get("service") == "peer":
                ready = next((value for value in controls if value["connection"] == event["connection"] and value.get("type") == "ready" and value["direction"] == "from_relay"), None)
                if ready and capture.traffic[event["connection"]]["binary_payload_bytes"]:
                    circuits.append({"dial": event, "ready": ready, **capture.traffic[event["connection"]]})
        require(circuits, "actual R carries the cold source's peer dial to the fixed B and binary Noise/application traffic")
        require(not invocations(run), "directory discovery and authorized queries never invoke a host fixture")
        verify_no_model_budget(nodes)
        require(file_sha256(binary) == report["binary_sha256"], "Lane 4 keeps one immutable binary")
        case["packaged_node_cases"]["D1_directory_isolation"] = {"status": "passed", "short_record_accepted": accepted_short,
            "short_record_send": short_sent, "source_rejections": rejected, "healthy_release": release,
            "healthy_candidate": healthy_result, "authenticated_target_capabilities": target, "original_target_query": query,
            "query_finished": finished, "slow_reply_released": False, "data_carrying_peer_circuits": circuits,
            "noise_evidence": "production connect_target pins original B; online remote capabilities and signed-origin B native status reply both succeed",
            "all_directory_results": completed_queries(a, log_offset)}
        slow_publication = NetworkDirectoryFixture("slow-publication")
        slow_publication.publish_gate.clear()
        directories.append(slow_publication)
        b.stop()
        b.configure(network={"bootstrap_urls": [relay_url, directory_url, slow_publication.url], "advertise_urls": [], "mdns": False, "relay_enabled": False})
        restart = {"wall_time": time.time(), "monotonic": time.monotonic(),
                   "b_log_offset": (b.directory / "process.log").stat().st_size,
                   "relay_control_offset": len(capture.controls), "directory_control_offset": len(directory_capture.controls)}
        b.start(env=trace_env)
        held_publish = wait_for("B's independent slow publication actually enters the unreleased fixture", lambda: next((event for event in slow_publication.snapshot()["events"]
            if event["kind"] == "control_received" and event.get("control") == "publish" and event.get("peer_id") == b.peer_id), None), timeout=5)
        d_publication = wait_for("B's fresh publication to actual directory D is acknowledged despite slow publication elsewhere",
            lambda: next((entry for entry in directory_logs(b, restart["b_log_offset"], "directory publication completed")
                          if entry["fields"].get("directory") == directory_url and entry["fields"].get("outcome") == "published"), None), timeout=5)
        wait_for("actual D still returns the original signed relay-only B record", target_record, timeout=5)
        ongoing_target = wait_for("B's actual relay reservation and Incoming/Attach remain usable during slow publication", authenticated_target,
                                 timeout=max(.1, restart["monotonic"] + 10 - time.monotonic()))
        ongoing_query = network_peer_query(a, b, "net1-lane4-d2-read-only")
        ongoing_finished = {"wall_time": time.time(), "monotonic": time.monotonic()}
        require(ongoing_query == {"native_sessions": []} and ongoing_finished["monotonic"] < restart["monotonic"] + 10,
                "fresh B target query completes before the slow publication's conservative total deadline")
        require(d_publication["wall_time"] is not None and d_publication["wall_time"] <= ongoing_finished["wall_time"],
                "fresh D publication ACK precedes the successful query while the slow publication remains blocked")
        require(not slow_publication.publish_gate.is_set()
                and not any(event["kind"] == "publish_acknowledged_locally" for event in slow_publication.snapshot()["events"]),
                "the slow publication has not replied while other relay work completes")
        b_reservations = [event for event in capture.controls[restart["relay_control_offset"]:]
                          if event.get("type") == "reserve" and event.get("peer_id") == b.peer_id]
        require(len(b_reservations) == 1, "one actual B reservation establishes the healthy relay during the held publication")
        reservation_connection = b_reservations[0]["connection"]
        wait_for("actual directory D receives a signed B publication", lambda: any(event.get("type") == "publish" and event.get("peer_id") == b.peer_id
                 for event in directory_capture.controls[restart["directory_control_offset"]:]), timeout=5)
        wait_for("startup relay-change publications settle across the first rejected directory retry", lambda: len([event for event in directory_capture.controls[restart["directory_control_offset"]:]
                 if event.get("type") == "reserve" and event.get("peer_id") == b.peer_id]) >= 2, timeout=10)
        stable = {"wall_time": time.time(), "monotonic": time.monotonic(), "directory_control_offset": len(directory_capture.controls),
                  "relay_control_offset": len(capture.controls),
                  "directory_log_offset": (directory.directory / "process.log").stat().st_size,
                  "reservation_heartbeat_baseline": dict(capture.traffic[reservation_connection])}
        progress = []

        def refused_without_republication():
            fresh = list(directory_capture.controls[stable["directory_control_offset"]:])
            reserves = [event for event in fresh if event.get("type") == "reserve" and event.get("peer_id") == b.peer_id]
            publications = [event for event in fresh if event.get("type") == "publish" and event.get("peer_id") == b.peer_id]
            require(not publications, "directory Reserve rejection does not drive a new B publication inside the stable pre-refresh window")
            require(not any(event.get("type") == "reserve" and event.get("peer_id") == b.peer_id for event in capture.controls[stable["relay_control_offset"]:]),
                    "the healthy B reservation remains the same during refused directory retries")
            value = network_peer_query(a, b, "net1-lane4-d2-read-only")
            require(value == {"native_sessions": []}, "original B remains queryable during repeated directory Reserve rejection")
            if not progress or progress[-1]["observed_reserve_refusals"] != len(reserves):
                progress.append({"wall_time": time.time(), "monotonic": time.monotonic(), "observed_reserve_refusals": len(reserves), "original_target_query": value})
            return reserves if len(reserves) >= 2 else None

        refusals = wait_for("two actual rejected directory Reserve retries without republishing", refused_without_republication, timeout=15)
        require(time.monotonic() - restart["monotonic"] < 60, "publication independence is measured before its periodic refresh")
        heartbeat = capture.traffic[reservation_connection]
        heartbeat_baseline = stable["reservation_heartbeat_baseline"]
        wait_for("the unchanged B relay reservation advances actual Ping/Pong after the stable-window baseline",
                 lambda: heartbeat["ping_frames"] > heartbeat_baseline["ping_frames"] and heartbeat["pong_frames"] > heartbeat_baseline["pong_frames"], timeout=25)
        failed_slow_publication = wait_for("the blocked publication has its own bounded timeout", lambda: next((entry for entry in directory_logs(b, restart["b_log_offset"], "directory publication completed")
            if entry["fields"].get("directory") == slow_publication.url and entry["fields"].get("outcome") == "rejected" and "timed out" in entry["fields"].get("reason", "")), None), timeout=15)
        refusals = refused_without_republication()
        require(refusals and time.monotonic() - restart["monotonic"] < 60, "the complete stable lifecycle observation remains before periodic refresh with the same relay reservation")
        with (directory.directory / "process.log").open("rb") as stream:
            stream.seek(stable["directory_log_offset"])
            directory_rejections = [re.sub(r"\x1b\[[0-9;]*m", "", line) for line in stream.read().decode(errors="replace").splitlines()
                                    if "public relay is disabled" in line]
        require(len(directory_rejections) >= len(refusals), "actual relay-disabled directory logs fresh stable-window reservation rejection instead of silently losing requests")
        case["packaged_node_cases"]["D2_publication_independence"] = {"status": "passed", "actual_directory": directory_url,
            "actual_directory_relay_enabled": False, "healthy_relay": relay_url, "b_restart": restart,
            "fresh_actual_D_publication": d_publication,
            "held_publication": held_publish, "slow_publication_released": False, "ongoing_original_target_query": ongoing_query,
            "ongoing_authenticated_target": ongoing_target, "ongoing_query_finished": ongoing_finished,
            "stable_observation": stable, "reserve_refusals": refusals, "query_progress": progress,
            "same_relay_reservation_connection": reservation_connection, "actual_heartbeat_frames": dict(heartbeat),
            "heartbeat_timing_scope": "same reservation counter growth after the stable-window baseline; may follow slow-publication timeout, not claimed to overlap its entire blocked period",
            "slow_publication_timeout": failed_slow_publication,
            "actual_D_reservation_rejections": directory_rejections,
            "actual_D_controls": list(directory_capture.controls[restart["directory_control_offset"]:]),
            "b_publication_results": directory_logs(b, restart["b_log_offset"], "directory publication completed")}
        require(not invocations(run), "D2 lifecycle and repeated target reads never invoke host work")
        verify_no_model_budget(nodes)
        require(file_sha256(binary) == report["binary_sha256"], "Lane 4 keeps the same frozen binary through D2")
        case["status"] = report["status"] = "passed"
        report["fixture_invocations"] = 0
        print(f"PASS NET-1 Lane 4 directory isolation and independent publication: {run / 'report.json'}", flush=True)
    except Exception:
        case["status"] = "failed"
        raise
    finally:
        cleanup_errors = []
        for node in reversed(nodes):
            try:
                node.stop()
            except Exception as error:
                cleanup_errors.append(f"{node.name}: {type(error).__name__}")
        for source in directories:
            try:
                source.close()
            except Exception as error:
                cleanup_errors.append(f"{source.name}: {type(error).__name__}")
        case["directory_fixtures"] = [source.snapshot() for source in directories]
        if capture:
            try:
                capture.close()
            except Exception as error:
                cleanup_errors.append(f"capture: {type(error).__name__}")
        if directory_capture:
            try:
                directory_capture.close()
            except Exception as error:
                cleanup_errors.append(f"directory capture: {type(error).__name__}")
        case["cleanup"] = {"errors": cleanup_errors,
                           "directory_active_sockets": sum(source.snapshot()["active_sockets"] for source in directories)}
        require(not cleanup_errors and case["cleanup"]["directory_active_sockets"] == 0, "Lane 4 reclaims all owned process and fixture resources")
        require(not any(source.errors for source in directories), "bounded directory fixtures complete without protocol errors")
        require(not any(source.rejected_connections for source in directories), "fixture capacity does not confound production directory results")
        require(capture is None or not capture.errors, "Lane 4 relay capture remains valid")
        require(directory_capture is None or not directory_capture.errors, "Lane 4 actual-directory capture remains valid")


def network_reliability_mdns(run, binary, workspace, fixture, helper, module, report, nodes):
    trace_env = {**os.environ, "RUST_LOG": "vibeface::network=debug,vibeface::peer=debug,vibeface::runtime=debug,vibeface::api=debug", "NO_COLOR": "1"}
    interfaces = network_mdns_interfaces()
    interface, address = interfaces[0]["name"], interfaces[0]["ipv4"][0]
    case = {"status": "running", "interfaces_observed_read_only": interfaces, "selected_interface": interface,
            "advertisements": {}, "source_lifecycle": {},
            "not_checked": ["actual interface IP add/delete", "IPv6 listener advertisement", "other host interfaces beyond the selected DNS-SD interface"],
            "fixture_scope": "task-owned DNS-SD registrations and Node data directories; existing host interfaces are only observed"}
    report["cases"]["mdns_lifecycle"] = case
    owned_nodes, observers, captures, clients = [], [], [], []

    def node(name):
        value = Node(run, name, binary, workspace, fixture, helper, module)
        value.configure(manager=None, codex_socket=str(run / "unused-codex.sock"),
                        network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False, "relay_enabled": False})
        nodes.append(value)
        owned_nodes.append(value)
        return value

    def observe(name, arguments):
        value = NetworkDnsSd(run, name, interface, arguments)
        observers.append(value)
        return value

    def resolution(name, instance, port, target):
        value = observe(name, ["-L", instance, "_vibeface._tcp", "local"])
        result = wait_for("actual DNS-SD fullname, SRV port and complete TXT identity",
                          lambda: next((item for item in value.resolutions()
                                        if item["port"] == port and any(f"peer_id={target}" in line for line in item["txt_lines"])), None))
        return {"instance": instance, **result}

    def source_state(value):
        sources, epoch = {}, None
        entries = [entry for entry in network_trace_records(value, messages=["mdns source changed", "mdns daemon reset"])
                   if "mdns daemon reset" in entry["message"] or entry["fields"].get("target") == b.peer_id]
        for entry in entries:
            fields = entry["fields"]
            actual_epoch = int(fields["epoch"])
            if epoch is not None and actual_epoch < epoch:
                continue
            if epoch is None or actual_epoch > epoch:
                sources.clear()
                epoch = actual_epoch
            if "mdns daemon reset" in entry["message"]:
                sources.clear()
            elif fields.get("event") == "resolved":
                sources[network_dns_name(fields["fullname"])] = entry
            elif fields.get("event") == "removed":
                sources.pop(network_dns_name(fields["fullname"]), None)
        return {"epoch": epoch, "sources": sources, "entries": entries}

    try:
        browse = observe("mdns-browse", ["-B", "_vibeface._tcp", "local"])
        for name, wildcard in (("mdns-fixed", False), ("mdns-wildcard", True)):
            target = node(name)
            expected_id = target.peer_id
            target.configure(roles=["relay"], network={"bootstrap_urls": [], "advertise_urls": [], "mdns": True, "relay_enabled": False})
            target.set_url(address, listen=f"0.0.0.0:{target.port}" if wildcard else None)
            target.start(env=trace_env)
            require(target.peer_id == expected_id, "HTTP listener retains the initialized temporary Node identity")
            advertisement = wait_for("actual Node DNS-SD service advertisement", lambda: next(
                (item for item in browse.browse_records() if item["action"] == "Add" and item["instance"].startswith(expected_id[:32])), None))
            resolved = resolution(name + "-resolve", advertisement["instance"], target.port, expected_id)
            addresses = observe(name + "-a", ["-Q", resolved["hostname"], "A", "IN"])
            ipv6 = observe(name + "-aaaa", ["-Q", resolved["hostname"], "AAAA", "IN"])
            require(ipv6.process.poll() is None, "AAAA observer is running before its observation window")
            records = wait_for("independent DNS-SD A records", lambda: addresses.addresses(resolved["hostname"]))
            observed = sorted({item["address"] for item in records})
            assigned = {item for value in interfaces for item in value["ipv4"]}
            require((set(observed) <= assigned if wildcard else observed == [address]),
                    "advertised IPv4 addresses match the real wildcard or fixed listener")
            accepted = []
            for advertised in observed:
                client = Client(f"http://{advertised}:{target.port}", helper, module)
                clients.append(client)
                info = client.request("/api/info")
                require(info["peer_id"] == expected_id, "each observed address reaches the original HTTP Node")
                client.connect(expected_id)
                encrypted = client.call("info")
                require(encrypted["peer_id"] == expected_id, "each observed address completes pinned Noise and encrypted info")
                accepted.append({"address": advertised, "http_peer_id": info["peer_id"], "encrypted_peer_id": encrypted["peer_id"]})
                client.close()
            time.sleep(1)
            require(ipv6.process.poll() is None and "DNSServiceQueryRecord failed" not in ipv6.text(),
                    "empty AAAA observation is backed by a live query without a command error")
            require(not ipv6.addresses(resolved["hostname"]), "IPv4 listener has no observed AAAA advertisement during the positive A observation")
            registered = wait_for("Node reports its actual mDNS registration", lambda: network_trace_records(target, messages=["mdns service registered"]))
            require(registered[-1]["fields"]["addr_auto"] == str(wildcard).lower(), "fixed and wildcard address automation match listener configuration")
            case["advertisements"][name] = {"peer_id": expected_id, "listen": json.loads((target.directory / "config.json").read_text())["listen"],
                                           "browse": advertisement, "resolve": resolved, "a_records": records,
                                           "aaaa_records": [], "aaaa_observer": ipv6.snapshot(),
                                           "accepted_addresses": accepted, "registration": registered[-1],
                                           "scope": "the selected existing IPv4 interface; empty AAAA is a bounded observation with a positive A control"}
            target.stop()

        a, b = node("mdns-A"), node("mdns-B")
        a.configure(roles=["gateway", "replica"], network={"bootstrap_urls": [], "advertise_urls": [], "mdns": True, "relay_enabled": False})
        a.start(env=trace_env)
        network = a.bootstrap()
        b.pair(a, network)
        b.configure(roles=["gateway", "replica", "scheduler"])
        b.set_url(address)
        proxy = RelayCapture(b.port, retain_payloads=False, listen_host=address, upstream_host=address)
        captures.append(proxy)
        b.configure(allowed_origins=[proxy.url])
        b.start(env=trace_env)
        b.client.connect(b.peer_id)
        instance = b.peer_id[:32]
        registrations = {b.port: observe("mdns-register-first", ["-R", instance, "_vibeface._tcp", "local", b.port,
                                                                 f"peer_id={b.peer_id}", "path=/api/network/ws"])}
        first = wait_for("first real DNS-SD registration", lambda: next(
            (item for item in browse.browse_records() if item["action"] == "Add" and item["instance"] == instance), None))
        first_resolution = resolution("mdns-first-resolve", first["instance"], b.port, b.peer_id)
        registrations[proxy.port] = observe("mdns-register-second", ["-R", instance, "_vibeface._tcp", "local", proxy.port,
                                                                    f"peer_id={b.peer_id}", "path=/api/network/ws"])
        conflict = wait_for("actual DNS-SD conflict rename", lambda: next(
            (item for item in browse.browse_records() if item["action"] == "Add" and item["instance"].startswith(instance)
             and item["instance"] != first["instance"]), None))
        conflict_lookup = observe("mdns-conflict-resolve", ["-L", conflict["instance"], "_vibeface._tcp", "local"])
        renamed = wait_for("actual renamed instance resolves to one of the two owned listener ports", lambda: next(
            (item for item in conflict_lookup.resolutions() if item["port"] in registrations and any(f"peer_id={b.peer_id}" in line for line in item["txt_lines"])), None))
        require(renamed["canonical_fullname"] != first_resolution["canonical_fullname"], "actual conflict fullname differs from the original registration")
        remaining_port = next(port for port in registrations if port != renamed["port"])
        current_original = resolution("mdns-original-after-conflict", first["instance"], remaining_port, b.peer_id)
        observed_sources = wait_for("both actual fullnames contribute to the original target", lambda: (
            state if len([entry for entry in state["sources"].values() if entry["fields"].get("target") == b.peer_id]) == 2 else None)
            if (state := source_state(a)) else None)
        target_sources = {key: entry for key, entry in observed_sources["sources"].items() if entry["fields"]["target"] == b.peer_id}
        require(renamed["canonical_fullname"] in target_sources and first_resolution["canonical_fullname"] in target_sources,
                "Node source keys match the independently observed actual DNS-SD fullnames")
        wait_for("mDNS target's previously paired client grant", lambda: b.client.authenticate(network))
        verify_offset = (a.directory / "process.log").stat().st_size
        verified = wait_for("new real DNS cache verify of the renamed instance", lambda: next(
            (entry for entry in network_trace_records(a, verify_offset, messages=["mdns source verify"])
             if entry["fields"].get("target") == b.peer_id
             and int(entry["fields"]["epoch"]) == observed_sources["epoch"]
             and network_dns_name(entry["fields"]["fullname"]) == renamed["canonical_fullname"]), None))
        time.sleep(min(int(verified["fields"]["timeout_ms"]) / 1000 + .2, 5))
        healthy = source_state(a)
        verify_changes = network_trace_records(a, verify_offset, messages=["mdns source changed", "mdns daemon reset"])
        require(not any("mdns daemon reset" in entry["message"] or (
            entry["fields"].get("target") == b.peer_id and entry["fields"].get("event") == "removed"
            and int(entry["fields"]["epoch"]) == observed_sources["epoch"]
            and network_dns_name(entry["fields"]["fullname"]) in target_sources) for entry in verify_changes),
                "neither actual target source is removed or reset during the new verify window")
        require(healthy["epoch"] == observed_sources["epoch"] and all(key in healthy["sources"] for key in target_sources),
                "both healthy sources survive the real verify timeout in the same daemon epoch")
        resolved_in_window = [entry for entry in verify_changes if entry["fields"].get("target") == b.peer_id
                              and entry["fields"].get("event") == "resolved"]
        before = network_peer_query(a, b, "net1-lane7-mdns-read-only")
        require(before == {"native_sessions": []}, "original target answers a real read before source removal")
        removal_offset = (a.directory / "process.log").stat().st_size
        removal_browse_line = len(browse.text().splitlines())
        registrations[renamed["port"]].close()
        removed = wait_for("browser observes actual renamed instance removal", lambda: next(
            (item for item in browse.browse_records() if item["line"] > removal_browse_line
             and item["action"] == "Rmv" and item["canonical_fullname"] == renamed["canonical_fullname"]), None))
        survivor = wait_for("Node removes exactly the actual conflict fullname while retaining its other source", lambda: (
            state if state["epoch"] == observed_sources["epoch"] and renamed["canonical_fullname"] not in state["sources"]
            and len([entry for entry in state["sources"].values() if entry["fields"].get("target") == b.peer_id]) == 1 else None)
            if (state := source_state(a)) else None)
        surviving = next(entry for entry in survivor["sources"].values() if entry["fields"].get("target") == b.peer_id)
        require(any(entry["fields"].get("event") == "removed" and entry["fields"].get("target") == b.peer_id
                    and entry["log_offset"] >= removal_offset
                    and int(entry["fields"]["epoch"]) == observed_sources["epoch"]
                    and network_dns_name(entry["fields"]["fullname"]) == renamed["canonical_fullname"] for entry in survivor["entries"]),
                "actual Node removal log identifies the same target, fullname and daemon epoch")
        old_link = wait_for("original target has an installed peer link before its restart", lambda: next(
            (entry for entry in reversed(network_trace_records(a, messages=["peer link ready"]))
             if entry["fields"].get("network") == network and entry["fields"].get("peer") == b.peer_id), None))
        restart_offset = (a.directory / "process.log").stat().st_size
        b.stop()
        old_removed = wait_for("the current original peer link is actually removed", lambda: next(
            (entry for entry in network_trace_records(a, restart_offset, messages=["peer link removed"])
             if entry["fields"].get("network") == network and entry["fields"].get("peer") == b.peer_id
             and entry["fields"].get("link_id") == old_link["fields"]["link_id"]
             and entry["fields"].get("current") == "true"), None))
        b.start(env=trace_env)
        read_offset = (a.directory / "process.log").stat().st_size
        target_read_offset = (b.directory / "process.log").stat().st_size
        after = network_peer_query(a, b, "net1-lane7-mdns-read-only")
        read_end = (a.directory / "process.log").stat().st_size
        require(after == {"native_sessions": []}, "original target remains readable after its peer connection and one discovery source are removed")
        surviving_urls = json.loads(surviving["fields"]["urls"])
        selected = wait_for("a new authenticated path uses an actual surviving mDNS endpoint", lambda: next(
            (entry for entry in network_trace_records(a, restart_offset, messages=["authenticated path selected"])
             if entry["fields"].get("target") == b.peer_id
             and entry["log_end"] <= read_end
             and entry["fields"].get("route") in ["direct:" + url for url in surviving_urls]), None))
        new_link = wait_for("the restarted original target installs a distinct peer link", lambda: next(
            (entry for entry in network_trace_records(a, restart_offset, messages=["peer link ready"])
             if entry["fields"].get("network") == network and entry["fields"].get("peer") == b.peer_id
             and entry["fields"].get("link_id") != old_link["fields"]["link_id"]
             and entry["fields"].get("outbound") == "true" and entry["log_end"] <= read_end
             and entry["log_offset"] > selected["log_offset"]), None))
        read_evidence = network_peer_read_evidence(a, b, network, "net1-lane7-mdns-read-only", read_offset, target_read_offset,
                                                   next_hop=b.peer_id, link_id=new_link["fields"]["link_id"], route_kind="direct")
        require(source_state(a)["epoch"] == observed_sources["epoch"], "reconnecting the target does not reset the observer's mDNS epoch")
        last_removal_offset = (a.directory / "process.log").stat().st_size
        last_removal_browse_line = len(browse.text().splitlines())
        registrations[remaining_port].close()
        remaining_fullname = network_dns_name(surviving["fields"]["fullname"])
        last_browser_removed = wait_for("browser observes removal of the exact final fullname", lambda: next(
            (item for item in browse.browse_records() if item["line"] > last_removal_browse_line and item["action"] == "Rmv"
             and item["canonical_fullname"] == remaining_fullname), None))
        empty = wait_for("last actual source is removed from the target", lambda: (
            state if state["epoch"] == observed_sources["epoch"] and not state["sources"]
            and any(entry["fields"].get("event") == "removed" and entry["fields"].get("target") == b.peer_id
                    and entry["log_offset"] >= last_removal_offset
                    and int(entry["fields"]["epoch"]) == observed_sources["epoch"]
                    and network_dns_name(entry["fields"]["fullname"]) == remaining_fullname for entry in state["entries"]) else None)
            if (state := source_state(a)) else None)
        registration_mapping = [{"port": port, "registration": registration.snapshot(),
                                 "resolved_fullname": renamed["canonical_fullname"] if port == renamed["port"] else remaining_fullname}
                                for port, registration in registrations.items()]
        case["source_lifecycle"] = {"target": b.peer_id, "network": network, "first_resolution": first_resolution,
                                    "original_resolution_after_conflict": current_original,
                                    "actual_conflict": {"browse": conflict, "resolve": renamed}, "initial_sources": target_sources,
                                    "owned_registration_mapping": registration_mapping,
                                    "epoch": observed_sources["epoch"], "verify": verified, "verify_log_offset": verify_offset,
                                    "verify_window_resolved_count": len(resolved_in_window), "verify_window_changes": verify_changes,
                                    "after_verify": healthy,
                                    "removal_log_offset": removal_offset, "removal_browse_line": removal_browse_line,
                                    "browser_removed": removed, "after_exact_removal": survivor, "surviving_source": surviving,
                                    "read_before": before, "read_after_target_restart": after,
                                    "target_restart": {"log_offset": restart_offset, "old_link": old_link,
                                                       "old_removed": old_removed, "selected_path": selected, "new_link": new_link,
                                                       "read_log_offset": read_offset, "read_log_end": read_end,
                                                       "target_read_log_offset": target_read_offset, "read_evidence": read_evidence},
                                    "last_removal_log_offset": last_removal_offset, "last_removal_browse_line": last_removal_browse_line,
                                    "last_browser_removed": last_browser_removed, "after_last_removal": empty}
        require(not invocations(run), "mDNS lifecycle uses no host fixture or paid model")
        case["status"] = "passed"
    except BaseException as error:
        case["status"] = "failed"
        case["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        original_error = sys.exc_info()[1]
        errors = []
        for kind, values in (("dns_sd", observers), ("client", clients), ("node_client", [value.client for value in owned_nodes]),
                             ("node", owned_nodes), ("capture", captures)):
            for value in reversed(values):
                try:
                    value.stop() if kind == "node" else value.close()
                except Exception as error:
                    errors.append({"kind": kind, "error_type": type(error).__name__})
        ports = []
        endpoints = [(value.name, urllib.parse.urlsplit(value.url).hostname, value.port) for value in owned_nodes]
        endpoints += [("capture", urllib.parse.urlsplit(value.url).hostname, value.port) for value in captures]
        for name, host, port in endpoints:
            try:
                with socket.socket() as probe:
                    probe.settimeout(.3)
                    ports.append({"name": name, "host": host, "port": port, "closed": probe.connect_ex((host, port)) != 0})
            except Exception as error:
                errors.append({"kind": "port_probe", "name": name, "error_type": type(error).__name__})
        case["dns_sd"] = []
        for value in observers:
            try:
                case["dns_sd"].append(value.snapshot())
            except Exception as error:
                errors.append({"kind": "dns_sd_snapshot", "error_type": type(error).__name__})
        case["cleanup"] = {"errors": errors, "ports": ports, "remaining_owned_nodes": sum(value.process is not None for value in owned_nodes),
                           "remaining_owned_clients": sum(value.process is not None for value in [*clients, *[node.client for node in owned_nodes]])}
        if errors or not all(item["closed"] for item in ports) or any(value["process_running"] for value in case["dns_sd"]) or case["cleanup"]["remaining_owned_nodes"] or case["cleanup"]["remaining_owned_clients"]:
            case["status"] = "failed"
        if original_error is None:
            require(case["status"] == "passed", "mDNS lifecycle and all owned process cleanup passed")


def network_reliability_routes(run, binary, workspace, fixture, helper, module, report, nodes):
    run = run / "route-lifecycle"
    run.mkdir(mode=0o700)
    trace_env = {**os.environ, "RUST_LOG": "vibeface::network=debug,vibeface::peer=debug,vibeface::runtime=debug,vibeface::api=debug", "NO_COLOR": "1"}
    case = {"status": "running", "fixture_scope": "four isolated Nodes; original signed directory publications; one disabled next-hop proxy; read-only native status queries"}
    report["cases"]["route_lifecycle"] = case
    owned_nodes, directories, captures = [], [], {}
    allowed, by_target = {}, {}
    adjacency_lock = threading.Lock()

    def record_for(owner, target):
        with adjacency_lock:
            permitted = target in allowed.get(owner, set())
        return by_target[target].published(target) if permitted and target in by_target else None

    def links(value):
        current = {}
        for entry in network_trace_records(value, messages=["peer link ready", "peer link removed"]):
            fields = entry["fields"]
            if fields.get("network") != network:
                continue
            peer = fields["peer"]
            if "peer link ready" in entry["message"]:
                current[peer] = entry
            elif peer in current and current[peer]["fields"]["link_id"] == fields["link_id"]:
                current.pop(peer)
        return current

    def online_snapshot():
        snapshot = a.client.rpc("nodes")
        by_id = {item["peer_id"]: item for item in snapshot["nodes"]}
        return snapshot if t.peer_id in by_id and not by_id[t.peer_id]["online"] and by_id.get(u.peer_id, {}).get("online") else None

    try:
        for name in ("route-A", "route-M", "route-T", "route-U"):
            value = Node(run, name, binary, workspace, fixture, helper, module)
            nodes.append(value)
            owned_nodes.append(value)
            directory = NetworkDirectoryFixture(name + "-directory", retain_publications=True,
                                                record_factory=lambda target, owner=value.peer_id: record_for(owner, target))
            directories.append(directory)
            by_target[value.peer_id] = directory
            advertise_urls = []
            if name != "route-A":
                capture = RelayCapture(value.port, retain_payloads=False)
                captures[value.peer_id] = capture
                advertise_urls = [capture.url.replace("http:", "ws:") + "/api/network/ws"]
                value.configure(allowed_origins=[capture.url])
            value.configure(manager=None, codex_socket=str(run / "unused-codex.sock"),
                            network={"bootstrap_urls": [directory.url], "advertise_urls": advertise_urls,
                                     "mdns": False, "relay_enabled": False})
        a, middle, t, u = owned_nodes
        with adjacency_lock:
            allowed.update({a.peer_id: {middle.peer_id, u.peer_id}, middle.peer_id: {t.peer_id}, t.peer_id: set(), u.peer_id: set()})
        a.configure(roles=["gateway", "replica"])
        a.start(env=trace_env)
        network = a.bootstrap()
        for value in (middle, t, u):
            value.pair(a, network)
            value.client.close()
            value.configure(roles=["gateway", "replica", "relay"] if value is middle else ["gateway", "replica", "scheduler"])
        for value in (t, u, middle):
            value.start(env=trace_env)
        publications = {}
        for value in (middle, t, u):
            publication = wait_for("real signed target publication at its own isolated directory",
                                   lambda value=value: by_target[value.peer_id].published(value.peer_id))
            require(publication["peer_id"] == value.peer_id
                    and publication["direct_urls"] == [captures[value.peer_id].url.replace("http:", "ws:") + "/api/network/ws"]
                    and not publication["relay_urls"] and publication["expires_at"] > time.time(),
                    "directory returns the unmodified original target's live direct-only publication")
            publications[value.peer_id] = publication
        initial_a_links = wait_for("A installs only its two allowed neighbor links", lambda: (
            state if middle.peer_id in state and u.peer_id in state and t.peer_id not in state else None)
            if (state := links(a)) else None)
        initial_m_links = wait_for("M installs its independent target link", lambda: (
            state if t.peer_id in state else None) if (state := links(middle)) else None)
        presence = wait_for("the original target is reached through the authenticated M link generation", lambda: next(
            (entry for entry in network_trace_records(a, messages=["peer presence route observed"])
             if entry["fields"].get("network") == network and entry["fields"].get("target") == t.peer_id
             and entry["fields"].get("neighbor") == middle.peer_id
             and entry["fields"].get("link_id") == initial_a_links[middle.peer_id]["fields"]["link_id"]), None))
        initial_request_offset = (a.directory / "process.log").stat().st_size
        initial_target_offset = (t.directory / "process.log").stat().st_size
        initial_query = network_peer_query(a, t, "net1-lane7-via-middle-read-only")
        require(initial_query == {"native_sessions": []}, "the fixed target answers the initial read across its live intermediate neighbor")
        initial_read = network_peer_read_evidence(a, t, network, "net1-lane7-via-middle-read-only", initial_request_offset, initial_target_offset,
                                                  next_hop=middle.peer_id, link_id=initial_a_links[middle.peer_id]["fields"]["link_id"], route_kind="presence")
        require(t.peer_id not in links(a), "the initial target read has no direct A-T peer link")
        cut_offset = (a.directory / "process.log").stat().st_size
        middle_cut_offset = (middle.directory / "process.log").stat().st_size
        cut = captures[middle.peer_id].pause()
        removed = wait_for("A revokes the exact current M link and its learned routes", lambda: next(
            (entry for entry in network_trace_records(a, cut_offset, messages=["peer link removed"])
             if entry["fields"].get("network") == network and entry["fields"].get("peer") == middle.peer_id
             and entry["fields"].get("link_id") == initial_a_links[middle.peer_id]["fields"]["link_id"]
             and entry["fields"].get("current") == "true" and int(entry["fields"]["removed_routes"]) > 0), None))
        offline = wait_for("the target loses indirect online status while unrelated U stays online", online_snapshot)
        require(links(a)[u.peer_id]["fields"]["link_id"] == initial_a_links[u.peer_id]["fields"]["link_id"]
                and links(middle)[t.peer_id]["fields"]["link_id"] == initial_m_links[t.peer_id]["fields"]["link_id"],
                "cutting only the M listener preserves the existing A-U and M-T link generations")
        recovery_directory = by_target[a.peer_id]
        recovery_directory.reply_gate.clear()
        gate_started = time.monotonic()
        request_offset = (a.directory / "process.log").stat().st_size
        target_offset = (t.directory / "process.log").stat().st_size
        with adjacency_lock:
            allowed[a.peer_id].add(t.peer_id)
        query_label = "net1-lane7-original-target-query"
        with ThreadPoolExecutor(max_workers=1) as pool:
            request = pool.submit(network_peer_query, a, t, query_label)
            try:
                on_demand = wait_for("the single read starts or joins the target's shared connection flight", lambda: next(
                    (entry for entry in network_trace_records(a, request_offset, messages=["peer route flight started", "peer route flight joined"])
                     if entry["fields"].get("network") == network and entry["fields"].get("target") == t.peer_id
                     and entry["fields"].get("trigger") == "on_demand"
                     and entry["fields"].get("request_type") == "native_status"), None), timeout=8)
            finally:
                gate_released = recovery_directory.release()
            recovered = request.result(timeout=25)
        request_end = (a.directory / "process.log").stat().st_size
        require(recovered == {"native_sessions": []}, "the single new read returns the original target's native status result")
        restored = wait_for("the original target installs a new direct link within the single read", lambda: next(
            (entry for entry in network_trace_records(a, request_offset, messages=["peer link ready"])
             if entry["fields"].get("network") == network and entry["fields"].get("peer") == t.peer_id
             and entry["fields"].get("outbound") == "true" and entry["log_end"] <= request_end), None))
        ready = wait_for("the same shared flight reports the installed target link ready", lambda: next(
            (entry for entry in network_trace_records(a, request_offset, messages=["peer route flight ready"])
             if entry["fields"].get("network") == network and entry["fields"].get("target") == t.peer_id
             and entry["fields"].get("flight_id") == on_demand["fields"]["flight_id"]
             and entry["fields"].get("link_id") == restored["fields"]["link_id"] and entry["fields"].get("state") == "ready"), None))
        selected = wait_for("the alternate path authenticates the actual original target endpoint", lambda: next(
            (entry for entry in network_trace_records(a, request_offset, messages=["authenticated path selected"])
             if entry["fields"].get("target") == t.peer_id
             and entry["fields"].get("route") == "direct:" + captures[t.peer_id].url.replace("http:", "ws:") + "/api/network/ws"
             and entry["log_end"] <= restored["log_offset"]), None))
        recovered_read = network_peer_read_evidence(a, t, network, query_label, request_offset, target_offset,
                                                    next_hop=t.peer_id, link_id=restored["fields"]["link_id"], route_kind="direct")
        require(middle.peer_id not in links(a), "the disabled original next hop is not silently restored")
        require(links(a)[u.peer_id]["fields"]["link_id"] == initial_a_links[u.peer_id]["fields"]["link_id"]
                and links(middle)[t.peer_id]["fields"]["link_id"] == initial_m_links[t.peer_id]["fields"]["link_id"],
                "alternate recovery leaves both unaffected link generations intact")
        case.update(network=network, targets={value.name: value.peer_id for value in owned_nodes}, publications=publications,
                    initial_a_links=initial_a_links, initial_m_links=initial_m_links, presence=presence,
                    initial_query=initial_query, initial_read_evidence=initial_read,
                    cut=cut, cut_log_offset=cut_offset, middle_cut_log_offset=middle_cut_offset,
                    removed=removed, offline_snapshot=offline, observation=query_label, recovered=recovered,
                    request_log_offset=request_offset, target_log_offset=target_offset, request_log_end=request_end,
                    restored_link=restored, recovered_read_evidence=recovered_read,
                    recovery_flight={"on_demand": on_demand, "ready": ready, "selected_path": selected,
                                     "directory_gate_started_monotonic": gate_started, "directory_gate_released": gate_released},
                    unaffected_links_after_recovery={"A_to_U": links(a)[u.peer_id], "M_to_T": links(middle)[t.peer_id]})
        require(not invocations(run.parent), "route recovery invokes no host work or paid model")
        case["status"] = "passed"
    except BaseException as error:
        case["status"] = "failed"
        case["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        original_error = sys.exc_info()[1]
        errors = []
        for capture in captures.values():
            try:
                capture.pause()
            except Exception as error:
                errors.append({"kind": "capture_pause", "error_type": type(error).__name__})
        for kind, values in (("client", [value.client for value in owned_nodes]), ("node", owned_nodes),
                             ("directory", directories), ("capture", list(captures.values()))):
            for value in values if kind == "node" else reversed(values):
                try:
                    value.stop() if kind == "node" else value.close()
                except Exception as error:
                    errors.append({"kind": kind, "error_type": type(error).__name__})
        case["directory_fixtures"] = []
        for directory in directories:
            try:
                case["directory_fixtures"].append(directory.snapshot())
            except Exception as error:
                errors.append({"kind": "directory_snapshot", "error_type": type(error).__name__})
        endpoints = [(value.name, value.port) for value in owned_nodes]
        endpoints += [(directory.name, directory.port) for directory in directories]
        endpoints += [("capture", capture.port) for capture in captures.values()]
        ports = []
        for name, port in endpoints:
            try:
                with socket.socket() as probe:
                    probe.settimeout(.3)
                    ports.append({"name": name, "port": port, "closed": probe.connect_ex(("127.0.0.1", port)) != 0})
            except Exception as error:
                errors.append({"kind": "port_probe", "name": name, "error_type": type(error).__name__})
        case["cleanup"] = {"errors": errors, "ports": ports,
                           "remaining_owned_nodes": sum(value.process is not None for value in owned_nodes),
                           "remaining_owned_clients": sum(value.client.process is not None for value in owned_nodes)}
        if errors or not all(item["closed"] for item in ports) or any(value["active_sockets"] or value["errors"] or value["fixture_capacity_rejections"] for value in case["directory_fixtures"]) or any(capture.errors for capture in captures.values()) or case["cleanup"]["remaining_owned_nodes"] or case["cleanup"]["remaining_owned_clients"]:
            case["status"] = "failed"
        if original_error is None:
            require(case["status"] == "passed", "route lifecycle and all owned process cleanup passed")


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
    lane = options.net1_lane
    baseline_report = None
    run = artifacts / ({1: "lane-1-noop-", 2: "lane-2-resume-", 3: "lane-3-closure-", 4: "lane-4-directory-", 5: "lane-5-generations-", 6: "lane-6-budgets-", 7: "lane-7-routes-"}.get(lane, "m0-baseline-") + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    fixture = fixture_wrapper(run, "fixture-manager", "--fixture-manager")
    module = options.crypto_module.resolve(strict=True)
    require(module.with_name("vibeface_secure_bg.wasm").read_bytes().startswith(b"\x00asm"),
            "NET-1 uses the real browser cryptography WASM")
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    helper.chmod(0o600)
    report = {"status": "running", "mode": f"network-reliability-lane-{lane}" if lane else "network-reliability-m0",
              "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repository, text=True).strip(),
              "binary_sha256": file_sha256(binary), "artifacts": str(run),
              "command": [sys.executable, *sys.argv], "provider_calls": 0, "cases": {},
              "not_checked": ["mixed resolver replies and full resolver cache", "slow consumers and source quotas",
                              "attachment crash recovery and 64 MiB completion", "rendered browser UI", "production networks"]}
    nodes, capture = [], None
    try:
        if lane == 7:
            report["tracked_worktree_dirty"] = subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=repository).returncode != 0
            report["tracked_diff_sha256"] = hashlib.sha256(subprocess.check_output(["git", "diff", "HEAD", "--", "crates/node", "scripts/verify-node.py", "scripts/crypto-client.mjs"], cwd=repository)).hexdigest()
            report["verification_scripts"] = {name: file_sha256(repository / "scripts" / name) for name in ("verify-node.py", "crypto-client.mjs")}
            report["not_checked"] = ["actual host interface IP add/delete", "IPv6 listener advertisement", "other NET-1 lanes", "rendered browser UI", "production networks"]
            network_reliability_mdns(run, binary, workspace, fixture, helper, module, report, nodes)
            network_reliability_routes(run, binary, workspace, fixture, helper, module, report, nodes)
            require(all(case["status"] == "passed" for case in report["cases"].values()), "both mDNS and route lifecycle cases and their cleanup pass")
            require(file_sha256(binary) == report["binary_sha256"], "Lane 7 uses one unchanged frozen binary")
            require(all(file_sha256(repository / "scripts" / name) == digest for name, digest in report["verification_scripts"].items()), "Lane 7 uses unchanged verification scripts")
            require(not invocations(run), "Lane 7 performs only read-only peer queries without host work or model")
            verify_no_model_budget(nodes)
            report["status"] = "passed"
            report["fixture_invocations"] = 0
            return
        if lane in (5, 6):
            report["tracked_worktree_dirty"] = subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=repository).returncode != 0
            report["not_checked"] = ["other NET-1 lanes", "rendered browser UI", "production networks"]
            (network_reliability_reservation if lane == 5 else network_reliability_budgets)(run, binary, workspace, fixture, helper, module, report, nodes, options.evidence_dir)
            return
        if lane == 4:
            report["tracked_worktree_dirty"] = subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=repository).returncode != 0
            report["not_checked"] = ["packaged Node full private-cache workload", "other NET-1 lanes", "rendered browser UI", "production networks"]
            network_reliability_directory(run, binary, workspace, fixture, helper, module, report, nodes, options.evidence_dir)
            return
        if lane == 2:
            report["tracked_worktree_dirty"] = subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=repository).returncode != 0
            report["base_commit"] = "4947bc8e82be6a156b858c2ef3053bc42838b3be"
            report["tracked_diff_sha256"] = hashlib.sha256(subprocess.check_output(["git", "diff", "HEAD", "--", "crates/node", "scripts/verify-node.py", "scripts/crypto-client.mjs"], cwd=repository)).hexdigest()
            source_paths = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "Cargo.toml", "Cargo.lock", "crates", "scripts/verify-node.py", "scripts/crypto-client.mjs"], cwd=repository).split(b"\0")
            fingerprint = hashlib.sha256()
            for raw in sorted(set(path for path in source_paths if path)):
                path = repository / os.fsdecode(raw)
                if "trash" not in str(path) and path.is_file():
                    fingerprint.update(raw + b"\0" + file_sha256(path).encode() + b"\n")
            report["source_files_sha256"] = fingerprint.hexdigest()
            report["source_files_counter"] = "path and SHA256 for tracked or nonignored Rust workspace files plus both acceptance scripts; includes untracked core modules"
            report["not_checked"] = ["core fsync/SQL boundary crash matrix", "physical SQLite VFS read amplification", "other NET-1 lanes", "rendered browser UI", "production networks"]
            if options.evidence_dir:
                baseline = options.evidence_dir / "base-release-probe.json"
                if baseline.exists():
                    report["failed_base_release_probe"] = {"evidence": str(baseline), "sha256": file_sha256(baseline), "comparison": "saved failed 32 MiB baseline; no fabricated completed timing or speedup"}
            network_reliability_blob_resume(run, binary, workspace, fixture, helper, module, report, nodes)
            return
        if lane == 3:
            report["tracked_worktree_dirty"] = subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=repository).returncode != 0
            report["not_checked"] = ["other NET-1 lanes", "rendered browser UI", "production networks"]
            network_reliability_closure_progress(run, binary, workspace, fixture, helper, module, report, nodes)
            return
        if lane == 1:
            require(options.evidence_dir is not None, "Lane 1 requires --evidence-dir with the saved M0 baseline")
            baseline_path = options.evidence_dir / "m0-baseline.json"
            baseline_report = json.loads(baseline_path.read_text())
            require(baseline_report["status"] == "observed" and baseline_report["mode"] == "network-reliability-m0",
                    "Lane 1 compares against an observed M0 process baseline")
            ancestry = subprocess.run(["git", "merge-base", "--is-ancestor", baseline_report["source_commit"], "HEAD"],
                                      cwd=repository, capture_output=True)
            require(ancestry.returncode == 0, "saved baseline is an ancestor of the current head")
            report["base_commit"] = baseline_report["source_commit"]
            report["baseline"] = {"evidence": str(baseline_path), "binary_sha256": baseline_report["binary_sha256"],
                                  "noop_sync": baseline_report["cases"]["noop_sync"]}
            report["tracked_worktree_dirty"] = subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=repository).returncode != 0
            report["not_checked"] = ["base/head interleaved perf probes", "other NET-1 lanes", "rendered browser UI", "production networks"]
        for name in ("R", "A", "B"):
            nodes.append(Node(run, name, binary, workspace, fixture, helper, module))
        relay, source, receiver = nodes
        relay.configure(roles=["relay"], network={"bootstrap_urls": [], "advertise_urls": [],
                                               "mdns": False, "relay_enabled": True})
        capture = RelayCapture(relay.port, retain_payloads=False)
        relay.configure(allowed_origins=[capture.url])
        relay.start()
        if not lane:
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
        with capture.lock:
            source_relay_start_connection = capture.next_connection
        source.start()
        network = source.bootstrap()
        report["relay_readiness"] = wait_for_relay_target(source, capture, source_relay_start_connection)
        receiver.pair(source, network, entry_url=capture.url)
        receiver.start(env={**os.environ, "RUST_LOG": "vibeface::runtime=debug", "NO_COLOR": "1"} if lane == 1 else None)
        receiver.client.connect(receiver.peer_id)
        wait_for("receiver client authorization", lambda: receiver.client.authenticate())
        conversation = "net1-baseline-history"
        source.client.rpc("create_conversation", conversation_id=conversation, title="NET-1 baseline")
        baseline = wait_for("receiver has the complete signed history",
                            lambda: (value if len(value["events"]) == 1 else None)
                            if (value := receiver.client.rpc("export", conversation_id=conversation)) else None)
        database = receiver.directory / "node.sqlite3"
        with sqlite3.connect(database, timeout=10) as connection:
            connection.execute("CREATE TABLE net1_projection_audit(table_name TEXT, operation TEXT)")
            for table in ("conversations", "messages", "events"):
                for operation in ("INSERT", "UPDATE", "DELETE"):
                    connection.execute(f"CREATE TRIGGER net1_{table}_{operation.lower()} AFTER {operation} ON {table} "
                                       f"BEGIN INSERT INTO net1_projection_audit VALUES('{table}','{operation}'); END")
        source.client.rpc("set_autonomy", conversation_id=conversation, mode="off", expected_version=0)
        baseline = wait_for("audit positive control imports a real new event",
                            lambda: (value if len(value["events"]) == 2 else None)
                            if (value := receiver.client.rpc("export", conversation_id=conversation)) else None)
        require(any(item["peer_id"] == source.peer_id and item["online"]
                    for item in receiver.client.rpc("nodes")["nodes"]), "receiver has a real authenticated source peer")
        if lane == 1:
            log_path = receiver.directory / "process.log"

            def complete_syncs(offset):
                with log_path.open("rb") as log:
                    log.seek(offset)
                    lines = log.read().decode(errors="replace").splitlines()
                flights, completed = set(), []
                for line in lines:
                    line = re.sub(r"\x1b\[[0-9;]*m", "", line)
                    if "history sync flight" not in line:
                        continue
                    fields = history_sync_fields(line)
                    if fields.get("network") == network and fields.get("scope") == conversation:
                        flights.add(fields["flight_id"])
                    if "history sync flight finished" in line and fields.get("flight_id") in flights:
                        completed.append(line)
                return completed

            converged = wait_for("complete sync after the full fixture has arrived",
                                 lambda offset=log_path.stat().st_size: complete_syncs(offset))
            require(source.client.rpc("export", conversation_id=conversation) == baseline,
                    "receiver has the source's exact complete signed history")
        with sqlite3.connect(database, timeout=10) as connection:
            positive_writes = connection.execute("SELECT COUNT(*) FROM net1_projection_audit WHERE table_name!='events'").fetchone()[0]
            require(positive_writes > 0, "database audit observes actual projection writes before the no-op check")
            if lane == 1:
                projection = {table: connection.execute(f"SELECT * FROM {table} ORDER BY rowid").fetchall()
                              for table in ("conversations", "messages")}
            connection.execute("DELETE FROM net1_projection_audit")
        started = time.monotonic()
        if lane == 1:
            log_offset = log_path.stat().st_size
            starting_bytes, starting_frames = capture.payload_bytes, capture.frame_count
            completed = wait_for("a new complete unchanged scope sync inside the audit window",
                                 lambda: complete_syncs(log_offset))
        else:
            time.sleep(7)
        after = receiver.client.rpc("export", conversation_id=conversation)
        require(after == baseline, "unchanged sync preserves the exact complete signed snapshot")
        with sqlite3.connect(database, timeout=10) as connection:
            writes = [{"table": table, "operation": operation, "rows": count}
                      for table, operation, count in connection.execute(
                          "SELECT table_name,operation,COUNT(*) FROM net1_projection_audit GROUP BY table_name,operation ORDER BY table_name,operation")]
            if lane == 1:
                require(all(connection.execute(f"SELECT * FROM {table} ORDER BY rowid").fetchall() == rows
                            for table, rows in projection.items()), "no-op sync preserves the exact business projection")
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
        if lane == 1:
            report["cases"]["noop_sync"].update({"status": "passed" if business_writes == 0 else "failed",
                "projection_identical": True, "convergence_completions": converged,
                "window_completions": completed, "receiver_log": str(log_path), "log_window_offset": log_offset,
                "wire_payload_bytes": capture.payload_bytes - starting_bytes, "wire_frames": capture.frame_count - starting_frames,
                "wire_counter": "all captured directions and both relay legs, including control and protocol overhead"})
            require(business_writes == 0, "completed no-op scope sync performs zero business projection writes")
            require(not capture.errors, "relay capture parses all observed frames")
            require(not invocations(run), "Lane 1 synchronization never invokes host work")
            verify_no_model_budget(nodes)
            require(file_sha256(binary) == report["binary_sha256"], "Lane 1 binary remains unchanged")
            report["status"] = "passed"
            report["fixture_invocations"] = 0
            print(f"PASS NET-1 Lane 1 report: {run / 'report.json'}", flush=True)
            return
        receiver.stop()
        source.stop()

        def blob_complete(reference):
            with sqlite3.connect(database, timeout=10) as connection:
                return connection.execute("SELECT EXISTS(SELECT 1 FROM blobs WHERE network_id=? AND hash=?)",
                                          (network, reference["hash"])).fetchone()[0] == 1

        small, _ = seed_net1_attachment(source, network, conversation, 4096, module)
        source.start()
        receiver.start()
        wait_for("small signed attachment validates the transfer fixture", lambda: blob_complete(small), timeout=25)
        require(verify_blobs(receiver, network) == 1, "small attachment fixture passes real peer signature, size and hash checks")
        receiver.stop()
        source.stop()
        blob, envelope = seed_net1_attachment(source, network, conversation, 32 * 1024 * 1024, module)
        source.start()
        receiver.start()
        receiver.client.connect(receiver.peer_id)
        wait_for("restarted receiver client authorization", lambda: receiver.client.authenticate())
        wait_for("restarted attachment peer is authenticated online", lambda: any(
            item["peer_id"] == source.peer_id and item["online"]
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
        require(not invocations(run), "NET-1 synchronization fixtures never invoke host work")
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
            evidence_path = options.evidence_dir / {1: "live/01-noop-sync.json", 2: "live/02-resume-blob.json", 3: "live/03-closure-progress.json", 4: "live/04-directory-isolation.json", 5: "live/05-reservation-generation.json", 6: "live/06-budget-and-duplex.json", 7: "live/07-route-lifecycle.json"}.get(lane, "m0-baseline.json")
            evidence_path.parent.mkdir(parents=True, exist_ok=True)
            private_json(evidence_path, report)


def shared_history(client, conversation):
    result = client.rpc("conversation", conversation_id=conversation)
    return result if (any(message["message_id"] == "message-original" for message in result["messages"])
                      and result["autonomy"]["version"] == 1
                      and not any(result["missing"].values())) else None


def auto_route_acceptance(options):
    repository = Path(__file__).resolve().parents[1]
    binary, module = options.binary.resolve(strict=True), options.crypto_module.resolve(strict=True)
    artifacts = options.artifacts.resolve()
    require(artifacts.is_relative_to(repository / ".artifacts"), "routing fixtures stay in the task worktree")
    require(subprocess.run(["git", "check-ignore", "--quiet", str(artifacts / "probe")],
                           cwd=repository).returncode == 0, "routing artifacts are ignored")
    artifacts.mkdir(parents=True, exist_ok=True)
    run = artifacts / ("auto-route-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir()
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    fixture = fixture_wrapper(run, "fixture-manager", "--fixture-manager")
    transport = repository / "crates/node/web/transport.js"
    nodes, clients, captures, directories = [], [], [], []
    report = {"status": "running", "source_commit": subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repository, text=True).strip(),
        "binary_sha256": file_sha256(binary), "transport_sha256": file_sha256(transport),
        "provider_calls": 0, "cases": {}, "artifacts": str(run),
        "not_checked": ["rendered browser UI", "production ingress", "provider models"],
        "measurement": "one functional run per case; includes helper startup, Noise, authorization and read, not UI latency or a speed comparison"}

    def ws(url):
        return url.replace("http:", "ws:") + "/api/network/ws"

    def fresh(url, *, identity_file=None):
        client = Client(url, helper, module, identity_file=identity_file)
        clients.append(client)
        return client

    def connect(client, target, *, network=None):
        client.peer_id = target
        started = time.monotonic()
        client.command("connect_auto", url=client.url, peer_id=target, transport=str(transport))
        require(client.call("info")["peer_id"] == target, "production transport reads the fixed authenticated target")
        if network:
            client.authenticate(network)
            client.call("subscribe")
            client.rpc("nodes")
        stats = wait_for("only the winning data channel survives", lambda:
                         (lambda value: value if value["active_data"] == 1 else None)(client.command("route_stats")))
        require(stats["max_active_data"] <= 2 and stats["authenticated_sessions"] == 1,
                "routing respects concurrency and selects one authenticated session")
        stats["ready_seconds"] = time.monotonic() - started
        return stats

    def passed(name, **evidence):
        report["cases"][name] = evidence
        print("PASS auto-route " + name, flush=True)

    def publish(url, record):
        stream = NetworkControlSocket(ws(url))
        try:
            stream.send_control({"type": "publish", "record": record})
            require(stream.control()["type"] == "published", "real entry accepts target-signed reachability")
        finally:
            stream.close()

    def signed_record(node, *, direct=None, relay=None, ttl=180):
        result = subprocess.run(["node", str(repository / "scripts/crypto-client.mjs"), "--module", str(module)],
            input=json.dumps({"op": "fixture_reachability", "identity_file": str(node.directory / "identity.json"),
                              "expected_peer_id": node.peer_id, "direct_urls": direct or [],
                              "relay_urls": relay or [], "ttl_secs": ttl}) + "\n",
            text=True, capture_output=True, timeout=10)
        reply = json.loads(result.stdout)
        require(result.returncode == 0 and reply["ok"], "actual target key signs the fixture record")
        return reply["result"]

    try:
        for name in ("R1", "R2", "TARGET"):
            node = Node(run, name, binary, workspace, fixture, helper, module, relay_only=name != "TARGET")
            nodes.append(node)
            node.configure(roles=["relay"] if name != "TARGET" else ["gateway", "replica", "relay", "scheduler"],
                           network={"bootstrap_urls": [], "advertise_urls": [], "mdns": False,
                                    "relay_enabled": name != "TARGET"})
            capture = RelayCapture(node.port)
            captures.append(capture)
            node.configure(allowed_origins=[capture.url])
        r1, r2, target = nodes
        c1, c2, local = captures
        target.configure(manager=fixture_manager_settings(run, workspace), codex_bin=sys.executable, daily_decision_limit=100)
        r1.start()
        r2.start()
        target.configure(network={"bootstrap_urls": [ws(c2.url)], "advertise_urls": [],
                                  "mdns": False, "relay_enabled": False})
        target.start()
        network = target.bootstrap()
        wait_for_relay_target(target, c2, 0)
        record = target.client.command("network_probe", kind="record", url=r2.url, target=target.peer_id)
        require(record["direct_urls"] == [] and record["relay_urls"] == [ws(c2.url)],
                "target is advertised only through R2")
        browser = fresh(local.url, identity_file=target.client.identity_file)
        stats = connect(browser, target.peer_id, network=network)
        require(sum(item["type"] in {"dial", "direct"} for item in stats["controls"]) == 1,
                "self target uses exactly one data channel with public relay disabled")
        passed("local-with-relay-disabled", target=target.peer_id, stats=stats)
        browser.url = c2.url
        passed("same-entry-reservation", target=target.peer_id, stats=connect(browser, target.peer_id, network=network))
        publish(r1.url, record)
        browser.url = c1.url
        stats = connect(browser, target.peer_id, network=network)
        require(any(item["type"] == "dial" and item["url"] == ws(c2.url) for item in stats["controls"]),
                "client follows R1's signed record to R2 without changing its entry")
        passed("other-entry", entry=c1.url, target=target.peer_id, stats=stats)
        fallback = signed_record(target, direct=[f"ws://127.0.0.1:{available_port()}/api/network/ws"], relay=[ws(c2.url)])
        directory = NetworkDirectoryFixture("fallback", record=fallback)
        directories.append(directory)
        browser.url = directory.url.replace("ws:", "http:").removesuffix("/api/network/ws")
        stats = connect(browser, target.peer_id, network=network)
        require(any(item["type"] == "dial" and item["url"] == ws(c2.url) for item in stats["controls"]),
                "failed direct candidate retains the healthy signed relay")
        passed("failed-direct-fallback", stats=stats)
        for kind in ("signature", "expired", "other-target"):
            invalid = copy.deepcopy(record)
            if kind == "signature":
                invalid["signature"] = "00" * 64
            elif kind == "expired":
                invalid["expires_at"] = int(time.time()) - 1
            else:
                invalid = signed_record(r1, relay=[ws(c2.url)])
            directory = NetworkDirectoryFixture(kind, record=invalid)
            directories.append(directory)
            rejected = fresh(directory.url.replace("ws:", "http:").removesuffix("/api/network/ws"))
            result = rejected.command("connect_auto", url=rejected.url, peer_id=target.peer_id,
                                      transport=str(transport), check=False)
            stats = rejected.command("route_stats")
            require(not result["ok"] and not stats["authenticated_sessions"]
                    and all(item["url"] != ws(c2.url) for item in stats["controls"]),
                    "unverified candidates never receive a connection")
            passed("reject-" + kind, stats=stats)
        cancelled_directory = NetworkDirectoryFixture("cancelled")
        directories.append(cancelled_directory)
        cancelled_directory.reply_gate.clear()
        cancelled = fresh(cancelled_directory.url.replace("ws:", "http:").removesuffix("/api/network/ws"))
        result = cancelled.command("connect_auto", url=cancelled.url, peer_id=target.peer_id,
                                   transport=str(transport), cancel_after_ms=50, check=False)
        cancelled_directory.reply_gate.set()
        stats = wait_for("cancel releases every transport socket", lambda:
                         (lambda value: value if value["opened"] == value["closed"] else None)(cancelled.command("route_stats")))
        require(not result["ok"] and stats["active_data"] == 0, "cancelled routing cannot select a session")
        passed("cancel", stats=stats)
        outsider = fresh(c1.url)
        connect(outsider, target.peer_id)
        outsider.call("rpc", {"method": "nodes"}, status=401)
        wrong = outsider.command("connect_auto", url=c1.url, peer_id="ab" * 32,
                                 transport=str(transport), check=False)
        require(not wrong["ok"], "wrong target cannot authenticate an available entry")
        browser.url = c1.url
        connect(browser, target.peer_id, network=network)
        invitation = target.client.invite("client")
        guest = fresh(c1.url)
        connect(guest, target.peer_id)
        prepared = guest.call("prepare_pair_request", {**invitation, "name": "Auto-route guest", "request_id": uuid.uuid4().hex})
        prepared["request"]["proof"] = guest.command("sign", **prepared["signing"])
        receipt = guest.call("pair_request", {"request": prepared["request"]})
        guest.call("pair_accept", {"request_id": receipt["request_id"],
                                   "signature": guest.command("sign", **receipt["signing"])})
        target.client.approve(receipt["request_id"])
        require(guest.call("pair_status", {"request_id": receipt["request_id"]})["status"] == "granted",
                "cross-entry invitation retains its original dual-signed request")
        guest.authenticate(network)
        revocation = target.client.call("prepare_revoke", {"subject_id": guest.command("identity")["subject_id"]})
        revocation["event"]["signature"] = target.client.command("sign", **revocation["signing"])
        target.client.call("revoke", {"event": revocation["event"]})
        guest.call("rpc", {"method": "nodes"}, status=403)
        connect(guest, target.peer_id)
        guest.call("authenticate", {"network_id": network}, status=403)
        passed("invitation-and-revocation", request_id=receipt["request_id"], network=network, target=target.peer_id)
        joining = Node(run, "CLI", binary, workspace, fixture, helper, module)
        nodes.append(joining)
        joining.pair(target, network, entry_url=c1.url)
        passed("cli-other-entry-invitation", subject=joining.peer_id, target=target.peer_id)
        conversation = "acceptance-conversation"
        browser.rpc("create_conversation", conversation_id=conversation, title="Automatic routing")
        browser.rpc("message", conversation_id=conversation, message_id="message-original", text="Keep this context after A disconnects.")
        directive = json.dumps({"label": "auto-route", "host_command": "printf once >> auto-route-count.txt",
                                "host_cwd": str(workspace), "gate": str(run / "never-release")})
        original = {"conversation_id": conversation, "message_id": "auto-route-unknown", "text": directive,
                    "scheduler_node": target.peer_id, "report_results": False}
        connect(browser, target.peer_id, network=network)
        with c2.lock:
            circuit = next(item for item in reversed(c2.controls) if item["direction"] == "to_relay"
                           and item["type"] == "dial" and item.get("target") == target.peer_id)
        c2.reply_connections = {circuit["connection"]}
        c2.reply_gate.clear()
        marker = workspace / "auto-route-count.txt"
        with ThreadPoolExecutor(max_workers=1) as pool:
            pending = pool.submit(browser.rpc, "message", **original)
            wait_for("exactly one real Agent write starts before its response is delivered", lambda:
                     marker.exists() and marker.read_text() == "once" and len(invocations(run, "auto-route")) == 1)
            trigger = invocations(run, "auto-route")[0]["trigger"]
            running = scheduler_record(target, network, trigger)
            require(running and running["state"] == "running", "write is bound to one running Agent attempt")
            attempt_id = running["attempt"]["id"]
            c2.pause()
            try:
                pending.result(timeout=10)
                raise AssertionError("withheld write response must remain unconfirmed")
            except AssertionError as error:
                require("加密连接已断开" in str(error), "lost reply is reported as an unconfirmed write")
        target.stop()
        target.configure(network={"bootstrap_urls": [ws(c1.url)], "advertise_urls": [], "mdns": False, "relay_enabled": False})
        target.start()
        target.client.connect()
        target.client.authenticate()
        wait_for_relay_target(target, c1, 0)
        stats = connect(browser, target.peer_id, network=network)
        snapshot = browser.rpc("conversation", conversation_id=conversation)
        message = next(row for row in snapshot["messages"] if row["message_id"] == original["message_id"])
        job = next(row for row in snapshot["scheduler"]["jobs"] if row["trigger"] == trigger)
        require(message["event_hash"] == trigger and job["state"] == "unknown"
                and job["attempt"]["id"] == attempt_id and snapshot["scheduler"]["write_frozen"],
                "reconnected reads retain the original unknown write and frozen attempt")
        require(marker.read_text() == "once" and len(invocations(run, "auto-route")) == 1
                and not any(item.get("rpc_method") == "message" for item in browser.command("route_stats")["calls"]),
                "changed path restores reads without replaying the original write")
        duplicate = browser.rpc("message", **original)
        require(duplicate["trigger"] == trigger and duplicate["state"] == "unknown",
                "an explicit same-ID retry returns the original unknown job")
        require_quiet("a same-ID retry never repeats the original host write", lambda:
                      marker.read_text() == "once" and len(invocations(run, "auto-route")) == 1)
        passed("uncertain-write-and-path-change", message_id=original["message_id"], trigger=trigger,
               attempt_id=attempt_id, invocations=1, stats=stats)
        recovered = fresh(c1.url)
        backup = verify_recovery(target, recovered, [target], network, entry_url=c1.url)
        require(backup["network_id"] == network, "automatic-route recovery retains the original network")
        passed("recovery", network=network, target=target.peer_id)
        for relay in (r1, r2):
            with sqlite3.connect(f"file:{relay.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
                require(database.execute("SELECT COUNT(*) FROM events WHERE network_id=?", (network,)).fetchone()[0] == 0,
                        "public entries never acquire private network history")
        require(all(b"Keep this context after A disconnects." not in b"".join(capture.payloads) for capture in (c1, c2)),
                "relays cannot see encrypted business text")
        passed("blind-public-entries", private_event_count=0, plaintext_canary_found=False)
        report["status"] = "passed"
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        for client in reversed(clients):
            client.close()
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        for directory in reversed(directories):
            directory.close()
        for capture in reversed(captures):
            capture.close()
        require(not any(capture.errors for capture in captures), "routing captures remain valid")
        report["cleanup"] = {"remaining_nodes": sum(node.process is not None for node in nodes),
                             "remaining_clients": sum(client.process is not None for client in clients),
                             "remaining_directory_sockets": sum(directory.snapshot()["active_sockets"] for directory in directories)}
        private_json(run / "report.json", report)
        if options.evidence_dir:
            options.evidence_dir.mkdir(parents=True, exist_ok=True)
            private_json(options.evidence_dir / "auto-route-process.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


if __name__ == "__main__":
    os.umask(0o077)
    if "--fixture-manager" in sys.argv:
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
        parser.add_argument("--evidence-dir", type=Path, help="save compact NET-1 reports in the task evidence directory")
        parser.add_argument("--net1-lane", type=int, choices=[1, 2, 3, 4, 5, 6, 7], help="run one focused NET-1 process lane")
        modes = parser.add_mutually_exclusive_group()
        modes.add_argument("--web-only", action="store_true", help="check packaged Web resources without Node business or provider scenarios")
        modes.add_argument("--phase1", action="store_true", help="run focused durable Manager queue/follow-up process checks with controlled fixtures")
        modes.add_argument("--chat-agent", action="store_true", help="run Agent delegation, host effects, cancellation and native observation process checks")
        modes.add_argument("--network-reliability", action="store_true", help="record NET-1 isolated process baselines without provider calls")
        modes.add_argument("--auto-route", action="store_true", help="check production target routing, invitation, recovery and uncertain writes")
        arguments = parser.parse_args()
        if arguments.net1_lane and not arguments.network_reliability:
            parser.error("--net1-lane requires --network-reliability")
        try:
            if arguments.auto_route:
                auto_route_acceptance(arguments)
            elif arguments.web_only:
                web_acceptance(arguments)
            elif arguments.network_reliability:
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
